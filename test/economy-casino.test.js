// pkg-economy-casino tests: pkg-economy.md §11 items 15-23 and 25, the payload checks of item 24 for the three games, and the
// flow cases the escrow rewrite exists for (settle exactly once, forfeit on abandon, unlock always, acknowledge first,
// max payout, channel rules). The flow cases run offline against a fake escrow service injected through execute()'s third
// argument. With TEST_DATABASE_URL (a migrated database) the same flows also run against the real economyService:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/economy-casino.test.js
import { test, after } from 'node:test';
import { EventEmitter } from 'node:events';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is only used with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const svc = await import('../src/services/economyService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const settingsSvc = await import('../src/services/settingsService.js');
const { COIN_MAX, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { buildCustomId, parseCustomId } = await import('../src/config/customIds.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const bj = await import('../src/commands/games/blackjack.js');
const roulette = await import('../src/commands/games/roulette.js');
const slots = await import('../src/commands/games/slots.js');

// Silence the Redis reconnect noise and the logger output of the failure-path tests.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const G = (n) => `95502600000000${String(n).padStart(4, '0')}`;
const U = (n) => `95502690000000${String(n).padStart(4, '0')}`;
const GUILDS = Array.from({ length: 40 }, (_, i) => G(i + 1));

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) for (const guildId of GUILDS) await prisma.guildSettings.deleteMany({ where: { guildId } }).catch(() => {});
  await prisma.$disconnect().catch(() => {});
});

// ═══════════════════════════════════ helpers ═══════════════════════════════════

const ROOT_FILES = ['blackjack', 'roulette', 'slots'].map((n) => resolve(ROOT, 'src', 'commands', 'games', `${n}.js`));

/** A card as the game builds it. */
const card = (rank, suit = '♠️') => ({ rank, suit, value: rank === 'A' ? 11 : ['J', 'Q', 'K'].includes(rank) ? 10 : Number(rank) });
const hand = (...ranks) => ranks.map((r) => card(String(r)));
/** A deck whose pop() order is the listed ranks (player, player, dealer, dealer, then later draws). */
const stack = (...ranks) => ranks.map((r) => card(String(r))).reverse();

function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const embedOf = (payload, n = 0) => payload?.embeds?.[n]?.toJSON?.() ?? null;
const fieldOf = (embed, name) => embed?.fields?.find((f) => f.name === name || f.name.startsWith(`${name} (`))?.value;
const tick = () => new Promise((resolve) => setImmediate(resolve));
const defer = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };

/** Mimics a discord.js collector: stop() ends it once and emits `end` synchronously; deliver() awaits every collect handler. */
class FakeCollector extends EventEmitter {
  constructor(options) {
    super();
    this.options = options;
    this.ended = false;
    this.reason = null;
  }

  stop(reason = 'user') {
    if (this.ended) return;
    this.ended = true;
    this.reason = reason;
    this.emit('end', new Map(), reason);
  }

  async deliver(click) {
    if (this.ended) return false;
    await Promise.all(this.listeners('collect').map((fn) => fn(click)));
    return true;
  }

  /** A click that was already being dispatched when stop() ran (discord.js awaits its filter before emitting collect). */
  async deliverRaw(click) {
    await Promise.all(this.listeners('collect').map((fn) => fn(click)));
  }
}

let nextId = 1000000000000000;
const snowflake = () => String(nextId++);

function mkClick(interactionId, action, userId, timeline = null) {
  const calls = [];
  const click = {
    id: snowflake(), customId: buildCustomId('bj', interactionId, action), user: { id: userId },
    deferred: false, replied: false, ephemeral: null, calls,
    deferUpdate: async () => { click.deferred = true; calls.push(['deferUpdate']); timeline?.push('ack'); },
    reply: async (o) => { click.replied = true; calls.push(['reply', o]); timeline?.push('ack'); },
    followUp: async (o) => { calls.push(['followUp', o]); },
    editReply: async (o) => { calls.push(['editReply', o]); },
  };
  return click;
}
const verbs = (click) => click.calls.map((c) => c[0]);

function mkGuild(channelIds = []) {
  return { id: G(1), channels: { cache: new Map(channelIds.map((id) => [id, { id }])) } };
}

/** A slash-command interaction; its editReply returns a message that hands out FakeCollectors. */
function mkSlash({ bet = 100, space = 'red', userId = U(1), channelId = '500', guild = mkGuild(), guildId = G(1), timeline = null, editFails = false, failFirstEdit = false } = {}) {
  const calls = [];
  const collectors = [];
  let edits = 0;
  const message = {
    createMessageComponentCollector(options) {
      if (message.collectorFails) throw new Error('collector failed');
      const c = new FakeCollector(options);
      collectors.push(c);
      return c;
    },
  };
  const it = {
    id: snowflake(), guildId, channelId, channel: { id: channelId }, guild, createdTimestamp: Date.now(),
    user: { id: userId }, deferred: false, replied: false, ephemeral: null, calls, collectors, message,
    options: { getInteger: () => bet, getString: () => space },
    reply: async (o) => { it.replied = true; it.ephemeral = Boolean(o?.ephemeral); calls.push(['reply', o]); timeline?.push('reply'); },
    deferReply: async () => { it.deferred = true; it.ephemeral = false; calls.push(['deferReply']); timeline?.push('defer'); },
    editReply: async (o) => {
      calls.push(['editReply', o]);
      timeline?.push('edit');
      edits += 1;
      if (editFails || (failFirstEdit && edits === 1)) throw Object.assign(new Error('Unknown Message'), { code: 10008 });
      return message;
    },
    deleteReply: async () => { calls.push(['deleteReply']); },
    followUp: async (o) => { calls.push(['followUp', o]); },
  };
  return it;
}
const editsOf = (it) => it.calls.filter((c) => c[0] === 'editReply').map((c) => c[1]);
const lastEdit = (it) => editsOf(it).at(-1);
const lastPrivate = (it) => it.calls.filter((c) => c[0] === 'reply' || c[0] === 'followUp').at(-1)?.[1];

/** A fake escrow service with a ledger: balances move exactly as the real reserveBet / settleBet move them. */
function mkWorld({ balance = 1000, settings = {}, deck = null, spin = null } = {}) {
  const timeline = [];
  const state = { balance, locked: new Set(), reserves: [], settles: [], failSettle: false, failReserve: false, gate: null, settleGate: null };
  const row = { guildId: G(1), ...settings };
  const cfg = svc.economyConfig(row);
  const deps = {
    loadSettings: async () => row,
    tryLockCasino: (g, u) => {
      const k = `${g}:${u}`;
      if (state.locked.has(k)) return false;
      state.locked.add(k);
      timeline.push('lock');
      return true;
    },
    unlockCasino: (g, u) => { if (state.locked.delete(`${g}:${u}`)) timeline.push('unlock'); },
    reserveBet: async (g, u, bet) => {
      timeline.push(`reserve:${bet}`);
      state.reserves.push(bet);
      if (state.gate && state.reserves.length === 2) await state.gate;
      else await Promise.resolve();
      if (state.failReserve) throw new Error('db down');
      if (bet > state.balance) return { ok: false, code: 'INSUFFICIENT', balance: state.balance };
      state.balance -= bet;
      return { ok: true, balance: state.balance, before: state.balance + bet };
    },
    settleBet: async (g, u, payout, s, opts = {}) => {
      timeline.push(`settle:${payout}/${opts.stake}`);
      state.settles.push({ payout, stake: opts.stake });
      if (state.settleGate) await state.settleGate;
      else await Promise.resolve();
      if (!Number.isSafeInteger(opts.stake)) throw new svc.EconomyError('INVALID');
      if (state.failSettle) throw new Error('db down');
      const { payout: paid, capped } = svc.clampPayout(payout, opts.stake, cfg.maxPayout);
      state.balance += paid;
      return { balance: state.balance, capped: false, payoutCapped: capped };
    },
    newDeck: () => (deck ? [...deck] : bj.buildDeck()),
    spin: spin ?? (() => 0),
    sleep: async () => {},
  };
  return { deps, state, timeline, cfg, row };
}

const grid3 = (lineEmoji, otherEmoji = '🍋') => {
  const sym = (emoji) => slots.SYMBOLS.find((s) => s.emoji === emoji);
  // Only the top row is a line: row 0 = lineEmoji x3, the rest alternates so no other line completes.
  return [lineEmoji, lineEmoji, lineEmoji, otherEmoji, '🍒', otherEmoji, '🍇', otherEmoji, '🍒'].map(sym);
};

/** Starts a blackjack hand and returns the interaction, its collector and the click factory. */
async function startHand(world, opts = {}) {
  const it = mkSlash({ timeline: world.timeline, ...opts });
  await bj.execute(it, null, world.deps);
  const click = (action, userId = opts.userId ?? U(1)) => mkClick(it.id, action, userId, world.timeline);
  return { it, collector: it.collectors[0], click };
}

// ═══════════════════════════ blackjack: pure helpers (items 15-19) ═══════════════════════════

test('item 15: hand values, aces and naturals', () => {
  assert.equal(bj.handValue(hand('A', 'K')), 21);
  assert.equal(bj.isNatural(hand('A', 'K')), true);
  assert.equal(bj.handValue(hand('A', 'A', 9)), 21);
  assert.equal(bj.isNatural(hand('A', 'A', 9)), false);
  assert.equal(bj.handValue(hand('K', 'Q', 5)), 25);
  assert.equal(bj.handValue(hand('A', 'A', 'A')), 13);
  assert.equal(bj.handValue(hand('A', 9, 'A')), 21);
  assert.equal(bj.handValue(hand('A', 5, 'K')), 16, 'the ace drops to 1 once the hand would bust');
  assert.equal(bj.handValue(hand(10, 'A', 'A', 'A')), 13);
  assert.equal(bj.handValue(hand('A', 'A', 'A', 'A', 2, 2, 2, 2, 3, 3, 3)), 21, 'eleven cards, every ace as 1');
  assert.equal(bj.isNatural(hand(10, 5, 6)), false, 'a three-card 21 is not a natural');
  assert.equal(bj.isSoft(hand('A', 6)), true);
  assert.equal(bj.isSoft(hand('A', 6, 10)), false);
  assert.equal(bj.isSoft(hand(10, 7)), false);
});

test('item 16: the dealer hits below 17 and on soft 17 (H17), stands on hard 17 and above', () => {
  assert.equal(bj.shouldDealerHit(hand('A', 6)), true, 'soft 17');
  assert.equal(bj.shouldDealerHit(hand(10, 7)), false);
  assert.equal(bj.shouldDealerHit(hand(10, 6)), true);
  assert.equal(bj.shouldDealerHit(hand('A', 7)), false, 'soft 18 stands');
  assert.equal(bj.shouldDealerHit(hand('A', 'A', 5)), true, 'soft 17 with two aces');
  assert.equal(bj.shouldDealerHit(hand('A', 6, 10)), false, 'the same cards as a hard 17 stand');
  assert.equal(bj.shouldDealerHit(hand(10, 10)), false);
  assert.equal(bj.shouldDealerHit(hand(10, 6, 5)), false, 'hard 21');
});

test('item 17: naturalOutcome (the dealer peek, R078)', () => {
  assert.deepEqual(bj.naturalOutcome(hand('A', 'K'), hand('A', 'Q'), 100), { result: 'push-naturals', payout: 100 });
  assert.deepEqual(bj.naturalOutcome(hand('A', 'K'), hand(10, 9), 5), { result: 'blackjack', payout: 12 });
  assert.deepEqual(bj.naturalOutcome(hand('A', 'K'), hand(10, 9), 100), { result: 'blackjack', payout: 250 });
  assert.deepEqual(bj.naturalOutcome(hand('A', 'K'), hand(10, 9), 1), { result: 'blackjack', payout: 2 });
  assert.deepEqual(bj.naturalOutcome(hand(10, 9), hand('A', 'K'), 100), { result: 'dealer-blackjack', payout: 0 });
  assert.equal(bj.naturalOutcome(hand(10, 9), hand(10, 8), 100), null);
  assert.equal(bj.naturalOutcome(hand(10, 5, 6), hand('A', 'K'), 100).result, 'dealer-blackjack', 'a three-card 21 never meets a dealer natural');
  const big = bj.naturalOutcome(hand('A', 'K'), hand(10, 9), COIN_MAX);
  assert.equal(big.payout, 2_500_000_000);
  assert.ok(Number.isSafeInteger(big.payout));
});

test('item 18: finalOutcome (stake is the total escrowed)', () => {
  assert.deepEqual(bj.finalOutcome(hand(10, 9, 5), hand(10, 7), 100), { result: 'bust', payout: 0 });
  assert.deepEqual(bj.finalOutcome(hand(10, 8), hand(10, 6, 9), 200), { result: 'dealer-bust', payout: 400 });
  assert.deepEqual(bj.finalOutcome(hand(10, 9), hand(10, 8), 100), { result: 'win', payout: 200 });
  assert.deepEqual(bj.finalOutcome(hand(10, 8), hand(10, 8), 100), { result: 'push', payout: 100 });
  assert.deepEqual(bj.finalOutcome(hand(10, 7), hand(10, 8), 100), { result: 'lose', payout: 0 });
  assert.deepEqual(bj.finalOutcome(hand(6, 5, 10), hand(10, 8), 200), { result: 'win', payout: 400 }, 'doubled: 2x the bet staked, 2x that returned');
  assert.deepEqual(bj.finalOutcome(hand(10, 5, 6), hand(10, 6, 5), 100), { result: 'push', payout: 100 }, 'two 21s push');
  assert.deepEqual(bj.finalOutcome(hand(10, 9), hand(10, 8), 1_000_000_000), { result: 'win', payout: 2_000_000_000 });
});

test('item 19: buildDeck has 52 unique cards and is a permutation shuffled by the rng', () => {
  const rng = mulberry32(7);
  const deck = bj.buildDeck((n) => Math.floor(rng() * n));
  assert.equal(deck.length, 52);
  assert.equal(new Set(deck.map((c) => `${c.rank}${c.suit}`)).size, 52);
  assert.deepEqual([...new Set(deck.map((c) => c.suit))].sort(), [...bj.SUITS].sort());
  assert.equal(deck.reduce((sum, c) => sum + c.value, 0), 4 * (11 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 10 + 10 + 10));
  const calls = [];
  bj.buildDeck((n) => { calls.push(n); return 0; });
  assert.deepEqual(calls, Array.from({ length: 51 }, (_, k) => 52 - k), 'Fisher-Yates calls rng(i + 1) from 52 down to 2');
  assert.notDeepEqual(bj.buildDeck().map((c) => c.rank + c.suit), bj.buildDeck().map((c) => c.rank + c.suit), 'the default rng shuffles');
});

// ═══════════════════════════ roulette: pure helpers (items 20-22) ═══════════════════════════

test('item 20: parseSpace is strict (R082) and green is the 0 pocket (R081)', () => {
  assert.equal(roulette.parseSpace('red').token, 'red');
  assert.equal(roulette.parseSpace('BLACK').token, 'black');
  const green = roulette.parseSpace('Green');
  assert.equal(green.token, '0');
  assert.equal(green.multiplier, 36);
  assert.equal(roulette.parseSpace('0').token, '0');
  assert.equal(roulette.parseSpace('0').multiplier, 36);
  assert.equal(roulette.parseSpace('36').token, '36');
  assert.equal(roulette.parseSpace('1-18').token, 'low');
  assert.equal(roulette.parseSpace('19 - 36').token, 'high');
  for (const dozen of ['1-12', '13-24', '25-36']) assert.equal(roulette.parseSpace(dozen).token, dozen);
  assert.equal(roulette.parseSpace('  Even ').token, 'even');
  assert.equal(roulette.parseSpace('odd').kind, 'parity');
  assert.equal(roulette.parseSpace('17').label, 'Number 17');
  assert.equal(roulette.parseSpace('13–24').token, '13-24', 'an en dash reads as a hyphen');
  for (const bad of ['1st 12', '12 red', '3.9', '00', '07', '37', '-1', '', '1e1', '0x10', '+5', 'red black', '1-13', '99', 'greenish']) {
    assert.equal(roulette.parseSpace(bad), null, `"${bad}" must be rejected`);
  }
  assert.equal(roulette.parseSpace(undefined), null);
});

test('item 21: spaceReturn pays the right multiples', () => {
  const p = roulette.parseSpace;
  assert.equal(roulette.spaceReturn(p('green'), 0, 100), 3600);
  assert.equal(roulette.spaceReturn(p('red'), 0, 100), 0);
  assert.equal(roulette.spaceReturn(p('1-12'), 12, 100), 300);
  assert.equal(roulette.spaceReturn(p('1-12'), 13, 100), 0);
  assert.equal(roulette.spaceReturn(p('13-24'), 13, 100), 300);
  assert.equal(roulette.spaceReturn(p('25-36'), 36, 100), 300);
  assert.equal(roulette.spaceReturn(p('low'), 18, 10), 20);
  assert.equal(roulette.spaceReturn(p('low'), 19, 10), 0);
  assert.equal(roulette.spaceReturn(p('low'), 0, 10), 0);
  assert.equal(roulette.spaceReturn(p('high'), 19, 10), 20);
  assert.equal(roulette.spaceReturn(p('17'), 17, 1), 36);
  assert.equal(roulette.spaceReturn(p('17'), 18, 1), 0);
  assert.equal(roulette.spaceReturn(p('even'), 0, 10), 0);
  assert.equal(roulette.spaceReturn(p('even'), 2, 10), 20);
  assert.equal(roulette.spaceReturn(p('odd'), 0, 10), 0);
  assert.equal(roulette.spaceReturn(p('odd'), 35, 10), 20);
  assert.equal(roulette.spaceReturn(p('red'), 1, 10), 20);
  assert.equal(roulette.spaceReturn(p('black'), 2, 10), 20);
  assert.equal(roulette.spaceReturn(p('black'), 1, 10), 0);
  assert.equal(roulette.spaceReturn(null, 1, 10), 0);
});

test('every roulette space returns exactly 36x the bet over the 37 pockets (one house edge for all bets)', () => {
  const tokens = ['red', 'black', 'green', 'even', 'odd', 'low', 'high', '1-12', '13-24', '25-36', ...Array.from({ length: 37 }, (_, n) => String(n))];
  for (const token of tokens) {
    const space = roulette.parseSpace(token);
    let total = 0;
    for (let n = 0; n <= 36; n++) total += roulette.spaceReturn(space, n, 1);
    assert.equal(total, 36, `${token} returns ${total} over 37 pockets`);
  }
  assert.equal([...Array(37).keys()].filter((n) => roulette.wheelColor(n).label === 'Red').length, 18);
  assert.equal([...Array(37).keys()].filter((n) => roulette.wheelColor(n).label === 'Black').length, 18);
  assert.equal(roulette.wheelColor(0).emoji, '🟢');
  assert.equal(roulette.spinNumber(() => 36), 36);
  const seen = [];
  roulette.spinNumber((n) => { seen.push(n); return 0; });
  assert.deepEqual(seen, [37]);
});

test('item 22: the largest roulette return is a safe integer (R207/R236)', () => {
  const r = roulette.spaceReturn(roulette.parseSpace('17'), 17, 1_000_000_000);
  assert.equal(r, 36_000_000_000);
  assert.ok(Number.isSafeInteger(r));
});

test('roulette autocomplete lists the named spaces first, filters by prefix and answers without a database', async () => {
  let answered = null;
  const ask = async (q) => {
    answered = null;
    await roulette.autocomplete({ options: { getFocused: () => q }, responded: false, respond: async (list) => { answered = list; } });
    return answered;
  };
  const all = await ask('');
  assert.equal(all.length, 25);
  assert.deepEqual(all.slice(0, 10).map((c) => c.value), ['red', 'black', 'green', 'even', 'odd', 'low', 'high', '1-12', '13-24', '25-36']);
  assert.equal(all[0].name, 'Red (2× return)');
  assert.deepEqual((await ask('1')).map((c) => c.value).slice(0, 4), ['1-12', '13-24', '1', '10']);
  assert.deepEqual((await ask('G')).map((c) => c.value), ['green']);
  assert.equal((await ask('17'))[0].name, 'Number 17 (36× return)');
  assert.equal((await ask('0'))[0].name, 'Number 0 (36× return)');
  assert.deepEqual(await ask('zzz'), []);
  assert.deepEqual((await ask(' 1 3 -')).map((c) => c.value), ['13-24']);
});

// ═══════════════════════════ slots: pure helpers (item 23) ═══════════════════════════

test('item 23: slotsPayout multiplies the lines, floors, and pays at least one coin of profit', () => {
  const apples = grid3('🍎');
  assert.equal(slots.checkWins(apples).length, 1);
  assert.equal(slots.slotsPayout(apples, 1).payout, 2);
  assert.equal(slots.slotsPayout(apples, 100).payout, 150);
  assert.equal(slots.slotsPayout(apples, 3).payout, 4, 'floor(4.5) = 4');
  const none = ['🍎', '🍋', '🍒', '🍇', '💎', '7️⃣', '🍎', '🍋', '🍒'].map((e) => slots.SYMBOLS.find((s) => s.emoji === e));
  assert.deepEqual(slots.slotsPayout(none, 100), { payout: 0, multiplier: 0, wins: [] });
  const diamonds = grid3('💎');
  assert.equal(slots.slotsPayout(diamonds, 100).payout, 500);
  const allDiamonds = Array(9).fill(slots.SYMBOLS[0]);
  const jackpot = slots.slotsPayout(allDiamonds, 1_000_000_000);
  assert.equal(jackpot.wins.length, 5);
  assert.equal(jackpot.payout, 25_000_000_000);
  assert.ok(Number.isSafeInteger(jackpot.payout));
  for (const sym of slots.SYMBOLS) {
    const p = slots.slotsPayout(grid3(sym.emoji, sym.emoji === '🍋' ? '🍎' : '🍋'), 1).payout;
    assert.ok(p >= 2, `${sym.emoji} at bet 1 returns ${p}`);
  }
});

test('slots: the weighted walk reproduces the weights exactly and every cell is drawn independently', () => {
  assert.equal(slots.TOTAL_WEIGHT, 21);
  const counts = new Map();
  for (let r = 0; r < slots.TOTAL_WEIGHT; r++) {
    const sym = slots.spinSymbol(() => r);
    counts.set(sym.emoji, (counts.get(sym.emoji) ?? 0) + 1);
  }
  for (const sym of slots.SYMBOLS) assert.equal(counts.get(sym.emoji), sym.weight, sym.emoji);
  const seen = [];
  const grid = slots.spinGrid((n) => { seen.push(n); return 0; });
  assert.equal(grid.length, 9);
  assert.deepEqual(seen, Array(9).fill(21));
  assert.ok(grid.every((s) => s.emoji === '💎'));
  assert.equal(slots.LINES.length, 5);
  assert.match(slots.renderGrid(grid3('🍎'), slots.checkWins(grid3('🍎'))), /◀ WIN/);
});

// ═══════════════════════════ payloads and exports (items 24, 25) ═══════════════════════════

const charCount = (node) => {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
};

test('item 24: module, permission level, gating and payload budgets of the three games', () => {
  for (const [name, mod, budget] of [['blackjack', bj, 100], ['roulette', roulette, 160], ['slots', slots, 70]]) {
    assert.equal(mod.module, 'economy', `${name} module`);
    assert.equal(mod.permissionLevel, PERMISSION_LEVELS.MEMBER, `${name} permission`);
    assert.notEqual(mod.gated, false, `${name} stays gated by the economy module`);
    assert.equal(typeof mod.execute, 'function');
    const json = buildCommandPayload(mod);
    assert.equal(json.name, name);
    assert.ok(charCount(json) <= budget, `${name} is ${charCount(json)} characters (budget ${budget})`);
    assert.equal(json.default_member_permissions ?? null, null);
    assert.deepEqual(json.contexts, [0], `${name} is guild-only`);
    const bet = json.options.find((o) => o.name === 'bet');
    assert.equal(bet.required, true);
    assert.equal(bet.min_value, 1);
    assert.equal(bet.max_value, COIN_MAX);
  }
  const space = buildCommandPayload(roulette).options.find((o) => o.name === 'space');
  assert.equal(space.autocomplete, true);
  assert.equal(space.max_length, 10);
  assert.equal(typeof roulette.autocomplete, 'function');
});

test('item 25: the bj customId stays within 100 characters and parses back', () => {
  const id = buildCustomId('bj', '1'.repeat(20), 'double');
  assert.ok(id.length <= 100);
  assert.deepEqual(parseCustomId(id), { prefix: 'bj', params: ['1'.repeat(20), 'double'] });
  const buttons = bj.buildButtons('123456789012345678', { canDouble: true }).toJSON().components;
  assert.deepEqual(buttons.map((b) => b.custom_id), ['bj:123456789012345678:hit', 'bj:123456789012345678:stand', 'bj:123456789012345678:double']);
  assert.ok(buttons.every((b) => !b.disabled));
  assert.equal(bj.buildButtons('1', { canDouble: false }).toJSON().components[2].disabled, true);
  assert.ok(bj.buildButtons('1', { canDouble: true, disabled: true }).toJSON().components.every((b) => b.disabled));
});

test('static scan: the games never import the database client and every settleBet call passes its stake', () => {
  for (const file of ROOT_FILES) {
    const text = readFileSync(file, 'utf8');
    assert.ok(!/database\/client/.test(text), `${file} imports the database client`);
    assert.ok(!/\bconsole\.(log|error|warn|info|debug)\(/.test(text), `${file} uses console`);
    assert.ok(!/getOrCreate|applyBet|getBalance/.test(text), `${file} still uses the removed economy exports`);
    for (const line of text.split(/\r?\n/).filter((l) => /\.settleBet\(/.test(l))) assert.match(line, /\{ stake[:,} ]/, `settleBet without a stake: ${line.trim()}`);
  }
});

// ═══════════════════════════ shared start sequence (all three games) ═══════════════════════════

const GAMES = [
  ['blackjack', bj, () => ({ deck: stack(10, 9, 10, 8) })],
  ['roulette', roulette, () => ({ spin: () => 17 })],
  ['slots', slots, () => ({ spin: () => grid3('🍎') })],
];

for (const [name, mod, extras] of GAMES) {
  test(`${name}: acknowledges before the first database access, then reserves, in that order (R088, R007)`, async () => {
    const world = mkWorld({ ...extras() });
    const it = mkSlash({ timeline: world.timeline, space: 'red' });
    await mod.execute(it, null, world.deps);
    const order = world.timeline.filter((e) => e === 'lock' || e === 'defer' || e.startsWith('reserve:'));
    assert.deepEqual(order, ['lock', 'defer', 'reserve:100']);
    assert.deepEqual(it.calls.filter((c) => c[0] === 'reply'), [], 'no ephemeral reply on the happy path');
    for (const c of it.collectors) c.stop('idle');
  });

  test(`${name}: a channel outside the allow list is refused privately before any acknowledgement, lock or database access`, async () => {
    const world = mkWorld({ ...extras(), settings: { economyGameChannelIds: ['700', '701'], economyGameChannelMode: 'ALLOW' } });
    const it = mkSlash({ timeline: world.timeline, channelId: '500', guild: mkGuild(['700', '701']) });
    await mod.execute(it, null, world.deps);
    assert.equal(it.deferred, false);
    assert.deepEqual(world.timeline, ['reply']);
    const reply = lastPrivate(it);
    assert.equal(reply.ephemeral, true);
    assert.equal(embedOf(reply).description, 'Casino games can only be played in <#700>, <#701>.');
    assert.equal(world.state.balance, 1000);
  });

  test(`${name}: a listed channel in deny mode is refused, other channels play`, async () => {
    const denied = mkWorld({ ...extras(), settings: { economyGameChannelIds: ['700'], economyGameChannelMode: 'DENY' } });
    const it = mkSlash({ timeline: denied.timeline, channelId: '700', guild: mkGuild(['700']) });
    await mod.execute(it, null, denied.deps);
    assert.deepEqual(denied.timeline, ['reply']);
    assert.equal(embedOf(lastPrivate(it)).description, 'Casino games are not allowed in this channel.');

    const open = mkWorld({ ...extras(), settings: { economyGameChannelIds: ['700'], economyGameChannelMode: 'DENY' } });
    const ok = mkSlash({ timeline: open.timeline, channelId: '500', guild: mkGuild(['700']) });
    await mod.execute(ok, null, open.deps);
    assert.ok(open.timeline.includes('reserve:100'));
    // a thread of the denied channel counts as that channel
    const thread = mkSlash({ channelId: '999', guild: mkGuild(['700']) });
    thread.channel = { id: '999', parentId: '700', isThread: () => true };
    const w2 = mkWorld({ ...extras(), settings: { economyGameChannelIds: ['700'], economyGameChannelMode: 'DENY' } });
    await mod.execute(thread, null, w2.deps);
    assert.equal(embedOf(lastPrivate(thread)).description, 'Casino games are not allowed in this channel.');
  });

  test(`${name}: a bet above the max bet is refused before the defer`, async () => {
    const world = mkWorld({ ...extras(), settings: { economyMaxBet: 50 } });
    const it = mkSlash({ timeline: world.timeline, bet: 51 });
    await mod.execute(it, null, world.deps);
    assert.equal(it.deferred, false);
    assert.deepEqual(world.timeline, ['reply']);
    const e = embedOf(lastPrivate(it));
    assert.match(e.title, /Bet Too High/);
    assert.match(e.description, /maximum bet on this server is 🪙 \*\*50\*\* coins/);
  });

  test(`${name}: a second game while one runs is refused with Game In Progress and never reserves`, async () => {
    const world = mkWorld({ ...extras() });
    world.deps.tryLockCasino(G(1), U(1)); // the member already has a game running
    const it = mkSlash({ timeline: world.timeline });
    await mod.execute(it, null, world.deps);
    assert.equal(it.deferred, false);
    assert.equal(world.state.reserves.length, 0);
    assert.match(embedOf(lastPrivate(it)).title, /Game In Progress/);
    assert.equal(lastPrivate(it).ephemeral, true);
  });

  test(`${name}: insufficient funds is a private error after the defer, nothing settles, the lock is released`, async () => {
    const world = mkWorld({ ...extras(), balance: 40 });
    const it = mkSlash({ timeline: world.timeline, bet: 100 });
    await mod.execute(it, null, world.deps);
    assert.deepEqual(it.calls.map((c) => c[0]), ['deferReply', 'deleteReply', 'followUp']);
    const follow = it.calls.at(-1)[1];
    assert.equal(follow.ephemeral, true);
    assert.match(embedOf(follow).title, /Insufficient Funds/);
    assert.match(embedOf(follow).description, /only have 🪙 \*\*40\*\* coins/);
    assert.equal(world.state.settles.length, 0);
    assert.equal(world.state.balance, 40);
    assert.equal(world.state.locked.size, 0);
  });

  test(`${name}: a database failure while reserving takes nothing, reports privately and unlocks`, async () => {
    const world = mkWorld({ ...extras() });
    world.state.failReserve = true;
    const it = mkSlash({ timeline: world.timeline });
    await mod.execute(it, null, world.deps);
    assert.equal(world.state.settles.length, 0);
    assert.equal(world.state.balance, 1000);
    assert.equal(world.state.locked.size, 0);
    assert.equal(it.calls.at(-1)[1].ephemeral, true);
    assert.match(embedOf(it.calls.at(-1)[1]).title, /Economy Unavailable/);
  });

  test(`${name}: when the settings cannot be loaded the game is refused privately (no defaults, no lock)`, async () => {
    const world = mkWorld({ ...extras() });
    world.deps.loadSettings = async () => null;
    const it = mkSlash({ timeline: world.timeline });
    await mod.execute(it, null, world.deps);
    assert.equal(it.deferred, false);
    assert.equal(world.state.reserves.length, 0);
    assert.equal(world.state.locked.size, 0);
    assert.match(embedOf(lastPrivate(it)).title, /Temporarily Unavailable/);
  });
}

// ═══════════════════════════ slots flows ═══════════════════════════

test('slots: a win settles the gross payout with the stake, shows payout / net / before -> after (R240)', async () => {
  const world = mkWorld({ spin: () => grid3('🍎') });
  const it = mkSlash({ timeline: world.timeline, bet: 100 });
  await slots.execute(it, null, world.deps);
  assert.deepEqual(world.state.settles, [{ payout: 150, stake: 100 }]);
  assert.equal(world.state.balance, 1050);
  const e = embedOf(lastEdit(it));
  assert.equal(fieldOf(e, 'Bet'), '🪙 **100** coins');
  assert.equal(fieldOf(e, 'Payout'), '🪙 **150** coins');
  assert.equal(fieldOf(e, 'Net'), '+🪙 **50** coins');
  assert.equal(fieldOf(e, 'Balance'), '1,000 → 1,050 coins');
  assert.match(e.description, /1 winning line \(1\.5× payout\) — \+🪙 \*\*50\*\* coins!/);
  assert.equal(e.color, 0x57f287);
  assert.deepEqual(world.timeline.slice(-3), ['settle:150/100', 'edit', 'unlock']);
  assert.equal(world.state.locked.size, 0);
});

test('slots: at bet 1 an apple line returns 2 (net +1), never "wins" the stake back', async () => {
  const world = mkWorld({ spin: () => grid3('🍎') });
  const it = mkSlash({ timeline: world.timeline, bet: 1 });
  await slots.execute(it, null, world.deps);
  assert.deepEqual(world.state.settles, [{ payout: 2, stake: 1 }]);
  assert.equal(fieldOf(embedOf(lastEdit(it)), 'Net'), '+🪙 **1** coins');
});

test('slots: a loss settles 0, shows the negative net and the lower balance', async () => {
  const none = ['🍎', '🍋', '🍒', '🍇', '💎', '7️⃣', '🍎', '🍋', '🍒'].map((e) => slots.SYMBOLS.find((s) => s.emoji === e));
  const world = mkWorld({ spin: () => none });
  const it = mkSlash({ timeline: world.timeline, bet: 100 });
  await slots.execute(it, null, world.deps);
  assert.deepEqual(world.state.settles, [{ payout: 0, stake: 100 }]);
  const e = embedOf(lastEdit(it));
  assert.equal(fieldOf(e, 'Payout'), '🪙 **0** coins');
  assert.equal(fieldOf(e, 'Net'), '−🪙 **100** coins');
  assert.equal(fieldOf(e, 'Balance'), '1,000 → 900 coins');
  assert.equal(e.color, 0xed4245);
});

test('slots: the max payout caps the winnings, settleBet gets the stake and the embed says so', async () => {
  const world = mkWorld({ spin: () => grid3('💎'), settings: { economyMaxPayout: 120 } });
  const it = mkSlash({ timeline: world.timeline, bet: 100 });
  await slots.execute(it, null, world.deps);
  assert.deepEqual(world.state.settles, [{ payout: 500, stake: 100 }]);
  assert.equal(world.state.balance, 900 + 220, 'stake + max payout comes back');
  const e = embedOf(lastEdit(it));
  assert.equal(fieldOf(e, 'Payout'), '🪙 **220** coins');
  assert.equal(fieldOf(e, 'Net'), '+🪙 **120** coins');
  assert.match(e.description, /Winnings capped at 🪙 \*\*120\*\* coins by the server's max payout\./);
});

test('slots: a failed payout is shown, logged, never thrown, and the lock is released', async () => {
  const world = mkWorld({ spin: () => grid3('🍎') });
  world.state.failSettle = true;
  const it = mkSlash({ timeline: world.timeline, bet: 100 });
  await assert.doesNotReject(() => slots.execute(it, null, world.deps));
  assert.match(fieldOf(embedOf(lastEdit(it)), 'Balance'), /Payout failed/);
  assert.equal(world.state.locked.size, 0);
});

test('slots: the result embed failing to send still leaves the bet settled and the lock released', async () => {
  const world = mkWorld({ spin: () => grid3('🍎') });
  const it = mkSlash({ timeline: world.timeline, bet: 100, editFails: true });
  await assert.doesNotReject(() => slots.execute(it, null, world.deps));
  assert.deepEqual(world.state.settles, [{ payout: 150, stake: 100 }]);
  assert.equal(world.state.locked.size, 0);
});

test('slots: an unexpected failure between the reserve and the settlement refunds the stake once and unlocks', async () => {
  const world = mkWorld({ spin: () => { throw new Error('reel jam'); } });
  const it = mkSlash({ timeline: world.timeline, bet: 100 });
  await slots.execute(it, null, world.deps);
  assert.deepEqual(world.state.settles, [{ payout: 100, stake: 100 }]);
  assert.equal(world.state.balance, 1000);
  assert.equal(world.state.locked.size, 0);
});

// ═══════════════════════════ roulette flows ═══════════════════════════

test('roulette: an invalid space is refused privately before any acknowledgement, lock or database access (R082)', async () => {
  for (const bad of ['12 red', '3.9', '00', '37', '']) {
    const world = mkWorld({ spin: () => 5 });
    const it = mkSlash({ timeline: world.timeline, space: bad });
    await roulette.execute(it, null, world.deps);
    assert.equal(it.deferred, false);
    assert.deepEqual(world.timeline, ['reply'], bad);
    const e = embedOf(lastPrivate(it));
    assert.match(e.title, /Invalid Space/);
    assert.match(e.description, /is not a valid bet\. Use red, black, green, even, odd, low, high, 1-12, 13-24, 25-36, or a number from 0 to 36\./);
  }
});

test('roulette: the bet is reserved before the animation, the spin settles once after it (R007)', async () => {
  const world = mkWorld({ spin: () => 17 });
  const it = mkSlash({ timeline: world.timeline, bet: 10, space: '17' });
  await roulette.execute(it, null, world.deps);
  assert.deepEqual(world.timeline.filter((e) => e !== 'unlock' && e !== 'lock'), ['defer', 'reserve:10', 'edit', 'edit', 'edit', 'settle:360/10', 'edit']);
  assert.equal(world.state.balance, 1000 - 10 + 360);
  const e = embedOf(lastEdit(it));
  assert.equal(e.title, '🎰 The ball lands on 17 ⚫ Black');
  assert.equal(fieldOf(e, 'Your bet'), 'Number 17 — 🪙 **10** coins');
  assert.equal(fieldOf(e, 'Bet'), '🪙 **10** coins');
  assert.equal(fieldOf(e, 'Payout'), '🪙 **360** coins');
  assert.equal(fieldOf(e, 'Net'), '+🪙 **350** coins');
  assert.equal(fieldOf(e, 'Balance'), '1,000 → 1,350 coins');
  assert.match(e.description, /You win \+🪙 \*\*350\*\* coins!/);
  assert.equal(world.state.locked.size, 0);
});

test('roulette: green and 0 are the same 36x bet (R081)', async () => {
  for (const space of ['green', '0']) {
    const world = mkWorld({ spin: () => 0 });
    const it = mkSlash({ timeline: world.timeline, bet: 100, space });
    await roulette.execute(it, null, world.deps);
    assert.deepEqual(world.state.settles, [{ payout: 3600, stake: 100 }], space);
    assert.equal(embedOf(lastEdit(it)).title, '🎰 The ball lands on 0 🟢 Green');
    assert.equal(fieldOf(embedOf(lastEdit(it)), 'Your bet'), 'Green (0) — 🪙 **100** coins');
  }
});

test('roulette: a losing spin settles 0 and shows the loss', async () => {
  const world = mkWorld({ spin: () => 0 });
  const it = mkSlash({ timeline: world.timeline, bet: 100, space: 'red' });
  await roulette.execute(it, null, world.deps);
  assert.deepEqual(world.state.settles, [{ payout: 0, stake: 100 }]);
  const e = embedOf(lastEdit(it));
  assert.match(e.description, /You lose/);
  assert.equal(fieldOf(e, 'Net'), '−🪙 **100** coins');
  assert.equal(fieldOf(e, 'Balance'), '1,000 → 900 coins');
});

test('roulette: animation edits failing never block the settlement and the lock is released', async () => {
  const world = mkWorld({ spin: () => 17 });
  const it = mkSlash({ timeline: world.timeline, bet: 10, space: '17', editFails: true });
  await assert.doesNotReject(() => roulette.execute(it, null, world.deps));
  assert.deepEqual(world.state.settles, [{ payout: 360, stake: 10 }]);
  assert.equal(world.state.locked.size, 0);
});

test('roulette: the max payout caps a 36x win at stake + max payout and says so', async () => {
  const world = mkWorld({ spin: () => 17, settings: { economyMaxPayout: 100 } });
  const it = mkSlash({ timeline: world.timeline, bet: 10, space: '17' });
  await roulette.execute(it, null, world.deps);
  assert.equal(world.state.balance, 990 + 110);
  const e = embedOf(lastEdit(it));
  assert.equal(fieldOf(e, 'Payout'), '🪙 **110** coins');
  assert.match(e.description, /Winnings capped at 🪙 \*\*100\*\* coins/);
});

test('roulette: a failed payout is shown, logged and never thrown', async () => {
  const world = mkWorld({ spin: () => 17 });
  world.state.failSettle = true;
  const it = mkSlash({ timeline: world.timeline, bet: 10, space: '17' });
  await assert.doesNotReject(() => roulette.execute(it, null, world.deps));
  assert.match(fieldOf(embedOf(lastEdit(it)), 'Balance'), /Payout failed/);
  assert.equal(world.state.locked.size, 0);
});

test('roulette: two games by the same member at once: the second is refused, the first settles once', async () => {
  const world = mkWorld({ spin: () => 17 });
  const a = mkSlash({ timeline: world.timeline, bet: 10, space: '17' });
  const b = mkSlash({ timeline: world.timeline, bet: 10, space: '17' });
  await Promise.all([roulette.execute(a, null, world.deps), roulette.execute(b, null, world.deps)]);
  assert.equal(world.state.settles.length, 1);
  assert.equal(world.state.reserves.length, 1);
  assert.match(embedOf(lastPrivate(b)).title, /Game In Progress/);
});

// ═══════════════════════════ blackjack flows ═══════════════════════════

test('blackjack: a natural settles at once (3:2), without buttons or a collector (R078)', async () => {
  const world = mkWorld({ deck: stack('A', 'K', 5, 6) });
  const { it } = await startHand(world, { bet: 100 });
  assert.equal(it.collectors.length, 0);
  assert.deepEqual(world.state.settles, [{ payout: 250, stake: 100 }]);
  assert.equal(world.state.balance, 1150);
  const payload = lastEdit(it);
  assert.deepEqual(payload.components, []);
  const e = embedOf(payload);
  assert.equal(e.description, '🃏 Blackjack! Paid 3:2.');
  assert.equal(fieldOf(e, 'Net'), '+🪙 **150** coins');
  assert.equal(fieldOf(e, 'Balance'), '1,000 → 1,150 coins');
  assert.equal(fieldOf(e, 'Dealer'), '♠️5  ♠️6', 'the dealer is revealed');
  assert.equal(world.state.locked.size, 0);
});

test('blackjack: both naturals push and return exactly the bet', async () => {
  const world = mkWorld({ deck: stack('A', 'K', 'A', 'Q') });
  const { it } = await startHand(world, { bet: 100 });
  assert.deepEqual(world.state.settles, [{ payout: 100, stake: 100 }]);
  assert.equal(world.state.balance, 1000);
  assert.equal(embedOf(lastEdit(it)).description, '🤝 Both have blackjack — push.');
  assert.equal(world.state.locked.size, 0);
});

test('blackjack: the dealer peeks, so a dealer natural ends the hand before the player acts, even with 21 in hand (R078)', async () => {
  const world = mkWorld({ deck: stack(10, 9, 'A', 'K') });
  const { it } = await startHand(world, { bet: 100 });
  assert.equal(it.collectors.length, 0, 'no buttons to press');
  assert.deepEqual(world.state.settles, [{ payout: 0, stake: 100 }]);
  assert.equal(world.state.balance, 900);
  assert.equal(embedOf(lastEdit(it)).description, '🂡 Dealer has blackjack.');
  assert.equal(world.state.locked.size, 0);
});

test('blackjack: a normal hand shows hit / stand / double and hides the hole card', async () => {
  const world = mkWorld({ deck: stack(10, 9, 10, 8) });
  const { it, collector } = await startHand(world, { bet: 100 });
  assert.equal(it.collectors.length, 1);
  assert.equal(collector.options.idle, 60_000);
  assert.equal(collector.options.time, 600_000);
  const payload = lastEdit(it);
  const e = embedOf(payload);
  assert.equal(fieldOf(e, 'Dealer'), '♠️10  🂠');
  assert.equal(e.fields.find((f) => f.name.startsWith('Dealer')).name, 'Dealer (?)');
  assert.equal(e.description, 'Hit, stand or double down.');
  assert.equal(fieldOf(e, 'Bet'), '🪙 **100** coins');
  const buttons = payload.components[0].toJSON().components;
  assert.deepEqual(buttons.map((b) => b.custom_id), [`bj:${it.id}:hit`, `bj:${it.id}:stand`, `bj:${it.id}:double`]);
  assert.equal(buttons[2].disabled, false);
  assert.equal(world.state.locked.size, 1, 'the collector owns the lock');
  assert.deepEqual(world.state.settles, []);
  collector.stop('idle');
});

test('blackjack: Stand settles once, acknowledges first, edits through editReply and unlocks after the payout', async () => {
  const world = mkWorld({ deck: stack(10, 9, 10, 8) });
  const { it, collector, click } = await startHand(world, { bet: 100 });
  world.timeline.length = 0;
  const stand = click('stand');
  await collector.deliver(stand);
  assert.deepEqual(world.timeline.slice(0, 3), ['ack', 'settle:200/100', 'edit'], 'the acknowledgement is the first thing that happens');
  assert.equal(world.timeline.at(-1), 'unlock', 'the lock is released after the settlement');
  assert.deepEqual(verbs(stand), ['deferUpdate']);
  assert.equal(collector.reason, 'done');
  assert.deepEqual(world.state.settles, [{ payout: 200, stake: 100 }]);
  assert.equal(world.state.balance, 1100);
  const payload = lastEdit(it);
  assert.deepEqual(payload.components, []);
  const e = embedOf(payload);
  assert.equal(e.description, '🎉 You win!');
  assert.equal(fieldOf(e, 'Net'), '+🪙 **100** coins');
  assert.equal(fieldOf(e, 'Balance'), '1,000 → 1,100 coins');
  assert.equal(fieldOf(e, 'Dealer'), '♠️10  ♠️8');
  assert.equal(world.state.locked.size, 0);
});

test('blackjack: the idle timer firing while the payout is in flight neither forfeits the hand nor releases the lock early', async () => {
  const world = mkWorld({ deck: stack(10, 9, 10, 8) });
  const { it, collector, click } = await startHand(world, { bet: 100 });
  const gate = defer();
  world.state.settleGate = gate.promise;
  const delivered = collector.deliver(click('stand'));
  await tick();
  assert.equal(world.state.settles.length, 1, 'the settlement has started');
  collector.stop('idle'); // too late: the hand was claimed and the collector stopped before the first await
  assert.equal(collector.reason, 'done');
  assert.equal(world.state.locked.size, 1, 'the lock is held until the payout is in');
  gate.resolve();
  await delivered;
  assert.equal(world.state.settles.length, 1);
  assert.equal(world.state.balance, 1100);
  assert.equal(embedOf(lastEdit(it)).description, '🎉 You win!');
  assert.equal(world.state.locked.size, 0);
});

test('blackjack: a double Stand (two overlapping clicks) settles exactly once (R016, R017, R079)', async () => {
  const world = mkWorld({ deck: stack(10, 9, 10, 8) });
  const { collector, click } = await startHand(world, { bet: 100 });
  const first = click('stand');
  const second = click('stand');
  await Promise.all([collector.deliver(first), collector.deliverRaw(second)]);
  assert.equal(world.state.settles.length, 1);
  assert.equal(world.state.balance, 1100);
  assert.deepEqual(verbs(first), ['deferUpdate']);
  assert.deepEqual(verbs(second), ['deferUpdate'], 'the second click is acknowledged and dropped');
});

test('blackjack: Stand and Hit racing settle once; a click after the hand is over is only acknowledged', async () => {
  const world = mkWorld({ deck: stack(10, 9, 10, 8, 2) });
  const { collector, click } = await startHand(world, { bet: 100 });
  const stand = click('stand');
  const hit = click('hit');
  await Promise.all([collector.deliver(stand), collector.deliverRaw(hit)]);
  assert.equal(world.state.settles.length, 1);
  const late = click('double');
  await collector.deliverRaw(late);
  assert.deepEqual(verbs(late), ['deferUpdate']);
  assert.equal(world.state.settles.length, 1);
  assert.equal(world.state.reserves.length, 1, 'no late double-down reservation');
});

test('blackjack: Hit continues the hand (double disabled), 21 stands automatically, a bust loses the stake', async () => {
  const world = mkWorld({ deck: stack(10, 5, 10, 7, 2, 4, 'K') });
  const { it, collector, click } = await startHand(world, { bet: 100 });
  const hit = click('hit');
  await collector.deliver(hit);
  assert.deepEqual(verbs(hit), ['deferUpdate']);
  assert.equal(world.state.settles.length, 0);
  let payload = lastEdit(it);
  assert.equal(embedOf(payload).description, 'Hit or stand.');
  assert.equal(payload.components[0].toJSON().components[2].disabled, true, 'no double after a hit');
  assert.equal(fieldOf(embedOf(payload), 'You'), '♠️10  ♠️5  ♠️2');
  const hit2 = click('hit'); // 17 -> 21: stands automatically
  await collector.deliver(hit2);
  assert.equal(collector.reason, 'done');
  assert.deepEqual(world.state.settles, [{ payout: 200, stake: 100 }], '21 beats the dealer 17');
  payload = lastEdit(it);
  assert.equal(embedOf(payload).description, '🎉 You win!');

  const bust = mkWorld({ deck: stack(10, 6, 10, 7, 'K') });
  const h = await startHand(bust, { bet: 100 });
  await h.collector.deliver(h.click('hit'));
  assert.deepEqual(bust.state.settles, [{ payout: 0, stake: 100 }]);
  assert.equal(embedOf(lastEdit(h.it)).description, '💥 You busted.');
  assert.equal(fieldOf(embedOf(lastEdit(h.it)), 'Dealer'), '♠️10  ♠️7', 'the dealer does not draw after a bust');
  assert.equal(bust.state.balance, 900);
  assert.equal(bust.state.locked.size, 0);
});

test('blackjack: the dealer hits soft 17 and stands on the resulting hard 17', async () => {
  const world = mkWorld({ deck: stack(10, 10, 'A', 6, 10) });
  const { it, collector, click } = await startHand(world, { bet: 100 });
  await collector.deliver(click('stand'));
  const e = embedOf(lastEdit(it));
  assert.equal(e.fields.find((f) => f.name.startsWith('Dealer')).name, 'Dealer (17)');
  assert.equal(fieldOf(e, 'Dealer'), '♠️A  ♠️6  ♠️10');
  assert.equal(e.description, '🎉 You win!');
});

test('blackjack: double down reserves the bet again, draws one card, settles 2x stake once (stake 2x bet)', async () => {
  const world = mkWorld({ deck: stack(6, 5, 10, 8, 10) });
  const { it, collector, click } = await startHand(world, { bet: 100 });
  world.timeline.length = 0;
  const dbl = click('double');
  await collector.deliver(dbl);
  assert.deepEqual(world.timeline.slice(0, 4), ['ack', 'reserve:100', 'settle:400/200', 'edit'], 'acknowledged before the second reservation');
  assert.deepEqual(world.state.reserves, [100, 100]);
  assert.deepEqual(world.state.settles, [{ payout: 400, stake: 200 }]);
  assert.equal(world.state.balance, 1200);
  const e = embedOf(lastEdit(it));
  assert.equal(e.description, '🎉 You win! (doubled down)');
  assert.equal(fieldOf(e, 'Bet'), '🪙 **200** coins');
  assert.equal(fieldOf(e, 'Payout'), '🪙 **400** coins');
  assert.equal(fieldOf(e, 'Net'), '+🪙 **200** coins');
  assert.equal(fieldOf(e, 'Balance'), '1,000 → 1,200 coins', 'before comes from the first reserve');
  assert.equal(fieldOf(e, 'You'), '♠️6  ♠️5  ♠️10');
  assert.equal(world.state.locked.size, 0);
});

test('blackjack: a lost double down costs both stakes', async () => {
  const world = mkWorld({ deck: stack(6, 5, 10, 8, 2) });
  const { collector, click } = await startHand(world, { bet: 100 });
  await collector.deliver(click('double'));
  assert.deepEqual(world.state.settles, [{ payout: 0, stake: 200 }]);
  assert.equal(world.state.balance, 800);
});

test('blackjack: a double down without the coins is refused privately and the hand goes on', async () => {
  const world = mkWorld({ balance: 150, deck: stack(6, 5, 10, 8, 10) });
  const { it, collector, click } = await startHand(world, { bet: 100 });
  const dbl = click('double');
  await collector.deliver(dbl);
  assert.deepEqual(verbs(dbl), ['deferUpdate', 'followUp']);
  const follow = dbl.calls[1][1];
  assert.equal(follow.ephemeral, true);
  assert.match(embedOf(follow).title, /Cannot Double Down/);
  assert.match(embedOf(follow).description, /need another 🪙 \*\*100\*\* coins/);
  assert.equal(world.state.settles.length, 0);
  assert.equal(collector.ended, false, 'the hand is still live');
  assert.equal(world.state.locked.size, 1);
  assert.equal(world.state.balance, 50);
  collector.stop('idle');
  assert.equal(lastEdit(it).components.length, 0);
});

test('blackjack: the double button is disabled when 2x the bet would pass the max bet; a forced double click does nothing', async () => {
  const world = mkWorld({ deck: stack(6, 5, 10, 8, 10), settings: { economyMaxBet: 150 } });
  const { it, collector, click } = await startHand(world, { bet: 100 });
  assert.equal(lastEdit(it).components[0].toJSON().components[2].disabled, true);
  assert.equal(embedOf(lastEdit(it)).description, 'Hit or stand.');
  const dbl = click('double');
  await collector.deliver(dbl);
  assert.deepEqual(verbs(dbl), ['deferUpdate']);
  assert.deepEqual(world.state.reserves, [100]);
  collector.stop('idle');
});

test('blackjack: an abandoned hand is forfeited: no settlement, the stake stays taken, the lock is released (R018-R020, R077)', async () => {
  const world = mkWorld({ deck: stack(10, 9, 10, 8) });
  const { it, collector, click } = await startHand(world, { bet: 100 });
  collector.stop('idle');
  await tick();
  assert.equal(world.state.settles.length, 0, 'forfeit is not a settlement');
  assert.equal(world.state.balance, 900);
  assert.equal(world.state.locked.size, 0);
  const payload = lastEdit(it);
  assert.deepEqual(payload.components, []);
  const e = embedOf(payload);
  assert.match(e.description, /walked away from the table\. Your bet of 🪙 \*\*100\*\* coins is forfeited\./);
  assert.equal(e.color, 0xfee75c);
  // a click that was already in flight when the collector ended is acknowledged and ignored
  const late = click('stand');
  await collector.deliverRaw(late);
  assert.deepEqual(verbs(late), ['deferUpdate']);
  assert.equal(world.state.settles.length, 0);
});

test('blackjack: the collector ending for any other reason also forfeits (total time, message deleted)', async () => {
  for (const reason of ['time', 'messageDelete', 'channelDelete']) {
    const world = mkWorld({ deck: stack(10, 9, 10, 8) });
    const { collector } = await startHand(world, { bet: 100 });
    collector.stop(reason);
    await tick();
    assert.equal(world.state.settles.length, 0, reason);
    assert.equal(world.state.locked.size, 0, reason);
    assert.equal(world.state.balance, 900, reason);
  }
});

test('blackjack: the collector ending while the double-down reservation is in flight refunds the extra stake only', async () => {
  const world = mkWorld({ deck: stack(6, 5, 10, 8, 10) });
  const { it, collector, click } = await startHand(world, { bet: 100 });
  const gate = defer();
  world.state.gate = gate.promise;
  const dbl = click('double');
  const delivered = collector.deliver(dbl);
  await tick();
  assert.deepEqual(world.state.reserves, [100, 100], 'the second reserve is waiting');
  collector.stop('idle'); // the hand is forfeited while the database call is still pending
  gate.resolve();
  await delivered;
  await tick();
  assert.deepEqual(world.state.settles, [{ payout: 100, stake: 100 }], 'only the extra stake goes back');
  assert.equal(world.state.balance, 900, 'the original bet stays forfeited');
  assert.equal(world.state.locked.size, 0);
  assert.match(embedOf(lastEdit(it)).description, /forfeited/);
});

test('blackjack: only the player can press the buttons; a stranger gets a private reply and nothing else happens', async () => {
  const world = mkWorld({ deck: stack(10, 9, 10, 8) });
  const { collector, click } = await startHand(world, { bet: 100 });
  const stranger = click('stand', U(2));
  await collector.deliver(stranger);
  assert.deepEqual(verbs(stranger), ['reply']);
  assert.equal(stranger.calls[0][1].ephemeral, true);
  assert.equal(world.state.settles.length, 0);
  assert.equal(collector.ended, false);
  collector.stop('idle');
});

test('blackjack: a push with a max payout returns exactly the stake; a capped win says so', async () => {
  const push = mkWorld({ deck: stack(10, 8, 10, 8), settings: { economyMaxPayout: 5 } });
  const a = await startHand(push, { bet: 100 });
  await a.collector.deliver(a.click('stand'));
  assert.deepEqual(push.state.settles, [{ payout: 100, stake: 100 }]);
  assert.equal(push.state.balance, 1000, 'a push is never cut by the max payout');
  assert.equal(embedOf(lastEdit(a.it)).description, '🤝 Push — your bet is returned.');
  assert.equal(fieldOf(embedOf(lastEdit(a.it)), 'Net'), '±🪙 **0** coins');

  const win = mkWorld({ deck: stack(10, 9, 10, 8), settings: { economyMaxPayout: 5 } });
  const b = await startHand(win, { bet: 100 });
  await b.collector.deliver(b.click('stand'));
  assert.equal(win.state.balance, 1005);
  const e = embedOf(lastEdit(b.it));
  assert.equal(fieldOf(e, 'Payout'), '🪙 **105** coins');
  assert.equal(fieldOf(e, 'Net'), '+🪙 **5** coins');
  assert.match(e.description, /Winnings capped at 🪙 \*\*5\*\* coins by the server's max payout\./);

  const nat = mkWorld({ deck: stack('A', 'K', 5, 6), settings: { economyMaxPayout: 100 } });
  const c = await startHand(nat, { bet: 100 });
  assert.equal(nat.state.balance, 1100, 'a natural is capped like any win');
  assert.match(embedOf(lastEdit(c.it)).description, /Winnings capped/);
});

test('blackjack: a double-down win is capped against the doubled stake', async () => {
  const world = mkWorld({ deck: stack(6, 5, 10, 8, 10), settings: { economyMaxPayout: 50 } });
  const { collector, click } = await startHand(world, { bet: 100 });
  await collector.deliver(click('double'));
  assert.deepEqual(world.state.settles, [{ payout: 400, stake: 200 }]);
  assert.equal(world.state.balance, 800 + 250);
});

test('blackjack: a failed payout is shown and logged, never thrown, and the lock is released', async () => {
  const world = mkWorld({ deck: stack(10, 9, 10, 8) });
  const { it, collector, click } = await startHand(world, { bet: 100 });
  world.state.failSettle = true;
  await assert.doesNotReject(() => collector.deliver(click('stand')));
  assert.match(fieldOf(embedOf(lastEdit(it)), 'Balance'), /Payout failed/);
  assert.equal(world.state.locked.size, 0);
  assert.equal(world.state.settles.length, 1, 'no second attempt from the game');
});

test('blackjack: if the hand cannot be shown after the reserve, the bet is refunded exactly once and the lock released (R077)', async () => {
  const world = mkWorld({ deck: stack(10, 9, 10, 8) });
  const it = mkSlash({ timeline: world.timeline, bet: 100, failFirstEdit: true });
  await bj.execute(it, null, world.deps);
  assert.deepEqual(world.state.settles, [{ payout: 100, stake: 100 }]);
  assert.equal(world.state.balance, 1000);
  assert.equal(world.state.locked.size, 0);
  assert.equal(it.collectors.length, 0);
  assert.match(embedOf(it.calls.at(-1)[1]).description, /refunded/);

  const w2 = mkWorld({ deck: stack(10, 9, 10, 8) });
  const it2 = mkSlash({ timeline: w2.timeline, bet: 100 });
  it2.message.collectorFails = true;
  await bj.execute(it2, null, w2.deps);
  assert.deepEqual(w2.state.settles, [{ payout: 100, stake: 100 }]);
  assert.equal(w2.state.balance, 1000);
  assert.equal(w2.state.locked.size, 0);
});

test('blackjack: a failing natural result message still leaves the hand settled and the lock released', async () => {
  const world = mkWorld({ deck: stack('A', 'K', 5, 6) });
  const it = mkSlash({ timeline: world.timeline, bet: 100, editFails: true });
  await assert.doesNotReject(() => bj.execute(it, null, world.deps));
  assert.deepEqual(world.state.settles, [{ payout: 250, stake: 100 }]);
  assert.equal(world.state.locked.size, 0);
});

test('blackjack: a second /blackjack while a hand is live is refused, and allowed again after the hand', async () => {
  const world = mkWorld({ deck: stack(10, 9, 10, 8) });
  const { collector, click } = await startHand(world, { bet: 100 });
  const again = mkSlash({ timeline: world.timeline, bet: 100 });
  await bj.execute(again, null, world.deps);
  assert.match(embedOf(lastPrivate(again)).title, /Game In Progress/);
  assert.deepEqual(world.state.reserves, [100]);
  await collector.deliver(click('stand'));
  const third = mkSlash({ timeline: world.timeline, bet: 100 });
  await bj.execute(third, null, { ...world.deps, newDeck: () => stack('A', 'K', 5, 6) });
  assert.deepEqual(world.state.reserves, [100, 100]);
});

// ═══════════════════════════ the real service (needs a migrated database) ═══════════════════════════

const realWorld = (settings = {}, { deck = null, spin = null } = {}) => {
  const row = { economyEnabled: true, ...settings };
  const timeline = [];
  return {
    timeline,
    row,
    deps: {
      loadSettings: async () => row,
      tryLockCasino: svc.tryLockCasino,
      unlockCasino: svc.unlockCasino,
      reserveBet: svc.reserveBet,
      settleBet: svc.settleBet,
      newDeck: () => (deck ? [...deck] : bj.buildDeck()),
      spin: spin ?? (() => 0),
      sleep: async () => {},
    },
  };
};
const balanceOf = async (guildId, userId) => (await prisma.economyAccount.findUnique({ where: { guildId_userId: { guildId, userId } } }))?.balance;
const seedAccount = async (guildId, userId, balance) => {
  await settingsSvc.ensureGuildSettings(guildId);
  await prisma.economyAccount.upsert({
    where: { guildId_userId: { guildId, userId } },
    create: { guildId, userId, balance },
    update: { balance },
  });
};
const mk = (guildId, userId, opts = {}) => mkSlash({ guildId, userId, guild: { id: guildId, channels: { cache: new Map() } }, ...opts });

test('DB blackjack: stand win, double win, natural, push and a lost hand move the real balance exactly', { skip: skipDb }, async () => {
  const g = G(2);
  const u = U(2);
  await seedAccount(g, u, 1000);
  const play = async (deck, bet, ...actions) => {
    const w = realWorld({}, { deck });
    const it = mk(g, u, { bet, timeline: w.timeline });
    await bj.execute(it, null, w.deps);
    for (const action of actions) await it.collectors[0].deliver(mkClick(it.id, action, u));
    return it;
  };
  await play(stack(10, 9, 10, 8), 100, 'stand');
  assert.equal(await balanceOf(g, u), 1100);
  await play(stack(6, 5, 10, 8, 10), 100, 'double');
  assert.equal(await balanceOf(g, u), 1100 - 200 + 400);
  await play(stack('A', 'K', 5, 6), 100);
  assert.equal(await balanceOf(g, u), 1300 + 150);
  await play(stack(10, 8, 10, 8), 100, 'stand');
  assert.equal(await balanceOf(g, u), 1450, 'a push returns the stake');
  await play(stack(10, 7, 10, 8), 100, 'stand');
  assert.equal(await balanceOf(g, u), 1350);
  assert.equal(svc.isCasinoLocked(g, u), false);
});

test('DB blackjack: an abandoned hand keeps the stake taken and a double Stand pays once', { skip: skipDb }, async () => {
  const g = G(3);
  const u = U(3);
  await seedAccount(g, u, 1000);
  const w = realWorld({}, { deck: stack(10, 9, 10, 8) });
  const it = mk(g, u, { bet: 100 });
  await bj.execute(it, null, w.deps);
  assert.equal(await balanceOf(g, u), 900, 'the bet is escrowed before the player acts');
  assert.equal(svc.isCasinoLocked(g, u), true);
  it.collectors[0].stop('idle');
  await tick();
  assert.equal(await balanceOf(g, u), 900, 'forfeited');
  assert.equal(svc.isCasinoLocked(g, u), false);

  const w2 = realWorld({}, { deck: stack(10, 9, 10, 8) });
  const it2 = mk(g, u, { bet: 100 });
  await bj.execute(it2, null, w2.deps);
  await Promise.all([it2.collectors[0].deliver(mkClick(it2.id, 'stand', u)), it2.collectors[0].deliverRaw(mkClick(it2.id, 'stand', u))]);
  assert.equal(await balanceOf(g, u), 900 - 100 + 200, 'one payout, not two');
});

test('DB blackjack: max payout leaves a push at exactly the stake and caps a win', { skip: skipDb }, async () => {
  const g = G(4);
  const u = U(4);
  await seedAccount(g, u, 1000);
  const push = realWorld({ economyMaxPayout: 5 }, { deck: stack(10, 8, 10, 8) });
  const a = mk(g, u, { bet: 100 });
  await bj.execute(a, null, push.deps);
  await a.collectors[0].deliver(mkClick(a.id, 'stand', u));
  assert.equal(await balanceOf(g, u), 1000);
  const win = realWorld({ economyMaxPayout: 5 }, { deck: stack(10, 9, 10, 8) });
  const b = mk(g, u, { bet: 100 });
  await bj.execute(b, null, win.deps);
  await b.collectors[0].deliver(mkClick(b.id, 'stand', u));
  assert.equal(await balanceOf(g, u), 1005);
  assert.match(embedOf(lastEdit(b)).description, /Winnings capped at/);
});

test('DB blackjack: a double down needs the second bet to be there', { skip: skipDb }, async () => {
  const g = G(5);
  const u = U(5);
  await seedAccount(g, u, 150);
  const w = realWorld({}, { deck: stack(6, 5, 10, 8, 10) });
  const it = mk(g, u, { bet: 100 });
  await bj.execute(it, null, w.deps);
  const dbl = mkClick(it.id, 'double', u);
  await it.collectors[0].deliver(dbl);
  assert.deepEqual(verbs(dbl), ['deferUpdate', 'followUp']);
  assert.equal(await balanceOf(g, u), 50);
  await it.collectors[0].deliver(mkClick(it.id, 'stand', u));
  assert.equal(await balanceOf(g, u), 50 + 0 + 0, 'player 11 loses to the dealer 18');
});

test('DB roulette: straight-up and green win 36x, a loss costs the bet, the balance never goes negative', { skip: skipDb }, async () => {
  const g = G(6);
  const u = U(6);
  await seedAccount(g, u, 1000);
  const spin = async (space, n, bet) => {
    const w = realWorld({}, { spin: () => n });
    const it = mk(g, u, { bet, space });
    await roulette.execute(it, null, w.deps);
    return it;
  };
  await spin('17', 17, 10);
  assert.equal(await balanceOf(g, u), 1350);
  await spin('green', 0, 10);
  assert.equal(await balanceOf(g, u), 1700);
  await spin('red', 0, 100);
  assert.equal(await balanceOf(g, u), 1600);
  const all = await spin('red', 2, 1600);
  assert.equal(await balanceOf(g, u), 0);
  assert.equal(fieldOf(embedOf(lastEdit(all)), 'Balance'), '1,600 → 0 coins');
  const broke = await spin('red', 1, 1);
  assert.equal(await balanceOf(g, u), 0, 'an empty wallet cannot bet');
  assert.match(embedOf(broke.calls.at(-1)[1]).title, /Insufficient Funds/);
});

test('DB roulette: two all-in spins started together: the lock refuses the second, the balance is exact', { skip: skipDb }, async () => {
  const g = G(7);
  const u = U(7);
  await seedAccount(g, u, 1000);
  const w = realWorld({}, { spin: () => 1 });
  const a = mk(g, u, { bet: 1000, space: 'red' });
  const b = mk(g, u, { bet: 1000, space: 'red' });
  await Promise.all([roulette.execute(a, null, w.deps), roulette.execute(b, null, w.deps)]);
  assert.equal(await balanceOf(g, u), 2000, 'exactly one of the two all-in bets was played and won');
  assert.ok([a, b].some((it) => /Game In Progress/.test(embedOf(lastPrivate(it))?.title ?? '')));
});

test('DB slots: win, max payout and the server maximum all land exactly', { skip: skipDb }, async () => {
  const g = G(8);
  const u = U(8);
  await seedAccount(g, u, 1000);
  const run = async (settings, grid, bet) => {
    const w = realWorld(settings, { spin: () => grid });
    const it = mk(g, u, { bet });
    await slots.execute(it, null, w.deps);
    return it;
  };
  await run({}, grid3('🍎'), 100);
  assert.equal(await balanceOf(g, u), 1050);
  await run({ economyMaxPayout: 20 }, grid3('💎'), 100);
  assert.equal(await balanceOf(g, u), 1050 - 100 + 120, 'stake + max payout');
  const capped = await run({ economyMaxBalance: 1100 }, grid3('💎'), 100);
  assert.equal(await balanceOf(g, u), 1100, 'clamped to the server maximum');
  assert.match(fieldOf(embedOf(lastEdit(capped)), 'Balance'), /\(server maximum\)$/);
  await seedAccount(g, u, 5);
  const lost = await run({}, ['🍎', '🍋', '🍒', '🍇', '💎', '7️⃣', '🍎', '🍋', '🍒'].map((e) => slots.SYMBOLS.find((s) => s.emoji === e)), 5);
  assert.equal(await balanceOf(g, u), 0);
  assert.equal(fieldOf(embedOf(lastEdit(lost)), 'Balance'), '5 → 0 coins');
});

test('DB: three games started at once with the same coins: one plays, the others are refused, no negative balance', { skip: skipDb }, async () => {
  const g = G(9);
  const u = U(9);
  await seedAccount(g, u, 500);
  const w = realWorld({}, { spin: () => 1, deck: stack(10, 9, 10, 8) });
  const a = mk(g, u, { bet: 500, space: 'black' });
  const b = mk(g, u, { bet: 500 });
  const c = mk(g, u, { bet: 500 });
  await Promise.all([roulette.execute(a, null, w.deps), slots.execute(b, null, { ...w.deps, spin: () => slots.spinGrid() }), bj.execute(c, null, w.deps)]);
  assert.equal(await balanceOf(g, u), 0, 'the roulette (first in line) lost its 500; nothing else was reserved');
  for (const it of [b, c]) assert.match(embedOf(lastPrivate(it))?.title ?? '', /Game In Progress/);
  assert.equal(svc.isCasinoLocked(g, u), false);
});
