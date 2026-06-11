import Redis from 'ioredis';
import logger from '../utils/logger.js';

const client = new Redis(process.env.REDIS_URL || 'redis://localhost:6379', {
  retryStrategy: (times) => Math.min(times * 100, 3_000),
  enableReadyCheck: true,
  maxRetriesPerRequest: 3,
  lazyConnect: false,
});

client.on('connect', () => logger.info('Redis connected'));
client.on('ready',   () => logger.success('Redis ready'));
client.on('error',   (err) => logger.error('Redis error', err));
client.on('close',   () => logger.warn('Redis connection closed'));
client.on('reconnecting', (ms) => logger.info(`Redis reconnecting in ${ms}ms`));

// ── Helper wrapper ────────────────────────────────────────────────────────────

export const cache = {
  /** @returns {Promise<any|null>} Parsed JSON value or null */
  async get(key) {
    const raw = await client.get(key);
    if (raw === null) return null;
    try { return JSON.parse(raw); } catch { return raw; }
  },

  /** @param {string} key @param {any} value @param {number} [ttl] seconds */
  async set(key, value, ttl) {
    const serialised = JSON.stringify(value);
    if (ttl) return client.setex(key, ttl, serialised);
    return client.set(key, serialised);
  },

  async del(key) {
    return client.del(key);
  },

  async hget(hash, field) {
    const raw = await client.hget(hash, field);
    if (raw === null) return null;
    try { return JSON.parse(raw); } catch { return raw; }
  },

  async hset(hash, field, value) {
    return client.hset(hash, field, JSON.stringify(value));
  },

  async hdel(hash, field) {
    return client.hdel(hash, field);
  },

  /**
   * Deletes all keys matching a glob pattern using SCAN (never KEYS).
   * @param {string} pattern e.g. "guild:*:settings"
   */
  async invalidate(pattern) {
    let cursor = '0';
    do {
      const [nextCursor, keys] = await client.scan(cursor, 'MATCH', pattern, 'COUNT', 100);
      cursor = nextCursor;
      if (keys.length > 0) await client.del(...keys);
    } while (cursor !== '0');
  },
};

export default client;
