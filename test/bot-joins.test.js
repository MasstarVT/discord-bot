// Bot joins tests. Offline (no Discord, Postgres or Redis): who added the bot (audit log first, then the integration),
// the guildReady recording against an in-memory BotJoin table, the dashboard read and the plugin declaration.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const { AuditLogEvent, Collection, PermissionFlagsBits } = await import('discord.js');
const { findInviter, recordBotJoin, listBotJoins } = await import('../src/services/botJoinService.js');
const { plugin } = await import('../src/plugins/bot-joins.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// In-memory BotJoin table; GuildSettings rows always "exist".
const rows = new Map();
const realBotJoin = prisma.botJoin;
const realGuildSettings = prisma.guildSettings;
prisma.botJoin = {
  findUnique: async ({ where }) => (rows.has(where.guildId) ? { ...rows.get(where.guildId) } : null),
  findMany: async ({ where }) => [...rows.values()].filter((r) => where.guildId.in.includes(r.guildId)).map((r) => ({ ...r })),
  upsert: async ({ where, create, update }) => {
    const row = rows.has(where.guildId) ? { ...rows.get(where.guildId), ...update } : { ...create };
    rows.set(where.guildId, row);
    return { ...row };
  },
};
prisma.guildSettings = { upsert: async () => ({}) };

after(async () => {
  prisma.botJoin = realBotJoin;
  prisma.guildSettings = realGuildSettings;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const BOT = '100000000000000001';
const OTHER_BOT = '100000000000000002';
const ALICE = { id: '200000000000000001', tag: 'alice' };
const BOB = { id: '200000000000000002', tag: 'bob' };
const JOINED = new Date('2026-10-01T12:00:00.000Z');
let nextGuild = 300000000000000000n;

const auditEntry = (targetId, user) => ({ targetId, executorId: user.id, executor: user });
const integration = ({ appId = null, accountId = null, user = null }) => ({
  type: 'discord', application: appId ? { id: appId } : null, account: accountId ? { id: accountId } : null, user,
});
const toCollection = (list) => new Collection(list.map((x, i) => [String(i), x]));

function fakeGuild({ perms = [], audit = [], integrations = [], fail = false } = {}) {
  const calls = [];
  return {
    id: String(nextGuild++),
    joinedAt: JOINED,
    calls,
    members: { me: { permissions: { has: (bit) => perms.includes(bit) } } },
    fetchAuditLogs: async (opts) => {
      calls.push(['audit', opts.type]);
      if (fail) throw new Error('Missing Access');
      return { entries: toCollection(audit) };
    },
    fetchIntegrations: async () => {
      calls.push(['integrations']);
      if (fail) throw new Error('Missing Access');
      return toCollection(integrations);
    },
  };
}
const client = { user: { id: BOT }, application: { id: BOT } };
const { ViewAuditLog, ManageGuild } = PermissionFlagsBits;

test('the audit log names the newest inviter of this bot', async () => {
  const guild = fakeGuild({
    perms: [ViewAuditLog, ManageGuild],
    audit: [auditEntry(OTHER_BOT, BOB), auditEntry(BOT, ALICE), auditEntry(BOT, BOB)],
  });
  assert.deepEqual(await findInviter(guild, BOT), { inviterId: ALICE.id, inviterName: 'alice', source: 'audit' });
  assert.deepEqual(guild.calls, [['audit', AuditLogEvent.BotAdd]], 'the integration is not needed');
});

test('without an audit entry or View Audit Log, the integration names the inviter', async () => {
  const integrations = [integration({ appId: OTHER_BOT, user: BOB }), integration({ appId: BOT, user: ALICE })];
  const expired = fakeGuild({ perms: [ViewAuditLog, ManageGuild], audit: [auditEntry(OTHER_BOT, BOB)], integrations });
  assert.deepEqual(await findInviter(expired, BOT), { inviterId: ALICE.id, inviterName: 'alice', source: 'integration' });

  const noAudit = fakeGuild({ perms: [ManageGuild], integrations: [integration({ accountId: BOT, user: BOB })] });
  assert.deepEqual(await findInviter(noAudit, BOT, '999'), { inviterId: BOB.id, inviterName: 'bob', source: 'integration' });
  assert.deepEqual(noAudit.calls, [['integrations']], 'no audit-log call without View Audit Log');
});

test('no permission, a failing fetch or no match is unknown', async () => {
  const none = fakeGuild({ audit: [auditEntry(BOT, ALICE)] });
  assert.deepEqual(await findInviter(none, BOT), { inviterId: null, inviterName: null, source: null });
  assert.deepEqual(none.calls, []);
  const failing = fakeGuild({ perms: [ViewAuditLog, ManageGuild], fail: true });
  assert.deepEqual(await findInviter(failing, BOT), { inviterId: null, inviterName: null, source: null });
  const userless = fakeGuild({ perms: [ManageGuild], integrations: [integration({ appId: BOT })] });
  assert.equal((await findInviter(userless, BOT)).inviterId, null);
});

test('startup records a guild once; a join replaces the row', async () => {
  const guild = fakeGuild({ perms: [ViewAuditLog], audit: [auditEntry(BOT, ALICE)] });
  await recordBotJoin({ client, guild, reason: 'startup' });
  assert.deepEqual(rows.get(guild.id), {
    guildId: guild.id, inviterId: ALICE.id, inviterName: 'alice', source: 'audit', joinedAt: JOINED,
  });
  assert.equal(guild.calls.length, 1);

  await recordBotJoin({ client, guild, reason: 'available' });
  assert.equal(guild.calls.length, 1, 'a known guild is not looked up again');

  // Kicked and added again by someone else.
  const readded = { ...guild, calls: [], fetchAuditLogs: async () => ({ entries: toCollection([auditEntry(BOT, BOB)]) }) };
  await recordBotJoin({ client, guild: readded, reason: 'join' });
  assert.equal(rows.get(guild.id).inviterId, BOB.id);

  // Unknown is stored too (with the join date), and the next startup tries again: here the permission was granted.
  const blind = fakeGuild();
  await recordBotJoin({ client, guild: blind, reason: 'startup' });
  assert.equal(rows.get(blind.id).inviterId, null);
  assert.equal(rows.get(blind.id).joinedAt, JOINED);
  const granted = { ...blind, members: { me: { permissions: { has: (bit) => bit === ManageGuild } } },
    fetchIntegrations: async () => toCollection([integration({ appId: BOT, user: ALICE })]) };
  await recordBotJoin({ client, guild: granted, reason: 'startup' });
  assert.equal(rows.get(blind.id).inviterId, ALICE.id);
  assert.equal(rows.get(blind.id).source, 'integration');
});

test('listBotJoins returns the rows by guild id', async () => {
  const guild = fakeGuild({ perms: [ViewAuditLog], audit: [auditEntry(BOT, ALICE)] });
  await recordBotJoin({ client, guild, reason: 'startup' });
  const map = await listBotJoins([guild.id, '1']);
  assert.deepEqual([...map.keys()], [guild.id]);
  assert.equal(map.get(guild.id).inviterName, 'alice');
  assert.equal(Object.hasOwn(map.get(guild.id), 'guildId'), false);
  assert.equal((await listBotJoins([])).size, 0);
});

test('the plugin is a core guildReady hook', () => {
  assert.equal(plugin.id, 'bot-joins');
  assert.equal(plugin.module, null);
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id]), [['guildReady', 'bot-joins']]);
  assert.ok(HOOK_ORDER.guildReady.includes('bot-joins'));
});
