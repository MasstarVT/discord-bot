//! Port of `src/utils/welcomeCard.js`: the 700x250 welcome card.
//!
//! A 700x250 PNG: avatar, "WELCOME TO", server name, display name, @username and member count. Everything
//! user-influenced is clamped and every image is fetched through `safe_fetch_image` (which also refuses decompression
//! bombs via the shared header check). The drawing itself ([`render_welcome_card`]) takes bytes and does no I/O;
//! [`generate_welcome_card`] fetches the avatar and the background first, like the JS function.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use bot_util::image_size::assert_image_dimensions_default;
use bot_util::safe_fetch::{ImageOptions, SafeFetchCode, SafeFetchError, safe_fetch_image};
use bot_util::text::{clamp_text, format_number};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use tiny_skia::{GradientStop, LinearGradient, Pixmap, Point, RadialGradient, SpreadMode, Stroke, Transform};
use tokio::time::{Duration, Instant};

use crate::canvas::{
    self, CardError, circle, clip_mask, decode_image, draw_image, encode_png, fill_path, fill_rect, fit_text, hex_color,
    hex_rgba, is_hex6, shaded, solid, with_alpha,
};
use crate::font::{self, Weight};

pub const CARD_WIDTH: u32 = 700;
pub const CARD_HEIGHT: u32 = 250;
const W: f32 = CARD_WIDTH as f32;
const H: f32 = CARD_HEIGHT as f32;
const AVATAR_X: f32 = 100.0;
const AVATAR_Y: f32 = 125.0;
const AVATAR_R: f32 = 65.0;
const TEXT_X: f32 = 200.0;

/// The colours the card used before it became customisable; `style = None` reproduces it exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyCardColors {
    pub background: &'static str,
    pub background2: &'static str,
    pub accent: &'static str,
    pub text: &'static str,
    pub secondary: &'static str,
    pub count: &'static str,
    pub avatar_fallback: &'static str,
    pub divider: &'static str,
}

/// `LEGACY_CARD_COLORS`.
pub const LEGACY_CARD_COLORS: LegacyCardColors = LegacyCardColors {
    background: "#23272a",
    background2: "#2c2f33",
    accent: "#5865f2",
    text: "#ffffff",
    secondary: "#b9bbbe",
    count: "#57f287",
    avatar_fallback: "#4f545c",
    divider: "#4f545c",
};
const L: LegacyCardColors = LEGACY_CARD_COLORS;

const MAX_BG_BYTES: u64 = 5 * 1024 * 1024;
const MAX_AVATAR_BYTES: u64 = 2 * 1024 * 1024;

/// The style the card is drawn with: welcomeService's `parseCardStyle` result (its `enabled` flag is not read here).
/// Every field is "style value, else the legacy value".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WelcomeCardStyle {
    pub background: Option<String>,
    pub background_url: Option<String>,
    pub text_color: Option<String>,
    pub accent_color: Option<String>,
    pub overlay_opacity: Option<f64>,
}

fn color_or_none(v: &Option<String>) -> Option<String> {
    v.as_deref().filter(|s| is_hex6(s)).map(str::to_ascii_lowercase)
}

/// `/^https?:\/\//i.test(url) && url.length <= 2048`.
fn is_http_prefix(url: &str) -> bool {
    let lower = url.get(..8).map(str::to_ascii_lowercase).unwrap_or_else(|| url.to_ascii_lowercase());
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// `sanitizeStyle`: re-validates a style defensively; anything invalid counts as "not set".
pub fn sanitize_style(style: Option<&WelcomeCardStyle>) -> WelcomeCardStyle {
    let Some(s) = style else { return WelcomeCardStyle::default() };
    let url = s
        .background_url
        .as_deref()
        .filter(|u| is_http_prefix(u) && bot_util::js::utf16_len(u) <= 2048)
        .map(str::to_string);
    WelcomeCardStyle {
        background: color_or_none(&s.background),
        background_url: url,
        text_color: color_or_none(&s.text_color),
        accent_color: color_or_none(&s.accent_color),
        overlay_opacity: s.overlay_opacity.filter(|o| o.is_finite()).map(|o| o.clamp(0.0, 1.0)),
    }
}

// ── Safe image loading ─────────────────────────────────────────────────────────

/// Why a download or decode failed (a `SafeFetchError`, or the decoder's error).
#[derive(Debug, Clone)]
enum LoadError {
    Fetch(SafeFetchError),
    Decode(String),
}

impl LoadError {
    /// `err?.code ?? err?.message ?? 'error'`.
    fn code_or_message(&self) -> String {
        match self {
            Self::Fetch(e) => e.code.as_str().to_string(),
            Self::Decode(m) => m.clone(),
        }
    }
}

/// `loadImageSafe`: downloads through `safe_fetch_image` (SSRF guard, size cap, timeout, header-size guard) and
/// decodes.
async fn load_image_safe(url: &str, timeout_ms: u64, max_bytes: u64) -> Result<Pixmap, LoadError> {
    let opts = ImageOptions { timeout_ms, max_bytes, ..ImageOptions::default() };
    let buf = safe_fetch_image(url, &opts).await.map_err(LoadError::Fetch)?;
    decode_image(&buf).map_err(LoadError::Decode)
}

/// `loadAvatar`: the avatar at `url` (`member.displayAvatarURL({ extension: 'png', forceStatic: true, size: 256 })`),
/// downloaded with a 5 s timeout and a 2 MB cap. Never fails: a missing URL or any error gives `None`, and the
/// fallback circle is drawn instead.
pub async fn load_avatar(url: Option<&str>) -> Option<Vec<u8>> {
    let url = url.filter(|u| !u.is_empty())?;
    let opts = ImageOptions { timeout_ms: 5000, max_bytes: MAX_AVATAR_BYTES, ..ImageOptions::default() };
    let buf = safe_fetch_image(url, &opts).await.ok()?;
    // loadImage(buf) must succeed too, or the JS returns null.
    decode_image(&buf).ok()?;
    Some(buf)
}

// ── Background cache ───────────────────────────────────────────────────────────
// url -> { shared load, expiresAt }. At most 20 entries (oldest evicted first, insertion order); success lives
// 30 minutes, failure 5. Concurrent loads share one future.

/// A background, cover-fitted onto a 700x250 canvas.
pub type CardBackground = Arc<Pixmap>;
type BackgroundFuture = Shared<BoxFuture<'static, Option<CardBackground>>>;

struct CacheEntry {
    id: u64,
    future: BackgroundFuture,
    /// None while the load is in flight (never expires).
    expires_at: Option<Instant>,
}

#[derive(Default)]
struct BgCache {
    next_id: u64,
    entries: HashMap<String, CacheEntry>,
    /// Insertion order, oldest first (the JS Map order).
    order: Vec<String>,
}

impl BgCache {
    fn remove(&mut self, url: &str) {
        if self.entries.remove(url).is_some() {
            self.order.retain(|u| u != url);
        }
    }

    fn store(&mut self, url: &str, id: u64, future: BackgroundFuture, expires_at: Option<Instant>) {
        self.remove(url); // re-insert so the entry counts as the newest
        self.entries.insert(url.to_string(), CacheEntry { id, future, expires_at });
        self.order.push(url.to_string());
        while self.entries.len() > BG_CACHE_MAX {
            let oldest = self.order.remove(0);
            self.entries.remove(&oldest);
        }
    }

    fn set_expiry(&mut self, url: &str, id: u64, expires_at: Instant) {
        if let Some(e) = self.entries.get_mut(url)
            && e.id == id
        {
            e.expires_at = Some(expires_at);
        }
    }
}

static BG_CACHE: LazyLock<Mutex<BgCache>> = LazyLock::new(|| Mutex::new(BgCache::default()));
const BG_CACHE_MAX: usize = 20;
const BG_OK_TTL: Duration = Duration::from_secs(30 * 60);
const BG_FAIL_TTL: Duration = Duration::from_secs(5 * 60);

fn cache() -> std::sync::MutexGuard<'static, BgCache> {
    BG_CACHE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Cover-fits a decoded image onto a 700x250 canvas.
pub fn cover_background(img: &Pixmap) -> Result<Pixmap, CardError> {
    let mut canvas = Pixmap::new(CARD_WIDTH, CARD_HEIGHT).ok_or(CardError::Canvas)?;
    canvas::draw_cover(&mut canvas, img, W, H, None);
    Ok(canvas)
}

/// `fetchBackgroundCanvas`: downloads the image and cover-fits it onto a 700x250 canvas.
async fn fetch_background_canvas(url: &str, timeout_ms: u64) -> Result<Pixmap, LoadError> {
    let img = load_image_safe(url, timeout_ms, MAX_BG_BYTES).await?;
    cover_background(&img).map_err(|e| LoadError::Decode(e.to_string()))
}

/// `loadCardBackground(url, { timeoutMs = 4000 })`: loads a card background (cached). Never fails: a failure
/// resolves to `None` and is remembered for 5 minutes.
pub async fn load_card_background(url: &str, timeout_ms: Option<u64>) -> Option<CardBackground> {
    let timeout_ms = timeout_ms.unwrap_or(4000);
    let future = {
        let mut c = cache();
        if let Some(hit) = c.entries.get(url)
            && hit.expires_at.is_none_or(|t| t > Instant::now())
        {
            hit.future.clone()
        } else {
            c.next_id += 1;
            let id = c.next_id;
            let owned = url.to_string();
            let fut: BoxFuture<'static, Option<CardBackground>> = Box::pin(async move {
                match fetch_background_canvas(&owned, timeout_ms).await {
                    Ok(canvas) => {
                        cache().set_expiry(&owned, id, Instant::now() + BG_OK_TTL);
                        Some(Arc::new(canvas))
                    }
                    Err(err) => {
                        bot_util::log_warn!(format!(
                            "Welcome card background could not be loaded ({}); using the plain background for 5 minutes",
                            err.code_or_message()
                        ));
                        cache().set_expiry(&owned, id, Instant::now() + BG_FAIL_TTL);
                        None
                    }
                }
            });
            let shared = fut.shared();
            c.store(url, id, shared.clone(), None); // in flight until it settles
            // Like a JS promise, the load runs (and settles the entry) even if this caller stops waiting.
            tokio::spawn(shared.clone());
            shared
        }
    };
    future.await
}

/// The `REASONS` table.
fn reason_for_code(code: &str) -> Option<&'static str> {
    Some(match code {
        "INVALID_URL" => "That is not a valid http(s) URL.",
        "BLOCKED_ADDRESS" | "DNS_FAILED" => "That address cannot be reached from the bot.",
        "TIMEOUT" => "The image took too long to download.",
        "TOO_LARGE" => "The image is larger than 5 MB.",
        "BAD_CONTENT_TYPE" => "That URL is not an image.",
        "BAD_IMAGE" | "IMAGE_TOO_LARGE_OR_UNKNOWN" => "Use a PNG, JPEG, GIF or WebP image of at most 8000×8000 pixels.",
        _ => return None,
    })
}

/// Result of [`validate_card_background`]: `{ ok: true }` or `{ ok: false, reason }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundCheck {
    Ok,
    Invalid { reason: String },
}

/// `validateCardBackground(url)`: config-time check of a background URL. Downloads and decodes it (8 s), and on
/// success seeds the cache. Never fails. `url` is `None` for a non-string JS value.
pub async fn validate_card_background(url: Option<&str>) -> BackgroundCheck {
    let invalid = |r: &str| BackgroundCheck::Invalid { reason: r.to_string() };
    let Some(url) = url.filter(|u| is_http_prefix(u) && bot_util::js::utf16_len(u) <= 2048) else {
        return invalid(reason_for_code("INVALID_URL").unwrap_or_default());
    };
    cache().remove(url);
    match fetch_background_canvas(url, 8000).await {
        Ok(canvas) => {
            let bg: CardBackground = Arc::new(canvas);
            let fut: BoxFuture<'static, Option<CardBackground>> = Box::pin(async move { Some(bg) });
            let mut c = cache();
            c.next_id += 1;
            let id = c.next_id;
            c.store(url, id, fut.shared(), Some(Instant::now() + BG_OK_TTL));
            BackgroundCheck::Ok
        }
        Err(LoadError::Fetch(e)) => {
            if e.code == SafeFetchCode::HttpError {
                let status = e.status.map(|s| s.to_string()).unwrap_or_else(|| "error".into());
                return BackgroundCheck::Invalid { reason: format!("The image server answered HTTP {status}.") };
            }
            match reason_for_code(e.code.as_str()) {
                Some(r) => invalid(r),
                None => invalid("I couldn't load that image."),
            }
        }
        // `err?.message && REASONS[err.message]`: a decoder error message is never a REASONS key.
        Err(LoadError::Decode(m)) => match reason_for_code(&m) {
            Some(r) => invalid(r),
            None => invalid("I couldn't load that image."),
        },
    }
}

/// Test hook: the number of cached backgrounds.
pub fn background_cache_size() -> usize {
    cache().entries.len()
}

// ── Card ───────────────────────────────────────────────────────────────────────

/// What the card shows about the member and the guild (the fields the JS reads from discord.js objects).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WelcomeCardData {
    /// `member.displayName`.
    pub display_name: Option<String>,
    /// `member.user.username`.
    pub username: Option<String>,
    /// `guild.name`.
    pub guild_name: Option<String>,
    /// `guild.memberCount` (None prints "0", like `formatNumber(undefined)`).
    pub member_count: Option<f64>,
    /// `member.displayAvatarURL({ extension: 'png', forceStatic: true, size: 256 })`, used by
    /// [`generate_welcome_card`] only.
    pub avatar_url: Option<String>,
}

/// `generateWelcomeCard(member, guild, style)`: fetches the avatar and (cached) background, then draws the card.
/// Fails only on a canvas failure.
pub async fn generate_welcome_card(data: &WelcomeCardData, style: Option<&WelcomeCardStyle>) -> Result<Vec<u8>, CardError> {
    let st = sanitize_style(style);
    let (avatar, bg) = tokio::join!(load_avatar(data.avatar_url.as_deref()), async {
        match st.background_url.as_deref() {
            Some(url) => load_card_background(url, None).await,
            None => None,
        }
    });
    // Drawing and PNG encoding are CPU work: keep them off the async workers (the JS encodes off the main thread).
    let data = data.clone();
    tokio::task::spawn_blocking(move || render_welcome_card(&data, Some(&st), avatar.as_deref(), bg.as_deref()))
        .await
        .map_err(|e| CardError::Encode(e.to_string()))?
}

/// Draws the 700x250 welcome card from already-loaded images: `avatar` is the image file's bytes (an undecodable or
/// oversized image draws the fallback circle), `background` the cover-fitted canvas from [`load_card_background`]
/// (or [`cover_background`]). `style` is sanitized again; `None` reproduces the legacy card.
pub fn render_welcome_card(
    data: &WelcomeCardData,
    style: Option<&WelcomeCardStyle>,
    avatar: Option<&[u8]>,
    background: Option<&Pixmap>,
) -> Result<Vec<u8>, CardError> {
    let st = sanitize_style(style);
    let avatar = avatar.and_then(|b| {
        // safeFetchImage refuses these before the decoder sees them; bytes from elsewhere get the same check.
        assert_image_dimensions_default(b).ok()?;
        decode_image(b).ok()
    });
    let mut pm = Pixmap::new(CARD_WIDTH, CARD_HEIGHT).ok_or(CardError::Canvas)?;

    // ── Background ──────────────────────────────────────────────────────────────
    match st.background.as_deref() {
        Some(bg) => fill_rect(&mut pm, 0.0, 0.0, W, H, &solid(hex_color(bg)), None),
        None => {
            let grad = LinearGradient::new(
                Point::from_xy(0.0, 0.0),
                Point::from_xy(W, 0.0),
                vec![GradientStop::new(0.0, hex_color(L.background)), GradientStop::new(1.0, hex_color(L.background2))],
                SpreadMode::Pad,
                Transform::identity(),
            );
            if let Some(g) = grad {
                fill_rect(&mut pm, 0.0, 0.0, W, H, &shaded(g), None);
            }
        }
    }

    if let Some(bg) = background {
        draw_image(&mut pm, bg, 0.0, 0.0, bg.width() as f32, bg.height() as f32, None);
        let o = st.overlay_opacity.unwrap_or(0.5) as f32;
        fill_rect(&mut pm, 0.0, 0.0, W, H, &solid(tiny_skia::Color::from_rgba(0.0, 0.0, 0.0, o).unwrap_or(tiny_skia::Color::BLACK)), None);
    }

    // ── Accent: glow on the right, bottom bar, avatar ring, server name ─────────
    let accent = st.accent_color.clone().unwrap_or_else(|| L.accent.to_string());
    let glow = RadialGradient::new(
        Point::from_xy(W, H / 2.0),
        Point::from_xy(W, H / 2.0),
        350.0,
        vec![GradientStop::new(0.0, hex_rgba(&accent, 0.18)), GradientStop::new(1.0, tiny_skia::Color::TRANSPARENT)],
        SpreadMode::Pad,
        Transform::identity(),
    );
    if let Some(g) = glow {
        fill_rect(&mut pm, 0.0, 0.0, W, H, &shaded(g), None);
    }

    fill_rect(&mut pm, 0.0, H - 5.0, W, 5.0, &solid(hex_color(&accent)), None);

    if let Some(ring) = circle(AVATAR_X, AVATAR_Y, AVATAR_R + 6.0) {
        fill_path(&mut pm, &ring, &solid(hex_color(&accent)), None);
    }

    if let Some(disc) = circle(AVATAR_X, AVATAR_Y, AVATAR_R) {
        match &avatar {
            Some(img) => {
                let mask = clip_mask(CARD_WIDTH, CARD_HEIGHT, &disc);
                let d = AVATAR_R * 2.0;
                draw_image(&mut pm, img, AVATAR_X - AVATAR_R, AVATAR_Y - AVATAR_R, d, d, mask.as_ref());
            }
            None => fill_path(&mut pm, &disc, &solid(hex_color(L.avatar_fallback)), None),
        }
    }

    // ── Text ────────────────────────────────────────────────────────────────────
    let text_max_width = W - TEXT_X - 20.0;
    let muted_color = st.text_color.clone().unwrap_or_else(|| L.secondary.to_string());
    let muted_alpha = if st.text_color.is_some() { 0.7 } else { 1.0 };
    let muted = solid(with_alpha(hex_color(&muted_color), muted_alpha));

    font::fill_text(&mut pm, "WELCOME TO", 18.0, Weight::Normal, TEXT_X, 75.0, &muted);

    let server_name = welcome_server_name(data.guild_name.as_deref());
    let size = fit_text(&server_name, text_max_width, 26);
    font::fill_text(&mut pm, &server_name, size as f32, Weight::Bold, TEXT_X, 108.0, &solid(hex_color(&accent)));

    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(TEXT_X, 120.0);
    pb.line_to(W - 20.0, 120.0);
    if let Some(line) = pb.finish() {
        let stroke = Stroke { width: 1.0, ..Stroke::default() };
        pm.stroke_path(&line, &solid(hex_color(L.divider)), &stroke, Transform::identity(), None);
    }

    let display_name = welcome_display_name(data);
    let size = fit_text(&display_name, text_max_width, 36);
    let text_color = st.text_color.clone().unwrap_or_else(|| L.text.to_string());
    font::fill_text(&mut pm, &display_name, size as f32, Weight::Bold, TEXT_X, 158.0, &solid(hex_color(&text_color)));

    font::fill_text(&mut pm, &welcome_username(data.username.as_deref()), 18.0, Weight::Normal, TEXT_X, 182.0, &muted);

    let count_color = st.accent_color.clone().unwrap_or_else(|| L.count.to_string());
    let count = format!("Member #{}", format_number(data.member_count.unwrap_or(f64::NAN)));
    font::fill_text(&mut pm, &count, 20.0, Weight::Normal, TEXT_X, 212.0, &solid(hex_color(&count_color)));

    encode_png(&pm)
}

/// The server name as drawn: `clampText(String(guild?.name ?? '').toUpperCase(), 32)`.
pub fn welcome_server_name(guild_name: Option<&str>) -> String {
    clamp_text(&guild_name.unwrap_or("").to_uppercase(), 32)
}

/// The display name as drawn: `clampText(member?.displayName ?? member?.user?.username ?? '', 26)`.
pub fn welcome_display_name(data: &WelcomeCardData) -> String {
    clamp_text(data.display_name.as_deref().or(data.username.as_deref()).unwrap_or(""), 26)
}

/// The username line as drawn: `` `@${clampText(member?.user?.username ?? '', 28)}` ``.
pub fn welcome_username(username: Option<&str>) -> String {
    format!("@{}", clamp_text(username.unwrap_or(""), 28))
}

/// The font size `fitText` picks for the server name (26 down to 14) and the display name (36 down to 14).
pub fn welcome_fit_sizes(data: &WelcomeCardData) -> (u32, u32) {
    let max = W - TEXT_X - 20.0;
    (fit_text(&welcome_server_name(data.guild_name.as_deref()), max, 26), fit_text(&welcome_display_name(data), max, 36))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_style_matches_js() {
        assert_eq!(sanitize_style(None), WelcomeCardStyle::default());
        let s = WelcomeCardStyle {
            background: Some("#ABCDEF".into()),
            background_url: Some("HTTPS://x.test/a.png".into()),
            text_color: Some("nonsense".into()),
            accent_color: Some("#fff".into()),
            overlay_opacity: Some(3.0),
        };
        let out = sanitize_style(Some(&s));
        assert_eq!(out.background.as_deref(), Some("#abcdef"));
        assert_eq!(out.background_url.as_deref(), Some("HTTPS://x.test/a.png"));
        assert_eq!(out.text_color, None);
        assert_eq!(out.accent_color, None);
        assert_eq!(out.overlay_opacity, Some(1.0));
        let s = WelcomeCardStyle { background_url: Some("ftp://x".into()), overlay_opacity: Some(f64::NAN), ..Default::default() };
        let out = sanitize_style(Some(&s));
        assert_eq!(out.background_url, None);
        assert_eq!(out.overlay_opacity, None);
        let s = WelcomeCardStyle { background_url: Some(format!("https://x.test/{}", "a".repeat(2100))), overlay_opacity: Some(-1.0), ..Default::default() };
        let out = sanitize_style(Some(&s));
        assert_eq!(out.background_url, None);
        assert_eq!(out.overlay_opacity, Some(0.0));
    }

    #[test]
    fn cache_evicts_oldest_beyond_twenty() {
        let mut c = BgCache::default();
        for i in 0..25u64 {
            let f: BoxFuture<'static, Option<CardBackground>> = Box::pin(async { None });
            c.store(&format!("u{i}"), i, f.shared(), None);
        }
        assert_eq!(c.entries.len(), 20);
        assert!(!c.entries.contains_key("u4"));
        assert!(c.entries.contains_key("u5"));
        assert_eq!(c.order.first().map(String::as_str), Some("u5"));
    }
}
