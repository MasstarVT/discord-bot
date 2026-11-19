//! Server Backups tests: the ported `test/backups.test.js` (and the backup part of `test/sec-misc.test.js`), the JS
//! golden data of `rust/fixtures/gen/feat-backups-golden.mjs` (snapshot, parser, masking, whole restores of the same
//! fake guilds with their call logs, texts), the declarations, the database code and both cross-language directions.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_core::testing::{DbTestApp, FakeInteraction, TestApp, assert_commands_match_golden, assert_plugin_matches_golden, load_golden, run_node};
use bot_core::prelude::{JobOutcome, PermanentJobError, SettingsPatch};
use chrono::Utc;
use feat_backups::backup_service::{self as svc, CreateResult, RestoreHost};
use feat_backups::guild_snapshot::{self as snap, *};
use feat_backups::live_guild::SummaryPost;
use indexmap::IndexMap;
use serde_json::{Value, json};
use serenity::all::CreateEmbed;

fn golden() -> Value {
    load_golden("feat-backups/golden.json")
}

fn registry() -> bot_core::Registry {
    let mut reg = bot_core::Registry::new();
    feat_backups::register(&mut reg);
    reg
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

// ── The fake guild (the JS fake of test/backups.test.js, from the same JSON spec) ──

#[derive(Default, Debug)]
struct Log {
    create: Vec<Value>,
    edit: Vec<Value>,
    put: Vec<Value>,
    role_positions: Vec<Value>,
    channel_positions: Vec<Value>,
    fetches: Vec<String>,
}

#[derive(Default)]
struct FakeState {
    view: GuildView,
    next_id: u64,
    audit: Vec<(AuditKind, AuditEntry)>,
    log: Log,
    sent: Vec<(String, Option<String>, Value, Option<String>)>,
    dms: Vec<(String, Value)>,
    role_creates: usize,
}

/// How role creates time out: `(first n calls, made anyway)`.
#[derive(Clone, Copy)]
enum Plan {
    Normal,
    FirstTimesOutAfterMaking,
    AlwaysTimesOut,
    Hold,
}

struct Fake {
    s: Mutex<FakeState>,
    members: Vec<String>,
    fail_role: bool,
    fail_channel: bool,
    audit_fails: bool,
    no_audit: bool,
    plan: Mutex<Plan>,
    hold: tokio::sync::Notify,
    usable: Mutex<HashSet<String>>,
}

fn b(v: Option<&Value>) -> u128 {
    snap::bits(v)
}

fn role_from_spec(r: &Value) -> RoleView {
    RoleView {
        id: r["id"].as_str().unwrap().into(),
        name: r["name"].as_str().unwrap().into(),
        color: r.get("color").and_then(Value::as_u64).unwrap_or(0) as u32,
        hoist: r.get("hoist").and_then(Value::as_bool).unwrap_or(false),
        mentionable: r.get("mentionable").and_then(Value::as_bool).unwrap_or(false),
        permissions: b(r.get("perms")),
        raw_position: r.get("pos").and_then(Value::as_i64).unwrap_or(1),
        unicode_emoji: r.get("emoji").and_then(Value::as_str).map(String::from),
        managed: r.get("managed").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn channel_from_spec(c: &Value) -> ChannelView {
    let mut v = ChannelView::new(c["id"].as_str().unwrap(), c.get("type").and_then(Value::as_i64).unwrap_or(0), c["name"].as_str().unwrap());
    v.parent_id = c.get("parent").and_then(Value::as_str).map(String::from);
    v.raw_position = c.get("pos").and_then(Value::as_i64).unwrap_or(0);
    v.topic = c.get("topic").and_then(Value::as_str).map(String::from);
    v.nsfw = c.get("nsfw").and_then(Value::as_bool).unwrap_or(false);
    v.rate_limit_per_user = Some(c.get("slow").and_then(Value::as_i64).unwrap_or(0));
    v.bitrate = c.get("bitrate").and_then(Value::as_i64);
    v.user_limit = Some(c.get("limit").and_then(Value::as_i64).unwrap_or(0));
    v.default_auto_archive_duration = c.get("archive").and_then(Value::as_i64);
    v.manageable = c.get("manageable").and_then(Value::as_bool).unwrap_or(true);
    v.is_thread = c.get("thread").and_then(Value::as_bool).unwrap_or(false);
    for t in c.get("tags").and_then(Value::as_array).cloned().unwrap_or_default() {
        v.tags.push(TagView {
            name: t["name"].as_str().unwrap().into(),
            moderated: t["moderated"].as_bool().unwrap_or(false),
            emoji_id: t["emoji"]["id"].as_str().map(String::from),
            emoji_name: t["emoji"]["name"].as_str().map(String::from),
        });
    }
    for o in c.get("overwrites").and_then(Value::as_array).cloned().unwrap_or_default() {
        let id = o["id"].as_str().unwrap().to_string();
        let kind = if o.get("type").and_then(Value::as_i64) == Some(1) { OwKind::Member } else { OwKind::Role };
        v.overwrites.insert(id.clone(), OverwriteView { id, kind, allow: b(o.get("allow")), deny: b(o.get("deny")) });
    }
    v
}

impl Fake {
    fn from_spec(spec: &Value, id: &str) -> Arc<Fake> {
        let mut roles = IndexMap::new();
        for r in spec.get("roles").and_then(Value::as_array).cloned().unwrap_or_default() {
            let v = role_from_spec(&r);
            roles.insert(v.id.clone(), v);
        }
        roles.insert(
            id.to_string(),
            RoleView { id: id.into(), name: "@everyone".into(), color: 0, hoist: false, mentionable: false, permissions: b(spec.get("everyonePerms")), raw_position: 0, unicode_emoji: None, managed: false },
        );
        let mut channels = IndexMap::new();
        for c in spec.get("channels").and_then(Value::as_array).cloned().unwrap_or_default() {
            let v = channel_from_spec(&c);
            channels.insert(v.id.clone(), v);
        }
        let view = GuildView {
            id: id.into(),
            name: "Test Server".into(),
            owner_id: "OWNER".into(),
            roles,
            channels,
            features: vec![],
            maximum_bitrate: Some(96000),
            me: Some(MeView { id: "9999".into(), permissions: b(spec.get("botPerms")), highest_position: spec.get("botTop").and_then(Value::as_i64).unwrap_or(10) }),
            member_ids: HashSet::new(),
        };
        Arc::new(Fake {
            s: Mutex::new(FakeState { view, next_id: 5000, ..Default::default() }),
            members: spec.get("members").and_then(Value::as_array).map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect()).unwrap_or_default(),
            fail_role: spec["fail"]["roleCreate"].as_bool().unwrap_or(false),
            fail_channel: spec["fail"]["channelCreate"].as_bool().unwrap_or(false),
            audit_fails: false,
            no_audit: false,
            plan: Mutex::new(Plan::Normal),
            hold: tokio::sync::Notify::new(),
            usable: Mutex::new(HashSet::new()),
        })
    }

    fn st(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.s.lock().unwrap()
    }

    fn new_id(&self) -> String {
        let mut s = self.st();
        let id = s.next_id.to_string();
        s.next_id += 1;
        id
    }

    fn role_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.st().view.roles.values().map(|r| r.name.clone()).collect();
        v.sort();
        v
    }

    fn channel_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.st().view.channels.values().map(|c| c.name.clone()).collect();
        v.sort();
        v
    }

    fn add_role(&self, name: &str, executor: &str, reason: Option<&str>, created_ms: i64) -> String {
        let id = self.new_id();
        let mut s = self.st();
        s.view.roles.insert(id.clone(), RoleView { id: id.clone(), name: name.into(), color: 0, hoist: false, mentionable: false, permissions: 0, raw_position: 3, unicode_emoji: None, managed: false });
        s.audit.push((AuditKind::RoleCreate, AuditEntry { target_id: Some(id.clone()), executor_id: Some(executor.into()), reason: reason.map(String::from), created_ms }));
        id
    }

    fn add_channel(&self, name: &str, parent: Option<&str>, executor: &str, reason: Option<&str>, created_ms: i64) -> String {
        let id = self.new_id();
        let mut c = ChannelView::new(&id, 0, name);
        c.parent_id = parent.map(String::from);
        let mut s = self.st();
        s.view.channels.insert(id.clone(), c);
        s.audit.push((AuditKind::ChannelCreate, AuditEntry { target_id: Some(id.clone()), executor_id: Some(executor.into()), reason: reason.map(String::from), created_ms }));
        id
    }
}

fn role_data_js(d: &RoleData, reason: &str) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(v) = &d.name {
        m.insert("name".into(), json!(v));
    }
    if let Some(v) = d.color {
        m.insert("colors".into(), json!({ "primaryColor": v }));
    }
    if let Some(v) = d.hoist {
        m.insert("hoist".into(), json!(v));
    }
    if let Some(v) = d.mentionable {
        m.insert("mentionable".into(), json!(v));
    }
    if let Some(v) = d.permissions {
        m.insert("permissions".into(), json!(v.to_string()));
    }
    if let Some(v) = &d.unicode_emoji {
        m.insert("unicodeEmoji".into(), json!(v));
    }
    m.insert("reason".into(), json!(reason));
    Value::Object(m)
}

fn channel_data_js(d: &ChannelCreate, reason: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("name".into(), json!(d.name));
    m.insert("type".into(), json!(d.kind));
    m.insert("reason".into(), json!(reason));
    if let Some(p) = &d.parent {
        m.insert("parent".into(), json!(p));
    }
    m.insert("position".into(), json!(d.position));
    if !d.permission_overwrites.is_empty() {
        m.insert(
            "permissionOverwrites".into(),
            json!(d.permission_overwrites.iter().map(|o| json!({ "id": o.id, "type": o.kind.code(), "allow": o.allow.to_string(), "deny": o.deny.to_string() })).collect::<Vec<_>>()),
        );
    }
    let mut opt = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            m.insert(k.into(), v);
        }
    };
    opt("topic", d.topic.as_ref().map(|t| json!(t)));
    opt("nsfw", d.nsfw.map(|t| json!(t)));
    opt("rateLimitPerUser", d.rate_limit_per_user.map(|t| json!(t)));
    opt("defaultAutoArchiveDuration", d.default_auto_archive_duration.map(|t| json!(t)));
    opt("bitrate", d.bitrate.map(|t| json!(t)));
    opt("userLimit", d.user_limit.map(|t| json!(t)));
    if !d.available_tags.is_empty() {
        m.insert(
            "availableTags".into(),
            json!(d.available_tags.iter().map(|t| json!({ "name": t.name, "moderated": t.moderated, "emoji": t.emoji_name.as_ref().map(|n| json!({ "id": null, "name": n })) })).collect::<Vec<_>>()),
        );
    }
    Value::Object(m)
}

fn channel_edit_js(d: &ChannelEdit, reason: &str) -> Value {
    let mut v = d.to_body();
    let m = v.as_object_mut().unwrap();
    for (from, to) in [("rate_limit_per_user", "rateLimitPerUser"), ("default_auto_archive_duration", "defaultAutoArchiveDuration"), ("user_limit", "userLimit"), ("parent_id", "parent")] {
        if let Some(x) = m.shift_remove(from) {
            m.insert(to.into(), x);
        }
    }
    m.insert("reason".into(), json!(reason));
    v
}

fn timeout_err() -> ApiError {
    ApiError::timeout("role")
}

#[async_trait]
impl RestoreApi for Fake {
    fn view(&self) -> GuildView {
        self.st().view.clone()
    }
    async fn edit_everyone(&self, permissions: u128, reason: &str) -> Result<(), ApiError> {
        let mut s = self.st();
        let gid = s.view.id.clone();
        s.log.edit.push(json!(["role", gid, { "permissions": permissions.to_string(), "reason": reason }]));
        if let Some(r) = s.view.roles.get_mut(&gid) {
            r.permissions = permissions;
        }
        Ok(())
    }
    async fn create_role(&self, data: &RoleData, reason: &str) -> Result<RoleView, ApiError> {
        let plan = *self.plan.lock().unwrap();
        let n = {
            let mut s = self.st();
            s.role_creates += 1;
            s.role_creates
        };
        match plan {
            Plan::AlwaysTimesOut => return Err(timeout_err()),
            Plan::Hold if n == 1 => self.hold.notified().await,
            _ => {}
        }
        if self.fail_role {
            return Err(ApiError::new("Maximum number of server roles reached", Some(30005)));
        }
        let id = self.new_id();
        let role = RoleView {
            id: id.clone(),
            name: data.name.clone().unwrap_or_default(),
            color: data.color.unwrap_or(0),
            hoist: data.hoist.unwrap_or(false),
            mentionable: data.mentionable.unwrap_or(false),
            permissions: data.permissions.unwrap_or(0),
            raw_position: 1,
            unicode_emoji: data.unicode_emoji.clone().flatten(),
            managed: false,
        };
        {
            let mut s = self.st();
            s.log.create.push(json!(["role", role_data_js(data, reason)]));
            s.view.roles.insert(id.clone(), role.clone());
            s.audit.push((AuditKind::RoleCreate, AuditEntry { target_id: Some(id), executor_id: Some("9999".into()), reason: Some(reason.into()), created_ms: now_ms() }));
        }
        if matches!(plan, Plan::FirstTimesOutAfterMaking) && n == 1 {
            return Err(timeout_err());
        }
        Ok(role)
    }
    async fn edit_role(&self, role_id: &str, data: &RoleData, reason: &str) -> Result<(), ApiError> {
        let mut s = self.st();
        s.log.edit.push(json!(["role", role_id, role_data_js(data, reason)]));
        if let Some(r) = s.view.roles.get_mut(role_id) {
            if let Some(v) = &data.name {
                r.name = v.clone();
            }
            if let Some(v) = data.color {
                r.color = v;
            }
            if let Some(v) = data.hoist {
                r.hoist = v;
            }
            if let Some(v) = data.mentionable {
                r.mentionable = v;
            }
            if let Some(v) = data.permissions {
                r.permissions = v;
            }
            if let Some(v) = &data.unicode_emoji {
                r.unicode_emoji = v.clone();
            }
        }
        Ok(())
    }
    async fn set_role_positions(&self, entries: &[(String, i64)]) -> Result<(), ApiError> {
        let mut s = self.st();
        s.log.role_positions.push(json!(entries.iter().map(|(r, p)| json!({ "role": r, "position": p })).collect::<Vec<_>>()));
        for (r, p) in entries {
            if let Some(x) = s.view.roles.get_mut(r) {
                x.raw_position = *p;
            }
        }
        Ok(())
    }
    async fn create_channel(&self, data: &ChannelCreate, reason: &str) -> Result<ChannelView, ApiError> {
        if self.fail_channel {
            return Err(ApiError::new("Missing Permissions", Some(50013)));
        }
        let id = self.new_id();
        let mut c = ChannelView::new(&id, data.kind, &data.name);
        c.parent_id = data.parent.clone();
        c.raw_position = data.position;
        c.topic = data.topic.clone();
        c.nsfw = data.nsfw.unwrap_or(false);
        c.rate_limit_per_user = Some(data.rate_limit_per_user.unwrap_or(0));
        c.bitrate = data.bitrate;
        c.user_limit = Some(data.user_limit.unwrap_or(0));
        for o in &data.permission_overwrites {
            c.overwrites.insert(o.id.clone(), o.clone());
        }
        let mut s = self.st();
        s.log.create.push(json!(["channel", channel_data_js(data, reason)]));
        s.view.channels.insert(id.clone(), c.clone());
        s.audit.push((AuditKind::ChannelCreate, AuditEntry { target_id: Some(id), executor_id: Some("9999".into()), reason: Some(reason.into()), created_ms: now_ms() }));
        Ok(c)
    }
    async fn edit_channel(&self, channel_id: &str, data: &ChannelEdit, reason: &str) -> Result<(), ApiError> {
        let mut s = self.st();
        s.log.edit.push(json!(["channel", channel_id, channel_edit_js(data, reason)]));
        if let Some(c) = s.view.channels.get_mut(channel_id) {
            if let Some(v) = &data.name {
                c.name = v.clone();
            }
            if let Some(v) = &data.topic {
                c.topic = if v.is_empty() { None } else { Some(v.clone()) };
            }
            if let Some(v) = data.nsfw {
                c.nsfw = v;
            }
            if let Some(v) = data.rate_limit_per_user {
                c.rate_limit_per_user = Some(v);
            }
            if let Some(v) = data.bitrate {
                c.bitrate = Some(v);
            }
            if let Some(v) = data.user_limit {
                c.user_limit = Some(v);
            }
            if let Some(v) = &data.parent {
                c.parent_id = Some(v.clone());
            }
        }
        Ok(())
    }
    async fn set_channel_positions(&self, entries: &[(String, i64)]) -> Result<(), ApiError> {
        let mut s = self.st();
        s.log.channel_positions.push(json!(entries.iter().map(|(c, p)| json!({ "channel": c, "position": p })).collect::<Vec<_>>()));
        for (c, p) in entries {
            if let Some(x) = s.view.channels.get_mut(c) {
                x.raw_position = *p;
            }
        }
        Ok(())
    }
    async fn put_overwrite(&self, channel_id: &str, target: &str, kind: OwKind, allow: u128, deny: u128, reason: &str) -> Result<(), ApiError> {
        let mut s = self.st();
        s.log.put.push(json!({ "route": format!("/channels/{channel_id}/permissions/{target}"), "body": { "type": kind.code(), "allow": allow.to_string(), "deny": deny.to_string() }, "reason": reason }));
        // Like discord.js, a raw REST call does not touch the cache (the gateway update comes later).
        Ok(())
    }
    async fn fetch_member(&self, user_id: &str) -> Result<bool, ApiError> {
        self.st().log.fetches.push(user_id.into());
        if self.members.iter().any(|m| m == user_id) { Ok(true) } else { Err(ApiError::new("Unknown Member", Some(10007))) }
    }
    async fn audit_log(&self, kind: AuditKind, _limit: u8) -> Result<Vec<AuditEntry>, ApiError> {
        if self.no_audit || self.audit_fails {
            return Err(ApiError::new("Missing Permissions", Some(50013)));
        }
        Ok(self.st().audit.iter().filter(|(k, _)| *k == kind).map(|(_, e)| e.clone()).collect())
    }
}

#[async_trait]
impl RestoreHost for Fake {
    async fn fetch_me(&self) -> Option<MeView> {
        None
    }
    fn channel_usable(&self, channel_id: &str) -> bool {
        self.usable.lock().unwrap().contains(channel_id)
    }
    async fn send_channel(&self, channel_id: &str, post: &SummaryPost) -> Result<(), ApiError> {
        self.st().sent.push((channel_id.into(), post.content.clone(), serde_json::to_value(&post.embed).unwrap(), post.ping_user.clone()));
        Ok(())
    }
    async fn send_dm(&self, user_id: &str, embed: &CreateEmbed) -> Result<(), ApiError> {
        self.st().dms.push((user_id.into(), serde_json::to_value(embed).unwrap()));
        Ok(())
    }
}

fn original() -> Value {
    golden()["original"].clone()
}

fn snapshot() -> Snapshot {
    parse_snapshot(&golden()["snapshot"]).unwrap()
}

fn round(state: &RestoreState) -> RestoreState {
    normalise_restore_state(&serde_json::to_value(state).unwrap(), now_ms()).unwrap()
}

struct Run {
    state: RestoreState,
    batches: usize,
    calls: Vec<usize>,
}

/// `restoreAll`: batches like the job (state through JSON between them) until `Finish`.
async fn restore_all(f: &Fake, mode: &str, snapshot: &Snapshot, id_map: &IndexMap<String, String>, retry: bool, max: usize) -> Run {
    let mut state = new_restore_state("bk000001", mode, Some("OWNER"), Some("CH"), id_map, now_ms() - 1000);
    state.retry = retry;
    let mut run = Run { state: state.clone(), batches: 0, calls: vec![] };
    loop {
        state = round(&state);
        let api: &dyn RestoreApi = f;
        let mut env = make_env(f.view(), snapshot, state, "test", Some(api));
        let r = run_restore_batch(&mut env).await;
        run.calls.push(env.calls);
        state = env.state.clone();
        state.retry = false;
        run.batches += 1;
        if r == BatchResult::Finish || run.batches >= max {
            run.state = state;
            return run;
        }
    }
}

/// Fields of an embed as `{ name, value, inline }` (JS leaves `inline` out when false).
fn embed_parts(e: &Value) -> Value {
    let fields: Vec<Value> = e["fields"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|f| json!({ "name": f["name"], "value": f["value"], "inline": f.get("inline").and_then(Value::as_bool).unwrap_or(false) }))
        .collect();
    json!({ "title": e.get("title"), "description": e.get("description"), "fields": fields, "footer": e.get("footer").map(|f| f["text"].clone()) })
}

fn embed_json(e: &CreateEmbed) -> Value {
    serde_json::to_value(e).unwrap()
}

// ── Golden: snapshot, parse, state, bits ──────────────────────────────────────

#[test]
fn capture_matches_the_js_snapshot() {
    let g = golden();
    let f = Fake::from_spec(&original(), "900");
    let taken = chrono::DateTime::parse_from_rfc3339("2026-10-21T10:00:00Z").unwrap().with_timezone(&Utc);
    let snap = capture_guild(&f.view(), taken);
    assert_eq!(snap.to_json(), g["snapshot"]);
    assert_eq!(snapshot_bytes(&g["snapshot"]), g["snapshotBytes"].as_u64().unwrap() as usize);
    let tricky = &g["snapshotBytesTricky"];
    assert_eq!(snapshot_bytes(&tricky["data"]), tricky["bytes"].as_u64().unwrap() as usize);
    assert_eq!(serde_json::to_value(snapshot_stats(&snap)).unwrap(), g["stats"]);
    assert_eq!(json!(snapshot_stats(&snap).overwrites), json!(5));
    // A huge server stays under BACKUP_BYTES.
    let mut spec = json!({ "roles": [], "channels": [] });
    for i in 0..250 {
        spec["roles"].as_array_mut().unwrap().push(json!({ "id": (2000 + i).to_string(), "name": format!("r{i}"), "pos": i + 1 }));
    }
    for i in 0..500 {
        let ows: Vec<Value> = (0..10).map(|j| json!({ "id": (100 + j).to_string(), "allow": "1024" })).collect();
        spec["channels"].as_array_mut().unwrap().push(json!({ "id": (1000 + i).to_string(), "name": format!("channel-{i}"), "pos": i, "overwrites": ows }));
    }
    let big = capture_guild(&Fake::from_spec(&spec, "900").view(), Utc::now());
    assert!(snapshot_bytes(&big.to_json()) < bot_config::limits::BACKUP_BYTES);
}

#[test]
fn parse_snapshot_matches_the_js_sweep() {
    let g = golden();
    for (raw, want) in g["messy"].as_array().unwrap().iter().zip(g["parse"].as_array().unwrap()) {
        match parse_snapshot(raw) {
            Ok(s) => assert_eq!(s.to_parsed_json(), want["ok"], "input {raw}"),
            Err(e) => assert_eq!(json!(e.0), want["error"], "input {raw}"),
        }
    }
    // exportable drops the id map.
    let mut with = g["snapshot"].clone();
    with["idMap"] = json!({ "1": "2" });
    assert!(exportable(&with).get("idMap").is_none());
    assert_eq!(exportable(&g["snapshot"]), g["snapshot"]);
}

#[test]
fn restore_state_matches_the_js() {
    let g = golden();
    for (raw, want) in g["payloads"].as_array().unwrap().iter().zip(g["normalise"].as_array().unwrap()) {
        let got = normalise_restore_state(raw, 777);
        match got {
            None => assert!(want.is_null(), "payload {raw}"),
            Some(s) => {
                let mut v = s.to_json();
                if want["startedAt"] == json!("now") {
                    assert_eq!(v["startedAt"], json!(777));
                    v["startedAt"] = json!("now");
                }
                assert_eq!(v, *want, "payload {raw}");
            }
        }
    }
    let ids: IndexMap<String, String> = [("1".to_string(), "2".to_string()), ("x".into(), "y".into())].into_iter().collect();
    assert_eq!(new_restore_state("bk1", "missing", None, Some("C"), &ids, 5).to_json(), g["newState"]);
    // The retry signal: only a real true counts (sec-misc).
    let fresh = new_restore_state("bk000001", "missing", None, None, &IndexMap::new(), 1);
    assert!(!fresh.retry);
    let mut v = fresh.to_json();
    v["retry"] = json!(true);
    assert!(normalise_restore_state(&v, 1).unwrap().retry);
    v["retry"] = json!("yes");
    assert!(!normalise_restore_state(&v, 1).unwrap().retry);
}

#[test]
fn bits_masking_and_names_match_the_js() {
    let g = golden();
    for c in g["mask"].as_array().unwrap() {
        let i: Vec<u128> = c["in"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().parse().unwrap()).collect();
        let m = mask_bits(i[0], i[1], i[2]);
        assert_eq!(json!({ "value": m.value.to_string(), "dropped": m.dropped.to_string() }), c["out"]);
    }
    for c in g["bits"].as_array().unwrap() {
        assert_eq!(bits(Some(&c["in"])).to_string(), c["out"].as_str().unwrap(), "bits({})", c["in"]);
    }
    assert_eq!(all_permission_bits().to_string(), g["allBits"].as_str().unwrap());
    assert_eq!(bot_bits(Some(&MeView { id: "1".into(), permissions: perm::ADMINISTRATOR, highest_position: 1 })), all_permission_bits());
    for c in g["bitNames"].as_array().unwrap() {
        assert_eq!(json!(bit_names(c["in"].as_str().unwrap().parse().unwrap())), c["out"]);
    }
}

#[test]
fn rows_labels_and_texts_match_the_js() {
    let g = golden();
    for c in g["cleanName"].as_array().unwrap() {
        assert_eq!(json!(svc::clean_name(c["in"].as_str())), c["out"], "cleanName({})", c["in"]);
    }
    for c in g["stamps"].as_array().unwrap() {
        let d = chrono::DateTime::parse_from_rfc3339(c["in"].as_str().unwrap()).unwrap().with_timezone(&Utc);
        assert_eq!(svc::stamp_of(&d), c["out"].as_str().unwrap());
    }
    let rows: Vec<svc::BackupInfo> = g["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| svc::BackupInfo {
            id: r["id"].as_str().unwrap().into(),
            guild_id: "1".into(),
            name: r["name"].as_str().map(String::from),
            automatic: r["automatic"].as_bool().unwrap(),
            created_by_id: Some("U1".into()),
            size_bytes: 12345,
            role_count: r["roleCount"].as_i64().unwrap() as i32,
            channel_count: r["channelCount"].as_i64().unwrap() as i32,
            created_at: bot_db::serde_js::parse_js_date(r["createdAt"].as_str().unwrap()).unwrap(),
        })
        .collect();
    let labels: Vec<String> = rows.iter().map(svc::backup_label).collect();
    assert_eq!(json!(labels), g["labels"]);
    assert_eq!(embed_parts(&embed_json(&svc::build_list_embed(None, &rows))), embed_parts(&g["listEmbed"]));
    assert_eq!(embed_parts(&embed_json(&svc::build_list_embed(None, &[]))), embed_parts(&g["listEmpty"]));
    assert_eq!(embed_parts(&embed_json(&svc::build_info_embed(None, &rows[0], &snapshot()))), embed_parts(&g["infoEmbed"]));
    for c in g["missingPerms"].as_array().unwrap() {
        let m: Vec<&str> = c["in"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
        assert_eq!(embed_parts(&embed_json(&svc::missing_permissions_embed(&m))), embed_parts(&c["out"]));
    }
    assert_eq!(embed_parts(&embed_json(&svc::owner_only_embed())), embed_parts(&g["ownerOnly"]));
    assert_eq!(bot_config::custom_id!("backup", "restore", "cm0abcdefghijkl", "full").unwrap(), g["customIds"]["restore"].as_str().unwrap());
    assert_eq!(bot_config::custom_id!("backup", "cancel").unwrap(), g["customIds"]["cancel"].as_str().unwrap());
    assert_eq!(svc::auto_job_key("123"), g["keys"]["auto"].as_str().unwrap());
    assert_eq!(svc::restore_job_key("123"), g["keys"]["restore"].as_str().unwrap());
    let row = serde_json::to_value(svc::restore_buttons("cmabc", "missing").unwrap()).unwrap();
    let ours: Vec<Value> = row["components"].as_array().unwrap().iter().map(|c| json!([c["custom_id"], c["label"], c["style"]])).collect();
    let js: Vec<Value> = g["buttons"]["components"].as_array().unwrap().iter().map(|c| json!([c["custom_id"], c["label"], c["style"]])).collect();
    assert_eq!(ours, js);
}

fn settings_of(guild_id: &str, extra: Value) -> bot_db::GuildSettings {
    let mut row = bot_core::testing::load_golden("mockdb/baseline.json")["GuildSettings"]["min"].clone();
    row["id"] = json!(format!("gs{guild_id}"));
    row["guildId"] = json!(guild_id);
    for (k, x) in extra.as_object().unwrap() {
        row[k] = x.clone();
    }
    bot_db::GuildSettings::from_json(&row).expect("settings row")
}

fn job_row(status: bot_db::JobStatus, payload: Value, last_error: Option<&str>) -> bot_db::ScheduledJob {
    bot_db::ScheduledJob {
        id: "j".into(),
        guild_id: "1".into(),
        r#type: "backup.restore".into(),
        dedupe_key: Some("backup.restore:1".into()),
        run_at: Utc::now(),
        payload,
        status,
        attempts: 0,
        max_attempts: 5,
        last_error: last_error.map(String::from),
        locked_by: None,
        locked_until: None,
        version: 0,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

#[test]
fn status_texts_match_the_js() {
    use bot_db::JobStatus::*;
    let g = golden();
    let mut live = new_restore_state("bk1", "full", Some("OWNER"), Some("C"), &IndexMap::new(), 1_700_000_000_000).to_json();
    live["counts"] = json!({ "rolesCreated": 1234, "categoriesCreated": 1, "channelsCreated": 2 });
    live["phase"] = json!("channelPositions");
    let mut missing = live.clone();
    missing["mode"] = json!("missing");
    missing["startedById"] = Value::Null;
    missing["phase"] = json!("finish");
    let s = &g["status"];
    assert_eq!(svc::restore_status_text(None, 0), s["none"].as_str().unwrap());
    assert_eq!(svc::restore_status_text(Some(&job_row(Running, live.clone(), None)), 0), s["live"].as_str().unwrap());
    assert_eq!(svc::restore_status_text(Some(&job_row(Pending, missing, None)), 0), s["liveMissing"].as_str().unwrap());
    let boom = "boom ".repeat(60);
    assert_eq!(svc::restore_status_text(Some(&job_row(Failed, live.clone(), Some(&boom))), 0), s["failed"].as_str().unwrap());
    assert_eq!(svc::restore_status_text(Some(&job_row(Failed, live.clone(), None)), 0), s["failedNull"].as_str().unwrap());
    assert_eq!(svc::restore_status_text(Some(&job_row(Done, live, None)), 0), s["done"].as_str().unwrap());
    for c in g["statusEmbeds"].as_array().unwrap() {
        let i = &c["input"];
        let settings = settings_of("1", i["settings"].clone());
        let newest = i["newest"]["createdAt"].as_str().and_then(bot_db::serde_js::parse_js_date);
        let embed = svc::build_status_embed(svc::StatusInput {
            settings: Some(&settings),
            counts: svc::BackupCounts { manual: i["counts"]["manual"].as_i64().unwrap(), automatic: i["counts"]["automatic"].as_i64().unwrap() },
            newest,
            restore_job: None,
            problems: i["problems"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect(),
            now_ms: now_ms(),
        });
        assert_eq!(embed_parts(&embed_json(&embed))["fields"], embed_parts(&c["embed"])["fields"]);
    }
}

// ── Golden: whole restores of the same fake guilds ────────────────────────────

#[tokio::test]
async fn restores_match_the_js_call_for_call() {
    let g = golden();
    let snapshot = snapshot();
    for (name, sc) in g["restores"].as_object().unwrap() {
        let f = Fake::from_spec(&sc["spec"], "900");
        for mode in ["missing", "full"] {
            let p = preview_restore(&f.view(), &snapshot, mode, &IndexMap::new());
            assert_eq!(serde_json::to_value(&p).unwrap(), sc["previews"][mode], "{name}: preview {mode}");
        }
        let mode = sc["mode"].as_str().unwrap();
        let run = restore_all(&f, mode, &snapshot, &IndexMap::new(), false, 60).await;
        let mut state = run.state.to_json();
        state["startedAt"] = json!(0);
        assert_eq!(state, sc["state"], "{name}: state");
        assert_eq!(run.batches as u64, sc["batches"].as_u64().unwrap(), "{name}: batches");
        let s = f.st();
        assert_eq!(json!(s.log.create), sc["log"]["create"], "{name}: creates");
        assert_eq!(json!(s.log.edit), sc["log"]["edit"], "{name}: edits");
        assert_eq!(json!(s.log.put), sc["log"]["put"], "{name}: overwrites");
        assert_eq!(json!(s.log.role_positions), sc["log"]["rolePositions"], "{name}: role order");
        assert_eq!(json!(s.log.channel_positions), sc["log"]["channelPositions"], "{name}: channel order");
        assert_eq!(json!(s.log.fetches), sc["log"]["fetches"], "{name}: member lookups");
        let roles: Vec<Value> = s
            .view
            .roles
            .values()
            .map(|r| json!({ "id": r.id, "name": r.name, "color": r.color, "hoist": r.hoist, "mentionable": r.mentionable, "permissions": r.permissions.to_string(), "rawPosition": r.raw_position }))
            .collect();
        assert_eq!(json!(roles), sc["after"]["roles"], "{name}: roles after");
        let channels: Vec<Value> = s
            .view
            .channels
            .values()
            .map(|c| {
                json!({ "id": c.id, "type": c.kind, "name": c.name, "parentId": c.parent_id, "rawPosition": c.raw_position, "topic": c.topic, "nsfw": c.nsfw,
                    "rateLimitPerUser": c.rate_limit_per_user, "bitrate": c.bitrate, "userLimit": c.user_limit,
                    "overwrites": c.overwrites.values().map(|o| json!({ "id": o.id, "type": o.kind.code(), "allow": o.allow.to_string(), "deny": o.deny.to_string() })).collect::<Vec<_>>() })
            })
            .collect();
        assert_eq!(json!(channels), sc["after"]["channels"], "{name}: channels after");
        assert_eq!(embed_parts(&embed_json(&svc::build_summary_embed(&run.state, None))), embed_parts(&sc["summary"]), "{name}: summary");
        assert_eq!(
            embed_parts(&embed_json(&svc::build_summary_embed(&run.state, Some("The backup was deleted, so the restore stopped.")))),
            embed_parts(&sc["summaryStopped"]),
            "{name}: stopped summary"
        );
        let row = svc::BackupInfo {
            id: "x".into(),
            guild_id: "900".into(),
            name: Some("Nightly".into()),
            automatic: false,
            created_by_id: None,
            size_bytes: 1,
            role_count: 1,
            channel_count: 1,
            created_at: bot_db::serde_js::parse_js_date("2026-10-21T10:00:00Z").unwrap(),
        };
        let p = preview_restore(&Fake::from_spec(&sc["spec"], "900").view(), &snapshot, mode, &IndexMap::new());
        assert_eq!(embed_parts(&embed_json(&svc::build_preview_embed(&row, &p))), embed_parts(&sc["previewEmbed"]), "{name}: preview embed");
    }
}

// ── Engine behaviour (ported tests) ───────────────────────────────────────────

fn empty_spec() -> Value {
    json!({ "botPerms": golden()["original"]["botPerms"], "roles": [{ "id": "999", "name": "Bot", "pos": 10 }], "members": ["777", "778"] })
}

#[tokio::test]
async fn one_batch_is_about_ten_calls_and_a_full_restore_takes_several() {
    let f = Fake::from_spec(&empty_spec(), "900");
    let run = restore_all(&f, "missing", &snapshot(), &IndexMap::new(), false, 40).await;
    assert!(run.batches >= 2);
    assert!(run.calls.iter().all(|n| *n <= budget::CALLS + 1), "{:?}", run.calls);
    assert_eq!(run.state.phase, "finish");
    assert_eq!(run.state.count("errors"), 0, "{:?}", run.state.errors);
}

#[tokio::test]
async fn a_crash_mid_batch_adopts_what_was_made_instead_of_making_it_twice() {
    let f = Fake::from_spec(&json!({ "botPerms": golden()["original"]["botPerms"], "roles": [{ "id": "999", "name": "Bot", "pos": 10 }], "members": ["777"] }), "900");
    let snapshot = snapshot();
    let started = now_ms() - 1000;
    let state = new_restore_state("bk000001", "missing", Some("OWNER"), Some("CH"), &IndexMap::new(), started);
    let api: &dyn RestoreApi = f.as_ref();
    let mut env = make_env(f.view(), &snapshot, state, "test", Some(api));
    assert_eq!(run_restore_batch(&mut env).await, BatchResult::More);
    assert!(f.st().log.create.len() >= 5);
    // The retry starts from the old payload, marked `retry`; its progress was lost.
    let run = restore_all(&f, "missing", &snapshot, &IndexMap::new(), true, 40).await;
    let names = f.role_names();
    assert_eq!(names.iter().filter(|n| *n == "Mod" || *n == "Member").count(), 2, "{names:?}");
    for n in ["Info", "general", "Lounge"] {
        assert_eq!(f.channel_names().iter().filter(|x| *x == n).count(), 1, "{n}");
    }
    assert_eq!(run.state.count("rolesCreated"), 3);
    assert!(run.state.errors.is_empty());
    assert!(run.state.id_map.len() >= 5);
}

#[tokio::test]
async fn running_the_whole_restore_again_creates_nothing_twice() {
    let f = Fake::from_spec(&json!({ "botPerms": golden()["original"]["botPerms"], "roles": [{ "id": "999", "name": "Bot", "pos": 10 }], "members": ["777"] }), "900");
    let snapshot = snapshot();
    let first = restore_all(&f, "missing", &snapshot, &IndexMap::new(), false, 40).await;
    let creates = f.st().log.create.len();
    let puts = f.st().log.put.len();
    let second = restore_all(&f, "missing", &snapshot, &first.state.id_map, false, 40).await;
    assert_eq!(f.st().log.create.len(), creates);
    assert_eq!(f.st().log.put.len(), puts);
    assert_eq!(second.state.count("rolesCreated") + second.state.count("channelsCreated"), 0);
    let third = restore_all(&f, "full", &snapshot, &first.state.id_map, false, 40).await;
    assert_eq!(third.state.count("rolesCreated") + third.state.count("channelsCreated") + third.state.count("categoriesCreated"), 0);
}

#[tokio::test]
async fn a_timed_out_create_is_retried_and_adopted_and_a_stuck_one_is_skipped() {
    let f = Fake::from_spec(&json!({ "botPerms": golden()["original"]["botPerms"], "roles": [{ "id": "999", "name": "Bot", "pos": 10 }], "members": ["777"] }), "900");
    *f.plan.lock().unwrap() = Plan::FirstTimesOutAfterMaking;
    let run = restore_all(&f, "missing", &snapshot(), &IndexMap::new(), false, 40).await;
    assert_eq!(f.role_names().iter().filter(|n| *n == "Member").count(), 1, "the role that appeared late is adopted");
    assert_eq!(run.state.count("errors"), 0);
    assert_eq!(run.state.stalls, 0);

    let stuck = Fake::from_spec(&json!({ "botPerms": golden()["original"]["botPerms"], "roles": [{ "id": "999", "name": "Bot", "pos": 10 }] }), "900");
    *stuck.plan.lock().unwrap() = Plan::AlwaysTimesOut;
    let out = restore_all(&stuck, "missing", &snapshot(), &IndexMap::new(), false, 200).await;
    assert_eq!(out.state.phase, "finish");
    assert_eq!(out.state.errors.iter().filter(|e| e.contains("did not answer in time")).count(), 3);
    assert!(stuck.channel_names().contains(&"general".to_string()));
}

#[tokio::test]
async fn a_recreated_private_channel_is_created_private() {
    let staff_allow = (1u128 << 10) | (1 << 11) | perm::MANAGE_ROLES;
    let spec = |bot: &str| {
        json!({ "botPerms": bot, "roles": [{ "id": "401", "name": "Staff", "perms": "3072", "pos": 4 }, { "id": "999", "name": "Bot", "pos": 10 }], "members": ["777"] })
    };
    let mut src = spec("0");
    src["channels"] = json!([{ "id": "402", "name": "staff-only", "pos": 0, "overwrites": [
        { "id": "960", "deny": "1024" }, { "id": "401", "allow": staff_allow.to_string() }, { "id": "777", "type": 1, "allow": "1024" }] }]);
    let snapshot = capture_guild(&Fake::from_spec(&src, "960").view(), Utc::now());
    let snapshot = parse_snapshot(&snapshot.to_json()).unwrap();
    for (bot, admin) in [(golden()["original"]["botPerms"].as_str().unwrap().to_string(), false), ("8".to_string(), true)] {
        let f = Fake::from_spec(&spec(&bot), "960");
        let mut state = new_restore_state("bk", "missing", None, None, &IndexMap::new(), now_ms() - 1000);
        // Batches of ONE call until the overwrites phase.
        for _ in 0..80 {
            if RESTORE_PHASES.iter().position(|p| *p == state.phase) >= RESTORE_PHASES.iter().position(|p| *p == "channelPositions") {
                break;
            }
            let api: &dyn RestoreApi = f.as_ref();
            let mut env = make_env(f.view(), &snapshot, state, "test", Some(api));
            env.max_calls = 1;
            run_restore_batch(&mut env).await;
            state = round(&env.state);
        }
        let made = f.st().view.channels.values().find(|c| c.name == "staff-only").cloned().expect("made");
        assert!(f.st().log.put.is_empty());
        assert_eq!(made.overwrites.get("960").map(|o| o.deny), Some(1024));
        assert_eq!(made.overwrites.get("401").map(|o| o.allow & perm::MANAGE_ROLES), Some(if admin { perm::MANAGE_ROLES } else { 0 }));
        assert!(!made.overwrites.contains_key("777"));
        for _ in 0..40 {
            if state.phase == "finish" {
                break;
            }
            let api: &dyn RestoreApi = f.as_ref();
            let mut env = make_env(f.view(), &snapshot, state, "test", Some(api));
            run_restore_batch(&mut env).await;
            state = round(&env.state);
        }
        let puts = f.st().log.put.clone();
        let to = |t: &str| puts.iter().find(|p| p["route"] == json!(format!("/channels/{}/permissions/{t}", made.id))).cloned();
        assert!(to("777").is_some());
        if admin {
            assert!(to("401").is_none());
        } else {
            assert_eq!(to("401").unwrap()["body"]["allow"].as_str().unwrap().parse::<u128>().unwrap() & perm::MANAGE_ROLES, perm::MANAGE_ROLES);
        }
        assert!(state.errors.is_empty(), "{:?}", state.errors);
    }
}

// sec-misc: adoption only after an earlier try, and only of what the audit log shows the bot made for this restore.
fn sec_snapshot() -> Snapshot {
    parse_snapshot(&json!({
        "version": 1, "takenAt": "2026-10-29T10:00:00.000Z", "guild": { "id": "900", "name": "Test Server" }, "everyone": { "permissions": "0" },
        "roles": [{ "id": "101", "name": "Moderator", "permissions": "2", "position": 6 }], "categories": [],
        "channels": [
            { "id": "201", "type": 0, "name": "mods", "position": 0, "overwrites": [{ "id": "900", "type": "role", "allow": "0", "deny": "1024" }, { "id": "101", "type": "role", "allow": "3072", "deny": "0" }] },
            { "id": "202", "type": 0, "name": "staff", "position": 1, "overwrites": [{ "id": "900", "type": "role", "allow": "0", "deny": "1024" }, { "id": "101", "type": "role", "allow": "1024", "deny": "0" }] }
        ]
    }))
    .unwrap()
}

fn sec_guild() -> Arc<Fake> {
    let spec = json!({ "botPerms": ((1u128 << 28) | (1 << 4) | (1 << 10) | (1 << 11)).to_string(), "roles": [{ "id": "999", "name": "Bot", "pos": 10 }],
        "channels": [{ "id": "201", "name": "mods", "overwrites": [{ "id": "900", "deny": "1024" }] }] });
    let f = Fake::from_spec(&spec, "900");
    f.st().next_id = 7000;
    f
}

async fn sec_restore(f: &Fake, retry: bool, stalls: i64) -> RestoreState {
    let snapshot = sec_snapshot();
    let mut state = new_restore_state("bk000001", "missing", Some("OWNER"), Some("CH"), &IndexMap::new(), now_ms() - 1000);
    state.retry = retry;
    state.stalls = stalls;
    for _ in 0..40 {
        state = round(&state);
        let api: &dyn RestoreApi = f;
        let mut env = make_env(f.view(), &snapshot, state, "Server backup restore test", Some(api));
        let r = run_restore_batch(&mut env).await;
        state = env.state.clone();
        state.retry = false;
        if r == BatchResult::Finish {
            return state;
        }
    }
    panic!("the restore did not finish");
}

fn role_targets(f: &Fake) -> Vec<String> {
    let s = f.st();
    let mut out: Vec<String> = s.log.role_positions.iter().flat_map(|l| l.as_array().unwrap().iter().map(|e| e["role"].as_str().unwrap().to_string()).collect::<Vec<_>>()).collect();
    out.extend(s.log.put.iter().map(|p| p["route"].as_str().unwrap().split("/permissions/").nth(1).unwrap().to_string()));
    out
}

#[tokio::test]
async fn a_strangers_same_named_role_or_channel_is_never_adopted() {
    const R: &str = "Server backup restore test";
    for (retry, stalls) in [(false, 0), (false, 1), (true, 0)] {
        let f = sec_guild();
        let stranger = f.add_role("Moderator", "STRANGER", None, now_ms());
        let state = sec_restore(&f, retry, stalls).await;
        assert_eq!(f.st().log.create.iter().filter(|c| c[0] == "role").count(), 1);
        let made = state.id_map.get("101").cloned().unwrap();
        assert_ne!(made, stranger);
        assert!(!state.made.contains(&stranger) && !role_targets(&f).contains(&stranger));
        assert!(f.st().log.put.iter().any(|p| p["route"] == json!(format!("/channels/201/permissions/{made}"))));

        let f = sec_guild();
        let stranger = f.add_channel("staff", None, "STRANGER", None, now_ms());
        let state = sec_restore(&f, retry, stalls).await;
        assert_eq!(f.st().log.create.iter().filter(|c| c[0] == "channel" && c[1]["name"] == "staff").count(), 1);
        let made = state.id_map.get("202").cloned().unwrap();
        assert_ne!(made, stranger);
        assert!(!f.st().log.put.iter().any(|p| p["route"].as_str().unwrap().split('/').nth(2) == Some(stranger.as_str())));
        assert!(f.st().view.channels[&made].overwrites.contains_key("900"));
    }
    // What the bot made for this restore is adopted on a retry (a stranger's twin does not shadow it).
    for (retry, stalls) in [(false, 1), (true, 0)] {
        let f = sec_guild();
        let stranger = f.add_role("Moderator", "STRANGER", None, now_ms());
        let own = f.add_role("Moderator", "9999", Some(R), now_ms());
        let state = sec_restore(&f, retry, stalls).await;
        assert_eq!(f.st().log.create.iter().filter(|c| c[0] == "role").count(), 0);
        assert_eq!(state.id_map.get("101"), Some(&own));
        assert!(!role_targets(&f).contains(&stranger));
    }
    let f = sec_guild();
    let _stranger = f.add_channel("staff", None, "STRANGER", None, now_ms());
    let own = f.add_channel("staff", None, "9999", Some(R), now_ms());
    let state = sec_restore(&f, true, 0).await;
    assert_eq!(f.st().log.create.iter().filter(|c| c[0] == "channel").count(), 0);
    assert_eq!(state.id_map.get("202"), Some(&own));
}

#[tokio::test]
async fn never_adopted_on_a_first_try_or_without_audit_proof() {
    const R: &str = "Server backup restore test";
    let f = sec_guild();
    let early = f.add_role("Moderator", "9999", Some(R), now_ms());
    let state = sec_restore(&f, false, 0).await;
    assert_ne!(state.id_map.get("101"), Some(&early));
    for kind in ["fails", "none"] {
        let base = sec_guild();
        let mut f = Arc::try_unwrap(base).ok().unwrap();
        if kind == "fails" { f.audit_fails = true } else { f.no_audit = true }
        let candidate = f.add_role("Moderator", "9999", Some(R), now_ms());
        let state = sec_restore(&f, true, 0).await;
        assert_ne!(state.id_map.get("101"), Some(&candidate));
        assert!(state.errors.is_empty());
    }
    let f = sec_guild();
    let other = f.add_role("Moderator", "9999", Some("/liverole by someone (1)"), now_ms());
    assert_ne!(sec_restore(&f, true, 0).await.id_map.get("101"), Some(&other));
    let f = sec_guild();
    let old = f.add_role("Moderator", "9999", Some(R), now_ms() - 3_600_000);
    assert_ne!(sec_restore(&f, true, 0).await.id_map.get("101"), Some(&old));
}

#[test]
fn the_restore_never_deletes() {
    let root = bot_core::testing::repo_root().join("rust/crates/feat-backups/src");
    for file in ["guild_snapshot.rs", "live_guild.rs", "backup_service.rs"] {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let code: String = text.lines().map(|l| l.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n");
        for bad in ["delete_role", "delete_channel", "delete_permission", "LightMethod::Delete", "kick", "ban_user", ".ban(", "remove_member_role"] {
            assert!(!code.contains(bad), "{file} has a destructive call: {bad}");
        }
        assert!(!code.contains("DELETE FROM \"ServerBackup\" WHERE \"id\" = $1\"") , "{file}: only deletes scoped by guild");
    }
    let live = std::fs::read_to_string(root.join("live_guild.rs")).unwrap();
    assert!(live.contains("create_permission("), "overwrites are upserted one target at a time");
}

// ── Declarations ──────────────────────────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    assert_commands_match_golden(&reg, &["backup", "emoji"]);
    assert_plugin_matches_golden(&reg, "backups");
}

#[tokio::test]
async fn delete_and_restore_refuse_everyone_but_the_owner_before_deferring() {
    use bot_core::command::option_type::STRING;
    let guild = bot_core::testing::FakeGuild::new("940000000000000001", "940000000000000009")
        .role("940000000000000005", 3, serenity::all::Permissions::ADMINISTRATOR)
        .member("940000000000000002", &["940000000000000005"]);
    let t = TestApp::new().with_guilds(["940000000000000001"]).with_fake_guilds(&[guild]);
    t.install(registry());
    for sub in ["delete", "restore"] {
        let i = FakeInteraction::slash("backup").guild("940000000000000001").user("940000000000000002").roles(&["940000000000000005"]).permissions(serenity::all::Permissions::ADMINISTRATOR).subcommand(sub)
            .option("backup", STRING, json!("cm1")).build();
        bot_core::router::route_interaction(&t, i).await;
    }
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["reply", "reply"]);
    for c in calls {
        assert_eq!(c.embed_title(), Some("❌  Server Owner Only"));
        assert!(c.ephemeral());
    }
}

#[tokio::test]
async fn on_guild_ready_seeds_backup_auto_only_for_guilds_that_use_the_module() {
    let t = TestApp::new().with_guilds(["920000000000000001", "920000000000000002", "920000000000000003"]);
    t.install(registry());
    t.settings_store.put(settings_of("920000000000000001", json!({ "backupsEnabled": false, "backupIntervalHours": 0 })));
    t.settings_store.put(settings_of("920000000000000002", json!({ "backupsEnabled": true, "backupIntervalHours": 0 })));
    t.settings_store.put(settings_of("920000000000000003", json!({ "backupsEnabled": false, "backupIntervalHours": 12 })));
    for g in ["920000000000000001", "920000000000000002", "920000000000000003"] {
        t.run_hooks(guild_ready(g)).await;
    }
    let keys: Vec<String> = t.job_store.all().iter().filter_map(|j| j.dedupe_key.clone()).collect();
    assert!(!keys.contains(&"backup.auto:920000000000000001".to_string()));
    assert!(keys.contains(&"backup.auto:920000000000000002".to_string()));
    assert!(keys.contains(&"backup.auto:920000000000000003".to_string()));
    // A failed job is revived; a healthy one is only pulled earlier.
    t.job_store.modify("backup.auto:920000000000000002", |j| j.status = bot_db::JobStatus::Failed);
    assert!(svc::ensure_auto_job(&t, "920000000000000002", None).await);
    let job = t.job_store.all().into_iter().find(|j| j.dedupe_key.as_deref() == Some("backup.auto:920000000000000002")).unwrap();
    assert_eq!(job.status, bot_db::JobStatus::Pending);
    let later = job.run_at + chrono::Duration::hours(5);
    svc::ensure_auto_job(&t, "920000000000000002", Some(later)).await;
    let again = t.job_store.all().into_iter().find(|j| j.dedupe_key.as_deref() == Some("backup.auto:920000000000000002")).unwrap();
    assert_eq!(again.run_at, job.run_at, "a healthy job is never pushed later");
}

fn guild_ready(g: &str) -> bot_core::hooks::events::GuildReady {
    bot_core::hooks::events::GuildReady { guild_id: serenity::all::GuildId::new(g.parse().unwrap()), reason: bot_core::hooks::events::GuildReadyReason::Startup }
}

// ── With PostgreSQL: the service, the jobs and the JS ─────────────────────────

async fn db_app(guilds: &[&str]) -> Option<DbTestApp> {
    let url = bot_core::testing::database_url()?;
    let t = DbTestApp::new(&url).await.with_guilds(guilds.iter().map(|g| g.to_string()));
    t.reset_guilds(guilds).await;
    t.install(registry());
    Some(t)
}

fn view_of(spec: &Value, id: &str) -> GuildView {
    Fake::from_spec(spec, id).view()
}

#[tokio::test]
async fn create_backup_caps_manual_ones_and_refuses_oversize() {
    let (g1, g2, g3) = ("910000000000000001", "910000000000000002", "910000000000000003");
    let Some(t) = db_app(&[g1, g2, g3]).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    let view = view_of(&original(), g1);
    for i in 0..bot_config::limits::BACKUPS_MANUAL {
        let r = svc::create_backup(&t, &view, Some("U1"), Some(&format!("  manual\n{i} ")), false).await.unwrap();
        let CreateResult::Ok(b) = r else { panic!("{r:?}") };
        if i == 0 {
            assert_eq!(b.name.as_deref(), Some("manual 0"));
        }
    }
    assert!(matches!(svc::create_backup(&t, &view, Some("U1"), None, false).await.unwrap(), CreateResult::Refused { reason: "cap", .. }));
    let CreateResult::Ok(auto) = svc::create_backup(&t, &view, Some("U1"), None, true).await.unwrap() else { panic!() };
    assert!(auto.automatic && auto.created_by_id.is_none());
    // Concurrent manual creates cannot jump the cap.
    let v2 = view_of(&original(), g2);
    let futs: Vec<_> = (0..bot_config::limits::BACKUPS_MANUAL + 4).map(|_| svc::create_backup(&t, &v2, Some("U1"), None, false)).collect();
    let oks = futures::future::join_all(futs).await.into_iter().filter(|r| matches!(r, Ok(CreateResult::Ok(_)))).count();
    assert_eq!(oks, bot_config::limits::BACKUPS_MANUAL);
    // Oversize.
    let mut huge = view_of(&json!({}), g3);
    for i in 0..25000 {
        let id = (100000 + i).to_string();
        huge.roles.insert(id.clone(), RoleView { id, name: "y".repeat(100), color: 0, hoist: false, mentionable: false, permissions: 0, raw_position: i, unicode_emoji: None, managed: false });
    }
    assert!(matches!(svc::create_backup(&t, &huge, Some("U1"), None, false).await.unwrap(), CreateResult::Refused { reason: "size", .. }));
    // Lists, counts, prune.
    let counts = svc::count_backups(t.db(), g1).await.unwrap();
    assert_eq!((counts.manual, counts.automatic), (10, 1));
    t.reset_guilds(&[g1, g2, g3]).await;
}

#[tokio::test]
async fn prune_keeps_the_newest_automatic_ones_and_never_manual_or_spared() {
    let g = "914000000000000001";
    let Some(t) = db_app(&[g]).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    let view = view_of(&json!({}), g);
    let mut manual = vec![];
    for i in 0..3 {
        let CreateResult::Ok(b) = svc::create_backup(&t, &view, Some("U1"), Some(&format!("m{i}")), false).await.unwrap() else { panic!() };
        manual.push(b.id);
    }
    let mut autos = vec![];
    for i in 0..6 {
        let CreateResult::Ok(b) = svc::create_backup(&t, &view, None, None, true).await.unwrap() else { panic!() };
        sqlx::query(r#"UPDATE "ServerBackup" SET "createdAt" = $1 WHERE "id" = $2"#)
            .bind((Utc::now() - chrono::Duration::minutes(60 - i)).naive_utc())
            .bind(&b.id)
            .execute(t.db())
            .await
            .unwrap();
        autos.push(b.id);
    }
    assert_eq!(svc::prune_automatic(t.db(), g, Some(3), &[]).await.unwrap(), 3);
    let left: Vec<String> = svc::list_backups(t.db(), g).await.unwrap().into_iter().map(|r| r.id).collect();
    for id in &autos[3..] {
        assert!(left.contains(id));
    }
    for id in &manual {
        assert!(left.contains(id));
    }
    assert_eq!(svc::prune_automatic(t.db(), g, Some(0), std::slice::from_ref(&autos[3])).await.unwrap(), 1, "keep 0 acts as keep 1; the spared one stays");
    assert_eq!(svc::prune_automatic(t.db(), g, Some(9999), &[]).await.unwrap(), 0);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn backup_auto_idles_backs_up_when_due_prunes_and_pauses_during_a_restore() {
    let g = "915000000000000001";
    let Some(t) = db_app(&[g]).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    let view = view_of(&json!({}), g);
    let set = |on: bool, hours: i32, keep: i32| {
        let t = &t;
        async move {
            t.settings().update_settings(g, SettingsPatch::new().set("backupsEnabled", on).set("backupIntervalHours", hours).set("backupKeep", keep)).await.unwrap();
        }
    };
    let wait = |o: &JobOutcome| match o {
        JobOutcome::Reschedule { next_run_at, .. } => next_run_at.timestamp_millis() - now_ms(),
        _ => panic!("recurring"),
    };
    let hour = 3_600_000;
    set(false, 6, 2).await;
    let out = svc::handle_auto_job_with(&t, g, Some(&view)).await;
    assert!(wait(&out) <= hour + 1000 && wait(&out) > 50 * 60_000);
    set(true, 0, 7).await;
    svc::handle_auto_job_with(&t, g, Some(&view)).await;
    assert_eq!(svc::count_backups(t.db(), g).await.unwrap().automatic, 0);
    set(true, 6, 2).await;
    let out = svc::handle_auto_job_with(&t, g, Some(&view)).await;
    assert_eq!(svc::count_backups(t.db(), g).await.unwrap().automatic, 1);
    assert!(wait(&out) <= hour + 1000);
    svc::handle_auto_job_with(&t, g, Some(&view)).await;
    assert_eq!(svc::count_backups(t.db(), g).await.unwrap().automatic, 1, "not due again yet");
    // Old ones: the newest is 7 hours old and two older ones exist; one new is made and two are kept.
    sqlx::query(r#"UPDATE "ServerBackup" SET "createdAt" = $1 WHERE "guildId" = $2"#).bind((Utc::now() - chrono::Duration::hours(7)).naive_utc()).bind(g).execute(t.db()).await.unwrap();
    for i in 0..2 {
        let CreateResult::Ok(b) = svc::create_backup(&t, &view, None, None, true).await.unwrap() else { panic!() };
        sqlx::query(r#"UPDATE "ServerBackup" SET "createdAt" = $1 WHERE "id" = $2"#).bind((Utc::now() - chrono::Duration::hours(30 + i)).naive_utc()).bind(&b.id).execute(t.db()).await.unwrap();
    }
    let CreateResult::Ok(mine) = svc::create_backup(&t, &view, Some("U1"), Some("mine"), false).await.unwrap() else { panic!() };
    svc::handle_auto_job_with(&t, g, Some(&view)).await;
    let counts = svc::count_backups(t.db(), g).await.unwrap();
    assert_eq!(counts.automatic, 2, "backupKeep 2");
    assert!(svc::get_backup(t.db(), g, &mine.id).await.unwrap().is_some());
    // A running restore pauses automatic backups.
    sqlx::query(r#"UPDATE "ServerBackup" SET "createdAt" = $1 WHERE "guildId" = $2"#).bind((Utc::now() - chrono::Duration::hours(40)).naive_utc()).bind(g).execute(t.db()).await.unwrap();
    let snapshot = parse_snapshot(&golden()["snapshot"]).unwrap();
    svc::start_restore(&t, &view, &mine.id, &snapshot, "missing", Some("OWNER"), Some("CH")).await.unwrap().unwrap();
    let before = svc::count_backups(t.db(), g).await.unwrap();
    let out = svc::handle_auto_job_with(&t, g, Some(&view)).await;
    assert_eq!(svc::count_backups(t.db(), g).await.unwrap(), before);
    assert!(wait(&out) > 0);
    // Refused while a restore runs.
    assert!(matches!(svc::create_backup(&t, &view, Some("U1"), None, false).await.unwrap(), CreateResult::Refused { reason: "restoring", .. }));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn the_restore_job_runs_batch_by_batch_posts_the_summary_and_remembers_ids() {
    let g = "931000000000000001";
    let Some(t) = db_app(&[g]).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    let f = Fake::from_spec(&json!({ "botPerms": original()["botPerms"], "roles": [{ "id": "999", "name": "Bot", "pos": 10 }], "members": ["777"], "channels": [{ "id": "8888", "name": "admin" }] }), g);
    f.usable.lock().unwrap().insert("8888".into());
    let CreateResult::Ok(row) = svc::create_backup(&t, &view_of(&original(), g), Some("OWNER"), Some("seed"), false).await.unwrap() else { panic!() };
    // The stored snapshot is the original server (guild id g): the restore makes it again in the fake.
    let data = svc::get_backup_with_data(t.db(), g, &row.id).await.unwrap().unwrap().data;
    let snapshot = parse_snapshot(&data).unwrap();
    assert_eq!(svc::start_restore(&t, &f.view(), &row.id, &snapshot, "missing", Some("OWNER"), Some("8888")).await.unwrap(), Ok(()));
    assert_eq!(svc::start_restore(&t, &f.view(), &row.id, &snapshot, "full", Some("OWNER"), Some("8888")).await.unwrap(), Err(svc::StartRefused::Running));
    assert_eq!(svc::start_restore(&t, &f.view(), &row.id, &snapshot, "bogus", None, None).await.unwrap(), Err(svc::StartRefused::Mode));
    let job_row = svc::get_restore_job(&t, g).await.unwrap();
    assert_eq!((job_row.r#type.as_str(), job_row.max_attempts, job_row.payload["phase"].clone(), job_row.payload["mode"].clone()), ("backup.restore", 5, json!("roles"), json!("missing")));
    let mut job = bot_core::scheduler::Job::from_row(&job_row);
    let mut steps = 0;
    loop {
        let out = svc::handle_restore_job_with(&t, &job, 100, 1, f.as_ref()).await.unwrap();
        steps += 1;
        match out {
            JobOutcome::Done => break,
            JobOutcome::Reschedule { next_run_at, payload } => {
                let ms = next_run_at.timestamp_millis() - now_ms();
                assert!(ms > 0 && ms <= 3000);
                job.payload = payload.unwrap();
            }
        }
        assert!(steps < 40);
    }
    assert!(steps >= 2);
    let sent = f.st().sent.clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1.as_deref(), Some("<@OWNER>"));
    assert_eq!(sent[0].3.as_deref(), Some("OWNER"));
    assert!(sent[0].2["title"].as_str().unwrap().contains("Restore Finished"));
    assert!(sent[0].2["fields"].as_array().unwrap().iter().any(|f| f["name"] == "Roles" && f["value"].as_str().unwrap().contains("created 3")));
    let stored = svc::get_backup_with_data(t.db(), g, &row.id).await.unwrap().unwrap().data;
    let id102 = stored["idMap"]["102"].as_str().unwrap().to_string();
    assert!(f.view().roles.contains_key(&id102));
    assert!(exportable(&stored).get("idMap").is_none());
    // A second restore of the same backup seeds from that map and makes nothing again.
    let creates = f.st().log.create.len();
    sqlx::query(r#"DELETE FROM "ScheduledJob" WHERE "dedupeKey" = $1"#).bind(svc::restore_job_key(g)).execute(t.db()).await.unwrap();
    let snapshot2 = parse_snapshot(&stored).unwrap();
    svc::start_restore(&t, &f.view(), &row.id, &snapshot2, "missing", Some("OWNER"), Some("8888")).await.unwrap().unwrap();
    let mut job = bot_core::scheduler::Job::from_row(&svc::get_restore_job(&t, g).await.unwrap());
    assert!(job.payload["idMap"].as_object().unwrap().len() >= 5);
    for _ in 0..40 {
        match svc::handle_restore_job_with(&t, &job, 0, 1, f.as_ref()).await.unwrap() {
            JobOutcome::Done => break,
            JobOutcome::Reschedule { payload, .. } => job.payload = payload.unwrap(),
        }
    }
    assert_eq!(f.st().log.create.len(), creates, "no second copy");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn the_restore_job_stops_cleanly_and_a_retry_waits_for_a_batch_in_flight() {
    let g = "933000000000000001";
    let Some(t) = db_app(&[g]).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    let f = Fake::from_spec(&json!({ "botPerms": original()["botPerms"], "roles": [{ "id": "999", "name": "Bot", "pos": 10 }], "channels": [{ "id": "8888", "name": "admin" }] }), g);
    f.usable.lock().unwrap().insert("8888".into());
    let base = new_restore_state("gone", "missing", Some("OWNER"), Some("8888"), &IndexMap::new(), now_ms());
    let job = |payload: Value| bot_core::scheduler::Job {
        id: "j".into(),
        guild_id: g.into(),
        job_type: "backup.restore".into(),
        payload,
        run_at: Utc::now(),
        attempts: 1,
        max_attempts: 5,
        dedupe_key: None,
        version: 0,
    };
    let last_desc = |f: &Fake| f.st().sent.last().unwrap().2["description"].as_str().unwrap().to_string();
    assert_eq!(svc::handle_restore_job_with(&t, &job(base.to_json()), 0, 1, f.as_ref()).await.unwrap(), JobOutcome::Done);
    assert!(last_desc(&f).contains("backup was deleted"));
    assert!(f.st().sent.last().unwrap().2["title"].as_str().unwrap().contains("Stopped"));
    // Unreadable data.
    t.settings().ensure_guild_settings(g).await.unwrap();
    let bad: bot_db::ServerBackup = bot_db::InsertBuilder::new(&bot_db::SERVER_BACKUP_INFO)
        .values([("guildId", json!(g)), ("name", json!("bad")), ("automatic", json!(false)), ("data", json!({ "version": 9 })), ("sizeBytes", json!(1)), ("roleCount", json!(0)), ("channelCount", json!(0))])
        .unwrap()
        .build()
        .unwrap()
        .fetch_one(t.db())
        .await
        .unwrap();
    let mut p = base.clone();
    p.backup_id = bad.id.clone();
    assert_eq!(svc::handle_restore_job_with(&t, &job(p.to_json()), 0, 1, f.as_ref()).await.unwrap(), JobOutcome::Done);
    assert!(last_desc(&f).contains("version 9"));
    // Stale: stopped as FAILED (permanent), nothing made.
    let CreateResult::Ok(good) = svc::create_backup(&t, &view_of(&original(), g), Some("OWNER"), None, false).await.unwrap() else { panic!() };
    let mut p = base.clone();
    p.backup_id = good.id.clone();
    let err = svc::handle_restore_job_with(&t, &job(p.to_json()), 3 * 3_600_000, 1, f.as_ref()).await.unwrap_err();
    assert!(PermanentJobError::is(&err));
    assert!(last_desc(&f).contains("waited too long"));
    assert!(f.st().log.create.is_empty());
    // A bad payload is permanent.
    assert!(PermanentJobError::is(&svc::handle_restore_job_with(&t, &job(json!({ "nope": true })), 0, 1, f.as_ref()).await.unwrap_err()));
    // No usable channel: the starter gets a DM.
    let lonely = Fake::from_spec(&json!({ "botPerms": original()["botPerms"], "roles": [{ "id": "999", "name": "Bot", "pos": 10 }] }), g);
    let mut p = base.clone();
    p.channel_id = Some("NOPE".into());
    svc::handle_restore_job_with(&t, &job(p.to_json()), 0, 1, lonely.as_ref()).await.unwrap();
    assert_eq!(lonely.st().dms.len(), 1);
    assert_eq!(lonely.st().dms[0].0, "OWNER");
    // Manage Roles taken away while the restore runs: stopped (permanent), nothing made.
    let weak = Fake::from_spec(&json!({ "botPerms": ((1u128 << 4) | (1 << 10) | (1 << 11) | (1 << 14)).to_string(), "roles": [{ "id": "999", "name": "Bot", "pos": 10 }], "channels": [{ "id": "8888", "name": "admin" }] }), g);
    weak.usable.lock().unwrap().insert("8888".into());
    let mut p = base.clone();
    p.backup_id = good.id.clone();
    assert!(PermanentJobError::is(&svc::handle_restore_job_with(&t, &job(p.to_json()), 0, 1, weak.as_ref()).await.unwrap_err()));
    assert!(weak.st().log.create.is_empty());
    assert!(weak.st().sent.last().unwrap().2["description"].as_str().unwrap().contains("Manage Roles"));
    // A batch already in flight makes a retry wait, with the payload left alone (marked retry).
    let held = Fake::from_spec(&json!({ "botPerms": original()["botPerms"], "roles": [{ "id": "999", "name": "Bot", "pos": 10 }] }), g);
    *held.plan.lock().unwrap() = Plan::Hold;
    let mut p = base.clone();
    p.backup_id = good.id.clone();
    let payload = p.to_json();
    let (t2, h2, j2) = (t.app.clone(), held.clone(), job(payload.clone()));
    let running = tokio::spawn(async move { svc::handle_restore_job_with(&t2, &j2, 0, 1, h2.as_ref()).await });
    for _ in 0..200 {
        if held.st().role_creates > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let retry = svc::handle_restore_job_with(&t, &job(payload.clone()), 0, 2, held.as_ref()).await.unwrap();
    let JobOutcome::Reschedule { payload: Some(rp), .. } = retry else { panic!("{retry:?}") };
    let mut want = payload.clone();
    want["retry"] = json!(true);
    assert_eq!(rp, want);
    held.hold.notify_one();
    let _ = running.await.unwrap();
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn a_stopped_restore_is_continued_and_strangers_are_never_adopted() {
    let g = "961000000000000001";
    let Some(t) = db_app(&[g]).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    let f = Fake::from_spec(&json!({ "botPerms": original()["botPerms"], "roles": [{ "id": "999", "name": "Bot", "pos": 10 }], "members": ["777"], "channels": [{ "id": "8888", "name": "admin" }] }), g);
    f.usable.lock().unwrap().insert("8888".into());
    let CreateResult::Ok(row) = svc::create_backup(&t, &view_of(&original(), g), Some("OWNER"), None, false).await.unwrap() else { panic!() };
    let snapshot = parse_snapshot(&svc::get_backup_with_data(t.db(), g, &row.id).await.unwrap().unwrap().data).unwrap();
    svc::start_restore(&t, &f.view(), &row.id, &snapshot, "missing", Some("OWNER"), Some("8888")).await.unwrap().unwrap();
    let mut state = normalise_restore_state(&svc::get_restore_job(&t, g).await.unwrap().payload, now_ms()).unwrap();
    let started = state.started_at;
    for _ in 0..80 {
        if state.phase == "channelPositions" {
            break;
        }
        let api: &dyn RestoreApi = f.as_ref();
        let mut env = make_env(f.view(), &snapshot, state, "test", Some(api));
        env.max_calls = 1;
        run_restore_batch(&mut env).await;
        state = round(&env.state);
    }
    state.started_at = started - 3_600_000;
    sqlx::query(r#"UPDATE "ScheduledJob" SET "status" = 'FAILED', "payload" = $1 WHERE "dedupeKey" = $2"#).bind(state.to_json()).bind(svc::restore_job_key(g)).execute(t.db()).await.unwrap();
    let info = f.view().channels.values().find(|c| c.name == "Info").unwrap().id.clone();
    let stranger_role = f.add_role("Admin", "STRANGER", None, now_ms());
    let stranger_channel = f.add_channel("general", Some(&info), "STRANGER", None, now_ms());
    let creates = f.st().log.create.len();
    assert!(creates >= 7);
    let prev = svc::get_restore_job(&t, g).await;
    assert_eq!(svc::restore_preview(&f.view(), &snapshot, "missing", None, None, now_ms()).roles.create, 3);
    assert_eq!(svc::restore_preview(&f.view(), &snapshot, "missing", prev.as_ref(), Some(&row.id), now_ms()).roles.create, 0);
    assert_eq!(svc::carried_from(&f.view(), prev.as_ref(), "other", now_ms()), svc::Carried::default());
    assert_eq!(svc::start_restore(&t, &f.view(), &row.id, &snapshot, "missing", Some("OWNER"), Some("8888")).await.unwrap(), Ok(()));
    let mut job = bot_core::scheduler::Job::from_row(&svc::get_restore_job(&t, g).await.unwrap());
    assert!(job.payload["startedAt"].as_i64().unwrap() >= started);
    assert!(job.payload["made"].as_array().unwrap().len() >= 7);
    for _ in 0..40 {
        match svc::handle_restore_job_with(&t, &job, 0, 1, f.as_ref()).await.unwrap() {
            JobOutcome::Done => break,
            JobOutcome::Reschedule { payload, .. } => job.payload = payload.unwrap(),
        }
    }
    assert_eq!(f.st().log.create.len(), creates, "nothing is made a second time");
    assert_eq!(f.role_names().iter().filter(|n| *n == "Admin").count(), 2);
    assert!(!f.st().log.put.iter().any(|p| p["route"].as_str().unwrap().starts_with(&format!("/channels/{stranger_channel}/"))));
    assert!(!role_targets(&f).contains(&stranger_role));
    let general = f.view().channels.values().find(|c| c.name == "general" && c.id != stranger_channel).unwrap().id.clone();
    assert!(f.st().log.put.iter().any(|p| p["route"] == json!(format!("/channels/{general}/permissions/777"))));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn raw_sql_runs_in_a_rolled_back_transaction() {
    let Some(url) = bot_core::testing::database_url() else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let mut tx = pool.begin().await.unwrap();
    for (name, sql) in svc::RAW_SQL {
        let q = sqlx::query(*sql).bind("900000000000000001");
        let q = if sql.contains("$2") { q.bind(1_i64) } else { q };
        q.fetch_all(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    tx.rollback().await.unwrap();
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_backups_restore_under_rust_and_rust_backups_under_js() {
    let (gj, gr) = ("970000000000000001", "970000000000000002");
    let Some(t) = db_app(&[gj, gr]).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    if bot_core::testing::redis_url().is_none() {
        return eprintln!("skipped: TEST_REDIS_URL not set");
    }
    // JS writes, Rust reads and restores.
    let js = run_node("rust/fixtures/gen/feat-backups-js.mjs", &["create", gj, "from js 100%_x"]);
    assert!(js.get("error").is_none(), "{js}");
    let id = js["res"]["backup"]["id"].as_str().unwrap().to_string();
    let row = svc::get_backup_with_data(t.db(), gj, &id).await.unwrap().unwrap();
    assert_eq!(row.data, js["data"]);
    let parsed = parse_snapshot(&row.data).unwrap();
    let info = svc::BackupInfo::of(&row);
    assert_eq!(serde_json::to_value(&info).unwrap(), js["res"]["backup"]);
    let f = Fake::from_spec(&empty_spec(), gj);
    let run = restore_all(&f, "missing", &parsed, &IndexMap::new(), false, 40).await;
    let js_read = run_node("rust/fixtures/gen/feat-backups-js.mjs", &["read", gj, &id]);
    assert_eq!(js_read["parsed"], parsed.to_parsed_json());
    assert_eq!(serde_json::to_value(&run.state.counts).unwrap(), js_read["counts"]);
    assert_eq!(serde_json::to_value(&run.state.id_map).unwrap(), js_read["idMap"]);
    assert_eq!(svc::backup_label(&info), js_read["label"].as_str().unwrap());

    // Rust writes, JS reads and restores the same way.
    let CreateResult::Ok(made) = svc::create_backup(&t, &view_of(&original(), gr), Some("U1"), Some("from rust"), false).await.unwrap() else { panic!() };
    let js_read = run_node("rust/fixtures/gen/feat-backups-js.mjs", &["read", gr, &made.id]);
    assert!(js_read.get("error").is_none(), "{js_read}");
    assert_eq!(js_read["info"], serde_json::to_value(&made).unwrap());
    let rust_parsed = parse_snapshot(&svc::get_backup_with_data(t.db(), gr, &made.id).await.unwrap().unwrap().data).unwrap();
    assert_eq!(js_read["parsed"], rust_parsed.to_parsed_json());
    let f = Fake::from_spec(&empty_spec(), gr);
    let run = restore_all(&f, "missing", &rust_parsed, &IndexMap::new(), false, 40).await;
    assert_eq!(serde_json::to_value(&run.state.counts).unwrap(), js_read["counts"]);

    // Lists and the autocomplete search (wildcards are literal).
    svc::create_backup(&t, &view_of(&original(), gj), Some("U1"), Some("from js 1000 x"), true).await.unwrap();
    for q in ["", "JS", "100%", "%", "_x", "from"] {
        let js = run_node("rust/fixtures/gen/feat-backups-js.mjs", &["lists", gj, q]);
        let ours: Vec<String> = svc::search_backups(t.db(), gj, q).await.unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(json!(ours), js["search"], "search {q:?}");
        let list: Vec<String> = svc::list_backups(t.db(), gj).await.unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(json!(list), js["list"]);
        assert_eq!(serde_json::to_value(svc::count_backups(t.db(), gj).await.unwrap()).unwrap(), js["counts"]);
        assert_eq!(json!(svc::newest_automatic(t.db(), gj).await.unwrap().map(|x| x.0)), js["newest"]);
    }
    t.reset_guilds(&[gj, gr]).await;
}

#[tokio::test]
async fn restore_jobs_cross_between_js_and_rust() {
    let (gj, gr) = ("971000000000000001", "971000000000000002");
    let Some(t) = db_app(&[gj, gr]).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    if bot_core::testing::redis_url().is_none() {
        return eprintln!("skipped: TEST_REDIS_URL not set");
    }
    // JS schedules the restore; Rust runs the job it left to the end.
    let CreateResult::Ok(row) = svc::create_backup(&t, &view_of(&original(), gj), Some("U1"), None, false).await.unwrap() else { panic!() };
    let js = run_node("rust/fixtures/gen/feat-backups-js.mjs", &["start", gj, &row.id]);
    assert_eq!(js["res"], json!({ "ok": true }), "{js}");
    let job_row = svc::get_restore_job(&t, gj).await.unwrap();
    assert_eq!(job_row.payload, js["job"]["payload"]);
    assert_eq!((job_row.max_attempts, job_row.r#type.as_str()), (5, "backup.restore"));
    let state = normalise_restore_state(&job_row.payload, now_ms()).unwrap();
    assert_eq!(state.backup_id, row.id);
    let f = Fake::from_spec(&empty_spec(), gj);
    let mut job = bot_core::scheduler::Job::from_row(&job_row);
    for _ in 0..40 {
        match svc::handle_restore_job_with(&t, &job, 0, 1, f.as_ref()).await.unwrap() {
            JobOutcome::Done => break,
            JobOutcome::Reschedule { payload, .. } => job.payload = payload.unwrap(),
        }
    }
    assert_eq!(f.role_names().iter().filter(|n| *n == "Mod").count(), 1);
    // Rust schedules; JS reads the job and its payload the same way.
    let CreateResult::Ok(row) = svc::create_backup(&t, &view_of(&original(), gr), Some("U1"), None, false).await.unwrap() else { panic!() };
    let fr = Fake::from_spec(&empty_spec(), gr);
    svc::start_restore(&t, &fr.view(), &row.id, &parse_snapshot(&golden()["snapshot"]).unwrap(), "full", Some("OWNER"), Some("CH")).await.unwrap().unwrap();
    let js = run_node("rust/fixtures/gen/feat-backups-js.mjs", &["job", gr]);
    let ours = svc::get_restore_job(&t, gr).await.unwrap();
    assert_eq!(js["job"]["payload"], ours.payload);
    assert_eq!(js["job"]["dedupeKey"], json!(svc::restore_job_key(gr)));
    assert_eq!(js["job"]["maxAttempts"], json!(5));
    assert_eq!(js["state"], normalise_restore_state(&ours.payload, 0).unwrap().to_json());
    t.reset_guilds(&[gj, gr]).await;
}

// ── /backup and the Confirm button through the router (with PostgreSQL) ───────

const ROWNER: &str = "980000000000000009";
const RADMIN: &str = "980000000000000002";
const RADMIN_ROLE: &str = "980000000000000005";
const RBOT_ROLE: &str = "980000000000000006";
const RCHAN: &str = "980000000000000010";

#[allow(non_snake_case)]
async fn router_app(RG: &str, bot: serenity::all::Permissions) -> Option<DbTestApp> {
    use serenity::all::Permissions as P;
    let t = db_app(&[RG]).await?;
    let guild = bot_core::testing::FakeGuild::new(RG, ROWNER)
        .role(RADMIN_ROLE, 3, P::ADMINISTRATOR)
        .role(RBOT_ROLE, 5, bot)
        .member(ROWNER, &[RADMIN_ROLE])
        .member(RADMIN, &[RADMIN_ROLE])
        .member(bot_core::testing::TEST_BOT_ID, &[RBOT_ROLE])
        .channel(RCHAN, 0, "general", &[]);
    t.set_discord(bot_core::testing::offline_discord_as(&[guild.json()]));
    Some(t)
}

#[allow(non_snake_case)]
fn slash(RG: &str, user: &str, sub: &str) -> FakeInteraction {
    FakeInteraction::slash("backup").guild(RG).user(user).roles(&[RADMIN_ROLE]).channel(RCHAN).permissions(serenity::all::Permissions::ADMINISTRATOR).subcommand(sub)
}

#[allow(non_snake_case)]
fn button(RG: &str, user: &str, custom_id: &str, age_ms: i64) -> serenity::all::Interaction {
    let mut b = FakeInteraction::button(custom_id).guild(RG).user(user).roles(&[RADMIN_ROLE]).channel(RCHAN).message_id("1000000000000000100");
    let ts = (Utc::now() - chrono::Duration::milliseconds(age_ms)).to_rfc3339_opts(chrono::SecondsFormat::Micros, false);
    b.json_mut()["message"]["timestamp"] = json!(ts);
    b.build()
}

fn bot_ok() -> serenity::all::Permissions {
    use serenity::all::Permissions as P;
    P::MANAGE_ROLES | P::MANAGE_CHANNELS | P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS
}

#[tokio::test]
async fn backup_subcommands_answer_privately_and_restore_shows_a_confirm() {
    const RG: &str = "980000000000000001";
    use bot_core::command::option_type::STRING;
    use bot_core::router::route_interaction;
    let Some(t) = router_app(RG, bot_ok()).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    t.settings().update_settings(RG, SettingsPatch::new().set("backupsEnabled", true).set("backupIntervalHours", 12).set("backupKeep", 3)).await.unwrap();
    route_interaction(&t, slash(RG, RADMIN, "create").option("name", STRING, json!("before the raid")).build()).await;
    let last = t.responses.calls().last().cloned().unwrap();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(t.responses.calls()[0].ephemeral());
    assert_eq!(last.embed_title(), Some("✅  Backup Saved"));
    let row = svc::list_backups(t.db(), RG).await.unwrap().remove(0);
    assert_eq!(row.name.as_deref(), Some("before the raid"));
    assert_eq!(row.created_by_id.as_deref(), Some(RADMIN));
    for _ in 0..100 {
        if t.scheduler().get_job(&svc::auto_job_key(RG)).await.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(t.scheduler().get_job(&svc::auto_job_key(RG)).await.is_some(), "an admin touching the module keeps the schedule alive");

    t.responses.clear();
    route_interaction(&t, slash(RG, RADMIN, "list").build()).await;
    assert!(t.responses.calls().last().unwrap().embed_description().unwrap().contains("before the raid"));
    t.responses.clear();
    route_interaction(&t, slash(RG, RADMIN, "info").option("backup", STRING, json!(row.id)).build()).await;
    assert!(t.responses.calls().last().unwrap().embed_title().unwrap().contains("before the raid"));
    t.responses.clear();
    route_interaction(&t, slash(RG, RADMIN, "export").option("backup", STRING, json!(row.id)).build()).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("✅  Backup Exported"));
    let full = svc::get_backup_with_data(t.db(), RG, &row.id).await.unwrap().unwrap();
    let (name, bytes) = feat_backups::commands::backup::export_file(&full);
    assert!(regex::Regex::new(r"^backup-\d{8}-[a-z0-9]{8}\.json$").unwrap().is_match(&name), "{name}");
    let exported: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(exported["version"], json!(1));
    t.responses.clear();
    route_interaction(&t, slash(RG, RADMIN, "status").build()).await;
    let body = t.responses.calls().last().unwrap().body.to_string();
    assert!(body.contains("Every **12** hours") && body.contains("1 / 10 manual"), "{body}");
    t.responses.clear();
    route_interaction(&t, slash(RG, RADMIN, "info").option("backup", STRING, json!("nope")).build()).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("❌  Backup Not Found"));

    // Restore (owner): the preview with Restore and Cancel; nothing is scheduled yet.
    t.responses.clear();
    route_interaction(&t, slash(RG, ROWNER, "restore").option("backup", STRING, json!(row.id)).option("mode", STRING, json!("full")).build()).await;
    let last = t.responses.calls().last().cloned().unwrap();
    assert_eq!(last.embed_title(), Some("⚠️  Restore This Backup?"));
    let ids: Vec<Value> = last.body["components"][0]["components"].as_array().unwrap().iter().map(|c| c["custom_id"].clone()).collect();
    assert_eq!(ids, vec![json!(format!("backup:restore:{}:full", row.id)), json!("backup:cancel")]);
    assert!(svc::get_restore_job(&t, RG).await.is_none());
    t.responses.clear();
    route_interaction(&t, slash(RG, ROWNER, "restore").option("backup", STRING, json!(row.id)).build()).await;
    assert_eq!(t.responses.calls().last().unwrap().body["components"][0]["components"][0]["custom_id"], json!(format!("backup:restore:{}:missing", row.id)));

    // Autocomplete: name and date, this guild only, and nothing for `mode`.
    t.responses.clear();
    let ac = FakeInteraction::autocomplete("backup").guild(RG).user(RADMIN).roles(&[RADMIN_ROLE]).permissions(serenity::all::Permissions::ADMINISTRATOR).subcommand("info").focused("backup", "befo").build();
    route_interaction(&t, ac).await;
    let choices = t.responses.calls().last().unwrap().choices();
    assert_eq!(choices.len(), 1);
    assert!(regex::Regex::new(r"^before the raid · \d{4}-\d{2}-\d{2} \d{2}:\d{2} UTC$").unwrap().is_match(choices[0]["name"].as_str().unwrap()));
    t.responses.clear();
    let ac = FakeInteraction::autocomplete("backup").guild(RG).user(RADMIN).roles(&[RADMIN_ROLE]).permissions(serenity::all::Permissions::ADMINISTRATOR).subcommand("restore").focused("mode", "").build();
    route_interaction(&t, ac).await;
    assert!(t.responses.calls().last().unwrap().choices().is_empty());
    t.reset_guilds(&[RG]).await;
}

#[tokio::test]
async fn the_confirm_button_is_the_owners_and_schedules_the_restore_once() {
    const RG: &str = "980000000000000011";
    let Some(t) = router_app(RG, bot_ok()).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    use bot_core::router::route_interaction;
    let live = feat_backups::live_guild::LiveGuild::new(&t, RG);
    let CreateResult::Ok(row) = svc::create_backup(&t, &RestoreApi::view(&live), Some(ROWNER), Some("seed"), false).await.unwrap() else { panic!() };
    let restore = format!("backup:restore:{}:full", row.id);
    // A server administrator who is not the owner is refused, even for Cancel.
    for id in [restore.as_str(), "backup:cancel"] {
        t.responses.clear();
        route_interaction(&t, button(RG, RADMIN, id, 1000)).await;
        assert_eq!(t.responses.names(), vec!["reply"]);
        assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Server Owner Only"));
        assert!(t.responses.calls()[0].ephemeral());
    }
    assert!(svc::get_restore_job(&t, RG).await.is_none());
    // Expired (judged from the message).
    t.responses.clear();
    route_interaction(&t, button(RG, ROWNER, &restore, svc::backup::CONFIRM_TTL_MS + 5000)).await;
    assert_eq!(t.responses.names(), vec!["update"]);
    assert_eq!(t.responses.calls()[0].embed_title(), Some("⚠️  Confirmation Expired"));
    // Unknown backup, bad mode, cancel.
    t.responses.clear();
    route_interaction(&t, button(RG, ROWNER, "backup:restore:nope:missing", 1000)).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("❌  Backup Not Found"));
    t.responses.clear();
    route_interaction(&t, button(RG, ROWNER, &format!("backup:restore:{}:everything", row.id), 1000)).await;
    assert_eq!(t.responses.names(), vec!["reply"]);
    t.responses.clear();
    route_interaction(&t, button(RG, ROWNER, "backup:cancel", 1000)).await;
    assert_eq!(t.responses.names(), vec!["update"]);
    assert_eq!(t.responses.calls()[0].embed_title(), Some("ℹ️  Restore Cancelled"));
    assert!(svc::get_restore_job(&t, RG).await.is_none());
    // The owner confirms: scheduled with the mode; a second Confirm is refused.
    t.responses.clear();
    route_interaction(&t, button(RG, ROWNER, &restore, 1000)).await;
    assert_eq!(t.responses.names(), vec!["deferUpdate", "editReply"]);
    let last = t.responses.calls().last().cloned().unwrap();
    assert_eq!(last.embed_title(), Some("✅  Restore Started"));
    assert_eq!(last.body["components"], json!([]));
    let job = svc::get_restore_job(&t, RG).await.unwrap();
    assert_eq!(
        (job.payload["mode"].clone(), job.payload["backupId"].clone(), job.payload["channelId"].clone(), job.payload["startedById"].clone()),
        (json!("full"), json!(row.id), json!(RCHAN), json!(ROWNER))
    );
    t.responses.clear();
    route_interaction(&t, button(RG, ROWNER, &format!("backup:restore:{}:missing", row.id), 1000)).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("⚠️  Restore Already Running"));
    assert_eq!(svc::get_restore_job(&t, RG).await.unwrap().version, job.version);
    // /backup restore and delete are refused while it runs.
    t.responses.clear();
    route_interaction(&t, slash(RG, ROWNER, "restore").option("backup", bot_core::command::option_type::STRING, json!(row.id)).build()).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("⚠️  Restore Already Running"));
    t.responses.clear();
    route_interaction(&t, slash(RG, ROWNER, "delete").option("backup", bot_core::command::option_type::STRING, json!(row.id)).build()).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("⚠️  Restore Running"));
    assert!(svc::get_backup(t.db(), RG, &row.id).await.unwrap().is_some());
    t.reset_guilds(&[RG]).await;
}

#[tokio::test]
async fn without_manage_roles_nothing_is_scheduled_and_delete_confirms() {
    const RG: &str = "980000000000000021";
    use bot_core::command::option_type::STRING;
    use bot_core::router::route_interaction;
    use serenity::all::Permissions as P;
    let Some(t) = router_app(RG, P::MANAGE_CHANNELS | P::VIEW_CHANNEL | P::SEND_MESSAGES).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    let live = feat_backups::live_guild::LiveGuild::new(&t, RG);
    let CreateResult::Ok(row) = svc::create_backup(&t, &RestoreApi::view(&live), Some(ROWNER), None, false).await.unwrap() else { panic!() };
    route_interaction(&t, slash(RG, ROWNER, "restore").option("backup", STRING, json!(row.id)).build()).await;
    let last = t.responses.calls().last().cloned().unwrap();
    assert_eq!(last.embed_title(), Some("❌  Missing Permissions"));
    assert!(last.embed_description().unwrap().contains("Manage Roles"));
    assert!(last.body.get("components").is_none_or(|c| c.as_array().is_none_or(|a| a.is_empty())));
    t.responses.clear();
    route_interaction(&t, button(RG, ROWNER, &format!("backup:restore:{}:missing", row.id), 1000)).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("❌  Missing Permissions"));
    assert!(svc::get_restore_job(&t, RG).await.is_none());
    // Delete: confirmed through the collector.
    t.responses.clear();
    let t2 = t.app.clone();
    let interaction = slash(RG, ROWNER, "delete").option("backup", STRING, json!(row.id)).build();
    let run = tokio::spawn(async move { route_interaction(&t2, interaction).await });
    for _ in 0..300 {
        if !t.collectors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let confirm = t
        .responses
        .calls()
        .iter()
        .rev()
        .find_map(|c| c.body["components"][0]["components"][0]["custom_id"].as_str().map(String::from))
        .expect("a confirm prompt");
    let click = FakeInteraction::button(&confirm).guild(RG).user(ROWNER).roles(&[RADMIN_ROLE]).message_id("1000000000000000100").build();
    route_interaction(&t, click).await;
    run.await.unwrap();
    assert!(svc::get_backup(t.db(), RG, &row.id).await.unwrap().is_none());
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("✅  Backup Deleted"));
    t.reset_guilds(&[RG]).await;
}

// ── Audit: behaviour checked against discord.js ───────────────────────────────

#[tokio::test]
async fn backup_create_and_status_fail_like_the_js_when_the_guild_is_not_cached() {
    // `createBackup({ guild: interaction.guild })` and `guild.members.me` throw for a missing guild: "Could Not Finish",
    // and no (empty) backup is stored.
    const RG: &str = "980000000000000031";
    use bot_core::router::route_interaction;
    let Some(t) = db_app(&[RG]).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    t.set_discord(bot_core::testing::offline_discord_as(&[]));
    for sub in ["create", "status"] {
        t.responses.clear();
        route_interaction(&t, slash(RG, RADMIN, sub).build()).await;
        assert_eq!(t.responses.calls().last().and_then(|c| c.embed_title().map(String::from)).as_deref(), Some("❌  Could Not Finish"), "{sub}");
    }
    assert!(svc::list_backups(t.db(), RG).await.unwrap().is_empty(), "no empty backup was stored");
    t.reset_guilds(&[RG]).await;
}

#[tokio::test]
async fn what_a_restore_creates_is_cached_at_once_like_discord_js() {
    // discord.js `roles.create` / `channels.create` add the result to the cache, so the next batch and the summary see
    // it before the gateway event arrives. Role order goes as one whole-list PATCH without a reason (setPositions).
    use feat_backups::live_guild::LiveGuild;
    const G: &str = "980000000000000041";
    let rest = bot_core::testing::FakeRest::new();
    let guild = bot_core::testing::FakeGuild::new(G, ROWNER).member(bot_core::testing::TEST_BOT_ID, &[]);
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[guild], &rest);
    rest.on("POST", "/guilds/*/roles", 200, json!({
        "id": "980000000000000042", "name": "Muted", "color": 0, "hoist": false, "position": 1, "permissions": "0", "managed": false,
        "mentionable": false, "flags": 0, "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    }));
    rest.on("POST", "/guilds/*/channels", 201, json!({
        "id": "980000000000000043", "type": 0, "guild_id": G, "name": "rules", "position": 3, "permission_overwrites": [], "nsfw": false
    }));
    rest.on("PATCH", "/guilds/*/roles", 200, json!([]));
    let live = LiveGuild::new(&t, G);
    let data = RoleData { name: Some("Muted".into()), ..Default::default() };
    live.create_role(&data, "test").await.expect("role");
    let ch = ChannelCreate { name: "rules".into(), kind: 0, position: 3, ..Default::default() };
    live.create_channel(&ch, "test").await.expect("channel");
    let view = RestoreApi::view(&live);
    assert!(view.roles.contains_key("980000000000000042"), "the created role is in the cache");
    assert!(view.channels.contains_key("980000000000000043"), "the created channel is in the cache");
    live.set_role_positions(&[("980000000000000042".into(), 2)]).await.expect("positions");
    let patches = rest.calls_to("PATCH", &format!("/guilds/{G}/roles"));
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].body, json!([{ "id": "980000000000000042", "position": 2 }]));
    assert_eq!(patches[0].reason, None);
}

#[test]
fn a_call_after_the_batch_deadline_gets_the_js_two_second_timeout() {
    // `Math.max(2000, Math.min(CALL_MS, left + 5000))`: `left` goes negative past the deadline (an item's second call).
    use feat_backups::guild_snapshot::call_timeout_ms;
    assert_eq!(call_timeout_ms(18_000), 12_000);
    assert_eq!(call_timeout_ms(1_000), 6_000);
    assert_eq!(call_timeout_ms(0), 5_000);
    assert_eq!(call_timeout_ms(-2_000), 3_000);
    assert_eq!(call_timeout_ms(-4_000), 2_000);
    assert_eq!(call_timeout_ms(i64::MIN), 2_000);
}
