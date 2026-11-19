// Golden data of dash-records-a, produced from the JS stores (no database needed):
//   rust/fixtures/golden/dash-records-a/raw-sql.json   every RAW_SQL statement of the 11 stores: { name, text, values }
//
//   node rust/fixtures/gen/dash-records-a-golden.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL ??= 'postgresql://test:test@127.0.0.1:1/test';
process.env.REDIS_URL ??= 'redis://127.0.0.1:1';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';

const STORES = ['achievements', 'automations', 'balances', 'cases', 'economyBoosts', 'embeds', 'feeds', 'giveaways', 'levelRoles', 'levels', 'polls'];
const out = [];
const seen = new Set();
for (const key of STORES) {
  const mod = await imp('src', 'dashboard', 'collections', `${key}.store.js`);
  for (const { name, build } of mod.RAW_SQL ?? []) {
    if (seen.has(name)) continue;
    seen.add(name);
    const q = build();
    out.push({ name, store: key, text: q.text, values: q.values });
  }
}
const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'dash-records-a');
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, 'raw-sql.json'), `${JSON.stringify(out, null, 2)}\n`);
process.stdout.write(`wrote ${out.length} statements\n`);
process.exit(0);
