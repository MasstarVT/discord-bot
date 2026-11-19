//! The captcha image (`renderCaptchaImage`), drawn the way bot-cards draws its cards: tiny-skia and the embedded
//! Nunito-Bold face. Visual jitter uses a non-crypto random source and never depends on the code; the code itself comes
//! from the crypto RNG ([`super::generate_code`]).

use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use super::{CODE_LENGTH, IMAGE_HEIGHT, IMAGE_WIDTH};

/// `isImageCaptchaAvailable()`: the font is embedded in the binary, so the image mode always works.
pub fn is_image_captcha_available() -> bool {
    bot_cards::font::has_glyph('A')
}

/// CSS `hsla(h, s%, l%, a)` as a colour.
pub fn hsla(h: f32, s: f32, l: f32, a: f32) -> Color {
    let s = s / 100.0;
    let l = l / 100.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    Color::from_rgba((r1 + m).clamp(0.0, 1.0), (g1 + m).clamp(0.0, 1.0), (b1 + m).clamp(0.0, 1.0), a.clamp(0.0, 1.0)).unwrap_or(Color::BLACK)
}

fn paint(c: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    p
}

/// `renderCaptchaImage(code)`: the code with noise and rotation, as a PNG. Fails only when the canvas cannot be
/// allocated or encoded.
pub fn render_captcha_image(code: &str) -> anyhow::Result<Vec<u8>> {
    let (w, h) = (IMAGE_WIDTH as f32, IMAGE_HEIGHT as f32);
    let mut pm = Pixmap::new(IMAGE_WIDTH, IMAGE_HEIGHT).ok_or_else(|| anyhow::anyhow!("canvas allocation failed"))?;
    let r = |a: f32, b: f32| a + rand::random::<f32>() * (b - a);
    let hue = || rand::random_range(0..360u32) as f32;

    pm.fill(hsla(hue(), 40.0, 92.0, 1.0));

    let curve = |pm: &mut Pixmap, width: f32, stroke: Color| {
        let mut pb = PathBuilder::new();
        pb.move_to(r(0.0, 20.0), r(0.0, h));
        pb.cubic_to(r(60.0, 140.0), r(0.0, h), r(180.0, 260.0), r(0.0, h), r(w - 20.0, w), r(0.0, h));
        if let Some(path) = pb.finish() {
            let s = Stroke { width, line_cap: LineCap::Butt, ..Stroke::default() };
            pm.stroke_path(&path, &paint(stroke), &s, Transform::identity(), None);
        }
    };
    for _ in 0..4 {
        curve(&mut pm, r(1.0, 2.0), hsla(hue(), 50.0, 55.0, 0.5));
    }
    for _ in 0..150 {
        let c = Color::from_rgba(0.0, 0.0, 0.0, r(0.1, 0.4)).unwrap_or(Color::BLACK);
        if let Some(rect) = Rect::from_xywh(r(0.0, w), r(0.0, h), 2.0, 2.0) {
            pm.fill_rect(rect, &paint(c), Transform::identity(), None);
        }
    }

    let slot = (w - 40.0) / CODE_LENGTH as f32;
    for (i, ch) in code.chars().enumerate() {
        let size = rand::random_range(46..57u32) as f32;
        let text = ch.to_string();
        // textAlign center, textBaseline middle: the glyph is centred on the origin before the rotation.
        let width = bot_cards::font::measure(&text, size);
        let Some(path) = bot_cards::font::text_path(&text, size, -width / 2.0, 0.0) else { continue };
        let b = path.bounds();
        let dy = -(b.top() + b.bottom()) / 2.0;
        let ts = Transform::from_translate(20.0 + slot * (i as f32 + 0.5) + r(-5.0, 5.0), h / 2.0 + r(-12.0, 12.0))
            .pre_rotate(r(-0.4, 0.4).to_degrees())
            .pre_translate(0.0, dy);
        let fill = paint(hsla(hue(), 60.0, 28.0, 1.0));
        pm.fill_path(&path, &fill, FillRule::Winding, ts, None);
        // `bold`: the outline is grown the way Skia emboldens the variable face (see bot_cards::font::fake_bold_width).
        let s = Stroke { width: bot_cards::font::fake_bold_width(size), ..Stroke::default() };
        pm.stroke_path(&path, &fill, &s, ts, None);
    }

    for _ in 0..2 {
        curve(&mut pm, r(2.0, 3.0), hsla(hue(), 50.0, 30.0, 0.6));
    }
    bot_cards::canvas::encode_png(&pm).map_err(|e| anyhow::anyhow!(e.to_string()))
}

/// [`render_captcha_image`] off the async threads, bounded by `IMAGE_TIMEOUT_MS` (`withTimeout(..., 'captcha image')`).
pub async fn render_captcha_image_async(code: String) -> anyhow::Result<Vec<u8>> {
    let task = tokio::task::spawn_blocking(move || render_captcha_image(&code));
    match tokio::time::timeout(std::time::Duration::from_millis(super::IMAGE_TIMEOUT_MS), task).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(anyhow::anyhow!("captcha image failed: {e}")),
        Err(_) => Err(anyhow::anyhow!("captcha image timed out")),
    }
}
