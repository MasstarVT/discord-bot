// Generates the descriptor data of the dash-collections crate from the JS dashboard (src/dashboard/collections/*.js, not
// the *.store.js files): every collection definition with its server-only function hooks left out (the crate restates
// them in Rust; `hooks` lists which exist so its tests can check the two agree).
//   rust/crates/dash-collections/src/collections_data.json
// Pure: no database or Redis. Usage (repository root): node rust/fixtures/gen/dash-collections-data.mjs
import { writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (p) => import(pathToFileURL(join(ROOT, p)).href);
const { COLLECTION_LIST } = await imp('src/dashboard/collections/index.js');

const OUT = join(ROOT, 'rust', 'crates', 'dash-collections', 'src', 'collections_data.json');

/** A JSON copy that refuses anything JSON cannot carry exactly (functions are dropped and listed by the caller). */
function plain(v, at) {
  if (v === null || typeof v === 'string' || typeof v === 'boolean') return v;
  if (typeof v === 'number') {
    if (!Number.isFinite(v)) throw new Error(`${at}: non-finite number`);
    return v;
  }
  if (Array.isArray(v)) {
    return v.map((x, i) => {
      if (x === undefined || typeof x === 'function') throw new Error(`${at}.${i}: ${typeof x} in an array`);
      return plain(x, `${at}.${i}`);
    });
  }
  if (typeof v === 'object') {
    if (Object.getPrototypeOf(v) !== Object.prototype && Object.getPrototypeOf(v) !== null) throw new Error(`${at}: not a plain object`);
    const out = {};
    for (const [k, x] of Object.entries(v)) {
      if (x === undefined || typeof x === 'function') continue;
      out[k] = plain(x, `${at}.${k}`);
    }
    return out;
  }
  throw new Error(`${at}: ${typeof v}`);
}

const hooksOf = (c) => {
  const out = Object.keys(c).filter((k) => typeof c[k] === 'function');
  for (const a of c.actions ?? []) for (const k of Object.keys(a)) if (typeof a[k] === 'function') out.push(`actions.${a.id}.${k}`);
  return out;
};

const data = {
  order: COLLECTION_LIST.map((c) => c.key),
  collections: Object.fromEntries(COLLECTION_LIST.map((c) => [c.key, plain(c, c.key)])),
  hooks: Object.fromEntries(COLLECTION_LIST.map((c) => [c.key, hooksOf(c)])),
};
writeFileSync(OUT, `${JSON.stringify(data, null, 1)}\n`);
process.stdout.write(`${OUT.slice(ROOT.length + 1)}\n`);
process.exit(0);
