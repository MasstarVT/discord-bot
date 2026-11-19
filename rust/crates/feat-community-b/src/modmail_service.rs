//! Port of `src/services/modmailService.js`: the modmail rules that need no Discord connection and the
//! `ModmailThread` table access.
//!
//! Members DM the bot and every conversation becomes a thread in the staff channel (`GuildSettings.modmailChannelId`).
//! At most one OPEN row exists per (guild, member): a row is reserved under a per-(guild, member) advisory lock BEFORE
//! the thread is created, with a placeholder thread id, and the real id is attached afterwards. The Discord work
//! (opening, relaying, closing) is in [`crate::plugins::modmail`].

use bot_config::{ModuleKey, PermissionLevel};
use bot_core::App;
use bot_core::settings::is_module_enabled;
use bot_db::{GuildSettings, MODMAIL_THREAD_INFO, ModmailThread, PgPool, SelectBuilder, SqlValue, UpdateBuilder};
use bot_util::text::{clamp_text, clamp_text_with, join_with_limit, sanitize_name};
use serde_json::json;
use serenity::all::Permissions;

/// The greeting sent while `GuildSettings.modmailGreeting` is null (the same text as `DEFAULT_TEXTS.modmailGreeting`).
pub const DEFAULT_GREETING: &str = "Thanks for your message! The staff of **{server}** will answer you here soon.";

/// `MODMAIL`.
pub mod limits {
    /// A staff message that starts with this stays in the thread: an internal note.
    pub const NOTE_PREFIX: &str = "//";
    pub const OK_EMOJI: &str = "✅";
    pub const FAIL_EMOJI: &str = "❌";
    /// On a DM that arrived while a "which server?" prompt was open: it is sent once the member answers it.
    pub const WAIT_EMOJI: &str = "⏳";
    /// Answering a prompt also sends at most this many of the member's DMs that waited for it.
    pub const CATCH_UP_MAX: usize = 10;
    /// Messages looked at after the prompting one (the bot's own prompts sit in between).
    pub const CATCH_UP_SCAN: u8 = 30;
    /// A select menu holds at most 25 options.
    pub const PICK_MAX: usize = 25;
    /// Placeholder thread id of a row whose thread is still being created.
    pub const PENDING_PREFIX: &str = "pending-";
    /// A placeholder this old belongs to an attempt that died; the next open replaces it.
    pub const PENDING_STALE_MS: i64 = 2 * 60_000;
    /// One member fetch while looking for the servers a member shares with the bot.
    pub const MEMBER_FETCH_TIMEOUT_MS: u64 = 2_500;
    /// No new server is looked at after this long (the hook has a 10 s limit).
    pub const SCAN_BUDGET_MS: i64 = 6_000;
    pub const SCAN_CONCURRENCY: usize = 5;
    /// Embed description cap.
    pub const TEXT_MAX: usize = 4000;
    pub const THREAD_NAME_MAX: usize = 100;
    pub const REASON_MAX: usize = 500;
    /// A member's relayed messages: at most RATE_MAX per RATE_WINDOW_MS.
    pub const RATE_MAX: u32 = 10;
    pub const RATE_WINDOW_MS: i64 = 10_000;
    /// The "open a conversation?" question is asked at most this often per member.
    pub const PROMPT_EVERY_MS: u64 = 8_000;
    /// Rows listed by /modmail blocked.
    pub const BLOCKED_LIST_MAX: usize = 50;
}

/// The staff-side permission that makes a member count as staff in a modmail thread.
pub const STAFF_THREAD_PERMISSION: Permissions = Permissions::MANAGE_THREADS;

/// The note a staff member sees under the "who" lines.
pub const STAFF_HELP: &str = "Messages you write here go to the member. Start one with `//` for an internal note. End the conversation with the button below or `/modmail close`.";

// ── Pure rules ────────────────────────────────────────────────────────────────

/// `isInternalNote(content)`: staff messages starting with `//` are internal notes, never sent to the member.
pub fn is_internal_note(content: Option<&str>) -> bool {
    content.is_some_and(|c| bot_util::js::trim_start(c).starts_with(limits::NOTE_PREFIX))
}

/// `isBlocked(settings, userId)`.
pub fn is_blocked(settings: Option<&GuildSettings>, user_id: &str) -> bool {
    settings.is_some_and(|s| s.modmail_blocked_user_ids.iter().any(|id| id == user_id))
}

/// `acceptsConversations(settings, userId)`: module on, a staff channel set, member not blocked.
pub fn accepts_conversations(settings: Option<&GuildSettings>, user_id: &str) -> bool {
    is_module_enabled(settings, Some(ModuleKey::Modmail))
        && settings.and_then(|s| s.modmail_channel_id.as_deref()).is_some_and(|c| !c.is_empty())
        && !is_blocked(settings, user_id)
}

/// `isPendingThreadId(threadId)`: a placeholder thread id (a row whose thread is still being created).
pub fn is_pending_thread_id(thread_id: &str) -> bool {
    thread_id.starts_with(limits::PENDING_PREFIX)
}

/// What the relay reads from a message: its text, attachments (`name`, `url`) and sticker names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayMessage {
    pub content: String,
    pub attachments: Vec<(Option<String>, String)>,
    pub stickers: Vec<Option<String>>,
}

impl RelayMessage {
    /// Just text (`{ content: text }`, what `/modmail reply` relays).
    pub fn text(content: impl Into<String>) -> Self {
        Self { content: content.into(), ..Default::default() }
    }

    pub fn of(message: &serenity::all::Message) -> Self {
        Self {
            content: message.content.clone(),
            attachments: message.attachments.iter().map(|a| (Some(a.filename.clone()), a.url.clone())).collect(),
            stickers: message.sticker_items.iter().map(|s| Some(s.name.clone())).collect(),
        }
    }
}

/// Markdown-safe link text: brackets would end the link early.
fn link_text(name: Option<&str>) -> String {
    let cleaned: String = name.unwrap_or("file").chars().filter(|c| !matches!(c, '[' | ']' | '\\' | '`' | '*' | '_' | '~' | '|' | '<' | '>')).collect();
    let trimmed = bot_util::js::trim(&cleaned);
    clamp_text_with(if trimmed.is_empty() { "file" } else { trimmed }, 60, "")
}

/// `attachmentLines(message)`: one line per attachment and sticker, `[name](url)` links and `[Sticker: name]`.
pub fn attachment_lines(message: &RelayMessage) -> Vec<String> {
    let mut lines = Vec::new();
    for (name, url) in &message.attachments {
        if !url.is_empty() {
            lines.push(format!("[{}]({url})", link_text(name.as_deref())));
        }
    }
    for s in &message.stickers {
        lines.push(format!("[Sticker: {}]", link_text(Some(s.as_deref().unwrap_or("sticker")))));
    }
    lines
}

/// `hasContent(message)`: whether a message has anything to relay (text, a file or a sticker).
pub fn has_content(message: &RelayMessage) -> bool {
    !bot_util::js::trim(&message.content).is_empty() || !attachment_lines(message).is_empty()
}

/// The fields of a relayed message embed (`relayParts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayParts {
    pub description: String,
    pub attachments: Option<String>,
}

/// `relayParts(message)`: the text (cut to fit) and the "Attachments" field.
pub fn relay_parts(message: &RelayMessage) -> RelayParts {
    let text = bot_util::js::trim(&message.content);
    let lines = attachment_lines(message);
    RelayParts {
        description: clamp_text(text, limits::TEXT_MAX),
        attachments: if lines.is_empty() { None } else { Some(join_with_limit(&lines, "\n", 1000, bot_util::text::default_more)) },
    }
}

/// `threadName(user)`: `modmail-<username>`, lowercase and dash-safe, at most 100 characters.
pub fn thread_name(username: Option<&str>) -> String {
    clamp_text_with(&format!("modmail-{}", sanitize_name(username.unwrap_or(""), 40, "member")), limits::THREAD_NAME_MAX, "")
}

/// Who speaks for staff in the member's DMs (`staffAuthor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaffAuthor {
    pub name: String,
    pub icon_url: Option<String>,
}

/// `staffAuthor(member, guild, anonymous)`: the staff member's name, or "Staff" when replies are anonymous.
/// `member` is `(display name, avatar url)`; `guild_icon` is the server icon.
pub fn staff_author(member: Option<(&str, Option<String>)>, guild_icon: Option<String>, anonymous: bool) -> StaffAuthor {
    match member {
        Some((name, icon)) if !anonymous => StaffAuthor { name: clamp_text(name, 100), icon_url: icon },
        _ => StaffAuthor { name: "Staff".into(), icon_url: guild_icon },
    }
}

/// `isStaffAuthor(member, settings, channel)`'s decision: a moderator (by level), a holder of one of the ping roles,
/// or someone who can manage threads in the channel (`channel_perms`, the member's permissions there).
pub fn is_staff_author(level: Option<PermissionLevel>, member_role_ids: &[String], settings: Option<&GuildSettings>, channel_perms: Option<Permissions>) -> bool {
    let Some(level) = level else { return false };
    if level >= PermissionLevel::Moderator {
        return true;
    }
    if settings.is_some_and(|s| s.modmail_ping_role_ids.iter().any(|id| member_role_ids.iter().any(|r| r == id))) {
        return true;
    }
    channel_perms.is_some_and(|p| bot_core::permissions::js_has(p, STAFF_THREAD_PERMISSION))
}

/// `whoLines(user, member)`: the "who" lines of a new thread's first message (times in epoch ms).
pub fn who_lines(user_id: &str, user_created_ms: Option<f64>, member_joined_ms: Option<f64>) -> Vec<String> {
    let mut lines = vec![format!("**Member:** <@{user_id}> (`{user_id}`)")];
    if let Some(c) = user_created_ms.filter(|c| *c != 0.0) {
        lines.push(format!("**Account created:** <t:{}:R>", (c / 1000.0).floor() as i64));
    }
    if let Some(j) = member_joined_ms.filter(|j| *j != 0.0) {
        lines.push(format!("**Joined the server:** <t:{}:R>", (j / 1000.0).floor() as i64));
    }
    lines
}

// ── Database ──────────────────────────────────────────────────────────────────

/// The advisory lock key of `openLockSql(guildId, userId)` (`pg_advisory_xact_lock(hashtext(key))`).
pub fn open_lock_key(guild_id: &str, user_id: &str) -> String {
    format!("modmail-open:{guild_id}:{user_id}")
}

/// The (guild, member) open lock of this process (`createKeyedMutex()`), one per app.
#[derive(Default)]
pub struct OpenMutex(pub bot_core::sync::KeyedMutex);

/// `runOpenLocked(guildId, userId, fn)`: opens of one member in one guild run one at a time in this process too.
pub fn run_open_locked<T, F, Fut>(app: &App, guild_id: &str, user_id: &str, f: F) -> impl std::future::Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    app.ext::<OpenMutex>().0.run(&format!("{guild_id}:{user_id}"), f)
}

/// `findByThread(threadId)`: the row for a Discord thread id (any status).
pub async fn find_by_thread(pool: &PgPool, thread_id: &str) -> anyhow::Result<Option<ModmailThread>> {
    let q = SelectBuilder::new(&MODMAIL_THREAD_INFO).filter("threadId", json!(thread_id))?.build()?;
    Ok(q.fetch_optional(pool).await?)
}

/// `findInGuild(guildId, id)`: a row by its id inside a guild (an id from another guild is simply not found).
pub async fn find_in_guild(pool: &PgPool, guild_id: &str, id: &str) -> anyhow::Result<Option<ModmailThread>> {
    let q = SelectBuilder::new(&MODMAIL_THREAD_INFO).filter("id", json!(id))?.filter("guildId", json!(guild_id))?.limit(1).build()?;
    Ok(q.fetch_optional(pool).await?)
}

/// `findForUser(userId, id)`: a row by id for a member (the DM buttons carry row ids).
pub async fn find_for_user(pool: &PgPool, user_id: &str, id: &str) -> anyhow::Result<Option<ModmailThread>> {
    let q = SelectBuilder::new(&MODMAIL_THREAD_INFO).filter("id", json!(id))?.filter("userId", json!(user_id))?.limit(1).build()?;
    Ok(q.fetch_optional(pool).await?)
}

/// `findOpenInGuild(guildId, userId)`: the OPEN conversation of a member in one guild that has a real thread.
pub async fn find_open_in_guild(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<Option<ModmailThread>> {
    let q = SelectBuilder::new(&MODMAIL_THREAD_INFO)
        .filter("guildId", json!(guild_id))?
        .filter("userId", json!(user_id))?
        .filter("status", json!("OPEN"))?
        .order_by("lastActivityAt", false)?
        .limit(5)
        .build()?;
    let rows: Vec<ModmailThread> = q.fetch_all(pool).await?;
    Ok(rows.into_iter().find(|r| !is_pending_thread_id(&r.thread_id)))
}

/// `findOpenForUser(userId, limit = PICK_MAX)`: the member's OPEN conversations with a real thread, most recent first.
pub async fn find_open_for_user(pool: &PgPool, user_id: &str, limit: usize) -> anyhow::Result<Vec<ModmailThread>> {
    let q = SelectBuilder::new(&MODMAIL_THREAD_INFO)
        .filter("userId", json!(user_id))?
        .filter("status", json!("OPEN"))?
        .order_by("lastActivityAt", false)?
        .limit(limit as i64 + 5)
        .build()?;
    let rows: Vec<ModmailThread> = q.fetch_all(pool).await?;
    Ok(rows.into_iter().filter(|r| !is_pending_thread_id(&r.thread_id)).take(limit).collect())
}

/// What [`reserve_open`] found or made.
#[derive(Debug, Clone, PartialEq)]
pub enum Reserved {
    /// A new row with a placeholder thread id.
    Row(ModmailThread),
    /// The member's OPEN row in that guild (a real thread, or a placeholder still being created).
    Existing(ModmailThread),
}

/// `reserveOpen(guildId, userId)`: reserves the one OPEN row of a member in a guild, under an advisory lock. An
/// existing OPEN row is returned instead of a new one; a placeholder left by an attempt that died is replaced.
pub async fn reserve_open(app: &App, guild_id: &str, user_id: &str) -> anyhow::Result<Reserved> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let (g, u) = (guild_id.to_string(), user_id.to_string());
    let now_ms = app.clock().now_ms();
    let reserved = bot_db::transaction(app.db(), bot_db::TxOptions::new(5_000, 10_000), move |tx| {
        Box::pin(async move {
            bot_db::advisory_xact_lock(tx, &open_lock_key(&g, &u)).await?;
            let q = SelectBuilder::new(&MODMAIL_THREAD_INFO).filter("guildId", json!(g))?.filter("userId", json!(u))?.filter("status", json!("OPEN"))?.build()?;
            let open: Vec<ModmailThread> = q.fetch_all(&mut *tx).await?;
            for row in open {
                let stale = is_pending_thread_id(&row.thread_id) && now_ms - row.created_at.timestamp_millis() > limits::PENDING_STALE_MS;
                if stale {
                    bot_db::delete_where(&MODMAIL_THREAD_INFO, [("id", json!(row.id))])?.execute(&mut *tx).await?;
                } else {
                    return Ok::<_, bot_db::DbError>(Reserved::Existing(row));
                }
            }
            let row: ModmailThread = bot_db::InsertBuilder::new(&MODMAIL_THREAD_INFO)
                .values([
                    ("guildId", json!(g)),
                    ("userId", json!(u)),
                    ("threadId", json!(format!("{}{}", limits::PENDING_PREFIX, bot_db::new_uuid()))),
                ])?
                .build()?
                .fetch_one(&mut *tx)
                .await?;
            Ok(Reserved::Row(row))
        })
    })
    .await?;
    Ok(reserved)
}

/// `attachThread(id, threadId)`: points a reserved row at its real thread.
pub async fn attach_thread(app: &App, id: &str, thread_id: &str) -> anyhow::Result<ModmailThread> {
    let q = UpdateBuilder::new(&MODMAIL_THREAD_INFO)
        .set("threadId", json!(thread_id))?
        .set_typed("lastActivityAt", SqlValue::timestamp(app.clock().now()))?
        .filter("id", json!(id))?
        .build()?;
    Ok(q.fetch_one(app.db()).await?)
}

/// `releaseReserved(id)`: drops a reserved row whose thread could not be created. Never fails.
pub async fn release_reserved(pool: &PgPool, id: &str) {
    let res = match bot_db::delete_where(&MODMAIL_THREAD_INFO, [("id", json!(id))]) {
        Ok(q) => q.execute(pool).await.map(|_| ()),
        Err(e) => Err(e),
    };
    if let Err(e) = res {
        bot_util::log_warn!(format!("[modmail] could not drop a reserved row: {e}"));
    }
}

/// `closeRow(id, { closedById, reason })`: OPEN -> CLOSED, once. A second caller gets `false`.
pub async fn close_row(app: &App, id: &str, closed_by_id: Option<&str>, reason: Option<&str>) -> anyhow::Result<bool> {
    let reason = reason.filter(|r| !r.is_empty()).map(|r| clamp_text(r, limits::REASON_MAX));
    let q = UpdateBuilder::new(&MODMAIL_THREAD_INFO)
        .set("status", json!("CLOSED"))?
        .set("closedById", json!(closed_by_id))?
        .set_typed("closedAt", SqlValue::timestamp(app.clock().now()))?
        .set("closeReason", json!(reason))?
        .filter("id", json!(id))?
        .filter("status", json!("OPEN"))?
        .without_returning()
        .build()?;
    Ok(q.execute(app.db()).await? > 0)
}

/// The reason [`close_by_thread`] records by default.
pub const THREAD_DELETED: &str = "The thread was deleted.";

/// `closeByThread(threadId, reason)`: closes the OPEN row of a deleted thread; the number of rows closed.
pub async fn close_by_thread(app: &App, thread_id: &str, reason: &str) -> anyhow::Result<u64> {
    let q = UpdateBuilder::new(&MODMAIL_THREAD_INFO)
        .set("status", json!("CLOSED"))?
        .set_typed("closedAt", SqlValue::timestamp(app.clock().now()))?
        .set("closeReason", json!(reason))?
        .filter("threadId", json!(thread_id))?
        .filter("status", json!("OPEN"))?
        .without_returning()
        .build()?;
    Ok(q.execute(app.db()).await?)
}

/// `listOpenInGuild(guildId, limit = 200)`: the OPEN rows of a guild.
pub async fn list_open_in_guild(pool: &PgPool, guild_id: &str, limit: i64) -> anyhow::Result<Vec<ModmailThread>> {
    let q = SelectBuilder::new(&MODMAIL_THREAD_INFO).filter("guildId", json!(guild_id))?.filter("status", json!("OPEN"))?.limit(limit).build()?;
    Ok(q.fetch_all(pool).await?)
}

/// `touch(id)` awaited: keeps `lastActivityAt` current. Never fails.
pub async fn touch_now(app: &App, id: &str) {
    let res = async {
        UpdateBuilder::new(&MODMAIL_THREAD_INFO)
            .set_typed("lastActivityAt", SqlValue::timestamp(app.clock().now()))?
            .filter("id", json!(id))?
            .without_returning()
            .build()?
            .execute(app.db())
            .await
    }
    .await;
    if let Err(e) = res {
        bot_util::log_debug!(format!("[modmail] touch failed: {e}"));
    }
}

/// `touch(id)`: fire and forget.
pub fn touch(app: &App, id: &str) {
    let (app, id) = (app.clone(), id.to_string());
    tokio::spawn(async move { touch_now(&app, &id).await });
}
