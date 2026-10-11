import { createCanvas, loadImage, GlobalFonts } from '@napi-rs/canvas';
import { existsSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { safeFetchImage, SafeFetchError } from './safeFetch.js';
import { clampText, formatNumber } from './text.js';
import logger from './logger.js';

// ── Welcome card ───────────────────────────────────────────────────────────────
// A 700x250 PNG: avatar, "WELCOME TO", server name, display name, @username and member count.
// Everything user-influenced is clamped, every image is fetched through safeFetchImage (which
// also refuses decompression bombs via the shared header check), and nothing here does I/O at
// import time: the font is registered on the first render.

const __dirname = dirname(fileURLToPath(import.meta.url));
const FONT_PATH = resolve(__dirname, '../../assets/fonts/Nunito-Bold.ttf');

let FONT_FAMILY = 'sans-serif';
let fontReady = false;

/** Registers the bundled font once, on the first render. A missing or broken font falls back to sans-serif. */
function ensureFont() {
  if (fontReady) return;
  fontReady = true;
  try {
    if (existsSync(FONT_PATH)) {
      GlobalFonts.registerFromPath(FONT_PATH, 'Nunito');
      FONT_FAMILY = 'Nunito';
    }
  } catch {
    // Font registration failed: keep the system font.
  }
}

export const CARD_WIDTH = 700;
export const CARD_HEIGHT = 250;
const W = CARD_WIDTH;
const H = CARD_HEIGHT;
const AVATAR_X = 100;
const AVATAR_Y = 125;
const AVATAR_R = 65;
const TEXT_X = 200;

/** The colours the card used before it became customisable; `style = null` reproduces it exactly. */
export const LEGACY_CARD_COLORS = Object.freeze({
  background: '#23272a',
  background2: '#2c2f33',
  accent: '#5865f2',
  text: '#ffffff',
  secondary: '#b9bbbe',
  count: '#57f287',
  avatarFallback: '#4f545c',
  divider: '#4f545c',
});

const L = LEGACY_CARD_COLORS;
const HEX_RE = /^#[0-9a-f]{6}$/i;
const MAX_BG_BYTES = 5 * 1024 * 1024;
const MAX_AVATAR_BYTES = 2 * 1024 * 1024;

// ── Drawing helpers ────────────────────────────────────────────────────────────

function fitText(ctx, text, maxWidth, fontSize, fontFamily) {
  let size = fontSize;
  ctx.font = `bold ${size}px ${fontFamily}`;
  while (ctx.measureText(text).width > maxWidth && size > 14) {
    size -= 2;
    ctx.font = `bold ${size}px ${fontFamily}`;
  }
  return size;
}

function hexToRgba(hex, alpha) {
  const n = parseInt(hex.slice(1), 16);
  return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${alpha})`;
}

const colorOrNull = (v) => (typeof v === 'string' && HEX_RE.test(v) ? v.toLowerCase() : null);

// Re-validates a style defensively: anything invalid counts as "not set".
function sanitizeStyle(style) {
  if (!style || typeof style !== 'object') {
    return { background: null, backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: null };
  }
  const url = typeof style.backgroundUrl === 'string' && /^https?:\/\//i.test(style.backgroundUrl) && style.backgroundUrl.length <= 2048
    ? style.backgroundUrl : null;
  const o = Number(style.overlayOpacity);
  return {
    background: colorOrNull(style.background),
    backgroundUrl: url,
    textColor: colorOrNull(style.textColor),
    accentColor: colorOrNull(style.accentColor),
    overlayOpacity: style.overlayOpacity !== null && style.overlayOpacity !== undefined && Number.isFinite(o)
      ? Math.min(1, Math.max(0, o)) : null,
  };
}

// ── Safe image loading ─────────────────────────────────────────────────────────

/**
 * Downloads an image through safeFetchImage (SSRF guard, size cap, timeout, header-size guard)
 * and decodes it. Throws SafeFetchError for every download problem.
 */
async function loadImageSafe(url, { timeoutMs, maxBytes }) {
  const buf = await safeFetchImage(url, { timeoutMs, maxBytes });
  return loadImage(buf);
}

async function loadAvatar(member) {
  try {
    const opts = { extension: 'png', forceStatic: true, size: 256 };
    const url = typeof member?.displayAvatarURL === 'function'
      ? member.displayAvatarURL(opts)
      : member?.user?.displayAvatarURL?.(opts);
    if (!url) return null;
    return await loadImageSafe(url, { timeoutMs: 5000, maxBytes: MAX_AVATAR_BYTES });
  } catch {
    return null; // the fallback circle is drawn instead
  }
}

// ── Background cache ───────────────────────────────────────────────────────────
// url -> { promise: Promise<Canvas|null>, expiresAt }. At most 20 entries (oldest evicted first,
// Map insertion order); success lives 30 minutes, failure 5. Concurrent loads share one promise.
const BG_CACHE = new Map();
const BG_CACHE_MAX = 20;
const BG_OK_TTL_MS = 30 * 60_000;
const BG_FAIL_TTL_MS = 5 * 60_000;

function storeBackground(url, promise, expiresAt) {
  BG_CACHE.delete(url); // re-insert so the entry counts as the newest
  BG_CACHE.set(url, { promise, expiresAt });
  while (BG_CACHE.size > BG_CACHE_MAX) BG_CACHE.delete(BG_CACHE.keys().next().value);
}

// Downloads the image and cover-fits it onto a 700x250 canvas.
async function fetchBackgroundCanvas(url, timeoutMs) {
  const img = await loadImageSafe(url, { timeoutMs, maxBytes: MAX_BG_BYTES });
  const canvas = createCanvas(W, H);
  const ctx = canvas.getContext('2d');
  const scale = Math.max(W / img.width, H / img.height);
  const dw = img.width * scale;
  const dh = img.height * scale;
  ctx.drawImage(img, (W - dw) / 2, (H - dh) / 2, dw, dh);
  return canvas;
}

/**
 * Loads a card background (cached). Never throws: a failure resolves to null and is remembered for 5 minutes.
 * @param {string} url
 * @param {{ timeoutMs?: number }} [opts]
 * @returns {Promise<import('@napi-rs/canvas').Canvas|null>}
 */
export function loadCardBackground(url, { timeoutMs = 4000 } = {}) {
  const hit = BG_CACHE.get(url);
  if (hit && hit.expiresAt > Date.now()) return hit.promise;

  const promise = (async () => {
    try {
      const canvas = await fetchBackgroundCanvas(url, timeoutMs);
      const entry = BG_CACHE.get(url);
      if (entry?.promise === promise) entry.expiresAt = Date.now() + BG_OK_TTL_MS;
      return canvas;
    } catch (err) {
      logger.warn(`Welcome card background could not be loaded (${err?.code ?? err?.message ?? 'error'}); using the plain background for 5 minutes`);
      const entry = BG_CACHE.get(url);
      if (entry?.promise === promise) entry.expiresAt = Date.now() + BG_FAIL_TTL_MS;
      return null;
    }
  })();
  storeBackground(url, promise, Number.POSITIVE_INFINITY); // in flight until it settles
  return promise;
}

const REASONS = Object.freeze({
  INVALID_URL: 'That is not a valid http(s) URL.',
  BLOCKED_ADDRESS: 'That address cannot be reached from the bot.',
  DNS_FAILED: 'That address cannot be reached from the bot.',
  TIMEOUT: 'The image took too long to download.',
  TOO_LARGE: 'The image is larger than 5 MB.',
  BAD_CONTENT_TYPE: 'That URL is not an image.',
  BAD_IMAGE: 'Use a PNG, JPEG, GIF or WebP image of at most 8000×8000 pixels.',
  IMAGE_TOO_LARGE_OR_UNKNOWN: 'Use a PNG, JPEG, GIF or WebP image of at most 8000×8000 pixels.',
});

/**
 * Config-time check of a background URL: downloads and decodes it (8 s), and on success seeds the cache.
 * Never throws.
 * @param {string} url
 * @returns {Promise<{ ok: true } | { ok: false, reason: string }>}
 */
export async function validateCardBackground(url) {
  if (typeof url !== 'string' || !/^https?:\/\//i.test(url) || url.length > 2048) {
    return { ok: false, reason: REASONS.INVALID_URL };
  }
  BG_CACHE.delete(url);
  try {
    const canvas = await fetchBackgroundCanvas(url, 8000);
    storeBackground(url, Promise.resolve(canvas), Date.now() + BG_OK_TTL_MS);
    return { ok: true };
  } catch (err) {
    if (err instanceof SafeFetchError) {
      if (err.code === 'HTTP_ERROR') return { ok: false, reason: `The image server answered HTTP ${err.status ?? 'error'}.` };
      if (REASONS[err.code]) return { ok: false, reason: REASONS[err.code] };
    } else if (err?.message && REASONS[err.message]) {
      return { ok: false, reason: REASONS[err.message] };
    }
    return { ok: false, reason: "I couldn't load that image." };
  }
}

/** Test hook: the number of cached backgrounds. */
export function backgroundCacheSize() {
  return BG_CACHE.size;
}

// ── Card ───────────────────────────────────────────────────────────────────────

/**
 * Generates the 700x250 welcome card. `style` is the parseCardStyle result (or null); each element
 * is "style value ?? legacy value", so null reproduces the original card. Throws only on a canvas failure.
 *
 * @param {import('discord.js').GuildMember} member
 * @param {import('discord.js').Guild} guild
 * @param {{ background?: string|null, backgroundUrl?: string|null, textColor?: string|null,
 *   accentColor?: string|null, overlayOpacity?: number|null }|null} [style]
 * @returns {Promise<Buffer>} PNG
 */
export async function generateWelcomeCard(member, guild, style = null) {
  ensureFont();
  const st = sanitizeStyle(style);
  const [avatar, bg] = await Promise.all([
    loadAvatar(member),
    st.backgroundUrl ? loadCardBackground(st.backgroundUrl) : null,
  ]);

  const canvas = createCanvas(W, H);
  const ctx = canvas.getContext('2d');

  // ── Background ──────────────────────────────────────────────────────────────
  if (st.background) {
    ctx.fillStyle = st.background;
  } else {
    const bgGrad = ctx.createLinearGradient(0, 0, W, 0);
    bgGrad.addColorStop(0, L.background);
    bgGrad.addColorStop(1, L.background2);
    ctx.fillStyle = bgGrad;
  }
  ctx.fillRect(0, 0, W, H);

  if (bg) {
    ctx.drawImage(bg, 0, 0);
    ctx.fillStyle = `rgba(0,0,0,${st.overlayOpacity ?? 0.5})`;
    ctx.fillRect(0, 0, W, H);
  }

  // ── Accent: glow on the right, bottom bar, avatar ring, server name ─────────
  const accent = st.accentColor ?? L.accent;
  const glow = ctx.createRadialGradient(W, H / 2, 0, W, H / 2, 350);
  glow.addColorStop(0, hexToRgba(accent, 0.18));
  glow.addColorStop(1, 'transparent');
  ctx.fillStyle = glow;
  ctx.fillRect(0, 0, W, H);

  ctx.fillStyle = accent;
  ctx.fillRect(0, H - 5, W, 5);

  ctx.save();
  ctx.beginPath();
  ctx.arc(AVATAR_X, AVATAR_Y, AVATAR_R + 6, 0, Math.PI * 2);
  ctx.fillStyle = accent;
  ctx.fill();
  ctx.restore();

  ctx.save();
  ctx.beginPath();
  ctx.arc(AVATAR_X, AVATAR_Y, AVATAR_R, 0, Math.PI * 2);
  if (avatar) {
    ctx.clip();
    ctx.drawImage(avatar, AVATAR_X - AVATAR_R, AVATAR_Y - AVATAR_R, AVATAR_R * 2, AVATAR_R * 2);
  } else {
    ctx.fillStyle = L.avatarFallback;
    ctx.fill();
  }
  ctx.restore();

  // ── Text ────────────────────────────────────────────────────────────────────
  const textMaxWidth = W - TEXT_X - 20;
  const mutedColor = st.textColor ?? L.secondary;
  const mutedAlpha = st.textColor ? 0.7 : 1;

  ctx.save();
  ctx.globalAlpha = mutedAlpha;
  ctx.fillStyle = mutedColor;
  ctx.font = `18px ${FONT_FAMILY}`;
  ctx.fillText('WELCOME TO', TEXT_X, 75);
  ctx.restore();

  const serverName = clampText(String(guild?.name ?? '').toUpperCase(), 32);
  fitText(ctx, serverName, textMaxWidth, 26, FONT_FAMILY);
  ctx.fillStyle = accent;
  ctx.fillText(serverName, TEXT_X, 108);

  ctx.strokeStyle = L.divider;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(TEXT_X, 120);
  ctx.lineTo(W - 20, 120);
  ctx.stroke();

  const displayName = clampText(member?.displayName ?? member?.user?.username ?? '', 26);
  fitText(ctx, displayName, textMaxWidth, 36, FONT_FAMILY);
  ctx.fillStyle = st.textColor ?? L.text;
  ctx.fillText(displayName, TEXT_X, 158);

  ctx.save();
  ctx.globalAlpha = mutedAlpha;
  ctx.fillStyle = mutedColor;
  ctx.font = `18px ${FONT_FAMILY}`;
  ctx.fillText(`@${clampText(member?.user?.username ?? '', 28)}`, TEXT_X, 182);
  ctx.restore();

  ctx.fillStyle = st.accentColor ?? L.count;
  ctx.font = `20px ${FONT_FAMILY}`;
  ctx.fillText(`Member #${formatNumber(guild?.memberCount)}`, TEXT_X, 212);

  return canvas.encode('png');
}
