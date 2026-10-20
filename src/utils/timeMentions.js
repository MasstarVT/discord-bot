// ── Times typed in chat (Auto Timestamps) ──────────────────────────────────────
// Pure. Finds clock times in a message ("fri 5pm", "tomorrow at 18:30", "7pm EST", "oct 20 9:30am") and reads each
// with parseWhen. Strict on purpose, because it answers ordinary chat: a time needs am/pm, noon or midnight, or a
// 24-hour HH:MM whose hour has a leading zero or is 13 or more ("09:00", "18:30"; "10:30" could be either half of the
// day). 24-hour times, noon and midnight also need a day, a zone or "at" next to them. Bare hours ("at 5"),
// durations ("in 2h"), day words with no time and anything in code, quotes, spoilers, links or mentions are skipped,
// and so is a message that already holds a Discord timestamp.
import { parseWhen, ZONE_ABBREVIATIONS, zoneFromAbbreviation, zoneFromOffset } from './time.js';

const WEEKDAY = 'sun(?:day)?|mon(?:day)?|tue(?:s(?:day)?)?|wed(?:s|nesday)?|thu(?:r(?:s(?:day)?)?)?|fri(?:day)?|sat(?:urday)?';
const MONTH = 'jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|june?|july?|aug(?:ust)?|sep(?:t(?:ember)?)?'
  + '|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?';
const ORD = '(?:st|nd|rd|th)?';
const DAY = `(?:(?:next|this) )?(?:${WEEKDAY})|today|tonight|tomorrow|tmrw|tmr`
  + `|(?:${MONTH})\\.? \\d{1,2}${ORD}(?:,? \\d{4})?|\\d{1,2}${ORD} (?:of )?(?:${MONTH})\\.?(?:,? \\d{4})?|\\d{4}-\\d{1,2}-\\d{1,2}`;
const TIME = '(?<h12>\\d{1,2})(?:[:.](?<m12>[0-5]\\d))? ?(?<ap>[ap])\\.?m\\.?(?![a-z0-9])'
  + '|(?<h24>0\\d|1[3-9]|2[0-3]):(?<m24>[0-5]\\d)(?![\\w:])'
  + '|(?<word>noon|midnight)(?![a-z])';
// Longest first, so 'cest' is tried before 'cet'.
const ABBREVIATIONS = [...Object.keys(ZONE_ABBREVIATIONS), 'utc', 'gmt'].sort((a, b) => b.length - a.length).join('|');
const ZONE = `(?<offset>(?:utc|gmt) ?[+\\-−] ?\\d{1,2}(?::?00)?)(?![\\d:])|(?<abbr>${ABBREVIATIONS})(?![\\w+\\-−])`
  + '|(?<badzone>(?:utc|gmt)\\S*)';

const MENTION_RE = new RegExp(
  `(?<![\\w:.'/$£€#])(?:(?<dayB>${DAY}),? )?(?<at>at |@ ?)?(?:${TIME})(?: (?:${ZONE}))?(?:,? (?:on )?(?<dayA>${DAY})(?![\\w-]))?`,
  'gi',
);
// A zone after a trailing day: "5pm on friday EST".
const TRAILING_ZONE_RE = new RegExp(` (?:${ZONE})`, 'iy');

// Zone abbreviations that are also English words: only read as zones when typed in capitals ("5pm EAT").
const WORDLIKE_ABBREVIATIONS = new Set(['art', 'eat', 'wat', 'west', 'wet']);

// Code, quotes, spoilers, links and Discord tokens (mentions, emojis, existing timestamps) are never read.
const IGNORED_PARTS = [
  /```[\s\S]*?```/g,
  /`[^`]*`/g,
  /\|\|[\s\S]*?\|\|/g,
  /^>>> [\s\S]*/m,
  /^> .*$/gm,
  /\bhttps?:\/\/\S+/gi,
  /<[^<>\s]+>/g,
];

/**
 * Cheap pre-filter for every message: true when the text may hold a time (the full scan happens later).
 * @param {unknown} content
 * @returns {boolean}
 */
export function mayMentionTime(content) {
  return typeof content === 'string' && /\d ?[ap]\.?m|\d:[0-5]\d|noon|midnight/i.test(content);
}

const normalizeDay = (day) => {
  const d = day.toLowerCase().replace(/ of /, ' ');
  return d === 'tonight' ? 'today' : d;
};

function timeText(g) {
  if (g.word) return g.word.toLowerCase();
  if (g.h24) return `${g.h24}:${g.m24}`;
  return `${g.h12}${g.m12 ? `:${g.m12}` : ''}${g.ap.toLowerCase()}m`;
}

// The zone a matched zone word names; undefined when it is not a usable zone.
function namedZone(g) {
  if (g.offset) return zoneFromOffset(g.offset.replace(/ /g, '')) ?? undefined;
  if (!g.abbr) return undefined;
  if (WORDLIKE_ABBREVIATIONS.has(g.abbr.toLowerCase()) && g.abbr !== g.abbr.toUpperCase()) return undefined;
  return zoneFromAbbreviation(g.abbr) ?? undefined;
}

/**
 * The times in a message, not yet read: what was typed, the day and time parseWhen will read, and the zone the
 * text names (null when it names none). At most `max` spans (a cap on the scan, not on the answer).
 * @param {unknown} content
 * @param {{ max?: number }} [opts]
 * @returns {{ text: string, day: string|null, time: string, zone: string|null }[]}
 */
export function extractTimeSpans(content, { max = 10 } = {}) {
  // A message that already holds a Discord timestamp has done the job itself.
  if (!mayMentionTime(content) || /<t:-?\d/.test(content)) return [];
  let text = content;
  for (const re of IGNORED_PARTS) text = text.replace(re, ' ');
  text = text.replace(/\s+/g, ' ');

  const spans = [];
  for (const m of text.matchAll(MENTION_RE)) {
    let g = m.groups;
    let typed = m[0];
    if (g.dayA && !g.offset && !g.abbr && !g.badzone) {
      TRAILING_ZONE_RE.lastIndex = m.index + m[0].length;
      const z = TRAILING_ZONE_RE.exec(text);
      if (z) {
        g = { ...g, offset: z.groups.offset, abbr: z.groups.abbr, badzone: z.groups.badzone };
        typed += z[0];
      }
    }
    if (g.badzone) continue; // 'utc+5:30': a zone we cannot read, so the time cannot be read either
    const zone = namedZone(g) ?? null;
    if (g.abbr && !zone) typed = typed.replace(` ${g.abbr}`, ''); // '5pm eat': the word is not part of the time
    const day = g.dayB ?? g.dayA ?? null;
    // A 24-hour time, noon or midnight on its own is too often a score, a video position or a plain word.
    if (!g.ap && !day && !zone && !g.at) continue;
    spans.push({ text: typed.trim(), day: day && normalizeDay(day), time: timeText(g), zone });
    if (spans.length >= max) break;
  }
  return spans;
}

/**
 * Reads spans as instants. A span that names no zone is read in `zone`, and skipped when `zone` is null. A time
 * with no day is today's when it passed less than `pastGraceMs` ago, else the next one. Instants further in the past
 * than that, or more than a year ahead, are dropped; so are repeats.
 * @param {ReturnType<typeof extractTimeSpans>} spans
 * @param {string|null} zone - the author's zone, or null when they have none
 * @param {{ now?: Date, pastGraceMs?: number, max?: number }} [opts]
 * @returns {{ text: string, date: Date, zone: string, zoneFromInput: boolean }[]}
 */
export function readTimeSpans(spans, zone, { now = new Date(), pastGraceMs = 3_600_000, max = 3 } = {}) {
  const nowMs = now.getTime();
  const out = [];
  const seen = new Set();
  for (const span of spans) {
    const tz = span.zone ?? zone;
    if (!tz) continue;
    let parsed = parseWhen(`${span.day ?? 'today'} ${span.time}`, tz, { now });
    if (!span.day && parsed && parsed.date.getTime() < nowMs - pastGraceMs) parsed = parseWhen(span.time, tz, { now });
    const ms = parsed?.date.getTime();
    if (!Number.isFinite(ms) || ms < nowMs - pastGraceMs || ms > nowMs + 366 * 86_400_000 || seen.has(ms)) continue;
    seen.add(ms);
    out.push({ text: span.text, date: parsed.date, zone: tz, zoneFromInput: Boolean(span.zone) });
    if (out.length >= max) break;
  }
  return out;
}
