// The counting game: members count up from 1 in one channel, one number per message and never twice in a row. A number
// can also be written as maths (`2+2`). A wrong number (or a second count in a row by the same member) resets the count
// to 0. State lives in the CountingState table (one row per guild); the channel is GuildSettings.countingChannelId. The
// message handling is in src/plugins/counting.js.
import prisma from '../database/client.js';
import { ensureGuildSettings } from './settingsService.js';
import { createKeyedMutex } from '../utils/async.js';
import { LIMITS } from '../config/constants.js';

/** Largest number a message can count (9 digits, so it always fits the 32-bit Int column). */
export const COUNT_MAX = 999_999_999;

// The whole message must be the number (or a sum, below); anything else is chat and is ignored.
const COUNT_RE = /^\d{1,9}$/;

// Maths a count may be written as: whole ASCII numbers, + - * / % ^ and parentheses. `×` `x` `÷` `−` and Discord's
// escaped `\*` work too. `**` is not a power (`**5**` is bold, so it stays chat).
const MATH_TOKENS = Object.freeze({
  '+': '+', '-': '-', '−': '-', '*': '*', '×': '*', x: '*', X: '*', '/': '/', '÷': '/', '%': '%', '^': '^', '(': '(', ')': ')',
});
// Longer literals are no longer exact as doubles.
const MATH_LITERAL_DIGITS = 15;
// How close to a whole number a result must be to count as it (`1/49*49` is not exactly 1 in floating point).
const MATH_EPSILON = 1e-9;

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

function mathTokens(text) {
  const tokens = [];
  for (let i = 0; i < text.length;) {
    const c = text[i];
    if (c >= '0' && c <= '9') {
      let j = i + 1;
      while (j < text.length && text[j] >= '0' && text[j] <= '9') j += 1;
      if (j - i > MATH_LITERAL_DIGITS) return null;
      tokens.push(Number(text.slice(i, j)));
      i = j;
    } else if (/\s/.test(c)) {
      i += 1;
    } else if (c === '\\' && text[i + 1] === '*') {
      tokens.push('*');
      i += 2;
    } else if (Object.hasOwn(MATH_TOKENS, c)) {
      tokens.push(MATH_TOKENS[c]);
      i += 1;
    } else {
      return null;
    }
  }
  return tokens;
}

/**
 * Works out `text` as maths: the whole number it comes to, or null when it is not maths, does not come to a whole
 * number or is not finite (`1/0`). `^` binds tightest and from the right, and `-2^2` is -4. The caller caps the length,
 * which also bounds the recursion.
 * @param {string} text
 * @returns {number|null}
 */
export function evaluateMath(text) {
  const tokens = mathTokens(text);
  if (!tokens?.length) return null;
  let pos = 0;
  let bad = false; // a syntax error; NaN alone is not enough (NaN ^ 0 is 1)

  function take(t) {
    if (tokens[pos] !== t) return false;
    pos += 1;
    return true;
  }
  function sum() {
    let v = product();
    for (;;) {
      if (take('+')) v += product();
      else if (take('-')) v -= product();
      else return v;
    }
  }
  function product() {
    let v = unary();
    for (;;) {
      if (take('*')) v *= unary();
      else if (take('/')) v /= unary();
      else if (take('%')) v %= unary();
      else return v;
    }
  }
  function unary() {
    return take('-') ? -unary() : power();
  }
  function power() {
    const base = primary();
    return take('^') ? base ** unary() : base;
  }
  function primary() {
    const t = tokens[pos];
    if (typeof t === 'number') {
      pos += 1;
      return t;
    }
    if (take('(')) {
      const v = sum();
      if (take(')')) return v;
    }
    bad = true;
    return NaN;
  }

  const value = sum();
  if (bad || pos !== tokens.length || !Number.isFinite(value)) return null;
  const whole = Math.round(value);
  if (Math.abs(value - whole) > MATH_EPSILON * Math.max(1, Math.abs(whole))) return null;
  return whole || 0; // never -0
}

/**
 * The number a message counts, or null when it is not a count (chat). The message is either just the number or maths
 * that comes to it (`2+2`); maths that comes to a fraction, a negative number or more than COUNT_MAX is chat.
 * @param {unknown} content
 * @returns {number|null}
 */
export function parseCount(content) {
  if (typeof content !== 'string') return null;
  const text = content.trim();
  if (COUNT_RE.test(text)) return Number(text);
  if (text.length > LIMITS.COUNTING_MATH_LENGTH) return null;
  const value = evaluateMath(text);
  return value !== null && value >= 0 && value <= COUNT_MAX ? value : null;
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
