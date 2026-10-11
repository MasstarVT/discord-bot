// /slots — 3x3 slot machine on the per-server economy (pkg-economy.md §5.12 and §6.3).
//
// Money: the bet is ESCROWED before the reels spin and settled once with the gross return (R007, R136); the casino lock is
// released in `finally` (R077). The result shows the gross payout, the signed net and "balance before -> after" from the
// reserve (R240); every winning spin returns at least one coin of profit.
import { randomInt } from 'node:crypto';
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS, COIN_MAX } from '../../config/constants.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import {
  tryLockCasino, unlockCasino, reserveBet, settleBet, economyConfig, gameChannelCheck,
  formatCoins, formatNet, replyFail, clampPayout,
} from '../../services/economyService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { neutralEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { formatNumber, joinWithLimit } from '../../utils/text.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

// ── Pure helpers (unit-tested) ───────────────────────────────────────────────

// Rarer symbols have lower weight and higher payout multipliers (per winning line)
export const SYMBOLS = Object.freeze([
  Object.freeze({ emoji: '💎', weight: 1, multiplier: 5 }),
  Object.freeze({ emoji: '7️⃣', weight: 2, multiplier: 4 }),
  Object.freeze({ emoji: '🍇', weight: 3, multiplier: 3 }),
  Object.freeze({ emoji: '🍒', weight: 4, multiplier: 2.5 }),
  Object.freeze({ emoji: '🍋', weight: 5, multiplier: 2 }),
  Object.freeze({ emoji: '🍎', weight: 6, multiplier: 1.5 }),
]);

export const TOTAL_WEIGHT = SYMBOLS.reduce((sum, s) => sum + s.weight, 0);

// Winning lines: 3 rows + 2 diagonals (indices into the flat 9-cell grid)
export const LINES = Object.freeze([[0, 1, 2], [3, 4, 5], [6, 7, 8], [0, 4, 8], [2, 4, 6]]);

/** One weighted symbol: r = rng(TOTAL_WEIGHT) is walked through the weights. */
export function spinSymbol(rng = randomInt) {
  let r = rng(TOTAL_WEIGHT);
  for (const sym of SYMBOLS) {
    if (r < sym.weight) return sym;
    r -= sym.weight;
  }
  return SYMBOLS[SYMBOLS.length - 1];
}

/** @returns {Array<{ emoji: string, weight: number, multiplier: number }>} the nine cells, row by row */
export function spinGrid(rng = randomInt) {
  return Array.from({ length: 9 }, () => spinSymbol(rng));
}

/** @returns {Array<{ a: number, b: number, c: number, symbol: object }>} every line of three equal symbols */
export function checkWins(grid) {
  return LINES.filter(([a, b, c]) => grid[a].emoji === grid[b].emoji && grid[a].emoji === grid[c].emoji)
    .map(([a, b, c]) => ({ a, b, c, symbol: grid[a] }));
}

/**
 * Gross payout (stake included): the multipliers of every winning line add up, the product is floored, and a winning spin
 * always returns at least `bet + 1` (R240: at bet 1 an apple line used to "win" exactly the stake). 0 when nothing lines up.
 */
export function slotsPayout(grid, bet) {
  const wins = checkWins(grid);
  const multiplier = wins.reduce((sum, w) => sum + w.symbol.multiplier, 0);
  const payout = wins.length ? Math.max(Math.floor(bet * multiplier), bet + 1) : 0;
  return { payout, multiplier, wins };
}

export function renderGrid(grid, wins) {
  const rows = [];
  for (let r = 0; r < 3; r++) {
    const line = [r * 3, r * 3 + 1, r * 3 + 2];
    const cells = line.map((i) => grid[i].emoji).join(' ');
    const isWin = wins.some((w) => w.a === line[0] && w.b === line[1] && w.c === line[2]);
    rows.push(`${cells}${isWin ? '  ◀ WIN' : ''}`);
  }
  const diagonals = [];
  if (wins.some((w) => w.a === 0 && w.b === 4 && w.c === 8)) diagonals.push('↘ Diagonal: WIN');
  if (wins.some((w) => w.a === 2 && w.b === 4 && w.c === 6)) diagonals.push('↙ Diagonal: WIN');
  return rows.join('\n') + (diagonals.length ? `\n${diagonals.join('\n')}` : '');
}

// ── Command ──────────────────────────────────────────────────────────────────

export const data = new SlashCommandBuilder()
  .setName('slots')
  .setDescription('Spin the 3×3 slot machine for coins.')
  .addIntegerOption((o) => o.setName('bet').setDescription('Coins to bet').setRequired(true).setMinValue(1).setMaxValue(COIN_MAX));

// Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
const loadSettings = (interaction) => peekSettings(interaction.guildId)
  ?? withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'casino settings').catch(() => null);

/** Test seam: the economy service and the reels are injected. The router calls execute(interaction, client) only. */
const DEFAULTS = Object.freeze({ loadSettings, tryLockCasino, unlockCasino, reserveBet, settleBet, spin: () => spinGrid() });

const goneOrWarn = (what) => (err) => {
  if (isInteractionGone(err)) logger.debug(`Slots: ${what}: interaction gone (${err?.code})`);
  else logger.warn(`Slots: ${what} failed`, err);
};

/** Shared start sequence, steps 1-3 (§6.0): settings, channel rule, max bet; private reply and null when refused. */
async function begin(interaction, bet, deps) {
  const settings = await deps.loadSettings(interaction);
  if (!settings) {
    await replyFail(interaction, 'Temporarily Unavailable', 'Server settings could not be loaded. Please try again in a moment.');
    return null;
  }
  const cfg = economyConfig(settings);
  const chk = gameChannelCheck(settings, interaction.channel ?? { id: interaction.channelId }, interaction.guild);
  if (!chk.ok) {
    const deny = chk.mode === 'DENY' || Array.isArray(chk.deniedIds);
    await replyFail(interaction, 'Wrong Channel', deny
      ? 'Casino games are not allowed in this channel.'
      : `Casino games can only be played in ${joinWithLimit((chk.allowedIds ?? []).map((id) => `<#${id}>`), { max: 1500 })}.`);
    return null;
  }
  if (cfg.maxBet > 0 && bet > cfg.maxBet) {
    await replyFail(interaction, 'Bet Too High', `The maximum bet on this server is ${formatCoins(cfg.maxBet, settings)}.`);
    return null;
  }
  return { settings, cfg };
}

/** Title and text for a refused reserve. */
function refusal(res, settings, cfg) {
  if (res.code === 'INSUFFICIENT') return ['Insufficient Funds', `You only have ${formatCoins(res.balance, settings)}. Try /daily, /work or /pity.`];
  if (res.code === 'MAX_BET') return ['Bet Too High', `The maximum bet on this server is ${formatCoins(res.maxBet ?? cfg.maxBet, settings)}.`];
  return ['Invalid Bet', 'That is not a valid bet.'];
}

export async function execute(interaction, _client, deps = DEFAULTS) {
  const bet = interaction.options.getInteger('bet', true);
  const start = await begin(interaction, bet, deps);
  if (!start) return;
  const { settings, cfg } = start;
  const { guildId } = interaction;
  const userId = interaction.user.id;

  // The lock is taken synchronously, with nothing awaited between begin() and here that could interleave.
  if (!deps.tryLockCasino(guildId, userId)) {
    await replyFail(interaction, null, null, { embed: warnEmbed('Game In Progress', 'You already have a casino game running. Finish it first.') });
    return;
  }

  let reserved = false;
  let settled = false;
  try {
    await interaction.deferReply();

    let res;
    try {
      res = await deps.reserveBet(guildId, userId, bet, settings);
    } catch (err) {
      logger.error('Slots: reserving the bet failed', { guildId, userId, bet }, err);
      await replyFail(interaction, 'Economy Unavailable', 'Could not place your bet. Nothing was taken — please try again in a moment.');
      return;
    }
    if (!res.ok) {
      const [title, text] = refusal(res, settings, cfg);
      await replyFail(interaction, title, text);
      return;
    }
    reserved = true;

    const grid = deps.spin();
    const { payout, multiplier, wins } = slotsPayout(grid, bet);

    settled = true;
    let settlement;
    try {
      const r = await deps.settleBet(guildId, userId, payout, settings, { stake: bet });
      settlement = { failed: false, balance: r.balance, capped: Boolean(r.capped), payoutCapped: Boolean(r.payoutCapped) };
    } catch (err) {
      logger.error('Slots: settlement failed', { guildId, userId, payout, stake: bet }, err);
      settlement = { failed: true, balance: null, capped: false, payoutCapped: false };
    }

    const clamp = clampPayout(payout, bet, cfg.maxPayout);
    const shown = clamp.payout;
    let resultText = shown > 0
      ? `🎉 ${wins.length} winning line${wins.length === 1 ? '' : 's'} (${multiplier}× payout) — ${formatNet(shown - bet, settings)}!`
      : '❌ No match — better luck next time!';
    if (settlement.payoutCapped || clamp.capped) {
      resultText += `\nWinnings capped at ${formatCoins(cfg.maxPayout, settings)} by the server's max payout.`;
    }
    const balance = settlement.failed
      ? '⚠️ Payout failed — an admin has been notified in the bot logs.'
      : `${formatNumber(res.before)} → ${formatNumber(settlement.balance)} ${cfg.currencyName}${settlement.capped ? ' (server maximum)' : ''}`;

    const embed = neutralEmbed('🎰 Slot Machine', `\`\`\`\n${renderGrid(grid, wins)}\n\`\`\`\n${resultText}`)
      .setColor(shown > bet ? COLORS.SUCCESS : shown > 0 ? COLORS.NEUTRAL : COLORS.ERROR)
      .addFields(
        { name: 'Bet', value: formatCoins(bet, settings), inline: true },
        { name: 'Payout', value: formatCoins(shown, settings), inline: true },
        { name: 'Net', value: formatNet(shown - bet, settings), inline: true },
        { name: 'Balance', value: balance, inline: true },
      );
    await interaction.editReply({ embeds: [embed] }).catch(goneOrWarn('result'));
  } catch (err) {
    if (reserved && !settled) {
      // Unexpected failure after the bet was escrowed and before the spin was settled: the stake goes back, once.
      logger.error('Slots failed after the bet was reserved', { guildId, userId, bet }, err);
      settled = true;
      try { await deps.settleBet(guildId, userId, bet, settings, { stake: bet }); } catch (e) { logger.error('Slots: refund failed', { guildId, userId, bet }, e); }
      await replyFail(interaction, 'Slots Unavailable', 'Something went wrong; your bet has been refunded.');
      return;
    }
    throw err;
  } finally {
    deps.unlockCasino(guildId, userId);
  }
}
