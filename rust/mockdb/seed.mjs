// Baseline seed for the mock database: writes rows into EVERY table through the real Prisma client (the same client
// the JS bot uses), then reads them back through Prisma and stores what it saw in
// rust/fixtures/golden/mockdb/baseline.json. The Rust tests read the same rows with sqlx and must see the same values.
// Each model gets two rows: "min" (only the required fields, so every database and Prisma default applies) and "full"
// (every column filled, with awkward text). The values are synthetic: they test encoding, not feature behaviour.
// Usage: DATABASE_URL=<url> node seed.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Prisma, PrismaClient } from '@prisma/client';

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT_DIR = resolve(HERE, '..', 'fixtures', 'golden', 'mockdb');
const prisma = new PrismaClient();
const { models, enums } = Prisma.dmmf.datamodel;
const enumValues = Object.fromEntries(enums.map((e) => [e.name, e.values.map((v) => v.name)]));
const byName = Object.fromEntries(models.map((m) => [m.name, m]));
const delegate = (name) => prisma[name[0].toLowerCase() + name.slice(1)];

// Parents before children.
const order = [];
const seen = new Set();
function visit(model) {
  if (seen.has(model.name)) return;
  seen.add(model.name);
  for (const f of model.fields) if (f.kind === 'object' && f.relationFromFields?.length && f.type !== model.name) visit(byName[f.type]);
  order.push(model);
}
visit(byName.GuildSettings);
models.forEach(visit);

export const GUILD_IDS = ['900000000000000001', '900000000000000002'];
const AWKWARD = 'ünïcödé ✨ \'single\' "double" \\ back\nnewline <@123> {user}';
const BASE_DATE = Date.UTC(2026, 0, 2, 3, 4, 5, 678);
let counter = 0;

function value(model, field, variant) {
  counter += 1;
  if (field.kind === 'enum') {
    const vals = enumValues[field.type];
    const v = vals[variant === 0 ? 0 : vals.length - 1];
    return field.isList ? [v] : v;
  }
  switch (field.type) {
    case 'String': {
      if (field.isList) return variant === 0 ? [] : ['a', 'b ✨', '800000000000000042'];
      if (/Id$/.test(field.name) || field.name === 'id') return `8${String(counter).padStart(17, '0')}`;
      return variant === 0 ? `${model.name}.${field.name}` : `${model.name}.${field.name} ${AWKWARD}`;
    }
    case 'Int': return field.isList ? [1, 2, 3] : (variant === 0 ? 1 : 2_000_000_000 - (counter % 1000));
    case 'BigInt': return BigInt('9007199254740993') + BigInt(counter);
    case 'Float': return variant === 0 ? 0.5 : 1234.5678;
    case 'Boolean': return variant === 1 ? !(field.hasDefaultValue && field.default === true) : false;
    case 'DateTime': return new Date(BASE_DATE + counter * 86_400_000 + (counter % 997));
    case 'Json': return variant === 0 ? {} : { text: AWKWARD, n: 1.5, big: 9007199254740991, list: [1, 'two', null, { three: true }], nested: { empty: {}, none: null } };
    default: throw new Error(`unhandled type ${field.type} on ${model.name}.${field.name}`);
  }
}

const created = {}; // model -> [minRow, fullRow]
for (const model of order) {
  const fks = new Map(); // scalar field -> { parent, toField }
  for (const f of model.fields) {
    if (f.kind !== 'object' || !f.relationFromFields?.length) continue;
    f.relationFromFields.forEach((from, i) => fks.set(from, { parent: f.type, toField: f.relationToFields[i] }));
  }
  created[model.name] = [];
  for (const variant of [0, 1]) {
    const data = {};
    for (const f of model.fields) {
      if (f.kind === 'object') continue;
      const fk = fks.get(f.name);
      if (fk) {
        const parentRow = fk.parent === model.name ? created[model.name][0] : created[fk.parent][variant];
        if (parentRow) data[f.name] = parentRow[fk.toField];
        continue;
      }
      if (model.name === 'GuildSettings' && f.name === 'guildId') { data.guildId = GUILD_IDS[variant]; continue; }
      if (f.isUpdatedAt) continue;
      const generated = f.hasDefaultValue && typeof f.default === 'object' && f.default !== null && !Array.isArray(f.default);
      if (variant === 0) {
        if (!f.isRequired || f.hasDefaultValue || f.isList) continue;
      } else if (generated && f.default.name !== 'now') continue; // cuid(), uuid(), autoincrement(): let Prisma do it
      data[f.name] = value(model, f, variant);
    }
    try {
      created[model.name].push(await delegate(model.name).create({ data }));
    } catch (err) {
      process.stderr.write(`seed failed on ${model.name} (${variant === 0 ? 'min' : 'full'}): ${err.message}\n`);
      process.exitCode = 1;
      await prisma.$disconnect();
      process.exit(1);
    }
  }
}

// Read everything back through Prisma: this is "what the JS bot sees".
const snapshot = {};
let rows = 0;
for (const model of order) {
  const [min, full] = created[model.name];
  const pk = model.primaryKey?.fields ?? model.fields.filter((f) => f.isId).map((f) => f.name);
  const where = (row) => (pk.length === 1 ? { [pk[0]]: row[pk[0]] } : { [pk.join('_')]: Object.fromEntries(pk.map((k) => [k, row[k]])) });
  snapshot[model.name] = {
    table: model.dbName ?? model.name,
    primaryKey: pk,
    min: await delegate(model.name).findUnique({ where: where(min) }),
    full: await delegate(model.name).findUnique({ where: where(full) }),
  };
  rows += 2;
}
mkdirSync(OUT_DIR, { recursive: true });
writeFileSync(join(OUT_DIR, 'baseline.json'), `${JSON.stringify(snapshot, (_k, v) => (typeof v === 'bigint' ? v.toString() : v), 2)}\n`);
process.stdout.write(`seeded ${rows} rows in ${order.length} tables; snapshot in rust/fixtures/golden/mockdb/baseline.json\n`);
await prisma.$disconnect();
