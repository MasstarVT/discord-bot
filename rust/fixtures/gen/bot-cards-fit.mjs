// Produces rust/fixtures/golden/bot-cards/fit.json: text widths, welcome-card fitText sizes and profile-card truncate
// results computed by @napi-rs/canvas with the bundled Nunito-Bold font, using the exact fitting logic of
// src/utils/welcomeCard.js (fitText, clampText) and src/utils/profileCard.js (truncate, font()).
// Run from the repository root: node rust/fixtures/gen/bot-cards-fit.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { createCanvas, GlobalFonts } from '@napi-rs/canvas';
import { clampText } from '../../../src/utils/text.js';

GlobalFonts.registerFromPath('assets/fonts/Nunito-Bold.ttf', 'Nunito');
const ctx = createCanvas(10, 10).getContext('2d');

// Only characters Nunito has: on this machine napi-rs falls back to system fonts for the rest, the deployed image has none.
const SAMPLES = [
  '', 'nova', 'Night Owls', 'NIGHT OWLS', 'WELCOME TO', 'Member #1,284', 'LEVEL 12', 'RANK #4', 'RANK #1,234,567',
  '1.2K / 1.5K XP', 'Total: 18.7K XP', 'The Quick Brown Fox Jumps Over', 'AVAVAVAVAVAV Typography To Ta Yo',
  'WWWWWWWWWWWWWWWWWWWWWWWWWW', 'iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii', 'Supercalifragilisticexpialidocious_Name',
  'x_xX_Gamer_Xx_x 1234567890', 'MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM', 'Ünïcödé Ñámé Çàfé', '@nova',
  'THE VERY LONG SERVER NAME OF DOOM', 'A Cosy Little Server For Friends', 'mmmmmmmmmmmmmmmmmmmmmmmmmm', 'Ab, cd. ef! (gh) [ij] {kl}',
];

const widths = [];
for (const family of ['Nunito', 'Nunito, sans-serif']) {
  for (const weight of ['', 'bold ']) {
    for (const size of [14, 16, 18, 20, 26, 28, 32, 36]) {
      ctx.font = `${weight}${size}px ${family}`;
      for (const text of SAMPLES) widths.push({ font: ctx.font, size, text, width: ctx.measureText(text).width });
    }
  }
}

// welcomeCard.js fitText (font family 'Nunito', textMaxWidth = 700 - 200 - 20).
function fitText(text, maxWidth, fontSize, fontFamily) {
  let size = fontSize;
  ctx.font = `bold ${size}px ${fontFamily}`;
  while (ctx.measureText(text).width > maxWidth && size > 14) {
    size -= 2;
    ctx.font = `bold ${size}px ${fontFamily}`;
  }
  return size;
}
const welcome = [];
for (const raw of SAMPLES) {
  const serverName = clampText(String(raw).toUpperCase(), 32);
  const displayName = clampText(raw, 26);
  welcome.push({
    input: raw,
    serverName, serverSize: fitText(serverName, 480, 26, 'Nunito'),
    displayName, displaySize: fitText(displayName, 480, 36, 'Nunito'),
    username: `@${clampText(raw, 28)}`,
  });
}

// profileCard.js truncate with font(28) for 380 and font(18) for 320.
function truncate(text, maxWidth) {
  if (ctx.measureText(text).width <= maxWidth) return text;
  let t = text;
  while (ctx.measureText(t + '…').width > maxWidth && t.length > 0) t = t.slice(0, -1);
  return t + '…';
}
const profile = [];
for (const raw of SAMPLES) {
  ctx.font = 'bold 28px Nunito, sans-serif';
  const displayName = truncate(raw, 380);
  ctx.font = 'bold 18px Nunito, sans-serif';
  const username = truncate(raw, 320);
  profile.push({ input: raw, displayName, username });
}

mkdirSync('rust/fixtures/golden/bot-cards', { recursive: true });
writeFileSync('rust/fixtures/golden/bot-cards/fit.json', `${JSON.stringify({ widths, welcome, profile }, null, 1)}\n`);
process.stdout.write(`fit.json: ${widths.length} widths, ${welcome.length} welcome, ${profile.length} profile\n`);
