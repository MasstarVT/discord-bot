// Reference answers from the JS for the feat-backups audit tests: `node rust/fixtures/gen/feat-backups-audit.mjs <fn> <json args>`.
// Prints one `RESULT <json>` line. Supported: nameFromUrl <url>.
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};

const emoji = await import(pathToFileURL(join(ROOT, 'src', 'commands', 'admin', 'emoji.js')).href);
const [fn, ...args] = process.argv.slice(2);
let out;
if (fn === 'nameFromUrl') out = args.map((u) => ({ in: u, out: emoji.nameFromUrl(u) }));
else out = { error: `unknown ${fn}` };
process.stdout.write(`RESULT ${JSON.stringify(out)}\n`);
process.exit(0);
