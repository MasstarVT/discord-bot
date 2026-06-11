import { createCanvas, loadImage, GlobalFonts } from '@napi-rs/canvas';
import { existsSync } from 'fs';
import { resolve, dirname } from 'path';
import { fileURLToPath } from 'url';
import logger from './logger.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const FONT_PATH  = resolve(__dirname, '../../assets/fonts/Nunito-Bold.ttf');

// Register bundled font once at module load if present
let FONT_FAMILY = 'sans-serif';
if (existsSync(FONT_PATH)) {
  try {
    GlobalFonts.registerFromPath(FONT_PATH, 'Nunito');
    FONT_FAMILY = 'Nunito';
  } catch {
    // Font registration failed — fall back to system font
  }
}

// ── Drawing helpers ────────────────────────────────────────────────────────────

function roundedRect(ctx, x, y, w, h, r) {
  ctx.beginPath();
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

function fitText(ctx, text, maxWidth, fontSize, fontFamily) {
  let size = fontSize;
  ctx.font = `bold ${size}px ${fontFamily}`;
  while (ctx.measureText(text).width > maxWidth && size > 14) {
    size -= 2;
    ctx.font = `bold ${size}px ${fontFamily}`;
  }
  return size;
}

// ── Card dimensions ────────────────────────────────────────────────────────────

const W = 700, H = 250;
const AVATAR_X = 100, AVATAR_Y = 125, AVATAR_R = 65;
const TEXT_X = 200;

// Discord palette
const COLOR_BG        = '#23272a';
const COLOR_BG2       = '#2c2f33';
const COLOR_BLURPLE   = '#5865F2';
const COLOR_WHITE     = '#ffffff';
const COLOR_SECONDARY = '#b9bbbe';
const COLOR_ACCENT    = '#57F287';

/**
 * Generates a 700x250 welcome card PNG buffer.
 * Falls back gracefully — callers should catch and proceed without the card.
 *
 * @param {import('discord.js').GuildMember} member
 * @param {import('discord.js').Guild} guild
 * @returns {Promise<Buffer>} PNG buffer
 */
export async function generateWelcomeCard(member, guild) {
  const canvas = createCanvas(W, H);
  const ctx    = canvas.getContext('2d');

  // ── Background ──────────────────────────────────────────────────────────────
  const bgGrad = ctx.createLinearGradient(0, 0, W, 0);
  bgGrad.addColorStop(0, COLOR_BG);
  bgGrad.addColorStop(1, COLOR_BG2);
  ctx.fillStyle = bgGrad;
  ctx.fillRect(0, 0, W, H);

  // Blurple accent overlay (right side)
  const accentGrad = ctx.createRadialGradient(W, H / 2, 0, W, H / 2, 350);
  accentGrad.addColorStop(0, 'rgba(88,101,242,0.18)');
  accentGrad.addColorStop(1, 'transparent');
  ctx.fillStyle = accentGrad;
  ctx.fillRect(0, 0, W, H);

  // Bottom accent bar
  ctx.fillStyle = COLOR_BLURPLE;
  ctx.fillRect(0, H - 5, W, 5);

  // ── Avatar ring (blurple circle behind avatar) ──────────────────────────────
  ctx.save();
  ctx.beginPath();
  ctx.arc(AVATAR_X, AVATAR_Y, AVATAR_R + 6, 0, Math.PI * 2);
  ctx.fillStyle = COLOR_BLURPLE;
  ctx.fill();
  ctx.restore();

  // Avatar image
  try {
    const avatarURL = member.user.displayAvatarURL({ extension: 'png', size: 256 });
    const avatar    = await loadImage(avatarURL);
    ctx.save();
    ctx.beginPath();
    ctx.arc(AVATAR_X, AVATAR_Y, AVATAR_R, 0, Math.PI * 2);
    ctx.clip();
    ctx.drawImage(avatar, AVATAR_X - AVATAR_R, AVATAR_Y - AVATAR_R, AVATAR_R * 2, AVATAR_R * 2);
    ctx.restore();
  } catch {
    // Draw fallback circle if avatar fails to load
    ctx.save();
    ctx.beginPath();
    ctx.arc(AVATAR_X, AVATAR_Y, AVATAR_R, 0, Math.PI * 2);
    ctx.fillStyle = '#4f545c';
    ctx.fill();
    ctx.restore();
  }

  // ── Text ────────────────────────────────────────────────────────────────────
  const textMaxWidth = W - TEXT_X - 20;

  // "WELCOME TO" label
  ctx.fillStyle = COLOR_SECONDARY;
  ctx.font      = `18px ${FONT_FAMILY}`;
  ctx.fillText('WELCOME TO', TEXT_X, 75);

  // Server name
  const serverName = guild.name.toUpperCase();
  fitText(ctx, serverName, textMaxWidth, 26, FONT_FAMILY);
  ctx.fillStyle = COLOR_BLURPLE;
  ctx.fillText(serverName.slice(0, 32), TEXT_X, 108);

  // Divider line
  ctx.strokeStyle = '#4f545c';
  ctx.lineWidth   = 1;
  ctx.beginPath();
  ctx.moveTo(TEXT_X, 120);
  ctx.lineTo(W - 20, 120);
  ctx.stroke();

  // Display name (nickname > username)
  const displayName = member.displayName ?? member.user.username;
  fitText(ctx, displayName, textMaxWidth, 36, FONT_FAMILY);
  ctx.fillStyle = COLOR_WHITE;
  ctx.fillText(displayName.slice(0, 26), TEXT_X, 158);

  // Username handle
  ctx.fillStyle = COLOR_SECONDARY;
  ctx.font      = `18px ${FONT_FAMILY}`;
  ctx.fillText(`@${member.user.username.slice(0, 28)}`, TEXT_X, 182);

  // Member count
  ctx.fillStyle = COLOR_ACCENT;
  ctx.font      = `20px ${FONT_FAMILY}`;
  ctx.fillText(`Member #${guild.memberCount.toLocaleString()}`, TEXT_X, 212);

  return canvas.toBuffer('image/png');
}
