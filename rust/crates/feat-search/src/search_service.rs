//! Port of `src/services/searchService.js` (pkg-search): `/search anime` and `/search manga` (AniList GraphQL),
//! `/search pokemon` (PokeAPI) and `/search urban` (Urban Dictionary). Every API is keyless.
//!
//! Outbound traffic is deliberately polite. Every request goes through `safeFetch` / `safeFetchJson` (here
//! [`SearchFetch`], production [`SafeFetchDeps`]) and passes, in this order:
//!   Redis cache (projected objects only) -> single-flight per query -> per-source pause (429 / Retry-After /
//!   X-RateLimit-Remaining) -> 3 s per-user fetch cooldown -> per-source requests-per-minute budget (a per-user and a
//!   per-server slice first) -> the request.
//! Cache hits and single-flight followers consume nothing. Errors are never cached. Autocomplete never touches the
//! network. Redis is fail-open everywhere; nothing here deletes keys.
//!
//! Third-party text is stripped of HTML, markdown-escaped and defanged (mentions, Discord tokens), then clamped, before
//! it is cached or rendered. Replies carry only embeds, so nothing can ping.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use bot_core::error::BoxFuture;
use bot_core::interaction_helpers::{Choice, Payload, is_interaction_gone, reply_error, respond_autocomplete, safe_defer, safe_reply};
use bot_core::markdown::escape_markdown;
use bot_core::paginator::{FooterMode, PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_util::jsre::LazyRe;
use bot_util::lazy_re;
use bot_util::safe_fetch::{Body, FetchOptions, FetchResponse, SafeFetchCode, SafeFetchError};
use bot_util::text::{clamp_text, default_more, format_number, join_with_limit, progress_bar};
use futures::FutureExt;
use futures::future::Shared;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, Timestamp};
use serenity::builder::CreateEmbed;
use sha1::{Digest, Sha1};
use unicode_normalization::UnicodeNormalization;

use crate::ops;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const SEARCH_TIMEOUT_MS: u64 = 8_000;
pub const MAX_RESULTS: usize = 5;
pub const FETCH_COOLDOWN_SECONDS: u64 = 3;
/// `CACHE_TTL` (seconds).
pub mod cache_ttl {
    pub const ANILIST: u64 = 6 * 3600;
    pub const URBAN: u64 = 6 * 3600;
    pub const POKEAPI: u64 = 7 * 24 * 3600;
    pub const NEGATIVE: u64 = 3600;
}
/// `MAX_BYTES`.
pub mod max_bytes {
    pub const ANILIST: u64 = 512 * 1024;
    pub const URBAN: u64 = 1024 * 1024;
    pub const POKEAPI: u64 = 3 * 1024 * 1024;
    pub const POKE_INDEX: u64 = 1024 * 1024;
}
pub const ANILIST_URL: &str = "https://graphql.anilist.co";
pub const POKEAPI_BASE: &str = "https://pokeapi.co/api/v2";
pub const URBAN_DEFINE_URL: &str = "https://api.urbandictionary.com/v0/define";
pub const ANILIST_PAUSE_KEY: &str = "search:anilist:pause";
pub const POKEAPI_INDEX_KEY: &str = "search:pokeapi:index";
pub const STAT_ORDER: [&str; 6] = ["hp", "attack", "defense", "special-attack", "special-defense", "speed"];

/// `STAT_LABELS`.
pub fn stat_label(key: &str) -> Option<&'static str> {
    Some(match key {
        "hp" => "HP",
        "attack" => "Attack",
        "defense" => "Defense",
        "special-attack" => "Sp. Atk",
        "special-defense" => "Sp. Def",
        "speed" => "Speed",
        _ => return None,
    })
}

/// `FORMAT_LABELS`.
pub fn format_label(key: &str) -> Option<&'static str> {
    Some(match key {
        "TV" => "TV",
        "TV_SHORT" => "TV Short",
        "MOVIE" => "Movie",
        "SPECIAL" => "Special",
        "OVA" => "OVA",
        "ONA" => "ONA",
        "MUSIC" => "Music",
        "MANGA" => "Manga",
        "NOVEL" => "Light Novel",
        "ONE_SHOT" => "One Shot",
        _ => return None,
    })
}

/// `SEASON_LABELS`.
pub fn season_label(key: &str) -> Option<&'static str> {
    Some(match key {
        "WINTER" => "Winter",
        "SPRING" => "Spring",
        "SUMMER" => "Summer",
        "FALL" => "Fall",
        _ => return None,
    })
}

const ZWSP: &str = "\u{200B}";
const PAUSE_MAX_MS: i64 = 300_000;
const INDEX_MEMO_MS: i64 = 24 * 3600 * 1000;
const INDEX_RETRY_MS: i64 = 10 * 60 * 1000;

pub const ANILIST_QUERY: &str = "query ($search: String, $type: MediaType, $perPage: Int, $isAdult: Boolean) {
  Page(page: 1, perPage: $perPage) {
    media(search: $search, type: $type, isAdult: $isAdult, sort: [SEARCH_MATCH, POPULARITY_DESC]) {
      id siteUrl isAdult type format status episodes duration chapters volumes
      season seasonYear averageScore meanScore genres
      title { romaji english native }
      description(asHtml: false)
      coverImage { extraLarge large }
      startDate { year } endDate { year }
      nextAiringEpisode { airingAt episode }
      studios(isMain: true) { nodes { name } }
    }
  }
}";

/// The sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Api {
    AniList,
    Urban,
    PokeApi,
}

impl Api {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AniList => "anilist",
            Self::Urban => "urban",
            Self::PokeApi => "pokeapi",
        }
    }

    /// `BUDGET_PER_MINUTE`: lookups (not requests) that may go to the network per minute, across every shard.
    pub fn budget_per_minute(self) -> i64 {
        match self {
            Self::AniList => 24,
            Self::Urban => 30,
            Self::PokeApi => 30,
        }
    }

    /// `pauseKey(api)`.
    pub fn pause_key(self) -> String {
        match self {
            Self::AniList => ANILIST_PAUSE_KEY.to_string(),
            other => format!("search:{}:pause", other.as_str()),
        }
    }
}

/// AniList's `MediaType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Anime,
    Manga,
}

impl MediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anime => "ANIME",
            Self::Manga => "MANGA",
        }
    }
}

/// `SearchError.code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchCode {
    RateLimited,
    Cooldown,
    Timeout,
    Unavailable,
}

impl SearchCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "RATE_LIMITED",
            Self::Cooldown => "COOLDOWN",
            Self::Timeout => "TIMEOUT",
            Self::Unavailable => "UNAVAILABLE",
        }
    }
}

/// `SearchError`: `{ code, api, status, retryAt }` (`retryAt` in epoch ms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchError {
    pub code: SearchCode,
    pub api: Option<Api>,
    pub status: Option<u16>,
    pub retry_at: Option<i64>,
}

impl SearchError {
    pub fn new(code: SearchCode, api: Option<Api>) -> Self {
        Self { code, api, status: None, retry_at: None }
    }

    pub fn status(mut self, status: Option<u16>) -> Self {
        self.status = status;
        self
    }

    pub fn retry_at(mut self, at: i64) -> Self {
        self.retry_at = Some(at);
        self
    }
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.api.map(Api::as_str).unwrap_or("search"), self.code.as_str())
    }
}

impl std::error::Error for SearchError {}

// ── The network seam (`deps`) ─────────────────────────────────────────────────

/// `safeFetch` and `safeFetchJson`, replaceable per app (the JS `__setDeps`).
pub trait SearchFetch: Send + Sync + 'static {
    fn fetch(&self, url: String, opts: FetchOptions) -> BoxFuture<Result<FetchResponse, SafeFetchError>>;
    fn fetch_json(&self, url: String, opts: FetchOptions) -> BoxFuture<Result<Value, SafeFetchError>>;
}

/// Production: `bot_util::safe_fetch` (SSRF guard, timeout, size cap).
#[derive(Debug, Default, Clone, Copy)]
pub struct SafeFetchDeps;

impl SearchFetch for SafeFetchDeps {
    fn fetch(&self, url: String, opts: FetchOptions) -> BoxFuture<Result<FetchResponse, SafeFetchError>> {
        Box::pin(async move { bot_util::safe_fetch::safe_fetch(&url, &opts).await })
    }

    fn fetch_json(&self, url: String, opts: FetchOptions) -> BoxFuture<Result<Value, SafeFetchError>> {
        Box::pin(async move { bot_util::safe_fetch::safe_fetch_json(&url, &opts).await })
    }
}

/// `indexMemo`: when it was loaded, and the entries.
type IndexMemo = Option<(i64, Vec<(String, i64)>)>;

type Flight = Shared<BoxFuture<Result<Vec<Value>, SearchError>>>;

/// The in-process state of this module (one per app): `inflight`, `pausedUntil`, `localBudgets`, the Pokémon index
/// memo and the network seam.
pub struct SearchState {
    deps: Mutex<Arc<dyn SearchFetch>>,
    inflight: Mutex<HashMap<String, (u64, Flight)>>,
    next_flight: std::sync::atomic::AtomicU64,
    paused_until: Mutex<HashMap<Api, i64>>,
    local_budgets: Mutex<HashMap<Api, (i64, i64)>>,
    index_memo: Mutex<IndexMemo>,
    index_refresh: std::sync::atomic::AtomicBool,
    index_failed_at: Mutex<i64>,
    warn_throttle: bot_util::async_util::Throttle,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            deps: Mutex::new(Arc::new(SafeFetchDeps)),
            inflight: Mutex::new(HashMap::new()),
            next_flight: std::sync::atomic::AtomicU64::new(1),
            paused_until: Mutex::new(HashMap::new()),
            local_budgets: Mutex::new(HashMap::new()),
            index_memo: Mutex::new(None),
            index_refresh: std::sync::atomic::AtomicBool::new(false),
            index_failed_at: Mutex::new(0),
            warn_throttle: bot_util::async_util::create_throttle(60_000),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn state(app: &App) -> Arc<SearchState> {
    app.ext::<SearchState>()
}

/// Tests only (`__setDeps`): replaces `safeFetch` / `safeFetchJson` for this app.
pub fn set_fetch(app: &App, deps: Arc<dyn SearchFetch>) {
    *lock(&state(app).deps) = deps;
}

fn deps(app: &App) -> Arc<dyn SearchFetch> {
    lock(&state(app).deps).clone()
}

fn now(app: &App) -> i64 {
    app.clock().now_ms()
}

// ── JS value helpers ──────────────────────────────────────────────────────────

/// `String(x ?? '')` for a JSON value.
pub fn js_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
        Some(Value::Array(a)) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(Some(x)) }).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `Number.isInteger(x) ? x : null` for a JSON value.
fn js_int(v: Option<&Value>) -> Option<i64> {
    let n = v?.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    (f.is_finite() && f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15).then_some(f as i64)
}

fn non_neg_int(v: Option<&Value>) -> Option<i64> {
    js_int(v).filter(|n| *n >= 0)
}

fn string_or_null(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

/// JS truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `Number(x)` for a JSON value (`NaN` when it is not a number).
fn js_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = bot_util::js::trim(s);
            if t.is_empty() { 0.0 } else { t.parse::<f64>().unwrap_or(f64::NAN) }
        }
        Some(_) => f64::NAN,
    }
}

/// `Number.parseInt(s, 10)`: leading whitespace, an optional sign, then decimal digits; `None` for NaN.
pub fn js_parse_int(s: Option<&str>) -> Option<i64> {
    let s = bot_util::js::trim_start(s?);
    let (neg, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let n: i64 = digits.parse().unwrap_or(i64::MAX);
    Some(if neg { -n } else { n })
}

fn get<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    v?.as_object()?.get(key).filter(|x| !x.is_null())
}

fn utf16_len(s: &str) -> usize {
    bot_util::js::utf16_len(s)
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

static CONTROL_RUNS: LazyRe = lazy_re!(r"\p{C}+", "gu");
static SPACE_RUNS_U: LazyRe = lazy_re!(r"\s+", "gu");
static ONE_LINE_RUNS: LazyRe = lazy_re!(r"[\p{C}\s]+", "gu");
static MARKS: LazyRe = lazy_re!(r"\p{M}+", "gu");
static QUOTES_DOTS: LazyRe = lazy_re!(r"['’‘`.]", "g");
static NON_SLUG: LazyRe = lazy_re!(r"[^a-z0-9-]+", "g");
static DASH_RUNS: LazyRe = lazy_re!(r"-{2,}", "g");
static EDGE_DASHES: LazyRe = lazy_re!(r"^-+|-+$", "g");
static POKE_NUMBER: LazyRe = lazy_re!(r"^#?\s*(\d{1,5})$", "");

/// `cleanQuery(input)`: single-line, NFKC, control/format characters and runs of whitespace collapsed, at most 100
/// code points.
pub fn clean_query(input: &str) -> String {
    let s: String = input.nfkc().collect();
    let s = CONTROL_RUNS.replace_all(&s, " ");
    let s = SPACE_RUNS_U.replace_all(&s, " ");
    bot_util::js::trim(&s).chars().take(100).collect()
}

/// `cleanQuery` of a JSON value (`String(input ?? '')`).
pub fn clean_query_value(input: Option<&Value>) -> String {
    clean_query(&js_string(input))
}

/// `keyForm(input)`.
pub fn key_form(input: &str) -> String {
    bot_util::js::lower(&clean_query(input))
}

/// `cacheKey(api, source)`: `search:{api}:{sha1 hex}`.
pub fn cache_key(api: Api, source: &str) -> String {
    let digest = Sha1::digest(source.as_bytes());
    format!("search:{}:{}", api.as_str(), hex::encode(digest))
}

/// `readEnvelope(value)`: the results of a `{ v: 1, results: [...] }` cache value.
pub fn read_envelope(value: Option<&Value>) -> Option<Vec<Value>> {
    let o = value?.as_object()?;
    if o.get("v").and_then(Value::as_f64) != Some(1.0) {
        return None;
    }
    o.get("results")?.as_array().cloned()
}

/// `isAgeRestrictedChannel(channel)`: the channel (or, for a thread, its parent) is flagged NSFW. Anything unknown is
/// not. `parent` is the thread's parent channel when known.
pub fn is_age_restricted(channel: Option<&serenity::all::GuildChannel>, parent: Option<&serenity::all::GuildChannel>) -> bool {
    let Some(ch) = channel else { return false };
    if bot_core::permissions::is_thread(ch) {
        return parent.is_some_and(|p| p.nsfw);
    }
    ch.nsfw
}

/// [`is_age_restricted`] for a channel of a cached guild (`interaction.channel`).
pub fn is_age_restricted_channel(app: &App, guild_id: Option<GuildId>, channel_id: ChannelId) -> bool {
    is_age_restricted_channel_with(app, guild_id, channel_id, None)
}

/// [`is_age_restricted_channel`] with the interaction's own channel data. discord.js builds `interaction.channel` from
/// that data when the channel is not cached (an archived or private thread, typically), so a thread the cache lacks
/// still counts as age-restricted when its parent is.
pub fn is_age_restricted_channel_with(app: &App, guild_id: Option<GuildId>, channel_id: ChannelId, partial: Option<&serenity::all::PartialChannel>) -> bool {
    use serenity::all::ChannelType;
    let Some(g) = guild_id else { return false };
    ops::view(app, g, |guild| {
        let ch = bot_core::permissions::find_channel(guild, channel_id);
        if ch.is_none()
            && let Some(p) = partial.filter(|p| p.id == channel_id)
        {
            let is_thread = matches!(p.kind, ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread);
            return is_thread && p.parent_id.and_then(|parent| bot_core::permissions::find_channel(guild, parent)).is_some_and(|c| c.nsfw);
        }
        let parent = ch.and_then(|c| c.parent_id).and_then(|p| bot_core::permissions::find_channel(guild, p));
        is_age_restricted(ch, parent)
    })
    .unwrap_or(false)
}

/// A parsed `/search pokemon` name: a Pokédex number or a slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PokemonQuery {
    Id(i64),
    Name(String),
}

fn slugify(s: &str) -> String {
    let s: String = s.nfd().collect();
    let s = MARKS.replace_all(&s, "");
    let s = s.replace('♀', "-f").replace('♂', "-m");
    let s = QUOTES_DOTS.replace_all(&s, "");
    let s = NON_SLUG.replace_all(&s, "-");
    let s = DASH_RUNS.replace_all(&s, "-");
    EDGE_DASHES.replace_all(&s, "")
}

/// `pokemonSlug(input)`.
pub fn pokemon_slug(input: &str) -> Option<PokemonQuery> {
    let s: String = input.nfkc().collect();
    let s = bot_util::js::lower(bot_util::js::trim(&s));
    if let Some((t, m)) = POKE_NUMBER.exec(&s) {
        let id: i64 = m.group(&t, 1).and_then(|d| d.parse().ok()).unwrap_or(0);
        return (id >= 1).then_some(PokemonQuery::Id(id));
    }
    let s = slugify(&s);
    if s.is_empty() || utf16_len(&s) > 40 {
        return None;
    }
    Some(PokemonQuery::Name(s))
}

/// `titleCase(slug)`.
pub fn title_case(slug: &str) -> String {
    slug.split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), c.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `httpsUrl(u, { hosts })`: the URL when it is https, credential-free, at most 2048 characters and (optionally) on an
/// allowed host.
pub fn https_url(u: Option<&Value>, hosts: Option<&[&str]>) -> Option<String> {
    let s = u?.as_str()?;
    if s.is_empty() || utf16_len(s) > 2048 {
        return None;
    }
    let url = url::Url::parse(s).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    if let Some(h) = hosts
        && !h.contains(&url.host_str().unwrap_or(""))
    {
        return None;
    }
    Some(s.to_string())
}

static EVERYONE: LazyRe = lazy_re!(r"@(everyone|here)\b", "gi");
static TOKEN_START: LazyRe = lazy_re!(r"<(?=@|#|:|a:|t:|id:|\/)", "gi");

/// `defangMentions(text)`: breaks text Discord would render as a mention or token with a zero-width space.
pub fn defang_mentions(text: &str) -> String {
    let s = EVERYONE.replace_all_with(text, |m, t| format!("@{ZWSP}{}", m.group(t, 1).unwrap_or_default()));
    TOKEN_START.replace_all(&s, &format!("<{ZWSP}"))
}

static HEADING: LazyRe = lazy_re!(r"^( {0,2})([*-] )?( *)(#{1,3} )", "gm");
static QUOTE_LINE: LazyRe = lazy_re!(r"^([ \t]*)(>|-#)", "gm");

// The default escapes of `@discordjs/formatters` (the patterns of `bot_core::markdown`), each run only when its marker
// is present: the lookbehinds of the italic and underline patterns scan backwards from every position, which makes a
// long run of text without the marker quadratic.
static MD_INLINE_CODE: LazyRe = lazy_re!(r"(?<=^|[^`])``?(?=[^`]|$)", "g");
static MD_ITALIC_STAR: LazyRe = lazy_re!(r"(?<=^|[^*])(?<!(?<!<)https?:\/\/\S*|<[^\s:]+:\/[^\s>]*)\*([^*]|\*\*|$)", "g");
static MD_ITALIC_UNDERSCORE: LazyRe = lazy_re!(r"(?<=^|[^_])(?<!<a?:.+|(?<!<)https?:\/\/\S*|<[^\s:]:\/[^\s>]*)_(?!:\d+>)([^_]|__|$)", "g");
static MD_BOLD: LazyRe = lazy_re!(r"\*\*(\*)?", "g");
static MD_UNDERLINE: LazyRe = lazy_re!(r"(?<!<a?:.+|https?:\/\/\S+)__(_)?(?!:\d+>)", "g");

/// `escapeMarkdown(text)` with the default options: the same result as [`bot_core::markdown::escape_markdown`], with
/// every step skipped when its marker is absent (that step would change nothing).
pub fn escape_markdown_default(text: &str) -> String {
    let mut res = text.replace('\\', "\\\\");
    if res.contains('`') {
        res = MD_INLINE_CODE.replace_all_with(&res, |m, _| if m.end - m.start == 2 { "\\`\\`".into() } else { "\\`".into() });
        res = res.replace("```", "\\`\\`\\`");
    }
    if res.contains('*') {
        let mut idx = 0u32;
        res = MD_ITALIC_STAR.replace_all_with(&res, |m, t| {
            let g = m.group(t, 1).unwrap_or_default();
            if g == "**" {
                idx += 1;
                if idx % 2 == 1 { format!("\\*{g}") } else { format!("{g}\\*") }
            } else {
                format!("\\*{g}")
            }
        });
    }
    if res.contains('_') {
        let mut idx = 0u32;
        res = MD_ITALIC_UNDERSCORE.replace_all_with(&res, |m, t| {
            let g = m.group(t, 1).unwrap_or_default();
            if g == "__" {
                idx += 1;
                if idx % 2 == 1 { format!("\\_{g}") } else { format!("{g}\\_") }
            } else {
                format!("\\_{g}")
            }
        });
    }
    if res.contains("**") {
        let mut idx = 0u32;
        res = MD_BOLD.replace_all_with(&res, |m, t| match m.group(t, 1) {
            Some(g) => {
                idx += 1;
                if idx % 2 == 1 { format!("{g}\\*\\*") } else { format!("\\*\\*{g}") }
            }
            None => "\\*\\*".into(),
        });
    }
    if res.contains("__") {
        let mut idx = 0u32;
        res = MD_UNDERLINE.replace_all_with(&res, |m, t| match m.group(t, 1) {
            Some(g) => {
                idx += 1;
                if idx % 2 == 1 { format!("{g}\\_\\_") } else { format!("\\_\\_{g}") }
            }
            None => "\\_\\_".into(),
        });
    }
    res = res.replace("~~", "\\~\\~");
    res.replace("||", "\\|\\|")
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `text.replaceAll(/\[.+]\(.+\)/gm, '\\$&')` in one pass. `.+` is greedy and cannot cross a line end, so on a line
/// the only possible match starts at its first `[` and ends at its last `)`; it exists when a `](` with at least one
/// character on each side lies between them.
fn escape_masked_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut line: Vec<char> = Vec::new();
    let flush = |line: &mut Vec<char>, out: &mut String| {
        let at = line.iter().position(|c| *c == '[').filter(|&i0| {
            line.iter().rposition(|c| *c == ')').is_some_and(|m| m >= i0 + 5 && (i0 + 2..=m - 3).any(|k| line[k] == ']' && line[k + 1] == '('))
        });
        for (i, c) in line.iter().enumerate() {
            if Some(i) == at {
                out.push('\\');
            }
            out.push(*c);
        }
        line.clear();
    };
    for c in text.chars() {
        if is_line_terminator(c) {
            flush(&mut line, &mut out);
            out.push(c);
        } else {
            line.push(c);
        }
    }
    flush(&mut line, &mut out);
    out
}

/// `escapeMarkdown(text, { maskedLink: true, heading: true })` (the default escapes, then headings, then masked links).
pub fn escape_markdown_block(text: &str) -> String {
    let s = escape_markdown_default(text);
    let s = HEADING.replace_all_with(&s, |m, t| {
        format!(
            "{}{}{}\\{}",
            m.group(t, 1).unwrap_or_default(),
            m.group(t, 2).unwrap_or_default(),
            m.group(t, 3).unwrap_or_default(),
            m.group(t, 4).unwrap_or_default()
        )
    });
    escape_masked_links(&s)
}

/// `escapeBlock(text)`: no bold, links, headings, spoilers, quotes or subtext can form.
fn escape_block(text: &str) -> String {
    let s = escape_markdown_block(text);
    QUOTE_LINE.replace_all_with(&s, |m, t| format!("{}\\{}", m.group(t, 1).unwrap_or_default(), m.group(t, 2).unwrap_or_default()))
}

fn named_entity(key: &str) -> Option<&'static str> {
    Some(match key {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "rsquo" => "’",
        "lsquo" => "‘",
        "rdquo" => "”",
        "ldquo" => "“",
        _ => return None,
    })
}

static CRLF: LazyRe = lazy_re!(r"\r\n?", "g");
static BR: LazyRe = lazy_re!(r"<br\s*\/?>", "gi");
static TAG: LazyRe = lazy_re!(r"<[^<>]{0,200}>", "g");
static ENTITY: LazyRe = lazy_re!(r"&(#x[0-9a-f]{1,6}|#\d{1,7}|[a-z]{2,8});", "gi");
static NL3: LazyRe = lazy_re!(r"\n{3,}", "g");

/// `s.split(/~!([\s\S]*?)!~/)`: the pieces and the spoiler texts, interleaved. Lazy, so a spoiler ends at the first
/// `!~` after its `~!`; when no `!~` follows a `~!`, none follows a later one either, so the scan stops (one pass).
fn split_spoilers(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut last = 0;
    let mut pos = 0;
    while let Some(i) = s[pos..].find("~!").map(|i| i + pos) {
        let Some(j) = s[i + 2..].find("!~").map(|j| j + i + 2) else { break };
        out.push(s[last..i].to_string());
        out.push(s[i + 2..j].to_string());
        last = j + 2;
        pos = j + 2;
    }
    out.push(s[last..].to_string());
    out
}

/// `s.replace(/[ \t]+\n/g, '\n')` in one pass: a run of spaces and tabs right before a newline is dropped.
fn drop_space_before_newline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    for c in s.chars() {
        match c {
            ' ' | '\t' => run.push(c),
            '\n' => {
                run.clear();
                out.push(c);
            }
            _ => {
                out.push_str(&run);
                run.clear();
                out.push(c);
            }
        }
    }
    out.push_str(&run);
    out
}

/// `stripAniListHtml(text, max)`: an AniList description (HTML-ish, with `~!spoiler!~`) as safe Discord markdown, at
/// most `max` characters.
pub fn strip_anilist_html(text: &str, max: usize) -> String {
    let s = bot_util::js::utf16_prefix(text, max * 10);
    let s = CRLF.replace_all(s, "\n");
    let s = BR.replace_all(&s, "\n");
    let s = TAG.replace_all(&s, "");
    let s = ENTITY.replace_all_with(&s, |m, t| {
        let whole = m.text(t);
        let body = m.group(t, 1).unwrap_or_default();
        if let Some(num) = body.strip_prefix('#') {
            let n = if num.starts_with(['x', 'X']) { u32::from_str_radix(&num[1..], 16).ok() } else { num.parse::<u32>().ok() };
            return match n {
                Some(n) if n > 0 && n <= 0x10ffff && !(0xd800..=0xdfff).contains(&n) => char::from_u32(n).map(String::from).unwrap_or_default(),
                _ => String::new(),
            };
        }
        named_entity(&body.to_ascii_lowercase()).map(str::to_string).unwrap_or(whole)
    });
    let s = defang_mentions(&s);
    let s: String = split_spoilers(&s)
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            if i % 2 == 1 {
                if bot_util::js::trim(seg).is_empty() { String::new() } else { format!("||{}||", escape_block(seg)) }
            } else {
                escape_block(seg)
            }
        })
        .collect();
    let s = drop_space_before_newline(&s);
    let s = NL3.replace_all(&s, "\n\n");
    let mut s = clamp_text(bot_util::js::trim(&s), max.saturating_sub(2));
    if s.matches("||").count() % 2 == 1 {
        s.push_str("||");
    }
    s
}

static URBAN_BRACKET: LazyRe = lazy_re!(r"\[([^[\]\n]{1,100})\]", "g");

/// `stripUrbanBrackets(text)`: `[word]` links become plain words.
pub fn strip_urban_brackets(text: &str) -> String {
    URBAN_BRACKET.replace_all_with(text, |m, t| m.group(t, 1).unwrap_or_default())
}

/// `cleanUrbanText(text, max)`.
pub fn clean_urban_text(text: &str, max: usize) -> String {
    let s = bot_util::js::utf16_prefix(text, max * 8);
    let s = CRLF.replace_all(s, "\n");
    let s = strip_urban_brackets(&s);
    let s = defang_mentions(&s);
    let s = escape_block(&s);
    let s = NL3.replace_all(&s, "\n\n");
    clamp_text(bot_util::js::trim(&s), max)
}

static FLAVOR_WS: LazyRe = lazy_re!(r"[\f\n\r\t\v]+", "g");
static SPACES2: LazyRe = lazy_re!(r" {2,}", "g");

/// `cleanFlavorText(text)`: a Pokédex entry on one line.
pub fn clean_flavor_text(text: &str) -> String {
    let s = text.replace("\u{AD}\n", "").replace('\u{AD}', "");
    let s = FLAVOR_WS.replace_all(&s, " ");
    let s = SPACES2.replace_all(&s, " ");
    let s = s.replace("POKéMON", "Pokémon").replace("POKé BALL", "Poké Ball");
    bot_util::js::trim(&s).to_string()
}

static RETRY_DIGITS: LazyRe = lazy_re!(r"^\s*\d{1,6}\s*$", "");

/// `parseRetryAfter(value, now)`: seconds (1-300) from a Retry-After value (delta seconds or an HTTP date).
pub fn parse_retry_after(value: Option<&str>, now: i64) -> Option<i64> {
    let s = value?;
    let secs = if RETRY_DIGITS.test(s) {
        js_parse_int(Some(s))?
    } else {
        let t = bot_util::js::date_parse(s);
        if !t.is_finite() {
            return None;
        }
        ((t - now as f64) / 1000.0).ceil() as i64
    };
    Some(secs.clamp(1, 300))
}

/// `header(h, name)`: a lookup that ignores case.
fn header<'a>(h: &'a IndexMap<String, String>, name: &str) -> Option<&'a str> {
    h.get(name).or_else(|| h.iter().find(|(k, _)| bot_util::js::lower(k) == name).map(|(_, v)| v)).map(String::as_str)
}

fn reset_seconds_from(reset: Option<i64>, now: i64) -> Option<i64> {
    let reset = reset?;
    if reset.saturating_mul(1000) <= now {
        return None;
    }
    Some(((reset as f64 - now as f64 / 1000.0).ceil() as i64).clamp(1, 300))
}

/// `interpretAniListResponse(...)`'s verdict. `pause_until` (epoch ms) is set whenever the next request would be
/// refused.
#[derive(Debug, Clone, PartialEq)]
pub enum AniListVerdict {
    RateLimited { pause_until: i64 },
    Error { status: u16, pause_until: Option<i64> },
    Ok { media: Vec<Value>, pause_until: Option<i64> },
}

/// `interpretAniListResponse({ status, headers, body }, now)`.
pub fn interpret_anilist_response(status: u16, headers: &IndexMap<String, String>, body: Option<&Value>, now: i64) -> AniListVerdict {
    let remaining = js_parse_int(header(headers, "x-ratelimit-remaining"));
    let reset = js_parse_int(header(headers, "x-ratelimit-reset"));
    let errors = get(body, "errors").and_then(Value::as_array);
    let gql_status = errors.and_then(|e| e.iter().map(|x| js_int(get(Some(x), "status"))).find(Option::is_some).flatten());

    if status == 429 || gql_status == Some(429) {
        let retry = parse_retry_after(header(headers, "retry-after"), now).or_else(|| reset_seconds_from(reset, now)).unwrap_or(60);
        return AniListVerdict::RateLimited { pause_until: now + retry * 1000 };
    }
    let pause_until = (remaining == Some(0)).then(|| now + reset_seconds_from(reset, now).unwrap_or(60) * 1000);
    if !(200..300).contains(&status) {
        return AniListVerdict::Error { status, pause_until };
    }
    if errors.is_some_and(|e| !e.is_empty()) {
        return AniListVerdict::Error { status, pause_until };
    }
    match get(get(get(body, "data"), "Page"), "media").and_then(Value::as_array) {
        Some(media) => AniListVerdict::Ok { media: media.clone(), pause_until },
        None => AniListVerdict::Error { status, pause_until },
    }
}

fn one_line(v: Option<&Value>) -> String {
    let s = js_string(v);
    bot_util::js::trim(&ONE_LINE_RUNS.replace_all(&s, " ")).to_string()
}

/// The next episode of an airing anime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NextEpisode {
    pub episode: i64,
    pub airing_at: i64,
}

/// One projected, cache-safe AniList result (the JS object, key for key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AniListResult {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub url: Option<String>,
    pub title: String,
    #[serde(default)]
    pub alt_titles: Vec<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub episodes: Option<i64>,
    #[serde(default)]
    pub duration: Option<i64>,
    #[serde(default)]
    pub chapters: Option<i64>,
    #[serde(default)]
    pub volumes: Option<i64>,
    #[serde(default)]
    pub season: Option<String>,
    #[serde(default)]
    pub season_year: Option<i64>,
    #[serde(default)]
    pub start_year: Option<i64>,
    #[serde(default)]
    pub end_year: Option<i64>,
    #[serde(default)]
    pub score: Option<i64>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub studios: Vec<String>,
    #[serde(default)]
    pub is_adult: bool,
    #[serde(default)]
    pub synopsis: String,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub next_episode: Option<NextEpisode>,
}

/// `projectAniListResults(media, { type, allowAdult })`: at most [`MAX_RESULTS`] projected objects (adult titles
/// dropped unless allowed).
pub fn project_anilist_results(media: &[Value], kind: MediaType, allow_adult: bool) -> Vec<AniListResult> {
    let mut out = Vec::new();
    for m in media {
        if out.len() >= MAX_RESULTS {
            break;
        }
        let Some(obj) = m.as_object() else { continue };
        let Some(id) = js_int(obj.get("id")) else { continue };
        let is_adult = obj.get("isAdult") == Some(&Value::Bool(true));
        if !allow_adult && is_adult {
            continue;
        }
        let t = get(Some(m), "title");
        let first = [one_line(get(t, "english")), one_line(get(t, "romaji")), one_line(get(t, "native"))].into_iter().find(|s| !s.is_empty());
        let title = clamp_text(&first.unwrap_or_else(|| "Untitled".into()), 256);
        let mut seen: HashSet<String> = HashSet::from([bot_util::js::lower(&title)]);
        let mut alt_titles = Vec::new();
        for alt in [get(t, "romaji"), get(t, "native")] {
            let a = one_line(alt);
            if a.is_empty() || seen.contains(&bot_util::js::lower(&a)) {
                continue;
            }
            seen.insert(bot_util::js::lower(&a));
            alt_titles.push(clamp_text(&a, 120));
        }
        let score_v = match obj.get("averageScore") {
            None | Some(Value::Null) => obj.get("meanScore"),
            v => v,
        };
        let next = get(Some(m), "nextAiringEpisode");
        let (ep, at) = (js_int(get(next, "episode")), js_int(get(next, "airingAt")));
        let genres: Vec<String> = obj
            .get("genres")
            .and_then(Value::as_array)
            .map(|g| g.iter().filter_map(|x| x.as_str().filter(|s| !s.is_empty())).map(|s| clamp_text(&one_line(Some(&json!(s))), 40)).take(20).collect())
            .unwrap_or_default();
        let studios: Vec<String> = get(get(Some(m), "studios"), "nodes")
            .and_then(Value::as_array)
            .map(|n| {
                n.iter()
                    .filter_map(|x| get(Some(x), "name").and_then(Value::as_str).filter(|s| !s.is_empty()))
                    .map(|s| clamp_text(&one_line(Some(&json!(s))), 80))
                    .take(5)
                    .collect()
            })
            .unwrap_or_default();
        let cover = get(Some(m), "coverImage");
        out.push(AniListResult {
            id,
            kind: kind.as_str().to_string(),
            url: https_url(obj.get("siteUrl"), Some(&["anilist.co"])),
            title,
            alt_titles,
            format: string_or_null(obj.get("format")),
            status: string_or_null(obj.get("status")),
            episodes: non_neg_int(obj.get("episodes")),
            duration: non_neg_int(obj.get("duration")),
            chapters: non_neg_int(obj.get("chapters")),
            volumes: non_neg_int(obj.get("volumes")),
            season: string_or_null(obj.get("season")),
            season_year: non_neg_int(obj.get("seasonYear")),
            start_year: non_neg_int(get(get(Some(m), "startDate"), "year")),
            end_year: non_neg_int(get(get(Some(m), "endDate"), "year")),
            score: js_int(score_v).filter(|s| (0..=100).contains(s)),
            genres,
            studios,
            is_adult,
            synopsis: strip_anilist_html(&js_string(obj.get("description")), 1000),
            cover: https_url(get(cover, "extraLarge"), None).or_else(|| https_url(get(cover, "large"), None)),
            next_episode: match (ep, at) {
                (Some(e), Some(a)) if e > 0 && a > 0 => Some(NextEpisode { episode: e, airing_at: a }),
                _ => None,
            },
        });
    }
    out
}

/// A Pokémon ability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ability {
    pub name: String,
    pub hidden: bool,
}

/// A base stat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
    pub key: String,
    pub value: i64,
}

/// `projectPokemon(pokemon, species)`: the small cache-safe object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PokemonInfo {
    pub id: i64,
    pub dex: i64,
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub genus: Option<String>,
    #[serde(default)]
    pub flavor: Option<String>,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub abilities: Vec<Ability>,
    #[serde(default)]
    pub stats: Vec<Stat>,
    #[serde(default)]
    pub height: i64,
    #[serde(default)]
    pub weight: i64,
    #[serde(default)]
    pub sprite: Option<String>,
    #[serde(default)]
    pub generation: Option<String>,
    #[serde(default)]
    pub legendary: bool,
    #[serde(default)]
    pub mythical: bool,
}

fn english_entries<'a>(list: Option<&'a Value>, text_key: &str) -> Vec<&'a Value> {
    list.and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .filter(|e| get(get(Some(e), "language"), "name").and_then(Value::as_str) == Some("en") && e.get(text_key).is_some_and(Value::is_string))
                .collect()
        })
        .unwrap_or_default()
}

static GENERATION: LazyRe = lazy_re!(r"^generation-([a-z0-9]+)$", "");
static SLUG_RE: LazyRe = lazy_re!(r"^[a-z0-9-]{1,60}$", "");

fn sorted_by_slot(list: Option<&Value>) -> Vec<&Value> {
    let mut v: Vec<&Value> = list.and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let slot = |x: &Value| {
        let n = js_number(get(Some(x), "slot"));
        if n.is_nan() { 0.0 } else { n }
    };
    v.sort_by(|a, b| slot(a).partial_cmp(&slot(b)).unwrap_or(std::cmp::Ordering::Equal));
    v
}

/// `projectPokemon(pokemon, species)` (species may be absent). Never fails.
pub fn project_pokemon(pokemon: Option<&Value>, species: Option<&Value>) -> PokemonInfo {
    let p = pokemon.filter(|v| v.is_object() || v.is_array());
    let sp = species.filter(|v| v.is_object() || v.is_array());
    let slug = js_string(get(p, "name"));
    let species_name = get(sp, "name").and_then(Value::as_str).unwrap_or("").to_string();

    let english = english_entries(get(sp, "names"), "name").first().and_then(|e| e.get("name")).and_then(Value::as_str).unwrap_or("").to_string();
    let base = [english, title_case(if species_name.is_empty() { &slug } else { &species_name })]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".into());
    let is_form = sp.is_some() && !species_name.is_empty() && slug != species_name && slug.starts_with(&format!("{species_name}-"));
    let name = if is_form {
        let rest = bot_util::js::utf16_suffix_from(&slug, utf16_len(&species_name) + 1);
        clamp_text(&format!("{base} ({})", title_case(rest)), 200)
    } else {
        clamp_text(&base, 200)
    };

    let types: Vec<String> = sorted_by_slot(get(p, "types"))
        .into_iter()
        .filter_map(|t| get(get(Some(t), "type"), "name").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .map(title_case)
        .collect();
    let abilities: Vec<Ability> = sorted_by_slot(get(p, "abilities"))
        .into_iter()
        .filter_map(|a| {
            let name = get(get(Some(a), "ability"), "name").and_then(Value::as_str).filter(|s| !s.is_empty())?;
            Some(Ability { name: title_case(name), hidden: a.get("is_hidden") == Some(&Value::Bool(true)) })
        })
        .collect();
    let stat_list: Vec<&Value> = get(p, "stats").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let stats = STAT_ORDER
        .iter()
        .map(|key| {
            let v = stat_list.iter().find(|s| get(get(Some(s), "stat"), "name").and_then(Value::as_str) == Some(key)).and_then(|s| non_neg_int(s.get("base_stat")));
            Stat { key: key.to_string(), value: v.unwrap_or(0) }
        })
        .collect();

    let flavor_raw = english_entries(get(sp, "flavor_text_entries"), "flavor_text").last().and_then(|e| e.get("flavor_text")).and_then(Value::as_str).map(str::to_string);
    let flavor = flavor_raw.map(|f| clamp_text(&clean_flavor_text(&f), 600)).unwrap_or_default();
    let gen_name = js_string(get(get(sp, "generation"), "name"));
    let generation = GENERATION.exec(&gen_name).and_then(|(t, m)| m.group(&t, 1));
    let sprites = get(p, "sprites");
    let other = get(sprites, "other");

    PokemonInfo {
        id: js_int(get(p, "id")).unwrap_or(0),
        dex: js_int(get(sp, "id")).or_else(|| js_int(get(p, "id"))).unwrap_or(0),
        name,
        slug,
        genus: english_entries(get(sp, "genera"), "genus").first().and_then(|e| e.get("genus")).and_then(Value::as_str).map(str::to_string),
        flavor: (!flavor.is_empty()).then_some(flavor),
        types,
        abilities,
        stats,
        height: non_neg_int(get(p, "height")).unwrap_or(0),
        weight: non_neg_int(get(p, "weight")).unwrap_or(0),
        sprite: https_url(get(get(other, "official-artwork"), "front_default"), None)
            .or_else(|| https_url(get(get(other, "home"), "front_default"), None))
            .or_else(|| https_url(get(sprites, "front_default"), None)),
        generation: generation.map(|g| format!("Generation {}", g.to_uppercase())),
        legendary: get(sp, "is_legendary") == Some(&Value::Bool(true)),
        mythical: get(sp, "is_mythical") == Some(&Value::Bool(true)),
    }
}

/// One cleaned Urban Dictionary definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UrbanResult {
    pub word: String,
    #[serde(default)]
    pub definition: String,
    #[serde(default)]
    pub example: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub thumbs_up: i64,
    #[serde(default)]
    pub thumbs_down: i64,
    #[serde(default)]
    pub written_on: Option<String>,
}

/// `projectUrban(list)`: at most [`MAX_RESULTS`] cleaned definitions, in the API's order.
pub fn project_urban(list: Option<&Value>) -> Vec<UrbanResult> {
    let mut out = Vec::new();
    let Some(list) = list.and_then(Value::as_array) else { return out };
    for d in list {
        if out.len() >= MAX_RESULTS {
            break;
        }
        let Some(obj) = d.as_object() else { continue };
        let Some(definition) = obj.get("definition").and_then(Value::as_str) else { continue };
        if bot_util::js::trim(definition).is_empty() {
            continue;
        }
        let written = obj.get("written_on").and_then(Value::as_str).map(bot_util::js::date_parse).unwrap_or(f64::NAN);
        let word = clean_query_value(obj.get("word"));
        out.push(UrbanResult {
            word: clamp_text(if word.is_empty() { "Untitled" } else { &word }, 256),
            definition: clean_urban_text(definition, 1800),
            example: clean_urban_text(&js_string(obj.get("example")), 1000),
            author: clamp_text(&clean_query_value(obj.get("author")), 64),
            url: https_url(obj.get("permalink"), Some(&["www.urbandictionary.com", "urbandictionary.com"])),
            thumbs_up: non_neg_int(obj.get("thumbs_up")).unwrap_or(0),
            thumbs_down: non_neg_int(obj.get("thumbs_down")).unwrap_or(0),
            written_on: if written.is_finite() { bot_util::js::to_iso_string(written) } else { None },
        });
    }
    out
}

/// `matchPokemonIndex(entries, query, limit)`: prefix matches first, then substring matches, each in Pokédex order.
pub fn match_pokemon_index(entries: &[(String, i64)], query: &str, limit: usize) -> Vec<Choice> {
    let q: String = query.nfkc().collect();
    let q = bot_util::js::lower(bot_util::js::trim(&q));
    let q = q.strip_prefix('#').unwrap_or(&q).to_string();
    let q = slugify(&q);
    let picked: Vec<&(String, i64)> = if q.is_empty() {
        entries.iter().collect()
    } else if q.bytes().all(|b| b.is_ascii_digit()) {
        entries.iter().filter(|(_, id)| id.to_string().starts_with(&q)).collect()
    } else {
        let mut v: Vec<&(String, i64)> = entries.iter().filter(|(n, _)| n.starts_with(&q)).collect();
        v.extend(entries.iter().filter(|(n, _)| !n.starts_with(&q) && n.contains(&q)));
        v
    };
    picked.into_iter().take(limit).map(|(name, id)| Choice::new(format!("{} (#{id})", title_case(name)), name.clone())).collect()
}

// ── Embeds ────────────────────────────────────────────────────────────────────

fn esc(s: &str) -> String {
    escape_markdown(s)
}

fn status_label(manga: bool, status: Option<&str>) -> &'static str {
    match status {
        Some("FINISHED") => "Finished",
        Some("RELEASING") => {
            if manga {
                "Publishing"
            } else {
                "Airing"
            }
        }
        Some("NOT_YET_RELEASED") => {
            if manga {
                "Not yet published"
            } else {
                "Not yet aired"
            }
        }
        Some("CANCELLED") => "Cancelled",
        Some("HIATUS") => "On hiatus",
        _ => "Unknown",
    }
}

fn year_range(m: &AniListResult) -> String {
    let Some(start) = m.start_year.filter(|y| *y != 0) else { return "—".into() };
    if let Some(end) = m.end_year.filter(|e| *e != 0 && *e != start) {
        return format!("{start}–{end}");
    }
    if m.status.as_deref() == Some("RELEASING") {
        return format!("{start}–present");
    }
    start.to_string()
}

fn dash(v: Option<&str>) -> String {
    match v.filter(|s| !s.is_empty()) {
        Some(s) => clamp_text(s, 1024),
        None => "—".into(),
    }
}

fn join_1024(items: &[String]) -> String {
    join_with_limit(items, ", ", 1024, default_more)
}

/// `buildAniListEmbeds(results, { type, settings, now })`: one embed per result (at most [`MAX_RESULTS`]).
pub fn build_anilist_embeds(results: &[AniListResult], kind: MediaType, settings: Option<&bot_db::GuildSettings>, now_ms: i64) -> Vec<CreateEmbed> {
    results
        .iter()
        .take(MAX_RESULTS)
        .map(|m| {
            let manga = if m.kind.is_empty() { kind == MediaType::Manga } else { m.kind == "MANGA" };
            let alt = if m.alt_titles.is_empty() { String::new() } else { format!("*{}*", escape_markdown(&m.alt_titles.join(" · "))) };
            let synopsis = if m.synopsis.is_empty() { "*No synopsis available.*".to_string() } else { m.synopsis.clone() };
            let description = clamp_text(&[alt, synopsis].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n"), 4096);
            let footer = format!("Source: AniList{}", if m.is_adult { " · 🔞 18+" } else { "" });
            let mut e = guild_embed(settings, Some(&esc(&m.title)), Some(&description), Some(&footer));
            if let Some(u) = &m.url {
                e = e.url(u);
            }
            if let Some(c) = &m.cover {
                e = e.thumbnail(c);
            }
            let score = m.score.map(|s| format!("{s}%")).unwrap_or_else(|| "—".into());
            let mut fields: Vec<(String, String, bool)> = vec![("⭐ Score".into(), score, true)];
            let fmt = m.format.as_deref().and_then(format_label);
            if manga {
                fields.extend([
                    ("📖 Format".into(), dash(fmt), true),
                    ("📡 Status".into(), status_label(true, m.status.as_deref()).into(), true),
                    ("📄 Chapters".into(), m.chapters.map(|n| n.to_string()).unwrap_or_else(|| "?".into()), true),
                    ("📚 Volumes".into(), m.volumes.map(|n| n.to_string()).unwrap_or_else(|| "?".into()), true),
                    ("🗓️ Published".into(), year_range(m), true),
                ]);
            } else {
                let mut episodes = format!(
                    "{}{}",
                    m.episodes.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                    m.duration.filter(|d| *d != 0).map(|d| format!(" × {d} min")).unwrap_or_default()
                );
                if let Some(next) = &m.next_episode
                    && next.airing_at.saturating_mul(1000) > now_ms
                {
                    episodes.push_str(&format!("\nEp {} {}", next.episode, bot_util::time::discord_timestamp_ms(next.airing_at as f64 * 1000.0, "R")));
                }
                let season = match (m.season.as_deref().and_then(season_label), m.season_year.filter(|y| *y != 0)) {
                    (Some(l), Some(y)) => format!("{l} {y}"),
                    _ => year_range(m),
                };
                let studios: Vec<String> = m.studios.iter().map(|s| esc(s)).collect();
                fields.extend([
                    ("📺 Format".into(), dash(fmt), true),
                    ("📡 Status".into(), status_label(false, m.status.as_deref()).into(), true),
                    ("🎞️ Episodes".into(), clamp_text(&episodes, 1024), true),
                    ("🗓️ Season".into(), season, true),
                    ("🏢 Studio".into(), dash(Some(&join_1024(&studios))), true),
                ]);
            }
            let genres: Vec<String> = m.genres.iter().map(|g| esc(g)).collect();
            fields.push(("🏷️ Genres".into(), dash(Some(&join_1024(&genres))), false));
            for (name, value, inline) in fields {
                e = e.field(name, value, inline);
            }
            e
        })
        .collect()
}

/// `buildPokemonEmbed(info, settings)`.
pub fn build_pokemon_embed(info: &PokemonInfo, settings: Option<&bot_db::GuildSettings>) -> CreateEmbed {
    let parts: Vec<String> =
        [info.genus.as_deref().filter(|g| !g.is_empty()).map(|g| format!("*{}*", esc(g))), info.flavor.as_deref().filter(|f| !f.is_empty()).map(esc)]
            .into_iter()
            .flatten()
            .collect();
    let description = if parts.is_empty() { "*No Pokédex entry available.*".to_string() } else { parts.join("\n\n") };
    let tag = if info.legendary {
        Some("Legendary")
    } else if info.mythical {
        Some("Mythical")
    } else {
        None
    };
    let footer = [Some("Source: PokeAPI"), info.generation.as_deref().filter(|g| !g.is_empty()), tag].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let title = clamp_text(&format!("{} · #{}", esc(&info.name), info.dex), 256);
    let mut e = guild_embed(settings, Some(&title), Some(&description), Some(&footer));
    if let Some(s) = &info.sprite {
        e = e.thumbnail(s);
    }
    let sum: i64 = info.stats.iter().map(|s| s.value).sum();
    let pad_end = |s: &str, n: usize| format!("{s}{}", " ".repeat(n.saturating_sub(utf16_len(s))));
    let pad_start = |s: &str, n: usize| format!("{}{s}", " ".repeat(n.saturating_sub(utf16_len(s))));
    let lines: Vec<String> = info
        .stats
        .iter()
        .map(|s| {
            let label = stat_label(&s.key).unwrap_or(&s.key);
            format!("{} {} {}", pad_end(label, 7), pad_start(&s.value.to_string(), 3), progress_bar((s.value as f64 / 255.0).min(1.0), 10.0, "▰", "▱"))
        })
        .collect();
    let block = format!("```\n{}\n{} {}\n```", lines.join("\n"), pad_end("Total", 7), pad_start(&sum.to_string(), 3));
    let types = if info.types.is_empty() { "—".to_string() } else { info.types.join(" / ") };
    let abilities: Vec<String> = info.abilities.iter().map(|a| if a.hidden { format!("{} *(hidden)*", a.name) } else { a.name.clone() }).collect();
    let abilities = if abilities.is_empty() { "—".to_string() } else { abilities.join(", ") };
    e.field("Type", clamp_text(&types, 1024), true)
        .field("Abilities", clamp_text(&abilities, 1024), true)
        .field("Height", format!("{:.1} m", info.height as f64 / 10.0), true)
        .field("Weight", format!("{:.1} kg", info.weight as f64 / 10.0), true)
        .field("Base stats", block, false)
}

/// `buildUrbanEmbeds(results, { settings })`: one embed per definition (at most [`MAX_RESULTS`]).
pub fn build_urban_embeds(results: &[UrbanResult], settings: Option<&bot_db::GuildSettings>) -> Vec<CreateEmbed> {
    results
        .iter()
        .take(MAX_RESULTS)
        .map(|d| {
            let footer = format!("Urban Dictionary · by {}", if d.author.is_empty() { "anonymous" } else { &d.author });
            let mut e = guild_embed(settings, Some(&esc(&d.word)), Some(&d.definition), Some(&footer));
            if let Some(u) = &d.url {
                e = e.url(u);
            }
            if !d.example.is_empty() {
                e = e.field("Example", clamp_text(&d.example, 1024), false);
            }
            if d.thumbs_up + d.thumbs_down > 0 {
                e = e.field("Votes", format!("👍 {} · 👎 {}", format_number(d.thumbs_up as f64), format_number(d.thumbs_down as f64)), true);
            }
            if let Some(ts) = d.written_on.as_deref().and_then(|w| Timestamp::parse(w).ok()) {
                e = e.timestamp(ts);
            }
            e
        })
        .collect()
}

/// What a "No Results" embed is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoResultsKind {
    Anime,
    Manga,
    Pokemon,
    Urban,
}

/// `noResultsEmbed(kind, query, { allowAdult })`.
pub fn no_results_embed(kind: NoResultsKind, query: &str, allow_adult: bool) -> CreateEmbed {
    let q = escape_markdown(&defang_mentions(&clamp_text(&clean_query(query), 100)));
    match kind {
        NoResultsKind::Pokemon => warn_embed("No Results", Some(&format!("No Pokémon found for **{q}**. Check the spelling or try its Pokédex number.")), None),
        NoResultsKind::Urban => warn_embed("No Results", Some(&format!("Urban Dictionary has no definition for **{q}**.")), None),
        _ => {
            let hint = if allow_adult { "" } else { "\nAdult titles are only shown in age-restricted channels." };
            let what = if kind == NoResultsKind::Manga { "manga" } else { "anime" };
            warn_embed("No Results", Some(&format!("No {what} found for **{q}**.{hint}")), None)
        }
    }
}

/// `searchErrorEmbed(err, label)`; `None` is any error that is not a `SearchError`.
pub fn search_error_embed(err: Option<&SearchError>, label: &str) -> CreateEmbed {
    let ts = |ms: i64| bot_util::time::discord_timestamp_ms(ms as f64, "R");
    if let Some(e) = err {
        match e.code {
            SearchCode::RateLimited => {
                let when = e.retry_at.map(ts).unwrap_or_else(|| "in a minute".into());
                return warn_embed("Slow Down", Some(&format!("{label} is limiting how often we can search. Try again {when}.")), None);
            }
            SearchCode::Cooldown => {
                let text = match e.retry_at {
                    Some(at) => format!("Searches from you or this server are arriving too fast. Try again {}.", ts(at)),
                    None => "You are searching too fast. Wait a few seconds and try again.".to_string(),
                };
                return warn_embed("Slow Down", Some(&text), None);
            }
            SearchCode::Timeout => {
                return error_embed("Search Timed Out", Some(&format!("{label} did not answer within 8 seconds. Please try again later.")), None);
            }
            SearchCode::Unavailable => {
                let status = e.status.map(|s| format!(" (HTTP {s})")).unwrap_or_default();
                return error_embed("Search Unavailable", Some(&format!("{label} is not responding right now{status}. Please try again later.")), None);
            }
        }
    }
    error_embed("Search Failed", Some("Something went wrong while searching. Please try again later."), None)
}

// ── Pause, budget, cooldown, cache (internal) ─────────────────────────────────

/// `assertNotPaused(api)`: RATE_LIMITED while the source is paused. The shared pause is fail-open.
async fn assert_not_paused(app: &App, api: Api) -> Result<(), SearchError> {
    let now = now(app);
    let st = state(app);
    let local = lock(&st.paused_until).get(&api).copied().unwrap_or(0);
    if local > now {
        return Err(SearchError::new(SearchCode::RateLimited, Some(api)).retry_at(local));
    }
    if let Some(shared) = app.cache().get(&api.pause_key()).await.and_then(|v| v.as_f64()).filter(|f| f.is_finite()) {
        let shared_ms = shared as i64;
        if shared > now as f64 && shared <= (now + PAUSE_MAX_MS) as f64 {
            lock(&st.paused_until).insert(api, shared_ms);
            return Err(SearchError::new(SearchCode::RateLimited, Some(api)).retry_at(shared_ms));
        }
    }
    Ok(())
}

/// `setPause(api, untilMs)`: never longer than 5 minutes, never shortened; shared through Redis (fail-open).
async fn set_pause(app: &App, api: Api, until_ms: i64) {
    let now = now(app);
    let clamped = until_ms.min(now + PAUSE_MAX_MS);
    {
        let st = state(app);
        let mut map = lock(&st.paused_until);
        if clamped <= map.get(&api).copied().unwrap_or(0) {
            return;
        }
        map.insert(api, clamped);
    }
    let ttl = (((clamped - now) as f64 / 1000.0).ceil() as i64).max(1);
    app.cache().set(&api.pause_key(), &clamped, ttl).await;
}

/// `consumeBudget(api, { userId, guildId })`: the per-user and per-server minute slices (COOLDOWN), then the bot-wide
/// count (RATE_LIMITED; an in-process count while Redis is down).
async fn consume_budget(app: &App, api: Api, user_id: Option<&str>, guild_id: Option<&str>) -> Result<(), SearchError> {
    let limit = api.budget_per_minute();
    let now = now(app);
    let bucket = now.div_euclid(60_000);
    let retry_at = (bucket + 1) * 60_000;
    let guild_slice = (limit / bot_config::limits::SEARCH_GUILD_BUDGET_DIVISOR as i64).max(1);
    let slices = [
        (user_id.filter(|u| !u.is_empty()).map(|u| format!("u:{u}")), bot_config::limits::SEARCH_USER_PER_MINUTE as i64),
        (guild_id.filter(|g| !g.is_empty()).map(|g| format!("g:{g}")), guild_slice),
    ];
    for (slice, max) in slices {
        let Some(slice) = slice else { continue };
        let used = app.cache().incr(&format!("search:budget:{}:{slice}:{bucket}", api.as_str()), 120).await;
        if used.is_some_and(|u| u > max) {
            return Err(SearchError::new(SearchCode::Cooldown, Some(api)).retry_at(retry_at));
        }
    }
    let n = match app.cache().incr(&format!("search:budget:{}:{bucket}", api.as_str()), 120).await {
        Some(n) => n,
        None => {
            let st = state(app);
            let mut map = lock(&st.local_budgets);
            let n = match map.get(&api) {
                Some((b, n)) if *b == bucket => n + 1,
                _ => 1,
            };
            map.insert(api, (bucket, n));
            n
        }
    };
    if n > limit {
        return Err(SearchError::new(SearchCode::RateLimited, Some(api)).retry_at(retry_at));
    }
    Ok(())
}

/// `toSearchError(err, api)`: a 429 also pauses the source (Retry-After when sent, else 60 s).
async fn to_search_error(app: &App, err: &SafeFetchError, api: Api) -> SearchError {
    match err.code {
        SafeFetchCode::Timeout => SearchError::new(SearchCode::Timeout, Some(api)),
        SafeFetchCode::HttpError => {
            if err.status == Some(429) {
                let wait = match err.retry_after_ms.filter(|f| f.is_finite()) {
                    Some(ms) => (ms.max(1000.0).min(PAUSE_MAX_MS as f64)) as i64,
                    None => 60_000,
                };
                let retry_at = now(app) + wait;
                set_pause(app, api, retry_at).await;
                return SearchError::new(SearchCode::RateLimited, Some(api)).status(Some(429)).retry_at(retry_at);
            }
            // A 2xx here means "not valid JSON", which is not worth showing as an HTTP status.
            SearchError::new(SearchCode::Unavailable, Some(api)).status(err.status.filter(|s| *s >= 400))
        }
        _ => SearchError::new(SearchCode::Unavailable, Some(api)),
    }
}

/// The results of a lookup, and whether they came from the cache.
#[derive(Debug, Clone, PartialEq)]
pub struct Lookup<T> {
    pub results: Vec<T>,
    pub cached: bool,
}

fn decode<T: serde::de::DeserializeOwned>(results: Vec<Value>) -> Option<Vec<T>> {
    serde_json::from_value(Value::Array(results)).ok()
}

fn encode<T: Serialize>(results: &[T]) -> Vec<Value> {
    results.iter().filter_map(|r| serde_json::to_value(r).ok()).collect()
}

/// `cachedLookup(...)`: cache -> single-flight -> pause -> user cooldown -> budget -> fetch. Errors are never
/// cached; empty results are cached for an hour. A cached value Rust cannot read is a miss.
#[allow(clippy::too_many_arguments)]
async fn cached_lookup<T, F>(app: &App, api: Api, source: &str, ttl: u64, user_id: Option<&str>, guild_id: Option<&str>, fetcher: F) -> Result<Lookup<T>, SearchError>
where
    T: Serialize + serde::de::DeserializeOwned,
    F: std::future::Future<Output = Result<Vec<T>, SearchError>> + Send + 'static,
{
    let key = cache_key(api, source);
    if let Some(hit) = read_envelope(app.cache().get(&key).await.as_ref()).and_then(decode::<T>) {
        return Ok(Lookup { results: hit, cached: true });
    }
    let st = state(app);
    let running = lock(&st.inflight).get(&key).map(|(_, f)| f.clone());
    if let Some(f) = running {
        return Ok(Lookup { results: decode(f.await?).unwrap_or_default(), cached: false });
    }

    assert_not_paused(app, api).await?;
    if let Some(u) = user_id.filter(|u| !u.is_empty())
        && app.cache().set_nx(&format!("search:cd:{u}"), &1, FETCH_COOLDOWN_SECONDS).await == Some(false)
    {
        return Err(SearchError::new(SearchCode::Cooldown, Some(api)));
    }
    consume_budget(app, api, user_id, guild_id).await?;

    // The awaits above let another user start the same lookup; join it instead of fetching twice. From here to the
    // insert nothing yields, so exactly one flight exists per key.
    let raced = lock(&st.inflight).get(&key).map(|(_, f)| f.clone());
    if let Some(f) = raced {
        return Ok(Lookup { results: decode(f.await?).unwrap_or_default(), cached: false });
    }
    let flight = {
        let mut map = lock(&st.inflight);
        let (app2, key2) = (app.clone(), key.clone());
        let id = st.next_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let fut: BoxFuture<Result<Vec<Value>, SearchError>> = Box::pin(async move {
            let out = async {
                let results = encode(&fetcher.await?);
                let envelope = json!({ "v": 1, "results": results });
                app2.cache().set(&key2, &envelope, if results.is_empty() { cache_ttl::NEGATIVE } else { ttl }).await;
                Ok(results)
            }
            .await;
            // `finally`: whoever drives the flight to its end removes it.
            let st = state(&app2);
            let mut map = lock(&st.inflight);
            if map.get(&key2).is_some_and(|(i, _)| *i == id) {
                map.remove(&key2);
            }
            out
        });
        let shared = fut.shared();
        map.insert(key.clone(), (id, shared.clone()));
        shared
    };
    Ok(Lookup { results: decode(flight.await?).unwrap_or_default(), cached: false })
}

// ── AniList ───────────────────────────────────────────────────────────────────

/// The AniList request body (`JSON.stringify({ query, variables })`). Age-restricted channels omit `isAdult`
/// entirely, never null.
pub fn anilist_request_body(kind: MediaType, query: &str, allow_adult: bool) -> String {
    let mut variables = json!({ "search": query, "type": kind.as_str(), "perPage": MAX_RESULTS });
    if !allow_adult {
        variables["isAdult"] = json!(false);
    }
    json!({ "query": ANILIST_QUERY, "variables": variables }).to_string()
}

/// The options of the AniList POST.
pub fn anilist_fetch_options(body: String) -> FetchOptions {
    FetchOptions {
        method: "POST".into(),
        headers: vec![("Content-Type".into(), "application/json".into()), ("Accept".into(), "application/json".into())],
        body: Some(Body::Text(body)),
        timeout_ms: SEARCH_TIMEOUT_MS,
        max_bytes: max_bytes::ANILIST,
        ..FetchOptions::default()
    }
}

/// `searchAniList(type, query, { allowAdult, userId, guildId })`.
pub async fn search_anilist(
    app: &App,
    kind: MediaType,
    query: &str,
    allow_adult: bool,
    user_id: Option<&str>,
    guild_id: Option<&str>,
) -> Result<Lookup<AniListResult>, SearchError> {
    let q = clean_query(query);
    if q.is_empty() {
        return Ok(Lookup { results: Vec::new(), cached: false });
    }
    let source = format!("{}|{}|{}", bot_util::js::lower(kind.as_str()), if allow_adult { "nsfw" } else { "sfw" }, key_form(&q));
    let body = anilist_request_body(kind, &q, allow_adult);
    let app2 = app.clone();
    let fetcher = async move {
        let res = match deps(&app2).fetch(ANILIST_URL.to_string(), anilist_fetch_options(body)).await {
            Ok(r) => r,
            Err(e) => return Err(to_search_error(&app2, &e, Api::AniList).await),
        };
        let parsed = res.json().ok();
        let verdict = interpret_anilist_response(res.status, &res.headers, parsed.as_ref(), now(&app2));
        let pause = match &verdict {
            AniListVerdict::RateLimited { pause_until } => Some(*pause_until),
            AniListVerdict::Error { pause_until, .. } | AniListVerdict::Ok { pause_until, .. } => *pause_until,
        };
        if let Some(p) = pause {
            set_pause(&app2, Api::AniList, p).await;
        }
        match verdict {
            AniListVerdict::RateLimited { pause_until } => {
                Err(SearchError::new(SearchCode::RateLimited, Some(Api::AniList)).status(Some(429)).retry_at(pause_until))
            }
            // A GraphQL error arrives with HTTP 200; "(HTTP 200)" would only confuse people.
            AniListVerdict::Error { status, .. } => Err(SearchError::new(SearchCode::Unavailable, Some(Api::AniList)).status(Some(status).filter(|s| *s >= 400))),
            AniListVerdict::Ok { media, .. } => Ok(project_anilist_results(&media, kind, allow_adult)),
        }
    };
    cached_lookup(app, Api::AniList, &source, cache_ttl::ANILIST, user_id, guild_id, fetcher).await
}

/// `getJson(url, opts)`: one retry for a GET that failed before any answer came back (NETWORK, DNS_FAILED).
async fn get_json(app: &App, url: &str, opts: FetchOptions) -> Result<Value, SafeFetchError> {
    let d = deps(app);
    match d.fetch_json(url.to_string(), opts.clone()).await {
        Ok(v) => Ok(v),
        Err(e) if matches!(e.code, SafeFetchCode::Network | SafeFetchCode::DnsFailed) => {
            bot_util::log_debug!(format!("search: retrying {url} after {}", e.message));
            d.fetch_json(url.to_string(), opts).await
        }
        Err(e) => Err(e),
    }
}

/// The options of a PokeAPI GET.
pub fn pokeapi_fetch_options() -> FetchOptions {
    FetchOptions {
        timeout_ms: SEARCH_TIMEOUT_MS,
        max_bytes: max_bytes::POKEAPI,
        headers: vec![("Accept".into(), "application/json".into())],
        ..FetchOptions::default()
    }
}

/// `pokeGet(path)`: the parsed JSON, or `None` on a 404.
async fn poke_get(app: &App, path: &str) -> Result<Option<Value>, SearchError> {
    match get_json(app, &format!("{POKEAPI_BASE}{path}"), pokeapi_fetch_options()).await {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code == SafeFetchCode::HttpError && e.status == Some(404) => Ok(None),
        Err(e) => Err(to_search_error(app, &e, Api::PokeApi).await),
    }
}

/// `getPokemon(parsed, { userId, guildId })`: `results` is empty when nothing matches.
pub async fn get_pokemon(app: &App, parsed: &PokemonQuery, user_id: Option<&str>, guild_id: Option<&str>) -> Result<Lookup<PokemonInfo>, SearchError> {
    let (x, source) = match parsed {
        PokemonQuery::Id(id) => (id.to_string(), format!("pokemon|#{id}")),
        PokemonQuery::Name(slug) => (slug.clone(), format!("pokemon|{slug}")),
    };
    let app2 = app.clone();
    let fetcher = async move {
        let enc = bot_util::js::encode_uri_component(&x);
        let mut species = poke_get(&app2, &format!("/pokemon-species/{enc}")).await?;
        let mut pokemon = None;
        if truthy(species.as_ref()) {
            // Species whose default form has a suffix (deoxys -> deoxys-normal): URLs are built from names, never
            // followed.
            let def = species
                .as_ref()
                .and_then(|s| s.get("varieties"))
                .and_then(Value::as_array)
                .and_then(|v| v.iter().find(|x| truthy(get(Some(x), "is_default"))))
                .and_then(|v| get(get(Some(v), "pokemon"), "name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(name) = def.filter(|n| SLUG_RE.test(n)) {
                pokemon = poke_get(&app2, &format!("/pokemon/{name}")).await?;
            }
        } else {
            // Forms and form ids: charizard-mega-x, 10034.
            pokemon = poke_get(&app2, &format!("/pokemon/{enc}")).await?;
            let species_name = get(get(pokemon.as_ref(), "species"), "name").and_then(Value::as_str).map(str::to_string);
            if let Some(name) = species_name.filter(|n| SLUG_RE.test(n)) {
                species = poke_get(&app2, &format!("/pokemon-species/{name}")).await?;
            }
        }
        Ok(if truthy(pokemon.as_ref()) { vec![project_pokemon(pokemon.as_ref(), species.as_ref())] } else { Vec::new() })
    };
    cached_lookup(app, Api::PokeApi, &source, cache_ttl::POKEAPI, user_id, guild_id, fetcher).await
}

// ── Pokémon name index (autocomplete only) ────────────────────────────────────

static INDEX_URL_ID: LazyRe = lazy_re!(r"\/pokemon-species\/(\d+)\/?$", "");
static INDEX_NAME: LazyRe = lazy_re!(r"^[a-z0-9-]{1,40}$", "");

/// The species index from a `/pokemon-species?limit=2000` body, in Pokédex order.
pub fn index_entries(body: &Value) -> Vec<(String, i64)> {
    let mut entries = Vec::new();
    for r in body.get("results").and_then(Value::as_array).into_iter().flatten() {
        let url = js_string(get(Some(r), "url"));
        let id = INDEX_URL_ID.exec(&url).and_then(|(t, m)| m.group(&t, 1)).and_then(|d| d.parse::<i64>().ok());
        if let (Some(name), Some(id)) = (r.get("name").and_then(Value::as_str), id)
            && INDEX_NAME.test(name)
            && id > 0
        {
            entries.push((name.to_string(), id));
        }
    }
    entries.sort_by_key(|(_, id)| *id);
    entries
}

async fn refresh_pokemon_index(app: &App) -> Result<(), String> {
    let result: Result<(), String> = async {
        assert_not_paused(app, Api::PokeApi).await.map_err(|e| e.to_string())?;
        let opts = FetchOptions { timeout_ms: SEARCH_TIMEOUT_MS, max_bytes: max_bytes::POKE_INDEX, ..FetchOptions::default() };
        let body = match get_json(app, &format!("{POKEAPI_BASE}/pokemon-species?limit=2000"), opts).await {
            Ok(b) => b,
            // A 429 pauses PokeAPI for every lookup.
            Err(e) => return Err(to_search_error(app, &e, Api::PokeApi).await.to_string()),
        };
        let entries = index_entries(&body);
        if entries.is_empty() {
            return Err("empty Pokémon index".into());
        }
        *lock(&state(app).index_memo) = Some((now(app), entries.clone()));
        app.cache().set(POKEAPI_INDEX_KEY, &json!({ "v": 1, "entries": entries }), cache_ttl::POKEAPI).await;
        Ok(())
    }
    .await;
    if let Err(e) = &result {
        let st = state(app);
        *lock(&st.index_failed_at) = now(app);
        if st.warn_throttle.allow("pokeindex") {
            bot_util::log_warn!("search: Pokémon index refresh failed", bot_util::logger::LogArg::Error { message: e.clone(), stack: None });
        }
    }
    result
}

/// `getPokemonIndex()`: the species index, or `None` when it is not loaded yet. Never fails and never waits for the
/// network (a refresh runs in the background).
pub async fn get_pokemon_index(app: &App) -> Option<Vec<(String, i64)>> {
    let st = state(app);
    let now = now(app);
    if let Some((at, entries)) = lock(&st.index_memo).as_ref()
        && now - at < INDEX_MEMO_MS
    {
        return Some(entries.clone());
    }
    if let Some(v) = app.cache().get(POKEAPI_INDEX_KEY).await
        && v.get("v").and_then(Value::as_f64) == Some(1.0)
        && let Some(entries) = v.get("entries").and_then(|e| serde_json::from_value::<Vec<(String, i64)>>(e.clone()).ok())
        && !entries.is_empty()
    {
        *lock(&st.index_memo) = Some((now, entries.clone()));
        return Some(entries);
    }
    let failed_at = *lock(&st.index_failed_at);
    if now - failed_at > INDEX_RETRY_MS && !st.index_refresh.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let app2 = app.clone();
        tokio::spawn(async move {
            let _ = refresh_pokemon_index(&app2).await;
            state(&app2).index_refresh.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }
    lock(&st.index_memo).as_ref().map(|(_, e)| e.clone())
}

// ── Urban Dictionary ──────────────────────────────────────────────────────────

/// The options of the Urban Dictionary GET.
pub fn urban_fetch_options() -> FetchOptions {
    FetchOptions {
        timeout_ms: SEARCH_TIMEOUT_MS,
        max_bytes: max_bytes::URBAN,
        headers: vec![("Accept".into(), "application/json".into())],
        ..FetchOptions::default()
    }
}

/// `defineUrban(term, { userId, guildId })`.
pub async fn define_urban(app: &App, term: &str, user_id: Option<&str>, guild_id: Option<&str>) -> Result<Lookup<UrbanResult>, SearchError> {
    let t = clean_query(term);
    if t.is_empty() {
        return Ok(Lookup { results: Vec::new(), cached: false });
    }
    let source = format!("define|{}", key_form(&t));
    let url = format!("{URBAN_DEFINE_URL}?term={}", bot_util::js::encode_uri_component(&t));
    let app2 = app.clone();
    let fetcher = async move {
        let body = match get_json(&app2, &url, urban_fetch_options()).await {
            Ok(b) => b,
            Err(e) => return Err(to_search_error(&app2, &e, Api::Urban).await),
        };
        match body.get("list").filter(|l| l.is_array()) {
            Some(list) => Ok(project_urban(Some(list))),
            None => Err(SearchError::new(SearchCode::Unavailable, Some(Api::Urban))),
        }
    };
    cached_lookup(app, Api::Urban, &source, cache_ttl::URBAN, user_id, guild_id, fetcher).await
}

// ── Command flows ─────────────────────────────────────────────────────────────

/// `replyFailure(interaction, embed)`: removes the public "thinking..." placeholder and posts the failure privately.
async fn reply_failure(ctx: &CommandCtx, embed: CreateEmbed) {
    let private = Payload::embed(embed.clone()).ephemeral(true).no_mentions();
    let err = match ctx.delete_reply().await {
        Ok(()) => match ctx.follow_up(private.to_follow_up()).await {
            Ok(_) => return,
            Err(e) => e,
        },
        Err(e) => e,
    };
    if is_interaction_gone(&err) {
        return;
    }
    bot_util::log_warn!(
        "search: could not send ephemeral failure, editing the reply instead",
        bot_util::logger::LogArg::Error { message: err.to_string(), stack: None }
    );
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
}

async fn send_pages(ctx: &CommandCtx, pages: &[CreateEmbed]) {
    let opts = PaginateOptions { timeout_ms: 120_000.0, footer: FooterMode::Append, ..PaginateOptions::default() };
    if let Err(e) = paginate(ctx, pages, opts).await {
        bot_util::log_error!("search: paginate failed", bot_util::logger::LogArg::Error { message: e.to_string(), stack: None });
    }
}

async fn fail_with(ctx: &CommandCtx, err: &SearchError, api: Api, label: &str) {
    if err.code != SearchCode::Cooldown && state(&ctx.app).warn_throttle.allow(api.as_str()) {
        let status = err.status.map(|s| format!(" (HTTP {s})")).unwrap_or_default();
        bot_util::log_warn!(format!("search: {} {}{status}", api.as_str(), err.code.as_str()));
    }
    reply_failure(ctx, search_error_embed(Some(err), label)).await;
}

/// `settingsBeforeAck(interaction)`: the memo, else a load bounded by the acknowledgement budget; `None` when
/// unavailable.
async fn settings_before_ack(ctx: &CommandCtx) -> Option<Settings> {
    let g = ctx.guild_id()?.to_string();
    if let Some(hit) = ctx.app.settings().peek_settings(&g) {
        return Some(hit);
    }
    let app = ctx.app.clone();
    let load = async move { Ok::<_, std::convert::Infallible>(app.settings().get_settings_safe(&g).await) };
    bot_util::async_util::with_timeout(load, ctx.ack_budget_ms(), "search settings").await.ok().flatten()
}

fn ids(ctx: &CommandCtx) -> (String, Option<String>) {
    (ctx.user().id.to_string(), ctx.guild_id().map(|g| g.to_string()))
}

/// `/search anime` and `/search manga` (`executeAniList`).
pub async fn execute_anilist(ctx: &CommandCtx, kind: MediaType) -> BotResult {
    let query = clean_query(&ctx.get_string("query").unwrap_or_default());
    if query.is_empty() {
        reply_error(&ctx.responder, "Invalid Search", Some("Type a title to search for."), true).await;
        return Ok(());
    }
    let settings = settings_before_ack(ctx).await;
    let allow_adult = is_age_restricted_channel_with(&ctx.app, ctx.guild_id(), ctx.channel_id(), ctx.interaction.channel.as_ref());
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let (user, guild) = ids(ctx);
    let results = match search_anilist(&ctx.app, kind, &query, allow_adult, Some(&user), guild.as_deref()).await {
        Ok(l) => l.results,
        Err(e) => {
            fail_with(ctx, &e, Api::AniList, "AniList").await;
            return Ok(());
        }
    };
    if results.is_empty() {
        let k = if kind == MediaType::Manga { NoResultsKind::Manga } else { NoResultsKind::Anime };
        reply_failure(ctx, no_results_embed(k, &query, allow_adult)).await;
        return Ok(());
    }
    let pages = build_anilist_embeds(&results, kind, settings.as_deref(), now(&ctx.app));
    send_pages(ctx, &pages).await;
    Ok(())
}

/// `/search pokemon` (`executePokemon`).
pub async fn execute_pokemon(ctx: &CommandCtx) -> BotResult {
    let raw = ctx.get_string("name").unwrap_or_default();
    let Some(parsed) = pokemon_slug(&raw) else {
        reply_error(
            &ctx.responder,
            "Invalid Pokémon",
            Some("Type a Pokémon name (for example `pikachu`) or a Pokédex number (for example `25`)."),
            true,
        )
        .await;
        return Ok(());
    };
    let settings = settings_before_ack(ctx).await;
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let (user, guild) = ids(ctx);
    let results = match get_pokemon(&ctx.app, &parsed, Some(&user), guild.as_deref()).await {
        Ok(l) => l.results,
        Err(e) => {
            fail_with(ctx, &e, Api::PokeApi, "PokeAPI").await;
            return Ok(());
        }
    };
    let Some(first) = results.first() else {
        reply_failure(ctx, no_results_embed(NoResultsKind::Pokemon, &raw, true)).await;
        return Ok(());
    };
    let embed = build_pokemon_embed(first, settings.as_deref());
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

/// `/search pokemon` autocomplete (`autocompletePokemon`): suggestions from the in-memory index only.
pub async fn autocomplete_pokemon(ctx: &AutocompleteCtx) -> BotResult {
    let focused = ctx.focused_value();
    if let Some(g) = ctx.guild_id()
        && let Some(s) = ctx.app.settings().peek_settings(&g.to_string())
        && !is_module_enabled(Some(&s), Some(ModuleKey::Search))
    {
        respond_autocomplete(ctx, &[]).await;
        return Ok(());
    }
    let choices = match get_pokemon_index(&ctx.app).await {
        Some(entries) => match_pokemon_index(&entries, &focused, 25),
        None => Vec::new(),
    };
    respond_autocomplete(ctx, &choices).await;
    Ok(())
}

/// `/search urban` (`executeUrban`): only age-restricted channels unless the guild turned `urbanNsfwOnly` off (fail
/// closed on anything unknown).
pub async fn execute_urban(ctx: &CommandCtx) -> BotResult {
    let term = clean_query(&ctx.get_string("term").unwrap_or_default());
    if term.is_empty() {
        reply_error(&ctx.responder, "Invalid Search", Some("Type a word or phrase to define."), true).await;
        return Ok(());
    }
    let settings = settings_before_ack(ctx).await;
    let nsfw_only = settings.as_ref().is_none_or(|s| s.urban_nsfw_only);
    if nsfw_only && !is_age_restricted_channel_with(&ctx.app, ctx.guild_id(), ctx.channel_id(), ctx.interaction.channel.as_ref()) {
        reply_error(
            &ctx.responder,
            "Age-Restricted Channel Required",
            Some("Urban Dictionary definitions can only be shown in age-restricted channels on this server."),
            true,
        )
        .await;
        return Ok(());
    }
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let (user, guild) = ids(ctx);
    let results = match define_urban(&ctx.app, &term, Some(&user), guild.as_deref()).await {
        Ok(l) => l.results,
        Err(e) => {
            fail_with(ctx, &e, Api::Urban, "Urban Dictionary").await;
            return Ok(());
        }
    };
    if results.is_empty() {
        reply_failure(ctx, no_results_embed(NoResultsKind::Urban, &term, true)).await;
        return Ok(());
    }
    let pages = build_urban_embeds(&results, settings.as_deref());
    send_pages(ctx, &pages).await;
    Ok(())
}
