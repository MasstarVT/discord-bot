// The JS side of bot-core's cross-language tests. Runs the JS bot's own services against the test servers
// (TEST_DATABASE_URL, TEST_REDIS_URL) and prints one JSON line on stdout.
//
//   node rust/fixtures/gen/bot-core-js.mjs settings-get <guildId>          JS getSettings({ fresh: true }) (writes Redis)
//   node rust/fixtures/gen/bot-core-js.mjs settings-memo <guildId>         JS getSettings() in a fresh process (Redis first)
//   node rust/fixtures/gen/bot-core-js.mjs settings-update <guildId> <json> JS updateSettings (write-through)
//   node rust/fixtures/gen/bot-core-js.mjs schedule <guildId> <json opts>   JS scheduleJob (runAt in opts.runAtMs)
//   node rust/fixtures/gen/bot-core-js.mjs get-job <dedupeKey>              JS getJob
//   node rust/fixtures/gen/bot-core-js.mjs cache-set <key> <json> <ttl>     JS cache.set
//   node rust/fixtures/gen/bot-core-js.mjs cache-get <key>                  JS cache.get
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production'; // no debug lines on stdout
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL;
process.env.REDIS_URL = process.env.TEST_REDIS_URL;

// The logger writes to stdout; the answer is the last line, prefixed so the Rust side can find it.
const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const { default: redis, cache } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'settings-get' || cmd === 'settings-memo' || cmd === 'settings-update') {
    const settings = await imp('src', 'services', 'settingsService.js');
    let row;
    if (cmd === 'settings-get') row = await settings.getSettings(args[0], { fresh: true });
    else if (cmd === 'settings-memo') row = await settings.getSettings(args[0]);
    else row = await settings.updateSettings(args[0], JSON.parse(args[1]));
    answer({ row, raw: await redis.get(settings.settingsKey(args[0])) });
  } else if (cmd === 'schedule') {
    const scheduler = await imp('src', 'services', 'scheduler.js');
    const opts = JSON.parse(args[1]);
    const res = await scheduler.scheduleJob({ ...opts, guildId: args[0], runAt: new Date(opts.runAtMs) });
    answer({ id: res.id, runAt: res.runAt });
  } else if (cmd === 'get-job') {
    const scheduler = await imp('src', 'services', 'scheduler.js');
    answer(await scheduler.getJob(args[0]));
  } else if (cmd === 'cache-set') {
    answer(await cache.set(args[0], JSON.parse(args[1]), Number(args[2])));
  } else if (cmd === 'cache-get') {
    answer(await cache.get(args[0]));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect();
process.exit(0);
