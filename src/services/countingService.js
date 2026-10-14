// The counting game: members count up from 1 in one channel, one number per message and never twice in a row. A wrong
// number (or a second count in a row by the same member) resets the count to 0. State lives in the CountingState table
// (one row per guild); the channel is GuildSettings.countingChannelId. The message handling is in src/plugins/counting.js.
import prisma from '../database/client.js';
import { ensureGuildSettings } from './settingsService.js';
import { createKeyedMutex } from '../utils/async.js';

/** Largest number a message can count (9 digits, so it always fits the 32-bit Int column). */
export const COUNT_MAX = 999_999_999;

// The whole message must be the number; anything else is chat and is ignored.
const COUNT_RE = /^\d{1,9}$/;

// One guild's counts run one at a time, in the order their messages reached this shard (a guild lives on one shard).
const lock = createKeyedMutex();

/**
 * Runs `fn` while holding the guild's counting lock. Message handling and /counting both use it.
 * @template T
 * @param {string} guildId
 * @param {() => Promise<T>|T} fn
 * @returns {Promise<T>}
 */
export function runLocked(guildId, fn) {
  return lock.run(guildId, fn);
}

/**
 * The number a message counts, or null when it is not a count (chat).
 * @param {unknown} content
 * @returns {number|null}
 */
export function parseCount(content) {
  if (typeof content !== 'string') return null;
  const text = content.trim();
  return COUNT_RE.test(text) ? Number(text) : null;
}

/**
 * The state that applies in `channelId`: a missing row, or one left over from another channel, starts from 0.
 * The best run is per server, so it carries over.
 * @param {{ channelId: string, count: number, lastUserId: string|null, lastMessageId: string|null, best: number }|null} row
 * @param {string} channelId
 */
export function currentState(row, channelId) {
  if (row && row.channelId === channelId) {
    return { count: row.count, lastUserId: row.lastUserId, lastMessageId: row.lastMessageId, best: row.best };
  }
  return { count: 0, lastUserId: null, lastMessageId: null, best: row?.best ?? 0 };
}

/**
 * Whether `userId` posting `number` continues the count.
 * @param {{ count: number, lastUserId: string|null }} state
 * @param {string} userId
 * @param {number} number
 * @returns {{ ok: true, expected: number } | { ok: false, reason: 'wrong'|'twice', expected: number }}
 */
export function judgeCount(state, userId, number) {
  const expected = state.count + 1;
  if (number !== expected) return { ok: false, reason: 'wrong', expected };
  if (state.lastUserId !== null && state.lastUserId === userId) return { ok: false, reason: 'twice', expected };
  return { ok: true, expected };
}

/** The guild's CountingState row, or null. */
export function loadState(guildId) {
  return prisma.countingState.findUnique({ where: { guildId } });
}

/**
 * Writes the guild's counting state (creates the row on first use).
 * @param {string} guildId
 * @param {{ channelId: string, count: number, lastUserId: string|null, lastMessageId: string|null, best: number }} state
 */
export async function saveState(guildId, state) {
  const data = {
    channelId: state.channelId,
    count: state.count,
    lastUserId: state.lastUserId,
    lastMessageId: state.lastMessageId,
    best: state.best,
  };
  await ensureGuildSettings(guildId);
  return prisma.countingState.upsert({ where: { guildId }, create: { guildId, ...data }, update: data });
}
