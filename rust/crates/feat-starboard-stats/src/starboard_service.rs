//! Port of `src/services/starboardService.js`: starboards (MEE6 Starboards). Pure helpers, the per-guild board cache,
//! the CRUD behind `/starboard` and the sync engine the plugin hooks drive.
//!
//! What the JS kept in module-level variables (the board memo, the generations, the coalescer, the throttles) lives in
//! [`StarboardState`] (`app.ext`). Discord goes through [`crate::starboard_io`].

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use bot_core::App;
use bot_core::deletion_tracker::{mark_bot_deletion, was_deleted_by_bot};
use bot_db::{DbError, InsertBuilder, STARBOARD_ENTRY_INFO, STARBOARD_INFO, SelectBuilder, Starboard, StarboardEntry, TxOptions, UpdateBuilder};
use bot_util::text::{clamp_text, format_number, join_with_limit};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::all::{Member, Message, MessageReaction, ReactionType};

use crate::discord_io::{CappedMap, DiscordError, is_snowflake, system_now_ms};
use crate::starboard_io::{ReactUser, SbChannel, SbEmoji, SbGuild, starboard_io};

macro_rules! sql_now {
    () => {
        "(now() AT TIME ZONE 'UTC')"
    };
}

pub const STAR_COLOR: u32 = 0xFFAC33;
pub const MAX_EMOJIS_PER_BOARD: usize = 10;
pub const MAX_IGNORED_CHANNELS: usize = 50;
/// 100 users per page -> at most 1000 users fetched per reaction.
pub const MAX_USER_PAGES: usize = 10;
/// Concurrent syncs per shard.
pub const SYNC_CONCURRENCY: usize = 4;
/// A message that was synced a moment ago waits this long before its next sync.
pub const SYNC_SPACING_MS: i64 = 1500;
/// A message that already has a repost waits longer (the last sync of a burst still runs).
pub const REPOST_SYNC_SPACING_MS: i64 = 10_000;
const REPOSTED_TTL_MS: i64 = 10 * 60_000;
const REPOSTED_MAX: usize = 5_000;
pub const BOARD_MEMO_TTL_MS: i64 = 60_000;
const BOARD_CACHE_TTL_S: u64 = 300;
const BOARD_MEMO_MAX: usize = 10_000;
const DAY_MS: i64 = 86_400_000;
const SNOWFLAKE_EPOCH: i64 = 1_420_070_400_000;
const COUNT_MAX: i64 = 1_000_000_000;
/// Characters `/starboard list` may spend on field values (Discord's 6000-character embed total).
pub const LIST_EMBED_BUDGET: usize = 5600;

// ── Errors ────────────────────────────────────────────────────────────────────

/// `BoardLimitError`, `NotFoundError`, `IgnoreLimitError`, `SettingsConflictError`, or a database error (a duplicate name
/// is a [`DbError`] for which [`BoardError::is_unique_violation`] is true, rethrown unchanged like the JS P2002).
#[derive(Debug)]
pub enum BoardError {
    Limit,
    NotFound,
    IgnoreLimit,
    SettingsConflict,
    Db(DbError),
    Other(anyhow::Error),
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit => write!(f, "A server can have at most {} starboards.", bot_config::limits::STARBOARDS),
            Self::NotFound => f.write_str("Starboard not found."),
            Self::IgnoreLimit => write!(f, "A starboard can ignore at most {MAX_IGNORED_CHANNELS} channels."),
            Self::SettingsConflict => f.write_str("Self-stars are counted on this board, so they cannot also be removed."),
            Self::Db(e) => write!(f, "{e}"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BoardError {}

impl From<DbError> for BoardError {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}

impl From<bot_db::sqlx::Error> for BoardError {
    fn from(e: bot_db::sqlx::Error) -> Self {
        Self::Db(DbError::from(e))
    }
}

impl BoardError {
    /// `isUniqueViolation(err)` (Prisma P2002).
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::Db(e) if e.is_unique_violation())
    }
}

/// `isUniqueViolation(err)` for any error.
pub fn is_unique_violation(err: &anyhow::Error) -> bool {
    if let Some(b) = err.downcast_ref::<BoardError>() {
        return b.is_unique_violation();
    }
    err.downcast_ref::<DbError>().is_some_and(DbError::is_unique_violation)
}

// ── Per-app state ─────────────────────────────────────────────────────────────

type SyncFuture = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
/// The test seam of `__setSyncImpl`: replaces the sync the reaction hook enqueues.
pub type SyncImpl = Arc<dyn Fn(App, SyncRequest) -> SyncFuture + Send + Sync>;

/// The module-level state of the JS service, one per app.
pub struct StarboardState {
    memo: Mutex<CappedMap<(i64, Arc<Vec<BoardLite>>)>>,
    generations: Mutex<CappedMap<u64>>,
    reposted: Arc<Mutex<CappedMap<i64>>>,
    /// The per-message mutex shared by syncs, edits and deletes.
    pub mutex: bot_core::sync::KeyedMutex,
    semaphore: bot_util::async_util::Semaphore,
    coalescer: Coalescer,
    allow_board_warn: bot_util::async_util::Throttle,
    allow_perm_warn: bot_util::async_util::Throttle,
    allow_read_warn: bot_util::async_util::Throttle,
    sync_impl: RwLock<Option<SyncImpl>>,
}

impl Default for StarboardState {
    fn default() -> Self {
        let mutex = bot_core::sync::KeyedMutex::new();
        let semaphore = bot_util::async_util::create_semaphore(SYNC_CONCURRENCY);
        let reposted: Arc<Mutex<CappedMap<i64>>> = Arc::new(Mutex::new(CappedMap::new(REPOSTED_MAX)));
        let r = reposted.clone();
        let coalescer = Coalescer::new(mutex.clone(), semaphore.clone(), Arc::new(move |key: &str| spacing_from(&r, key, system_now_ms())));
        Self {
            memo: Mutex::new(CappedMap::new(BOARD_MEMO_MAX)),
            generations: Mutex::new(CappedMap::new(BOARD_MEMO_MAX * 2)),
            reposted,
            mutex,
            semaphore,
            coalescer,
            allow_board_warn: bot_util::async_util::create_throttle(60_000),
            allow_perm_warn: bot_util::async_util::create_throttle(600_000),
            allow_read_warn: bot_util::async_util::create_throttle(600_000),
            sync_impl: RwLock::new(None),
        }
    }
}

fn state(app: &App) -> Arc<StarboardState> {
    app.ext::<StarboardState>()
}

/// `__setSyncImpl(fn)`: test seam; `None` restores the real sync.
pub fn set_sync_impl(app: &App, f: Option<SyncImpl>) {
    *state(app).sync_impl.write().unwrap_or_else(|e| e.into_inner()) = f;
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `logSyncError(err)`.
pub fn log_sync_error(err: &anyhow::Error) {
    bot_util::log_error!("starboard sync failed", bot_core::error::log_arg(err));
}

/// logger.warn throttled per board and error code (10 minutes).
fn warn_once(app: &App, board: &BoardLite, code: &str, msg: String) {
    if state(app).allow_perm_warn.allow(&format!("{}:{code}", board.id)) {
        bot_util::log_warn!(msg);
    }
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// `normalizeBoardName(input)`: lowercase `[a-z0-9_-]`, 1-32 characters, or `None`.
pub fn normalize_board_name(input: &str) -> Option<String> {
    let lowered = bot_util::js::lower(bot_util::js::trim(input));
    let mut dashed = String::new();
    let mut in_space = false;
    for c in lowered.chars() {
        if bot_util::js::is_js_space(c) {
            if !in_space {
                dashed.push('-');
            }
            in_space = true;
        } else {
            dashed.push(c);
            in_space = false;
        }
    }
    let kept: String = dashed.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-').collect();
    let mut collapsed = String::new();
    for c in kept.chars() {
        if c == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(c);
    }
    let trimmed = collapsed.trim_matches('-');
    let n: String = trimmed.chars().take(32).collect();
    if n.is_empty() { None } else { Some(n) }
}

/// `normalizeEmojiKey(key)`: strips the emoji variation selector (U+FE0F), so '❤' and '❤️' are one emoji.
pub fn normalize_emoji_key(key: &str) -> String {
    key.replace('\u{FE0F}', "")
}

/// `emojiKeyOf(emoji)`: the custom emoji id, or the unicode emoji itself.
pub fn emoji_key_of(emoji: &ReactionType) -> String {
    bot_core::hooks::events::emoji_key(emoji)
}

/// `boardMatchesEmoji(board, key)`.
pub fn board_matches_emoji(board: &BoardLite, key: Option<&str>) -> bool {
    if board.emojis.is_empty() {
        return true;
    }
    let Some(key) = key else { return false };
    let k = normalize_emoji_key(key);
    board.emojis.iter().any(|e| normalize_emoji_key(e) == k)
}

fn unicode_emoji_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(concat!(
            r"^(?:\p{Regional_Indicator}{2}|[#*0-9]\x{FE0F}?\x{20E3}|(?:\p{Extended_Pictographic}|\p{Emoji_Presentation})",
            r"(?:\x{FE0F}|\p{Emoji_Modifier})?(?:[\x{E0020}-\x{E007E}]+\x{E007F})?",
            r"(?:\x{200D}(?:\p{Extended_Pictographic}|\p{Emoji_Presentation})(?:\x{FE0F}|\p{Emoji_Modifier})?)*)$"
        ))
        .unwrap_or_else(|e| panic!("UNICODE_EMOJI_RE does not compile: {e}"))
    })
}

/// `isUnicodeEmoji(grapheme)`: one grapheme is a unicode emoji.
pub fn is_unicode_emoji(grapheme: &str) -> bool {
    unicode_emoji_re().is_match(grapheme)
}

fn find_guild_emoji<'a>(emojis: &'a [SbEmoji], name: &str) -> Option<&'a SbEmoji> {
    let lower = bot_util::js::lower(name);
    let mut loose = None;
    for e in emojis {
        if e.name == name {
            return Some(e);
        }
        if loose.is_none() && bot_util::js::lower(&e.name) == lower {
            loose = Some(e);
        }
    }
    loose
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Replaces every `<a?:\w{1,32}:(\d{17,20})>` with ` $1 `.
fn strip_custom_emoji_markup(s: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"<a?:[A-Za-z0-9_]{1,32}:([0-9]{17,20})>").unwrap_or_else(|e| panic!("{e}")));
    re.replace_all(s, " $1 ").into_owned()
}

/// `parseEmojiList(input, guild)`: unicode emojis, custom emojis (`<:name:id>`, `:name:` or a bare id) or "any".
/// `Ok(vec![])` is "any emoji"; `Err` holds the message to show.
pub fn parse_emoji_list(input: &str, guild_emojis: &[SbEmoji]) -> Result<Vec<String>, String> {
    use unicode_segmentation::UnicodeSegmentation;
    let s = bot_util::js::trim(input);
    let empty = || "Give at least one emoji, or \"any\" for every emoji.".to_string();
    if s.is_empty() {
        return Err(empty());
    }
    let low = s.to_ascii_lowercase();
    if low == "any" || low == "all" || s == "*" {
        return Ok(Vec::new());
    }
    let replaced = strip_custom_emoji_markup(s);
    let tokens: Vec<&str> = replaced.split(|c: char| c == ',' || bot_util::js::is_js_space(c)).filter(|t| !t.is_empty()).collect();
    let mut found: Vec<String> = Vec::new();
    for token in tokens {
        if is_snowflake(token) {
            found.push(token.to_string());
            continue;
        }
        let inner = token.strip_prefix(':').and_then(|t| t.strip_suffix(':'));
        if let Some(name) = inner.filter(|n| (1..=32).contains(&n.len()) && n.chars().all(is_word)) {
            match find_guild_emoji(guild_emojis, name) {
                Some(e) => found.push(e.id.clone()),
                None => return Err(format!("I can't find a server emoji called :{name}:. Use the emoji itself.")),
            }
            continue;
        }
        for segment in token.graphemes(true) {
            if !is_unicode_emoji(segment) {
                return Err(format!("\"{}\" is not an emoji.", clamp_text(token, 40)));
            }
            found.push(segment.to_string());
        }
    }
    if found.is_empty() {
        return Err(empty());
    }
    let mut seen = HashSet::new();
    let mut emojis = Vec::new();
    for e in found {
        if seen.insert(normalize_emoji_key(&e)) {
            emojis.push(e);
        }
    }
    if emojis.len() > MAX_EMOJIS_PER_BOARD {
        return Err(format!("A starboard can use at most {MAX_EMOJIS_PER_BOARD} emojis."));
    }
    Ok(emojis)
}

/// `emoji.toString()` of a reaction emoji.
pub fn reaction_text(emoji: &ReactionType) -> String {
    match emoji {
        ReactionType::Custom { animated, id, name } => {
            format!("<{}:{}:{}>", if *animated { "a" } else { "" }, name.as_deref().unwrap_or("null"), id)
        }
        ReactionType::Unicode(s) => s.clone(),
        _ => String::new(),
    }
}

/// `formatEmojiKey(key, { client, message })`: unicode as is; an id as the custom emoji when it can be resolved (from
/// the message's reactions, then the client's emoji cache), else a star.
pub fn format_emoji_key(key: &str, custom: &dyn Fn(&str) -> Option<SbEmoji>, reactions: Option<&[MessageReaction]>) -> String {
    if !is_snowflake(key) {
        return key.to_string();
    }
    for r in reactions.unwrap_or(&[]) {
        if let ReactionType::Custom { id, .. } = &r.reaction_type
            && id.to_string() == key
        {
            return reaction_text(&r.reaction_type);
        }
    }
    custom(key).map(|e| e.mention()).unwrap_or_else(|| "⭐".to_string())
}

/// `formatEmojiList(keys, client)`.
pub fn format_emoji_list(keys: &[String], custom: &dyn Fn(&str) -> Option<SbEmoji>) -> String {
    if keys.is_empty() {
        return "any emoji".to_string();
    }
    keys.iter().map(|k| format_emoji_key(k, custom, None)).collect::<Vec<_>>().join(" ")
}

/// [`format_emoji_list`] with the app's emoji cache.
pub fn format_emoji_list_app(app: &App, keys: &[String]) -> String {
    let io = starboard_io(app);
    format_emoji_list(keys, &|id| io.custom_emoji(app, id))
}

/// `snowflakeTime(id)`: creation time (ms) of a snowflake.
pub fn snowflake_time(id: &str) -> i64 {
    id.parse::<u64>().map(|v| ((v >> 22) as i64) + SNOWFLAKE_EPOCH).unwrap_or(SNOWFLAKE_EPOCH)
}

/// `channelChain(channel)`: the channel, its parent channel or category, and a thread's category.
pub fn channel_chain(channel: &SbChannel) -> Vec<String> {
    let mut chain = vec![channel.id.clone()];
    if let Some(p) = channel.parent_id.as_ref().filter(|p| !p.is_empty()) {
        chain.push(p.clone());
    }
    if channel.is_thread
        && let Some(pp) = channel.parent_parent_id.as_ref().filter(|p| !p.is_empty())
    {
        chain.push(pp.clone());
    }
    chain
}

/// `isNsfwChannel(channel)`.
pub fn is_nsfw_channel(channel: &SbChannel) -> bool {
    if channel.is_thread { channel.parent_nsfw } else { channel.nsfw }
}

/// `sourceSkipReason(...)`: why a channel cannot feed this board (`board-channel`, `ignored`, `nsfw`, `private`).
/// `leaks_private` is `copyLeaksPrivate(channel, boardChannel)`, asked last.
pub fn source_skip_reason(
    board: &BoardLite,
    channel: &SbChannel,
    board_channel_ids: &HashSet<String>,
    board_channel: Option<&SbChannel>,
    leaks_private: impl FnOnce() -> bool,
) -> Option<&'static str> {
    let chain = channel_chain(channel);
    if chain.iter().any(|id| board_channel_ids.contains(id)) {
        return Some("board-channel");
    }
    if chain.iter().any(|id| board.ignored_channel_ids.contains(id)) {
        return Some("ignored");
    }
    if is_nsfw_channel(channel) && !(board.allow_nsfw && board_channel.is_some_and(|c| c.nsfw)) {
        return Some("nsfw");
    }
    if leaks_private() {
        return Some("private");
    }
    None
}

/// discord.js `message.system`: not a default, reply or application-command message.
pub fn is_system_message(message: &Message) -> bool {
    !matches!(u8::from(message.kind), 0 | 19 | 20 | 23)
}

/// `messageSkipReason(...)`: why a message cannot be reposted (`system`, `own-bot`, `bot-author`, `too-old`).
pub fn message_skip_reason(board: &BoardLite, message: &Message, has_entry: bool, now: i64, bot_user_id: &str) -> Option<&'static str> {
    if is_system_message(message) {
        return Some("system");
    }
    if message.author.id.to_string() == bot_user_id {
        return Some("own-bot");
    }
    if !has_entry && board.ignore_bots && (message.author.bot || message.webhook_id.is_some()) {
        return Some("bot-author");
    }
    if !has_entry && board.max_message_age_days > 0 && now - snowflake_time(&message.id.to_string()) > board.max_message_age_days * DAY_MS {
        return Some("too-old");
    }
    None
}

/// One reaction's users, as [`fetch_reaction_users`] returns them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UserList {
    pub users: Vec<ReactUser>,
    /// The unverified remainder of a truncated list.
    pub extra: i64,
}

/// `computeCount(...)`: distinct users across a board's qualifying reactions, after exclusions.
pub fn compute_count(board: &BoardLite, user_lists: &[UserList], author_id: Option<&str>, bot_user_id: Option<&str>) -> i64 {
    let mut union: indexmap::IndexMap<&str, &ReactUser> = indexmap::IndexMap::new();
    let mut extra = 0i64;
    for list in user_lists {
        for u in &list.users {
            union.insert(u.id.as_str(), u);
        }
        extra += list.extra;
    }
    let mut total = 0i64;
    for (id, u) in union {
        if Some(id) == bot_user_id {
            continue;
        }
        if board.ignore_bots && u.bot {
            continue;
        }
        if !board.self_star && Some(id) == author_id {
            continue;
        }
        total += 1;
    }
    (total + extra).min(COUNT_MAX)
}

/// What to do for one (board, message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    Create,
    Repost,
    Update,
    Remove,
    Store,
    None,
}

impl SyncAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Repost => "repost",
            Self::Update => "update",
            Self::Remove => "remove",
            Self::Store => "store",
            Self::None => "none",
        }
    }
}

/// The fields of a `StarboardEntry` the decision reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EntryState {
    pub blocked: bool,
    pub board_message_id: Option<String>,
    pub count: i64,
}

impl From<&StarboardEntry> for EntryState {
    fn from(e: &StarboardEntry) -> Self {
        Self { blocked: e.blocked, board_message_id: e.board_message_id.clone(), count: i64::from(e.count) }
    }
}

/// `decideAction({ entry, count, board })`.
pub fn decide_action(entry: Option<&EntryState>, count: i64, board: &BoardLite) -> SyncAction {
    let Some(entry) = entry else {
        return if count >= board.threshold { SyncAction::Create } else { SyncAction::None };
    };
    // A repost that staff deleted by hand is never reposted.
    if entry.blocked {
        return if count != entry.count { SyncAction::Store } else { SyncAction::None };
    }
    if entry.board_message_id.is_some() {
        if count < board.threshold && board.remove_below_threshold {
            return SyncAction::Remove;
        }
        return if count != entry.count { SyncAction::Update } else { SyncAction::None };
    }
    if count >= board.threshold {
        return SyncAction::Repost;
    }
    if count != entry.count { SyncAction::Store } else { SyncAction::None }
}

/// `pickDisplayEmoji({ board, qualifying, client, message })`.
pub fn pick_display_emoji(board: &BoardLite, qualifying: &[MessageReaction], custom: &dyn Fn(&str) -> Option<SbEmoji>, reactions: Option<&[MessageReaction]>) -> String {
    if let Some(first) = board.emojis.first() {
        return format_emoji_key(first, custom, reactions);
    }
    let mut best: Option<&MessageReaction> = None;
    for r in qualifying {
        if best.is_none_or(|b| r.count > b.count) {
            best = Some(r);
        }
    }
    best.map(|b| reaction_text(&b.reaction_type)).unwrap_or_else(|| "⭐".to_string())
}

/// `buildCountLine({ emojiText, count, sourceChannelId })`.
pub fn build_count_line(emoji_text: &str, count: i64, source_channel_id: &str) -> String {
    format!("{emoji_text} **{}** · <#{source_channel_id}>", format_number(count as f64))
}

fn is_spoiler_name(name: &str) -> bool {
    name.starts_with("SPOILER_")
}

fn is_image_attachment(content_type: Option<&str>, name: &str) -> bool {
    if content_type.unwrap_or("").starts_with("image/") {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|ext| lower.ends_with(ext))
}

/// discord.js `sticker.url` (`StickerFormatExtensionMap`: PNG and APNG are png, GIF is gif from the media proxy,
/// Lottie is json).
pub fn sticker_url(id: &str, format: u8) -> String {
    match format {
        4 => format!("https://media.discordapp.net/stickers/{id}.gif"),
        3 => format!("https://cdn.discordapp.com/stickers/{id}.json"),
        _ => format!("https://cdn.discordapp.com/stickers/{id}.png"),
    }
}

/// An image to show: its URL and, for an attachment, the attachment id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedImage {
    pub url: String,
    pub attachment_id: Option<String>,
}

/// `selectImage(message)`: the first usable image of a message.
pub fn select_image(message: &Message) -> Option<SelectedImage> {
    for att in &message.attachments {
        if !is_spoiler_name(&att.filename) && is_image_attachment(att.content_type.as_deref(), &att.filename) {
            return Some(SelectedImage { url: att.url.clone(), attachment_id: Some(att.id.to_string()) });
        }
    }
    for embed in &message.embeds {
        if let Some(img) = &embed.image {
            return Some(SelectedImage { url: img.url.clone(), attachment_id: None });
        }
        let kind = embed.kind.as_deref();
        if matches!(kind, Some("image") | Some("gifv"))
            && let Some(t) = &embed.thumbnail
        {
            return Some(SelectedImage { url: t.url.clone(), attachment_id: None });
        }
    }
    for s in &message.sticker_items {
        let format = u8::from(s.format_type);
        if format != 3 {
            return Some(SelectedImage { url: sticker_url(&s.id.to_string(), format), attachment_id: None });
        }
    }
    None
}

/// The message's link (`message.url`).
pub fn message_url(guild_id: &str, channel_id: &str, message_id: &str) -> String {
    format!("https://discord.com/channels/{guild_id}/{channel_id}/{message_id}")
}

/// The author line's avatar: `member.displayAvatarURL({ extension: 'png', size: 128 })` (server avatar first), else the
/// user's.
fn author_icon_url(guild_id: &str, message: &Message, member: Option<&Member>) -> String {
    use bot_core::adapters::{ImageUrlOptions, member_display_avatar_url_with, user_display_avatar_url_with};
    let opts = || ImageUrlOptions::new("png", Some(128));
    if let (Some(m), Some(g)) = (member, crate::discord_io::parse_id(guild_id)) {
        return member_display_avatar_url_with(serenity::all::GuildId::new(g), m, opts());
    }
    match message.member.as_ref().and_then(|m| m.avatar.as_ref()) {
        Some(h) => {
            let h = h.to_string();
            let ext = if h.starts_with("a_") { "gif" } else { "png" };
            format!("https://cdn.discordapp.com/guilds/{guild_id}/users/{}/avatars/{h}.{ext}?size=128", message.author.id)
        }
        None => user_display_avatar_url_with(&message.author, opts()),
    }
}

/// The author line's name: `message.member.displayName` (the nickname, else the user's display name), else the
/// author's display name.
fn author_name(message: &Message, member: Option<&Member>) -> String {
    let display = |u: &serenity::all::User| u.global_name.clone().unwrap_or_else(|| u.name.clone());
    match member {
        Some(m) => m.nick.clone().unwrap_or_else(|| display(&m.user)),
        None => message.member.as_ref().and_then(|m| m.nick.clone()).unwrap_or_else(|| display(&message.author)),
    }
}

/// `buildRepostPayload({ message, count, emojiText })`: the repost message as Discord's API JSON (the discord.js
/// `toJSON()` of the embed and the row, `allowed_mentions: { parse: [] }`). It mirrors user content, so it carries the
/// source's timestamp and no title. The author is the message payload's member, else the user; see
/// [`build_repost_payload_with`] for the cached member discord.js reads.
pub fn build_repost_payload(guild_id: &str, message: &Message, count: i64, emoji_text: &str) -> Value {
    build_repost_payload_with(guild_id, message, None, count, emoji_text)
}

/// [`build_repost_payload`] with the author's cached guild member: discord.js `message.member` is
/// `guild.members.resolve(author)`, so its nickname and server avatar are shown even for a message fetched over REST
/// (which carries no member).
pub fn build_repost_payload_with(guild_id: &str, message: &Message, member: Option<&Member>, count: i64, emoji_text: &str) -> Value {
    let name = clamp_text(&author_name(message, member), 256);
    let name = if name.is_empty() { "Unknown".to_string() } else { name };
    let mut embed = serde_json::Map::new();
    embed.insert("color".into(), json!(STAR_COLOR));
    embed.insert("author".into(), json!({ "name": name, "icon_url": author_icon_url(guild_id, message, member) }));

    let mut text = message.content.clone();
    if text.is_empty()
        && let Some(e) = message.embeds.first()
    {
        text = e.description.clone().or_else(|| e.title.clone()).unwrap_or_default();
    }
    for s in &message.sticker_items {
        text = bot_util::js::trim(&format!("{text}\n\nSticker: **{}**", s.name)).to_string();
    }
    if !text.is_empty() {
        embed.insert("description".into(), json!(clamp_text(&text, 4096)));
    }
    let img = select_image(message);
    if let Some(i) = &img {
        embed.insert("image".into(), json!({ "url": i.url }));
    }
    let mut items: Vec<String> = Vec::new();
    for att in &message.attachments {
        if img.as_ref().is_some_and(|i| i.attachment_id.as_deref() == Some(att.id.to_string().as_str())) {
            continue;
        }
        let label = clamp_text(&att.filename.replace(['[', ']'], ""), 80);
        let label = if label.is_empty() { "file".to_string() } else { label };
        let link = format!("[{label}]({})", att.url);
        items.push(if is_spoiler_name(&att.filename) { format!("||{link}||") } else { link });
    }
    if !items.is_empty() {
        embed.insert("fields".into(), json!([{ "name": "Attachments", "value": join_with_limit(&items, "\n", 1024, bot_util::text::default_more) }]));
    }
    let ts = message.timestamp.timestamp_millis();
    embed.insert("timestamp".into(), json!(bot_util::js::to_iso_string(ts as f64).unwrap_or_default()));

    let url = message_url(guild_id, &message.channel_id.to_string(), &message.id.to_string());
    json!({
        "content": clamp_text(&build_count_line(emoji_text, count, &message.channel_id.to_string()), 2000),
        "embeds": [Value::Object(embed)],
        "components": [{ "type": 1, "components": [{ "type": 2, "style": 5, "label": "Jump to message", "url": url }] }],
        "allowed_mentions": { "parse": [] },
    })
}

// ── The coalescer ─────────────────────────────────────────────────────────────

/// The spacing function of a [`Coalescer`]: asked when a run ends.
pub type SpacingFor = Arc<dyn Fn(&str) -> i64 + Send + Sync>;

#[derive(Default)]
struct CoalescerInner {
    waiting: HashSet<String>,
    /// key -> earliest start of its next run.
    next_at: HashMap<String, i64>,
}

/// `createCoalescer({ mutex, semaphore, spacingFor })`: per key at most one running sync and one waiting sync. A
/// request while one is already waiting returns `None`, because the waiting run reads the newest Discord state anyway.
/// Lock order is always mutex, then semaphore. A run waits until `spacing_for(key)` after the previous run of the key
/// ended. Times are the system clock (the spacing is about real Discord rate limits).
#[derive(Clone)]
pub struct Coalescer {
    mutex: bot_core::sync::KeyedMutex,
    semaphore: bot_util::async_util::Semaphore,
    spacing_for: SpacingFor,
    inner: Arc<Mutex<CoalescerInner>>,
}

enum Attempt<T> {
    Wait(i64),
    Value(T),
}

impl Coalescer {
    pub fn new(mutex: bot_core::sync::KeyedMutex, semaphore: bot_util::async_util::Semaphore, spacing_for: SpacingFor) -> Self {
        Self { mutex, semaphore, spacing_for, inner: Arc::new(Mutex::new(CoalescerInner::default())) }
    }

    /// Requests a run of `f` for `key`; `None` when a run of the key is already waiting. The first attempt takes its
    /// place in the mutex now.
    pub fn request<T, F, Fut>(&self, key: &str, f: F) -> Option<impl Future<Output = T> + Send + 'static + use<T, F, Fut>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        {
            let mut inner = lock(&self.inner);
            if inner.waiting.contains(key) {
                return None;
            }
            inner.waiting.insert(key.to_string());
        }
        let job: Arc<Mutex<Option<F>>> = Arc::new(Mutex::new(Some(f)));
        let attempt = {
            let this = self.clone();
            let key = key.to_string();
            move || {
                let this = this.clone();
                let key = key.clone();
                let job = job.clone();
                let k2 = key.clone();
                this.mutex.clone().run(&k2, move || async move {
                    let wait = lock(&this.inner).next_at.get(&key).copied().unwrap_or(0) - system_now_ms();
                    if wait > 0 {
                        return Attempt::Wait(wait);
                    }
                    let inner = this.inner.clone();
                    let k = key.clone();
                    let value = {
                        let sem = this.semaphore.clone();
                        sem.run(move || async move {
                            lock(&inner).waiting.remove(&k);
                            let f = lock(&job).take();
                            match f {
                                Some(f) => Some(f().await),
                                None => None,
                            }
                        })
                        .await
                    };
                    let gap = (this.spacing_for)(&key);
                    if gap > 0 {
                        let now = system_now_ms();
                        let mut inner = lock(&this.inner);
                        inner.next_at.insert(key.clone(), now + gap);
                        if inner.next_at.len() > 500 {
                            inner.next_at.retain(|_, t| *t > now);
                        }
                    }
                    match value {
                        Some(v) => Attempt::Value(v),
                        None => Attempt::Wait(0),
                    }
                })
            }
        };
        let first = attempt();
        Some(async move {
            let mut out = first.await;
            loop {
                match out {
                    Attempt::Value(v) => return v,
                    Attempt::Wait(ms) => {
                        tokio::time::sleep(std::time::Duration::from_millis(ms.max(0) as u64)).await;
                        out = attempt().await;
                    }
                }
            }
        })
    }

    /// Keys with a run waiting.
    pub fn waiting_size(&self) -> usize {
        lock(&self.inner).waiting.len()
    }
}

fn spacing_from(reposted: &Mutex<CappedMap<i64>>, message_id: &str, now: i64) -> i64 {
    let mut map = lock(reposted);
    match map.get(message_id).copied() {
        None => SYNC_SPACING_MS,
        Some(until) if until <= now => {
            map.remove(message_id);
            SYNC_SPACING_MS
        }
        Some(_) => REPOST_SYNC_SPACING_MS,
    }
}

/// `syncSpacingFor(messageId)`: the spacing of the next sync of a source message.
pub fn sync_spacing_for(app: &App, message_id: &str) -> i64 {
    spacing_from(&state(app).reposted, message_id, system_now_ms())
}

/// `noteReposted(messageId, yes)`.
pub fn note_reposted(app: &App, message_id: &str, yes: bool) {
    let st = state(app);
    let mut map = lock(&st.reposted);
    map.remove(message_id);
    if yes {
        map.set(message_id, system_now_ms() + REPOSTED_TTL_MS);
    }
}

// ── RAW SQL ───────────────────────────────────────────────────────────────────

/// `lockGuildSettingsRow(guildId)`.
pub const LOCK_GUILD_SETTINGS_SQL: &str = r#"SELECT 1 AS ok FROM "GuildSettings" WHERE "guildId" = $1 FOR UPDATE"#;
/// `lockBoardRow(boardId, guildId)`.
pub const LOCK_BOARD_SQL: &str = r#"SELECT 1 AS ok FROM "Starboard" WHERE "id" = $1 AND "guildId" = $2 FOR UPDATE"#;
/// `removeIgnoredChannelSql(guildId, channelId)`.
pub const REMOVE_IGNORED_CHANNEL_SQL: &str = concat!(
    r#"UPDATE "Starboard" SET "ignoredChannelIds" = array_remove("ignoredChannelIds", $2), "updatedAt" = "#,
    sql_now!(),
    r#" WHERE "guildId" = $1 AND $2 = ANY("ignoredChannelIds")"#
);

const ENTRIES_FOR_MESSAGE_SQL: &str = r#"SELECT * FROM "StarboardEntry" WHERE "sourceMessageId" = $1 AND "starboardId" = ANY($2)"#;
const DISABLE_BOARDS_SQL: &str =
    concat!(r#"UPDATE "Starboard" SET "enabled" = false, "updatedAt" = "#, sql_now!(), r#" WHERE "id" = ANY($1) AND "guildId" = $2"#);
const DROP_REPOSTS_OF_BOARDS_SQL: &str =
    concat!(r#"UPDATE "StarboardEntry" SET "boardMessageId" = NULL, "updatedAt" = "#, sql_now!(), r#" WHERE "starboardId" = ANY($1)"#);
const BLOCK_ENTRY_SQL: &str = concat!(
    r#"UPDATE "StarboardEntry" SET "boardMessageId" = NULL, "blocked" = true, "updatedAt" = "#,
    sql_now!(),
    r#" WHERE "id" = $1 AND "boardMessageId" = $2"#
);
const STORE_COUNT_SQL: &str = concat!(r#"UPDATE "StarboardEntry" SET "count" = $2, "updatedAt" = "#, sql_now!(), r#" WHERE "id" = $1"#);
const SET_REPOST_SQL: &str = concat!(
    r#"UPDATE "StarboardEntry" SET "boardMessageId" = $2, "count" = $3, "updatedAt" = "#,
    sql_now!(),
    r#" WHERE "id" = $1 AND "boardMessageId" IS NULL AND "blocked" = false"#
);
const DROP_REPOST_SQL: &str = concat!(
    r#"UPDATE "StarboardEntry" SET "boardMessageId" = NULL, "count" = $3, "updatedAt" = "#,
    sql_now!(),
    r#" WHERE "id" = $1 AND "boardMessageId" = $2"#
);
const LIVE_ENTRIES_SQL: &str = r#"SELECT * FROM "StarboardEntry" WHERE "sourceMessageId" = $1 AND "boardMessageId" IS NOT NULL AND "blocked" = false AND "starboardId" = ANY($2)"#;
const SOURCE_ENTRIES_SQL: &str = r#"SELECT e.*, s."channelId" AS "boardChannelId", s."name" AS "boardName" FROM "StarboardEntry" e JOIN "Starboard" s ON s."id" = e."starboardId" WHERE e."sourceMessageId" = $1 AND s."guildId" = $2"#;
const DELETE_ENTRY_SQL: &str = r#"DELETE FROM "StarboardEntry" WHERE "id" = $1"#;
const ENTRY_BY_REPOST_SQL: &str = r#"SELECT e."id", e."sourceMessageId" FROM "StarboardEntry" e JOIN "Starboard" s ON s."id" = e."starboardId" WHERE e."boardMessageId" = $1 AND s."guildId" = $2 LIMIT 1"#;
const BLOCK_REPOSTS_SQL: &str = concat!(
    r#"UPDATE "StarboardEntry" SET "boardMessageId" = NULL, "blocked" = true, "updatedAt" = "#,
    sql_now!(),
    r#" WHERE "boardMessageId" = ANY($1) AND "starboardId" IN (SELECT "id" FROM "Starboard" WHERE "guildId" = $2)"#
);
const SOURCES_OF_SQL: &str = r#"SELECT DISTINCT e."sourceMessageId" FROM "StarboardEntry" e JOIN "Starboard" s ON s."id" = e."starboardId" WHERE e."sourceMessageId" = ANY($1) AND s."guildId" = $2"#;
const DISABLE_CHANNEL_BOARDS_SQL: &str =
    concat!(r#"UPDATE "Starboard" SET "enabled" = false, "updatedAt" = "#, sql_now!(), r#" WHERE "guildId" = $1 AND "channelId" = $2"#);
const DELETE_SOURCE_CHANNEL_ENTRIES_SQL: &str = r#"DELETE FROM "StarboardEntry" WHERE "sourceChannelId" = $1 AND "starboardId" IN (SELECT "id" FROM "Starboard" WHERE "guildId" = $2)"#;
const COUNT_BOARDS_SQL: &str = r#"SELECT COUNT(*)::bigint FROM "Starboard" WHERE "guildId" = $1"#;
const DELETE_BOARD_SQL: &str = r#"DELETE FROM "Starboard" WHERE "id" = $1 AND "guildId" = $2"#;
const REPOST_COUNTS_SQL: &str = r#"SELECT "starboardId", COUNT(*)::bigint FROM "StarboardEntry" WHERE "starboardId" = ANY($1) AND "boardMessageId" IS NOT NULL GROUP BY "starboardId""#;
const SEARCH_BOARDS_SQL: &str = r#"SELECT "name", "channelId", "enabled" FROM "Starboard" WHERE "guildId" = $1 AND "name" ILIKE $2 ORDER BY "name" ASC LIMIT 25"#;

/// Every hand-written statement (name, SQL), like the JS `RAW_SQL` list (01-foundation §1). The first three are the
/// JS list; the others are the Prisma queries that need more than an equality filter.
pub const RAW_SQL: &[(&str, &str)] = &[
    ("starboard.lockGuildSettings", LOCK_GUILD_SETTINGS_SQL),
    ("starboard.lockBoard", LOCK_BOARD_SQL),
    ("starboard.removeIgnoredChannel", REMOVE_IGNORED_CHANNEL_SQL),
    ("starboard.entriesForMessage", ENTRIES_FOR_MESSAGE_SQL),
    ("starboard.disableBoards", DISABLE_BOARDS_SQL),
    ("starboard.dropRepostsOfBoards", DROP_REPOSTS_OF_BOARDS_SQL),
    ("starboard.blockEntry", BLOCK_ENTRY_SQL),
    ("starboard.storeCount", STORE_COUNT_SQL),
    ("starboard.setRepost", SET_REPOST_SQL),
    ("starboard.dropRepost", DROP_REPOST_SQL),
    ("starboard.liveEntries", LIVE_ENTRIES_SQL),
    ("starboard.sourceEntries", SOURCE_ENTRIES_SQL),
    ("starboard.deleteEntry", DELETE_ENTRY_SQL),
    ("starboard.entryByRepost", ENTRY_BY_REPOST_SQL),
    ("starboard.blockReposts", BLOCK_REPOSTS_SQL),
    ("starboard.sourcesOf", SOURCES_OF_SQL),
    ("starboard.disableChannelBoards", DISABLE_CHANNEL_BOARDS_SQL),
    ("starboard.deleteSourceChannelEntries", DELETE_SOURCE_CHANNEL_ENTRIES_SQL),
    ("starboard.countBoards", COUNT_BOARDS_SQL),
    ("starboard.deleteBoard", DELETE_BOARD_SQL),
    ("starboard.repostCounts", REPOST_COUNTS_SQL),
    ("starboard.searchBoards", SEARCH_BOARDS_SQL),
];

fn q(sql: &'static str) -> bot_db::sqlx::query::Query<'static, bot_db::sqlx::Postgres, bot_db::sqlx::postgres::PgArguments> {
    bot_db::sqlx::query(sql)
}

async fn exec(app: &App, query: bot_db::sqlx::query::Query<'static, bot_db::sqlx::Postgres, bot_db::sqlx::postgres::PgArguments>) -> Result<u64, DbError> {
    Ok(query.execute(app.db()).await?.rows_affected())
}

// ── The board cache ───────────────────────────────────────────────────────────

/// A board as the cache keeps it (`toBoardLite`): primitives only, the JS key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardLite {
    pub id: String,
    pub name: String,
    pub channel_id: String,
    pub emojis: Vec<String>,
    pub threshold: i64,
    pub self_star: bool,
    pub ignore_bots: bool,
    pub remove_below_threshold: bool,
    pub auto_react: bool,
    pub max_message_age_days: i64,
    pub allow_nsfw: bool,
    pub ignored_channel_ids: Vec<String>,
    pub remove_self_stars: bool,
    pub enabled: bool,
}

impl From<&Starboard> for BoardLite {
    fn from(b: &Starboard) -> Self {
        Self {
            id: b.id.clone(),
            name: b.name.clone(),
            channel_id: b.channel_id.clone(),
            emojis: b.emojis.clone(),
            threshold: i64::from(b.threshold),
            self_star: b.self_star,
            ignore_bots: b.ignore_bots,
            remove_below_threshold: b.remove_below_threshold,
            auto_react: b.auto_react,
            max_message_age_days: i64::from(b.max_message_age_days),
            allow_nsfw: b.allow_nsfw,
            ignored_channel_ids: b.ignored_channel_ids.clone(),
            remove_self_stars: b.remove_self_stars,
            enabled: b.enabled,
        }
    }
}

fn js_integer(v: &Value) -> Option<i64> {
    let n = v.as_f64()?;
    if n.is_finite() && n.fract() == 0.0 { Some(n as i64) } else { None }
}

fn js_string_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Number(n) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// `toBoardLite(row)`: one cached element, with the JS defaults; `None` for an element that is not a board.
pub fn to_board_lite(row: &Value) -> Option<BoardLite> {
    let o = row.as_object()?;
    let id = o.get("id")?.as_str()?.to_string();
    let channel_id = o.get("channelId")?.as_str()?.to_string();
    let list = |k: &str| o.get(k).and_then(Value::as_array).map(|a| a.iter().map(js_string_of).collect()).unwrap_or_default();
    let is_true = |k: &str| o.get(k) == Some(&Value::Bool(true));
    let not_false = |k: &str| o.get(k) != Some(&Value::Bool(false));
    Some(BoardLite {
        id,
        name: match o.get("name") {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_string_of(v),
        },
        channel_id,
        emojis: list("emojis"),
        threshold: o.get("threshold").and_then(js_integer).unwrap_or(3),
        self_star: is_true("selfStar"),
        ignore_bots: not_false("ignoreBots"),
        remove_below_threshold: not_false("removeBelowThreshold"),
        auto_react: is_true("autoReact"),
        max_message_age_days: o.get("maxMessageAgeDays").and_then(js_integer).unwrap_or(0),
        allow_nsfw: is_true("allowNsfw"),
        ignored_channel_ids: list("ignoredChannelIds"),
        remove_self_stars: is_true("removeSelfStars"),
        enabled: not_false("enabled"),
    })
}

/// `guild:{g}:starboards`.
pub fn boards_key(guild_id: &str) -> String {
    format!("guild:{guild_id}:starboards")
}

fn generation(app: &App, guild_id: &str) -> u64 {
    lock(&state(app).generations).get(guild_id).copied().unwrap_or(0)
}

fn memo_set(app: &App, guild_id: &str, boards: Arc<Vec<BoardLite>>) {
    let now = app.clock().now_ms();
    lock(&state(app).memo).set(guild_id, (now, boards));
}

/// `peekBoards(guildId)`: synchronous, memo only; `None` when the guild is unknown or its entry expired.
pub fn peek_boards(app: &App, guild_id: &str) -> Option<Arc<Vec<BoardLite>>> {
    let st = state(app);
    let mut memo = lock(&st.memo);
    let (at, boards) = memo.get(guild_id)?.clone();
    if app.clock().now_ms() - at >= BOARD_MEMO_TTL_MS {
        memo.remove(guild_id);
        return None;
    }
    Some(boards)
}

async fn load_board_rows(app: &App, guild_id: &str) -> Result<Vec<Starboard>, DbError> {
    SelectBuilder::new(&STARBOARD_INFO).filter("guildId", json!(guild_id))?.order_by("createdAt", true)?.build()?.fetch_all(app.db()).await
}

/// `getBoards(guildId, { fresh })`: the boards of a guild. Never fails. Redis down -> fail open to Postgres; Postgres
/// down -> `None`, and every caller then skips the event (fail closed).
pub async fn get_boards(app: &App, guild_id: &str, fresh: bool) -> Option<Arc<Vec<BoardLite>>> {
    // A list read while invalidate_boards ran is returned to this caller but never memoised.
    let gen_at_start = generation(app, guild_id);
    if !fresh {
        if let Some(m) = peek_boards(app, guild_id) {
            return Some(m);
        }
        if let Some(Value::Array(items)) = app.cache().get(&boards_key(guild_id)).await {
            let boards = Arc::new(items.iter().filter_map(to_board_lite).collect::<Vec<_>>());
            if generation(app, guild_id) == gen_at_start {
                memo_set(app, guild_id, boards.clone());
            }
            return Some(boards);
        }
    }
    match load_board_rows(app, guild_id).await {
        Ok(rows) => {
            let boards = Arc::new(rows.iter().map(BoardLite::from).collect::<Vec<_>>());
            if generation(app, guild_id) == gen_at_start {
                app.cache().set(&boards_key(guild_id), boards.as_ref(), BOARD_CACHE_TTL_S).await;
                memo_set(app, guild_id, boards.clone());
            }
            Some(boards)
        }
        Err(e) => {
            if state(app).allow_board_warn.allow(guild_id) {
                bot_util::log_warn!(format!("Starboard: could not load the boards of guild {guild_id}: {e}"));
            }
            None
        }
    }
}

/// `invalidateBoards(guildId)`: drops the memo entry and the Redis list. Call after every create, edit, delete and disable.
pub async fn invalidate_boards(app: &App, guild_id: &str) {
    {
        let st = state(app);
        lock(&st.memo).remove(guild_id);
        let mut gens = lock(&st.generations);
        let next = gens.get(guild_id).copied().unwrap_or(0) + 1;
        gens.set(guild_id, next);
    }
    app.cache().del_one(&boards_key(guild_id)).await;
}

/// `guildMayHaveBoards(ctx)`: hook filter; false only when the memo says this guild has no boards at all.
pub fn guild_may_have_boards(app: &App, guild_id: &str) -> bool {
    !guild_id.is_empty() && peek_boards(app, guild_id).is_none_or(|b| !b.is_empty())
}

/// `__setBoardsMemo(guildId, boards)`: test seam; `None` clears the memo entry.
pub fn set_boards_memo(app: &App, guild_id: &str, boards: Option<Vec<BoardLite>>) {
    match boards {
        None => {
            lock(&state(app).memo).remove(guild_id);
        }
        Some(b) => memo_set(app, guild_id, Arc::new(b)),
    }
}

// ── Counting qualifying reactions ─────────────────────────────────────────────

/// `fetchReactionUsers(reaction)`: every distinct user of one reaction (normal plus super reactions), at most
/// [`MAX_USER_PAGES`] pages. `extra` is the unverified remainder of a truncated list.
pub async fn fetch_reaction_users(app: &App, channel_id: &str, message_id: &str, reaction: &MessageReaction) -> Result<UserList, DiscordError> {
    let io = starboard_io(app);
    let mut types = vec![false];
    if reaction.count_details.burst > 0 {
        types.push(true);
    }
    let mut users: indexmap::IndexMap<String, ReactUser> = indexmap::IndexMap::new();
    let mut pages = 0usize;
    let mut truncated = false;
    for burst in types {
        let mut after: Option<String> = None;
        loop {
            if pages >= MAX_USER_PAGES {
                truncated = true;
                break;
            }
            let batch = io.reaction_users(app, channel_id, message_id, &reaction.reaction_type, burst, after.as_deref()).await?;
            pages += 1;
            let size = batch.len();
            let max = batch.iter().filter_map(|u| u.id.parse::<u64>().ok()).max();
            for u in batch {
                users.entry(u.id.clone()).or_insert(u);
            }
            if size < 100 {
                break;
            }
            after = max.map(|m| m.to_string());
        }
        if truncated {
            break;
        }
    }
    let extra = if truncated { (reaction.count as i64 - users.len() as i64).max(0) } else { 0 };
    Ok(UserList { users: users.into_values().collect(), extra })
}

// ── Sync engine ───────────────────────────────────────────────────────────────

fn source_ok(app: &App, guild: &SbGuild, board: &BoardLite, channel: &SbChannel, board_channel_ids: &HashSet<String>, board_channel: Option<&SbChannel>) -> bool {
    let io = starboard_io(app);
    source_skip_reason(board, channel, board_channel_ids, board_channel, || io.leaks_private(app, &guild.id, &channel.id, &board.channel_id)).is_none()
}

/// Boards that are on and may be fed by `channel`. Board channels are taken from ALL boards (disabled ones included).
fn eligible_boards(app: &App, boards: &[BoardLite], channel: &SbChannel, guild: &SbGuild) -> Vec<BoardLite> {
    let ids: HashSet<String> = boards.iter().map(|b| b.channel_id.clone()).collect();
    boards.iter().filter(|b| b.enabled && source_ok(app, guild, b, channel, &ids, guild.channel(&b.channel_id))).cloned().collect()
}

async fn disable_boards(app: &App, guild_id: &str, board_ids: &[String], channel_id: &str) -> Result<(), DbError> {
    let (gid, ids) = (guild_id.to_string(), board_ids.to_vec());
    bot_db::transaction(app.db(), TxOptions::default(), move |tx| {
        Box::pin(async move {
            q(DISABLE_BOARDS_SQL).bind(ids.clone()).bind(gid.clone()).execute(&mut *tx).await?;
            q(DROP_REPOSTS_OF_BOARDS_SQL).bind(ids.clone()).execute(&mut *tx).await?;
            Ok::<_, DbError>(())
        })
    })
    .await?;
    invalidate_boards(app, guild_id).await;
    bot_util::log_info!(format!("Starboard(s) {} disabled: channel {channel_id} is gone ({guild_id})", board_ids.join(", ")));
    Ok(())
}

/// The board's channel from the cache, else a REST fetch. 10003 from REST disables the board (never on a cache miss).
async fn resolve_board_channel(app: &App, guild: &SbGuild, board: &BoardLite) -> Option<SbChannel> {
    if let Some(c) = guild.channel(&board.channel_id) {
        return Some(c.clone());
    }
    match starboard_io(app).fetch_channel(app, &board.channel_id).await {
        Ok(Some(c)) if c.guild_id.as_deref() == Some(guild.id.as_str()) => Some(c),
        Ok(_) => None,
        Err(e) => {
            if e.code == Some(10003)
                && guild.available
                && let Err(db) = disable_boards(app, &guild.id, std::slice::from_ref(&board.id), &board.channel_id).await
            {
                bot_util::log_error!("Starboard: could not disable a board whose channel is gone", db.to_string());
            }
            None
        }
    }
}

async fn block_entry(app: &App, entry_id: &str, board_message_id: &str) -> Result<u64, DbError> {
    exec(app, q(BLOCK_ENTRY_SQL).bind(entry_id.to_string()).bind(board_message_id.to_string())).await
}

async fn store_count(app: &App, entry_id: &str, count: i64) -> Result<u64, DbError> {
    exec(app, q(STORE_COUNT_SQL).bind(entry_id.to_string()).bind(clamp_i32(count))).await
}

fn clamp_i32(n: i64) -> i32 {
    n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// Edits a live repost. 10008 (deleted by hand) blocks the entry, 50001/50013 warn. `false` when the repost is gone.
async fn edit_repost(app: &App, guild: &SbGuild, board: &BoardLite, entry: &StarboardEntry, bc: &SbChannel, payload: &Value) -> anyhow::Result<bool> {
    let Some(repost) = entry.board_message_id.clone() else { return Ok(false) };
    match starboard_io(app).edit(app, &bc.id, &repost, payload).await {
        Ok(()) => Ok(true),
        Err(e) if e.code == Some(10008) => {
            if guild.available {
                block_entry(app, &entry.id, &repost).await?;
            }
            Ok(false)
        }
        Err(e) if e.is_no_access() => {
            warn_once(
                app,
                board,
                "perm",
                format!(
                    "Starboard \"{}\": cannot edit reposts in <#{}> ({}); give me View Channel, Send Messages and Embed Links there",
                    board.name, board.channel_id, guild.id
                ),
            );
            Ok(true)
        }
        Err(e) => Err(e.into()),
    }
}

const REPOST_NEEDS: [&str; 3] = ["ViewChannel", "SendMessages", "EmbedLinks"];

/// `checkChannelUsable(bc, { needs })`: the cache, or for a channel Discord returned while the cache missed it, the
/// permissions computed when it was fetched.
fn board_channel_usable(app: &App, guild: &SbGuild, bc: &SbChannel, needs: &[&str]) -> bot_core::interaction_helpers::ChannelUsable {
    use bot_core::interaction_helpers::{ChannelUsable, missing_from};
    let Some(bits) = bc.bot_permissions.filter(|_| guild.channel(&bc.id).is_none()) else {
        return starboard_io(app).channel_usable(app, &guild.id, &bc.id, needs);
    };
    // isTextBased(): text, announcement, voice, stage and threads.
    if !matches!(bc.kind, 0 | 2 | 5 | 10 | 11 | 12 | 13) {
        return ChannelUsable { ok: false, missing: Vec::new(), reason: Some("That is not a text channel".into()) };
    }
    let need: Vec<&str> = if bc.is_thread { needs.iter().map(|k| if *k == "SendMessages" { "SendMessagesInThreads" } else { k }).collect() } else { needs.to_vec() };
    let missing = missing_from(Some(serenity::all::Permissions::from_bits_truncate(bits)), &need);
    if missing.is_empty() {
        return ChannelUsable { ok: true, missing, reason: None };
    }
    let reason = format!("I need **{}** in <#{}>.", bot_core::permissions::permission_names(&missing).join(", "), bc.id);
    ChannelUsable { ok: false, missing, reason: Some(reason) }
}

fn react_identifier(app: &App, board: &BoardLite, qualifying: &[MessageReaction]) -> Option<ReactionType> {
    let Some(first) = board.emojis.first() else {
        let mut best: Option<&MessageReaction> = None;
        for r in qualifying {
            if best.is_none_or(|b| r.count > b.count) {
                best = Some(r);
            }
        }
        return best.map(|b| b.reaction_type.clone());
    };
    if !is_snowflake(first) {
        return Some(ReactionType::Unicode(first.clone()));
    }
    if let Some(e) = starboard_io(app).custom_emoji(app, first)
        && let Some(id) = crate::discord_io::parse_id(&e.id)
    {
        return Some(ReactionType::Custom { animated: e.animated, id: serenity::all::EmojiId::new(id), name: Some(e.name) });
    }
    qualifying.iter().find(|r| matches!(&r.reaction_type, ReactionType::Custom { id, .. } if id.to_string() == *first)).map(|r| r.reaction_type.clone())
}

/// One decided action and what it needs.
pub struct ApplyArgs<'a> {
    pub action: SyncAction,
    pub board: &'a BoardLite,
    pub entry: Option<&'a StarboardEntry>,
    pub message: &'a Message,
    pub count: i64,
    pub emoji_text: &'a str,
    pub guild: &'a SbGuild,
    pub qualifying: &'a [MessageReaction],
}

/// `applyAction(...)`: applies one decided action. Discord and database errors are handled as the JS does; anything
/// unexpected is returned.
pub async fn apply_action(app: &App, a: ApplyArgs<'_>) -> anyhow::Result<()> {
    let ApplyArgs { action, board, entry, message, count, emoji_text, guild, qualifying } = a;
    if action == SyncAction::None {
        return Ok(());
    }
    if action == SyncAction::Store {
        if let Some(e) = entry {
            store_count(app, &e.id, count).await?;
        }
        return Ok(());
    }
    let Some(bc) = resolve_board_channel(app, guild, board).await else { return Ok(()) };
    let io = starboard_io(app);

    if matches!(action, SyncAction::Create | SyncAction::Repost) {
        let usable = board_channel_usable(app, guild, &bc, &REPOST_NEEDS);
        if !usable.ok {
            // The entry is not created, so the next event retries once the permissions are fixed.
            warn_once(
                app,
                board,
                "perm",
                format!(
                    "Starboard \"{}\": cannot post in <#{}> ({}): {}",
                    board.name,
                    board.channel_id,
                    guild.id,
                    usable.reason.unwrap_or_else(|| "missing permissions".into())
                ),
            );
            return Ok(());
        }
        let member = io.cached_member(app, &guild.id, &message.author.id.to_string());
        let payload = build_repost_payload_with(&guild.id, message, member.as_ref(), count, emoji_text);
        let sent = match io.send(app, &bc.id, &payload).await {
            Ok(id) => id,
            Err(e) if e.is_no_access() => {
                warn_once(app, board, "perm", format!("Starboard \"{}\": cannot post in <#{}> ({})", board.name, board.channel_id, guild.id));
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        let saved = if action == SyncAction::Create {
            let insert = InsertBuilder::new(&STARBOARD_ENTRY_INFO)
                .values([
                    ("starboardId", json!(board.id)),
                    ("sourceChannelId", json!(message.channel_id.to_string())),
                    ("sourceMessageId", json!(message.id.to_string())),
                    ("authorId", json!(message.author.id.to_string())),
                    ("boardMessageId", json!(sent)),
                    ("count", json!(count)),
                ])
                .and_then(InsertBuilder::build);
            match insert {
                Ok(q) => match q.execute(app.db()).await {
                    Ok(_) => true,
                    Err(e) if e.is_unique_violation() => {
                        bot_util::log_debug!(format!("Starboard: entry for {} already exists, removing the extra copy", message.id));
                        false
                    }
                    Err(e) => {
                        bot_util::log_error!("Starboard: could not save an entry, removing the repost", e.to_string());
                        false
                    }
                },
                Err(e) => {
                    bot_util::log_error!("Starboard: could not save an entry, removing the repost", e.to_string());
                    false
                }
            }
        } else {
            let id = entry.map(|e| e.id.clone()).unwrap_or_default();
            match exec(app, q(SET_REPOST_SQL).bind(id).bind(sent.clone()).bind(clamp_i32(count))).await {
                Ok(n) => {
                    if n == 0 {
                        bot_util::log_debug!(format!("Starboard: repost of {} conflicted, removing the extra copy", message.id));
                    }
                    n > 0
                }
                Err(e) => {
                    if e.is_unique_violation() {
                        bot_util::log_debug!(format!("Starboard: entry for {} already exists, removing the extra copy", message.id));
                    } else {
                        bot_util::log_error!("Starboard: could not save an entry, removing the repost", e.to_string());
                    }
                    false
                }
            }
        };
        if !saved {
            mark_bot_deletion(app, &[sent.as_str()], Some("starboard"), None);
            let _ = io.delete(app, &bc.id, &sent).await;
            return Ok(());
        }

        if board.auto_react
            && let Some(identifier) = react_identifier(app, board, qualifying)
            && let Err(e) = io.react(app, &bc.id, &sent, &identifier).await
        {
            let why = e.code.map(|c| c.to_string()).unwrap_or(e.message.clone());
            warn_once(app, board, "react", format!("Starboard \"{}\": could not add the auto-reaction ({why})", board.name));
        }
        return Ok(());
    }

    let Some(entry) = entry else { return Ok(()) };
    if action == SyncAction::Update {
        // Only the content line changes; the embed and the button stay as posted.
        let payload = json!({
            "content": build_count_line(emoji_text, count, &message.channel_id.to_string()),
            "allowed_mentions": { "parse": [] },
        });
        if !edit_repost(app, guild, board, entry, &bc, &payload).await? {
            return Ok(());
        }
        store_count(app, &entry.id, count).await?;
        return Ok(());
    }

    // Remove.
    let Some(repost) = entry.board_message_id.clone() else { return Ok(()) };
    mark_bot_deletion(app, &[repost.as_str()], Some("starboard"), None);
    if let Err(e) = io.delete(app, &bc.id, &repost).await {
        if e.is_no_access() {
            warn_once(
                app,
                board,
                "perm",
                format!("Starboard \"{}\": cannot delete reposts in <#{}> ({}); give me View Channel there", board.name, board.channel_id, guild.id),
            );
            store_count(app, &entry.id, count).await?;
            return Ok(());
        }
        if e.code != Some(10008) {
            return Err(e.into());
        }
    }
    // Dropped: reposted again when the count reaches the threshold.
    exec(app, q(DROP_REPOST_SQL).bind(entry.id.clone()).bind(repost).bind(clamp_i32(count))).await?;
    Ok(())
}

/// Reads of the message or its reactions failed: 10008/10003 end the run silently, 50001/50013 with a throttled warning.
fn read_failure_is_benign(app: &App, err: &DiscordError, channel: &SbChannel, guild_id: &str) -> bool {
    if err.is_gone() {
        return true;
    }
    if err.is_no_access() {
        if state(app).allow_read_warn.allow(&channel.id) {
            bot_util::log_warn!(format!("Starboard: no access to read reactions in #{} ({guild_id})", channel.name));
        }
        return true;
    }
    false
}

async fn entries_for(app: &App, message_id: &str, board_ids: Vec<String>) -> Result<Vec<StarboardEntry>, DbError> {
    Ok(bot_db::sqlx::query_as::<_, StarboardEntry>(ENTRIES_FOR_MESSAGE_SQL).bind(message_id.to_string()).bind(board_ids).fetch_all(app.db()).await?)
}

/// What a sync needs: the guild, the source channel and the message id (`syncMessage({ guild, channel, messageId })`).
#[derive(Debug, Clone)]
pub struct SyncRequest {
    pub guild_id: String,
    pub channel: SbChannel,
    pub message_id: String,
    /// A message already in hand (`ensureMessage`); else it is fetched.
    pub message: Option<Message>,
}

/// `syncMessage(...)`: recounts one message for every eligible board and applies the result. Every run re-evaluates ALL
/// eligible boards, so a coalesced event never loses a board.
pub async fn sync_message(app: &App, req: SyncRequest) -> anyhow::Result<()> {
    let io = starboard_io(app);
    let Some(boards) = get_boards(app, &req.guild_id, false).await else { return Ok(()) };
    let Some(guild) = io.guild(app, &req.guild_id) else { return Ok(()) };
    let mut candidates = eligible_boards(app, &boards, &req.channel, &guild);
    if candidates.is_empty() {
        return Ok(());
    }
    let entries = entries_for(app, &req.message_id, candidates.iter().map(|b| b.id.clone()).collect()).await?;
    let by_board: HashMap<String, StarboardEntry> = entries.into_iter().map(|e| (e.starboard_id.clone(), e)).collect();
    note_reposted(app, &req.message_id, by_board.values().any(|e| e.board_message_id.is_some() && !e.blocked));

    // Age pre-check, before any fetch: a message with no entry that is already too old can never be reposted.
    let now = app.clock().now_ms();
    let born = snowflake_time(&req.message_id);
    candidates.retain(|b| by_board.contains_key(&b.id) || !(b.max_message_age_days > 0 && now - born > b.max_message_age_days * DAY_MS));
    if candidates.is_empty() {
        return Ok(());
    }

    let message = match req.message.clone() {
        Some(m) => m,
        None => match io.fetch_message(app, &req.channel.id, &req.message_id).await {
            Ok(m) => m,
            Err(e) => {
                if read_failure_is_benign(app, &e, &req.channel, &req.guild_id) {
                    return Ok(());
                }
                return Err(e.into());
            }
        },
    };

    let bot_user_id = io.bot_user_id(app).unwrap_or_default();
    let mut user_memo: HashMap<String, UserList> = HashMap::new();
    let channel_id = req.channel.id.clone();
    let custom = |id: &str| io.custom_emoji(app, id);

    let mut first_error: Option<anyhow::Error> = None;
    for board in &candidates {
        let result: anyhow::Result<bool> = async {
            let entry = by_board.get(&board.id);
            // A blocked entry never changes again, so it needs no recount.
            if entry.is_some_and(|e| e.blocked) {
                return Ok(true);
            }
            if message_skip_reason(board, &message, entry.is_some(), now, &bot_user_id).is_some() {
                return Ok(true);
            }
            let qualifying: Vec<MessageReaction> =
                message.reactions.iter().filter(|r| board_matches_emoji(board, Some(&emoji_key_of(&r.reaction_type)))).cloned().collect();
            // Exclusions only lower the count, so a board whose raw total is below the threshold cannot create a repost.
            let raw_total: i64 = qualifying.iter().map(|r| r.count as i64).sum();
            if entry.is_none() && raw_total < board.threshold {
                return Ok(true);
            }
            let count = if board.self_star && !board.ignore_bots && qualifying.len() == 1 {
                (qualifying[0].count as i64 - i64::from(qualifying[0].me)).max(0)
            } else {
                let mut lists = Vec::new();
                for r in &qualifying {
                    let key = normalize_emoji_key(&emoji_key_of(&r.reaction_type));
                    if let Some(l) = user_memo.get(&key) {
                        lists.push(l.clone());
                        continue;
                    }
                    match fetch_reaction_users(app, &channel_id, &message.id.to_string(), r).await {
                        Ok(l) => {
                            user_memo.insert(key, l.clone());
                            lists.push(l);
                        }
                        Err(e) => {
                            if read_failure_is_benign(app, &e, &req.channel, &req.guild_id) {
                                return Ok(false);
                            }
                            return Err(e.into());
                        }
                    }
                }
                compute_count(board, &lists, Some(&message.author.id.to_string()), Some(&bot_user_id))
            };
            let entry_state = entry.map(EntryState::from);
            let action = decide_action(entry_state.as_ref(), count, board);
            let emoji_text = pick_display_emoji(board, &qualifying, &custom, Some(&message.reactions));
            apply_action(app, ApplyArgs { action, board, entry, message: &message, count, emoji_text: &emoji_text, guild: &guild, qualifying: &qualifying })
                .await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ── Hook: reactions ───────────────────────────────────────────────────────────

/// Which reaction event fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionEventKind {
    Add,
    Remove,
    RemoveAll,
    RemoveEmoji,
}

/// What the reaction hooks pass on (`ctx` of the four reaction events).
#[derive(Debug, Clone)]
pub struct ReactionInput {
    pub kind: ReactionEventKind,
    pub guild_id: String,
    pub channel_id: String,
    pub message_id: String,
    /// `ctx.emojiKey` (none for RemoveAll).
    pub emoji_key: Option<String>,
    /// The reaction's emoji (`ctx.reaction.emoji`).
    pub emoji: Option<ReactionType>,
    /// The reacting user (Add and Remove).
    pub user_id: Option<String>,
    /// The message's author when known without a fetch (`ctx.message.author` of a non-partial message).
    pub message_author_id: Option<String>,
}

/// `reactionFilter(ctx)`: false when no enabled board can care about this emoji. RemoveAll has no emoji key.
pub fn reaction_filter(app: &App, guild_id: &str, emoji_key: Option<&str>) -> bool {
    if guild_id.is_empty() {
        return false;
    }
    match peek_boards(app, guild_id) {
        None => true,
        Some(boards) => boards.iter().any(|b| b.enabled && (emoji_key.is_none() || board_matches_emoji(b, emoji_key))),
    }
}

/// MEE6 "remove self-stars": takes the author's own qualifying reaction away. Needs Manage Messages; never counts.
async fn remove_self_star(app: &App, input: &ReactionInput, channel: &SbChannel, boards: &[BoardLite]) -> anyhow::Result<()> {
    let io = starboard_io(app);
    let (Some(user_id), Some(emoji)) = (input.user_id.as_deref(), input.emoji.as_ref()) else { return Ok(()) };
    let author = match &input.message_author_id {
        Some(a) => Some(a.clone()),
        None => io.fetch_message(app, &input.channel_id, &input.message_id).await.ok().map(|m| m.author.id.to_string()),
    };
    if author.as_deref() != Some(user_id) {
        return Ok(());
    }
    let guard: Vec<&BoardLite> = boards.iter().filter(|b| b.remove_self_stars && board_matches_emoji(b, input.emoji_key.as_deref())).collect();
    let Some(first) = guard.first() else { return Ok(()) };
    if !io.channel_usable(app, &input.guild_id, &channel.id, &["ViewChannel", "ManageMessages"]).ok {
        warn_once(
            app,
            first,
            "selfstar",
            format!("Starboard \"{}\": I need Manage Messages in #{} ({}) to remove self-stars", first.name, channel.name, input.guild_id),
        );
        return Ok(());
    }
    if let Err(e) = io.remove_reaction(app, &input.channel_id, &input.message_id, emoji, user_id).await {
        if e.is_no_access() {
            warn_once(
                app,
                first,
                "selfstar",
                format!("Starboard \"{}\": could not remove a self-star in #{} ({}): missing Manage Messages", first.name, channel.name, input.guild_id),
            );
        } else if e.code != Some(10008) {
            return Err(e.into());
        }
    }
    Ok(())
}

/// `handleReaction(ctx)`: the reaction hook body. Resolves once the sync is queued (never waits for Discord). Returns
/// what happened: `skipped`, `self` (the author's own reaction cannot change a count), `coalesced` or `queued`.
pub async fn handle_reaction(app: &App, input: ReactionInput) -> &'static str {
    let io = starboard_io(app);
    let Some(boards) = get_boards(app, &input.guild_id, false).await else { return "skipped" };
    let active: Vec<BoardLite> =
        boards.iter().filter(|b| b.enabled && (input.emoji_key.is_none() || board_matches_emoji(b, input.emoji_key.as_deref()))).cloned().collect();
    if active.is_empty() {
        return "skipped";
    }
    let guild = io.guild(app, &input.guild_id).unwrap_or_else(|| SbGuild { id: input.guild_id.clone(), ..Default::default() });
    let channel = match guild.channel(&input.channel_id) {
        Some(c) => Some(c.clone()),
        None => io.fetch_channel(app, &input.channel_id).await.ok().flatten(),
    };
    let Some(channel) = channel else { return "skipped" };

    let ids: HashSet<String> = boards.iter().map(|b| b.channel_id.clone()).collect();
    let candidates: Vec<BoardLite> = active.into_iter().filter(|b| source_ok(app, &guild, b, &channel, &ids, guild.channel(&b.channel_id))).collect();
    if candidates.is_empty() {
        return "skipped";
    }

    if input.kind == ReactionEventKind::Add && input.user_id.is_some() && input.emoji.is_some() && candidates.iter().any(|b| b.remove_self_stars) {
        let (a, i, c, b) = (app.clone(), input.clone(), channel.clone(), candidates.clone());
        tokio::spawn(async move {
            if let Err(e) = remove_self_star(&a, &i, &c, &b).await {
                log_sync_error(&e);
            }
        });
    }

    // The author's own reaction cannot change the count when no candidate counts self-stars.
    if matches!(input.kind, ReactionEventKind::Add | ReactionEventKind::Remove)
        && input.user_id.is_some()
        && input.message_author_id.is_some()
        && input.message_author_id == input.user_id
        && candidates.iter().all(|b| !b.self_star)
    {
        return "self";
    }

    // The steps above are early exits only: the sync re-reads the configuration and re-evaluates every eligible board.
    let st = state(app);
    let custom = st.sync_impl.read().unwrap_or_else(|e| e.into_inner()).clone();
    let req = SyncRequest { guild_id: input.guild_id.clone(), channel, message_id: input.message_id.clone(), message: None };
    let a = app.clone();
    let run = move || async move {
        match custom {
            Some(f) => f(a, req).await,
            None => sync_message(&a, req).await,
        }
    };
    match st.coalescer.request(&input.message_id, run) {
        None => "coalesced",
        Some(fut) => {
            tokio::spawn(async move {
                if let Err(e) = fut.await {
                    log_sync_error(&e);
                }
            });
            "queued"
        }
    }
}

/// The hook `run` of the four reaction events: returns at once, the work continues in the background.
pub fn on_reaction_event(app: &App, input: ReactionInput) {
    let a = app.clone();
    tokio::spawn(async move {
        handle_reaction(&a, input).await;
    });
}

// ── Hook: messageUpdate ───────────────────────────────────────────────────────

async fn mirror_edit(app: &App, guild_id: &str, message: &Message) -> anyhow::Result<()> {
    let io = starboard_io(app);
    let Some(boards) = get_boards(app, guild_id, false).await else { return Ok(()) };
    let enabled: Vec<&BoardLite> = boards.iter().filter(|b| b.enabled).collect();
    if enabled.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = enabled.iter().map(|b| b.id.clone()).collect();
    let entries =
        bot_db::sqlx::query_as::<_, StarboardEntry>(LIVE_ENTRIES_SQL).bind(message.id.to_string()).bind(ids).fetch_all(app.db()).await.map_err(DbError::from)?;
    if entries.is_empty() {
        return Ok(());
    }
    let Some(guild) = io.guild(app, guild_id) else { return Ok(()) };
    // The source may have become ineligible since it was reposted: like a sync, an ineligible source is left alone.
    let Some(channel) = guild.channel(&message.channel_id.to_string()).cloned() else { return Ok(()) };
    let board_ids: HashSet<String> = boards.iter().map(|b| b.channel_id.clone()).collect();
    let custom = |id: &str| io.custom_emoji(app, id);
    for entry in &entries {
        let r: anyhow::Result<()> = async {
            let Some(board) = enabled.iter().find(|b| b.id == entry.starboard_id) else { return Ok(()) };
            let Some(bc) = resolve_board_channel(app, &guild, board).await else { return Ok(()) };
            if !source_ok(app, &guild, board, &channel, &board_ids, Some(&bc)) {
                return Ok(());
            }
            let qualifying: Vec<MessageReaction> =
                message.reactions.iter().filter(|r| board_matches_emoji(board, Some(&emoji_key_of(&r.reaction_type)))).cloned().collect();
            let emoji_text = pick_display_emoji(board, &qualifying, &custom, Some(&message.reactions));
            let member = io.cached_member(app, guild_id, &message.author.id.to_string());
            let payload = build_repost_payload_with(guild_id, message, member.as_ref(), i64::from(entry.count), &emoji_text);
            edit_repost(app, &guild, board, entry, &bc, &payload).await?;
            Ok(())
        }
        .await;
        if let Err(e) = r {
            log_sync_error(&e);
        }
    }
    Ok(())
}

/// The hook `run` of messageUpdate. Shares the per-message mutex and the semaphore with the syncs; never coalesced.
pub fn on_message_update(app: &App, guild_id: &str, message: Message) {
    let st = state(app);
    let (a, g, sem) = (app.clone(), guild_id.to_string(), st.semaphore.clone());
    let key = message.id.to_string();
    let fut = st.mutex.run(&key, move || async move { sem.run(|| async move { mirror_edit(&a, &g, &message).await }).await });
    tokio::spawn(async move {
        if let Err(e) = fut.await {
            log_sync_error(&e);
        }
    });
}

// ── Hooks: deletions ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, sqlx::FromRow)]
struct SourceEntryRow {
    #[sqlx(rename = "id")]
    id: String,
    #[sqlx(rename = "boardMessageId")]
    board_message_id: Option<String>,
    #[sqlx(rename = "boardChannelId")]
    board_channel_id: String,
    #[sqlx(rename = "boardName")]
    board_name: String,
}

/// Deletes the reposts of one deleted source message and forgets its entries. Runs inside the per-message mutex.
async fn delete_source_entries(app: &App, guild_id: &str, message_id: &str) -> anyhow::Result<()> {
    let io = starboard_io(app);
    let entries = bot_db::sqlx::query_as::<_, SourceEntryRow>(SOURCE_ENTRIES_SQL)
        .bind(message_id.to_string())
        .bind(guild_id.to_string())
        .fetch_all(app.db())
        .await
        .map_err(DbError::from)?;
    for entry in entries {
        if let Some(repost) = &entry.board_message_id {
            mark_bot_deletion(app, &[repost.as_str()], Some("starboard"), None);
            if let Err(e) = io.delete(app, &entry.board_channel_id, repost).await {
                if e.is_no_access() {
                    bot_util::log_warn!(format!(
                        "Starboard \"{}\": could not delete the repost of a deleted message; remove it by hand ({guild_id})",
                        entry.board_name
                    ));
                } else if !e.is_gone() {
                    bot_util::log_error!("Starboard: could not delete the repost of a deleted message", e.to_string());
                }
            }
        }
        // The repost is deleted first, so a deleted message's content never lingers because of a crash between the two.
        exec(app, q(DELETE_ENTRY_SQL).bind(entry.id.clone())).await?;
    }
    Ok(())
}

async fn message_delete(app: &App, guild_id: &str, channel_id: &str, id: &str) -> anyhow::Result<()> {
    if was_deleted_by_bot(app, id).as_deref() == Some("starboard") {
        return Ok(());
    }
    let Some(boards) = get_boards(app, guild_id, false).await else { return Ok(()) };
    if boards.is_empty() {
        return Ok(());
    }
    let st = state(app);
    if boards.iter().any(|b| b.channel_id == channel_id) {
        let row: Option<(String, String)> =
            bot_db::sqlx::query_as(ENTRY_BY_REPOST_SQL).bind(id.to_string()).bind(guild_id.to_string()).fetch_optional(app.db()).await.map_err(DbError::from)?;
        let Some((entry_id, source)) = row else { return Ok(()) };
        // The boardMessageId guard makes this a no-op when a concurrent sync already replaced or dropped the repost.
        let (a, i) = (app.clone(), id.to_string());
        st.mutex.run(&source, move || async move { block_entry(&a, &entry_id, &i).await }).await?;
        return Ok(());
    }
    let (a, g, i) = (app.clone(), guild_id.to_string(), id.to_string());
    st.mutex.run(id, move || async move { delete_source_entries(&a, &g, &i).await }).await
}

/// The hook `run` of messageDelete (always): a staff-deleted repost is blocked; a deleted source loses its reposts.
pub fn on_message_delete(app: &App, guild_id: &str, channel_id: &str, message_id: &str) {
    let (a, g, c, m) = (app.clone(), guild_id.to_string(), channel_id.to_string(), message_id.to_string());
    tokio::spawn(async move {
        if let Err(e) = message_delete(&a, &g, &c, &m).await {
            log_sync_error(&e);
        }
    });
}

async fn message_delete_bulk(app: &App, guild_id: &str, channel_id: &str, ids: Vec<String>) -> anyhow::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let Some(boards) = get_boards(app, guild_id, false).await else { return Ok(()) };
    if boards.is_empty() {
        return Ok(());
    }
    if boards.iter().any(|b| b.channel_id == channel_id) {
        let repost_ids: Vec<String> = ids.into_iter().filter(|id| was_deleted_by_bot(app, id).as_deref() != Some("starboard")).collect();
        if !repost_ids.is_empty() {
            exec(app, q(BLOCK_REPOSTS_SQL).bind(repost_ids).bind(guild_id.to_string())).await?;
        }
        return Ok(());
    }
    let sources: Vec<(String,)> =
        bot_db::sqlx::query_as(SOURCES_OF_SQL).bind(ids).bind(guild_id.to_string()).fetch_all(app.db()).await.map_err(DbError::from)?;
    let st = state(app);
    let (a, g) = (app.clone(), guild_id.to_string());
    bot_util::async_util::map_limit(sources, 5, move |(source,): (String,), _| {
        let (a, g, st) = (a.clone(), g.clone(), st.clone());
        async move {
            let s2 = source.clone();
            if let Err(e) = st.mutex.run(&source, move || async move { delete_source_entries(&a, &g, &s2).await }).await {
                log_sync_error(&e);
            }
            Ok::<(), ()>(())
        }
    })
    .await;
    Ok(())
}

/// The hook `run` of messageDeleteBulk (always): purged reposts are blocked, purged sources lose their reposts.
pub fn on_message_delete_bulk(app: &App, guild_id: &str, channel_id: &str, message_ids: Vec<String>) {
    let (a, g, c) = (app.clone(), guild_id.to_string(), channel_id.to_string());
    tokio::spawn(async move {
        if let Err(e) = message_delete_bulk(&a, &g, &c, message_ids).await {
            log_sync_error(&e);
        }
    });
}

/// The hook `run` of channelDelete (always, awaited, database only): a deleted board channel disables its boards.
pub async fn on_channel_delete(app: &App, guild_id: &str, channel_id: &str) {
    let r: anyhow::Result<()> = async {
        let Some(boards) = get_boards(app, guild_id, true).await else { return Ok(()) };
        if boards.is_empty() {
            return Ok(());
        }
        let mut changed = false;
        let hit: Vec<&BoardLite> = boards.iter().filter(|b| b.channel_id == channel_id).collect();
        if !hit.is_empty() {
            let hit_ids: Vec<String> = hit.iter().map(|b| b.id.clone()).collect();
            let (g, c) = (guild_id.to_string(), channel_id.to_string());
            bot_db::transaction(app.db(), TxOptions::default(), move |tx| {
                Box::pin(async move {
                    q(DISABLE_CHANNEL_BOARDS_SQL).bind(g.clone()).bind(c.clone()).execute(&mut *tx).await?;
                    q(DROP_REPOSTS_OF_BOARDS_SQL).bind(hit_ids.clone()).execute(&mut *tx).await?;
                    Ok::<_, DbError>(())
                })
            })
            .await?;
            changed = true;
            bot_util::log_info!(format!(
                "Starboard(s) {} disabled: channel deleted ({guild_id})",
                hit.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        if boards.iter().any(|b| b.ignored_channel_ids.iter().any(|i| i == channel_id)) {
            exec(app, q(REMOVE_IGNORED_CHANNEL_SQL).bind(guild_id.to_string()).bind(channel_id.to_string())).await?;
            changed = true;
        }
        // The reposts stay as an archive, but they can no longer update.
        let gone = exec(app, q(DELETE_SOURCE_CHANNEL_ENTRIES_SQL).bind(channel_id.to_string()).bind(guild_id.to_string())).await?;
        if gone > 0 {
            changed = true;
        }
        if changed {
            invalidate_boards(app, guild_id).await;
        }
        Ok(())
    }
    .await;
    if let Err(e) = r {
        bot_util::log_error!("starboard channelDelete failed", bot_core::error::log_arg(&e));
    }
}

// ── CRUD used by /starboard ───────────────────────────────────────────────────

/// The options of [`create_board`] and [`update_board`] (`data` / `patch`): unset fields are left out, so the schema
/// defaults (create) or the current values (update) apply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoardPatch {
    pub name: Option<String>,
    pub channel_id: Option<String>,
    pub emojis: Option<Vec<String>>,
    pub threshold: Option<i64>,
    pub self_star: Option<bool>,
    pub ignore_bots: Option<bool>,
    pub remove_below_threshold: Option<bool>,
    pub auto_react: Option<bool>,
    pub max_message_age_days: Option<i64>,
    pub allow_nsfw: Option<bool>,
    pub remove_self_stars: Option<bool>,
    pub enabled: Option<bool>,
    /// Update only.
    pub add_ignored: Option<String>,
    /// Update only.
    pub remove_ignored: Option<String>,
}

impl BoardPatch {
    /// The set fields as `(Prisma field, value)`, in the JS `PATCH_FIELDS` order.
    pub fn fields(&self) -> Vec<(&'static str, Value)> {
        let mut out: Vec<(&'static str, Value)> = Vec::new();
        let mut push = |k: &'static str, v: Option<Value>| {
            if let Some(v) = v {
                out.push((k, v));
            }
        };
        push("name", self.name.clone().map(Value::from));
        push("channelId", self.channel_id.clone().map(Value::from));
        push("emojis", self.emojis.clone().map(|e| json!(e)));
        push("threshold", self.threshold.map(Value::from));
        push("selfStar", self.self_star.map(Value::from));
        push("ignoreBots", self.ignore_bots.map(Value::from));
        push("removeBelowThreshold", self.remove_below_threshold.map(Value::from));
        push("autoReact", self.auto_react.map(Value::from));
        push("maxMessageAgeDays", self.max_message_age_days.map(Value::from));
        push("allowNsfw", self.allow_nsfw.map(Value::from));
        push("removeSelfStars", self.remove_self_stars.map(Value::from));
        push("enabled", self.enabled.map(Value::from));
        out
    }
}

/// `createBoard(guildId, data)`: creates a board under the per-guild cap (the GuildSettings row lock serialises
/// concurrent creates). A duplicate name is the database's unique violation, unchanged.
pub async fn create_board(app: &App, guild_id: &str, data: BoardPatch) -> Result<Starboard, BoardError> {
    app.settings().ensure_guild_settings(guild_id).await.map_err(|e| BoardError::Other(anyhow::anyhow!("{e}")))?;
    let gid = guild_id.to_string();
    let row = bot_db::transaction(app.db(), TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            q(LOCK_GUILD_SETTINGS_SQL).bind(gid.clone()).execute(&mut *tx).await?;
            let (n,): (i64,) = bot_db::sqlx::query_as(COUNT_BOARDS_SQL).bind(gid.clone()).fetch_one(&mut *tx).await?;
            if n >= bot_config::limits::STARBOARDS as i64 {
                return Err(BoardError::Limit);
            }
            let mut ins = InsertBuilder::new(&STARBOARD_INFO).value("guildId", json!(gid))?;
            for (k, v) in data.fields() {
                ins = ins.value(k, v)?;
            }
            let row: Starboard = ins.build()?.fetch_one(&mut *tx).await?;
            Ok(row)
        })
    })
    .await?;
    invalidate_boards(app, guild_id).await;
    Ok(row)
}

/// `getBoardByName(guildId, name)`.
pub async fn get_board_by_name(app: &App, guild_id: &str, name: Option<&str>) -> Result<Option<Starboard>, DbError> {
    let Some(name) = name else { return Ok(None) };
    SelectBuilder::new(&STARBOARD_INFO).filter("guildId", json!(guild_id))?.filter("name", json!(name))?.limit(1).build()?.fetch_optional(app.db()).await
}

/// `updateBoard(guildId, boardId, patch)`: edits a board in one transaction. A channel change drops every live entry
/// (they are reposted into the new channel). Returns `(before, after)`.
pub async fn update_board(app: &App, guild_id: &str, board_id: &str, patch: BoardPatch) -> Result<(Starboard, Starboard), BoardError> {
    let (gid, bid) = (guild_id.to_string(), board_id.to_string());
    let result = bot_db::transaction(app.db(), TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            q(LOCK_BOARD_SQL).bind(bid.clone()).bind(gid.clone()).execute(&mut *tx).await?;
            let before: Option<Starboard> =
                SelectBuilder::new(&STARBOARD_INFO).filter("id", json!(bid))?.filter("guildId", json!(gid))?.limit(1).build()?.fetch_optional(&mut *tx).await?;
            let Some(before) = before else { return Err(BoardError::NotFound) };
            let mut data = patch.fields();
            if patch.add_ignored.is_some() || patch.remove_ignored.is_some() {
                let mut list = before.ignored_channel_ids.clone();
                if let Some(add) = &patch.add_ignored
                    && !list.contains(add)
                {
                    list.push(add.clone());
                }
                if let Some(rm) = &patch.remove_ignored {
                    list.retain(|id| id != rm);
                }
                if list.len() > MAX_IGNORED_CHANNELS {
                    return Err(BoardError::IgnoreLimit);
                }
                data.push(("ignoredChannelIds", json!(list)));
            }
            let self_star = patch.self_star.unwrap_or(before.self_star);
            let remove_self = patch.remove_self_stars.unwrap_or(before.remove_self_stars);
            if self_star && remove_self {
                return Err(BoardError::SettingsConflict);
            }
            let mut upd = UpdateBuilder::new(&STARBOARD_INFO);
            for (k, v) in data {
                upd = upd.set(k, v)?;
            }
            let after: Starboard = upd.filter("id", json!(bid))?.build()?.fetch_one(&mut *tx).await?;
            if let Some(c) = &patch.channel_id
                && !c.is_empty()
                && *c != before.channel_id
            {
                q(DROP_REPOSTS_OF_BOARDS_SQL).bind(vec![bid.clone()]).execute(&mut *tx).await?;
            }
            Ok((before, after))
        })
    })
    .await?;
    invalidate_boards(app, guild_id).await;
    Ok(result)
}

/// `deleteBoard(guildId, boardId)`: rows deleted (their entries cascade).
pub async fn delete_board(app: &App, guild_id: &str, board_id: &str) -> Result<u64, DbError> {
    let n = exec(app, q(DELETE_BOARD_SQL).bind(board_id.to_string()).bind(guild_id.to_string())).await?;
    invalidate_boards(app, guild_id).await;
    Ok(n)
}

/// A board with `reposts`, the number of its entries that have a live repost.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardWithCount {
    pub board: Starboard,
    pub reposts: i64,
}

/// `listBoardsWithCounts(guildId)`: every board of the guild by creation, with its live repost count.
pub async fn list_boards_with_counts(app: &App, guild_id: &str) -> Result<Vec<BoardWithCount>, DbError> {
    let boards = load_board_rows(app, guild_id).await?;
    if boards.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = boards.iter().map(|b| b.id.clone()).collect();
    let groups: Vec<(String, i64)> = bot_db::sqlx::query_as(REPOST_COUNTS_SQL).bind(ids).fetch_all(app.db()).await?;
    let counts: HashMap<String, i64> = groups.into_iter().collect();
    Ok(boards.into_iter().map(|b| BoardWithCount { reposts: counts.get(&b.id).copied().unwrap_or(0), board: b }).collect())
}

/// One autocomplete row (`{ name, channelId, enabled }`).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BoardSearchRow {
    #[sqlx(rename = "name")]
    pub name: String,
    #[sqlx(rename = "channelId")]
    pub channel_id: String,
    #[sqlx(rename = "enabled")]
    pub enabled: bool,
}

/// Prisma's `contains` pattern: `%text%`. Prisma 5.22 does not escape `%`, `_` or `\` in the text (checked against the
/// JS `searchBoards` on PostgreSQL: `contains: 'a_c'` also finds `abc`), so neither does this: `_` matches any one
/// character, and a trailing `\` makes PostgreSQL reject the pattern (an empty list, as in the JS).
pub fn contains_pattern(text: &str) -> String {
    format!("%{text}%")
}

/// `searchBoards(guildId, focused)`: autocomplete lookup, filtered in the database. Never fails.
pub async fn search_boards(app: &App, guild_id: &str, focused: &str) -> Vec<BoardSearchRow> {
    let needle: String = bot_util::js::utf16_prefix(&bot_util::js::lower(focused), 32).to_string();
    match bot_db::sqlx::query_as::<_, BoardSearchRow>(SEARCH_BOARDS_SQL).bind(guild_id.to_string()).bind(contains_pattern(&needle)).fetch_all(app.db()).await {
        Ok(rows) => rows,
        Err(e) => {
            bot_util::log_debug!(format!("starboard autocomplete failed: {e}"));
            Vec::new()
        }
    }
}

/// `settingsSummary(board, { client, reposts })`: the settings block shared by create, edit and list.
pub fn settings_summary(board: &BoardLite, custom: &dyn Fn(&str) -> Option<SbEmoji>, reposts: Option<i64>) -> String {
    let self_star = if board.self_star {
        "on"
    } else if board.remove_self_stars {
        "removed"
    } else {
        "off"
    };
    let mut last = vec![
        format!("Max age: {}", if board.max_message_age_days > 0 { format!("{} days", board.max_message_age_days) } else { "no limit".into() }),
        format!("NSFW: {}", if board.allow_nsfw { "allowed" } else { "blocked" }),
    ];
    if let Some(r) = reposts {
        last.push(format!("Reposts: {r}"));
    }
    [
        format!("Channel: <#{}> · Emojis: {}", board.channel_id, format_emoji_list(&board.emojis, custom)),
        format!(
            "Threshold: {} · Self-star: {self_star} · Bots: {}",
            board.threshold,
            if board.ignore_bots { "ignored" } else { "counted" }
        ),
        format!(
            "Below threshold: {} · Auto-react: {}",
            if board.remove_below_threshold { "remove" } else { "keep" },
            if board.auto_react { "on" } else { "off" }
        ),
        last.join(" · "),
    ]
    .join("\n")
}
