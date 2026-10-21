// AFK tests. Offline (no Discord, Postgres or Redis): the text and nickname rules, the per-guild AFK id memo, the message
// handler against an in-memory AfkStatus table and fake channels, the plugin declaration, the /afk payload and its
// subcommands.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const svc = await import('../src/services/afkService.js');
const afk = await import('../src/plugins/afk.js');
const cmd = await import('../src/commands/community/afk.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { PERMISSION_LEVELS, LIMITS } = await import('../src/config/constants.js');
const { updateSettings } = await import('../src/services/settingsService.js');

const {
  AFK, cleanReason, plainText, nameWithoutPrefix, afkLength, readPings, isIgnoredChannel, prefixedNickname, restoredNickname,
  peekAfkIds, loadAfkIds, forgetAfkIds, setAfk, clearAfk, loadStatus,
} = svc;
const { handleMessage, mayConcernAfk, welcomeText, noticeText, plugin } = afk;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// In-memory AfkStatus table; GuildSettings rows always "exist".
const rows = new Map();
const calls = { findUnique: 0, findMany: 0 };
let failFindMany = false;
const keyOf = (where) => `${where.guildId_userId.guildId}:${where.guildId_userId.userId}`;
const clone = (r) => ({ ...r, since: new Date(r.since), pings: JSON.parse(JSON.stringify(r.pings)) });
const realAfkStatus = prisma.afkStatus;
const realGuildSettings = prisma.guildSettings;
prisma.afkStatus = {
  findUnique: async ({ where }) => {
    calls.findUnique += 1;
    const r = rows.get(keyOf(where));
    return r ? clone(r) : null;
  },
  findMany: async ({ where, orderBy, take }) => {
    calls.findMany += 1;
    if (failFindMany) throw new Error('database down');
    let list = [...rows.values()].filter((r) => r.guildId === where.guildId);
    if (where.userId?.in) list = list.filter((r) => where.userId.in.includes(r.userId));
    if (orderBy) list.sort((a, b) => a.since - b.since);
    if (take) list = list.slice(0, take);
    return list.map(clone);
  },
  count: async ({ where }) => [...rows.values()].filter((r) => r.guildId === where.guildId).length,
  upsert: async ({ where, create, update }) => {
    const key = keyOf(where);
    const row = rows.has(key)
      ? { ...rows.get(key), ...update }
      : { reason: null, nickChanged: false, previousNick: null, pings: [], pingCount: 0, since: new Date(), ...create };
    rows.set(key, row);
    return clone(row);
  },
  update: async ({ where, data }) => {
    const key = keyOf(where);
    const row = { ...rows.get(key) };
    for (const [k, v] of Object.entries(data)) row[k] = v && typeof v === 'object' && 'increment' in v ? row[k] + v.increment : v;
    rows.set(key, row);
    return clone(row);
  },
  deleteMany: async ({ where }) => {
    const key = `${where.guildId}:${where.userId}`;
    return { count: rows.delete(key) ? 1 : 0 };
  },
};
prisma.guildSettings = { upsert: async () => ({}) };

after(async () => {
  prisma.afkStatus = realAfkStatus;
  prisma.guildSettings = realGuildSettings;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

let nextId = 1000;
const MINUTE = 60_000;
const ago = (ms) => new Date(Date.now() - ms);

function fakeChannel(id, { allowed = true, parentId = null, parent = null } = {}) {
  return {
    id, guildId: 'G', parentId, parent,
    guild: { members: { me: {} } },
    permissionsFor: () => ({ has: () => allowed }),
    isThread: () => false,
  };
}

function fakeMember(id, { nickname = null, username = `user${id}`, globalName = null, manageable = true, canNick = true } = {}) {
  const m = {
    id, nickname, manageable, nicks: [],
    user: { id, username, globalName },
    guild: { id: 'G', members: { me: { permissions: { has: () => canNick } } } },
    get displayName() { return m.nickname ?? globalName ?? username; },
    async setNickname(nick) { m.nicks.push(nick); m.nickname = nick; },
  };
  return m;
}

function fakeMessage(channel, member, { mentions = [] } = {}) {
  const users = new Map(mentions.map((x) => [x.user.id, x.user]));
  const members = new Map(mentions.map((x) => [x.user.id, x]));
  const m = {
    id: String(nextId++), guildId: 'G', channelId: channel.id, channel, member, author: member.user,
    mentions: { users, members },
    replies: [], sent: [],
    async reply(payload) {
      m.replies.push(payload);
      const sent = { payload, deleted: false, async delete() { sent.deleted = true; } };
      m.sent.push(sent);
      return sent;
    },
  };
  return m;
}

const ctxFor = (guildId, message, settings = { afkEnabled: true }) => ({ guildId, message, getSettings: async () => settings });
const seed = (guildId, userId, extra = {}) => {
  rows.set(`${guildId}:${userId}`, { guildId, userId, reason: null, nickChanged: false, previousNick: null, pings: [], pingCount: 0, since: ago(5 * MINUTE), ...extra });
};

// ── Text and nickname rules ───────────────────────────────────────────────────

test('cleanReason: one line, trimmed and capped', () => {
  assert.equal(cleanReason('  at\nthe   gym \t'), 'at the gym');
  assert.equal(cleanReason('   '), null);
  assert.equal(cleanReason(null), null);
  assert.equal(cleanReason(5), null);
  const long = cleanReason('x'.repeat(LIMITS.AFK_REASON_LENGTH + 50));
  assert.ok(long.length <= LIMITS.AFK_REASON_LENGTH);
});

test('plainText breaks mention syntax and caps the length', () => {
  const t = plainText('hi @everyone and @here <@123> <@&456> <#789> :smile:');
  assert.ok(!t.includes('@everyone') && !t.includes('@here'));
  assert.ok(!t.includes('<@') && !t.includes('<#'));
  assert.ok(t.includes(':smile:'), 'emoji syntax is kept');
  assert.equal(plainText('x'.repeat(500), 40).length, 40);
  assert.equal(plainText(null), '');
});

test('nameWithoutPrefix, afkLength and readPings', () => {
  assert.equal(nameWithoutPrefix('[AFK] Bob'), 'Bob');
  assert.equal(nameWithoutPrefix('Bob'), 'Bob');
  assert.equal(afkLength(5 * MINUTE), '5m');
  assert.equal(afkLength(2 * 3_600_000 + 5 * MINUTE + 9000), '2h 5m');
  assert.equal(afkLength(30_000), '30s');
  assert.equal(afkLength(-5), '0s');
  const pings = readPings([{ channelId: 'C', messageId: 'M', authorId: 'A', at: 5 }, { channelId: 'C' }, null, 'x']);
  assert.deepEqual(pings, [{ channelId: 'C', messageId: 'M', authorId: 'A', at: 5 }]);
  assert.deepEqual(readPings({}), []);
});

test('prefixedNickname adds the prefix within 32 characters and never twice', () => {
  assert.equal(prefixedNickname('Bob'), '[AFK] Bob');
  assert.equal(prefixedNickname('[AFK] Bob'), null);
  const long = prefixedNickname('n'.repeat(40));
  assert.equal(long.length, AFK.NICKNAME_MAX);
  assert.ok(long.startsWith(AFK.PREFIX));
  // Never splits an emoji at the cut.
  const emoji = prefixedNickname(`${'a'.repeat(25)}😀😀`);
  assert.ok(emoji.length <= AFK.NICKNAME_MAX);
  assert.ok(!/[\ud800-\udbff]$/.test(emoji));
});

test('restoredNickname: the saved nickname back, only while the nickname still starts with the prefix', () => {
  const long = 'L'.repeat(32);
  assert.equal(restoredNickname('[AFK] Bobster', 'Bobster'), 'Bobster');
  assert.equal(restoredNickname(prefixedNickname(long), long), long, 'a nickname cut to fit the prefix comes back whole');
  assert.equal(restoredNickname('[AFK] Bob', null), null, 'no nickname before the prefix: clear it');
  assert.equal(restoredNickname('[AFK] ', undefined), null);
  assert.equal(restoredNickname('Robert', 'Bobster'), undefined, 'renamed: leave it');
  assert.equal(restoredNickname(null, 'Bobster'), undefined);
});

test('isIgnoredChannel: the channel, its category, or a thread\'s channel and that channel\'s category', () => {
  const settings = { afkIgnoredChannelIds: ['CAT', 'C1'] };
  assert.equal(isIgnoredChannel(settings, { id: 'C1' }), true);
  assert.equal(isIgnoredChannel(settings, { id: 'C2', parentId: 'CAT' }), true);
  assert.equal(isIgnoredChannel(settings, { id: 'T', parentId: 'C9', parent: { parentId: 'CAT' } }), true);
  assert.equal(isIgnoredChannel(settings, { id: 'C2', parentId: 'OTHER' }), false);
  assert.equal(isIgnoredChannel({ afkIgnoredChannelIds: [] }, { id: 'C1' }), false);
  assert.equal(isIgnoredChannel(null, { id: 'C1' }), false);
  assert.equal(isIgnoredChannel(settings, null), false);
});

test('welcomeText and noticeText', () => {
  const since = ago(2 * 3_600_000 + 5 * MINUTE);
  const none = welcomeText({ name: 'Bob', guildId: 'G', row: { since, pingCount: 0, pings: [] } });
  assert.match(none, /Welcome back, \*\*Bob\*\*! You were AFK for \*\*2h 5m\*\*\./);
  assert.ok(!none.includes('mentioned'));
  const pings = Array.from({ length: 8 }, (_, i) => ({ channelId: 'C', messageId: `M${i}`, authorId: 'A', at: i }));
  const text = welcomeText({ name: 'Bob', guildId: 'G', row: { since, pingCount: 12, pings } });
  assert.match(text, /mentioned \*\*12\*\* times/);
  assert.equal(text.match(/https:\/\/discord\.com\/channels\/G\/C\/M\d/g).length, AFK.MAX_LINKS, 'at most 5 jump links');
  assert.ok(text.includes('/M7)') && !text.includes('/M2)'), 'the latest pings');
  assert.match(welcomeText({ name: 'Bob', guildId: 'G', row: { since, pingCount: 1, pings: [] } }), /mentioned \*\*1\*\* time\./);

  assert.match(noticeText({ name: 'Bob', row: { reason: 'at the gym', since } }), /^💤 \*\*Bob\*\* is AFK: at the gym \(since <t:\d+:R>\)$/);
  assert.match(noticeText({ name: 'Bob', row: { reason: null, since } }), /^💤 \*\*Bob\*\* is AFK \(since <t:\d+:R>\)$/);
});

// ── State and the id memo ─────────────────────────────────────────────────────

test('setAfk, clearAfk and the memoised set of ids', async () => {
  forgetAfkIds('G1');
  assert.equal(peekAfkIds('G1'), null, 'unknown until loaded');
  const before = calls.findMany;
  const ids = await loadAfkIds('G1');
  assert.equal(ids.size, 0);
  assert.equal(calls.findMany, before + 1);

  const row = await setAfk({ guildId: 'G1', userId: 'U1', reason: 'lunch', nickChanged: true, previousNick: 'Uncle One' });
  assert.equal(row.reason, 'lunch');
  assert.equal(row.previousNick, 'Uncle One');
  assert.ok(peekAfkIds('G1').has('U1'), 'the memo follows the write');
  assert.equal(calls.findMany, before + 1, 'no second query');

  // A second /afk set changes the reason only: the start time, the saved pings, the nickname flag and the saved nickname stay.
  const since = new Date(Date.now() - 3 * MINUTE);
  rows.get('G1:U1').since = since;
  rows.get('G1:U1').pingCount = 4;
  await setAfk({ guildId: 'G1', userId: 'U1', reason: 'dinner', nickChanged: false });
  const kept = await loadStatus('G1', 'U1');
  assert.equal(kept.reason, 'dinner');
  assert.equal(kept.since.getTime(), since.getTime());
  assert.equal(kept.pingCount, 4);
  assert.equal(kept.nickChanged, true);
  assert.equal(kept.previousNick, 'Uncle One');

  const cleared = await clearAfk('G1', 'U1');
  assert.equal(cleared.reason, 'dinner');
  assert.equal(peekAfkIds('G1').has('U1'), false);
  assert.equal(await clearAfk('G1', 'U1'), null, 'only the first clear gets the row');
});

test('a failed load reads as nobody AFK for a moment, and concurrent loads share one query', async () => {
  forgetAfkIds('G2');
  failFindMany = true;
  const before = calls.findMany;
  try {
    const [a, b] = await Promise.all([loadAfkIds('G2'), loadAfkIds('G2')]);
    assert.equal(a.size, 0);
    assert.equal(a, b);
    assert.equal(calls.findMany, before + 1);
    assert.equal(peekAfkIds('G2').size, 0, 'remembered, so messages do not each retry the query');
  } finally {
    failFindMany = false;
    forgetAfkIds('G2');
  }
});

test('the hook filter lets through only the author who is AFK or a mention of one', async () => {
  forgetAfkIds('G3');
  const ch = fakeChannel('C3');
  const bob = fakeMember('B3');
  const amy = fakeMember('A3');
  const cat = fakeMember('K3');
  // Not loaded yet: the handler decides.
  assert.equal(mayConcernAfk({ guildId: 'G3', message: fakeMessage(ch, bob) }), true);

  await loadAfkIds('G3');
  assert.equal(mayConcernAfk({ guildId: 'G3', message: fakeMessage(ch, bob) }), false, 'nobody is AFK');
  await setAfk({ guildId: 'G3', userId: 'A3', reason: null });
  assert.equal(mayConcernAfk({ guildId: 'G3', message: fakeMessage(ch, amy) }), true, 'the AFK author');
  assert.equal(mayConcernAfk({ guildId: 'G3', message: fakeMessage(ch, bob, { mentions: [amy] }) }), true, 'a mention of an AFK member');
  assert.equal(mayConcernAfk({ guildId: 'G3', message: fakeMessage(ch, bob, { mentions: [cat] }) }), false, 'a mention of someone else');
  assert.equal(mayConcernAfk({ guildId: 'G3', message: fakeMessage(ch, bob) }), false);
});

// ── The message handler ───────────────────────────────────────────────────────

test('the AFK author writes: the row is deleted, the prefix comes off and a self-deleting welcome is sent', async () => {
  const ch = fakeChannel('C10');
  const bob = fakeMember('B10', { nickname: '[AFK] Bobster' });
  const pings = [{ channelId: 'C10', messageId: 'M1', authorId: 'X', at: 1 }, { channelId: 'C11', messageId: 'M2', authorId: 'Y', at: 2 }];
  seed('G10', 'B10', { nickChanged: true, previousNick: 'Bobster', pings, pingCount: 2, since: ago(75 * MINUTE) });
  forgetAfkIds('G10');

  const msg = fakeMessage(ch, bob);
  await handleMessage(ctxFor('G10', msg), { welcomeDeleteMs: 15 });

  assert.equal(rows.has('G10:B10'), false);
  assert.ok(!peekAfkIds('G10').has('B10'));
  assert.deepEqual(bob.nicks, ['Bobster']);
  assert.equal(msg.replies.length, 1);
  const reply = msg.replies[0];
  assert.match(reply.content, /Welcome back, \*\*Bobster\*\*! You were AFK for \*\*1h 15m\*\*/);
  assert.match(reply.content, /mentioned \*\*2\*\* times/);
  assert.ok(reply.content.includes('https://discord.com/channels/G10/C10/M1') && reply.content.includes('https://discord.com/channels/G10/C11/M2'));
  assert.deepEqual(reply.allowedMentions, { parse: [], repliedUser: false });
  assert.equal(msg.sent[0].deleted, false, 'not yet');
  await new Promise((r) => { setTimeout(r, 60); });
  assert.equal(msg.sent[0].deleted, true, 'deletes itself');
});

test('the welcome reply deletes itself after about 10 seconds, on an unref\'d timer', async () => {
  assert.equal(AFK.WELCOME_DELETE_MS, 10_000);
  const ch = fakeChannel('C12');
  const bob = fakeMember('B12');
  seed('G12', 'B12');
  forgetAfkIds('G12');
  const realSetTimeout = globalThis.setTimeout;
  const timers = [];
  globalThis.setTimeout = (fn, ms, ...rest) => {
    const t = realSetTimeout(fn, ms, ...rest);
    timers.push({ ms, t });
    return t;
  };
  try {
    await handleMessage(ctxFor('G12', fakeMessage(ch, bob)));
  } finally {
    globalThis.setTimeout = realSetTimeout;
  }
  const mine = timers.find((x) => x.ms === AFK.WELCOME_DELETE_MS);
  assert.ok(mine, 'a 10 s delete timer was set');
  assert.equal(mine.t.hasRef(), false, 'unref\'d');
  clearTimeout(mine.t);
});

test('a message within 10 seconds of /afk set does not end it', async () => {
  const ch = fakeChannel('C13');
  const bob = fakeMember('B13');
  seed('G13', 'B13', { since: ago(3_000) });
  forgetAfkIds('G13');
  const msg = fakeMessage(ch, bob);
  await handleMessage(ctxFor('G13', msg));
  assert.ok(rows.has('G13:B13'));
  assert.equal(msg.replies.length, 0);

  rows.get('G13:B13').since = ago(AFK.GRACE_MS + 2_000);
  const later = fakeMessage(ch, bob);
  await handleMessage(ctxFor('G13', later));
  assert.equal(rows.has('G13:B13'), false);
  assert.equal(later.replies.length, 1);
});

test('ignored channels and categories neither end an AFK nor answer mentions', async () => {
  const settings = { afkEnabled: true, afkIgnoredChannelIds: ['CAT14', 'C14'] };
  const bob = fakeMember('B14');
  const amy = fakeMember('A14');
  seed('G14', 'B14');
  seed('G14', 'A14', { reason: 'out' });
  forgetAfkIds('G14');
  for (const ch of [fakeChannel('C14'), fakeChannel('C14b', { parentId: 'CAT14' })]) {
    const own = fakeMessage(ch, bob);
    const ping = fakeMessage(ch, fakeMember('X14'), { mentions: [amy] });
    await handleMessage(ctxFor('G14', own, settings));
    await handleMessage(ctxFor('G14', ping, settings));
    assert.equal(own.replies.length, 0);
    assert.equal(ping.replies.length, 0);
  }
  assert.ok(rows.has('G14:B14'), 'still AFK');
  assert.equal(rows.get('G14:A14').pingCount, 0, 'no ping recorded');
  // Settings that could not be loaded do nothing either.
  await handleMessage(ctxFor('G14', fakeMessage(fakeChannel('C14c'), bob), null));
  assert.ok(rows.has('G14:B14'));
});

test('a mention of an AFK member gets one reply with the reason, no pings, and records the ping', async () => {
  const ch = fakeChannel('C15');
  const amy = fakeMember('A15', { nickname: '[AFK] Amy' });
  const bob = fakeMember('B15');
  seed('G15', 'A15', { reason: 'at the gym @everyone <@123>', since: ago(20 * MINUTE) });
  forgetAfkIds('G15');

  const msg = fakeMessage(ch, bob, { mentions: [amy] });
  await handleMessage(ctxFor('G15', msg));
  assert.equal(msg.replies.length, 1);
  const { content, allowedMentions, flags } = msg.replies[0];
  assert.match(content, /^💤 \*\*Amy\*\* is AFK: at the gym .* \(since <t:\d+:R>\)$/);
  assert.ok(!content.includes('@everyone') && !content.includes('<@123>'), 'the reason cannot ping or highlight');
  assert.deepEqual(allowedMentions, { parse: [], repliedUser: false });
  assert.ok(flags, 'embeds suppressed');

  const row = rows.get('G15:A15');
  assert.equal(row.pingCount, 1);
  assert.deepEqual(row.pings.map((p) => [p.channelId, p.messageId, p.authorId]), [['C15', msg.id, 'B15']]);
});

test('the same AFK member is announced once a minute per channel, but every ping is recorded', async () => {
  const chA = fakeChannel('C16a');
  const chB = fakeChannel('C16b');
  const amy = fakeMember('A16');
  const bob = fakeMember('B16');
  seed('G16', 'A16', { reason: 'out' });
  forgetAfkIds('G16');

  const first = fakeMessage(chA, bob, { mentions: [amy] });
  const second = fakeMessage(chA, bob, { mentions: [amy] });
  const otherChannel = fakeMessage(chB, bob, { mentions: [amy] });
  for (const m of [first, second, otherChannel]) await handleMessage(ctxFor('G16', m));
  assert.equal(first.replies.length, 1);
  assert.equal(second.replies.length, 0, 'throttled in the same channel');
  assert.equal(otherChannel.replies.length, 1, 'another channel has its own throttle');
  assert.equal(rows.get('G16:A16').pingCount, 3, 'all counted');
});

test('only the last AFK_PINGS_KEPT pings are kept, the count covers all', async () => {
  const ch = fakeChannel('C17');
  const amy = fakeMember('A17');
  const bob = fakeMember('B17');
  seed('G17', 'A17');
  forgetAfkIds('G17');
  const total = LIMITS.AFK_PINGS_KEPT + 3;
  const ids = [];
  for (let i = 0; i < total; i++) {
    const m = fakeMessage(ch, bob, { mentions: [amy] });
    ids.push(m.id);
    await handleMessage(ctxFor('G17', m));
  }
  const row = rows.get('G17:A17');
  assert.equal(row.pingCount, total);
  assert.equal(row.pings.length, LIMITS.AFK_PINGS_KEPT);
  assert.deepEqual(row.pings.map((p) => p.messageId), ids.slice(-LIMITS.AFK_PINGS_KEPT));
});

test('mentioning yourself while AFK is not announced, and at most 5 AFK members are', async () => {
  const ch = fakeChannel('C18');
  const author = fakeMember('B18');
  seed('G18', 'B18', { since: ago(2_000) }); // within the grace period: still AFK
  const afkMembers = Array.from({ length: 7 }, (_, i) => fakeMember(`A18${i}`));
  afkMembers.forEach((m, i) => seed('G18', m.id, { reason: `r${i}` }));
  forgetAfkIds('G18');

  const selfOnly = fakeMessage(ch, author, { mentions: [author] });
  await handleMessage(ctxFor('G18', selfOnly));
  assert.equal(selfOnly.replies.length, 0);

  const many = fakeMessage(ch, author, { mentions: [author, ...afkMembers] });
  await handleMessage(ctxFor('G18', many));
  assert.equal(many.replies.length, 1);
  assert.equal(many.replies[0].content.split('\n').length, AFK.MAX_NOTICES);
  assert.equal(afkMembers.filter((m) => rows.get(`G18:${m.id}`).pingCount === 1).length, AFK.MAX_NOTICES, 'only the announced five are recorded');
});

test('an AFK author who mentions another AFK member returns and announces', async () => {
  const ch = fakeChannel('C19');
  const bob = fakeMember('B19');
  const amy = fakeMember('A19');
  seed('G19', 'B19');
  seed('G19', 'A19', { reason: 'out' });
  forgetAfkIds('G19');
  const msg = fakeMessage(ch, bob, { mentions: [amy] });
  await handleMessage(ctxFor('G19', msg), { welcomeDeleteMs: 0 });
  assert.equal(rows.has('G19:B19'), false);
  assert.ok(rows.has('G19:A19'));
  assert.equal(msg.replies.length, 2);
  assert.match(msg.replies[0].content, /Welcome back/);
  assert.match(msg.replies[1].content, /is AFK: out/);
});

test('chat from members who are not AFK never queries the database once the ids are known', async () => {
  const ch = fakeChannel('C20');
  const bob = fakeMember('B20');
  seed('G20', 'A20');
  forgetAfkIds('G20');
  await handleMessage(ctxFor('G20', fakeMessage(ch, bob))); // loads the ids
  const before = { ...calls };
  for (let i = 0; i < 5; i++) {
    const msg = fakeMessage(ch, bob, { mentions: [fakeMember('K20')] });
    assert.equal(mayConcernAfk(ctxFor('G20', msg)), false);
    await handleMessage(ctxFor('G20', msg));
  }
  assert.deepEqual(calls, before);
});

test('a nickname the member changed themselves is left alone, and nothing is touched when the bot set none', async () => {
  const ch = fakeChannel('C21');
  const renamed = fakeMember('B21', { nickname: 'Robert' });
  const manual = fakeMember('B21b', { nickname: '[AFK] Manual' });
  seed('G21', 'B21', { nickChanged: true });
  seed('G21', 'B21b', { nickChanged: false });
  forgetAfkIds('G21');
  await handleMessage(ctxFor('G21', fakeMessage(ch, renamed)), { welcomeDeleteMs: 0 });
  await handleMessage(ctxFor('G21', fakeMessage(ch, manual)), { welcomeDeleteMs: 0 });
  assert.deepEqual(renamed.nicks, []);
  assert.deepEqual(manual.nicks, [], 'the bot did not add that prefix, so it does not remove it');

  const natural = fakeMember('B21c', { nickname: '[AFK] Cat', globalName: 'Cat' });
  seed('G21', 'B21c', { nickChanged: true });
  forgetAfkIds('G21');
  await handleMessage(ctxFor('G21', fakeMessage(ch, natural)), { welcomeDeleteMs: 0 });
  assert.deepEqual(natural.nicks, [null], 'the nickname is cleared when only the account name was behind the prefix');
});

test('without Send Messages the ping is still recorded but nothing is sent', async () => {
  const ch = fakeChannel('C22', { allowed: false });
  const amy = fakeMember('A22');
  const bob = fakeMember('B22');
  seed('G22', 'A22', { reason: 'out' });
  forgetAfkIds('G22');
  const msg = fakeMessage(ch, bob, { mentions: [amy] });
  await handleMessage(ctxFor('G22', msg));
  assert.equal(msg.replies.length, 0);
  assert.equal(rows.get('G22:A22').pingCount, 1);
});

test('a reply that fails (message gone) does not throw', async () => {
  const ch = fakeChannel('C23');
  const amy = fakeMember('A23');
  const bob = fakeMember('B23');
  seed('G23', 'A23', { reason: 'out' });
  forgetAfkIds('G23');
  const msg = fakeMessage(ch, bob, { mentions: [amy] });
  msg.reply = async () => { const e = new Error('Unknown Message'); e.code = 10008; throw e; };
  await handleMessage(ctxFor('G23', msg));
  assert.equal(rows.get('G23:A23').pingCount, 1);
});

// ── Declarations ──────────────────────────────────────────────────────────────

test('plugin declaration and hook order', async () => {
  assert.equal(plugin.id, 'afk');
  assert.equal(plugin.module, 'afk');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id]), [['messageCreate', 'afk']]);
  assert.ok(!plugin.hooks[0].always, 'gated by the module');
  assert.equal(typeof plugin.hooks[0].filter, 'function');
  assert.deepEqual(plugin.components, []);
  assert.deepEqual(plugin.jobs, []);
  assert.ok(HOOK_ORDER.messageCreate.indexOf('afk') > HOOK_ORDER.messageCreate.indexOf('automod'), 'after automod');
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'afk'), 'collectPlugins keeps it');
  assert.deepEqual(errors.filter((e) => String(e?.message ?? e).includes('afk')), []);
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/afk payload and permissions', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'afk');
  assert.equal(json.default_member_permissions ?? null, null, 'every member may use it');
  assert.deepEqual(json.contexts, [0]);
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['set', 1], ['clear', 1], ['remove', 1], ['list', 1]]);
  const sub = Object.fromEntries(json.options.map((o) => [o.name, o]));
  assert.deepEqual(sub.set.options.map((o) => [o.name, o.type, !!o.required, o.max_length]), [['reason', 3, false, LIMITS.AFK_REASON_LENGTH]]);
  assert.deepEqual(sub.remove.options.map((o) => [o.name, o.type, !!o.required]), [['user', 6, true]]);
  assert.deepEqual(sub.clear.options ?? [], []);
  assert.deepEqual(sub.list.options ?? [], []);
  assert.ok(charCount(json) <= 800, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'afk');
  assert.equal(cmd.gated, true);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.deepEqual({ ...cmd.subcommandPermissions }, { remove: PERMISSION_LEVELS.MODERATOR, list: PERMISSION_LEVELS.MODERATOR });
});

// ── /afk ──────────────────────────────────────────────────────────────────────

function fakeInteraction({ guildId, member, sub, options = {} }) {
  const i = {
    guildId, user: member.user, member, deferred: false, replied: false, ephemeral: undefined, edits: [],
    guild: { id: guildId, members: { cache: new Map(), fetch: async () => null } },
    inCachedGuild: () => true,
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => null,
      getString: (n) => options[n] ?? null,
      getUser: (n) => options[n] ?? null,
    },
    async deferReply({ ephemeral } = {}) { i.deferred = true; i.ephemeral = Boolean(ephemeral); },
    async editReply(p) { i.edits.push(p); return { id: 'm' }; },
    async followUp(p) { i.edits.push(p); return { id: 'm' }; },
    async reply(p) { i.replied = true; i.edits.push(p); return {}; },
  };
  return i;
}
const desc = (i) => i.edits.at(-1).embeds[0].toJSON().description ?? '';
const title = (i) => i.edits.at(-1).embeds[0].toJSON().title ?? '';

async function seedSettings(guildId, extra = {}) {
  const upsert = prisma.guildSettings.upsert;
  prisma.guildSettings.upsert = async ({ where, update }) => ({ guildId: where.guildId, ...update });
  try {
    await updateSettings(guildId, { afkEnabled: true, afkNicknamePrefix: true, ...extra });
  } finally {
    prisma.guildSettings.upsert = upsert;
  }
}

test('/afk set marks the member AFK, adds the prefix and keeps the reply private', async () => {
  await seedSettings('G30');
  forgetAfkIds('G30');
  const bob = fakeMember('B30', { username: 'bob' });
  const i = fakeInteraction({ guildId: 'G30', member: bob, sub: 'set', options: { reason: ' at the\ngym ' } });
  await cmd.execute(i);
  assert.equal(i.ephemeral, true);
  assert.match(desc(i), /You are now AFK: at the gym/);
  assert.deepEqual(bob.nicks, ['[AFK] bob']);
  const row = rows.get('G30:B30');
  assert.equal(row.reason, 'at the gym');
  assert.equal(row.nickChanged, true);
  assert.deepEqual(i.edits.at(-1).allowedMentions, { parse: [] });
});

test('/afk set without the nickname setting, or when the bot cannot rename, still works and says why', async () => {
  await seedSettings('G31', { afkNicknamePrefix: false });
  const off = fakeMember('B31', { username: 'bob' });
  const i1 = fakeInteraction({ guildId: 'G31', member: off, sub: 'set' });
  await cmd.execute(i1);
  assert.deepEqual(off.nicks, []);
  assert.equal(rows.get('G31:B31').nickChanged, false);
  assert.ok(!desc(i1).includes("couldn't"));

  await seedSettings('G32', { afkNicknamePrefix: true });
  const stuck = fakeMember('B32', { username: 'bob', manageable: false });
  const i2 = fakeInteraction({ guildId: 'G32', member: stuck, sub: 'set', options: { reason: 'brb' } });
  await cmd.execute(i2);
  assert.deepEqual(stuck.nicks, []);
  assert.equal(rows.get('G32:B32').nickChanged, false);
  assert.match(desc(i2), /Manage Nicknames and a role above yours/);

  const noPerm = fakeMember('B32b', { username: 'bob', canNick: false });
  const i3 = fakeInteraction({ guildId: 'G32', member: noPerm, sub: 'set' });
  await cmd.execute(i3);
  assert.deepEqual(noPerm.nicks, []);
  assert.match(desc(i3), /Manage Nicknames/);
});

test('/afk clear ends the AFK and takes the prefix off; nobody AFK gets a note', async () => {
  await seedSettings('G33');
  const bob = fakeMember('B33', { nickname: '[AFK] Bob' });
  seed('G33', 'B33', { nickChanged: true, previousNick: 'Bob', since: ago(90 * MINUTE) });
  const i = fakeInteraction({ guildId: 'G33', member: bob, sub: 'clear' });
  await cmd.execute(i);
  assert.match(title(i), /Welcome back/);
  assert.match(desc(i), /1h 30m/);
  assert.equal(rows.has('G33:B33'), false);
  assert.deepEqual(bob.nicks, ['Bob']);

  const again = fakeInteraction({ guildId: 'G33', member: bob, sub: 'clear' });
  await cmd.execute(again);
  assert.match(desc(again), /not AFK/);
});

test('/afk remove ends another member\'s AFK and strips the prefix we added', async () => {
  await seedSettings('G34');
  const mod = fakeMember('M34');
  const amy = fakeMember('A34', { nickname: '[AFK] Amy' });
  seed('G34', 'A34', { nickChanged: true, previousNick: 'Amy' });
  const i = fakeInteraction({ guildId: 'G34', member: mod, sub: 'remove', options: { user: amy.user } });
  i.guild.members.cache.set('A34', amy);
  await cmd.execute(i);
  assert.match(desc(i), /<@A34> is no longer AFK/);
  assert.equal(rows.has('G34:A34'), false);
  assert.deepEqual(amy.nicks, ['Amy']);

  const none = fakeInteraction({ guildId: 'G34', member: mod, sub: 'remove', options: { user: amy.user } });
  await cmd.execute(none);
  assert.match(desc(none), /is not AFK/);
});

test('/afk list shows who is AFK, longest first, with the total', async () => {
  await seedSettings('G35');
  const mod = fakeMember('M35');
  const empty = fakeInteraction({ guildId: 'G35', member: mod, sub: 'list' });
  await cmd.execute(empty);
  assert.match(desc(empty), /Nobody is AFK/);

  for (let i = 0; i < AFK.LIST_MAX + 2; i++) seed('G35', `U35${String(i).padStart(2, '0')}`, { reason: i === 0 ? 'out @everyone' : null, since: ago((100 - i) * MINUTE) });
  const i = fakeInteraction({ guildId: 'G35', member: mod, sub: 'list' });
  await cmd.execute(i);
  assert.match(title(i), new RegExp(`\\(${AFK.LIST_MAX + 2}\\)`));
  const lines = desc(i).split('\n');
  assert.equal(lines.filter((l) => l.startsWith('<@')).length, AFK.LIST_MAX);
  assert.match(lines[0], /^<@U3500>: out @\u200beveryone \(since <t:\d+:R>\)$/, 'longest AFK first, reason defanged');
  assert.match(lines.at(-1), /… and 2 more/);
});

test('/afk set that cannot save puts the nickname back', async () => {
  await seedSettings('G36');
  const bob = fakeMember('B36', { username: 'bob' });
  const upsert = prisma.afkStatus.upsert;
  prisma.afkStatus.upsert = async () => { throw new Error('database down'); };
  const i = fakeInteraction({ guildId: 'G36', member: bob, sub: 'set', options: { reason: 'brb' } });
  try {
    await cmd.execute(i);
  } finally {
    prisma.afkStatus.upsert = upsert;
  }
  assert.deepEqual(bob.nicks, ['[AFK] bob', null], 'the prefix went on and came off again');
  assert.equal(rows.has('G36:B36'), false);
  assert.match(title(i), /Something went wrong/);
});

test('a nickname too long for the prefix comes back whole when the member writes', async () => {
  await seedSettings('G37');
  forgetAfkIds('G37');
  const long = 'L'.repeat(AFK.NICKNAME_MAX);
  const lee = fakeMember('B37', { nickname: long, username: 'lee' });
  await cmd.execute(fakeInteraction({ guildId: 'G37', member: lee, sub: 'set' }));
  assert.equal(lee.nickname, AFK.PREFIX + 'L'.repeat(AFK.NICKNAME_MAX - AFK.PREFIX.length), 'cut to fit the prefix while AFK');
  assert.equal(rows.get('G37:B37').previousNick, long);

  rows.get('G37:B37').since = ago(5 * MINUTE);
  await handleMessage(ctxFor('G37', fakeMessage(fakeChannel('C37'), lee)), { welcomeDeleteMs: 0 });
  assert.equal(rows.has('G37:B37'), false);
  assert.equal(lee.nickname, long, 'the whole nickname, not the cut one');
  assert.equal(lee.nicks.length, 2);
});

test('a member with no nickname gets none back after /afk clear, and a second /afk set keeps the saved nickname', async () => {
  await seedSettings('G38');
  forgetAfkIds('G38');
  const bob = fakeMember('B38', { username: 'bob' });
  await cmd.execute(fakeInteraction({ guildId: 'G38', member: bob, sub: 'set' }));
  assert.equal(bob.nickname, '[AFK] bob');
  assert.equal(rows.get('G38:B38').previousNick, null);
  await cmd.execute(fakeInteraction({ guildId: 'G38', member: bob, sub: 'clear' }));
  assert.equal(bob.nickname, null, 'cleared: Discord shows the account name again');
  assert.deepEqual(bob.nicks, ['[AFK] bob', null]);

  const long = 'M'.repeat(AFK.NICKNAME_MAX);
  const amy = fakeMember('A38', { nickname: long, username: 'amy' });
  const mod = fakeMember('M38');
  await cmd.execute(fakeInteraction({ guildId: 'G38', member: amy, sub: 'set', options: { reason: 'one' } }));
  await cmd.execute(fakeInteraction({ guildId: 'G38', member: amy, sub: 'set', options: { reason: 'two' } }));
  assert.equal(rows.get('G38:A38').reason, 'two');
  assert.equal(rows.get('G38:A38').nickChanged, true);
  assert.equal(rows.get('G38:A38').previousNick, long, 'the second set did not save the prefixed nickname over it');
  assert.equal(amy.nicks.length, 1, 'and did not prefix it again');

  const remove = fakeInteraction({ guildId: 'G38', member: mod, sub: 'remove', options: { user: amy.user } });
  remove.guild.members.cache.set('A38', amy);
  await cmd.execute(remove);
  assert.equal(amy.nickname, long);
});
