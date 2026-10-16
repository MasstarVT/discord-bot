// Registry and runtime tests (01-foundation §18.2): the module, hook, customId and job registries,
// the plugin loader, the hook runner, the component maps, the router, the command payloads and the
// static scans over src/. Everything runs offline: no Discord, Postgres, Redis or network.
import { test, after, afterEach, mock } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname, relative } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// Forced (not defaulted): this file must never reach a developer's real services.
Object.assign(process.env, {
  NODE_ENV: 'test',
  DISCORD_TOKEN: 'test-token',
  CLIENT_ID: '000000000000000000',
  DATABASE_URL: 'postgresql://test:test@127.0.0.1:1/test',
  REDIS_URL: 'redis://127.0.0.1:1',
});

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SRC = join(ROOT, 'src');
const rel = (file) => relative(ROOT, file).replace(/\\/g, '/');
const load = (file) => import(pathToFileURL(file).href);

const { MessageType, PermissionFlagsBits, SlashCommandBuilder } = await import('discord.js');
const { PERMISSION_LEVELS, DEFAULT_MODULE_SETTINGS } = await import('../src/config/constants.js');
const { MODULES, MODULE_NAMES, MODULE_FLAGS, isModuleEnabled } = await import('../src/config/modules.js');
const { HOOK_ORDER, INTERNAL_HOOK_POINTS, SYNTHETIC_HOOK_POINTS } = await import('../src/config/hookPoints.js');
const { CUSTOM_ID_PREFIXES, buildCustomId } = await import('../src/config/customIds.js');
const { JOB_TYPES } = await import('../src/config/jobTypes.js');
const { collectPlugins, installPlugins } = await import('../src/handlers/pluginLoader.js');
const {
  clearHooks, registerHook, listHooks, runHooks, runGuildReady, runGuildAvailable, createReadyGate, createHookContext, DEFAULT_HOOK_TIMEOUT_MS,
} = await import('../src/handlers/hookRegistry.js');
const { HandlerMap, createHandlerMaps, registerCoreComponents } = await import('../src/handlers/componentRegistry.js');
const { buildCommandPayload, computeDeployHash } = await import('../src/handlers/commandLoader.js');
const { routeInteraction } = await import('../src/handlers/interactionRouter.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

// Silence the Redis/Prisma reconnect noise and the logger output of the failure-path tests.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug };
console.error = console.warn = console.log = console.debug = () => {};

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});
afterEach(() => {
  clearHooks();
  mock.timers.reset();
});

function walk(dir) {
  return readdirSync(dir).flatMap((entry) => {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) return walk(full);
    return entry.endsWith('.js') ? [full] : [];
  });
}

// ── 1. Module registry vs defaults vs schema ─────────────────────────────────

test('MODULES, DEFAULT_MODULE_SETTINGS and the schema agree', () => {
  assert.deepEqual(Object.keys(MODULES).sort(), Object.keys(DEFAULT_MODULE_SETTINGS).sort());
  const schema = readFileSync(join(ROOT, 'prisma/schema.prisma'), 'utf8');
  const model = schema.match(/model GuildSettings \{([\s\S]*?)\n\}/)[1];
  for (const key of MODULE_NAMES) {
    const mod = MODULES[key];
    assert.equal(mod.default, DEFAULT_MODULE_SETTINGS[key], `${key} default`);
    const decl = model.match(new RegExp(`\\n\\s+${mod.flag}\\s+Boolean\\s+@default\\((true|false)\\)`));
    assert.ok(decl, `${mod.flag} is declared in model GuildSettings as Boolean @default(...)`);
    assert.equal(decl[1] === 'true', mod.default, `${mod.flag} schema default`);
  }
  assert.deepEqual(MODULE_FLAGS, Object.fromEntries(MODULE_NAMES.map((k) => [k, MODULES[k].flag])));
  assert.equal(MODULE_FLAGS.leveling, 'xpEnabled');
});

// ── 2. isModuleEnabled ────────────────────────────────────────────────────────

test('isModuleEnabled', () => {
  assert.equal(isModuleEnabled(null, null), true, 'a null key is core');
  assert.equal(isModuleEnabled(null, 'moderation'), false, 'null settings');
  assert.equal(isModuleEnabled({ modEnabled: true }, 'nope'), false, 'unknown key');
  assert.equal(isModuleEnabled({ modEnabled: true }, 'moderation'), true);
  assert.equal(isModuleEnabled({ modEnabled: false }, 'moderation'), false);
});

// ── 3-5. Hook, customId and job registries ───────────────────────────────────

test('HOOK_ORDER has no duplicates and well-formed ids', () => {
  for (const [event, ids] of Object.entries(HOOK_ORDER)) {
    assert.equal(new Set(ids).size, ids.length, `${event}: duplicate hook id`);
    for (const id of ids) assert.match(id, /^[a-z0-9-]+(:[a-z0-9-]+)?$/, `${event}: ${id}`);
  }
});

test('CUSTOM_ID_PREFIXES are well-formed and buildCustomId enforces them', () => {
  for (const [prefix, info] of Object.entries(CUSTOM_ID_PREFIXES)) {
    assert.match(prefix, /^[a-z][a-z0-9-]*$/);
    assert.ok(['persistent', 'collector', 'field'].includes(info.kind), `${prefix}: kind`);
    for (const t of info.types) assert.ok(['button', 'select', 'modal', 'field'].includes(t), `${prefix}: type ${t}`);
  }
  assert.throws(() => buildCustomId('no-such-prefix'));
  assert.throws(() => buildCustomId('rr', 'x'.repeat(100)));
  assert.equal(buildCustomId('noop'), 'noop');
});

test('JOB_TYPES keys are well-formed and owned', () => {
  for (const [type, info] of Object.entries(JOB_TYPES)) {
    assert.match(type, /^[a-z][a-z0-9-]*\.[a-z][a-zA-Z0-9.-]*$/);
    assert.ok(typeof info.owner === 'string' && info.owner.length > 0, `${type}: owner`);
  }
});

// ── 6. Plugins ────────────────────────────────────────────────────────────────

test('collectPlugins() on src/plugins reports no errors (strict)', async () => {
  const { errors } = await collectPlugins();
  assert.deepEqual(errors, []);
});

function pluginDir(files) {
  const dir = mkdtempSync(join(tmpdir(), 'bot-plugins-'));
  for (const [name, body] of Object.entries(files)) writeFileSync(join(dir, name), body);
  return dir;
}

test('collectPlugins validates declarations and drops only the offending entry', async () => {
  const dir = pluginDir({
    // valid: a hook, a persistent component, a job and a provider that belong to it
    'giveaways.js': `export const plugin = {
      id: 'giveaways', module: 'giveaways',
      hooks: [
        { event: 'messageDelete', id: 'giveaways', run: async () => {} },
        { event: 'messageDelete', id: 'polls', run: async () => {} },             // another package's id
        { event: 'messageDelete', id: 'giveaways', run: async () => {} },         // duplicate
        { event: 'noSuchEvent', id: 'giveaways', run: async () => {} },
        { event: 'messageDeleteBulk', id: 'giveaways', run: 'not a function' },
      ],
      components: [
        { type: 'button', prefix: 'giveaway', handler: async () => {} },
        { type: 'select', prefix: 'giveaway', handler: async () => {} },          // registry lists button only
        { type: 'button', prefix: 'rr', handler: async () => {} },                // owned by reaction-roles
        { type: 'button', prefix: 'paginator', handler: async () => {} },         // collector, not persistent
      ],
      jobs: [
        { type: 'giveaway.end', handler: async () => {} },
        { type: 'poll.end', handler: async () => {} },                            // owned by polls
      ],
      provides: { xp: { grant() {}, take() {} }, gems: { grant() {}, take() {} } },
    };`,
    'wrongid.js': `export const plugin = { id: 'other', module: null };`,
    'badmodule.js': `export const plugin = { id: 'badmodule', module: 'nope' };`,
    'noexport.js': `export const x = 1;`,
    'broken.js': `throw new Error('boom');`,
    '_helper.js': `throw new Error('skipped files are never imported');`,
    'readme.txt': 'not javascript',
  });
  try {
    const { plugins, errors } = await collectPlugins({ dir });
    assert.deepEqual(plugins.map((p) => p.id), ['giveaways']);
    const [p] = plugins;
    assert.deepEqual(p.hooks.map((h) => `${h.event}:${h.id}`), ['messageDelete:giveaways']);
    assert.deepEqual(p.components.map((c) => `${c.type}:${c.prefix}`), ['button:giveaway']);
    assert.deepEqual(p.jobs.map((j) => j.type), ['giveaway.end']);
    assert.deepEqual(Object.keys(p.provides), ['xp']);
    const text = errors.join('\n');
    for (const expected of ['polls', 'duplicate hook', 'unknown event', 'run must be a function', 'rr', 'paginator', 'poll.end', 'gems', 'wrongid.js', 'badmodule.js', 'noexport.js', 'broken.js']) {
      assert.ok(text.includes(expected), `an error mentions ${expected}`);
    }
    assert.ok(!text.includes('_helper'), 'files starting with _ are skipped');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  assert.deepEqual(await collectPlugins({ dir: join(tmpdir(), 'bot-plugins-does-not-exist') }), { plugins: [], errors: [] });
});

test('installPlugins registers hooks and components, wraps module-gated components and reports errors', async () => {
  const dir = pluginDir({
    'polls.js': `export const plugin = {
      id: 'polls', module: 'polls',
      hooks: [{ event: 'messageDelete', id: 'polls', run: async () => {} }],
      components: [
        { type: 'button', prefix: 'poll', handler: async (i) => { i.handled = 'poll'; } },
        { type: 'select', prefix: 'poll-select', handler: async (i) => { i.handled = 'select'; }, module: null },
        { type: 'modal', prefix: 'poll-modal', handler: async () => {} },
      ],
      jobs: [{ type: 'poll.end', handler: async () => {} }],
      init: () => { throw new Error('init exploded'); },
    };`,
  });
  try {
    const collected = await collectPlugins({ dir });
    const client = {};
    createHandlerMaps(client);
    const result = installPlugins(client, collected);
    assert.equal(result.hooks, 1);
    assert.equal(result.components, 3);
    assert.equal(result.jobs, 1);
    assert.equal(result.errors.length, 1, 'init failure is recorded, nothing else is');
    assert.match(result.errors[0], /init exploded/);
    assert.deepEqual(listHooks('messageDelete'), [{ id: 'polls', pluginId: 'polls', module: 'polls', always: false }]);

    // `select` opted out of gating (module: null), so it is the raw handler; `button` is wrapped.
    assert.notEqual(client.buttonHandlers.get('poll'), undefined);
    const select = { inGuild: () => true, guildId: 'g' };
    await client.selectHandlers.get('poll-select')(select, client);
    assert.equal(select.handled, 'select');
    // A gated component outside a guild has no settings to check and passes straight through.
    const dm = { inGuild: () => false };
    await client.buttonHandlers.get('poll')(dm, client);
    assert.equal(dm.handled, 'poll');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// ── 7. Static customId scan ──────────────────────────────────────────────────

test('every literal customId prefix under src/ is registered', () => {
  const problems = [];
  const res = [/setCustomId\(\s*[`'"]([a-z][a-z0-9-]*)/g, /\bcustom_?[iI]d\s*:\s*[`'"]([a-z][a-z0-9-]*)/g];
  for (const file of walk(SRC)) {
    const text = readFileSync(file, 'utf8');
    for (const re of res) {
      for (const m of text.matchAll(re)) {
        if (!Object.hasOwn(CUSTOM_ID_PREFIXES, m[1])) {
          problems.push(`${rel(file)}:${text.slice(0, m.index).split('\n').length} unknown prefix "${m[1]}"`);
        }
      }
    }
  }
  assert.deepEqual(problems, []);
});

// ── 8. Command module exports ────────────────────────────────────────────────

test('command modules export valid module, gated, subcommandPermissions and allowInDMs when present', async () => {
  const levels = new Set(Object.values(PERMISSION_LEVELS));
  const problems = [];
  for (const file of walk(join(SRC, 'commands'))) {
    const mod = await load(file);
    const where = rel(file);
    if ('module' in mod && mod.module !== null && !Object.hasOwn(MODULES, mod.module)) problems.push(`${where}: module ${JSON.stringify(mod.module)}`);
    if ('gated' in mod && typeof mod.gated !== 'boolean') problems.push(`${where}: gated must be a boolean`);
    if ('allowInDMs' in mod && typeof mod.allowInDMs !== 'boolean') problems.push(`${where}: allowInDMs must be a boolean`);
    if ('permissionLevel' in mod && !levels.has(mod.permissionLevel)) problems.push(`${where}: permissionLevel`);
    for (const [sub, level] of Object.entries(mod.subcommandPermissions ?? {})) {
      if (!levels.has(level)) problems.push(`${where}: subcommandPermissions["${sub}"]`);
    }
  }
  assert.deepEqual(problems, []);
});

// ── 9. buildCommandPayload ───────────────────────────────────────────────────

test('buildCommandPayload makes commands guild-only unless allowInDMs', () => {
  const data = new SlashCommandBuilder().setName('probe').setDescription('probe');
  const guildOnly = buildCommandPayload({ data });
  assert.deepEqual(guildOnly.contexts, [0]);
  assert.deepEqual(guildOnly.integration_types, [0]);
  const dm = buildCommandPayload({ data, allowInDMs: true });
  assert.equal(dm.contexts, undefined);
  assert.equal(dm.integration_types, undefined);
});

test('the deploy hash covers the deploy target and ignores payload order (R270)', () => {
  const a = { name: 'a', description: 'a' };
  const b = { name: 'b', description: 'b' };
  const base = computeDeployHash('c1', undefined, [a, b]);
  assert.equal(base, computeDeployHash('c1', undefined, [b, a]), 'order');
  assert.equal(base, computeDeployHash('c1', '', [a, b]), 'no guild = global');
  assert.notEqual(base, computeDeployHash('c2', undefined, [a, b]), 'CLIENT_ID');
  assert.notEqual(base, computeDeployHash('c1', '123', [a, b]), 'DEPLOY_GUILD_ID');
  assert.notEqual(base, computeDeployHash('c1', undefined, [a]), 'payloads');
});

// ── 10. Hook registry ────────────────────────────────────────────────────────

const noModule = { pluginId: 'test', pluginModule: null };

test('hooks run in HOOK_ORDER regardless of registration order', async () => {
  const seen = [];
  for (const id of [...HOOK_ORDER.messageCreate].reverse()) {
    registerHook({ event: 'messageCreate', id, run: () => { seen.push(id); } }, noModule);
  }
  assert.deepEqual(listHooks('messageCreate').map((h) => h.id), [...HOOK_ORDER.messageCreate]);
  await runHooks('messageCreate', {});
  assert.deepEqual(seen, [...HOOK_ORDER.messageCreate]);
});

test('a throwing hook does not stop later hooks', async () => {
  const seen = [];
  registerHook({ event: 'messageCreate', id: 'automod', run: () => { throw new Error('sync boom'); } }, noModule);
  registerHook({ event: 'messageCreate', id: 'logging', run: async () => { throw new Error('async boom'); } }, noModule);
  registerHook({ event: 'messageCreate', id: 'levels', run: () => { seen.push('levels'); } }, noModule);
  const ctx = await runHooks('messageCreate', {});
  assert.deepEqual(seen, ['levels']);
  assert.equal(ctx.stopped, false);
});

test('ctx.stop() stops later hooks, and state is shared within a dispatch', async () => {
  const seen = [];
  registerHook({ event: 'messageCreate', id: 'automod', run: (ctx) => { ctx.state.deleted = true; ctx.stop('automod'); } }, noModule);
  registerHook({ event: 'messageCreate', id: 'levels', run: () => { seen.push('levels'); } }, noModule);
  const ctx = await runHooks('messageCreate', {});
  assert.deepEqual(seen, []);
  assert.equal(ctx.stopped, true);
  assert.equal(ctx.stopReason, 'automod');
  assert.equal(ctx.state.deleted, true);
});

test('a module-gated hook is skipped without settings, unless it is always or ungated', async () => {
  const seen = [];
  registerHook({ event: 'messageCreate', id: 'automod', run: () => { seen.push('gated'); } }, { pluginId: 'test', pluginModule: 'automod' });
  registerHook({ event: 'messageCreate', id: 'logging', always: true, run: () => { seen.push('always'); } }, { pluginId: 'test', pluginModule: 'logging' });
  registerHook({ event: 'messageCreate', id: 'levels', module: null, run: () => { seen.push('ungated'); } }, { pluginId: 'test', pluginModule: 'leveling' });
  await runHooks('messageCreate', { guildId: null }); // no guild: getSettings() resolves null
  assert.deepEqual(seen, ['always', 'ungated']);
});

test('a sync filter that returns false or throws skips the hook', async () => {
  const seen = [];
  registerHook({ event: 'messageCreate', id: 'automod', filter: () => false, run: () => { seen.push('no'); } }, noModule);
  registerHook({ event: 'messageCreate', id: 'logging', filter: () => { throw new Error('bad filter'); }, run: () => { seen.push('no'); } }, noModule);
  registerHook({ event: 'messageCreate', id: 'levels', filter: (ctx) => ctx.flag === 1, run: () => { seen.push('levels'); } }, noModule);
  await runHooks('messageCreate', { flag: 1 });
  assert.deepEqual(seen, ['levels']);
});

test('a hook that exceeds its timeoutMs is abandoned and the next hook runs', async () => {
  const seen = [];
  registerHook({ event: 'messageCreate', id: 'automod', timeoutMs: 50, run: () => new Promise(() => {}) }, noModule);
  registerHook({ event: 'messageCreate', id: 'logging', run: () => { seen.push('logging'); } }, noModule);
  const started = Date.now();
  await runHooks('messageCreate', {});
  assert.deepEqual(seen, ['logging']);
  assert.ok(Date.now() - started < DEFAULT_HOOK_TIMEOUT_MS, 'the soft timeout, not the default, applied');
});

test('the context core keys cannot be overridden by fields', () => {
  const ctx = createHookContext('messageCreate', { event: 'x', state: 'x', stopped: true, guildId: 'g1', extra: 1 });
  assert.equal(ctx.event, 'messageCreate');
  assert.equal(typeof ctx.state, 'object');
  assert.equal(ctx.stopped, false);
  assert.equal(ctx.guildId, 'g1');
  assert.equal(ctx.extra, 1);
  assert.equal(createHookContext('e', { guild: { id: 'g2' } }).guildId, 'g2');
});

test('registerHook refuses a duplicate (event, id)', () => {
  registerHook({ event: 'messageCreate', id: 'automod', run: () => {} }, noModule);
  registerHook({ event: 'messageCreate', id: 'automod', run: () => {} }, noModule);
  assert.equal(listHooks('messageCreate').length, 1);
});

// ── 11. HandlerMap ───────────────────────────────────────────────────────────

test('HandlerMap refuses a second, different handler for the same key', () => {
  const map = new HandlerMap('button');
  const a = () => {};
  const b = () => {};
  map.set('x', a);
  map.set('x', b);
  assert.equal(map.get('x'), a);
  map.set('x', a); // the same handler again is fine
  assert.equal(map.size, 1);
});

test('registerCoreComponents registers noop once and is idempotent', async () => {
  const client = {};
  createHandlerMaps(client);
  registerCoreComponents(client);
  registerCoreComponents(client);
  assert.ok(client.buttonHandlers instanceof HandlerMap && client.selectHandlers instanceof HandlerMap && client.modalHandlers instanceof HandlerMap);
  let deferred = 0;
  await client.buttonHandlers.get('noop')({ deferUpdate: async () => { deferred++; } });
  assert.equal(deferred, 1);
});

// ── 12. No bare now() in raw SQL ─────────────────────────────────────────────

test('no raw SQL template under src/ uses a bare now() or CURRENT_TIMESTAMP', () => {
  const RAW = /(?:\$queryRaw|\$executeRaw|Prisma\.sql)\s*`([\s\S]*?)`/g;
  const BARE = /(?<![\w.])(?:now\(\)(?!\s*AT\s+TIME\s+ZONE\s+'UTC')|CURRENT_TIMESTAMP)/i;
  const problems = [];
  for (const file of walk(SRC)) {
    if (rel(file) === 'src/database/client.js') continue; // the SQL_NOW definition itself
    const text = readFileSync(file, 'utf8');
    for (const m of text.matchAll(RAW)) {
      if (BARE.test(m[1])) problems.push(`${rel(file)}:${text.slice(0, m.index).split('\n').length}`);
    }
  }
  assert.deepEqual(problems, [], 'use SQL_NOW from src/database/client.js');
});

// ── 13. Event files ──────────────────────────────────────────────────────────

const EVENT_FILES = walk(join(SRC, 'events'));

test('every event file exports { name, once, execute } and guildAvailable.js and userUpdate.js exist', async () => {
  const names = EVENT_FILES.map((f) => rel(f));
  assert.ok(names.includes('src/events/guildAvailable.js'));
  assert.ok(names.includes('src/events/userUpdate.js'));
  for (const file of EVENT_FILES) {
    const mod = await load(file);
    assert.equal(typeof mod.name, 'string', `${rel(file)}: name`);
    assert.equal(typeof mod.once, 'boolean', `${rel(file)}: once`);
    assert.equal(typeof mod.execute, 'function', `${rel(file)}: execute`);
  }
  const unique = new Set();
  for (const file of EVENT_FILES) unique.add((await load(file)).name);
  assert.equal(unique.size, EVENT_FILES.length, 'one file per gateway event');
});

test('every gateway hook point is dispatched by an event file, and event files import no feature code', () => {
  const texts = EVENT_FILES.map((f) => readFileSync(f, 'utf8'));
  const all = texts.join('\n');
  const synthetic = new Set([...INTERNAL_HOOK_POINTS, ...SYNTHETIC_HOOK_POINTS]);
  for (const event of Object.keys(HOOK_ORDER)) {
    if (synthetic.has(event)) continue;
    assert.ok(all.includes(`runHooks('${event}'`), `no event file runs runHooks('${event}'`);
  }
  const allowed = new Set([
    'discord.js', '../handlers/hookRegistry.js', '../handlers/interactionRouter.js', '../services/settingsService.js',
    '../services/scheduler.js', '../services/redis.js', '../config/jobTypes.js', '../utils/async.js', '../utils/logger.js',
  ]);
  const problems = [];
  EVENT_FILES.forEach((file, i) => {
    for (const m of texts[i].matchAll(/from\s+'([^']+)'/g)) if (!allowed.has(m[1])) problems.push(`${rel(file)} imports ${m[1]}`);
  });
  assert.deepEqual(problems, []);
});

function recordHook(event) {
  const calls = [];
  registerHook({ event, id: HOOK_ORDER[event][0], module: null, run: (ctx) => { calls.push(ctx); } }, noModule);
  return calls;
}

test('messageCreate only dispatches user messages (R262, R263)', async () => {
  const { execute } = await load(join(SRC, 'events', 'messageCreate.js'));
  const calls = recordHook('messageCreate');
  const guild = { id: 'g1' };
  const msg = (over) => ({
    inGuild: () => true, guild, member: {}, channel: {}, author: { bot: false }, webhookId: null, type: MessageType.Default, ...over,
  });
  await execute(msg(), {});
  await execute(msg({ type: MessageType.Reply }), {});
  assert.equal(calls.length, 2);
  await execute(msg({ inGuild: () => false }), {});
  await execute(msg({ author: { bot: true } }), {});
  await execute(msg({ webhookId: '1' }), {});
  await execute(msg({ type: MessageType.UserJoin }), {});
  await execute(msg({ type: MessageType.ChannelPinnedMessage }), {});
  await execute(msg({ type: MessageType.ThreadCreated }), {});
  assert.equal(calls.length, 2, 'bots, webhooks, DMs and system messages never enter the pipeline');
  assert.equal(calls[0].guildId, 'g1');
});

test('messageUpdate only dispatches real edits (R120, R121)', async () => {
  const { execute } = await load(join(SRC, 'events', 'messageUpdate.js'));
  const calls = recordHook('messageUpdate');
  const base = { guildId: 'g1', guild: { id: 'g1' }, partial: false, inGuild: () => true, author: { bot: false }, webhookId: null, member: {}, channel: {} };
  await execute({ partial: false, content: 'a' }, { ...base, content: 'a' }, {});          // pin or unfurl
  assert.equal(calls.length, 0);
  await execute({ partial: false, content: 'a' }, { ...base, content: 'b' }, {});          // real edit
  assert.equal(calls.length, 1);
  assert.equal(calls[0].oldKnown, true);
  await execute({ partial: true }, { ...base, content: 'b', editedTimestamp: Date.now() - 1000 }, {});
  assert.equal(calls.length, 2);
  assert.equal(calls[1].oldKnown, false);
  await execute({ partial: true }, { ...base, content: 'b', editedTimestamp: null }, {});   // nothing says it was edited
  await execute({ partial: true }, { ...base, content: 'b', editedTimestamp: Date.now() - 60_000 }, {});
  assert.equal(calls.length, 2);

  // A partial new message (uncached) is fetched only when its editedTimestamp says it was just edited.
  let fetches = 0;
  const partialMsg = (editedTimestamp) => ({
    ...base, partial: true, editedTimestamp, fetch: async () => { fetches++; return { ...base, content: 'c', editedTimestamp }; },
  });
  await execute({ partial: true }, partialMsg(null), {});
  await execute({ partial: true }, partialMsg(Date.now() - 60_000), {});
  assert.equal(fetches, 0, 'not an edit: no REST fetch');
  await execute({ partial: true }, partialMsg(Date.now() - 1000), {});
  assert.equal(fetches, 1);
  assert.equal(calls.length, 3);
});

test('thread events dispatch their hook points and skip threads without a guild', async () => {
  const guild = { id: 'g1' };
  const created = recordHook('threadCreate');
  const updated = recordHook('threadUpdate');
  const deleted = recordHook('threadDelete');
  const eventFile = (name) => load(join(SRC, 'events', `${name}.js`));

  const create = await eventFile('threadCreate');
  await create.execute({ guild, id: 't1' }, true, {});
  await create.execute({ guild, id: 't2' }, false, {}); // the bot was added to an existing thread
  assert.deepEqual(created.map((c) => c.thread.id), ['t1']);

  const update = await eventFile('threadUpdate');
  assert.equal(update.name, 'threadUpdate');
  await update.execute({ id: 't1', guild, name: 'a' }, { id: 't1', guild, name: 'b' }, {});
  await update.execute({ id: 't1' }, { id: 't1', guild: null }, {});
  assert.equal(updated.length, 1);
  assert.equal(updated[0].oldThread.name, 'a');
  assert.equal(updated[0].newThread.name, 'b');
  assert.equal(updated[0].thread, updated[0].newThread);
  assert.equal(updated[0].guildId, 'g1');

  const del = await eventFile('threadDelete');
  assert.equal(del.name, 'threadDelete');
  await del.execute({ id: 't1', guild }, {});
  await del.execute({ id: 't2', guild: null }, {});
  assert.deepEqual(deleted.map((c) => c.thread.id), ['t1']);
});

test('loadCommands fills client.commandCategories with each command\'s folder (/games and /help group by it)', async () => {
  const { loadCommands } = await import('../src/handlers/commandLoader.js');
  const client = { commands: new Map(), commandCategories: new Map() };
  createHandlerMaps(client);
  await loadCommands(client);
  assert.ok(client.commands.size > 30, 'the real command tree was loaded');
  for (const name of client.commands.keys()) assert.equal(typeof client.commandCategories.get(name), 'string', `${name} has a category`);
  assert.equal(client.commandCategories.get('rps'), 'games');
  assert.equal(client.commandCategories.get('slots'), 'games');
  assert.equal(client.commandCategories.get('ban'), 'moderation');
  // src/bot.js creates the map the loader fills.
  assert.match(readFileSync(join(SRC, 'bot.js'), 'utf8'), /client\.commandCategories\s*=\s*new Map\(\)/);
});

test('client.shutdown() stops the scheduler, flushes the log queue (bounded) and only then destroys the client', () => {
  // src/bot.js logs in at import, so the order is pinned on the source of the shutdown hook.
  const text = readFileSync(join(SRC, 'bot.js'), 'utf8');
  const start = text.indexOf('client.shutdown = async () => {');
  assert.ok(start > 0, 'client.shutdown is defined');
  const body = text.slice(start, text.indexOf('\n};', start));
  const at = (needle) => {
    const i = body.indexOf(needle);
    assert.ok(i >= 0, `shutdown calls ${needle}`);
    return i;
  };
  assert.ok(at('stopScheduler(') < at('flushLogQueue()'));
  assert.ok(at('flushLogQueue()') < at('client.destroy()'), 'queued audit logs are posted while the client can still send');
  assert.match(body, /withTimeout\(flushLogQueue\(\), LOG_FLUSH_TIMEOUT_MS/, 'the flush is bounded');
  assert.match(text, /const LOG_FLUSH_TIMEOUT_MS = \d{3,5};/);
  assert.match(text, /import \{ flushLogQueue \} from '\.\/services\/loggingService\.js';/);
});

// ── 14. Ordering guarantee ───────────────────────────────────────────────────

test('the first always/ungated hook is invoked synchronously by runHooks (ordering guarantee)', () => {
  const record = [];
  registerHook({ event: 'guildMemberAdd', id: 'invites', always: true, run: () => { record.push('ran'); } }, { pluginId: 'test', pluginModule: 'invites' });
  const pending = runHooks('guildMemberAdd', { guildId: null }); // deliberately NOT awaited
  assert.deepEqual(record, ['ran'], 'the hook ran before runHooks returned its promise');
  return pending;
});

// ── 15. Unavailable guilds ───────────────────────────────────────────────────

test('runGuildReady skips an unavailable guild', async () => {
  const seen = [];
  registerHook({ event: 'guildReady', id: 'invites', module: null, run: () => { seen.push('ran'); } }, noModule);
  await runGuildReady({}, { id: 'g', available: false }, 'startup');
  assert.deepEqual(seen, []);
});

test('guildAvailable gate: one re-run per guild per cooldown, a startup run counts, and the cap holds', async () => {
  const gate = createReadyGate({ concurrency: 1, cooldownMs: 60_000 });
  let runs = 0;
  assert.equal(await gate.run('g1', () => { runs++; }), true);
  assert.equal(await gate.run('g1', () => { runs++; }), false, 'inside the cooldown');
  gate.record('g2'); // what runGuildReady does for a startup or join run
  assert.equal(await gate.run('g2', () => { runs++; }), false);
  assert.equal(runs, 1);

  let release;
  const order = [];
  const first = gate.run('a', () => new Promise((resolve) => { order.push('a start'); release = resolve; }));
  const second = gate.run('b', () => { order.push('b start'); });
  await new Promise((resolve) => { setImmediate(resolve); });
  assert.deepEqual(order, ['a start'], 'b waits for the only slot');
  release();
  assert.deepEqual(await Promise.all([first, second]), [true, true]);
  assert.deepEqual(order, ['a start', 'b start']);

  assert.equal(await runGuildAvailable({}, { id: 'g-off', available: false }), false, 'an unavailable guild never runs');
});

// ── Router ───────────────────────────────────────────────────────────────────

function fakeInteraction(over = {}) {
  const replies = [];
  return {
    replies,
    replied: false,
    deferred: false,
    isChatInputCommand: () => false,
    isAutocomplete: () => false,
    isButton: () => false,
    isAnySelectMenu: () => false,
    isModalSubmit: () => false,
    reply: async (payload) => { replies.push(payload); },
    editReply: async (payload) => { replies.push(payload); },
    user: { id: 'u1' },
    ...over,
  };
}

const chat = (command, over = {}) => fakeInteraction({
  isChatInputCommand: () => true,
  commandName: command,
  options: { getSubcommand: () => null, getSubcommandGroup: () => null },
  ...over,
});
const routerClient = (commands = {}) => {
  const client = { commands: new Map(Object.entries(commands)) };
  createHandlerMaps(client);
  return client;
};
const titleOf = (reply) => reply.embeds?.[0]?.data?.title ?? reply.content;

test('the router fails closed outside a cached guild (R123, R124, R271)', async () => {
  let ran = 0;
  const cmd = { execute: async () => { ran++; }, permissionLevel: PERMISSION_LEVELS.ADMIN };
  const client = routerClient({ ban: cmd, ping: { execute: async () => { ran++; }, allowInDMs: true } });

  const dm = chat('ban', { inGuild: () => false, inCachedGuild: () => false, guildId: null });
  await routeInteraction(dm, client);
  assert.match(titleOf(dm.replies[0]), /Server Only/);

  const uncached = chat('ban', { inGuild: () => true, inCachedGuild: () => false, guildId: 'g1' });
  await routeInteraction(uncached, client);
  assert.match(titleOf(uncached.replies[0]), /Bot Not In Server/);
  assert.equal(ran, 0);

  // A DM-safe command runs in a DM...
  await routeInteraction(chat('ping', { inGuild: () => false, inCachedGuild: () => false, guildId: null }), client);
  assert.equal(ran, 1);
  // ...but one that also needs a permission level cannot prove it there.
  client.commands.set('both', { execute: async () => { ran++; }, allowInDMs: true, permissionLevel: PERMISSION_LEVELS.MODERATOR });
  const both = chat('both', { inGuild: () => false, inCachedGuild: () => false, guildId: null });
  await routeInteraction(both, client);
  assert.match(titleOf(both.replies[0]), /Server Only/);
  assert.equal(ran, 1);
});

test('the router runs an ungated MEMBER command without loading settings, and reports unknown commands', async () => {
  let ran = 0;
  const client = routerClient({ hello: { execute: async () => { ran++; } } });
  await routeInteraction(chat('hello', { inGuild: () => true, inCachedGuild: () => true, guildId: 'g-router' }), client);
  assert.equal(ran, 1);
  const unknown = chat('nope', { inGuild: () => true, inCachedGuild: () => true, guildId: 'g-router' });
  await routeInteraction(unknown, client);
  assert.match(titleOf(unknown.replies[0]), /Unknown Command/);
});

test('a gated command is refused with a friendly notice when settings cannot be loaded', async () => {
  let ran = 0;
  const client = routerClient({ slots: { module: 'games', execute: async () => { ran++; } } });
  const i = chat('slots', { inGuild: () => true, inCachedGuild: () => true, guildId: 'g-router-offline' });
  await routeInteraction(i, client); // Postgres is unreachable: bounded by 1500 ms, then null
  assert.equal(ran, 0);
  assert.match(titleOf(i.replies[0]), /Temporarily Unavailable/);
});

test('the router replies at once to components nobody handles, and gives collectors a grace period (R272)', async () => {
  const client = routerClient();
  const button = fakeInteraction({ isButton: () => true, customId: 'unknown-prefix:1' });
  await routeInteraction(button, client);
  assert.equal(button.replies[0].content, 'This button is no longer active.');
  assert.equal(button.replies[0].ephemeral, true);

  const select = fakeInteraction({ isAnySelectMenu: () => true, customId: 'unknown-prefix:1' });
  await routeInteraction(select, client);
  assert.equal(select.replies[0].content, 'This menu is no longer active.');

  const modal = fakeInteraction({ isModalSubmit: () => true, customId: 'unknown-prefix:1' });
  await routeInteraction(modal, client);
  assert.equal(modal.replies[0].content, 'This form has expired. Please run the command again.');

  mock.timers.enable({ apis: ['setTimeout'] });
  const collector = fakeInteraction({ isButton: () => true, customId: 'rps:123:rock' }); // a collector-only prefix
  await routeInteraction(collector, client);
  assert.equal(collector.replies.length, 0, 'the live collector gets the first chance');
  mock.timers.tick(2500);
  assert.equal(collector.replies[0].content, 'This menu has expired.');

  const answered = fakeInteraction({ isButton: () => true, customId: 'rps:123:rock' });
  await routeInteraction(answered, client);
  answered.deferred = true; // the collector acknowledged it in time
  mock.timers.tick(2500);
  assert.equal(answered.replies.length, 0);
});

test('the router routes components by prefix, selects of every kind, and survives a failing handler', async () => {
  const client = routerClient();
  const seen = [];
  client.buttonHandlers.set('noop', (i) => { seen.push(`button:${i.customId}`); });
  client.selectHandlers.set('rr-select', (i) => { seen.push(`select:${i.customId}`); });
  client.modalHandlers.set('welcome-setup', () => { throw new Error('handler boom'); });

  await routeInteraction(fakeInteraction({ isButton: () => true, customId: 'noop:a:b' }), client);
  await routeInteraction(fakeInteraction({ isAnySelectMenu: () => true, customId: 'rr-select:g' }), client); // e.g. a user select
  assert.deepEqual(seen, ['button:noop:a:b', 'select:rr-select:g']);

  const failing = fakeInteraction({ isModalSubmit: () => true, customId: 'welcome-setup' });
  await routeInteraction(failing, client);
  assert.match(titleOf(failing.replies[0]), /Modal Error/);
});

// ── Autocomplete gating ──────────────────────────────────────────────────────

const autocompleteOf = (command, over = {}) => {
  const i = fakeInteraction({
    isAutocomplete: () => true,
    commandName: command,
    guildId: 'g-ac',
    inGuild: () => true,
    inCachedGuild: () => true,
    options: { getSubcommand: () => null, getSubcommandGroup: () => null, getFocused: () => '' },
    member: { id: 'member-1', permissions: { has: () => false }, roles: { cache: new Map() }, guild: { ownerId: 'someone-else' } },
    responses: [],
    respond: async (choices) => { i.responses.push(choices); },
    ...over,
  });
  return i;
};
const adminMember = () => ({
  id: 'admin-1', roles: { cache: new Map() }, guild: { ownerId: 'someone-else' },
  permissions: { has: (bit) => bit === PermissionFlagsBits.Administrator },
});
/** Replaces the GuildSettings model with a stub (the router loads settings for any level above MEMBER); returns the restorer. */
function stubSettings(extra = () => ({})) {
  const real = prisma.guildSettings;
  prisma.guildSettings = {
    findUnique: async ({ where }) => ({ guildId: where.guildId, modRoleIds: [], ...extra() }),
    upsert: async ({ create }) => ({ ...create, ...extra() }),
  };
  return () => { prisma.guildSettings = real; };
}

test('autocomplete is gated like execute: a MEMBER gets [] from an ADMIN command, an ADMIN gets suggestions', async () => {
  let handled = 0;
  const choices = [{ name: 'secret role', value: 'r1' }];
  const client = routerClient({
    config: { permissionLevel: PERMISSION_LEVELS.ADMIN, execute() {}, autocomplete: async (i) => { handled++; await i.respond(choices); } },
    open: { execute() {}, autocomplete: async (i) => { handled++; await i.respond(choices); } },
  });
  const restore = stubSettings();
  try {
    const denied = autocompleteOf('config');
    await routeInteraction(denied, client);
    assert.deepEqual(denied.responses, [[]], 'a MEMBER sees no admin-only suggestions');
    assert.equal(handled, 0, 'the command handler never runs for a refused caller');

    const allowed = autocompleteOf('config', { member: adminMember() });
    await routeInteraction(allowed, client);
    assert.deepEqual(allowed.responses, [choices], 'positive control: an ADMIN reaches the handler');
    assert.equal(handled, 1);

    const open = autocompleteOf('open');
    await routeInteraction(open, client);
    assert.deepEqual(open.responses, [choices], 'positive control: a MEMBER command still autocompletes for a MEMBER');
    assert.equal(handled, 2);
  } finally {
    restore();
  }
});

test('autocomplete honours subcommandPermissions (group + subcommand beats subcommand beats the command level)', async () => {
  let handled = 0;
  const client = routerClient({
    mixed: {
      execute() {},
      subcommandPermissions: { setup: PERMISSION_LEVELS.ADMIN, 'danger wipe': PERMISSION_LEVELS.ADMIN },
      autocomplete: async (i) => { handled++; await i.respond([{ name: 'x', value: 'x' }]); },
    },
  });
  const optionsOf = (sub, group = null) => ({ getSubcommand: () => sub, getSubcommandGroup: () => group, getFocused: () => '' });
  const restore = stubSettings();
  try {
    const setup = autocompleteOf('mixed', { options: optionsOf('setup') });
    await routeInteraction(setup, client);
    assert.deepEqual(setup.responses, [[]]);
    const wipe = autocompleteOf('mixed', { options: optionsOf('wipe', 'danger') });
    await routeInteraction(wipe, client);
    assert.deepEqual(wipe.responses, [[]]);
    assert.equal(handled, 0);

    const list = autocompleteOf('mixed', { options: optionsOf('list') });
    await routeInteraction(list, client);
    assert.deepEqual(list.responses, [[{ name: 'x', value: 'x' }]], 'a MEMBER-level subcommand stays open');
    const adminSetup = autocompleteOf('mixed', { options: optionsOf('setup'), member: adminMember() });
    await routeInteraction(adminSetup, client);
    assert.equal(adminSetup.responses[0].length, 1);
    assert.equal(handled, 2);
  } finally {
    restore();
  }
});

test('autocomplete of a command in a disabled module gets [] (and runs once the module is on)', async () => {
  let gamesEnabled = false;
  const restore = stubSettings(() => ({ gamesEnabled }));
  try {
    let handled = 0;
    const client = routerClient({
      slots: { module: 'games', execute() {}, autocomplete: async (i) => { handled++; await i.respond([{ name: 'bet', value: '10' }]); } },
      ungated: { module: 'games', gated: false, execute() {}, autocomplete: async (i) => { handled++; await i.respond([{ name: 'ok', value: 'ok' }]); } },
    });

    const off = autocompleteOf('slots', { guildId: 'g-ac-games-off' });
    await routeInteraction(off, client);
    assert.deepEqual(off.responses, [[]], 'the module is off: no suggestions');
    assert.equal(handled, 0);
    const exempt = autocompleteOf('ungated', { guildId: 'g-ac-games-off' });
    await routeInteraction(exempt, client);
    assert.equal(exempt.responses[0][0].value, 'ok', 'gated: false commands are not module-gated, for autocomplete either');

    gamesEnabled = true;
    const on = autocompleteOf('slots', { guildId: 'g-ac-games-on' });
    await routeInteraction(on, client);
    assert.deepEqual(on.responses, [[{ name: 'bet', value: '10' }]], 'positive control: module on, the handler runs');
    assert.equal(handled, 2);
  } finally {
    restore();
  }
});

test('autocomplete answers [] when settings cannot be loaded for a gated command, and never throws', async () => {
  const client = routerClient({
    slots: { module: 'games', execute() {}, autocomplete: async (i) => { await i.respond([{ name: 'bet', value: '10' }]); } },
    boom: { execute() {}, autocomplete: async () => { throw new Error('autocomplete boom'); } },
  });
  const start = Date.now();
  const offline = autocompleteOf('slots', { guildId: 'g-ac-offline' }); // Postgres is unreachable: bounded, then null
  await routeInteraction(offline, client);
  assert.deepEqual(offline.responses, [[]]);
  assert.ok(Date.now() - start < 2500, 'the gate stays inside the 3 s answer window');

  const failing = autocompleteOf('boom');
  await assert.doesNotReject(routeInteraction(failing, client)); // logged, not thrown
});

test('autocomplete outside a cached guild answers [] unless the command is DM-safe', async () => {
  let handled = 0;
  const autocomplete = async (i) => { handled++; await i.respond([{ name: 'x', value: 'x' }]); };
  const client = routerClient({
    guildOnly: { execute() {}, autocomplete },
    dmSafe: { execute() {}, allowInDMs: true, autocomplete },
    dmAdmin: { execute() {}, allowInDMs: true, permissionLevel: PERMISSION_LEVELS.MODERATOR, autocomplete },
  });
  const dm = { inGuild: () => false, inCachedGuild: () => false, guildId: null, member: null };
  const a = autocompleteOf('guildOnly', dm);
  await routeInteraction(a, client);
  const uncached = autocompleteOf('guildOnly', { inCachedGuild: () => false });
  await routeInteraction(uncached, client);
  const c = autocompleteOf('dmAdmin', dm);
  await routeInteraction(c, client);
  assert.deepEqual([a.responses, uncached.responses, c.responses], [[[]], [[]], [[]]]);
  assert.equal(handled, 0);
  const ok = autocompleteOf('dmSafe', dm);
  await routeInteraction(ok, client);
  assert.equal(ok.responses[0].length, 1);
});

// ── /module and /help ────────────────────────────────────────────────────────

function fakeAutocomplete(sub, focused) {
  const interaction = {
    guildId: 'g-offline', responded: false, choices: null,
    options: { getSubcommand: () => sub, getFocused: () => focused },
    respond: async (choices) => { interaction.choices = choices; },
  };
  return interaction;
}

test('/module is a core admin command and its autocomplete lists at most 25 modules by key', async () => {
  const mod = await load(join(SRC, 'commands', 'admin', 'module.js'));
  assert.equal(mod.module, null);
  assert.equal(mod.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(MODULE_NAMES.length, 27);

  const all = fakeAutocomplete('enable', '');
  await mod.autocomplete(all); // settings cannot load offline: plain MODULE_NAMES order, no state suffix
  assert.equal(all.choices.length, 25);
  assert.deepEqual(all.choices.map((c) => c.value), MODULE_NAMES.slice(0, 25));
  assert.ok(all.choices.every((c) => c.name.length <= 100));

  const filtered = fakeAutocomplete('disable', 'AUDIT'); // matches the label of `logging`
  await mod.autocomplete(filtered);
  assert.deepEqual(filtered.choices.map((c) => c.value), ['logging']);
});

test('/help lists custom commands as /tag use when the prefix is off, and as prefix commands otherwise', async () => {
  const mod = await load(join(SRC, 'commands', 'utility', 'help.js'));
  const realCustomTag = prisma.customTag;
  const queries = [];
  prisma.customTag = {
    findMany: async (q) => {
      queries.push(q);
      return [{ name: 'rules', description: 'Server rules' }, { name: 'faq', description: null }];
    },
  };
  try {
    const settings = (over) => ({ customCommandsEnabled: true, customPrefix: '!', ...over });

    const on = await mod.customCommandGroup('g1', settings());
    assert.deepEqual(on.lines, ['`!rules` — Server rules', '`!faq` — custom command']);
    const custom = await mod.customCommandGroup('g1', settings({ customPrefix: '>>' }));
    assert.equal(custom.lines[0], '`>>rules` — Server rules');

    const off = await mod.customCommandGroup('g1', settings({ customPrefix: '' })); // /settings prefix off
    assert.deepEqual(off.lines, ['`/tag use name:rules` — Server rules', '`/tag use name:faq` — custom command']);
    assert.ok(!off.lines.some((l) => /^`rules`/.test(l)), 'never a bare name');

    // A hand-edited, invalid prefix behaves like the default (effectivePrefix).
    assert.equal((await mod.customCommandGroup('g1', settings({ customPrefix: 'two words' }))).lines[0], '`!rules` — Server rules');

    const before = queries.length;
    assert.equal(await mod.customCommandGroup('g1', null), null, 'unknown settings: nothing is listed');
    assert.equal(await mod.customCommandGroup('g1', settings({ customCommandsEnabled: false })), null, 'module off: nothing is listed');
    assert.equal(queries.length, before, 'no query when there is nothing to list');

    // A member only sees the commands their roles can run; no viewer (an admin) sees them all.
    prisma.customTag = {
      findMany: async () => [{ name: 'rules', description: null, allowRoleIds: [], denyRoleIds: [] },
        { name: 'staff', description: null, allowRoleIds: ['r-staff'], denyRoleIds: [] }],
    };
    const viewer = (ids) => ({ roles: { cache: new Map(ids.map((id) => [id, { id }])) } });
    assert.deepEqual((await mod.customCommandGroup('g1', settings(), viewer([]))).lines, ['`!rules` — custom command']);
    assert.equal((await mod.customCommandGroup('g1', settings(), viewer(['r-staff']))).lines.length, 2);
    assert.equal((await mod.customCommandGroup('g1', settings())).lines.length, 2);

    prisma.customTag = { findMany: async () => [] };
    assert.equal(await mod.customCommandGroup('g1', settings({ customPrefix: '' })), null, 'no custom commands: no group');
  } finally {
    prisma.customTag = realCustomTag;
  }
  assert.equal(mod.customCommandLine({ name: 'x', description: 'd'.repeat(300) }, null).length, '`/tag use name:x` — '.length + 100);

  // Core must not import the feature package, so /help keeps its own copy of the prefix rule: it must not drift.
  const { effectivePrefix } = await import('../src/services/tagService.js');
  for (const p of ['', '!', '?', '>>', '12345', '123456', 'two words', ' ', '\t!', 'é', '😀😀', undefined, null, 5, {}]) {
    assert.equal(mod.customPrefixOf(p), effectivePrefix(p), `prefix ${JSON.stringify(p)}`);
  }
  assert.doesNotMatch(readFileSync(join(SRC, 'commands', 'utility', 'help.js'), 'utf8'), /services\/tagService/, 'core imports no feature package');
});

test('/help lists only what the caller may use and never hides a command because settings are missing', async () => {
  const mod = await load(join(SRC, 'commands', 'utility', 'help.js'));
  assert.equal(mod.module, null);
  assert.equal(mod.permissionLevel, PERMISSION_LEVELS.MEMBER);

  const command = (over) => ({ data: { description: 'd' }, execute() {}, ...over });
  const client = {
    commands: new Map([
      ['hello', command({})],
      ['slots', command({ module: 'games' })],
      ['ban', command({ permissionLevel: PERMISSION_LEVELS.MODERATOR })],
      ['config', command({ permissionLevel: PERMISSION_LEVELS.ADMIN })],
      ['mixed', command({ subcommandPermissions: { setup: PERMISSION_LEVELS.ADMIN } })], // visible: some part is MEMBER
    ]),
    commandCategories: new Map(),
  };
  const member = { id: 'u1', permissions: { has: () => false }, roles: { cache: new Map() }, guild: { ownerId: 'someone-else' } };
  const i = fakeAutocomplete(null, '');
  Object.assign(i, { member });
  await mod.autocomplete(i, client);
  assert.deepEqual(i.choices.map((c) => c.value), ['hello', 'mixed', 'slots']);
});
