//! The embedded Nunito-Bold face: shaping (rustybuzz, so kerning and ligatures match Skia's text layout), measuring
//! (`ctx.measureText(text).width`) and drawing (`ctx.fillText(text, x, y)` with the alphabetic baseline and left
//! alignment, the canvas defaults).
//!
//! The JS registers `assets/fonts/Nunito-Bold.ttf` as "Nunito". The deployed image has no system fonts, so a character
//! the font has no glyph for has nothing to fall back to: Skia draws the font's `.notdef` glyph (glyph 0) with its
//! advance. rustybuzz maps such characters to glyph 0 as well, and this module measures and draws it the same way.

use std::sync::OnceLock;

use rustybuzz::ttf_parser::{GlyphId, OutlineBuilder};
use rustybuzz::{Face, UnicodeBuffer};
use tiny_skia::{FillRule, LineJoin, Mask, MaskType, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

/// The bundled font file (a copy of `assets/fonts/Nunito-Bold.ttf`).
pub static NUNITO_BOLD: &[u8] = include_bytes!("../assets/Nunito-Bold.ttf");

fn face() -> &'static Face<'static> {
    static FACE: OnceLock<Face<'static>> = OnceLock::new();
    FACE.get_or_init(|| Face::from_slice(NUNITO_BOLD, 0).expect("embedded Nunito-Bold.ttf is a valid font"))
}

/// One shaped glyph, in font units.
#[derive(Debug, Clone, Copy)]
struct ShapedGlyph {
    id: u16,
    x_advance: i32,
    x_offset: i32,
    y_offset: i32,
}

fn shape(text: &str) -> Vec<ShapedGlyph> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut buf = UnicodeBuffer::new();
    buf.push_str(text);
    buf.guess_segment_properties();
    let out = rustybuzz::shape(face(), &[], buf);
    out.glyph_infos()
        .iter()
        .zip(out.glyph_positions())
        .map(|(info, pos)| ShapedGlyph {
            id: info.glyph_id as u16,
            x_advance: pos.x_advance,
            x_offset: pos.x_offset,
            y_offset: pos.y_offset,
        })
        .collect()
}

fn units_per_em() -> f32 {
    face().units_per_em() as f32
}

/// `ctx.measureText(text).width` with the font at `size` px.
pub fn measure(text: &str, size: f32) -> f32 {
    let total: i64 = shape(text).iter().map(|g| i64::from(g.x_advance)).sum();
    total as f32 * size / units_per_em()
}

/// Whether the font has a glyph for `c` (otherwise `.notdef` is drawn).
pub fn has_glyph(c: char) -> bool {
    face().glyph_index(c).is_some()
}

struct Outline<'a> {
    pb: &'a mut PathBuilder,
    scale: f32,
    x: f32,
    y: f32,
}

impl Outline<'_> {
    fn p(&self, x: f32, y: f32) -> (f32, f32) {
        // Font units are y-up; the canvas is y-down with the baseline at self.y.
        (self.x + x * self.scale, self.y - y * self.scale)
    }
}

impl OutlineBuilder for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (a, b) = self.p(x, y);
        self.pb.move_to(a, b);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (a, b) = self.p(x, y);
        self.pb.line_to(a, b);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (a, b) = self.p(x1, y1);
        let (c, d) = self.p(x, y);
        self.pb.quad_to(a, b, c, d);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (a, b) = self.p(x1, y1);
        let (c, d) = self.p(x2, y2);
        let (e, f) = self.p(x, y);
        self.pb.cubic_to(a, b, c, d, e, f);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

/// The outline of `text` laid out at (`x`, `y`) (alphabetic baseline, left aligned), or None when nothing is visible.
pub fn text_path(text: &str, size: f32, x: f32, y: f32) -> Option<Path> {
    let scale = size / units_per_em();
    let mut pb = PathBuilder::new();
    let mut pen = 0i64;
    for g in shape(text) {
        let gx = x + (pen + i64::from(g.x_offset)) as f32 * scale;
        let gy = y - g.y_offset as f32 * scale;
        let mut ob = Outline { pb: &mut pb, scale, x: gx, y: gy };
        face().outline_glyph(GlyphId(g.id), &mut ob);
        pen += i64::from(g.x_advance);
    }
    pb.finish()
}

/// The weight the canvas font string asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// `"18px Nunito"`: drawn as the face is.
    Normal,
    /// `"bold 18px Nunito"`.
    Bold,
}

/// The outline growth Skia's synthetic bold adds at `size` px (the full stroke width; half on each side).
///
/// The bundled `Nunito-Bold.ttf` is in fact the variable Nunito font, whose default instance is ExtraLight (wght 200,
/// `usWeightClass` 200). Skia does not move the `wght` axis for a `bold` font string: it sees a 200-weight face for a
/// 700 request and emboldens the outlines (FreeType's `FT_Outline_Embolden` with strength `size / 24`), without
/// changing advances. That is why `measureText` is the same for "bold" and normal, and why bold text is visibly
/// heavier in the JS cards.
pub fn fake_bold_width(size: f32) -> f32 {
    size / 24.0
}

/// `ctx.fillText(text, x, y)`: fills the glyph outlines (anti-aliased, non-zero winding) with `paint`; with
/// [`Weight::Bold`] the outlines are grown by [`fake_bold_width`] (stroke-and-fill, miter joins).
pub fn fill_text(pixmap: &mut Pixmap, text: &str, size: f32, weight: Weight, x: f32, y: f32, paint: &Paint) {
    let Some(path) = text_path(text, size, x, y) else { return };
    if weight == Weight::Normal {
        pixmap.fill_path(&path, paint, FillRule::Winding, Transform::identity(), None);
        return;
    }
    // Draw fill and stroke as one coverage mask so a translucent paint is not applied twice where they overlap.
    let (w, h) = (pixmap.width(), pixmap.height());
    let Some(mut cover) = Pixmap::new(w, h) else { return };
    let white = {
        let mut p = Paint::default();
        p.set_color(tiny_skia::Color::WHITE);
        p.anti_alias = true;
        p
    };
    cover.fill_path(&path, &white, FillRule::Winding, Transform::identity(), None);
    let stroke = Stroke { width: fake_bold_width(size), line_join: LineJoin::Miter, miter_limit: 4.0, ..Stroke::default() };
    cover.stroke_path(&path, &white, &stroke, Transform::identity(), None);
    let glyph_mask = Mask::from_pixmap(cover.as_ref(), MaskType::Alpha);
    let Some(full) = tiny_skia::Rect::from_xywh(0.0, 0.0, w as f32, h as f32) else { return };
    pixmap.fill_rect(full, paint, Transform::identity(), Some(&glyph_mask));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_loads_and_has_ellipsis() {
        assert!(units_per_em() > 0.0);
        assert!(has_glyph('…'));
        assert!(has_glyph('A'));
    }
}
