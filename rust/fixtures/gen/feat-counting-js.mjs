// The JS side of feat-counting's cross-language tests: the JS bot's own countingService against the test database
// (TEST_DATABASE_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-counting-js.mjs save <guildId> <json state>   countingService.saveState
//   node rust/fixtures/gen/feat-counting-js.mjs load <guildId>                countingService.loadState
//   node rust/fixtures/gen/feat-counting-js.mjs parse <json array of strings> parseCount of each (no servers)
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const svc = await imp('src', 'services', 'countingService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'save') {
    answer(await svc.saveState(args[0], JSON.parse(args[1])));
  } else if (cmd === 'load') {
    answer(await svc.loadState(args[0]));
  } else if (cmd === 'parse') {
    answer(JSON.parse(args[0]).map((s) => svc.parseCount(s)));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
