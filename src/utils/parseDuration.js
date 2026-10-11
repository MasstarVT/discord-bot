// ── Duration parsing and formatting ────────────────────────────────────────────
// Pure; no imports. Neither function throws.

/** Longest accepted duration: 100 years. Larger inputs parse to null (R198). */
export const MAX_DURATION_MS = 100 * 365 * 86_400_000;

const UNIT_MS = Object.freeze({
  y:  31_536_000_000, // 365 days
  mo: 2_592_000_000,  // 30 days
  w:  604_800_000,
  d:  86_400_000,
  h:  3_600_000,
  m:  60_000,
  s:  1_000,
});

// Sticky tokenizer: the whole input must be consumed (R198). `months?|mos?` is tried
// before `m`, and a unit may not be followed by another letter, so '5ms' fails and
// '1 month' is 30 days, never 1 minute.
const TOKEN_RE = /(\d+(?:\.\d+)?|\.\d+)\s*(years?|yrs?|y|months?|mos?|weeks?|wks?|w|days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s)(?![a-z])/iy;
const SEP_RE = /(?:\s+|,|\band\b)*/iy;

function unitKey(unit) {
  const u = unit.toLowerCase();
  if (u.startsWith('mo')) return 'mo';
  if (u === 'm' || u.startsWith('mi')) return 'm';
  return u[0];
}

/**
 * Parses a human-readable duration into milliseconds.
 * Accepts decimals and the units y/yr/year, mo/month, w/wk/week, d/day, h/hr/hour,
 * m/min/minute, s/sec/second, e.g. "1h", "1.5h", "2h30m", "1 month", "5 minutes and 3s".
 * Only whitespace, ',' and 'and' may separate tokens; anything else gives null.
 * @param {string} str
 * @returns {number|null} milliseconds (1 .. MAX_DURATION_MS), or null if unparseable
 */
export function parseDuration(str) {
  if (typeof str !== 'string') return null;
  const s = str.trim();
  if (!s || s.length > 100) return null;
  let pos = 0;
  let sum = 0;
  while (pos < s.length) {
    if (pos > 0) {
      SEP_RE.lastIndex = pos;
      if (SEP_RE.exec(s)) pos = SEP_RE.lastIndex;
      if (pos >= s.length) break;
    }
    TOKEN_RE.lastIndex = pos;
    const m = TOKEN_RE.exec(s);
    if (!m) return null;
    sum += parseFloat(m[1]) * UNIT_MS[unitKey(m[2])];
    pos = TOKEN_RE.lastIndex;
  }
  const total = Math.round(sum);
  return Number.isFinite(total) && total > 0 && total <= MAX_DURATION_MS ? total : null;
}

/**
 * Formats milliseconds into a readable string like "2h 30m".
 * @param {number} ms
 * @returns {string}
 */
export function formatDuration(ms) {
  if (!ms || ms <= 0) return 'Permanent';
  const parts = [];
  const intervals = [
    [604_800_000, 'w'],
    [86_400_000,  'd'],
    [3_600_000,   'h'],
    [60_000,      'm'],
    [1_000,       's'],
  ];
  for (const [unit, label] of intervals) {
    const count = Math.floor(ms / unit);
    if (count > 0) { parts.push(`${count}${label}`); ms %= unit; }
  }
  return parts.join(' ') || '0s';
}

const LONG_UNITS = [
  [31_536_000, 'year'],
  [2_592_000,  'month'],
  [604_800,    'week'],
  [86_400,     'day'],
  [3_600,      'hour'],
  [60,         'minute'],
  [1,          'second'],
];

/**
 * Formats milliseconds in words, e.g. 9_000_000 → "2 hours 30 minutes".
 * @param {number} ms
 * @returns {string} 'Permanent' for 0, negative or non-finite input
 */
export function formatDurationLong(ms) {
  if (!ms || !Number.isFinite(ms) || ms <= 0) return 'Permanent';
  let secs = Math.floor(ms / 1000);
  const parts = [];
  for (const [size, label] of LONG_UNITS) {
    const n = Math.floor(secs / size);
    if (n > 0) { parts.push(`${n} ${label}${n === 1 ? '' : 's'}`); secs -= n * size; }
  }
  return parts.join(' ') || '0 seconds';
}
