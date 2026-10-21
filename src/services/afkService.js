// AFK: members mark themselves away (/afk set), whoever mentions them is told, and the next message they write ends it.
// State is one AfkStatus row per (guild, member); GuildSettings.afkNicknamePrefix and afkIgnoredChannelIds configure it.
// The message handling is in src/plugins/afk.js, /afk in src/commands/community/afk.js. This file holds the rules:
// the per-guild set of AFK member ids (so chat from everyone else never queries Postgres), the row writes, the
// "[AFK] " nickname prefix and the text clean-up for reasons (member text: never pings, one line, length-capped).
import prisma from '../database/client.js';
import { LIMITS } from '../config/constants.js';
import { ensureGuildSettings } from './settingsService.js';
import { createKeyedMutex, createThrottle } from '../utils/async.js';
import { missingBotPermissions } from '../utils/interactionHelpers.js';
import { formatDuration } from '../utils/parseDuration.js';
import { clampText } from '../utils/text.js';
import logger from '../utils/logger.js';

export const AFK = Object.freeze({
  EMOJI: '💤',
  PREFIX: '[AFK] ',
  NICKNAME_MAX: 32,
  // A message this soon after /afk set does not end the AFK (it was typed before, or is the command's own echo).
  GRACE_MS: 10_000,
  // The "Welcome back" reply deletes itself after this long.
  WELCOME_DELETE_MS: 10_000,
  // One "X is AFK" reply per channel and AFK member this often.
  NOTICE_COOLDOWN_MS: 60_000,
  // AFK members announced per message, and ping links shown on return.
  MAX_NOTICES: 5,
  MAX_LINKS: 5,
  // /afk list shows this many members.
  LIST_MAX: 25,
  // How long the per-guild set of AFK ids is trusted; a failed load is retried after the short one.
  IDS_TTL_MS: 120_000,
  IDS_FAILED_TTL_MS: 15_000,
  IDS_MEMO_MAX: 5_000,
});

// Unknown channel, message or member, missing access or permission: the target is gone, nothing to retry.
export const GONE_CODES = new Set([10003, 10007, 10008, 50001, 50013]);

const notifyThrottle = createThrottle(AFK.NOTICE_COOLDOWN_MS);
const pingLock = createKeyedMutex();
const warnLoad = createThrottle(60_000);

/**
 * Whether a notice about `userId` may be posted in `channelId` now (at most once a minute per pair). Consumes the slot.
 * @param {string} channelId
 * @param {string} userId
 * @returns {boolean}
 */
export function mayNotify(channelId, userId) {
  return notifyThrottle(`${channelId}:${userId}`);
}

// ── Text ──────────────────────────────────────────────────────────────────────

/**
 * The reason as stored: one line (no newlines), trimmed, at most LIMITS.AFK_REASON_LENGTH characters; null when empty.
 * @param {unknown} raw
 * @returns {string|null}
 */
export function cleanReason(raw) {
  if (typeof raw !== 'string') return null;
  const text = raw.replace(/\s+/g, ' ').trim();
  return text ? clampText(text, LIMITS.AFK_REASON_LENGTH) : null;
}

/**
 * Member text made safe to repeat: mention syntax (@everyone, @here, <@id>, <@&id>, <#id>) is broken with a zero-width
 * space so it cannot highlight like a ping, and the result is cut to `max` characters. Messages that carry it also send
 * `allowedMentions: { parse: [] }`, which is what actually prevents pings.
 * @param {unknown} raw
 * @param {number} [max]
 * @returns {string}
 */
export function plainText(raw, max = LIMITS.AFK_REASON_LENGTH) {
  const text = clampText(String(raw ?? '').replace(/\s+/g, ' ').trim(), max);
  return text.replace(/@(everyone|here)/gi, '@​$1').replace(/<(?=[@#])/g, '<​');
}

/** A display name without the "[AFK] " prefix the bot added. */
export function nameWithoutPrefix(name) {
  const s = String(name ?? '');
  return s.startsWith(AFK.PREFIX) ? s.slice(AFK.PREFIX.length) : s;
}

/**
 * How long a member was AFK, in at most two units ("2h 5m").
 * @param {number} ms
 * @returns {string}
 */
export function afkLength(ms) {
  if (!(ms >= 1000)) return '0s'; // formatDuration reads 0 as "Permanent"
  return formatDuration(ms).split(' ').slice(0, 2).join(' ');
}

/**
 * The saved pings that can be shown: the entries of the Json column that have a channel and message id, oldest first.
 * @param {unknown} pings
 * @returns {{ channelId: string, messageId: string, authorId: string|null, at: number }[]}
 */
export function readPings(pings) {
  if (!Array.isArray(pings)) return [];
  const out = [];
  for (const p of pings) {
    if (p && typeof p.channelId === 'string' && typeof p.messageId === 'string') {
      out.push({ channelId: p.channelId, messageId: p.messageId, authorId: typeof p.authorId === 'string' ? p.authorId : null, at: Number(p.at) || 0 });
    }
  }
  return out;
}

/** Jump link to a message. */
export const jumpLink = (guildId, channelId, messageId) => `https://discord.com/channels/${guildId}/${channelId}/${messageId}`;

/**
 * Whether the channel, its parent (a thread's channel or a channel's category) or that parent's category is on
 * GuildSettings.afkIgnoredChannelIds: AFK is neither cleared nor announced there.
 * @param {{ afkIgnoredChannelIds?: string[] }|null|undefined} settings
 * @param {{ id?: string, parentId?: string|null, parent?: { parentId?: string|null }|null }|null|undefined} channel
 * @returns {boolean}
 */
export function isIgnoredChannel(settings, channel) {
  const list = settings?.afkIgnoredChannelIds;
  if (!channel || !Array.isArray(list) || list.length === 0) return false;
  return [channel.id, channel.parentId, channel.parent?.parentId].some((id) => id != null && list.includes(id));
}

// ── Nickname prefix ───────────────────────────────────────────────────────────

/**
 * The nickname with "[AFK] " in front, cut so it fits Discord's 32 characters; null when the name already has it.
 * The cut is only for as long as the member is AFK: AfkStatus.previousNick keeps the full nickname for the way back.
 * @param {string} displayName
 * @returns {string|null}
 */
export function prefixedNickname(displayName) {
  const name = String(displayName ?? '');
  if (name.startsWith(AFK.PREFIX)) return null;
  return AFK.PREFIX + clampText(name, AFK.NICKNAME_MAX - AFK.PREFIX.length, '');
}

/**
 * The nickname to set when the prefix comes off: exactly what the member had before it (`previousNick`; null = none,
 * which clears the nickname). `undefined` when the nickname no longer starts with the prefix (the member renamed
 * themselves: leave it).
 * @param {string|null|undefined} nickname the current nickname
 * @param {string|null|undefined} previousNick AfkStatus.previousNick
 * @returns {string|null|undefined}
 */
export function restoredNickname(nickname, previousNick) {
  if (typeof nickname !== 'string' || !nickname.startsWith(AFK.PREFIX)) return undefined;
  return typeof previousNick === 'string' && previousNick ? previousNick : null;
}

/** Whether the bot may rename this member (Manage Nicknames, a role above theirs, not the owner). Never throws. */
function canRename(member) {
  try {
    return missingBotPermissions(member.guild, 'ManageNicknames').length === 0 && member.manageable !== false;
  } catch {
    return false;
  }
}

/**
 * Puts "[AFK] " in front of the member's name when the setting is on and the bot can.
 * @param {import('discord.js').GuildMember} member
 * @param {{ afkNicknamePrefix?: boolean }|null} settings
 * @returns {Promise<{ changed: boolean, problem: 'cannot'|'failed'|null, previousNick: string|null }>}
 * changed: the bot set the nickname (so it must take it off again); previousNick: the nickname before the prefix, null
 * when there was none (to give back by removeNicknamePrefix)
 */
export async function addNicknamePrefix(member, settings) {
  if (!settings?.afkNicknamePrefix || !member) return { changed: false, problem: null, previousNick: null };
  const next = prefixedNickname(member.displayName);
  if (next === null) return { changed: false, problem: null, previousNick: null };
  if (!canRename(member)) return { changed: false, problem: 'cannot', previousNick: null };
  const previousNick = typeof member.nickname === 'string' && member.nickname ? member.nickname : null;
  try {
    await member.setNickname(next, 'Went AFK');
    return { changed: true, problem: null, previousNick };
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.warn(`[afk] ${member.guild?.id}: could not add the prefix: ${err?.message}`);
    return { changed: false, problem: 'failed', previousNick: null };
  }
}

/**
 * Takes "[AFK] " off the member's nickname and gives back the one they had, only while the nickname still starts with
 * the prefix. Never throws.
 * @param {import('discord.js').GuildMember|null|undefined} member
 * @param {string|null} [previousNick] AfkStatus.previousNick (null = the member had no nickname)
 * @returns {Promise<boolean>} whether the nickname was changed
 */
export async function removeNicknamePrefix(member, previousNick = null) {
  if (!member) return false;
  const next = restoredNickname(member.nickname, previousNick);
  if (next === undefined || !canRename(member)) return false;
  try {
    await member.setNickname(next, 'No longer AFK');
    return true;
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.warn(`[afk] ${member.guild?.id}: could not remove the prefix: ${err?.message}`);
    return false;
  }
}

// ── The per-guild set of AFK ids ──────────────────────────────────────────────

/** @type {Map<string, { ids: Set<string>, expires: number }>} oldest first */
const idMemo = new Map();
/** @type {Map<string, Promise<Set<string>>>} one load per guild at a time */
const idLoads = new Map();
// Bumped by every set/clear so a load that started before one does not memoise a set that misses it.
let writes = 0;

function memoPut(guildId, ids, ttlMs) {
  idMemo.delete(guildId);
  idMemo.set(guildId, { ids, expires: Date.now() + ttlMs });
  while (idMemo.size > AFK.IDS_MEMO_MAX) idMemo.delete(idMemo.keys().next().value);
}

/**
 * The memoised set of AFK member ids of a guild, no I/O: a Set (maybe empty), or null when it is not known.
 * Sync hook filters use it, so chat from members who are not AFK and mention nobody who is never waits on Postgres.
 * @param {string} guildId
 * @returns {Set<string>|null}
 */
export function peekAfkIds(guildId) {
  const entry = idMemo.get(guildId);
  if (!entry) return null;
  if (entry.expires <= Date.now()) {
    idMemo.delete(guildId);
    return null;
  }
  return entry.ids;
}

/**
 * The AFK member ids of a guild: the memo, else one query (shared by concurrent callers). A failed query reads as
 * "nobody is AFK" for a few seconds, so a database outage does not turn into a query per message.
 * @param {string} guildId
 * @returns {Promise<Set<string>>}
 */
export function loadAfkIds(guildId) {
  const hit = peekAfkIds(guildId);
  if (hit) return Promise.resolve(hit);
  const pending = idLoads.get(guildId);
  if (pending) return pending;
  const startedAt = writes;
  const load = (async () => {
    try {
      const rows = await prisma.afkStatus.findMany({ where: { guildId }, select: { userId: true } });
      const ids = new Set(rows.map((r) => r.userId));
      if (writes === startedAt) memoPut(guildId, ids, AFK.IDS_TTL_MS);
      return ids;
    } catch (err) {
      if (warnLoad(guildId)) logger.warn(`[afk] ${guildId}: could not load the AFK members: ${err?.message ?? err}`);
      const ids = new Set();
      memoPut(guildId, ids, AFK.IDS_FAILED_TTL_MS);
      return ids;
    } finally {
      idLoads.delete(guildId);
    }
  })();
  idLoads.set(guildId, load);
  return load;
}

/** Keeps the memoised set in step with a write made here (a guild lives on one shard, so this process is the only writer). */
function noteAfk(guildId, userId, afk) {
  writes += 1;
  const ids = peekAfkIds(guildId);
  if (!ids) return;
  const next = new Set(ids);
  if (afk) next.add(userId);
  else next.delete(userId);
  idMemo.set(guildId, { ids: next, expires: idMemo.get(guildId).expires });
}

/** Removes a member from the memoised set (the memo said AFK but the row is gone). */
export function forgetAfkMember(guildId, userId) {
  noteAfk(guildId, userId, false);
}

/** Drops a guild's memo (tests, and a guild the bot left). */
export function forgetAfkIds(guildId) {
  idMemo.delete(guildId);
}

// ── Rows ──────────────────────────────────────────────────────────────────────

/** The member's AfkStatus row, or null. */
export function loadStatus(guildId, userId) {
  return prisma.afkStatus.findUnique({ where: { guildId_userId: { guildId, userId } } });
}

/** The AfkStatus rows of these members, in the order of `userIds`. */
export async function loadStatuses(guildId, userIds) {
  if (!userIds.length) return [];
  const rows = await prisma.afkStatus.findMany({ where: { guildId, userId: { in: userIds } } });
  const byId = new Map(rows.map((r) => [r.userId, r]));
  return userIds.map((id) => byId.get(id)).filter(Boolean);
}

/**
 * Marks a member AFK, or changes the reason of one who already is (the start time and saved pings stay).
 * @param {{ guildId: string, userId: string, reason: string|null, nickChanged?: boolean, previousNick?: string|null }} args
 * nickChanged: the bot just added the nickname prefix (an existing true is never reset); previousNick: the nickname
 * before it (null = none), saved only along with a new prefix, so a second set that adds none keeps the original
 */
export async function setAfk({ guildId, userId, reason, nickChanged = false, previousNick = null }) {
  await ensureGuildSettings(guildId);
  const row = await prisma.afkStatus.upsert({
    where: { guildId_userId: { guildId, userId } },
    create: { guildId, userId, reason, nickChanged, previousNick: nickChanged ? previousNick : null },
    update: nickChanged ? { reason, nickChanged: true, previousNick } : { reason },
  });
  noteAfk(guildId, userId, true);
  return row;
}

/**
 * Ends a member's AFK. Returns the row that was deleted (for the length, the pings and nickChanged), or null when
 * there was none: of two callers racing (a message and /afk clear) only one gets the row.
 * @param {string} guildId
 * @param {string} userId
 */
export async function clearAfk(guildId, userId) {
  const row = await loadStatus(guildId, userId);
  if (!row) {
    noteAfk(guildId, userId, false);
    return null;
  }
  const { count } = await prisma.afkStatus.deleteMany({ where: { guildId, userId } });
  noteAfk(guildId, userId, false);
  return count > 0 ? row : null;
}

/**
 * Records a mention of an AFK member: keeps the last LIMITS.AFK_PINGS_KEPT and counts all. Never throws.
 * @param {string} guildId
 * @param {string} userId the AFK member
 * @param {{ channelId: string, messageId: string, authorId: string }} ping
 */
export function recordPing(guildId, userId, ping) {
  return pingLock.run(`${guildId}:${userId}`, async () => {
    try {
      const row = await loadStatus(guildId, userId);
      if (!row) return;
      const kept = [...readPings(row.pings), { ...ping, at: Date.now() }].slice(-LIMITS.AFK_PINGS_KEPT);
      await prisma.afkStatus.update({
        where: { guildId_userId: { guildId, userId } },
        data: { pings: kept, pingCount: { increment: 1 } },
      });
    } catch (err) {
      logger.warn(`[afk] ${guildId}: could not record a ping: ${err?.message}`);
    }
  });
}

/**
 * The longest-AFK members first, for /afk list.
 * @param {string} guildId
 * @returns {Promise<{ rows: object[], total: number }>}
 */
export async function listStatuses(guildId) {
  const [rows, total] = await Promise.all([
    prisma.afkStatus.findMany({ where: { guildId }, orderBy: { since: 'asc' }, take: AFK.LIST_MAX }),
    prisma.afkStatus.count({ where: { guildId } }),
  ]);
  return { rows, total };
}
