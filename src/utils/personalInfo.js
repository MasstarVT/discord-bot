// ── Personal information (doxxing) detection ───────────────────────────────────
// Finds phone numbers, street addresses, email addresses, US social security numbers and payment card numbers in
// message text, for the automod Personal Info rule, and blanks them out of the logs that would otherwise repeat them.
//
// Built to keep false positives low: ordinary numbers, dates, times, prices, Discord ids, mentions, custom emoji,
// timestamps and links never count. A phone number needs a phone shape (+country code, the North American 3-3-4
// shape, or a 0-prefixed national number of 10-11 digits), a card number must pass the Luhn check, and a street
// address needs a house number, a name and a street suffix ("12 Oak Street", "4521 n main st").
// IP addresses are deliberately not matched: game-server addresses are shared all the time.
//
// Every regex is fixed (no user input) and bounded; the Rust port (bot-util `personal_info`) uses the same patterns.

/** Kinds in detection order; also the order of the detail text. */
export const PERSONAL_INFO_KINDS = Object.freeze(['phone', 'address', 'email', 'ssn', 'card']);

export const PERSONAL_INFO_LABELS = Object.freeze({
  phone: 'phone number',
  address: 'street address',
  email: 'email address',
  ssn: 'social security number',
  card: 'card number',
});

/** The deletion-tracker reason automod uses for a Personal Info delete; the message-delete log redacts on it. */
export const PERSONAL_INFO_DELETE_REASON = 'automod: personal info';

/** Text scanned at most (a Nitro message is 4000 characters; forwards add their snapshots). */
export const PERSONAL_INFO_INPUT_CAP = 8_000;

// Invisible and zero-width characters (the automod set): "5​5​5…" must not hide a number.
const INVISIBLE_RE = /[\u{AD}\u{34F}\u{61C}\u{180E}\u{200B}-\u{200F}\u{202A}-\u{202E}\u{2060}-\u{206F}\u{FEFF}]/gu;

// Tokens that hold digits but are never personal info: custom emoji, mentions, Discord timestamps and links.
const SKIP_RE = /<a?:\w{2,32}:\d{17,20}>|<(?:@[!&]?|#)\d{17,20}>|<t:-?\d{1,13}(?::[tTdDfFR])?>|https?:\/\/\S+/giu;

// Before a number: not part of a longer number, word, price, id, decimal or version ("phone: 555…" is fine).
const NB = String.raw`(?<![\p{L}\p{N}_+#$€£¥]|\p{N}[.,/:-])`;
// After a number: not followed by more digits, a decimal part or a dash/dot run of digits.
const NA = String.raw`(?![\p{L}\p{N}_]|[.,:/-]\p{N})`;

const PATTERNS = Object.freeze({
  phone: [
    // International: +country code, then 7-14 more digits in groups.
    new RegExp(String.raw`${NB}\+\d{1,3}(?:[ .-]?\(\d{1,5}\))?[ .-]?\d{1,14}(?:[ .-]\d{1,14}){0,5}${NA}`, 'gu'),
    // North American 3-3-4 with separators or (area code), optional leading 1.
    new RegExp(String.raw`${NB}(?:1[ .-]?)?(?:\([2-9]\d{2}\)[ .-]?|[2-9]\d{2}[ .-])\d{3}[ .-]\d{4}${NA}`, 'gu'),
    // North American, ten bare digits (valid area code and exchange, so timestamps and round numbers do not count).
    new RegExp(String.raw`${NB}[2-9]\d{2}[2-9]\d{6}${NA}`, 'gu'),
    // National numbers with a trunk 0 (UK 07700 900123, FR 06 12 34 56 78, AU 0412 345 678).
    new RegExp(String.raw`${NB}0\d{1,4}(?:[ .-]?\d{2,4}){1,4}${NA}`, 'gu'),
  ],
  address: [
    new RegExp(String.raw`${NB}\d{1,6}[a-z]?(?:[ \t]+(?:[a-z][a-z.'-]*|\d{1,3}(?:st|nd|rd|th))){1,4}?[ \t]+`
      + String.raw`(?:street|st|avenue|ave|av|boulevard|blvd|road|rd|lane|ln|drive|dr|court|ct|place|pl|terrace|ter|way|parkway|pkwy|highway|hwy|circle|cir|crescent|cres)\.?(?![\p{L}\p{N}_])`, 'giu'),
  ],
  email: [
    /(?<![\p{L}\p{N}._%+-])[a-z0-9][a-z0-9._%+-]{0,63}@(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.){1,8}[a-z]{2,24}(?![\p{L}\p{N}_-])/giu,
  ],
  ssn: [
    new RegExp(String.raw`${NB}(?!000|666|9)\d{3}-(?!00)\d{2}-(?!0000)\d{4}${NA}`, 'gu'),
  ],
  card: [
    new RegExp(String.raw`${NB}[2-6]\d{3}(?:[ -]?\d){9,15}${NA}`, 'gu'),
  ],
});

// Street words that are also everyday English need a Title-case street name ("12 Oak Drive", not "2 hard drive" or
// "2 SSD drive").
const PLAIN_WORD_SUFFIXES = new Set(['road', 'lane', 'drive', 'dr', 'court', 'place', 'pl', 'way', 'circle', 'terrace']);

// Words that show the number is a quantity, not a house number ("5 miles down the road", "3 cats in the way").
const NOT_A_STREET = new Set([
  'a', 'an', 'the', 'in', 'on', 'at', 'to', 'of', 'for', 'from', 'by', 'with', 'into', 'onto', 'off', 'out', 'over',
  'under', 'down', 'up', 'across', 'along', 'around', 'through', 'past', 'near', 'my', 'your', 'his', 'her', 'their',
  'our', 'its', 'this', 'that', 'these', 'those', 'and', 'or', 'but', 'so', 'is', 'are', 'was', 'were', 'be', 'i',
  'you', 'he', 'she', 'we', 'they', 'it', 'more', 'less', 'times', 'minutes', 'mins', 'min', 'hours', 'hrs', 'days',
  'weeks', 'months', 'years', 'miles', 'mi', 'km', 'blocks', 'people', 'guys', 'of', 'than', 'then', 'if', 'not',
]);

const digitsOf = (s) => s.replace(/\D/g, '');

function luhn(digits) {
  let sum = 0;
  let dbl = false;
  for (let i = digits.length - 1; i >= 0; i--) {
    let d = digits.charCodeAt(i) - 48;
    if (dbl) { d *= 2; if (d > 9) d -= 9; }
    sum += d;
    dbl = !dbl;
  }
  return sum % 10 === 0;
}

const VALIDATORS = Object.freeze({
  phone: (m) => {
    const n = digitsOf(m).length;
    if (m.startsWith('+')) return n >= 8 && n <= 15;
    if (m.startsWith('0')) return n >= 10 && n <= 11;
    return n === 10 || n === 11;
  },
  address: (m) => {
    const words = m.trim().split(/[ \t]+/);
    const suffix = words[words.length - 1].replace(/\.$/, '');
    const name = words.slice(1, -1);
    if (name.length === 0) return false;
    if (name.some((w) => NOT_A_STREET.has(w.toLowerCase().replace(/[.'-]+$/, '')))) return false;
    if (PLAIN_WORD_SUFFIXES.has(suffix.toLowerCase())) return name.some((w) => /^(?:\p{Lu}\p{Ll}|\d)/u.test(w));
    return true;
  },
  email: () => true,
  ssn: () => true,
  card: (m) => {
    const d = digitsOf(m);
    if (d.length < 13 || d.length > 19) return false;
    // Bare digits: only the common 15-16 digit lengths (17-19 bare digits are usually Discord ids).
    if (d.length === m.length && d.length !== 15 && d.length !== 16) return false;
    return luhn(d);
  },
});

/** NFKC (fullwidth digits become ASCII), invisible characters removed, capped. */
export function personalInfoText(text) {
  const s = String(text ?? '').normalize('NFKC').replace(INVISIBLE_RE, '');
  return s.length > PERSONAL_INFO_INPUT_CAP ? s.slice(0, PERSONAL_INFO_INPUT_CAP) : s;
}

/** Every validated hit in already normalised text: { kind, start, end }, sorted by start, overlaps removed. */
function scan(t) {
  const skips = [...t.matchAll(SKIP_RE)].map((m) => [m.index, m.index + m[0].length]);
  const inSkip = (s, e) => skips.some(([a, b]) => s < b && e > a);
  const hits = [];
  for (const kind of PERSONAL_INFO_KINDS) {
    for (const re of PATTERNS[kind]) {
      for (const m of t.matchAll(re)) {
        const start = m.index;
        const end = start + m[0].length;
        if (inSkip(start, end) || !VALIDATORS[kind](m[0])) continue;
        hits.push({ kind, start, end });
      }
    }
  }
  // Earliest first; at the same start the longer hit wins; anything overlapping a kept hit is dropped.
  hits.sort((a, b) => a.start - b.start || (b.end - b.start) - (a.end - a.start));
  const out = [];
  for (const h of hits) if (out.length === 0 || h.start >= out[out.length - 1].end) out.push(h);
  return out;
}

/**
 * The kinds of personal information in `text`, in PERSONAL_INFO_KINDS order (empty when none).
 * @param {string} text
 * @returns {string[]}
 */
export function findPersonalInfo(text) {
  const t = personalInfoText(text);
  if (!/[\d@]/.test(t)) return [];
  const found = new Set(scan(t).map((h) => h.kind));
  return PERSONAL_INFO_KINDS.filter((k) => found.has(k));
}

/** "phone number, email address" for the automod detail line (never the values themselves). */
export const describePersonalInfo = (kinds) => kinds.map((k) => PERSONAL_INFO_LABELS[k] ?? k).join(', ');

/**
 * The text with every hit replaced by "[phone number]", "[email address]" and so on, for logs. Text without a hit
 * comes back unchanged (not normalised).
 * @param {string} text
 * @returns {string}
 */
export function redactPersonalInfo(text) {
  const raw = String(text ?? '');
  const t = personalInfoText(raw);
  if (!/[\d@]/.test(t)) return raw;
  const hits = scan(t);
  if (hits.length === 0) return raw;
  let out = '';
  let last = 0;
  for (const h of hits) {
    out += `${t.slice(last, h.start)}[${PERSONAL_INFO_LABELS[h.kind]}]`;
    last = h.end;
  }
  return out + t.slice(last);
}
