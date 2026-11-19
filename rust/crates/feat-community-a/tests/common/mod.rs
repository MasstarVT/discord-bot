//! Shared test helpers: a recording stand-in for Discord REST ([`FakeIo`]), message and guild builders.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_core::interaction_helpers::Payload;
use feat_community_a::io::{CommunityIo, IoError, set_io};
use serde_json::{Value, json};
use serenity::all::{ChannelId, Message, MessageId, User, UserId};

/// One recorded REST call.
#[derive(Debug, Clone)]
pub struct IoCall {
    pub method: &'static str,
    pub channel: String,
    pub message: Option<String>,
    /// The JSON Discord would have received (`content`, `embeds`, `components`, `allowed_mentions`), plus
    /// `files`: the attachment names.
    pub body: Value,
}

impl IoCall {
    pub fn content(&self) -> &str {
        self.body["content"].as_str().unwrap_or("")
    }
    pub fn embed(&self) -> &Value {
        &self.body["embeds"][0]
    }
}

/// Discord REST, recorded. A method listed in `fail` returns that error (until cleared).
#[derive(Default)]
pub struct FakeIo {
    pub calls: Mutex<Vec<IoCall>>,
    pub fail: Mutex<HashMap<&'static str, IoError>>,
    pub messages: Mutex<HashMap<String, Message>>,
    pub users: Mutex<HashMap<String, User>>,
    /// Channels `fetch_channel` finds (everything else is 10003).
    pub channels: Mutex<Vec<String>>,
    /// Runs after each successful `send`, before it returns (a write that races the caller's next step).
    pub on_send: Mutex<Option<OnSend>>,
}

/// A hook [`FakeIo::send`] awaits after recording the message.
pub type OnSend = Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// Message ids are unique across every test of a run (`messageId` is a unique column), and across runs.
static NEXT_ID: std::sync::LazyLock<AtomicU64> = std::sync::LazyLock::new(|| {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    AtomicU64::new(1_000_000_000_000_000_000 + (ms % 1_000_000_000) * 1000)
});

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::SeqCst)
}

impl FakeIo {
    pub fn install(app: &bot_core::App) -> Arc<FakeIo> {
        let io = Arc::new(FakeIo::default());
        set_io(app, Some(io.clone()));
        io
    }
    pub fn calls(&self) -> Vec<IoCall> {
        self.calls.lock().unwrap().clone()
    }
    pub fn of(&self, method: &str) -> Vec<IoCall> {
        self.calls().into_iter().filter(|c| c.method == method).collect()
    }
    pub fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }
    pub fn fail_with(&self, method: &'static str, code: Option<u64>) {
        self.fail.lock().unwrap().insert(method, IoError::new(code, format!("fake {method} failure")));
    }
    pub fn heal(&self) {
        self.fail.lock().unwrap().clear();
    }
    fn failure(&self, method: &str) -> Option<IoError> {
        self.fail.lock().unwrap().get(method).cloned()
    }
    fn record(&self, method: &'static str, channel: impl ToString, message: Option<String>, body: Value) {
        self.calls.lock().unwrap().push(IoCall { method, channel: channel.to_string(), message, body });
    }
    pub fn add_user(&self, id: &str, name: &str) {
        let u: User = serde_json::from_value(json!({ "id": id, "username": name, "global_name": name, "discriminator": "0", "avatar": null })).unwrap();
        self.users.lock().unwrap().insert(id.to_string(), u);
    }
}

pub fn payload_json(p: &Payload) -> Value {
    let mut v = serde_json::to_value(p.to_message()).unwrap_or(Value::Null);
    v["files"] = json!(p.files.iter().map(|f| f.filename.clone()).collect::<Vec<_>>());
    v
}

#[async_trait]
impl CommunityIo for FakeIo {
    async fn send(&self, channel_id: ChannelId, payload: Payload) -> Result<MessageId, IoError> {
        if let Some(e) = self.failure("send") {
            return Err(e);
        }
        let id = next_id();
        self.record("send", channel_id, Some(id.to_string()), payload_json(&payload));
        let hook = self.on_send.lock().unwrap().clone();
        if let Some(h) = hook {
            h().await;
        }
        Ok(MessageId::new(id))
    }
    async fn edit(&self, channel_id: ChannelId, message_id: MessageId, payload: Payload) -> Result<(), IoError> {
        if let Some(e) = self.failure("edit") {
            return Err(e);
        }
        self.record("edit", channel_id, Some(message_id.to_string()), payload_json(&payload));
        Ok(())
    }
    async fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<(), IoError> {
        if let Some(e) = self.failure("delete_message") {
            return Err(e);
        }
        self.record("delete_message", channel_id, Some(message_id.to_string()), Value::Null);
        Ok(())
    }
    async fn fetch_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<Message, IoError> {
        if let Some(e) = self.failure("fetch_message") {
            return Err(e);
        }
        self.record("fetch_message", channel_id, Some(message_id.to_string()), Value::Null);
        self.messages.lock().unwrap().get(&message_id.to_string()).cloned().ok_or_else(|| IoError::new(Some(10008), "Unknown Message"))
    }
    async fn fetch_channel(&self, channel_id: ChannelId) -> Result<(), IoError> {
        if let Some(e) = self.failure("fetch_channel") {
            return Err(e);
        }
        if self.channels.lock().unwrap().contains(&channel_id.to_string()) { Ok(()) } else { Err(IoError::new(Some(10003), "Unknown Channel")) }
    }
    async fn start_thread(&self, channel_id: ChannelId, message_id: MessageId, name: &str, reason: &str) -> Result<ChannelId, IoError> {
        if let Some(e) = self.failure("start_thread") {
            return Err(e);
        }
        let id = next_id();
        self.record("start_thread", channel_id, Some(message_id.to_string()), json!({ "name": name, "reason": reason, "id": id.to_string() }));
        Ok(ChannelId::new(id))
    }
    async fn delete_channel(&self, channel_id: ChannelId, reason: &str) -> Result<(), IoError> {
        if let Some(e) = self.failure("delete_channel") {
            return Err(e);
        }
        self.record("delete_channel", channel_id, None, json!({ "reason": reason }));
        Ok(())
    }
    async fn fetch_user(&self, user_id: UserId) -> Result<User, IoError> {
        if let Some(e) = self.failure("fetch_user") {
            return Err(e);
        }
        self.users.lock().unwrap().get(&user_id.to_string()).cloned().ok_or_else(|| IoError::new(Some(10013), "Unknown User"))
    }
    async fn dm(&self, user_id: UserId, payload: Payload) -> Result<(), IoError> {
        if let Some(e) = self.failure("dm") {
            return Err(e);
        }
        self.record("dm", user_id, None, payload_json(&payload));
        Ok(())
    }
}

/// A guild message JSON by `author` (with `bot`), embeds and an optional interaction-metadata user.
pub fn bot_message(id: &str, guild: &str, channel: &str, author: &str, embeds: Value, bumper: Option<(&str, bool)>, timestamp: &str) -> Message {
    let mut j = json!({
        "id": id, "channel_id": channel, "guild_id": guild,
        "author": { "id": author, "username": "DISBOARD", "discriminator": "0", "avatar": null, "bot": true },
        "content": "", "timestamp": timestamp, "edited_timestamp": null, "tts": false,
        "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": embeds, "pinned": false,
        "type": 20, "components": []
    });
    if let Some((uid, bot)) = bumper {
        j["interaction_metadata"] = json!({
            "id": "1300000000000000001", "type": 2, "authorizing_integration_owners": {},
            "user": { "id": uid, "username": format!("user{uid}"), "discriminator": "0", "avatar": null, "bot": bot }
        });
    }
    serde_json::from_value(j).unwrap_or_else(|e| panic!("message does not deserialize: {e}"))
}

/// The embed fields of a recorded interaction answer, as `(name, value)`.
pub fn fields_of(body: &Value) -> Vec<(String, String)> {
    body["embeds"][0]["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| (f["name"].as_str().unwrap_or("").to_string(), f["value"].as_str().unwrap_or("").to_string()))
        .collect()
}

/// Loads `rust/fixtures/golden/feat-community-a/<name>.json`.
pub fn golden(name: &str) -> Value {
    bot_core::testing::load_golden(&format!("feat-community-a/{name}.json"))
}

/// Drops `timestamp` (and serenity's `type: "rich"`, which discord.js leaves out) from an embed JSON.
pub fn no_ts(v: Value) -> Value {
    let mut v = embed_json(v);
    if let Some(o) = v.as_object_mut() {
        o.remove("timestamp");
    }
    v
}

/// An embed JSON without serenity's `type: "rich"`.
pub fn embed_json(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut()
        && o.get("type") == Some(&json!("rich"))
    {
        o.remove("type");
    }
    v
}

/// discord.js leaves `inline` out when it was not set and writes `animated: false` on unicode emoji; serenity writes
/// `inline: false` and no `animated`. Both mean the same to Discord, so both sides are normalised.
pub fn norm(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, x) in m {
                if (k == "inline" && x == &json!(false)) || (k == "animated" && x == &json!(false)) || (k == "disabled" && x == &json!(false)) || (k == "type" && x == &json!("rich")) {
                    continue;
                }
                if k == "timestamp" || x.is_null() {
                    continue;
                }
                out.insert(k.clone(), norm(x));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        other => other.clone(),
    }
}

