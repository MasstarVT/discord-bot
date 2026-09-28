import prisma from '../database/client.js';
import { cache } from './redis.js';
import logger from '../utils/logger.js';
import { EmbedBuilder } from 'discord.js';
import { COLORS } from '../config/constants.js';

// ── Language map ──────────────────────────────────────────────────────────────

export const LANGUAGES = {
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
  zu: 'Zulu',
};

// Language code → ISO 3166-1 alpha-2 country code for flag emoji
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
  yi: 'IL', zh: 'CN', zu: 'ZA',
};

function langFlag(langCode) {
  const country = LANG_FLAGS[langCode];
  if (!country) return '🌐';
  return [...country.toUpperCase()]
    .map((c) => String.fromCodePoint(0x1F1E6 + c.charCodeAt(0) - 65))
    .join('');
}

export function getLanguages() {
  return LANGUAGES;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function hasCjk(text) {
  return /[一-鿿㐀-䶿]/.test(text);
}

function cleanContent(content) {
  return content
    .replace(/\p{Extended_Pictographic}/gu, '')
    .replace(/<a?:\w+:\d+>/g, '')
    .trim();
}

// ── HTTP layer ────────────────────────────────────────────────────────────────
//
// Backend is chosen with TRANSLATE_PROVIDER:
//   google (default)  the free translate.googleapis.com endpoint (no key, but it rate-limits hard)
//   libretranslate    a LibreTranslate server at LIBRETRANSLATE_URL (optional LIBRETRANSLATE_API_KEY)

const PROVIDER = (process.env.TRANSLATE_PROVIDER || 'google').toLowerCase() === 'libretranslate' ? 'libretranslate' : 'google';
const LT_URL = (process.env.LIBRETRANSLATE_URL || 'http://localhost:5000').replace(/\/+$/, '');
const LT_KEY = process.env.LIBRETRANSLATE_API_KEY || '';

// Circuit breaker: after a 429, stop calling the backend for a while instead of
// hammering it (hammering Google while it rate-limits you only extends the block).
const PAUSE_ON_429_MS = PROVIDER === 'google' ? 30 * 60_000 : 60_000;
let pausedUntil = 0;

export function translationPaused() {
  return Date.now() < pausedUntil;
}

function assertNotPaused() {
  if (translationPaused()) {
    const mins = Math.ceil((pausedUntil - Date.now()) / 60_000);
    throw new Error(`Translation is paused for ${mins} more minute(s) after a rate limit`);
  }
}

function tripOnStatus(status) {
  if (status !== 429 || translationPaused()) return;
  pausedUntil = Date.now() + PAUSE_ON_429_MS;
  logger.warn(`translationService: ${PROVIDER} returned 429; pausing translation for ${PAUSE_ON_429_MS / 60_000} min`);
}

// Bot language codes follow Google's; LibreTranslate names Chinese and Hebrew differently.
const TO_LT = { 'zh-cn': 'zh-Hans', 'zh-tw': 'zh-Hant', zh: 'zh-Hans', iw: 'he' };
const FROM_LT = { 'zh-hans': 'zh-CN', 'zh-hant': 'zh-TW', he: 'iw' };
const toLt = (code) => TO_LT[code.toLowerCase()] ?? code;
const fromLt = (code) => FROM_LT[code.toLowerCase()] ?? code;

async function libreTranslatePost(path, body) {
  assertNotPaused();
  const res = await fetch(`${LT_URL}${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(LT_KEY ? { ...body, api_key: LT_KEY } : body),
    signal: AbortSignal.timeout(15000),
  });
  if (!res.ok) {
    tripOnStatus(res.status);
    throw new Error(`LibreTranslate returned ${res.status}`);
  }
  return res.json();
}

async function fetchTranslation(text, targetLang, sourceLang = 'auto') {
  if (PROVIDER === 'libretranslate') {
    const data = await libreTranslatePost('/translate', {
      q: text,
      source: sourceLang === 'auto' ? 'auto' : toLt(sourceLang),
      target: toLt(targetLang),
      format: 'text',
    });
    const detected = sourceLang === 'auto' ? (data.detectedLanguage?.language ?? 'auto') : sourceLang;
    return { translatedText: data.translatedText ?? '', detectedLang: fromLt(detected) };
  }

  assertNotPaused();
  const url = `https://translate.googleapis.com/translate_a/single?client=gtx&sl=${sourceLang}&tl=${targetLang}&dt=t&q=${encodeURIComponent(text)}`;
  const res = await fetch(url, {
    headers: { 'User-Agent': 'Mozilla/5.0' },
    signal: AbortSignal.timeout(8000),
  });
  if (!res.ok) {
    tripOnStatus(res.status);
    throw new Error(`Translation API returned ${res.status}`);
  }
  const data = await res.json();
  const translatedText = data[0]?.map((seg) => seg?.[0] ?? '').join('') ?? '';
  const detectedLang = data[2] ?? sourceLang;
  return { translatedText, detectedLang };
}

async function fetchDetection(text) {
  if (PROVIDER === 'libretranslate') {
    const data = await libreTranslatePost('/detect', { q: text });
    const best = Array.isArray(data) ? data[0] : null;
    if (!best?.language) throw new Error('LibreTranslate could not detect the language');
    return fromLt(best.language);
  }
  // Google has no separate detect call; a translation to English reports the source language.
  const sourceLang = hasCjk(text) ? 'zh-CN' : 'auto';
  const { detectedLang } = await fetchTranslation(text, 'en', sourceLang);
  return detectedLang;
}

// ── Public API ────────────────────────────────────────────────────────────────

export async function detectLanguage(text) {
  const detectedLang = await fetchDetection(text);
  const code = detectedLang.split('-')[0].toLowerCase();
  const name = LANGUAGES[code] ?? detectedLang;
  return { code, name };
}

export async function translate(text, targetLang) {
  // Google misdetects some CJK text, so it gets an explicit hint; LibreTranslate detects ja/zh itself.
  const sourceLang = PROVIDER === 'google' && hasCjk(text) ? 'zh-CN' : 'auto';
  const { translatedText, detectedLang } = await fetchTranslation(text, targetLang, sourceLang);
  const sourceCode = detectedLang.split('-')[0].toLowerCase();
  const sourceName = LANGUAGES[sourceCode] ?? detectedLang;
  const targetName = LANGUAGES[targetLang.split('-')[0].toLowerCase()] ?? targetLang;
  return { translatedText, sourceCode, sourceName, targetName };
}

// ── Cache helpers ─────────────────────────────────────────────────────────────

const TRANSLATE_CACHE_TTL = 300;

async function getSettings(guildId) {
  const key = `guild:${guildId}:settings`;
  const cached = await cache.get(key);
  if (cached) return cached;
  const settings = await prisma.guildSettings.findUnique({ where: { guildId } });
  if (settings) await cache.set(key, settings, TRANSLATE_CACHE_TTL);
  return settings;
}

async function getChannelConfigs(guildId) {
  const key = `guild:${guildId}:translateChannels`;
  const cached = await cache.get(key);
  if (cached) {
    return new Map(cached.map((r) => [r.channelId, r.targetLangs]));
  }
  const rows = await prisma.translateChannel.findMany({ where: { guildId } });
  await cache.set(key, rows, TRANSLATE_CACHE_TTL);
  return new Map(rows.map((r) => [r.channelId, r.targetLangs]));
}

export async function invalidateTranslateCache(guildId) {
  await cache.del(`guild:${guildId}:translateChannels`);
}

// ── Auto-translate engine ─────────────────────────────────────────────────────

export async function runAutoTranslate(message) {
  if (message.author.bot || !message.content || !message.guild) return;

  if (/^(https?:\/\/\S+\s*)+$/.test(message.content.trim())) return;

  const guildId = message.guild.id;
  const settings = await getSettings(guildId);
  if (!settings?.translationEnabled) return;

  const channelMap = await getChannelConfigs(guildId);
  const targetLangs = channelMap.get(message.channelId);
  if (!targetLangs || targetLangs.length === 0) return;

  const cleaned = cleanContent(message.content);
  if (!cleaned) return;

  // While rate-limited, skip quietly (the breaker already logged once).
  if (translationPaused()) return;

  // Detect source language once
  let sourceCode = 'auto';
  let sourceName = 'Unknown';
  try {
    const detected = await detectLanguage(cleaned);
    sourceCode = detected.code;
    sourceName = detected.name;
  } catch (err) {
    if (!translationPaused()) logger.warn(`translationService: language detection failed in ${message.channelId}: ${err.message}`);
    return;
  }

  // Collect all translations, skipping same-language targets
  const results = [];
  for (const targetLang of targetLangs) {
    if (sourceCode.slice(0, 2) === targetLang.slice(0, 2)) continue;
    try {
      const { translatedText } = await fetchTranslation(cleaned, targetLang, sourceCode);
      const targetName = LANGUAGES[targetLang] ?? targetLang;
      results.push({ flag: langFlag(targetLang), targetLang, translatedText });
    } catch (err) {
      if (translationPaused()) break;
      logger.warn(`translationService: failed to translate to ${targetLang} in ${message.channelId}: ${err.message}`);
    }
  }

  if (results.length === 0) return;

  const description = results
    .map(({ flag, targetLang, translatedText }) => `${flag} **${targetLang.toUpperCase()}** — ${translatedText.slice(0, 300)}`)
    .join('\n');

  const embed = new EmbedBuilder()
    .setColor(COLORS.INFO)
    .setDescription(description)
    .setFooter({ text: `Detected: ${sourceName}` });

  await message.reply({ embeds: [embed] }).catch((err) =>
    logger.warn(`translationService: reply failed in ${message.channelId}: ${err.message}`)
  );
}
