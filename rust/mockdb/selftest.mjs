// Checks the mock servers with the JS bot's own cache and settings code. Usage (from the repo root):
//   eval "$(node rust/mockdb/mockdb.mjs db selftest)" && node rust/mockdb/selftest.mjs
import assert from 'node:assert/strict';
process.env.NODE_ENV = 'test';
process.env.REDIS_URL = process.env.TEST_REDIS_URL;
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL;
const { default: redis, cache } = await import('../../src/services/redis.js');
const { default: prisma } = await import('../../src/database/client.js');
const settings = await import('../../src/services/settingsService.js');
await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));
await redis.flushdb();

assert.equal(await cache.set('t:a', { x: 1, when: new Date(0) }, 60), true);
assert.deepEqual(await cache.get('t:a'), { x: 1, when: '1970-01-01T00:00:00.000Z' });
assert.ok((await redis.ttl('t:a')) > 55);
assert.equal(await cache.setNX('t:nx', 1, 30), true);
assert.equal(await cache.setNX('t:nx', 1, 30), false);
assert.equal(await cache.incr('t:n', 30), 1);
assert.equal(await cache.incr('t:n', 30), 2);
assert.ok((await redis.ttl('t:n')) > 25);
assert.equal(await cache.slidingWindow('t:w', 5000), 1);
assert.equal(await cache.slidingWindow('t:w', 5000), 2);
assert.deepEqual(await cache.pushRecent('t:l', 'a', 2, 30), ['a']);
assert.deepEqual(await cache.pushRecent('t:l', 'b', 2, 30), ['b', 'a']);
assert.deepEqual(await cache.pushRecent('t:l', 'c', 2, 30), ['c', 'b']);
assert.equal(await cache.hset('t:h', 'f', { y: 2 }), true);
assert.deepEqual(await cache.hget('t:h', 'f'), { y: 2 });
await cache.set('guild:42:x', 1, 60); await cache.set('xp:cooldown:42:7', 1, 60); await cache.set('guild:43:x', 1, 60);
assert.equal(await cache.invalidateAny('*:42:*', ['guild:42:*', 'xp:cooldown:42:*']), 2);
assert.equal(await cache.get('guild:43:x'), 1);

// Settings: Postgres row -> Redis copy, written by the JS bot's own service.
const row = await settings.getSettings('900000000000000002', { fresh: true });
assert.equal(row.guildId, '900000000000000002');
const cached = JSON.parse(await redis.get('guild:900000000000000002:settings'));
assert.equal(cached._cacheV, settings.SETTINGS_CACHE_VERSION);
const created = await settings.getSettings('123456789012345678');
assert.equal(created.guildId, '123456789012345678');
process.stdout.write(`selftest ok: cache API, settings cache (${Object.keys(cached).length} fields), ${await prisma.guildSettings.count()} GuildSettings rows\n`);
redis.disconnect(); await prisma.$disconnect(); process.exit(0);
