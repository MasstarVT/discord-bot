// Runs the JS test suite with bot-util-record-hooks.mjs loaded and writes every recorded call of the ported src/utils
// functions to rust/fixtures/golden/bot-util/recorded.json ({ "module.fn": [[args, result], ...] }).
// Run from the repo root: node rust/fixtures/gen/bot-util-record.mjs [test files...]
// Test failures do not matter here (there is no Redis or PostgreSQL); only the recorded calls are kept.
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'bot-util');
const hooks = pathToFileURL(join(ROOT, 'rust', 'fixtures', 'gen', 'bot-util-record-hooks.mjs')).href;
const dir = mkdtempSync(join(tmpdir(), 'bot-util-record-'));

const files = process.argv.slice(2).length
  ? process.argv.slice(2)
  : readdirSync(join(ROOT, 'test')).filter((f) => f.endsWith('.test.js')).sort().map((f) => join('test', f));

for (const file of files) {
  const r = spawnSync(process.execPath, ['--import', hooks, '--test', '--test-reporter=dot', file], {
    cwd: ROOT, env: { ...process.env, BOT_UTIL_RECORD_DIR: dir, NODE_ENV: 'test' }, encoding: 'utf8', timeout: 300_000,
  });
  process.stdout.write(`${file}: exit ${r.status}\n`);
}

const merged = {};
const seen = new Set();
for (const f of readdirSync(dir).sort()) {
  const rec = JSON.parse(readFileSync(join(dir, f), 'utf8'));
  for (const [key, list] of Object.entries(rec)) {
    for (const pair of list) {
      const id = `${key}\u0000${JSON.stringify(pair[0])}`;
      if (seen.has(id)) continue;
      seen.add(id);
      (merged[key] ??= []).push(pair);
    }
  }
}
rmSync(dir, { recursive: true, force: true });
const sorted = Object.fromEntries(Object.keys(merged).sort().map((k) => [k, merged[k]]));
mkdirSync(OUT, { recursive: true });
writeFileSync(join(OUT, 'recorded.json'), `${JSON.stringify(sorted)}\n`);
for (const [k, v] of Object.entries(sorted)) process.stdout.write(`${k}: ${v.length}\n`);
