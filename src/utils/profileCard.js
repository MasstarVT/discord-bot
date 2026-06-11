import { createCanvas, loadImage, GlobalFonts } from '@napi-rs/canvas';
import { existsSync }  from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath }  from 'url';
import { xpForLevel }    from '../config/constants.js';
import { xpIntoLevel }   from '../services/xpService.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const FONT_PATH = join(__dirname, '../../assets/fonts/Nunito-Bold.ttf');

if (existsSync(FONT_PATH)) {
  GlobalFonts.registerFromPath(FONT_PATH, 'Nunito');
}

const W = 900;
const H = 280;

const BLURPLE  = '#5865F2';
const WHITE    = '#FFFFFF';
const GREY     = '#B9BBBE';
const DARK_BG  = '#2b2d31';
const BAR_BG   = '#3f4147';
const BAR_FILL = '#5865F2';
const GREEN    = '#57F287';

function font(size, family = 'Nunito, sans-serif') {
  return `bold ${size}px ${family}`;
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
 * }} opts
 * @returns {Promise<Buffer>}
 */
export async function generateProfileCard({ avatarBuffer, username, displayName, level, totalXp, rank }) {
  const canvas = createCanvas(W, H);
  const ctx    = canvas.getContext('2d');

  // ── Background ─────────────────────────────────────────────────────────────
  ctx.fillStyle = DARK_BG;
  ctx.beginPath();
  roundRect(ctx, 0, 0, W, H, 16);
  ctx.fill();

  // Subtle left accent gradient
  const accent = ctx.createLinearGradient(0, 0, 220, 0);
  accent.addColorStop(0, 'rgba(88,101,242,0.25)');
  accent.addColorStop(1, 'rgba(88,101,242,0)');
  ctx.fillStyle = accent;
  ctx.beginPath();
  roundRect(ctx, 0, 0, W, H, 16);
  ctx.fill();

  // ── Avatar ─────────────────────────────────────────────────────────────────
  const AX = 36, AY = 80, AR = 60; // center x, center y, radius

  // Blurple ring
  ctx.save();
  ctx.beginPath();
  ctx.arc(AX + AR, AY + AR, AR + 5, 0, Math.PI * 2);
  ctx.fillStyle = BLURPLE;
  ctx.fill();
  ctx.restore();

  // Clip circle for avatar
  ctx.save();
  ctx.beginPath();
  ctx.arc(AX + AR, AY + AR, AR, 0, Math.PI * 2);
  ctx.clip();

  if (avatarBuffer) {
    try {
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

  ctx.fillStyle = WHITE;
  ctx.font = font(28);
  ctx.fillText(truncate(ctx, displayName, 380), nameX, 118);

  ctx.fillStyle = GREY;
  ctx.font = font(18);
  ctx.fillText(`@${truncate(ctx, username, 320)}`, nameX, 148);

  // ── Level + Rank (top right) ───────────────────────────────────────────────
  const levelLabel = `LEVEL ${level}`;
  const rankLabel  = `RANK #${rank}`;

  ctx.fillStyle = BLURPLE;
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

  const xpCurrent = xpIntoLevel(totalXp);
  const xpNeeded  = xpForLevel(level);
  const progress  = Math.min(xpCurrent / xpNeeded, 1);

  // Track
  ctx.fillStyle = BAR_BG;
  ctx.beginPath();
  roundRect(ctx, BAR_X, BAR_Y, BAR_W, BAR_H, BAR_H / 2);
  ctx.fill();

  // Fill
  if (progress > 0) {
    const fillW = Math.max(BAR_H, Math.floor(BAR_W * progress)); // min width keeps ends round
    const barGrad = ctx.createLinearGradient(BAR_X, 0, BAR_X + fillW, 0);
    barGrad.addColorStop(0, BLURPLE);
    barGrad.addColorStop(1, '#7289DA');
    ctx.fillStyle = barGrad;
    ctx.beginPath();
    roundRect(ctx, BAR_X, BAR_Y, fillW, BAR_H, BAR_H / 2);
    ctx.fill();
  }

  // XP label
  ctx.fillStyle = WHITE;
  ctx.font = font(16);
  const xpText = `${fmtNum(xpCurrent)} / ${fmtNum(xpNeeded)} XP`;
  ctx.fillText(xpText, BAR_X, BAR_Y + BAR_H + 22);

  // Total XP (right-aligned)
  ctx.fillStyle = GREEN;
  ctx.font = font(16);
  const totalText = `Total: ${fmtNum(totalXp)} XP`;
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

function fmtNum(n) {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M';
  if (n >= 1_000)     return (n / 1_000).toFixed(1) + 'K';
  return String(n);
}
