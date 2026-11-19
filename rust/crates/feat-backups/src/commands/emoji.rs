//! Port of `src/commands/admin/emoji.js`: `/emoji add | list | delete`. Core ADMIN command (module = null): copies a
//! custom emoji or uploads an image link as a server emoji, lists the server's emojis with the slot usage for its boost
//! tier, and deletes one after a confirm dialog. No database or Redis: Discord is the source of truth (the guild's emoji
//! cache); the only shared state is the per-guild upload guard (in `app.ext`), because Discord rate-limits emoji creation
//! per guild. Every image download goes through `safe_fetch_image`; uploads are always our own
//! `data:<sniffed mime>;base64,` string.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::{
    Choice, ConfirmOptions, Payload, PermTarget, await_confirm, cached_guild, missing_bot_permissions, reply_error, respond_autocomplete, safe_defer, safe_reply,
};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_util::safe_fetch::{ImageOptions, SafeFetchCode, SafeFetchError, safe_fetch_image};
use bot_util::text::clamp_text;
use futures::future::BoxFuture;
use regex::Regex;
use serenity::all::{EmojiId, GuildId, Permissions};
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("emoji", "Add, list or delete emojis (MEE6 /add_emoji)")
        .default_member_permissions(Permissions::MANAGE_GUILD | Permissions::MANAGE_GUILD_EXPRESSIONS)
        .option(
            Opt::sub_command("add", "Copy an emoji or upload an image URL")
                .option(Opt::string("emoji", "Custom emoji or image URL (max 256 KB)").required(true).max_length(512))
                .option(Opt::string("name", "2–32 letters, numbers or _").min_length(2).max_length(32))
                .option(Opt::role("role", "Only this role can use it")),
        )
        .option(Opt::sub_command("list", "List emojis and free slots"))
        .option(Opt::sub_command("delete", "Delete an emoji (with confirm)").option(Opt::string("emoji", "Emoji, name or ID").required(true).max_length(100).autocomplete(true)))
}

/// `/emoji` (core: `module = null`, ADMIN, guild only, autocomplete).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), None, execute).permission_level(PermissionLevel::Admin).autocomplete(autocomplete)
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Discord's emoji upload limit.
pub const EMOJI_MAX_BYTES: u64 = 256 * 1024;
pub const TIER_EMOJI_LIMITS: [i64; 4] = [50, 100, 150, 250];
/// Guild feature `MORE_EMOJI`.
pub const MORE_EMOJI_LIMIT: i64 = 200;
pub const LIST_PAGE_SIZE: usize = 20;
pub const DOWNLOAD_TIMEOUT_MS: u64 = 8_000;
pub const CREATE_TIMEOUT_MS: u64 = 30_000;
pub const DELETE_TIMEOUT_MS: u64 = 15_000;
pub const IN_FLIGHT_STALE_MS: i64 = 15 * 60_000;
const DISCORD_CDN_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];

static WRAPPED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^<(https?://[^\s<>]+)>$").expect("regex"));
static MENTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^<(a?):([A-Za-z0-9_]{1,32}):([0-9]{17,21})>$").expect("regex"));
static HTTP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^https?://").expect("regex"));
static CDN_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^/emojis/([0-9]{17,21})(?:\.(png|gif|webp|jpe?g|avif))?$").expect("regex"));
static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{17,21}$").expect("regex"));
static SHORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^:([A-Za-z0-9_]{2,32})(?:~[0-9]+)?:$").expect("regex"));
static PICTO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{Extended_Pictographic}|\p{Regional_Indicator}").expect("regex"));
static NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_]{2,32}$").expect("regex"));
static ALL_DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+$").expect("regex"));
static EXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.[A-Za-z0-9]{2,5}$").expect("regex"));
static ANY_MENTION_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^<a?:[A-Za-z0-9_]{1,32}:([0-9]{17,21})>$").expect("regex"));
static ID_IN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([0-9]{17,21})").expect("regex"));

/// `EMOJI_NAME_RE.test(name)`.
pub fn valid_emoji_name(name: &str) -> bool {
    NAME_RE.is_match(name)
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// What the `emoji` option of /emoji add points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmojiSource {
    Custom { id: String, name: Option<String>, animated: bool },
    Url { url: String, name_hint: Option<String> },
    Shortcode { name: String },
    Id { id: String },
    Unicode,
    Empty,
    Invalid,
}

impl EmojiSource {
    /// The JS object (`{ kind, ... }`), for golden comparisons.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            EmojiSource::Custom { id, name, animated } => json!({ "kind": "custom", "id": id, "name": name, "animated": animated }),
            EmojiSource::Url { url, name_hint } => json!({ "kind": "url", "url": url, "nameHint": name_hint }),
            EmojiSource::Shortcode { name } => json!({ "kind": "shortcode", "name": name }),
            EmojiSource::Id { id } => json!({ "kind": "id", "id": id }),
            EmojiSource::Unicode => json!({ "kind": "unicode" }),
            EmojiSource::Empty => json!({ "kind": "empty" }),
            EmojiSource::Invalid => json!({ "kind": "invalid" }),
        }
    }
}

fn query_param(u: &url::Url, key: &str) -> Option<String> {
    u.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

/// What the `emoji` option of /emoji add points at. Never fails.
pub fn parse_emoji_source(input: &str) -> EmojiSource {
    let mut s = bot_util::js::trim(input).to_string();
    if s.is_empty() {
        return EmojiSource::Empty;
    }
    // Discord's embed-suppression brackets around a link.
    if let Some(c) = WRAPPED.captures(&s) {
        s = c[1].to_string();
    }
    if let Some(c) = MENTION.captures(&s) {
        return EmojiSource::Custom { id: c[3].to_string(), name: Some(c[2].to_string()), animated: &c[1] == "a" };
    }
    if HTTP.is_match(&s) {
        let Ok(u) = url::Url::parse(&s) else { return EmojiSource::Invalid };
        if let Some(host) = u.host_str()
            && DISCORD_CDN_HOSTS.contains(&host)
            && let Some(m) = CDN_PATH.captures(u.path())
        {
            let n = sanitize_emoji_name(&query_param(&u, "name").unwrap_or_default());
            let ext = m.get(2).map(|x| x.as_str().to_lowercase()).unwrap_or_default();
            return EmojiSource::Custom {
                id: m[1].to_string(),
                name: valid_emoji_name(&n).then_some(n),
                animated: ext == "gif" || query_param(&u, "animated").as_deref() == Some("true"),
            };
        }
        let href = u.as_str().to_string();
        return EmojiSource::Url { name_hint: name_from_url(&href), url: href };
    }
    // Before the shortcode check: digits also match the shortcode pattern.
    if ID.is_match(&s) {
        return EmojiSource::Id { id: s };
    }
    // Both colons: Discord sends ":name:" for an emoji the sender cannot use. A bare word is just text.
    if let Some(c) = SHORT.captures(&s) {
        return EmojiSource::Shortcode { name: c[1].to_string() };
    }
    if PICTO.is_match(&s) {
        return EmojiSource::Unicode;
    }
    EmojiSource::Invalid
}

/// The Discord CDN image of a custom emoji.
pub fn emoji_cdn_url(id: &str, animated: bool, size: Option<u32>) -> String {
    format!("https://cdn.discordapp.com/emojis/{id}.{}{}", if animated { "gif" } else { "png" }, size.map(|s| format!("?size={s}")).unwrap_or_default())
}

/// Cleans a name toward `[A-Za-z0-9_]`. The result may be '' or one character long; the caller validates it.
pub fn sanitize_emoji_name(raw: &str) -> String {
    let decomposed: String = raw.nfkd().filter(|c| !('\u{0300}'..='\u{036f}').contains(c)).collect();
    let trimmed = bot_util::js::trim(&decomposed);
    let mut out = String::new();
    let mut in_run = false;
    for c in trimmed.chars() {
        if bot_util::js::is_js_space(c) || c == '.' || c == '-' {
            if !in_run {
                out.push('_');
            }
            in_run = true;
        } else {
            in_run = false;
            out.push(c);
        }
    }
    out.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(32).collect()
}

/// `decodeURIComponent`: `None` on a malformed escape or bytes that are not UTF-8.
fn decode_uri_component(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3).filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A usable emoji name from a link: its `?name=` parameter, or the file name without extension.
pub fn name_from_url(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let from_query = sanitize_emoji_name(&query_param(&u, "name").unwrap_or_default());
    if valid_emoji_name(&from_query) {
        return Some(from_query);
    }
    let segment = u.path().split('/').rfind(|p| !p.is_empty())?.to_string();
    let decoded = decode_uri_component(&segment).unwrap_or(segment);
    let n = sanitize_emoji_name(&EXT.replace(&decoded, ""));
    (valid_emoji_name(&n) && !ALL_DIGITS.is_match(&n)).then_some(n)
}

/// The name for a new emoji (`resolveEmojiName`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameResult {
    Ok { name: String, adjusted: bool, derived: bool },
    Err(String),
}

/// The name for a new emoji: the given one (sanitised, must be valid) or one derived from the source.
pub fn resolve_emoji_name(given: Option<&str>, source: &EmojiSource) -> NameResult {
    if let Some(raw) = given {
        let name = sanitize_emoji_name(raw);
        if !valid_emoji_name(&name) {
            return NameResult::Err("Emoji names need 2–32 characters: letters, numbers or underscores.".into());
        }
        let adjusted = name != bot_util::js::trim(raw);
        return NameResult::Ok { name, adjusted, derived: false };
    }
    let base = match source {
        EmojiSource::Custom { name, .. } => name.clone(),
        EmojiSource::Url { name_hint, .. } => name_hint.clone(),
        _ => None,
    };
    let mut name = sanitize_emoji_name(&base.unwrap_or_default());
    if !valid_emoji_name(&name) || ALL_DIGITS.is_match(&name) {
        name = "emoji".into();
    }
    NameResult::Ok { name, adjusted: false, derived: true }
}

/// A server emoji as these helpers see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiInfo {
    pub id: String,
    pub name: String,
    pub animated: bool,
    pub managed: bool,
    pub available: bool,
    /// `e.roles.cache.size`.
    pub role_count: usize,
}

impl EmojiInfo {
    pub fn of(e: &serenity::all::Emoji) -> Self {
        Self { id: e.id.to_string(), name: e.name.clone(), animated: e.animated, managed: e.managed, available: e.available, role_count: e.roles.len() }
    }
    /// `${emoji}`: `<:name:id>` or `<a:name:id>`.
    pub fn mention(&self) -> String {
        format!("<{}:{}:{}>", if self.animated { "a" } else { "" }, self.name, self.id)
    }
}

/// The lowercased names of every emoji, managed ones included.
pub fn taken_names(emojis: &[EmojiInfo]) -> HashSet<String> {
    emojis.iter().map(|e| e.name.to_lowercase()).collect()
}

/// `base`, or `base_2` … `base_99` (cut so the result stays within 32 characters), whichever is free first.
pub fn pick_free_name(base: &str, taken_lower: &HashSet<String>) -> Option<String> {
    if !taken_lower.contains(&base.to_lowercase()) {
        return Some(base.to_string());
    }
    for n in 2..=99 {
        let suffix = format!("_{n}");
        let keep = 32usize.saturating_sub(suffix.len());
        let cand = format!("{}{suffix}", bot_util::js::utf16_prefix(base, keep));
        if !taken_lower.contains(&cand.to_lowercase()) {
            return Some(cand);
        }
    }
    None
}

/// Emoji slots per kind (static and animated are counted separately) for a boost tier.
pub fn emoji_slot_limit(premium_tier: i64, features: &[String]) -> i64 {
    let tier = usize::try_from(premium_tier).ok().and_then(|t| TIER_EMOJI_LIMITS.get(t)).copied().unwrap_or(TIER_EMOJI_LIMITS[0]);
    if features.iter().any(|f| f == "MORE_EMOJI") { MORE_EMOJI_LIMIT.max(tier) } else { tier }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SlotUse {
    pub used: i64,
    pub max: i64,
    pub free: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Slots {
    pub tier: i64,
    #[serde(rename = "static")]
    pub static_: SlotUse,
    pub animated: SlotUse,
}

impl Slots {
    fn of_kind(&self, animated: bool) -> &SlotUse {
        if animated { &self.animated } else { &self.static_ }
    }
}

/// Slot usage. Managed (integration) emojis are not counted; unavailable ones are.
pub fn count_emoji_slots(emojis: &[EmojiInfo], premium_tier: i64, features: &[String]) -> Slots {
    let max = emoji_slot_limit(premium_tier, features);
    let (mut s, mut a) = (0, 0);
    for e in emojis.iter().filter(|e| !e.managed) {
        if e.animated { a += 1 } else { s += 1 }
    }
    Slots { tier: premium_tier, static_: SlotUse { used: s, max, free: (max - s).max(0) }, animated: SlotUse { used: a, max, free: (max - a).max(0) } }
}

pub fn format_slots(s: &Slots) -> String {
    format!("Static {}/{} · Animated {}/{} · Boost level {}", s.static_.used, s.static_.max, s.animated.used, s.animated.max, s.tier)
}

/// The image type from its magic bytes (the Content-Type header is never trusted): `(mime, ext, animated)`.
pub fn sniff_image(b: &[u8]) -> Option<(&'static str, &'static str, bool)> {
    if b.len() < 12 {
        return None;
    }
    if b[0] == 0x89 && &b[1..4] == b"PNG" && b[4] == 0x0d && b[5] == 0x0a && b[6] == 0x1a && b[7] == 0x0a {
        return Some(("image/png", "png", false)); // APNG counts as static
    }
    if b[0] == 0xff && b[1] == 0xd8 && b[2] == 0xff {
        return Some(("image/jpeg", "jpg", false));
    }
    if &b[0..6] == b"GIF87a" || &b[0..6] == b"GIF89a" {
        return Some(("image/gif", "gif", true));
    }
    if &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        let animated = b.len() >= 16 && &b[12..16] == b"VP8X" && b.len() > 20 && (b[20] & 0x02) != 0;
        return Some(("image/webp", "webp", animated));
    }
    None
}

/// A failed download (`SafeFetchError`, or anything else).
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadError {
    Fetch(SafeFetchError),
    Other(String),
}

/// A human text for a failed download.
pub fn describe_fetch_error(err: &DownloadError, from_cdn: bool) -> String {
    let DownloadError::Fetch(e) = err else { return "The image could not be downloaded.".into() };
    match e.code {
        SafeFetchCode::InvalidUrl => e.message.clone(),
        SafeFetchCode::BlockedAddress => "That link points to a private or reserved network address.".into(),
        SafeFetchCode::DnsFailed => "I couldn't resolve that link's host name.".into(),
        SafeFetchCode::Timeout => "The image took too long to download (8 s limit).".into(),
        SafeFetchCode::TooLarge => "That image is larger than 256 KB, Discord's limit for emojis. Use a smaller image.".into(),
        SafeFetchCode::TooManyRedirects => "That link redirects too many times.".into(),
        SafeFetchCode::HttpError => {
            if from_cdn && e.status == Some(404) {
                "Discord's CDN doesn't have that emoji (it may have been deleted).".into()
            } else {
                format!("The link returned HTTP {}.", e.status.map(|s| s.to_string()).unwrap_or_else(|| "?".into()))
            }
        }
        SafeFetchCode::BadContentType => "That link is not a PNG, JPG, GIF or WebP image.".into(),
        SafeFetchCode::BadImage => "That file is not a valid image (it may be corrupt or have too large dimensions).".into(),
        SafeFetchCode::Network => "A network error stopped the download. Try again in a moment.".into(),
    }
}

/// A failed create or delete: a timeout, or a Discord error.
#[derive(Debug, Clone, PartialEq)]
pub enum EmojiError {
    Timeout,
    Discord { code: Option<u64>, message: String },
}

impl EmojiError {
    pub fn of(e: &serenity::Error) -> Self {
        EmojiError::Discord { code: bot_core::discord::error_code(e), message: bot_core::discord::error_message(e) }
    }
}

/// A human text for a failed create or delete.
pub fn describe_emoji_error(err: &EmojiError, delete: bool) -> String {
    let (code, message) = match err {
        EmojiError::Timeout => {
            return if delete {
                "Discord is rate-limiting emoji changes right now. The deletion is queued and will finish on its own; check `/emoji list` later.".into()
            } else {
                "Discord is rate-limiting emoji uploads for this server. Your upload is queued and will finish on its own when Discord allows it (this can take a while). Check `/emoji list` later before trying again.".into()
            };
        }
        EmojiError::Discord { code, message } => (*code, message.as_str()),
    };
    match code {
        Some(30008) => "This server has no free static emoji slots.".into(),
        Some(30018) => "This server has no free animated emoji slots.".into(),
        Some(50013) if delete => {
            "I need the **Manage Expressions** permission to delete that emoji (with only Create Expressions I can delete just the emojis I uploaded).".into()
        }
        Some(50013) => "I need the **Manage Expressions** (or **Create Expressions**) permission to upload emojis.".into(),
        Some(50001) => "I can't access this server's emojis.".into(),
        Some(50045) => "That image is larger than 256 KB.".into(),
        Some(50046) => "Discord couldn't read that image file.".into(),
        Some(50138) => "Discord couldn't resize that image to fit its emoji limits. Try a smaller or simpler image.".into(),
        Some(50035) => format!("Discord rejected the emoji: {}", clamp_text(message, 300)),
        Some(10014) => "That emoji no longer exists.".into(),
        _ => format!("Discord returned an error: {}", clamp_text(message, 200)),
    }
}

/// Codes that are expected user situations: logged at debug, not warn.
const EXPECTED_DISCORD_CODES: [u64; 7] = [30008, 30018, 50013, 50045, 50046, 50138, 10014];

/// One line of /emoji list.
pub fn format_emoji_line(e: &EmojiInfo) -> String {
    let mut tags: Vec<String> = vec![];
    if e.animated {
        tags.push("animated".into());
    }
    if e.role_count > 0 {
        tags.push(format!("🔒 {} role{}", e.role_count, if e.role_count == 1 { "" } else { "s" }));
    }
    if e.managed {
        tags.push("managed".into());
    }
    if !e.available {
        tags.push("⚠️ unavailable".into());
    }
    format!("{} `:{}:` · `{}`{}", e.mention(), e.name, e.id, if tags.is_empty() { String::new() } else { format!(" · {}", tags.join(" · ")) })
}

/// One page of /emoji list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ListPage {
    pub description: String,
    pub footer: String,
}

/// The /emoji list pages: static before animated, each by name then id. Empty for no emojis.
pub fn build_list_pages(emojis: &[EmojiInfo], slots: &Slots, per_page: usize) -> Vec<ListPage> {
    if emojis.is_empty() {
        return vec![];
    }
    let size = per_page.clamp(1, 25);
    let mut sorted = emojis.to_vec();
    sorted.sort_by(|a, b| {
        a.animated
            .cmp(&b.animated)
            .then_with(|| a.name.to_lowercase().encode_utf16().cmp(b.name.to_lowercase().encode_utf16()))
            .then_with(|| a.id.encode_utf16().cmp(b.id.encode_utf16()))
    });
    let total = sorted.len();
    let footer = format!("{total} custom emoji{}", if total == 1 { "" } else { "s" });
    sorted
        .chunks(size)
        .map(|chunk| {
            let lines: Vec<String> = chunk.iter().map(format_emoji_line).collect();
            ListPage { description: clamp_text(&format!("{}\n\n{}", format_slots(slots), lines.join("\n")), 4096), footer: footer.clone() }
        })
        .collect()
}

/// Finds one of the server's emojis from a mention, an ID, or a name (exact case first, then case-insensitive).
pub fn resolve_guild_emoji(emojis: &[EmojiInfo], input: &str) -> Result<EmojiInfo, String> {
    let s = bot_util::js::trim(input);
    if s.is_empty() {
        return Err("Pick an emoji from the list, or paste it or its ID.".into());
    }
    if let Some(c) = ANY_MENTION_ID.captures(s) {
        return emojis.iter().find(|e| e.id == c[1]).cloned().ok_or_else(|| "That emoji isn't from this server.".into());
    }
    if ID.is_match(s) {
        return emojis.iter().find(|e| e.id == s).cloned().ok_or_else(|| "No emoji with that ID exists in this server.".into());
    }
    let name = s.strip_prefix(':').unwrap_or(s);
    let name = name.strip_suffix(':').unwrap_or(name);
    let shown = clamp_text(name, 40);
    let ambiguous = || format!("Several emojis are named `:{shown}:`. Pick one from the list or use its ID.");
    let exact: Vec<&EmojiInfo> = emojis.iter().filter(|e| e.name == name).collect();
    if exact.len() == 1 {
        return Ok(exact[0].clone());
    }
    if exact.len() > 1 {
        return Err(ambiguous());
    }
    let lower = name.to_lowercase();
    let loose: Vec<&EmojiInfo> = emojis.iter().filter(|e| e.name.to_lowercase() == lower).collect();
    if loose.len() == 1 {
        return Ok(loose[0].clone());
    }
    if loose.len() > 1 {
        return Err(ambiguous());
    }
    Err(format!("No emoji named `:{shown}:` in this server."))
}

/// Autocomplete choices for /emoji delete: at most 25 non-managed emojis, values are IDs.
pub fn emoji_choices(emojis: &[EmojiInfo], query: &str) -> Vec<(String, String)> {
    let q = bot_util::js::trim(query).to_lowercase();
    let pool: Vec<&EmojiInfo> = emojis.iter().filter(|e| !e.managed).collect();
    let picked: Vec<&EmojiInfo> = if let Some(id) = ID_IN.captures(&q).map(|c| c[1].to_string()) {
        pool.into_iter().filter(|e| e.id == id).collect()
    } else {
        let nq: String = q.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_').collect();
        let mut v: Vec<&EmojiInfo> = pool.into_iter().filter(|e| e.name.to_lowercase().contains(&nq)).collect();
        v.sort_by(|a, b| {
            let (an, bn) = (a.name.to_lowercase(), b.name.to_lowercase());
            let (as_, bs) = (!an.starts_with(&nq), !bn.starts_with(&nq));
            as_.cmp(&bs).then_with(|| an.encode_utf16().cmp(bn.encode_utf16()))
        });
        v
    };
    picked.into_iter().take(25).map(|e| (format!(":{}:{}", e.name, if e.animated { " (animated)" } else { "" }), e.id.clone())).collect()
}

// ── Per-guild upload guard (in-process; a guild's commands always reach its own shard) ─────

/// The upload guard: guild -> (token, startedAt).
#[derive(Default)]
pub struct UploadGuard {
    inner: Mutex<GuardState>,
}

/// The last token handed out, and guild -> (token, startedAt).
type GuardState = (u64, HashMap<String, (u64, i64)>);

impl UploadGuard {
    /// Claims the guild's upload slot: a token, or `None` while an upload is running (and not stale).
    pub fn begin(&self, guild_id: &str, now_ms: i64) -> Option<u64> {
        let mut g = self.inner.lock().ok()?;
        if let Some((_, started)) = g.1.get(guild_id)
            && now_ms - started < IN_FLIGHT_STALE_MS
        {
            return None;
        }
        g.0 += 1;
        let token = g.0;
        g.1.insert(guild_id.to_string(), (token, now_ms));
        Some(token)
    }

    /// Releases the slot only when `token` still owns it.
    pub fn end(&self, guild_id: &str, token: u64) {
        if let Ok(mut g) = self.inner.lock()
            && g.1.get(guild_id).is_some_and(|(t, _)| *t == token)
        {
            g.1.remove(guild_id);
        }
    }

    pub fn is_busy(&self, guild_id: &str) -> bool {
        self.inner.lock().map(|g| g.1.contains_key(guild_id)).unwrap_or(false)
    }
}

// ── Download and create (test seams in `app.ext`) ─────────────────────────────

/// A download: the source in, the bytes out.
pub type DownloadFn = Arc<dyn Fn(EmojiSource) -> BoxFuture<'static, Result<Vec<u8>, DownloadError>> + Send + Sync>;
/// An emoji create: `(guild, name, data URI, role, reason)` in, the new emoji out.
pub type CreateFn = Arc<dyn Fn(GuildId, String, String, Option<String>, String) -> BoxFuture<'static, Result<EmojiInfo, EmojiError>> + Send + Sync>;

/// `__setTestHooks`: replaces the download, the create call and the create timeout.
#[derive(Default)]
pub struct EmojiTestHooks {
    pub download: Mutex<Option<DownloadFn>>,
    pub create: Mutex<Option<CreateFn>>,
    pub create_timeout_ms: Mutex<Option<u64>>,
}

async fn download_emoji_image(source: EmojiSource) -> Result<Vec<u8>, DownloadError> {
    let opts = ImageOptions { max_bytes: EMOJI_MAX_BYTES, timeout_ms: DOWNLOAD_TIMEOUT_MS, ..Default::default() };
    match &source {
        EmojiSource::Custom { id, animated, .. } => match safe_fetch_image(&emoji_cdn_url(id, *animated, None), &opts).await {
            Ok(b) => Ok(b),
            // One retry, smaller.
            Err(e) if e.code == SafeFetchCode::TooLarge => safe_fetch_image(&emoji_cdn_url(id, *animated, Some(96)), &opts).await.map_err(DownloadError::Fetch),
            Err(e) => Err(DownloadError::Fetch(e)),
        },
        EmojiSource::Url { url, .. } => safe_fetch_image(url, &opts).await.map_err(DownloadError::Fetch),
        _ => Err(DownloadError::Other("nothing to download".into())),
    }
}

fn default_create(app: &App) -> CreateFn {
    let app = app.clone();
    Arc::new(move |guild_id, name, image, role, reason| {
        let app = app.clone();
        Box::pin(async move {
            let Some(http) = app.http() else { return Err(EmojiError::Discord { code: None, message: "Discord is not connected".into() }) };
            let mut body = serde_json::json!({ "name": name, "image": image });
            if let Some(r) = role {
                body["roles"] = serde_json::json!([r]);
            }
            http.create_emoji(guild_id, &body, Some(&reason)).await.map(|e| EmojiInfo::of(&e)).map_err(|e| EmojiError::of(&e))
        })
    })
}

// ── Command helpers ───────────────────────────────────────────────────────────

fn bot_has(app: &App, g: GuildId, key: &str) -> bool {
    missing_bot_permissions(app, PermTarget::Guild(g), &[key]).is_empty()
}
fn bot_can_create(app: &App, g: GuildId) -> bool {
    bot_has(app, g, "ManageGuildExpressions") || bot_has(app, g, "CreateGuildExpressions")
}
fn bot_can_manage(app: &App, g: GuildId) -> bool {
    bot_has(app, g, "ManageGuildExpressions")
}

fn audit_reason(ctx: &CommandCtx, sub: &str) -> String {
    clamp_text(&format!("{} ({}) via /emoji {sub}", bot_core::adapters::user_tag(ctx.user()), ctx.user().id), 512)
}

/// Guild-level permissions of the invoker (the cached member's roles; otherwise the interaction's own).
fn invoker_has(ctx: &CommandCtx, flag: Permissions) -> bool {
    let cached = ctx.guild_id().and_then(|g| cached_guild(&ctx.app, g)).and_then(|guild| {
        let roles = ctx.member().map(|m| m.roles.clone())?;
        Some(bot_core::permissions::guild_permissions(&guild, ctx.user().id, &roles))
    });
    let perms = cached.or_else(|| ctx.member().and_then(|m| m.permissions));
    perms.is_some_and(|p| bot_core::permissions::js_has(p, flag))
}

fn no_slots_text(animated: bool, slots: &Slots) -> String {
    let kind = if animated { "animated" } else { "static" };
    let s = slots.of_kind(animated);
    format!("This server has no free {kind} emoji slots ({}/{}, boost level {}). Delete one with `/emoji delete` or boost the server.", s.used, s.max, slots.tier)
}

fn log_discord_error(sub: &str, err: &EmojiError) {
    match err {
        EmojiError::Discord { code: Some(c), message } if EXPECTED_DISCORD_CODES.contains(c) => bot_util::log_debug!(format!("[emoji] {sub} failed: {c} {message}")),
        EmojiError::Discord { code, message } => bot_util::log_warn!(format!("[emoji] {sub} failed: {} {message}", code.map(|c| c.to_string()).unwrap_or_default())),
        EmojiError::Timeout => {}
    }
}

/// The guild's emojis, tier and features from the cache.
struct GuildEmojis {
    name: String,
    emojis: Vec<EmojiInfo>,
    tier: i64,
    features: Vec<String>,
}

fn guild_emojis(app: &App, g: GuildId) -> GuildEmojis {
    match cached_guild(app, g) {
        Some(guild) => {
            let mut emojis: Vec<EmojiInfo> = guild.emojis.values().map(EmojiInfo::of).collect();
            emojis.sort_by_key(|e| e.id.parse::<u64>().unwrap_or(0));
            GuildEmojis { name: guild.name.clone(), emojis, tier: u8::from(guild.premium_tier) as i64, features: guild.features.clone() }
        }
        None => GuildEmojis { name: String::new(), emojis: vec![], tier: 0, features: vec![] },
    }
}

async fn error_reply(ctx: &CommandCtx, title: &str, desc: &str) {
    reply_error(&ctx.responder, title, Some(desc), true).await;
}

async fn public_answer(ctx: &CommandCtx, embed: serenity::all::CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
}

// ── /emoji add ────────────────────────────────────────────────────────────────

async fn run_add(ctx: &CommandCtx) -> BotResult {
    let Some(gid) = ctx.guild_id() else { return Ok(()) };
    // Pre-defer checks use only synchronous data, so they stay inside the 3-second window. Failures are ephemeral.
    if !invoker_has(ctx, Permissions::MANAGE_GUILD_EXPRESSIONS) {
        error_reply(ctx, "Missing Permissions", "You need the **Manage Expressions** permission to add emojis.").await;
        return Ok(());
    }
    if !bot_can_create(&ctx.app, gid) {
        error_reply(ctx, "Missing Permissions", "I need the **Manage Expressions** (or **Create Expressions**) permission in this server to add emojis.").await;
        return Ok(());
    }
    let source = parse_emoji_source(&ctx.get_string("emoji").unwrap_or_default());
    match &source {
        EmojiSource::Empty | EmojiSource::Invalid => {
            error_reply(ctx, "Invalid Emoji", "That isn't a custom emoji or an image link. Paste a custom emoji (`<:name:id>`) or a direct image link starting with https://. Tip: right-click an emoji → **Copy Link**.").await;
            return Ok(());
        }
        EmojiSource::Unicode => {
            error_reply(ctx, "Invalid Emoji", "Standard emojis are built into Discord and can't be uploaded. Paste a custom emoji or an image link instead.").await;
            return Ok(());
        }
        EmojiSource::Shortcode { name } => {
            error_reply(ctx, "Invalid Emoji", &format!("I only received the name `:{name}:`. Discord sends the full emoji only if you can use it (e.g. with Nitro). Right-click the emoji → **Copy Link** and paste that link instead.")).await;
            return Ok(());
        }
        EmojiSource::Id { .. } => {
            error_reply(ctx, "Invalid Emoji", "An ID alone doesn't tell me which image to copy or whether it is animated. Paste the emoji itself or its image link.").await;
            return Ok(());
        }
        _ => {}
    }
    let ge = guild_emojis(&ctx.app, gid);
    if let EmojiSource::Custom { id, .. } = &source
        && let Some(existing) = ge.emojis.iter().find(|e| &e.id == id)
    {
        error_reply(ctx, "Already Added", &format!("That emoji is already in this server: {} `:{}:`", existing.mention(), existing.name)).await;
        return Ok(());
    }
    let role = ctx.get_role_id("role");
    if role.is_some_and(|r| r.get() == gid.get()) {
        error_reply(ctx, "Invalid Role", "Pick a specific role. Restricting an emoji to @everyone does nothing.").await;
        return Ok(());
    }
    let (mut name, adjusted, derived) = match resolve_emoji_name(ctx.get_string("name").as_deref(), &source) {
        NameResult::Err(reason) => {
            error_reply(ctx, "Invalid Name", &reason).await;
            return Ok(());
        }
        NameResult::Ok { name, adjusted, derived } => (name, adjusted, derived),
    };
    let taken = taken_names(&ge.emojis);
    let mut renamed_from = None;
    if derived {
        let Some(free) = pick_free_name(&name, &taken) else {
            error_reply(ctx, "Name Taken", &format!("`:{name}:` and its numbered variants are taken. Choose a `name`.")).await;
            return Ok(());
        };
        if free != name {
            renamed_from = Some(name.clone());
        }
        name = free;
    } else if taken.contains(&name.to_lowercase()) {
        error_reply(ctx, "Name Taken", &format!("An emoji named `:{name}:` already exists. Choose another `name`.")).await;
        return Ok(());
    }
    let slots = count_emoji_slots(&ge.emojis, ge.tier, &ge.features);
    if let EmojiSource::Custom { animated, .. } = &source {
        if slots.of_kind(*animated).free == 0 {
            error_reply(ctx, "No Free Slots", &no_slots_text(*animated, &slots)).await;
            return Ok(());
        }
    } else if slots.static_.free == 0 && slots.animated.free == 0 {
        error_reply(ctx, "No Free Slots", &format!("This server has no free emoji slots ({}).", format_slots(&slots))).await;
        return Ok(());
    }

    let guard = ctx.app.ext::<UploadGuard>();
    let g = gid.to_string();
    let Some(token) = guard.begin(&g, ctx.app.clock().now_ms()) else {
        error_reply(ctx, "Upload In Progress", "Another emoji upload is still running in this server (it may be waiting on Discord's rate limit). Try again in a moment.").await;
        return Ok(());
    };
    // Once the create task owns the guard, this function must not release it.
    let handed_off = add_after_guard(ctx, gid, &source, &name, adjusted, renamed_from, role.map(|r| r.to_string()), token).await;
    if !handed_off {
        guard.end(&g, token);
    }
    Ok(())
}

/// The part of `/emoji add` that runs while this command holds the upload guard. True once the create owns it.
#[allow(clippy::too_many_arguments)]
async fn add_after_guard(
    ctx: &CommandCtx,
    gid: GuildId,
    source: &EmojiSource,
    name: &str,
    adjusted: bool,
    renamed_from: Option<String>,
    role: Option<String>,
    token: u64,
) -> bool {
    if !safe_defer(&ctx.responder, false).await {
        return false;
    }
    let hooks = ctx.app.ext::<EmojiTestHooks>();
    let download = hooks.download.lock().ok().and_then(|d| d.clone());
    let downloaded = match download {
        Some(f) => f(source.clone()).await,
        None => download_emoji_image(source.clone()).await,
    };
    let buffer = match downloaded {
        Ok(b) => b,
        Err(err) => {
            match &err {
                DownloadError::Fetch(e) => bot_util::log_debug!(format!("[emoji] download failed ({}): {}", e.code.as_str(), e.message)),
                DownloadError::Other(m) => bot_util::log_warn!(format!("[emoji] download failed: {m}")),
            }
            let from_cdn = matches!(source, EmojiSource::Custom { .. });
            public_answer(ctx, error_embed("Download Failed", Some(&describe_fetch_error(&err, from_cdn)), None)).await;
            return false;
        }
    };
    let Some((mime, _ext, animated)) = sniff_image(&buffer) else {
        public_answer(ctx, error_embed("Unsupported Image", Some("That file is not a PNG, JPG, GIF or WebP image."), None)).await;
        return false;
    };
    // The download took a while: another admin may have filled the last slot. The image decides the kind now.
    let ge = guild_emojis(&ctx.app, gid);
    let slots = count_emoji_slots(&ge.emojis, ge.tier, &ge.features);
    if slots.of_kind(animated).free == 0 {
        public_answer(ctx, error_embed("No Free Slots", Some(&no_slots_text(animated, &slots)), None)).await;
        return false;
    }
    let image = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&buffer));
    let create = hooks.create.lock().ok().and_then(|c| c.clone()).unwrap_or_else(|| default_create(&ctx.app));
    let timeout_ms = hooks.create_timeout_ms.lock().ok().and_then(|t| *t).unwrap_or(CREATE_TIMEOUT_MS);
    let fut = create(gid, name.to_string(), image, role.clone(), audit_reason(ctx, "add"));
    // Hand the guard to the create task: it is released when the create settles, whatever this command does.
    let (guard, g) = (ctx.app.ext::<UploadGuard>(), gid.to_string());
    let task = tokio::spawn(async move {
        let r = fut.await;
        guard.end(&g, token);
        r
    });
    let result = match tokio::time::timeout(Duration::from_millis(timeout_ms), task).await {
        Err(_) => Err(EmojiError::Timeout),
        Ok(Err(join)) => Err(EmojiError::Discord { code: None, message: join.to_string() }),
        Ok(Ok(r)) => r,
    };
    let emoji = match result {
        Ok(e) => e,
        Err(EmojiError::Timeout) => {
            public_answer(ctx, warn_embed("Upload Queued", Some(&describe_emoji_error(&EmojiError::Timeout, false)), None)).await;
            return true;
        }
        Err(err) => {
            log_discord_error("add", &err);
            public_answer(ctx, error_embed("Upload Failed", Some(&describe_emoji_error(&err, false)), None)).await;
            return true;
        }
    };
    bot_util::log_info!(format!("[emoji] {gid}: added {} :{}: by {}", emoji.id, emoji.name, ctx.user().id));
    // The cache learns of the new emoji from the gateway; count it now, as discord.js does.
    let mut after_list = guild_emojis(&ctx.app, gid);
    if !after_list.emojis.iter().any(|e| e.id == emoji.id) {
        after_list.emojis.push(emoji.clone());
    }
    let after = count_emoji_slots(&after_list.emojis, after_list.tier, &after_list.features);
    let mut lines = vec![format!("{} `:{}:` is ready to use.", emoji.mention(), emoji.name)];
    if let Some(r) = &role {
        lines.push(format!("Only members with <@&{r}> can use it."));
    }
    if adjusted {
        lines.push(format!("The name was adjusted to `{}` (letters, numbers and _ only).", emoji.name));
    }
    if let Some(from) = renamed_from {
        lines.push(format!("`{from}` was taken, so it was named `{}`.", emoji.name));
    }
    lines.push(String::new());
    lines.push(format!("**Slots:** {}", format_slots(&after)));
    let embed = success_embed("Emoji Added", Some(&lines.join("\n")), None).thumbnail(emoji_cdn_url(&emoji.id, emoji.animated, Some(128)));
    public_answer(ctx, embed).await;
    true
}

// ── /emoji list ───────────────────────────────────────────────────────────────

async fn run_list(ctx: &CommandCtx) -> BotResult {
    let Some(gid) = ctx.guild_id() else { return Ok(()) };
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let settings = ctx.app.settings().get_settings_safe(&gid.to_string()).await;
    let mut ge = guild_emojis(&ctx.app, gid);
    if let Some(http) = ctx.app.http() {
        match tokio::time::timeout(Duration::from_millis(10_000), http.get_emojis(gid)).await {
            Ok(Ok(list)) => ge.emojis = list.iter().map(EmojiInfo::of).collect(),
            Ok(Err(e)) => bot_util::log_debug!(format!("[emoji] list fetch failed, using the cache: {e}")),
            Err(_) => bot_util::log_debug!("[emoji] list fetch failed, using the cache: emoji list fetch timed out"),
        }
    }
    let slots = count_emoji_slots(&ge.emojis, ge.tier, &ge.features);
    let pages = build_list_pages(&ge.emojis, &slots, LIST_PAGE_SIZE);
    if pages.is_empty() {
        let desc = format!("This server has no custom emojis yet. Add one with `/emoji add`.\n\n**Slots:** {}", format_slots(&slots));
        public_answer(ctx, guild_embed(settings.as_deref(), Some("Server Emojis"), Some(&desc), None)).await;
        return Ok(());
    }
    let title = clamp_text(&format!("Emojis in {}", ge.name), 256);
    let embeds: Vec<_> = pages.iter().map(|p| guild_embed(settings.as_deref(), Some(&title), Some(&p.description), Some(&p.footer))).collect();
    paginate(ctx, &embeds, PaginateOptions::default()).await?;
    Ok(())
}

// ── /emoji delete ─────────────────────────────────────────────────────────────

async fn run_delete(ctx: &CommandCtx) -> BotResult {
    let Some(gid) = ctx.guild_id() else { return Ok(()) };
    if !invoker_has(ctx, Permissions::MANAGE_GUILD_EXPRESSIONS) {
        error_reply(ctx, "Missing Permissions", "You need the **Manage Expressions** permission to delete emojis.").await;
        return Ok(());
    }
    // With only Create Expressions the attempt is still made: Discord lets the bot delete emojis it uploaded itself.
    if !bot_can_manage(&ctx.app, gid) && !bot_can_create(&ctx.app, gid) {
        error_reply(ctx, "Missing Permissions", "I need the **Manage Expressions** permission in this server to delete emojis.").await;
        return Ok(());
    }
    let ge = guild_emojis(&ctx.app, gid);
    let emoji = match resolve_guild_emoji(&ge.emojis, &ctx.get_string("emoji").unwrap_or_default()) {
        Err(reason) => {
            error_reply(ctx, "Emoji Not Found", &reason).await;
            return Ok(());
        }
        Ok(e) => e,
    };
    if emoji.managed {
        error_reply(ctx, "Managed Emoji", "That emoji is managed by an integration (for example Twitch) and can't be deleted here.").await;
        return Ok(());
    }
    let ok = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Delete this emoji?".into()),
            description: Some(format!("{} `:{}:` will be removed for everyone. This cannot be undone.", emoji.mention(), emoji.name)),
            confirm_label: "Delete".into(),
            cancel_label: "Cancel".into(),
            danger: true,
            timeout_ms: 30_000.0,
            ephemeral: true,
        },
    )
    .await;
    if !ok {
        return Ok(()); // the confirm message already says "Cancelled" or "Timed out"
    }
    if !guild_emojis(&ctx.app, gid).emojis.iter().any(|e| e.id == emoji.id) {
        safe_reply(&ctx.responder, Payload::embed(error_embed("Already Deleted", Some("That emoji was already deleted."), None)).ephemeral(true)).await;
        return Ok(());
    }
    let reason = audit_reason(ctx, "delete");
    let res = match (ctx.app.http(), emoji.id.parse::<u64>()) {
        (Some(http), Ok(id)) => {
            // Spawned: a delete waiting on Discord's rate limit keeps going after the timeout ("queued").
            let task = tokio::spawn(async move { http.delete_emoji(gid, EmojiId::new(id), Some(&reason)).await.map_err(|e| EmojiError::of(&e)) });
            match tokio::time::timeout(Duration::from_millis(DELETE_TIMEOUT_MS), task).await {
                Err(_) => Err(EmojiError::Timeout),
                Ok(Err(join)) => Err(EmojiError::Discord { code: None, message: join.to_string() }),
                Ok(Ok(r)) => r,
            }
        }
        _ => Err(EmojiError::Discord { code: None, message: "Discord is not connected".into() }),
    };
    if let Err(err) = res {
        if err == EmojiError::Timeout {
            safe_reply(&ctx.responder, Payload::embed(warn_embed("Deletion Queued", Some(&describe_emoji_error(&err, true)), None)).ephemeral(true)).await;
        } else {
            log_discord_error("delete", &err);
            safe_reply(&ctx.responder, Payload::embed(error_embed("Delete Failed", Some(&describe_emoji_error(&err, true)), None)).ephemeral(true)).await;
        }
        return Ok(());
    }
    // The cache is updated only by the gateway event, so exclude the deleted id explicitly.
    let now = guild_emojis(&ctx.app, gid);
    let rest: Vec<EmojiInfo> = now.emojis.into_iter().filter(|e| e.id != emoji.id).collect();
    let after = count_emoji_slots(&rest, now.tier, &now.features);
    bot_util::log_info!(format!("[emoji] {gid}: deleted {} :{}: by {}", emoji.id, emoji.name, ctx.user().id));
    public_answer(ctx, success_embed("Emoji Deleted", Some(&format!("`:{}:` was deleted.\n\n**Slots:** {}", emoji.name, format_slots(&after))), None)).await;
    Ok(())
}

// ── Entry points ──────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("add") => run_add(&ctx).await,
        Some("list") => run_list(&ctx).await,
        Some("delete") => run_delete(&ctx).await,
        _ => Ok(()),
    }
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let mut choices: Vec<Choice> = vec![];
    if let Some((name, value)) = ctx.focused()
        && ctx.subcommand().as_deref() == Some("delete")
        && name == "emoji"
    {
        let list = ctx.guild_id().map(|g| guild_emojis(&ctx.app, g).emojis).unwrap_or_default();
        choices = emoji_choices(&list, &value).into_iter().map(|(n, v)| Choice::new(n, v)).collect();
    }
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
