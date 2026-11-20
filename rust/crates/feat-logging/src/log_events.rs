//! Port of `src/services/logEvents.js`: the pure embed builders and diff functions of every audit-log event. No I/O,
//! no settings, no Discord REST, no Redis.
//!
//! The JS builders read discord.js objects (or test fakes); here they read small plain views ([`LogUser`],
//! [`LogMessage`], [`LogMember`], ...) that [`crate::adapters`] fills from serenity models. The views deserialize from
//! the JS fake shape (camelCase), so the golden fixtures feed the same case to both sides.
//!
//! Every builder returns the embed as discord.js's `embed.toJSON()` (a `serde_json::Value` APIEmbed: `color`,
//! `timestamp`, `title`, ...), passed through [`crate::logging_service::fit_embed_data`] last, or `None` when there is
//! nothing to log.

use std::collections::{HashMap, HashSet};

use bot_util::text::{clamp_text, format_number, join_with_limit};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::logging_service::{channel_type_label, extract_mentions, fit_embed_data};

/// `LOG_COLORS`.
pub mod log_colors {
    pub const CREATE: u32 = bot_config::colors::SUCCESS;
    pub const DELETE: u32 = bot_config::colors::ERROR;
    pub const EDIT: u32 = bot_config::colors::WARNING;
    pub const UPDATE: u32 = bot_config::colors::INFO;
    pub const ALERT: u32 = 0xff9800;
    pub const NEUTRAL: u32 = bot_config::colors::NEUTRAL;
}

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
const NEW_ACCOUNT_MS: i64 = 7 * DAY_MS;
/// The cap of a bulk-delete transcript, in UTF-16 units.
pub const TRANSCRIPT_MAX_CHARS: usize = 1_000_000;
const NO_TEXT: &str = "*[No text content]*";
const ZWSP: &str = "\u{200b}";

// ── Views (what the JS builders read) ─────────────────────────────────────────

/// A user (`user.id`, `tag`, `username`, `globalName`, `bot`, `createdTimestamp`, `displayAvatarURL()`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogUser {
    pub id: String,
    pub username: Option<String>,
    pub tag: Option<String>,
    pub global_name: Option<String>,
    pub bot: bool,
    pub created_timestamp: Option<i64>,
    /// `displayAvatarURL()`.
    pub avatar_url: Option<String>,
    /// `displayAvatarURL({ size: 256 })`.
    pub avatar_url256: Option<String>,
}

/// A message attachment (`name`, `size`, `url`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogAttachment {
    pub name: Option<String>,
    pub size: Option<u64>,
    pub url: Option<String>,
}

/// A role mention (`message.mentions.roles`: id and name).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogRoleRef {
    pub id: String,
    pub name: String,
}

/// A message, or a partial one (`partial: true`: only the ids are known).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogMessage {
    pub id: String,
    pub partial: bool,
    pub content: Option<String>,
    /// The contents of the forwarded message snapshots (`effectiveText` reads them).
    pub snapshot_contents: Vec<String>,
    pub author: Option<LogUser>,
    pub channel_id: Option<String>,
    pub webhook_id: Option<String>,
    pub created_timestamp: Option<i64>,
    pub edited_timestamp: Option<i64>,
    pub attachments: Vec<LogAttachment>,
    /// `message.embeds.length`.
    pub embed_count: usize,
    pub url: Option<String>,
    pub mention_users: Vec<LogUser>,
    pub mention_roles: Vec<LogRoleRef>,
    pub mention_everyone: bool,
}

impl LogMessage {
    /// A partial message: only its id (and channel) are known.
    pub fn partial(id: &str, channel_id: Option<&str>) -> Self {
        Self { id: id.into(), partial: true, channel_id: channel_id.map(String::from), ..Default::default() }
    }
}

/// `effectiveText(message)`: the content plus every forwarded snapshot's content, joined by '\n'.
pub fn effective_text(message: &LogMessage) -> String {
    bot_util::text::effective_text(message.content.as_deref(), message.snapshot_contents.iter().map(|s| Some(s.as_str())))
}

/// A member role (`member.roles.cache`: id and position).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogMemberRole {
    pub id: String,
    pub position: i64,
}

/// A guild member.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogMember {
    pub id: String,
    pub user: Option<LogUser>,
    pub guild_id: Option<String>,
    pub nickname: Option<String>,
    /// The server avatar hash.
    pub avatar: Option<String>,
    /// `member.avatarURL({ size: 256 })`.
    pub avatar_url256: Option<String>,
    pub partial: bool,
    pub joined_timestamp: Option<i64>,
    pub roles: Vec<LogMemberRole>,
    /// `communicationDisabledUntilTimestamp`.
    pub timeout_until: Option<i64>,
}

impl LogMember {
    fn user_id(&self) -> String {
        self.user.as_ref().map(|u| u.id.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| self.id.clone())
    }

    /// `isCommunicationDisabled()`.
    pub fn is_timed_out(&self, now_ms: i64) -> bool {
        self.timeout_until.is_some_and(|u| u > now_ms)
    }
}

/// The voice state fields `classifyVoice` reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogVoiceState {
    pub channel_id: Option<String>,
    pub server_mute: bool,
    pub server_deaf: bool,
    pub self_deaf: bool,
    pub self_mute: bool,
    pub streaming: bool,
}

/// A permission overwrite (`type` 0 role, 1 member; bits as numbers).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogOverwrite {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(deserialize_with = "de_bits")]
    pub allow: u64,
    #[serde(deserialize_with = "de_bits")]
    pub deny: u64,
}

fn de_bits<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    })
}

/// A guild channel (or a thread, for the channel events).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogChannel {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: u8,
    pub topic: Option<String>,
    pub nsfw: bool,
    pub rate_limit_per_user: Option<u32>,
    pub parent_id: Option<String>,
    /// `channel.parent?.name`.
    pub parent_name: Option<String>,
    /// `undefined` (None) on channels without a bitrate.
    pub bitrate: Option<u32>,
    pub user_limit: Option<u32>,
    /// `undefined` (None) on channels without a region; `Some(None)` is the automatic region.
    #[serde(deserialize_with = "de_present")]
    pub rtc_region: Option<Option<String>>,
    pub overwrites: Vec<LogOverwrite>,
}

fn de_present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(Some(v.as_str().map(String::from)))
}

/// A forum tag of a thread's parent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogTag {
    pub id: String,
    pub name: String,
}

/// A thread. Unknown (partial) fields are `None` and never compared.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogThread {
    pub id: String,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub kind: u8,
    pub parent_id: Option<String>,
    pub owner_id: Option<String>,
    pub auto_archive_duration: Option<u32>,
    pub rate_limit_per_user: Option<u32>,
    pub locked: Option<bool>,
    pub invitable: Option<bool>,
    pub applied_tags: Option<Vec<String>>,
    /// The parent forum's tags (`thread.parent.availableTags`).
    pub available_tags: Vec<LogTag>,
}

/// A role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogRole {
    pub id: String,
    pub name: String,
    pub hex_color: Option<String>,
    pub hoist: bool,
    pub mentionable: bool,
    pub icon: Option<String>,
    pub unicode_emoji: Option<String>,
    pub managed: bool,
    #[serde(deserialize_with = "de_bits")]
    pub permissions: u64,
    pub created_timestamp: Option<i64>,
}

/// The guild properties `diffGuild` compares (`None` is `null`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogGuild {
    pub id: String,
    pub name: Option<String>,
    pub icon: Option<String>,
    pub banner: Option<String>,
    pub splash: Option<String>,
    pub description: Option<String>,
    pub owner_id: Option<String>,
    pub verification_level: Option<u8>,
    pub explicit_content_filter: Option<u8>,
    pub default_message_notifications: Option<u8>,
    pub afk_channel_id: Option<String>,
    pub afk_timeout: Option<u64>,
    pub system_channel_id: Option<String>,
    pub rules_channel_id: Option<String>,
    pub public_updates_channel_id: Option<String>,
    #[serde(rename = "vanityURLCode")]
    pub vanity_url_code: Option<String>,
    pub preferred_locale: Option<String>,
    pub premium_tier: Option<u8>,
    pub mfa_level: Option<u8>,
    pub nsfw_level: Option<u8>,
    pub premium_progress_bar_enabled: Option<bool>,
    /// `newGuild.iconURL()`.
    pub icon_url: Option<String>,
}

/// A guild emoji.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogEmoji {
    pub id: String,
    pub name: String,
    pub animated: bool,
    pub roles: Vec<String>,
    /// `emoji.imageURL()`.
    pub image_url: Option<String>,
    /// `emoji.toString()` (`<:name:id>`).
    pub mention: String,
}

/// An invite (create / delete events).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogInvite {
    pub code: String,
    pub channel_id: Option<String>,
    pub inviter: Option<LogUser>,
    pub max_uses: Option<u64>,
    pub expires_timestamp: Option<i64>,
    pub temporary: bool,
}

/// One `{ name, value }` diff line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub name: String,
    pub value: String,
}

impl Line {
    fn new(name: &str, value: String) -> Self {
        Self { name: name.into(), value: clamp_text(&value, 1024) }
    }
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// The embed under construction (`EmbedBuilder#data`).
#[derive(Debug, Clone, Default)]
struct Builder(Map<String, Value>);

fn iso(ms: i64) -> String {
    bot_util::js::to_iso_string(ms as f64).unwrap_or_default()
}

/// `logEmbed(color, title)`: an embed from the neutral factory with an event colour (`{ color, timestamp, title }`).
pub fn log_embed(now_ms: i64, color: u32, title: &str) -> Value {
    Value::Object(log_embed_builder(now_ms, color, title).0)
}

/// `fitEmbed(embed)`: clamps an embed to Discord's limits (see [`fit_embed_data`]).
pub fn fit_embed(embed: &Value) -> Value {
    fit_embed_data(embed)
}

fn log_embed_builder(now_ms: i64, color: u32, title: &str) -> Builder {
    let mut m = Map::new();
    m.insert("color".into(), json!(color));
    m.insert("timestamp".into(), json!(iso(now_ms)));
    if !title.is_empty() {
        m.insert("title".into(), json!(clamp_text(title, 256)));
    }
    Builder(m)
}

impl Builder {
    fn description(mut self, text: &str, max: usize) -> Self {
        if !text.is_empty() {
            self.0.insert("description".into(), json!(clamp_text(text, max)));
        }
        self
    }
    fn desc(self, text: &str) -> Self {
        self.description(text, 4096)
    }
    fn footer(mut self, text: &str) -> Self {
        self.0.insert("footer".into(), json!({ "text": clamp_text(text, 2048) }));
        self
    }
    fn thumb(mut self, url: Option<&str>) -> Self {
        if let Some(u) = url.filter(|u| !u.is_empty()) {
            self.0.insert("thumbnail".into(), json!({ "url": u }));
        }
        self
    }
    fn author(mut self, user: Option<&LogUser>) -> Self {
        let Some(u) = user.filter(|u| !u.id.is_empty()) else { return self };
        let mut a = Map::new();
        a.insert("name".into(), json!(user_tag(Some(u))));
        if let Some(icon) = u.avatar_url.as_ref().filter(|s| !s.is_empty()) {
            a.insert("icon_url".into(), json!(icon));
        }
        self.0.insert("author".into(), Value::Object(a));
        self
    }
    fn timestamp(mut self, ms: i64) -> Self {
        self.0.insert("timestamp".into(), json!(iso(ms)));
        self
    }
    fn fields(mut self, fields: Vec<Value>) -> Self {
        if !fields.is_empty() {
            self.0.insert("fields".into(), Value::Array(fields));
        }
        self
    }
    /// `fitEmbed(embed)`.
    fn fit(self) -> Value {
        fit_embed_data(&Value::Object(self.0))
    }
}

/// `field(name, value, inline)`.
fn field(name: &str, value: &str, inline: bool) -> Value {
    let n = clamp_text(name, 256);
    let v = clamp_text(value, 1024);
    json!({ "name": if n.is_empty() { ZWSP.to_string() } else { n }, "value": if v.is_empty() { ZWSP.to_string() } else { v }, "inline": inline })
}

/// `userTag(user)`.
pub fn user_tag(user: Option<&LogUser>) -> String {
    let raw = match user {
        Some(u) => u.tag.clone().or_else(|| u.username.clone()).unwrap_or_else(|| if u.id.is_empty() { "Unknown".into() } else { u.id.clone() }),
        None => "Unknown".into(),
    };
    clamp_text(&raw, 100)
}

/// `userLine(user)`: a mention plus the tag in code, or `Unknown`.
pub fn user_line(user: Option<&LogUser>) -> String {
    match user.filter(|u| !u.id.is_empty()) {
        Some(u) => format!("<@{}> `{}`", u.id, user_tag(Some(u)).replace('`', "'")),
        None => "Unknown".into(),
    }
}

/// `channelRef(guild, id)`: `<#id>`, plus " (deleted channel)" when the guild's channel cache is known and lacks it.
pub fn channel_ref(known: Option<&HashSet<String>>, id: Option<&str>) -> String {
    let Some(id) = id.filter(|i| !i.is_empty()) else { return "*(none)*".into() };
    let deleted = known.is_some_and(|k| !k.contains(id));
    format!("<#{id}>{}", if deleted { " (deleted channel)" } else { "" })
}

fn role_ref(id: &str) -> String {
    format!("<@&{id}>")
}

fn yes_no(v: bool) -> &'static str {
    if v { "Yes" } else { "No" }
}

/// Inline code for a changed value; backticks inside are replaced, empty becomes "(none)".
fn code(v: Option<&str>) -> String {
    match v {
        None | Some("") => "*(none)*".into(),
        Some(s) => format!("`{}`", clamp_text(&s.replace('`', "'"), 200)),
    }
}

fn change(before: Option<&str>, after: Option<&str>) -> String {
    format!("{} → {}", code(before), code(after))
}

fn same_set(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.contains(x))
}

fn join(items: &[String], sep: &str, max: usize) -> String {
    join_with_limit(items, sep, max, bot_util::text::default_more)
}

/// `Number#toFixed(1)` for the exact binary values `formatBytes` divides (powers of two): ties round up.
fn to_fixed1(x: f64) -> String {
    let n = (x * 10.0 + 0.5).floor() as i64;
    format!("{}.{}", n / 10, n % 10)
}

/// `formatBytes(n)`.
pub fn format_bytes(n: Option<u64>) -> String {
    let Some(v) = n else { return "?".into() };
    let v = v as f64;
    if v < 1024.0 {
        format!("{v} B")
    } else if v < 1024.0 * 1024.0 {
        format!("{} KB", to_fixed1(v / 1024.0))
    } else {
        format!("{} MB", to_fixed1(v / (1024.0 * 1024.0)))
    }
}

/// HEAD's formatter, verbatim (`formatTimeInServer(ms)`).
pub fn format_time_in_server(ms: i64) -> String {
    let d = ms.div_euclid(86_400_000);
    let h = ms.rem_euclid(86_400_000) / 3_600_000;
    let m = ms.rem_euclid(3_600_000) / 60_000;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        "Just joined".into()
    }
}

fn ts(ms: i64, style: &str) -> String {
    bot_util::time::discord_timestamp_ms(ms as f64, style)
}

fn duration_long(ms: f64) -> String {
    bot_util::parse_duration::format_duration_long(ms)
}

fn msg_footer(message: &LogMessage) -> String {
    match message.author.as_ref().filter(|a| !a.id.is_empty()) {
        Some(a) => format!("User ID: {} · Msg ID: {}", a.id, message.id),
        None => format!("Msg ID: {}", message.id),
    }
}

// ── Messages (§6.3) ───────────────────────────────────────────────────────────

/// Credits a bot deletion: the bot's mention and why, e.g. "<@id> (tempvoice)".
fn deleted_by_bot(bot_id: Option<&str>, reason: &str) -> String {
    let who = match bot_id.filter(|b| !b.is_empty()) {
        Some(b) => format!("<@{b}>"),
        None => "the bot".into(),
    };
    format!("{who} ({})", clamp_text(reason, 100))
}

/// `isGhostPing({ message, botReason, now })`: mentioned someone other than its author (not a bot), removed within
/// an hour, and not deleted by the bot.
pub fn is_ghost_ping(message: Option<&LogMessage>, bot_reason: Option<&str>, now_ms: i64) -> (bool, Vec<String>) {
    let Some(m) = message.filter(|m| !m.partial) else { return (false, Vec::new()) };
    if bot_reason.is_some_and(|r| !r.is_empty()) {
        return (false, Vec::new());
    }
    let Some(created) = m.created_timestamp else { return (false, Vec::new()) };
    if now_ms - created > HOUR_MS {
        return (false, Vec::new());
    }
    let mentions = extract_mentions(Some(m), m.author.as_ref().map(|a| a.id.as_str()));
    (!mentions.is_empty(), mentions)
}

/// `buildMessageDeleteEmbed({ message, botReason, botId, now })`. `known` is the guild's channel cache.
pub fn build_message_delete_embed(message: &LogMessage, bot_reason: Option<&str>, bot_id: Option<&str>, now_ms: i64, known: Option<&HashSet<String>>) -> Value {
    let bot_reason = bot_reason.filter(|r| !r.is_empty());
    let (ghost, mentions) = is_ghost_ping(Some(message), bot_reason, now_ms);
    let (title, color) = if ghost { ("👻 Ghost Ping Detected — Message Deleted", log_colors::ALERT) } else { ("🗑️ Message Deleted", log_colors::DELETE) };
    let mut e = log_embed_builder(now_ms, color, title);
    let mut fields = Vec::new();
    if let Some(a) = &message.author {
        e = e.author(Some(a));
        fields.push(field("Author", &user_line(Some(a)), true));
    }
    fields.push(field("Channel", &channel_ref(known, message.channel_id.as_deref()), true));
    if let Some(r) = bot_reason {
        fields.push(field("Deleted By", &deleted_by_bot(bot_id, r), true));
    }
    let text = effective_text(message);
    e = if message.partial {
        e.desc("*Message was not cached — content unavailable.*")
    } else if !text.is_empty() {
        e.desc(&clamp_text(&text, 4000))
    } else {
        e.desc(NO_TEXT)
    };
    if !message.attachments.is_empty() {
        let list: Vec<String> = message
            .attachments
            .iter()
            .map(|a| format!("{} ({})", clamp_text(a.name.as_deref().unwrap_or("file"), 100), format_bytes(a.size)))
            .collect();
        fields.push(field(&format!("Attachments ({})", message.attachments.len()), &join(&list, "\n", 1024), false));
    }
    if message.embed_count > 0 && text.is_empty() {
        fields.push(field("Embeds", &format!("{} embed(s)", message.embed_count), false));
    }
    if ghost {
        fields.push(field("⚠️ Ghost Pinged", &join(&mentions, "\n", 1024), false));
    }
    if let Some(c) = message.created_timestamp {
        fields.push(field("Sent", &ts(c, "R"), true));
    }
    e.footer(&msg_footer(message)).fields(fields).fit()
}

/// `buildMessageEditEmbed({ oldMessage, newMessage, oldKnown, now })`.
pub fn build_message_edit_embed(old: Option<&LogMessage>, new: &LogMessage, old_known: bool, now_ms: i64, known: Option<&HashSet<String>>) -> Value {
    let author = new.author.as_ref();
    let author_id = author.map(|a| a.id.as_str());
    let mut removed = Vec::new();
    let mut ghost = false;
    if old_known {
        let before = extract_mentions(old, author_id);
        let after: HashSet<String> = extract_mentions(Some(new), author_id).into_iter().collect();
        removed = before.into_iter().filter(|m| !after.contains(m)).collect();
        // `now - createdTimestamp <= HOUR`: an unknown creation time compares false (NaN).
        ghost = !removed.is_empty() && new.created_timestamp.is_some_and(|c| now_ms - c <= HOUR_MS);
    }
    let (color, title) = if ghost { (log_colors::ALERT, "👻 Ghost Ping Detected — Message Edited") } else { (log_colors::EDIT, "✏️ Message Edited") };
    let mut e = log_embed_builder(now_ms, color, title).timestamp(new.edited_timestamp.unwrap_or(now_ms)).author(author);
    let mut fields = Vec::new();
    if let Some(a) = author {
        fields.push(field("Author", &user_line(Some(a)), true));
    }
    fields.push(field("Channel", &channel_ref(known, new.channel_id.as_deref()), true));
    let before_text = if old_known {
        match old.and_then(|o| o.content.as_deref()).filter(|c| !c.is_empty()) {
            Some(c) => clamp_text(c, 1024),
            None => NO_TEXT.into(),
        }
    } else {
        "*Not cached*".into()
    };
    fields.push(field("Before", &before_text, false));
    let after_text = match new.content.as_deref().filter(|c| !c.is_empty()) {
        Some(c) => clamp_text(c, 1024),
        None => NO_TEXT.into(),
    };
    fields.push(field("After", &after_text, false));
    if ghost {
        fields.push(field("⚠️ Removed Mentions", &join(&removed, "\n", 1024), false));
    }
    if let Some(u) = new.url.as_deref().filter(|u| !u.is_empty()) {
        fields.push(field("Jump", &format!("[View Message]({u})"), true));
    }
    e = e.footer(&msg_footer(new));
    e.fields(fields).fit()
}

/// `buildBulkDeleteTranscript(messages, channel)`: oldest first, unknown times last, capped at 1,000,000 characters.
pub fn build_bulk_delete_transcript(messages: &[LogMessage], channel_name: Option<&str>) -> String {
    let rank = |m: &LogMessage| m.created_timestamp.map(|t| t as f64).unwrap_or(f64::INFINITY);
    let mut sorted: Vec<&LogMessage> = messages.iter().collect();
    sorted.sort_by(|a, b| rank(a).partial_cmp(&rank(b)).unwrap_or(std::cmp::Ordering::Equal));
    let head = format!("Bulk delete — {} messages — channel {}", messages.len(), channel_name.unwrap_or(""));
    let u16len = bot_util::js::utf16_len;
    let mut length = u16len(&head);
    let mut out = vec![head];
    let mut written = 0usize;
    for m in &sorted {
        let line = match (&m.author, m.partial) {
            (Some(a), false) => {
                let time = m.created_timestamp.and_then(|t| bot_util::js::to_iso_string(t as f64)).unwrap_or_else(|| "unknown time".into());
                let content = effective_text(m).replace("\r\n", " ⏎ ").replace('\n', " ⏎ ");
                let urls: Vec<&str> = m.attachments.iter().filter_map(|a| a.url.as_deref()).filter(|u| !u.is_empty()).collect();
                format!(
                    "[{time}] {} ({}){}: {content}{}",
                    user_tag(Some(a)),
                    a.id,
                    if a.bot { " [BOT]" } else { "" },
                    if urls.is_empty() { String::new() } else { format!(" [attachments: {}]", urls.join(" ")) }
                )
            }
            _ => format!("[unknown time] (not cached) message {}", m.id),
        };
        let l = u16len(&line);
        if length + 1 + l > TRANSCRIPT_MAX_CHARS {
            break;
        }
        out.push(line);
        length += 1 + l;
        written += 1;
    }
    if written < sorted.len() {
        out.push(format!("… transcript truncated ({} more messages)", sorted.len() - written));
    }
    out.join("\n")
}

/// The transcript file of a bulk delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// `buildBulkDeleteEmbed({ channel, messages, botReason, botId })`: the embed and the transcript file (`None` when no
/// message was cached). `channel` is `(id, name)`; `known` is the guild's channel cache.
pub fn build_bulk_delete_embed(
    channel_id: Option<&str>,
    channel_name: Option<&str>,
    messages: &[LogMessage],
    bot_reason: Option<&str>,
    bot_id: Option<&str>,
    now_ms: i64,
    known: Option<&HashSet<String>>,
) -> (Value, Option<LogFile>) {
    let n = messages.len();
    let cached: Vec<&LogMessage> = messages.iter().filter(|m| !m.partial && m.author.is_some()).collect();
    let by = match bot_reason.filter(|r| !r.is_empty()) {
        Some(r) => format!(" by {}", deleted_by_bot(bot_id, r)),
        None => String::new(),
    };
    let e = log_embed_builder(now_ms, log_colors::DELETE, "🧹 Messages Bulk-Deleted")
        .desc(&format!("**{n}** messages were deleted in {}{by}.", channel_ref(known, channel_id)));
    let mut fields = vec![field("Cached", &format!("{} of {n} messages had cached content.", cached.len()), false)];
    if !cached.is_empty() {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for m in &cached {
            let id = m.author.as_ref().map(|a| a.id.clone()).unwrap_or_default();
            match counts.iter_mut().find(|(i, _)| *i == id) {
                Some((_, c)) => *c += 1,
                None => counts.push((id, 1)),
            }
        }
        counts.sort_by_key(|c| std::cmp::Reverse(c.1));
        let authors: Vec<String> = counts.iter().map(|(id, c)| format!("<@{id}> ×{c}")).collect();
        fields.push(field("Authors", &join(&authors, ", ", 1024), false));
    }
    let mut file = None;
    if !cached.is_empty() {
        let text = build_bulk_delete_transcript(messages, channel_name);
        file = Some(LogFile { name: format!("bulk-delete-{}-{now_ms}.txt", channel_id.unwrap_or("channel")), bytes: text.into_bytes() });
        fields.push(field("Transcript", "Attached as a text file.", false));
    }
    let e = e.footer(&format!("Channel ID: {}", channel_id.unwrap_or("unknown")));
    (e.fields(fields).fit(), file)
}

// ── Invite posted (§6.3.5) ────────────────────────────────────────────────────

fn invite_code_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?:https?://)?(?:www\.)?(?:discord(?:app)?\.com/invite|discord\.gg)/([A-Za-z0-9-]{2,32})").unwrap_or_else(|_| regex::Regex::new("$^").expect("regex"))
    })
}

/// `findInviteCodes(content)`: the unique invite codes in a message, in order of appearance, at most 10.
pub fn find_invite_codes(content: Option<&str>) -> Vec<String> {
    let Some(c) = content else { return Vec::new() };
    let head = bot_util::js::utf16_prefix(c, 4000);
    let mut codes: Vec<String> = Vec::new();
    for cap in invite_code_re().captures_iter(head) {
        if let Some(m) = cap.get(1) {
            let code = m.as_str().to_string();
            if !codes.contains(&code) {
                codes.push(code);
            }
            if codes.len() >= 10 {
                break;
            }
        }
    }
    codes
}

/// `buildInvitePostedEmbed({ message, codes, vanityCode })`.
pub fn build_invite_posted_embed(message: &LogMessage, codes: &[String], vanity_code: Option<&str>, now_ms: i64, known: Option<&HashSet<String>>) -> Value {
    let mut e = log_embed_builder(now_ms, log_colors::ALERT, "📨 Invite Posted").author(message.author.as_ref());
    let vanity = vanity_code.filter(|v| !v.is_empty()).map(|v| v.to_lowercase());
    let mut fields = Vec::new();
    if let Some(a) = &message.author {
        fields.push(field("Author", &user_line(Some(a)), true));
    }
    fields.push(field("Channel", &channel_ref(known, message.channel_id.as_deref()), true));
    let list: Vec<String> = codes
        .iter()
        .map(|c| {
            let mine = vanity.as_deref().is_some_and(|v| c.to_lowercase() == v);
            format!("discord.gg/{c}{}", if mine { " (this server's vanity URL)" } else { "" })
        })
        .collect();
    fields.push(field("Invites", &join(&list, "\n", 1024), false));
    if let Some(u) = message.url.as_deref().filter(|u| !u.is_empty()) {
        fields.push(field("Jump", &format!("[View Message]({u})"), true));
    }
    e = e.desc(&clamp_text(&effective_text(message), 4000)).footer(&msg_footer(message));
    e.fields(fields).fit()
}

// ── Joins and leaves (§6.4.1, §6.4.2) ─────────────────────────────────────────

/// The invite a join used, as the invites plugin publishes it (`{ code, inviterId, vanity }`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteInfo {
    pub code: Option<String>,
    pub inviter_id: Option<String>,
    #[serde(default)]
    pub vanity: bool,
}

/// `buildMemberJoinEmbed({ member, invite, now })`. `invite` is `None` when the invites module recorded nothing
/// (the field is omitted), `Some(None)` when it could not tell (Unknown). `member_count` is `guild.memberCount`.
pub fn build_member_join_embed(member: &LogMember, invite: Option<Option<&InviteInfo>>, member_count: Option<u64>, now_ms: i64) -> Value {
    let user = member.user.as_ref();
    let is_bot = user.is_some_and(|u| u.bot);
    let is_new = user.and_then(|u| u.created_timestamp).is_some_and(|c| now_ms - c < NEW_ACCOUNT_MS);
    let (title, color) = if is_bot {
        ("🤖 Bot Added", log_colors::UPDATE)
    } else if is_new {
        ("⚠️ New Account Joined", log_colors::ALERT)
    } else {
        ("📥 Member Joined", log_colors::CREATE)
    };
    let mut e = log_embed_builder(now_ms, color, title).thumb(user.and_then(|u| u.avatar_url.as_deref()));
    let mut fields = vec![field("User", &user_line(user), true)];
    if let Some(c) = user.and_then(|u| u.created_timestamp) {
        fields.push(field("Account Created", &ts(c, "R"), true));
    }
    let count = member_count.map(|c| c as f64).unwrap_or(f64::NAN);
    fields.push(field("Member Count", &format!("#{}", format_number(count)), true));
    match invite {
        None => {}
        Some(None) => fields.push(field("Invite", "Unknown", true)),
        Some(Some(i)) => {
            if i.vanity {
                fields.push(field("Invite", "Vanity URL", true));
            } else if let Some(code) = i.code.as_deref().filter(|c| !c.is_empty()) {
                let by = i.inviter_id.as_deref().filter(|x| !x.is_empty()).map(|x| format!(" by <@{x}>")).unwrap_or_default();
                fields.push(field("Invite", &format!("discord.gg/{code}{by}"), true));
            } else {
                fields.push(field("Invite", "Unknown", true));
            }
        }
    }
    if is_new && !is_bot {
        e = e.desc("⚠️ **This account was created less than 7 days ago.**");
    }
    let uid = user.map(|u| u.id.clone()).unwrap_or_else(|| "undefined".into());
    e.footer(&format!("User ID: {uid}")).fields(fields).fit()
}

/// `buildMemberLeaveEmbed({ member, now })`.
pub fn build_member_leave_embed(member: &LogMember, now_ms: i64) -> Value {
    let user = member.user.as_ref();
    let e = log_embed_builder(now_ms, log_colors::DELETE, "📤 Member Left").thumb(user.and_then(|u| u.avatar_url.as_deref()));
    let time = match member.joined_timestamp.filter(|t| *t != 0) {
        Some(j) => format_time_in_server(now_ms - j),
        None => "Unknown".into(),
    };
    let mut fields = vec![field("User", &user_line(user), true), field("Time in Server", &time, true)];
    if !member.partial {
        let guild_id = member.guild_id.clone().unwrap_or_default();
        let mut roles: Vec<&LogMemberRole> = member.roles.iter().filter(|r| r.id != guild_id).collect();
        roles.sort_by_key(|r| std::cmp::Reverse(r.position));
        let list: Vec<String> = roles.iter().map(|r| role_ref(&r.id)).collect();
        if !list.is_empty() {
            fields.push(field("Roles", &join(&list, ", ", 1024), false));
        }
    }
    let uid = user.map(|u| u.id.clone()).unwrap_or_else(|| "undefined".into());
    e.footer(&format!("User ID: {uid}")).fields(fields).fit()
}

// ── Member updates (§6.4.3) ───────────────────────────────────────────────────

/// A nickname change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NicknameChange {
    pub before: Option<String>,
    pub after: Option<String>,
}

/// A role change (ids, @everyone never included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolesChange {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// A server avatar change (`avatarURL({ size: 256 })` before and after).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvatarChange {
    pub before: Option<String>,
    pub after: Option<String>,
}

/// A timeout change (`until` / `from` in epoch ms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeoutChange {
    Added { until: Option<i64> },
    Removed,
    Changed { from: Option<i64>, until: Option<i64> },
    /// The previous state was not cached, but a timeout is active.
    ActiveUnknown { until: Option<i64> },
}

impl TimeoutChange {
    /// `timeout.kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Added { .. } => "added",
            Self::Removed => "removed",
            Self::Changed { .. } => "changed",
            Self::ActiveUnknown { .. } => "active-unknown",
        }
    }
    /// `timeout.until`.
    pub fn until(&self) -> Option<i64> {
        match self {
            Self::Added { until } | Self::Changed { until, .. } | Self::ActiveUnknown { until } => *until,
            Self::Removed => None,
        }
    }
}

/// The result of [`diff_member_update`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberDiff {
    pub nickname: Option<NicknameChange>,
    pub roles: Option<RolesChange>,
    pub avatar: Option<AvatarChange>,
    pub timeout: Option<TimeoutChange>,
}

/// `diffMemberUpdate({ oldMember, newMember, oldKnown })`. With `old_known == false` nothing but an active timeout
/// can be reported, and that as `active-unknown`. Timeouts compare `isCommunicationDisabled()` on both sides.
pub fn diff_member_update(old: Option<&LogMember>, new: &LogMember, old_known: bool, now_ms: i64) -> MemberDiff {
    let mut out = MemberDiff::default();
    let old = match (old_known, old) {
        (true, Some(o)) => o,
        _ => {
            if new.is_timed_out(now_ms) {
                out.timeout = Some(TimeoutChange::ActiveUnknown { until: new.timeout_until });
            }
            return out;
        }
    };
    if old.nickname != new.nickname {
        out.nickname = Some(NicknameChange { before: old.nickname.clone(), after: new.nickname.clone() });
    }
    let guild_id = new.guild_id.clone().unwrap_or_default();
    let old_roles: Vec<&str> = old.roles.iter().map(|r| r.id.as_str()).collect();
    let new_roles: Vec<&str> = new.roles.iter().map(|r| r.id.as_str()).collect();
    let added: Vec<String> = new_roles.iter().filter(|id| !old_roles.contains(id) && **id != guild_id).map(|s| s.to_string()).collect();
    let removed: Vec<String> = old_roles.iter().filter(|id| !new_roles.contains(id) && **id != guild_id).map(|s| s.to_string()).collect();
    if !added.is_empty() || !removed.is_empty() {
        out.roles = Some(RolesChange { added, removed });
    }
    if old.avatar != new.avatar {
        out.avatar = Some(AvatarChange { before: old.avatar_url256.clone(), after: new.avatar_url256.clone() });
    }
    let was = old.is_timed_out(now_ms);
    let is = new.is_timed_out(now_ms);
    if !was && is {
        out.timeout = Some(TimeoutChange::Added { until: new.timeout_until });
    } else if was && !is {
        out.timeout = Some(TimeoutChange::Removed);
    } else if was && is && old.timeout_until != new.timeout_until {
        out.timeout = Some(TimeoutChange::Changed { from: old.timeout_until, until: new.timeout_until });
    }
    out
}

fn member_embed(now_ms: i64, color: u32, title: &str, user: Option<&LogUser>) -> Builder {
    log_embed_builder(now_ms, color, title).author(user)
}

/// `buildMemberRolesEmbed({ member, roles })`.
pub fn build_member_roles_embed(member: &LogMember, roles: &RolesChange, now_ms: i64) -> Value {
    let e = member_embed(now_ms, log_colors::UPDATE, "🎭 Member Roles Changed", member.user.as_ref());
    let mut fields = vec![field("Member", &user_line(member.user.as_ref()), false)];
    if !roles.added.is_empty() {
        let l: Vec<String> = roles.added.iter().map(|r| role_ref(r)).collect();
        fields.push(field("✅ Added", &join(&l, ", ", 1024), false));
    }
    if !roles.removed.is_empty() {
        let l: Vec<String> = roles.removed.iter().map(|r| role_ref(r)).collect();
        fields.push(field("❌ Removed", &join(&l, ", ", 1024), false));
    }
    e.footer(&format!("User ID: {}", member.user_id())).fields(fields).fit()
}

/// `buildNicknameEmbed({ member, nickname })`.
pub fn build_nickname_embed(member: &LogMember, nickname: &NicknameChange, now_ms: i64) -> Value {
    let e = member_embed(now_ms, log_colors::UPDATE, "📝 Nickname Changed", member.user.as_ref());
    let username = member.user.as_ref().and_then(|u| u.username.clone()).unwrap_or_else(|| "user".into());
    let none = format!("*{}* (no nickname)", clamp_text(&username, 100).replace(['*', '_', '~', '`', '|'], ""));
    let show = |n: &Option<String>| match n.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => clamp_text(s, 1024),
        None => none.clone(),
    };
    e.footer(&format!("User ID: {}", member.user_id()))
        .fields(vec![
            field("Member", &user_line(member.user.as_ref()), false),
            field("Before", &show(&nickname.before), true),
            field("After", &show(&nickname.after), true),
        ])
        .fit()
}

fn until_text(ms: Option<i64>) -> String {
    match ms {
        Some(d) => format!("{} ({})", ts(d, "f"), ts(d, "R")),
        None => "Unknown".into(),
    }
}

/// `buildTimeoutEmbed({ member, timeout })`.
pub fn build_timeout_embed(member: &LogMember, timeout: &TimeoutChange, now_ms: i64) -> Value {
    let user = member.user.as_ref();
    let mut fields = vec![field("Member", &user_line(user), false)];
    let mut footer = format!("User ID: {}", member.user_id());
    let e = match timeout {
        TimeoutChange::Removed => {
            member_embed(now_ms, log_colors::CREATE, "🔊 Timeout Removed", user).desc("The timeout was lifted before it expired.")
        }
        TimeoutChange::Changed { from, until } => {
            fields.push(field("Before", &until_text(*from), true));
            fields.push(field("After", &until_text(*until), true));
            member_embed(now_ms, log_colors::ALERT, "⏱️ Timeout Changed", user)
        }
        TimeoutChange::ActiveUnknown { until } => {
            fields.push(field("Until", &until_text(*until), false));
            footer.push_str(" · previous state not cached");
            member_embed(now_ms, log_colors::ALERT, "⏱️ Member Timed Out", user)
        }
        TimeoutChange::Added { until } => {
            fields.push(field("Until", &until_text(*until), false));
            member_embed(now_ms, log_colors::ALERT, "⏱️ Member Timed Out", user)
        }
    };
    e.footer(&footer).fields(fields).fit()
}

/// `buildAvatarEmbed({ member, avatar })`.
pub fn build_avatar_embed(member: &LogMember, avatar: &AvatarChange, now_ms: i64) -> Value {
    member_embed(now_ms, log_colors::UPDATE, "🖼️ Server Avatar Changed", member.user.as_ref())
        .thumb(avatar.after.as_deref())
        .footer(&format!("User ID: {}", member.user_id()))
        .fields(vec![
            field("Member", &user_line(member.user.as_ref()), false),
            field("Before", avatar.before.as_deref().unwrap_or("(none)"), false),
            field("After", avatar.after.as_deref().unwrap_or("(none)"), false),
        ])
        .fit()
}

// ── User updates ──────────────────────────────────────────────────────────────

/// A before/after pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub before: Option<String>,
    pub after: Option<String>,
}

/// The result of [`diff_user`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserDiff {
    pub username: Option<Change>,
    pub display_name: Option<Change>,
    pub avatar: Option<Change>,
}

/// `diffUser(oldUser, newUser)`: username, display name (globalName) and avatar (`displayAvatarURL({ size: 256 })`).
pub fn diff_user(old: Option<&LogUser>, new: Option<&LogUser>) -> UserDiff {
    let mut out = UserDiff::default();
    let un = |u: Option<&LogUser>| u.and_then(|x| x.username.clone());
    let gn = |u: Option<&LogUser>| u.and_then(|x| x.global_name.clone());
    let av = |u: Option<&LogUser>| u.and_then(|x| x.avatar_url256.clone());
    if un(old) != un(new) {
        out.username = Some(Change { before: un(old), after: un(new) });
    }
    if gn(old) != gn(new) {
        out.display_name = Some(Change { before: gn(old), after: gn(new) });
    }
    if av(old) != av(new) {
        out.avatar = Some(Change { before: av(old), after: av(new) });
    }
    out
}

/// `buildUserUpdateEmbed({ user, diff })`: `None` when nothing changed.
pub fn build_user_update_embed(user: &LogUser, diff: &UserDiff, now_ms: i64) -> Option<Value> {
    if diff.username.is_none() && diff.display_name.is_none() && diff.avatar.is_none() {
        return None;
    }
    let mut e = member_embed(now_ms, log_colors::UPDATE, "👤 User Updated", Some(user));
    let mut fields = vec![field("User", &user_line(Some(user)), false)];
    if let Some(c) = &diff.username {
        fields.push(field("Username", &change(c.before.as_deref(), c.after.as_deref()), false));
    }
    if let Some(c) = &diff.display_name {
        fields.push(field("Display name", &change(c.before.as_deref(), c.after.as_deref()), false));
    }
    if let Some(c) = &diff.avatar {
        fields.push(field("Avatar", "Avatar changed", false));
        e = e.thumb(c.after.as_deref());
    }
    Some(e.footer(&format!("User ID: {}", user.id)).fields(fields).fit())
}

// ── Bans (§6.4.4) ─────────────────────────────────────────────────────────────

/// `buildBanEmbed({ user, executorId, reason, unban, botId })`.
pub fn build_ban_embed(user: &LogUser, executor_id: Option<&str>, reason: Option<&str>, unban: bool, bot_id: Option<&str>, now_ms: i64) -> Value {
    let (color, title) = if unban { (log_colors::CREATE, "✅ Member Unbanned") } else { (log_colors::DELETE, "🔨 Member Banned") };
    let by = match executor_id.filter(|x| !x.is_empty()) {
        Some(x) => format!("<@{x}>{}", if bot_id.is_some_and(|b| b == x) { " (via bot command)" } else { "" }),
        None => "Unknown".into(),
    };
    let reason = match reason.filter(|r| !r.is_empty()) {
        Some(r) => clamp_text(r, 1024),
        None => "No reason given".into(),
    };
    log_embed_builder(now_ms, color, title)
        .thumb(user.avatar_url.as_deref())
        .footer(&format!("User ID: {}", user.id))
        .fields(vec![field("User", &user_line(Some(user)), true), field("By", &by, true), field("Reason", &reason, false)])
        .fit()
}

// ── Voice (§6.5) ──────────────────────────────────────────────────────────────

/// A voice change kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceKind {
    Join,
    Leave,
    Move,
    State,
}

/// The result of [`classify_voice`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceClass {
    pub kind: Option<VoiceKind>,
    pub old_id: Option<String>,
    pub new_id: Option<String>,
    pub changes: Vec<&'static str>,
}

/// `classifyVoice(oldState, newState)`: by channel ids (a deleted channel keeps its id).
pub fn classify_voice(old: Option<&LogVoiceState>, new: Option<&LogVoiceState>) -> VoiceClass {
    let old_id = old.and_then(|s| s.channel_id.clone()).filter(|s| !s.is_empty());
    let new_id = new.and_then(|s| s.channel_id.clone()).filter(|s| !s.is_empty());
    let mut out = VoiceClass { kind: None, old_id: old_id.clone(), new_id: new_id.clone(), changes: Vec::new() };
    match (&old_id, &new_id) {
        (None, None) => return out,
        (None, Some(_)) => {
            out.kind = Some(VoiceKind::Join);
            return out;
        }
        (Some(_), None) => {
            out.kind = Some(VoiceKind::Leave);
            return out;
        }
        (Some(a), Some(b)) if a != b => {
            out.kind = Some(VoiceKind::Move);
            return out;
        }
        _ => {}
    }
    let d = LogVoiceState::default();
    let o = old.unwrap_or(&d);
    let n = new.unwrap_or(&d);
    let mut c = Vec::new();
    let mut pair = |was: bool, is: bool, on: &'static str, off: &'static str| {
        if !was && is {
            c.push(on);
        }
        if was && !is {
            c.push(off);
        }
    };
    pair(o.server_mute, n.server_mute, "Server muted", "Server unmuted");
    pair(o.server_deaf, n.server_deaf, "Server deafened", "Server undeafened");
    pair(o.self_deaf, n.self_deaf, "Self-deafened", "Self-undeafened");
    pair(o.self_mute, n.self_mute, "Self-muted", "Self-unmuted");
    pair(o.streaming, n.streaming, "Started streaming", "Stopped streaming");
    if c.is_empty() {
        return out;
    }
    out.kind = Some(VoiceKind::State);
    out.changes = c;
    out
}

/// `buildVoiceEmbed({ member, guild, c })`: `None` for no change.
pub fn build_voice_embed(member: &LogMember, known: Option<&HashSet<String>>, names: Option<&HashMap<String, String>>, c: &VoiceClass, now_ms: i64) -> Option<Value> {
    // `voiceChannelRef`: the mention plus the cached name in code (a deleted temp channel renders as "#unknown"), else `channel_ref`.
    let channel_ref = |cid: Option<&str>| -> String {
        match cid.and_then(|i| names.and_then(|n| n.get(i)).filter(|n| !n.is_empty()).map(|n| (i, n))) {
            Some((i, n)) => format!("<#{i}> `{}`", ticked(n)),
            None => channel_ref(known, cid),
        }
    };
    let id = if member.id.is_empty() { member.user_id() } else { member.id.clone() };
    let (title, color, desc) = match c.kind? {
        VoiceKind::Join => ("🔊 Joined Voice", log_colors::CREATE, format!("<@{id}> joined {}", channel_ref(c.new_id.as_deref()))),
        VoiceKind::Leave => ("🔇 Left Voice", log_colors::DELETE, format!("<@{id}> left {}", channel_ref(c.old_id.as_deref()))),
        VoiceKind::Move => (
            "↔️ Moved Voice Channel",
            log_colors::UPDATE,
            format!("<@{id}> moved from {} → {}", channel_ref(c.old_id.as_deref()), channel_ref(c.new_id.as_deref())),
        ),
        VoiceKind::State => (
            "🔔 Voice State Changed",
            log_colors::NEUTRAL,
            format!("<@{id}> in {}\n{}", channel_ref(c.new_id.as_deref().or(c.old_id.as_deref())), c.changes.join(", ")),
        ),
    };
    Some(log_embed_builder(now_ms, color, title).desc(&desc).author(member.user.as_ref()).footer(&format!("User ID: {id}")).fit())
}

// ── Channels (§6.6) ───────────────────────────────────────────────────────────

fn ticked(name: &str) -> String {
    clamp_text(name, 100).replace('`', "'")
}

/// `buildChannelCreateEmbed({ channel })`.
pub fn build_channel_create_embed(channel: &LogChannel, now_ms: i64) -> Value {
    let mut fields = vec![
        field("Name", &format!("<#{}> `{}`", channel.id, ticked(&channel.name)), true),
        field("Type", &channel_type_label(channel.kind), true),
    ];
    if let Some(p) = channel.parent_name.as_deref().filter(|p| !p.is_empty()) {
        fields.push(field("Category", p, true));
    }
    if let Some(t) = channel.topic.as_deref().filter(|t| !t.is_empty()) {
        fields.push(field("Topic", &clamp_text(t, 1024), false));
    }
    log_embed_builder(now_ms, log_colors::CREATE, "📢 Channel Created").footer(&format!("Channel ID: {}", channel.id)).fields(fields).fit()
}

fn overwrite_target(o: &LogOverwrite) -> String {
    if o.kind == 1 { format!("<@{}>", o.id) } else { format!("<@&{}>", o.id) }
}

fn perm_names(bits: u64) -> String {
    bot_core::permissions::permission_names_of_bits(serenity::all::Permissions::from_bits_retain(bits)).join(", ")
}

/// One line per permission overwrite that was added, removed or changed.
fn diff_overwrites(old: &LogChannel, new: &LogChannel) -> Vec<String> {
    let mut lines = Vec::new();
    for n in &new.overwrites {
        let Some(o) = old.overwrites.iter().find(|o| o.id == n.id) else {
            let mut parts = Vec::new();
            if n.allow != 0 {
                parts.push(format!("✅ {}", perm_names(n.allow)));
            }
            if n.deny != 0 {
                parts.push(format!("❌ {}", perm_names(n.deny)));
            }
            let tail = if parts.is_empty() { String::new() } else { format!(" · {}", parts.join(" · ")) };
            lines.push(format!("{}: added{tail}", overwrite_target(n)));
            continue;
        };
        if o.allow == n.allow && o.deny == n.deny {
            continue;
        }
        let allowed = n.allow & !o.allow;
        let denied = n.deny & !o.deny;
        let reset = (o.allow | o.deny) & !(n.allow | n.deny);
        let mut parts = Vec::new();
        if allowed != 0 {
            parts.push(format!("✅ {}", perm_names(allowed)));
        }
        if denied != 0 {
            parts.push(format!("❌ {}", perm_names(denied)));
        }
        if reset != 0 {
            parts.push(format!("↩️ {}", perm_names(reset)));
        }
        lines.push(format!("{}: {}", overwrite_target(n), parts.join(" · ")));
    }
    for o in &old.overwrites {
        if !new.overwrites.iter().any(|n| n.id == o.id) {
            lines.push(format!("{}: removed", overwrite_target(o)));
        }
    }
    lines.into_iter().map(|l| clamp_text(&l, 500)).collect()
}

/// `diffChannel(oldChannel, newChannel)`: a position-only change gives no lines.
pub fn diff_channel(o: &LogChannel, n: &LogChannel) -> Vec<Line> {
    let mut lines = Vec::new();
    if o.name != n.name {
        lines.push(Line::new("Name", change(Some(&o.name), Some(&n.name))));
    }
    if o.topic.as_deref().unwrap_or("") != n.topic.as_deref().unwrap_or("") {
        let half = |t: &Option<String>| match t.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => clamp_text(s, 500),
            None => "(none)".into(),
        };
        lines.push(Line::new("Topic", format!("**Before:** {}\n**After:** {}", half(&o.topic), half(&n.topic))));
    }
    if o.nsfw != n.nsfw {
        lines.push(Line::new("NSFW", yes_no(n.nsfw).into()));
    }
    let (or, nr) = (o.rate_limit_per_user.unwrap_or(0), n.rate_limit_per_user.unwrap_or(0));
    if or != nr {
        let fmt = |s: u32| if s > 0 { duration_long(f64::from(s) * 1000.0) } else { "off".into() };
        lines.push(Line::new("Slowmode", format!("{} → {}", fmt(or), fmt(nr))));
    }
    if o.parent_id != n.parent_id {
        let fmt = |c: &LogChannel| match (&c.parent_name, &c.parent_id) {
            (Some(name), _) => name.clone(),
            (None, Some(p)) if !p.is_empty() => format!("<#{p}>"),
            _ => "(none)".into(),
        };
        lines.push(Line::new("Category", format!("{} → {}", fmt(o), fmt(n))));
    }
    if o.kind != n.kind {
        lines.push(Line::new("Type", format!("{} → {}", channel_type_label(o.kind), channel_type_label(n.kind))));
    }
    if let (Some(a), Some(b)) = (o.bitrate, n.bitrate)
        && a != b
    {
        let k = |v: u32| bot_util::text::js_round(f64::from(v) / 1000.0);
        lines.push(Line::new("Bitrate", format!("{} kbps → {} kbps", k(a), k(b))));
    }
    if let (Some(a), Some(b)) = (o.user_limit, n.user_limit)
        && a != b
    {
        let fmt = |v: u32| if v > 0 { v.to_string() } else { "unlimited".into() };
        lines.push(Line::new("User Limit", format!("{} → {}", fmt(a), fmt(b))));
    }
    if let (Some(a), Some(b)) = (&o.rtc_region, &n.rtc_region)
        && a != b
    {
        let fmt = |v: &Option<String>| v.clone().unwrap_or_else(|| "automatic".into());
        lines.push(Line::new("Region", format!("{} → {}", fmt(a), fmt(b))));
    }
    let ows = diff_overwrites(o, n);
    if !ows.is_empty() {
        lines.push(Line::new("Permissions", join(&ows, "\n", 1024)));
    }
    lines
}

fn line_fields(lines: &[Line]) -> Vec<Value> {
    lines.iter().map(|l| field(&l.name, &l.value, false)).collect()
}

/// `buildChannelUpdateEmbed({ channel, lines })`: `None` without lines.
pub fn build_channel_update_embed(channel: &LogChannel, lines: &[Line], now_ms: i64) -> Option<Value> {
    if lines.is_empty() {
        return None;
    }
    Some(
        log_embed_builder(now_ms, log_colors::UPDATE, "🛠️ Channel Updated")
            .desc(&format!("<#{}> `{}`", channel.id, ticked(&channel.name)))
            .footer(&format!("Channel ID: {}", channel.id))
            .fields(line_fields(lines))
            .fit(),
    )
}

/// `buildChannelDeleteEmbed({ channel, occupants, botReason, botId })`.
pub fn build_channel_delete_embed(channel: &LogChannel, occupants: &[String], bot_reason: Option<&str>, bot_id: Option<&str>, now_ms: i64) -> Value {
    let mut fields = vec![field("Name", &format!("`{}`", ticked(&channel.name)), true), field("Type", &channel_type_label(channel.kind), true)];
    if let Some(p) = channel.parent_name.as_deref().filter(|p| !p.is_empty()) {
        fields.push(field("Category", p, true));
    }
    if !occupants.is_empty() {
        let l: Vec<String> = occupants.iter().map(|id| format!("<@{id}>")).collect();
        fields.push(field(&format!("Connected Members ({})", occupants.len()), &join(&l, ", ", 1024), false));
    }
    if let Some(r) = bot_reason.filter(|r| !r.is_empty()) {
        fields.push(field("Deleted By", &deleted_by_bot(bot_id, r), false));
    }
    log_embed_builder(now_ms, log_colors::DELETE, "🗑️ Channel Deleted").footer(&format!("Channel ID: {}", channel.id)).fields(fields).fit()
}

// ── Threads ───────────────────────────────────────────────────────────────────

fn archive_label(minutes: Option<u32>) -> String {
    match minutes.filter(|m| *m > 0) {
        Some(60) => "1 hour".into(),
        Some(1440) => "24 hours".into(),
        Some(4320) => "3 days".into(),
        Some(10080) => "1 week".into(),
        Some(m) => duration_long(f64::from(m) * 60_000.0),
        None => "Unknown".into(),
    }
}

fn thread_name(thread: &LogThread) -> String {
    ticked(thread.name.as_deref().unwrap_or("unknown"))
}

fn tag_label(thread: &LogThread, id: &str) -> String {
    match thread.available_tags.iter().find(|t| t.id == id).filter(|t| !t.name.is_empty()) {
        Some(t) => clamp_text(&t.name, 50),
        None => id.to_string(),
    }
}

/// `buildThreadCreateEmbed({ thread })`.
pub fn build_thread_create_embed(thread: &LogThread, now_ms: i64) -> Value {
    let mut fields = vec![field("Thread", &format!("<#{}> `{}`", thread.id, thread_name(thread)), true), field("Type", &channel_type_label(thread.kind), true)];
    if let Some(p) = thread.parent_id.as_deref().filter(|p| !p.is_empty()) {
        fields.push(field("Channel", &format!("<#{p}>"), true));
    }
    if let Some(o) = thread.owner_id.as_deref().filter(|p| !p.is_empty()) {
        fields.push(field("Started By", &format!("<@{o}>"), true));
    }
    if let Some(a) = thread.auto_archive_duration.filter(|a| *a > 0) {
        fields.push(field("Auto-archive", &archive_label(Some(a)), true));
    }
    if let Some(s) = thread.rate_limit_per_user.filter(|s| *s > 0) {
        fields.push(field("Slowmode", &duration_long(f64::from(s) * 1000.0), true));
    }
    log_embed_builder(now_ms, log_colors::CREATE, "🧵 Thread Created").footer(&format!("Thread ID: {}", thread.id)).fields(fields).fit()
}

/// `diffThread(oldThread, newThread)`: name, locked, slowmode, auto-archive, invitable and tags. Archiving is never a
/// line; unknown (partial) fields are never compared.
pub fn diff_thread(o: &LogThread, n: &LogThread) -> Vec<Line> {
    let mut lines = Vec::new();
    if let (Some(a), Some(b)) = (&o.name, &n.name)
        && a != b
    {
        lines.push(Line::new("Name", change(Some(a), Some(b))));
    }
    if let (Some(a), Some(b)) = (o.locked, n.locked)
        && a != b
    {
        lines.push(Line::new("Locked", if b { "Locked".into() } else { "Unlocked".into() }));
    }
    if let (Some(a), Some(b)) = (o.rate_limit_per_user, n.rate_limit_per_user)
        && a != b
    {
        let fmt = |s: u32| if s > 0 { duration_long(f64::from(s) * 1000.0) } else { "off".into() };
        lines.push(Line::new("Slowmode", format!("{} → {}", fmt(a), fmt(b))));
    }
    if let (Some(a), Some(b)) = (o.auto_archive_duration, n.auto_archive_duration)
        && a != b
    {
        lines.push(Line::new("Auto-archive", format!("{} → {}", archive_label(Some(a)), archive_label(Some(b)))));
    }
    if let (Some(a), Some(b)) = (o.invitable, n.invitable)
        && a != b
    {
        lines.push(Line::new("Invitable", yes_no(b).into()));
    }
    if let (Some(a), Some(b)) = (&o.applied_tags, &n.applied_tags)
        && !same_set(a, b)
    {
        let mut list: Vec<String> = b.iter().filter(|id| !a.contains(id)).map(|id| format!("+ {}", tag_label(n, id))).collect();
        list.extend(a.iter().filter(|id| !b.contains(id)).map(|id| format!("- {}", tag_label(n, id))));
        lines.push(Line::new("Tags", join(&list, "\n", 1024)));
    }
    lines
}

/// `buildThreadUpdateEmbed({ thread, lines })`: `None` without lines.
pub fn build_thread_update_embed(thread: &LogThread, lines: &[Line], now_ms: i64) -> Option<Value> {
    if lines.is_empty() {
        return None;
    }
    Some(
        log_embed_builder(now_ms, log_colors::UPDATE, "🧵 Thread Updated")
            .desc(&format!("<#{}> `{}`", thread.id, thread_name(thread)))
            .footer(&format!("Thread ID: {}", thread.id))
            .fields(line_fields(lines))
            .fit(),
    )
}

/// `buildThreadDeleteEmbed({ thread, botReason, botId })`. `known` is the guild's channel cache.
pub fn build_thread_delete_embed(thread: &LogThread, bot_reason: Option<&str>, bot_id: Option<&str>, now_ms: i64, known: Option<&HashSet<String>>) -> Value {
    let mut fields = vec![field("Name", &format!("`{}`", thread_name(thread)), true), field("Type", &channel_type_label(thread.kind), true)];
    if let Some(p) = thread.parent_id.as_deref().filter(|p| !p.is_empty()) {
        fields.push(field("Channel", &channel_ref(known, Some(p)), true));
    }
    if let Some(o) = thread.owner_id.as_deref().filter(|p| !p.is_empty()) {
        fields.push(field("Started By", &format!("<@{o}>"), true));
    }
    if let Some(r) = bot_reason.filter(|r| !r.is_empty()) {
        fields.push(field("Deleted By", &deleted_by_bot(bot_id, r), false));
    }
    log_embed_builder(now_ms, log_colors::DELETE, "🗑️ Thread Deleted").footer(&format!("Thread ID: {}", thread.id)).fields(fields).fit()
}

// ── Roles (§6.7) ──────────────────────────────────────────────────────────────

fn hex(role: &LogRole) -> String {
    role.hex_color.clone().unwrap_or_else(|| "#000000".into())
}

/// `buildRoleCreateEmbed({ role })`.
pub fn build_role_create_embed(role: &LogRole, now_ms: i64) -> Value {
    let perms = bot_core::permissions::permission_names_of_bits(serenity::all::Permissions::from_bits_retain(role.permissions));
    let mut fields = vec![
        field("Role", &format!("<@&{}> `{}`", role.id, ticked(&role.name)), false),
        field("Color", &hex(role), true),
        field("Hoisted", yes_no(role.hoist), true),
        field("Mentionable", yes_no(role.mentionable), true),
    ];
    if role.managed {
        fields.push(field("Managed", "Yes (integration or bot role)", false));
    }
    fields.push(field("Permissions", &if perms.is_empty() { "None".into() } else { join(&perms, ", ", 1024) }, false));
    log_embed_builder(now_ms, log_colors::CREATE, "➕ Role Created").footer(&format!("Role ID: {}", role.id)).fields(fields).fit()
}

/// `diffRole(oldRole, newRole)`: a position-only change gives no lines.
pub fn diff_role(o: &LogRole, n: &LogRole) -> Vec<Line> {
    let mut lines = Vec::new();
    if o.name != n.name {
        lines.push(Line::new("Name", change(Some(&o.name), Some(&n.name))));
    }
    if o.hex_color != n.hex_color {
        lines.push(Line::new("Color", change(o.hex_color.as_deref(), n.hex_color.as_deref())));
    }
    if o.hoist != n.hoist {
        lines.push(Line::new("Hoisted", format!("{} → {}", yes_no(o.hoist), yes_no(n.hoist))));
    }
    if o.mentionable != n.mentionable {
        lines.push(Line::new("Mentionable", format!("{} → {}", yes_no(o.mentionable), yes_no(n.mentionable))));
    }
    if o.icon != n.icon || o.unicode_emoji != n.unicode_emoji {
        lines.push(Line::new("Icon", "changed".into()));
    }
    let added = n.permissions & !o.permissions;
    let removed = o.permissions & !n.permissions;
    let names = |b: u64| bot_core::permissions::permission_names_of_bits(serenity::all::Permissions::from_bits_retain(b));
    if added != 0 {
        lines.push(Line::new("Permissions Added", join(&names(added), ", ", 1024)));
    }
    if removed != 0 {
        lines.push(Line::new("Permissions Removed", join(&names(removed), ", ", 1024)));
    }
    lines
}

/// `buildRoleUpdateEmbed({ role, lines })`: `None` without lines.
pub fn build_role_update_embed(role: &LogRole, lines: &[Line], now_ms: i64) -> Option<Value> {
    if lines.is_empty() {
        return None;
    }
    Some(
        log_embed_builder(now_ms, log_colors::UPDATE, "🛠️ Role Updated")
            .desc(&format!("<@&{}> `{}`", role.id, ticked(&role.name)))
            .footer(&format!("Role ID: {}", role.id))
            .fields(line_fields(lines))
            .fit(),
    )
}

/// `buildRoleDeleteEmbed({ role })`.
pub fn build_role_delete_embed(role: &LogRole, now_ms: i64) -> Value {
    let mut fields = vec![field("Name", &format!("`{}`", ticked(&role.name)), true), field("Color", &hex(role), true)];
    if let Some(c) = role.created_timestamp {
        fields.push(field("Created", &ts(c, "R"), true));
    }
    log_embed_builder(now_ms, log_colors::DELETE, "➖ Role Deleted").footer(&format!("Role ID: {}", role.id)).fields(fields).fit()
}

// ── Server (§6.8) ─────────────────────────────────────────────────────────────

const VERIFICATION: &[&str] = &["None", "Low", "Medium", "High", "VeryHigh"];
const CONTENT_FILTER: &[&str] = &["Disabled", "MembersWithoutRoles", "AllMembers"];
const NOTIFICATIONS: &[&str] = &["AllMessages", "OnlyMentions"];
const MFA: &[&str] = &["None", "Elevated"];
const NSFW: &[&str] = &["Default", "Explicit", "Safe", "AgeRestricted"];

fn enum_name(names: &[&str], v: Option<u8>) -> String {
    match v {
        None => "(none)".into(),
        Some(i) => names.get(usize::from(i)).map(|s| s.to_string()).unwrap_or_else(|| i.to_string()),
    }
}

/// `diffGuild(oldGuild, newGuild)`: properties that are not listed (member and boost counts) produce nothing.
pub fn diff_guild(o: &LogGuild, n: &LogGuild) -> Vec<Line> {
    let mut lines = Vec::new();
    let ch = |id: &Option<String>| match id.as_deref().filter(|s| !s.is_empty()) {
        Some(i) => format!("<#{i}>"),
        None => "*(none)*".into(),
    };
    let mut add = |name: &str, value: String| lines.push(Line::new(name, value));
    if o.name != n.name {
        add("Name", change(o.name.as_deref(), n.name.as_deref()));
    }
    if o.icon != n.icon {
        add("Icon", "changed".into());
    }
    if o.banner != n.banner {
        add("Banner", "changed".into());
    }
    if o.splash != n.splash {
        add("Invite Splash", "changed".into());
    }
    if o.description != n.description {
        let half = |t: &Option<String>| match t.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => clamp_text(s, 500),
            None => "(none)".into(),
        };
        add("Description", format!("**Before:** {}\n**After:** {}", half(&o.description), half(&n.description)));
    }
    if o.owner_id != n.owner_id {
        let id = |v: &Option<String>| v.clone().unwrap_or_else(|| "undefined".into());
        add("Owner", format!("<@{}> → <@{}>", id(&o.owner_id), id(&n.owner_id)));
    }
    if o.verification_level != n.verification_level {
        add("Verification Level", format!("{} → {}", enum_name(VERIFICATION, o.verification_level), enum_name(VERIFICATION, n.verification_level)));
    }
    if o.explicit_content_filter != n.explicit_content_filter {
        add(
            "Explicit Content Filter",
            format!("{} → {}", enum_name(CONTENT_FILTER, o.explicit_content_filter), enum_name(CONTENT_FILTER, n.explicit_content_filter)),
        );
    }
    if o.default_message_notifications != n.default_message_notifications {
        add(
            "Default Notifications",
            format!("{} → {}", enum_name(NOTIFICATIONS, o.default_message_notifications), enum_name(NOTIFICATIONS, n.default_message_notifications)),
        );
    }
    if o.afk_channel_id != n.afk_channel_id {
        add("AFK Channel", format!("{} → {}", ch(&o.afk_channel_id), ch(&n.afk_channel_id)));
    }
    if o.afk_timeout != n.afk_timeout {
        let f = |v: Option<u64>| duration_long(v.unwrap_or(0) as f64 * 1000.0);
        add("AFK Timeout", format!("{} → {}", f(o.afk_timeout), f(n.afk_timeout)));
    }
    if o.system_channel_id != n.system_channel_id {
        add("System Channel", format!("{} → {}", ch(&o.system_channel_id), ch(&n.system_channel_id)));
    }
    if o.rules_channel_id != n.rules_channel_id {
        add("Rules Channel", format!("{} → {}", ch(&o.rules_channel_id), ch(&n.rules_channel_id)));
    }
    if o.public_updates_channel_id != n.public_updates_channel_id {
        add("Community Updates Channel", format!("{} → {}", ch(&o.public_updates_channel_id), ch(&n.public_updates_channel_id)));
    }
    if o.vanity_url_code != n.vanity_url_code {
        add("Vanity URL", change(o.vanity_url_code.as_deref(), n.vanity_url_code.as_deref()));
    }
    if o.preferred_locale != n.preferred_locale {
        add("Preferred Locale", change(o.preferred_locale.as_deref(), n.preferred_locale.as_deref()));
    }
    if o.premium_tier != n.premium_tier {
        add("Boost Level", format!("{} → {}", o.premium_tier.unwrap_or(0), n.premium_tier.unwrap_or(0)));
    }
    if o.mfa_level != n.mfa_level {
        add("2FA Requirement", format!("{} → {}", enum_name(MFA, o.mfa_level), enum_name(MFA, n.mfa_level)));
    }
    if o.nsfw_level != n.nsfw_level {
        add("NSFW Level", format!("{} → {}", enum_name(NSFW, o.nsfw_level), enum_name(NSFW, n.nsfw_level)));
    }
    if o.premium_progress_bar_enabled != n.premium_progress_bar_enabled {
        add(
            "Boost Progress Bar",
            format!("{} → {}", yes_no(o.premium_progress_bar_enabled.unwrap_or(false)), yes_no(n.premium_progress_bar_enabled.unwrap_or(false))),
        );
    }
    lines
}

/// `buildGuildUpdateEmbed({ oldGuild, newGuild, lines })`: `None` without lines.
pub fn build_guild_update_embed(old: &LogGuild, new: &LogGuild, lines: &[Line], now_ms: i64) -> Option<Value> {
    if lines.is_empty() {
        return None;
    }
    let mut e = log_embed_builder(now_ms, log_colors::UPDATE, "🏠 Server Edited");
    if old.icon != new.icon {
        e = e.thumb(new.icon_url.as_deref());
    }
    Some(e.footer(&format!("Server ID: {}", new.id)).fields(line_fields(lines)).fit())
}

// ── Emojis (§6.9) ─────────────────────────────────────────────────────────────

/// `buildEmojiCreateEmbed({ emoji })`.
pub fn build_emoji_create_embed(emoji: &LogEmoji, now_ms: i64) -> Value {
    let mut fields = vec![field("Emoji", &format!("{} `:{}:`", emoji.mention, clamp_text(&emoji.name, 64)), false), field("Animated", yes_no(emoji.animated), true)];
    if !emoji.roles.is_empty() {
        let l: Vec<String> = emoji.roles.iter().map(|r| role_ref(r)).collect();
        fields.push(field("Restricted To", &join(&l, ", ", 1024), false));
    }
    log_embed_builder(now_ms, log_colors::CREATE, "😀 Emoji Created")
        .thumb(emoji.image_url.as_deref())
        .footer(&format!("Emoji ID: {}", emoji.id))
        .fields(fields)
        .fit()
}

/// `diffEmoji(oldEmoji, newEmoji)` (lines are not clamped here, as in the JS).
pub fn diff_emoji(o: &LogEmoji, n: &LogEmoji) -> Vec<Line> {
    let mut lines = Vec::new();
    if o.name != n.name {
        lines.push(Line { name: "Name".into(), value: change(Some(&o.name), Some(&n.name)) });
    }
    if !same_set(&o.roles, &n.roles) {
        let added: Vec<String> = n.roles.iter().filter(|id| !o.roles.contains(id)).map(|r| role_ref(r)).collect();
        let removed: Vec<String> = o.roles.iter().filter(|id| !n.roles.contains(id)).map(|r| role_ref(r)).collect();
        let mut parts = Vec::new();
        if !added.is_empty() {
            parts.push(format!("✅ {}", join(&added, ", ", 480)));
        }
        if !removed.is_empty() {
            parts.push(format!("❌ {}", join(&removed, ", ", 480)));
        }
        lines.push(Line { name: "Roles".into(), value: parts.join("\n") });
    }
    lines
}

/// `buildEmojiUpdateEmbed({ emoji, lines })`: `None` without lines.
pub fn build_emoji_update_embed(emoji: &LogEmoji, lines: &[Line], now_ms: i64) -> Option<Value> {
    if lines.is_empty() {
        return None;
    }
    Some(
        log_embed_builder(now_ms, log_colors::UPDATE, "😀 Emoji Updated")
            .thumb(emoji.image_url.as_deref())
            .desc(&format!("{} `:{}:`", emoji.mention, clamp_text(&emoji.name, 64)))
            .footer(&format!("Emoji ID: {}", emoji.id))
            .fields(line_fields(lines))
            .fit(),
    )
}

/// `buildEmojiDeleteEmbed({ emoji })`.
pub fn build_emoji_delete_embed(emoji: &LogEmoji, now_ms: i64) -> Value {
    log_embed_builder(now_ms, log_colors::DELETE, "😶 Emoji Deleted")
        .thumb(emoji.image_url.as_deref())
        .footer(&format!("Emoji ID: {}", emoji.id))
        .fields(vec![field("Name", &format!("`:{}:`", clamp_text(&emoji.name, 64)), false)])
        .fit()
}

// ── Invites (§6.10) ───────────────────────────────────────────────────────────

/// `buildInviteCreateEmbed({ invite })`.
pub fn build_invite_create_embed(invite: &LogInvite, now_ms: i64) -> Value {
    let channel = invite.channel_id.as_deref().filter(|c| !c.is_empty()).map(|c| format!("<#{c}>")).unwrap_or_else(|| "Unknown".into());
    let inviter = match &invite.inviter {
        Some(u) => user_line(Some(u)),
        None => "Unknown".into(),
    };
    let max = invite.max_uses.filter(|m| *m > 0).map(|m| m.to_string()).unwrap_or_else(|| "∞".into());
    let expires = invite.expires_timestamp.map(|e| ts(e, "R")).unwrap_or_else(|| "Never".into());
    log_embed_builder(now_ms, log_colors::UPDATE, "🔗 Invite Created")
        .footer(&format!("Code: {}", invite.code))
        .fields(vec![
            field("Code", &format!("discord.gg/{}", invite.code), true),
            field("Channel", &channel, true),
            field("Inviter", &inviter, true),
            field("Max Uses", &max, true),
            field("Expires", &expires, true),
            field("Temporary", yes_no(invite.temporary), true),
        ])
        .fit()
}

/// `buildInviteDeleteEmbed({ invite })`.
pub fn build_invite_delete_embed(invite: &LogInvite, now_ms: i64) -> Value {
    let channel = invite.channel_id.as_deref().filter(|c| !c.is_empty()).map(|c| format!("<#{c}>")).unwrap_or_else(|| "Unknown".into());
    log_embed_builder(now_ms, log_colors::DELETE, "🔗 Invite Deleted / Expired")
        .footer(&format!("Code: {}", invite.code))
        .fields(vec![field("Code", &format!("discord.gg/{}", invite.code), true), field("Channel", &channel, true)])
        .fit()
}
