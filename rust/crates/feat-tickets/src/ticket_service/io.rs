//! The Discord side of the ticket service, behind one seam, plus the in-process state (§3.4).
//!
//! The JS calls discord.js objects directly (`guild.channels.create`, `channel.send`, `channel.setParent`,
//! `channel.permissionOverwrites.edit`, `channel.messages.fetch`, `client.users.fetch`, ...). Here every REST call goes
//! through [`TicketIo`], so the flows run the same in production ([`SerenityTicketIo`], serenity's HTTP client) and in
//! tests (a fake installed with [`set_io`]). Cache reads (channels, roles, permissions) come from serenity's cache
//! through `app.discord()`; like discord.js, a successful create, edit or delete is written into that cache at once.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use bot_core::App;
use bot_core::error::BoxFuture;
use bot_core::interaction_helpers::{Payload, cached_channel, cached_guild};
use bot_core::sync::KeyedMutex;
use bot_util::async_util::{Throttle, create_throttle};
use indexmap::IndexMap;
use serde_json::json;
use serenity::all::{
    ChannelId, ChannelType, GuildChannel, GuildId, Message, MessageId, PermissionOverwrite, PermissionOverwriteType, Permissions, RoleId, User,
    UserId,
};
use serenity::builder::{CreateChannel, EditChannel, GetMessages};

use super::db::PanelWithOptions;
use super::rules::{DELETE_DELAY_MS, IoError, OverwriteSpec, PANEL_MEMO_MAX, PANEL_MEMO_TTL_MS, RENAME_LOG_MAX, RENAME_WINDOW_MS, rename_window_check};

// ── The seam ──────────────────────────────────────────────────────────────────

/// A posted message: its id and jump link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    pub id: MessageId,
    pub channel_id: ChannelId,
    pub url: String,
}

/// `guild.channels.create(opts)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChannel {
    pub name: String,
    /// `ChannelType` number (0 text, 4 category).
    pub kind: u8,
    pub parent: Option<ChannelId>,
    pub topic: Option<String>,
    pub overwrites: Vec<OverwriteSpec>,
    pub reason: String,
}

/// `channel.permissionOverwrites.edit(id, { Key: true|false, ... }, { type })`: the keys given are set, the others keep
/// what the overwrite had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverwriteEdit {
    pub id: String,
    /// 0 role, 1 member.
    pub kind: u8,
    pub edits: Vec<(&'static str, bool)>,
    pub reason: String,
}

/// The REST calls of the ticket service.
pub trait TicketIo: Send + Sync {
    /// `guild.channels.fetch(id)`: `Ok(None)` when Discord answered without a guild channel.
    fn fetch_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> BoxFuture<Result<Option<GuildChannel>, IoError>>;
    /// `guild.channels.create(opts)`.
    fn create_channel(&self, guild_id: GuildId, spec: NewChannel) -> BoxFuture<Result<GuildChannel, IoError>>;
    /// `channel.send(payload)`.
    fn send_message(&self, channel_id: ChannelId, payload: Payload) -> BoxFuture<Result<SentMessage, IoError>>;
    /// `channel.messages.fetch(id)` then `message.edit(payload)`.
    fn edit_message(&self, channel_id: ChannelId, message_id: MessageId, payload: Payload) -> BoxFuture<Result<SentMessage, IoError>>;
    /// `channel.messages.delete(id)`.
    fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> BoxFuture<Result<(), IoError>>;
    /// `channel.setParent(id, { lockPermissions: false, reason })`.
    fn set_parent(&self, channel_id: ChannelId, parent_id: ChannelId, reason: String) -> BoxFuture<Result<(), IoError>>;
    /// `channel.setName(name, reason)`.
    fn set_name(&self, channel_id: ChannelId, name: String, reason: String) -> BoxFuture<Result<(), IoError>>;
    /// `channel.permissionOverwrites.edit(...)`.
    fn edit_overwrite(&self, guild_id: GuildId, channel_id: ChannelId, edit: OverwriteEdit) -> BoxFuture<Result<(), IoError>>;
    /// `channel.permissionOverwrites.delete(id, reason)`.
    fn delete_overwrite(&self, channel_id: ChannelId, target_id: String, reason: String) -> BoxFuture<Result<(), IoError>>;
    /// `channel.delete(reason)`.
    fn delete_channel(&self, channel_id: ChannelId, reason: String) -> BoxFuture<Result<(), IoError>>;
    /// `channel.messages.fetch({ limit, before, cache: false })`: newest first.
    fn fetch_messages(&self, channel_id: ChannelId, before: Option<MessageId>, limit: u8) -> BoxFuture<Result<Vec<Message>, IoError>>;
    /// `client.users.fetch(id)`.
    fn fetch_user(&self, user_id: UserId) -> BoxFuture<Result<User, IoError>>;
    /// `user.send(payload)`.
    fn send_dm(&self, user_id: UserId, payload: Payload) -> BoxFuture<Result<(), IoError>>;
}

/// The production implementation: serenity's HTTP client (every call fails offline).
pub struct SerenityTicketIo(pub App);

fn http_of(app: &App) -> Result<Arc<serenity::all::Http>, IoError> {
    app.http().ok_or_else(IoError::offline)
}

fn err(e: serenity::Error) -> IoError {
    IoError::of(&e)
}

/// Writes a channel into serenity's cache as discord.js does after a REST call.
pub fn cache_channel(app: &App, channel: &GuildChannel, deleted: bool) {
    let Some(d) = app.discord() else { return };
    let value = json!(channel);
    if deleted {
        if let Ok(mut ev) = serde_json::from_value::<serenity::all::ChannelDeleteEvent>(value) {
            d.cache.update(&mut ev);
        }
    } else if cached_channel(app, channel.guild_id, channel.id).is_some() {
        if let Ok(mut ev) = serde_json::from_value::<serenity::all::ChannelUpdateEvent>(value) {
            d.cache.update(&mut ev);
        }
    } else if let Ok(mut ev) = serde_json::from_value::<serenity::all::ChannelCreateEvent>(value) {
        d.cache.update(&mut ev);
    }
}

fn find_cached(app: &App, channel_id: ChannelId) -> Option<GuildChannel> {
    let d = app.discord()?;
    d.cache.guilds().into_iter().find_map(|g| d.cache.guild(g).and_then(|guild| guild.channels.get(&channel_id).cloned()))
}

/// The serenity overwrite of a spec (unknown permission keys are ignored).
pub fn overwrite_of(spec: &OverwriteSpec) -> Option<PermissionOverwrite> {
    let id: u64 = spec.id.parse().ok().filter(|v| *v != 0)?;
    let bits = |keys: &Option<Vec<&'static str>>| {
        keys.iter().flatten().filter_map(|k| bot_core::permissions::flag(k)).fold(Permissions::empty(), |a, b| a | b)
    };
    let kind = if spec.kind == 0 { PermissionOverwriteType::Role(RoleId::new(id)) } else { PermissionOverwriteType::Member(UserId::new(id)) };
    Some(PermissionOverwrite { allow: bits(&spec.allow), deny: bits(&spec.deny), kind })
}

/// The overwrite after `permissionOverwrites.edit`: the given keys set, the rest kept.
pub fn merged_overwrite(existing: Option<&PermissionOverwrite>, edit: &OverwriteEdit) -> Option<PermissionOverwrite> {
    let id: u64 = edit.id.parse().ok().filter(|v| *v != 0)?;
    let (mut allow, mut deny) = existing.map(|o| (o.allow, o.deny)).unwrap_or((Permissions::empty(), Permissions::empty()));
    for (key, on) in &edit.edits {
        let Some(f) = bot_core::permissions::flag(key) else { continue };
        if *on {
            allow |= f;
            deny &= !f;
        } else {
            deny |= f;
            allow &= !f;
        }
    }
    let kind = if edit.kind == 0 { PermissionOverwriteType::Role(RoleId::new(id)) } else { PermissionOverwriteType::Member(UserId::new(id)) };
    Some(PermissionOverwrite { allow, deny, kind })
}

fn overwrite_target_id(kind: &PermissionOverwriteType) -> u64 {
    match kind {
        PermissionOverwriteType::Member(u) => u.get(),
        PermissionOverwriteType::Role(r) => r.get(),
        _ => 0,
    }
}

impl TicketIo for SerenityTicketIo {
    fn fetch_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> BoxFuture<Result<Option<GuildChannel>, IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            let ch = http.get_channel(channel_id).await.map_err(err)?;
            let Some(gc) = ch.guild() else { return Ok(None) };
            // discord.js `guild.channels.fetch(id)` refuses a channel of another guild (GuildChannelUnowned, no
            // numeric code: the caller sees 'unknown') and caches what it fetched.
            if gc.guild_id != guild_id {
                return Err(IoError { message: "The fetched channel does not belong to this manager's guild.".into(), ..Default::default() });
            }
            bot_core::discord::cache_created_channel(&app, &gc);
            Ok(Some(gc))
        })
    }

    fn create_channel(&self, guild_id: GuildId, spec: NewChannel) -> BoxFuture<Result<GuildChannel, IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            let mut b = CreateChannel::new(spec.name.clone())
                .kind(ChannelType::from(spec.kind))
                .permissions(spec.overwrites.iter().filter_map(overwrite_of).collect::<Vec<_>>())
                .audit_log_reason(&spec.reason);
            if let Some(p) = spec.parent {
                b = b.category(p);
            }
            if let Some(t) = &spec.topic {
                b = b.topic(t.clone());
            }
            let ch = guild_id.create_channel(http.as_ref(), b).await.map_err(err)?;
            cache_channel(&app, &ch, false);
            Ok(ch)
        })
    }

    fn send_message(&self, channel_id: ChannelId, payload: Payload) -> BoxFuture<Result<SentMessage, IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            let m = channel_id.send_message(http.as_ref(), payload.to_message()).await.map_err(err)?;
            let url = m.link();
            Ok(SentMessage { id: m.id, channel_id, url })
        })
    }

    fn edit_message(&self, channel_id: ChannelId, message_id: MessageId, payload: Payload) -> BoxFuture<Result<SentMessage, IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            let m = channel_id.edit_message(http.as_ref(), message_id, payload.to_message_edit()).await.map_err(err)?;
            let url = m.link();
            Ok(SentMessage { id: m.id, channel_id, url })
        })
    }

    fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> BoxFuture<Result<(), IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            channel_id.delete_message(http.as_ref(), message_id).await.map_err(err)
        })
    }

    fn set_parent(&self, channel_id: ChannelId, parent_id: ChannelId, reason: String) -> BoxFuture<Result<(), IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            // A plain parent_id PATCH: Discord keeps the channel's own overwrites (lockPermissions: false).
            let ch = channel_id.edit(http.as_ref(), EditChannel::new().category(parent_id).audit_log_reason(&reason)).await.map_err(err)?;
            cache_channel(&app, &ch, false);
            Ok(())
        })
    }

    fn set_name(&self, channel_id: ChannelId, name: String, reason: String) -> BoxFuture<Result<(), IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            let ch = channel_id.edit(http.as_ref(), EditChannel::new().name(name).audit_log_reason(&reason)).await.map_err(err)?;
            cache_channel(&app, &ch, false);
            Ok(())
        })
    }

    fn edit_overwrite(&self, _guild_id: GuildId, channel_id: ChannelId, edit: OverwriteEdit) -> BoxFuture<Result<(), IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            let existing = find_cached(&app, channel_id)
                .and_then(|c| c.permission_overwrites.iter().find(|o| overwrite_target_id(&o.kind).to_string() == edit.id).cloned());
            let Some(ow) = merged_overwrite(existing.as_ref(), &edit) else { return Err(IoError::code(50035, "Invalid overwrite target")) };
            let target = overwrite_target_id(&ow.kind);
            let body = json!({ "allow": ow.allow.bits().to_string(), "deny": ow.deny.bits().to_string(), "type": edit.kind });
            http.create_permission(channel_id, serenity::all::TargetId::new(target), &body, Some(&edit.reason)).await.map_err(err)
        })
    }

    fn delete_overwrite(&self, channel_id: ChannelId, target_id: String, reason: String) -> BoxFuture<Result<(), IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            let id: u64 = target_id.parse().map_err(|_| IoError::code(10009, "Unknown Overwrite"))?;
            http.delete_permission(channel_id, id.into(), Some(&reason)).await.map_err(err)
        })
    }

    fn delete_channel(&self, channel_id: ChannelId, reason: String) -> BoxFuture<Result<(), IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            let ch = http.delete_channel(channel_id, Some(&reason)).await.map_err(err)?;
            if let Some(g) = ch.guild() {
                cache_channel(&app, &g, true);
            }
            Ok(())
        })
    }

    fn fetch_messages(&self, channel_id: ChannelId, before: Option<MessageId>, limit: u8) -> BoxFuture<Result<Vec<Message>, IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            let mut q = GetMessages::new().limit(limit);
            if let Some(b) = before {
                q = q.before(b);
            }
            channel_id.messages(http.as_ref(), q).await.map_err(err)
        })
    }

    fn fetch_user(&self, user_id: UserId) -> BoxFuture<Result<User, IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            if let Some(u) = app.discord().and_then(|d| d.cache.user(user_id).map(|u| u.clone())) {
                return Ok(u);
            }
            let http = http_of(&app)?;
            http.get_user(user_id).await.map_err(err)
        })
    }

    fn send_dm(&self, user_id: UserId, payload: Payload) -> BoxFuture<Result<(), IoError>> {
        let app = self.0.clone();
        Box::pin(async move {
            let http = http_of(&app)?;
            let dm = user_id.create_dm_channel(http.as_ref()).await.map_err(err)?;
            dm.id.send_message(http.as_ref(), payload.to_message()).await.map(|_| ()).map_err(err)
        })
    }
}

/// The per-app override of the seam (`app.ext`).
#[derive(Default)]
pub struct IoSlot(RwLock<Option<Arc<dyn TicketIo>>>);

/// Installs `io` for this app (tests); `None` restores serenity.
pub fn set_io(app: &App, io: Option<Arc<dyn TicketIo>>) {
    *app.ext::<IoSlot>().0.write().unwrap_or_else(|e| e.into_inner()) = io;
}

/// The seam this app uses.
pub fn io(app: &App) -> Arc<dyn TicketIo> {
    if let Some(io) = app.ext::<IoSlot>().0.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return io;
    }
    Arc::new(SerenityTicketIo(app.clone()))
}

// ── In-process state (§3.4) ───────────────────────────────────────────────────

struct MemoEntry {
    guild_id: String,
    at: i64,
    panel: Arc<PanelWithOptions>,
}

/// Everything the JS keeps in module-level variables. All of it may vanish on restart: every customId carries the ids
/// it needs.
pub struct TicketState {
    panel_memo: Mutex<IndexMap<String, MemoEntry>>,
    memo_epoch: AtomicU64,
    pub ticket_mutex: KeyedMutex,
    pub panel_mutex: KeyedMutex,
    deleting: Mutex<HashSet<String>>,
    open_throttle: Throttle,
    transcript_throttle: Throttle,
    rename_log: Mutex<IndexMap<String, Vec<f64>>>,
    /// The pause before a ticket channel is deleted (`DELETE_DELAY_MS`); tests shorten it.
    pub delete_delay_ms: AtomicU64,
    /// How long a rename is waited for (`withTimeout(..., 15_000)`); tests shorten it.
    pub rename_timeout_ms: AtomicU64,
}

impl Default for TicketState {
    fn default() -> Self {
        Self {
            panel_memo: Mutex::new(IndexMap::new()),
            memo_epoch: AtomicU64::new(0),
            ticket_mutex: KeyedMutex::new(),
            panel_mutex: KeyedMutex::new(),
            deleting: Mutex::new(HashSet::new()),
            open_throttle: create_throttle(5_000),
            transcript_throttle: create_throttle(60_000),
            rename_log: Mutex::new(IndexMap::new()),
            delete_delay_ms: AtomicU64::new(DELETE_DELAY_MS),
            rename_timeout_ms: AtomicU64::new(15_000),
        }
    }
}

/// The app's ticket state.
pub fn state(app: &App) -> Arc<TicketState> {
    app.ext::<TicketState>()
}

/// `invalidatePanel(panelId)`.
pub fn invalidate_panel(app: &App, panel_id: &str) {
    let st = state(app);
    st.memo_epoch.fetch_add(1, Ordering::SeqCst);
    st.panel_memo.lock().unwrap_or_else(|e| e.into_inner()).shift_remove(panel_id);
}

/// `invalidateGuildPanels(guildId)`.
pub fn invalidate_guild_panels(app: &App, guild_id: &str) {
    let st = state(app);
    st.memo_epoch.fetch_add(1, Ordering::SeqCst);
    st.panel_memo.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, v| v.guild_id != guild_id);
}

/// `peekPanel(guildId, panelId)`: memo only, synchronous; the panel when it was read in the last 60 s.
pub fn peek_panel(app: &App, guild_id: &str, panel_id: &str) -> Option<Arc<PanelWithOptions>> {
    let now = app.clock().now_ms();
    let st = state(app);
    let memo = st.panel_memo.lock().unwrap_or_else(|e| e.into_inner());
    let hit = memo.get(panel_id)?;
    if hit.guild_id != guild_id || now - hit.at >= PANEL_MEMO_TTL_MS {
        return None;
    }
    Some(hit.panel.clone())
}

/// The memo epoch (bumped by every invalidation).
pub fn memo_epoch(app: &App) -> u64 {
    state(app).memo_epoch.load(Ordering::SeqCst)
}

/// Stores a panel read at `epoch`, unless an invalidation happened since.
pub fn memo_store(app: &App, guild_id: &str, epoch: u64, panel: Arc<PanelWithOptions>) {
    let st = state(app);
    if st.memo_epoch.load(Ordering::SeqCst) != epoch {
        return;
    }
    let mut memo = st.panel_memo.lock().unwrap_or_else(|e| e.into_inner());
    let id = panel.panel.id.clone();
    memo.shift_remove(&id);
    memo.insert(id, MemoEntry { guild_id: guild_id.to_string(), at: app.clock().now_ms(), panel });
    while memo.len() > PANEL_MEMO_MAX {
        memo.shift_remove_index(0);
    }
}

/// `deleting.has(ticketId)`.
pub fn is_deleting(app: &App, ticket_id: &str) -> bool {
    state(app).deleting.lock().unwrap_or_else(|e| e.into_inner()).contains(ticket_id)
}

pub(crate) fn set_deleting(app: &App, ticket_id: &str, on: bool) {
    let st = state(app);
    let mut d = st.deleting.lock().unwrap_or_else(|e| e.into_inner());
    if on {
        d.insert(ticket_id.to_string());
    } else {
        d.remove(ticket_id);
    }
}

/// `openThrottle(key)`.
pub fn allow_open(app: &App, key: &str) -> bool {
    state(app).open_throttle.allow(key)
}

/// `allowTranscript(ticketId)`: one on-demand transcript per ticket per minute.
pub fn allow_transcript(app: &App, ticket_id: &str) -> bool {
    state(app).transcript_throttle.allow(ticket_id)
}

/// `renameWindow(channelId, now)`.
pub fn rename_window(app: &App, channel_id: &str, now_ms: f64) -> (bool, Option<f64>) {
    let st = state(app);
    let mut log = st.rename_log.lock().unwrap_or_else(|e| e.into_inner());
    let kept: Vec<f64> = log.get(channel_id).map(|v| v.iter().copied().filter(|t| now_ms - t < RENAME_WINDOW_MS).collect()).unwrap_or_default();
    if kept.is_empty() {
        log.shift_remove(channel_id);
    } else {
        log.insert(channel_id.to_string(), kept.clone());
    }
    rename_window_check(&kept, now_ms)
}

/// `recordRename(channelId, now)`.
pub fn record_rename(app: &App, channel_id: &str, now_ms: f64) {
    let st = state(app);
    let mut log = st.rename_log.lock().unwrap_or_else(|e| e.into_inner());
    let mut kept: Vec<f64> = log.get(channel_id).map(|v| v.iter().copied().filter(|t| now_ms - t < RENAME_WINDOW_MS).collect()).unwrap_or_default();
    kept.push(now_ms);
    log.shift_remove(channel_id);
    log.insert(channel_id.to_string(), kept);
    while log.len() > RENAME_LOG_MAX {
        log.shift_remove_index(0);
    }
}

// ── Cache reads ───────────────────────────────────────────────────────────────

/// Parses a snowflake id.
pub fn sf(id: &str) -> Option<u64> {
    id.parse::<u64>().ok().filter(|v| *v != 0)
}

/// `guild.channels.cache.get(id)`.
pub fn channel_cached(app: &App, guild_id: GuildId, id: &str) -> Option<GuildChannel> {
    cached_channel(app, guild_id, ChannelId::new(sf(id)?))
}

/// `guild.roles.cache.has(id)`.
pub fn role_cached(app: &App, guild_id: GuildId, id: &str) -> bool {
    let Some(r) = sf(id) else { return false };
    app.discord().and_then(|d| d.cache.guild(guild_id).map(|g| g.roles.contains_key(&RoleId::new(r)))).unwrap_or(false)
}

/// `guild.available !== false`: the guild is in the cache.
pub fn guild_available(app: &App, guild_id: GuildId) -> bool {
    app.discord().is_some_and(|d| d.cache.guild(guild_id).is_some())
}

/// The cached channels of a guild.
pub fn guild_channels(app: &App, guild_id: GuildId) -> Vec<GuildChannel> {
    cached_guild(app, guild_id).map(|g| g.channels.into_values().collect()).unwrap_or_default()
}

/// `childCount(category)`: channels in a category (from the cache).
pub fn child_count(app: &App, guild_id: GuildId, category_id: ChannelId) -> usize {
    app.discord()
        .and_then(|d| d.cache.guild(guild_id).map(|g| g.channels.values().filter(|c| c.parent_id == Some(category_id)).count()))
        .unwrap_or(0)
}

/// The bot's user id.
pub fn bot_id(app: &App) -> Option<UserId> {
    app.discord().map(|d| d.bot_user_id())
}

/// Whether a channel is a category.
pub fn is_category(c: &GuildChannel) -> bool {
    c.kind == ChannelType::Category
}

/// Where a channel lookup ended (`resolveChannel`'s `state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    Present,
    Gone,
    Unknown,
}

/// `resolveChannel(guild, id)`: the cache first, then a REST fetch. `Gone` only when Discord answered 10003 while the
/// guild is available; a timeout or any other error is `Unknown` and callers treat it as present.
pub async fn resolve_channel(app: &App, guild_id: GuildId, id: &str) -> (Option<GuildChannel>, ChannelState) {
    resolve_channel_timeout(app, guild_id, id, 4000).await
}

pub async fn resolve_channel_timeout(app: &App, guild_id: GuildId, id: &str, timeout_ms: u64) -> (Option<GuildChannel>, ChannelState) {
    if let Some(c) = channel_cached(app, guild_id, id) {
        return (Some(c), ChannelState::Present);
    }
    if !guild_available(app, guild_id) {
        return (None, ChannelState::Unknown);
    }
    let Some(cid) = sf(id) else { return (None, ChannelState::Unknown) };
    let fut = io(app).fetch_channel(guild_id, ChannelId::new(cid));
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), fut).await {
        Ok(Ok(Some(c))) => (Some(c), ChannelState::Present),
        Ok(Ok(None)) => (None, ChannelState::Unknown),
        Ok(Err(e)) if e.is(10003) && guild_available(app, guild_id) => (None, ChannelState::Gone),
        _ => (None, ChannelState::Unknown),
    }
}

/// `confirmGoneChannels(guild, ids)`: the ids (of those not in the cache) that Discord confirmed are gone.
pub async fn confirm_gone_channels(app: &App, guild_id: GuildId, ids: &[String]) -> HashSet<String> {
    let mut missing: Vec<String> = Vec::new();
    for id in ids {
        if !missing.contains(id) && channel_cached(app, guild_id, id).is_none() {
            missing.push(id.clone());
        }
    }
    let app2 = app.clone();
    let results = bot_util::async_util::map_limit(missing.clone(), 5, move |id: String, _i: usize| {
        let app = app2.clone();
        async move { Ok::<_, ()>(resolve_channel(&app, guild_id, &id).await.1) }
    })
    .await;
    let mut gone = HashSet::new();
    for (i, r) in results.into_iter().enumerate() {
        if let Ok(ChannelState::Gone) = r {
            gone.insert(missing[i].clone());
        }
    }
    gone
}

/// `missingBotPermissions(channel, perms)` for a channel object in hand: computed from the channel's own overwrites,
/// so a channel found by a REST fetch is judged like a cached one (discord.js caches what it fetched).
pub fn missing_in_channel(app: &App, guild_id: GuildId, channel: &GuildChannel, perms: &[&str]) -> Vec<String> {
    bot_core::interaction_helpers::missing_from(bot_core::interaction_helpers::bot_permissions_for(app, guild_id, channel), perms)
}

/// `checkChannelUsable(channel, { needs })` for a channel object in hand.
pub fn usable_channel(app: &App, guild_id: GuildId, channel: &GuildChannel, needs: &[&str]) -> bot_core::interaction_helpers::ChannelUsable {
    bot_core::interaction_helpers::check_channel_usable_for(app, guild_id, channel, needs)
}

/// The id of a role as the cache knows it.
pub fn role_id(id: &str) -> Option<RoleId> {
    sf(id).map(RoleId::new)
}
