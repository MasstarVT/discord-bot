// ── User-supplied regular expressions ──────────────────────────────────────────
// Validation rejects the shapes that cause catastrophic backtracking, and execution runs
// inside a vm context with a hard timeout, so one bad pattern can never freeze the bot.
// Everything is synchronous and nothing here throws. The vm context and the caches are
// created lazily, never at import time.
import vm from 'node:vm';
import logger from './logger.js';
import { clampText } from './text.js';

export const MAX_PATTERN_LENGTH = 200;
export const DEFAULT_INPUT_CAP = 2000;

const ALLOWED_FLAGS = 'imsu';
const COMPILE_CACHE_MAX = 500;
const WORD_CACHE_MAX = 1000;
const QUARANTINE_MAX = 1000;
const QUARANTINE_TTL_MS = 10 * 60_000;

// ── Validation ─────────────────────────────────────────────────────────────────

function dedupeFlags(flags) {
  return [...new Set(typeof flags === 'string' ? flags : '')].join('');
}

// Walks the pattern once, tracking the group stack. Returns 'backref', 'nested' or null.
// A group that already repeats or alternates, and is then repeated by a large quantifier,
// is the classic exponential shape: (a+)+  (a|aa)*  (\w+\s?)*  (.*a){5}
function scanShape(p) {
  const stack = [];
  let lastClosed = null;
  let i = 0;
  const top = () => stack[stack.length - 1];
  while (i < p.length) {
    const c = p[i];
    if (c === '\\') {
      const n = p[i + 1];
      if ((n >= '1' && n <= '9') || (n === 'k' && p[i + 2] === '<')) return 'backref';
      if ((n === 'p' || n === 'P' || n === 'u') && p[i + 2] === '{') {
        const close = p.indexOf('}', i + 3);
        i = close === -1 ? p.length : close + 1;
      } else {
        i += 2;
      }
      lastClosed = null;
      continue;
    }
    if (c === '[') {
      let j = i + 1;
      if (p[j] === '^') j++;
      while (j < p.length && p[j] !== ']') {
        if (p[j] === '\\') j++;
        j++;
      }
      i = j + 1;
      lastClosed = null;
      continue;
    }
    if (c === '(') {
      stack.push({ q: false, alt: false });
      let prefix = 1;
      if (p[i + 1] === '?') {
        const a = p[i + 2];
        if (a === ':' || a === '=' || a === '!') prefix = 3;
        else if (a === '<' && (p[i + 3] === '=' || p[i + 3] === '!')) prefix = 4;
        else if (a === '<') {
          const close = p.indexOf('>', i + 3);
          prefix = close === -1 ? 3 : close - i + 1;
        }
      }
      i += prefix;
      lastClosed = null;
      continue;
    }
    if (c === ')') {
      const f = stack.pop();
      i++;
      if (f && stack.length) {
        top().q ||= f.q;
        top().alt ||= f.alt;
      }
      lastClosed = f ?? null;
      continue;
    }
    if (c === '|') {
      if (stack.length) top().alt = true;
      i++;
      lastClosed = null;
      continue;
    }
    let qLen = 0;
    let large = false;
    if (c === '*' || c === '+') { qLen = 1; large = true; }
    else if (c === '?') { qLen = 1; }
    else if (c === '{') {
      const m = /^\{(\d+)(,(\d*))?\}/.exec(p.slice(i, i + 30));
      if (m) {
        qLen = m[0].length;
        if (m[2] === undefined) large = Number(m[1]) > 1;
        else if (m[3] === '') large = true;
        else large = Number(m[3]) > 1;
      }
    }
    if (qLen) {
      if (large && lastClosed && (lastClosed.q || lastClosed.alt)) return 'nested';
      if (large && stack.length) top().q = true;
      i += qLen;
      if (p[i] === '?') i++; // lazy suffix
      lastClosed = null;
      continue;
    }
    i++;
    lastClosed = null;
  }
  return null;
}

/**
 * Checks a user-supplied pattern: length, flags, syntax, backreferences and nested repetition.
 * @param {unknown} pattern
 * @param {string} [flags='i'] - only i, m, s and u are allowed (g and y never are)
 * @returns {{ ok: true } | { ok: false, reason: string }}
 */
export function validateRegex(pattern, flags = 'i') {
  try {
    if (typeof pattern !== 'string') return { ok: false, reason: 'Pattern must be text.' };
    if (pattern === '') return { ok: false, reason: 'Pattern is empty.' };
    if (pattern.length > MAX_PATTERN_LENGTH) {
      return { ok: false, reason: `Pattern is ${pattern.length} characters (max ${MAX_PATTERN_LENGTH}).` };
    }
    const fl = dedupeFlags(flags);
    for (const c of fl) {
      if (!ALLOWED_FLAGS.includes(c)) return { ok: false, reason: `Flag "${c}" is not allowed (use i, m, s or u).` };
    }
    try {
      new RegExp(pattern, fl);
    } catch (e) {
      return { ok: false, reason: `Invalid regular expression: ${clampText(e?.message, 150)}` };
    }
    const shape = scanShape(pattern);
    if (shape === 'backref') return { ok: false, reason: 'Backreferences (\\1, \\k<name>) are not allowed.' };
    if (shape === 'nested') {
      return {
        ok: false,
        reason: 'Nested repetition is not allowed (e.g. (a+)+ or (a|aa)*). Rewrite the pattern without repeating a group that already repeats or alternates.',
      };
    }
    return { ok: true };
  } catch {
    return { ok: false, reason: 'Pattern could not be checked.' };
  }
}

/** @type {Map<string, RegExp>|null} */
let compileCache = null;

/**
 * Validates and compiles a user pattern. The result is cached and shared; `g` and `y` are
 * never allowed, so `lastIndex` is never used.
 * @param {unknown} pattern
 * @param {string} [flags='i']
 * @returns {RegExp|null} null when invalid
 */
export function compileSafeRegex(pattern, flags = 'i') {
  try {
    if (!validateRegex(pattern, flags).ok) return null;
    const fl = dedupeFlags(flags);
    const key = `${pattern}\u0000${fl}`;
    compileCache ??= new Map();
    const hit = compileCache.get(key);
    if (hit) return hit;
    const re = new RegExp(pattern, fl);
    if (compileCache.size >= COMPILE_CACHE_MAX) compileCache.delete(compileCache.keys().next().value);
    compileCache.set(key, re);
    return re;
  } catch {
    return null;
  }
}

// ── Sandboxed execution ────────────────────────────────────────────────────────

let sandbox = null; // { ctx, TEST, FIRST }

function ensureVm() {
  if (!sandbox) {
    sandbox = {
      ctx: vm.createContext(Object.create(null)),
      TEST: new vm.Script('re.test(input)'),
      FIRST: new vm.Script('(() => { for (let i = 0; i < res.length; i++) if (res[i] && res[i].test(input)) return i; return -1; })()'),
    };
  }
  return sandbox;
}

/** @type {Map<string, number>} `${source}/${flags}` → expiresAt; insertion order = age order */
const quarantined = new Map();

const quarantineKey = (re) => `${re.source}/${re.flags}`;

function quarantine(re, ms) {
  const key = quarantineKey(re);
  quarantined.delete(key);
  quarantined.set(key, Date.now() + QUARANTINE_TTL_MS);
  while (quarantined.size > QUARANTINE_MAX) quarantined.delete(quarantined.keys().next().value);
  logger.warn(`Regex /${clampText(re.source, 100)}/${re.flags} exceeded ${ms} ms and is quarantined for 10 minutes`);
}

/**
 * True while a regex that timed out is being skipped.
 * @param {RegExp} regex
 * @returns {boolean}
 */
export function isQuarantined(regex) {
  if (!(regex instanceof RegExp)) return false;
  const key = quarantineKey(regex);
  const expiresAt = quarantined.get(key);
  if (expiresAt === undefined) return false;
  if (expiresAt <= Date.now()) {
    quarantined.delete(key);
    return false;
  }
  return true;
}

/**
 * Tests a regex against `input` with a hard timeout. A regex that times out is quarantined
 * for 10 minutes and counts as no match.
 * @param {RegExp} regex
 * @param {unknown} input - cut to `inputCap` characters first
 * @param {{ timeoutMs?: number, inputCap?: number }} [opts]
 * @returns {boolean}
 */
export function safeRegexTest(regex, input, { timeoutMs = 25, inputCap = DEFAULT_INPUT_CAP } = {}) {
  if (!(regex instanceof RegExp)) return false;
  if (isQuarantined(regex)) return false;
  let sb = null;
  try {
    const str = String(input ?? '').slice(0, inputCap);
    regex.lastIndex = 0; // an unvalidated regex may carry g or y
    sb = ensureVm();
    sb.ctx.re = regex;
    sb.ctx.input = str;
    return sb.TEST.runInContext(sb.ctx, { timeout: timeoutMs }) === true;
  } catch (err) {
    if (err?.code === 'ERR_SCRIPT_EXECUTION_TIMEOUT') quarantine(regex, timeoutMs);
    else logger.warn(`safeRegexTest failed: ${err?.message}`);
    return false;
  } finally {
    if (sb) { sb.ctx.re = null; sb.ctx.input = null; }
  }
}

/**
 * Tests several regexes against one input and returns the index of the first (in array order)
 * that matches. When the batch times out, every regex in it is quarantined and then retried on
 * its own, so the innocent ones are released.
 * @param {RegExp[]} regexes
 * @param {unknown} input
 * @param {{ timeoutMs?: number, inputCap?: number }} [opts]
 * @returns {number} index or -1
 */
export function safeRegexFirstMatch(regexes, input, { timeoutMs = 50, inputCap = DEFAULT_INPUT_CAP } = {}) {
  const list = Array.isArray(regexes) ? regexes : [];
  const res = list.map((r) => {
    if (r instanceof RegExp && !isQuarantined(r)) { r.lastIndex = 0; return r; }
    return null;
  });
  if (res.every((r) => r === null)) return -1;
  let sb = null;
  let str = '';
  try {
    str = String(input ?? '').slice(0, inputCap);
    sb = ensureVm();
    sb.ctx.res = res;
    sb.ctx.input = str;
    const idx = sb.FIRST.runInContext(sb.ctx, { timeout: timeoutMs });
    return Number.isInteger(idx) ? idx : -1;
  } catch (err) {
    if (err?.code !== 'ERR_SCRIPT_EXECUTION_TIMEOUT') {
      logger.warn(`safeRegexFirstMatch failed: ${err?.message}`);
      return -1;
    }
    for (const r of res) if (r) quarantine(r, timeoutMs);
    let first = -1;
    res.forEach((r, i) => {
      if (!r) return;
      quarantined.delete(quarantineKey(r));
      if (safeRegexTest(r, str, { timeoutMs: 25, inputCap }) && first === -1) first = i;
    });
    return first;
  } finally {
    if (sb) { sb.ctx.res = null; sb.ctx.input = null; }
  }
}

// ── Escaping and whole-word matching ───────────────────────────────────────────

/**
 * Escapes regex syntax characters. `-` and `/` are not escaped: `\-` is a SyntaxError under
 * the `u` flag, and containsWord and automod build `iu` patterns from this output.
 * @param {unknown} str
 * @returns {string}
 */
export function escapeRegex(str) {
  return String(str ?? '').replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** @type {Map<string, RegExp>|null} */
let wordCache = null;

/**
 * Whole-word match that is Unicode-aware, so 'cunt' does not match inside 'Scunthorpe' (R034).
 * Safe by construction: the pattern is linear and contains no user regex syntax.
 * @param {unknown} content
 * @param {unknown} word
 * @param {{ caseSensitive?: boolean }} [opts]
 * @returns {boolean}
 */
export function containsWord(content, word, { caseSensitive = false } = {}) {
  try {
    const w = String(word ?? '');
    if (!w) return false;
    const flags = caseSensitive ? 'u' : 'iu';
    const key = `${flags}\u0000${w}`;
    wordCache ??= new Map();
    let re = wordCache.get(key);
    if (!re) {
      re = new RegExp(`(?<![\\p{L}\\p{N}_])${escapeRegex(w)}(?![\\p{L}\\p{N}_])`, flags);
      if (wordCache.size >= WORD_CACHE_MAX) wordCache.delete(wordCache.keys().next().value);
      wordCache.set(key, re);
    }
    return re.test(String(content ?? ''));
  } catch {
    return false;
  }
}
