// Calls the real src/utils functions that bot-util ports over large tables of inputs (normal, edge and hostile) and
// writes rust/fixtures/golden/bot-util/tables.json ({ "module.fn": [[args, result], ...] }, encoded by
// bot-util-encode.mjs). The Rust tests replay every pair. Every clock is fixed, so the output is deterministic.
// Run from the repo root: node rust/fixtures/gen/bot-util-tables.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { ChannelType } from 'discord.js';
import { enc, plainCtx } from './bot-util-encode.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'bot-util');
const imp = (p) => import(pathToFileURL(join(ROOT, p)).href);

const text = await imp('src/utils/text.js');
const dur = await imp('src/utils/parseDuration.js');
const img = await imp('src/utils/imageSize.js');
const cal = await imp('src/utils/calendar.js');
const time = await imp('src/utils/time.js');
const tzs = await imp('src/utils/timezones.js');
const tm = await imp('src/utils/timeMentions.js');
const sr = await imp('src/utils/safeRegex.js');
const sf = await imp('src/utils/safeFetch.js');
const fg = await imp('src/utils/fatalGateway.js');
const rb = await imp('src/utils/restartBrake.js');
const ph = await imp('src/utils/placeholderParser.js');

const out = {};
function rec(key, fn, argsList, encodeArgs = (a) => a.map((x) => enc(x))) {
  const list = (out[key] ??= []);
  for (const args of argsList) {
    let result;
    try {
      result = enc(fn(...args));
    } catch (e) {
      result = { $throw: String(e?.message) };
    }
    list.push([encodeArgs(args), result]);
  }
}
const cross = (...lists) => lists.reduce((acc, l) => acc.flatMap((a) => l.map((x) => [...a, x])), [[]]);

const NOW = new Date(Date.UTC(2026, 9, 16, 14, 30, 15, 123)); // Fri 2026-10-16 14:30:15.123Z
const NOWS = [
  NOW,
  new Date(Date.UTC(2026, 2, 8, 6, 59)), // just before the US spring-forward
  new Date(Date.UTC(2026, 10, 1, 5, 30)), // inside the US fall-back hour
  new Date(Date.UTC(2026, 11, 31, 23, 45)), // year end
  new Date(Date.UTC(2024, 1, 29, 12)), // leap day
  new Date(Date.UTC(2026, 3, 5, 15, 30)), // Lord Howe / southern DST end
];

// ── text ───────────────────────────────────────────────────────────────────────
const STRINGS = ['', 'a', 'hello world', 'hello world   ', '  spaced  out  ', 'tab\there', 'line\nbreak', 'emoji 😀 here',
  '😀😀😀😀😀', 'a😀b😀c', 'café', 'é', 'ｆｕｌｌｗｉｄｔｈ', 'Ünïcödé Ñame', 'x'.repeat(50), 'word '.repeat(20), '﻿bom ', 'nbsp  ',
  'trailing　', ' sep', '日本語のテキスト', 'mixed 123 !@#', '<@123> mention', 'a\u0000b'];
const MAXES = [0, 1, 2, 3, 4, 5, 6, 10, 11, 12, 20, 100, -1, 2.7, NaN, Infinity, '5', null];
rec('text.clampText', text.clampText, [
  ...cross(STRINGS, MAXES).map(([s, m]) => [s, m]),
  ...cross(STRINGS.slice(0, 12), [3, 5, 8], ['...', '', '…!', null, undefined]).map(([s, m, e]) => [s, m, e]),
  [null, 5], [undefined, 5], [12345, 3], [true, 2], [{ a: 1 }, 6], [[1, 2, 3], 3],
]);
const LINES = [[], ['a'], ['a', 'b', 'c'], ['x'.repeat(30), 'y'.repeat(30), 'z'], ['😀'.repeat(20), 'tail'], [1, null, undefined, 'x'],
  Array.from({ length: 40 }, (_, i) => `line ${i}`), ['x'.repeat(2000)], ['', '', '']];
rec('text.chunkLines', text.chunkLines, [
  ...cross(LINES, [5, 10, 31, 64, 1024]).map(([l, m]) => [l, m]),
  ...LINES.map((l) => [l]), ['notarray', 10], [null, 10],
]);
const ITEMS = [[], ['a'], ['<@1>', '<@22>', '<@333>', '<@4444>'], Array.from({ length: 30 }, (_, i) => `<@${100000 + i}>`),
  ['x'.repeat(50)], ['short', 'x'.repeat(40), 'y'], [1, 2, 3, null], 'abc'];
rec('text.joinWithLimit', text.joinWithLimit, [
  ...cross(ITEMS, [1, 5, 10, 20, 30, 50, 100, 1024]).map(([i, m]) => [i, { max: m }]),
  ...cross(ITEMS, [' ', ' | ', '']).map(([i, s]) => [i, { sep: s, max: 25 }]),
  ...ITEMS.map((i) => [i]), [null], [undefined],
]);
const NUMS = [0, -0, 1, -1, 7, 12, 999, 1000, 1001, 1234, 12345, 123456, 1234567, 1e6, 999999.999, 1.005, 2.675, 1.115, 0.125, 0.135,
  1.5, 2.5, -2.5, 0.005, 0.015, 1e-7, 123456.785, 1e15, 1e16, 9007199254740993, 1e21, 1.5e21, 1e300, -1234.5678, 0.1 + 0.2,
  1/3, 2/3, 99.995, 99.994, 1234.565, 1000000.005, NaN, Infinity, -Infinity, '1234', '12.345', 'abc', '', ' 42 ', null, undefined,
  true, false, [], [5], {}, 10n, 1234567890123456789n, -9876543210n, 0n, 4.35, 8.345, 1.255, 10.005];
rec('text.formatNumber', text.formatNumber, NUMS.map((n) => [n]));
rec('text.ordinal', text.ordinal, [...NUMS, 11, 12, 13, 21, 22, 23, 101, 111, 112, 113, 1001, 1011, 2022, -1, -2, -3, -11, -21, 3.7, -3.7]
  .map((n) => [n]));
rec('text.compactNumber', text.compactNumber, [...NUMS, 999.9, 1499, 1500, 1549, 1550, 9999, 99_950, 999_499, 999_950, 999_999, 1_000_000,
  1_250_000, 999_950_000, 1e9, 1.5e9, 999_950_000_000, 1e12, 1.5e12, 999e12, 1e15, 1e18, -1500, -999_950, -1e12, 12.7, -12.7, 0.4]
  .map((n) => [n]));
rec('text.progressBar', text.progressBar, [
  ...cross([0, 0.04, 0.05, 0.15, 0.25, 0.5, 0.55, 0.95, 1, 1.5, -0.5, NaN, '0.3', null, undefined, Infinity], [1, 3, 10, 20]).map(([f, w]) => [f, w]),
  [0.5], [0.5, 0], [0.5, 1000], [0.5, NaN], [0.5, Infinity], [0.5, 2.9], [0.5, -4], [0.5, '7'],
  [0.3, 10, { filled: '#', empty: '-' }], [0.3, 5, { filled: '😀' }], [0.6, 4, { empty: '' }],
]);
rec('text.sanitizeName', text.sanitizeName, [
  ...['Hello World', '  Café Crème  ', 'ÀÉÎÕÜ', 'ﬁle', 'Ⅻ roman', '①②③', 'full　width', 'a--b---c', '--lead-trail--', 'émoji 😀 name',
    'UPPER_case-123', 'tab\tand\nnewline', '日本語', '', null, undefined, 12345, 'x'.repeat(40), 'ab-'.repeat(15), 'ǅ digraph',
    'Straße', 'İstanbul', 'ΣΊΣΥΦΟΣ', 'ß', 'ﬀ', '½', 'ｱｲｳ', '́abc', 'ȧ', 'ﾟ'].map((s) => [s]),
  ['Some Long Name Here', { max: 8 }], ['abc def', { max: 4 }], ['---', { fallback: 'none' }], ['', { fallback: '' }],
  ['abc', { max: 0 }], ['ab cd', { max: 3 }],
]);
rec('text.percentages', text.percentages, [[[]], [[0, 0]], [[1]], [[1, 1, 1]], [[1, 2]], [[5, 3, 2]], [[1, 1, 1, 1, 1, 1, 1]], [[33, 33, 33]],
  [[2, 2, 1]], [[-1, 3]], [[1.9, 2.1]], [['3', '1']], [[null, 4]], [[100, 0, 0]], [[7, 7, 6, 1]], [null], [undefined], [[1e6, 1, 1]]]);

// ── parseDuration ──────────────────────────────────────────────────────────────
const DURATIONS = ['1h', '1.5h', '2h30m', '1 month', '1 months', '1mo', '2mos', '5 minutes and 3s', '1y', '1 yr', '2 yrs', '3 years', '1w2d',
  '1 wk', '3 weeks', '4d', '4 days', '1 day', '2 hrs', '1 hr', '45 mins', '1 min', '30 secs', '1 sec', '10 seconds', '5ms', '5 mo', '.5h',
  '0.25d', '1.h', '1h 30m', '1h, 30m', '1h and 30m', '1h,30m', '1hand30m', '1h and and 30m', ' 1h ', '1H', '1HOUR', '1 Hour', '0s', '0',
  '1', 'h', '', '   ', 'abc', '1x', '1h x', '-1h', '+1h', '1e3s', '100y', '101y', '100 years 1 day', '36500d', '36501d', '1 month 1 minute',
  '5 m', '5 mi', '5 minutesx', '5 mins!', '1h1', '1h-30m', '1h;30m', '1 hour, 2 minutes, and 3 seconds', '1hour', '2days', '1.5.5h',
  '1,5h', '9'.repeat(30) + 's', 'x'.repeat(101), '1s'.repeat(51), '1s'.repeat(50), '0.0001s', '0.0005s', '0.0004s', '1 and', 'and 1s',
  ', 1s', '1s,', '1s and', '1 y 2 mo 3 w 4 d 5 h 6 m 7 s', '1M', '1MO', '1 Month', '3 mins and', '\t1h\n', '1h 30m', 'in 2h', null, 5, undefined];
rec('parseDuration.parseDuration', dur.parseDuration, DURATIONS.map((s) => [s]));
const MS = [0, -1, 1, 999, 1000, 1500, 59_999, 60_000, 61_000, 3_600_000, 9_000_000, 86_400_000, 90_061_000, 604_800_000, 694_861_001,
  2_592_000_000, 31_536_000_000, 3_153_600_000_000, 1.5, NaN, Infinity, null, undefined, '5000', 123456789012];
rec('parseDuration.formatDuration', dur.formatDuration, MS.map((m) => [m]));
rec('parseDuration.formatDurationLong', dur.formatDurationLong, MS.map((m) => [m]));

// ── imageSize ──────────────────────────────────────────────────────────────────
function png(w, h, { sig = true, chunk = 'IHDR', len = 24 } = {}) {
  const b = Buffer.alloc(len);
  if (sig) Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).copy(b);
  if (len >= 16) { b.writeUInt32BE(13, 8); b.write(chunk, 12, 'latin1'); }
  if (len >= 24) { b.writeUInt32BE(w >>> 0, 16); b.writeUInt32BE(h >>> 0, 20); }
  return b;
}
function gif(w, h, sig = 'GIF89a', len = 10) {
  const b = Buffer.alloc(len);
  b.write(sig, 0, 'latin1');
  if (len >= 10) { b.writeUInt16LE(w, 6); b.writeUInt16LE(h, 8); }
  return b;
}
function jpeg(segments) {
  return Buffer.from([0xff, 0xd8, ...segments.flat()]);
}
const sof = (marker, w, h) => [0xff, marker, 0x00, 0x11, 0x08, h >> 8, h & 255, w >> 8, w & 255, 0x03];
const app = (marker, n) => [0xff, marker, (n + 2) >> 8, (n + 2) & 255, ...Array(n).fill(0x41)];
function webp(kind, w, h) {
  const b = Buffer.alloc(30);
  b.write('RIFF', 0, 'latin1'); b.write('WEBP', 8, 'latin1'); b.write(kind, 12, 'latin1');
  if (kind === 'VP8 ') { b[23] = 0x9d; b[24] = 0x01; b[25] = 0x2a; b.writeUInt16LE(w, 26); b.writeUInt16LE(h, 28); }
  if (kind === 'VP8L') { b[20] = 0x2f; b.writeUInt32LE(((w - 1) & 0x3fff) | (((h - 1) & 0x3fff) << 14), 21); }
  if (kind === 'VP8X') { b.writeUIntLE(w - 1, 24, 3); b.writeUIntLE(h - 1, 27, 3); }
  return b;
}
const IMAGES = [
  png(64, 64), png(1, 1), png(8000, 2000), png(8001, 10), png(4000, 4000), png(4001, 4000), png(30000, 30000), png(0, 10), png(10, 0),
  png(0xffffffff, 1), png(64, 64, { sig: false }), png(64, 64, { chunk: 'IDAT' }), png(64, 64, { len: 23 }), png(64, 64, { len: 40 }),
  gif(320, 240), gif(320, 240, 'GIF87a'), gif(320, 240, 'GIF88a'), gif(0, 5), gif(65535, 65535), gif(5, 5, 'GIF89a', 9),
  jpeg([sof(0xc0, 640, 480)]), jpeg([app(0xe0, 14), app(0xe1, 100), sof(0xc2, 1920, 1080)]), jpeg([app(0xe0, 14), [0xff, 0xff, 0xff], sof(0xc1, 50, 60)]),
  jpeg([[0xff, 0xd0], sof(0xc0, 10, 20)]), jpeg([[0xff, 0x01], sof(0xc0, 10, 20)]), jpeg([app(0xc4, 10), sof(0xc0, 33, 44)]),
  jpeg([app(0xc8, 10), sof(0xcf, 33, 44)]), jpeg([app(0xcc, 4), sof(0xcb, 7, 8)]), jpeg([[0xff, 0xda, 0, 4, 1, 2]]), jpeg([[0xff, 0xe0, 0, 1]]),
  jpeg([[0xff, 0xe0, 0, 0]]), jpeg([[0x00, 0x01, 0x02, 0x03]]), jpeg([sof(0xc0, 0, 10)]), jpeg([[0xff, 0xc0, 0, 17, 8, 1]]), jpeg([app(0xe0, 2000)]),
  jpeg(Array.from({ length: 1100 }, () => app(0xe0, 0))), jpeg([...Array.from({ length: 999 }, () => app(0xe0, 0)), sof(0xc0, 9, 9)]),
  jpeg([...Array.from({ length: 1000 }, () => app(0xe0, 0)), sof(0xc0, 9, 9)]),
  Buffer.from([0xff, 0xd8, 0xff]), Buffer.from([0xff, 0xd8]),
  webp('VP8 ', 800, 600), webp('VP8 ', 0x7fff, 0x7fff), webp('VP8L', 1024, 768), webp('VP8L', 16384, 16384), webp('VP8X', 5000, 3000),
  webp('VP8X', 16777216, 1), webp('ABCD', 5, 5), Buffer.concat([webp('VP8 ', 5, 5).subarray(0, 23), Buffer.from([0, 0, 0, 0, 0, 0, 0])]),
  webp('VP8L', 10, 10).fill(0, 20, 21), webp('VP8 ', 1, 1).subarray(0, 29),
  Buffer.alloc(0), Buffer.from('not an image at all, just text'), Buffer.from('<svg xmlns="http://www.w3.org/2000/svg"/>'),
  new Uint8Array(png(100, 50)), new Uint8Array(gif(7, 9)).subarray(0, 10),
];
rec('imageSize.readImageSize', img.readImageSize, [...IMAGES.map((b) => [b]), [null], [undefined], ['string'], [123]]);
rec('imageSize.assertImageDimensions', img.assertImageDimensions, [
  ...IMAGES.map((b) => [b]),
  ...cross(IMAGES.slice(0, 8), [{ maxSide: 100 }, { maxPixels: 4096 }, { maxSide: 70000, maxPixels: 1e12 }, { maxSide: 0 }]).map(([b, o]) => [b, o]),
]);

// ── calendar ───────────────────────────────────────────────────────────────────
const START = new Date(Date.UTC(2026, 10, 5, 19, 0));
const END = new Date(Date.UTC(2026, 10, 5, 21, 30, 45, 999));
const ENTRIES = [
  { uid: 'evt-1@bot', title: 'Game night', start: START, end: END },
  { uid: 'evt-2@bot', title: 'Raid; prep, & notes\\ok', description: 'Bring snacks.\nLine two\r\nLine three', location: 'Voice, #1', start: START, end: END, url: 'https://discord.com/channels/1/2/3', sequence: 3 },
  { uid: 'x', title: '😀'.repeat(200), description: 'é'.repeat(400), location: 'ü'.repeat(100), start: START, end: END, url: `https://example.com/${'p'.repeat(300)}` },
  { uid: 'y', title: 'Lone \uD800 surrogate', description: 'tail \uDC00', start: START, end: END, sequence: -2 },
  { uid: 'z', title: 'Long description', description: 'word '.repeat(300), start: START, end: END, url: 'https://x.y/z', sequence: 2.7 },
  { uid: 'w', title: '', description: '   ', start: START, end: END, url: '', sequence: NaN },
  { uid: 'v', title: 'A'.repeat(600), start: START, end: END },
  { uid: 'u', title: 'Url with newline', url: 'https://a.b/c\r\nd', description: null, location: null, start: START, end: END },
  { uid: 'semi;colon,comma', title: 'T', start: START.getTime(), end: END.getTime(), sequence: '4' },
  { uid: 't', title: 'Only url', description: '', url: `https://example.com/${'q'.repeat(600)}`, start: START, end: END },
];
rec('calendar.fitEncoded', cal.fitEncoded, cross(['', 'hello', 'hello world  ', 'é'.repeat(30), '😀'.repeat(30), 'a b c d e f', ' leading',
  'x\uD800y', null, 12345, 'ab  cd'], [0, 1, 3, 9, 10, 12, 18, 30, 100]).map(([t, b]) => [t, b]));
rec('calendar.googleCalendarUrl', cal.googleCalendarUrl, [...ENTRIES.map((e) => [e]), ...cross(ENTRIES.slice(1, 4), [200, 300, 120]).map(([e, m]) => [e, m])]);
rec('calendar.outlookCalendarUrl', cal.outlookCalendarUrl, [...ENTRIES.map((e) => [e]), ...cross(ENTRIES.slice(1, 4), [250, 350, 160]).map(([e, m]) => [e, m])]);
rec('calendar.icsEscape', cal.icsEscape, ['', 'plain', 'a;b,c\\d', 'l1\nl2\r\nl3\rl4', '\\n literal', 'x\uD800', null, undefined, 42].map((s) => [s]));
rec('calendar.icsFold', cal.icsFold, ['', 'short', 'x'.repeat(75), 'x'.repeat(76), 'x'.repeat(200), 'é'.repeat(40), 'é'.repeat(80), '😀'.repeat(40),
  `SUMMARY:${'a'.repeat(73)}é`, `DESCRIPTION:${'中'.repeat(60)}`].map((s) => [s]));
rec('calendar.buildIcs', cal.buildIcs, [
  ...ENTRIES.map((e) => [e, { now: NOW }]),
  [ENTRIES[1], { now: NOW.getTime(), prodId: '-//x//y//EN' }],
]);

// ── time ───────────────────────────────────────────────────────────────────────
const ZONES = ['UTC', 'utc', 'Europe/London', 'America/New_York', 'America/Chicago', 'America/Los_Angeles', 'America/Sao_Paulo',
  'Australia/Lord_Howe', 'Europe/Dublin', 'Asia/Kathmandu', 'Pacific/Chatham', 'Pacific/Apia', 'Asia/Kolkata', 'Asia/Calcutta',
  'Australia/Adelaide', 'Pacific/Kiritimati', 'Etc/GMT+12', 'Etc/GMT-14', 'America/St_Johns', 'Africa/Casablanca', 'Asia/Tehran',
  'Europe/Kyiv', 'Europe/Kiev', 'Antarctica/Troll', 'America/Asuncion', '+05:30', '-0800', 'EST', 'Japan', 'US/Eastern',
  'Not/AZone', '', 'europe/london', 'Europe/London ', 'x'.repeat(65), null, 42];
rec('time.isValidTimeZone', time.isValidTimeZone, [...ZONES, 'America/Argentina/Buenos_Aires', 'PST', 'ACT', 'SystemV/AST4', 'GMT0', '+24:00',
  '+14:00', '−05:00', '+5', 'Etc/Unknown', 'UTC+2', 'Mars/Olympus', 'Europe', 'Z', 'zulu', 'Etc/GMT+15', 'EST5EDT', 'CET', 'Asia/Saigon'].map((z) => [z]));
const INSTANTS = [0, Date.UTC(2026, 2, 8, 7), Date.UTC(2026, 2, 8, 6, 59, 59, 999), Date.UTC(2026, 10, 1, 5, 30), Date.UTC(2026, 10, 1, 6, 30),
  Date.UTC(2026, 3, 5, 14, 59), Date.UTC(2026, 3, 5, 15, 30), Date.UTC(2011, 11, 30, 10), Date.UTC(2011, 11, 29, 9), Date.UTC(1999, 11, 31, 23, 59, 59),
  Date.UTC(2024, 1, 29, 12), Date.UTC(-100, 0, 1), Date.UTC(5, 0, 1), Date.UTC(275760, 8, 13), 8.64e15 + 1, -1, 1.5, NaN, NOW.getTime(),
  NOW, new Date(NaN), '2026-01-01', null, Date.UTC(2038, 0, 19, 3, 14, 8), Date.UTC(2100, 6, 4)];
rec('time.zonedParts', time.zonedParts, cross(INSTANTS, ZONES.slice(0, 30)).map(([d, z]) => [d, z]));
rec('time.localDateKey', time.localDateKey, cross(INSTANTS, ['UTC', 'Pacific/Kiritimati', 'Etc/GMT+12', 'America/New_York']).map(([d, z]) => [d, z]));
const LOCALS = [
  { year: 2026, month: 3, day: 8, hour: 2, minute: 30 }, { year: 2026, month: 3, day: 8, hour: 1, minute: 59 }, { year: 2026, month: 3, day: 8, hour: 3 },
  { year: 2026, month: 11, day: 1, hour: 1, minute: 30 }, { year: 2026, month: 11, day: 1, hour: 2 }, { year: 2026, month: 3, day: 29, hour: 1, minute: 30 },
  { year: 2026, month: 10, day: 25, hour: 1, minute: 30 }, { year: 2026, month: 4, day: 5, hour: 1, minute: 45 }, { year: 2026, month: 10, day: 4, hour: 2, minute: 15 },
  { year: 2011, month: 12, day: 30, hour: 12 }, { year: 2011, month: 12, day: 29, hour: 23, minute: 59 }, { year: 2026, month: 1, day: 1 },
  { year: 2026, month: 2, day: 30, hour: 10 }, { year: 2026, month: 13, day: 1 }, { year: 2026, month: 0, day: 0 }, { year: 50, month: 6, day: 1 },
  { year: 2026, month: 6, day: 15, hour: 25, minute: 70 }, { year: 2026, month: 6, day: 15, hour: -1 }, { year: NaN, month: 1, day: 1 },
  { year: 2026, month: 1, day: 1, hour: 1.5 }, { year: 300000, month: 1, day: 1 }, { year: 2026, month: '3', day: 8, hour: '2' },
];
rec('time.zonedToUtc', time.zonedToUtc, cross(LOCALS, ZONES.slice(0, 30)).map(([l, z]) => [l, z]));
rec('time.daysInMonth', time.daysInMonth, cross([2023, 2024, 1900, 2000, 2100, 0, 50, 99, 100, -1], [1, 2, 3, 4, 12, 13, 0, -1]).map(([y, m]) => [y, m]));
const TIMES = ['9', '09:30', '9:30pm', '21:30', '12am', '12pm', 'noon', 'midnight', 'NOON', ' 7 PM ', '7 pm', '7p.m.', '7 a.m.', '0', '00:00', '23:59',
  '24:00', '13pm', '0am', '12:60', '9:5', '9:05am', '1:00 PM', '7pm cst', '', null, 'abc', '7.30pm', '１２:00', '007:00', '9 :30'];
rec('time.parseTimeOfDay', time.parseTimeOfDay, TIMES.map((t) => [t]));
rec('time.formatTimeOfDay', time.formatTimeOfDay, [[{ hour: 0, minute: 0 }], [{ hour: 9, minute: 5 }], [{ hour: 23, minute: 59 }], [{ hour: 123, minute: 7 }]]);
rec('time.parseWeekdays', time.parseWeekdays, ['mon,wed,fri', 'fri-mon', 'weekdays', 'weekends', 'all', 'daily', 'everyday', 'every day', 'Mon Tue',
  'sun-sat', 'sat-sun', '0-6', '6-0', '1,3,5', 'tues thurs', 'weds', 'mon-mon', 'mon,,wed', 'mon-', '-mon', 'monday-friday', 'xyz', 'mon,xyz',
  '7', '', null, '  ', 'MON,WED', 'thu-tue', 'mon-fri,sun', 'mon - fri'].map((s) => [s]));
const after = NOW;
rec('time.nextTimeOfDay', time.nextTimeOfDay, [
  ...cross(['UTC', 'America/New_York', 'Europe/London', 'Australia/Lord_Howe', 'Pacific/Apia', 'Asia/Kathmandu', 'Nope/Zone'],
    [[0, 0], [2, 30], [1, 30], [14, 30], [14, 31], [23, 59]], [[], [1, 3, 5], [0], [6], [7, -1]], NOWS.slice(0, 4))
    .map(([tz, [hour, minute], weekdays, a]) => [{ tz, hour, minute, weekdays, after: a }]),
  [{ tz: 'UTC', hour: 9, minute: 0, after: new Date(NaN) }], [{ tz: 'UTC', hour: 9, minute: 0, after: NOW.getTime() }],
]);
const WINDOWS = [[null, null], ['09:00', '17:00'], ['22:00', '02:00'], [null, '06:00'], ['18:00', null], ['10:00', '10:00'], ['bad', null], [null, 'bad'], ['9pm', '1am']];
rec('time.nextIntervalRun', time.nextIntervalRun, [
  ...cross(['UTC', 'America/New_York', 'Australia/Lord_Howe', 'Asia/Kathmandu'], [1, 7, 60, 90, 1440, 10080], [[], [1, 2, 3, 4, 5], [0]],
    WINDOWS, NOWS.slice(0, 3)).map(([tz, intervalMinutes, weekdays, [windowStart, windowEnd], a]) => [{ tz, intervalMinutes, weekdays, windowStart, windowEnd, after: a }]),
  ...cross([1, 60, 1440], [new Date(Date.UTC(2026, 0, 1, 0, 7)), new Date(Date.UTC(2030, 0, 1)), null]).map(([i, anchor]) => [{ tz: 'Europe/London', intervalMinutes: i, after: NOW, anchor }]),
  [{ tz: 'UTC', intervalMinutes: 0, after: NOW }], [{ tz: 'UTC', intervalMinutes: 10081, after: NOW }], [{ tz: 'UTC', intervalMinutes: 1.5, after: NOW }],
  [{ tz: 'UTC', intervalMinutes: 60, after: new Date(NaN) }], [{ tz: 'UTC', intervalMinutes: 60, weekdays: [9], after: NOW }],
]);
const DATE_INPUTS = ['<t:1700000000>', '<t:1700000000:R>', '<t:99999999999999>', '2h30m', 'in 2 days', 'IN 3h', '2026-12-25', '2026-12-25 18:30',
  '2026-12-25T7pm', '2026-02-30', '2026-13-01', '2024-02-29 noon', '1969-07-20 20:17', '12-25', '12-25 9am', '02-29', '2-29 23:59', '13-01',
  '10-16', '10-16 14:30', '10-16 14:31', '18:30', '7pm', '14:30', '14:31', 'noon', 'midnight', 'tomorrow', '', 'garbage', '2026-3-8 2:30',
  '2026-11-01 1:30', '0050-01-01', '99999-01-01', '2026-10-16 25:00', '06-31', null];
rec('time.parseDateInput', time.parseDateInput, cross(DATE_INPUTS, ['UTC', 'America/New_York', 'Australia/Lord_Howe', 'Bad/Zone'], NOWS.slice(0, 3))
  .map(([s, z, n]) => [s, z, { now: n }]));
rec('time.parseBirthday', time.parseBirthday, [
  ...['03-14', '3-14', '1990-03-14', 'march 14', '14 march', 'mar 14 1990', '14 mar 1990', 'Mar. 14th, 1990', '14th of march', 'sept 1', 'SEPT 1st',
    '02-29', '2023-02-29', '2024-02-29', '1899-01-01', '2027-01-01', '2026-10-16', '03/14', '14/03/1990', 'smarch 14', '00-10', '13-01', '04-31',
    'jan 0', 'feb 30', '', null, 'may 5', 'june 31', 'july 4th 1776', 'dec 25, 2000', '25 dec, 2000'].map((s) => [s, { now: NOW }]),
  ['03-14', { requireYear: true, now: NOW }], ['1990-03-14', { requireYear: true, now: NOW }], ['2026-01-01', { now: new Date(Date.UTC(2025, 5)) }],
]);
rec('time.discordTimestamp', time.discordTimestamp, [[NOW], [NOW, 'R'], [NOW, 't'], [NOW, 'x'], [NOW, 'RR'], [NOW, null], [1700000000999, 'D'],
  [-1500, 'f'], [new Date(NaN)], [0, 'F'], ['1700000000000', 'T']]);
rec('time.zoneFromAbbreviation', time.zoneFromAbbreviation, [...Object.keys(time.ZONE_ABBREVIATIONS), 'UTC', 'gmt', 'Z', 'zulu', 'IST', 'AST',
  ' CST ', 'xyz', '', null, 'toString', '__proto__', 'constructor', 'hasOwnProperty'].map((s) => [s]));
rec('time.zoneFromOffset', time.zoneFromOffset, ['utc+2', 'GMT-5', 'utc+05:00', 'UTC+0500', 'utc + 2', 'utc−3', 'utc+0', 'utc-0', 'utc+14', 'utc+15',
  'utc-12', 'utc-13', 'utc+5:30', 'utc+05:30', '+2', 'gmt+2:00', ' utc+2 ', 'UTC+02:0', 'utc+002', '', null].map((s) => [s]));
const WHEN = ['now', 'today', 'today 7pm', 'tonight', 'tomorrow', 'tomorrow at 18:30', 'tmrw 9am', 'tmr noon', 'yesterday 3pm', 'fri', 'fri 5pm',
  'friday at 5pm', 'next fri', 'next friday 5pm', 'this fri 5pm', '5pm fri', '7pm next fri', 'fri 2:30pm', 'fri 14:30', 'fri 14:31', 'sat 0:00',
  'oct 20', 'oct 20 9:30am', 'october 20th 2027', 'oct 20, 2027, 7pm', '20 oct', '20 october 7pm', '20th oct 2027', 'oct 16', 'oct 16 14:00',
  'feb 29', 'feb 29 2027', 'feb 30', 'jan 1 1969', 'dec 31 9999 23:59', '7pm', '7pm cst', '7pm CST', '7pm est', '7pm pdt', '7pm utc', '7pm utc+2',
  '7pm utc +2', '7pm gmt-5', '7pm utc+15', '7pm utc+5:30', '7pm Europe/London', '7pm europe/london', '7pm Asia/Calcutta', '7pm America/Nowhere',
  'fri 5pm ist', 'fri 5pm EAT', 'fri 5pm eat', 'at 7pm', 'at fri 7pm', 'fri at 7pm est', '2h30m', 'in 2 days', '2026-12-25 18:30',
  '2026-12-25 18:30 jst', '12-25', '<t:1700000000:R>', '<t:1700000000:R> cst', '', '   ', 'garbage', 'x'.repeat(101), 'fri 25:00', 'sun 3am',
  'next sun 2:30am', 'today 2:30am', 'tomorrow 1:30am', 'mon', 'tues 9pm', 'thurs 9pm', 'weds 9pm', 'next', 'this', 'oct', '16 oct 2:30am',
  'Oct. 20 7pm', 'oct 20th, 2027 at 7pm', 'utc', '7pm z', '7pm zulu', '7pm GMT', 'chamorro 7pm', '7pm chamorro', '19:00 utc−3', null];
rec('time.parseWhen', time.parseWhen, cross(WHEN, ['UTC', 'America/Chicago', 'Australia/Lord_Howe', 'Asia/Kathmandu', 'Nope'], NOWS.slice(0, 3))
  .map(([s, z, n]) => [s, z, { now: n }]));

// ── timezones ──────────────────────────────────────────────────────────────────
rec('timezones.listTimeZones', tzs.listTimeZones, [[]]);
const TZ_INPUTS = ['UTC', 'utc', 'GMT', 'Etc/UTC', 'etc/gmt-0', 'Zulu', 'Universal', 'Greenwich', 'UCT', 'GMT0', 'America/Chicago', 'america/chicago',
  'AMERICA/CHICAGO', 'Asia/Calcutta', 'asia/calcutta', 'Asia/Kolkata', 'Europe/Kiev', 'europe/kyiv', 'US/Eastern', 'us/pacific', 'Japan', 'EST', 'est',
  'CET', 'PST', 'CST', 'IST', 'AST', 'Etc/GMT+5', 'etc/gmt-14', '+05:30', '-06:00', '5', 'utc+2', 'UTC + 2', 'GMT-5', 'gmt+0', '−3', 'America/Nowhere',
  'Nowhere', '', '   ', null, 'x'.repeat(65), ' America/Chicago ', 'America/Argentina/Buenos_Aires', 'America/Buenos_Aires', 'Asia/Saigon',
  'Pacific/Enderbury', 'America/Godthab', 'Atlantic/Faeroe', 'Etc/Unknown', 'SystemV/AST4', 'Antarctica/South_Pole', 'Europe/Belfast',
  'America/Indianapolis', 'America/Indiana/Indianapolis', 'Etc/GMT', 'etc/zulu', 'NZ', 'Asia/Rangoon', 'cst', 'CST/CDT', 'chamorro', 'ChST',
  'eat', 'Brazil/East', 'Canada/Eastern', 'Mexico/General', 'Asia/Ho_Chi_Minh', 'Europe/Uzhgorod', 'America/Montreal', 'Asia/Chongqing'];
rec('timezones.normalizeTimeZone', tzs.normalizeTimeZone, TZ_INPUTS.map((s) => [s]));
rec('timezones.parseZoneInput', tzs.parseZoneInput, TZ_INPUTS.map((s) => [s]));
const LABEL_ZONES = ['UTC', 'America/New_York', 'Asia/Kolkata', 'Asia/Kathmandu', 'Pacific/Chatham', 'Australia/Lord_Howe', 'Europe/Dublin',
  'Etc/GMT+12', 'Etc/GMT-14', 'Pacific/Kiritimati', 'America/St_Johns', '+05:30', '-08:00', 'Bad/Zone', '', 'Asia/Calcutta', 'Europe/London',
  'Africa/Casablanca', 'America/Sao_Paulo', 'Pacific/Apia'];
rec('timezones.utcOffsetLabel', tzs.utcOffsetLabel, cross(LABEL_ZONES, NOWS).map(([z, d]) => [z, d]));
rec('timezones.zoneLabel', tzs.zoneLabel, cross([...LABEL_ZONES, 'Etc/GMT-5', 'Etc/GMT+10', 'Etc/GMT+15'], NOWS.slice(0, 3)).map(([z, d]) => [z, d]));
rec('timezones.describeLocalTime', tzs.describeLocalTime, cross(LABEL_ZONES, NOWS).map(([z, d]) => [z, d]));
rec('timezones.formatInZone', tzs.formatInZone, [
  ...cross(LABEL_ZONES, NOWS).map(([z, d]) => [d, z]),
  [NOW.getTime(), 'UTC'], [0, 'UTC'], [Date.UTC(2026, 0, 1, 0, 0), 'UTC'], [Date.UTC(2026, 0, 1, 12, 5), 'UTC'], [Date.UTC(1, 0, 1), 'UTC'],
  [Date.UTC(-50, 0, 1), 'UTC'], [Date.UTC(12345, 5, 6, 7, 8), 'UTC'],
]);
const QUERIES = ['', 'chicago', 'Chicago', 'new york', 'new_york', 'america', 'America/', 'ind', 'kolkata', 'calcutta', 'kyiv', 'kiev', 'utc',
  'UTC', 'gmt', 'london', 'lon', 'europe/lo', 'a', 'z', 'zzz', 'sao', 'são', '+5:30', '+05:30', '+5', '-3', '−3', 'utc-3', 'utc+1', 'gmt+5:45',
  '+0', '-0', '+00', '+14', '+15', '-12', 'utc+10:30', '+8', 'cst', 'CST', 'est', 'pst', 'ist', 'jst', 'chamorro', 'gmt', 'x'.repeat(80), null,
  'st', 'port', 'island', 'argentina/b', 'Buenos Aires'];
rec('timezones.searchTimeZones', tzs.searchTimeZones, [
  ...QUERIES.map((q) => [q, { now: NOW }]),
  ...QUERIES.slice(0, 25).map((q) => [q, { now: NOWS[1], abbreviations: true, current: 'America/Chicago' }]),
  ...['cst', 'est', 'utc', 'gmt', 'jst', 'pdt', 'aest', 'ist', 'chamorro', 'z'].map((q) => [q, { now: NOW, abbreviations: true }]),
  ['', { now: NOW, current: 'Asia/Tokyo' }], ['', { now: NOW, current: 'Not/Listed' }], ['', { now: NOW, current: 'UTC' }],
  ['a', { now: NOW, limit: 5 }], ['a', { now: NOW, limit: 0 }], ['+1', { now: NOW, limit: 100 }],
]);
rec('timezones.zoneFromRoleName', tzs.zoneFromRoleName, ['CST', 'CST/CDT', '🇺🇸 Eastern (EST)', 'UTC-6', 'GMT+1', 'PST (UTC-8)', 'EST and PST',
  'utc', 'UTC / EST', 'utc+5:30', 'UTC+2 UTC+3', 'UTC+2 and GMT+2', 'Timezone', '', null, 'IST', 'gmt-0', 'EAT', 'eat lunch', 'art club',
  'JST/KST', 'CET-CEST', 'utc +2', 'utc−3', 'Zulu time', 'z'].map((s) => [s]));
rec('timezones.normalizeTimezoneRoles', tzs.normalizeTimezoneRoles, [[null], ['x'], [[]], [[
  { roleId: '123456789012345678', zone: 'America/Chicago' }, { roleId: '123456789012345678', zone: 'UTC' }, { roleId: 'abc', zone: 'UTC' },
  { roleId: '1234567890123456', zone: 'UTC' }, { roleId: '123456789012345679', zone: 'Bad/Zone' }, null, 5, { roleId: 123456789012345680, zone: 'UTC' },
  { roleId: '12345678901234567890', zone: 'europe/london', extra: 1 }, { roleId: '123456789012345681' },
]]]);
rec('timezones.zoneSourceText', tzs.zoneSourceText, [[{ source: 'personal', roleId: null }], [{ source: 'role', roleId: '42' }], [{ source: 'server', roleId: null }],
  [{ source: 'default', roleId: null }], [{ source: 'role', roleId: '42' }, { zoneFromInput: true }], [{ source: 'weird', roleId: null }, {}]]);
rec('timezones.zoneInputError', tzs.zoneInputError, [['', { code: 'EMPTY' }], ['+5', { code: 'OFFSET' }], ['ist', { code: 'AMBIGUOUS', options: ['A/B', 'C/D'] }],
  ['EST', { code: 'ABBREVIATION' }], ['Mars/Base', { code: 'UNKNOWN' }], ['`evil`\\x', { code: 'UNKNOWN' }], [null, { code: 'ABBREVIATION' }],
  ['x'.repeat(100), { code: 'UNKNOWN' }], ['  ', { code: 'ABBREVIATION' }], ['a', { code: 'OTHER' }]]);

// ── timeMentions ───────────────────────────────────────────────────────────────
const CHAT = ['see you fri 5pm', 'tomorrow at 18:30', '7pm EST', 'oct 20 9:30am', 'meet at 5', 'in 2h', 'friday', 'score was 10:30', 'at 10:30',
  'video at 1:23:45', 'lunch at noon', 'noon', 'midnight on sat', 'we eat at 5pm', '5pm EAT', '5pm eat', '5pm on friday EST', '5pm on friday est please',
  'call at 9am PST and 10am CET', '`7pm` in code', '```\n7pm\n```', '||7pm||', '> quoted 7pm', '>>> block\n7pm', 'https://x.com/7pm', '<t:1700000000:R> 7pm',
  '<@123> 7pm', 'at 7 p.m.', '7 P.M.', '7pm utc+2', '7pm utc+5:30', '7pm gmt-5', '7pm utc + 2', '7pm GMT', '19:00 cet', '09:00 tomorrow', '13:00',
  'tmrw 9am', 'tonight 8pm', 'next fri 6pm', 'this sat 11am', '20th of october 7pm', 'oct. 20th, 2027 at 7pm', '2026-12-25 7pm', '1st jan 9am',
  '$5pm', '£7pm', "it's 5pm", 'a5pm', '5pmish', '5pm.', '5pm!', '(5pm)', '5pm-6pm', '5pm/6pm', '12am', '12pm', '0am', '13pm', '9:60pm', '23:59 utc',
  '24:00 utc', '07:30 jst', 'sun 3am', 'at 7pm ART', 'at 7pm art', 'at 7pm WEST', 'at 7pm west', 'mon 9am, tue 10am, wed 11am, thu 12pm, fri 1pm, sat 2pm',
  '1pm 2pm 3pm 4pm 5pm 6pm 7pm 8pm 9pm 10pm 11pm 12pm', '', null, 123, 'nothing here', '5 pm', '5 p.m. tomorrow', 'at 5:30pm on monday', 'midnight utc',
  'noon tomorrow', '@ 7pm', '@7pm', 'at  7pm', '7pm\tfri', 'FRI 5PM', 'Fri 5pm CEST', '7pm cest', '7pm acdt', 'oct 32 7pm', '7pm utc+15'];
rec('timeMentions.mayMentionTime', tm.mayMentionTime, CHAT.map((s) => [s]));
rec('timeMentions.extractTimeSpans', tm.extractTimeSpans, [...CHAT.map((s) => [s]), [CHAT.slice(0, 20).join(' '), { max: 3 }], [CHAT.slice(0, 20).join(' '), { max: 0 }]]);
const SPANS = CHAT.map((s) => tm.extractTimeSpans(s)).filter((x) => x.length);
rec('timeMentions.readTimeSpans', tm.readTimeSpans, [
  ...cross(SPANS, ['America/Chicago', 'UTC', null], NOWS.slice(0, 3)).map(([s, z, n]) => [s, z, { now: n }]),
  [SPANS.flat(), 'Europe/London', { now: NOW, max: 10 }], [SPANS.flat(), 'Europe/London', { now: NOW, pastGraceMs: 0 }],
  [[{ text: '1am', day: null, time: '1am', zone: null }], 'UTC', { now: NOW, pastGraceMs: 86_400_000 }],
  [[{ text: 'x', day: 'today', time: '2pm', zone: null }, { text: 'y', day: 'today', time: '14:00', zone: 'UTC' }], 'UTC', { now: NOW }],
]);

// ── safeRegex ──────────────────────────────────────────────────────────────────
const PATTERNS = ['(a+)+', '(a|aa)*', '(\\w+\\s?)*', '(.*a){5}', '((a+))+', '(a{2})+', '\\1', '(a)\\k<x>', '^hello\\s+world$', '[a-z]+@[a-z]+\\.com',
  '(foo|bar)baz', '(ab)+', '(a|b)?', '[(]+', '\\\\1', 'a+?b', '(?:ab)+c', '(?<n>ab)+', '(', ')', '[unclosed', 'a{', 'a{1', 'a{1,', 'a{2,1}', '{', '}', ']',
  'a]', '\\', 'abc\\', '*', '+a', '?', 'a**', 'a++', 'a??', 'a{1}{2}', '(?<n>a)(?<n>b)', '(?<1a>x)', '(?<a', '(?=a)', '(?!a)', '(?<=a)b', '(?<!a)b',
  '(?=a)+', '(?<=a)+', '(?:)', '()', '(?)', '(?i)a', '(?i:a)', '(?-i:a)', '\\p{L}+', '\\P{Lu}', '\\p{Script=Greek}', '\\p{Foo}', '\\u{1F600}', '\\u0041',
  '\\x41', '\\c', '\\cA', '\\c1', '\\0', '\\00', '\\07', '\\8', '\\9', '\\k', '\\k<', '\\q', '\\-', '[\\-]', '[a-]', '[-a]', '[z-a]', '[\\d-z]', '[\\w-]',
  '[]', '[^]', '[^a]', '\\b\\B', '\\d\\D\\w\\W\\s\\S', '.', '^$', 'a|', '|a', '||', 'a{0}', 'a{1,1}', 'a{,5}', 'a{99999999999}', 'a{2,}', '(a{1,3})+',
  '(a?)+', '(a*)*', '(?:a|b)+', '(?:a|b)*?', '(a{0,1})*', '(a{1})*', '([ab])+', '(a)+', '(a b)+', '(\\d+)?', '(\\d+)', '(?:x+)+', '(?:(x))+',
  '((x)|y)+', '(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)\\10', 'x{3}{4}', 'émoji😀', '😀+', '[😀-😂]', '\\u{110000}', '\\uD83D\\uDE00', 'a/b', 'a\\/b',
  'new\nline', '(?<a>.)\\k<a>', '[\\b]', '\\B+', '^*', '$+', '\\b*', 'a{3', '[a-z', 'x)', '(?<=a{2,})b', '(?<=(a+)+)b', '[\\p{L}]', '\\p{L}',
  'a'.repeat(200), 'a'.repeat(201), '😀'.repeat(100), '😀'.repeat(101), '', 5, null, 'ab(?#c)', '\\/', '[/]', '(?:a+){2}', '(?:a+){1}', '(a+){1,}',
  '(a+){0,1}', '(a|b){1,2}', '(a|b){2}', '(?:a{2}){3}', '\\w+\\s?\\w+', '(x+x+)+y', '(.*)*', '(.+)+', '(.?){10}', '[a-z]{1,10}', '(?:[a-z]{1,10})+'];
const FLAGS = ['i', '', 'g', 'y', 'v', 'imsu', 'iimm', 'u', 'iu', 'ms', 'x', 'I', 'd', 'sig', null, 5];
rec('safeRegex.validateRegex', sr.validateRegex, [
  ...PATTERNS.map((p) => [p]),
  ...cross(PATTERNS.slice(0, 120), ['u', 'imsu']).map(([p, f]) => [p, f]),
  ...cross(['abc', '(a+)+', '['], FLAGS).map(([p, f]) => [p, f]),
]);
rec('safeRegex.compileSafeRegex', sr.compileSafeRegex, [...PATTERNS.map((p) => [p]), ...cross(['^hello$', 'a/b', 'x'], ['i', 'u', 'ms', 'g', 'si', 'msi'])
  .map(([p, f]) => [p, f])]);
rec('safeRegex.escapeRegex', sr.escapeRegex, ['foo-bar/baz', 'a.b*c', '.*+?^${}()|[]\\', '', null, undefined, 42, 'x\ny', '😀.', '-/'].map((s) => [s]));
const WORD_CONTENT = ['Scunthorpe', 'you are a jerk!', 'a foo-bar!', 'JERK', 'café au lait', 'a (b) c', 'jerk_face', 'jerk2', '2jerk', 'jerk', 'JeRk.',
  'über jerk', 'Jerkü', 'ΣΊΣΥΦΟΣ', 'σίσυφος', 'straße', 'STRASSE', 'İstanbul', 'istanbul', 'ǅemal', 'a😀jerk😀b', 'x́jerk', '٣jerk', 'jerk٣',
  'ǈ', '', null, 'kK', 'ſecret', 'secret'];
const WORDS = ['jerk', 'cunt', 'foo-bar', 'caf', '(b)', 'σίσυφος', 'straße', 'istanbul', 'ǆemal', 'k', 'secret', 'Jerk', '', null, '😀', '٣'];
rec('safeRegex.containsWord', sr.containsWord, [
  ...cross(WORD_CONTENT, WORDS).map(([c, w]) => [c, w]),
  ...cross(WORD_CONTENT, WORDS.slice(0, 8)).map(([c, w]) => [c, w, { caseSensitive: true }]),
]);
const TEST_RES = [/^hello/i, /hello/, /a.c/s, /a.c/, /^b$/m, /^b$/, /\bword\b/i, /\w+/u, /😀/u, /^.$/u, /^.$/, /^..$/, /ſ/i, /s/i, /ſ/iu, /K/iu,
  /K/i, /[a-z]+\d/i, /(?<=\$)\d+/, /(?<!x)y/, /(?=abc)a/, /\p{Lu}/u, /x*/, /é/i, /É/i, /ß/i, /ẞ/iu, /\s/, /\S+/, /[^]/, /$/, /\d/u, /[Ā-ſ]/i];
const TEST_INPUTS = ['Hello there', 'nope', 'a\nc', 'abc', 'a\nb\nc', 'b', 'a word here', 'swordfish', 'Ünïcödé', '😀', '😀😀', 'ſ', 'S', 's', 'k', 'K',
  'K', 'xyz9', '$100', 'xy', 'zy', 'abcdef', 'ÀB', '', 'É', 'é', 'SS', 'ß', 'ẞ', ' ', '﻿', ' ', 'x'.repeat(3000) + 'y', 'Ā', 'ā', '1'];
rec('safeRegex.safeRegexTest', sr.safeRegexTest, [
  ...cross(TEST_RES, TEST_INPUTS).map(([r, s]) => [r, s]),
  [/y/, 'x'.repeat(3000) + 'y', { inputCap: 3001 }], [/y/, 'x'.repeat(10) + 'y', { inputCap: 5 }], [/😀$/u, 'a😀', { inputCap: 2 }], [/\uD83D$/, 'a😀', { inputCap: 2 }],
  [/^a/g, 'aaa'], [/a/y, 'ba'], ['not a regex', 'x'], [/x/, null], [/null/, null], [/undefined/, undefined], [/123/, 123], [/\[object Object\]/, {}],
]);
rec('safeRegex.safeRegexFirstMatch', sr.safeRegexFirstMatch, [
  [[/x/i, /hello/i, /hel/i], 'say hello'], [[/x/i], 'nothing here'], [[], 'x'], ['nope', 'x'], [[null, 'str', /b/], 'abc'], [[/c/, /b/, /a/], 'abc'],
  [[/z/, /y/], 'abc'], [[/^.$/u], '😀'], [[/^.$/], '😀'], [[/y/], 'x'.repeat(2500) + 'y'], [[/y/], 'x'.repeat(2500) + 'y', { inputCap: 3000 }],
]);

// ── safeFetch / fatalGateway / restartBrake ────────────────────────────────────
rec('safeFetch.isBlockedAddress', sf.isBlockedAddress, ['127.0.0.1', '10.1.2.3', '100.64.0.1', '100.127.255.255', '100.128.0.0', '169.254.169.254',
  '0.0.0.0', '0.1.2.3', '172.16.0.1', '172.31.255.255', '172.32.0.0', '192.168.1.1', '192.0.0.8', '192.0.2.1', '192.88.99.1', '198.18.0.1',
  '198.19.255.255', '198.20.0.0', '198.51.100.7', '203.0.113.9', '224.0.0.1', '239.255.255.255', '240.0.0.1', '255.255.255.255', '1.1.1.1', '8.8.8.8',
  '93.184.216.34', '11.0.0.1', '126.255.255.255', '128.0.0.1', '01.1.1.1', '1.1.1', '1.1.1.1.1', '256.1.1.1', ' 1.1.1.1 ', '::1', '::', '::0',
  '0:0:0:0:0:0:0:1', 'fd00::1', 'fc00::', 'fe80::1', 'febf::1', 'fec0::1', 'ff02::1', '2001:db8::1', '2001::1', '2001:0:1::', '2001:1::1', '100::1',
  '100::1:0:0:0:1', '::ffff:127.0.0.1', '::ffff:7f00:1', '::ffff:1.1.1.1', '::ffff:0:127.0.0.1', '::ffff:0:1.1.1.1', '64:ff9b::7f00:1',
  '64:ff9b::101:101', '64:ff9b::10.0.0.1', '64:ff9b:1::a00:1', '64:ff9b:1::101:101', '2002:7f00:1::', '2002:101:101::', '2002:a00:1::1',
  '::7f00:1', '::1.1.1.1', '::101:101', '2606:4700:4700::1111', '2606:4700::1', 'not-an-ip', '', '[::1]', '[2606:4700::1]', 'fe80::1%eth0',
  '2606:4700::1%1', 'FE80::1', '::FFFF:127.0.0.1', '1::2::3', '1:2:3:4:5:6:7:8:9', ':::', '2606:4700:4700::1111:', 'gggg::1', '1:2:3:4:5:6:1.1.1.1',
  '::1.1.1.256', '1:2:3:4:5:6:7:1.1.1.1', '12345::1', null, undefined, 2130706433, 'localhost'].map((s) => [s]));
const errs = [null, undefined, '', 'x', { code: 'TokenInvalid' }, new Error('Authentication failed'), new Error('Used invalid intents'),
  new Error('Used disallowed intents'), new Error('Something else'), { message: 'Authentication failed' }, { code: 'TokenInvalid', message: 'Used invalid intents' },
  Object.assign(new Error('Used disallowed intents'), { code: 'X' }), 0, 5];
rec('fatalGateway.fatalCodeFromError', fg.fatalCodeFromError, errs.map((e) => [e]));
rec('restartBrake.brakeDelayMs', rb.brakeDelayMs, [-1, 0, 1, 4, 5, 6, 7, 8, 9, 10, 11, 12, 20, 100, 1.5, 5.5].map((n) => [n]));

// ── placeholderParser ──────────────────────────────────────────────────────────
const IMAGE = (base) => (opts) => `${base}.${opts?.extension}?size=${opts?.size}`;
const userA = { id: '111111111111111111', username: 'alice', tag: 'alice', discriminator: '0', globalName: 'Alice <@1>', bot: false,
  createdTimestamp: 1_600_000_000_123, displayAvatarURL: IMAGE('https://cdn/u/a') };
const userB = { id: '222', username: 'bob', discriminator: '1234', bot: true, createdTimestamp: NaN, displayAvatarURL: IMAGE('https://cdn/u/b') };
const userC = { id: '333', username: undefined, globalName: null };
const guildA = { id: '999', name: 'My <#Server>', ownerId: '111111111111111111', memberCount: 1234567, verificationLevel: 2,
  joinedTimestamp: 1_650_000_000_000, createdTimestamp: 1_500_000_000_000, premiumSubscriptionCount: 14, premiumTier: 3, iconURL: IMAGE('https://cdn/g') };
const guildB = { id: '998', name: undefined, memberCount: undefined, verificationLevel: 0, premiumTier: 0 };
const memberA = { user: userA, guild: guildA, displayName: 'Ali <@&5> ce', joinedTimestamp: 1_700_000_000_500, displayAvatarURL: IMAGE('https://cdn/m/a') };
const memberB = { user: userB, guild: guildB, displayName: undefined, joinedTimestamp: null };
const chans = [ChannelType.GuildText, ChannelType.GuildVoice, ChannelType.GuildCategory, ChannelType.GuildAnnouncement, ChannelType.GuildStageVoice,
  ChannelType.GuildForum, ChannelType.GuildMedia, ChannelType.PublicThread, ChannelType.PrivateThread, ChannelType.AnnouncementThread, ChannelType.DM, 99]
  .map((type, i) => ({ id: `50${i}`, name: i === 0 ? 'general <#1>' : `chan-${i}`, type }));
const CTXS = [
  {}, { member: memberA }, { member: memberB }, { user: userA }, { user: userB, guild: guildA }, { user: userC, guild: guildB, channel: chans[0] },
  { member: memberA, channel: chans[3] }, { guild: guildA }, { channel: chans[8] },
  { member: memberA, args: ['one', 'two', 'three', '', 'five'] }, { args: [] }, { args: ['solo'] },
  { member: memberA, vars: { 'user.name': 'override', '{Level}': 7, ' XP ': 12.5, flag: true, nil: null, undef: undefined, fn: () => 'from fn', fnNull: () => null, fnNum: () => 42, '{1}': 'var one' } },
  { vars: { count: 1e21, neg: -0, nan: NaN, big: 123456789 } },
];
const TEMPLATES = ['{user} and {user.mention} and {user}', '{user.name} {user.username} {user.idname} {user.tag} {user.id} {user.avatar} {user.avatar_url} {user.bot}',
  '{user.display_name} {user.nick} {user.nickname} {user.created} {user.created_at} {user.joined} {user.joined_at}',
  '{server} {server.name} {server.id} {server.icon} {server.icon_url} {server.owner} {server.owner_id}',
  '{server.member_count} {server.member_count.ordinal} {server.verification_level} {server.joined_at} {server.created_at} {server.boost_count} {server.boost_level}',
  '{channel} {channel.mention} {channel.name} {channel.id} {channel.type}', '{USER.NAME} { user.name } {User.Id}', 'cost $& {user.name} $1 $$',
  '{1} {2} {3} {4} {5} {6} {0}', '{1:default} {4:fallback} {9:nine} {0:zero}', '{2...} {...2} {2...4} {4...2} {...} {0...} {...0} {0...2}',
  '{1000} {999} {1:a:b} {1:}', '{level} {Level} {xp} {flag} {nil} {undef} {fn} {fnNull} {fnNum}', '{count} {neg} {nan} {big}', '{unknown} {Unknown} {unknown} {other}',
  '{', '}', '{}', '{{user}}', '{user}}', '{{user.name}', '{a\nb}', `{${'x'.repeat(64)}}`, `{${'x'.repeat(65)}}`, '{😀}', `{${'😀'.repeat(32)}}`, `{${'😀'.repeat(33)}}`,
  Array.from({ length: 60 }, (_, i) => `{u${i}}`).join(' '), 'plain text', '', 'Welcome {user}! You are member #{server.member_count.ordinal} of {server}.',
  '{random:5:5} {random:-3:-3} {random:7:7}', '{random:1:a}', '{random:10000000000:1}', '{ random }'];
const encCtxArgs = (a) => [enc(a[0]), plainCtx(a[1] ?? {}), enc(a[2])];
rec('placeholderParser.renderPlaceholdersDetailed', ph.renderPlaceholdersDetailed, [
  ...cross(TEMPLATES, CTXS).map(([t, c]) => [t, c]),
  ...cross(TEMPLATES.slice(0, 10), [5, 20, 0, -1, 2.5]).map(([t, m]) => [t, { member: memberA, channel: chans[0] }, { maxLength: m }]),
  [null, {}], [undefined, {}], [123, {}], [{ a: 1 }, {}], ['{user}', null], ['{user}', 'str'],
], encCtxArgs);
rec('placeholderParser.renderPlaceholders', ph.renderPlaceholders, cross(TEMPLATES.slice(0, 12), CTXS.slice(0, 4)).map(([t, c]) => [t, c]), encCtxArgs);
rec('placeholderParser.parsePlaceholders', ph.parsePlaceholders, cross(TEMPLATES.slice(0, 8), [[memberA, guildA], [null, guildA], [memberB, null], [null, null]])
  .map(([t, [m, g]]) => [t, m, g]), (a) => [enc(a[0]), plainCtx({ member: a[1], guild: a[2] })]);
rec('placeholderParser.placeholderHelp', ph.placeholderHelp, [[], [['user']], [['server', 'user']], [['channel', 'random', 'args']], [['nope', 5, 'args']],
  ['user'], [null], [[]], [['user', 'server', 'channel', 'random', 'args']], [['toString', 'hasOwnProperty']]]);

// Constants other crates compare against.
out['constants'] = [[[], enc({
  PLACEHOLDERS: ph.PLACEHOLDERS, PLACEHOLDER_LIST: ph.PLACEHOLDER_LIST, ZONE_ABBREVIATIONS: time.ZONE_ABBREVIATIONS, WEEKDAY_NAMES: time.WEEKDAY_NAMES,
  LEGACY_TZ_NAMES: tzs.LEGACY_TZ_NAMES, POPULAR_TIMEZONES: tzs.POPULAR_TIMEZONES, TIMEZONE_ROLE_TEMPLATES: tzs.TIMEZONE_ROLE_TEMPLATES,
  TIMEZONE_TEMPLATE_PAST_ZONES: tzs.TIMEZONE_TEMPLATE_PAST_ZONES, TIMEZONE_PANEL: tzs.TIMEZONE_PANEL, FATAL_CLOSE_CODES: fg.FATAL_CLOSE_CODES,
  FATAL_MESSAGE_KEY: fg.FATAL_MESSAGE_KEY, MAX_DURATION_MS: dur.MAX_DURATION_MS, DEFAULT_MAX_SIDE: img.DEFAULT_MAX_SIDE,
  DEFAULT_MAX_PIXELS: img.DEFAULT_MAX_PIXELS, LINK_URL_MAX: cal.LINK_URL_MAX, MAX_PATTERN_LENGTH: sr.MAX_PATTERN_LENGTH,
  DEFAULT_INPUT_CAP: sr.DEFAULT_INPUT_CAP,
})]];

mkdirSync(OUT, { recursive: true });
writeFileSync(join(OUT, 'tables.json'), `${JSON.stringify(out)}\n`);
for (const [k, v] of Object.entries(out)) process.stdout.write(`${k}: ${v.length}\n`);
