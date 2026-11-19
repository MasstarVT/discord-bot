// The JS side of feat-economy's lookup test: how the JS economyService's case-insensitive lookups (Prisma `equals` /
// `contains` with `mode: 'insensitive'`) treat names and queries holding LIKE wildcards (`%`, `_`) and backslashes.
//
//   node rust/fixtures/gen/feat-economy-search.mjs <guildId> <userId>
// Creates the shop items, boosts and inventory rows through the JS service (the guild must be empty), then prints one
// `RESULT <json>` line with every lookup's answer. Uses TEST_DATABASE_URL / TEST_REDIS_URL.
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.LOG_LEVEL = 'error';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const svc = await imp('src', 'services', 'economyService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

export const NAMES = ['50% off', '50x off', 'a_b', 'axb', 'back\\slash', 'backslash', 'Plain'];
export const QUERIES = ['%', '_', '50%', '50% off', 'a_b', 'A_B', '\\', 'back\\slash', 'back\\\\slash', 'x', '%off', 'a%', 'plain', ''];

const [g, u] = process.argv.slice(2);
try {
  const ids = {};
  let price = 1;
  for (const name of NAMES) {
    const item = await svc.createItem(g, { name, price: price++, type: 'USABLE' });
    ids[item.id] = name;
    await svc.giveItem({ guildId: g, userId: u, itemId: item.id, quantity: 1 });
    await svc.createBoost(g, { name, percent: price, period: 'DAILY', createdBy: u });
  }
  // A query PostgreSQL refuses (a pattern ending in a lone backslash) rejects in the JS: recorded as 'ERR'.
  const tryIt = async (f) => { try { return await f(); } catch { return 'ERR'; } };
  const names = (rows, pick = (r) => r.name) => rows.map(pick);
  const out = { items: {}, resolve: {}, taken: {}, boosts: {}, resolveBoost: {}, inventory: {} };
  for (const q of QUERIES) {
    out.items[q] = await tryIt(async () => names(await svc.searchItems(g, q, {})));
    out.resolve[q] = await tryIt(async () => (await svc.resolveItem(g, q))?.name ?? null);
    out.taken[q] = await tryIt(async () => (await svc.findItemByNameInsensitive(g, q))?.name ?? null);
    out.boosts[q] = await tryIt(async () => names(await svc.searchBoosts(g, q)));
    out.resolveBoost[q] = await tryIt(async () => (await svc.resolveBoost(g, q))?.name ?? null);
    out.inventory[q] = await tryIt(async () => names(await svc.searchInventory(g, u, q, { usableOnly: true }), (r) => r.item.name));
  }
  answer(out);
} catch (err) {
  answer({ error: String(err?.stack ?? err) });
} finally {
  await prisma.$disconnect().catch(() => {});
  redis.disconnect?.();
}
