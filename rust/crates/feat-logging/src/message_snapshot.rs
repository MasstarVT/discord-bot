//! Port of `src/services/messageSnapshot.js`: Redis copies of human messages for the audit log.
//!
//! The key (`msgsnap:<channelId>:<messageId>`) and the JSON shape are shared with the JS bot, so either side reads
//! the other's snapshots. Redis is cache only: every function fails open (a miss, a write that did not happen) and
//! never errors.

use std::collections::HashMap;

use bot_core::prelude::*;
use bot_core::settings::Settings;
use bot_util::text::clamp_text;
use serde::{Deserialize, Serialize};
use serenity::all::{GuildId, Message};

use crate::log_events::{LogAttachment, LogMessage, LogUser};
use crate::logging_service::{ChannelChain, LogType, is_ignored_channel, is_log_channel_dead, is_log_event_enabled};

const DEFAULT_TTL_DAYS: f64 = 7.0;
const MAX_TTL_DAYS: f64 = 30.0;
const URL_MAX: usize = 1_000;

/// `snapshotKey(channelId, messageId)`.
pub fn snapshot_key(channel_id: &str, message_id: &str) -> String {
    format!("msgsnap:{channel_id}:{message_id}")
}

/// `snapshotTtlSeconds()`: `MESSAGE_SNAPSHOT_TTL_DAYS` (default 7, clamped to 1-30 days) in seconds.
pub fn snapshot_ttl_seconds() -> i64 {
    ttl_seconds_of(std::env::var("MESSAGE_SNAPSHOT_TTL_DAYS").ok().as_deref())
}

fn ttl_seconds_of(raw: Option<&str>) -> i64 {
    let n = raw.and_then(|r| r.trim().parse::<f64>().ok()).filter(|n| n.is_finite() && *n > 0.0);
    let days = n.map_or(DEFAULT_TTL_DAYS, |n| n.clamp(1.0, MAX_TTL_DAYS));
    (days * 86_400.0).round() as i64
}

/// The author part of a stored snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SnapshotAuthor {
    pub id: String,
    pub username: Option<String>,
    pub tag: Option<String>,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
    pub bot: bool,
}

/// One stored attachment (`size` kept as the JSON number it was written as).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotAttachment {
    pub name: Option<String>,
    pub size: serde_json::Number,
    pub url: Option<String>,
}

impl Default for SnapshotAttachment {
    fn default() -> Self {
        Self { name: None, size: 0.into(), url: None }
    }
}

/// The JSON stored for a message (`buildSnapshot`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Snapshot {
    pub id: String,
    pub channel_id: Option<String>,
    pub content: Option<String>,
    pub author: SnapshotAuthor,
    pub created_timestamp: Option<i64>,
    pub attachments: Vec<SnapshotAttachment>,
    pub embeds: u64,
}

/// `shouldSnapshot(settings, message)`: logging on, messageDelete or messageEdit logged to a message log channel that
/// is set and not known dead, channel not ignored, a human author (no bot, no webhook) and something to remember.
/// `guild_id` comes from the hook: a message fetched over REST carries none of its own.
pub fn should_snapshot(app: &App, settings: Option<&Settings>, guild_id: GuildId, message: &Message) -> bool {
    let Some(settings) = settings else { return false };
    if message.author.bot || message.webhook_id.is_some() {
        return false;
    }
    if !bot_core::settings::is_module_enabled(Some(settings), Some(ModuleKey::Logging)) {
        return false;
    }
    if !is_log_event_enabled(Some(settings), "messageDelete") && !is_log_event_enabled(Some(settings), "messageEdit") {
        return false;
    }
    match LogType::Message.channel_of(settings) {
        Some(dest) if !is_log_channel_dead(app, dest) => {}
        _ => return false,
    }
    if is_ignored_channel(Some(settings), Some(&ChannelChain::of(app, guild_id, message.channel_id))) {
        return false;
    }
    !message.content.is_empty() || !message.attachments.is_empty() || !message.embeds.is_empty() || !message.message_snapshots.is_empty()
}

/// `buildSnapshot(message)` from the message view; `None` without an author.
pub fn build_snapshot(m: &LogMessage) -> Option<Snapshot> {
    let a = m.author.as_ref()?;
    let attachments = m
        .attachments
        .iter()
        .take(bot_config::limits::MESSAGE_SNAPSHOT_ATTACHMENTS)
        .map(|x| SnapshotAttachment {
            name: Some(clamp_text(x.name.as_deref().unwrap_or("file"), 100)),
            size: x.size.unwrap_or(0).into(),
            url: Some(x.url.clone().filter(|u| u.encode_utf16().count() <= URL_MAX).unwrap_or_default()),
        })
        .collect();
    Some(Snapshot {
        id: m.id.clone(),
        channel_id: m.channel_id.clone(),
        content: Some(clamp_text(&crate::log_events::effective_text(m), bot_config::limits::MESSAGE_SNAPSHOT_CHARS)),
        author: SnapshotAuthor {
            id: a.id.clone(),
            username: a.username.clone(),
            tag: a.tag.clone().or_else(|| a.username.clone()),
            global_name: a.global_name.clone(),
            avatar: a.avatar_url.clone(),
            bot: a.bot,
        },
        created_timestamp: m.created_timestamp,
        attachments,
        embeds: m.embed_count as u64,
    })
}

/// `snapshotToMessage(snap)`: a never-partial message view (mentions are not kept).
pub fn snapshot_to_message(s: &Snapshot) -> LogMessage {
    let a = &s.author;
    LogMessage {
        id: s.id.clone(),
        partial: false,
        content: Some(s.content.clone().unwrap_or_default()),
        channel_id: s.channel_id.clone(),
        author: Some(LogUser {
            id: a.id.clone(),
            username: a.username.clone(),
            tag: a.tag.clone(),
            global_name: a.global_name.clone(),
            bot: a.bot,
            created_timestamp: None,
            avatar_url: a.avatar.clone(),
            avatar_url256: a.avatar.clone(),
        }),
        created_timestamp: s.created_timestamp,
        attachments: s
            .attachments
            .iter()
            .map(|x| LogAttachment { name: x.name.clone(), size: x.size.as_u64().or_else(|| x.size.as_f64().map(|f| f.max(0.0) as u64)), url: x.url.clone() })
            .collect(),
        embed_count: s.embeds as usize,
        ..Default::default()
    }
}

fn valid(s: &Snapshot) -> bool {
    !s.author.id.is_empty() && !s.id.is_empty()
}

fn parse(v: Option<serde_json::Value>) -> Option<Snapshot> {
    v.and_then(|v| serde_json::from_value::<Snapshot>(v).ok()).filter(valid)
}

/// `storeSnapshot(message)`: writes (or overwrites) the snapshot; `true` when written.
pub async fn store_snapshot(app: &App, m: &LogMessage) -> bool {
    let Some(snap) = build_snapshot(m) else { return false };
    let Some(channel_id) = snap.channel_id.clone().filter(|c| !c.is_empty()) else { return false };
    if snap.id.is_empty() {
        return false;
    }
    app.cache().set(&snapshot_key(&channel_id, &snap.id), &snap, snapshot_ttl_seconds()).await
}

/// `loadSnapshot(channelId, messageId)`: the snapshot, or `None` (miss, Redis down, bad data).
pub async fn load_snapshot(app: &App, channel_id: &str, message_id: &str) -> Option<Snapshot> {
    if channel_id.is_empty() || message_id.is_empty() {
        return None;
    }
    parse(app.cache().get(&snapshot_key(channel_id, message_id)).await)
}

/// `loadSnapshots(channelId, ids)`: one batched read; message id -> snapshot for the hits.
pub async fn load_snapshots(app: &App, channel_id: &str, ids: &[String]) -> HashMap<String, Snapshot> {
    let mut out = HashMap::new();
    if channel_id.is_empty() || ids.is_empty() {
        return out;
    }
    let keys: Vec<String> = ids.iter().map(|id| snapshot_key(channel_id, id)).collect();
    for (id, v) in ids.iter().zip(app.cache().mget(&keys).await) {
        if let Some(s) = parse(v) {
            out.insert(id.clone(), s);
        }
    }
    out
}

/// `forgetSnapshots(channelId, ids)`: removes snapshots after a delete.
pub async fn forget_snapshots(app: &App, channel_id: &str, ids: &[String]) {
    if channel_id.is_empty() || ids.is_empty() {
        return;
    }
    let keys: Vec<String> = ids.iter().filter(|i| !i.is_empty()).map(|id| snapshot_key(channel_id, id)).collect();
    app.cache().del(&keys).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_defaults_and_clamps() {
        assert_eq!(ttl_seconds_of(None), 7 * 86_400);
        assert_eq!(ttl_seconds_of(Some("")), 7 * 86_400);
        assert_eq!(ttl_seconds_of(Some("abc")), 7 * 86_400);
        assert_eq!(ttl_seconds_of(Some("0")), 7 * 86_400);
        assert_eq!(ttl_seconds_of(Some("0.5")), 86_400);
        assert_eq!(ttl_seconds_of(Some("90")), 30 * 86_400);
        assert_eq!(ttl_seconds_of(Some("2")), 2 * 86_400);
    }
}
