//! The small part of the HTML canvas API the two cards use, on tiny-skia: colours, paths (`arc`, the cards'
//! quadratic `roundRect`), anti-aliased clips, `drawImage` with scaling, image decoding, PNG encoding, and the two
//! text-fitting helpers (`fitText` from welcomeCard.js, `truncate` from profileCard.js).

use image::ImageReader;
use std::io::Cursor;
use tiny_skia::{
    Color, FillRule, FilterQuality, Mask, Paint, Path, PathBuilder, Pattern, Pixmap, PremultipliedColorU8, Rect, Shader,
    SpreadMode, Transform,
};

use crate::font;

/// Why a card could not be produced (the JS rejects only on a canvas failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardError {
    /// The canvas could not be allocated.
    Canvas,
    /// PNG encoding failed.
    Encode(String),
}

impl std::fmt::Display for CardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canvas => f.write_str("canvas allocation failed"),
            Self::Encode(e) => write!(f, "PNG encoding failed: {e}"),
        }
    }
}

impl std::error::Error for CardError {}

/// `#rrggbb` (validated by the callers' HEX_RE) as (r, g, b). Anything else is black, which never happens for
/// validated input.
pub fn hex_rgb(hex: &str) -> (u8, u8, u8) {
    let n = u32::from_str_radix(hex.get(1..).unwrap_or(""), 16).unwrap_or(0);
    (((n >> 16) & 255) as u8, ((n >> 8) & 255) as u8, (n & 255) as u8)
}

/// `rgba(r,g,b,alpha)` of a `#rrggbb` colour (`hexToRgba` / `rgba` in the JS).
pub fn hex_rgba(hex: &str, alpha: f32) -> Color {
    let (r, g, b) = hex_rgb(hex);
    Color::from_rgba8(r, g, b, alpha_u8(alpha))
}

/// An opaque `#rrggbb` colour.
pub fn hex_color(hex: &str) -> Color {
    hex_rgba(hex, 1.0)
}

/// The colour with its alpha multiplied by `global_alpha` (`ctx.globalAlpha`).
pub fn with_alpha(mut c: Color, global_alpha: f32) -> Color {
    c.apply_opacity(global_alpha);
    c
}

fn alpha_u8(a: f32) -> u8 {
    (a.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// `/^#[0-9a-f]{6}$/i`.
pub fn is_hex6(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7 && b[0] == b'#' && b[1..].iter().all(u8::is_ascii_hexdigit)
}

/// A paint with a solid colour, anti-aliased.
pub fn solid(c: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    p
}

/// A paint with a shader, anti-aliased.
pub fn shaded(shader: Shader<'_>) -> Paint<'_> {
    Paint { shader, anti_alias: true, ..Paint::default() }
}

/// `ctx.fillRect(x, y, w, h)` with `paint`, optionally clipped.
pub fn fill_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, paint: &Paint, mask: Option<&Mask>) {
    if let Some(r) = Rect::from_xywh(x, y, w, h) {
        // Through a path so non-integer rects are anti-aliased the way Skia draws them.
        let path = PathBuilder::from_rect(r);
        pm.fill_path(&path, paint, FillRule::Winding, Transform::identity(), mask);
    }
}

/// `ctx.beginPath(); ctx.arc(cx, cy, r, 0, 2π)`.
pub fn circle(cx: f32, cy: f32, r: f32) -> Option<Path> {
    PathBuilder::from_circle(cx, cy, r)
}

/// profileCard.js `roundRect(ctx, x, y, w, h, r)`: straight edges joined by quadratic curves through the corners.
pub fn round_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

/// `ctx.fill()` of `path`.
pub fn fill_path(pm: &mut Pixmap, path: &Path, paint: &Paint, mask: Option<&Mask>) {
    pm.fill_path(path, paint, FillRule::Winding, Transform::identity(), mask);
}

/// `ctx.clip()` to `path` (anti-aliased, like Skia's canvas clips).
pub fn clip_mask(width: u32, height: u32, path: &Path) -> Option<Mask> {
    let mut m = Mask::new(width, height)?;
    m.fill_path(path, FillRule::Winding, true, Transform::identity());
    Some(m)
}

/// `ctx.drawImage(img, dx, dy, dw, dh)`: the whole image scaled into the destination rectangle (bilinear, the
/// canvas default `imageSmoothingQuality = 'low'`).
pub fn draw_image(pm: &mut Pixmap, img: &Pixmap, dx: f32, dy: f32, dw: f32, dh: f32, mask: Option<&Mask>) {
    if img.width() == 0 || img.height() == 0 || !(dw > 0.0 && dh > 0.0) {
        return;
    }
    let sx = dw / img.width() as f32;
    let sy = dh / img.height() as f32;
    let shader = Pattern::new(
        img.as_ref(),
        SpreadMode::Pad,
        FilterQuality::Bilinear,
        1.0,
        Transform::from_row(sx, 0.0, 0.0, sy, dx, dy),
    );
    let paint = Paint { shader, anti_alias: true, ..Paint::default() };
    if let Some(r) = Rect::from_xywh(dx, dy, dw, dh) {
        let path = PathBuilder::from_rect(r);
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), mask);
    }
}

/// `drawCover`: `img` scaled to fill a `w`x`h` canvas, centred (the overflow is cropped).
pub fn draw_cover(pm: &mut Pixmap, img: &Pixmap, w: f32, h: f32, mask: Option<&Mask>) {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let scale = (w / iw).max(h / ih);
    let dw = iw * scale;
    let dh = ih * scale;
    draw_image(pm, img, (w - dw) / 2.0, (h - dh) / 2.0, dw, dh, mask);
}

/// `loadImage(buffer)`: decodes PNG, JPEG, GIF (first frame) or WebP (the formats
/// `read_image_size` accepts) into a premultiplied pixmap.
pub fn decode_image(bytes: &[u8]) -> Result<Pixmap, String> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let img = reader.decode().map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut pm = Pixmap::new(w, h).ok_or_else(|| "Unsupported image dimensions".to_string())?;
    for (dst, src) in pm.pixels_mut().iter_mut().zip(rgba.pixels()) {
        let [r, g, b, a] = src.0;
        let c = tiny_skia::ColorU8::from_rgba(r, g, b, a).premultiply();
        *dst = PremultipliedColorU8::from_rgba(c.red(), c.green(), c.blue(), c.alpha()).unwrap_or(PremultipliedColorU8::TRANSPARENT);
    }
    Ok(pm)
}

/// `canvas.encode('png')`.
pub fn encode_png(pm: &Pixmap) -> Result<Vec<u8>, CardError> {
    pm.encode_png().map_err(|e| CardError::Encode(e.to_string()))
}

/// welcomeCard.js `fitText`: the largest size, from `font_size` down in steps of 2 (never below 14), at which
/// `text` is at most `max_width` wide.
pub fn fit_text(text: &str, max_width: f32, font_size: u32) -> u32 {
    let mut size = font_size;
    while font::measure(text, size as f32) > max_width && size > 14 {
        size -= 2;
    }
    size
}

/// profileCard.js `truncate`: `text`, or its longest UTF-16 prefix that fits with a trailing `…`. Like
/// `t.slice(0, -1)`, characters are dropped one UTF-16 unit at a time; a split surrogate pair is rendered as U+FFFD.
pub fn truncate(text: &str, size: f32, max_width: f32) -> String {
    if font::measure(text, size) <= max_width {
        return text.to_string();
    }
    let mut units: Vec<u16> = text.encode_utf16().collect();
    loop {
        let candidate = format!("{}…", String::from_utf16_lossy(&units));
        if font::measure(&candidate, size) <= max_width || units.is_empty() {
            return candidate;
        }
        units.pop();
    }
}
