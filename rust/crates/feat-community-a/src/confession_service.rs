//! Port of `src/services/confessionService.js`: Confessions (beyond MEE6).
//!
//! Members post anonymously with `/confession submit` (or the Confess button of a panel), moderators can hold every
//! confession for review, and only an admin can look an author up, which is written to the moderation log. This file
//! holds the rules: the numbered row (per-guild number and cooldown under one advisory lock), the post and the review
//! message, the claim-based approve / reject / delete, the author blocklist, the logged reveal and the cleanup hooks.
//!
//! ANONYMITY is the point of the module. The author (`author_id`) is read in exactly three places: the cooldown
//! lookup, the block command and the reveal. No embed builder takes it, no message sent to a channel carries it, and
//! the process log never prints it: database failures are logged by name and code only ([`err_tag`]).

use std::sync::{Arc, RwLock};

use bot_config::colors;
use bot_core::deletion_tracker::mark_bot_deletion;
use bot_core::interaction::ack_budget_ms;
use bot_core::interaction_helpers::{CHANNEL_POST_NEEDS, Interactive, Payload, cached_guild, is_interaction_gone, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_db::{CONFESSION_INFO, Confession, ConfessionStatus, InsertBuilder, PgPool};
use bot_util::text::clamp_text;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use serenity::all::{Attachment, ButtonStyle, ChannelId, Embed, GuildId, InputTextStyle, Message, MessageId, ReactionType, UserId};
use serenity::builder::{CreateActionRow, CreateAttachment, CreateButton, CreateEmbed, CreateInputText, CreateModal};

use crate::io::{IoError, io, snowflake};

/// `CONFESSION.IMAGE_NAME`: every image is re-uploaded as `confession.<ext>` (the member's file name can give them
/// away).
pub const IMAGE_NAME: &str = "confession";
/// `CONFESSION.IMAGE_TIMEOUT_MS`.
pub const IMAGE_TIMEOUT_MS: u64 = 10_000;
/// `CONFESSION.EMBED_TEXT_MAX`: the post and the review message hold the text in an embed description.
pub const EMBED_TEXT_MAX: usize = 4000;

const UNKNOWN_CHANNEL: u64 = 10003;
const UNKNOWN_MESSAGE: u64 = 10008;
const POST_NEEDS: [&str; 3] = ["ViewChannel", "SendMessages", "EmbedLinks"];

/// One row of `STATUS_INFO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusInfo {
    pub label: &'static str,
    pub emoji: &'static str,
    pub color: u32,
}

/// `STATUS_INFO[status]`.
pub fn status_info(status: ConfessionStatus) -> StatusInfo {
    match status {
        ConfessionStatus::Pending => StatusInfo { label: "Pending review", emoji: "⏳", color: colors::WARNING },
        ConfessionStatus::Approved => StatusInfo { label: "Approved", emoji: "✅", color: colors::SUCCESS },
        ConfessionStatus::Rejected => StatusInfo { label: "Rejected", emoji: "❌", color: colors::ERROR },
        ConfessionStatus::Deleted => StatusInfo { label: "Deleted", emoji: "🗑️", color: colors::NEUTRAL },
    }
}

// ── Raw SQL ───────────────────────────────────────────────────────────────────

/// `numberLockSql(guildId)`: `$1` is `confession-number:<guildId>`.
pub const NUMBER_LOCK_SQL: &str = "SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext($1))";
/// `nextNumberSql(guildId)`.
pub const NEXT_NUMBER_SQL: &str = r#"SELECT (COALESCE(MAX("number"), 0) + 1)::int AS next FROM "Confession" WHERE "guildId" = $1"#;

/// `RAW_SQL`: every raw statement with sample parameters `(name, sql, binds)`.
pub fn raw_sql() -> Vec<(&'static str, &'static str, Vec<Value>)> {
    vec![("confessions.numberLock", NUMBER_LOCK_SQL, vec![json!("confession-number:0")]), ("confessions.nextNumber", NEXT_NUMBER_SQL, vec![json!("0")])]
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// `errTag(err)`: the database echoes the arguments of a failed call (author id and text included), so errors are
/// logged by name and code only. A database error is `DbError <sqlstate>`, a Discord error `DiscordAPIError
/// <code>`.
pub fn err_tag(err: &anyhow::Error) -> String {
    if let Some(e) = err.downcast_ref::<bot_db::DbError>() {
        return match e.sqlstate() {
            Some(code) => format!("DbError {code}"),
            None => "DbError".into(),
        };
    }
    if let Some(e) = err.downcast_ref::<bot_core::settings::SettingsError>() {
        return match e.db().sqlstate() {
            Some(code) => format!("DbError {code}"),
            None => "DbError".into(),
        };
    }
    if let Some(e) = err.downcast_ref::<IoError>() {
        return match e.code {
            Some(code) => format!("DiscordAPIError {code}"),
            None => "DiscordAPIError".into(),
        };
    }
    "Error".into()
}

/// Why `parseContent` refused a text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentError {
    Empty,
    TooLong,
}

/// `parseContent(raw)`: trims and enforces `LIMITS.CONFESSION_LENGTH`.
pub fn parse_content(raw: Option<&str>) -> Result<String, ContentError> {
    let content = bot_util::js::trim(raw.unwrap_or(""));
    if content.is_empty() {
        return Err(ContentError::Empty);
    }
    if bot_util::js::utf16_len(content) > bot_config::limits::CONFESSION_LENGTH {
        return Err(ContentError::TooLong);
    }
    Ok(content.to_string())
}

// Zero-width and soft-hyphen characters hide a link from a plain match.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}')
}
// A scheme, www., a Discord invite host, or a domain with a path (bit.ly/x). Static pattern, linear in the text.
static LINK: bot_util::jsre::LazyRe = bot_util::lazy_re!(
    r"(?:[a-z][a-z0-9+.-]*:\/\/|\bwww\.|\bdiscord(?:app)?\.com\/invite\b|\bdiscord\.gg\b|\bdsc\.gg\b|\binvite\.gg\b|\b[a-z0-9-]+(?:\.[a-z0-9-]+)*\.[a-z]{2,24}\/\S)",
    "i"
);

/// `containsLink(text)`: whether the text has a URL or a Discord invite (NFKC first, invisible characters removed).
pub fn contains_link(text: &str) -> bool {
    use unicode_normalization::UnicodeNormalization;
    let cleaned: String = text.nfkc().filter(|c| !is_invisible(*c)).collect();
    LINK.test(&cleaned)
}

/// `cooldownRemainingMs(lastAt, cooldownSeconds, now)`: milliseconds until the member may confess again.
pub fn cooldown_remaining_ms(last_at: Option<&DateTime<Utc>>, cooldown_seconds: f64, now_ms: i64) -> i64 {
    crate::suggestion_service::cooldown_remaining_ms(last_at, cooldown_seconds, now_ms)
}

/// `jumpUrl(c)`: the link to the post, or `None` when the message is gone or not posted yet.
pub fn jump_url(c: &Confession) -> Option<String> {
    match (&c.message_id, &c.channel_id) {
        (Some(m), Some(ch)) => Some(format!("https://discord.com/channels/{}/{ch}/{m}", c.guild_id)),
        _ => None,
    }
}

/// `moduleOffEmbed()`.
pub fn module_off_embed() -> CreateEmbed {
    warn_embed(
        "Module Disabled",
        Some("The **Confessions** module is turned off on this server. An admin can enable it with `/module enable confessions`."),
        None,
    )
}

fn no_channel_embed() -> CreateEmbed {
    error_embed("No Confession Channel", Some("Confessions are not set up yet. An admin can pick a channel with `/confession setup`."), None)
}

/// Neutral on purpose: it must not say why (a block is never explained).
fn blocked_embed() -> CreateEmbed {
    error_embed("Can't Confess", Some("You can't post confessions on this server."), None)
}

fn settings_unavailable_embed() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

fn db_error_embed() -> CreateEmbed {
    error_embed("Could Not Save", Some("Your confession could not be saved. Please try again in a moment."), None)
}

/// `cooldownEmbed(ms)`.
pub fn cooldown_embed(ms: i64, now_ms: i64) -> CreateEmbed {
    let when = bot_util::time::discord_timestamp_ms((now_ms + ms) as f64, "R");
    warn_embed("Slow Down", Some(&format!("You can post another confession {when}.")), None)
}

/// `submitProblem(settings, userId)`: what stops a member from confessing, from settings alone.
pub fn submit_problem(settings: &bot_db::GuildSettings, user_id: Option<&str>) -> Option<CreateEmbed> {
    if !is_module_enabled(Some(settings), Some(ModuleKey::Confessions)) {
        return Some(module_off_embed());
    }
    if settings.confession_channel_id.as_deref().is_none_or(str::is_empty) {
        return Some(no_channel_embed());
    }
    if user_id.is_some_and(|u| settings.confession_blocked_user_ids.iter().any(|b| b == u)) {
        return Some(blocked_embed());
    }
    None
}

/// The fields of an attachment `imageProblem` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentInfo {
    pub content_type: Option<String>,
    pub size: u64,
    pub url: String,
}

impl From<&Attachment> for AttachmentInfo {
    fn from(a: &Attachment) -> Self {
        Self { content_type: a.content_type.clone(), size: u64::from(a.size), url: a.url.clone() }
    }
}

/// `imageProblem(settings, attachment)`: what is wrong with an attached image, from the option alone.
pub fn image_problem(allow_images: bool, attachment: Option<&AttachmentInfo>) -> Option<CreateEmbed> {
    let a = attachment?;
    if !allow_images {
        return Some(error_embed("Images Not Allowed", Some("This server does not allow images in confessions. Remove the `image` option."), None));
    }
    let raw = a.content_type.clone().unwrap_or_default();
    let kind = raw.split(';').next().unwrap_or("").trim().to_lowercase();
    if !kind.is_empty() && (!kind.starts_with("image/") || kind == "image/svg+xml") {
        return Some(error_embed("Not An Image", Some("The file has to be a PNG, JPEG, GIF or WebP image."), None));
    }
    if a.size > bot_config::limits::CONFESSION_IMAGE_BYTES as u64 {
        let mb = bot_config::limits::CONFESSION_IMAGE_BYTES / 1_000_000;
        return Some(error_embed("Image Too Large", Some(&format!("Images can be at most **{mb} MB**.")), None));
    }
    None
}

// ── Messages ──────────────────────────────────────────────────────────────────

fn ts(date: &DateTime<Utc>) -> serenity::all::Timestamp {
    serenity::all::Timestamp::from(*date)
}

/// `buildConfessionEmbed(c, { imageName })`: the post. The time is the time it is posted (`now`), not the time it
/// was written. Nothing here can name the author.
pub fn build_confession_embed(number: i32, content: &str, image_name: Option<&str>, now: &DateTime<Utc>) -> CreateEmbed {
    let mut e = CreateEmbed::new()
        .color(colors::PRIMARY)
        .title(format!("🤫 Confession #{number}"))
        .description(clamp_text(content, EMBED_TEXT_MAX))
        .timestamp(ts(now));
    if let Some(name) = image_name {
        e = e.image(format!("attachment://{name}"));
    }
    e
}

/// `withStatus(embed, text, color)` on an embed's JSON: replaces the "Status" field (last), and the colour.
fn with_status_json(mut embed: Value, text: &str, color: Option<u32>) -> Value {
    let mut fields: Vec<Value> = embed["fields"].as_array().cloned().unwrap_or_default().into_iter().filter(|f| f["name"] != "Status").collect();
    fields.push(json!({ "name": "Status", "value": clamp_text(text, 1024), "inline": false }));
    embed["fields"] = Value::Array(fields);
    if let Some(c) = color {
        embed["color"] = json!(c);
    }
    embed
}

fn embed_from_json(v: Value) -> Option<CreateEmbed> {
    serde_json::from_value::<Embed>(v).ok().map(CreateEmbed::from)
}

/// `buildReviewEmbed(c, { imageName })`: the review message, the post plus a status line, timestamped with the
/// submission time. It never shows the author.
pub fn build_review_embed(c: &Confession, image_name: Option<&str>) -> CreateEmbed {
    let pending = status_info(ConfessionStatus::Pending);
    let base = build_confession_embed(c.number, &c.content, image_name, &c.created_at).color(pending.color);
    let v = serde_json::to_value(&base).unwrap_or(Value::Null);
    embed_from_json(with_status_json(v, &format!("{} {}", pending.emoji, pending.label), None)).unwrap_or(base)
}

/// `buildReviewComponents(id)`: Approve and Reject.
pub fn build_review_components(id: &str) -> Vec<CreateActionRow> {
    let approve = bot_config::custom_id!("confession", id, "approve").unwrap_or_default();
    let reject = bot_config::custom_id!("confession", id, "reject").unwrap_or_default();
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(approve).style(ButtonStyle::Success).label("Approve").emoji(ReactionType::Unicode("✅".into())),
        CreateButton::new(reject).style(ButtonStyle::Danger).label("Reject").emoji(ReactionType::Unicode("❌".into())),
    ])]
}

/// `buildConfessionModal()`: the form `/confession submit` and the panel button open. It has no image field.
pub fn build_confession_modal() -> CreateModal {
    let id = bot_config::custom_id!("confession-modal").unwrap_or_default();
    let field = bot_config::custom_id!("confession-text").unwrap_or_default();
    CreateModal::new(id, "Make a confession").components(vec![CreateActionRow::InputText(
        CreateInputText::new(InputTextStyle::Paragraph, "Your confession", field)
            .required(true)
            .min_length(1)
            .max_length(bot_config::limits::CONFESSION_LENGTH as u16)
            .placeholder("Say it here. Your name is not shown with it."),
    )])
}

/// `buildPanelPayload()`: the panel message with the Confess button.
pub fn build_panel_payload() -> Payload {
    let embed = CreateEmbed::new().color(colors::PRIMARY).title("🤫 Confessions").description(
        [
            "Got something to get off your chest? Press **Confess** and write it in the form. Your name is not shown with it.",
            "Members and moderators cannot see who wrote a confession. Server admins can look an author up when they have to, and that is always written to the moderation log.",
        ]
        .join("\n\n"),
    );
    let id = bot_config::custom_id!("confession", "new").unwrap_or_default();
    let row = CreateActionRow::Buttons(vec![CreateButton::new(id).style(ButtonStyle::Primary).label("Confess").emoji(ReactionType::Unicode("🤫".into()))]);
    Payload::embed(embed).components(vec![row]).no_mentions()
}

// ── Discord helpers ───────────────────────────────────────────────────────────

/// `sendFailureReason(err)`.
pub fn send_failure_reason(code: Option<u64>) -> String {
    match code {
        Some(50013) | Some(50001) => "I'm missing permissions there.".into(),
        Some(UNKNOWN_CHANNEL) => "the channel no longer exists.".into(),
        Some(c) => format!("Discord error {c}."),
        None => "Discord error unknown.".into(),
    }
}

async fn delete_row_quietly(pool: &PgPool, id: &str) {
    if let Err(e) = sqlx::query(r#"DELETE FROM "Confession" WHERE "id" = $1"#).bind(id).execute(pool).await {
        bot_util::log_warn!(format!("[confessions] could not remove a reserved confession row ({})", err_tag(&bot_db::DbError::from(e).into())));
    }
}

/// Where a confession goes: the post channel, and the review channel when one is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Targets {
    pub post: ChannelId,
    pub review: Option<ChannelId>,
}

/// `resolveTargets(guild, settings, { image })`: each channel must exist and let the bot post (and attach files for
/// an image). A channel the cache lacks is fetched over REST (`guild.channels.fetch`; any failure: it no longer
/// exists).
pub async fn resolve_targets(app: &App, guild_id: GuildId, post_id: Option<&str>, review_id: Option<&str>, image: bool) -> Result<Targets, String> {
    use bot_core::interaction_helpers::{check_channel_usable_for, resolve_channel};
    let needs: Vec<&str> = if image { POST_NEEDS.iter().copied().chain(["AttachFiles"]).collect() } else { POST_NEEDS.to_vec() };
    let find = async |id: Option<&str>| match id.and_then(snowflake).map(ChannelId::new) {
        Some(c) => resolve_channel(app, guild_id, c).await,
        None => None,
    };
    let Some(post) = find(post_id).await else { return Err("The confession channel no longer exists.".into()) };
    let usable = check_channel_usable_for(app, guild_id, &post, &needs);
    if !usable.ok {
        return Err(usable.reason.unwrap_or_default());
    }
    let mut review = None;
    if review_id.is_some_and(|r| !r.is_empty()) {
        let Some(r) = find(review_id).await else { return Err("The review channel no longer exists.".into()) };
        let ok = check_channel_usable_for(app, guild_id, &r, &needs);
        if !ok.ok {
            return Err(ok.reason.unwrap_or_default());
        }
        review = Some(r.id);
    }
    Ok(Targets { post: post.id, review })
}

// ── Images ────────────────────────────────────────────────────────────────────

/// A replacement for the download (tests): `url -> bytes`.
pub type ImageFetchFn = Arc<dyn Fn(String) -> bot_core::BoxFuture<Result<Vec<u8>, String>> + Send + Sync>;

/// The per-app image download seam (`__setImageFetch`).
#[derive(Default)]
pub struct ImageFetchSlot(RwLock<Option<ImageFetchFn>>);

/// `__setImageFetch(fn)`: replaces the download (tests are offline). `None` restores it.
pub fn set_image_fetch(app: &App, f: Option<ImageFetchFn>) {
    *app.ext::<ImageFetchSlot>().0.write().unwrap_or_else(|e| e.into_inner()) = f;
}

/// A downloaded image under its neutral name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub buffer: Vec<u8>,
    pub name: String,
}

/// `downloadImage(url)`: through safeFetchImage (public hosts only, size cap, image content type), named neutrally.
/// `Err` is the message for the member.
pub async fn download_image(app: &App, url: &str) -> Result<Image, String> {
    let custom = app.ext::<ImageFetchSlot>().0.read().unwrap_or_else(|e| e.into_inner()).clone();
    let fetched: Result<Vec<u8>, String> = match custom {
        Some(f) => f(url.to_string()).await,
        None => {
            let opts = bot_util::safe_fetch::ImageOptions {
                max_bytes: bot_config::limits::CONFESSION_IMAGE_BYTES as u64,
                timeout_ms: IMAGE_TIMEOUT_MS,
                ..Default::default()
            };
            bot_util::safe_fetch::safe_fetch_image(url, &opts).await.map_err(|e| e.code.as_str().to_string())
        }
    };
    let buffer = match fetched {
        Ok(b) => b,
        Err(code) => {
            bot_util::log_warn!(format!("[confessions] image download failed ({code})"));
            return Err("I could not read that image. Try again, or post without it.".into());
        }
    };
    let Some(size) = bot_util::image_size::read_image_size(&buffer) else { return Err("That file is not a PNG, JPEG, GIF or WebP image.".into()) };
    if buffer.len() > bot_config::limits::CONFESSION_IMAGE_BYTES {
        return Err("The image is too large.".into());
    }
    let ext = match size.kind {
        bot_util::image_size::ImageType::Png => "png",
        bot_util::image_size::ImageType::Jpeg => "jpg",
        bot_util::image_size::ImageType::Gif => "gif",
        bot_util::image_size::ImageType::Webp => "webp",
    };
    Ok(Image { buffer, name: format!("{IMAGE_NAME}.{ext}") })
}

fn with_image(p: Payload, image: Option<&Image>) -> Payload {
    match image {
        Some(i) => p.file(CreateAttachment::bytes(i.buffer.clone(), i.name.clone())),
        None => p,
    }
}

// ── Reads ─────────────────────────────────────────────────────────────────────

/// `prisma.confession.findUnique({ where: { id } })`.
pub async fn find_by_id(pool: &PgPool, id: &str) -> anyhow::Result<Option<Confession>> {
    Ok(sqlx::query_as::<_, Confession>(r#"SELECT * FROM "Confession" WHERE "id" = $1"#).bind(id).fetch_optional(pool).await.map_err(bot_db::DbError::from)?)
}

/// `prisma.confession.findUnique({ where: { guildId_number } })`.
pub async fn find_by_number(pool: &PgPool, guild_id: &str, number: i64) -> anyhow::Result<Option<Confession>> {
    let Ok(n) = i32::try_from(number) else { return Ok(None) };
    Ok(sqlx::query_as::<_, Confession>(r#"SELECT * FROM "Confession" WHERE "guildId" = $1 AND "number" = $2"#)
        .bind(guild_id)
        .bind(n)
        .fetch_optional(pool)
        .await
        .map_err(bot_db::DbError::from)?)
}

/// `updateMany` with a WHERE on id (and optionally status), setting the given SQL. Returns the rows changed.
async fn update_many(pool: &PgPool, id: &str, status: Option<ConfessionStatus>, set: &str, binds: Vec<Option<String>>) -> Result<u64, bot_db::DbError> {
    let mut sql = format!(r#"UPDATE "Confession" SET {set} WHERE "id" = $1"#);
    if status.is_some() {
        sql.push_str(&format!(r#" AND "status" = ${}::"ConfessionStatus""#, binds.len() + 2));
    }
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id.to_string());
    for b in binds {
        q = q.bind(b);
    }
    if let Some(s) = status {
        q = q.bind(s.as_str());
    }
    Ok(q.execute(pool).await?.rows_affected())
}

// ── Create ────────────────────────────────────────────────────────────────────

async fn last_created<'e, E: sqlx::PgExecutor<'e>>(db: E, guild_id: &str, user_id: &str) -> Result<Option<DateTime<Utc>>, bot_db::DbError> {
    let row: Option<(chrono::NaiveDateTime,)> =
        sqlx::query_as(r#"SELECT "createdAt" FROM "Confession" WHERE "guildId" = $1 AND "authorId" = $2 ORDER BY "createdAt" DESC LIMIT 1"#)
            .bind(guild_id)
            .bind(user_id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|r| Utc.from_utc_datetime(&r.0)))
}

/// `cooldownLeftMs(guildId, userId, settings)`: the member's cooldown left in ms. Read-only (for the pre-check before
/// a form); the create re-checks under the lock.
pub async fn cooldown_left_ms(app: &App, guild_id: &str, user_id: &str, settings: &bot_db::GuildSettings) -> anyhow::Result<i64> {
    let secs = f64::from(settings.confession_cooldown_seconds);
    if secs <= 0.0 {
        return Ok(0);
    }
    let last = last_created(app.db(), guild_id, user_id).await?;
    Ok(cooldown_remaining_ms(last.as_ref(), secs, app.clock().now_ms()))
}

/// Why `createConfession` refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    Channel(String),
    Cooldown(i64),
    Send(String),
    DbError,
}

/// What `createConfession` returns on success.
#[derive(Debug, Clone)]
pub struct Created {
    pub confession: Confession,
    pub pending: bool,
}

enum Reserved {
    Cooldown(i64),
    Row(Box<Confession>),
}

/// `createConfession({ client, guild, user, content, image, settings, targets })`: reserves the numbered row (cooldown
/// and number under one per-guild advisory lock), then sends the review message (review channel set) or the post.
/// The message is sent by the bot to the channel, never as an interaction reply. A failed send removes the row.
pub async fn create_confession(
    app: &App,
    guild_id: GuildId,
    user_id: &str,
    content: &str,
    image: Option<&Image>,
    settings: &bot_db::GuildSettings,
    targets: Option<Targets>,
) -> Result<Created, CreateError> {
    let t = match targets {
        Some(t) => t,
        None => resolve_targets(app, guild_id, settings.confession_channel_id.as_deref(), settings.confession_review_channel_id.as_deref(), image.is_some())
            .await
            .map_err(CreateError::Channel)?,
    };
    let pending = t.review.is_some();
    let g = guild_id.to_string();
    let reserved: anyhow::Result<Reserved> = async {
        app.settings().ensure_guild_settings(&g).await?;
        let (gid, uid, text, post) = (g.clone(), user_id.to_string(), content.to_string(), t.post.to_string());
        let secs = f64::from(settings.confession_cooldown_seconds);
        let now = app.clock().now();
        let has_image = image.is_some();
        bot_db::transaction(app.db(), bot_db::TxOptions::TEN_SECONDS, move |tx| {
            Box::pin(async move {
                bot_db::advisory_xact_lock(tx, &format!("confession-number:{gid}")).await?;
                if secs > 0.0 {
                    let last = last_created(&mut *tx, &gid, &uid).await?;
                    let left = cooldown_remaining_ms(last.as_ref(), secs, now.timestamp_millis());
                    if left > 0 {
                        return Ok::<_, anyhow::Error>(Reserved::Cooldown(left));
                    }
                }
                let (next,): (i32,) = sqlx::query_as(NEXT_NUMBER_SQL).bind(&gid).fetch_one(&mut *tx).await.map_err(bot_db::DbError::from)?;
                let row: Confession = InsertBuilder::new(&CONFESSION_INFO)
                    .values([
                        ("guildId", json!(gid)),
                        ("number", json!(next)),
                        ("authorId", json!(uid)),
                        ("content", json!(text)),
                        ("hasImage", json!(has_image)),
                        ("status", json!(if pending { "PENDING" } else { "APPROVED" })),
                        ("channelId", json!(post)),
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
            bot_util::log_error!(format!("[confessions] {g}: reserving a confession failed ({})", err_tag(&e)));
            return Err(CreateError::DbError);
        }
    };

    let image_name = image.map(|i| i.name.as_str());
    let sent = if let Some(review) = t.review {
        let p = Payload::embed(build_review_embed(&row, image_name)).components(build_review_components(&row.id)).no_mentions();
        io(app).send(review, with_image(p, image)).await.map(|m| (review, m))
    } else {
        let p = Payload::embed(build_confession_embed(row.number, &row.content, image_name, &app.clock().now())).no_mentions();
        io(app).send(t.post, with_image(p, image)).await.map(|m| (t.post, m))
    };
    let (sent_channel, message_id) = match sent {
        Ok(v) => v,
        Err(e) => {
            delete_row_quietly(app.db(), &row.id).await;
            bot_util::log_warn!(format!("[confessions] {g}: could not post confession #{}: {}", row.number, e.message));
            return Err(CreateError::Send(send_failure_reason(e.code)));
        }
    };

    // Guarded: a delete that raced in (status DELETED, or the row gone) must not get a message id back.
    let column = if pending { "reviewMessageId" } else { "messageId" };
    let status = if pending { ConfessionStatus::Pending } else { ConfessionStatus::Approved };
    let saved = update_many(app.db(), &row.id, Some(status), &format!(r#""{column}" = $2"#), vec![Some(message_id.to_string())]).await;
    let saved = match saved {
        Ok(n) => Some(n),
        Err(e) => {
            bot_util::log_error!(format!("[confessions] {g}: could not save the message of confession #{} ({})", row.number, err_tag(&e.into())));
            None
        }
    };
    if saved.is_none_or(|n| n == 0) {
        mark_bot_deletion(app, &[message_id.to_string()], Some("confessions:rollback"), None);
        let _ = io(app).delete_message(sent_channel, message_id).await;
        if saved.is_none() {
            delete_row_quietly(app.db(), &row.id).await;
        }
        return Err(CreateError::DbError);
    }
    let mut confession = row;
    if pending {
        confession.review_message_id = Some(message_id.to_string());
    } else {
        confession.message_id = Some(message_id.to_string());
    }
    Ok(Created { confession, pending })
}

/// `CREATE_FAILURES[code](data)`.
pub fn create_failure_embed(err: &CreateError, now_ms: i64) -> CreateEmbed {
    match err {
        CreateError::Channel(reason) => channel_failure_embed(reason),
        CreateError::Cooldown(ms) => cooldown_embed(*ms, now_ms),
        CreateError::Send(reason) => error_embed("Could Not Post", Some(&format!("I couldn't post your confession: {reason}")), None),
        CreateError::DbError => db_error_embed(),
    }
}

fn channel_failure_embed(reason: &str) -> CreateEmbed {
    error_embed("Cannot Post Confessions", Some(&format!("{reason} Ask an admin to check it with `/confession setup`.")), None)
}

/// `submitFromInteraction(interaction, client, { content, attachment, settings })`: the whole submit flow after the
/// interaction was deferred (ephemerally). Never fails, and nothing in it reaches the router's error log (which
/// prints the user id).
pub async fn submit_from_interaction(ctx: &impl Interactive, content: Option<&str>, attachment: Option<AttachmentInfo>, settings: Option<Settings>) {
    let app = ctx.app();
    let reply = |embed: CreateEmbed| safe_reply(ctx.responder(), Payload::embed(embed).no_mentions());
    let Some(settings) = settings else {
        reply(settings_unavailable_embed()).await;
        return;
    };
    let user_id = ctx.user().id.to_string();
    if let Some(p) = submit_problem(&settings, Some(&user_id)) {
        reply(p).await;
        return;
    }
    let content = match parse_content(content) {
        Ok(c) => c,
        Err(ContentError::Empty) => {
            reply(error_embed("Empty Confession", Some("Write your confession first."), None)).await;
            return;
        }
        Err(ContentError::TooLong) => {
            reply(error_embed("Too Long", Some(&format!("Confessions can be at most **{}** characters.", bot_config::limits::CONFESSION_LENGTH)), None)).await;
            return;
        }
    };
    if settings.confession_block_links && contains_link(&content) {
        reply(error_embed("Links Not Allowed", Some("Confessions on this server cannot contain links or Discord invites."), None)).await;
        return;
    }
    if let Some(p) = image_problem(settings.confession_allow_images, attachment.as_ref()) {
        reply(p).await;
        return;
    }
    let Some(guild_id) = ctx.guild_id() else {
        reply(db_error_embed()).await;
        return;
    };
    let targets = match resolve_targets(app, guild_id, settings.confession_channel_id.as_deref(), settings.confession_review_channel_id.as_deref(), attachment.is_some()).await {
        Ok(t) => t,
        Err(reason) => {
            reply(channel_failure_embed(&reason)).await;
            return;
        }
    };
    match cooldown_left_ms(app, &guild_id.to_string(), &user_id, &settings).await {
        Ok(left) if left > 0 => {
            reply(cooldown_embed(left, app.clock().now_ms())).await;
            return;
        }
        Ok(_) => {}
        Err(e) => {
            bot_util::log_error!(format!("[confessions] {guild_id}: submit failed ({})", err_tag(&e)));
            reply(db_error_embed()).await;
            return;
        }
    }
    let mut image = None;
    if let Some(a) = &attachment {
        match download_image(app, &a.url).await {
            Ok(i) => image = Some(i),
            Err(message) => {
                reply(error_embed("Image Problem", Some(&message), None)).await;
                return;
            }
        }
    }
    let out = match create_confession(app, guild_id, &user_id, &content, image.as_ref(), &settings, Some(targets)).await {
        Ok(o) => o,
        Err(e) => {
            reply(create_failure_embed(&e, app.clock().now_ms())).await;
            return;
        }
    };
    let c = &out.confession;
    let mut lines = vec![if out.pending {
        format!("Your confession was sent to the moderators for review as **#{}**. It appears in <#{}> once it is approved.", c.number, targets.post)
    } else {
        match jump_url(c) {
            Some(url) => format!("Your confession is live as [#{}]({url}).", c.number),
            None => format!("Your confession is live as #{}.", c.number),
        }
    }];
    lines.push("Your name is not shown with it, and only you can see this message.".into());
    reply(success_embed(if out.pending { "Confession Sent For Review" } else { "Confession Posted" }, Some(&lines.join("\n")), None)).await;
}

// ── Opening the form ──────────────────────────────────────────────────────────

/// `settingsWithinBudget(interaction)`: the memo, or a bounded load that leaves the rest of the 3 s window.
pub async fn settings_within_budget(ctx: &impl Interactive) -> Option<Settings> {
    let g = ctx.guild_id()?.to_string();
    crate::commands::bumps::settings_in_budget_labeled(ctx.app(), &g, ack_budget_ms(ctx.interaction_id()), "confession settings").await
}

/// `openConfessionForm(interaction)`: opens the form as the FIRST response (only bounded lookups before it), or
/// answers ephemerally with what stops the member. Used by `/confession submit` without text and the panel button.
pub async fn open_confession_form(ctx: &impl Interactive) {
    let app = ctx.app();
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let settings = settings_within_budget(ctx).await;
    let user_id = ctx.user().id.to_string();
    let mut problem = settings.as_ref().and_then(|s| submit_problem(s, Some(&user_id)));
    let budget = ack_budget_ms(ctx.interaction_id());
    if problem.is_none()
        && let Some(s) = settings.clone()
        && budget > 0
    {
        // The lookup never fails outward: a database error is logged here by code only.
        let (a, gid, uid) = (app.clone(), g.clone(), user_id.clone());
        let lookup = async move {
            match cooldown_left_ms(&a, &gid, &uid, &s).await {
                Ok(v) => Ok::<_, String>(v),
                Err(e) => {
                    bot_util::log_warn!(format!("[confessions] {gid}: cooldown lookup failed ({})", err_tag(&e)));
                    Ok(0)
                }
            }
        };
        let left = bot_util::async_util::with_timeout(lookup, budget, "confession cooldown").await.unwrap_or(0);
        if left > 0 {
            problem = Some(cooldown_embed(left, app.clock().now_ms()));
        }
    }
    if let Some(p) = problem {
        safe_reply(ctx.responder(), Payload::embed(p).ephemeral(true).no_mentions()).await;
        return;
    }
    if let Err(e) = ctx.responder().show_modal(build_confession_modal()).await
        && !is_interaction_gone(&e)
    {
        bot_util::log_error!(format!("[confessions] {g}: showModal failed ({})", e.code().map(|c| format!("DiscordAPIError {c}")).unwrap_or_else(|| "Error".into())));
    }
}

/// `confession-modal`: the form. Everything is checked again here.
pub async fn handle_confession_modal(ctx: ModalCtx) -> BotResult {
    let content = ctx.value("confession-text").unwrap_or_default();
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    submit_from_interaction(&ctx, Some(&content), None, settings).await;
    Ok(())
}

// ── Review ────────────────────────────────────────────────────────────────────

/// `markReviewMessage(message, c, text, color)`: edits a review message: the status line, no buttons. The image
/// stays (the embed is rebuilt from the message's own). Best effort.
async fn mark_review_message(app: &App, message: &Message, c: &Confession, text: &str, color: Option<u32>) -> bool {
    let base: Value = match message.embeds.first() {
        Some(e) => {
            let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
            // A received embed names its image by CDN url (signed, it expires); the attachment stays on the message.
            if let Some(name) = message.attachments.first().map(|a| a.filename.clone())
                && !v["image"].is_null()
            {
                v["image"] = json!({ "url": format!("attachment://{name}") });
            }
            v
        }
        None => serde_json::to_value(build_confession_embed(c.number, &c.content, None, &app.clock().now())).unwrap_or(Value::Null),
    };
    let Some(embed) = embed_from_json(with_status_json(base, text, color)) else { return false };
    let payload = Payload::embed(embed).components(Vec::new()).no_mentions();
    match io(app).edit(message.channel_id, message.id, payload).await {
        Ok(()) => true,
        Err(e) => {
            bot_util::log_warn!(format!("[confessions] could not update the review message of #{}: {}", c.number, e.message));
            false
        }
    }
}

/// `statusLine(status, byId)`.
pub fn status_line(status: ConfessionStatus, by: Option<&str>, now_ms: i64) -> String {
    let info = status_info(status);
    let by = by.map(|b| format!(" by <@{b}>")).unwrap_or_default();
    format!("{} {}{by} {}", info.emoji, info.label, bot_util::time::discord_timestamp_ms(now_ms as f64, "R"))
}

fn has_live_buttons(message: &Message) -> bool {
    message.components.iter().any(|r| r.components.iter().any(|b| matches!(b, serenity::all::ActionRowComponent::Button(x) if !x.disabled)))
}

/// `settleStaleMessage(message, c)`: a review message whose confession is already decided loses its buttons and
/// shows how it ended.
async fn settle_stale_message(app: &App, message: &Message, c: &Confession) {
    if has_live_buttons(message) {
        let by = if c.status == ConfessionStatus::Deleted { None } else { c.reviewer_id.as_deref() };
        mark_review_message(app, message, c, &status_line(c.status, by, app.clock().now_ms()), Some(status_info(c.status).color)).await;
    }
}

async fn fetch_fresh_message(ctx: &ComponentCtx) -> Message {
    let m = &ctx.interaction.message;
    io(&ctx.app).fetch_message(m.channel_id, m.id).await.unwrap_or_else(|_| (**m).clone())
}

async fn claim(pool: &PgPool, id: &str, to: ConfessionStatus, reviewer: &str, now: &DateTime<Utc>) -> Result<u64, bot_db::DbError> {
    let sql = r#"UPDATE "Confession" SET "status" = $2::"ConfessionStatus", "reviewerId" = $3, "reviewedAt" = $4 WHERE "id" = $1 AND "status" = 'PENDING'"#;
    Ok(sqlx::query(sql).bind(id).bind(to.as_str()).bind(reviewer).bind(now.naive_utc()).execute(pool).await?.rows_affected())
}

/// `alreadyDecided(interaction, id)`: somebody else decided first: say how, and fix this message.
async fn already_decided(ctx: &ComponentCtx, id: &str) -> CreateEmbed {
    let now = find_by_id(ctx.app.db(), id).await.ok().flatten();
    if let Some(c) = &now {
        settle_stale_message(&ctx.app, &ctx.interaction.message, c).await;
    }
    match now.map(|c| status_info(c.status)) {
        Some(info) => info_embed("Already Reviewed", Some(&format!("This confession was already {}.", info.label.to_lowercase())), None),
        None => info_embed("Already Reviewed", Some("This confession was already reviewed."), None),
    }
}

/// Approve: copy the image from a fresh fetch of the review message, claim the row, post, mark.
async fn approve(ctx: &ComponentCtx, c: &Confession, settings: Option<&bot_db::GuildSettings>) -> anyhow::Result<CreateEmbed> {
    let app = &ctx.app;
    let Some(settings) = settings.filter(|s| s.confession_channel_id.as_deref().is_some_and(|c| !c.is_empty())) else {
        return Ok(error_embed("No Confession Channel", Some("Set one with `/confession setup` first. The confession stays pending."), None));
    };
    let Some(guild_id) = ctx.guild_id() else { return Ok(db_error_embed()) };
    let targets = match resolve_targets(app, guild_id, settings.confession_channel_id.as_deref(), None, c.has_image).await {
        Ok(t) => t,
        Err(reason) => return Ok(error_embed("Cannot Post", Some(&format!("{reason} The confession stays pending.")), None)),
    };
    let mut image = None;
    let mut warning = None;
    let message = fetch_fresh_message(ctx).await;
    if c.has_image {
        match message.attachments.first() {
            None => warning = Some("The review message no longer has its image, so it was posted without one."),
            Some(a) => match download_image(app, &a.url).await {
                Ok(i) => image = Some(i),
                Err(m) => return Ok(error_embed("Image Problem", Some(&format!("{m} The confession stays pending.")), None)),
            },
        }
    }
    let reviewer = ctx.user().id.to_string();
    if claim(app.db(), &c.id, ConfessionStatus::Approved, &reviewer, &app.clock().now()).await? == 0 {
        return Ok(already_decided(ctx, &c.id).await);
    }
    let p = Payload::embed(build_confession_embed(c.number, &c.content, image.as_ref().map(|i| i.name.as_str()), &app.clock().now())).no_mentions();
    let sent = match io(app).send(targets.post, with_image(p, image.as_ref())).await {
        Ok(m) => m,
        Err(e) => {
            // The post failed: hand the confession back to the queue.
            let _ = sqlx::query(r#"UPDATE "Confession" SET "status" = 'PENDING', "reviewerId" = NULL, "reviewedAt" = NULL WHERE "id" = $1 AND "status" = 'APPROVED' AND "messageId" IS NULL"#)
                .bind(&c.id)
                .execute(app.db())
                .await;
            bot_util::log_warn!(format!("[confessions] {}: could not post confession #{}: {}", c.guild_id, c.number, e.message));
            return Ok(error_embed("Could Not Post", Some(&format!("I couldn't post it: {} The confession stays pending.", send_failure_reason(e.code))), None));
        }
    };
    let saved = update_many(app.db(), &c.id, Some(ConfessionStatus::Approved), r#""messageId" = $2, "channelId" = $3"#, vec![Some(sent.to_string()), Some(targets.post.to_string())]).await;
    let saved = match saved {
        Ok(n) => n,
        Err(e) => {
            bot_util::log_error!(format!("[confessions] {}: could not save the post of #{} ({})", c.guild_id, c.number, err_tag(&e.into())));
            0
        }
    };
    if saved == 0 {
        // Deleted while it was being posted: take the post back.
        mark_bot_deletion(app, &[sent.to_string()], Some("confessions:rollback"), None);
        let _ = io(app).delete_message(targets.post, sent).await;
        return Ok(warn_embed("Confession Removed", Some(&format!("Confession #{} was deleted while it was being posted, so nothing was published.", c.number)), None));
    }
    let now_ms = app.clock().now_ms();
    mark_review_message(app, &message, c, &status_line(ConfessionStatus::Approved, Some(&reviewer), now_ms), Some(status_info(ConfessionStatus::Approved).color)).await;
    bot_util::log_info!(format!("[confessions] {}: #{} approved by {reviewer}", c.guild_id, c.number));
    let url = format!("https://discord.com/channels/{}/{}/{sent}", c.guild_id, targets.post);
    let mut lines = vec![format!("Confession #{} is live ([jump]({url})).", c.number)];
    if let Some(w) = warning {
        lines.push(format!("⚠️ {w}"));
    }
    Ok(success_embed("Approved", Some(&lines.join("\n")), None))
}

async fn reject(ctx: &ComponentCtx, c: &Confession) -> anyhow::Result<CreateEmbed> {
    let app = &ctx.app;
    let reviewer = ctx.user().id.to_string();
    if claim(app.db(), &c.id, ConfessionStatus::Rejected, &reviewer, &app.clock().now()).await? == 0 {
        return Ok(already_decided(ctx, &c.id).await);
    }
    let message = fetch_fresh_message(ctx).await;
    mark_review_message(app, &message, c, &status_line(ConfessionStatus::Rejected, Some(&reviewer), app.clock().now_ms()), Some(status_info(ConfessionStatus::Rejected).color)).await;
    bot_util::log_info!(format!("[confessions] {}: #{} rejected by {reviewer}", c.guild_id, c.number));
    Ok(success_embed("Rejected", Some(&format!("Confession #{} was rejected and will not be posted.", c.number)), None))
}

/// `confession:{id}:approve|reject`, the buttons of a review message. Moderators only, re-checked here.
async fn handle_review_button(ctx: &ComponentCtx, id: &str, action: &str) {
    let refuse = |embed: CreateEmbed| safe_reply(&ctx.responder, Payload::embed(embed).ephemeral(true).no_mentions());
    let in_cached_guild = ctx.guild_id().is_some_and(|g| cached_guild(&ctx.app, g).is_some()) && ctx.member().is_some();
    if !in_cached_guild {
        refuse(error_embed("Server Only", Some("This button only works in a server."), None)).await;
        return;
    }
    let settings = settings_within_budget(ctx).await;
    // ADMIN follows from Discord permissions and a moderator role from the settings, so missing settings never grant
    // anything.
    let level = resolve_permission_level(ctx.member_access().as_ref(), settings.as_deref(), ctx.app.config().bot_owner_id.as_deref());
    if level < PermissionLevel::Moderator {
        refuse(error_embed("Insufficient Permissions", Some("Only moderators can review confessions."), None)).await;
        return;
    }
    if !safe_defer(&ctx.responder, true).await {
        return;
    }
    let reply = |embed: CreateEmbed| safe_reply(&ctx.responder, Payload::embed(embed).no_mentions());
    let run = async {
        let c = find_by_id(ctx.app.db(), id).await?;
        let Some(c) = c.filter(|c| ctx.guild_id().map(|g| g.to_string()).as_deref() == Some(c.guild_id.as_str())) else {
            return Ok::<_, anyhow::Error>(error_embed("Confession Not Found", Some("This confession no longer exists."), None));
        };
        if c.status != ConfessionStatus::Pending {
            settle_stale_message(&ctx.app, &ctx.interaction.message, &c).await;
            return Ok(info_embed("Already Reviewed", Some(&format!("Confession #{} was already {}.", c.number, status_info(c.status).label.to_lowercase())), None));
        }
        if action == "approve" {
            let s = match settings.clone() {
                Some(s) => Some(s),
                None => ctx.app.settings().get_settings_safe(&c.guild_id).await,
            };
            approve(ctx, &c, s.as_deref()).await
        } else {
            reject(ctx, &c).await
        }
    };
    match run.await {
        Ok(embed) => {
            reply(embed).await;
        }
        Err(e) => {
            bot_util::log_error!(format!("[confessions] {}: review failed ({})", ctx.guild_id().map(|g| g.to_string()).unwrap_or_default(), err_tag(&e)));
            reply(error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)).await;
        }
    }
}

/// All `confession` buttons: `confession:new` (the panel) and `confession:{id}:approve|reject` (a review message).
pub async fn handle_button(ctx: ComponentCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    if params.len() == 1 && params[0] == "new" {
        open_confession_form(&ctx).await;
        return Ok(());
    }
    let valid = params.len() == 2 && !params[0].is_empty() && (params[1] == "approve" || params[1] == "reject");
    if !valid {
        safe_reply(&ctx.responder, Payload::text("This button is no longer active.").ephemeral(true)).await;
        return Ok(());
    }
    handle_review_button(&ctx, &params[0], &params[1]).await;
    Ok(())
}

// ── Staff: delete, block, reveal ──────────────────────────────────────────────

/// What `deleteConfession` did.
#[derive(Debug, Clone)]
pub enum Deleted {
    Ok(Box<Confession>),
    NotFound,
    Already,
    Changed,
    /// The post could not be deleted (the reason); nothing changed.
    Message(String),
}

/// `deleteConfession({ client, guild, number, settings })`: the row is claimed first (status DELETED), so a racing
/// approve cannot publish it; a post that cannot be deleted gives the claim back. The row keeps its data.
pub async fn delete_confession(app: &App, guild_id: GuildId, number: i64, settings: Option<&bot_db::GuildSettings>) -> anyhow::Result<Deleted> {
    let g = guild_id.to_string();
    let Some(c) = find_by_number(app.db(), &g, number).await? else { return Ok(Deleted::NotFound) };
    if c.status == ConfessionStatus::Deleted {
        return Ok(Deleted::Already);
    }
    if update_many(app.db(), &c.id, Some(c.status), r#""status" = 'DELETED'"#, vec![]).await? == 0 {
        return Ok(Deleted::Changed);
    }
    if let Some(mid) = c.message_id.as_deref().and_then(snowflake).map(MessageId::new) {
        let outcome = async {
            match crate::suggestion_service::resolve_channel(app, guild_id, c.channel_id.as_deref().unwrap_or("")).await? {
                crate::suggestion_service::Resolved::NoAccess => Err(IoError::new(Some(50001), "no access")),
                crate::suggestion_service::Resolved::Channel(ch) => {
                    mark_bot_deletion(app, &[mid.to_string()], Some("confessions:delete"), None);
                    io(app).delete_message(ch, mid).await
                }
                crate::suggestion_service::Resolved::Gone => Ok(()),
            }
        }
        .await;
        if let Err(e) = outcome
            && e.code != Some(UNKNOWN_MESSAGE)
            && e.code != Some(UNKNOWN_CHANNEL)
        {
            let _ = sqlx::query(r#"UPDATE "Confession" SET "status" = $2::"ConfessionStatus" WHERE "id" = $1 AND "status" = 'DELETED'"#)
                .bind(&c.id)
                .bind(c.status.as_str())
                .execute(app.db())
                .await;
            bot_util::log_warn!(format!("[confessions] {g}: could not delete the post of #{}: {}", c.number, e.message));
            return Ok(Deleted::Message(send_failure_reason(e.code)));
        }
        sqlx::query(r#"UPDATE "Confession" SET "messageId" = NULL WHERE "id" = $1"#).bind(&c.id).execute(app.db()).await.map_err(bot_db::DbError::from)?;
    }
    // The review message (kept in the review channel the settings name) shows the outcome and loses its buttons.
    if let (Some(rmid), Some(review)) = (
        c.review_message_id.as_deref().and_then(snowflake).map(MessageId::new),
        settings.and_then(|s| s.confession_review_channel_id.as_deref()).filter(|r| !r.is_empty()),
    ) && let Ok(crate::suggestion_service::Resolved::Channel(ch)) = crate::suggestion_service::resolve_channel(app, guild_id, review).await
        && let Ok(message) = io(app).fetch_message(ch, rmid).await
    {
        mark_review_message(app, &message, &c, &status_line(ConfessionStatus::Deleted, None, app.clock().now_ms()), Some(status_info(ConfessionStatus::Deleted).color)).await;
    }
    let mut out = c;
    out.status = ConfessionStatus::Deleted;
    out.message_id = None;
    Ok(Deleted::Ok(Box::new(out)))
}

/// What `setBlocked` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockResult {
    Ok,
    NotFound,
    Full,
}

#[derive(Debug)]
struct BlocklistFull;
impl std::fmt::Display for BlocklistFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("blocklist full")
    }
}
impl std::error::Error for BlocklistFull {}

/// `setBlocked({ guildId, number, blocked })`: adds or removes the author of a confession in the blocklist, without
/// ever showing who it is. The answer is the same whether or not they were already (un)blocked; at the cap a block is
/// refused whoever the author is.
pub async fn set_blocked(app: &App, guild_id: &str, number: i64, blocked: bool) -> anyhow::Result<BlockResult> {
    let Ok(n) = i32::try_from(number) else { return Ok(BlockResult::NotFound) };
    let row: Option<(String,)> = sqlx::query_as(r#"SELECT "authorId" FROM "Confession" WHERE "guildId" = $1 AND "number" = $2"#)
        .bind(guild_id)
        .bind(n)
        .fetch_optional(app.db())
        .await
        .map_err(bot_db::DbError::from)?;
    let Some((author,)) = row else { return Ok(BlockResult::NotFound) };
    let res = app
        .settings()
        .update_settings_with(guild_id, move |current| {
            let list = current.confession_blocked_user_ids.clone();
            if blocked {
                if list.len() >= bot_config::limits::CONFESSION_BLOCKED_USERS {
                    return Err(BlocklistFull.into());
                }
                if list.contains(&author) {
                    return Ok(SettingsPatch::new());
                }
                let mut next = list;
                next.push(author);
                return Ok(SettingsPatch::new().set("confessionBlockedUserIds", json!(next)));
            }
            if list.contains(&author) {
                let next: Vec<String> = list.into_iter().filter(|id| *id != author).collect();
                Ok(SettingsPatch::new().set("confessionBlockedUserIds", json!(next)))
            } else {
                Ok(SettingsPatch::new())
            }
        })
        .await;
    match res {
        Ok(_) => Ok(BlockResult::Ok),
        Err(e) if e.downcast_ref::<BlocklistFull>().is_some() => Ok(BlockResult::Full),
        Err(e) => Err(e),
    }
}

/// What `revealConfession` did.
#[derive(Debug, Clone)]
pub enum Revealed {
    Ok(Box<Confession>),
    NotFound,
    NoLog(String),
    LogFailed(String),
}

/// `revealConfession({ guild, number, admin })`: writes the reveal to the moderation log, then (and only then)
/// returns the author. The entry names the admin and the number, never the author. Without a usable moderation log
/// channel nothing is revealed.
pub async fn reveal_confession(app: &App, guild_id: GuildId, number: i64, admin_id: &str) -> anyhow::Result<Revealed> {
    let g = guild_id.to_string();
    let Some(c) = find_by_number(app.db(), &g, number).await? else { return Ok(Revealed::NotFound) };
    let settings = app.settings().get_settings_safe(&g).await;
    let Some(log_id) = settings.as_ref().and_then(|s| s.mod_log_channel_id.clone()).filter(|l| !l.is_empty()) else {
        return Ok(Revealed::NoLog("No moderation log channel is set.".into()));
    };
    // The cache, else `guild.channels.fetch` (a failed fetch is no channel).
    let channel = snowflake(&log_id).map(ChannelId::new);
    let usable = bot_core::interaction_helpers::check_channel_usable_fetch(app, guild_id, channel, &CHANNEL_POST_NEEDS).await;
    let Some(channel) = channel.filter(|_| usable.ok) else {
        return Ok(Revealed::NoLog(usable.reason.unwrap_or_else(|| "The moderation log channel is unusable.".into())));
    };
    let embed = neutral_embed(Some("🕵️ Confession author revealed"), Some(&format!("<@{admin_id}> looked up the author of **Confession #{number}**.")), Some(&format!("Admin ID: {admin_id}")));
    if let Err(e) = io(app).send(channel, Payload::embed(embed).no_mentions()).await {
        let code = e.code.map(|c| c.to_string()).unwrap_or_else(|| "DiscordAPIError".into());
        bot_util::log_warn!(format!("[confessions] {g}: the reveal of #{number} was not logged ({code}), so nothing was revealed"));
        return Ok(Revealed::LogFailed(send_failure_reason(e.code)));
    }
    bot_util::log_info!(format!("[confessions] {g}: author of #{number} revealed by {admin_id}"));
    Ok(Revealed::Ok(Box::new(c)))
}

/// The reveal reply's view of the author (the only place an author is shown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorView {
    pub id: String,
    pub name: Option<String>,
    pub in_server: bool,
}

/// `describeAuthor(client, guild, authorId)`.
pub async fn describe_author(app: &App, guild_id: GuildId, author_id: &str) -> AuthorView {
    let uid = snowflake(author_id).map(UserId::new);
    let member = uid.and_then(|u| cached_guild(app, guild_id).and_then(|g| g.members.get(&u).cloned()));
    let mut user = member.as_ref().map(|m| m.user.clone()).or_else(|| uid.and_then(|u| app.discord().and_then(|d| d.cache.user(u).map(|x| x.clone()))));
    if user.is_none()
        && let Some(u) = uid
    {
        user = io(app).fetch_user(u).await.ok();
    }
    let name = member.as_ref().map(bot_core::adapters::member_display_name).or_else(|| user.as_ref().map(bot_core::adapters::user_display_name));
    AuthorView { id: author_id.to_string(), name, in_server: member.is_some() }
}

// ── Hooks ─────────────────────────────────────────────────────────────────────

/// `onMessageDelete(ctx)`: a post or review message vanished. The row keeps its data; only the message id is
/// cleared.
pub async fn on_message_delete(app: &App, guild_id: &str, message_id: &str) {
    let run = async {
        sqlx::query(r#"UPDATE "Confession" SET "messageId" = NULL WHERE "guildId" = $1 AND "messageId" = $2"#).bind(guild_id).bind(message_id).execute(app.db()).await?;
        sqlx::query(r#"UPDATE "Confession" SET "reviewMessageId" = NULL WHERE "guildId" = $1 AND "reviewMessageId" = $2"#)
            .bind(guild_id)
            .bind(message_id)
            .execute(app.db())
            .await?;
        Ok::<_, sqlx::Error>(())
    };
    if let Err(e) = run.await {
        bot_util::log_error!(format!("[confessions] {guild_id}: clearing a deleted message failed ({})", err_tag(&bot_db::DbError::from(e).into())));
    }
}

/// The settings fields a deleted channel unsets (`onChannelDelete`'s decision). When the REVIEW channel is deleted
/// the confession channel is unset too: clearing only the review channel would make every confession public
/// unreviewed.
pub fn channel_delete_update(settings: &bot_db::GuildSettings, channel_id: &str) -> Vec<&'static str> {
    let mut keys = Vec::new();
    if settings.confession_channel_id.as_deref() == Some(channel_id) {
        keys.push("confessionChannelId");
    }
    if settings.confession_review_channel_id.as_deref() == Some(channel_id) {
        keys.push("confessionReviewChannelId");
        if !keys.contains(&"confessionChannelId") {
            keys.push("confessionChannelId");
        }
    }
    keys
}

/// `onChannelDelete(ctx)`: the confession channel is unset; posts in the deleted channel lose their message id;
/// confessions that were waiting for review lose their review message when the review channel goes.
pub async fn on_channel_delete(app: &App, guild_id: &str, channel_id: &str) {
    let run = async {
        let settings = app.settings().get_settings_with(guild_id, true).await?;
        let keys = channel_delete_update(&settings, channel_id);
        if keys.contains(&"confessionReviewChannelId") {
            sqlx::query(r#"UPDATE "Confession" SET "reviewMessageId" = NULL WHERE "guildId" = $1 AND "status" = 'PENDING' AND "reviewMessageId" IS NOT NULL"#)
                .bind(guild_id)
                .execute(app.db())
                .await
                .map_err(bot_db::DbError::from)?;
        }
        if !keys.is_empty() {
            let mut patch = SettingsPatch::new();
            for k in &keys {
                patch.insert(k, Value::Null);
            }
            app.settings().update_settings(guild_id, patch).await?;
            let verb = if keys.len() > 1 { "are" } else { "is" };
            bot_util::log_info!(format!("[confessions] {guild_id}: a confession channel was deleted, so {} {verb} unset", keys.join(" and ")));
        }
        sqlx::query(r#"UPDATE "Confession" SET "messageId" = NULL WHERE "guildId" = $1 AND "channelId" = $2 AND "messageId" IS NOT NULL"#)
            .bind(guild_id)
            .bind(channel_id)
            .execute(app.db())
            .await
            .map_err(bot_db::DbError::from)?;
        Ok::<_, anyhow::Error>(())
    };
    if let Err(e) = run.await {
        bot_util::log_error!(format!("[confessions] {guild_id}: channel delete cleanup failed ({})", err_tag(&e)));
    }
}
