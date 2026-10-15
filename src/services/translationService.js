// Translation service (pkg-translation-fixes): language registry and code normalisation, the Google
// (keyless) and LibreTranslate adapters, a circuit breaker, request sizing, a short-text result
// cache, the channel-configuration cache and the auto-translate engine used by the
// `translation-fixes` messageCreate hook. Nothing here throws into a caller except translateText,
// detectLanguage and getGuildTranslateConfig, which throw TranslationError / the Postgres error.
import { createHash } from 'node:crypto';
import { EmbedBuilder } from 'discord.js';
import { Prisma } from '@prisma/client';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import { STAT_VIA, recordTranslation } from './translateStatsService.js';
import { COLORS, LIMITS } from '../config/constants.js';
import logger from '../utils/logger.js';
import { clampText } from '../utils/text.js';
import { applyGuildColor, errorEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { checkChannelUsable } from '../utils/interactionHelpers.js';
import { wasDeletedByBot } from '../utils/deletionTracker.js';
import { createThrottle } from '../utils/async.js';
import { discordTimestamp } from '../utils/time.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const GOOGLE_ENDPOINT = 'https://translate.googleapis.com/translate_a/single';
// Only the `gtx` client is used. A 429 trips the circuit breaker (pause) instead of switching to another
// client value: rotating identities to get around Google's rate limit would just extend the block.
export const GOOGLE_CLIENTS = Object.freeze(['gtx']);
export const GOOGLE_CLIENT_BLOCK_MS = 10 * 60_000;
export const GOOGLE_TIMEOUT_MS = 8_000;
export const LT_TIMEOUT_MS = 15_000;
export const LT_LANGUAGES_TIMEOUT_MS = 5_000;
export const LT_LANGUAGES_TTL_MS = 6 * 60 * 60_000;     // re-fetch the server list every 6 h
export const LT_LANGUAGES_RETRY_MS = 5 * 60_000;        // after a failed fetch
export const PAUSE_MS = Object.freeze({ google: 30 * 60_000, libretranslate: 60_000 });
export const PAUSE_MAX_MS = 6 * 60 * 60_000;            // cap for Retry-After
export const MAX_Q_ENCODED_BYTES = 6_000;               // Google GET: q stays <= 6,000 URL-encoded bytes (URL < 6.2 KB)
export const MAX_POST_Q_BYTES = 24_000;                 // above the GET limit, one POST (form body) up to this size
export const AUTO_INPUT_MAX_CODEPOINTS = 1_000;
export const DETECT_SAMPLE_CODEPOINTS = 500;
export const TRANSLATE_MAX_CHUNKS = 4;                  // only used above MAX_POST_Q_BYTES, which /translate (1,500 code points) never reaches
export const AUTO_PER_TARGET_MAX = 1_000;
export const AUTO_DESCRIPTION_BUDGET = 4_000;
export const AUTO_DEADLINE_MS = 25_000;
export const AUTO_MAX_INFLIGHT = 6;
export const RESULT_CACHE_TTL_S = 3_600;
export const RESULT_CACHE_MAX_CODEPOINTS = 300;
export const CHANNELS_CACHE_TTL_S = 300;
export const CHANNELS_MEMO_TTL_MS = 30_000;
export const EMBED_PART_MAX = 4_000;
export const EMBED_MAX_PARTS = 3;

const MAX_TARGETS = 5;
const CHANNEL_MEMO_MAX = 10_000;

// ── Language registry (R176, R177, R179, R286, R287) ──────────────────────────

export const LANGUAGES = Object.freeze({
  af: 'Afrikaans',
  ar: 'Arabic',
  az: 'Azerbaijani',
  be: 'Belarusian',
  bg: 'Bulgarian',
  bn: 'Bengali',
  bs: 'Bosnian',
  ca: 'Catalan',
  cs: 'Czech',
  cy: 'Welsh',
  da: 'Danish',
  de: 'German',
  el: 'Greek',
  en: 'English',
  eo: 'Esperanto',
  es: 'Spanish',
  et: 'Estonian',
  fa: 'Persian',
  fi: 'Finnish',
  fr: 'French',
  ga: 'Irish',
  gl: 'Galician',
  gu: 'Gujarati',
  he: 'Hebrew',
  hi: 'Hindi',
  hr: 'Croatian',
  hu: 'Hungarian',
  hy: 'Armenian',
  id: 'Indonesian',
  is: 'Icelandic',
  it: 'Italian',
  ja: 'Japanese',
  ka: 'Georgian',
  kk: 'Kazakh',
  km: 'Khmer',
  ko: 'Korean',
  lt: 'Lithuanian',
  lv: 'Latvian',
  mk: 'Macedonian',
  ml: 'Malayalam',
  mn: 'Mongolian',
  mr: 'Marathi',
  ms: 'Malay',
  mt: 'Maltese',
  my: 'Myanmar (Burmese)',
  ne: 'Nepali',
  nl: 'Dutch',
  no: 'Norwegian',
  pa: 'Punjabi',
  pl: 'Polish',
  pt: 'Portuguese',
  ro: 'Romanian',
  ru: 'Russian',
  si: 'Sinhala',
  sk: 'Slovak',
  sl: 'Slovenian',
  sq: 'Albanian',
  sr: 'Serbian',
  sv: 'Swedish',
  sw: 'Swahili',
  ta: 'Tamil',
  te: 'Telugu',
  th: 'Thai',
  tl: 'Filipino',
  tr: 'Turkish',
  uk: 'Ukrainian',
  ur: 'Urdu',
  uz: 'Uzbek',
  vi: 'Vietnamese',
  xh: 'Xhosa',
  yi: 'Yiddish',
  zh: 'Chinese (Simplified)',
  'zh-tw': 'Chinese (Traditional)',
  zu: 'Zulu',
});

// Language code -> ISO 3166-1 alpha-2 country code for the flag emoji.
const LANG_FLAGS = {
  af: 'ZA', ar: 'SA', az: 'AZ', be: 'BY', bg: 'BG', bn: 'BD', bs: 'BA',
  ca: 'ES', cs: 'CZ', cy: 'GB', da: 'DK', de: 'DE', el: 'GR', en: 'GB',
  eo: null, es: 'ES', et: 'EE', fa: 'IR', fi: 'FI', fr: 'FR', ga: 'IE',
  gl: 'ES', gu: 'IN', he: 'IL', hi: 'IN', hr: 'HR', hu: 'HU', hy: 'AM',
  id: 'ID', is: 'IS', it: 'IT', ja: 'JP', ka: 'GE', kk: 'KZ', km: 'KH',
  ko: 'KR', lt: 'LT', lv: 'LV', mk: 'MK', ml: 'IN', mn: 'MN', mr: 'IN',
  ms: 'MY', mt: 'MT', my: 'MM', ne: 'NP', nl: 'NL', no: 'NO', pa: 'IN',
  pl: 'PL', pt: 'PT', ro: 'RO', ru: 'RU', si: 'LK', sk: 'SK', sl: 'SI',
  sq: 'AL', sr: 'RS', sv: 'SE', sw: 'TZ', ta: 'IN', te: 'IN', th: 'TH',
  tl: 'PH', tr: 'TR', uk: 'UA', ur: 'PK', uz: 'UZ', vi: 'VN', xh: 'ZA',
  yi: 'IL', zh: 'CN', 'zh-tw': 'TW', zu: 'ZA',
};

/** @param {string} langCode canonical code @returns {string} flag emoji, or a globe when unknown */
export function langFlag(langCode) {
  const country = Object.hasOwn(LANG_FLAGS, langCode) ? LANG_FLAGS[langCode] : null;
  if (!country) return '🌐';
  return [...country.toUpperCase()]
    .map((c) => String.fromCodePoint(0x1F1E6 + c.charCodeAt(0) - 65))
    .join('');
}

// Legacy and regional codes -> canonical codes. Google reports Hebrew as `iw`; LibreTranslate says `nb`.
export const CODE_ALIASES = Object.freeze({
  iw: 'he', ji: 'yi', jw: 'jv', in: 'id',
  'zh-cn': 'zh', 'zh-sg': 'zh', 'zh-hans': 'zh',
  'zh-hk': 'zh-tw', 'zh-mo': 'zh-tw', 'zh-hant': 'zh-tw', zt: 'zh-tw',
  nb: 'no', nn: 'no', fil: 'tl', pb: 'pt', 'pt-br': 'pt', 'pt-pt': 'pt',
});
const NAME_ALIASES = Object.freeze({
  chinese: 'zh', 'simplified chinese': 'zh', 'traditional chinese': 'zh-tw',
  burmese: 'my', myanmar: 'my', tagalog: 'tl', farsi: 'fa',
  'norwegian bokmål': 'no', 'norwegian bokmal': 'no',
});
const DID_YOU_MEAN = Object.freeze({
  jp: 'ja', kr: 'ko', cn: 'zh', tw: 'zh-tw', hk: 'zh-tw', gr: 'el', dk: 'da',
  se: 'sv', ua: 'uk', cz: 'cs', il: 'he', vn: 'vi', br: 'pt', ir: 'fa', kz: 'kk', gb: 'en', us: 'en',
  rs: 'sr', pk: 'ur', by: 'be', al: 'sq', ge: 'ka', ee: 'et', ch: 'de', at: 'de',
});

const has = (obj, key) => Object.hasOwn(obj, key);

/**
 * Canonical language code for any code a provider or user may produce, or null.
 * Keeps Traditional Chinese (`zh-tw`) apart from Simplified (`zh`); the result may be a code that is
 * not in LANGUAGES (e.g. `ceb`).
 * @param {unknown} code
 * @returns {string|null}
 */
export function normalizeLangCode(code) {
  if (typeof code !== 'string') return null;
  const c = code.trim().toLowerCase().replace(/_/g, '-');
  if (!/^[a-z]{2,3}(?:-[a-z0-9]{2,8}){0,3}$/.test(c)) return null; // also rejects "auto"
  if (c === 'und') return null;
  if (has(CODE_ALIASES, c)) return CODE_ALIASES[c];
  if (has(LANGUAGES, c)) return c;
  const [base, ...subs] = c.split('-');
  if (base === 'zh') return subs.some((s) => ['hant', 'tw', 'hk', 'mo'].includes(s)) ? 'zh-tw' : 'zh';
  if (has(CODE_ALIASES, base)) return CODE_ALIASES[base];
  return base;
}

let displayNames = null;

/** @param {string|null} code @returns {string} English name; the raw code when unknown, 'Unknown' for null */
export function languageName(code) {
  if (code == null || code === '') return 'Unknown';
  if (has(LANGUAGES, code)) return LANGUAGES[code];
  try {
    displayNames ??= new Intl.DisplayNames(['en'], { type: 'language' });
    const name = displayNames.of(code);
    return name && name !== code ? name : code;
  } catch {
    return code;
  }
}

/**
 * A LANGUAGES key for a code ("ZH-TW", "iw", "zh-Hant") or an English name ("Japanese"), else null.
 * @param {unknown} input
 * @returns {string|null}
 */
export function resolveLanguageInput(input) {
  const n = normalizeLangCode(input);
  if (n && has(LANGUAGES, n)) return n;
  const name = String(input ?? '').trim().toLowerCase();
  if (!name) return null;
  for (const [code, label] of Object.entries(LANGUAGES)) if (label.toLowerCase() === name) return code;
  return has(NAME_ALIASES, name) ? NAME_ALIASES[name] : null;
}

/** @param {unknown} input @returns {string|null} a likely intended code for a country code such as "jp" */
export function suggestLanguage(input) {
  const key = String(input ?? '').trim().toLowerCase();
  return has(DID_YOU_MEAN, key) ? DID_YOU_MEAN[key] : null;
}

/**
 * Canonicalises and de-duplicates a list of user inputs (R211/R212).
 * @param {unknown[]} inputs
 * @returns {{ valid: string[], invalid: string[] }} `invalid` holds the raw input of entries that are not a known language
 */
export function normalizeTargetList(inputs) {
  const valid = [];
  const invalid = [];
  for (const input of Array.isArray(inputs) ? inputs : []) {
    const code = resolveLanguageInput(input);
    if (!code) invalid.push(input);
    else if (!valid.includes(code)) valid.push(code); // a repeat is dropped silently (R211/R212)
  }
  return { valid, invalid };
}

/** @param {string} code canonical @returns {string} the code Google expects */
export function toGoogleCode(code) {
  return { zh: 'zh-CN', 'zh-tw': 'zh-TW' }[code] ?? code;
}

// ── Provider configuration (read lazily, so importing has no side effects) ────

let envConfig = null;
let override = {};

function config() {
  if (!envConfig) {
    const raw = String(process.env.TRANSLATE_PROVIDER ?? '').trim();
    let provider = 'google';
    if (raw && raw.toLowerCase() === 'libretranslate') provider = 'libretranslate';
    else if (raw && raw.toLowerCase() !== 'google') logger.warn(`TRANSLATE_PROVIDER="${raw}" is not supported; using google.`);
    envConfig = {
      provider,
      ltUrl: String(process.env.LIBRETRANSLATE_URL || 'http://localhost:5000').replace(/\/+$/, ''),
      ltKey: process.env.LIBRETRANSLATE_API_KEY || '',
      resultCache: true,
    };
  }
  return { ...envConfig, ...override };
}

/** @returns {'google'|'libretranslate'} */
export function providerName() {
  return config().provider;
}

/** @returns {string} */
export function providerLabel() {
  return providerName() === 'libretranslate' ? 'LibreTranslate' : 'Google Translate (keyless)';
}

// ── Mutable state (per shard; reset by __testing.reset) ───────────────────────

let pausedUntil = 0;
let lastPauseSync = 0;
let ltSupport = { codes: new Set(), rawCodes: new Set(), loadedAt: 0, failedAt: 0, inflight: null, known: false };
let runtimeUnsupported = new Set();
let channelMemo = new Map();
let channelGen = new Map(); // per guild: bumped by every invalidation, so a read that began earlier never memoises
let autoInflight = 0;
const googleBlockedUntil = new Map();
let langCache = { key: '', value: LANGUAGES };

const makeThrottles = () => ({
  tenMin: createThrottle(10 * 60_000),
  hour: createThrottle(60 * 60_000),
  minute: createThrottle(60_000),
});
let throttles = makeThrottles();

// ── Errors ────────────────────────────────────────────────────────────────────

export class TranslationError extends Error {
  /**
   * @param {'PAUSED'|'RATE_LIMITED'|'UNSUPPORTED_LANGUAGE'|'EMPTY'|'TOO_LONG'|'TIMEOUT'|'NETWORK'|'HTTP_ERROR'|'BAD_RESPONSE'} code
   * @param {string} [message]
   * @param {{ status?: number|null, retryAfterMs?: number|null, lang?: string|null, cause?: unknown }} [extra]
   */
  constructor(code, message = code, { status = null, retryAfterMs = null, lang = null, cause } = {}) {
    super(message, cause === undefined ? undefined : { cause });
    this.name = 'TranslationError';
    this.code = code;
    this.status = status;
    this.retryAfterMs = retryAfterMs;
    this.lang = lang;
  }
}

async function doFetch(url, init) {
  try {
    return await fetch(url, init); // looked up at call time so tests can stub it
  } catch (err) {
    if (err?.name === 'AbortError' || err?.name === 'TimeoutError') {
      throw new TranslationError('TIMEOUT', 'The request timed out', { cause: err });
    }
    throw new TranslationError('NETWORK', err?.message ?? 'Network error', { cause: err });
  }
}

// ── Circuit breaker (R255) ────────────────────────────────────────────────────

/** Sync: used by the hook filter and by the commands before they defer. */
export function translationPaused() {
  return Date.now() < pausedUntil;
}

export function pauseRemainingMs() {
  return Math.max(0, pausedUntil - Date.now());
}

async function tripBreaker(res) {
  const provider = providerName();
  let pauseMs = PAUSE_MS[provider];
  const retryAfter = Number(res?.headers?.get?.('retry-after'));
  if (Number.isFinite(retryAfter) && retryAfter * 1000 > pauseMs) pauseMs = Math.min(retryAfter * 1000, PAUSE_MAX_MS);
  const wasPaused = translationPaused();
  const until = Date.now() + pauseMs;
  if (until > pausedUntil) pausedUntil = until;
  if (!wasPaused) {
    logger.warn(`translationService: ${provider} returned 429; pausing translation for ${Math.ceil(pauseMs / 60_000)} min`);
  }
  // Fail-open: when Redis is down the in-process pause still holds; the key only shares it across shards.
  await cache.set(`translate:${provider}:pausedUntil`, pausedUntil, Math.ceil(pauseMs / 1000));
  throw new TranslationError('RATE_LIMITED', 'Rate limited', { status: 429, retryAfterMs: pauseMs });
}

async function assertNotPaused() {
  const now = Date.now();
  if (now - lastPauseSync >= 60_000) {
    lastPauseSync = now;
    const shared = await cache.get(`translate:${providerName()}:pausedUntil`);
    if (typeof shared === 'number' && Number.isFinite(shared) && shared > pausedUntil) pausedUntil = shared;
  }
  if (translationPaused()) {
    throw new TranslationError('PAUSED', 'Translation is paused after a rate limit', { retryAfterMs: pauseRemainingMs() });
  }
}

// ── Providers ─────────────────────────────────────────────────────────────────

const GOOGLE_HEADERS = { 'User-Agent': 'Mozilla/5.0' };

/**
 * One translation through the keyless Google endpoint. GET while `q` is at most MAX_Q_ENCODED_BYTES
 * once encoded; one POST (form body) above that, so long CJK text never meets a URL limit (R178).
 * A client that answers 429 is skipped for a while and the next one is tried; only when every client is
 * limited does the circuit breaker trip.
 * @returns {Promise<{ text: string, detected: string|null }>}
 */
async function googleTranslate(text, target, source) {
  const q = encodeURIComponent(text);
  if (q.length > MAX_POST_Q_BYTES) throw new TranslationError('TOO_LONG', 'Text is too long for one request');
  const post = q.length > MAX_Q_ENCODED_BYTES;
  const sl = encodeURIComponent(source === 'auto' ? 'auto' : toGoogleCode(source));
  const tl = encodeURIComponent(toGoogleCode(target));

  const now = Date.now();
  const open = GOOGLE_CLIENTS.filter((c) => (googleBlockedUntil.get(c) ?? 0) <= now);
  const order = open.length ? open : GOOGLE_CLIENTS;
  let limited = null;
  let rejected = null;
  for (let i = 0; i < order.length; i++) {
    const client = order[i];
    const base = `${GOOGLE_ENDPOINT}?client=${client}&sl=${sl}&tl=${tl}&dt=t`;
    const res = post
      ? await doFetch(base, {
        method: 'POST',
        headers: { ...GOOGLE_HEADERS, 'Content-Type': 'application/x-www-form-urlencoded;charset=UTF-8' },
        body: `q=${q}`,
        signal: AbortSignal.timeout(GOOGLE_TIMEOUT_MS),
      })
      : await doFetch(`${base}&q=${q}`, { headers: GOOGLE_HEADERS, signal: AbortSignal.timeout(GOOGLE_TIMEOUT_MS) });

    if (res.status === 429) {
      googleBlockedUntil.set(client, Date.now() + GOOGLE_CLIENT_BLOCK_MS);
      limited = res;
      if (i < order.length - 1 && throttles.tenMin(`google429:${client}`)) {
        logger.warn(`translationService: google client "${client}" returned 429; trying "${order[i + 1]}"`);
      }
      continue;
    }
    if (!res.ok) {
      // A POST some client refuses (4xx) is retried with the next client; everything else is final.
      if (post && res.status >= 400 && res.status < 500 && i < order.length - 1) { rejected = res; continue; }
      throw new TranslationError('HTTP_ERROR', `Google returned ${res.status}`, { status: res.status });
    }
    let data;
    try {
      data = await res.json();
    } catch (err) {
      throw new TranslationError('BAD_RESPONSE', 'Google returned invalid JSON', { cause: err });
    }
    if (!Array.isArray(data) || !Array.isArray(data[0])) throw new TranslationError('BAD_RESPONSE', 'Unexpected Google response');
    googleBlockedUntil.delete(client);
    return {
      text: data[0].map((seg) => seg?.[0] ?? '').join(''),
      detected: normalizeLangCode(typeof data[2] === 'string' ? data[2] : null),
    };
  }
  if (limited) await tripBreaker(limited); // throws RATE_LIMITED
  throw new TranslationError('HTTP_ERROR', `Google returned ${rejected?.status ?? 'an error'}`, { status: rejected?.status ?? null });
}

/**
 * LibreTranslate server code for a canonical code (no -> nb, zh -> zh-Hans, zh-tw -> zh-Hant ...),
 * chosen from the server's own list when it is known.
 * @param {string} code canonical
 * @param {Set<string>} [rawCodes] lower-case server codes (default: the loaded list)
 * @returns {string}
 */
export function toLtCode(code, rawCodes = ltSupport.rawCodes) {
  const candidates = {
    zh: ['zh-Hans', 'zh'], 'zh-tw': ['zh-Hant', 'zt'], no: ['nb', 'no'], he: ['he', 'iw'], tl: ['tl', 'fil'], pt: ['pt', 'pt-BR'],
  }[code] ?? [code];
  if (rawCodes && rawCodes.size) {
    const hit = candidates.find((c) => rawCodes.has(c.toLowerCase()));
    if (hit) return hit;
  }
  return candidates[0];
}

async function ltPost(path, body) {
  const { ltUrl, ltKey } = config();
  const res = await doFetch(`${ltUrl}${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(ltKey ? { ...body, api_key: ltKey } : body),
    signal: AbortSignal.timeout(LT_TIMEOUT_MS),
  });
  if (res.status === 429) await tripBreaker(res);
  return res;
}

const LT_UNSUPPORTED_RE = /not supported|unsupported|invalid (source|target)? ?language/i;

async function ltTranslate(text, target, source) {
  const res = await ltPost('/translate', {
    q: text,
    source: source === 'auto' ? 'auto' : toLtCode(source),
    target: toLtCode(target),
    format: 'text',
  });
  if (!res.ok) {
    if (res.status === 400 || res.status === 403) {
      const body = await res.json().catch(() => null);
      const message = String(body?.error ?? '');
      if (LT_UNSUPPORTED_RE.test(message)) {
        const namesSource = source !== 'auto' && /source/i.test(message) && !/target/i.test(message);
        const lang = namesSource ? source : target;
        runtimeUnsupported.add(lang);
        throw new TranslationError('UNSUPPORTED_LANGUAGE', message, { status: res.status, lang });
      }
    }
    throw new TranslationError('HTTP_ERROR', `LibreTranslate returned ${res.status}`, { status: res.status });
  }
  let data;
  try {
    data = await res.json();
  } catch (err) {
    throw new TranslationError('BAD_RESPONSE', 'LibreTranslate returned invalid JSON', { cause: err });
  }
  if (!data || typeof data !== 'object') throw new TranslationError('BAD_RESPONSE', 'Unexpected LibreTranslate response');
  return {
    text: typeof data.translatedText === 'string' ? data.translatedText : '',
    detected: source === 'auto' ? normalizeLangCode(data.detectedLanguage?.language ?? null) : source,
  };
}

async function ltDetect(text) {
  const res = await ltPost('/detect', { q: text });
  if (!res.ok) throw new TranslationError('HTTP_ERROR', `LibreTranslate returned ${res.status}`, { status: res.status });
  let data;
  try {
    data = await res.json();
  } catch (err) {
    throw new TranslationError('BAD_RESPONSE', 'LibreTranslate returned invalid JSON', { cause: err });
  }
  const best = Array.isArray(data) ? data[0] : null;
  return normalizeLangCode(best?.language ?? null);
}

/**
 * The language codes a LibreTranslate server supports (canonical). Google: null at once (everything in
 * LANGUAGES works). Never throws; null also means "could not load" (retried after LT_LANGUAGES_RETRY_MS).
 * @param {{ force?: boolean }} [opts]
 * @returns {Promise<Set<string>|null>}
 */
export async function loadLanguageSupport({ force = false } = {}) {
  if (providerName() !== 'libretranslate') return null;
  const now = Date.now();
  if (!force) {
    if (ltSupport.known && now - ltSupport.loadedAt < LT_LANGUAGES_TTL_MS) return ltSupport.codes;
    // A recent failure is not retried at once; a stale but known list keeps serving meanwhile.
    if (ltSupport.failedAt && now - ltSupport.failedAt < LT_LANGUAGES_RETRY_MS) return ltSupport.known ? ltSupport.codes : null;
  }
  if (ltSupport.inflight) return ltSupport.inflight;
  const state = ltSupport;
  const promise = (async () => {
    try {
      const res = await doFetch(`${config().ltUrl}/languages`, { signal: AbortSignal.timeout(LT_LANGUAGES_TIMEOUT_MS) });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const entries = await res.json();
      if (!Array.isArray(entries)) throw new Error('unexpected response');
      const rawCodes = new Set(entries.map((e) => String(e?.code ?? '').toLowerCase()).filter(Boolean));
      const codes = new Set(entries.map((e) => normalizeLangCode(e?.code)).filter((c) => c && has(LANGUAGES, c)));
      if (state === ltSupport) Object.assign(ltSupport, { codes, rawCodes, loadedAt: Date.now(), failedAt: 0, known: true });
      return codes;
    } catch (err) {
      if (state === ltSupport) ltSupport.failedAt = Date.now();
      if (throttles.tenMin('lt-languages')) {
        logger.warn(`translationService: could not load the LibreTranslate language list: ${err?.message ?? err}`);
      }
      return null;
    } finally {
      if (state === ltSupport) ltSupport.inflight = null;
    }
  })();
  ltSupport.inflight = promise;
  return promise;
}

/** Fire-and-forget loadLanguageSupport (autocomplete). */
export function warmLanguageSupport() {
  loadLanguageSupport().catch(() => {});
}

/** @param {string} code canonical @returns {true|false|null} null = unknown */
export function isSupportedTarget(code) {
  if (providerName() !== 'libretranslate') return true;
  if (runtimeUnsupported.has(code)) return false;
  return ltSupport.known ? ltSupport.codes.has(code) : null;
}

/** @returns {Readonly<Record<string, string>>} the languages the active backend offers (sync) */
export function getLanguages() {
  if (providerName() !== 'libretranslate' || !ltSupport.known) return LANGUAGES;
  const key = `${ltSupport.loadedAt}:${runtimeUnsupported.size}`;
  if (langCache.key !== key) {
    langCache = {
      key,
      value: Object.freeze(Object.fromEntries(Object.entries(LANGUAGES).filter(([code]) => isSupportedTarget(code) !== false))),
    };
  }
  return langCache.value;
}

/**
 * Autocomplete choices for a language option: exact code, code prefix, name prefix, name substring.
 * @param {unknown} query
 * @returns {Array<{ name: string, value: string }>}
 */
export function languageChoices(query) {
  const q = String(query ?? '').trim().toLowerCase();
  const scored = [];
  for (const [code, name] of Object.entries(getLanguages())) {
    const lower = name.toLowerCase();
    let score = -1;
    if (!q) score = 2;
    else if (code === q || (has(CODE_ALIASES, q) && CODE_ALIASES[q] === code)) score = 0;
    else if (code.startsWith(q)) score = 1;
    else if (lower.startsWith(q)) score = 2;
    else if (lower.includes(q)) score = 3;
    if (score >= 0) scored.push({ score, name, code });
  }
  scored.sort((a, b) => a.score - b.score || a.name.localeCompare(b.name));
  return scored.slice(0, 25).map(({ name, code }) => ({ name: `${name} (${code})`, value: code }));
}

// ── Text preparation (R011, R012, R172, R173, R178, R288) ─────────────────────
// Every regex below is linear: no nested quantifiers and no unbounded alternation.

const LINK_TOKEN = /^<?https?:\/\/[^\s<>]+>?$/i;

/**
 * True when the message is nothing but links (R011/R012: replaces a backtracking regex that ran on
 * every guild message). Splits on whitespace and tests each word.
 * @param {unknown} content
 */
export function isLinkOnly(content) {
  const t = String(content ?? '').trim();
  if (!t) return false;
  return t.split(/\s+/).every((w) => LINK_TOKEN.test(w));
}

const EMOJI_RESIDUE = /[\p{Extended_Pictographic}\p{Emoji_Modifier}\p{Regional_Indicator}‍︎️⃣\u{E0020}-\u{E007F}]/gu;

/**
 * Removes everything that is not worth translating: links, mentions, timestamps, custom emoji and
 * emoji pieces (R172/R173).
 * @param {unknown} content
 * @returns {string}
 */
export function cleanForTranslation(content) {
  return String(content ?? '').toWellFormed()
    .replace(/<a?:\w{1,32}:\d{17,20}>/g, ' ')              // custom emoji
    .replace(/<(?:@[!&]?|#)\d{17,20}>/g, ' ')               // user / role / channel mentions
    .replace(/<\/[^:>\n]{1,96}:\d{17,20}>/g, ' ')           // slash-command mentions
    .replace(/<t:-?\d{1,13}(?::[tTdDfFR])?>/g, ' ')         // timestamps
    .replace(/@(?:everyone|here)\b/g, ' ')
    .replace(/<?https?:\/\/[^\s<>]+>?/gi, ' ')              // links are never translated
    .replace(EMOJI_RESIDUE, ' ')                             // emoji incl. VS16, skin tones, ZWJ, flags, keycaps, tag chars
    .replace(/[^\S\n]+/g, ' ').replace(/ ?\n ?/g, '\n').replace(/\n{3,}/g, '\n\n')
    .trim();
}

const CJK_CHAR = /[\p{Script=Han}\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Hangul}]/u;

/** At least two letters, or any CJK character (single-character words exist). */
export function hasTranslatableText(text) {
  const s = String(text ?? '');
  let letters = 0;
  for (const ch of s) if (/\p{L}/u.test(ch) && ++letters >= 2) return true;
  return CJK_CHAR.test(s);
}

const CJK_END = new Set(['。', '！', '？']);
const LATIN_END = new Set(['.', '!', '?', '؟', '।']);
const isSpace = (ch) => ch !== undefined && /\s/.test(ch);

// A cut point in a code-point array, searched backwards from `limit` down to `minIdx`: a newline, then a
// CJK sentence end, then a sentence end followed by whitespace, then any whitespace. The piece is
// cps[start..end); the next piece starts at `next`; `sep` is what the consumed boundary stood for.
function findCut(cps, start, limit, minIdx) {
  const lo = Math.max(minIdx, start + 1);
  for (let b = limit; b >= lo; b--) if (cps[b] === '\n') return { end: b, next: b + 1, sep: '\n' };
  for (let b = limit; b >= lo; b--) if (CJK_END.has(cps[b - 1])) return { end: b, next: b, sep: '' };
  for (let b = limit; b >= lo; b--) if (LATIN_END.has(cps[b - 1]) && isSpace(cps[b])) return { end: b, next: b + 1, sep: ' ' };
  for (let b = limit; b >= lo; b--) if (isSpace(cps[b])) return { end: b, next: b + 1, sep: ' ' };
  return null;
}

/**
 * Cuts text to the request limits on a sentence or word boundary (never inside a surrogate pair).
 * @param {string} text
 * @param {{ maxCodePoints: number, maxEncodedBytes?: number }} limits
 * @returns {{ text: string, truncated: boolean }}
 */
export function clampForRequest(text, { maxCodePoints, maxEncodedBytes = Infinity }) {
  const s = String(text ?? '').toWellFormed();
  const cps = Array.from(s);
  let bytes = 0;
  let n = 0;
  while (n < cps.length && n < maxCodePoints) {
    const b = maxEncodedBytes === Infinity ? 0 : encodeURIComponent(cps[n]).length;
    if (bytes + b > maxEncodedBytes) break;
    bytes += b;
    n++;
  }
  if (n >= cps.length) return { text: s, truncated: false };
  const cut = findCut(cps, 0, n, Math.floor(n * 0.7));
  return { text: cps.slice(0, cut ? cut.end : n).join('').trimEnd(), truncated: true };
}

/**
 * Splits text into pieces whose URL-encoded size is at most `maxEncodedBytes`, on sentence or word
 * boundaries. `pieces.map((p) => p.text + p.sep).join('')` restores the text (consumed whitespace aside).
 * @param {string} text
 * @param {number} maxEncodedBytes
 * @returns {Array<{ text: string, sep: string }>}
 */
export function splitForRequests(text, maxEncodedBytes) {
  const cps = Array.from(String(text ?? '').toWellFormed());
  const enc = cps.map((c) => encodeURIComponent(c).length);
  const pieces = [];
  let start = 0;
  while (start < cps.length) {
    let bytes = 0;
    let end = start;
    while (end < cps.length && bytes + enc[end] <= maxEncodedBytes) { bytes += enc[end]; end++; }
    if (end >= cps.length) { pieces.push({ text: cps.slice(start).join(''), sep: '' }); break; }
    if (end === start) end = start + 1; // a single code point above the limit: take it alone, never loop
    const cut = findCut(cps, start, end, start + Math.floor((end - start) * 0.5));
    pieces.push({ text: cps.slice(start, cut ? cut.end : end).join(''), sep: cut ? cut.sep : '' });
    start = cut ? cut.next : end;
  }
  return pieces.length ? pieces : [{ text: '', sep: '' }];
}

/**
 * Splits a long result into at most `maxParts` embed descriptions of at most `max` UTF-16 units; when
 * text is left over, the last part ends with an ellipsis (R104/R105).
 * @param {string} text
 * @param {number} [max]
 * @param {number} [maxParts]
 * @returns {string[]}
 */
export function splitForEmbeds(text, max = EMBED_PART_MAX, maxParts = EMBED_MAX_PARTS) {
  const cps = Array.from(String(text ?? '').toWellFormed());
  const parts = [];
  let start = 0;
  while (start < cps.length && parts.length < maxParts) {
    let len = 0;
    let end = start;
    while (end < cps.length && len + cps[end].length <= max) { len += cps[end].length; end++; }
    if (end >= cps.length) { parts.push(cps.slice(start).join('')); break; }
    if (parts.length === maxParts - 1) { parts.push(clampText(cps.slice(start).join(''), max - 1)); break; }
    if (end === start) end = start + 1;
    const cut = findCut(cps, start, end, start + Math.floor((end - start) * 0.5));
    parts.push(cps.slice(start, cut ? cut.end : end).join(''));
    start = cut ? cut.next : end;
  }
  return parts.map((p) => p.trim()).filter(Boolean);
}

/**
 * What auto-translate sends for a message: cleaned, capped, with the spoiler flag. null = nothing to translate.
 * @param {unknown} content
 * @returns {{ text: string, truncated: boolean, hadSpoiler: boolean }|null}
 */
export function prepareAutoInput(content) {
  const raw = String(content ?? '');
  const hadSpoiler = (raw.match(/\|\|/g)?.length ?? 0) >= 2;
  const cleaned = cleanForTranslation(raw.replaceAll('||', ' '));
  if (!hasTranslatableText(cleaned)) return null;
  const { text, truncated } = clampForRequest(cleaned, {
    maxCodePoints: AUTO_INPUT_MAX_CODEPOINTS,
    maxEncodedBytes: providerName() === 'google' ? MAX_POST_Q_BYTES : Infinity,
  });
  return { text, truncated, hadSpoiler };
}

/** True when a "translation" only repeats the source (ignoring case, spaces and punctuation). */
export function isEcho(a, b) {
  const norm = (s) => String(s ?? '').toLowerCase().replace(/[\s\p{P}\p{S}]/gu, '');
  return norm(a) === norm(b);
}

// ── Translation core (R050, R051, R178, R179, R180, R181) ─────────────────────

const LATIN_LETTER = /\p{Script=Latin}/u;
const HAN_CHAR = /\p{Script=Han}/u;
const KANA_OR_HANGUL = /[\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Hangul}]/u;

/**
 * Rejects an impossible detection. Google reports Traditional Chinese as "en" for some sentences
 * (observed live, while the translation itself is right), which would drop the English target and send
 * a wrong source hint with the next request. A language written in the Latin script cannot be the source
 * of text without a single Latin letter: Han-only text is then taken as Traditional Chinese (Google
 * labels Simplified correctly), anything else is "unknown" (null). Every plausible answer passes through.
 * @param {string} text the text that was sent
 * @param {string|null} detected canonical code from the provider
 * @returns {string|null}
 */
function reconcileDetected(text, detected) {
  if (!detected || LATIN_LETTER.test(text) || !/\p{L}/u.test(text)) return detected;
  let script;
  try {
    script = new Intl.Locale(detected).maximize().script;
  } catch {
    return detected;
  }
  if (script !== 'Latn') return detected;
  return HAN_CHAR.test(text) && !KANA_OR_HANGUL.test(text) ? 'zh-tw' : null;
}

const resultKey = (provider, target, text) => `tr:${provider}:${target}:${createHash('sha1').update(text, 'utf8').digest('hex')}`;

/**
 * Translates `text` into `target` (a canonical LANGUAGES key). The source is always detected by the
 * provider (never guessed from the script) unless the caller passes a canonical `source`.
 * Google: one GET, or one POST for long text; text above MAX_POST_Q_BYTES is split into at most
 * `maxChunks` sequential requests.
 * @param {string} text
 * @param {string} target
 * @param {{ source?: string, maxChunks?: number }} [opts]
 * @returns {Promise<{ text: string, detected: string|null }>}
 * @throws {TranslationError}
 */
export async function translateText(text, target, { source = 'auto', maxChunks = 1 } = {}) {
  if (!has(LANGUAGES, target) || isSupportedTarget(target) === false) {
    throw new TranslationError('UNSUPPORTED_LANGUAGE', `Unsupported target "${target}"`, { lang: target });
  }
  await assertNotPaused();
  const input = String(text ?? '').toWellFormed().trim();
  if (!input) throw new TranslationError('EMPTY', 'Nothing to translate');

  const { provider, resultCache } = config();
  // Fail-open: Redis being down only means a network request is made.
  const cacheable = resultCache && Array.from(input).length <= RESULT_CACHE_MAX_CODEPOINTS;
  const key = cacheable ? resultKey(provider, target, input) : null;
  if (key) {
    const hit = await cache.get(key);
    if (hit && typeof hit.t === 'string') return { text: hit.t, detected: hit.d ?? null };
  }

  let translated;
  let detected;
  if (provider === 'libretranslate') {
    ({ text: translated, detected } = await ltTranslate(input, target, source));
    if (source === 'auto') detected = reconcileDetected(input, detected);
  } else if (encodeURIComponent(input).length <= MAX_POST_Q_BYTES) {
    ({ text: translated, detected } = await googleTranslate(input, target, source));
    if (source === 'auto') detected = reconcileDetected(input, detected);
  } else {
    const pieces = splitForRequests(input, MAX_POST_Q_BYTES);
    if (pieces.length > maxChunks) throw new TranslationError('TOO_LONG', 'Text is too long to translate');
    translated = '';
    detected = null;
    let from = source;
    for (const piece of pieces) {
      const r = await googleTranslate(piece.text, target, from);
      translated += r.text + piece.sep;
      if (detected === null) {
        detected = from === 'auto' ? reconcileDetected(piece.text, r.detected) : r.detected;
        if (from === 'auto' && detected && has(LANGUAGES, detected)) from = detected;
      }
    }
    translated = translated.trim();
  }

  if (key) await cache.set(key, { t: translated, d: detected }, RESULT_CACHE_TTL_S);
  return { text: translated, detected };
}

/**
 * One request per target at most and no separate detect request: the first response supplies the
 * detected language (R180/R181). Never throws.
 * @param {string} text
 * @param {string[]} targets canonical codes
 * @param {{ deadline?: number }} [opts]
 * @returns {Promise<{ detected: string|null, results: Array<{ target: string, text: string }>, failed: Array<{ target: string, code: string }>, pausedStop: boolean }>}
 */
export async function translateToTargets(text, targets, { deadline = Date.now() + AUTO_DEADLINE_MS } = {}) {
  let detected = null;
  const results = [];
  const failed = [];
  let pausedStop = false;
  for (const target of targets) {
    if (Date.now() >= deadline) { failed.push({ target, code: 'TIMEOUT' }); continue; }
    if (detected && target === detected) continue; // same language: no request (R176/R177)
    if (isSupportedTarget(target) === false) { failed.push({ target, code: 'UNSUPPORTED_LANGUAGE' }); continue; }
    const source = detected && has(LANGUAGES, detected) && isSupportedTarget(detected) !== false ? detected : 'auto';
    try {
      const r = await translateText(text, target, { source });
      if (!detected && r.detected) detected = r.detected;   // the first response supplies the detection
      if (r.detected && r.detected === target) continue;    // the first target was the source language: drop it
      if (!r.text.trim() || isEcho(text, r.text)) continue; // R173: nothing new to show
      results.push({ target, text: r.text });
    } catch (err) {
      if (err?.code === 'PAUSED' || err?.code === 'RATE_LIMITED') { pausedStop = true; break; }
      failed.push({ target, code: err?.code ?? 'ERROR' });
    }
  }
  return { detected, results, failed, pausedStop };
}

/**
 * Detects the language of `text` (used by /detectlanguage). Long input is sampled.
 * @param {string} text
 * @returns {Promise<{ code: string|null, name: string }>}
 * @throws {TranslationError}
 */
export async function detectLanguage(text) {
  const sample = clampForRequest(cleanForTranslation(text), {
    maxCodePoints: DETECT_SAMPLE_CODEPOINTS,
    maxEncodedBytes: MAX_Q_ENCODED_BYTES,
  }).text;
  if (!hasTranslatableText(sample)) throw new TranslationError('EMPTY', 'Nothing to analyse');
  await assertNotPaused();
  const raw = providerName() === 'libretranslate'
    ? await ltDetect(sample)
    : (await googleTranslate(sample, 'en', 'auto')).detected;
  const code = reconcileDetected(sample, raw);
  return { code, name: code ? languageName(code) : 'Unknown' };
}

// ── Error descriptions for commands (R104, R105, R255) ────────────────────────

/**
 * @param {unknown} err
 * @returns {{ level: 'warn'|'error', embed: EmbedBuilder, log: 'none'|'warn'|'error' }}
 */
export function describeTranslationError(err) {
  const code = err instanceof TranslationError ? err.code : null;
  switch (code) {
    case 'PAUSED':
    case 'RATE_LIMITED': {
      const ms = err.retryAfterMs ?? pauseRemainingMs();
      const minutes = Math.max(1, Math.ceil(ms / 60_000));
      return {
        level: 'warn',
        log: 'none',
        embed: warnEmbed(
          'Translation Paused',
          `The translation provider is rate-limiting this bot, so translation is paused. Try again ${discordTimestamp(new Date(Date.now() + ms), 'R')} (about ${minutes} minute${minutes === 1 ? '' : 's'}).`,
        ),
      };
    }
    case 'UNSUPPORTED_LANGUAGE':
      return {
        level: 'error',
        log: 'none',
        embed: errorEmbed(
          'Language Not Supported',
          `${err.lang ? `${languageName(err.lang)} (\`${err.lang}\`)` : 'That language'} isn't supported by this bot's translation backend (${providerLabel()}). Use \`/languages\` to see what is available.`,
        ),
      };
    case 'EMPTY':
      return { level: 'error', log: 'none', embed: errorEmbed('Nothing to Translate', 'That text has no words to translate.') };
    case 'TOO_LONG':
      return { level: 'error', log: 'warn', embed: errorEmbed('Text Too Long', 'That text is too long to translate in one go. Please shorten it.') };
    case 'TIMEOUT':
      return { level: 'error', log: 'warn', embed: errorEmbed('Translation Failed', 'The translation service took too long to answer. Please try again.') };
    case 'NETWORK':
    case 'HTTP_ERROR':
    case 'BAD_RESPONSE':
      return { level: 'error', log: 'warn', embed: errorEmbed('Translation Failed', 'Could not reach the translation service. Please try again later.') };
    default:
      return { level: 'error', log: 'error', embed: errorEmbed('Translation Failed', 'Something went wrong while translating. Please try again.') };
  }
}

// ── Output formatting (R182, R183, R184, R289, R290) ──────────────────────────

/**
 * Closes markdown a cut left open (fences, **, __, ~~, backticks). Only for text that was cut.
 * @param {string} text
 * @returns {string}
 */
export function balanceMarkdown(text) {
  let s = String(text ?? '');
  const count = (needle) => s.split(needle).length - 1;
  if (count('```') % 2 === 1) return `${s}\n\`\`\``;
  for (const marker of ['**', '__', '~~']) if (count(marker) % 2 === 1) s += marker;
  const singles = (s.match(/`/g)?.length ?? 0) - 3 * count('```');
  if (singles % 2 === 1) s += '`';
  return s;
}

/** @returns {string} e.g. "🇹🇼 **ZH-TW** — ..." */
export function formatTargetLine(target, body) {
  return `${langFlag(target)} **${target.toUpperCase()}** — ${body}`;
}

/**
 * The auto-translate embed: one line per target within a 4,000 character budget; a cut line ends with an
 * ellipsis and balanced markdown; text from a spoilered message is fully spoilered (R182, R289).
 * @param {{ results: Array<{ target: string, text: string }>, detected: string|null, truncatedInput?: boolean,
 *   hadSpoiler?: boolean, pausedStop?: boolean, settings?: object|null }} args
 * @returns {EmbedBuilder}
 */
export function buildAutoTranslateEmbed({ results, detected, truncatedInput = false, hadSpoiler = false, pausedStop = false, settings = null }) {
  const n = Math.max(1, results.length);
  const per = Math.min(AUTO_PER_TARGET_MAX, Math.floor(AUTO_DESCRIPTION_BUDGET / n) - 24);
  const lines = results.map(({ target, text }) => {
    const room = per - (hadSpoiler ? 4 : 0) - 8;
    let body = clampText(text, room);
    if (body !== text) body = balanceMarkdown(body);
    if (hadSpoiler) body = `||${body.replaceAll('||', '')}||`;
    return formatTargetLine(target, body);
  });
  const footer = `Detected: ${languageName(detected)}${truncatedInput ? ' · only the first part of this long message was translated' : ''}${pausedStop ? ' · some languages were skipped (rate limited)' : ''}`;
  return applyGuildColor(new EmbedBuilder().setColor(COLORS.INFO), settings)
    .setDescription(clampText(lines.join('\n'), 4096))
    .setFooter({ text: clampText(footer, 2048) });
}

/**
 * The reply payload: never pings (the author is not pinged on a reply either, R183/R184/R290), and
 * failIfNotExists keeps a translation from being posted for a message deleted meanwhile.
 * @param {EmbedBuilder} embed
 */
export function buildAutoReplyPayload(embed) {
  return { embeds: [embed], allowedMentions: { parse: [], repliedUser: false }, failIfNotExists: true };
}

// ── Channel configuration cache ───────────────────────────────────────────────

const channelsKey = (guildId) => `guild:${guildId}:translateChannels`;
const validRows = (rows) => Array.isArray(rows)
  && rows.every((r) => r && typeof r.channelId === 'string' && Array.isArray(r.targetLangs));

function memoize(guildId, map) {
  channelMemo.delete(guildId);
  channelMemo.set(guildId, { at: Date.now(), map });
  while (channelMemo.size > CHANNEL_MEMO_MAX) channelMemo.delete(channelMemo.keys().next().value);
}

const genOf = (guildId) => channelGen.get(guildId) ?? 0;

function bumpGen(guildId) {
  const next = genOf(guildId) + 1;
  channelGen.delete(guildId);
  channelGen.set(guildId, next);
  while (channelGen.size > CHANNEL_MEMO_MAX) channelGen.delete(channelGen.keys().next().value);
  return next;
}

const toMap = (rows) => new Map(rows.map((r) => [r.channelId, normalizeTargetList(r.targetLangs).valid.slice(0, MAX_TARGETS)]));

async function loadRows(guildId) {
  return prisma.translateChannel.findMany({ where: { guildId }, select: { channelId: true, targetLangs: true } });
}

/**
 * Every configured channel of a guild with its canonical target list. Reads the memo, then Redis, then
 * Postgres (fail-open: a Redis miss or outage reads Postgres). Legacy duplicates and codes are cleaned
 * on read.
 * @param {string} guildId
 * @returns {Promise<Map<string, string[]>>}
 * @throws only when Postgres fails
 */
export async function getGuildTranslateConfig(guildId) {
  const memo = channelMemo.get(guildId);
  if (memo && Date.now() - memo.at < CHANNELS_MEMO_TTL_MS) return memo.map;
  const gen = genOf(guildId); // a change that lands while this read is in flight must not be overwritten by it
  let rows = await cache.get(channelsKey(guildId));
  if (!validRows(rows)) {
    rows = await loadRows(guildId);
    // setNX: a reader that loaded rows just before an /autotranslate change must not overwrite the new list.
    if (genOf(guildId) === gen) await cache.setNX(channelsKey(guildId), rows, CHANNELS_CACHE_TTL_S);
  }
  const map = toMap(rows);
  if (genOf(guildId) === gen) memoize(guildId, map);
  return map;
}

/**
 * The target languages of one channel, or null. Exact channel match only: a thread does not inherit its
 * parent's configuration. Never throws.
 * @param {string} guildId
 * @param {string} channelId
 * @returns {Promise<string[]|null>}
 */
export async function getChannelTargets(guildId, channelId) {
  try {
    return (await getGuildTranslateConfig(guildId)).get(channelId) ?? null;
  } catch (err) {
    if (throttles.tenMin(`config:${guildId}`)) logger.warn(`translationService: could not load the channel config of guild ${guildId}: ${err?.message ?? err}`);
    return null;
  }
}

/**
 * Write-through after a change (a plain SET that overwrites). Never throws.
 * @param {string} guildId
 * @returns {Promise<void>}
 */
export async function refreshTranslateCache(guildId) {
  const gen = bumpGen(guildId); // reads that began before this change no longer store what they loaded
  try {
    const rows = await loadRows(guildId);
    if (genOf(guildId) !== gen) return; // a newer refresh superseded this one and writes the newer list
    await cache.set(channelsKey(guildId), rows, CHANNELS_CACHE_TTL_S);
    if (genOf(guildId) !== gen) return;
    bumpGen(guildId); // also reads that began while this refresh ran
    memoize(guildId, toMap(rows));
  } catch (err) {
    bumpGen(guildId);
    channelMemo.delete(guildId);
    await cache.del(channelsKey(guildId));
    logger.warn(`translationService: could not refresh the channel config of guild ${guildId}: ${err?.message ?? err}`);
  }
}

// ── Capped writes ─────────────────────────────────────────────────────────────

const TX_OPTS = { timeout: 10_000, maxWait: 10_000 };
const channelsLockSql = (guildId) => Prisma.sql`SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext(${`translate-channels:${guildId}`}))`;

export const RAW_SQL = Object.freeze([
  { name: 'translation.channelsLock', build: () => channelsLockSql('0') },
]);

/**
 * Adds a translated channel, or sets the languages of an existing one, under LIMITS.TRANSLATE_CHANNELS. The advisory
 * lock per guild makes the cap exact when /autotranslate and the dashboard save at the same time; both writers use it.
 * The caller does ensureGuildSettings first and refreshTranslateCache after.
 * @param {string} guildId
 * @param {string} channelId
 * @param {string[]} targetLangs canonical codes
 * @param {{ updateExisting?: boolean }} [opts] false: an existing row is reported as 'EXISTS' instead of updated
 * @returns {Promise<{ ok: true, created: boolean, row: object } | { ok: false, code: 'LIMIT', count: number } | { ok: false, code: 'EXISTS' }>}
 */
export async function saveTranslateChannel(guildId, channelId, targetLangs, { updateExisting = true } = {}) {
  return prisma.$transaction(async (tx) => {
    await tx.$queryRaw(channelsLockSql(guildId));
    const existing = await tx.translateChannel.findUnique({
      where: { guildId_channelId: { guildId, channelId } }, select: { id: true },
    });
    if (existing && !updateExisting) return { ok: false, code: 'EXISTS' };
    if (!existing) {
      const count = await tx.translateChannel.count({ where: { guildId } });
      if (count >= LIMITS.TRANSLATE_CHANNELS) return { ok: false, code: 'LIMIT', count };
    }
    const row = await tx.translateChannel.upsert({
      where: { guildId_channelId: { guildId, channelId } },
      update: { targetLangs },
      create: { guildId, channelId, targetLangs },
    });
    return { ok: true, created: !existing, row };
  }, TX_OPTS);
}

/** @deprecated Alias of refreshTranslateCache, kept for compatibility. */
export const invalidateTranslateCache = refreshTranslateCache;

// ── The auto-translate hook body ──────────────────────────────────────────────

const REPLY_GONE = new Set([10008, 50035]);
const REPLY_BLOCKED = new Set([10003, 50001, 50013, 50083, 160005]);
const AUTO_NEEDS = ['ViewChannel', 'SendMessages', 'EmbedLinks', 'ReadMessageHistory'];

/**
 * messageCreate hook `translation-fixes` (last in the chain). Translates a message into the targets
 * of its channel and replies with one embed. Never throws.
 * @param {object} ctx hook context ({ message, guildId, state, getSettings })
 * @param {{ checkPermissions?: boolean }} [opts] the option exists only for offline tests
 * @returns {Promise<void>}
 */
export async function handleAutoTranslate(ctx, { checkPermissions = true } = {}) {
  try {
    const { message, guildId } = ctx ?? {};
    if (!message || !guildId) return;
    if (message.system || message.author?.bot || message.webhookId) return; // R288
    if (ctx.state?.automodAction?.deleted || wasDeletedByBot(message.id)) return; // R264/R265

    // Everything below runs only in configured channels, so other channels never run the cleaner (R011/R012).
    let targets = await getChannelTargets(guildId, message.channelId);
    if (!targets || !targets.length) return;
    if (isLinkOnly(message.content)) return;
    const prepared = prepareAutoInput(message.content);
    if (!prepared) return;

    targets = targets.filter((code) => {
      if (isSupportedTarget(code) !== false) return true;
      if (throttles.hour(`unsupported:${guildId}:${code}`)) {
        logger.warn(`Auto-translate target "${code}" in guild ${guildId} is not supported by ${providerLabel()}; remove it with /autotranslate.`);
      }
      return false;
    });
    if (!targets.length) return;

    if (checkPermissions) {
      const usable = checkChannelUsable(message.channel, { needs: AUTO_NEEDS });
      if (!usable.ok) {
        if (throttles.tenMin(`perm:${message.channelId}`)) {
          logger.warn(`Auto-translate: missing ${usable.missing.join(', ') || 'access'} in ${message.channelId}`);
        }
        return; // before any request: no wasted quota
      }
    }

    if (autoInflight >= AUTO_MAX_INFLIGHT) {
      if (throttles.minute('busy')) logger.warn('Auto-translate is busy; skipping messages');
      return;
    }
    autoInflight++;
    try {
      const outcome = await translateToTargets(prepared.text, targets, { deadline: Date.now() + AUTO_DEADLINE_MS });
      const codes = [...new Set(outcome.failed.filter((f) => f.code !== 'UNSUPPORTED_LANGUAGE').map((f) => f.code))];
      if (codes.length && throttles.tenMin(`fail:${message.channelId}`)) {
        logger.warn(`Auto-translate failed in ${message.channelId}: ${codes.join(', ')}`);
      }
      if (!outcome.results.length) return;
      if (wasDeletedByBot(message.id)) return;

      const settings = typeof ctx.getSettings === 'function' ? await ctx.getSettings() : null;
      const embed = buildAutoTranslateEmbed({
        results: outcome.results,
        detected: outcome.detected,
        truncatedInput: prepared.truncated,
        hadSpoiler: prepared.hadSpoiler,
        pausedStop: outcome.pausedStop,
        settings,
      });
      try {
        await message.reply(buildAutoReplyPayload(embed));
        // Unawaited: counting never delays or fails the hook.
        void recordTranslation({
          guildId, channelId: message.channelId, via: STAT_VIA.AUTO,
          detected: outcome.detected, targets: outcome.results.map((r) => r.target),
        });
      } catch (err) {
        if (REPLY_GONE.has(err?.code)) logger.debug(`Auto-translate: the message ${message.id} is gone (${err.code})`);
        else if (REPLY_BLOCKED.has(err?.code)) {
          if (throttles.tenMin(`reply:${message.channelId}`)) logger.warn(`Auto-translate cannot reply in ${message.channelId} (${err.code})`);
        } else logger.warn(`Auto-translate reply failed in ${message.channelId}: ${err?.message ?? err}`);
      }
    } finally {
      autoInflight--;
    }
  } catch (err) {
    logger.warn(`Auto-translate failed: ${err?.message ?? err}`);
  }
}

// ── Test seam (offline tests only; never used by production code) ─────────────

export const __testing = Object.freeze({
  /** Overrides the lazy env config: { provider, ltUrl, ltKey, resultCache }. */
  configure(opts = {}) {
    override = { ...override, ...Object.fromEntries(Object.entries(opts).filter(([, v]) => v !== undefined)) };
  },
  /** Puts a fresh entry in the channel memo (no DB or Redis): primeChannelConfig('g1', { c1: ['es'] }). */
  primeChannelConfig(guildId, channels = {}) {
    memoize(guildId, new Map(Object.entries(channels).map(([id, langs]) => [id, [...langs]])));
  },
  reset() {
    pausedUntil = 0;
    lastPauseSync = 0;
    ltSupport = { codes: new Set(), rawCodes: new Set(), loadedAt: 0, failedAt: 0, inflight: null, known: false };
    runtimeUnsupported = new Set();
    channelMemo = new Map();
    channelGen = new Map();
    autoInflight = 0;
    googleBlockedUntil.clear();
    langCache = { key: '', value: LANGUAGES };
    throttles = makeThrottles();
    override = {};
    envConfig = null;
    displayNames = null;
  },
});
