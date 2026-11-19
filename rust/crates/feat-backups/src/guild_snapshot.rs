//! Port of `src/utils/guildSnapshot.js`: the snapshot of a guild's roles, categories, channels and permission
//! overwrites (version 1), and the restore engine that rebuilds what is missing.
//!
//! The restore NEVER deletes anything: it creates roles and channels, edits the ones that still exist (mode `full`
//! only), moves them and upserts one permission overwrite at a time. Every decision is made from the snapshot and the
//! guild's CURRENT state ([`GuildView`]), so a batch that is repeated after a crash only does what is still undone (and
//! adopts what the crashed run created before it could write down its id, but only after the audit log shows the bot
//! made it for this restore).
//!
//! The engine takes a plain [`Env`] and talks to Discord only through [`RestoreApi`], so tests drive it with fakes. Every
//! call goes through `api()`: bounded in number and time, errors recorded in the state, never returned. Like discord.js,
//! the engine applies what a call returned to its own view of the guild at once (a created category is the next channel's
//! parent in the same batch).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use bot_util::text::clamp_text_with;

pub const SNAPSHOT_VERSION: i64 = 1;
pub const RESTORE_MODES: [&str; 2] = ["missing", "full"];
pub const RESTORE_PHASES: [&str; 7] = ["roles", "rolePositions", "categories", "channels", "channelPositions", "overwrites", "finish"];

/// `RESTORE_BUDGET`.
pub mod budget {
    /// Discord calls per batch (about).
    pub const CALLS: usize = 10;
    /// No new item starts after this long (the scheduler's job timeout is 60 s).
    pub const BATCH_MS: u64 = 18_000;
    /// One call's own timeout (an item makes at most two calls).
    pub const CALL_MS: u64 = 12_000;
    /// Batches an item may time out in before it is reported and skipped.
    pub const STALLS: i64 = 5;
    pub const ERRORS_KEPT: usize = 20;
    pub const ERROR_CHARS: usize = 200;
    /// Channels per position call.
    pub const POSITION_CHUNK: usize = 50;
    /// Audit log entries read to confirm that the bot made a role or channel (Discord's maximum).
    pub const AUDIT_LIMIT: u8 = 100;
    /// Entries accepted from a payload or a stored map.
    pub const ID_MAP_MAX: usize = 4000;
    /// Roles / channels read from a stored snapshot.
    pub const LIST_MAX: usize = 1500;
    /// Overwrites per channel read from a stored snapshot.
    pub const OVERWRITES_MAX: usize = 200;
}

/// discord.js `ChannelType` values the snapshot uses.
pub mod channel_kind {
    pub const GUILD_TEXT: i64 = 0;
    pub const GUILD_VOICE: i64 = 2;
    pub const GUILD_CATEGORY: i64 = 4;
    pub const GUILD_ANNOUNCEMENT: i64 = 5;
    pub const ANNOUNCEMENT_THREAD: i64 = 10;
    pub const PUBLIC_THREAD: i64 = 11;
    pub const PRIVATE_THREAD: i64 = 12;
    pub const GUILD_STAGE_VOICE: i64 = 13;
    pub const GUILD_DIRECTORY: i64 = 14;
    pub const GUILD_FORUM: i64 = 15;
    pub const GUILD_MEDIA: i64 = 16;
}
use channel_kind as ck;

/// Channel types a snapshot holds (threads, directory and the like are left out).
pub const CHANNEL_TYPES: [i64; 6] = [ck::GUILD_TEXT, ck::GUILD_VOICE, ck::GUILD_ANNOUNCEMENT, ck::GUILD_STAGE_VOICE, ck::GUILD_FORUM, ck::GUILD_MEDIA];
pub fn is_text_like(t: i64) -> bool {
    matches!(t, ck::GUILD_TEXT | ck::GUILD_ANNOUNCEMENT | ck::GUILD_FORUM | ck::GUILD_MEDIA)
}
pub fn is_voice_like(t: i64) -> bool {
    matches!(t, ck::GUILD_VOICE | ck::GUILD_STAGE_VOICE)
}
pub fn is_forum_like(t: i64) -> bool {
    matches!(t, ck::GUILD_FORUM | ck::GUILD_MEDIA)
}
const ARCHIVE_DURATIONS: [i64; 4] = [60, 1440, 4320, 10080];

/// Permission bits used by the engine (`PermissionFlagsBits`).
pub mod perm {
    pub const ADMINISTRATOR: u128 = 1 << 3;
    pub const MANAGE_CHANNELS: u128 = 1 << 4;
    /// `ManageRoles` (shown as "Manage Permissions" on a channel overwrite).
    pub const MANAGE_ROLES: u128 = 1 << 28;
}

/// `PermissionsBitField.All`: every permission flag discord.js knows.
pub fn all_permission_bits() -> u128 {
    bot_core::permissions::all_bits().bits() as u128
}

/// `SnapshotError`: a stored snapshot that cannot be read; the message is shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotError(pub String);
impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SnapshotError {}

// ── JS value helpers ──────────────────────────────────────────────────────────

/// `Number.isInteger(v) ? v : None` for a JSON value (`5.0` counts, as in JS).
pub fn js_integer(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Some(i);
            }
            let f = n.as_f64()?;
            (f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18).then_some(f as i64)
        }
        _ => None,
    }
}

/// `int(v, min, max, fallback)`.
pub fn js_int(v: Option<&Value>, min: i64, max: i64, fallback: i64) -> i64 {
    match v.and_then(js_integer) {
        Some(i) if i >= min && i <= max => i,
        _ => fallback,
    }
}

fn js_int_opt(v: Option<&Value>, min: i64, max: i64) -> Option<i64> {
    v.and_then(js_integer).filter(|i| *i >= min && *i <= max)
}

/// `String(v)` for a JSON value (`undefined` for a missing one).
pub fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => js_number_string(n),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| if x.is_null() { String::new() } else { js_string(Some(x)) })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

fn js_number_string(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{f:.0}")
    } else {
        f.to_string()
    }
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit())
}

/// `text(v, max)`: a string clamped to `max` UTF-16 units with no ellipsis; anything else is `None`.
fn text(v: Option<&Value>, max: usize) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(clamp_text_with(s, max, "")),
        _ => None,
    }
}

fn text_str(s: &str, max: usize) -> String {
    clamp_text_with(s, max, "")
}

// ── Permission bits ───────────────────────────────────────────────────────────

/// `bits(value)`: a bitfield (string or number) as an integer; anything unreadable is 0.
pub fn bits(value: Option<&Value>) -> u128 {
    match value {
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                return if u <= 9_007_199_254_740_991 { u as u128 } else { 0 };
            }
            if n.as_i64().is_some() {
                return 0; // negative
            }
            match n.as_f64() {
                Some(f) if f.fract() == 0.0 && (0.0..=9_007_199_254_740_991.0).contains(&f) => f as u128,
                _ => 0,
            }
        }
        None | Some(Value::Null) => 0,
        other => bits_str(&js_string(other)),
    }
}

/// `bits(string)`.
pub fn bits_str(s: &str) -> u128 {
    if is_digits(s) { s.parse::<u128>().unwrap_or(0) } else { 0 }
}

/// Every permission bit the bot can hand out: its own (all of them with Administrator).
pub fn bot_bits(me: Option<&MeView>) -> u128 {
    match me {
        None => 0,
        Some(m) if m.permissions & perm::ADMINISTRATOR != 0 => all_permission_bits(),
        Some(m) => m.permissions,
    }
}

/// The result of [`mask_bits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Masked {
    pub value: u128,
    /// Wanted bits that could not be applied.
    pub dropped: u128,
}

/// What to write for a permission field. The bot can only give what it has, so wanted bits it lacks are dropped; bits
/// it cannot control that the target ALREADY has are kept (so nothing is taken away by accident).
pub fn mask_bits(wanted: u128, bot: u128, current: u128) -> Masked {
    Masked { value: (wanted & bot) | (current & !bot), dropped: wanted & !bot & !current }
}

/// Names of the permission bits in `value`, each one on its own (`bitNames`).
pub fn bit_names(value: u128) -> Vec<String> {
    let p = serenity::all::Permissions::from_bits_retain(value as u64);
    bot_core::permissions::permission_names(&bot_core::permissions::to_array(p))
}

// ── The guild as the engine sees it ───────────────────────────────────────────

/// A role as the engine sees it (from the cache, or what a call returned).
#[derive(Debug, Clone, PartialEq)]
pub struct RoleView {
    pub id: String,
    pub name: String,
    /// `colors.primaryColor ?? color`.
    pub color: u32,
    pub hoist: bool,
    pub mentionable: bool,
    pub permissions: u128,
    pub raw_position: i64,
    pub unicode_emoji: Option<String>,
    pub managed: bool,
}

/// `OverwriteType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwKind {
    Role,
    Member,
}
impl OwKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OwKind::Role => "role",
            OwKind::Member => "member",
        }
    }
    /// discord.js `OverwriteType` (0 role, 1 member).
    pub fn code(self) -> u8 {
        match self {
            OwKind::Role => 0,
            OwKind::Member => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverwriteView {
    pub id: String,
    pub kind: OwKind,
    pub allow: u128,
    pub deny: u128,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TagView {
    pub name: String,
    pub moderated: bool,
    pub emoji_id: Option<String>,
    pub emoji_name: Option<String>,
}

/// A channel (or category, or thread) as the engine sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelView {
    pub id: String,
    pub kind: i64,
    pub name: String,
    pub parent_id: Option<String>,
    pub raw_position: i64,
    pub topic: Option<String>,
    pub nsfw: bool,
    pub rate_limit_per_user: Option<i64>,
    pub default_auto_archive_duration: Option<i64>,
    pub bitrate: Option<i64>,
    pub user_limit: Option<i64>,
    pub tags: Vec<TagView>,
    pub overwrites: IndexMap<String, OverwriteView>,
    /// discord.js `GuildChannel#manageable`.
    pub manageable: bool,
    pub is_thread: bool,
}

impl ChannelView {
    pub fn new(id: &str, kind: i64, name: &str) -> Self {
        Self {
            id: id.into(),
            kind,
            name: name.into(),
            parent_id: None,
            raw_position: 0,
            topic: None,
            nsfw: false,
            rate_limit_per_user: Some(0),
            default_auto_archive_duration: None,
            bitrate: None,
            user_limit: Some(0),
            tags: vec![],
            overwrites: IndexMap::new(),
            manageable: true,
            is_thread: false,
        }
    }
}

/// `guild.members.me`.
#[derive(Debug, Clone, PartialEq)]
pub struct MeView {
    pub id: String,
    /// `me.permissions.bitfield` (guild level; everything for the owner).
    pub permissions: u128,
    /// `me.roles.highest.rawPosition`.
    pub highest_position: i64,
}

impl MeView {
    /// `me.permissions.has(flag)` (checkAdmin).
    pub fn has(&self, flag: u128) -> bool {
        self.permissions & perm::ADMINISTRATOR != 0 || self.permissions & flag == flag
    }
}

/// The guild as the engine sees it: roles (with `@everyone`, whose id is the guild id) and channels in cache order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GuildView {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub roles: IndexMap<String, RoleView>,
    pub channels: IndexMap<String, ChannelView>,
    pub features: Vec<String>,
    /// `guild.maximumBitrate`.
    pub maximum_bitrate: Option<i64>,
    pub me: Option<MeView>,
    /// `guild.members.cache` ids.
    pub member_ids: HashSet<String>,
}

impl GuildView {
    pub fn everyone(&self) -> Option<&RoleView> {
        self.roles.get(&self.id)
    }
    pub fn has_role_or_channel(&self, id: &str) -> bool {
        self.roles.contains_key(id) || self.channels.contains_key(id)
    }
}

// ── The snapshot ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct RoleSnap {
    pub id: String,
    pub name: String,
    pub color: i64,
    pub hoist: bool,
    pub mentionable: bool,
    pub permissions: u128,
    pub position: i64,
    pub unicode_emoji: Option<String>,
    pub managed: bool,
}

impl RoleSnap {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "color": self.color, "hoist": self.hoist, "mentionable": self.mentionable,
            "permissions": self.permissions.to_string(), "position": self.position, "unicodeEmoji": self.unicode_emoji, "managed": self.managed,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverwriteSnap {
    pub id: String,
    pub kind: OwKind,
    pub allow: u128,
    pub deny: u128,
}

impl OverwriteSnap {
    pub fn to_json(&self) -> Value {
        json!({ "id": self.id, "type": self.kind.as_str(), "allow": self.allow.to_string(), "deny": self.deny.to_string() })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TagSnap {
    pub name: String,
    pub moderated: bool,
    pub emoji_name: Option<String>,
}

/// A category or channel of the snapshot. Which of the optional fields exist depends on the type (as in the JS).
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelSnap {
    pub id: String,
    pub kind: i64,
    pub name: String,
    pub parent_id: Option<String>,
    pub position: i64,
    pub overwrites: Vec<OverwriteSnap>,
    pub topic: Option<String>,
    pub nsfw: bool,
    pub rate_limit_per_user: i64,
    pub default_auto_archive_duration: Option<i64>,
    pub bitrate: Option<i64>,
    pub user_limit: i64,
    pub tags: Vec<TagSnap>,
}

impl ChannelSnap {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("type".into(), json!(self.kind));
        m.insert("name".into(), json!(self.name));
        m.insert("parentId".into(), json!(self.parent_id));
        m.insert("position".into(), json!(self.position));
        m.insert("overwrites".into(), Value::Array(self.overwrites.iter().map(OverwriteSnap::to_json).collect()));
        if self.kind == ck::GUILD_CATEGORY {
            return Value::Object(m);
        }
        if is_text_like(self.kind) {
            m.insert("topic".into(), json!(self.topic));
            m.insert("nsfw".into(), json!(self.nsfw));
            m.insert("rateLimitPerUser".into(), json!(self.rate_limit_per_user));
            m.insert("defaultAutoArchiveDuration".into(), json!(self.default_auto_archive_duration));
        }
        if is_voice_like(self.kind) {
            m.insert("bitrate".into(), json!(self.bitrate));
            m.insert("userLimit".into(), json!(self.user_limit));
            m.insert("rateLimitPerUser".into(), json!(self.rate_limit_per_user));
        }
        if is_forum_like(self.kind) {
            m.insert(
                "tags".into(),
                Value::Array(self.tags.iter().map(|t| json!({ "name": t.name, "moderated": t.moderated, "emojiName": t.emoji_name })).collect()),
            );
        }
        Value::Object(m)
    }
}

/// A snapshot (as captured, or as `parseSnapshot` returned it).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub version: i64,
    pub taken_at: String,
    pub guild_id: String,
    pub guild_name: String,
    pub everyone_permissions: u128,
    pub roles: Vec<RoleSnap>,
    pub categories: Vec<ChannelSnap>,
    pub channels: Vec<ChannelSnap>,
    pub skipped_channels: i64,
    /// Old id -> the id an earlier restore gave it (only on a parsed snapshot; `data.idMap`).
    pub id_map: IndexMap<String, String>,
}

impl Snapshot {
    /// The stored JSON as `captureGuild` returns it (no `idMap`).
    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "takenAt": self.taken_at,
            "guild": { "id": self.guild_id, "name": self.guild_name },
            "everyone": { "permissions": self.everyone_permissions.to_string() },
            "roles": self.roles.iter().map(RoleSnap::to_json).collect::<Vec<_>>(),
            "categories": self.categories.iter().map(ChannelSnap::to_json).collect::<Vec<_>>(),
            "channels": self.channels.iter().map(ChannelSnap::to_json).collect::<Vec<_>>(),
            "skipped": { "channels": self.skipped_channels },
        })
    }

    /// What `parseSnapshot` returns (with `idMap`).
    pub fn to_parsed_json(&self) -> Value {
        let mut v = self.to_json();
        if let Value::Object(m) = &mut v {
            m.insert("idMap".into(), json!(self.id_map));
        }
        v
    }
}

fn by_position<T>(pos: impl Fn(&T) -> (i64, &str)) -> impl Fn(&T, &T) -> std::cmp::Ordering {
    move |a, b| {
        let (pa, ia) = pos(a);
        let (pb, ib) = pos(b);
        pa.cmp(&pb).then_with(|| ia.encode_utf16().cmp(ib.encode_utf16()))
    }
}

// ── Capture ───────────────────────────────────────────────────────────────────

fn capture_channel(c: &ChannelView) -> ChannelSnap {
    let text_like = is_text_like(c.kind);
    let voice_like = is_voice_like(c.kind);
    let rate = c.rate_limit_per_user.unwrap_or(0);
    ChannelSnap {
        id: c.id.clone(),
        kind: c.kind,
        name: text_str(&c.name, 100),
        parent_id: c.parent_id.clone(),
        position: c.raw_position,
        overwrites: c
            .overwrites
            .values()
            .map(|o| OverwriteSnap { id: o.id.clone(), kind: o.kind, allow: o.allow, deny: o.deny })
            .collect(),
        topic: if text_like { c.topic.as_deref().map(|t| text_str(t, 4096)) } else { None },
        nsfw: text_like && c.nsfw,
        rate_limit_per_user: if text_like || voice_like { rate } else { 0 },
        default_auto_archive_duration: c.default_auto_archive_duration.filter(|d| ARCHIVE_DURATIONS.contains(d)),
        bitrate: if voice_like { c.bitrate } else { None },
        user_limit: if voice_like { c.user_limit.unwrap_or(0) } else { 0 },
        tags: if is_forum_like(c.kind) {
            c.tags
                .iter()
                .take(20)
                .map(|t| TagSnap {
                    name: text_str(&t.name, 20),
                    moderated: t.moderated,
                    emoji_name: if t.emoji_id.is_none() { t.emoji_name.clone() } else { None },
                })
                .collect()
        } else {
            vec![]
        },
    }
}

/// Reads the guild's roles, categories, channels and permission overwrites from its view (no Discord call). Version 1
/// holds no threads, messages, members, emojis, stickers, webhooks or bot settings.
pub fn capture_guild(guild: &GuildView, now: chrono::DateTime<chrono::Utc>) -> Snapshot {
    let mut roles: Vec<RoleSnap> = guild
        .roles
        .values()
        .filter(|r| r.id != guild.id)
        .map(|r| RoleSnap {
            id: r.id.clone(),
            name: text_str(&r.name, 100),
            color: if r.color <= 0xffffff { r.color as i64 } else { 0 },
            hoist: r.hoist,
            mentionable: r.mentionable,
            permissions: r.permissions,
            position: r.raw_position,
            unicode_emoji: r.unicode_emoji.clone(),
            managed: r.managed,
        })
        .collect();
    roles.sort_by(by_position(|r: &RoleSnap| (r.position, r.id.as_str())));

    let mut categories = vec![];
    let mut channels = vec![];
    let mut skipped = 0;
    for c in guild.channels.values() {
        if c.is_thread {
            continue; // the cache holds threads too; version 1 keeps none
        }
        if c.kind == ck::GUILD_CATEGORY {
            categories.push(capture_channel(c));
        } else if CHANNEL_TYPES.contains(&c.kind) {
            channels.push(capture_channel(c));
        } else {
            skipped += 1;
        }
    }
    categories.sort_by(by_position(|c: &ChannelSnap| (c.position, c.id.as_str())));
    channels.sort_by(by_position(|c: &ChannelSnap| (c.position, c.id.as_str())));

    Snapshot {
        version: SNAPSHOT_VERSION,
        taken_at: bot_db::serde_js::format_js_date(&now),
        guild_id: guild.id.clone(),
        guild_name: text_str(&guild.name, 100),
        everyone_permissions: guild.everyone().map(|r| r.permissions).unwrap_or(0),
        roles,
        categories,
        channels,
        skipped_channels: skipped,
        id_map: IndexMap::new(),
    }
}

/// Size of a snapshot as stored, in bytes (`Buffer.byteLength(JSON.stringify(data))`).
pub fn snapshot_bytes(data: &Value) -> usize {
    serde_json::to_string(data).map(|s| s.len()).unwrap_or(0)
}

/// Counts shown in lists and info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SnapshotStats {
    pub roles: usize,
    pub categories: usize,
    pub channels: usize,
    pub overwrites: usize,
}

pub fn snapshot_stats(s: &Snapshot) -> SnapshotStats {
    let sum = |l: &[ChannelSnap]| l.iter().map(|c| c.overwrites.len()).sum::<usize>();
    SnapshotStats { roles: s.roles.len(), categories: s.categories.len(), channels: s.channels.len(), overwrites: sum(&s.categories) + sum(&s.channels) }
}

// ── Parse (a stored snapshot is read defensively) ─────────────────────────────

fn digits_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| is_digits(s))
}

fn parse_overwrite(o: &Value) -> Option<OverwriteSnap> {
    let id = digits_str(o.get("id"))?;
    if !o.is_object() {
        return None;
    }
    let allow = bits(o.get("allow"));
    let deny = bits(o.get("deny")) & !allow;
    let kind = if o.get("type").and_then(Value::as_str) == Some("member") { OwKind::Member } else { OwKind::Role };
    Some(OverwriteSnap { id: id.into(), kind, allow, deny })
}

fn parse_channel(c: &Value, is_category: bool) -> Option<ChannelSnap> {
    if !c.is_object() {
        return None;
    }
    let id = digits_str(c.get("id"))?;
    let raw_type = c.get("type").and_then(js_integer);
    let kind = if is_category { ck::GUILD_CATEGORY } else { raw_type? };
    if is_category {
        if raw_type != Some(ck::GUILD_CATEGORY) {
            return None;
        }
    } else if !CHANNEL_TYPES.contains(&kind) {
        return None;
    }
    let overwrites = match c.get("overwrites") {
        Some(Value::Array(a)) => a.iter().take(budget::OVERWRITES_MAX).filter_map(parse_overwrite).collect(),
        _ => vec![],
    };
    let mut out = ChannelSnap {
        id: id.into(),
        kind,
        name: text(c.get("name"), 100).filter(|s| !s.is_empty()).unwrap_or_else(|| "channel".into()),
        parent_id: digits_str(c.get("parentId")).map(String::from),
        position: js_int(c.get("position"), 0, 100_000, 0),
        overwrites,
        topic: None,
        nsfw: false,
        rate_limit_per_user: 0,
        default_auto_archive_duration: None,
        bitrate: None,
        user_limit: 0,
        tags: vec![],
    };
    if is_text_like(kind) {
        out.topic = text(c.get("topic"), 4096).filter(|s| !s.is_empty());
        out.nsfw = c.get("nsfw") == Some(&Value::Bool(true));
        out.rate_limit_per_user = js_int(c.get("rateLimitPerUser"), 0, 21600, 0);
        out.default_auto_archive_duration = c.get("defaultAutoArchiveDuration").and_then(js_integer).filter(|d| ARCHIVE_DURATIONS.contains(d));
    }
    if is_voice_like(kind) {
        out.bitrate = js_int_opt(c.get("bitrate"), 8000, 10_000_000);
        out.user_limit = js_int(c.get("userLimit"), 0, 99, 0);
        out.rate_limit_per_user = js_int(c.get("rateLimitPerUser"), 0, 21600, 0);
    }
    if is_forum_like(kind)
        && let Some(Value::Array(tags)) = c.get("tags")
    {
        {
            out.tags = tags
                .iter()
                .take(20)
                .filter(|t| matches!(t.get("name"), Some(Value::String(s)) if !s.is_empty()))
                .map(|t| TagSnap {
                    name: text(t.get("name"), 20).unwrap_or_default(),
                    moderated: t.get("moderated") == Some(&Value::Bool(true)),
                    emoji_name: t.get("emojiName").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from),
                })
                .collect();
        }
    }
    Some(out)
}

fn parse_role(r: &Value) -> Option<RoleSnap> {
    if !r.is_object() {
        return None;
    }
    let id = digits_str(r.get("id"))?;
    Some(RoleSnap {
        id: id.into(),
        name: text(r.get("name"), 100).filter(|s| !s.is_empty()).unwrap_or_else(|| "role".into()),
        color: js_int(r.get("color"), 0, 0xffffff, 0),
        hoist: r.get("hoist") == Some(&Value::Bool(true)),
        mentionable: r.get("mentionable") == Some(&Value::Bool(true)),
        permissions: bits(r.get("permissions")),
        position: js_int(r.get("position"), 0, 100_000, 0),
        unicode_emoji: r.get("unicodeEmoji").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from),
        managed: r.get("managed") == Some(&Value::Bool(true)),
    })
}

/// Old role/channel id -> the id the earlier restore gave it; only entries whose new id may still exist are useful.
pub fn parse_id_map(raw: Option<&Value>) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    let Some(Value::Object(m)) = raw else { return out };
    for (k, v) in js_object_entries(m) {
        if out.len() >= budget::ID_MAP_MAX {
            break;
        }
        if let Value::String(v) = v
            && is_digits(k)
            && is_digits(v)
        {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// `Object.entries` order: array-index keys (canonical integers below 2^32 - 1) ascending first, then the rest in
/// insertion order.
fn js_object_entries(m: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let index_of = |k: &str| -> Option<u32> {
        if k.is_empty() || (k.len() > 1 && k.starts_with('0')) || !k.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        k.parse::<u64>().ok().filter(|n| *n < 4_294_967_295).map(|n| n as u32)
    };
    let mut idx: Vec<(u32, (&String, &Value))> = vec![];
    let mut rest = vec![];
    for (k, v) in m {
        match index_of(k) {
            Some(n) => idx.push((n, (k, v))),
            None => rest.push((k, v)),
        }
    }
    idx.sort_by_key(|(n, _)| *n);
    idx.into_iter().map(|(_, e)| e).chain(rest).collect()
}

/// Validates and normalises a stored snapshot (`ServerBackup.data`).
pub fn parse_snapshot(raw: &Value) -> Result<Snapshot, SnapshotError> {
    let Value::Object(m) = raw else { return Err(SnapshotError("The backup data cannot be read.".into())) };
    if m.get("version").and_then(js_integer) != Some(SNAPSHOT_VERSION) {
        return Err(SnapshotError(format!("This backup uses format version {}, which this bot cannot read.", js_string(m.get("version")))));
    }
    let list = |k: &str| -> Vec<Value> {
        match m.get(k) {
            Some(Value::Array(a)) => a.iter().take(budget::LIST_MAX).cloned().collect(),
            _ => vec![],
        }
    };
    let mut roles: Vec<RoleSnap> = list("roles").iter().filter_map(parse_role).collect();
    roles.sort_by(by_position(|r: &RoleSnap| (r.position, r.id.as_str())));
    let mut categories: Vec<ChannelSnap> = list("categories").iter().filter_map(|c| parse_channel(c, true)).collect();
    categories.sort_by(by_position(|c: &ChannelSnap| (c.position, c.id.as_str())));
    let mut channels: Vec<ChannelSnap> = list("channels").iter().filter_map(|c| parse_channel(c, false)).collect();
    channels.sort_by(by_position(|c: &ChannelSnap| (c.position, c.id.as_str())));
    let guild = m.get("guild");
    let guild_id = match guild.and_then(|g| g.get("id")) {
        None | Some(Value::Null) => String::new(),
        other => js_string(other),
    };
    Ok(Snapshot {
        version: SNAPSHOT_VERSION,
        taken_at: m.get("takenAt").and_then(Value::as_str).unwrap_or("").to_string(),
        guild_id,
        guild_name: text(guild.and_then(|g| g.get("name")), 100).unwrap_or_default(),
        everyone_permissions: bits(m.get("everyone").and_then(|e| e.get("permissions"))),
        roles,
        categories,
        channels,
        skipped_channels: js_int(m.get("skipped").and_then(|s| s.get("channels")), 0, 100_000, 0),
        id_map: parse_id_map(m.get("idMap")),
    })
}

/// The snapshot as exported: without the bookkeeping a restore adds.
pub fn exportable(data: &Value) -> Value {
    match data {
        Value::Object(m) => {
            let mut out = m.clone();
            out.shift_remove("idMap");
            Value::Object(out)
        }
        other => other.clone(),
    }
}

// ── Restore state ─────────────────────────────────────────────────────────────

pub const COUNT_KEYS: [&str; 19] = [
    "rolesCreated", "rolesUpdated", "rolesKept", "rolesSkipped",
    "categoriesCreated", "categoriesUpdated", "categoriesKept", "categoriesSkipped",
    "channelsCreated", "channelsUpdated", "channelsKept", "channelsSkipped",
    "overwritesSet", "overwritesKept", "overwritesSkipped", "positionsSet", "positionsLimited", "masked", "errors",
];

/// The payload of a `backup.restore` job (same keys, same order as the JS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreState {
    pub backup_id: String,
    pub mode: String,
    pub phase: String,
    pub cursor: i64,
    pub stalls: i64,
    pub retry: bool,
    pub id_map: IndexMap<String, String>,
    pub made: Vec<String>,
    pub counts: IndexMap<String, i64>,
    pub errors: Vec<String>,
    pub dropped: String,
    pub started_by_id: Option<String>,
    pub channel_id: Option<String>,
    pub started_at: i64,
}

impl RestoreState {
    pub fn count(&self, key: &str) -> i64 {
        self.counts.get(key).copied().unwrap_or(0)
    }
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
    fn bump(&mut self, key: &str, n: i64) {
        *self.counts.entry(key.to_string()).or_insert(0) += n;
    }
    fn note_error(&mut self, line: &str) {
        self.bump("errors", 1);
        if self.errors.len() < budget::ERRORS_KEPT {
            self.errors.push(bot_util::text::clamp_text(line, budget::ERROR_CHARS));
        }
    }
    fn note_dropped(&mut self, dropped: u128) {
        if dropped == 0 {
            return;
        }
        self.bump("masked", 1);
        self.dropped = (bits_str(&self.dropped) | dropped).to_string();
    }
}

/// The payload of a `backup.restore` job at the start.
pub fn new_restore_state(
    backup_id: &str,
    mode: &str,
    started_by_id: Option<&str>,
    channel_id: Option<&str>,
    id_map: &IndexMap<String, String>,
    now_ms: i64,
) -> RestoreState {
    let raw = serde_json::to_value(id_map).unwrap_or(Value::Null);
    RestoreState {
        backup_id: backup_id.into(),
        mode: mode.into(),
        phase: "roles".into(),
        cursor: 0,
        stalls: 0,
        retry: false,
        id_map: parse_id_map(Some(&raw)),
        made: vec![],
        counts: IndexMap::new(),
        errors: vec![],
        dropped: "0".into(),
        started_by_id: started_by_id.map(String::from),
        channel_id: channel_id.map(String::from),
        started_at: now_ms,
    }
}

/// Validates a job payload; `None` when it is unusable (a permanent failure). `now_ms` stands in for a missing start.
pub fn normalise_restore_state(raw: &Value, now_ms: i64) -> Option<RestoreState> {
    let Value::Object(m) = raw else { return None };
    let backup_id = m.get("backupId").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let mode = m.get("mode").and_then(Value::as_str).filter(|s| RESTORE_MODES.contains(s))?;
    let phase = m.get("phase").and_then(Value::as_str).filter(|s| RESTORE_PHASES.contains(s))?;
    let counts_raw = m.get("counts");
    let mut counts = IndexMap::new();
    for k in COUNT_KEYS {
        counts.insert(k.to_string(), js_int(counts_raw.and_then(|c| c.get(k)), 0, 1_000_000_000, 0));
    }
    let made = match m.get("made") {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).filter(|s| is_digits(s)).take(budget::ID_MAP_MAX).map(String::from).collect(),
        _ => vec![],
    };
    let errors = match m.get("errors") {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).take(budget::ERRORS_KEPT).map(String::from).collect(),
        _ => vec![],
    };
    Some(RestoreState {
        backup_id: backup_id.into(),
        mode: mode.into(),
        phase: phase.into(),
        cursor: js_int(m.get("cursor"), 0, 1_000_000_000, 0),
        stalls: js_int(m.get("stalls"), 0, 1000, 0),
        retry: m.get("retry") == Some(&Value::Bool(true)),
        id_map: parse_id_map(m.get("idMap")),
        made,
        counts,
        errors,
        dropped: bits(m.get("dropped")).to_string(),
        started_by_id: m.get("startedById").and_then(Value::as_str).map(String::from),
        channel_id: m.get("channelId").and_then(Value::as_str).map(String::from),
        started_at: js_int(m.get("startedAt"), 0, 9_007_199_254_740_991, now_ms),
    })
}

// ── Discord, behind a trait ───────────────────────────────────────────────────

/// A failed Discord call: its message and code (`err.message`, `err.code`), or a timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub message: String,
    pub code: Option<String>,
    pub timeout: bool,
}

impl ApiError {
    pub fn new(message: impl Into<String>, code: Option<impl ToString>) -> Self {
        Self { message: message.into(), code: code.map(|c| c.to_string()), timeout: false }
    }
    pub fn timeout(label: &str) -> Self {
        Self { message: format!("{label} timed out"), code: None, timeout: true }
    }
}

/// The body of `guild.roles.create` / `roles.edit` (only the fields that are `Some` are sent).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RoleData {
    pub name: Option<String>,
    pub color: Option<u32>,
    pub hoist: Option<bool>,
    pub mentionable: Option<bool>,
    pub permissions: Option<u128>,
    /// `Some(None)` clears the icon.
    pub unicode_emoji: Option<Option<String>>,
}

impl RoleData {
    /// The REST body (`colors.primaryColor`, permissions as a string).
    pub fn to_body(&self) -> Value {
        let mut m = Map::new();
        if let Some(n) = &self.name {
            m.insert("name".into(), json!(n));
        }
        if let Some(c) = self.color {
            m.insert("colors".into(), json!({ "primary_color": c }));
        }
        if let Some(h) = self.hoist {
            m.insert("hoist".into(), json!(h));
        }
        if let Some(x) = self.mentionable {
            m.insert("mentionable".into(), json!(x));
        }
        if let Some(p) = self.permissions {
            m.insert("permissions".into(), json!(p.to_string()));
        }
        if let Some(e) = &self.unicode_emoji {
            m.insert("unicode_emoji".into(), json!(e));
        }
        Value::Object(m)
    }
    fn apply(&self, r: &mut RoleView) {
        if let Some(n) = &self.name {
            r.name = n.clone();
        }
        if let Some(c) = self.color {
            r.color = c;
        }
        if let Some(h) = self.hoist {
            r.hoist = h;
        }
        if let Some(x) = self.mentionable {
            r.mentionable = x;
        }
        if let Some(p) = self.permissions {
            r.permissions = p;
        }
        if let Some(e) = &self.unicode_emoji {
            r.unicode_emoji = e.clone();
        }
    }
}

/// A forum tag to create.
#[derive(Debug, Clone, PartialEq)]
pub struct TagCreate {
    pub name: String,
    pub moderated: bool,
    pub emoji_name: Option<String>,
}

/// The body of `guild.channels.create`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChannelCreate {
    pub name: String,
    pub kind: i64,
    pub parent: Option<String>,
    pub position: i64,
    pub permission_overwrites: Vec<OverwriteView>,
    pub topic: Option<String>,
    pub nsfw: Option<bool>,
    pub rate_limit_per_user: Option<i64>,
    pub default_auto_archive_duration: Option<i64>,
    pub bitrate: Option<i64>,
    pub user_limit: Option<i64>,
    pub available_tags: Vec<TagCreate>,
}

impl ChannelCreate {
    pub fn to_body(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("type".into(), json!(self.kind));
        if let Some(p) = &self.parent {
            m.insert("parent_id".into(), json!(p));
        }
        m.insert("position".into(), json!(self.position));
        if !self.permission_overwrites.is_empty() {
            m.insert(
                "permission_overwrites".into(),
                Value::Array(
                    self.permission_overwrites
                        .iter()
                        .map(|o| json!({ "id": o.id, "type": o.kind.code(), "allow": o.allow.to_string(), "deny": o.deny.to_string() }))
                        .collect(),
                ),
            );
        }
        if let Some(t) = &self.topic {
            m.insert("topic".into(), json!(t));
        }
        if let Some(n) = self.nsfw {
            m.insert("nsfw".into(), json!(n));
        }
        if let Some(r) = self.rate_limit_per_user {
            m.insert("rate_limit_per_user".into(), json!(r));
        }
        if let Some(d) = self.default_auto_archive_duration {
            m.insert("default_auto_archive_duration".into(), json!(d));
        }
        if let Some(b) = self.bitrate {
            m.insert("bitrate".into(), json!(b));
        }
        if let Some(u) = self.user_limit {
            m.insert("user_limit".into(), json!(u));
        }
        if !self.available_tags.is_empty() {
            m.insert(
                "available_tags".into(),
                Value::Array(self.available_tags.iter().map(|t| json!({ "name": t.name, "moderated": t.moderated, "emoji_id": null, "emoji_name": t.emoji_name })).collect()),
            );
        }
        Value::Object(m)
    }
}

/// The body of `guild.channels.edit` (only the `Some` fields).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChannelEdit {
    pub name: Option<String>,
    /// `''` clears the topic.
    pub topic: Option<String>,
    pub nsfw: Option<bool>,
    pub rate_limit_per_user: Option<i64>,
    pub default_auto_archive_duration: Option<i64>,
    pub bitrate: Option<i64>,
    pub user_limit: Option<i64>,
    pub parent: Option<String>,
}

impl ChannelEdit {
    pub fn is_empty(&self) -> bool {
        *self == ChannelEdit::default()
    }
    pub fn to_body(&self) -> Value {
        let mut m = Map::new();
        if let Some(v) = &self.name {
            m.insert("name".into(), json!(v));
        }
        if let Some(v) = &self.topic {
            m.insert("topic".into(), json!(v));
        }
        if let Some(v) = self.nsfw {
            m.insert("nsfw".into(), json!(v));
        }
        if let Some(v) = self.rate_limit_per_user {
            m.insert("rate_limit_per_user".into(), json!(v));
        }
        if let Some(v) = self.default_auto_archive_duration {
            m.insert("default_auto_archive_duration".into(), json!(v));
        }
        if let Some(v) = self.bitrate {
            m.insert("bitrate".into(), json!(v));
        }
        if let Some(v) = self.user_limit {
            m.insert("user_limit".into(), json!(v));
        }
        if let Some(v) = &self.parent {
            m.insert("parent_id".into(), json!(v));
        }
        Value::Object(m)
    }
    fn apply(&self, c: &mut ChannelView) {
        if let Some(v) = &self.name {
            c.name = v.clone();
        }
        if let Some(v) = &self.topic {
            c.topic = if v.is_empty() { None } else { Some(v.clone()) };
        }
        if let Some(v) = self.nsfw {
            c.nsfw = v;
        }
        if let Some(v) = self.rate_limit_per_user {
            c.rate_limit_per_user = Some(v);
        }
        if let Some(v) = self.default_auto_archive_duration {
            c.default_auto_archive_duration = Some(v);
        }
        if let Some(v) = self.bitrate {
            c.bitrate = Some(v);
        }
        if let Some(v) = self.user_limit {
            c.user_limit = Some(v);
        }
        if let Some(v) = &self.parent {
            c.parent_id = Some(v.clone());
        }
    }
}

/// Which audit log event (`AuditLogEvent.RoleCreate` / `ChannelCreate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuditKind {
    RoleCreate,
    ChannelCreate,
}

/// One audit log entry.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    pub target_id: Option<String>,
    pub executor_id: Option<String>,
    pub reason: Option<String>,
    pub created_ms: i64,
}

/// The Discord calls a restore makes. The live implementation is [`crate::live_guild::LiveGuild`]; tests use fakes.
#[async_trait]
pub trait RestoreApi: Send + Sync {
    /// The guild as it is now (the cache).
    fn view(&self) -> GuildView;
    /// `guild.roles.edit(everyone, { permissions })`.
    async fn edit_everyone(&self, permissions: u128, reason: &str) -> Result<(), ApiError>;
    async fn create_role(&self, data: &RoleData, reason: &str) -> Result<RoleView, ApiError>;
    async fn edit_role(&self, role_id: &str, data: &RoleData, reason: &str) -> Result<(), ApiError>;
    /// `guild.roles.setPositions([{ role, position }])`.
    async fn set_role_positions(&self, entries: &[(String, i64)]) -> Result<(), ApiError>;
    async fn create_channel(&self, data: &ChannelCreate, reason: &str) -> Result<ChannelView, ApiError>;
    async fn edit_channel(&self, channel_id: &str, data: &ChannelEdit, reason: &str) -> Result<(), ApiError>;
    /// `guild.channels.setPositions([{ channel, position }])`.
    async fn set_channel_positions(&self, entries: &[(String, i64)]) -> Result<(), ApiError>;
    /// `PUT /channels/{channel}/permissions/{target}`: one overwrite, upserted.
    async fn put_overwrite(&self, channel_id: &str, target: &str, kind: OwKind, allow: u128, deny: u128, reason: &str) -> Result<(), ApiError>;
    /// `guild.members.fetch(id)`: `Ok(true)` when the member is in the server.
    async fn fetch_member(&self, user_id: &str) -> Result<bool, ApiError>;
    /// `guild.fetchAuditLogs({ type, limit })`.
    async fn audit_log(&self, kind: AuditKind, limit: u8) -> Result<Vec<AuditEntry>, ApiError>;
}

// ── The environment of one batch (or of a preview) ────────────────────────────

/// One batch (or a preview): the guild, the snapshot, the state and the budget.
pub struct Env<'a> {
    pub guild: GuildView,
    pub snapshot: &'a Snapshot,
    pub state: RestoreState,
    pub reason: String,
    pub mode: String,
    pub me: Option<MeView>,
    pub bot_b: u128,
    pub bot_top: i64,
    pub can_manage_roles: bool,
    pub can_manage_channels: bool,
    pub role_icons: bool,
    pub made_set: HashSet<String>,
    audit: std::collections::HashMap<AuditKind, HashSet<String>>,
    pub original_role_ids: HashSet<String>,
    pub original_channel_ids: HashSet<String>,
    /// Calls made in this batch.
    pub calls: usize,
    /// At most this many calls (`budget.max`; tests lower it).
    pub max_calls: usize,
    pub deadline: Instant,
    pub timed_out: bool,
    api: Option<&'a dyn RestoreApi>,
}

/// `makeEnv({ guild, snapshot, state, me, reason })`.
pub fn make_env<'a>(guild: GuildView, snapshot: &'a Snapshot, state: RestoreState, reason: &str, api: Option<&'a dyn RestoreApi>) -> Env<'a> {
    let me = guild.me.clone();
    Env {
        bot_b: bot_bits(me.as_ref()),
        bot_top: me.as_ref().map(|m| m.highest_position).unwrap_or(0),
        can_manage_roles: me.as_ref().is_some_and(|m| m.has(perm::MANAGE_ROLES)),
        can_manage_channels: me.as_ref().is_some_and(|m| m.has(perm::MANAGE_CHANNELS)),
        role_icons: guild.features.iter().any(|f| f == "ROLE_ICONS"),
        made_set: state.made.iter().cloned().collect(),
        audit: Default::default(),
        original_role_ids: snapshot.roles.iter().map(|r| r.id.clone()).collect(),
        original_channel_ids: snapshot.categories.iter().chain(&snapshot.channels).map(|c| c.id.clone()).collect(),
        calls: 0,
        max_calls: budget::CALLS,
        deadline: Instant::now() + Duration::from_millis(budget::BATCH_MS),
        timed_out: false,
        mode: state.mode.clone(),
        me,
        guild,
        snapshot,
        state,
        reason: reason.into(),
        api,
    }
}

impl Env<'_> {
    fn has_room(&self) -> bool {
        self.calls < self.max_calls && Instant::now() < self.deadline
    }

    fn find_role(&self, old_id: &str) -> Option<&RoleView> {
        self.guild.roles.get(old_id).or_else(|| self.state.id_map.get(old_id).and_then(|m| self.guild.roles.get(m)))
    }

    fn find_channel(&self, old_id: &str) -> Option<&ChannelView> {
        self.guild.channels.get(old_id).or_else(|| self.state.id_map.get(old_id).and_then(|m| self.guild.channels.get(m)))
    }

    fn mapped_parent(&self, c: &ChannelSnap) -> Option<String> {
        c.parent_id.as_deref().and_then(|p| self.find_channel(p)).map(|ch| ch.id.clone())
    }

    fn is_admin_bot(&self) -> bool {
        self.me.as_ref().is_some_and(|m| m.has(perm::ADMINISTRATOR))
    }
}

fn describe_error(err: &ApiError) -> String {
    let code = match &err.code {
        Some(c) if !c.is_empty() => format!(" ({c})"),
        _ => String::new(),
    };
    format!("{}{code}", bot_util::text::clamp_text(&err.message, 120))
}

/// The outcome of `api()`.
struct Call<T> {
    value: Option<T>,
    timeout: bool,
}

/// One call's own timeout (`Math.max(2000, Math.min(CALL_MS, left + 5000))`), where `left_ms` is the time to the
/// batch deadline and may be negative.
pub fn call_timeout_ms(left_ms: i64) -> u64 {
    left_ms.saturating_add(5000).clamp(2000, budget::CALL_MS as i64) as u64
}

/// One bounded Discord call. Never fails: a failure is recorded in the state (unless `quiet`).
async fn api<'a, T, F>(env: &mut Env<'a>, label: &str, quiet: bool, f: F) -> Call<T>
where
    F: FnOnce(&'a dyn RestoreApi) -> futures::future::BoxFuture<'a, Result<T, ApiError>>,
{
    env.calls += 1;
    let Some(api) = env.api else {
        if !quiet {
            env.state.note_error(&format!("{label}: Discord is not connected"));
        }
        return Call { value: None, timeout: false };
    };
    let now = Instant::now();
    // `env.budget.deadline - Date.now()`: negative once the batch is past its deadline (an item's second call).
    let left = if env.deadline >= now {
        env.deadline.duration_since(now).as_millis() as i64
    } else {
        -(now.duration_since(env.deadline).as_millis() as i64)
    };
    let ms = call_timeout_ms(left);
    let res = match tokio::time::timeout(Duration::from_millis(ms), f(api)).await {
        Ok(r) => r,
        Err(_) => Err(ApiError::timeout(label)),
    };
    match res {
        Ok(v) => Call { value: Some(v), timeout: false },
        Err(e) if e.timeout => {
            // The call may still go through later. The item is tried again in the next batch (see stalled) and may adopt
            // what appeared.
            env.timed_out = true;
            Call { value: None, timeout: true }
        }
        Err(e) => {
            if !quiet {
                env.state.note_error(&format!("{label}: {}", describe_error(&e)));
            }
            Call { value: None, timeout: false }
        }
    }
}

/// After an item: true when a call timed out and the batch should end with the cursor where it is.
fn stalled(env: &mut Env<'_>) -> bool {
    if !env.timed_out {
        env.state.stalls = 0;
        return false;
    }
    env.timed_out = false;
    env.state.stalls += 1;
    if env.state.stalls <= budget::STALLS {
        return true;
    }
    env.state.stalls = 0;
    env.state.note_error("Discord did not answer in time, so this item was skipped.");
    false
}

// ── Decisions (shared by the preview and the job) ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Create,
    Update,
    Keep,
    Skip,
    Set,
    Untouched,
}

/// What to do with one snapshot role.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleDecision {
    pub action: Action,
    pub existing: Option<String>,
    pub data: Option<RoleData>,
    pub dropped: u128,
    pub why: Option<&'static str>,
}

fn role_create_data(env: &Env<'_>, r: &RoleSnap) -> (RoleData, u128) {
    let m = mask_bits(r.permissions, env.bot_b, 0);
    let data = RoleData {
        name: Some(r.name.clone()),
        color: Some(r.color as u32),
        hoist: Some(r.hoist),
        mentionable: Some(r.mentionable),
        permissions: Some(m.value),
        unicode_emoji: match &r.unicode_emoji {
            Some(e) if env.role_icons => Some(Some(e.clone())),
            _ => None,
        },
    };
    (data, m.dropped)
}

fn role_changes(env: &Env<'_>, existing: &RoleView, r: &RoleSnap) -> (Option<RoleData>, u128) {
    let mut d = RoleData::default();
    if existing.name != r.name {
        d.name = Some(r.name.clone());
    }
    if existing.color as i64 != r.color {
        d.color = Some(r.color as u32);
    }
    if existing.hoist != r.hoist {
        d.hoist = Some(r.hoist);
    }
    if existing.mentionable != r.mentionable {
        d.mentionable = Some(r.mentionable);
    }
    let m = mask_bits(r.permissions, env.bot_b, existing.permissions);
    if m.value != existing.permissions {
        d.permissions = Some(m.value);
    }
    if env.role_icons && existing.unicode_emoji != r.unicode_emoji {
        d.unicode_emoji = Some(r.unicode_emoji.clone());
    }
    (if d == RoleData::default() { None } else { Some(d) }, m.dropped)
}

/// What to do with one snapshot role: create, update (mode full), keep or skip.
pub fn decide_role(env: &Env<'_>, r: &RoleSnap) -> RoleDecision {
    let existing = env.find_role(&r.id);
    let ex_id = existing.map(|e| e.id.clone());
    let d = |action, data, dropped, why| RoleDecision { action, existing: ex_id.clone(), data, dropped, why };
    if r.managed {
        // An integration's role cannot be made.
        return d(if existing.is_some() { Action::Keep } else { Action::Skip }, None, 0, Some("managed"));
    }
    let Some(existing) = existing else {
        let (data, dropped) = role_create_data(env, r);
        return d(Action::Create, Some(data), dropped, None);
    };
    if env.mode != "full" {
        return d(Action::Keep, None, 0, None);
    }
    if existing.managed || existing.raw_position >= env.bot_top {
        return d(Action::Skip, None, 0, Some("above me"));
    }
    match role_changes(env, existing, r) {
        (Some(data), dropped) => d(Action::Update, Some(data), dropped, None),
        (None, dropped) => d(Action::Keep, None, dropped, None),
    }
}

/// The overwrites a recreated channel is CREATED with, so a private channel is never public (and never locks the bot
/// out): @everyone, roles that exist now, and the bot's own member overwrite. Masked like every other write; only an
/// administrator bot may put Manage Permissions in a new channel's overwrites.
fn create_overwrites(env: &Env<'_>, c: &ChannelSnap) -> Vec<OverwriteView> {
    if !env.can_manage_roles {
        return vec![];
    }
    let keep = if env.is_admin_bot() { !0u128 } else { !perm::MANAGE_ROLES };
    let mut out = vec![];
    for ow in &c.overwrites {
        let target = match ow.kind {
            OwKind::Role if ow.id == env.guild.id => Some(ow.id.clone()),
            OwKind::Role => env.find_role(&ow.id).map(|r| r.id.clone()),
            OwKind::Member => env.me.as_ref().filter(|m| m.id == ow.id).map(|_| ow.id.clone()),
        };
        let Some(target) = target else { continue };
        let allow = mask_bits(ow.allow, env.bot_b, 0).value & keep;
        let deny = mask_bits(ow.deny, env.bot_b, 0).value & keep;
        if allow == 0 && deny == 0 {
            continue;
        }
        out.push(OverwriteView { id: target, kind: ow.kind, allow, deny });
        if out.len() >= 100 {
            break;
        }
    }
    out
}

fn clamp_bitrate(env: &Env<'_>, b: i64) -> i64 {
    b.min(env.guild.maximum_bitrate.unwrap_or(b)).max(8000)
}

fn channel_create_data(env: &Env<'_>, c: &ChannelSnap) -> ChannelCreate {
    let mut data = ChannelCreate { name: c.name.clone(), kind: c.kind, parent: env.mapped_parent(c), position: c.position, ..Default::default() };
    data.permission_overwrites = create_overwrites(env, c);
    if is_text_like(c.kind) {
        data.topic = c.topic.clone().filter(|t| !t.is_empty());
        if c.nsfw {
            data.nsfw = Some(true);
        }
        if c.rate_limit_per_user != 0 {
            data.rate_limit_per_user = Some(c.rate_limit_per_user);
        }
        data.default_auto_archive_duration = c.default_auto_archive_duration.filter(|d| *d != 0);
    }
    if is_voice_like(c.kind) {
        data.bitrate = c.bitrate.filter(|b| *b != 0).map(|b| clamp_bitrate(env, b));
        if c.user_limit != 0 {
            data.user_limit = Some(c.user_limit);
        }
        if c.rate_limit_per_user != 0 {
            data.rate_limit_per_user = Some(c.rate_limit_per_user);
        }
    }
    if is_forum_like(c.kind) {
        data.available_tags = c.tags.iter().map(|t| TagCreate { name: t.name.clone(), moderated: t.moderated, emoji_name: t.emoji_name.clone() }).collect();
    }
    data
}

fn channel_changes(env: &Env<'_>, existing: &ChannelView, c: &ChannelSnap) -> Option<ChannelEdit> {
    let mut d = ChannelEdit::default();
    if existing.name != c.name {
        d.name = Some(c.name.clone());
    }
    if is_text_like(c.kind) {
        if existing.topic != c.topic {
            d.topic = Some(c.topic.clone().unwrap_or_default());
        }
        if existing.nsfw != c.nsfw {
            d.nsfw = Some(c.nsfw);
        }
        if existing.rate_limit_per_user.unwrap_or(0) != c.rate_limit_per_user {
            d.rate_limit_per_user = Some(c.rate_limit_per_user);
        }
        if let Some(a) = c.default_auto_archive_duration
            && a != 0
            && existing.default_auto_archive_duration != Some(a)
        {
            d.default_auto_archive_duration = Some(a);
        }
    }
    if is_voice_like(c.kind) {
        let bitrate = c.bitrate.filter(|b| *b != 0).map(|b| clamp_bitrate(env, b));
        if let Some(b) = bitrate
            && existing.bitrate != Some(b)
        {
            d.bitrate = Some(b);
        }
        if existing.user_limit.unwrap_or(0) != c.user_limit {
            d.user_limit = Some(c.user_limit);
        }
    }
    if let Some(parent) = env.mapped_parent(c)
        && existing.parent_id.as_deref() != Some(parent.as_str())
    {
        d.parent = Some(parent);
    }
    if d.is_empty() { None } else { Some(d) }
}

/// What to do with one snapshot category or channel.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelDecision {
    pub action: Action,
    pub existing: Option<String>,
    pub create: Option<ChannelCreate>,
    pub edit: Option<ChannelEdit>,
}

/// What to do with one snapshot category or channel: create, update (mode full), keep or skip.
pub fn decide_channel(env: &Env<'_>, c: &ChannelSnap) -> ChannelDecision {
    let Some(existing) = env.find_channel(&c.id) else {
        return ChannelDecision { action: Action::Create, existing: None, create: Some(channel_create_data(env, c)), edit: None };
    };
    let ex = Some(existing.id.clone());
    if env.mode != "full" {
        return ChannelDecision { action: Action::Keep, existing: ex, create: None, edit: None };
    }
    if !existing.manageable {
        return ChannelDecision { action: Action::Skip, existing: ex, create: None, edit: None };
    }
    match channel_changes(env, existing, c) {
        Some(edit) => ChannelDecision { action: Action::Update, existing: ex, create: None, edit: Some(edit) },
        None => ChannelDecision { action: Action::Keep, existing: ex, create: None, edit: None },
    }
}

/// A role overwrite whose role was RECREATED: the original id is gone and the id map names its new role, which exists.
fn is_recreated_role(env: &Env<'_>, ow: &OverwriteSnap) -> bool {
    if ow.kind != OwKind::Role || ow.id == env.guild.id || env.guild.roles.contains_key(&ow.id) {
        return false;
    }
    env.state.id_map.get(&ow.id).is_some_and(|m| env.guild.roles.contains_key(m))
}

/// One permission overwrite of the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct OverwriteItem {
    pub channel_id: String,
    pub ow: OverwriteSnap,
}

/// What to do with one overwrite.
#[derive(Debug, Clone, PartialEq)]
pub struct OverwriteDecision {
    pub action: Action,
    pub channel_id: Option<String>,
    pub channel_name: String,
    pub target: Option<String>,
    pub kind: OwKind,
    pub allow: u128,
    pub deny: u128,
    pub dropped: u128,
}

/// What to do with one permission overwrite of the snapshot.
pub fn decide_overwrite(env: &Env<'_>, item: &OverwriteItem) -> OverwriteDecision {
    let ow = &item.ow;
    let mut d = OverwriteDecision { action: Action::Skip, channel_id: None, channel_name: String::new(), target: None, kind: ow.kind, allow: 0, deny: 0, dropped: 0 };
    let Some(channel) = env.find_channel(&item.channel_id) else { return d };
    // Mode missing leaves the channels that still exist as they are, except that a role it RECREATED gets its overwrite
    // back there: added only where the channel has none for that role, never changing or removing an existing one.
    let additive = env.mode != "full" && !env.made_set.contains(&channel.id);
    if additive && !is_recreated_role(env, ow) {
        d.action = Action::Untouched;
        return d;
    }
    let target = match ow.kind {
        OwKind::Role if ow.id == env.guild.id => Some(ow.id.clone()),
        OwKind::Role => env.find_role(&ow.id).map(|r| r.id.clone()),
        OwKind::Member => Some(ow.id.clone()),
    };
    let Some(target) = target else { return d };
    let current = channel.overwrites.get(&target);
    if additive && current.is_some() {
        d.action = Action::Untouched;
        return d;
    }
    let cur_allow = current.map(|c| c.allow).unwrap_or(0);
    let cur_deny = current.map(|c| c.deny).unwrap_or(0);
    let allow = mask_bits(ow.allow, env.bot_b, cur_allow);
    let deny = mask_bits(ow.deny, env.bot_b, cur_deny);
    d.dropped = allow.dropped | deny.dropped;
    let same = if current.is_some() { allow.value == cur_allow && deny.value == cur_deny } else { allow.value == 0 && deny.value == 0 };
    if same {
        d.action = Action::Keep;
        return d;
    }
    d.action = Action::Set;
    d.channel_id = Some(channel.id.clone());
    d.channel_name = channel.name.clone();
    d.target = Some(target);
    d.allow = allow.value;
    d.deny = deny.value;
    d
}

// ── Work lists (from the snapshot only, so a cursor means the same thing in every run) ──

pub enum RoleItem<'s> {
    Everyone,
    Role(&'s RoleSnap),
}

pub fn role_items<'s>(mode: &str, snapshot: &'s Snapshot) -> Vec<RoleItem<'s>> {
    let mut items = vec![];
    if mode == "full" {
        items.push(RoleItem::Everyone);
    }
    items.extend(snapshot.roles.iter().map(RoleItem::Role));
    items
}

pub fn overwrite_items(snapshot: &Snapshot) -> Vec<OverwriteItem> {
    snapshot
        .categories
        .iter()
        .chain(&snapshot.channels)
        .flat_map(|c| c.overwrites.iter().map(move |ow| OverwriteItem { channel_id: c.id.clone(), ow: ow.clone() }))
        .collect()
}

// ── Preview ───────────────────────────────────────────────────────────────────

/// Counts of one kind in a preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct PreviewCounts {
    pub create: i64,
    pub update: i64,
    pub keep: i64,
    pub skip: i64,
}

impl PreviewCounts {
    fn add(&mut self, a: Action) {
        match a {
            Action::Create => self.create += 1,
            Action::Update => self.update += 1,
            Action::Keep => self.keep += 1,
            _ => self.skip += 1,
        }
    }
}

/// What a restore would do right now (`previewRestore`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub mode: String,
    pub roles: PreviewCounts,
    pub categories: PreviewCounts,
    pub channels: PreviewCounts,
    pub overwrites: i64,
    pub masked: i64,
    #[serde(serialize_with = "ser_u128_str")]
    pub dropped: u128,
    pub unsupported: i64,
    pub managed_missing: i64,
    pub can_manage_roles: bool,
    pub can_manage_channels: bool,
    pub role_total_after: i64,
    pub channel_total_after: i64,
    pub calls: i64,
}

fn ser_u128_str<S: serde::Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

/// What a restore would do right now, counted with the same decisions the job uses. No Discord call.
pub fn preview_restore(guild: &GuildView, snapshot: &Snapshot, mode: &str, id_map: &IndexMap<String, String>) -> Preview {
    let mut state = new_restore_state("", mode, None, None, id_map, 0);
    state.counts = IndexMap::new();
    let env = make_env(guild.clone(), snapshot, state, "Server backup restore", None);
    let mut out = Preview {
        mode: mode.into(),
        roles: Default::default(),
        categories: Default::default(),
        channels: Default::default(),
        overwrites: 0,
        masked: 0,
        dropped: 0,
        unsupported: snapshot.skipped_channels,
        managed_missing: 0,
        can_manage_roles: env.can_manage_roles,
        can_manage_channels: env.can_manage_channels,
        role_total_after: guild.roles.len() as i64,
        channel_total_after: guild.channels.len() as i64,
        calls: 0,
    };
    let mut will_have_role: HashSet<String> = HashSet::new();
    let mut created_roles: HashSet<String> = HashSet::new();
    let note_drop = |out: &mut Preview, d: u128| {
        if d != 0 {
            out.masked += 1;
            out.dropped |= d;
        }
    };

    for item in role_items(mode, snapshot) {
        match item {
            RoleItem::Everyone => {
                let cur = guild.everyone().map(|r| r.permissions).unwrap_or(0);
                let m = mask_bits(snapshot.everyone_permissions, env.bot_b, cur);
                if m.value != cur {
                    out.roles.update += 1;
                    out.calls += 1;
                }
                note_drop(&mut out, m.dropped);
            }
            RoleItem::Role(r) => {
                let d = decide_role(&env, r);
                out.roles.add(d.action);
                if d.action == Action::Skip && d.why == Some("managed") {
                    out.managed_missing += 1;
                }
                if d.action == Action::Create {
                    out.calls += 1;
                    will_have_role.insert(r.id.clone());
                    created_roles.insert(r.id.clone());
                    note_drop(&mut out, d.dropped);
                }
                if d.action == Action::Update {
                    out.calls += 1;
                    note_drop(&mut out, d.dropped);
                }
                if d.existing.is_some() {
                    will_have_role.insert(r.id.clone());
                }
            }
        }
    }
    out.role_total_after += out.roles.create;
    if out.roles.create > 0 || (mode == "full" && out.roles.update > 0) {
        out.calls += 1; // the position call
    }

    let mut will_have_channel: HashSet<String> = HashSet::new();
    let mut created: HashSet<String> = HashSet::new();
    for (is_cat, list) in [(true, &snapshot.categories), (false, &snapshot.channels)] {
        for c in list {
            let d = decide_channel(&env, c);
            if is_cat { out.categories.add(d.action) } else { out.channels.add(d.action) }
            if d.action == Action::Create {
                out.calls += 1;
                created.insert(c.id.clone());
            }
            if d.action == Action::Update {
                out.calls += 1;
            }
            if d.existing.is_some() || d.action == Action::Create {
                will_have_channel.insert(c.id.clone());
            }
        }
    }
    out.channel_total_after += out.categories.create + out.channels.create;
    if mode == "full" {
        out.calls += (snapshot.categories.len() + snapshot.channels.len()).div_ceil(budget::POSITION_CHUNK) as i64;
    }

    for item in overwrite_items(snapshot) {
        if !will_have_channel.contains(&item.channel_id) {
            continue;
        }
        let ow = &item.ow;
        let role_target = ow.kind == OwKind::Role && ow.id != guild.id;
        let whole = |out: &mut Preview| {
            let a = mask_bits(ow.allow, env.bot_b, 0);
            let d = mask_bits(ow.deny, env.bot_b, 0);
            note_drop(out, a.dropped | d.dropped);
            if a.value != 0 || d.value != 0 {
                out.overwrites += 1;
                out.calls += 1;
            }
        };
        if mode != "full" && !created.contains(&item.channel_id) {
            // A channel that still exists only gets the overwrites of RECREATED roles, where it has none for that role.
            if created_roles.contains(&ow.id) {
                whole(&mut out);
            } else if role_target {
                let d = decide_overwrite(&env, &item); // a role an earlier restore recreated (id map): additive
                if d.action == Action::Set {
                    out.overwrites += 1;
                    out.calls += 1;
                    note_drop(&mut out, d.dropped);
                }
            }
            continue;
        }
        if role_target && !will_have_role.contains(&ow.id) && !guild.roles.contains_key(&ow.id) {
            continue; // the role is gone for good
        }
        let exists_now = !role_target || env.find_role(&ow.id).is_some();
        if created.contains(&item.channel_id) || !exists_now {
            // A new channel, or a role that will be made first: nothing to compare with, the whole overwrite is written.
            whole(&mut out);
            continue;
        }
        let d = decide_overwrite(&env, &item);
        if d.action == Action::Set {
            out.overwrites += 1;
            out.calls += 1;
            note_drop(&mut out, d.dropped);
        }
    }
    out
}

// ── Adopting what a crashed run made ──────────────────────────────────────────
// A role or channel that an earlier try of THIS restore made, but whose id was never saved, is used instead of being
// made twice. Never by name alone: it takes an earlier try (mayAdopt), an audit log entry that shows the bot made it for
// this restore, and then the name (and type and parent) of the snapshot item.

fn may_adopt(env: &Env<'_>) -> bool {
    env.state.stalls > 0 || env.state.retry
}

async fn made_by_restore(env: &mut Env<'_>, kind: AuditKind) -> HashSet<String> {
    if let Some(known) = env.audit.get(&kind) {
        return known.clone();
    }
    env.audit.insert(kind, HashSet::new()); // a failed or timed-out read is not repeated inside the batch
    let Some(me_id) = env.me.as_ref().map(|m| m.id.clone()) else { return HashSet::new() };
    let res = api(env, "Audit log", true, move |a| Box::pin(a.audit_log(kind, budget::AUDIT_LIMIT))).await;
    let Some(entries) = res.value else { return HashSet::new() };
    let mut ids = HashSet::new();
    for e in entries {
        if let Some(t) = e.target_id
            && e.executor_id.as_deref() == Some(me_id.as_str())
            && e.reason.as_deref() == Some(env.reason.as_str())
            && e.created_ms >= env.state.started_at
        {
            ids.insert(t);
        }
    }
    env.audit.insert(kind, ids.clone());
    ids
}

async fn adopt_role(env: &mut Env<'_>, r: &RoleSnap) -> Option<RoleView> {
    if !may_adopt(env) {
        return None;
    }
    let taken: HashSet<&String> = env.state.id_map.values().collect();
    let named: Vec<RoleView> = env
        .guild
        .roles
        .values()
        .filter(|role| role.id != env.guild.id && !role.managed && !taken.contains(&role.id) && !env.original_role_ids.contains(&role.id) && role.name == r.name)
        .cloned()
        .collect();
    if named.is_empty() {
        return None;
    }
    let made = made_by_restore(env, AuditKind::RoleCreate).await;
    named.into_iter().find(|role| made.contains(&role.id))
}

async fn adopt_channel(env: &mut Env<'_>, c: &ChannelSnap) -> Option<ChannelView> {
    if !may_adopt(env) {
        return None;
    }
    let taken: HashSet<&String> = env.state.id_map.values().collect();
    let parent = env.mapped_parent(c);
    let named: Vec<ChannelView> = env
        .guild
        .channels
        .values()
        .filter(|ch| {
            !taken.contains(&ch.id)
                && !env.original_channel_ids.contains(&ch.id)
                && !ch.is_thread
                && ch.kind == c.kind
                && ch.name == c.name
                && ch.parent_id == parent
        })
        .cloned()
        .collect();
    if named.is_empty() {
        return None;
    }
    let made = made_by_restore(env, AuditKind::ChannelCreate).await;
    named.into_iter().find(|ch| made.contains(&ch.id))
}

fn remember(env: &mut Env<'_>, old_id: &str, new_id: &str) {
    env.state.id_map.insert(old_id.to_string(), new_id.to_string());
    if !env.state.made.iter().any(|m| m == new_id) {
        env.state.made.push(new_id.to_string());
    }
    env.made_set.insert(new_id.to_string());
}

// ── Phases ────────────────────────────────────────────────────────────────────

async fn step_roles(env: &mut Env<'_>) -> bool {
    let snapshot = env.snapshot;
    let items = role_items(&env.mode, snapshot);
    if !env.can_manage_roles {
        if (env.state.cursor as usize) < items.len() {
            let n = items.len() as i64 - env.state.cursor;
            env.state.bump("rolesSkipped", n);
            env.state.note_error("I need the Manage Roles permission, so no roles were restored.");
        }
        env.state.cursor = items.len() as i64;
        return true;
    }
    while (env.state.cursor as usize) < items.len() {
        if !env.has_room() {
            return false;
        }
        match &items[env.state.cursor as usize] {
            RoleItem::Everyone => {
                if let Some(everyone) = env.guild.everyone() {
                    let cur = everyone.permissions;
                    let m = mask_bits(snapshot.everyone_permissions, env.bot_b, cur);
                    if m.value != cur {
                        let reason = env.reason.clone();
                        let res = api(env, "@everyone permissions", false, move |a| Box::pin(async move { a.edit_everyone(m.value, &reason).await })).await;
                        if res.value.is_some() {
                            let gid = env.guild.id.clone();
                            if let Some(e) = env.guild.roles.get_mut(&gid) {
                                e.permissions = m.value;
                            }
                            env.state.note_dropped(m.dropped);
                            env.state.bump("rolesUpdated", 1);
                        }
                    }
                }
            }
            RoleItem::Role(r) => restore_role(env, r).await,
        }
        if stalled(env) {
            return false;
        }
        env.state.cursor += 1;
    }
    true
}

async fn restore_role(env: &mut Env<'_>, r: &RoleSnap) {
    let d = decide_role(env, r);
    match d.action {
        Action::Keep => {
            env.state.note_dropped(d.dropped);
            env.state.bump("rolesKept", 1);
        }
        Action::Skip => env.state.bump("rolesSkipped", 1), // an integration's role, or one above the bot
        Action::Update => {
            let (Some(id), Some(data)) = (d.existing.clone(), d.data.clone()) else { return };
            let reason = env.reason.clone();
            let (id2, data2) = (id.clone(), data.clone());
            let res = api(env, &format!("Role \"{}\"", r.name), false, move |a| Box::pin(async move { a.edit_role(&id2, &data2, &reason).await })).await;
            if res.value.is_some() {
                if let Some(role) = env.guild.roles.get_mut(&id) {
                    data.apply(role);
                }
                env.state.note_dropped(d.dropped);
                env.state.bump("rolesUpdated", 1);
            }
        }
        _ => {
            if let Some(adopted) = adopt_role(env, r).await {
                remember(env, &r.id, &adopted.id);
                env.state.bump("rolesCreated", 1);
                return;
            }
            let Some(data) = d.data.clone() else { return };
            let reason = env.reason.clone();
            let res = api(env, &format!("Role \"{}\"", r.name), false, move |a| Box::pin(async move { a.create_role(&data, &reason).await })).await;
            if let Some(role) = res.value
                && !role.id.is_empty()
            {
                remember(env, &r.id, &role.id);
                env.guild.roles.insert(role.id.clone(), role);
                env.state.note_dropped(d.dropped);
                env.state.bump("rolesCreated", 1);
            }
        }
    }
}

async fn step_role_positions(env: &mut Env<'_>) -> bool {
    if env.state.cursor > 0 || !env.can_manage_roles {
        return true;
    }
    if !env.has_room() {
        return false;
    }
    let top = env.bot_top;
    let mut entries: Vec<(String, i64)> = vec![];
    let mut limited = 0; // recreated roles that were above where the bot may place a role
    if env.mode == "missing" {
        // The recreated roles go back to the places they had, as far as the bot's own role allows.
        for r in &env.snapshot.roles {
            let Some(role) = env.find_role(&r.id) else { continue };
            if !env.made_set.contains(&role.id) {
                continue;
            }
            let position = r.position.min(top - 1).max(1);
            if r.position > top - 1 {
                limited += 1;
            }
            if top > 1 && role.raw_position != position {
                entries.push((role.id.clone(), position));
            }
        }
    } else {
        // Only roles the bot may move are put in the backup's order, using the places those same roles hold now.
        let mut movable: Vec<(&RoleSnap, &RoleView)> = vec![];
        for r in &env.snapshot.roles {
            if let Some(role) = env.find_role(&r.id)
                && role.id != env.guild.id
                && !role.managed
                && role.raw_position < top
            {
                movable.push((r, role));
            }
        }
        let mut slots: Vec<i64> = movable.iter().map(|(_, role)| role.raw_position).collect();
        slots.sort();
        movable.sort_by(by_position(|m: &(&RoleSnap, &RoleView)| (m.0.position, m.0.id.as_str())));
        entries = movable
            .iter()
            .enumerate()
            .filter(|(i, (_, role))| role.raw_position != slots[*i])
            .map(|(i, (_, role))| (role.id.clone(), slots[i]))
            .collect();
    }
    if entries.is_empty() {
        env.state.bump("positionsLimited", limited);
        env.state.cursor = 1;
        return true;
    }
    let sent = entries.clone();
    let res = api(env, "Role order", false, move |a| Box::pin(async move { a.set_role_positions(&sent).await })).await;
    if res.value.is_some() {
        for (id, pos) in &entries {
            if let Some(r) = env.guild.roles.get_mut(id) {
                r.raw_position = *pos;
            }
        }
        env.state.bump("positionsSet", entries.len() as i64);
    }
    if stalled(env) {
        return false; // a call that timed out is tried again next batch (and counted then)
    }
    env.state.bump("positionsLimited", limited);
    env.state.cursor = 1; // one attempt per restore: a failing call is reported, not retried for ever
    true
}

async fn step_channels(env: &mut Env<'_>, categories: bool) -> bool {
    let snapshot = env.snapshot;
    let list = if categories { &snapshot.categories } else { &snapshot.channels };
    let bucket = if categories { "categories" } else { "channels" };
    // Never a channel without its permission overwrites (it would be open to everyone): no Manage Roles, no channels.
    if !env.can_manage_channels || !env.can_manage_roles {
        if (env.state.cursor as usize) < list.len() {
            let n = list.len() as i64 - env.state.cursor;
            env.state.bump(&format!("{bucket}Skipped"), n);
            let line = if env.can_manage_channels {
                format!("I need the Manage Roles permission to give {bucket} their permissions, so no {bucket} were restored.")
            } else {
                format!("I need the Manage Channels permission, so no {bucket} were restored.")
            };
            env.state.note_error(&line);
        }
        env.state.cursor = list.len() as i64;
        return true;
    }
    while (env.state.cursor as usize) < list.len() {
        if !env.has_room() {
            return false;
        }
        let c = &list[env.state.cursor as usize];
        let d = decide_channel(env, c);
        match d.action {
            Action::Keep => env.state.bump(&format!("{bucket}Kept"), 1),
            Action::Skip => env.state.bump(&format!("{bucket}Skipped"), 1),
            Action::Update => {
                if let (Some(id), Some(edit)) = (d.existing.clone(), d.edit.clone()) {
                    let reason = env.reason.clone();
                    let (id2, edit2) = (id.clone(), edit.clone());
                    let res = api(env, &format!("#{}", c.name), false, move |a| Box::pin(async move { a.edit_channel(&id2, &edit2, &reason).await })).await;
                    if res.value.is_some() {
                        if let Some(ch) = env.guild.channels.get_mut(&id) {
                            edit.apply(ch);
                        }
                        env.state.bump(&format!("{bucket}Updated"), 1);
                    }
                }
            }
            _ => {
                if let Some(adopted) = adopt_channel(env, c).await {
                    remember(env, &c.id, &adopted.id);
                    env.state.bump(&format!("{bucket}Created"), 1);
                } else if let Some(data) = d.create.clone() {
                    let reason = env.reason.clone();
                    let res = api(env, &format!("#{}", c.name), false, move |a| Box::pin(async move { a.create_channel(&data, &reason).await })).await;
                    if let Some(ch) = res.value
                        && !ch.id.is_empty()
                    {
                        remember(env, &c.id, &ch.id);
                        env.guild.channels.insert(ch.id.clone(), ch);
                        env.state.bump(&format!("{bucket}Created"), 1);
                    }
                }
            }
        }
        if stalled(env) {
            return false;
        }
        env.state.cursor += 1;
    }
    true
}

async fn step_channel_positions(env: &mut Env<'_>) -> bool {
    if env.mode != "full" || !env.can_manage_channels {
        return true;
    }
    let mut entries: Vec<(String, i64)> = vec![];
    let mut off = 0;
    for c in env.snapshot.categories.iter().chain(&env.snapshot.channels) {
        let Some(ch) = env.find_channel(&c.id) else { continue };
        if !ch.manageable {
            continue;
        }
        entries.push((ch.id.clone(), c.position));
        if ch.raw_position != c.position {
            off += 1;
        }
    }
    let chunk = budget::POSITION_CHUNK;
    if env.state.cursor == 0 && off == 0 {
        return true;
    }
    while (env.state.cursor as usize) * chunk < entries.len() {
        if !env.has_room() {
            return false;
        }
        let start = env.state.cursor as usize * chunk;
        let part: Vec<(String, i64)> = entries[start..(start + chunk).min(entries.len())].to_vec();
        let sent = part.clone();
        let res = api(env, "Channel order", false, move |a| Box::pin(async move { a.set_channel_positions(&sent).await })).await;
        if res.value.is_some() {
            for (id, pos) in &part {
                if let Some(ch) = env.guild.channels.get_mut(id) {
                    ch.raw_position = *pos;
                }
            }
            env.state.bump("positionsSet", part.len() as i64);
        }
        if stalled(env) {
            return false;
        }
        env.state.cursor += 1;
    }
    true
}

async fn step_overwrites(env: &mut Env<'_>) -> bool {
    let items = overwrite_items(env.snapshot);
    if !env.can_manage_roles {
        if (env.state.cursor as usize) < items.len() {
            let n = items.len() as i64 - env.state.cursor;
            env.state.bump("overwritesSkipped", n);
            if !items.is_empty() {
                env.state.note_error("I need the Manage Roles permission, so no channel permissions were restored.");
            }
        }
        env.state.cursor = items.len() as i64;
        return true;
    }
    while (env.state.cursor as usize) < items.len() {
        if !env.has_room() {
            return false;
        }
        let d = decide_overwrite(env, &items[env.state.cursor as usize]);
        match d.action {
            Action::Keep => {
                env.state.note_dropped(d.dropped);
                env.state.bump("overwritesKept", 1);
            }
            Action::Skip => env.state.bump("overwritesSkipped", 1),
            Action::Set => apply_overwrite(env, d).await,
            _ => {}
        }
        if stalled(env) {
            return false;
        }
        env.state.cursor += 1;
    }
    true
}

async fn apply_overwrite(env: &mut Env<'_>, d: OverwriteDecision) {
    let (Some(channel_id), Some(target)) = (d.channel_id.clone(), d.target.clone()) else { return };
    if d.kind == OwKind::Member {
        // A member who left cannot be the target of an overwrite.
        let mut present = env.guild.member_ids.contains(&target);
        if !present {
            let t = target.clone();
            let res = api(env, "Member lookup", true, move |a| Box::pin(async move { a.fetch_member(&t).await })).await;
            if res.timeout {
                return; // retried in the next batch
            }
            present = res.value == Some(true);
        }
        if !present {
            env.state.bump("overwritesSkipped", 1);
            return;
        }
    }
    let reason = env.reason.clone();
    let (kind, allow, deny) = (d.kind, d.allow, d.deny);
    let res = api(env, &format!("Permissions of #{}", d.channel_name), false, move |a| {
        Box::pin(async move { a.put_overwrite(&channel_id, &target, kind, allow, deny, &reason).await })
    })
    .await;
    if res.value.is_some() {
        env.state.note_dropped(d.dropped);
        env.state.bump("overwritesSet", 1);
    }
}

/// Batch result of [`run_restore_batch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchResult {
    More,
    Finish,
}

/// Runs one bounded batch of the restore (about 10 Discord calls, about 20 seconds at most), advancing `env.state`.
/// Never fails and never deletes anything. `Finish`: every phase is done (the caller writes the summary).
pub async fn run_restore_batch(env: &mut Env<'_>) -> BatchResult {
    loop {
        let done = match env.state.phase.as_str() {
            "finish" => return BatchResult::Finish,
            "roles" => step_roles(env).await,
            "rolePositions" => step_role_positions(env).await,
            "categories" => step_channels(env, true).await,
            "channels" => step_channels(env, false).await,
            "channelPositions" => step_channel_positions(env).await,
            "overwrites" => step_overwrites(env).await,
            _ => return BatchResult::Finish,
        };
        if !done {
            return BatchResult::More;
        }
        let idx = RESTORE_PHASES.iter().position(|p| *p == env.state.phase).unwrap_or(RESTORE_PHASES.len() - 1);
        env.state.phase = RESTORE_PHASES.get(idx + 1).unwrap_or(&"finish").to_string();
        env.state.cursor = 0;
        if env.state.phase != "finish" && !env.has_room() {
            return BatchResult::More;
        }
    }
}
