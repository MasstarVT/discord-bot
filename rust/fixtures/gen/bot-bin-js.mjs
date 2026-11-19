// The JS side of the bot binary's tests (rust/bins/bot). Prints one `RESULT <json>` line on stdout.
//
//   node rust/fixtures/gen/bot-bin-js.mjs deploy-hash <clientId> [guildId]
//       computeDeployHash over the payloads of every real command file (the auto-deploy hash the JS writes to
//       DEPLOY_HASH_FILE), plus the payload names in walk order.
//   node rust/fixtures/gen/bot-bin-js.mjs dotenv-parse <file>
//       dotenv.parse of the file (the dotenv the bot uses), as [key, value] pairs.
//
// deploy-hash runs offline like test/integration.test.js: the services the command files import point at closed
// ports, so nothing reaches a real database, cache or Discord.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);
const [cmd, ...args] = process.argv.slice(2);

function walk(dir) {
  return readdirSync(dir).sort().flatMap((entry) => {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) return walk(full);
    return entry.endsWith('.js') && !entry.startsWith('_') ? [full] : [];
  });
}

try {
  if (cmd === 'deploy-hash') {
    Object.assign(process.env, {
      NODE_ENV: 'production',
      DATABASE_URL: 'postgresql://test:test@127.0.0.1:1/test',
      REDIS_URL: 'redis://127.0.0.1:1',
    });
    const { buildCommandPayload, computeDeployHash } = await import(pathToFileURL(join(ROOT, 'src/handlers/commandLoader.js')).href);
    const payloads = [];
    for (const file of walk(join(ROOT, 'src', 'commands'))) {
      const mod = await import(pathToFileURL(file).href);
      if (mod.data?.toJSON) payloads.push(buildCommandPayload(mod));
    }
    const [clientId, guildId] = args;
    // autoDeployCommands sorts first and computeDeployHash sorts again; both orders give the same hash.
    const sorted = [...payloads].sort((a, b) => a.name.localeCompare(b.name));
    answer({
      hash: computeDeployHash(clientId, guildId || undefined, sorted),
      unsortedHash: computeDeployHash(clientId, guildId || undefined, payloads),
      names: payloads.map((p) => p.name),
      sortedNames: sorted.map((p) => p.name),
    });
  } else if (cmd === 'dotenv-parse') {
    const require = createRequire(join(ROOT, 'package.json'));
    const dotenv = require('dotenv');
    answer({ version: require('dotenv/package.json').version, entries: Object.entries(dotenv.parse(readFileSync(args[0], 'utf8'))) });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err) });
}
process.exit(0);
