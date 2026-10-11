// ── Custom commands and triggers (pkg-custom-commands.md) ──────────────────────
// One service for both halves of the module:
//   * custom commands (CustomTag): names, the action model, arguments, access lists, cooldowns,
//     the shared executor `runTag` (/tag use and the prefix hook) and the prefix hook body;
//   * triggers (AutoTrigger): the cache, safe regex compilation, selection and the trigger hook body.
// Every user-supplied regular expression goes through safeRegex (validated at write time, compiled
// once, executed in one batched time-limited vm call), and every outgoing text goes through
// renderTemplate, which clamps after expansion. Mentions of users are allowed; roles only through
// per-action pingRoleIds filtered by pingableRoleIds; @everyone and @here are never parsed.
// Nothing here has side effects at import.
import { PermissionFlagsBits } from 'discord.js';
import { Prisma } from '@prisma/client';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import { peekSettings } from './settingsService.js';
import { COLORS, LIMITS } from '../config/constants.js';
import { renderPlaceholdersDetailed } from '../utils/placeholderParser.js';
import { renderTemplate } from '../utils/customEmbed.js';
import { compileSafeRegex, isQuarantined, safeRegexFirstMatch, validateRegex } from '../utils/safeRegex.js';
import {
  checkAssignableRole, checkChannelUsable, isInteractionGone, pingableRoleIds, permissionNames, safeDefer, safeReply,
} from '../utils/interactionHelpers.js';
import { markBotDeletion, wasDeletedByBot } from '../utils/deletionTracker.js';
import { errorEmbed, successEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { clampText, joinWithLimit } from '../utils/text.js';
import { ackBudgetMs, createThrottle, withTimeout } from '../utils/async.js';
import { formatDurationLong } from '../utils/parseDuration.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const TAG_NAME_RE = /^[a-z0-9_-]{1,50}$/; // every stored name (legacy rows were up to 50)
export const NEW_TAG_NAME_MAX = 32;
export const PLAIN_MAX = 2000; // message content limit (write and render)
export const EMBED_TEXT_MAX = 4000; // write limit for embed text; the render clamps to 4096
export const DESCRIPTION_MAX = 100;
export const ARGS_MAX = 100; // arguments kept per invocation
export const COOLDOWN_MAX_SECONDS = 31_536_000; // 365 days
export const TRIGGER_PATTERN_MAX = 500;
export const TRIGGER_COOLDOWN_MAX = 86_400;
export const TAG_ACCESS_MAX = LIMITS.TAG_ACCESS_ENTRIES; // per list
export const TRIGGER_SCOPE_MAX = LIMITS.TRIGGER_SCOPE_ENTRIES; // channelIds / exemptRoleIds
export const PING_ROLES_MAX = LIMITS.TAG_PING_ROLES; // pingRoleIds per action
export const MEMO_TTL_MS = 60_000;
export const MEMO_MAX_GUILDS = 5_000;
export const NOTICE_TTL_MS = 10_000; // prefix usage and cooldown notices are deleted after this
export const MESSAGE_ACTION_TYPES = Object.freeze(['reply', 'send']);
export const ROLE_ACTION_TYPES = Object.freeze(['addRole', 'removeRole', 'toggleRole']);
export const MATCH_TYPES = Object.freeze(['CONTAINS', 'EXACT', 'STARTS_WITH', 'ENDS_WITH', 'REGEX']);
export const MATCH_LABELS = Object.freeze({
  CONTAINS: 'Contains', EXACT: 'Exact', STARTS_WITH: 'Starts with', ENDS_WITH: 'Ends with', REGEX: 'Regex',
});
const SNOWFLAKE_RE = /^\d{17,20}$/;
// {n:default}, {n...}, {...n}, {a...b}, {...}
const OPTIONAL_ARG_RE = /\{(?:\d{1,3}:[^{}\n]{0,60}|\d{0,3}\.\.\.\d{0,3})\}/;
const COMPILED_MAX = 5_000;
const TRIGGER_CACHE_TTL = 300; // seconds

// ── In-process state (per shard; __testing.reset() clears all of it) ──────────
// A guild's commands and messages always arrive on the shard that owns it, so these are consistent.
// Every write path invalidates its memo locally (invalidateTagIndex, invalidateTriggerCache).

/** guildId -> { names: Set<string>, at } (insertion order = age) */
const tagIndex = new Map();
/** guildId -> { entries: Array<{ t, re }>, at } */
const triggerMemo = new Map();
/** `${id}:${v}:${caseSensitive ? 1 : 0}` -> RegExp | null */
const compiled = new Map();
/** Test seam: guildId -> Map<name, row>; findTagRow reads these without Prisma. */
const primed = new Map();
let prefixThrottle = createThrottle(1_000); // `${guildId}:${userId}`
let noticeThrottle = createThrottle(10_000); // `${guildId}:${tagId}:${userId}`
let triggerChannelThrottle = createThrottle(1_000); // channelId
let warnThrottle = createThrottle(10 * 60_000); // `${guildId}:${what}:${id}`
let formatThrottle = createThrottle(60 * 60_000); // unknown actions formats: once per tag per hour
let countUses = true;

function memoPut(map, guildId, value) {
  map.delete(guildId);
  map.set(guildId, value);
  while (map.size > MEMO_MAX_GUILDS) map.delete(map.keys().next().value);
}

/** A throttled warning (one per key per 10 minutes). */
function warnOnce(key, message) {
  if (warnThrottle(key)) logger.warn(message);
}

// ── Errors ────────────────────────────────────────────────────────────────────

/** A failure with a title and text an admin can read; commands render it as errorEmbed(title, message). */
export class TagError extends Error {
  constructor(code, title, message) {
    super(message);
    this.name = 'TagError';
    this.code = code;
    this.title = title;
  }
}

// ── Raw SQL (00-overview rule 22: builders are exported so a DB test can run each one) ──

const lockSql = (key) => Prisma.sql`SELECT 1 AS ok FROM (SELECT pg_advisory_xact_lock(hashtext(${key}))) AS l`;
// uses is an int4 column: the maths run in bigint and the result is clamped to 2147483647.
const tagUsesSql = (id) => Prisma.sql`UPDATE "CustomTag" SET "uses" = LEAST("uses"::bigint + 1, 2147483647)::int WHERE "id" = ${id}`;
const triggerUsesSql = (id) => Prisma.sql`UPDATE "AutoTrigger" SET "uses" = LEAST("uses"::bigint + 1, 2147483647)::int WHERE "id" = ${id}`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'tag.lockGuildTags', build: () => lockSql('customtag:0') },
  { name: 'tag.lockGuildTriggers', build: () => lockSql('autotrigger:0') },
  { name: 'tag.incrementUses', build: () => tagUsesSql('0') },
  { name: 'trigger.incrementUses', build: () => triggerUsesSql('0') },
]);

// ── Display helpers ───────────────────────────────────────────────────────────

/** Backticks inside inline code would end it; ˋ (U+02CB) looks the same. */
export const codeSafe = (s) => String(s ?? '').replace(/`/g, 'ˋ');

/** Whitespace collapsed to single spaces. */
export const oneLine = (s) => String(s ?? '').replace(/\s+/g, ' ').trim();

/**
 * Mentions of ids for an embed (they render and never ping), `⚠️ deleted (id)` for an id missing from the cache.
 * @param {string[]} ids
 * @param {{ kind: 'role'|'channel', guild?: object|null, empty?: string, max?: number }} opts
 * @returns {string}
 */
export function formatIdList(ids, { kind, guild = null, empty = 'None', max = 1024 } = {}) {
  const list = Array.isArray(ids) ? ids : [];
  if (!list.length) return empty;
  const cacheOf = kind === 'role' ? guild?.roles?.cache : guild?.channels?.cache;
  return joinWithLimit(list.map((id) => {
    const mention = kind === 'role' ? `<@&${id}>` : `<#${id}>`;
    return guild && cacheOf && !cacheOf.has(id) ? `⚠️ deleted (${id})` : mention;
  }), { max });
}

const replyCount = (n) => `${n} repl${n === 1 ? 'y' : 'ies'}`;

/** One line describing an action, with mentions. A missing role or channel gets ` ⚠️ deleted` when `guild` is given. */
export function describeAction(a, guild = null) {
  if (!a) return '';
  const gone = (cacheOf, id) => (guild && cacheOf && !cacheOf.has(id) ? ' ⚠️ deleted' : '');
  if (MESSAGE_ACTION_TYPES.includes(a.type)) {
    const n = Array.isArray(a.responses) ? a.responses.length : 0;
    const ping = a.pingRoleIds?.length ? ` · pings ${a.pingRoleIds.map((id) => `<@&${id}>`).join(' ')}` : '';
    const suffix = `${a.embed ? ' · embed' : ''} · ${replyCount(n)}${ping}`;
    if (a.type === 'send') return `📤 Send to <#${a.channelId}>${gone(guild?.channels?.cache, a.channelId)}${suffix}`;
    return `${a.dm ? '📨 Reply by DM' : '💬 Reply in channel'}${suffix}`;
  }
  const role = `<@&${a.roleId}>${gone(guild?.roles?.cache, a.roleId)}`;
  if (a.type === 'addRole') return `➕ Give ${role}`;
  if (a.type === 'removeRole') return `➖ Remove ${role}`;
  if (a.type === 'toggleRole') return `🔁 Toggle ${role}`;
  return String(a.type ?? '');
}

/** The plain-text variant of describeAction (autocomplete choices cannot hold mentions). */
export function actionLabel(a, guild = null) {
  if (!a) return '';
  const channelName = (id) => {
    const c = guild?.channels?.cache?.get(id);
    return c ? `#${c.name}` : '(deleted)';
  };
  const roleName = (id) => {
    const r = guild?.roles?.cache?.get(id);
    return r ? `@${r.name}` : '(deleted)';
  };
  if (a.type === 'send') return `Send to ${channelName(a.channelId)}`;
  if (a.type === 'reply') return a.dm ? 'Reply by DM' : 'Reply in channel';
  if (a.type === 'addRole') return `Give ${roleName(a.roleId)}`;
  if (a.type === 'removeRole') return `Remove ${roleName(a.roleId)}`;
  if (a.type === 'toggleRole') return `Toggle ${roleName(a.roleId)}`;
  return String(a.type ?? '');
}

/** `None`, `Server-wide · 5 minutes` or `Per user · 5 minutes`. */
export function cooldownLabel(tag) {
  const secs = Number(tag?.cooldownSeconds) || 0;
  if (!tag || tag.cooldownScope === 'NONE' || secs <= 0) return 'None';
  return `${tag.cooldownScope === 'SERVER' ? 'Server-wide' : 'Per user'} · ${formatDurationLong(secs * 1000)}`;
}

export const FAILURE_TEXT = Object.freeze({
  EMPTY: 'The message was empty after variables were filled in.',
  DM_CLOSED: "I couldn't DM you; your DMs may be closed.",
  CHANNEL_GONE: 'The target channel no longer exists.',
  MISSING_PERMS: "I'm missing permissions there: {detail}.",
  ROLE_GONE: 'The role no longer exists.',
  ROLE_UNASSIGNABLE: '{detail}',
  MEMBER_GONE: "You're no longer in the server.",
  EXPIRED: 'The interaction expired before I could reply.',
  DISCORD_ERROR: 'Discord rejected the request.',
});

function failureText(code, detail) {
  const text = FAILURE_TEXT[code] ?? FAILURE_TEXT.DISCORD_ERROR;
  if (!text.includes('{detail}')) return text;
  return detail ? text.replace('{detail}', detail) : text.replace(': {detail}', '').replace('{detail}', '');
}

// ── Names (R102) ──────────────────────────────────────────────────────────────

/** The exact legacy create normalisation plus trim and the 50 cap; every stored name is a fixed point. */
export function normalizeTagName(input) {
  return String(input ?? '').trim().toLowerCase().replace(/\s+/g, '-').replace(/[^a-z0-9_-]/g, '').slice(0, 50);
}

/** Creation never strips characters silently: `c++` is refused instead of becoming `c`. */
export function validateNewTagName(input) {
  const base = String(input ?? '').trim().toLowerCase().replace(/\s+/g, '-');
  if (!/^[a-z0-9_-]{1,32}$/.test(base)) {
    return { ok: false, reason: 'Names can use only a–z, 0–9, - and _ (up to 32 characters), e.g. `server-rules`.' };
  }
  return { ok: true, name: base };
}

// ── Lookups and the name index ────────────────────────────────────────────────

/** Throws on a database failure so commands reach the router's error reply. */
export async function findTag(guildId, rawName) {
  const name = normalizeTagName(rawName);
  if (!name) return null;
  return prisma.customTag.findUnique({ where: { guildId_name: { guildId, name } } });
}

/** Drops the memoised name set of a guild; called after every tag write. */
export function invalidateTagIndex(guildId) {
  tagIndex.delete(guildId);
}

/** True when an enabled command has this name. Never throws: a database error gives false (prefix commands go silent). */
export async function hasTagName(guildId, name) {
  try {
    const hit = tagIndex.get(guildId);
    if (hit && Date.now() - hit.at < MEMO_TTL_MS) return hit.names.has(name);
    const rows = await prisma.customTag.findMany({ where: { guildId, enabled: true }, select: { name: true } });
    memoPut(tagIndex, guildId, { names: new Set(rows.map((r) => r.name)), at: Date.now() });
    return tagIndex.get(guildId).names.has(name);
  } catch (err) {
    warnOnce(`${guildId}:tagindex:0`, `Custom commands: could not load the name index of ${guildId}: ${err?.message ?? err}`);
    return false;
  }
}

/** Hook-path lookup: primed test rows first, then Prisma. Never throws (null). */
export async function findTagRow(guildId, name) {
  try {
    const rows = primed.get(guildId);
    if (rows) return rows.get(name) ?? null;
    return await prisma.customTag.findUnique({ where: { guildId_name: { guildId, name } } });
  } catch (err) {
    warnOnce(`${guildId}:tagrow:0`, `Custom commands: lookup of "${name}" failed in ${guildId}: ${err?.message ?? err}`);
    return null;
  }
}

// ── Arguments, usage, prefix parsing ──────────────────────────────────────────

/** MEE6 arguments are space-separated and quotes mean nothing; use {n...} for the rest of the text. */
export function parseArgs(str) {
  const s = String(str ?? '').trim();
  if (s === '') return [];
  return s.split(/\s+/u).slice(0, ARGS_MAX);
}

/**
 * The prefix commands answer to. `''` (set by `/settings prefix off`) disables prefix invocation and
 * returns null; any other invalid value (a hand-edited row) falls back to '!'.
 * @param {unknown} p
 * @returns {string|null}
 */
export function effectivePrefix(p) {
  if (p === '') return null;
  return typeof p === 'string' && p.length >= 1 && [...p].length <= 5 && !/\s/u.test(p) ? p : '!';
}

/**
 * @param {unknown} content
 * @param {string|null} prefix
 * @returns {{ name: string, args: string[] }|null}
 */
export function parseInvocation(content, prefix) {
  if (typeof content !== 'string' || typeof prefix !== 'string' || prefix === '' || !content.startsWith(prefix)) return null;
  const rest = content.slice(prefix.length);
  const m = /^(\S{1,50})(?:\s+|$)/u.exec(rest);
  if (!m) return null;
  const name = m[1].toLowerCase();
  if (!TAG_NAME_RE.test(name)) return null;
  return { name, args: parseArgs(rest.slice(m[0].length)) };
}

/**
 * `!name <1> [more…]` or `/tag use name:x args:<1>`. With a null prefix (prefix commands off) the slash form is returned.
 * Arguments are positional, so a command that only uses {2} still needs a first word.
 */
export function buildUsage(tag, { prefix = '!', slash = false } = {}) {
  try {
    const texts = [];
    for (const a of getEffectiveActions(tag)) {
      if (MESSAGE_ACTION_TYPES.includes(a.type)) texts.push(...a.responses);
    }
    let maxReq = 0;
    for (const t of texts) {
      for (const n of renderPlaceholdersDetailed(t, { args: [] }).missingArgs) maxReq = Math.max(maxReq, n);
    }
    const optional = texts.some((t) => OPTIONAL_ARG_RE.test(t));
    const argText = [
      ...Array.from({ length: maxReq }, (_, i) => `<${i + 1}>`),
      ...(optional ? ['[more…]'] : []),
    ].join(' ');
    if (slash || typeof prefix !== 'string' || prefix === '') {
      return `/tag use name:${tag.name}${argText ? ` args:${argText}` : ''}`;
    }
    return `${prefix}${tag.name}${argText ? ` ${argText}` : ''}`;
  } catch {
    return typeof prefix === 'string' && prefix && !slash ? `${prefix}${tag?.name ?? ''}` : `/tag use name:${tag?.name ?? ''}`;
  }
}

// ── The action model (§3.3) ───────────────────────────────────────────────────

/**
 * Pure, tolerant, never throws. null = legacy (null input) or an unknown format (a non-array).
 * Invalid entries are dropped silently; the result is capped at LIMITS.TAG_ACTIONS.
 * @param {unknown} json
 * @returns {Array<object>|null}
 */
export function normalizeActions(json) {
  try {
    if (json === null || json === undefined) return null;
    if (!Array.isArray(json)) return null;
    const out = [];
    for (const a of json.slice(0, LIMITS.TAG_ACTIONS)) {
      if (!a || typeof a !== 'object') continue;
      if (MESSAGE_ACTION_TYPES.includes(a.type)) {
        const responses = (Array.isArray(a.responses) ? a.responses : [])
          .filter((s) => typeof s === 'string' && s.trim() !== '')
          .slice(0, LIMITS.TAG_RESPONSES);
        if (!responses.length) continue;
        if (a.type === 'send' && !(typeof a.channelId === 'string' && SNOWFLAKE_RE.test(a.channelId))) continue;
        const action = { type: a.type, responses, embed: a.embed === true };
        if (a.type === 'reply') action.dm = a.dm === true;
        else action.channelId = a.channelId;
        const pings = [...new Set((Array.isArray(a.pingRoleIds) ? a.pingRoleIds : [])
          .filter((id) => typeof id === 'string' && SNOWFLAKE_RE.test(id)))].slice(0, PING_ROLES_MAX);
        if (pings.length && !(a.type === 'reply' && action.dm)) action.pingRoleIds = pings;
        out.push(action);
      } else if (ROLE_ACTION_TYPES.includes(a.type)) {
        if (typeof a.roleId === 'string' && SNOWFLAKE_RE.test(a.roleId)) out.push({ type: a.type, roleId: a.roleId });
      }
    }
    return out;
  } catch {
    return null;
  }
}

/** The effective action list of a legacy tag (actions null). */
export function legacyActions(tag) {
  return [{ type: 'reply', dm: false, embed: tag?.useEmbed === true, responses: [tag?.content ?? ''] }];
}

/** normalizeActions(tag.actions) ?? legacyActions(tag); an unknown stored format is logged once per tag per hour. */
export function getEffectiveActions(tag) {
  const normalized = normalizeActions(tag?.actions);
  if (normalized !== null) return normalized;
  if (tag?.actions !== null && tag?.actions !== undefined && formatThrottle(`${tag.guildId}:${tag.id}`)) {
    logger.warn(`Custom command ${tag.id} has an unknown actions format; using its content`);
  }
  return legacyActions(tag);
}

/** The first index whose type is reply or send, else -1. */
export const primaryMessageIndex = (actions) => (Array.isArray(actions)
  ? actions.findIndex((a) => MESSAGE_ACTION_TYPES.includes(a?.type)) : -1);

/** The first index with type reply and no dm, else -1. */
export const channelReplyIndex = (actions) => (Array.isArray(actions)
  ? actions.findIndex((a) => a?.type === 'reply' && !a.dm) : -1);

/** The mirror columns written together with `actions` (content and useEmbed follow the primary message action). */
export function syncLegacyColumns(actions) {
  const i = primaryMessageIndex(actions);
  return i < 0 ? {} : { content: actions[i].responses[0], useEmbed: actions[i].embed === true };
}

// ── Access and scope (R282) ───────────────────────────────────────────────────

/** The ids a message channel counts as: itself, a thread's parent and that parent's category, or its own category. */
export function channelScopeIds(channel) {
  const ids = [];
  const add = (id) => {
    if (id !== null && id !== undefined && id !== '' && !ids.includes(String(id))) ids.push(String(id));
  };
  add(channel?.id);
  if (channel?.isThread?.()) {
    add(channel.parentId);
    add(channel.parent?.parentId);
  } else {
    add(channel?.parentId);
  }
  return ids;
}

/**
 * Allow and deny lists of a custom command. There is no admin bypass (MEE6 applies the lists to everyone).
 * @returns {{ ok: true }|{ ok: false, code: 'CHANNEL_DENIED'|'CHANNEL_NOT_ALLOWED'|'ROLE_DENIED'|'ROLE_NOT_ALLOWED' }}
 */
export function checkTagAccess(tag, { member, channel }) {
  const ids = channelScopeIds(channel);
  const denyChannels = tag?.denyChannelIds ?? [];
  const allowChannels = tag?.allowChannelIds ?? [];
  if (denyChannels.some((id) => ids.includes(id))) return { ok: false, code: 'CHANNEL_DENIED' };
  if (allowChannels.length && !allowChannels.some((id) => ids.includes(id))) return { ok: false, code: 'CHANNEL_NOT_ALLOWED' };
  const roles = member?.roles?.cache;
  const denyRoles = tag?.denyRoleIds ?? [];
  const allowRoles = tag?.allowRoleIds ?? [];
  if (denyRoles.some((id) => roles?.has(id))) return { ok: false, code: 'ROLE_DENIED' };
  if (allowRoles.length && !allowRoles.some((id) => roles?.has(id))) return { ok: false, code: 'ROLE_NOT_ALLOWED' };
  return { ok: true };
}

/** Channel scope and exempt roles of a trigger; stale ids simply never match. */
export function triggerAppliesHere(t, { channel, member }) {
  const channels = t?.channelIds ?? [];
  if (channels.length) {
    const ids = channelScopeIds(channel);
    if (!channels.some((id) => ids.includes(id))) return false;
  }
  const exempt = t?.exemptRoleIds ?? [];
  if (exempt.length && exempt.some((id) => member?.roles?.cache?.has(id))) return false;
  return true;
}

// ── Rendering message actions (§6.5) ──────────────────────────────────────────

export const pickResponse = (responses, rng = Math.random) => {
  const list = Array.isArray(responses) ? responses : [];
  if (!list.length) return '';
  const i = Math.floor(rng() * list.length);
  return list[Math.min(list.length - 1, Math.max(0, Number.isFinite(i) ? i : 0))];
};

export const renderCtxFor = ({ member, user, guild, channel, args } = {}) => ({ member, user, guild, channel, args });

/**
 * Renders one response of a message action into a payload, or null when nothing visible remains.
 * Content is clamped to 2000 and an embed description to 4096 AFTER expansion; user mentions are
 * allowed, roles only through `pingRoleIds` (already filtered by pingableRoleIds when `opts` has it),
 * and @everyone / @here are never parsed.
 * @param {object} action
 * @param {string} response
 * @param {object} renderCtx
 * @param {object|null} settings
 * @param {{ pingRoleIds?: string[] }} [opts] the filtered role ids; defaults to action.pingRoleIds
 */
export function buildMessagePayload(action, response, renderCtx, settings, { pingRoleIds } = {}) {
  const tpl = action?.embed ? { embed: { description: response, timestamp: true } } : { content: response };
  const color = settings?.embedColor;
  const defaultColor = Number.isInteger(color) && color >= 0 && color <= 0xffffff ? color : COLORS.INFO;
  const roles = pingRoleIds ?? action?.pingRoleIds;
  const allowedMentions = roles?.length ? { parse: ['users'], roles: [...roles] } : { parse: ['users'] };
  return renderTemplate(tpl, renderCtx, { defaultColor, allowedMentions });
}

/** Pure: picks one response per message action and finds the argument numbers that were asked for but not given. */
export function planRun(tag, renderCtx, rng = Math.random) {
  const steps = getEffectiveActions(tag).map((a) => (MESSAGE_ACTION_TYPES.includes(a.type)
    ? { action: a, response: pickResponse(a.responses, rng) }
    : { action: a }));
  const missing = new Set();
  for (const step of steps) {
    if (step.response === undefined) continue;
    for (const n of renderPlaceholdersDetailed(step.response, renderCtx).missingArgs) missing.add(n);
  }
  return { steps, missing: [...missing].sort((a, b) => a - b) };
}

// ── Cooldowns (R170) ──────────────────────────────────────────────────────────

/** null for NONE (or 0 seconds); a per-guild key for SERVER; a per-user key for USER. */
export function tagCooldownKey(tag, guildId, userId) {
  if (!tag || tag.cooldownScope === 'NONE' || !(tag.cooldownSeconds > 0)) return null;
  if (tag.cooldownScope === 'SERVER') return `tag:cd:${guildId}:${tag.id}`;
  if (tag.cooldownScope === 'USER') return `tag:cd:${guildId}:${tag.id}:${userId}`;
  return null;
}

/**
 * Atomic SET NX EX. Fails CLOSED when Redis is unavailable (a command with a cooldown does not run
 * while the cooldown cannot be enforced).
 * @returns {Promise<{ ok: true }|{ ok: false, unavailable: true }|{ ok: false, until: number }>}
 */
export async function acquireCooldown(key, seconds) {
  const until = Date.now() + seconds * 1000;
  const r = await cache.setNX(key, until, seconds);
  if (r === true) return { ok: true };
  if (r === null) return { ok: false, unavailable: true };
  const v = Number(await cache.get(key));
  return { ok: false, until: Number.isFinite(v) && v > Date.now() ? v : until };
}

/** A run that did nothing does not burn the cooldown. */
export async function releaseCooldown(key) {
  await cache.del(key);
}

/** Drops the server key and every per-user key of a command (after delete or a cooldown change). */
export async function clearTagCooldowns(guildId, tagId) {
  await cache.del(`tag:cd:${guildId}:${tagId}`);
  await cache.invalidate(`tag:cd:${guildId}:${tagId}:*`);
}

// ── Usage counters ────────────────────────────────────────────────────────────

/** Fire-and-forget, clamped to int4 in SQL; raw SQL leaves updatedAt for real edits. */
export function incrementTagUses(id) {
  if (!countUses) return Promise.resolve();
  return prisma.$executeRaw(tagUsesSql(id)).catch((err) => logger.debug(`Custom command uses increment failed: ${err?.message}`));
}

export function incrementTriggerUses(id) {
  if (!countUses) return Promise.resolve();
  return prisma.$executeRaw(triggerUsesSql(id)).catch((err) => logger.debug(`Trigger uses increment failed: ${err?.message}`));
}

// ── The executor (§6.6) ───────────────────────────────────────────────────────

const needsFor = (a) => (a?.embed ? ['ViewChannel', 'SendMessages', 'EmbedLinks'] : ['ViewChannel', 'SendMessages']);

/** Maps a Discord error onto one of the FAILURE_TEXT codes for the kind of step that failed. */
function failureFromError(err, kind) {
  if (isInteractionGone(err)) return { ok: false, code: 'EXPIRED' };
  const code = err?.code;
  if (kind === 'dm' && code === 50007) return { ok: false, code: 'DM_CLOSED' };
  if ((kind === 'send' || kind === 'reply') && code === 10003) return { ok: false, code: 'CHANNEL_GONE' };
  if (kind === 'role' && code === 10011) return { ok: false, code: 'ROLE_GONE' };
  if (kind === 'role' && code === 10007) return { ok: false, code: 'MEMBER_GONE' };
  if (code === 50001 || code === 50013) return { ok: false, code: 'MISSING_PERMS' };
  logger.warn(`Custom command step failed (${kind}): ${code ?? ''} ${err?.message ?? err}`);
  return { ok: false, code: 'DISCORD_ERROR' };
}

/**
 * Runs a custom command: access, arguments, cooldown, every action in order, the outcome report and
 * (prefix only) deleting the usage. Nothing with a side effect happens before the cooldown is taken.
 * @param {object} tag CustomTag row
 * @param {{ source: 'slash'|'prefix', interaction?: object, message?: object, guild: object, channel: object,
 *   member: object|null, user: object, args: string[], settings: object|null, prefix: string|null,
 *   rng?: () => number, checkPermissions?: boolean }} opts
 * @returns {Promise<{ status: string, results?: object[], success?: boolean, deleted?: boolean }>}
 */
export async function runTag(tag, opts) {
  const { source, interaction, message, guild, channel, member, user, settings } = opts;
  const args = opts.args ?? [];
  const prefix = opts.prefix === undefined ? '!' : opts.prefix;
  const rng = opts.rng ?? Math.random;
  const checkPermissions = opts.checkPermissions !== false;
  const slash = source === 'slash';
  const guildId = guild.id;

  // Ephemeral answer to a slash invocation that did not get to run; silent for prefix use.
  const tell = async (embed) => {
    if (slash) await safeReply(interaction, { embeds: [embed], ephemeral: true });
  };

  // 1. Access.
  const access = checkTagAccess(tag, { member, channel });
  if (!access.ok) {
    await tell(errorEmbed('Not Allowed', access.code.startsWith('CHANNEL')
      ? "This command can't be used in this channel."
      : "You don't have a role that can use this command."));
    return { status: 'denied' };
  }

  // 2. Actions exist.
  const renderCtx = renderCtxFor({ member, user, guild, channel, args });
  const plan = planRun(tag, renderCtx, rng);
  const steps = plan.steps;
  if (steps.length === 0) {
    await tell(warnEmbed('Nothing to Do', 'This command has no actions.'));
    return { status: 'empty' };
  }

  // 3. Arguments (the MEE6 usage reply).
  if (plan.missing.length) {
    const usage = buildUsage(tag, { prefix, slash });
    const text = `Usage: \`${usage}\`${tag.description ? `\n${tag.description}` : ''}`;
    const embed = warnEmbed('Missing Arguments', text);
    if (slash) await tell(embed);
    else await sendNotice(message, embed, tag, { checkPermissions });
    return { status: 'usage' };
  }

  // 4. Cooldown (atomic; fails closed when Redis is down).
  const key = tagCooldownKey(tag, guildId, user.id);
  if (key) {
    let cd;
    if (slash) {
      // The cooldown store must not eat the 3-second acknowledgement window.
      const budget = Number.isFinite(interaction?.createdTimestamp) ? ackBudgetMs(interaction) : 2000;
      cd = await withTimeout(acquireCooldown(key, tag.cooldownSeconds), budget, 'tag cooldown')
        .catch(() => ({ ok: false, unavailable: true }));
    } else {
      cd = await acquireCooldown(key, tag.cooldownSeconds);
    }
    if (cd.unavailable) {
      if (slash) await tell(warnEmbed('Temporarily Unavailable', "This command has a cooldown that I can't check right now. Try again in a moment."));
      else warnOnce(`${guildId}:cdstore:0`, `Custom command cooldown store unavailable in ${guildId}`);
      return { status: 'unavailable' };
    }
    if (!cd.ok) {
      const when = `<t:${Math.ceil(cd.until / 1000)}:R>`;
      const text = tag.cooldownScope === 'SERVER' ? `This command can be used again ${when}.` : `You can use this command again ${when}.`;
      const embed = warnEmbed('Cooldown', text);
      if (slash) await tell(embed);
      else await sendNotice(message, embed, tag, { checkPermissions });
      return { status: 'cooldown' };
    }
  }

  // 5. Execute.
  const results = new Array(steps.length).fill(null);

  /** Payload of a message step; role pings follow pingableRoleIds with the bot as the sender. */
  const payloadFor = (step, target) => {
    const a = step.action;
    let pingRoleIds;
    if (a.pingRoleIds?.length && !(a.type === 'reply' && a.dm)) {
      pingRoleIds = target ? pingableRoleIds(target, guild.members?.me ?? null, a.pingRoleIds).allowed : [];
    }
    return buildMessagePayload(a, step.response, renderCtx, settings, { pingRoleIds });
  };

  // How a public "reply in channel" step is posted when it is not the interaction's primary reply.
  const postChannelReply = async (step, how) => {
    const payload = payloadFor(step, channel);
    if (!payload) return { ok: false, code: 'EMPTY' };
    if (how === 'followUp') {
      await interaction.followUp(payload);
    } else {
      if (checkPermissions) {
        const usable = checkChannelUsable(channel, { needs: needsFor(step.action) });
        if (!usable.ok) return { ok: false, code: 'MISSING_PERMS', detail: permissionNames(usable.missing).join(', ') };
      }
      await channel.send(payload);
    }
    return { ok: true, summary: '💬 Replied in the channel.' };
  };

  const execStep = async (step, how) => {
    const a = step.action;
    let kind = 'reply';
    try {
      if (a.type === 'reply' && a.dm) {
        kind = 'dm';
        const payload = payloadFor(step, null);
        if (!payload) return { ok: false, code: 'EMPTY' };
        await user.send(payload);
        return { ok: true, summary: '📨 Sent you a DM.' };
      }
      if (a.type === 'reply') return await postChannelReply(step, how);
      if (a.type === 'send') {
        kind = 'send';
        const target = guild.channels?.cache?.get(a.channelId);
        if (!target) return { ok: false, code: 'CHANNEL_GONE' };
        const payload = payloadFor(step, target);
        if (!payload) return { ok: false, code: 'EMPTY' };
        if (checkPermissions) {
          const usable = checkChannelUsable(target, { needs: needsFor(a) });
          if (!usable.ok) return { ok: false, code: 'MISSING_PERMS', detail: permissionNames(usable.missing).join(', ') };
        }
        await target.send(payload);
        return { ok: true, summary: `📤 Posted in <#${target.id}>.` };
      }
      // Role actions, one role at a time (never the array form).
      kind = 'role';
      const role = guild.roles?.cache?.get(a.roleId);
      if (!role) return { ok: false, code: 'ROLE_GONE' };
      const assignable = checkAssignableRole(guild, role);
      if (!assignable.ok) return { ok: false, code: 'ROLE_UNASSIGNABLE', detail: assignable.reason };
      const m = member ?? await guild.members.fetch(user.id).catch(() => null);
      if (!m) return { ok: false, code: 'MEMBER_GONE' };
      const has = m.roles.cache.has(role.id);
      const reason = `Custom command ${tag.name}`;
      const give = a.type === 'addRole' || (a.type === 'toggleRole' && !has);
      if (give) {
        if (has) return { ok: true, summary: `ℹ️ You already have <@&${role.id}>.` };
        await m.roles.add(role.id, reason);
        return { ok: true, summary: `➕ Gave you <@&${role.id}>.` };
      }
      if (!has) return { ok: true, summary: `ℹ️ You don't have <@&${role.id}>.` };
      await m.roles.remove(role.id, reason);
      return { ok: true, summary: `➖ Removed <@&${role.id}>.` };
    } catch (err) {
      return failureFromError(err, kind);
    }
  };

  // 'replied' | 'deferred' | 'gone': how the slash interaction was acknowledged.
  let acked = 'gone';
  if (slash) {
    const p = channelReplyIndex(steps.map((s) => s.action));
    let primaryDone = false;
    if (p >= 0) {
      const payload = payloadFor(steps[p], channel);
      if (!payload) {
        results[p] = { ok: false, code: 'EMPTY' };
      } else {
        try {
          await interaction.reply(payload);
          results[p] = { ok: true, summary: '💬 Replied in the channel.' };
          acked = 'replied';
          primaryDone = true;
        } catch (err) {
          results[p] = failureFromError(err, 'reply');
          if (results[p].code === 'EXPIRED') primaryDone = true; // nothing can be acknowledged any more
        }
      }
      if (!primaryDone) {
        // Never leave the interaction unacknowledged: the summary goes out through editReply.
        acked = (await safeDefer(interaction, { ephemeral: true })) ? 'deferred' : 'gone';
      }
    } else {
      acked = (await safeDefer(interaction, { ephemeral: true })) ? 'deferred' : 'gone';
    }
    for (let i = 0; i < steps.length; i++) {
      if (results[i]) continue;
      // After the primary reply further channel replies are new public messages; after a defer they are
      // posted with channel.send, because a follow-up would replace the "thinking" message and the final
      // editReply would then overwrite it.
      results[i] = await execStep(steps[i], acked === 'replied' ? 'followUp' : 'channel');
    }
  } else {
    for (let i = 0; i < steps.length; i++) results[i] = await execStep(steps[i], 'channel');
  }

  // 6. Outcome.
  const success = results.some((r) => r?.ok);
  if (!success && key) await releaseCooldown(key); // a failed run does not burn the cooldown
  if (success) incrementTagUses(tag.id);

  // 7. Report.
  const failureLines = [];
  const doneLines = [];
  results.forEach((r, i) => {
    if (r?.ok) {
      if (r.summary) doneLines.push(r.summary);
      return;
    }
    failureLines.push(`⚠️ Action ${i + 1} (${actionLabel(steps[i].action, guild)}): ${failureText(r?.code, r?.detail)}`);
  });
  if (slash) {
    try {
      if (acked === 'replied') {
        if (failureLines.length) {
          await interaction.followUp({
            ephemeral: true,
            embeds: [warnEmbed('Some Actions Failed', joinWithLimit(failureLines, { sep: '\n', max: 4000 }))],
          });
        }
      } else if (acked === 'deferred') {
        const lines = joinWithLimit([...doneLines, ...failureLines], { sep: '\n', max: 4000 });
        await interaction.editReply({
          embeds: [failureLines.length ? warnEmbed('Some Actions Failed', lines) : successEmbed('Done', lines)],
        });
      }
    } catch (err) {
      if (!isInteractionGone(err)) logger.warn(`Custom command ${tag.name}: could not report the outcome: ${err?.message ?? err}`);
    }
  } else {
    results.forEach((r, i) => {
      if (r && !r.ok) warnOnce(`${guildId}:cc:${tag.id}:${r.code}`, `Custom command ${tag.name} (${guildId}): action ${i + 1} failed: ${r.code}`);
    });
  }

  // 8. Delete the usage (prefix only, after at least one action worked).
  let deleted = false;
  if (!slash && tag.deleteInvocation && success) {
    const canDelete = !checkPermissions
      || message.channel?.permissionsFor?.(guild.members?.me)?.has(PermissionFlagsBits.ManageMessages);
    if (!canDelete) {
      warnOnce(`${guildId}:ccdel:${tag.id}`, `Custom command ${tag.name} (${guildId}): I need Manage Messages in #${message.channel?.name ?? message.channelId} to delete usages`);
    } else {
      markBotDeletion(message.id, 'custom-command'); // immediately before the delete
      try {
        await message.delete();
        deleted = true;
      } catch (err) {
        if (err?.code === 10008) deleted = true; // already gone
        else warnOnce(`${guildId}:ccdel:${tag.id}`, `Custom command ${tag.name} (${guildId}): could not delete the usage (${err?.code ?? err?.message})`);
      }
    }
  }
  return { status: 'ran', results, success, deleted };
}

// ── The prefix hook (§6.7) ────────────────────────────────────────────────────

/** Sync filter of the prefix hook: peekSettings prefix check, no I/O. */
export function isPossibleInvocation(ctx) {
  const c = ctx?.message?.content;
  if (typeof c !== 'string' || c.length < 2) return false;
  const s = peekSettings(ctx.guildId);
  if (!s) return true;
  const prefix = effectivePrefix(s.customPrefix);
  return prefix !== null && c.startsWith(prefix);
}

/**
 * Prefix usage and cooldown notices: a short reply that deletes itself after NOTICE_TTL_MS.
 * Rate-limited to one per user per command per 10 seconds; skipped when the bot cannot reply there.
 */
export async function sendNotice(message, embed, tag, { checkPermissions = true } = {}) {
  try {
    const guildId = message.guildId ?? message.guild?.id;
    if (!noticeThrottle(`${guildId}:${tag.id}:${message.author?.id}`)) return;
    if (checkPermissions && !checkChannelUsable(message.channel, {
      needs: ['ViewChannel', 'SendMessages', 'EmbedLinks', 'ReadMessageHistory'],
    }).ok) return;
    const sent = await message.reply({ embeds: [embed], allowedMentions: { parse: [], repliedUser: false } });
    const timer = setTimeout(() => {
      try {
        markBotDeletion(sent.id, 'custom-command');
        Promise.resolve(sent.delete()).catch(() => {});
      } catch {
        // never throws
      }
    }, NOTICE_TTL_MS);
    timer.unref?.();
  } catch (err) {
    logger.debug(`Custom command notice failed: ${err?.message ?? err}`);
  }
}

/**
 * The body of the `custom-commands` hook. Never throws. Calls ctx.stop() only after the invocation
 * message was deleted, so later hooks (levels, triggers, translation) skip a message that is gone.
 * @param {object} ctx HookContext of messageCreate
 * @param {{ checkPermissions?: boolean }} [opts] false only in offline tests
 */
export async function handlePrefixInvocation(ctx, { checkPermissions = true } = {}) {
  try {
    const { message, guild, guildId } = ctx;
    if (!message || !guild || !guildId) return;
    if (wasDeletedByBot(message.id)) return;
    const settings = await ctx.getSettings();
    if (!settings) return;
    const prefix = effectivePrefix(settings.customPrefix);
    if (prefix === null) return; // prefix commands are off
    const inv = parseInvocation(message.content, prefix);
    if (!inv) return;
    // Unknown names are silent: other bots share prefixes.
    if (!(await hasTagName(guildId, inv.name))) return;
    const tag = await findTagRow(guildId, inv.name);
    if (!tag || !tag.enabled) return;
    ctx.state.customCommand = { tagId: tag.id, name: tag.name };
    if (!prefixThrottle(`${guildId}:${message.author.id}`)) return; // silent
    const res = await runTag(tag, {
      source: 'prefix', message, guild, channel: message.channel, member: message.member, user: message.author,
      args: inv.args, settings, prefix, checkPermissions,
    });
    if (res.deleted) ctx.stop('custom-commands: usage deleted');
  } catch (err) {
    logger.error('Custom command hook failed', err instanceof Error ? err : new Error(String(err)));
  }
}

// ── Triggers: validation, cache, compilation, selection (§6.8) ────────────────

/**
 * Write-time validation of a trigger pattern. REGEX patterns go through safeRegex.validateRegex
 * (nested quantifiers, backreferences, more than 200 characters, syntax).
 * @returns {{ ok: true }|{ ok: false, reason: string }}
 */
export function validateTriggerPattern(pattern, matchType, caseSensitive) {
  const p = String(pattern ?? '');
  if (p.trim() === '') return { ok: false, reason: 'The pattern is empty.' };
  if (p.length > TRIGGER_PATTERN_MAX) return { ok: false, reason: `The pattern can be at most ${TRIGGER_PATTERN_MAX} characters.` };
  if (matchType === 'REGEX') return validateRegex(p, caseSensitive ? '' : 'i');
  if (!MATCH_TYPES.includes(matchType)) return { ok: false, reason: 'Unknown match type.' };
  return { ok: true };
}

const toProjection = (row) => ({
  id: row.id,
  trigger: row.trigger,
  response: row.response,
  matchType: row.matchType,
  caseSensitive: row.caseSensitive,
  cooldown: row.cooldown,
  deleteMessage: row.deleteMessage,
  channelIds: row.channelIds ?? [],
  exemptRoleIds: row.exemptRoleIds ?? [],
  v: row.updatedAt instanceof Date ? row.updatedAt.toISOString() : String(row.updatedAt),
});

/**
 * The compiled regex of a REGEX projection, memoised per id + updatedAt. null for unsafe or invalid
 * patterns (legacy rows created before validation), which therefore never run.
 */
export function compiledFor(t, guildId = '?') {
  const key = `${t.id}:${t.v}:${t.caseSensitive ? 1 : 0}`;
  if (compiled.has(key)) return compiled.get(key);
  const re = compileSafeRegex(t.trigger, t.caseSensitive ? '' : 'i');
  if (!re) warnOnce(`${guildId}:trigger:${t.id}`, `Trigger ${t.id} in ${guildId} has an unsafe or invalid regex and is skipped`);
  compiled.set(key, re);
  while (compiled.size > COMPILED_MAX) compiled.delete(compiled.keys().next().value);
  return re;
}

const triggerKey = (guildId) => `guild:${guildId}:triggers`;

/** Drops the memo and the Redis list of a guild; called after every trigger write. */
export async function invalidateTriggerCache(guildId) {
  triggerMemo.delete(guildId);
  await cache.del(triggerKey(guildId));
}

function makeEntries(projections, guildId) {
  return projections.map((t) => ({ t, re: t.matchType === 'REGEX' ? compiledFor(t, guildId) : null }));
}

/**
 * Enabled triggers of a guild in creation order, with their compiled regexes. Memo, then Redis
 * (fail open: a miss falls through to Postgres), then Postgres. Never throws; null = triggers stay silent.
 * @returns {Promise<{ entries: Array<{ t: object, re: RegExp|null }> }|null>}
 */
export async function getTriggerSet(guildId) {
  try {
    const hit = triggerMemo.get(guildId);
    if (hit && Date.now() - hit.at < MEMO_TTL_MS) return hit;
    let projections = null;
    const cached = await cache.get(triggerKey(guildId));
    // Legacy-shaped values (full rows) carry no `v` and count as a miss.
    if (Array.isArray(cached) && cached.every((t) => t && typeof t.v === 'string')) projections = cached;
    if (!projections) {
      const rows = await prisma.autoTrigger.findMany({
        where: { guildId, enabled: true },
        orderBy: [{ createdAt: 'asc' }, { id: 'asc' }],
      });
      projections = rows.map(toProjection);
      await cache.set(triggerKey(guildId), projections, TRIGGER_CACHE_TTL);
    }
    const entry = { entries: makeEntries(projections, guildId), at: Date.now() };
    memoPut(triggerMemo, guildId, entry);
    return entry;
  } catch (err) {
    warnOnce(`${guildId}:triggerset:0`, `Triggers: could not load the triggers of ${guildId}: ${err?.message ?? err}`);
    return null;
  }
}

/** Non-regex matching. EXACT compares trimmed text (trailing spaces are invisible in Discord). */
export function matchesPlain(t, content) {
  let s = String(content ?? '');
  let p = String(t?.trigger ?? '');
  if (!t?.caseSensitive) {
    s = s.toLowerCase();
    p = p.toLowerCase();
  }
  switch (t?.matchType) {
    case 'EXACT': return s.trim() === p.trim();
    case 'CONTAINS': return s.includes(p);
    case 'STARTS_WITH': return s.startsWith(p);
    case 'ENDS_WITH': return s.endsWith(p);
    default: return false;
  }
}

/**
 * Picks the first applicable matching trigger in creation order. Cheap scope checks come first, plain
 * matches are string operations, and every candidate regex runs in ONE batched, time-limited vm call
 * (safeRegexFirstMatch: 50 ms budget, input capped at 2000, quarantine on timeout), so the shard never
 * blocks for more than about 50 ms per message (R010, R048).
 * @param {Array<{ t: object, re: RegExp|null }>} entries
 * @param {string} content
 * @param {{ channel: object, member: object|null }} scope
 * @returns {object|null} the projection
 */
export function selectTrigger(entries, content, scope) {
  let firstPlain = -1;
  const regexCands = [];
  for (let i = 0; i < entries.length; i++) {
    const { t, re } = entries[i];
    if (!triggerAppliesHere(t, scope)) continue;
    if (t.matchType === 'REGEX') {
      if (re) regexCands.push(i);
      continue;
    }
    if (matchesPlain(t, content)) { firstPlain = i; break; }
  }
  if (regexCands.length) {
    // Only regexes created before the first plain match can win.
    const k = safeRegexFirstMatch(regexCands.map((i) => entries[i].re), content);
    if (k >= 0) return entries[regexCands[k]].t;
  }
  return firstPlain >= 0 ? entries[firstPlain].t : null;
}

/** True when the regex of a trigger timed out recently and is being skipped (for /trigger info). */
export function triggerQuarantined(row) {
  try {
    const re = compiledFor(toProjection({ ...row, updatedAt: row.updatedAt ?? new Date(0) }), row.guildId);
    return Boolean(re && isQuarantined(re));
  } catch {
    return false;
  }
}

// ── The trigger hook (§6.9) ───────────────────────────────────────────────────

/** Sync filter of the trigger hook: there is text, and no custom command already answered this message. */
export function hasTriggerContent(ctx) {
  if (typeof ctx?.message?.content !== 'string' || ctx.message.content.length === 0 || ctx.state?.customCommand) return false;
  const s = peekSettings(ctx.guildId);
  return !(s && s.autoTriggersEnabled === false);
}

/**
 * The body of the `custom-commands:triggers` hook. Never throws. Never calls ctx.stop() (this hook is not
 * a stopping hook): a deleted trigger message is skipped by later hooks through wasDeletedByBot.
 * @param {object} ctx HookContext of messageCreate
 * @param {{ checkPermissions?: boolean }} [opts] false only in offline tests
 */
export async function handleTriggers(ctx, { checkPermissions = true } = {}) {
  try {
    const { message, guild, guildId } = ctx;
    if (!message || !guild || !guildId) return;
    if (ctx.state?.customCommand || wasDeletedByBot(message.id)) return;
    // Triggers have their own switch next to the module flag (the runner already checked the module).
    const settings = await ctx.getSettings?.();
    if (settings?.autoTriggersEnabled === false) return;
    const set = await getTriggerSet(guildId);
    if (!set?.entries.length) return;
    const t = selectTrigger(set.entries, message.content, { channel: message.channel, member: message.member });
    if (!t) return;
    if (!triggerChannelThrottle(message.channelId)) return;
    if (checkPermissions) {
      const usable = checkChannelUsable(message.channel, { needs: ['ViewChannel', 'SendMessages'] });
      if (!usable.ok) {
        warnOnce(`${guildId}:trigperm:${message.channelId}`, `Trigger ${t.id} in ${guildId}: ${usable.reason ?? 'cannot send'}`);
        return;
      }
    }

    // The matching trigger on cooldown ends the scan: no other trigger fires (R284). Fails closed when Redis is down.
    let cdKey = null;
    if (t.cooldown > 0) {
      cdKey = `trigger:cd:${guildId}:${t.id}:${message.author.id}`;
      const cd = await acquireCooldown(cdKey, t.cooldown);
      if (!cd.ok) return;
    }

    const payload = renderTemplate(
      { content: t.response },
      { member: message.member, user: message.author, guild, channel: message.channel },
      { allowedMentions: { parse: ['users'] } },
    );
    if (!payload) {
      if (cdKey) await releaseCooldown(cdKey);
      return;
    }
    try {
      await message.channel.send(payload);
    } catch (err) {
      if (cdKey) await releaseCooldown(cdKey);
      warnOnce(`${guildId}:trigsend:${t.id}`, `Trigger ${t.id} in ${guildId}: send failed (${err?.code ?? err?.message})`);
      return; // a failed send never deletes the member's message (R221)
    }
    incrementTriggerUses(t.id);

    if (t.deleteMessage) {
      const canDelete = !checkPermissions
        || message.channel?.permissionsFor?.(guild.members?.me)?.has(PermissionFlagsBits.ManageMessages);
      if (!canDelete) {
        warnOnce(`${guildId}:trigdel:${t.id}`, `Trigger ${t.id} in ${guildId}: I need Manage Messages to delete the matching message`);
      } else {
        markBotDeletion(message.id, 'trigger');
        try {
          await message.delete();
        } catch (err) {
          if (err?.code !== 10008) warnOnce(`${guildId}:trigdel:${t.id}`, `Trigger ${t.id} in ${guildId}: could not delete the message (${err?.code ?? err?.message})`);
        }
      }
    }
  } catch (err) {
    logger.error('Trigger hook failed', err instanceof Error ? err : new Error(String(err)));
  }
}

// ── Resolving a trigger reference (R014) ──────────────────────────────────────

/**
 * `#3` / `3` (the number /trigger list shows), an ID (autocomplete value) or an exact pattern.
 * @returns {Promise<object>} the AutoTrigger row
 * @throws {TagError} NOT_FOUND or AMBIGUOUS
 */
export async function resolveTrigger(guildId, input) {
  const s = String(input ?? '').trim();
  const num = /^#?(\d{1,4})$/.exec(s);
  if (num && Number(num[1]) >= 1) {
    const rows = await prisma.autoTrigger.findMany({
      where: { guildId },
      orderBy: [{ createdAt: 'asc' }, { id: 'asc' }],
      skip: Number(num[1]) - 1,
      take: 1,
    });
    if (rows[0]) return rows[0];
  }
  if (s) {
    const byId = await prisma.autoTrigger.findFirst({ where: { id: s, guildId } });
    if (byId) return byId;
    const byPattern = await prisma.autoTrigger.findMany({
      where: { guildId, trigger: { equals: s, mode: 'insensitive' } },
      take: 2,
    });
    if (byPattern.length === 1) return byPattern[0];
    if (byPattern.length === 2) {
      throw new TagError('AMBIGUOUS', 'Several Matches', 'Several triggers use that pattern; use its #number or ID from /trigger list.');
    }
  }
  throw new TagError('NOT_FOUND', 'Not Found', `No trigger matches \`${codeSafe(clampText(s, 80))}\`. Use /trigger list to see numbers and IDs.`);
}

/** The #n of a trigger: the count of rows ordered up to and including it. */
export async function triggerNumber(guildId, row) {
  return prisma.autoTrigger.count({
    where: { guildId, OR: [{ createdAt: { lt: row.createdAt } }, { createdAt: row.createdAt, id: { lte: row.id } }] },
  });
}

// ── Writes, caps and concurrency (R254) ───────────────────────────────────────

/**
 * Creates a CustomTag under a per-guild advisory lock so concurrent creates cannot pass the cap together.
 * A duplicate name propagates Prisma's P2002 to the caller.
 * @throws {TagError} LIMIT
 */
export async function createTagRow(data) {
  return prisma.$transaction(async (tx) => {
    await tx.$queryRaw(lockSql(`customtag:${data.guildId}`));
    if (await tx.customTag.count({ where: { guildId: data.guildId } }) >= LIMITS.CUSTOM_TAGS) {
      throw new TagError('LIMIT', 'Limit Reached', `This server already has ${LIMITS.CUSTOM_TAGS} custom commands, the maximum. Delete one with \`/tag delete\` first.`);
    }
    return tx.customTag.create({ data });
  });
}

/** Like createTagRow, for triggers. @throws {TagError} LIMIT */
export async function createTriggerRow(data) {
  return prisma.$transaction(async (tx) => {
    await tx.$queryRaw(lockSql(`autotrigger:${data.guildId}`));
    if (await tx.autoTrigger.count({ where: { guildId: data.guildId } }) >= LIMITS.AUTO_TRIGGERS) {
      throw new TagError('LIMIT', 'Limit Reached', `This server already has ${LIMITS.AUTO_TRIGGERS} triggers, the maximum. Remove one with \`/trigger remove\`.`);
    }
    return tx.autoTrigger.create({ data });
  });
}

/**
 * Read-modify-write of one custom command with optimistic concurrency (updatedAt is the token), so
 * two admins never lose each other's change to the JSON or array fields. Two attempts.
 * @param {string} guildId
 * @param {string} rawName
 * @param {(tag: object) => object|null|undefined} mutator pure; returns the fields to change; may throw TagError
 * @returns {Promise<object>} the updated row (the unchanged row when the mutator returned nothing)
 * @throws {TagError} NOT_FOUND or CONFLICT (and whatever the mutator throws)
 */
export async function writeTag(guildId, rawName, mutator) {
  for (let attempt = 0; attempt < 2; attempt++) {
    const tag = await findTag(guildId, rawName);
    if (!tag) throw new TagError('NOT_FOUND', 'Not Found', `No custom command named \`${codeSafe(normalizeTagName(rawName))}\` exists here.`);
    const data = mutator(tag);
    if (!data || Object.keys(data).length === 0) return tag;
    const { count } = await prisma.customTag.updateMany({ where: { id: tag.id, updatedAt: tag.updatedAt }, data });
    if (count === 1) {
      invalidateTagIndex(guildId);
      return prisma.customTag.findUnique({ where: { id: tag.id } });
    }
  }
  throw new TagError('CONFLICT', 'Changed Meanwhile', 'Someone else changed this command. Run the command again.');
}

/**
 * Optimistic update of one trigger; the admin's options were computed from `row`, so there is no retry.
 * @throws {TagError} CONFLICT
 */
export async function writeTrigger(guildId, row, data) {
  const { count } = await prisma.autoTrigger.updateMany({ where: { id: row.id, guildId, updatedAt: row.updatedAt }, data });
  if (count === 0) throw new TagError('CONFLICT', 'Changed Meanwhile', 'Someone else changed this trigger. Run the command again.');
  return prisma.autoTrigger.findUnique({ where: { id: row.id } });
}

// ── Test seam (offline tests only; never used by production code) ─────────────

export const __testing = Object.freeze({
  /** Clears every memo, throttle, primed row and the configuration. */
  reset() {
    tagIndex.clear();
    triggerMemo.clear();
    compiled.clear();
    primed.clear();
    prefixThrottle = createThrottle(1_000);
    noticeThrottle = createThrottle(10_000);
    triggerChannelThrottle = createThrottle(1_000);
    warnThrottle = createThrottle(10 * 60_000);
    formatThrottle = createThrottle(60 * 60_000);
    countUses = true;
  },
  /** The name index becomes the enabled names of `rows`; findTagRow returns these rows without Prisma. */
  primeTags(guildId, rows) {
    primed.set(guildId, new Map(rows.map((r) => [r.name, r])));
    memoPut(tagIndex, guildId, { names: new Set(rows.filter((r) => r.enabled).map((r) => r.name)), at: Date.now() });
  },
  /** A trigger memo entry (compiled through compiledFor) without Prisma or Redis. */
  primeTriggers(guildId, projections) {
    memoPut(triggerMemo, guildId, { entries: makeEntries(projections, guildId), at: Date.now() });
  },
  configure({ countUses: count = true } = {}) {
    countUses = count;
  },
});
