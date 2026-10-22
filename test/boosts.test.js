// Server Boosts tests. Offline (no Discord, Postgres or Redis): the pure rules (boost transitions, role names, colours, emojis,
// sweep plans), the booster role create / edit / delete flows against a fake guild whose role list behaves like discord.js'
// (positions shift, setPosition re-inserts), the hooks, the boosts.sweep job and the /boost payload and subcommands, all
// against in-memory BoosterRole / GuildSettings / ScheduledJob tables.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const { PermissionFlagsBits } = await import('discord.js');
const svc = await import('../src/services/boostService.js');
const boosts = await import('../src/plugins/boosts.js');
const cmd = await import('../src/commands/community/boost.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { JOB_TYPES } = await import('../src/config/jobTypes.js');
const { LIMITS, PERMISSION_LEVELS, DEFAULT_MODULE_SETTINGS } = await import('../src/config/constants.js');
const { MODULES } = await import('../src/config/modules.js');
const { updateSettings } = await import('../src/services/settingsService.js');
const { DEFAULT_TEXTS } = await import('../src/dashboard/fields.js');

const {
  BOOST, DEFAULT_BOOST_MESSAGE, isBoosting, boostTransition, graceMs, tierNumber, messageTemplate, renderThankYou, parseColor,
  parseEmoji, validateRoleName, planSweep, cooldownLeftMs, noteEdit, resetCooldowns, memberProblem, problemEmbed, placementFor,
  createBoosterRole, editBoosterRole, deleteOwnRole, removeMemberRole, postThankYou, onMemberUpdate, onMemberRemove, onRoleDelete,
  onChannelDelete, onGuildReady, memberUpdateFilter, memberRemoveFilter, channelDeleteFilter, sweepGuild, sweepJobHandler,
  ensureSweepJob, resetSweepState, __setIconFetch,
} = svc;
const { plugin } = boosts;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory tables ──────────────────────────────────────────────────────────

const rows = new Map();          // `${guildId}|${userId}` -> BoosterRole row
const settingsRows = new Map();
const jobs = new Map();          // dedupeKey -> ScheduledJob row
const faults = { create: null, rowsFail: false };
let jobSeq = 1;

const copy = (row) => (row ? { ...row } : null);
const rowKey = (g, u) => `${g}|${u}`;
const matches = (row, where = {}) => Object.entries(where).every(([k, v]) => {
  if (v && typeof v === 'object' && !(v instanceof Date)) {
    if ('not' in v) return row[k] !== v.not;
    return false;
  }
  return row[k] === v;
});

const boosterRoleModel = {
  findUnique: async ({ where }) => {
    if (faults.rowsFail) throw new Error('database is down');
    const k = where.guildId_userId;
    return copy(rows.get(rowKey(k.guildId, k.userId)));
  },
  findMany: async ({ where }) => {
    if (faults.rowsFail) throw new Error('database is down');
    return [...rows.values()].filter((r) => matches(r, where)).sort((a, b) => a.createdAt - b.createdAt).map(copy);
  },
  count: async ({ where }) => {
    if (faults.rowsFail) throw new Error('database is down');
    return [...rows.values()].filter((r) => matches(r, where)).length;
  },
  create: async ({ data }) => {
    if (faults.create) {
      const code = faults.create;
      faults.create = null;
      throw Object.assign(new Error('Unique constraint failed'), { code });
    }
    if (rows.has(rowKey(data.guildId, data.userId)) || [...rows.values()].some((r) => r.roleId === data.roleId)) {
      throw Object.assign(new Error('Unique constraint failed'), { code: 'P2002' });
    }
    const row = { boostEndedAt: null, createdAt: new Date(Date.now() + rows.size), updatedAt: new Date(), ...data };
    rows.set(rowKey(row.guildId, row.userId), row);
    return copy(row);
  },
  updateMany: async ({ where, data }) => {
    const hit = [...rows.values()].filter((r) => matches(r, where));
    for (const r of hit) Object.assign(r, data);
    return { count: hit.length };
  },
  deleteMany: async ({ where }) => {
    const hit = [...rows.entries()].filter(([, r]) => matches(r, where));
    for (const [k] of hit) rows.delete(k);
    return { count: hit.length };
  },
};

const jobModel = {
  findUnique: async ({ where }) => copy(jobs.get(where.dedupeKey)),
  create: async ({ data }) => {
    const row = { id: `j${jobSeq++}`, status: 'PENDING', ...data };
    jobs.set(data.dedupeKey, row);
    return copy(row);
  },
  update: async ({ where, data }) => {
    const row = [...jobs.values()].find((r) => r.id === where.id);
    Object.assign(row, data);
    return copy(row);
  },
};
const transaction = async (fn) => fn({
  $queryRaw: async (q) => { const j = jobs.get(q.values[0]); return j ? [copy(j)] : []; },
  scheduledJob: jobModel,
});

const real = {
  boosterRole: prisma.boosterRole, guildSettings: prisma.guildSettings, scheduledJob: prisma.scheduledJob, $transaction: prisma.$transaction,
};
prisma.boosterRole = boosterRoleModel;
prisma.scheduledJob = jobModel;
prisma.$transaction = transaction;
prisma.guildSettings = {
  findUnique: async ({ where }) => copy(settingsRows.get(where.guildId)),
  upsert: async ({ where, update }) => {
    const row = { ...(settingsRows.get(where.guildId) ?? { guildId: where.guildId }), ...update };
    settingsRows.set(where.guildId, row);
    return copy(row);
  },
};

after(async () => {
  Object.assign(prisma, real);
  __setIconFetch();
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

let nextId = 9000;
const ZERO_WIDTH = String.fromCodePoint(0x200b);
const PNG = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0]);
const JPEG = Buffer.from([0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0]);
const apiError = (code, message = 'api error') => Object.assign(new Error(message), { code });

/**
 * A guild whose role list behaves like discord.js': a created role lands at position 1 and shifts the others up, and
 * setPosition removes the role and re-inserts it at the index (Util.moveElementInArray).
 */
function makeWorld({
  id = 'G1', features = ['ROLE_ICONS'], manageRoles = true, botTop = 8, anchorAt = null, tier = 2, boostCount = 7, roleNames = [],
} = {}) {
  const roles = new Map();
  const log = { created: [], deleted: [], moved: [], edits: [], fetches: 0, added: [], sent: [] };
  const faultsW = { create: null, deleteCode: null, setPosition: null, add: null, edit: null };
  const addRole = (roleId, name, position) => {
    const role = {
      id: roleId, name, position, managed: false, editable: true,
      async edit(opts) {
        if (faultsW.edit) throw faultsW.edit;
        log.edits.push({ roleId, ...opts });
        if (opts.name) this.name = opts.name;
        return this;
      },
      async setPosition(k, o) {
        if (faultsW.setPosition) throw faultsW.setPosition;
        const sorted = [...roles.values()].sort((a, b) => a.position - b.position);
        sorted.splice(sorted.indexOf(this), 1);
        sorted.splice(k, 0, this);
        sorted.forEach((r, i) => { r.position = i; });
        log.moved.push({ roleId, k, reason: o?.reason });
        return this;
      },
    };
    roles.set(roleId, role);
    return role;
  };
  addRole(id, '@everyone', 0);
  // roles 1..botTop-1 are "ordinary"; the bot's own role sits at botTop
  for (let p = 1; p < botTop; p++) addRole(`R${p}`, roleNames[p - 1] ?? `role-${p}`, p);
  const botRole = addRole('BOTROLE', 'The Bot', botTop);
  const anchor = anchorAt ? roles.get(`R${anchorAt}`) : null;

  const guild = {
    id, name: 'Test Guild', available: true, ownerId: 'OWNER', premiumTier: tier, premiumSubscriptionCount: boostCount, features,
    roles: {
      cache: roles,
      async create(opts) {
        if (faultsW.create) throw faultsW.create;
        const roleId = `NEW${nextId++}`;
        for (const r of roles.values()) if (r.position >= 1) r.position += 1;
        const role = addRole(roleId, opts.name, 1);
        log.created.push({ roleId, ...opts });
        return role;
      },
      async fetch() { log.fetches += 1; return roles; },
      async delete(roleId, reason) {
        if (!roles.has(roleId)) throw apiError(10011, 'Unknown Role');
        if (faultsW.deleteCode) throw apiError(faultsW.deleteCode, 'Missing Permissions');
        roles.delete(roleId);
        const sorted = [...roles.values()].sort((a, b) => a.position - b.position);
        sorted.forEach((r, i) => { r.position = i; });
        log.deleted.push({ roleId, reason });
      },
    },
    members: {
      cache: new Map(),
      remote: new Map(), // everyone who is really in the server, cached or not
      me: { permissions: { has: () => manageRoles }, roles: { get highest() { return botRole; } } },
      fetchFails: new Set(),
      async fetch(userId) {
        if (this.fetchFails.has(userId)) throw apiError(500, 'gateway timeout');
        const m = this.remote.get(userId);
        if (!m) throw apiError(10007, 'Unknown Member');
        this.cache.set(userId, m); // like discord.js: a fetched member is cached
        return m;
      },
    },
    channels: { cache: new Map() },
  };
  const addMember = (userId, { boostingSince = null, roleIds = [], cached = true, bot = false } = {}) => {
    const held = new Map(roleIds.map((r) => [r, roles.get(r)]));
    const m = {
      id: userId, user: { id: userId, bot }, displayName: `User ${userId}`, premiumSinceTimestamp: boostingSince, guild,
      roles: {
        cache: held,
        async add(role, reason) {
          if (faultsW.add) throw faultsW.add;
          const rid = typeof role === 'string' ? role : role.id;
          held.set(rid, roles.get(rid));
          log.added.push({ userId, roleId: rid, reason });
        },
      },
    };
    guild.members.remote.set(userId, m);
    if (cached) guild.members.cache.set(userId, m);
    return m;
  };
  const addChannel = (channelId, { denied = [] } = {}) => {
    const ch = {
      id: channelId, guild, isTextBased: () => true, isThread: () => false,
      permissionsFor: () => ({ has: (bit) => !denied.some((d) => PermissionFlagsBits[d] === bit) }),
      async send(payload) { if (ch.sendFails) throw new Error('boom'); log.sent.push({ channelId, ...payload }); return { id: String(nextId++) }; },
    };
    guild.channels.cache.set(channelId, ch);
    return ch;
  };
  return { guild, roles, log, faults: faultsW, addMember, addChannel, anchor, botRole };
}

const order = (w) => [...w.roles.values()].sort((a, b) => a.position - b.position).map((r) => r.name);
const BOOSTING = 1_700_000_000_000;
const settingsFor = (data = {}) => ({
  boostsEnabled: true, boostCustomRoles: true, boostChannelId: null, boostMessage: null, boostRoleAnchorId: null, boostRoleGraceDays: 0,
  ...data,
});
const seedSettings = (guildId, data) => updateSettings(guildId, data);
const addRow = (guildId, userId, roleId, extra = {}) => {
  const row = { guildId, userId, roleId, boostEndedAt: null, createdAt: new Date(Date.now() + rows.size), updatedAt: new Date(), ...extra };
  rows.set(rowKey(guildId, userId), row);
  return row;
};
const reset = () => {
  rows.clear(); jobs.clear(); settingsRows.clear();
  faults.create = null; faults.rowsFail = false;
  resetCooldowns(); resetSweepState(); __setIconFetch();
};

// ── Pure rules ────────────────────────────────────────────────────────────────

test('the default thank-you is the dashboard default, and the module is registered off by default', () => {
  assert.equal(DEFAULT_BOOST_MESSAGE, DEFAULT_TEXTS.boostMessage);
  assert.equal(MODULES.boosts.flag, 'boostsEnabled');
  assert.equal(DEFAULT_MODULE_SETTINGS.boosts, false);
  assert.equal(MODULES.boosts.configCommand, '/boost');
});

test('isBoosting and boostTransition: start, end, nothing; a partial or empty member is not boosting', () => {
  assert.equal(isBoosting({ premiumSinceTimestamp: BOOSTING }), true);
  assert.equal(isBoosting({ premiumSinceTimestamp: null }), false);
  assert.equal(isBoosting({ premiumSince: new Date() }), true);
  assert.equal(isBoosting(null), false);
  const on = { premiumSinceTimestamp: BOOSTING };
  const off = { premiumSinceTimestamp: null };
  assert.equal(boostTransition(off, on), 'start');
  assert.equal(boostTransition(on, off), 'end');
  assert.equal(boostTransition(on, { premiumSinceTimestamp: BOOSTING + 5 }), null);
  assert.equal(boostTransition(off, off), null);
});

test('graceMs is 0..30 days, tierNumber 0..3', () => {
  const day = BOOST.DAY_MS;
  assert.equal(graceMs({ boostRoleGraceDays: 0 }), 0);
  assert.equal(graceMs({ boostRoleGraceDays: 3 }), 3 * day);
  assert.equal(graceMs({ boostRoleGraceDays: 99 }), 30 * day);
  assert.equal(graceMs({ boostRoleGraceDays: -4 }), 0);
  assert.equal(graceMs({ boostRoleGraceDays: 'x' }), 0);
  assert.equal(graceMs(null), 0);
  assert.deepEqual([0, 1, 2, 3, 9, -1, null].map((t) => tierNumber({ premiumTier: t })), [0, 1, 2, 3, 3, 0, 0]);
});

test('the thank-you: default when blank, {boosts} and {tier} filled in, at most 2000 characters', () => {
  const w = makeWorld({ tier: 2, boostCount: 14 });
  const m = w.addMember('U1');
  assert.equal(messageTemplate({ boostMessage: null }), DEFAULT_BOOST_MESSAGE);
  assert.equal(messageTemplate({ boostMessage: '   ' }), DEFAULT_BOOST_MESSAGE);
  assert.equal(messageTemplate({ boostMessage: 'hi' }), 'hi');
  assert.equal(renderThankYou({ boostMessage: null }, m, w.guild), '💎 <@U1> just boosted **Test Guild**! Thank you! We now have 14 boosts.');
  assert.equal(renderThankYou({ boostMessage: '{user.id} / {boosts} / {tier} / {server}' }, m, w.guild), 'U1 / 14 / 2 / Test Guild');
  assert.equal(renderThankYou({ boostMessage: 'x'.repeat(5000) }, m, w.guild).length, 2000);
  assert.equal(renderThankYou({ boostMessage: '{unknownthing}' }, m, w.guild), '{unknownthing}');
  // A preview counts the next boost, so a server with none does not read "We now have 0 boosts".
  assert.equal(renderThankYou({ boostMessage: '{boosts}' }, m, w.guild, { preview: true }), '15');
  const none = makeWorld({ tier: 0, boostCount: 0 });
  assert.equal(renderThankYou({ boostMessage: '{boosts}' }, none.addMember('U2'), none.guild), '0', 'the real thank-you is not changed');
  assert.match(renderThankYou({ boostMessage: null }, none.addMember('U3'), none.guild, { preview: true }), /We now have 1 boosts\./);
});

test('parseColor: #rrggbb with or without the #, blank means none, anything else is refused', () => {
  assert.deepEqual(parseColor('#ff8800'), { ok: true, value: 0xff8800 });
  assert.deepEqual(parseColor('FF8800'), { ok: true, value: 0xff8800 });
  assert.deepEqual(parseColor(' #000000 '), { ok: true, value: 0 });
  assert.deepEqual(parseColor(null), { ok: true, value: null });
  assert.deepEqual(parseColor(''), { ok: true, value: null });
  for (const bad of ['#fff', 'red', '#gg0000', '#ff88001', '0xff8800']) assert.equal(parseColor(bad).ok, false, bad);
});

test('parseEmoji: one standard emoji only', () => {
  for (const ok of ['💎', '👍🏽', '🇩🇪', '1️⃣', '❤️', '👨‍👩‍👧']) assert.equal(parseEmoji(ok).ok, true, ok);
  for (const bad of ['', 'ab', '💎💎', '<:gem:123456789>', '💎 hi', 'x'.repeat(40)]) assert.equal(parseEmoji(bad).ok, false, bad);
});

test('validateRoleName: trimmed, no @, no links, no duplicate of an existing role, own role excepted', () => {
  const w = makeWorld({ roleNames: ['Moderator', 'Admin Team'] });
  const g = w.guild;
  assert.deepEqual(validateRoleName('  Cool   Booster  ', g), { ok: true, name: 'Cool Booster' });
  assert.deepEqual(validateRoleName(`a${ZERO_WIDTH}b`, g), { ok: true, name: 'ab' });
  assert.equal(validateRoleName('', g).reason, 'empty');
  assert.equal(validateRoleName(`   ${ZERO_WIDTH} `, g).reason, 'empty');
  assert.equal(validateRoleName('x'.repeat(LIMITS.BOOSTER_ROLE_NAME), g).ok, true);
  assert.equal(validateRoleName('x'.repeat(LIMITS.BOOSTER_ROLE_NAME + 1), g).reason, 'too-long');
  for (const at of ['@everyone', 'hi @here', 'mail@x', 'a＠b']) assert.equal(validateRoleName(at, g).reason, 'at', at);
  for (const link of ['https://x.io', 'www.example.net', 'discord.gg/abc', 'discord.com/invite/abc', 'join x.com now']) {
    assert.equal(validateRoleName(link, g).reason, 'link', link);
  }
  // existing role names, any case, extra spaces or punctuation, full-width look-alikes
  for (const dup of ['moderator', 'MODERATOR', ' Moderator ', 'Mod erator', 'M.o.d.e.r.a.t.o.r', 'Ｍｏｄｅｒａｔｏｒ', 'admin  team', 'Admin-Team']) {
    assert.equal(validateRoleName(dup, g).reason, 'taken', dup);
  }
  // the member's own role does not collide with itself (re-casing it is fine)
  const own = [...g.roles.cache.values()].find((r) => r.name === 'Moderator');
  assert.equal(validateRoleName('MODERATOR', g, { exceptRoleId: own.id }).ok, true);
  assert.equal(validateRoleName('Admin Team', g, { exceptRoleId: own.id }).reason, 'taken');
});

test('planSweep: who keeps, loses or regains a role', () => {
  const now = 10 * BOOST.DAY_MS;
  const grace2 = { boostRoleGraceDays: 2 };
  const none = { boostRoleGraceDays: 0 };
  const boosting = { premiumSinceTimestamp: BOOSTING };
  const idle = { premiumSinceTimestamp: null };
  const live = { boostEndedAt: null };
  const ended = (daysAgo) => ({ boostEndedAt: new Date(now - daysAgo * BOOST.DAY_MS) });
  assert.equal(planSweep(live, undefined, none, now).type, 'skip');
  assert.equal(planSweep(ended(9), undefined, none, now).type, 'skip');
  assert.deepEqual(planSweep(live, null, grace2, now).type, 'delete');
  assert.equal(planSweep(live, boosting, grace2, now).type, 'none');
  assert.equal(planSweep(ended(1), boosting, grace2, now).type, 'resumed');
  assert.equal(planSweep(live, idle, none, now).type, 'delete');
  assert.equal(planSweep(live, idle, grace2, now).type, 'ended');
  assert.equal(planSweep(ended(1), idle, grace2, now).type, 'none');
  assert.equal(planSweep(ended(2), idle, grace2, now).type, 'delete');
  assert.equal(planSweep(ended(5), idle, grace2, now).type, 'delete');
});

test('the edit cooldown is one minute per member and per server, and forgets old entries', () => {
  resetCooldowns();
  assert.equal(cooldownLeftMs('G', 'U'), 0);
  noteEdit('G', 'U', 1_000_000);
  assert.equal(cooldownLeftMs('G', 'U', 1_000_000 + 10_000), BOOST.EDIT_COOLDOWN_MS - 10_000);
  assert.equal(cooldownLeftMs('G', 'U', 1_000_000 + BOOST.EDIT_COOLDOWN_MS), 0);
  assert.equal(cooldownLeftMs('G', 'OTHER', 1_000_001), 0);
  assert.equal(cooldownLeftMs('OTHER', 'U', 1_000_001), 0);
  resetCooldowns();
});

test('memberProblem: settings, module, booster roles on, boosting', () => {
  const booster = { premiumSinceTimestamp: BOOSTING };
  assert.equal(memberProblem(null, booster).code, 'SETTINGS');
  assert.equal(memberProblem(settingsFor({ boostsEnabled: false }), booster).code, 'MODULE_OFF');
  assert.equal(memberProblem(settingsFor({ boostCustomRoles: false }), booster).code, 'ROLES_OFF');
  assert.equal(memberProblem(settingsFor(), { premiumSinceTimestamp: null }).code, 'NOT_BOOSTING');
  assert.equal(memberProblem(settingsFor(), { premiumSinceTimestamp: null }, { needBoosting: false }), null);
  assert.equal(memberProblem(settingsFor(), booster), null);
  const off = problemEmbed({ code: 'MODULE_OFF' }).toJSON();
  assert.match(off.title, /Module Disabled/);
  assert.equal(off.description, 'The **Server Boosts** module is turned off on this server. An admin can enable it with `/module enable boosts`.');
});

test('placementFor: needs Manage Roles, room below the bot and an anchor that is not above the bot', () => {
  const ok = makeWorld({ anchorAt: 4 });
  assert.deepEqual(placementFor(ok.guild, {}), { ok: true, kind: 'bottom', base: null, target: 1 }); // no anchor, no Nitro Booster role
  assert.equal(placementFor(ok.guild, { boostRoleAnchorId: 'R4' }).base.id, 'R4');
  assert.equal(placementFor(ok.guild, { boostRoleAnchorId: 'DELETED' }).kind, 'bottom'); // a missing anchor falls back
  assert.equal(placementFor(ok.guild, { boostRoleAnchorId: ok.guild.id }).kind, 'bottom'); // @everyone is not an anchor
  assert.equal(placementFor(makeWorld({ manageRoles: false }).guild, {}).code, 'BOT_PERMS');
  assert.equal(placementFor(makeWorld({ botTop: 1 }).guild, {}).code, 'BOT_LOW');
  const high = makeWorld({ botTop: 4 });
  high.roles.set('UP', { id: 'UP', name: 'Up', position: 9 });
  assert.equal(placementFor(high.guild, { boostRoleAnchorId: 'UP' }).code, 'ANCHOR_HIGH');
});

// ── Declaration and payload ───────────────────────────────────────────────────

test('the plugin declares the five hooks in HOOK_ORDER, the sweep job and no components', () => {
  assert.equal(plugin.id, 'boosts');
  assert.equal(plugin.module, 'boosts');
  const byEvent = Object.fromEntries(plugin.hooks.map((h) => [h.event, h]));
  assert.deepEqual(Object.keys(byEvent).sort(), ['channelDelete', 'guildMemberRemove', 'guildMemberUpdate', 'guildReady', 'roleDelete']);
  for (const h of plugin.hooks) {
    assert.equal(h.id, 'boosts');
    assert.ok(HOOK_ORDER[h.event].includes('boosts'), `${h.event}:boosts is in HOOK_ORDER`);
    assert.equal(typeof h.run, 'function');
    if (h.filter) assert.equal(h.filter.constructor.name, 'Function', `${h.event} filter is synchronous`);
  }
  // cleanup runs even while the module is off; the rest is gated by the module
  assert.deepEqual(plugin.hooks.filter((h) => h.always === true).map((h) => h.event).sort(), ['channelDelete', 'roleDelete']);
  assert.ok(byEvent.guildMemberUpdate.filter && byEvent.guildMemberRemove.filter, 'the per-member hooks have a cheap filter');
  assert.deepEqual(plugin.components, []);
  assert.deepEqual(plugin.jobs.map((j) => j.type), ['boosts.sweep']);
  assert.equal(plugin.jobs[0].handler, sweepJobHandler);
  assert.deepEqual(JOB_TYPES['boosts.sweep'], { owner: 'boosts', recurring: true, dedupe: 'boosts.sweep:{guildId}' });
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/boost: member command, ungated, admin subcommands, role group, payload under 2000 characters', () => {
  const payload = buildCommandPayload(cmd);
  assert.equal(payload.name, 'boost');
  assert.ok(charCount(payload) < 2000, `${charCount(payload)} characters`);
  assert.equal(cmd.module, 'boosts');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel ?? PERMISSION_LEVELS.MEMBER, PERMISSION_LEVELS.MEMBER);
  assert.equal(payload.default_member_permissions, undefined, 'members must see /boost role');
  assert.deepEqual(cmd.subcommandPermissions, {
    config: PERMISSION_LEVELS.ADMIN, test: PERMISSION_LEVELS.ADMIN, list: PERMISSION_LEVELS.ADMIN, remove: PERMISSION_LEVELS.ADMIN,
  });
  const keys = new Set();
  for (const o of payload.options) {
    if (o.type === 1) keys.add(o.name);
    else for (const s of o.options) keys.add(`${o.name} ${s.name}`);
  }
  assert.deepEqual([...keys].sort(), ['config', 'list', 'remove', 'role create', 'role delete', 'role edit', 'test']);
  for (const k of Object.keys(cmd.subcommandPermissions)) assert.ok(keys.has(k), k);
  const role = payload.options.find((o) => o.name === 'role');
  const create = role.options.find((s) => s.name === 'create');
  assert.deepEqual(create.options.map((o) => o.name), ['name', 'color', 'icon_emoji', 'icon_image']);
  assert.equal(create.options[0].required, true);
  assert.equal(create.options[0].max_length, LIMITS.BOOSTER_ROLE_NAME);
  const edit = role.options.find((s) => s.name === 'edit');
  assert.deepEqual(edit.options.map((o) => o.name), ['name', 'color', 'icon_emoji', 'icon_image']);
  assert.ok(edit.options.every((o) => !o.required));
  const config = payload.options.find((o) => o.name === 'config');
  assert.deepEqual(config.options.map((o) => o.name), ['channel', 'custom_roles', 'anchor', 'grace_days']);
  const grace = config.options.find((o) => o.name === 'grace_days');
  assert.deepEqual([grace.min_value, grace.max_value], [0, 30]);
  assert.equal(payload.options.find((o) => o.name === 'remove').options[0].required, true);
});

// ── Filters ───────────────────────────────────────────────────────────────────

test('memberUpdateFilter: only a boost change of a cached person; a partial old member never counts', () => {
  const on = { premiumSinceTimestamp: BOOSTING, user: { bot: false } };
  const off = { premiumSinceTimestamp: null, user: { bot: false } };
  assert.equal(memberUpdateFilter({ oldKnown: true, oldMember: off, newMember: on }), true);
  assert.equal(memberUpdateFilter({ oldKnown: true, oldMember: on, newMember: off }), true);
  assert.equal(memberUpdateFilter({ oldKnown: true, oldMember: on, newMember: { ...on } }), false);
  assert.equal(memberUpdateFilter({ oldKnown: true, oldMember: off, newMember: { ...off, nickname: 'x' } }), false);
  // a partial old member has no known premiumSince: no thank-you after a restart
  assert.equal(memberUpdateFilter({ oldKnown: false, oldMember: { partial: true }, newMember: on }), false);
  assert.equal(memberUpdateFilter({ oldKnown: undefined, oldMember: off, newMember: on }), false);
  assert.equal(memberUpdateFilter({ oldKnown: true, oldMember: null, newMember: on }), false);
  assert.equal(memberUpdateFilter({ oldKnown: true, oldMember: off, newMember: { ...on, user: { bot: true } } }), false);
  assert.equal(memberRemoveFilter({ user: { bot: false } }), true);
  assert.equal(memberRemoveFilter({ user: { bot: true } }), false);
  assert.equal(memberRemoveFilter({}), false);
});

test('channelDeleteFilter: skipped once the memoised settings show another channel', async () => {
  reset();
  assert.equal(channelDeleteFilter({ guildId: 'FILTER-UNKNOWN', channel: { id: 'C' } }), true); // no memo: fails open
  await seedSettings('FILTER-1', { boostChannelId: 'C1' });
  assert.equal(channelDeleteFilter({ guildId: 'FILTER-1', channel: { id: 'C1' } }), true);
  assert.equal(channelDeleteFilter({ guildId: 'FILTER-1', channel: { id: 'C2' } }), false);
});

// ── Creating a booster role ───────────────────────────────────────────────────

test('create: no permissions, not hoisted, not mentionable, trimmed name, colour, at the bottom (no Nitro Booster role), given with a reason', async () => {
  reset();
  const w = makeWorld({ id: 'GC1' });
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  const res = await createBoosterRole({ guild: w.guild, member, settings: settingsFor(), name: '  Gem   Lord ', color: '#ff8800' });
  assert.equal(res.ok, true);
  assert.equal(res.moved, true);
  const [made] = w.log.created;
  assert.equal(made.name, 'Gem Lord');
  assert.equal(made.permissions, 0n);
  assert.equal(made.hoist, false);
  assert.equal(made.mentionable, false);
  assert.deepEqual(made.colors, { primaryColor: 0xff8800 });
  assert.equal(made.color, undefined, 'the deprecated `color` is not used');
  assert.match(made.reason, /^Server Boosts: /);
  // fetched first (positions are stale after the create); with no anchor and no Nitro Booster role it stays at the bottom
  assert.ok(w.log.fetches >= 1);
  const names = order(w);
  assert.equal(names.at(-1), 'The Bot');
  assert.equal(names[1], 'Gem Lord');
  // given to the member with an audit-log reason, one role
  assert.equal(w.log.added.length, 1);
  assert.equal(w.log.added[0].roleId, res.role.id);
  assert.match(w.log.added[0].reason, /^Server Boosts: /);
  // saved, and the sweep is seeded
  assert.equal(rows.get(rowKey('GC1', 'U1')).roleId, res.role.id);
  assert.equal(rows.get(rowKey('GC1', 'U1')).boostEndedAt, null);
  assert.equal(jobs.get('boosts.sweep:GC1').type, 'boosts.sweep');
});

test('create: placed just below the anchor role, so the order is [... new role, anchor ...]', async () => {
  reset();
  const w = makeWorld({ id: 'GC2', anchorAt: 4 });
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  const res = await createBoosterRole({ guild: w.guild, member, settings: settingsFor({ boostRoleAnchorId: 'R4' }), name: 'Below Anchor' });
  assert.equal(res.ok, true);
  const names = order(w);
  const at = names.indexOf('Below Anchor');
  assert.equal(names[at + 1], 'role-4', 'the anchor is right above the new role');
  assert.equal(names[at - 1], 'role-3', 'the roles below the anchor kept their order');
  assert.ok(w.roles.get(res.role.id).position < w.botRole.position, 'below the bot');
  assert.deepEqual(names, ['@everyone', 'role-1', 'role-2', 'role-3', 'Below Anchor', 'role-4', 'role-5', 'role-6', 'role-7', 'The Bot']);
});

test('REVIEW: with no anchor a booster role goes just ABOVE the Nitro Booster role, below the moderators; never at or above the bot', async () => {
  reset();
  const w = makeWorld({ id: 'GN1', roleNames: ['Members', 'Server Booster', 'Moderator', 'Admin'] });
  w.roles.get('R2').tags = { premiumSubscriberRole: true };
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  const res = await createBoosterRole({ guild: w.guild, member, settings: settingsFor(), name: 'Shiny' });
  assert.equal(res.ok, true);
  assert.equal(res.moved, true);
  const names = order(w);
  assert.deepEqual(names.slice(0, 6), ['@everyone', 'Members', 'Server Booster', 'Shiny', 'Moderator', 'Admin'],
    'right above the boost role (its colour shows), below the moderators (they can still kick, ban and time out boosters)');
  assert.ok(placementFor(w.guild, settingsFor()).kind === 'nitro');

  // A Nitro Booster role above the bot: the role goes as high as the bot may, never at or above it.
  const hi = makeWorld({ id: 'GN3', botTop: 3 });
  hi.roles.set('NB', { id: 'NB', name: 'Server Booster', position: 4, tags: { premiumSubscriberRole: true } });
  const m2 = hi.addMember('U1', { boostingSince: BOOSTING });
  const r2 = await createBoosterRole({ guild: hi.guild, member: m2, settings: settingsFor(), name: 'Capped' });
  assert.equal(r2.ok, true);
  assert.deepEqual(order(hi), ['@everyone', 'role-1', 'role-2', 'Capped', 'The Bot', 'Server Booster']);

  // An anchor still wins over the Nitro Booster role.
  const a = makeWorld({ id: 'GN4', anchorAt: 4 });
  a.roles.get('R2').tags = { premiumSubscriberRole: true };
  const m3 = a.addMember('U1', { boostingSince: BOOSTING });
  await createBoosterRole({ guild: a.guild, member: m3, settings: settingsFor({ boostRoleAnchorId: 'R4' }), name: 'Anchored' });
  const an = order(a);
  assert.equal(an[an.indexOf('Anchored') + 1], 'role-4');
});

test('create: a name that collides, holds a link or an @, is empty or too long, or a bad colour, is refused and creates nothing', async () => {
  reset();
  const w = makeWorld({ id: 'GC3', roleNames: ['Moderator'] });
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  const settings = settingsFor();
  const bad = async (extra) => createBoosterRole({ guild: w.guild, member, settings, name: 'Fine Name', ...extra });
  assert.equal((await bad({ name: 'moderator' })).code, 'BAD_NAME');
  assert.equal((await bad({ name: 'see discord.gg/x' })).code, 'BAD_NAME');
  assert.equal((await bad({ name: 'hi @here' })).code, 'BAD_NAME');
  assert.equal((await bad({ name: '   ' })).code, 'BAD_NAME');
  assert.equal((await bad({ name: 'x'.repeat(LIMITS.BOOSTER_ROLE_NAME + 1) })).code, 'BAD_NAME');
  assert.equal((await bad({ color: 'green' })).code, 'BAD_COLOR');
  const named = await bad({ name: 'moderator' });
  assert.match(problemEmbed(named).toJSON().description, /already exists/);
  assert.equal(w.log.created.length, 0);
  assert.equal(rows.size, 0);
});

test('create: refusals do not spend the cooldown; success does; a second role is refused', async () => {
  reset();
  const w = makeWorld({ id: 'GC4', roleNames: ['Moderator'] });
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  const settings = settingsFor();
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'moderator' })).code, 'BAD_NAME');
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'Nice', color: 'nope' })).code, 'BAD_COLOR');
  assert.equal(cooldownLeftMs('GC4', 'U1'), 0);
  assert.equal(w.log.created.length, 0);
  const ok = await createBoosterRole({ guild: w.guild, member, settings, name: 'Nice' });
  assert.equal(ok.ok, true);
  assert.ok(cooldownLeftMs('GC4', 'U1') > 0);
  const again = await createBoosterRole({ guild: w.guild, member, settings, name: 'Other' });
  assert.equal(again.code, 'COOLDOWN');
  assert.ok(again.detail.ms > 0);
  resetCooldowns();
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'Other' })).code, 'ALREADY');
  assert.equal(w.log.created.length, 1);
});

test('create: two clicks at once make one role', async () => {
  reset();
  const w = makeWorld({ id: 'GC5' });
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  const settings = settingsFor();
  const [a, b] = await Promise.all([
    createBoosterRole({ guild: w.guild, member, settings, name: 'One' }),
    createBoosterRole({ guild: w.guild, member, settings, name: 'Two' }),
  ]);
  assert.deepEqual([a.ok, b.ok].sort(), [false, true]);
  assert.equal(w.log.created.length, 1);
  assert.equal(rows.size, 1);
});

test('create: a row whose role was deleted behind our back starts over', async () => {
  reset();
  const w = makeWorld({ id: 'GC6' });
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  addRow('GC6', 'U1', 'GONE-ROLE');
  const res = await createBoosterRole({ guild: w.guild, member, settings: settingsFor(), name: 'Fresh' });
  assert.equal(res.ok, true);
  assert.equal(rows.get(rowKey('GC6', 'U1')).roleId, res.role.id);
});

test('create: the bot needs Manage Roles and room below it; the anchor must not be above it; 250 roles is the limit', async () => {
  reset();
  const noPerm = makeWorld({ id: 'GC7', manageRoles: false });
  const m1 = noPerm.addMember('U1', { boostingSince: BOOSTING });
  assert.equal((await createBoosterRole({ guild: noPerm.guild, member: m1, settings: settingsFor(), name: 'A' })).code, 'BOT_PERMS');
  const low = makeWorld({ id: 'GC8', botTop: 1 });
  const m2 = low.addMember('U1', { boostingSince: BOOSTING });
  assert.equal((await createBoosterRole({ guild: low.guild, member: m2, settings: settingsFor(), name: 'A' })).code, 'BOT_LOW');
  const high = makeWorld({ id: 'GC9', botTop: 4 });
  high.roles.set('UP', { id: 'UP', name: 'Up', position: 9 });
  const m3 = high.addMember('U1', { boostingSince: BOOSTING });
  assert.equal((await createBoosterRole({ guild: high.guild, member: m3, settings: settingsFor({ boostRoleAnchorId: 'UP' }), name: 'A' })).code, 'ANCHOR_HIGH');
  const full = makeWorld({ id: 'GC10' });
  for (let i = 0; i < BOOST.ROLES_PER_GUILD; i++) full.roles.set(`F${i}`, { id: `F${i}`, name: `f${i}`, position: 0 });
  const m4 = full.addMember('U1', { boostingSince: BOOSTING });
  assert.equal((await createBoosterRole({ guild: full.guild, member: m4, settings: settingsFor(), name: 'A' })).code, 'ROLE_LIMIT');
  for (const w of [noPerm, low, high, full]) assert.equal(w.log.created.length, 0);
  assert.equal(rows.size, 0);
});

test('create: icons need role icons, one kind at a time, a standard emoji or a PNG/JPEG within the size limit', async () => {
  reset();
  const locked = makeWorld({ id: 'GI1', features: [] });
  const mLocked = locked.addMember('U1', { boostingSince: BOOSTING });
  assert.equal((await createBoosterRole({ guild: locked.guild, member: mLocked, settings: settingsFor(), name: 'A', iconEmoji: '💎' })).code, 'NO_ROLE_ICONS');
  assert.equal((await createBoosterRole({ guild: locked.guild, member: mLocked, settings: settingsFor(), name: 'A', iconImage: { url: 'https://cdn.test/a.png', contentType: 'image/png', size: 10 } })).code, 'NO_ROLE_ICONS');
  assert.equal(locked.log.created.length, 0);

  const w = makeWorld({ id: 'GI2' });
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  const settings = settingsFor();
  const img = (o = {}) => ({ url: 'https://cdn.test/a.png', contentType: 'image/png', size: 1000, ...o });
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'A', iconEmoji: '💎', iconImage: img() })).code, 'ICON_BOTH');
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'A', iconEmoji: '<:gem:123456789>' })).code, 'BAD_EMOJI');
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'A', iconImage: img({ contentType: 'image/gif' }) })).code, 'BAD_ICON');
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'A', iconImage: img({ contentType: 'image/webp' }) })).code, 'BAD_ICON');
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'A', iconImage: img({ size: LIMITS.BOOSTER_ROLE_ICON_BYTES + 1 }) })).code, 'BAD_ICON');
  // the bytes are checked too: a GIF renamed to .png is refused, and a failed download is a clean refusal
  __setIconFetch(async () => Buffer.from('GIF89a...........'));
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'A', iconImage: img() })).code, 'BAD_ICON');
  __setIconFetch(async () => { throw new Error('network'); });
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'A', iconImage: img() })).code, 'BAD_ICON');
  __setIconFetch(async () => Buffer.alloc(LIMITS.BOOSTER_ROLE_ICON_BYTES + 1, 0xff));
  assert.equal((await createBoosterRole({ guild: w.guild, member, settings, name: 'A', iconImage: img() })).code, 'BAD_ICON');
  assert.equal(w.log.created.length, 0);
  assert.equal(cooldownLeftMs('GI2', 'U1'), 0, 'refusals are free');

  // a good emoji, then (another member) a good image passed to Discord as a Buffer
  const emoji = await createBoosterRole({ guild: w.guild, member, settings, name: 'Gem', iconEmoji: '💎' });
  assert.equal(emoji.ok, true);
  assert.equal(w.log.created[0].unicodeEmoji, '💎');
  assert.equal(w.log.created[0].icon, undefined);
  const other = w.addMember('U2', { boostingSince: BOOSTING });
  const seen = [];
  __setIconFetch(async (url) => { seen.push(url); return PNG; });
  const image = await createBoosterRole({ guild: w.guild, member: other, settings, name: 'Pic', iconImage: img({ url: 'https://cdn.test/pic.png' }) });
  assert.equal(image.ok, true);
  assert.deepEqual(seen, ['https://cdn.test/pic.png']);
  assert.ok(Buffer.isBuffer(w.log.created[1].icon));
  assert.equal(w.log.created[1].unicodeEmoji, undefined);
  __setIconFetch(async () => JPEG);
  const third = w.addMember('U3', { boostingSince: BOOSTING });
  assert.equal((await createBoosterRole({ guild: w.guild, member: third, settings, name: 'Jpg', iconImage: img({ contentType: 'image/jpeg' }) })).ok, true);
});

test('create: any failure after the role exists deletes the role again and saves nothing', async () => {
  reset();
  const settings = settingsFor();
  // giving the role fails
  const a = makeWorld({ id: 'GR1' });
  const ma = a.addMember('U1', { boostingSince: BOOSTING });
  a.faults.add = apiError(50013, 'Missing Permissions');
  const resA = await createBoosterRole({ guild: a.guild, member: ma, settings, name: 'A' });
  assert.equal(resA.ok, false);
  assert.equal(a.log.created.length, 1);
  assert.deepEqual(a.log.deleted.map((d) => d.roleId), [a.log.created[0].roleId]);
  assert.equal(a.roles.has(a.log.created[0].roleId), false);
  assert.equal(rows.size, 0);
  // the row cannot be saved (database error)
  resetCooldowns();
  const b = makeWorld({ id: 'GR2' });
  const mb = b.addMember('U1', { boostingSince: BOOSTING });
  faults.create = 'P1001';
  const resB = await createBoosterRole({ guild: b.guild, member: mb, settings, name: 'B' });
  assert.equal(resB.code, 'FAILED');
  assert.equal(b.roles.has(b.log.created[0].roleId), false);
  assert.equal(rows.size, 0);
  // someone else won the row (unique violation)
  resetCooldowns();
  const c = makeWorld({ id: 'GR3' });
  const mc = c.addMember('U1', { boostingSince: BOOSTING });
  faults.create = 'P2002';
  assert.equal((await createBoosterRole({ guild: c.guild, member: mc, settings, name: 'C' })).code, 'ALREADY');
  assert.equal(c.roles.has(c.log.created[0].roleId), false);
  // Discord refuses the create itself: nothing to clean up
  resetCooldowns();
  const d = makeWorld({ id: 'GR4' });
  const md = d.addMember('U1', { boostingSince: BOOSTING });
  d.faults.create = apiError(30005, 'Maximum number of server roles reached');
  const resD = await createBoosterRole({ guild: d.guild, member: md, settings, name: 'D' });
  assert.equal(resD.code, 'DISCORD');
  assert.match(resD.detail.text, /limit/);
  assert.equal(d.log.deleted.length, 0);
});

test('create: a role that cannot be moved is kept and reported', async () => {
  reset();
  const w = makeWorld({ id: 'GM1' });
  w.roles.get('R2').tags = { premiumSubscriberRole: true }; // so the role has to be moved
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  w.faults.setPosition = apiError(50013, 'Missing Permissions');
  const res = await createBoosterRole({ guild: w.guild, member, settings: settingsFor(), name: 'Stuck' });
  assert.equal(res.ok, true);
  assert.equal(res.moved, false);
  assert.equal(rows.size, 1);
});

// ── Editing and deleting ──────────────────────────────────────────────────────

async function withRole(id, name = 'Mine', worldOpts = {}) {
  const w = makeWorld({ id, ...worldOpts });
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  const res = await createBoosterRole({ guild: w.guild, member, settings: settingsFor(), name });
  assert.equal(res.ok, true);
  resetCooldowns();
  return { w, member, role: res.role };
}

test('edit: name, colour and icon of the own role; the own name does not collide; nothing to change is refused', async () => {
  reset();
  const { w, member, role } = await withRole('GE1', 'Mine', { roleNames: ['Moderator'] });
  assert.equal((await editBoosterRole({ guild: w.guild, member })).code, 'NOTHING');
  assert.equal((await editBoosterRole({ guild: w.guild, member, name: '   ' })).code, 'NOTHING');
  assert.equal((await editBoosterRole({ guild: w.guild, member, name: 'moderator' })).code, 'BAD_NAME');
  assert.equal((await editBoosterRole({ guild: w.guild, member, color: 'nope' })).code, 'BAD_COLOR');
  assert.equal(cooldownLeftMs('GE1', 'U1'), 0, 'refusals are free');
  // re-casing the own role is fine
  const recase = await editBoosterRole({ guild: w.guild, member, name: 'MINE' });
  assert.equal(recase.ok, true);
  assert.deepEqual(recase.changed, ['name']);
  assert.deepEqual(w.log.edits.at(-1), { roleId: role.id, name: 'MINE', reason: w.log.edits.at(-1).reason });
  // the cooldown now holds
  assert.equal((await editBoosterRole({ guild: w.guild, member, color: '#00ff00' })).code, 'COOLDOWN');
  resetCooldowns();
  const colour = await editBoosterRole({ guild: w.guild, member, color: '#00ff00', iconEmoji: '🔥' });
  assert.deepEqual(colour.changed, ['colour', 'icon']);
  const last = w.log.edits.at(-1);
  assert.deepEqual(last.colors, { primaryColor: 0x00ff00 });
  assert.equal(last.unicodeEmoji, '🔥');
  assert.equal(last.name, undefined, 'only what was asked for is sent');
  assert.equal(last.permissions, undefined, 'permissions are never touched');
  assert.match(last.reason, /^Server Boosts: /);
});

test('edit: a lost role is given back, a deleted role drops the row, a role above the bot cannot be edited, Discord errors are explained', async () => {
  reset();
  const { w, member, role } = await withRole('GE2');
  member.roles.cache.delete(role.id);
  const given = w.log.added.length;
  assert.equal((await editBoosterRole({ guild: w.guild, member, name: 'Again' })).ok, true);
  assert.equal(w.log.added.length, given + 1);
  assert.ok(member.roles.cache.has(role.id));

  resetCooldowns();
  role.editable = false;
  assert.equal((await editBoosterRole({ guild: w.guild, member, name: 'Higher' })).code, 'BOT_ROLE');
  role.editable = true;
  w.faults.edit = apiError(50035, 'Invalid Form Body');
  const refused = await editBoosterRole({ guild: w.guild, member, name: 'Broken' });
  assert.equal(refused.code, 'DISCORD');
  assert.match(refused.detail.text, /rejected/);
  w.faults.edit = null;

  resetCooldowns();
  w.roles.delete(role.id);
  assert.equal((await editBoosterRole({ guild: w.guild, member, name: 'Late' })).code, 'NO_ROLE');
  assert.equal(rows.size, 0, 'the row of a role that no longer exists is dropped');
  assert.equal((await editBoosterRole({ guild: w.guild, member, name: 'Late' })).code, 'NO_ROLE');
});

test('delete: removes the role and the row, also for a member who stopped boosting; nothing to delete is explained', async () => {
  reset();
  const { w, member, role } = await withRole('GD1');
  member.premiumSinceTimestamp = null;
  const res = await deleteOwnRole({ guild: w.guild, member });
  assert.equal(res.ok, true);
  assert.equal(w.roles.has(role.id), false);
  assert.equal(rows.size, 0);
  assert.match(w.log.deleted[0].reason, /^Server Boosts: /);
  assert.equal((await deleteOwnRole({ guild: w.guild, member })).code, 'NO_ROLE');

  // a role Discord will not let go keeps its row
  reset();
  const b = await withRole('GD2');
  b.w.faults.deleteCode = 50013;
  assert.equal((await deleteOwnRole({ guild: b.w.guild, member: b.member })).code, 'BOT_ROLE');
  assert.equal(rows.size, 1);
  b.w.faults.deleteCode = null;
  // a role that is already gone counts as deleted
  b.w.roles.delete(b.role.id);
  assert.equal((await deleteOwnRole({ guild: b.w.guild, member: b.member })).ok, true);
  assert.equal(rows.size, 0);
});

test('remove (admin): deletes that member\'s role and row', async () => {
  reset();
  const { w, role } = await withRole('GX1');
  const none = await removeMemberRole({ guild: w.guild, userId: 'NOBODY', actorId: 'ADMIN' });
  assert.equal(none.code, 'NO_ROLE');
  const res = await removeMemberRole({ guild: w.guild, userId: 'U1', actorId: 'ADMIN' });
  assert.deepEqual(res, { ok: true, roleId: role.id });
  assert.equal(w.roles.has(role.id), false);
  assert.match(w.log.deleted[0].reason, /ADMIN/);
  assert.equal(rows.size, 0);
});

// ── The thank-you ─────────────────────────────────────────────────────────────

test('postThankYou: pings only the booster, never @everyone or roles; a preview pings nobody and is labelled', async () => {
  reset();
  const w = makeWorld({ id: 'GT1', boostCount: 3, tier: 1 });
  w.addChannel('CH');
  const member = w.addMember('U1');
  const settings = settingsFor({ boostChannelId: 'CH', boostMessage: '{user} boosted! @everyone <@&99> {boosts}/{tier}' });
  const res = await postThankYou(w.guild, member, settings);
  assert.deepEqual(res, { ok: true, channelId: 'CH' });
  assert.equal(w.log.sent[0].content, '<@U1> boosted! @everyone <@&99> 3/1');
  assert.deepEqual(w.log.sent[0].allowedMentions, { parse: [], users: ['U1'] });

  const prev = await postThankYou(w.guild, member, settings, { preview: true });
  assert.equal(prev.ok, true);
  assert.match(w.log.sent[1].content, /^🧪 \*\*Preview of the boost thank-you\*\*/);
  assert.ok(w.log.sent[1].content.length <= 2000);
  assert.deepEqual(w.log.sent[1].allowedMentions, { parse: [] });

  // no channel, a missing channel, no permission, a failing send
  assert.deepEqual(await postThankYou(w.guild, member, settingsFor()), { ok: false, reason: 'no-channel' });
  assert.equal((await postThankYou(w.guild, member, settingsFor({ boostChannelId: 'MISSING' }))).reason, 'unusable');
  w.addChannel('NOPE', { denied: ['SendMessages'] });
  assert.equal((await postThankYou(w.guild, member, settingsFor({ boostChannelId: 'NOPE' }))).reason, 'unusable');
  w.guild.channels.cache.get('CH').sendFails = true;
  assert.equal((await postThankYou(w.guild, member, settings)).reason, 'failed');
});

// ── Hooks ─────────────────────────────────────────────────────────────────────

const updateCtx = (w, oldMember, newMember, settings) => ({
  guild: w.guild, guildId: w.guild.id, oldMember, newMember, oldKnown: true, getSettings: async () => settings,
});

test('guildMemberUpdate, boost starts: the thank-you is posted; a returning booster keeps the role and the end is cleared', async () => {
  reset();
  const w = makeWorld({ id: 'GH1', boostCount: 5 });
  w.addChannel('CH');
  const before = w.addMember('U1');
  const after = { ...before, premiumSinceTimestamp: BOOSTING, user: before.user };
  const settings = settingsFor({ boostChannelId: 'CH' });
  addRow('GH1', 'U1', 'R1', { boostEndedAt: new Date(Date.now() - 1000) });
  await onMemberUpdate(updateCtx(w, before, after, settings));
  assert.equal(w.log.sent.length, 1);
  assert.equal(w.log.sent[0].content, '💎 <@U1> just boosted **Test Guild**! Thank you! We now have 5 boosts.');
  assert.deepEqual(w.log.sent[0].allowedMentions, { parse: [], users: ['U1'] });
  assert.equal(rows.get(rowKey('GH1', 'U1')).boostEndedAt, null);
  assert.equal(rows.size, 1, 'the row (and so the role) is kept');

  // no thank-you channel: nothing posted, nothing thrown
  await onMemberUpdate(updateCtx(w, before, after, settingsFor()));
  assert.equal(w.log.sent.length, 1);
  // unchanged boost state and no settings: nothing at all
  await onMemberUpdate(updateCtx(w, after, { ...after }, settings));
  await onMemberUpdate(updateCtx(w, before, after, null));
  assert.equal(w.log.sent.length, 1);
});

test('guildMemberUpdate, boost ends: with no grace the role goes at once; with grace the end is stamped and the sweep seeded', async () => {
  reset();
  const noGrace = makeWorld({ id: 'GH2' });
  const m1 = noGrace.addMember('U1', { boostingSince: BOOSTING });
  const res = await createBoosterRole({ guild: noGrace.guild, member: m1, settings: settingsFor(), name: 'Gone Soon' });
  assert.equal(res.ok, true);
  const stopped = { ...m1, premiumSinceTimestamp: null, user: m1.user };
  await onMemberUpdate(updateCtx(noGrace, m1, stopped, settingsFor({ boostRoleGraceDays: 0 })));
  assert.equal(noGrace.roles.has(res.role.id), false);
  assert.equal(rows.size, 0);

  reset();
  const grace = makeWorld({ id: 'GH3' });
  const m2 = grace.addMember('U1', { boostingSince: BOOSTING });
  const made = await createBoosterRole({ guild: grace.guild, member: m2, settings: settingsFor(), name: 'Stays' });
  jobs.clear();
  const stopped2 = { ...m2, premiumSinceTimestamp: null, user: m2.user };
  await onMemberUpdate(updateCtx(grace, m2, stopped2, settingsFor({ boostRoleGraceDays: 3 })));
  assert.equal(grace.roles.has(made.role.id), true, 'the role stays during the grace period');
  const row = rows.get(rowKey('GH3', 'U1'));
  assert.ok(row.boostEndedAt instanceof Date);
  assert.ok(jobs.has('boosts.sweep:GH3'), 'the sweep will remove it later');

  // a member with no booster role: nothing happens
  const plain = grace.addMember('U9', { boostingSince: BOOSTING });
  await onMemberUpdate(updateCtx(grace, plain, { ...plain, premiumSinceTimestamp: null, user: plain.user }, settingsFor()));
  assert.equal(rows.size, 1);
});

test('guildMemberRemove: the role and row of the member who left go', async () => {
  reset();
  const { w, role } = await withRole('GH4');
  await onMemberRemove({ guild: w.guild, guildId: 'GH4', user: { id: 'NOBODY', bot: false } });
  assert.equal(rows.size, 1);
  await onMemberRemove({ guild: w.guild, guildId: 'GH4', user: { id: 'U1', bot: false } });
  assert.equal(w.roles.has(role.id), false);
  assert.equal(rows.size, 0);
  await onMemberRemove({ guild: w.guild, guildId: 'GH4', user: undefined }); // nothing to do, no throw
});

test('roleDelete drops the row of that role and forgets a deleted anchor; channelDelete forgets the thank-you channel', async () => {
  reset();
  addRow('GH5', 'U1', 'ROLE-A');
  addRow('GH5', 'U2', 'ROLE-B');
  await seedSettings('GH5', { boostRoleAnchorId: 'ANCHOR', boostChannelId: 'CH' });
  const ctx = (extra) => ({ guildId: 'GH5', getSettings: async () => settingsRows.get('GH5'), ...extra });
  await onRoleDelete(ctx({ role: { id: 'ROLE-A' } }));
  assert.deepEqual([...rows.values()].map((r) => r.userId), ['U2']);
  assert.equal(settingsRows.get('GH5').boostRoleAnchorId, 'ANCHOR');
  await onRoleDelete(ctx({ role: { id: 'ANCHOR' } }));
  assert.equal(settingsRows.get('GH5').boostRoleAnchorId, null);
  await onRoleDelete(ctx({ role: undefined }));

  await onChannelDelete(ctx({ channel: { id: 'OTHER' } }));
  assert.equal(settingsRows.get('GH5').boostChannelId, 'CH');
  await onChannelDelete(ctx({ channel: { id: 'CH' } }));
  assert.equal(settingsRows.get('GH5').boostChannelId, null);
  await onChannelDelete({ guildId: 'GH5', channel: { id: 'CH' }, getSettings: async () => null }); // no settings: no throw
});

test('guildReady seeds the sweep only for a guild with booster roles, and keeps a live job', async () => {
  reset();
  await onGuildReady({ guildId: 'GH6' });
  assert.equal(jobs.size, 0);
  addRow('GH6', 'U1', 'ROLE-A');
  await onGuildReady({ guildId: 'GH6' });
  const first = jobs.get('boosts.sweep:GH6');
  assert.equal(first.type, 'boosts.sweep');
  assert.ok(first.runAt.getTime() >= Date.now() + BOOST.STARTUP_DELAY_MS - 1000);
  const runAt = first.runAt.getTime();
  await onGuildReady({ guildId: 'GH6' });
  assert.equal(jobs.get('boosts.sweep:GH6').runAt.getTime(), runAt, 'a live job is not moved');
  // a finished or failed job is created again
  jobs.get('boosts.sweep:GH6').status = 'DONE';
  assert.equal(await ensureSweepJob('GH6'), true);
  assert.equal(jobs.get('boosts.sweep:GH6').status, 'PENDING');
  // a database error is logged, not thrown
  faults.rowsFail = true;
  await onGuildReady({ guildId: 'GH6' });
});

// ── The sweep ─────────────────────────────────────────────────────────────────

const DAY = BOOST.DAY_MS;

test('sweep: deletes roles of members who left or whose grace is over, repairs rows, never acts on a failed lookup', async () => {
  reset();
  const w = makeWorld({ id: 'GS1' });
  const settings = { ...settingsFor(), boostRoleGraceDays: 2 };
  const withRole = (roleId) => w.roles.set(roleId, { id: roleId, name: roleId, position: 3 });
  const stamp = (days) => new Date(Date.now() - days * DAY);

  // left the server (not in the server at all: Unknown Member) -> role and row go
  withRole('RL');
  addRow('GS1', 'LEFT', 'RL');
  // the lookup fails with a gateway error -> nothing is touched
  withRole('RU');
  addRow('GS1', 'UNKNOWN', 'RU');
  w.guild.members.fetchFails.add('UNKNOWN');
  // boosts again: the end is cleared and the role kept
  w.addMember('AGAIN', { boostingSince: BOOSTING });
  withRole('RA');
  addRow('GS1', 'AGAIN', 'RA', { boostEndedAt: stamp(1) });
  // stopped while the bot was offline: the grace period starts now
  w.addMember('OFFLINE');
  withRole('RO');
  addRow('GS1', 'OFFLINE', 'RO');
  // grace over -> deleted; grace running -> kept
  w.addMember('OVER');
  withRole('RV');
  addRow('GS1', 'OVER', 'RV', { boostEndedAt: stamp(3) });
  w.addMember('WAIT');
  withRole('RW');
  addRow('GS1', 'WAIT', 'RW', { boostEndedAt: stamp(1) });
  // a member who still boosts is left alone
  w.addMember('FINE', { boostingSince: BOOSTING });
  withRole('RF');
  addRow('GS1', 'FINE', 'RF');
  // left, and the role is already gone (Unknown Role): the row is dropped all the same
  addRow('GS1', 'GHOST', 'NO-SUCH-ROLE');

  const out = await sweepGuild(w.guild, [...rows.values()], settings);
  assert.deepEqual(out, { deleted: 3, ended: 1, resumed: 1, failed: 0, more: false });
  assert.deepEqual([...rows.values()].map((r) => r.userId).sort(), ['AGAIN', 'FINE', 'OFFLINE', 'UNKNOWN', 'WAIT']);
  assert.equal(rows.get(rowKey('GS1', 'AGAIN')).boostEndedAt, null);
  assert.ok(rows.get(rowKey('GS1', 'OFFLINE')).boostEndedAt instanceof Date);
  assert.deepEqual(w.log.deleted.map((d) => d.roleId).sort(), ['RL', 'RV']);
  assert.ok(w.roles.has('RU'), 'an unknown member keeps the role');
  for (const kept of ['RW', 'RO', 'RA', 'RF']) assert.ok(w.roles.has(kept), kept);
});

test('sweep: with no grace a member who stopped is removed; Missing Permissions keeps the row; the module being off changes nothing', async () => {
  reset();
  const w = makeWorld({ id: 'GS2' });
  w.addMember('STOPPED');
  w.roles.set('RS', { id: 'RS', name: 'RS', position: 3 });
  addRow('GS2', 'STOPPED', 'RS');
  const off = settingsFor({ boostsEnabled: false, boostRoleGraceDays: 0 });
  w.faults.deleteCode = 50013;
  const blocked = await sweepGuild(w.guild, [...rows.values()], off);
  assert.equal(blocked.failed, 1);
  assert.equal(rows.size, 1, 'the row stays so the next sweep tries again');
  w.faults.deleteCode = null;
  const ok = await sweepGuild(w.guild, [...rows.values()], off);
  assert.equal(ok.deleted, 1);
  assert.equal(rows.size, 0);
  assert.equal(w.roles.has('RS'), false);
});

test('sweep: bounded per run (deletions and API lookups) and comes back soon when something was left', async () => {
  reset();
  const w = makeWorld({ id: 'GS3' });
  const total = BOOST.DELETES_PER_RUN + 5;
  for (let i = 0; i < total; i++) {
    w.roles.set(`RD${i}`, { id: `RD${i}`, name: `RD${i}`, position: 3 });
    w.addMember(`D${i}`);
    addRow('GS3', `D${i}`, `RD${i}`);
  }
  const settings = settingsFor({ boostRoleGraceDays: 0 });
  const first = await sweepGuild(w.guild, [...rows.values()], settings);
  assert.equal(first.deleted, BOOST.DELETES_PER_RUN);
  assert.equal(first.more, true);
  const second = await sweepGuild(w.guild, [...rows.values()], settings);
  assert.equal(second.deleted, 5);
  assert.equal(second.more, false);
  assert.equal(rows.size, 0);

  // members who are in the server but not cached are looked up over the API, at most LOOKUPS_PER_RUN a run (a fetched
  // member is cached, so the next run only looks up the rest)
  reset();
  const v = makeWorld({ id: 'GS4' });
  const lookups = [];
  const original = v.guild.members.fetch.bind(v.guild.members);
  v.guild.members.fetch = async (id) => { lookups.push(id); return original(id); };
  const count = BOOST.LOOKUPS_PER_RUN + 10;
  for (let i = 0; i < count; i++) {
    v.addMember(`P${i}`, { boostingSince: BOOSTING, cached: false });
    addRow('GS4', `P${i}`, `RP${i}`);
  }
  const rowsNow = () => [...rows.values()];
  const run1 = await sweepGuild(v.guild, rowsNow(), settingsFor());
  assert.equal(lookups.length, BOOST.LOOKUPS_PER_RUN);
  assert.equal(run1.more, true);
  const run2 = await sweepGuild(v.guild, rowsNow(), settingsFor());
  assert.equal(lookups.length, count, 'the second run looks up the rest');
  assert.equal(new Set(lookups).size, count, 'nobody is looked up twice');
  assert.equal(run2.more, false);
  assert.equal(rows.size, count, 'everybody still boosts, so nothing is removed');
});

test('REVIEW: sweep: a slow Discord cannot push a run past the scheduler timeout; lookups and removals stop at the time budget', async () => {
  // Lookups that take long (Discord slow, 8 s timeouts) stop at the budget; the next runs continue, nobody twice.
  reset();
  const v = makeWorld({ id: 'GS9' });
  const lookups = [];
  const original = v.guild.members.fetch.bind(v.guild.members);
  v.guild.members.fetch = async (id) => {
    lookups.push(id);
    await new Promise((r) => setTimeout(r, 30));
    return original(id);
  };
  for (let i = 0; i < 6; i++) {
    v.addMember(`S${i}`, { boostingSince: BOOSTING, cached: false });
    addRow('GS9', `S${i}`, `RS${i}`);
  }
  const rowsNow = () => [...rows.values()];
  const run1 = await sweepGuild(v.guild, rowsNow(), settingsFor(), { budgetMs: 70 });
  assert.ok(lookups.length >= 1 && lookups.length < 6, `stopped at the budget after ${lookups.length} lookups`);
  assert.equal(run1.more, true, 'comes back in a minute for the rest');
  for (let i = 0; i < 10 && lookups.length < 6; i++) await sweepGuild(v.guild, rowsNow(), settingsFor(), { budgetMs: 70 });
  assert.equal(lookups.length, 6);
  assert.equal(new Set(lookups).size, 6, 'nobody is looked up twice');
  assert.ok(BOOST.SWEEP_BUDGET_MS + BOOST.LOOKUP_TIMEOUT_MS < 60_000, 'the budget plus one last lookup stays inside the job timeout');

  // Removals stop at the budget too, and nothing is lost: the rows wait for the next run.
  reset();
  const w = makeWorld({ id: 'GS10' });
  for (let i = 0; i < 5; i++) {
    w.roles.set(`RT${i}`, { id: `RT${i}`, name: `RT${i}`, position: 3 });
    w.addMember(`T${i}`);
    addRow('GS10', `T${i}`, `RT${i}`);
  }
  const out = await sweepGuild(w.guild, rowsNow(), settingsFor({ boostRoleGraceDays: 0 }), { budgetMs: 0 });
  assert.equal(out.deleted, 0);
  assert.equal(out.more, true);
  assert.equal(rows.size, 5);
  assert.equal((await sweepGuild(w.guild, rowsNow(), settingsFor({ boostRoleGraceDays: 0 }))).deleted, 5);
});

test('boosts.sweep job: ends itself with no rows, reschedules otherwise, never throws', async () => {
  reset();
  const w = makeWorld({ id: 'GS5' });
  await seedSettings('GS5', { boostRoleGraceDays: 1 });
  const job = { guildId: 'GS5' };
  assert.equal(await sweepJobHandler(job, { guild: w.guild }), undefined);

  w.addMember('U1', { boostingSince: BOOSTING });
  w.roles.set('RX', { id: 'RX', name: 'RX', position: 3 });
  addRow('GS5', 'U1', 'RX');
  const next = await sweepJobHandler(job, { guild: w.guild });
  const wait = next.nextRunAt.getTime() - Date.now();
  assert.ok(Math.abs(wait - BOOST.SWEEP_EVERY_MS) < 5000, `about an hour (${wait})`);

  // an unavailable guild is retried soon; a database error is a retry, not an exception
  const away = await sweepJobHandler(job, { guild: { ...w.guild, available: false } });
  assert.ok(Math.abs(away.nextRunAt.getTime() - Date.now() - BOOST.SWEEP_RETRY_MS) < 5000);
  faults.rowsFail = true;
  const broken = await sweepJobHandler(job, { guild: w.guild });
  assert.ok(Math.abs(broken.nextRunAt.getTime() - Date.now() - BOOST.SWEEP_RETRY_MS) < 5000);
  faults.rowsFail = false;
  assert.ok((await sweepJobHandler(job, {})).nextRunAt, 'no guild at all is a retry too');
});

test('boosts.sweep job: cleans up while the module is off, and comes back in a minute when it had more to do', async () => {
  reset();
  const w = makeWorld({ id: 'GS6' });
  await seedSettings('GS6', { boostsEnabled: false, boostRoleGraceDays: 0 });
  for (let i = 0; i < BOOST.DELETES_PER_RUN + 1; i++) {
    w.roles.set(`RJ${i}`, { id: `RJ${i}`, name: `RJ${i}`, position: 3 });
    w.addMember(`J${i}`);
    addRow('GS6', `J${i}`, `RJ${i}`);
  }
  const next = await sweepJobHandler({ guildId: 'GS6' }, { guild: w.guild });
  assert.equal(rows.size, 1);
  assert.ok(Math.abs(next.nextRunAt.getTime() - Date.now() - BOOST.SWEEP_MORE_MS) < 5000);
  const done = await sweepJobHandler({ guildId: 'GS6' }, { guild: w.guild });
  assert.equal(rows.size, 0);
  assert.ok(done.nextRunAt);
  assert.equal(await sweepJobHandler({ guildId: 'GS6' }, { guild: w.guild }), undefined, 'the next run finds no rows and ends');
});

// ── The command ───────────────────────────────────────────────────────────────

function fakeInteraction({ world, userId = 'U1', group = null, sub, vals = {}, member = null }) {
  const m = member ?? world.guild.members.cache.get(userId);
  const i = {
    id: String(nextId++), createdTimestamp: Date.now(), guild: world.guild, guildId: world.guild.id, user: { id: userId }, member: m,
    deferred: false, replied: false, ephemeral: null, replies: [],
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      getString: (n) => vals[n] ?? null,
      getBoolean: (n) => vals[n] ?? null,
      getInteger: (n) => vals[n] ?? null,
      getChannel: (n) => vals[n] ?? null,
      getRole: (n) => vals[n] ?? null,
      getUser: (n) => vals[n] ?? null,
      getAttachment: (n) => vals[n] ?? null,
    },
    async deferReply({ ephemeral } = {}) { this.deferred = true; this.ephemeral = Boolean(ephemeral); },
    async reply(p) { this.replied = true; this.replies.push(p); return { resource: { message: null } }; },
    async editReply(p) { this.replies.push(p); },
    async followUp(p) { this.replies.push(p); },
  };
  return i;
}
const lastEmbed = (i) => i.replies.at(-1).embeds[0].toJSON();

async function run(world, args) {
  const i = fakeInteraction({ world, ...args });
  await cmd.execute(i, {});
  return i;
}

test('/boost role create: module off, booster roles off and not boosting are refused with the right words', async () => {
  reset();
  const w = makeWorld({ id: 'GK1' });
  const booster = w.addMember('U1', { boostingSince: BOOSTING });
  w.addMember('U2');
  const create = (userId, extra = {}) => run(w, { userId, group: 'role', sub: 'create', vals: { name: 'Mine', ...extra } });

  await seedSettings('GK1', { boostsEnabled: false, boostCustomRoles: true });
  let i = await create('U1');
  assert.equal(i.ephemeral, true, 'only the member sees the answer');
  assert.equal(lastEmbed(i).title, '⚠️  Module Disabled');
  assert.equal(lastEmbed(i).description, 'The **Server Boosts** module is turned off on this server. An admin can enable it with `/module enable boosts`.');

  await seedSettings('GK1', { boostsEnabled: true, boostCustomRoles: false });
  i = await create('U1');
  assert.match(lastEmbed(i).title, /Booster Roles Are Off/);

  await seedSettings('GK1', { boostCustomRoles: true });
  i = await create('U2');
  assert.match(lastEmbed(i).title, /Boosters Only/);
  assert.equal(w.log.created.length, 0);

  i = await create('U1');
  assert.match(lastEmbed(i).title, /Booster Role Created/);
  assert.match(lastEmbed(i).description, new RegExp(`<@&${rows.get(rowKey('GK1', 'U1')).roleId}>`));
  assert.deepEqual(i.replies.at(-1).allowedMentions, { parse: [] });
  assert.equal(booster.roles.cache.size, 1);
});

test('/boost role edit and delete through the command; delete also works after the boost ended', async () => {
  reset();
  const w = makeWorld({ id: 'GK2' });
  const member = w.addMember('U1', { boostingSince: BOOSTING });
  await seedSettings('GK2', { boostsEnabled: true, boostCustomRoles: true });
  await run(w, { group: 'role', sub: 'create', vals: { name: 'Mine' } });
  resetCooldowns();
  let i = await run(w, { group: 'role', sub: 'edit', vals: { color: '#112233' } });
  assert.match(lastEmbed(i).title, /Booster Role Updated/);
  assert.match(lastEmbed(i).description, /colour/);
  resetCooldowns();
  i = await run(w, { group: 'role', sub: 'edit', vals: {} });
  assert.match(lastEmbed(i).title, /Nothing To Change/);
  i = await run(w, { group: 'role', sub: 'edit', vals: { icon_emoji: 'not an emoji' } });
  assert.match(lastEmbed(i).title, /Invalid Icon/);

  member.premiumSinceTimestamp = null;
  resetCooldowns();
  i = await run(w, { group: 'role', sub: 'edit', vals: { color: '#112233' } });
  assert.match(lastEmbed(i).title, /Boosters Only/);
  i = await run(w, { group: 'role', sub: 'delete' });
  assert.match(lastEmbed(i).title, /Booster Role Deleted/);
  assert.equal(rows.size, 0);
  i = await run(w, { group: 'role', sub: 'delete' });
  assert.match(lastEmbed(i).title, /No Booster Role/);
});

test('/boost config: shows the settings, saves what was given, refuses an unusable channel and a bad anchor', async () => {
  reset();
  const w = makeWorld({ id: 'GK3' });
  w.addMember('ADMIN');
  w.addChannel('CH');
  w.addChannel('BAD', { denied: ['SendMessages'] });
  await seedSettings('GK3', { boostsEnabled: false });
  let i = await run(w, { userId: 'ADMIN', sub: 'config' });
  assert.equal(i.ephemeral, false);
  assert.match(lastEmbed(i).description, /Module: ❌ Off/);
  assert.match(lastEmbed(i).description, /Thank-you channel: Not set/);

  i = await run(w, { userId: 'ADMIN', sub: 'config', vals: { channel: { id: 'BAD' } } });
  assert.match(lastEmbed(i).title, /Cannot Use That Channel/);
  assert.equal(settingsRows.get('GK3').boostChannelId, undefined);

  const anchor = w.roles.get('R5');
  i = await run(w, { userId: 'ADMIN', sub: 'config', vals: { channel: { id: 'CH' }, custom_roles: true, anchor, grace_days: 7 } });
  assert.match(lastEmbed(i).title, /Server Boosts Updated/);
  assert.match(lastEmbed(i).description, /<#CH>/);
  assert.match(lastEmbed(i).description, /7 days/);
  assert.match(lastEmbed(i).description, new RegExp(`<@&R5>`));
  assert.deepEqual(
    { c: settingsRows.get('GK3').boostChannelId, r: settingsRows.get('GK3').boostCustomRoles, a: settingsRows.get('GK3').boostRoleAnchorId, g: settingsRows.get('GK3').boostRoleGraceDays },
    { c: 'CH', r: true, a: 'R5', g: 7 },
  );

  i = await run(w, { userId: 'ADMIN', sub: 'config', vals: { anchor: { id: w.guild.id, position: 0 } } });
  assert.match(lastEmbed(i).title, /Invalid Role/);
  i = await run(w, { userId: 'ADMIN', sub: 'config', vals: { anchor: { id: 'UP', position: 99 } } });
  assert.match(lastEmbed(i).title, /Role Too High/);
  assert.equal(settingsRows.get('GK3').boostRoleAnchorId, 'R5');
});

test('/boost config warns when the bot cannot make roles', async () => {
  reset();
  const w = makeWorld({ id: 'GK4', manageRoles: false });
  w.addMember('ADMIN');
  await seedSettings('GK4', { boostsEnabled: true, boostCustomRoles: true });
  const i = await run(w, { userId: 'ADMIN', sub: 'config' });
  const embed = lastEmbed(i);
  assert.match(embed.fields.find((f) => f.name === 'Problems').value, /Manage Roles/);
});

test('/boost test, list and remove', async () => {
  reset();
  const w = makeWorld({ id: 'GK5' });
  const admin = w.addMember('ADMIN');
  const booster = w.addMember('U1', { boostingSince: BOOSTING });
  w.addChannel('CH');
  await seedSettings('GK5', { boostsEnabled: false, boostCustomRoles: true, boostChannelId: null });

  // no channel yet: the preview is shown in the answer
  let i = await run(w, { userId: 'ADMIN', sub: 'test' });
  assert.match(lastEmbed(i).title, /No Thank-you Channel/);
  assert.match(lastEmbed(i).description, /just boosted \*\*Test Guild\*\*/);
  assert.match(lastEmbed(i).description, /module is off/);
  assert.equal(w.log.sent.length, 0);

  await seedSettings('GK5', { boostsEnabled: true, boostChannelId: 'CH' });
  i = await run(w, { userId: 'ADMIN', sub: 'test' });
  assert.match(lastEmbed(i).title, /Preview Posted/);
  assert.equal(w.log.sent.length, 1);
  assert.match(w.log.sent[0].content, /Preview/);
  assert.deepEqual(w.log.sent[0].allowedMentions, { parse: [] });
  assert.equal(admin.id, 'ADMIN');

  // list
  i = await run(w, { userId: 'ADMIN', sub: 'list' });
  assert.match(lastEmbed(i).title, /No Booster Roles/);
  const made = await createBoosterRole({ guild: w.guild, member: booster, settings: settingsFor(), name: 'Listed' });
  assert.equal(made.ok, true);
  rows.get(rowKey('GK5', 'U1')).boostEndedAt = new Date(Date.now() - 60_000);
  i = await run(w, { userId: 'ADMIN', sub: 'list' });
  assert.match(lastEmbed(i).title, /Booster Roles \(1\)/);
  assert.match(lastEmbed(i).description, new RegExp(`<@&${made.role.id}> · <@U1> · boost ended <t:\\d+:R>`));
  assert.deepEqual(i.replies.at(-1).allowedMentions, { parse: [] });

  // remove
  i = await run(w, { userId: 'ADMIN', sub: 'remove', vals: { user: { id: 'NOBODY' } } });
  assert.match(lastEmbed(i).title, /Nothing To Remove/);
  i = await run(w, { userId: 'ADMIN', sub: 'remove', vals: { user: { id: 'U1' } } });
  assert.match(lastEmbed(i).title, /Booster Role Removed/);
  assert.equal(w.roles.has(made.role.id), false);
  assert.equal(rows.size, 0);
});
