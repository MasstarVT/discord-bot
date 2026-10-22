// Levels (pkg-levels.md): XP maths, atomic XP writes, the level-change pipeline (announcements, reward roles,
// the `levelUp` hook), voice XP sessions, bulk role tasks, the `xp` rewards provider and the hook handlers
// that src/plugins/levels.js wires up. Settings are read through settingsService only; the cache never
// throws (a Redis failure is decided fail-open or fail-closed at each call site).
import { randomUUID } from 'node:crypto';
import { ActionRowBuilder, ButtonBuilder, ButtonStyle, escapeMarkdown } from 'discord.js';
import { Prisma } from '@prisma/client';
import prisma, { SQL_NOW } from '../database/client.js';
import { cache } from './redis.js';
import { ensureGuildSettings, getSettingsSafe, isModuleEnabled, updateSettingsWith } from './settingsService.js';
import { runHooks } from '../handlers/hookRegistry.js';
import { scheduleJob } from './scheduler.js';
import { COLORS, LIMITS, XP_MAX, xpForLevel } from '../config/constants.js';
import { createKeyedMutex, createThrottle, withTimeout } from '../utils/async.js';
import { renderPlaceholders } from '../utils/placeholderParser.js';
import { renderTemplate } from '../utils/customEmbed.js';
import { infoEmbed } from '../utils/embedBuilder.js';
import { checkAssignableRole, checkChannelUsable, isInteractionGone } from '../utils/interactionHelpers.js';
import { clampText, formatNumber } from '../utils/text.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const DEFAULT_LEVELUP_MESSAGE = 'GG {player}, you just advanced to level {level}!';
export const ANNOUNCE_MODES = Object.freeze(['OFF', 'CURRENT', 'DM', 'CHANNEL']);
export const ANNOUNCE_MODE_LABELS = Object.freeze({
  OFF: 'Disabled', CURRENT: 'Current channel', DM: 'Private message', CHANNEL: 'Custom channel',
});
export const XP_RANGE_MAX = 1000;
export const VOICE_KEY_TTL_SECONDS = 93_600; // 26 h: outlives the 24 h segment cap
export const VOICE_SEGMENT_CAP_MINUTES = 1_440; // a single unpaid stretch credits at most 24 h (R194)
export const VOICE_CHECKPOINT_MS = 10 * 60_000; // pay out and rotate a session at most every 10 minutes
export const VOICE_CHANNEL_SCAN_CAP = 50; // other members re-evaluated per channel per event
export const ROLE_GIVE_MAX_MEMBERS = 5_000; // /levelconfig xp give target:@Role
export const BULK_SYNC_MAX_MEMBERS = 5_000; // role add backfill / role sync
export const BULK_SQL_CHUNK = 500;
export const LEADERBOARD_MAX_ROWS = 200; // 20 pages x 10

const DEFAULT_XP_MIN = 15;
const DEFAULT_XP_MAX = 25;
const SNOWFLAKE_RE = /^\d{17,20}$/;
const VOICE_SEED_CAP = 2_000;
const INT_MAX = 2_147_483_647;

// ── In-process state (plain objects, no I/O at import) ───────────────────────

const voiceMutex = createKeyedMutex();
const warnThrottle = createThrottle(60 * 60_000);
const bulkRunning = new Set();
const sweepSeedThrottle = createThrottle(5 * 60_000);
const deprecatedWarned = new Set();

/** Logs a warning at most once per hour per key (a broken channel or role would otherwise flood the log). */
function warnOnce(key, message) {
  if (warnThrottle(key)) logger.warn(message);
}

const asError = (err) => (err instanceof Error ? err : new Error(String(err)));

// ── Pure helpers ──────────────────────────────────────────────────────────────

/**
 * Level reached with `totalXp` accumulated XP. A non-finite or negative input gives 0.
 * @param {number} totalXp
 * @returns {number}
 */
export function levelFromXp(totalXp) {
  const xp = Number(totalXp);
  if (!Number.isFinite(xp) || xp < 0) return 0;
  let level = 0;
  let remaining = xp;
  for (let i = 0; i < 2000 && remaining >= xpForLevel(level); i++) {
    remaining -= xpForLevel(level);
    level++;
  }
  return level;
}

/**
 * XP accumulated within the current level (0 to xpForLevel(level) - 1).
 * @param {number} totalXp
 * @returns {number}
 */
export function xpIntoLevel(totalXp) {
  const xp = Number(totalXp);
  if (!Number.isFinite(xp) || xp < 0) return 0;
  let remaining = xp;
  for (let level = 0; level < 2000 && remaining >= xpForLevel(level); level++) remaining -= xpForLevel(level);
  return remaining;
}

/** XP needed to advance from `level` to the next one. */
export function xpToNextLevel(level) {
  return xpForLevel(level);
}

/**
 * Cumulative XP needed to REACH `level` (levelFromXp(totalXpForLevel(L)) === L).
 * @param {number} level
 * @returns {number}
 */
export function totalXpForLevel(level) {
  const L = Math.trunc(Number(level));
  if (!(L > 0)) return 0;
  return 5 * (((L - 1) * L * (2 * L - 1)) / 6) + 25 * L * (L - 1) + 100 * L;
}

const toRangeValue = (v, fallback) => {
  if (typeof v !== 'number' || !Number.isFinite(v)) return fallback;
  return Math.min(XP_RANGE_MAX, Math.max(1, Math.trunc(v)));
};

/**
 * Each value is truncated and clamped to 1..XP_RANGE_MAX (a non-number becomes the default 15 / 25);
 * the pair is swapped when min is above max.
 * @param {number} min
 * @param {number} max
 * @returns {{ min: number, max: number }}
 */
export function normalizeXpRange(min, max) {
  const lo = toRangeValue(min, DEFAULT_XP_MIN);
  const hi = toRangeValue(max, DEFAULT_XP_MAX);
  return lo > hi ? { min: hi, max: lo } : { min: lo, max: hi };
}

const sanitizeMultiplier = (m) => (typeof m === 'number' && Number.isFinite(m) && m > 0 ? m : 1);
const sanitizeBoost = (b) => (typeof b === 'number' && Number.isFinite(b) && b >= 1 ? b : 1);

/**
 * One random XP award: uniform in [min, max], times the server multiplier and the member's boost, at least 1.
 * @param {{ min: number, max: number, multiplier?: number, boost?: number }} opts
 * @param {() => number} [rng]
 * @returns {number}
 */
export function rollXp({ min, max, multiplier = 1, boost = 1 }, rng = Math.random) {
  const { min: lo, max: hi } = normalizeXpRange(min, max);
  const base = lo + Math.floor(rng() * (hi - lo + 1));
  return Math.max(1, Math.floor(base * sanitizeMultiplier(multiplier) * sanitizeBoost(boost)));
}

/**
 * The only reader of GuildSettings.xpBoostRoles: valid entries only, deduplicated by role (the last value wins),
 * multipliers rounded to two decimals, truncated to LIMITS.XP_BOOST_ROLES. Never mutates its input.
 * @param {unknown} json
 * @returns {Array<{ roleId: string, multiplier: number }>}
 */
export function normalizeBoostRoles(json) {
  if (!Array.isArray(json)) return [];
  const byRole = new Map();
  for (const entry of json) {
    if (!entry || typeof entry !== 'object') continue;
    const { roleId, multiplier } = entry;
    if (typeof roleId !== 'string' || !SNOWFLAKE_RE.test(roleId)) continue;
    if (typeof multiplier !== 'number' || !Number.isFinite(multiplier) || multiplier < 1 || multiplier > 5) continue;
    byRole.set(roleId, Math.round(multiplier * 100) / 100);
  }
  return [...byRole].slice(0, LIMITS.XP_BOOST_ROLES).map(([roleId, multiplier]) => ({ roleId, multiplier }));
}

/**
 * The highest multiplier among the boost roles a member holds (boosts never stack), else 1.
 * @param {Array<{ roleId: string, multiplier: number }>} boostRoles
 * @param {(roleId: string) => boolean} hasRole
 * @returns {number}
 */
export function bestBoostMultiplier(boostRoles, hasRole) {
  let best = 1;
  for (const b of boostRoles ?? []) if (b.multiplier > best && hasRole(b.roleId)) best = b.multiplier;
  return best;
}

/** True when the member holds any role on the no-XP list. */
export function hasNoXpRole(noXpRoleIds, hasRole) {
  return Array.isArray(noXpRoleIds) && noXpRoleIds.some((id) => hasRole(id));
}

/**
 * True when the channel, its parent (category / forum) or, for threads, the parent's category is on the no-XP list (R189).
 * @param {string[]} noXpChannelIds
 * @param {{ id?: string, parentId?: string|null, isThread?: () => boolean, parent?: { parentId?: string|null }|null }|null} channel
 */
export function isNoXpChannel(noXpChannelIds, channel) {
  if (!channel || !Array.isArray(noXpChannelIds) || noXpChannelIds.length === 0) return false;
  const ids = [channel.id, channel.parentId];
  if (channel.isThread?.()) ids.push(channel.parent?.parentId);
  return ids.some((id) => id && noXpChannelIds.includes(id));
}

/**
 * Whether a member earns voice XP right now. `reason` is the first failing check, in this order:
 * no-channel, afk, no-xp-channel, no-xp-role, deafened, alone. Self-mute and stage suppression do not matter.
 * @returns {{ eligible: boolean, reason: string|null }}
 */
export function voiceEligibility({
  channelId, parentId = null, afkChannelId = null, selfDeaf = false, serverDeaf = false,
  noXpChannelIds = [], noXpRole = false, otherListeners = 0,
}) {
  const noXp = Array.isArray(noXpChannelIds) ? noXpChannelIds : [];
  if (!channelId) return { eligible: false, reason: 'no-channel' };
  if (afkChannelId && channelId === afkChannelId) return { eligible: false, reason: 'afk' };
  if (noXp.includes(channelId) || (parentId && noXp.includes(parentId))) return { eligible: false, reason: 'no-xp-channel' };
  if (noXpRole) return { eligible: false, reason: 'no-xp-role' };
  if (selfDeaf || serverDeaf) return { eligible: false, reason: 'deafened' };
  if (!(otherListeners >= 1)) return { eligible: false, reason: 'alone' };
  return { eligible: true, reason: null };
}

/**
 * Non-bot members of a voice channel other than `selfId` who are not deafened.
 * @param {{ members?: Iterable<any> }|null} channel
 * @param {string} selfId
 * @returns {number}
 */
export function countOtherListeners(channel, selfId) {
  const members = channel?.members;
  if (!members) return 0;
  const iterable = typeof members.values === 'function' ? members.values() : members;
  let n = 0;
  for (const m of iterable) {
    if (!m || m.id === selfId || m.user?.bot) continue;
    if (m.voice?.selfDeaf || m.voice?.serverDeaf) continue;
    n++;
  }
  return n;
}

/**
 * Parses a stored voice session: `{ start, channelId }`, or a legacy number / numeric string.
 * @param {unknown} raw
 * @param {number} [now]
 * @returns {{ start: number, channelId: string|null }|null}
 */
export function parseVoiceSession(raw, now = Date.now()) {
  let start;
  let channelId = null;
  if (raw && typeof raw === 'object') {
    start = Number(raw.start);
    if (typeof raw.channelId === 'string' && raw.channelId) channelId = raw.channelId;
  } else if (typeof raw === 'number') {
    start = raw;
  } else if (typeof raw === 'string' && /^\d+$/.test(raw.trim())) {
    start = Number(raw);
  } else {
    return null;
  }
  if (!Number.isFinite(start) || start <= 0 || start > now + 60_000) return null;
  return { start, channelId };
}

/**
 * The paid part of a voice stretch. Time before `readyAt` (this process becoming ready) is never credited, one
 * stretch pays at most `capMinutes` (the excess is dropped), and the sub-minute remainder is carried in `nextStart`.
 * @returns {{ minutes: number, nextStart: number }}
 */
export function computeVoiceSegment({ start, now, readyAt = 0, capMinutes = VOICE_SEGMENT_CAP_MINUTES }) {
  const effective = Math.min(now, Math.max(start, readyAt));
  const raw = Math.floor((now - effective) / 60_000);
  if (raw >= capMinutes) return { minutes: capMinutes, nextStart: now };
  return { minutes: raw, nextStart: effective + raw * 60_000 };
}

/** XP for `minutes` of voice: rate (1-50) per minute times the multiplier and the boost, at least 1. */
export function voiceXpForMinutes(minutes, { rate, multiplier = 1, boost = 1 } = {}) {
  if (!(minutes >= 1)) return 0;
  const r = typeof rate === 'number' && Number.isFinite(rate) ? Math.min(50, Math.max(1, rate)) : 5;
  return Math.max(1, Math.floor(minutes * r * sanitizeMultiplier(multiplier) * sanitizeBoost(boost)));
}

/**
 * Which reward roles to add and remove for a member at `level`. The caller applies every add before any remove (R060).
 * @param {{ rewards: Array<{ level: number, roleId: string }>, level: number, stack: boolean, syncDown: boolean,
 *   memberRoleIds: { has: (id: string) => boolean } | string[] }} opts
 * @returns {{ add: string[], remove: string[] }}
 */
export function planLevelRoles({ rewards, level, stack, syncDown, memberRoleIds }) {
  const holds = typeof memberRoleIds?.has === 'function' ? (id) => memberRoleIds.has(id) : ((set) => (id) => set.has(id))(new Set(memberRoleIds ?? []));
  const sorted = [...(rewards ?? [])].sort((a, b) => a.level - b.level);
  const earned = [];
  const unearned = [];
  let top = null;
  for (const r of sorted) {
    if (r.level <= level) {
      if (!earned.includes(r.roleId)) earned.push(r.roleId);
      top = r.roleId;
    }
  }
  for (const r of sorted) if (!earned.includes(r.roleId) && !unearned.includes(r.roleId)) unearned.push(r.roleId);

  const add = [];
  const remove = [];
  if (stack) {
    for (const id of earned) if (!holds(id)) add.push(id);
  } else if (top !== null) {
    if (!holds(top)) add.push(top);
    for (const id of earned) if (id !== top && holds(id)) remove.push(id);
  }
  if (syncDown) for (const id of unearned) if (holds(id)) remove.push(id);
  return { add: [...new Set(add)], remove: [...new Set(remove)].filter((id) => !add.includes(id)) };
}

/**
 * Where a level-up goes (the §7.3 matrix). Admin XP changes never announce; an unknown mode counts as CURRENT.
 * @returns {{ kind: 'none' } | { kind: 'dm' } | { kind: 'channel', channelId: string, fallback: boolean }}
 */
export function resolveAnnounceTarget({ mode, source, levelUpChannelId = null, sourceChannelId = null }) {
  if (source === 'admin') return { kind: 'none' };
  const m = ANNOUNCE_MODES.includes(mode) ? mode : 'CURRENT';
  if (m === 'OFF') return { kind: 'none' };
  if (m === 'DM') return { kind: 'dm' };
  if (m === 'CURRENT') return sourceChannelId ? { kind: 'channel', channelId: sourceChannelId, fallback: false } : { kind: 'none' };
  if (levelUpChannelId) return { kind: 'channel', channelId: levelUpChannelId, fallback: false };
  return sourceChannelId ? { kind: 'channel', channelId: sourceChannelId, fallback: true } : { kind: 'none' };
}

/** True when a level-up template pings the member (so embed mode must add the mention as message content). */
export function templateMentionsPlayer(template) {
  return /\{(player|mention|user|user\.mention)\}/i.test(String(template ?? ''));
}

/**
 * Placeholder variables of a level-up message. Keys are lower-case (the renderer looks variables up lower-cased).
 */
export function levelUpVars({ user, member, level, xp, rank }) {
  return {
    player: `<@${user.id}>`,
    'player.name': escapeMarkdown(String(member?.displayName ?? user.globalName ?? user.username ?? '')), // like achievements: a name is not markdown
    level: String(level),
    rank: rank == null ? '?' : String(rank),
    xp: formatNumber(xp),
    mention: `<@${user.id}>`,
  };
}

/**
 * Builds the level-up message. Every token is replaced in a single pass by the shared renderer (R190); the ping is
 * scoped to the levelled member only (never a role, @everyone or {server.owner}); in embed mode the mention goes in
 * `content`, because a mention inside an embed never notifies (R298).
 * @returns {object|null} a message payload, or null when nothing visible remains
 */
export function buildLevelUpPayload({ template, useEmbed, member, user, guild, channel, level, xp, rank, embedColor, dm = false }) {
  const renderCtx = { member, user, guild, channel, vars: levelUpVars({ user, member, level, xp, rank }) };
  const mentionOnly = { users: [user.id] };
  const row = dm
    ? new ActionRowBuilder().addComponents(
      new ButtonBuilder()
        .setCustomId('noop:levelup')
        .setLabel(clampText(`Sent from ${guild.name}`, 80))
        .setStyle(ButtonStyle.Secondary)
        .setDisabled(true),
    )
    : null;

  if (!useEmbed) {
    const content = renderPlaceholders(template, renderCtx, { maxLength: 2000 });
    if (!content.trim()) return null;
    return { content, allowedMentions: mentionOnly, ...(row ? { components: [row] } : {}) };
  }

  // An embed whose text renders empty would show only a thumbnail: send nothing instead.
  if (!renderPlaceholders(template, renderCtx, { maxLength: 4096 }).trim()) return null;
  const tpl = {
    content: templateMentionsPlayer(template) ? `<@${user.id}>` : null,
    embed: { description: template, thumbnail: '{user.avatar}' },
    buttons: [],
  };
  const payload = renderTemplate(tpl, renderCtx, {
    defaultColor: Number.isInteger(embedColor) ? embedColor : COLORS.SUCCESS,
    allowedMentions: mentionOnly,
    ...(row ? { extraComponents: [row] } : {}),
  });
  if (!payload || (!payload.content && !payload.embeds?.length)) return null;
  return payload;
}

/**
 * The single tie rule shared by `{rank}`, /rank, /levelconfig xp view and the leaderboard (which orders by xp desc, userId asc).
 * @returns {object} a Prisma `where` for the rows ranked ahead of this member
 */
export function rankWhere(guildId, userId, xp) {
  return { guildId, OR: [{ xp: { gt: xp } }, { xp, userId: { lt: userId } }] };
}

// ── Raw SQL (00-overview rule 22; every timestamp is SQL_NOW) ────────────────

const toInt = (v, min, max) => Math.min(max, Math.max(min, Math.trunc(Number(v)) || 0));

const applyDeltaSql = ({ id, guildId, userId, delta, messages, voiceMinutes, touchLastXp }) => Prisma.sql`
  INSERT INTO "UserLevel" ("id","guildId","userId","xp","level","messageCount","voiceMinutes","lastXpAt","createdAt","updatedAt")
  VALUES (${id}, ${guildId}, ${userId}, LEAST(${delta}::bigint, ${XP_MAX}::bigint)::int, 0, ${messages}, ${voiceMinutes},
          CASE WHEN ${touchLastXp}::boolean THEN ${SQL_NOW} ELSE NULL END, ${SQL_NOW}, ${SQL_NOW})
  ON CONFLICT ("guildId","userId") DO UPDATE SET
    "xp"           = LEAST("UserLevel"."xp"::bigint + ${delta}::bigint, ${XP_MAX}::bigint)::int,
    "messageCount" = LEAST("UserLevel"."messageCount"::bigint + ${messages}::bigint, ${INT_MAX}::bigint)::int,
    "voiceMinutes" = LEAST("UserLevel"."voiceMinutes"::bigint + ${voiceMinutes}::bigint, ${INT_MAX}::bigint)::int,
    "lastXpAt"     = CASE WHEN ${touchLastXp}::boolean THEN ${SQL_NOW} ELSE "UserLevel"."lastXpAt" END,
    "updatedAt"    = ${SQL_NOW}
  RETURNING "xp", "level", (xmax = 0) AS "created"`;

const subtractSql = ({ guildId, userId, amount }) => Prisma.sql`
  UPDATE "UserLevel" AS t SET "xp" = GREATEST(t."xp"::bigint - ${amount}::bigint, 0)::int, "updatedAt" = ${SQL_NOW}
  FROM (SELECT "id", "xp" AS "oldXp" FROM "UserLevel" WHERE "guildId" = ${guildId} AND "userId" = ${userId} FOR UPDATE) AS o
  WHERE t."id" = o."id"
  RETURNING t."xp", t."level", o."oldXp"`;

const setZeroSql = ({ guildId, userId }) => Prisma.sql`
  UPDATE "UserLevel" SET "xp" = 0, "updatedAt" = ${SQL_NOW}
  WHERE "guildId" = ${guildId} AND "userId" = ${userId}
  RETURNING "xp", "level"`;

const setXpSql = ({ id, guildId, userId, xp }) => Prisma.sql`
  INSERT INTO "UserLevel" ("id","guildId","userId","xp","level","messageCount","voiceMinutes","createdAt","updatedAt")
  VALUES (${id}, ${guildId}, ${userId}, LEAST(${xp}::bigint, ${XP_MAX}::bigint)::int, 0, 0, 0, ${SQL_NOW}, ${SQL_NOW})
  ON CONFLICT ("guildId","userId") DO UPDATE SET "xp" = EXCLUDED."xp", "updatedAt" = ${SQL_NOW}
  RETURNING "xp", "level", (xmax = 0) AS "created"`;

const resetSql = ({ guildId, userId }) => Prisma.sql`
  DELETE FROM "UserLevel" WHERE "guildId" = ${guildId} AND "userId" = ${userId} RETURNING "level"`;

const storedLevelSql = ({ guildId, userId, xpAfter, newLevel }) => Prisma.sql`
  UPDATE "UserLevel" SET "level" = ${newLevel}, "updatedAt" = ${SQL_NOW}
  WHERE "guildId" = ${guildId} AND "userId" = ${userId} AND "xp" = ${xpAfter} AND "level" <> ${newLevel}`;

const bulkGiveSql = ({ guildId, amount, ids, userIds }) => Prisma.sql`
  INSERT INTO "UserLevel" ("id","guildId","userId","xp","level","messageCount","voiceMinutes","createdAt","updatedAt")
  SELECT u."id", ${guildId}, u."userId", LEAST(${amount}::bigint, ${XP_MAX}::bigint)::int, 0, 0, 0, ${SQL_NOW}, ${SQL_NOW}
  FROM unnest(${ids}::text[], ${userIds}::text[]) AS u("id","userId")
  ON CONFLICT ("guildId","userId") DO UPDATE SET
    "xp" = LEAST("UserLevel"."xp"::bigint + ${amount}::bigint, ${XP_MAX}::bigint)::int, "updatedAt" = ${SQL_NOW}
  RETURNING "userId", "xp", "level"`;

const bulkLevelSql = ({ guildId, userIds, xps, levels }) => Prisma.sql`
  UPDATE "UserLevel" AS t SET "level" = v."level", "updatedAt" = ${SQL_NOW}
  FROM unnest(${userIds}::text[], ${xps}::int[], ${levels}::int[]) AS v("userId","xp","level")
  WHERE t."guildId" = ${guildId} AND t."userId" = v."userId" AND t."xp" = v."xp" AND t."level" <> v."level"
  RETURNING t."userId"`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'levels.applyXpDelta', build: () => applyDeltaSql({ id: 'sample', guildId: '0', userId: '0', delta: 1, messages: 1, voiceMinutes: 0, touchLastXp: true }) },
  { name: 'levels.subtractXp', build: () => subtractSql({ guildId: '0', userId: '0', amount: 1 }) },
  { name: 'levels.setXpZero', build: () => setZeroSql({ guildId: '0', userId: '0' }) },
  { name: 'levels.setXp', build: () => setXpSql({ id: 'sample', guildId: '0', userId: '0', xp: 1 }) },
  { name: 'levels.resetXp', build: () => resetSql({ guildId: '0', userId: '0' }) },
  { name: 'levels.setStoredLevel', build: () => storedLevelSql({ guildId: '0', userId: '0', xpAfter: 1, newLevel: 1 }) },
  { name: 'levels.bulkGive', build: () => bulkGiveSql({ guildId: '0', amount: 1, ids: ['sample'], userIds: ['0'] }) },
  { name: 'levels.bulkLevel', build: () => bulkLevelSql({ guildId: '0', userIds: ['0'], xps: [1], levels: [1] }) },
]);

// ── Database operations ───────────────────────────────────────────────────────

/**
 * Atomically adds XP (clamped to XP_MAX in SQL). The stored `level` column is NOT touched here; the writer that wins
 * setStoredLevel owns the transition.
 * @param {{ guildId: string, userId: string, delta: number, messages?: number, voiceMinutes?: number, touchLastXp?: boolean }} opts
 * @returns {Promise<{ xp: number, oldLevel: number, newLevel: number, created: boolean }>}
 * @throws on DB failure
 */
export async function applyXpDelta({ guildId, userId, delta, messages = 0, voiceMinutes = 0, touchLastXp = false }) {
  await ensureGuildSettings(guildId);
  const rows = await prisma.$queryRaw(applyDeltaSql({
    id: randomUUID(),
    guildId,
    userId,
    delta: toInt(delta, 1, XP_MAX),
    messages: toInt(messages, 0, INT_MAX),
    voiceMinutes: toInt(voiceMinutes, 0, INT_MAX),
    touchLastXp: Boolean(touchLastXp),
  }));
  const row = rows[0];
  return { xp: row.xp, oldLevel: row.level, newLevel: levelFromXp(row.xp), created: row.created === true };
}

/**
 * Atomically removes XP (floored at 0). Never inserts a row (R300).
 * @returns {Promise<{ xp: number, oldXp: number, oldLevel: number, newLevel: number }|null>} null when the member has no row
 * @throws on DB failure
 */
export async function subtractXp({ guildId, userId, amount }) {
  await ensureGuildSettings(guildId);
  const rows = await prisma.$queryRaw(subtractSql({ guildId, userId, amount: toInt(amount, 1, XP_MAX) }));
  const row = rows[0];
  if (!row) return null;
  return { xp: row.xp, oldXp: row.oldXp, oldLevel: row.level, newLevel: levelFromXp(row.xp) };
}

/**
 * Sets total XP (clamped to 0..XP_MAX). Setting 0 never creates a row.
 * @returns {Promise<{ xp: number, oldLevel: number, newLevel: number, created: boolean }|null>}
 * @throws on DB failure
 */
export async function setXp({ guildId, userId, xp }) {
  await ensureGuildSettings(guildId);
  const value = toInt(xp, 0, XP_MAX);
  if (value === 0) {
    const rows = await prisma.$queryRaw(setZeroSql({ guildId, userId }));
    const row = rows[0];
    return row ? { xp: 0, oldLevel: row.level, newLevel: 0, created: false } : null;
  }
  const rows = await prisma.$queryRaw(setXpSql({ id: randomUUID(), guildId, userId, xp: value }));
  const row = rows[0];
  return { xp: row.xp, oldLevel: row.level, newLevel: levelFromXp(row.xp), created: row.created === true };
}

/**
 * Deletes a member's XP row.
 * @returns {Promise<{ oldLevel: number }|null>} null when there was no row
 * @throws on DB failure
 */
export async function resetXp(guildId, userId) {
  const rows = await prisma.$queryRaw(resetSql({ guildId, userId }));
  return rows[0] ? { oldLevel: rows[0].level } : null;
}

/**
 * Writes the stored level only while the XP is still the value the caller saw, so a writer whose XP was superseded
 * never writes a stale level. True for exactly one writer per transition.
 * @returns {Promise<boolean>}
 * @throws on DB failure
 */
export async function setStoredLevel(guildId, userId, xpAfter, newLevel) {
  const affected = await prisma.$executeRaw(storedLevelSql({ guildId, userId, xpAfter, newLevel }));
  return affected === 1;
}

/**
 * Gives the same XP to many members in chunked statements.
 * @param {{ guildId: string, userIds: string[], amount: number }} opts
 * @returns {Promise<{ updated: number, levelUps: Array<{ userId: string, oldLevel: number, newLevel: number, xp: number }> }>}
 * @throws on DB failure
 */
export async function giveXpToMembers({ guildId, userIds, amount }) {
  await ensureGuildSettings(guildId);
  const unique = [...new Set(userIds)];
  const give = toInt(amount, 1, XP_MAX);
  let updated = 0;
  const levelUps = [];
  for (let i = 0; i < unique.length; i += BULK_SQL_CHUNK) {
    const chunk = unique.slice(i, i + BULK_SQL_CHUNK);
    const rows = await prisma.$queryRaw(bulkGiveSql({ guildId, amount: give, ids: chunk.map(() => randomUUID()), userIds: chunk }));
    updated += rows.length;
    const stale = rows.filter((r) => levelFromXp(r.xp) !== r.level);
    if (!stale.length) continue;
    const owned = await prisma.$queryRaw(bulkLevelSql({
      guildId,
      userIds: stale.map((r) => r.userId),
      xps: stale.map((r) => r.xp),
      levels: stale.map((r) => levelFromXp(r.xp)),
    }));
    const won = new Set(owned.map((r) => r.userId));
    for (const r of stale) {
      const newLevel = levelFromXp(r.xp);
      if (won.has(r.userId) && newLevel > r.level) levelUps.push({ userId: r.userId, oldLevel: r.level, newLevel, xp: r.xp });
    }
  }
  return { updated, levelUps };
}

/**
 * 1-based leaderboard position (xp desc, userId asc).
 * @returns {Promise<number>}
 */
export async function getRank(guildId, userId, xp) {
  return (await prisma.userLevel.count({ where: rankWhere(guildId, userId, xp) })) + 1;
}

// ── Reward roles ──────────────────────────────────────────────────────────────

/**
 * Brings a member's reward roles in line with `level`: adds first, then removes, one role per call (R060, rule 13).
 * Never throws; failures are returned and logged (throttled).
 * @param {import('discord.js').GuildMember} member
 * @param {number} level
 * @param {object} settings
 * @param {{ syncDown?: boolean, rewards?: Array<{ level: number, roleId: string }>|null, reason?: string }} [opts]
 * @returns {Promise<{ added: string[], removed: string[], failed: Array<{ roleId: string, reason: string }> }>}
 */
export async function syncLevelRoles(member, level, settings, { syncDown = false, rewards = null, reason = 'Level rewards' } = {}) {
  const out = { added: [], removed: [], failed: [] };
  try {
    const guild = member.guild;
    let list = rewards;
    if (!list) {
      try {
        list = await prisma.levelRole.findMany({ where: { guildId: guild.id } });
      } catch (err) {
        logger.error(`levels: could not load role rewards of guild ${guild.id}`, asError(err));
        return out;
      }
    }
    if (!list.length) return out;
    const plan = planLevelRoles({
      rewards: list, level, stack: settings?.levelRolesStack !== false, syncDown, memberRoleIds: member.roles.cache,
    });

    const apply = async (roleId, op) => {
      const role = guild.roles.cache.get(roleId);
      const check = checkAssignableRole(guild, role, { allowDangerous: true });
      if (!check.ok) {
        out.failed.push({ roleId, reason: check.reason });
        warnOnce(`role:${guild.id}:${roleId}`, `levels: cannot manage role ${roleId} in ${guild.id}: ${check.reason}`);
        return;
      }
      try {
        if (op === 'add') await member.roles.add(roleId, reason);
        else await member.roles.remove(roleId, reason);
        (op === 'add' ? out.added : out.removed).push(roleId);
      } catch (err) {
        const code = err?.code;
        const why = String(err?.message ?? err);
        out.failed.push({ roleId, reason: why });
        if (code === 10011 || code === 10007) return;
        if (code === 50013 || code === 50001) warnOnce(`role:${guild.id}:${roleId}`, `levels: Discord refused to ${op} role ${roleId} in ${guild.id}: ${why}`);
        else logger.warn(`levels: could not ${op} role ${roleId} in ${guild.id}: ${why}`);
      }
    };

    for (const roleId of plan.add) await apply(roleId, 'add');
    for (const roleId of plan.remove) await apply(roleId, 'remove');
  } catch (err) {
    logger.error('levels: role sync failed', asError(err));
  }
  return out;
}

// ── Announcements ─────────────────────────────────────────────────────────────

/**
 * Sends the level-up message where the settings say. Never throws.
 * @returns {Promise<boolean>} true when a message was sent
 */
export async function announceLevelUp({ guild, member, level, xp, settings, source, sourceChannel = null }) {
  try {
    const target = resolveAnnounceTarget({
      mode: settings.levelUpMode, source, levelUpChannelId: settings.levelUpChannelId, sourceChannelId: sourceChannel?.id ?? null,
    });
    if (target.kind === 'none') return false;

    const template = settings.levelUpMessage?.trim() ? settings.levelUpMessage : DEFAULT_LEVELUP_MESSAGE;
    const rank = /\{rank\}/i.test(template) ? await getRank(guild.id, member.id, xp).catch(() => null) : null;
    const payload = buildLevelUpPayload({
      template,
      useEmbed: settings.levelUpUseEmbed === true,
      member,
      user: member.user,
      guild,
      channel: sourceChannel,
      level,
      xp,
      rank,
      embedColor: settings.embedColor,
      dm: target.kind === 'dm',
    });
    if (!payload) return false;

    if (target.kind === 'dm') {
      try {
        await member.send(payload);
      } catch (err) {
        if (err?.code !== 50007) warnOnce(`announce-dm:${guild.id}`, `levels: could not DM level-up to ${member.id} in ${guild.id}: ${err?.message ?? err}`);
        return false;
      }
      return true;
    }

    const id = target.channelId;
    const ch = (sourceChannel && sourceChannel.id === id ? sourceChannel : null)
      ?? guild.channels.cache.get(id)
      ?? await guild.channels.fetch(id).catch(() => null);
    const needs = settings.levelUpUseEmbed ? ['ViewChannel', 'SendMessages', 'EmbedLinks'] : ['ViewChannel', 'SendMessages'];
    const usable = checkChannelUsable(ch, { needs });
    if (!usable.ok) {
      // No fallback to another channel: a deleted or forbidden custom channel stays silent until an admin fixes it
      // (`/levelconfig status` flags it).
      warnOnce(`announce:${guild.id}`, `levels: cannot announce in ${id} (${usable.reason ?? usable.missing.join(', ')}) for guild ${guild.id}`);
      return false;
    }
    try {
      await ch.send(payload);
    } catch (err) {
      const code = err?.code;
      if (code === 10003 || code === 50001 || code === 50013) warnOnce(`announce:${guild.id}`, `levels: could not announce in ${id} for guild ${guild.id}: ${err?.message ?? err}`);
      else logger.warn(`levels: could not announce in ${id} for guild ${guild.id}: ${err?.message ?? err}`);
      return false;
    }
    return true;
  } catch (err) {
    logger.error('levels: announcement failed', asError(err));
    return false;
  }
}

/**
 * The level-change pipeline: record the new stored level (one winner per transition), announce a level-up once for the
 * final level, sync reward roles, emit `levelUp`. A decrease only re-syncs roles (when the toggle is on). Never throws.
 * @param {{ guild: object, member?: object|null, userId: string, result: { oldLevel: number, newLevel: number, xp: number },
 *   settings: object, source: 'message'|'voice'|'admin'|'reward', channel?: object|null, announce?: boolean }} opts
 * @returns {Promise<{ leveledUp: boolean, roles: object|null }>}
 */
export async function handleLevelChange({ guild, member = null, userId, result, settings, source, channel = null, announce = true }) {
  const { oldLevel, newLevel, xp } = result;
  if (oldLevel === newLevel) return { leveledUp: false, roles: null };
  try {
    const owned = await setStoredLevel(guild.id, userId, xp, newLevel);
    if (!owned) return { leveledUp: false, roles: null }; // a concurrent writer owns this transition

    const who = member ?? await guild.members.fetch(userId).catch(() => null);

    if (newLevel > oldLevel) {
      if (announce && who && isModuleEnabled(settings, 'leveling')) {
        await announceLevelUp({ guild, member: who, level: newLevel, xp, settings, source, sourceChannel: channel });
      }
      const roles = who
        ? await syncLevelRoles(who, newLevel, settings, { syncDown: false, reason: `Level ${newLevel} reward` })
        : null;
      await runHooks('levelUp', { client: guild.client, guild, member: who, userId, oldLevel, newLevel, channel: channel ?? null, source });
      return { leveledUp: true, roles };
    }

    const roles = who && settings?.levelRolesSyncOnDemotion
      ? await syncLevelRoles(who, newLevel, settings, { syncDown: true, reason: 'Level decreased' })
      : null;
    return { leveledUp: false, roles };
  } catch (err) {
    logger.error(`levels: level change failed for ${userId} in ${guild?.id}`, asError(err));
    return { leveledUp: false, roles: null };
  }
}

// ── Message XP ────────────────────────────────────────────────────────────────

/**
 * messageCreate hook. The core already dropped bots, webhooks, DMs and system messages and the module gate guarantees
 * xpEnabled.
 * @param {object} ctx HookContext
 */
export async function handleMessageXp(ctx) {
  const { message, member, guild } = ctx;
  const settings = await ctx.getSettings();
  if (!settings) return;
  if (isNoXpChannel(settings.noXpChannelIds, message.channel)) return; // R189
  const hasRole = (id) => member.roles.cache.has(id);
  if (hasNoXpRole(settings.noXpRoleIds, hasRole)) return;

  // Atomic cooldown claim (R295-R297). `null` means Redis is unavailable: FAIL CLOSED (no XP), because without the
  // cooldown a spammer would earn XP on every message. The check runs after the exemptions so exempt messages
  // do not consume the cooldown.
  const claimed = await cache.setNX(`xp:cooldown:${guild.id}:${member.id}`, 1, Math.max(1, Math.trunc(settings.xpCooldown) || 1));
  if (claimed !== true) return;

  const amount = rollXp({
    min: settings.xpMin,
    max: settings.xpMax,
    multiplier: settings.xpMultiplier,
    boost: bestBoostMultiplier(normalizeBoostRoles(settings.xpBoostRoles), hasRole),
  });
  const r = await applyXpDelta({ guildId: guild.id, userId: member.id, delta: amount, messages: 1, touchLastXp: true });
  if (r.newLevel !== r.oldLevel) {
    // Detached, so role edits and the announcement never delay the later messageCreate hooks.
    void handleLevelChange({ guild, member, userId: member.id, result: r, settings, source: 'message', channel: message.channel, announce: true })
      .catch((err) => logger.error('levels: level change failed', asError(err)));
  }
}

// ── Voice XP ──────────────────────────────────────────────────────────────────

const voiceKey = (guildId, userId) => `voice:start:${guildId}:${userId}`;

async function payVoiceMinutes({ guild, member, settings, minutes, channelId }) {
  try {
    const xp = voiceXpForMinutes(minutes, {
      rate: settings.voiceXpRate,
      multiplier: settings.xpMultiplier,
      boost: bestBoostMultiplier(normalizeBoostRoles(settings.xpBoostRoles), (id) => member.roles.cache.has(id)),
    });
    if (xp < 1) return;
    const r = await applyXpDelta({ guildId: guild.id, userId: member.id, delta: xp, voiceMinutes: minutes, touchLastXp: true });
    if (r.newLevel !== r.oldLevel) {
      // A failure here loses at most this segment: the session key was already rotated or deleted.
      void handleLevelChange({
        guild, member, userId: member.id, result: r, settings, source: 'voice',
        channel: (channelId && guild.channels.cache.get(channelId)) || null, announce: true,
      }).catch((err) => logger.error('levels: voice level change failed', asError(err)));
    }
  } catch (err) {
    logger.error(`levels: could not credit voice XP to ${member?.id} in ${guild?.id}`, asError(err));
  }
}

/**
 * Starts, checkpoints or ends one member's voice session according to their current eligibility (R054, R191-R195).
 * Serialised per member. Returns whether the member is eligible now.
 * @param {{ guild: object, member: object, state: object|null, settings: object, now?: number, freshJoin?: boolean }} opts
 * @returns {Promise<boolean>}
 */
export function reconcileVoiceMember({ guild, member, state, settings, now = Date.now(), freshJoin = false }) {
  return voiceMutex.run(`${guild.id}:${member.id}`, async () => {
    try {
      const key = voiceKey(guild.id, member.id);
      const readyAt = guild.client?.readyTimestamp ?? now; // never credit time before this process was ready (R194)
      const channel = state?.channel ?? null;
      const elig = voiceEligibility({
        channelId: channel?.id ?? null,
        parentId: channel?.parentId ?? null,
        afkChannelId: guild.afkChannelId,
        selfDeaf: Boolean(state?.selfDeaf),
        serverDeaf: Boolean(state?.serverDeaf),
        noXpChannelIds: settings.noXpChannelIds,
        noXpRole: hasNoXpRole(settings.noXpRoleIds, (id) => member.roles.cache.has(id)),
        otherListeners: countOtherListeners(channel, member.id),
      });
      let session = parseVoiceSession(await cache.get(key), now);
      if (session && freshJoin) {
        // A stored session on a fresh join is stale (a missed leave): discard it without credit. The key is deleted
        // too, so an ineligible join (an empty channel) cannot leave it to be paid out by a later event.
        await cache.del(key);
        session = null;
      }

      if (!elig.eligible) {
        // Left, AFK, no-XP channel, deafened or alone.
        if (session) {
          await cache.del(key);
          const seg = computeVoiceSegment({ start: session.start, now, readyAt });
          if (seg.minutes >= 1) await payVoiceMinutes({ guild, member, settings, minutes: seg.minutes, channelId: session.channelId });
        }
        return false;
      }

      if (!session) {
        // (Re)start, whatever the transition: covers AFK -> normal, un-deafen, a partner joining (R054/R192/R193).
        // Redis down: set() reports false and no session starts (fail closed).
        await cache.set(key, { start: now, channelId: channel.id }, VOICE_KEY_TTL_SECONDS);
        return true;
      }

      const effective = Math.max(session.start, readyAt);
      if (session.channelId !== channel.id || now - effective >= VOICE_CHECKPOINT_MS) {
        const seg = computeVoiceSegment({ start: session.start, now, readyAt });
        await cache.set(key, { start: seg.nextStart, channelId: channel.id }, VOICE_KEY_TTL_SECONDS);
        if (seg.minutes >= 1) await payVoiceMinutes({ guild, member, settings, minutes: seg.minutes, channelId: session.channelId ?? channel.id });
      }
      return true;
    } catch (err) {
      logger.error(`levels: voice reconcile failed for ${member?.id} in ${guild?.id}`, asError(err));
      return false;
    }
  });
}

/** Seeds the recurring 10-minute checkpoint job for a guild (one row per guild; an existing one is kept). */
async function seedVoiceSweep(guild) {
  try {
    await scheduleJob({
      guildId: guild.id,
      type: 'levels.voiceSweep',
      runAt: new Date(Date.now() + VOICE_CHECKPOINT_MS),
      dedupeKey: `levels.voiceSweep:${guild.id}`,
      onConflict: 'keep',
    });
  } catch (err) {
    logger.warn(`levels: could not schedule the voice sweep of guild ${guild.id}: ${err?.message ?? err}`);
  }
}

/**
 * Sync filter of the voiceStateUpdate hook. Eligibility depends only on the channel and the deafen state, so a 'state'
 * change that left both deafen flags alone (mute, camera, stream, suppress) changes nobody's voice XP and is skipped.
 * Sessions still checkpoint through the 10-minute sweep.
 * @param {object} ctx HookContext
 * @returns {boolean}
 */
export function voiceChangeMatters(ctx) {
  if (ctx?.change !== 'state') return true;
  const deaf = (s) => Boolean(s?.selfDeaf || s?.serverDeaf);
  return deaf(ctx.oldState) !== deaf(ctx.newState);
}

// Re-evaluates the other members of the given channels, one at a time (each takes its member's mutex). Never throws.
async function reconcileOthers({ guild, member, channels, settings, now }) {
  for (const ch of channels) {
    let scanned = 0;
    for (const m of ch.members.values()) {
      if (m.user?.bot || m.id === member.id) continue;
      if (scanned++ >= VOICE_CHANNEL_SCAN_CAP) break;
      await reconcileVoiceMember({ guild, member: m, state: m.voice, settings, now });
    }
  }
}

/**
 * voiceStateUpdate hook. The core dropped bots; `ctx.change` is join / leave / move / state.
 * @param {object} ctx HookContext
 */
export async function handleVoiceStateXp(ctx) {
  const { guild, member, oldState, newState } = ctx;
  const settings = await ctx.getSettings();
  if (!settings) return; // settings unavailable (database down): leave the session alone
  if (!settings.voiceXpEnabled) {
    await cache.del(voiceKey(guild.id, member.id)); // drop without credit
    return;
  }
  const now = Date.now();
  await reconcileVoiceMember({ guild, member, state: newState, settings, now, freshJoin: ctx.change === 'join' });

  // Other members whose "alone" status may have changed (R191). Another member is eligible only with at least one
  // other listener, so their status can flip only when a channel now has one or two listeners (the count crossed
  // 1 <-> 2); busier channels are skipped. The re-check is detached so logging and automations do not wait for it.
  const channelIds = new Set();
  if (oldState?.channelId && oldState.channelId !== newState?.channelId) channelIds.add(oldState.channelId);
  if (newState?.channelId) channelIds.add(newState.channelId);
  const channels = [];
  for (const id of channelIds) {
    const ch = guild.channels.cache.get(id);
    if (ch?.members && countOtherListeners(ch, null) <= 2) channels.push(ch);
  }
  if (channels.length) {
    void reconcileOthers({ guild, member, channels, settings, now })
      .catch((err) => logger.error('levels: voice re-check of other members failed', asError(err)));
  }

  if (ctx.change === 'join' && sweepSeedThrottle(guild.id)) await seedVoiceSweep(guild);
}

/**
 * guildReady hook: starts sessions for members already in voice (joined during downtime) and seeds the sweep job.
 * `ctx.reason` 'available' behaves like 'startup'.
 * @param {object} ctx HookContext
 */
export async function seedVoiceSessions(ctx) {
  const settings = await ctx.getSettings();
  if (!settings?.voiceXpEnabled) return;
  const guild = ctx.guild;
  let seen = 0;
  let anyEligible = false;
  for (const state of guild.voiceStates?.cache?.values() ?? []) {
    if (seen >= VOICE_SEED_CAP) break;
    if (!state.channelId || !state.member || state.member.user?.bot) continue;
    seen++;
    if (await reconcileVoiceMember({ guild, member: state.member, state, settings })) anyEligible = true;
  }
  if (anyEligible) await seedVoiceSweep(guild);
}

/**
 * `levels.voiceSweep` job handler (idempotent): checkpoints sessions older than 10 minutes and starts missing ones,
 * then re-seeds itself while anyone is in voice.
 * @param {object} job
 * @param {{ guild: object }} jobCtx
 * @returns {Promise<{ nextRunAt: Date }|undefined>}
 */
export async function runVoiceSweep(job, { guild }) {
  const settings = await getSettingsSafe(guild.id);
  // A null row means the database was unreachable: fail so the scheduler retries instead of dropping the job.
  if (!settings) throw new Error(`settings of guild ${guild.id} unavailable`);
  if (!isModuleEnabled(settings, 'leveling') || !settings.voiceXpEnabled) return undefined;
  let humans = 0;
  for (const state of guild.voiceStates?.cache?.values() ?? []) {
    if (humans >= VOICE_SEED_CAP) break;
    if (!state.channelId || !state.member || state.member.user?.bot) continue;
    humans++;
    await reconcileVoiceMember({ guild, member: state.member, state, settings });
  }
  if (humans === 0) return undefined; // the next join re-seeds
  return { nextRunAt: new Date(Date.now() + VOICE_CHECKPOINT_MS) };
}

// ── roleDelete cleanup ────────────────────────────────────────────────────────

/**
 * roleDelete hook (always runs, even while the module is off): drops the role's reward and its no-XP / boost entries.
 * @param {object} ctx HookContext
 */
export async function handleRoleDelete(ctx) {
  const { guild, role } = ctx;
  const n = await prisma.levelRole.deleteMany({ where: { guildId: guild.id, roleId: role.id } });
  const s = await ctx.getSettings();
  if (s && ((s.noXpRoleIds ?? []).includes(role.id) || normalizeBoostRoles(s.xpBoostRoles).some((b) => b.roleId === role.id))) {
    await updateSettingsWith(guild.id, (cur) => {
      const out = {};
      if ((cur.noXpRoleIds ?? []).includes(role.id)) out.noXpRoleIds = cur.noXpRoleIds.filter((x) => x !== role.id);
      const boosts = normalizeBoostRoles(cur.xpBoostRoles);
      if (boosts.some((x) => x.roleId === role.id)) out.xpBoostRoles = boosts.filter((x) => x.roleId !== role.id);
      return out;
    });
  }
  if (n.count) logger.info(`levels: removed ${n.count} reward(s) for deleted role ${role.id} in ${guild.id}`);
}

// ── Bulk tasks ────────────────────────────────────────────────────────────────

/** @returns {boolean} true when this call took the guild's bulk lock */
export function tryStartBulk(guildId) {
  if (bulkRunning.has(guildId)) return false;
  bulkRunning.add(guildId);
  return true;
}

export function endBulk(guildId) {
  bulkRunning.delete(guildId);
}

export function isBulkRunning(guildId) {
  return bulkRunning.has(guildId);
}

/**
 * Re-syncs reward roles for a list of ranked members, sequentially. Never throws.
 * @param {{ guild: object, entries: Array<{ userId: string, level: number }>, settings: object, syncDown: boolean }} opts
 * @returns {Promise<{ processed: number, changed: number, failed: number, missing: number }>}
 */
export async function runBulkRoleSync({ guild, entries, settings, syncDown }) {
  const out = { processed: 0, changed: 0, failed: 0, missing: 0 };
  try {
    const rewards = await prisma.levelRole.findMany({ where: { guildId: guild.id } });
    if (!rewards.length) return out;
    for (let i = 0; i < entries.length; i += 100) {
      const chunk = entries.slice(i, i + 100);
      await withTimeout(guild.members.fetch({ user: chunk.map((e) => e.userId) }), 10_000, 'levels bulk fetch').catch(() => null);
      for (const entry of chunk) {
        const m = guild.members.cache.get(entry.userId);
        if (!m) { out.missing++; continue; }
        if (m.user?.bot) continue;
        const r = await syncLevelRoles(m, entry.level, settings, { syncDown, rewards });
        if (r.added.length || r.removed.length) out.changed++;
        if (r.failed.length) out.failed++;
        out.processed++;
      }
    }
  } catch (err) {
    logger.error(`levels: bulk role sync failed in ${guild?.id}`, asError(err));
  }
  return out;
}

/**
 * Reports a finished background task to the invoker: a follow-up while the interaction token lives, else the channel,
 * else the log. `ephemeral` should match the command's own visibility. Every branch is caught.
 */
export async function reportBulkResult(interaction, text, { ephemeral = interaction?.ephemeral === true } = {}) {
  try {
    const embeds = [infoEmbed('Role Sync Finished', text)];
    if (Date.now() - interaction.createdTimestamp < 14 * 60_000) {
      await interaction.followUp({ embeds, ephemeral });
      return;
    }
    if (checkChannelUsable(interaction.channel).ok) {
      await interaction.channel.send({ embeds, allowedMentions: { parse: [] } });
      return;
    }
    logger.info(text);
  } catch (err) {
    if (isInteractionGone(err)) logger.debug(`levels: bulk report skipped, interaction gone (${err?.code})`);
    else logger.warn(`levels: could not report a bulk result: ${err?.message ?? err}`);
  }
}

// ── Rewards provider (the `xp` kind on the rewards bus) ──────────────────────

export const xpRewardProvider = Object.freeze({
  /** @returns {Promise<{ ok: boolean, total?: number, reason?: string }>} */
  async grant({ guild, userId, amount, member = null, channel = null }) {
    try {
      const settings = await getSettingsSafe(guild.id);
      if (!isModuleEnabled(settings, 'leveling')) return { ok: false, reason: 'module-disabled' };
      const who = member ?? await guild.members.fetch(userId).catch(() => null);
      if (!who) return { ok: false, reason: 'not-member' };
      if (who.user?.bot) return { ok: false, reason: 'bot' };
      const r = await applyXpDelta({ guildId: guild.id, userId, delta: amount });
      await handleLevelChange({ guild, member: who, userId, result: r, settings, source: 'reward', channel, announce: true });
      return { ok: true, total: r.xp };
    } catch (err) {
      logger.error('levels: xp reward grant failed', asError(err));
      return { ok: false, reason: 'error' };
    }
  },

  /** XP clamps at 0: taking more than the member has removes everything they have (unlike coins). */
  async take({ guild, userId, amount, member = null }) {
    try {
      const settings = await getSettingsSafe(guild.id);
      if (!isModuleEnabled(settings, 'leveling')) return { ok: false, reason: 'module-disabled' };
      const r = await subtractXp({ guildId: guild.id, userId, amount });
      if (!r || r.oldXp === 0) return { ok: false, reason: 'insufficient' };
      const who = member ?? await guild.members.fetch(userId).catch(() => null);
      if (r.newLevel < r.oldLevel) {
        await handleLevelChange({ guild, member: who, userId, result: r, settings, source: 'reward', announce: false });
      }
      return { ok: true, total: r.xp };
    } catch (err) {
      logger.error('levels: xp reward take failed', asError(err));
      return { ok: false, reason: 'error' };
    }
  },
});

// ── Deprecated entry points ───────────────────────────────────────────────────
// XP now runs through the levels plugin. These remain only so a stale event-file import can never crash a shard or
// double-grant.

/** @deprecated no-op */
export async function grantXp() {
  if (!deprecatedWarned.has('grantXp')) {
    deprecatedWarned.add('grantXp');
    logger.warn('xpService.grantXp/handleVoiceXp is deprecated; XP runs via the levels plugin');
  }
}

/** @deprecated no-op */
export async function handleVoiceXp() {
  if (!deprecatedWarned.has('handleVoiceXp')) {
    deprecatedWarned.add('handleVoiceXp');
    logger.warn('xpService.grantXp/handleVoiceXp is deprecated; XP runs via the levels plugin');
  }
}
