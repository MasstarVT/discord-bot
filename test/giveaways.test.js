// pkg-giveaways tests (pkg-giveaways.md §8 + the architect amendments: deny roles, the bulk-delete hook, pingableRoleIds).
// Offline by default: pure helpers (weights, the lazy A-ES draw, packing, embeds, rows, refs, end-time validation, prize
// messages), the member resolver, announcements and payments against fakes, the command payload, the plugin declaration.
// With TEST_DATABASE_URL (a migrated database) the DB cases also run: start / cap / send failure, the Participate toggle
// (including concurrent clicks), end / reroll / cancel / restart / odds, the delete hooks, the end job, list / autocomplete
// queries and the /giveaway subcommands against fake interactions:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/giveaways.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';
const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const { ButtonStyle, ChannelType, Collection } = await import('discord.js');
const svc = await import('../src/services/giveawayService.js');
const cmd = await import('../src/commands/community/giveaway.js');
const pluginMod = await import('../src/plugins/giveaways.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { COLORS, LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { buildCustomId, parseCustomId } = await import('../src/config/customIds.js');
const { registerProvider, hasProvider } = await import('../src/services/rewards.js');
const { getJob, scheduleJob, PermanentJobError } = await import('../src/services/scheduler.js');
const { ensureGuildSettings, updateSettings } = await import('../src/services/settingsService.js');

const {
  END_JOB_TYPE, END_MAX_ATTEMPTS, MAX_ROLE_WEIGHTS, ANNOUNCE_CHUNK, endJobKey, normalizeRoleWeights, memberWeight,
  meetsRequirements, heldDeniedRole, drawWinners, makeResolver, maxWeightFor, packMentions, buildJoinRow, buildGiveawayEmbed,
  parseGiveawayRef, resolveGiveawayRef, validateEndsAt, rewardUnavailableMessage, formatListLine, payPrizes, announceWinners,
  startGiveaway, endGiveaway, rerollGiveaway, cancelGiveaway, restartGiveaway, updateOdds, handleJoinButton,
  handleGiveawayMessageDelete, handleGiveawayBulkDelete, handleEndJob, resetCountRefresh,
} = svc;

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const G = (n) => `95501600000000${String(n).padStart(4, '0')}`; // guild ids unique to this package (port 55016)
const ALL_GUILDS = Array.from({ length: 24 }, (_, i) => G(i + 1));

after(async () => {
  Object.assign(console, quiet);
  resetCountRefresh();
  redis.disconnect();
  if (DB) {
    // The cascade removes Giveaway, GiveawayEntry and ScheduledJob rows.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: ALL_GUILDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

// ── Helpers ──────────────────────────────────────────────────────────────────

/** Seeded PRNG in (0, 1). */
function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6D2B79F5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return (((t ^ (t >>> 14)) >>> 0) + 0.5) / 4294967297;
  };
}

const sid = (prefix, n) => `${prefix}${String(n).padStart(18 - prefix.length, '0')}`; // 18-digit snowflake-like id
const uid = (n) => sid('7', n);
const rid = (n) => sid('8', n);
const HOST = uid(999);
const BOT = sid('6', 1);
let seq = 0;
const nextId = () => sid('5', ++seq);

const resolverOf = (table) => async (ids) => new Map(ids.filter((id) => id in table).map((id) => [id, table[id]]));

// ── Fakes (just enough of discord.js for the service) ────────────────────────

function makeMember(id, roleIds = [], { bot = false } = {}) {
  return { id, user: { id, bot }, partial: false, roles: { cache: new Set(roleIds) } };
}

function makeGuild(id, { roles = [] } = {}) {
  const guild = {
    id,
    available: true,
    roles: { cache: new Map(roles.map((r) => [r.id, r])) },
    channels: {
      cache: new Map(),
      async fetch(cid) {
        throw Object.assign(new Error(`Unknown Channel ${cid}`), { code: 10003 });
      },
    },
    members: {
      cache: new Map(),
      me: { id: BOT },
      fetchable: new Map(),
      fetchCalls: [],
      failFetch: false,
      async fetch(opts) {
        this.fetchCalls.push(opts);
        if (this.failFetch) throw new Error('Members didn\'t arrive in time.');
        const out = new Map();
        for (const uidOne of opts.user) if (this.fetchable.has(uidOne)) out.set(uidOne, this.fetchable.get(uidOne));
        return out;
      },
    },
  };
  return guild;
}

function makeChannel(guild, id, { type = ChannelType.GuildText } = {}) {
  const store = new Map();
  const ch = {
    id,
    name: `chan-${id}`,
    type,
    guild,
    perms: true,
    sent: [],
    edits: [],
    failSend: null,
    store,
    isTextBased: () => true,
    isThread: () => false,
    permissionsFor: () => ({ has: () => ch.perms }),
    toString: () => `<#${id}>`,
    async send(payload) {
      if (ch.failSend) throw ch.failSend;
      const message = makeMessage(ch, nextId());
      ch.sent.push({ ...payload, id: message.id });
      return message;
    },
    messages: {
      async fetch(mid) {
        const m = store.get(mid);
        if (!m) throw Object.assign(new Error('Unknown Message'), { code: 10008 });
        return m;
      },
      async edit(mid, payload) {
        if (!store.has(mid)) throw Object.assign(new Error('Unknown Message'), { code: 10008 });
        ch.edits.push({ id: mid, ...payload });
        return store.get(mid);
      },
    },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function makeMessage(channel, id) {
  const m = {
    id,
    channel,
    url: `https://discord.com/channels/${channel.guild.id}/${channel.id}/${id}`,
    async delete() {
      channel.store.delete(id);
    },
  };
  channel.store.set(id, m);
  return m;
}

async function makeEnv(guildNo, { roles = [], members = [] } = {}) {
  const guildId = G(guildNo);
  await ensureGuildSettings(guildId);
  const guild = makeGuild(guildId, { roles });
  for (const m of members) guild.members.cache.set(m.id, m);
  const channel = makeChannel(guild, nextId());
  const client = { guilds: { cache: new Map([[guildId, guild]]) }, user: { id: BOT } };
  return { guildId, guild, channel, client };
}

/** A giveaway row whose message exists in the fake channel. */
async function makeRow(env, over = {}) {
  const messageId = over.messageId === undefined ? nextId() : over.messageId;
  const g = await prisma.giveaway.create({
    data: {
      guildId: env.guildId,
      channelId: env.channel.id,
      messageId,
      hostId: HOST,
      prize: 'Test prize',
      winnerCount: 1,
      requiredRoleIds: [],
      deniedRoleIds: [],
      roleWeights: [],
      endsAt: new Date(Date.now() + 3_600_000),
      status: 'ACTIVE',
      ...over,
    },
  });
  if (messageId) makeMessage(env.channel, messageId);
  return g;
}

// Explicit, increasing createdAt: one INSERT would give every row the same transaction timestamp.
const addEntries = (giveawayId, userIds) => prisma.giveawayEntry.createMany({
  data: userIds.map((userId, i) => ({ giveawayId, userId, createdAt: new Date(Date.now() - 100_000 + i * 1000) })),
});
const past = () => new Date(Date.now() - 1000);
const reload = (id) => prisma.giveaway.findUnique({ where: { id } });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// A counting reward provider. xp is registered once (the "not installed" assertions run before it); coins never is.
const grants = [];
let failXpFor = new Set();
function ensureXpProvider() {
  if (hasProvider('xp')) return;
  registerProvider('xp', {
    async grant({ guild, userId, amount, reason }) {
      grants.push({ guildId: guild.id, userId, amount, reason });
      return failXpFor.has(userId) ? { ok: false, reason: 'module-disabled' } : { ok: true, total: amount };
    },
    async take() { return { ok: true }; },
  }, 'giveaways-test');
}
const grantsFor = (prize) => grants.filter((x) => x.reason.includes(prize));

/** A fake Participate click. */
function makeClick(env, giveawayId, userId, roleIds = []) {
  const i = {
    customId: `giveaway:join:${giveawayId}`,
    user: { id: userId },
    guildId: env.guildId,
    guild: env.guild,
    member: makeMember(userId, roleIds),
    deferred: false,
    replied: false,
    ephemeral: undefined,
    replies: [],
    createdTimestamp: Date.now(),
    inCachedGuild: () => true,
    async deferReply({ ephemeral }) { i.deferred = true; i.ephemeral = ephemeral; },
    async editReply(p) { i.replies.push(p); return { id: 'r' }; },
    async reply(p) { i.replied = true; i.replies.push(p); return { resource: { message: { id: 'r' } } }; },
  };
  return i;
}
const replyTitle = (i) => i.replies.at(-1)?.embeds?.[0]?.data?.title ?? i.replies.at(-1)?.content ?? '';
const replyText = (i) => JSON.stringify(i.replies.at(-1) ?? {});

// ── 1. normalizeRoleWeights ───────────────────────────────────────────────────

test('1. normalizeRoleWeights', () => {
  for (const bad of [null, undefined, {}, 'x', 5, true]) assert.deepEqual(normalizeRoleWeights(bad), []);
  const id = '1'.repeat(18);
  assert.deepEqual(normalizeRoleWeights([{ roleId: id, weight: 3 }, { roleId: id, weight: 7 }]), [{ roleId: id, weight: 7 }]);
  assert.deepEqual(normalizeRoleWeights([{ roleId: id, weight: 7 }, { roleId: id, weight: 3 }]), [{ roleId: id, weight: 7 }]);
  for (const weight of [0, 11, 1.5, -2, '3', NaN, null]) assert.deepEqual(normalizeRoleWeights([{ roleId: id, weight }]), []);
  assert.deepEqual(normalizeRoleWeights([{ roleId: 'abc', weight: 2 }, { roleId: 123456789012345678, weight: 2 }, null, 'x']), []);
  // Weight 1 is allowed (the stored default), weights 1..10 are kept.
  assert.equal(normalizeRoleWeights([{ roleId: id, weight: 1 }]).length, 1);
  assert.equal(normalizeRoleWeights([{ roleId: id, weight: 10 }]).length, 1);
  // 12 valid distinct roles are cut to 10, sorted by weight descending then roleId.
  const twelve = Array.from({ length: 12 }, (_, i) => ({ roleId: rid(i + 1), weight: 1 + (i % 10) }));
  const out = normalizeRoleWeights(twelve);
  assert.equal(out.length, MAX_ROLE_WEIGHTS);
  for (let i = 1; i < out.length; i++) {
    assert.ok(out[i - 1].weight > out[i].weight || (out[i - 1].weight === out[i].weight && out[i - 1].roleId < out[i].roleId));
  }
});

// ── 2. memberWeight, meetsRequirements, heldDeniedRole ────────────────────────

test('2. memberWeight, meetsRequirements and heldDeniedRole', () => {
  const weights = [{ roleId: 'a', weight: 3 }, { roleId: 'b', weight: 7 }];
  assert.equal(memberWeight(() => false, weights), 1);
  assert.equal(memberWeight((r) => r === 'a', weights), 3);
  assert.equal(memberWeight(() => true, weights), 7, 'the highest matching weight applies');
  assert.equal(memberWeight(() => true, []), 1);
  assert.equal(meetsRequirements(() => true, []), true);
  assert.equal(meetsRequirements(() => false, []), true);
  assert.equal(meetsRequirements((r) => r === 'x', ['x', 'y']), false);
  assert.equal(meetsRequirements(() => true, ['x', 'y']), true);
  assert.equal(heldDeniedRole(() => false, ['x']), null);
  assert.equal(heldDeniedRole((r) => r === 'y', ['x', 'y']), 'y');
  assert.equal(heldDeniedRole(() => true, []), null);
});

// ── 3. drawWinners ────────────────────────────────────────────────────────────

test('3a. basic draw', async () => {
  const ids = Array.from({ length: 10 }, (_, i) => uid(i + 1));
  const res = await drawWinners({ userIds: ids, count: 3, resolve: async (b) => new Map(b.map((id) => [id, { weight: 1 }])), rng: mulberry32(1) });
  assert.equal(res.winnerIds.length, 3);
  assert.equal(new Set(res.winnerIds).size, 3);
  assert.ok(res.winnerIds.every((id) => ids.includes(id)));
  assert.equal(res.complete, true);
  assert.equal(res.unknown, 0);
});

test('3b. a count above the eligible entrants gives exactly the eligible ones', async () => {
  const ids = Array.from({ length: 7 }, (_, i) => uid(i + 1));
  const eligible = new Set([ids[1], ids[4]]);
  const res = await drawWinners({
    userIds: ids, count: 5, rng: mulberry32(2),
    resolve: async (b) => new Map(b.map((id) => [id, eligible.has(id) ? { weight: 1 } : null])),
  });
  assert.deepEqual([...res.winnerIds].sort(), [...eligible].sort());
  assert.equal(res.complete, true);
});

test('3c. unknown entrants are counted and never chosen', async () => {
  const ids = Array.from({ length: 6 }, (_, i) => uid(i + 1));
  const omitted = new Set([ids[0], ids[3]]);
  const res = await drawWinners({
    userIds: ids, count: 10, rng: mulberry32(3),
    resolve: async (b) => new Map(b.filter((id) => !omitted.has(id)).map((id) => [id, { weight: 1 }])),
  });
  assert.equal(res.unknown, 2);
  assert.equal(res.winnerIds.length, 4);
  assert.ok(res.winnerIds.every((id) => !omitted.has(id)));
});

test('3d. the draw is lazy: 1000 entrants, one winner, one resolver call', async () => {
  const ids = Array.from({ length: 1000 }, (_, i) => uid(i + 1));
  let calls = 0;
  const res = await drawWinners({
    userIds: ids, count: 1, maxWeight: 1, batchSize: 100, rng: mulberry32(4),
    resolve: async (b) => { calls++; assert.ok(b.length <= 100); return new Map(b.map((id) => [id, { weight: 1 }])); },
  });
  assert.equal(calls, 1);
  assert.equal(res.winnerIds.length, 1);
});

test('3e. the lazy draw equals the eager A-ES reference (50 seeds, weights 1, 2 and 10)', async () => {
  const ids = Array.from({ length: 30 }, (_, i) => uid(i + 1));
  const weightOf = (i) => [1, 2, 10][i % 3];
  const table = Object.fromEntries(ids.map((id, i) => [id, i % 7 === 0 ? null : { weight: weightOf(i) }]));
  for (let seed = 1; seed <= 50; seed++) {
    const lazy = await drawWinners({ userIds: ids, count: 4, maxWeight: 10, batchSize: 5, rng: mulberry32(seed), resolve: resolverOf(table) });
    const rng = mulberry32(seed);
    const eager = ids
      .map((id) => ({ id, u: rng() }))
      .filter((x) => table[x.id] !== null)
      .map((x) => ({ id: x.id, key: x.u ** (1 / table[x.id].weight) }))
      .sort((a, b) => b.key - a.key)
      .slice(0, 4)
      .map((x) => x.id);
    assert.deepEqual(lazy.winnerIds, eager, `seed ${seed}`);
    assert.equal(lazy.complete, true);
  }
});

test('3f. distribution: weights 1 and 3 give the heavy entrant 0.75 of the wins', async () => {
  const rng = mulberry32(20261010);
  const resolve = async (b) => new Map(b.map((id) => [id, { weight: id === 'heavy' ? 3 : 1 }]));
  let heavy = 0;
  const trials = 20_000;
  for (let t = 0; t < trials; t++) {
    const res = await drawWinners({ userIds: ['light', 'heavy'], count: 1, maxWeight: 3, rng, resolve });
    if (res.winnerIds[0] === 'heavy') heavy++;
  }
  assert.ok(Math.abs(heavy / trials - 0.75) <= 0.02, `heavy won ${heavy / trials}`);
});

test('3g. a passed deadline gives complete: false without resolving', async () => {
  let calls = 0;
  const res = await drawWinners({
    userIds: ['a', 'b', 'c'], count: 1, deadline: 5, now: () => 10, rng: mulberry32(5),
    resolve: async () => { calls++; return new Map(); },
  });
  assert.equal(res.complete, false);
  assert.equal(calls, 0);
  assert.deepEqual(res.winnerIds, []);
});

test('3h. duplicate ids are drawn at most once; an empty input or count 0 gives nothing', async () => {
  const resolve = async (b) => new Map(b.map((id) => [id, { weight: 1 }]));
  const res = await drawWinners({ userIds: ['a', 'a', 'a', 'b', 'b'], count: 5, rng: mulberry32(6), resolve });
  assert.deepEqual([...res.winnerIds].sort(), ['a', 'b']);
  assert.deepEqual(await drawWinners({ userIds: [], count: 3, resolve }), { winnerIds: [], complete: true, unknown: 0 });
  assert.deepEqual(await drawWinners({ userIds: ['a'], count: 0, resolve }), { winnerIds: [], complete: true, unknown: 0 });
});

// ── makeResolver / maxWeightFor ───────────────────────────────────────────────

test('makeResolver: required, denied and bonus roles, bots, members who left, deleted roles, failed lookups', async () => {
  const REQ = rid(1);
  const DEN = rid(2);
  const BONUS = rid(3);
  const GONE = rid(4); // deleted: not in guild.roles.cache
  const guild = makeGuild(G(1), { roles: [{ id: REQ }, { id: DEN }, { id: BONUS }] });
  const cached = [makeMember(uid(1), [REQ]), makeMember(uid(2), [REQ, BONUS]), makeMember(uid(3), []), makeMember(uid(4), [REQ, DEN]), makeMember(uid(5), [REQ], { bot: true })];
  for (const m of cached) guild.members.cache.set(m.id, m);
  guild.members.fetchable.set(uid(6), makeMember(uid(6), [REQ]));
  const g = {
    requiredRoleIds: [REQ, GONE], deniedRoleIds: [DEN], roleWeights: [{ roleId: BONUS, weight: 5 }, { roleId: GONE, weight: 9 }],
  };
  assert.equal(maxWeightFor(g, guild), 5, 'a deleted bonus role is ignored');
  const out = await makeResolver(guild, g)([uid(1), uid(2), uid(3), uid(4), uid(5), uid(6), uid(7)]);
  assert.deepEqual(out.get(uid(1)), { weight: 1 });
  assert.deepEqual(out.get(uid(2)), { weight: 5 });
  assert.equal(out.get(uid(3)), null, 'lacks the required role');
  assert.equal(out.get(uid(4)), null, 'holds a denied role');
  assert.equal(out.get(uid(5)), null, 'bot');
  assert.deepEqual(out.get(uid(6)), { weight: 1 }, 'fetched through the gateway');
  assert.equal(out.get(uid(7)), null, 'missing from a successful fetch: left the server');
  assert.equal(guild.members.fetchCalls.length, 1, 'one chunk request for all misses');
  assert.deepEqual(guild.members.fetchCalls[0].user, [uid(6), uid(7)]);

  // A deleted required or denied role no longer restricts anyone.
  const lax = await makeResolver(guild, { requiredRoleIds: [GONE], deniedRoleIds: [GONE], roleWeights: [] })([uid(3)]);
  assert.deepEqual(lax.get(uid(3)), { weight: 1 });

  // A failed lookup leaves the ids unset (unknown), never "left".
  guild.members.failFetch = true;
  const failed = await makeResolver(guild, g)([uid(6), uid(3)]);
  assert.equal(failed.has(uid(6)), false);
  assert.equal(failed.get(uid(3)), null);
});

// ── 4. packMentions ───────────────────────────────────────────────────────────

test('4. packMentions', () => {
  const ids = Array.from({ length: 90 }, (_, i) => `1234567890123456${String(i).padStart(3, '0')}`);
  assert.ok(ids.every((id) => id.length === 19));
  const chunks = packMentions(ids, ANNOUNCE_CHUNK);
  assert.ok(chunks.length >= 2);
  for (const c of chunks) {
    assert.ok(c.length <= 1700);
    assert.equal(c.split('<@').length - 1, c.split('>').length - 1, 'no partial mention');
    assert.match(c, /^<@\d+>(, <@\d+>)*$/);
  }
  const back = chunks.flatMap((c) => [...c.matchAll(/<@(\d+)>/g)].map((m) => m[1]));
  assert.deepEqual(back, ids);
  assert.deepEqual(packMentions([]), []);
  assert.deepEqual(packMentions(['1'.repeat(18)], 5), ['<@111111111111111111>'], 'an over-long single mention is still not split');
});

// ── 5. buildJoinRow ───────────────────────────────────────────────────────────

test('5. buildJoinRow', () => {
  const id = 'c' + 'a'.repeat(24);
  const row = buildJoinRow(id, 1234);
  const b = row.toJSON().components[0];
  assert.equal(b.custom_id, `giveaway:join:${id}`);
  assert.ok(b.custom_id.length <= 100 && b.custom_id.length <= 39);
  assert.equal(b.label, 'Participate (1,234)');
  assert.equal(b.emoji.name, '🎉');
  assert.equal(b.style, ButtonStyle.Primary);
  assert.ok(!b.disabled);
  assert.equal(buildJoinRow(id, 0, { disabled: true }).toJSON().components[0].disabled, true);
  assert.equal(row.toJSON().components.length, 1);
});

// ── 6. buildGiveawayEmbed ─────────────────────────────────────────────────────

const baseRow = (over = {}) => ({
  id: 'c' + 'b'.repeat(24), prize: 'Prize', description: null, hostId: uid(1), winnerCount: 2, requiredRoleIds: [], deniedRoleIds: [],
  roleWeights: [], xpPrize: null, coinPrize: null, embedColor: null, endsAt: new Date('2026-12-24T18:00:00Z'), endedAt: null,
  status: 'ACTIVE', winnerIds: [], ...over,
});

test('6. buildGiveawayEmbed', () => {
  const long = buildGiveawayEmbed(baseRow({ prize: 'P'.repeat(256) })).toJSON();
  assert.ok(long.title.length <= 256);
  const weights = Array.from({ length: 10 }, (_, i) => ({ roleId: rid(i + 1), weight: 2 + (i % 9) }));
  const big = buildGiveawayEmbed(baseRow({
    description: 'd'.repeat(1000), roleWeights: weights, requiredRoleIds: [rid(20)], deniedRoleIds: [rid(21)], xpPrize: 5000, coinPrize: 70,
  })).toJSON();
  assert.ok(big.description.length <= 4096);
  assert.match(big.description, /\*\*Required role:\*\* <@&8\d+>/);
  assert.match(big.description, /\*\*Not for:\*\* <@&8\d+>/);
  assert.match(big.description, /\*\*Bonus odds:\*\*/);
  assert.match(big.description, /\*\*Bonus prize:\*\* `5,000 XP` \+ `🪙 70 coins` each/);
  assert.ok(big.footer.text.includes(`ID: ${baseRow().id}`));
  assert.equal(big.timestamp, '2026-12-24T18:00:00.000Z');

  // Roles that no longer exist are left out of the role lines.
  const hidden = buildGiveawayEmbed(baseRow({ requiredRoleIds: [rid(20)], deniedRoleIds: [rid(21)], roleWeights: weights }), { roleExists: () => false }).toJSON();
  assert.ok(!/Required role|Not for|Bonus odds/.test(hidden.description));

  const none = buildGiveawayEmbed(baseRow({ status: 'ENDED', winnerIds: [], endedAt: new Date('2026-12-24T18:00:00Z') }), { entryCount: 0 }).toJSON();
  assert.match(none.description, /No valid entrants\./);
  assert.match(none.footer.text, /Ended at/);

  const ninety = Array.from({ length: 90 }, (_, i) => `1234567890123456${String(i).padStart(3, '0')}`);
  const ended = buildGiveawayEmbed(baseRow({ status: 'ENDED', winnerIds: ninety, endedAt: new Date() }), { entryCount: 1234 }).toJSON();
  const line = ended.description.split('\n').find((l) => l.startsWith('**Winners:**'));
  assert.ok(line.slice('**Winners:** '.length).length <= 1024);
  assert.match(ended.description, /\*\*Entries:\*\* 1,234/);

  // embedColor applies on ACTIVE but not on ENDED / CANCELLED (NEUTRAL).
  assert.equal(buildGiveawayEmbed(baseRow({ embedColor: 0x123456 })).toJSON().color, 0x123456);
  assert.equal(buildGiveawayEmbed(baseRow({ embedColor: 0x123456, status: 'ENDED', endedAt: new Date() })).toJSON().color, COLORS.NEUTRAL);
  const cancelled = buildGiveawayEmbed(baseRow({ embedColor: 0x123456, status: 'CANCELLED', endedAt: new Date() })).toJSON();
  assert.equal(cancelled.color, COLORS.NEUTRAL);
  assert.match(cancelled.description, /was cancelled/);
  assert.match(cancelled.title, /^🚫/);
  // DRAFT renders as ACTIVE.
  assert.match(buildGiveawayEmbed(baseRow({ status: 'DRAFT' })).toJSON().description, /Participate/);
  // Prize text is rendered literally (no placeholders).
  assert.match(buildGiveawayEmbed(baseRow({ prize: '{user} {server}' })).toJSON().title, /\{user\} \{server\}/);
});

// ── 7. parseGiveawayRef ───────────────────────────────────────────────────────

test('7. parseGiveawayRef', () => {
  const g = '1'.repeat(18); const c = '2'.repeat(18); const m = '3'.repeat(19);
  assert.deepEqual(parseGiveawayRef(`https://discord.com/channels/${g}/${c}/${m}`), { kind: 'message', value: m, guildId: g });
  assert.deepEqual(parseGiveawayRef(`https://canary.discord.com/channels/${g}/${c}/${m}/`), { kind: 'message', value: m, guildId: g });
  assert.deepEqual(parseGiveawayRef(`  https://ptb.discordapp.com/channels/${g}/${c}/${m}  `), { kind: 'message', value: m, guildId: g });
  assert.deepEqual(parseGiveawayRef(m), { kind: 'message', value: m });
  assert.deepEqual(parseGiveawayRef('c' + 'a'.repeat(24)), { kind: 'id', value: 'c' + 'a'.repeat(24) });
  for (const bad of ['hello', '', null, undefined, '123', `https://discord.com/channels/@me/${c}/${m}`, `https://evil.example/channels/${g}/${c}/${m}`]) {
    assert.equal(parseGiveawayRef(bad), null);
  }
});

// ── 8. validateEndsAt ─────────────────────────────────────────────────────────

test('8. validateEndsAt', () => {
  const now = new Date('2026-10-10T12:00:00Z');
  const at = (ms) => new Date(now.getTime() + ms);
  const bad = validateEndsAt(new Date('nope'), now, 'Europe/Paris');
  assert.deepEqual([bad.ok, bad.title], [false, 'Invalid Duration']);
  assert.match(bad.message, /Europe\/Paris/);
  assert.equal(validateEndsAt(null, now).ok, false);
  const past = validateEndsAt(at(-1000), now, 'Asia/Tokyo');
  assert.deepEqual([past.ok, past.title], [false, 'Invalid Duration']);
  assert.match(past.message, /in the past \(server timezone: Asia\/Tokyo\)/);
  assert.equal(validateEndsAt(at(30_000), now).title, 'Too Short');
  assert.equal(validateEndsAt(at(91 * 86_400_000), now).title, 'Too Long');
  assert.equal(validateEndsAt(at(3_600_000), now).ok, true);
  assert.equal(validateEndsAt(at(60_000), now).ok, true, 'exactly one minute is allowed');
  assert.equal(validateEndsAt(at(90 * 86_400_000), now).ok, true, 'exactly 90 days is allowed');
});

// ── 9. rewardUnavailableMessage (must run before any provider is registered) ──

test('9. rewardUnavailableMessage: not installed, module off, available', () => {
  assert.equal(hasProvider('xp'), false, 'this file registers the xp provider only after this check');
  assert.match(rewardUnavailableMessage('xp', { xpEnabled: true }), /XP prizes are not available on this bot \(the ⭐ Levels module is not installed\)\./);
  assert.match(rewardUnavailableMessage('coins', { economyEnabled: true }), /Coin prizes are not available on this bot \(the 🪙 Economy module is not installed\)\./);
  ensureXpProvider();
  assert.equal(rewardUnavailableMessage('xp', { xpEnabled: false }), 'XP prizes need the **⭐ Levels** module. Enable it with `/module enable leveling`.');
  assert.equal(rewardUnavailableMessage('xp', { xpEnabled: true }), null);
  assert.equal(rewardUnavailableMessage('xp', null), 'XP prizes need the **⭐ Levels** module. Enable it with `/module enable leveling`.');
  // Coins stay unregistered for the whole file: the "refuse coin prizes cleanly" path.
  assert.match(rewardUnavailableMessage('coins', { economyEnabled: true }), /not installed/);
});

test('formatListLine', () => {
  const t = new Date('2026-12-24T18:00:00Z');
  const row = { id: 'cid', prize: 'x'.repeat(100), status: 'ACTIVE', channelId: '123', endsAt: t, endedAt: null, _count: { entries: 1 } };
  assert.match(formatListLine(row), /^🟢 \*\*x{79}…\*\* · <#123> · ends <t:\d+:R> · 1 entry\n`cid`$/);
  assert.match(formatListLine({ ...row, status: 'DRAFT', _count: { entries: 1234 } }), /📝.* · draft · 1,234 entries\n/);
  assert.match(formatListLine({ ...row, status: 'ENDED', endedAt: t }), /🏁.* · ended <t:\d+:R> ·/);
  assert.match(formatListLine({ ...row, status: 'CANCELLED', _count: { entries: 0 } }), /🚫.* · cancelled · 0 entries/);
});

// ── announceWinners / payPrizes against fakes ─────────────────────────────────

test('announceWinners: pings only the winners, chunks 90 winners, replies, no-winner text, reroll text, never throws', async () => {
  const guild = makeGuild(G(1));
  const ch = makeChannel(guild, nextId());
  const ids = Array.from({ length: 90 }, (_, i) => `1234567890123456${String(i).padStart(3, '0')}`);
  const g = baseRow({ messageId: '555555555555555555', xpPrize: 10, coinPrize: 5 });
  assert.equal(await announceWinners(ch, g, ids, { settings: null, reroll: false }), true);
  assert.ok(ch.sent.length >= 2);
  for (const m of ch.sent) {
    assert.ok(m.content.length <= 2000);
    assert.deepEqual(m.allowedMentions.parse, []);
    assert.deepEqual(m.allowedMentions.users, [...m.content.matchAll(/<@(\d+)>/g)].map((x) => x[1]));
    assert.ok(m.allowedMentions.users.length <= 100);
  }
  assert.deepEqual(ch.sent.flatMap((m) => m.allowedMentions.users), ids);
  assert.deepEqual(ch.sent[0].reply, { messageReference: '555555555555555555', failIfNotExists: false });
  assert.equal(ch.sent[1].reply, undefined);
  assert.match(ch.sent[0].content, /^🎉 Congratulations .+! You won \*\*Prize\*\*!\nEach winner also gets \*\*10 XP\*\* and \*\*🪙 5 coins\*\*\.$/s);

  const ch2 = makeChannel(guild, nextId());
  await announceWinners(ch2, baseRow(), [ids[0]], { settings: { currencyName: 'gems', currencyEmoji: '💎' }, reroll: true });
  assert.match(ch2.sent[0].content, /^🎉 New winner: <@\d+>! Congratulations, you won \*\*Prize\*\*!$/);
  await announceWinners(ch2, baseRow({ coinPrize: 3 }), ids.slice(0, 2), { settings: { currencyName: 'gems', currencyEmoji: '💎' }, reroll: true });
  assert.match(ch2.sent[1].content, /New winners: .*\nEach winner also gets \*\*💎 3 gems\*\*\./s);

  const ch3 = makeChannel(guild, nextId());
  await announceWinners(ch3, baseRow(), [], {});
  assert.equal(ch3.sent[0].content, 'No valid entrants for **Prize**, so no winner could be chosen.');
  assert.deepEqual(ch3.sent[0].allowedMentions, { parse: [] });

  ch3.failSend = Object.assign(new Error('Missing Access'), { code: 50001 });
  assert.equal(await announceWinners(ch3, baseRow(), [ids[0]], {}), false);
});

test('payPrizes: pays through the rewards bus, warns per kind with the reason, pays nobody twice', async () => {
  ensureXpProvider();
  const guild = makeGuild(G(1));
  guild.members.cache.set(uid(1), makeMember(uid(1)));
  const g = baseRow({ prize: 'PayTest-A', xpPrize: 40, coinPrize: 7 });
  failXpFor = new Set([uid(2)]);
  const res = await payPrizes({ guild, giveaway: g, winnerIds: [uid(1), uid(2), uid(3)], channel: null });
  failXpFor = new Set();
  assert.deepEqual(grantsFor('PayTest-A').map((x) => x.userId).sort(), [uid(1), uid(2), uid(3)]);
  assert.ok(grantsFor('PayTest-A').every((x) => x.amount === 40 && x.guildId === guild.id));
  assert.equal(res.results.xp.ok, 2);
  assert.equal(res.results.xp.failed, 1);
  assert.equal(res.results.coins.failed, 3);
  assert.equal(res.warnings.length, 2);
  assert.match(res.warnings[0], /^⚠️ The XP prize could not be paid to 1 winner: the ⭐ Levels module is turned off\. Please pay it manually\.$/);
  assert.match(res.warnings[1], /^⚠️ The coin prize could not be paid to 3 winners: the 🪙 Economy module is not installed\. Please pay it manually\.$/);
  const none = await payPrizes({ guild, giveaway: baseRow(), winnerIds: [uid(1)], channel: null });
  assert.deepEqual(none.warnings, []);
});

// ── 10. Command payload ───────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('10. /giveaway payload and exports', () => {
  const json = buildCommandPayload(cmd);
  assert.deepEqual(json.contexts, [0]);
  assert.ok(charCount(json) <= 2000, `payload is ${charCount(json)} characters`);
  assert.deepEqual(json.options.map((o) => o.name), ['start', 'end', 'reroll', 'cancel', 'restart', 'list', 'entrants', 'odds']);
  const start = json.options[0];
  assert.equal(start.options.length, 13);
  assert.deepEqual(start.options.slice(0, 2).map((o) => [o.name, o.required]), [['prize', true], ['duration', true]]);
  assert.ok(start.options.slice(2).every((o) => !o.required));
  assert.deepEqual(start.options.map((o) => o.name), [
    'prize', 'duration', 'winners', 'channel', 'required_role', 'denied_role', 'bonus_role', 'bonus_weight', 'xp', 'coins', 'ping_role', 'description', 'color',
  ]);
  assert.equal(start.options.find((o) => o.name === 'winners').max_value, LIMITS.GIVEAWAY_WINNERS);
  assert.equal(start.options.find((o) => o.name === 'denied_role').description, 'Members with this role cannot enter');
  const giveawayOptions = [];
  const walk = (n) => { for (const o of n.options ?? []) { if (o.name === 'giveaway') giveawayOptions.push(o); walk(o); } };
  walk(json);
  assert.equal(giveawayOptions.length, 7);
  assert.ok(giveawayOptions.every((o) => o.autocomplete === true && o.required === true));
  assert.deepEqual(json.options.find((o) => o.name === 'odds').options.map((o) => o.name), ['add', 'remove']);
  assert.equal(cmd.module, 'giveaways');
  assert.equal(cmd.gated, true);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.MODERATOR);
  assert.equal(cmd.allowInDMs, undefined);
  assert.equal(typeof cmd.execute, 'function');
  assert.equal(typeof cmd.autocomplete, 'function');
  assert.equal(json.default_member_permissions, String(1n << 40n), 'Moderate Members');
});

// ── 11. Plugin ────────────────────────────────────────────────────────────────

test('11. plugin declaration: hooks, button, job', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('giveaways')), []);
  const p = plugins.find((x) => x.id === 'giveaways');
  assert.ok(p, 'the giveaways plugin is collected');
  assert.equal(p.module, 'giveaways');
  assert.deepEqual(p.hooks.map((h) => `${h.event}:${h.id}:${h.always}`), ['messageDelete:giveaways:true', 'messageDeleteBulk:giveaways:true']);
  assert.deepEqual(p.components.map((c) => [c.type, c.prefix]), [['button', 'giveaway']]);
  assert.deepEqual(p.jobs.map((j) => j.type), ['giveaway.end']);
  assert.deepEqual(pluginMod.plugin.provides, {});
  const filter = pluginMod.plugin.hooks[0].filter;
  const client = { user: { id: 'B' } };
  assert.equal(filter({ message: { author: null }, client }), true);
  assert.equal(filter({ message: { author: { id: 'B' } }, client }), true);
  assert.equal(filter({ message: { author: { id: 'U' } }, client }), false);
  assert.equal(pluginMod.plugin.hooks[1].filter, undefined);
});

// ── 12. customIds ─────────────────────────────────────────────────────────────

test('12. customIds round-trip', () => {
  const id = 'c' + 'a'.repeat(24);
  const custom = buildCustomId('giveaway', 'join', id);
  assert.equal(custom, `giveaway:join:${id}`);
  assert.deepEqual(parseCustomId(custom), { prefix: 'giveaway', params: ['join', id] });
  assert.equal(END_JOB_TYPE, 'giveaway.end');
  assert.equal(endJobKey(id), `giveaway.end:${id}`);
  assert.equal(END_MAX_ATTEMPTS, 5);
});

test('static checks (§9): no console, setInterval, cache.del, parsed @everyone, array role writes, foreign services', () => {
  const files = ['src/commands/community/giveaway.js', 'src/services/giveawayService.js', 'src/plugins/giveaways.js'];
  const bad = [/console\./, /setInterval/, /cache\.del\(/, /parse: \[[^\]]*everyone/, /roles\.(add|set)\(\[/, /from '\.\.\/services\/(xpService|economyService)/, /from '\.\.\/\.\.\/services\/(xpService|economyService)/];
  for (const f of files) {
    const text = readFileSync(resolve(ROOT, f), 'utf8');
    for (const re of bad) assert.ok(!re.test(text), `${f} matches ${re}`);
  }
});

// ── DB: start ─────────────────────────────────────────────────────────────────

test('DB start: creates the row, posts the message, schedules the end job', { skip: skipDb }, async () => {
  const env = await makeEnv(1, { roles: [{ id: rid(1), mentionable: true }, { id: rid(2), mentionable: false }] });
  const endsAt = new Date(Date.now() + 2 * 3_600_000);
  const res = await startGiveaway({
    client: env.client, guild: env.guild, channel: env.channel, hostId: HOST, prize: 'Start prize', description: 'desc', winnerCount: 3, endsAt,
    requiredRoleIds: [rid(1)], deniedRoleIds: [rid(2)], roleWeights: [{ roleId: rid(1), weight: 4 }], xpPrize: 25, coinPrize: null,
    pingRoleId: rid(1), embedColor: 0xabcdef, settings: null,
  });
  assert.equal(res.ok, true);
  const row = await reload(res.giveaway.id);
  assert.equal(row.status, 'ACTIVE');
  assert.equal(row.messageId, res.message.id);
  assert.deepEqual([row.requiredRoleIds, row.deniedRoleIds, row.roleWeights], [[rid(1)], [rid(2)], [{ roleId: rid(1), weight: 4 }]]);
  assert.equal(row.xpPrize, 25);
  assert.equal(row.coinPrize, null);
  assert.equal(row.embedColor, 0xabcdef);
  assert.equal(row.endsAt.getTime(), endsAt.getTime());
  const job = await getJob(endJobKey(row.id));
  assert.equal(job.type, 'giveaway.end');
  assert.deepEqual(job.payload, { giveawayId: row.id });
  assert.equal(job.runAt.getTime(), endsAt.getTime());
  assert.equal(job.maxAttempts, END_MAX_ATTEMPTS);
  const sent = env.channel.sent[0];
  assert.equal(sent.content, `<@&${rid(1)}>`);
  assert.deepEqual(sent.allowedMentions, { parse: [], roles: [rid(1)] });
  assert.equal(sent.components[0].toJSON().components[0].custom_id, `giveaway:join:${row.id}`);
  assert.match(sent.embeds[0].toJSON().description, /\*\*Not for:\*\*/);

  // A ping role that no longer exists is skipped silently at send time.
  const res2 = await startGiveaway({
    client: env.client, guild: env.guild, channel: env.channel, hostId: HOST, prize: 'No ping', winnerCount: 1, endsAt, pingRoleId: rid(99), settings: null,
  });
  assert.equal(res2.ok, true);
  assert.equal(env.channel.sent.at(-1).content, undefined, 'a deleted ping role is skipped silently');
  assert.deepEqual(env.channel.sent.at(-1).allowedMentions, { parse: [], roles: [] });
});

test('DB start: the cap counts ACTIVE and DRAFT only; a send failure leaves nothing behind', { skip: skipDb }, async () => {
  const env = await makeEnv(2);
  const base = { guildId: env.guildId, channelId: env.channel.id, hostId: HOST, prize: 'cap', winnerCount: 1, endsAt: new Date(Date.now() + 3_600_000), roleWeights: [] };
  await prisma.giveaway.createMany({ data: [
    ...Array.from({ length: 98 }, () => ({ ...base, status: 'ACTIVE' })),
    { ...base, status: 'DRAFT' },
    ...Array.from({ length: 10 }, () => ({ ...base, status: 'ENDED' })),
    ...Array.from({ length: 10 }, () => ({ ...base, status: 'CANCELLED' })),
  ] });
  const args = { client: env.client, guild: env.guild, channel: env.channel, hostId: HOST, prize: 'cap-new', winnerCount: 1, endsAt: new Date(Date.now() + 3_600_000), settings: null };
  assert.equal((await startGiveaway(args)).ok, true, '99 open + history is under the cap of 100');
  const res = await startGiveaway(args);
  assert.deepEqual([res.ok, res.code], [false, 'LIMIT']);
  assert.equal(LIMITS.ACTIVE_GIVEAWAYS, 100);

  const env2 = await makeEnv(3);
  env2.channel.failSend = Object.assign(new Error('Missing Permissions'), { code: 50013 });
  const failed = await startGiveaway({ ...args, client: env2.client, guild: env2.guild, channel: env2.channel, prize: 'send-fails' });
  assert.deepEqual([failed.ok, failed.code], [false, 'SEND_FAILED']);
  assert.equal(await prisma.giveaway.count({ where: { guildId: env2.guildId } }), 0);
  assert.equal(await prisma.scheduledJob.count({ where: { guildId: env2.guildId } }), 0);
});

// ── DB: the Participate button ────────────────────────────────────────────────

test('DB button: enter, leave, requirements, denied roles, closed giveaways', { skip: skipDb }, async () => {
  const REQ = rid(11);
  const DEN = rid(12);
  const env = await makeEnv(4, { roles: [{ id: REQ }, { id: DEN }] });
  const g = await makeRow(env, { requiredRoleIds: [REQ], deniedRoleIds: [DEN], prize: 'Button prize' });
  const count = () => prisma.giveawayEntry.count({ where: { giveawayId: g.id } });

  let c = makeClick(env, g.id, uid(1), [REQ]);
  await handleJoinButton(c, env.client);
  assert.equal(c.ephemeral, true, 'the reply is ephemeral');
  assert.match(replyTitle(c), /Entered/);
  assert.equal(await count(), 1);

  c = makeClick(env, g.id, uid(1), [REQ]);
  await handleJoinButton(c, env.client);
  assert.match(replyTitle(c), /Left Giveaway/);
  assert.equal(await count(), 0);

  c = makeClick(env, g.id, uid(2), []);
  await handleJoinButton(c, env.client);
  assert.match(replyTitle(c), /Requirement Not Met/);
  assert.match(replyText(c), new RegExp(`<@&${REQ}>`));
  assert.equal(await count(), 0);

  c = makeClick(env, g.id, uid(3), [REQ, DEN]);
  await handleJoinButton(c, env.client);
  assert.match(replyTitle(c), /Requirement Not Met/);
  assert.match(replyText(c), new RegExp(`You can't enter this giveaway with the <@&${DEN}> role`));
  assert.equal(await count(), 0);

  // Leaving is always allowed, even after losing the required role or gaining a denied one.
  await addEntries(g.id, [uid(4)]);
  c = makeClick(env, g.id, uid(4), [DEN]);
  await handleJoinButton(c, env.client);
  assert.match(replyTitle(c), /Left Giveaway/);

  // A deleted required / denied role no longer restricts.
  env.guild.roles.cache.delete(REQ);
  env.guild.roles.cache.delete(DEN);
  c = makeClick(env, g.id, uid(5), [DEN]);
  await handleJoinButton(c, env.client);
  assert.match(replyTitle(c), /Entered/);

  // Draft, ended, past-due, other guild, unknown id, malformed id.
  await prisma.giveaway.update({ where: { id: g.id }, data: { status: 'DRAFT', messageId: null } });
  c = makeClick(env, g.id, uid(6));
  await handleJoinButton(c, env.client);
  assert.match(replyTitle(c), /Giveaway Paused/);
  await prisma.giveaway.update({ where: { id: g.id }, data: { status: 'ENDED' } });
  c = makeClick(env, g.id, uid(6));
  await handleJoinButton(c, env.client);
  assert.match(replyTitle(c), /Giveaway Closed/);
  await prisma.giveaway.update({ where: { id: g.id }, data: { status: 'ACTIVE', endsAt: new Date(Date.now() - 1000) } });
  c = makeClick(env, g.id, uid(6));
  await handleJoinButton(c, env.client);
  assert.match(replyTitle(c), /Giveaway Closed/);
  c = makeClick(env, 'c' + 'z'.repeat(24), uid(6));
  await handleJoinButton(c, env.client);
  assert.match(replyTitle(c), /Giveaway Not Found/);
  const other = await makeEnv(5);
  c = makeClick(other, g.id, uid(6));
  await handleJoinButton(c, other.client);
  assert.match(replyTitle(c), /Giveaway Not Found/, 'an id from another guild is simply not found');
  c = makeClick(env, g.id, uid(6));
  c.customId = 'giveaway:nope';
  await handleJoinButton(c, env.client);
  assert.match(replyText(c), /no longer active/);
  resetCountRefresh();
});

test('DB button: concurrent clicks never create a second entry and never throw', { skip: skipDb }, async () => {
  const env = await makeEnv(6);
  const g = await makeRow(env, { prize: 'Race prize' });

  // The forced race: every click read "not entered" (deleteMany saw nothing), so all of them try to create the row.
  // The unique key makes exactly one win; the others take the P2002 path and still answer "Entered".
  const original = prisma.giveawayEntry.deleteMany;
  prisma.giveawayEntry.deleteMany = async () => ({ count: 0 });
  let clicks;
  try {
    clicks = Array.from({ length: 8 }, () => makeClick(env, g.id, uid(1)));
    await Promise.all(clicks.map((c) => handleJoinButton(c, env.client)));
  } finally {
    prisma.giveawayEntry.deleteMany = original;
  }
  assert.equal(await prisma.giveawayEntry.count({ where: { giveawayId: g.id, userId: uid(1) } }), 1);
  for (const c of clicks) assert.match(replyTitle(c), /Entered/);

  // The unforced race: each click is a toggle, so the final state is 0 or 1 rows (never more), and every click answers.
  const raw = Array.from({ length: 10 }, () => makeClick(env, g.id, uid(2)));
  await Promise.all(raw.map((c) => handleJoinButton(c, env.client)));
  assert.ok(await prisma.giveawayEntry.count({ where: { giveawayId: g.id, userId: uid(2) } }) <= 1);
  for (const c of raw) assert.match(replyTitle(c), /Entered|Left Giveaway/);

  // Many users at once: one row each.
  const users = Array.from({ length: 12 }, (_, i) => uid(100 + i));
  await Promise.all(users.map((u) => handleJoinButton(makeClick(env, g.id, u), env.client)));
  assert.equal(await prisma.giveawayEntry.count({ where: { giveawayId: g.id, userId: { in: users } } }), 12);

  // The Participate (N) label is refreshed (leading edge) from the live count.
  await sleep(300);
  const edit = env.channel.edits.at(-1);
  assert.ok(edit, 'the count was refreshed');
  assert.match(edit.components[0].toJSON().components[0].label, /^Participate \(\d+\)$/);
  assert.equal(edit.embeds, undefined, 'only the components are edited');
  resetCountRefresh();
});

// ── DB: end ───────────────────────────────────────────────────────────────────

test('DB end: eligible entrants only, claim, edit, announce, pay once, job gone', { skip: skipDb }, async () => {
  ensureXpProvider();
  const REQ = rid(21);
  const DEN = rid(22);
  const env = await makeEnv(7, {
    roles: [{ id: REQ }, { id: DEN }],
    members: [
      makeMember(uid(1), [REQ]), makeMember(uid(2), [REQ, DEN]), makeMember(uid(3), []), makeMember(uid(4), [REQ], { bot: true }),
      makeMember(uid(5), [REQ]),
    ],
  });
  // uid(6) left the server: not cached, not fetchable.
  const g = await makeRow(env, { prize: 'EndTest-1', winnerCount: 10, requiredRoleIds: [REQ], deniedRoleIds: [DEN], xpPrize: 30, coinPrize: 9, endsAt: new Date(Date.now() + 120_000) });
  await scheduleJob({ guildId: env.guildId, type: END_JOB_TYPE, runAt: g.endsAt, payload: { giveawayId: g.id }, dedupeKey: endJobKey(g.id), maxAttempts: END_MAX_ATTEMPTS });
  await addEntries(g.id, [uid(1), uid(2), uid(3), uid(4), uid(5), uid(6)]);

  const res = await endGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, source: 'command' });
  assert.equal(res.ok, true);
  assert.deepEqual([...res.winnerIds].sort(), [uid(1), uid(5)]);
  assert.equal(res.entryCount, 6);
  assert.equal(res.announced, true);
  const row = await reload(g.id);
  assert.equal(row.status, 'ENDED');
  assert.deepEqual([...row.winnerIds].sort(), [uid(1), uid(5)]);
  assert.ok(row.endedAt);
  assert.equal(row.endsAt.getTime(), g.endsAt.getTime(), 'the planned end is never modified');
  assert.equal(await getJob(endJobKey(g.id)), null, 'the end job is cancelled');

  const edit = env.channel.edits.at(-1);
  assert.equal(edit.id, g.messageId);
  assert.equal(edit.components[0].toJSON().components[0].disabled, true);
  assert.match(edit.embeds[0].toJSON().description, /\*\*Winners:\*\* <@\d+>, <@\d+>/);
  const announcement = env.channel.sent.find((m) => /Congratulations/.test(m.content));
  assert.deepEqual([...announcement.allowedMentions.users].sort(), [uid(1), uid(5)]);
  assert.deepEqual(announcement.allowedMentions.parse, []);
  assert.deepEqual(announcement.reply, { messageReference: g.messageId, failIfNotExists: false });
  assert.deepEqual(grantsFor('EndTest-1').map((x) => x.userId).sort(), [uid(1), uid(5)], 'xp paid once per winner');
  assert.equal(res.payWarnings.length, 1);
  assert.match(res.payWarnings[0], /coin prize could not be paid to 2 winners/);
  assert.match(env.channel.sent.at(-1).content, /coin prize could not be paid/);

  // A second end changes nothing and pays nobody again.
  const again = await endGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, source: 'command' });
  assert.deepEqual([again.ok, again.code], [false, 'NOT_ACTIVE']);
  assert.equal(grantsFor('EndTest-1').length, 2);
  // Wrong guild or id.
  assert.equal((await endGiveaway({ client: env.client, guild: { ...env.guild, id: G(8) }, giveawayId: g.id, source: 'command' })).code, 'NOT_FOUND');
});

test('DB end: concurrent ends claim once and pay each winner exactly once', { skip: skipDb }, async () => {
  ensureXpProvider();
  const members = Array.from({ length: 6 }, (_, i) => makeMember(uid(i + 1)));
  const env = await makeEnv(9, { members });
  const g = await makeRow(env, { prize: 'EndRace-1', winnerCount: 3, xpPrize: 5, endsAt: past() });
  await addEntries(g.id, members.map((m) => m.id));
  const results = await Promise.all([
    endGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, source: 'job', attempt: 1 }),
    endGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, source: 'command' }),
    endGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, source: 'command' }),
  ]);
  assert.equal(results.filter((r) => r.ok).length, 1);
  assert.equal(results.filter((r) => r.code === 'NOT_ACTIVE').length, 2);
  const winners = results.find((r) => r.ok).winnerIds;
  assert.equal(winners.length, 3);
  assert.deepEqual(grantsFor('EndRace-1').map((x) => x.userId).sort(), [...winners].sort());

  // The conditional claim also holds when two flows overlap that the in-process mutex does not serialise
  // (a rolling restart): a second claim on a row that already ended affects zero rows.
  const second = await prisma.giveaway.updateMany({ where: { id: g.id, status: 'ACTIVE' }, data: { status: 'ENDED', winnerIds: ['x'] } });
  assert.equal(second.count, 0);
});

test('DB end: no entrants, weights, deleted message drafts the giveaway, not-due, no access, unresolved entrants', { skip: skipDb }, async () => {
  ensureXpProvider();
  const BONUS = rid(31);
  const members = Array.from({ length: 5 }, (_, i) => makeMember(uid(i + 1), i === 0 ? [BONUS] : []));
  const env = await makeEnv(10, { roles: [{ id: BONUS }], members });

  // No entrants: ends with no winners and says so.
  const empty = await makeRow(env, { prize: 'Empty-1' });
  const r0 = await endGiveaway({ client: env.client, guild: env.guild, giveawayId: empty.id, source: 'command' });
  assert.deepEqual([r0.ok, r0.winnerIds], [true, []]);
  assert.match(env.channel.sent.at(-1).content, /^No valid entrants for \*\*Empty-1\*\*/);
  assert.match(env.channel.edits.at(-1).embeds[0].toJSON().description, /No valid entrants\./);

  // The message was deleted (a purge fires no messageDelete): the end finds it gone, drafts the giveaway, cancels the job.
  const gone = await makeRow(env, { prize: 'Gone-1', endsAt: past() });
  await scheduleJob({ guildId: env.guildId, type: END_JOB_TYPE, runAt: gone.endsAt, payload: { giveawayId: gone.id }, dedupeKey: endJobKey(gone.id) });
  env.channel.store.delete(gone.messageId);
  const r1 = await endGiveaway({ client: env.client, guild: env.guild, giveawayId: gone.id, source: 'job', attempt: 1 });
  assert.deepEqual([r1.ok, r1.code], [false, 'DRAFTED']);
  const goneRow = await reload(gone.id);
  assert.deepEqual([goneRow.status, goneRow.messageId], ['DRAFT', null]);
  assert.equal(await getJob(endJobKey(gone.id)), null);

  // A deleted channel behaves the same.
  const lostChannel = await makeRow(env, { prize: 'LostChannel-1', channelId: nextId() });
  assert.equal((await endGiveaway({ client: env.client, guild: env.guild, giveawayId: lostChannel.id, source: 'command' })).code, 'DRAFTED');

  // A job that fires early is rescheduled, not ended.
  const early = await makeRow(env, { prize: 'Early-1', endsAt: new Date(Date.now() + 600_000) });
  const r2 = await endGiveaway({ client: env.client, guild: env.guild, giveawayId: early.id, source: 'job', attempt: 1 });
  assert.equal(r2.code, 'NOT_DUE');
  assert.equal(r2.nextRunAt.getTime(), early.endsAt.getTime());
  assert.deepEqual(await handleEndJob({ payload: { giveawayId: early.id } }, { client: env.client, guild: env.guild, attempt: 1 }), { nextRunAt: early.endsAt });
  assert.equal((await reload(early.id)).status, 'ACTIVE');
  await assert.rejects(handleEndJob({ payload: {} }, { client: env.client, guild: env.guild, attempt: 1 }), PermanentJobError);
  assert.equal(await handleEndJob({ payload: { giveawayId: 'c' + 'q'.repeat(24) } }, { client: env.client, guild: env.guild, attempt: 1 }), undefined);

  // No access: a command gets NO_ACCESS; a job retries until its final attempt, then ends without posting but still pays.
  const locked = await makeRow(env, { prize: 'Locked-1', winnerCount: 2, xpPrize: 11, endsAt: past() });
  await addEntries(locked.id, [uid(2), uid(3)]);
  env.channel.perms = false;
  const sentBefore = env.channel.sent.length;
  const cmdRes = await endGiveaway({ client: env.client, guild: env.guild, giveawayId: locked.id, source: 'command' });
  assert.deepEqual([cmdRes.ok, cmdRes.code], [false, 'NO_ACCESS']);
  assert.ok(cmdRes.missing.includes('SendMessages'));
  await assert.rejects(endGiveaway({ client: env.client, guild: env.guild, giveawayId: locked.id, source: 'job', attempt: 2 }), /channel unusable/);
  assert.equal((await reload(locked.id)).status, 'ACTIVE', 'a retryable failure changes nothing');
  const last = await endGiveaway({ client: env.client, guild: env.guild, giveawayId: locked.id, source: 'job', attempt: END_MAX_ATTEMPTS });
  assert.equal(last.ok, true);
  assert.equal(last.announced, false);
  assert.equal(env.channel.sent.length, sentBefore, 'nothing was posted');
  assert.equal(grantsFor('Locked-1').length, 2, 'prizes are still paid');
  env.channel.perms = true;

  // Unresolved entrants (the member lookup fails): a command refuses, a job retries, the final attempt ends without them.
  const flaky = await makeRow(env, { prize: 'Flaky-1', winnerCount: 2, endsAt: past() });
  await addEntries(flaky.id, [uid(41), uid(42)]); // neither cached
  env.guild.members.failFetch = true;
  assert.equal((await endGiveaway({ client: env.client, guild: env.guild, giveawayId: flaky.id, source: 'command' })).code, 'RESOLVE_FAILED');
  await assert.rejects(endGiveaway({ client: env.client, guild: env.guild, giveawayId: flaky.id, source: 'job', attempt: 1 }), /could not check every entrant/);
  assert.equal((await reload(flaky.id)).status, 'ACTIVE');
  const fin = await endGiveaway({ client: env.client, guild: env.guild, giveawayId: flaky.id, source: 'job', attempt: END_MAX_ATTEMPTS });
  assert.deepEqual([fin.ok, fin.winnerIds], [true, []]);
  env.guild.members.failFetch = false;

  // An unavailable guild is never judged (rule 20).
  const unavailable = await makeRow(env, { prize: 'Unavail-1', endsAt: past() });
  env.guild.available = false;
  await assert.rejects(endGiveaway({ client: env.client, guild: env.guild, giveawayId: unavailable.id, source: 'job', attempt: 1 }), /unavailable/);
  assert.equal((await endGiveaway({ client: env.client, guild: env.guild, giveawayId: unavailable.id, source: 'command' })).code, 'RESOLVE_FAILED');
  env.guild.available = true;
  assert.equal((await reload(unavailable.id)).status, 'ACTIVE');
});

test('DB end: a bonus role changes the odds (the heavy entrant wins more often)', { skip: skipDb }, async () => {
  const BONUS = rid(41);
  const heavy = makeMember(uid(1), [BONUS]);
  const light = makeMember(uid(2), []);
  const env = await makeEnv(11, { roles: [{ id: BONUS }], members: [heavy, light] });
  let heavyWins = 0;
  const trials = 60;
  for (let i = 0; i < trials; i++) {
    const g = await makeRow(env, { prize: `Odds-${i}`, roleWeights: [{ roleId: BONUS, weight: 10 }] });
    await addEntries(g.id, [heavy.id, light.id]);
    const res = await endGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, source: 'command' });
    if (res.winnerIds[0] === heavy.id) heavyWins++;
  }
  // Weight 10 against 1: expected share 10/11 = 0.909; with 60 trials anything under 0.7 would be a defect.
  assert.ok(heavyWins / trials > 0.7, `heavy won ${heavyWins}/${trials}`);
});

// ── DB: reroll ────────────────────────────────────────────────────────────────

test('DB reroll: excludes earlier winners, appends, pays the new winners only, then runs out', { skip: skipDb }, async () => {
  ensureXpProvider();
  const members = Array.from({ length: 4 }, (_, i) => makeMember(uid(i + 1)));
  const env = await makeEnv(12, { members });
  const g = await makeRow(env, { prize: 'Reroll-1', status: 'ENDED', endedAt: new Date(), winnerIds: [uid(1)], xpPrize: 12 });
  await addEntries(g.id, members.map((m) => m.id));

  const r = await rerollGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, count: 2 });
  assert.equal(r.ok, true);
  assert.equal(r.newWinnerIds.length, 2);
  assert.ok(!r.newWinnerIds.includes(uid(1)));
  assert.equal(r.announced, true);
  const row = await reload(g.id);
  assert.deepEqual(row.winnerIds, [uid(1), ...r.newWinnerIds], 'the history is appended in order');
  assert.deepEqual(grantsFor('Reroll-1').map((x) => x.userId).sort(), [...r.newWinnerIds].sort(), 'earlier winners keep theirs; only the new ones are paid');
  const ann = env.channel.sent.find((m) => /New winners:/.test(m.content));
  assert.deepEqual([...ann.allowedMentions.users].sort(), [...r.newWinnerIds].sort());
  assert.match(env.channel.edits.at(-1).embeds[0].toJSON().description, /\*\*Winners:\*\* <@\d+>, <@\d+>, <@\d+>/);

  const r2 = await rerollGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, count: 5 });
  assert.equal(r2.newWinnerIds.length, 1, 'only the one remaining entrant');
  const r3 = await rerollGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, count: 1 });
  assert.deepEqual([r3.ok, r3.code], [false, 'NO_ENTRANTS']);

  // Not ended / not found.
  const active = await makeRow(env, { prize: 'Reroll-2' });
  assert.equal((await rerollGiveaway({ client: env.client, guild: env.guild, giveawayId: active.id, count: 1 })).code, 'NOT_ENDED');
  assert.equal((await rerollGiveaway({ client: env.client, guild: env.guild, giveawayId: 'c' + 'q'.repeat(24), count: 1 })).code, 'NOT_FOUND');
});

test('DB reroll: a giveaway that changed while drawing gives CONFLICT, a failed lookup RESOLVE_FAILED, a deleted message still announces', { skip: skipDb }, async () => {
  const members = Array.from({ length: 3 }, (_, i) => makeMember(uid(i + 1)));
  const env = await makeEnv(13, { members });
  const g = await makeRow(env, { prize: 'Reroll-3', status: 'ENDED', endedAt: new Date(), winnerIds: [uid(1)] });
  await addEntries(g.id, members.map((m) => m.id));

  const original = prisma.giveawayEntry.findMany.bind(prisma.giveawayEntry);
  prisma.giveawayEntry.findMany = async (args) => {
    const rows = await original(args);
    await prisma.giveaway.update({ where: { id: g.id }, data: { description: 'changed meanwhile' } });
    return rows;
  };
  let res;
  try {
    res = await rerollGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, count: 1 });
  } finally {
    prisma.giveawayEntry.findMany = original;
  }
  assert.deepEqual([res.ok, res.code], [false, 'CONFLICT']);
  assert.deepEqual((await reload(g.id)).winnerIds, [uid(1)], 'nothing was recorded');

  env.guild.members.cache.delete(uid(2));
  env.guild.members.cache.delete(uid(3));
  env.guild.members.failFetch = true;
  assert.equal((await rerollGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, count: 1 })).code, 'RESOLVE_FAILED');
  env.guild.members.failFetch = false;
  env.guild.members.cache.set(uid(2), members[1]);
  env.guild.members.cache.set(uid(3), members[2]);

  // The giveaway message was deleted: the reroll still announces in the channel, without a reply and without an edit.
  env.channel.store.delete(g.messageId);
  const editsBefore = env.channel.edits.length;
  const ok = await rerollGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, count: 1 });
  assert.equal(ok.ok, true);
  assert.equal(ok.announced, true);
  assert.equal(env.channel.edits.length, editsBefore);
});

// ── DB: cancel, restart ───────────────────────────────────────────────────────

test('DB cancel: ACTIVE and DRAFT become CANCELLED, the job and the button are gone, entries stay', { skip: skipDb }, async () => {
  const env = await makeEnv(14);
  const g = await makeRow(env, { prize: 'Cancel-1' });
  await addEntries(g.id, [uid(1), uid(2)]);
  await scheduleJob({ guildId: env.guildId, type: END_JOB_TYPE, runAt: g.endsAt, payload: { giveawayId: g.id }, dedupeKey: endJobKey(g.id) });
  assert.deepEqual(await cancelGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id }), { ok: true });
  const row = await reload(g.id);
  assert.equal(row.status, 'CANCELLED');
  assert.ok(row.endedAt);
  assert.equal(await getJob(endJobKey(g.id)), null);
  assert.equal(await prisma.giveawayEntry.count({ where: { giveawayId: g.id } }), 2);
  const edit = env.channel.edits.at(-1);
  assert.equal(edit.components[0].toJSON().components[0].disabled, true);
  assert.match(edit.embeds[0].toJSON().description, /was cancelled/);
  assert.equal((await cancelGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id })).code, 'NOT_OPEN');

  const draft = await makeRow(env, { prize: 'Cancel-2', status: 'DRAFT', messageId: null });
  assert.equal((await cancelGiveaway({ client: env.client, guild: env.guild, giveawayId: draft.id })).ok, true);
  const ended = await makeRow(env, { prize: 'Cancel-3', status: 'ENDED' });
  assert.equal((await cancelGiveaway({ client: env.client, guild: env.guild, giveawayId: ended.id })).code, 'NOT_OPEN');
});

test('DB restart: a draft becomes ACTIVE again with a new message and job; entries are kept', { skip: skipDb }, async () => {
  const PING = rid(51);
  const env = await makeEnv(15, { roles: [{ id: PING, mentionable: true }] });
  const g = await makeRow(env, { prize: 'Restart-1', status: 'DRAFT', messageId: null, pingRoleId: rid(52) /* deleted */ });
  await addEntries(g.id, [uid(1), uid(2)]);
  const endsAt = new Date(Date.now() + 5 * 3_600_000);
  const other = makeChannel(env.guild, nextId());

  const res = await restartGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, channel: other, endsAt, settings: null });
  assert.equal(res.ok, true);
  const row = await reload(g.id);
  assert.deepEqual([row.status, row.channelId, row.messageId], ['ACTIVE', other.id, res.message.id]);
  assert.equal(row.endsAt.getTime(), endsAt.getTime());
  assert.equal((await getJob(endJobKey(g.id))).runAt.getTime(), endsAt.getTime());
  assert.equal(await prisma.giveawayEntry.count({ where: { giveawayId: g.id } }), 2);
  assert.equal(other.sent[0].content, undefined, 'a deleted ping role is skipped silently');
  assert.deepEqual(other.sent[0].allowedMentions, { parse: [], roles: [] });
  assert.match(other.sent[0].components[0].toJSON().components[0].label, /Participate \(2\)/);

  assert.equal((await restartGiveaway({ client: env.client, guild: env.guild, giveawayId: g.id, channel: other, endsAt, settings: null })).code, 'NOT_DRAFT');

  const draft = await makeRow(env, { prize: 'Restart-2', status: 'DRAFT', messageId: null, pingRoleId: PING });
  other.failSend = Object.assign(new Error('Missing Access'), { code: 50001 });
  const failed = await restartGiveaway({ client: env.client, guild: env.guild, giveawayId: draft.id, channel: other, endsAt, settings: null });
  assert.deepEqual([failed.ok, failed.code], [false, 'SEND_FAILED']);
  assert.equal((await reload(draft.id)).status, 'DRAFT');
  other.failSend = null;
  const ok = await restartGiveaway({ client: env.client, guild: env.guild, giveawayId: draft.id, channel: other, endsAt, settings: null });
  assert.equal(ok.ok, true);
  assert.equal(other.sent.at(-1).content, `<@&${PING}>`, 'the ping role is pinged again at a restart');
  assert.deepEqual(other.sent.at(-1).allowedMentions, { parse: [], roles: [PING] });
});

// ── DB: delete hooks ──────────────────────────────────────────────────────────

test('DB hooks: a deleted message drafts an ACTIVE giveaway (single and bulk) and cancels its job', { skip: skipDb }, async () => {
  const env = await makeEnv(16);
  const mk = async (prize) => {
    const g = await makeRow(env, { prize });
    await scheduleJob({ guildId: env.guildId, type: END_JOB_TYPE, runAt: g.endsAt, payload: { giveawayId: g.id }, dedupeKey: endJobKey(g.id) });
    return g;
  };

  const a = await mk('Hook-1');
  await handleGiveawayMessageDelete({ guildId: env.guildId, message: { id: a.messageId } });
  let row = await reload(a.id);
  assert.deepEqual([row.status, row.messageId], ['DRAFT', null]);
  assert.equal(await getJob(endJobKey(a.id)), null);

  // Through the plugin's declaration, including its filter on a partial message.
  const b = await mk('Hook-2');
  const hook = pluginMod.plugin.hooks[0];
  const ctx = { guildId: env.guildId, client: env.client, message: { id: b.messageId } };
  assert.equal(hook.filter(ctx), true);
  await hook.run(ctx);
  assert.equal((await reload(b.id)).status, 'DRAFT');

  // Another guild's delete, an unknown message and a non-giveaway message do nothing.
  const c = await mk('Hook-3');
  await handleGiveawayMessageDelete({ guildId: G(17), message: { id: c.messageId } });
  await handleGiveawayMessageDelete({ guildId: env.guildId, message: { id: nextId() } });
  assert.equal((await reload(c.id)).status, 'ACTIVE');

  // ENDED and CANCELLED rows only lose the dangling messageId.
  const ended = await makeRow(env, { prize: 'Hook-4', status: 'ENDED' });
  await handleGiveawayMessageDelete({ guildId: env.guildId, message: { id: ended.messageId } });
  row = await reload(ended.id);
  assert.deepEqual([row.status, row.messageId], ['ENDED', null]);

  // Bulk: one batched lookup drafts every ACTIVE giveaway among the ids and leaves the rest alone.
  const d = await mk('Hook-5');
  const e = await mk('Hook-6');
  const keep = await mk('Hook-7');
  const endedBulk = await makeRow(env, { prize: 'Hook-8', status: 'ENDED' });
  const messages = new Collection([[d.messageId, {}], [e.messageId, {}], [endedBulk.messageId, {}], [nextId(), {}]]);
  await pluginMod.plugin.hooks[1].run({ guildId: env.guildId, client: env.client, messages });
  for (const x of [d, e]) {
    row = await reload(x.id);
    assert.deepEqual([row.status, row.messageId], ['DRAFT', null]);
    assert.equal(await getJob(endJobKey(x.id)), null);
  }
  assert.equal((await reload(keep.id)).status, 'ACTIVE');
  assert.notEqual(await getJob(endJobKey(keep.id)), null);
  assert.equal((await reload(endedBulk.id)).status, 'ENDED');
  await handleGiveawayBulkDelete({ guildId: env.guildId, messages: new Collection() });
  await handleGiveawayBulkDelete({ guildId: env.guildId });
});

// ── DB: odds ──────────────────────────────────────────────────────────────────

test('DB odds: add, replace, remove, the ten-role limit, only open giveaways, the embed is refreshed', { skip: skipDb }, async () => {
  const env = await makeEnv(18, { roles: Array.from({ length: 12 }, (_, i) => ({ id: rid(60 + i) })) });
  const g = await makeRow(env, { prize: 'Odds-X' });
  const call = (op, n, weight) => updateOdds({ client: env.client, guild: env.guild, giveawayId: g.id, op, roleId: rid(60 + n), weight, settings: null });

  let r = await call('add', 0, 3);
  assert.deepEqual(r, { ok: true, roleWeights: [{ roleId: rid(60), weight: 3 }] });
  assert.match(env.channel.edits.at(-1).embeds[0].toJSON().description, new RegExp(`\\*\\*Bonus odds:\\*\\* <@&${rid(60)}> ×3`));
  r = await call('add', 0, 6);
  assert.deepEqual(r.roleWeights, [{ roleId: rid(60), weight: 6 }], 'adding the same role replaces its weight');
  for (let n = 1; n < MAX_ROLE_WEIGHTS; n++) assert.equal((await call('add', n, 2)).ok, true);
  assert.equal((await reload(g.id)).roleWeights.length, MAX_ROLE_WEIGHTS);
  assert.equal((await call('add', 10, 2)).code, 'LIMIT', 'an 11th role is refused, not silently dropped');
  assert.equal((await call('add', 3, 9)).ok, true, 'changing an existing role at the limit still works');
  assert.equal((await reload(g.id)).roleWeights.length, MAX_ROLE_WEIGHTS);
  assert.equal((await call('remove', 3)).ok, true);
  assert.equal((await call('remove', 3)).code, 'NOT_SET');
  assert.equal((await reload(g.id)).roleWeights.length, MAX_ROLE_WEIGHTS - 1);
  assert.equal((await call('add', 10, 2)).ok, true);

  const draft = await makeRow(env, { prize: 'Odds-Y', status: 'DRAFT', messageId: null });
  assert.equal((await updateOdds({ client: env.client, guild: env.guild, giveawayId: draft.id, op: 'add', roleId: rid(60), weight: 2, settings: null })).ok, true);
  const ended = await makeRow(env, { prize: 'Odds-Z', status: 'ENDED' });
  assert.equal((await updateOdds({ client: env.client, guild: env.guild, giveawayId: ended.id, op: 'add', roleId: rid(60), weight: 2, settings: null })).code, 'NOT_OPEN');
  assert.equal((await updateOdds({ client: env.client, guild: env.guild, giveawayId: 'c' + 'q'.repeat(24), op: 'add', roleId: rid(60), weight: 2, settings: null })).code, 'NOT_FOUND');
});

// ── DB: references, list and autocomplete queries ─────────────────────────────

test('DB resolveGiveawayRef: by id, message id and message link, scoped to the guild', { skip: skipDb }, async () => {
  const env = await makeEnv(19);
  const g = await makeRow(env, { prize: 'Ref-1' });
  assert.equal((await resolveGiveawayRef(env.guildId, g.id)).id, g.id);
  assert.equal((await resolveGiveawayRef(env.guildId, g.messageId)).id, g.id);
  assert.equal((await resolveGiveawayRef(env.guildId, `https://discord.com/channels/${env.guildId}/${g.channelId}/${g.messageId}`)).id, g.id);
  assert.equal(await resolveGiveawayRef(G(20), g.id), null);
  assert.equal(await resolveGiveawayRef(G(20), g.messageId), null);
  assert.equal(await resolveGiveawayRef(env.guildId, `https://discord.com/channels/${G(20)}/${g.channelId}/${g.messageId}`), null);
  assert.equal(await resolveGiveawayRef(env.guildId, 'nonsense'), null);
});

// ── DB: the /giveaway subcommands against fake interactions ───────────────────

function makeCommand(env, sub, opts = {}, { group = null, focused = null, member = null } = {}) {
  const role = (id) => env.guild.roles.cache.get(id) ?? null;
  const i = {
    guildId: env.guildId,
    guild: env.guild,
    channel: env.channel,
    user: { id: HOST },
    member: member ?? makeMember(HOST, []),
    createdTimestamp: Date.now(),
    deferred: false,
    replied: false,
    ephemeral: undefined,
    replies: [],
    responded: false,
    choices: null,
    isRepliable: () => true,
    async deferReply({ ephemeral }) { i.deferred = true; i.ephemeral = ephemeral; },
    async editReply(p) { i.replies.push(p); return { id: 'm', createMessageComponentCollector: () => { throw new Error('single page expected'); } }; },
    async respond(c) { i.responded = true; i.choices = c; },
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      getString: (n) => opts[n] ?? null,
      getInteger: (n) => opts[n] ?? null,
      getRole: (n) => (opts[n] ? role(opts[n]) : null),
      getChannel: (n) => (opts[n] ? env.guild.channels.cache.get(opts[n]) : null),
      getFocused: () => focused,
    },
  };
  return i;
}
const embedOf = (i) => i.replies.at(-1)?.embeds?.[0]?.toJSON?.() ?? {};

test('DB /giveaway start: validation order, refusals and a successful start', { skip: skipDb }, async () => {
  ensureXpProvider();
  const REQ = rid(71);
  const BON = rid(72);
  const PING = rid(73);
  const env = await makeEnv(1, { roles: [{ id: REQ }, { id: BON }, { id: PING, mentionable: false }] });
  env.guild.roles.cache.set(env.guildId, { id: env.guildId, mentionable: false }); // @everyone
  await updateSettings(env.guildId, { xpEnabled: true, timezone: 'UTC' });

  const run = async (opts, extra) => {
    const i = makeCommand(env, 'start', { prize: 'Cmd prize', duration: '2h', ...opts }, extra);
    await cmd.execute(i, env.client);
    return i;
  };
  let i = await run({});
  assert.equal(i.ephemeral, true, 'every reply is ephemeral');
  assert.match(embedOf(i).title, /Giveaway Started/);
  assert.match(embedOf(i).description, /ID `c[a-z0-9]+`/);
  assert.equal(await prisma.giveaway.count({ where: { guildId: env.guildId, prize: 'Cmd prize' } }), 1);

  i = await run({ duration: 'soon' });
  assert.match(embedOf(i).title, /Invalid Duration/);
  i = await run({ duration: '10s' });
  assert.match(embedOf(i).title, /Too Short/);
  i = await run({ duration: '100d' });
  assert.match(embedOf(i).title, /Too Long/);
  i = await run({ duration: '1m' });
  assert.match(embedOf(i).title, /Giveaway Started/, 'a one-minute giveaway is allowed (one clock reading for parse and validate)');
  i = await run({ required_role: env.guildId });
  assert.match(embedOf(i).title, /Invalid Role/);
  i = await run({ denied_role: env.guildId });
  assert.match(embedOf(i).description, /not @everyone/);
  i = await run({ required_role: REQ, denied_role: REQ });
  assert.match(embedOf(i).description, /both required and denied/);
  i = await run({ bonus_weight: 3 });
  assert.match(embedOf(i).title, /Missing Role/);
  i = await run({ bonus_role: env.guildId });
  assert.match(embedOf(i).title, /Invalid Role/);
  i = await run({ coins: 50 });
  assert.match(embedOf(i).title, /Prize Unavailable/);
  assert.match(embedOf(i).description, /Coin prizes are not available on this bot/);
  i = await run({ ping_role: PING }); // not mentionable, and the fake channel grants Mention Everyone: allowed
  assert.match(embedOf(i).title, /Giveaway Started/);
  i = await run({ ping_role: env.guildId });
  assert.match(embedOf(i).description, /never ping @everyone/);
  i = await run({ color: 'not-a-colour' });
  assert.match(embedOf(i).title, /Invalid Colour/);
  i = await run({ prize: '   ' });
  assert.match(embedOf(i).title, /Invalid Prize/);
  env.channel.perms = false;
  i = await run({});
  assert.match(embedOf(i).title, /No Access/);
  env.channel.perms = true;

  const before = await prisma.giveaway.count({ where: { guildId: env.guildId } });
  i = await run({
    prize: 'Full prize', description: 'About it', winners: 5, required_role: REQ, bonus_role: BON, bonus_weight: 7, xp: 100, color: '#5865F2',
  });
  assert.match(embedOf(i).title, /Giveaway Started/);
  const row = await prisma.giveaway.findFirst({ where: { guildId: env.guildId, prize: 'Full prize' } });
  assert.equal(await prisma.giveaway.count({ where: { guildId: env.guildId } }), before + 1);
  assert.deepEqual([row.winnerCount, row.requiredRoleIds, row.deniedRoleIds, row.xpPrize, row.coinPrize, row.embedColor], [5, [REQ], [], 100, null, 0x5865f2]);
  assert.deepEqual(row.roleWeights, [{ roleId: BON, weight: 7 }]);
  assert.equal(row.description, 'About it');
  assert.equal(row.hostId, HOST);

  // A denied role is stored and shown.
  i = await run({ prize: 'Denied prize', denied_role: BON });
  const denied = await prisma.giveaway.findFirst({ where: { guildId: env.guildId, prize: 'Denied prize' } });
  assert.deepEqual(denied.deniedRoleIds, [BON]);

  // With the Levels module off the xp prize is refused cleanly.
  await updateSettings(env.guildId, { xpEnabled: false });
  i = await run({ xp: 10 });
  assert.match(embedOf(i).description, /XP prizes need the \*\*⭐ Levels\*\* module\. Enable it with `\/module enable leveling`\./);
});

test('DB /giveaway end, reroll, list, entrants, odds and autocomplete', { skip: skipDb }, async () => {
  const BON = rid(81);
  const members = Array.from({ length: 3 }, (_, i) => makeMember(uid(i + 1)));
  const env = await makeEnv(21, { roles: [{ id: BON }], members });
  await updateSettings(env.guildId, { xpEnabled: false });
  env.guild.channels.cache.set(env.channel.id, env.channel);

  const active = await makeRow(env, { prize: 'Cmd-Active' });
  await addEntries(active.id, members.map((m) => m.id));
  const draft = await makeRow(env, { prize: 'Cmd-Draft', status: 'DRAFT', messageId: null });
  const done = await makeRow(env, { prize: 'Cmd-Done', status: 'ENDED', endedAt: new Date(), winnerIds: [uid(1)] });
  await addEntries(done.id, [uid(1), uid(2)]);

  const call = async (sub, opts, extra) => {
    const i = makeCommand(env, sub, opts, extra);
    await cmd.execute(i, env.client);
    return i;
  };

  // end
  let i = await call('end', { giveaway: draft.id });
  assert.match(embedOf(i).description, /draft/);
  i = await call('end', { giveaway: 'nonsense' });
  assert.match(embedOf(i).title, /Giveaway Not Found/);
  i = await call('end', { giveaway: done.id });
  assert.match(embedOf(i).description, /already ended/);
  i = await call('end', { giveaway: active.messageId });
  assert.match(embedOf(i).title, /Giveaway Ended/);
  assert.match(embedOf(i).description, /Winners of \*\*Cmd-Active\*\*: <@\d+>/);
  i = await call('end', { giveaway: active.id });
  assert.match(embedOf(i).description, /already ended/);

  // reroll
  i = await call('reroll', { giveaway: draft.id });
  assert.match(embedOf(i).title, /Cannot Reroll/);
  i = await call('reroll', { giveaway: done.id });
  assert.match(embedOf(i).title, /Giveaway Rerolled/);
  assert.match(embedOf(i).description, new RegExp(`New winner of \\*\\*Cmd-Done\\*\\*: <@${uid(2)}>`));
  i = await call('reroll', { giveaway: done.id });
  assert.match(embedOf(i).title, /No One Left/);
  const act2 = await makeRow(env, { prize: 'Cmd-Active2' });
  i = await call('reroll', { giveaway: act2.id });
  assert.match(embedOf(i).title, /Still Running/);

  // list: filters, order, entry counts, one page (the fake interaction has no collector)
  i = await call('list', {});
  assert.match(embedOf(i).description, /Cmd-Draft/);
  assert.match(embedOf(i).description, /Cmd-Active2/);
  assert.ok(!/Cmd-Done/.test(embedOf(i).description), 'the default filter shows open giveaways only');
  i = await call('list', { status: 'ended' });
  assert.match(embedOf(i).description, /Cmd-Done.* 2 entries/s);
  i = await call('list', { status: 'cancelled' });
  assert.match(embedOf(i).title, /No Giveaways/);
  assert.match(embedOf(i).description, /no cancelled giveaways.*status:all/, 'names the filter and points to status:all');
  i = await call('list', { status: 'all' });
  assert.match(embedOf(i).description, /Cmd-Done/);

  // entrants
  i = await call('entrants', { giveaway: done.id });
  const e = embedOf(i);
  assert.match(e.title, /Entrants: Cmd-Done/);
  assert.match(e.description, /\*\*Status:\*\* ended/);
  assert.match(e.description, new RegExp(`1\\. <@${uid(1)}> · <t:\\d+:R> 🏆`));
  assert.match(e.footer.text, /^2 entrants/);
  const none = await call('entrants', { giveaway: draft.id });
  assert.match(embedOf(none).title, /No Entrants/);

  // odds
  const odds = await call('add', { giveaway: act2.id, role: BON, weight: 4 }, { group: 'odds' });
  assert.match(embedOf(odds).title, /Odds Updated/);
  assert.deepEqual((await reload(act2.id)).roleWeights, [{ roleId: BON, weight: 4 }]);
  const rm = await call('remove', { giveaway: act2.id, role: BON }, { group: 'odds' });
  assert.match(embedOf(rm).title, /Odds Updated/);
  const rm2 = await call('remove', { giveaway: act2.id, role: BON }, { group: 'odds' });
  assert.match(embedOf(rm2).title, /Not Set/);
  const oddsEnded = await call('add', { giveaway: done.id, role: BON, weight: 4 }, { group: 'odds' });
  assert.match(embedOf(oddsEnded).title, /Not Open/);

  // restart (a draft whose end time has passed needs a new duration)
  const old = await makeRow(env, { prize: 'Cmd-Old', status: 'DRAFT', messageId: null, endsAt: new Date(Date.now() - 1000) });
  i = await call('restart', { giveaway: old.id });
  assert.match(embedOf(i).title, /Duration Needed/);
  i = await call('restart', { giveaway: old.id, duration: '3h' });
  assert.match(embedOf(i).title, /Giveaway Restarted/);
  assert.equal((await reload(old.id)).status, 'ACTIVE');
  i = await call('restart', { giveaway: draft.id });
  assert.match(embedOf(i).title, /Giveaway Restarted/);
  i = await call('restart', { giveaway: active.id });
  assert.match(embedOf(i).title, /Not a Draft/);

  // autocomplete: filtered in the database by prize text, id prefix or message id, restricted by subcommand status
  const ac = async (sub, value, group = null) => {
    const a = makeCommand(env, sub, {}, { group, focused: { name: 'giveaway', value } });
    await cmd.autocomplete(a, env.client);
    return a.choices;
  };
  let choices = await ac('end', '');
  assert.ok(choices.some((c) => c.value === act2.id), 'ACTIVE giveaways are offered to /end');
  assert.ok(!choices.some((c) => c.value === done.id), 'ended ones are not');
  assert.match(choices.find((c) => c.value === act2.id).name, /^🟢 Cmd-Active2 · #deleted-channel|^🟢 Cmd-Active2 · #[^ ]+ · ends in /);
  choices = await ac('end', 'active2');
  assert.deepEqual(choices.map((c) => c.value), [act2.id]);
  choices = await ac('reroll', done.id.slice(0, 20));
  assert.deepEqual(choices.map((c) => c.value), [done.id]);
  choices = await ac('entrants', done.messageId ?? 'none');
  assert.ok(choices.every((c) => c.value.length <= 100));
  choices = await ac('add', 'Cmd-', 'odds');
  assert.ok(choices.length > 0 && choices.length <= 25);
  const wrong = makeCommand(env, 'end', {}, { focused: { name: 'other', value: 'x' } });
  await cmd.autocomplete(wrong, env.client);
  assert.deepEqual(wrong.choices, []);
});
