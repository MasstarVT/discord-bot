// Personal timezones (/timezone set): one UserTimezone row per user, global across servers.
// Reads go memo (per shard, 30 s) → Redis (1 h, misses cached too) → Postgres, so /timestamp and its
// autocomplete rarely touch the database. Writes go to Postgres first, then refresh the caches.
import prisma from '../database/client.js';
import { cache } from './redis.js';
import { isValidTimeZone } from '../utils/time.js';
import { resolveTimezone } from '../utils/timezones.js';
import logger from '../utils/logger.js';

export const MEMO_TTL_MS = 30_000;
export const MEMO_MAX = 5000;
export const CACHE_TTL_S = 3600;
export const userTimezoneKey = (userId) => `usertz:${userId}`;

/** @type {Map<string, { zone: string|null, at: number }>} */
const memo = new Map();

function remember(userId, zone) {
  if (memo.size >= MEMO_MAX && !memo.has(userId)) memo.delete(memo.keys().next().value);
  memo.set(userId, { zone, at: Date.now() });
}

/**
 * The memoised zone, with no I/O.
 * @param {string} userId
 * @returns {string|null|undefined} undefined when not memoised (or expired)
 */
export function peekUserTimezone(userId) {
  const hit = memo.get(userId);
  if (!hit) return undefined;
  if (Date.now() - hit.at > MEMO_TTL_MS) {
    memo.delete(userId);
    return undefined;
  }
  return hit.zone;
}

/**
 * A user's own zone. Never throws.
 * @param {string} userId
 * @param {{ cacheOnly?: boolean }} [opts] - cacheOnly: memo and Redis only (autocomplete)
 * @returns {Promise<string|null|undefined>} null = none saved; undefined = unknown (lookup failed, or a
 *   cacheOnly miss)
 */
export async function getUserTimezone(userId, { cacheOnly = false } = {}) {
  const peeked = peekUserTimezone(userId);
  if (peeked !== undefined) return peeked;
  const cached = await cache.get(userTimezoneKey(userId));
  if (cached && typeof cached === 'object' && 'zone' in cached) {
    const zone = isValidTimeZone(cached.zone) ? cached.zone : null;
    remember(userId, zone);
    return zone;
  }
  if (cacheOnly) return undefined;
  try {
    const row = await prisma.userTimezone.findUnique({ where: { userId }, select: { zone: true } });
    const zone = row && isValidTimeZone(row.zone) ? row.zone : null;
    remember(userId, zone);
    await cache.set(userTimezoneKey(userId), { zone }, CACHE_TTL_S);
    return zone;
  } catch (err) {
    logger.warn(`[timezone] lookup failed for ${userId}: ${err?.message}`);
    return undefined;
  }
}

/**
 * Saves a user's own zone (already validated). Throws when Postgres fails.
 * @param {string} userId
 * @param {string} zone
 */
export async function setUserTimezone(userId, zone) {
  await prisma.userTimezone.upsert({ where: { userId }, create: { userId, zone }, update: { zone } });
  remember(userId, zone);
  await cache.set(userTimezoneKey(userId), { zone }, CACHE_TTL_S);
}

/**
 * Removes a user's own zone. Throws when Postgres fails.
 * @param {string} userId
 * @returns {Promise<boolean>} true when one was saved
 */
export async function clearUserTimezone(userId) {
  const { count } = await prisma.userTimezone.deleteMany({ where: { userId } });
  remember(userId, null);
  await cache.set(userTimezoneKey(userId), { zone: null }, CACHE_TTL_S);
  return count > 0;
}

/**
 * The ids of the roles a member holds: a cached GuildMember or the raw interaction member.
 * @param {any} member
 * @returns {string[]}
 */
export function memberRoleIds(member) {
  if (member?.roles?.cache) return [...member.roles.cache.keys()];
  return Array.isArray(member?.roles) ? member.roles : [];
}

/**
 * resolveTimezone for an interaction's member. `personal` is what getUserTimezone returned.
 * @param {{ member?: any, guild?: any, settings?: object|null, personal?: string|null }} input
 */
export function resolveMemberTimezone({ member = null, guild = null, settings = null, personal = null }) {
  return resolveTimezone({
    personal,
    memberRoleIds: memberRoleIds(member),
    rolePosition: (id) => guild?.roles?.cache?.get(id)?.position ?? -1,
    settings,
  });
}

/** Test hook: forget the memo. */
export function __resetUserTimezoneMemo() {
  memo.clear();
}
