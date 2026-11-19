// The deploy hash of the real command set, as the JS commandLoader computes it, so the Rust bot can prove that
// switching from JS to Rust does not redeploy unchanged commands. Also the localeCompare order of the command names
// and of a list of names that differ only in punctuation, digits and case.
// Writes rust/fixtures/golden/bot-core/deploy-hash.json. Usage (repository root): node rust/fixtures/gen/bot-core-deploy.mjs
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'bot-core');
process.env.NODE_ENV = 'test';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const { computeDeployHash } = await import(pathToFileURL(join(ROOT, 'src', 'handlers', 'commandLoader.js')).href);
const commands = JSON.parse(readFileSync(join(ROOT, 'rust', 'fixtures', 'golden', 'commands.json'), 'utf8'));
const payloads = commands.map((c) => c.payload);

const synthetic = ['a-b', 'a_b', 'ab', 'a0', 'a9', 'a', 'A', 'b', 'a-', 'a_', 'aa', 'Ab', 'a1b', 'a-a', 'a_a', 'a b', 'a.b', 'z-9', 'z_0', 'Zz', 'zZ'];
const out = {
  clientId: '123456789012345678',
  guildId: '987654321098765432',
  global: computeDeployHash('123456789012345678', undefined, payloads),
  guild: computeDeployHash('123456789012345678', '987654321098765432', payloads),
  reversedGlobal: computeDeployHash('123456789012345678', undefined, [...payloads].reverse()),
  sortedNames: [...payloads].sort((a, b) => a.name.localeCompare(b.name)).map((p) => p.name),
  syntheticSorted: [...synthetic].sort((a, b) => a.localeCompare(b)),
  small: {
    base: computeDeployHash('c1', undefined, [{ name: 'a', description: 'a' }, { name: 'b', description: 'b' }]),
    emptyGuild: computeDeployHash('c1', '', [{ name: 'a', description: 'a' }, { name: 'b', description: 'b' }]),
  },
};
mkdirSync(OUT, { recursive: true });
writeFileSync(join(OUT, 'deploy-hash.json'), `${JSON.stringify(out, null, 2)}\n`);
process.stdout.write(`deploy-hash.json: ${payloads.length} payloads, global ${out.global}\n`);
process.exit(0);
