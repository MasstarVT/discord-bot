// Captures what the JS settingsService writes to Redis for a GuildSettings row: the default row (guild
// 900000000000000001, only required fields) and the row with every column filled (900000000000000002), both seeded
// by rust/mockdb/seed.mjs. Writes rust/fixtures/golden/bot-core/settings-cache.json with the raw Redis strings.
// Usage (repository root): eval "$(node rust/mockdb/mockdb.mjs db bot_core_gen)" && node rust/fixtures/gen/bot-core-settings.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'bot-core');
if (!process.env.TEST_DATABASE_URL || !process.env.TEST_REDIS_URL) {
  process.stderr.write('TEST_DATABASE_URL and TEST_REDIS_URL must be set (node rust/mockdb/mockdb.mjs db <name>)\n');
  process.exit(1);
}
process.env.NODE_ENV = 'test';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL;
process.env.REDIS_URL = process.env.TEST_REDIS_URL;

const { default: redis } = await import(pathToFileURL(join(ROOT, 'src', 'services', 'redis.js')).href);
const { default: prisma } = await import(pathToFileURL(join(ROOT, 'src', 'database', 'client.js')).href);
const settings = await import(pathToFileURL(join(ROOT, 'src', 'services', 'settingsService.js')).href);
await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));

const out = { version: settings.SETTINGS_CACHE_VERSION, ttl: settings.SETTINGS_TTL_SECONDS, rows: {} };
for (const [name, guildId] of [['default', '900000000000000001'], ['full', '900000000000000002']]) {
  await redis.del(settings.settingsKey(guildId));
  await settings.invalidateSettings(guildId);
  await settings.getSettings(guildId, { fresh: true });
  const raw = await redis.get(settings.settingsKey(guildId));
  const ttl = await redis.ttl(settings.settingsKey(guildId));
  out.rows[name] = { guildId, key: settings.settingsKey(guildId), raw, ttl };
}
mkdirSync(OUT, { recursive: true });
writeFileSync(join(OUT, 'settings-cache.json'), `${JSON.stringify(out, null, 2)}\n`);
process.stdout.write(`settings-cache.json: ${Object.keys(out.rows).length} rows\n`);
redis.disconnect();
await prisma.$disconnect();
process.exit(0);
