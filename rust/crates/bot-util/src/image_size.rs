//! Port of `src/utils/imageSize.js`: reads the declared size from an image header (PNG, JPEG, GIF, WebP) so a
//! decompression bomb can be refused before it is decoded. Never reads past the header and never panics.

/// Widest or tallest image accepted by default.
pub const DEFAULT_MAX_SIDE: u64 = 8000;
/// Most pixels accepted by default.
pub const DEFAULT_MAX_PIXELS: u64 = 16_000_000;

const JPEG_SEGMENT_LIMIT: usize = 1000;

/// The format of an image header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageType {
    /// 'png', 'jpeg', 'gif' or 'webp'.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }
}

/// A declared image size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSize {
    pub kind: ImageType,
    pub width: u64,
    pub height: u64,
}

fn be16(b: &[u8], i: usize) -> Option<u64> {
    Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u64)
}
fn le16(b: &[u8], i: usize) -> Option<u64> {
    Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u64)
}
fn be32(b: &[u8], i: usize) -> Option<u64> {
    Some(u32::from_be_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, *b.get(i + 3)?]) as u64)
}
fn le32(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, *b.get(i + 3)?]))
}

fn read_png(b: &[u8]) -> Option<ImageSize> {
    if b.len() < 24 || b[..8] != [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a] || &b[12..16] != b"IHDR" {
        return None;
    }
    Some(ImageSize { kind: ImageType::Png, width: be32(b, 16)?, height: be32(b, 20)? })
}

fn read_gif(b: &[u8]) -> Option<ImageSize> {
    if b.len() < 10 || (&b[..6] != b"GIF87a" && &b[..6] != b"GIF89a") {
        return None;
    }
    Some(ImageSize { kind: ImageType::Gif, width: le16(b, 6)?, height: le16(b, 8)? })
}

fn read_jpeg(b: &[u8]) -> Option<ImageSize> {
    if b.len() < 4 || b[0] != 0xff || b[1] != 0xd8 {
        return None;
    }
    let mut pos = 2;
    let mut n = 0;
    while n < JPEG_SEGMENT_LIMIT && pos + 4 <= b.len() {
        n += 1;
        if b[pos] != 0xff {
            return None;
        }
        while pos < b.len() && b[pos] == 0xff {
            pos += 1;
        }
        let marker = *b.get(pos)?;
        pos += 1;
        if marker == 0x01 || (0xd0..=0xd9).contains(&marker) {
            continue;
        }
        if pos + 2 > b.len() {
            return None;
        }
        let len = be16(b, pos)? as usize;
        if (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc {
            if pos + 7 > b.len() {
                return None;
            }
            return Some(ImageSize { kind: ImageType::Jpeg, width: be16(b, pos + 5)?, height: be16(b, pos + 3)? });
        }
        if marker == 0xda || len < 2 {
            return None;
        }
        pos += len;
    }
    None
}

fn read_webp(b: &[u8]) -> Option<ImageSize> {
    if b.len() < 30 || &b[..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return None;
    }
    match &b[12..16] {
        b"VP8 " => {
            if b[23] != 0x9d || b[24] != 0x01 || b[25] != 0x2a {
                return None;
            }
            Some(ImageSize { kind: ImageType::Webp, width: le16(b, 26)? & 0x3fff, height: le16(b, 28)? & 0x3fff })
        }
        b"VP8L" => {
            if b[20] != 0x2f {
                return None;
            }
            let bits = le32(b, 21)?;
            Some(ImageSize { kind: ImageType::Webp, width: (bits & 0x3fff) as u64 + 1, height: ((bits >> 14) & 0x3fff) as u64 + 1 })
        }
        b"VP8X" => Some(ImageSize {
            kind: ImageType::Webp,
            width: (b[24] as u64 | (b[25] as u64) << 8 | (b[26] as u64) << 16) + 1,
            height: (b[27] as u64 | (b[28] as u64) << 8 | (b[29] as u64) << 16) + 1,
        }),
        _ => None,
    }
}

/// Reads the declared size from an image header; None for anything else or a truncated header.
pub fn read_image_size(b: &[u8]) -> Option<ImageSize> {
    let size = read_png(b).or_else(|| read_jpeg(b)).or_else(|| read_gif(b)).or_else(|| read_webp(b))?;
    if size.width == 0 || size.height == 0 {
        return None;
    }
    Some(size)
}

/// The error of [`assert_image_dimensions`]; its message is `IMAGE_TOO_LARGE_OR_UNKNOWN`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("IMAGE_TOO_LARGE_OR_UNKNOWN")]
pub struct ImageTooLargeOrUnknown;

/// Refuses an image that is unknown, too wide or tall, or has too many pixels. Call it before decoding user bytes.
pub fn assert_image_dimensions(b: &[u8], max_side: u64, max_pixels: u64) -> Result<ImageSize, ImageTooLargeOrUnknown> {
    match read_image_size(b) {
        Some(s) if s.width <= max_side && s.height <= max_side && (s.width as f64) * (s.height as f64) <= max_pixels as f64 => Ok(s),
        _ => Err(ImageTooLargeOrUnknown),
    }
}

/// [`assert_image_dimensions`] with the default limits.
pub fn assert_image_dimensions_default(b: &[u8]) -> Result<ImageSize, ImageTooLargeOrUnknown> {
    assert_image_dimensions(b, DEFAULT_MAX_SIDE, DEFAULT_MAX_PIXELS)
}
