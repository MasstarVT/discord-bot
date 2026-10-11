// ── Capped full-member fetch ───────────────────────────────────────────────────
// One full member fetch (gateway opcode 8) per guild at a time, a size cap, a timeout and
// gateway rate-limit memory. Never throws. Imports only logger: the timeout is local because
// async.js belongs to foundation.
import logger from './logger.js';

export const FETCH_ALL_DEFAULT_TIMEOUT_MS = 120_000;
export const FETCH_ALL_MAX_MEMBERS = 100_000;

const MAX_RETRY_MS = 10 * 60_000;

/** @type {Map<string, Promise<{ ok: true, members: any } | { ok: false, reason: string, retryAfterMs?: number }>>} */
const inFlight = new Map();
/** @type {Map<string, number>} guild id → time before which the gateway is not asked again */
const retryUntil = new Map();

// Turns the fetch promise into one that never rejects.
function settle(guild) {
  return Promise.resolve()
    .then(() => guild.members.fetch())
    .then((members) => ({ ok: true, members }))
    .catch((err) => {
      // GatewayRateLimitError, or any rejection that carries data.retry_after (seconds).
      const retrySeconds = Number(err?.data?.retry_after);
      if (err?.name === 'GatewayRateLimitError' || Number.isFinite(retrySeconds)) {
        const ms = Math.min(MAX_RETRY_MS, Math.max(1_000, Math.ceil((Number.isFinite(retrySeconds) ? retrySeconds : 30) * 1000)));
        retryUntil.set(guild.id, Date.now() + ms);
        return { ok: false, reason: 'rate-limited', retryAfterMs: ms };
      }
      logger.warn(`fetchAllMembers failed for guild ${guild.id}: ${err?.message}`);
      return { ok: false, reason: 'error' };
    });
}

/**
 * Fetches every member of a guild, safely. Concurrent callers share one in-flight fetch.
 * @param {import('discord.js').Guild} guild
 * @param {{ timeoutMs?: number, maxMembers?: number }} [opts]
 * @returns {Promise<{ ok: true, members: import('discord.js').Collection<string, import('discord.js').GuildMember> }
 *   | { ok: false, reason: 'too-large' | 'rate-limited' | 'timeout' | 'error', retryAfterMs?: number }>}
 */
export async function fetchAllMembers(guild, {
  timeoutMs = FETCH_ALL_DEFAULT_TIMEOUT_MS, maxMembers = FETCH_ALL_MAX_MEMBERS,
} = {}) {
  let timer = null;
  try {
    if (!guild?.id || typeof guild.members?.fetch !== 'function') return { ok: false, reason: 'error' };
    if (Number(guild.memberCount ?? 0) > maxMembers) return { ok: false, reason: 'too-large' };

    const until = retryUntil.get(guild.id);
    if (until !== undefined) {
      if (until > Date.now()) return { ok: false, reason: 'rate-limited', retryAfterMs: until - Date.now() };
      retryUntil.delete(guild.id);
    }

    let pending = inFlight.get(guild.id);
    if (!pending) {
      pending = settle(guild).finally(() => {
        if (inFlight.get(guild.id) === pending) inFlight.delete(guild.id);
      });
      inFlight.set(guild.id, pending);
    }

    const timeout = new Promise((resolve) => {
      timer = setTimeout(() => resolve({ ok: false, reason: 'timeout' }), timeoutMs);
      timer.unref?.();
    });
    return await Promise.race([pending, timeout]);
  } catch (err) {
    logger.warn(`fetchAllMembers failed: ${err?.message}`);
    return { ok: false, reason: 'error' };
  } finally {
    if (timer) clearTimeout(timer);
  }
}
