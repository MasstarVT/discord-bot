// Renders reference cards with the JS renderers (src/utils/welcomeCard.js, src/utils/profileCard.js) into
// rust/fixtures/golden/bot-cards/, plus the inputs the Rust tests reuse: the demo avatar of scripts/render-demo-cards.js
// (avatar.png) and a synthetic background image (background.png). See rust/crates/bot-cards/tests/cards.rs.
// Run from the repository root: node rust/fixtures/gen/bot-cards-render.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { createCanvas } from '@napi-rs/canvas';
import { generateProfileCard } from '../../../src/utils/profileCard.js';
import { generateWelcomeCard } from '../../../src/utils/welcomeCard.js';

const OUT = 'rust/fixtures/golden/bot-cards';
mkdirSync(OUT, { recursive: true });

// The demo avatar, exactly as scripts/render-demo-cards.js draws it.
const a = createCanvas(256, 256);
const actx = a.getContext('2d');
actx.fillStyle = '#E8853D';
actx.fillRect(0, 0, 256, 256);
actx.fillStyle = '#FFF4E8';
actx.font = 'bold 120px sans-serif';
actx.textAlign = 'center';
actx.textBaseline = 'middle';
actx.fillText('N', 128, 138);
const avatar = a.toBuffer('image/png');
writeFileSync(`${OUT}/avatar.png`, avatar);

// A 400x300 background: diagonal colour bands, so the cover crop is visible.
const b = createCanvas(400, 300);
const bctx = b.getContext('2d');
const g = bctx.createLinearGradient(0, 0, 400, 300);
g.addColorStop(0, '#ff3366');
g.addColorStop(0.5, '#33ccff');
g.addColorStop(1, '#ffee33');
bctx.fillStyle = g;
bctx.fillRect(0, 0, 400, 300);
bctx.fillStyle = '#000000';
bctx.fillRect(190, 0, 20, 300);
const background = b.toBuffer('image/png');
writeFileSync(`${OUT}/background.png`, background);

// The cover-fitted 900x280 PNG profileCard.js caches for a background URL (fetchBackground without the fetch).
const pc = createCanvas(900, 280);
{
  const ctx = pc.getContext('2d');
  const { loadImage } = await import('@napi-rs/canvas');
  const img = await loadImage(background);
  const scale = Math.max(900 / img.width, 280 / img.height);
  ctx.drawImage(img, (900 - img.width * scale) / 2, (280 - img.height * scale) / 2, img.width * scale, img.height * scale);
}
const profileBg = await pc.encode('png');

const base = {
  avatarBuffer: avatar, username: 'nova', displayName: 'nova', level: 12, totalXp: 18_740, rank: 4, xpCurrent: 1_240, xpNeeded: 1_500,
};
const profiles = {
  'profile-demo': base,
  'profile-styled': { ...base, style: { background: '#102030', accentColor: '#ff8800', textColor: '#eeeeaa', overlayOpacity: 0.3 } },
  'profile-background': { ...base, backgroundBuffer: profileBg, style: { overlayOpacity: 0.3 } },
  'profile-no-avatar': { ...base, avatarBuffer: null, xpCurrent: 0 },
  'profile-bad-avatar': { ...base, avatarBuffer: Buffer.from('not an image'), xpCurrent: null, xpNeeded: null },
  'profile-long': {
    ...base, displayName: 'Supercalifragilisticexpialidocious_Name', username: 'MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM',
    level: 250, rank: 1_234_567, totalXp: 987_654_321, xpCurrent: 5, xpNeeded: 1_000_000,
  },
};
for (const [name, opts] of Object.entries(profiles)) writeFileSync(`${OUT}/${name}.png`, await generateProfileCard(opts));

const nova = { displayName: 'nova', user: { username: 'nova' } };
const owls = { name: 'Night Owls', memberCount: 1_284 };
const welcomes = {
  'welcome-demo': [nova, owls, null],
  'welcome-styled': [nova, owls, { background: '#102030', accentColor: '#ff8800', textColor: '#eeeeaa' }],
  'welcome-long': [
    { displayName: 'Supercalifragilisticexpialidocious_Name', user: { username: 'x_xX_Gamer_Xx_x 1234567890_and_more' } },
    { name: 'The Very Long Server Name Of Doom And Gloom', memberCount: 1_234_567 },
    { accentColor: '#22aa44' },
  ],
};
for (const [name, args] of Object.entries(welcomes)) writeFileSync(`${OUT}/${name}.png`, await generateWelcomeCard(...args));

process.stdout.write(`reference cards written to ${OUT}\n`);
process.exit(0);
