// The RGI emoji ZWJ sequences of this Node's regex engine (`/^\p{RGI_Emoji}$/v`), for bot-core's exact RGI matcher.
// JS cannot list the strings of `\p{RGI_Emoji}`, so they are found by search, and every one written is confirmed by
// V8 itself:
//   E  = every RGI emoji of data/emoji.json that is not a flag, keycap or tag sequence (skin-tone variants included);
//   E0 = E without skin-tone variants;
//   pairs    a‍b       for every a, b in E (exhaustive);
//   triples  a‍b‍c     for a in the tone-free pair heads and the people (also with 🏻), b in E0, c in the tone-free
//                      pair tails and the people (also with 🏼);
//   quads    a‍b‍c‍d   for a in the triple heads, b in the triple middles, c in E0, d as c above;
//   then every skin-tone combination of the people at the first and last position of each triple / quad found.
// Assumptions (true of the Unicode emoji data so far): sequences have at most four elements, only the first and last
// element carry a skin tone, and each toned triple or quad also exists tone-free or as 🏻 … 🏼.
//   node rust/fixtures/gen/bot-core-emoji-zwj.mjs   -> rust/crates/bot-core/data/emoji-zwj.json
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const DATA = join(resolve(dirname(fileURLToPath(import.meta.url)), '..', '..'), 'crates', 'bot-core', 'data');
const RGI = /^\p{RGI_Emoji}$/v;
const MOD_BASE = /^\p{Emoji_Modifier_Base}/u;
const TONE = /[\u{1F3FB}-\u{1F3FF}]/u;
const TONES = ['', '\u{1F3FB}', '\u{1F3FC}', '\u{1F3FD}', '\u{1F3FE}', '\u{1F3FF}'];
const ZWJ = '‍';

const base = JSON.parse(readFileSync(join(DATA, 'emoji.json'), 'utf8'));
const isFlag = (s) => /^[\u{1F1E6}-\u{1F1FF}]{2}$/u.test(s);
const isKeycap = (s) => s.endsWith('⃣');
const isTag = (s) => /[\u{E0020}-\u{E007F}]/u.test(s);
const E = base.rgi.filter((s) => !isFlag(s) && !isKeycap(s) && !isTag(s));
const E0 = E.filter((s) => !TONE.test(s));
const PEOPLE0 = E0.filter((s) => MOD_BASE.test(s));

const found = new Set();
const heads2 = new Set();
const tails2 = new Set(PEOPLE0);
for (const a of E) {
  for (const b of E) {
    const s = a + ZWJ + b;
    if (RGI.test(s)) {
      found.add(s);
      if (!TONE.test(a)) heads2.add(a);
      if (!TONE.test(b)) tails2.add(b);
    }
  }
}
const pairs = found.size;
// People also stand in with a light tone (heads) and a medium-light tone (tails): some sequences exist only with two
// different tones (👩🏻‍🤝‍👨🏼; the tone-free couple is the single 👫).
const inE = new Set(E);
const withTone = (el, t) => [...el].filter((ch) => ch !== '️').join('') + t;
for (const p of PEOPLE0) {
  if (inE.has(withTone(p, '\u{1F3FB}'))) heads2.add(withTone(p, '\u{1F3FB}'));
  if (inE.has(withTone(p, '\u{1F3FC}'))) tails2.add(withTone(p, '\u{1F3FC}'));
}
const tails = [...tails2];
const bases = [];
const heads3 = new Set();
const mids3 = new Set();
for (const a of heads2) {
  for (const b of E0) {
    const ab = a + ZWJ + b + ZWJ;
    for (const c of tails) {
      if (RGI.test(ab + c)) { bases.push([a, b, c]); heads3.add(a); mids3.add(b); }
    }
  }
}
for (const a of heads3) {
  for (const b of mids3) {
    for (const c of E0) {
      const abc = a + ZWJ + b + ZWJ + c + ZWJ;
      for (const d of tails) {
        if (RGI.test(abc + d)) bases.push([a, b, c, d]);
      }
    }
  }
}
// Skin tones on the first and last person.
const untone = (el) => [...el].filter((ch) => !TONE.test(ch)).join('');
const toned = (el) => (MOD_BASE.test(el) ? TONES.map((t) => withTone(untone(el), t)) : [el]);
for (const seq of bases) {
  const first = toned(seq[0]);
  const last = toned(seq[seq.length - 1]);
  for (const f of first) {
    for (const l of last) {
      for (const [x, y] of [[f, l], [seq[0], l], [f, seq[seq.length - 1]]]) {
        const s = [x, ...seq.slice(1, -1), y].join(ZWJ);
        if (RGI.test(s)) found.add(s);
      }
    }
  }
}
const zwj = [...found].sort();
const longer = zwj.length - pairs;
writeFileSync(join(DATA, 'emoji-zwj.json'), `${JSON.stringify({ node: process.version, unicode: process.versions.unicode, zwj })}\n`);
console.log(`wrote emoji-zwj.json: ${zwj.length} ZWJ sequences (${pairs} pairs, ${longer} longer), Unicode ${process.versions.unicode}`);
