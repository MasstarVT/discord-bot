// Strict cross-package integration tests. They turn the planning registries (modules, hook order, customId
// prefixes, job types), the repo-wide rules and the docs into hard failures when a package disagrees with them.
// Offline: no Discord, Postgres, Redis or network. Run with `npm test`.
//
// Order matters (one process, shared state). Sections A-K read *declarations* (the raw `plugin` objects).
// installPlugins() runs exactly once, in section Z, which therefore stays last.
import { test, describe, before, after, mock } from 'node:test';
import assert from 'node:assert/strict';
import { readdirSync, readFileSync, statSync, existsSync } from 'node:fs';
import { join, resolve, dirname, relative, basename } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { EventEmitter } from 'node:events';
import dns from 'node:dns';
import net from 'node:net';
import { syncBuiltinESMExports } from 'node:module';

// ── Offline environment ──────────────────────────────────────────────────────
// Forced (not ??=): a developer's shell must never point the tests at real services.
Object.assign(process.env, {
  NODE_ENV: 'test',
  DISCORD_TOKEN: 'test-token',
  CLIENT_ID: '000000000000000000',
  DATABASE_URL: 'postgresql://test:test@127.0.0.1:1/test',
  REDIS_URL: 'redis://127.0.0.1:1',
});
for (const key of ['BOT_OWNER_ID', 'DEPLOY_GUILD_ID', 'TRANSLATE_PROVIDER', 'LIBRETRANSLATE_URL',
  'LIBRETRANSLATE_API_KEY', 'HTTP_CONTACT', 'REDDIT_USER_AGENT', 'PRISMA_LOG_QUERIES']) delete process.env[key];

// ── Network guard: loopback only (Redis/Postgres at 127.0.0.1:1 fail fast) ──
const LOOPBACK = new Set(['127.0.0.1', '::1', 'localhost']);
const offline = (what) => Object.assign(new Error(`Network access is disabled in tests (${what})`), { code: 'ENETDISABLED' });
const realLookup = dns.lookup;
dns.lookup = function lookup(host, options, callback) {
  if (typeof options === 'function') { callback = options; options = {}; }
  if (LOOPBACK.has(host)) return realLookup.call(dns, host, options, callback);
  process.nextTick(callback, offline(host));
  return undefined;
};
const realPromisesLookup = dns.promises.lookup;
dns.promises.lookup = async function lookup(host, options) {
  if (LOOPBACK.has(host)) return realPromisesLookup.call(dns.promises, host, options);
  throw offline(host);
};
const realConnect = net.Socket.prototype.connect;
net.Socket.prototype.connect = function connect(...args) {
  const first = Array.isArray(args[0]) ? args[0][0] : args[0];
  const opts = first && typeof first === 'object' ? first : { port: first, host: typeof args[1] === 'string' ? args[1] : 'localhost' };
  if (opts.path != null || LOOPBACK.has(opts.host ?? 'localhost')) return realConnect.apply(this, args);
  process.nextTick(() => this.destroy(offline(`${opts.host}:${opts.port}`)));
  return this;
};
const FETCH_GUARD = async (input) => { throw offline(String(input?.url ?? input)); };
globalThis.fetch = FETCH_GUARD;
syncBuiltinESMExports();

// Silence the Redis reconnect noise and logger output while modules load and run.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

after(async () => {
  try { (await import('../src/services/redis.js')).default.disconnect(); } catch { /* ignore */ }
  try { await (await import('../src/database/client.js')).disconnect(); } catch { /* ignore */ }
  Object.assign(console, quiet);
});

// Every src/ module is loaded AFTER the env block above (static imports would hoist above it).
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { MODULES, MODULE_NAMES, isModuleKey } = await import('../src/config/modules.js');
const { HOOK_ORDER, INTERNAL_HOOK_POINTS } = await import('../src/config/hookPoints.js');
const { CUSTOM_ID_PREFIXES } = await import('../src/config/customIds.js');
const { JOB_TYPES } = await import('../src/config/jobTypes.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { collectPlugins, installPlugins } = await import('../src/handlers/pluginLoader.js');
const { listHooks, clearHooks } = await import('../src/handlers/hookRegistry.js');
const { createHandlerMaps, registerCoreComponents } = await import('../src/handlers/componentRegistry.js');
const rewards = await import('../src/services/rewards.js');
const { default: logger } = await import('../src/utils/logger.js');

// ── Inline constants (planning data copied into the test) ────────────────────

// Every package of the MEE6-parity build, in slice order.
const PACKAGE_IDS = Object.freeze([
  'foundation', 'foundation-utils', 'foundation-runtime', 'moderation', 'moderation-commands', 'mod-utilities',
  'automod', 'logging', 'welcome', 'levels', 'invites', 'settings', 'translation-fixes', 'games-fixes',
  'embeds', 'reminders', 'giveaways', 'polls', 'starboard', 'reaction-roles', 'tickets', 'temp-voice',
  'stat-channels', 'birthdays', 'custom-commands', 'economy', 'economy-casino', 'achievements', 'automations',
  'captcha', 'feeds', 'search', 'liverole', 'counting', 'emojis', 'dashboard-tasks', 'bot-joins', 'integration',
]);
// Owners that are not plugins. They may own collector/field prefixes, and only the
// persistent prefixes that registerCoreComponents() registers (today: 'noop').
const CORE_OWNERS = Object.freeze(['foundation', 'foundation-runtime', 'foundation-utils']);
// Packages that ship src/plugins/<id>.js, with the module their plugin declares.
const EXPECTED_PLUGIN_MODULE = Object.freeze({
  moderation: 'moderation', automod: 'automod', logging: 'logging', welcome: 'welcome',
  levels: 'leveling', invites: 'invites', 'translation-fixes': 'translation', 'games-fixes': 'games',
  embeds: 'embeds', reminders: 'reminders', giveaways: 'giveaways', polls: 'polls',
  starboard: 'starboard', 'reaction-roles': 'reactionroles', tickets: 'tickets',
  'temp-voice': 'tempvoice', 'stat-channels': 'stats', birthdays: 'birthdays',
  'custom-commands': 'customcommands', economy: 'economy', achievements: 'achievements',
  automations: 'automations', captcha: 'captcha', feeds: 'feeds', liverole: 'liverole', counting: 'counting', 'dashboard-tasks': null, 'bot-joins': null,
});
// Only these files may call ctx.stop() (automod and custom-commands).
const STOP_ALLOWED_FILES = Object.freeze([
  'src/plugins/automod.js', 'src/services/automodService.js',
  'src/plugins/custom-commands.js', 'src/services/tagService.js',
]);
// What src/events/*.js may import: thin dispatchers only.
const EVENT_IMPORT_ALLOWLIST = Object.freeze([
  'discord.js', '../handlers/hookRegistry.js', '../services/settingsService.js',
  '../services/scheduler.js', '../services/redis.js', '../config/jobTypes.js', '../utils/async.js', '../utils/logger.js',
]);
const EVENT_IMPORT_EXTRA = Object.freeze({ 'src/events/interactionCreate.js': ['../handlers/interactionRouter.js'] });
// Env vars set by discord.js / the runtime, never by the operator.
const RUNTIME_ENV = Object.freeze(['SHARDS', 'SHARD_ID', 'DISCORD_SHARD_ID', 'SHARD_COUNT', 'DISCORD_SHARD_COUNT']);

// The command table: file, module, gating and base permission level of every slash command. `level` is a key of
// PERMISSION_LEVELS; `gated: null` marks a core command (module null); `budget` is the planned payload-character
// estimate and only produces a diagnostic (A10; the smoke test fails above 4000).
const EXPECTED_COMMANDS = Object.freeze({
  'achievement-config': { file: 'src/commands/admin/achievement-config.js', module: 'achievements',   gated: false, level: 'ADMIN',     budget: 2500 },
  'automation':         { file: 'src/commands/admin/automation.js',         module: 'automations',    gated: false, level: 'ADMIN',     budget: 2000 },
  'automod':            { file: 'src/commands/admin/automod.js',            module: 'automod',        gated: false, level: 'ADMIN',     budget: 2600 },
  'autotranslate':      { file: 'src/commands/admin/autotranslate.js',      module: 'translation',    gated: false, level: 'ADMIN',     budget:  450 },
  'counting':           { file: 'src/commands/admin/counting.js',           module: 'counting',       gated: false, level: 'ADMIN',     budget:  700 },
  'birthday-config':    { file: 'src/commands/admin/birthday-config.js',    module: 'birthdays',      gated: false, level: 'ADMIN',     budget: 1200 },
  'captcha':            { file: 'src/commands/admin/captcha.js',            module: 'captcha',        gated: false, level: 'ADMIN',     budget: 1200 },
  'economy-admin':      { file: 'src/commands/admin/economy-admin.js',      module: 'economy',        gated: false, level: 'ADMIN',     budget: 3400 },
  'economy-boost':      { file: 'src/commands/admin/economy-boost.js',      module: 'economy',        gated: false, level: 'ADMIN',     budget:  650 },
  'embed':              { file: 'src/commands/admin/embed.js',              module: 'embeds',         gated: true,  level: 'ADMIN',     budget: 2900 },
  'emoji':              { file: 'src/commands/admin/emoji.js',              module: null,             gated: null,  level: 'ADMIN',     budget:  300 },
  'feed':               { file: 'src/commands/admin/feed.js',               module: 'feeds',          gated: false, level: 'ADMIN',     budget: 2400 },
  'goodbye':            { file: 'src/commands/admin/goodbye.js',            module: 'welcome',        gated: false, level: 'ADMIN',     budget:  800 },
  'levelconfig':        { file: 'src/commands/admin/levelconfig.js',        module: 'leveling',       gated: false, level: 'ADMIN',     budget: 2600 },
  'liverole':           { file: 'src/commands/admin/liverole.js',           module: 'liverole',       gated: false, level: 'ADMIN',     budget:  450 },
  'logging':            { file: 'src/commands/admin/logging.js',            module: 'logging',        gated: false, level: 'ADMIN',     budget: 1300 },
  'module':             { file: 'src/commands/admin/module.js',             module: null,             gated: null,  level: 'ADMIN',     budget:  260 },
  'reactionrole':       { file: 'src/commands/admin/reactionrole.js',       module: 'reactionroles',  gated: false, level: 'ADMIN',     budget: 2200 },
  'reminder':           { file: 'src/commands/admin/reminder.js',           module: 'reminders',      gated: false, level: 'ADMIN',     budget: 2000 },
  'settings':           { file: 'src/commands/admin/settings.js',           module: null,             gated: null,  level: 'ADMIN',     budget:  800 },
  'starboard':          { file: 'src/commands/admin/starboard.js',          module: 'starboard',      gated: false, level: 'ADMIN',     budget: 1700 },
  'stats':              { file: 'src/commands/admin/stats.js',              module: 'stats',          gated: false, level: 'ADMIN',     budget:  900 },
  'ticket-panel':       { file: 'src/commands/admin/ticket-panel.js',       module: 'tickets',        gated: false, level: 'ADMIN',     budget: 2900 },
  'trigger':            { file: 'src/commands/admin/trigger.js',            module: 'customcommands', gated: false, level: 'ADMIN',     budget: 1150 },
  'voice-hub':          { file: 'src/commands/admin/voice-hub.js',          module: 'tempvoice',      gated: false, level: 'ADMIN',     budget: 1800 },
  'welcome':            { file: 'src/commands/admin/welcome.js',            module: 'welcome',        gated: false, level: 'ADMIN',     budget: 2000 },
  'achievements':       { file: 'src/commands/community/achievements.js',   module: 'achievements',   gated: true,  level: 'MEMBER',    budget:  260 },
  'birthday':           { file: 'src/commands/community/birthday.js',       module: 'birthdays',      gated: true,  level: 'MEMBER',    budget:  600 },
  'giveaway':           { file: 'src/commands/community/giveaway.js',       module: 'giveaways',      gated: true,  level: 'MODERATOR', budget: 2000 },
  'poll':               { file: 'src/commands/community/poll.js',           module: 'polls',          gated: true,  level: 'MODERATOR', budget:  900 },
  'ticket':             { file: 'src/commands/community/ticket.js',         module: 'tickets',        gated: true,  level: 'MEMBER',    budget:  800 },
  'voice':              { file: 'src/commands/community/voice.js',          module: 'tempvoice',      gated: true,  level: 'MEMBER',    budget: 1300 },
  'balance':            { file: 'src/commands/economy/balance.js',          module: 'economy',        gated: true,  level: 'MEMBER',    budget:  110 },
  'daily':              { file: 'src/commands/economy/daily.js',            module: 'economy',        gated: true,  level: 'MEMBER',    budget:   90 },
  'inventory':          { file: 'src/commands/economy/inventory.js',        module: 'economy',        gated: true,  level: 'MEMBER',    budget:  110 },
  'pay':                { file: 'src/commands/economy/pay.js',              module: 'economy',        gated: true,  level: 'MEMBER',    budget:  150 },
  'pity':               { file: 'src/commands/economy/pity.js',             module: 'economy',        gated: true,  level: 'MEMBER',    budget:   90 },
  'richest':            { file: 'src/commands/economy/richest.js',          module: 'economy',        gated: true,  level: 'MEMBER',    budget:   70 },
  'shop':               { file: 'src/commands/economy/shop.js',             module: 'economy',        gated: true,  level: 'MEMBER',    budget:  400 },
  'use':                { file: 'src/commands/economy/use.js',              module: 'economy',        gated: true,  level: 'MEMBER',    budget:  120 },
  'work':               { file: 'src/commands/economy/work.js',             module: 'economy',        gated: true,  level: 'MEMBER',    budget:   70 },
  'blackjack':          { file: 'src/commands/games/blackjack.js',          module: 'economy',        gated: true,  level: 'MEMBER',    budget:  100 },
  'games':              { file: 'src/commands/games/games.js',              module: 'games',          gated: true,  level: 'MEMBER',    budget:   60 },
  'numguess':           { file: 'src/commands/games/numguess.js',           module: 'games',          gated: true,  level: 'MEMBER',    budget:   60 },
  'roulette':           { file: 'src/commands/games/roulette.js',           module: 'economy',        gated: true,  level: 'MEMBER',    budget:  160 },
  'rps':                { file: 'src/commands/games/rps.js',                module: 'games',          gated: true,  level: 'MEMBER',    budget:   60 },
  'scrabble-help':      { file: 'src/commands/games/scrabble-help.js',      module: 'games',          gated: true,  level: 'MEMBER',    budget:   70 },
  'scrabble-play':      { file: 'src/commands/games/scrabble-play.js',      module: 'games',          gated: true,  level: 'MEMBER',    budget:  320 },
  'scrabble':           { file: 'src/commands/games/scrabble.js',           module: 'games',          gated: true,  level: 'MEMBER',    budget:  110 },
  'slots':              { file: 'src/commands/games/slots.js',              module: 'economy',        gated: true,  level: 'MEMBER',    budget:   70 },
  'tictactoe':          { file: 'src/commands/games/tictactoe.js',          module: 'games',          gated: true,  level: 'MEMBER',    budget:   90 },
  'leaderboard':        { file: 'src/commands/leveling/leaderboard.js',     module: 'leveling',       gated: true,  level: 'MEMBER',    budget:   90 },
  'rank':               { file: 'src/commands/leveling/rank.js',            module: 'leveling',       gated: true,  level: 'MEMBER',    budget:   90 },
  'xp':                 { file: 'src/commands/leveling/xp.js',              module: 'leveling',       gated: false, level: 'ADMIN',     budget:  520 },
  'ban':                { file: 'src/commands/moderation/ban.js',           module: 'moderation',     gated: true,  level: 'MODERATOR', budget:  300 },
  'case':               { file: 'src/commands/moderation/case.js',          module: 'moderation',     gated: true,  level: 'MODERATOR', budget:  220 },
  'history':            { file: 'src/commands/moderation/history.js',       module: 'moderation',     gated: true,  level: 'MODERATOR', budget:  480 },
  'kick':               { file: 'src/commands/moderation/kick.js',          module: 'moderation',     gated: true,  level: 'MODERATOR', budget:  100 },
  'modconfig':          { file: 'src/commands/moderation/modconfig.js',     module: 'moderation',     gated: false, level: 'ADMIN',     budget: 2000 },
  'purge':              { file: 'src/commands/moderation/purge.js',         module: 'moderation',     gated: true,  level: 'MODERATOR', budget:  430 },
  'slowmode':           { file: 'src/commands/moderation/slowmode.js',      module: 'moderation',     gated: true,  level: 'MODERATOR', budget:  220 },
  'softban':            { file: 'src/commands/moderation/softban.js',       module: 'moderation',     gated: true,  level: 'MODERATOR', budget:  200 },
  'timeout':            { file: 'src/commands/moderation/timeout.js',       module: 'moderation',     gated: true,  level: 'MODERATOR', budget:  220 },
  'unban':              { file: 'src/commands/moderation/unban.js',         module: 'moderation',     gated: true,  level: 'MODERATOR', budget:  120 },
  'unmute':             { file: 'src/commands/moderation/unmute.js',        module: 'moderation',     gated: true,  level: 'MODERATOR', budget:   90 },
  'warn':               { file: 'src/commands/moderation/warn.js',          module: 'moderation',     gated: true,  level: 'MODERATOR', budget:  110 },
  'anime':              { file: 'src/commands/search/anime.js',             module: 'search',         gated: true,  level: 'MEMBER',    budget:   90 },
  'manga':              { file: 'src/commands/search/manga.js',             module: 'search',         gated: true,  level: 'MEMBER',    budget:   90 },
  'pokemon':            { file: 'src/commands/search/pokemon.js',           module: 'search',         gated: true,  level: 'MEMBER',    budget:   90 },
  'urban':              { file: 'src/commands/search/urban.js',             module: 'search',         gated: true,  level: 'MEMBER',    budget:   90 },
  'detectlanguage':     { file: 'src/commands/utility/detectlanguage.js',   module: null,             gated: null,  level: 'MEMBER',    budget:   80 },
  'help':               { file: 'src/commands/utility/help.js',             module: null,             gated: null,  level: 'MEMBER',    budget:  160 },
  'info':               { file: 'src/commands/utility/info.js',             module: null,             gated: null,  level: 'MEMBER',    budget:  280 },
  'invites':            { file: 'src/commands/utility/invites.js',          module: 'invites',        gated: true,  level: 'MEMBER',    budget:  500 },
  'languages':          { file: 'src/commands/utility/languages.js',        module: null,             gated: null,  level: 'MEMBER',    budget:   60 },
  'tag':                { file: 'src/commands/utility/tag.js',              module: 'customcommands', gated: true,  level: 'MEMBER',    budget: 2200 },
  'translate':          { file: 'src/commands/utility/translate.js',        module: null,             gated: null,  level: 'MEMBER',    budget:  170 },
});

// Test files each package owns (P0). Every other id owns test/<id>.test.js.
const TEST_FILES = Object.freeze({
  foundation: ['test/foundation.test.js'],
  'foundation-runtime': ['test/smoke.test.js', 'test/registry.test.js'],
  'foundation-utils': ['test/utils.test.js'],
  integration: ['test/integration.test.js', 'test/db.test.js'],
});

// ── Helpers and loaders ──────────────────────────────────────────────────────

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SRC = join(ROOT, 'src');

function walk(dir) {
  if (!existsSync(dir)) return [];
  return readdirSync(dir).sort().flatMap((entry) => {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) return walk(full);
    return entry.endsWith('.js') ? [full] : [];
  });
}
const rel = (file) => relative(ROOT, file).replace(/\\/g, '/');
const load = (file) => import(pathToFileURL(file).href);
const readRepo = (path) => readFileSync(join(ROOT, path), 'utf8');

// Removes comments, keeping line numbers. Trailing `//` comments are kept so URLs inside strings are never cut.
const stripComments = (code) => code
  .replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '))
  .replace(/^(\s*)\/\/.*$/gm, '$1');

const SRC_FILES = walk(SRC).map(rel);
const CODE = new Map(SRC_FILES.map((p) => [p, stripComments(readRepo(p))]));

/** Every match of a global regex over CODE: [{ file, line, groups, index }]. */
function scan(regex, files = SRC_FILES) {
  const hits = [];
  for (const file of files) {
    const code = CODE.get(file);
    for (const m of code.matchAll(regex)) {
      hits.push({ file, line: 1 + (code.slice(0, m.index).match(/\n/g)?.length ?? 0), groups: m.slice(1), index: m.index });
    }
  }
  return hits;
}
const where = (hit) => `${hit.file}:${hit.line}`;

/** The text between a `(` and its matching `)`; quoted strings and template literals are opaque. '' when unbalanced. */
function callArgs(code, open) {
  if (code[open] !== '(') return '';
  let depth = 0;
  for (let i = open; i < code.length; i++) {
    const ch = code[i];
    if (ch === "'" || ch === '"' || ch === '`') {
      for (i++; i < code.length && code[i] !== ch; i++) if (code[i] === '\\') i++;
      continue;
    }
    if (ch === '(') depth++;
    else if (ch === ')' && --depth === 0) return code.slice(open + 1, i);
  }
  return '';
}

// Discord caps the combined length of every name, description and choice value (same as test/smoke.test.js).
function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

/** The set of 'sub' (type 1 at the top level) and 'group sub' (type 1 under a type 2) keys of a payload. */
function subcommandKeys(payload) {
  const keys = new Set();
  for (const opt of payload.options ?? []) {
    if (opt.type === 1) keys.add(opt.name);
    else if (opt.type === 2) for (const sub of opt.options ?? []) if (sub.type === 1) keys.add(`${opt.name} ${sub.name}`);
  }
  return keys;
}

const diff = (a, b) => [...a].filter((x) => !b.has(x));
const sortedUnique = (list) => [...new Set(list)].sort();
/** Asserts a list of problem strings is empty; the list is printed in the failure message. */
function expectNone(list, what) {
  assert.deepEqual(list, [], `${what} (${list.length}):\n  ${list.join('\n  ')}`);
}
/** Like expectNone for several [what, list] groups, so one failure names everything that is wrong. */
function expectNoneOf(groups) {
  const bad = groups.filter(([, list]) => list.length > 0);
  assert.deepEqual(bad.flatMap(([, list]) => list), [], bad.map(([what, list]) => `${what} (${list.length}):\n  ${list.join('\n  ')}`).join('\n'));
}

// Commands: every file under src/commands (files starting with `_` are skipped, as the real loader does).
const COMMANDS = [];
for (const file of walk(join(SRC, 'commands'))) {
  if (basename(file).startsWith('_')) continue;
  const entry = { file, ns: null, payload: null, error: null };
  try {
    entry.ns = await load(file);
    entry.payload = buildCommandPayload(entry.ns);
  } catch (err) {
    entry.error = err;
  }
  COMMANDS.push(entry);
}
const OK_COMMANDS = COMMANDS.filter((c) => c.error === null && c.payload);
const CMD_BY_NAME = new Map(OK_COMMANDS.map((c) => [c.payload.name, c]));

// Plugins: src/plugins/<id>.js, each exporting `plugin`.
const PLUGIN_FILES = SRC_FILES.filter((p) => /^src\/plugins\/[^_/][^/]*\.js$/.test(p));
const PLUGINS = [];
for (const p of PLUGIN_FILES) {
  const entry = { id: basename(p, '.js'), file: p, plugin: undefined, error: null };
  try {
    entry.plugin = (await load(join(ROOT, p))).plugin;
  } catch (err) {
    entry.error = err;
  }
  PLUGINS.push(entry);
}
const PLUGIN_IDS = new Set(PLUGINS.map((p) => p.id));

// Declared registrations, read from the raw plugin objects (never from installed state).
const effModule = (decl, plugin) => (decl.module !== undefined ? decl.module : plugin.module);
const DECL_HOOKS = PLUGINS.flatMap(({ id, plugin }) => (plugin?.hooks ?? []).map((h) =>
  ({ pluginId: id, event: h.event, id: h.id, module: effModule(h, plugin), always: h.always === true, decl: h })));
const DECL_COMPONENTS = PLUGINS.flatMap(({ id, plugin }) => (plugin?.components ?? []).map((c) =>
  ({ pluginId: id, type: c.type, prefix: c.prefix, module: effModule(c, plugin), decl: c })));
const DECL_JOBS = PLUGINS.flatMap(({ id, plugin }) => (plugin?.jobs ?? []).map((j) => ({ pluginId: id, type: j.type, decl: j })));
const DECL_PROVIDERS = PLUGINS.flatMap(({ id, plugin }) => Object.entries(plugin?.provides ?? {}).map(([kind, impl]) => ({ pluginId: id, kind, impl })));
const MAP_OF = { button: 'buttonHandlers', select: 'selectHandlers', modal: 'modalHandlers' };

const isModuleRef = (m) => m === null || isModuleKey(m);

// What registerCoreComponents() registers, computed on a private client (nothing global is touched).
const coreClient = {};
createHandlerMaps(coreClient);
registerCoreComponents(coreClient);
const CORE_REG = Object.entries(MAP_OF).flatMap(([type, map]) => [...coreClient[map].keys()].map((prefix) => ({ type, prefix })));

// ── A. Commands ──────────────────────────────────────────────────────────────

describe('A commands', () => {
  test('A0 every command file loads and exports { data, execute }', () => {
    const problems = COMMANDS.flatMap((c) => {
      if (c.error) return [`${rel(c.file)}: ${c.error.message}`];
      const out = [];
      if (!c.ns.data || typeof c.ns.data !== 'object') out.push(`${rel(c.file)}: data is not an object`);
      if (typeof c.ns.execute !== 'function') out.push(`${rel(c.file)}: execute is not a function`);
      return out;
    });
    expectNone(problems, 'broken command files');
  });

  test('A1 the command set equals the expected table', () => {
    const names = OK_COMMANDS.map((c) => c.payload.name);
    const actual = new Set(names);
    const expected = new Set(Object.keys(EXPECTED_COMMANDS));
    expectNoneOf([
      ['expected commands that do not exist', diff(expected, actual).map((n) => `/${n}`)],
      ['commands missing from EXPECTED_COMMANDS', diff(actual, expected).map((n) => `/${n}`)],
      ['duplicate command names', sortedUnique(names.filter((n, i) => names.indexOf(n) !== i))],
    ]);
    assert.equal(COMMANDS.length, 77, `${COMMANDS.length} command files (expected 77)`);
    assert.ok(COMMANDS.length <= 100, `${COMMANDS.length} commands exceed Discord's limit of 100`);
  });

  test('A2 every command lives at its expected path', () => {
    const problems = OK_COMMANDS.flatMap((c) => {
      const exp = EXPECTED_COMMANDS[c.payload.name];
      return exp && rel(c.file) !== exp.file ? [`/${c.payload.name}: ${rel(c.file)} (expected ${exp.file})`] : [];
    });
    expectNone(problems, 'misplaced command files');
  });

  test('A3 every command exports module explicitly', () => {
    const problems = OK_COMMANDS.flatMap((c) => {
      if (!('module' in c.ns)) return [`/${c.payload.name}: no \`module\` export`];
      if (!(c.ns.module === null || isModuleKey(c.ns.module))) return [`/${c.payload.name}: module ${JSON.stringify(c.ns.module)} is not null or a MODULES key`];
      return [];
    });
    expectNone(problems, 'commands without a valid module');
  });

  test('A4 every command belongs to its expected module', () => {
    const problems = OK_COMMANDS.flatMap((c) => {
      const exp = EXPECTED_COMMANDS[c.payload.name];
      return exp && c.ns.module !== exp.module ? [`/${c.payload.name}: module ${JSON.stringify(c.ns.module)} (expected ${JSON.stringify(exp.module)})`] : [];
    });
    expectNone(problems, 'module mismatches');
  });

  test('A5 effective gating matches (gated defaults to true)', () => {
    const problems = OK_COMMANDS.flatMap((c) => {
      const { gated, module } = c.ns;
      const exp = EXPECTED_COMMANDS[c.payload.name];
      if (gated !== undefined && typeof gated !== 'boolean') return [`/${c.payload.name}: gated is ${JSON.stringify(gated)}, not a boolean`];
      if (module === null || !exp) return [];
      const effective = gated !== false;
      return effective !== exp.gated ? [`/${c.payload.name}: effectively gated=${effective} (expected ${exp.gated})`] : [];
    });
    expectNone(problems, 'gating mismatches');
  });

  test('A6 every command has its expected base permission level', () => {
    const levels = Object.values(PERMISSION_LEVELS);
    const problems = OK_COMMANDS.flatMap((c) => {
      const level = c.ns.permissionLevel;
      const exp = EXPECTED_COMMANDS[c.payload.name];
      const out = [];
      if (level !== undefined && !levels.includes(level)) out.push(`/${c.payload.name}: permissionLevel ${JSON.stringify(level)} is not a PERMISSION_LEVELS value`);
      if (exp && (level ?? PERMISSION_LEVELS.MEMBER) !== PERMISSION_LEVELS[exp.level]) {
        out.push(`/${c.payload.name}: level ${level ?? PERMISSION_LEVELS.MEMBER} (expected ${exp.level} = ${PERMISSION_LEVELS[exp.level]})`);
      }
      return out;
    });
    expectNone(problems, 'permission level mismatches');
  });

  test('A7 subcommandPermissions keys name real subcommands', () => {
    const levels = Object.values(PERMISSION_LEVELS);
    const problems = OK_COMMANDS.flatMap((c) => {
      const perms = c.ns.subcommandPermissions;
      if (perms === undefined) return [];
      if (perms === null || typeof perms !== 'object' || Array.isArray(perms)) return [`/${c.payload.name}: subcommandPermissions is not a plain object`];
      const keys = subcommandKeys(c.payload);
      return Object.entries(perms).flatMap(([key, value]) => [
        ...(keys.has(key) ? [] : [`/${c.payload.name}: key "${key}" is not a subcommand (have: ${[...keys].join(', ') || 'none'})`]),
        ...(levels.includes(value) ? [] : [`/${c.payload.name}: "${key}" has value ${JSON.stringify(value)}, not a PERMISSION_LEVELS value`]),
      ]);
    });
    expectNone(problems, 'bad subcommandPermissions');
  });

  test('A8 MODERATOR and ADMIN commands restrict default member permissions; optional exports have the right type', () => {
    const problems = OK_COMMANDS.flatMap((c) => {
      const out = [];
      const dmp = c.payload.default_member_permissions;
      if ((c.ns.permissionLevel ?? PERMISSION_LEVELS.MEMBER) >= PERMISSION_LEVELS.MODERATOR && !(typeof dmp === 'string' && dmp.length > 0)) {
        out.push(`/${c.payload.name}: level ${c.ns.permissionLevel} command without default_member_permissions (got ${JSON.stringify(dmp)})`);
      }
      if (c.ns.allowInDMs !== undefined && typeof c.ns.allowInDMs !== 'boolean') out.push(`/${c.payload.name}: allowInDMs is not a boolean`);
      if (c.ns.autocomplete !== undefined && typeof c.ns.autocomplete !== 'function') out.push(`/${c.payload.name}: autocomplete is not a function`);
      return out;
    });
    expectNone(problems, 'command export problems');
  });

  test('A9 every module has an existing config command', () => {
    const problems = MODULE_NAMES.flatMap((k) => {
      const cfg = MODULES[k].configCommand;
      if (typeof cfg !== 'string' || !/^\/[a-z0-9-]+$/.test(cfg)) return [`${k}: configCommand ${JSON.stringify(cfg)} is not "/name"`];
      const cmd = CMD_BY_NAME.get(cfg.slice(1));
      if (!cmd) return [`${k}: configCommand ${cfg} does not exist`];
      // `null` is allowed: search's config command is the core /settings.
      if (cmd.ns.module !== k && cmd.ns.module !== null) return [`${k}: ${cfg} belongs to module ${JSON.stringify(cmd.ns.module)}`];
      return [];
    });
    expectNone(problems, 'bad module config commands');
  });

  test('A10 payload size against the planned budgets (diagnostic only)', (t) => {
    for (const c of OK_COMMANDS) {
      const exp = EXPECTED_COMMANDS[c.payload.name];
      const chars = charCount(c.payload);
      if (exp && chars > exp.budget) t.diagnostic(`/${c.payload.name}: ${chars} chars (planned ${exp.budget})`);
    }
  });
});

// ── B. Hooks ─────────────────────────────────────────────────────────────────

describe('B hooks', () => {
  test('B1 every HOOK_ORDER entry is declared by exactly one plugin hook', () => {
    const problems = Object.entries(HOOK_ORDER).flatMap(([event, ids]) => ids.flatMap((id) => {
      const n = DECL_HOOKS.filter((h) => h.event === event && h.id === id).length;
      return n === 1 ? [] : [`${event}:${id} is declared ${n} time(s)`];
    }));
    expectNone(problems, 'HOOK_ORDER entries without exactly one declaration');
  });

  test('B2 every declared hook is listed in HOOK_ORDER under its plugin id', () => {
    const problems = DECL_HOOKS.flatMap((h) => {
      const out = [];
      if (!Object.hasOwn(HOOK_ORDER, h.event) || !HOOK_ORDER[h.event].includes(h.id)) out.push(`${h.pluginId}: ${h.event}:${h.id} is not in HOOK_ORDER`);
      if (!(h.id === h.pluginId || String(h.id).startsWith(`${h.pluginId}:`))) out.push(`${h.pluginId}: ${h.event}:${h.id} must be "${h.pluginId}" or start with "${h.pluginId}:"`);
      return out;
    });
    expectNone(problems, 'unlisted or misnamed hooks');
  });

  test('B3 every declared hook has a valid shape', () => {
    // Soft-timeout ceiling per hook (ms). deviation: liverole report - guildReady:liverole does a full member fetch
    // plus up to 50 role edits, so its package spec sets 120 s; every other hook stays within 60 s.
    const maxTimeout = (h) => (h.event === 'guildReady' && h.id === 'liverole' ? 120_000 : 60_000);
    const problems = DECL_HOOKS.flatMap((h) => {
      const tag = `${h.pluginId}: ${h.event}:${h.id}`;
      const d = h.decl;
      const out = [];
      if (typeof d.run !== 'function') out.push(`${tag}: run is not a function`);
      if (d.filter !== undefined && (typeof d.filter !== 'function' || d.filter.constructor.name === 'AsyncFunction')) out.push(`${tag}: filter must be a synchronous function`);
      if (d.always !== undefined && typeof d.always !== 'boolean') out.push(`${tag}: always is not a boolean`);
      if (d.timeoutMs !== undefined && !(Number.isInteger(d.timeoutMs) && d.timeoutMs >= 1 && d.timeoutMs <= maxTimeout(h))) out.push(`${tag}: timeoutMs ${JSON.stringify(d.timeoutMs)} is not an integer in 1..${maxTimeout(h)}`);
      if (!isModuleRef(h.module)) out.push(`${tag}: effective module ${JSON.stringify(h.module)} is not null or a MODULES key`);
      return out;
    });
    expectNone(problems, 'malformed hooks');
  });

  test('B4 internal hook points are emitted by their owning package only', () => {
    const INTERNAL_EMITTERS = {
      levelUp: (f) => ['src/services/xpService.js', 'src/plugins/levels.js', 'src/commands/admin/levelconfig.js'].includes(f) || f.startsWith('src/commands/leveling/'),
      economyPurchase: (f) => ['src/services/economyService.js', 'src/plugins/economy.js', 'src/commands/admin/economy-admin.js',
        'src/commands/games/blackjack.js', 'src/commands/games/roulette.js', 'src/commands/games/slots.js'].includes(f) || f.startsWith('src/commands/economy/'),
    };
    const problems = [];
    for (const ev of INTERNAL_HOOK_POINTS) {
      const hits = scan(new RegExp(`runHooks\\(\\s*['"\`]${ev}['"\`]`, 'g'));
      if (!INTERNAL_EMITTERS[ev]) { problems.push(`${ev}: no emitter rule in this test`); continue; }
      if (hits.length === 0) problems.push(`${ev}: runHooks('${ev}') is never called`);
      for (const h of hits) if (!INTERNAL_EMITTERS[ev](h.file)) problems.push(`${ev}: emitted from ${where(h)}`);
    }
    expectNone(problems, 'internal hook point problems');
  });

  test('B5 every gateway hook point has a dispatcher', () => {
    const problems = [];
    for (const event of Object.keys(HOOK_ORDER)) {
      if (INTERNAL_HOOK_POINTS.includes(event)) continue;
      const re = new RegExp(`runHooks\\(\\s*['"\`]${event}['"\`]`, 'g');
      if (event === 'guildReady') {
        if (scan(re, ['src/handlers/hookRegistry.js']).length === 0) problems.push("guildReady: runHooks('guildReady' not found in src/handlers/hookRegistry.js");
      } else if (scan(re, SRC_FILES.filter((f) => f.startsWith('src/events/'))).length === 0) {
        problems.push(`${event}: no file under src/events/ calls runHooks('${event}'`);
      }
    }
    expectNone(problems, 'hook points without a dispatcher');
  });

  test('B6 every literal runHooks event name is a HOOK_ORDER key', () => {
    const problems = scan(/\brunHooks\(\s*['"`]([A-Za-z0-9_]+)['"`]/g)
      .filter((h) => !Object.hasOwn(HOOK_ORDER, h.groups[0]))
      .map((h) => `${where(h)}: runHooks('${h.groups[0]}'`);
    expectNone(problems, 'runHooks calls with an unknown event');
  });

  test('B7 only automod and custom-commands may call ctx.stop()', () => {
    const problems = scan(/\bctx\.stop\(/g)
      .filter((h) => !h.file.startsWith('src/handlers/') && !STOP_ALLOWED_FILES.includes(h.file))
      .map(where);
    expectNone(problems, 'ctx.stop() outside the allowed files');
  });
});

// ── C. customIds ─────────────────────────────────────────────────────────────

describe('C customIds', () => {
  const ALL_REG = [...DECL_COMPONENTS.map((c) => ({ type: c.type, prefix: c.prefix })), ...CORE_REG];
  const registeredTypes = (prefix) => ALL_REG.filter((r) => r.prefix === prefix).map((r) => r.type);

  // Prefixes emitted by a literal in src/ (shared by C4 and C5).
  const EMIT_PATTERNS = [
    /setCustomId\(\s*[`'"]([a-z][a-z0-9-]*)/g,
    /\bcustom_?[iI]d\s*:\s*[`'"]([a-z][a-z0-9-]*)/g,
    /buildCustomId\(\s*[`'"]([a-z][a-z0-9-]*)[`'"]/g,
  ];
  const EMIT_FILES = SRC_FILES.filter((f) => f !== 'src/config/customIds.js');
  const EMITTED = EMIT_PATTERNS.flatMap((re) => scan(re, EMIT_FILES)).map((h) => ({ prefix: h.groups[0], at: where(h) }));

  test('C1 every declared component is a registered persistent prefix of its owner', () => {
    const problems = DECL_COMPONENTS.flatMap((c) => {
      const tag = `${c.pluginId}: ${c.type} "${c.prefix}"`;
      const info = Object.hasOwn(CUSTOM_ID_PREFIXES, c.prefix) ? CUSTOM_ID_PREFIXES[c.prefix] : undefined;
      const out = [];
      if (!Object.hasOwn(MAP_OF, c.type)) out.push(`${tag}: type must be button, select or modal`);
      if (typeof c.decl.handler !== 'function') out.push(`${tag}: handler is not a function`);
      if (!isModuleRef(c.module)) out.push(`${tag}: effective module ${JSON.stringify(c.module)} is not null or a MODULES key`);
      if (!info) return [...out, `${tag}: prefix is not in CUSTOM_ID_PREFIXES`];
      if (info.kind !== 'persistent') out.push(`${tag}: registry kind is ${info.kind}`);
      if (info.owner !== c.pluginId) out.push(`${tag}: registry owner is ${info.owner}`);
      if (!info.types.includes(c.type)) out.push(`${tag}: registry types are ${info.types.join('/')}`);
      return out;
    });
    expectNone(problems, 'bad component declarations');
  });

  test('C2 core registrations are persistent prefixes owned by the core', () => {
    const problems = CORE_REG.flatMap(({ type, prefix }) => {
      const info = CUSTOM_ID_PREFIXES[prefix];
      if (!info) return [`${type} "${prefix}" is not in CUSTOM_ID_PREFIXES`];
      const out = [];
      if (info.kind !== 'persistent') out.push(`${type} "${prefix}" is ${info.kind}`);
      if (!CORE_OWNERS.includes(info.owner)) out.push(`${type} "${prefix}" is owned by ${info.owner}`);
      if (!info.types.includes(type)) out.push(`${type} "${prefix}": registry types are ${info.types.join('/')}`);
      return out;
    });
    expectNone(problems, 'bad core registrations');
  });

  test('C3 every persistent prefix has exactly one handler', () => {
    const problems = Object.entries(CUSTOM_ID_PREFIXES).filter(([, info]) => info.kind === 'persistent').flatMap(([prefix, info]) =>
      info.types.flatMap((type) => {
        const n = ALL_REG.filter((r) => r.type === type && r.prefix === prefix).length;
        return n === 1 ? [] : [`${type} "${prefix}" (owner ${info.owner}) has ${n} handler(s)`];
      }));
    expectNone(problems, 'persistent prefixes without exactly one handler');
  });

  test('C4 every customId prefix emitted in src/ is registered correctly', () => {
    const problems = EMITTED.flatMap(({ prefix, at }) => {
      const info = Object.hasOwn(CUSTOM_ID_PREFIXES, prefix) ? CUSTOM_ID_PREFIXES[prefix] : undefined;
      if (!info) return [`${at}: "${prefix}" is not in CUSTOM_ID_PREFIXES`];
      const types = registeredTypes(prefix);
      if (info.kind === 'persistent' && !info.types.some((t) => types.includes(t))) return [`${at}: persistent "${prefix}" has no registered handler`];
      if (info.kind !== 'persistent' && types.length) return [`${at}: ${info.kind} "${prefix}" must not be registered (registered as ${types.join('/')})`];
      return [];
    });
    expectNone(problems, 'emitted customId prefixes with a problem');
  });

  test('C5 persistent prefixes never emitted by a literal (diagnostic only)', (t) => {
    const emitted = new Set(EMITTED.map((e) => e.prefix));
    for (const [prefix, info] of Object.entries(CUSTOM_ID_PREFIXES)) {
      if (info.kind === 'persistent' && !emitted.has(prefix)) t.diagnostic(`persistent prefix "${prefix}" is never emitted by a literal`);
    }
  });

  test('C6 collector and field prefixes are never registered', () => {
    const problems = Object.entries(CUSTOM_ID_PREFIXES)
      .filter(([, info]) => info.kind === 'collector' || info.kind === 'field')
      .filter(([prefix]) => ALL_REG.some((r) => r.prefix === prefix))
      .map(([prefix, info]) => `${info.kind} "${prefix}" is registered as ${registeredTypes(prefix).join('/')}`);
    expectNone(problems, 'registered collector/field prefixes');
  });

  test('C7 legacy handler registration is gone', () => {
    const exporters = OK_COMMANDS.filter((c) => 'registerHandlers' in c.ns).map((c) => `${rel(c.file)} exports registerHandlers`);
    const direct = scan(/\.(?:buttonHandlers|selectHandlers|modalHandlers)\.set\(/g)
      .filter((h) => !h.file.startsWith('src/handlers/')).map((h) => `${where(h)}: direct handler registration`);
    expectNone([...exporters, ...direct], 'legacy registrations');
  });
});

// ── D. Scheduler jobs ────────────────────────────────────────────────────────

describe('D jobs', () => {
  test('D1 every JOB_TYPES entry has exactly one handler, declared by its owner', () => {
    const problems = Object.entries(JOB_TYPES).flatMap(([type, info]) => {
      const decls = DECL_JOBS.filter((j) => j.type === type);
      if (decls.length !== 1) return [`${type}: ${decls.length} handler declaration(s)`];
      const out = [];
      if (decls[0].pluginId !== info.owner) out.push(`${type}: declared by ${decls[0].pluginId}, owner is ${info.owner}`);
      if (typeof decls[0].decl.handler !== 'function') out.push(`${type}: handler is not a function`);
      return out;
    });
    expectNone(problems, 'job type problems');
  });

  test('D2 every declared job type is registered in JOB_TYPES', () => {
    expectNone(DECL_JOBS.filter((j) => !Object.hasOwn(JOB_TYPES, j.type)).map((j) => `${j.pluginId}: ${j.type}`), 'unregistered job types');
  });

  test('D3 literal job types passed to scheduleJob/cancelJobs are registered', () => {
    const problems = [];
    for (const h of scan(/\b(?:scheduleJob|cancelJobs)\(/g)) {
      const code = CODE.get(h.file);
      const args = callArgs(code, code.indexOf('(', h.index));
      const m = args.match(/\btype\s*:\s*[`'"]([^`'"$]+)[`'"]/);
      if (m && !Object.hasOwn(JOB_TYPES, m[1])) problems.push(`${where(h)}: job type "${m[1]}"`);
    }
    expectNone(problems, 'unknown literal job types');
  });

  test('D4 literal dedupe keys use a registered prefix', () => {
    const prefixes = new Set(Object.values(JOB_TYPES).map((j) => j.dedupe.split(':')[0]));
    const problems = [
      ...scan(/\bdedupeKey\s*:\s*[`'"]([a-z][a-zA-Z0-9.-]*):/g),
      ...scan(/\bgetJob\(\s*[`'"]([a-z][a-zA-Z0-9.-]*):/g),
    ].filter((h) => !prefixes.has(h.groups[0])).map((h) => `${where(h)}: "${h.groups[0]}:"`);
    expectNone(problems, `dedupe prefixes outside {${[...prefixes].join(', ')}}`);
  });

  test('D5 nothing under src/ uses setInterval (recurring work belongs to the scheduler)', () => {
    expectNone(scan(/\bsetInterval\(/g).map(where), 'setInterval calls');
  });
});

// ── E. Plugins and owners ────────────────────────────────────────────────────

describe('E plugins and owners', () => {
  test('E1 the plugin files are exactly the expected packages', () => {
    const problems = PLUGINS.flatMap((p) => {
      if (p.error) return [`${p.file}: import failed: ${p.error.message}`];
      return p.plugin?.id === p.id ? [] : [`${p.file}: plugin.id is ${JSON.stringify(p.plugin?.id)}, expected "${p.id}"`];
    });
    const expected = new Set(Object.keys(EXPECTED_PLUGIN_MODULE));
    expectNoneOf([
      ['broken plugin files', problems],
      ['expected plugins that do not exist', diff(expected, PLUGIN_IDS).map((id) => `src/plugins/${id}.js`)],
      ['unexpected plugin files', diff(PLUGIN_IDS, expected).map((id) => `src/plugins/${id}.js`)],
    ]);
  });

  test('E2 collectPlugins() validates every plugin without errors', async () => {
    const collected = await collectPlugins();
    assert.deepEqual(collected.errors, []);
    assert.equal(collected.plugins.length, PLUGINS.length);
  });

  test('E3 every plugin declares its expected module; init is synchronous', () => {
    const problems = PLUGINS.filter((p) => p.plugin).flatMap(({ id, plugin }) => {
      const out = [];
      if (plugin.module !== EXPECTED_PLUGIN_MODULE[id]) out.push(`${id}: module ${JSON.stringify(plugin.module)} (expected ${JSON.stringify(EXPECTED_PLUGIN_MODULE[id])})`);
      if (plugin.init !== undefined && (typeof plugin.init !== 'function' || plugin.init.constructor.name === 'AsyncFunction')) out.push(`${id}: init must be a synchronous function`);
      return out;
    });
    expectNone(problems, 'plugin module/init problems');
  });

  test('E4 every owner in the registries is real', () => {
    const problems = [];
    for (const [event, ids] of Object.entries(HOOK_ORDER)) {
      for (const id of ids) if (!PLUGIN_IDS.has(id.split(':')[0])) problems.push(`HOOK_ORDER.${event}: "${id}" has no plugin "${id.split(':')[0]}"`);
    }
    for (const [type, info] of Object.entries(JOB_TYPES)) if (!PLUGIN_IDS.has(info.owner)) problems.push(`JOB_TYPES["${type}"]: owner "${info.owner}" is not a plugin`);
    const coreRegistered = new Set(CORE_REG.map((r) => r.prefix));
    for (const [prefix, info] of Object.entries(CUSTOM_ID_PREFIXES)) {
      if (info.kind === 'persistent') {
        const ok = PLUGIN_IDS.has(info.owner) || (CORE_OWNERS.includes(info.owner) && coreRegistered.has(prefix));
        if (!ok) problems.push(`CUSTOM_ID_PREFIXES["${prefix}"]: persistent prefix owner "${info.owner}" is neither a plugin nor a core registration`);
      } else if (!PACKAGE_IDS.includes(info.owner) || info.owner === 'integration') {
        problems.push(`CUSTOM_ID_PREFIXES["${prefix}"]: ${info.kind} prefix owner "${info.owner}" is not a package`);
      }
    }
    expectNone(problems, 'registry entries with an unknown owner');
  });

  test('E5 plugin ids are real packages and never a core owner', () => {
    const problems = [...PLUGIN_IDS].flatMap((id) => [
      ...(CORE_OWNERS.includes(id) || id === 'integration' ? [`${id}: must not ship a plugin`] : []),
      ...(PACKAGE_IDS.includes(id) ? [] : [`${id}: not a known package id`]),
    ]);
    expectNone(problems, 'bad plugin ids');
  });
});

// ── F. Rewards and internal hooks ────────────────────────────────────────────

describe('F rewards and internal hooks', () => {
  test('F1 every reward kind has exactly one provider from the right module', () => {
    const problems = rewards.REWARD_KINDS.flatMap((kind) => {
      const decls = DECL_PROVIDERS.filter((p) => p.kind === kind);
      if (decls.length !== 1) return [`${kind}: ${decls.length} provider declaration(s)`];
      const [d] = decls;
      const out = [];
      if (typeof d.impl.grant !== 'function') out.push(`${kind}: ${d.pluginId} provider has no grant()`);
      if (typeof d.impl.take !== 'function') out.push(`${kind}: ${d.pluginId} provider has no take()`);
      const mod = PLUGINS.find((p) => p.id === d.pluginId)?.plugin?.module;
      if (mod !== rewards.REWARD_MODULES[kind]) out.push(`${kind}: provided by ${d.pluginId} (module ${JSON.stringify(mod)}), expected module ${rewards.REWARD_MODULES[kind]}`);
      return out;
    });
    expectNone(problems, 'reward provider problems');
    assert.deepEqual(DECL_PROVIDERS.filter((p) => !rewards.REWARD_KINDS.includes(p.kind)).map((p) => `${p.pluginId}: ${p.kind}`), [], 'providers for unknown reward kinds');
  });

  test('F2 no provider is registered before the install', async () => {
    assert.equal(rewards.hasProvider('xp'), false);
    assert.equal(rewards.hasProvider('coins'), false);
    assert.equal(rewards.isRewardAvailable('xp', { xpEnabled: true }), false);
    assert.deepEqual(await rewards.grant('coins', { guild: null, userId: '1', amount: 5 }), { ok: false, reason: 'unavailable' });
  });

  test('F3 every internal hook point has at least one hook in HOOK_ORDER', () => {
    const problems = INTERNAL_HOOK_POINTS.filter((ev) => !Array.isArray(HOOK_ORDER[ev]) || HOOK_ORDER[ev].length === 0);
    expectNone(problems, 'internal hook points without hooks');
  });
});

// ── G. Repo-wide static rules ────────────────────────────────────────────────

describe('G repo rules', () => {
  const onlyIn = (hits, allowed) => hits.filter((h) => !allowed.includes(h.file)).map(where);

  test('G1 console is used only by the logger', () => {
    expectNone(onlyIn(scan(/\bconsole\.(?:log|error|warn|info|debug)\(/g), ['src/utils/logger.js']), 'console.* outside src/utils/logger.js');
  });

  test('G2 the settings cache key is built only by settingsService', () => {
    const hits = [...scan(/guild:\$\{[^}]*\}:settings\b/g), ...scan(/\bsettingsKey\(/g)];
    expectNone(onlyIn(hits, ['src/services/settingsService.js']), 'settings cache key outside src/services/settingsService.js');
  });

  test('G3 GuildSettings is read and written only through settingsService', () => {
    expectNone(onlyIn(scan(/\b(?:prisma|tx|db)\.guildSettings\.[a-zA-Z]+\(/g), ['src/services/settingsService.js', 'src/database/seed.js']),
      'direct guildSettings access');
  });

  test('G4 roles are added and removed one at a time', () => {
    const hits = [...scan(/\.roles\.(?:add|remove)\(\s*\[/g), ...scan(/\.roles\.set\(/g)];
    expectNone(hits.map(where), 'bulk role changes');
  });

  test('G5 allowedMentions never parses everyone', () => {
    expectNone(scan(/parse\s*:\s*\[[^\]]*['"]everyone['"]/g).map(where), "allowedMentions parse: ['everyone']");
  });

  test('G6 event files are thin dispatchers (allowed imports only)', () => {
    const events = SRC_FILES.filter((f) => f.startsWith('src/events/'));
    const hits = [...scan(/^\s*import\s[^'"]*['"]([^'"]+)['"]/gm, events), ...scan(/\bimport\(\s*['"]([^'"]+)['"]\s*\)/g, events)];
    const problems = hits.filter((h) => {
      const spec = h.groups[0];
      return !(EVENT_IMPORT_ALLOWLIST.includes(spec) || (EVENT_IMPORT_EXTRA[h.file] ?? []).includes(spec) || spec.startsWith('node:'));
    }).map((h) => `${where(h)}: imports ${h.groups[0]}`);
    expectNone(problems, 'event files importing outside the allow-list');
  });

  test('G7 ephemeral replies keep the `ephemeral` option (no flags churn)', () => {
    // Reading a message's flags to detect an ephemeral message (`message.flags.has(MessageFlags.Ephemeral)`) is not
    // churn: the rule forbids *setting* reply flags. Only a bare MessageFlags.Ephemeral outside `.has(` counts.
    const hits = [...scan(/(?<!\.has(?:\?\.)?\(\s*)MessageFlags\.Ephemeral\b/g), ...scan(/\bflags\s*:\s*64\b/g)];
    expectNone(hits.map(where), 'MessageFlags.Ephemeral / flags: 64');
  });

  test('G8 Redis KEYS is never used', () => {
    expectNone(scan(/\.keys\(\s*[`'"]/g).map(where), '.keys("pattern") calls');
  });

  test('G9 every environment variable the code reads is documented in .env.example', () => {
    const found = new Map(); // name -> first location
    const note = (name, loc) => { if (!found.has(name)) found.set(name, loc); };
    const rawFiles = [...SRC_FILES, 'index.js'];
    for (const file of rawFiles) {
      const raw = readRepo(file);
      const lineOf = (index) => 1 + (raw.slice(0, index).match(/\n/g)?.length ?? 0);
      for (const m of raw.matchAll(/process\.env\.([A-Z][A-Z0-9_]*)/g)) note(m[1], `${file}:${lineOf(m.index)}`);
      for (const m of raw.matchAll(/process\.env\[\s*['"`]([A-Z][A-Z0-9_]*)['"`]\s*\]/g)) note(m[1], `${file}:${lineOf(m.index)}`);
    }
    for (const m of readRepo('prisma/schema.prisma').matchAll(/env\("([A-Z][A-Z0-9_]*)"\)/g)) note(m[1], 'prisma/schema.prisma');
    const documented = new Set([...readRepo('.env.example').matchAll(/^#?\s*([A-Z][A-Z0-9_]*)=/gm)].map((m) => m[1]));
    const problems = [...found].filter(([name]) => !RUNTIME_ENV.includes(name) && !documented.has(name))
      .map(([name, loc]) => `${name} (read at ${loc})`);
    expectNone(problems, 'env vars missing from .env.example');
  });

  test('G10 settings cache keys are never deleted directly', () => {
    expectNone(scan(/cache\.del\([^)]*:settings/g).map(where), 'cache.del on a settings key');
  });
});

// ── H. Docs consistency ──────────────────────────────────────────────────────

describe('H docs', () => {
  test('H1 the README command table lists every command exactly once', () => {
    const readme = readRepo('README.md');
    const rows = [...readme.matchAll(/^\|\s*`\/([a-z0-9-]+)`\s*\|/gm)].map((m) => m[1]);
    const actual = new Set(OK_COMMANDS.map((c) => c.payload.name));
    const table = new Set(rows);
    expectNoneOf([
      ['commands missing from the README table', diff(actual, table).map((n) => `/${n}`)],
      ['stale README rows (no such command)', diff(table, actual).map((n) => `/${n}`)],
      ['commands listed more than once in the README table', sortedUnique(rows.filter((n, i) => rows.indexOf(n) !== i)).map((n) => `/${n}`)],
    ]);
  });

  test('H2 the README names every module label', () => {
    const readme = readRepo('README.md');
    expectNone(MODULE_NAMES.filter((k) => !readme.includes(MODULES[k].label)).map((k) => `${k}: "${MODULES[k].label}"`), 'module labels missing from README.md');
  });

  test('H3 CLAUDE.md names every module, flag, job type and plugin file', () => {
    const doc = readRepo('CLAUDE.md');
    const inTicks = (s) => doc.includes(`\`${s}\``);
    expectNoneOf([
      ['module keys missing from CLAUDE.md', MODULE_NAMES.filter((k) => !inTicks(k)).map((k) => `\`${k}\``)],
      ['module flags missing from CLAUDE.md', MODULE_NAMES.filter((k) => !inTicks(MODULES[k].flag)).map((k) => `\`${MODULES[k].flag}\` (${k})`)],
      ['job types missing from CLAUDE.md', Object.keys(JOB_TYPES).filter((t) => !inTicks(t)).map((t) => `\`${t}\``)],
      ['plugin files missing from CLAUDE.md', [...PLUGIN_IDS].filter((id) => !inTicks(`src/plugins/${id}.js`)).map((id) => `\`src/plugins/${id}.js\``)],
    ]);
  });

  test('H4 .env.example documents the optional keys as comments only', () => {
    const env = readRepo('.env.example');
    const keys = ['HTTP_CONTACT', 'REDDIT_USER_AGENT', 'PRISMA_LOG_QUERIES'];
    expectNoneOf([
      ['.env.example lacks a commented line for', keys.filter((k) => !new RegExp(`^#\\s*${k}=`, 'm').test(env))],
      ['.env.example sets (uncommented) the optional key', keys.filter((k) => new RegExp(`^${k}=`, 'm').test(env))],
    ]);
  });

  test('H5 the research doc labels what was built and keeps the research-only section', () => {
    const doc = readRepo('docs/external-api-research.md');
    const stale = doc.split('\n').flatMap((line, i) => (/being built/i.test(line) ? [`line ${i + 1}: ${line.slice(0, 90)}`] : []));
    const labels = ['Built (keyless)', 'Built (Discord-only)'].filter((label) => !doc.includes(label));
    const heading = /^## 4\. Research only — not integrated$/m.exec(doc);
    let subHeadings = [];
    if (heading) {
      const rest = doc.slice(heading.index + heading[0].length);
      const next = rest.search(/^## /m);
      subHeadings = [...(next === -1 ? rest : rest.slice(0, next)).matchAll(/^### (.+)$/gm)].map((m) => m[1]);
    }
    const required = ['Twitch', 'YouTube Data API', 'Reddit Data API', 'X / Twitter', 'Instagram', 'TikTok', 'Kick', 'Music', 'AI'];
    expectNoneOf([
      ['the research doc still says "being built"', stale],
      ['status labels missing', labels],
      ['missing heading', heading ? [] : ['## 4. Research only — not integrated']],
      [`section 4 lacks subsections (has: ${subHeadings.join(' | ')})`, heading ? required.filter((name) => !subHeadings.some((h) => h.includes(name))).map((n) => `### ...${n}...`) : []],
    ]);
  });

  test('H6 every repo path named in CLAUDE.md and README.md exists', () => {
    const problems = ['CLAUDE.md', 'README.md'].flatMap((doc) => {
      const text = readRepo(doc);
      return [...text.matchAll(/`((?:src|test)\/[A-Za-z0-9_./-]+\.(?:js|cjs|mjs))`/g)]
        .filter((m) => !existsSync(join(ROOT, m[1])))
        .map((m) => `${doc}: \`${m[1]}\``);
    });
    expectNone(sortedUnique(problems), 'documented paths that do not exist');
  });

  test('H7 the docs do not mention the local audit folder', () => {
    const needle = '.audit' + '-checkpoint';
    const problems = ['CLAUDE.md', 'README.md'].filter((doc) => readRepo(doc).includes(needle));
    expectNone(problems, `docs mentioning "${needle}"`);
  });
});

// ── K. Contracts between packages ────────────────────────────────────────────

describe('K contracts', () => {
  const isFn = (v) => typeof v === 'function';

  test('K1 moderationService exports the shared moderation API', async () => {
    const m = await import('../src/services/moderationService.js');
    expectNone(['executeModAction', 'isImmune', 'createInfraction'].filter((n) => !isFn(m[n])), 'moderationService exports that are not functions');
    assert.deepEqual(m.MOD_SOURCES, ['command', 'automod', 'escalation', 'captcha', 'dashboard']);
  });

  test('K2 loggingService exports sendLog, extractMentions, channelTypeLabel, LOG_TYPES and LOG_EVENTS', async () => {
    const m = await import('../src/services/loggingService.js');
    expectNone(['sendLog', 'extractMentions', 'channelTypeLabel'].filter((n) => !isFn(m[n])), 'loggingService exports that are not functions');
    const have = new Set(Object.keys(m.LOG_TYPES));
    expectNone(['MESSAGE', 'MEMBER', 'SERVER', 'VOICE', 'JOIN_LEAVE', 'MOD'].filter((k) => !have.has(k)), 'LOG_TYPES keys missing');
    assert.ok(m.LOG_EVENTS !== null && typeof m.LOG_EVENTS === 'object', 'LOG_EVENTS must be an object');
  });

  test('K3 welcomeService exports assignAutorole', async () => {
    assert.ok(isFn((await import('../src/services/welcomeService.js')).assignAutorole));
  });

  test('K4 embedService exports getSavedEmbedTemplate and searchSavedEmbeds', async () => {
    const m = await import('../src/services/embedService.js');
    expectNone(['getSavedEmbedTemplate', 'searchSavedEmbeds'].filter((n) => !isFn(m[n])), 'embedService exports that are not functions');
  });

  test('K5 rewards exports the kinds, their modules and a synchronous isRewardAvailable', () => {
    assert.deepEqual([...rewards.REWARD_KINDS], ['xp', 'coins']);
    assert.deepEqual({ ...rewards.REWARD_MODULES }, { xp: 'leveling', coins: 'economy' });
    assert.ok(isFn(rewards.isRewardAvailable));
    assert.notEqual(rewards.isRewardAvailable.constructor.name, 'AsyncFunction', 'isRewardAvailable must be synchronous');
  });

  test('K6 deletionTracker marks and reads bot deletions', async () => {
    const m = await import('../src/utils/deletionTracker.js');
    assert.ok(isFn(m.markBotDeletion) && isFn(m.wasDeletedByBot));
    m.markBotDeletion('k6', 'test');
    assert.equal(m.wasDeletedByBot('k6'), 'test');
  });

  test('K7 economyService exports getAccount (the lazy global-to-server copy)', async () => {
    assert.ok(isFn((await import('../src/services/economyService.js')).getAccount));
  });

  test('K8 the levels and economy plugins provide xp and coins', () => {
    const provides = (id) => PLUGINS.find((p) => p.id === id)?.plugin?.provides ?? {};
    for (const [id, kind] of [['levels', 'xp'], ['economy', 'coins']]) {
      const impl = provides(id)[kind];
      assert.ok(impl && isFn(impl.grant) && isFn(impl.take), `${id} must provide ${kind} with grant() and take()`);
    }
  });
});

// ── P. Per-package test files ────────────────────────────────────────────────

test('P0 every package owns an existing test file', () => {
  const missing = PACKAGE_IDS.flatMap((id) => (TEST_FILES[id] ?? [`test/${id}.test.js`]).filter((p) => !existsSync(join(ROOT, p))));
  assert.deepEqual(missing, [], `missing test files: ${missing.join(', ')}`);
});

// ── Z. Runtime install (must stay last: the ONLY place installPlugins is called) ─

describe('Z runtime install (must stay last)', () => {
  const fake = Object.assign(new EventEmitter(), {
    user: null, shard: null, options: {}, commands: new Map(),
    guilds: { cache: new Map() }, channels: { cache: new Map() }, users: { cache: new Map() },
  });
  let res; let collected; let errorCalls;

  before(async () => {
    clearHooks();
    createHandlerMaps(fake);
    collected = await collectPlugins(); // async part BEFORE the spy window
    // Synchronous window: no await between spy and restore, so async noise (e.g. the Redis client's
    // rate-limited connection-error log) cannot interleave.
    const spy = mock.method(logger, 'error', () => {});
    try {
      res = installPlugins(fake, collected);
      registerCoreComponents(fake);
    } finally {
      errorCalls = spy.mock.calls.map((c) => c.arguments.map(String).join(' '));
      spy.mock.restore();
    }
  });

  test('Z1 the install reports and logs no errors', () => {
    assert.deepEqual(collected.errors, []);
    assert.deepEqual(res.errors, []);
    assert.deepEqual(errorCalls, []);
  });

  test('Z2 the install counts match the declarations', () => {
    assert.deepEqual(
      { hooks: res.hooks, components: res.components, jobs: res.jobs, providers: res.providers },
      { hooks: DECL_HOOKS.length, components: DECL_COMPONENTS.length, jobs: DECL_JOBS.length, providers: DECL_PROVIDERS.length },
    );
  });

  test('Z3 installed hooks run in exactly the HOOK_ORDER order', () => {
    const problems = [];
    for (const [event, ids] of Object.entries(HOOK_ORDER)) {
      const installed = listHooks(event);
      if (JSON.stringify(installed.map((h) => h.id)) !== JSON.stringify([...ids])) {
        problems.push(`${event}: installed [${installed.map((h) => h.id).join(', ')}], expected [${ids.join(', ')}]`);
        continue;
      }
      for (const h of installed) {
        const decl = DECL_HOOKS.find((d) => d.event === event && d.id === h.id);
        if (h.pluginId !== h.id.split(':')[0]) problems.push(`${event}:${h.id}: pluginId ${h.pluginId}`);
        if (!decl) { problems.push(`${event}:${h.id}: no declaration`); continue; }
        if (h.module !== decl.module) problems.push(`${event}:${h.id}: module ${JSON.stringify(h.module)} (declared ${JSON.stringify(decl.module)})`);
        if (h.always !== decl.always) problems.push(`${event}:${h.id}: always ${h.always} (declared ${decl.always})`);
      }
    }
    expectNone(problems, 'installed hook problems');
  });

  test('Z4 the component maps hold exactly the persistent prefixes', () => {
    const problems = [];
    for (const [type, map] of Object.entries(MAP_OF)) {
      const expected = Object.entries(CUSTOM_ID_PREFIXES)
        .filter(([, info]) => info.kind === 'persistent' && info.types.includes(type)).map(([prefix]) => prefix).sort();
      for (const prefix of expected) {
        if (!fake[map].has(prefix)) problems.push(`${map} lacks "${prefix}"`);
        else if (typeof fake[map].get(prefix) !== 'function') problems.push(`${map}["${prefix}"] is not a function`);
      }
      for (const extra of [...fake[map].keys()].filter((k) => !expected.includes(k))) problems.push(`${map} has the unexpected key "${extra}"`);
    }
    expectNone(problems, 'component map problems');
  });

  test('Z5 the reward providers are registered', async () => {
    assert.equal(rewards.hasProvider('xp'), true);
    assert.equal(rewards.hasProvider('coins'), true);
    assert.equal(rewards.isRewardAvailable('xp', { xpEnabled: true }), true);
    assert.equal(rewards.isRewardAvailable('xp', { xpEnabled: false }), false);
    assert.equal(rewards.isRewardAvailable('coins', { economyEnabled: true }), true);
    assert.equal(rewards.isRewardAvailable('coins', null), false);
    // Validation runs before the provider, so this never reaches the database.
    assert.deepEqual(await rewards.grant('xp', { guild: null, userId: '1', amount: 0 }), { ok: false, reason: 'invalid-amount' });
  });

  test('Z6 collectPlugins() is idempotent (installPlugins is never called twice)', async () => {
    const again = await collectPlugins();
    assert.deepEqual(again.errors, []);
    assert.deepEqual(again.plugins.map((p) => p.id).sort(), collected.plugins.map((p) => p.id).sort());
  });
});
