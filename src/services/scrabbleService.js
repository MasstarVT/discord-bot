import { EmbedBuilder, ActionRowBuilder, ButtonBuilder, ButtonStyle } from 'discord.js';
import { COLORS } from '../config/constants.js';
import { warnEmbed, successEmbed } from '../utils/embedBuilder.js';

// ── Tile data ─────────────────────────────────────────────────────────────────

export const TILE_POINTS = {
  A:1, B:3, C:3, D:2, E:1, F:4, G:2, H:4, I:1, J:8, K:5,
  L:1, M:3, N:1, O:1, P:3, Q:10, R:1, S:1, T:1, U:1,
  V:4, W:4, X:8, Y:4, Z:10,
};

const TILE_COUNTS = {
  A:9, B:2, C:2, D:4, E:12, F:2, G:3, H:2, I:9, J:1, K:1,
  L:4, M:2, N:6, O:8, P:2, Q:1, R:6, S:4, T:6, U:4,
  V:2, W:2, X:1, Y:2, Z:1,
};

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

const COLS = ['A','B','C','D','E','F','G','H','I','J','K','L','M','N','O'];

// ── Board cell schema ─────────────────────────────────────────────────────────
// null               → empty, no multiplier
// 'TW'|'DW'|'TL'|'DL'|'CENTER' → empty multiplier square
// { letter, points } → played tile (multiplier consumed)

export function createBoard() {
  const b = Array.from({ length: 15 }, () => Array(15).fill(null));
  for (const [r, c] of TW_SQUARES) b[r][c] = 'TW';
  for (const [r, c] of DW_SQUARES) b[r][c] = 'DW';
  for (const [r, c] of TL_SQUARES) b[r][c] = 'TL';
  for (const [r, c] of DL_SQUARES) b[r][c] = 'DL';
  b[7][7] = 'CENTER';
  return b;
}

// ── Tile bag ──────────────────────────────────────────────────────────────────

export function createTileBag() {
  const bag = [];
  for (const [letter, count] of Object.entries(TILE_COUNTS))
    for (let i = 0; i < count; i++) bag.push({ letter, points: TILE_POINTS[letter] });
  for (let i = bag.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [bag[i], bag[j]] = [bag[j], bag[i]];
  }
  return bag;
}

export function drawTiles(bag, n) {
  return bag.splice(0, Math.min(n, bag.length));
}

// ── Board renderer ────────────────────────────────────────────────────────────

const CELL_LABEL = { TW: 'TW', DW: 'DW', TL: 'TL', DL: 'DL', CENTER: '★ ' };

export function renderBoard(board) {
  const header = '    ' + COLS.map(c => c.padEnd(2, ' ')).join(' ');
  const rows = board.map((row, r) => {
    const num   = String(r + 1).padStart(2, ' ');
    const cells = row.map(cell => {
      if (cell === null)            return '· ';
      if (typeof cell === 'string') return CELL_LABEL[cell] ?? '· ';
      return cell.letter + ' ';
    });
    return `${num}  ${cells.join(' ')}`;
  });
  return '```\n' + header + '\n' + rows.join('\n') + '\n```';
}

// ── Active game state ─────────────────────────────────────────────────────────
// Keyed by channelId. Shape:
// {
//   board, bag, players: { [userId]: { user, rack, score } },
//   turnOrder: [uid1, uid2], currentTurn: 0|1, firstTurn: bool,
//   turnTimer: TimeoutId, gameMessage: Message|null, channelId: string
// }

export const activeGames = new Map();

export const TURN_TIMEOUT_MS = 180_000; // 3 minutes

export function endGame(channelId) {
  const game = activeGames.get(channelId);
  if (!game) return;
  clearTimeout(game.turnTimer);
  activeGames.delete(channelId);
}

export function startTurnTimer(channelId, channel) {
  return setTimeout(async () => {
    const game = activeGames.get(channelId);
    if (!game) return;
    const who = game.players[game.turnOrder[game.currentTurn]].user;
    // Disable the View My Tiles button on the board message
    if (game.gameMessage) {
      await game.gameMessage.edit({ components: [] }).catch(() => {});
    }
    await channel
      .send({ embeds: [warnEmbed('Scrabble — Timed Out', `<@${who.id}> didn't play within 3 minutes. Game over!`)] })
      .catch(() => {});
    endGame(channelId);
  }, TURN_TIMEOUT_MS);
}

// ── Shared UI components ──────────────────────────────────────────────────────

export const RACK_ROW = new ActionRowBuilder().addComponents(
  new ButtonBuilder()
    .setCustomId('scrabble-rack')
    .setLabel('🎒 View My Tiles')
    .setStyle(ButtonStyle.Secondary),
);

export function buildGameEmbed(game) {
  const [uid1, uid2] = game.turnOrder;
  const p1      = game.players[uid1];
  const p2      = game.players[uid2];
  const current = game.players[game.turnOrder[game.currentTurn]].user;

  return new EmbedBuilder()
    .setColor(COLORS.INFO)
    .setTitle('🔤 Scrabble')
    .setDescription(renderBoard(game.board))
    .addFields(
      { name: p1.user.username, value: `**${p1.score}** pts`, inline: true },
      { name: p2.user.username, value: `**${p2.score}** pts`, inline: true },
      { name: 'Tiles in bag',   value: String(game.bag.length), inline: true },
      { name: "It's your turn", value: `<@${current.id}>` },
    )
    .setFooter({ text: 'TW=Triple Word  DW=Double Word  TL=Triple Letter  DL=Double Letter  ★=Center(DW)' })
    .setTimestamp();
}

// ── Move validation ───────────────────────────────────────────────────────────
// Returns { valid, error?, newTiles? }
// newTiles = positions where a new letter is placed (cell was empty/multiplier)

export function validatePlay(game, userId, word, row0, col0, dir) {
  const W = word.toUpperCase();

  if (game.turnOrder[game.currentTurn] !== userId)
    return { valid: false, error: "It's not your turn!" };

  const dr = dir === 'v' ? 1 : 0;
  const dc = dir === 'h' ? 1 : 0;

  // Out-of-bounds check
  const endR = row0 + dr * (W.length - 1);
  const endC = col0 + dc * (W.length - 1);
  if (endR > 14 || endC > 14)
    return { valid: false, error: 'Word extends off the board.' };

  const board       = game.board;
  const newTiles    = [];
  const rackNeeded  = [];

  for (let i = 0; i < W.length; i++) {
    const r      = row0 + dr * i;
    const c      = col0 + dc * i;
    const letter = W[i];
    const cell   = board[r][c];

    if (cell !== null && typeof cell === 'object') {
      // Existing tile — must match
      if (cell.letter !== letter)
        return {
          valid: false,
          error: `Cell ${COLS[c]}${r + 1} already has '${cell.letter}', not '${letter}'.`,
        };
    } else {
      // New placement (null or multiplier string)
      newTiles.push({ r, c, letter, points: TILE_POINTS[letter] ?? 0, cellWas: cell });
      rackNeeded.push(letter);
    }
  }

  if (newTiles.length === 0)
    return { valid: false, error: 'You must place at least one new tile.' };

  // Rack possession — check against a copy
  const rackCopy = [...game.players[userId].rack];
  for (const letter of rackNeeded) {
    const idx = rackCopy.findIndex(t => t.letter === letter);
    if (idx === -1)
      return { valid: false, error: `You don't have the tile **${letter}** in your rack.` };
    rackCopy.splice(idx, 1);
  }

  if (game.firstTurn) {
    // Must cover center (7,7) and be at least 2 letters
    if (W.length < 2) return { valid: false, error: 'First word must be at least 2 letters.' };
    const coversCenter = Array.from({ length: W.length }, (_, i) => ({
      r: row0 + dr * i, c: col0 + dc * i,
    })).some(p => p.r === 7 && p.c === 7);
    if (!coversCenter)
      return { valid: false, error: 'The first word must cover the center square (**H8**).' };
  } else {
    // Must touch or pass through at least one existing tile
    const touchesExisting = newTiles.some(({ r, c }) =>
      [[r-1,c],[r+1,c],[r,c-1],[r,c+1]].some(([nr, nc]) => {
        if (nr < 0 || nr > 14 || nc < 0 || nc > 14) return false;
        const nb = board[nr][nc];
        return nb !== null && typeof nb === 'object';
      })
    );
    const overlapsExisting = W.length > newTiles.length;
    if (!touchesExisting && !overlapsExisting)
      return { valid: false, error: 'Your word must touch or extend from an existing tile on the board.' };
  }

  return { valid: true, newTiles };
}

// ── Move application ──────────────────────────────────────────────────────────
// Mutates game state. Returns { wordScore, bingo }.

export function applyPlay(game, userId, word, row0, col0, dir, newTiles) {
  const W     = word.toUpperCase();
  const dr    = dir === 'v' ? 1 : 0;
  const dc    = dir === 'h' ? 1 : 0;
  const board = game.board;

  // Calculate score BEFORE placing tiles (multipliers read from pre-placement board)
  let letterSum = 0;
  let wordMult  = 1;

  for (let i = 0; i < W.length; i++) {
    const r      = row0 + dr * i;
    const c      = col0 + dc * i;
    const cell   = board[r][c];
    const base   = TILE_POINTS[W[i]] ?? 0;
    const isNew  = !(cell !== null && typeof cell === 'object');
    const mult   = isNew ? cell : null; // multiplier string or null

    let lp = base;
    if (isNew && mult === 'DL') lp *= 2;
    if (isNew && mult === 'TL') lp *= 3;
    letterSum += lp;

    if (isNew && (mult === 'DW' || mult === 'CENTER')) wordMult *= 2;
    if (isNew && mult === 'TW') wordMult *= 3;
  }

  const wordScore = letterSum * wordMult;
  const bingo     = newTiles.length === 7; // used all 7 rack tiles → +50

  // Place tiles (consuming the multiplier square)
  for (const { r, c, letter, points } of newTiles) {
    board[r][c] = { letter, points };
  }

  // Remove used letters from rack
  const player = game.players[userId];
  for (const letter of newTiles.map(t => t.letter)) {
    const idx = player.rack.findIndex(t => t.letter === letter);
    if (idx !== -1) player.rack.splice(idx, 1);
  }

  // Refill rack from bag
  player.rack.push(...drawTiles(game.bag, 7 - player.rack.length));

  // Update score and advance turn
  player.score   += wordScore + (bingo ? 50 : 0);
  game.firstTurn  = false;
  game.currentTurn = (game.currentTurn + 1) % 2;

  return { wordScore, bingo };
}

// ── Game-over helper ──────────────────────────────────────────────────────────
// Returns an embed if the game just ended, null otherwise.

export function checkGameOver(game) {
  // The player who JUST played is now the previous turn
  const prevIdx    = (game.currentTurn + 1) % 2;
  const prevPlayer = game.players[game.turnOrder[prevIdx]];
  if (game.bag.length > 0 || prevPlayer.rack.length > 0) return null;

  // Bag empty + previous player used all tiles → game over
  const otherPlayer = game.players[game.turnOrder[game.currentTurn]];
  const penalty     = otherPlayer.rack.reduce((s, t) => s + t.points, 0);
  otherPlayer.score -= penalty;
  prevPlayer.score  += penalty;

  const [uid1, uid2] = game.turnOrder;
  const p1     = game.players[uid1];
  const p2     = game.players[uid2];
  const tied   = p1.score === p2.score;
  const winner = tied ? null : (p1.score > p2.score ? p1 : p2);

  const lines = [
    `**${p1.user.username}**: ${p1.score} pts`,
    `**${p2.user.username}**: ${p2.score} pts`,
    '',
    tied
      ? "🤝 It's a tie!"
      : `🏆 Winner: **${winner.user.username}**!`,
  ];

  return successEmbed('🔤 Game Over!', lines.join('\n'));
}
