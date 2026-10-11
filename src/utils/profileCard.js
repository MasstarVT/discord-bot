import { createCanvas, loadImage, GlobalFonts } from '@napi-rs/canvas';
import { existsSync }  from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath }  from 'url';
import { xpForLevel }    from '../config/constants.js';
import { isHttpUrl }     from './customEmbed.js';
import { assertImageDimensions } from './imageSize.js';
import logger            from './logger.js';
import { SafeFetchError, safeFetchImage } from './safeFetch.js';
import { compactNumber, formatNumber } from './text.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const FONT_PATH = join(__dirname, '../../assets/fonts/Nunito-Bold.ttf');

if (existsSync(FONT_PATH)) {
  GlobalFonts.registerFromPath(FONT_PATH, 'Nunito');
}

const W = 900;
const H = 280;

const GREY     = '#B9BBBE';
const BAR_BG   = '#3f4147';
const GREEN    = '#57F287';

const BACKGROUND_MAX_BYTES = 8 * 1024 * 1024;
const BACKGROUND_TIMEOUT_MS = 8000;
const BACKGROUND_TTL_MS = 30 * 60_000;
const BACKGROUND_NEGATIVE_TTL_MS = 10 * 60_000;
const BACKGROUND_CACHE_MAX = 50;

// ── Card style (GuildSettings.rankCardStyle) ──────────────────────────────────

export const DEFAULT_RANK_CARD_STYLE = Object.freeze({
  background: '#2B2D31',
  backgroundUrl: null,
  accentColor: '#5865F2',
  textColor: '#FFFFFF',
  overlayOpacity: 0.5,
});

const HEX_RE = /^#[0-9A-F]{6}$/i;

/**
 * Fills the defaults of a stored card style, key by key. Colours must be #RRGGBB (stored upper-case), the image URL must
 * be an http(s) URL, the overlay a number in 0..0.9. Pure; never mutates its input (settings rows are frozen).
 * @param {unknown} json
 * @returns {{ background: string, backgroundUrl: string|null, accentColor: string, textColor: string, overlayOpacity: number }}
 */
export function normalizeCardStyle(json) {
  const src = json && typeof json === 'object' && !Array.isArray(json) ? json : {};
  const colour = (value, fallback) => (typeof value === 'string' && HEX_RE.test(value) ? value.toUpperCase() : fallback);
  const overlay = src.overlayOpacity;
  return {
    background: colour(src.background, DEFAULT_RANK_CARD_STYLE.background),
    backgroundUrl: isHttpUrl(src.backgroundUrl) ? src.backgroundUrl : null,
    accentColor: colour(src.accentColor, DEFAULT_RANK_CARD_STYLE.accentColor),
    textColor: colour(src.textColor, DEFAULT_RANK_CARD_STYLE.textColor),
    overlayOpacity: typeof overlay === 'number' && Number.isFinite(overlay) && overlay >= 0 && overlay <= 0.9
      ? overlay
      : DEFAULT_RANK_CARD_STYLE.overlayOpacity,
  };
}

// ── Background images ─────────────────────────────────────────────────────────

/** url -> { png: Buffer|null, expiresAt }. Insertion order = age, so the first key is evicted first. */
const backgroundCache = new Map();
/** url -> in-flight load, so a burst of /rank calls shares one download. */
const backgroundLoading = new Map();

function cachePut(url, png, ttlMs) {
  backgroundCache.delete(url);
  backgroundCache.set(url, { png, expiresAt: Date.now() + ttlMs });
  while (backgroundCache.size > BACKGROUND_CACHE_MAX) backgroundCache.delete(backgroundCache.keys().next().value);
}

/** Draws `img` cover-cropped (scaled to fill, centred) over the whole card. */
function drawCover(ctx, img) {
  const scale = Math.max(W / img.width, H / img.height);
  const dw = img.width * scale;
  const dh = img.height * scale;
  ctx.drawImage(img, (W - dw) / 2, (H - dh) / 2, dw, dh);
}

async function fetchBackground(url) {
  const buffer = await safeFetchImage(url, { maxBytes: BACKGROUND_MAX_BYTES, timeoutMs: BACKGROUND_TIMEOUT_MS });
  assertImageDimensions(buffer); // refuse a decompression bomb before the decoder sees it (D4)
  const img = await loadImage(buffer);
  const canvas = createCanvas(W, H);
  drawCover(canvas.getContext('2d'), img);
  return canvas.toBuffer('image/png');
}

/**
 * Downloads and cover-crops a background image to the card size, cached for 30 minutes (a failure is cached for 10).
 * Never throws.
 * @param {string|null} url
 * @returns {Promise<Buffer|null>}
 */
export async function loadCardBackground(url) {
  if (typeof url !== 'string' || !url) return null;
  const hit = backgroundCache.get(url);
  if (hit && hit.expiresAt > Date.now()) return hit.png;
  const pending = backgroundLoading.get(url);
  if (pending) return pending;
  const promise = (async () => {
    try {
      const png = await fetchBackground(url);
      cachePut(url, png, BACKGROUND_TTL_MS);
      return png;
    } catch (err) {
      logger.debug(`rank card background ${url} not loaded: ${err?.message ?? err}`);
      cachePut(url, null, BACKGROUND_NEGATIVE_TTL_MS);
      return null;
    }
  })();
  backgroundLoading.set(url, promise);
  promise.finally(() => backgroundLoading.delete(url)).catch(() => {});
  return promise;
}

/** Forgets a cached background (after the style changed). */
export function clearCardBackground(url) {
  if (typeof url === 'string' && url) backgroundCache.delete(url);
}

const FAILURE_REASONS = {
  TIMEOUT: 'it took too long',
  TOO_LARGE: 'it is larger than 8 MB',
  BAD_CONTENT_TYPE: 'it is not an image',
  BLOCKED_ADDRESS: 'that address is not allowed',
  INVALID_URL: 'that address is not allowed',
};

/**
 * Checks that a URL serves a usable image. Never throws.
 * @param {string} url
 * @returns {Promise<{ ok: true } | { ok: false, reason: string }>}
 */
export async function validateCardBackground(url) {
  try {
    const buffer = await safeFetchImage(url, { maxBytes: BACKGROUND_MAX_BYTES, timeoutMs: BACKGROUND_TIMEOUT_MS });
    assertImageDimensions(buffer);
    await loadImage(buffer);
    return { ok: true };
  } catch (err) {
    if (err instanceof SafeFetchError) {
      if (err.code === 'HTTP_ERROR') return { ok: false, reason: `the server answered ${err.status}` };
      if (Object.hasOwn(FAILURE_REASONS, err.code)) return { ok: false, reason: FAILURE_REASONS[err.code] };
    }
    return { ok: false, reason: 'it could not be read' };
  }
}

// ── Colour helpers ────────────────────────────────────────────────────────────

function rgba(hex, alpha) {
  const n = parseInt(hex.slice(1), 16);
  return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${alpha})`;
}

/** Mixes `hex` with white: amount 0 = the colour, 1 = white. */
function lighten(hex, amount) {
  const n = parseInt(hex.slice(1), 16);
  const mix = (c) => Math.round(c + (255 - c) * amount);
  return `#${[(n >> 16) & 255, (n >> 8) & 255, n & 255].map((c) => mix(c).toString(16).padStart(2, '0')).join('')}`;
}

function font(size, family = 'Nunito, sans-serif') {
  return `bold ${size}px ${family}`;
}

/** XP into the current level and XP needed for the next, computed locally (a util must not import a service). */
function levelProgress(totalXp) {
  let level = 0;
  let remaining = Math.max(0, Number(totalXp) || 0);
  while (level < 2000 && remaining >= xpForLevel(level)) {
    remaining -= xpForLevel(level);
    level++;
  }
  return { xpCurrent: remaining, xpNeeded: xpForLevel(level) };
}

/**
 * Generates a rank card PNG buffer.
 *
 * @param {{
 *   avatarBuffer: Buffer|null,
 *   username: string,
 *   displayName: string,
 *   level: number,
 *   totalXp: number,
 *   rank: number,
 *   xpCurrent?: number|null,
 *   xpNeeded?: number|null,
 *   style?: ReturnType<typeof normalizeCardStyle>,
 *   backgroundBuffer?: Buffer|null,
 * }} opts
 * @returns {Promise<Buffer>}
 */
export async function generateProfileCard({
  avatarBuffer, username, displayName, level, totalXp, rank,
  xpCurrent = null, xpNeeded = null, style = DEFAULT_RANK_CARD_STYLE, backgroundBuffer = null,
}) {
  const st = normalizeCardStyle(style);
  const canvas = createCanvas(W, H);
  const ctx    = canvas.getContext('2d');

  // ── Background ─────────────────────────────────────────────────────────────
  ctx.fillStyle = st.background;
  ctx.beginPath();
  roundRect(ctx, 0, 0, W, H, 16);
  ctx.fill();

  if (backgroundBuffer) {
    try {
      assertImageDimensions(backgroundBuffer); // never hand an oversized image to the decoder (D4)
      const bg = await loadImage(backgroundBuffer);
      ctx.save();
      ctx.beginPath();
      roundRect(ctx, 0, 0, W, H, 16);
      ctx.clip();
      drawCover(ctx, bg);
      ctx.fillStyle = `rgba(0,0,0,${st.overlayOpacity})`;
      ctx.fillRect(0, 0, W, H);
      ctx.restore();
    } catch (err) {
      logger.debug(`rank card background skipped: ${err?.message ?? err}`);
    }
  }

  // Subtle left accent gradient
  const accent = ctx.createLinearGradient(0, 0, 220, 0);
  accent.addColorStop(0, rgba(st.accentColor, 0.25));
  accent.addColorStop(1, rgba(st.accentColor, 0));
  ctx.fillStyle = accent;
  ctx.beginPath();
  roundRect(ctx, 0, 0, W, H, 16);
  ctx.fill();

  // ── Avatar ─────────────────────────────────────────────────────────────────
  const AX = 36, AY = 80, AR = 60; // center x, center y, radius

  // Accent ring
  ctx.save();
  ctx.beginPath();
  ctx.arc(AX + AR, AY + AR, AR + 5, 0, Math.PI * 2);
  ctx.fillStyle = st.accentColor;
  ctx.fill();
  ctx.restore();

  // Clip circle for avatar
  ctx.save();
  ctx.beginPath();
  ctx.arc(AX + AR, AY + AR, AR, 0, Math.PI * 2);
  ctx.clip();

  if (avatarBuffer) {
    try {
      assertImageDimensions(avatarBuffer);
      const img = await loadImage(avatarBuffer);
      ctx.drawImage(img, AX, AY, AR * 2, AR * 2);
    } catch {
      ctx.fillStyle = '#40444b';
      ctx.fillRect(AX, AY, AR * 2, AR * 2);
    }
  } else {
    ctx.fillStyle = '#40444b';
    ctx.fillRect(AX, AY, AR * 2, AR * 2);
  }
  ctx.restore();

  // ── Name block ─────────────────────────────────────────────────────────────
  const nameX = AX + AR * 2 + 24;

  ctx.fillStyle = st.textColor;
  ctx.font = font(28);
  ctx.fillText(truncate(ctx, displayName, 380), nameX, 118);

  ctx.fillStyle = GREY;
  ctx.font = font(18);
  ctx.fillText(`@${truncate(ctx, username, 320)}`, nameX, 148);

  // ── Level + Rank (top right) ───────────────────────────────────────────────
  const levelLabel = `LEVEL ${level}`;
  const rankLabel  = `RANK #${formatNumber(rank)}`;

  ctx.fillStyle = st.accentColor;
  ctx.font = font(32);
  const levelW = ctx.measureText(levelLabel).width;
  ctx.fillText(levelLabel, W - levelW - 28, 90);

  ctx.fillStyle = GREY;
  ctx.font = font(18);
  const rankW = ctx.measureText(rankLabel).width;
  ctx.fillText(rankLabel, W - rankW - 28, 120);

  // ── XP progress bar ────────────────────────────────────────────────────────
  const BAR_X = nameX;
  const BAR_Y = 176;
  const BAR_W = W - nameX - 28;
  const BAR_H = 24;

  const fallback = levelProgress(totalXp);
  const current  = Number.isFinite(xpCurrent) ? xpCurrent : fallback.xpCurrent;
  const needed   = Number.isFinite(xpNeeded) && xpNeeded > 0 ? xpNeeded : fallback.xpNeeded;
  const progress = Math.min(Math.max(current / needed, 0), 1);

  // Track
  ctx.fillStyle = BAR_BG;
  ctx.beginPath();
  roundRect(ctx, BAR_X, BAR_Y, BAR_W, BAR_H, BAR_H / 2);
  ctx.fill();

  // Fill
  if (progress > 0) {
    const fillW = Math.max(BAR_H, Math.floor(BAR_W * progress)); // min width keeps ends round
    const barGrad = ctx.createLinearGradient(BAR_X, 0, BAR_X + fillW, 0);
    barGrad.addColorStop(0, st.accentColor);
    barGrad.addColorStop(1, lighten(st.accentColor, 0.3));
    ctx.fillStyle = barGrad;
    ctx.beginPath();
    roundRect(ctx, BAR_X, BAR_Y, fillW, BAR_H, BAR_H / 2);
    ctx.fill();
  }

  // XP label
  ctx.fillStyle = st.textColor;
  ctx.font = font(16);
  const xpText = `${compactNumber(current)} / ${compactNumber(needed)} XP`;
  ctx.fillText(xpText, BAR_X, BAR_Y + BAR_H + 22);

  // Total XP (right-aligned)
  ctx.fillStyle = GREEN;
  ctx.font = font(16);
  const totalText = `Total: ${compactNumber(totalXp)} XP`;
  const totalW = ctx.measureText(totalText).width;
  ctx.fillText(totalText, BAR_X + BAR_W - totalW, BAR_Y + BAR_H + 22);

  return canvas.toBuffer('image/png');
}

// ── Canvas helpers ────────────────────────────────────────────────────────────

function roundRect(ctx, x, y, w, h, r) {
  ctx.moveTo(x + r, y);
  ctx.lineTo(x + w - r, y);
  ctx.quadraticCurveTo(x + w, y, x + w, y + r);
  ctx.lineTo(x + w, y + h - r);
  ctx.quadraticCurveTo(x + w, y + h, x + w - r, y + h);
  ctx.lineTo(x + r, y + h);
  ctx.quadraticCurveTo(x, y + h, x, y + h - r);
  ctx.lineTo(x, y + r);
  ctx.quadraticCurveTo(x, y, x + r, y);
  ctx.closePath();
}

function truncate(ctx, text, maxWidth) {
  if (ctx.measureText(text).width <= maxWidth) return text;
  let t = text;
  while (ctx.measureText(t + '…').width > maxWidth && t.length > 0) t = t.slice(0, -1);
  return t + '…';
}
