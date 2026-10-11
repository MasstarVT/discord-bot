// Automations engine (When -> If -> Do), pkg-automations.md.
//
// This file holds: constants and the trigger / condition / action tables, the pure helpers (normalisers, validators, the condition
// evaluator, presets, JSON import/export, descriptions), the per-guild cache and the CRUD with row locks, the option-to-parameter
// mapping that /automation uses, the dispatcher, the action executor and every hook body. It is side-effect free at import: the only
// module-scope state is the pure constructors listed at "Module-scope state" below (no timers, no I/O, no registration).
//
// Degrade decisions (fail-open / fail-closed) are written at each call site.
import {
  ActionRowBuilder, ButtonBuilder, ButtonStyle, ChannelType, MessageType, RESTJSONErrorCodes,
} from 'discord.js';
import { Prisma } from '@prisma/client';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import { ensureGuildSettings, getSettingsSafe } from './settingsService.js';
import * as rewards from './rewards.js';
import { getSavedEmbedTemplate, searchSavedEmbeds } from './embedService.js';
import { COLORS, LIMITS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { renderPlaceholders } from '../utils/placeholderParser.js';
import { isHttpUrl, renderTemplate } from '../utils/customEmbed.js';
import { compileSafeRegex, containsWord, safeRegexTest, validateRegex } from '../utils/safeRegex.js';
import {
  checkAssignableRole, checkChannelUsable, missingBotPermissions, pingableRoleIds, permissionNames,
} from '../utils/interactionHelpers.js';
import { markBotDeletion, wasDeletedByBot } from '../utils/deletionTracker.js';
import { clampText, formatNumber, joinWithLimit } from '../utils/text.js';
import { guildEmbed } from '../utils/embedBuilder.js';
import { TimeoutError, createKeyedMutex, createThrottle, withTimeout } from '../utils/async.js';
import logger from '../utils/logger.js';

// ── Constants ────────────────────────────────────────────────────────────────

export const AUTOMATION_NAME_MAX = 32;
export const IMPORT_MAX_BYTES = 32_768;
export const EXPORT_FORMAT = 'automation';
export const EXPORT_VERSION = 1;
export const MAX_CHAIN_DEPTH = 3;
export const CAUSE_TTL_MS = 15_000;
export const RUN_BUDGET_MAX = 30;
export const RUN_BUDGET_WINDOW_MS = 10_000;
export const MAX_PENDING_RUNS = 25;
export const RUN_TIMEOUT_MS = 30_000;
export const MEMO_TTL_MS = 60_000;
export const CACHE_TTL_SECONDS = 300;
export const REPOST_DEDUPE_TTL_SECONDS = 2_592_000;
export const PROBLEM_TTL_SECONDS = 604_800;
export const MAX_COOLDOWN_SECONDS = 604_800;
export const MAX_REWARD_AMOUNT = 1_000_000;
export const THREAD_ARCHIVE_MINUTES = Object.freeze([60, 1440, 4320, 10080]);
export const DEFAULT_THREAD_NAME = "{user.name}'s thread";
export const WELCOMER_DEFAULT_TEXT = 'Hey {user}, welcome to **{server}**!';
export const PRESETS = Object.freeze(['welcomer', 'starboard', 'thread-joiner']);

export const MAX_BUTTON_INDEX = 99;
const MEMO_MAX_GUILDS = 5_000;
const MAX_IMPORT_MEMBER_FETCHES = 25;
const MAX_STORED_ELEMENTS = 100;          // a stored array longer than this is a corrupt row: the tail is ignored
const CONTENT_MAX = 2000;
const THREAD_NAME_MAX = 100;
const BUTTON_LABEL_MAX = 80;
const BUTTON_URL_MAX = 512;
const DEFAULT_STAR = '⭐';
const DEFAULT_THRESHOLD = 3;
const BUTTON_STYLES = Object.freeze(['PRIMARY', 'SECONDARY', 'SUCCESS', 'DANGER']);
const AUTOMATION_ID_RE = /^[a-z0-9]{8,40}$/i;
const SNOWFLAKE_RE = /^\d{17,20}$/;
const CUSTOM_EMOJI_RE = /^<(a?):(\w{2,32}):(\d{17,20})>$/;
const FE0F_RE = /️/g;

export const automationsCacheKey = (guildId) => `guild:${guildId}:automations`;
const cooldownKey = (guildId, automationId, userId) => `automation:cd:${guildId}:${automationId}:${userId}`;
const repostKey = (automationId, messageId) => `automation:repost:${automationId}:${messageId}`;
const problemKey = (automationId) => `automation:problem:${automationId}`;

/**
 * An error whose `.message` is shown to the admin. `.code` lets the command choose the embed title:
 * 'INVALID' (default), 'NOTFOUND', 'LIMIT', 'NOACTIONS', 'BUTTONS'.
 */
export class AutomationError extends Error {
  constructor(message, code = 'INVALID') {
    super(message);
    this.name = 'AutomationError';
    this.code = code;
  }
}

/** The exact user-facing texts shared by the service and the command (spec 4.7). */
export const ERROR_TEXTS = Object.freeze({
  name: 'Automation names can use letters, numbers, - and _ (up to 32 characters).',
  notFound: (name) => `There is no automation called **${name}**. See \`/automation list\`.`,
  taken: (name) => `An automation called **${name}** already exists.`,
  limit: () => `This server already has ${LIMITS.AUTOMATIONS} automations, the maximum. Delete one with \`/automation delete\` first.`,
  maxConditions: () => `An automation can have at most ${LIMITS.AUTOMATION_CONDITIONS} conditions.`,
  maxActions: () => `An automation can have at most ${LIMITS.AUTOMATION_ACTIONS} actions.`,
  buttonsUsed: 'This automation has used all 99 button numbers. Create a new automation for more buttons.',
  needsActions: 'Add at least one action before turning it on.',
});

/** @returns {boolean} true for a Prisma unique-constraint violation (P2002) */
export const isUniqueViolation = (err) => err instanceof Prisma.PrismaClientKnownRequestError && err.code === 'P2002';

// ── Raw SQL (00-overview rule 22; builders are exported as RAW_SQL) ──────────

const lockGuildSql = (guildId) => Prisma.sql`SELECT 1 AS ok FROM "GuildSettings" WHERE "guildId" = ${guildId} FOR UPDATE`;
const lockByNameSql = (guildId, name) => Prisma.sql`SELECT "id" FROM "Automation" WHERE "guildId" = ${guildId} AND "name" = ${name} FOR UPDATE`;
const lockByIdSql = (guildId, id) => Prisma.sql`SELECT "id" FROM "Automation" WHERE "guildId" = ${guildId} AND "id" = ${id} FOR UPDATE`;
// uses is an int4 column: the maths run in bigint and the result is clamped to 2147483647 (no updatedAt change, no timestamp involved).
const usesSql = (id) => Prisma.sql`UPDATE "Automation" SET "uses" = LEAST("uses"::bigint + 1, 2147483647)::int WHERE "id" = ${id}`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'automation.lockGuildSettings', build: () => lockGuildSql('0') },
  { name: 'automation.lockByName', build: () => lockByNameSql('0', 'x') },
  { name: 'automation.lockById', build: () => lockByIdSql('0', 'x') },
  { name: 'automation.incrementUses', build: () => usesSql('0') },
]);

// ── Triggers (spec 3.2) ──────────────────────────────────────────────────────

function frozenSet(items) {
  const s = new Set(items);
  for (const m of ['add', 'delete', 'clear']) {
    Object.defineProperty(s, m, { value() { throw new TypeError('This set is immutable'); } });
  }
  return Object.freeze(s);
}

/** Trigger table. `channel`: whether the trigger happens in a channel (the trigger channel is the default send target). */
export const TRIGGERS = Object.freeze({
  MESSAGE_SENT: Object.freeze({ label: 'Message sent', channel: true }),
  MESSAGE_DELETED: Object.freeze({ label: 'Message deleted', channel: true }),
  MESSAGE_EDITED: Object.freeze({ label: 'Message edited', channel: true }),
  BUTTON_CLICKED: Object.freeze({ label: 'Button clicked', channel: true }),
  REACTION_ADDED: Object.freeze({ label: 'Reaction added', channel: true }),
  ROLE_GAINED: Object.freeze({ label: 'Role gained', channel: false }),
  ROLE_LOST: Object.freeze({ label: 'Role lost', channel: false }),
  VOICE_JOINED: Object.freeze({ label: 'Joined voice', channel: true }),
  VOICE_LEFT: Object.freeze({ label: 'Left voice', channel: true }),
  THREAD_CREATED: Object.freeze({ label: 'Thread created', channel: true }),
  MEMBER_JOINED: Object.freeze({ label: 'Member joined', channel: false }),
});

export const T_ALL = frozenSet(Object.keys(TRIGGERS));
export const T_CHANNEL = frozenSet(['MESSAGE_SENT', 'MESSAGE_DELETED', 'MESSAGE_EDITED', 'BUTTON_CLICKED', 'REACTION_ADDED', 'VOICE_JOINED', 'VOICE_LEFT', 'THREAD_CREATED']);
export const T_CONTENT = frozenSet(['MESSAGE_SENT', 'MESSAGE_DELETED', 'MESSAGE_EDITED', 'REACTION_ADDED']);
export const T_LIVE = frozenSet(['MESSAGE_SENT', 'MESSAGE_EDITED', 'REACTION_ADDED']);       // the user message still exists
export const T_COUNTS = frozenSet(['MESSAGE_DELETED', 'MESSAGE_EDITED', 'REACTION_ADDED']);   // reaction counts are meaningful
export const T_THREADY = frozenSet(['THREAD_CREATED', 'MESSAGE_SENT', 'MESSAGE_EDITED', 'REACTION_ADDED', 'BUTTON_CLICKED']);
const T_REPLYABLE = frozenSet([...T_LIVE, 'BUTTON_CLICKED']);
const T_REACTION = frozenSet(['REACTION_ADDED']);
const T_ROLE_GAINED = frozenSet(['ROLE_GAINED']);
const T_ROLE_LOST = frozenSet(['ROLE_LOST']);
const T_BUTTON = frozenSet(['BUTTON_CLICKED']);

const isTrigger = (t) => typeof t === 'string' && Object.hasOwn(TRIGGERS, t);

// ── Condition and action tables (spec 3.3) ───────────────────────────────────

const cdef = (label, triggers, list = null, max = null, extra = {}) => Object.freeze({ label, triggers, list, max, ...extra });

/** `list` names the array parameter that a repeated `condition add` of the same type extends; `max` is the per-list cap. */
export const CONDITION_TYPES = Object.freeze({
  CHANNEL_IN: cdef('Channel is one of', T_CHANNEL, 'channelIds', 25),
  CHANNEL_NOT_IN: cdef('Channel is not one of', T_CHANNEL, 'channelIds', 25),
  CATEGORY_IN: cdef('Category is one of', T_CHANNEL, 'categoryIds', 25),
  CATEGORY_NOT_IN: cdef('Category is not one of', T_CHANNEL, 'categoryIds', 25),
  CONTAINS: cdef('Message contains words', T_CONTENT, 'words', 25),
  EXACT: cdef('Message is exactly', T_CONTENT, 'texts', 25),
  NOT_CONTAINS: cdef('Message does not contain words', T_CONTENT, 'words', 25),
  MATCHES_REGEX: cdef('Message matches a regex', T_CONTENT),
  ATTACHMENT_AUDIO: cdef('Has an audio attachment', T_CONTENT, null, null, { noParams: true }),
  ATTACHMENT_IMAGE: cdef('Has an image attachment', T_CONTENT, null, null, { noParams: true }),
  ATTACHMENT_VIDEO: cdef('Has a video attachment', T_CONTENT, null, null, { noParams: true }),
  ATTACHMENT_TEXT: cdef('Has a text-file attachment', T_CONTENT, null, null, { noParams: true }),
  REACTED_WITH: cdef('Reacted with emoji', T_REACTION, 'emojis', 10),
  HAS_REACTIONS: cdef('Message has at least N reactions', T_COUNTS),
  IS_REPLY: cdef('Message is a reply', T_CONTENT, null, null, { noParams: true }),
  IS_NOT_REPLY: cdef('Message is not a reply', T_CONTENT, null, null, { noParams: true }),
  HAS_ANY_ROLE: cdef('Member has any of these roles', T_ALL, 'roleIds', 25),
  HAS_ALL_ROLES: cdef('Member has all of these roles', T_ALL, 'roleIds', 25),
  HAS_NOT_ANY_ROLE: cdef('Member has none of these roles', T_ALL, 'roleIds', 25),
  HAS_NOT_ALL_ROLES: cdef('Member lacks one of these roles', T_ALL, 'roleIds', 25),
  GAINED_ROLE: cdef('Gained one of these roles', T_ROLE_GAINED, 'roleIds', 25),
  LOST_ROLE: cdef('Lost one of these roles', T_ROLE_LOST, 'roleIds', 25),
  USER_IS: cdef('User is one of', T_ALL, 'userIds', 25),
  BUTTON_IS: cdef('Clicked this button', T_BUTTON, 'buttons', 25),
});

const adef = (label, triggers, extra = {}) => Object.freeze({ label, triggers, ...extra });

/**
 * `triggers` is the base applicability. Three types have extra, context-dependent rules in validateAutomationShape:
 * SEND_MESSAGE / SEND_EMBED need a channel outside T_CHANNEL, CREATE_THREAD needs one outside T_LIVE, and the thread actions
 * need T_THREADY or an earlier CREATE_THREAD.
 */
export const ACTION_TYPES = Object.freeze({
  CREATE_THREAD: adef('Create a thread', T_ALL),
  SEND_MESSAGE: adef('Send a message', T_ALL, { button: true }),
  REPLY: adef('Reply', T_REPLYABLE, { button: true }),
  REPOST: adef('Repost the message', T_CONTENT),
  PIN: adef('Pin the message', T_LIVE),
  DELETE: adef('Delete the message', T_LIVE),
  REACT: adef('React with an emoji', T_LIVE),
  REMOVE_REACTION: adef('Remove the reaction', T_REACTION),
  GIVE_ROLES: adef('Give roles', T_ALL),
  TAKE_ROLES: adef('Take roles', T_ALL),
  ADD_TO_THREAD: adef('Add members to the thread', T_ALL, { thread: true }),
  SEND_IN_THREAD: adef('Send a message in the thread', T_ALL, { button: true, thread: true }),
  GIVE_COINS: adef('Give coins', T_ALL, { reward: 'coins' }),
  TAKE_COINS: adef('Take coins', T_ALL, { reward: 'coins' }),
  GIVE_XP: adef('Give XP', T_ALL, { reward: 'xp' }),
  TAKE_XP: adef('Take XP', T_ALL, { reward: 'xp' }),
  SEND_EMBED: adef('Send a saved embed', T_ALL),
});

const isConditionType = (t) => typeof t === 'string' && Object.hasOwn(CONDITION_TYPES, t);
const isActionType = (t) => typeof t === 'string' && Object.hasOwn(ACTION_TYPES, t);

const NO_PARAM_CONDITIONS = frozenSet(Object.entries(CONDITION_TYPES).filter(([, d]) => d.noParams).map(([t]) => t));
// Conditions that read the message (content, attachments, reply flag or reaction counts): evaluated only after the message is known.
const NEEDS_MESSAGE = frozenSet([
  'CONTAINS', 'EXACT', 'NOT_CONTAINS', 'MATCHES_REGEX', 'ATTACHMENT_AUDIO', 'ATTACHMENT_IMAGE', 'ATTACHMENT_VIDEO',
  'ATTACHMENT_TEXT', 'IS_REPLY', 'IS_NOT_REPLY', 'HAS_REACTIONS',
]);

// ── Small pure helpers ───────────────────────────────────────────────────────

const ok = (value) => ({ ok: true, value });
const bad = (error) => ({ ok: false, error });
const lc = (s) => String(s).toLocaleLowerCase('en-US');
const stripFE0F = (s) => String(s ?? '').replace(FE0F_RE, '');

function isPlainObject(x) {
  if (x === null || typeof x !== 'object' || Array.isArray(x)) return false;
  const proto = Object.getPrototypeOf(x);
  return proto === Object.prototype || proto === null;
}

/** Normalised slug, letters / digits / - / _ up to 32 characters (copy of the embeds normaliser). */
export function normalizeAutomationName(input) {
  if (typeof input !== 'string') return null;
  const s = input.normalize('NFKD').replace(/\p{M}+/gu, '').trim().toLowerCase()
    .replace(/\s+/g, '-').replace(/[^a-z0-9_-]/g, '').replace(/-{2,}/g, '-')
    .replace(/^-+/, '').slice(0, AUTOMATION_NAME_MAX).replace(/-+$/, '');
  return s.length ? s : null;
}

/** 'Gained role' / 'gained-role' / 'GAINED_ROLE' -> 'GAINED_ROLE' (autocomplete values are already keys). */
export function normalizeTypeInput(str) {
  return String(str ?? '').trim().toUpperCase().replace(/[\s-]+/g, '_');
}

export function isSnowflake(str) {
  return typeof str === 'string' && SNOWFLAKE_RE.test(str);
}

/**
 * Ids from free text, in order, unique. `kind` picks which mention form counts: 'channel' (<#id>), 'role' (<@&id>) or
 * 'user' (<@id>, <@!id>); a bare snowflake is accepted for every kind.
 */
export function parseIdList(text, kind) {
  const out = [];
  const seen = new Set();
  const s = typeof text === 'string' ? text : '';
  const re = /<#(\d{17,20})>|<@&(\d{17,20})>|<@!?(\d{17,20})>|(?<!\d)(\d{17,20})(?!\d)/g;
  for (const m of s.matchAll(re)) {
    let id = null;
    if (m[1]) id = kind === 'channel' ? m[1] : null;
    else if (m[2]) id = kind === 'role' ? m[2] : null;
    else if (m[3]) id = kind === 'user' ? m[3] : null;
    else id = m[4];
    if (id && !seen.has(id)) { seen.add(id); out.push(id); }
  }
  return out;
}

/** Comma-separated words: trimmed, empties dropped, case-insensitively unique (first spelling kept). */
export function parseWords(text) {
  const out = [];
  const seen = new Set();
  for (const part of String(text ?? '').split(',')) {
    const w = part.trim();
    if (!w) continue;
    const k = lc(w);
    if (!seen.has(k)) { seen.add(k); out.push(w); }
  }
  return out;
}

// ── Emoji ────────────────────────────────────────────────────────────────────

const PICTOGRAPH_RE = /\p{Extended_Pictographic}|\p{Regional_Indicator}|[#*0-9]️?⃣/u;
let segmenter = null;
function graphemeCount(str) {
  segmenter ??= new Intl.Segmenter('en', { granularity: 'grapheme' });
  let n = 0;
  for (const _seg of segmenter.segment(str)) { n++; if (n > 1) break; }
  return n;
}

/** True for a stored emoji: `<:name:id>` / `<a:name:id>` or one unicode emoji grapheme (at most 32 UTF-16 units). */
export function isStoredEmoji(str) {
  if (typeof str !== 'string' || str.length === 0 || str.length > 32) return false;
  if (CUSTOM_EMOJI_RE.test(str)) return true;
  return graphemeCount(str) === 1 && PICTOGRAPH_RE.test(str);
}

/** The matching key of a stored emoji: the id for a custom emoji, otherwise the string without U+FE0F. */
export function emojiKeyOf(stored) {
  const s = String(stored ?? '');
  const m = CUSTOM_EMOJI_RE.exec(s);
  return m ? m[3] : stripFE0F(s);
}

/**
 * Turns user input into a stored emoji. `resolveCustom(idOrName)` must return `{ id, name, animated }` or null; the command passes
 * a resolver over the emojis the bot can see.
 * @returns {{ ok: true, value: string } | { ok: false, error: string }}
 */
export function parseEmojiInput(str, resolveCustom = () => null) {
  const s = typeof str === 'string' ? str.trim() : '';
  if (!s) return bad('That is not an emoji.');
  if (CUSTOM_EMOJI_RE.test(s)) return ok(s);
  const named = /^:(\w{2,32}):$/.exec(s);
  if (named || /^\d{17,20}$/.test(s)) {
    let found = null;
    try { found = resolveCustom(named ? named[1] : s); } catch { found = null; }
    if (found?.id && found?.name) return ok(`<${found.animated ? 'a' : ''}:${found.name}:${found.id}>`);
    return bad('I can only use custom emojis from servers I am in.');
  }
  if (isStoredEmoji(s)) return ok(s);
  return bad('That is not an emoji.');
}

// ── Attachments ──────────────────────────────────────────────────────────────

const EXT = Object.freeze({
  IMAGE: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'avif'],
  VIDEO: ['mp4', 'mov', 'webm', 'mkv', 'avi', 'm4v'],
  AUDIO: ['mp3', 'wav', 'ogg', 'oga', 'flac', 'm4a', 'opus', 'aac'],
  TEXT: ['txt', 'md', 'log', 'csv', 'json'],
});

/** @returns {'AUDIO'|'IMAGE'|'VIDEO'|'TEXT'|null} the content type wins over the extension */
export function attachmentKind({ name, contentType } = {}) {
  const type = String(contentType ?? '').split(';')[0].trim().toLowerCase();
  if (type) {
    if (type.startsWith('image/')) return 'IMAGE';
    if (type.startsWith('video/')) return 'VIDEO';
    if (type.startsWith('audio/')) return 'AUDIO';
    if (type.startsWith('text/') || type === 'application/json') return 'TEXT';
  }
  const m = /\.([a-z0-9]+)$/i.exec(String(name ?? ''));
  const ext = m ? m[1].toLowerCase() : '';
  if (!ext) return null;
  for (const kind of ['IMAGE', 'VIDEO', 'AUDIO', 'TEXT']) if (EXT[kind].includes(ext)) return kind;
  return null;
}

// ── Normalisers (every write stores their output; every read passes through them again) ──

function readIds(raw, field, { min = 1, max }) {
  if (!Array.isArray(raw)) return { error: `${field} must be a list.` };
  if (raw.length > max) return { error: `${field} can hold at most ${max} entries.` };
  const out = [];
  const seen = new Set();
  for (const v of raw) {
    const id = typeof v === 'string' ? v.trim() : v;
    if (!isSnowflake(id)) return { error: `${field} has an invalid id.` };
    if (!seen.has(id)) { seen.add(id); out.push(id); }
  }
  if (out.length < min) return { error: `${field} needs at least ${min} entr${min === 1 ? 'y' : 'ies'}.` };
  return { value: out };
}

function readStrings(raw, field, { maxEach, max }) {
  if (!Array.isArray(raw)) return { error: `${field} must be a list.` };
  if (raw.length > max) return { error: `${field} can hold at most ${max} entries.` };
  const out = [];
  const seen = new Set();
  for (const v of raw) {
    const s = typeof v === 'string' ? v.trim() : '';
    if (s.length < 1 || s.length > maxEach) return { error: `Each entry of ${field} must be 1-${maxEach} characters.` };
    const k = lc(s);
    if (!seen.has(k)) { seen.add(k); out.push(s); }
  }
  if (out.length < 1) return { error: `${field} needs at least 1 entry.` };
  return { value: out };
}

function readOptionalId(v, field) {
  if (v === undefined || v === null) return { value: null };
  const id = typeof v === 'string' ? v.trim() : v;
  return isSnowflake(id) ? { value: id } : { error: `${field} is not a valid id.` };
}

function readContent(v, field = 'content') {
  const s = typeof v === 'string' ? v.trim() : '';
  if (s.length < 1 || s.length > CONTENT_MAX) return { error: `${field} must be 1-${CONTENT_MAX} characters.` };
  return { value: s };
}

function readEmoji(v, field = 'emoji') {
  const s = typeof v === 'string' ? v.trim() : v;
  return isStoredEmoji(s) ? { value: s } : { error: `${field} is not a valid emoji.` };
}

/** @returns {{ ok: boolean, value?: object|null, error?: string }} value is null for "no button" */
export function normalizeButton(raw) {
  if (raw === undefined || raw === null) return ok(null);
  if (!isPlainObject(raw)) return bad('The button must be an object.');
  const label = typeof raw.label === 'string' ? raw.label.trim() : '';
  if (label.length < 1 || label.length > BUTTON_LABEL_MAX) return bad(`The button label must be 1-${BUTTON_LABEL_MAX} characters.`);
  const rawUrl = typeof raw.url === 'string' ? raw.url.trim() : raw.url;
  if (rawUrl !== undefined && rawUrl !== null && rawUrl !== '') {
    if (typeof rawUrl !== 'string' || rawUrl.length > BUTTON_URL_MAX || !isHttpUrl(rawUrl)) {
      return bad('The button link must start with http:// or https://.');
    }
    return ok({ label, url: rawUrl, style: 'PRIMARY', index: null });
  }
  const { index } = raw;
  if (!Number.isInteger(index) || index < 1 || index > MAX_BUTTON_INDEX) {
    return bad(`An automation button needs a number from 1 to ${MAX_BUTTON_INDEX}.`);
  }
  const style = raw.style === undefined || raw.style === null ? 'PRIMARY' : raw.style;
  if (!BUTTON_STYLES.includes(style)) return bad(`The button style must be one of ${BUTTON_STYLES.join(', ')}.`);
  return ok({ label, url: null, style, index });
}

const idListCondition = (type, field, max) => (raw) => {
  const r = readIds(raw[field], field, { max });
  return r.error ? bad(r.error) : ok({ type, [field]: r.value });
};
const wordListCondition = (type, field, maxEach) => (raw) => {
  const r = readStrings(raw[field], field, { maxEach, max: 25 });
  return r.error ? bad(r.error) : ok({ type, [field]: r.value });
};
const flagCondition = (type) => () => ok({ type });

function normalizeFlags(raw) {
  if (raw === undefined || raw === null) return { value: 'i' };
  if (typeof raw !== 'string' || !/^[imsu]{0,4}$/.test(raw)) return { error: 'The regex flags can only use i, m, s and u.' };
  return { value: [...new Set(raw)].sort().join('') };
}

const CONDITION_NORMALIZERS = Object.freeze({
  CHANNEL_IN: idListCondition('CHANNEL_IN', 'channelIds', 25),
  CHANNEL_NOT_IN: idListCondition('CHANNEL_NOT_IN', 'channelIds', 25),
  CATEGORY_IN: idListCondition('CATEGORY_IN', 'categoryIds', 25),
  CATEGORY_NOT_IN: idListCondition('CATEGORY_NOT_IN', 'categoryIds', 25),
  CONTAINS: wordListCondition('CONTAINS', 'words', 100),
  EXACT: wordListCondition('EXACT', 'texts', 500),
  NOT_CONTAINS: wordListCondition('NOT_CONTAINS', 'words', 100),
  MATCHES_REGEX: (raw) => {
    const pattern = typeof raw.pattern === 'string' ? raw.pattern : '';
    const f = normalizeFlags(raw.flags);
    if (f.error) return bad(f.error);
    const v = validateRegex(pattern, f.value);
    if (!v.ok) return bad(`Invalid regex: ${v.reason}`);
    return ok({ type: 'MATCHES_REGEX', pattern, flags: f.value });
  },
  ATTACHMENT_AUDIO: flagCondition('ATTACHMENT_AUDIO'),
  ATTACHMENT_IMAGE: flagCondition('ATTACHMENT_IMAGE'),
  ATTACHMENT_VIDEO: flagCondition('ATTACHMENT_VIDEO'),
  ATTACHMENT_TEXT: flagCondition('ATTACHMENT_TEXT'),
  REACTED_WITH: (raw) => {
    if (!Array.isArray(raw.emojis)) return bad('emojis must be a list.');
    if (raw.emojis.length > 10) return bad('emojis can hold at most 10 entries.');
    const out = [];
    const seen = new Set();
    for (const v of raw.emojis) {
      const e = readEmoji(v);
      if (e.error) return bad(e.error);
      const k = emojiKeyOf(e.value);
      if (!seen.has(k)) { seen.add(k); out.push(e.value); }
    }
    return out.length ? ok({ type: 'REACTED_WITH', emojis: out }) : bad('emojis needs at least 1 entry.');
  },
  HAS_REACTIONS: (raw) => {
    if (!Number.isInteger(raw.min) || raw.min < 1 || raw.min > 1000) return bad('min must be a whole number from 1 to 1000.');
    let emoji = null;
    if (raw.emoji !== undefined && raw.emoji !== null) {
      const e = readEmoji(raw.emoji);
      if (e.error) return bad(e.error);
      emoji = e.value;
    }
    return ok({ type: 'HAS_REACTIONS', min: raw.min, emoji });
  },
  IS_REPLY: flagCondition('IS_REPLY'),
  IS_NOT_REPLY: flagCondition('IS_NOT_REPLY'),
  HAS_ANY_ROLE: idListCondition('HAS_ANY_ROLE', 'roleIds', 25),
  HAS_ALL_ROLES: idListCondition('HAS_ALL_ROLES', 'roleIds', 25),
  HAS_NOT_ANY_ROLE: idListCondition('HAS_NOT_ANY_ROLE', 'roleIds', 25),
  HAS_NOT_ALL_ROLES: idListCondition('HAS_NOT_ALL_ROLES', 'roleIds', 25),
  GAINED_ROLE: idListCondition('GAINED_ROLE', 'roleIds', 25),
  LOST_ROLE: idListCondition('LOST_ROLE', 'roleIds', 25),
  USER_IS: idListCondition('USER_IS', 'userIds', 25),
  BUTTON_IS: (raw, opts) => {
    if (!Array.isArray(raw.buttons)) return bad('buttons must be a list.');
    if (raw.buttons.length > 25) return bad('buttons can hold at most 25 entries.');
    const out = [];
    const seen = new Set();
    for (const b of raw.buttons) {
      if (!isPlainObject(b)) return bad('Each button reference must be an object.');
      const automationId = typeof b.automationId === 'string' ? b.automationId.trim() : '';
      if (!AUTOMATION_ID_RE.test(automationId)) return bad('A button reference has an invalid automationId.');
      if (!Number.isInteger(b.index) || b.index < 1 || b.index > MAX_BUTTON_INDEX) return bad(`A button number must be from 1 to ${MAX_BUTTON_INDEX}.`);
      const k = `${automationId}:${b.index}`;
      if (seen.has(k)) continue;
      seen.add(k);
      const entry = { automationId, index: b.index };
      // The name hint exists only in export files, for the import's reference remap (resolveImportReferences strips it).
      if (opts?.keepHints && typeof b.automationName === 'string' && b.automationName.trim()) {
        entry.automationName = clampText(b.automationName.trim(), 64);
      }
      out.push(entry);
    }
    return out.length ? ok({ type: 'BUTTON_IS', buttons: out }) : bad('buttons needs at least 1 entry.');
  },
});

/** @returns {{ ok: true, value: object } | { ok: false, error: string }} a fresh object; unknown keys are dropped */
export function normalizeCondition(raw, opts = {}) {
  if (!isPlainObject(raw)) return bad('A condition must be an object.');
  if (!isConditionType(raw.type)) return bad('Unknown condition type.');
  try {
    return CONDITION_NORMALIZERS[raw.type](raw, opts);
  } catch {
    return bad('The condition could not be read.');
  }
}

const contentAction = (type, { button }) => (raw) => {
  const c = readContent(raw.content);
  if (c.error) return bad(c.error);
  let btn = null;
  if (button) {
    const b = normalizeButton(raw.button);
    if (!b.ok) return bad(b.error);
    btn = b.value;
  }
  return ok(button ? { type, content: c.value, button: btn } : { type, content: c.value });
};
const roleListAction = (type) => (raw) => {
  const r = readIds(raw.roleIds, 'roleIds', { max: 10 });
  return r.error ? bad(r.error) : ok({ type, roleIds: r.value });
};
const amountAction = (type) => (raw) => {
  if (!Number.isInteger(raw.amount) || raw.amount < 1 || raw.amount > MAX_REWARD_AMOUNT) {
    return bad(`amount must be a whole number from 1 to ${formatNumber(MAX_REWARD_AMOUNT)}.`);
  }
  return ok({ type, amount: raw.amount });
};
const bareAction = (type) => () => ok({ type });

const ACTION_NORMALIZERS = Object.freeze({
  CREATE_THREAD: (raw) => {
    let name = DEFAULT_THREAD_NAME;
    if (raw.name !== undefined && raw.name !== null) {
      name = typeof raw.name === 'string' ? raw.name.trim() : '';
      if (name.length < 1 || name.length > THREAD_NAME_MAX) return bad(`The thread name must be 1-${THREAD_NAME_MAX} characters.`);
    }
    const ch = readOptionalId(raw.channelId, 'channelId');
    if (ch.error) return bad(ch.error);
    const minutes = raw.autoArchiveMinutes === undefined || raw.autoArchiveMinutes === null ? 1440 : raw.autoArchiveMinutes;
    if (!THREAD_ARCHIVE_MINUTES.includes(minutes)) return bad(`autoArchiveMinutes must be one of ${THREAD_ARCHIVE_MINUTES.join(', ')}.`);
    return ok({ type: 'CREATE_THREAD', name, channelId: ch.value, autoArchiveMinutes: minutes });
  },
  SEND_MESSAGE: (raw) => {
    const base = contentAction('SEND_MESSAGE', { button: true })(raw);
    if (!base.ok) return base;
    const ch = readOptionalId(raw.channelId, 'channelId');
    if (ch.error) return bad(ch.error);
    return ok({ type: 'SEND_MESSAGE', content: base.value.content, channelId: ch.value, button: base.value.button });
  },
  REPLY: contentAction('REPLY', { button: true }),
  REPOST: (raw) => {
    const ch = readOptionalId(raw.channelId, 'channelId');
    if (ch.error) return bad(ch.error);
    return ch.value ? ok({ type: 'REPOST', channelId: ch.value }) : bad('Pick the channel to repost to.');
  },
  PIN: bareAction('PIN'),
  DELETE: bareAction('DELETE'),
  REACT: (raw) => {
    const e = readEmoji(raw.emoji);
    return e.error ? bad(e.error) : ok({ type: 'REACT', emoji: e.value });
  },
  REMOVE_REACTION: (raw) => {
    if (raw.emoji === undefined || raw.emoji === null) return ok({ type: 'REMOVE_REACTION', emoji: null });
    const e = readEmoji(raw.emoji);
    return e.error ? bad(e.error) : ok({ type: 'REMOVE_REACTION', emoji: e.value });
  },
  GIVE_ROLES: roleListAction('GIVE_ROLES'),
  TAKE_ROLES: roleListAction('TAKE_ROLES'),
  ADD_TO_THREAD: (raw) => {
    const r = readIds(raw.userIds === undefined || raw.userIds === null ? [] : raw.userIds, 'userIds', { min: 0, max: 10 });
    return r.error ? bad(r.error) : ok({ type: 'ADD_TO_THREAD', userIds: r.value });
  },
  SEND_IN_THREAD: contentAction('SEND_IN_THREAD', { button: true }),
  GIVE_COINS: amountAction('GIVE_COINS'),
  TAKE_COINS: amountAction('TAKE_COINS'),
  GIVE_XP: amountAction('GIVE_XP'),
  TAKE_XP: amountAction('TAKE_XP'),
  SEND_EMBED: (raw) => {
    const embedName = normalizeAutomationName(raw.embedName);
    if (!embedName) return bad('embedName must be a saved embed name.');
    const ch = readOptionalId(raw.channelId, 'channelId');
    if (ch.error) return bad(ch.error);
    return ok({ type: 'SEND_EMBED', embedName, channelId: ch.value });
  },
});

/** @returns {{ ok: true, value: object } | { ok: false, error: string }} a fresh object; unknown keys are dropped */
export function normalizeAction(raw) {
  if (!isPlainObject(raw)) return bad('An action must be an object.');
  if (!isActionType(raw.type)) return bad('Unknown action type.');
  try {
    return ACTION_NORMALIZERS[raw.type](raw);
  } catch {
    return bad('The action could not be read.');
  }
}

/**
 * Normalises a stored or imported array. Invalid elements are dropped and reported with their 1-based position.
 * @param {unknown} rawArray
 * @param {'condition'|'action'} kind
 * @returns {{ items: object[], invalid: Array<{ index: number, error: string }> }}
 */
export function normalizeList(rawArray, kind, opts = {}) {
  const items = [];
  const invalid = [];
  if (!Array.isArray(rawArray)) return { items, invalid };
  const norm = kind === 'condition' ? (r) => normalizeCondition(r, opts) : (r) => normalizeAction(r);
  const n = Math.min(rawArray.length, MAX_STORED_ELEMENTS);
  for (let i = 0; i < n; i++) {
    const r = norm(rawArray[i]);
    if (r.ok) items.push(r.value);
    else invalid.push({ index: i + 1, error: r.error });
  }
  return { items, invalid };
}

// ── Shape validation (spec 3.3.3) ────────────────────────────────────────────

/** @returns {string[]} human-readable errors; [] means valid (`i` is 1-based) */
export function validateAutomationShape({ trigger, conditions, actions, cooldownSeconds } = {}) {
  const errors = [];
  if (!isTrigger(trigger)) return ['Unknown trigger.'];
  const conds = Array.isArray(conditions) ? conditions : [];
  const acts = Array.isArray(actions) ? actions : [];
  const tLabel = TRIGGERS[trigger].label;

  if (conds.length > LIMITS.AUTOMATION_CONDITIONS) errors.push(ERROR_TEXTS.maxConditions());
  if (acts.length > LIMITS.AUTOMATION_ACTIONS) errors.push(ERROR_TEXTS.maxActions());
  if (!Number.isInteger(cooldownSeconds) || cooldownSeconds < 0 || cooldownSeconds > MAX_COOLDOWN_SECONDS) {
    errors.push(`The cooldown must be a whole number of seconds from 0 to ${formatNumber(MAX_COOLDOWN_SECONDS)}.`);
  }

  const seenFlags = new Set();
  conds.forEach((c, k) => {
    const i = k + 1;
    if (!isConditionType(c?.type)) { errors.push(`Condition ${i}: unknown type.`); return; }
    const def = CONDITION_TYPES[c.type];
    if (!def.triggers.has(trigger)) errors.push(`Condition ${i}: "${def.label}" doesn't work with the trigger "${tLabel}".`);
    if (NO_PARAM_CONDITIONS.has(c.type)) {
      if (seenFlags.has(c.type)) errors.push(`Condition ${i} is already there.`);
      seenFlags.add(c.type);
    }
  });
  if (seenFlags.has('IS_REPLY') && seenFlags.has('IS_NOT_REPLY')) {
    errors.push('These two conditions can never both be true: "Message is a reply" and "Message is not a reply".');
  }

  const usedButtons = new Set();
  let threadBefore = false;
  acts.forEach((a, k) => {
    const i = k + 1;
    if (!isActionType(a?.type)) { errors.push(`Action ${i}: unknown type.`); return; }
    const def = ACTION_TYPES[a.type];
    if (!def.triggers.has(trigger)) errors.push(`Action ${i}: "${def.label}" doesn't work with the trigger "${tLabel}".`);
    if ((a.type === 'SEND_MESSAGE' || a.type === 'SEND_EMBED') && !a.channelId && !T_CHANNEL.has(trigger)) {
      errors.push(`Action ${i}: this trigger doesn't happen in a channel, so pick a channel.`);
    }
    if (a.type === 'CREATE_THREAD' && !a.channelId && !T_LIVE.has(trigger)) {
      errors.push(`Action ${i}: there is no message to start a thread from, so pick a channel.`);
    }
    if (def.thread && !T_THREADY.has(trigger) && !threadBefore) {
      errors.push(`Action ${i}: there is no thread here. Add "Create a thread" before it.`);
    }
    if (a.type === 'CREATE_THREAD') threadBefore = true;
    if (a.button && a.button.url == null && Number.isInteger(a.button.index)) {
      if (usedButtons.has(a.button.index)) errors.push(`Action ${i}: button number ${a.button.index} is used twice.`);
      usedButtons.add(a.button.index);
    }
  });
  return errors;
}

/** 1 + the highest automation-button number among the actions, or 1; null when that exceeds 99. (Assignment uses buttonSeq.) */
export function nextButtonIndex(actions) {
  let max = 0;
  for (const a of Array.isArray(actions) ? actions : []) {
    if (a?.button && a.button.url == null && Number.isInteger(a.button.index) && a.button.index > max) max = a.button.index;
  }
  return max + 1 > MAX_BUTTON_INDEX ? null : max + 1;
}

const highestButtonIndex = (actions) => {
  let max = 0;
  for (const a of Array.isArray(actions) ? actions : []) {
    if (a?.button && a.button.url == null && Number.isInteger(a.button.index) && a.button.index > max) max = a.button.index;
  }
  return max;
};

/** The monotonic number the next new automation button gets: above both the row's buttonSeq and every existing number. */
export function nextButtonSeq(row) {
  const next = Math.max(Number.isInteger(row?.buttonSeq) ? row.buttonSeq : 0, highestButtonIndex(row?.actions)) + 1;
  return next > MAX_BUTTON_INDEX ? null : next;
}

const conditionListKey = (type, entry) => {
  if (type === 'CONTAINS' || type === 'NOT_CONTAINS' || type === 'EXACT') return lc(entry);
  if (type === 'REACTED_WITH') return emojiKeyOf(entry);
  if (type === 'BUTTON_IS') return `${entry.automationId}:${entry.index}`;
  return entry;
};

/**
 * Adds a condition to a row: merges into the first condition of the same list type, otherwise appends.
 * @param {{ conditions: object[], trigger?: string }} row
 * @param {string} type
 * @param {object} params
 * @returns {{ conditions: object[], merged: boolean, index: number, addedNew: boolean }} `index` is 1-based
 * @throws {AutomationError}
 */
export function addCondition(row, type, params) {
  if (!isConditionType(type)) throw new AutomationError('Unknown condition type.');
  const def = CONDITION_TYPES[type];
  const current = Array.isArray(row?.conditions) ? row.conditions : [];
  if (def.list) {
    const at = current.findIndex((c) => c.type === type);
    if (at !== -1) {
      const existing = current[at][def.list];
      const incoming = Array.isArray(params?.[def.list]) ? params[def.list] : [];
      const seen = new Set(existing.map((e) => conditionListKey(type, e)));
      const fresh = [];
      for (const e of incoming) {
        const k = conditionListKey(type, e);
        if (!seen.has(k)) { seen.add(k); fresh.push(e); }
      }
      if (fresh.length === 0) return { conditions: current, merged: true, index: at + 1, addedNew: false };
      if (existing.length + fresh.length > def.max) throw new AutomationError(`That list can hold at most ${def.max} entries.`);
      const next = normalizeCondition({ ...current[at], [def.list]: [...existing, ...fresh] });
      if (!next.ok) throw new AutomationError(next.error);
      const conditions = current.map((c, k) => (k === at ? next.value : c));
      return { conditions, merged: true, index: at + 1, addedNew: true };
    }
  }
  if (current.length >= LIMITS.AUTOMATION_CONDITIONS) throw new AutomationError(ERROR_TEXTS.maxConditions());
  const made = normalizeCondition({ ...params, type });
  if (!made.ok) throw new AutomationError(made.error);
  return { conditions: [...current, made.value], merged: false, index: current.length + 1, addedNew: true };
}

/**
 * Appends an action. A new automation button gets `buttonSeq + 1` (never `max(existing) + 1`, so a removed button's number is not
 * reused); the caller stores the returned `buttonSeq` in the same locked write.
 * @param {{ actions: object[], buttonSeq?: number }} row
 * @param {object} action an action whose `button.index` may still be null
 * @returns {{ actions: object[], index: number, buttonIndex: number|null, buttonSeq: number|null }}
 * @throws {AutomationError}
 */
export function appendAction(row, action) {
  const current = Array.isArray(row?.actions) ? row.actions : [];
  if (current.length >= LIMITS.AUTOMATION_ACTIONS) throw new AutomationError(ERROR_TEXTS.maxActions());
  let raw = action;
  let buttonIndex = null;
  if (action?.button && (action.button.url === null || action.button.url === undefined || action.button.url === '')) {
    buttonIndex = nextButtonSeq(row);
    if (buttonIndex === null) throw new AutomationError(ERROR_TEXTS.buttonsUsed, 'BUTTONS');
    raw = { ...action, button: { ...action.button, url: null, index: buttonIndex } };
  }
  const made = normalizeAction(raw);
  if (!made.ok) throw new AutomationError(made.error);
  return { actions: [...current, made.value], index: current.length + 1, buttonIndex, buttonSeq: buttonIndex };
}

/** Removes the 1-based `index1`. @throws {AutomationError} when out of range */
export function removeAt(list, index1) {
  const arr = Array.isArray(list) ? list : [];
  if (!Number.isInteger(index1) || index1 < 1 || index1 > arr.length) throw new AutomationError(`There are ${arr.length} to choose from.`);
  return { list: arr.filter((_, k) => k !== index1 - 1), removed: arr[index1 - 1] };
}

// ── Condition evaluator (spec 3.3.1, 6.5) ────────────────────────────────────

const regexCache = new Map();
function compiledRegex(cond) {
  const key = `${cond.pattern}\u0000${cond.flags}`;
  if (regexCache.has(key)) return regexCache.get(key);
  const re = compileSafeRegex(cond.pattern, cond.flags);
  if (regexCache.size >= 500) regexCache.delete(regexCache.keys().next().value);
  regexCache.set(key, re);
  return re;
}

/** Unavailable facts fail the condition (fail-closed), including the "not" types. An unknown type always fails. */
export function evaluateCondition(cond, facts) {
  try {
    if (!cond || !facts || !isConditionType(cond.type)) return false;
    const f = facts;
    const held = Array.isArray(f.memberRoleIds) ? f.memberRoleIds : null;
    switch (cond.type) {
      case 'CHANNEL_IN': return (f.channelId != null && cond.channelIds.includes(f.channelId)) || (f.parentId != null && cond.channelIds.includes(f.parentId));
      case 'CHANNEL_NOT_IN': return f.channelId != null && !cond.channelIds.includes(f.channelId) && !(f.parentId != null && cond.channelIds.includes(f.parentId));
      case 'CATEGORY_IN': return f.categoryId != null && cond.categoryIds.includes(f.categoryId);
      case 'CATEGORY_NOT_IN': return f.channelId != null && (f.categoryId == null || !cond.categoryIds.includes(f.categoryId));
      case 'CONTAINS': return f.content != null && cond.words.some((w) => containsWord(f.content, w));
      case 'NOT_CONTAINS': return f.content != null && !cond.words.some((w) => containsWord(f.content, w));
      case 'EXACT': {
        if (f.content == null) return false;
        const c = lc(String(f.content).trim());
        return cond.texts.some((t) => lc(t) === c);
      }
      case 'MATCHES_REGEX': {
        if (f.content == null) return false;
        const re = compiledRegex(cond);
        return re ? safeRegexTest(re, f.content) : false;   // a pattern that no longer compiles fails
      }
      case 'ATTACHMENT_AUDIO': return attachmentsHave(f, 'AUDIO');
      case 'ATTACHMENT_IMAGE': return attachmentsHave(f, 'IMAGE');
      case 'ATTACHMENT_VIDEO': return attachmentsHave(f, 'VIDEO');
      case 'ATTACHMENT_TEXT': return attachmentsHave(f, 'TEXT');
      case 'REACTED_WITH': return f.reactionKey != null && cond.emojis.some((e) => emojiKeyOf(e) === f.reactionKey);
      case 'HAS_REACTIONS': {
        if (!f.reactionCounts) return false;
        const count = cond.emoji
          ? (f.reactionCounts[emojiKeyOf(cond.emoji)] ?? 0)
          : Object.values(f.reactionCounts).reduce((a, b) => a + b, 0);
        return count >= cond.min;
      }
      case 'IS_REPLY': return f.isReply === true;
      case 'IS_NOT_REPLY': return f.isReply === false;
      case 'HAS_ANY_ROLE': return held !== null && cond.roleIds.some((r) => held.includes(r));
      case 'HAS_ALL_ROLES': return held !== null && cond.roleIds.every((r) => held.includes(r));
      case 'HAS_NOT_ANY_ROLE': return held !== null && !cond.roleIds.some((r) => held.includes(r));
      case 'HAS_NOT_ALL_ROLES': return held !== null && cond.roleIds.some((r) => !held.includes(r));
      case 'GAINED_ROLE':
      case 'LOST_ROLE': return Array.isArray(f.changedRoleIds) && cond.roleIds.some((r) => f.changedRoleIds.includes(r));
      case 'USER_IS': return f.userId != null && cond.userIds.includes(f.userId);
      case 'BUTTON_IS': return !!f.button && cond.buttons.some((b) => b.automationId === f.button.automationId && b.index === f.button.index);
      default: return false;
    }
  } catch {
    return false;
  }
}

function attachmentsHave(facts, kind) {
  return Array.isArray(facts.attachments) && facts.attachments.some((a) => attachmentKind(a) === kind);
}

/** Every condition must pass; no conditions always passes. */
export function evaluateConditions(conds, facts) {
  return (Array.isArray(conds) ? conds : []).every((c) => evaluateCondition(c, facts));
}

/** Because all conditions are ANDed, evaluating the two halves separately is equivalent to evaluating all of them. */
export function splitConditions(conds) {
  const cheap = [];
  const needsMessage = [];
  for (const c of Array.isArray(conds) ? conds : []) (NEEDS_MESSAGE.has(c?.type) ? needsMessage : cheap).push(c);
  return { cheap, needsMessage };
}

/** Role ids gained and lost between two snapshots; @everyone (the guild id) never counts. */
export function roleDiff(oldRoleIds, newRoleIds, guildId) {
  const before = new Set(oldRoleIds ?? []);
  const after = new Set(newRoleIds ?? []);
  return {
    gained: [...after].filter((id) => !before.has(id) && id !== guildId),
    lost: [...before].filter((id) => !after.has(id) && id !== guildId),
  };
}

/** A voice move fires VOICE_LEFT (old channel) then VOICE_JOINED (new channel). */
export function voiceTriggers(change) {
  if (change === 'join') return ['VOICE_JOINED'];
  if (change === 'leave') return ['VOICE_LEFT'];
  if (change === 'move') return ['VOICE_LEFT', 'VOICE_JOINED'];
  return [];
}

// ── Rate limits and loop protection (spec 6.7) ───────────────────────────────

/** Per-guild sliding window: `take` returns false when `max` runs started within the last `windowMs`. */
export function createRunBudget({ max = RUN_BUDGET_MAX, windowMs = RUN_BUDGET_WINDOW_MS } = {}) {
  const runs = new Map();
  return {
    take(guildId, now = Date.now()) {
      let list = runs.get(guildId);
      if (list) {
        while (list.length && now - list[0] >= windowMs) list.shift();
        if (list.length === 0) { runs.delete(guildId); list = undefined; }
      }
      if (list && list.length >= max) return false;
      if (!list) {
        if (runs.size >= 5_000) {
          for (const [g, l] of runs) if (!l.length || now - l[l.length - 1] >= windowMs) runs.delete(g);
          if (runs.size >= 5_000) runs.delete(runs.keys().next().value);
        }
        list = [];
        runs.set(guildId, list);
      }
      list.push(now);
      return true;
    },
  };
}

/**
 * Remembers, per (guild, user), the chain depth of role changes the bot is about to cause, so the resulting guildMemberUpdate
 * knows how deep the automation chain is. `mark` keeps the highest live depth.
 */
export function createCauseTracker({ ttlMs = CAUSE_TTL_MS, cap = 10_000 } = {}) {
  const map = new Map();
  return {
    mark(guildId, userId, depth, now = Date.now()) {
      const key = `${guildId}:${userId}`;
      const prev = map.get(key);
      const live = prev && prev.expiresAt > now ? prev.depth : 0;
      map.delete(key);
      map.set(key, { depth: Math.max(live, depth), expiresAt: now + ttlMs });
      while (map.size > cap) map.delete(map.keys().next().value);
    },
    peek(guildId, userId, now = Date.now()) {
      const e = map.get(`${guildId}:${userId}`);
      return e && e.expiresAt > now ? e.depth : 0;
    },
  };
}

/** A bounded set of keys that expire: `add` returns true when the key was newly added (not live). */
export function createLocalExpiringSet({ cap = 10_000 } = {}) {
  const map = new Map();
  return {
    add(key, ttlMs, now = Date.now()) {
      const prev = map.get(key);
      if (prev !== undefined && prev > now) return false;
      map.delete(key);
      if (map.size >= cap) {
        for (const [k, exp] of map) if (exp <= now) map.delete(k);
        while (map.size >= cap) map.delete(map.keys().next().value);
      }
      map.set(key, now + Math.max(1, ttlMs));
      return true;
    },
    has(key, now = Date.now()) {
      const exp = map.get(key);
      return exp !== undefined && exp > now;
    },
    delete(key) {
      return map.delete(key);
    },
  };
}

// ── Module-scope state (pure constructors only: no timers, no I/O) ───────────

const memo = new Map();                       // guildId -> { at, byTrigger }
const generation = new Map();                 // guildId -> number, bumped by invalidateAutomations
const inflight = new Map();                   // `${guildId}:${gen}` -> Promise (one cold load per guild at a time)
const localCooldowns = createLocalExpiringSet();
const localReposts = createLocalExpiringSet();
const causes = createCauseTracker();
const budget = createRunBudget();
const guildQueue = createKeyedMutex();
const pending = new Map();                    // guildId -> queued + running runs
const warnThrottle = createThrottle(600_000);
const chainThrottle = createThrottle(60_000);

// ── Descriptions ─────────────────────────────────────────────────────────────

const codeSafe = (s) => String(s ?? '').replace(/`/g, 'ˋ');
const oneLine = (s) => String(s ?? '').replace(/\s+/g, ' ').trim();

/** A mention for an id; `⚠️ deleted (id)` when the guild is known to lack it (display only: nothing is ever acted on). */
function entityText(kind, id, guild) {
  const mention = kind === 'role' ? `<@&${id}>` : kind === 'user' ? `<@${id}>` : `<#${id}>`;
  if (!guild || kind === 'user' || guild.available === false) return mention;
  const store = kind === 'role' ? guild.roles?.cache : guild.channels?.cache;
  return store && typeof store.has === 'function' && !store.has(id) ? `⚠️ deleted (${id})` : mention;
}

const entityList = (kind, ids, guild, max = 120) => joinWithLimit((ids ?? []).map((id) => entityText(kind, id, guild)), { sep: ', ', max });
const quotedList = (items, max = 120) => joinWithLimit((items ?? []).map((w) => `"${clampText(oneLine(w), 40)}"`), { sep: ', ', max });

/** @returns {string} one line, at most 200 characters; never throws */
export function describeCondition(cond, { guild = null } = {}) {
  try {
    if (!cond || !isConditionType(cond.type)) return 'Unknown condition';
    const label = CONDITION_TYPES[cond.type].label;
    let detail = '';
    switch (cond.type) {
      case 'CHANNEL_IN': case 'CHANNEL_NOT_IN': detail = entityList('channel', cond.channelIds, guild); break;
      case 'CATEGORY_IN': case 'CATEGORY_NOT_IN': detail = entityList('channel', cond.categoryIds, guild); break;
      case 'CONTAINS': case 'NOT_CONTAINS': detail = quotedList(cond.words); break;
      case 'EXACT': detail = quotedList(cond.texts); break;
      case 'MATCHES_REGEX': detail = `\`/${codeSafe(clampText(cond.pattern, 100))}/${cond.flags}\``; break;
      case 'REACTED_WITH': detail = joinWithLimit(cond.emojis, { sep: ' ', max: 120 }); break;
      case 'HAS_REACTIONS': return clampText(`Message has at least ${formatNumber(cond.min)} ${cond.emoji ?? ''} reaction${cond.min === 1 ? '' : 's'}`.replace(/\s+/g, ' '), 200);
      case 'HAS_ANY_ROLE': case 'HAS_ALL_ROLES': case 'HAS_NOT_ANY_ROLE': case 'HAS_NOT_ALL_ROLES':
      case 'GAINED_ROLE': case 'LOST_ROLE': detail = entityList('role', cond.roleIds, guild); break;
      case 'USER_IS': detail = entityList('user', cond.userIds, guild); break;
      case 'BUTTON_IS':
        detail = joinWithLimit(cond.buttons.map((b) => `button ${b.index} of \`${codeSafe(clampText(b.automationId, 12))}\``), { sep: ', ', max: 120 });
        break;
      default: break;
    }
    return clampText(detail ? `${label}: ${detail}` : label, 200);
  } catch {
    return 'Condition';
  }
}

function buttonText(button) {
  if (!button) return '';
  return button.url == null
    ? ` with button ${button.index} "${clampText(oneLine(button.label), 30)}"`
    : ` with link button "${clampText(oneLine(button.label), 30)}"`;
}

/** @returns {string} one line, at most 200 characters; never throws */
export function describeAction(action, { guild = null, settings = null } = {}) {
  try {
    if (!action || !isActionType(action.type)) return 'Unknown action';
    const inCh = (id) => (id ? ` in ${entityText('channel', id, guild)}` : '');
    let text;
    switch (action.type) {
      case 'CREATE_THREAD': text = `Create a thread "${clampText(oneLine(action.name), 40)}"${inCh(action.channelId)}`; break;
      case 'SEND_MESSAGE': text = `Send "${clampText(oneLine(action.content), 50)}"${inCh(action.channelId)}${buttonText(action.button)}`; break;
      case 'REPLY': text = `Reply "${clampText(oneLine(action.content), 50)}"${buttonText(action.button)}`; break;
      case 'REPOST': text = `Repost the message${action.channelId ? ` to ${entityText('channel', action.channelId, guild)}` : ''}`; break;
      case 'PIN': text = 'Pin the message'; break;
      case 'DELETE': text = 'Delete the message'; break;
      case 'REACT': text = `React with ${action.emoji}`; break;
      case 'REMOVE_REACTION': text = action.emoji ? `Remove the ${action.emoji} reaction` : 'Remove the reaction that triggered this'; break;
      case 'GIVE_ROLES': text = `Give roles: ${entityList('role', action.roleIds, guild, 100)}`; break;
      case 'TAKE_ROLES': text = `Take roles: ${entityList('role', action.roleIds, guild, 100)}`; break;
      case 'ADD_TO_THREAD': text = `Add ${action.userIds.length ? entityList('user', action.userIds, guild, 100) : 'the member'} to the thread`; break;
      case 'SEND_IN_THREAD': text = `Send "${clampText(oneLine(action.content), 50)}" in the thread${buttonText(action.button)}`; break;
      case 'GIVE_COINS': case 'TAKE_COINS': {
        const verb = action.type === 'GIVE_COINS' ? 'Give' : 'Take';
        text = `${verb} ${formatNumber(action.amount)} ${settings?.currencyEmoji ?? ''} ${settings?.currencyName ?? 'coins'}`;
        break;
      }
      case 'GIVE_XP': text = `Give ${formatNumber(action.amount)} XP`; break;
      case 'TAKE_XP': text = `Take ${formatNumber(action.amount)} XP`; break;
      case 'SEND_EMBED': text = `Send the saved embed **${action.embedName}**${inCh(action.channelId)}`; break;
      default: text = ACTION_TYPES[action.type].label;
    }
    return clampText(text.replace(/\s+/g, ' ').trim(), 200);
  } catch {
    return 'Action';
  }
}

// ── Variables (spec 6.8) ─────────────────────────────────────────────────────

/**
 * Feature variables for renderPlaceholders (`ctx.vars`). Values are strings, or functions that read `run` at render time
 * (the thread only exists after CREATE_THREAD ran). Missing values render ''.
 */
export function buildVars(base, run) {
  const b = base ?? {};
  const r = run ?? {};
  const msg = b.message ?? null;
  const safe = (fn) => { try { return fn() ?? ''; } catch { return ''; } };
  const vars = {
    automation: r.automation?.name ?? '',
    reaction: b.reaction?.display ?? '',
  };
  if (msg) {
    vars['message.content'] = safe(() => clampText(msg.content ?? '', 1500));
    vars['message.link'] = safe(() => msg.url ?? '');
    vars['message.id'] = safe(() => msg.id);
    vars['message.author'] = safe(() => (msg.author?.id ? `<@${msg.author.id}>` : ''));
    vars['message.author.name'] = safe(() => msg.author?.username ?? '');
  }
  if (typeof b.oldContent === 'string') vars['message.old_content'] = clampText(b.oldContent, 1500);
  const roleId = Array.isArray(b.roleIds) ? b.roleIds[0] : null;
  if (roleId) {
    vars.role = `<@&${roleId}>`;
    vars['role.id'] = roleId;
    vars['role.name'] = safe(() => b.guild?.roles?.cache?.get(roleId)?.name ?? '');
  }
  if (b.voiceChannel) {
    vars.voice = safe(() => `<#${b.voiceChannel.id}>`);
    vars['voice.name'] = safe(() => b.voiceChannel.name ?? '');
  }
  vars.thread = () => safe(() => (r.thread ? `<#${r.thread.id}>` : ''));
  vars['thread.name'] = () => safe(() => r.thread?.name ?? '');
  return vars;
}

// ── Presets (spec 6.11) ──────────────────────────────────────────────────────

/** Which options each preset reads (the others are reported as ignored). */
export const PRESET_OPTIONS = Object.freeze({
  welcomer: Object.freeze(['channel', 'message', 'role']),
  starboard: Object.freeze(['channel', 'emoji', 'threshold']),
  'thread-joiner': Object.freeze(['user', 'channel']),
});

function mustNormalize(list, kind) {
  return list.map((raw) => {
    const r = kind === 'condition' ? normalizeCondition(raw) : normalizeAction(raw);
    if (!r.ok) throw new AutomationError(r.error);
    return r.value;
  });
}

/**
 * @param {'welcomer'|'starboard'|'thread-joiner'} preset
 * @param {{ channelId?: string|null, message?: string|null, roleId?: string|null, emoji?: string|null, threshold?: number|null, userId?: string|null }} opts
 * @returns {{ trigger: string, conditions: object[], actions: object[] }} normalised and shape-valid
 * @throws {AutomationError}
 */
export function buildPresetAutomation(preset, opts = {}) {
  let built;
  if (preset === 'welcomer') {
    const actions = [{ type: 'SEND_MESSAGE', channelId: opts.channelId, content: opts.message ?? WELCOMER_DEFAULT_TEXT, button: null }];
    if (opts.roleId) actions.push({ type: 'GIVE_ROLES', roleIds: [opts.roleId] });
    built = { trigger: 'MEMBER_JOINED', conditions: [], actions };
  } else if (preset === 'starboard') {
    const emoji = opts.emoji ?? DEFAULT_STAR;
    built = {
      trigger: 'REACTION_ADDED',
      conditions: [
        { type: 'REACTED_WITH', emojis: [emoji] },
        { type: 'HAS_REACTIONS', min: opts.threshold ?? DEFAULT_THRESHOLD, emoji },
        { type: 'CHANNEL_NOT_IN', channelIds: [opts.channelId] },
      ],
      actions: [{ type: 'REPOST', channelId: opts.channelId }],
    };
  } else if (preset === 'thread-joiner') {
    built = {
      trigger: 'THREAD_CREATED',
      conditions: opts.channelId ? [{ type: 'CHANNEL_IN', channelIds: [opts.channelId] }] : [],
      actions: [{ type: 'ADD_TO_THREAD', userIds: [opts.userId] }],
    };
  } else {
    throw new AutomationError('Unknown preset.');
  }
  const conditions = mustNormalize(built.conditions, 'condition');
  const actions = mustNormalize(built.actions, 'action');
  const errors = validateAutomationShape({ trigger: built.trigger, conditions, actions, cooldownSeconds: 0 });
  if (errors.length) throw new AutomationError(errors.join('\n'));
  return { trigger: built.trigger, conditions, actions };
}

// ── JSON export / import (spec 6.12) ─────────────────────────────────────────

/**
 * @param {object} row an Automation row (arrays are re-normalised)
 * @param {Map<string, { name: string }|string>} [automationsById] adds an `automationName` hint to BUTTON_IS entries
 */
export function toExportObject(row, automationsById = new Map()) {
  const nameOf = (id) => {
    const v = automationsById?.get?.(id);
    const n = typeof v === 'string' ? v : v?.name;
    return typeof n === 'string' && n ? n : null;
  };
  const conditions = normalizeList(row?.conditions, 'condition').items.map((c) => {
    if (c.type !== 'BUTTON_IS') return c;
    return {
      ...c,
      buttons: c.buttons.map((b) => {
        const name = nameOf(b.automationId);
        return name ? { automationId: b.automationId, index: b.index, automationName: name } : b;
      }),
    };
  });
  return {
    format: EXPORT_FORMAT,
    version: EXPORT_VERSION,
    exportedId: row?.id ?? null,
    name: row?.name ?? null,
    trigger: row?.trigger,
    enabled: row?.enabled === true,
    cooldownSeconds: Number.isInteger(row?.cooldownSeconds) ? row.cooldownSeconds : 0,
    conditions,
    actions: normalizeList(row?.actions, 'action').items,
  };
}

export function toExportJson(row, automationsById) {
  return JSON.stringify(toExportObject(row, automationsById), null, 2);
}

/**
 * Strictly validates pasted or attached JSON. Pure. Size is capped, every field is type-checked and every element goes through the
 * normalisers, which build fresh objects field by field (unknown keys, `__proto__` and friends never reach storage).
 * @param {string} text
 * @returns {{ ok: boolean, data?: object, errors: string[], warnings: string[] }}
 */
export function parseAutomationJson(text) {
  const errors = [];
  const fail = (...msgs) => ({ ok: false, errors: msgs.length ? msgs : errors, warnings: [] });
  if (typeof text !== 'string') return fail('That is not valid JSON.');
  if (Buffer.byteLength(text, 'utf8') > IMPORT_MAX_BYTES) return fail('The JSON must be 32 KB or smaller.');
  let obj;
  try {
    obj = JSON.parse(text.charCodeAt(0) === 0xfeff ? text.slice(1) : text);
  } catch {
    return fail('That is not valid JSON.');
  }
  if (!isPlainObject(obj)) return fail('The JSON must be an object from /automation export.');
  if (obj.format !== undefined && obj.format !== EXPORT_FORMAT) return fail('This is not an automation export.');
  if (obj.version !== EXPORT_VERSION) return fail('Unsupported version (expected "version": 1).');
  if (!isTrigger(obj.trigger)) return fail(`Unknown trigger "${clampText(String(obj.trigger), 40)}".`);

  let name = null;
  if (obj.name !== undefined && obj.name !== null) {
    if (typeof obj.name !== 'string') errors.push('name must be text.');
    else name = normalizeAutomationName(obj.name);
  }
  let enabled = true;
  if (obj.enabled !== undefined) {
    if (typeof obj.enabled !== 'boolean') errors.push('enabled must be true or false.');
    else enabled = obj.enabled;
  }
  let cooldownSeconds = 0;
  if (obj.cooldownSeconds !== undefined) {
    if (!Number.isInteger(obj.cooldownSeconds) || obj.cooldownSeconds < 0 || obj.cooldownSeconds > MAX_COOLDOWN_SECONDS) {
      errors.push(`cooldownSeconds must be a whole number from 0 to ${MAX_COOLDOWN_SECONDS}.`);
    } else cooldownSeconds = obj.cooldownSeconds;
  }

  const readArray = (value, label, max) => {
    if (value === undefined) return [];
    if (!Array.isArray(value)) { errors.push(`${label}s must be a list.`); return null; }
    if (value.length > max) { errors.push(`At most ${max} ${label.toLowerCase()}s are allowed (found ${value.length}).`); return null; }
    return value;
  };
  const rawConds = readArray(obj.conditions, 'Condition', LIMITS.AUTOMATION_CONDITIONS);
  const rawActs = readArray(obj.actions, 'Action', LIMITS.AUTOMATION_ACTIONS);

  const conditions = [];
  const actions = [];
  (rawConds ?? []).forEach((raw, k) => {
    const r = normalizeCondition(raw, { keepHints: true });
    if (r.ok) conditions.push(r.value); else errors.push(`Condition ${k + 1}: ${r.error}`);
  });
  (rawActs ?? []).forEach((raw, k) => {
    const r = normalizeAction(raw);
    if (r.ok) actions.push(r.value); else errors.push(`Action ${k + 1}: ${r.error}`);
  });
  if (!errors.length) errors.push(...validateAutomationShape({ trigger: obj.trigger, conditions, actions, cooldownSeconds }));

  const exportedId = typeof obj.exportedId === 'string' && AUTOMATION_ID_RE.test(obj.exportedId) ? obj.exportedId : null;
  return {
    ok: errors.length === 0,
    data: { name, trigger: obj.trigger, enabled, cooldownSeconds, conditions, actions, exportedId },
    errors,
    warnings: [],
  };
}

// ── Discord errors (spec 6.14) ───────────────────────────────────────────────

/** @returns {string|null} the problem text, or null for an error to skip silently */
export function discordErrorText(err) {
  if (err instanceof TimeoutError) return 'The action took too long';
  switch (err?.code) {
    case RESTJSONErrorCodes.UnknownMessage:
    case RESTJSONErrorCodes.UnknownMember: return null;
    case RESTJSONErrorCodes.UnknownChannel: return 'The channel was deleted';
    case RESTJSONErrorCodes.UnknownRole: return 'The role was deleted';
    case RESTJSONErrorCodes.UnknownEmoji: return 'The emoji is not available';
    case RESTJSONErrorCodes.MissingAccess: return "I can't see that channel";
    case RESTJSONErrorCodes.MissingPermissions: return "I'm missing a permission for this";
    case RESTJSONErrorCodes.MaximumNumberOfPinsReachedForTheChannel: return 'The channel has too many pinned messages';
    case RESTJSONErrorCodes.MaximumNumberOfReactionsReached: return 'The message has too many reactions';
    case RESTJSONErrorCodes.ReactionWasBlocked: return 'Discord blocked the reaction';
    case RESTJSONErrorCodes.InvalidActionOnArchivedThread:
    case RESTJSONErrorCodes.ThreadLocked: return 'The thread is archived or locked';
    case RESTJSONErrorCodes.MaximumActiveThreads: return 'The channel has too many active threads';
    case RESTJSONErrorCodes.MaximumThreadParticipantsReached: return 'The thread is full';
    case RESTJSONErrorCodes.InvalidFormBodyOrContentType: return `Discord rejected the message: ${clampText(err.message, 150)}`;
    default: return clampText(err?.message ?? 'Unknown error', 200);
  }
}

// ── Loading and caching (spec 3.4, 6.3) ──────────────────────────────────────
// AutomationLite = { id, name, trigger, cooldownSeconds, conditions, actions } with normalised arrays. Only rows that are enabled
// and have at least one valid action are cached, so quiet guilds never reach Postgres per event.

function setMemo(guildId, byTrigger) {
  memo.delete(guildId);
  memo.set(guildId, { at: Date.now(), byTrigger });
  while (memo.size > MEMO_MAX_GUILDS) memo.delete(memo.keys().next().value);
}

function liteFromRow(row) {
  const actions = normalizeList(row.actions, 'action').items;
  if (actions.length === 0) return null;
  return {
    id: row.id,
    name: row.name,
    trigger: row.trigger,
    cooldownSeconds: Number.isInteger(row.cooldownSeconds) ? row.cooldownSeconds : 0,
    conditions: normalizeList(row.conditions, 'condition').items,
    actions,
  };
}

async function loadAutomations(guildId, gen) {
  const key = automationsCacheKey(guildId);
  // Degrade decision: a Redis read failure (cache.get -> null) reads Postgres, i.e. it fails open to the database.
  const cached = await cache.get(key);
  if (cached && cached.v === 1 && isPlainObject(cached.byTrigger)) {
    if ((generation.get(guildId) ?? 0) === gen) setMemo(guildId, cached.byTrigger);
    return cached.byTrigger;
  }
  let rows;
  try {
    rows = await prisma.automation.findMany({
      where: { guildId, enabled: true },
      orderBy: { createdAt: 'asc' },
      select: { id: true, name: true, trigger: true, cooldownSeconds: true, conditions: true, actions: true },
    });
  } catch (err) {
    // Degrade decision: Postgres down -> null and the event is skipped (fail-closed: nothing runs). Logged once per guild per 10 min.
    if (warnThrottle(`db:${guildId}`)) logger.error(`automations: could not load automations for ${guildId}`, err);
    return null;
  }
  const byTrigger = {};
  for (const row of rows) {
    const lite = liteFromRow(row);
    if (lite) (byTrigger[lite.trigger] ??= []).push(lite);
  }
  // A loader that started before an invalidation must not write stale rows back into the memo or Redis.
  if ((generation.get(guildId) ?? 0) === gen) {
    setMemo(guildId, byTrigger);
    await cache.set(key, { v: 1, byTrigger }, CACHE_TTL_SECONDS);
  }
  return byTrigger;
}

/** @returns {Promise<Record<string, object[]>|null>} the enabled automations by trigger; null when Postgres is unreachable. Never throws. */
export async function getAutomations(guildId) {
  try {
    const hit = memo.get(guildId);
    if (hit && Date.now() - hit.at < MEMO_TTL_MS) return hit.byTrigger;
    const gen = generation.get(guildId) ?? 0;
    const key = `${guildId}:${gen}`;
    let p = inflight.get(key);
    if (!p) {
      p = loadAutomations(guildId, gen).finally(() => inflight.delete(key));
      inflight.set(key, p);
    }
    return await p;
  } catch (err) {
    if (warnThrottle(`db:${guildId}`)) logger.error(`automations: could not load automations for ${guildId}`, err);
    return null;
  }
}

/** Sync peek of the in-process memo: false only when a fresh entry has nothing for the trigger; unknown or stale -> true. */
export function peekHasTrigger(guildId, trigger) {
  const entry = memo.get(guildId);
  if (!entry || Date.now() - entry.at >= MEMO_TTL_MS) return true;
  const list = Object.hasOwn(entry.byTrigger, trigger) ? entry.byTrigger[trigger] : null;
  return Array.isArray(list) && list.length > 0;
}

/** Drops the memo and the Redis key and bumps the generation. Call after every committed write. Never throws. */
export async function invalidateAutomations(guildId) {
  try {
    generation.set(guildId, (generation.get(guildId) ?? 0) + 1);
    memo.delete(guildId);
    await cache.del(automationsCacheKey(guildId));
  } catch (err) {
    logger.warn('automations: invalidate failed', err);
  }
}

/** Tests only. */
export function __setAutomationMemo(guildId, byTrigger) {
  setMemo(guildId, byTrigger);
}

// ── Reads ────────────────────────────────────────────────────────────────────

/** A row with normalised arrays; invalid stored elements are dropped from the arrays and listed in invalidConditions / invalidActions. */
function hydrate(row) {
  const c = normalizeList(row.conditions, 'condition');
  const a = normalizeList(row.actions, 'action');
  return { ...row, conditions: c.items, actions: a.items, invalidConditions: c.invalid, invalidActions: a.invalid };
}

/** @returns {Promise<object|null>} @throws on a DB failure */
export async function getAutomationByName(guildId, name) {
  const row = await prisma.automation.findUnique({ where: { guildId_name: { guildId, name } } });
  return row ? hydrate(row) : null;
}

/** Autocomplete lookup. Never throws. */
export async function searchAutomations(guildId, q) {
  try {
    return await prisma.automation.findMany({
      where: { guildId, name: { contains: q, mode: 'insensitive' } },
      select: { name: true, trigger: true, enabled: true },
      orderBy: { name: 'asc' },
      take: 25,
    });
  } catch (err) {
    logger.debug(`automations: search failed (${err?.message})`);
    return [];
  }
}

/** Saved-embed names for autocomplete, through the embeds contract. Never throws. */
export async function searchSavedEmbedNames(guildId, q) {
  try {
    const rows = await searchSavedEmbeds(guildId, q, 25);
    return rows.map((r) => r.name);
  } catch {
    return [];
  }
}

/** @throws on a DB failure */
export async function listAutomations(guildId) {
  const rows = await prisma.automation.findMany({
    where: { guildId },
    orderBy: { createdAt: 'asc' },
    take: LIMITS.AUTOMATIONS,
    select: { name: true, trigger: true, enabled: true, conditions: true, actions: true, uses: true },
  });
  return rows.map((r) => ({
    ...r,
    conditions: normalizeList(r.conditions, 'condition').items,
    actions: normalizeList(r.actions, 'action').items,
  }));
}

/** Every automation of the guild with its normalised actions: [{ id, name, actions }]. Never throws. */
export async function listAutomationRefs(guildId) {
  try {
    const rows = await prisma.automation.findMany({
      where: { guildId },
      orderBy: { createdAt: 'asc' },
      take: LIMITS.AUTOMATIONS,
      select: { id: true, name: true, actions: true },
    });
    return rows.map((r) => ({ id: r.id, name: r.name, actions: normalizeList(r.actions, 'action').items }));
  } catch (err) {
    logger.debug(`automations: listing refs failed (${err?.message})`);
    return [];
  }
}

/** Every automation button of the guild: [{ automationId, automationName, index, label }]. Never throws. */
export async function listAutomationButtons(guildId) {
  const out = [];
  for (const ref of await listAutomationRefs(guildId)) {
    for (const a of ref.actions) {
      if (a.button && a.button.url == null) {
        out.push({ automationId: ref.id, automationName: ref.name, index: a.button.index, label: a.button.label });
      }
    }
  }
  return out;
}

// ── Writes (spec 6.10) ───────────────────────────────────────────────────────
// Inside a $transaction callback only `tx` may be used: the connection pool can be a single connection (connection_limit=1), so a
// call through the global client would wait for the transaction it is part of.

/** Normalises and validates the fields of a create / replace. @throws {AutomationError} */
function prepareWrite(data) {
  const trigger = data?.trigger;
  if (!isTrigger(trigger)) throw new AutomationError('Unknown trigger.');
  const c = normalizeList(data.conditions ?? [], 'condition');
  if (c.invalid.length) throw new AutomationError(`Condition ${c.invalid[0].index}: ${c.invalid[0].error}`);
  const a = normalizeList(data.actions ?? [], 'action');
  if (a.invalid.length) throw new AutomationError(`Action ${a.invalid[0].index}: ${a.invalid[0].error}`);
  const cooldownSeconds = data.cooldownSeconds ?? 0;
  const errors = validateAutomationShape({ trigger, conditions: c.items, actions: a.items, cooldownSeconds });
  if (errors.length) throw new AutomationError(errors.join('\n'));
  return {
    trigger,
    conditions: c.items,
    actions: a.items,
    cooldownSeconds,
    // A row with no actions never runs and cannot be enabled.
    enabled: data.enabled === true && a.items.length > 0,
  };
}

const rewriteSelfRefs = (conditions, from, to) => conditions.map((c) => (c.type === 'BUTTON_IS'
  ? { ...c, buttons: c.buttons.map((b) => (b.automationId === from ? { ...b, automationId: to } : b)) }
  : c));
const hasSelfRef = (conditions, id) => conditions.some((c) => c.type === 'BUTTON_IS' && c.buttons.some((b) => b.automationId === id));

/**
 * Creates an automation under the per-guild cap. `selfRefId` (JSON import) is the exported id: BUTTON_IS entries that carry it are
 * rewritten to the new row's id inside the transaction.
 * @returns {Promise<object>} the row (hydrated)
 * @throws {AutomationError} invalid shape or the cap (code LIMIT); Prisma P2002 is rethrown for a taken name
 */
export async function createAutomation(guildId, data, createdBy, { selfRefId = null } = {}) {
  if (!createdBy) throw new TypeError('createAutomation: createdBy is required');
  const name = normalizeAutomationName(data?.name);
  if (!name) throw new AutomationError(ERROR_TEXTS.name, 'NAME');
  const w = prepareWrite(data);
  await ensureGuildSettings(guildId);
  const row = await prisma.$transaction(async (tx) => {
    await tx.$queryRaw(lockGuildSql(guildId));
    const count = await tx.automation.count({ where: { guildId } });
    if (count >= LIMITS.AUTOMATIONS) throw new AutomationError(ERROR_TEXTS.limit(), 'LIMIT');
    const created = await tx.automation.create({
      data: {
        guildId, name, trigger: w.trigger, conditions: w.conditions, actions: w.actions,
        enabled: w.enabled, cooldownSeconds: w.cooldownSeconds, buttonSeq: highestButtonIndex(w.actions), createdBy,
      },
    });
    if (selfRefId && hasSelfRef(w.conditions, selfRefId)) {
      return tx.automation.update({ where: { id: created.id }, data: { conditions: rewriteSelfRefs(w.conditions, selfRefId, created.id) } });
    }
    return created;
  });
  await invalidateAutomations(guildId);
  return hydrate(row);
}

/**
 * Read-modify-write of one automation under a row lock. `mutator(current)` is pure and synchronous: it receives the row with
 * normalised arrays and returns `{ data, result }` (data = the columns to change; empty = no write). It may throw AutomationError.
 * @returns {Promise<{ row: object, result: any }>}
 * @throws {AutomationError} not found (code NOTFOUND) or an invalid result; Prisma P2002 on a rename to a taken name
 */
export async function mutateAutomation(guildId, name, mutator) {
  const out = await prisma.$transaction(async (tx) => {
    const locked = await tx.$queryRaw(lockByNameSql(guildId, name));
    const id = locked?.[0]?.id;
    const row = id ? await tx.automation.findUnique({ where: { id } }) : null;
    if (!row) throw new AutomationError(ERROR_TEXTS.notFound(name), 'NOTFOUND');
    const current = hydrate(row);
    const { data = {}, result = undefined } = mutator(current) ?? {};
    const errors = validateAutomationShape({ ...current, ...data });
    if (errors.length) throw new AutomationError(errors.join('\n'));
    const updated = Object.keys(data).length ? await tx.automation.update({ where: { id: row.id }, data }) : row;
    return { row: updated, result };
  });
  await invalidateAutomations(guildId);
  return { row: hydrate(out.row), result: out.result };
}

/**
 * Import with overwrite: replaces trigger, conditions, actions, enabled and cooldown of the row (and the name when `data.name`
 * differs). `uses` and `createdBy` are kept, and buttonSeq only grows, so a number that was ever used is never reused.
 * @throws {AutomationError} invalid shape or NOTFOUND; Prisma P2002 on a rename to a taken name
 */
export async function replaceAutomation(guildId, id, data) {
  const w = prepareWrite(data);
  const newName = data?.name == null ? null : normalizeAutomationName(data.name);
  if (data?.name != null && !newName) throw new AutomationError(ERROR_TEXTS.name, 'NAME');
  const row = await prisma.$transaction(async (tx) => {
    const locked = await tx.$queryRaw(lockByIdSql(guildId, id));
    const found = locked?.[0]?.id ? await tx.automation.findUnique({ where: { id: locked[0].id } }) : null;
    if (!found) throw new AutomationError(ERROR_TEXTS.notFound(newName ?? '?'), 'NOTFOUND');
    return tx.automation.update({
      where: { id: found.id },
      data: {
        ...(newName && newName !== found.name ? { name: newName } : {}),
        trigger: w.trigger, conditions: w.conditions, actions: w.actions, enabled: w.enabled, cooldownSeconds: w.cooldownSeconds,
        buttonSeq: Math.max(found.buttonSeq ?? 0, highestButtonIndex(w.actions)),
      },
    });
  });
  await invalidateAutomations(guildId);
  return hydrate(row);
}

/** @returns {Promise<number>} rows deleted */
export async function deleteAutomation(guildId, id) {
  const { count } = await prisma.automation.deleteMany({ where: { guildId, id } });
  await invalidateAutomations(guildId);
  return count;
}

/** Fire-and-forget run counter (the Int column is clamped; no updatedAt change). */
function bumpUses(id) {
  prisma.$executeRaw(usesSql(id)).catch((err) => logger.debug(`automations: uses bump failed (${err?.message})`));
}

// ── Option -> parameter mapping for /automation (spec 4.5) ───────────────────

const TEXT_OR_ANNOUNCEMENT = Object.freeze([ChannelType.GuildText, ChannelType.GuildAnnouncement]);

const CONDITION_OPTION_USE = Object.freeze({
  CHANNEL_IN: ['channel', 'text'], CHANNEL_NOT_IN: ['channel', 'text'], CATEGORY_IN: ['channel', 'text'], CATEGORY_NOT_IN: ['channel', 'text'],
  CONTAINS: ['text'], NOT_CONTAINS: ['text'], EXACT: ['text'], MATCHES_REGEX: ['text'],
  ATTACHMENT_AUDIO: [], ATTACHMENT_IMAGE: [], ATTACHMENT_VIDEO: [], ATTACHMENT_TEXT: [], IS_REPLY: [], IS_NOT_REPLY: [],
  REACTED_WITH: ['text'], HAS_REACTIONS: ['number', 'text'],
  HAS_ANY_ROLE: ['role', 'text'], HAS_ALL_ROLES: ['role', 'text'], HAS_NOT_ANY_ROLE: ['role', 'text'], HAS_NOT_ALL_ROLES: ['role', 'text'],
  GAINED_ROLE: ['role', 'text'], LOST_ROLE: ['role', 'text'], USER_IS: ['user', 'text'], BUTTON_IS: ['button'],
});
const ALL_CONDITION_OPTIONS = Object.freeze(['channel', 'role', 'user', 'text', 'number', 'button']);

const ACTION_OPTION_USE = Object.freeze({
  CREATE_THREAD: ['text', 'channel'], SEND_MESSAGE: ['text', 'channel', 'button_label', 'button_url'],
  REPLY: ['text', 'button_label', 'button_url'], REPOST: ['channel'], PIN: [], DELETE: [], REACT: ['emoji'], REMOVE_REACTION: ['emoji'],
  GIVE_ROLES: ['role', 'text'], TAKE_ROLES: ['role', 'text'], ADD_TO_THREAD: ['user', 'text'],
  SEND_IN_THREAD: ['text', 'button_label', 'button_url'],
  GIVE_COINS: ['amount'], TAKE_COINS: ['amount'], GIVE_XP: ['amount'], TAKE_XP: ['amount'], SEND_EMBED: ['embed', 'channel'],
});
const ALL_ACTION_OPTIONS = Object.freeze(['channel', 'text', 'role', 'user', 'emoji', 'amount', 'embed', 'button_label', 'button_url']);

const OPT_KEY = Object.freeze({ button_label: 'buttonLabel', button_url: 'buttonUrl' });
const given = (opts, name) => {
  const v = opts?.[OPT_KEY[name] ?? name];
  return v !== undefined && v !== null && v !== '';
};
const ignoredOptions = (used, all, opts) => all.filter((n) => given(opts, n) && !(used ?? []).includes(n));

const fail = (error, errorKind = 'value', extra = {}) => ({ ok: false, error, errorKind, ...extra });

export function emojiResolver(guild) {
  return (idOrName) => {
    const find = (store) => store?.get?.(idOrName) ?? store?.find?.((e) => e.name === idOrName) ?? null;
    const e = find(guild?.emojis?.cache) ?? find(guild?.client?.emojis?.cache);
    return e ? { id: e.id, name: e.name, animated: e.animated === true } : null;
  };
}

const unique = (arr) => [...new Set(arr)];

/** The role option plus role mentions / ids in `text`, each checked to exist; @everyone is refused. */
function collectRoles(opts, guild) {
  const ids = [];
  if (opts.role) ids.push(opts.role.id);
  for (const id of parseIdList(opts.text, 'role')) {
    if (!guild.roles.cache.has(id)) return { error: `Unknown role: ${id}` };
    ids.push(id);
  }
  const roles = unique(ids);
  for (const id of roles) if (id === guild.id) return { error: 'The @everyone role cannot be used here.' };
  return roles.length ? { ids: roles } : { error: 'Pick a role or mention roles in text.' };
}

/** The user option plus user mentions / ids in `text`, each resolved to a member (cache, then one fetch). */
async function collectUsers(opts, guild, { required }) {
  const members = [];
  if (opts.user) members.push({ id: opts.user.id, bot: opts.user.bot === true });
  for (const id of parseIdList(opts.text, 'user')) {
    const m = guild.members.cache.get(id) ?? await guild.members.fetch(id).catch(() => null);
    if (!m) return { error: `Unknown user: ${id}` };
    members.push({ id, bot: m.user?.bot === true });
  }
  const seen = new Set();
  const list = members.filter((m) => (seen.has(m.id) ? false : (seen.add(m.id), true)));
  if (required && !list.length) return { error: 'Pick a user or mention users in text.' };
  return { members: list };
}

function buttonFromOptions(opts, type) {
  const label = typeof opts.buttonLabel === 'string' ? opts.buttonLabel.trim() : '';
  const url = typeof opts.buttonUrl === 'string' ? opts.buttonUrl.trim() : '';
  if (!ACTION_TYPES[type].button) return { value: undefined };
  if (!label && !url) return { value: null };
  if (!label) return { error: 'Give button_label too.' };
  if (label.length > BUTTON_LABEL_MAX) return { error: `The button label can be at most ${BUTTON_LABEL_MAX} characters.` };
  if (url) {
    if (url.length > BUTTON_URL_MAX || !isHttpUrl(url)) return { error: 'The button link must start with http:// or https://.' };
    return { value: { label, url, style: 'PRIMARY', index: null } };
  }
  // An automation button: its number is assigned under the row lock by appendAction.
  return { value: { label, url: null, style: 'PRIMARY', index: null } };
}

function channelProblem(channel, check) {
  return check.missing?.length
    ? fail(check.reason, 'perms', { missing: check.missing, channel })
    : fail(check.reason ?? 'That channel cannot be used.', 'value');
}

/**
 * Maps the `condition add` options of one type to condition parameters (spec 4.5). Async because ids may need a member fetch or a
 * lookup of the guild's automation buttons. Never throws.
 * @returns {Promise<{ ok: boolean, params?: object, error?: string, errorKind?: string, ignored: string[] }>}
 */
export async function conditionParamsFromOptions(type, opts = {}, { guild } = {}) {
  if (!isConditionType(type)) return { ...fail('Unknown condition type. Pick one from the list.'), ignored: [] };
  const ignored = ignoredOptions(CONDITION_OPTION_USE[type], ALL_CONDITION_OPTIONS, opts);
  const done = (r) => ({ ignored, ...r });
  try {
    switch (type) {
      case 'CHANNEL_IN':
      case 'CHANNEL_NOT_IN':
      case 'CATEGORY_IN':
      case 'CATEGORY_NOT_IN': {
        const wantCategory = type.startsWith('CATEGORY');
        const picked = [];
        if (opts.channel) picked.push(guild.channels.cache.get(opts.channel.id) ?? opts.channel);
        for (const id of parseIdList(opts.text, 'channel')) {
          const ch = guild.channels.cache.get(id);
          if (!ch) return done(fail(`Unknown channel: ${id}`));
          picked.push(ch);
        }
        const ids = [];
        for (const ch of picked) {
          const isCategory = ch.type === ChannelType.GuildCategory;
          if (wantCategory && !isCategory) return done(fail('Pick a category (not a channel).'));
          if (!wantCategory && isCategory) return done(fail('That is a category. Use "Category is one of" instead.'));
          ids.push(ch.id);
        }
        if (!ids.length) {
          return done(fail(wantCategory ? 'Pick a category (not a channel).' : 'Pick a channel or mention channels in text.'));
        }
        return done({ ok: true, params: { [wantCategory ? 'categoryIds' : 'channelIds']: unique(ids) } });
      }
      case 'CONTAINS':
      case 'NOT_CONTAINS': {
        const words = parseWords(opts.text);
        if (!words.length) return done(fail('Give words in text, separated by commas.'));
        if (words.some((w) => w.length > 100)) return done(fail('Each word can be at most 100 characters.'));
        return done({ ok: true, params: { words } });
      }
      case 'EXACT': {
        const t = typeof opts.text === 'string' ? opts.text.trim() : '';
        if (!t) return done(fail('Give the exact text in text.'));
        if (t.length > 500) return done(fail('The text can be at most 500 characters.'));
        return done({ ok: true, params: { texts: [t] } });
      }
      case 'MATCHES_REGEX': {
        const t = typeof opts.text === 'string' ? opts.text.trim() : '';
        if (!t) return done(fail('Give the regex in text.'));
        const m = /^\/(.+)\/([a-z]*)$/s.exec(t);
        const pattern = m ? m[1] : t;
        const flags = m ? m[2] : 'i';
        const v = validateRegex(pattern, flags);
        if (!v.ok) return done(fail(`Invalid regex: ${v.reason}`));
        return done({ ok: true, params: { pattern, flags } });
      }
      case 'ATTACHMENT_AUDIO': case 'ATTACHMENT_IMAGE': case 'ATTACHMENT_VIDEO': case 'ATTACHMENT_TEXT':
      case 'IS_REPLY': case 'IS_NOT_REPLY':
        return done({ ok: true, params: {} });
      case 'REACTED_WITH': {
        const tokens = String(opts.text ?? '').trim().split(/\s+/).filter(Boolean);
        if (!tokens.length || tokens.length > 10) return done(fail('Give one or more emojis in text.'));
        const resolver = emojiResolver(guild);
        const emojis = [];
        for (const t of tokens) {
          const e = parseEmojiInput(t, resolver);
          if (!e.ok) return done(fail(e.error));
          emojis.push(e.value);
        }
        return done({ ok: true, params: { emojis } });
      }
      case 'HAS_REACTIONS': {
        if (!Number.isInteger(opts.number)) return done(fail('Give the minimum count in number.'));
        let emoji = null;
        if (typeof opts.text === 'string' && opts.text.trim()) {
          const e = parseEmojiInput(opts.text, emojiResolver(guild));
          if (!e.ok) return done(fail(e.error));
          emoji = e.value;
        }
        return done({ ok: true, params: { min: opts.number, emoji } });
      }
      case 'HAS_ANY_ROLE': case 'HAS_ALL_ROLES': case 'HAS_NOT_ANY_ROLE': case 'HAS_NOT_ALL_ROLES':
      case 'GAINED_ROLE': case 'LOST_ROLE': {
        const r = collectRoles(opts, guild);
        return done(r.error ? fail(r.error) : { ok: true, params: { roleIds: r.ids } });
      }
      case 'USER_IS': {
        const u = await collectUsers(opts, guild, { required: true });
        return done(u.error ? fail(u.error) : { ok: true, params: { userIds: u.members.map((m) => m.id) } });
      }
      case 'BUTTON_IS': {
        const m = /^([a-z0-9]{8,40}):(\d{1,2})$/i.exec(String(opts.button ?? '').trim());
        if (!m) return done(fail('Pick a button from the list.'));
        const index = Number(m[2]);
        const exists = (await listAutomationButtons(guild.id)).some((b) => b.automationId === m[1] && b.index === index);
        if (!exists) return done(fail('Pick a button from the list.'));
        return done({ ok: true, params: { buttons: [{ automationId: m[1], index }] } });
      }
      default:
        return done(fail('Unknown condition type. Pick one from the list.'));
    }
  } catch (err) {
    logger.warn('automations: conditionParamsFromOptions failed', err);
    return done(fail('I could not read those values.'));
  }
}

/**
 * Maps the `action add` options of one type to an action (spec 4.5) and runs the add-time checks: bot permissions, assignable
 * roles, reward modules and saved embeds. `params.button.index` is left null: appendAction assigns it under the row lock.
 * @returns {Promise<{ ok: boolean, params?: object, error?: string, errorKind?: 'value'|'perms'|'role'|'reward'|'embed',
 *   missing?: string[], channel?: object, rewardKind?: string, embedName?: string, ignored: string[] }>}
 */
export async function actionParamsFromOptions(type, opts = {}, { guild, member = null, settings = null } = {}) {
  if (!isActionType(type)) return { ...fail('Unknown action type. Pick one from the list.'), ignored: [] };
  const ignored = ignoredOptions(ACTION_OPTION_USE[type], ALL_ACTION_OPTIONS, opts);
  const done = (r) => ({ ignored, ...r });
  try {
    const targetChannel = opts.channel ? (guild.channels.cache.get(opts.channel.id) ?? opts.channel) : null;
    const text = typeof opts.text === 'string' ? opts.text.trim() : '';
    const withButton = (action) => {
      const b = buttonFromOptions(opts, type);
      if (b.error) return done(fail(b.error));
      return done({ ok: true, params: { ...action, button: b.value ?? null } });
    };
    switch (type) {
      case 'CREATE_THREAD': {
        const name = text || DEFAULT_THREAD_NAME;
        if (name.length > THREAD_NAME_MAX) return done(fail(`The thread name can be at most ${THREAD_NAME_MAX} characters.`));
        if (targetChannel) {
          if (!TEXT_OR_ANNOUNCEMENT.includes(targetChannel.type)) return done(fail('Pick a text or announcement channel.'));
          const missing = missingBotPermissions(targetChannel, ['ViewChannel', 'CreatePublicThreads', 'SendMessagesInThreads']);
          if (missing.length) return done(fail(`I need ${permissionNames(missing).join(', ')} in <#${targetChannel.id}>.`, 'perms', { missing, channel: targetChannel }));
        }
        return done({ ok: true, params: { name, channelId: targetChannel?.id ?? null, autoArchiveMinutes: 1440 } });
      }
      case 'SEND_MESSAGE': {
        if (!text) return done(fail('Give the message in text.'));
        if (text.length > CONTENT_MAX) return done(fail(`The message can be at most ${CONTENT_MAX} characters.`));
        if (targetChannel) {
          const check = checkChannelUsable(targetChannel, { needs: ['ViewChannel', 'SendMessages'] });
          if (!check.ok) return done(channelProblem(targetChannel, check));
        }
        return withButton({ content: text, channelId: targetChannel?.id ?? null });
      }
      case 'REPLY':
      case 'SEND_IN_THREAD': {
        if (!text) return done(fail('Give the message in text.'));
        if (text.length > CONTENT_MAX) return done(fail(`The message can be at most ${CONTENT_MAX} characters.`));
        return withButton({ content: text });
      }
      case 'REPOST': {
        if (!targetChannel) return done(fail('Pick the channel to repost to.'));
        const check = checkChannelUsable(targetChannel, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks'] });
        if (!check.ok) return done(channelProblem(targetChannel, check));
        return done({ ok: true, params: { channelId: targetChannel.id } });
      }
      case 'PIN':
      case 'DELETE':
        return done({ ok: true, params: {} });
      case 'REACT':
      case 'REMOVE_REACTION': {
        const raw = typeof opts.emoji === 'string' ? opts.emoji.trim() : '';
        if (!raw) return done(type === 'REACT' ? fail('Give the emoji in emoji.') : { ok: true, params: { emoji: null } });
        const e = parseEmojiInput(raw, emojiResolver(guild));
        return done(e.ok ? { ok: true, params: { emoji: e.value } } : fail(e.error));
      }
      case 'GIVE_ROLES':
      case 'TAKE_ROLES': {
        const r = collectRoles(opts, guild);
        if (r.error) return done(fail(r.error));
        if (r.ids.length > 10) return done(fail('Pick at most 10 roles.'));
        for (const id of r.ids) {
          const check = checkAssignableRole(guild, guild.roles.cache.get(id), { executor: member });
          if (!check.ok) return done(fail(check.reason, 'role'));
        }
        return done({ ok: true, params: { roleIds: r.ids } });
      }
      case 'ADD_TO_THREAD': {
        const u = await collectUsers(opts, guild, { required: false });
        if (u.error) return done(fail(u.error));
        if (u.members.some((m) => m.bot)) return done(fail('Bots cannot be added to threads by automations.'));
        if (u.members.length > 10) return done(fail('Pick at most 10 members.'));
        return done({ ok: true, params: { userIds: u.members.map((m) => m.id) } });
      }
      case 'GIVE_COINS': case 'TAKE_COINS': case 'GIVE_XP': case 'TAKE_XP': {
        if (!Number.isInteger(opts.amount)) return done(fail('Give the amount.'));
        const kind = ACTION_TYPES[type].reward;
        if (!rewards.isRewardAvailable(kind, settings)) return done(fail('That reward is not available.', 'reward', { rewardKind: kind }));
        return done({ ok: true, params: { amount: opts.amount } });
      }
      case 'SEND_EMBED': {
        const embedName = normalizeAutomationName(opts.embed);
        if (!embedName) return done(fail('Give the saved embed name in embed.'));
        if (!(await getSavedEmbedTemplate(guild.id, embedName))) return done(fail('Embed not found.', 'embed', { embedName }));
        if (targetChannel) {
          const check = checkChannelUsable(targetChannel, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks'] });
          if (!check.ok) return done(channelProblem(targetChannel, check));
        }
        return done({ ok: true, params: { embedName, channelId: targetChannel?.id ?? null } });
      }
      default:
        return done(fail('Unknown action type. Pick one from the list.'));
    }
  } catch (err) {
    logger.warn('automations: actionParamsFromOptions failed', err);
    return done(fail('I could not read those values.'));
  }
}

// ── Import references (spec 6.12) ────────────────────────────────────────────

/**
 * Checks an imported automation against this server and remaps button references. Returns a copy of `data` and the problems:
 * `warnings` make the import save the automation switched off; `info` lines (a remapped button reference) do not.
 * @returns {Promise<{ data: object, warnings: string[], info: string[] }>}
 */
export async function resolveImportReferences(guild, data, { settings = null, existingRow = null } = {}) {
  const warnings = [];
  const info = [];
  const conditions = structuredClone(data.conditions ?? []);
  const actions = structuredClone(data.actions ?? []);
  const selfId = existingRow?.id ?? null;
  const exportedId = data.exportedId ?? null;
  const refs = await listAutomationRefs(guild.id);
  const byId = new Map(refs.map((r) => [r.id, r]));
  const byName = new Map(refs.map((r) => [r.name, r]));
  const channels = guild.channels.cache;
  const roles = guild.roles.cache;

  const memberIds = new Set();
  const checkChannel = (label, id, { category = false } = {}) => {
    const ch = channels.get(id);
    if (!ch) warnings.push(`${label}: Unknown channel ${id}`);
    else if (category && ch.type !== ChannelType.GuildCategory) warnings.push(`${label}: ${id} is not a category`);
  };
  const checkRole = (label, id, { assignable = false } = {}) => {
    const role = roles.get(id);
    if (!role) { warnings.push(`${label}: Unknown role ${id}`); return; }
    if (assignable) {
      const check = checkAssignableRole(guild, role);
      if (!check.ok) warnings.push(`${label}: ${check.reason}`);
    }
  };

  conditions.forEach((c, k) => {
    const label = `Condition ${k + 1}`;
    if (c.type === 'CHANNEL_IN' || c.type === 'CHANNEL_NOT_IN') c.channelIds.forEach((id) => checkChannel(label, id));
    else if (c.type === 'CATEGORY_IN' || c.type === 'CATEGORY_NOT_IN') c.categoryIds.forEach((id) => checkChannel(label, id, { category: true }));
    else if (c.roleIds) c.roleIds.forEach((id) => checkRole(label, id));
    else if (c.type === 'USER_IS') c.userIds.forEach((id) => memberIds.add(id));
    else if (c.type === 'BUTTON_IS') {
      for (const b of c.buttons) {
        if (exportedId && b.automationId === exportedId) {
          // A self reference: an existing row keeps its id; a new row gets the created id inside createAutomation (selfRefId).
          if (selfId) b.automationId = selfId;
        } else if (!byId.has(b.automationId)) {
          const hinted = b.automationName ? byName.get(normalizeAutomationName(b.automationName) ?? '') : null;
          if (hinted) {
            b.automationId = hinted.id;
            info.push(`${label}: button reference remapped to "${hinted.name}".`);
          } else {
            warnings.push(`${label}: refers to a button of an automation that does not exist here.`);
          }
        }
      }
    }
  });

  for (const [k, a] of actions.entries()) {
    const label = `Action ${k + 1}`;
    if (a.channelId) checkChannel(label, a.channelId);
    if (a.roleIds) a.roleIds.forEach((id) => checkRole(label, id, { assignable: true }));
    if (a.userIds) a.userIds.forEach((id) => memberIds.add(id));
    if (a.type === 'SEND_EMBED' && !(await getSavedEmbedTemplate(guild.id, a.embedName))) warnings.push(`${label}: Saved embed "${a.embedName}" not found`);
    const kind = ACTION_TYPES[a.type]?.reward;
    if (kind && !rewards.isRewardAvailable(kind, settings)) {
      warnings.push(`${label}: ${kind === 'xp' ? 'XP' : 'Coins'} rewards are not available (module off)`);
    }
  }

  let fetches = 0;
  for (const id of memberIds) {
    if (guild.members.cache.has(id)) continue;
    if (fetches >= MAX_IMPORT_MEMBER_FETCHES) { warnings.push(`Member ${id} could not be checked (too many members listed)`); continue; }
    fetches++;
    const m = await guild.members.fetch(id).catch(() => null);
    if (!m) warnings.push(`Unknown member ${id}`);
  }

  // Normalising again drops the automationName hints and duplicates that a remap created.
  return {
    data: {
      ...data,
      conditions: normalizeList(conditions, 'condition').items,
      actions: normalizeList(actions, 'action').items,
    },
    warnings,
    info,
  };
}

// ── Problems (spec 6.13) ─────────────────────────────────────────────────────

/**
 * Live checks of a stored automation for /automation info and enable: invalid stored elements, deleted channels and roles, missing
 * permissions, saved embeds, reward modules, dangling button references and the last runtime problem. One line each.
 * @param {object} guild
 * @param {object} row a row from getAutomationByName / mutateAutomation (hydrated)
 * @returns {Promise<string[]>}
 */
export async function automationProblems(guild, row, settings) {
  const out = [];
  try {
    for (const inv of row.invalidConditions ?? []) out.push(`Condition ${inv.index} is invalid and ignored: ${inv.error}`);
    for (const inv of row.invalidActions ?? []) out.push(`Action ${inv.index} is invalid and ignored: ${inv.error}`);
    if (row.enabled && row.actions.length === 0) out.push('It is on but has no actions, so it never runs.');
    if (guild.available === false) return out;

    const channels = guild.channels.cache;
    const roles = guild.roles.cache;
    const chanGone = (label, id) => { if (!channels.has(id)) { out.push(`${label}: the channel ${id} was deleted.`); return true; } return false; };
    const roleGone = (label, id) => { if (!roles.has(id)) { out.push(`${label}: the role ${id} was deleted.`); return true; } return false; };

    const refs = (row.conditions ?? []).some((c) => c.type === 'BUTTON_IS') ? await listAutomationRefs(guild.id) : [];
    const byId = new Map(refs.map((r) => [r.id, r]));

    row.conditions.forEach((c, k) => {
      const label = `Condition ${k + 1}`;
      for (const id of c.channelIds ?? []) chanGone(label, id);
      for (const id of c.categoryIds ?? []) chanGone(label, id);
      for (const id of c.roleIds ?? []) roleGone(label, id);
      for (const b of c.buttons ?? []) {
        const ref = byId.get(b.automationId);
        if (!ref) out.push(`${label}: refers to a button of an automation that no longer exists.`);
        else if (!ref.actions.some((a) => a.button && a.button.url == null && a.button.index === b.index)) {
          out.push(`${label}: button ${b.index} no longer exists on "${ref.name}".`);
        }
      }
    });

    for (const [k, a] of row.actions.entries()) {
      const label = `Action ${k + 1}`;
      let target = null;
      if (a.channelId && !chanGone(label, a.channelId)) target = channels.get(a.channelId);
      for (const id of a.roleIds ?? []) {
        if (!roleGone(label, id)) {
          const check = checkAssignableRole(guild, roles.get(id));
          if (!check.ok) out.push(`${label}: ${check.reason}`);
        }
      }
      if (target) {
        let need = null;
        if (a.type === 'CREATE_THREAD') {
          const missing = missingBotPermissions(target, ['ViewChannel', 'CreatePublicThreads', 'SendMessagesInThreads']);
          if (missing.length) out.push(`${label}: I need **${permissionNames(missing).join(', ')}** in <#${target.id}>.`);
        } else if (a.type === 'SEND_MESSAGE') need = ['ViewChannel', 'SendMessages'];
        else if (a.type === 'SEND_EMBED' || a.type === 'REPOST') need = ['ViewChannel', 'SendMessages', 'EmbedLinks'];
        if (need) {
          const check = checkChannelUsable(target, { needs: need });
          if (!check.ok) out.push(`${label}: ${check.reason}`);
        }
      }
      if (a.type === 'SEND_EMBED' && !(await getSavedEmbedTemplate(guild.id, a.embedName))) {
        out.push(`${label}: the saved embed "${a.embedName}" was not found.`);
      }
      const kind = ACTION_TYPES[a.type]?.reward;
      if (kind && !rewards.isRewardAvailable(kind, settings)) {
        out.push(`${label}: ${kind === 'xp' ? 'XP' : 'Coin'} rewards need the ${kind === 'xp' ? 'Levels' : 'Economy'} module.`);
      }
    }

    const last = await cache.get(problemKey(row.id));
    if (last && typeof last.text === 'string') {
      const sec = Math.floor(Date.parse(last.at) / 1000);
      out.push(Number.isFinite(sec) ? `Last problem <t:${sec}:R>: ${last.text}` : `Last problem: ${last.text}`);
    }
  } catch (err) {
    logger.warn('automations: automationProblems failed', err);
  }
  return out;
}

// ── Facts (spec 6.5) ─────────────────────────────────────────────────────────

const reactionKeyOf = (reaction) => reaction?.emoji?.id ?? stripFE0F(reaction?.emoji?.name);

/** Plain facts for evaluateCondition. Reads discord.js objects defensively and never throws. */
export function buildFacts(trigger, base) {
  const b = base ?? {};
  const channel = b.channel ?? null;
  const isThread = typeof channel?.isThread === 'function' && channel.isThread();
  const message = b.message ?? null;
  const facts = {
    userId: b.user?.id ?? b.member?.id ?? null,
    channelId: channel?.id ?? null,
    parentId: isThread ? (channel.parentId ?? null) : null,
    categoryId: isThread ? (channel.parent?.parentId ?? null) : (channel?.parentId ?? null),
    content: null,
    attachments: null,
    isReply: null,
    reactionKey: null,
    reactionCounts: null,
    memberRoleIds: null,
    changedRoleIds: null,
    button: null,
  };
  try {
    if (b.member?.roles?.cache) facts.memberRoleIds = [...b.member.roles.cache.keys()];
    if ((trigger === 'ROLE_GAINED' || trigger === 'ROLE_LOST') && Array.isArray(b.roleIds)) facts.changedRoleIds = b.roleIds;
    if (trigger === 'BUTTON_CLICKED' && b.button) facts.button = { automationId: b.button.automationId, index: b.button.index };
    if (trigger === 'REACTION_ADDED' && b.reaction?.key != null) facts.reactionKey = stripFE0F(b.reaction.key);
    if (message && !message.partial) {
      if (T_CONTENT.has(trigger)) {
        facts.content = message.content ?? '';
        facts.attachments = [...(message.attachments?.values?.() ?? [])].map((a) => ({ name: a.name, contentType: a.contentType }));
        facts.isReply = message.type === MessageType.Reply;
      }
      if (T_COUNTS.has(trigger) && message.reactions?.cache) {
        const counts = {};
        for (const r of message.reactions.cache.values()) {
          const key = reactionKeyOf(r);
          if (key) counts[key] = (counts[key] ?? 0) + (r.count ?? 0);
        }
        facts.reactionCounts = counts;
      }
    }
  } catch (err) {
    logger.debug(`automations: buildFacts failed (${err?.message})`);
  }
  return facts;
}

// ── Execution (spec 6.6) ─────────────────────────────────────────────────────

const BUTTON_STYLE_MAP = Object.freeze({
  PRIMARY: ButtonStyle.Primary, SECONDARY: ButtonStyle.Secondary, SUCCESS: ButtonStyle.Success, DANGER: ButtonStyle.Danger,
});

function recordProblem(a, i, text, guildId) {
  if (!text || !warnThrottle(`${a.id}:${i}`)) return;
  const action = a.actions[i];
  logger.warn(`automations: ${guildId} "${a.name}" action ${i + 1} (${action?.type}): ${text}`);
  const label = ACTION_TYPES[action?.type]?.label ?? 'Action';
  // Problem key writes ignore failures: it is a diagnostic shown by /automation info, nothing depends on it.
  cache.set(problemKey(a.id), { at: new Date().toISOString(), text: clampText(`Action ${i + 1} (${label}): ${text}`, 300) }, PROBLEM_TTL_SECONDS).catch(() => {});
}

const reasonFor = (a) => clampText(`Automation: ${a.name}`, 512);

function renderContext(base, run, channel = null) {
  return {
    member: base.member ?? null,
    user: base.user ?? null,
    guild: base.guild,
    // {channel} is where it happened, or the target channel for triggers that have no channel.
    channel: base.channel ?? channel ?? null,
    vars: buildVars(base, run),
  };
}

const renderText = (template, base, run, maxLength, channel = null) => renderPlaceholders(template, renderContext(base, run, channel), { maxLength });

/** Role mentions typed by the admin in the template ping only when pingableRoleIds allows them for the bot; users always may. */
function allowedMentionsFor(channel, guild, template) {
  const ids = unique([...String(template ?? '').matchAll(/<@&(\d{17,20})>/g)].map((m) => m[1]));
  if (!ids.length) return { parse: ['users'] };
  const { allowed } = pingableRoleIds(channel, guild?.members?.me ?? null, ids);
  return allowed.length ? { parse: ['users'], roles: allowed } : { parse: ['users'] };
}

function buttonComponents(automationId, button, base, run, channel) {
  if (!button) return [];
  const label = renderText(button.label, base, run, BUTTON_LABEL_MAX, channel).trim() || 'Click';
  const b = new ButtonBuilder().setLabel(clampText(label, BUTTON_LABEL_MAX));
  if (button.url != null) b.setStyle(ButtonStyle.Link).setURL(button.url);
  else {
    b.setStyle(BUTTON_STYLE_MAP[button.style] ?? ButtonStyle.Primary)
      .setCustomId(buildCustomId('automation', automationId, String(button.index)));
  }
  return [new ActionRowBuilder().addComponents(b)];
}

const channelFor = (action, base) => (action.channelId ? base.guild.channels.cache.get(action.channelId) ?? null : base.channel ?? null);

/** Resolves the send target; records a problem and returns null when it cannot be used. */
function usableTarget(action, base, run, i, needs) {
  const ch = channelFor(action, base);
  if (!ch) { recordProblem(run.automation, i, action.channelId ? 'The channel was deleted' : 'There is no channel for this action', base.guildId); return null; }
  const check = checkChannelUsable(ch, { needs });
  if (!check.ok) { recordProblem(run.automation, i, check.reason, base.guildId); return null; }
  return ch;
}

function missingPerms(run, base, i, channel, perms) {
  const missing = missingBotPermissions(channel, perms);
  if (!missing.length) return false;
  recordProblem(run.automation, i, `I need ${permissionNames(missing).join(', ')} in <#${channel?.id}>.`, base.guildId);
  return true;
}

async function repostEmbed(message, base, run) {
  const author = message.author;
  const member = base.member?.id === author?.id ? base.member : (message.member ?? null);
  const embed = guildEmbed(run.settings, null, clampText(message.content ?? '', 4096) || null);
  embed.setAuthor({
    name: clampText(member?.displayName ?? author?.username ?? 'Unknown', 256),
    iconURL: author?.displayAvatarURL?.({ extension: 'png', size: 128 }) ?? undefined,
  });
  const files = [...(message.attachments?.values?.() ?? [])];
  const image = files.find((a) => attachmentKind(a) === 'IMAGE' && !String(a.name ?? '').startsWith('SPOILER_'));
  if (image) embed.setImage(image.url);
  const others = files.filter((a) => a !== image).map((a) => `[${clampText(a.name ?? 'file', 40)}](${a.url})`);
  if (others.length) embed.addFields({ name: 'Attachments', value: joinWithLimit(others, { sep: '\n', max: 1024 }) });
  embed.setFooter({ text: clampText(`#${message.channel?.name ?? 'unknown'}`, 128) });
  embed.setTimestamp(message.createdTimestamp);
  return embed;
}

const hasKey = (reaction, key) => reactionKeyOf(reaction) === key;

async function roleChange(kind, action, run, base, i) {
  const member = base.member;
  if (!member) return 'skipped';
  const a = run.automation;
  // Captcha verification must not be bypassed by an automation: skip role gifts while it is pending (spec 1.3 #10).
  if (kind === 'GIVE' && a.trigger === 'MEMBER_JOINED' && base.captchaPending) {
    logger.debug(`automations: "${a.name}" skipped GIVE_ROLES for ${member.id}: verification is pending`);
    return 'skipped';
  }
  const guild = base.guild;
  let marked = false;
  let changed = 0;
  // One role at a time (00-overview rule 13): the array form PATCHes the whole role list.
  for (const id of action.roleIds) {
    try {
      const role = guild.roles.cache.get(id);
      if (!role) { recordProblem(a, i, 'The role was deleted', base.guildId); continue; }
      const check = checkAssignableRole(guild, role);
      if (!check.ok) { recordProblem(a, i, check.reason, base.guildId); continue; }
      const has = member.roles.cache.has(id);
      if (kind === 'GIVE' ? has : !has) continue;
      if (!marked) { causes.mark(guild.id, member.id, run.depth + 1); marked = true; }   // before the first Discord call
      await (kind === 'GIVE' ? member.roles.add(id, reasonFor(a)) : member.roles.remove(id, reasonFor(a)));
      changed++;
    } catch (err) {
      const text = discordErrorText(err);
      if (text) recordProblem(a, i, text, base.guildId); else logger.debug(`automations: role ${id} skipped (${err?.code})`);
    }
  }
  return changed > 0 ? 'done' : 'skipped';
}

async function rewardAction(op, kind, action, run, base, i) {
  const userId = base.member?.id ?? base.user?.id;
  if (!userId) return 'skipped';
  const a = run.automation;
  // Level-up role sync can fire ROLE_GAINED: mark the chain depth before the XP call.
  if (kind === 'xp') causes.mark(base.guildId, userId, run.depth + 1);
  const res = await rewards[op](kind, {
    guild: base.guild, userId, amount: action.amount, reason: `automation:${a.name}`, member: base.member ?? null, channel: base.channel ?? null,
  });
  if (res.ok) return 'done';
  const pkg = kind === 'xp' ? 'Levels' : 'Economy';
  if (res.reason === 'insufficient') logger.debug(`automations: "${a.name}" skipped ${op} ${kind}: insufficient balance`);
  else if (res.reason === 'module-disabled') recordProblem(a, i, `The ${pkg} module is off`, base.guildId);
  else if (res.reason === 'unavailable') recordProblem(a, i, `The ${pkg} package is not installed`, base.guildId);
  else recordProblem(a, i, res.reason ?? 'The reward failed', base.guildId);
  return 'skipped';
}

/** Each runner returns 'done' | 'skipped' and may throw: executeAutomation isolates every action. */
const ACTION_RUNNERS = Object.freeze({
  async CREATE_THREAD(action, run, base, i) {
    const a = run.automation;
    const guild = base.guild;
    const name = renderText(action.name, base, run, THREAD_NAME_MAX).trim() || 'Thread';
    const reason = reasonFor(a);
    if (action.channelId) {
      const ch = guild.channels.cache.get(action.channelId);
      if (!ch) { recordProblem(a, i, 'The channel was deleted', base.guildId); return 'skipped'; }
      if (!TEXT_OR_ANNOUNCEMENT.includes(ch.type) || !ch.threads?.create) { recordProblem(a, i, 'That channel cannot hold threads', base.guildId); return 'skipped'; }
      if (missingPerms(run, base, i, ch, ['ViewChannel', 'CreatePublicThreads', 'SendMessagesInThreads'])) return 'skipped';
      run.thread = await ch.threads.create({
        name,
        autoArchiveDuration: action.autoArchiveMinutes,
        type: ch.type === ChannelType.GuildAnnouncement ? ChannelType.AnnouncementThread : ChannelType.PublicThread,
        reason,
      });
      return 'done';
    }
    const message = base.message;
    if (!message || run.messageDeleted || !TEXT_OR_ANNOUNCEMENT.includes(base.channel?.type)) return 'skipped';
    if (message.hasThread) {
      run.thread = message.thread ?? await message.fetch(true).then((m) => m.thread).catch(() => null);
      return run.thread ? 'done' : 'skipped';
    }
    if (missingPerms(run, base, i, base.channel, ['ViewChannel', 'CreatePublicThreads', 'ReadMessageHistory'])) return 'skipped';
    try {
      run.thread = await message.startThread({ name, autoArchiveDuration: action.autoArchiveMinutes, reason });
    } catch (err) {
      if (err?.code !== RESTJSONErrorCodes.ThreadAlreadyCreatedForMessage) throw err;
      run.thread = await message.fetch(true).then((m) => m.thread);
    }
    return 'done';
  },

  async SEND_MESSAGE(action, run, base, i) {
    const ch = usableTarget(action, base, run, i, ['ViewChannel', 'SendMessages']);
    if (!ch) return 'skipped';
    const content = renderText(action.content, base, run, CONTENT_MAX, ch);
    if (!content.trim()) return 'skipped';   // Discord rejects an empty message
    await ch.send({
      content,
      components: buttonComponents(run.automation.id, action.button, base, run, ch),
      allowedMentions: allowedMentionsFor(ch, base.guild, action.content),
    });
    return 'done';
  },

  async SEND_EMBED(action, run, base, i) {
    const tpl = await getSavedEmbedTemplate(base.guildId, action.embedName);
    if (!tpl) { recordProblem(run.automation, i, `Saved embed "${action.embedName}" not found`, base.guildId); return 'skipped'; }
    const ch = usableTarget(action, base, run, i, ['ViewChannel', 'SendMessages', 'EmbedLinks']);
    if (!ch) return 'skipped';
    const payload = renderTemplate(tpl, renderContext(base, run, ch), {
      defaultColor: run.settings?.embedColor ?? COLORS.INFO,
      allowedMentions: allowedMentionsFor(ch, base.guild, tpl.content),
    });
    if (!payload) return 'skipped';
    await ch.send(payload);
    return 'done';
  },

  async REPLY(action, run, base, i) {
    const ch = base.channel;
    const content = renderText(action.content, base, run, CONTENT_MAX, ch);
    if (!content.trim()) return 'skipped';
    const components = buttonComponents(run.automation.id, action.button, base, run, ch);
    const allowedMentions = allowedMentionsFor(ch, base.guild, action.content);
    if (run.automation.trigger === 'BUTTON_CLICKED') {
      await base.interaction.followUp({ content, components, allowedMentions, ephemeral: true });
      return 'done';
    }
    const check = checkChannelUsable(ch, { needs: ['ViewChannel', 'SendMessages', 'ReadMessageHistory'] });
    if (!check.ok) { recordProblem(run.automation, i, check.reason, base.guildId); return 'skipped'; }
    await ch.send({
      content, components, allowedMentions,
      ...(run.messageDeleted || !base.message ? {} : { reply: { messageReference: base.message.id, failIfNotExists: false } }),
    });
    return 'done';
  },

  async REPOST(action, run, base, i) {
    const a = run.automation;
    const message = base.message;
    if (!message) return 'skipped';
    const ch = usableTarget(action, { ...base, channel: null }, run, i, ['ViewChannel', 'SendMessages', 'EmbedLinks']);
    if (!ch) return 'skipped';
    const key = repostKey(a.id, message.id);
    // Degrade decision: at most one repost per source message. setNX null (Redis down) falls back to this process's set (this
    // shard owns the guild), so a Redis outage can never repost the same message twice from here.
    const first = await cache.setNX(key, '1', REPOST_DEDUPE_TTL_SECONDS);
    if (first === false) return 'skipped';
    if (first === null && !localReposts.add(key, REPOST_DEDUPE_TTL_SECONDS * 1000)) return 'skipped';
    try {
      const embed = await repostEmbed(message, base, run);
      const components = a.trigger === 'MESSAGE_DELETED' || !message.url
        ? []
        : [new ActionRowBuilder().addComponents(new ButtonBuilder().setStyle(ButtonStyle.Link).setLabel('Jump to message').setURL(message.url))];
      await ch.send({ embeds: [embed], components, allowedMentions: { parse: [] } });
    } catch (err) {
      // The send failed: release the dedupe key so a later trigger can try again.
      await cache.del(key);
      localReposts.delete(key);
      throw err;
    }
    return 'done';
  },

  async PIN(action, run, base, i) {
    const message = base.message;
    if (!message || run.messageDeleted || message.pinned) return 'skipped';
    if (missingPerms(run, base, i, base.channel, ['ViewChannel', 'PinMessages', 'ReadMessageHistory'])) return 'skipped';
    await message.pin(reasonFor(run.automation));
    return 'done';
  },

  async DELETE(action, run, base, i) {
    const message = base.message;
    if (!message || run.messageDeleted) return 'skipped';
    if (message.author?.id !== base.client?.user?.id && missingPerms(run, base, i, base.channel, ['ViewChannel', 'ManageMessages'])) return 'skipped';
    // Logging and starboard read this mark: an automation delete is not a member delete.
    markBotDeletion(message.id, 'automations');
    try {
      await message.delete();
    } catch (err) {
      if (err?.code !== RESTJSONErrorCodes.UnknownMessage) throw err;
    }
    run.messageDeleted = true;
    return 'done';
  },

  async REACT(action, run, base, i) {
    const message = base.message;
    if (!message || run.messageDeleted) return 'skipped';
    const key = emojiKeyOf(action.emoji);
    if (message.reactions?.cache?.some((r) => r.me && hasKey(r, key))) return 'skipped';
    if (missingPerms(run, base, i, base.channel, ['ViewChannel', 'AddReactions', 'ReadMessageHistory'])) return 'skipped';
    await message.react(action.emoji);
    return 'done';
  },

  async REMOVE_REACTION(action, run, base, i) {
    const message = base.message;
    if (!message || run.messageDeleted) return 'skipped';
    const key = action.emoji ? emojiKeyOf(action.emoji) : base.reaction?.key;
    const userId = base.user?.id;
    if (key == null || !userId) return 'skipped';
    const reaction = message.reactions?.cache?.find((r) => hasKey(r, key));
    if (!reaction) return 'skipped';
    if (missingPerms(run, base, i, base.channel, ['ViewChannel', 'ManageMessages'])) return 'skipped';
    await reaction.users.remove(userId);
    return 'done';
  },

  GIVE_ROLES: (action, run, base, i) => roleChange('GIVE', action, run, base, i),
  TAKE_ROLES: (action, run, base, i) => roleChange('TAKE', action, run, base, i),

  async ADD_TO_THREAD(action, run, base, i) {
    const thread = run.thread;
    if (!thread || thread.archived || thread.locked) return 'skipped';
    const ids = action.userIds.length ? action.userIds : [base.member?.id ?? base.user?.id].filter(Boolean);
    let added = 0;
    for (const id of ids) {
      try {
        await thread.members.add(id);
        added++;
      } catch (err) {
        const text = discordErrorText(err);   // UnknownMember is silent
        if (text) recordProblem(run.automation, i, text, base.guildId);
      }
    }
    return added > 0 ? 'done' : 'skipped';
  },

  async SEND_IN_THREAD(action, run, base, i) {
    const thread = run.thread;
    if (!thread) return 'skipped';
    const check = checkChannelUsable(thread, { needs: ['ViewChannel', 'SendMessages'] });
    if (!check.ok) { recordProblem(run.automation, i, check.reason, base.guildId); return 'skipped'; }
    const content = renderText(action.content, base, run, CONTENT_MAX, thread);
    if (!content.trim()) return 'skipped';
    await thread.send({
      content,
      components: buttonComponents(run.automation.id, action.button, base, run, thread),
      allowedMentions: allowedMentionsFor(thread, base.guild, action.content),
    });
    return 'done';
  },

  GIVE_COINS: (action, run, base, i) => rewardAction('grant', 'coins', action, run, base, i),
  TAKE_COINS: (action, run, base, i) => rewardAction('take', 'coins', action, run, base, i),
  GIVE_XP: (action, run, base, i) => rewardAction('grant', 'xp', action, run, base, i),
  TAKE_XP: (action, run, base, i) => rewardAction('take', 'xp', action, run, base, i),
});

/** Runs one automation's actions in order; each action is isolated. Never throws. */
export async function executeAutomation(a, base) {
  try {
    const settings = typeof base.getSettings === 'function' ? await base.getSettings() : await getSettingsSafe(base.guildId);
    const run = {
      automation: a,
      depth: base.depth ?? 0,
      thread: base.thread ?? (typeof base.channel?.isThread === 'function' && base.channel.isThread() ? base.channel : null),
      messageDeleted: base.messageGone === true,
      settings,
    };
    let attempted = 0;
    for (const [i, action] of a.actions.entries()) {
      try {
        const runner = Object.hasOwn(ACTION_RUNNERS, action.type) ? ACTION_RUNNERS[action.type] : null;
        if (!runner) continue;
        const outcome = await runner(action, run, base, i);
        if (outcome !== 'skipped') attempted++;
      } catch (err) {
        attempted++;
        const text = discordErrorText(err);
        if (text) recordProblem(a, i, text, base.guildId);
        else logger.debug(`automations: "${a.name}" action ${i + 1} skipped (${err?.code})`);
      }
    }
    if (attempted) bumpUses(a.id);
  } catch (err) {
    logger.error(`automations: run of "${a?.name}" failed`, err);
  }
}

// ── Dispatch (spec 6.4, 6.7) ─────────────────────────────────────────────────

async function takeCooldown(guildId, automationId, userId, seconds) {
  const key = cooldownKey(guildId, automationId, userId);
  // Degrade decision: setNX true -> run, false -> skip, null (Redis down) -> the in-process set below. A guild's events always land
  // on its own shard, so that is consistent; the cooldown is never skipped and never fails open to "no cooldown".
  const r = await cache.setNX(key, '1', seconds);
  if (r === true) return true;
  if (r === false) return false;
  return localCooldowns.add(key, seconds * 1000);
}

function enqueueRun(guildId, fn) {
  if ((pending.get(guildId) ?? 0) >= MAX_PENDING_RUNS) {
    if (warnThrottle(`queue:${guildId}`)) logger.warn(`automations: run queue is full in ${guildId}; dropping a run`);
    return Promise.resolve(false);
  }
  pending.set(guildId, (pending.get(guildId) ?? 0) + 1);
  return guildQueue.run(guildId, () => withTimeout(fn(), RUN_TIMEOUT_MS, 'automation run')).finally(() => {
    const left = (pending.get(guildId) ?? 1) - 1;
    if (left <= 0) pending.delete(guildId); else pending.set(guildId, left);
  });
}

/**
 * Finds the automations of a trigger whose conditions pass and queues their runs (spec 6.4). Never throws.
 * @param {string} trigger
 * @param {object} base { guild, guildId, client, member, user, channel, message, depth, getSettings, ...trigger specifics }
 * @param {{ awaitRuns?: boolean }} [opts]
 */
export async function dispatch(trigger, base, { awaitRuns = false } = {}) {
  try {
    const guildId = base.guildId ?? base.guild?.id;
    if (!guildId || !isTrigger(trigger)) return;
    const b = { ...base, guildId };
    const byTrigger = await getAutomations(guildId);
    if (!byTrigger) return;   // fail-closed: nothing runs while Postgres is unreachable
    const autos = Object.hasOwn(byTrigger, trigger) && Array.isArray(byTrigger[trigger]) ? byTrigger[trigger] : [];
    if (autos.length === 0) return;
    if ((b.depth ?? 0) >= MAX_CHAIN_DEPTH) {
      if (chainThrottle(guildId)) logger.warn(`automations: chain depth ${b.depth} reached in ${guildId}; stopping`);
      return;
    }

    const facts0 = buildFacts(trigger, b);
    let candidates = autos.filter((a) => evaluateConditions(splitConditions(a.conditions).cheap, facts0));
    if (candidates.length === 0) return;

    let facts = facts0;
    if (trigger === 'REACTION_ADDED') {
      // The message is fetched only now that a cheap condition set passed. Bot-authored messages are allowed here (the reactor is
      // never a bot, so there is no loop); system messages (joins, pins) are not.
      const msg = typeof b.ensureMessage === 'function' ? await b.ensureMessage() : b.message;
      if (!msg || msg.system) return;
      b.message = msg;
      facts = buildFacts(trigger, b);
    }
    candidates = candidates.filter((a) => evaluateConditions(splitConditions(a.conditions).needsMessage, facts));

    for (const a of candidates) {
      if (a.cooldownSeconds > 0 && !(await takeCooldown(guildId, a.id, facts.userId ?? 'none', a.cooldownSeconds))) continue;
      if (!budget.take(guildId)) {
        if (warnThrottle(`budget:${guildId}`)) logger.warn(`automations: run budget exceeded in ${guildId}`);
        break;
      }
      const p = enqueueRun(guildId, () => executeAutomation(a, b));
      if (awaitRuns) await p.catch((err) => logger.error('automations: run failed', err));
      else p.catch((err) => logger.error('automations: run failed', err));
    }
  } catch (err) {
    logger.error(`automations: dispatch of ${trigger} failed`, err);
  }
}

// ── Hooks (spec 6.2) ─────────────────────────────────────────────────────────

const gid = (ctx) => ctx.guildId ?? ctx.guild?.id ?? null;

export const filterMessageCreate = (ctx) => peekHasTrigger(gid(ctx), 'MESSAGE_SENT');
export const filterMessageUpdate = (ctx) => peekHasTrigger(gid(ctx), 'MESSAGE_EDITED');
export const filterMessageDelete = (ctx) => !ctx.partial && !ctx.message?.author?.bot && !ctx.message?.webhookId
  && !ctx.message?.system && peekHasTrigger(gid(ctx), 'MESSAGE_DELETED');
export const filterReactionAdd = (ctx) => peekHasTrigger(gid(ctx), 'REACTION_ADDED');
export const filterVoiceState = (ctx) => ctx.change !== 'state'
  && (peekHasTrigger(gid(ctx), 'VOICE_JOINED') || peekHasTrigger(gid(ctx), 'VOICE_LEFT'));
export const filterMemberAdd = (ctx) => !ctx.member?.user?.bot && peekHasTrigger(gid(ctx), 'MEMBER_JOINED');
export const filterMemberUpdate = (ctx) => ctx.oldKnown === true && !ctx.newMember?.user?.bot
  && (peekHasTrigger(gid(ctx), 'ROLE_GAINED') || peekHasTrigger(gid(ctx), 'ROLE_LOST'));
// The owner check comes first: threads the bot itself creates (CREATE_THREAD, forum posts by the bot) never trigger anything.
export const filterThreadCreate = (ctx) => {
  const ownerId = ctx.thread?.ownerId;
  if (!ownerId || ownerId === ctx.client?.user?.id) return false;
  return peekHasTrigger(gid(ctx), 'THREAD_CREATED');
};

const baseOf = (ctx, extra) => ({
  guild: ctx.guild, guildId: gid(ctx), client: ctx.client, depth: 0,
  getSettings: typeof ctx.getSettings === 'function' ? ctx.getSettings : () => getSettingsSafe(gid(ctx)),
  ...extra,
});

export async function onMessageCreate(ctx) {
  try {
    const { message } = ctx;
    if (ctx.state?.automodAction?.deleted || wasDeletedByBot(message.id)) return;
    await dispatch('MESSAGE_SENT', baseOf(ctx, {
      trigger: 'MESSAGE_SENT', member: message.member ?? null, user: message.author, channel: message.channel, message,
    }));
  } catch (err) {
    logger.error('automations: messageCreate failed', err);
  }
}

export async function onMessageUpdate(ctx) {
  try {
    const message = ctx.newMessage ?? ctx.message;
    if (wasDeletedByBot(message.id)) return;
    await dispatch('MESSAGE_EDITED', baseOf(ctx, {
      trigger: 'MESSAGE_EDITED', member: message.member ?? null, user: message.author, channel: message.channel, message,
      oldContent: ctx.oldKnown ? ctx.oldMessage?.content ?? null : null,
    }));
  } catch (err) {
    logger.error('automations: messageUpdate failed', err);
  }
}

export async function onMessageDelete(ctx) {
  try {
    const { message, guild } = ctx;
    // Deletions made by the bot (automod, purge, custom commands, automations ...) are not member deletions.
    if (wasDeletedByBot(message.id)) return;
    const member = message.member ?? guild.members.cache.get(message.author.id) ?? null;
    await dispatch('MESSAGE_DELETED', baseOf(ctx, {
      trigger: 'MESSAGE_DELETED', member, user: message.author, channel: message.channel, message, messageGone: true,
    }));
  } catch (err) {
    logger.error('automations: messageDelete failed', err);
  }
}

export async function onReactionAdd(ctx) {
  try {
    const member = await ctx.getMember();
    if (!member) return;
    const { guild } = ctx;
    await dispatch('REACTION_ADDED', baseOf(ctx, {
      trigger: 'REACTION_ADDED', member, user: ctx.user,
      channel: ctx.message.channel ?? guild.channels.cache.get(ctx.message.channelId) ?? null,
      message: ctx.message,   // may be partial: the dispatcher fetches it lazily
      reaction: { key: stripFE0F(ctx.emojiKey), display: ctx.reaction.emoji.toString() },
      ensureMessage: ctx.ensureMessage,
    }));
  } catch (err) {
    logger.error('automations: messageReactionAdd failed', err);
  }
}

export async function onVoiceState(ctx) {
  try {
    for (const trigger of voiceTriggers(ctx.change)) {
      const channel = trigger === 'VOICE_LEFT' ? ctx.oldState?.channel : ctx.newState?.channel;
      if (!channel) continue;
      await dispatch(trigger, baseOf(ctx, { trigger, member: ctx.member, user: ctx.member.user, channel, voiceChannel: channel }));
    }
  } catch (err) {
    logger.error('automations: voiceStateUpdate failed', err);
  }
}

export async function onMemberAdd(ctx) {
  try {
    await dispatch('MEMBER_JOINED', baseOf(ctx, {
      trigger: 'MEMBER_JOINED', member: ctx.member, user: ctx.member.user, channel: null, captchaPending: ctx.state?.captchaPending === true,
    }));
  } catch (err) {
    logger.error('automations: guildMemberAdd failed', err);
  }
}

export async function onMemberUpdate(ctx) {
  try {
    if (!ctx.oldKnown) return;   // without the old member there is no reliable role diff
    const { guild, newMember, oldMember } = ctx;
    const { gained, lost } = roleDiff([...oldMember.roles.cache.keys()], [...newMember.roles.cache.keys()], guild.id);
    if (!gained.length && !lost.length) return;
    const depth = causes.peek(guild.id, newMember.id);
    const extra = { member: newMember, user: newMember.user, channel: null, depth };
    if (gained.length) await dispatch('ROLE_GAINED', baseOf(ctx, { ...extra, trigger: 'ROLE_GAINED', roleIds: gained }));
    if (lost.length) await dispatch('ROLE_LOST', baseOf(ctx, { ...extra, trigger: 'ROLE_LOST', roleIds: lost }));
  } catch (err) {
    logger.error('automations: guildMemberUpdate failed', err);
  }
}

export async function onThreadCreate(ctx) {
  try {
    const { thread, guild } = ctx;
    const member = guild.members.cache.get(thread.ownerId) ?? await guild.members.fetch(thread.ownerId).catch(() => null);
    if (!member || member.user.bot) return;
    await dispatch('THREAD_CREATED', baseOf(ctx, { trigger: 'THREAD_CREATED', member, user: member.user, channel: thread, thread }));
  } catch (err) {
    logger.error('automations: threadCreate failed', err);
  }
}

// ── Button handler (spec 6.9) ────────────────────────────────────────────────

/** Persistent `automation:{automationId}:{buttonIndex}` buttons. The click is acknowledged first and changes nothing visible. */
export async function onAutomationButton(interaction, client) {
  try {
    if (!interaction.inCachedGuild()) {
      await interaction.reply({ content: 'This button only works in a server.', ephemeral: true }).catch(() => {});
      return;
    }
    const { params } = parseCustomId(interaction.customId);
    const [automationId, rawIndex] = params;
    const index = Number(rawIndex);
    await interaction.deferUpdate().catch(() => {});
    if (!AUTOMATION_ID_RE.test(automationId ?? '') || !Number.isInteger(index) || index < 1 || index > MAX_BUTTON_INDEX) return;
    const { guild, guildId } = interaction;
    await dispatch('BUTTON_CLICKED', {
      trigger: 'BUTTON_CLICKED', guild, guildId, client, member: interaction.member, user: interaction.user, channel: interaction.channel,
      message: interaction.message, interaction, button: { automationId, index }, depth: 0,
      getSettings: () => getSettingsSafe(guildId),
    }, { awaitRuns: true });
  } catch (err) {
    logger.error('automations: button click failed', err);
  }
}
