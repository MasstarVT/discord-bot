// Security regression tests for two low findings. Offline: no Discord, Postgres or Redis; the Prisma delegates are
// replaced with in-memory stubs.
//  1. Escalation state lives in Infraction.escalationTierId (set only by runEscalation). "[Escalation T…]" text typed
//     into a warn, /case reason, /unmute or /unban reason must not mark a tier as fired, and editing a real escalation
//     case's reason must not make it fire again.
//  2. Role Persistence must not give back a mute, event or birthday role whose lifecycle ended while the member was away.
import { test, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back and Postgres is never reached (the delegates below are stubs).
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';
delete process.env.BOT_OWNER_ID;

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const { PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const svc = await import('../src/services/moderationService.js');
const warnCmd = await import('../src/commands/moderation/warn.js');
const caseCmd = await import('../src/commands/moderation/case.js');
const unmuteCmd = await import('../src/commands/moderation/unmute.js');
const unbanCmd = await import('../src/commands/moderation/unban.js');
const rp = await import('../src/plugins/role-persist.js');
const { snapshotMember } = await import('../src/services/rolePersistService.js');
const events = await import('../src/services/eventService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { invalidateSettings } = await import('../src/services/settingsService.js');

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory tables ──────────────────────────────────────────────────────────

const settingsRows = new Map();
const infractions = [];
const persisted = new Map(); // `${guildId}:${userId}` -> PersistedMember row
const guildEvents = []; // { id, guildId, roleId, status }
const rsvps = []; // { eventId, userId, roleGranted }

const real = {
  guildSettings: prisma.guildSettings,
  infraction: prisma.infraction,
  scheduledJob: prisma.scheduledJob,
  persistedMember: prisma.persistedMember,
  guildEvent: prisma.guildEvent,
  eventRsvp: prisma.eventRsvp,
  $transaction: prisma.$transaction,
};
after(async () => {
  Object.assign(prisma, real);
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

/** Prisma-style where matching for the operators these code paths use; anything else throws, so a stub never hides a filter. */
const FIELDS = new Set(['id', 'guildId', 'targetUserId', 'moderatorId', 'type', 'active', 'reason', 'expiresAt', 'createdAt', 'escalationTierId', 'caseId']);
function matches(row, where = {}) {
  for (const [key, cond] of Object.entries(where)) {
    if (key === 'OR') {
      if (!cond.some((c) => matches(row, c))) return false;
      continue;
    }
    if (!FIELDS.has(key)) throw new Error(`stub: unsupported where key "${key}"`);
    const v = row[key] ?? null;
    if (cond !== null && typeof cond === 'object' && !(cond instanceof Date)) {
      for (const [op, arg] of Object.entries(cond)) {
        if (op === 'in') { if (!arg.includes(v)) return false; }
        else if (op === 'not') { if (arg === null ? v === null : v === arg) return false; }
        else if (op === 'gte') { if (v === null || !(v >= arg)) return false; }
        else if (op === 'gt') { if (v === null || !(v > arg)) return false; }
        else if (op === 'startsWith') { if (!String(v ?? '').startsWith(arg)) return false; }
        else throw new Error(`stub: unsupported operator "${op}"`);
      }
    } else if (v !== (cond ?? null)) return false;
  }
  return true;
}

let nextId = 1;
const copy = (r) => ({ ...r });
const byCase = (where) => infractions.find((r) => r.guildId === where.guildId_caseId.guildId && r.caseId === where.guildId_caseId.caseId) ?? null;
const infractionStub = {
  aggregate: async ({ where }) => {
    const ids = infractions.filter((r) => matches(r, where)).map((r) => r.caseId);
    return { _max: { caseId: ids.length ? Math.max(...ids) : null } };
  },
  create: async ({ data }) => {
    const row = { id: nextId++, proofUrl: null, escalationTierId: null, createdAt: new Date(), updatedAt: new Date(), ...data };
    infractions.push(row);
    return copy(row);
  },
  findMany: async ({ where, orderBy, take }) => {
    let list = infractions.filter((r) => matches(r, where));
    if (orderBy?.createdAt === 'desc') list = [...list].sort((a, b) => b.createdAt - a.createdAt);
    return (take ? list.slice(0, take) : list).map(copy);
  },
  findFirst: async ({ where }) => {
    const r = infractions.find((x) => matches(x, where));
    return r ? copy(r) : null;
  },
  findUnique: async ({ where }) => {
    const r = where.guildId_caseId ? byCase(where) : infractions.find((x) => x.id === where.id);
    return r ? copy(r) : null;
  },
  count: async ({ where }) => infractions.filter((r) => matches(r, where)).length,
  update: async ({ where, data }) => {
    const r = byCase(where);
    if (!r) throw Object.assign(new Error('not found'), { code: 'P2025' });
    Object.assign(r, data);
    return copy(r);
  },
  updateMany: async ({ where, data }) => {
    const list = infractions.filter((r) => matches(r, where));
    for (const r of list) Object.assign(r, data);
    return { count: list.length };
  },
};
const jobStub = { deleteMany: async () => ({ count: 0 }), create: async ({ data }) => ({ id: 'job', runAt: data.runAt }), update: async ({ data }) => ({ id: 'job', runAt: data.runAt }) };
const settingsStub = {
  findUnique: async ({ where }) => (settingsRows.has(where.guildId) ? { ...settingsRows.get(where.guildId) } : null),
  upsert: async ({ where, update, create }) => {
    const row = { ...(settingsRows.get(where.guildId) ?? { guildId: where.guildId, ...create }), ...update };
    settingsRows.set(where.guildId, row);
    return { ...row };
  },
};
const rowKey = (g, u) => `${g}:${u}`;
const persistedStub = {
  findUnique: async ({ where }) => {
    const { guildId, userId } = where.guildId_userId;
    const r = persisted.get(rowKey(guildId, userId));
    return r ? { ...r, roleIds: [...r.roleIds] } : null;
  },
  upsert: async ({ where, create, update }) => {
    const { guildId, userId } = where.guildId_userId;
    const key = rowKey(guildId, userId);
    const row = persisted.has(key) ? { ...persisted.get(key), ...update } : { ...create };
    persisted.set(key, row);
    return { ...row };
  },
  deleteMany: async ({ where }) => {
    let count = 0;
    for (const [key, r] of persisted) {
      if (r.guildId !== where.guildId || (where.userId && r.userId !== where.userId)) continue;
      if (where.leftAt?.lt && !(new Date(r.leftAt) < where.leftAt.lt)) continue;
      persisted.delete(key);
      count++;
    }
    return { count };
  },
};
const rsvpMatches = (r, where) => {
  for (const [k, c] of Object.entries(where)) {
    if (!['eventId', 'userId', 'roleGranted'].includes(k)) throw new Error(`stub: unsupported eventRsvp where key "${k}"`);
    if (c !== null && typeof c === 'object') {
      if ('in' in c && !c.in.includes(r[k])) return false;
      if ('gt' in c && !(r[k] > c.gt)) return false;
      if (!('in' in c) && !('gt' in c)) throw new Error('stub: unsupported eventRsvp operator');
    } else if (r[k] !== c) return false;
  }
  return true;
};
const guildEventStub = {
  findMany: async ({ where, select, take }) => {
    for (const k of Object.keys(where)) if (!['guildId', 'roleId', 'rsvps'].includes(k)) throw new Error(`stub: unsupported guildEvent where key "${k}"`);
    const some = where.rsvps.some;
    const list = guildEvents.filter((e) => e.guildId === where.guildId && where.roleId.in.includes(e.roleId)
      && rsvps.some((r) => r.eventId === e.id && rsvpMatches(r, some)));
    return list.slice(0, take).map((e) => Object.fromEntries(Object.keys(select).map((k) => [k, e[k]])));
  },
  findFirst: async () => null,
};
const eventRsvpStub = {
  findMany: async ({ where, orderBy, take, select }) => {
    const list = rsvps.filter((r) => rsvpMatches(r, where)).sort((a, b) => (a.userId < b.userId ? -1 : 1));
    return list.slice(0, take).map((r) => Object.fromEntries(Object.keys(select).map((k) => [k, r[k]])));
  },
  updateMany: async ({ where, data }) => {
    const list = rsvps.filter((r) => rsvpMatches(r, where));
    for (const r of list) Object.assign(r, data);
    return { count: list.length };
  },
};

prisma.guildSettings = settingsStub;
prisma.infraction = infractionStub;
prisma.scheduledJob = jobStub;
prisma.persistedMember = persistedStub;
prisma.guildEvent = guildEventStub;
prisma.eventRsvp = eventRsvpStub;
prisma.$transaction = async (fn) => fn({
  $executeRaw: async () => 0,
  $queryRaw: async () => [],
  guildSettings: settingsStub,
  infraction: infractionStub,
  scheduledJob: jobStub,
});

beforeEach(() => {
  infractions.length = 0;
  persisted.clear();
  guildEvents.length = 0;
  rsvps.length = 0;
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

const BOT = '100000000000000001';
const OWNER = '100000000000000002';
const ALICE = '100000000000000003';
const MOD = '100000000000000005';
const bits = (...names) => new PermissionsBitField(names.map((n) => PermissionFlagsBits[n]));

function mkUser(id) {
  const user = { id, username: `user${id.slice(-3)}`, tag: `user${id.slice(-3)}`, bot: false, displayAvatarURL: () => 'https://cdn.example/a.png' };
  user.send = async () => ({ delete: async () => {} });
  return user;
}

function mkRole(id, position = 1) {
  return { id, position, managed: false, mentionable: false, permissions: new PermissionsBitField() };
}

function mkMember(guild, id, { position = 1, perms = [], roles = [] } = {}) {
  const member = {
    id, guild, user: mkUser(id), permissions: bits(...perms), pending: false, nickname: null, partial: false,
    moderatable: true, kickable: true, bannable: true, manageable: true, joinedTimestamp: Date.now(),
    adds: [], timeouts: [],
    isCommunicationDisabled: () => false,
    timeout: async (ms, reason) => { member.timeouts.push([ms, reason]); },
    kick: async () => {},
    setNickname: async () => {},
    roles: {
      cache: new Map(roles.map((r) => [r, guild.roles.cache.get(r) ?? { id: r }])),
      highest: { position, comparePositionTo(other) { return this.position - other.position; } },
      add: async (role, reason) => {
        const id2 = typeof role === 'string' ? role : role.id;
        member.adds.push({ id: id2, reason });
        member.roles.cache.set(id2, guild.roles.cache.get(id2) ?? { id: id2 });
      },
      remove: async (role) => { member.roles.cache.delete(typeof role === 'string' ? role : role.id); },
    },
  };
  guild.members.cache.set(id, member);
  return member;
}

let guildCounter = 0;
/** A guild with the bot, a moderator (above the target) and the given roles; one id per scenario. */
function mkGuild({ roles = [] } = {}) {
  const guild = { id: `9560${String(++guildCounter).padStart(14, '0')}`, ownerId: OWNER, available: true, name: 'Test Guild' };
  guild.members = {
    cache: new Map(),
    me: null,
    fetchMe: async () => guild.members.me,
    fetch: async (uid) => {
      const m = guild.members.cache.get(uid);
      if (!m) throw Object.assign(new Error('Unknown Member'), { code: 10007 });
      return m;
    },
  };
  guild.roles = { cache: new Map(roles.map((r) => [r.id, r])), fetch: async () => new Map(guild.roles.cache) };
  guild.channels = { cache: new Map(), fetch: async () => null };
  guild.client = { user: { id: BOT, username: 'Bot', tag: 'Bot#0' }, users: { cache: new Map(), fetch: async (uid) => mkUser(uid) } };
  guild.banned = new Set();
  guild.bans = {
    fetch: async ({ user }) => {
      if (guild.banned.has(user)) return { user: { id: user, tag: 'banned' } };
      throw Object.assign(new Error('Unknown Ban'), { code: 10026 });
    },
    create: async (uid) => { guild.banned.add(uid); },
    remove: async (uid) => { guild.banned.delete(uid); },
  };
  guild.members.me = mkMember(guild, BOT, { position: 100, perms: ['BanMembers', 'KickMembers', 'ModerateMembers', 'ManageRoles', 'ViewAuditLog'] });
  mkMember(guild, MOD, { position: 50, perms: ['BanMembers', 'KickMembers', 'ModerateMembers'] });
  return guild;
}

const T1 = { id: 1, warnCount: 3, withinSeconds: 604_800, action: 'TIMEOUT', durationSeconds: 3600, countFrom: null };
const HOUR_MS = 3_600_000;

/** Seeds the guild's GuildSettings row and drops the memo, so the next read loads it. */
async function seed(guildId, over = {}) {
  settingsRows.set(guildId, {
    guildId, modEnabled: true, modDmTargets: false, modVisibility: 'PRIVATE', modLogChannelId: null, modRoleIds: [], adminRoleIds: [],
    immuneRoleIds: [], infractionEscalation: [], muteRoleId: null, captchaEnabled: false, captchaRoleId: null,
    rolePersistEnabled: true, rolePersistNicknames: false, rolePersistIgnoredRoleIds: [], rolePersistDays: 90,
    voiceRoleAnyRoleIds: [], voiceRoleLinks: [], birthdayRoleId: null, ...over,
  });
  await invalidateSettings(guildId);
  return settingsRows.get(guildId);
}

function mkInteraction(guild, { opts = {}, sub = null } = {}) {
  const calls = [];
  const i = {
    calls, guild, guildId: guild.id, client: guild.client, user: guild.members.cache.get(MOD).user, member: guild.members.cache.get(MOD),
    isRepliable: () => true,
    options: {
      getUser: (n) => opts[n] ?? null,
      getString: (n) => opts[n] ?? null,
      getInteger: (n) => opts[n] ?? null,
      getSubcommand: () => sub,
    },
    deferReply: async () => {},
    editReply: async (p) => { calls.push(p); },
    deleteReply: async () => {},
    followUp: async (p) => { calls.push(p); },
  };
  return i;
}
const replyText = (i) => i.calls.map((p) => p.embeds.map((e) => JSON.stringify(typeof e.toJSON === 'function' ? e.toJSON() : e)).join('')).join('');

// ── Escalation (f07) ──────────────────────────────────────────────────────────

/** A guild with Alice as a member and tier T1 (3 warnings within 7 days -> 1 hour timeout). */
async function escalationScenario(over = {}) {
  const guild = mkGuild();
  const alice = mkMember(guild, ALICE, { position: 5 });
  await seed(guild.id, { infractionEscalation: [T1], ...over });
  return { guild, alice };
}
const warn = (guild, reason) => {
  const i = mkInteraction(guild, { opts: { user: guild.members.cache.get(ALICE).user, reason } });
  return warnCmd.execute(i).then(() => i);
};
const editReason = (guild, id, reason) => caseCmd.execute(mkInteraction(guild, { sub: 'reason', opts: { id, reason } }));
const escalations = (alice) => alice.timeouts.filter(([ms]) => ms === HOUR_MS);
const FORGED = '[Escalation T1] rule 1';

test('control: three warnings fire T1 once and store the tier in the column', async () => {
  const { guild, alice } = await escalationScenario();
  await warn(guild, 'rule 1');
  await warn(guild, 'rule 2');
  const third = await warn(guild, 'rule 3');
  assert.match(replyText(third), /Escalation T1/);
  assert.equal(escalations(alice).length, 1);
  const esc = infractions.find((r) => r.type === 'TIMEOUT');
  assert.equal(esc.escalationTierId, 1);
  assert.equal(esc.moderatorId, BOT);
  assert.match(esc.reason, /^\[Escalation T1\] Reached 3 active warnings within 1 week/);
  await warn(guild, 'rule 4');
  assert.equal(escalations(alice).length, 1, 'the tier fires once per window');
});

test('a forged tag in a /warn reason does not suppress the next tier', async () => {
  const { guild, alice } = await escalationScenario();
  await warn(guild, FORGED);
  await warn(guild, 'rule 2');
  // The old prior-lookup (reason startsWith "[Escalation T") would have counted the typed tag as a fired tier.
  const old = await prisma.infraction.findMany({ where: { guildId: guild.id, targetUserId: ALICE, reason: { startsWith: '[Escalation T' } } });
  assert.equal(old.length, 1, 'the typed tag is visible to a reason-text lookup');
  assert.equal(infractions[0].escalationTierId, null);
  const third = await warn(guild, 'rule 3');
  assert.match(replyText(third), /Escalation T1/);
  assert.equal(escalations(alice).length, 1, 'T1 still fires');
});

test('a forged tag in a /case reason edit does not suppress the next tier', async () => {
  const { guild, alice } = await escalationScenario();
  await warn(guild, 'rule 1');
  await editReason(guild, 1, FORGED);
  assert.equal(infractions[0].reason, FORGED);
  assert.equal(infractions[0].escalationTierId, null);
  await warn(guild, 'rule 2');
  await warn(guild, 'rule 3');
  assert.equal(escalations(alice).length, 1);
});

test('a forged tag in an /unmute or /unban reason does not suppress the next tier', async () => {
  const { guild, alice } = await escalationScenario();
  await warn(guild, 'rule 1');
  // /unmute: a timed-out member, unmuted with the tag as the reason
  const timed = await svc.executeModAction({ guild, target: alice, moderator: guild.members.cache.get(MOD).user, action: 'TIMEOUT', durationMs: 600_000, source: 'command', reason: 'cool off' });
  assert.equal(timed.ok, true);
  alice.isCommunicationDisabled = () => true;
  const unmute = mkInteraction(guild, { opts: { user: alice.user, reason: FORGED } });
  await unmuteCmd.execute(unmute);
  assert.equal(infractions.find((r) => r.type === 'UNMUTE').reason, FORGED);
  // /unban: a banned user, unbanned with the tag as the reason
  const other = '100000000000000009';
  guild.banned.add(other);
  const unban = mkInteraction(guild, { opts: { user_id: other, reason: FORGED } });
  await unbanCmd.execute(unban);
  assert.equal(infractions.find((r) => r.type === 'UNBAN').reason, FORGED);
  // The tagged rows belong to Alice's target (UNMUTE) and are counted by a reason-text lookup, but not by the column.
  const typed = await prisma.infraction.findMany({ where: { guildId: guild.id, targetUserId: ALICE, reason: { startsWith: '[Escalation T' } } });
  assert.equal(typed.length, 1);
  await warn(guild, 'rule 2');
  await warn(guild, 'rule 3');
  assert.equal(escalations(alice).length, 1, 'T1 still fires');
});

test('editing an escalation case reason does not re-arm the tier', async () => {
  const { guild, alice } = await escalationScenario();
  await warn(guild, 'rule 1');
  await warn(guild, 'rule 2');
  await warn(guild, 'rule 3');
  const esc = infractions.find((r) => r.type === 'TIMEOUT');
  assert.equal(escalations(alice).length, 1);

  await editReason(guild, esc.caseId, 'harmless note');
  assert.equal(esc.reason, 'harmless note');
  assert.equal(esc.escalationTierId, 1, 'the structured marker survives the edit');

  await warn(guild, 'rule 4');
  assert.equal(escalations(alice).length, 1, 'T1 does not fire a second time in the same window');
});

test('only runEscalation can set escalationTierId', async () => {
  const { guild } = await escalationScenario({ infractionEscalation: [] });
  const moderator = guild.members.cache.get(MOD).user;
  const person = await svc.executeModAction({ guild, target: mkUser(ALICE), moderator, action: 'WARN', source: 'command', reason: 'x', escalationTierId: 1 });
  assert.equal(person.ok, true);
  assert.equal(person.infraction.escalationTierId, null, 'a person-driven action ignores the field');
  const dash = await svc.executeModAction({ guild, target: mkUser(ALICE), moderator, action: 'WARN', source: 'dashboard', reason: 'x', escalationTierId: 1 });
  assert.equal(dash.infraction.escalationTierId, null);
  const spoof = await svc.executeModAction({ guild, target: mkUser(ALICE), moderator, action: 'WARN', source: 'escalation', reason: 'x', escalationTierId: 1 });
  assert.equal(spoof.infraction.escalationTierId, null, 'not the bot: ignored even with source "escalation"');
  const bot = await svc.executeModAction({ guild, target: mkUser(ALICE), moderator: guild.client.user, action: 'WARN', source: 'escalation', reason: 'x', escalationTierId: 2 });
  assert.equal(bot.infraction.escalationTierId, 2);
  const bad = await svc.recordInfraction({ guildId: guild.id, targetUserId: ALICE, moderatorId: BOT, type: 'WARN', reason: 'x', escalationTierId: 'T1' });
  assert.equal(bad.escalationTierId, null, 'a non-integer is dropped');
});

test('the migration backfill recognises what runEscalation writes and nothing a person would type', async () => {
  const sql = readFileSync(resolve(ROOT, 'prisma/migrations/20261029120000_escalation_tier/migration.sql'), 'utf8');
  assert.match(sql, /ADD COLUMN\s+"escalationTierId" INTEGER;/);
  const pattern = new RegExp(/"reason" ~ '([^']+)'/.exec(sql)[1]);
  const types = /"type" IN \(([^)]+)\)/.exec(sql)[1].match(/'(\w+)'/g).map((s) => s.slice(1, -1));
  assert.deepEqual(types, ['TIMEOUT', 'MUTE', 'KICK', 'BAN']);
  assert.ok(!types.includes('WARN') && !types.includes('UNMUTE') && !types.includes('UNBAN'));

  const { guild } = await escalationScenario();
  await warn(guild, 'rule 1');
  await warn(guild, 'rule 2');
  await warn(guild, 'rule 3');
  const esc = infractions.find((r) => r.type === 'TIMEOUT');
  assert.match(esc.reason, pattern, 'the real generated reason is backfilled');
  assert.ok(types.includes(esc.type));
  assert.doesNotMatch('[Escalation T1] rule 1', pattern, 'the obvious forgery is not');
  assert.doesNotMatch('[Escalation T99999999999999] Reached 3 active warnings', pattern, 'an id too large for an integer is not');
  assert.match('[Escalation T12] Reached 1 active warning', pattern);
});

// ── Role Persistence (f10) ────────────────────────────────────────────────────

const ROLE_MUTED = 'RMUTE';
const ROLE_A = 'RA';
const ROLE_EVENT = 'REVENT';
const ROLE_BDAY = 'RBDAY';
const rpRoles = () => [mkRole(ROLE_MUTED, 2), mkRole(ROLE_A, 1), mkRole(ROLE_EVENT, 3), mkRole(ROLE_BDAY, 4)];

const ctxFor = (guild, member, settings) => ({
  client: guild.client, guild, guildId: guild.id, member, user: member.user, state: {}, getSettings: async () => settings,
});
const givenIds = (member) => member.adds.map((a) => a.id);

/** The member leaves with their roles saved, then comes back with none. */
async function leaveAndRejoin(guild, member, settings, whileAway = async () => {}) {
  await rp.onLeave(ctxFor(guild, member, settings));
  guild.members.cache.delete(member.id);
  await whileAway();
  const back = mkMember(guild, member.id, { position: 1, roles: [] });
  await svc.reapplyMuteOnJoin(ctxFor(guild, back, settings));
  const res = await rp.restoreMember(back, settings);
  return { back, res };
}

test('mute: leave while muted, /unmute while away, rejoin does not bring the mute role back (Moderation on)', async () => {
  const guild = mkGuild({ roles: rpRoles() });
  const settings = await seed(guild.id, { muteRoleId: ROLE_MUTED });
  const alice = mkMember(guild, ALICE, { position: 5, roles: [ROLE_A] });
  const muted = await svc.executeModAction({ guild, target: alice, moderator: guild.members.cache.get(MOD).user, action: 'MUTE', source: 'command', reason: 'spam' });
  assert.equal(muted.ok, true);
  assert.ok(alice.roles.cache.has(ROLE_MUTED));

  const { back, res } = await leaveAndRejoin(guild, alice, settings, async () => {
    assert.deepEqual(persisted.get(rowKey(guild.id, ALICE)).roleIds, [ROLE_A], 'the mute role is not saved while Moderation is on');
    await unmuteCmd.execute(mkInteraction(guild, { opts: { user: mkUser(ALICE), reason: 'appealed' } }));
    assert.equal(infractions.filter((r) => r.type === 'MUTE' && r.active).length, 0);
  });
  assert.equal(res.status, 'restored');
  assert.deepEqual(givenIds(back), [ROLE_A]);
  assert.ok(!back.roles.cache.has(ROLE_MUTED));
});

test('mute: a row saved before the fix, which holds the mute role, does not give it back either', async () => {
  const guild = mkGuild({ roles: rpRoles() });
  const settings = await seed(guild.id, { muteRoleId: ROLE_MUTED });
  persisted.set(rowKey(guild.id, ALICE), { guildId: guild.id, userId: ALICE, roleIds: [ROLE_MUTED, ROLE_A], nickname: null, leftAt: new Date() });
  const back = mkMember(guild, ALICE, { position: 1 });
  const res = await rp.restoreMember(back, settings);
  assert.equal(res.status, 'restored');
  assert.deepEqual(givenIds(back), [ROLE_A]);
  assert.ok(res.skipped.some((s) => s.roleId === ROLE_MUTED && /Moderation/.test(s.reason)));
});

test('mute: with Moderation off the role comes back only while a mute is still active', async () => {
  const guild = mkGuild({ roles: rpRoles() });
  const settings = await seed(guild.id, { muteRoleId: ROLE_MUTED, modEnabled: false });
  const alice = mkMember(guild, ALICE, { position: 5, roles: [ROLE_A] });
  const muted = await svc.executeModAction({ guild, target: alice, moderator: guild.members.cache.get(MOD).user, action: 'MUTE', source: 'command', reason: 'spam', durationMs: 3_600_000 });
  assert.equal(muted.ok, true);

  // Still muted on return: kept (no Moderation join hook runs in this configuration).
  const still = await leaveAndRejoin(guild, alice, settings);
  assert.equal(still.res.status, 'restored');
  assert.ok(givenIds(still.back).includes(ROLE_MUTED), 'an active mute is given back');

  // The mute expires while the member is away: the expiry job closes the row, the rejoin does not re-mute.
  persisted.clear();
  const gone = mkMember(guild, ALICE, { position: 5, roles: [ROLE_A, ROLE_MUTED] });
  await rp.onLeave(ctxFor(guild, gone, settings));
  guild.members.cache.delete(ALICE);
  const row = infractions.find((r) => r.type === 'MUTE' && r.active);
  row.expiresAt = new Date(Date.now() - 1000);
  await svc.runExpireJob({ type: 'mod.expire', guildId: guild.id, payload: { infractionId: row.id, roleId: ROLE_MUTED } }, { client: guild.client, guild });
  assert.equal(row.active, false);
  const back = mkMember(guild, ALICE, { position: 1 });
  const res = await rp.restoreMember(back, settings);
  assert.equal(res.status, 'restored');
  assert.deepEqual(givenIds(back), [ROLE_A]);
  assert.ok(res.skipped.some((s) => s.roleId === ROLE_MUTED));
});

const flagOf = (eventId, userId) => rsvps.find((r) => r.eventId === eventId && r.userId === userId)?.roleGranted;

/** The event ends while the member is away: Events strips the role from the members it can reach (the real stripRoles). */
async function endEvent(guild, event) {
  event.status = 'ENDED';
  await events.stripRoles({ guilds: { cache: new Map([[guild.id, guild]]) } }, { ...event, title: 'Game night' });
}

test('event role: a member the event granted it to, who left before the strip, does not get it back', async () => {
  const guild = mkGuild({ roles: rpRoles() });
  const settings = await seed(guild.id);
  const event = { id: 'E1', guildId: guild.id, roleId: ROLE_EVENT, status: 'ACTIVE' };
  guildEvents.push(event);
  rsvps.push({ eventId: 'E1', userId: ALICE, roleGranted: true });
  const alice = mkMember(guild, ALICE, { position: 5, roles: [ROLE_A, ROLE_EVENT] });

  const { back, res } = await leaveAndRejoin(guild, alice, settings, async () => {
    await endEvent(guild, event);
    assert.equal(flagOf('E1', ALICE), true, 'the strip could not reach the member: the flag stays as the marker');
  });
  assert.equal(res.status, 'restored');
  assert.deepEqual(givenIds(back), [ROLE_A]);
  assert.ok(res.skipped.some((s) => s.roleId === ROLE_EVENT && /event/.test(s.reason)));
  assert.equal(flagOf('E1', ALICE), false, 'the marker is cleared once the restore has acted on it');
});

test('event role: a member who held the role on their own gets it back after the event ends', async () => {
  const guild = mkGuild({ roles: rpRoles() });
  const settings = await seed(guild.id);
  const event = { id: 'E1', guildId: guild.id, roleId: ROLE_EVENT, status: 'ACTIVE' };
  guildEvents.push(event);
  // Held the role already when they RSVP'd (the bot never granted it), or never RSVP'd at all.
  rsvps.push({ eventId: 'E1', userId: ALICE, roleGranted: false });
  const alice = mkMember(guild, ALICE, { position: 5, roles: [ROLE_A, ROLE_EVENT] });

  const { back, res } = await leaveAndRejoin(guild, alice, settings, async () => { await endEvent(guild, event); });
  assert.equal(res.status, 'restored');
  assert.deepEqual(givenIds(back).sort(), [ROLE_A, ROLE_EVENT]);

  persisted.clear();
  const bob = mkMember(guild, ALICE, { position: 5, roles: [ROLE_A, ROLE_EVENT] });
  rsvps.length = 0;
  const again = await leaveAndRejoin(guild, bob, settings);
  assert.deepEqual(givenIds(again.back).sort(), [ROLE_A, ROLE_EVENT], 'no RSVP row at all: same');
});

test('event role: given back while the event is still open, flag or not', async () => {
  const guild = mkGuild({ roles: rpRoles() });
  const settings = await seed(guild.id);
  guildEvents.push({ id: 'E1', guildId: guild.id, roleId: ROLE_EVENT, status: 'ACTIVE' });
  rsvps.push({ eventId: 'E1', userId: ALICE, roleGranted: true });
  const alice = mkMember(guild, ALICE, { position: 5, roles: [ROLE_A, ROLE_EVENT] });
  const { back } = await leaveAndRejoin(guild, alice, settings);
  assert.deepEqual(givenIds(back).sort(), [ROLE_A, ROLE_EVENT]);
  assert.equal(flagOf('E1', ALICE), true, 'an open event keeps its flag');
});

test('event role: a role with a live grant from a later, still open event is still given back', async () => {
  const guild = mkGuild({ roles: rpRoles() });
  const settings = await seed(guild.id);
  guildEvents.push({ id: 'E1', guildId: guild.id, roleId: ROLE_EVENT, status: 'ENDED' }, { id: 'E2', guildId: guild.id, roleId: ROLE_EVENT, status: 'SCHEDULED' });
  rsvps.push({ eventId: 'E1', userId: ALICE, roleGranted: true }, { eventId: 'E2', userId: ALICE, roleGranted: true });
  persisted.set(rowKey(guild.id, ALICE), { guildId: guild.id, userId: ALICE, roleIds: [ROLE_EVENT], nickname: null, leftAt: new Date() });
  const back = mkMember(guild, ALICE, { position: 1 });
  await rp.restoreMember(back, settings);
  assert.deepEqual(givenIds(back), [ROLE_EVENT]);
});

test('event role: another member\'s flag or another guild\'s event does not matter', async () => {
  const guild = mkGuild({ roles: rpRoles() });
  const settings = await seed(guild.id);
  guildEvents.push({ id: 'E1', guildId: guild.id, roleId: ROLE_EVENT, status: 'ENDED' }, { id: 'E9', guildId: 'OTHER', roleId: ROLE_EVENT, status: 'ENDED' });
  rsvps.push({ eventId: 'E1', userId: 'someone-else', roleGranted: true }, { eventId: 'E9', userId: ALICE, roleGranted: true });
  persisted.set(rowKey(guild.id, ALICE), { guildId: guild.id, userId: ALICE, roleIds: [ROLE_EVENT], nickname: null, leftAt: new Date() });
  const back = mkMember(guild, ALICE, { position: 1 });
  await rp.restoreMember(back, settings);
  assert.deepEqual(givenIds(back), [ROLE_EVENT]);
});

test('birthday role: not saved, and not given back from a row saved before the fix', async () => {
  const guild = mkGuild({ roles: rpRoles() });
  const settings = await seed(guild.id, { birthdayRoleId: ROLE_BDAY });
  const alice = mkMember(guild, ALICE, { position: 5, roles: [ROLE_A, ROLE_BDAY] });
  assert.deepEqual(snapshotMember(alice, guild, settings).roleIds, [ROLE_A]);

  const { back } = await leaveAndRejoin(guild, alice, settings);
  assert.deepEqual(givenIds(back), [ROLE_A]);

  persisted.set(rowKey(guild.id, ALICE), { guildId: guild.id, userId: ALICE, roleIds: [ROLE_BDAY, ROLE_A], nickname: null, leftAt: new Date() });
  const again = mkMember(guild, ALICE, { position: 1 });
  const res = await rp.restoreMember(again, settings);
  assert.deepEqual(givenIds(again), [ROLE_A]);
  assert.ok(res.skipped.some((s) => s.roleId === ROLE_BDAY && /Birthdays/.test(s.reason)));
});
