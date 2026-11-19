// Differential fuzz for the string parsers of src/utils that bot-util ports: random (seeded, so deterministic) inputs
// built from awkward tokens (non-ASCII letters, case-folding traps, full-width and other digits, separators) are run
// through the real JS functions and written to rust/fixtures/golden/bot-util/fuzz.json in the tables.json format. The
// Rust golden replay tests replay these pairs too.
// Run from the repo root: node rust/fixtures/gen/bot-util-fuzz.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { enc } from './bot-util-encode.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'bot-util');
const imp = (p) => import(pathToFileURL(join(ROOT, p)).href);

const text = await imp('src/utils/text.js');
const dur = await imp('src/utils/parseDuration.js');
const time = await imp('src/utils/time.js');
const tzs = await imp('src/utils/timezones.js');
const tm = await imp('src/utils/timeMentions.js');
const sr = await imp('src/utils/safeRegex.js');
const cal = await imp('src/utils/calendar.js');

let seed = 0x5eed1234;
const rnd = () => {
  seed = (seed * 1103515245 + 12345) >>> 0;
  return seed / 2 ** 32;
};
const pick = (a) => a[Math.floor(rnd() * a.length)];

const TOKENS = [
  'fri', 'Friday', 'next', 'this', 'at', 'today', 'tomorrow', 'tmrw', 'tonight', 'noon', 'midnight', 'oct', 'October', '20th', '1st',
  '7pm', '7 pm', '12:30', '23:59', '9am', '0:00', '25:00', '2026-12-25', '12-25', '3/14', '1990', 'utc', 'utc+2', 'gmt-5:30', 'UTC+0500',
  'est', 'CST', 'ist', 'Z', 'zulu', 'Europe/London', 'america/new_york', 'Asia/Calcutta', '+05:30', '-0800', '+1é1',
  '1h', '1.5h', '2h30m', '5 minutes', 'and', ',', '1 month', '3mo', '1y', '.5d', '10s', '5ms', 'wk', 'yrs',
  'é', 'É', 'İ', 'ı', 'ß', 'ẞ', 'ſ', 'K', 'Ω', 'ﬀ', '😀', '🇺🇸', '​', ' ', ' ', '\t', '１２', '٣', '−', '–', '—',
  ' ', ' ', ' ', '-', '/', ':', '.', '(', ')', '[', '*', '\\', '$', '^', '|', '?', '+', 'a', 'Z', '0', '7', '99', '1e3', 'NaN', 'Infinity',
];
const str = (maxTokens) => {
  const n = 1 + Math.floor(rnd() * maxTokens);
  let s = '';
  for (let i = 0; i < n; i++) s += pick(TOKENS);
  return s;
};
const ZONES = ['UTC', 'America/Chicago', 'Asia/Kathmandu', 'Australia/Lord_Howe', 'Nope'];
const NOW = new Date(Date.UTC(2026, 9, 16, 14, 30, 15, 123));

const out = {};
function rec(key, fn, argsList) {
  const list = (out[key] ??= []);
  for (const args of argsList) {
    let result;
    try {
      result = enc(fn(...args));
    } catch (e) {
      result = { $throw: String(e?.message) };
    }
    list.push([args.map((x) => enc(x)), result]);
  }
}
const many = (n, make) => Array.from({ length: n }, make);

rec('parseDuration.parseDuration', dur.parseDuration, many(400, () => [str(4)]));
rec('text.sanitizeName', text.sanitizeName, many(200, () => [str(6)]));
rec('text.clampText', text.clampText, many(200, () => [str(8), Math.floor(rnd() * 12)]));
rec('time.isValidTimeZone', time.isValidTimeZone, many(200, () => [str(2)]));
rec('time.parseTimeOfDay', time.parseTimeOfDay, many(300, () => [str(3)]));
rec('time.parseWeekdays', time.parseWeekdays, many(200, () => [str(4)]));
rec('time.zoneFromOffset', time.zoneFromOffset, many(200, () => [str(3)]));
rec('time.zoneFromAbbreviation', time.zoneFromAbbreviation, many(100, () => [str(2)]));
rec('time.parseBirthday', time.parseBirthday, many(300, () => [str(4), { now: NOW }]));
rec('time.parseDateInput', time.parseDateInput, many(300, () => [str(4), pick(ZONES), { now: NOW }]));
rec('time.parseWhen', time.parseWhen, many(600, () => [str(5), pick(ZONES), { now: NOW }]));
rec('timezones.normalizeTimeZone', tzs.normalizeTimeZone, many(300, () => [str(3)]));
rec('timezones.parseZoneInput', tzs.parseZoneInput, many(300, () => [str(3)]));
rec('timezones.zoneFromRoleName', tzs.zoneFromRoleName, many(200, () => [str(4)]));
rec('timeMentions.mayMentionTime', tm.mayMentionTime, many(200, () => [str(8)]));
rec('timeMentions.extractTimeSpans', tm.extractTimeSpans, many(300, () => [str(10)]));
rec('safeRegex.escapeRegex', sr.escapeRegex, many(100, () => [str(5)]));
rec('safeRegex.containsWord', sr.containsWord, many(300, () => [str(8), str(2)]));
rec('calendar.icsEscape', cal.icsEscape, many(100, () => [str(6)]));
rec('calendar.fitEncoded', cal.fitEncoded, many(200, () => [str(10), Math.floor(rnd() * 40)]));

mkdirSync(OUT, { recursive: true });
writeFileSync(join(OUT, 'fuzz.json'), `${JSON.stringify(out)}\n`);
for (const [k, v] of Object.entries(out)) process.stdout.write(`${k}: ${v.length}\n`);
