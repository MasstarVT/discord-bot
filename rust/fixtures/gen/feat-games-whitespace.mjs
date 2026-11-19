// The JS side of feat-games' whitespace test: how the JS games treat Unicode white space around typed input (JS `\s`
// and `String.prototype.trim` are not Rust's `char::is_whitespace`: U+0085 is white space only in Rust).
//
//   node rust/fixtures/gen/feat-games-whitespace.mjs
// Prints one `RESULT <json>` line: per input, the /games numguess message filter and guess value, the /scrabble word
// and exchange parsers and the /roulette space parser. No servers needed.
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.LOG_LEVEL = 'error';

const scrabble = await imp('src', 'services', 'scrabbleService.js');
const roulette = await imp('src', 'commands', 'games', 'roulette.js');

const SPACES = ['\u0085', ' ', '﻿', ' ', '᠎', '　', '​', ' ', '\t'];
const inputs = [];
for (const sp of SPACES) inputs.push(`${sp}42`, `42${sp}`, `HI${sp}`, `${sp}AB`, `re${sp}d`);

const out = inputs.map((input) => ({
  input,
  guess: /^\s*\d{1,6}\s*$/.test(input),
  value: /^\s*\d{1,6}\s*$/.test(input) ? parseInt(input.trim(), 10) : null,
  word: scrabble.parseWordInput(input),
  tiles: scrabble.parseTilesInput(input),
  space: roulette.parseSpace(input)?.token ?? null,
}));
process.stdout.write(`\nRESULT ${JSON.stringify(out)}\n`);
process.exit(0);
