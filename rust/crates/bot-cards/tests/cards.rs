//! Card rendering tests. Reference data comes from the JS renderers: `rust/fixtures/gen/bot-cards-fit.mjs`
//! (text widths, fitText sizes, truncate results) and `rust/fixtures/gen/bot-cards-render.mjs` (reference PNGs, the
//! demo avatar and a background image). Every rendered card is also written to
//! `rust/target-agents/bot-cards/samples/` for a visual comparison with the JS references.

use std::path::PathBuf;

use bot_cards::canvas::decode_image;
use bot_cards::profile_card::{self, ProfileCardOptions, RankCardStyle, normalize_card_style};
use bot_cards::welcome_card::{self, WelcomeCardData, WelcomeCardStyle, render_welcome_card};
use bot_cards::{font, generate_profile_card};
use serde_json::{Value, json};
use tiny_skia::Pixmap;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/bot-cards")
}

fn golden(name: &str) -> Vec<u8> {
    std::fs::read(golden_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn fit_json() -> Value {
    serde_json::from_slice(&golden("fit.json")).expect("fit.json")
}

fn save_sample(name: &str, png: &[u8]) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target-agents/bot-cards/samples");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(format!("{name}.png")), png);
}

/// Decodes a PNG and checks the signature and size.
fn decode_png(png: &[u8], w: u32, h: u32) -> Pixmap {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
    let size = bot_util::image_size::read_image_size(png).expect("readable PNG header");
    assert_eq!((size.width, size.height), (u64::from(w), u64::from(h)));
    let pm = decode_image(png).expect("decodable PNG");
    assert_eq!((pm.width(), pm.height()), (w, h));
    pm
}

fn rgba(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
    let c = pm.pixel(x, y).expect("in bounds").demultiply();
    [c.red(), c.green(), c.blue(), c.alpha()]
}

/// Number of pixels in the box whose colour differs from `bg` by more than `tol` in some channel.
fn differing(pm: &Pixmap, (x0, y0, x1, y1): (u32, u32, u32, u32), bg: [u8; 4], tol: u8) -> usize {
    let mut n = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = rgba(pm, x, y);
            if p.iter().zip(bg).any(|(a, b)| a.abs_diff(b) > tol) {
                n += 1;
            }
        }
    }
    n
}

/// Mean absolute channel difference (0..255) between two same-sized images, alpha included.
fn mean_abs_diff(a: &Pixmap, b: &Pixmap) -> f64 {
    assert_eq!((a.width(), a.height()), (b.width(), b.height()));
    let mut sum = 0u64;
    let mut n = 0u64;
    for y in 0..a.height() {
        for x in 0..a.width() {
            for (p, q) in rgba(a, x, y).iter().zip(rgba(b, x, y)) {
                sum += u64::from(p.abs_diff(q));
                n += 1;
            }
        }
    }
    sum as f64 / n as f64
}

fn assert_close_to_reference(name: &str, png: &[u8], w: u32, h: u32) {
    let ours = decode_png(png, w, h);
    let theirs = decode_image(&golden(&format!("{name}.png"))).expect("reference");
    let d = mean_abs_diff(&ours, &theirs);
    println!("{name}: mean abs diff vs JS = {d:.3}");
    assert!(d < 1.5, "{name}: mean absolute difference {d} against the JS reference");
}

// ── Text measurement and fitting against @napi-rs/canvas ──────────────────────

#[test]
fn text_widths_match_napi_canvas() {
    let fit = fit_json();
    let widths = fit["widths"].as_array().expect("widths");
    assert!(widths.len() > 500);
    for w in widths {
        let text = w["text"].as_str().expect("text");
        let size = w["size"].as_f64().expect("size") as f32;
        let ours = f64::from(font::measure(text, size));
        let js = w["width"].as_f64().expect("width");
        assert!((ours - js).abs() < 0.02, "{} {text:?}: rust {ours}, js {js}", w["font"]);
    }
}

#[test]
fn welcome_fit_matches_js() {
    for case in fit_json()["welcome"].as_array().expect("welcome") {
        let input = case["input"].as_str().expect("input");
        let data = WelcomeCardData {
            display_name: Some(input.into()),
            username: Some(input.into()),
            guild_name: Some(input.into()),
            ..Default::default()
        };
        assert_eq!(welcome_card::welcome_server_name(data.guild_name.as_deref()), case["serverName"].as_str().unwrap(), "{input:?}");
        assert_eq!(welcome_card::welcome_display_name(&data), case["displayName"].as_str().unwrap(), "{input:?}");
        assert_eq!(welcome_card::welcome_username(data.username.as_deref()), case["username"].as_str().unwrap(), "{input:?}");
        let (server, display) = welcome_card::welcome_fit_sizes(&data);
        assert_eq!(u64::from(server), case["serverSize"].as_u64().unwrap(), "server size {input:?}");
        assert_eq!(u64::from(display), case["displaySize"].as_u64().unwrap(), "display size {input:?}");
    }
}

#[test]
fn profile_truncate_matches_js() {
    let mut truncated = 0;
    for case in fit_json()["profile"].as_array().expect("profile") {
        let input = case["input"].as_str().expect("input");
        let (display, username) = profile_card::profile_names(input, input);
        assert_eq!(display, case["displayName"].as_str().unwrap(), "{input:?}");
        assert_eq!(username, format!("@{}", case["username"].as_str().unwrap()), "{input:?}");
        if display != input {
            truncated += 1;
        }
    }
    assert!(truncated >= 5, "the samples exercise truncation");
}

#[test]
fn truncate_drops_utf16_units() {
    // t.slice(0, -1) removes one UTF-16 unit, so a long run of astral characters loses half a pair at a time; a
    // split pair becomes U+FFFD, which the font draws as .notdef.
    let s = "𝔸".repeat(60);
    let out = bot_cards::canvas::truncate(&s, 28.0, 380.0);
    assert!(out.ends_with('…'));
    assert!(font::measure(&out, 28.0) <= 380.0);
}

#[test]
fn glyphs_the_font_lacks_draw_notdef() {
    assert!(!font::has_glyph('漢'));
    assert!(!font::has_glyph('😀'));
    // .notdef has an advance, so such text has a width and visible pixels.
    assert!(font::measure("漢😀", 28.0) > 0.0);
    let mut pm = Pixmap::new(100, 50).unwrap();
    font::fill_text(&mut pm, "漢", 28.0, font::Weight::Bold, 10.0, 35.0, &bot_cards::canvas::solid(tiny_skia::Color::WHITE));
    assert!(pm.pixels().iter().any(|p| p.alpha() > 0), ".notdef is drawn");
}

// ── Welcome card ──────────────────────────────────────────────────────────────

fn nova() -> WelcomeCardData {
    WelcomeCardData {
        display_name: Some("nova".into()),
        username: Some("nova".into()),
        guild_name: Some("Night Owls".into()),
        member_count: Some(1284.0),
        avatar_url: None,
    }
}

fn long_welcome() -> WelcomeCardData {
    WelcomeCardData {
        display_name: Some("Supercalifragilisticexpialidocious_Name".into()),
        username: Some("x_xX_Gamer_Xx_x 1234567890_and_more".into()),
        guild_name: Some("The Very Long Server Name Of Doom And Gloom".into()),
        member_count: Some(1_234_567.0),
        avatar_url: None,
    }
}

fn styled_welcome() -> WelcomeCardStyle {
    WelcomeCardStyle {
        background: Some("#102030".into()),
        accent_color: Some("#ff8800".into()),
        text_color: Some("#eeeeaa".into()),
        ..Default::default()
    }
}

#[test]
fn welcome_cards_match_js_references() {
    let png = render_welcome_card(&nova(), None, None, None).unwrap();
    save_sample("welcome-demo", &png);
    assert_close_to_reference("welcome-demo", &png, 700, 250);

    let png = render_welcome_card(&nova(), Some(&styled_welcome()), None, None).unwrap();
    save_sample("welcome-styled", &png);
    assert_close_to_reference("welcome-styled", &png, 700, 250);

    let accent = WelcomeCardStyle { accent_color: Some("#22aa44".into()), ..Default::default() };
    let png = render_welcome_card(&long_welcome(), Some(&accent), None, None).unwrap();
    save_sample("welcome-long", &png);
    assert_close_to_reference("welcome-long", &png, 700, 250);
}

#[test]
fn welcome_card_every_style_option_renders() {
    let avatar = golden("avatar.png");
    let bg_img = decode_image(&golden("background.png")).unwrap();
    let bg = welcome_card::cover_background(&bg_img).unwrap();
    let styles: Vec<(&str, Option<WelcomeCardStyle>)> = vec![
        ("none", None),
        ("empty", Some(WelcomeCardStyle::default())),
        ("background", Some(WelcomeCardStyle { background: Some("#112233".into()), ..Default::default() })),
        ("text", Some(WelcomeCardStyle { text_color: Some("#ffeedd".into()), ..Default::default() })),
        ("accent", Some(WelcomeCardStyle { accent_color: Some("#ff0000".into()), ..Default::default() })),
        ("overlay", Some(WelcomeCardStyle { overlay_opacity: Some(0.3), ..Default::default() })),
        ("url", Some(WelcomeCardStyle { background_url: Some("https://x.test/a.png".into()), ..Default::default() })),
        ("all", Some(WelcomeCardStyle {
            background: Some("#112233".into()),
            background_url: Some("https://x.test/a.png".into()),
            text_color: Some("#ffeedd".into()),
            accent_color: Some("#ff0000".into()),
            overlay_opacity: Some(0.3),
        })),
        ("nonsense", Some(WelcomeCardStyle { background: Some("nonsense".into()), overlay_opacity: Some(f64::NAN), ..Default::default() })),
    ];
    for (name, style) in &styles {
        for (variant, avatar, bg) in [("plain", None, None), ("images", Some(avatar.as_slice()), Some(&bg))] {
            let png = render_welcome_card(&nova(), style.as_ref(), avatar, bg).unwrap();
            let pm = decode_png(&png, 700, 250);
            save_sample(&format!("welcome-{name}-{variant}"), &png);
            // Text is drawn: the "WELCOME TO" box and the name box differ from the pixels just left of the text.
            let bg_px = rgba(&pm, 195, 70);
            assert!(differing(&pm, (200, 58, 320, 78), bg_px, 40) > 100, "{name}/{variant}: WELCOME TO drawn");
            let bg_px = rgba(&pm, 195, 150);
            assert!(differing(&pm, (200, 128, 290, 160), bg_px, 40) > 150, "{name}/{variant}: display name drawn");
            // The bottom accent bar.
            let accent = style.as_ref().and_then(|s| s.accent_color.clone()).unwrap_or_else(|| "#5865f2".into());
            let (r, g, b) = bot_cards::canvas::hex_rgb(&accent);
            assert_eq!(rgba(&pm, 350, 248), [r, g, b, 255], "{name}/{variant}: accent bar");
        }
    }
}

#[test]
fn welcome_avatar_and_background_are_drawn_or_fall_back() {
    let avatar = golden("avatar.png");
    // The demo avatar is orange (#E8853D) with a light "N".
    let png = render_welcome_card(&nova(), None, Some(&avatar), None).unwrap();
    let pm = decode_png(&png, 700, 250);
    save_sample("welcome-avatar", &png);
    assert_eq!(rgba(&pm, 60, 125), [0xe8, 0x85, 0x3d, 255]);
    // Bad bytes, a header-only bomb and nothing: the fallback disc.
    for (name, bytes) in [("bad", b"not an image".to_vec()), ("bomb", bomb_png(30_000, 30_000)), ("truncated", avatar[..100].to_vec())] {
        let png = render_welcome_card(&nova(), None, Some(&bytes), None).unwrap();
        let pm = decode_png(&png, 700, 250);
        assert_eq!(rgba(&pm, 60, 125), [0x4f, 0x54, 0x5c, 255], "{name}: fallback circle");
    }
    // The background (cover-fitted, then a 50% black overlay).
    let bg = welcome_card::cover_background(&decode_image(&golden("background.png")).unwrap()).unwrap();
    let plain = decode_png(&render_welcome_card(&nova(), None, None, None).unwrap(), 700, 250);
    let png = render_welcome_card(&nova(), None, None, Some(&bg)).unwrap();
    save_sample("welcome-background", &png);
    let with_bg = decode_png(&png, 700, 250);
    assert_ne!(rgba(&plain, 20, 20), rgba(&with_bg, 20, 20));
}

// ── Profile card ──────────────────────────────────────────────────────────────

fn base<'a>(avatar: &'a [u8]) -> ProfileCardOptions<'a> {
    ProfileCardOptions {
        avatar_buffer: Some(avatar),
        username: "nova",
        display_name: "nova",
        level: 12.0,
        total_xp: 18_740.0,
        rank: 4.0,
        xp_current: Some(1_240.0),
        xp_needed: Some(1_500.0),
        style: None,
        background_buffer: None,
    }
}

/// The 900x280 PNG profileCard.js caches for a background URL.
fn profile_background() -> Vec<u8> {
    profile_card::cover_background_png(&decode_image(&golden("background.png")).unwrap()).unwrap()
}

#[test]
fn profile_cards_match_js_references() {
    let avatar = golden("avatar.png");
    let styled = normalize_card_style(&json!({ "background": "#102030", "accentColor": "#ff8800", "textColor": "#eeeeaa", "overlayOpacity": 0.3 }));
    let overlay = normalize_card_style(&json!({ "overlayOpacity": 0.3 }));
    let bg = profile_background();
    let bad = b"not an image".to_vec();
    let cases: Vec<(&str, ProfileCardOptions)> = vec![
        ("profile-demo", base(&avatar)),
        ("profile-styled", ProfileCardOptions { style: Some(&styled), ..base(&avatar) }),
        ("profile-background", ProfileCardOptions { style: Some(&overlay), background_buffer: Some(&bg), ..base(&avatar) }),
        ("profile-no-avatar", ProfileCardOptions { avatar_buffer: None, xp_current: Some(0.0), ..base(&avatar) }),
        ("profile-bad-avatar", ProfileCardOptions { avatar_buffer: Some(&bad), xp_current: None, xp_needed: None, ..base(&avatar) }),
        ("profile-long", ProfileCardOptions {
            display_name: "Supercalifragilisticexpialidocious_Name",
            username: "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM",
            level: 250.0,
            rank: 1_234_567.0,
            total_xp: 987_654_321.0,
            xp_current: Some(5.0),
            xp_needed: Some(1_000_000.0),
            ..base(&avatar)
        }),
    ];
    for (name, o) in cases {
        let png = generate_profile_card(&o).unwrap();
        save_sample(name, &png);
        assert_close_to_reference(name, &png, 900, 280);
    }
}

#[test]
fn profile_card_every_style_option_renders() {
    let avatar = golden("avatar.png");
    let bg = profile_background();
    let styles = [
        json!(null),
        json!({}),
        json!({ "background": "#112233" }),
        json!({ "accentColor": "#ff8800" }),
        json!({ "textColor": "#eeeeee" }),
        json!({ "overlayOpacity": 0 }),
        json!({ "overlayOpacity": 0.9 }),
        json!({ "backgroundUrl": "https://img.example.test/a.png" }),
        json!({ "background": "#112233", "accentColor": "#ff8800", "textColor": "#eeeeee", "overlayOpacity": 0.3 }),
        json!({ "background": "red", "overlayOpacity": 7 }),
    ];
    for (i, s) in styles.iter().enumerate() {
        let style: RankCardStyle = normalize_card_style(s);
        for (variant, bg) in [("plain", None), ("bg", Some(bg.as_slice()))] {
            let o = ProfileCardOptions { style: Some(&style), background_buffer: bg, ..base(&avatar) };
            let png = generate_profile_card(&o).unwrap();
            let pm = decode_png(&png, 900, 280);
            save_sample(&format!("profile-style{i}-{variant}"), &png);
            // Rounded corners are transparent, the centre is opaque.
            assert_eq!(rgba(&pm, 0, 0)[3], 0, "style {i}: corner");
            assert_eq!(rgba(&pm, 450, 20)[3], 255, "style {i}: body");
            // Text is drawn: name, LEVEL label, XP label.
            let px = rgba(&pm, 175, 60);
            assert!(differing(&pm, (180, 96, 250, 122), px, 40) > 100, "style {i}/{variant}: display name");
            let px = rgba(&pm, 700, 40);
            assert!(differing(&pm, (720, 64, 872, 92), px, 40) > 150, "style {i}/{variant}: LEVEL");
            let px = rgba(&pm, 175, 230);
            assert!(differing(&pm, (180, 208, 300, 226), px, 40) > 80, "style {i}/{variant}: XP label");
            // The progress bar fill starts with the accent colour.
            let (r, g, b) = bot_cards::canvas::hex_rgb(&style.accent_color);
            let p = rgba(&pm, 200, 188);
            assert!(p[0].abs_diff(r) < 8 && p[1].abs_diff(g) < 8 && p[2].abs_diff(b) < 8, "style {i}: bar {p:?}");
        }
    }
}

/// A PNG whose header claims `w` x `h` pixels with no real image data (test/levels.test.js `bombPng`).
fn bomb_png(w: u32, h: u32) -> Vec<u8> {
    fn crc32(buf: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in buf {
            let mut c = (crc ^ u32::from(b)) & 0xff;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            crc = (crc >> 8) ^ c;
        }
        crc ^ 0xffff_ffff
    }
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32(&out[4..]).to_be_bytes());
        out
    }
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut out = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    out.extend(chunk(b"IHDR", &ihdr));
    out.extend(chunk(b"IDAT", &[0x78, 0x9c, 0x63, 0x60, 0x00, 0x00, 0x00, 0x08, 0x00, 0x01]));
    out.extend(chunk(b"IEND", &[]));
    out
}

// Port of test/levels.test.js "generateProfileCard renders a 900x280 PNG with a custom style and compact numbers" and
// "a crafted 30000x30000 background is refused before it is decoded (D4)".
#[test]
fn profile_card_bad_images_fall_back() {
    let style = normalize_card_style(&json!({ "background": "#112233", "accentColor": "#ff8800", "textColor": "#eeeeee", "overlayOpacity": 0.3 }));
    let png = generate_profile_card(&ProfileCardOptions {
        username: "alice",
        display_name: "Alice",
        level: 443.0,
        total_xp: 999_960.0,
        rank: 1234.0,
        style: Some(&style),
        ..Default::default()
    })
    .unwrap();
    decode_png(&png, 900, 280);

    let bomb = bomb_png(30_000, 30_000);
    let avatar = golden("avatar.png");
    let plain = decode_png(&generate_profile_card(&base(&avatar)).unwrap(), 900, 280);
    for (name, bytes) in [("text", b"not an image".to_vec()), ("bomb", bomb.clone()), ("truncated", avatar[..100].to_vec()), ("empty", Vec::new())] {
        // As a background: skipped, the card is the plain one.
        let o = ProfileCardOptions { background_buffer: Some(&bytes), ..base(&avatar) };
        let pm = decode_png(&generate_profile_card(&o).unwrap(), 900, 280);
        assert_eq!(mean_abs_diff(&pm, &plain), 0.0, "{name}: background skipped");
        // As an avatar: the #40444b placeholder.
        let o = ProfileCardOptions { avatar_buffer: Some(&bytes), ..base(&avatar) };
        let pm = decode_png(&generate_profile_card(&o).unwrap(), 900, 280);
        assert_eq!(rgba(&pm, 96, 140), [0x40, 0x44, 0x4b, 255], "{name}: avatar placeholder");
    }
    assert_eq!(rgba(&plain, 60, 140), [0xe8, 0x85, 0x3d, 255], "the real avatar");
}

// ── Loaders (offline: only refusals that never reach the network) ─────────────

// Port of test/welcome.test.js "validateCardBackground refuses non-URLs without touching the network".
#[tokio::test]
async fn welcome_validate_refuses_non_urls() {
    let long = format!("https://x.test/{}", "a".repeat(2100));
    for bad in [Some(""), Some("not a url"), Some("ftp://x.test/a.png"), Some("javascript:alert(1)"), Some(long.as_str()), None] {
        match welcome_card::validate_card_background(bad).await {
            welcome_card::BackgroundCheck::Invalid { reason } => assert!(reason.contains("valid http(s) URL"), "{bad:?}: {reason}"),
            other => panic!("{bad:?}: {other:?}"),
        }
    }
    match welcome_card::validate_card_background(Some("http://127.0.0.1/a.png")).await {
        welcome_card::BackgroundCheck::Invalid { reason } => assert!(reason.contains("cannot be reached"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(welcome_card::background_cache_size() <= 20);
    // A refused background resolves to None (and is remembered).
    assert!(welcome_card::load_card_background("http://127.0.0.1:9/x.png", None).await.is_none());
    assert!(welcome_card::load_card_background("http://127.0.0.1:9/x.png", None).await.is_none());
    assert!(welcome_card::background_cache_size() >= 1);
    // An avatar that cannot be fetched is None.
    assert!(welcome_card::load_avatar(Some("http://127.0.0.1:9/a.png")).await.is_none());
    assert!(welcome_card::load_avatar(None).await.is_none());
    // generate_welcome_card with unreachable images still renders.
    let data = WelcomeCardData { avatar_url: Some("http://127.0.0.1:9/a.png".into()), ..nova() };
    let style = WelcomeCardStyle { background_url: Some("http://127.0.0.1:9/x.png".into()), ..Default::default() };
    let png = welcome_card::generate_welcome_card(&data, Some(&style)).await.unwrap();
    decode_png(&png, 700, 250);
}

// Port of test/levels.test.js "validateCardBackground and loadCardBackground never throw and refuse local addresses".
#[tokio::test]
async fn profile_validate_and_load_refuse_local_addresses() {
    use profile_card::BackgroundCheck;
    assert_eq!(
        profile_card::validate_card_background("http://127.0.0.1:9/x.png").await,
        BackgroundCheck::Invalid { reason: "that address is not allowed".into() }
    );
    assert_eq!(
        profile_card::validate_card_background("not a url").await,
        BackgroundCheck::Invalid { reason: "that address is not allowed".into() }
    );
    assert_eq!(profile_card::load_card_background(Some("http://127.0.0.1:9/x.png")).await, None);
    assert_eq!(profile_card::load_card_background(Some("http://127.0.0.1:9/x.png")).await, None, "negative cache");
    assert!(profile_card::background_cache_size() >= 1);
    assert_eq!(profile_card::load_card_background(None).await, None);
    profile_card::clear_card_background(Some("http://127.0.0.1:9/x.png"));
    profile_card::clear_card_background(None);
    assert_eq!(profile_card::background_cache_size(), 0);
}
