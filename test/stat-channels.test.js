// pkg-stat-channels tests (pkg-stat-channels.md §9). Offline by default: the pure helpers, snapshot and value maths, the rename and
// next-run decisions, the hook filters, the plugin declaration and the /stats payload.
// With TEST_DATABASE_URL (a migrated database) the DB cases also run: create / edit / delete, the lastRenamedAt reservation (even
// under concurrency), the conditional peak update, coalesced renames and the trailing next run, the job body, the hooks, ensureRefreshJob
// (reviving FAILED, 'earliest' never later), the /stats command end to end, and the rule-20 cases (only a REST 10003 removes a row):
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/stat-channels.test.js
import { test, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';
delete process.env.BOT_OWNER_ID;

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ChannelType, OverwriteType, PermissionFlagsBits: P, PermissionsBitField } = await import('discord.js');
const stats = await import('../src/services/statsService.js');
const statsCmd = await import('../src/commands/admin/stats.js');
const { plugin } = await import('../src/plugins/stat-channels.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { JOB_TYPES } = await import('../src/config/jobTypes.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { TimeoutError } = await import('../src/utils/async.js');
const { ensureGuildSettings, getSettingsSafe, updateSettings } = await import('../src/services/settingsService.js');
const { wasDeletedByBot } = await import('../src/utils/deletionTracker.js');

const {
  STAT_KINDS, KIND_INFO, clampInt, refreshJobKey, normalizeTemplate, renderStatName, validateStatTemplate, isMemberCacheComplete,
  countChannels, buildSnapshot, computeValue, decideRename, computeNextRunAt, parseChannelRef, buildOverwrites, claimPull, runExclusive,
  describeProblem, memberEventFilter, channelDeleteFilter, MEMBER_LIST_MAX_MEMBERS, MIN_RENAME_GAP_MS, REFRESH_INTERVAL_MS,
  MIN_NEXT_RUN_MS, StatsError, __setRowsMemo, __resetState,
} = stats;

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const GUILD_IDS = [];
let guildSeq = 0;
/** Guild ids unique to this package (port 55022); every one is deleted in after(). */
const G = () => {
  const id = `95502200000000${String(++guildSeq).padStart(4, '0')}`;
  GUILD_IDS.push(id);
  return id;
};
let chanSeq = 0;
const cid = () => `9550220100${String(++chanSeq).padStart(8, '0')}`; // 18 digits, accepted by parseChannelRef

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB && GUILD_IDS.length) {
    await prisma.scheduledJob.deleteMany({ where: { guildId: { in: GUILD_IDS } } }).catch(() => {});
    // The cascade removes the StatChannel rows.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: GUILD_IDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

beforeEach(() => __resetState());

// ── Fakes ─────────────────────────────────────────────────────────────────────

const BOT = '900000000000000001';
const ALL = () => new PermissionsBitField(P.Administrator);
const NONE = () => new PermissionsBitField(0n);

const mkMember = (bot, roleIds = []) => ({ user: { bot }, roles: { cache: new Set(roleIds) } });

/** The small fake of spec §9: caches only. */
function fakeGuild({
  memberCount = 0, members = [], presences = [], channels = [], roles = [], boosts = 0,
} = {}) {
  return {
    id: 'g1',
    name: 'Guild',
    memberCount,
    members: { cache: new Map(members.map((m, i) => [`m${i}`, m])), me: null },
    presences: { cache: new Map(presences.map((p, i) => [`p${i}`, p])) },
    channels: { cache: new Map(channels.map((c, i) => [`c${i}`, c])) },
    roles: { cache: new Map(roles.map((r) => [r, { id: r, name: r }])) },
    premiumSubscriptionCount: boosts,
  };
}

/** A richer guild for the DB cases: a REST-like channel manager, a bot member, settable behaviour. */
function makeGuild(id, {
  humans = 10, bots = 0, memberCount = null, online = 0, available = true, approx = null,
} = {}) {
  const members = new Map();
  for (let i = 0; i < humans; i++) members.set(`h${i}`, mkMember(false));
  for (let i = 0; i < bots; i++) members.set(`b${i}`, mkMember(true));
  const guild = {
    id,
    name: 'Test Guild',
    available,
    memberCount: memberCount ?? humans + bots,
    premiumSubscriptionCount: 0,
    members: { cache: members, me: { id: BOT, permissions: ALL() }, fetch: async () => members },
    presences: { cache: new Map(Array.from({ length: online }, (_, i) => [`p${i}`, { status: 'online' }])) },
    roles: { cache: new Map([[id, { id, name: '@everyone' }]]) },
    channels: { cache: new Map() },
    fetch: async () => ({ approximatePresenceCount: approx }),
    calls: { created: [], deleted: [], renamed: [], deletedById: [] },
    createError: null,
    fetchChannel: null, // (id) => channel | throws
  };
  guild.channels.create = async (opts) => {
    if (guild.createError) throw guild.createError;
    guild.calls.created.push(opts);
    return addChannel(guild, { name: opts.name, type: opts.type });
  };
  guild.channels.fetch = async (channelId) => {
    if (guild.fetchChannel) return guild.fetchChannel(channelId);
    const c = guild.channels.cache.get(channelId);
    if (c) return c;
    throw Object.assign(new Error('Unknown Channel'), { code: 10003 });
  };
  guild.channels.delete = async (channelId, reason) => {
    guild.calls.deletedById.push([channelId, reason]);
    guild.channels.cache.delete(channelId);
  };
  return guild;
}

function addChannel(guild, {
  id = cid(), name = 'chan', type = ChannelType.GuildVoice, perms = ALL(), setName = null,
} = {}) {
  const ch = {
    id,
    name,
    type,
    guild,
    isThread: () => false,
    permissionsFor: () => perms,
    setName: async (n, reason) => {
      if (setName) return setName(ch, n, reason);
      guild.calls.renamed.push([id, n, reason]);
      ch.name = n;
      return ch;
    },
    delete: async (reason) => {
      guild.calls.deleted.push([id, reason]);
      guild.channels.cache.delete(id);
    },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

const addRole = (guild, id, name = `Role ${id}`) => guild.roles.cache.set(id, { id, name });

/** Inserts a StatChannel row (and a fake channel). */
async function seed(guild, kind, {
  name = 'old name', template = KIND_INFO[kind].template, roleId = null, lastRenamedAt = null, lastValue = null, peakValue = null,
  perms = ALL(), setName = null, noChannel = false, channelId = null,
} = {}) {
  await ensureGuildSettings(guild.id);
  const channel = noChannel ? null : addChannel(guild, { name, perms, setName });
  const row = await prisma.statChannel.create({
    data: {
      guildId: guild.id, channelId: channel?.id ?? channelId ?? cid(), kind, roleId, template, lastValue, lastRenamedAt, peakValue,
    },
  });
  return { row, channel };
}

const fresh = (guildId) => stats.loadRows(guildId);
const jobOf = (guildId) => prisma.scheduledJob.findUnique({ where: { dedupeKey: refreshJobKey(guildId) } });
const rowOf = (id) => prisma.statChannel.findUnique({ where: { id } });
const within = (value, lo, hi) => value >= lo && value <= hi;

// ── 1. validateStatTemplate ───────────────────────────────────────────────────

test('1. validateStatTemplate', () => {
  const guild = fakeGuild();
  const v = (input, opts = {}) => validateStatTemplate(input, { kind: 'MEMBERS', guild, ...opts });
  assert.deepEqual(v('Members: {count}'), { ok: true, template: 'Members: {count}' });
  assert.deepEqual(v('  Members:   {count}  '), { ok: true, template: 'Members: {count}' });
  assert.equal(v('{COUNT} online').ok, true);
  const missing = v('Members');
  assert.equal(missing.ok, false);
  assert.match(missing.error, /\{count\}/);
  assert.match(v('{count} {random}').error, /Random variables/);
  assert.match(v('{role}: {count}').error, /Role members/);
  assert.equal(v('{role}: {count}', { kind: 'ROLE_MEMBERS', roleName: 'Mods' }).ok, true);
  assert.equal(v('{role}: {count}', { kind: 'ROLE_MEMBERS', roleName: '{vip}' }).ok, true, 'braces in a role name are not a variable');
  assert.match(v('{count} {user}').error, /Unknown variable `\{user\}`/);
  assert.match(v('{count} {nope}').error, /Unknown variable `\{nope\}`/);
  assert.equal(v('{server}: {count}').ok, true);
  assert.match(v(`${'x'.repeat(94)}{count}`).error, /1–100 characters/);
  assert.match(v(`${'x'.repeat(88)}{count}`).error, /large count.*101/);
  assert.equal(v(`${'x'.repeat(87)}{count}`).ok, true);
  for (const k of STAT_KINDS) {
    assert.equal(validateStatTemplate(KIND_INFO[k].template, { kind: k, roleName: 'Mods', guild }).ok, true, k);
  }
});

// ── 2. renderStatName ─────────────────────────────────────────────────────────

test('2. renderStatName', () => {
  assert.equal(renderStatName('Members: {count}', { count: 1234 }), 'Members: 1,234');
  assert.equal(renderStatName('Members: {count}', { count: null }), 'Members: …');
  assert.equal(renderStatName('{role}: {count}', { count: 5, roleName: 'Mods' }), 'Mods: 5');
  assert.ok(renderStatName('{role}: {count}', { count: 5, roleName: 'r'.repeat(150) }).length <= 100);
  assert.equal(renderStatName('{count}', { count: 0 }), '0');
  assert.equal(renderStatName('{server}: {count}', { count: 3, guild: fakeGuild() }), 'Guild: 3');
});

// ── 3. countChannels ──────────────────────────────────────────────────────────

test('3. countChannels', () => {
  const ch = (type, thread = false) => ({ type, isThread: () => thread });
  const channels = new Map([
    ['1', ch(ChannelType.GuildText)], ['2', ch(ChannelType.GuildAnnouncement)], ['3', ch(ChannelType.GuildForum)],
    ['4', ch(ChannelType.GuildVoice)], ['5', ch(ChannelType.GuildStageVoice)], ['6', ch(ChannelType.GuildCategory)],
    ['7', ch(ChannelType.PublicThread, true)],
  ]);
  assert.deepEqual(countChannels(channels), { text: 3, voice: 2, total: 5 });
});

// ── 4. buildSnapshot and computeValue ─────────────────────────────────────────

test('4. buildSnapshot and computeValue', () => {
  const value = (kind, snap, row) => computeValue(kind, snap, row);
  const complete = fakeGuild({
    memberCount: 5,
    members: [mkMember(false, ['r1']), mkMember(false), mkMember(false, ['r1']), mkMember(true), mkMember(true)],
    roles: ['g1', 'r1'],
  });
  const snap = buildSnapshot(complete, { roleIds: ['r1'] });
  assert.equal(value('MEMBERS', snap).value, 5);
  assert.equal(value('HUMANS', snap).value, 3);
  assert.equal(value('BOTS', snap).value, 2);
  assert.equal(value('ROLE_MEMBERS', snap, { roleId: 'r1' }).value, 2);

  const incomplete = buildSnapshot(fakeGuild({ memberCount: 6, members: [...complete.members.cache.values()] }));
  assert.deepEqual(value('HUMANS', incomplete), { value: null, problem: 'members-incomplete' });
  assert.equal(value('MEMBERS', incomplete).value, 6);

  const huge = buildSnapshot(fakeGuild({ memberCount: 150_000 }));
  assert.deepEqual(value('HUMANS', huge), { value: null, problem: 'server-too-large' });

  const gone = buildSnapshot(complete, { roleIds: ['missing'] });
  assert.deepEqual(value('ROLE_MEMBERS', gone, { roleId: 'missing' }), { value: null, problem: 'role-deleted' });

  const presences = buildSnapshot(fakeGuild({
    memberCount: 4, presences: [{ status: 'online' }, { status: 'idle' }, { status: 'dnd' }, { status: 'offline' }],
  }));
  assert.equal(value('ONLINE', presences).value, 3);

  assert.equal(value('ONLINE', buildSnapshot(fakeGuild({ memberCount: 80_000 }), { approxOnline: 1234 })).value, 1234);
  assert.deepEqual(value('ONLINE', buildSnapshot(fakeGuild({ memberCount: 80_000 }), { approxOnline: null })), { value: null, problem: 'presence-unavailable' });

  const online7 = buildSnapshot(fakeGuild({ memberCount: 7, presences: Array.from({ length: 7 }, () => ({ status: 'online' })) }));
  const online12 = buildSnapshot(fakeGuild({ memberCount: 12, presences: Array.from({ length: 12 }, () => ({ status: 'online' })) }));
  const unavailable = buildSnapshot(fakeGuild({ memberCount: 80_000 }), { approxOnline: null });
  assert.equal(value('PEAK_ONLINE', online7, { peakValue: 10 }).value, 10);
  assert.equal(value('PEAK_ONLINE', online12, { peakValue: 10 }).value, 12);
  assert.deepEqual(value('PEAK_ONLINE', unavailable, { peakValue: null }), { value: null, problem: 'presence-unavailable' });
  assert.equal(value('PEAK_ONLINE', unavailable, { peakValue: 9 }).value, 9);

  assert.equal(value('ROLES', buildSnapshot(fakeGuild({ roles: ['e', 'a', 'b', 'c'] }))).value, 3);
  assert.equal(value('BOOSTS', buildSnapshot(fakeGuild({ boosts: null }))).value, 0);
  assert.equal(value('NOPE', snap).problem, 'error');
});

// ── 5. isMemberCacheComplete ──────────────────────────────────────────────────

test('5. isMemberCacheComplete', () => {
  const members = (n) => Array.from({ length: n }, () => mkMember(false));
  assert.equal(isMemberCacheComplete(fakeGuild({ memberCount: 5, members: members(5) })), true);
  assert.equal(isMemberCacheComplete(fakeGuild({ memberCount: 5, members: members(4) })), false);
  assert.equal(isMemberCacheComplete(fakeGuild({ memberCount: 5, members: members(6) })), true);
});

// ── 6. decideRename ───────────────────────────────────────────────────────────

test('6. decideRename', () => {
  const now = 1_000_000_000;
  assert.deepEqual(decideRename({ desiredName: 'a', currentName: 'a', now }), { action: 'same' });
  assert.deepEqual(decideRename({ desiredName: 'a', currentName: 'b', lastRenamedAt: null, now }), { action: 'rename' });
  const four = new Date(now - 4 * 60_000);
  assert.deepEqual(decideRename({ desiredName: 'a', currentName: 'b', lastRenamedAt: four, now }), { action: 'wait', retryAt: four.getTime() + 300_000 });
  assert.deepEqual(decideRename({ desiredName: 'a', currentName: 'b', lastRenamedAt: new Date(now - 5 * 60_000), now }), { action: 'rename' });
  assert.deepEqual(
    decideRename({ desiredName: 'A', currentName: 'a', alias: { desired: 'A', actual: 'a' }, lastRenamedAt: four, now }),
    { action: 'same' },
  );
});

// ── 7. computeNextRunAt ───────────────────────────────────────────────────────

test('7. computeNextRunAt', () => {
  assert.equal(computeNextRunAt({ now: 0 }).getTime(), 600_000);
  assert.equal(computeNextRunAt({ now: 0, retryAts: [120_000] }).getTime(), 125_000);
  assert.equal(computeNextRunAt({ now: 0, retryAts: [10_000] }).getTime(), 60_000, 'the 60 s floor');
  assert.equal(computeNextRunAt({ now: 0, soon: true }).getTime(), 60_000);
  assert.equal(computeNextRunAt({ now: 0, retryAts: [900_000] }).getTime(), 600_000, 'a retry later than the interval does not postpone it');
});

// ── 8. Small helpers ──────────────────────────────────────────────────────────

test('8. parseChannelRef, buildOverwrites, refreshJobKey, claimPull, runExclusive, describeProblem, clampInt', async () => {
  assert.equal(parseChannelRef('<#123456789012345678>'), '123456789012345678');
  assert.equal(parseChannelRef('123456789012345678'), '123456789012345678');
  assert.equal(parseChannelRef('abc'), null);
  assert.equal(parseChannelRef('<#12>'), null);

  const o = buildOverwrites({ id: 'g' }, 'b');
  assert.equal(o.length, 2);
  assert.deepEqual(o[0], { id: 'g', type: OverwriteType.Role, deny: [P.Connect] });
  assert.deepEqual(o[1], { id: 'b', type: OverwriteType.Member, allow: [P.ViewChannel, P.ManageChannels] });

  assert.equal(refreshJobKey('g'), 'stats.refresh:g');
  assert.equal(claimPull('g', 0), true);
  assert.equal(claimPull('g', 30_000), false);
  assert.equal(claimPull('g', 61_000), true);

  let release;
  const gate = new Promise((r) => { release = r; });
  const first = runExclusive('guild', () => gate);
  assert.deepEqual(await runExclusive('guild', () => 1), { busy: true });
  release('done');
  assert.deepEqual(await first, { busy: false, value: 'done' });
  assert.deepEqual(await runExclusive('guild', () => 2), { busy: false, value: 2 }, 'released after the run');

  assert.equal(describeProblem('nope'), describeProblem('error'));
  assert.equal(describeProblem('constructor'), describeProblem('error'), 'no prototype lookups');
  assert.equal(clampInt(-5), 0);
  assert.equal(clampInt(3e12), 2_147_483_647);
  assert.equal(clampInt('7.9'), 7);
  assert.equal(clampInt(NaN), 0);
  assert.equal(normalizeTemplate('  a \n b '), 'a b');
});

test('8b. __resetState recreates the throttle and the mutex', async () => {
  claimPull('g', 0);
  __resetState();
  assert.equal(claimPull('g', 30_000), true, 'pending pulls are cleared');
  __setRowsMemo('g', [{ id: '1', channelId: '1', kind: 'MEMBERS' }]);
  assert.equal(memberEventFilter({ guildId: 'g' }), true);
  __resetState();
  assert.equal(memberEventFilter({ guildId: 'g' }), true, 'no memo fails open');
});

// ── 9. Kind metadata and filters ──────────────────────────────────────────────

test('9. STAT_KINDS matches the Prisma enum, KIND_INFO and the filters', () => {
  const schema = readFileSync(new URL('../prisma/schema.prisma', import.meta.url), 'utf8');
  const body = /enum StatKind \{([^}]*)\}/.exec(schema)[1];
  const enumValues = body.split('\n').map((l) => l.trim()).filter((l) => l && !l.startsWith('//'));
  assert.deepEqual([...STAT_KINDS], enumValues);
  assert.deepEqual(Object.keys(KIND_INFO), enumValues);
  for (const k of STAT_KINDS) assert.ok(KIND_INFO[k].label.length <= 100);

  // memberEventFilter
  assert.equal(memberEventFilter({ guildId: 'unknown' }), true);
  __setRowsMemo('gx', []);
  assert.equal(memberEventFilter({ guildId: 'gx' }), false);
  __setRowsMemo('gx', [{ id: '1', channelId: '1', kind: 'TEXT_CHANNELS' }]);
  assert.equal(memberEventFilter({ guildId: 'gx' }), false);
  __setRowsMemo('gx', [{ id: '1', channelId: '1', kind: 'MEMBERS' }]);
  assert.equal(memberEventFilter({ guildId: 'gx' }), true);
  __setRowsMemo('gx', [], Date.now() - 16 * 60_000);
  assert.equal(memberEventFilter({ guildId: 'gx' }), true, 'an expired memo fails open');

  // channelDeleteFilter
  const ev = (type, id = 'c1', guildId = 'gy') => ({ guildId, channel: { id, type } });
  assert.equal(channelDeleteFilter(ev(ChannelType.GuildText)), false);
  __setRowsMemo('gy', [{ id: '1', channelId: 'c9', kind: 'MEMBERS' }]);
  assert.equal(channelDeleteFilter(ev(ChannelType.GuildVoice)), false, 'a voice channel that is not a counter');
  assert.equal(channelDeleteFilter(ev(ChannelType.GuildVoice, 'c9')), true);
  assert.equal(channelDeleteFilter(ev(ChannelType.GuildVoice, 'c1', 'unknown')), true, 'an unknown memo fails open');
  assert.equal(channelDeleteFilter(ev(ChannelType.GuildCategory, 'cat', 'no-settings')), true, 'no settings memo');
});

// ── 10. Plugin ────────────────────────────────────────────────────────────────

test('10. the plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('stat-channels')), []);
  assert.ok(plugins.some((p) => p.id === 'stat-channels'));
  assert.equal(plugin.id, 'stat-channels');
  assert.equal(plugin.module, 'stats');
  const events = plugin.hooks.map((h) => h.event).sort();
  assert.deepEqual(events, ['channelDelete', 'guildMemberAdd', 'guildMemberRemove', 'guildReady']);
  for (const h of plugin.hooks) {
    assert.equal(h.id, 'stat-channels');
    assert.ok(HOOK_ORDER[h.event].includes('stat-channels'), h.event);
  }
  assert.equal(plugin.hooks.find((h) => h.event === 'guildReady').always, true);
  assert.equal(plugin.hooks.find((h) => h.event === 'channelDelete').always, true);
  assert.equal(plugin.hooks.find((h) => h.event === 'guildMemberAdd').always, undefined, 'member hooks are module-gated');
  assert.equal(plugin.jobs.length, 1);
  assert.equal(plugin.jobs[0].type, 'stats.refresh');
  assert.equal(typeof plugin.jobs[0].handler, 'function');
  assert.equal(JOB_TYPES['stats.refresh'].owner, 'stat-channels');
  assert.deepEqual(plugin.components, []);
  assert.deepEqual(plugin.provides, {});
});

// ── 11. Command payload ───────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('11. the /stats payload', () => {
  const json = buildCommandPayload(statsCmd);
  assert.equal(json.name, 'stats');
  assert.deepEqual(json.options.map((o) => o.name), ['create', 'edit', 'delete', 'list', 'refresh', 'category']);
  const sub = (name) => json.options.find((o) => o.name === name);
  const opts = (name) => (sub(name).options ?? []).map((o) => [o.name, Boolean(o.required)]);

  assert.deepEqual(opts('create'), [['kind', true], ['template', false], ['role', false], ['category', false], ['new_category', false]]);
  assert.deepEqual(sub('create').options[0].choices.map((c) => c.value), [...STAT_KINDS]);
  assert.deepEqual(sub('create').options[0].choices.map((c) => c.name), STAT_KINDS.map((k) => KIND_INFO[k].label));
  assert.deepEqual(opts('edit'), [['channel', true], ['template', true]]);
  assert.equal(sub('edit').options[0].autocomplete, true);
  assert.deepEqual(opts('delete'), [['channel', true], ['keep_channel', false]]);
  assert.equal(sub('delete').options[0].autocomplete, true);
  assert.deepEqual(opts('category'), [['category', false], ['new_category', false]]);
  assert.deepEqual(opts('list'), []);
  assert.deepEqual(opts('refresh'), []);

  const walk = (node) => {
    assert.ok(node.description.length >= 1 && node.description.length <= 100, `${node.name}: description length`);
    for (const o of node.options ?? []) walk(o);
  };
  walk(json);
  assert.ok(charCount(json) <= 1000, `charCount ${charCount(json)}`);
  assert.equal(json.default_member_permissions, String(P.ManageGuild));
  assert.deepEqual(json.contexts, [0]);
  assert.equal(statsCmd.module, 'stats');
  assert.equal(statsCmd.gated, false);
  assert.equal(statsCmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(typeof statsCmd.autocomplete, 'function');
});

// ══ DB cases ══════════════════════════════════════════════════════════════════

// ── create / edit / delete ────────────────────────────────────────────────────

test('DB: createStatChannel creates the locked channel, the row, the job and the memo entry', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 8, bots: 2 });
  const client = { user: { id: BOT } };
  const made = await stats.createStatChannel({
    guild, client, kind: 'MEMBERS', template: 'Members: {count}', actorTag: 'tester#0',
  });
  assert.equal(made.value, 10);
  const opts = guild.calls.created[0];
  assert.equal(opts.name, 'Members: 10');
  assert.equal(opts.type, ChannelType.GuildVoice);
  assert.equal(opts.parent, null);
  assert.deepEqual(opts.permissionOverwrites, buildOverwrites(guild, BOT));
  assert.match(opts.reason, /tester#0/);
  const row = await rowOf(made.row.id);
  assert.equal(row.channelId, made.channel.id);
  assert.equal(row.lastValue, 10);
  assert.equal(row.lastRenamedAt, null);
  const job = await jobOf(gid);
  assert.equal(job.status, 'PENDING');
  assert.ok(within(job.runAt.getTime() - Date.now(), REFRESH_INTERVAL_MS - 10_000, REFRESH_INTERVAL_MS + 10_000), 'first run in ~10 min');
  assert.equal(memberEventFilter({ guildId: gid }), true, 'the memo knows the new counter');
});

test('DB: createStatChannel with an unknown value (incomplete member list) shows an ellipsis and schedules a run in ~60 s', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3, memberCount: 9 });
  guild.members.fetch = async () => guild.members.cache; // the fetch cannot complete the list
  const made = await stats.createStatChannel({
    guild, client: { user: { id: BOT } }, kind: 'HUMANS', template: 'Humans: {count}', actorTag: 't',
  });
  assert.equal(made.value, null);
  assert.equal(guild.calls.created[0].name, 'Humans: …');
  assert.equal((await rowOf(made.row.id)).lastValue, null);
  const job = await jobOf(gid);
  assert.ok(within(job.runAt.getTime() - Date.now(), 50_000, 70_000));
});

test('DB: createStatChannel fetches the full member list for member-list kinds', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 2, bots: 0, memberCount: 5 });
  guild.members.fetch = async () => {
    for (let i = 0; i < 2; i++) guild.members.cache.set(`bot${i}`, mkMember(true));
    guild.members.cache.set('late', mkMember(false));
    return guild.members.cache;
  };
  const made = await stats.createStatChannel({
    guild, client: { user: { id: BOT } }, kind: 'BOTS', template: 'Bots: {count}', actorTag: 't',
  });
  assert.equal(made.value, 2);
  assert.equal(guild.calls.created[0].name, 'Bots: 2');
});

test('DB: duplicates, the role cap and the guild cap', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 4 });
  const client = { user: { id: BOT } };
  const create = (kind, extra = {}) => stats.createStatChannel({
    guild, client, kind, template: KIND_INFO[kind].template, actorTag: 't', ...extra,
  });
  const first = await create('MEMBERS');
  await assert.rejects(create('MEMBERS'), (e) => e instanceof StatsError && e.code === 'DUPLICATE' && e.channelId === first.channel.id);
  await create('BOTS');

  for (let i = 1; i <= LIMITS.ROLE_STAT_CHANNELS; i++) {
    addRole(guild, `r${i}`);
    await create('ROLE_MEMBERS', { role: guild.roles.cache.get(`r${i}`), template: '{role}: {count}' });
  }
  addRole(guild, 'rx');
  await assert.rejects(
    create('ROLE_MEMBERS', { role: guild.roles.cache.get('rx'), template: '{role}: {count}' }),
    (e) => e.code === 'ROLE_LIMIT',
  );
  // The same role twice is a duplicate even below the cap.
  const gid2 = G();
  const guild2 = makeGuild(gid2, { humans: 2 });
  addRole(guild2, 'ra');
  await stats.createStatChannel({ guild: guild2, client, kind: 'ROLE_MEMBERS', template: '{role}: {count}', role: guild2.roles.cache.get('ra'), actorTag: 't' });
  await assert.rejects(
    stats.createStatChannel({ guild: guild2, client, kind: 'ROLE_MEMBERS', template: '{role}: {count}', role: guild2.roles.cache.get('ra'), actorTag: 't' }),
    (e) => e.code === 'DUPLICATE',
  );

  // The guild cap: fill a third guild with rows, then one more creation is refused.
  const gid3 = G();
  const guild3 = makeGuild(gid3, { humans: 2 });
  await ensureGuildSettings(gid3);
  for (let i = 0; i < LIMITS.STAT_CHANNELS; i++) {
    addChannel(guild3, { id: cid() });
  }
  const ids = [...guild3.channels.cache.keys()];
  await prisma.statChannel.createMany({ data: ids.map((channelId) => ({ guildId: gid3, channelId, kind: 'MEMBERS', template: 'x {count}' })) });
  await assert.rejects(
    stats.createStatChannel({ guild: guild3, client, kind: 'ROLES', template: 'Roles: {count}', actorTag: 't' }),
    (e) => e.code === 'LIMIT',
  );
});

test('DB: createStatChannel maps Discord errors and compensates when the row cannot be saved', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3 });
  const client = { user: { id: BOT } };
  const base = { guild, client, kind: 'MEMBERS', template: 'Members: {count}', actorTag: 't' };
  for (const [code, expected] of [[50013, 'MISSING_PERMISSIONS'], [50001, 'MISSING_PERMISSIONS'], [30013, 'CHANNEL_LIMIT'], [50035, 'DISCORD_REJECTED']]) {
    guild.createError = Object.assign(new Error('x'), { code });
    await assert.rejects(stats.createStatChannel(base), (e) => e instanceof StatsError && e.code === expected, String(code));
  }
  guild.createError = Object.assign(new Error('boom'), { code: 500 });
  await assert.rejects(stats.createStatChannel(base), (e) => !(e instanceof StatsError) && e.message === 'boom');
  guild.createError = null;
  assert.equal(await prisma.statChannel.count({ where: { guildId: gid } }), 0, 'no row after the failures');

  // A row that cannot be saved (invalid kind) deletes the new channel again and marks the deletion as the bot's.
  await assert.rejects(stats.createStatChannel({ ...base, kind: 'NOT_A_KIND' }));
  assert.equal(guild.calls.deleted.length, 1);
  assert.equal(wasDeletedByBot(guild.calls.deleted[0][0]), 'stats');
  assert.equal(guild.channels.cache.size, 0);
});

test('DB: createStatChannel creates inside the given category', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3 });
  await stats.createStatChannel({
    guild, client: { user: { id: BOT } }, kind: 'BOOSTS', template: 'Boosts: {count}', category: { id: '777000000000000001' }, actorTag: 't',
  });
  assert.equal(guild.calls.created[0].parent, '777000000000000001');
  assert.equal(guild.calls.created[0].name, 'Boosts: 0');
});

test('DB: editTemplate updates the row, pulls the job forward and reports a vanished row', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3 });
  const { row } = await seed(guild, 'MEMBERS');
  await stats.editTemplate(guild, row, 'People: {count}');
  assert.equal((await rowOf(row.id)).template, 'People: {count}');
  const job = await jobOf(gid);
  assert.ok(within(job.runAt.getTime() - Date.now(), 0, 20_000), 'pulled to ~5 s');
  await prisma.statChannel.delete({ where: { id: row.id } });
  await assert.rejects(stats.editTemplate(guild, row, 'x {count}'), (e) => e instanceof StatsError && e.code === 'NOT_FOUND');
});

test('DB: deleteStatChannel deletes the channel (or keeps it), refuses without permissions and cancels the job with the last row', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3 });
  const a = await seed(guild, 'MEMBERS');
  const b = await seed(guild, 'BOTS');
  await stats.ensureRefreshJob(gid);

  // Keep the channel: the row goes, the channel stays.
  assert.deepEqual(await stats.deleteStatChannel(guild, a.row, { keepChannel: true, actorTag: 't' }), { channelDeleted: false });
  assert.equal(guild.channels.cache.has(a.channel.id), true);
  assert.equal(await rowOf(a.row.id), null);
  assert.ok(await jobOf(gid), 'the job stays while a counter is left');

  // Without Manage Channels: nothing is deleted.
  b.channel.permissionsFor = () => NONE();
  await assert.rejects(stats.deleteStatChannel(guild, b.row, { actorTag: 't' }), (e) => e instanceof StatsError && e.code === 'MISSING_PERMISSIONS');
  assert.ok(await rowOf(b.row.id));
  assert.equal(guild.calls.deleted.length, 0);

  // Discord answers 50013 on the delete itself.
  b.channel.permissionsFor = () => ALL();
  b.channel.delete = async () => { throw Object.assign(new Error('no'), { code: 50013 }); };
  await assert.rejects(stats.deleteStatChannel(guild, b.row, { actorTag: 't' }), (e) => e.code === 'MISSING_PERMISSIONS');
  assert.ok(await rowOf(b.row.id), 'the row survives a refused delete');

  // Delete for real: marked as a bot deletion, the last row cancels the job.
  b.channel.delete = async (reason) => { guild.calls.deleted.push([b.channel.id, reason]); guild.channels.cache.delete(b.channel.id); };
  assert.deepEqual(await stats.deleteStatChannel(guild, b.row, { actorTag: 'tester#0' }), { channelDeleted: true });
  assert.equal(wasDeletedByBot(b.channel.id), 'stats');
  assert.match(guild.calls.deleted[0][1], /tester#0/);
  assert.equal(await rowOf(b.row.id), null);
  assert.equal(await jobOf(gid), null, 'no counters left, no job');
});

test('DB: deleteStatChannel deletes an uncached channel by id; a 10003 still removes the row', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3 });
  const a = await seed(guild, 'MEMBERS');
  guild.channels.cache.delete(a.channel.id); // not in the cache, but it may still exist
  await stats.deleteStatChannel(guild, a.row, { actorTag: 't' });
  assert.equal(guild.calls.deletedById[0][0], a.channel.id);
  assert.equal(await rowOf(a.row.id), null);

  const b = await seed(guild, 'BOTS');
  guild.channels.cache.delete(b.channel.id);
  guild.channels.delete = async () => { throw Object.assign(new Error('Unknown Channel'), { code: 10003 }); };
  assert.deepEqual(await stats.deleteStatChannel(guild, b.row, { actorTag: 't' }), { channelDeleted: false });
  assert.equal(await rowOf(b.row.id), null);
});

test('DB: findRowByRef finds a counter by id, mention and name; listRowsSafe is bounded to 25', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3 });
  const a = await seed(guild, 'MEMBERS', { name: 'Members: 3' });
  assert.equal((await stats.findRowByRef(guild, a.channel.id)).id, a.row.id);
  assert.equal((await stats.findRowByRef(guild, `<#${a.channel.id}>`)).id, a.row.id);
  assert.equal((await stats.findRowByRef(guild, ' members: 3 ')).id, a.row.id);
  assert.equal(await stats.findRowByRef(guild, 'nothing'), null);
  assert.equal(await stats.findRowByRef(guild, '123456789012345678'), null);
  assert.equal(await stats.findRowByRef(guild, ''), null);
  assert.equal(await stats.findRowByRef({ id: gid, channels: null }, 'x'), null, 'never throws');
  const safe = await stats.listRowsSafe(gid);
  assert.deepEqual(safe, [{ channelId: a.channel.id, kind: 'MEMBERS', roleId: null }]);
});

// ── The refresh engine ────────────────────────────────────────────────────────

test('DB: refreshGuild renames a changed counter once and reserves the slot first', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 12 });
  const { row, channel } = await seed(guild, 'MEMBERS', { name: 'Members: 10', lastValue: 10 });
  const result = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(result.renamed, 1);
  assert.equal(channel.name, 'Members: 12');
  assert.equal(guild.calls.renamed[0][2], 'Statistics channel update');
  const after = await rowOf(row.id);
  assert.equal(after.lastValue, 12);
  assert.ok(after.lastRenamedAt && Date.now() - after.lastRenamedAt.getTime() < 10_000);
  assert.ok(within(result.nextRunAt.getTime() - Date.now(), REFRESH_INTERVAL_MS - 5_000, REFRESH_INTERVAL_MS + 5_000));
});

test('DB: an up-to-date counter is not renamed and no row is written', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 12 });
  const { row } = await seed(guild, 'MEMBERS', { name: 'Members: 12', lastValue: 12 });
  const before = await rowOf(row.id);
  const result = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(result.unchanged, 1);
  assert.equal(result.renamed, 0);
  assert.equal(guild.calls.renamed.length, 0);
  assert.equal((await rowOf(row.id)).updatedAt.getTime(), before.updatedAt.getTime(), 'no database write when nothing changed');

  // Same name, stale lastValue: only the value is brought up to date.
  await prisma.statChannel.update({ where: { id: row.id }, data: { lastValue: 5 } });
  const again = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(again.unchanged, 1);
  assert.equal((await rowOf(row.id)).lastValue, 12);
  assert.equal(guild.calls.renamed.length, 0);
});

test('DB: coalesced renames: a change inside the gap waits, and the trailing update is scheduled, never dropped', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 12 });
  const renamedAt = new Date(Date.now() - 3 * 60_000);
  const { row, channel } = await seed(guild, 'MEMBERS', { name: 'Members: 10', lastValue: 10, lastRenamedAt: renamedAt });
  const result = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(result.renamed, 0);
  assert.equal(guild.calls.renamed.length, 0, 'no rename inside the gap');
  assert.equal(channel.name, 'Members: 10');
  assert.equal(result.waiting.length, 1);
  assert.equal(result.waiting[0].retryAt, renamedAt.getTime() + MIN_RENAME_GAP_MS);
  // The next run is the moment the rename becomes allowed (+5 s), not 10 minutes away.
  const delta = result.nextRunAt.getTime() - (renamedAt.getTime() + MIN_RENAME_GAP_MS + 5_000);
  assert.ok(Math.abs(delta) < 2_000, `next run at the retry time (off by ${delta} ms)`);
  assert.equal((await rowOf(row.id)).lastRenamedAt.getTime(), renamedAt.getTime(), 'a waiting counter keeps its reservation');

  // The run after the gap applies the LATEST value (a join in between is included).
  guild.members.cache.set('late', mkMember(false));
  guild.memberCount += 1;
  await prisma.statChannel.update({ where: { id: row.id }, data: { lastRenamedAt: new Date(Date.now() - 6 * 60_000) } });
  const later = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(later.renamed, 1);
  assert.equal(channel.name, 'Members: 13');
});

test('DB: concurrent refreshes rename a channel once (the lastRenamedAt reservation)', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 7 });
  await seed(guild, 'MEMBERS', { name: 'Members: 1', lastValue: 1 });
  const rows = await fresh(gid);
  const [a, b] = await Promise.all([stats.refreshGuild(guild, rows), stats.refreshGuild(guild, rows)]);
  assert.equal(a.renamed + b.renamed, 1);
  assert.equal(guild.calls.renamed.length, 1, 'exactly one setName call');
  assert.equal(a.waiting.length + b.waiting.length, 1, 'the loser is reported as waiting');
});

test('DB: a name Discord normalises is remembered so the next run does not rename in a loop', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 7 });
  const calls = [];
  const { row, channel } = await seed(guild, 'MEMBERS', {
    name: 'members: 1',
    setName: (ch, n) => { calls.push(n); ch.name = n.toLowerCase(); return ch; },
  });
  const first = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(first.renamed, 1);
  assert.equal(channel.name, 'members: 7');
  await prisma.statChannel.update({ where: { id: row.id }, data: { lastRenamedAt: new Date(Date.now() - 10 * 60_000) } });
  const second = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(second.unchanged, 1);
  assert.equal(calls.length, 1, 'no second rename');
});

test('DB: rename errors: a stuck request waits, 50013 skips, 10003 removes the row, anything else is reported', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 9, bots: 1 });
  const fail = (error) => () => { throw error; };
  const stuck = await seed(guild, 'MEMBERS', { name: 'x', setName: fail(new TimeoutError('stats rename')) });
  const noperm = await seed(guild, 'BOTS', { name: 'x', setName: fail(Object.assign(new Error('Missing Permissions'), { code: 50013 })) });
  const vanished = await seed(guild, 'ROLES', { name: 'x', setName: fail(Object.assign(new Error('Unknown Channel'), { code: 10003 })) });
  const broken = await seed(guild, 'BOOSTS', { name: 'x', setName: fail(new Error('boom')) });
  const result = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(result.renamed, 0);
  assert.deepEqual(result.waiting.map((w) => w.channelId), [stuck.channel.id]);
  assert.ok(within(result.waiting[0].retryAt - Date.now(), MIN_RENAME_GAP_MS - 5_000, MIN_RENAME_GAP_MS + 1_000));
  assert.deepEqual(result.skipped.map((s) => [s.channelId, s.problem]).sort(), [
    [noperm.channel.id, 'missing-permissions'], [broken.channel.id, 'error'],
  ].sort());
  assert.equal(result.removed, 1);
  assert.equal(await rowOf(vanished.row.id), null);
  for (const kept of [stuck, noperm, broken]) assert.ok(await rowOf(kept.row.id), 'only a 10003 removes a row');
  // The reservation was made before the request, so even the failures count against the gap.
  assert.ok((await rowOf(stuck.row.id)).lastRenamedAt);
  assert.ok((await rowOf(broken.row.id)).lastRenamedAt);
});

test('DB: PEAK_ONLINE only ever increases', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 20, online: 7 });
  const { row, channel } = await seed(guild, 'PEAK_ONLINE', { name: 'x', peakValue: 10, lastValue: 10 });
  await stats.refreshGuild(guild, await fresh(gid));
  assert.equal((await rowOf(row.id)).peakValue, 10, 'a lower online count never lowers the peak');
  assert.equal(channel.name, 'Peak Online: 10');

  for (let i = 7; i < 12; i++) guild.presences.cache.set(`p${i}`, { status: 'idle' });
  guild.presences.cache.set('off', { status: 'offline' });
  await prisma.statChannel.update({ where: { id: row.id }, data: { lastRenamedAt: new Date(Date.now() - 6 * 60_000) } });
  await stats.refreshGuild(guild, await fresh(gid));
  assert.equal((await rowOf(row.id)).peakValue, 12);
  assert.equal(channel.name, 'Peak Online: 12');

  // The peak is stored even while the rename itself is blocked by the gap.
  for (let i = 12; i < 15; i++) guild.presences.cache.set(`p${i}`, { status: 'dnd' });
  const waiting = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(waiting.waiting.length, 1);
  assert.equal((await rowOf(row.id)).peakValue, 15);

  // Direct check of the conditional write: a lower value matches nothing.
  const { count } = await prisma.statChannel.updateMany({
    where: { id: row.id, OR: [{ peakValue: null }, { peakValue: { lt: 3 } }] }, data: { peakValue: 3 },
  });
  assert.equal(count, 0);
});

test('DB: ONLINE counts non-offline presences, and uses the approximate count above 75,000 members', { skip: skipDb }, async () => {
  const gid = G();
  const small = makeGuild(gid, { humans: 5, online: 3 });
  small.presences.cache.set('x', { status: 'offline' });
  const { channel } = await seed(small, 'ONLINE', { name: 'x' });
  await stats.refreshGuild(small, await fresh(gid));
  assert.equal(channel.name, 'Online: 3');

  const gid2 = G();
  const big = makeGuild(gid2, { humans: 2, memberCount: 80_000, approx: 1234 });
  const bigRow = await seed(big, 'ONLINE', { name: 'x' });
  await stats.refreshGuild(big, await fresh(gid2));
  assert.equal(bigRow.channel.name, 'Online: 1,234');

  const gid3 = G();
  const noData = makeGuild(gid3, { humans: 2, memberCount: 80_000, approx: null });
  noData.fetch = async () => { throw new Error('boom'); };
  const noRow = await seed(noData, 'ONLINE', { name: 'keep me' });
  const r = await stats.refreshGuild(noData, await fresh(gid3));
  assert.equal(noRow.channel.name, 'keep me');
  assert.deepEqual(r.skipped, [{ channelId: noRow.channel.id, problem: 'presence-unavailable' }]);
});

test('DB: a member-list count from an incomplete cache is never shown', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3, memberCount: 8 });
  const { channel } = await seed(guild, 'HUMANS', { name: 'Humans: 3', lastValue: 3 });
  const blocked = await stats.refreshGuild(guild, await fresh(gid), { allowFetch: false });
  assert.deepEqual(blocked.skipped.map((s) => s.problem), ['members-incomplete']);
  assert.equal(channel.name, 'Humans: 3', 'the old name stays');
  assert.ok(within(blocked.nextRunAt.getTime() - Date.now(), 50_000, 70_000), 'soon');

  // With the fetch allowed the list completes and the count is shown.
  guild.members.fetch = async () => {
    for (let i = 0; i < 5; i++) guild.members.cache.set(`n${i}`, mkMember(i === 4));
    return guild.members.cache;
  };
  const done = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(done.renamed, 1);
  assert.equal(channel.name, 'Humans: 7');
});

test('DB: a rate-limited member fetch backs off and keeps the old name', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3, memberCount: 8 });
  let fetches = 0;
  guild.members.fetch = async () => {
    fetches++;
    throw Object.assign(new Error('rate limited'), { name: 'GatewayRateLimitError', data: { retry_after: 45 } });
  };
  const { channel } = await seed(guild, 'BOTS', { name: 'Bots: 0' });
  const r = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(fetches, 1);
  assert.equal(channel.name, 'Bots: 0');
  assert.deepEqual(r.skipped.map((s) => s.problem), ['members-incomplete']);
  assert.ok(r.nextRunAt.getTime() - Date.now() >= 40_000, 'the next run waits for the back-off');
  const again = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(fetches, 1, 'no second fetch while backed off');
  assert.equal(again.skipped.length, 1);
});

test('DB: ROLE_MEMBERS counts the role, shows its name, and keeps the name when the role is deleted', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 0 });
  addRole(guild, 'role1', 'Mods');
  for (let i = 0; i < 4; i++) guild.members.cache.set(`m${i}`, mkMember(false, i < 3 ? ['role1'] : []));
  guild.memberCount = 4;
  const { row, channel } = await seed(guild, 'ROLE_MEMBERS', { name: 'x', roleId: 'role1', template: '{role}: {count}' });
  await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(channel.name, 'Mods: 3');

  guild.roles.cache.delete('role1');
  await prisma.statChannel.update({ where: { id: row.id }, data: { lastRenamedAt: null } });
  const r = await stats.refreshGuild(guild, await fresh(gid));
  assert.deepEqual(r.skipped.map((s) => s.problem), ['role-deleted']);
  assert.equal(channel.name, 'Mods: 3');
  assert.ok(await rowOf(row.id), 'the row is kept; only the admin removes it');
});

test('DB: a counter without Manage Channels is skipped with a friendly problem, not renamed', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  const { row, channel } = await seed(guild, 'MEMBERS', { name: 'x', perms: NONE() });
  const r = await stats.refreshGuild(guild, await fresh(gid));
  assert.deepEqual(r.skipped, [{ channelId: channel.id, problem: 'missing-permissions' }]);
  assert.equal(guild.calls.renamed.length, 0);
  assert.equal((await rowOf(row.id)).lastRenamedAt, null, 'no slot was reserved');
});

test('DB: the time budget defers the remaining counters and brings the next run to ~60 s', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  await seed(guild, 'MEMBERS', { name: 'x' });
  await seed(guild, 'BOTS', { name: 'x' });
  const r = await stats.refreshGuild(guild, await fresh(gid), { startedAt: Date.now() - 41_000 });
  assert.equal(r.deferred, 2);
  assert.equal(r.renamed, 0);
  assert.ok(within(r.nextRunAt.getTime() - Date.now(), 55_000, 65_000));
});

test('DB: an unavailable guild is left alone', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5, available: false });
  const { row, channel } = await seed(guild, 'MEMBERS', { name: 'x' });
  guild.channels.cache.delete(channel.id); // an unavailable guild has empty caches
  const r = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(r.removed, 0);
  assert.equal(r.renamed, 0);
  assert.ok(await rowOf(row.id));
});

// ── Rule 20: a cache miss is not a deletion ──────────────────────────────────

test('DB (rule 20): a counter missing from the cache is removed only after a REST 10003', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  const gone = await seed(guild, 'MEMBERS', { name: 'x' });
  guild.channels.cache.delete(gone.channel.id);
  const r = await stats.refreshGuild(guild, await fresh(gid)); // channels.fetch answers 10003
  assert.equal(r.removed, 1);
  assert.equal(await rowOf(gone.row.id), null);
});

test('DB (rule 20): 50001, a server error and an unavailable guild all keep the row', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  const a = await seed(guild, 'MEMBERS', { name: 'x' });
  const b = await seed(guild, 'BOTS', { name: 'x' });
  guild.channels.cache.delete(a.channel.id);
  guild.channels.cache.delete(b.channel.id);
  guild.fetchChannel = async (id) => {
    if (id === a.channel.id) throw Object.assign(new Error('Missing Access'), { code: 50001 });
    throw Object.assign(new Error('Service Unavailable'), { status: 503 });
  };
  const r = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(r.removed, 0);
  assert.deepEqual(r.skipped.map((s) => [s.channelId, s.problem]).sort(), [
    [a.channel.id, 'missing-permissions'], [b.channel.id, 'channel-unknown'],
  ].sort());
  assert.ok(await rowOf(a.row.id));
  assert.ok(await rowOf(b.row.id));
  assert.ok(within(r.nextRunAt.getTime() - Date.now(), 55_000, 130_000), 'an unchecked channel is looked at again soon');

  const down = await stats.confirmChannel({ ...guild, available: false, channels: { cache: new Map(), fetch: async () => { throw new Error('never called'); } } }, 'x');
  assert.equal(down.state, 'unknown', 'an unavailable guild is never "gone"');
});

test('DB (rule 20): a channel missing from the cache that Discord still returns is renamed normally', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  const { row, channel } = await seed(guild, 'MEMBERS', { name: 'x' });
  guild.channels.cache.delete(channel.id);
  guild.fetchChannel = async () => channel;
  const r = await stats.refreshGuild(guild, await fresh(gid));
  assert.equal(r.renamed, 1);
  assert.equal(channel.name, 'Members: 5');
  assert.ok(await rowOf(row.id));
});

test('DB (rule 20): create prunes only rows whose channel is confirmed gone, and counts unchecked rows against the caps', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  const client = { user: { id: BOT } };
  const dead = await seed(guild, 'ROLES', { name: 'x' });
  const unsure = await seed(guild, 'BOOSTS', { name: 'x' });
  guild.channels.cache.delete(dead.channel.id);
  guild.channels.cache.delete(unsure.channel.id);
  guild.fetchChannel = async (id) => {
    if (id === dead.channel.id) throw Object.assign(new Error('Unknown Channel'), { code: 10003 });
    throw Object.assign(new Error('Server Error'), { status: 500 });
  };
  await stats.createStatChannel({ guild, client, kind: 'MEMBERS', template: 'Members: {count}', actorTag: 't' });
  assert.equal(await rowOf(dead.row.id), null, 'a confirmed 10003 is pruned');
  assert.ok(await rowOf(unsure.row.id), 'an unchecked row is kept');
  // The unchecked row still blocks a duplicate of its kind.
  await assert.rejects(
    stats.createStatChannel({ guild, client, kind: 'BOOSTS', template: 'Boosts: {count}', actorTag: 't' }),
    (e) => e.code === 'DUPLICATE' && e.channelId === unsure.channel.id,
  );
});

test('DB (rule 20): the channelDelete event removes the row and cancels the job with the last counter', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  const a = await seed(guild, 'MEMBERS', { name: 'x' });
  const b = await seed(guild, 'BOTS', { name: 'x' });
  await stats.ensureRefreshJob(gid);
  const ctx = (channel) => ({ guildId: gid, channel: { id: channel.id, type: ChannelType.GuildVoice }, getSettings: () => getSettingsSafe(gid) });
  await stats.onChannelDelete(ctx(a.channel));
  assert.equal(await rowOf(a.row.id), null);
  assert.ok(await jobOf(gid), 'a counter is left, the job stays');
  await stats.onChannelDelete(ctx(b.channel));
  assert.equal(await rowOf(b.row.id), null);
  assert.equal(await jobOf(gid), null);
  await stats.onChannelDelete({ guildId: gid, channel: { id: 'zzz', type: ChannelType.GuildVoice }, getSettings: () => null }); // unknown channel: no-op
});

test('DB: deleting the stats category clears statsCategoryId (and only that category)', { skip: skipDb }, async () => {
  const gid = G();
  await updateSettings(gid, { statsCategoryId: '555000000000000001' });
  const ctx = (id) => ({ guildId: gid, channel: { id, type: ChannelType.GuildCategory }, getSettings: () => getSettingsSafe(gid) });
  await stats.onChannelDelete(ctx('555000000000000009'));
  assert.equal((await getSettingsSafe(gid)).statsCategoryId, '555000000000000001');
  await stats.onChannelDelete(ctx('555000000000000001'));
  assert.equal((await getSettingsSafe(gid)).statsCategoryId, null);
});

// ── Scheduling: ensureRefreshJob, pullRefresh, the job body, the hooks ───────

test("DB: ensureRefreshJob creates, revives a FAILED job, and 'earliest' never pushes a healthy job later", { skip: skipDb }, async () => {
  const gid = G();
  const at = (ms) => new Date(Date.now() + ms);
  assert.equal(await stats.ensureRefreshJob(gid, at(10 * 60_000)), true);
  const created = await jobOf(gid);
  assert.equal(created.status, 'PENDING');
  assert.equal(created.type, 'stats.refresh');

  await stats.ensureRefreshJob(gid, at(20 * 60_000));
  assert.equal((await jobOf(gid)).runAt.getTime(), created.runAt.getTime(), 'a later time does not postpone a healthy job');
  await stats.ensureRefreshJob(gid, at(2 * 60_000));
  assert.ok((await jobOf(gid)).runAt.getTime() < created.runAt.getTime(), 'an earlier time pulls it forward');

  await prisma.scheduledJob.update({ where: { dedupeKey: refreshJobKey(gid) }, data: { status: 'FAILED', lastError: 'boom' } });
  await stats.ensureRefreshJob(gid, at(30 * 60_000));
  const revived = await jobOf(gid);
  assert.equal(revived.status, 'PENDING');
  assert.equal(revived.lastError, null);
  assert.ok(within(revived.runAt.getTime() - Date.now(), 29 * 60_000, 31 * 60_000), 'a revived job takes the requested time');
});

test('DB: pullRefresh pulls the job to now + delay and never pushes it later', { skip: skipDb }, async () => {
  const gid = G();
  await stats.ensureRefreshJob(gid, new Date(Date.now() + 10 * 60_000));
  assert.equal(await stats.pullRefresh(gid, 60_000), true);
  const pulled = await jobOf(gid);
  assert.ok(within(pulled.runAt.getTime() - Date.now(), 50_000, 61_000));
  await stats.pullRefresh(gid, 5 * 60_000);
  assert.equal((await jobOf(gid)).runAt.getTime(), pulled.runAt.getTime(), 'a later pull is a no-op');
});

test('DB: refreshJobHandler returns the next run, idles while the module is off and deletes the job when no counters are left', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  await ensureGuildSettings(gid);
  const job = { guildId: gid };
  assert.equal(await stats.refreshJobHandler(job, { guild }), undefined, 'no counters: the job is deleted');

  const { channel } = await seed(guild, 'MEMBERS', { name: 'x' });
  const out = await stats.refreshJobHandler(job, { guild });
  assert.ok(out.nextRunAt instanceof Date);
  assert.equal(channel.name, 'Members: 5');
  assert.ok(within(out.nextRunAt.getTime() - Date.now(), REFRESH_INTERVAL_MS - 5_000, REFRESH_INTERVAL_MS + 5_000));

  await updateSettings(gid, { statsEnabled: false });
  guild.members.cache.set('new', mkMember(false));
  guild.memberCount += 1;
  const off = await stats.refreshJobHandler(job, { guild });
  assert.ok(within(off.nextRunAt.getTime() - Date.now(), REFRESH_INTERVAL_MS - 5_000, REFRESH_INTERVAL_MS + 5_000));
  assert.equal(channel.name, 'Members: 5', 'module off: no Discord calls');
  await updateSettings(gid, { statsEnabled: true });

  const unavailable = await stats.refreshJobHandler(job, { guild: { ...guild, available: false } });
  assert.ok(within(unavailable.nextRunAt.getTime() - Date.now(), 4 * 60_000, 6 * 60_000));
  const missingGuild = await stats.refreshJobHandler(job, {});
  assert.ok(missingGuild.nextRunAt instanceof Date, 'never throws');
});

test('DB: a member event during a run caps the next run (the trailing value is not dropped)', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  await seed(guild, 'MEMBERS', {
    name: 'x',
    setName: (ch, n) => {
      // A member joins while the rename is in flight: the onMemberChange claim happens now.
      claimPull(gid);
      ch.name = n;
      return ch;
    },
  });
  const out = await stats.refreshJobHandler({ guildId: gid }, { guild });
  const delta = out.nextRunAt.getTime() - Date.now();
  assert.ok(within(delta, 50_000, 70_000), `next run ~60 s after the event, not 10 min (${delta} ms)`);
});

test('DB: an edit or a new counter during a run also caps the next run (pullRefresh / ensureRefreshJob while RUNNING)', { skip: skipDb }, async () => {
  const run = async (during, expectedMs) => {
    const gid = G();
    const guild = makeGuild(gid, { humans: 5 });
    await seed(guild, 'MEMBERS', {
      name: 'x',
      setName: async (ch, n) => {
        await during(gid);
        ch.name = n;
        return ch;
      },
    });
    // The job as the scheduler leaves it while the handler runs: RUNNING with a past runAt, which 'earliest' keeps.
    await stats.ensureRefreshJob(gid, new Date(Date.now() - 1_000));
    await prisma.scheduledJob.update({ where: { dedupeKey: refreshJobKey(gid) }, data: { status: 'RUNNING' } });
    const pastRunAt = (await jobOf(gid)).runAt.getTime();
    const out = await stats.refreshJobHandler({ guildId: gid }, { guild });
    const delta = out.nextRunAt.getTime() - Date.now();
    assert.ok(within(delta, expectedMs - 2_000, expectedMs + 6_000), `next run ~${expectedMs} ms away, not 10 min (${delta} ms)`);
    assert.equal((await jobOf(gid)).runAt.getTime(), pastRunAt, 'the RUNNING job itself was not moved');
  };
  await run((gid) => stats.pullRefresh(gid, stats.EDIT_PULL_DELAY_MS), stats.EDIT_PULL_DELAY_MS); // /stats edit
  await run((gid) => stats.ensureRefreshJob(gid, new Date(Date.now() + MIN_NEXT_RUN_MS)), MIN_NEXT_RUN_MS); // a new counter with an unknown value

  // Outside a run nothing is remembered (the claim only exists while a refresh is in flight).
  const quietGuild = G();
  await stats.pullRefresh(quietGuild, 5_000);
  assert.equal(claimPull(quietGuild), true, 'no pendingPull entry was written outside a run');
});

test('DB: refreshJobHandler survives a failing database and the busy guard', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  await seed(guild, 'MEMBERS', { name: 'x' });
  let release;
  const hold = new Promise((r) => { release = r; });
  const running = runExclusive(gid, () => hold);
  const busy = await stats.refreshJobHandler({ guildId: gid }, { guild });
  assert.ok(within(busy.nextRunAt.getTime() - Date.now(), 55_000, 65_000), 'a busy guild is retried in 60 s');
  release();
  await running;

  const broken = { guildId: gid };
  const original = prisma.statChannel.findMany;
  // A throwing delegate method must not escape the handler.
  const out = await (async () => {
    try {
      prisma.statChannel.findMany = () => { throw new Error('db down'); };
      return await stats.refreshJobHandler(broken, { guild });
    } finally {
      prisma.statChannel.findMany = original;
    }
  })();
  assert.ok(within(out.nextRunAt.getTime() - Date.now(), 4 * 60_000, 6 * 60_000), 'a DB failure: retry in 5 min');
});

test('DB: refreshNow reports empty, busy and the result, and keeps the job on the earliest next run', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  await ensureGuildSettings(gid);
  assert.deepEqual(await stats.refreshNow(guild), { empty: true });
  const { channel } = await seed(guild, 'MEMBERS', { name: 'x' });
  const out = await stats.refreshNow(guild);
  assert.equal(out.result.renamed, 1);
  assert.equal(channel.name, 'Members: 5');
  assert.ok(await jobOf(gid));
  let release;
  const hold = new Promise((r) => { release = r; });
  const running = runExclusive(gid, () => hold);
  assert.deepEqual(await stats.refreshNow(guild), { busy: true });
  release();
  await running;
});

test('DB: onGuildReady keeps the job alive for guilds with counters and primes an empty memo for the rest', { skip: skipDb }, async () => {
  const withRows = G();
  const guild = makeGuild(withRows, { humans: 5 });
  await seed(guild, 'MEMBERS');
  await stats.onGuildReady({ guildId: withRows });
  const job = await jobOf(withRows);
  assert.ok(within(job.runAt.getTime() - Date.now(), 25_000, 125_000), 'first run at +30 s ... +120 s');

  const without = G();
  await ensureGuildSettings(without);
  await stats.onGuildReady({ guildId: without });
  assert.equal(await jobOf(without), null);
  assert.equal(memberEventFilter({ guildId: without }), false, 'later member events cost nothing');

  // A FAILED job is repaired on start.
  await prisma.scheduledJob.update({ where: { dedupeKey: refreshJobKey(withRows) }, data: { status: 'FAILED' } });
  await stats.onGuildReady({ guildId: withRows });
  assert.equal((await jobOf(withRows)).status, 'PENDING');
});

test('DB: onMemberChange pulls the job once per minute, coalescing bursts, and ignores guilds without member-driven counters', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  await seed(guild, 'MEMBERS');
  await stats.ensureRefreshJob(gid, new Date(Date.now() + 10 * 60_000));
  await stats.onMemberChange({ guildId: gid });
  const pulled = await jobOf(gid);
  assert.ok(within(pulled.runAt.getTime() - Date.now(), 50_000, 61_000));
  // A burst: later events do not touch the database again (the claim is held).
  await prisma.scheduledJob.update({ where: { dedupeKey: refreshJobKey(gid) }, data: { runAt: new Date(Date.now() + 9 * 60_000) } });
  await stats.onMemberChange({ guildId: gid });
  assert.ok((await jobOf(gid)).runAt.getTime() - Date.now() > 8 * 60_000, 'the second event was coalesced');

  const other = G();
  const g2 = makeGuild(other, { humans: 5 });
  await seed(g2, 'TEXT_CHANNELS');
  await stats.onMemberChange({ guildId: other });
  assert.equal(await jobOf(other), null);

  const none = G();
  await ensureGuildSettings(none);
  await stats.onMemberChange({ guildId: none });
  assert.equal(await jobOf(none), null);
});

// ── The /stats command ───────────────────────────────────────────────────────

function fakeInteraction(guild, sub, { strings = {}, role = null, channel = null, bool = null, focused = '' } = {}) {
  const out = { replies: [], deferred: false, responded: null };
  const interaction = {
    guild,
    guildId: guild.id,
    user: { tag: 'tester#0' },
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => null,
      get: () => channel,
      getString: (name) => strings[name] ?? null,
      getRole: () => role,
      getChannel: () => channel,
      getBoolean: () => bool,
      getFocused: () => focused,
    },
    deferReply: async () => { out.deferred = true; },
    editReply: async (payload) => { out.replies.push(payload); },
    respond: async (choices) => { out.responded = choices; },
    responded: false,
    out,
  };
  return interaction;
}

const lastEmbed = (i) => i.out.replies.at(-1).embeds[0].data;
const client = { user: { id: BOT } };

test('DB: /stats create: validation errors come before any Discord call', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  const run = async (opts) => {
    const i = fakeInteraction(guild, 'create', opts);
    await statsCmd.execute(i, client);
    assert.equal(i.out.deferred, true, 'deferReply first');
    return lastEmbed(i);
  };
  assert.match((await run({ strings: { kind: 'ROLE_MEMBERS' } })).title, /Role Required/);
  assert.match((await run({ strings: { kind: 'MEMBERS' }, role: { id: 'r1', name: 'R' } })).title, /Role Not Used/);
  assert.match((await run({ strings: { kind: 'ROLE_MEMBERS' }, role: { id: gid, name: '@everyone' } })).title, /Invalid Role/);
  assert.match((await run({ strings: { kind: 'MEMBERS', template: 'no count here' } })).title, /Invalid Template/);
  const big = makeGuild(G(), { humans: 2, memberCount: MEMBER_LIST_MAX_MEMBERS + 1 });
  const i = fakeInteraction(big, 'create', { strings: { kind: 'HUMANS' } });
  await statsCmd.execute(i, client);
  assert.match(lastEmbed(i).title, /Server Too Large/);
  assert.equal(guild.calls.created.length, 0);

  guild.members.me.permissions = NONE();
  const noPerms = fakeInteraction(guild, 'create', { strings: { kind: 'MEMBERS' } });
  await statsCmd.execute(noPerms, client);
  assert.match(lastEmbed(noPerms).title, /Missing Permissions/);
  assert.match(lastEmbed(noPerms).description, /View Channel.*Manage Channels.*Connect/);
  assert.equal(guild.calls.created.length, 0);
  guild.members.me.permissions = ALL();

  const catCheck = fakeInteraction(guild, 'create', { strings: { kind: 'MEMBERS' }, channel: { id: 'cat1', name: 'Cat', permissionsFor: () => NONE(), guild } });
  await statsCmd.execute(catCheck, client);
  assert.match(lastEmbed(catCheck).description, /in the \*\*Cat\*\* category/);
});

test('DB: /stats create, list, edit, refresh, category and delete end to end', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 8, bots: 1 });
  await ensureGuildSettings(gid);
  const run = async (sub, opts) => {
    const i = fakeInteraction(guild, sub, opts);
    await statsCmd.execute(i, client);
    return lastEmbed(i);
  };

  const created = await run('create', { strings: { kind: 'MEMBERS' } });
  assert.match(created.title, /Counter Created/);
  assert.match(created.description, /\*\*Members\*\*/);
  assert.match(created.description, /Members: \{count\}/);
  assert.equal(guild.calls.created[0].name, 'Members: 9');

  const dup = await run('create', { strings: { kind: 'MEMBERS' } });
  assert.match(dup.title, /Already Exists/);

  addRole(guild, 'roleA', 'Staff');
  const roleCounter = await run('create', { strings: { kind: 'ROLE_MEMBERS', template: '{role} online: {count}' }, role: guild.roles.cache.get('roleA') });
  assert.match(roleCounter.title, /Counter Created/);
  assert.match(roleCounter.description, /<@&roleA>/);
  assert.equal(guild.calls.created[1].name, 'Staff online: 0');

  const list = await run('list');
  assert.match(list.title, /Statistics Channels/);
  assert.match(list.description, /2\/20 counters · 1\/8 role counters/);
  assert.match(list.description, /New counters go to: top level/);
  assert.ok(list.fields[0].value.includes('**Members**'));
  assert.ok(list.fields[0].value.includes('not renamed yet'));
  assert.ok(list.fields[0].value.includes('showing **9**'));

  const row = (await fresh(gid)).find((r) => r.kind === 'MEMBERS');
  const edited = await run('edit', { strings: { channel: row.channelId, template: 'People: {count}' } });
  assert.match(edited.title, /Counter Updated/);
  assert.match(edited.description, /People: 9/);
  assert.match((await run('edit', { strings: { channel: row.channelId, template: 'nope' } })).title, /Invalid Template/);
  assert.match((await run('edit', { strings: { channel: '123456789012345678', template: 'x {count}' } })).title, /Not Found/);

  const refreshed = await run('refresh');
  assert.match(refreshed.title, /Counters Refreshed/);
  assert.match(refreshed.description, /Renamed: 1/);

  const cat = { id: '888000000000000001', name: 'Stats', permissionsFor: () => ALL(), guild };
  const set = await run('category', { channel: cat });
  assert.match(set.title, /Stats Category Set/);
  assert.equal((await getSettingsSafe(gid)).statsCategoryId, cat.id);
  const catList = await run('list');
  assert.match(catList.description, /<#888000000000000001>/);
  assert.match((await run('category', {})).title, /Stats Category Cleared/);
  assert.equal((await getSettingsSafe(gid)).statsCategoryId, null);
  const bad = await run('category', { channel: { ...cat, permissionsFor: () => NONE() } });
  assert.match(bad.title, /Missing Permissions/);

  const kept = await run('delete', { strings: { channel: row.channelId }, bool: true });
  assert.match(kept.title, /Counter Removed/);
  assert.match(kept.description, /was kept and no longer updates/);
  const roleRow = (await fresh(gid)).find((r) => r.kind === 'ROLE_MEMBERS');
  const gone = await run('delete', { strings: { channel: roleRow.channelId } });
  assert.match(gone.description, /its channel deleted/);
  assert.match(gone.description, /<@&roleA>/);
  assert.match((await run('list')).description, /No counters yet/);
  assert.equal(await jobOf(gid), null);
});

test('DB: /stats shows the module-off note, and refresh refuses while the module is off', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3 });
  await updateSettings(gid, { statsEnabled: false });
  const run = async (sub, opts) => {
    const i = fakeInteraction(guild, sub, opts);
    await statsCmd.execute(i, client);
    return lastEmbed(i);
  };
  const created = await run('create', { strings: { kind: 'BOOSTS' } });
  assert.match(created.description, /module is off, so counters don't update/);
  assert.match((await run('list')).description, /\/module enable stats/);
  const refresh = await run('refresh');
  assert.match(refresh.title, /Module Disabled/);
  assert.match((await run('category', {})).description, /module is off/);
  const row = (await fresh(gid))[0];
  assert.match((await run('delete', { strings: { channel: row.channelId } })).description, /module is off/);
});

test('DB: /stats refresh lists skipped counters and the rename limit wait', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  await ensureGuildSettings(gid);
  await seed(guild, 'MEMBERS', { name: 'x', lastRenamedAt: new Date(Date.now() - 60_000) });
  await seed(guild, 'BOTS', { name: 'x', perms: NONE() });
  const i = fakeInteraction(guild, 'refresh');
  await statsCmd.execute(i, client);
  const e = lastEmbed(i);
  assert.match(e.description, /Waiting for Discord's rename limit: 1/);
  assert.match(e.description, /I need View Channel and Manage Channels/);
});

test('DB: /stats create clears a saved category only when Discord confirms it is gone (rule 20)', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  await updateSettings(gid, { statsCategoryId: '444000000000000001' });
  const create = async () => {
    const i = fakeInteraction(guild, 'create', { strings: { kind: 'MEMBERS' } });
    await statsCmd.execute(i, client);
    return lastEmbed(i);
  };
  // Unknown (a server error): nothing is cleared, nothing is created.
  guild.fetchChannel = async () => { throw Object.assign(new Error('Server Error'), { status: 500 }); };
  const unknown = await create();
  assert.match(unknown.title, /Try Again/);
  assert.equal((await getSettingsSafe(gid)).statsCategoryId, '444000000000000001');
  assert.equal(guild.calls.created.length, 0);

  // Confirmed gone: created at the top level and the setting is cleared.
  guild.fetchChannel = null;
  const done = await create();
  assert.match(done.title, /Counter Created/);
  assert.match(done.description, /saved stats category no longer exists/);
  assert.equal(guild.calls.created[0].parent, null);
  await new Promise((r) => setTimeout(r, 200)); // the clear is fire-and-forget
  assert.equal((await getSettingsSafe(gid)).statsCategoryId, null);
});

test('DB: /stats create uses a saved category that exists', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  addChannel(guild, { id: '444000000000000002', name: 'Stats', type: ChannelType.GuildCategory });
  await updateSettings(gid, { statsCategoryId: '444000000000000002' });
  const i = fakeInteraction(guild, 'create', { strings: { kind: 'ROLES' } });
  await statsCmd.execute(i, client);
  assert.match(lastEmbed(i).title, /Counter Created/);
  assert.equal(guild.calls.created[0].parent, '444000000000000002');
});

test('DB: /stats answers with the generic error when something unexpected fails', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 5 });
  const original = prisma.statChannel.findMany;
  const i = fakeInteraction(guild, 'list');
  try {
    prisma.statChannel.findMany = () => { throw new Error('db down'); };
    await statsCmd.execute(i, client);
  } finally {
    prisma.statChannel.findMany = original;
  }
  assert.match(lastEmbed(i).title, /Something Went Wrong/);
});

test('DB: /stats autocomplete lists the counters by channel name, kind and role, filtered', { skip: skipDb }, async () => {
  const gid = G();
  const guild = makeGuild(gid, { humans: 3 });
  addRole(guild, 'roleB', 'Crew');
  const a = await seed(guild, 'MEMBERS', { name: 'Members: 3' });
  const b = await seed(guild, 'ROLE_MEMBERS', { name: 'Crew: 1', roleId: 'roleB', template: '{role}: {count}' });
  const all = fakeInteraction(guild, 'edit', { focused: '' });
  await statsCmd.autocomplete(all);
  assert.deepEqual(all.out.responded.map((c) => c.value).sort(), [a.channel.id, b.channel.id].sort());
  const mine = all.out.responded.find((c) => c.value === b.channel.id);
  assert.equal(mine.name, 'Crew: 1 — Role members (@Crew)');
  const filtered = fakeInteraction(guild, 'edit', { focused: 'CREW' });
  await statsCmd.autocomplete(filtered);
  assert.deepEqual(filtered.out.responded.map((c) => c.value), [b.channel.id]);
});
