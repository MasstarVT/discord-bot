// Smoke test: loads every module the bot loads at runtime and checks every
// slash-command payload against Discord's limits, without Discord, Postgres
// or Redis. Run with `npm test`.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readdirSync, statSync } from 'node:fs';
import { join, resolve, dirname, relative } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

process.env.NODE_ENV      ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID     ??= '000000000000000000';
process.env.DATABASE_URL  ??= 'postgresql://test:test@127.0.0.1:1/test';
// Nothing listens here; the client retries in the background until after().
process.env.REDIS_URL     ??= 'redis://127.0.0.1:1';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SRC  = join(ROOT, 'src');

// Entry points that log in, start servers or touch the database on import.
const SKIP = new Set(['src/bot.js', 'src/database/seed.js']);

function walk(dir) {
  return readdirSync(dir).flatMap((entry) => {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) return walk(full);
    return entry.endsWith('.js') ? [full] : [];
  });
}

const rel = (file) => relative(ROOT, file).replace(/\\/g, '/');
const load = (file) => import(pathToFileURL(file).href);

// Silence the Redis reconnect noise while modules load.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug };
console.error = console.warn = console.log = console.debug = () => {};

after(async () => {
  const { default: redis } = await import('../src/services/redis.js');
  redis.disconnect();
  Object.assign(console, quiet);
});

test('every source module imports', async () => {
  const failures = [];
  for (const file of walk(SRC)) {
    if (SKIP.has(rel(file))) continue;
    try {
      await load(file);
    } catch (err) {
      failures.push(`${rel(file)}: ${err.message}`);
    }
  }
  assert.deepEqual(failures, []);
});

// ── Slash-command payload limits ─────────────────────────────────────────────
// https://discord.com/developers/docs/interactions/application-commands

const NAME_RE = /^[-_\p{L}\p{N}\p{sc=Deva}\p{sc=Thai}]{1,32}$/u;
const SUB_TYPES = new Set([1, 2]); // SUB_COMMAND, SUB_COMMAND_GROUP

function checkOptions(path, options, problems, depth = 0) {
  if (options.length > 25) problems.push(`${path}: ${options.length} options (max 25)`);
  const names = new Set();
  let seenOptional = false;
  for (const opt of options) {
    const p = `${path} ${opt.name}`;
    if (!NAME_RE.test(opt.name) || opt.name !== opt.name.toLowerCase()) problems.push(`${p}: invalid name`);
    if (names.has(opt.name)) problems.push(`${p}: duplicate option name`);
    names.add(opt.name);
    if (!opt.description || opt.description.length > 100) problems.push(`${p}: description must be 1-100 chars`);
    if (opt.choices?.length > 25) problems.push(`${p}: ${opt.choices.length} choices (max 25)`);
    if (opt.choices && opt.autocomplete) problems.push(`${p}: choices and autocomplete are exclusive`);
    for (const c of opt.choices ?? []) {
      if (!c.name || c.name.length > 100) problems.push(`${p}: choice name "${c.name}" must be 1-100 chars`);
      if (typeof c.value === 'string' && c.value.length > 100) problems.push(`${p}: choice value too long`);
    }
    if (SUB_TYPES.has(opt.type)) {
      if (opt.type === 2 && depth > 0) problems.push(`${p}: subcommand group nested too deep`);
      checkOptions(p, opt.options ?? [], problems, depth + 1);
    } else {
      if (opt.required) {
        if (seenOptional) problems.push(`${p}: required option after an optional one`);
      } else {
        seenOptional = true;
      }
    }
  }
  const hasSub = options.some((o) => SUB_TYPES.has(o.type));
  if (hasSub && options.some((o) => !SUB_TYPES.has(o.type))) {
    problems.push(`${path}: mixes subcommands with plain options`);
  }
}

// Discord caps the combined length of every name, description and choice value.
function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('every command exports a valid payload within Discord limits', async () => {
  // The payload Discord actually receives (adds guild-only contexts unless a command sets allowInDMs).
  const { buildCommandPayload } = await load(join(SRC, 'handlers', 'commandLoader.js'));
  const problems = [];
  const names = new Map();
  for (const file of walk(join(SRC, 'commands'))) {
    const mod = await load(file);
    const where = rel(file);
    if (!mod.data || typeof mod.execute !== 'function') {
      problems.push(`${where}: missing { data, execute }`);
      continue;
    }
    let json;
    try {
      json = buildCommandPayload(mod);
    } catch (err) {
      problems.push(`${where}: building the payload threw: ${err.message}`);
      continue;
    }
    if (mod.allowInDMs !== true) {
      if (JSON.stringify(json.contexts) !== '[0]') problems.push(`${where}: guild-only command must have contexts [0], got ${JSON.stringify(json.contexts)}`);
      if (JSON.stringify(json.integration_types) !== '[0]') problems.push(`${where}: guild-only command must have integration_types [0], got ${JSON.stringify(json.integration_types)}`);
    }
    if (names.has(json.name)) problems.push(`${where}: duplicate command name "${json.name}" (also ${names.get(json.name)})`);
    names.set(json.name, where);
    if (!NAME_RE.test(json.name) || json.name !== json.name.toLowerCase()) problems.push(`${where}: invalid name`);
    if (!json.description || json.description.length > 100) problems.push(`${where}: description must be 1-100 chars`);
    checkOptions(`/${json.name}`, json.options ?? [], problems);
    const chars = charCount(json);
    if (chars > 4000) problems.push(`${where}: ${chars} combined characters (max 4000)`);
    if (mod.autocomplete !== undefined && typeof mod.autocomplete !== 'function') problems.push(`${where}: autocomplete is not a function`);
    const usesAutocomplete = JSON.stringify(json).includes('"autocomplete":true');
    if (usesAutocomplete && typeof mod.autocomplete !== 'function') problems.push(`${where}: has autocomplete options but no autocomplete()`);
  }
  assert.ok(names.size <= 100, `${names.size} global chat commands (max 100)`);
  assert.deepEqual(problems, []);
});

test('every event module exports { name, execute }', async () => {
  const problems = [];
  for (const file of walk(join(SRC, 'events'))) {
    const mod = await load(file);
    if (typeof mod.name !== 'string' || typeof mod.execute !== 'function') problems.push(rel(file));
  }
  assert.deepEqual(problems, []);
});

test('a payload without allowInDMs is registered for servers only', async () => {
  const { buildCommandPayload } = await load(join(SRC, 'handlers', 'commandLoader.js'));
  const { SlashCommandBuilder } = await import('discord.js');
  const data = new SlashCommandBuilder().setName('probe').setDescription('probe');
  assert.deepEqual(buildCommandPayload({ data }).contexts, [0]);
  assert.deepEqual(buildCommandPayload({ data }).integration_types, [0]);
  assert.equal(buildCommandPayload({ data, allowInDMs: true }).contexts, undefined);
});
