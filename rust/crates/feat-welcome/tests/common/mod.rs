//! Shared helpers of the feat-welcome tests: the golden file, settings rows built like the JS tests' `settings(over)`,
//! serenity members and guilds shaped like the JS fakes, and a recording Discord fake.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_core::error::BoxFuture;
use bot_core::interaction_helpers::Payload;
use bot_db::GuildSettings;
use feat_welcome::io::{DiscordIo, IoError};
use serde_json::{Value, json};
use serenity::all::{ChannelId, Guild, GuildId, Member, MessageId, RoleId, User, UserId};

pub const GID: &str = "900000000000000501";
pub const UID: &str = "111111111111111111";
pub const OWNER: &str = "600000000000000099";
pub const BOT_ROLE: &str = "820000000000000001";
pub const CH: &str = "200000000000000001";
pub const CH_MUTE: &str = "200000000000000002";
pub const CH_NO_EMBEDS: &str = "200000000000000003";
pub const CH_NO_FILES: &str = "200000000000000004";

pub fn gid() -> GuildId {
    GuildId::new(GID.parse().unwrap_or(1))
}

pub fn golden() -> Value {
    bot_core::testing::load_golden("feat-welcome/golden.json")
}

/// The JS `settings(over)`: the required-fields row of the mock database, then `BASE`, then `over` (guild `GID`).
pub fn settings(over: &Value) -> GuildSettings {
    let baseline = bot_core::testing::load_golden("mockdb/baseline.json");
    let mut row = baseline["GuildSettings"]["min"].clone();
    for (k, v) in golden()["base"].as_object().into_iter().flatten().chain(over.as_object().into_iter().flatten()) {
        row[k] = v.clone();
    }
    row["guildId"] = json!(GID);
    serde_json::from_value(row).unwrap_or_else(|e| panic!("settings row: {e}"))
}

/// Same JSON (key order ignored), or a panic naming the first difference.
pub fn same(got: &Value, expected: &Value, what: &str) {
    if let Some(d) = bot_core::testing::json_diff(got, expected, false) {
        panic!("{what}: {d}\n  rust: {got}\n  js:   {expected}");
    }
}

/// Same JSON, key order included.
pub fn same_ordered(got: &Value, expected: &Value, what: &str) {
    if let Some(d) = bot_core::testing::json_diff(got, expected, true) {
        panic!("{what}: {d}\n  rust: {got}\n  js:   {expected}");
    }
}

/// An embed as JSON with its timestamp masked (`'<ts>'`).
pub fn embed_json(e: &serenity::builder::CreateEmbed) -> Value {
    mask_ts(serde_json::to_value(e).unwrap_or(Value::Null))
}

pub fn mask_ts(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        if o.contains_key("timestamp") {
            o.insert("timestamp".into(), json!("<ts>"));
        }
        if o.get("type") == Some(&json!("rich")) {
            o.shift_remove("type");
        }
    }
    v
}

/// Drops what Discord reads as absent: `null` values and `inline: false` (serenity writes both, discord.js does not).
pub fn clean(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter().filter(|(k, x)| !x.is_null() && !(k.as_str() == "inline" && **x == json!(false))).map(|(k, x)| (k.clone(), clean(x))).collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(clean).collect()),
        other => other.clone(),
    }
}

/// [`same`] after [`clean`] on both sides.
pub fn same_clean(got: &Value, expected: &Value, what: &str) {
    same(&clean(got), &clean(expected), what);
}

/// A built welcome payload's JSON in the golden file's shape.
pub fn normalize_payload(mut v: Value) -> Value {
    if let Some(embeds) = v.get_mut("embeds").and_then(Value::as_array_mut) {
        for e in embeds.iter_mut() {
            *e = mask_ts(e.take());
        }
    }
    v
}

/// A guild like the JS fake (`name`, 22 members) as serenity deserializes it.
pub fn test_guild(name: &str) -> Guild {
    let mut g = bot_core::testing::FakeGuild::new(GID, OWNER).json();
    g["name"] = json!(name);
    g["member_count"] = json!(22);
    let event: serenity::all::GuildCreateEvent = serde_json::from_value(g).unwrap_or_else(|e| panic!("guild: {e}"));
    event.guild
}

/// The new member of the JS fakes (`newbie`, id `UID`, no avatar).
pub fn test_member() -> Member {
    serde_json::from_value(json!({
        "user": { "id": UID, "username": "newbie", "discriminator": "0", "avatar": null, "global_name": null, "bot": false },
        "nick": null, "roles": [], "joined_at": "2023-11-14T22:13:20.000000+00:00", "deaf": false, "mute": false, "flags": 0,
        "pending": false, "guild_id": GID,
    }))
    .unwrap_or_else(|e| panic!("member: {e}"))
}

pub fn user(id: &str, name: &str) -> User {
    serde_json::from_value(json!({ "id": id, "username": name, "discriminator": "0", "avatar": null, "global_name": null, "bot": false }))
        .unwrap_or_else(|e| panic!("user: {e}"))
}

pub fn member_of(id: &str, roles: &[&str]) -> Member {
    serde_json::from_value(json!({
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "bot": false },
        "roles": roles, "joined_at": "2023-07-22T04:26:40.000000+00:00", "deaf": false, "mute": false, "flags": 0, "pending": false, "guild_id": GID,
    }))
    .unwrap_or_else(|e| panic!("member: {e}"))
}

/// A payload as JSON: `{ content, embeds, components, files, allowed_mentions }` (`null` for unset parts).
pub fn payload_json(p: &Payload) -> Value {
    json!({
        "content": p.content,
        "embeds": p.embeds.as_ref().map(|e| e.iter().map(|x| mask_ts(serde_json::to_value(x).unwrap_or(Value::Null))).collect::<Vec<_>>()),
        "components": p.components.as_ref().map(|c| c.iter().map(|x| serde_json::to_value(x).unwrap_or(Value::Null)).collect::<Vec<_>>()),
        "files": p.files.iter().map(|f| f.filename.clone()).collect::<Vec<_>>(),
        "allowed_mentions": p.allowed_mentions.as_ref().map(|m| serde_json::to_value(m).unwrap_or(Value::Null)),
    })
}

/// Message ids unique across every test of the process (`reviewMessageId` and `panelMessageId` are unique columns).
fn next_message_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let base = 700_000_000_000_000_000 + (chrono::Utc::now().timestamp_millis() as u64 % 1_000_000_000) * 1_000;
    let _ = NEXT.compare_exchange(0, base, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

#[derive(Default)]
struct State {
    sent: Vec<(String, Value)>,
    dms: Vec<(String, Value)>,
    roles: Vec<(String, String, Option<String>)>,
    edits: Vec<(String, String, Value)>,
    deletes: Vec<(String, String)>,
    cards: usize,
    send_failures: VecDeque<Option<u64>>,
    dm_failures: VecDeque<Option<u64>>,
    role_failures: VecDeque<Option<u64>>,
    fetch_failures: VecDeque<Option<u64>>,
    members: Vec<Member>,
}

/// A recording Discord: every call is recorded; failures are queued per call kind (`Some(code)` fails once with that
/// Discord code).
#[derive(Default)]
pub struct FakeIo(Mutex<State>);

fn fail(q: &mut VecDeque<Option<u64>>) -> Option<IoError> {
    q.pop_front().map(|c| IoError::new(c, format!("api error {}", c.unwrap_or(0))))
}

impl FakeIo {
    pub fn install(app: &App) -> Arc<FakeIo> {
        let io = Arc::new(FakeIo::default());
        feat_welcome::io::set_io(app, io.clone());
        io
    }
    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn sent(&self) -> Vec<(String, Value)> {
        self.st().sent.clone()
    }
    pub fn dms(&self) -> Vec<(String, Value)> {
        self.st().dms.clone()
    }
    pub fn roles(&self) -> Vec<(String, String, Option<String>)> {
        self.st().roles.clone()
    }
    pub fn edits(&self) -> Vec<(String, String, Value)> {
        self.st().edits.clone()
    }
    pub fn deletes(&self) -> Vec<(String, String)> {
        self.st().deletes.clone()
    }
    pub fn cards(&self) -> usize {
        self.st().cards
    }
    pub fn fail_sends(&self, codes: &[Option<u64>]) {
        self.st().send_failures.extend(codes.iter().copied());
    }
    pub fn fail_dms(&self, codes: &[Option<u64>]) {
        self.st().dm_failures.extend(codes.iter().copied());
    }
    pub fn fail_roles(&self, codes: &[Option<u64>]) {
        self.st().role_failures.extend(codes.iter().copied());
    }
    pub fn fail_fetches(&self, codes: &[Option<u64>]) {
        self.st().fetch_failures.extend(codes.iter().copied());
    }
    /// A member `fetch_member` returns (by user id; the last one set wins).
    pub fn set_member(&self, m: Member) {
        let mut st = self.st();
        st.members.retain(|x| x.user.id != m.user.id);
        st.members.push(m);
    }
    pub fn remove_member(&self, id: &str) {
        self.st().members.retain(|x| x.user.id.to_string() != id);
    }
}

impl DiscordIo for FakeIo {
    fn send_message(&self, channel_id: ChannelId, payload: Payload) -> BoxFuture<Result<MessageId, IoError>> {
        let mut st = self.st();
        st.sent.push((channel_id.to_string(), payload_json(&payload)));
        let r = match fail(&mut st.send_failures) {
            Some(e) => Err(e),
            None => Ok(MessageId::new(next_message_id())),
        };
        Box::pin(async move { r })
    }
    fn send_dm(&self, user_id: UserId, payload: Payload) -> BoxFuture<Result<(), IoError>> {
        let mut st = self.st();
        st.dms.push((user_id.to_string(), payload_json(&payload)));
        let r = match fail(&mut st.dm_failures) {
            Some(e) => Err(e),
            None => Ok(()),
        };
        Box::pin(async move { r })
    }
    fn add_role(&self, _guild_id: GuildId, user_id: UserId, role_id: RoleId, reason: Option<String>) -> BoxFuture<Result<(), IoError>> {
        let mut st = self.st();
        st.roles.push((user_id.to_string(), role_id.to_string(), reason));
        let r = match fail(&mut st.role_failures) {
            Some(e) => Err(e),
            None => Ok(()),
        };
        Box::pin(async move { r })
    }
    fn fetch_member(&self, _guild_id: GuildId, user_id: UserId) -> BoxFuture<Result<Member, IoError>> {
        let mut st = self.st();
        let r = match fail(&mut st.fetch_failures) {
            Some(e) => Err(e),
            None => st.members.iter().find(|m| m.user.id == user_id).cloned().ok_or_else(|| IoError::new(Some(10007), "Unknown Member")),
        };
        Box::pin(async move { r })
    }
    fn fetch_user(&self, user_id: UserId) -> BoxFuture<Result<User, IoError>> {
        let id = user_id.to_string();
        Box::pin(async move { Ok(user(&id, &format!("user{id}"))) })
    }
    fn edit_message(&self, channel_id: ChannelId, message_id: MessageId, payload: Payload) -> BoxFuture<Result<(), IoError>> {
        self.st().edits.push((channel_id.to_string(), message_id.to_string(), payload_json(&payload)));
        Box::pin(async { Ok(()) })
    }
    fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> BoxFuture<Result<(), IoError>> {
        self.st().deletes.push((channel_id.to_string(), message_id.to_string()));
        Box::pin(async { Ok(()) })
    }
    fn render_card(&self, _data: bot_cards::WelcomeCardData, _style: bot_cards::WelcomeCardStyle) -> BoxFuture<Result<Vec<u8>, String>> {
        self.st().cards += 1;
        Box::pin(async { Ok(vec![0x89, b'P', b'N', b'G']) })
    }
    fn validate_background(&self, url: String) -> BoxFuture<bot_cards::welcome_card::BackgroundCheck> {
        Box::pin(async move {
            if url.contains("bad") {
                bot_cards::welcome_card::BackgroundCheck::Invalid { reason: "I couldn't load that image.".into() }
            } else {
                bot_cards::welcome_card::BackgroundCheck::Ok
            }
        })
    }
}
