// pkg-levels tests (pkg-levels.md §10): level maths, XP rolls, boosts, no-XP channels, voice eligibility and sessions,
// reward-role planning, announce routing, level-up payloads, rank tie rule, rank-card style and rendering, command
// payloads, the plugin declaration and a static scan. Offline by default. With TEST_DATABASE_URL (a migrated database)
// the DB cases also run (atomic XP SQL, the level-change pipeline, voice payouts, bulk gives, /levelconfig and /xp):
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/levels.test.js
import { test, after, afterEach, mock } from 'node:test';
import { EventEmitter } from 'node:events';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { deflateSync } from 'node:zlib';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is only used with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ButtonStyle, ChannelType, Collection, ComponentType } = await import('discord.js');
const { Prisma } = await import('@prisma/client');
const svc = await import('../src/services/xpService.js');
const card = await import('../src/utils/profileCard.js');
const { default: redis, cache } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const settingsSvc = await import('../src/services/settingsService.js');
const { LIMITS, PERMISSION_LEVELS, XP_MAX, xpForLevel } = await import('../src/config/constants.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { clearHooks, registerHook } = await import('../src/handlers/hookRegistry.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { default: logger } = await import('../src/utils/logger.js');
const { plugin } = await import('../src/plugins/levels.js');
const levelconfig = await import('../src/commands/admin/levelconfig.js');
const xpCmd = await import('../src/commands/leveling/xp.js');
const rankCmd = await import('../src/commands/leveling/rank.js');
const boardCmd = await import('../src/commands/leveling/leaderboard.js');

const {
  levelFromXp, xpIntoLevel, totalXpForLevel, rollXp, normalizeXpRange, normalizeBoostRoles, bestBoostMultiplier,
  hasNoXpRole, isNoXpChannel, voiceEligibility, countOtherListeners, parseVoiceSession, computeVoiceSegment,
  voiceXpForMinutes, planLevelRoles, resolveAnnounceTarget, levelUpVars, buildLevelUpPayload, rankWhere,
  DEFAULT_LEVELUP_MESSAGE, ANNOUNCE_MODES, VOICE_CHECKPOINT_MS,
} = svc;

// Silence the Redis reconnect noise and the logger output of the failure-path tests.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const GUILDS = Array.from({ length: 16 }, (_, i) => `95500900000000${String(i + 1).padStart(4, '0')}`);
const G = (n) => GUILDS[n - 1];
const U = (n) => `95500910000000${String(n).padStart(4, '0')}`;
const R = (n) => `95500920000000${String(n).padStart(4, '0')}`;
const C = (n) => `95500930000000${String(n).padStart(4, '0')}`;

after(async () => {
  Object.assign(console, quiet);
  clearHooks();
  mock.restoreAll();
  redis.disconnect();
  if (DB) {
    // Leave nothing behind in a shared test database.
    for (const guildId of GUILDS) {
      await prisma.userLevel.deleteMany({ where: { guildId } }).catch(() => {});
      await prisma.levelRole.deleteMany({ where: { guildId } }).catch(() => {});
      await prisma.scheduledJob.deleteMany({ where: { guildId } }).catch(() => {});
      await prisma.guildSettings.deleteMany({ where: { guildId } }).catch(() => {});
    }
  }
  await prisma.$disconnect().catch(() => {});
});
afterEach(() => {
  clearHooks();
  mock.restoreAll();
});

// ── Fakes ────────────────────────────────────────────────────────────────────

const AVATAR = 'https://cdn.example.test/avatar.png';

function fakeUser(id, over = {}) {
  return { id, username: `user${id.slice(-4)}`, globalName: null, bot: false, displayAvatarURL: () => AVATAR, createdTimestamp: 1_600_000_000_000, ...over };
}

function fakeRole(id, { position = 1, managed = false } = {}) {
  return { id, name: `role-${id.slice(-3)}`, position, managed, permissions: { has: () => false } };
}

/** A GuildMember with a role cache that follows add/remove and records every call. */
function fakeMember(guild, id, { roles = [], bot = false, displayName, voice = null } = {}) {
  const user = fakeUser(id, { bot });
  const calls = [];
  const sent = [];
  const member = {
    id,
    user,
    guild,
    displayName: displayName ?? `Member ${id.slice(-4)}`,
    displayAvatarURL: () => AVATAR,
    joinedTimestamp: 1_650_000_000_000,
    voice,
    calls,
    sent,
    roles: {
      cache: new Map(roles.map((r) => [r, { id: r }])),
      async add(roleId, reason) { calls.push(['add', roleId, reason]); member.roles.cache.set(roleId, { id: roleId }); },
      async remove(roleId, reason) { calls.push(['remove', roleId, reason]); member.roles.cache.delete(roleId); },
    },
    async send(payload) { sent.push(payload); return payload; },
  };
  guild.members.cache.set(id, member);
  return member;
}

function fakeChannel(id, { type = ChannelType.GuildText, parentId = null, members = null, canSend = true } = {}) {
  const sent = [];
  const channel = {
    id,
    type,
    parentId,
    members,
    sent,
    guild: { members: { me: {} } },
    isTextBased: () => true,
    isThread: () => false,
    permissionsFor: () => ({ has: () => canSend }),
    async send(payload) { sent.push(payload); return payload; },
  };
  return channel;
}

function fakeGuild(id, { roles = [], channels = [], botPosition = 100, readyTimestamp = 0, afkChannelId = null } = {}) {
  const me = { permissions: { has: () => true }, roles: { highest: { position: botPosition } } };
  const guild = {
    id,
    name: 'Test Guild',
    available: true,
    ownerId: '1',
    memberCount: 5,
    afkChannelId,
    client: { readyTimestamp },
    iconURL: () => null,
    roles: { cache: new Map(roles.map((r) => [r.id, r])) },
    channels: { cache: new Map(channels.map((c) => [c.id, c])), async fetch() { return null; } },
    members: { me, cache: new Map(), async fetch() { return null; } },
    voiceStates: { cache: new Map() },
  };
  return guild;
}

/** An in-memory stand-in for the Redis cache (values round-trip through JSON like the real one). */
function installFakeCache() {
  const store = new Map();
  mock.method(cache, 'get', async (k) => (store.has(k) ? JSON.parse(store.get(k)) : null));
  mock.method(cache, 'set', async (k, v) => { store.set(k, JSON.stringify(v)); return true; });
  mock.method(cache, 'del', async (...ks) => { for (const k of ks.flat()) store.delete(k); return true; });
  mock.method(cache, 'setNX', async (k, v) => {
    if (store.has(k)) return false;
    store.set(k, JSON.stringify(v));
    return true;
  });
  mock.method(cache, 'invalidate', async (pattern) => {
    const prefix = pattern.replace(/\*$/, '');
    let n = 0;
    for (const k of [...store.keys()]) if (k.startsWith(prefix)) { store.delete(k); n++; }
    return n;
  });
  return store;
}

// ── 1. Level maths ───────────────────────────────────────────────────────────

test('level maths: totalXpForLevel and levelFromXp are inverses (1..900)', () => {
  for (let L = 1; L <= 900; L++) {
    const need = totalXpForLevel(L);
    if (need > XP_MAX * 2) break; // beyond what a 32-bit column can hold
    assert.equal(levelFromXp(need), L, `level ${L}`);
    assert.equal(levelFromXp(need - 1), L - 1, `level ${L} minus 1 xp`);
  }
  assert.equal(totalXpForLevel(1), 100);
  assert.equal(totalXpForLevel(2), 255);
  assert.equal(totalXpForLevel(5), 1150);
  assert.equal(totalXpForLevel(10), 4675);
  assert.equal(totalXpForLevel(100), 1_899_250);
  assert.equal(totalXpForLevel(0), 0);
  assert.equal(totalXpForLevel(-3), 0);
  assert.equal(levelFromXp(1e6), 79);
  assert.equal(levelFromXp(1e9), 838);
  assert.equal(levelFromXp(-5), 0);
  assert.equal(levelFromXp(Number.NaN), 0);
  assert.equal(xpIntoLevel(1150), 0);
  assert.equal(xpIntoLevel(1200), 50);
  assert.equal(xpIntoLevel(-1), 0);
  assert.ok(totalXpForLevel(839) > XP_MAX, 'level 839 is unreachable');
});

// ── 2. rollXp ────────────────────────────────────────────────────────────────

test('rollXp: range, swap, multiplier, boost and clamps', () => {
  assert.equal(rollXp({ min: 15, max: 25 }, () => 0), 15);
  assert.equal(rollXp({ min: 15, max: 25 }, () => 0.99999), 25);
  assert.equal(rollXp({ min: 25, max: 15 }, () => 0), 15, 'swapped');
  assert.equal(rollXp({ min: 15, max: 25, multiplier: 1.5, boost: 2 }, () => 0), 45);
  assert.equal(rollXp({ min: 15, max: 25, multiplier: 0 }, () => 0), 15, 'multiplier 0 counts as 1');
  assert.equal(rollXp({ min: 15, max: 25, boost: 0.5 }, () => 0), 15, 'boost below 1 counts as 1');
  assert.equal(rollXp({ min: 0, max: 0 }, () => 0), 1, 'min 0 clamps to 1');
  assert.equal(rollXp({ min: 5, max: 5000 }, () => 0.99999), 1000, 'max clamps to 1000');
  assert.deepEqual(normalizeXpRange(undefined, 'x'), { min: 15, max: 25 });
  assert.deepEqual(normalizeXpRange(30.9, 10), { min: 10, max: 30 });
  for (let i = 0; i < 200; i++) {
    const v = rollXp({ min: 15, max: 25 });
    assert.ok(v >= 15 && v <= 25);
  }
});

// ── 3. Boost roles ───────────────────────────────────────────────────────────

test('normalizeBoostRoles and bestBoostMultiplier', () => {
  assert.deepEqual(normalizeBoostRoles(null), []);
  assert.deepEqual(normalizeBoostRoles({ roleId: R(1), multiplier: 2 }), []);
  const out = normalizeBoostRoles([
    { roleId: R(1), multiplier: 1.5 },
    { roleId: 'abc', multiplier: 2 }, // invalid id
    { roleId: R(2), multiplier: 9 }, // out of range
    { roleId: R(3), multiplier: Number.NaN },
    { roleId: R(4), multiplier: 1.234 },
    { roleId: R(1), multiplier: 3 }, // the last duplicate wins
    null,
    'x',
  ]);
  assert.deepEqual(out, [{ roleId: R(1), multiplier: 3 }, { roleId: R(4), multiplier: 1.23 }]);

  const many = Array.from({ length: LIMITS.XP_BOOST_ROLES + 5 }, (_, i) => ({ roleId: R(100 + i), multiplier: 2 }));
  assert.equal(normalizeBoostRoles(many).length, LIMITS.XP_BOOST_ROLES);

  const held = new Set([R(1), R(4)]);
  assert.equal(bestBoostMultiplier(out, (id) => held.has(id)), 3);
  assert.equal(bestBoostMultiplier(out, (id) => id === R(4)), 1.23);
  assert.equal(bestBoostMultiplier(out, () => false), 1);
  assert.equal(bestBoostMultiplier([], () => true), 1);

  // Settings rows are deep-frozen: the readers must never mutate them.
  const frozen = Object.freeze([Object.freeze({ roleId: R(5), multiplier: 2.5 }), Object.freeze({ roleId: R(5), multiplier: 1.5 })]);
  assert.deepEqual(normalizeBoostRoles(frozen), [{ roleId: R(5), multiplier: 1.5 }]);
});

test('hasNoXpRole', () => {
  assert.equal(hasNoXpRole([R(1), R(2)], (id) => id === R(2)), true);
  assert.equal(hasNoXpRole([R(1)], () => false), false);
  assert.equal(hasNoXpRole([], () => true), false);
  assert.equal(hasNoXpRole(undefined, () => true), false);
});

// ── 4. isNoXpChannel ─────────────────────────────────────────────────────────

test('isNoXpChannel: id, parent, thread parent category (R189)', () => {
  assert.equal(isNoXpChannel(['X'], { id: 'X' }), true);
  const thread = { id: 'T', parentId: 'C', isThread: () => true, parent: { parentId: 'CAT' } };
  assert.equal(isNoXpChannel(['C'], thread), true);
  assert.equal(isNoXpChannel(['CAT'], thread), true);
  assert.equal(isNoXpChannel(['T'], thread), true);
  assert.equal(isNoXpChannel(['CAT'], { id: 'X', parentId: 'CAT' }), true);
  assert.equal(isNoXpChannel(['OTHER'], { id: 'X', parentId: 'CAT' }), false);
  assert.equal(isNoXpChannel(['CAT'], { id: 'X', parentId: null, isThread: () => false }), false);
  assert.equal(isNoXpChannel(['X'], null), false);
  assert.equal(isNoXpChannel([], { id: 'X' }), false);
  assert.equal(isNoXpChannel(undefined, { id: 'X' }), false);
});

// ── 5. voiceEligibility ──────────────────────────────────────────────────────

test('voiceEligibility: every reason, in order', () => {
  const base = { channelId: 'V', parentId: 'CAT', afkChannelId: 'AFK', noXpChannelIds: [], noXpRole: false, otherListeners: 1 };
  assert.deepEqual(voiceEligibility(base), { eligible: true, reason: null });
  assert.equal(voiceEligibility({ ...base, channelId: null }).reason, 'no-channel');
  assert.equal(voiceEligibility({ ...base, channelId: 'AFK' }).reason, 'afk');
  assert.equal(voiceEligibility({ ...base, noXpChannelIds: ['V'] }).reason, 'no-xp-channel');
  assert.equal(voiceEligibility({ ...base, noXpChannelIds: ['CAT'] }).reason, 'no-xp-channel', 'via the category');
  assert.equal(voiceEligibility({ ...base, noXpRole: true }).reason, 'no-xp-role');
  assert.equal(voiceEligibility({ ...base, selfDeaf: true }).reason, 'deafened');
  assert.equal(voiceEligibility({ ...base, serverDeaf: true }).reason, 'deafened');
  assert.equal(voiceEligibility({ ...base, otherListeners: 0 }).reason, 'alone');
  // order: afk beats no-xp-channel beats role beats deafened beats alone
  assert.equal(voiceEligibility({ ...base, channelId: 'AFK', noXpChannelIds: ['AFK'], noXpRole: true, selfDeaf: true, otherListeners: 0 }).reason, 'afk');
  assert.equal(voiceEligibility({ ...base, noXpRole: true, selfDeaf: true, otherListeners: 0 }).reason, 'no-xp-role');
  assert.equal(voiceEligibility({ ...base, selfDeaf: true, otherListeners: 0 }).reason, 'deafened');
  // Self-mute does not matter (there is no such input), and one listener is enough.
  assert.equal(voiceEligibility({ ...base, otherListeners: 1 }).eligible, true);
});

// ── 6. countOtherListeners ───────────────────────────────────────────────────

test('countOtherListeners excludes bots, self and deafened members', () => {
  const m = (id, { bot = false, selfDeaf = false, serverDeaf = false } = {}) => ({ id, user: { bot }, voice: { selfDeaf, serverDeaf } });
  const members = new Map([
    ['me', m('me')], ['bot', m('bot', { bot: true })], ['deaf', m('deaf', { selfDeaf: true })],
    ['sdeaf', m('sdeaf', { serverDeaf: true })], ['a', m('a')], ['b', m('b')],
  ]);
  assert.equal(countOtherListeners({ members }, 'me'), 2);
  assert.equal(countOtherListeners({ members: [m('x'), m('y', { bot: true })] }, 'me'), 1, 'arrays work too');
  assert.equal(countOtherListeners(null, 'me'), 0);
  assert.equal(countOtherListeners({}, 'me'), 0);
});

// ── 7. parseVoiceSession ─────────────────────────────────────────────────────

test('parseVoiceSession accepts new and legacy formats', () => {
  const now = 1_700_000_100_000;
  assert.deepEqual(parseVoiceSession(1_700_000_000_000, now), { start: 1_700_000_000_000, channelId: null });
  assert.deepEqual(parseVoiceSession('1700000000000', now), { start: 1_700_000_000_000, channelId: null });
  assert.deepEqual(parseVoiceSession({ start: 1_700_000_000_000, channelId: 'V' }, now), { start: 1_700_000_000_000, channelId: 'V' });
  assert.equal(parseVoiceSession('abc', now), null);
  assert.equal(parseVoiceSession(0, now), null);
  assert.equal(parseVoiceSession(now + 61_000, now), null, 'the future beyond 60 s');
  assert.equal(parseVoiceSession(null, now), null);
  assert.equal(parseVoiceSession({ start: 'x' }, now), null);
  assert.deepEqual(parseVoiceSession(now + 30_000, now), { start: now + 30_000, channelId: null });
});

// ── 8. computeVoiceSegment ───────────────────────────────────────────────────

test('computeVoiceSegment: minutes, remainder, readyAt floor, 24 h cap', () => {
  const now = 1_700_000_000_000;
  const start = now - 125_000;
  assert.deepEqual(computeVoiceSegment({ start, now }), { minutes: 2, nextStart: start + 120_000 });
  assert.deepEqual(computeVoiceSegment({ start, now, readyAt: now - 61_000 }), { minutes: 1, nextStart: now - 61_000 + 60_000 });
  assert.deepEqual(computeVoiceSegment({ start: now - 30 * 3_600_000, now }), { minutes: 1440, nextStart: now });
  assert.equal(computeVoiceSegment({ start: now + 5000, now }).minutes, 0);
  assert.deepEqual(computeVoiceSegment({ start: now - 30_000, now }), { minutes: 0, nextStart: now - 30_000 });
});

// ── 9. voiceXpForMinutes ─────────────────────────────────────────────────────

test('voiceXpForMinutes', () => {
  assert.equal(voiceXpForMinutes(0, { rate: 5 }), 0);
  assert.equal(voiceXpForMinutes(10, { rate: 5 }), 50);
  assert.equal(voiceXpForMinutes(2, { rate: 500 }), 100, 'rate clamps to 50');
  assert.equal(voiceXpForMinutes(10, { rate: 5, multiplier: 2, boost: 1.5 }), 150);
  assert.equal(voiceXpForMinutes(1, { rate: 1, multiplier: 0.1 }), 1, 'at least 1');
});

// ── 10. planLevelRoles ───────────────────────────────────────────────────────

test('planLevelRoles', () => {
  const rewards = [{ level: 1, roleId: 'A' }, { level: 5, roleId: 'B' }, { level: 10, roleId: 'C' }];
  const plan = (o) => planLevelRoles({ rewards, ...o });
  assert.deepEqual(plan({ level: 7, stack: true, syncDown: false, memberRoleIds: [] }), { add: ['A', 'B'], remove: [] });
  assert.deepEqual(plan({ level: 7, stack: false, syncDown: false, memberRoleIds: ['A'] }), { add: ['B'], remove: ['A'] });
  assert.deepEqual(plan({ level: 0, stack: false, syncDown: false, memberRoleIds: ['A'] }), { add: [], remove: [] });
  assert.deepEqual(plan({ level: 3, stack: true, syncDown: true, memberRoleIds: ['A', 'C'] }), { add: [], remove: ['C'] });
  assert.deepEqual(plan({ level: 12, stack: false, syncDown: false, memberRoleIds: ['A', 'B', 'C'] }), { add: [], remove: ['A', 'B'] });
  assert.deepEqual(plan({ level: 0, stack: false, syncDown: true, memberRoleIds: ['A'] }), { add: [], remove: ['A'] });
  // A Set and a Map (the discord.js role cache) both work as the member's roles.
  assert.deepEqual(plan({ level: 7, stack: true, syncDown: false, memberRoleIds: new Set(['A', 'B']) }), { add: [], remove: [] });
  assert.deepEqual(plan({ level: 7, stack: true, syncDown: false, memberRoleIds: new Map([['A', 1]]) }), { add: ['B'], remove: [] });
  // A role listed at two levels counts as earned when either qualifies.
  const dup = [{ level: 1, roleId: 'A' }, { level: 10, roleId: 'A' }, { level: 5, roleId: 'B' }];
  assert.deepEqual(planLevelRoles({ rewards: dup, level: 3, stack: true, syncDown: true, memberRoleIds: ['A', 'B'] }), { add: [], remove: ['B'] });
  // Never overlapping, whatever the input.
  for (const level of [0, 1, 4, 5, 9, 10, 50]) {
    for (const stack of [true, false]) {
      for (const syncDown of [true, false]) {
        for (const held of [[], ['A'], ['B', 'C'], ['A', 'B', 'C']]) {
          const p = plan({ level, stack, syncDown, memberRoleIds: held });
          assert.equal(p.add.some((x) => p.remove.includes(x)), false);
        }
      }
    }
  }
});

// ── 11. resolveAnnounceTarget ────────────────────────────────────────────────

test('resolveAnnounceTarget: the whole matrix', () => {
  const t = (mode, source, cfg = null, cur = null) => resolveAnnounceTarget({ mode, source, levelUpChannelId: cfg, sourceChannelId: cur });
  for (const source of ['message', 'voice', 'reward']) {
    assert.deepEqual(t('OFF', source, 'C', 'S'), { kind: 'none' });
    assert.deepEqual(t('CURRENT', source, 'C', 'S'), { kind: 'channel', channelId: 'S', fallback: false });
    assert.deepEqual(t('CURRENT', source, 'C', null), { kind: 'none' });
    assert.deepEqual(t('DM', source, 'C', 'S'), { kind: 'dm' });
    assert.deepEqual(t('DM', source, null, null), { kind: 'dm' });
    assert.deepEqual(t('CHANNEL', source, 'C', 'S'), { kind: 'channel', channelId: 'C', fallback: false });
    assert.deepEqual(t('CHANNEL', source, null, 'S'), { kind: 'channel', channelId: 'S', fallback: true });
    assert.deepEqual(t('CHANNEL', source, null, null), { kind: 'none' });
  }
  for (const mode of ANNOUNCE_MODES) assert.deepEqual(t(mode, 'admin', 'C', 'S'), { kind: 'none' }, `admin never announces (${mode})`);
  assert.deepEqual(t('WHATEVER', 'message', 'C', 'S'), { kind: 'channel', channelId: 'S', fallback: false }, 'unknown mode = CURRENT');
  assert.deepEqual(t(undefined, 'message', 'C', 'S'), { kind: 'channel', channelId: 'S', fallback: false });
});

// ── 12. levelUpVars / buildLevelUpPayload ────────────────────────────────────

const PAYLOAD_GUILD = { id: G(1), name: 'Srv', iconURL: () => 'https://cdn.example.test/i.png', ownerId: '1', memberCount: 12 };
const PAYLOAD_USER = fakeUser(U(1), { username: 'alice' });
const PAYLOAD_MEMBER = { id: U(1), user: PAYLOAD_USER, displayName: 'Alice', guild: PAYLOAD_GUILD, displayAvatarURL: () => AVATAR, joinedTimestamp: 1_650_000_000_000 };
const payloadArgs = (over = {}) => ({
  template: 'GG {player}, level {level}!', useEmbed: false, member: PAYLOAD_MEMBER, user: PAYLOAD_USER, guild: PAYLOAD_GUILD,
  channel: null, level: 7, xp: 1234, rank: 3, embedColor: null, dm: false, ...over,
});

test('levelUpVars keys are lower-case and values are strings', () => {
  const vars = levelUpVars({ user: PAYLOAD_USER, member: PAYLOAD_MEMBER, level: 7, xp: 1234, rank: null });
  assert.deepEqual(vars, {
    player: `<@${U(1)}>`, 'player.name': 'Alice', level: '7', rank: '?', xp: '1,234', mention: `<@${U(1)}>`,
  });
  assert.equal(levelUpVars({ user: { ...PAYLOAD_USER, globalName: 'Glob' }, member: null, level: 1, xp: 1, rank: 2 })['player.name'], 'Glob');
  assert.equal(levelUpVars({ user: PAYLOAD_USER, member: null, level: 1, xp: 1, rank: 2 })['player.name'], 'alice');
});

test('buildLevelUpPayload: text mode replaces every token once and keeps $& literal (R190, R298)', () => {
  const p = buildLevelUpPayload(payloadArgs({ template: 'GG {player}! {player} hit {level} in {server} $& {player.name} {PLAYER}' }));
  assert.equal(p.content, `GG <@${U(1)}>! <@${U(1)}> hit 7 in Srv $& Alice <@${U(1)}>`);
  assert.deepEqual(p.allowedMentions, { users: [U(1)] });
  assert.equal(p.components, undefined);
  assert.equal(p.embeds, undefined);
  assert.equal(buildLevelUpPayload(payloadArgs({ template: DEFAULT_LEVELUP_MESSAGE })).content, `GG <@${U(1)}>, you just advanced to level 7!`);
  // The old {user} (mention now) and {mention} alias, and rank.
  assert.equal(buildLevelUpPayload(payloadArgs({ template: '{user} {mention} #{rank} {xp}' })).content, `<@${U(1)}> <@${U(1)}> #3 1,234`);
  assert.equal(buildLevelUpPayload(payloadArgs({ template: '{user.name}' })).content, 'alice');
});

test('buildLevelUpPayload: embed mode puts the mention in content', () => {
  const p = buildLevelUpPayload(payloadArgs({ template: 'GG {player}, level {level}!', useEmbed: true, embedColor: 0x123456 }));
  assert.equal(p.content, `<@${U(1)}>`);
  assert.ok(p.embeds.length === 1);
  const data = p.embeds[0].toJSON();
  assert.equal(data.description, `GG <@${U(1)}>, level 7!`);
  assert.ok(!data.description.includes('{level}'));
  assert.equal(data.color, 0x123456);
  assert.equal(data.thumbnail.url, AVATAR);
  assert.deepEqual(p.allowedMentions, { users: [U(1)] });
  const plain = buildLevelUpPayload(payloadArgs({ template: 'Level {level} reached!', useEmbed: true }));
  assert.equal(plain.content, undefined, 'no player token: no content');
  assert.equal(plain.embeds[0].toJSON().color, 0x57f287, 'default colour');
});

test('buildLevelUpPayload: DM adds the disabled "Sent from" button; empty text is null', () => {
  for (const useEmbed of [false, true]) {
    const p = buildLevelUpPayload(payloadArgs({ useEmbed, dm: true }));
    assert.equal(p.components.length, 1);
    const row = p.components[0].toJSON();
    assert.equal(row.components.length, 1);
    assert.equal(row.components[0].custom_id, 'noop:levelup');
    assert.equal(row.components[0].disabled, true);
    assert.equal(row.components[0].style, ButtonStyle.Secondary);
    assert.equal(row.components[0].label, 'Sent from Srv');
    assert.equal(row.type, ComponentType.ActionRow);
  }
  assert.equal(buildLevelUpPayload(payloadArgs({ template: '   ' })), null);
  assert.equal(buildLevelUpPayload(payloadArgs({ template: '   ', useEmbed: true })), null);
  // Over-long output is clamped to the 2000-character message limit.
  assert.ok(buildLevelUpPayload(payloadArgs({ template: 'x'.repeat(5000) })).content.length <= 2000);
});

// ── 13. rankWhere ────────────────────────────────────────────────────────────

test('rankWhere is the single tie rule', () => {
  assert.deepEqual(rankWhere('g', 'u', 50), { guildId: 'g', OR: [{ xp: { gt: 50 } }, { xp: 50, userId: { lt: 'u' } }] });
});

// ── 14. normalizeCardStyle ───────────────────────────────────────────────────

test('normalizeCardStyle fills defaults per key and never mutates', () => {
  assert.deepEqual(card.normalizeCardStyle(null), card.DEFAULT_RANK_CARD_STYLE);
  assert.deepEqual(card.normalizeCardStyle(undefined), card.DEFAULT_RANK_CARD_STYLE);
  assert.deepEqual(card.normalizeCardStyle([1, 2]), card.DEFAULT_RANK_CARD_STYLE);
  assert.deepEqual(
    card.normalizeCardStyle({ background: '#abcdef', overlayOpacity: 2, backgroundUrl: 'ftp://x' }),
    { background: '#ABCDEF', backgroundUrl: null, accentColor: '#5865F2', textColor: '#FFFFFF', overlayOpacity: 0.5 },
  );
  const frozen = Object.freeze({ accentColor: '#ff0000', textColor: 'red', overlayOpacity: 0.9, backgroundUrl: 'https://img.example.test/a.png' });
  assert.deepEqual(card.normalizeCardStyle(frozen), {
    background: '#2B2D31', backgroundUrl: 'https://img.example.test/a.png', accentColor: '#FF0000', textColor: '#FFFFFF', overlayOpacity: 0.9,
  });
  assert.equal(card.normalizeCardStyle({ overlayOpacity: -0.1 }).overlayOpacity, 0.5);
  assert.equal(card.normalizeCardStyle({ overlayOpacity: 0 }).overlayOpacity, 0);
  assert.equal(card.normalizeCardStyle({ background: '#fff' }).background, '#2B2D31', 'short hex is rejected');
});

// ── Rank card rendering ──────────────────────────────────────────────────────

/** A PNG whose header claims `w` x `h` pixels; there is no image data, so a decode attempt would fail differently. */
function crc32(buf) {
  let c;
  let crc = 0xffffffff;
  for (let n = 0; n < buf.length; n++) {
    c = (crc ^ buf[n]) & 0xff;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    crc = (crc >>> 8) ^ c;
  }
  return (crc ^ 0xffffffff) >>> 0;
}
function pngChunk(type, data) {
  const out = Buffer.alloc(12 + data.length);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, 'latin1');
  data.copy(out, 8);
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length);
  return out;
}
function bombPng(w, h) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8; ihdr[9] = 2; // 8-bit RGB
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk('IHDR', ihdr), pngChunk('IDAT', deflateSync(Buffer.alloc(8))), pngChunk('IEND', Buffer.alloc(0)),
  ]);
}

test('generateProfileCard renders a 900x280 PNG with a custom style and compact numbers', async () => {
  const png = await card.generateProfileCard({
    avatarBuffer: null, username: 'alice', displayName: 'Alice', level: 443, totalXp: 999_960, rank: 1234,
    style: { background: '#112233', accentColor: '#ff8800', textColor: '#eeeeee', overlayOpacity: 0.3 },
  });
  assert.equal(png.subarray(1, 4).toString('latin1'), 'PNG');
  assert.equal(png.readUInt32BE(16), 900);
  assert.equal(png.readUInt32BE(20), 280);
  const plain = await card.generateProfileCard({ avatarBuffer: Buffer.from('not an image'), username: 'a', displayName: 'A', level: 0, totalXp: 0, rank: 1 });
  assert.equal(plain.subarray(1, 4).toString('latin1'), 'PNG');
});

test('a crafted 30000x30000 background is refused before it is decoded (D4)', async () => {
  const bomb = bombPng(30_000, 30_000);
  const debug = mock.method(logger, 'debug', () => {});
  const png = await card.generateProfileCard({
    avatarBuffer: null, username: 'a', displayName: 'A', level: 1, totalXp: 100, rank: 1, backgroundBuffer: bomb,
  });
  assert.equal(png.subarray(1, 4).toString('latin1'), 'PNG', 'the card still renders without the background');
  const skipped = debug.mock.calls.map((c) => String(c.arguments[0])).find((m) => m.includes('background skipped'));
  assert.ok(skipped?.includes('IMAGE_TOO_LARGE_OR_UNKNOWN'), `refused by the dimension check, not by a decoder error: ${skipped}`);
  // The same bomb as an avatar is refused too (the placeholder is drawn instead).
  const png2 = await card.generateProfileCard({ avatarBuffer: bomb, username: 'a', displayName: 'A', level: 1, totalXp: 100, rank: 1 });
  assert.equal(png2.subarray(1, 4).toString('latin1'), 'PNG');
});

test('validateCardBackground and loadCardBackground never throw and refuse local addresses', async () => {
  assert.deepEqual(await card.validateCardBackground('http://127.0.0.1:9/x.png'), { ok: false, reason: 'that address is not allowed' });
  assert.deepEqual(await card.validateCardBackground('not a url'), { ok: false, reason: 'that address is not allowed' });
  assert.equal(await card.loadCardBackground('http://127.0.0.1:9/x.png'), null);
  assert.equal(await card.loadCardBackground('http://127.0.0.1:9/x.png'), null, 'negative cache');
  assert.equal(await card.loadCardBackground(null), null);
  card.clearCardBackground('http://127.0.0.1:9/x.png');
  card.clearCardBackground(null);
});

// ── 15. Payload shapes ───────────────────────────────────────────────────────

const charCount = (node) => {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
};

test('command payload shapes', () => {
  const lc = levelconfig.data.toJSON();
  assert.equal(lc.options.length, 14);
  assert.ok(charCount(lc) <= 2600, `levelconfig ${charCount(lc)} characters`);
  const names = lc.options.map((o) => o.name);
  for (const n of ['enable', 'disable', 'status', 'range', 'cooldown', 'multiplier', 'announce', 'message', 'voicexp', 'card', 'demotion-sync']) {
    assert.ok(names.includes(n), `${n} present`);
  }
  const groups = lc.options.filter((o) => o.type === 2).map((o) => o.name);
  assert.deepEqual(groups.sort(), ['boost', 'noxp', 'role']);
  assert.equal(names.includes('rate'), false, '/levelconfig rate was removed');
  assert.equal(lc.options.some((o) => o.name === 'channel' && o.type === 1), false, 'the top-level channel subcommand was removed');
  const announce = lc.options.find((o) => o.name === 'announce');
  assert.deepEqual(announce.options.find((o) => o.name === 'mode').choices.map((c) => c.value), ['OFF', 'CURRENT', 'DM', 'CHANNEL']);
  const roleRemove = lc.options.find((o) => o.name === 'role').options.find((o) => o.name === 'remove');
  assert.equal(roleRemove.options[0].autocomplete, true);

  const xp = xpCmd.data.toJSON();
  assert.ok(charCount(xp) <= 520);
  const give = xp.options.find((o) => o.name === 'give');
  assert.deepEqual(give.options.map((o) => o.type), [9, 4]);
  assert.deepEqual(xp.options.map((o) => o.name), ['give', 'remove', 'set', 'reset', 'view']);
  assert.ok(charCount(rankCmd.data.toJSON()) <= 90);
  assert.ok(charCount(boardCmd.data.toJSON()) <= 90);

  for (const mod of [levelconfig, xpCmd, rankCmd, boardCmd]) {
    assert.equal(mod.module, 'leveling');
    assert.doesNotThrow(() => buildCommandPayload(mod));
  }
  for (const mod of [levelconfig, xpCmd]) {
    assert.equal(mod.gated, false);
    assert.equal(mod.permissionLevel, PERMISSION_LEVELS.ADMIN);
  }
  assert.equal(rankCmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.equal(boardCmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.equal(rankCmd.gated, undefined);
  assert.equal(typeof levelconfig.autocomplete, 'function');
});

// ── 16. Plugin shape ─────────────────────────────────────────────────────────

test('plugin declaration', async () => {
  assert.equal(plugin.id, 'levels');
  assert.equal(plugin.module, 'leveling');
  assert.ok(plugin.hooks.every((h) => h.id === 'levels'));
  const events = plugin.hooks.map((h) => h.event).sort();
  assert.deepEqual(events, ['guildReady', 'messageCreate', 'roleDelete', 'voiceStateUpdate']);
  assert.equal(plugin.hooks.find((h) => h.event === 'roleDelete').always, true);
  assert.equal(typeof plugin.provides.xp.grant, 'function');
  assert.equal(typeof plugin.provides.xp.take, 'function');
  assert.deepEqual(plugin.jobs.map((j) => j.type), ['levels.voiceSweep']);
  assert.deepEqual(plugin.components, []);
  const filter = plugin.hooks.find((h) => h.event === 'messageCreate').filter;
  assert.equal(filter({ member: null }), false);
  assert.equal(filter({ member: {} }), true);
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => /levels/.test(e)), []);
  assert.ok(plugins.some((p) => p.id === 'levels' && p.provides.xp));
});

// ── 17. Deprecated wrappers ──────────────────────────────────────────────────

test('deprecated grantXp and handleVoiceXp are no-ops', async () => {
  assert.equal(await svc.grantXp({}, {}), undefined);
  assert.equal(await svc.handleVoiceXp({}, {}, {}), undefined);
});

// ── Reward roles (fakes) ─────────────────────────────────────────────────────

test('syncLevelRoles: adds before removes, one role per call, failures reported (R060)', async () => {
  const guild = fakeGuild(G(1), { roles: [fakeRole(R(1)), fakeRole(R(2)), fakeRole(R(3), { position: 500 })], botPosition: 50 });
  const member = fakeMember(guild, U(1), { roles: [R(1)] });
  const rewards = [{ level: 1, roleId: R(1) }, { level: 5, roleId: R(2) }, { level: 9, roleId: R(3) }];
  const out = await svc.syncLevelRoles(member, 7, { levelRolesStack: false }, { rewards, reason: 'test' });
  assert.deepEqual(member.calls.map((c) => c.slice(0, 2)), [['add', R(2)], ['remove', R(1)]], 'the add comes first');
  assert.deepEqual(out.added, [R(2)]);
  assert.deepEqual(out.removed, [R(1)]);
  assert.deepEqual(out.failed, []);

  // A role above the bot is reported and never attempted; nothing throws.
  const member2 = fakeMember(guild, U(2));
  const out2 = await svc.syncLevelRoles(member2, 20, { levelRolesStack: true }, { rewards });
  assert.deepEqual(member2.calls.map((c) => c[1]), [R(1), R(2)]);
  assert.equal(out2.failed.length, 1);
  assert.equal(out2.failed[0].roleId, R(3));
  assert.match(out2.failed[0].reason, /above my highest role/);

  // Discord refusing an add is reported, not thrown.
  const member3 = fakeMember(guild, U(3));
  member3.roles.add = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  const out3 = await svc.syncLevelRoles(member3, 1, { levelRolesStack: true }, { rewards });
  assert.equal(out3.added.length, 0);
  assert.equal(out3.failed[0].roleId, R(1));

  // Demotion sync removes unearned reward roles; without it they stay.
  const member4 = fakeMember(guild, U(4), { roles: [R(1), R(2)] });
  assert.deepEqual((await svc.syncLevelRoles(member4, 2, { levelRolesStack: true }, { rewards, syncDown: false })).removed, []);
  assert.deepEqual((await svc.syncLevelRoles(member4, 2, { levelRolesStack: true }, { rewards, syncDown: true })).removed, [R(2)]);
  // No rewards configured: nothing happens.
  assert.deepEqual(await svc.syncLevelRoles(member4, 2, {}, { rewards: [] }), { added: [], removed: [], failed: [] });
});

test('announceLevelUp: routing, permissions and silence on failure', async () => {
  const guild = fakeGuild(G(1));
  const member = fakeMember(guild, U(1), { displayName: 'Alice' });
  const here = fakeChannel(C(1));
  const custom = fakeChannel(C(2));
  guild.channels.cache.set(C(2), custom);
  const base = { guild, member, level: 5, xp: 1150, settings: { levelUpMode: 'CURRENT', levelUpUseEmbed: false, levelUpMessage: null }, source: 'message', sourceChannel: here };

  assert.equal(await svc.announceLevelUp(base), true);
  assert.equal(here.sent.length, 1);
  assert.equal(here.sent[0].content, `GG <@${U(1)}>, you just advanced to level 5!`);

  assert.equal(await svc.announceLevelUp({ ...base, settings: { ...base.settings, levelUpMode: 'OFF' } }), false);
  assert.equal(await svc.announceLevelUp({ ...base, source: 'admin' }), false);
  assert.equal(here.sent.length, 1);

  assert.equal(await svc.announceLevelUp({ ...base, settings: { ...base.settings, levelUpMode: 'CHANNEL', levelUpChannelId: C(2) } }), true);
  assert.equal(custom.sent.length, 1);
  assert.equal(here.sent.length, 1);

  // A configured channel that is gone stays silent (no fallback to the current channel).
  assert.equal(await svc.announceLevelUp({ ...base, settings: { ...base.settings, levelUpMode: 'CHANNEL', levelUpChannelId: C(9) } }), false);
  assert.equal(here.sent.length, 1);

  // DM mode: closed DMs (50007) are silent.
  assert.equal(await svc.announceLevelUp({ ...base, settings: { ...base.settings, levelUpMode: 'DM' } }), true);
  assert.equal(member.sent.length, 1);
  assert.equal(member.sent[0].components[0].toJSON().components[0].custom_id, 'noop:levelup');
  member.send = async () => { throw Object.assign(new Error('Cannot send messages to this user'), { code: 50007 }); };
  assert.equal(await svc.announceLevelUp({ ...base, settings: { ...base.settings, levelUpMode: 'DM' } }), false);

  // A channel the bot cannot post in stays silent; embed mode needs Embed Links.
  const locked = fakeChannel(C(3), { canSend: false });
  assert.equal(await svc.announceLevelUp({ ...base, sourceChannel: locked }), false);
  assert.equal(locked.sent.length, 0);
  // A custom message with every variable and the embed format.
  const embedSettings = { ...base.settings, levelUpUseEmbed: true, levelUpMessage: '{player.name} has {xp} XP' };
  assert.equal(await svc.announceLevelUp({ ...base, settings: embedSettings }), true);
  assert.equal(here.sent.at(-1).embeds[0].toJSON().description, 'Alice has 1,150 XP');
});

test('reconcileVoiceMember: start, discard on a fresh join, never credit while offline', async () => {
  const store = installFakeCache();
  const guild = fakeGuild(G(1), { readyTimestamp: 0 });
  const member = fakeMember(guild, U(1));
  const buddy = fakeMember(guild, U(2), { voice: { selfDeaf: false, serverDeaf: false } });
  const members = new Map([[U(1), member], [U(2), buddy]]);
  const channel = fakeChannel(C(1), { type: ChannelType.GuildVoice, members });
  const key = `voice:start:${G(1)}:${U(1)}`;
  const settings = { voiceXpEnabled: true, voiceXpRate: 5, xpMultiplier: 1, noXpChannelIds: [], noXpRoleIds: [], xpBoostRoles: null };
  const t0 = 1_700_000_000_000;

  // Eligible with no session: a session starts.
  assert.equal(await svc.reconcileVoiceMember({ guild, member, state: { channel, selfDeaf: false, serverDeaf: false }, settings, now: t0, freshJoin: true }), true);
  assert.deepEqual(JSON.parse(store.get(key)), { start: t0, channelId: C(1) });

  // A fresh join over a stored session discards it, and an ineligible fresh join leaves no key behind.
  const alone = fakeChannel(C(2), { type: ChannelType.GuildVoice, members: new Map([[U(1), member]]) });
  store.set(key, JSON.stringify({ start: t0 - 3 * 3_600_000, channelId: C(1) }));
  assert.equal(await svc.reconcileVoiceMember({ guild, member, state: { channel: alone, selfDeaf: false, serverDeaf: false }, settings, now: t0, freshJoin: true }), false);
  assert.equal(store.has(key), false, 'the stale session is deleted, not paid out later');

  // Ineligible without a session: nothing to do, nothing written.
  assert.equal(await svc.reconcileVoiceMember({ guild, member, state: null, settings, now: t0 }), false);
  assert.equal(store.has(key), false);

  // No credit is attempted for under a minute (and so no database is needed here).
  store.set(key, JSON.stringify({ start: t0 - 30_000, channelId: C(1) }));
  assert.equal(await svc.reconcileVoiceMember({ guild, member, state: { channel: alone }, settings, now: t0 }), false);
  assert.equal(store.has(key), false);
});

test('handleVoiceStateXp: voice XP off drops the member key without credit', async () => {
  const store = installFakeCache();
  const guild = fakeGuild(G(1));
  const member = fakeMember(guild, U(1));
  const key = `voice:start:${G(1)}:${U(1)}`;
  store.set(key, JSON.stringify({ start: Date.now() - 600_000, channelId: C(1) }));
  await svc.handleVoiceStateXp({ guild, member, oldState: {}, newState: {}, change: 'state', getSettings: async () => ({ voiceXpEnabled: false }) });
  assert.equal(store.has(key), false);
});

test('voiceChangeMatters: mute, camera and stream toggles skip the voice hook; deafen changes do not', () => {
  const { voiceChangeMatters } = svc;
  for (const change of ['join', 'leave', 'move']) assert.equal(voiceChangeMatters({ change, oldState: {}, newState: {} }), true, change);
  const state = (oldState, newState) => voiceChangeMatters({ change: 'state', oldState, newState });
  assert.equal(state({ selfDeaf: false, selfMute: false }, { selfDeaf: false, selfMute: true }), false, 'a mute');
  assert.equal(state({ selfVideo: false }, { selfVideo: true }), false, 'a camera');
  assert.equal(state({ selfDeaf: false }, { selfDeaf: true }), true, 'a self-deafen');
  assert.equal(state({ serverDeaf: true }, { serverDeaf: false }), true, 'a server undeafen');
  assert.equal(state({ selfDeaf: true, serverDeaf: false }, { selfDeaf: true, serverDeaf: true }), false, 'still deafened');
  assert.equal(state(null, null), false);
  const hook = plugin.hooks.find((h) => h.event === 'voiceStateUpdate');
  assert.equal(hook.filter, voiceChangeMatters, 'the hook gates on it before any settings or Redis work');
});

test('handleVoiceStateXp: other members are re-checked only when a channel has one or two listeners', async () => {
  const store = installFakeCache();
  const settings = { voiceXpEnabled: true, voiceXpRate: 5, xpMultiplier: 1, noXpChannelIds: [], noXpRoleIds: [], xpBoostRoles: null };
  const key = (g, u) => `voice:start:${g}:${u}`;
  const ticks = async (n = 5) => { for (let i = 0; i < n; i++) await new Promise((r) => { setImmediate(r); }); };
  const setup = (guildId, userIds) => {
    const guild = fakeGuild(guildId);
    const members = new Map();
    const channel = fakeChannel(C(30), { type: ChannelType.GuildVoice, members });
    guild.channels.cache.set(channel.id, channel);
    const list = userIds.map((id) => {
      const m = fakeMember(guild, id, { voice: { channel, channelId: channel.id, selfDeaf: false, serverDeaf: false } });
      members.set(id, m);
      return m;
    });
    return { guild, list };
  };
  // The mover arrives from a channel this guild does not cache, so only the destination is re-checked.
  const move = (guild, member) => svc.handleVoiceStateXp({
    guild, member, oldState: { channelId: C(39) }, newState: member.voice, change: 'move', getSettings: async () => settings,
  });

  // Two listeners now: U(1) was alone before U(2) arrived, so the detached re-check starts U(1)'s session.
  let { guild, list: [a, b] } = setup(G(3), [U(1), U(2)]);
  await move(guild, b);
  assert.ok(store.has(key(G(3), U(2))), "the mover's own session starts before the hook returns");
  await ticks();
  assert.ok(store.has(key(G(3), U(1))), 'the member who was alone is re-checked');
  assert.equal(a.id, U(1));

  // Three listeners: nobody else's status can have changed, so nobody else is touched (U(1) has no session here only
  // to show that the re-check was skipped).
  ({ guild, list: [a, , b] } = setup(G(4), [U(1), U(3), U(2)]));
  await move(guild, b);
  await ticks();
  assert.ok(store.has(key(G(4), U(2))));
  assert.equal(store.has(key(G(4), U(1))), false, 'a busy channel is not re-scanned');
  assert.equal(store.has(key(G(4), U(3))), false);
});

test('handleMessageXp: exemptions first, Redis down fails closed', async () => {
  const guild = fakeGuild(G(1));
  const member = fakeMember(guild, U(1), { roles: [R(1)] });
  const ctxFor = (settings, channel) => ({ guild, member, message: { channel }, getSettings: async () => settings });
  const calls = [];
  mock.method(cache, 'setNX', async (k, v, ttl) => { calls.push([k, v, ttl]); return null; });
  const base = { xpMin: 15, xpMax: 25, xpCooldown: 60, xpMultiplier: 1, noXpChannelIds: [], noXpRoleIds: [], xpBoostRoles: null };

  // Exempt channel / role: the cooldown is never consumed.
  await svc.handleMessageXp(ctxFor({ ...base, noXpChannelIds: [C(1)] }, { id: C(1) }));
  await svc.handleMessageXp(ctxFor({ ...base, noXpChannelIds: [C(9)] }, { id: C(1), parentId: C(9) }));
  await svc.handleMessageXp(ctxFor({ ...base, noXpRoleIds: [R(1)] }, { id: C(1) }));
  await svc.handleMessageXp(ctxFor(null, { id: C(1) }));
  assert.equal(calls.length, 0);

  // Redis unavailable (null): fail closed. A cooldown hit (false) also awards nothing. Neither reaches the database.
  await svc.handleMessageXp(ctxFor(base, { id: C(1) }));
  assert.deepEqual(calls, [[`xp:cooldown:${G(1)}:${U(1)}`, 1, 60]]);
  cache.setNX.mock.mockImplementation(async () => false);
  await svc.handleMessageXp(ctxFor({ ...base, xpCooldown: 0 }, { id: C(1) }));
  assert.equal(cache.setNX.mock.calls.at(-1).arguments[2], 1, 'the TTL is at least one second');
});

// ── Static scan (acceptance checklist) ───────────────────────────────────────

test('owned files obey the static rules', () => {
  const files = [
    'src/services/xpService.js', 'src/plugins/levels.js', 'src/commands/admin/levelconfig.js', 'src/commands/leveling/xp.js',
    'src/commands/leveling/rank.js', 'src/commands/leveling/leaderboard.js', 'src/utils/profileCard.js',
  ];
  for (const f of files) {
    const text = readFileSync(resolve(ROOT, f), 'utf8');
    assert.equal(/cache\.del\(\s*['"`]guild:/.test(text), false, `${f}: no cache.del of a settings key`);
    assert.equal(/\bconsole\./.test(text), false, `${f}: no console`);
    assert.equal(/prisma\.guildSettings/.test(text), false, `${f}: settings only through settingsService`);
    assert.equal(/roles\.(add|remove)\(\s*\[/.test(text), false, `${f}: no array-form role edits`);
    assert.equal(/LIMITS\.[A-Z_]+\s*\?\?/.test(text), false, `${f}: no LIMITS fallbacks`);
    assert.equal(/\bxpRate\b|\blevelingEnabled\b/.test(text), false, `${f}: deprecated fields are never read or written`);
  }
  const xpText = readFileSync(resolve(ROOT, 'src/services/xpService.js'), 'utf8');
  assert.equal(/replace\(\s*'\{/.test(xpText), false, 'no string-pattern replace of a token (R190)');
  assert.equal(/\bnow\(\)/i.test(xpText.replace(/Date\.now\(\)/g, '')), false, 'no bare now() in raw SQL');
  const lcText = readFileSync(resolve(ROOT, 'src/commands/admin/levelconfig.js'), 'utf8');
  // R002: the group dispatch must come before the plain subcommand switch.
  assert.ok(lcText.indexOf("group === 'noxp'") < lcText.indexOf("case 'enable'"), 'dispatch is group-first');
  assert.equal(/getSubcommand\(\)\s*===\s*'channel'/.test(lcText), false);
  assert.ok(Array.isArray(svc.RAW_SQL) && svc.RAW_SQL.every((r) => typeof r.name === 'string' && typeof r.build === 'function'));
  for (const { build } of svc.RAW_SQL) assert.ok(build() instanceof Prisma.sql``.constructor);
});

// ═════════════════════════════════════════════════════════════════════════════
// DB cases (TEST_DATABASE_URL, a migrated database)
// ═════════════════════════════════════════════════════════════════════════════

test('DB: every RAW_SQL statement executes against the migrated schema', { skip: skipDb }, async () => {
  const guildId = '0';
  for (const { name, build } of svc.RAW_SQL) {
    try {
      await prisma.$transaction(async (tx) => {
        await tx.guildSettings.upsert({ where: { guildId }, update: {}, create: { guildId } });
        await tx.$queryRaw(build());
        throw new Error('rollback');
      });
    } catch (err) {
      if (err.message !== 'rollback') assert.fail(`${name}: ${err.message}`);
    }
  }
});

test('DB: atomic XP statements (R299, R300)', { skip: skipDb }, async () => {
  const guildId = G(1);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  const a = await svc.applyXpDelta({ guildId, userId: U(1), delta: 50, messages: 1, touchLastXp: true });
  assert.deepEqual(a, { xp: 50, oldLevel: 0, newLevel: 0, created: true });
  const b = await svc.applyXpDelta({ guildId, userId: U(1), delta: 60, messages: 1, touchLastXp: true });
  assert.deepEqual(b, { xp: 110, oldLevel: 0, newLevel: 1, created: false });
  for (const v of Object.values(b)) assert.notEqual(typeof v, 'bigint');
  let row = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(1) } } });
  assert.equal(row.messageCount, 2);
  assert.ok(row.lastXpAt instanceof Date);
  assert.equal(row.level, 0, 'applyXpDelta never touches the stored level');

  // setStoredLevel only writes while the XP is still what the caller saw (one winner per transition).
  assert.equal(await svc.setStoredLevel(guildId, U(1), 999, 1), false, 'stale XP');
  assert.equal(await svc.setStoredLevel(guildId, U(1), 110, 1), true);
  assert.equal(await svc.setStoredLevel(guildId, U(1), 110, 1), false, 'already stored');

  // Voice credit leaves the message counter alone and does not need a lastXpAt.
  await svc.applyXpDelta({ guildId, userId: U(1), delta: 5, voiceMinutes: 3 });
  row = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(1) } } });
  assert.equal(row.voiceMinutes, 3);
  assert.equal(row.messageCount, 2);

  // subtractXp: floors at 0, reports the old XP, never inserts (R300).
  assert.deepEqual(await svc.subtractXp({ guildId, userId: U(1), amount: 30 }), { xp: 85, oldXp: 115, oldLevel: 1, newLevel: 0 });
  assert.equal((await svc.subtractXp({ guildId, userId: U(1), amount: 1_000_000 })).xp, 0);
  assert.equal(await svc.subtractXp({ guildId, userId: U(99), amount: 5 }), null);
  assert.equal(await prisma.userLevel.count({ where: { guildId, userId: U(99) } }), 0, 'no row was created');

  // setXp: 0 never creates a row; otherwise insert or overwrite; clamped to XP_MAX.
  assert.equal(await svc.setXp({ guildId, userId: U(98), xp: 0 }), null);
  assert.equal(await prisma.userLevel.count({ where: { guildId, userId: U(98) } }), 0);
  assert.deepEqual(await svc.setXp({ guildId, userId: U(98), xp: 5000 }), { xp: 5000, oldLevel: 0, newLevel: 10, created: true });
  assert.equal((await svc.setXp({ guildId, userId: U(98), xp: 6000 })).created, false);
  assert.equal((await svc.setXp({ guildId, userId: U(98), xp: 5_000_000_000 })).xp, XP_MAX);
  assert.equal((await svc.setXp({ guildId, userId: U(98), xp: 0 })).xp, 0);
  assert.deepEqual(await svc.resetXp(guildId, U(98)), { oldLevel: 0 });
  assert.equal(await svc.resetXp(guildId, U(98)), null);

  // The clamp holds in SQL (no int overflow).
  const big = await svc.applyXpDelta({ guildId, userId: U(97), delta: 2_000_000_000 });
  assert.equal(big.xp, XP_MAX);
  assert.equal((await svc.applyXpDelta({ guildId, userId: U(97), delta: 5 })).xp, XP_MAX);
});

test('DB: giveXpToMembers (bulk) and the shared rank tie rule (R092, R243)', { skip: skipDb }, async () => {
  const guildId = G(2);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  await svc.applyXpDelta({ guildId, userId: U(1), delta: 80 });
  const res = await svc.giveXpToMembers({ guildId, userIds: [U(1), U(2), U(3), U(2)], amount: 1000 });
  assert.equal(res.updated, 3, 'duplicates collapse');
  const levelUps = Object.fromEntries(res.levelUps.map((l) => [l.userId, l]));
  assert.deepEqual(levelUps[U(1)], { userId: U(1), oldLevel: 0, newLevel: 4, xp: 1080 });
  assert.equal(levelUps[U(2)].newLevel, 4);
  const rows = await prisma.userLevel.findMany({ where: { guildId } });
  assert.ok(rows.every((r) => r.level === levelFromXp(r.xp)), 'stored levels match the XP');
  // A second give over the cap-free range reports no new level-ups for the same level.
  assert.equal((await svc.giveXpToMembers({ guildId, userIds: [U(2)], amount: 5 })).levelUps.length, 0);

  // Ties: xp equal -> the smaller userId ranks first, and /rank agrees with the leaderboard ordering.
  const board = await prisma.userLevel.findMany({ where: { guildId, xp: { gt: 0 } }, orderBy: [{ xp: 'desc' }, { userId: 'asc' }] });
  for (let i = 0; i < board.length; i++) assert.equal(await svc.getRank(guildId, board[i].userId, board[i].xp), i + 1);
  assert.deepEqual(await svc.giveXpToMembers({ guildId, userIds: [], amount: 5 }), { updated: 0, levelUps: [] });
});

test('DB: concurrent XP writers never lose XP and one writer owns each transition', { skip: skipDb }, async () => {
  const guildId = G(3);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  const guild = fakeGuild(G(3));
  const member = fakeMember(guild, U(1));
  const ch = fakeChannel(C(1));
  const settings = { ...(await settingsSvc.getSettings(guildId)), xpEnabled: true, levelUpMode: 'CURRENT' };
  const emitted = [];
  registerHook({ event: 'levelUp', id: 'achievements', run: (ctx) => emitted.push([ctx.oldLevel, ctx.newLevel]) }, { pluginId: 'test' });

  const N = 30;
  const results = await Promise.all(Array.from({ length: N }, async () => {
    const r = await svc.applyXpDelta({ guildId, userId: U(1), delta: 40, messages: 1 });
    return svc.handleLevelChange({ guild, member, userId: U(1), result: r, settings, source: 'message', channel: ch });
  }));
  const row = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(1) } } });
  assert.equal(row.xp, N * 40, 'no lost update');
  assert.equal(row.messageCount, N);
  assert.equal(row.level, levelFromXp(row.xp), 'the stored level matches the final XP');
  // Every announcement belongs to a distinct winning transition and the final level was announced at the end.
  const wins = results.filter((r) => r.leveledUp).length;
  assert.equal(ch.sent.length, wins);
  assert.equal(emitted.length, wins);
  assert.ok(wins >= 1 && wins <= levelFromXp(N * 40));
  assert.equal(Math.max(...emitted.map((e) => e[1])), levelFromXp(N * 40), 'the final level was announced (event order is not XP order)');
});

test('DB: level-change pipeline: one announcement for a multi-level jump, roles, hook, demotion re-sync (R196)', { skip: skipDb }, async () => {
  const guildId = G(4);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  await prisma.levelRole.deleteMany({ where: { guildId } });
  await settingsSvc.ensureGuildSettings(guildId);
  await prisma.levelRole.createMany({ data: [{ guildId, level: 1, roleId: R(1) }, { guildId, level: 5, roleId: R(2) }, { guildId, level: 20, roleId: R(3) }] });
  const guild = fakeGuild(G(4), { roles: [fakeRole(R(1)), fakeRole(R(2)), fakeRole(R(3))] });
  const member = fakeMember(guild, U(1));
  const ch = fakeChannel(C(1));
  const settings = { ...(await settingsSvc.getSettings(guildId)), xpEnabled: true, levelUpMode: 'CURRENT', levelUpUseEmbed: false, levelRolesSyncOnDemotion: true, levelRolesStack: true };
  const emitted = [];
  registerHook({ event: 'levelUp', id: 'achievements', run: (ctx) => emitted.push({ old: ctx.oldLevel, new: ctx.newLevel, source: ctx.source, member: ctx.member?.id }) }, { pluginId: 'test' });

  // +1500 XP in one go: level 0 -> 5 (1150 <= 1500 < 1150 + 5*25+250+100).
  const r = await svc.applyXpDelta({ guildId, userId: U(1), delta: 1500 });
  const lc = await svc.handleLevelChange({ guild, member, userId: U(1), result: r, settings, source: 'message', channel: ch });
  assert.equal(lc.leveledUp, true);
  assert.equal(ch.sent.length, 1, 'exactly one announcement');
  assert.match(ch.sent[0].content, /level 5!/);
  assert.deepEqual(emitted, [{ old: 0, new: 5, source: 'message', member: U(1) }], 'one levelUp with the real old level');
  assert.deepEqual([...member.roles.cache.keys()].sort(), [R(1), R(2)].sort(), 'every earned reward is given (backfill)');
  assert.equal((await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(1) } } })).level, 5);

  // Repeating the same transition (a stale writer) does nothing.
  const again = await svc.handleLevelChange({ guild, member, userId: U(1), result: r, settings, source: 'message', channel: ch });
  assert.equal(again.leveledUp, false);
  assert.equal(ch.sent.length, 1);

  // Admin gains never announce, but still sync roles and emit with source 'admin'.
  const r2 = await svc.applyXpDelta({ guildId, userId: U(1), delta: totalXpForLevel(20) });
  const lc2 = await svc.handleLevelChange({ guild, member, userId: U(1), result: r2, settings, source: 'admin', channel: ch, announce: false });
  assert.equal(lc2.leveledUp, true);
  assert.equal(ch.sent.length, 1);
  assert.equal(emitted.at(-1).source, 'admin');
  assert.ok(member.roles.cache.has(R(3)));
  assert.deepEqual(lc2.roles.added, [R(3)]);

  // Demotion: /xp remove drops below level 20 and 5: reward roles above the new level are removed.
  const sub = await svc.subtractXp({ guildId, userId: U(1), amount: XP_MAX });
  assert.equal(sub.newLevel, 0);
  const lc3 = await svc.handleLevelChange({ guild, member, userId: U(1), result: sub, settings, source: 'admin', announce: false });
  assert.equal(lc3.leveledUp, false);
  assert.deepEqual([...member.roles.cache.keys()], [], 'all reward roles removed');
  assert.equal(emitted.length, 2, 'no levelUp on a decrease');
  assert.equal((await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(1) } } })).level, 0);

  // With the toggle off the roles are kept.
  const member2 = fakeMember(guild, U(2), { roles: [R(1)] });
  const lc4 = await svc.handleLevelChange({
    guild, member: member2, userId: U(2), result: { xp: 10, oldLevel: 3, newLevel: 0 }, settings: { ...settings, levelRolesSyncOnDemotion: false }, source: 'admin', announce: false,
  });
  assert.equal(lc4.roles, null);
  assert.ok(member2.roles.cache.has(R(1)));
});

test('DB: handleMessageXp awards 15-25 once per cooldown and detaches the level-up', { skip: skipDb }, async () => {
  const guildId = G(5);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  const store = installFakeCache();
  const guild = fakeGuild(G(5));
  const member = fakeMember(guild, U(1), { roles: [R(7)] });
  const ch = fakeChannel(C(1));
  const row = await settingsSvc.getSettings(guildId);
  const settings = { ...row, xpEnabled: true, xpMultiplier: 1, xpBoostRoles: [{ roleId: R(7), multiplier: 2 }] };
  const ctx = { guild, member, message: { channel: ch }, getSettings: async () => settings };

  await svc.handleMessageXp(ctx);
  assert.ok(store.has(`xp:cooldown:${G(5)}:${U(1)}`));
  let u = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(1) } } });
  assert.ok(u.xp >= 30 && u.xp <= 50, `boosted x2: ${u.xp}`);
  assert.equal(u.messageCount, 1);
  const first = u.xp;
  await svc.handleMessageXp(ctx);
  u = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(1) } } });
  assert.equal(u.xp, first, 'the cooldown blocks the second message');
  assert.equal(u.messageCount, 1);
});

test('DB: voice sessions pay on checkpoint, AFK, deafen and move (R054, R191-R195)', { skip: skipDb }, async () => {
  const guildId = G(6);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  const store = installFakeCache();
  const guild = fakeGuild(G(6), { readyTimestamp: 0, afkChannelId: C(9) });
  const member = fakeMember(guild, U(1));
  const buddy = fakeMember(guild, U(2), { voice: { selfDeaf: false, serverDeaf: false } });
  const vc1 = fakeChannel(C(1), { type: ChannelType.GuildVoice, members: new Map([[U(1), member], [U(2), buddy]]) });
  const vc2 = fakeChannel(C(2), { type: ChannelType.GuildVoice, members: new Map([[U(1), member], [U(2), buddy]]) });
  const afk = fakeChannel(C(9), { type: ChannelType.GuildVoice, members: new Map([[U(1), member]]) });
  const settings = { ...(await settingsSvc.getSettings(guildId)), xpEnabled: true, voiceXpEnabled: true, voiceXpRate: 5, xpMultiplier: 1, levelUpMode: 'OFF' };
  const key = `voice:start:${G(6)}:${U(1)}`;
  const t0 = 1_700_000_000_000;
  const state = (channel, over = {}) => ({ channel, selfDeaf: false, serverDeaf: false, ...over });
  const row = () => prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(1) } } });

  await svc.reconcileVoiceMember({ guild, member, state: state(vc1), settings, now: t0, freshJoin: true });
  assert.deepEqual(JSON.parse(store.get(key)), { start: t0, channelId: C(1) });
  await svc.reconcileVoiceMember({ guild, member, state: state(vc1), settings, now: t0 + 125_000 });
  assert.equal(await row(), null, 'no payout before the checkpoint');

  // Eleven minutes: one checkpoint pays 11 minutes x 5 XP and keeps the session going.
  await svc.reconcileVoiceMember({ guild, member, state: state(vc1), settings, now: t0 + 11 * 60_000 });
  let u = await row();
  assert.equal(u.xp, 55);
  assert.equal(u.voiceMinutes, 11);
  assert.equal(u.messageCount, 0);
  assert.equal(JSON.parse(store.get(key)).start, t0 + 11 * 60_000);

  // Moving to another channel pays the stretch and keeps counting in the new one.
  await svc.reconcileVoiceMember({ guild, member, state: state(vc2), settings, now: t0 + 13 * 60_000 + 30_000 });
  u = await row();
  assert.equal(u.voiceMinutes, 13);
  assert.deepEqual(JSON.parse(store.get(key)), { start: t0 + 13 * 60_000, channelId: C(2) });

  // Deafened: the session ends and the minutes so far are paid (R191).
  await svc.reconcileVoiceMember({ guild, member, state: state(vc2, { selfDeaf: true }), settings, now: t0 + 16 * 60_000 });
  u = await row();
  assert.equal(u.voiceMinutes, 16);
  assert.equal(store.has(key), false);

  // Un-deafen: a session starts again whatever the transition was (R054/R192/R193).
  await svc.reconcileVoiceMember({ guild, member, state: state(vc2), settings, now: t0 + 20 * 60_000 });
  assert.deepEqual(JSON.parse(store.get(key)), { start: t0 + 20 * 60_000, channelId: C(2) });
  // AFK channel: pays and ends.
  await svc.reconcileVoiceMember({ guild, member, state: state(afk), settings, now: t0 + 22 * 60_000 });
  assert.equal((await row()).voiceMinutes, 18);
  assert.equal(store.has(key), false);

  // A session left over from before this process started is credited only from readyAt (R194).
  guild.client.readyTimestamp = t0 + 100 * 60_000;
  store.set(key, JSON.stringify({ start: t0, channelId: C(1) }));
  await svc.reconcileVoiceMember({ guild, member, state: state(vc1), settings, now: t0 + 100 * 60_000 + 11 * 60_000 });
  assert.equal((await row()).voiceMinutes, 18 + 11, 'only the 11 minutes since readyAt');

  // A 30 h stretch pays at most 24 h (R194).
  guild.client.readyTimestamp = 0;
  store.set(key, JSON.stringify({ start: t0, channelId: C(1) }));
  const before = (await row()).voiceMinutes;
  await svc.reconcileVoiceMember({ guild, member, state: state(vc1), settings, now: t0 + 30 * 3_600_000 });
  assert.equal((await row()).voiceMinutes - before, 1440);
});

test('DB: runVoiceSweep checkpoints and reschedules while anyone is in voice', { skip: skipDb }, async () => {
  const guildId = G(7);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  const store = installFakeCache();
  const guild = fakeGuild(G(7), { readyTimestamp: 0 });
  const member = fakeMember(guild, U(1));
  const buddy = fakeMember(guild, U(2), { voice: { selfDeaf: false, serverDeaf: false } });
  const vc = fakeChannel(C(1), { type: ChannelType.GuildVoice, members: new Map([[U(1), member], [U(2), buddy]]) });
  guild.voiceStates.cache.set(U(1), { channelId: C(1), channel: vc, member, selfDeaf: false, serverDeaf: false });
  guild.voiceStates.cache.set(U(2), { channelId: C(1), channel: vc, member: buddy, selfDeaf: false, serverDeaf: false });
  await settingsSvc.updateSettings(guildId, { xpEnabled: true, voiceXpEnabled: true });
  const key = `voice:start:${G(7)}:${U(1)}`;
  store.set(key, JSON.stringify({ start: Date.now() - 12 * 60_000, channelId: C(1) }));

  const out = await svc.runVoiceSweep({}, { guild });
  assert.ok(out.nextRunAt instanceof Date);
  assert.ok(Math.abs(out.nextRunAt.getTime() - Date.now() - VOICE_CHECKPOINT_MS) < 5000);
  assert.ok((await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(1) } } })).voiceMinutes >= 12);

  guild.voiceStates.cache.clear();
  assert.equal(await svc.runVoiceSweep({}, { guild }), undefined, 'nobody in voice: the job ends');
  await settingsSvc.updateSettings(guildId, { voiceXpEnabled: false });
  assert.equal(await svc.runVoiceSweep({}, { guild }), undefined, 'voice XP off: the job ends');
});

test('DB: roleDelete cleanup and the xp rewards provider', { skip: skipDb }, async () => {
  const guildId = G(8);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  await prisma.levelRole.deleteMany({ where: { guildId } });
  await settingsSvc.updateSettings(guildId, { xpEnabled: true, levelUpMode: 'OFF' });
  await prisma.levelRole.createMany({ data: [{ guildId, level: 1, roleId: R(1) }, { guildId, level: 2, roleId: R(2) }] });
  await settingsSvc.updateSettingsWith(guildId, () => ({ noXpRoleIds: [R(1), R(3)], xpBoostRoles: [{ roleId: R(1), multiplier: 2 }, { roleId: R(4), multiplier: 3 }] }));
  const guild = fakeGuild(G(8));
  await svc.handleRoleDelete({ guild, role: { id: R(1) }, getSettings: () => settingsSvc.getSettings(guildId) });
  assert.deepEqual((await prisma.levelRole.findMany({ where: { guildId } })).map((r) => r.roleId), [R(2)]);
  const s = await settingsSvc.getSettings(guildId, { fresh: true });
  assert.deepEqual(s.noXpRoleIds, [R(3)]);
  assert.deepEqual(s.xpBoostRoles, [{ roleId: R(4), multiplier: 3 }]);

  // The provider.
  const member = fakeMember(guild, U(1));
  const grant = await svc.xpRewardProvider.grant({ guild, userId: U(1), amount: 150, reason: 'test', member, channel: null });
  assert.deepEqual(grant, { ok: true, total: 150 });
  assert.equal((await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(1) } } })).level, 1);
  assert.deepEqual(await svc.xpRewardProvider.grant({ guild, userId: U(2), amount: 5, member: fakeMember(guild, U(2), { bot: true }) }), { ok: false, reason: 'bot' });
  assert.deepEqual(await svc.xpRewardProvider.take({ guild, userId: U(1), amount: 40 }), { ok: true, total: 110 });
  assert.deepEqual(await svc.xpRewardProvider.take({ guild, userId: U(9), amount: 40 }), { ok: false, reason: 'insufficient' });
  assert.deepEqual(await svc.xpRewardProvider.take({ guild, userId: U(1), amount: 5000 }), { ok: true, total: 0 }, 'XP clamps at zero');
  assert.deepEqual(await svc.xpRewardProvider.take({ guild, userId: U(1), amount: 1 }), { ok: false, reason: 'insufficient' });
  await settingsSvc.updateSettings(guildId, { xpEnabled: false });
  assert.deepEqual(await svc.xpRewardProvider.grant({ guild, userId: U(1), amount: 5, member }), { ok: false, reason: 'module-disabled' });
  assert.deepEqual(await svc.xpRewardProvider.take({ guild, userId: U(1), amount: 5 }), { ok: false, reason: 'module-disabled' });
});

// ── Commands against the database (fake interactions) ────────────────────────

function fakeInteraction(guild, { sub, group = null, opts = {}, user, member, channel = null }) {
  const replies = [];
  const get = (name, required) => {
    const v = opts[name];
    if (v === undefined) {
      if (required) throw new Error(`missing required option ${name}`);
      return null;
    }
    return v;
  };
  return {
    replies,
    guild,
    guildId: guild.id,
    user,
    member: member ?? null,
    channel,
    createdTimestamp: Date.now(),
    deferred: false,
    replied: false,
    ephemeral: false,
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => group,
      getString: get, getInteger: get, getNumber: get, getBoolean: get, getChannel: get, getRole: get, getUser: get, getMember: get,
      get: (name) => ({ name, ...(opts[`_${name}`] ?? {}) }),
      getFocused: () => opts.focused ?? '',
    },
    async deferReply() { this.deferred = true; },
    async editReply(p) { replies.push(p); this.replied = true; return p; },
    async followUp(p) { replies.push(p); return p; },
  };
}

const embedOf = (i) => i.replies.at(-1).embeds[0].toJSON();

test('DB: /levelconfig noxp channel never touches the announcement channel (R002)', { skip: skipDb }, async () => {
  const guildId = G(9);
  await settingsSvc.updateSettings(guildId, { levelUpChannelId: C(5), xpEnabled: true });
  const chan = fakeChannel(C(1));
  const guild = fakeGuild(G(9), { channels: [chan, fakeChannel(C(5))] });
  const user = fakeUser(U(1));
  const run = async (args) => {
    const i = fakeInteraction(guild, { user, ...args });
    await levelconfig.execute(i);
    return i;
  };

  let i = await run({ group: 'noxp', sub: 'channel', opts: { channel: chan } });
  assert.equal(embedOf(i).title, '✅  No-XP Channels');
  let s = await settingsSvc.getSettings(guildId, { fresh: true });
  assert.deepEqual(s.noXpChannelIds, [C(1)]);
  assert.equal(s.levelUpChannelId, C(5), 'the announcement channel is untouched');
  i = await run({ group: 'noxp', sub: 'channel', opts: { channel: chan } });
  assert.equal(embedOf(i).title, '❌  Not Saved');
  i = await run({ group: 'noxp', sub: 'channel', opts: { channel: chan, remove: true } });
  assert.deepEqual((await settingsSvc.getSettings(guildId, { fresh: true })).noXpChannelIds, []);
  i = await run({ group: 'noxp', sub: 'channel', opts: { channel: chan, remove: true } });
  assert.equal(embedOf(i).title, '❌  Not Saved', 'not on the list');
  // A deleted channel is pruned from the list on the next edit (the guild is available).
  await settingsSvc.updateSettings(guildId, { noXpChannelIds: [C(99)] });
  await run({ group: 'noxp', sub: 'channel', opts: { channel: chan } });
  assert.deepEqual((await settingsSvc.getSettings(guildId, { fresh: true })).noXpChannelIds, [C(1)]);
  // Roles: @everyone is refused.
  const role = fakeRole(R(1));
  guild.roles.cache.set(R(1), role);
  i = await run({ group: 'noxp', sub: 'role', opts: { role } });
  assert.equal(embedOf(i).title, '✅  No-XP Roles');
  i = await run({ group: 'noxp', sub: 'role', opts: { role: { id: guild.id } } });
  assert.match(embedOf(i).description, /levelconfig disable/);
});

test('DB: /levelconfig settings subcommands write through settingsService and reply from the saved row', { skip: skipDb }, async () => {
  const guildId = G(10);
  const guild = fakeGuild(G(10), { channels: [fakeChannel(C(1))] });
  const user = fakeUser(U(1));
  const member = fakeMember(guild, U(1), { displayName: 'Admin' });
  const run = async (args) => {
    const i = fakeInteraction(guild, { user, member, channel: fakeChannel(C(1)), ...args });
    await levelconfig.execute(i);
    return i;
  };
  const saved = () => settingsSvc.getSettings(guildId, { fresh: true });

  let i = await run({ sub: 'enable' });
  assert.match(embedOf(i).description, /15–25 XP/);
  assert.equal((await saved()).xpEnabled, true);
  i = await run({ sub: 'range', opts: { min: 30, max: 10 } });
  assert.equal(embedOf(i).title, '❌  Invalid Range');
  i = await run({ sub: 'range', opts: { min: 10, max: 30 } });
  assert.match(embedOf(i).description, /10–30 XP/);
  assert.deepEqual([(await saved()).xpMin, (await saved()).xpMax], [10, 30]);
  await run({ sub: 'cooldown', opts: { seconds: 45 } });
  await run({ sub: 'multiplier', opts: { value: 1.256 } });
  assert.deepEqual([(await saved()).xpCooldown, (await saved()).xpMultiplier], [45, 1.26]);

  // voicexp: the reply describes the saved row and warns when the module is off (R216).
  await settingsSvc.updateSettings(guildId, { xpEnabled: false, voiceXpRate: 9 });
  i = await run({ sub: 'voicexp', opts: { enabled: true } });
  assert.match(embedOf(i).description, /9 XP\/minute/);
  assert.match(embedOf(i).description, /module is off/);

  // announce
  i = await run({ sub: 'announce', opts: { mode: 'CHANNEL' } });
  assert.equal(embedOf(i).title, '❌  Channel Required');
  i = await run({ sub: 'announce', opts: { mode: 'DM', channel: fakeChannel(C(1)) } });
  assert.match(embedOf(i).description, /only used with \*\*Custom channel\*\*/);
  i = await run({ sub: 'announce', opts: { mode: 'CHANNEL', channel: fakeChannel(C(1)), embed: true } });
  assert.match(embedOf(i).description, /Custom channel\*\* → <#/);
  let s = await saved();
  assert.deepEqual([s.levelUpMode, s.levelUpChannelId, s.levelUpUseEmbed], ['CHANNEL', C(1), true]);
  await run({ sub: 'announce', opts: { mode: 'DM' } });
  s = await saved();
  assert.deepEqual([s.levelUpMode, s.levelUpChannelId], ['DM', C(1)], 'switching modes keeps the stored channel');

  // message: preview, unknown variables, reset
  i = await run({ sub: 'message', opts: { text: 'GG {player} {level} {nope} {player}' } });
  const d = embedOf(i);
  assert.match(d.description, /GG <@\d+> 5 \{nope\} <@\d+>/);
  assert.ok(d.fields.some((f) => f.name.includes('Unknown variables') && f.value.includes('{nope}')));
  assert.equal((await saved()).levelUpMessage, 'GG {player} {level} {nope} {player}');
  i = await run({ sub: 'message', opts: { text: '   ' } });
  assert.equal(embedOf(i).title, '❌  Not Saved');
  i = await run({ sub: 'message' });
  assert.equal(embedOf(i).title, '✅  Level-up Message Reset');
  assert.equal((await saved()).levelUpMessage, null);

  // status stays within embed limits even with long lists (R217/R218)
  await settingsSvc.updateSettingsWith(guildId, () => ({
    noXpRoleIds: Array.from({ length: 100 }, (_, n) => R(n + 1)),
    noXpChannelIds: Array.from({ length: 100 }, (_, n) => C(n + 1)),
    levelUpMessage: '```' + 'x'.repeat(2000),
  }));
  i = await run({ sub: 'status' });
  const st = embedOf(i);
  assert.ok(st.fields.length <= 12);
  assert.ok(st.fields.every((f) => f.value.length <= 1024 && f.name.length <= 256));
  const total = (st.title?.length ?? 0) + (st.description?.length ?? 0) + st.fields.reduce((n, f) => n + f.name.length + f.value.length, 0);
  assert.ok(total < 6000, `embed total ${total}`);

  // boost roles
  const role = fakeRole(R(1));
  i = await run({ group: 'boost', sub: 'add', opts: { role, multiplier: 1.5 } });
  assert.equal(embedOf(i).title, '✅  XP Boost Saved');
  await run({ group: 'boost', sub: 'add', opts: { role, multiplier: 2.005 } });
  assert.deepEqual((await saved()).xpBoostRoles, [{ roleId: R(1), multiplier: 2.01 }]);
  i = await run({ group: 'boost', sub: 'list' });
  assert.match(embedOf(i).description, /×2\.01 ⚠️ deleted role/);
  i = await run({ group: 'boost', sub: 'remove', opts: { role } });
  assert.deepEqual((await saved()).xpBoostRoles, []);
  i = await run({ group: 'boost', sub: 'remove', opts: { role } });
  assert.equal(embedOf(i).title, '❌  Not Saved');

  // demotion sync and stack
  await run({ sub: 'demotion-sync', opts: { enabled: false } });
  await run({ group: 'role', sub: 'stack', opts: { enabled: false } });
  s = await saved();
  assert.deepEqual([s.levelRolesSyncOnDemotion, s.levelRolesStack], [false, false]);
  await run({ sub: 'disable' });
  assert.equal((await saved()).xpEnabled, false);
});

test('DB: /levelconfig role rewards and card (R060)', { skip: skipDb }, async () => {
  const guildId = G(11);
  await prisma.levelRole.deleteMany({ where: { guildId } });
  const role1 = fakeRole(R(1), { position: 5 });
  const role2 = fakeRole(R(2), { position: 6 });
  const high = fakeRole(R(3), { position: 500 });
  const guild = fakeGuild(G(11), { roles: [role1, role2, high], botPosition: 50 });
  const user = fakeUser(U(1));
  const member = fakeMember(guild, U(1), { displayName: 'Admin' });
  member.roles.highest = { position: 300 };
  const run = async (args) => {
    const i = fakeInteraction(guild, { user, member, channel: fakeChannel(C(1)), ...args });
    await levelconfig.execute(i);
    return i;
  };

  let i = await run({ group: 'role', sub: 'add', opts: { level: 5, role: high, sync: false } });
  assert.equal(embedOf(i).title, "❌  Can't Use This Role", 'a role above the bot is refused');
  i = await run({ group: 'role', sub: 'add', opts: { level: 900, role: role1, sync: false } });
  assert.equal(embedOf(i).title, '❌  Level Too High');
  i = await run({ group: 'role', sub: 'add', opts: { level: 5, role: role1, sync: false } });
  assert.equal(embedOf(i).title, '✅  Role Reward Added');
  i = await run({ group: 'role', sub: 'add', opts: { level: 6, role: role1, sync: false } });
  assert.equal(embedOf(i).title, '❌  Already a Reward');
  i = await run({ group: 'role', sub: 'add', opts: { level: 5, role: role2, sync: false } });
  assert.match(embedOf(i).description, /Replaces <@&/);
  assert.deepEqual((await prisma.levelRole.findMany({ where: { guildId } })).map((r) => [r.level, r.roleId]), [[5, R(2)]]);

  i = await run({ group: 'role', sub: 'list' });
  assert.match(embedOf(i).description, /\*\*Level 5\*\* → <@&\d+>/);
  i = await run({ group: 'role', sub: 'remove', opts: { level: 7 } });
  assert.equal(embedOf(i).title, '❌  Not Found');
  i = await run({ group: 'role', sub: 'remove', opts: { level: 5 } });
  assert.equal(embedOf(i).title, '✅  Role Reward Removed');
  i = await run({ group: 'role', sub: 'list' });
  assert.equal(embedOf(i).title, 'ℹ️  No Role Rewards');

  // The autocomplete filters by level prefix and names the role.
  await prisma.levelRole.createMany({ data: [{ guildId, level: 10, roleId: R(1) }, { guildId, level: 15, roleId: R(2) }, { guildId, level: 20, roleId: R(9) }] });
  let answered = null;
  await levelconfig.autocomplete({
    guild, guildId, createdTimestamp: Date.now(), responded: false, options: { getFocused: () => '1' },
    respond: async (c) => { answered = c; },
  });
  assert.deepEqual(answered.map((c) => c.value), [10, 15]);
  assert.match(answered[0].name, /^Level 10 → role-/);
  await levelconfig.autocomplete({
    guild, guildId, createdTimestamp: Date.now(), responded: false, options: { getFocused: () => '2' },
    respond: async (c) => { answered = c; },
  });
  assert.deepEqual(answered, [{ name: 'Level 20 → deleted role', value: 20 }]);

  // Rank card options
  i = await run({ sub: 'card', opts: { color: 'nope' } });
  assert.equal(embedOf(i).title, '❌  Invalid Colour');
  i = await run({ sub: 'card', opts: { background_url: 'ftp://x/y.png' } });
  assert.equal(embedOf(i).title, '❌  Invalid URL');
  i = await run({ sub: 'card', opts: { background_url: 'http://127.0.0.1:9/bg.png' } });
  assert.equal(embedOf(i).title, '❌  Image Not Loaded');
  i = await run({ sub: 'card', opts: { color: '#102030', accent: 'red', overlay: 0.456, background_url: 'none' } });
  assert.equal(embedOf(i).title, '✅  Rank Card Updated');
  assert.equal(i.replies.at(-1).files?.length, 1, 'a preview card is attached');
  assert.deepEqual(await settingsSvc.getSettings(guildId, { fresh: true }).then((s) => s.rankCardStyle), {
    background: '#102030', backgroundUrl: null, accentColor: '#ED4245', textColor: '#FFFFFF', overlayOpacity: 0.46,
  });
  i = await run({ sub: 'card' });
  assert.equal(embedOf(i).title, 'ℹ️  Rank Card');
  i = await run({ sub: 'card', opts: { reset: true } });
  assert.equal(embedOf(i).title, '✅  Rank Card Reset');
  assert.equal((await settingsSvc.getSettings(guildId, { fresh: true })).rankCardStyle, null);
});

test('DB: /xp give, remove, set, reset, view (R196, R299, R300)', { skip: skipDb }, async () => {
  const guildId = G(12);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  await prisma.levelRole.deleteMany({ where: { guildId } });
  await settingsSvc.updateSettings(guildId, { xpEnabled: true, levelUpMode: 'CURRENT', levelRolesSyncOnDemotion: true, levelRolesStack: true });
  await prisma.levelRole.createMany({ data: [{ guildId, level: 1, roleId: R(1) }] });
  const guild = fakeGuild(G(12), { roles: [fakeRole(R(1))], channels: [fakeChannel(C(1))] });
  const admin = fakeUser(U(1));
  const target = fakeMember(guild, U(2), { displayName: 'Target' });
  guild.members.fetch = async (id) => guild.members.cache.get(id) ?? null;
  const ch = fakeChannel(C(1));
  const run = async (sub, opts) => {
    const i = fakeInteraction(guild, { sub, opts, user: admin, member: fakeMember(guild, U(1)), channel: ch });
    await xpCmd.execute(i, guild.client);
    return i;
  };
  const row = () => prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: U(2) } } });

  // give: atomic add, level change, role sync, no announcement.
  let i = await run('give', { amount: 150, _target: { user: target.user, member: target } });
  assert.equal(embedOf(i).title, '✅  XP Given');
  assert.match(embedOf(i).description, /Level 0 → 1/);
  assert.match(embedOf(i).description, /Roles added/);
  assert.equal((await row()).xp, 150);
  assert.equal((await row()).level, 1);
  assert.ok(target.roles.cache.has(R(1)));
  assert.equal(ch.sent.length, 0, 'admin XP never announces');
  i = await run('give', { amount: 5, _target: { user: fakeUser(U(3), { bot: true }) } });
  assert.match(embedOf(i).description, /Bots don't earn XP/);

  // remove: partial, then everything, then nothing (no row is created for a stranger).
  i = await run('remove', { user: target.user, amount: 100 });
  assert.match(embedOf(i).description, /Removed \*\*100 XP\*\*.*Total: \*\*50 XP\*\* \(level 0\)/);
  assert.match(embedOf(i).description, /Roles removed/);
  assert.equal(target.roles.cache.has(R(1)), false, 'demotion sync removed the reward role (R196)');
  i = await run('remove', { user: fakeUser(U(7)), amount: 100 });
  assert.equal(embedOf(i).title, 'ℹ️  Nothing to Remove');
  assert.equal(await prisma.userLevel.count({ where: { guildId, userId: U(7) } }), 0, 'no zero-XP row (R300)');

  // set
  i = await run('set', { user: target.user, amount: 300 });
  assert.match(embedOf(i).description, /now has \*\*300 XP\*\* \(level 2\)/);
  assert.ok(target.roles.cache.has(R(1)));
  i = await run('set', { user: fakeUser(U(8)), amount: 0 });
  assert.equal(embedOf(i).title, 'ℹ️  Nothing to Set');

  // view
  i = await run('view', { user: target.user });
  const f = Object.fromEntries(embedOf(i).fields.map((x) => [x.name, x.value]));
  assert.equal(f.Level, '2');
  assert.equal(f['Total XP'], '300');
  assert.equal(f.Rank, '#1');
  assert.equal(f.Progress, '45 / 220');

  // reset
  i = await run('reset', { user: target.user });
  assert.match(embedOf(i).description, /was level 2/);
  assert.equal(await row(), null);
  assert.equal(target.roles.cache.has(R(1)), false);
  i = await run('reset', { user: target.user });
  assert.equal(embedOf(i).title, 'ℹ️  Nothing to Reset');
  i = await run('view', { user: target.user });
  assert.equal(embedOf(i).title, 'ℹ️  No Data');
});

// ── /xp give @Role, /rank and /leaderboard against the database ─────────────

/** Makes the first editReply of a fake interaction look like awaitConfirm's message, and answers it. */
function autoConfirm(interaction, { yes = true } = {}) {
  const original = interaction.editReply.bind(interaction);
  let confirmed = false;
  interaction.editReply = async (p) => {
    const yesId = p.components?.[0]?.components?.[0]?.data?.custom_id;
    if (yesId && !confirmed) {
      confirmed = true;
      interaction.replies.push(p);
      return {
        id: 'confirm-message',
        createMessageComponentCollector() {
          const ee = new EventEmitter();
          setTimeout(() => {
            ee.emit('collect', { customId: yes ? yesId : yesId.replace(/:yes$/, ':no'), update: async () => {} });
            ee.emit('end');
          }, 5);
          return ee;
        },
      };
    }
    return original(p);
  };
  return interaction;
}

const waitFor = async (fn, ms = 3000) => {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (await fn()) return true;
    await new Promise((r) => setTimeout(r, 20));
  }
  return false;
};

test('DB: /xp give target:@Role confirms, gives in bulk and releases the bulk lock (R196, 5,000 cap)', { skip: skipDb }, async () => {
  const guildId = G(13);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  await prisma.levelRole.deleteMany({ where: { guildId } });
  await settingsSvc.updateSettings(guildId, { xpEnabled: true });
  await prisma.levelRole.createMany({ data: [{ guildId, level: 1, roleId: R(1) }] });
  const guild = fakeGuild(G(13), { roles: [fakeRole(R(1)), fakeRole(R(5))] });
  const m1 = fakeMember(guild, U(1));
  const m2 = fakeMember(guild, U(2));
  const m3 = fakeMember(guild, U(3));
  const robot = fakeMember(guild, U(4), { bot: true });
  guild.memberCount = guild.members.cache.size;
  const role = { id: R(5), members: new Collection([[U(1), m1], [U(2), m2], [U(3), m3], [U(4), robot]]) };
  const emitted = [];
  registerHook({ event: 'levelUp', id: 'achievements', run: (ctx) => emitted.push([ctx.userId, ctx.source]) }, { pluginId: 'test' });
  const make = (opts) => fakeInteraction(guild, { sub: 'give', opts, user: fakeUser(U(9)), member: fakeMember(guild, U(9)), channel: fakeChannel(C(1)) });

  // @everyone is refused, and a running bulk task blocks a second one.
  let i = make({ amount: 500, _target: { role: { id: guild.id } } });
  await xpCmd.execute(i, guild.client);
  assert.match(embedOf(i).description, /@everyone isn't allowed/);
  assert.equal(svc.tryStartBulk(guildId), true);
  i = make({ amount: 500, _target: { role } });
  await xpCmd.execute(i, guild.client);
  assert.equal(embedOf(i).title, '❌  Busy');
  svc.endBulk(guildId);

  // Cancelled: nothing is given and the lock is released.
  i = autoConfirm(make({ amount: 500, _target: { role } }), { yes: false });
  await xpCmd.execute(i, guild.client);
  assert.equal(embedOf(i).title, 'ℹ️  Cancelled');
  assert.equal(await prisma.userLevel.count({ where: { guildId } }), 0);
  assert.equal(svc.isBulkRunning(guildId), false);

  // Confirmed: three humans get the XP, the bot does not, roles sync and levelUp fires in the background.
  i = autoConfirm(make({ amount: 500, _target: { role } }));
  await xpCmd.execute(i, guild.client);
  assert.equal(embedOf(i).title, '✅  XP Given');
  assert.match(embedOf(i).description, /\*\*3\*\* members.*3 levelled up/);
  assert.equal(await prisma.userLevel.count({ where: { guildId } }), 3);
  assert.equal(await prisma.userLevel.count({ where: { guildId, userId: U(4) } }), 0, 'bots are skipped');
  assert.ok(await waitFor(() => !svc.isBulkRunning(guildId)), 'the bulk lock is released after the background task');
  assert.ok([m1, m2, m3].every((m) => m.roles.cache.has(R(1))), 'reward roles were synced');
  assert.equal(emitted.length, 3);
  assert.ok(emitted.every(([, source]) => source === 'admin'));
  assert.ok((await prisma.userLevel.findMany({ where: { guildId } })).every((r) => r.level === levelFromXp(r.xp)));

  // A role with nobody (but bots) and a role beyond the cap are refused without leaking the lock.
  i = make({ amount: 5, _target: { role: { id: R(6), members: new Collection([[U(4), robot]]) } } });
  await xpCmd.execute(i, guild.client);
  assert.match(embedOf(i).description, /No members/);
  const huge = new Collection(Array.from({ length: svc.ROLE_GIVE_MAX_MEMBERS + 1 }, (_, n) => [String(n), { id: String(n), user: { bot: false } }]));
  i = make({ amount: 5, _target: { role: { id: R(7), members: huge } } });
  await xpCmd.execute(i, guild.client);
  assert.match(embedOf(i).description, /limit is 5,000/);
  assert.equal(svc.isBulkRunning(guildId), false);
});

test('DB: /rank uses the tie rule and the member name; no XP and bots get friendly replies (R243, R245)', { skip: skipDb }, async () => {
  const guildId = G(14);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  await settingsSvc.updateSettings(guildId, { xpEnabled: true });
  const guild = fakeGuild(G(14));
  const me = fakeUser(U(1));
  me.displayAvatarURL = () => 'http://127.0.0.1:9/avatar.png'; // blocked by safeFetch: the card is drawn without an avatar
  const invoker = fakeMember(guild, U(1), { displayName: 'Invoker' });
  invoker.displayAvatarURL = () => 'http://127.0.0.1:9/avatar.png';
  const run = async (target) => {
    const i = fakeInteraction(guild, { sub: null, user: me, member: invoker, opts: target ? { user: target } : {} });
    await rankCmd.execute(i);
    return i;
  };

  let i = await run(null);
  assert.equal(embedOf(i).title, 'ℹ️  No XP Yet');
  assert.match(embedOf(i).description, /^Invoker hasn't earned any XP/, 'the server display name, not the global one');
  i = await run(fakeUser(U(5), { bot: true }));
  assert.equal(embedOf(i).title, 'ℹ️  No Rank');

  for (const n of [1, 2, 3]) await svc.setXp({ guildId, userId: U(n), xp: 500 });
  await svc.setXp({ guildId, userId: U(4), xp: 900 });
  i = await run(null);
  const reply = i.replies.at(-1);
  assert.equal(reply.files?.length, 1, 'the rank card is attached');
  assert.equal(reply.files[0].name, 'rank.png');
  assert.equal(await svc.getRank(guildId, U(1), 500), 2, 'ahead of the equal-XP members with a larger id');
  assert.equal(await svc.getRank(guildId, U(3), 500), 4);
});

test('DB: /leaderboard orders by xp then userId, paginates and shows your place (R092, R093, R244)', { skip: skipDb }, async () => {
  const guildId = G(15);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  await settingsSvc.updateSettings(guildId, { xpEnabled: true });
  const guild = fakeGuild(G(15));
  const named = fakeMember(guild, U(3), { displayName: 'Third **bold**' });
  const run = async (opts) => {
    const i = fakeInteraction(guild, { sub: null, opts, user: fakeUser(U(3)), member: named });
    i.isRepliable = () => true;
    i.deferred = true;
    i.editReply = async (p) => { i.replies.push(p); return { id: 'm', createMessageComponentCollector: () => ({ on() {}, stop() {} }) }; };
    await boardCmd.execute(i);
    return i;
  };

  let i = await run({});
  assert.equal(embedOf(i).title, 'ℹ️  Leaderboard');
  assert.match(embedOf(i).description, /No one has earned XP/);

  // Twelve members; U(1)..U(3) tie on 700, the rest descend.
  for (let n = 1; n <= 3; n++) await svc.setXp({ guildId, userId: U(n), xp: 700 });
  for (let n = 4; n <= 12; n++) await svc.setXp({ guildId, userId: U(n), xp: 1000 - n * 10 });
  i = await run({});
  const first = embedOf(i);
  const lines = first.description.split('\n');
  assert.equal(lines.length, 10);
  assert.match(lines[0], /^🥇 <@\d+> — Level \*\*\d+\*\* · 960 XP$/, 'the highest XP first, uncached members as mentions');
  assert.ok(lines[0].includes(U(4)));
  assert.ok(first.footer.text.startsWith('Top 12 of 12 ranked members'));
  assert.match(first.footer.text, /You: #12/);
  assert.match(first.footer.text, / • Page 1\/2$/, 'the paginator appends the page once (R244)');
  assert.equal(first.title, '🏆 Test Guild Leaderboard');

  // Page 2 starts with the tail; ties go by userId ascending, matching /rank.
  i = await run({ page: 2 });
  const second = embedOf(i).description.split('\n');
  assert.equal(second.length, 2);
  const board = await prisma.userLevel.findMany({ where: { guildId, xp: { gt: 0 } }, orderBy: [{ xp: 'desc' }, { userId: 'asc' }] });
  assert.deepEqual(board.slice(-3).map((r) => r.userId), [U(1), U(2), U(3)]);
  assert.ok(second[1].includes('Third \\*\\*bold\\*\\*'), 'the cached member is shown by escaped display name');
  assert.equal(await svc.getRank(guildId, U(3), 700), 12);
});

test('DB: /levelconfig role add backfills in the background and role sync re-syncs members (R196)', { skip: skipDb }, async () => {
  const guildId = G(16);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  await prisma.levelRole.deleteMany({ where: { guildId } });
  await settingsSvc.updateSettings(guildId, { xpEnabled: true, levelRolesStack: true, levelRolesSyncOnDemotion: true });
  const reward = fakeRole(R(1), { position: 5 });
  const guild = fakeGuild(G(16), { roles: [reward], botPosition: 50 });
  const admin = fakeMember(guild, U(9), { displayName: 'Admin' });
  admin.roles.highest = { position: 300 };
  const m1 = fakeMember(guild, U(1)); // level 4 (1000 XP)
  const m2 = fakeMember(guild, U(2)); // level 0 (50 XP), wrongly holding the reward
  const m3 = fakeMember(guild, U(3), { bot: true });
  m2.roles.cache.set(R(1), { id: R(1) });
  await svc.setXp({ guildId, userId: U(1), xp: 1000 });
  await svc.setXp({ guildId, userId: U(2), xp: 50 });
  await svc.setXp({ guildId, userId: U(3), xp: 5000 });
  guild.members.fetch = async (arg) => (typeof arg === 'string' ? guild.members.cache.get(arg) ?? null : null);
  const make = (args) => fakeInteraction(guild, { user: admin.user, member: admin, channel: fakeChannel(C(1)), ...args });

  // role add (sync defaults to yes): the reply comes first, the backfill runs afterwards and reports back.
  let i = make({ group: 'role', sub: 'add', opts: { level: 3, role: reward } });
  await levelconfig.execute(i);
  assert.equal(embedOf(i).title, '✅  Role Reward Added');
  assert.match(embedOf(i).description, /in the background/);
  assert.ok(await waitFor(() => i.replies.length >= 2 && !svc.isBulkRunning(guildId)), 'the summary follow-up is sent and the lock released');
  assert.match(i.replies.at(-1).embeds[0].toJSON().description, /Role sync for level 3: 1 updated, 0 failed, 0 no longer in the server/);
  assert.ok(m1.roles.cache.has(R(1)), 'the member already at level 3+ received the role');
  assert.ok(m2.roles.cache.has(R(1)), 'a lower member is untouched by the backfill');
  assert.equal(m3.calls.length, 0, 'bots are skipped');

  // role sync for one member removes the role they should not have (demotion sync is on).
  i = make({ group: 'role', sub: 'sync', opts: { user: m2.user } });
  await levelconfig.execute(i);
  assert.equal(embedOf(i).title, '✅  Roles Synced');
  assert.match(embedOf(i).description, /level 0\): added none, removed <@&/);
  assert.equal(m2.roles.cache.has(R(1)), false);
  i = make({ group: 'role', sub: 'sync', opts: { user: m3.user } });
  await levelconfig.execute(i);
  assert.match(embedOf(i).description, /Bots don't have levels/);

  // role sync for everyone runs in the background and holds the bulk lock while it works.
  m2.roles.cache.set(R(1), { id: R(1) });
  m1.roles.cache.delete(R(1));
  i = make({ group: 'role', sub: 'sync', opts: {} });
  await levelconfig.execute(i);
  assert.equal(embedOf(i).title, '✅  Role Sync Started');
  assert.match(embedOf(i).description, /3 ranked member/);
  assert.ok(await waitFor(() => i.replies.length >= 2 && !svc.isBulkRunning(guildId)));
  assert.ok(m1.roles.cache.has(R(1)) && !m2.roles.cache.has(R(1)));
  assert.equal(svc.tryStartBulk(guildId), true);
  i = make({ group: 'role', sub: 'sync', opts: {} });
  await levelconfig.execute(i);
  assert.equal(embedOf(i).title, '❌  Busy');
  svc.endBulk(guildId);
});

test('DB: a {rank} level-up template shows the real position', { skip: skipDb }, async () => {
  const guildId = G(16);
  await prisma.userLevel.deleteMany({ where: { guildId } });
  const guild = fakeGuild(G(16));
  const member = fakeMember(guild, U(1), { displayName: 'Alice' });
  await svc.setXp({ guildId, userId: U(2), xp: 5000 });
  await svc.setXp({ guildId, userId: U(1), xp: 1200 });
  const ch = fakeChannel(C(1));
  const settings = { levelUpMode: 'CURRENT', levelUpUseEmbed: false, levelUpMessage: '{player.name} is #{rank} with {xp} XP at level {level}' };
  assert.equal(await svc.announceLevelUp({ guild, member, level: 5, xp: 1200, settings, source: 'message', sourceChannel: ch }), true);
  assert.equal(ch.sent[0].content, 'Alice is #2 with 1,200 XP at level 5');
});

// ── Command latency: independent reads overlap (no database: Prisma is stubbed) ─

/** Replaces Prisma model methods for one test; returns the restore function. */
function stubPrisma(model, methods) {
  const real = {};
  for (const [name, fn] of Object.entries(methods)) {
    real[name] = prisma[model][name];
    prisma[model][name] = fn;
  }
  return () => { for (const [name, fn] of Object.entries(real)) prisma[model][name] = fn; };
}

/** A promise held open until `open()`, to keep a stubbed query in flight. */
function gate() {
  let open;
  const promise = new Promise((resolve) => { open = resolve; });
  return { promise, open };
}

const eventually = async (fn, ms = 2000) => {
  const end = Date.now() + ms;
  while (!fn()) {
    if (Date.now() > end) return false;
    await new Promise((resolve) => { setTimeout(resolve, 5); });
  }
  return true;
};

test('/rank loads the XP row and the settings together, then draws the card off the main thread', async () => {
  installFakeCache();
  const guildId = '955009999999999901';
  const started = [];
  const rowGate = gate();
  const settingsGate = gate();
  const restore = [
    stubPrisma('userLevel', {
      findUnique: async () => { started.push('row'); await rowGate.promise; return { guildId, userId: U(1), xp: 500 }; },
      count: async () => { started.push('rank'); return 1; },
    }),
    stubPrisma('guildSettings', {
      findUnique: async () => { started.push('settings'); await settingsGate.promise; return { guildId, xpEnabled: true }; },
    }),
  ];
  try {
    const guild = fakeGuild(guildId);
    const me = fakeUser(U(1));
    me.displayAvatarURL = () => 'http://127.0.0.1:9/avatar.png'; // blocked by safeFetch: drawn without an avatar
    const invoker = fakeMember(guild, U(1));
    invoker.displayAvatarURL = () => 'http://127.0.0.1:9/avatar.png';
    const i = fakeInteraction(guild, { sub: null, user: me, member: invoker, opts: {} });
    const run = rankCmd.execute(i);
    assert.ok(await eventually(() => started.length >= 2));
    assert.deepEqual([...started].sort(), ['row', 'settings'], 'both reads are in flight before either finishes');
    rowGate.open();
    settingsGate.open();
    await run;
    assert.ok(started.includes('rank'));
    const file = i.replies.at(-1).files?.[0];
    assert.equal(file?.name, 'rank.png');
    assert.ok(Buffer.isBuffer(file.attachment) && file.attachment.length > 0, 'encode() still yields a PNG buffer');
  } finally {
    for (const r of restore) r();
  }
});

test('/leaderboard runs its three reads together and finds your place while member names load', async () => {
  installFakeCache();
  const guildId = '955009999999999902';
  const started = [];
  const gates = { total: gate(), rows: gate(), settings: gate(), names: gate() };
  const restore = [
    stubPrisma('userLevel', {
      count: async ({ where }) => {
        if (where.OR) { started.push('own-rank'); return 7; } // seven members ahead of the invoker
        started.push('total');
        await gates.total.promise;
        return 2;
      },
      findMany: async () => {
        started.push('rows');
        await gates.rows.promise;
        return [{ userId: U(1), xp: 900 }, { userId: U(2), xp: 800 }];
      },
      findUnique: async () => { started.push('own-row'); return { xp: 10 }; },
    }),
    stubPrisma('guildSettings', {
      findUnique: async () => { started.push('settings'); await gates.settings.promise; return { guildId, xpEnabled: true }; },
    }),
  ];
  try {
    const guild = fakeGuild(guildId);
    guild.members.fetch = async () => { started.push('names'); await gates.names.promise; return null; };
    const i = fakeInteraction(guild, { sub: null, opts: {}, user: fakeUser(U(3)), member: fakeMember(guild, U(3)) });
    guild.members.cache.delete(U(3));
    i.isRepliable = () => true;
    i.deferred = true;
    i.editReply = async (p) => { i.replies.push(p); return { id: 'm', createMessageComponentCollector: () => ({ on() {}, stop() {} }) }; };
    const run = boardCmd.execute(i);

    assert.ok(await eventually(() => started.length >= 3));
    assert.deepEqual([...started].sort(), ['rows', 'settings', 'total'], 'the three reads are in flight together');
    gates.total.open();
    gates.rows.open();
    gates.settings.open();

    assert.ok(await eventually(() => started.includes('own-rank')), 'your place is looked up while the names still load');
    assert.ok(started.includes('names'));
    gates.names.open();
    await run;
    assert.match(embedOf(i).footer.text, /^Top 2 of 2 ranked members · You: #8/);
  } finally {
    for (const r of restore) r();
  }
});
