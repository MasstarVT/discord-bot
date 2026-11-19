//! Port of `src/services/suggestionService.js`: Suggestions (beyond MEE6).
//!
//! Members post ideas with `/suggestion submit`, everyone votes with the Up/Down buttons and staff approve, deny,
//! consider or implement them. This file holds the rules: the post (embed + buttons), the numbered row (per-guild
//! number and cooldown under one advisory lock), the vote transaction (row lock, one vote per member, totals
//! recounted from SuggestionVote), the debounced message edit, the review (status, note, reviewer, author DM) and the
//! delete. [`crate::plugins::suggestions`] declares the hooks and components; [`crate::commands::suggestion`] is the
//! command.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use bot_config::colors;
use bot_core::deletion_tracker::mark_bot_deletion;
use bot_core::discord::GuildState;
use bot_core::interaction_helpers::{Interactive, Payload, PermTarget, cached_channel, cached_guild, missing_bot_permissions, safe_defer, safe_reply};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_db::{InsertBuilder, PgPool, SUGGESTION_INFO, SUGGESTION_VOTE_INFO, Suggestion, SuggestionStatus, UpdateBuilder};
use bot_util::text::clamp_text;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use serenity::all::{ActionRowComponent, ButtonStyle, ChannelId, GuildId, InputTextStyle, MessageId, ReactionType, UserId};
use serenity::builder::{CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateInputText, CreateModal};

use crate::io::{IoError, io, snowflake};

/// `SUGGESTION.RENDER_INTERVAL_MS`: message edits per suggestion: one at once, then at most one more per window.
pub const RENDER_INTERVAL_MS: u64 = 2000;
/// `SUGGESTION.NOTE_MAX`: the review note (also the length of the command option; an embed field holds 1024).
pub const NOTE_MAX: usize = 1000;
/// `SUGGESTION.ANONYMOUS_NAME`.
pub const ANONYMOUS_NAME: &str = "Anonymous";

const RENDER_SWEEP_AT: usize = 500;
const RENDER_IDLE_MS: i64 = 10 * 60_000;
const WARN_EVERY_MS: i64 = 10 * 60_000;
const UNKNOWN_CHANNEL: u64 = 10003;
const UNKNOWN_MESSAGE: u64 = 10008;
/// Missing access / permissions / archived thread.
const NO_ACCESS_CODES: [u64; 3] = [50001, 50013, 50083];
const POST_NEEDS: [&str; 3] = ["ViewChannel", "SendMessages", "EmbedLinks"];

/// One row of `STATUS_INFO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusInfo {
    pub label: &'static str,
    pub emoji: &'static str,
    pub color: u32,
    pub dm: &'static str,
}

/// `STATUS_INFO[status]`.
pub fn status_info(status: SuggestionStatus) -> StatusInfo {
    match status {
        SuggestionStatus::Open => StatusInfo { label: "Open", emoji: "🗳️", color: colors::INFO, dm: "is open for votes" },
        SuggestionStatus::Considered => StatusInfo { label: "Under consideration", emoji: "🤔", color: colors::WARNING, dm: "is under consideration" },
        SuggestionStatus::Approved => StatusInfo { label: "Approved", emoji: "✅", color: colors::SUCCESS, dm: "was approved" },
        SuggestionStatus::Denied => StatusInfo { label: "Denied", emoji: "❌", color: colors::ERROR, dm: "was denied" },
        SuggestionStatus::Implemented => StatusInfo { label: "Implemented", emoji: "🚀", color: 0x9b59b6, dm: "was implemented" },
    }
}

/// `CLOSED_STATUSES`: voting closes once staff decided; CONSIDERED keeps it open.
pub const CLOSED_STATUSES: [SuggestionStatus; 3] = [SuggestionStatus::Approved, SuggestionStatus::Denied, SuggestionStatus::Implemented];

/// `isVotingOpen(status)`.
pub fn is_voting_open(status: SuggestionStatus) -> bool {
    !CLOSED_STATUSES.contains(&status)
}

// ── Raw SQL ───────────────────────────────────────────────────────────────────

/// `numberLockSql(guildId)`: `$1` is `suggestion-number:<guildId>`.
pub const NUMBER_LOCK_SQL: &str = "SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext($1))";
/// `nextNumberSql(guildId)`.
pub const NEXT_NUMBER_SQL: &str = r#"SELECT (COALESCE(MAX("number"), 0) + 1)::int AS next FROM "Suggestion" WHERE "guildId" = $1"#;
/// `lockRowSql(id)`.
pub const LOCK_ROW_SQL: &str = r#"SELECT "status"::text AS status FROM "Suggestion" WHERE "id" = $1 FOR UPDATE"#;

/// `RAW_SQL`: every raw statement with sample parameters `(name, sql, binds)`.
pub fn raw_sql() -> Vec<(&'static str, &'static str, Vec<Value>)> {
    vec![
        ("suggestions.numberLock", NUMBER_LOCK_SQL, vec![json!("suggestion-number:0")]),
        ("suggestions.nextNumber", NEXT_NUMBER_SQL, vec![json!("0")]),
        ("suggestions.lockRow", LOCK_ROW_SQL, vec![json!("0")]),
    ]
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// Why `parseContent` refused a text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentError {
    Empty,
    TooLong,
}

/// `parseContent(raw)`: trims and enforces `LIMITS.SUGGESTION_LENGTH`.
pub fn parse_content(raw: Option<&str>) -> Result<String, ContentError> {
    let content = bot_util::js::trim(raw.unwrap_or(""));
    if content.is_empty() {
        return Err(ContentError::Empty);
    }
    if bot_util::js::utf16_len(content) > bot_config::limits::SUGGESTION_LENGTH {
        return Err(ContentError::TooLong);
    }
    Ok(content.to_string())
}

/// `cooldownRemainingMs(lastAt, cooldownSeconds, now)`: milliseconds until the member may post again (0 = now).
pub fn cooldown_remaining_ms(last_at: Option<&DateTime<Utc>>, cooldown_seconds: f64, now_ms: i64) -> i64 {
    let Some(last) = last_at else { return 0 };
    if !cooldown_seconds.is_finite() || cooldown_seconds <= 0.0 {
        return 0;
    }
    let until = last.timestamp_millis() as f64 + cooldown_seconds * 1000.0 - now_ms as f64;
    until.max(0.0) as i64
}

/// The result of a vote toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteResult {
    Added,
    Switched,
    Removed,
}

impl VoteResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Switched => "switched",
            Self::Removed => "removed",
        }
    }
}

/// `toggleVote(current, value)`: one vote per member; the same vote again removes it, the other one switches it.
/// Returns `(next, result)`.
pub fn toggle_vote(current: i32, value: i32) -> (i32, VoteResult) {
    if current == value {
        return (0, VoteResult::Removed);
    }
    (value, if current == 0 { VoteResult::Added } else { VoteResult::Switched })
}

/// `jumpUrl(s)`: the link to the post, or `None` when the message is gone.
pub fn jump_url(s: &Suggestion) -> Option<String> {
    s.message_id.as_ref().map(|m| format!("https://discord.com/channels/{}/{}/{m}", s.guild_id, s.channel_id))
}

/// `moduleOffEmbed()`: the router's wording for a member command used while the module is off.
pub fn module_off_embed() -> CreateEmbed {
    warn_embed(
        "Module Disabled",
        Some("The **Suggestions** module is turned off on this server. An admin can enable it with `/module enable suggestions`."),
        None,
    )
}

fn no_channel_embed() -> CreateEmbed {
    error_embed("No Suggestion Channel", Some("Suggestions are not set up yet. An admin can pick a channel with `/suggestion setup`."), None)
}

fn anonymous_not_allowed() -> CreateEmbed {
    error_embed("Anonymous Not Allowed", Some("This server does not allow anonymous suggestions. Leave `anonymous` off."), None)
}

/// `submitProblem(settings, { anonymous })`: what stops a member from submitting, from settings alone.
pub fn submit_problem(settings: &bot_db::GuildSettings, anonymous: bool) -> Option<CreateEmbed> {
    if !is_module_enabled(Some(settings), Some(ModuleKey::Suggestions)) {
        return Some(module_off_embed());
    }
    if settings.suggestion_channel_id.as_deref().is_none_or(str::is_empty) {
        return Some(no_channel_embed());
    }
    if anonymous && !settings.suggestion_allow_anonymous {
        return Some(anonymous_not_allowed());
    }
    None
}

/// `cooldownEmbed(ms)`.
pub fn cooldown_embed(ms: i64, now_ms: i64) -> CreateEmbed {
    let when = bot_util::time::discord_timestamp_ms((now_ms + ms) as f64, "R");
    warn_embed("Slow Down", Some(&format!("You can post another suggestion {when}.")), None)
}

// ── Message ───────────────────────────────────────────────────────────────────

/// A name and avatar for the post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub name: String,
    pub icon_url: Option<String>,
}

fn ts(date: &DateTime<Utc>) -> serenity::all::Timestamp {
    serenity::all::Timestamp::from(*date)
}

/// `buildSuggestionEmbed(s, { author })`: the post. An anonymous suggestion shows no name, avatar or id anywhere.
pub fn build_suggestion_embed(s: &Suggestion, author: Option<&Author>) -> CreateEmbed {
    let info = status_info(s.status);
    let mut embed = CreateEmbed::new()
        .color(info.color)
        .title(format!("💡 Suggestion #{}", s.number))
        .description(clamp_text(&s.content, 4000))
        .field("Status", format!("{} {}", info.emoji, info.label), true)
        .field("Votes", format!("👍 {}  ·  👎 {}", s.upvotes, s.downvotes), true)
        .timestamp(ts(&s.created_at));
    if s.anonymous {
        embed = embed.author(CreateEmbedAuthor::new(ANONYMOUS_NAME));
    } else if let Some(a) = author.filter(|a| !a.name.is_empty()) {
        let mut au = CreateEmbedAuthor::new(clamp_text(&a.name, 256));
        if let Some(icon) = a.icon_url.as_deref().filter(|i| !i.is_empty()) {
            au = au.icon_url(icon);
        }
        embed = embed.author(au);
    }
    if s.reviewed_at.is_some() || s.reviewer_id.is_some() {
        let note = s.review_note.as_deref().filter(|n| !n.is_empty()).map(|n| clamp_text(n, NOTE_MAX)).unwrap_or_else(|| "*No note.*".into());
        let by = s.reviewer_id.as_deref().map(|r| format!("\n— <@{r}>")).unwrap_or_default();
        embed = embed.field(format!("{} Staff response", info.emoji), format!("{note}{by}"), false);
    }
    embed
}

/// `buildSuggestionComponents(s)`: Up and Down buttons carrying the counts; disabled once voting is closed.
pub fn build_suggestion_components(s: &Suggestion) -> Vec<CreateActionRow> {
    let closed = !is_voting_open(s.status);
    let button = |dir: &str, emoji: &str, count: i32| {
        let id = bot_config::custom_id!("suggestion", s.id, dir).unwrap_or_default();
        CreateButton::new(id).style(ButtonStyle::Secondary).emoji(ReactionType::Unicode(emoji.into())).label(count.to_string()).disabled(closed)
    };
    vec![CreateActionRow::Buttons(vec![button("up", "👍", s.upvotes), button("down", "👎", s.downvotes)])]
}

/// `buildSuggestionPayload(s, opts)`.
pub fn build_suggestion_payload(s: &Suggestion, author: Option<&Author>) -> Payload {
    Payload::embed(build_suggestion_embed(s, author)).components(build_suggestion_components(s))
}

/// `buildSuggestionModal(anonymous)`: the form `/suggestion submit` opens without text.
pub fn build_suggestion_modal(anonymous: bool) -> CreateModal {
    let id = bot_config::custom_id!("suggestion-modal", if anonymous { "1" } else { "0" }).unwrap_or_default();
    let field = bot_config::custom_id!("suggestion-text").unwrap_or_default();
    CreateModal::new(id, "Make a suggestion").components(vec![CreateActionRow::InputText(
        CreateInputText::new(InputTextStyle::Paragraph, "Your suggestion", field)
            .required(true)
            .min_length(1)
            .max_length(bot_config::limits::SUGGESTION_LENGTH as u16)
            .placeholder("What would you like to see on this server?"),
    )])
}

/// `buildReviewDm(guildName, s)`: the DM an author gets when staff change their suggestion.
pub fn build_review_dm(guild_name: Option<&str>, s: &Suggestion, now: &DateTime<Utc>) -> CreateEmbed {
    let info = status_info(s.status);
    let mut lines = vec![format!("> {}", clamp_text(&s.content, 300).replace('\n', "\n> "))];
    if let Some(n) = s.review_note.as_deref().filter(|n| !n.is_empty()) {
        lines.push(String::new());
        lines.push(format!("**Staff note:** {}", clamp_text(n, NOTE_MAX)));
    }
    if let Some(url) = jump_url(s) {
        lines.push(String::new());
        lines.push(format!("[View the suggestion]({url})"));
    }
    CreateEmbed::new()
        .color(info.color)
        .title(clamp_text(&format!("{} Your suggestion #{} {}", info.emoji, s.number, info.dm), 256))
        .description(lines.join("\n"))
        .footer(CreateEmbedFooter::new(clamp_text(guild_name.unwrap_or("Suggestions"), 100)))
        .timestamp(ts(now))
}

// ── Discord helpers ───────────────────────────────────────────────────────────

/// `authorFor(client, guild, s)`: name and avatar for the post. Anonymous posts never look the author up.
pub async fn author_for(app: &App, guild_id: GuildId, s: &Suggestion) -> Author {
    if s.anonymous {
        return Author { name: ANONYMOUS_NAME.into(), icon_url: None };
    }
    let Some(uid) = snowflake(&s.author_id).map(UserId::new) else { return Author { name: "Unknown member".into(), icon_url: None } };
    if let Some(member) = cached_guild(app, guild_id).and_then(|g| g.members.get(&uid).cloned()) {
        return Author {
            name: bot_core::adapters::member_display_name(&member),
            // `member.displayAvatarURL()`: discord.js's defaults (webp, gif when animated, no size).
            icon_url: Some(bot_core::adapters::member_display_avatar_url_with(guild_id, &member, bot_core::adapters::ImageUrlOptions::default())),
        };
    }
    let cached = app.discord().and_then(|d| d.cache.user(uid).map(|u| u.clone()));
    let user = match cached {
        Some(u) => Some(u),
        None => io(app).fetch_user(uid).await.ok(),
    };
    match user {
        Some(u) => Author { name: bot_core::adapters::user_display_name(&u), icon_url: Some(bot_core::adapters::display_avatar_url_default(&u)) },
        None => Author { name: "Unknown member".into(), icon_url: None },
    }
}

/// What `resolveChannel` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Channel(ChannelId),
    /// REST said 10003.
    Gone,
    NoAccess,
}

/// `resolveChannel(client, channelId)`: the cache, else a REST fetch.
pub async fn resolve_channel(app: &App, guild_id: GuildId, channel_id: &str) -> Result<Resolved, IoError> {
    let Some(c) = snowflake(channel_id).map(ChannelId::new) else { return Ok(Resolved::Gone) };
    if cached_channel(app, guild_id, c).is_some() {
        return Ok(Resolved::Channel(c));
    }
    match io(app).fetch_channel(c).await {
        Ok(()) => Ok(Resolved::Channel(c)),
        Err(e) if e.code == Some(UNKNOWN_CHANNEL) => Ok(Resolved::Gone),
        Err(e) if e.code.is_some_and(|c| NO_ACCESS_CODES.contains(&c)) => Ok(Resolved::NoAccess),
        Err(e) => Err(e),
    }
}

fn guild_available(app: &App, guild_id: &str) -> bool {
    app.guild_directory().guild_state(guild_id) == GuildState::Available
}

/// `sendFailureReason(err)`.
pub fn send_failure_reason(code: Option<u64>) -> String {
    match code {
        Some(50013) | Some(50001) => "I'm missing permissions there.".into(),
        Some(50083) => "that thread is archived.".into(),
        Some(UNKNOWN_CHANNEL) => "the channel no longer exists.".into(),
        Some(c) => format!("Discord error {c}."),
        None => "Discord error unknown.".into(),
    }
}

async fn delete_row_quietly(pool: &PgPool, id: &str) {
    if let Err(e) = sqlx::query(r#"DELETE FROM "Suggestion" WHERE "id" = $1"#).bind(id).execute(pool).await {
        bot_util::log_warn!("[suggestions] could not remove a reserved suggestion row", bot_core::error::log_arg(&anyhow::Error::from(e)));
    }
}

// ── Reads ─────────────────────────────────────────────────────────────────────

/// `prisma.suggestion.findUnique({ where: { id } })`.
pub async fn find_by_id(pool: &PgPool, id: &str) -> anyhow::Result<Option<Suggestion>> {
    Ok(sqlx::query_as::<_, Suggestion>(r#"SELECT * FROM "Suggestion" WHERE "id" = $1"#).bind(id).fetch_optional(pool).await.map_err(bot_db::DbError::from)?)
}

/// `prisma.suggestion.findUnique({ where: { guildId_number } })`.
pub async fn find_by_number(pool: &PgPool, guild_id: &str, number: i64) -> anyhow::Result<Option<Suggestion>> {
    let Ok(n) = i32::try_from(number) else { return Ok(None) };
    Ok(sqlx::query_as::<_, Suggestion>(r#"SELECT * FROM "Suggestion" WHERE "guildId" = $1 AND "number" = $2"#)
        .bind(guild_id)
        .bind(n)
        .fetch_optional(pool)
        .await
        .map_err(bot_db::DbError::from)?)
}

// ── Create ────────────────────────────────────────────────────────────────────

async fn last_created<'e, E: sqlx::PgExecutor<'e>>(db: E, guild_id: &str, user_id: &str) -> Result<Option<DateTime<Utc>>, bot_db::DbError> {
    let row: Option<(chrono::NaiveDateTime,)> =
        sqlx::query_as(r#"SELECT "createdAt" FROM "Suggestion" WHERE "guildId" = $1 AND "authorId" = $2 ORDER BY "createdAt" DESC LIMIT 1"#)
            .bind(guild_id)
            .bind(user_id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|r| Utc.from_utc_datetime(&r.0)))
}

/// `cooldownLeftMs(guildId, userId, settings)`: the member's cooldown left in ms. Read-only (the pre-check before a
/// modal); the create re-checks under the lock.
pub async fn cooldown_left_ms(app: &App, guild_id: &str, user_id: &str, settings: &bot_db::GuildSettings) -> anyhow::Result<i64> {
    let secs = f64::from(settings.suggestion_cooldown_seconds);
    if secs <= 0.0 {
        return Ok(0);
    }
    let last = last_created(app.db(), guild_id, user_id).await?;
    Ok(cooldown_remaining_ms(last.as_ref(), secs, app.clock().now_ms()))
}

/// Why `createSuggestion` refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    NoChannel,
    Anonymous,
    Channel(String),
    Cooldown(i64),
    Send(String),
    DbError,
}

/// What `createSuggestion` returns on success.
#[derive(Debug, Clone)]
pub struct Created {
    pub suggestion: Suggestion,
    pub thread_note: Option<String>,
}

enum Reserved {
    Cooldown(i64),
    Row(Box<Suggestion>),
}

/// `createSuggestion({ client, guild, user, content, anonymous, settings })`: reserves the numbered row (cooldown +
/// number under one per-guild advisory lock, no Discord call inside the transaction), sends the post, then opens the
/// thread. A failed send removes the row, so it never costs the cooldown.
pub async fn create_suggestion(app: &App, guild_id: GuildId, user_id: &str, content: &str, anonymous: bool, settings: &bot_db::GuildSettings) -> Result<Created, CreateError> {
    let Some(channel_id) = settings.suggestion_channel_id.clone().filter(|c| !c.is_empty()) else { return Err(CreateError::NoChannel) };
    if anonymous && !settings.suggestion_allow_anonymous {
        return Err(CreateError::Anonymous);
    }
    // The cache, else `guild.channels.fetch` (any failure: the channel is gone).
    let found = match snowflake(&channel_id).map(ChannelId::new) {
        Some(c) => bot_core::interaction_helpers::resolve_channel(app, guild_id, c).await,
        None => None,
    };
    let Some(found) = found else { return Err(CreateError::Channel(format!("<#{channel_id}> no longer exists."))) };
    let channel = found.id;
    let usable = bot_core::interaction_helpers::check_channel_usable_for(app, guild_id, &found, &POST_NEEDS);
    if !usable.ok {
        return Err(CreateError::Channel(usable.reason.unwrap_or_default()));
    }

    let g = guild_id.to_string();
    let reserved: anyhow::Result<Reserved> = async {
        app.settings().ensure_guild_settings(&g).await?;
        let (gid, uid, text, ch) = (g.clone(), user_id.to_string(), content.to_string(), channel.to_string());
        let secs = f64::from(settings.suggestion_cooldown_seconds);
        let now = app.clock().now();
        bot_db::transaction(app.db(), bot_db::TxOptions::TEN_SECONDS, move |tx| {
            Box::pin(async move {
                bot_db::advisory_xact_lock(tx, &format!("suggestion-number:{gid}")).await?;
                if secs > 0.0 {
                    let last = last_created(&mut *tx, &gid, &uid).await?;
                    let left = cooldown_remaining_ms(last.as_ref(), secs, now.timestamp_millis());
                    if left > 0 {
                        return Ok::<_, anyhow::Error>(Reserved::Cooldown(left));
                    }
                }
                let (next,): (i32,) = sqlx::query_as(NEXT_NUMBER_SQL).bind(&gid).fetch_one(&mut *tx).await.map_err(bot_db::DbError::from)?;
                let row: Suggestion = InsertBuilder::new(&SUGGESTION_INFO)
                    .values([
                        ("guildId", json!(gid)),
                        ("number", json!(next)),
                        ("authorId", json!(uid)),
                        ("anonymous", json!(anonymous)),
                        ("content", json!(text)),
                        ("channelId", json!(ch)),
                        ("createdAt", json!(bot_db::format_js_date(&now))),
                    ])?
                    .build()?
                    .fetch_one(&mut *tx)
                    .await?;
                Ok(Reserved::Row(Box::new(row)))
            })
        })
        .await
    }
    .await;
    let row = match reserved {
        Ok(Reserved::Cooldown(ms)) => return Err(CreateError::Cooldown(ms)),
        Ok(Reserved::Row(r)) => *r,
        Err(e) => {
            bot_util::log_error!(format!("[suggestions] {g}: reserving a suggestion failed"), bot_core::error::log_arg(&e));
            return Err(CreateError::DbError);
        }
    };

    let author = author_for(app, guild_id, &row).await;
    let message_id = match io(app).send(channel, build_suggestion_payload(&row, Some(&author)).no_mentions()).await {
        Ok(m) => m,
        Err(e) => {
            delete_row_quietly(app.db(), &row.id).await;
            bot_util::log_warn!(format!("[suggestions] {g}: could not post suggestion #{}: {}", row.number, e.message));
            return Err(CreateError::Send(send_failure_reason(e.code)));
        }
    };

    let saved = async {
        UpdateBuilder::new(&SUGGESTION_INFO).set("messageId", json!(message_id.to_string()))?.filter("id", json!(row.id))?.build()?.fetch_one::<Suggestion, _>(app.db()).await
    }
    .await;
    let mut saved = match saved {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_error!(format!("[suggestions] {g}: could not save the message of suggestion #{}", row.number), bot_core::error::log_arg(&anyhow::Error::from(e)));
            mark_bot_deletion(app, &[message_id.to_string()], Some("suggestions:rollback"), None);
            let _ = io(app).delete_message(channel, message_id).await;
            delete_row_quietly(app.db(), &row.id).await;
            return Err(CreateError::DbError);
        }
    };

    let mut thread_note = None;
    if settings.suggestion_threads {
        let missing = missing_bot_permissions(app, PermTarget::Channel(guild_id, channel), &["CreatePublicThreads"]);
        if !missing.is_empty() {
            thread_note = Some(format!("I couldn't open a discussion thread: I need **{}** in <#{channel}>.", permission_names(&missing).join(", ")));
        } else {
            let name = format!("Suggestion #{}", row.number);
            let opened = async {
                let thread = io(app).start_thread(channel, message_id, &name, &name).await.map_err(anyhow::Error::from)?;
                let s = UpdateBuilder::new(&SUGGESTION_INFO).set("threadId", json!(thread.to_string()))?.filter("id", json!(row.id))?.build()?.fetch_one::<Suggestion, _>(app.db()).await?;
                Ok::<_, anyhow::Error>(s)
            }
            .await;
            match opened {
                Ok(s) => saved = s,
                Err(e) => {
                    bot_util::log_warn!(format!("[suggestions] {g}: could not open a thread for #{}: {e}", row.number));
                    thread_note = Some("I couldn't open a discussion thread for it.".into());
                }
            }
        }
    }
    Ok(Created { suggestion: saved, thread_note })
}

/// `CREATE_FAILURES[code](data)`.
pub fn create_failure_embed(err: &CreateError, now_ms: i64) -> CreateEmbed {
    match err {
        CreateError::NoChannel => no_channel_embed(),
        CreateError::Anonymous => anonymous_not_allowed(),
        CreateError::Channel(reason) => error_embed("Cannot Post Suggestions", Some(&format!("{reason} Ask an admin to check it with `/suggestion setup`.")), None),
        CreateError::Cooldown(ms) => cooldown_embed(*ms, now_ms),
        CreateError::Send(reason) => error_embed("Could Not Post", Some(&format!("I couldn't post your suggestion: {reason}")), None),
        CreateError::DbError => db_error_embed(),
    }
}

fn db_error_embed() -> CreateEmbed {
    error_embed("Could Not Save", Some("Your suggestion could not be saved. Please try again in a moment."), None)
}

/// `submitFromInteraction(interaction, client, { content, anonymous, settings })`: the whole submit flow after the
/// interaction was deferred: module, content and channel checks, create, reply. Used by the slash command (text
/// option) and the modal. Never fails.
pub async fn submit_from_interaction(ctx: &impl Interactive, content: Option<&str>, anonymous: bool, settings: Option<Settings>) {
    let app = ctx.app();
    let reply = |embed: CreateEmbed| safe_reply(ctx.responder(), Payload::embed(embed).no_mentions());
    let Some(settings) = settings else {
        reply(error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)).await;
        return;
    };
    if let Some(problem) = submit_problem(&settings, anonymous) {
        reply(problem).await;
        return;
    }
    let content = match parse_content(content) {
        Ok(c) => c,
        Err(ContentError::Empty) => {
            reply(error_embed("Empty Suggestion", Some("Write your suggestion first."), None)).await;
            return;
        }
        Err(ContentError::TooLong) => {
            reply(error_embed("Too Long", Some(&format!("Suggestions can be at most **{}** characters.", bot_config::limits::SUGGESTION_LENGTH)), None)).await;
            return;
        }
    };
    let Some(guild_id) = ctx.guild_id() else {
        reply(db_error_embed()).await;
        return;
    };
    match create_suggestion(app, guild_id, &ctx.user().id.to_string(), &content, anonymous, &settings).await {
        Err(e) => {
            reply(create_failure_embed(&e, app.clock().now_ms())).await;
        }
        Ok(out) => {
            let s = &out.suggestion;
            let mut lines = vec![match jump_url(s) {
                Some(url) => format!("Your suggestion is live as [#{}]({url}).", s.number),
                None => format!("Your suggestion is live as #{}.", s.number),
            }];
            if anonymous {
                lines.push("It was posted anonymously.".into());
            }
            if let Some(n) = &out.thread_note {
                lines.push(format!("⚠️ {n}"));
            }
            reply(success_embed("Suggestion Posted", Some(&lines.join("\n")), None)).await;
        }
    }
}

// ── Votes ─────────────────────────────────────────────────────────────────────

/// What `applyVote` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoteOutcome {
    Gone,
    Closed(SuggestionStatus),
    Voted { result: VoteResult, value: i32, upvotes: i64, downvotes: i64 },
}

/// `applyVote({ suggestionId, userId, value })`: the vote transaction. FOR UPDATE on the suggestion row serialises
/// votes of one suggestion and against a review. The totals are recounted from SuggestionVote, so they cannot drift.
pub async fn apply_vote(pool: &PgPool, suggestion_id: &str, user_id: &str, value: i32) -> anyhow::Result<VoteOutcome> {
    let (sid, uid) = (suggestion_id.to_string(), user_id.to_string());
    bot_db::transaction(pool, bot_db::TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            let row: Option<(String,)> = sqlx::query_as(LOCK_ROW_SQL).bind(&sid).fetch_optional(&mut *tx).await.map_err(bot_db::DbError::from)?;
            let Some((status,)) = row else { return Ok::<_, anyhow::Error>(VoteOutcome::Gone) };
            let status = SuggestionStatus::parse(&status).unwrap_or(SuggestionStatus::Open);
            if !is_voting_open(status) {
                return Ok(VoteOutcome::Closed(status));
            }
            let existing: Option<(i32,)> = sqlx::query_as(r#"SELECT "value" FROM "SuggestionVote" WHERE "suggestionId" = $1 AND "userId" = $2"#)
                .bind(&sid)
                .bind(&uid)
                .fetch_optional(&mut *tx)
                .await
                .map_err(bot_db::DbError::from)?;
            let (next, result) = toggle_vote(existing.map(|e| e.0).unwrap_or(0), value);
            if next == 0 {
                sqlx::query(r#"DELETE FROM "SuggestionVote" WHERE "suggestionId" = $1 AND "userId" = $2"#).bind(&sid).bind(&uid).execute(&mut *tx).await.map_err(bot_db::DbError::from)?;
            } else if existing.is_some() {
                sqlx::query(r#"UPDATE "SuggestionVote" SET "value" = $3 WHERE "suggestionId" = $1 AND "userId" = $2"#)
                    .bind(&sid)
                    .bind(&uid)
                    .bind(next)
                    .execute(&mut *tx)
                    .await
                    .map_err(bot_db::DbError::from)?;
            } else {
                InsertBuilder::new(&SUGGESTION_VOTE_INFO)
                    .values([("suggestionId", json!(sid)), ("userId", json!(uid)), ("value", json!(next))])?
                    .without_returning()
                    .build()?
                    .execute(&mut *tx)
                    .await?;
            }
            let (up, down): (i64, i64) = sqlx::query_as(
                r#"SELECT COUNT(*) FILTER (WHERE "value" = 1), COUNT(*) FILTER (WHERE "value" = -1) FROM "SuggestionVote" WHERE "suggestionId" = $1"#,
            )
            .bind(&sid)
            .fetch_one(&mut *tx)
            .await
            .map_err(bot_db::DbError::from)?;
            UpdateBuilder::new(&SUGGESTION_INFO)
                .set("upvotes", json!(up))?
                .set("downvotes", json!(down))?
                .filter("id", json!(sid))?
                .without_returning()
                .build()?
                .execute(&mut *tx)
                .await?;
            Ok(VoteOutcome::Voted { result, value: next, upvotes: up, downvotes: down })
        })
    })
    .await
}

/// `voteReply(number, dir, out)`.
pub fn vote_reply(number: i32, up: bool, result: VoteResult) -> String {
    let emoji = if up { "👍" } else { "👎" };
    match result {
        VoteResult::Added => format!("{emoji} You {} suggestion #{number}.", if up { "upvoted" } else { "downvoted" }),
        VoteResult::Switched => format!("🔁 Your vote on suggestion #{number} changed to {emoji}."),
        VoteResult::Removed => format!("🗑️ Your vote on suggestion #{number} was removed."),
    }
}

/// `suggestion:{id}:up|down`. Defers first; everything after may touch the database.
pub async fn handle_vote_button(ctx: ComponentCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    let id = params.first().cloned().unwrap_or_default();
    let dir = params.get(1).cloned().unwrap_or_default();
    if id.is_empty() || (dir != "up" && dir != "down") {
        safe_reply(&ctx.responder, Payload::text("This button is no longer active.").ephemeral(true)).await;
        return Ok(());
    }
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let reply = |content: String| safe_reply(&ctx.responder, Payload::text(content).no_mentions());
    let run = async {
        let s = find_by_id(ctx.app.db(), &id).await?;
        let Some(s) = s.filter(|s| ctx.guild_id().map(|g| g.to_string()).as_deref() == Some(s.guild_id.as_str())) else {
            reply("This suggestion no longer exists.".into()).await;
            return Ok::<_, anyhow::Error>(());
        };
        let closed_text = format!("Voting on suggestion #{} is closed ({}).", s.number, status_info(s.status).label);
        if !is_voting_open(s.status) {
            // The post still shows live buttons (an edit was lost): fix it.
            let live = ctx.interaction.message.components.iter().any(|r| r.components.iter().any(|c| matches!(c, ActionRowComponent::Button(b) if !b.disabled)));
            if live {
                request_render(&ctx.app, &s.id);
            }
            reply(closed_text).await;
            return Ok(());
        }
        let up = dir == "up";
        match apply_vote(ctx.app.db(), &s.id, &ctx.user().id.to_string(), if up { 1 } else { -1 }).await? {
            VoteOutcome::Gone => {
                reply("This suggestion no longer exists.".into()).await;
            }
            VoteOutcome::Closed(_) => {
                request_render(&ctx.app, &s.id);
                reply(closed_text).await;
            }
            VoteOutcome::Voted { result, .. } => {
                reply(vote_reply(s.number, up, result)).await;
                request_render(&ctx.app, &s.id);
            }
        }
        Ok(())
    };
    if let Err(e) = run.await {
        bot_util::log_error!("[suggestions] vote failed", bot_core::error::log_arg(&e));
        reply("Something went wrong while saving your vote. Please try again.".into()).await;
    }
    Ok(())
}

// ── Modal ─────────────────────────────────────────────────────────────────────

/// `suggestion-modal:{0|1}`: the form from `/suggestion submit`. Everything is checked again here.
pub async fn handle_suggestion_modal(ctx: ModalCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    let anonymous = params.first().map(String::as_str) == Some("1");
    let content = ctx.value("suggestion-text").unwrap_or_default();
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    submit_from_interaction(&ctx, Some(&content), anonymous, settings).await;
    Ok(())
}

// ── Debounced message edit ────────────────────────────────────────────────────

/// What a render did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderResult {
    Ok,
    Skip,
    Gone,
    Error,
}

/// A replacement for the Discord render (tests): `(app, suggestion id) -> result`.
pub type RenderFn = Arc<dyn Fn(App, String) -> bot_core::BoxFuture<anyhow::Result<RenderResult>> + Send + Sync>;

#[derive(Default)]
struct RenderSlot {
    last_at: i64,
    timer: Option<tokio::task::AbortHandle>,
}

/// The per-app render state (`renderMutex`, `renderState`, `warnedAt`, the interval and the test seam).
#[derive(Default)]
pub struct RenderRuntime {
    mutex: bot_core::sync::KeyedMutex,
    state: Mutex<HashMap<String, RenderSlot>>,
    warned_at: Mutex<HashMap<String, i64>>,
    interval_ms: Mutex<Option<u64>>,
    implementation: RwLock<Option<RenderFn>>,
}

fn warn_throttled(app: &App, key: &str, message: String) {
    let rt = app.ext::<RenderRuntime>();
    let now = app.clock().now_ms();
    let mut w = rt.warned_at.lock().unwrap_or_else(|e| e.into_inner());
    if now - w.get(key).copied().unwrap_or(i64::MIN / 2) < WARN_EVERY_MS {
        return;
    }
    if w.len() > 200 {
        w.retain(|_, t| now - *t < WARN_EVERY_MS);
    }
    w.insert(key.to_string(), now);
    drop(w);
    bot_util::log_warn!(message);
}

/// `renderSuggestionMessage(client, id)`: edits the suggestion's post with the current row. The message id is
/// cleared when Discord says the message or channel is gone.
pub async fn render_suggestion_message(app: &App, id: &str) -> anyhow::Result<RenderResult> {
    let Some(s) = find_by_id(app.db(), id).await? else { return Ok(RenderResult::Skip) };
    let Some(message_id) = s.message_id.clone() else { return Ok(RenderResult::Skip) };
    if !guild_available(app, &s.guild_id) {
        return Ok(RenderResult::Skip);
    }
    let guild_id = GuildId::new(snowflake(&s.guild_id).unwrap_or(1));
    let forget = || async {
        sqlx::query(sqlx::AssertSqlSafe(format!(r#"UPDATE "Suggestion" SET "messageId" = NULL, "updatedAt" = {} WHERE "id" = $1 AND "messageId" = $2"#, bot_db::SQL_NOW)))
            .bind(id)
            .bind(&message_id)
            .execute(app.db())
            .await
            .map_err(bot_db::DbError::from)?;
        Ok::<_, anyhow::Error>(RenderResult::Gone)
    };
    let found = match resolve_channel(app, guild_id, &s.channel_id).await {
        Ok(f) => f,
        Err(e) => return Err(e.into()),
    };
    let channel = match found {
        Resolved::Gone => return forget().await,
        Resolved::NoAccess => {
            warn_throttled(app, &format!("render:{id}"), format!("[suggestions] {}: cannot reach the channel of suggestion #{} to update it", s.guild_id, s.number));
            return Ok(RenderResult::Skip);
        }
        Resolved::Channel(c) => c,
    };
    let author = author_for(app, guild_id, &s).await;
    let mid = MessageId::new(snowflake(&message_id).unwrap_or(1));
    match io(app).edit(channel, mid, build_suggestion_payload(&s, Some(&author)).no_mentions()).await {
        Ok(()) => Ok(RenderResult::Ok),
        Err(e) if e.code == Some(UNKNOWN_MESSAGE) || e.code == Some(UNKNOWN_CHANNEL) => forget().await,
        Err(e) if e.code.is_some_and(|c| NO_ACCESS_CODES.contains(&c)) => {
            warn_throttled(app, &format!("render:{id}"), format!("[suggestions] {}: cannot edit suggestion #{} (Discord {})", s.guild_id, s.number, e.code.unwrap_or(0)));
            Ok(RenderResult::Skip)
        }
        Err(e) => Err(e.into()),
    }
}

/// `__setRenderImpl(fn)`: replaces the Discord render (tests). `None` restores it.
pub fn set_render_impl(app: &App, f: Option<RenderFn>) {
    *app.ext::<RenderRuntime>().implementation.write().unwrap_or_else(|e| e.into_inner()) = f;
}

/// `__setRenderInterval(ms)`: the debounce window (tests). `None` restores [`RENDER_INTERVAL_MS`].
pub fn set_render_interval(app: &App, ms: Option<u64>) {
    *app.ext::<RenderRuntime>().interval_ms.lock().unwrap_or_else(|e| e.into_inner()) = ms;
}

/// `renderNow(client, id)`: renders now under the suggestion's mutex (reviews). Never fails.
pub async fn render_now(app: &App, id: &str) -> RenderResult {
    let rt = app.ext::<RenderRuntime>();
    let custom = rt.implementation.read().unwrap_or_else(|e| e.into_inner()).clone();
    let (a, sid) = (app.clone(), id.to_string());
    let out = rt
        .mutex
        .run(id, move || async move {
            match custom {
                Some(f) => f(a, sid).await,
                None => render_suggestion_message(&a, &sid).await,
            }
        })
        .await;
    match out {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[suggestions] render failed ({id})"), bot_core::error::log_arg(&e));
            RenderResult::Error
        }
    }
}

/// `requestRender(client, id)`: asks for the post to be re-rendered. The first request after a quiet period
/// renders at once; requests inside the window collapse into one trailing render that reads the database when it
/// runs. The timer is cleared before the render starts, so a vote that commits meanwhile queues another one.
pub fn request_render(app: &App, id: &str) {
    let rt = app.ext::<RenderRuntime>();
    let interval = rt.interval_ms.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or(RENDER_INTERVAL_MS) as i64;
    let now = app.clock().now_ms();
    let mut state = rt.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.get(id).is_some_and(|s| s.timer.is_some()) {
        return;
    }
    if state.len() > RENDER_SWEEP_AT {
        state.retain(|k, s| s.timer.is_some() || k == id || now - s.last_at <= RENDER_IDLE_MS);
    }
    let slot = state.entry(id.to_string()).or_default();
    let wait = (slot.last_at + interval - now).max(0) as u64;
    let (a, sid) = (app.clone(), id.to_string());
    let handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
        let rt = a.ext::<RenderRuntime>();
        {
            let mut st = rt.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(s) = st.get_mut(&sid) else { return };
            s.last_at = a.clock().now_ms();
            s.timer = None;
        }
        // A separate task: a later cancel aborts only a timer that has not fired.
        tokio::spawn(async move {
            render_now(&a, &sid).await;
        });
    });
    slot.timer = Some(handle.abort_handle());
}

/// `cancelRender(id)`: drops a queued render (the suggestion was deleted or lost its message).
pub fn cancel_render(app: &App, id: &str) {
    let rt = app.ext::<RenderRuntime>();
    let mut state = rt.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = state.remove(id)
        && let Some(t) = s.timer
    {
        t.abort();
    }
}

// ── Review and delete ─────────────────────────────────────────────────────────

/// How the author DM went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmResult {
    Sent,
    Failed,
    Off,
}

async fn dm_author(app: &App, guild_name: Option<&str>, s: &Suggestion) -> DmResult {
    let Some(uid) = snowflake(&s.author_id).map(UserId::new) else { return DmResult::Failed };
    let embed = build_review_dm(guild_name, s, &app.clock().now());
    match io(app).dm(uid, Payload::embed(embed).no_mentions()).await {
        Ok(()) => DmResult::Sent,
        Err(_) => DmResult::Failed,
    }
}

/// What `reviewSuggestion` did.
#[derive(Debug, Clone)]
pub struct Reviewed {
    pub suggestion: Suggestion,
    pub previous: SuggestionStatus,
    pub edited: bool,
    pub dm: DmResult,
}

/// `reviewSuggestion({ client, guild, number, status, note, reviewer, settings })`: sets a suggestion's status with
/// the reviewer and an optional note, edits the post and DMs the author. `Ok(None)` is NOT_FOUND.
pub async fn review_suggestion(
    app: &App,
    guild_id: GuildId,
    number: i64,
    status: SuggestionStatus,
    note: Option<&str>,
    reviewer_id: &str,
    settings: Option<&bot_db::GuildSettings>,
) -> anyhow::Result<Option<Reviewed>> {
    let g = guild_id.to_string();
    let Some(existing) = find_by_number(app.db(), &g, number).await? else { return Ok(None) };
    let text = bot_util::js::trim(note.unwrap_or(""));
    let row: Suggestion = UpdateBuilder::new(&SUGGESTION_INFO)
        .set("status", json!(status.as_str()))?
        .set("reviewerId", json!(reviewer_id))?
        .set("reviewNote", if text.is_empty() { Value::Null } else { json!(clamp_text(text, NOTE_MAX)) })?
        .set("reviewedAt", json!(bot_db::format_js_date(&app.clock().now())))?
        .filter("id", json!(existing.id))?
        .build()?
        .fetch_one(app.db())
        .await?;
    let edited = render_now(app, &row.id).await == RenderResult::Ok;
    let mut dm = DmResult::Off;
    if settings.is_some_and(|s| s.suggestion_dm_on_update) && row.author_id != reviewer_id {
        let guild_name = cached_guild(app, guild_id).map(|g| g.name);
        dm = dm_author(app, guild_name.as_deref(), &row).await;
    }
    Ok(Some(Reviewed { suggestion: row, previous: existing.status, edited, dm }))
}

/// What `deleteSuggestion` did.
#[derive(Debug, Clone)]
pub enum Deleted {
    Ok(Box<Suggestion>),
    NotFound,
    /// The post could not be deleted (the reason); nothing changed.
    Message(String),
}

/// `deleteSuggestion({ client, guild, number })`: the post goes first (a failure other than "already gone" refuses
/// and changes nothing), then the discussion thread (best effort) and the row (votes cascade).
pub async fn delete_suggestion(app: &App, guild_id: GuildId, number: i64) -> anyhow::Result<Deleted> {
    let g = guild_id.to_string();
    let Some(s) = find_by_number(app.db(), &g, number).await? else { return Ok(Deleted::NotFound) };
    cancel_render(app, &s.id);
    if let Some(mid) = s.message_id.as_deref().and_then(snowflake).map(MessageId::new) {
        let outcome = async {
            match resolve_channel(app, guild_id, &s.channel_id).await? {
                Resolved::NoAccess => return Ok::<_, IoError>(Some(format!("I can't reach <#{}>.", s.channel_id))),
                Resolved::Channel(c) => {
                    mark_bot_deletion(app, &[mid.to_string()], Some("suggestions:delete"), None);
                    io(app).delete_message(c, mid).await?;
                }
                Resolved::Gone => {}
            }
            Ok(None)
        }
        .await;
        match outcome {
            Ok(Some(reason)) => return Ok(Deleted::Message(reason)),
            Ok(None) => {}
            Err(e) if e.code == Some(UNKNOWN_MESSAGE) || e.code == Some(UNKNOWN_CHANNEL) => {}
            Err(e) => {
                bot_util::log_warn!(format!("[suggestions] {g}: could not delete the post of #{}: {}", s.number, e.message));
                return Ok(Deleted::Message(send_failure_reason(e.code)));
            }
        }
    }
    if let Some(tid) = s.thread_id.as_deref().and_then(snowflake).map(ChannelId::new)
        && let Err(e) = io(app).delete_channel(tid, &format!("Suggestion #{} deleted", s.number)).await
    {
        bot_util::log_debug!(format!("[suggestions] {g}: thread of #{} not deleted: {}", s.number, e.message));
    }
    sqlx::query(r#"DELETE FROM "Suggestion" WHERE "id" = $1"#).bind(&s.id).execute(app.db()).await.map_err(bot_db::DbError::from)?;
    Ok(Deleted::Ok(Box::new(s)))
}

// ── Hooks ─────────────────────────────────────────────────────────────────────

/// `onMessageDelete(ctx)`: the post vanished. The row keeps its data; only the message id is cleared.
pub async fn on_message_delete(app: &App, guild_id: &str, message_id: &str) {
    let run = async {
        let ids: Vec<(String,)> = sqlx::query_as(r#"SELECT "id" FROM "Suggestion" WHERE "guildId" = $1 AND "messageId" = $2"#)
            .bind(guild_id)
            .bind(message_id)
            .fetch_all(app.db())
            .await
            .map_err(bot_db::DbError::from)?;
        if ids.is_empty() {
            return Ok::<_, anyhow::Error>(());
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(r#"UPDATE "Suggestion" SET "messageId" = NULL, "updatedAt" = {} WHERE "guildId" = $1 AND "messageId" = $2"#, bot_db::SQL_NOW)))
            .bind(guild_id)
            .bind(message_id)
            .execute(app.db())
            .await
            .map_err(bot_db::DbError::from)?;
        for (id,) in ids {
            cancel_render(app, &id);
        }
        Ok(())
    };
    if let Err(e) = run.await {
        bot_util::log_error!(format!("[suggestions] {guild_id}: clearing a deleted post failed"), bot_core::error::log_arg(&e));
    }
}

/// `onChannelDelete(ctx)`: the suggestion channel is gone, so it is unset; its posts lose their message id.
pub async fn on_channel_delete(app: &App, guild_id: &str, channel_id: &str) {
    let run = async {
        let settings = app.settings().get_settings_with(guild_id, true).await?;
        if settings.suggestion_channel_id.as_deref() == Some(channel_id) {
            app.settings().update_settings(guild_id, SettingsPatch::new().set("suggestionChannelId", Value::Null)).await?;
            bot_util::log_info!(format!("[suggestions] {guild_id}: the suggestion channel was deleted, so it is unset"));
        }
        let ids: Vec<(String,)> = sqlx::query_as(r#"SELECT "id" FROM "Suggestion" WHERE "guildId" = $1 AND "channelId" = $2 AND "messageId" IS NOT NULL"#)
            .bind(guild_id)
            .bind(channel_id)
            .fetch_all(app.db())
            .await
            .map_err(bot_db::DbError::from)?;
        if !ids.is_empty() {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                r#"UPDATE "Suggestion" SET "messageId" = NULL, "updatedAt" = {} WHERE "guildId" = $1 AND "channelId" = $2 AND "messageId" IS NOT NULL"#,
                bot_db::SQL_NOW
            )))
            .bind(guild_id)
            .bind(channel_id)
            .execute(app.db())
            .await
            .map_err(bot_db::DbError::from)?;
            for (id,) in ids {
                cancel_render(app, &id);
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    if let Err(e) = run.await {
        bot_util::log_error!(format!("[suggestions] {guild_id}: channel delete cleanup failed"), bot_core::error::log_arg(&e));
    }
}
