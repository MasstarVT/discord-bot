// Review regressions for the community2 area (giveaways, polls, birthdays, reminders, tickets). Offline: prisma methods are
// replaced on the shared client for the duration of a test and every Discord object is a small fake.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const { ChannelType } = await import('discord.js');
const svc = await import('../src/services/giveawayService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const sid = (prefix, n) => `${prefix}${String(n).padStart(18 - prefix.length, '0')}`; // 18-digit snowflake-like id
const uid = (n) => sid('7', n);

/** Replaces prisma delegate methods and returns a function that puts the originals back. */
function patch(table) {
  const saved = [];
  for (const [delegate, methods] of Object.entries(table)) {
    for (const [name, fn] of Object.entries(methods)) {
      saved.push([prisma[delegate], name, prisma[delegate][name]]);
      prisma[delegate][name] = fn;
    }
  }
  return () => { for (const [obj, name, fn] of saved) obj[name] = fn; };
}

// ── giveaways: announcement length ───────────────────────────────────────────

test('announceWinners keeps the first message within 2000 characters with 90 winners, a 200 character prize and both bonus prizes', async () => {
  const sent = [];
  const channel = { id: 'c', async send(payload) { sent.push(payload); return { id: 'm' }; } };
  const winners = Array.from({ length: 90 }, (_, i) => uid(i + 1));
  const g = { id: 'g1', messageId: sid('5', 1), prize: 'p'.repeat(200), xpPrize: 1_000_000, coinPrize: 1_000_000 };
  const ok = await svc.announceWinners(channel, g, winners, { settings: { currencyEmoji: '🪙', currencyName: 'coins' } });
  assert.equal(ok, true);
  assert.ok(sent.length >= 2);
  for (const p of sent) assert.ok(p.content.length <= 2000, `message of ${p.content.length} characters`);
  const mentioned = sent.flatMap((p) => [...p.content.matchAll(/<@(\d+)>/g)].map((m) => m[1]));
  assert.deepEqual(mentioned, winners);
  for (const p of sent) assert.deepEqual(p.allowedMentions.parse, []);
});

// ── giveaways: a winner who leaves during a manual end ───────────────────────

function endFixture() {
  const guildId = sid('9', 1);
  const members = [uid(1), uid(2), uid(3)].map((id) => ({ id, user: { id, bot: false }, partial: false, roles: { cache: new Set() } }));
  const channelId = sid('5', 2);
  const messageId = sid('5', 3);
  const edits = [];
  const sent = [];
  const message = { id: messageId };
  const channel = {
    id: channelId,
    name: 'giveaways',
    type: ChannelType.GuildText,
    isTextBased: () => true,
    isThread: () => false,
    permissionsFor: () => ({ has: () => true }),
    async send(payload) { sent.push(payload); return { id: sid('5', 4) }; },
    messages: {
      async fetch() { return message; },
      async edit(id, payload) { edits.push({ id, ...payload }); return message; },
    },
  };
  const guild = {
    id: guildId,
    available: true,
    roles: { cache: new Map() },
    channels: { cache: new Map([[channelId, channel]]), async fetch() { throw Object.assign(new Error('Unknown Channel'), { code: 10003 }); } },
    members: { cache: new Map(members.map((m) => [m.id, m])), me: { id: sid('6', 1) }, async fetch() { return new Map(); } },
  };
  channel.guild = guild;
  const row = {
    id: 'cgiveaway0000000000000001',
    guildId,
    channelId,
    messageId,
    hostId: uid(999),
    prize: 'Prize',
    description: null,
    winnerCount: 1,
    requiredRoleIds: [],
    deniedRoleIds: [],
    roleWeights: [],
    xpPrize: null,
    coinPrize: null,
    pingRoleId: null,
    embedColor: null,
    endsAt: new Date(Date.now() + 3_600_000),
    endedAt: null,
    status: 'ACTIVE',
    winnerIds: [],
    createdAt: new Date(),
    updatedAt: new Date(),
  };
  return { guild, channel, row, members, sent, edits, client: { guilds: { cache: new Map([[guildId, guild]]) }, user: { id: sid('6', 1) } } };
}

test('endGiveaway replaces a winner who clicked Leave while the draw was running', async () => {
  const env = endFixture();
  const everyone = env.members.map((m) => m.id);
  let leaver = null;
  let entryReads = 0;
  const claims = [];
  const restore = patch({
    giveaway: {
      findUnique: async () => env.row,
      updateMany: async (args) => { claims.push(args); return { count: 1 }; },
    },
    giveawayEntry: {
      findMany: async (args) => {
        entryReads++;
        const wanted = args.where.userId?.in;
        if (wanted) {
          // The presence check of the drawn winners: the first of them has left by now.
          leaver = wanted[0];
          return wanted.filter((id) => id !== leaver).map((userId) => ({ userId }));
        }
        return everyone.filter((id) => id !== leaver).map((userId) => ({ userId }));
      },
    },
    scheduledJob: { deleteMany: async () => ({ count: 0 }) },
    guildSettings: { findUnique: async () => { throw new Error('offline'); } },
  });
  try {
    const res = await svc.endGiveaway({ client: env.client, guild: env.guild, giveawayId: env.row.id, source: 'command' });
    assert.equal(res.ok, true);
    assert.ok(leaver, 'the presence check ran');
    assert.equal(res.winnerIds.length, 1, 'the departed winner is replaced, not just dropped');
    assert.ok(!res.winnerIds.includes(leaver));
    assert.ok(everyone.includes(res.winnerIds[0]));
    assert.deepEqual(claims[0].data.winnerIds, res.winnerIds, 'the stored winners are the replaced ones');
    const announced = env.sent.map((p) => p.content).join('\n');
    assert.ok(announced.includes(`<@${res.winnerIds[0]}>`));
    assert.ok(!announced.includes(`<@${leaver}>`));
    assert.equal(entryReads, 3, 'entries, presence check, fresh entries');
  } finally {
    restore();
  }
});

test('endGiveaway does not re-read entries when every winner is still in', async () => {
  const env = endFixture();
  const everyone = env.members.map((m) => m.id);
  const reads = [];
  const restore = patch({
    giveaway: { findUnique: async () => env.row, updateMany: async () => ({ count: 1 }) },
    giveawayEntry: {
      findMany: async (args) => {
        reads.push(args.where.userId ? 'presence' : 'all');
        const wanted = args.where.userId?.in;
        return (wanted ?? everyone).map((userId) => ({ userId }));
      },
    },
    scheduledJob: { deleteMany: async () => ({ count: 0 }) },
    guildSettings: { findUnique: async () => { throw new Error('offline'); } },
  });
  try {
    const res = await svc.endGiveaway({ client: env.client, guild: env.guild, giveawayId: env.row.id, source: 'command' });
    assert.equal(res.ok, true);
    assert.equal(res.winnerIds.length, 1);
    assert.deepEqual(reads, ['all', 'presence']);
  } finally {
    restore();
  }
});

// ── giveaways: a join that raced the end is never drawn, not even on a reroll ─────────────────────

test('endGiveaway reads the entries up to endedAt and stores that same endedAt', async () => {
  const env = endFixture();
  const reads = [];
  const claims = [];
  const restore = patch({
    giveaway: { findUnique: async () => env.row, updateMany: async (args) => { claims.push(args); return { count: 1 }; } },
    giveawayEntry: {
      findMany: async (args) => {
        reads.push(args);
        return (args.where.userId?.in ?? env.members.map((m) => m.id)).map((userId) => ({ userId }));
      },
    },
    scheduledJob: { deleteMany: async () => ({ count: 0 }) },
    guildSettings: { findUnique: async () => { throw new Error('offline'); } },
  });
  try {
    const res = await svc.endGiveaway({ client: env.client, guild: env.guild, giveawayId: env.row.id, source: 'command' });
    assert.equal(res.ok, true);
    const cutoff = reads[0].where.createdAt?.lte;
    assert.ok(cutoff instanceof Date, 'the draw only counts entries up to a cut-off');
    assert.equal(claims[0].data.endedAt.getTime(), cutoff.getTime(), 'the stored endedAt is that cut-off');
  } finally {
    restore();
  }
});

test('rerollGiveaway ignores an entry created after endedAt', async () => {
  const env = endFixture();
  const [first, second, late] = env.members.map((m) => m.id);
  const endedAt = new Date(Date.now() - 60_000);
  const at = (ms) => new Date(endedAt.getTime() + ms);
  const entries = [
    { userId: first, createdAt: at(-5000) }, // the winner
    { userId: second, createdAt: at(-3000) },
    { userId: late, createdAt: at(2000) }, // clicked just before the end claim, entered just after it
  ];
  env.row.status = 'ENDED';
  env.row.endedAt = endedAt;
  env.row.winnerIds = [first];
  env.row.winnerCount = 1;
  const wheres = [];
  const restore = patch({
    giveaway: { findUnique: async () => env.row, updateMany: async () => ({ count: 1 }) },
    giveawayEntry: {
      findMany: async (args) => {
        wheres.push(args.where);
        const lte = args.where.createdAt?.lte;
        return entries.filter((e) => !lte || e.createdAt <= lte).map((e) => ({ userId: e.userId }));
      },
    },
    guildSettings: { findUnique: async () => { throw new Error('offline'); } },
  });
  try {
    const res = await svc.rerollGiveaway({ guild: env.guild, giveawayId: env.row.id, count: 5 });
    assert.equal(res.ok, true);
    assert.deepEqual(res.newWinnerIds, [second], 'the late entry cannot win a reroll');
    assert.deepEqual(wheres[0].createdAt, { lte: endedAt });

    // An ended row without endedAt (older data) is not filtered.
    wheres.length = 0;
    env.row.endedAt = null;
    env.row.winnerIds = [first, second];
    const res2 = await svc.rerollGiveaway({ guild: env.guild, giveawayId: env.row.id, count: 5 });
    assert.deepEqual(res2.newWinnerIds, [late]);
    assert.equal(wheres[0].createdAt, undefined);
  } finally {
    restore();
  }
});

// ── polls: percentages that add up ───────────────────────────────────────────────────────────────

const polls = await import('../src/services/pollService.js');
const dashPolls = await import('../src/dashboard/collections/polls.js');

test('percentages: largest remainder, always 100 once anyone voted, all 0 otherwise', () => {
  assert.deepEqual(polls.percentages([1, 1, 1]), [34, 33, 33]);
  assert.deepEqual(polls.percentages([1, 2]), [33, 67]);
  assert.deepEqual(polls.percentages([2, 1, 1]), [50, 25, 25]);
  assert.equal(polls.percentages([1, 1, 1, 1, 1, 1, 1]).reduce((a, b) => a + b, 0), 100);
  assert.deepEqual(polls.percentages([0, 0, 0]), [0, 0, 0]);
  assert.deepEqual(polls.percentages([]), []);
  assert.deepEqual(polls.percentages([5, 0]), [100, 0]);
  // What per-option rounding gave: 99 and 102.
  assert.equal([1, 1, 1].map((c) => polls.percent(c, 3)).reduce((a, b) => a + b, 0), 99);
  assert.equal([1, 1, 1, 1, 1, 1].map((c) => polls.percent(c, 6)).reduce((a, b) => a + b, 0), 102);
  let seed = 7;
  const rnd = (n) => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed % n; };
  for (let t = 0; t < 500; t += 1) {
    const counts = Array.from({ length: 2 + rnd(9) }, () => rnd(40));
    const out = polls.percentages(counts);
    const sum = counts.reduce((a, b) => a + b, 0);
    assert.equal(out.reduce((a, b) => a + b, 0), sum === 0 ? 0 : 100, JSON.stringify(counts));
    counts.forEach((c, i) => {
      if (!sum) return;
      assert.ok(out[i] === Math.floor((c * 100) / sum) || out[i] === Math.ceil((c * 100) / sum), `${c}/${sum} -> ${out[i]}`);
    });
  }
});

test('poll results show percentages that add up to 100, with bars and the winner line from the same numbers', () => {
  const options = ['A', 'B', 'C'];
  const tally = { counts: [1, 1, 1], votes: 3, voters: 3 };
  const text = polls.formatOptionLines(options, tally, { ended: true });
  const shown = [...text.matchAll(/\*\*(\d+)%\*\*/g)].map((m) => Number(m[1]));
  assert.deepEqual(shown, [34, 33, 33]);
  const bars = [...text.matchAll(/([▰▱]+) \*\*\d+%\*\*/g)].map((m) => (m[1].match(/▰/g) ?? []).length);
  assert.deepEqual(bars, shown.map((p) => Math.round((p / 100) * 10)), 'each bar is drawn from the number beside it');

  // votes above the sum of the counts (two reads that are not one snapshot) still add up.
  const skewed = polls.formatOptionLines(options, { counts: [1, 1, 1], votes: 9, voters: 3 }, {});
  assert.deepEqual([...skewed.matchAll(/\*\*(\d+)%\*\*/g)].map((m) => Number(m[1])), [34, 33, 33]);

  const none = polls.formatOptionLines(options, { counts: [0, 0, 0], votes: 0, voters: 0 }, {});
  assert.deepEqual([...none.matchAll(/\*\*(\d+)%\*\*/g)].map((m) => Number(m[1])), [0, 0, 0]);

  // The winner line quotes the same number as the option line.
  const poll = { id: 'p', question: 'Q?', options, maxChoices: 1, hideResults: false, status: 'ENDED', endsAt: null, messageId: null, authorId: '1', createdAt: new Date() };
  const win = polls.buildResultsEmbed(poll, { counts: [2, 1, 1], votes: 4, voters: 4 }, {}).data.description;
  assert.match(win, /won with \*\*2\*\* votes \(50%\)/);
  assert.match(win, /\*\*1\.\*\* 🏆 A\n▰{5}▱{5} \*\*50%\*\*/);
  const tie = polls.buildResultsEmbed(poll, { counts: [2, 2, 1], votes: 5, voters: 5 }, {}).data.description;
  assert.match(tie, /Tie between/);
});

test('the dashboard poll results add up to 100 as well', () => {
  const row = { id: 'p', guildId: '1', channelId: '2', messageId: null, question: 'Q', options: ['A', 'B', 'C'], maxChoices: 1, status: 'ENDED', hideResults: false, allowedRoleIds: [], deniedRoleIds: [], authorId: '3', endsAt: null, createdAt: new Date() };
  const item = dashPolls.pollItem(row, { tally: { counts: [1, 1, 1], votes: 3, voters: 3 } });
  assert.deepEqual(item.results.map((r) => Number(/\((\d+)%\)/.exec(r)[1])), [34, 33, 33]);
  const lead = dashPolls.pollItem(row, { tally: { counts: [2, 1, 0], votes: 3, voters: 3 } });
  assert.match(lead.summary, /won: A \(67%\)/);
  assert.deepEqual(lead.results.map((r) => Number(/\((\d+)%\)/.exec(r)[1])), [67, 33, 0]);
});

// ── reminders: transient send failures are retried through the scheduler ─────────────────────────

const rem = await import('../src/services/reminderService.js');
const { DiscordAPIError } = await import('discord.js');
const { TimeoutError } = await import('../src/utils/async.js');

const apiError = (code, status = 400) => new DiscordAPIError({ message: `err ${code}`, code }, code, status, 'POST', 'https://discord.test/api', {});

test('isTransientSendError: 5xx, 429, timeouts and network errors retry; permission and unknown-target codes do not', () => {
  assert.equal(rem.isTransientSendError(apiError(0, 500)), true);
  assert.equal(rem.isTransientSendError(apiError(0, 503)), true);
  assert.equal(rem.isTransientSendError(apiError(0, 429)), true);
  assert.equal(rem.isTransientSendError(new TimeoutError('send')), true);
  assert.equal(rem.isTransientSendError(Object.assign(new Error('socket hang up'), { code: 'ECONNRESET' })), true);
  assert.equal(rem.isTransientSendError(new Error('fetch failed')), true);
  for (const code of [50001, 50013, 10003, 10013, 50007, 50035]) assert.equal(rem.isTransientSendError(apiError(code, 403)), false, `code ${code}`);
  assert.equal(rem.isTransientSendError(apiError(0, 404)), false);
});

test('mayRetry: never on the final attempt, never without scheduler info, only while the backoff fits the tolerance', () => {
  const tol = 15 * 60_000;
  assert.equal(rem.mayRetry({ attempt: 1, maxAttempts: 10, lateMs: 0, toleranceMs: tol }), true);
  assert.equal(rem.mayRetry({ attempt: 10, maxAttempts: 10, lateMs: 0, toleranceMs: tol }), false, 'the final attempt');
  assert.equal(rem.mayRetry({ attempt: undefined, maxAttempts: undefined, lateMs: 0, toleranceMs: tol }), false);
  assert.equal(rem.schedulerRetryDelayMs(1), 30_000);
  assert.equal(rem.schedulerRetryDelayMs(3), 120_000);
  assert.equal(rem.schedulerRetryDelayMs(20), 3_600_000);
  assert.equal(rem.mayRetry({ attempt: 5, maxAttempts: 10, lateMs: 0, toleranceMs: tol }), true, '480 s fits 15 min');
  assert.equal(rem.mayRetry({ attempt: 5, maxAttempts: 10, lateMs: 8 * 60_000, toleranceMs: tol }), false, '8 + 8 min does not');
  assert.equal(rem.occurrenceNonce('c1', new Date(5)).length <= 25, true);
  assert.equal(rem.occurrenceNonce('c1', new Date(5)), rem.occurrenceNonce('c1', new Date(5)));
  assert.notEqual(rem.occurrenceNonce('c1', new Date(5)), rem.occurrenceNonce('c1', new Date(6)));
});

let remSeq = 0;
function reminderWorld({ mode = 'ONE_SHOT', onSend } = {}) {
  remSeq += 1;
  const guildId = sid('8', 100 + remSeq);
  const channelId = sid('5', 100 + remSeq);
  const sent = [];
  const attempts = [];
  const channel = {
    id: channelId, name: 'general', type: ChannelType.GuildText,
    isTextBased: () => true, isThread: () => false,
    permissionsFor: () => ({ has: () => true }),
    lastMessageId: null, lastMessage: null,
    async send(payload) {
      attempts.push(payload);
      if (onSend) await onSend(payload, attempts.length);
      sent.push(payload);
      return { id: sid('9', 100 + sent.length) };
    },
  };
  const guild = {
    id: guildId, name: 'Guild', available: true, ownerId: '1', memberCount: 3, iconURL: () => null,
    roles: { cache: new Map() }, members: { me: { id: sid('6', 1) } },
    channels: { cache: new Map([[channelId, channel]]), fetch: async () => { throw apiError(10003); } },
  };
  channel.guild = guild;
  const base = {
    id: `crem${String(remSeq).padStart(21, '0')}`, guildId, name: 'standup', channelId, enabled: true, pingRoleId: null,
    template: { content: 'hi', embed: null, buttons: [] }, requireActivity: false, lastSentAt: null, lastMessageId: null,
    weekdays: [], timeOfDay: null, windowStart: null, windowEnd: null, createdBy: '1', updatedAt: new Date(),
  };
  const row = mode === 'ONE_SHOT'
    ? { ...base, mode, runAt: new Date(Date.now() - 5000), intervalMinutes: null, createdAt: new Date(Date.now() - 3_600_000) }
    : { ...base, mode, runAt: null, intervalMinutes: 5, createdAt: new Date(Date.now() - 10 * 60_000 - 20_000) }; // a grid slot 20 s ago
  const restore = patch({
    reminder: {
      findFirst: async () => ({ ...row }),
      update: async ({ data }) => Object.assign(row, data),
      updateMany: async ({ where, data }) => {
        if (where.enabled === true && !row.enabled) return { count: 0 };
        if (where.OR) {
          const lt = where.OR[1].lastSentAt.lt;
          if (!(row.lastSentAt == null || row.lastSentAt < lt)) return { count: 0 };
        } else if (where.lastSentAt instanceof Date && row.lastSentAt?.getTime() !== where.lastSentAt.getTime()) {
          return { count: 0 };
        }
        Object.assign(row, data);
        return { count: 1 };
      },
    },
    guildSettings: { findUnique: async () => ({ guildId, remindersEnabled: true, timezone: 'UTC', embedColor: null }) },
  });
  const fire = (attempt, maxAttempts = 10) => rem.fireReminder(
    { guildId, maxAttempts, payload: { reminderId: row.id } }, { client: {}, guild, attempt },
  );
  return { row, sent, attempts, fire, restore, guild, channel };
}

test('a ONE_SHOT whose send fails transiently is retried by the scheduler and sent once, with the same nonce', async () => {
  const w = reminderWorld({ onSend: async (_p, n) => { if (n === 1) throw apiError(0, 503); } });
  try {
    await assert.rejects(() => w.fire(1), /transient send failure/);
    assert.equal(w.row.enabled, true, 'not disabled');
    assert.equal(w.row.lastSentAt, null, 'the claim was given back so the retry can send');
    assert.equal(w.sent.length, 0);
    assert.equal(await w.fire(2), undefined);
    assert.equal(w.sent.length, 1);
    assert.equal(w.row.enabled, false, 'finished after the successful retry');
    assert.equal(w.attempts[0].nonce, w.attempts[1].nonce);
    assert.equal(w.attempts[0].enforceNonce, true);
  } finally {
    w.restore();
  }
});

test('a ONE_SHOT is disabled on the final attempt and on a permanent error, never retried then', async () => {
  const flaky = reminderWorld({ onSend: async () => { throw apiError(0, 500); } });
  try {
    assert.equal(await flaky.fire(10), undefined, 'the final attempt does not throw');
    assert.equal(flaky.row.enabled, false);
    assert.equal(flaky.sent.length, 0);
  } finally {
    flaky.restore();
  }
  for (const code of [50013, 50001, 10003, 50007]) {
    const w = reminderWorld({ onSend: async () => { throw apiError(code, 403); } });
    try {
      assert.equal(await w.fire(1), undefined, `code ${code}: no retry`);
      assert.equal(w.row.enabled, false);
      assert.equal(w.attempts.length, 1);
    } finally {
      w.restore();
    }
  }
});

test('a ONE_SHOT retries a failed channel lookup too, but not past its late tolerance', async () => {
  const w = reminderWorld();
  w.guild.channels.cache.clear();
  w.guild.channels.fetch = async () => { throw Object.assign(new Error('socket hang up'), { code: 'ECONNRESET' }); };
  try {
    await assert.rejects(() => w.fire(1), /transient send failure/);
    assert.equal(w.row.enabled, true);
    // 59.5 minutes late: the next backoff (a minute) would pass the 60 minute window, so this is the last try.
    w.row.runAt = new Date(Date.now() - 59.5 * 60_000);
    assert.equal(await w.fire(2), undefined);
    assert.equal(w.row.enabled, false);
  } finally {
    w.restore();
  }
});

test('a timed-out send is retried only on the early backoff steps and keeps its claim after that', async () => {
  const early = reminderWorld({ onSend: async () => { throw new TimeoutError('reminder send'); } });
  try {
    await assert.rejects(() => early.fire(1), /transient send failure/);
    assert.equal(early.row.lastSentAt, null, 'the retry (same nonce) may send');
    assert.equal(early.row.enabled, true);
  } finally {
    early.restore();
  }
  const late = reminderWorld({ onSend: async () => { throw new TimeoutError('reminder send'); } });
  try {
    assert.equal(await late.fire(4), undefined, '240 s is beyond the nonce window: no retry');
    assert.ok(late.row.lastSentAt, 'the claim stays, so the occurrence is never sent a second time');
    assert.equal(late.row.enabled, false);
  } finally {
    late.restore();
  }
});

test('a recurring reminder retries a transient failure but is rescheduled, not failed, on the final attempt', async () => {
  const w = reminderWorld({ mode: 'INTERVAL', onSend: async (_p, n) => { if (n <= 2) throw apiError(0, 502); } });
  try {
    await assert.rejects(() => w.fire(1), /transient send failure/);
    assert.equal(w.row.lastSentAt, null);
    assert.equal(w.row.enabled, true);
    const last = await w.fire(10); // fails again, but it is the final attempt
    assert.ok(last.nextRunAt > new Date(), 'rescheduled');
    assert.equal(w.row.enabled, true);
    assert.equal(w.sent.length, 0);
    // The claim was given back, so the same slot is still sendable, and a success is a plain reschedule.
    const ok = await w.fire(1);
    assert.equal(w.sent.length, 1);
    assert.ok(ok.nextRunAt > new Date());
    assert.equal(w.attempts[0].nonce, w.attempts[2].nonce, 'one nonce for the occurrence across attempts');
  } finally {
    w.restore();
  }
});

test('a recurring reminder stops retrying when the backoff would pass its late tolerance', async () => {
  const w = reminderWorld({ mode: 'INTERVAL', onSend: async () => { throw apiError(0, 500); } });
  try {
    // tolerance = 2.5 min and the slot is 20 s old: a 120 s wait after attempt 3 fits, a 240 s wait after attempt 4 does not.
    await assert.rejects(() => w.fire(3), /transient send failure/);
    const res = await w.fire(4);
    assert.ok(res.nextRunAt instanceof Date, 'rescheduled to its next slot instead of retrying past the tolerance');
    assert.equal(w.row.enabled, true);
  } finally {
    w.restore();
  }
});

test('a recurring reminder does not retry a permanent permission error', async () => {
  const w = reminderWorld({ mode: 'INTERVAL', onSend: async () => { throw apiError(50013, 403); } });
  try {
    const res = await w.fire(1);
    assert.ok(res.nextRunAt instanceof Date);
    assert.equal(w.row.enabled, true, 'an admin may fix the permission');
    assert.equal(w.row.lastSentAt, null, 'the claim is given back, as before');
  } finally {
    w.restore();
  }
});

// ── birthdays: departed members cannot starve the members behind them ────────────────────────────

const bday = await import('../src/services/birthdayService.js');
const configCmd = await import('../src/commands/admin/birthday-config.js');

function birthdayWorld({ departed, present }) {
  const guildId = sid('8', 500 + departed + present);
  const channelId = sid('5', 500 + departed + present);
  const sent = [];
  const local = new Date();
  const month = local.getUTCMonth() + 1;
  const day = local.getUTCDate();
  const rows = [
    ...Array.from({ length: departed }, (_, i) => uid(1 + i)),
    ...Array.from({ length: present }, (_, i) => uid(1000 + i)),
  ].map((userId) => ({ id: `b${userId}`, userId, month, day, year: null, lastAnnouncedYear: null, guildId }));
  const store = new Map(rows.filter((r) => r.userId >= uid(1000)).map((r) => [r.userId, {
    id: r.userId, pending: false,
    user: { id: r.userId, bot: false, username: `user${r.userId.slice(-3)}`, displayAvatarURL: () => '', toString: () => `<@${r.userId}>` },
    displayName: `User ${r.userId.slice(-3)}`, toString: () => `<@${r.userId}>`,
    roles: { cache: new Set() },
  }]));
  const channel = {
    id: channelId, name: 'birthdays', type: ChannelType.GuildText, isTextBased: () => true, isThread: () => false,
    permissionsFor: () => ({ has: () => true }),
    async send(payload) { sent.push(payload); return { id: 'm' }; },
  };
  const fetchCalls = [];
  const guild = {
    id: guildId, name: 'Guild', available: true, memberCount: 10, ownerId: '1',
    channels: { cache: new Map([[channelId, channel]]) },
    roles: { cache: new Map() },
    members: {
      cache: new Map(),
      me: { id: sid('6', 1), permissions: { has: () => true }, roles: { highest: { position: 10 } } },
      async fetch(arg) {
        fetchCalls.push(arg.user.length);
        return new Map(arg.user.filter((id) => store.has(id)).map((id) => [id, store.get(id)]));
      },
    },
  };
  channel.guild = guild;
  const claims = [];
  const findManyCalls = [];
  const restore = patch({
    guildSettings: {
      findUnique: async () => ({
        guildId, birthdaysEnabled: true, timezone: 'UTC', birthdayHour: 0, birthdayChannelId: channelId, birthdayRoleId: null,
        birthdayMessage: null, birthdayMessageAge: null,
      }),
    },
    birthday: {
      findMany: async (args) => {
        if (args.where.roleGrantedAt) return [];
        const gt = args.where.userId?.gt ?? null;
        findManyCalls.push({ gt, take: args.take });
        return rows.filter((r) => gt === null || r.userId > gt).sort((a, b) => (a.userId < b.userId ? -1 : 1)).slice(0, args.take);
      },
    },
  });
  const originalRaw = prisma.$queryRaw;
  prisma.$queryRaw = async (query) => {
    const ids = query.values.find(Array.isArray) ?? [];
    claims.push(ids);
    return rows.filter((r) => ids.includes(r.id)).map((r) => ({ id: r.id, userId: r.userId, year: null }));
  };
  return { guild, sent, claims, fetchCalls, findManyCalls, rows, restore: () => { restore(); prisma.$queryRaw = originalRaw; } };
}

test('birthday tick: 60 departed members sharing today do not starve the members behind them', async () => {
  const w = birthdayWorld({ departed: 60, present: 3 });
  try {
    const res = await bday.tickGuild({}, w.guild);
    assert.equal(w.sent.length, 1, 'the present members are announced on the first tick');
    for (let i = 0; i < 3; i += 1) assert.ok(w.sent[0].content.includes(`<@${uid(1000 + i)}>`));
    assert.ok(!w.sent[0].content.includes(`<@${uid(1)}>`), 'a departed member is never announced');
    assert.equal(w.claims.length, 1);
    assert.deepEqual(w.claims[0].sort(), [0, 1, 2].map((i) => `b${uid(1000 + i)}`).sort(), 'only present members are claimed; departed rows stay unclaimed');
    assert.ok(w.findManyCalls.length >= 2 && w.findManyCalls[1].gt === uid(51), 'the second page starts after the first (keyset by userId)');
    assert.equal(res.nextRunAt.getTime(), bday.nextTickAt(new Date(), 'UTC').getTime(), 'no continuation run: nothing is left behind');
  } finally {
    w.restore();
  }
});

test('birthday tick: only a 51st present member asks for a continuation run', async () => {
  const w = birthdayWorld({ departed: 55, present: 51 });
  try {
    const before = Date.now();
    const res = await bday.tickGuild({}, w.guild);
    assert.equal(w.claims[0].length, 50, 'a batch is 50 celebrations');
    assert.ok(res.nextRunAt.getTime() < before + 90_000, 'a continuation run follows for the remaining member');
  } finally {
    w.restore();
  }
  const exactly = birthdayWorld({ departed: 55, present: 50 });
  try {
    const res = await bday.tickGuild({}, exactly.guild);
    assert.equal(exactly.claims[0].length, 50);
    assert.equal(res.nextRunAt.getTime(), bday.nextTickAt(new Date(), 'UTC').getTime(), 'departed members behind the batch never trigger a continuation');
  } finally {
    exactly.restore();
  }
});

// ── birthdays: /birthday-config set-user tells the admin the age will be public ─────────────────

function setUserCall(guildId, { date, settings = {} }) {
  const member = { id: uid(77), user: { id: uid(77), bot: false }, toString: () => `<@${uid(77)}>` };
  const edits = [];
  const options = { date, 'member:member': member };
  const i = {
    guild: { id: guildId }, guildId, user: { id: uid(1) }, member: null, channel: null,
    deferred: false, replied: false, ephemeral: undefined,
    inCachedGuild: () => true,
    options: {
      getSubcommand: () => 'set-user', getSubcommandGroup: () => null,
      get: (n) => options[n] ?? null, getString: (n) => options[n] ?? null, getUser: () => null,
      getMember: (n) => options[`${n}:member`] ?? null, getChannel: () => null, getRole: () => null, getInteger: () => null, getBoolean: () => null,
    },
    async deferReply({ ephemeral } = {}) { i.deferred = true; i.ephemeral = Boolean(ephemeral); },
    async editReply(p) { edits.push(p); return { id: 'm' }; },
    async followUp(p) { edits.push(p); return { id: 'm' }; },
    async reply(p) { i.replied = true; edits.push(p); return {}; },
  };
  const restore = patch({
    guildSettings: {
      findUnique: async () => ({
        guildId, birthdaysEnabled: true, timezone: 'UTC', birthdayHour: 9, birthdayChannelId: sid('5', 77), birthdayRoleId: null,
        birthdayMessage: null, birthdayMessageAge: null, ...settings,
      }),
      upsert: async () => ({ guildId }),
    },
    birthday: {
      findUnique: async () => null,
      upsert: async ({ create }) => ({ id: 'b1', ...create }),
    },
  });
  return { i, edits, restore, desc: () => edits.at(-1).embeds[0].data.description ?? '', title: () => edits.at(-1).embeds[0].data.title };
}

test('birthday-config set-user: a given year says the age shows in the announcement; the reply never carries it', async () => {
  const withYear = setUserCall(sid('8', 901), { date: '1990-04-06' });
  try {
    await configCmd.execute(withYear.i);
    assert.match(withYear.title(), /Birthday Saved/);
    assert.match(withYear.desc(), /their age will be shown in the public announcement in <#\d+>/);
    assert.ok(!/1990/.test(withYear.desc()), 'the year stays out of the public reply');
    assert.ok(!/\b3\d\b/.test(withYear.desc().replace(/<[^>]+>/g, '')), 'and so does the age');
  } finally {
    withYear.restore();
  }

  const noYear = setUserCall(sid('8', 902), { date: '04-06' });
  try {
    await configCmd.execute(noYear.i);
    assert.match(noYear.title(), /Birthday Saved/);
    assert.ok(!/age will be shown/.test(noYear.desc()));
  } finally {
    noYear.restore();
  }

  // No channel, nobody to see an announcement; a message without {age}: the age is not shown.
  const noChannel = setUserCall(sid('8', 903), { date: '1990-04-06', settings: { birthdayChannelId: null } });
  const noAge = setUserCall(sid('8', 904), { date: '1990-04-06', settings: { birthdayMessageAge: 'Happy birthday {user}!' } });
  try {
    await configCmd.execute(noChannel.i);
    assert.ok(!/age will be shown/.test(noChannel.desc()));
    await configCmd.execute(noAge.i);
    assert.ok(!/age will be shown/.test(noAge.desc()));
  } finally {
    noChannel.restore();
    noAge.restore();
  }
});
