const UNITS = {
  s: 1_000,
  m: 60_000,
  h: 3_600_000,
  d: 86_400_000,
  w: 604_800_000,
};

const PATTERN = /(\d+)\s*(w(?:eeks?)?|d(?:ays?)?|h(?:rs?|ours?)?|m(?:ins?|inutes?)?|s(?:ec(?:onds?)?)?)/gi;

/**
 * Parses a human-readable duration string into milliseconds.
 * Supports: "1h", "30m", "7d", "2h30m", "1w", "90s", etc.
 * @param {string} str
 * @returns {number|null} milliseconds, or null if unparseable
 */
export function parseDuration(str) {
  if (!str) return null;
  let ms = 0;
  let matched = false;
  for (const [, n, unit] of str.matchAll(PATTERN)) {
    ms += parseInt(n, 10) * (UNITS[unit[0].toLowerCase()] ?? 0);
    matched = true;
  }
  return matched && ms > 0 ? ms : null;
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
