// ── Text and number formatting helpers ─────────────────────────────────────────
// Pure functions with no imports. None of them ever throws.

/**
 * Cuts a string to at most `max` characters, ending with `ellipsis` when it was cut.
 * Never splits a surrogate pair, and the result length is always ≤ max.
 * @param {unknown} str
 * @param {number} max
 * @param {string} [ellipsis='…']
 * @returns {string} '' for null/undefined
 */
export function clampText(str, max, ellipsis = '…') {
  try {
    if (str == null) return '';
    const s = String(str);
    const limit = Math.floor(Number(max));
    if (!Number.isFinite(limit)) return s;
    if (limit <= 0) return '';
    if (s.length <= limit) return s;
    const ell = String(ellipsis ?? '');
    if (limit <= ell.length) return ell.slice(0, limit);
    let cut = s.slice(0, limit - ell.length);
    const last = cut.charCodeAt(cut.length - 1);
    if (last >= 0xd800 && last <= 0xdbff) cut = cut.slice(0, -1);
    cut = cut.replace(/\s+$/, '');
    return cut + ell;
  } catch {
    return '';
  }
}

/**
 * Packs lines greedily into chunks joined with '\n', each at most `maxLen` long.
 * An overlong single line is clamped first.
 * @param {unknown[]} lines
 * @param {number} [maxLen=1024]
 * @returns {string[]}
 */
export function chunkLines(lines, maxLen = 1024) {
  try {
    if (!Array.isArray(lines)) return [];
    const chunks = [];
    let current = null;
    for (const raw of lines) {
      const line = clampText(String(raw), maxLen);
      if (current === null) { current = line; continue; }
      if (current.length + 1 + line.length <= maxLen) current += `\n${line}`;
      else { chunks.push(current); current = line; }
    }
    if (current !== null) chunks.push(current);
    return chunks;
  } catch {
    return [];
  }
}

/**
 * Joins items with `sep`, keeping only whole items, and appends `more(omitted)` when
 * some had to be left out. The result is at most `max` characters (R260: never cuts a mention).
 * @param {Iterable<unknown>} items
 * @param {{ sep?: string, max?: number, more?: (n: number) => string }} [opts]
 * @returns {string}
 */
export function joinWithLimit(items, { sep = ', ', max = 1024, more = (n) => `… and ${n} more` } = {}) {
  try {
    const strs = [...(items ?? [])].map(String);
    if (!strs.length) return '';
    const whole = strs.join(sep);
    if (whole.length <= max) return whole; // nothing needs omitting, so no "… and N more" tail is reserved
    const kept = [];
    let len = 0;
    for (let i = 0; i < strs.length; i++) {
      const nextLen = len + (kept.length ? sep.length : 0) + strs[i].length;
      const remaining = strs.length - (i + 1);
      const tail = remaining > 0 ? sep.length + String(more(remaining)).length : 0;
      if (nextLen + tail > max) break;
      kept.push(strs[i]);
      len = nextLen;
    }
    // Fix-up: the "… and N more" text for the real omitted count must also fit.
    while (kept.length > 0 && kept.length < strs.length
      && (kept.join(sep) + sep + more(strs.length - kept.length)).length > max) {
      kept.pop();
    }
    if (kept.length === strs.length) return kept.join(sep);
    if (kept.length === 0) return clampText(more(strs.length), max);
    return kept.join(sep) + sep + more(strs.length - kept.length);
  } catch {
    return '';
  }
}

/**
 * Formats a number with en-US grouping (BigInt supported). Non-numbers give '0'.
 * @param {unknown} n
 * @returns {string}
 */
export function formatNumber(n) {
  try {
    if (typeof n === 'bigint') return n.toLocaleString('en-US');
    const v = Number(n);
    if (Number.isFinite(v)) return v.toLocaleString('en-US', { maximumFractionDigits: 2 });
    return '0';
  } catch {
    return '0';
  }
}

/**
 * English ordinal with en-US grouping: 1st, 2nd, 3rd, 11th, 22nd, 1,001st.
 * @param {unknown} n
 * @returns {string}
 */
export function ordinal(n) {
  try {
    const v = Math.trunc(Number(n));
    if (!Number.isFinite(v)) return String(n);
    const a = Math.abs(v);
    const mod100 = a % 100;
    let suffix = 'th';
    if (mod100 < 11 || mod100 > 13) {
      const mod10 = a % 10;
      if (mod10 === 1) suffix = 'st';
      else if (mod10 === 2) suffix = 'nd';
      else if (mod10 === 3) suffix = 'rd';
    }
    return formatNumber(v) + suffix;
  } catch {
    return '';
  }
}

const COMPACT_UNITS = [['K', 1e3], ['M', 1e6], ['B', 1e9], ['T', 1e12]];

/**
 * Short number: 999 → '999', 1500 → '1.5K', 999_950 → '1M' (never '1000.0K', R306).
 * @param {unknown} n
 * @returns {string}
 */
export function compactNumber(n) {
  try {
    const v = Number(n);
    if (!Number.isFinite(v)) return '0';
    const abs = Math.abs(v);
    if (abs < 1000) return String(Math.trunc(v));
    let idx = 0;
    for (let i = COMPACT_UNITS.length - 1; i >= 0; i--) {
      if (abs >= COMPACT_UNITS[i][1]) { idx = i; break; }
    }
    let r = Math.round((v / COMPACT_UNITS[idx][1]) * 10) / 10;
    if (Math.abs(r) >= 1000 && idx < COMPACT_UNITS.length - 1) {
      idx += 1;
      r = Math.round((v / COMPACT_UNITS[idx][1]) * 10) / 10;
    }
    return String(r).replace(/\.0$/, '') + COMPACT_UNITS[idx][0];
  } catch {
    return '0';
  }
}

/**
 * Text progress bar, e.g. '▰▰▰▱▱▱▱▱▱▱'.
 * @param {number} fraction - 0..1 (clamped; NaN gives 0)
 * @param {number} [width=10] - 1..100
 * @param {{ filled?: string, empty?: string }} [chars]
 * @returns {string}
 */
export function progressBar(fraction, width = 10, { filled = '▰', empty = '▱' } = {}) {
  try {
    let f = Number(fraction);
    if (Number.isNaN(f)) f = 0;
    f = Math.min(1, Math.max(0, f));
    let w = Math.trunc(Number(width));
    if (!Number.isFinite(w)) w = 10;
    w = Math.min(100, Math.max(1, w));
    const k = Math.round(f * w);
    return String(filled).repeat(k) + String(empty).repeat(w - k);
  } catch {
    return '';
  }
}

/**
 * Normalises a user-supplied name for use as a key: lowercase, accents removed,
 * whitespace → '-', only [a-z0-9_-] kept. An empty result gives `fallback`.
 * @param {unknown} str
 * @param {{ max?: number, fallback?: string }} [opts]
 * @returns {string}
 */
export function sanitizeName(str, { max = 32, fallback = 'item' } = {}) {
  try {
    const out = String(str ?? '')
      .normalize('NFKD')
      .replace(/[̀-ͯ]/g, '')
      .toLowerCase()
      .trim()
      .replace(/\s+/g, '-')
      .replace(/[^a-z0-9_-]/g, '')
      .replace(/-{2,}/g, '-')
      .replace(/^-+|-+$/g, '')
      .slice(0, max)
      .replace(/-+$/, '');
    return out || fallback;
  } catch {
    return fallback;
  }
}
