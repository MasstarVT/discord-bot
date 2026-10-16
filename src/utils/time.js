// ── Timezone maths and date/time parsing ───────────────────────────────────────
// Pure; uses Intl.DateTimeFormat only. Nothing is constructed at import time: the
// formatters are created lazily and cached. No function here throws, and an invalid
// timezone anywhere is treated as 'UTC'.
import { parseDuration } from './parseDuration.js';

export const WEEKDAY_NAMES = Object.freeze(['sun', 'mon', 'tue', 'wed', 'thu', 'fri', 'sat']);

const DAY_MS = 86_400_000;
const MAX_FORMATTERS = 200;

/** @type {Map<string, Intl.DateTimeFormat>|null} */
let formatters = null;
/** @type {Map<string, boolean>|null} */
let validZones = null;

// ── Time zones ─────────────────────────────────────────────────────────────────

/**
 * True when `tz` is an IANA timezone name the runtime knows ('Europe/London', 'utc').
 * @param {unknown} tz
 * @returns {boolean}
 */
export function isValidTimeZone(tz) {
  if (typeof tz !== 'string' || tz.length < 1 || tz.length > 64) return false;
  validZones ??= new Map();
  const cached = validZones.get(tz);
  if (cached !== undefined) return cached;
  let ok = false;
  try {
    new Intl.DateTimeFormat('en-US', { timeZone: tz });
    ok = true;
  } catch {
    ok = false;
  }
  if (validZones.size >= MAX_FORMATTERS) validZones.delete(validZones.keys().next().value);
  validZones.set(tz, ok);
  return ok;
}

function safeTz(tz) {
  return isValidTimeZone(tz) ? tz : 'UTC';
}

// hourCycle 'h23' is mandatory: with hour12:false some engines print midnight as 24.
function fmt(tz) {
  formatters ??= new Map();
  let f = formatters.get(tz);
  if (!f) {
    f = new Intl.DateTimeFormat('en-US', {
      timeZone: tz,
      hourCycle: 'h23',
      year: 'numeric',
      month: 'numeric',
      day: 'numeric',
      hour: 'numeric',
      minute: 'numeric',
      second: 'numeric',
    });
    if (formatters.size >= MAX_FORMATTERS) formatters.delete(formatters.keys().next().value);
    formatters.set(tz, f);
  }
  return f;
}

/**
 * Wall-clock fields of an instant in a timezone.
 * @param {Date|number} date - a Date or epoch milliseconds (an invalid value gives the epoch)
 * @param {string} tz
 * @returns {{ year: number, month: number, day: number, hour: number, minute: number, second: number, weekday: number }}
 *   month is 1-12, weekday is 0 (Sunday) to 6
 */
export function zonedParts(date, tz) {
  let ms = date instanceof Date ? date.getTime() : Number(date);
  if (!Number.isFinite(ms)) ms = 0;
  const out = { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0, weekday: 4 };
  try {
    for (const p of fmt(safeTz(tz)).formatToParts(new Date(ms))) {
      if (p.type in out) out[p.type] = Number(p.value);
    }
  } catch {
    // keep the epoch defaults
  }
  if (out.hour === 24) out.hour = 0;
  out.weekday = new Date(Date.UTC(out.year, out.month - 1, out.day)).getUTCDay();
  return out;
}

// Offset of the timezone at an instant, in ms (positive east of UTC).
function offsetAt(ms, tz) {
  const p = zonedParts(ms, tz);
  return Date.UTC(p.year, p.month - 1, p.day, p.hour, p.minute, p.second) - Math.floor(ms / 1000) * 1000;
}

/**
 * Converts a local wall-clock time in `tz` to the matching instant.
 * A time inside a DST gap gives the first valid instant after the gap; a time inside an
 * overlap gives the earlier instant.
 * @param {{ year: number, month: number, day: number, hour?: number, minute?: number }} local
 * @param {string} tz
 * @returns {Date} an Invalid Date when a field is not a number
 */
export function zonedToUtc({ year, month, day, hour = 0, minute = 0 }, tz) {
  const zone = safeTz(tz);
  const wall = Date.UTC(year, month - 1, day, hour, minute);
  if (!Number.isFinite(wall)) return new Date(NaN);

  const offBefore = offsetAt(wall - DAY_MS, zone);
  const offAfter = offsetAt(wall + DAY_MS, zone);
  const offs = [...new Set([offBefore, offAfter])];
  const cands = offs.map((o) => wall - o).filter((t) => t + offsetAt(t, zone) === wall).sort((a, b) => a - b);
  if (cands.length) return new Date(cands[0]);

  // Gap: the wall time does not exist. Find the transition instant (first instant that
  // already has the offset after the gap), searching whole minutes.
  let lo = wall - offAfter;
  let hi = wall - offBefore;
  if (lo > hi) [lo, hi] = [hi, lo];
  let a = Math.floor(lo / 60_000);
  let b = Math.ceil(hi / 60_000);
  while (a < b) {
    const mid = Math.floor((a + b) / 2);
    if (offsetAt(mid * 60_000, zone) === offAfter) b = mid;
    else a = mid + 1;
  }
  return new Date(a * 60_000);
}

/**
 * Local calendar date key in a timezone.
 * @param {Date|number} date
 * @param {string} tz
 * @returns {string} 'YYYY-MM-DD'
 */
export function localDateKey(date, tz) {
  const p = zonedParts(date, tz);
  return `${String(p.year).padStart(4, '0')}-${String(p.month).padStart(2, '0')}-${String(p.day).padStart(2, '0')}`;
}

/**
 * @param {number} year
 * @param {number} month - 1-12
 * @returns {number}
 */
export function daysInMonth(year, month) {
  return new Date(Date.UTC(year, month, 0)).getUTCDate();
}

// ── Time of day and weekdays ───────────────────────────────────────────────────

const TIME_RE = /^(\d{1,2})(?::(\d{2}))?\s*(am|pm|a\.m\.|p\.m\.)?$/;

/**
 * Parses '9', '09:30', '9:30pm', '21:30', '12am', '12pm', 'noon' or 'midnight'.
 * @param {unknown} str
 * @returns {{ hour: number, minute: number } | null}
 */
export function parseTimeOfDay(str) {
  const s = String(str ?? '').trim().toLowerCase();
  if (s === 'noon') return { hour: 12, minute: 0 };
  if (s === 'midnight') return { hour: 0, minute: 0 };
  const m = TIME_RE.exec(s);
  if (!m) return null;
  let hour = Number(m[1]);
  const minute = m[2] === undefined ? 0 : Number(m[2]);
  if (minute < 0 || minute > 59) return null;
  if (m[3]) {
    if (hour < 1 || hour > 12) return null;
    const pm = m[3].startsWith('p');
    if (hour === 12) hour = pm ? 12 : 0;
    else if (pm) hour += 12;
  } else if (hour < 0 || hour > 23) {
    return null;
  }
  return { hour, minute };
}

/**
 * @param {{ hour: number, minute: number }} t
 * @returns {string} 'HH:MM'
 */
export function formatTimeOfDay({ hour, minute }) {
  return `${String(hour).padStart(2, '0')}:${String(minute).padStart(2, '0')}`;
}

const DAY_ALIASES = Object.freeze({
  sun: 0, sunday: 0,
  mon: 1, monday: 1,
  tue: 2, tues: 2, tuesday: 2,
  wed: 3, weds: 3, wednesday: 3,
  thu: 4, thur: 4, thurs: 4, thursday: 4,
  fri: 5, friday: 5,
  sat: 6, saturday: 6,
});

function dayToken(tok) {
  if (/^[0-6]$/.test(tok)) return Number(tok);
  return Object.hasOwn(DAY_ALIASES, tok) ? DAY_ALIASES[tok] : null;
}

/**
 * Parses a weekday list: 'mon,wed,fri', 'fri-mon' (wraps), 'weekdays', 'weekends', 'all', 'daily'.
 * @param {unknown} str
 * @returns {number[] | null} sorted unique days (0 = Sunday), or null when unreadable
 */
export function parseWeekdays(str) {
  const s = String(str ?? '').trim().toLowerCase();
  if (!s) return null;
  if (s === 'all' || s === 'daily' || s === 'everyday' || s === 'every day') return [0, 1, 2, 3, 4, 5, 6];
  if (s === 'weekdays') return [1, 2, 3, 4, 5];
  if (s === 'weekends') return [0, 6];
  const days = new Set();
  for (const tok of s.split(/[\s,]+/)) {
    if (!tok) continue;
    const single = dayToken(tok);
    if (single !== null) { days.add(single); continue; }
    const range = /^([a-z0-9]+)-([a-z0-9]+)$/.exec(tok);
    if (!range) return null;
    const from = dayToken(range[1]);
    const to = dayToken(range[2]);
    if (from === null || to === null) return null;
    for (let d = from; ; d = (d + 1) % 7) {
      days.add(d);
      if (d === to) break;
    }
  }
  return days.size ? [...days].sort((a, b) => a - b) : null;
}

// ── Scheduling ─────────────────────────────────────────────────────────────────

function allowedDays(weekdays) {
  const wd = (Array.isArray(weekdays) ? weekdays : []).filter((d) => Number.isInteger(d) && d >= 0 && d <= 6);
  return wd.length ? new Set(wd) : null;
}

// Local calendar date `offset` days from the date of `p`, via Date.UTC arithmetic.
function addDays(p, offset) {
  const d = new Date(Date.UTC(p.year, p.month - 1, p.day + offset));
  return { year: d.getUTCFullYear(), month: d.getUTCMonth() + 1, day: d.getUTCDate(), weekday: d.getUTCDay() };
}

/**
 * Next occurrence of a local time of day, strictly after `after`.
 * Exactly one instant per allowed local date: a DST gap gives the first valid instant after
 * the missing time, and an overlap gives the earlier instant only (it never fires twice).
 * @param {{ tz: string, hour: number, minute: number, weekdays?: number[], after?: Date }} opts
 * @returns {Date}
 */
export function nextTimeOfDay({ tz, hour, minute, weekdays = [], after = new Date() }) {
  const wd = allowedDays(weekdays);
  const afterMs = after instanceof Date ? after.getTime() : Number(after);
  if (!Number.isFinite(afterMs)) return new Date(NaN);
  const p = zonedParts(afterMs, tz);
  for (let k = 0; k <= 8; k++) {
    const d = addDays(p, k);
    if (wd && !wd.has(d.weekday)) continue;
    const t = zonedToUtc({ year: d.year, month: d.month, day: d.day, hour, minute }, tz);
    if (t.getTime() > afterMs) return t;
  }
  return new Date(afterMs + DAY_MS);
}

/**
 * Next run of a repeating interval, strictly after `after`.
 * - No window (both bounds null): candidates are `anchor + k·interval` (anchor null means `after`);
 *   the first one whose local weekday is allowed.
 * - Window (either bound set): a null start is 00:00 and a null end is 24:00. For each allowed
 *   local day D (scanning from the day before `after`) the candidates are `D at start + j·interval`
 *   while before the window end. An end at or before the start crosses midnight; a crossing window
 *   belongs to its start day. `anchor` is ignored.
 * @param {{ tz: string, intervalMinutes: number, weekdays?: number[], windowStart?: string|null,
 *   windowEnd?: string|null, after?: Date, anchor?: Date|null }} opts
 * @returns {Date | null} null when nothing exists within 8 days (or the input is invalid)
 */
export function nextIntervalRun({
  tz, intervalMinutes, weekdays = [], windowStart = null, windowEnd = null, after = new Date(), anchor = null,
}) {
  if (!Number.isInteger(intervalMinutes) || intervalMinutes < 1 || intervalMinutes > 10_080) return null;
  const I = intervalMinutes * 60_000;
  const afterMs = after instanceof Date ? after.getTime() : Number(after);
  if (!Number.isFinite(afterMs)) return null;
  const wd = allowedDays(weekdays);
  const horizon = afterMs + 8 * DAY_MS;

  if (windowStart == null && windowEnd == null) {
    const originMs = anchor != null && Number.isFinite(+anchor) ? +anchor : afterMs;
    let t = originMs + (Math.floor((afterMs - originMs) / I) + 1) * I;
    while (t <= horizon) {
      const p = zonedParts(t, tz);
      if (!wd || wd.has(p.weekday)) return new Date(t);
      // Jump to the first grid point on the next local day.
      const next = addDays(p, 1);
      const midnight = zonedToUtc({ year: next.year, month: next.month, day: next.day }, tz).getTime();
      t = originMs + Math.max(Math.ceil((midnight - originMs) / I), Math.floor((t - originMs) / I) + 1) * I;
    }
    return null;
  }

  const start = windowStart == null ? { hour: 0, minute: 0 } : parseTimeOfDay(windowStart);
  if (!start) return null;
  let end = null; // null = 24:00
  if (windowEnd != null) {
    end = parseTimeOfDay(windowEnd);
    if (!end) return null;
  }
  const startMin = start.hour * 60 + start.minute;
  const endNextDay = end === null || end.hour * 60 + end.minute <= startMin;

  const p = zonedParts(afterMs, tz);
  for (let k = -1; k <= 8; k++) {
    const d = addDays(p, k);
    if (wd && !wd.has(d.weekday)) continue;
    const segStart = zonedToUtc({ year: d.year, month: d.month, day: d.day, hour: start.hour, minute: start.minute }, tz).getTime();
    const e = addDays(p, k + (endNextDay ? 1 : 0));
    const segEnd = zonedToUtc({ year: e.year, month: e.month, day: e.day, hour: end?.hour ?? 0, minute: end?.minute ?? 0 }, tz).getTime();
    const lo = Math.max(segStart, afterMs + 1);
    const t = segStart + Math.ceil((lo - segStart) / I) * I;
    if (t < segEnd) return new Date(t);
  }
  return null;
}

// ── Date input ─────────────────────────────────────────────────────────────────

/**
 * Parses a date a user typed: a Discord timestamp, a relative duration ('2h30m', 'in 2 days'),
 * 'YYYY-MM-DD [HH:MM]', 'MM-DD [HH:MM]' (this year or the next ones) or 'HH:MM' (today or tomorrow).
 * Past 'YYYY-MM-DD' dates are returned as they are; the caller decides what to do with them.
 * @param {unknown} str
 * @param {string} tz
 * @param {{ now?: Date }} [opts]
 * @returns {Date | null}
 */
export function parseDateInput(str, tz, { now = new Date() } = {}) {
  const s = String(str ?? '').trim();
  if (!s) return null;
  const nowMs = now instanceof Date ? now.getTime() : Number(now);
  if (!Number.isFinite(nowMs)) return null;

  const stamp = /^<t:(\d{1,13})(?::[tTdDfFR])?>$/.exec(s);
  if (stamp) return new Date(Number(stamp[1]) * 1000);

  const rel = parseDuration(s.replace(/^in\s+/i, ''));
  if (rel !== null) return new Date(nowMs + rel);

  const full = /^(\d{4})-(\d{1,2})-(\d{1,2})(?:[ T](.+))?$/.exec(s);
  if (full) {
    const year = Number(full[1]);
    const month = Number(full[2]);
    const day = Number(full[3]);
    if (month < 1 || month > 12 || day < 1 || day > daysInMonth(year, month)) return null;
    const time = full[4] === undefined ? { hour: 0, minute: 0 } : parseTimeOfDay(full[4]);
    if (!time) return null;
    return zonedToUtc({ year, month, day, hour: time.hour, minute: time.minute }, tz);
  }

  const md = /^(\d{1,2})-(\d{1,2})(?:\s+(.+))?$/.exec(s);
  if (md) {
    const month = Number(md[1]);
    const day = Number(md[2]);
    if (month < 1 || month > 12 || day < 1) return null;
    const time = md[3] === undefined ? { hour: 0, minute: 0 } : parseTimeOfDay(md[3]);
    if (!time) return null;
    const thisYear = zonedParts(nowMs, tz).year;
    for (let k = 0; k <= 8; k++) {
      const year = thisYear + k;
      if (day > daysInMonth(year, month)) continue;
      const t = zonedToUtc({ year, month, day, hour: time.hour, minute: time.minute }, tz);
      if (t.getTime() > nowMs) return t;
    }
    return null;
  }

  const tod = parseTimeOfDay(s);
  if (tod) return nextTimeOfDay({ tz, hour: tod.hour, minute: tod.minute, after: new Date(nowMs) });
  return null;
}

const MONTHS = Object.freeze({
  january: 1, jan: 1, february: 2, feb: 2, march: 3, mar: 3, april: 4, apr: 4, may: 5,
  june: 6, jun: 6, july: 7, jul: 7, august: 8, aug: 8, september: 9, sep: 9, sept: 9,
  october: 10, oct: 10, november: 11, nov: 11, december: 12, dec: 12,
});

/**
 * Parses a birthday: 'MM-DD', 'YYYY-MM-DD', 'march 14', '14 march', 'mar 14 1990', '14 mar 1990'.
 * Anything with '/' is rejected (DD/MM and MM/DD are ambiguous). Feb 29 is allowed without a year.
 * @param {unknown} str
 * @param {{ requireYear?: boolean, now?: Date }} [opts]
 * @returns {{ month: number, day: number, year: number | null } | null}
 */
export function parseBirthday(str, { requireYear = false, now = new Date() } = {}) {
  const s = String(str ?? '').trim().toLowerCase();
  if (!s || s.includes('/')) return null;
  let month; let day; let year = null;
  let m;
  if ((m = /^(\d{1,2})-(\d{1,2})$/.exec(s))) {
    month = Number(m[1]); day = Number(m[2]);
  } else if ((m = /^(\d{4})-(\d{1,2})-(\d{1,2})$/.exec(s))) {
    year = Number(m[1]); month = Number(m[2]); day = Number(m[3]);
  } else if ((m = /^([a-z]+)\.?\s+(\d{1,2})(?:st|nd|rd|th)?(?:,?\s+(\d{4}))?$/.exec(s))) {
    month = Object.hasOwn(MONTHS, m[1]) ? MONTHS[m[1]] : 0; day = Number(m[2]);
    if (m[3] !== undefined) year = Number(m[3]);
  } else if ((m = /^(\d{1,2})(?:st|nd|rd|th)?\s+([a-z]+)\.?(?:,?\s+(\d{4}))?$/.exec(s))) {
    day = Number(m[1]); month = Object.hasOwn(MONTHS, m[2]) ? MONTHS[m[2]] : 0;
    if (m[3] !== undefined) year = Number(m[3]);
  } else {
    return null;
  }
  if (!month || month < 1 || month > 12 || !day || day < 1) return null;
  const nowYear = (now instanceof Date ? now : new Date(now)).getUTCFullYear();
  if (year !== null && (year < 1900 || year > nowYear)) return null;
  if (year === null && requireYear) return null;
  if (day > daysInMonth(year ?? 2000, month)) return null;
  return { month, day, year };
}

/**
 * @param {Date|number} date
 * @param {string} [style='f'] - one of t T d D f F R
 * @returns {string} e.g. '<t:1700000000:f>'
 */
export function discordTimestamp(date, style = 'f') {
  const ms = date instanceof Date ? date.getTime() : Number(date);
  const st = typeof style === 'string' && style.length === 1 && 'tTdDfFR'.includes(style) ? style : 'f';
  return `<t:${Math.floor(ms / 1000)}:${st}>`;
}

// ── Free-form times (/timestamp) ───────────────────────────────────────────────

// Abbreviations people type, mapped to the region they name. Each means that region's local time,
// daylight saving included: "7pm CST" in July is 7 pm in Chicago. Never hand these to Intl, where
// EST, MST and HST are fixed offsets with no DST. Ambiguous ones are left out on purpose
// (IST: India, Ireland or Israel; AST: Atlantic or Arabia).
export const ZONE_ABBREVIATIONS = Object.freeze({
  et: 'America/New_York', est: 'America/New_York', edt: 'America/New_York',
  ct: 'America/Chicago', cst: 'America/Chicago', cdt: 'America/Chicago',
  mt: 'America/Denver', mst: 'America/Denver', mdt: 'America/Denver',
  pt: 'America/Los_Angeles', pst: 'America/Los_Angeles', pdt: 'America/Los_Angeles',
  akst: 'America/Anchorage', akdt: 'America/Anchorage', hst: 'Pacific/Honolulu',
  adt: 'America/Halifax', nst: 'America/St_Johns', ndt: 'America/St_Johns',
  brt: 'America/Sao_Paulo', art: 'America/Argentina/Buenos_Aires',
  wet: 'Europe/Lisbon', west: 'Europe/Lisbon', bst: 'Europe/London',
  cet: 'Europe/Berlin', cest: 'Europe/Berlin', eet: 'Europe/Athens', eest: 'Europe/Athens',
  msk: 'Europe/Moscow', wat: 'Africa/Lagos', sast: 'Africa/Johannesburg', eat: 'Africa/Nairobi',
  gst: 'Asia/Dubai', pkt: 'Asia/Karachi', npt: 'Asia/Kathmandu', ict: 'Asia/Bangkok', wib: 'Asia/Jakarta',
  sgt: 'Asia/Singapore', hkt: 'Asia/Hong_Kong', pht: 'Asia/Manila', jst: 'Asia/Tokyo', kst: 'Asia/Seoul',
  awst: 'Australia/Perth', acst: 'Australia/Adelaide', acdt: 'Australia/Adelaide',
  aest: 'Australia/Sydney', aedt: 'Australia/Sydney', nzst: 'Pacific/Auckland', nzdt: 'Pacific/Auckland',
});

const UTC_WORDS = new Set(['utc', 'gmt', 'z', 'zulu']);

/**
 * The zone an abbreviation names ('CST' → 'America/Chicago'); 'UTC' and 'GMT' give 'UTC'.
 * @param {unknown} str
 * @returns {string | null}
 */
export function zoneFromAbbreviation(str) {
  const s = String(str ?? '').trim().toLowerCase();
  if (UTC_WORDS.has(s)) return 'UTC';
  return Object.hasOwn(ZONE_ABBREVIATIONS, s) ? ZONE_ABBREVIATIONS[s] : null;
}

/**
 * A whole-hour offset ('utc+2', 'GMT-5', 'utc+05:00') as its Etc/GMT zone. Etc/GMT signs are
 * inverted (UTC+2 is Etc/GMT-2), and these zones have no daylight saving.
 * @param {unknown} str
 * @returns {string | null} null for anything else, including half-hour offsets
 */
export function zoneFromOffset(str) {
  const m = /^(?:utc|gmt)\s*([+\-−])\s*(\d{1,2})(?::?00)?$/i.exec(String(str ?? '').trim());
  if (!m) return null;
  const hours = Number(m[2]);
  if (hours === 0) return 'UTC';
  const east = m[1] === '+';
  if (hours > (east ? 14 : 12)) return null;
  return `Etc/GMT${east ? '-' : '+'}${hours}`;
}

// A trailing word of /timestamp input that names a zone, or null.
function zoneToken(tok) {
  const named = zoneFromAbbreviation(tok) ?? zoneFromOffset(tok);
  if (named) return named;
  if (!tok.includes('/') || !isValidTimeZone(tok)) return null;
  try {
    return fmt(tok).resolvedOptions().timeZone; // canonical spelling: 'europe/london' → 'Europe/London'
  } catch {
    return null;
  }
}

const DAY_WORDS = Object.freeze({ yesterday: -1, today: 0, tomorrow: 1, tmrw: 1, tmr: 1 });
const DAY_WORD_RE = /^(?:yesterday|today|tomorrow|tmrw|tmr)$/;
const WEEKDAY_RE = /^(?:(next|this) )?([a-z]+)$/;
const LATEST_MS = Date.UTC(9999, 11, 31, 23, 59, 59);

// '7pm', '19:00', 'noon'; no text means midnight.
const timeOrMidnight = (part) => (part === undefined ? { hour: 0, minute: 0 } : parseTimeOfDay(part));

// Splits '<day> [time]' or '<time> <day>' where `isDay` recognises the day part. Two-word day
// parts go first, so '7pm next fri' is not read as 'fri' at '7pm next'.
function splitDay(s, isDay) {
  const words = s.split(' ');
  for (const n of [2, 1]) {
    if (words.length < n) continue;
    const head = words.slice(0, n).join(' ');
    if (isDay(head)) return [head, words.slice(n).join(' ') || undefined];
    if (words.length > n) {
      const tail = words.slice(-n).join(' ');
      if (isDay(tail)) return [tail, words.slice(0, -n).join(' ')];
    }
  }
  return null;
}

const isWeekdayPart = (part) => {
  const m = WEEKDAY_RE.exec(part);
  return Boolean(m && Object.hasOwn(DAY_ALIASES, m[2]));
};

// The /timestamp-only forms, read in `tz`. undefined = not one of them (parseDateInput decides).
function parseLocal(s, tz, nowMs) {
  if (s === 'now') return new Date(nowMs);
  const today = zonedParts(nowMs, tz);

  const dayWord = splitDay(s, (part) => DAY_WORD_RE.test(part));
  if (dayWord) {
    const time = timeOrMidnight(dayWord[1]);
    if (!time) return null;
    const d = addDays(today, DAY_WORDS[dayWord[0]]);
    return zonedToUtc({ year: d.year, month: d.month, day: d.day, ...time }, tz);
  }

  const weekday = splitDay(s, isWeekdayPart);
  if (weekday) {
    const [, which, name] = WEEKDAY_RE.exec(weekday[0]);
    const time = timeOrMidnight(weekday[1]);
    if (!time) return null;
    // 'fri' is the next Friday whose time is still ahead (today counts); 'next fri' never means today.
    for (let k = which === 'next' ? 1 : 0; k <= 7; k++) {
      const d = addDays(today, k);
      if (d.weekday !== DAY_ALIASES[name]) continue;
      const t = zonedToUtc({ year: d.year, month: d.month, day: d.day, ...time }, tz);
      if (k > 0 || t.getTime() > nowMs) return t;
    }
    return null;
  }

  const md = /^([a-z]+)\.? (\d{1,2})(?:st|nd|rd|th)?(?:,? (\d{4}))?(?:,? (.+))?$/.exec(s)
    ?? /^(\d{1,2})(?:st|nd|rd|th)? ([a-z]+)\.?(?:,? (\d{4}))?(?:,? (.+))?$/.exec(s);
  if (md && Object.hasOwn(MONTHS, /^\d/.test(md[1]) ? md[2] : md[1])) {
    const month = MONTHS[/^\d/.test(md[1]) ? md[2] : md[1]];
    const day = Number(/^\d/.test(md[1]) ? md[1] : md[2]);
    const time = timeOrMidnight(md[4]);
    if (!time || day < 1) return null;
    if (md[3] !== undefined) {
      const year = Number(md[3]);
      if (year < 1970 || day > daysInMonth(year, month)) return null;
      return zonedToUtc({ year, month, day, ...time }, tz);
    }
    // No year: this year unless that date is over, so 'oct 16' on Oct 16 means today.
    let year = today.year + (month * 100 + day < today.month * 100 + today.day ? 1 : 0);
    for (let k = 0; k <= 8; k++, year++) {
      if (day <= daysInMonth(year, month)) return zonedToUtc({ year, month, day, ...time }, tz);
    }
    return null;
  }
  return undefined;
}

/**
 * Parses a time typed for /timestamp. On top of parseDateInput (Discord timestamps, '2h30m',
 * 'in 2 days', 'YYYY-MM-DD [HH:MM]', 'MM-DD [HH:MM]', '7pm'), it reads 'now',
 * 'today|tomorrow|yesterday [time]', '[next] fri [time]' (also '7pm fri'), 'oct 20 [2027] [time]' and
 * '20 oct [time]', with an optional 'at' ('fri at 7pm') and an optional last word naming the zone:
 * an abbreviation from ZONE_ABBREVIATIONS ('7pm cst'), 'utc', a whole-hour offset ('utc+2') or a
 * Region/City name. A day with no time means midnight. Past times are returned as they are.
 * @param {unknown} str
 * @param {string} tz - the zone used when the text names none
 * @param {{ now?: Date }} [opts]
 * @returns {{ date: Date, zone: string, zoneFromInput: boolean } | null}
 */
export function parseWhen(str, tz, { now = new Date() } = {}) {
  let s = String(str ?? '').trim().replace(/\s+/g, ' ');
  if (!s || s.length > 100) return null;
  const nowMs = now instanceof Date ? now.getTime() : Number(now);
  if (!Number.isFinite(nowMs)) return null;

  let zone = safeTz(tz);
  let zoneFromInput = false;
  s = s.replace(/\b(utc|gmt) ([+\-−])/gi, '$1$2'); // 'utc +2' → 'utc+2'
  const last = s.lastIndexOf(' ');
  if (last > 0) {
    const named = zoneToken(s.slice(last + 1));
    if (named) {
      zone = named;
      zoneFromInput = true;
      s = s.slice(0, last);
    }
  }
  s = s.replace(/^at /i, '').replace(/ at /i, ' ');

  let date = parseLocal(s.toLowerCase(), zone, nowMs);
  if (date === undefined) date = parseDateInput(s, zone, { now: new Date(nowMs) });
  const ms = date?.getTime();
  if (!Number.isFinite(ms) || ms < 0 || ms > LATEST_MS) return null;
  return { date, zone, zoneFromInput };
}
