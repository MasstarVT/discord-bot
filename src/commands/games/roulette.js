// /roulette — European roulette on the per-server economy (pkg-economy.md §5.11 and §6.2).
//
// Money: the bet is ESCROWED before the wheel spins and settled once with the gross return (R007, R136); the casino lock is
// released in `finally` (R077). Spaces are parsed strictly (R082) and "green" is the 0 pocket paying 36x like the number 0
// (R081). Everything is shown as gross payout, signed net and "balance before -> after" taken from the reserve (R240).
import { randomInt } from 'node:crypto';
import { SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS, COLORS, COIN_MAX } from '../../config/constants.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import {
  tryLockCasino, unlockCasino, reserveBet, settleBet, economyConfig, gameChannelCheck,
  formatCoins, formatNet, replyFail, clampPayout,
} from '../../services/economyService.js';
import { ackBudgetMs, sleep, withTimeout } from '../../utils/async.js';
import { neutralEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone, respondAutocomplete } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { clampText, formatNumber, joinWithLimit } from '../../utils/text.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

// ── Pure helpers (unit-tested) ───────────────────────────────────────────────

// Standard European roulette red numbers (0 is green, the rest split red/black)
export const RED_NUMBERS = new Set([1, 3, 5, 7, 9, 12, 14, 16, 18, 19, 21, 23, 25, 27, 30, 32, 34, 36]);

/** The named spaces by token. `0` is the green pocket: "green" and "0" are the same bet (R081). */
export const ROULETTE_SPACES = Object.freeze({
  red: Object.freeze({ kind: 'colour', label: 'Red', multiplier: 2 }),
  black: Object.freeze({ kind: 'colour', label: 'Black', multiplier: 2 }),
  0: Object.freeze({ kind: 'number', label: 'Green (0)', multiplier: 36 }),
  even: Object.freeze({ kind: 'parity', label: 'Even', multiplier: 2 }),
  odd: Object.freeze({ kind: 'parity', label: 'Odd', multiplier: 2 }),
  low: Object.freeze({ kind: 'range', label: 'Low (1–18)', multiplier: 2 }),
  high: Object.freeze({ kind: 'range', label: 'High (19–36)', multiplier: 2 }),
  '1-12': Object.freeze({ kind: 'dozen', label: '1st 12', multiplier: 3 }),
  '13-24': Object.freeze({ kind: 'dozen', label: '2nd 12', multiplier: 3 }),
  '25-36': Object.freeze({ kind: 'dozen', label: '3rd 12', multiplier: 3 }),
});

const NUMBER_SPACE = /^(?:[1-9]|[12]\d|3[0-6])$/; // 1..36; 0 is "green"/"0", never "00" or "07"
const STRAIGHT_UP = 36;

export function wheelColor(n) {
  if (n === 0) return { label: 'Green', emoji: '🟢' };
  return RED_NUMBERS.has(n) ? { label: 'Red', emoji: '🔴' } : { label: 'Black', emoji: '⚫' };
}

/**
 * Strict parser (R082). The whole input, lower-cased with all whitespace removed (and en/em dashes read as "-"), must be one
 * of: red, black, green, 0, even, odd, low, 1-18, high, 19-36, 1-12, 13-24, 25-36 or a number 1-36. Anything else
 * ("1st 12", "12 red", "3.9", "00", "07", "37", "-1", "") is null.
 * @returns {{ token: string, kind: string, label: string, multiplier: number } | null}
 */
export function parseSpace(raw) {
  const s = String(raw ?? '').toLowerCase().replace(/[–—]/g, '-').replace(/\s+/g, '');
  let token = null;
  if (s === 'green' || s === '0') token = '0';
  else if (s === '1-18') token = 'low';
  else if (s === '19-36') token = 'high';
  else if (Object.hasOwn(ROULETTE_SPACES, s)) token = s;
  if (token !== null) return { token, ...ROULETTE_SPACES[token] };
  if (NUMBER_SPACE.test(s)) return { token: s, kind: 'number', label: `Number ${s}`, multiplier: STRAIGHT_UP };
  return null;
}

function covers(space, n) {
  switch (space.kind) {
    case 'number': return n === Number(space.token);
    case 'colour': return n !== 0 && wheelColor(n).label.toLowerCase() === space.token;
    case 'parity': return n !== 0 && (n % 2 === 0) === (space.token === 'even');
    case 'range': return space.token === 'low' ? n >= 1 && n <= 18 : n >= 19 && n <= 36;
    case 'dozen': {
      const [from, to] = space.token.split('-').map(Number);
      return n >= from && n <= to;
    }
    default: return false;
  }
}

/** Gross payout (stake included) for a bet on `space` when the ball lands on `winNumber`; 0 for a loss. */
export function spaceReturn(space, winNumber, bet) {
  if (!space || !covers(space, winNumber)) return 0;
  return bet * space.multiplier;
}

/** A uniformly random pocket, 0..36. */
export function spinNumber(rng = randomInt) {
  return rng(37);
}

const SPIN_FRAMES = [
  '🎰  Spinning...       🔴 ⚫ 🟢 🔴 ⚫ 🔴 ⚫ 🟢 ⚫ 🔴',
  '🎰  Slowing down...   ⚫ 🟢 🔴 ⚫ 🔴 🟢 ⚫ 🔴 🟢',
  '🎰  Almost there...   🔴 🟢 ⚫ 🔴 🟢 ⚫ 🔴',
];

const INVALID_SPACE_HELP = 'Use red, black, green, even, odd, low, high, 1-12, 13-24, 25-36, or a number from 0 to 36.';

// ── Command ──────────────────────────────────────────────────────────────────

export const data = new SlashCommandBuilder()
  .setName('roulette')
  .setDescription('Bet coins on the European roulette wheel (MEE6 /roulette).')
  .addIntegerOption((o) => o.setName('bet').setDescription('Coins to bet').setRequired(true).setMinValue(1).setMaxValue(COIN_MAX))
  .addStringOption((o) => o
    .setName('space')
    .setDescription('red, black, green, even, odd, low, high, 1-12, 13-24, 25-36, or 0-36')
    .setRequired(true)
    .setAutocomplete(true)
    .setMaxLength(10));

const CANDIDATES = [
  'red', 'black', 'green', 'even', 'odd', 'low', 'high', '1-12', '13-24', '25-36',
  ...Array.from({ length: 37 }, (_, n) => String(n)),
];

/** `space` autocomplete: no database access. */
export async function autocomplete(interaction) {
  const q = String(interaction.options.getFocused() ?? '').toLowerCase().replace(/\s+/g, '');
  const choices = CANDIDATES.filter((token) => token.startsWith(q)).slice(0, 25).map((token) => {
    const space = parseSpace(token);
    // "0" is also the green pocket; keep its two entries apart in the list.
    const label = token === '0' ? 'Number 0' : space.label;
    return { name: `${label} (${space.multiplier}× return)`, value: token };
  });
  await respondAutocomplete(interaction, choices);
}

// Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
const loadSettings = (interaction) => peekSettings(interaction.guildId)
  ?? withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'casino settings').catch(() => null);

/** Test seam: the economy service, the wheel and the pause are injected. The router calls execute(interaction, client) only. */
const DEFAULTS = Object.freeze({ loadSettings, tryLockCasino, unlockCasino, reserveBet, settleBet, spin: () => spinNumber(), sleep });

const goneOrWarn = (what) => (err) => {
  if (isInteractionGone(err)) logger.debug(`Roulette: ${what}: interaction gone (${err?.code})`);
  else logger.warn(`Roulette: ${what} failed`, err);
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
  const rawSpace = interaction.options.getString('space', true);
  const space = parseSpace(rawSpace);
  if (!space) {
    await replyFail(interaction, 'Invalid Space', `**${clampText(escapeMarkdown(rawSpace), 20)}** is not a valid bet. ${INVALID_SPACE_HELP}`);
    return;
  }

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
      logger.error('Roulette: reserving the bet failed', { guildId, userId, bet }, err);
      await replyFail(interaction, 'Economy Unavailable', 'Could not place your bet. Nothing was taken — please try again in a moment.');
      return;
    }
    if (!res.ok) {
      const [title, text] = refusal(res, settings, cfg);
      await replyFail(interaction, title, text);
      return;
    }
    reserved = true;

    // The bet is escrowed: whatever happens to the animation, the spin is settled below.
    for (const frame of SPIN_FRAMES) {
      await interaction.editReply({ embeds: [neutralEmbed(frame)] }).catch(goneOrWarn('animation frame'));
      await deps.sleep(1000);
    }

    const winNumber = deps.spin();
    const { label, emoji } = wheelColor(winNumber);
    const payout = spaceReturn(space, winNumber, bet);

    settled = true;
    let settlement;
    try {
      const r = await deps.settleBet(guildId, userId, payout, settings, { stake: bet });
      settlement = { failed: false, balance: r.balance, capped: Boolean(r.capped), payoutCapped: Boolean(r.payoutCapped) };
    } catch (err) {
      logger.error('Roulette: settlement failed', { guildId, userId, payout, stake: bet }, err);
      settlement = { failed: true, balance: null, capped: false, payoutCapped: false };
    }

    const clamp = clampPayout(payout, bet, cfg.maxPayout);
    const shown = clamp.payout;
    let description = shown > 0 ? `🎉 You win ${formatNet(shown - bet, settings)}!` : '😔 You lose. Better luck next time.';
    if (settlement.payoutCapped || clamp.capped) {
      description += `\nWinnings capped at ${formatCoins(cfg.maxPayout, settings)} by the server's max payout.`;
    }
    const balance = settlement.failed
      ? '⚠️ Payout failed — an admin has been notified in the bot logs.'
      : `${formatNumber(res.before)} → ${formatNumber(settlement.balance)} ${cfg.currencyName}${settlement.capped ? ' (server maximum)' : ''}`;

    const embed = neutralEmbed(`🎰 The ball lands on ${winNumber} ${emoji} ${label}`, description)
      .setColor(shown > 0 ? COLORS.SUCCESS : COLORS.ERROR)
      .addFields(
        { name: 'Your bet', value: `${space.label} — ${formatCoins(bet, settings)}`, inline: true },
        { name: 'Bet', value: formatCoins(bet, settings), inline: true },
        { name: 'Payout', value: formatCoins(shown, settings), inline: true },
        { name: 'Net', value: formatNet(shown - bet, settings), inline: true },
        { name: 'Balance', value: balance, inline: true },
      );
    await interaction.editReply({ embeds: [embed] }).catch(goneOrWarn('result'));
  } catch (err) {
    if (reserved && !settled) {
      // Unexpected failure after the bet was escrowed and before the spin was settled: the stake goes back, once.
      logger.error('Roulette failed after the bet was reserved', { guildId, userId, bet }, err);
      settled = true;
      try { await deps.settleBet(guildId, userId, bet, settings, { stake: bet }); } catch (e) { logger.error('Roulette: refund failed', { guildId, userId, bet }, e); }
      await replyFail(interaction, 'Roulette Unavailable', 'Something went wrong; your bet has been refunded.');
      return;
    }
    throw err;
  } finally {
    deps.unlockCasino(guildId, userId);
  }
}
