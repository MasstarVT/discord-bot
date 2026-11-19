//! A fake Discord for the ticket tests: the `TicketIo` seam recorded in memory, with created, moved, renamed and
//! deleted channels written into the app's serenity cache (as discord.js does after a REST call). It plays the part of
//! `makeWorld` / `makeChannel` in `test/tickets.test.js`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_core::error::BoxFuture;
use bot_core::interaction_helpers::Payload;
use feat_tickets::ticket_service::{IoError, NewChannel, OverwriteEdit, SentMessage, TicketIo, cache_channel};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildChannel, GuildId, Message, MessageId, User, UserId};

/// One recorded call.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub op: &'static str,
    pub channel: String,
    pub data: Value,
}

#[derive(Default)]
pub struct World {
    pub calls: Vec<Call>,
    /// Messages sent per channel (payload JSON), in order.
    pub sent: HashMap<String, Vec<Value>>,
    /// Message ids that exist (edits of others answer 10008).
    pub messages: HashSet<String>,
    /// The history `fetch_messages` reads, oldest first, per channel.
    pub history: HashMap<String, Vec<Message>>,
    pub create_errors: VecDeque<IoError>,
    pub full_parents: HashSet<String>,
    pub fetch_error: Option<IoError>,
    pub send_errors: HashMap<String, IoError>,
    pub delete_error: Option<IoError>,
    pub overwrite_error: Option<IoError>,
    pub dm_error: Option<IoError>,
    pub rename_error: Option<IoError>,
    pub dms: Vec<(String, Value)>,
    pub created: Vec<NewChannel>,
    /// Channels only a REST fetch finds (not in the cache), by id.
    pub rest_only: HashMap<String, GuildChannel>,
    /// A rename waits this long before it lands (a rate-limited PATCH).
    pub rename_delay_ms: Option<u64>,
}

pub struct FakeIo {
    pub app: App,
    pub world: Mutex<World>,
    next: AtomicU64,
    /// Renames that landed after a delay (`rename_delay_ms`): channel id and name.
    pub late_renames: Arc<Mutex<Vec<(String, String)>>>,
}

impl FakeIo {
    /// Installs a fake on `app`; ids start at `base` (give every test its own range).
    pub fn new(app: &App, base: u64) -> Arc<FakeIo> {
        let io = Arc::new(FakeIo { app: app.clone(), world: Mutex::new(World::default()), next: AtomicU64::new(base), late_renames: Arc::default() });
        feat_tickets::ticket_service::set_io(app, Some(io.clone()));
        io
    }

    pub fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    pub fn w(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap()
    }

    pub fn calls(&self, op: &str) -> Vec<Call> {
        self.w().calls.iter().filter(|c| c.op == op).cloned().collect()
    }

    pub fn sent(&self, channel: impl ToString) -> Vec<Value> {
        self.w().sent.get(&channel.to_string()).cloned().unwrap_or_default()
    }

    fn record(&self, op: &'static str, channel: impl ToString, data: Value) {
        self.w().calls.push(Call { op, channel: channel.to_string(), data });
    }

    /// Adds a channel to the cache (a category: kind 4).
    pub fn add_channel(&self, guild_id: &str, kind: u8, name: &str, parent: Option<&str>) -> GuildChannel {
        let id = self.id();
        let ch = channel_json(id, guild_id, kind, name, parent, json!([]), None);
        cache_channel(&self.app, &ch, false);
        ch
    }

    /// Puts messages into a channel's history (oldest first).
    pub fn add_history(&self, channel_id: &str, count: usize, author: &str) {
        let mut w = self.w();
        let list = w.history.entry(channel_id.to_string()).or_default();
        for _ in 0..count {
            let m: Message = serde_json::from_value(json!({
                "id": (700_000_000_000_000_000_u64 + list.len() as u64).to_string(), "channel_id": channel_id,
                "author": { "id": author, "username": format!("user{author}"), "discriminator": "0", "avatar": null },
                "content": format!("message {}", list.len()), "timestamp": "2026-01-02T03:04:05.000000+00:00", "edited_timestamp": null, "tts": false,
                "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0,
            }))
            .unwrap();
            list.push(m);
        }
    }
}

pub fn channel_json(id: u64, guild_id: &str, kind: u8, name: &str, parent: Option<&str>, overwrites: Value, topic: Option<&str>) -> GuildChannel {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": guild_id, "type": kind, "name": name, "parent_id": parent, "position": 0,
        "permission_overwrites": overwrites, "topic": topic, "nsfw": false,
    }))
    .unwrap_or_else(|e| panic!("channel JSON: {e}"))
}

fn payload_json(p: &Payload) -> Value {
    json!({
        "content": p.content,
        "embeds": p.embeds.clone().unwrap_or_default().iter().map(|e| serde_json::to_value(e).unwrap()).collect::<Vec<_>>(),
        "components": p.components.clone().unwrap_or_default().iter().map(|r| serde_json::to_value(r).unwrap()).collect::<Vec<_>>(),
        "files": p.files.iter().map(|f| json!({ "name": f.filename, "description": f.description, "html": String::from_utf8_lossy(&f.data) })).collect::<Vec<_>>(),
        "allowedMentions": p.allowed_mentions.as_ref().map(|m| serde_json::to_value(m).unwrap()),
    })
}

fn cached(app: &App, channel_id: ChannelId) -> Option<GuildChannel> {
    let d = app.discord()?;
    d.cache.guilds().into_iter().find_map(|g| d.cache.guild(g).and_then(|guild| guild.channels.get(&channel_id).cloned()))
}

fn ok<T: Send + 'static>(v: T) -> BoxFuture<Result<T, IoError>> {
    Box::pin(async move { Ok(v) })
}

fn err<T: Send + 'static>(e: IoError) -> BoxFuture<Result<T, IoError>> {
    Box::pin(async move { Err(e) })
}

impl TicketIo for FakeIo {
    fn fetch_channel(&self, _guild_id: GuildId, channel_id: ChannelId) -> BoxFuture<Result<Option<GuildChannel>, IoError>> {
        self.record("fetch", channel_id, Value::Null);
        if let Some(c) = cached(&self.app, channel_id) {
            return ok(Some(c));
        }
        if let Some(c) = self.w().rest_only.get(&channel_id.to_string()).cloned() {
            return ok(Some(c));
        }
        let e = self.w().fetch_error.clone().unwrap_or_else(|| IoError::code(10003, "Unknown Channel"));
        err(e)
    }

    fn create_channel(&self, guild_id: GuildId, spec: NewChannel) -> BoxFuture<Result<GuildChannel, IoError>> {
        self.w().created.push(spec.clone());
        self.record("create", spec.parent.map(|p| p.to_string()).unwrap_or_default(), json!({ "name": spec.name, "kind": spec.kind }));
        if let Some(e) = self.w().create_errors.pop_front() {
            return err(e);
        }
        if let Some(p) = spec.parent
            && self.w().full_parents.contains(&p.to_string())
        {
            return err(IoError { code: Some(50035), error_codes: vec!["CHANNEL_PARENT_MAX_CHANNELS".into()], message: "full".into(), ..Default::default() });
        }
        let overwrites: Vec<Value> = spec
            .overwrites
            .iter()
            .filter_map(feat_tickets::ticket_service::overwrite_of)
            .map(|o| serde_json::to_value(o).unwrap())
            .collect();
        let ch = channel_json(self.id(), &guild_id.to_string(), spec.kind, &spec.name, spec.parent.map(|p| p.to_string()).as_deref(), json!(overwrites), spec.topic.as_deref());
        cache_channel(&self.app, &ch, false);
        ok(ch)
    }

    fn send_message(&self, channel_id: ChannelId, payload: Payload) -> BoxFuture<Result<SentMessage, IoError>> {
        if let Some(e) = self.w().send_errors.get(&channel_id.to_string()).cloned() {
            return err(e);
        }
        let id = MessageId::new(self.id());
        let mut w = self.w();
        w.sent.entry(channel_id.to_string()).or_default().push(payload_json(&payload));
        w.messages.insert(id.to_string());
        w.calls.push(Call { op: "send", channel: channel_id.to_string(), data: json!({ "id": id.to_string() }) });
        ok(SentMessage { id, channel_id, url: format!("https://discord.com/channels/@me/{channel_id}/{id}") })
    }

    fn edit_message(&self, channel_id: ChannelId, message_id: MessageId, payload: Payload) -> BoxFuture<Result<SentMessage, IoError>> {
        let exists = self.w().messages.contains(&message_id.to_string());
        self.record("edit", channel_id, json!({ "id": message_id.to_string(), "payload": payload_json(&payload) }));
        if !exists {
            return err(IoError::code(10008, "Unknown Message"));
        }
        ok(SentMessage { id: message_id, channel_id, url: format!("https://discord.com/channels/@me/{channel_id}/{message_id}") })
    }

    fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> BoxFuture<Result<(), IoError>> {
        self.record("deleteMessage", channel_id, json!(message_id.to_string()));
        self.w().messages.remove(&message_id.to_string());
        ok(())
    }

    fn set_parent(&self, channel_id: ChannelId, parent_id: ChannelId, reason: String) -> BoxFuture<Result<(), IoError>> {
        self.record("setParent", channel_id, json!({ "parent": parent_id.to_string(), "reason": reason }));
        if let Some(mut c) = cached(&self.app, channel_id) {
            c.parent_id = Some(parent_id);
            cache_channel(&self.app, &c, false);
        }
        ok(())
    }

    fn set_name(&self, channel_id: ChannelId, name: String, reason: String) -> BoxFuture<Result<(), IoError>> {
        if let Some(e) = self.w().rename_error.clone() {
            return err(e);
        }
        if let Some(ms) = self.w().rename_delay_ms {
            let late = self.late_renames.clone();
            return Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                late.lock().unwrap().push((channel_id.to_string(), name));
                Ok(())
            });
        }
        self.record("setName", channel_id, json!({ "name": name, "reason": reason }));
        if let Some(mut c) = cached(&self.app, channel_id) {
            c.name = name;
            cache_channel(&self.app, &c, false);
        }
        ok(())
    }

    fn edit_overwrite(&self, _guild_id: GuildId, channel_id: ChannelId, edit: OverwriteEdit) -> BoxFuture<Result<(), IoError>> {
        if let Some(e) = self.w().overwrite_error.clone() {
            return err(e);
        }
        let edits: serde_json::Map<String, Value> = edit.edits.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        self.record("overwrite.edit", channel_id, json!({ "id": edit.id, "type": edit.kind, "edits": edits, "reason": edit.reason }));
        ok(())
    }

    fn delete_overwrite(&self, channel_id: ChannelId, target_id: String, reason: String) -> BoxFuture<Result<(), IoError>> {
        if let Some(e) = self.w().overwrite_error.clone() {
            return err(e);
        }
        self.record("overwrite.delete", channel_id, json!({ "id": target_id, "reason": reason }));
        ok(())
    }

    fn delete_channel(&self, channel_id: ChannelId, reason: String) -> BoxFuture<Result<(), IoError>> {
        if let Some(e) = self.w().delete_error.clone() {
            return err(e);
        }
        self.record("delete", channel_id, json!(reason));
        if let Some(c) = cached(&self.app, channel_id) {
            cache_channel(&self.app, &c, true);
        }
        ok(())
    }

    fn fetch_messages(&self, channel_id: ChannelId, before: Option<MessageId>, limit: u8) -> BoxFuture<Result<Vec<Message>, IoError>> {
        let list = self.w().history.get(&channel_id.to_string()).cloned().unwrap_or_default();
        let mut newest_first: Vec<Message> = list.into_iter().rev().collect();
        if let Some(b) = before
            && let Some(pos) = newest_first.iter().position(|m| m.id == b)
        {
            newest_first = newest_first.split_off(pos + 1);
        }
        newest_first.truncate(usize::from(limit));
        ok(newest_first)
    }

    fn fetch_user(&self, user_id: UserId) -> BoxFuture<Result<User, IoError>> {
        let u: User = serde_json::from_value(json!({ "id": user_id.to_string(), "username": format!("u{user_id}"), "discriminator": "0", "avatar": null })).unwrap();
        ok(u)
    }

    fn send_dm(&self, user_id: UserId, payload: Payload) -> BoxFuture<Result<(), IoError>> {
        if let Some(e) = self.w().dm_error.clone() {
            return err(e);
        }
        self.w().dms.push((user_id.to_string(), payload_json(&payload)));
        ok(())
    }
}
