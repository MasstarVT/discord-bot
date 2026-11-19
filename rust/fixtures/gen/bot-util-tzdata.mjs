// Dumps the ICU time zone data the JS bot relies on (through Intl) so bot-util reproduces it exactly.
// Run from the repo root: node rust/fixtures/gen/bot-util-tzdata.mjs [path to chrono-tz's tz directory]
// Writes rust/crates/bot-util/data/tz.json (embedded in the crate at build time):
//   list      listTimeZones() of src/utils/timezones.js, verbatim
//   ids       every id Intl accepts (lower-cased) → [resolvedOptions().timeZone, chrono-tz zone used for offsets]
//   rank      localeCompare order (en-US ICU collation) of every string the JS sorts with localeCompare
//   tzdata    Node's tzdata version (chrono-tz may ship an older one; see the report)
import { readdirSync, readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (p) => import(pathToFileURL(join(ROOT, p)).href);
const { listTimeZones } = await imp('src/utils/timezones.js');

function findTzDir() {
  if (process.argv[2]) return process.argv[2];
  const reg = join(homedir(), '.cargo', 'registry', 'src');
  for (const idx of readdirSync(reg)) {
    for (const d of readdirSync(join(reg, idx))) {
      if (d.startsWith('chrono-tz-0.10.')) return join(reg, idx, d, 'tz');
    }
  }
  throw new Error('chrono-tz tz directory not found; pass it as the first argument');
}

const tzDir = findTzDir();
const chronoNames = new Set();
for (const f of ['africa', 'antarctica', 'asia', 'australasia', 'backward', 'etcetera', 'europe', 'northamerica', 'southamerica']) {
  const p = join(tzDir, f);
  if (!existsSync(p)) continue;
  for (const line of readFileSync(p, 'utf8').split('\n')) {
    const m = /^Zone\s+(\S+)/.exec(line) ?? /^Link\s+\S+\s+(\S+)/.exec(line);
    if (m) chronoNames.add(m[1]);
  }
}

const canonicalOf = (t) => {
  try {
    return new Intl.DateTimeFormat('en-US', { timeZone: t }).resolvedOptions().timeZone;
  } catch {
    return null;
  }
};

// Candidates: chrono-tz names and links, Intl's own list, every 1-4 letter upper-case word (ICU's legacy ids such as
// PST, ACT, CTT), SystemV zones, and a few odd spellings.
const candidates = new Set([...chronoNames, ...Intl.supportedValuesOf('timeZone')]);
const A = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ';
const words = [''];
for (let len = 1; len <= 4; len++) {
  for (const w of words.filter((x) => x.length === len - 1)) for (const c of A) words.push(w + c);
}
for (const w of words) if (w) candidates.add(w);
for (const s of ['AST4', 'AST4ADT', 'CST6', 'CST6CDT', 'EST5', 'EST5EDT', 'HST10', 'MST7', 'MST7MDT', 'PST8', 'PST8PDT', 'YST9', 'YST9YDT']) {
  candidates.add(`SystemV/${s}`);
}
for (const s of ['GMT0', 'GMT+0', 'GMT-0', 'Etc/Unknown', 'Etc/GMT0', 'Etc/GMT+0', 'Etc/GMT-0', 'Etc/UCT', 'Etc/Universal', 'Etc/Zulu', 'Etc/Greenwich', 'UCT', 'Universal', 'Zulu', 'Greenwich']) candidates.add(s);
for (let h = 0; h <= 14; h++) { candidates.add(`Etc/GMT+${h}`); candidates.add(`Etc/GMT-${h}`); candidates.add(`GMT+${h}`); candidates.add(`GMT-${h}`); }

// Offsets of a zone at sample instants, to map an ICU-only id onto a chrono-tz zone with the same rules.
const SAMPLES = [];
for (let y = 1970; y <= 2040; y++) for (const m of [0, 3, 6, 9]) SAMPLES.push(Date.UTC(y, m, 15));
const offsetSig = (zone) => {
  const f = new Intl.DateTimeFormat('en-US', { timeZone: zone, timeZoneName: 'longOffset' });
  return SAMPLES.map((t) => f.formatToParts(t).find((p) => p.type === 'timeZoneName').value).join(',');
};
const sigCache = new Map();
const sigOf = (z) => {
  if (!sigCache.has(z)) sigCache.set(z, offsetSig(z));
  return sigCache.get(z);
};

// SystemV zones with daylight saving follow the old US rules, which no tzdata zone keeps; use the nearest region.
const SYSTEMV_DST = {
  'SystemV/AST4ADT': 'America/Halifax', 'SystemV/EST5EDT': 'EST5EDT', 'SystemV/CST6CDT': 'CST6CDT',
  'SystemV/MST7MDT': 'MST7MDT', 'SystemV/PST8PDT': 'PST8PDT', 'SystemV/YST9YDT': 'America/Anchorage',
};
const ids = {};
for (const c of [...candidates].sort()) {
  const canonical = canonicalOf(c);
  if (!canonical) continue;
  let chrono = chronoNames.has(canonical) ? canonical : chronoNames.has(c) ? c : SYSTEMV_DST[canonical] ?? null;
  if (!chrono) {
    const want = sigOf(canonical);
    chrono = [...chronoNames].sort().find((z) => canonicalOf(z) && sigOf(z) === want) ?? 'UTC';
  }
  const key = c.toLowerCase();
  if (!ids[key]) ids[key] = [canonical, chrono];
}

// localeCompare ranks: the zone list and every 'UTC±HH:MM' label at 15-minute steps.
const labels = [];
for (let m = -24 * 60 + 15; m < 24 * 60; m += 15) {
  const a = Math.abs(m);
  labels.push(`UTC${m < 0 ? '-' : '+'}${String(Math.floor(a / 60)).padStart(2, '0')}:${String(a % 60).padStart(2, '0')}`);
}
const toRank = [...new Set([...listTimeZones(), ...Object.values(ids).map(([c]) => c), ...labels])]
  .sort((a, b) => a.localeCompare(b));
const rank = Object.fromEntries(toRank.map((s, i) => [s, i]));

// Zones whose rules in chrono-tz's (older) tzdata differ from Node's ICU, found by bot-util's tzdata test: their full
// ICU transition list from 1900 on is embedded and used instead of chrono-tz (keyed by ICU's canonical id). The SystemV
// zones with daylight saving follow old US rules no tzdata zone keeps, so they are tabulated the same way.
const OVERRIDE_ZONES = ['America/Vancouver', 'America/Tijuana', 'SystemV/AST4ADT', 'SystemV/CST6CDT', 'SystemV/EST5EDT', 'SystemV/MST7MDT',
  'SystemV/PST8PDT', 'SystemV/YST9YDT'];
const secsOf = (f, t) => {
  const raw = f.formatToParts(t).find((p) => p.type === 'timeZoneName').value.replace('−', '-');
  const m = /^GMT([+-])(\d{2}):(\d{2})(?::(\d{2}))?$/.exec(raw);
  return m ? (m[1] === '-' ? -1 : 1) * (Number(m[2]) * 3600 + Number(m[3]) * 60 + Number(m[4] ?? 0)) : 0;
};
const overrides = {};
for (const zone of OVERRIDE_ZONES) {
  const f = new Intl.DateTimeFormat('en-US', { timeZone: zone, timeZoneName: 'longOffset' });
  const STEP = 6 * 3600_000;
  let t = Date.UTC(1900, 0, 1);
  const end = Date.UTC(2100, 0, 1);
  let cur = secsOf(f, t);
  const list = [[t, cur]];
  while (t < end) {
    const next = t + STEP;
    const o = secsOf(f, next);
    if (o !== cur) {
      let lo = t;
      let hi = next; // offset changes in (lo, hi]
      while (hi - lo > 1000) {
        const mid = lo + Math.floor((hi - lo) / 2000) * 1000;
        if (secsOf(f, mid) === cur) lo = mid; else hi = mid;
      }
      list.push([hi, o]);
      cur = o;
    }
    t = next;
  }
  overrides[zone] = list;
}
// Every alias of these zones resolves to the zone itself (tz.json ids map aliases to the canonical chrono-tz zone).

const out = join(ROOT, 'rust', 'crates', 'bot-util', 'data');
mkdirSync(out, { recursive: true });
writeFileSync(join(out, 'tz.json'), `${JSON.stringify({
  tzdata: process.versions.tz, icu: process.versions.icu, list: listTimeZones(), ids, rank, overrides,
})}\n`);
process.stdout.write(`tz.json: ${listTimeZones().length} zones, ${Object.keys(ids).length} ids, ${toRank.length} ranks\n`);

// Offsets of every accepted id on the 1st and 15th of each month 1970-2014 and every day 2015-2040 (noon UTC), for the
// tzdata comparison test (chrono-tz may ship an older tzdata than Node's ICU). Changes only: [sample index, seconds].
const GOLDEN = join(ROOT, 'rust', 'fixtures', 'golden', 'bot-util');
mkdirSync(GOLDEN, { recursive: true });
const samples = [];
for (let y = 1970; y <= 2014; y++) for (let m = 0; m < 12; m++) for (const d of [1, 15]) samples.push(Date.UTC(y, m, d, 12));
for (let t = Date.UTC(2015, 0, 1, 12); t <= Date.UTC(2040, 11, 31, 12); t += 86_400_000) samples.push(t);
const offsets = {};
const allIds = [...new Set([...listTimeZones(), ...Object.keys(ids)])];
for (const zone of allIds) {
  const f = new Intl.DateTimeFormat('en-US', { timeZone: zone, timeZoneName: 'longOffset' });
  const changes = [];
  let last = null;
  samples.forEach((t, i) => {
    const raw = f.formatToParts(t).find((p) => p.type === 'timeZoneName').value.replace('−', '-');
    const m = /^GMT([+-])(\d{2}):(\d{2})(?::(\d{2}))?$/.exec(raw);
    const secs = m ? (m[1] === '-' ? -1 : 1) * (Number(m[2]) * 3600 + Number(m[3]) * 60 + Number(m[4] ?? 0)) : 0;
    if (secs !== last) { changes.push([i, secs]); last = secs; }
  });
  offsets[zone] = changes;
}
writeFileSync(join(GOLDEN, 'tz-offsets.json'), `${JSON.stringify({ tzdata: process.versions.tz, samples, offsets })}\n`);
process.stdout.write(`tz-offsets.json: ${Object.keys(offsets).length} zones x ${samples.length} samples\n`);
