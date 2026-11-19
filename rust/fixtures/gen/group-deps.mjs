// Checks rust/docs/groups.json against the JS import graph: every src/{services,plugins,commands} file belongs to
// exactly one group (or to the core), and prints which other groups each group imports from.
// Run from the repo root: node rust/fixtures/gen/group-deps.mjs
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const groups = JSON.parse(readFileSync(join(ROOT, 'rust', 'docs', 'groups.json'), 'utf8'));
const rel = (p) => relative(ROOT, p).split(/[\\/]/).join('/');
const walk = (dir) => readdirSync(dir).flatMap((e) => {
  const full = join(dir, e);
  return statSync(full).isDirectory() ? walk(full) : e.endsWith('.js') ? [full] : [];
});

const owner = new Map(); // repo-relative file -> group
for (const [name, g] of Object.entries(groups)) {
  const files = [
    ...g.services.map((s) => `src/services/${s}.js`),
    ...g.plugins.map((s) => `src/plugins/${s}.js`),
    ...g.commands.map((s) => `src/commands/${s}.js`),
    ...g.utils.map((s) => `src/utils/${s}.js`),
  ];
  for (const f of files) {
    if (owner.has(f)) console.log(`DUPLICATE ${f}: ${owner.get(f)} and ${name}`);
    try { statSync(join(ROOT, f)); } catch { console.log(`MISSING FILE ${f} (${name})`); }
    owner.set(f, name);
  }
}
const CORE_SERVICES = new Set(['redis', 'settingsService', 'scheduler', 'rewards'].map((s) => `src/services/${s}.js`));
const all = ['services', 'plugins', 'commands'].flatMap((d) => walk(join(ROOT, 'src', d))).map(rel);
for (const f of all) if (!owner.has(f) && !CORE_SERVICES.has(f)) console.log(`UNASSIGNED ${f}`);

const deps = {};
for (const [file, group] of owner) {
  const text = readFileSync(join(ROOT, file), 'utf8');
  for (const m of text.matchAll(/(?:from\s+|import\s*\(\s*)['"](\.[^'"]+)['"]/g)) {
    const target = rel(resolve(dirname(join(ROOT, file)), m[1]));
    const other = owner.get(target);
    if (other && other !== group) ((deps[group] ??= {})[other] ??= new Set()).add(`${file.replace('src/', '')} -> ${target.replace('src/', '')}`);
  }
}
for (const [name, g] of Object.entries(groups)) {
  const d = deps[name] ?? {};
  const bad = Object.keys(d).filter((o) => groups[o].wave >= g.wave);
  console.log(`${name} (wave ${g.wave}): ${Object.keys(d).join(', ') || '-'}${bad.length ? `   <-- SAME OR LATER WAVE: ${bad.join(', ')}` : ''}`);
  for (const o of bad) for (const edge of d[o]) console.log(`      ${edge}`);
}
