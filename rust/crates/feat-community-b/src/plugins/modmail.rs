//! Port of `src/plugins/modmail.js`: members DM the bot, each conversation becomes a thread in
//! `GuildSettings.modmailChannelId`, and what staff write in that thread reaches the member's DMs.
//!
//! - member side: the `directMessage` hook (ungated: a DM has no guild, so every server is checked here) relays into
//!   the open conversation, or asks first (a button for one server, a select menu for several) and then opens one.
//! - staff side: the `messageCreate` hook sends a message in an OPEN thread to the member (unless it starts with `//`).
//! - cleanup: `channelDelete` / `threadDelete` close what was lost.
//!
//! The persistent `modmail` buttons and `modmail-select` menus are registered ungated: a DM has no guild, so they
//! re-check everything themselves. Rules and database access: [`crate::modmail_service`]; commands: `/modmail`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bot_config::{ModuleKey, PermissionLevel};
use bot_core::adapters;
use bot_core::embed_builder::{error_embed, guild_embed, info_embed, success_embed};
use bot_core::interaction_helpers::{PermTarget, Payload, cached_channel, missing_bot_permissions, pingable_role_ids, reply_error, safe_reply};
use bot_core::markdown::escape_markdown;
use bot_core::permission_checker::{MemberAccess, resolve_permission_level};
use bot_core::permissions::{self, permission_names};
use bot_core::prelude::*;
use bot_core::settings::Settings;
use bot_db::{GuildSettings, ModmailStatus, ModmailThread};
use bot_util::placeholder_parser::{RenderContext, render_placeholders};
use bot_util::text::clamp_text;
use indexmap::IndexMap;
use serde_json::{Value, json};
use serenity::all::{
    ButtonStyle, ChannelId, ChannelType, GuildChannel, GuildId, Member, Message, MessageId, MessagePagination, ReactionType, User, UserId,
};
use serenity::builder::{
    CreateActionRow, CreateAllowedMentions, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateInteractionResponseMessage,
    CreateMessage, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, EditInteractionResponse,
};

use crate::modmail_service::{
    self as svc, DEFAULT_GREETING, RelayMessage, Reserved, STAFF_HELP, StaffAuthor, THREAD_DELETED, accepts_conversations, has_content, is_blocked,
    is_internal_note, limits as MODMAIL, relay_parts, thread_name, who_lines,
};

/// Unknown channel / message / member, missing access or permission: the target is gone, nothing to retry.
const GONE_CODES: [u64; 5] = [10003, 10007, 10008, 50001, 50013];
/// "Cannot send messages to this user": DMs closed, or no server in common any more.
const DM_CLOSED_CODE: u64 = 50007;
const NOT_MODMAIL_TTL_MS: i64 = 5 * 60_000;
const NOT_MODMAIL_MAX: usize = 5000;

/// The plugin's per-app state (the JS module-level maps and throttles).
pub struct ModmailState {
    prompt_throttle: bot_util::async_util::Throttle,
    click_once: bot_util::async_util::Throttle,
    no_server_notice: bot_util::async_util::Throttle,
    rate: Mutex<HashMap<String, (u32, i64)>>,
    prompting: Mutex<HashMap<String, i64>>,
    not_modmail: Mutex<IndexMap<String, i64>>,
}

impl Default for ModmailState {
    fn default() -> Self {
        Self {
            prompt_throttle: bot_util::async_util::create_throttle(MODMAIL::PROMPT_EVERY_MS),
            click_once: bot_util::async_util::create_throttle(60_000),
            no_server_notice: bot_util::async_util::create_throttle(30_000),
            rate: Mutex::new(HashMap::new()),
            prompting: Mutex::new(HashMap::new()),
            not_modmail: Mutex::new(IndexMap::new()),
        }
    }
}

fn state(app: &App) -> Arc<ModmailState> {
    app.ext::<ModmailState>()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

// ── Small helpers ─────────────────────────────────────────────────────────────

/// `withinRate(userId, now)`: a member's relay budget, RATE_MAX messages per RATE_WINDOW_MS.
pub fn within_rate(app: &App, user_id: &str, now: i64) -> bool {
    let st = state(app);
    let mut rate = lock(&st.rate);
    if rate.len() > 2000 {
        rate.retain(|_, (_, reset)| now < *reset);
    }
    match rate.get_mut(user_id) {
        Some((count, reset)) if now < *reset => {
            *count += 1;
            *count <= MODMAIL::RATE_MAX
        }
        _ => {
            rate.insert(user_id.to_string(), (1, now + MODMAIL::RATE_WINDOW_MS));
            true
        }
    }
}

async fn settings_of(app: &App, guild_id: &str) -> Option<Settings> {
    match app.settings().peek_settings(guild_id) {
        Some(s) => Some(s),
        None => app.settings().get_settings_safe(guild_id).await,
    }
}

fn unicode(emoji: &str) -> ReactionType {
    ReactionType::Unicode(emoji.to_string())
}

/// `react(message, emoji, { inGuild })`: never fails. In a thread the bot needs the permissions to react.
async fn react(app: &App, guild_id: Option<GuildId>, channel_id: ChannelId, message_id: MessageId, emoji: &str) {
    if let Some(g) = guild_id
        && !missing_bot_permissions(app, PermTarget::Channel(g, channel_id), &["AddReactions", "ReadMessageHistory"]).is_empty()
    {
        return;
    }
    let Some(http) = app.http() else { return };
    if let Err(e) = http.create_reaction(channel_id, message_id, &unicode(emoji)).await
        && !bot_core::discord::error_code(&e).is_some_and(|c| GONE_CODES.contains(&c))
    {
        bot_util::log_debug!(format!("[modmail] could not react: {}", bot_core::discord::error_message(&e)));
    }
}

/// Whether the bot's own reaction `emoji` is on the message.
pub fn has_bot_reaction(message: &Message, emoji: &str) -> bool {
    message.reactions.iter().any(|r| r.me && matches!(&r.reaction_type, ReactionType::Unicode(u) if u == emoji))
}

/// `unreact(message, emoji)`: takes the bot's own reaction off a message. Never fails.
async fn unreact(app: &App, message: &Message, emoji: &str) {
    if !has_bot_reaction(message, emoji) {
        return;
    }
    let Some(http) = app.http() else { return };
    if let Err(e) = http.delete_reaction_me(message.channel_id, message.id, &unicode(emoji)).await
        && !bot_core::discord::error_code(&e).is_some_and(|c| GONE_CODES.contains(&c))
    {
        bot_util::log_debug!(format!("[modmail] could not remove a reaction: {}", bot_core::discord::error_message(&e)));
    }
}

/// `markResult(message, ok)`: ✅ or ❌, clearing the ⏳ it may carry.
async fn mark_result(app: &App, message: &Message, ok: bool) {
    unreact(app, message, MODMAIL::WAIT_EMOJI).await;
    react(app, None, message.channel_id, message.id, if ok { MODMAIL::OK_EMOJI } else { MODMAIL::FAIL_EMOJI }).await;
}

/// What `resolveThread` found.
#[derive(Debug, Clone)]
pub enum ThreadLookup {
    Ok(Box<GuildChannel>),
    /// Discord says it does not exist.
    Gone,
    /// Anything else (may be temporary).
    Error,
}

/// `resolveThread(guild, threadId)`: the thread of a conversation; `Gone` only when Discord says it does not exist.
pub async fn resolve_thread(app: &App, guild_id: GuildId, thread_id: &str) -> ThreadLookup {
    // `guild.channels.fetch(id)`: the cache, else REST (bounded), a channel of another guild is an error, not "gone".
    // A placeholder id ("pending-...") is not a snowflake: Discord refuses it (50035), which is an error too.
    let Some(tid) = parse_id(thread_id).map(ChannelId::new) else {
        bot_util::log_warn!(format!("[modmail] {guild_id}: could not fetch thread {thread_id}: Invalid Form Body"));
        return ThreadLookup::Error;
    };
    let a = app.clone();
    let fetch = async move { Ok::<_, std::convert::Infallible>(bot_core::interaction_helpers::fetch_channel(&a, guild_id, tid).await) };
    match bot_util::async_util::with_timeout(fetch, 5000, "modmail thread fetch").await {
        Ok(Ok(c)) => ThreadLookup::Ok(Box::new(c)),
        Ok(Err(bot_core::interaction_helpers::FetchChannelError::Rest(e))) if bot_core::discord::error_code(&e) == Some(10003) => ThreadLookup::Gone,
        Ok(Err(e)) => {
            bot_util::log_warn!(format!("[modmail] {guild_id}: could not fetch thread {thread_id}: {e}"));
            ThreadLookup::Error
        }
        Err(e) => {
            bot_util::log_warn!(format!("[modmail] {guild_id}: could not fetch thread {thread_id}: {e}"));
            ThreadLookup::Error
        }
    }
}

/// What `sendDm` came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmResult {
    Ok,
    DmClosed,
    Failed,
}

/// `sendDm(user, payload)`: never pings, never fails.
async fn send_dm(app: &App, user_id: UserId, msg: CreateMessage) -> DmResult {
    let Some(http) = app.http() else { return DmResult::Failed };
    let msg = msg.allowed_mentions(CreateAllowedMentions::new());
    let sent = match user_id.create_dm_channel(http.as_ref()).await {
        Ok(ch) => ch.id.send_message(http.as_ref(), msg).await.map(|_| ()),
        Err(e) => Err(e),
    };
    match sent {
        Ok(()) => DmResult::Ok,
        Err(e) => {
            let code = bot_core::discord::error_code(&e);
            if code != Some(DM_CLOSED_CODE) {
                bot_util::log_warn!(format!("[modmail] could not DM {user_id}: {}", bot_core::discord::error_message(&e)));
            }
            if code == Some(DM_CLOSED_CODE) { DmResult::DmClosed } else { DmResult::Failed }
        }
    }
}

// ── Embeds ────────────────────────────────────────────────────────────────────

/// Who wrote a member's message (`message.author`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAuthor {
    pub id: String,
    /// `user.displayName ?? user.username`.
    pub name: String,
    pub avatar_url: Option<String>,
}

impl RelayAuthor {
    pub fn of(user: &User) -> Self {
        Self { id: user.id.to_string(), name: adapters::user_display_name(user), avatar_url: Some(adapters::display_avatar_url_default(user)) }
    }
}

/// `memberMessageEmbed(settings, user, message)`: a member's message as staff see it in the thread.
pub fn member_message_embed(settings: Option<&GuildSettings>, user: &RelayAuthor, message: &RelayMessage) -> CreateEmbed {
    let parts = relay_parts(message);
    let mut author = CreateEmbedAuthor::new(clamp_text(&user.name, 100));
    if let Some(url) = &user.avatar_url {
        author = author.icon_url(url);
    }
    let mut e = guild_embed(settings, None, Some(&parts.description), None).author(author).footer(CreateEmbedFooter::new(format!("ID {}", user.id)));
    if let Some(a) = parts.attachments {
        e = e.field("Attachments", a, false);
    }
    e
}

/// `staffMessageEmbed(settings, guild, author, message)`: staff's message as the member sees it in their DMs.
pub fn staff_message_embed(settings: Option<&GuildSettings>, guild_name: &str, guild_icon: Option<&str>, author: &StaffAuthor, message: &RelayMessage) -> CreateEmbed {
    let parts = relay_parts(message);
    let mut a = CreateEmbedAuthor::new(&author.name);
    if let Some(url) = &author.icon_url {
        a = a.icon_url(url);
    }
    let mut footer = CreateEmbedFooter::new(clamp_text(guild_name, 100));
    if let Some(icon) = guild_icon {
        footer = footer.icon_url(icon);
    }
    let mut e = guild_embed(settings, None, Some(&parts.description), None).author(a).footer(footer);
    if let Some(att) = parts.attachments {
        e = e.field("Attachments", att, false);
    }
    e
}

/// `closedEmbed(guild, reason)`.
pub fn closed_embed(guild_name: &str, reason: Option<&str>) -> CreateEmbed {
    let why = reason.filter(|r| !r.is_empty()).map(|r| format!("\n**Reason:** {}", clamp_text(r, MODMAIL::REASON_MAX))).unwrap_or_default();
    info_embed(
        "Conversation Closed",
        Some(&format!(
            "Your conversation with the staff of **{}** has ended.{why}\nSend me a message any time to start a new one.",
            escape_markdown(guild_name)
        )),
        None,
    )
}

/// `greetingText(settings, { guild, member, user })`: placeholders rendered, never empty.
pub fn greeting_text(settings: Option<&GuildSettings>, ctx: &RenderContext) -> String {
    let template = settings.and_then(|s| s.modmail_greeting.as_deref()).filter(|t| !t.is_empty()).unwrap_or(DEFAULT_GREETING);
    let text = render_placeholders(template, ctx, Some(1000));
    if text.is_empty() { render_placeholders(DEFAULT_GREETING, ctx, Some(1000)) } else { text }
}

// ── Finding the right server ──────────────────────────────────────────────────

/// An open conversation that can be used (`{ row, guild, settings }`).
#[derive(Debug, Clone)]
pub struct Conversation {
    pub row: ModmailThread,
    pub guild_id: GuildId,
    pub guild_name: String,
    pub settings: Settings,
}

fn guild_available(app: &App, guild_id: &str) -> bool {
    app.guild_directory().guild_state(guild_id) == bot_core::discord::GuildState::Available
}

fn guild_name(app: &App, guild_id: GuildId) -> Option<String> {
    app.discord().and_then(|d| d.cache.guild(guild_id).map(|g| g.name.clone()))
}

fn guild_icon(app: &App, guild_id: GuildId) -> Option<String> {
    app.discord().and_then(|d| d.cache.guild(guild_id).and_then(|g| adapters::guild_icon_url_default(guild_id, g.icon.as_ref())))
}

/// `listOpenConversations(client, user)`: the member's OPEN conversations whose server is cached, with modmail on and
/// the member not blocked.
pub async fn list_open_conversations(app: &App, user_id: &str) -> anyhow::Result<Vec<Conversation>> {
    let rows = svc::find_open_for_user(app.db(), user_id, MODMAIL::PICK_MAX).await?;
    let mut out = Vec::new();
    for row in rows {
        let Some(gid) = parse_id(&row.guild_id).map(GuildId::new) else { continue };
        if !guild_available(app, &row.guild_id) {
            continue;
        }
        let Some(settings) = settings_of(app, &row.guild_id).await else { continue };
        if !is_module_enabled(Some(&settings), Some(ModuleKey::Modmail)) || is_blocked(Some(&settings), user_id) {
            continue;
        }
        let guild_name = guild_name(app, gid).unwrap_or_default();
        out.push(Conversation { row, guild_id: gid, guild_name, settings });
    }
    Ok(out)
}

async fn member_of(app: &App, guild_id: GuildId, user_id: UserId) -> Option<Member> {
    let d = app.discord()?;
    if let Some(m) = d.cache.guild(guild_id).and_then(|g| g.members.get(&user_id).cloned()) {
        return Some(m);
    }
    let http = d.http.clone();
    let fetch = async move { http.get_member(guild_id, user_id).await };
    match bot_util::async_util::with_timeout(fetch, MODMAIL::MEMBER_FETCH_TIMEOUT_MS, "modmail member fetch").await {
        Ok(m) => Some(m),
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => {
            if bot_core::discord::error_code(&e) != Some(10007) {
                bot_util::log_debug!(format!("[modmail] {guild_id}: member fetch failed: {}", bot_core::discord::error_message(&e)));
            }
            None
        }
        Err(_) => None,
    }
}

/// A server a member can open a conversation in (`{ guild, member, settings }`).
#[derive(Debug, Clone)]
pub struct Server {
    pub guild_id: GuildId,
    pub guild_name: String,
    pub member: Member,
    pub settings: Settings,
}

/// `serverFor(client, guildId, user)`: bot in it, modmail on, a staff channel set, not blocked, member of it.
pub async fn server_for(app: &App, guild_id: &str, user_id: UserId) -> Option<Server> {
    let gid = GuildId::new(parse_id(guild_id)?);
    if !guild_available(app, guild_id) {
        return None;
    }
    let settings = settings_of(app, guild_id).await;
    let u = user_id.to_string();
    if !accepts_conversations(settings.as_deref(), &u) {
        return None;
    }
    let settings = settings?;
    let channel = settings.modmail_channel_id.as_deref().and_then(parse_id).map(ChannelId::new)?;
    cached_channel(app, gid, channel)?;
    let member = member_of(app, gid, user_id).await?;
    Some(Server { guild_id: gid, guild_name: guild_name(app, gid).unwrap_or_default(), member, settings })
}

/// `findServers(client, user)`: the servers a member can open a conversation in, sorted by name. A member fetch only
/// where modmail is on, a few at a time, and no new server after SCAN_BUDGET_MS.
pub async fn find_servers(app: &App, user_id: UserId) -> Vec<Server> {
    let started = app.clock().now_ms();
    let ids = app.guild_directory().available_guild_ids();
    let results = bot_util::async_util::map_limit(ids, MODMAIL::SCAN_CONCURRENCY, |g, _| {
        let app = app.clone();
        async move {
            if app.clock().now_ms() - started > MODMAIL::SCAN_BUDGET_MS {
                return Ok::<_, ()>(None);
            }
            Ok(server_for(&app, &g, user_id).await)
        }
    })
    .await;
    let mut out: Vec<Server> = results.into_iter().filter_map(|r| r.ok().flatten()).collect();
    out.sort_by(|a, b| bot_util::locale_compare(&a.guild_name, &b.guild_name));
    out
}

// ── Relaying ──────────────────────────────────────────────────────────────────

/// The outcome of a relay (`{ ok }` or the `code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relay {
    Ok,
    Empty,
    Gone,
    DmClosed,
    Failed,
}

/// `relayToThread({ thread, row, settings, message })`: member -> staff.
pub async fn relay_to_thread(app: &App, thread: &GuildChannel, row: &ModmailThread, settings: Option<&GuildSettings>, author: &RelayAuthor, message: &RelayMessage) -> Relay {
    if !has_content(message) {
        return Relay::Empty;
    }
    let Some(http) = app.http() else { return Relay::Failed };
    if thread.thread_metadata.is_some_and(|m| m.archived) {
        let _ = http.edit_thread(thread.id, &json!({ "archived": false }), None).await;
    }
    let msg = CreateMessage::new().embed(member_message_embed(settings, author, message)).allowed_mentions(CreateAllowedMentions::new());
    match thread.id.send_message(http.as_ref(), msg).await {
        Ok(_) => {
            svc::touch(app, &row.id);
            Relay::Ok
        }
        Err(e) if bot_core::discord::error_code(&e) == Some(10003) => Relay::Gone,
        Err(e) => {
            bot_util::log_warn!(format!("[modmail] {}: could not post in thread {}: {}", row.guild_id, row.thread_id, bot_core::discord::error_message(&e)));
            Relay::Failed
        }
    }
}

/// `relayToMember({ client, row, guild, settings, author, message })`: staff -> member.
pub async fn relay_to_member(app: &App, row: &ModmailThread, guild_id: GuildId, settings: Option<&GuildSettings>, author: &StaffAuthor, message: &RelayMessage) -> Relay {
    if !has_content(message) {
        return Relay::Empty;
    }
    let Some(uid) = parse_id(&row.user_id).map(UserId::new) else { return Relay::Failed };
    let Some(http) = app.http() else { return Relay::Failed };
    let fetch = async move { http.get_user(uid).await };
    if let Err(e) = bot_util::async_util::with_timeout(fetch, 5000, "modmail user fetch").await {
        bot_util::log_warn!(format!("[modmail] {guild_id}: could not fetch member {}: {e}", row.user_id));
        return Relay::Failed;
    }
    let name = guild_name(app, guild_id).unwrap_or_default();
    let icon = guild_icon(app, guild_id);
    let embed = staff_message_embed(settings, &name, icon.as_deref(), author, message);
    match send_dm(app, uid, CreateMessage::new().embed(embed)).await {
        DmResult::Ok => {
            svc::touch(app, &row.id);
            Relay::Ok
        }
        DmResult::DmClosed => Relay::DmClosed,
        DmResult::Failed => Relay::Failed,
    }
}

/// 'OK' | 'FAILED' | 'GONE' of a delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Ok,
    Failed,
    Gone,
}

/// `deliverDm(conv, message)`: delivers a member's DM into an existing conversation; `Gone` (and the row closed)
/// when the thread is gone. No reaction.
async fn deliver_dm(app: &App, conv: &Conversation, message: &Message) -> Delivery {
    let close = |app: App, id: String| async move {
        let _ = svc::close_row(&app, &id, None, Some(THREAD_DELETED)).await;
    };
    let res = match resolve_thread(app, conv.guild_id, &conv.row.thread_id).await {
        ThreadLookup::Gone => {
            close(app.clone(), conv.row.id.clone()).await;
            return Delivery::Gone;
        }
        ThreadLookup::Ok(thread) => {
            relay_to_thread(app, &thread, &conv.row, Some(&conv.settings), &RelayAuthor::of(&message.author), &RelayMessage::of(message)).await
        }
        ThreadLookup::Error => Relay::Failed,
    };
    match res {
        Relay::Gone => {
            close(app.clone(), conv.row.id.clone()).await;
            Delivery::Gone
        }
        Relay::Ok => Delivery::Ok,
        _ => Delivery::Failed,
    }
}

/// `relayDm(conv, message)`: [`deliver_dm`], reacting ✅ or ❌ on the message.
async fn relay_dm(app: &App, conv: &Conversation, message: &Message) -> Delivery {
    let outcome = deliver_dm(app, conv, message).await;
    if outcome != Delivery::Gone {
        mark_result(app, message, outcome == Delivery::Ok).await;
    }
    outcome
}

// ── Opening a conversation ────────────────────────────────────────────────────

const OPEN_PERMS: [&str; 3] = ["ViewChannel", "SendMessagesInThreads", "EmbedLinks"];

/// What the staff channel needs from the bot (`channelProblems`): `errors` stop a setup, `warnings` are told.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelProblems {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub can_private: bool,
    pub can_public: bool,
}

/// `channelProblems(channel, settings)`. `ping_role_ids` is `Some` when the settings are given.
pub fn channel_problems(app: &App, guild_id: GuildId, channel_id: ChannelId, ping_role_ids: Option<&[String]>) -> ChannelProblems {
    let missing = |perms: &[&str]| missing_bot_permissions(app, PermTarget::Channel(guild_id, channel_id), perms);
    let mut p = ChannelProblems::default();
    let lacking = missing(&OPEN_PERMS);
    if !lacking.is_empty() {
        p.errors.push(format!("I need **{}** in <#{channel_id}>.", permission_names(&lacking).join(", ")));
    }
    p.can_private = missing(&["CreatePrivateThreads"]).is_empty();
    p.can_public = missing(&["CreatePublicThreads"]).is_empty();
    if !p.can_private && !p.can_public {
        p.errors.push(format!("I need **Create Private Threads** in <#{channel_id}> to open conversations."));
    } else if !p.can_private {
        p.warnings.push("I lack **Create Private Threads**, so conversations would open as **public** threads. Give me that permission.".into());
    }
    if !missing(&["ManageThreads"]).is_empty() {
        p.warnings.push("I lack **Manage Threads**, so I can archive closed conversations but not lock them.".into());
    }
    if ping_role_ids.is_some_and(|r| r.is_empty()) {
        p.warnings.push("No role is pinged, so only members who can manage threads will see new private threads. Set one with `ping_role`.".into());
    }
    p
}

/// `createThread(channel, options)`: a week's archive, else a day (a week needs a higher boost level: 50035).
async fn create_thread(app: &App, channel_id: ChannelId, mut body: Value, reason: &str) -> Result<GuildChannel, Box<serenity::Error>> {
    let Some(http) = app.http() else { return Err(Box::new(serenity::Error::Other("not connected"))) };
    body["auto_archive_duration"] = json!(10080);
    match http.create_thread(channel_id, &body, Some(reason)).await {
        Err(e) if bot_core::discord::error_code(&e) == Some(50035) => {
            body["auto_archive_duration"] = json!(1440);
            http.create_thread(channel_id, &body, Some(reason)).await.map_err(Box::new)
        }
        other => other.map_err(Box::new),
    }
}

/// `closeRow(id)` of the plugin (the Close button of a thread's first message).
pub fn close_button_row(row_id: &str) -> CreateActionRow {
    let id = bot_config::custom_id!("modmail", "close", row_id).unwrap_or_default();
    CreateActionRow::Buttons(vec![CreateButton::new(id).label("Close conversation").emoji(unicode("🔒")).style(ButtonStyle::Secondary)])
}

/// What a new thread's first message is made of (`introPayload`).
pub struct Intro {
    pub user_id: String,
    pub user_created_ms: Option<f64>,
    pub member_joined_ms: Option<f64>,
    /// `pingableRoleIds(...)` for the bot.
    pub allowed: Vec<String>,
    pub refused: Vec<String>,
    pub public_reason: Option<String>,
    pub row_id: String,
}

/// `introPayload(...)`: who the member is, the pings and the close button.
pub fn intro_payload(settings: Option<&GuildSettings>, intro: &Intro) -> CreateMessage {
    let mut lines = who_lines(&intro.user_id, intro.user_created_ms, intro.member_joined_ms);
    if !intro.refused.is_empty() {
        let list = intro.refused.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(", ");
        lines.push(format!("⚠️ I cannot ping {list}: make the role mentionable or give me **Mention Everyone**."));
    }
    if let Some(r) = &intro.public_reason {
        lines.push(format!("⚠️ {r}"));
    }
    lines.push(String::new());
    lines.push(STAFF_HELP.into());
    let mut msg = CreateMessage::new()
        .embed(guild_embed(settings, Some("📬 New conversation"), Some(&lines.join("\n")), None))
        .components(vec![close_button_row(&intro.row_id)]);
    if !intro.allowed.is_empty() {
        msg = msg.content(intro.allowed.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" "));
    }
    let roles: Vec<serenity::all::RoleId> = intro.allowed.iter().filter_map(|r| parse_id(r)).map(serenity::all::RoleId::new).collect();
    msg.allowed_mentions(CreateAllowedMentions::new().roles(roles).empty_users())
}

/// The result of [`open_conversation`].
#[derive(Debug, Clone)]
pub enum Opened {
    Ok { row: ModmailThread, thread: Box<GuildChannel>, relayed: bool, notes: Vec<String> },
    /// `BLOCKED`, `NO_CHANNEL`, `BOT_PERMS`, `OPENING`, `CREATE_FAILED`, `DB_ERROR` or `EXISTS` (with the row).
    Refused { code: &'static str, row: Option<ModmailThread>, errors: Vec<String> },
}

fn refused(code: &'static str) -> Opened {
    Opened::Refused { code, row: None, errors: Vec::new() }
}

/// `openConversation({ guild, member, user, settings, message })`: re-checks the server, reserves the one OPEN row,
/// creates the thread (private, else public), posts who the member is and pings the configured roles, greets the
/// member and relays their first message.
pub async fn open_conversation(app: &App, guild_id: GuildId, member: Option<Member>, user: User, settings: Settings, message: RelayMessage) -> Opened {
    let a = app.clone();
    svc::run_open_locked(app, &guild_id.to_string(), &user.id.to_string(), move || async move { open_locked(&a, guild_id, member, user, settings, message).await })
        .await
}

async fn open_locked(app: &App, guild_id: GuildId, member: Option<Member>, user: User, settings: Settings, message: RelayMessage) -> Opened {
    let (g, u) = (guild_id.to_string(), user.id.to_string());
    if !accepts_conversations(Some(&settings), &u) {
        return refused("BLOCKED");
    }
    let Some(cid) = settings.modmail_channel_id.as_deref().and_then(parse_id).map(ChannelId::new) else { return refused("NO_CHANNEL") };
    let Some(channel) = cached_channel(app, guild_id, cid).filter(|c| c.kind == ChannelType::Text) else { return refused("NO_CHANNEL") };
    let problems = channel_problems(app, guild_id, channel.id, None);
    if !problems.errors.is_empty() {
        bot_util::log_warn!(format!("[modmail] {g}: cannot open a conversation: {}", problems.errors.join(" ")));
        return Opened::Refused { code: "BOT_PERMS", row: None, errors: problems.errors };
    }

    let row = match svc::reserve_open(app, &g, &u).await {
        Ok(Reserved::Row(r)) => r,
        Ok(Reserved::Existing(r)) => {
            let code = if svc::is_pending_thread_id(&r.thread_id) { "OPENING" } else { "EXISTS" };
            return Opened::Refused { code, row: Some(r), errors: Vec::new() };
        }
        Err(e) => {
            bot_util::log_error!(format!("[modmail] {g}: reserving a conversation failed"), bot_core::error::log_arg(&e));
            return refused("DB_ERROR");
        }
    };

    let name = thread_name(Some(&user.name));
    let why = clamp_text(&format!("Modmail conversation with {} ({})", user.name, user.id), 512);
    let mut thread: Option<GuildChannel> = None;
    let mut is_public = false;
    let mut public_reason = String::new();
    let mut last_error: Option<String> = None;
    if problems.can_private {
        match create_thread(app, channel.id, json!({ "name": name, "type": 12, "invitable": false }), &why).await {
            Ok(t) => thread = Some(t),
            Err(e) => {
                let what = bot_core::discord::error_code(&e).map(|c| c.to_string()).unwrap_or_else(|| bot_core::discord::error_message(&e));
                last_error = Some(bot_core::discord::error_message(&e));
                public_reason = "Private threads could not be created here, so this conversation is in a **public** thread.".into();
                bot_util::log_warn!(format!("[modmail] {g}: private thread failed ({what}), falling back to a public thread"));
            }
        }
    } else {
        public_reason = "I lack **Create Private Threads**, so this conversation is in a **public** thread.".into();
    }
    if thread.is_none() && problems.can_public {
        match create_thread(app, channel.id, json!({ "name": name, "type": 11 }), &why).await {
            Ok(t) => {
                thread = Some(t);
                is_public = true;
            }
            Err(e) => last_error = Some(bot_core::discord::error_message(&e)),
        }
    }
    let Some(thread) = thread else {
        svc::release_reserved(app.db(), &row.id).await;
        bot_util::log_warn!(format!("[modmail] {g}: could not create a thread: {}", last_error.unwrap_or_else(|| "undefined".into())));
        return refused("CREATE_FAILED");
    };

    let (allowed, refused_roles) = match (app.discord().and_then(|d| d.cache.guild(guild_id).map(|g| g.clone())), app.discord().map(|d| d.bot_user_id())) {
        (Some(guild), Some(bot)) => {
            let me = permissions::me(&guild, bot).map(|m| (bot, m.roles.clone()));
            let p = pingable_role_ids(&guild, &channel, me.as_ref().map(|(id, roles)| (*id, roles.as_slice())), &settings.modmail_ping_role_ids);
            (p.allowed, p.refused)
        }
        _ => (Vec::new(), settings.modmail_ping_role_ids.clone()),
    };
    let intro = Intro {
        user_id: u.clone(),
        user_created_ms: Some(adapters::snowflake_ms(user.id.get())),
        member_joined_ms: member.as_ref().and_then(|m| m.joined_at).map(|t| t.unix_timestamp() as f64 * 1000.0),
        allowed,
        refused: refused_roles,
        public_reason: is_public.then_some(public_reason),
        row_id: row.id.clone(),
    };
    let started: anyhow::Result<ModmailThread> = async {
        let http = app.http().ok_or_else(|| anyhow::anyhow!("not connected"))?;
        thread.id.send_message(http.as_ref(), intro_payload(Some(&settings), &intro)).await?;
        svc::attach_thread(app, &row.id, &thread.id.to_string()).await
    }
    .await;
    let row = match started {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[modmail] {g}: could not start the thread"), bot_core::error::log_arg(&e));
            if let Some(http) = app.http() {
                let _ = http.delete_channel(thread.id, Some("Modmail conversation could not be saved")).await;
            }
            svc::release_reserved(app.db(), &row.id).await;
            return refused("CREATE_FAILED");
        }
    };

    let mut notes = Vec::new();
    let ctx = adapters::render_context(app, Some(guild_id), &user, member.as_ref(), None);
    let gname = guild_name(app, guild_id).unwrap_or_default();
    let greeting = guild_embed(Some(&settings), Some(&format!("📬 {}", clamp_text(&gname, 200))), Some(&greeting_text(Some(&settings), &ctx)), None)
        .footer(CreateEmbedFooter::new("Reply to this message to talk to the staff."));
    if send_dm(app, user.id, CreateMessage::new().embed(greeting)).await != DmResult::Ok {
        notes.push("The greeting could not be sent.".to_string());
    }
    let relayed = relay_to_thread(app, &thread, &row, Some(&settings), &RelayAuthor::of(&user), &message).await == Relay::Ok;
    Opened::Ok { row, thread: Box::new(thread), relayed, notes }
}

// ── Closing ───────────────────────────────────────────────────────────────────

/// What `closeAndNotify` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Closed {
    pub closed: bool,
    pub notified: bool,
}

/// `closeAndNotify({ client, guild, row, closedById, reason, memberReason, notify })`: closes a conversation's row
/// and tells the member. Once only.
pub async fn close_and_notify(app: &App, guild_id: GuildId, row: &ModmailThread, closed_by_id: Option<&str>, reason: Option<&str>, member_reason: Option<&str>, notify: bool) -> anyhow::Result<Closed> {
    let closed = svc::close_row(app, &row.id, closed_by_id, reason).await?;
    if !closed || !notify {
        return Ok(Closed { closed, notified: false });
    }
    let Some(uid) = parse_id(&row.user_id).map(UserId::new) else { return Ok(Closed { closed, notified: false }) };
    let Some(http) = app.http() else { return Ok(Closed { closed, notified: false }) };
    let fetch = async move { http.get_user(uid).await };
    if let Err(e) = bot_util::async_util::with_timeout(fetch, 5000, "modmail user fetch").await {
        bot_util::log_debug!(format!("[modmail] {guild_id}: could not tell {} the conversation closed: {e}", row.user_id));
        return Ok(Closed { closed, notified: false });
    }
    let name = guild_name(app, guild_id).unwrap_or_default();
    let notified = send_dm(app, uid, CreateMessage::new().embed(closed_embed(&name, member_reason))).await == DmResult::Ok;
    Ok(Closed { closed, notified })
}

/// `archiveThread(thread)`: locks (when allowed) and archives a closed conversation's thread. Never fails; the note
/// for staff.
pub async fn archive_thread(app: &App, guild_id: GuildId, thread: Option<&GuildChannel>) -> Option<String> {
    let thread = thread?;
    let Some(http) = app.http() else { return Some("I couldn't archive the thread.".into()) };
    let (locked, archived) = thread.thread_metadata.map(|m| (m.locked, m.archived)).unwrap_or((false, false));
    let mut note: Option<String> = None;
    if !locked {
        if missing_bot_permissions(app, PermTarget::Channel(guild_id, thread.id), &["ManageThreads"]).is_empty() {
            if let Err(e) = http.edit_thread(thread.id, &json!({ "locked": true }), None).await {
                note = Some("I couldn't lock the thread.".into());
                bot_util::log_debug!(format!("[modmail] could not lock {}: {}", thread.id, bot_core::discord::error_message(&e)));
            }
        } else {
            note = Some("I couldn't lock the thread (I need **Manage Threads**).".into());
        }
    }
    if !archived
        && let Err(e) = http.edit_thread(thread.id, &json!({ "archived": true }), None).await
    {
        note.get_or_insert_with(|| "I couldn't archive the thread.".into());
        bot_util::log_debug!(format!("[modmail] could not archive {}: {}", thread.id, bot_core::discord::error_message(&e)));
    }
    note
}

/// `closeConversation(...)`: closes a conversation, says so in its thread, then locks and archives it.
pub async fn close_conversation(
    app: &App,
    guild_id: GuildId,
    row: &ModmailThread,
    closed_by_id: Option<&str>,
    reason: Option<&str>,
    member_reason: Option<&str>,
    text: Option<&str>,
) -> anyhow::Result<Closed> {
    let res = close_and_notify(app, guild_id, row, closed_by_id, reason, member_reason, true).await?;
    if !res.closed {
        return Ok(res);
    }
    if let ThreadLookup::Ok(thread) = resolve_thread(app, guild_id, &row.thread_id).await {
        let notice = match text {
            Some(t) => t.to_string(),
            None => close_notice(closed_by_id, reason),
        };
        if let Some(http) = app.http() {
            let msg = CreateMessage::new().embed(success_embed("Conversation Closed", Some(&notice), None)).allowed_mentions(CreateAllowedMentions::new());
            let _ = thread.id.send_message(http.as_ref(), msg).await;
        }
        archive_thread(app, guild_id, Some(&thread)).await;
    }
    Ok(res)
}

/// The default thread notice of [`close_conversation`].
pub fn close_notice(closed_by_id: Option<&str>, reason: Option<&str>) -> String {
    let by = closed_by_id.filter(|c| !c.is_empty()).map(|c| format!(" by <@{c}>")).unwrap_or_default();
    let why = reason.filter(|r| !r.is_empty()).map(|r| format!("\n**Reason:** {}", clamp_text(r, MODMAIL::REASON_MAX))).unwrap_or_default();
    format!("Conversation closed{by}.{why}")
}

// ── Member side: the DM flow ──────────────────────────────────────────────────

fn options_row(buttons: Vec<CreateButton>) -> CreateActionRow {
    CreateActionRow::Buttons(buttons)
}

/// `confirmPayload(guild, messageId)`: "Contact staff of X?" for one server.
pub fn confirm_payload(guild_id: &str, guild_name: &str, message_id: &str) -> CreateMessage {
    let open = bot_config::custom_id!("modmail", "open", guild_id, message_id).unwrap_or_default();
    let cancel = bot_config::custom_id!("modmail", "cancel").unwrap_or_default();
    CreateMessage::new()
        .embed(info_embed(
            "Contact Staff?",
            Some(&format!(
                "Do you want to send your message to the staff of **{}**? A private conversation opens; staff will see your name and your messages, and answer you here.",
                escape_markdown(guild_name)
            )),
            None,
        ))
        .components(vec![options_row(vec![
            CreateButton::new(open).label("Send to staff").emoji(unicode("📨")).style(ButtonStyle::Primary),
            CreateButton::new(cancel).label("Cancel").style(ButtonStyle::Secondary),
        ])])
        .allowed_mentions(CreateAllowedMentions::new())
}

fn pick_payload(mode: &str, entries: &[(String, String)], message_id: &str, title: &str, text: &str) -> CreateMessage {
    let shown = &entries[..entries.len().min(MODMAIL::PICK_MAX)];
    let id = bot_config::custom_id!("modmail-select", mode, message_id).unwrap_or_default();
    let options: Vec<CreateSelectMenuOption> = shown
        .iter()
        .map(|(name, value)| {
            let label = clamp_text(name, 100);
            CreateSelectMenuOption::new(if label.is_empty() { "Server".to_string() } else { label }, value.clone())
        })
        .collect();
    let menu = CreateSelectMenu::new(id, CreateSelectMenuKind::String { options }).placeholder("Choose a server");
    let more = if entries.len() > shown.len() { format!("\nShowing the first {} of {} servers.", shown.len(), entries.len()) } else { String::new() };
    CreateMessage::new()
        .embed(info_embed(title, Some(&format!("{text}{more}")), None))
        .components(vec![CreateActionRow::SelectMenu(menu)])
        .allowed_mentions(CreateAllowedMentions::new())
}

/// `openPickPayload(servers, messageId)`: the server menu (`modmail-select:open`). Entries are `(name, guild id)`.
pub fn open_pick_payload(servers: &[(String, String)], message_id: &str) -> CreateMessage {
    pick_payload("open", servers, message_id, "Which server?", "Which server do you want to contact? Your message will be sent to its staff.")
}

/// `relayPickPayload(conversations, messageId)`: the conversation menu (`modmail-select:relay`). Entries are
/// `(server name, row id)`.
pub fn relay_pick_payload(conversations: &[(String, String)], message_id: &str) -> CreateMessage {
    pick_payload("relay", conversations, message_id, "Which conversation?", "You have several open conversations. Which one is this message for?")
}

async fn send_notice(app: &App, channel_id: ChannelId, embed: CreateEmbed) {
    let Some(http) = app.http() else { return };
    if let Err(e) = channel_id.send_message(http.as_ref(), CreateMessage::new().embed(embed).allowed_mentions(CreateAllowedMentions::new())).await {
        bot_util::log_debug!(format!("[modmail] could not reply in a DM: {}", bot_core::discord::error_message(&e)));
    }
}

fn start_prompting(app: &App, user_id: &str, now: i64) {
    let st = state(app);
    let mut p = lock(&st.prompting);
    if p.len() > 2000 {
        p.retain(|_, until| *until > now);
    }
    p.insert(user_id.to_string(), now + MODMAIL::PROMPT_EVERY_MS as i64);
}

fn stop_prompting(app: &App, user_id: &str) {
    lock(&state(app).prompting).remove(user_id);
}

/// Whether a "which server?" prompt of the member is out (tests).
pub fn is_prompting(app: &App, user_id: &str) -> bool {
    lock(&state(app).prompting).get(user_id).is_some_and(|until| *until > app.clock().now_ms())
}

/// `markWaiting(message)`: ⏳ on a throttled DM; it is sent once the member answers the prompt.
async fn mark_waiting(app: &App, message: &Message) {
    if is_prompting(app, &message.author.id.to_string()) {
        react(app, None, message.channel_id, message.id, MODMAIL::WAIT_EMOJI).await;
    }
}

fn unavailable() -> CreateEmbed {
    error_embed("Try Again Later", Some("I couldn't reach the staff just now. Please try again in a moment."), None)
}

/// The "No Server Found" notice.
pub fn no_server_embed() -> CreateEmbed {
    info_embed(
        "No Server Found",
        Some("I couldn't find a server where you can contact staff through me. Staff can only be reached in servers we share that have modmail turned on."),
        None,
    )
}

async fn send_payload(app: &App, channel_id: ChannelId, msg: CreateMessage) -> anyhow::Result<()> {
    let http = app.http().ok_or_else(|| anyhow::anyhow!("not connected"))?;
    channel_id.send_message(http.as_ref(), msg).await?;
    Ok(())
}

/// `handleDirectMessage(ctx)`: a DM to the bot. Relays it into the member's open conversation, or asks where to open
/// one. Never fails.
pub async fn handle_direct_message(app: &App, message: &Message) {
    let user = &message.author;
    if user.bot || !has_content(&RelayMessage::of(message)) {
        return;
    }
    let u = user.id.to_string();
    let res: anyhow::Result<()> = async {
        if !within_rate(app, &u, app.clock().now_ms()) {
            react(app, None, message.channel_id, message.id, MODMAIL::FAIL_EMOJI).await;
            return Ok(());
        }
        let st = state(app);
        let open = list_open_conversations(app, &u).await?;
        if open.len() == 1 {
            if relay_dm(app, &open[0], message).await != Delivery::Gone {
                return Ok(());
            }
        } else if open.len() > 1 {
            if !st.prompt_throttle.allow(&u) {
                mark_waiting(app, message).await;
                return Ok(());
            }
            start_prompting(app, &u, app.clock().now_ms());
            let entries: Vec<(String, String)> = open.iter().map(|c| (c.guild_name.clone(), c.row.id.clone())).collect();
            send_payload(app, message.channel_id, relay_pick_payload(&entries, &message.id.to_string())).await?;
            return Ok(());
        }

        // Nothing open: ask before opening anything.
        if !st.prompt_throttle.allow(&u) {
            mark_waiting(app, message).await;
            return Ok(());
        }
        start_prompting(app, &u, app.clock().now_ms());
        let servers = find_servers(app, user.id).await;
        if servers.is_empty() {
            stop_prompting(app, &u);
            if st.no_server_notice.allow(&u) {
                send_notice(app, message.channel_id, no_server_embed()).await;
            }
            return Ok(());
        }
        let mid = message.id.to_string();
        let payload = if servers.len() == 1 {
            confirm_payload(&servers[0].guild_id.to_string(), &servers[0].guild_name, &mid)
        } else {
            let entries: Vec<(String, String)> = servers.iter().map(|s| (s.guild_name.clone(), s.guild_id.to_string())).collect();
            open_pick_payload(&entries, &mid)
        };
        send_payload(app, message.channel_id, payload).await
    }
    .await;
    if let Err(e) = res {
        stop_prompting(app, &u);
        bot_util::log_error!(format!("[modmail] handling a DM from {u} failed"), bot_core::error::log_arg(&e));
        send_notice(app, message.channel_id, unavailable()).await;
    }
}

/// `dmText[code]`: the DM answer to a refused open.
pub fn dm_text(code: &str) -> CreateEmbed {
    match code {
        "BLOCKED" => error_embed("Not Available", Some("You cannot contact that server through me right now."), None),
        "NO_CHANNEL" => error_embed("Not Available", Some("That server is not set up to receive messages right now."), None),
        "OPENING" => info_embed("One Moment", Some("Your conversation is being opened. Please send your message again in a moment."), None),
        _ => unavailable(),
    }
}

/// `finishPrompt(interaction, embed)`: the prompt message becomes the answer, without its components.
async fn finish_prompt(ctx: &ComponentCtx, embed: CreateEmbed) {
    if let Err(e) = ctx.edit_reply(EditInteractionResponse::new().embeds(vec![embed]).components(vec![])).await {
        bot_util::log_debug!(format!("[modmail] could not update a prompt: {e}"));
    }
}

async fn safe_defer_update(ctx: &ComponentCtx) -> bool {
    if ctx.is_acknowledged() {
        return true;
    }
    match ctx.defer_update().await {
        Ok(()) => true,
        Err(e) => {
            bot_util::log_debug!(format!("[modmail] deferUpdate failed: {e}"));
            false
        }
    }
}

/// `fetchOriginal(interaction, messageId)`: the member's own DM the prompt was about.
async fn fetch_original(app: &App, channel_id: ChannelId, message_id: &str, user_id: UserId) -> Option<Message> {
    let http = app.http()?;
    let mid = MessageId::new(parse_id(message_id)?);
    let fetch = async move { http.get_message(channel_id, mid).await };
    let m = bot_util::async_util::with_timeout(fetch, 5000, "modmail original message").await.ok()?;
    (m.author.id == user_id).then_some(m)
}

/// `fetchWaiting`'s choice: the run of the member's messages right after the prompting one that carry the bot's ⏳,
/// oldest first, at most CATCH_UP_MAX; skipping ones already marked ✅; the run ends at the first message without ⏳.
pub fn waiting_run(batch: Vec<Message>, user_id: UserId) -> Vec<Message> {
    let mut theirs: Vec<Message> = batch.into_iter().filter(|m| m.author.id == user_id && !m.author.bot && has_content(&RelayMessage::of(m))).collect();
    theirs.sort_by_key(|m| m.id.get());
    let mut waiting = Vec::new();
    for m in theirs {
        if !has_bot_reaction(&m, MODMAIL::WAIT_EMOJI) {
            break;
        }
        if has_bot_reaction(&m, MODMAIL::OK_EMOJI) {
            continue;
        }
        waiting.push(m);
        if waiting.len() >= MODMAIL::CATCH_UP_MAX {
            break;
        }
    }
    waiting
}

/// `fetchWaiting(interaction, messageId)`: the DMs that waited for the member's answer. Never fails.
async fn fetch_waiting(app: &App, channel_id: ChannelId, message_id: &str, user_id: UserId) -> Vec<Message> {
    let (Some(http), Some(mid)) = (app.http(), parse_id(message_id)) else { return Vec::new() };
    let fetch = async move { http.get_messages(channel_id, Some(MessagePagination::After(MessageId::new(mid))), Some(MODMAIL::CATCH_UP_SCAN)).await };
    match bot_util::async_util::with_timeout(fetch, 5000, "modmail waiting messages").await {
        Ok(batch) => waiting_run(batch, user_id),
        Err(e) => {
            bot_util::log_debug!(format!("[modmail] could not look for waiting messages: {e}"));
            Vec::new()
        }
    }
}

/// Where [`relay_all`] delivers.
enum Target<'a> {
    Thread { thread: &'a GuildChannel, row: &'a ModmailThread, settings: &'a GuildSettings },
    Conversation(&'a Conversation),
    Nowhere,
}

async fn deliver_to(app: &App, target: &Target<'_>, message: &Message) -> Delivery {
    match target {
        Target::Thread { thread, row, settings } => {
            match relay_to_thread(app, thread, row, Some(settings), &RelayAuthor::of(&message.author), &RelayMessage::of(message)).await {
                Relay::Ok => Delivery::Ok,
                Relay::Gone => Delivery::Gone,
                _ => Delivery::Failed,
            }
        }
        Target::Conversation(conv) => deliver_dm(app, conv, message).await,
        Target::Nowhere => Delivery::Failed,
    }
}

/// What `relayAll` sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub sent: usize,
    pub failed: usize,
    pub gone: bool,
}

/// `relayAll(messages, deliver)`: one after another, each marked ✅ or ❌; after a `Gone` the rest are ❌ untried.
async fn relay_all(app: &App, messages: &[Message], target: &Target<'_>) -> Tally {
    let mut tally = Tally::default();
    for m in messages {
        let outcome = if tally.gone { Delivery::Gone } else { deliver_to(app, target, m).await };
        if outcome == Delivery::Gone {
            tally.gone = true;
        }
        mark_result(app, m, outcome == Delivery::Ok).await;
        if outcome == Delivery::Ok {
            tally.sent += 1;
        } else {
            tally.failed += 1;
        }
    }
    tally
}

/// `sentText(sent, failed, suffix)`.
pub fn sent_text(sent: usize, failed: usize, suffix: &str) -> String {
    let head = if sent > 1 { format!("Your {sent} messages were sent") } else { "Your message was sent".to_string() };
    let note = if failed > 0 { format!(" {failed} could not be delivered (marked {}).", MODMAIL::FAIL_EMOJI) } else { String::new() };
    format!("{head} {suffix}{note}")
}

fn not_found_embed() -> CreateEmbed {
    error_embed("Message Not Found", Some("I couldn't find your message any more. Please send it again."), None)
}

fn click_key(ctx: &ComponentCtx) -> String {
    ctx.interaction.message.id.to_string()
}

/// `openFromPrompt(interaction, client, guildId, messageId)`: opens a conversation for the member who answered a
/// prompt, then relays the message the prompt was about and the DMs that waited for the answer.
async fn open_from_prompt(ctx: &ComponentCtx, guild_id: &str, message_id: &str) {
    let app = &ctx.app;
    if !state(app).click_once.allow(&click_key(ctx)) {
        safe_defer_update(ctx).await;
        return;
    }
    if !safe_defer_update(ctx).await {
        return;
    }
    let user = ctx.user().clone();
    let channel_id = ctx.interaction.channel_id;
    let Some(server) = server_for(app, guild_id, user.id).await else {
        return finish_prompt(ctx, dm_text("BLOCKED")).await;
    };
    let Some(original) = fetch_original(app, channel_id, message_id, user.id).await else {
        return finish_prompt(ctx, not_found_embed()).await;
    };
    let gname = escape_markdown(&server.guild_name);
    let opened = open_conversation(app, server.guild_id, Some(server.member.clone()), user.clone(), server.settings.clone(), RelayMessage::of(&original)).await;
    match opened {
        Opened::Ok { row, thread, relayed, .. } => {
            mark_result(app, &original, relayed).await;
            let waiting = fetch_waiting(app, channel_id, message_id, user.id).await;
            let later = relay_all(app, &waiting, &Target::Thread { thread: &thread, row: &row, settings: &server.settings }).await;
            let text = sent_text(usize::from(relayed) + later.sent, later.failed, &format!("to the staff of **{gname}**. They will answer you here."));
            finish_prompt(ctx, success_embed("Conversation Opened", Some(&text), None)).await;
        }
        Opened::Refused { code: "EXISTS", row: Some(row), .. } => {
            let found = resolve_thread(app, server.guild_id, &row.thread_id).await;
            let mut messages = vec![original];
            messages.extend(fetch_waiting(app, channel_id, message_id, user.id).await);
            let tally = match &found {
                ThreadLookup::Ok(thread) => relay_all(app, &messages, &Target::Thread { thread, row: &row, settings: &server.settings }).await,
                _ => relay_all(app, &messages, &Target::Nowhere).await,
            };
            let embed = if tally.sent > 0 { success_embed("Message Sent", Some(&sent_text(tally.sent, tally.failed, "to the staff.")), None) } else { unavailable() };
            finish_prompt(ctx, embed).await;
        }
        Opened::Refused { code, .. } => finish_prompt(ctx, dm_text(code)).await,
    }
}

/// `relayFromPrompt(interaction, client, rowId, messageId)`: the member picked which open conversation a message is for.
async fn relay_from_prompt(ctx: &ComponentCtx, row_id: &str, message_id: &str) {
    let app = &ctx.app;
    if !state(app).click_once.allow(&click_key(ctx)) {
        safe_defer_update(ctx).await;
        return;
    }
    if !safe_defer_update(ctx).await {
        return;
    }
    let user = ctx.user().clone();
    let u = user.id.to_string();
    let channel_id = ctx.interaction.channel_id;
    let row = match svc::find_for_user(app.db(), &u, row_id).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[modmail] relaying for {u} failed"), bot_core::error::log_arg(&e));
            return finish_prompt(ctx, unavailable()).await;
        }
    };
    let gid = row.as_ref().filter(|r| guild_available(app, &r.guild_id)).and_then(|r| parse_id(&r.guild_id)).map(GuildId::new);
    let settings = match &row {
        Some(r) if gid.is_some() => settings_of(app, &r.guild_id).await,
        _ => None,
    };
    let (Some(row), Some(gid), Some(settings)) = (row, gid, settings) else {
        return finish_prompt(ctx, closed_conversation_embed()).await;
    };
    if row.status != ModmailStatus::Open || !is_module_enabled(Some(&settings), Some(ModuleKey::Modmail)) || is_blocked(Some(&settings), &u) {
        return finish_prompt(ctx, closed_conversation_embed()).await;
    }
    let Some(original) = fetch_original(app, channel_id, message_id, user.id).await else {
        return finish_prompt(ctx, not_found_embed()).await;
    };
    let gname = guild_name(app, gid).unwrap_or_default();
    let conv = Conversation { row, guild_id: gid, guild_name: gname.clone(), settings };
    let mut messages = vec![original];
    messages.extend(fetch_waiting(app, channel_id, message_id, user.id).await);
    let tally = relay_all(app, &messages, &Target::Conversation(&conv)).await;
    let embed = if tally.sent > 0 {
        success_embed("Message Sent", Some(&sent_text(tally.sent, tally.failed, &format!("to the staff of **{}**.", escape_markdown(&gname)))), None)
    } else if tally.gone {
        error_embed("Not Sent", Some("That conversation was closed. Send your message again to start a new one."), None)
    } else {
        error_embed("Not Sent", Some("I couldn't deliver your message. Please try again."), None)
    };
    finish_prompt(ctx, embed).await;
}

fn closed_conversation_embed() -> CreateEmbed {
    error_embed("Conversation Closed", Some("That conversation is no longer open. Send your message again to start a new one."), None)
}

fn expired(title: &str, text: &str) -> Payload {
    Payload::embed(error_embed(title, Some(text), None)).ephemeral(true)
}

/// `handleSelect(interaction, client)`: `modmail-select:<open|relay>:<messageId>`.
pub async fn handle_select(ctx: ComponentCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    let mode = params.first().cloned().unwrap_or_default();
    let message_id = params.get(1).cloned().unwrap_or_default();
    let value = ctx.values().into_iter().next().unwrap_or_default();
    if ctx.guild_id().is_some() || value.is_empty() || message_id.is_empty() {
        safe_reply(&ctx.responder, expired("Menu Expired", "This menu is no longer active.")).await;
        return Ok(());
    }
    match mode.as_str() {
        "open" => open_from_prompt(&ctx, &value, &message_id).await,
        "relay" => relay_from_prompt(&ctx, &value, &message_id).await,
        _ => {
            safe_reply(&ctx.responder, expired("Menu Expired", "This menu is no longer active.")).await;
        }
    }
    Ok(())
}

// ── Staff side: the close button and the thread relay ─────────────────────────

fn in_cached_guild(app: &App, guild_id: Option<GuildId>) -> bool {
    guild_id.is_some_and(|g| app.discord().is_some_and(|d| d.cache.guild(g).is_some()))
}

/// The "Close conversation" button in a thread's first message. Moderators only.
async fn handle_close_button(ctx: &ComponentCtx, row_id: &str) -> BotResult {
    let app = &ctx.app;
    if !in_cached_guild(app, ctx.guild_id()) {
        safe_reply(&ctx.responder, expired("Server Only", "This button only works inside the server.")).await;
        return Ok(());
    }
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    if close_button_level(ctx, guild_id).await < PermissionLevel::Moderator {
        reply_error(&ctx.responder, "Insufficient Permissions", Some("You need the Moderator level to close a conversation."), true).await;
        return Ok(());
    }
    if !safe_defer_update(ctx).await {
        return Ok(());
    }
    let channel_id = ctx.interaction.channel_id;
    let row = svc::find_in_guild(app.db(), &guild_id.to_string(), row_id).await?;
    let already = || expired("Already Closed", "This conversation is already closed.");
    let Some(row) = row.filter(|r| r.status == ModmailStatus::Open && r.thread_id == channel_id.to_string()) else {
        let _ = ctx.edit_reply(EditInteractionResponse::new().components(vec![])).await;
        safe_reply(&ctx.responder, already()).await;
        return Ok(());
    };
    let user = ctx.user().id.to_string();
    let res = close_and_notify(app, guild_id, &row, Some(&user), None, None, true).await?;
    let _ = ctx.edit_reply(EditInteractionResponse::new().components(vec![])).await;
    if !res.closed {
        safe_reply(&ctx.responder, already()).await;
        return Ok(());
    }
    let note = if res.notified { "" } else { "\nThe member could not be notified (DMs closed)." };
    safe_reply(&ctx.responder, Payload::embed(success_embed("Conversation Closed", Some(&format!("Closed by <@{user}>.{note}")), None)).no_mentions()).await;
    // `interaction.channel`: the cache, else fetched.
    let thread = bot_core::interaction_helpers::resolve_channel(app, guild_id, channel_id).await;
    archive_thread(app, guild_id, thread.as_ref()).await;
    Ok(())
}

/// The clicker's level for the close button: `peekSettings(guildId)`, else `getSettingsSafe` bounded by the whole
/// `ackBudgetMs(interaction)` (not capped at 1 s), then `resolvePermissionLevel(member, settings)`.
async fn close_button_level(ctx: &ComponentCtx, guild_id: GuildId) -> PermissionLevel {
    let app = &ctx.app;
    let g = guild_id.to_string();
    let mut settings = app.settings().peek_settings(&g);
    if settings.is_none() {
        let a = app.clone();
        let load = async move { Ok::<_, std::convert::Infallible>(a.settings().get_settings_safe(&g).await) };
        settings = bot_util::async_util::with_timeout(load, ctx.ack_budget_ms(), "modmail close settings").await.ok().flatten();
    }
    let access = ctx.member_access();
    resolve_permission_level(access.as_ref(), settings.as_deref(), app.config().bot_owner_id.as_deref())
}

/// `handleButton(interaction, client)`: `modmail:open:<guildId>:<messageId>` and `modmail:cancel` (DMs),
/// `modmail:close:<rowId>` (a thread).
pub async fn handle_button(ctx: ComponentCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    let action = params.first().map(String::as_str).unwrap_or("");
    let a = params.get(1).cloned().unwrap_or_default();
    let b = params.get(2).cloned().unwrap_or_default();
    if action == "close" && !a.is_empty() {
        return handle_close_button(&ctx, &a).await;
    }
    if ctx.guild_id().is_none() {
        if action == "open" && !a.is_empty() && !b.is_empty() {
            open_from_prompt(&ctx, &a, &b).await;
            return Ok(());
        }
        if action == "cancel" {
            let _ = ctx.update(CreateInteractionResponseMessage::new().embed(info_embed("Cancelled", Some("Nothing was sent."), None)).components(vec![])).await;
            return Ok(());
        }
    }
    safe_reply(&ctx.responder, expired("Button Expired", "This button is no longer active.")).await;
    Ok(())
}

fn remember_not_modmail(app: &App, thread_id: &str) {
    let st = state(app);
    let mut m = lock(&st.not_modmail);
    m.insert(thread_id.to_string(), app.clock().now_ms() + NOT_MODMAIL_TTL_MS);
    if m.len() > NOT_MODMAIL_MAX {
        m.shift_remove_index(0);
    }
}

/// `knownNotModmail(threadId)`: a thread already checked and found not to be a modmail thread (5 minutes).
pub fn known_not_modmail(app: &App, thread_id: &str) -> bool {
    let st = state(app);
    let mut m = lock(&st.not_modmail);
    let Some(until) = m.get(thread_id).copied() else { return false };
    if until > app.clock().now_ms() {
        return true;
    }
    m.shift_remove(thread_id);
    false
}

/// `staffMessageFilter(ctx)`: a message in a thread that is not a note, not in a thread someone else (not the bot)
/// owns, and not in a thread already checked.
pub fn staff_message_filter(app: &App, channel: Option<&GuildChannel>, content: &str) -> bool {
    let Some(channel) = channel.filter(|c| permissions::is_thread(c)) else { return false };
    if is_internal_note(Some(content)) {
        return false;
    }
    if let (Some(owner), Some(bot)) = (channel.owner_id, app.discord().map(|d| d.bot_user_id()))
        && owner != bot
    {
        return false;
    }
    !known_not_modmail(app, &channel.id.to_string())
}

fn hook_channel(app: &App, ctx: &HookCtx<MessageCreate>) -> Option<GuildChannel> {
    ctx.data.channel.clone().or_else(|| cached_channel(app, ctx.data.guild_id, ctx.data.channel_id))
}

/// `isStaffAuthor(member, settings, channel)` against the cache.
pub fn is_staff_member(app: &App, guild_id: GuildId, member: &Member, settings: &GuildSettings, channel: &GuildChannel) -> bool {
    let d = app.discord();
    let access = MemberAccess::from_cache(d.map(|d| d.cache.as_ref()), guild_id, member);
    let level = resolve_permission_level(Some(&access), Some(settings), app.config().bot_owner_id.as_deref());
    let perms = d.and_then(|d| d.cache.guild(guild_id).and_then(|g| permissions::member_channel_permissions(&g, channel, member)));
    svc::is_staff_author(Some(level), &access.role_ids, Some(settings), perms)
}

/// `handleStaffMessage(ctx)`: a message in an OPEN modmail thread goes to the member's DMs; ✅ when delivered, ❌ not.
pub async fn handle_staff_message(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let app = &ctx.app;
    let message = &ctx.data.message;
    if message.author.bot || message.webhook_id.is_some() {
        return Ok(());
    }
    let Some(thread) = hook_channel(app, &ctx) else { return Ok(()) };
    let Some(row) = svc::find_by_thread(app.db(), &thread.id.to_string()).await? else {
        remember_not_modmail(app, &thread.id.to_string());
        return Ok(());
    };
    let guild_id = ctx.data.guild_id;
    if row.status != ModmailStatus::Open || row.guild_id != guild_id.to_string() {
        return Ok(());
    }
    let relay = RelayMessage::of(message);
    if is_internal_note(Some(&message.content)) || !has_content(&relay) {
        return Ok(());
    }
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let member = match &ctx.data.member {
        Some(m) => Some(m.clone()),
        None => match app.http() {
            Some(http) => http.get_member(guild_id, message.author.id).await.ok(),
            None => None,
        },
    };
    let Some(member) = member.filter(|m| is_staff_member(app, guild_id, m, &settings, &thread)) else { return Ok(()) };
    let author = svc::staff_author(
        Some((&adapters::member_display_name(&member), Some(adapters::member_display_avatar_url_with(guild_id, &member, adapters::ImageUrlOptions::default())))),
        guild_icon(app, guild_id),
        settings.modmail_anonymous_replies,
    );
    let res = relay_to_member(app, &row, guild_id, Some(&settings), &author, &relay).await;
    react(app, Some(guild_id), thread.id, message.id, if res == Relay::Ok { MODMAIL::OK_EMOJI } else { MODMAIL::FAIL_EMOJI }).await;
    if res == Relay::DmClosed
        && let Some(http) = app.http()
    {
        let embed = error_embed("Not Delivered", Some("The member's DMs are closed, so they did not get that message. They can still write to me."), None);
        let _ = thread.id.send_message(http.as_ref(), CreateMessage::new().embed(embed).allowed_mentions(CreateAllowedMentions::new())).await;
    }
    Ok(())
}

// ── Cleanup ───────────────────────────────────────────────────────────────────

/// `onThreadDeleted(ctx)`: a deleted thread closes its conversation.
pub async fn on_thread_deleted(ctx: Arc<HookCtx<ThreadDelete>>) -> BotResult {
    let id = ctx.data.thread_id.to_string();
    let closed = svc::close_by_thread(&ctx.app, &id, THREAD_DELETED).await?;
    if closed > 0 {
        bot_util::log_info!(format!("[modmail] {}: thread {id} was deleted, conversation closed", ctx.data.guild_id));
    }
    Ok(())
}

/// `onChannelDeleted(ctx)`: the staff channel was deleted: the setting is cleared and the conversations whose thread
/// Discord says is gone are closed.
pub async fn on_channel_deleted(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let channel = ctx.data.channel.id.to_string();
    if settings.modmail_channel_id.as_deref() != Some(channel.as_str()) {
        return Ok(());
    }
    let g = ctx.data.guild_id;
    ctx.app.settings().update_settings(&g.to_string(), SettingsPatch::new().set("modmailChannelId", Value::Null)).await?;
    bot_util::log_info!(format!("[modmail] {g}: the staff channel {channel} was deleted, modmail has no channel now"));
    let rows = svc::list_open_in_guild(ctx.app.db(), &g.to_string(), 200).await?;
    let app = ctx.app.clone();
    bot_util::async_util::map_limit(rows, 3, |row, _| {
        let app = app.clone();
        async move {
            if matches!(resolve_thread(&app, g, &row.thread_id).await, ThreadLookup::Gone) {
                svc::close_by_thread(&app, &row.thread_id, "The staff channel was deleted.").await?;
            }
            Ok::<_, anyhow::Error>(())
        }
    })
    .await;
    Ok(())
}

/// The `channelDelete` filter: skipped once the memoised settings show another staff channel.
pub fn channel_delete_filter(app: &App, guild_id: &str, channel_id: &str) -> bool {
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => s.modmail_channel_id.as_deref() == Some(channel_id),
    }
}

/// The plugin (`src/plugins/modmail.js`).
pub fn plugin() -> Plugin {
    Plugin::new("modmail", Some(ModuleKey::Modmail))
        .hook(
            // A DM has no guild, so the hook cannot be module-gated: it checks every server itself.
            Hook::<DirectMessage>::new("modmail", |ctx: Arc<HookCtx<DirectMessage>>| async move {
                handle_direct_message(&ctx.app, &ctx.data.message).await;
                Ok(())
            })
            .ungated()
            .always()
            .timeout_ms(30_000)
            .filter(|ctx| ctx.data.message.guild_id.is_none() && !ctx.data.message.author.bot),
        )
        .hook(
            // HOOK_ORDER.messageCreate: after autotime.
            Hook::<MessageCreate>::new("modmail", handle_staff_message)
                .filter(|ctx| staff_message_filter(&ctx.app, hook_channel(&ctx.app, ctx).as_ref(), &ctx.data.message.content)),
        )
        .hook(
            Hook::<ChannelDelete>::new("modmail", on_channel_deleted)
                .always()
                .timeout_ms(30_000)
                .filter(|ctx| channel_delete_filter(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string())),
        )
        .hook(Hook::<ThreadDelete>::new("modmail", on_thread_deleted).always())
        // A DM has no guild, so there is no module toggle to read; every handler re-checks the server.
        .button("modmail", handle_button)
        .component_module(ModuleRef::Ungated)
        .select("modmail-select", handle_select)
        .component_module(ModuleRef::Ungated)
}
