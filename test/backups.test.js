// Server Backups tests. Offline (no Discord, Postgres or Redis): the snapshot capture and parse, permission masking, the
// restore engine against a fake guild (missing and full modes, masking, crash and resume without duplicates, a budget per
// batch, errors that never escape, nothing ever deleted), the backup and restore jobs against in-memory ServerBackup and
// ScheduledJob tables, the server-owner-only rules of the command and of the confirmation button, the plugin declaration
// and the /backup payload.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const { ChannelType, OverwriteType, PermissionFlagsBits: P, PermissionsBitField } = await import('discord.js');
const svc = await import('../src/services/backupService.js');
const snap = await import('../src/utils/guildSnapshot.js');
const backups = await import('../src/plugins/backups.js');
const cmd = await import('../src/commands/admin/backup.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { CUSTOM_ID_PREFIXES } = await import('../src/config/customIds.js');
const { JOB_TYPES } = await import('../src/config/jobTypes.js');
const { LIMITS, PERMISSION_LEVELS, DEFAULT_MODULE_SETTINGS } = await import('../src/config/constants.js');
const { MODULES } = await import('../src/config/modules.js');
const { updateSettings } = await import('../src/services/settingsService.js');
const { PermanentJobError } = await import('../src/services/scheduler.js');

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory tables ──────────────────────────────────────────────────────────

const backupRows = new Map();   // id -> row
const jobRows = new Map();      // dedupeKey -> row
let backupSeq = 1;
let jobSeq = 1;
let tick = 0;

const matchWhere = (row, where = {}) => Object.entries(where).every(([k, v]) => {
  if (v && typeof v === 'object' && !(v instanceof Date)) {
    if ('in' in v) return v.in.includes(row[k]);
    if ('contains' in v) return String(row[k] ?? '').toLowerCase().includes(String(v.contains).toLowerCase());
    return false;
  }
  return row[k] === v;
});
const pick = (row, select) => {
  if (!row) return null;
  if (!select) return { ...row };
  return Object.fromEntries(Object.keys(select).filter((k) => select[k]).map((k) => [k, row[k]]));
};
const sorted = (rows, orderBy) => (orderBy
  ? [...rows].sort((a, b) => (b.createdAt - a.createdAt) || (a.id < b.id ? 1 : -1))
  : rows);

const realServerBackup = prisma.serverBackup;
const realScheduledJob = prisma.scheduledJob;
const realGuildSettings = prisma.guildSettings;
const realTransaction = prisma.$transaction;
prisma.serverBackup = {
  count: async ({ where }) => [...backupRows.values()].filter((r) => matchWhere(r, where)).length,
  create: async ({ data, select }) => {
    const id = `bk${String(backupSeq++).padStart(6, '0')}`;
    const row = { id, createdAt: new Date(Date.now() + (tick++)), ...data };
    backupRows.set(id, row);
    return pick(row, select);
  },
  findFirst: async ({ where, orderBy, select }) => pick(sorted([...backupRows.values()].filter((r) => matchWhere(r, where)), orderBy)[0], select),
  findMany: async ({ where, orderBy, skip = 0, take = 1000, select }) => sorted([...backupRows.values()].filter((r) => matchWhere(r, where)), orderBy)
    .slice(skip, skip + take).map((r) => pick(r, select)),
  update: async ({ where, data }) => Object.assign(backupRows.get(where.id), data),
  deleteMany: async ({ where }) => {
    const hit = [...backupRows.values()].filter((r) => matchWhere(r, where));
    for (const r of hit) backupRows.delete(r.id);
    return { count: hit.length };
  },
};
prisma.guildSettings = { upsert: async ({ where, update }) => ({ guildId: where.guildId, ...update }) };
prisma.scheduledJob = {
  findUnique: async ({ where }) => (jobRows.has(where.dedupeKey) ? { ...jobRows.get(where.dedupeKey) } : null),
  create: async ({ data }) => {
    const row = { id: `job${jobSeq++}`, status: 'PENDING', attempts: 0, version: 0, lastError: null, ...data };
    jobRows.set(data.dedupeKey, row);
    return { ...row };
  },
  update: async ({ where, data }) => {
    const row = [...jobRows.values()].find((r) => r.id === where.id);
    const { version, ...rest } = data;
    Object.assign(row, rest, version?.increment ? { version: row.version + version.increment } : {});
    return { ...row };
  },
};
prisma.$transaction = async (fn) => fn({
  $queryRaw: async (sql) => {
    const row = jobRows.get(sql.values[0]);
    return row ? [{ id: row.id, runAt: row.runAt, status: row.status }] : [];
  },
  scheduledJob: prisma.scheduledJob,
});

after(async () => {
  prisma.serverBackup = realServerBackup;
  prisma.scheduledJob = realScheduledJob;
  prisma.guildSettings = realGuildSettings;
  prisma.$transaction = realTransaction;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Fake guild ────────────────────────────────────────────────────────────────

const bigPerms = (...flags) => flags.reduce((a, f) => a | f, 0n);
const BOT_PERMS = [P.ViewChannel, P.SendMessages, P.EmbedLinks, P.ManageRoles, P.ManageChannels, P.KickMembers, P.BanMembers, P.ManageMessages, P.ReadMessageHistory];
const BOT_TOP = 10;
let nextId = 5000;
const newId = () => String(nextId++);

const mapOf = (items) => {
  const m = new Map();
  for (const x of items) m.set(x.id, x);
  return m;
};

function fakeRole(spec) {
  return {
    id: spec.id, name: spec.name, colors: { primaryColor: spec.color ?? 0 }, hoist: spec.hoist ?? false, mentionable: spec.mentionable ?? false,
    permissions: { bitfield: spec.perms ?? 0n }, rawPosition: spec.pos ?? 1, unicodeEmoji: spec.emoji ?? null, managed: spec.managed ?? false,
    createdTimestamp: spec.created ?? 1,
  };
}

function fakeChannel(spec) {
  return {
    id: spec.id, type: spec.type ?? ChannelType.GuildText, name: spec.name, parentId: spec.parent ?? null, rawPosition: spec.pos ?? 0,
    topic: spec.topic ?? null, nsfw: spec.nsfw ?? false, rateLimitPerUser: spec.slow ?? 0, bitrate: spec.bitrate ?? null, userLimit: spec.limit ?? 0,
    defaultAutoArchiveDuration: spec.archive ?? null, availableTags: spec.tags ?? [], createdTimestamp: spec.created ?? 1, manageable: spec.manageable ?? true,
    permissionOverwrites: { cache: mapOf((spec.overwrites ?? []).map((o) => ({
      id: o.id, type: o.type ?? OverwriteType.Role, allow: { bitfield: o.allow ?? 0n }, deny: { bitfield: o.deny ?? 0n },
    }))) },
    isThread: () => spec.thread === true,
    isTextBased: () => [ChannelType.GuildText, ChannelType.GuildAnnouncement].includes(spec.type ?? ChannelType.GuildText),
    guild: null,
    permissionsFor: () => ({ has: () => spec.canSend !== false }),
    sent: [],
    async send(payload) { this.sent.push(payload); return { id: newId() }; },
  };
}

/**
 * A fake guild with the manager methods the restore uses. `log` records every mutating call, `members` are the ids that
 * can be fetched, `fail` names a manager method that rejects once per call.
 */
function makeGuild({ id = '900', ownerId = 'OWNER', roles = [], channels = [], botPerms = BOT_PERMS, botTop = BOT_TOP, members = [], everyonePerms = 0n,
  features = [], fail = {} } = {}) {
  const log = { create: [], edit: [], put: [], rolePositions: [], channelPositions: [], fetches: [] };
  const roleCache = mapOf(roles.map(fakeRole));
  const everyone = fakeRole({ id, name: '@everyone', perms: everyonePerms, pos: 0 });
  roleCache.set(id, everyone);
  const channelCache = mapOf(channels.map(fakeChannel));
  const me = {
    permissions: new PermissionsBitField(botPerms),
    roles: { highest: { rawPosition: botTop } },
  };
  const guild = {
    id, name: 'Test Server', ownerId, available: true, features, maximumBitrate: 96000, members: null,
    roles: {
      cache: roleCache, everyone,
      async create(data) {
        if (fail.roleCreate) throw Object.assign(new Error('Maximum number of server roles reached'), { code: 30005 });
        log.create.push(['role', data]);
        const role = fakeRole({
          id: newId(), name: data.name, color: data.colors?.primaryColor, hoist: data.hoist, mentionable: data.mentionable, perms: data.permissions,
          emoji: data.unicodeEmoji, pos: 1, created: Date.now(),
        });
        roleCache.set(role.id, role);
        return role;
      },
      async edit(role, data) {
        log.edit.push(['role', role.id, data]);
        if (data.name !== undefined) role.name = data.name;
        if (data.colors) role.colors = { primaryColor: data.colors.primaryColor };
        if (data.hoist !== undefined) role.hoist = data.hoist;
        if (data.mentionable !== undefined) role.mentionable = data.mentionable;
        if (data.permissions !== undefined) role.permissions = { bitfield: data.permissions };
        if (data.unicodeEmoji !== undefined) role.unicodeEmoji = data.unicodeEmoji;
        return role;
      },
      async setPositions(list) {
        log.rolePositions.push(list);
        for (const e of list) roleCache.get(e.role).rawPosition = e.position;
      },
    },
    channels: {
      cache: channelCache,
      async create(data) {
        if (fail.channelCreate) throw Object.assign(new Error('Missing Permissions'), { code: 50013 });
        log.create.push(['channel', data]);
        const ch = fakeChannel({
          id: newId(), type: data.type, name: data.name, parent: data.parent ?? null, pos: data.position ?? 0, topic: data.topic ?? null,
          nsfw: data.nsfw ?? false, slow: data.rateLimitPerUser ?? 0, bitrate: data.bitrate ?? null, limit: data.userLimit ?? 0, created: Date.now(),
          overwrites: (data.permissionOverwrites ?? []).map((o) => ({ id: o.id, type: o.type, allow: o.allow, deny: o.deny })),
        });
        ch.guild = guild;
        channelCache.set(ch.id, ch);
        return ch;
      },
      async edit(ch, data) {
        log.edit.push(['channel', ch.id, data]);
        if (data.name !== undefined) ch.name = data.name;
        if (data.topic !== undefined) ch.topic = data.topic || null;
        if (data.nsfw !== undefined) ch.nsfw = data.nsfw;
        if (data.rateLimitPerUser !== undefined) ch.rateLimitPerUser = data.rateLimitPerUser;
        if (data.bitrate !== undefined) ch.bitrate = data.bitrate;
        if (data.userLimit !== undefined) ch.userLimit = data.userLimit;
        if (data.parent !== undefined) ch.parentId = data.parent;
        return ch;
      },
      async setPositions(list) {
        log.channelPositions.push(list);
        for (const e of list) channelCache.get(e.channel).rawPosition = e.position;
      },
    },
    members: {
      me,
      cache: new Map(),
      async fetch(userId) {
        log.fetches.push(userId);
        if (!members.includes(userId)) throw Object.assign(new Error('Unknown Member'), { code: 10007 });
        return { id: userId };
      },
    },
    client: {
      rest: {
        async put(route, options) {
          log.put.push({ route, ...options });
          return {};
        },
      },
      users: { sent: [], async fetch(userId) { return { send: async (p) => { guild.client.users.sent.push([userId, p]); } }; } },
    },
  };
  for (const ch of channelCache.values()) ch.guild = guild;
  guild.me = me;
  guild.log = log;
  return guild;
}

// ── The server the tests back up ──────────────────────────────────────────────

const ADMIN_PERMS = bigPerms(P.Administrator, P.ManageRoles);
const MOD_PERMS = bigPerms(P.KickMembers, P.BanMembers);
const MEMBER_PERMS = bigPerms(P.ViewChannel, P.SendMessages);
const EVERYONE_PERMS = bigPerms(P.ViewChannel, P.SendMessages, P.ReadMessageHistory);

const ORIGINAL = {
  roles: [
    { id: '101', name: 'Admin', color: 0xff0000, hoist: true, perms: ADMIN_PERMS, pos: 8 },
    { id: '102', name: 'Mod', mentionable: true, perms: MOD_PERMS, pos: 6 },
    { id: '103', name: 'Member', perms: MEMBER_PERMS, pos: 3 },
    { id: '104', name: 'SomeBot', managed: true, perms: bigPerms(P.ViewChannel), pos: 2 },
  ],
  channels: [
    { id: '201', type: ChannelType.GuildCategory, name: 'Info', pos: 0, overwrites: [{ id: '900', deny: bigPerms(P.SendMessages) }] },
    {
      id: '202', name: 'general', parent: '201', pos: 0, topic: 'Say hi', slow: 5, archive: 1440,
      overwrites: [
        { id: '102', allow: bigPerms(P.ViewChannel, P.ManageMessages) },
        { id: '104', allow: bigPerms(P.ViewChannel) },
        { id: '777', type: OverwriteType.Member, deny: bigPerms(P.SendMessages) },
        { id: '778', type: OverwriteType.Member, allow: bigPerms(P.ViewChannel) },
      ],
    },
    { id: '203', type: ChannelType.GuildVoice, name: 'Lounge', parent: '201', pos: 1, bitrate: 64000, limit: 5 },
    { id: '204', type: ChannelType.PublicThread, name: 'a thread', parent: '202', thread: true },
    { id: '205', type: ChannelType.GuildDirectory, name: 'directory' },
    {
      id: '206', type: ChannelType.GuildForum, name: 'help', pos: 2,
      tags: [{ name: 'Solved', moderated: true, emoji: { id: null, name: '✅' } }, { name: 'Custom', moderated: false, emoji: { id: '55', name: 'x' } }],
    },
  ],
  everyonePerms: EVERYONE_PERMS,
};
const original = () => makeGuild({ ...ORIGINAL });
const SNAPSHOT = snap.captureGuild(original(), new Date('2026-10-21T10:00:00Z'));
const roundTrip = (v) => JSON.parse(JSON.stringify(v));

/** Runs restore batches like the job does (state through JSON between batches) until 'finish'. */
async function restoreAll(guild, { mode = 'missing', snapshot = SNAPSHOT, idMap = {}, startedAt = Date.now() - 1000, maxBatches = 40 } = {}) {
  let state = snap.newRestoreState({ backupId: 'bk000001', mode, startedById: 'OWNER', channelId: 'CH', idMap, now: startedAt });
  let batches = 0;
  let calls = [];
  for (;;) {
    state = snap.normaliseRestoreState(roundTrip(state));
    const env = snap.makeEnv({ guild, snapshot: snap.parseSnapshot(roundTrip(snapshot)), state, reason: 'test' });
    const result = await snap.runRestoreBatch(env);
    calls.push(env.budget.calls);
    batches += 1;
    if (result === 'finish') return { state, batches, calls };
    assert.ok(batches < maxBatches, 'the restore does not finish');
  }
}

const nameOf = (guild) => [...guild.roles.cache.values()].map((r) => r.name).sort();
const chanNames = (guild) => [...guild.channels.cache.values()].map((c) => c.name).sort();

// ── Snapshot ──────────────────────────────────────────────────────────────────

test('captureGuild: roles, categories, channels and overwrites as strings; no threads; unsupported types counted', () => {
  assert.equal(SNAPSHOT.version, 1);
  assert.equal(SNAPSHOT.everyone.permissions, String(EVERYONE_PERMS));
  assert.deepEqual(SNAPSHOT.roles.map((r) => r.id), ['104', '103', '102', '101'], 'by position, @everyone excluded');
  const admin = SNAPSHOT.roles.find((r) => r.id === '101');
  assert.deepEqual(admin, {
    id: '101', name: 'Admin', color: 0xff0000, hoist: true, mentionable: false, permissions: String(ADMIN_PERMS), position: 8, unicodeEmoji: null, managed: false,
  });
  assert.equal(SNAPSHOT.roles.find((r) => r.id === '104').managed, true);
  assert.deepEqual(SNAPSHOT.categories.map((c) => c.id), ['201']);
  assert.deepEqual(SNAPSHOT.channels.map((c) => c.id), ['202', '203', '206']);
  assert.equal(SNAPSHOT.skipped.channels, 1, 'the directory channel');
  const general = SNAPSHOT.channels.find((c) => c.id === '202');
  assert.equal(general.parentId, '201');
  assert.equal(general.topic, 'Say hi');
  assert.equal(general.rateLimitPerUser, 5);
  assert.deepEqual(general.overwrites[0], { id: '102', type: 'role', allow: String(bigPerms(P.ViewChannel, P.ManageMessages)), deny: '0' });
  assert.deepEqual(general.overwrites[2], { id: '777', type: 'member', allow: '0', deny: String(P.SendMessages) });
  const forum = SNAPSHOT.channels.find((c) => c.id === '206');
  assert.deepEqual(forum.tags, [{ name: 'Solved', moderated: true, emojiName: '✅' }, { name: 'Custom', moderated: false, emojiName: null }]);
  for (const o of SNAPSHOT.channels.flatMap((c) => c.overwrites)) assert.equal(typeof o.allow, 'string');
  assert.equal(JSON.stringify(SNAPSHOT).includes('a thread'), false);
  assert.deepEqual(snap.snapshotStats(SNAPSHOT), { roles: 4, categories: 1, channels: 3, overwrites: 5 });
});

test('parseSnapshot: version check, defensive filtering, deny never overlaps allow; exportable drops the id map', () => {
  assert.throws(() => snap.parseSnapshot(null), snap.SnapshotError);
  assert.throws(() => snap.parseSnapshot({ version: 2 }), /version 2/);
  const parsed = snap.parseSnapshot({
    ...roundTrip(SNAPSHOT),
    roles: [...roundTrip(SNAPSHOT).roles, { id: 'nope', name: 'bad' }, null],
    channels: [{ id: '300', type: ChannelType.GuildText, name: 'x', overwrites: [{ id: '5', type: 'role', allow: '3', deny: '7' }, { id: 'zz' }] }],
    idMap: { 101: '999', abc: '1', 102: 'x' },
  });
  assert.equal(parsed.roles.length, 4, 'unreadable roles are dropped');
  assert.deepEqual(parsed.channels[0].overwrites, [{ id: '5', type: 'role', allow: '3', deny: '4' }]);
  assert.deepEqual(parsed.idMap, { 101: '999' });
  assert.equal('idMap' in snap.exportable({ ...SNAPSHOT, idMap: { 1: '2' } }), false);
  assert.equal('idMap' in snap.exportable(SNAPSHOT), false);
});

test('maskBits: the bot gives only what it has and keeps what it cannot control', () => {
  const bot = bigPerms(P.ViewChannel, P.SendMessages);
  assert.deepEqual(snap.maskBits(bigPerms(P.ViewChannel, P.Administrator), bot), { value: P.ViewChannel, dropped: P.Administrator });
  // The target already has Administrator: it stays, and nothing counts as dropped.
  assert.deepEqual(snap.maskBits(bigPerms(P.Administrator), bot, P.Administrator), { value: P.Administrator, dropped: 0n });
  // Bits the bot cannot control are not taken away even when the backup lacks them.
  assert.equal(snap.maskBits(0n, bot, bigPerms(P.ViewChannel, P.BanMembers)).value, P.BanMembers);
  assert.equal(snap.botBits({ permissions: new PermissionsBitField([P.Administrator]) }), PermissionsBitField.All);
  assert.equal(snap.bits('12'), 12n);
  assert.equal(snap.bits('x'), 0n);
});

test('the snapshot of a big server stays under BACKUP_BYTES, and a huge one is measured', () => {
  assert.ok(snap.snapshotBytes(SNAPSHOT) < LIMITS.BACKUP_BYTES);
  const channels = Array.from({ length: 500 }, (_, i) => ({
    id: String(1000 + i), name: `channel-${i}`, pos: i, overwrites: Array.from({ length: 10 }, (__, j) => ({ id: String(100 + j), allow: bigPerms(P.ViewChannel) })),
  }));
  const big = snap.captureGuild(makeGuild({ channels, roles: Array.from({ length: 250 }, (_, i) => ({ id: String(2000 + i), name: `r${i}`, pos: i + 1 })) }));
  assert.ok(snap.snapshotBytes(big) < LIMITS.BACKUP_BYTES, `500 channels with 10 overwrites and 250 roles is ${snap.snapshotBytes(big)} bytes`);
});

// ── Restore engine ────────────────────────────────────────────────────────────

test('mode missing: recreates only what is gone, maps parents and overwrite targets, edits nothing that exists', async () => {
  // Admin (101) and the integration role (104) and the forum still exist; Mod, Member, the category, general and Lounge are gone.
  const guild = makeGuild({
    roles: [{ id: '101', name: 'Admin (renamed)', perms: bigPerms(P.ViewChannel), pos: 8 }, { id: '104', name: 'SomeBot', managed: true, pos: 2 }, { id: '999', name: 'Bot', pos: 10 }],
    channels: [{ id: '206', type: ChannelType.GuildForum, name: 'help (renamed)', pos: 2 }],
    members: ['777'], everyonePerms: bigPerms(P.ViewChannel),
  });
  const { state } = await restoreAll(guild);

  assert.deepEqual(guild.log.edit, [], 'mode missing edits nothing that exists');
  assert.equal(guild.roles.cache.get('101').name, 'Admin (renamed)');
  assert.equal(guild.channels.cache.get('206').name, 'help (renamed)');
  assert.equal(guild.roles.everyone.permissions.bitfield, bigPerms(P.ViewChannel), '@everyone is only reset in mode full');
  assert.deepEqual(nameOf(guild).filter((n) => ['Mod', 'Member'].includes(n)), ['Member', 'Mod']);
  assert.equal(nameOf(guild).filter((n) => n === 'SomeBot').length, 1, 'a managed role is never created');
  assert.deepEqual(chanNames(guild), ['Info', 'Lounge', 'general', 'help (renamed)']);

  const mod = [...guild.roles.cache.values()].find((r) => r.name === 'Mod');
  const info = [...guild.channels.cache.values()].find((c) => c.name === 'Info');
  const general = [...guild.channels.cache.values()].find((c) => c.name === 'general');
  assert.equal(state.idMap['102'], mod.id);
  assert.equal(state.idMap['201'], info.id);
  assert.equal(general.parentId, info.id, 'the parent is mapped through the id map');
  assert.equal(general.topic, 'Say hi');
  assert.equal(general.rateLimitPerUser, 5);
  assert.equal(mod.permissions.bitfield, MOD_PERMS);
  assert.equal(mod.mentionable, true);

  const roleCreate = guild.log.create.find(([kind, d]) => kind === 'role' && d.name === 'Mod')[1];
  assert.deepEqual(roleCreate.colors, { primaryColor: 0 });
  assert.equal('color' in roleCreate, false, 'colors, not the deprecated color');
  assert.equal('position' in roleCreate, false, 'positions are set afterwards in one call');

  // Overwrites: only the new channels. @everyone and roles come WITH the create (the role target mapped), so a new channel is
  // never public in between; member overwrites follow one by one, and 778 is left out because it is not in the server.
  const createdWith = (ch) => guild.log.create.find(([kind, d]) => kind === 'channel' && d.name === ch.name)[1].permissionOverwrites ?? [];
  assert.deepEqual(createdWith(general).map((o) => o.id).sort(), [mod.id, '104'].sort(), 'the role target is mapped');
  assert.deepEqual(createdWith(general).find((o) => o.id === mod.id), { id: mod.id, type: OverwriteType.Role, allow: bigPerms(P.ViewChannel, P.ManageMessages), deny: 0n });
  assert.ok(createdWith(info).some((o) => o.id === '900' && o.deny === P.SendMessages), '@everyone maps to itself');
  const puts = guild.log.put.map((p) => [...p.route.match(/^\/channels\/(\d+)\/permissions\/(\d+)$/).slice(1), p.body]);
  const general_ = puts.filter(([channelId]) => channelId === general.id);
  assert.deepEqual(general_.map(([, target]) => target), ['777'], 'members one by one; 778 left the server');
  assert.equal(general_[0][2].type, OverwriteType.Member);
  assert.ok(state.counts.overwritesKept >= 3, 'what came with the create is already right');
  assert.ok(puts.every(([channelId]) => channelId !== '206'), 'the channel that still exists keeps its overwrites');
  assert.ok(!guild.log.put.some((p) => p.route.endsWith('/778')), 'a member who left gets no overwrite');
  assert.equal(state.counts.rolesCreated, 2);
  assert.equal(state.counts.categoriesCreated, 1);
  assert.equal(state.counts.channelsCreated, 2);
  assert.equal(state.counts.overwritesSkipped >= 1, true);
  assert.equal(state.errors.length, 0, JSON.stringify(state.errors));
  // The new roles are placed back where they were (below the bot's role at 10).
  assert.deepEqual(guild.log.rolePositions.at(-1).map((e) => e.position).sort((a, b) => a - b), [3, 6]);
});

test('the managed role that still exists is a valid overwrite target', async () => {
  const guild = makeGuild({ roles: [{ id: '104', name: 'SomeBot', managed: true, pos: 2 }, { id: '999', name: 'Bot', pos: 10 }] });
  await restoreAll(guild);
  const general = [...guild.channels.cache.values()].find((c) => c.name === 'general');
  assert.ok(general.permissionOverwrites.cache.has('104'), JSON.stringify([...general.permissionOverwrites.cache.keys()]));
});

test('mode full: resets what exists (names, permissions, order, channel settings and overwrites), still deletes nothing', async () => {
  const guild = makeGuild({
    roles: [
      { id: '101', name: 'Admins', color: 0, perms: bigPerms(P.ViewChannel), pos: 4 },
      { id: '102', name: 'Mod', mentionable: true, perms: MOD_PERMS, pos: 5 },
      { id: '103', name: 'Member', perms: MEMBER_PERMS, pos: 3 },
      { id: '104', name: 'SomeBot', managed: true, pos: 7 },
      { id: '150', name: 'Newcomer', pos: 6 },
      { id: '160', name: 'Boss', pos: 12 },
      { id: '999', name: 'Bot', pos: 10 },
    ],
    channels: [
      { id: '201', type: ChannelType.GuildCategory, name: 'Information', pos: 3 },
      { id: '202', name: 'chat', parent: null, pos: 5, topic: 'old', slow: 0, overwrites: [{ id: '102', allow: 0n, deny: 0n }, { id: '888', type: OverwriteType.Member, deny: bigPerms(P.ViewChannel) }] },
      { id: '203', type: ChannelType.GuildVoice, name: 'Lounge', parent: '201', pos: 1, bitrate: 64000, limit: 5 },
      { id: '206', type: ChannelType.GuildForum, name: 'help', pos: 9 },
    ],
    members: ['777', '778'], everyonePerms: bigPerms(P.ViewChannel, P.SendMessages),
  });
  const before = {
    roles: [...guild.roles.cache.keys()], channels: [...guild.channels.cache.keys()],
  };
  const { state } = await restoreAll(guild, { mode: 'full' });

  assert.equal(guild.roles.cache.get('101').name, 'Admin');
  assert.equal(guild.roles.cache.get('101').colors.primaryColor, 0xff0000);
  // Administrator is not given: the bot has no Administrator itself. ManageRoles it has.
  assert.equal(guild.roles.cache.get('101').permissions.bitfield, bigPerms(P.ManageRoles));
  assert.equal(guild.roles.everyone.permissions.bitfield, EVERYONE_PERMS);
  assert.equal(guild.channels.cache.get('201').name, 'Info');
  assert.equal(guild.channels.cache.get('202').name, 'general');
  assert.equal(guild.channels.cache.get('202').topic, 'Say hi');
  assert.equal(guild.channels.cache.get('202').parentId, '201');
  assert.equal(guild.channels.cache.get('202').rateLimitPerUser, 5);
  assert.equal(guild.roles.cache.get('160').name, 'Boss', 'a role above the bot is never touched');
  assert.equal(guild.roles.cache.get('150').name, 'Newcomer', 'a role that is not in the backup is kept');
  assert.equal(guild.roles.cache.get('104').rawPosition, 7, 'a managed role stays where it is');
  for (const id of before.roles) assert.ok(guild.roles.cache.has(id), `role ${id} still exists`);
  for (const id of before.channels) assert.ok(guild.channels.cache.has(id), `channel ${id} still exists`);

  // Role order: Admin, Mod, Member keep the slots they held (3, 4, 5 sorted) in the backup's order Member < Mod < Admin.
  const order = ['103', '102', '101'].map((id) => guild.roles.cache.get(id).rawPosition);
  assert.ok(order[0] < order[1] && order[1] < order[2], `backup order kept: ${order}`);
  assert.deepEqual(order.slice().sort((a, b) => a - b), [4, 5, 3].sort((a, b) => a - b), 'only the places those roles held are used');
  assert.ok(Math.max(...order) < BOT_TOP, 'nothing moves above the bot');

  // The existing overwrite for the member 777 is added; an extra overwrite the backup does not know (888) is left alone.
  assert.ok(guild.log.put.some((p) => p.route === '/channels/202/permissions/777'));
  assert.ok(guild.channels.cache.get('202').permissionOverwrites.cache.has('888'), 'overwrites added after the backup stay');
  assert.ok(guild.log.channelPositions.length >= 1);
  assert.equal(state.counts.rolesUpdated >= 2, true);
  assert.equal(state.counts.masked >= 1, true, 'Administrator could not be given');
  assert.match(snap.bits(state.dropped).toString(), /^\d+$/);
  assert.notEqual(BigInt(state.dropped) & P.Administrator, 0n);
});

test('the restore never deletes: no delete call exists on the restore path and overwrites are upserted one by one', () => {
  const DESTRUCTIVE = /(?<![A-Za-z])(?:roles|channels|members|bans|permissionOverwrites|guild|role|channel|member|thread|threads)\??\.(?:delete|destroy|remove|ban|kick|prune|bulkDelete)\(/;
  const DELETE_ROUTE = /rest\??\.delete\(|method:\s*'DELETE'/;
  const REPLACES_LIST = /permissionOverwrites\.set\(|roles\.set\(|\.setPermissions\(|\.members\.ban/;
  const BARE_ROW_DELETE = /prisma\.serverBackup\.delete\(/;
  // The patterns must be able to fail: each one matches a sample of the thing it forbids.
  assert.match('await guild.roles.delete(role)', DESTRUCTIVE);
  assert.match('channel.permissionOverwrites.delete(id)', DESTRUCTIVE);
  assert.match('await env.guild.channels?.delete(c)', DESTRUCTIVE);
  assert.match('await guild.members.ban(id)', DESTRUCTIVE);
  assert.match('client.rest.delete(Routes.channel(id))', DELETE_ROUTE);
  assert.match('channel.permissionOverwrites.set(list)', REPLACES_LIST);
  assert.match('guild.roles.set(list)', REPLACES_LIST);
  assert.match('await prisma.serverBackup.delete({ where })', BARE_ROW_DELETE);
  // ...and never matches what the code does legitimately.
  assert.doesNotMatch('batching.delete(guildId); prisma.serverBackup.deleteMany({})', DESTRUCTIVE);
  assert.doesNotMatch('guild.client.rest.put(route, { body })', DELETE_ROUTE);

  for (const file of ['../src/utils/guildSnapshot.js', '../src/services/backupService.js']) {
    const text = readFileSync(new URL(file, import.meta.url), 'utf8').replace(/\/\/.*$/gm, '').replace(/\/\*[\s\S]*?\*\//g, '');
    assert.doesNotMatch(text, DESTRUCTIVE, `${file} has a destructive Discord call`);
    assert.doesNotMatch(text, DELETE_ROUTE, `${file} calls a DELETE route`);
    assert.doesNotMatch(text, REPLACES_LIST, `${file} replaces a whole list`);
    assert.doesNotMatch(text, BARE_ROW_DELETE, `${file}: only deleteMany scoped by guild, never a bare delete`);
  }
  const engine = readFileSync(new URL('../src/utils/guildSnapshot.js', import.meta.url), 'utf8');
  assert.match(engine, /rest\.put\(Routes\.channelPermission\(/, 'overwrites are upserted one target at a time');
});

test('permission masking: Administrator is dropped for a bot that lacks it and the summary names it', async () => {
  const guild = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const { state } = await restoreAll(guild);
  const admin = [...guild.roles.cache.values()].find((r) => r.name === 'Admin');
  assert.equal(admin.permissions.bitfield & P.Administrator, 0n);
  assert.equal(admin.permissions.bitfield & P.ManageRoles, P.ManageRoles, 'what the bot has is kept');
  assert.ok(state.counts.masked >= 1);
  const embed = svc.buildSummaryEmbed(state).toJSON();
  const field = embed.fields.find((f) => f.name === 'Permissions I could not give');
  assert.ok(field, 'reported');
  assert.match(field.value, /Administrator/);
  assert.doesNotMatch(field.value, /Kick Members/, 'only the dropped bits, not every permission');
});

test('roles above the bot\'s own role are placed as high as the bot may, and the summary says so', async () => {
  const guild = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 5 }], botTop: 5 });
  const { state } = await restoreAll(guild);
  const placed = ['Admin', 'Mod', 'Member'].map((n) => [...guild.roles.cache.values()].find((r) => r.name === n).rawPosition);
  assert.ok(placed.every((p) => p < 5), `nothing at or above the bot: ${placed}`);
  assert.ok(state.counts.positionsLimited >= 2, 'Admin (8) and Mod (6) were above the bot');
  const text = JSON.stringify(svc.buildSummaryEmbed(state).toJSON());
  assert.match(text, /placed lower than before/);
  assert.match(text, /cannot be placed above my own highest role/);
});

test('a bot with Administrator gives everything', async () => {
  const guild = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 10 }], botPerms: [P.Administrator] });
  const { state } = await restoreAll(guild);
  const admin = [...guild.roles.cache.values()].find((r) => r.name === 'Admin');
  assert.equal(admin.permissions.bitfield, ADMIN_PERMS);
  assert.equal(state.counts.masked, 0);
});

test('one batch makes about ten Discord calls and says there is more; a full restore needs several batches', async () => {
  const guild = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 10 }], members: ['777', '778'] });
  const { batches, calls, state } = await restoreAll(guild);
  assert.ok(batches >= 2, `batches: ${batches}`);
  for (const n of calls) assert.ok(n <= snap.RESTORE_BUDGET.CALLS + 1, `a batch made ${n} calls`);
  assert.equal(state.phase, 'finish');
  assert.equal(state.counts.errors, 0, JSON.stringify(state.errors));
});

test('a crash in the middle of a batch (progress lost) adopts what was made instead of making it twice', async () => {
  const guild = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 10 }], members: ['777'] });
  const startedAt = Date.now() - 1000;
  let state = snap.newRestoreState({ backupId: 'bk000001', mode: 'missing', startedById: 'OWNER', channelId: 'CH', now: startedAt });
  const saved = roundTrip(state); // the payload the scheduler still has

  // The first run makes Mod and Member and the first channels, then "crashes": its state is never saved.
  let env = snap.makeEnv({ guild, snapshot: snap.parseSnapshot(roundTrip(SNAPSHOT)), state, reason: 'test' });
  assert.equal(await snap.runRestoreBatch(env), 'more');
  const made = guild.log.create.length;
  assert.ok(made >= 5, `the first batch made ${made} things`);
  const rolesNow = nameOf(guild);

  // The retry starts from the old payload.
  const retry = await restoreAll(guild, { startedAt });
  void saved;
  assert.deepEqual(nameOf(guild).filter((n) => n === 'Mod' || n === 'Member'), ['Member', 'Mod'], 'no second Mod or Member');
  assert.equal(chanNames(guild).filter((n) => n === 'Info').length, 1);
  assert.equal(chanNames(guild).filter((n) => n === 'general').length, 1);
  assert.equal(chanNames(guild).filter((n) => n === 'Lounge').length, 1);
  assert.ok(rolesNow.includes('Mod'));
  assert.equal(retry.state.counts.rolesCreated, 3, 'adopted roles count as made by this restore (Admin, Mod, Member)');
  assert.equal(retry.state.errors.length, 0);
  state = retry.state;
  assert.ok(Object.keys(state.idMap).length >= 5);
});

test('running the whole restore again does not create anything a second time (idempotent)', async () => {
  const guild = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 10 }], members: ['777'] });
  const first = await restoreAll(guild, { startedAt: Date.now() - 1000 });
  const creates = guild.log.create.length;
  const puts = guild.log.put.length;
  // A second restore of the same backup, with the id map the first one stored, makes nothing and sets nothing new.
  const second = await restoreAll(guild, { idMap: first.state.idMap, startedAt: Date.now() });
  assert.equal(guild.log.create.length, creates, 'nothing is created again');
  assert.equal(guild.log.put.length, puts, 'no overwrite is written again');
  assert.equal(second.state.counts.rolesCreated, 0);
  assert.equal(second.state.counts.channelsCreated, 0);
  // Mode full on the restored server changes nothing that is already right.
  const edits = guild.log.edit.length;
  const third = await restoreAll(guild, { mode: 'full', idMap: first.state.idMap });
  assert.equal(third.state.counts.rolesCreated + third.state.counts.channelsCreated + third.state.counts.categoriesCreated, 0);
  assert.ok(guild.log.edit.length >= edits);
});

test('a call that times out is retried in the next batch and adopts what appeared late; after a few stalls it is reported and skipped', async () => {
  const guild = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 10 }], members: ['777'] });
  const real = guild.roles.create.bind(guild.roles);
  const timeout = () => Object.assign(new Error('role timed out'), { name: 'TimeoutError' });
  let n = 0;
  // The first call "times out" but Discord still makes the role a moment later.
  guild.roles.create = async (data) => {
    n += 1;
    if (n === 1) {
      await real(data);
      throw timeout();
    }
    return real(data);
  };
  const { state } = await restoreAll(guild);
  assert.equal(nameOf(guild).filter((r) => r === 'Member').length, 1, 'the role that appeared late is adopted, not made twice');
  assert.equal(state.counts.errors, 0, 'a retried timeout is not an error');
  assert.equal(state.stalls, 0);

  // A call that keeps timing out is given up on after STALLS tries, with one line in the summary, and the restore goes on.
  const stuck = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  stuck.roles.create = async () => { throw timeout(); };
  const out = await restoreAll(stuck, { maxBatches: 200 });
  assert.equal(out.state.phase, 'finish');
  assert.equal(out.state.errors.filter((e) => /did not answer in time/.test(e)).length, 3, 'Admin, Mod and Member');
  assert.ok(chanNames(stuck).includes('general'), 'channels were still restored');
});

test('Discord errors never escape: they are counted, listed (capped) and the restore carries on', async () => {
  const guild = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 10 }], fail: { roleCreate: true, channelCreate: true } });
  const { state } = await restoreAll(guild);
  assert.equal(state.phase, 'finish');
  assert.ok(state.counts.errors >= 5);
  assert.ok(state.errors.length <= snap.RESTORE_BUDGET.ERRORS_KEPT);
  assert.match(state.errors[0], /Maximum number of server roles reached \(30005\)/);
  assert.equal(state.counts.rolesCreated, 0);
  assert.equal(guild.log.put.length, 0, 'no channel was made, so no overwrite is written');
});

test('a missing permission skips the phases it needs, once, with one line each', async () => {
  const guild = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 10 }], botPerms: [P.ViewChannel, P.SendMessages] });
  const { state } = await restoreAll(guild);
  assert.equal(guild.log.create.length, 0);
  assert.equal(state.counts.rolesSkipped, 4);
  assert.equal(state.counts.channelsSkipped, 3);
  assert.equal(state.counts.categoriesSkipped, 1);
  assert.equal(state.errors.filter((e) => /Manage Roles/.test(e)).length, 2, 'roles and channel permissions');
  assert.equal(state.errors.filter((e) => /Manage Channels/.test(e)).length, 2, 'categories and channels');
});

test('a thread-like or unreadable channel in a stored snapshot is ignored, not restored', async () => {
  const stored = roundTrip(SNAPSHOT);
  stored.channels.push({ id: '300', type: ChannelType.PublicThread, name: 'thread' }, { id: '301', type: ChannelType.GuildDirectory, name: 'dir' });
  assert.equal(snap.parseSnapshot(stored).channels.length, 3);
});

test('previewRestore uses the job decisions: counts per mode, masked permissions, warnings', () => {
  const guild = makeGuild({
    roles: [{ id: '101', name: 'Admins', perms: bigPerms(P.ViewChannel), pos: 4 }, { id: '999', name: 'Bot', pos: 10 }],
    channels: [{ id: '206', type: ChannelType.GuildForum, name: 'help', pos: 2 }],
  });
  const missing = snap.previewRestore({ guild, snapshot: SNAPSHOT, mode: 'missing' });
    assert.equal(missing.roles.create, 2);
  assert.equal(missing.roles.skip, 1, 'the managed role cannot be made');
  assert.equal(missing.managedMissing, 1);
  assert.equal(missing.categories.create, 1);
  assert.equal(missing.channels.create, 2);
  assert.equal(missing.channels.keep, 1);
  assert.equal(missing.overwrites, 4, 'the category, general: Mod, 777 and 778; the managed role is gone');
  assert.equal(missing.unsupported, 1);
  assert.equal(missing.masked, 0, 'Mod and Member need nothing the bot lacks');
  const full = snap.previewRestore({ guild, snapshot: SNAPSHOT, mode: 'full' });
  assert.ok(full.masked >= 1 && (full.dropped & P.Administrator) === P.Administrator, 'Admin gets Administrator back only if the bot has it');
    assert.ok(full.roles.update >= 1);
  assert.ok(full.calls > missing.calls - 5);
  const embed = svc.buildPreviewEmbed({ name: 'Nightly', createdAt: new Date('2026-10-21T10:00:00Z'), automatic: false }, missing).toJSON();
  assert.match(embed.description, /Missing only/);
  assert.match(embed.description, /Nothing is ever deleted/);
  assert.ok(embed.fields.some((f) => f.name === 'Heads up' && /integrations/.test(f.value)));
  assert.match(svc.buildPreviewEmbed({ name: 'x', createdAt: new Date(), automatic: false }, full).toJSON().description, /Full/);
});

// ── Service: backups, auto job ────────────────────────────────────────────────

async function enable(guildId, extra = {}) {
  await updateSettings(guildId, { backupsEnabled: true, backupIntervalHours: 0, backupKeep: 7, ...extra });
}

test('createBackup: manual backups are capped, automatic ones are not; an oversize snapshot is refused', async () => {
  const guild = makeGuild({ id: '910' });
  const made = [];
  for (let i = 0; i < LIMITS.BACKUPS_MANUAL; i += 1) {
    const r = await svc.createBackup({ guild, createdById: 'U1', name: `  manual\n${i} ` });
    assert.equal(r.ok, true);
    made.push(r.backup);
  }
  assert.equal(made[0].name, 'manual 0', 'one line, trimmed');
  assert.equal('data' in made[0], false, 'the snapshot is not returned');
  const over = await svc.createBackup({ guild, createdById: 'U1' });
  assert.equal(over.ok, false);
  assert.equal(over.reason, 'cap');
  const auto = await svc.createBackup({ guild, automatic: true, createdById: 'U1' });
  assert.equal(auto.ok, true, 'automatic backups do not count against the manual cap');
  assert.equal(auto.backup.createdById, null);
  assert.equal(auto.backup.automatic, true);

  // Concurrent manual creates cannot jump the cap.
  const guild2 = makeGuild({ id: '911' });
  const results = await Promise.all(Array.from({ length: LIMITS.BACKUPS_MANUAL + 4 }, () => svc.createBackup({ guild: guild2, createdById: 'U1' })));
  assert.equal(results.filter((r) => r.ok).length, LIMITS.BACKUPS_MANUAL);

  const huge = makeGuild({ id: '912', channels: [{ id: '1', name: 'x'.repeat(100) }], roles: [] });
  const realCapture = huge.roles.cache;
  for (let i = 0; i < 25000; i += 1) realCapture.set(`r${i}`, { id: String(100000 + i), name: 'y'.repeat(100), colors: {}, permissions: { bitfield: 0n }, rawPosition: i, managed: false });
  const refused = await svc.createBackup({ guild: huge, createdById: 'U1' });
  assert.equal(refused.ok, false);
  assert.equal(refused.reason, 'size');
});

test('createBackup is refused while a restore is running', async () => {
  const guild = makeGuild({ id: '913' });
  jobRows.set('backup.restore:913', { id: 'jobx', status: 'RUNNING', dedupeKey: 'backup.restore:913', payload: { backupId: 'bk1' }, runAt: new Date(), version: 0 });
  const r = await svc.createBackup({ guild, createdById: 'U1' });
  assert.equal(r.ok, false);
  assert.equal(r.reason, 'restoring');
  jobRows.delete('backup.restore:913');
});

test('pruneAutomatic keeps the newest N automatic backups and never touches manual ones or the spared one', async () => {
  const guild = makeGuild({ id: '914' });
  const manual = [];
  for (let i = 0; i < 3; i += 1) manual.push((await svc.createBackup({ guild, createdById: 'U1', name: `m${i}` })).backup.id);
  const autos = [];
  for (let i = 0; i < 6; i += 1) autos.push((await svc.createBackup({ guild, automatic: true })).backup.id);
  assert.equal(await svc.pruneAutomatic('914', 3), 3);
  const left = [...backupRows.values()].filter((r) => r.guildId === '914');
  assert.deepEqual(left.filter((r) => r.automatic).map((r) => r.id).sort(), autos.slice(3).sort(), 'the newest three stay');
  assert.deepEqual(left.filter((r) => !r.automatic).map((r) => r.id).sort(), manual.sort(), 'manual backups are never pruned');
  // keep is clamped to 1..BACKUPS_AUTO_KEEP and a spared backup survives.
  assert.equal(await svc.pruneAutomatic('914', 0, { spare: [autos[3]] }), 1, 'keep 0 acts as keep 1; the spared one stays');
  assert.ok(backupRows.has(autos[3]) && backupRows.has(autos[5]));
  assert.equal(await svc.pruneAutomatic('914', 9999), 0);
});

test('backup.auto: idle while the module is off or the interval is 0, otherwise backs up when due and prunes', async () => {
  const guild = makeGuild({ id: '915' });
  const ctx = { guild };
  const job = { guildId: '915' };
  const hour = 3_600_000;

  // Module off.
  await updateSettings('915', { backupsEnabled: false, backupIntervalHours: 6, backupKeep: 2 });
  let out = await svc.handleAutoJob(job, ctx);
  assert.ok(out.nextRunAt.getTime() - Date.now() <= hour + 1000 && out.nextRunAt.getTime() - Date.now() > 50 * 60_000, 'still watches, within an hour');
  assert.equal([...backupRows.values()].filter((r) => r.guildId === '915').length, 0);

  // Module on, interval 0.
  await enable('915', { backupIntervalHours: 0 });
  out = await svc.handleAutoJob(job, ctx);
  assert.equal([...backupRows.values()].filter((r) => r.guildId === '915').length, 0);
  assert.ok(out.nextRunAt.getTime() - Date.now() <= hour + 1000);

  // On, 6 h, none yet: one is made at once; the next run is within the hour.
  await enable('915', { backupIntervalHours: 6, backupKeep: 2 });
  out = await svc.handleAutoJob(job, ctx);
  assert.equal([...backupRows.values()].filter((r) => r.guildId === '915' && r.automatic).length, 1);
  assert.ok(out.nextRunAt.getTime() - Date.now() <= hour + 1000);
  // Not due again yet.
  await svc.handleAutoJob(job, ctx);
  assert.equal([...backupRows.values()].filter((r) => r.guildId === '915' && r.automatic).length, 1);

  // Old ones: make the newest 7 hours old, add two more older ones, run: one new, two kept, nothing manual lost.
  const manualId = (await svc.createBackup({ guild, createdById: 'U1', name: 'mine' })).backup.id;
  const autos = [...backupRows.values()].filter((r) => r.guildId === '915' && r.automatic);
  autos[0].createdAt = new Date(Date.now() - 7 * hour);
  for (let i = 0; i < 2; i += 1) {
    backupRows.set(`old${i}`, { ...autos[0], id: `old${i}`, createdAt: new Date(Date.now() - (30 + i) * hour) });
  }
  await svc.handleAutoJob(job, ctx);
  const after = [...backupRows.values()].filter((r) => r.guildId === '915');
  assert.equal(after.filter((r) => r.automatic).length, 2, 'backupKeep 2');
  assert.ok(after.some((r) => r.id === manualId), 'the manual backup is untouched');

  // A running restore pauses automatic backups (the server is half-restored).
  autos.forEach((r) => { r.createdAt = new Date(Date.now() - 40 * hour); });
  jobRows.set('backup.restore:915', { id: 'jobr', status: 'PENDING', dedupeKey: 'backup.restore:915', payload: { backupId: 'x' }, runAt: new Date(), version: 0 });
  const before = backupRows.size;
  out = await svc.handleAutoJob(job, ctx);
  assert.equal(backupRows.size, before);
  assert.ok(out.nextRunAt instanceof Date);
  jobRows.delete('backup.restore:915');
});

test('backup.auto never throws: a database failure becomes a retry', async () => {
  const guild = makeGuild({ id: '916' });
  await enable('916', { backupIntervalHours: 1 });
  const find = prisma.serverBackup.findFirst;
  prisma.serverBackup.findFirst = async () => { throw new Error('db down'); };
  try {
    const out = await svc.handleAutoJob({ guildId: '916' }, { guild });
    assert.ok(out.nextRunAt instanceof Date);
  } finally {
    prisma.serverBackup.findFirst = find;
  }
});

test('onGuildReady seeds backup.auto only for guilds that use the module', async () => {
  await svc.onGuildReady({ guildId: '920', getSettings: async () => ({ backupsEnabled: false, backupIntervalHours: 0 }) });
  assert.equal(jobRows.has('backup.auto:920'), false, 'untouched guilds cost nothing');
  await svc.onGuildReady({ guildId: '921', getSettings: async () => ({ backupsEnabled: true, backupIntervalHours: 0 }) });
  assert.equal(jobRows.has('backup.auto:921'), true);
  await svc.onGuildReady({ guildId: '922', getSettings: async () => ({ backupsEnabled: false, backupIntervalHours: 12 }) });
  assert.equal(jobRows.has('backup.auto:922'), true, 'an interval set on the dashboard counts');
  await svc.onGuildReady({ guildId: '923', getSettings: async () => null });
  assert.equal(jobRows.has('backup.auto:923'), false);
  // A failed job is revived; a healthy one is only pulled earlier.
  jobRows.get('backup.auto:921').status = 'FAILED';
  assert.equal(await svc.ensureAutoJob('921'), true);
  assert.equal(jobRows.get('backup.auto:921').status, 'PENDING');
  const runAt = jobRows.get('backup.auto:921').runAt.getTime();
  await svc.ensureAutoJob('921', new Date(runAt + 5 * 3_600_000));
  assert.equal(jobRows.get('backup.auto:921').runAt.getTime(), runAt, 'a healthy job is never pushed later');
});

// ── Restore job ───────────────────────────────────────────────────────────────

function summaryChannel(id = 'CH') {
  const ch = fakeChannel({ id, name: 'admin' });
  return ch;
}

async function seedBackup(guildId, snapshot = SNAPSHOT, extra = {}) {
  const row = await prisma.serverBackup.create({
    data: { guildId, name: 'seed', automatic: false, createdById: 'OWNER', data: roundTrip(snapshot), sizeBytes: 1000, roleCount: 4, channelCount: 4, ...extra },
  });
  return row;
}

test('startRestore: one restore at a time per guild, the payload carries the resume state', async () => {
  const guild = makeGuild({ id: '930', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const row = await seedBackup('930');
  const first = await svc.startRestore({ guild, backupId: row.id, snapshot: snap.parseSnapshot(row.data), mode: 'missing', startedById: 'OWNER', channelId: 'CH' });
  assert.deepEqual(first, { ok: true });
  const job = jobRows.get('backup.restore:930');
  assert.equal(job.type, 'backup.restore');
  assert.equal(job.status, 'PENDING');
  assert.equal(job.maxAttempts, 5);
  for (const k of ['backupId', 'mode', 'phase', 'cursor', 'idMap', 'counts', 'errors', 'startedById', 'channelId', 'startedAt']) assert.ok(k in job.payload, k);
  assert.equal(job.payload.phase, 'roles');
  assert.equal(job.payload.cursor, 0);
  const version = job.version;
  const second = await svc.startRestore({ guild, backupId: row.id, snapshot: snap.parseSnapshot(row.data), mode: 'full', startedById: 'OWNER', channelId: 'CH' });
  assert.deepEqual(second, { ok: false, reason: 'running' });
  assert.equal(jobRows.get('backup.restore:930').version, version, 'the running job is untouched (not replaced)');
  assert.equal(jobRows.get('backup.restore:930').payload.mode, 'missing');
  assert.deepEqual(await svc.startRestore({ guild, backupId: row.id, snapshot: SNAPSHOT, mode: 'bogus' }), { ok: false, reason: 'mode' });
  // A finished (DONE/FAILED) row does not block a new restore.
  jobRows.get('backup.restore:930').status = 'FAILED';
  assert.equal((await svc.startRestore({ guild, backupId: row.id, snapshot: snap.parseSnapshot(row.data), mode: 'full', startedById: 'OWNER', channelId: 'CH' })).ok, true);
  assert.equal(jobRows.get('backup.restore:930').payload.mode, 'full');
});

test('the restore job runs batch by batch from the saved payload, then posts the summary and remembers the new ids', async () => {
  const guild = makeGuild({ id: '931', roles: [{ id: '999', name: 'Bot', pos: 10 }], members: ['777'] });
  const channel = summaryChannel('CH931');
  channel.guild = guild;
  guild.channels.cache.set(channel.id, channel);
  const row = await seedBackup('931');
  await svc.startRestore({ guild, backupId: row.id, snapshot: snap.parseSnapshot(row.data), mode: 'missing', startedById: 'OWNER', channelId: channel.id });
  let payload = jobRows.get('backup.restore:931').payload;

  let steps = 0;
  for (;;) {
    const out = await svc.handleRestoreJob({ guildId: '931', payload, attempts: 1, maxAttempts: 5 }, { guild, client: guild.client, lateByMs: 100, attempt: 1 });
    steps += 1;
    if (out === undefined) break;
    assert.ok(out.nextRunAt instanceof Date && out.nextRunAt.getTime() > Date.now(), 'the next batch is a couple of seconds away');
    assert.ok(out.nextRunAt.getTime() - Date.now() <= 3000);
    payload = roundTrip(out.payload);
    assert.ok(steps < 40);
  }
  assert.ok(steps >= 2, 'bounded batches');
  assert.equal(channel.sent.length, 1, 'one summary');
  const sent = channel.sent[0];
  assert.deepEqual(sent.allowedMentions, { users: ['OWNER'] });
  assert.equal(sent.content, '<@OWNER>');
  const embed = sent.embeds[0].toJSON();
  assert.match(embed.title, /Restore Finished/);
  assert.match(embed.description, /Nothing was deleted/);
  assert.ok(embed.fields.some((f) => f.name === 'Roles' && /created 3/.test(f.value)));
  assert.equal(nameOf(guild).filter((n) => n === 'Mod').length, 1);
  // The new ids are stored in the backup (and kept out of exports).
  const stored = backupRows.get(row.id).data.idMap;
  assert.ok(stored['102'] && guild.roles.cache.has(stored['102']));
  assert.equal('idMap' in snap.exportable(backupRows.get(row.id).data), false);

  // A second restore of the same backup seeds from that map and makes nothing again.
  const createsBefore = guild.log.create.length;
  jobRows.get('backup.restore:931').status = 'DONE'; // the first one finished (the scheduler deletes the row; the stub driver does not)
  await svc.startRestore({ guild, backupId: row.id, snapshot: snap.parseSnapshot(backupRows.get(row.id).data), mode: 'missing', startedById: 'OWNER', channelId: channel.id });
  payload = jobRows.get('backup.restore:931').payload;
  assert.ok(Object.keys(payload.idMap).length >= 5, 'seeded from the first restore');
  for (let i = 0; i < 40; i += 1) {
    const out = await svc.handleRestoreJob({ guildId: '931', payload, attempts: 1, maxAttempts: 5 }, { guild, client: guild.client, lateByMs: 0, attempt: 1 });
    if (out === undefined) break;
    payload = roundTrip(out.payload);
  }
  assert.equal(guild.log.create.length, createsBefore, 'no second copy');
});

test('the restore job: a batch already in flight in this process makes a retry wait, with the payload left alone', async () => {
  const guild = makeGuild({ id: '932', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const row = await seedBackup('932');
  const payload = snap.newRestoreState({ backupId: row.id, mode: 'missing', startedById: 'OWNER', channelId: 'CH', now: Date.now() });
  let release;
  const real = guild.roles.create.bind(guild.roles);
  let held = false;
  guild.roles.create = (data) => {
    if (held) return real(data);
    held = true;
    return new Promise((resolve) => { release = () => resolve(real(data)); });
  };
  const ctx = { guild, client: guild.client, lateByMs: 0, attempt: 1 };
  const running = svc.handleRestoreJob({ guildId: '932', payload, attempts: 1, maxAttempts: 5 }, ctx);
  await new Promise((r) => setTimeout(r, 20));
  const retry = await svc.handleRestoreJob({ guildId: '932', payload, attempts: 2, maxAttempts: 5 }, ctx);
  assert.ok(retry.nextRunAt instanceof Date);
  assert.equal('payload' in retry, false, 'the saved payload stays as it was');
  release();
  const out = await running;
  assert.ok(out === undefined || out.nextRunAt instanceof Date);
});

test('the restore job stops cleanly: backup deleted, unreadable data, a job that waited too long; a bad payload is permanent', async () => {
  const guild = makeGuild({ id: '933', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const channel = summaryChannel('CH933');
  channel.guild = guild;
  guild.channels.cache.set(channel.id, channel);
  const base = snap.newRestoreState({ backupId: 'gone', mode: 'missing', startedById: 'OWNER', channelId: channel.id });
  const ctx = (lateByMs = 0) => ({ guild, client: guild.client, lateByMs, attempt: 1 });

  assert.equal(await svc.handleRestoreJob({ guildId: '933', payload: base }, ctx()), undefined);
  assert.match(channel.sent.at(-1).embeds[0].toJSON().description, /backup was deleted/);
  assert.match(channel.sent.at(-1).embeds[0].toJSON().title, /Stopped/);
  assert.equal(guild.log.create.length, 0);

  const bad = await prisma.serverBackup.create({ data: { guildId: '933', name: 'bad', automatic: false, data: { version: 9 }, sizeBytes: 1, roleCount: 0, channelCount: 0 } });
  assert.equal(await svc.handleRestoreJob({ guildId: '933', payload: { ...base, backupId: bad.id } }, ctx()), undefined);
  assert.match(channel.sent.at(-1).embeds[0].toJSON().description, /version 9/);

  const good = await seedBackup('933');
  // A stale job stops, but as FAILED (kept with its payload), not as finished: the next restore continues from it.
  await assert.rejects(svc.handleRestoreJob({ guildId: '933', payload: { ...base, backupId: good.id } }, ctx(3 * 3_600_000)), PermanentJobError);
  assert.match(channel.sent.at(-1).embeds[0].toJSON().description, /waited too long/);
  assert.equal(guild.log.create.length, 0, 'a stale job does nothing');

  await assert.rejects(svc.handleRestoreJob({ guildId: '933', payload: { nope: true } }, ctx()), PermanentJobError);

  // No usable channel: the starter gets a DM instead.
  const lonely = makeGuild({ id: '934', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  await svc.handleRestoreJob({ guildId: '934', payload: { ...base, channelId: 'NOPE' } }, { guild: lonely, client: lonely.client, lateByMs: 0, attempt: 1 });
  assert.equal(lonely.client.users.sent.length, 1);
  assert.equal(lonely.client.users.sent[0][0], 'OWNER');
});

/** Runs restore batches of ONE Discord call each until the state reaches `phase` (the overwrites phase has not run). */
async function stepUntil(guild, snapshot, state, phase) {
  let s = state;
  for (let i = 0; i < 80 && snap.RESTORE_PHASES.indexOf(s.phase) < snap.RESTORE_PHASES.indexOf(phase); i += 1) {
    const env = snap.makeEnv({ guild, snapshot: snap.parseSnapshot(roundTrip(snapshot)), state: s, reason: 'test' });
    env.budget.max = 1;
    await snap.runRestoreBatch(env);
    s = snap.normaliseRestoreState(roundTrip(s));
  }
  return s;
}

test('REVIEW: a recreated private channel is created private (@everyone deny and role allows come with the create), even before the overwrites phase', async () => {
  const STAFF_ALLOW = bigPerms(P.ViewChannel, P.SendMessages, P.ManageRoles);
  const roles = [{ id: '401', name: 'Staff', perms: MEMBER_PERMS, pos: 4 }, { id: '999', name: 'Bot', pos: 10 }];
  const snapshot = snap.captureGuild(makeGuild({
    id: '960', roles,
    channels: [{ id: '402', name: 'staff-only', pos: 0, overwrites: [
      { id: '960', deny: bigPerms(P.ViewChannel) },
      { id: '401', allow: STAFF_ALLOW },
      { id: '777', type: OverwriteType.Member, allow: bigPerms(P.ViewChannel) },
    ] }],
  }));
  for (const botPerms of [BOT_PERMS, [P.Administrator]]) {
    const admin = botPerms.includes(P.Administrator);
    // A raid deleted the channel; Staff is still there.
    const guild = makeGuild({ id: '960', roles, members: ['777'], botPerms });
    let state = await stepUntil(guild, snapshot, snap.newRestoreState({ backupId: 'bk', mode: 'missing', now: Date.now() - 1000 }), 'channelPositions');
    const made = [...guild.channels.cache.values()].find((c) => c.name === 'staff-only');
    assert.ok(made, 'the channel was made');
    assert.equal(guild.log.put.length, 0, 'the overwrites phase has not run yet');
    const ows = made.permissionOverwrites.cache;
    assert.equal(ows.get('960')?.deny.bitfield, bigPerms(P.ViewChannel), 'private from the moment it exists, not public until a later batch');
    assert.ok(ows.get('401'), 'Staff can see it from the start (and a bot without Administrator is not locked out by the @everyone deny)');
    assert.equal(ows.get('401').allow.bitfield & P.ManageRoles, admin ? P.ManageRoles : 0n, 'only an Administrator bot may put Manage Permissions in a new channel');
    assert.equal(ows.has('777'), false, 'members follow in the overwrites phase');

    // The rest of the restore adds the member and, for a bot without Administrator, the Manage Permissions bit.
    for (let i = 0; i < 40 && state.phase !== 'finish'; i += 1) {
      await snap.runRestoreBatch(snap.makeEnv({ guild, snapshot: snap.parseSnapshot(roundTrip(snapshot)), state, reason: 'test' }));
      state = snap.normaliseRestoreState(roundTrip(state));
    }
    const putTo = (target) => guild.log.put.find((p) => p.route === `/channels/${made.id}/permissions/${target}`);
    assert.ok(putTo('777'));
    if (admin) assert.equal(putTo('401'), undefined, 'already right');
    else assert.equal(BigInt(putTo('401').body.allow) & P.ManageRoles, P.ManageRoles);
    assert.equal(state.errors.length, 0, JSON.stringify(state.errors));
  }
});

test('REVIEW: a restore that did not finish (FAILED, or stopped as stale) is continued by the next restore of that backup: nothing is made twice', async () => {
  // A restore that FAILED (database errors) after the channels were made: its job row keeps the last saved payload.
  const guild = makeGuild({ id: '961', roles: [{ id: '999', name: 'Bot', pos: 10 }], members: ['777'] });
  const channel = summaryChannel('CH961');
  channel.guild = guild;
  guild.channels.cache.set(channel.id, channel);
  const row = await seedBackup('961');
  const snapshot = snap.parseSnapshot(row.data);
  await svc.startRestore({ guild, backupId: row.id, snapshot, mode: 'missing', startedById: 'OWNER', channelId: channel.id });
  const job = jobRows.get('backup.restore:961');
  const startedAt = job.payload.startedAt;
  const stopped = await stepUntil(guild, row.data, snap.normaliseRestoreState(roundTrip(job.payload)), 'channelPositions');
  Object.assign(job, { status: 'FAILED', payload: { ...roundTrip(stopped), startedAt: startedAt - 3_600_000 } }); // it began an hour ago
  // Made by someone else since (a booster's role, an admin's channel) with names of the backup: never adopted.
  const info = [...guild.channels.cache.values()].find((c) => c.name === 'Info');
  const strangerRole = fakeRole({ id: newId(), name: 'Admin', pos: 1, created: Date.now() });
  guild.roles.cache.set(strangerRole.id, strangerRole);
  const strangerChannel = fakeChannel({ id: newId(), name: 'general', parent: info.id, created: Date.now() });
  guild.channels.cache.set(strangerChannel.id, strangerChannel);
  const createsBefore = guild.log.create.length;
  assert.ok(createsBefore >= 7, `the first run made ${createsBefore} things`);
  assert.equal(guild.log.put.length, 0);

  // The preview knows what the stopped run made; another backup's or a finished job's state is never carried.
  assert.equal(svc.restorePreview(guild, snapshot, 'missing').roles.create, 3, 'without the job: everything looks missing');
  assert.equal(svc.restorePreview(guild, snapshot, 'missing', { prevJob: job, backupId: row.id }).roles.create, 0);
  assert.deepEqual(svc.carriedFrom(guild, { ...job, payload: { ...job.payload, backupId: 'other' } }, row.id), { idMap: {}, made: [], startedAt: null });
  assert.deepEqual(svc.carriedFrom(guild, { ...job, status: 'DONE' }, row.id).made, []);

  assert.deepEqual(await svc.startRestore({ guild, backupId: row.id, snapshot, mode: 'missing', startedById: 'OWNER', channelId: channel.id }), { ok: true });
  let payload = jobRows.get('backup.restore:961').payload;
  assert.ok(payload.startedAt >= startedAt, 'the adoption window is not widened to the stopped run (it began an hour ago)');
  assert.ok(payload.made.length >= 7);
  for (let i = 0; i < 40; i += 1) {
    const out = await svc.handleRestoreJob({ guildId: '961', payload }, { guild, client: guild.client, lateByMs: 0, attempt: 1 });
    if (out === undefined) break;
    payload = roundTrip(out.payload);
  }
  assert.equal(guild.log.create.length, createsBefore, 'nothing is made a second time');
  for (const n of ['Mod', 'Member']) assert.equal(nameOf(guild).filter((x) => x === n).length, 1, n);
  assert.equal(nameOf(guild).filter((x) => x === 'Admin').length, 2, 'the stopped run\'s Admin and the stranger\'s');
  for (const n of ['Info', 'Lounge', 'help']) assert.equal(chanNames(guild).filter((x) => x === n).length, 1, n);
  const ids = [...Object.values(payload.idMap ?? {}), ...(payload.made ?? []), ...Object.values(backupRows.get(row.id).data.idMap ?? {})];
  assert.ok(!ids.includes(strangerRole.id) && !ids.includes(strangerChannel.id), 'what someone else made is never adopted');
  assert.ok(!guild.log.put.some((p) => p.route.startsWith(`/channels/${strangerChannel.id}/`)), 'nothing is written onto it');
  const general = [...guild.channels.cache.values()].find((c) => c.name === 'general' && c.id !== strangerChannel.id);
  assert.ok(guild.log.put.some((p) => p.route === `/channels/${general.id}/permissions/777`), 'a channel the stopped run made still gets its member overwrites in mode missing');

  // Stopped as stale: the ids go into the backup and the job ends FAILED (kept), so the same holds after a long outage.
  const late = makeGuild({ id: '962', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const lateRow = await seedBackup('962');
  const lateState = await stepUntil(late, lateRow.data, snap.newRestoreState({ backupId: lateRow.id, mode: 'missing', startedById: 'OWNER', channelId: 'NOPE' }), 'channelPositions');
  await assert.rejects(svc.handleRestoreJob({ guildId: '962', payload: roundTrip(lateState) }, { guild: late, client: late.client, lateByMs: svc.BACKUP.STALE_MS + 1, attempt: 1 }), PermanentJobError);
  const stored = backupRows.get(lateRow.id).data.idMap;
  assert.ok(stored['102'] && late.roles.cache.has(stored['102']), 'the ids are stored in the backup');
  assert.equal(svc.restorePreview(late, snap.parseSnapshot(backupRows.get(lateRow.id).data), 'missing').roles.create, 0);
});

test('REVIEW: mode missing adds a RECREATED role\'s overwrites to channels that still exist, only where there is none; it changes no existing one', async () => {
  // A raid deleted Mod (102); the channels still exist, and general's own overwrites differ from the backup.
  const guild = makeGuild({
    roles: [{ id: '101', name: 'Admin', pos: 8 }, { id: '103', name: 'Member', pos: 3 }, { id: '104', name: 'SomeBot', managed: true, pos: 2 }, { id: '999', name: 'Bot', pos: 10 }],
    channels: [
      { id: '201', type: ChannelType.GuildCategory, name: 'Info', pos: 0 },
      { id: '202', name: 'general', parent: '201', pos: 0, overwrites: [{ id: '104', allow: bigPerms(P.SendMessages) }] },
      { id: '203', type: ChannelType.GuildVoice, name: 'Lounge', parent: '201', pos: 1 },
      { id: '206', type: ChannelType.GuildForum, name: 'help', pos: 2 },
    ],
    members: ['777', '778'],
  });
  assert.equal(snap.previewRestore({ guild, snapshot: SNAPSHOT, mode: 'missing' }).overwrites, 1, 'the preview counts Mod on general');
  const { state } = await restoreAll(guild);
  const mod = [...guild.roles.cache.values()].find((r) => r.name === 'Mod');
  assert.ok(mod && mod.id !== '102');
  assert.deepEqual(guild.log.put.map((p) => p.route), [`/channels/202/permissions/${mod.id}`], 'only the recreated role; @everyone, roles that exist and members are left alone');
  assert.equal(guild.log.put[0].body.allow, String(bigPerms(P.ViewChannel, P.ManageMessages)));
  assert.equal(guild.channels.cache.get('202').permissionOverwrites.cache.get('104').allow.bitfield, bigPerms(P.SendMessages), 'an existing overwrite is not changed');
  assert.deepEqual(guild.log.edit, [], 'nothing that exists is edited');
  assert.equal(state.errors.length, 0, JSON.stringify(state.errors));

  // Additive only: the channel already has an overwrite for the recreated role (an admin changed it since): it stays.
  guild.channels.cache.get('202').permissionOverwrites.cache.set(mod.id, { id: mod.id, type: OverwriteType.Role, allow: { bitfield: 0n }, deny: { bitfield: bigPerms(P.ViewChannel) } });
  const before = guild.log.put.length;
  assert.equal(snap.previewRestore({ guild, snapshot: SNAPSHOT, mode: 'missing', idMap: state.idMap }).overwrites, 0);
  await restoreAll(guild, { idMap: state.idMap });
  assert.equal(guild.log.put.length, before, 'never changes an existing overwrite');
});

test('REVIEW: no restore without BOTH Manage Roles and Manage Channels: refused before scheduling, stopped in the job, never an open channel', async () => {
  // The command and the button refuse when either one is missing, and say which; nothing is scheduled.
  for (const [botPerms, lacking] of [[[P.ManageChannels, P.ViewChannel, P.SendMessages], 'Manage Roles'], [[P.ManageRoles, P.ViewChannel, P.SendMessages], 'Manage Channels']]) {
    const guild = makeGuild({ id: '964', ownerId: 'OWNER', botPerms, roles: [{ id: '999', name: 'Bot', pos: 10 }] });
    const row = await seedBackup('964');
    const c = interactionFor(guild, { sub: 'restore', opts: { backup: row.id } });
    await cmd.execute(c.i);
    const embed = c.out.replies.at(-1).embeds[0].toJSON();
    assert.match(embed.title, /Missing Permissions/);
    assert.match(embed.description, new RegExp(lacking));
    assert.equal(c.out.replies.at(-1).components?.length ?? 0, 0, 'no Confirm button');
    const b = buttonFor(guild, `backup:restore:${row.id}:missing`);
    await svc.handleBackupButton(b.i, {});
    assert.match(lastTitle(b.calls), /Missing Permissions/);
    assert.equal(jobRows.has('backup.restore:964'), false, 'nothing scheduled');
  }

  // Manage Roles is taken away while the restore runs: the job stops with a report (resumable) instead of making open channels.
  const guild = makeGuild({ id: '965', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const channel = summaryChannel('CH965');
  channel.guild = guild;
  guild.channels.cache.set(channel.id, channel);
  const row = await seedBackup('965');
  const state = snap.newRestoreState({ backupId: row.id, mode: 'missing', startedById: 'OWNER', channelId: channel.id });
  guild.members.me.permissions = new PermissionsBitField([P.ManageChannels, P.ViewChannel, P.SendMessages, P.EmbedLinks]);
  await assert.rejects(svc.handleRestoreJob({ guildId: '965', payload: roundTrip(state) }, { guild, client: guild.client, lateByMs: 0, attempt: 1 }), PermanentJobError);
  assert.equal(guild.log.create.length, 0, 'nothing made');
  const summary = channel.sent.at(-1).embeds[0].toJSON();
  assert.match(summary.title, /Stopped/);
  assert.match(summary.description, /Manage Roles/);

  // The engine itself never creates a channel without Manage Roles either.
  const eng = makeGuild({ roles: [{ id: '999', name: 'Bot', pos: 10 }], botPerms: [P.ManageChannels, P.ViewChannel] });
  const { state: s } = await restoreAll(eng);
  assert.equal(eng.log.create.filter(([kind]) => kind === 'channel').length, 0);
  assert.ok(s.errors.some((e) => /Manage Roles permission to give channels their permissions/.test(e)), JSON.stringify(s.errors));
});

// ── Command: owner-only rules ─────────────────────────────────────────────────

function interactionFor(guild, { sub, opts = {}, userId = 'OWNER', focused = null } = {}) {
  const out = { replies: [], responded: null };
  const i = {
    guild, guildId: guild.id, channelId: 'CHAN', user: { id: userId }, deferred: false, replied: false, ephemeral: null,
    options: {
      getSubcommand: () => sub,
      getString: (name, required) => {
        const v = opts[name] ?? null;
        if (required && v == null) throw new Error(`missing ${name}`);
        return v;
      },
      getFocused: () => focused,
    },
    async deferReply({ ephemeral }) { i.deferred = true; i.ephemeral = Boolean(ephemeral); },
    async reply(payload) { out.replies.push({ via: 'reply', ...payload }); i.replied = true; return { resource: { message: {} } }; },
    async editReply(payload) { out.replies.push({ via: 'editReply', ...payload }); i.replied = true; return {}; },
    async followUp(payload) { out.replies.push({ via: 'followUp', ...payload }); return {}; },
    async respond(list) { out.responded = list; },
  };
  return { i, out };
}
const titleOf = (payload) => payload.embeds[0].toJSON().title;

test('/backup delete and restore refuse everyone but the server owner, before anything else', async () => {
  const guild = makeGuild({ id: '940', ownerId: 'OWNER' });
  const row = await seedBackup('940');
  for (const sub of ['delete', 'restore']) {
    for (const userId of ['ADMIN1', process.env.BOT_OWNER_ID ?? 'BOTOWNER']) {
      const { i, out } = interactionFor(guild, { sub, opts: { backup: row.id }, userId });
      await cmd.execute(i);
      assert.equal(i.deferred, false, `${sub}: refused before deferring`);
      assert.equal(out.replies.length, 1);
      assert.match(titleOf(out.replies[0]), /Server Owner Only/);
      assert.equal(out.replies[0].ephemeral, true);
    }
  }
  assert.ok(backupRows.has(row.id), 'nothing was deleted');
  assert.equal(jobRows.has('backup.restore:940'), false);
});

test('/backup restore (owner): shows what will happen with Confirm and Cancel; nothing is scheduled yet', async () => {
  const guild = makeGuild({ id: '941', ownerId: 'OWNER', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const row = await seedBackup('941');
  const { i, out } = interactionFor(guild, { sub: 'restore', opts: { backup: row.id, mode: 'full' } });
  await cmd.execute(i);
  assert.equal(i.deferred, true);
  assert.equal(i.ephemeral, true);
  const reply = out.replies.at(-1);
  const embed = reply.embeds[0].toJSON();
  assert.match(embed.title, /Restore This Backup/);
  assert.match(embed.description, /Full/);
  assert.ok(embed.fields.some((f) => f.name === 'Roles'));
  const ids = reply.components[0].toJSON().components.map((c) => c.custom_id);
  assert.deepEqual(ids, [`backup:restore:${row.id}:full`, 'backup:cancel']);
  assert.ok(ids.every((id) => id.length <= 100));
  assert.equal(jobRows.has('backup.restore:941'), false, 'confirm schedules it, the command does not');

  // Default mode is missing.
  const second = interactionFor(guild, { sub: 'restore', opts: { backup: row.id } });
  await cmd.execute(second.i);
  assert.deepEqual(second.out.replies.at(-1).components[0].toJSON().components.map((c) => c.custom_id)[0], `backup:restore:${row.id}:missing`);

  // Another server's backup id is not found.
  const other = await seedBackup('942');
  const foreign = interactionFor(guild, { sub: 'restore', opts: { backup: other.id } });
  await cmd.execute(foreign.i);
  assert.match(titleOf(foreign.out.replies.at(-1)), /Backup Not Found/);
  assert.equal(foreign.out.replies.at(-1).components, undefined);
});

test('/backup restore is refused while a restore runs, and without the bot permissions', async () => {
  const guild = makeGuild({ id: '943', ownerId: 'OWNER', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const row = await seedBackup('943');
  jobRows.set('backup.restore:943', { id: 'j', status: 'RUNNING', dedupeKey: 'backup.restore:943', payload: { backupId: row.id }, runAt: new Date(), version: 0 });
  const busy = interactionFor(guild, { sub: 'restore', opts: { backup: row.id } });
  await cmd.execute(busy.i);
  assert.match(titleOf(busy.out.replies.at(-1)), /Already Running/);
  jobRows.delete('backup.restore:943');

  const weak = makeGuild({ id: '944', ownerId: 'OWNER', botPerms: [P.ViewChannel, P.SendMessages] });
  const weakRow = await seedBackup('944');
  const refused = interactionFor(weak, { sub: 'restore', opts: { backup: weakRow.id } });
  await cmd.execute(refused.i);
  assert.match(titleOf(refused.out.replies.at(-1)), /Missing Permissions/);
});

test('/backup delete (owner): confirms, deletes only that guild\'s backup, and refuses one being restored', async () => {
  const guild = makeGuild({ id: '945', ownerId: 'OWNER' });
  const row = await seedBackup('945');
  const other = await seedBackup('946');

  const declined = interactionFor(guild, { sub: 'delete', opts: { backup: row.id } });
  await cmd.runDelete(declined.i, { confirm: async () => false });
  assert.ok(backupRows.has(row.id));

  jobRows.set('backup.restore:945', { id: 'j', status: 'PENDING', dedupeKey: 'backup.restore:945', payload: { backupId: row.id }, runAt: new Date(), version: 0 });
  const busy = interactionFor(guild, { sub: 'delete', opts: { backup: row.id } });
  let asked = false;
  await cmd.runDelete(busy.i, { confirm: async () => { asked = true; return true; } });
  assert.equal(asked, false, 'not even asked');
  assert.match(titleOf(busy.out.replies.at(-1)), /Restore Running/);
  assert.ok(backupRows.has(row.id));
  jobRows.delete('backup.restore:945');

  const foreign = interactionFor(guild, { sub: 'delete', opts: { backup: other.id } });
  await cmd.runDelete(foreign.i, { confirm: async () => true });
  assert.ok(backupRows.has(other.id), 'another server\'s backup cannot be deleted');
  assert.match(titleOf(foreign.out.replies.at(-1)), /Backup Not Found/);

  const yes = interactionFor(guild, { sub: 'delete', opts: { backup: row.id } });
  await cmd.runDelete(yes.i, { confirm: async () => true });
  assert.equal(backupRows.has(row.id), false);
  assert.match(titleOf(yes.out.replies.at(-1)), /Backup Deleted/);
  assert.equal(yes.i.ephemeral, true, 'the whole conversation is private');
});

test('/backup create, list, info, export and status (admin replies are private)', async () => {
  const guild = makeGuild({ id: '947', ownerId: 'OWNER', roles: [{ id: '999', name: 'Bot', pos: 10 }, ...ORIGINAL.roles], channels: ORIGINAL.channels });
  await enable('947', { backupIntervalHours: 12, backupKeep: 3 });

  const created = interactionFor(guild, { sub: 'create', opts: { name: 'before the raid' }, userId: 'ADMIN1' });
  await cmd.execute(created.i);
  assert.equal(created.i.ephemeral, true);
  assert.match(titleOf(created.out.replies.at(-1)), /Backup Saved/);
  const row = [...backupRows.values()].find((r) => r.guildId === '947');
  assert.equal(row.name, 'before the raid');
  assert.equal(row.createdById, 'ADMIN1');
  assert.equal(row.data.version, 1);
  assert.equal(row.roleCount, 5, 'the bot role and the four of the original server');
  assert.equal(row.channelCount, 4, 'the thread and the directory are not counted');
  assert.equal(jobRows.has('backup.auto:947'), true, 'an admin touching the module keeps the schedule alive');

  const list = interactionFor(guild, { sub: 'list' });
  await cmd.execute(list.i);
  assert.match(list.out.replies.at(-1).embeds[0].toJSON().description, /before the raid/);

  const info = interactionFor(guild, { sub: 'info', opts: { backup: row.id } });
  await cmd.execute(info.i);
  const infoEmbed = info.out.replies.at(-1).embeds[0].toJSON();
  assert.match(infoEmbed.title, /before the raid/);
  assert.ok(infoEmbed.fields.some((f) => f.name === 'Not included' && /Threads, messages, members/.test(f.value)));

  backupRows.get(row.id).data = { ...backupRows.get(row.id).data, idMap: { 1: '2' } };
  const exp = interactionFor(guild, { sub: 'export', opts: { backup: row.id } });
  await cmd.execute(exp.i);
  const file = exp.out.replies.at(-1).files[0];
  assert.match(file.name, /^backup-\d{8}-[a-z0-9]+\.json$/);
  const json = JSON.parse(file.attachment.toString('utf8'));
  assert.equal(json.version, 1);
  assert.equal('idMap' in json, false, 'bookkeeping is not exported');
  assert.equal(exp.i.ephemeral, true);

  jobRows.set('backup.restore:947', { id: 'j', status: 'RUNNING', dedupeKey: 'backup.restore:947', payload: snap.newRestoreState({ backupId: row.id, mode: 'full', startedById: 'OWNER', channelId: 'C' }), runAt: new Date(), version: 0 });
  const status = interactionFor(guild, { sub: 'status' });
  await cmd.execute(status.i);
  const statusEmbed = status.out.replies.at(-1).embeds[0].toJSON();
  assert.ok(statusEmbed.fields.some((f) => f.name === 'Automatic backups' && /Every \*\*12\*\* hours/.test(f.value)));
  assert.ok(statusEmbed.fields.some((f) => f.name === 'Restore' && /Restoring \(full\)/.test(f.value)));
  assert.ok(statusEmbed.fields.some((f) => f.name === 'Stored' && /1 \/ 10 manual/.test(f.value)));
  jobRows.delete('backup.restore:947');

  const missing = interactionFor(guild, { sub: 'info', opts: { backup: 'nope' } });
  await cmd.execute(missing.i);
  assert.match(titleOf(missing.out.replies.at(-1)), /Backup Not Found/);
});

test('/backup autocomplete lists backups by name and date, only for the backup option', async () => {
  const guild = makeGuild({ id: '948' });
  await svc.createBackup({ guild, createdById: 'U', name: 'alpha' });
  await svc.createBackup({ guild, createdById: 'U', name: 'beta' });
  await svc.createBackup({ guild: makeGuild({ id: '949' }), createdById: 'U', name: 'alpha elsewhere' });
  const a = interactionFor(guild, { sub: 'info', focused: { name: 'backup', value: 'alp' } });
  a.i.options.getFocused = () => ({ name: 'backup', value: 'alp' });
  await cmd.autocomplete(a.i);
  assert.equal(a.out.responded.length, 1);
  assert.match(a.out.responded[0].name, /^alpha · \d{4}-\d{2}-\d{2} \d{2}:\d{2} UTC$/);
  assert.ok(a.out.responded[0].value.startsWith('bk'));
  const all = interactionFor(guild, { sub: 'info' });
  all.i.options.getFocused = () => ({ name: 'backup', value: '' });
  await cmd.autocomplete(all.i);
  assert.equal(all.out.responded.length, 2, 'this server\'s backups only');
  const other = interactionFor(guild, { sub: 'restore' });
  other.i.options.getFocused = () => ({ name: 'mode', value: '' });
  await cmd.autocomplete(other.i);
  assert.deepEqual(other.out.responded, []);
});

// ── Confirmation buttons ──────────────────────────────────────────────────────

function buttonFor(guild, customId, { userId = 'OWNER', age = 1000, channelId = 'CHAN' } = {}) {
  const calls = [];
  const i = {
    customId, guild, guildId: guild.id, channelId, user: { id: userId }, deferred: false, replied: false, ephemeral: null,
    message: { createdTimestamp: Date.now() - age },
    async deferUpdate() { calls.push(['deferUpdate']); i.deferred = true; },
    async update(payload) { calls.push(['update', payload]); i.replied = true; },
    async editReply(payload) { calls.push(['editReply', payload]); return {}; },
    async reply(payload) { calls.push(['reply', payload]); i.replied = true; return { resource: { message: {} } }; },
    async followUp(payload) { calls.push(['followUp', payload]); return {}; },
  };
  return { i, calls };
}
const lastTitle = (calls) => {
  const hit = calls.filter(([, p]) => p?.embeds?.length).at(-1);
  return hit[1].embeds[0].toJSON().title;
};

test('Confirm button: a server administrator who is not the owner is refused, in front of everything else', async () => {
  const guild = makeGuild({ id: '950', ownerId: 'OWNER', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const row = await seedBackup('950');
  const { i, calls } = buttonFor(guild, `backup:restore:${row.id}:missing`, { userId: 'ADMIN1' });
  await svc.handleBackupButton(i, { guilds: { cache: new Map() } });
  assert.deepEqual(calls.map(([name]) => name), ['reply']);
  assert.match(lastTitle(calls), /Server Owner Only/);
  assert.equal(calls[0][1].ephemeral, true);
  assert.equal(jobRows.has('backup.restore:950'), false);

  const cancel = buttonFor(guild, 'backup:cancel', { userId: 'ADMIN1' });
  await svc.handleBackupButton(cancel.i, {});
  assert.match(lastTitle(cancel.calls), /Server Owner Only/, 'even Cancel is the owner\'s');
});

test('Confirm button: the owner confirms, the job is scheduled with the mode, a second Confirm is refused', async () => {
  const guild = makeGuild({ id: '951', ownerId: 'OWNER', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const row = await seedBackup('951');
  const first = buttonFor(guild, `backup:restore:${row.id}:full`, { channelId: 'CHAN-A' });
  await svc.handleBackupButton(first.i, {});
  assert.deepEqual(first.calls.map(([name]) => name), ['deferUpdate', 'editReply']);
  assert.match(lastTitle(first.calls), /Restore Started/);
  assert.deepEqual(first.calls.at(-1)[1].components, [], 'the buttons are gone');
  const job = jobRows.get('backup.restore:951');
  assert.equal(job.payload.mode, 'full');
  assert.equal(job.payload.backupId, row.id);
  assert.equal(job.payload.channelId, 'CHAN-A');
  assert.equal(job.payload.startedById, 'OWNER');
  assert.equal(job.type, 'backup.restore');

  const second = buttonFor(guild, `backup:restore:${row.id}:missing`);
  await svc.handleBackupButton(second.i, {});
  assert.match(lastTitle(second.calls), /Already Running/);
  assert.equal(jobRows.get('backup.restore:951').payload.mode, 'full', 'the running restore was not replaced');
  assert.equal(jobRows.get('backup.restore:951').version, job.version);
});

test('Confirm button: expired after 5 minutes (judged from the message), unknown backup, bad mode, cancel', async () => {
  const guild = makeGuild({ id: '952', ownerId: 'OWNER', roles: [{ id: '999', name: 'Bot', pos: 10 }] });
  const row = await seedBackup('952');
  const stale = buttonFor(guild, `backup:restore:${row.id}:missing`, { age: svc.BACKUP.CONFIRM_TTL_MS + 5000 });
  await svc.handleBackupButton(stale.i, {});
  assert.match(lastTitle(stale.calls), /Confirmation Expired/);
  assert.equal(stale.calls.some(([name]) => name === 'deferUpdate'), false);
  assert.equal(jobRows.has('backup.restore:952'), false);

  const fresh = buttonFor(guild, `backup:restore:${row.id}:missing`, { age: svc.BACKUP.CONFIRM_TTL_MS - 5000 });
  await svc.handleBackupButton(fresh.i, {});
  assert.match(lastTitle(fresh.calls), /Restore Started/);
  jobRows.delete('backup.restore:952');

  const unknown = buttonFor(guild, 'backup:restore:nope:missing');
  await svc.handleBackupButton(unknown.i, {});
  assert.match(lastTitle(unknown.calls), /Backup Not Found/);

  const foreign = await seedBackup('953');
  const cross = buttonFor(guild, `backup:restore:${foreign.id}:missing`);
  await svc.handleBackupButton(cross.i, {});
  assert.match(lastTitle(cross.calls), /Backup Not Found/, 'another server\'s backup');
  assert.equal(jobRows.has('backup.restore:952'), false);

  const badMode = buttonFor(guild, `backup:restore:${row.id}:everything`);
  await svc.handleBackupButton(badMode.i, {});
  assert.equal(jobRows.has('backup.restore:952'), false);
  assert.equal(badMode.calls[0][0], 'reply');

  const cancel = buttonFor(guild, 'backup:cancel');
  await svc.handleBackupButton(cancel.i, {});
  assert.deepEqual(cancel.calls.map(([name]) => name), ['update']);
  assert.match(lastTitle(cancel.calls), /Restore Cancelled/);
  assert.deepEqual(cancel.calls[0][1].components, []);
});

test('Confirm button: without Manage Roles and Manage Channels nothing is scheduled', async () => {
  const guild = makeGuild({ id: '954', ownerId: 'OWNER', botPerms: [P.ViewChannel] });
  const row = await seedBackup('954');
  const { i, calls } = buttonFor(guild, `backup:restore:${row.id}:missing`);
  await svc.handleBackupButton(i, {});
  assert.match(lastTitle(calls), /Missing Permissions/);
  assert.equal(jobRows.has('backup.restore:954'), false);
});

// ── Declarations ──────────────────────────────────────────────────────────────

test('plugin declaration: guildReady hook, the backup button for any state of the module, both jobs', async () => {
  const { plugin } = backups;
  assert.equal(plugin.id, 'backups');
  assert.equal(plugin.module, 'backups');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id, h.always === true]), [['guildReady', 'backups', true]]);
  assert.ok(HOOK_ORDER.guildReady.includes('backups'));
  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix, c.module]), [['button', 'backup', null]]);
  assert.deepEqual(plugin.jobs.map((j) => j.type).sort(), ['backup.auto', 'backup.restore']);
  assert.deepEqual(CUSTOM_ID_PREFIXES.backup, { owner: 'backups', types: ['button'], kind: 'persistent' });
  assert.equal(JOB_TYPES['backup.auto'].recurring, true);
  assert.equal(JOB_TYPES['backup.auto'].dedupe, 'backup.auto:{guildId}');
  assert.equal(JOB_TYPES['backup.restore'].recurring, false);
  assert.equal(JOB_TYPES['backup.restore'].dedupe, 'backup.restore:{guildId}');
  assert.equal(MODULES.backups.configCommand, '/backup');
  assert.equal(DEFAULT_MODULE_SETTINGS.backups, false);
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'backups'));
  assert.deepEqual(errors.filter((e) => String(e?.message ?? e).includes('backups')), []);
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/backup payload and command exports', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'backup');
  assert.equal(json.default_member_permissions, '32');
  assert.deepEqual(json.contexts, [0]);
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['create', 1], ['list', 1], ['info', 1], ['export', 1], ['status', 1], ['delete', 1], ['restore', 1]]);
  const sub = Object.fromEntries(json.options.map((o) => [o.name, o]));
  assert.deepEqual(sub.create.options.map((o) => [o.name, o.type, !!o.required, o.max_length]), [['name', 3, false, 64]]);
  for (const name of ['info', 'export', 'delete']) {
    assert.deepEqual(sub[name].options.map((o) => [o.name, o.type, !!o.required, !!o.autocomplete]), [['backup', 3, true, true]], name);
  }
  assert.deepEqual(sub.restore.options.map((o) => [o.name, !!o.required, !!o.autocomplete]), [['backup', true, true], ['mode', false, false]]);
  assert.deepEqual(sub.restore.options[1].choices.map((c) => c.value), ['missing', 'full']);
  assert.ok(charCount(json) <= 1500, `payload is ${charCount(json)} chars`);
  assert.ok(JSON.stringify(json).length < 4000);
  assert.equal(cmd.module, 'backups');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(typeof cmd.autocomplete, 'function');
  assert.equal(cmd.subcommandPermissions, undefined, 'owner-only is checked in the code: PERMISSION_LEVELS.OWNER is the bot owner');
});
