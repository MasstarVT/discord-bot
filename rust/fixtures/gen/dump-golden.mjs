// Dumps reference ("golden") data from the JavaScript bot so the Rust port can be tested against it.
// Run from the repo root: node rust/fixtures/gen/dump-golden.mjs
import { readdirSync, statSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden');
const imp = (p) => import(pathToFileURL(join(ROOT, p)).href);
const write = (name, value) => {
  const json = JSON.stringify(value, (_k, v) => (typeof v === 'bigint' ? v.toString() : v instanceof RegExp ? { $regex: v.source, flags: v.flags } : typeof v === 'function' ? undefined : v), 2);
  writeFileSync(join(OUT, name), `${json}\n`);
  process.stdout.write(`${name}: ${json.length} bytes\n`);
};
function walk(dir) {
  return readdirSync(dir).sort().flatMap((entry) => {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) return walk(full);
    return entry.endsWith('.js') && !entry.startsWith('_') ? [full] : [];
  });
}

// 1. Every slash command: the exact Discord payload plus the routing metadata.
const { buildCommandPayload } = await imp('src/handlers/commandLoader.js');
const commands = [];
for (const file of walk(join(ROOT, 'src', 'commands'))) {
  const ns = await import(pathToFileURL(file).href);
  if (!ns.data?.toJSON) continue;
  commands.push({
    file: relative(ROOT, file).split(/[\\/]/).join('/'),
    category: basename(dirname(file)),
    name: ns.data.name,
    module: ns.module ?? null,
    gated: ns.gated ?? true,
    permissionLevel: ns.permissionLevel ?? 0,
    subcommandPermissions: ns.subcommandPermissions ?? {},
    allowInDMs: ns.allowInDMs === true,
    hasAutocomplete: typeof ns.autocomplete === 'function',
    payload: buildCommandPayload(ns),
  });
}
write('commands.json', commands);

// 2. Config registries.
const cfg = {};
for (const f of ['constants', 'customIds', 'hookPoints', 'jobTypes', 'modules']) {
  const ns = await imp(`src/config/${f}.js`);
  cfg[f] = Object.fromEntries(Object.entries(ns).filter(([, v]) => typeof v !== 'function'));
}
write('config.json', cfg);

// 3. Plugin manifests.
const plugins = [];
for (const file of walk(join(ROOT, 'src', 'plugins'))) {
  const { plugin } = await import(pathToFileURL(file).href);
  if (!plugin) continue;
  plugins.push({
    file: relative(ROOT, file).split(/[\\/]/).join('/'),
    id: plugin.id,
    module: plugin.module ?? null,
    hooks: (plugin.hooks ?? []).map((h) => ({ event: h.event, id: h.id, module: h.module, always: h.always === true, timeoutMs: h.timeoutMs ?? null, hasFilter: typeof h.filter === 'function' })),
    components: (plugin.components ?? []).map((c) => ({ type: c.type, prefix: c.prefix, module: c.module })),
    jobs: (plugin.jobs ?? []).map((j) => j.type),
    provides: Object.keys(plugin.provides ?? {}),
    hasInit: typeof plugin.init === 'function',
  });
}
write('plugins.json', plugins);

// 4. The Prisma data model (tables, columns, defaults, enums, indexes) as Prisma itself sees it.
const { Prisma } = await import('@prisma/client');
write('prisma-dmmf.json', Prisma.dmmf.datamodel);

// 5. Dashboard catalog (what GET /api/catalog answers).
const { buildCatalog } = await imp('src/dashboard/catalog.js');
write('dashboard-catalog.json', buildCatalog());

process.exit(0);
