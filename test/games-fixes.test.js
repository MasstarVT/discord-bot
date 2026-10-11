// Offline tests for pkg-games-fixes: no Discord, Postgres or Redis. Run with `node --test test/games-fixes.test.js`.
import { test, describe, beforeEach, after, mock } from 'node:test';
import assert from 'node:assert/strict';
import { readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV = 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL ??= 'postgresql://test:test@127.0.0.1:1/test';
// Nothing listens here: cache.* fails fast and returns its fallbacks.
process.env.REDIS_URL = 'redis://127.0.0.1:1';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const svc = await import('../src/services/scrabbleService.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { parseCustomId } = await import('../src/config/customIds.js');
const gamesCmd = await import('../src/commands/games/games.js');
const numguessCmd = await import('../src/commands/games/numguess.js');
const rpsCmd = await import('../src/commands/games/rps.js');
const tttCmd = await import('../src/commands/games/tictactoe.js');
const scrabbleCmd = await import('../src/commands/games/scrabble.js');
const scrabblePlayCmd = await import('../src/commands/games/scrabble-play.js');
const scrabbleHelpCmd = await import('../src/commands/games/scrabble-help.js');
const { plugin } = await import('../src/plugins/games-fixes.js');

const {
  TILE_COUNTS, TURN_TIMEOUT_MS, activeGames, pendingChallenges,
  createGame, createTileBag, parseWordInput, parseTilesInput, parseDictionary, playWord, passTurn,
  exchangeTiles, computePlay, finishGame, registerGame, getGame, handleTurnTimeout, clearTurnTimer,
  reserveChannel, releaseChannel, isChannelBusy, buildGameEmbed, renderBoard, rackRow, rackValue,
  handleRackButton, currentPlayerId, otherPlayerId, __testing,
} = svc;

after(async () => {
  mock.timers.reset();
  Object.assign(console, quiet);
  (await import('../src/services/redis.js')).default.disconnect();
});

// ── Helpers ───────────────────────────────────────────────────────────────────

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

const A = '111111111111111111';
const B = '222222222222222222';
let seq = 0;
const nextChannel = () => `ch-${++seq}`;
const games = [];

/** A fresh two-player game in a unique channel (never registered unless a test does it). */
function mkGame({ channelId = nextChannel(), dictionary = null, id } = {}) {
  const g = createGame({
    id: id ?? `g-${channelId}`,
    guildId: 'guild-1',
    channelId,
    players: [{ id: A, name: 'Alice' }, { id: B, name: 'Bob' }],
    dictionary,
    rng: mulberry32(1),
  });
  games.push(g);
  return g;
}

/** Sets a rack by hand: 'CAT?' gives tiles C, A, T and a blank. */
function setRack(game, userId, letters) {
  game.players[userId].rack = [...letters].map((letter) => ({ letter, points: svc.TILE_POINTS[letter] }));
}

const summary = (words) => words.map((w) => `${w.text} ${w.score}`);

beforeEach(() => {
  activeGames.clear();
  pendingChallenges.clear();
  __testing.reset();
});

// Every game a test created is ended, so no real turn timer is left behind.
function cleanup() {
  for (const g of games.splice(0)) {
    clearTurnTimer(g);
    if (!g.ended) finishGame(g, 'aborted');
  }
  activeGames.clear();
  pendingChallenges.clear();
}
after(cleanup);
beforeEach(cleanup);

// ── scrabbleService ───────────────────────────────────────────────────────────

describe('tile bag and parsing', () => {
  test('1. the bag has 100 tiles including exactly 2 blanks (R280)', () => {
    const bag = createTileBag(mulberry32(7));
    assert.equal(bag.length, 100);
    const blanks = bag.filter((t) => t.letter === '?');
    assert.equal(blanks.length, 2);
    assert.ok(blanks.every((t) => t.points === 0));
    const counts = {};
    for (const t of bag) counts[t.letter] = (counts[t.letter] ?? 0) + 1;
    assert.deepEqual(counts, { ...TILE_COUNTS });
  });

  test('2. parseWordInput handles blanks and rejects bad input', () => {
    const ok = parseWordInput('qu?iz');
    assert.equal(ok.ok, true);
    assert.deepEqual(ok.tiles, [
      { letter: 'Q', blank: false },
      { letter: 'U', blank: false },
      { letter: 'I', blank: true },
      { letter: 'Z', blank: false },
    ]);
    for (const bad of ['?', 'C4T', '??A', 'A?', 'CA T', 'ABCDEFGHIJKLMNOP', '']) {
      const r = parseWordInput(bad);
      assert.equal(r.ok, false, `"${bad}" should be rejected`);
      assert.ok(r.error.length > 0);
    }
    assert.match(parseWordInput('ABCDEFGHIJKLMNOP').error, /at most 15/);
    assert.equal(parseWordInput('ABCDEFGHIJKLMNO').ok, true);
  });

  test('parseTilesInput accepts A-Z and ? up to 7 tiles', () => {
    assert.deepEqual(parseTilesInput('ab?').letters, ['A', 'B', '?']);
    for (const bad of ['', 'ABCDEFGH', 'A B', 'A1']) assert.equal(parseTilesInput(bad).ok, false);
  });

  test('7. parseDictionary keeps 2-15 letter A-Z words and handles CRLF', () => {
    const set = parseDictionary('cat\r\n# c\nQI\nx\nfoo-bar\n');
    assert.deepEqual([...set].sort(), ['CAT', 'QI']);
  });
});

describe('scoring (empty board, hand-set racks)', () => {
  test('3a. CAT at the centre scores 10', () => {
    const g = mkGame();
    setRack(g, A, 'CATXYZE');
    const r = playWord(g, A, 'CAT', 7, 7, 'h');
    assert.equal(r.ok, true);
    assert.deepEqual(summary(r.words), ['CAT 10']);
    assert.equal(r.total, 10);
    assert.equal(g.players[A].score, 10);
    assert.equal(g.placedCount, 3);
  });

  test('3b. first move errors: centre, one letter, off the board', () => {
    const g = mkGame();
    setRack(g, A, 'CATSXYZ');
    const centre = playWord(g, A, 'CAT', 0, 0, 'h');
    assert.equal(centre.ok, false);
    assert.match(centre.error, /centre square/);
    const one = playWord(g, A, 'A', 7, 7, 'h');
    assert.equal(one.ok, false);
    assert.match(one.error, /form a word of at least 2 letters/);
    const off = playWord(g, A, 'CATS', 7, 12, 'h');
    assert.equal(off.ok, false);
    assert.match(off.error, /runs off the board/);
    // nothing changed
    assert.equal(g.placedCount, 0);
    assert.equal(g.players[A].rack.length, 7);
  });

  test('3c. a blank scores 0, is stored as a blank and consumes the ? tile', () => {
    const g = mkGame();
    setRack(g, A, 'C?ATXYZ');
    const r = playWord(g, A, 'C?AT', 7, 7, 'h');
    assert.equal(r.ok, true);
    assert.equal(r.total, 8);
    assert.deepEqual(g.board[7][8], { letter: 'A', points: 0, blank: true });
    assert.equal(g.players[A].rack.filter((t) => t.letter === '?').length, 0);
  });

  test('3d. using all seven tiles earns the 50-point bingo (84)', () => {
    const g = mkGame();
    setRack(g, A, 'ABCDEFG');
    const r = playWord(g, A, 'ABCDEFG', 7, 7, 'h');
    assert.equal(r.ok, true);
    assert.equal(r.bingo, true);
    assert.equal(r.total, 84);
  });
});

describe('cross-words and extension (R047, R168)', () => {
  function afterCat() {
    const g = mkGame();
    setRack(g, A, 'CATXYZE');
    assert.equal(playWord(g, A, 'CAT', 7, 7, 'h').ok, true);
    return g;
  }

  test('4. OX under CAT scores OX + CO + AX = 38', () => {
    const g = afterCat();
    setRack(g, B, 'OXQJKLM');
    const r = playWord(g, B, 'OX', 8, 7, 'h');
    assert.equal(r.ok, true);
    assert.deepEqual(summary(r.words), ['OX 17', 'CO 4', 'AX 17']);
    assert.equal(r.total, 38);
  });

  test('5. the declared word is extended to the whole run on the board', () => {
    const play = (letters, word, r, c, dir) => {
      const g = afterCat();
      setRack(g, B, letters);
      return computePlay(g, B, parseWordInput(word).tiles, r, c, dir);
    };
    assert.deepEqual(summary(play('SQJKLMN', 'S', 7, 10, 'h').words), ['CATS 6']);
    assert.deepEqual(summary(play('SQJKLMN', 'S', 7, 10, 'v').words), ['CATS 6']);
    assert.deepEqual(summary(play('DOGQJKL', 'DOG', 7, 10, 'h').words), ['CATDOG 11']);
    assert.deepEqual(summary(play('EQJKLMN', 'ATE', 7, 8, 'h').words), ['CATE 6']);
    const far = play('XUQJKLM', 'XU', 0, 0, 'h');
    assert.equal(far.ok, false);
    assert.match(far.error, /touch or pass through/);
  });

  test('6. a dictionary rejects invalid words, including cross-words and extensions', () => {
    const dictionary = new Set(['CAT', 'CATS']);
    const g = mkGame({ dictionary });
    setRack(g, A, 'CATXYZE');
    assert.equal(playWord(g, A, 'CAT', 7, 7, 'h').ok, true);
    setRack(g, B, 'DOGSQJK');
    const bad = computePlay(g, B, parseWordInput('DOG').tiles, 7, 10, 'h');
    assert.equal(bad.ok, false);
    assert.match(bad.error, /\*\*CATDOG\*\*/);
    const good = playWord(g, B, 'S', 7, 10, 'h');
    assert.equal(good.ok, true);
    assert.deepEqual(summary(good.words), ['CATS 6']);
  });
});

describe('turns, racks, exchange, endings', () => {
  test('8. two synchronous plays by the same user: the second is refused and changes nothing (R083)', () => {
    const g = mkGame();
    setRack(g, A, 'CATXYZE');
    assert.equal(playWord(g, A, 'CAT', 7, 7, 'h').ok, true);
    const rackBefore = JSON.stringify(g.players[A].rack);
    const boardBefore = JSON.stringify(g.board);
    const again = playWord(g, A, 'XE', 8, 7, 'h');
    assert.equal(again.ok, false);
    assert.match(again.error, /It's not your turn/);
    assert.equal(JSON.stringify(g.players[A].rack), rackBefore);
    assert.equal(JSON.stringify(g.board), boardBefore);
  });

  test('9. rack errors suggest ?Q only when a blank is held', () => {
    const g = mkGame();
    setRack(g, A, 'ABC?EFG');
    const hint = computePlay(g, A, parseWordInput('QA').tiles, 7, 7, 'h');
    assert.equal(hint.ok, false);
    assert.ok(hint.error.includes('`?Q`'));
    setRack(g, A, 'ABCDEFG');
    const plain = computePlay(g, A, parseWordInput('QA').tiles, 7, 7, 'h');
    assert.match(plain.error, /You don't have the tile \*\*Q\*\*/);
    const noBlank = computePlay(g, A, parseWordInput('A?B').tiles, 7, 7, 'h');
    assert.match(noBlank.error, /don't have a blank/);
  });

  test('10. exchange: bag rule, size bookkeeping, scoreless counter, possession', () => {
    const g = mkGame();
    g.bag.length = 6;
    assert.match(exchangeTiles(g, A, 'AB').error, /at least 7 tiles \(it has \*\*6\*\*\)/);

    const g2 = mkGame();
    setRack(g2, A, 'ABCDEFG');
    const bagBefore = g2.bag.length;
    const r = exchangeTiles(g2, A, 'AB');
    assert.equal(r.ok, true);
    assert.equal(r.count, 2);
    assert.equal(g2.players[A].rack.length, 7);
    assert.equal(g2.bag.length, bagBefore);
    assert.equal(g2.currentTurn, 1);
    assert.equal(g2.scorelessTurns, 1);

    const g3 = mkGame();
    setRack(g3, A, 'QABCDEF');
    const dup = exchangeTiles(g3, A, 'QQ');
    assert.equal(dup.ok, false);
    assert.match(dup.error, /don't have the tile \*\*Q\*\*/);
    assert.equal(g3.currentTurn, 0);
  });

  test('11. six scoreless turns end the game and deduct each rack', () => {
    mock.timers.enable({ apis: ['setTimeout'] });
    try {
      const g = mkGame();
      assert.equal(registerGame(g), true);
      const scoresBefore = { [A]: rackValue(g.players[A].rack), [B]: rackValue(g.players[B].rack) };
      let r;
      for (let i = 0; i < 6; i++) r = passTurn(g, currentPlayerId(g));
      assert.equal(r.ok, true);
      assert.equal(r.gameOver.reason, 'scoreless');
      assert.equal(g.players[A].score, -scoresBefore[A]);
      assert.equal(g.players[B].score, -scoresBefore[B]);
      assert.equal(r.gameOver.adjustments[A], -scoresBefore[A]);
      assert.equal(activeGames.has(g.channelId), false);
      assert.equal(g.timer, null);
      assert.equal(g.ended, true);
    } finally {
      mock.timers.reset();
    }
  });

  test('12. emptying the rack with an empty bag ends the game and moves rack points', () => {
    const g = mkGame();
    g.bag.length = 0;
    setRack(g, A, 'CAT');
    setRack(g, B, 'QZ');
    const r = playWord(g, A, 'CAT', 7, 7, 'h');
    assert.equal(r.ok, true);
    assert.equal(r.gameOver.reason, 'out');
    assert.equal(r.gameOver.moverId, A);
    assert.equal(g.players[A].score, 10 + 20);
    assert.equal(g.players[B].score, -20);
    assert.equal(r.gameOver.winnerId, A);
    assert.equal(r.gameOver.adjustments[A], 20);
    assert.equal(r.gameOver.adjustments[B], -20);
  });

  test('resign ends the game for the other player at any time', () => {
    const g = mkGame();
    const res = svc.resignGame(g, B); // not Bob's turn
    assert.equal(res.reason, 'resign');
    assert.equal(res.winnerId, A);
    assert.equal(res.loserId, B);
    assert.equal(svc.resignGame(g, A), res); // idempotent
  });
});

describe('timers and registry (R021, R087, R167)', () => {
  test('13. a stale game or sequence never ends a newer game; the real timer forfeits', async () => {
    mock.timers.enable({ apis: ['setTimeout'] });
    try {
      const channelId = nextChannel();
      const game1 = mkGame({ channelId, id: 'one' });
      assert.equal(registerGame(game1), true);
      const seq1 = game1.turnSeq;
      finishGame(game1, 'resign', { loserId: A });
      const game2 = mkGame({ channelId, id: 'two' });
      assert.equal(registerGame(game2), true);

      // stale game
      await handleTurnTimeout(game1, seq1);
      assert.equal(getGame(channelId), game2);
      assert.equal(game2.ended, false);

      // stale sequence
      const seq0 = game2.turnSeq;
      assert.equal(passTurn(game2, currentPlayerId(game2)).ok, true);
      await handleTurnTimeout(game2, seq0);
      assert.equal(game2.ended, false);
      assert.equal(getGame(channelId), game2);

      // the real timer (re-armed by the pass)
      const mover = currentPlayerId(game2);
      mock.timers.tick(TURN_TIMEOUT_MS - 1);
      assert.equal(game2.ended, false);
      mock.timers.tick(1);
      await new Promise((r) => setImmediate(r));
      assert.equal(game2.ended, true);
      assert.equal(game2.result.reason, 'timeout');
      assert.equal(game2.result.loserId, mover);
      assert.equal(game2.result.winnerId, otherPlayerId(game2, mover));
      assert.equal(getGame(channelId), null);
    } finally {
      mock.timers.reset();
    }
  });

  test('14. reservations: only the owner releases; registerGame honours them', () => {
    assert.equal(reserveChannel('c', 'x'), true);
    assert.equal(isChannelBusy('c'), true);
    assert.equal(reserveChannel('c', 'y'), false);
    releaseChannel('c', 'y');
    assert.equal(isChannelBusy('c'), true);
    const other = mkGame({ channelId: 'c', id: 'other' });
    assert.equal(registerGame(other), false);
    releaseChannel('c', 'x');
    assert.equal(isChannelBusy('c'), false);

    // the challenge id doubles as the game id, so the reserver can register
    assert.equal(reserveChannel('c', 'mine'), true);
    const mine = mkGame({ channelId: 'c', id: 'mine' });
    assert.equal(registerGame(mine), true);
    assert.equal(pendingChallenges.has('c'), false);
    assert.equal(registerGame(mkGame({ channelId: 'c', id: 'late' })), false);
    clearTurnTimer(mine);
  });

  test('registering a game arms a timer; finishing clears it', () => {
    mock.timers.enable({ apis: ['setTimeout'] });
    try {
      const g = mkGame();
      assert.equal(g.timer, null);
      registerGame(g);
      assert.notEqual(g.timer, null);
      finishGame(g, 'aborted');
      assert.equal(g.timer, null);
      assert.equal(g.result.reason, 'aborted');
      assert.equal(g.result.winnerId, null);
    } finally {
      mock.timers.reset();
    }
  });
});

describe('rendering and the rack button', () => {
  test('15. the finished embed has no Turn field; blanks render lowercase; the board is short', () => {
    const g = mkGame();
    setRack(g, A, 'C?ATXYZ');
    playWord(g, A, 'C?AT', 7, 7, 'h');
    const fin = buildGameEmbed(g, { finished: true }).toJSON();
    assert.equal(fin.fields.some((f) => f.name === 'Turn'), false);
    assert.equal(fin.fields.find((f) => f.name === 'Status')?.value, 'Game over');
    const live = buildGameEmbed(g).toJSON();
    assert.ok(live.fields.some((f) => f.name === 'Turn' && f.value.includes(`<@${B}>`)));
    const board = renderBoard(g.board);
    assert.ok(board.length < 1100);
    assert.match(board, /C {2}a {2}T/);
    assert.match(fin.footer.text, /Word check: off/);
    const withDict = mkGame({ dictionary: new Set(['CAT']) });
    assert.match(buildGameEmbed(withDict).toJSON().footer.text, /Word check: on/);
  });

  test('16. the rack button answers for no game, a stranger and a player', async () => {
    const replies = [];
    const click = (customId, userId, channelId = 'x') => ({
      customId, channelId, user: { id: userId },
      reply: async (o) => { replies.push(o); },
    });

    await handleRackButton(click('scrabble-rack:999', A));
    assert.match(replies.at(-1).content, /no longer active/);
    assert.equal(replies.at(-1).ephemeral, true);

    mock.timers.enable({ apis: ['setTimeout'] });
    try {
      const g = mkGame({ channelId: 'rack-ch', id: 'rack-game' });
      registerGame(g);
      await handleRackButton(click('scrabble-rack:rack-game', '333333333333333333'));
      assert.match(replies.at(-1).content, /You're not a player/);
      await handleRackButton(click('scrabble-rack:rack-game', A));
      assert.match(replies.at(-1).content, /Your tiles/);
      assert.match(replies.at(-1).content, /your turn/);
      await handleRackButton(click('scrabble-rack:rack-game', B));
      assert.match(replies.at(-1).content, /Waiting for \*\*Alice\*\*/);
      // the bare legacy id resolves by channel
      await handleRackButton(click('scrabble-rack', A, 'rack-ch'));
      assert.match(replies.at(-1).content, /Your tiles/);
      finishGame(g, 'aborted');
      await handleRackButton(click('scrabble-rack:rack-game', A));
      assert.match(replies.at(-1).content, /no longer active/);
    } finally {
      mock.timers.reset();
    }
  });

  test('17. rackRow builds scrabble-rack:{gameId}', () => {
    const row = rackRow('1234567890123456789').toJSON();
    const id = row.components[0].custom_id;
    assert.equal(id, 'scrabble-rack:1234567890123456789');
    assert.equal(parseCustomId(id).params[0], '1234567890123456789');
  });
});

// ── Fakes for the command-level tests ─────────────────────────────────────────

import { EventEmitter } from 'node:events';

const prisma = (await import('../src/database/client.js')).default;
const { getSettingsSafe } = await import('../src/services/settingsService.js');
const realDelegate = prisma.guildSettings;
// A settings row without a database: the commands load it through settingsService (memo, then this stub).
const SETTINGS = { guildId: 'guild-1', embedColor: 0x123456, gamesEnabled: true, economyEnabled: true };
prisma.guildSettings = { findUnique: async () => ({ ...SETTINGS }), upsert: async () => ({ ...SETTINGS }) };
after(() => { prisma.guildSettings = realDelegate; });
await getSettingsSafe('guild-1'); // warm the memo so peekSettings() answers synchronously

/** Mimics a discord.js collector: stop() ends it once; deliver() applies the filter and awaits every collect handler. */
class FakeCollector extends EventEmitter {
  constructor(options) {
    super();
    this.options = options;
    this.ended = false;
    this.reason = null;
    this.collected = new Map();
  }

  stop(reason = 'user') {
    if (this.ended) return;
    this.ended = true;
    this.reason = reason;
    this.emit('end', this.collected, reason);
  }

  async deliver(item) {
    if (this.ended) return false;
    if (this.options.filter && !this.options.filter(item)) return false;
    this.collected.set(String(this.collected.size), item);
    await Promise.all(this.listeners('collect').map((fn) => fn(item)));
    return true;
  }

  /**
   * A click that was already being dispatched when stop() ran: real discord.js awaits the filter before it
   * emits `collect`, so such a click still reaches the handler after the collector ended. This is the case
   * the settled / played / finished flags exist for.
   */
  async deliverRaw(item) {
    this.collected.set(String(this.collected.size), item);
    await Promise.all(this.listeners('collect').map((fn) => fn(item)));
    return true;
  }
}

let nextId = 1000000000000000;
const snowflake = () => String(nextId++);

function mkClick(customId, userId) {
  const calls = [];
  const i = {
    id: snowflake(), customId, user: { id: userId }, replied: false, deferred: false, calls,
    update: async (o) => { i.replied = true; calls.push(['update', o]); },
    reply: async (o) => { i.replied = true; calls.push(['reply', o]); },
    deferUpdate: async () => { i.deferred = true; calls.push(['deferUpdate']); },
    followUp: async (o) => { calls.push(['followUp', o]); return { id: `msg-${calls.length}` }; },
    editReply: async (o) => { calls.push(['editReply', o]); return { id: 'edited' }; },
  };
  return i;
}
const verbs = (i) => i.calls.map((c) => c[0]);

/** A slash-command interaction whose reply() returns a response that hands out FakeCollectors. */
function mkSlash({ userId, targetId, name = 'Player', channelId = nextChannel(), replyError: replyFails = null } = {}) {
  const collectors = [];
  const calls = [];
  const response = {
    createMessageComponentCollector(options) {
      const c = new FakeCollector(options);
      collectors.push(c);
      return c;
    },
  };
  const i = {
    id: snowflake(), guildId: 'guild-1', channelId, createdTimestamp: Date.now(),
    user: { id: userId, username: `user${userId}`, bot: false },
    member: { displayName: `${name}-me` },
    replied: false, deferred: false, calls, collectors, response,
    options: {
      getUser: () => ({ id: targetId, username: `user${targetId}`, bot: false }),
      getMember: () => ({ displayName: `${name}-target` }),
    },
    reply: async (o) => {
      calls.push(['reply', o]);
      if (replyFails) throw replyFails;
      i.replied = true;
      return response;
    },
    editReply: async (o) => { calls.push(['editReply', o]); return { id: 'edited' }; },
    followUp: async (o) => { calls.push(['followUp', o]); return { id: `msg-${calls.length}` }; },
  };
  return i;
}
const lastOf = (calls, verb) => calls.filter((c) => c[0] === verb).at(-1)?.[1];
const titleOf = (payload) => payload?.embeds?.[0]?.toJSON?.().title ?? '';
const descOf = (payload) => payload?.embeds?.[0]?.toJSON?.().description ?? '';

// ── tictactoe ─────────────────────────────────────────────────────────────────

describe('tictactoe', () => {
  test('18. checkWinner covers the 8 lines, draws and open boards', () => {
    for (const [a, b, c] of tttCmd.WIN_CONDITIONS) {
      const board = Array(9).fill('');
      board[a] = board[b] = board[c] = 'X';
      assert.equal(tttCmd.checkWinner(board), 'X');
    }
    assert.equal(tttCmd.checkWinner(['X', 'O', 'X', 'X', 'O', 'O', 'O', 'X', 'X']), 'draw');
    assert.equal(tttCmd.checkWinner(['X', 'O', '', '', '', '', '', '', '']), null);
  });

  test('18. buildBoard makes 3x3 buttons with ttt:id:n ids; filled and disabled cells are disabled', () => {
    const rows = tttCmd.buildBoard(Array(9).fill(''), 'id').map((r) => r.toJSON());
    assert.equal(rows.length, 3);
    assert.ok(rows.every((r) => r.components.length === 3));
    assert.deepEqual(rows.flatMap((r) => r.components.map((c) => c.custom_id)), Array.from({ length: 9 }, (_, n) => `ttt:id:${n}`));
    assert.ok(rows.flatMap((r) => r.components).every((c) => !c.disabled));
    const filled = tttCmd.buildBoard(['X', '', '', '', 'O', '', '', '', ''], 'id').flatMap((r) => r.toJSON().components);
    assert.equal(filled[0].disabled, true);
    assert.equal(filled[4].disabled, true);
    assert.ok(!filled[1].disabled);
    assert.ok(tttCmd.buildBoard(Array(9).fill(''), 'id', true).flatMap((r) => r.toJSON().components).every((c) => c.disabled));
  });

  async function startGame() {
    const x = snowflake();
    const o = snowflake();
    const it = mkSlash({ userId: x, targetId: o });
    await tttCmd.execute(it);
    const [challenge] = it.collectors;
    const accept = mkClick(`ttt-ch:${it.id}:accept`, o);
    return { x, o, it, challenge, accept };
  }

  test('a double Accept starts exactly one game (R090) with idle + capped timers (R022/R091)', async () => {
    const { o, it, challenge, accept } = await startGame();
    const second = mkClick(`ttt-ch:${it.id}:accept`, o);
    await Promise.all([challenge.deliver(accept), challenge.deliverRaw(second)]);
    assert.equal(it.collectors.length, 2, 'one challenge collector and one game collector');
    assert.equal(challenge.reason, 'accept');
    assert.deepEqual(verbs(accept), ['update']);
    assert.deepEqual(verbs(second), ['deferUpdate'], 'the in-flight second click is acknowledged and ignored (settled)');
    const game = it.collectors[1];
    assert.equal(game.options.idle, tttCmd.MOVE_IDLE_MS);
    assert.equal(game.options.time, tttCmd.GAME_MAX_MS);
    assert.notEqual(game.options.time, 60_000);
    assert.ok(game.options.filter({ customId: `ttt:${it.id}:3` }));
    assert.equal(game.options.filter({ customId: `ttt-ch:${it.id}:accept` }), false);
    assert.equal(game.options.filter({ customId: 'ttt:other:3' }), false);
    game.stop('done');
  });

  test('moves, spectators, wrong turn, a win and late clicks (R241, R242)', async () => {
    const { x, o, it, challenge, accept } = await startGame();
    await challenge.deliver(accept);
    const game = it.collectors[1];
    const click = (userId, n) => mkClick(`ttt:${it.id}:${n}`, userId);

    const spectator = click(snowflake(), 0);
    await game.deliver(spectator);
    assert.match(lastOf(spectator.calls, 'reply').content, /not a player in this game/);

    const early = click(o, 4); // it is X's turn
    await game.deliver(early);
    assert.match(lastOf(early.calls, 'reply').content, /turn/);

    for (const [u, n] of [[x, 0], [o, 3], [x, 1], [o, 4]]) {
      const m = click(u, n);
      await game.deliver(m);
      assert.deepEqual(verbs(m), ['update']);
    }
    const taken = click(x, 3); // already filled
    await game.deliver(taken);
    assert.deepEqual(verbs(taken), ['deferUpdate']);

    const win = click(x, 2);
    const late = click(o, 5);
    // The winning click and a click that arrives before its update lands.
    await Promise.all([game.deliver(win), game.deliverRaw(late)]);
    assert.equal(game.reason, 'done');
    assert.match(titleOf(lastOf(win.calls, 'update')), /Winner/);
    assert.match(descOf(lastOf(win.calls, 'update')), /wins with \*\*X\*\*s/);
    assert.deepEqual(verbs(late), ['deferUpdate'], 'no move after the win (finished)');
    // the lock is released: the same players can start again
    const again = mkSlash({ userId: x, targetId: o });
    await tttCmd.execute(again);
    assert.equal(lastOf(again.calls, 'reply').ephemeral, undefined);
    again.collectors[0].stop('time');
  });

  test('wrong users and the challenger on a challenge get friendly replies', async () => {
    const { x, it, challenge } = await startGame();
    const stranger = mkClick(`ttt-ch:${it.id}:accept`, snowflake());
    await challenge.deliver(stranger);
    assert.match(lastOf(stranger.calls, 'reply').content, /isn't for you/);
    const self = mkClick(`ttt-ch:${it.id}:accept`, x);
    await challenge.deliver(self);
    assert.match(lastOf(self.calls, 'reply').content, /Waiting for/);
    const cancel = mkClick(`ttt-ch:${it.id}:decline`, x);
    await challenge.deliver(cancel);
    assert.equal(challenge.reason, 'cancel');
    assert.match(descOf(lastOf(cancel.calls, 'update')), /cancelled the challenge/);
    // released again
    const again = mkSlash({ userId: x, targetId: snowflake() });
    await tttCmd.execute(again);
    assert.equal(again.collectors.length, 1);
    again.collectors[0].stop('time');
  });

  test('timeouts edit the message and release the locks', async () => {
    const { x, o, it, challenge } = await startGame();
    challenge.stop('time');
    assert.match(titleOf(lastOf(it.calls, 'editReply')), /Challenge Timed Out/);

    const g = await startGame();
    await g.challenge.deliver(g.accept);
    g.it.collectors[1].stop('idle');
    assert.match(descOf(lastOf(g.it.calls, 'editReply')), /didn't move within 60 seconds/);

    const h = await startGame();
    await h.challenge.deliver(h.accept);
    h.it.collectors[1].stop('time');
    assert.match(descOf(lastOf(h.it.calls, 'editReply')), /10-minute limit/);

    // every lock is free
    const again = mkSlash({ userId: x, targetId: o });
    await tttCmd.execute(again);
    assert.equal(again.collectors.length, 1);
    again.collectors[0].stop('time');
  });

  test('R089: a failed reply releases both locks', async () => {
    const x = snowflake();
    const o = snowflake();
    const bad = mkSlash({ userId: x, targetId: o, replyError: new Error('boom') });
    await assert.rejects(() => tttCmd.execute(bad), /boom/);
    const ok = mkSlash({ userId: x, targetId: o });
    await tttCmd.execute(ok);
    assert.equal(ok.collectors.length, 1);
    ok.collectors[0].stop('time');
  });

  test('pre-checks refuse in order', async () => {
    const x = snowflake();
    const self = mkSlash({ userId: x, targetId: x });
    await tttCmd.execute(self);
    assert.match(lastOf(self.calls, 'reply').content, /cannot challenge yourself/);

    const bot = mkSlash({ userId: x, targetId: snowflake() });
    bot.options.getUser = () => ({ id: '9', username: 'bot', bot: true });
    await tttCmd.execute(bot);
    assert.match(lastOf(bot.calls, 'reply').content, /cannot challenge a bot/);

    const absent = mkSlash({ userId: x, targetId: snowflake() });
    absent.options.getMember = () => null;
    await tttCmd.execute(absent);
    assert.match(lastOf(absent.calls, 'reply').content, /isn't in this server/);

    const busy = mkSlash({ userId: x, targetId: snowflake() });
    await tttCmd.execute(busy);
    const second = mkSlash({ userId: x, targetId: snowflake() });
    await tttCmd.execute(second);
    assert.match(lastOf(second.calls, 'reply').content, /already have an active/);
    const third = mkSlash({ userId: snowflake(), targetId: x });
    await tttCmd.execute(third);
    assert.match(lastOf(third.calls, 'reply').content, /is already in a game/);
    busy.collectors[0].stop('time');
  });
});

// ── rps ───────────────────────────────────────────────────────────────────────

describe('rps', () => {
  test('19. decide covers all nine pairs', () => {
    const expected = {
      'rock,rock': 'draw', 'rock,paper': 'lose', 'rock,scissors': 'win',
      'paper,rock': 'win', 'paper,paper': 'draw', 'paper,scissors': 'lose',
      'scissors,rock': 'lose', 'scissors,paper': 'win', 'scissors,scissors': 'draw',
    };
    for (const [pair, outcome] of Object.entries(expected)) {
      const [p, b] = pair.split(',');
      assert.equal(rpsCmd.decide(p, b), outcome, pair);
    }
  });

  test('a double click plays one round (R237); other users are told it is not their game', async () => {
    const u = snowflake();
    const it = mkSlash({ userId: u });
    await rpsCmd.execute(it);
    const [c] = it.collectors;
    assert.equal(c.options.time, 30_000);
    assert.ok(c.options.filter({ customId: `rps:${it.id}:rock` }));
    assert.equal(c.options.filter({ customId: 'rps:other:rock' }), false);

    const stranger = mkClick(`rps:${it.id}:rock`, snowflake());
    await c.deliver(stranger);
    assert.match(lastOf(stranger.calls, 'reply').content, /isn't your game/);
    assert.equal(c.ended, false);

    const first = mkClick(`rps:${it.id}:rock`, u);
    const second = mkClick(`rps:${it.id}:paper`, u);
    await Promise.all([c.deliver(first), c.deliverRaw(second)]);
    assert.deepEqual(verbs(first), ['update']);
    assert.deepEqual(verbs(second), ['deferUpdate'], 'the in-flight second click is acknowledged and ignored (played)');
    assert.equal(c.reason, 'done');
    assert.match(descOf(lastOf(first.calls, 'update')), /You: 🪨 \*\*rock\*\*/);
    assert.ok(lastOf(first.calls, 'update').components[0].toJSON().components.every((b) => b.disabled));
    // released
    const again = mkSlash({ userId: u });
    await rpsCmd.execute(again);
    assert.equal(again.collectors.length, 1);
    again.collectors[0].stop('time');
  });

  test('a timeout edits the message; a failed reply releases the lock; a second game is refused', async () => {
    const u = snowflake();
    const it = mkSlash({ userId: u });
    await rpsCmd.execute(it);
    const dup = mkSlash({ userId: u });
    await rpsCmd.execute(dup);
    assert.match(lastOf(dup.calls, 'reply').content, /already have an active RPS game/);
    it.collectors[0].stop('time');
    assert.match(descOf(lastOf(it.calls, 'editReply')), /Time's up/);

    const bad = mkSlash({ userId: u, replyError: new Error('boom') });
    await assert.rejects(() => rpsCmd.execute(bad), /boom/);
    const ok = mkSlash({ userId: u });
    await rpsCmd.execute(ok);
    assert.equal(ok.collectors.length, 1);
    ok.collectors[0].stop('user');
    assert.equal(ok.calls.filter((c) => c[0] === 'editReply').length, 0, 'no edit for other end reasons');
  });
});

// ── numguess ──────────────────────────────────────────────────────────────────

describe('numguess', () => {
  test('20. evaluateGuess', () => {
    const e = numguessCmd.evaluateGuess;
    assert.equal(e(42, 42), 'correct');
    assert.equal(e(42, 10), 'higher');
    assert.equal(e(42, 90), 'lower');
    for (const bad of [0, 101, NaN, 1.5, -3]) assert.equal(e(42, bad), 'out-of-range');
  });

  function mkChannel({ canView = true } = {}) {
    const ch = {
      collectors: [],
      isTextBased: () => true,
      guild: { members: { me: {} } },
      permissionsFor: () => ({ has: () => canView }),
      createMessageCollector(options) {
        const c = new FakeCollector(options);
        ch.collectors.push(c);
        return c;
      },
    };
    return ch;
  }
  const guessMsg = (userId, text) => ({ author: { id: userId }, content: text });

  async function startGame({ channel = mkChannel(), userId = snowflake() } = {}) {
    const it = mkSlash({ userId });
    it.channel = channel;
    await numguessCmd.execute(it);
    return { it, userId, channel, collector: channel.collectors.at(-1) };
  }
  const feedback = (it) => it.calls.filter((c) => c[0] === 'followUp').map((c) => c[1]);

  test('timers: idle 60 s (reset by guesses) + 10 min cap, no absolute 2-minute limit (R234)', async () => {
    const { it, collector } = await startGame();
    assert.equal(collector.options.idle, numguessCmd.GUESS_IDLE_MS);
    assert.equal(collector.options.time, numguessCmd.GAME_MAX_MS);
    assert.match(descOf(lastOf(it.calls, 'reply')), /60 seconds/);
    assert.equal(collector.options.filter(guessMsg(it.user.id, ' 42 ')), true);
    assert.equal(collector.options.filter(guessMsg(it.user.id, 'hello')), false);
    assert.equal(collector.options.filter(guessMsg('someone-else', '42')), false);
    collector.stop('user');
  });

  test('refuses without View Channel (R080) and for a second game', async () => {
    const noView = mkSlash({ userId: snowflake() });
    noView.channel = mkChannel({ canView: false });
    await numguessCmd.execute(noView);
    assert.match(titleOf(lastOf(noView.calls, 'reply')), /Missing Permissions/);
    assert.equal(noView.channel.collectors.length, 0);

    const { userId, collector } = await startGame();
    const dup = mkSlash({ userId });
    dup.channel = mkChannel();
    await numguessCmd.execute(dup);
    assert.match(lastOf(dup.calls, 'reply').content, /already have an active guessing game/);
    collector.stop('user');
  });

  test('a full game by binary search: hints through followUp, stop before the reply, nothing after the win', async () => {
    const { it, userId, collector } = await startGame();
    let lo = 1;
    let hi = 100;
    let won = false;
    for (let n = 0; n < 7 && !won; n++) {
      const guess = Math.floor((lo + hi) / 2);
      await collector.deliver(guessMsg(userId, String(guess)));
      const title = titleOf(feedback(it).at(-1));
      if (/Correct/.test(title)) won = true;
      else if (/Higher/.test(title)) lo = guess + 1;
      else hi = guess - 1;
    }
    assert.equal(won, true);
    assert.equal(collector.reason, 'won');
    const before = feedback(it).length;
    assert.equal(await collector.deliver(guessMsg(userId, '50')), false);
    assert.equal(feedback(it).length, before);
    // 'won' ends without an extra message
    assert.ok(!feedback(it).some((p) => /Time's Up/.test(titleOf(p))));
  });

  test('seven wrong guesses end the game; in-flight guesses after the decision are ignored (R235)', async () => {
    for (let attempt = 0; attempt < 6; attempt++) {
      const { it, userId, collector } = await startGame();
      await collector.deliver(guessMsg(userId, '1'));
      if (/Correct/.test(titleOf(feedback(it).at(-1)))) continue; // the secret was 1: try another game
      for (let n = 2; n <= 6; n++) await collector.deliver(guessMsg(userId, '1'));
      assert.match(descOf(feedback(it).at(-1)), /1 guess remaining/);
      const last = collector.deliver(guessMsg(userId, '1'));
      const inFlight = collector.deliverRaw(guessMsg(userId, '1')); // dispatched before stop(), handled after
      await Promise.all([last, inFlight]);
      assert.equal(collector.reason, 'exhausted');
      assert.match(titleOf(feedback(it).at(-1)), /Out of Guesses/);
      assert.equal(feedback(it).filter((p) => /Out of Guesses/.test(titleOf(p))).length, 1);
      return;
    }
    assert.fail('the secret was 1 six times in a row');
  });

  test('out-of-range warnings are throttled and do not count', async () => {
    const { it, userId, collector } = await startGame();
    await collector.deliver(guessMsg(userId, '0'));
    await collector.deliver(guessMsg(userId, '101'));
    await collector.deliver(guessMsg(userId, '500'));
    const warns = feedback(it).filter((p) => /Out of Range/.test(titleOf(p)));
    assert.equal(warns.length, 1);
    await collector.deliver(guessMsg(userId, '50'));
    assert.match(descOf(feedback(it).at(-1)), /6 guesses remaining|Correct|number was/);
    collector.stop('user');
  });

  test('end reasons: idle (with and without a read-permission hint), time, and silence for the rest', async () => {
    const a = await startGame();
    a.collector.stop('idle');
    assert.match(descOf(feedback(a.it).at(-1)), /didn't guess for 60 seconds/);
    assert.match(descOf(feedback(a.it).at(-1)), /may not be able to read messages/);

    const b = await startGame();
    await b.collector.deliver(guessMsg(b.userId, '50'));
    b.collector.stop('idle');
    if (!b.collector.reason || b.collector.reason === 'idle') {
      assert.doesNotMatch(descOf(feedback(b.it).at(-1)), /may not be able to read/);
    }

    const c = await startGame();
    c.collector.stop('time');
    assert.match(descOf(feedback(c.it).at(-1)), /10-minute limit/);

    const d = await startGame();
    const n = feedback(d.it).length;
    d.collector.stop('channelDelete');
    assert.equal(feedback(d.it).length, n);
  });

  test('R089: a failed intro reply releases the lock', async () => {
    const userId = snowflake();
    const bad = mkSlash({ userId, replyError: new Error('boom') });
    bad.channel = mkChannel();
    await assert.rejects(() => numguessCmd.execute(bad), /boom/);
    const ok = await startGame({ userId });
    assert.equal(ok.channel.collectors.length, 1);
    ok.collector.stop('user');
  });
});

// ── /scrabble (challenge) ─────────────────────────────────────────────────────

describe('/scrabble challenge', () => {
  const { dictionaryStatus } = svc;

  async function challenge(channelId = nextChannel()) {
    const challenger = snowflake();
    const target = snowflake();
    const it = mkSlash({ userId: challenger, targetId: target, channelId });
    await scrabbleCmd.execute(it);
    return { challenger, target, it, channelId, collector: it.collectors[0] };
  }
  const accept = (it, userId) => mkClick(`scrabble-ch:${it.id}:accept`, userId);
  const decline = (it, userId) => mkClick(`scrabble-ch:${it.id}:decline`, userId);

  test('a challenge reserves the channel (R085); a second one is refused', async () => {
    const { it, channelId, collector } = await challenge();
    assert.equal(pendingChallenges.get(channelId), it.id);
    assert.equal(lastOf(it.calls, 'reply').allowedMentions.users.length, 1);
    assert.ok(collector.options.filter({ customId: `scrabble-ch:${it.id}:accept` }));
    assert.equal(collector.options.filter({ customId: 'scrabble-ch:other:accept' }), false);
    assert.equal(collector.options.time, 30_000);

    const second = mkSlash({ userId: snowflake(), targetId: snowflake(), channelId });
    await scrabbleCmd.execute(second);
    assert.match(lastOf(second.calls, 'reply').content, /already a Scrabble game or a pending challenge/);
    assert.equal(second.collectors.length, 0);
    collector.stop('user');
    assert.equal(isChannelBusy(channelId), false, 'any other end reason releases the reservation');
  });

  test('a double Accept starts exactly one game, arms its timer and posts the board (R021, R086, R087)', async () => {
    const { challenger, target, it, channelId, collector } = await challenge();
    const first = accept(it, target);
    const second = accept(it, target);
    await Promise.all([collector.deliver(first), collector.deliverRaw(second)]);
    const game = getGame(channelId);
    assert.ok(game);
    assert.equal(game.id, it.id);
    assert.notEqual(game.timer, null, 'a registered game always has a timer');
    assert.equal(game.dictionary, null);
    assert.equal(game.settings.embedColor, 0x123456, 'the guild colour is captured');
    assert.equal(pendingChallenges.has(channelId), false);
    assert.equal(collector.reason, 'accept');
    assert.deepEqual(verbs(first), ['deferUpdate', 'editReply', 'followUp'], 'acknowledged first, then edited, then the board');
    assert.deepEqual(verbs(second), ['deferUpdate'], 'the in-flight second click is acknowledged and ignored (settled)');
    assert.match(descOf(first.calls[1][1] ? { embeds: first.calls[1][1].embeds } : null), /accepted/);
    const board = lastOf(first.calls, 'followUp');
    assert.match(board.content, new RegExp(`<@${challenger}>, you go first`));
    assert.deepEqual(board.allowedMentions, { users: [challenger] });
    assert.equal(board.components[0].toJSON().components[0].custom_id, `scrabble-rack:${it.id}`);
    assert.equal(game.lastBoard.messageId, `msg-${first.calls.length}`);
    assert.equal(game.lastBoard.interaction, first);

    // a stray Accept after the start never replaces the running game
    const late = accept(it, target);
    assert.equal(await collector.deliver(late), false);
    assert.equal(getGame(channelId), game);
    // and a new challenge in the channel is refused while the game runs
    const another = mkSlash({ userId: snowflake(), targetId: snowflake(), channelId });
    await scrabbleCmd.execute(another);
    assert.match(lastOf(another.calls, 'reply').content, /already a Scrabble game/);
  });

  test('strangers and the challenger get friendly replies; cancel and decline free the channel', async () => {
    const a = await challenge();
    const stranger = accept(a.it, snowflake());
    await a.collector.deliver(stranger);
    assert.match(lastOf(stranger.calls, 'reply').content, /isn't for you/);
    const self = accept(a.it, a.challenger);
    await a.collector.deliver(self);
    assert.match(lastOf(self.calls, 'reply').content, /Waiting for/);
    const cancel = decline(a.it, a.challenger);
    await a.collector.deliver(cancel);
    assert.equal(a.collector.reason, 'cancel');
    assert.match(descOf(lastOf(cancel.calls, 'update')), /cancelled the challenge/);
    assert.equal(isChannelBusy(a.channelId), false);

    const b = await challenge();
    const no = decline(b.it, b.target);
    await b.collector.deliver(no);
    assert.equal(b.collector.reason, 'decline');
    assert.match(descOf(lastOf(no.calls, 'update')), /declined the Scrabble challenge/);
    assert.equal(isChannelBusy(b.channelId), false);
  });

  test('a timeout edits the challenge and frees the channel', async () => {
    const { it, channelId, collector } = await challenge();
    collector.stop('time');
    assert.match(titleOf(lastOf(it.calls, 'editReply')), /Challenge Timed Out/);
    assert.equal(isChannelBusy(channelId), false);
  });

  test('R087: a start that fails leaves no game, no timer and a free channel', async () => {
    const a = await challenge();
    const broken = accept(a.it, a.target);
    broken.editReply = async () => { throw new Error('edit failed'); };
    await a.collector.deliver(broken);
    assert.equal(getGame(a.channelId), null);
    assert.equal(isChannelBusy(a.channelId), false);
    assert.equal(lastOf(broken.calls, 'followUp').ephemeral, true);
    assert.match(lastOf(broken.calls, 'followUp').content, /couldn't start the game/);

    const b = await challenge();
    const noBoard = accept(b.it, b.target);
    noBoard.followUp = async () => { throw new Error('no webhook'); };
    await b.collector.deliver(noBoard);
    assert.equal(getGame(b.channelId), null);
    assert.equal(isChannelBusy(b.channelId), false);
  });

  test('R089: a failed challenge reply releases the reservation', async () => {
    const channelId = nextChannel();
    const bad = mkSlash({ userId: snowflake(), targetId: snowflake(), channelId, replyError: new Error('boom') });
    await assert.rejects(() => scrabbleCmd.execute(bad), /boom/);
    assert.equal(isChannelBusy(channelId), false);
  });

  test('pre-checks: self, bot, not a member', async () => {
    const x = snowflake();
    const self = mkSlash({ userId: x, targetId: x });
    await scrabbleCmd.execute(self);
    assert.match(lastOf(self.calls, 'reply').content, /cannot challenge yourself/);
    const bot = mkSlash({ userId: x, targetId: snowflake() });
    bot.options.getUser = () => ({ id: '9', username: 'bot', bot: true });
    await scrabbleCmd.execute(bot);
    assert.match(lastOf(bot.calls, 'reply').content, /cannot challenge a bot/);
    const absent = mkSlash({ userId: x, targetId: snowflake() });
    absent.options.getMember = () => null;
    await scrabbleCmd.execute(absent);
    assert.match(lastOf(absent.calls, 'reply').content, /isn't in this server/);
    assert.ok(dictionaryStatus());
  });
});

// ── /scrabble-play ────────────────────────────────────────────────────────────

describe('/scrabble-play', () => {
  function mkPlay(sub, userId, channelId, opts = {}) {
    const calls = [];
    const i = {
      id: snowflake(), guildId: 'guild-1', channelId, user: { id: userId },
      deferred: false, replied: false, ephemeral: null, calls,
      options: {
        getSubcommand: () => sub,
        getString: (n) => opts[n] ?? null,
        getInteger: (n) => opts[n] ?? null,
      },
      deferReply: async ({ ephemeral }) => { i.deferred = true; i.ephemeral = ephemeral; calls.push(['deferReply', { ephemeral }]); },
      editReply: async (o) => { i.replied = true; calls.push(['editReply', o]); return { id: 'edited' }; },
      followUp: async (o) => { calls.push(['followUp', o]); return { id: `msg-${calls.length}` }; },
    };
    return i;
  }
  const word = (userId, channelId, w, row, col, direction = 'h') => mkPlay('word', userId, channelId, { word: w, row, col, direction });

  function liveGame() {
    mock.timers.enable({ apis: ['setTimeout'] });
    const channelId = nextChannel();
    const g = mkGame({ channelId });
    registerGame(g);
    setRack(g, A, 'CATXYZE');
    setRack(g, B, 'OXQJKLM');
    return { g, channelId };
  }

  test('R083/R084: acknowledges first, edits the private reply BEFORE the public board, pings the next player', async () => {
    try {
      const { g, channelId } = liveGame();
      const it = word(A, channelId, 'cat', 8, 'H');
      await scrabblePlayCmd.execute(it);
      assert.deepEqual(verbs(it), ['deferReply', 'editReply', 'followUp']);
      assert.equal(it.ephemeral, true);
      assert.match(titleOf(lastOf(it.calls, 'editReply')), /Word Played/);
      assert.match(descOf(lastOf(it.calls, 'editReply')), /\*\*CAT\*\* \(10\) = \*\*10\*\* pts/);
      const pub = lastOf(it.calls, 'followUp');
      assert.match(pub.content, new RegExp(`<@${B}>, it's your turn! \\*\\*Alice\\*\\* played \\*\\*CAT\\*\\* \\(10\\) for \\*\\*10\\*\\* points`));
      assert.deepEqual(pub.allowedMentions, { users: [B] });
      assert.equal(pub.ephemeral, undefined, 'the board is public');
      assert.equal(g.currentTurn, 1);
      assert.equal(g.lastBoard.interaction, it);
    } finally {
      mock.timers.reset();
    }
  });

  test('R083: two concurrent plays by one player apply once; the other is "not your turn"', async () => {
    try {
      const { g, channelId } = liveGame();
      const first = word(A, channelId, 'CAT', 8, 'H');
      const second = word(A, channelId, 'XE', 9, 'H');
      await Promise.all([scrabblePlayCmd.execute(first), scrabblePlayCmd.execute(second)]);
      assert.equal(g.moves, 1);
      assert.match(titleOf(lastOf(first.calls, 'editReply')), /Word Played/);
      assert.match(titleOf(lastOf(second.calls, 'editReply')), /Invalid Move/);
      assert.match(descOf(lastOf(second.calls, 'editReply')), /not your turn/);
      assert.ok(!verbs(second).includes('followUp'));
    } finally {
      mock.timers.reset();
    }
  });

  test('errors: invalid move, no game, not a player', async () => {
    try {
      const { g, channelId } = liveGame();
      const bad = word(A, channelId, 'CAT', 1, 'A');
      await scrabblePlayCmd.execute(bad);
      assert.deepEqual(verbs(bad), ['deferReply', 'editReply']);
      assert.match(descOf(lastOf(bad.calls, 'editReply')), /centre square/);
      assert.equal(g.currentTurn, 0);

      const stranger = word(snowflake(), channelId, 'CAT', 8, 'H');
      await scrabblePlayCmd.execute(stranger);
      assert.match(titleOf(lastOf(stranger.calls, 'editReply')), /Not Your Game/);

      const nobody = word(A, nextChannel(), 'CAT', 8, 'H');
      await scrabblePlayCmd.execute(nobody);
      assert.match(titleOf(lastOf(nobody.calls, 'editReply')), /No Active Game/);
    } finally {
      mock.timers.reset();
    }
  });

  test('R281: pass, exchange (never reveals the tiles) and the six-scoreless-turn ending with a result post', async () => {
    try {
      const { g, channelId } = liveGame();
      const pass = mkPlay('pass', A, channelId);
      await scrabblePlayCmd.execute(pass);
      assert.match(lastOf(pass.calls, 'followUp').content, new RegExp(`<@${B}>, it's your turn! \\*\\*Alice\\*\\* passed`));

      const swap = mkPlay('exchange', B, channelId, { tiles: 'OX' });
      await scrabblePlayCmd.execute(swap);
      const text = lastOf(swap.calls, 'followUp').content;
      assert.match(text, /exchanged 2 tiles/);
      assert.doesNotMatch(text, /\*\*O\*\*|\*\*X\*\*/);
      assert.match(descOf(lastOf(swap.calls, 'editReply')), /swapped \*\*2\*\* tiles/);
      assert.equal(g.scorelessTurns, 2);

      let last;
      for (let n = 0; n < 4; n++) {
        last = mkPlay('pass', currentPlayerId(g), channelId);
        await scrabblePlayCmd.execute(last);
      }
      assert.equal(g.ended, true);
      assert.equal(getGame(channelId), null);
      const result = lastOf(last.calls, 'followUp');
      assert.match(result.content, new RegExp(`🏁 <@${A}> <@${B}>`));
      assert.equal(result.embeds.length, 2);
      assert.equal(result.embeds[0].toJSON().fields.some((f) => f.name === 'Turn'), false);
      assert.match(result.embeds[1].toJSON().description, /Six scoreless turns/);
    } finally {
      mock.timers.reset();
    }
  });

  test('resign: asks for confirmation (outside the mutex), then posts the result; cancelling keeps the game', async () => {
    try {
      const { g, channelId } = liveGame();
      const drive = async (userId, answer) => {
        const it = mkPlay('resign', userId, channelId);
        let collector = null;
        let yesId = null;
        let noId = null;
        it.editReply = async (o) => {
          it.calls.push(['editReply', o]);
          it.replied = true;
          const ids = o.components?.[0]?.toJSON().components.map((c) => c.custom_id);
          if (ids) [yesId, noId] = ids;
          return { id: 'confirm', createMessageComponentCollector(opts) { collector = new FakeCollector(opts); return collector; } };
        };
        const run = scrabblePlayCmd.execute(it);
        for (let n = 0; n < 50 && !collector; n++) await new Promise((r) => setImmediate(r));
        assert.ok(collector, 'the confirm dialog was shown');
        await collector.deliver(mkClick(answer ? yesId : noId, userId));
        collector.stop('limit');
        await run;
        return it;
      };

      const keep = await drive(B, false);
      assert.equal(g.ended, false);
      assert.ok(!verbs(keep).includes('followUp'));

      const out = await drive(B, true);
      assert.equal(g.ended, true);
      assert.equal(g.result.reason, 'resign');
      const post = lastOf(out.calls, 'followUp');
      assert.match(post.embeds[1].toJSON().description, /\*\*Bob\*\* resigned/);
      assert.match(post.embeds[1].toJSON().description, /Winner: \*\*Alice\*\*/);

      const gone = mkPlay('resign', A, channelId);
      await scrabblePlayCmd.execute(gone);
      assert.match(titleOf(lastOf(gone.calls, 'editReply')), /No Active Game/);
    } finally {
      mock.timers.reset();
    }
  });
});

// ── /scrabble-help ────────────────────────────────────────────────────────────

describe('/scrabble-help', () => {
  test('every field stays within 1024 characters and the dictionary line follows the status', () => {
    for (const status of [{ state: 'missing', size: 0 }, { state: 'loaded', size: 172823 }]) {
      const json = scrabbleHelpCmd.buildHelpEmbed(null, status).toJSON();
      assert.ok(json.fields.every((f) => f.value.length <= 1024 && f.name.length <= 256));
      assert.ok(json.fields.length <= 25);
      const rules = json.fields.find((f) => f.name.includes('Rules')).value;
      assert.match(rules, status.state === 'loaded' ? /\*\*172,823-word\*\* list/ : /Word checking is \*\*off\*\*/);
    }
    const names = scrabbleHelpCmd.buildHelpEmbed(null, { state: 'missing', size: 0 }).toJSON().fields.map((f) => f.name);
    assert.deepEqual(names.length, 6);
    assert.ok(names.some((n) => n.includes('Ending the Game')));
  });

  test('acknowledges with a public defer and then edits in the embed', async () => {
    const calls = [];
    const it = {
      guildId: 'guild-1', deferred: false, replied: false,
      deferReply: async (o) => { it.deferred = true; calls.push(['deferReply', o]); },
      editReply: async (o) => { calls.push(['editReply', o]); },
    };
    __testing.setDictionary(new Set(['AA', 'AB']));
    await scrabbleHelpCmd.execute(it);
    assert.equal(calls[0][0], 'deferReply');
    assert.notEqual(calls[0][1]?.ephemeral, true);
    const rules = calls[1][1].embeds[0].toJSON().fields.find((f) => f.name.includes('Rules')).value;
    assert.match(rules, /2-word/);
  });
});

// ── /games ────────────────────────────────────────────────────────────────────

describe('/games', () => {
  const gameFiles = readdirSync(resolve(ROOT, 'src/commands/games')).filter((f) => f.endsWith('.js')).map((f) => f.replace(/\.js$/, ''));
  const entry = (name, module = 'games', description = `Description of ${name}`) => ({ name, module, description });

  test('21. fields stay within limits, every real game has an emoji, the casino group needs economy entries', () => {
    const real = gameFiles.filter((n) => n !== 'games');
    assert.ok(real.length >= 7);
    for (const name of real) assert.ok(gamesCmd.GAME_EMOJI[name], `${name} needs a GAME_EMOJI entry`);

    const synthetic = Array.from({ length: 40 }, (_, n) => entry(`synthetic-game-${n}`, 'games', 'x'.repeat(150)));
    const economy = [entry('blackjack', 'economy'), entry('roulette', 'economy'), entry('slots', 'economy')];
    const withCasino = gamesCmd.buildGamesEmbed([...real.map((n) => entry(n, ['blackjack', 'roulette', 'slots'].includes(n) ? 'economy' : 'games')), ...synthetic], null).toJSON();
    assert.ok(withCasino.fields.length <= 25);
    assert.ok(withCasino.fields.every((f) => f.value.length <= 1024));
    assert.ok(withCasino.fields.some((f) => f.name.includes('Casino')));
    assert.match(withCasino.footer.text, /see \/balance/);
    assert.ok(JSON.stringify(withCasino).length < 6000);

    const noCasino = gamesCmd.buildGamesEmbed([entry('rps'), entry('scrabble'), entry('scrabble-play'), entry('scrabble-help')], null).toJSON();
    assert.ok(!noCasino.fields.some((f) => f.name.includes('Casino')));
    assert.equal(noCasino.footer, undefined);
    assert.deepEqual(noCasino.fields.map((f) => f.name), ['🕹️ Games', '🔤 Scrabble']);
    assert.deepEqual(noCasino.fields[1].value.split('\n').map((l) => l.match(/`\/([a-z-]+)`/)[1]), ['scrabble', 'scrabble-play', 'scrabble-help']);
    assert.equal(withCasino.fields.length > 0, true);
    void economy;
  });

  test('an empty listing says so, and other modules get their own group', () => {
    assert.match(gamesCmd.buildGamesEmbed([], null).toJSON().description, /No games are available/);
    const other = gamesCmd.buildGamesEmbed([entry('trivia', 'achievements')], null).toJSON();
    assert.match(other.fields[0].name, /Achievements/);
  });

  test('collectGameCommands uses the category or the module, and skips /games', () => {
    const mod = (name, module, description = 'd') => ({ data: { name, description }, module });
    const client = {
      commands: new Map([
        ['games', mod('games', 'games')], ['rps', mod('rps', 'games')], ['slots', mod('slots', 'economy')],
        ['ping', mod('ping', null)], ['oddball', mod('oddball', 'games')], ['nomod', { data: { name: 'nomod', description: 'x' } }],
      ]),
      commandCategories: new Map([['games', 'games'], ['rps', 'games'], ['slots', 'games'], ['ping', 'utility'], ['nomod', 'games']]),
    };
    const got = gamesCmd.collectGameCommands(client);
    assert.deepEqual(got.map((e) => e.name).sort(), ['nomod', 'oddball', 'rps', 'slots']);
    assert.equal(got.find((e) => e.name === 'nomod').module, 'games');
    // without commandCategories the module still finds games
    const noCat = gamesCmd.collectGameCommands({ commands: client.commands });
    assert.deepEqual(noCat.map((e) => e.name).sort(), ['oddball', 'rps']);
  });

  test('execute hides games whose module is off', async () => {
    const calls = [];
    const it = { guildId: 'guild-1', createdTimestamp: Date.now(), reply: async (o) => { calls.push(o); } };
    const mod = (name, module) => ({ data: { name, description: `d-${name}` }, module });
    const client = {
      commands: new Map([['rps', mod('rps', 'games')], ['slots', mod('slots', 'economy')]]),
      commandCategories: new Map([['rps', 'games'], ['slots', 'games']]),
    };
    await gamesCmd.execute(it, client);
    assert.ok(JSON.stringify(calls[0].embeds[0].toJSON()).includes('/slots'));
    prisma.guildSettings = { findUnique: async () => ({ ...SETTINGS, guildId: 'guild-2', economyEnabled: false }), upsert: async () => ({}) };
    const it2 = { ...it, guildId: 'guild-2' };
    await gamesCmd.execute(it2, client);
    const text = JSON.stringify(calls[1].embeds[0].toJSON());
    assert.ok(text.includes('/rps'));
    assert.ok(!text.includes('/slots'), 'the casino is hidden while Economy is off');
  });
});

// ── payloads, exports, plugin ─────────────────────────────────────────────────

describe('payloads, exports and the plugin', () => {
  const charCount = (node) => {
    let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
    for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
    for (const o of node.options ?? []) n += charCount(o);
    return n;
  };
  const mods = {
    games: [gamesCmd, 60], numguess: [numguessCmd, 60], rps: [rpsCmd, 60], tictactoe: [tttCmd, 90],
    scrabble: [scrabbleCmd, 110], 'scrabble-play': [scrabblePlayCmd, 320], 'scrabble-help': [scrabbleHelpCmd, 70],
  };

  test('22. /scrabble-play has four subcommands and 15 column choices', () => {
    const json = buildCommandPayload(scrabblePlayCmd).options ? buildCommandPayload(scrabblePlayCmd) : null;
    assert.ok(json);
    assert.deepEqual(json.options.map((o) => o.name), ['word', 'pass', 'exchange', 'resign']);
    assert.ok(json.options.every((o) => o.type === 1));
    const col = json.options[0].options.find((o) => o.name === 'col');
    assert.equal(col.choices.length, 15);
    assert.equal(col.required, true);
    assert.ok(charCount(json) <= 320);
  });

  test('22. payload budgets, module and permission level of the seven commands', () => {
    for (const [name, [mod, budget]] of Object.entries(mods)) {
      const json = buildCommandPayload(mod);
      assert.equal(json.name, name);
      assert.ok(charCount(json) <= budget, `${name}: ${charCount(json)} > ${budget}`);
      assert.equal(mod.module, 'games', name);
      assert.equal(mod.permissionLevel, 0, name);
      assert.equal(typeof mod.execute, 'function');
      assert.equal(mod.allowInDMs, undefined, `${name} is guild-only`);
    }
  });

  test('23. the plugin registers exactly the persistent rack button and has no errors', async () => {
    const { plugins, errors } = await collectPlugins();
    assert.deepEqual(errors.filter((e) => e.includes('games-fixes')), []);
    const mine = plugins.find((p) => p.id === 'games-fixes');
    assert.ok(mine);
    assert.equal(mine.module, 'games');
    assert.equal(mine.components.length, 1);
    assert.equal(mine.components[0].type, 'button');
    assert.equal(mine.components[0].prefix, 'scrabble-rack');
    assert.equal(mine.components[0].handler, handleRackButton);
    assert.equal(plugin.hooks.length + plugin.jobs.length, 0);
  });

  test('source checks: no lazy registration, no absolute 60/120 s limits, no console', async () => {
    const { readFileSync } = await import('node:fs');
    const files = ['games', 'numguess', 'rps', 'scrabble-help', 'scrabble-play', 'scrabble', 'tictactoe']
      .map((n) => `src/commands/games/${n}.js`)
      .concat(['src/services/scrabbleService.js', 'src/plugins/games-fixes.js']);
    for (const f of files) {
      const text = readFileSync(resolve(ROOT, f), 'utf8');
      assert.doesNotMatch(text, /buttonHandlers/, `${f}: lazy handler registration`);
      assert.doesNotMatch(text, /\bconsole\./, `${f}: console`);
    }
    for (const f of ['tictactoe', 'numguess']) {
      assert.doesNotMatch(readFileSync(resolve(ROOT, `src/commands/games/${f}.js`), 'utf8'), /time: 60_000|time: 120_000/);
    }
  });
});
