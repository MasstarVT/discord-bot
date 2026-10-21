// Renders the sample rank card and welcome card the landing page's demo shows (src/dashboard/public/demo/), with the
// bot's own card renderers and made-up data: member "nova" on the server "Night Owls". Run: npm run docs:cards
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createCanvas } from '@napi-rs/canvas';
import { generateProfileCard } from '../src/utils/profileCard.js';
import { generateWelcomeCard } from '../src/utils/welcomeCard.js';

const OUT = join(resolve(dirname(fileURLToPath(import.meta.url)), '..'), 'src', 'dashboard', 'public', 'demo');

// The sample avatar: a flat disc with an initial (the welcome card loads avatars from Discord, so it shows its own
// placeholder instead).
const canvas = createCanvas(256, 256);
const ctx = canvas.getContext('2d');
ctx.fillStyle = '#E8853D';
ctx.fillRect(0, 0, 256, 256);
ctx.fillStyle = '#FFF4E8';
ctx.font = 'bold 120px sans-serif';
ctx.textAlign = 'center';
ctx.textBaseline = 'middle';
ctx.fillText('N', 128, 138);

mkdirSync(OUT, { recursive: true });
writeFileSync(join(OUT, 'rank-card.png'), await generateProfileCard({
  avatarBuffer: canvas.toBuffer('image/png'), username: 'nova', displayName: 'nova',
  level: 12, totalXp: 18_740, rank: 4, xpCurrent: 1_240, xpNeeded: 1_500,
}));
writeFileSync(join(OUT, 'welcome-card.png'), await generateWelcomeCard(
  { displayName: 'nova', user: { username: 'nova' } },
  { name: 'Night Owls', memberCount: 1_284 },
));
process.stdout.write(`demo cards written to ${OUT}\n`);
process.exit(0);
