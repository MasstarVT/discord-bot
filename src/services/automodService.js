// ── Auto-moderation (pkg-automod.md) ───────────────────────────────────────────
// The MEE6 rule set (bad words, repeated text, invites, links, caps, emojis, spoilers, mentions,
// zalgo, message-rate anti-spam), each with its own action and role/channel scope.
//
// This file holds the pure config model (v2 shape, v1 upgrade, normalisation), the detectors, the
// compiled-matcher cache, outcome resolution and the action pipeline `runAutomod(ctx, { isEdit })`.
// It has no database access of its own: settings arrive through the hook context, and every
// punishment goes through moderationService.executeModAction so the Discord action really happens.
//
// Failure policy (decided explicitly, 00-overview rule 10):
//   - Redis down       -> `spam` and `repeatedText` are skipped (fail OPEN: never punish on missing data).
//   - Malformed config -> normalised, never thrown; a bad list entry is dropped.
//   - Unknown member   -> if the lookup failed for any reason other than "not in the server", do
//                         nothing (staff status cannot be verified, so nothing is deleted).
//   - A failed delete  -> never punishes when the action needed the delete, never tells the member
//                         their message was removed.
import { PermissionFlagsBits } from 'discord.js';
import { createHash } from 'node:crypto';
import { domainToASCII } from 'node:url';
import { cache } from './redis.js';
import { executeModAction, isImmune } from './moderationService.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { PERMISSION_LEVELS, LIMITS } from '../config/constants.js';
import { createThrottle } from '../utils/async.js';
import { escapeRegex, compileSafeRegex, safeRegexFirstMatch, containsWord } from '../utils/safeRegex.js';
import { markBotDeletion } from '../utils/deletionTracker.js';
import { clampText } from '../utils/text.js';
import { checkChannelUsable } from '../utils/interactionHelpers.js';
import { warnEmbed, errorEmbed } from '../utils/embedBuilder.js';
import { formatDurationLong } from '../utils/parseDuration.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const AUTOMOD_CONFIG_VERSION = 2;
export const SCOPE_MODES = Object.freeze(['except', 'only']);
export const DEFAULT_TIMEOUT_SECONDS = 600;
export const MIN_TIMEOUT_SECONDS = 60;
export const MAX_TIMEOUT_SECONDS = 2_419_200; // 28 days (Discord timeout maximum)
export const CAPS_MIN_LETTERS = 10;
export const REPEAT_WINDOW_MS = 30_000;
export const SPAM_WINDOW_MS = 5_000;
export const PUNISH_WINDOW_MS = 30_000;
export const NOTICE_TTL_MS = 6_000;
export const BAN_DELETE_MESSAGE_SECONDS = 3_600;
export const PATTERN_INPUT_CAP = 4_000; // Nitro messages exceed safeRegex's 2000 default
export const WORD_MIN_LENGTH = 2;
export const WORD_MAX_LENGTH = 64;
export const EXEMPT_CAP = LIMITS.AUTOMOD_EXEMPTIONS; // 50

/** Seeded into `allowedDomains` only when that key is missing (new config or v1 upgrade). */
export const DEFAULT_ALLOWED_DOMAINS = Object.freeze([
  'tenor.com', 'giphy.com', 'discord.com', 'discordapp.com', 'discordapp.net',
]);

/** Rule keys in display order (= tie-break order). */
export const RULE_KEYS = Object.freeze([
  'badWords', 'repeatedText', 'invites', 'links', 'caps', 'emojis', 'spoilers', 'mentions', 'zalgo', 'spam',
]);

const ruleMeta = (label, emoji, threshold, onEdit, description) => Object.freeze({
  label,
  emoji,
  threshold: threshold ? Object.freeze(threshold) : null,
  onEdit,
  description,
});

/** The rule registry. `threshold` is `{ min, max, default, unit }` or null. */
export const RULES = Object.freeze({
  badWords: ruleMeta('Bad Words', '🤬', null, true, 'Removes messages that contain a listed word, phrase or pattern.'),
  repeatedText: ruleMeta('Repeated Text', '🔁', { min: 2, max: 20, default: 3, unit: 'identical messages within 30 s' }, false,
    'Catches the same message sent over and over.'),
  invites: ruleMeta('Server Invites', '📨', null, true, 'Catches invite links to other Discord servers.'),
  links: ruleMeta('External Links', '🔗', null, true, 'Catches links that are blocked, or not allowed when no domain is blocked.'),
  caps: ruleMeta('Excessive Caps', '🔠', { min: 50, max: 100, default: 70, unit: '% capitals, messages with at least 10 letters' }, true,
    'Catches messages that are mostly capital letters.'),
  emojis: ruleMeta('Excessive Emojis', '😀', { min: 1, max: 50, default: 10, unit: 'emojis' }, true,
    'Catches messages with too many emojis.'),
  spoilers: ruleMeta('Excessive Spoilers', '🙈', { min: 1, max: 50, default: 5, unit: 'spoilers' }, true,
    'Catches messages with too many spoiler tags or spoiler attachments.'),
  mentions: ruleMeta('Excessive Mentions', '📢', { min: 2, max: 50, default: 5, unit: 'mentions' }, true,
    'Catches messages that ping too many members or roles.'),
  zalgo: ruleMeta('Zalgo', '👹', { min: 3, max: 20, default: 4, unit: 'stacked combining marks' }, true,
    'Catches glitch text with piles of combining marks.'),
  spam: ruleMeta('Anti-Spam', '⏩', { min: 3, max: 30, default: 6, unit: 'messages within 5 s' }, false,
    'Catches members who send messages too fast.'),
});

export const ACTIONS = Object.freeze(['off', 'delete', 'warn', 'delete_warn', 'timeout', 'kick', 'ban']);

export const ACTION_LABELS = Object.freeze({
  off: 'Off',
  delete: 'Delete message',
  warn: 'Warn member',
  delete_warn: 'Delete & warn',
  timeout: 'Delete & timeout',
  kick: 'Delete & kick',
  ban: 'Delete & ban',
});

export const DELETING_ACTIONS = Object.freeze(new Set(['delete', 'delete_warn', 'timeout', 'kick', 'ban']));
export const PUNISHMENTS = Object.freeze({
  warn: 'WARN', delete_warn: 'WARN', timeout: 'TIMEOUT', kick: 'KICK', ban: 'BAN',
});
export const PUNISHMENT_RANK = Object.freeze({ WARN: 1, TIMEOUT: 2, KICK: 3, BAN: 4 });

const PUNISHMENT_WORD = Object.freeze({ WARN: 'Warn', TIMEOUT: 'Timeout', KICK: 'Kick', BAN: 'Ban' });

const SNOWFLAKE_RE = /^\d{17,20}$/;
const isPlainObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const clampInt = (n, min, max) => Math.min(max, Math.max(min, n));

// ── Config model ──────────────────────────────────────────────────────────────

function defaultRule(key) {
  const t = RULES[key].threshold;
  return {
    action: 'off',
    ...(t ? { threshold: t.default } : {}),
    timeoutSeconds: DEFAULT_TIMEOUT_SECONDS,
    roleMode: 'except',
    roleIds: [],
    channelMode: 'except',
    channelIds: [],
  };
}

/** A fresh, fully populated v2 config with every rule off. */
export function defaultConfig() {
  const rules = {};
  for (const key of RULE_KEYS) rules[key] = defaultRule(key);
  return {
    version: AUTOMOD_CONFIG_VERSION,
    rules,
    badWords: [],
    badWordPatterns: [],
    blockedDomains: [],
    allowedDomains: [...DEFAULT_ALLOWED_DOMAINS],
    exemptRoleIds: [],
    exemptChannelIds: [],
  };
}

/** True for a plain object whose `version` is undefined (the pre-v2 shape; `{}` counts). */
export function isLegacyConfig(raw) {
  return isPlainObject(raw) && raw.version === undefined;
}

// Invisible and zero-width characters: soft hyphen, combining grapheme joiner, Arabic letter mark,
// Mongolian vowel separator, zero-width and bidi marks, word joiner and invisible operators,
// the bidi isolates, and the byte-order mark.
const INVISIBLE_RE = /[\u{AD}\u{34F}\u{61C}\u{180E}\u{200B}-\u{200F}\u{202A}-\u{202E}\u{2060}-\u{206F}\u{FEFF}]/gu;

/** Removes invisible characters. */
export const stripInvisible = (s) => String(s ?? '').replace(INVISIBLE_RE, '');

/**
 * Bad words and patterns. NFKC folds fullwidth and math-styled letters, markdown emphasis, spoiler
 * bars and escapes are removed so "b**a**d" and "||bad||" still match. `_` stays: it is a word character.
 */
export const textForWords = (content) => stripInvisible(String(content ?? '').normalize('NFKC')).replace(/[\\*~|`]/g, '');

/** Invites and links: NFKC, invisible characters removed, and unescaped (discord\.gg). */
export const textForLinks = (content) => stripInvisible(String(content ?? '').normalize('NFKC')).replace(/\\/g, '');

/**
 * Words: NFKC, invisible characters removed, trimmed, lower-cased, inner whitespace collapsed.
 * Length 2-64 and at least one letter or digit.
 * @returns {string|null}
 */
export function normalizeWord(input) {
  const s = stripInvisible(String(input ?? '').normalize('NFKC')).trim().toLowerCase().replace(/\s+/g, ' ');
  if (s.length < WORD_MIN_LENGTH || s.length > WORD_MAX_LENGTH || !/[\p{L}\p{N}]/u.test(s)) return null;
  return s;
}

/**
 * Domains. Accepts "Example.COM", "https://www.example.com/path", "*.example.com" and IDN.
 * @returns {string|null} the ASCII (punycode) host without "www.", or null when invalid
 */
export function normalizeDomain(input) {
  let s = String(input ?? '').trim().toLowerCase();
  if (!s) return null;
  if (s.includes('://')) {
    try { s = new URL(s).hostname; } catch { return null; }
  }
  s = s.split(/[/?#]/)[0].replace(/:\d+$/, '').replace(/^\*\./, '').replace(/^www\./, '').replace(/\.$/, '');
  s = domainToASCII(s); // '' when invalid
  return /^(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+(?:[a-z]{2,63}|xn--[a-z0-9-]{1,59})$/.test(s) && s.length <= 253 ? s : null;
}

const dedupe = (list) => [...new Set(list)];

function cleanSnowflakes(v) {
  return Array.isArray(v) ? dedupe(v.filter((x) => typeof x === 'string' && SNOWFLAKE_RE.test(x))).slice(0, EXEMPT_CAP) : [];
}

function cleanWords(v) {
  if (!Array.isArray(v)) return [];
  const out = [];
  for (const item of v) {
    if (typeof item !== 'string' && !(typeof item === 'number' && Number.isFinite(item))) continue;
    const w = normalizeWord(String(item));
    if (w !== null) out.push(w);
  }
  return dedupe(out).slice(0, LIMITS.AUTOMOD_WORDS);
}

function cleanPatterns(v) {
  if (!Array.isArray(v)) return [];
  const out = [];
  for (const item of v) {
    if (typeof item !== 'string') continue;
    const p = item.trim();
    if (p && p.length <= 200) out.push(p);
  }
  return dedupe(out).slice(0, LIMITS.AUTOMOD_PATTERNS);
}

function cleanDomains(v) {
  if (!Array.isArray(v)) return [];
  const out = [];
  for (const item of v) {
    const d = normalizeDomain(item);
    if (d !== null) out.push(d);
  }
  return dedupe(out).slice(0, LIMITS.AUTOMOD_DOMAINS);
}

function cleanRule(key, r) {
  const out = defaultRule(key);
  if (!isPlainObject(r)) return out;
  if (ACTIONS.includes(r.action)) out.action = r.action;
  const t = RULES[key].threshold;
  if (t && Number.isInteger(r.threshold)) out.threshold = clampInt(r.threshold, t.min, t.max);
  if (Number.isInteger(r.timeoutSeconds)) out.timeoutSeconds = clampInt(r.timeoutSeconds, MIN_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS);
  if (SCOPE_MODES.includes(r.roleMode)) out.roleMode = r.roleMode;
  if (SCOPE_MODES.includes(r.channelMode)) out.channelMode = r.channelMode;
  out.roleIds = cleanSnowflakes(r.roleIds);
  out.channelIds = cleanSnowflakes(r.channelIds);
  return out;
}

function mapLegacyAction(value) {
  switch (String(value ?? '').toUpperCase()) {
    case 'NONE': return 'delete';
    case 'MUTE':
    case 'TIMEOUT': return 'timeout';
    case 'KICK': return 'kick';
    case 'BAN': return 'ban';
    default: return 'delete_warn'; // 'WARN', missing or unknown: the legacy default
  }
}

/**
 * Upgrades a v1 config (flat filter* keys) to v2. Only keys that are present enable a rule.
 * Pure, never throws, never mutates its input.
 * @param {unknown} raw
 * @returns {object} a normalised v2 config
 */
export function upgradeV1(raw) {
  try {
    const src = isPlainObject(raw) ? raw : {};
    const legacyAction = mapLegacyAction(src.automodAction);
    const rules = {};
    const enable = (key, extra = {}) => { rules[key] = { action: legacyAction, ...extra }; };

    if (src.filterInvites === true) enable('invites');
    if (src.filterLinks === true) enable('links');
    if (src.filterBadWords === true) enable('badWords');
    if (Number.isInteger(src.massMentionThreshold) && src.massMentionThreshold >= 1) {
      enable('mentions', { threshold: clampInt(src.massMentionThreshold, 2, 50) });
    }
    if (Number.isInteger(src.duplicateThreshold) && src.duplicateThreshold >= 2) {
      enable('repeatedText', { threshold: clampInt(src.duplicateThreshold, 2, 20) });
    }
    if (Number.isInteger(src.spamThreshold) && src.spamThreshold >= 1) {
      enable('spam', { threshold: clampInt(src.spamThreshold, 3, 30) });
    }

    return normalizeConfig({
      version: AUTOMOD_CONFIG_VERSION,
      rules,
      badWords: src.badWords,
      badWordPatterns: src.badWordPatterns,
      blockedDomains: src.blockedDomains,
      allowedDomains: [...DEFAULT_ALLOWED_DOMAINS],
      exemptRoleIds: src.exemptRoleIds,
      exemptChannelIds: src.exemptChannelIds,
    });
  } catch {
    return defaultConfig();
  }
}

/**
 * Returns a fresh, mutable, fully populated v2 config. Never throws, never mutates `raw` (which may
 * be deep-frozen), and is idempotent. Null, non-objects and arrays give the all-off default.
 * @param {unknown} raw
 * @returns {object}
 */
export function normalizeConfig(raw) {
  try {
    if (!isPlainObject(raw)) return defaultConfig();
    if (isLegacyConfig(raw)) return upgradeV1(raw);
    const src = structuredClone(raw);
    const cfg = defaultConfig();
    const srcRules = isPlainObject(src.rules) ? src.rules : {};
    for (const key of RULE_KEYS) cfg.rules[key] = cleanRule(key, srcRules[key]);
    cfg.badWords = cleanWords(src.badWords);
    cfg.badWordPatterns = cleanPatterns(src.badWordPatterns);
    cfg.blockedDomains = cleanDomains(src.blockedDomains);
    // An explicit [] stays empty (allowlist mode with nothing allowed); a missing or invalid key gets the defaults.
    if (Array.isArray(src.allowedDomains)) cfg.allowedDomains = cleanDomains(src.allowedDomains);
    cfg.exemptRoleIds = cleanSnowflakes(src.exemptRoleIds);
    cfg.exemptChannelIds = cleanSnowflakes(src.exemptChannelIds);
    return cfg;
  } catch (err) {
    logger.warn(`Automod config could not be normalised: ${err?.message ?? err}`);
    return defaultConfig();
  }
}

/** Rule keys whose action is not 'off', in display order. */
export const activeRuleKeys = (config) => RULE_KEYS.filter((k) => config?.rules?.[k] && config.rules[k].action !== 'off');

// ── Compiled matchers (in-process) ────────────────────────────────────────────

let warnedFallback = false;

/**
 * Compiles the bad-word alternation and the user patterns of a config.
 * The word regex is one escaped-literal alternation with no user regex syntax, so it cannot backtrack
 * catastrophically (R034). Patterns go through compileSafeRegex, which drops anything unsafe (R128).
 * @param {object} config a normalised v2 config
 * @returns {{ badWordsRe: RegExp|null, fallbackWords: string[]|null, patterns: RegExp[], patternIndex: number[] }}
 */
export function buildMatchers(config) {
  let badWordsRe = null;
  let fallbackWords = null;
  const words = Array.isArray(config?.badWords) ? config.badWords : [];
  if (words.length > 0) {
    try {
      const alts = [...words]
        .sort((a, b) => b.length - a.length)
        .map((w) => escapeRegex(w).replace(/ /g, '\\s+'));
      badWordsRe = new RegExp(`(?<![\\p{L}\\p{N}_])(?:${alts.join('|')})(?![\\p{L}\\p{N}_])`, 'iu');
    } catch (err) {
      if (!warnedFallback) {
        warnedFallback = true;
        logger.warn(`Automod bad-word regex could not be built, matching word by word: ${err?.message ?? err}`);
      }
      badWordsRe = null;
      fallbackWords = [...words];
    }
  }
  const patterns = [];
  const patternIndex = [];
  const list = Array.isArray(config?.badWordPatterns) ? config.badWordPatterns : [];
  list.forEach((p, i) => {
    const re = compileSafeRegex(p, 'i');
    if (re) { patterns.push(re); patternIndex.push(i); }
  });
  return { badWordsRe, fallbackWords, patterns, patternIndex };
}

/** @type {WeakMap<object, { config: object, matchers: object }>} */
let compiled = new WeakMap();
let DEFAULT_COMPILED = null;

/**
 * The normalised config and compiled matchers of a settings row, memoised per `automodConfig`
 * object (the memoised settings row is frozen, so the key is stable; a write makes a new object).
 * Callers must not mutate the returned `config`.
 * @param {{ automodConfig?: unknown }|null|undefined} settings
 * @returns {{ config: object, matchers: object }}
 */
export function getAutomodConfig(settings) {
  const raw = settings?.automodConfig;
  if (raw !== null && typeof raw === 'object') {
    let hit = compiled.get(raw);
    if (!hit) {
      const config = normalizeConfig(raw);
      hit = { config, matchers: buildMatchers(config) };
      compiled.set(raw, hit);
    }
    return hit;
  }
  if (!DEFAULT_COMPILED) {
    const config = defaultConfig();
    DEFAULT_COMPILED = { config, matchers: buildMatchers(config) };
  }
  return DEFAULT_COMPILED;
}

// ── In-process state (per shard; a guild's events always arrive on its own shard) ──

/** @type {Map<string, { rank: number, until: number }>} */
let recentPunishments = new Map();
let noticeThrottle = createThrottle(10_000);
let deleteLogThrottle = createThrottle(10_000);
let notDeletedThrottle = createThrottle(60_000);
let warnThrottle = createThrottle(600_000);

/** Clears every in-process cache and throttle (tests). */
export function resetAutomodState() {
  compiled = new WeakMap();
  DEFAULT_COMPILED = null;
  recentPunishments = new Map();
  noticeThrottle = createThrottle(10_000);
  deleteLogThrottle = createThrottle(10_000);
  notDeletedThrottle = createThrottle(60_000);
  warnThrottle = createThrottle(600_000);
  warnedFallback = false;
}

const warnOnce = (key, text) => { if (warnThrottle(key)) logger.warn(text); };

/**
 * One automod punishment per member per 30 s, unless the new one is more severe. Synchronous, so the
 * check and the set are atomic in-process. A failed punishment still occupies the slot.
 * @returns {boolean} true when the punishment may be applied
 */
export function claimPunishment(guildId, userId, rank, now = Date.now()) {
  const key = `${guildId}:${userId}`;
  const prev = recentPunishments.get(key);
  if (prev && prev.until > now && prev.rank >= rank) return false;
  recentPunishments.delete(key);
  recentPunishments.set(key, { rank, until: now + PUNISH_WINDOW_MS });
  if (recentPunishments.size > 10_000) recentPunishments.delete(recentPunishments.keys().next().value);
  return true;
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

// Invites. No nested quantifiers. Global flag: use only through matchAll (findInvites).
export const INVITE_RE = /(?<![\w.-])(?:https?:\/\/)?(?:(?:www|ptb|canary)\.)?(?:discord(?:app)?\.com\/invite|discord\.gg)\/([a-z0-9-]{2,32})/gi;

/** Every invite code in `text` (original case). */
export function findInvites(text) {
  return [...String(text ?? '').matchAll(INVITE_RE)].map((m) => m[1]);
}

// Links: only scheme URLs and www. hosts. A bare "word.word" is ignored (R126).
const URL_RE = /\bhttps?:\/\/[^\s<>"'`|]+/gi;
const WWW_RE = /(?<![\w./-])www\.[a-z0-9-]+(?:\.[a-z0-9-]+)+/gi;

/** An exact or dot-suffix match on a parsed hostname (R127): netflix.com is not x.com. */
export function hostMatches(host, domain) {
  return host === domain || host.endsWith(`.${domain}`);
}

/** The distinct hostnames of the links in `text`. Discord invite URLs belong to the invites rule. */
export function extractLinkHosts(text) {
  const out = [];
  const push = (raw) => {
    try {
      const u = new URL(raw);
      const host = u.hostname.toLowerCase().replace(/\.$/, '');
      if (!host) return;
      if (host === 'discord.gg'
        || ((hostMatches(host, 'discord.com') || hostMatches(host, 'discordapp.com')) && u.pathname.startsWith('/invite/'))) return;
      if (!out.includes(host)) out.push(host);
    } catch { /* not a URL */ }
  };
  const s = String(text ?? '');
  for (const m of s.matchAll(URL_RE)) push(m[0].replace(/[)\].,!?:;'"]+$/, ''));
  for (const m of s.matchAll(WWW_RE)) push(`https://${m[0]}`);
  return out;
}

/** Capitals share: ignores custom emoji, mentions and URLs; counts only cased letters. */
export function capsStats(content) {
  const t = String(content ?? '')
    .replace(/<a?:\w{2,32}:\d{17,20}>/g, '')
    .replace(/<(?:@[!&]?|#)\d{17,20}>/g, '')
    .replace(/\bhttps?:\/\/\S+/gi, '');
  const letters = (t.match(/[\p{Lu}\p{Ll}\p{Lt}]/gu) ?? []).length;
  const upper = (t.match(/[\p{Lu}\p{Lt}]/gu) ?? []).length;
  return { letters, upper, percent: letters ? Math.round((upper * 100) / letters) : 0 };
}

let segmenter = null;
const graphemeSegmenter = () => (segmenter ??= new Intl.Segmenter(undefined, { granularity: 'grapheme' }));

/** Custom emoji tokens plus grapheme clusters that hold a pictographic, a regional indicator or a keycap. */
export function countEmojis(content) {
  const s = String(content ?? '');
  const custom = (s.match(/<a?:\w{2,32}:\d{17,20}>/g) ?? []).length;
  const rest = s.replace(/<a?:\w{2,32}:\d{17,20}>/g, ' ');
  let n = 0;
  for (const { segment } of graphemeSegmenter().segment(rest)) {
    if (/\p{Extended_Pictographic}|\p{Regional_Indicator}|\u{20E3}/u.test(segment)) n++;
  }
  return custom + n;
}

/** Pairs of "||", plus attachments flagged as spoilers. */
export function countSpoilers(content, attachments) {
  const pairs = Math.floor((String(content ?? '').split('||').length - 1) / 2);
  let files = 0;
  for (const a of attachments?.values?.() ?? []) if (a?.spoiler) files++;
  return pairs + files;
}

/**
 * Parsed mentions only (R274): unique users other than the author, plus roles, plus 1 for a real
 * @everyone/@here ping. A reply ping does not count unless the content also mentions that user.
 */
export function countMentions(message) {
  const m = message?.mentions;
  if (!m) return 0;
  const authorId = message.author?.id;
  const content = String(message.content ?? '');
  let users = 0;
  for (const id of m.users?.keys?.() ?? []) if (id !== authorId) users++;
  const rid = m.repliedUser?.id;
  if (rid && rid !== authorId && m.users?.has?.(rid) && !content.includes(`<@${rid}>`) && !content.includes(`<@!${rid}>`)) users--;
  return users + (m.roles?.size ?? 0) + (m.everyone ? 1 : 0);
}

/** The longest run of combining marks after NFC, ignoring variation selectors and the keycap mark. */
export function maxCombiningRun(content) {
  const t = String(content ?? '').normalize('NFC').replace(/[\u{FE00}-\u{FE0F}\u{20E3}]|[\u{E0100}-\u{E01EF}]/gu, '');
  let max = 0;
  for (const m of t.matchAll(/[\p{Mn}\p{Me}]+/gu)) max = Math.max(max, [...m[0]].length);
  return max;
}

/** Fingerprint for repeated text; null for empty content, so images and stickers never count (R035/R130). */
export function duplicateFingerprint(content) {
  const fp = stripInvisible(String(content ?? '').normalize('NFKC')).toLowerCase().replace(/\s+/g, ' ').trim();
  return fp || null;
}

// ── Scope ─────────────────────────────────────────────────────────────────────

/** Channel ids a rule scope is compared against: thread -> [thread, parent, category]; channel -> [channel, category] (R135). */
export function scopeChannelIds(channel) {
  if (!channel) return [];
  const ids = [channel.id];
  const isThread = typeof channel.isThread === 'function' && channel.isThread();
  if (isThread && channel.parentId) ids.push(channel.parentId);
  const parent = isThread ? (channel.parent ?? channel.guild?.channels?.cache?.get(channel.parentId) ?? null) : null;
  const categoryId = isThread ? parent?.parentId : channel.parentId;
  if (categoryId) ids.push(categoryId);
  return ids;
}

/** Whether a rule applies to a member in a channel, by the rule's role and channel modes. */
export function ruleApplies(ruleCfg, { roleIds = [], channelIds = [] } = {}) {
  const roleHit = ruleCfg.roleIds.some((id) => roleIds.includes(id));
  const chanHit = ruleCfg.channelIds.some((id) => channelIds.includes(id));
  return (ruleCfg.roleMode === 'only' ? roleHit : !roleHit) && (ruleCfg.channelMode === 'only' ? chanHit : !chanHit);
}

/**
 * Staff bypass (R134): the owner, admins and immune roles (isImmune), MODERATOR level (mod roles,
 * Kick/Ban/Moderate Members, the bot owner) and members with Manage Messages. Fails safe: an
 * unexpected error counts as staff, so nothing is punished on a doubt.
 */
export function isStaff(member, settings) {
  try {
    if (!member) return false;
    if (isImmune(member, settings)) return true;
    if (resolvePermissionLevel(member, settings) >= PERMISSION_LEVELS.MODERATOR) return true;
    return Boolean(member.permissions?.has?.(PermissionFlagsBits.ManageMessages));
  } catch {
    return true;
  }
}

// ── Rule names and descriptions ───────────────────────────────────────────────

const RULE_ALIASES = Object.freeze({
  badwords: 'badWords', 'bad words': 'badWords',
  repeated: 'repeatedText', 'repeated text': 'repeatedText', duplicates: 'repeatedText', duplicate: 'repeatedText',
  invite: 'invites', 'server invites': 'invites',
  link: 'links', 'external links': 'links',
  capitals: 'caps', 'excessive caps': 'caps',
  emoji: 'emojis', 'excessive emojis': 'emojis',
  spoiler: 'spoilers', 'excessive spoilers': 'spoilers',
  mention: 'mentions', 'excessive mentions': 'mentions',
  'anti-spam': 'spam', antispam: 'spam', rate: 'spam',
});

/** Resolves a rule key, label or alias (also "Label (key)" as pasted from autocomplete) to a rule key, or null. */
export function resolveRuleKey(input) {
  const byName = new Map();
  for (const k of RULE_KEYS) {
    byName.set(k.toLowerCase(), k);
    byName.set(RULES[k].label.toLowerCase(), k);
  }
  const find = (s) => byName.get(s) ?? (Object.hasOwn(RULE_ALIASES, s) ? RULE_ALIASES[s] : null);
  const raw = String(input ?? '').trim().toLowerCase();
  if (!raw) return null;
  const stripped = raw.replace(/\s*\([^)]*\)\s*$/, '').replace(/^[^\p{L}\p{N}]+/u, '').trim();
  return find(raw) ?? find(stripped);
}

/** The threshold of a rule in words, e.g. "≥ 70% capitals". */
export function formatThreshold(key, value) {
  switch (key) {
    case 'caps': return `≥ ${value}% capitals`;
    case 'emojis': return `≥ ${value} emojis`;
    case 'spoilers': return `≥ ${value} spoilers`;
    case 'mentions': return `≥ ${value} mentions`;
    case 'zalgo': return `≥ ${value} stacked marks`;
    case 'repeatedText': return `${value} identical messages in 30 s`;
    case 'spam': return `${value} messages in 5 s`;
    default: return String(value);
  }
}

/** One status line for a rule: emoji, label, action, then the optional threshold, timeout and scope parts. */
export function describeRuleLine(key, ruleCfg, config) {
  const meta = RULES[key];
  const r = ruleCfg ?? defaultRule(key);
  let line = `${meta.emoji} **${meta.label}** — ${ACTION_LABELS[r.action] ?? r.action}`;
  if (meta.threshold && r.action !== 'off') line += ` · ${formatThreshold(key, r.threshold)}`;
  if (r.action === 'timeout') line += ` · ${formatDurationLong(r.timeoutSeconds * 1000)}`;
  if (r.roleIds.length > 0 || r.roleMode === 'only') line += ` · roles: ${r.roleMode} ${r.roleIds.length}`;
  if (r.channelIds.length > 0 || r.channelMode === 'only') line += ` · channels: ${r.channelMode} ${r.channelIds.length}`;
  if (key === 'badWords') line += ` · ${config?.badWords?.length ?? 0} words, ${config?.badWordPatterns?.length ?? 0} patterns`;
  return line;
}

/** The sentence that explains what the External Links rule does with the current domain lists. */
export function describeLinksMode(config) {
  return (config?.blockedDomains?.length ?? 0) === 0
    ? 'Links: every link is blocked except allowed domains.'
    : 'Links: only blocked domains are removed.';
}

// ── Detectors ─────────────────────────────────────────────────────────────────

function makeMatch(config, key, detail) {
  const r = config.rules[key];
  return { rule: key, action: r.action, detail: clampText(detail, 200), timeoutSeconds: r.timeoutSeconds };
}

/**
 * Bad words (whole-word, R034) then regex patterns (sandboxed, R128).
 * @returns {{ rule: string, action: string, detail: string, timeoutSeconds: number }|null}
 */
export function checkBadWords(content, config, matchers) {
  const t = textForWords(content);
  if (!t) return null;
  if (matchers?.badWordsRe) {
    const m = matchers.badWordsRe.exec(t);
    if (m) return makeMatch(config, 'badWords', `word "${clampText(m[0], 40)}"`);
  } else if (matchers?.fallbackWords) {
    const w = matchers.fallbackWords.find((word) => containsWord(t, word));
    if (w !== undefined) return makeMatch(config, 'badWords', `word "${clampText(w, 40)}"`);
  }
  if (matchers?.patterns?.length) {
    const k = safeRegexFirstMatch(matchers.patterns, t, { timeoutMs: 50, inputCap: PATTERN_INPUT_CAP });
    if (k >= 0) return makeMatch(config, 'badWords', `pattern #${matchers.patternIndex[k] + 1}`);
  }
  return null;
}

function checkInvites(content, config, guild) {
  const vanity = typeof guild?.vanityURLCode === 'string' ? guild.vanityURLCode.toLowerCase() : null;
  for (const code of findInvites(textForLinks(content))) {
    if (vanity && code.toLowerCase() === vanity) continue;
    if (guild?.invites?.cache?.has?.(code)) continue; // this guild's own invite; no API call
    return makeMatch(config, 'invites', `invite discord.gg/${code}`);
  }
  return null;
}

function checkLinks(content, config) {
  const allowed = config.allowedDomains;
  const blocked = config.blockedDomains;
  for (const host of extractLinkHosts(textForLinks(content))) {
    if (allowed.some((d) => hostMatches(host, d))) continue;
    // No blocked domain at all: allowlist mode, every non-allowed link is a violation.
    if (blocked.length === 0 || blocked.some((d) => hostMatches(host, d))) {
      return makeMatch(config, 'links', `link to ${host}`);
    }
  }
  return null;
}

const CONTENT_DETECTORS = Object.freeze({
  badWords: (message, config, matchers) => checkBadWords(message.content, config, matchers),
  invites: (message, config, _matchers, guild) => checkInvites(message.content, config, guild),
  links: (message, config) => checkLinks(message.content, config),
  caps: (message, config) => {
    const { letters, percent } = capsStats(message.content);
    return letters >= CAPS_MIN_LETTERS && percent >= config.rules.caps.threshold
      ? makeMatch(config, 'caps', `${percent}% capitals`) : null;
  },
  emojis: (message, config) => {
    const n = countEmojis(message.content);
    const limit = config.rules.emojis.threshold;
    return n >= limit ? makeMatch(config, 'emojis', `${n} emojis (limit ${limit})`) : null;
  },
  spoilers: (message, config) => {
    const n = countSpoilers(message.content, message.attachments);
    const limit = config.rules.spoilers.threshold;
    return n >= limit ? makeMatch(config, 'spoilers', `${n} spoilers (limit ${limit})`) : null;
  },
  mentions: (message, config) => {
    const n = countMentions(message);
    const limit = config.rules.mentions.threshold;
    return n >= limit ? makeMatch(config, 'mentions', `${n} mentions (limit ${limit})`) : null;
  },
  zalgo: (message, config) => {
    const n = maxCombiningRun(message.content);
    return n >= config.rules.zalgo.threshold ? makeMatch(config, 'zalgo', `zalgo text (${n} stacked marks)`) : null;
  },
});

/**
 * Runs the content rules in `activeKeys`, in RULE_KEYS order. Synchronous, no I/O (patterns run in a
 * bounded vm call). Empty content skips every rule except spoilers (spoiler attachments).
 * @returns {Array<{ rule: string, action: string, detail: string, timeoutSeconds: number }>}
 */
export function evaluateContentRules(message, activeKeys, config, matchers, guild) {
  const out = [];
  const hasText = String(message?.content ?? '').length > 0;
  for (const key of RULE_KEYS) {
    if (!activeKeys.includes(key) || !Object.hasOwn(CONTENT_DETECTORS, key)) continue;
    if (!hasText && key !== 'spoilers') continue;
    try {
      const m = CONTENT_DETECTORS[key](message, config, matchers, guild);
      if (m) out.push(m);
    } catch (err) {
      logger.warn(`Automod rule ${key} failed: ${err?.message ?? err}`);
    }
  }
  return out;
}

const sha16 = (text) => createHash('sha1').update(text).digest('hex').slice(0, 16);

/** Repeated text over a Redis sliding window. null count (Redis down) skips the rule: fail open. */
async function checkRepeatedText(message, config, guildId) {
  const fp = duplicateFingerprint(message.content);
  if (!fp || !cache.available()) return null;
  const key = `automod:dup:${guildId}:${message.author.id}:${sha16(fp)}`;
  const count = await cache.slidingWindow(key, REPEAT_WINDOW_MS, message.id);
  if (count === null || count === undefined || Number.isNaN(count)) return null;
  return count >= config.rules.repeatedText.threshold
    ? makeMatch(config, 'repeatedText', `same message ${count} times in 30 s`) : null;
}

/** Message rate over a Redis sliding window. Every message counts, whatever it holds. Fails open. */
async function checkSpam(message, config, guildId) {
  if (!cache.available()) return null;
  const key = `automod:rate:${guildId}:${message.author.id}`;
  const count = await cache.slidingWindow(key, SPAM_WINDOW_MS, message.id);
  if (count === null || count === undefined || Number.isNaN(count)) return null;
  return count >= config.rules.spam.threshold
    ? makeMatch(config, 'spam', `${count} messages in 5 s`) : null;
}

// ── Outcome resolution ────────────────────────────────────────────────────────

/**
 * Several matches still mean one action: delete when any rule asks for it, apply the strongest
 * punishment (ties go to the first rule in RULE_KEYS order).
 * @param {Array<{ rule: string, action: string, detail?: string, timeoutSeconds?: number }>} matches
 */
export function resolveOutcome(matches) {
  if (!matches?.length) return null;
  const ordered = [...matches].sort((a, b) => RULE_KEYS.indexOf(a.rule) - RULE_KEYS.indexOf(b.rule));
  const rank = (m) => PUNISHMENT_RANK[PUNISHMENTS[m.action]] ?? 0;
  const deletes = ordered.some((m) => DELETING_ACTIONS.has(m.action));
  const best = ordered.reduce((acc, m) => (rank(m) > rank(acc) ? m : acc), ordered[0]);
  const primary = rank(best) > 0 ? best : (ordered.find((m) => DELETING_ACTIONS.has(m.action)) ?? ordered[0]);
  const punishment = rank(primary) > 0 ? PUNISHMENTS[primary.action] : null;
  const timeoutSeconds = punishment === 'TIMEOUT'
    ? Math.max(...ordered.filter((m) => m.action === 'timeout').map((m) => m.timeoutSeconds ?? DEFAULT_TIMEOUT_SECONDS))
    : null;
  return {
    primary,
    others: ordered.filter((m) => m !== primary),
    deletes,
    punishment,
    punishmentNeedsDelete: DELETING_ACTIONS.has(primary.action),
    timeoutSeconds,
  };
}

const ruleLabel = (key) => RULES[key]?.label ?? String(key);

/** The audit-log reason: "[Automod] Label: detail (also: …) in #channel", at most 500 characters. */
export function buildReason(outcome, channelName) {
  const also = outcome.others.length ? ` (also: ${outcome.others.map((m) => ruleLabel(m.rule)).join(', ')})` : '';
  return clampText(
    `[Automod] ${ruleLabel(outcome.primary.rule)}: ${outcome.primary.detail ?? ''}${also} in #${channelName}`,
    500,
  );
}

/**
 * The `logFields` handed to executeModAction so moderation's case embed carries the evidence (R275).
 * Always exactly three entries, every value at most 1024 characters.
 * @param {string} ruleOrLabel a rule key or an already formatted label
 * @param {string} detail
 * @param {string} content the message text
 * @param {{ id?: string }|string|null} channel
 */
export function buildPunishmentLogFields(ruleOrLabel, detail, content, channel) {
  const label = RULES[ruleOrLabel] ? `${RULES[ruleOrLabel].emoji} ${RULES[ruleOrLabel].label}` : String(ruleOrLabel ?? 'Automod');
  const channelId = typeof channel === 'string' ? channel : channel?.id;
  const where = channelId ? ` in <#${channelId}>` : '';
  return [
    { name: 'Rule', value: clampText(label, 1024) },
    { name: 'Detail', value: clampText(`${detail ?? ''}${where}`, 1024) || '—' },
    { name: 'Content', value: clampText(content || '*[no text]*', 1024) },
  ];
}

// ── Staff-facing output ───────────────────────────────────────────────────────

/**
 * Posts an embed to the configured mod-log channel. Never throws.
 * @returns {Promise<void>}
 */
export async function postModLog(guild, settings, embed) {
  try {
    const id = settings?.modLogChannelId;
    if (!id) return;
    const channel = guild.channels?.cache?.get(id);
    const usable = checkChannelUsable(channel);
    if (!usable.ok) {
      warnOnce(`modlog:${guild.id}`, `Automod cannot post to the mod-log channel in ${guild.id}: ${usable.reason ?? 'unusable'}`);
      return;
    }
    await channel.send({ embeds: [embed], allowedMentions: { parse: [] } });
  } catch (err) {
    if ([10003, 50001, 50013].includes(err?.code)) {
      warnOnce(`modlog:${guild?.id}`, `Automod cannot post to the mod-log channel in ${guild?.id} (code ${err.code})`);
    } else {
      logger.error('Automod mod-log post failed', err);
    }
  }
}

function buildLogEmbed(kind, { title, message, isEdit, outcome, problem, footerExtra = '' }) {
  const author = message.author;
  const content = String(message.content ?? '');
  const attachments = message.attachments?.size ?? 0;
  const primary = RULES[outcome.primary.rule];
  const embed = (kind === 'warn' ? warnEmbed : errorEmbed)(
    title,
    undefined,
    clampText(`User ID: ${author.id} • Message ID: ${message.id}${footerExtra}`, 2048),
  );
  const also = outcome.others.length ? `\nAlso matched: ${outcome.others.map((m) => ruleLabel(m.rule)).join(', ')}` : '';
  embed.addFields(
    { name: 'Member', value: clampText(`<@${author.id}> \`${author.tag ?? author.username ?? author.id}\``, 1024), inline: true },
    { name: 'Channel', value: clampText(`<#${message.channelId ?? message.channel?.id}>${isEdit ? ' (edited message)' : ''}`, 1024), inline: true },
    { name: 'Rule', value: clampText(`${primary?.emoji ?? ''} ${ruleLabel(outcome.primary.rule)}${also}`.trim(), 1024) },
    { name: 'Detail', value: clampText(outcome.primary.detail || '—', 1024) },
    { name: 'Content', value: clampText((content || '*[no text]*') + (attachments ? `\n📎 ${attachments} attachment(s)` : ''), 1024) },
  );
  if (problem) embed.addFields({ name: 'Problem', value: clampText(problem, 1024) });
  return embed;
}

/**
 * The short in-channel notice: { text, mention }. The matched word is never echoed.
 * @param {{ deleted: boolean, status: string|null, punishment: string|null, author: { id: string, username?: string }, label: string, timeoutSeconds?: number|null }} p
 */
export function buildNoticeText({ deleted, status, punishment, author, label, timeoutSeconds }) {
  const mention = `<@${author.id}>`;
  if (status === 'applied') {
    if (punishment === 'KICK' || punishment === 'BAN') {
      return { text: `🛡\u{FE0F} **${clampText(author.username ?? author.id, 32)}** was ${punishment === 'KICK' ? 'kicked' : 'banned'} by automod: **${label}**.`, mention: false };
    }
    if (!deleted) return { text: `⚠\u{FE0F} ${mention}, you have been warned: **${label}**.`, mention: true };
    if (punishment === 'TIMEOUT') {
      return { text: `🛡\u{FE0F} ${mention}, your message was removed and you have been timed out for ${formatDurationLong(timeoutSeconds * 1000)}: **${label}**.`, mention: true };
    }
    return { text: `🛡\u{FE0F} ${mention}, your message was removed and you have been warned: **${label}**.`, mention: true };
  }
  return { text: `🛡\u{FE0F} ${mention}, your message was removed: **${label}**.`, mention: true };
}

async function sendNotice(message, notice) {
  try {
    const channel = message.channel;
    if (!checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] }).ok) return;
    const sent = await channel.send({
      content: clampText(notice.text, 2000),
      allowedMentions: { users: notice.mention ? [message.author.id] : [] },
    });
    const timer = setTimeout(() => {
      try {
        markBotDeletion(sent.id, 'automod');
        sent.delete().catch(() => {});
      } catch { /* ignore */ }
    }, NOTICE_TTL_MS);
    timer.unref?.();
  } catch { /* a notice is a nicety: ignore any failure */ }
}

// ── Applying an outcome ───────────────────────────────────────────────────────

async function deleteOffender(ctx, message, guild) {
  const state = { deleted: false, gone: false, problem: null };
  const channel = message.channel;
  const me = guild.members?.me;
  const perms = me && typeof channel?.permissionsFor === 'function' ? channel.permissionsFor(me) : null; // R132
  if (!perms?.has(PermissionFlagsBits.ManageMessages)) {
    state.problem = 'missing';
    warnOnce(`perm:${guild.id}:${channel?.id}`, `Automod cannot delete in #${channel?.name} (${guild.id}): missing Manage Messages`);
    return state;
  }
  markBotDeletion(message.id, 'automod'); // always before the delete call, so log hooks can label it
  try {
    await message.delete();
    state.deleted = true;
  } catch (err) {
    if (err?.code === 10008 || err?.code === 10003) {
      state.deleted = true;
      state.gone = true; // already gone
    } else if (err?.code === 50013 || err?.code === 50001) {
      state.problem = 'missing';
    } else {
      state.problem = 'error';
      logger.warn(`Automod could not delete message ${message.id} in ${guild.id}: ${err?.message ?? err}`);
    }
  }
  return state;
}

async function applyOutcome(ctx, { message, guild, member, settings, outcome, isEdit }) {
  const author = message.author;
  const channel = message.channel;
  const del = outcome.deletes ? await deleteOffender(ctx, message, guild) : { deleted: false, gone: false, problem: null };
  const { deleted, gone } = del;

  // A message that is gone must not reach XP, triggers or translation. Never stop when it survived.
  if (deleted) ctx.stop?.('automod');
  if (ctx.state) ctx.state.automodAction = { rule: outcome.primary.rule, deleted };

  // Punishment.
  let status = null;
  let result = null;
  if (outcome.punishment) {
    if (outcome.punishmentNeedsDelete && !deleted) status = 'skipped-delete-failed'; // R132: a failed delete never punishes
    else if (!member) status = 'no-member';
    else if (!claimPunishment(guild.id, member.id, PUNISHMENT_RANK[outcome.punishment])) status = 'suppressed';
    else {
      try {
        result = await executeModAction({
          guild,
          target: member,
          moderator: ctx.client.user,
          action: outcome.punishment,
          reason: buildReason(outcome, channel?.name ?? 'unknown'),
          ...(outcome.punishment === 'TIMEOUT' && { durationMs: outcome.timeoutSeconds * 1000 }),
          ...(outcome.punishment === 'BAN' && { deleteMessageSeconds: BAN_DELETE_MESSAGE_SECONDS }),
          source: 'automod',
          logFields: buildPunishmentLogFields(
            `${RULES[outcome.primary.rule]?.emoji ?? ''} ${ruleLabel(outcome.primary.rule)}${outcome.others.length ? ` (also: ${outcome.others.map((m) => ruleLabel(m.rule)).join(', ')})` : ''}`.trim(),
            outcome.primary.detail,
            message.content,
            channel,
          ),
        });
      } catch (err) {
        result = { ok: false, code: 'ERROR', error: err?.message ?? String(err) };
      }
      status = result?.ok === true ? 'applied' : 'failed';
    }
  }

  // In-channel notice (a short-lived nicety).
  const label = ruleLabel(outcome.primary.rule);
  if ((deleted && !gone) || status === 'applied') {
    if (noticeThrottle(`${guild.id}:${author.id}`)) {
      await sendNotice(message, buildNoticeText({
        deleted, status, punishment: outcome.punishment, author, label, timeoutSeconds: outcome.timeoutSeconds,
      }));
    }
  }

  // Mod log: exactly one embed per handled message at most (R275).
  const logArgs = { message, isEdit, outcome };
  const word = PUNISHMENT_WORD[outcome.punishment] ?? 'Action';
  let embed = null;
  if (status === 'applied' || status === 'suppressed') {
    embed = null; // moderation's case embed is the only one, or one per burst is enough
  } else if (deleted && !outcome.punishment) {
    if (deleteLogThrottle(`${guild.id}:${author.id}:${outcome.primary.rule}`)) {
      embed = buildLogEmbed('warn', {
        ...logArgs,
        title: 'Automod — Message Deleted',
        footerExtra: ' • Further removals for this member and rule are not logged for 10 s',
      });
    }
  } else if (deleted && (status === 'failed' || status === 'no-member')) {
    embed = buildLogEmbed('error', {
      ...logArgs,
      title: `Automod — Message Deleted, ${word} Not Applied`,
      problem: status === 'no-member'
        ? 'The member is no longer in the server; no punishment was applied.'
        : `${result?.code ?? 'ERROR'}: ${clampText(result?.error ?? 'unknown error', 300)}`,
    });
  } else if (del.problem) {
    if (notDeletedThrottle(`${guild.id}:${channel?.id}`)) {
      embed = buildLogEmbed('error', {
        ...logArgs,
        title: 'Automod — Message NOT Deleted',
        problem: del.problem === 'missing'
          ? `I need **Manage Messages** in <#${channel?.id}>. The message is still there: [jump](${message.url}). No punishment was applied.`
          : `Deleting failed (Discord error). The message may still be there: [jump](${message.url}). No punishment was applied.`,
      });
    }
  } else if (!outcome.deletes && outcome.punishment && status === 'failed') {
    embed = buildLogEmbed('error', {
      ...logArgs,
      title: 'Automod — Warn Not Applied',
      problem: `${result?.code ?? 'ERROR'}: ${clampText(result?.error ?? 'unknown error', 300)}`,
    });
  }
  if (embed) await postModLog(guild, settings, embed);
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

/** Looks a member up. Returns { member } (null when they left) or { error } for a failure that proves nothing. */
async function resolveMember(ctx, message, guild) {
  const known = ctx.member ?? message.member ?? null;
  if (known) return { member: known };
  try {
    return { member: await guild.members.fetch(message.author.id) };
  } catch (err) {
    // 10007 Unknown Member / 10013 Unknown User: the author left, so they are not staff. Anything
    // else (rate limit, 5xx) proves nothing, and nothing destructive may run on a doubt (rule 20).
    if (err?.code === 10007 || err?.code === 10013) return { member: null };
    return { error: err };
  }
}

/**
 * Runs every automod rule against a message (messageCreate) or an edit (messageUpdate). Never throws.
 * @param {object} ctx HookContext: guild, message, member, client, getSettings(), stop(), state, oldMessage, oldKnown
 * @param {{ isEdit?: boolean }} [opts]
 * @returns {Promise<void>}
 */
export async function runAutomod(ctx, { isEdit = false } = {}) {
  try {
    const message = ctx?.message;
    const guild = ctx?.guild;
    if (!message || !guild || !message.author) return;
    if (message.author.bot || message.webhookId || message.system) return;

    const settings = typeof ctx.getSettings === 'function' ? await ctx.getSettings() : null;
    if (!settings?.automodEnabled) return;
    const { config, matchers } = getAutomodConfig(settings);
    if (activeRuleKeys(config).length === 0) return;

    // Exempt channel (R135): thread, its parent and the category are all compared.
    const channelIds = scopeChannelIds(message.channel);
    if (channelIds.some((id) => config.exemptChannelIds.includes(id))) return;

    if (message.author.id === guild.ownerId) return;

    const found = await resolveMember(ctx, message, guild);
    if (found.error) {
      warnOnce(`member:${guild.id}`, `Automod could not look up a member in ${guild.id}: ${found.error?.message ?? found.error}`);
      return;
    }
    const member = found.member;
    if (member && isStaff(member, settings)) return;

    const roleIds = member?.roles?.cache ? [...member.roles.cache.keys()] : [];
    if (roleIds.some((id) => config.exemptRoleIds.includes(id))) return;

    const env = { roleIds, channelIds };
    const active = RULE_KEYS.filter((k) => config.rules[k].action !== 'off'
      && ruleApplies(config.rules[k], env)
      && (!isEdit || RULES[k].onEdit));
    if (active.length === 0) return;

    let matches = evaluateContentRules(message, active, config, matchers, guild);

    // A rule the old content already broke is not punished again (R119): a typo fix must not re-warn.
    if (isEdit && ctx.oldKnown && ctx.oldMessage) {
      const old = evaluateContentRules(ctx.oldMessage, active, config, matchers, guild);
      matches = matches.filter((m) => !old.some((o) => o.rule === m.rule));
    }

    if (!isEdit) {
      const redisChecks = [];
      if (active.includes('repeatedText')) redisChecks.push(checkRepeatedText(message, config, guild.id));
      if (active.includes('spam')) redisChecks.push(checkSpam(message, config, guild.id));
      for (const m of await Promise.all(redisChecks)) if (m) matches.push(m); // null = Redis down: fail open
    }

    const outcome = resolveOutcome(matches);
    if (!outcome) return;
    await applyOutcome(ctx, { message, guild, member, settings, outcome, isEdit });
  } catch (err) {
    logger.error(`Automod failed in guild ${ctx?.guildId ?? ctx?.guild?.id}`, err);
  }
}
