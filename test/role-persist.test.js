// Role Persistence tests. Offline (no Discord, Postgres or Redis): the save and restore rules against an in-memory
// PersistedMember table and fake guilds, members and roles, the gates (Rules Screening, captcha), the hook filters,
// the plugin declaration and the /rolepersist payload and replies.
import { test, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { PermissionFlagsBits } from 'discord.js';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const svc = await import('../src/services/rolePersistService.js');
const rp = await import('../src/plugins/role-persist.js');
const cmd = await import('../src/commands/admin/rolepersist.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { PERMISSION_LEVELS, LIMITS } = await import('../src/config/constants.js');
const { getSettings, invalidateSettings } = await import('../src/services/settingsService.js');

const { ROLEPERSIST, retentionDays, isStale, expiresAt, snapshotMember, planRoles, gateOf, pruneExpired, resetPruneStamps,
  isWaiting, resetWaiting, loadRow } = svc;
const { AUDIT_REASON, onLeave, onJoin, onUpdate, onRoleDelete, restoreMember, gateMayHaveCleared, roleDeleteFilter, plugin } = rp;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory tables ──────────────────────────────────────────────────────────

const rows = new Map(); // `${guildId}:${userId}` -> PersistedMember row
const settingsRows = new Map();
const stats = { deleteMany: 0 };
const realPersisted = prisma.persistedMember;
const realGuildSettings = prisma.guildSettings;
const realTransaction = prisma.$transaction;

const rowKey = (g, u) => `${g}:${u}`;
prisma.persistedMember = {
  findUnique: async ({ where }) => {
    const { guildId, userId } = where.guildId_userId;
    const r = rows.get(rowKey(guildId, userId));
    return r ? { ...r, roleIds: [...r.roleIds] } : null;
  },
  upsert: async ({ where, create, update }) => {
    const { guildId, userId } = where.guildId_userId;
    const key = rowKey(guildId, userId);
    const row = rows.has(key) ? { ...rows.get(key), ...update } : { ...create };
    rows.set(key, row);
    return { ...row };
  },
  deleteMany: async ({ where }) => {
    stats.deleteMany++;
    let count = 0;
    for (const [key, r] of rows) {
      if (r.guildId !== where.guildId) continue;
      if (where.userId && r.userId !== where.userId) continue;
      if (where.leftAt?.lt && !(new Date(r.leftAt) < where.leftAt.lt)) continue;
      rows.delete(key);
      count++;
    }
    return { count };
  },
  count: async ({ where }) => [...rows.values()].filter((r) => r.guildId === where.guildId).length,
};
const settingsStub = {
  findUnique: async ({ where }) => (settingsRows.has(where.guildId) ? { ...settingsRows.get(where.guildId) } : null),
  upsert: async ({ where, update, create }) => {
    const row = { ...(settingsRows.get(where.guildId) ?? { guildId: where.guildId, ...create }), ...update };
    settingsRows.set(where.guildId, row);
    return { ...row };
  },
  update: async ({ where, data }) => {
    const row = { ...settingsRows.get(where.guildId), ...data };
    settingsRows.set(where.guildId, row);
    return { ...row };
  },
};
prisma.guildSettings = settingsStub;
prisma.$transaction = async (fn) => fn({ $executeRaw: async () => 0, $queryRaw: async () => [], guildSettings: settingsStub });

after(async () => {
  prisma.persistedMember = realPersisted;
  prisma.guildSettings = realGuildSettings;
  prisma.$transaction = realTransaction;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

beforeEach(() => {
  rows.clear();
  stats.deleteMany = 0;
  resetPruneStamps();
  resetWaiting();
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

const BASE_SETTINGS = Object.freeze({
  rolePersistEnabled: true,
  rolePersistNicknames: true,
  rolePersistIgnoredRoleIds: [],
  rolePersistDays: 90,
  captchaEnabled: false,
  captchaRoleId: null,
  embedColor: null,
});
const SETTINGS = (over = {}) => ({ ...BASE_SETTINGS, rolePersistIgnoredRoleIds: [], ...over });

/** Seeds the guild's GuildSettings row and drops the memo, so the next read loads this row. */
async function seedSettings(guildId, over = {}) {
  settingsRows.set(guildId, { guildId, ...SETTINGS(over) });
  await invalidateSettings(guildId);
  return settingsRows.get(guildId);
}

const discordErr = (code) => Object.assign(new Error(`Discord error ${code}`), { code });

function fakeRole(id, { position = 1, managed = false, perms = [] } = {}) {
  return {
    id, name: id, position, managed,
    permissions: { has: (flag) => perms.some((k) => PermissionFlagsBits[k] === flag) },
  };
}

let guildCounter = 0;
function fakeGuild({ roles = [], botPosition = 10, botPerms = ['ManageRoles', 'ManageNicknames'] } = {}) {
  const guild = {
    id: `G${++guildCounter}`, ownerId: 'OWNER', roles: { cache: new Map() }, fetchCalls: [], fetchError: null, memberStore: new Map(),
    members: {
      me: {
        permissions: { has: (flag) => botPerms.some((k) => PermissionFlagsBits[k] === flag) },
        roles: { highest: { position: botPosition } },
      },
      fetch: async ({ user, force }) => {
        guild.fetchCalls.push({ user, force });
        if (guild.fetchError) throw guild.fetchError;
        return guild.memberStore.get(user);
      },
    },
  };
  guild.roles.cache.set(guild.id, fakeRole(guild.id));
  for (const r of roles) guild.roles.cache.set(r.id, r);
  return guild;
}

function fakeMember(guild, id, {
  roles = [], nickname = null, pending = false, bot = false, partial = false, manageable = true, joinedTimestamp = Date.now(),
} = {}) {
  const member = {
    id, guild, partial, pending, nickname, manageable, joinedTimestamp,
    user: { id, bot },
    adds: [], nicks: [], addError: null,
    roles: {
      cache: new Map([[guild.id, guild.roles.cache.get(guild.id)], ...roles.map((r) => [r, guild.roles.cache.get(r) ?? { id: r }])]),
      async add(ids, reason) {
        member.adds.push({ ids, reason });
        if (member.addError) member.addError(ids);
        for (const r of [].concat(ids)) member.roles.cache.set(r, guild.roles.cache.get(r) ?? { id: r });
      },
    },
    async setNickname(n, reason) { member.nicks.push({ n, reason }); member.nickname = n; },
  };
  guild.memberStore.set(id, member);
  return member;
}

const ctxFor = (guild, member, settings, extra = {}) => ({
  client: {}, guild, guildId: guild.id, member, user: member.user, state: {}, getSettings: async () => settings, ...extra,
});

const saveRow = (guild, userId, { roleIds = [], nickname = null, leftAt = new Date() } = {}) => {
  rows.set(rowKey(guild.id, userId), { guildId: guild.id, userId, roleIds, nickname, leftAt });
};
const rowOf = (guild, userId) => rows.get(rowKey(guild.id, userId)) ?? null;

/** A guild with the roles the restore tests need. */
function roleGuild(over = {}) {
  return fakeGuild({
    roles: [
      fakeRole('A', { position: 1 }),
      fakeRole('B', { position: 2 }),
      fakeRole('C', { position: 3 }),
      fakeRole('ADMINR', { position: 1, perms: ['Administrator'] }),
      fakeRole('HIGH', { position: 20 }),
      fakeRole('BOTROLE', { position: 1, managed: true }),
      fakeRole('IGN', { position: 1 }),
      fakeRole('CAP', { position: 1 }),
    ],
    ...over,
  });
}

// ── Settings and the pure rules ───────────────────────────────────────────────

test('retention: days are clamped to 1..365 and rows expire after them', () => {
  assert.equal(retentionDays({ rolePersistDays: 30 }), 30);
  assert.equal(retentionDays({ rolePersistDays: 0 }), ROLEPERSIST.MIN_DAYS);
  assert.equal(retentionDays({ rolePersistDays: 9999 }), ROLEPERSIST.MAX_DAYS);
  assert.equal(retentionDays({}), ROLEPERSIST.DEFAULT_DAYS);
  assert.equal(retentionDays(null), ROLEPERSIST.DEFAULT_DAYS);
  const now = Date.UTC(2026, 9, 20);
  const settings = { rolePersistDays: 10 };
  assert.equal(isStale({ leftAt: new Date(now - 9 * ROLEPERSIST.DAY_MS) }, settings, now), false);
  assert.equal(isStale({ leftAt: new Date(now - 10 * ROLEPERSIST.DAY_MS) }, settings, now), true);
  assert.equal(expiresAt({ leftAt: new Date(now) }, settings), now + 10 * ROLEPERSIST.DAY_MS);
});

test('snapshot: skips @everyone, managed, deleted and ignored roles; keeps the nickname', () => {
  const guild = roleGuild();
  const m = fakeMember(guild, 'U1', { roles: ['A', 'B', 'BOTROLE', 'IGN', 'GONE'], nickname: '  Nick  ' });
  const snap = snapshotMember(m, guild, SETTINGS({ rolePersistIgnoredRoleIds: ['IGN'] }));
  assert.deepEqual(snap.roleIds.sort(), ['A', 'B']);
  assert.equal(snap.nickname, 'Nick');
});

test('snapshot and plan: Voice Roles roles are never saved or given back', () => {
  const guild = roleGuild();
  const settings = SETTINGS({ voiceRoleLinks: [{ channelId: 'VC1', roleId: 'A' }], voiceRoleAnyRoleIds: ['B'] });
  assert.equal(snapshotMember(fakeMember(guild, 'U1', { roles: ['A', 'B'], nickname: null }), guild, settings), null);
  assert.deepEqual(planRoles(guild, ['A', 'B'], settings).give, []);
});

test('snapshot: a partial member with no roles known, or nothing to give back, saves nothing', () => {
  const guild = roleGuild();
  assert.equal(snapshotMember(fakeMember(guild, 'U1', { partial: true }), guild, SETTINGS()), null);
  assert.equal(snapshotMember(fakeMember(guild, 'U2', { roles: [], nickname: null }), guild, SETTINGS()), null);
  assert.equal(snapshotMember(fakeMember(guild, 'U3', { roles: ['BOTROLE'] }), guild, SETTINGS()), null);
  // A nickname alone is worth saving.
  assert.deepEqual(snapshotMember(fakeMember(guild, 'U4', { nickname: 'Solo' }), guild, SETTINGS()), { roleIds: [], nickname: 'Solo' });
  assert.equal(snapshotMember(fakeMember(guild, 'U5', { nickname: 'x'.repeat(40) }), guild, SETTINGS()).nickname.length, 32);
});

test('plan: gives only roles that exist, are allowed and safe', () => {
  const guild = roleGuild();
  const settings = SETTINGS({ rolePersistIgnoredRoleIds: ['IGN'], captchaEnabled: true, captchaRoleId: 'CAP' });
  const held = new Set(['B']);
  const { give, skipped } = planRoles(guild, ['A', 'B', 'ADMINR', 'HIGH', 'BOTROLE', 'IGN', 'CAP', 'GONE', guild.id, 'A'], settings, { held });
  assert.deepEqual(give.map((r) => r.id), ['A']);
  const why = Object.fromEntries(skipped.map((s) => [s.roleId, s.reason]));
  assert.deepEqual(Object.keys(why).sort(), ['ADMINR', 'BOTROLE', 'CAP', 'GONE', 'HIGH', 'IGN']);
  assert.match(why.ADMINR, /dangerous permissions: Administrator/);
  assert.match(why.HIGH, /above my highest role/);
  assert.match(why.BOTROLE, /managed/);
  assert.match(why.IGN, /never-give-back/);
  assert.match(why.CAP, /captcha/);
  assert.equal(skipped.find((s) => s.roleId === 'GONE').deleted, true);
});

test('plan: every dangerous permission in the brief keeps a role from being given back', () => {
  const perms = ['Administrator', 'ManageGuild', 'ManageRoles', 'ManageChannels', 'BanMembers', 'KickMembers',
    'ModerateMembers', 'ManageWebhooks', 'MentionEveryone', 'ManageMessages'];
  const guild = fakeGuild({ roles: perms.map((p) => fakeRole(p, { perms: [p] })) });
  const { give, skipped } = planRoles(guild, perms, SETTINGS());
  assert.deepEqual(give, []);
  assert.equal(skipped.length, perms.length);
});

test('gates: Rules Screening and the captcha role', () => {
  const guild = roleGuild();
  const captcha = SETTINGS({ captchaEnabled: true, captchaRoleId: 'CAP' });
  assert.equal(gateOf(fakeMember(guild, 'U1', { pending: true }), SETTINGS()), 'pending');
  assert.equal(gateOf(fakeMember(guild, 'U2'), captcha), 'captcha');
  assert.equal(gateOf(fakeMember(guild, 'U3', { roles: ['CAP'] }), captcha), null, 'verified');
  assert.equal(gateOf(fakeMember(guild, 'U4'), SETTINGS()), null);
  assert.equal(gateOf(fakeMember(guild, 'U5'), SETTINGS({ captchaEnabled: true, captchaRoleId: 'DELETED' })), null, 'a deleted captcha role never gates');
  assert.equal(gateOf(fakeMember(guild, 'U6'), SETTINGS(), { captchaPending: true }), 'captcha');
});

// ── Leave ─────────────────────────────────────────────────────────────────────

test('leave: saves roles and nickname', async () => {
  const guild = roleGuild();
  const settings = SETTINGS({ rolePersistIgnoredRoleIds: ['IGN'] });
  await onLeave(ctxFor(guild, fakeMember(guild, 'U1', { roles: ['A', 'B', 'IGN', 'BOTROLE'], nickname: 'Nick' }), settings));
  assert.deepEqual(rowOf(guild, 'U1').roleIds.sort(), ['A', 'B']);
  assert.equal(rowOf(guild, 'U1').nickname, 'Nick');
});

test('leave: a member who leaves again while the restore still waits keeps the roles saved the first time', async () => {
  const guild = roleGuild();
  const settings = SETTINGS({ captchaEnabled: true, captchaRoleId: 'CAP' });
  const first = new Date(Date.now() - 3 * ROLEPERSIST.DAY_MS);
  saveRow(guild, 'U1', { roleIds: ['A', 'B', 'C'], nickname: 'Old', leftAt: first });

  // Rejoined, the captcha holds the restore, and they leave again with one new role and no nickname.
  const rejoined = fakeMember(guild, 'U1');
  await onJoin(ctxFor(guild, rejoined, settings));
  assert.equal(isWaiting(guild.id, 'U1'), true);
  await onLeave(ctxFor(guild, fakeMember(guild, 'U1', { roles: ['ADMINR', 'B'], nickname: null }), settings));
  const row = rowOf(guild, 'U1');
  assert.deepEqual(row.roleIds, ['A', 'B', 'C', 'ADMINR'], 'old ids first, new ids added, no duplicates');
  assert.equal(row.nickname, 'Old', 'no nickname known now: the old one stays');
  assert.ok(row.leftAt > first, 'leftAt is refreshed');
  assert.equal(isWaiting(guild.id, 'U1'), false, 'leaving ends the wait');

  // A later rejoin that clears the gate gets all of them back (the plan still drops what is unsafe).
  const again = fakeMember(guild, 'U1', { roles: ['CAP'] });
  const res = await restoreMember(again, settings);
  assert.equal(res.status, 'restored');
  assert.deepEqual(res.given, ['A', 'B', 'C']);
  assert.deepEqual(res.skipped.map((s) => s.roleId), ['ADMINR']);
});

test('leave: a newer nickname replaces the older one when a row is merged', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A'], nickname: 'Old' });
  await onLeave(ctxFor(guild, fakeMember(guild, 'U1', { roles: ['B'], nickname: 'Newer' }), SETTINGS()));
  assert.deepEqual(rowOf(guild, 'U1').roleIds, ['A', 'B']);
  assert.equal(rowOf(guild, 'U1').nickname, 'Newer');
  // Roles only, no nickname before or after.
  saveRow(guild, 'U2', { roleIds: ['A'], nickname: null });
  await onLeave(ctxFor(guild, fakeMember(guild, 'U2', { roles: ['B'] }), SETTINGS()));
  assert.equal(rowOf(guild, 'U2').nickname, null);
});

test('leave: an expired row is replaced, not merged', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A', 'B'], nickname: 'Old', leftAt: new Date(Date.now() - 91 * ROLEPERSIST.DAY_MS) });
  await onLeave(ctxFor(guild, fakeMember(guild, 'U1', { roles: ['C'], nickname: null }), SETTINGS({ rolePersistDays: 90 })));
  assert.deepEqual(rowOf(guild, 'U1').roleIds, ['C']);
  assert.equal(rowOf(guild, 'U1').nickname, null);
});

test('mergeSnapshot: dedupes, keeps order, and prefers a known newer nickname', () => {
  assert.deepEqual(svc.mergeSnapshot(null, { roleIds: ['A'], nickname: null }), { roleIds: ['A'], nickname: null });
  assert.deepEqual(svc.mergeSnapshot({ roleIds: ['B', 'A'], nickname: 'Old' }, { roleIds: ['A', 'C'], nickname: null }),
    { roleIds: ['B', 'A', 'C'], nickname: 'Old' });
  assert.deepEqual(svc.mergeSnapshot({ roleIds: ['B'], nickname: 'Old' }, { roleIds: [], nickname: 'New' }),
    { roleIds: ['B'], nickname: 'New' });
});

test('leave: a partial member or a member with nothing to save keeps the older row', async () => {
  const guild = roleGuild();
  const old = new Date(Date.now() - 5 * ROLEPERSIST.DAY_MS);
  saveRow(guild, 'U1', { roleIds: ['A'], nickname: 'Old', leftAt: old });
  await onLeave(ctxFor(guild, fakeMember(guild, 'U1', { partial: true }), SETTINGS()));
  assert.deepEqual(rowOf(guild, 'U1').roleIds, ['A']);
  await onLeave(ctxFor(guild, fakeMember(guild, 'U1', { roles: [] }), SETTINGS()));
  assert.deepEqual(rowOf(guild, 'U1').roleIds, ['A']);
  assert.equal(rowOf(guild, 'U1').leftAt, old);
});

test('leave: no settings means nothing happens; the hook filter skips bots', async () => {
  const guild = roleGuild();
  await onLeave(ctxFor(guild, fakeMember(guild, 'U1', { roles: ['A'] }), null));
  assert.equal(rows.size, 0);
  assert.equal(rp.leaveFilter({ user: { id: 'U', bot: true } }), false);
  assert.equal(rp.leaveFilter({ user: { id: 'U', bot: false } }), true);
  assert.equal(rp.leaveFilter({}), false);
  assert.equal(rp.joinFilter({ member: { user: { bot: true } } }), false);
  assert.equal(rp.joinFilter({ member: { user: { bot: false } } }), true);
});

test('prune: expired rows go, at most once per guild per hour', async () => {
  const guild = roleGuild();
  const settings = SETTINGS({ rolePersistDays: 10 });
  const t0 = Date.now();
  saveRow(guild, 'OLD', { roleIds: ['A'], leftAt: new Date(t0 - 11 * ROLEPERSIST.DAY_MS) });
  saveRow(guild, 'NEW', { roleIds: ['A'], leftAt: new Date(t0 - 9 * ROLEPERSIST.DAY_MS) });
  assert.equal(await pruneExpired(guild.id, settings, t0), 1);
  assert.equal(rowOf(guild, 'OLD'), null);
  assert.ok(rowOf(guild, 'NEW'));
  saveRow(guild, 'OLD2', { roleIds: ['A'], leftAt: new Date(t0 - 30 * ROLEPERSIST.DAY_MS) });
  const calls = stats.deleteMany;
  assert.equal(await pruneExpired(guild.id, settings, t0 + 59 * 60_000), null, 'within the hour: skipped');
  assert.equal(stats.deleteMany, calls, 'no query');
  assert.ok(rowOf(guild, 'OLD2'));
  assert.equal(await pruneExpired(guild.id, settings, t0 + ROLEPERSIST.PRUNE_EVERY_MS + 1), 1);
  assert.equal(rowOf(guild, 'OLD2'), null);
  // Another guild has its own stamp.
  assert.notEqual(await pruneExpired('OTHER-GUILD', settings, t0 + 1), null);
});

test('leave: the hook prunes once an hour', async () => {
  const guild = roleGuild();
  saveRow(guild, 'OLD', { roleIds: ['A'], leftAt: new Date(Date.now() - 200 * ROLEPERSIST.DAY_MS) });
  await onLeave(ctxFor(guild, fakeMember(guild, 'U1', { roles: ['A'] }), SETTINGS()));
  assert.equal(rowOf(guild, 'OLD'), null);
  saveRow(guild, 'OLD2', { roleIds: ['A'], leftAt: new Date(Date.now() - 200 * ROLEPERSIST.DAY_MS) });
  await onLeave(ctxFor(guild, fakeMember(guild, 'U2', { roles: ['A'] }), SETTINGS()));
  assert.ok(rowOf(guild, 'OLD2'), 'the second leave within the hour does not prune');
});

// ── Rejoin ────────────────────────────────────────────────────────────────────

test('rejoin: gives the safe roles back one single-role call each with an audit reason, then deletes the row', async () => {
  const guild = roleGuild();
  const settings = SETTINGS({ rolePersistIgnoredRoleIds: ['IGN'] });
  saveRow(guild, 'U1', { roleIds: ['A', 'B', 'ADMINR', 'HIGH', 'BOTROLE', 'IGN', 'GONE'], nickname: 'Nick' });
  const m = fakeMember(guild, 'U1');
  await onJoin(ctxFor(guild, m, settings));

  assert.deepEqual(m.adds.map((a) => a.ids), ['A', 'B'], 'one role id per roles.add call, never an array');
  assert.ok(m.adds.every((a) => a.reason === AUDIT_REASON));
  assert.deepEqual(m.nicks, [{ n: 'Nick', reason: AUDIT_REASON }]);
  assert.equal(rowOf(guild, 'U1'), null, 'the row is deleted after the restore');
  assert.equal(guild.fetchCalls.length, 0, 'no member fetch is needed');
});

test('rejoin: roles go one at a time and a role another hook added meanwhile is never removed', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A', 'B', 'C'] });
  const m = fakeMember(guild, 'U1');
  // The server-side role set: only single-role PUTs may touch it, and they only add.
  const server = new Set([guild.id]);
  let inflight = 0;
  let maxInflight = 0;
  m.roles.add = async (id, reason) => {
    assert.equal(typeof id, 'string', 'a single-role PUT, not a PATCH of the whole list');
    inflight++;
    maxInflight = Math.max(maxInflight, inflight);
    m.adds.push({ ids: id, reason });
    await new Promise((r) => setImmediate(r));
    if (id === 'A') server.add('AUTOROLE'); // the captcha's autorole lands while the restore is under way
    server.add(id);
    inflight--;
  };
  const res = await restoreMember(m, SETTINGS());
  assert.equal(res.status, 'restored');
  assert.deepEqual(res.given, ['A', 'B', 'C']);
  assert.equal(maxInflight, 1, 'sequential');
  assert.deepEqual([...server].sort(), [guild.id, 'A', 'AUTOROLE', 'B', 'C'].sort(), 'the autorole survived');
});

test('rejoin: one role is a single call and needs no refresh', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A'] });
  const m = fakeMember(guild, 'U1');
  const res = await restoreMember(m, SETTINGS());
  assert.equal(res.status, 'restored');
  assert.deepEqual(m.adds.map((a) => a.ids), ['A']);
  assert.equal(guild.fetchCalls.length, 0);
});

test('rejoin: roles the member already has are skipped, and an empty plan still clears the row', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A'] });
  const m = fakeMember(guild, 'U1', { roles: ['A'] });
  const res = await restoreMember(m, SETTINGS());
  assert.equal(res.status, 'restored');
  assert.equal(m.adds.length, 0);
  assert.equal(rowOf(guild, 'U1'), null);
});

test('rejoin: a stale row is deleted and ignored', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A'], nickname: 'Old', leftAt: new Date(Date.now() - 91 * ROLEPERSIST.DAY_MS) });
  const m = fakeMember(guild, 'U1');
  const res = await restoreMember(m, SETTINGS({ rolePersistDays: 90 }));
  assert.equal(res.status, 'stale');
  assert.equal(m.adds.length, 0);
  assert.equal(m.nicks.length, 0);
  assert.equal(rowOf(guild, 'U1'), null);
});

test('rejoin: no row means nothing happens', async () => {
  const guild = roleGuild();
  const m = fakeMember(guild, 'U1');
  assert.equal((await restoreMember(m, SETTINGS())).status, 'none');
  assert.equal(m.adds.length, 0);
});

test('rejoin: without Manage Roles nothing is given and the row is kept', async () => {
  const guild = roleGuild({ botPerms: ['ManageNicknames'] });
  saveRow(guild, 'U1', { roleIds: ['A', 'B'], nickname: 'Nick' });
  const m = fakeMember(guild, 'U1');
  const res = await restoreMember(m, SETTINGS());
  assert.equal(res.status, 'blocked');
  assert.equal(m.adds.length, 0);
  assert.ok(rowOf(guild, 'U1'));
});

test('rejoin: a failing roles.add keeps the row; one unusable role does not stop the others', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A', 'B'] });
  const down = fakeMember(guild, 'U1');
  down.addError = () => { throw discordErr(500); };
  assert.equal((await restoreMember(down, SETTINGS())).status, 'failed');
  assert.ok(rowOf(guild, 'U1'), 'kept for a later attempt');

  const partial = fakeMember(guild, 'U1');
  guild.memberStore.set('U1', partial);
  partial.addError = (id) => { if (id === 'A') throw discordErr(50013); };
  const res = await restoreMember(partial, SETTINGS());
  assert.equal(res.status, 'restored');
  assert.deepEqual(res.given, ['B']);
  assert.deepEqual(partial.adds.map((a) => a.ids), ['A', 'B'], 'B was still tried after A failed');
  assert.equal(rowOf(guild, 'U1'), null);

  // A role that went missing in the meantime (Unknown Role) is skipped the same way.
  saveRow(guild, 'U1', { roleIds: ['A', 'B', 'C'] });
  const gone = fakeMember(guild, 'U1');
  gone.addError = (id) => { if (id === 'B') throw discordErr(10011); };
  const res2 = await restoreMember(gone, SETTINGS());
  assert.equal(res2.status, 'restored');
  assert.deepEqual(res2.given, ['A', 'C']);
});

test('rejoin: the member leaving during the restore stops it', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A', 'B'] });
  const m = fakeMember(guild, 'U1');
  m.addError = () => { throw discordErr(10007); };
  assert.equal((await restoreMember(m, SETTINGS())).status, 'left');
  assert.deepEqual(m.adds.map((a) => a.ids), ['A'], 'stops at the first call that says they are gone');
  assert.ok(rowOf(guild, 'U1'));
});

test('rejoin: an unexpected API error part-way keeps the row for a later attempt', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A', 'B'] });
  const m = fakeMember(guild, 'U1');
  m.addError = (id) => { if (id === 'B') throw discordErr(500); };
  assert.equal((await restoreMember(m, SETTINGS())).status, 'failed');
  assert.deepEqual(m.adds.map((a) => a.ids), ['A', 'B']);
  assert.ok(rowOf(guild, 'U1'));
});

test('nicknames: given back only when on, free, and the bot can manage the member', async () => {
  const guild = roleGuild();
  saveRow(guild, 'ON', { roleIds: [], nickname: 'Nick' });
  const on = fakeMember(guild, 'ON');
  await restoreMember(on, SETTINGS());
  assert.deepEqual(on.nicks.map((n) => n.n), ['Nick']);

  saveRow(guild, 'OFF', { roleIds: [], nickname: 'Nick' });
  const off = fakeMember(guild, 'OFF');
  await restoreMember(off, SETTINGS({ rolePersistNicknames: false }));
  assert.equal(off.nicks.length, 0);

  saveRow(guild, 'TAKEN', { roleIds: [], nickname: 'Nick' });
  const taken = fakeMember(guild, 'TAKEN', { nickname: 'Chosen' });
  await restoreMember(taken, SETTINGS());
  assert.equal(taken.nicks.length, 0, 'a nickname they already set is kept');

  saveRow(guild, 'HIGHER', { roleIds: [], nickname: 'Nick' });
  const higher = fakeMember(guild, 'HIGHER', { manageable: false });
  await restoreMember(higher, SETTINGS());
  assert.equal(higher.nicks.length, 0, 'not manageable');
  assert.equal(rowOf(guild, 'HIGHER'), null, 'the row still clears');

  const noPerm = roleGuild({ botPerms: ['ManageRoles'] });
  saveRow(noPerm, 'U9', { roleIds: [], nickname: 'Nick' });
  const m = fakeMember(noPerm, 'U9');
  await restoreMember(m, SETTINGS());
  assert.equal(m.nicks.length, 0, 'no Manage Nicknames');
});

test('a concurrent join and update restore once', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A'] });
  const m = fakeMember(guild, 'U1');
  await Promise.all([restoreMember(m, SETTINGS()), restoreMember(m, SETTINGS())]);
  assert.equal(m.adds.length, 1);
});

// ── Gates ─────────────────────────────────────────────────────────────────────

test('Rules Screening: nothing is restored while pending; the update that clears it restores', async () => {
  const guild = roleGuild();
  const settings = SETTINGS();
  saveRow(guild, 'U1', { roleIds: ['A', 'B'], nickname: 'Nick' });
  const m = fakeMember(guild, 'U1', { pending: true });
  await onJoin(ctxFor(guild, m, settings));
  assert.equal(m.adds.length, 0);
  assert.equal(m.nicks.length, 0);
  assert.ok(rowOf(guild, 'U1'));
  assert.equal(isWaiting(guild.id, 'U1'), true);

  const oldMember = { pending: true, roles: { cache: new Map() } };
  m.pending = false;
  const upd = ctxFor(guild, m, settings, { oldMember, newMember: m, oldKnown: true });
  assert.equal(gateMayHaveCleared(upd), true);
  await onUpdate(upd);
  assert.deepEqual(m.adds.map((a) => a.ids), ['A', 'B']);
  assert.equal(rowOf(guild, 'U1'), null);
  assert.equal(isWaiting(guild.id, 'U1'), false);
});

test('captcha: never bypassed; the roles come once the captcha role is held', async () => {
  const guild = roleGuild();
  const settings = SETTINGS({ captchaEnabled: true, captchaRoleId: 'CAP' });
  saveRow(guild, 'U1', { roleIds: ['A', 'CAP', 'B'] });
  const m = fakeMember(guild, 'U1');
  await onJoin(ctxFor(guild, m, settings));
  assert.equal(m.adds.length, 0, 'unverified: no roles, and above all not the captcha role');
  assert.ok(rowOf(guild, 'U1'));
  assert.equal(isWaiting(guild.id, 'U1'), true);

  // Still unverified: another update changes nothing.
  await onUpdate(ctxFor(guild, m, settings, { oldMember: m, newMember: m, oldKnown: true }));
  assert.equal(m.adds.length, 0);

  // The captcha gives the verified role: the update passes the filter and the rest is restored.
  const oldMember = { pending: false, roles: { cache: new Map(m.roles.cache) } };
  m.roles.cache.set('CAP', guild.roles.cache.get('CAP'));
  const upd = ctxFor(guild, m, settings, { oldMember, newMember: m, oldKnown: true });
  assert.equal(gateMayHaveCleared(upd), true);
  await onUpdate(upd);
  assert.deepEqual(m.adds.map((a) => a.ids).sort(), ['A', 'B'], 'the captcha role is never given');
  assert.equal(rowOf(guild, 'U1'), null);
});

test('captcha: the join hook flag gates even before the role check', async () => {
  const guild = roleGuild();
  saveRow(guild, 'U1', { roleIds: ['A'] });
  const m = fakeMember(guild, 'U1');
  await onJoin(ctxFor(guild, m, SETTINGS(), { state: { captchaPending: true } }));
  assert.equal(m.adds.length, 0);
  assert.ok(rowOf(guild, 'U1'));
});

test('captcha: the plan never lists the captcha role while captcha is on', () => {
  const guild = roleGuild();
  const settings = SETTINGS({ captchaEnabled: true, captchaRoleId: 'CAP' });
  const { give } = planRoles(guild, ['CAP', 'A'], settings);
  assert.deepEqual(give.map((r) => r.id), ['A']);
});

test('update filter: only gate-clearing updates are looked at', async () => {
  const guild = roleGuild();
  await seedSettings(guild.id, { captchaEnabled: true, captchaRoleId: 'CAP' });
  await getSettings(guild.id); // memoise, as any hook of the same guild would
  const base = (extra) => ({ guildId: guild.id, oldKnown: true, ...extra });
  const mk = (roles, extra = {}) => ({ roles: { cache: new Map(roles.map((r) => [r, {}])) }, pending: false, user: { bot: false }, id: 'U1', ...extra });

  // Role changes and nickname changes that are not the captcha role cost nothing.
  assert.equal(gateMayHaveCleared(base({ oldMember: mk(['A']), newMember: mk(['A', 'B']) })), false);
  assert.equal(gateMayHaveCleared(base({ oldMember: mk(['A']), newMember: mk(['A']) })), false);
  // The captcha role gained.
  assert.equal(gateMayHaveCleared(base({ oldMember: mk(['A']), newMember: mk(['A', 'CAP']) })), true);
  // Rules Screening passed.
  assert.equal(gateMayHaveCleared(base({ oldMember: mk([], { pending: true }), newMember: mk([]) })), true);
  // Bots never.
  assert.equal(gateMayHaveCleared(base({ oldMember: mk([]), newMember: mk(['CAP'], { user: { bot: true } }) })), false);
  // A member we are waiting for.
  svc.markWaiting(guild.id, 'U1');
  assert.equal(gateMayHaveCleared(base({ oldMember: mk(['A']), newMember: mk(['A']) })), true);
});

test('update filter: an uncached old member is only looked at for recent joiners, once a minute', () => {
  const guild = fakeGuild();
  const upd = (id, joinedAgoMs, extra = {}) => ({
    guildId: guild.id, oldKnown: false, oldMember: { partial: true },
    newMember: { id, pending: false, user: { bot: false }, joinedTimestamp: Date.now() - joinedAgoMs, ...extra },
  });
  assert.equal(gateMayHaveCleared(upd('P1', 60_000)), true);
  assert.equal(gateMayHaveCleared(upd('P1', 60_000)), false, 'throttled');
  assert.equal(gateMayHaveCleared(upd('P2', 2 * ROLEPERSIST.RECENT_JOIN_MS)), false, 'joined long ago');
  assert.equal(gateMayHaveCleared(upd('P3', 60_000, { pending: true })), false, 'still pending');
});

test('waiting set: bounded and expiring', () => {
  const now = Date.now();
  svc.markWaiting('GW', 'U1', now);
  assert.equal(isWaiting('GW', 'U1', now + 1), true);
  assert.equal(isWaiting('GW', 'U1', now + ROLEPERSIST.WAIT_TTL_MS + 1), false);
  for (let i = 0; i < ROLEPERSIST.WAIT_MAX + 10; i++) svc.markWaiting('GW', `M${i}`, now);
  assert.equal(isWaiting('GW', 'M0', now), false, 'the oldest entry went');
  assert.equal(isWaiting('GW', `M${ROLEPERSIST.WAIT_MAX + 9}`, now), true);
});

// ── roleDelete ────────────────────────────────────────────────────────────────

test('roleDelete: the role leaves the ignore list; the filter skips other roles once settings are memoised', async () => {
  const guild = fakeGuild();
  await seedSettings(guild.id, { rolePersistIgnoredRoleIds: ['R1', 'R2'] });
  assert.equal(roleDeleteFilter({ guildId: guild.id, role: { id: 'R3' } }), true, 'no memo yet: the handler decides');
  const settings = await getSettings(guild.id);
  assert.equal(roleDeleteFilter({ guildId: guild.id, role: { id: 'R3' } }), false);
  assert.equal(roleDeleteFilter({ guildId: guild.id, role: { id: 'R1' } }), true);

  await onRoleDelete({ guildId: guild.id, role: { id: 'R3' }, getSettings: async () => settings });
  assert.deepEqual(settingsRows.get(guild.id).rolePersistIgnoredRoleIds, ['R1', 'R2']);
  await onRoleDelete({ guildId: guild.id, role: { id: 'R1' }, getSettings: async () => settings });
  assert.deepEqual(settingsRows.get(guild.id).rolePersistIgnoredRoleIds, ['R2']);
});

// ── Declarations ──────────────────────────────────────────────────────────────

test('plugin declaration and hook order', async () => {
  assert.equal(plugin.id, 'role-persist');
  assert.equal(plugin.module, 'rolepersist');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id]), [
    ['guildMemberRemove', 'role-persist'], ['guildMemberAdd', 'role-persist'],
    ['guildMemberUpdate', 'role-persist'], ['roleDelete', 'role-persist'],
  ]);
  assert.deepEqual(plugin.hooks.filter((h) => h.always).map((h) => h.event), ['roleDelete']);
  assert.deepEqual(plugin.jobs, []);
  assert.deepEqual(plugin.components, []);
  for (const event of ['guildMemberRemove', 'guildMemberAdd', 'guildMemberUpdate', 'roleDelete']) {
    assert.ok(HOOK_ORDER[event].includes('role-persist'), event);
  }
  for (const before of ['welcome', 'captcha', 'moderation', 'antiraid']) {
    assert.ok(HOOK_ORDER.guildMemberAdd.indexOf('role-persist') > HOOK_ORDER.guildMemberAdd.indexOf(before), `after ${before}`);
  }
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'role-persist'), 'collectPlugins keeps it');
  assert.deepEqual(errors.filter((e) => String(e?.message ?? e).includes('role-persist')), []);
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/rolepersist payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'rolepersist');
  assert.equal(json.default_member_permissions, '32');
  assert.deepEqual(json.contexts, [0]);
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['status', 1], ['ignore', 2], ['view', 1], ['clear', 1]]);
  const byName = Object.fromEntries(json.options.map((o) => [o.name, o]));
  assert.deepEqual(byName.ignore.options.map((o) => [o.name, o.type]), [['add', 1], ['remove', 1]]);
  for (const sub of byName.ignore.options) assert.deepEqual(sub.options.map((o) => [o.name, o.type, !!o.required]), [['role', 8, true]]);
  assert.deepEqual(byName.view.options.map((o) => [o.name, o.type, !!o.required]), [['user', 6, true]]);
  assert.deepEqual(byName.clear.options.map((o) => [o.name, o.type, !!o.required]), [['user', 6, true]]);
  assert.ok(charCount(json) <= 1000, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'rolepersist');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
});

// ── /rolepersist replies ──────────────────────────────────────────────────────

function fakeInteraction(guild, { group = null, sub, role = null, user = null } = {}) {
  const it = {
    guild, guildId: guild.id, user: { id: 'ADMIN' }, createdTimestamp: Date.now(),
    deferred: false, replied: false, ephemeral: undefined, payloads: [],
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      getRole: () => role,
      getUser: () => user,
    },
    async deferReply() { it.deferred = true; it.ephemeral = false; },
    async editReply(p) { it.payloads.push(p); return {}; },
  };
  return it;
}

/** Everything the last reply says, as one searchable string. */
function said(it) {
  const p = it.payloads.at(-1);
  assert.ok(p, 'the command replied');
  assert.deepEqual(p.allowedMentions, { parse: [] });
  return (p.embeds ?? []).map((e) => {
    const j = e.toJSON();
    return [j.title, j.description, ...(j.fields ?? []).flatMap((f) => [f.name, f.value]), j.footer?.text].filter(Boolean).join('\n');
  }).join('\n');
}

test('/rolepersist status', async () => {
  const guild = roleGuild({ botPerms: ['ManageRoles'] });
  await seedSettings(guild.id, { rolePersistEnabled: true, rolePersistDays: 30, rolePersistIgnoredRoleIds: ['A'] });
  saveRow(guild, 'U1', { roleIds: ['A'] });
  saveRow(guild, 'U2', { roleIds: ['B'] });
  const it = fakeInteraction(guild, { sub: 'status' });
  await cmd.execute(it);
  const text = said(it);
  assert.match(text, /✅ On/);
  assert.match(text, /30 days/);
  assert.match(text, /Saved members\n2/);
  assert.match(text, /<@&A>/);
  assert.match(text, /Manage Nicknames/, 'a missing permission is reported');

  await seedSettings(guild.id, { rolePersistEnabled: false });
  const off = fakeInteraction(guild, { sub: 'status' });
  await cmd.execute(off);
  assert.match(said(off), /\/module enable rolepersist/);
});

test('/rolepersist ignore add and remove', async () => {
  const guild = roleGuild();
  await seedSettings(guild.id, { rolePersistEnabled: true });
  const run = async (sub, role) => {
    const it = fakeInteraction(guild, { group: 'ignore', sub, role });
    await cmd.execute(it);
    return said(it);
  };
  assert.match(await run('add', { id: 'A' }), /Role Ignored/);
  assert.deepEqual(settingsRows.get(guild.id).rolePersistIgnoredRoleIds, ['A']);
  assert.match(await run('add', { id: 'A' }), /already on the never-give-back list/);
  assert.match(await run('add', { id: guild.id }), /@everyone/);
  assert.match(await run('add', { id: 'BOTROLE', managed: true }), /managed by an integration/);
  assert.deepEqual(settingsRows.get(guild.id).rolePersistIgnoredRoleIds, ['A']);
  assert.match(await run('remove', { id: 'A' }), /Role No Longer Ignored/);
  assert.deepEqual(settingsRows.get(guild.id).rolePersistIgnoredRoleIds, []);
  assert.match(await run('remove', { id: 'A' }), /is not on the never-give-back list/);

  const full = Array.from({ length: LIMITS.ROLEPERSIST_IGNORED_ROLES }, (_, i) => `R${i}`);
  await seedSettings(guild.id, { rolePersistIgnoredRoleIds: full });
  assert.match(await run('add', { id: 'NEWONE' }), /List Full/);
  assert.equal(settingsRows.get(guild.id).rolePersistIgnoredRoleIds.length, LIMITS.ROLEPERSIST_IGNORED_ROLES);

  await seedSettings(guild.id, { rolePersistEnabled: false });
  assert.match(await run('add', { id: 'A' }), /\/module enable rolepersist/, 'tells the admin the module is off');
});

test('/rolepersist view shows what is saved and what would be skipped', async () => {
  const guild = roleGuild();
  await seedSettings(guild.id, { rolePersistDays: 90, rolePersistIgnoredRoleIds: ['IGN'] });
  saveRow(guild, 'U1', { roleIds: ['A', 'HIGH', 'IGN', 'GONE'], nickname: 'Nick' });
  const it = fakeInteraction(guild, { sub: 'view', user: { id: 'U1' } });
  await cmd.execute(it);
  const text = said(it);
  assert.match(text, /<@U1>/);
  assert.match(text, /✅ <@&A>/);
  assert.match(text, /⚠️ <@&HIGH> — That role is above my highest role/);
  assert.match(text, /⚠️ <@&IGN> — The role is on the never-give-back list/);
  assert.match(text, /deleted role \(`GONE`\)/);
  assert.match(text, /`Nick`/);
  assert.match(text, /Expires/);

  const none = fakeInteraction(guild, { sub: 'view', user: { id: 'NOBODY' } });
  await cmd.execute(none);
  assert.match(said(none), /Nothing is saved for <@NOBODY>/);

  saveRow(guild, 'OLD', { roleIds: ['A'], leftAt: new Date(Date.now() - 200 * ROLEPERSIST.DAY_MS) });
  const old = fakeInteraction(guild, { sub: 'view', user: { id: 'OLD' } });
  await cmd.execute(old);
  assert.match(said(old), /Expired/);
});

test('/rolepersist clear forgets a member and stops a waiting restore', async () => {
  const guild = roleGuild();
  await seedSettings(guild.id);
  saveRow(guild, 'U1', { roleIds: ['A'] });
  svc.markWaiting(guild.id, 'U1');
  const it = fakeInteraction(guild, { sub: 'clear', user: { id: 'U1' } });
  await cmd.execute(it);
  assert.match(said(it), /will not get their old roles back/);
  assert.equal(rowOf(guild, 'U1'), null);
  assert.equal(isWaiting(guild.id, 'U1'), false);
  assert.equal(await loadRow(guild.id, 'U1'), null);

  const again = fakeInteraction(guild, { sub: 'clear', user: { id: 'U1' } });
  await cmd.execute(again);
  assert.match(said(again), /Nothing was saved for <@U1>/);
});
