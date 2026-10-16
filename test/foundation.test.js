// Foundation core tests (01-foundation §18.0): config registries, settings cache validity,
// scheduler conflict resolution, SQL helpers, async helpers, permission levels, the never-throw
// Redis cache and the rewards bus. Everything runs offline; the DB cases at the bottom are skipped
// unless TEST_DATABASE_URL points at a migrated database (see .audit-checkpoint/DB-TESTING.md).
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV      ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID     ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is only reached with TEST_DATABASE_URL.
process.env.REDIS_URL     = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL  = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { PermissionFlagsBits } = await import('discord.js');
const { Prisma } = await import('@prisma/client');
const constants = await import('../src/config/constants.js');
const modules = await import('../src/config/modules.js');
const hookPoints = await import('../src/config/hookPoints.js');
const customIds = await import('../src/config/customIds.js');
const jobTypes = await import('../src/config/jobTypes.js');
const settingsSvc = await import('../src/services/settingsService.js');
const scheduler = await import('../src/services/scheduler.js');
const rewards = await import('../src/services/rewards.js');
const { default: redis, cache } = await import('../src/services/redis.js');
const { default: prisma, SQL_NOW, sqlUtc } = await import('../src/database/client.js');
const asyncUtils = await import('../src/utils/async.js');
const { resolvePermissionLevel } = await import('../src/utils/permissionChecker.js');
const { default: logger } = await import('../src/utils/logger.js');

const SqlClass = Prisma.sql``.constructor; // Prisma.Sql is not exported as a value in every Prisma build
const { MODULES, MODULE_NAMES, MODULE_FLAGS, isModuleEnabled, getModule, isModuleKey } = modules;
const { PERMISSION_LEVELS, DEFAULT_MODULE_SETTINGS, LIMITS } = constants;
const {
  SETTINGS_CACHE_VERSION, CACHE_VERSION_FIELD, isValidCachedSettings, reviveSettings, settingsKey,
  getSettings, getSettingsSafe, peekSettings, ensureGuildSettings, updateSettings, updateSettingsWith, invalidateSettings,
} = settingsSvc;

// Redis reconnect and error noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug };
const mute = () => { console.error = console.warn = console.log = console.debug = () => {}; };
const unmute = () => Object.assign(console, quiet);
mute();
after(async () => {
  unmute();
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── 1. Config tables ─────────────────────────────────────────────────────────

test('MODULES, DEFAULT_MODULE_SETTINGS and the schema agree', () => {
  assert.deepEqual(Object.keys(MODULES).sort(), Object.keys(DEFAULT_MODULE_SETTINGS).sort());
  const schema = readFileSync(resolve(ROOT, 'prisma/schema.prisma'), 'utf8');
  const model = schema.match(/model GuildSettings \{([\s\S]*?)\n\}/)[1];
  for (const key of MODULE_NAMES) {
    const mod = MODULES[key];
    assert.equal(mod.default, DEFAULT_MODULE_SETTINGS[key], `${key} default`);
    const decl = model.match(new RegExp(`\\n\\s+${mod.flag}\\s+Boolean\\s+@default\\((true|false)\\)`));
    assert.ok(decl, `${mod.flag} is declared in GuildSettings as Boolean @default(...)`);
    assert.equal(decl[1] === 'true', mod.default, `${mod.flag} schema default`);
  }
  assert.deepEqual(MODULE_FLAGS, Object.fromEntries(MODULE_NAMES.map((k) => [k, MODULES[k].flag])));
  assert.equal(MODULE_FLAGS.leveling, 'xpEnabled');
  assert.ok(Object.isFrozen(MODULES) && Object.isFrozen(MODULE_FLAGS) && Object.isFrozen(MODULE_NAMES));
  assert.equal(new Set(Object.values(MODULE_FLAGS)).size, MODULE_NAMES.length, 'flags are unique');
});

test('isModuleEnabled, getModule and isModuleKey', () => {
  assert.equal(isModuleEnabled({}, null), true, 'null key is core');
  assert.equal(isModuleEnabled({}, undefined), true);
  assert.equal(isModuleEnabled(null, 'moderation'), false, 'null settings');
  assert.equal(isModuleEnabled({ modEnabled: true }, 'nope'), false, 'unknown key');
  assert.equal(isModuleEnabled({ modEnabled: true }, 'moderation'), true);
  assert.equal(isModuleEnabled({ modEnabled: false }, 'moderation'), false);
  assert.equal(isModuleEnabled({ modEnabled: 1 }, 'moderation'), false, 'strictly true');
  assert.equal(isModuleEnabled({ xpEnabled: true }, 'leveling'), true);
  assert.equal(isModuleEnabled({ levelingEnabled: true }, 'leveling'), false, 'levelingEnabled stays unread');
  assert.equal(getModule('toString'), null, 'no prototype keys');
  assert.equal(getModule('games').flag, 'gamesEnabled');
  assert.equal(isModuleKey('games'), true);
  assert.equal(isModuleKey('constructor'), false);
  assert.equal(settingsSvc.isModuleEnabled, isModuleEnabled, 'settingsService re-exports it');
});

test('HOOK_ORDER ids are well-formed and hookIndex works', () => {
  const ID_RE = /^[a-z0-9-]+(:[a-z0-9-]+)?$/;
  for (const [event, ids] of Object.entries(hookPoints.HOOK_ORDER)) {
    assert.equal(new Set(ids).size, ids.length, `${event}: no duplicate ids`);
    for (const id of ids) assert.match(id, ID_RE, `${event}: ${id}`);
  }
  assert.deepEqual([...hookPoints.INTERNAL_HOOK_POINTS], ['levelUp', 'economyPurchase']);
  assert.deepEqual([...hookPoints.SYNTHETIC_HOOK_POINTS], ['guildReady']);
  for (const e of [...hookPoints.INTERNAL_HOOK_POINTS, ...hookPoints.SYNTHETIC_HOOK_POINTS]) assert.ok(e in hookPoints.HOOK_ORDER);
  assert.equal(hookPoints.hookIndex('messageCreate', 'automod'), 0);
  assert.equal(hookPoints.hookIndex('messageCreate', 'custom-commands:triggers'), 7);
  assert.equal(hookPoints.hookIndex('messageCreate', 'nope'), -1);
  assert.equal(hookPoints.hookIndex('noSuchEvent', 'automod'), -1);
});

test('CUSTOM_ID_PREFIXES and buildCustomId', () => {
  const { CUSTOM_ID_PREFIXES, buildCustomId, parseCustomId, prefixInfo, CUSTOM_ID_MAX_LENGTH } = customIds;
  assert.equal(CUSTOM_ID_MAX_LENGTH, 100);
  for (const [prefix, info] of Object.entries(CUSTOM_ID_PREFIXES)) {
    assert.match(prefix, /^[a-z][a-z0-9-]*$/);
    assert.ok(['persistent', 'collector', 'field'].includes(info.kind), prefix);
    assert.ok(info.types.every((t) => ['button', 'select', 'modal', 'field'].includes(t)), prefix);
    assert.ok(info.owner, prefix);
  }
  assert.equal(buildCustomId('rr', 'g1', 'r2'), 'rr:g1:r2');
  assert.equal(buildCustomId('noop'), 'noop');
  assert.deepEqual(parseCustomId('rr:g1:r2'), { prefix: 'rr', params: ['g1', 'r2'] });
  assert.deepEqual(parseCustomId('noop'), { prefix: 'noop', params: [] });
  assert.equal(prefixInfo('rr').owner, 'reaction-roles');
  assert.equal(prefixInfo('nope'), null);
  assert.throws(() => buildCustomId('nope'), /Unknown customId prefix/);
  assert.throws(() => buildCustomId('welcome-title', 'x'), /field/);
  assert.throws(() => buildCustomId('rr', 'a:b'), /':'/);
  assert.throws(() => buildCustomId('rr', 'x'.repeat(100)), /max 100/);
  assert.equal(buildCustomId('rr', 'x'.repeat(96)).length, 99);
});

test('JOB_TYPES keys are well-formed; RECURRING_JOB_TYPES equals the recurring keys', () => {
  const { JOB_TYPES, RECURRING_JOB_TYPES, isJobType } = jobTypes;
  for (const [type, info] of Object.entries(JOB_TYPES)) {
    assert.match(type, /^[a-z][a-z0-9-]*\.[a-z][a-zA-Z0-9.-]*$/);
    assert.ok(typeof info.owner === 'string' && info.owner.length, type);
    assert.equal(typeof info.recurring, 'boolean');
  }
  assert.deepEqual([...RECURRING_JOB_TYPES].sort(), Object.keys(JOB_TYPES).filter((t) => JOB_TYPES[t].recurring).sort());
  assert.ok(RECURRING_JOB_TYPES.includes('feed.poll') && !RECURRING_JOB_TYPES.includes('mod.unban'));
  assert.equal(isJobType('mod.unban'), true);
  assert.equal(isJobType('toString'), false);
  assert.equal(isJobType(5), false);
});

// ── 2. LIMITS ────────────────────────────────────────────────────────────────

test('LIMITS defines every key as a positive integer', () => {
  const keys = (`SAVED_EMBEDS REMINDERS ACTIVE_GIVEAWAYS GIVEAWAY_WINNERS ACTIVE_POLLS POLL_OPTIONS STARBOARDS TICKET_PANELS
    TICKET_PANEL_OPTIONS TICKET_MANAGER_ROLES TEMPVOICE_HUBS STAT_CHANNELS ROLE_STAT_CHANNELS ACHIEVEMENTS ACHIEVEMENT_TIERS
    AUTOMATIONS AUTOMATION_ACTIONS AUTOMATION_CONDITIONS SHOP_ITEMS ITEM_COPIES_PER_USER CUSTOM_TAGS TAG_ACTIONS TAG_RESPONSES
    AUTO_TRIGGERS FEEDS ESCALATION_TIERS AUTOMOD_WORDS AUTOMOD_PATTERNS AUTOMOD_DOMAINS REACTION_ROLE_PANELS LEVEL_ROLES
    XP_BOOST_ROLES IMMUNE_ROLES MOD_ROLES AUTOROLES AUTOMOD_EXEMPTIONS LOG_IGNORED_CHANNELS NOXP_ROLES NOXP_CHANNELS
    TAG_ACCESS_ENTRIES TRIGGER_SCOPE_ENTRIES TAG_PING_ROLES TRANSLATE_CHANNELS ECONOMY_GAME_CHANNELS ECONOMY_BOOSTS`).split(/\s+/);
  for (const key of keys) assert.ok(Number.isInteger(LIMITS[key]) && LIMITS[key] > 0, `LIMITS.${key}`);
  for (const key of ['AUTOMOD_EXEMPTIONS', 'LOG_IGNORED_CHANNELS', 'NOXP_ROLES', 'NOXP_CHANNELS', 'TAG_ACCESS_ENTRIES',
    'TRIGGER_SCOPE_ENTRIES', 'TAG_PING_ROLES', 'TRANSLATE_CHANNELS', 'ECONOMY_GAME_CHANNELS', 'ECONOMY_BOOSTS']) {
    assert.ok(key in LIMITS, `review key ${key}`);
  }
  assert.ok(Object.isFrozen(LIMITS));
  assert.equal(constants.XP_MAX, 1_000_000_000);
  assert.equal(constants.COIN_MAX, 1_000_000_000);
  assert.equal(constants.XP_DEFAULTS.min, 15);
  assert.equal(constants.XP_DEFAULTS.max, 25);
  assert.equal(constants.xpForLevel(0), 100);
});

// ── 3. Settings cache validity ───────────────────────────────────────────────

test('isValidCachedSettings', () => {
  assert.equal(isValidCachedSettings({ guildId: 'g', modEnabled: false }, 'g'), false, 'old shape, no marker');
  assert.equal(isValidCachedSettings({ guildId: 'g', [CACHE_VERSION_FIELD]: SETTINGS_CACHE_VERSION }, 'g'), true);
  assert.equal(isValidCachedSettings({ guildId: 'other', [CACHE_VERSION_FIELD]: SETTINGS_CACHE_VERSION }, 'g'), false);
  assert.equal(isValidCachedSettings({ guildId: 'g', _cacheV: 1 }, 'g'), false);
  assert.equal(isValidCachedSettings(null, 'g'), false);
  assert.equal(isValidCachedSettings('x', 'g'), false);
  assert.equal(isValidCachedSettings([], 'g'), false);
  assert.equal(settingsKey('42'), 'guild:42:settings');
});

test('reviveSettings strips the marker, revives dates and deep-freezes', () => {
  const input = {
    guildId: 'g', _cacheV: 2, createdAt: '2026-01-01T00:00:00.000Z', modRoleIds: ['1'],
    automodConfig: { rules: { caps: { roleIds: [] } } },
  };
  const row = reviveSettings(input);
  assert.equal(CACHE_VERSION_FIELD in row, false);
  assert.ok(row.createdAt instanceof Date);
  assert.equal(row.createdAt.toISOString(), '2026-01-01T00:00:00.000Z');
  for (const frozen of [row, row.modRoleIds, row.automodConfig, row.automodConfig.rules.caps, row.automodConfig.rules.caps.roleIds]) {
    assert.ok(Object.isFrozen(frozen));
  }
  assert.throws(() => { 'use strict'; row.modRoleIds.push('2'); }, TypeError);
  assert.equal(reviveSettings(null), null);
  assert.equal(CACHE_VERSION_FIELD in input, true, 'the caller object keeps its marker');
});

test('peekSettings is memo-only and never does I/O', () => {
  assert.equal(peekSettings('9550000000000999'), null);
});

test('getSettingsSafe returns null instead of throwing when Postgres is down', { skip: DB ? 'a database is available' : false }, async () => {
  assert.equal(await getSettingsSafe('9550000000000998'), null);
  await assert.rejects(getSettings('9550000000000998'));
});

// ── 4. Scheduler conflict resolution ─────────────────────────────────────────

test('resolveConflict', () => {
  const { resolveConflict } = scheduler;
  const early = new Date('2026-01-01T00:00:00Z');
  const late = new Date('2026-01-02T00:00:00Z');
  for (const mode of ['replace', 'keep', 'earliest']) assert.equal(resolveConflict(null, { runAt: early }, mode), 'insert');
  for (const status of ['PENDING', 'RUNNING']) {
    const existing = { status, runAt: late };
    assert.equal(resolveConflict(existing, { runAt: early }, 'replace'), 'replace', `${status} replace`);
    assert.equal(resolveConflict(existing, { runAt: early }, 'keep'), 'keep', `${status} keep`);
    assert.equal(resolveConflict(existing, { runAt: early }, 'earliest'), 'replace', `${status} earliest, earlier`);
    assert.equal(resolveConflict(existing, { runAt: late }, 'earliest'), 'keep', `${status} earliest, same`);
    assert.equal(resolveConflict({ status, runAt: early }, { runAt: late }, 'earliest'), 'keep', `${status} earliest, later`);
  }
  for (const status of ['FAILED', 'DONE', 'CANCELLED']) {
    for (const mode of ['replace', 'keep', 'earliest']) {
      assert.equal(resolveConflict({ status, runAt: early }, { runAt: late }, mode), 'replace', `${status} ${mode}`);
    }
  }
  assert.deepEqual([...scheduler.LIVE_STATUSES], ['PENDING', 'RUNNING']);
});

test('scheduler handler registry and offline tick', async () => {
  const { registerJobHandler, tick, scheduleJob, cancelJobs } = scheduler;
  assert.throws(() => registerJobHandler('no.such', () => {}), /Unknown job type/);
  const handler = async () => {};
  registerJobHandler('poll.end', handler);
  registerJobHandler('poll.end', handler); // same handler again is fine
  assert.throws(() => registerJobHandler('poll.end', async () => {}), /already registered/);
  assert.ok(new scheduler.PermanentJobError('x') instanceof Error);
  // Unavailable guilds are never claimed: no query is issued at all.
  const client = { guilds: { cache: new Map([['1', { id: '1', available: false }]]) }, shard: null };
  assert.equal(await tick(client), 0);
  assert.equal(await tick({ guilds: { cache: new Map() } }), 0);
  // Validation happens before any database access.
  const base = { guildId: '1', type: 'poll.end', runAt: new Date() };
  await assert.rejects(scheduleJob({ ...base, type: 'no.such' }), /Unknown job type/);
  await assert.rejects(scheduleJob({ ...base, runAt: new Date('nope') }), /Invalid runAt/);
  const cyclic = {}; cyclic.self = cyclic;
  await assert.rejects(scheduleJob({ ...base, payload: cyclic }), /not JSON-serialisable/);
  await assert.rejects(scheduleJob({ ...base, payload: { n: 1n } }), /not JSON-serialisable/);
  await assert.rejects(scheduleJob({ ...base, onConflict: 'sideways' }), /onConflict/);
  assert.equal(await cancelJobs({}), 0, 'no filter cancels nothing');
  assert.equal(await cancelJobs({ types: [] }), 0, 'an empty filter list matches nothing');
  assert.equal(await cancelJobs({ ids: [] }), 0);
  await scheduler.stop({ timeoutMs: 50 });
});

test('scheduler slot loop: a slow job never holds up the jobs due behind it', async () => {
  const { registerJobHandler, start, stop, fillSlots, JOB_CONCURRENCY } = scheduler;
  const total = JOB_CONCURRENCY + 3;
  const queue = Array.from({ length: total }, (_, n) => ({
    id: `job-${n}`, guildId: '7', type: 'reminder.fire', payload: { slow: n === 0 }, runAt: new Date(),
    attempts: 1, maxAttempts: 3, dedupeKey: null, version: 0,
  }));
  let releaseSlow;
  const slow = new Promise((resolve) => { releaseSlow = resolve; });
  const done = [];
  let active = 0;
  let peak = 0;
  registerJobHandler('reminder.fire', async (job) => {
    active++;
    peak = Math.max(peak, active);
    try {
      if (job.payload.slow) await slow; else await new Promise((resolve) => { setImmediate(resolve); });
      done.push(job.id);
    } finally {
      active--;
    }
  });
  // Ordinary claims hand out the queue, at most `LIMIT` rows each; the interactive loop's claims get nothing.
  const limits = [];
  const real = { queryRaw: prisma.$queryRaw, deleteMany: prisma.scheduledJob.deleteMany };
  prisma.$queryRaw = async (sql) => {
    if (!sql.strings.join('?').includes('NOT (type')) return [];
    const limit = sql.values.find((v) => typeof v === 'number');
    limits.push(limit);
    return queue.splice(0, limit);
  };
  prisma.scheduledJob.deleteMany = async () => ({ count: 1 });
  const eventually = async (fn, ms = 2000) => {
    const end = Date.now() + ms;
    while (!fn()) {
      if (Date.now() > end) return false;
      await new Promise((resolve) => { setTimeout(resolve, 5); });
    }
    return true;
  };
  try {
    // Shard 1: no GC timer. The 5 s poll never fires during the test: every claim after the first is a refill.
    const client = { guilds: { cache: new Map([['7', { id: '7', available: true }]]) }, shard: { ids: [1] } };
    start(client);
    assert.equal(await fillSlots(client), JOB_CONCURRENCY, 'claims exactly the free slots');
    assert.ok(await eventually(() => done.length === total - 1), 'every other job ran while the slow one still holds a slot');
    assert.equal(done.includes('job-0'), false);
    assert.ok(peak <= JOB_CONCURRENCY, `never more than ${JOB_CONCURRENCY} at once (peak ${peak})`);
    assert.ok(limits.every((l) => l >= 1 && l <= JOB_CONCURRENCY), `claims never exceed the free slots (${limits})`);
    // The queue is drained, so the last claim came back short: finishing the slow job claims nothing more.
    const claims = limits.length;
    releaseSlow();
    assert.ok(await eventually(() => done.includes('job-0')));
    await new Promise((resolve) => { setImmediate(resolve); });
    assert.equal(limits.length, claims, 'no backlog: a finished job leaves new work to the poll');
    await stop({ timeoutMs: 1000 });
  } finally {
    releaseSlow();
    await stop({ timeoutMs: 50 });
    prisma.$queryRaw = real.queryRaw;
    prisma.scheduledJob.deleteMany = real.deleteMany;
  }
});

// ── 5. SQL helpers ───────────────────────────────────────────────────────────

test('SQL_NOW and sqlUtc', () => {
  assert.ok(SQL_NOW instanceof SqlClass);
  assert.equal(SQL_NOW.sql, "(now() AT TIME ZONE 'UTC')");
  const frag = sqlUtc(new Date('2026-01-02T03:04:05.006Z'));
  assert.ok(frag instanceof SqlClass);
  assert.deepEqual(frag.values, ['2026-01-02T03:04:05.006Z']);
  assert.ok(frag.sql.includes("::timestamptz AT TIME ZONE 'UTC'"));
  assert.throws(() => sqlUtc('x'), TypeError);
  assert.throws(() => sqlUtc(new Date('nope')), TypeError);
  assert.throws(() => sqlUtc(Date.now()), TypeError);
});

test('RAW_SQL builders return Prisma.Sql without a bare now()', () => {
  for (const { name, build } of [...scheduler.RAW_SQL, ...settingsSvc.RAW_SQL]) {
    const sql = build();
    assert.ok(sql instanceof SqlClass, name);
    assert.doesNotMatch(sql.sql.replace(/now\(\) AT TIME ZONE 'UTC'/gi, ''), /\bnow\(\)|CURRENT_TIMESTAMP/i, name);
  }
});

// ── 6. async helpers ─────────────────────────────────────────────────────────

test('ackBudgetMs', () => {
  const { ackBudgetMs } = asyncUtils;
  const a = ackBudgetMs({ createdTimestamp: Date.now() - 1000 });
  assert.ok(a >= 1100 && a <= 1200, `got ${a}`);
  assert.equal(ackBudgetMs({ createdTimestamp: Date.now() - 5000 }), 0);
  const b = ackBudgetMs({ createdTimestamp: Date.now() - 1000 }, 0);
  assert.ok(b >= 1500 && b <= 1600, `got ${b}`);
});

test('withTimeout, sleep and a rejection after the timeout is logged, not unhandled', async () => {
  const { withTimeout, TimeoutError, sleep } = asyncUtils;
  assert.equal(await withTimeout(Promise.resolve(7), 50, 'fast'), 7);
  await assert.rejects(withTimeout(Promise.reject(new Error('inner')), 50, 'rej'), /inner/);
  const unhandled = [];
  const onUnhandled = (err) => unhandled.push(err);
  process.on('unhandledRejection', onUnhandled);
  try {
    const slow = new Promise((_, reject) => setTimeout(() => reject(new Error('late')), 40));
    await assert.rejects(withTimeout(slow, 5, 'slow thing'), (err) => err instanceof TimeoutError && /slow thing/.test(err.message));
    await sleep(90);
    assert.deepEqual(unhandled, []);
  } finally {
    process.off('unhandledRejection', onUnhandled);
  }
});

test('mapLimit keeps order, respects the limit and never rejects', async () => {
  const { mapLimit, sleep } = asyncUtils;
  let running = 0;
  let peak = 0;
  const out = await mapLimit([1, 2, 3, 4, 5, 6], 2, async (n) => {
    running++; peak = Math.max(peak, running);
    await sleep(5 * (7 - n));
    running--;
    if (n === 3) throw new Error('three');
    return n * 10;
  });
  assert.equal(peak, 2);
  assert.deepEqual(out.map((r) => r.status), ['fulfilled', 'fulfilled', 'rejected', 'fulfilled', 'fulfilled', 'fulfilled']);
  assert.deepEqual(out.filter((r) => r.status === 'fulfilled').map((r) => r.value), [10, 20, 40, 50, 60]);
  assert.equal(out[2].reason.message, 'three');
  assert.deepEqual(await mapLimit([], 3, async () => 1), []);
});

test('createKeyedMutex serialises per key, links synchronously and isolates rejections', async () => {
  const { createKeyedMutex, sleep } = asyncUtils;
  const mutex = createKeyedMutex();
  const order = [];
  const first = mutex.run('k', async () => { await sleep(20); order.push('first'); });
  const second = mutex.run('k', async () => { order.push('second'); });
  const other = mutex.run('other', async () => { order.push('other'); });
  await Promise.all([first, second, other]);
  assert.ok(order.indexOf('first') < order.indexOf('second'));
  assert.ok(order.indexOf('other') < order.indexOf('first'), 'a different key does not wait');

  const bad = mutex.run('k', async () => { throw new Error('bad'); });
  const good = mutex.run('k', async () => 'good');
  await assert.rejects(bad, /bad/);
  assert.equal(await good, 'good');

  // fn is linked into the chain before run() returns, even when called without awaiting.
  const seen = [];
  const a = mutex.run('sync', async () => { seen.push('a'); await sleep(10); seen.push('a-end'); });
  const b = mutex.run('sync', async () => { seen.push('b'); });
  await Promise.all([a, b]);
  assert.deepEqual(seen, ['a', 'a-end', 'b']);
});

test('createThrottle allows one call per interval per key', async () => {
  const { createThrottle, sleep } = asyncUtils;
  const allow = createThrottle(30);
  assert.equal(allow('a'), true);
  assert.equal(allow('a'), false);
  assert.equal(allow('b'), true);
  await sleep(45);
  assert.equal(allow('a'), true);
});

// ── 7. permissionChecker ─────────────────────────────────────────────────────

function fakeMember({ id = '10', ownerId = '99', perms = [], roles = [] } = {}) {
  const flags = new Set(perms);
  return { id, guild: { ownerId }, permissions: { has: (flag) => flags.has(flag) }, roles: { cache: new Map(roles.map((r) => [r, {}])) } };
}

test('resolvePermissionLevel', () => {
  const { MEMBER, MODERATOR, ADMIN, OWNER } = PERMISSION_LEVELS;
  const settings = { modRoleIds: ['mod-role'] };
  assert.equal(resolvePermissionLevel(fakeMember({ id: '5', ownerId: '5' }), settings), ADMIN, 'guild owner');
  assert.equal(resolvePermissionLevel(fakeMember({ perms: [PermissionFlagsBits.Administrator] }), settings), ADMIN);
  assert.equal(resolvePermissionLevel(fakeMember({ perms: [PermissionFlagsBits.ManageGuild] }), settings), ADMIN);
  assert.equal(resolvePermissionLevel(fakeMember({ perms: [PermissionFlagsBits.ModerateMembers] }), settings), MODERATOR);
  assert.equal(resolvePermissionLevel(fakeMember({ perms: [PermissionFlagsBits.BanMembers] }), settings), MODERATOR);
  assert.equal(resolvePermissionLevel(fakeMember({ roles: ['mod-role'] }), settings), MODERATOR, 'mod role');
  assert.equal(resolvePermissionLevel(fakeMember({ roles: ['other'] }), settings), MEMBER);
  assert.equal(resolvePermissionLevel(fakeMember({ perms: [PermissionFlagsBits.KickMembers] }), null), MODERATOR, 'null settings');
  assert.equal(resolvePermissionLevel(fakeMember({ roles: ['mod-role'] }), null), MEMBER);
  assert.equal(resolvePermissionLevel(fakeMember({ perms: [PermissionFlagsBits.SendMessages] }), settings), MEMBER);
  assert.equal(resolvePermissionLevel(fakeMember(), settings), MEMBER);
  assert.equal(resolvePermissionLevel(null, settings), MEMBER);
  const before = process.env.BOT_OWNER_ID;
  process.env.BOT_OWNER_ID = '1234';
  try {
    assert.equal(resolvePermissionLevel(fakeMember({ id: '1234' }), settings), OWNER);
    assert.equal(resolvePermissionLevel(fakeMember({ id: '1235' }), settings), MEMBER);
  } finally {
    if (before === undefined) delete process.env.BOT_OWNER_ID; else process.env.BOT_OWNER_ID = before;
  }
});

// ── 8. Logger ────────────────────────────────────────────────────────────────

test('logger inspects context objects and prefixes the shard id', () => {
  const lines = [];
  const prevShards = process.env.SHARDS;
  console.error = (line) => lines.push(String(line));
  console.log = (line) => lines.push(String(line));
  try {
    logger.error('boom', { guildId: '1', nested: { a: [1, 2] } });
    logger.info('plain', 5, null);
    process.env.SHARDS = '3';
    logger.info('with shard');
  } finally {
    if (prevShards === undefined) delete process.env.SHARDS; else process.env.SHARDS = prevShards;
    mute();
  }
  assert.match(lines[0], /boom \{ guildId: '1', nested: \{ a: \[ 1, 2 \] \} \}/);
  assert.doesNotMatch(lines[0], /\[object Object\]/);
  assert.match(lines[1], /plain 5 null/);
  assert.match(lines[2], /\[Shard 3\][\s\S]*with shard/);
  assert.doesNotMatch(lines[1], /Shard/);
});

// ── 9. never-throw Redis cache (Redis is down in this test) ───────────────────

test('cache.* never throws or rejects and returns the documented fallbacks', async () => {
  const started = Date.now();
  assert.equal(cache.available(), false);
  assert.equal(await cache.get('k'), null);
  assert.equal(await cache.set('k', { a: 1 }, 10), false);
  assert.equal(await cache.setNX('k', 1, 10), null);
  assert.equal(await cache.del('k'), false);
  assert.equal(await cache.del('a', 'b'), false);
  assert.equal(await cache.incr('c', 10), null);
  assert.equal(await cache.slidingWindow('w', 1000), null);
  assert.equal(await cache.pushRecent('r', 'x', 5, 10), null);
  assert.equal(await cache.hget('h', 'f'), null);
  assert.equal(await cache.hset('h', 'f', 1), false);
  assert.equal(await cache.hdel('h', 'f'), false);
  assert.equal(await cache.invalidate('guild:*:settings'), 0);
  assert.equal(await cache.set('big', { n: 1n }, 10), false, 'BigInt does not serialise');
  const cyclic = {}; cyclic.self = cyclic;
  assert.equal(await cache.set('cyc', cyclic, 10), false, 'cycle does not serialise');
  assert.ok(Date.now() - started < 3000, 'fails fast instead of queueing');
});

// ── 10. rewards bus ──────────────────────────────────────────────────────────

test('rewards bus', async () => {
  assert.deepEqual([...rewards.REWARD_KINDS], ['xp', 'coins']);
  assert.deepEqual({ ...rewards.REWARD_MODULES }, { xp: 'leveling', coins: 'economy' });
  assert.equal(rewards.providerModule('coins'), 'economy');
  assert.equal(rewards.providerModule('gems'), null);
  assert.equal(rewards.hasProvider('coins'), false);
  assert.equal(rewards.isRewardAvailable('coins', { economyEnabled: true }), false, 'no provider yet');
  assert.deepEqual(await rewards.grant('coins', { guild: {}, userId: '1', amount: 5 }), { ok: false, reason: 'unavailable' });
  assert.deepEqual(await rewards.take('coins', { guild: {}, userId: '1', amount: 5 }), { ok: false, reason: 'unavailable' });

  assert.throws(() => rewards.registerProvider('gems', { grant() {}, take() {} }, 'x'), /Unknown reward kind/);
  assert.throws(() => rewards.registerProvider('coins', { grant() {} }, 'x'), /grant\(\) and take\(\)/);
  const calls = [];
  rewards.registerProvider('coins', {
    async grant(args) { calls.push(['grant', args]); return args.userId === 'broke' ? { ok: false, reason: 'module-disabled' } : { ok: true, total: 105 }; },
    async take(args) { calls.push(['take', args]); throw new Error('provider exploded'); },
  }, 'economy');
  assert.throws(() => rewards.registerProvider('coins', { grant() {}, take() {} }, 'again'), /already registered/);
  assert.equal(rewards.hasProvider('coins'), true);
  assert.equal(rewards.isRewardAvailable('coins', { economyEnabled: true }), true);
  assert.equal(rewards.isRewardAvailable('coins', { economyEnabled: false }), false);
  assert.equal(rewards.isRewardAvailable('coins', null), false);
  assert.equal(rewards.isRewardAvailable('xp', { xpEnabled: true }), false, 'xp has no provider');

  assert.deepEqual(await rewards.grant('coins', { guild: { id: 'g' }, userId: '1', amount: 5, reason: 'test' }), { ok: true, total: 105 });
  assert.deepEqual(calls[0][1], { guild: { id: 'g' }, userId: '1', amount: 5, reason: 'test', member: null, channel: null });
  assert.deepEqual(await rewards.grant('coins', { guild: {}, userId: 'broke', amount: 5 }), { ok: false, reason: 'module-disabled' });
  assert.deepEqual(await rewards.take('coins', { guild: {}, userId: '1', amount: 5 }), { ok: false, reason: 'error' }, 'provider errors are caught');
  for (const amount of [0, -1, 1.5, '5', NaN, 1_000_000_001, undefined, null]) {
    assert.deepEqual(await rewards.grant('coins', { guild: {}, userId: '1', amount }), { ok: false, reason: 'invalid-amount' }, `amount ${amount}`);
  }
  assert.deepEqual(await rewards.grant('coins', { guild: {}, userId: '1', amount: 1_000_000_000 }), { ok: true, total: 105 });
  assert.deepEqual(await rewards.grant('coins'), { ok: false, reason: 'invalid-amount' });
});

// ── DB: SQL helpers, settingsService and the scheduler against Postgres ───────

const ID = (n) => `95500000000${String(n).padStart(5, '0')}`;
async function cleanup(...guildIds) {
  await prisma.guildSettings.deleteMany({ where: { guildId: { in: guildIds } } });
  for (const id of guildIds) await invalidateSettings(id);
}

const seen = [];
scheduler.registerJobHandler('mod.unban', async (job, ctx) => {
  seen.push({ id: job.id, attempt: ctx.attempt, lateByMs: ctx.lateByMs, available: ctx.guild.available, version: job.version });
  switch (job.payload.mode) {
    case 'resched': return { nextRunAt: new Date(Date.now() + 3_600_000), payload: { mode: 'ok', second: true } };
    case 'throw': throw new Error('boom '.repeat(1000));
    case 'permanent': throw new scheduler.PermanentJobError('never retry');
    default: return undefined;
  }
});

const fakeClient = (ids, { cacheMiss = false } = {}) => {
  const guilds = new Map(ids.map((id) => [id, { id, available: true }]));
  if (cacheMiss) guilds.get = () => undefined;
  return { guilds: { cache: guilds }, shard: null };
};
const inSeconds = (s) => new Date(Date.now() + s * 1000);
const rolledBack = Symbol('rollback');

test('DB: SQL_NOW and sqlUtc are UTC whatever the session TimeZone', { skip: skipDb }, async () => {
  await prisma.$transaction(async (tx) => {
    await tx.$executeRawUnsafe(`SET LOCAL TIME ZONE 'America/Los_Angeles'`);
    const [{ n, bad }] = await tx.$queryRaw(Prisma.sql`SELECT ${SQL_NOW} AS n, now()::timestamp AS bad`);
    assert.ok(Math.abs(n.getTime() - Date.now()) < 60_000, `SQL_NOW is UTC (${n.toISOString()})`);
    assert.ok(Math.abs(bad.getTime() - Date.now()) > 3_600_000, 'a bare now() would be shifted by the session time zone');
    const d = new Date('2026-01-02T03:04:05.006Z');
    const [{ t, lt }] = await tx.$queryRaw(Prisma.sql`SELECT ${sqlUtc(d)} AS t, (${sqlUtc(d)} < ${SQL_NOW}) AS lt`);
    assert.equal(t.getTime(), d.getTime());
    assert.equal(lt, true);
    throw rolledBack;
  }).catch((err) => { if (err !== rolledBack) throw err; });
});

test('DB: every RAW_SQL statement executes against the migrated schema', { skip: skipDb }, async () => {
  await prisma.$transaction(async (tx) => {
    for (const { name, build } of [...scheduler.RAW_SQL, ...settingsSvc.RAW_SQL]) {
      const sql = build();
      try {
        if (/^\s*DELETE/i.test(sql.sql)) await tx.$executeRaw(sql); else await tx.$queryRaw(sql);
      } catch (err) {
        assert.fail(`${name}: ${err.message}`);
      }
    }
    throw rolledBack;
  }).catch((err) => { if (err !== rolledBack) throw err; });
});

test('DB: settingsService get / update / updateSettingsWith / ensure', { skip: skipDb }, async () => {
  const gid = ID(101);
  await cleanup(gid);
  try {
    const s1 = await getSettings(gid);
    assert.equal(s1.guildId, gid);
    assert.equal(s1.modEnabled, true, 'moderation defaults on');
    assert.equal(s1.levelUpUseEmbed, false, 'new guilds keep the MEE6 plain-text default');
    assert.ok(s1.createdAt instanceof Date);
    assert.ok(Object.isFrozen(s1) && Object.isFrozen(s1.modRoleIds));
    assert.equal(await getSettings(gid), s1, 'memo returns the same frozen object');
    assert.equal(peekSettings(gid), s1);
    const fresh = await getSettings(gid, { fresh: true });
    assert.notEqual(fresh, s1);
    assert.equal(fresh.guildId, gid);
    await ensureGuildSettings(gid);
    await ensureGuildSettings(gid);

    const u = await updateSettings(gid, { automodEnabled: true, modLogChannelId: '123' });
    assert.equal(u.automodEnabled, true);
    assert.equal(peekSettings(gid), u, 'write-through memoises');
    assert.ok(Object.isFrozen(u));
    const row = await prisma.guildSettings.findUnique({ where: { guildId: gid } });
    assert.equal(row.modLogChannelId, '123');
    assert.equal((await getSettings(gid, { fresh: true })).modLogChannelId, '123');

    const pushed = await updateSettingsWith(gid, (cur) => ({ modRoleIds: [...cur.modRoleIds, 'r1'] }));
    assert.deepEqual([...pushed.modRoleIds], ['r1']);
    assert.ok(Object.isFrozen(pushed.modRoleIds));
    await updateSettingsWith(gid, (cur) => ({ automodConfig: { ...(cur.automodConfig ?? {}), rules: { caps: { roleIds: ['x'] } } } }));
    const withJson = await getSettings(gid, { fresh: true });
    assert.deepEqual(withJson.automodConfig, { rules: { caps: { roleIds: ['x'] } } });
    assert.ok(Object.isFrozen(withJson.automodConfig.rules.caps.roleIds));

    // No lost updates: ten concurrent read-modify-writes all land.
    await Promise.all(Array.from({ length: 10 }, (_, i) => updateSettingsWith(gid, (cur) => ({ modRoleIds: [...cur.modRoleIds, `c${i}`] }))));
    const all = (await prisma.guildSettings.findUnique({ where: { guildId: gid } })).modRoleIds;
    assert.equal(all.length, 11);
    assert.equal(new Set(all).size, 11);

    // A throwing mutator rolls back and its error arrives unchanged; an empty result changes nothing.
    const boom = new Error('validation says no');
    await assert.rejects(updateSettingsWith(gid, () => { throw boom; }), (err) => err === boom);
    const same = await updateSettingsWith(gid, () => null);
    assert.equal(same.modRoleIds.length, 11);
    assert.equal((await updateSettingsWith(gid, async () => ({}))).guildId, gid);

    // Missing rows are created by update and updateSettingsWith alike.
    const gid2 = ID(102);
    await cleanup(gid2);
    try {
      assert.equal((await updateSettings(gid2, { xpEnabled: true })).xpEnabled, true);
      await cleanup(gid2);
      assert.deepEqual([...(await updateSettingsWith(gid2, () => ({ autoroleIds: ['a'] }))).autoroleIds], ['a']);
    } finally {
      await cleanup(gid2);
    }

    await invalidateSettings(gid);
    assert.equal(peekSettings(gid), null);
    assert.equal((await getSettingsSafe(gid)).guildId, gid);
  } finally {
    await cleanup(gid);
  }
});

test('DB: scheduleJob dedupe modes, re-seeding of dead jobs, races and cancelJobs', { skip: skipDb }, async () => {
  const gid = ID(201);
  await cleanup(gid);
  const { scheduleJob, cancelJobs, getJob } = scheduler;
  const key = `mod.unban:${gid}:u1`;
  try {
    const a = await scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(3600), dedupeKey: key, payload: { mode: 'ok' } });
    assert.ok(a.id && a.runAt instanceof Date);
    // earliest + later runAt keeps the existing row
    const later = await scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(7200), dedupeKey: key, onConflict: 'earliest' });
    assert.equal(later.id, a.id);
    assert.equal(later.runAt.getTime(), a.runAt.getTime());
    // earliest + earlier runAt replaces and bumps the version
    const sooner = inSeconds(1800);
    const earlier = await scheduleJob({ guildId: gid, type: 'mod.unban', runAt: sooner, dedupeKey: key, onConflict: 'earliest', payload: { mode: 'resched' } });
    assert.equal(earlier.id, a.id);
    let row = await getJob(key);
    assert.equal(row.version, 1);
    assert.equal(row.runAt.getTime(), sooner.getTime());
    assert.deepEqual(row.payload, { mode: 'resched' });
    // keep leaves a live row untouched
    await scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(10), dedupeKey: key, onConflict: 'keep', payload: { mode: 'x' } });
    row = await getJob(key);
    assert.equal(row.version, 1);
    assert.deepEqual(row.payload, { mode: 'resched' });
    // a FAILED row is re-seeded even by 'keep'
    await prisma.scheduledJob.update({ where: { id: a.id }, data: { status: 'FAILED', attempts: 5, lastError: 'dead' } });
    await scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(60), dedupeKey: key, onConflict: 'keep', payload: { mode: 'ok' } });
    row = await getJob(key);
    assert.equal(row.status, 'PENDING');
    assert.equal(row.attempts, 0);
    assert.equal(row.lastError, null);
    assert.equal(row.version, 2);
    // without a dedupeKey every call inserts
    const n1 = await scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(60) });
    const n2 = await scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(60) });
    assert.notEqual(n1.id, n2.id);
    // two inserters racing on a new key leave exactly one row
    const raceKey = `mod.unban:${gid}:race`;
    await Promise.all([1, 2, 3].map((i) => scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(60 + i), dedupeKey: raceKey })));
    assert.equal(await prisma.scheduledJob.count({ where: { dedupeKey: raceKey } }), 1);

    // cancelJobs: by dedupeKey, ids, types and guild
    assert.equal(await cancelJobs({ dedupeKey: raceKey }), 1);
    assert.equal(await cancelJobs({ ids: [n1.id] }), 1);
    assert.equal(await cancelJobs({ guildId: gid, types: [] }), 0);
    assert.equal(await cancelJobs({ guildId: gid, type: 'poll.end' }), 0);
    assert.equal(await cancelJobs({ guildId: gid, types: ['poll.end', 'mod.unban'] }), 2);
    await scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(60) });
    assert.equal(await cancelJobs({ guildId: gid }), 1, 'guildId alone cancels every job of the guild');
    assert.equal(await getJob(key), null);
  } finally {
    await cleanup(gid);
  }
});

const interactiveSeen = [];
scheduler.registerJobHandler('dashboard.task', async (job) => { interactiveSeen.push(job.id); });

test('DB: interactive jobs have their own claim loop and are failed rather than run late', { skip: skipDb }, async () => {
  const gid = ID(311);
  await cleanup(gid);
  const { scheduleJob, tick, getJob } = scheduler;
  const task = (key, runAt) => scheduleJob({ guildId: gid, type: 'dashboard.task', runAt, dedupeKey: `dashboard.task:${gid}:${key}`, maxAttempts: 1 });
  try {
    interactiveSeen.length = 0;
    const due = await task('due', inSeconds(-2));
    assert.equal(await tick(fakeClient([gid])), 0, 'the ordinary loop leaves interactive jobs alone');
    assert.equal(await tick(fakeClient([gid]), { interactive: true }), 1);
    assert.deepEqual(interactiveSeen, [due.id]);
    assert.equal(await getJob(`dashboard.task:${gid}:due`), null);

    await scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(-5), dedupeKey: `${gid}:ordinary`, payload: { mode: 'ok' } });
    assert.equal(await tick(fakeClient([gid]), { interactive: true }), 0, 'the interactive loop leaves ordinary jobs alone');
    assert.equal(await tick(fakeClient([gid])), 1);

    // Too late: whoever asked was told it did not happen, so it never runs.
    interactiveSeen.length = 0;
    await task('late', inSeconds(-(scheduler.INTERACTIVE_MAX_LATE_MS / 1000 + 30)));
    assert.equal(await tick(fakeClient([gid]), { interactive: true }), 1);
    let row = await getJob(`dashboard.task:${gid}:late`);
    assert.equal(row.status, 'FAILED');
    assert.match(row.lastError, /waited too long/);
    assert.deepEqual(interactiveSeen, []);

    // The guild is not in this shard's cache: failed instead of released for later.
    await task('gone', inSeconds(-1));
    assert.equal(await tick(fakeClient([gid], { cacheMiss: true }), { interactive: true }), 1);
    row = await getJob(`dashboard.task:${gid}:gone`);
    assert.equal(row.status, 'FAILED');
    assert.match(row.lastError, /unavailable/);
    assert.deepEqual(interactiveSeen, []);
  } finally {
    await cleanup(gid);
  }
});

test('DB: claim, execute, reschedule, retry, fail and release', { skip: skipDb }, async () => {
  const gid = ID(301);
  const gid2 = ID(302);
  await cleanup(gid, gid2);
  const { scheduleJob, tick, getJob } = scheduler;
  const job = (mode, extra = {}) => scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(-5), payload: { mode }, ...extra });
  try {
    // success: handler runs once with a 1-based attempt, then the row is deleted
    seen.length = 0;
    const ok = await job('ok', { dedupeKey: `${gid}:ok` });
    assert.equal(await tick(fakeClient([gid])), 1);
    assert.equal(seen.length, 1);
    assert.deepEqual({ id: seen[0].id, attempt: seen[0].attempt, available: seen[0].available }, { id: ok.id, attempt: 1, available: true });
    assert.ok(seen[0].lateByMs >= 4000, `lateByMs ${seen[0].lateByMs}`);
    assert.equal(await getJob(`${gid}:ok`), null);
    assert.equal(await tick(fakeClient([gid])), 0, 'nothing left to claim');

    // not yet due, and a guild this client does not have available, are not claimed
    await scheduleJob({ guildId: gid, type: 'mod.unban', runAt: inSeconds(600), dedupeKey: `${gid}:future` });
    await scheduleJob({ guildId: gid2, type: 'mod.unban', runAt: inSeconds(-5), dedupeKey: `${gid}:away`, payload: { mode: 'ok' } });
    seen.length = 0;
    assert.equal(await tick(fakeClient([gid])), 0);
    assert.equal(seen.length, 0);
    assert.equal((await getJob(`${gid}:away`)).status, 'PENDING');
    assert.equal((await getJob(`${gid}:away`)).attempts, 0);

    // reschedule: { nextRunAt, payload } resets attempts and bumps the version
    await job('resched', { dedupeKey: `${gid}:resched` });
    assert.equal(await tick(fakeClient([gid])), 1);
    let row = await getJob(`${gid}:resched`);
    assert.equal(row.status, 'PENDING');
    assert.equal(row.attempts, 0);
    assert.equal(row.version, 1);
    assert.deepEqual(row.payload, { mode: 'ok', second: true });
    assert.ok(row.runAt.getTime() > Date.now() + 3_000_000);
    assert.equal(row.lockedBy, null);

    // failure with attempts left: attempts stay counted, back-off ~30 s, error recorded and truncated
    await job('throw', { dedupeKey: `${gid}:retry`, maxAttempts: 3 });
    assert.equal(await tick(fakeClient([gid])), 1);
    row = await getJob(`${gid}:retry`);
    assert.equal(row.status, 'PENDING');
    assert.equal(row.attempts, 1);
    assert.ok(row.runAt.getTime() > Date.now() + 20_000 && row.runAt.getTime() < Date.now() + 40_000, 'first back-off is 30 s');
    assert.ok(row.lastError.startsWith('boom') && row.lastError.length <= 2000);
    assert.equal(row.lockedBy, null);

    // final attempt fails permanently
    await job('throw', { dedupeKey: `${gid}:last`, maxAttempts: 1 });
    await tick(fakeClient([gid]));
    row = await getJob(`${gid}:last`);
    assert.equal(row.status, 'FAILED');
    assert.equal(row.attempts, 1);
    await job('permanent', { dedupeKey: `${gid}:perm` });
    await tick(fakeClient([gid]));
    row = await getJob(`${gid}:perm`);
    assert.equal(row.status, 'FAILED');
    assert.equal(row.lastError, 'never retry');
    assert.equal(row.attempts, 1, 'PermanentJobError is not retried');

    // poison guard: a crashed worker already used every attempt; the next claim fails the row unexecuted
    const poison = await job('ok', { dedupeKey: `${gid}:poison`, maxAttempts: 2 });
    await prisma.scheduledJob.update({ where: { id: poison.id }, data: { status: 'RUNNING', attempts: 2, lockedBy: 'dead', lockedUntil: inSeconds(-60) } });
    seen.length = 0;
    await tick(fakeClient([gid]));
    row = await getJob(`${gid}:poison`);
    assert.equal(row.status, 'FAILED');
    assert.match(row.lastError, /claimed 3 times without completing/);
    assert.equal(seen.length, 0, 'the handler never ran');

    // an expired lock is re-claimed; a live lock is not
    const stale = await job('ok', { dedupeKey: `${gid}:stale` });
    await prisma.scheduledJob.update({ where: { id: stale.id }, data: { status: 'RUNNING', attempts: 1, lockedBy: 'dead', lockedUntil: inSeconds(-60) } });
    const locked = await job('ok', { dedupeKey: `${gid}:locked` });
    await prisma.scheduledJob.update({ where: { id: locked.id }, data: { status: 'RUNNING', attempts: 1, lockedBy: 'alive', lockedUntil: inSeconds(120) } });
    seen.length = 0;
    assert.equal(await tick(fakeClient([gid])), 1);
    assert.equal(seen[0].id, stale.id);
    assert.equal(seen[0].attempt, 2);
    assert.equal(await getJob(`${gid}:stale`), null);
    assert.equal((await getJob(`${gid}:locked`)).status, 'RUNNING');

    // no handler registered: released without spending an attempt, pushed back ~10 min
    const orphan = await scheduleJob({ guildId: gid, type: 'captcha.timeout', runAt: inSeconds(-5), dedupeKey: `${gid}:orphan` });
    await tick(fakeClient([gid]));
    row = await getJob(`${gid}:orphan`);
    assert.equal(row.id, orphan.id);
    assert.equal(row.status, 'PENDING');
    assert.equal(row.attempts, 0);
    assert.ok(row.runAt.getTime() > Date.now() + 500_000);

    // guild missing from the cache at execution time: released, attempt not spent, runAt unchanged
    const miss = await job('ok', { dedupeKey: `${gid}:miss` });
    const before = (await getJob(`${gid}:miss`)).runAt.getTime();
    seen.length = 0;
    await tick(fakeClient([gid], { cacheMiss: true }));
    row = await getJob(`${gid}:miss`);
    assert.equal(row.id, miss.id);
    assert.equal(row.status, 'PENDING');
    assert.equal(row.attempts, 0);
    assert.equal(row.runAt.getTime(), before);
    assert.equal(seen.length, 0);

    // a job replaced while its handler runs is not deleted by the stale completion (version guard)
    scheduler.registerJobHandler('mod.expire', async () => {
      await scheduleJob({ guildId: gid, type: 'mod.expire', runAt: inSeconds(3600), dedupeKey: `${gid}:racing`, payload: { replaced: true } });
    });
    await scheduleJob({ guildId: gid, type: 'mod.expire', runAt: inSeconds(-5), dedupeKey: `${gid}:racing` });
    await tick(fakeClient([gid]));
    row = await getJob(`${gid}:racing`);
    assert.ok(row, 'the replacement survives the old run finishing');
    assert.equal(row.version, 1);
    assert.deepEqual(row.payload, { replaced: true });

    // stop() waits for an in-flight tick
    await scheduler.stop({ timeoutMs: 100 });
  } finally {
    await cleanup(gid, gid2);
  }
});

test('DB: GC removes only old finished jobs', { skip: skipDb }, async () => {
  const gid = ID(401);
  await cleanup(gid);
  try {
    await ensureGuildSettings(gid);
    const old = new Date(Date.now() - 8 * 86_400_000);
    const mk = (dedupeKey, status, updatedAt) => prisma.scheduledJob.create({
      data: { guildId: gid, type: 'mod.unban', dedupeKey, runAt: old, status, updatedAt },
    });
    await mk(`${gid}:old-failed`, 'FAILED', old);
    await mk(`${gid}:old-pending`, 'PENDING', old);
    await mk(`${gid}:new-failed`, 'FAILED', new Date());
    assert.ok((await scheduler.gc()) >= 1, 'GC deleted at least the old finished job');
    assert.equal(await scheduler.getJob(`${gid}:old-failed`), null);
    assert.ok(await scheduler.getJob(`${gid}:old-pending`));
    assert.ok(await scheduler.getJob(`${gid}:new-failed`));
  } finally {
    await cleanup(gid);
  }
});
