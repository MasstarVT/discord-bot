// ── Image header parsing ───────────────────────────────────────────────────────
// Pure; no imports. Reads only the first bytes of an image so a decompression bomb
// (a small PNG that declares 30000×30000 pixels) can be refused before @napi-rs/canvas
// decodes it. Neither function reads past the header, and readImageSize never throws.

export const DEFAULT_MAX_SIDE = 8000;
export const DEFAULT_MAX_PIXELS = 16_000_000;

const JPEG_SEGMENT_LIMIT = 1000; // bounded scan: a hostile file cannot loop forever

function readPng(b) {
  // 8-byte signature, then the IHDR chunk: length(4) 'IHDR'(4) width(4) height(4)
  if (b.length < 24) return null;
  if (b[0] !== 0x89 || b[1] !== 0x50 || b[2] !== 0x4e || b[3] !== 0x47) return null;
  if (b[4] !== 0x0d || b[5] !== 0x0a || b[6] !== 0x1a || b[7] !== 0x0a) return null;
  if (b.toString('latin1', 12, 16) !== 'IHDR') return null;
  return { type: 'png', width: b.readUInt32BE(16), height: b.readUInt32BE(20) };
}

function readGif(b) {
  if (b.length < 10) return null;
  const sig = b.toString('latin1', 0, 6);
  if (sig !== 'GIF87a' && sig !== 'GIF89a') return null;
  return { type: 'gif', width: b.readUInt16LE(6), height: b.readUInt16LE(8) };
}

function readJpeg(b) {
  if (b.length < 4 || b[0] !== 0xff || b[1] !== 0xd8) return null;
  let pos = 2;
  for (let n = 0; n < JPEG_SEGMENT_LIMIT && pos + 4 <= b.length; n++) {
    if (b[pos] !== 0xff) return null;
    while (pos < b.length && b[pos] === 0xff) pos++; // fill bytes
    const marker = b[pos];
    pos++;
    if (marker === undefined) return null;
    // Markers without a length field.
    if (marker === 0x01 || (marker >= 0xd0 && marker <= 0xd9)) continue;
    if (pos + 2 > b.length) return null;
    const len = b.readUInt16BE(pos);
    // SOF0..SOF15 except DHT (C4), JPG (C8) and DAC (CC).
    if (marker >= 0xc0 && marker <= 0xcf && marker !== 0xc4 && marker !== 0xc8 && marker !== 0xcc) {
      if (pos + 7 > b.length) return null;
      // length(2) precision(1) height(2) width(2)
      return { type: 'jpeg', width: b.readUInt16BE(pos + 5), height: b.readUInt16BE(pos + 3) };
    }
    if (marker === 0xda) return null; // start of scan: no SOF seen
    if (len < 2) return null;
    pos += len;
  }
  return null;
}

function readWebp(b) {
  if (b.length < 30) return null;
  if (b.toString('latin1', 0, 4) !== 'RIFF' || b.toString('latin1', 8, 12) !== 'WEBP') return null;
  const chunk = b.toString('latin1', 12, 16);
  if (chunk === 'VP8 ') {
    // frame tag(3) start code 9D 01 2A, then 14-bit width and height
    if (b[23] !== 0x9d || b[24] !== 0x01 || b[25] !== 0x2a) return null;
    return { type: 'webp', width: b.readUInt16LE(26) & 0x3fff, height: b.readUInt16LE(28) & 0x3fff };
  }
  if (chunk === 'VP8L') {
    if (b[20] !== 0x2f) return null;
    const bits = b.readUInt32LE(21);
    return { type: 'webp', width: (bits & 0x3fff) + 1, height: ((bits >>> 14) & 0x3fff) + 1 };
  }
  if (chunk === 'VP8X') {
    // flags(4) then canvas width-1 and height-1 as 24-bit little endian
    return {
      type: 'webp',
      width: (b[24] | (b[25] << 8) | (b[26] << 16)) + 1,
      height: (b[27] | (b[28] << 8) | (b[29] << 16)) + 1,
    };
  }
  return null;
}

/**
 * Reads the declared size from an image header (PNG, JPEG, GIF or WebP). Never throws.
 * @param {Buffer|Uint8Array} buffer
 * @returns {{ type: 'png'|'jpeg'|'gif'|'webp', width: number, height: number } | null} null for anything else or a truncated header
 */
export function readImageSize(buffer) {
  try {
    if (!buffer || typeof buffer.length !== 'number') return null;
    const b = Buffer.isBuffer(buffer) ? buffer : Buffer.from(buffer.buffer, buffer.byteOffset, buffer.byteLength);
    const size = readPng(b) ?? readJpeg(b) ?? readGif(b) ?? readWebp(b);
    if (!size || !(size.width > 0) || !(size.height > 0)) return null;
    return size;
  } catch {
    return null;
  }
}

/**
 * Refuses an image that is unknown, too wide or tall, or has too many pixels.
 * Call it before every `loadImage(buffer)` of user-supplied bytes.
 * @param {Buffer|Uint8Array} buffer
 * @param {{ maxSide?: number, maxPixels?: number }} [opts]
 * @returns {{ type: string, width: number, height: number }}
 * @throws {Error} `IMAGE_TOO_LARGE_OR_UNKNOWN`
 */
export function assertImageDimensions(buffer, { maxSide = DEFAULT_MAX_SIDE, maxPixels = DEFAULT_MAX_PIXELS } = {}) {
  const size = readImageSize(buffer);
  if (!size || size.width > maxSide || size.height > maxSide || size.width * size.height > maxPixels) {
    throw new Error('IMAGE_TOO_LARGE_OR_UNKNOWN');
  }
  return size;
}
