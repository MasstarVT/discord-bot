//! Port of `src/services/loggingService.js`: the log-type map, the event registry, the settings predicates and
//! [`send_log`]. `send_log` never fails, resolves on enqueue and posts through a per-channel queue that coalesces
//! bursts into messages of up to 10 embeds (about 1 s later). The embed builders live in [`crate::log_events`]; the
//! hooks that feed this service live in [`crate::plugins::logging`].
//!
//! Settings go through the settings service only. Everything else (the dead-channel memo, the queues, a warning
//! throttle) is per app and in memory ([`LoggingState`] in `app.ext`).
//!
//! Discord I/O goes through [`LogApi`] (serenity by default; tests install a recorder with [`set_log_api`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use bot_core::App;
use bot_core::settings::Settings;
use bot_db::GuildSettings;
use bot_util::text::clamp_text;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use serenity::all::{ChannelId, GuildChannel, GuildId, Permissions};
use serenity::builder::{CreateAllowedMentions, CreateAttachment, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateMessage};

use crate::log_events::{LogFile, LogMessage};

// ── Log type → GuildSettings channel field ────────────────────────────────────

/// `LOG_TYPES` keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogType {
    Message,
    Member,
    Server,
    Voice,
    JoinLeave,
    Mod,
}

impl LogType {
    /// Every type, in `LOG_TYPES` order.
    pub const ALL: [LogType; 6] = [LogType::Message, LogType::Member, LogType::Server, LogType::Voice, LogType::JoinLeave, LogType::Mod];

    /// The `LOG_TYPES` key (`'MESSAGE'`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Message => "MESSAGE",
            Self::Member => "MEMBER",
            Self::Server => "SERVER",
            Self::Voice => "VOICE",
            Self::JoinLeave => "JOIN_LEAVE",
            Self::Mod => "MOD",
        }
    }

    /// `LOG_TYPES[key]`: the GuildSettings field.
    pub fn field(self) -> &'static str {
        match self {
            Self::Message => "messageLogChannelId",
            Self::Member => "memberLogChannelId",
            Self::Server => "serverLogChannelId",
            Self::Voice => "voiceLogChannelId",
            Self::JoinLeave => "joinLeaveChannelId",
            Self::Mod => "modLogChannelId",
        }
    }

    /// `LOG_TYPE_LABELS[key]`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Message => "Messages",
            Self::Member => "Members",
            Self::Server => "Server",
            Self::Voice => "Voice",
            Self::JoinLeave => "Joins & leaves",
            Self::Mod => "Moderation",
        }
    }

    /// The type of a `LOG_TYPES` key; `None` for an unknown one.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.key() == key)
    }

    /// The channel configured for this type.
    pub fn channel_of(self, s: &GuildSettings) -> Option<&str> {
        let v = match self {
            Self::Message => &s.message_log_channel_id,
            Self::Member => &s.member_log_channel_id,
            Self::Server => &s.server_log_channel_id,
            Self::Voice => &s.voice_log_channel_id,
            Self::JoinLeave => &s.join_leave_channel_id,
            Self::Mod => &s.mod_log_channel_id,
        };
        v.as_deref().filter(|c| !c.is_empty())
    }
}

// ── Event registry (§3.2) ─────────────────────────────────────────────────────

/// One `LOG_EVENTS` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogEvent {
    pub key: &'static str,
    pub log_type: LogType,
    pub emoji: &'static str,
    pub label: &'static str,
}

const fn ev(key: &'static str, log_type: LogType, emoji: &'static str, label: &'static str) -> LogEvent {
    LogEvent { key, log_type, emoji, label }
}

/// `LOG_EVENTS`, in display order (`/logging status` and the autocomplete follow it).
pub const LOG_EVENTS: [LogEvent; 32] = [
    ev("messageDelete", LogType::Message, "🗑️", "Message deleted"),
    ev("messageEdit", LogType::Message, "✏️", "Message edited"),
    ev("messageBulkDelete", LogType::Message, "🧹", "Messages bulk-deleted"),
    ev("invitePosted", LogType::Message, "📨", "Invite posted"),
    ev("memberRoles", LogType::Member, "🎭", "Member roles changed"),
    ev("memberNickname", LogType::Member, "📝", "Nickname changed"),
    ev("memberTimeout", LogType::Member, "⏱️", "Member timed out / untimed"),
    ev("memberAvatar", LogType::Member, "🖼️", "Server avatar changed"),
    ev("userUpdate", LogType::Member, "👤", "User updated (name / avatar)"),
    ev("memberBan", LogType::Member, "🔨", "Member banned"),
    ev("memberUnban", LogType::Member, "✅", "Member unbanned"),
    ev("memberJoin", LogType::JoinLeave, "📥", "Member joined"),
    ev("memberLeave", LogType::JoinLeave, "📤", "Member left"),
    ev("voiceJoin", LogType::Voice, "🔊", "Voice channel joined"),
    ev("voiceLeave", LogType::Voice, "🔇", "Voice channel left"),
    ev("voiceMove", LogType::Voice, "↔️", "Voice channel switched"),
    ev("voiceState", LogType::Voice, "🔔", "Voice mute / deafen / stream"),
    ev("channelCreate", LogType::Server, "📢", "Channel created"),
    ev("channelUpdate", LogType::Server, "🛠️", "Channel updated"),
    ev("channelDelete", LogType::Server, "🗑️", "Channel deleted"),
    ev("threadCreate", LogType::Server, "🧵", "Thread created"),
    ev("threadUpdate", LogType::Server, "🛠️", "Thread updated"),
    ev("threadDelete", LogType::Server, "🗑️", "Thread deleted"),
    ev("roleCreate", LogType::Server, "➕", "Role created"),
    ev("roleUpdate", LogType::Server, "🛠️", "Role updated"),
    ev("roleDelete", LogType::Server, "➖", "Role deleted"),
    ev("serverUpdate", LogType::Server, "🏠", "Server edited"),
    ev("emojiCreate", LogType::Server, "😀", "Emoji created"),
    ev("emojiUpdate", LogType::Server, "😀", "Emoji updated"),
    ev("emojiDelete", LogType::Server, "😶", "Emoji deleted"),
    ev("inviteCreate", LogType::Server, "🔗", "Invite created"),
    ev("inviteDelete", LogType::Server, "🔗", "Invite deleted / expired"),
];

/// `LOG_EVENT_KEYS`.
pub fn log_event_keys() -> Vec<&'static str> {
    LOG_EVENTS.iter().map(|e| e.key).collect()
}

/// `LOG_EVENTS[key]`.
pub fn log_event(key: &str) -> Option<&'static LogEvent> {
    LOG_EVENTS.iter().find(|e| e.key == key)
}

/// The events the ignore list (`logIgnoredChannelIds`) applies to.
pub const MESSAGE_LOG_EVENTS: [&str; 4] = ["messageDelete", "messageEdit", "messageBulkDelete", "invitePosted"];

pub const DEAD_CHANNEL_TTL_MS: i64 = 600_000;
pub const QUEUE_FLUSH_MS: u64 = 1_000;
pub const QUEUE_MAX_ITEMS: usize = 100;

const DEAD_CHANNEL_MAX: usize = 5_000;
/// A failed (non-404) channel fetch is retried after 30 s, not 10 min.
const TRANSIENT_FETCH_TTL_MS: i64 = 30_000;
const BATCH_MAX_EMBEDS: usize = 10;
const BATCH_MAX_CHARS: usize = 6_000;
const BASE_NEEDS: [&str; 3] = ["ViewChannel", "SendMessages", "EmbedLinks"];
/// Unknown Channel, Missing Access, Missing Permissions: the channel is gone or we may not post there.
const HARD_SEND_CODES: [u64; 3] = [10003, 50001, 50013];

// ── Settings predicates ───────────────────────────────────────────────────────

/// `isLogEventKey(key)`.
pub fn is_log_event_key(key: &str) -> bool {
    log_event(key).is_some()
}

/// `isLogEventEnabled(settings, key)`: a missing settings row counts as on; an unknown key is never enabled.
pub fn is_log_event_enabled(settings: Option<&GuildSettings>, key: &str) -> bool {
    if !is_log_event_key(key) {
        return false;
    }
    !settings.is_some_and(|s| s.disabled_log_events.iter().any(|k| k == key))
}

/// `toggleLogEvents(disabled, name, on)`: the disabledLogEvents array after switching one event (or `all`) on or
/// off; unknown keys stripped, no duplicates, `LOG_EVENT_KEYS` order.
pub fn toggle_log_events<S: AsRef<str>>(disabled: &[S], name: &str, on: bool) -> Vec<String> {
    if name == "all" {
        return if on { Vec::new() } else { log_event_keys().into_iter().map(String::from).collect() };
    }
    let mut set: HashSet<String> = disabled.iter().map(|s| s.as_ref().to_string()).filter(|k| is_log_event_key(k)).collect();
    if is_log_event_key(name) {
        if on {
            set.remove(name);
        } else {
            set.insert(name.to_string());
        }
    }
    log_event_keys().into_iter().filter(|k| set.contains(*k)).map(String::from).collect()
}

/// The ids `isIgnoredChannel` checks: the channel, its parent and the parent's parent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelChain {
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub grandparent_id: Option<String>,
}

impl ChannelChain {
    /// The chain of a cached channel or thread (its parent, then the parent's category).
    pub fn of(app: &App, guild_id: GuildId, channel_id: ChannelId) -> Self {
        let mut chain = Self { id: Some(channel_id.to_string()), ..Default::default() };
        let Some(ch) = bot_core::interaction_helpers::cached_channel(app, guild_id, channel_id) else { return chain };
        if let Some(p) = ch.parent_id {
            chain.parent_id = Some(p.to_string());
            chain.grandparent_id = bot_core::interaction_helpers::cached_channel(app, guild_id, p).and_then(|pc| pc.parent_id).map(|g| g.to_string());
        }
        chain
    }
}

/// `isIgnoredChannel(settings, channel)`: the channel, its parent (a thread's channel or a channel's category) or
/// the parent's category is on the list.
pub fn is_ignored_channel(settings: Option<&GuildSettings>, channel: Option<&ChannelChain>) -> bool {
    let (Some(s), Some(c)) = (settings, channel) else { return false };
    if s.log_ignored_channel_ids.is_empty() {
        return false;
    }
    [&c.id, &c.parent_id, &c.grandparent_id].into_iter().flatten().any(|id| s.log_ignored_channel_ids.contains(id))
}

/// `channelCleanupData(settings, channelId)`: the fields to write when a channel is deleted (every log channel
/// field equal to the id becomes null, the id leaves the ignore list); `None` when nothing changes.
pub fn channel_cleanup_data(settings: Option<&GuildSettings>, channel_id: &str) -> Option<Map<String, Value>> {
    let s = settings?;
    if channel_id.is_empty() {
        return None;
    }
    let mut data = Map::new();
    for t in LogType::ALL {
        if t.channel_of(s) == Some(channel_id) {
            data.insert(t.field().into(), Value::Null);
        }
    }
    if s.log_ignored_channel_ids.iter().any(|i| i == channel_id) {
        let rest: Vec<&String> = s.log_ignored_channel_ids.iter().filter(|i| *i != channel_id).collect();
        data.insert("logIgnoredChannelIds".into(), json!(rest));
    }
    if data.is_empty() { None } else { Some(data) }
}

// ── Channel type label helper ─────────────────────────────────────────────────

/// `channelTypeLabel(type)`.
pub fn channel_type_label(kind: u8) -> String {
    match kind {
        0 => "Text Channel".into(),
        2 => "Voice Channel".into(),
        4 => "Category".into(),
        5 => "Announcement Channel".into(),
        13 => "Stage Channel".into(),
        15 => "Forum Channel".into(),
        16 => "Media Channel".into(),
        14 => "Directory Channel".into(),
        11 => "Public Thread".into(),
        12 => "Private Thread".into(),
        10 => "Announcement Thread".into(),
        other => format!("Unknown ({other})"),
    }
}

// ── Mention extraction (ghost pings) ──────────────────────────────────────────

/// `extractMentions(message, { excludeUserId })`: the mention strings of a message, skipping bots and the excluded
/// user. A partial message gives none.
pub fn extract_mentions(message: Option<&LogMessage>, exclude_user_id: Option<&str>) -> Vec<String> {
    let Some(m) = message.filter(|m| !m.partial) else { return Vec::new() };
    let mut out = Vec::new();
    for u in &m.mention_users {
        if u.bot || exclude_user_id.is_some_and(|x| !x.is_empty() && x == u.id) {
            continue;
        }
        let tag = u.tag.clone().or_else(|| u.username.clone()).unwrap_or_else(|| "undefined".into());
        out.push(format!("<@{}> ({tag})", u.id));
    }
    for r in &m.mention_roles {
        out.push(format!("<@&{}> ({})", r.id, r.name));
    }
    if m.mention_everyone {
        out.push("`@everyone` / `@here`".into());
    }
    out
}

// ── Embed limits ──────────────────────────────────────────────────────────────

const LIMIT_TITLE: usize = 256;
const LIMIT_DESCRIPTION: usize = 4096;
const LIMIT_FIELD_NAME: usize = 256;
const LIMIT_FIELD_VALUE: usize = 1024;
const LIMIT_FOOTER: usize = 2048;
const LIMIT_AUTHOR: usize = 256;
const LIMIT_FIELDS: usize = 25;
const LIMIT_TOTAL: usize = 6000;

fn u16(v: Option<&Value>) -> usize {
    v.and_then(Value::as_str).map(bot_util::js::utf16_len).unwrap_or(0)
}

/// discord.js `embedLength(data)`: title, description, field names and values, footer text and author name.
pub fn embed_length(data: &Value) -> usize {
    let mut n = u16(data.get("title")) + u16(data.get("description"));
    for f in data.get("fields").and_then(Value::as_array).into_iter().flatten() {
        n += u16(f.get("name")) + u16(f.get("value"));
    }
    n + u16(data.get("footer").and_then(|f| f.get("text"))) + u16(data.get("author").and_then(|a| a.get("name")))
}

fn js_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

/// `fitEmbedData(data)`: clamps a plain APIEmbed object to Discord's limits: at most 25 fields (24 plus "… and N more
/// changes"), every string cut, empty field names and values replaced by a zero-width space, and, while the whole
/// embed exceeds 6000 characters, the last fields are dropped and finally the description is cut.
pub fn fit_embed_data(data: &Value) -> Value {
    let mut out = data.as_object().cloned().unwrap_or_default();
    let clamp_key = |out: &mut Map<String, Value>, key: &str, max: usize| {
        if let Some(v) = out.get(key).filter(|v| !v.is_null()) {
            let s = clamp_text(&js_string(Some(v)), max);
            out.insert(key.into(), json!(s));
        }
    };
    clamp_key(&mut out, "title", LIMIT_TITLE);
    clamp_key(&mut out, "description", LIMIT_DESCRIPTION);
    for (key, inner, max) in [("footer", "text", LIMIT_FOOTER), ("author", "name", LIMIT_AUTHOR)] {
        if let Some(Value::Object(o)) = out.get(key).cloned()
            && o.get(inner).is_some_and(|v| !v.is_null())
        {
            let mut o = o;
            let s = clamp_text(&js_string(o.get(inner)), max);
            o.insert(inner.into(), json!(s));
            out.insert(key.into(), Value::Object(o));
        }
    }

    let mut fields: Vec<Value> = out.get("fields").and_then(Value::as_array).map(|a| a.iter().filter(|f| f.is_object()).cloned().collect()).unwrap_or_default();
    if fields.len() > LIMIT_FIELDS {
        let omitted = fields.len() - (LIMIT_FIELDS - 1);
        fields.truncate(LIMIT_FIELDS - 1);
        fields.push(json!({ "name": "…", "value": format!("and {omitted} more changes") }));
    }
    let fields: Vec<Value> = fields
        .iter()
        .map(|f| {
            let name = clamp_text(&js_string(f.get("name")), LIMIT_FIELD_NAME);
            let value = clamp_text(&js_string(f.get("value")), LIMIT_FIELD_VALUE);
            let mut m = Map::new();
            m.insert("name".into(), json!(if name.is_empty() { "\u{200b}".to_string() } else { name }));
            m.insert("value".into(), json!(if value.is_empty() { "\u{200b}".to_string() } else { value }));
            if f.get("inline").is_some_and(js_truthy) {
                m.insert("inline".into(), json!(true));
            }
            Value::Object(m)
        })
        .collect();
    if fields.is_empty() {
        out.remove("fields");
    } else {
        out.insert("fields".into(), Value::Array(fields));
    }

    let mut v = Value::Object(out);
    while embed_length(&v) > LIMIT_TOTAL && v.get("fields").and_then(Value::as_array).is_some_and(|f| f.len() > 1) {
        if let Some(Value::Array(f)) = v.get_mut("fields") {
            f.pop();
        }
    }
    let desc = v.get("description").and_then(Value::as_str).map(String::from).filter(|d| !d.is_empty());
    if embed_length(&v) > LIMIT_TOTAL
        && let Some(d) = desc
    {
        let over = embed_length(&v) - LIMIT_TOTAL;
        let keep = bot_util::js::utf16_len(&d).saturating_sub(over + 1);
        v["description"] = json!(clamp_text(&d, keep));
    }
    if embed_length(&v) > LIMIT_TOTAL
        && let Some(o) = v.as_object_mut()
    {
        o.remove("fields");
    }
    v
}

fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// An APIEmbed JSON object as a serenity builder (for sending).
pub fn to_create_embed(v: &Value) -> CreateEmbed {
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    let mut e = CreateEmbed::new();
    if let Some(t) = s("title") {
        e = e.title(t);
    }
    if let Some(d) = s("description") {
        e = e.description(d);
    }
    if let Some(u) = s("url") {
        e = e.url(u);
    }
    if let Some(c) = v.get("color").and_then(Value::as_u64) {
        e = e.color(c as u32);
    }
    if let Some(t) = s("timestamp").and_then(|t| serenity::model::Timestamp::parse(t).ok()) {
        e = e.timestamp(t);
    }
    if let Some(f) = v.get("footer").and_then(|f| f.get("text")).and_then(Value::as_str) {
        let mut footer = CreateEmbedFooter::new(f);
        if let Some(i) = v["footer"].get("icon_url").and_then(Value::as_str) {
            footer = footer.icon_url(i);
        }
        e = e.footer(footer);
    }
    if let Some(a) = v.get("author").and_then(|a| a.get("name")).and_then(Value::as_str) {
        let mut author = CreateEmbedAuthor::new(a);
        if let Some(i) = v["author"].get("icon_url").and_then(Value::as_str) {
            author = author.icon_url(i);
        }
        if let Some(u) = v["author"].get("url").and_then(Value::as_str) {
            author = author.url(u);
        }
        e = e.author(author);
    }
    if let Some(u) = v.get("thumbnail").and_then(|t| t.get("url")).and_then(Value::as_str) {
        e = e.thumbnail(u);
    }
    if let Some(u) = v.get("image").and_then(|t| t.get("url")).and_then(Value::as_str) {
        e = e.image(u);
    }
    for f in v.get("fields").and_then(Value::as_array).into_iter().flatten() {
        let name = f.get("name").and_then(Value::as_str).unwrap_or("\u{200b}");
        let value = f.get("value").and_then(Value::as_str).unwrap_or("\u{200b}");
        e = e.field(name, value, f.get("inline").is_some_and(js_truthy));
    }
    e
}

// ── Options (§5.2) ────────────────────────────────────────────────────────────

/// `sendLog`'s 4th argument, normalised (`normalizeLogOpts`): the event key, preloaded settings and at most one file.
#[derive(Debug, Clone, Default)]
pub struct SendLogOpts {
    pub event: Option<String>,
    pub settings: Option<Settings>,
    pub files: Vec<LogFile>,
}

impl SendLogOpts {
    pub fn event(mut self, event: &str) -> Self {
        self.event = Some(event.to_string());
        self
    }
    pub fn settings(mut self, settings: Option<Settings>) -> Self {
        self.settings = settings;
        self
    }
    /// Adds a file (only the first is kept, as the JS `files.slice(0, 1)`).
    pub fn file(mut self, file: LogFile) -> Self {
        if self.files.is_empty() {
            self.files.push(file);
        }
        self
    }
}

// ── Discord I/O ───────────────────────────────────────────────────────────────

/// A Discord REST failure: its JSON error code (when there was one) and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: Option<u64>,
    pub message: String,
}

impl ApiError {
    pub fn new(code: Option<u64>, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    pub fn from_serenity(e: &serenity::Error) -> Self {
        Self { code: bot_core::discord::error_code(e), message: bot_core::discord::error_message(e) }
    }
    /// No connection to Discord (offline).
    pub fn offline() -> Self {
        Self { code: None, message: "Discord is not connected".into() }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// One audit-log entry of a ban or unban.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub target_id: Option<String>,
    pub executor_id: Option<String>,
    pub reason: Option<String>,
    pub created_ms: i64,
}

/// The Discord REST calls of the logging feature.
#[async_trait::async_trait]
pub trait LogApi: Send + Sync {
    /// `guild.channels.fetch(id)`: `Ok(None)` when Discord answered with nothing usable.
    async fn fetch_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<Option<GuildChannel>, ApiError>;
    /// `channel.send({ embeds, files, allowedMentions: { parse: [] } })`.
    async fn send(&self, channel_id: ChannelId, embeds: Vec<Value>, file: Option<LogFile>) -> Result<(), ApiError>;
    /// `guild.fetchAuditLogs({ type: MemberBanAdd | MemberBanRemove, limit: 5 })`.
    async fn ban_audit_entries(&self, guild_id: GuildId, unban: bool) -> Result<Vec<AuditEntry>, ApiError>;
}

/// The production [`LogApi`]: serenity's HTTP client.
pub struct SerenityLogApi {
    pub http: Option<Arc<serenity::all::Http>>,
    /// The app, for `guild.channels.fetch(id)` (cache, then REST; the fetched channel is cached, and a channel of
    /// another guild is refused with `GuildChannelUnowned`).
    pub app: Option<App>,
}

/// `guild.channels.fetch` failures as [`ApiError`] (`GuildChannelUnowned` has no Discord code, as in discord.js).
fn fetch_error(e: bot_core::interaction_helpers::FetchChannelError) -> ApiError {
    use bot_core::interaction_helpers::FetchChannelError as F;
    match e {
        F::Rest(e) => ApiError::from_serenity(&e),
        F::Offline => ApiError::offline(),
        other @ F::Unowned => ApiError::new(None, other.to_string()),
    }
}

#[async_trait::async_trait]
impl LogApi for SerenityLogApi {
    async fn fetch_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<Option<GuildChannel>, ApiError> {
        if let Some(app) = &self.app {
            return bot_core::interaction_helpers::fetch_channel(app, guild_id, channel_id).await.map(Some).map_err(fetch_error);
        }
        let http = self.http.as_ref().ok_or_else(ApiError::offline)?;
        match http.get_channel(channel_id).await {
            Ok(serenity::all::Channel::Guild(c)) if c.guild_id == guild_id => Ok(Some(c)),
            Ok(_) => Err(fetch_error(bot_core::interaction_helpers::FetchChannelError::Unowned)),
            Err(e) => Err(ApiError::from_serenity(&e)),
        }
    }

    async fn send(&self, channel_id: ChannelId, embeds: Vec<Value>, file: Option<LogFile>) -> Result<(), ApiError> {
        let http = self.http.as_ref().ok_or_else(ApiError::offline)?;
        let mut msg = CreateMessage::new().embeds(embeds.iter().map(to_create_embed).collect()).allowed_mentions(CreateAllowedMentions::new());
        if let Some(f) = file {
            msg = msg.add_file(CreateAttachment::bytes(f.bytes, f.name));
        }
        channel_id.send_message(http.as_ref(), msg).await.map(|_| ()).map_err(|e| ApiError::from_serenity(&e))
    }

    async fn ban_audit_entries(&self, guild_id: GuildId, unban: bool) -> Result<Vec<AuditEntry>, ApiError> {
        use serenity::all::audit_log::{Action, MemberAction};
        let http = self.http.as_ref().ok_or_else(ApiError::offline)?;
        let action = if unban { MemberAction::BanRemove } else { MemberAction::BanAdd };
        let logs = guild_id.audit_logs(http.as_ref(), Some(Action::Member(action)), None, None, Some(5)).await.map_err(|e| ApiError::from_serenity(&e))?;
        Ok(logs
            .entries
            .iter()
            .map(|e| AuditEntry {
                target_id: e.target_id.map(|t| t.to_string()),
                executor_id: Some(e.user_id.to_string()),
                reason: e.reason.clone(),
                created_ms: bot_core::adapters::snowflake_ms(e.id.get()) as i64,
            })
            .collect())
    }
}

#[derive(Default)]
struct ApiSlot(RwLock<Option<Arc<dyn LogApi>>>);

/// Replaces the Discord I/O of the logging feature for this app (tests).
pub fn set_log_api(app: &App, api: Arc<dyn LogApi>) {
    *app.ext::<ApiSlot>().0.write().unwrap_or_else(|e| e.into_inner()) = Some(api);
}

/// The app's [`LogApi`]: the one set with [`set_log_api`], else serenity.
pub fn log_api(app: &App) -> Arc<dyn LogApi> {
    if let Some(api) = app.ext::<ApiSlot>().0.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return api;
    }
    Arc::new(SerenityLogApi { http: app.http(), app: Some(app.clone()) })
}

// ── In-process state (§3.4) ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Dead {
    guild_id: String,
    until: i64,
}

#[derive(Debug, Clone)]
struct Item {
    data: Value,
    file: Option<LogFile>,
    chars: usize,
}

struct Queue {
    guild_id: String,
    channel: Option<GuildChannel>,
    items: VecDeque<Item>,
    timer: bool,
    flushing: bool,
    dropped: usize,
}

/// The per-app state of the logging service (the JS module-level maps).
pub struct LoggingState {
    dead: Mutex<IndexMap<String, Dead>>,
    queues: Mutex<HashMap<String, Queue>>,
    flush_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    warn_throttle: Mutex<bot_util::async_util::Throttle>,
    warned_types: Mutex<HashSet<String>>,
    shutdown_registered: AtomicBool,
}

impl Default for LoggingState {
    fn default() -> Self {
        Self {
            dead: Mutex::new(IndexMap::new()),
            queues: Mutex::new(HashMap::new()),
            flush_locks: Mutex::new(HashMap::new()),
            warn_throttle: Mutex::new(bot_util::async_util::create_throttle(10 * 60_000)),
            warned_types: Mutex::new(HashSet::new()),
            shutdown_registered: AtomicBool::new(false),
        }
    }
}

fn state(app: &App) -> Arc<LoggingState> {
    app.ext::<LoggingState>()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn mark_dead(app: &App, guild_id: &str, channel_id: &str, reason: &str, ttl_ms: i64) {
    let st = state(app);
    {
        let mut dead = lock(&st.dead);
        dead.shift_remove(channel_id);
        dead.insert(channel_id.to_string(), Dead { guild_id: guild_id.to_string(), until: app.clock().now_ms() + ttl_ms });
        while dead.len() > DEAD_CHANNEL_MAX {
            dead.shift_remove_index(0);
        }
    }
    if ttl_ms == DEAD_CHANNEL_TTL_MS && lock(&st.warn_throttle).allow(channel_id) {
        bot_util::log_warn!(format!(
            "Log channel {channel_id} in guild {guild_id} is unusable ({reason}); log posts to it are paused for 10 minutes."
        ));
    }
    drop_queued(app, channel_id);
}

fn drop_queued(app: &App, channel_id: &str) {
    let st = state(app);
    let mut queues = lock(&st.queues);
    if let Some(q) = queues.get_mut(channel_id) {
        q.items.clear();
        q.dropped = 0;
        if !q.flushing {
            queues.remove(channel_id);
        }
    }
}

/// `isLogChannelDead(channelId)`: true while the channel is memoised as unusable (expired entries are removed).
pub fn is_log_channel_dead(app: &App, channel_id: &str) -> bool {
    let st = state(app);
    let mut dead = lock(&st.dead);
    let Some(entry) = dead.get(channel_id) else { return false };
    if entry.until > app.clock().now_ms() {
        return true;
    }
    dead.shift_remove(channel_id);
    false
}

/// `forgetDeadChannels(guildId, channelId)`: forgets one channel's dead memo, or every channel of the guild (when
/// `channel_id` is `None`).
pub fn forget_dead_channels(app: &App, guild_id: &str, channel_id: Option<&str>) {
    let st = state(app);
    let mut dead = lock(&st.dead);
    match channel_id {
        Some(c) => {
            dead.shift_remove(c);
        }
        None => dead.retain(|_, e| e.guild_id != guild_id),
    }
}

/// `markLogChannelDeleted(guildId, channelId)`: marks a deleted log channel dead without the warning, and drops its
/// queued items.
pub fn mark_log_channel_deleted(app: &App, guild_id: &str, channel_id: &str) {
    let st = state(app);
    {
        let mut dead = lock(&st.dead);
        dead.shift_remove(channel_id);
        dead.insert(channel_id.to_string(), Dead { guild_id: guild_id.to_string(), until: app.clock().now_ms() + DEAD_CHANNEL_TTL_MS });
        while dead.len() > DEAD_CHANNEL_MAX {
            dead.shift_remove_index(0);
        }
    }
    drop_queued(app, channel_id);
}

/// The result of [`usable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usable {
    pub ok: bool,
    pub missing: Vec<String>,
    pub reason: Option<String>,
}

/// `usable(guild, channel, needs)`: whether the bot may post in the channel. When the bot's own member is not cached
/// the permission check is skipped (a cache miss is not a denial): the send itself reports 50013.
pub fn usable(app: &App, guild_id: GuildId, channel: Option<&GuildChannel>, needs: &[&str]) -> Usable {
    let bot_member = app.discord().and_then(|d| {
        let bot = d.cache.current_user().id;
        d.cache.guild(guild_id).and_then(|g| bot_core::permissions::me(&g, bot).map(|_| ()))
    });
    if bot_member.is_none() {
        return Usable { ok: channel.is_some_and(bot_core::permissions::is_text_based), missing: Vec::new(), reason: None };
    }
    let perms: Option<Permissions> = channel.and_then(|c| {
        let d = app.discord()?;
        let bot = d.cache.current_user().id;
        let g = d.cache.guild(guild_id)?;
        let me = bot_core::permissions::me(&g, bot)?;
        bot_core::permissions::member_permissions_in(&g, c, bot, &me.roles, true)
    });
    let v = bot_core::interaction_helpers::channel_usable_verdict(channel, perms, needs);
    Usable { ok: v.ok, missing: v.missing, reason: v.reason }
}

// ── Queue (§6.1) ──────────────────────────────────────────────────────────────

fn schedule(app: &App, channel_id: &str, q: &mut Queue) {
    if q.timer || q.flushing {
        return;
    }
    q.timer = true;
    let (app, id) = (app.clone(), channel_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(QUEUE_FLUSH_MS)).await;
        flush(&app, &id).await;
    });
}

fn enqueue(app: &App, guild_id: &str, channel: GuildChannel, item: Item) {
    let st = state(app);
    let channel_id = channel.id.to_string();
    let mut queues = lock(&st.queues);
    let q = queues.entry(channel_id.clone()).or_insert_with(|| Queue {
        guild_id: guild_id.to_string(),
        channel: None,
        items: VecDeque::new(),
        timer: false,
        flushing: false,
        dropped: 0,
    });
    q.guild_id = guild_id.to_string();
    q.channel = Some(channel);
    if q.items.len() >= QUEUE_MAX_ITEMS {
        q.items.pop_front();
        q.dropped += 1;
    }
    q.items.push_back(item);
    schedule(app, &channel_id, q);
}

fn take_batch(items: &mut VecDeque<Item>) -> Vec<Item> {
    let Some(first) = items.pop_front() else { return Vec::new() };
    if first.file.is_some() {
        return vec![first]; // an item with an attachment goes alone
    }
    let mut chars = first.chars;
    let mut batch = vec![first];
    while let Some(next) = items.front() {
        if next.file.is_some() || batch.len() >= BATCH_MAX_EMBEDS || chars + next.chars > BATCH_MAX_CHARS {
            break;
        }
        chars += next.chars;
        if let Some(n) = items.pop_front() {
            batch.push(n);
        }
    }
    batch
}

async fn drain(app: &App, channel_id: &str) {
    let api = log_api(app);
    loop {
        let dead = is_log_channel_dead(app, channel_id);
        let step = {
            let st = state(app);
            let mut queues = lock(&st.queues);
            match queues.get_mut(channel_id) {
                None => None,
                Some(q) if q.items.is_empty() => None,
                Some(q) if dead => {
                    q.items.clear();
                    q.dropped = 0;
                    None
                }
                Some(q) => Some((q.guild_id.clone(), q.channel.clone(), std::mem::take(&mut q.dropped))),
            }
        };
        let Some((guild_id, channel, dropped)) = step else { break };

        // Re-resolve through the cache and fall back to the channel captured at enqueue time: a cache miss alone never
        // marks a channel dead.
        let gid = guild_id.parse::<u64>().ok().filter(|v| *v != 0).map(GuildId::new);
        let cid = ChannelId::new(channel_id.parse::<u64>().unwrap_or(1).max(1));
        let resolved = gid.and_then(|g| bot_core::interaction_helpers::cached_channel(app, g, cid)).or(channel);
        let u = match gid {
            Some(g) => usable(app, g, resolved.as_ref(), &BASE_NEEDS),
            None => Usable { ok: resolved.is_some(), missing: Vec::new(), reason: None },
        };
        if !u.ok {
            mark_dead(app, &guild_id, channel_id, if u.missing.is_empty() { "not-text" } else { "no-permission" }, DEAD_CHANNEL_TTL_MS);
            break;
        }

        if dropped > 0 {
            let text = format!(
                "{dropped} log entr{} dropped because events arrived faster than Discord allows posting.",
                if dropped == 1 { "y was" } else { "ies were" }
            );
            let notice = serde_json::to_value(bot_core::embed_builder::warn_embed("Log entries dropped", Some(&text), None)).unwrap_or(Value::Null);
            if let Err(e) = api.send(cid, vec![notice], None).await {
                if e.code.is_some_and(|c| HARD_SEND_CODES.contains(&c)) {
                    mark_dead(app, &guild_id, channel_id, &e.code.map(|c| c.to_string()).unwrap_or_default(), DEAD_CHANNEL_TTL_MS);
                    break;
                }
                bot_util::log_warn!(format!("sendLog: failed to post the dropped-entries notice to {channel_id}: {}", e.message));
            }
        }

        let batch = {
            let st = state(app);
            let mut queues = lock(&st.queues);
            match queues.get_mut(channel_id) {
                Some(q) => take_batch(&mut q.items),
                None => Vec::new(),
            }
        };
        if batch.is_empty() {
            break;
        }
        let n = batch.len();
        let file = batch[0].file.clone();
        let embeds: Vec<Value> = batch.into_iter().map(|i| i.data).collect();
        if let Err(e) = api.send(cid, embeds, file).await {
            if e.code.is_some_and(|c| HARD_SEND_CODES.contains(&c)) {
                mark_dead(app, &guild_id, channel_id, &e.code.map(|c| c.to_string()).unwrap_or_default(), DEAD_CHANNEL_TTL_MS);
                break;
            }
            // Dropped, not retried: a retry could duplicate a message that did arrive.
            bot_util::log_warn!(format!("sendLog: failed to post {n} log embed(s) to {channel_id}: {}", e.message));
        }
    }
}

async fn flush(app: &App, channel_id: &str) {
    let st = state(app);
    let gate = lock(&st.flush_locks).entry(channel_id.to_string()).or_default().clone();
    let _held = gate.lock().await;
    {
        let mut queues = lock(&st.queues);
        let Some(q) = queues.get_mut(channel_id) else { return };
        q.timer = false;
        q.flushing = true;
    }
    drain(app, channel_id).await;
    let mut queues = lock(&st.queues);
    let reschedule = match queues.get_mut(channel_id) {
        Some(q) => {
            q.flushing = false;
            !q.items.is_empty()
        }
        None => false,
    };
    if reschedule {
        if let Some(q) = queues.get_mut(channel_id) {
            schedule(app, channel_id, q); // entries arrived while posting
        }
    } else {
        queues.remove(channel_id);
    }
}

/// `flushLogQueue(channelId)`: flushes the queue of one channel (or of every channel) now. Never fails.
pub async fn flush_log_queue(app: &App, channel_id: Option<&str>) {
    match channel_id {
        Some(c) => flush(app, c).await,
        None => {
            let ids: Vec<String> = lock(&state(app).queues).keys().cloned().collect();
            futures::future::join_all(ids.iter().map(|id| flush(app, id))).await;
        }
    }
}

/// How many log entries are queued for a channel (tests and diagnostics).
pub fn queued_len(app: &App, channel_id: &str) -> usize {
    lock(&state(app).queues).get(channel_id).map(|q| q.items.len()).unwrap_or(0)
}

/// `_resetLoggingStateForTests()`: clears the dead-channel memo, the queues and the warning throttle.
pub fn reset_logging_state(app: &App) {
    let st = state(app);
    lock(&st.dead).clear();
    lock(&st.queues).clear();
    lock(&st.warned_types).clear();
    *lock(&st.warn_throttle) = bot_util::async_util::create_throttle(10 * 60_000);
}

/// Registers the shutdown flush of the log queue once per app (FRAMEWORK §26: order 20, 4000 ms).
pub fn ensure_shutdown_flush(app: &App) {
    let st = state(app);
    if st.shutdown_registered.swap(true, Ordering::SeqCst) {
        return;
    }
    let a = app.clone();
    bot_core::client::on_shutdown(app, 20, "log queue flush", 4000, "Log queue was not flushed before shutdown", move || {
        let a = a.clone();
        async move {
            flush_log_queue(&a, None).await;
            Ok(())
        }
    });
}

// ── Channel health (§5.3) ─────────────────────────────────────────────────────

/// The state of a configured log channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Unset,
    Ok,
    Missing,
    NoPermission,
    NotText,
}

impl HealthState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unset => "unset",
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::NoPermission => "no-permission",
            Self::NotText => "not-text",
        }
    }
}

/// The result of [`get_log_channel_health`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub state: HealthState,
    pub missing: Vec<String>,
    pub text: String,
}

/// `getLogChannelHealth(guild, channelId)`: cache-only, synchronous health of a configured log channel.
pub fn get_log_channel_health(app: &App, guild_id: GuildId, channel_id: Option<&str>) -> Health {
    let Some(id) = channel_id.filter(|c| !c.is_empty()) else { return Health { state: HealthState::Unset, missing: Vec::new(), text: String::new() } };
    let channel = id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|c| bot_core::interaction_helpers::cached_channel(app, guild_id, ChannelId::new(c)));
    let Some(channel) = channel else {
        return Health { state: HealthState::Missing, missing: Vec::new(), text: "channel not found (deleted?)".into() };
    };
    if !bot_core::permissions::is_text_based(&channel) {
        return Health { state: HealthState::NotText, missing: Vec::new(), text: "not a text channel".into() };
    }
    let u = usable(app, guild_id, Some(&channel), &BASE_NEEDS);
    if !u.ok {
        return if u.missing.is_empty() {
            Health { state: HealthState::NotText, missing: Vec::new(), text: "not a text channel".into() }
        } else {
            let text = format!("missing {}", bot_core::permissions::permission_names(&u.missing).join(", "));
            Health { state: HealthState::NoPermission, missing: u.missing, text }
        };
    }
    Health { state: HealthState::Ok, missing: Vec::new(), text: String::new() }
}

// ── Core dispatch (§5.2) ──────────────────────────────────────────────────────

/// `sendLog(guild, logType, embed, opts)` with the type given by its `LOG_TYPES` key: an unknown key is warned
/// about once and ignored.
pub async fn send_log_by_key(app: &App, guild_id: GuildId, log_type: &str, embed: Value, opts: SendLogOpts) {
    match LogType::from_key(log_type) {
        Some(t) => send_log(app, guild_id, t, embed, opts).await,
        None => {
            let st = state(app);
            let first = lock(&st.warned_types).insert(log_type.to_string());
            if first {
                bot_util::log_warn!(format!("sendLog: unknown log type \"{log_type}\""));
            }
        }
    }
}

/// `sendLog(guild, logType, embed, opts)`: posts a log embed (an APIEmbed JSON object, or `serde_json::to_value` of
/// a `CreateEmbed`) to the channel configured for `log_type`. Never fails; resolves once the embed is queued. For
/// every type except MOD it is a no-op when logging is off, the event is disabled, no channel is set or the channel
/// is unusable. MOD ignores the logging toggles.
pub async fn send_log(app: &App, guild_id: GuildId, log_type: LogType, embed: Value, opts: SendLogOpts) {
    let g = guild_id.to_string();
    let settings = match opts.settings {
        Some(s) => Some(s),
        None => app.settings().get_settings_safe(&g).await,
    };
    let Some(settings) = settings else { return };
    if log_type != LogType::Mod {
        if !settings.logging_enabled {
            return;
        }
        if let Some(e) = opts.event.as_deref()
            && !is_log_event_enabled(Some(&settings), e)
        {
            return;
        }
    }
    let Some(channel_id) = log_type.channel_of(&settings).map(String::from) else { return };
    if is_log_channel_dead(app, &channel_id) {
        return;
    }
    let Some(cid) = channel_id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new) else {
        mark_dead(app, &g, &channel_id, "missing", DEAD_CHANNEL_TTL_MS);
        return;
    };

    let channel = match bot_core::interaction_helpers::cached_channel(app, guild_id, cid) {
        Some(c) => c,
        None => {
            // At most one REST call per dead channel per 10 minutes. Only a definite "gone" answer marks the channel
            // dead for long; any other failure skips this entry (a cache miss is not a deletion).
            match log_api(app).fetch_channel(guild_id, cid).await {
                Ok(Some(c)) => c,
                Ok(None) => {
                    mark_dead(app, &g, &channel_id, "missing", DEAD_CHANNEL_TTL_MS);
                    return;
                }
                Err(e) if e.code.is_some_and(|c| HARD_SEND_CODES.contains(&c)) => {
                    mark_dead(app, &g, &channel_id, "missing", DEAD_CHANNEL_TTL_MS);
                    return;
                }
                Err(_) => {
                    mark_dead(app, &g, &channel_id, "fetch-failed", TRANSIENT_FETCH_TTL_MS);
                    return;
                }
            }
        }
    };

    let u = usable(app, guild_id, Some(&channel), &BASE_NEEDS);
    if !u.ok {
        mark_dead(app, &g, &channel_id, if u.missing.is_empty() { "not-text" } else { "no-permission" }, DEAD_CHANNEL_TTL_MS);
        return;
    }

    let has_content = embed.as_object().is_some_and(|o| {
        o.get("title").is_some_and(js_truthy)
            || o.get("description").is_some_and(js_truthy)
            || o.get("fields").and_then(Value::as_array).is_some_and(|f| !f.is_empty())
    });
    if !has_content {
        return;
    }
    let mut data = fit_embed_data(&embed);

    let mut files = opts.files;
    files.truncate(1);
    if !files.is_empty() && !usable(app, guild_id, Some(&channel), &["AttachFiles"]).ok {
        files.clear();
        let len = data.get("fields").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        if len < LIMIT_FIELDS
            && let Some(o) = data.as_object_mut()
        {
            let mut list = o.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
            list.push(json!({ "name": "Attachment omitted", "value": "I need the **Attach Files** permission in this channel to attach it." }));
            o.insert("fields".into(), Value::Array(list));
        }
    }

    let chars = embed_length(&data);
    ensure_shutdown_flush(app);
    enqueue(app, &g, channel, Item { data, file: files.into_iter().next(), chars });
}
