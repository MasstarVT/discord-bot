import { randomUUID } from 'node:crypto';
import Redis from 'ioredis';
import logger from '../utils/logger.js';

const client = new Redis(process.env.REDIS_URL || 'redis://localhost:6379', {
  retryStrategy: (times) => Math.min(times * 200, 5_000),
  enableReadyCheck: true,
  enableOfflineQueue: false,   // fail fast while disconnected instead of queueing for seconds (R045/R125/R165)
  maxRetriesPerRequest: 1,
  commandTimeout: 750,
  connectTimeout: 5_000,
  lazyConnect: false,
});

// Log lines for a flapping connection are rate-limited so an outage does not flood the log.
const LOG_INTERVAL_MS = 30_000;
let lastErrorLogAt = 0;
let lastUnavailableLogAt = 0;

client.on('connect', () => logger.info('Redis connected'));
client.on('ready',   () => logger.success('Redis ready'));
client.on('error',   (err) => {
  const now = Date.now();
  if (now - lastErrorLogAt < LOG_INTERVAL_MS) return;
  lastErrorLogAt = now;
  logger.error('Redis error', err);
});
client.on('close',   () => logger.warn('Redis connection closed'));
client.on('reconnecting', (ms) => logger.info(`Redis reconnecting in ${ms}ms`));

/** Logs at most one "Redis unavailable" warning per 30 s, whatever the operation. */
function noteFailure(op, err) {
  const now = Date.now();
  if (now - lastUnavailableLogAt < LOG_INTERVAL_MS) return;
  lastUnavailableLogAt = now;
  logger.warn(`Redis unavailable: ${op} (${err?.message ?? err})`);
}

/** MULTI results are [[err, value], ...]; the first error aborts the whole call. */
function multiValues(results) {
  if (!Array.isArray(results)) throw new Error('transaction aborted');
  return results.map(([err, value]) => {
    if (err) throw err;
    return value;
  });
}

function parse(raw) {
  if (raw === null || raw === undefined) return null;
  try { return JSON.parse(raw); } catch { return raw; }
}

// ── Helper wrapper ────────────────────────────────────────────────────────────

/**
 * A cache whose methods never throw or reject (01-foundation §4). On any error they log
 * (rate-limited) and return the documented fallback; the CALLER decides fail-open or
 * fail-closed on a fallback. Settings go through settingsService, not through here.
 */
export const cache = {
  /** @returns {boolean} true when the Redis connection is ready */
  available() {
    return client.status === 'ready';
  },

  /** @returns {Promise<any|null>} Parsed JSON value, the raw string if it is not JSON, or null (miss or error) */
  async get(key) {
    try {
      return parse(await client.get(key));
    } catch (err) {
      noteFailure('get', err);
      return null;
    }
  },

  /**
   * @param {string} key @param {any} value @param {number} [ttl] seconds
   * @returns {Promise<boolean>} true when written
   */
  async set(key, value, ttl) {
    let serialised;
    try {
      serialised = JSON.stringify(value);
    } catch (err) {
      logger.error(`cache.set could not serialise "${key}"`, err);
      return false;
    }
    if (serialised === undefined) {
      logger.error(`cache.set got an unserialisable value for "${key}"`);
      return false;
    }
    try {
      if (ttl) await client.set(key, serialised, 'EX', Math.ceil(ttl));
      else await client.set(key, serialised);
      return true;
    } catch (err) {
      noteFailure('set', err);
      return false;
    }
  },

  /**
   * SET key value EX ttl NX — cooldowns and locks. The caller decides fail-open or
   * fail-closed on `null`.
   * @returns {Promise<boolean|null>} true (set), false (already exists), null (Redis unavailable)
   */
  async setNX(key, value, ttlSeconds) {
    let serialised;
    try {
      serialised = JSON.stringify(value);
    } catch (err) {
      logger.error(`cache.setNX could not serialise "${key}"`, err);
      return null;
    }
    if (serialised === undefined) {
      logger.error(`cache.setNX got an unserialisable value for "${key}"`);
      return null;
    }
    try {
      const reply = ttlSeconds
        ? await client.set(key, serialised, 'EX', Math.ceil(ttlSeconds), 'NX')
        : await client.set(key, serialised, 'NX');
      return reply === 'OK';
    } catch (err) {
      noteFailure('setNX', err);
      return null;
    }
  },

  /** @returns {Promise<boolean>} true when the delete command succeeded */
  async del(...keys) {
    const flat = keys.flat().filter((k) => typeof k === 'string' && k.length > 0);
    if (!flat.length) return true;
    try {
      await client.del(...flat);
      return true;
    } catch (err) {
      noteFailure('del', err);
      return false;
    }
  },

  /** INCR, then EXPIRE when the result is 1. @returns {Promise<number|null>} the new count, or null */
  async incr(key, ttlSeconds) {
    try {
      const n = await client.incr(key);
      if (n === 1 && ttlSeconds) {
        try {
          await client.expire(key, Math.ceil(ttlSeconds));
        } catch (err) {
          // Never leave a counter without a TTL behind: the next INCR would not set one.
          await client.del(key).catch(() => {});
          throw err;
        }
      }
      return n;
    } catch (err) {
      noteFailure('incr', err);
      return null;
    }
  },

  /**
   * Sliding-window counter on a sorted set.
   * @returns {Promise<number|null>} hits in the window including this one, or null
   */
  async slidingWindow(key, windowMs, member = randomUUID()) {
    try {
      const now = Date.now();
      const out = multiValues(await client.multi()
        .zadd(key, now, member)
        .zremrangebyscore(key, 0, now - windowMs)
        .zcard(key)
        .pexpire(key, windowMs)
        .exec());
      return Number(out[2]);
    } catch (err) {
      noteFailure('slidingWindow', err);
      return null;
    }
  },

  /**
   * Push a value onto a capped list (newest first). Values are stored as strings.
   * @returns {Promise<string[]|null>} the list after the push, or null
   */
  async pushRecent(key, value, max, ttlSeconds) {
    try {
      const out = multiValues(await client.multi()
        .lpush(key, String(value))
        .ltrim(key, 0, max - 1)
        .expire(key, Math.ceil(ttlSeconds))
        .lrange(key, 0, max - 1)
        .exec());
      return out[3];
    } catch (err) {
      noteFailure('pushRecent', err);
      return null;
    }
  },

  async hget(hash, field) {
    try {
      return parse(await client.hget(hash, field));
    } catch (err) {
      noteFailure('hget', err);
      return null;
    }
  },

  async hset(hash, field, value) {
    try {
      const serialised = JSON.stringify(value);
      if (serialised === undefined) throw new Error('unserialisable value');
      await client.hset(hash, field, serialised);
      return true;
    } catch (err) {
      noteFailure('hset', err);
      return false;
    }
  },

  async hdel(hash, field) {
    try {
      await client.hdel(hash, field);
      return true;
    } catch (err) {
      noteFailure('hdel', err);
      return false;
    }
  },

  /**
   * Deletes all keys matching a glob pattern using SCAN (never KEYS).
   * @param {string} pattern e.g. "guild:*:settings"
   * @returns {Promise<number>} keys deleted (0 on failure)
   */
  async invalidate(pattern) {
    let deleted = 0;
    try {
      let cursor = '0';
      do {
        const [nextCursor, keys] = await client.scan(cursor, 'MATCH', pattern, 'COUNT', 100);
        cursor = nextCursor;
        if (keys.length > 0) deleted += await client.del(...keys);
      } while (cursor !== '0');
    } catch (err) {
      noteFailure('invalidate', err);
    }
    return deleted;
  },
};

export default client;
