// Produces the JS side of bot-db's JSON round-trip tests: what JSON.stringify writes for a Prisma row of every model
// (the Redis cache format) and for dates.
//   node rust/fixtures/gen/bot-db-json.mjs        (from the repository root)
// Writes rust/fixtures/golden/bot-db/rows.json and rust/fixtures/golden/bot-db/dates.json.
process.env.TZ = 'UTC';

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const RUST = resolve(HERE, '..', '..');
const OUT = join(RUST, 'fixtures', 'golden', 'bot-db');
mkdirSync(OUT, { recursive: true });
const dmmf = JSON.parse(readFileSync(join(RUST, 'fixtures', 'golden', 'prisma-dmmf.json'), 'utf8'));
const enums = Object.fromEntries(dmmf.enums.map((e) => [e.name, e.values.map((v) => v.name)]));

const BASE = Date.UTC(2026, 0, 2, 3, 4, 5, 678);

/** A JS value of the field's type, as a Prisma row holds it (Date objects, numbers, parsed Json). */
function sample(model, f, i, variant) {
  if (f.kind === 'enum') {
    const values = enums[f.type];
    return values[(i + (variant === 'full' ? 0 : 1)) % values.length];
  }
  const list = (fn) => (variant === 'full' ? [fn(0), fn(1), fn(2)] : []);
  switch (f.type) {
    case 'String': {
      const one = (k) => (f.isId ? `c${String(i).padStart(4, '0')}k${k}abcdefghijklmnopqrs`.slice(0, 25) : `${model}.${f.name}#${k} "q" \\ é ✓ 🎉\n`);
      return f.isList ? list(one) : one(0);
    }
    case 'Int': {
      const one = (k) => (i % 5 === 0 ? 2147483647 - k : i % 5 === 1 ? -2147483648 + k : (i * 37 + k) % 1000);
      return f.isList ? list(one) : one(0);
    }
    case 'BigInt': return BigInt(i);
    case 'Float': return variant === 'full' ? 1.5 + i : 2; // 2 is written as "2" by JSON.stringify
    case 'Boolean': return (i + (variant === 'full' ? 0 : 1)) % 2 === 0;
    case 'DateTime': {
      const one = (k) => new Date(BASE + i * 1_000_003 + k * 86_400_000 + (variant === 'full' ? 0 : 7));
      return f.isList ? list(one) : one(0);
    }
    case 'Json':
      return variant === 'full'
        ? { nested: [1, 2.5, -3, { deep: null, s: 'x' }], flag: true, text: `j${i}`, big: 9007199254740991, frac: 0.1 }
        : [`only-${i}`, 0, false];
    default: throw new Error(`unsupported ${f.type}`);
  }
}

const rows = {};
for (const m of dmmf.models) {
  const fields = m.fields.filter((f) => f.kind !== 'object');
  const full = {};
  const nulls = {};
  fields.forEach((f, i) => {
    full[f.name] = sample(m.name, f, i, 'full');
    nulls[f.name] = f.isRequired || f.isList ? sample(m.name, f, i, 'nulls') : null;
  });
  // The cached shape may also omit optional fields entirely.
  const missing = Object.fromEntries(Object.entries(nulls).filter(([k]) => {
    const f = fields.find((x) => x.name === k);
    return f.isRequired || f.isList;
  }));
  rows[m.name] = {
    full: JSON.stringify(full),
    nulls: JSON.stringify(nulls),
    // Extra keys (the settings cache's version stamp) must be ignored.
    withExtra: JSON.stringify({ ...full, _cacheV: 3 }),
    missing: JSON.stringify(missing),
  };
}
writeFileSync(join(OUT, 'rows.json'), `${JSON.stringify(rows, null, 1)}\n`);

// ── Dates ──
const format = [
  0, 1, -1, 999, 1000, BASE, Date.UTC(1999, 11, 31, 23, 59, 59, 999), Date.UTC(2000, 1, 29), Date.UTC(9999, 11, 31, 23, 59, 59, 999),
  Date.UTC(10000, 0, 1), Date.UTC(-1, 0, 1), Date.UTC(0, 0, 1) - 0, -62198755200000, 8.64e15, -8.64e15,
].map((ms) => ({ ms, json: JSON.parse(JSON.stringify({ d: new Date(ms) })).d }));

const inputs = [
  '2026-01-02T03:04:05.678Z', '2026-01-02T03:04:05Z', '2026-01-02T03:04:05.6Z', '2026-01-02T03:04:05.678912Z',
  '2026-01-02T03:04:05.678+02:00', '2026-01-02T03:04:05.678-0530', '2026-01-02T03:04:05+00:00', '2026-01-02T03:04Z',
  '2026-01-02T03:04:05', '2026-01-02T03:04', '2026-01-02', '2026-01', '2026', '2026-01-02T24:00:00Z',
  '+010000-01-01T00:00:00.000Z', '-000001-01-01T00:00:00.000Z', '2026-01-02 03:04:05.678Z', '2026-01-02T03:04:05.678z',
  'not a date', '', '2026-13-01T00:00:00Z', '2026-02-30T00:00:00Z', '2026-01-02T25:00:00Z',
];
const parse = inputs.map((input) => {
  const d = new Date(input);
  return { input, iso: Number.isNaN(d.getTime()) ? null : d.toISOString() };
});
writeFileSync(join(OUT, 'dates.json'), `${JSON.stringify({ format, parse }, null, 1)}\n`);
console.log(`wrote ${OUT}: ${Object.keys(rows).length} models, ${format.length} date formats, ${parse.length} date inputs`);
