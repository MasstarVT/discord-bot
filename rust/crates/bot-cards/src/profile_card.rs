//! Port of `src/utils/profileCard.js`: the 900x280 rank card, its style (`GuildSettings.rankCardStyle`) and the
//! background image cache. [`generate_profile_card`] draws from bytes and does no I/O; [`load_card_background`]
//! downloads (through `safe_fetch_image`) and caches the cover-cropped background PNG it takes.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use bot_util::image_size::assert_image_dimensions_default;
use bot_util::safe_fetch::{ImageOptions, SafeFetchCode, safe_fetch_image};
use bot_util::text::{compact_number, format_number};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde_json::Value;
use tiny_skia::{GradientStop, LinearGradient, Pixmap, Point, SpreadMode, Transform};
use tokio::time::{Duration, Instant};

use crate::canvas::{
    CardError, circle, clip_mask, decode_image, draw_cover, encode_png, fill_path, fill_rect, hex_color, hex_rgb, hex_rgba,
    is_hex6, round_rect, shaded, solid, truncate,
};
use crate::font::{self, Weight};

pub const CARD_WIDTH: u32 = 900;
pub const CARD_HEIGHT: u32 = 280;
const W: f32 = CARD_WIDTH as f32;
const H: f32 = CARD_HEIGHT as f32;

const GREY: &str = "#B9BBBE";
const BAR_BG: &str = "#3f4147";
const GREEN: &str = "#57F287";
const AVATAR_FALLBACK: &str = "#40444b";

const BACKGROUND_MAX_BYTES: u64 = 8 * 1024 * 1024;
const BACKGROUND_TIMEOUT_MS: u64 = 8000;
const BACKGROUND_TTL: Duration = Duration::from_secs(30 * 60);
const BACKGROUND_NEGATIVE_TTL: Duration = Duration::from_secs(10 * 60);
const BACKGROUND_CACHE_MAX: usize = 50;

// ── Card style (GuildSettings.rankCardStyle) ──────────────────────────────────

/// A normalized rank card style (`normalizeCardStyle`'s result).
#[derive(Debug, Clone, PartialEq)]
pub struct RankCardStyle {
    pub background: String,
    pub background_url: Option<String>,
    pub accent_color: String,
    pub text_color: String,
    pub overlay_opacity: f64,
}

impl Default for RankCardStyle {
    fn default() -> Self {
        default_rank_card_style()
    }
}

/// `DEFAULT_RANK_CARD_STYLE`.
pub fn default_rank_card_style() -> RankCardStyle {
    RankCardStyle {
        background: "#2B2D31".into(),
        background_url: None,
        accent_color: "#5865F2".into(),
        text_color: "#FFFFFF".into(),
        overlay_opacity: 0.5,
    }
}

impl RankCardStyle {
    /// The JSON shape the JS object has (`{ background, backgroundUrl, accentColor, textColor, overlayOpacity }`).
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "background": self.background,
            "backgroundUrl": self.background_url,
            "accentColor": self.accent_color,
            "textColor": self.text_color,
            "overlayOpacity": self.overlay_opacity,
        })
    }

    /// `normalizeCardStyle(this)`: the same checks applied to an already typed style.
    pub fn normalized(&self) -> RankCardStyle {
        normalize_card_style(&self.to_json())
    }
}

/// customEmbed.js `isHttpUrl`: at most `LIMITS.url` (2048) UTF-16 units, no whitespace, a WHATWG-parsable
/// http(s) URL with a host.
pub fn is_http_url(s: &str) -> bool {
    if bot_util::js::utf16_len(s) > 2048 || s.chars().any(bot_util::js::is_js_space) {
        return false;
    }
    match url::Url::parse(s) {
        Ok(u) => matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty()),
        Err(_) => false,
    }
}

/// `normalizeCardStyle(json)`: fills the defaults of a stored card style, key by key. Colours must be #RRGGBB (stored
/// upper-case), the image URL an http(s) URL, the overlay a number in 0..0.9. Pure.
pub fn normalize_card_style(json: &Value) -> RankCardStyle {
    let d = default_rank_card_style();
    let Some(src) = json.as_object() else { return d };
    let colour = |key: &str, fallback: String| match src.get(key).and_then(Value::as_str) {
        Some(v) if is_hex6(v) => v.to_ascii_uppercase(),
        _ => fallback,
    };
    let overlay = src.get("overlayOpacity").and_then(Value::as_f64).filter(|o| o.is_finite() && (0.0..=0.9).contains(o));
    RankCardStyle {
        background: colour("background", d.background),
        background_url: src.get("backgroundUrl").and_then(Value::as_str).filter(|u| is_http_url(u)).map(str::to_string),
        accent_color: colour("accentColor", d.accent_color),
        text_color: colour("textColor", d.text_color),
        overlay_opacity: overlay.unwrap_or(d.overlay_opacity),
    }
}

// ── Background images ─────────────────────────────────────────────────────────

/// A cached background: the cover-cropped 900x280 PNG, or None for a remembered failure.
type CachedPng = Option<Arc<Vec<u8>>>;
type LoadFuture = Shared<BoxFuture<'static, CachedPng>>;

#[derive(Default)]
struct BgCache {
    /// url -> (png, expiresAt).
    entries: HashMap<String, (CachedPng, Instant)>,
    /// Insertion order = age, so the first key is evicted first.
    order: Vec<String>,
    /// url -> in-flight load, so a burst of /rank calls shares one download.
    loading: HashMap<String, LoadFuture>,
}

impl BgCache {
    fn remove(&mut self, url: &str) {
        if self.entries.remove(url).is_some() {
            self.order.retain(|u| u != url);
        }
    }

    fn put(&mut self, url: &str, png: CachedPng, ttl: Duration) {
        self.remove(url);
        self.entries.insert(url.to_string(), (png, Instant::now() + ttl));
        self.order.push(url.to_string());
        while self.entries.len() > BACKGROUND_CACHE_MAX {
            let oldest = self.order.remove(0);
            self.entries.remove(&oldest);
        }
    }
}

static BG_CACHE: LazyLock<Mutex<BgCache>> = LazyLock::new(|| Mutex::new(BgCache::default()));

fn cache() -> std::sync::MutexGuard<'static, BgCache> {
    BG_CACHE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Why a background could not be used: a safeFetch error, the dimension check, or the decoder.
#[derive(Debug, Clone)]
enum BgError {
    Fetch { code: SafeFetchCode, status: Option<u16>, message: String },
    ImageTooLargeOrUnknown,
    Decode(String),
}

impl BgError {
    /// `err?.message`.
    fn message(&self) -> String {
        match self {
            Self::Fetch { message, .. } => message.clone(),
            Self::ImageTooLargeOrUnknown => "IMAGE_TOO_LARGE_OR_UNKNOWN".into(),
            Self::Decode(m) => m.clone(),
        }
    }
}

async fn fetch_image_checked(url: &str) -> Result<(Vec<u8>, Pixmap), BgError> {
    let opts = ImageOptions { max_bytes: BACKGROUND_MAX_BYTES, timeout_ms: BACKGROUND_TIMEOUT_MS, ..ImageOptions::default() };
    let buffer = safe_fetch_image(url, &opts)
        .await
        .map_err(|e| BgError::Fetch { code: e.code, status: e.status, message: e.message.clone() })?;
    // Refuse a decompression bomb before the decoder sees it (D4).
    assert_image_dimensions_default(&buffer).map_err(|_| BgError::ImageTooLargeOrUnknown)?;
    let img = decode_image(&buffer).map_err(BgError::Decode)?;
    Ok((buffer, img))
}

/// Cover-crops a decoded image to the card size and encodes it as PNG (what the cache holds).
pub fn cover_background_png(img: &Pixmap) -> Result<Vec<u8>, CardError> {
    let mut canvas = Pixmap::new(CARD_WIDTH, CARD_HEIGHT).ok_or(CardError::Canvas)?;
    draw_cover(&mut canvas, img, W, H, None);
    encode_png(&canvas)
}

async fn fetch_background(url: &str) -> Result<Vec<u8>, BgError> {
    let (_, img) = fetch_image_checked(url).await?;
    // Encoding a large crop is CPU work; keep it off the async worker like the JS keeps it off the main thread.
    tokio::task::spawn_blocking(move || cover_background_png(&img))
        .await
        .map_err(|e| BgError::Decode(e.to_string()))?
        .map_err(|e| BgError::Decode(e.to_string()))
}

/// `loadCardBackground(url)`: downloads and cover-crops a background image to the card size, cached for 30 minutes
/// (a failure is cached for 10). Never fails. `url` is `None` for a non-string JS value.
pub async fn load_card_background(url: Option<&str>) -> Option<Arc<Vec<u8>>> {
    let url = url.filter(|u| !u.is_empty())?;
    let future = {
        let mut c = cache();
        if let Some((png, expires_at)) = c.entries.get(url)
            && *expires_at > Instant::now()
        {
            return png.clone();
        }
        if let Some(pending) = c.loading.get(url) {
            pending.clone()
        } else {
            let owned = url.to_string();
            let fut: BoxFuture<'static, CachedPng> = Box::pin(async move {
                let out = match fetch_background(&owned).await {
                    Ok(png) => {
                        let png = Arc::new(png);
                        cache().put(&owned, Some(png.clone()), BACKGROUND_TTL);
                        Some(png)
                    }
                    Err(err) => {
                        bot_util::log_debug!(format!("rank card background {owned} not loaded: {}", err.message()));
                        cache().put(&owned, None, BACKGROUND_NEGATIVE_TTL);
                        None
                    }
                };
                cache().loading.remove(&owned);
                out
            });
            let shared = fut.shared();
            c.loading.insert(url.to_string(), shared.clone());
            // Like a JS promise, the load runs (and fills the cache) even if this caller stops waiting.
            tokio::spawn(shared.clone());
            shared
        }
    };
    future.await
}

/// `clearCardBackground(url)`: forgets a cached background (after the style changed).
pub fn clear_card_background(url: Option<&str>) {
    if let Some(u) = url.filter(|u| !u.is_empty()) {
        cache().remove(u);
    }
}

/// The number of cached backgrounds (successes and remembered failures).
pub fn background_cache_size() -> usize {
    cache().entries.len()
}

/// Result of [`validate_card_background`]: `{ ok: true }` or `{ ok: false, reason }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundCheck {
    Ok,
    Invalid { reason: String },
}

/// `FAILURE_REASONS`.
fn failure_reason(code: SafeFetchCode) -> Option<&'static str> {
    Some(match code {
        SafeFetchCode::Timeout => "it took too long",
        SafeFetchCode::TooLarge => "it is larger than 8 MB",
        SafeFetchCode::BadContentType => "it is not an image",
        SafeFetchCode::BlockedAddress | SafeFetchCode::InvalidUrl => "that address is not allowed",
        _ => return None,
    })
}

/// `validateCardBackground(url)`: checks that a URL serves a usable image. Never fails.
pub async fn validate_card_background(url: &str) -> BackgroundCheck {
    match fetch_image_checked(url).await {
        Ok(_) => BackgroundCheck::Ok,
        Err(BgError::Fetch { code: SafeFetchCode::HttpError, status, .. }) => {
            let status = status.map(|s| s.to_string()).unwrap_or_else(|| "undefined".into());
            BackgroundCheck::Invalid { reason: format!("the server answered {status}") }
        }
        Err(BgError::Fetch { code, .. }) if failure_reason(code).is_some() => {
            BackgroundCheck::Invalid { reason: failure_reason(code).unwrap_or_default().to_string() }
        }
        Err(_) => BackgroundCheck::Invalid { reason: "it could not be read".into() },
    }
}

// ── Colour helpers ────────────────────────────────────────────────────────────

/// `lighten(hex, amount)`: mixes `hex` with white (0 = the colour, 1 = white), as `#rrggbb`.
pub fn lighten(hex: &str, amount: f64) -> String {
    let (r, g, b) = hex_rgb(hex);
    let mix = |c: u8| {
        let c = f64::from(c);
        (c + (255.0 - c) * amount + 0.5).floor() as i64 // Math.round
    };
    format!("#{:02x}{:02x}{:02x}", mix(r), mix(g), mix(b))
}

/// `levelProgress(totalXp)`: XP into the current level and XP needed for the next, computed locally.
pub fn level_progress(total_xp: f64) -> (f64, f64) {
    let xp_for = |l: i64| bot_config::xp_for_level(l) as f64;
    let mut level = 0i64;
    // Math.max(0, Number(totalXp) || 0): NaN becomes 0.
    let mut remaining = if total_xp.is_nan() { 0.0 } else { total_xp.max(0.0) };
    while level < 2000 && remaining >= xp_for(level) {
        remaining -= xp_for(level);
        level += 1;
    }
    (remaining, xp_for(level))
}

// ── Card ──────────────────────────────────────────────────────────────────────

/// The options of `generateProfileCard`.
#[derive(Debug, Clone, Default)]
pub struct ProfileCardOptions<'a> {
    /// The avatar image file (PNG, JPEG, GIF, WebP); `None` or an unusable image draws the placeholder.
    pub avatar_buffer: Option<&'a [u8]>,
    pub username: &'a str,
    pub display_name: &'a str,
    pub level: f64,
    pub total_xp: f64,
    pub rank: f64,
    /// `xpCurrent`; None (or not finite) uses the value computed from `total_xp`.
    pub xp_current: Option<f64>,
    /// `xpNeeded`; None, not finite or <= 0 uses the value computed from `total_xp`.
    pub xp_needed: Option<f64>,
    /// None is `DEFAULT_RANK_CARD_STYLE`.
    pub style: Option<&'a RankCardStyle>,
    /// The cached background PNG from [`load_card_background`] (any image file works; it is cover-cropped).
    pub background_buffer: Option<&'a [u8]>,
}

/// The text the card draws, after fitting (exposed for tests).
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileCardText {
    pub display_name: String,
    pub username: String,
    pub level_label: String,
    pub rank_label: String,
    pub xp_text: String,
    pub total_text: String,
    pub progress: f64,
}

/// The fitted display name (28 px, 380 px wide) and `@username` (18 px, 320 px wide) lines.
pub fn profile_names(display_name: &str, username: &str) -> (String, String) {
    (truncate(display_name, 28.0, 380.0), format!("@{}", truncate(username, 18.0, 320.0)))
}

/// Everything the card writes, computed like the JS.
pub fn profile_card_text(o: &ProfileCardOptions<'_>) -> ProfileCardText {
    let (display_name, username) = profile_names(o.display_name, o.username);
    let (fb_current, fb_needed) = level_progress(o.total_xp);
    let current = o.xp_current.filter(|v| v.is_finite()).unwrap_or(fb_current);
    let needed = o.xp_needed.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(fb_needed);
    let p = current / needed;
    // Math.min(Math.max(p, 0), 1): NaN stays NaN and then counts as "no progress".
    let progress = if p.is_nan() { f64::NAN } else { p.clamp(0.0, 1.0) };
    ProfileCardText {
        display_name,
        username,
        level_label: format!("LEVEL {}", bot_util::js::number_to_string(o.level)),
        rank_label: format!("RANK #{}", format_number(o.rank)),
        xp_text: format!("{} / {} XP", compact_number(current), compact_number(needed)),
        total_text: format!("Total: {} XP", compact_number(o.total_xp)),
        progress,
    }
}

/// `generateProfileCard(opts)`: the 900x280 rank card PNG. Fails only on a canvas failure; a bad avatar or
/// background is skipped.
pub fn generate_profile_card(o: &ProfileCardOptions<'_>) -> Result<Vec<u8>, CardError> {
    let st = match o.style {
        Some(s) => s.normalized(),
        None => default_rank_card_style(),
    };
    let text = profile_card_text(o);
    let mut pm = Pixmap::new(CARD_WIDTH, CARD_HEIGHT).ok_or(CardError::Canvas)?;
    let card_shape = round_rect(0.0, 0.0, W, H, 16.0);

    // ── Background ─────────────────────────────────────────────────────────────
    if let Some(shape) = &card_shape {
        fill_path(&mut pm, shape, &solid(hex_color(&st.background)), None);
    }

    if let Some(buf) = o.background_buffer {
        // Never hand an oversized image to the decoder (D4).
        let decoded = assert_image_dimensions_default(buf)
            .map_err(|_| "IMAGE_TOO_LARGE_OR_UNKNOWN".to_string())
            .and_then(|_| decode_image(buf));
        match decoded {
            Ok(bg) => {
                let mask = card_shape.as_ref().and_then(|s| clip_mask(CARD_WIDTH, CARD_HEIGHT, s));
                draw_cover(&mut pm, &bg, W, H, mask.as_ref());
                let overlay = tiny_skia::Color::from_rgba(0.0, 0.0, 0.0, st.overlay_opacity as f32).unwrap_or(tiny_skia::Color::BLACK);
                fill_rect(&mut pm, 0.0, 0.0, W, H, &solid(overlay), mask.as_ref());
            }
            Err(msg) => bot_util::log_debug!(format!("rank card background skipped: {msg}")),
        }
    }

    // Subtle left accent gradient
    let grad = LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(220.0, 0.0),
        vec![GradientStop::new(0.0, hex_rgba(&st.accent_color, 0.25)), GradientStop::new(1.0, hex_rgba(&st.accent_color, 0.0))],
        SpreadMode::Pad,
        Transform::identity(),
    );
    if let (Some(g), Some(shape)) = (grad, &card_shape) {
        fill_path(&mut pm, shape, &shaded(g), None);
    }

    // ── Avatar ─────────────────────────────────────────────────────────────────
    const AX: f32 = 36.0;
    const AY: f32 = 80.0;
    const AR: f32 = 60.0; // left x, top y, radius

    // Accent ring
    if let Some(ring) = circle(AX + AR, AY + AR, AR + 5.0) {
        fill_path(&mut pm, &ring, &solid(hex_color(&st.accent_color)), None);
    }

    // Clip circle for avatar
    if let Some(disc) = circle(AX + AR, AY + AR, AR) {
        let mask = clip_mask(CARD_WIDTH, CARD_HEIGHT, &disc);
        let img = o.avatar_buffer.and_then(|b| {
            assert_image_dimensions_default(b).ok()?;
            decode_image(b).ok()
        });
        match img {
            Some(img) => crate::canvas::draw_image(&mut pm, &img, AX, AY, AR * 2.0, AR * 2.0, mask.as_ref()),
            None => fill_rect(&mut pm, AX, AY, AR * 2.0, AR * 2.0, &solid(hex_color(AVATAR_FALLBACK)), mask.as_ref()),
        }
    }

    // ── Name block ─────────────────────────────────────────────────────────────
    let name_x = AX + AR * 2.0 + 24.0;
    font::fill_text(&mut pm, &text.display_name, 28.0, Weight::Bold, name_x, 118.0, &solid(hex_color(&st.text_color)));
    font::fill_text(&mut pm, &text.username, 18.0, Weight::Bold, name_x, 148.0, &solid(hex_color(GREY)));

    // ── Level + Rank (top right) ───────────────────────────────────────────────
    let level_w = font::measure(&text.level_label, 32.0);
    font::fill_text(&mut pm, &text.level_label, 32.0, Weight::Bold, W - level_w - 28.0, 90.0, &solid(hex_color(&st.accent_color)));
    let rank_w = font::measure(&text.rank_label, 18.0);
    font::fill_text(&mut pm, &text.rank_label, 18.0, Weight::Bold, W - rank_w - 28.0, 120.0, &solid(hex_color(GREY)));

    // ── XP progress bar ────────────────────────────────────────────────────────
    let bar_x = name_x;
    let bar_y = 176.0;
    let bar_w = W - name_x - 28.0;
    let bar_h = 24.0;

    // Track
    if let Some(track) = round_rect(bar_x, bar_y, bar_w, bar_h, bar_h / 2.0) {
        fill_path(&mut pm, &track, &solid(hex_color(BAR_BG)), None);
    }

    // Fill
    if text.progress > 0.0 {
        let fill_w = bar_h.max((f64::from(bar_w) * text.progress).floor() as f32); // min width keeps ends round
        let grad = LinearGradient::new(
            Point::from_xy(bar_x, 0.0),
            Point::from_xy(bar_x + fill_w, 0.0),
            vec![
                GradientStop::new(0.0, hex_color(&st.accent_color)),
                GradientStop::new(1.0, hex_color(&lighten(&st.accent_color, 0.3))),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        );
        if let (Some(g), Some(bar)) = (grad, round_rect(bar_x, bar_y, fill_w, bar_h, bar_h / 2.0)) {
            fill_path(&mut pm, &bar, &shaded(g), None);
        }
    }

    // XP label
    font::fill_text(&mut pm, &text.xp_text, 16.0, Weight::Bold, bar_x, bar_y + bar_h + 22.0, &solid(hex_color(&st.text_color)));

    // Total XP (right-aligned)
    let total_w = font::measure(&text.total_text, 16.0);
    font::fill_text(&mut pm, &text.total_text, 16.0, Weight::Bold, bar_x + bar_w - total_w, bar_y + bar_h + 22.0, &solid(hex_color(GREEN)));

    encode_png(&pm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Port of test/levels.test.js "normalizeCardStyle fills defaults per key and never mutates".
    #[test]
    fn normalize_card_style_matches_js() {
        let d = default_rank_card_style();
        assert_eq!(normalize_card_style(&Value::Null), d);
        assert_eq!(normalize_card_style(&json!([1, 2])), d);
        assert_eq!(
            normalize_card_style(&json!({ "background": "#abcdef", "overlayOpacity": 2, "backgroundUrl": "ftp://x" })),
            RankCardStyle { background: "#ABCDEF".into(), background_url: None, accent_color: "#5865F2".into(), text_color: "#FFFFFF".into(), overlay_opacity: 0.5 }
        );
        assert_eq!(
            normalize_card_style(&json!({ "accentColor": "#ff0000", "textColor": "red", "overlayOpacity": 0.9, "backgroundUrl": "https://img.example.test/a.png" })),
            RankCardStyle {
                background: "#2B2D31".into(),
                background_url: Some("https://img.example.test/a.png".into()),
                accent_color: "#FF0000".into(),
                text_color: "#FFFFFF".into(),
                overlay_opacity: 0.9,
            }
        );
        assert_eq!(normalize_card_style(&json!({ "overlayOpacity": -0.1 })).overlay_opacity, 0.5);
        assert_eq!(normalize_card_style(&json!({ "overlayOpacity": 0 })).overlay_opacity, 0.0);
        assert_eq!(normalize_card_style(&json!({ "overlayOpacity": "0.3" })).overlay_opacity, 0.5);
        assert_eq!(normalize_card_style(&json!({ "background": "#fff" })).background, "#2B2D31", "short hex is rejected");
        assert_eq!(normalize_card_style(&json!({ "backgroundUrl": "https://a b.test/x" })).background_url, None);
        // new URL("http:///x") has the host "x".
        assert_eq!(normalize_card_style(&json!({ "backgroundUrl": "http:///x" })).background_url.as_deref(), Some("http:///x"));
        assert_eq!(d.normalized(), d);
    }

    #[test]
    fn lighten_and_level_progress() {
        assert_eq!(lighten("#5865F2", 0.3), "#8a93f6");
        assert_eq!(lighten("#000000", 0.5), "#808080");
        assert_eq!(lighten("#FFFFFF", 0.3), "#ffffff");
        assert_eq!(level_progress(0.0), (0.0, 100.0));
        assert_eq!(level_progress(f64::NAN), (0.0, 100.0));
        assert_eq!(level_progress(-5.0), (0.0, 100.0));
        assert_eq!(level_progress(100.0), (0.0, 155.0));
        assert_eq!(level_progress(18_740.0), (365.0, 2620.0)); // node: level 18, 365 left, 2620 needed
    }

    #[test]
    fn card_text_uses_fallbacks_like_js() {
        let o = ProfileCardOptions { username: "a", display_name: "A", level: 0.0, total_xp: 150.0, rank: 1234.0, ..Default::default() };
        let t = profile_card_text(&o);
        assert_eq!(t.xp_text, "50 / 155 XP");
        assert_eq!(t.rank_label, "RANK #1,234");
        assert_eq!(t.level_label, "LEVEL 0");
        assert_eq!(t.total_text, "Total: 150 XP");
        let o = ProfileCardOptions { xp_current: Some(1240.0), xp_needed: Some(0.0), total_xp: 0.0, ..o };
        assert_eq!(profile_card_text(&o).xp_text, "1.2K / 100 XP");
        assert_eq!(profile_card_text(&o).progress, 1.0);
    }
}
