// The JS side of feat-games' tests. Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-games-js.mjs pure [--write]        pure helpers and rendered output on fixed inputs (no
//                                                                   servers); --write also writes
//                                                                   rust/fixtures/golden/feat-games/pure.json
//   node rust/fixtures/gen/feat-games-js.mjs seed <g> <u> <bal>     JS writes: the guild settings (games and economy on)
//                                                                   and the member's balance, through the JS services
//   node rust/fixtures/gen/feat-games-js.mjs balance <g> <u>        JS reads the member's account
//   node rust/fixtures/gen/feat-games-js.mjs slots <g> <u> <bet> <emoji x9>
//                                                                   JS plays /slots through its own execute() with the
//                                                                   real economy and the given reels
//   node rust/fixtures/gen/feat-games-js.mjs blackjack <g> <u> <bet> <action> <ranks...>
//                                                                   JS plays /blackjack (deck in deal order), then clicks
// The database commands use TEST_DATABASE_URL / TEST_REDIS_URL.
import { mkdirSync, writeFileSync } from 'node:fs';
import { EventEmitter } from 'node:events';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.LOG_LEVEL = 'error';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const svc = await imp('src', 'services', 'scrabbleService.js');
const gamesCmd = await imp('src', 'commands', 'games', 'games.js');
const scrabbleCmd = await imp('src', 'commands', 'games', 'scrabble.js');
const bj = await imp('src', 'commands', 'games', 'blackjack.js');
const roulette = await imp('src', 'commands', 'games', 'roulette.js');
const slots = await imp('src', 'commands', 'games', 'slots.js');
const { buildCustomId } = await imp('src', 'config', 'customIds.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

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

const json = (b) => JSON.parse(JSON.stringify(b?.toJSON ? b.toJSON() : b));

// ── scrabble ──────────────────────────────────────────────────────────────────

const A = '111111111111111111';
const B = '222222222222222222';

function stateOf(g) {
  return JSON.parse(JSON.stringify({
    id: g.id, guildId: g.guildId, channelId: g.channelId, board: g.board, bag: g.bag, players: g.players,
    turnOrder: g.turnOrder, currentTurn: g.currentTurn, turnSeq: g.turnSeq, scorelessTurns: g.scorelessTurns,
    placedCount: g.placedCount, moves: g.moves, ended: g.ended, result: g.result,
  }));
}

/** Scripted games: every op's outcome and the game state after it. */
const SCRIPTS = [
  { seed: 1, dictionary: null, ops: [
    ['setRack', A, 'CATXYZE'], ['play', A, 'CAT', 0, 0, 'h'], ['play', A, 'A', 7, 7, 'h'], ['play', A, 'CATS', 7, 12, 'h'],
    ['play', A, 'cat', 7, 7, 'h'], ['play', A, 'XE', 8, 7, 'h'],
    ['setRack', B, 'OXQJKLM'], ['compute', B, 'S', 7, 10, 'h'], ['compute', B, 'XU', 0, 0, 'h'], ['play', B, 'OX', 8, 7, 'h'],
    ['play', A, 'CAT', 7, 7, 'h'], ['compute', A, 'T', 7, 9, 'v'], ['pass', B], ['pass', A], ['exchange', B, 'QQ'], ['exchange', B, 'q'],
    ['exchange', A, 'ab?'], ['setRack', A, 'ZEBRA?S'], ['compute', A, 'Z?EBRAS', 2, 7, 'v'], ['play', A, 'ZEBRAS', 1, 9, 'v'],
    ['compute', B, 'X', 99, 0, 'h'], ['compute', B, 'X', 0, -1, 'h'], ['compute', B, 'X', 0, 0, 'd'],
  ] },
  { seed: 2, dictionary: ['CAT', 'CATS'], ops: [
    ['setRack', A, 'CATXYZE'], ['play', A, 'CAT', 7, 7, 'h'], ['setRack', B, 'DOGSQJK'], ['compute', B, 'DOG', 7, 10, 'h'],
    ['play', B, 'S', 7, 10, 'h'], ['setRack', A, 'ABC?EFG'], ['compute', A, 'QA', 6, 7, 'v'],
  ] },
  { seed: 3, dictionary: null, ops: [
    ['setRack', A, 'ABC?EFG'], ['compute', A, 'QA', 7, 7, 'h'], ['setRack', A, 'ABCDEFG'], ['compute', A, 'QA', 7, 7, 'h'],
    ['compute', A, 'A?B', 7, 7, 'h'], ['play', A, 'ABCDEFG', 7, 7, 'h'], ['setRack', B, 'C?ATXYZ'], ['play', B, 'C?AT', 8, 3, 'v'],
  ] },
  { seed: 4, dictionary: null, ops: [
    ['setBag', 0], ['setRack', A, 'CAT'], ['setRack', B, 'QZ'], ['play', A, 'CAT', 7, 7, 'h'], ['pass', B],
  ] },
  { seed: 5, dictionary: null, ops: [
    ['pass', A], ['pass', B], ['exchange', A, 'X'], ['pass', B], ['pass', A], ['pass', B], ['pass', A],
  ] },
  { seed: 6, dictionary: null, ops: [
    ['setBag', 6], ['exchange', A, 'AB'], ['resign', B], ['resign', A], ['pass', A],
  ] },
  { seed: 7, dictionary: null, ops: [
    ['setRack', A, 'C?ATXYZ'], ['play', A, 'C?AT', 7, 7, 'h'], ['setRack', B, 'SCATTER'], ['play', B, 'SCATTER', 6, 6, 'v'],
    ['finish', 'timeout', B, null],
  ] },
];

function runOp(g, op) {
  const [kind, ...a] = op;
  const out = (() => {
    switch (kind) {
      case 'setRack':
        g.players[a[0]].rack = [...a[1]].map((letter) => ({ letter, points: svc.TILE_POINTS[letter] }));
        return null;
      case 'setBag':
        g.bag.length = a[0];
        return null;
      case 'play': return svc.playWord(g, a[0], a[1], a[2], a[3], a[4]);
      case 'compute': {
        const parsed = svc.parseWordInput(a[1]);
        return parsed.ok ? svc.computePlay(g, a[0], parsed.tiles, a[2], a[3], a[4]) : parsed;
      }
      case 'pass': return svc.passTurn(g, a[0]);
      case 'exchange': return svc.exchangeTiles(g, a[0], a[1]);
      case 'resign': return svc.resignGame(g, a[0]);
      case 'finish': return svc.finishGame(g, a[0], { loserId: a[1], moverId: a[2] });
      default: throw new Error(`unknown op ${kind}`);
    }
  })();
  svc.clearTurnTimer(g);
  return JSON.parse(JSON.stringify(out));
}

function scrabble() {
  const games = SCRIPTS.map(({ seed, dictionary, ops }) => {
    const g = svc.createGame({
      id: `game-${seed}`, guildId: '900000000000000001', channelId: `ch-${seed}`,
      players: [{ id: A, name: 'Alice' }, { id: B, name: 'Bob' }],
      dictionary: dictionary ? new Set(dictionary) : null,
      rng: mulberry32(seed),
    });
    const initial = stateOf(g);
    const steps = ops.map((op) => ({ op, out: runOp(g, op), state: stateOf(g) }));
    const out = {
      seed, dictionary, initial, steps,
      board: svc.renderBoard(g.board),
      embed: json(svc.buildGameEmbed(g)),
      finishedEmbed: json(svc.buildGameEmbed(g, { finished: true })),
      resultEmbed: g.result ? json(svc.buildResultEmbed(g, g.result)) : null,
      racks: Object.fromEntries(Object.values(g.players).map((p) => [p.id, svc.formatRack(p.rack)])),
    };
    svc.clearTurnTimer(g);
    return out;
  });
  const bags = [1, 7, 42].map((seed) => ({ seed, letters: svc.createTileBag(mulberry32(seed)).map((t) => t.letter).join('') }));
  const words = ['qu?iz', '?', 'C4T', '??A', 'A?', 'CA T', 'ABCDEFGHIJKLMNOP', 'ABCDEFGHIJKLMNO', '', '  cat ', 'é', '?a?b', 'Ab?c'];
  const tiles = ['ab?', '', 'ABCDEFGH', 'A B', 'A1', ' q ', '???????', 'abcdefg'];
  const dict = 'cat\r\n# c\nQI\nx\nfoo-bar\n  dog  \nABCDEFGHIJKLMNOP\nABCDEFGHIJKLMNO\n\n';
  return {
    games,
    bags,
    parseWord: words.map((w) => ({ input: w, out: svc.parseWordInput(w) })),
    parseTiles: tiles.map((t) => ({ input: t, out: svc.parseTilesInput(t) })),
    dictionary: [...svc.parseDictionary(dict)].sort(),
    describe: [svc.describeWords([{ text: 'OX', score: 17 }, { text: 'CO', score: 4 }]), svc.describeWords([])],
    formatRack: [svc.formatRack([]), svc.formatRack([{ letter: '?', points: 0 }, { letter: 'Q', points: 10 }])],
    rackRow: json(svc.rackRow('1234567890123456789')),
    emptyBoard: svc.renderBoard(svc.createBoard()),
    help: [{ state: 'missing', size: 0 }, { state: 'loaded', size: 172823 }, { state: 'loaded', size: 2 }].map((status) => ({
      status, embed: json(scrabbleCmd.buildHelpEmbed(null, status)), line: scrabbleCmd.dictionaryLine(status),
    })),
  };
}

// ── /games ────────────────────────────────────────────────────────────────────

function games() {
  const real = gamesCmd.collectGameCommands({
    commands: new Map([['blackjack', bj], ['games', gamesCmd], ['roulette', roulette], ['scrabble', scrabbleCmd], ['slots', slots]]),
    commandCategories: new Map(['blackjack', 'games', 'roulette', 'scrabble', 'slots'].map((n) => [n, 'games'])),
  });
  const entry = (name, module = 'games', description = `Description of ${name}`) => ({ name, module, description });
  const synthetic = Array.from({ length: 40 }, (_, n) => entry(`synthetic-game-${n}`, 'games', 'x'.repeat(150)));
  const lists = {
    real: real,
    realWithSynthetic: [...real, ...synthetic],
    empty: [],
    other: [entry('trivia', 'achievements'), entry('zzz', 'nosuchmodule'), entry('games rps')],
    scrabbleOnly: [entry('scrabble help'), entry('scrabble start'), entry('scrabble odd'), entry('scrabble word')],
    long: Array.from({ length: 30 }, (_, n) => entry(`game-${n}`, n % 2 ? 'games' : 'economy', 'é'.repeat(120))),
  };
  return {
    collected: real,
    embeds: Object.fromEntries(Object.entries(lists).map(([k, v]) => [k, json(gamesCmd.buildGamesEmbed(v, null))])),
    colored: json(gamesCmd.buildGamesEmbed(real, { embedColor: 0x123456 })),
    rps: { row: json(gamesCmd.buildRpsRow('123456789012345678')), disabled: json(gamesCmd.buildRpsRow('123456789012345678', true)) },
    decide: Object.fromEntries(gamesCmd.RPS_CHOICES.flatMap((p) => gamesCmd.RPS_CHOICES.map((b) => [`${p},${b}`, gamesCmd.decideRps(p, b)]))),
    guesses: [0, 1, 42, 41, 43, 100, 101, NaN, 1.5, -3].map((g) => [String(g), gamesCmd.evaluateGuess(42, g)]),
    board: {
      empty: gamesCmd.buildBoard(Array(9).fill(''), '123456789012345678').map(json),
      some: gamesCmd.buildBoard(['X', '', 'O', '', 'X', '', '', 'O', ''], '123456789012345678').map(json),
      disabled: gamesCmd.buildBoard(['X', '', '', '', '', '', '', '', ''], 'id', true).map(json),
    },
    winners: [
      ['X', 'O', 'X', 'X', 'O', 'O', 'O', 'X', 'X'], ['X', 'O', '', '', '', '', '', '', ''], ['O', 'O', 'O', '', 'X', 'X', '', '', ''],
      ['X', '', '', '', 'X', '', '', '', 'X'], ['', '', 'O', '', 'O', '', 'O', '', ''],
    ].map((b) => ({ board: b, winner: gamesCmd.checkWinner(b) })),
    customIds: {
      tttCh: [buildCustomId('ttt-ch', '123456789012345678', 'accept'), buildCustomId('ttt-ch', '123456789012345678', 'decline')],
      scrabbleCh: [buildCustomId('scrabble-ch', '123456789012345678', 'accept'), buildCustomId('scrabble-ch', '123456789012345678', 'decline')],
      bj: buildCustomId('bj', '1'.repeat(20), 'double'),
    },
  };
}

// ── casino ────────────────────────────────────────────────────────────────────

function casino() {
  const card = (rank) => ({ rank: String(rank), suit: '♠️', value: rank === 'A' ? 11 : ['J', 'Q', 'K'].includes(String(rank)) ? 10 : Number(rank) });
  const hands = [['A', 'K'], ['A', 'A', 9], ['K', 'Q', 5], ['A', 'A', 'A'], ['A', 9, 'A'], ['A', 5, 'K'], [10, 'A', 'A', 'A'],
    ['A', 'A', 'A', 'A', 2, 2, 2, 2, 3, 3, 3], [10, 5, 6], ['A', 6], ['A', 6, 10], [10, 7], [10, 6], ['A', 7], ['A', 'A', 5], [10, 10], [10, 6, 5]]
    .map((h) => h.map(card));
  const rng = mulberry32(7);
  const deck = bj.buildDeck((n) => Math.floor(rng() * n));
  const pairs = [];
  for (const p of hands) for (const d of hands) pairs.push([p.map((c) => c.rank), d.map((c) => c.rank)]);
  const spaces = ['red', 'BLACK', 'Green', '0', '36', '1-18', '19 - 36', '1-12', '13-24', '25-36', '  Even ', 'odd', '17', '13–24', '13—24',
    '1st 12', '12 red', '3.9', '00', '07', '37', '-1', '', '1e1', '0x10', '+5', 'red black', '1-13', '99', 'greenish', 'low', 'HIGH', 'r e d', '9', '10', '30', '31'];
  const tokens = ['red', 'black', 'green', 'even', 'odd', 'low', 'high', '1-12', '13-24', '25-36', ...Array.from({ length: 37 }, (_, n) => String(n))];
  const queries = ['', '1', 'G', '17', '0', 'zzz', ' 1 3 -', 'R', 'b', '2', '3', 'e', 'Ö', '25'];
  const reels = mulberry32(99);
  const grids = Array.from({ length: 60 }, () => slots.spinGrid((n) => Math.floor(reels() * n)));
  const sym = (e) => slots.SYMBOLS.find((s) => s.emoji === e);
  grids.push(['🍎', '🍎', '🍎', '🍋', '🍒', '🍋', '🍇', '🍋', '🍒'].map(sym), Array(9).fill(slots.SYMBOLS[0]),
    ['💎', '🍋', '💎', '🍋', '💎', '🍋', '💎', '🍋', '💎'].map(sym));
  return {
    hands: hands.map((h) => ({ hand: h.map((c) => c.rank), value: bj.handValue(h), soft: bj.isSoft(h), natural: bj.isNatural(h), dealerHits: bj.shouldDealerHit(h) })),
    outcomes: pairs.map(([p, d]) => ({
      player: p, dealer: d,
      natural: [1, 5, 100].map((bet) => bj.naturalOutcome(p.map(card), d.map(card), bet)),
      final: [1, 100, 200].map((stake) => bj.finalOutcome(p.map(card), d.map(card), stake)),
    })),
    deck: deck.map((c) => `${c.rank}${c.suit}`),
    buttons: [[true, false], [false, false], [true, true]].map(([canDouble, disabled]) => json(bj.buildButtons('123456789012345678', { canDouble, disabled }))),
    spaces: spaces.map((s) => ({ input: s, space: roulette.parseSpace(s) })),
    returns: Object.fromEntries(tokens.map((t) => [t, Array.from({ length: 37 }, (_, n) => roulette.spaceReturn(roulette.parseSpace(t), n, 3))])),
    colors: Array.from({ length: 37 }, (_, n) => roulette.wheelColor(n)),
    autocomplete: await_(queries),
    grids: grids.map((g) => {
      const res = slots.slotsPayout(g, 7);
      return { grid: g.map((s) => s.emoji), payout1: slots.slotsPayout(g, 1).payout, payout7: res.payout, multiplier: res.multiplier, render: slots.renderGrid(g, res.wins) };
    }),
  };
}

function await_(queries) {
  // roulette.autocomplete is async but answers synchronously through respond().
  return queries.map((q) => {
    let answered = null;
    roulette.autocomplete({ options: { getFocused: () => q }, responded: false, respond: async (list) => { answered = list; } });
    return { query: q, choices: answered };
  });
}

// ── database ──────────────────────────────────────────────────────────────────

async function seed(g, u, balance) {
  const settingsSvc = await imp('src', 'services', 'settingsService.js');
  const econ = await imp('src', 'services', 'economyService.js');
  await settingsSvc.ensureGuildSettings(g);
  await settingsSvc.updateSettings(g, { economyEnabled: true, gamesEnabled: true });
  await econ.setBalance(g, u, Number(balance), await settingsSvc.getSettings(g));
  return { balance: (await econ.getAccount(g, u)).balance };
}

async function balance(g, u) {
  const row = await prisma.economyAccount.findUnique({ where: { guildId_userId: { guildId: g, userId: u } } });
  return { balance: row?.balance ?? null };
}

/** A slash interaction double: editReply returns a message whose collector is a plain EventEmitter. */
function fakeInteraction(g, u, bet) {
  const collectors = [];
  const calls = [];
  const message = {
    createMessageComponentCollector() {
      const c = new EventEmitter();
      c.stop = (reason = 'user') => { if (!c.ended) { c.ended = true; c.emit('end', new Map(), reason); } };
      collectors.push(c);
      return c;
    },
  };
  const it = {
    id: '955026000000009999', guildId: g, channelId: '955026000000000500', channel: { id: '955026000000000500' },
    guild: { id: g, channels: { cache: new Map() } }, createdTimestamp: Date.now(), user: { id: u },
    deferred: false, replied: false, ephemeral: null, calls, collectors,
    options: { getInteger: () => bet, getString: () => null },
    reply: async (o) => { it.replied = true; calls.push(['reply', o]); },
    deferReply: async () => { it.deferred = true; it.ephemeral = false; calls.push(['deferReply']); },
    editReply: async (o) => { calls.push(['editReply', o]); return message; },
    deleteReply: async () => { calls.push(['deleteReply']); },
    followUp: async (o) => { calls.push(['followUp', o]); },
  };
  return it;
}

async function realDeps(extra) {
  const econ = await imp('src', 'services', 'economyService.js');
  const settingsSvc = await imp('src', 'services', 'settingsService.js');
  return {
    loadSettings: (i) => settingsSvc.getSettings(i.guildId),
    tryLockCasino: econ.tryLockCasino, unlockCasino: econ.unlockCasino, reserveBet: econ.reserveBet, settleBet: econ.settleBet,
    newDeck: () => bj.buildDeck(), spin: () => 0, sleep: async () => {},
    ...extra,
  };
}

async function playSlots(g, u, bet, ...emojis) {
  const grid = emojis.map((e) => slots.SYMBOLS.find((s) => s.emoji === e));
  const it = fakeInteraction(g, u, Number(bet));
  await slots.execute(it, null, await realDeps({ spin: () => grid }));
  return { ...(await balance(g, u)), last: json(it.calls.at(-1)?.[1]?.embeds?.[0]) };
}

async function playBlackjack(g, u, bet, action, ...ranks) {
  const deck = ranks.map((r) => ({ rank: r, suit: '♠️', value: r === 'A' ? 11 : ['J', 'Q', 'K'].includes(r) ? 10 : Number(r) })).reverse();
  const it = fakeInteraction(g, u, Number(bet));
  await bj.execute(it, null, await realDeps({ newDeck: () => [...deck] }));
  if (action !== 'none' && it.collectors[0]) {
    const click = {
      customId: buildCustomId('bj', it.id, action), user: { id: u }, deferred: false, replied: false,
      deferUpdate: async () => { click.deferred = true; }, reply: async () => {}, followUp: async () => {}, editReply: async () => {},
    };
    await Promise.all(it.collectors[0].listeners('collect').map((fn) => fn(click)));
  }
  return { ...(await balance(g, u)), last: json(it.calls.filter((c) => c[0] === 'editReply').at(-1)?.[1]?.embeds?.[0]) };
}

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'pure') {
    const out = { scrabble: scrabble(), games: games(), casino: casino() };
    if (args.includes('--write')) {
      const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-games');
      mkdirSync(dir, { recursive: true });
      writeFileSync(join(dir, 'pure.json'), `${JSON.stringify(out)}\n`);
    }
    answer({ ok: true });
  } else if (cmd === 'seed') {
    answer(await seed(...args));
  } else if (cmd === 'balance') {
    answer(await balance(...args));
  } else if (cmd === 'slots') {
    answer(await playSlots(...args));
  } else if (cmd === 'blackjack') {
    answer(await playBlackjack(...args));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
