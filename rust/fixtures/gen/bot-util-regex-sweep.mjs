// Sweeps what user regexes do in V8 so bot-util's safeRegex (V8 syntax rules + regress matching) can be checked
// beyond hand-picked cases. Writes rust/fixtures/golden/bot-util/regex-sweep.json:
//   fold     [c, r, bits] for every BMP code unit c and each unit r related to it by toUpperCase/toLowerCase:
//            bit 1 /^c$/i matches r, 2 /^[c]$/i, 4 /^c$/iu, 8 /^[c]$/iu
//   classes  { "pattern/flags": [[from, to], ...] } the code units 0..0xFFFF each class-like regex matches
//   random   [pattern, flags, reason|null, [[input, matches], ...]] for seeded random patterns
// Run from the repo root: node rust/fixtures/gen/bot-util-regex-sweep.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'bot-util');
const sr = await import(pathToFileURL(join(ROOT, 'src/utils/safeRegex.js')).href);

const hex4 = (c) => c.toString(16).padStart(4, '0');
const isSurrogate = (c) => c >= 0xd800 && c <= 0xdfff;

// ── case folding of single characters ──────────────────────────────────────────
const related = new Map();
const link = (a, b) => {
  if (a === b || isSurrogate(a) || isSurrogate(b)) return;
  if (!related.has(a)) related.set(a, new Set());
  if (!related.has(b)) related.set(b, new Set());
  related.get(a).add(b);
  related.get(b).add(a);
};
for (let c = 0; c <= 0xffff; c++) {
  if (isSurrogate(c)) continue;
  const ch = String.fromCharCode(c);
  for (const t of [ch.toUpperCase(), ch.toLowerCase(), ch.toUpperCase().toLowerCase(), ch.toLowerCase().toUpperCase()]) {
    if (t.length === 1) link(c, t.charCodeAt(0));
  }
}
const fold = [];
for (const [c, rs] of [...related].sort((a, b) => a[0] - b[0])) {
  const lit = `\\u${hex4(c)}`;
  const res = [new RegExp(`^${lit}$`, 'i'), new RegExp(`^[${lit}]$`, 'i'), new RegExp(`^${lit}$`, 'iu'), new RegExp(`^[${lit}]$`, 'iu')];
  for (const r of [...rs].sort((a, b) => a - b)) {
    const s = String.fromCharCode(r);
    let bits = 0;
    res.forEach((re, i) => { if (re.test(s)) bits |= 1 << i; });
    fold.push([c, r, bits]);
  }
}

// ── class escapes and the dot over every code unit ─────────────────────────────
const classes = {};
for (const pattern of ['^\\w$', '^\\W$', '^\\s$', '^\\S$', '^\\d$', '^.$', '^[^a]$', '^\\p{L}$', '^\\p{N}$', '^[\\p{L}\\p{N}_]$', '^[a-z]$', '^[A-Z]$']) {
  for (const flags of ['', 'i', 'u', 'iu', 's', 'su']) {
    if (pattern.includes('\\p') && !flags.includes('u')) continue;
    const re = new RegExp(pattern, flags);
    const ranges = [];
    for (let c = 0; c <= 0xffff; c++) {
      if (re.test(String.fromCharCode(c))) {
        const last = ranges[ranges.length - 1];
        if (last && last[1] === c - 1) last[1] = c;
        else ranges.push([c, c]);
      }
    }
    classes[`${pattern}/${flags}`] = ranges;
  }
}

// ── random patterns ────────────────────────────────────────────────────────────
let seed = 0x5eed1234;
const rnd = () => {
  seed = (seed * 1103515245 + 12345) & 0x7fffffff;
  return seed / 0x80000000;
};
const pick = (a) => a[Math.floor(rnd() * a.length)];
const CHARS = ['a', 'b', 'x', 'A', 'é', 'ſ', 'K', 'k', 's', 'S', '0', '9', '_', '-', '.', '/', ' ', '😀', 'ı', 'İ', 'ß', 'Ω', 'ω', 'µ', 'μ'];
const ESCAPES = ['\\d', '\\D', '\\w', '\\W', '\\s', '\\S', '\\b', '\\B', '\\.', '\\\\', '\\/', '\\-', '\\u0041', '\\u017f', '\\u212a', '\\x41', '\\cA',
  '\\c', '\\c1', '\\0', '\\00', '\\1', '\\8', '\\k<n>', '\\k', '\\p{L}', '\\p{Lu}', '\\P{L}', '\\p{Script=Greek}', '\\p{Foo}', '\\n', '\\t', '\\q',
  '\\]', '\\{', '\\u{1F600}', '\\uD83D\\uDE00', '\\u00df', '\\u03a9'];
const QUANTS = ['', '', '', '*', '+', '?', '{2}', '{1,3}', '{2,}', '{,2}', '{3,1}', '*?', '+?', '{', '{1', '??'];
function classItem() {
  switch (Math.floor(rnd() * 6)) {
    case 0: return pick(CHARS);
    case 1: return `${pick(CHARS)}-${pick(CHARS)}`;
    case 2: return pick(['\\d', '\\w', '\\s', '\\W', '\\-', '-', '\\b', '\\p{L}', '\\u017f', '\\u0100-\\u017f', 'a-z', 'A-Z', '\\cA', '\\c1']);
    case 3: return pick(['ſ', 'K', 'Ω', 'ß', 'ı']);
    case 4: return `${pick(['a', 'A', '\\u0041', 'Ā'])}-${pick(['z', 'ſ', 'ž', '\\u024f', 'K', 'Z'])}`;
    default: return pick(CHARS);
  }
}
function atom(depth) {
  const r = rnd();
  if (r < 0.35) return pick(CHARS);
  if (r < 0.55) return pick(ESCAPES);
  if (r < 0.7) return `[${rnd() < 0.2 ? '^' : ''}${Array.from({ length: 1 + Math.floor(rnd() * 3) }, classItem).join('')}]`;
  if (r < 0.75) return pick(['^', '$', '.', '|']);
  if (r < 0.78) return pick(['(', ')', '[', ']', '{', '}', '*', '+', '?', '(?', '(?<', '(?<a']);
  if (depth >= 2) return pick(CHARS);
  const open = pick(['(', '(?:', '(?=', '(?!', '(?<=', '(?<!', '(?<n>', '(?<m>', '(?i:', '(?-i:', '(?m:', '(?s-i:', '(?ii:', '(?i)', '(?--i:']);
  return `${open}${seq(depth + 1)})`;
}
function seq(depth) {
  const n = 1 + Math.floor(rnd() * 4);
  let s = '';
  for (let i = 0; i < n; i++) s += atom(depth) + pick(QUANTS);
  return s;
}
const INPUTS = ['', 'a', 'aaa', 'ab', 'xAbé', 'ſK', 'S k', '😀a', 'a-b_9', 'ß', 'İı', '\nab\n', 'Ωω', 'µμ', 'SS', 'kK', 'b x', 'Greek αβ',
  '0/9', 'A.B', 'a\\b', '\u0001'];
const random = [];
for (let i = 0; i < 20000; i++) {
  const pattern = seq(0);
  const flags = pick(['', 'i', 'u', 'iu', 'm', 's', 'imsu', 'mi', 'i', 'i']);
  const v = sr.validateRegex(pattern, flags);
  const tests = [];
  if (v.ok) {
    const re = new RegExp(pattern, flags);
    for (let k = 0; k < 5; k++) {
      const input = pick(INPUTS) + (rnd() < 0.5 ? pick(INPUTS) : '');
      const t0 = process.hrtime.bigint();
      const m = re.test(input);
      if (Number(process.hrtime.bigint() - t0) / 1e6 < 5) tests.push([input, m]);
    }
  }
  random.push([pattern, flags, v.ok ? null : v.reason, tests]);
}

mkdirSync(OUT, { recursive: true });
writeFileSync(join(OUT, 'regex-sweep.json'), `${JSON.stringify({ fold, classes, random })}\n`);
process.stdout.write(`regex-sweep.json: ${fold.length} fold pairs, ${Object.keys(classes).length} classes, ${random.length} random patterns `
  + `(${random.filter((r) => r[2] === null).length} valid)\n`);
