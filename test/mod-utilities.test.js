// pkg-mod-utilities tests: /purge fixes (R096, R097, R098, R247), /slowmode and /info.
// Offline by default: payloads, pure helpers, and the commands driven through fake channels, guilds and
// interactions. GuildSettings reads are stubbed offline; to run them against a migrated database instead:
//   TEST_DATABASE_URL='<migrated db>' node --test --test-concurrency=1 test/mod-utilities.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is only reached with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;

const { PermissionFlagsBits, PermissionsBitField, ChannelType, Collection } = await import('discord.js');
const purge = await import('../src/commands/moderation/purge.js');
const slowmode = await import('../src/commands/moderation/slowmode.js');
const info = await import('../src/commands/utility/info.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { wasDeletedByBot } = await import('../src/utils/deletionTracker.js');
const { formatDurationLong } = await import('../src/utils/parseDuration.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { updateSettings, invalidateSettings } = await import('../src/services/settingsService.js');

// Offline, GuildSettings reads are served by a stub (the commands only read modVisibility, embedColor and modRoleIds);
// with TEST_DATABASE_URL the real database is used. Redis is never reachable here, so every load goes to Prisma.
const STUBBED = new Map();
if (!DB) {
  prisma.guildSettings.findUnique = async ({ where }) => ({
    guildId: where.guildId, modVisibility: 'PUBLIC', modRoleIds: [], embedColor: null, ...(STUBBED.get(where.guildId) ?? {}),
  });
}
async function setVisibility(guildId, mode) {
  if (DB) await updateSettings(guildId, { modVisibility: mode });
  else {
    STUBBED.set(guildId, { modVisibility: mode });
    await invalidateSettings(guildId);
  }
}

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};
after(async () => {
  Object.assign(console, quiet);
  // Any GuildSettings row the commands created while loading settings (guild ids with this package's prefix).
  if (DB) await prisma.guildSettings.deleteMany({ where: { guildId: { startsWith: '95500500' } } }).catch(() => {});
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

const embedText = (e) => {
  const j = e.toJSON ? e.toJSON() : e;
  return `${j.title ?? ''}\n${j.description ?? ''}`;
};

// ═════════════════════════════════════════════════════════════════════════════
// Payloads and exports
// ═════════════════════════════════════════════════════════════════════════════

test('payload sizes stay inside the 00-overview §7 budgets', () => {
  assert.ok(charCount(buildCommandPayload(purge)) <= 430, `/purge ${charCount(buildCommandPayload(purge))}`);
  assert.ok(charCount(buildCommandPayload(slowmode)) <= 220, `/slowmode ${charCount(buildCommandPayload(slowmode))}`);
  assert.ok(charCount(buildCommandPayload(info)) <= 280, `/info ${charCount(buildCommandPayload(info))}`);
});

test('/purge payload', () => {
  const json = buildCommandPayload(purge);
  const opt = (n) => json.options.find((o) => o.name === n);
  assert.deepEqual(opt('filter').choices.map((c) => c.value), ['bots', 'humans', 'embeds', 'attachments']);
  assert.equal(opt('amount').min_value, 1);
  assert.equal(opt('amount').max_value, 1000);
  assert.equal(opt('contains').max_length, 100);
  assert.equal(opt('timeframe').max_length, 32);
  assert.equal(opt('pinned').type, 5);
  // D15: ModerateMembers matches the MODERATOR level the router enforces (like /warn).
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ModerateMembers));
  assert.ok(json.options.every((o) => !o.required), 'every option is optional');
});

test('/slowmode payload', () => {
  const json = buildCommandPayload(slowmode);
  const duration = json.options.find((o) => o.name === 'duration');
  assert.equal(duration.required, true);
  assert.equal(duration.autocomplete, true);
  assert.equal(duration.max_length, 16);
  const channel = json.options.find((o) => o.name === 'channel');
  assert.deepEqual([...channel.channel_types].sort((a, b) => a - b), [0, 2, 10, 11, 12, 13, 15, 16]);
  assert.ok(!channel.channel_types.includes(5), 'announcement channels are excluded');
  assert.equal(json.options.find((o) => o.name === 'reason').max_length, 512);
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ModerateMembers));
});

test('/info payload', () => {
  const json = buildCommandPayload(info);
  assert.deepEqual(json.options.map((o) => o.name), ['user', 'server', 'role']);
  assert.equal(json.options[2].options.find((o) => o.name === 'role').required, true);
  assert.ok(json.default_member_permissions == null, 'everyone can use /info');
});

test('exports and guild-only contexts', () => {
  assert.equal(purge.module, 'moderation');
  assert.equal(purge.permissionLevel, PERMISSION_LEVELS.MODERATOR);
  assert.equal(slowmode.module, 'moderation');
  assert.equal(slowmode.permissionLevel, PERMISSION_LEVELS.MODERATOR);
  assert.equal(typeof slowmode.autocomplete, 'function');
  assert.equal(info.module, null);
  assert.equal(info.permissionLevel, PERMISSION_LEVELS.MEMBER);
  for (const mod of [purge, slowmode, info]) {
    assert.equal(typeof mod.execute, 'function');
    assert.equal(mod.allowInDMs, undefined);
    assert.deepEqual(buildCommandPayload(mod).contexts, [0]);
  }
});

test('static checks on purge.js (spec §9)', () => {
  const src = readFileSync(resolve(ROOT, 'src/commands/moderation/purge.js'), 'utf8');
  assert.ok(!/setTimeout/.test(src), 'no setTimeout: the report is never auto-deleted');
  assert.ok(!/deleted \+= 1/.test(src), 'no uncounted-failure increment');
  assert.ok(!/bulkDelete\([^)]*true\)/.test(src), 'bulkDelete never uses filterOld=true');
  assert.ok(!/console\./.test(src));
  assert.ok(/ViewChannel.*ReadMessageHistory.*ManageMessages/.test(src));
});

// ═════════════════════════════════════════════════════════════════════════════
// /slowmode pure helpers
// ═════════════════════════════════════════════════════════════════════════════

test('parseSlowmode: off words and zero', () => {
  for (const s of ['off', 'OFF', ' none ', '0', '00', '0s']) assert.deepEqual(slowmode.parseSlowmode(s), { ok: true, seconds: 0 }, s);
});

test('parseSlowmode: durations', () => {
  assert.equal(slowmode.parseSlowmode('10').seconds, 10);
  assert.equal(slowmode.parseSlowmode('10s').seconds, 10);
  assert.equal(slowmode.parseSlowmode('1.5m').seconds, 90);
  assert.equal(slowmode.parseSlowmode('2h30m').seconds, 9000);
  assert.equal(slowmode.parseSlowmode('6h').seconds, 21_600);
  assert.equal(slowmode.SLOWMODE_MAX_SECONDS, 21_600);
});

test('parseSlowmode: rejections', () => {
  for (const s of ['6h1s', '7h', '99999999']) assert.deepEqual(slowmode.parseSlowmode(s), { ok: false, code: 'too-long' }, s);
  assert.deepEqual(slowmode.parseSlowmode('0.4s'), { ok: false, code: 'too-short' });
  for (const s of ['abc', '5x', '', 'x'.repeat(17), null, undefined]) assert.deepEqual(slowmode.parseSlowmode(s), { ok: false, code: 'invalid' }, String(s));
});

test('formatSlowmode', () => {
  assert.equal(slowmode.formatSlowmode(0), 'Off');
  assert.equal(slowmode.formatSlowmode(90), formatDurationLong(90_000));
});

test('buildSlowmodeEmbed: titles, moderator line, forum note, reason clamp', () => {
  const base = { channelId: '55', moderatorId: null };
  assert.match(embedText(slowmode.buildSlowmodeEmbed({ ...base, oldSeconds: 0, newSeconds: 10 })), /Slowmode Enabled/);
  assert.match(embedText(slowmode.buildSlowmodeEmbed({ ...base, oldSeconds: 10, newSeconds: 20 })), /Slowmode Updated/);
  assert.match(embedText(slowmode.buildSlowmodeEmbed({ ...base, oldSeconds: 10, newSeconds: 0 })), /Slowmode Disabled/);

  const plain = embedText(slowmode.buildSlowmodeEmbed({ ...base, oldSeconds: 0, newSeconds: 10 }));
  assert.ok(!plain.includes('Moderator'));
  assert.ok(plain.includes('<#55>') && plain.includes('**Previously:** Off'));

  const withMod = embedText(slowmode.buildSlowmodeEmbed({ ...base, moderatorId: '77', oldSeconds: 0, newSeconds: 10, reason: 'calm down' }));
  assert.ok(withMod.includes('**Reason:** calm down') && withMod.includes('**Moderator:** <@77>'));

  const forum = embedText(slowmode.buildSlowmodeEmbed({ ...base, oldSeconds: 0, newSeconds: 10, forum: true }));
  assert.ok(forum.trimEnd().endsWith('*Applies to creating new posts in this forum.*'));

  const long = slowmode.buildSlowmodeEmbed({ ...base, oldSeconds: 0, newSeconds: 10, reason: 'x'.repeat(5000) }).toJSON();
  assert.ok(long.description.length < 1300);
});

// ═════════════════════════════════════════════════════════════════════════════
// Fakes
// ═════════════════════════════════════════════════════════════════════════════

const BASE_ID = 1_000_000_000_000_000_000n;

/** Newest-first fake messages: ids decrease by one, one second apart. */
function makeMessages(n, patch = () => ({}), now = Date.now()) {
  return Array.from({ length: n }, (_, i) => ({
    id: String(BASE_ID - BigInt(i)),
    createdTimestamp: now - i * 1000,
    deletable: true,
    pinned: false,
    author: { id: 'u0', bot: false },
    webhookId: null,
    embeds: [],
    attachments: { size: 0 },
    content: `message ${i}`,
    ...patch(i),
  }));
}

const BOT_PERMS = ['ViewChannel', 'ReadMessageHistory', 'ManageMessages'];

function makeChannel({ id = '5000', messages = [], botPerms = BOT_PERMS, fetchError = null, bulkImpl = null, deleteImpl = null, thread = false, archived = false } = {}) {
  const me = { id: 'bot', isCommunicationDisabled: () => false };
  const guild = { id: '900', members: { me } };
  const calls = { fetch: [], bulk: [], single: [] };
  const channel = {
    id, guild, archived, calls,
    isThread: () => thread,
    permissionsFor: (m) => (m === me ? new PermissionsBitField(botPerms.map((p) => PermissionFlagsBits[p])) : new PermissionsBitField(0n)),
    messages: {
      async fetch(opts) {
        calls.fetch.push({ ...opts });
        if (fetchError) throw fetchError;
        if (opts.limit > 100) throw new Error('limit above 100');
        const older = opts.before ? messages.filter((m) => BigInt(m.id) < BigInt(opts.before)) : messages;
        return new Map(older.slice(0, opts.limit).map((m) => [m.id, m]));
      },
      async delete(mid) {
        calls.single.push(mid);
        if (deleteImpl) await deleteImpl(mid);
      },
    },
    async bulkDelete(ids, filterOld) {
      calls.bulk.push({ ids: [...ids], filterOld });
      if (bulkImpl) await bulkImpl(ids, filterOld);
      return new Collection();
    },
  };
  return { channel, guild, me, calls };
}

function makeInteraction({ options = {}, channel = null, guild = null, member = null, user = { id: 'mod1', tag: 'Mod#0001' }, guildId = '9550050000000001' } = {}) {
  const calls = [];
  const state = { deleted: false };
  return {
    calls, createdTimestamp: Date.now(), deferred: false, replied: false, ephemeral: null,
    guild, guildId, channel, channelId: channel?.id ?? null, member, user,
    options: {
      getInteger: (n) => options[n] ?? null,
      getString: (n, required) => {
        const v = options[n] ?? null;
        if (v === null && required) throw new Error(`missing ${n}`);
        return v;
      },
      getUser: (n) => options[n] ?? null,
      getBoolean: (n) => options[n] ?? null,
      getChannel: (n) => options[n] ?? null,
      getRole: (n) => options[n] ?? null,
      getFocused: () => options.focused ?? '',
      getSubcommand: () => options.sub,
    },
    async reply(p) { calls.push(['reply', p]); this.replied = true; return { resource: { message: {} } }; },
    async deferReply(p) { calls.push(['deferReply', p]); this.deferred = true; this.ephemeral = Boolean(p?.ephemeral); },
    async editReply(p) {
      if (state.deleted) throw Object.assign(new Error('Unknown Message'), { code: 10008 });
      calls.push(['editReply', p]);
      return {};
    },
    async deleteReply() { calls.push(['deleteReply']); state.deleted = true; },
    async followUp(p) { calls.push(['followUp', p]); return {}; },
    async respond(c) { calls.push(['respond', c]); },
    responded: false,
  };
}

const lastEmbedText = (i) => {
  const call = [...i.calls].reverse().find((c) => c[1]?.embeds);
  return call ? embedText(call[1].embeds[0]) : '';
};
const callNames = (i) => i.calls.map((c) => c[0]);

// ═════════════════════════════════════════════════════════════════════════════
// matchesFilters (R247)
// ═════════════════════════════════════════════════════════════════════════════

test('matchesFilters: "Has embed" no longer matches attachment-only messages (R247)', () => {
  assert.equal(purge.matchesFilters({ embeds: [], attachments: { size: 1 } }, { filter: 'embeds' }), false);
  assert.equal(purge.matchesFilters({ embeds: [{}], attachments: { size: 0 } }, { filter: 'embeds' }), true);
  assert.equal(purge.matchesFilters({ embeds: [], attachments: { size: 1 } }, { filter: 'attachments' }), true);
  assert.equal(purge.matchesFilters({ embeds: [{}], attachments: { size: 0 } }, { filter: 'attachments' }), false);
});

test('matchesFilters: webhooks count as bots, not humans', () => {
  const m = { author: { bot: false }, webhookId: '1' };
  assert.equal(purge.matchesFilters(m, { filter: 'humans' }), false);
  assert.equal(purge.matchesFilters(m, { filter: 'bots' }), true);
  assert.equal(purge.matchesFilters({ author: { bot: true } }, { filter: 'bots' }), true);
  assert.equal(purge.matchesFilters({ author: { bot: true } }, { filter: 'humans' }), false);
  assert.equal(purge.matchesFilters({ author: { bot: false }, webhookId: null }, { filter: 'humans' }), true);
});

test('matchesFilters: user + filter + contains are ANDed', () => {
  const m = { author: { id: 'u1', bot: false }, content: 'Say HELLO there', embeds: [], attachments: { size: 0 } };
  assert.equal(purge.matchesFilters(m, { contains: 'hello' }), true);
  assert.equal(purge.matchesFilters(m, { userId: 'u1', filter: 'humans', contains: 'hello' }), true);
  assert.equal(purge.matchesFilters(m, { userId: 'u2', filter: 'humans', contains: 'hello' }), false);
  assert.equal(purge.matchesFilters(m, { userId: 'u1', filter: 'bots', contains: 'hello' }), false);
  assert.equal(purge.matchesFilters(m, { userId: 'u1', filter: 'humans', contains: 'bye' }), false);
  assert.equal(purge.matchesFilters({ author: { id: 'u1' }, content: null }, { contains: 'x' }), false);
});

// ═════════════════════════════════════════════════════════════════════════════
// collectPurgeTargets (R097, R098)
// ═════════════════════════════════════════════════════════════════════════════

test('collect: amount above 100 pages through the channel (R098)', async () => {
  const msgs = makeMessages(600);
  const { channel, calls } = makeChannel({ messages: msgs });
  const r = await purge.collectPurgeTargets(channel, { limit: 500, cutoffTs: -Infinity, cutoffKind: 'age' });
  assert.equal(r.targets.length, 500);
  assert.equal(r.scanned, 500);
  assert.equal(r.stopReason, 'limit');
  assert.equal(calls.fetch.length, 5);
  assert.ok(calls.fetch.every((c) => c.cache === false && c.limit <= 100));
  assert.equal(calls.fetch[0].before, undefined);
  for (let i = 1; i < calls.fetch.length; i++) {
    const prev = msgs.slice((i - 1) * 100, i * 100);
    assert.equal(calls.fetch[i].before, prev[prev.length - 1].id, 'before is the smallest id of the previous page');
  }
});

test('collect: filters are applied inside the scan, so amount counts matches (R098)', async () => {
  const msgs = makeMessages(600, (i) => ({ author: { id: i % 3 === 0 ? 'u1' : 'u2', bot: false } }));
  const { channel } = makeChannel({ messages: msgs });
  const r = await purge.collectPurgeTargets(channel, { limit: 10, userId: 'u1' });
  assert.equal(r.targets.length, 10);
  assert.ok(r.targets.every((m) => m.author.id === 'u1'));
  assert.equal(r.stopReason, 'limit');
  assert.equal(r.scanned, 28, 'stopped as soon as the 10th match was found');
});

test('collect: a small amount with a timeframe reads one page, not the whole window (R097)', async () => {
  const now = Date.now();
  const { channel, calls } = makeChannel({ messages: makeMessages(600, () => ({}), now) });
  const r = await purge.collectPurgeTargets(channel, { limit: 5, cutoffTs: now - 12 * 86_400_000, cutoffKind: 'timeframe' });
  assert.equal(calls.fetch.length, 1);
  assert.equal(r.targets.length, 5);
});

test('collect: every mode is capped at SCAN_CAP examined messages (R097)', async () => {
  const { channel, calls } = makeChannel({ messages: makeMessages(2000) });
  const r = await purge.collectPurgeTargets(channel, { limit: 1000, contains: 'nothing-matches-this' });
  assert.equal(r.scanned, 1000);
  assert.equal(calls.fetch.length, 10);
  assert.equal(r.stopReason, 'scan-cap');
  assert.equal(r.targets.length, 0);
});

test('collect: never collects messages past the cut-off (14 days / timeframe)', async () => {
  const now = Date.now();
  const msgs = makeMessages(120, () => ({}), now);
  const cutoffTs = now - 49_500; // indexes 0..49 are newer
  const a = await purge.collectPurgeTargets(makeChannel({ messages: msgs }).channel, { limit: 1000, cutoffTs, cutoffKind: 'age' });
  assert.equal(a.targets.length, 50);
  assert.equal(a.scanned, 50);
  assert.equal(a.stopReason, 'age');
  assert.ok(a.targets.every((m) => m.createdTimestamp > cutoffTs));
  const b = await purge.collectPurgeTargets(makeChannel({ messages: msgs }).channel, { limit: 1000, cutoffTs, cutoffKind: 'timeframe' });
  assert.equal(b.stopReason, 'timeframe');
  assert.equal(b.targets.length, 50);
});

test('collect: pinned messages are kept unless requested; undeletable ones are skipped', async () => {
  const msgs = makeMessages(10, (i) => ({ pinned: i === 1 || i === 2, deletable: i !== 3 }));
  const kept = await purge.collectPurgeTargets(makeChannel({ messages: msgs }).channel, { limit: 100 });
  assert.equal(kept.targets.length, 7);
  assert.equal(kept.skippedPinned, 2);
  assert.equal(kept.skippedUndeletable, 1);
  const all = await purge.collectPurgeTargets(makeChannel({ messages: msgs }).channel, { limit: 100, includePinned: true });
  assert.equal(all.targets.length, 9);
  assert.equal(all.skippedPinned, 0);
  assert.equal(all.skippedUndeletable, 1);
});

test('collect: a short page means the channel is exhausted', async () => {
  const { channel, calls } = makeChannel({ messages: makeMessages(30) });
  const r = await purge.collectPurgeTargets(channel, { limit: 100 });
  assert.equal(r.stopReason, 'exhausted');
  assert.equal(calls.fetch.length, 1);
  assert.equal(r.targets.length, 30);
});

test('collect: fetch errors become stop reasons and never throw', async () => {
  const perm = await purge.collectPurgeTargets(makeChannel({ fetchError: { code: 50013 } }).channel, { limit: 10 });
  assert.equal(perm.stopReason, 'permission');
  assert.equal(perm.errorCode, 50013);
  const access = await purge.collectPurgeTargets(makeChannel({ fetchError: { code: 50001 } }).channel, { limit: 10 });
  assert.equal(access.stopReason, 'permission');
  const gone = await purge.collectPurgeTargets(makeChannel({ fetchError: { code: 10003 } }).channel, { limit: 10 });
  assert.equal(gone.stopReason, 'channel-gone');
  const other = await purge.collectPurgeTargets(makeChannel({ fetchError: new Error('boom') }).channel, { limit: 10 });
  assert.equal(other.stopReason, 'error');
});

test('collect: an expired deadline makes no REST call', async () => {
  const { channel, calls } = makeChannel({ messages: makeMessages(10) });
  const r = await purge.collectPurgeTargets(channel, { limit: 10, deadline: Date.now() - 1 });
  assert.equal(r.stopReason, 'deadline');
  assert.equal(calls.fetch.length, 0);
});

// ═════════════════════════════════════════════════════════════════════════════
// deleteInChunks (R096)
// ═════════════════════════════════════════════════════════════════════════════

test('delete: 250 messages go out as 100 + 100 + 50, marked for the log hooks', async () => {
  const msgs = makeMessages(250);
  const { channel, calls } = makeChannel({ messages: msgs });
  const progress = [];
  const r = await purge.deleteInChunks(channel, msgs, { reason: 'test reason', onProgress: (n) => progress.push(n) });
  assert.deepEqual(calls.bulk.map((c) => c.ids.length), [100, 100, 50]);
  assert.ok(calls.bulk.every((c) => c.filterOld === false));
  assert.equal(r.deleted, 250);
  assert.equal(r.stopReason, null);
  assert.deepEqual(progress, [100, 200, 250]);
  for (const m of msgs) assert.equal(wasDeletedByBot(m.id), 'test reason');
});

test('delete: a single message uses the single endpoint', async () => {
  const msgs = makeMessages(1);
  const { channel, calls } = makeChannel({ messages: msgs });
  const r = await purge.deleteInChunks(channel, msgs, { reason: 'one' });
  assert.deepEqual(calls.single, [msgs[0].id]);
  assert.equal(calls.bulk.length, 0);
  assert.equal(r.deleted, 1);
  assert.equal(wasDeletedByBot(msgs[0].id), 'one');
});

test('delete: a rejected bulk call falls back per message and counts exactly', async () => {
  const msgs = makeMessages(5);
  const gone = msgs[2].id;
  const { channel, calls } = makeChannel({
    messages: msgs,
    bulkImpl: () => { throw Object.assign(new Error('too old'), { code: 50034 }); },
    deleteImpl: (mid) => { if (mid === gone) throw Object.assign(new Error('Unknown Message'), { code: 10008 }); },
  });
  const r = await purge.deleteInChunks(channel, msgs, { reason: 'fallback' });
  assert.equal(calls.single.length, 5);
  assert.equal(r.alreadyGone, 1);
  assert.equal(r.deleted, 4);
  assert.equal(r.failed, 0);
  assert.equal(r.stopReason, null);
});

test('delete: losing the permission stops the run without throwing', async () => {
  const msgs = makeMessages(250);
  const { channel, calls } = makeChannel({ messages: msgs, bulkImpl: () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); } });
  const r = await purge.deleteInChunks(channel, msgs, { reason: 'perm' });
  assert.equal(r.stopReason, 'permission');
  assert.equal(r.errorCode, 50013);
  assert.equal(r.deleted, 0);
  assert.equal(calls.bulk.length, 1, 'no further chunk is attempted');
  const gone = await purge.deleteInChunks(makeChannel({ messages: msgs, bulkImpl: () => { throw Object.assign(new Error('x'), { code: 10003 }); } }).channel, msgs, {});
  assert.equal(gone.stopReason, 'channel-gone');
});

test('delete: a failing single delete is never counted (R096)', async () => {
  const msgs = makeMessages(1);
  const { channel } = makeChannel({ messages: msgs, deleteImpl: () => { throw Object.assign(new Error('Missing Access'), { code: 50001 }); } });
  const r = await purge.deleteInChunks(channel, msgs, {});
  assert.equal(r.deleted, 0);
  assert.equal(r.stopReason, 'permission');
  const other = await purge.deleteInChunks(makeChannel({ messages: msgs, deleteImpl: () => { throw new Error('5xx'); } }).channel, msgs, {});
  assert.equal(other.deleted, 0);
  assert.equal(other.failed, 1);
  assert.equal(other.stopReason, null);
});

test('delete: an expired deadline makes no REST call', async () => {
  const msgs = makeMessages(10);
  const { channel, calls } = makeChannel({ messages: msgs });
  const r = await purge.deleteInChunks(channel, msgs, { deadline: Date.now() - 1 });
  assert.equal(r.stopReason, 'deadline');
  assert.equal(calls.bulk.length + calls.single.length, 0);
});

// ═════════════════════════════════════════════════════════════════════════════
// buildPurgeReport
// ═════════════════════════════════════════════════════════════════════════════

test('report: titles and notes', () => {
  const full = purge.buildPurgeReport({ targets: new Array(3).fill({}), scanned: 12, stopReason: 'limit', deleted: 3, opts: { amount: 3 } });
  assert.match(embedText(full), /Purge Complete/);
  assert.match(embedText(full), /\*\*Deleted:\*\* 3 message\(s\)/);
  assert.match(embedText(full), /\*\*Limit:\*\* 3/);

  const partly = purge.buildPurgeReport({ targets: new Array(3).fill({}), scanned: 3, stopReason: 'limit', deleted: 2, failed: 1 });
  assert.match(embedText(partly), /Purge Partly Complete/);
  assert.match(embedText(partly), /1 message\(s\) could not be deleted\./);

  const stopped = purge.buildPurgeReport({ targets: new Array(3).fill({}), scanned: 3, stopReason: 'limit', deleted: 2, deleteStop: 'permission' });
  assert.match(embedText(stopped), /Purge Partly Complete/);
  assert.match(embedText(stopped), /Manage Messages permission/);

  const empty = purge.buildPurgeReport({ targets: [], scanned: 0, stopReason: 'exhausted' });
  assert.match(embedText(empty), /Nothing to Delete/);
  assert.ok(empty.toJSON().description.startsWith('This channel has no messages.'));

  const pinned = purge.buildPurgeReport({ targets: new Array(1).fill({}), scanned: 5, stopReason: 'exhausted', deleted: 1, skippedPinned: 2 });
  assert.match(embedText(pinned), /Kept 2 pinned message\(s\)/);
});

test('report: nothing deleted, every explanation', () => {
  const old = purge.buildPurgeReport({ targets: [], scanned: 0, stopReason: 'age' });
  assert.match(embedText(old), /Every message here is older than 14 days/);
  const tf = purge.buildPurgeReport({ targets: [], scanned: 0, stopReason: 'timeframe', opts: { timeframeMs: 3_600_000 } });
  assert.match(embedText(tf), /No messages were sent in the last 1 hour\./);
  const none = purge.buildPurgeReport({ targets: [], scanned: 250, stopReason: 'scan-cap', opts: { contains: 'a`b' } });
  assert.match(embedText(none), /No messages matched your options among the 250 message\(s\) I checked\./);
  assert.match(embedText(none), /Stopped after checking 1,000 messages/);
  const perm = purge.buildPurgeReport({ targets: [], scanned: 0, stopReason: 'permission', errorCode: 50001 });
  assert.match(embedText(perm), /Purge Failed/);
  assert.match(embedText(perm), /I can't read the message history/);
  const err = purge.buildPurgeReport({ targets: [], scanned: 100, stopReason: 'error', errorCode: 500 });
  assert.match(embedText(err), /Discord returned an error \(code 500\)/);
  const failedAll = purge.buildPurgeReport({ targets: new Array(2).fill({}), scanned: 2, stopReason: 'limit', deleted: 0, failed: 2 });
  assert.match(embedText(failedAll), /Purge Failed/);
  assert.ok(!embedText(purge.buildPurgeReport({ targets: [], scanned: 5, stopReason: 'exhausted', opts: { contains: 'a`b' } })).includes('a`b'));
});

// ═════════════════════════════════════════════════════════════════════════════
// /purge execute
// ═════════════════════════════════════════════════════════════════════════════

test('/purge: a bot without Manage Messages is refused before the deferral (R096)', async () => {
  const { channel, guild, calls } = makeChannel({ id: '5101', messages: makeMessages(5), botPerms: ['ViewChannel', 'ReadMessageHistory'] });
  const i = makeInteraction({ options: { amount: 5 }, channel, guild });
  await purge.execute(i);
  assert.deepEqual(callNames(i), ['reply']);
  assert.equal(i.calls[0][1].ephemeral, true);
  assert.match(lastEmbedText(i), /Missing Permissions/);
  assert.match(lastEmbedText(i), /Manage Messages/);
  assert.equal(calls.fetch.length, 0);
});

test('/purge: option validation replies privately and touches nothing', async () => {
  const { channel, guild, calls } = makeChannel({ id: '5102', messages: makeMessages(5) });
  const run = async (options) => {
    const i = makeInteraction({ options, channel, guild });
    await purge.execute(i);
    assert.deepEqual(callNames(i), ['reply']);
    assert.equal(i.calls[0][1].ephemeral, true);
    return lastEmbedText(i);
  };
  assert.match(await run({}), /Missing Options/);
  assert.match(await run({ timeframe: 'soon' }), /Invalid Timeframe/);
  assert.match(await run({ timeframe: '15d' }), /Timeframe Too Large/);
  assert.equal(calls.fetch.length, 0);
  const archived = makeChannel({ id: '5103', thread: true, archived: true });
  const ia = makeInteraction({ options: { amount: 3 }, channel: archived.channel, guild: archived.guild });
  await purge.execute(ia);
  assert.match(lastEmbedText(ia), /Thread Archived/);
  const i = makeInteraction({ options: { amount: 3 }, channel: { id: '1' }, guild });
  await purge.execute(i);
  assert.match(lastEmbedText(i), /Unsupported Channel/);
});

test('/purge: the bot being timed out is refused with a clear message', async () => {
  const { channel, guild, me } = makeChannel({ id: '5104', messages: makeMessages(3) });
  me.isCommunicationDisabled = () => true;
  const i = makeInteraction({ options: { amount: 3 }, channel, guild });
  await purge.execute(i);
  assert.match(lastEmbedText(i), /I Am Timed Out/);
});

test('/purge: happy path defers privately, deletes, keeps pinned, reports honestly, never auto-deletes', async () => {
  const msgs = makeMessages(30, (i) => ({ pinned: i === 4 }));
  const { channel, guild, calls } = makeChannel({ id: '5105', messages: msgs });
  const i = makeInteraction({ options: { amount: 10 }, channel, guild });
  await purge.execute(i);
  assert.deepEqual(callNames(i), ['deferReply', 'editReply']);
  assert.equal(i.calls[0][1].ephemeral, true);
  const text = lastEmbedText(i);
  assert.match(text, /Purge Complete/);
  assert.match(text, /\*\*Deleted:\*\* 10 message\(s\)/);
  assert.match(text, /Kept 1 pinned message\(s\)/);
  assert.equal(calls.bulk.length, 1);
  assert.ok(!calls.bulk[0].ids.includes(msgs[4].id), 'the pinned message is not deleted');
  assert.equal(calls.bulk[0].ids.length, 10);
  assert.equal(wasDeletedByBot(msgs[0].id), '/purge by Mod#0001 (mod1)');
  await new Promise((r) => setTimeout(r, 50));
  assert.ok(!callNames(i).includes('deleteReply'));
});

test('/purge: pinned:true includes pinned messages', async () => {
  const msgs = makeMessages(5, (i) => ({ pinned: i === 1 }));
  const { channel, guild, calls } = makeChannel({ id: '5106', messages: msgs });
  const i = makeInteraction({ options: { amount: 5, pinned: true }, channel, guild });
  await purge.execute(i);
  assert.equal(calls.bulk[0].ids.length, 5);
});

test('/purge: a failed single delete is reported, not counted as success (R096)', async () => {
  const msgs = makeMessages(1);
  const { channel, guild } = makeChannel({ id: '5107', messages: msgs, deleteImpl: () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); } });
  const i = makeInteraction({ options: { amount: 1 }, channel, guild });
  await purge.execute(i);
  const text = lastEmbedText(i);
  assert.match(text, /Purge Failed/);
  assert.match(text, /\*\*Deleted:\*\* 0 message\(s\)/);
  assert.match(text, /Manage Messages permission/);
});

test('/purge: a fetch error produces a report, never a thrown error (R096)', async () => {
  const { channel, guild } = makeChannel({ id: '5108', fetchError: { code: 50001 } });
  const i = makeInteraction({ options: { timeframe: '1h' }, channel, guild });
  await purge.execute(i);
  assert.match(lastEmbedText(i), /Purge Failed/);
  assert.match(lastEmbedText(i), /I can't read the message history/);
});

test('/purge: an unexpected internal failure ends in the last-resort reply and releases the lock', async () => {
  const { channel, guild } = makeChannel({ id: '5109', messages: makeMessages(3) });
  const broken = makeInteraction({ options: { amount: 3 }, channel, guild, user: null }); // interaction.user.tag throws
  await purge.execute(broken);
  assert.match(lastEmbedText(broken), /Purge Failed/);
  assert.match(lastEmbedText(broken), /Something went wrong/);
  // the lock is released, so a second run on the same channel is accepted
  const again = makeInteraction({ options: { amount: 3 }, channel, guild });
  await purge.execute(again);
  assert.ok(callNames(again).includes('deferReply'));
  assert.match(lastEmbedText(again), /Purge Complete/);
});

test('/purge: a non-Discord error while paging is reported as a stopped run', async () => {
  const { channel, guild } = makeChannel({ id: '5112', messages: makeMessages(3) });
  channel.messages.fetch = async () => { throw new TypeError('not a discord error'); };
  const i = makeInteraction({ options: { amount: 3 }, channel, guild });
  await purge.execute(i);
  assert.match(lastEmbedText(i), /Purge Failed/);
  assert.match(lastEmbedText(i), /Discord returned an error/);
});

test('/purge: only one purge per channel at a time', async () => {
  let release;
  const gate = new Promise((r) => { release = r; });
  const msgs = makeMessages(5);
  const { channel, guild } = makeChannel({ id: '5110', messages: msgs, bulkImpl: () => gate });
  const first = makeInteraction({ options: { amount: 5 }, channel, guild });
  const running = purge.execute(first);
  await new Promise((r) => setTimeout(r, 20));
  const second = makeInteraction({ options: { amount: 5 }, channel, guild });
  await purge.execute(second);
  assert.match(lastEmbedText(second), /Purge In Progress/);
  assert.deepEqual(callNames(second), ['reply']);
  release();
  await running;
  assert.match(lastEmbedText(first), /Purge Complete/);
});

test('/purge: a late progress edit can never overwrite the report', async () => {
  const realNow = Date.now;
  let offset = 0;
  Date.now = () => realNow() + offset;
  try {
    const msgs = makeMessages(250, () => ({}), realNow());
    const { channel, guild } = makeChannel({ id: '5111', messages: msgs, bulkImpl: () => { offset += 6_000; } });
    const i = makeInteraction({ options: { amount: 250 }, channel, guild });
    const done = [];
    const origEdit = i.editReply.bind(i);
    i.editReply = async (p) => {
      const isProgress = /Purging/.test(embedText(p.embeds[0]));
      if (isProgress) await new Promise((r) => setTimeout(r, 60)); // a slow progress edit
      const res = await origEdit(p);
      done.push(isProgress ? 'progress' : 'report');
      return res;
    };
    await purge.execute(i);
    assert.ok(done.includes('progress'), 'a progress edit was sent');
    assert.equal(done[done.length - 1], 'report', `completion order: ${done.join(',')}`);
    assert.match(lastEmbedText(i), /Purge Complete/);
  } finally {
    Date.now = realNow;
  }
});

// ═════════════════════════════════════════════════════════════════════════════
// /slowmode execute
// ═════════════════════════════════════════════════════════════════════════════

function makeTarget({ id = '600', type = ChannelType.GuildText, rateLimitPerUser = 0, thread = false, archived = false, botPerms, execPerms = [], editError = null } = {}) {
  const botGrant = botPerms ?? ['ViewChannel', thread ? 'ManageThreads' : 'ManageChannels'];
  const edits = [];
  const sent = [];
  const target = {
    id, type, rateLimitPerUser, archived, edits, sent,
    isThread: () => thread,
    guild: { id: '900', members: { me: { id: 'bot' } } },
    // the bot is `bot`, the invoking moderator is `exec`
    permissionsFor: (m) => new PermissionsBitField((m?.id === 'bot' ? botGrant : execPerms).map((p) => PermissionFlagsBits[p])),
    async edit(p) {
      edits.push(p);
      if (editError) throw editError;
      return target;
    },
    async send(p) {
      sent.push(p);
      return { id: 'notice' };
    },
  };
  return target;
}

function slowInteraction({ target, options = {}, executorPerms = ['ModerateMembers'], inCurrent = true }) {
  const member = {
    id: 'exec',
    guild: { ownerId: 'someone-else' },
    permissions: new PermissionsBitField(executorPerms.map((p) => PermissionFlagsBits[p])),
    roles: { cache: new Map() },
  };
  const guild = { id: '900', channels: { cache: new Collection([[target.id, target]]), fetch: async () => null } };
  const opts = inCurrent ? options : { ...options, channel: { id: target.id } };
  return makeInteraction({ options: opts, channel: inCurrent ? target : { id: 'other-channel' }, guild, member });
}

test('/slowmode: invalid, too long and too short durations are refused privately', async () => {
  const target = makeTarget();
  const run = async (duration) => {
    const i = slowInteraction({ target, options: { duration } });
    await slowmode.execute(i);
    assert.deepEqual(callNames(i), ['reply']);
    assert.equal(i.calls[0][1].ephemeral, true);
    return lastEmbedText(i);
  };
  assert.match(await run('banana'), /Invalid Duration/);
  assert.match(await run('7h'), /Duration Too Long/);
  assert.match(await run('0.2s'), /Duration Too Short/);
  assert.equal(target.edits.length, 0);
});

test('/slowmode: announcement channels, other types, archived threads, missing channel', async () => {
  const run = async (target, extra = {}) => {
    const i = slowInteraction({ target, options: { duration: '10s' }, ...extra });
    await slowmode.execute(i);
    return lastEmbedText(i);
  };
  assert.match(await run(makeTarget({ type: ChannelType.GuildAnnouncement })), /Announcement channels do not support slowmode/);
  assert.match(await run(makeTarget({ type: ChannelType.GuildCategory })), /Unsupported Channel/);
  assert.match(await run(makeTarget({ type: ChannelType.PublicThread, thread: true, archived: true })), /Thread Archived/);
  const i = makeInteraction({ options: { duration: '10s', channel: { id: 'nope' } }, channel: null, guild: { channels: { cache: new Collection(), fetch: async () => null } }, member: {} });
  await slowmode.execute(i);
  assert.match(lastEmbedText(i), /Channel Not Found/);
});

test('/slowmode: the bot needs Manage Channels, or Manage Threads for a thread (D2)', async () => {
  const text = makeTarget({ botPerms: ['ViewChannel'] });
  const i1 = slowInteraction({ target: text, options: { duration: '10s' } });
  await slowmode.execute(i1);
  assert.match(lastEmbedText(i1), /Manage Channels/);
  assert.equal(text.edits.length, 0);

  const thread = makeTarget({ type: ChannelType.PublicThread, thread: true, botPerms: ['ViewChannel', 'ManageChannels'] });
  const i2 = slowInteraction({ target: thread, options: { duration: '10s' } });
  await slowmode.execute(i2);
  assert.match(lastEmbedText(i2), /Manage Threads/);
  assert.equal(thread.edits.length, 0);

  const ok = makeTarget({ type: ChannelType.PublicThread, thread: true, botPerms: ['ViewChannel', 'ManageThreads'] });
  const i3 = slowInteraction({ target: ok, options: { duration: '10s' } });
  await slowmode.execute(i3);
  assert.equal(ok.edits.length, 1);
});

test('/slowmode: another channel needs the executor to manage it, unless they are an admin', async () => {
  const target = makeTarget({ id: '601' });
  const i = slowInteraction({ target, options: { duration: '10s' }, inCurrent: false });
  await slowmode.execute(i);
  assert.match(lastEmbedText(i), /You need \*\*Manage Channels\*\* in <#601>/);
  assert.equal(target.edits.length, 0);

  const allowed = makeTarget({ id: '602', execPerms: ['ViewChannel', 'ManageChannels'] });
  const i2 = slowInteraction({ target: allowed, options: { duration: '10s' }, inCurrent: false });
  await slowmode.execute(i2);
  assert.equal(allowed.edits.length, 1);

  const admin = makeTarget({ id: '603' });
  const i3 = slowInteraction({ target: admin, options: { duration: '10s' }, inCurrent: false, executorPerms: ['ManageGuild'] });
  await slowmode.execute(i3);
  assert.equal(admin.edits.length, 1, 'ADMIN level bypasses the per-channel check');
});

test('/slowmode: no change is reported privately and edits nothing', async () => {
  const target = makeTarget({ rateLimitPerUser: 30 });
  const i = slowInteraction({ target, options: { duration: '30s' } });
  await slowmode.execute(i);
  assert.deepEqual(callNames(i), ['reply']);
  assert.equal(i.calls[0][1].ephemeral, true);
  assert.match(lastEmbedText(i), /already set to 30 seconds/);
  const off = makeTarget({ rateLimitPerUser: 0 });
  const j = slowInteraction({ target: off, options: { duration: 'off' } });
  await slowmode.execute(j);
  assert.match(lastEmbedText(j), /already off/);
  assert.equal(target.edits.length + off.edits.length, 0);
});

test('/slowmode: applies the change with an audit-log reason and announces it publicly (default PUBLIC)', async () => {
  const target = makeTarget({ id: '604', rateLimitPerUser: 0 });
  const i = slowInteraction({ target, options: { duration: '1h', reason: 'raid' } });
  await slowmode.execute(i);
  assert.equal(target.edits.length, 1);
  assert.equal(target.edits[0].rateLimitPerUser, 3600);
  assert.match(target.edits[0].reason, /^raid \| by Mod#0001 via \/slowmode$/);
  assert.deepEqual(callNames(i), ['deferReply', 'deleteReply', 'followUp']);
  assert.equal(i.calls[0][1].ephemeral, true);
  const follow = i.calls[2][1];
  assert.deepEqual(follow.allowedMentions, { parse: [] });
  const text = embedText(follow.embeds[0]);
  assert.match(text, /Slowmode Enabled/);
  assert.match(text, /\*\*Interval:\*\* 1 hour/);
  assert.match(text, /\*\*Moderator:\*\* <@mod1>/);
});

test('/slowmode: "off" disables and a thread edit works', async () => {
  const target = makeTarget({ type: ChannelType.PublicThread, thread: true, rateLimitPerUser: 60 });
  const i = slowInteraction({ target, options: { duration: 'off' } });
  await slowmode.execute(i);
  assert.equal(target.edits[0].rateLimitPerUser, 0);
  assert.match(embedText(i.calls.at(-1)[1].embeds[0]), /Slowmode Disabled/);
});

test('/slowmode: forum channels get the forum note', async () => {
  const target = makeTarget({ type: ChannelType.GuildForum });
  const i = slowInteraction({ target, options: { duration: '5m' } });
  await slowmode.execute(i);
  assert.match(embedText(i.calls.at(-1)[1].embeds[0]), /creating new posts in this forum/);
});

test('/slowmode: Discord errors become private, specific replies', async () => {
  const cases = [
    [Object.assign(new Error('x'), { code: 50013 }), /Missing Permissions/],
    [Object.assign(new Error('x'), { code: 10003 }), /no longer exists/],
    [Object.assign(new Error('x'), { code: 50083 }), /Thread Archived/],
    [Object.assign(new Error('x'), { code: 50035 }), /Unsupported Channel/],
    [Object.assign(new Error('x'), { code: 12345 }), /code 12345/],
  ];
  for (const [err, pattern] of cases) {
    const target = makeTarget({ editError: err });
    const i = slowInteraction({ target, options: { duration: '10s' } });
    await slowmode.execute(i);
    assert.deepEqual(callNames(i), ['deferReply', 'editReply'], String(err.code));
    assert.match(lastEmbedText(i), pattern);
  }
});

test('/slowmode autocomplete: presets, custom value, too-long hint', async () => {
  const ask = async (focused) => {
    const i = makeInteraction({ options: { focused } });
    await slowmode.autocomplete(i);
    return i.calls.find((c) => c[0] === 'respond')[1];
  };
  const all = await ask('');
  assert.equal(all.length, 14);
  assert.deepEqual(all[0], { name: 'Off', value: 'off' });
  const custom = await ask('45s');
  assert.deepEqual(custom[0], { name: '45 seconds (custom)', value: '45s' });
  const preset = await ask('10s');
  assert.ok(!preset.some((c) => /custom/.test(c.name)), 'a preset value is not offered twice');
  assert.deepEqual(preset.map((c) => c.value), ['10s']);
  const long = await ask('9h');
  assert.equal(long[0].value, '6h');
  assert.match(long[0].name, /Too long/);
  const hours = await ask('hour');
  assert.deepEqual(hours.map((c) => c.value), ['1h', '2h', '6h']);
});

test('/slowmode: PUBLIC, ANONYMOUS and PRIVATE visibility', async () => {
  const guildId = '9550050000000001';
  try {
    const run = async (mode) => {
      await setVisibility(guildId, mode);
      const target = makeTarget({ id: '700' });
      const i = slowInteraction({ target, options: { duration: '10s' } });
      i.guildId = guildId;
      await slowmode.execute(i);
      return i;
    };
    const pub = await run('PUBLIC');
    assert.deepEqual(callNames(pub), ['deferReply', 'deleteReply', 'followUp']);

    const anon = await run('ANONYMOUS');
    // No interaction follow-up: that would show who ran the command. The notice is a channel message.
    assert.deepEqual(callNames(anon), ['deferReply', 'editReply']);
    assert.match(embedText(anon.calls[1][1].embeds[0]), /Moderator/, 'the private confirmation names the moderator');
    const notice = anon.channel.sent.at(-1);
    assert.ok(notice, 'the public notice was posted to the channel');
    assert.ok(!/Moderator/.test(embedText(notice.embeds[0])), 'the public post hides the moderator');

    const priv = await run('PRIVATE');
    assert.deepEqual(callNames(priv), ['deferReply', 'editReply']);
  } finally {
    await invalidateSettings(guildId);
    if (DB) await prisma.guildSettings.deleteMany({ where: { guildId } });
  }
});

// ═════════════════════════════════════════════════════════════════════════════
// /info
// ═════════════════════════════════════════════════════════════════════════════

test('keyPermissionNames', () => {
  assert.deepEqual(info.keyPermissionNames(new PermissionsBitField(['Administrator', 'BanMembers'])), ['Administrator (all permissions)']);
  assert.deepEqual(info.keyPermissionNames(new PermissionsBitField(['BanMembers', 'ManageRoles'])), ['Manage Roles', 'Ban Members']);
  assert.deepEqual(info.keyPermissionNames(new PermissionsBitField(['SendMessages'])), []);
  assert.ok(info.KEY_PERMISSIONS.every((k) => Object.hasOwn(PermissionFlagsBits, k)), 'every key permission exists in discord.js');
});

test('formatRoleMentions', () => {
  const roles = [{ id: '900', position: 0 }, { id: '1', position: 1 }, { id: '3', position: 3 }, { id: '2', position: 2 }];
  assert.equal(info.formatRoleMentions(roles, '900'), '<@&3> <@&2> <@&1>');
  assert.equal(info.formatRoleMentions([{ id: '900', position: 0 }], '900'), 'None');
  assert.equal(info.formatRoleMentions([], '900'), 'None');
  const many = Array.from({ length: 200 }, (_, i) => ({ id: String(100_000_000_000_000_000n + BigInt(i)), position: i + 1 }));
  const out = info.formatRoleMentions(many, '900');
  assert.ok(out.length <= 1024);
  const mentions = out.match(/<@&\d+>/g);
  assert.ok(mentions.length > 30);
  const stripped = out.replace(/<@&\d+>/g, '').replace(/…[^<]*$/, '').replace(/ /g, '');
  assert.equal(stripped, '', 'only whole mentions and the "more" tail remain');
});

test('countChannels', () => {
  const types = [0, 5, 2, 13, 15, 16, 4, 11].map((type) => ({ type }));
  assert.deepEqual(info.countChannels(types), { text: 2, voice: 2, forum: 2, category: 1 });
});

function fakeGuild(extra = {}) {
  const channels = [0, 0, 5, 2, 13, 15, 4, 11].map((type, i) => ({ id: `c${i}`, type }));
  const guild = {
    id: '9001', name: 'My *Server*', description: 'A _cool_ place', ownerId: '42', memberCount: 321,
    createdTimestamp: 1_600_000_000_000, premiumTier: 2, premiumSubscriptionCount: 9, verificationLevel: 2,
    explicitContentFilter: 1, mfaLevel: 1, preferredLocale: 'en-US', vanityURLCode: 'cool',
    features: ['VANITY_URL', 'COMMUNITY', 'ANIMATED_ICON'],
    iconURL: () => 'https://cdn.example/icon.png', bannerURL: () => null,
    channels: { cache: new Collection(channels.map((c) => [c.id, c])) },
    emojis: { cache: new Collection([['e1', { animated: true }], ['e2', { animated: false }]]) },
    roles: { cache: new Collection() },
    client: { rest: { calls: 0, async get() { this.calls++; return { approximate_member_count: 1234, approximate_presence_count: 77, stickers: [{ id: 's1' }] }; } } },
    ...extra,
  };
  guild.roles.cache.set(guild.id, { id: guild.id, position: 0, name: '@everyone' });
  return guild;
}

test('/info server: fields, escaping, cached counts', async () => {
  const guild = fakeGuild();
  const e1 = (await info.buildServerEmbed(guild, null)).toJSON();
  const e2 = (await info.buildServerEmbed(guild, null)).toJSON();
  assert.equal(guild.client.rest.calls, 1, 'the REST counts are cached for 60 s');
  assert.equal(e1.title, 'My *Server*');
  assert.equal(e1.description, 'A \\_cool\\_ place');
  assert.equal(e1.thumbnail.url, 'https://cdn.example/icon.png');
  assert.ok(!e1.image, 'no banner');
  const f = Object.fromEntries(e1.fields.map((x) => [x.name, x.value]));
  assert.equal(f['👑 Owner'], '<@42>');
  assert.equal(f['👥 Members'], '1,234 total\n🟢 77 online');
  assert.equal(f['💬 Channels'], 'Text: 3\nVoice: 2\nForum: 1\nCategories: 1');
  assert.equal(f['🎭 Roles'], '0');
  assert.equal(f['😀 Emojis & stickers'], '2 emojis (1 animated)\n1 stickers');
  assert.equal(f['💎 Boosts'], 'Level 2 · 9 boosts');
  assert.match(f['🛡️ Security'], /Verification: Medium\nContent filter: Members without roles\n2FA for moderators: Required/);
  assert.equal(f['🔗 Vanity URL'], 'discord.gg/cool');
  assert.equal(f['✨ Features'], 'Animated Icon, Community, Vanity Url');
  assert.ok(e1.fields.every((x) => x.value.length <= 1024));
  assert.deepEqual(e1.fields.find((x) => x.name.includes('Features')).inline, false);
  assert.equal(e2.footer.text, 'Server ID: 9001');
});

test('/info server: REST failure falls back to the cached member count', async () => {
  const guild = fakeGuild({ id: '9002' });
  guild.roles.cache.set('9002', { id: '9002' });
  guild.client.rest.get = async () => { throw new Error('rate limited'); };
  const e = (await info.buildServerEmbed(guild, null)).toJSON();
  assert.equal(e.fields.find((x) => x.name === '👥 Members').value, '321 total');
});

function fakeRole(extra = {}) {
  return {
    id: '31', name: 'Admins', position: 7, createdTimestamp: 1_650_000_000_000, hoist: true, mentionable: false,
    managed: false, unicodeEmoji: '🛡️', members: new Collection([['a', {}]]), iconURL: () => null,
    colors: { primaryColor: 0xff0000, secondaryColor: 0x00ff00, tertiaryColor: null },
    permissions: new PermissionsBitField(['KickMembers', 'ManageMessages']), tags: null,
    ...extra,
  };
}

test('/info role: fields, gradient colour, member counts from REST', async () => {
  const guild = fakeGuild({ id: '9003' });
  const role = fakeRole();
  guild.roles.cache.set(role.id, role);
  guild.roles.cache.set('9003', { id: '9003', position: 0 });
  guild.roles.fetchMemberCounts = async () => new Collection([['31', 12]]);
  const e = (await info.buildRoleEmbed(guild, role, null)).toJSON();
  assert.equal(e.title, '🛡️ Admins');
  assert.equal(e.description, '<@&31>');
  assert.equal(e.color, 0xff0000);
  assert.equal(e.footer.text, 'Role ID: 31');
  const f = Object.fromEntries(e.fields.map((x) => [x.name, x.value]));
  assert.equal(f['🎨 Colour'], '#FF0000 → #00FF00');
  assert.equal(f['👥 Members'], '12');
  assert.equal(f['📊 Position'], '7 of 1');
  assert.equal(f['⚙️ Settings'], 'Hoisted: Yes\nMentionable: No\nManaged: No');
  assert.equal(f['🔑 Key permissions'], 'Manage Messages, Kick Members');
  assert.ok(!('🔗 Managed by' in f));
});

test('/info role: managed roles, defaults and the count fallback', async () => {
  const guild = fakeGuild({ id: '9004' });
  guild.roles.cache.set('9004', { id: '9004', position: 0 });
  const bot = fakeRole({ id: '32', managed: true, tags: { botId: '555' }, colors: { primaryColor: 0, secondaryColor: null }, permissions: new PermissionsBitField(0n), unicodeEmoji: null });
  guild.roles.cache.set(bot.id, bot);
  guild.roles.fetchMemberCounts = async () => { throw new Error('boom'); };
  const e = (await info.buildRoleEmbed(guild, bot, null)).toJSON();
  const f = Object.fromEntries(e.fields.map((x) => [x.name, x.value]));
  assert.equal(f['🎨 Colour'], 'Default');
  assert.equal(f['👥 Members'], '1 (cached members only)');
  assert.equal(f['🔗 Managed by'], '<@555> (bot role)');
  assert.equal(f['🔑 Key permissions'], 'None');
  assert.equal(e.title, 'Admins');

  const everyone = (await info.buildRoleEmbed(guild, guild.roles.cache.get('9004'), null));
  assert.equal(everyone.toJSON().description, '@everyone');
});

function fakeMember(guild, extra = {}) {
  const roles = new Collection([[guild.id, { id: guild.id, position: 0 }], ['31', { id: '31', position: 7 }], ['30', { id: '30', position: 2 }]]);
  return {
    id: '111', guild, displayName: '*Ali*', displayColor: 0x00aa00, joinedTimestamp: 1_700_000_000_000,
    premiumSinceTimestamp: null, communicationDisabledUntilTimestamp: null, roles: { cache: roles },
    permissions: new PermissionsBitField(['BanMembers']), displayAvatarURL: () => 'https://cdn.example/m.png',
    ...extra,
  };
}

function fakeUser(extra = {}) {
  return {
    id: '111', username: 'ali_ce', globalName: 'Alice', bot: false, createdTimestamp: 1_500_000_000_000,
    flags: { toArray: () => ['Staff', 'ActiveDeveloper', 'Quarantined'] }, displayAvatarURL: () => 'https://cdn.example/u.png',
    ...extra,
  };
}

function userInteraction({ guild, member, user, invoker, picked = true }) {
  guild.members = { cache: new Collection(member ? [[member.id, member]] : []), fetch: async () => { throw Object.assign(new Error('Unknown Member'), { code: 10007 }); } };
  return makeInteraction({
    options: { sub: 'user', member: picked ? user : null },
    guild,
    member: invoker,
    user: { id: 'invoker', tag: 'Inv#0001', ...(picked ? {} : user) },
  });
}

test('/info user: fields, escaping, badges, access level', async () => {
  const guild = fakeGuild({ id: '9005', ownerId: '999' });
  const user = fakeUser();
  const member = fakeMember(guild);
  const invoker = fakeMember(guild, { id: 'invoker', permissions: new PermissionsBitField(['BanMembers']) });
  const e = (await info.buildUserEmbed(userInteraction({ guild, member, user, invoker }), null)).toJSON();
  assert.equal(e.title, '*Ali*');
  assert.equal(e.color, 0x00aa00);
  assert.equal(e.footer.text, 'User ID: 111');
  const f = Object.fromEntries(e.fields.map((x) => [x.name, x.value]));
  assert.match(f['👤 User'], /\*\*Username:\*\* ali\\_ce/);
  assert.match(f['👤 User'], /\*\*Display name:\*\* \\\*Ali\\\*/);
  assert.match(f['👤 User'], /\*\*Bot:\*\* No/);
  assert.equal(f['📥 Joined server'], '<t:1700000000:F>\n<t:1700000000:R>');
  assert.equal(f['🏅 Badges'], 'Discord Staff, Active Developer');
  assert.equal(f['🎭 Roles [2]'], '<@&31> <@&30>');
  assert.equal(f['🔑 Key permissions'], 'Ban Members');
  assert.equal(f['🛡️ Access level'], 'Moderator');
  assert.ok(!('💎 Boosting since' in f));
  assert.ok(!('🔇 Timed out until' in f));
});

test('/info user: the timeout is shown to moderators only', async () => {
  const guild = fakeGuild({ id: '9006', ownerId: '999' });
  const user = fakeUser();
  const until = Date.now() + 3_600_000;
  const member = fakeMember(guild, { communicationDisabledUntilTimestamp: until, premiumSinceTimestamp: 1_690_000_000_000 });
  const mod = fakeMember(guild, { id: 'invoker', permissions: new PermissionsBitField(['ModerateMembers']) });
  const plain = fakeMember(guild, { id: 'invoker', permissions: new PermissionsBitField(0n) });
  const seenByMod = (await info.buildUserEmbed(userInteraction({ guild, member, user, invoker: mod }), null)).toJSON();
  assert.ok(seenByMod.fields.some((x) => x.name === '🔇 Timed out until'));
  assert.ok(seenByMod.fields.some((x) => x.name === '💎 Boosting since'));
  const seenByMember = (await info.buildUserEmbed(userInteraction({ guild, member, user, invoker: plain }), null)).toJSON();
  assert.ok(!seenByMember.fields.some((x) => x.name === '🔇 Timed out until'));
});

test('/info user: someone who is not in the server, and the default target', async () => {
  const guild = fakeGuild({ id: '9007' });
  const user = fakeUser({ flags: null });
  const e = (await info.buildUserEmbed(userInteraction({ guild, member: null, user, invoker: fakeMember(guild, { id: 'invoker' }) }), null)).toJSON();
  assert.equal(e.title, 'Alice');
  const f = Object.fromEntries(e.fields.map((x) => [x.name, x.value]));
  assert.equal(f['📥 Joined server'], 'Not a member of this server');
  assert.ok(!Object.keys(f).some((k) => k.startsWith('🎭') || k.startsWith('🔑') || k.startsWith('🛡️') || k.startsWith('🏅')));
  // no `member` option: the invoker is inspected
  const self = (await info.buildUserEmbed(userInteraction({ guild, member: null, user, invoker: null, picked: false }), null)).toJSON();
  assert.equal(self.footer.text, 'User ID: 111');
});

test('/info execute: dispatches by subcommand and survives an internal error', async () => {
  const guild = fakeGuild({ id: '9008' });
  guild.roles.cache.set('9008', { id: '9008', position: 0 });
  const role = fakeRole({ id: '41' });
  guild.roles.cache.set(role.id, role);
  guild.roles.fetchMemberCounts = async () => new Collection([['41', 3]]);

  const roleCall = makeInteraction({ options: { sub: 'role', role }, guild, guildId: '9550050000000008' });
  await info.execute(roleCall);
  assert.deepEqual(callNames(roleCall), ['deferReply', 'editReply']);
  assert.equal(roleCall.calls[0][1].ephemeral, false, '/info replies publicly');
  assert.deepEqual(roleCall.calls[1][1].allowedMentions, { parse: [] });
  assert.equal(roleCall.calls[1][1].embeds[0].toJSON().title, '🛡️ Admins');

  const broken = makeInteraction({ options: { sub: 'server' }, guild: { ...guild, name: null, channels: null }, guildId: '9550050000000008' });
  await info.execute(broken);
  assert.match(lastEmbedText(broken), /Info Unavailable/);
});
