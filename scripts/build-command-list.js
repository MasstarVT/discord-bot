// Writes src/dashboard/commands.json, the command reference the public /docs page shows, from the command files.
// Run it after adding or changing a command: npm run docs:commands (test/integration.test.js fails while it is stale).
import { readdirSync, statSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { buildCommandPayload } from '../src/handlers/commandLoader.js';
import { summarizeCommands } from '../src/dashboard/commandList.js';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function walk(dir) {
  return readdirSync(dir).sort().flatMap((entry) => {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) return walk(full);
    return entry.endsWith('.js') && !entry.startsWith('_') ? [full] : [];
  });
}

const entries = [];
for (const file of walk(join(ROOT, 'src', 'commands'))) {
  const ns = await import(pathToFileURL(file).href);
  if (ns.data?.toJSON) entries.push({ ns, payload: buildCommandPayload(ns) });
}
const list = summarizeCommands(entries);
writeFileSync(join(ROOT, 'src', 'dashboard', 'commands.json'), `${JSON.stringify(list, null, 2)}\n`);
process.stdout.write(`${basename(fileURLToPath(import.meta.url))}: ${list.length} commands written\n`);
// The command files open database and cache clients on import; nothing here needs them to stay open.
process.exit(0);
