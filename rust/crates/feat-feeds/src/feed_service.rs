//! Port of `src/services/feedService.js`: keyless social feeds (RSS/Atom, YouTube channel Atom, Reddit `.rss`, the
//! Bluesky public AppView and podcasts). Constants, pure helpers, fetching with the shared Redis cache, add-time
//! resolution, the database helpers and the `feed.poll` job handler. No interaction replies live here.
//!
//! Every outbound request goes through [`crate::net`] (the SSRF-safe `safe_fetch` by default). State lives in
//! PostgreSQL (`Feed` and `ScheduledJob` rows); the first successful fetch of a feed only records the items already
//! there (no backlog), and delivery is at most once (ids are recorded before they are sent).

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use bot_core::interaction_helpers::{PermTarget, bot_permissions, cached_channel, cached_guild, channel_usable_verdict, pingable_role_ids};
use bot_core::prelude::*;
use bot_db::{FEED_INFO, Feed, FeedType, InsertBuilder, SqlValue, UpdateBuilder};
use bot_util::jsre::LazyRe;
use bot_util::lazy_re;
use bot_util::safe_fetch::{FetchOptions, SafeFetchCode};
use bot_util::text::clamp_text;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId, RoleId, UserId};
use serenity::builder::{CreateEmbed, CreateEmbedAuthor};
use sha1::Digest;

use crate::feed_parsers::{
    FeedItem, FeedParseError, ParsedFeed, decode_xml_buffer, discover_feed_links, looks_like_html, one_line, parse_bluesky_author_feed,
    parse_itunes_search, parse_xml_feed,
};
use crate::net::{self, FetchFailure};

// ── Constants ──────────────────────────────────────────────────────────────────

/// `FEED_TYPES`.
pub const FEED_TYPES: [FeedType; 5] = [FeedType::Rss, FeedType::Youtube, FeedType::Reddit, FeedType::Bluesky, FeedType::Podcast];
const MINUTE_MS: i64 = 60_000;
/// `MAX_BACKOFF_MS`.
pub const MAX_BACKOFF_MS: i64 = 12 * 60 * MINUTE_MS;
/// `REDDIT_BLOCK_MS`.
pub const REDDIT_BLOCK_MS: i64 = 60 * MINUTE_MS;
/// `MAX_FAILURES`.
pub const MAX_FAILURES: i32 = 20;
/// `MAX_POSTS_PER_POLL`.
pub const MAX_POSTS_PER_POLL: usize = 5;
/// `SEEN_CAP`.
pub const SEEN_CAP: usize = 100;
/// `WINDOW_SIZE`.
pub const WINDOW_SIZE: usize = 50;
/// `FETCH_TIMEOUT_MS`.
pub const FETCH_TIMEOUT_MS: u64 = 10_000;
/// `SEND_TIMEOUT_MS`.
pub const SEND_TIMEOUT_MS: u64 = 8_000;
/// `SEND_BUDGET_MS`: from handler start; keeps the job under the scheduler's 60 s.
pub const SEND_BUDGET_MS: i64 = 40_000;
/// `MODULE_OFF_RECHECK_MS`.
pub const MODULE_OFF_RECHECK_MS: i64 = 30 * MINUTE_MS;
/// `STALE_SLACK_MS`.
pub const STALE_SLACK_MS: i64 = 24 * 60 * MINUTE_MS;
/// `REDDIT_BLOCK_KEY`.
pub const REDDIT_BLOCK_KEY: &str = "feed:block:reddit";
/// `ITUNES_RL_KEY`.
pub const ITUNES_RL_KEY: &str = "feed:itunes:rl";
/// `REDDIT_RL_KEY`: one 60 s slot key per allowed Reddit fetch (`feed:rl:reddit:<slot>`).
pub const REDDIT_RL_KEY: &str = "feed:rl:reddit";
/// `REDDIT_MAX_PER_MIN`.
pub const REDDIT_MAX_PER_MIN: usize = 8;
const ITUNES_MAX_PER_MIN: i64 = 20;
/// `FEED_CHANNEL_TYPES`: GuildText, GuildAnnouncement.
pub const FEED_CHANNEL_TYPES: [u8; 2] = [0, 5];
const POST_CHANNEL_NEEDS: [&str; 3] = ["ViewChannel", "SendMessages", "EmbedLinks"];

/// `BASE_INTERVAL_MS[type]`: Bluesky 5 min, Reddit 10 min, RSS / YouTube 15 min, podcasts 30 min.
pub fn base_interval_ms(t: FeedType) -> i64 {
    match t {
        FeedType::Rss | FeedType::Youtube => 15 * MINUTE_MS,
        FeedType::Reddit => 10 * MINUTE_MS,
        FeedType::Bluesky => 5 * MINUTE_MS,
        FeedType::Podcast => 30 * MINUTE_MS,
    }
}

/// `MAX_BYTES[type]`.
pub fn max_bytes(t: FeedType) -> u64 {
    if t == FeedType::Podcast { 10 * 1_048_576 } else { 2 * 1_048_576 }
}

/// `feedJobKey(id)`.
pub fn feed_job_key(id: &str) -> String {
    format!("feed.poll:{id}")
}

/// `fetchCacheKey(url)`: `feed:fetch:<sha1 of the URL>`.
pub fn fetch_cache_key(url: &str) -> String {
    format!("feed:fetch:{}", hex::encode(sha1::Sha1::digest(url.as_bytes())))
}

/// `cacheTtlSeconds(type)`.
pub fn cache_ttl_seconds(t: FeedType) -> i64 {
    60.max(base_interval_ms(t) / 1000 - 30)
}

/// `FeedUserError`: an error whose title and message are meant for the admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedUserError {
    pub title: String,
    pub message: String,
}

impl FeedUserError {
    pub fn new(title: &str, message: impl Into<String>) -> Self {
        Self { title: title.into(), message: message.into() }
    }
}

impl std::fmt::Display for FeedUserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FeedUserError {}

/// `CARD_TYPES`: the types that get the bot's own embed card.
pub fn is_card_type(t: FeedType) -> bool {
    matches!(t, FeedType::Rss | FeedType::Reddit | FeedType::Podcast)
}

/// `FEED_VARS.common`.
pub const COMMON_VARS: [&str; 5] = ["title", "url", "link", "author", "feed"];

/// `FEED_VARS[type]`: the type's own variables.
pub fn type_vars(t: FeedType) -> &'static [&'static str] {
    match t {
        FeedType::Youtube => &["youtube_channel"],
        FeedType::Reddit => &["subreddit"],
        FeedType::Bluesky => &["bluesky_name", "bluesky_handle"],
        FeedType::Podcast => &["podcast_title", "episode_title"],
        FeedType::Rss => &[],
    }
}

/// `DEFAULT_TEMPLATES[type]`.
pub fn default_template(t: FeedType) -> &'static str {
    match t {
        FeedType::Rss => "📰 New from **{feed}**",
        FeedType::Youtube => "📺 **{youtube_channel}** just posted a new video!\n{link}",
        FeedType::Reddit => "👽 New post in **r/{subreddit}**",
        FeedType::Bluesky => "🦋 **{bluesky_name}** (@{bluesky_handle}) posted on Bluesky\n{link}",
        FeedType::Podcast => "🎙️ New episode of **{podcast_title}**: {episode_title}",
    }
}

/// `TYPE_LABELS[type]`.
pub fn type_label(t: FeedType) -> &'static str {
    match t {
        FeedType::Rss => "RSS feed",
        FeedType::Youtube => "YouTube channel",
        FeedType::Reddit => "Subreddit",
        FeedType::Bluesky => "Bluesky account",
        FeedType::Podcast => "Podcast",
    }
}

/// `TYPE_EMOJI[type]`.
pub fn type_emoji(t: FeedType) -> &'static str {
    match t {
        FeedType::Rss => "📰",
        FeedType::Youtube => "📺",
        FeedType::Reddit => "👽",
        FeedType::Bluesky => "🦋",
        FeedType::Podcast => "🎙️",
    }
}

/// `TYPE_COLORS[type]`.
pub fn type_color(t: FeedType) -> u32 {
    match t {
        FeedType::Rss => 0xF26522,
        FeedType::Youtube => 0xFF0000,
        FeedType::Reddit => 0xFF4500,
        FeedType::Bluesky => 0x1185FE,
        FeedType::Podcast => 0x8940FA,
    }
}

/// `BSKY_APPVIEW`: `BSKY_APPVIEW_URL` or the public AppView, without trailing slashes.
pub fn bsky_appview() -> String {
    let v = std::env::var("BSKY_APPVIEW_URL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://public.api.bsky.app".into());
    v.trim_end_matches('/').to_string()
}

static DID_RE: LazyRe = lazy_re!(r"^did:(plc|web):[A-Za-z0-9._:%-]{1,200}$", "");

// ── Per-app state ──────────────────────────────────────────────────────────────

/// What the JS keeps at module level: the in-process Reddit budget (used while Redis is down), the per-guild create
/// lock and the `/feed test` throttle.
pub struct FeedsState {
    local_reddit_hits: Mutex<VecDeque<i64>>,
    pub create_mutex: bot_core::sync::KeyedMutex,
    test_throttle: std::sync::OnceLock<bot_util::async_util::Throttle>,
}

impl Default for FeedsState {
    fn default() -> Self {
        Self { local_reddit_hits: Mutex::new(VecDeque::new()), create_mutex: bot_core::sync::KeyedMutex::new(), test_throttle: std::sync::OnceLock::new() }
    }
}

/// The JS `testThrottle = createThrottle(15_000)` of `/feed test`, on the app clock: true when `feed_id` may be tested
/// now (and records it).
pub fn feed_test_allowed(app: &App, feed_id: &str) -> bool {
    let state = app.ext::<FeedsState>();
    state.test_throttle.get_or_init(|| bot_core::clock::clock_throttle(app.clock(), 15_000)).allow(feed_id)
}

/// `__resetLocalState()`: forgets the in-process Reddit budget.
pub fn reset_local_state(app: &App) {
    if let Ok(mut h) = app.ext::<FeedsState>().local_reddit_hits.lock() {
        h.clear();
    }
}

fn local_reddit_allow(app: &App, now: i64) -> bool {
    let state = app.ext::<FeedsState>();
    let Ok(mut hits) = state.local_reddit_hits.lock() else { return true };
    while hits.front().is_some_and(|t| now - *t >= 60_000) {
        hits.pop_front();
    }
    if hits.len() >= REDDIT_MAX_PER_MIN {
        return false;
    }
    hits.push_back(now);
    true
}

fn now_ms(app: &App) -> i64 {
    app.clock().now_ms()
}

fn at_ms(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

fn random_int(max: i64) -> i64 {
    if max <= 0 { 0 } else { (rand::random::<f64>() * max as f64).floor() as i64 }
}

fn iso(ms: i64) -> String {
    bot_db::format_js_date(&at_ms(ms))
}

// ── Pure helpers ───────────────────────────────────────────────────────────────

static DIGITS: LazyRe = lazy_re!(r"^\d+$", "");

/// `parseRetryAfterMs(value, now)`: digits are seconds, otherwise an HTTP date; anything else is `None`.
pub fn parse_retry_after_ms(value: Option<&str>, now: i64) -> Option<f64> {
    let s = bot_util::js::trim(value?);
    if DIGITS.test(s) {
        return Some(s.parse::<f64>().unwrap_or(f64::INFINITY) * 1000.0);
    }
    let at = crate::js_date::date_parse(s);
    if at.is_nan() { None } else { Some((at - now as f64).max(0.0)) }
}

/// The inputs of [`compute_next_poll_at`] (`rand` is `Math.random()`).
#[derive(Debug, Clone, Copy)]
pub struct NextPoll {
    pub feed_type: FeedType,
    pub failure_count: i32,
    pub status: Option<u16>,
    pub retry_after_ms: Option<f64>,
    pub now: i64,
    pub rand: f64,
}

impl NextPoll {
    pub fn new(feed_type: FeedType, now: i64) -> Self {
        Self { feed_type, failure_count: 0, status: None, retry_after_ms: None, now, rand: rand::random::<f64>() }
    }
}

/// `computeNextPollAt(...)`: failures back off ×2 per consecutive failure up to ×32 (at most 12 h); a Reddit 403/429
/// waits at least an hour; Retry-After is honoured up to 6 h; ±10 % jitter; never under a minute.
pub fn compute_next_poll_at(p: NextPoll) -> DateTime<Utc> {
    let base = base_interval_ms(p.feed_type) as f64;
    let mut delay = if p.failure_count > 0 { (base * 2f64.powi(p.failure_count.min(5))).min(MAX_BACKOFF_MS as f64) } else { base };
    if p.feed_type == FeedType::Reddit && matches!(p.status, Some(403) | Some(429)) {
        delay = delay.max(REDDIT_BLOCK_MS as f64);
    }
    if let Some(ra) = p.retry_after_ms.filter(|r| r.is_finite() && *r > 0.0) {
        delay = delay.max(ra.min(6.0 * 3_600_000.0));
    }
    let delay = bot_util::text::js_round(delay * (0.9 + 0.2 * p.rand));
    at_ms(p.now + delay.max(60_000.0) as i64)
}

/// `windowItems(items)`: the newest [`WINDOW_SIZE`] items, unique by id; sorted by date only when every item has one.
pub fn window_items(items: Vec<FeedItem>) -> Vec<FeedItem> {
    let mut seen = HashSet::new();
    let mut unique: Vec<FeedItem> = items.into_iter().filter(|i| seen.insert(i.id.clone())).collect();
    if !unique.is_empty() && unique.iter().all(|i| i.published_at.is_some()) {
        unique.sort_by_key(|i| std::cmp::Reverse(i.published_at));
    }
    unique.truncate(WINDOW_SIZE);
    unique
}

/// The arguments of [`select_new_items`].
#[derive(Debug, Clone)]
pub struct SelectInput<'a> {
    pub items: &'a [FeedItem],
    pub seen_ids: &'a [String],
    pub initialized: bool,
    pub baseline: Option<DateTime<Utc>>,
    pub include_reposts: bool,
    pub max_post: usize,
}

/// What [`select_new_items`] decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// Oldest first.
    pub to_post: Vec<FeedItem>,
    pub next_seen_ids: Vec<String>,
    pub reseed: bool,
    pub skipped_old: usize,
    pub overflow: usize,
}

fn unique_strings<'a>(it: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut seen = HashSet::new();
    it.into_iter().filter(|s| seen.insert((*s).clone())).cloned().collect()
}

/// `selectNewItems(...)`: what to post from the window (newest first). Every window id is recorded; the first fetch
/// records and posts nothing; an undated feed whose ids all changed is a GUID reset; dated items older than the
/// previous success (minus a day) are archive churn; at most `max_post` newest items are posted, oldest first.
pub fn select_new_items(i: SelectInput<'_>) -> Selection {
    let window_ids = unique_strings(i.items.iter().map(|x| &x.id));
    let mut next_seen_ids = unique_strings(window_ids.iter().chain(i.seen_ids.iter()));
    next_seen_ids.truncate(SEEN_CAP);
    let empty = |reseed, skipped_old| Selection { to_post: Vec::new(), next_seen_ids: next_seen_ids.clone(), reseed, skipped_old, overflow: 0 };
    if !i.initialized {
        return empty(true, 0);
    }
    let seen: HashSet<&String> = i.seen_ids.iter().collect();
    let unseen: Vec<&FeedItem> = i.items.iter().filter(|x| !seen.contains(&x.id)).collect();
    if unseen.is_empty() {
        return empty(false, 0);
    }
    let overlap = i.items.iter().any(|x| seen.contains(&x.id));
    let dated = i.items.iter().any(|x| x.published_at.is_some());
    if !overlap && !dated && !i.seen_ids.is_empty() && unseen.len() > i.max_post {
        return empty(true, unseen.len());
    }
    let cutoff = i.baseline.map(|b| b.timestamp_millis() - STALE_SLACK_MS);
    let fresh: Vec<&FeedItem> =
        unseen.iter().copied().filter(|x| !matches!((cutoff, x.published_at), (Some(c), Some(p)) if p.timestamp_millis() < c)).collect();
    let eligible: Vec<&FeedItem> = fresh.iter().copied().filter(|x| i.include_reposts || !x.is_repost).collect();
    let mut to_post: Vec<FeedItem> = eligible.iter().take(i.max_post).map(|x| (*x).clone()).collect();
    to_post.reverse();
    Selection {
        overflow: eligible.len() - to_post.len(),
        skipped_old: unseen.len() - fresh.len(),
        to_post,
        next_seen_ids,
        reseed: false,
    }
}

/// `varHelp(type)`: `` `{title}` `{url}` … `` for the message variables of a type.
pub fn var_help(t: FeedType) -> String {
    COMMON_VARS.iter().chain(type_vars(t).iter()).map(|v| format!("`{{{v}}}`")).collect::<Vec<_>>().join(" ")
}

/// `parseMessageOption(raw)`: `\n` becomes a newline; blank or missing is `Ok(None)` ("no template").
pub fn parse_message_option(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let t = bot_util::js::trim(&raw.replace("\\n", "\n")).to_string();
    if t.is_empty() {
        return Ok(None);
    }
    if bot_util::js::utf16_len(&t) > 1000 {
        return Err("The message can be at most 1000 characters.".into());
    }
    Ok(Some(t))
}

/// The tokens a feed message may use (`allowedTokens(type)`): the built-in placeholders without the member ones,
/// plus the feed variables.
pub fn allowed_tokens(t: FeedType) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    for (key, _) in bot_util::placeholder_parser::placeholders() {
        let k = key.strip_prefix('{').unwrap_or(key);
        let k = bot_util::js::lower(k.strip_suffix('}').unwrap_or(k));
        if k.starts_with("user") || k == "random:x:y" {
            continue;
        }
        out.insert(k);
    }
    out.extend(COMMON_VARS.iter().chain(type_vars(t).iter()).map(|s| s.to_string()));
    out
}

static TOKEN: LazyRe = lazy_re!(r"\{([^{}\n]{1,64})\}", "g");
static RANDOM_TOKEN: LazyRe = lazy_re!(r"^random:-?\d{1,10}:-?\d{1,10}$", "");
static LINK_TOKEN: LazyRe = lazy_re!(r"\{(link|url)\}", "i");

/// `templateWarnings(type, template)`: unknown variables stay literal; YouTube and Bluesky posts need a link for the
/// preview.
pub fn template_warnings(t: FeedType, template: Option<&str>) -> Vec<String> {
    let Some(template) = template else { return Vec::new() };
    let mut out = Vec::new();
    let units = bot_util::jsre::u16s(template);
    let mut tokens: Vec<String> = Vec::new();
    for m in TOKEN.find_all(&units) {
        let tok = bot_util::js::lower(&m.group(&units, 1).unwrap_or_default());
        if !tokens.contains(&tok) {
            tokens.push(tok);
        }
    }
    let allowed = allowed_tokens(t);
    let bad: Vec<&String> = tokens.iter().filter(|x| !allowed.contains(*x) && !RANDOM_TOKEN.test(x)).collect();
    if !bad.is_empty() {
        let list: Vec<String> = bad.iter().map(|x| format!("`{{{x}}}`")).collect();
        let mut msg = format!(
            "⚠️ These variables don't work for {}s and will appear as typed: {}",
            type_label(t),
            bot_util::text::join_with_limit(&list, ", ", 500, bot_util::text::default_more)
        );
        if bad.iter().any(|x| x.starts_with("user")) {
            msg.push_str(" (feeds have no member, so {user…} variables never work)");
        }
        out.push(msg);
    }
    if matches!(t, FeedType::Youtube | FeedType::Bluesky) && !LINK_TOKEN.test(template) {
        out.push("⚠️ This message has no {link}, so Discord won't show a preview of the post.".into());
    }
    out
}

/// `escapeThirdParty(s)`: discord.js `escapeMarkdown` with `maskedLink`, `heading`, `bulletedList` and `numberedList`
/// also on (third-party text must not form masked links, headings or lists).
pub fn escape_third_party(s: &str) -> String {
    bot_core::markdown::escape_markdown_with(
        s,
        bot_core::markdown::EscapeOptions { masked_link: true, heading: true, bulleted_list: true, numbered_list: true, ..Default::default() },
    )
}

static R_PREFIX: LazyRe = lazy_re!(r"^r\/", "i");
static AT_PREFIX: LazyRe = lazy_re!(r"^@", "");

/// `buildFeedVars(feed, item, { cardStyle })`: the message variables. Text is markdown-escaped; URLs are not.
pub fn build_feed_vars(feed: &Feed, item: &FeedItem, card_style: bool) -> Vec<(String, String)> {
    let esc = escape_third_party;
    let link = match &item.url {
        Some(u) if !u.is_empty() => {
            if card_style {
                format!("<{u}>")
            } else {
                u.clone()
            }
        }
        _ => String::new(),
    };
    let mut vars = vec![
        ("title".to_string(), esc(&item.title)),
        ("url".to_string(), link.clone()),
        ("link".to_string(), link),
        ("author".to_string(), esc(&item.author)),
        ("feed".to_string(), esc(&feed.display_name)),
    ];
    let no_at = || AT_PREFIX.replace_first_with(&feed.display_name, |_, _| String::new());
    match feed.r#type {
        FeedType::Youtube => vars.push(("youtube_channel".into(), esc(if item.author.is_empty() { &feed.display_name } else { &item.author }))),
        FeedType::Reddit => vars.push(("subreddit".into(), esc(&R_PREFIX.replace_first_with(&feed.display_name, |_, _| String::new())))),
        FeedType::Bluesky => {
            vars.push(("bluesky_name".into(), esc(&if item.actor_name.is_empty() { no_at() } else { item.actor_name.clone() })));
            vars.push(("bluesky_handle".into(), esc(&if item.actor_handle.is_empty() { no_at() } else { item.actor_handle.clone() })));
        }
        FeedType::Podcast => {
            vars.push(("podcast_title".into(), esc(&feed.display_name)));
            vars.push(("episode_title".into(), esc(&item.title)));
        }
        FeedType::Rss => {}
    }
    vars
}

/// `buildItemEmbed(feed, item, { feedImage })`: the card of RSS, Reddit and podcast items.
pub fn build_item_embed(feed: &Feed, item: &FeedItem, feed_image: Option<&str>, now: DateTime<Utc>) -> CreateEmbed {
    let summary = if item.summary.is_empty() { None } else { Some(escape_third_party(&clamp_text(&item.summary, 350))) };
    let mut e = neutral_embed(Some(&clamp_text(&item.title, 256)), summary.as_deref(), Some(&clamp_text(&feed.display_name, 2048)))
        .colour(type_color(feed.r#type))
        .timestamp(serenity::all::Timestamp::from(item.published_at.unwrap_or(now)));
    if let Some(u) = item.url.as_deref().filter(|u| !u.is_empty()) {
        e = e.url(u);
    }
    if !item.author.is_empty() {
        e = e.author(CreateEmbedAuthor::new(clamp_text(&item.author, 256)));
    }
    let img = item.image.clone().or_else(|| if feed.r#type == FeedType::Podcast { feed_image.map(String::from) } else { None });
    if let Some(img) = img.filter(|i| !i.is_empty()) {
        e = if feed.r#type == FeedType::Rss { e.image(img) } else { e.thumbnail(img) };
    }
    e
}

/// Where a post goes: the cached guild and channel, and the bot's own id and roles (for the ping check).
#[derive(Debug, Clone, Default)]
pub struct PostCtx<'a> {
    pub guild: Option<&'a Guild>,
    pub channel: Option<&'a GuildChannel>,
    pub feed_image: Option<&'a str>,
    /// `false` for the `/feed test` preview: the role is shown but nobody is notified.
    pub ping: bool,
    pub now: DateTime<Utc>,
}

/// A message for one item.
#[derive(Debug, Clone)]
pub struct PostPayload {
    pub content: Option<String>,
    pub embeds: Vec<CreateEmbed>,
    /// `allowedMentions.roles` (`parse` is always empty).
    pub roles: Vec<String>,
}

impl PostPayload {
    /// The create-message JSON.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "embeds": self.embeds.iter().map(|e| serde_json::to_value(e).unwrap_or(Value::Null)).collect::<Vec<_>>(),
            "allowed_mentions": { "parse": [], "roles": self.roles },
        });
        if let Some(c) = &self.content {
            v["content"] = json!(c);
        }
        v
    }
}

/// The bot's id and roles in a cached guild.
pub fn bot_member(app: &App, guild: &Guild) -> Option<(UserId, Vec<RoleId>)> {
    let bot = app.discord()?.cache.current_user().id;
    guild.members.get(&bot).map(|m| (bot, m.roles.clone()))
}

/// `buildPostPayload(feed, item, { guild, channel, feedImage, ping })`: text clamped to 2000; feed text can never
/// ping; the ping role is allowed only when the bot may ping it in that channel.
pub fn build_post_payload(app: &App, feed: &Feed, item: &FeedItem, ctx: &PostCtx<'_>) -> PostPayload {
    let card = is_card_type(feed.r#type);
    let tpl = match feed.message_template.as_deref() {
        Some(t) if !bot_util::js::trim(t).is_empty() => t.to_string(),
        _ => default_template(feed.r#type).to_string(),
    };
    let render = bot_util::placeholder_parser::RenderContext {
        guild: ctx.guild.map(bot_core::adapters::placeholder_guild),
        channel: ctx.channel.map(bot_core::adapters::placeholder_channel),
        vars: Some(build_feed_vars(feed, item, card).into_iter().map(|(k, v)| (k, v.into())).collect()),
        ..Default::default()
    };
    let mut content = bot_util::placeholder_parser::render_placeholders(&tpl, &render, Some(2000));
    let role_id = feed.ping_role_id.clone().filter(|r| !r.is_empty());
    let role_ok = role_id.as_ref().is_some_and(|r| {
        ctx.guild.is_some_and(|g| r.parse::<u64>().ok().filter(|v| *v != 0).is_some_and(|v| g.roles.contains_key(&RoleId::new(v))))
    });
    if role_ok && let Some(r) = &role_id {
        content = clamp_text(bot_util::js::trim_end(&format!("<@&{r}> {content}")), 2000);
    }
    let embeds = if card { vec![build_item_embed(feed, item, ctx.feed_image, ctx.now)] } else { Vec::new() };
    if bot_util::js::trim(&content).is_empty() && embeds.is_empty() {
        content = clamp_text(item.url.as_deref().unwrap_or(&item.title), 2000);
    }
    let roles = match (ctx.ping && role_ok, ctx.guild, ctx.channel, &role_id) {
        (true, Some(g), Some(ch), Some(r)) => {
            let me = bot_member(app, g);
            pingable_role_ids(g, ch, me.as_ref().map(|(id, roles)| (*id, roles.as_slice())), std::slice::from_ref(r)).allowed
        }
        _ => Vec::new(),
    };
    PostPayload { content: if bot_util::js::trim(&content).is_empty() { None } else { Some(content) }, embeds, roles }
}

/// `displayNameUpdate(feed, parsed)`: the new display name after a successful fetch, or `None`.
pub fn display_name_update(feed: &Feed, title: Option<&str>, items: &[FeedItem]) -> Option<String> {
    if feed.r#type == FeedType::Youtube && feed.display_name == feed.source && let Some(t) = title.filter(|t| !t.is_empty()) {
        let n = clamp_text(&one_line(t), 100);
        return if n.is_empty() { None } else { Some(n) };
    }
    if feed.r#type == FeedType::Bluesky
        && let Some(h) = items.first().map(|i| i.actor_handle.as_str()).filter(|h| !h.is_empty())
        && format!("@{h}") != feed.display_name
    {
        return Some(clamp_text(&one_line(&format!("@{h}")), 100));
    }
    None
}

/// `sourceLink(feed)`: the human link (angle brackets suppress Discord's preview).
pub fn source_link(t: FeedType, source: &str, fetch_url: &str) -> String {
    match t {
        FeedType::Youtube => format!("<https://www.youtube.com/channel/{source}>"),
        FeedType::Reddit => format!("<https://www.reddit.com/r/{source}>"),
        FeedType::Bluesky => format!("<https://bsky.app/profile/{source}>"),
        _ => format!("<{fetch_url}>"),
    }
}

/// The normalised error of a fetch (`{ code, status?, message, retryAfterMs?, apiError?, links? }`).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchError {
    pub code: String,
    pub status: Option<u16>,
    pub message: String,
    pub retry_after_ms: Option<f64>,
    pub api_error: Option<String>,
    pub links: Vec<String>,
}

impl FetchError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), ..Default::default() }
    }
}

/// `describeFetchError(type, err)`: one line, at most 200 characters.
pub fn describe_fetch_error(t: FeedType, e: &FetchError) -> String {
    let base = match e.code.as_str() {
        "HTTP" => format!("HTTP {}", e.status.map(|s| s.to_string()).unwrap_or_else(|| "undefined".into())),
        "TIMEOUT" => "Timed out after 10 s".into(),
        "TOO_LARGE" => format!("Feed is larger than {} MB", max_bytes(t) / 1_048_576),
        "DNS_FAILED" => "Domain not found".into(),
        "BLOCKED_ADDRESS" => "That address is private or reserved (blocked)".into(),
        "TOO_MANY_REDIRECTS" => "Too many redirects".into(),
        "INVALID_URL" => "Invalid URL".into(),
        "NETWORK" => format!("Network error ({})", clamp_text(&e.message, 80)),
        "UNEXPECTED_304" => "The server answered \"304 Not Modified\" to a request that was not conditional, so it sent no feed".into(),
        "HTML_PAGE" => "The URL returned a web page, not a feed".into(),
        "NOT_A_FEED" => "Not an RSS or Atom feed".into(),
        "INVALID_XML" => "Feed XML could not be parsed".into(),
        _ => clamp_text(if e.message.is_empty() { "Unknown error" } else { &e.message }, 120),
    };
    let s = e.status;
    let hint = if t == FeedType::Youtube && s == Some(404) {
        "YouTube's public channel feed is returning 404 (a known YouTube outage)".to_string()
    } else if t == FeedType::Reddit && s == Some(403) {
        "Reddit refused the request (private subreddit, or Reddit blocks this server's IP)".into()
    } else if t == FeedType::Reddit && s == Some(429) {
        "Reddit rate limit".into()
    } else if t == FeedType::Reddit && e.code == "HTML_PAGE" {
        "Reddit returned a web page instead of the feed (probably blocking this server)".into()
    } else if t == FeedType::Bluesky && s == Some(400) {
        format!("Bluesky account unavailable{}", e.api_error.as_ref().map(|a| format!(" ({a})")).unwrap_or_default())
    } else if s == Some(404) {
        "not found".into()
    } else if s == Some(410) {
        "the feed was removed".into()
    } else if s == Some(401) || s == Some(403) {
        "access denied".into()
    } else if s.is_some_and(|x| x >= 500) {
        "the site is having problems".into()
    } else {
        String::new()
    };
    clamp_text(&if hint.is_empty() { base } else { format!("{base} — {hint}") }, 200)
}

/// `feedStatus(feed)`: `(icon, text)` for `/feed list` and `/feed test`.
pub fn feed_status(feed: &Feed) -> (&'static str, String) {
    if !feed.enabled {
        return ("⏸️", format!("Disabled — {}", feed.disabled_reason.as_deref().filter(|r| !r.is_empty()).unwrap_or("paused")));
    }
    if feed.failure_count > 0 {
        return ("⚠️", format!("Failing {}× — {}", feed.failure_count, feed.last_error.as_deref().filter(|r| !r.is_empty()).unwrap_or("unknown error")));
    }
    if let Some(e) = feed.last_error.as_deref().filter(|e| !e.is_empty()) {
        return ("⚠️", e.to_string());
    }
    if !feed.initialized {
        return ("🕓", "Waiting for the first check".into());
    }
    ("✅", "OK".into())
}

// ── Cache (de)serialisation ────────────────────────────────────────────────────

/// `serializeForCache(parsed)`: the value of `feed:fetch:<sha1>`.
pub fn serialize_for_cache(title: Option<&str>, link: Option<&str>, image: Option<&str>, items: &[FeedItem], now: i64) -> Value {
    json!({
        "v": 1,
        "fetchedAt": iso(now),
        "title": title,
        "link": link,
        "image": image,
        "items": items.iter().take(WINDOW_SIZE).collect::<Vec<_>>(),
    })
}

fn str_or(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

/// `reviveCached(items)`: cached items with a string id; dates parsed back (`null` when unreadable).
pub fn revive_cached(items: &[Value]) -> Vec<FeedItem> {
    let mut out = Vec::new();
    for i in items {
        let Some(id) = i.get("id").and_then(Value::as_str) else { continue };
        let published_at = i.get("publishedAt").and_then(Value::as_str).filter(|s| !s.is_empty()).and_then(crate::js_date::parse_date);
        out.push(FeedItem {
            id: id.to_string(),
            title: str_or(i.get("title")),
            url: i.get("url").and_then(Value::as_str).map(String::from),
            author: str_or(i.get("author")),
            published_at,
            image: i.get("image").and_then(Value::as_str).map(String::from),
            summary: str_or(i.get("summary")),
            is_repost: i.get("isRepost").and_then(Value::as_bool).unwrap_or(false),
            actor_name: str_or(i.get("actorName")),
            actor_handle: str_or(i.get("actorHandle")),
        });
    }
    out
}

// ── Fetching ───────────────────────────────────────────────────────────────────

fn user_agent_for(t: FeedType) -> String {
    if t == FeedType::Reddit
        && let Some(ua) = std::env::var("REDDIT_USER_AGENT").ok().map(|s| bot_util::js::trim(&s).to_string()).filter(|s| !s.is_empty())
    {
        return ua;
    }
    bot_util::safe_fetch::default_user_agent().to_string()
}

fn accept_for(t: FeedType) -> &'static str {
    if t == FeedType::Bluesky { "application/json" } else { "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.8" }
}

fn from_failure(f: FetchFailure) -> FetchError {
    match f {
        FetchFailure::Safe(e) => FetchError {
            code: if e.code == SafeFetchCode::HttpError { "HTTP".into() } else { e.code.as_str().into() },
            status: e.status,
            message: e.message,
            retry_after_ms: e.retry_after_ms,
            ..Default::default()
        },
        FetchFailure::Other(m) => FetchError::new("NETWORK", clamp_text(&m, 120)),
    }
}

fn parse_json_body(buffer: &[u8]) -> Option<Value> {
    serde_json::from_str(&String::from_utf8_lossy(buffer)).ok()
}

/// What [`fetch_and_parse`] got.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum FetchOutcome {
    Parsed { parsed: ParsedFeed, etag: Option<String>, last_modified: Option<String>, final_url: String },
    NotModified,
    Failed(FetchError),
}

/// `fetchAndParse(url, type, { headers })`: fetches and parses one feed URL. Never fails.
pub async fn fetch_and_parse(app: &App, url: &str, t: FeedType, headers: &[(String, String)]) -> FetchOutcome {
    let mut all = vec![("user-agent".to_string(), user_agent_for(t)), ("accept".to_string(), accept_for(t).to_string())];
    all.extend(headers.iter().cloned());
    let opts = FetchOptions { headers: all, timeout_ms: FETCH_TIMEOUT_MS, max_bytes: max_bytes(t), max_redirects: 5, ..Default::default() };
    let res = match net::fetcher(app).fetch(url.to_string(), opts).await {
        Ok(r) => r,
        Err(f) => return FetchOutcome::Failed(from_failure(f)),
    };
    if res.status == 304 {
        let conditional = headers.iter().any(|(k, v)| {
            let k = k.to_ascii_lowercase();
            (k == "if-none-match" || k == "if-modified-since") && !bot_util::js::trim(v).is_empty()
        });
        if conditional {
            return FetchOutcome::NotModified;
        }
        let mut e = FetchError::new("UNEXPECTED_304", "HTTP 304 without a conditional request");
        e.status = Some(304);
        return FetchOutcome::Failed(e);
    }
    if !res.ok {
        let api_error = if t == FeedType::Bluesky {
            parse_json_body(&res.buffer).and_then(|j| j.get("error").and_then(Value::as_str).map(|e| clamp_text(e, 60)))
        } else {
            None
        };
        return FetchOutcome::Failed(FetchError {
            code: "HTTP".into(),
            status: Some(res.status),
            message: format!("HTTP {}", res.status),
            retry_after_ms: if [429, 503].contains(&res.status) {
                parse_retry_after_ms(res.headers.get("retry-after").map(String::as_str), now_ms(app))
            } else {
                None
            },
            api_error,
            links: Vec::new(),
        });
    }
    let parsed: Result<ParsedFeed, FeedParseError> = if t == FeedType::Bluesky {
        match parse_json_body(&res.buffer) {
            Some(j) => parse_bluesky_author_feed(&j),
            None => return FetchOutcome::Failed(FetchError::new("BAD_JSON", "Bluesky sent invalid JSON")),
        }
    } else {
        let ct = res.headers.get("content-type").map(String::as_str).unwrap_or("");
        let body = decode_xml_buffer(&res.buffer, ct);
        if looks_like_html(&body, ct) {
            let mut e = FetchError::new("HTML_PAGE", "Web page, not a feed");
            e.links = discover_feed_links(&body, if res.url.is_empty() { url } else { &res.url });
            return FetchOutcome::Failed(e);
        }
        parse_xml_feed(&body)
    };
    match parsed {
        Err(e) => FetchOutcome::Failed(FetchError::new(&e.code, e.message)),
        Ok(mut p) => {
            p.items = window_items(std::mem::take(&mut p.items));
            let header = |k: &str| res.headers.get(k).map(|v| clamp_text(v, 200)).filter(|v| !v.is_empty());
            FetchOutcome::Parsed {
                parsed: p,
                etag: header("etag"),
                last_modified: header("last-modified"),
                final_url: if res.url.is_empty() { url.to_string() } else { res.url.clone() },
            }
        }
    }
}

/// `redditOverBudget()`: one 60 s slot key per allowed fetch; with Redis down the in-process budget applies.
async fn reddit_over_budget(app: &App) -> bool {
    for slot in 0..REDDIT_MAX_PER_MIN {
        match app.cache().set_nx(&format!("{REDDIT_RL_KEY}:{slot}"), &1, 60).await {
            None => return !local_reddit_allow(app, now_ms(app)),
            Some(true) => return false,
            Some(false) => {}
        }
    }
    true
}

fn budget_error() -> FetchError {
    FetchError::new("RATE_BUDGET", "Reddit checks are paused for about a minute (bot-wide limit)")
}

/// `redditBlockedUntil()`: when Reddit last answered 429 every Reddit feed waits. Fail-open.
async fn reddit_blocked_until(app: &App) -> Option<i64> {
    let block = app.cache().get(REDDIT_BLOCK_KEY).await?;
    let until = crate::js_date::date_parse(block.get("until")?.as_str()?);
    (until.is_finite() && until > now_ms(app) as f64).then_some(until as i64)
}

/// What [`fetch_feed_source`] got.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceResult {
    Ok {
        not_modified: bool,
        from_cache: bool,
        title: Option<String>,
        image: Option<String>,
        items: Vec<FeedItem>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    Err {
        error: FetchError,
        /// Refused by the bot-wide Reddit budget or block; nothing was fetched.
        budget: bool,
    },
}

/// `fetchFeedSource(feed, { useCache, conditional })`: the shared Redis cache first, then the network (a conditional
/// GET for an initialised feed). Never fails.
pub async fn fetch_feed_source(app: &App, feed: &Feed, use_cache: bool, conditional: bool) -> SourceResult {
    let key = fetch_cache_key(&feed.fetch_url);
    if use_cache
        && let Some(hit) = app.cache().get(&key).await
        && hit.get("v").and_then(Value::as_f64) == Some(1.0)
        && let Some(items) = hit.get("items").and_then(Value::as_array)
    {
        return SourceResult::Ok {
            not_modified: false,
            from_cache: true,
            title: hit.get("title").and_then(Value::as_str).map(String::from),
            image: hit.get("image").and_then(Value::as_str).map(String::from),
            items: revive_cached(items),
            etag: None,
            last_modified: None,
        };
    }
    if feed.r#type == FeedType::Reddit {
        if let Some(until) = reddit_blocked_until(app).await {
            return SourceResult::Err {
                error: FetchError::new("RATE_BUDGET", format!("Reddit asked this bot to slow down until {}", iso(until))),
                budget: true,
            };
        }
        if reddit_over_budget(app).await {
            return SourceResult::Err { error: budget_error(), budget: true };
        }
    }
    let mut headers = Vec::new();
    if conditional && feed.initialized {
        if let Some(e) = feed.etag.as_deref().filter(|e| !e.is_empty()) {
            headers.push(("if-none-match".to_string(), e.to_string()));
        }
        if let Some(l) = feed.last_modified.as_deref().filter(|l| !l.is_empty()) {
            headers.push(("if-modified-since".to_string(), l.to_string()));
        }
    }
    match fetch_and_parse(app, &feed.fetch_url, feed.r#type, &headers).await {
        FetchOutcome::Failed(error) => SourceResult::Err { error, budget: false },
        FetchOutcome::NotModified => SourceResult::Ok {
            not_modified: true,
            from_cache: false,
            title: None,
            image: None,
            items: Vec::new(),
            etag: None,
            last_modified: None,
        },
        FetchOutcome::Parsed { parsed, etag, last_modified, .. } => {
            let value = serialize_for_cache(parsed.title.as_deref(), parsed.link.as_deref(), parsed.image.as_deref(), &parsed.items, now_ms(app));
            app.cache().set(&key, &value, cache_ttl_seconds(feed.r#type)).await;
            SourceResult::Ok { not_modified: false, from_cache: false, title: parsed.title, image: parsed.image, items: parsed.items, etag, last_modified }
        }
    }
}

// ── Add-time resolution ────────────────────────────────────────────────────────

const NOT_A_FEED_PAGE: &str = "That page is a website, not an RSS/Atom feed, and it doesn't link to one. Paste the feed URL itself (it often ends in /feed, /rss or .xml).";

/// The probe made when a source was resolved: it seeds the feed when it worked.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    Ok { items: Vec<FeedItem>, etag: Option<String>, last_modified: Option<String> },
    Failed(FetchError),
}

impl Probe {
    pub fn is_ok(&self) -> bool {
        matches!(self, Probe::Ok { .. })
    }
}

/// A resolved source.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub source: String,
    pub fetch_url: String,
    pub display_name: String,
    pub probe: Probe,
    pub warnings: Vec<String>,
    pub image: Option<String>,
}

/// A refusal: `{ ok: false, title, message }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub title: String,
    pub message: String,
}

fn refuse(title: &str, message: impl Into<String>) -> Result<Resolved, Refusal> {
    Err(Refusal { title: title.into(), message: message.into() })
}

/// What an admin typed, per type: RSS `Url`, YOUTUBE `ChannelId`, REDDIT `Name`, BLUESKY `Actor`, PODCAST `Url` or
/// `Query`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceInput {
    Url(String),
    ChannelId(String),
    Name(String),
    Actor(String),
    Query(String),
}

async fn resolve_xml_feed_url(app: &App, start_url: &str, t: FeedType) -> Result<Resolved, Refusal> {
    let mut url = start_url.to_string();
    let mut r = fetch_and_parse(app, &url, t, &[]).await;
    if let FetchOutcome::Failed(e) = &r
        && e.code == "HTML_PAGE"
    {
        let Some(first) = e.links.first().cloned() else { return refuse("Not a Feed", NOT_A_FEED_PAGE) };
        url = first;
        r = fetch_and_parse(app, &url, t, &[]).await;
        if matches!(&r, FetchOutcome::Failed(e) if e.code == "HTML_PAGE") {
            return refuse("Not a Feed", NOT_A_FEED_PAGE);
        }
    }
    let (parsed, etag, last_modified) = match r {
        FetchOutcome::Parsed { parsed, etag, last_modified, .. } => (parsed, etag, last_modified),
        FetchOutcome::NotModified => return refuse("Could Not Fetch", format!("I couldn't load that feed: {}. Check the URL and try again.", describe_fetch_error(t, &FetchError::default()))),
        FetchOutcome::Failed(e) => {
            if e.code == "TOO_LARGE" {
                return if t == FeedType::Podcast {
                    refuse("Feed Too Large", "That podcast feed is bigger than 10 MB, which is too large to check regularly.")
                } else {
                    refuse("Feed Too Large", "That feed is bigger than 2 MB, which is too large to check regularly. If it's a podcast, use `/feed add podcast` (allows up to 10 MB).")
                };
            }
            if ["NOT_A_FEED", "INVALID_XML", "BAD_JSON"].contains(&e.code.as_str()) {
                return refuse("Not a Feed", "I couldn't read that as an RSS or Atom feed.");
            }
            return refuse("Could Not Fetch", format!("I couldn't load that feed: {}. Check the URL and try again.", describe_fetch_error(t, &e)));
        }
    };
    let display_name = match parsed.title.clone() {
        Some(t) if !t.is_empty() => t,
        _ => url::Url::parse(&url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_else(|| "Feed".into()),
    };
    Ok(Resolved {
        source: url.clone(),
        fetch_url: url,
        display_name,
        probe: Probe::Ok { items: parsed.items, etag, last_modified },
        warnings: Vec::new(),
        image: parsed.image,
    })
}

async fn set_reddit_block(app: &App, error: &FetchError, now: i64) -> i64 {
    let ttl_ms = error.retry_after_ms.unwrap_or(15.0 * 60_000.0).clamp(10.0 * 60_000.0, 60.0 * 60_000.0);
    let until = now + ttl_ms as i64;
    app.cache().set(REDDIT_BLOCK_KEY, &json!({ "until": iso(until) }), (ttl_ms / 1000.0).ceil() as i64).await;
    until
}

async fn resolve_youtube(app: &App, channel_id: &str) -> Result<Resolved, Refusal> {
    let fetch_url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}");
    match fetch_and_parse(app, &fetch_url, FeedType::Youtube, &[]).await {
        FetchOutcome::Parsed { parsed, etag, last_modified, .. } => Ok(Resolved {
            source: channel_id.into(),
            display_name: parsed.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| channel_id.into()),
            fetch_url,
            probe: Probe::Ok { items: parsed.items, etag, last_modified },
            warnings: Vec::new(),
            image: None,
        }),
        other => {
            // Any failure, 404 included, is accepted: the public feed has been unreliable since late 2025.
            let error = match other {
                FetchOutcome::Failed(e) => e,
                _ => FetchError::new("UNKNOWN", "No response"),
            };
            Ok(Resolved {
                source: channel_id.into(),
                fetch_url,
                display_name: channel_id.into(),
                probe: Probe::Failed(error),
                warnings: vec!["⚠️ YouTube's public channel feed has been unreliable since late 2025 and often returns 404. I'll keep trying; if it never works, `/feed list` shows the error and the feed switches itself off after 20 failed checks.".into()],
                image: None,
            })
        }
    }
}

async fn resolve_reddit(app: &App, name: &str) -> Result<Resolved, Refusal> {
    let fetch_url = format!("https://www.reddit.com/r/{name}/new/.rss");
    let failed = |error: FetchError, why: &str| {
        let warning = format!(
            "⚠️ Reddit feeds are best-effort and {why} ({}). I'll keep trying and show the status in `/feed list`.",
            describe_fetch_error(FeedType::Reddit, &error)
        );
        Ok(Resolved {
            source: bot_util::js::lower(name),
            fetch_url: fetch_url.clone(),
            display_name: format!("r/{name}"),
            probe: Probe::Failed(error),
            warnings: vec![warning],
            image: None,
        })
    };
    if let Some(until) = reddit_blocked_until(app).await {
        return failed(FetchError::new("RATE_BUDGET", format!("Reddit asked this bot to slow down until {}", iso(until))), "is rate limiting this bot right now");
    }
    if reddit_over_budget(app).await {
        return failed(budget_error(), "this bot is at its Reddit request limit for the minute");
    }
    match fetch_and_parse(app, &fetch_url, FeedType::Reddit, &[]).await {
        FetchOutcome::Parsed { parsed, etag, last_modified, .. } => Ok(Resolved {
            source: bot_util::js::lower(name),
            fetch_url: fetch_url.clone(),
            display_name: format!("r/{name}"),
            probe: Probe::Ok { items: parsed.items, etag, last_modified },
            warnings: Vec::new(),
            image: None,
        }),
        other => {
            let error = match other {
                FetchOutcome::Failed(e) => e,
                _ => FetchError::new("UNKNOWN", "No response"),
            };
            if error.status == Some(404) {
                return refuse("Subreddit Not Found", format!("r/{name} doesn't exist, is banned, or can't be read."));
            }
            if error.status == Some(429) {
                set_reddit_block(app, &error, now_ms(app)).await;
            }
            failed(error, "Reddit refused the request just now")
        }
    }
}

/// JS truthiness of a JSON value.
fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn json_fetch_opts(max: u64) -> FetchOptions {
    FetchOptions {
        headers: vec![("user-agent".into(), bot_util::safe_fetch::default_user_agent().to_string())],
        timeout_ms: FETCH_TIMEOUT_MS,
        max_bytes: max,
        ..Default::default()
    }
}

async fn resolve_bluesky(app: &App, actor: &str) -> Result<Resolved, Refusal> {
    let appview = bsky_appview();
    let profile_url = format!("{appview}/xrpc/app.bsky.actor.getProfile?actor={}", bot_util::js::encode_uri_component(actor));
    let profile = match net::fetcher(app).fetch_json(profile_url, json_fetch_opts(256 * 1024)).await {
        Ok(p) => p,
        Err(FetchFailure::Safe(e)) if e.status == Some(400) => {
            return refuse("Account Not Found", format!("I couldn't find the Bluesky account **{}**.", clamp_text(actor, 100)));
        }
        Err(f) => {
            let e = match f {
                FetchFailure::Safe(_) => from_failure(f),
                FetchFailure::Other(m) => FetchError::new("NETWORK", m),
            };
            return refuse("Bluesky Unavailable", format!("Bluesky didn't answer ({}). Please try again in a minute.", describe_fetch_error(FeedType::Bluesky, &e)));
        }
    };
    let Some(did) = profile.get("did").and_then(Value::as_str).filter(|d| DID_RE.test(d)).map(String::from) else {
        return refuse("Bluesky Unavailable", "Bluesky didn't answer (unexpected response). Please try again in a minute.");
    };
    // `profile.handle && profile.handle !== 'handle.invalid' ? String(profile.handle) : did`
    let handle = match profile.get("handle") {
        Some(h) if js_truthy(h) && h.as_str() != Some("handle.invalid") => crate::feed_parsers::js_string(h),
        _ => did.clone(),
    };
    let fetch_url = format!("{appview}/xrpc/app.bsky.feed.getAuthorFeed?actor={}&filter=posts_no_replies&limit=30", bot_util::js::encode_uri_component(&did));
    let r = fetch_and_parse(app, &fetch_url, FeedType::Bluesky, &[]).await;
    let display_name = format!("@{handle}");
    match r {
        FetchOutcome::Parsed { parsed, etag, last_modified, .. } => Ok(Resolved {
            source: did,
            fetch_url,
            display_name,
            probe: Probe::Ok { items: parsed.items, etag, last_modified },
            warnings: Vec::new(),
            image: None,
        }),
        other => {
            let error = match other {
                FetchOutcome::Failed(e) => e,
                _ => FetchError::new("UNKNOWN", "No response"),
            };
            let warning = format!("⚠️ I found the account but couldn't load its posts yet ({}). I'll keep trying.", describe_fetch_error(FeedType::Bluesky, &error));
            Ok(Resolved { source: did, fetch_url, display_name, probe: Probe::Failed(error), warnings: vec![warning], image: None })
        }
    }
}

async fn resolve_podcast(app: &App, url: Option<&str>, query: Option<&str>) -> Result<Resolved, Refusal> {
    if let Some(u) = url {
        return resolve_xml_feed_url(app, u, FeedType::Podcast).await;
    }
    let q = bot_util::js::trim(query.unwrap_or("")).to_string();
    // Fail-open on a missing count (Redis down): a handful of admin searches cannot hurt Apple.
    if let Some(n) = app.cache().sliding_window(ITUNES_RL_KEY, 60_000, None).await
        && n > ITUNES_MAX_PER_MIN
    {
        return refuse("Podcast Search Busy", "Too many podcast searches right now. Try again in a minute, or paste the RSS feed URL.");
    }
    let search = format!("https://itunes.apple.com/search?media=podcast&entity=podcast&limit=5&term={}", bot_util::js::encode_uri_component(&q));
    let json = match net::fetcher(app).fetch_json(search, json_fetch_opts(512 * 1024)).await {
        Ok(j) => j,
        Err(f) => {
            let e = from_failure(f);
            return refuse("Podcast Search Failed", format!("Apple's podcast search didn't answer ({}). Try again, or paste the RSS feed URL.", describe_fetch_error(FeedType::Podcast, &e)));
        }
    };
    let Some(hit) = parse_itunes_search(&json).into_iter().next() else {
        return refuse("Podcast Not Found", format!("No podcast found for \"{}\". Try its exact title, or paste its RSS feed URL.", clamp_text(&q, 80)));
    };
    let mut r = resolve_xml_feed_url(app, &hit.feed_url, FeedType::Podcast).await?;
    if !hit.name.is_empty() {
        r.display_name = hit.name.clone();
    }
    let by = if hit.artist.is_empty() { String::new() } else { format!(" by {}", bot_core::markdown::escape_markdown(&hit.artist)) };
    r.warnings.push(format!(
        "Matched **{}**{by}. Not the right show? Remove it and add its RSS feed URL instead.",
        bot_core::markdown::escape_markdown(&hit.name)
    ));
    Ok(r)
}

/// `resolveFeedSource(type, input)`: resolves what an admin typed into a pollable source and probes it once (the
/// probe seeds the feed). Expected failures are a [`Refusal`]; the display name is one line (`'Feed'` when empty).
pub async fn resolve_feed_source(app: &App, t: FeedType, input: &SourceInput) -> Result<Resolved, Refusal> {
    let r = match (t, input) {
        (FeedType::Rss, SourceInput::Url(u)) => resolve_xml_feed_url(app, u, FeedType::Rss).await,
        (FeedType::Youtube, SourceInput::ChannelId(c)) => resolve_youtube(app, c).await,
        (FeedType::Reddit, SourceInput::Name(n)) => resolve_reddit(app, n).await,
        (FeedType::Bluesky, SourceInput::Actor(a)) => resolve_bluesky(app, a).await,
        (FeedType::Podcast, SourceInput::Url(u)) => resolve_podcast(app, Some(u), None).await,
        (FeedType::Podcast, SourceInput::Query(q)) => resolve_podcast(app, None, Some(q)).await,
        _ => return refuse("Unknown Feed Type", "That kind of feed is not supported."),
    };
    r.map(|mut ok| {
        let n = one_line(&ok.display_name);
        ok.display_name = if n.is_empty() { "Feed".into() } else { n };
        ok
    })
}

// ── Database helpers ───────────────────────────────────────────────────────────

fn select_feeds(where_sql: &str) -> String {
    format!("SELECT {} FROM {} WHERE {where_sql}", FEED_INFO.select_list(), FEED_INFO.quoted_table())
}

fn text(v: &str) -> SqlValue {
    SqlValue::Text(Some(v.to_string()))
}

/// `findFeedForGuild(guildId, raw)`: a feed of this guild by id, else by its display name (case-insensitive, Prisma's
/// unescaped `ILIKE`) when exactly one matches.
pub async fn find_feed_for_guild(pool: &bot_db::PgPool, guild_id: &str, raw: &str) -> anyhow::Result<Option<Feed>> {
    let by_id = bot_db::BuiltQuery { sql: select_feeds(r#""id" = $1 AND "guildId" = $2 LIMIT 1"#), args: vec![text(raw), text(guild_id)] };
    if let Some(f) = by_id.fetch_optional::<Feed, _>(pool).await? {
        return Ok(Some(f));
    }
    let by_name = bot_db::BuiltQuery {
        sql: select_feeds(r#""guildId" = $1 AND "displayName" ILIKE $2 ORDER BY "id" ASC LIMIT 2"#),
        args: vec![text(guild_id), text(bot_util::js::trim(raw))],
    };
    let rows = by_name.fetch_all::<Feed, _>(pool).await?;
    Ok(if rows.len() == 1 { rows.into_iter().next() } else { None })
}

/// The feeds of a guild (`findMany({ where: { guildId }, orderBy: { createdAt: 'asc' } })`).
pub async fn list_feeds(pool: &bot_db::PgPool, guild_id: &str) -> anyhow::Result<Vec<Feed>> {
    let q = bot_db::BuiltQuery { sql: select_feeds(r#""guildId" = $1 ORDER BY "createdAt" ASC"#), args: vec![text(guild_id)] };
    Ok(q.fetch_all::<Feed, _>(pool).await?)
}

/// A feed of a guild by id (`findFirst({ where: { id, guildId } })`).
pub async fn get_feed(pool: &bot_db::PgPool, guild_id: &str, id: &str) -> anyhow::Result<Option<Feed>> {
    let q = bot_db::BuiltQuery { sql: select_feeds(r#""id" = $1 AND "guildId" = $2 LIMIT 1"#), args: vec![text(id), text(guild_id)] };
    Ok(q.fetch_optional::<Feed, _>(pool).await?)
}

/// `prisma.feed.count({ where: { guildId } })`.
pub async fn count_feeds(pool: &bot_db::PgPool, guild_id: &str) -> anyhow::Result<i64> {
    let row: (i64,) = bot_db::sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "Feed" WHERE "guildId" = $1"#).bind(guild_id).fetch_one(pool).await?;
    Ok(row.0)
}

/// The arguments of [`create_feed_row`].
#[derive(Debug, Clone)]
pub struct NewFeed {
    pub guild_id: String,
    pub feed_type: FeedType,
    pub source: String,
    pub display_name: String,
    pub fetch_url: String,
    pub channel_id: String,
    pub ping_role_id: Option<String>,
    pub message_template: Option<String>,
    pub include_reposts: bool,
    pub created_by: String,
    pub probe: Option<Probe>,
}

/// `createFeedRow(...)`: creates a feed under the per-guild lock (cap check + insert). A successful probe seeds it
/// (`initialized`, `seenIds`), so nothing already in the feed is posted. Fails with a [`FeedUserError`] (Feed Limit
/// Reached, Already Following) for the admin; anything else is a database error.
pub async fn create_feed_row(app: &App, f: NewFeed) -> anyhow::Result<Feed> {
    let app2 = app.clone();
    let guild = f.guild_id.clone();
    app.ext::<FeedsState>()
        .create_mutex
        .run(&guild, move || async move {
            let app = app2;
            let limit = bot_config::limits::FEEDS as i64;
            if count_feeds(app.db(), &f.guild_id).await? >= limit {
                return Err(FeedUserError::new(
                    "Feed Limit Reached",
                    format!("This server already follows {limit} feeds (the maximum). Remove one with `/feed remove` first."),
                )
                .into());
            }
            app.settings().ensure_guild_settings(&f.guild_id).await?;
            let now = app.clock().now();
            let (seeded, seen, etag, lm) = match &f.probe {
                Some(Probe::Ok { items, etag, last_modified }) => {
                    let mut seen = unique_strings(items.iter().map(|i| &i.id));
                    seen.truncate(SEEN_CAP);
                    (true, seen, etag.clone(), last_modified.clone())
                }
                _ => (false, Vec::new(), None, None),
            };
            let last_error = match &f.probe {
                Some(Probe::Failed(e)) => Some(describe_fetch_error(f.feed_type, e)),
                _ => None,
            };
            let ts = |d: Option<DateTime<Utc>>| match d {
                Some(d) => SqlValue::timestamp(d),
                None => SqlValue::Timestamp(None),
            };
            let q = InsertBuilder::new(&FEED_INFO)
                .values([
                    ("guildId", json!(f.guild_id)),
                    ("type", json!(f.feed_type.as_str())),
                    ("source", json!(f.source)),
                    ("displayName", json!(clamp_text(&one_line(&f.display_name), 100))),
                    ("fetchUrl", json!(f.fetch_url)),
                    ("channelId", json!(f.channel_id)),
                    ("pingRoleId", json!(f.ping_role_id)),
                    ("messageTemplate", json!(f.message_template)),
                    ("includeReposts", json!(f.feed_type == FeedType::Bluesky && f.include_reposts)),
                    ("createdBy", json!(f.created_by)),
                    ("initialized", json!(seeded)),
                    ("seenIds", json!(seen)),
                    ("etag", json!(etag)),
                    ("lastModified", json!(lm)),
                    ("lastError", json!(last_error)),
                ])?
                .typed("lastCheckedAt", ts(f.probe.as_ref().map(|_| now)))?
                .typed("lastSuccessAt", ts(seeded.then_some(now)))?
                .build()?;
            match q.fetch_one::<Feed, _>(app.db()).await {
                Ok(row) => Ok(row),
                Err(e) if e.is_unique_violation() => {
                    Err(FeedUserError::new("Already Following", format!("That feed is already posted in <#{}>.", f.channel_id)).into())
                }
                Err(e) => Err(e.into()),
            }
        })
        .await
}

/// `scheduleFeedJob(feed, { runAt })`: (re)schedules the poll job; `replace` bumps the version so an in-flight run
/// finishes as a no-op.
pub async fn schedule_feed_job(app: &App, guild_id: &str, feed_id: &str, run_at: DateTime<Utc>) -> anyhow::Result<()> {
    app.scheduler()
        .schedule_job(
            ScheduleJob::new(guild_id, JobType::FeedPoll, run_at)
                .payload(json!({ "feedId": feed_id }))
                .dedupe_key(feed_job_key(feed_id))
                .max_attempts(10)
                .on_conflict(OnConflict::Replace),
        )
        .await?;
    Ok(())
}

/// One line of a disable notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisabledFeed {
    pub feed_type: FeedType,
    pub display_name: String,
    pub channel_id: String,
    pub disabled_reason: String,
}

static CHANNEL_PROBLEM: LazyRe = lazy_re!(r"^(Its channel|I lost access)", "");

/// The embed of a disable notice.
pub fn disable_notice_embed(feeds: &[DisabledFeed]) -> CreateEmbed {
    let lines: Vec<String> =
        feeds.iter().map(|f| format!("{} **{}** — {}", type_emoji(f.feed_type), bot_core::markdown::escape_markdown(&f.display_name), f.disabled_reason)).collect();
    let channel_problem = feeds.iter().any(|f| CHANNEL_PROBLEM.test(&f.disabled_reason));
    let body = format!(
        "{}\n{}",
        bot_util::text::join_with_limit(&lines, "\n", 3500, bot_util::text::default_more),
        if channel_problem { "Pick a new channel with `/feed edit channel:`, then run `/feed enable`." } else { "Fix the problem, then run `/feed enable`." }
    );
    warn_embed(if feeds.len() == 1 { "Feed Disabled" } else { "Feeds Disabled" }, Some(&body), None)
}

fn parse_snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// Whether the bot can post in a channel of a guild (`checkChannelUsable(ch, { needs })`), from the cache.
pub fn usable_channel(app: &App, guild_id: GuildId, channel_id: &str, needs: &[&str]) -> Option<GuildChannel> {
    let cid = ChannelId::new(parse_snowflake(channel_id)?);
    let ch = cached_channel(app, guild_id, cid)?;
    let perms = bot_permissions(app, PermTarget::Channel(guild_id, cid));
    channel_usable_verdict(Some(&ch), perms, needs).ok.then_some(ch)
}

/// `sendDisableNotice(guild, feeds, settings, { preferFeedChannel })`: one notice in the feed's channel (exactly one
/// feed, usable channel), else in the mod-log channel, else nowhere. Never fails; true when sent.
pub async fn send_disable_notice(app: &App, guild_id: &str, feeds: &[DisabledFeed], mod_log_channel_id: Option<&str>, prefer_feed_channel: bool) -> bool {
    let Some(gid) = parse_snowflake(guild_id).map(GuildId::new) else { return false };
    let mut target = None;
    if prefer_feed_channel && feeds.len() == 1 {
        target = usable_channel(app, gid, &feeds[0].channel_id, &POST_CHANNEL_NEEDS);
    }
    if target.is_none()
        && let Some(m) = mod_log_channel_id.filter(|m| !m.is_empty())
    {
        target = usable_channel(app, gid, m, &POST_CHANNEL_NEEDS);
    }
    let Some(ch) = target else { return false };
    let body = json!({ "embeds": [disable_notice_embed(feeds)], "allowed_mentions": { "parse": [] } });
    let sent = bot_util::async_util::with_timeout(net::rest(app).send_message(app.clone(), ch.id, body), SEND_TIMEOUT_MS, "feed notice").await;
    match sent {
        Ok(_) => true,
        Err(e) => {
            let msg = match e {
                bot_util::async_util::WithTimeoutError::Timeout(t) => t.to_string(),
                bot_util::async_util::WithTimeoutError::Inner(r) => r.message,
            };
            bot_util::log_warn!(format!("feeds: could not post the disable notice in guild {guild_id}: {msg}"));
            false
        }
    }
}

/// `disableFeed(feed, reason, ctx, settings, { notify })`: the conditional update guarantees exactly one notice per
/// disable. True when this call disabled it.
pub async fn disable_feed(app: &App, feed: &Feed, reason: &str, mod_log_channel_id: Option<&str>, notify: bool) -> anyhow::Result<bool> {
    let count = UpdateBuilder::new(&FEED_INFO)
        .set("enabled", json!(false))?
        .set("disabledReason", json!(clamp_text(reason, 200)))?
        .filter("id", json!(feed.id))?
        .filter("enabled", json!(true))?
        .without_returning()
        .build()?
        .execute(app.db())
        .await?;
    if count != 1 {
        return Ok(false);
    }
    app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(feed_job_key(&feed.id)), ..Default::default() }).await;
    bot_util::log_warn!(format!("Feed \"{}\" ({}) in guild {} was disabled: {reason}", feed.display_name, feed.id, feed.guild_id));
    if notify {
        let d = DisabledFeed {
            feed_type: feed.r#type,
            display_name: feed.display_name.clone(),
            channel_id: feed.channel_id.clone(),
            disabled_reason: reason.to_string(),
        };
        send_disable_notice(app, &feed.guild_id, &[d], mod_log_channel_id, true).await;
    }
    Ok(true)
}

/// `disableFeedsForChannel(ctx)` (`channelDelete`): disables, and keeps, the feeds posting there. Returns how many.
pub async fn disable_feeds_for_channel(app: &App, guild_id: &str, channel_id: &str, mod_log_channel_id: Option<&str>) -> anyhow::Result<usize> {
    let q = bot_db::BuiltQuery {
        sql: select_feeds(r#""guildId" = $1 AND "channelId" = $2 AND "enabled" = $3"#),
        args: vec![text(guild_id), text(channel_id), SqlValue::Bool(Some(true))],
    };
    let rows = q.fetch_all::<Feed, _>(app.db()).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let sql = r#"UPDATE "Feed" SET "enabled" = $1, "disabledReason" = $2, "updatedAt" = $3 WHERE "id" = ANY($4) AND "enabled" = $5"#.to_string();
    bot_db::BuiltQuery {
        sql,
        args: vec![
            SqlValue::Bool(Some(false)),
            text("Its channel was deleted"),
            SqlValue::timestamp(bot_db::now_utc()),
            SqlValue::TextArray(ids.clone()),
            SqlValue::Bool(Some(true)),
        ],
    }
    .execute(app.db())
    .await?;
    for id in &ids {
        app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(feed_job_key(id)), ..Default::default() }).await;
    }
    bot_util::log_info!(format!("Feeds: disabled {} feed(s) in guild {guild_id}; channel {channel_id} was deleted", rows.len()));
    if let Some(m) = mod_log_channel_id.filter(|m| !m.is_empty() && *m != channel_id) {
        let list: Vec<DisabledFeed> = rows
            .iter()
            .map(|r| DisabledFeed {
                feed_type: r.r#type,
                display_name: r.display_name.clone(),
                channel_id: r.channel_id.clone(),
                disabled_reason: "Its channel was deleted".into(),
            })
            .collect();
        send_disable_notice(app, guild_id, &list, Some(m), false).await;
    }
    Ok(rows.len())
}

/// `ensureGuildFeedJobs(guild)` (`guildReady`): every enabled feed gets a live poll job; healthy jobs are left alone,
/// missing or finished ones are replaced with a random spread.
pub async fn ensure_guild_feed_jobs(app: &App, guild_id: &str) -> anyhow::Result<()> {
    let q = bot_db::BuiltQuery { sql: select_feeds(r#""guildId" = $1 AND "enabled" = $2"#), args: vec![text(guild_id), SqlValue::Bool(Some(true))] };
    let rows = q.fetch_all::<Feed, _>(app.db()).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let keys: Vec<String> = rows.iter().map(|r| feed_job_key(&r.id)).collect();
    let Some(jobs) = app.scheduler().get_jobs_by_keys(&keys).await else { return Ok(()) };
    for r in rows {
        if let Some(job) = jobs.get(&feed_job_key(&r.id))
            && matches!(job.status, bot_db::JobStatus::Pending | bot_db::JobStatus::Running)
        {
            continue;
        }
        let spread = 30_000 + random_int(base_interval_ms(r.r#type));
        if let Err(e) = schedule_feed_job(app, &r.guild_id, &r.id, at_ms(now_ms(app) + spread)).await {
            bot_util::log_error!(format!("feeds: could not ensure job for {}", r.id), bot_core::error::log_arg(&e));
        }
    }
    Ok(())
}

// ── Polling ────────────────────────────────────────────────────────────────────

fn reschedule(at: DateTime<Utc>) -> JobOutcome {
    JobOutcome::Reschedule { next_run_at: at, payload: None }
}

async fn record_channel_problem(app: &App, feed: &Feed, text_: &str, now: DateTime<Utc>) -> anyhow::Result<()> {
    UpdateBuilder::new(&FEED_INFO)
        .set("lastError", json!(clamp_text(text_, 200)))?
        .set_typed("lastCheckedAt", SqlValue::timestamp(now))?
        .filter("id", json!(feed.id))?
        .without_returning()
        .build()?
        .execute(app.db())
        .await?;
    Ok(())
}

async fn on_fetch_failure(app: &App, feed: &Feed, error: &FetchError, mod_log: Option<&str>, now: DateTime<Utc>) -> anyhow::Result<JobOutcome> {
    if feed.r#type == FeedType::Reddit && error.status == Some(429) {
        let until = set_reddit_block(app, error, now.timestamp_millis()).await;
        let text_ = format!(
            "{} — all Reddit feeds wait until {}",
            describe_fetch_error(FeedType::Reddit, error),
            bot_util::time::discord_timestamp_ms(until as f64, "t")
        );
        record_channel_problem(app, feed, &text_, now).await?;
        return Ok(reschedule(at_ms(until + random_int(60_000))));
    }
    let sql = format!(
        r#"UPDATE "Feed" SET "failureCount" = ("Feed"."failureCount" + $1), "lastError" = $2, "lastCheckedAt" = $3, "updatedAt" = $4 WHERE "id" = $5 RETURNING {}"#,
        FEED_INFO.select_list()
    );
    let q = bot_db::BuiltQuery {
        sql,
        args: vec![
            SqlValue::Int(Some(1)),
            text(&describe_fetch_error(feed.r#type, error)),
            SqlValue::timestamp(now),
            SqlValue::timestamp(bot_db::now_utc()),
            text(&feed.id),
        ],
    };
    let Ok(Some(updated)) = q.fetch_optional::<Feed, _>(app.db()).await else { return Ok(JobOutcome::Done) };
    if updated.failure_count >= MAX_FAILURES {
        let reason = format!("{MAX_FAILURES} failed checks in a row: {}", updated.last_error.clone().unwrap_or_default());
        disable_feed(app, &updated, &reason, mod_log, true).await?;
        return Ok(JobOutcome::Done);
    }
    Ok(reschedule(compute_next_poll_at(NextPoll {
        failure_count: updated.failure_count,
        status: error.status,
        retry_after_ms: error.retry_after_ms,
        ..NextPoll::new(feed.r#type, now.timestamp_millis())
    })))
}

/// The channel a poll posts in: the cached one, else a REST fetch.
enum ChannelLookup {
    Found(Box<GuildChannel>),
    Gone,
    Transient,
}

async fn lookup_channel(app: &App, guild_id: GuildId, channel_id: &str) -> ChannelLookup {
    let Some(cid) = parse_snowflake(channel_id).map(ChannelId::new) else { return ChannelLookup::Gone };
    if let Some(ch) = cached_channel(app, guild_id, cid) {
        return ChannelLookup::Found(Box::new(ch));
    }
    // `guild.channels.fetch(id)`: another guild's channel is GuildChannelUnowned (no code), i.e. transient.
    match net::fetch_guild_channel(app, guild_id, cid).await {
        Ok(ch) => ChannelLookup::Found(Box::new(ch)),
        Err(e) if matches!(e.code, Some(10003) | Some(50001)) => ChannelLookup::Gone,
        Err(_) => ChannelLookup::Transient,
    }
}

/// The bot's permissions in a channel (cached guild; also for a channel the cache does not have).
fn bot_perms_in(app: &App, guild: Option<&Guild>, channel: &GuildChannel) -> Option<serenity::all::Permissions> {
    let g = guild?;
    let (id, roles) = bot_member(app, g)?;
    bot_core::permissions::member_permissions_in(g, channel, id, &roles, true)
}

const GONE_REASON: &str = "Its channel was deleted or I lost access to it";

struct PollClock {
    started: i64,
    now: DateTime<Utc>,
}

#[allow(clippy::too_many_arguments)]
async fn deliver_items(
    app: &App,
    feed: &Feed,
    sel: &Selection,
    image: Option<&str>,
    guild: Option<&Guild>,
    channel: &GuildChannel,
    mod_log: Option<&str>,
    clock: &PollClock,
) -> anyhow::Result<JobOutcome> {
    let mut retry: HashSet<String> = HashSet::new();
    let mut posted = 0;
    let mut problem: Option<String> = None;
    for (k, item) in sel.to_post.iter().enumerate() {
        if now_ms(app) - clock.started > SEND_BUDGET_MS {
            retry.extend(sel.to_post[k..].iter().map(|i| i.id.clone()));
            break;
        }
        let payload = build_post_payload(app, feed, item, &PostCtx { guild, channel: Some(channel), feed_image: image, ping: true, now: app.clock().now() });
        let send = net::rest(app).send_message(app.clone(), channel.id, payload.to_json());
        match bot_util::async_util::with_timeout(send, SEND_TIMEOUT_MS, &format!("feed send {}", feed.id)).await {
            Ok(_) => posted += 1,
            Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => continue, // may have landed: keep it recorded
            Err(bot_util::async_util::WithTimeoutError::Inner(err)) => {
                if err.timeout {
                    continue;
                }
                if matches!(err.code, Some(10003) | Some(50001)) {
                    disable_feed(app, feed, "Its channel was deleted or I lost access to it", mod_log, true).await?;
                    return Ok(JobOutcome::Done);
                }
                if err.code == Some(50035) {
                    bot_util::log_warn!(format!("Feed {}: Discord rejected item {}: {}", feed.id, item.id, err.message));
                    continue;
                }
                problem = Some(if err.code == Some(50013) {
                    format!("Can't post in #{}: Missing Permissions", channel.name)
                } else {
                    format!("Couldn't post: {}", clamp_text(if err.message.is_empty() { "error" } else { &err.message }, 120))
                });
                retry.extend(sel.to_post[k..].iter().map(|i| i.id.clone()));
                break;
            }
        }
    }
    let mut upd = UpdateBuilder::new(&FEED_INFO);
    let mut any = false;
    if posted > 0 {
        upd = upd.set_typed("lastPostedAt", SqlValue::timestamp(app.clock().now()))?;
        any = true;
    }
    if !retry.is_empty() {
        let keep: Vec<&String> = sel.next_seen_ids.iter().filter(|id| !retry.contains(*id)).collect();
        upd = upd.set("seenIds", json!(keep))?;
        any = true;
    }
    if let Some(p) = &problem {
        upd = upd.set("lastError", json!(clamp_text(p, 200)))?;
        any = true;
    }
    if any {
        let r = upd.filter("id", json!(feed.id))?.without_returning().build()?.execute(app.db()).await;
        if let Err(e) = r {
            bot_util::log_warn!(format!("feed {} post-send update failed: {e}", feed.id));
        }
    }
    Ok(reschedule(compute_next_poll_at(NextPoll::new(feed.r#type, clock.now.timestamp_millis()))))
}

async fn run_poll(app: &App, feed: &Feed, mod_log: Option<&str>, clock: &PollClock) -> anyhow::Result<JobOutcome> {
    let gid = GuildId::new(parse_snowflake(&feed.guild_id).unwrap_or(1));
    let now = clock.now;
    // 1. Channel. A cache miss is never a deletion: a REST fetch confirms it first.
    let channel = match lookup_channel(app, gid, &feed.channel_id).await {
        ChannelLookup::Found(c) => *c,
        ChannelLookup::Gone => {
            disable_feed(app, feed, GONE_REASON, mod_log, true).await?;
            return Ok(JobOutcome::Done);
        }
        ChannelLookup::Transient => return Ok(reschedule(at_ms(now.timestamp_millis() + 5 * MINUTE_MS))),
    };
    if !FEED_CHANNEL_TYPES.contains(&u8::from(channel.kind)) {
        disable_feed(app, feed, "Its channel can no longer receive posts", mod_log, true).await?;
        return Ok(JobOutcome::Done);
    }
    let guild = cached_guild(app, gid);

    // 2. Permissions, before fetching: nothing is fetched that cannot be posted.
    let u = channel_usable_verdict(Some(&channel), bot_perms_in(app, guild.as_ref(), &channel), &POST_CHANNEL_NEEDS);
    if !u.ok {
        if u.missing.iter().any(|m| m == "ViewChannel") {
            disable_feed(app, feed, &format!("I lost access to #{} (View Channel)", channel.name), mod_log, true).await?;
            return Ok(JobOutcome::Done);
        }
        record_channel_problem(app, feed, &format!("Can't post in #{}: missing {}", channel.name, u.missing.join(", ")), now).await?;
        return Ok(reschedule(compute_next_poll_at(NextPoll { failure_count: 1, ..NextPoll::new(feed.r#type, now.timestamp_millis()) })));
    }

    // 3. The Reddit block set by any 429 (fail-open).
    if feed.r#type == FeedType::Reddit
        && let Some(until) = reddit_blocked_until(app).await
    {
        return Ok(reschedule(at_ms(until.max(clock.started + 60_000) + random_int(60_000))));
    }

    // 4. Fetch.
    let (not_modified, from_cache, title, image, items, etag, last_modified) = match fetch_feed_source(app, feed, true, true).await {
        SourceResult::Err { budget: true, .. } => return Ok(reschedule(at_ms(now_ms(app) + 60_000 + random_int(60_000)))),
        SourceResult::Err { error, .. } => return on_fetch_failure(app, feed, &error, mod_log, now).await,
        SourceResult::Ok { not_modified, from_cache, title, image, items, etag, last_modified } => (not_modified, from_cache, title, image, items, etag, last_modified),
    };

    // 5. Not modified.
    if not_modified {
        UpdateBuilder::new(&FEED_INFO)
            .set_typed("lastCheckedAt", SqlValue::timestamp(now))?
            .set_typed("lastSuccessAt", SqlValue::timestamp(now))?
            .set("failureCount", json!(0))?
            .set("lastError", Value::Null)?
            .filter("id", json!(feed.id))?
            .without_returning()
            .build()?
            .execute(app.db())
            .await?;
        return Ok(reschedule(compute_next_poll_at(NextPoll::new(feed.r#type, now.timestamp_millis()))));
    }

    // 6. Select.
    let sel = select_new_items(SelectInput {
        items: &items,
        seen_ids: &feed.seen_ids,
        initialized: feed.initialized,
        baseline: Some(feed.last_success_at.unwrap_or(feed.created_at)),
        include_reposts: feed.include_reposts,
        max_post: MAX_POSTS_PER_POLL,
    });
    if sel.reseed && feed.initialized {
        bot_util::log_info!(format!("Feed {}: ids all changed, re-seeded {} item(s) without posting", feed.id, sel.skipped_old));
    }
    if sel.overflow > 0 {
        bot_util::log_info!(format!("Feed {}: skipped {} older new item(s) over the per-poll cap", feed.id, sel.overflow));
    }

    // 7. Claim (optimistic, idempotent): ids are recorded before sending, so a crash loses items, never doubles them.
    let rename = display_name_update(feed, title.as_deref(), &items);
    let mut claim = UpdateBuilder::new(&FEED_INFO)
        .set("seenIds", json!(sel.next_seen_ids))?
        .set("initialized", json!(true))?
        .set_typed("lastCheckedAt", SqlValue::timestamp(now))?
        .set_typed("lastSuccessAt", SqlValue::timestamp(now))?
        .set("failureCount", json!(0))?
        .set("lastError", Value::Null)?;
    if !from_cache {
        claim = claim.set("etag", json!(etag))?.set("lastModified", json!(last_modified))?;
    }
    if let Some(r) = &rename {
        claim = claim.set("displayName", json!(r))?;
    }
    let count = claim
        .filter("id", json!(feed.id))?
        .filter("enabled", json!(true))?
        .filter("updatedAt", json!(bot_db::format_js_date(&feed.updated_at)))?
        .without_returning()
        .build()?
        .execute(app.db())
        .await?;
    if count != 1 {
        return Ok(reschedule(at_ms(clock.started + 60_000)));
    }

    // 8. Deliver.
    if sel.to_post.is_empty() {
        return Ok(reschedule(compute_next_poll_at(NextPoll::new(feed.r#type, now.timestamp_millis()))));
    }
    let mut posting = feed.clone();
    if let Some(r) = rename {
        posting.display_name = r;
    }
    deliver_items(app, &posting, &sel, image.as_deref(), guild.as_ref(), &channel, mod_log, clock).await
}

/// `pollFeedJob(job, ctx)`, the `feed.poll` handler: fails (and the scheduler retries) only when the first database
/// read fails; every fetch, parse or Discord error ends in a reschedule, an auto-disable, or a dropped job.
pub async fn poll_feed_job(job: Job, app: App) -> BotResult<JobOutcome> {
    let Some(feed_id) = job.payload.get("feedId").and_then(Value::as_str) else { return Ok(JobOutcome::Done) };
    let Some(feed) = get_feed(app.db(), &job.guild_id, feed_id).await? else { return Ok(JobOutcome::Done) };
    poll_feed_row(&app, &feed).await
}

/// The rest of [`poll_feed_job`] once the row was read: `feed` is the row as this run saw it, so a run whose
/// snapshot is stale (edited, paused, removed, or claimed by a parallel run) records and posts nothing.
pub async fn poll_feed_row(app: &App, feed: &Feed) -> BotResult<JobOutcome> {
    let started = now_ms(app);
    let clock = PollClock { started, now: at_ms(started) };
    if !feed.enabled {
        return Ok(JobOutcome::Done);
    }
    let Some(settings) = app.settings().get_settings_safe(&feed.guild_id).await else {
        return Ok(reschedule(at_ms(started + 5 * MINUTE_MS)));
    };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Feeds)) {
        return Ok(reschedule(at_ms(started + MODULE_OFF_RECHECK_MS)));
    }
    let mod_log = settings.mod_log_channel_id.clone();
    match run_poll(app, feed, mod_log.as_deref(), &clock).await {
        Ok(o) => Ok(o),
        Err(e) => {
            bot_util::log_error!(format!("feed.poll {} (guild {}) failed unexpectedly", feed.id, feed.guild_id), bot_core::error::log_arg(&e));
            Ok(reschedule(compute_next_poll_at(NextPoll { failure_count: feed.failure_count, ..NextPoll::new(feed.r#type, started) })))
        }
    }
}
