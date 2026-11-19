// Emoji data for customEmbed's button-emoji check (`/^(?:\p{RGI_Emoji}|\p{Extended_Pictographic}️?)$/v`), taken from
// this Node's own regex engine: the Extended_Pictographic code point ranges, and every RGI emoji that is a single
// code point (with or without U+FE0F), a keycap, a flag, a skin-tone modifier sequence or a subdivision flag. RGI ZWJ
// sequences cannot be enumerated from JS; Rust checks those structurally.
//   node rust/fixtures/gen/bot-core-emoji.mjs   -> rust/crates/bot-core/data/emoji.json (compiled into bot-core)
import { writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const OUT = join(resolve(dirname(fileURLToPath(import.meta.url)), '..', '..'), 'crates', 'bot-core', 'data', 'emoji.json');
const RGI = /^\p{RGI_Emoji}$/v;
const PICT = /^\p{Extended_Pictographic}$/u;
const MOD_BASE = /^\p{Emoji_Modifier_Base}$/u;

const ranges = [];
let start = -1;
for (let cp = 0; cp <= 0x10ffff; cp++) {
  if (cp >= 0xd800 && cp <= 0xdfff) { if (start >= 0) { ranges.push([start, cp - 1]); start = -1; } continue; }
  const on = PICT.test(String.fromCodePoint(cp));
  if (on && start < 0) start = cp;
  if (!on && start >= 0) { ranges.push([start, cp - 1]); start = -1; }
}
if (start >= 0) ranges.push([start, 0x10ffff]);

const rgi = new Set();
const add = (s) => { if (RGI.test(s)) rgi.add(s); };
for (let cp = 0; cp <= 0x10ffff; cp++) {
  if (cp >= 0xd800 && cp <= 0xdfff) continue;
  const c = String.fromCodePoint(cp);
  add(c);
  add(`${c}️`);
  if (MOD_BASE.test(c)) for (let m = 0x1f3fb; m <= 0x1f3ff; m++) add(c + String.fromCodePoint(m));
}
for (const k of '0123456789#*') add(`${k}️⃣`);
for (let a = 0; a < 26; a++) for (let b = 0; b < 26; b++) add(String.fromCodePoint(0x1f1e6 + a, 0x1f1e6 + b));
const tag = (s) => [...s].map((ch) => String.fromCodePoint(0xe0000 + ch.charCodeAt(0))).join('');
const letters = 'abcdefghijklmnopqrstuvwxyz';
for (const x of letters) for (const y of letters) for (const z of letters) add(`\u{1F3F4}${tag(`gb${x}${y}${z}`)}\u{E007F}`);

writeFileSync(OUT, `${JSON.stringify({ extendedPictographic: ranges, rgi: [...rgi].sort() })}\n`);
console.log(`wrote ${OUT}: ${ranges.length} ranges, ${rgi.size} RGI emoji`);
