// Scrabble rules engine, game registry, timers, rendering and the persistent rack button.
//
// Games are in memory and per shard (a guild's interactions always reach its own shard). A restart
// ends every game; the rack button of an old board then answers "no longer active".
//
// This module must stay importable without the database layer (tests import it directly), so it
// never imports settingsService: the command files capture the guild settings at game start and
// hand them to createGame().
//
// Importing it has no side effects: no timers, no file reads, no registration.
import { readFile, stat } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ActionRowBuilder, ButtonBuilder, ButtonStyle } from 'discord.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { createKeyedMutex } from '../utils/async.js';
import { guildEmbed } from '../utils/embedBuilder.js';
import { isInteractionGone } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { clampText } from '../utils/text.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const BOARD_SIZE = 15;
export const CENTER = 7;
export const RACK_SIZE = 7;
export const BINGO_BONUS = 50;
export const TURN_TIMEOUT_MS = 180_000; // 3 minutes
export const CHALLENGE_TIMEOUT_MS = 30_000;
export const MAX_SCORELESS_TURNS = 6;
export const MIN_BAG_FOR_EXCHANGE = 7;
export const COLS = 'ABCDEFGHIJKLMNO';
export const BLANK = '?';
export const MAX_WORD_LENGTH = 15;

export const TILE_POINTS = {
  A:1, B:3, C:3, D:2, E:1, F:4, G:2, H:4, I:1, J:8, K:5,
  L:1, M:3, N:1, O:1, P:3, Q:10, R:1, S:1, T:1, U:1,
  V:4, W:4, X:8, Y:4, Z:10,
  '?':0,
};

// R280: 98 lettered tiles + 2 blanks = 100.
export const TILE_COUNTS = {
  A:9, B:2, C:2, D:4, E:12, F:2, G:3, H:2, I:9, J:1, K:1,
  L:4, M:2, N:6, O:8, P:2, Q:1, R:6, S:4, T:6, U:4,
  V:2, W:2, X:1, Y:2, Z:1,
  '?':2,
};

export const DICTIONARY_PATH = resolve(dirname(fileURLToPath(import.meta.url)), '../../assets/dictionaries/words.txt');
export const DICTIONARY_MAX_BYTES = 8 * 1024 * 1024;

/** channelId → Game (running games only). */
export const activeGames = new Map();
/** channelId → challengeId (the /scrabble start interaction id) while a challenge is pending. */
export const pendingChallenges = new Map();
/** Serialises every move, pass, exchange and result post of one channel. */
export const gameMutex = createKeyedMutex();

// ── Board multiplier positions (0-indexed [row, col]) ─────────────────────────

const TW_SQUARES = [[0,0],[0,7],[0,14],[7,0],[7,14],[14,0],[14,7],[14,14]];
const DW_SQUARES = [
  [1,1],[2,2],[3,3],[4,4],[10,10],[11,11],[12,12],[13,13],
  [1,13],[2,12],[3,11],[4,10],[10,4],[11,3],[12,2],[13,1],
];
const TL_SQUARES = [
  [1,5],[1,9],[5,1],[5,5],[5,9],[5,13],
  [9,1],[9,5],[9,9],[9,13],[13,5],[13,9],
];
const DL_SQUARES = [
  [0,3],[0,11],[2,6],[2,8],[3,0],[3,7],[3,14],
  [6,2],[6,6],[6,8],[6,12],[7,3],[7,11],
  [8,2],[8,6],[8,8],[8,12],[11,0],[11,7],[11,14],
  [12,6],[12,8],[14,3],[14,11],
];

// Board cell schema:
//   null                              empty, no premium
//   'TW'|'DW'|'TL'|'DL'|'CENTER'      empty premium square
//   { letter, points, blank }         a played tile (the premium is consumed; a blank has points 0)
export function createBoard() {
  const b = Array.from({ length: BOARD_SIZE }, () => Array(BOARD_SIZE).fill(null));
  for (const [r, c] of TW_SQUARES) b[r][c] = 'TW';
  for (const [r, c] of DW_SQUARES) b[r][c] = 'DW';
  for (const [r, c] of TL_SQUARES) b[r][c] = 'TL';
  for (const [r, c] of DL_SQUARES) b[r][c] = 'DL';
  b[CENTER][CENTER] = 'CENTER';
  return b;
}

const isTile = (cell) => cell !== null && typeof cell === 'object';

// ── Pure helpers ──────────────────────────────────────────────────────────────

/** Fisher–Yates, in place. Returns `arr`. */
export function shuffle(arr, rng = Math.random) {
  for (let i = arr.length - 1; i > 0; i--) {
    const j = Math.floor(rng() * (i + 1));
    [arr[i], arr[j]] = [arr[j], arr[i]];
  }
  return arr;
}

export function createTileBag(rng = Math.random) {
  const bag = [];
  for (const [letter, count] of Object.entries(TILE_COUNTS)) {
    for (let i = 0; i < count; i++) bag.push({ letter, points: TILE_POINTS[letter] });
  }
  return shuffle(bag, rng);
}

export function drawTiles(bag, n) {
  return bag.splice(0, Math.min(n, bag.length));
}

export function rackValue(rack) {
  return rack.reduce((sum, t) => sum + t.points, 0);
}

const WORD_FORMAT_ERROR = 'Use letters A–Z only. Put `?` before a letter to play a blank as that letter (e.g. `QU?IZ`).';

/**
 * Parses the `word` option. `?` before a letter plays a blank as that letter.
 * @param {unknown} raw
 * @returns {{ ok: true, tiles: Array<{ letter: string, blank: boolean }> } | { ok: false, error: string }}
 */
export function parseWordInput(raw) {
  const tiles = [];
  let pendingBlank = false;
  for (const ch of String(raw ?? '').trim()) {
    if (ch === BLANK) {
      if (pendingBlank) return { ok: false, error: WORD_FORMAT_ERROR };
      pendingBlank = true;
    } else if (/^[A-Za-z]$/.test(ch)) {
      tiles.push({ letter: ch.toUpperCase(), blank: pendingBlank });
      pendingBlank = false;
    } else {
      return { ok: false, error: WORD_FORMAT_ERROR };
    }
  }
  if (pendingBlank || tiles.length === 0) return { ok: false, error: WORD_FORMAT_ERROR };
  if (tiles.length > MAX_WORD_LENGTH) return { ok: false, error: `A word can be at most ${MAX_WORD_LENGTH} letters.` };
  return { ok: true, tiles };
}

/**
 * Parses the `tiles` option of an exchange (letters A–Z, `?` = blank).
 * @param {unknown} raw
 * @returns {{ ok: true, letters: string[] } | { ok: false, error: string }}
 */
export function parseTilesInput(raw) {
  const s = String(raw ?? '').trim();
  if (!/^[A-Za-z?]{1,7}$/.test(s)) {
    return { ok: false, error: 'Use letters A–Z and `?` (blank), up to 7 tiles.' };
  }
  return { ok: true, letters: [...s.toUpperCase()] };
}

export function currentPlayerId(game) {
  return game.turnOrder[game.currentTurn];
}

export function otherPlayerId(game, userId) {
  return game.turnOrder[0] === userId ? game.turnOrder[1] : game.turnOrder[0];
}

export function describeWords(words) {
  return clampText(words.map((w) => `**${w.text}** (${w.score})`).join(' + '), 1000);
}

export function formatRack(rack) {
  if (!rack.length) return '_(empty)_';
  return rack.map((t) => (t.letter === BLANK ? '**?** blank' : `**${t.letter}** ${t.points}`)).join(' · ');
}

// ── Game lifecycle and registry ───────────────────────────────────────────────

/**
 * Builds a game. It does NOT register it and does NOT arm a timer (see registerGame).
 * @param {{ id: string, guildId: string, channelId: string,
 *   players: Array<{ id: string, name: string }>, dictionary?: Set<string>|null,
 *   settings?: object|null, rng?: () => number }} opts
 */
export function createGame({ id, guildId, channelId, players, dictionary = null, settings = null, rng = Math.random }) {
  const bag = createTileBag(rng);
  const byId = {};
  for (const p of players) {
    byId[p.id] = { id: p.id, name: p.name, rack: drawTiles(bag, RACK_SIZE), score: 0 };
  }
  return {
    id,
    guildId,
    channelId,
    settings: settings ? Object.freeze({ ...settings }) : null,
    dictionary: dictionary instanceof Set ? dictionary : null,
    rng,
    board: createBoard(),
    bag,
    players: byId,
    turnOrder: [players[0].id, players[1].id],
    currentTurn: 0,
    turnSeq: 0,
    scorelessTurns: 0,
    placedCount: 0,
    moves: 0,
    timer: null,
    ended: false,
    result: null,
    lastInteraction: null,
    lastBoard: null,
  };
}

export function isChannelBusy(channelId) {
  return activeGames.has(channelId) || pendingChallenges.has(channelId);
}

/** Reserves a channel for a pending challenge. Returns false when it is busy. */
export function reserveChannel(channelId, challengeId) {
  if (isChannelBusy(channelId)) return false;
  pendingChallenges.set(channelId, challengeId);
  return true;
}

/** Releases a reservation, but only the caller's own. */
export function releaseChannel(channelId, challengeId) {
  if (pendingChallenges.get(channelId) === challengeId) pendingChallenges.delete(channelId);
}

/**
 * Turns a reservation into the running game and arms its first turn timer in one synchronous step,
 * so a registered game always has a timer (R087). Returns false when the channel is taken.
 */
export function registerGame(game) {
  if (activeGames.has(game.channelId)) return false;
  const reservedBy = pendingChallenges.get(game.channelId);
  if (reservedBy !== undefined && reservedBy !== game.id) return false;
  activeGames.set(game.channelId, game);
  pendingChallenges.delete(game.channelId);
  armTurnTimer(game);
  return true;
}

export function getGame(channelId) {
  const g = activeGames.get(channelId);
  return g && !g.ended ? g : null;
}

export function findGameById(gameId) {
  for (const g of activeGames.values()) {
    if (g.id === gameId && !g.ended) return g;
  }
  return null;
}

function computeResult(game, reason, { loserId, moverId }) {
  const [a, b] = game.turnOrder;
  const adjustments = {};
  let winnerId = null;
  let loser = null;

  if (reason === 'out' && moverId) {
    const opponentId = otherPlayerId(game, moverId);
    const value = rackValue(game.players[opponentId].rack);
    game.players[moverId].score += value;
    game.players[opponentId].score -= value;
    adjustments[moverId] = value;
    adjustments[opponentId] = -value;
  } else if (reason === 'scoreless') {
    for (const id of [a, b]) {
      const value = rackValue(game.players[id].rack);
      game.players[id].score -= value;
      adjustments[id] = -value;
    }
  }

  if (reason === 'resign' || reason === 'timeout') {
    loser = loserId ?? null;
    winnerId = loser ? otherPlayerId(game, loser) : null;
  } else if (reason !== 'aborted') {
    const sa = game.players[a].score;
    const sb = game.players[b].score;
    if (sa !== sb) {
      winnerId = sa > sb ? a : b;
      loser = otherPlayerId(game, winnerId);
    }
  }

  return {
    reason,
    winnerId,
    loserId: loser,
    moverId: reason === 'out' ? moverId : null,
    adjustments,
    scores: { [a]: game.players[a].score, [b]: game.players[b].score },
  };
}

/**
 * Ends a game. Synchronous and idempotent. The registry entry is removed only when it is THIS game
 * (instance check), so a stale timer can never end a newer game in the same channel (R021, R167).
 * @param {object} game
 * @param {'out'|'scoreless'|'resign'|'timeout'|'aborted'} reason
 * @param {{ loserId?: string|null, moverId?: string|null }} [opts]
 */
export function finishGame(game, reason, { loserId = null, moverId = null } = {}) {
  if (game.ended) return game.result;
  game.ended = true;
  clearTurnTimer(game);
  if (activeGames.get(game.channelId) === game) activeGames.delete(game.channelId);
  game.result = computeResult(game, reason, { loserId, moverId });
  return game.result;
}

// ── Timers bound to a game (R167) ─────────────────────────────────────────────

export function clearTurnTimer(game) {
  if (game.timer) clearTimeout(game.timer);
  game.timer = null;
}

export function armTurnTimer(game) {
  clearTurnTimer(game);
  const seq = game.turnSeq;
  game.timer = setTimeout(() => { handleTurnTimeout(game, seq).catch(() => {}); }, TURN_TIMEOUT_MS);
  game.timer.unref?.();
}

/**
 * The turn timer's callback. The decision and the end of the game happen synchronously, before any
 * await; only the result post waits for the channel mutex. Never throws.
 */
export async function handleTurnTimeout(game, seq) {
  try {
    if (game.ended || game.turnSeq !== seq || activeGames.get(game.channelId) !== game) return;
    const result = finishGame(game, 'timeout', { loserId: currentPlayerId(game) });
    await gameMutex.run(game.channelId, () => postResult(game, game.lastInteraction, result));
  } catch (err) {
    logger.error('Scrabble turn timeout failed', err);
  }
}

function advanceTurn(game) {
  game.currentTurn = 1 - game.currentTurn;
  game.turnSeq++;
  armTurnTimer(game);
}

// ── Rules engine ──────────────────────────────────────────────────────────────

const fail = (error) => ({ ok: false, error });

function notYourTurn(game) {
  const name = game.players[currentPlayerId(game)]?.name ?? 'your opponent';
  return fail(`It's not your turn — it's **${name}**'s turn.`);
}

const square = (r, c) => `${COLS[c]}${r + 1}`;

/**
 * Validates and scores a play WITHOUT touching the game. Checks run in a fixed order and the first
 * error wins (turn, range, squares, new tile, rack, placement, words, dictionary).
 * @param {object} game
 * @param {string} userId
 * @param {Array<{ letter: string, blank: boolean }>} tiles
 * @param {number} row0 0-based
 * @param {number} col0 0-based
 * @param {'h'|'v'} dir
 */
export function computePlay(game, userId, tiles, row0, col0, dir) {
  // 1. Turn
  if (currentPlayerId(game) !== userId) return notYourTurn(game);

  // 2. Range
  const inRange = (n) => Number.isInteger(n) && n >= 0 && n < BOARD_SIZE;
  if (!inRange(row0) || !inRange(col0) || (dir !== 'h' && dir !== 'v') || !Array.isArray(tiles) || tiles.length === 0) {
    return fail('Invalid position.');
  }
  const dr = dir === 'v' ? 1 : 0;
  const dc = dir === 'h' ? 1 : 0;
  if (row0 + dr * (tiles.length - 1) >= BOARD_SIZE || col0 + dc * (tiles.length - 1) >= BOARD_SIZE) {
    return fail('That word runs off the board.');
  }

  // 3. Squares
  const board = game.board;
  const placements = [];
  for (let i = 0; i < tiles.length; i++) {
    const r = row0 + dr * i;
    const c = col0 + dc * i;
    const cell = board[r][c];
    const t = tiles[i];
    if (isTile(cell)) {
      if (t.blank) {
        return fail(`Square **${square(r, c)}** already has a tile — only new tiles can be blanks (remove the \`?\`).`);
      }
      if (cell.letter !== t.letter) {
        return fail(`Square **${square(r, c)}** already has **${cell.letter}**, not **${t.letter}**.`);
      }
    } else {
      placements.push({
        r, c,
        letter: t.letter,
        blank: Boolean(t.blank),
        points: t.blank ? 0 : (TILE_POINTS[t.letter] ?? 0),
        premium: cell,
      });
    }
  }

  // 4. At least one new tile
  if (placements.length === 0) return fail('You must place at least one new tile.');

  // 5. Rack (multiset, on a copy)
  const rack = game.players[userId].rack.map((t) => t.letter);
  for (const p of placements) {
    if (p.blank) {
      const idx = rack.indexOf(BLANK);
      if (idx === -1) return fail("You don't have a blank tile.");
      rack.splice(idx, 1);
    } else {
      const idx = rack.indexOf(p.letter);
      if (idx === -1) {
        return rack.includes(BLANK)
          ? fail(`You don't have **${p.letter}**. To play your blank as ${p.letter}, type \`?${p.letter}\`.`)
          : fail(`You don't have the tile **${p.letter}** in your rack.`);
      }
      rack.splice(idx, 1);
    }
  }

  // 6. Placement
  if (game.placedCount === 0) {
    if (!placements.some((p) => p.r === CENTER && p.c === CENTER)) {
      return fail('The first word must cover the centre square (**H8**).');
    }
  } else {
    const touches = placements.some(({ r, c }) => [[r - 1, c], [r + 1, c], [r, c - 1], [r, c + 1]].some(([nr, nc]) =>
      nr >= 0 && nr < BOARD_SIZE && nc >= 0 && nc < BOARD_SIZE && isTile(board[nr][nc])));
    const passesThrough = placements.length < tiles.length;
    if (!touches && !passesThrough) return fail('Your word must touch or pass through a tile already on the board.');
  }

  // 7. Formed words: the whole run through the declared span (R168) plus every cross-word (R047)
  const fresh = new Map(placements.map((p) => [p.r * BOARD_SIZE + p.c, p]));
  const at = (r, c) => {
    if (r < 0 || r >= BOARD_SIZE || c < 0 || c >= BOARD_SIZE) return null;
    return fresh.get(r * BOARD_SIZE + c) ?? (isTile(board[r][c]) ? board[r][c] : null);
  };
  const run = (r, c, rowStep, colStep) => {
    let sr = r;
    let sc = c;
    while (at(sr - rowStep, sc - colStep)) { sr -= rowStep; sc -= colStep; }
    const cells = [];
    while (at(sr, sc)) { cells.push([sr, sc]); sr += rowStep; sc += colStep; }
    return cells;
  };

  const runs = [];
  const main = run(row0, col0, dr, dc);
  if (main.length >= 2) runs.push(main);
  for (const p of placements) {
    const cross = run(p.r, p.c, dc, dr);
    if (cross.length >= 2) runs.push(cross);
  }
  if (runs.length === 0) return fail('Your tiles must form a word of at least 2 letters.');

  // 8. Score: only NEW tiles use their premium square
  const words = runs.map((cells) => {
    let letters = 0;
    let multiplier = 1;
    let text = '';
    for (const [r, c] of cells) {
      const n = fresh.get(r * BOARD_SIZE + c);
      if (n) {
        let v = n.points;
        if (n.premium === 'DL') v *= 2;
        else if (n.premium === 'TL') v *= 3;
        else if (n.premium === 'DW' || n.premium === 'CENTER') multiplier *= 2;
        else if (n.premium === 'TW') multiplier *= 3;
        letters += v;
        text += n.letter;
      } else {
        const t = board[r][c];
        letters += t.points;
        text += t.letter;
      }
    }
    return { text, score: letters * multiplier };
  });

  // 9. Dictionary (R046)
  if (game.dictionary instanceof Set) {
    const invalid = words.filter((w) => !game.dictionary.has(w.text));
    if (invalid.length) return fail(`Not in the word list: ${invalid.map((w) => `**${w.text}**`).join(', ')}.`);
  }

  // 10. Total
  const bingo = placements.length === RACK_SIZE;
  const total = words.reduce((sum, w) => sum + w.score, 0) + (bingo ? BINGO_BONUS : 0);
  return { ok: true, placements, words, total, bingo };
}

function removeFromRack(rack, letter) {
  const idx = rack.findIndex((t) => t.letter === letter);
  if (idx !== -1) rack.splice(idx, 1);
}

/**
 * Validates and commits a word play SYNCHRONOUSLY, with no await between the two (R083).
 * @returns {{ ok: false, error: string } | { ok: true, words: Array<{text: string, score: number}>,
 *   total: number, bingo: boolean, placements: object[], gameOver: object|null }}
 */
export function playWord(game, userId, raw, row0, col0, dir) {
  if (game.ended) return fail('This game has already ended.');
  if (currentPlayerId(game) !== userId) return notYourTurn(game);
  const parsed = parseWordInput(raw);
  if (!parsed.ok) return parsed;
  const res = computePlay(game, userId, parsed.tiles, row0, col0, dir);
  if (!res.ok) return res;

  const player = game.players[userId];
  for (const p of res.placements) {
    game.board[p.r][p.c] = { letter: p.letter, points: p.points, blank: p.blank };
    removeFromRack(player.rack, p.blank ? BLANK : p.letter);
  }
  player.score += res.total;
  game.placedCount += res.placements.length;
  game.moves++;
  game.scorelessTurns = res.total > 0 ? 0 : game.scorelessTurns + 1;
  player.rack.push(...drawTiles(game.bag, RACK_SIZE - player.rack.length));

  let gameOver = null;
  if (game.bag.length === 0 && player.rack.length === 0) gameOver = finishGame(game, 'out', { moverId: userId });
  else if (game.scorelessTurns >= MAX_SCORELESS_TURNS) gameOver = finishGame(game, 'scoreless');
  else advanceTurn(game);

  return { ok: true, words: res.words, total: res.total, bingo: res.bingo, placements: res.placements, gameOver };
}

/** @returns {{ ok: boolean, error?: string, gameOver: object|null }} */
export function passTurn(game, userId) {
  if (game.ended) return { ...fail('This game has already ended.'), gameOver: null };
  if (currentPlayerId(game) !== userId) return { ...notYourTurn(game), gameOver: null };
  game.scorelessTurns++;
  game.moves++;
  let gameOver = null;
  if (game.scorelessTurns >= MAX_SCORELESS_TURNS) gameOver = finishGame(game, 'scoreless');
  else advanceTurn(game);
  return { ok: true, gameOver };
}

/** @returns {{ ok: boolean, error?: string, count?: number, gameOver: object|null }} */
export function exchangeTiles(game, userId, raw) {
  if (game.ended) return { ...fail('This game has already ended.'), gameOver: null };
  if (currentPlayerId(game) !== userId) return { ...notYourTurn(game), gameOver: null };
  if (game.bag.length < MIN_BAG_FOR_EXCHANGE) {
    return {
      ...fail(`You can only exchange when the bag has at least ${MIN_BAG_FOR_EXCHANGE} tiles (it has **${game.bag.length}**).`),
      gameOver: null,
    };
  }
  const parsed = parseTilesInput(raw);
  if (!parsed.ok) return { ...parsed, gameOver: null };

  const player = game.players[userId];
  const left = player.rack.map((t) => t.letter);
  for (const letter of parsed.letters) {
    const idx = left.indexOf(letter);
    if (idx === -1) {
      const shown = letter === BLANK ? '**?** (blank)' : `**${letter}**`;
      return { ...fail(`You don't have the tile ${shown} in your rack.`), gameOver: null };
    }
    left.splice(idx, 1);
  }

  // Draw first, then put the old tiles back, so you can never draw your own tiles again.
  const n = parsed.letters.length;
  const drawn = drawTiles(game.bag, n);
  const removed = [];
  for (const letter of parsed.letters) {
    const idx = player.rack.findIndex((t) => t.letter === letter);
    removed.push(...player.rack.splice(idx, 1));
  }
  player.rack.push(...drawn);
  game.bag.push(...removed);
  shuffle(game.bag, game.rng);

  game.scorelessTurns++;
  game.moves++;
  let gameOver = null;
  if (game.scorelessTurns >= MAX_SCORELESS_TURNS) gameOver = finishGame(game, 'scoreless');
  else advanceTurn(game);
  return { ok: true, count: n, gameOver };
}

/** Allowed at any time, not only on your turn. */
export function resignGame(game, userId) {
  return finishGame(game, 'resign', { loserId: userId });
}

// ── Rendering ─────────────────────────────────────────────────────────────────

const CELL_LABEL = { TW: 'TW', DW: 'DW', TL: 'TL', DL: 'DL', CENTER: '★ ' };

/** The board as a code block (a blank tile shows as a lowercase letter). Well under 1,000 characters. */
export function renderBoard(board) {
  const header = '    ' + COLS.split('').map((c) => c.padEnd(2, ' ')).join(' ');
  const rows = board.map((row, r) => {
    const num = String(r + 1).padStart(2, ' ');
    const cells = row.map((cell) => {
      if (cell === null) return '· ';
      if (typeof cell === 'string') return CELL_LABEL[cell] ?? '· ';
      return (cell.blank ? cell.letter.toLowerCase() : cell.letter) + ' ';
    });
    return `${num}  ${cells.join(' ')}`;
  });
  return '```\n' + header + '\n' + rows.join('\n') + '\n```';
}

/**
 * The board embed. `finished` shows "Game over" instead of whose turn it is (R238).
 * @param {object} game
 * @param {{ finished?: boolean }} [opts]
 */
export function buildGameEmbed(game, { finished = false } = {}) {
  const [a, b] = game.turnOrder;
  const p1 = game.players[a];
  const p2 = game.players[b];
  const embed = guildEmbed(game.settings, '🔤 Scrabble', renderBoard(game.board))
    .addFields(
      { name: p1.name || 'Player 1', value: `**${p1.score}** pts`, inline: true },
      { name: p2.name || 'Player 2', value: `**${p2.score}** pts`, inline: true },
      { name: 'Tiles in bag', value: String(game.bag.length), inline: true },
      finished
        ? { name: 'Status', value: 'Game over' }
        : { name: 'Turn', value: `<@${currentPlayerId(game)}> · 3 min to play` },
    )
    .setFooter({
      text: 'TW/DW = triple/double word · TL/DL = triple/double letter · ★ = centre (DW) · '
        + `lowercase = blank · Word check: ${game.dictionary ? 'on' : 'off'}`,
    });
  return embed;
}

/** The final scores and the winner. */
export function buildResultEmbed(game, result) {
  const [a, b] = game.turnOrder;
  const lines = [];
  for (const id of [a, b]) {
    const adj = result.adjustments?.[id] ?? 0;
    lines.push(`**${game.players[id].name}**: ${result.scores[id]} pts${adj ? ` (${adj > 0 ? '+' : ''}${adj} from racks)` : ''}`);
  }
  lines.push('');
  const nameOf = (id) => game.players[id]?.name ?? 'Someone';
  if (result.reason === 'out') {
    lines.push(`**${nameOf(result.moverId ?? result.winnerId)}** used their last tile with the bag empty.`);
  } else if (result.reason === 'scoreless') {
    lines.push('Six scoreless turns in a row — remaining tiles were deducted.');
  } else if (result.reason === 'resign') {
    lines.push(`**${nameOf(result.loserId)}** resigned.`);
  } else if (result.reason === 'timeout') {
    lines.push(`**${nameOf(result.loserId)}** didn't play within 3 minutes.`);
  }
  lines.push(result.winnerId ? `🏆 Winner: **${nameOf(result.winnerId)}**!` : "🤝 It's a tie!");
  return guildEmbed(game.settings, '🏁 Scrabble — Game Over', lines.join('\n'));
}

/** One row with the persistent "View My Tiles" button, bound to a game id. */
export function rackRow(gameId) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder()
      .setCustomId(buildCustomId('scrabble-rack', gameId))
      .setLabel('🎒 View My Tiles')
      .setStyle(ButtonStyle.Secondary),
  );
}

function logPostError(label, err) {
  if (isInteractionGone(err) || err?.code === 10008) logger.debug(`Scrabble ${label}: ${err?.message ?? err}`);
  else logger.warn(`Scrabble ${label} failed`, err);
}

/** Strips the old board's button, keeping its embed. Never throws. */
async function stripOldBoard(game) {
  const last = game.lastBoard;
  if (!last) return;
  game.lastBoard = null;
  try {
    await last.interaction.editReply({ message: last.messageId, components: [] });
  } catch (err) {
    logPostError('old board edit', err);
  }
}

/**
 * Posts the new public board (with a mention of the next player) and strips the previous board's
 * button. Never throws; returns whether the post succeeded.
 * @param {object} game
 * @param {import('discord.js').RepliableInteraction} interaction a replied or deferred interaction (followUp is used)
 * @param {{ content: string, mentionIds?: string[] }} opts
 */
export async function postBoard(game, interaction, { content, mentionIds = [] }) {
  try {
    await stripOldBoard(game);
    const msg = await interaction.followUp({
      content: clampText(content, 2000),
      embeds: [buildGameEmbed(game)],
      components: [rackRow(game.id)],
      allowedMentions: { users: mentionIds },
    });
    game.lastBoard = { interaction, messageId: msg.id };
    game.lastInteraction = interaction;
    return true;
  } catch (err) {
    logPostError('board post', err);
    return false;
  }
}

/**
 * Posts the final board and the result (R166, R238). Never throws; nothing is posted for an aborted game.
 */
export async function postResult(game, interaction, result) {
  try {
    if (!result || result.reason === 'aborted' || !interaction) return;
    await stripOldBoard(game);
    const [a, b] = game.turnOrder;
    await interaction.followUp({
      content: `🏁 <@${a}> <@${b}>`,
      embeds: [buildGameEmbed(game, { finished: true }), buildResultEmbed(game, result)],
      allowedMentions: { users: [a, b] },
    });
  } catch (err) {
    logPostError('result post', err);
  }
}

/**
 * The persistent "View My Tiles" button (router-registered plugin component). Never throws.
 * `scrabble-rack:{gameId}`; the bare legacy `scrabble-rack` of older boards is resolved by channel.
 */
export async function handleRackButton(interaction) {
  try {
    const { params } = parseCustomId(interaction.customId);
    const game = params[0] ? findGameById(params[0]) : getGame(interaction.channelId);
    if (!game) {
      await interaction.reply({
        content: 'This Scrabble game is no longer active (it ended, or the bot restarted). Start a new one with `/scrabble start`.',
        ephemeral: true,
      });
      return;
    }
    const p = game.players[interaction.user.id];
    if (!p) {
      await interaction.reply({ content: "You're not a player in this game.", ephemeral: true });
      return;
    }
    const turn = currentPlayerId(game) === p.id
      ? "✅ It's your turn — use `/scrabble word`."
      : `⏳ Waiting for **${game.players[currentPlayerId(game)].name}**.`;
    await interaction.reply({
      content: [
        `🎒 Your tiles: ${formatRack(p.rack)}`,
        `📊 Your score: **${p.score}** pts · 👜 Bag: **${game.bag.length}**`,
        turn,
      ].join('\n'),
      ephemeral: true,
    });
  } catch (err) {
    if (isInteractionGone(err)) logger.debug('Scrabble rack button: interaction gone');
    else logger.error('Scrabble rack button failed', err);
  }
}

// ── Optional dictionary (R046) ────────────────────────────────────────────────

/** @type {{ state: 'unloaded'|'loading'|'loaded'|'missing'|'error', size: number }} */
let dictState = { state: 'unloaded', size: 0 };
/** @type {Set<string>|null} */
let dictSet = null;
/** @type {Promise<Set<string>|null>|null} */
let dictPromise = null;

/** Pure: one word per line; trimmed; blank lines and `#` lines ignored; keeps `^[A-Z]{2,15}$`. */
export function parseDictionary(text) {
  const set = new Set();
  for (const line of String(text ?? '').split(/\r?\n/)) {
    const w = line.trim().toUpperCase();
    if (!w || w.startsWith('#')) continue;
    if (/^[A-Z]{2,15}$/.test(w)) set.add(w);
  }
  return set;
}

async function readDictionary() {
  let info;
  try {
    info = await stat(DICTIONARY_PATH);
  } catch (err) {
    if (err?.code === 'ENOENT') {
      dictState = { state: 'missing', size: 0 };
      logger.info('Scrabble dictionary not found at assets/dictionaries/words.txt; word checking is off.');
    } else {
      dictState = { state: 'error', size: 0 };
      logger.warn('Scrabble dictionary could not be read; word checking is off.', err);
    }
    return null;
  }
  if (info.size > DICTIONARY_MAX_BYTES) {
    dictState = { state: 'error', size: 0 };
    logger.warn(`Scrabble dictionary is ${info.size} bytes (limit ${DICTIONARY_MAX_BYTES}); word checking is off.`);
    return null;
  }
  const set = parseDictionary(await readFile(DICTIONARY_PATH, 'utf8'));
  if (set.size === 0) {
    dictState = { state: 'error', size: 0 };
    logger.warn('Scrabble dictionary is empty; word checking is off.');
    return null;
  }
  dictSet = set;
  dictState = { state: 'loaded', size: set.size };
  logger.info(`Scrabble dictionary loaded: ${set.size} words`);
  return set;
}

/**
 * Loads the dictionary once per process (memoised). Never rejects; resolves null when there is none.
 * @returns {Promise<Set<string>|null>}
 */
export function loadDictionary() {
  if (dictPromise) return dictPromise;
  dictState = { state: 'loading', size: 0 };
  dictPromise = readDictionary().catch((err) => {
    dictState = { state: 'error', size: 0 };
    logger.warn('Scrabble dictionary load failed; word checking is off.', err);
    return null;
  });
  return dictPromise;
}

/** The loaded dictionary, or null (synchronous). */
export function getDictionary() {
  return dictState.state === 'loaded' ? dictSet : null;
}

export function dictionaryStatus() {
  return { state: dictState.state, size: dictState.size };
}

/** Test hooks only: reset the memoised dictionary, or install one. */
export const __testing = {
  reset() {
    dictState = { state: 'unloaded', size: 0 };
    dictSet = null;
    dictPromise = null;
  },
  setDictionary(set) {
    dictSet = set;
    dictState = { state: 'loaded', size: set.size };
    dictPromise = Promise.resolve(set);
  },
};
