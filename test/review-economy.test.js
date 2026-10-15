// Regression tests for the economy review: /pity must TAKE the casino lock (a /blackjack sent together with it could
// otherwise escrow the whole balance between the lock check and the claim, and pity would pay out against that bet).
// Offline: no Discord, Postgres, Redis or network.
// Also: settleBet settles at most once (retries only errors that prove nothing was sent), the slots paytable returns ~95 %, and
// refundPurchase returns the full price even to a buyer who reached the server's maximum balance.
import { test, after, mock } from 'node:test';
import assert from 'node:assert/strict';

Object.assign(process.env, {
  NODE_ENV: 'test',
  DISCORD_TOKEN: 'test-token',
  CLIENT_ID: '000000000000000000',
  DATABASE_URL: 'postgresql://test:test@127.0.0.1:1/test',
  REDIS_URL: 'redis://127.0.0.1:1',
});

const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { Prisma } = await import('@prisma/client');
const { tryLockCasino, unlockCasino, isCasinoLocked, settleBet, failedBeforeSend, refundPurchase } = await import('../src/services/economyService.js');
const { COIN_MAX } = await import('../src/config/constants.js');
const pity = await import('../src/commands/economy/pity.js');
const slots = await import('../src/commands/games/slots.js');

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

after(async () => {
  mock.restoreAll();
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const G = '955026000000000001';
const U = '955026900000000001';

function fakeInteraction({ deferReply } = {}) {
  const calls = [];
  return {
    guildId: G,
    user: { id: U, bot: false, username: 'tester' },
    deferred: false,
    replied: false,
    ephemeral: null,
    calls,
    deferReply: deferReply ?? (async () => { calls.push(['deferReply']); }),
    reply: async (p) => { calls.push(['reply', p]); },
    editReply: async (p) => { calls.push(['editReply', p]); },
    deleteReply: async () => { calls.push(['deleteReply']); },
    followUp: async (p) => { calls.push(['followUp', p]); },
  };
}

test('/pity holds the casino lock while it runs, so a game cannot escrow a bet between the check and the claim', async () => {
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const i = fakeInteraction({ deferReply: () => gate });

  const running = pity.execute(i).catch((err) => err);
  // The lock is taken synchronously, before the first await: a /blackjack, /roulette or /slots started now is refused.
  assert.equal(isCasinoLocked(G, U), true);
  assert.equal(tryLockCasino(G, U), false);

  release(); // the deferral lands; the settings / account lookups then fail offline, which must not leak the lock
  await running;
  assert.equal(isCasinoLocked(G, U), false);
});

test('/pity releases the casino lock when the deferral fails', async () => {
  const i = fakeInteraction({ deferReply: async () => { throw new Error('Unknown interaction'); } });
  const err = await pity.execute(i).catch((e) => e);
  assert.match(String(err?.message), /Unknown interaction/);
  assert.equal(isCasinoLocked(G, U), false);
  assert.equal(tryLockCasino(G, U), true);
  unlockCasino(G, U);
});

test('/pity is refused privately while a casino game holds the lock, and leaves that lock alone', async () => {
  assert.equal(tryLockCasino(G, U), true);
  try {
    const i = fakeInteraction();
    await pity.execute(i);
    assert.deepEqual(i.calls.map((c) => c[0]), ['reply']);
    assert.equal(i.calls[0][1].ephemeral, true);
    assert.match(i.calls[0][1].embeds[0].data.title, /Game In Progress/);
    assert.equal(isCasinoLocked(G, U), true, "the running game's lock is not released by the refused /pity");
  } finally {
    unlockCasino(G, U);
  }
});

// ═════════════════════════ settleBet settles at most once ═════════════════════════

const known = (code) => new Prisma.PrismaClientKnownRequestError(`db ${code}`, { code, clientVersion: 'test' });

/** Replaces prisma.$queryRaw with a script of outcomes (an Error is thrown, an array is returned); resolves to the call count. */
async function withScriptedCredit(script, fn) {
  let calls = 0;
  const m = mock.method(prisma, '$queryRaw', async () => {
    const step = script[Math.min(calls, script.length - 1)];
    calls++;
    if (step instanceof Error) throw step;
    return step;
  });
  try {
    return await fn(() => calls);
  } finally {
    m.mock.restore();
  }
}

test('failedBeforeSend accepts only errors that prove nothing reached the database', () => {
  assert.equal(failedBeforeSend(new Prisma.PrismaClientInitializationError('cannot connect', 'test', 'P1001')), true);
  assert.equal(failedBeforeSend(known('P1001')), true);
  assert.equal(failedBeforeSend(known('P2024')), true, 'pool timeout: no connection was ever taken');
  for (const code of ['P1008', 'P1017', 'P2010', 'P2034', 'P2002']) assert.equal(failedBeforeSend(known(code)), false, code);
  assert.equal(failedBeforeSend(new Prisma.PrismaClientUnknownRequestError('unknown', { clientVersion: 'test' })), false);
  assert.equal(failedBeforeSend(new Error('socket hang up')), false);
  assert.equal(failedBeforeSend(null), false);
});

test('settleBet never repeats a credit whose response may have been lost', async () => {
  for (const err of [known('P1017'), known('P1008'), new Error('socket hang up'), new Prisma.PrismaClientUnknownRequestError('x', { clientVersion: 'test' })]) {
    await withScriptedCredit([err, [{ balance: 1300 }]], async (calls) => {
      await assert.rejects(settleBet('g', 'u', 300, null, { stake: 100 }), err.constructor === Error ? /socket hang up/ : (e) => e === err);
      assert.equal(calls(), 1, `${err.code ?? err.message}: exactly one credit attempt, so the payout cannot land twice`);
    });
  }
});

test('settleBet retries once when the credit provably never left the process', async () => {
  for (const first of [known('P2024'), known('P1001'), new Prisma.PrismaClientInitializationError('cannot connect', 'test')]) {
    await withScriptedCredit([first, [{ balance: 1300 }]], async (calls) => {
      assert.deepEqual(await settleBet('g', 'u', 300, null, { stake: 100 }), { balance: 1300, capped: false, payoutCapped: false });
      assert.equal(calls(), 2);
    });
  }
  // Still at most one retry; and if the retry fails with an ambiguous error it is not repeated either.
  await withScriptedCredit([known('P2024'), known('P2024'), [{ balance: 1300 }]], async (calls) => {
    await assert.rejects(settleBet('g', 'u', 300, null, { stake: 100 }), (e) => e.code === 'P2024');
    assert.equal(calls(), 2);
  });
  await withScriptedCredit([known('P2024'), known('P1017'), [{ balance: 1300 }]], async (calls) => {
    await assert.rejects(settleBet('g', 'u', 300, null, { stake: 100 }), (e) => e.code === 'P1017');
    assert.equal(calls(), 2);
  });
});

// ═════════════════════════ slots: expected return ═════════════════════════

test('slots paytable returns about 95 % of the stake (94-96 %), computed from the exported reels', () => {
  const { SYMBOLS, LINES, TOTAL_WEIGHT } = slots;
  assert.equal(SYMBOLS.reduce((sum, s) => sum + s.weight, 0), TOTAL_WEIGHT);
  // Each line is three distinct cells drawn independently, so E[sum of line multipliers] = LINES * SUM((w / total)^3 * m).
  const rtp = LINES.length * SYMBOLS.reduce((sum, s) => sum + (s.weight / TOTAL_WEIGHT) ** 3 * s.multiplier, 0);
  assert.ok(rtp >= 0.94 && rtp <= 0.96, `slots RTP is ${rtp}`);
  assert.ok(Math.abs(rtp - 8800 / 9261) < 1e-12, 'the exact value documented in slots.js');
  // Whole-number multipliers of at least 2 keep floor() and the "bet + 1" minimum out of the payout, so the RTP holds at every bet.
  for (const s of SYMBOLS) assert.ok(Number.isInteger(s.multiplier) && s.multiplier >= 2, `${s.emoji} multiplier ${s.multiplier}`);
  for (const s of SYMBOLS) {
    const [x, y] = SYMBOLS.filter((o) => o !== s);
    const grid = [s, s, s, x, y, x, y, x, y]; // only the top row lines up
    assert.equal(slots.checkWins(grid).length, 1);
    for (const bet of [1, 2, 7, 100, 12345]) assert.equal(slots.slotsPayout(grid, bet).payout, bet * s.multiplier, `${s.emoji} at bet ${bet}`);
  }
});

// ═════════════════════════ refundPurchase returns the full price ═════════════════════════

test('refundPurchase credits up to COIN_MAX, not the server maximum balance', async () => {
  const statements = [];
  // The model delegate has no own `findUnique` for mock.method to wrap, so it is swapped by hand and put back in `finally`.
  const realFindUnique = prisma.economyAccount.findUnique;
  prisma.economyAccount.findUnique = async () => ({ balance: 5000, guildId: 'g', userId: 'u' });
  const tx = mock.method(prisma, '$transaction', async (fn) => fn({
    $executeRaw: async (sql) => { statements.push(['exec', sql]); return 1; },
    $queryRaw: async (sql) => { statements.push(['query', sql]); return [{ balance: 5300 }]; },
  }));
  try {
    // The server maximum is 5000 and the buyer sits at 5000 again: the 300-coin refund must not be clamped to it.
    await refundPurchase({ guildId: 'g', userId: 'u', item: { id: 'item' }, quantity: 1, total: 300, toInventory: false, settings: { economyMaxBalance: 5000 } });
    const credit = statements.find(([kind, sql]) => kind === 'query' && /"balance"/.test(sql.sql))?.[1];
    assert.ok(credit, 'the refund credits the account inside the transaction');
    assert.ok(credit.values.includes(BigInt(COIN_MAX)), 'the cap is COIN_MAX');
    assert.ok(!credit.values.includes(5000n), "the server's maximum balance is not used as the cap");
    assert.ok(credit.values.includes(300n), 'the full price is refunded');
  } finally {
    prisma.economyAccount.findUnique = realFindUnique;
    tx.mock.restore();
  }
});
