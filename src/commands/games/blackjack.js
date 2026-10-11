// /blackjack — blackjack against the dealer on the per-server economy (pkg-economy.md §5.10 and §6.1).
//
// Money: the bet is ESCROWED (debited atomically) before a card is dealt and settled exactly once with its gross payout
// (R007, R136). A hand that is abandoned, times out or is interrupted keeps the stake taken (R018-R020). Double-settlement
// is impossible because `settled` is claimed and `collector.stop('done')` runs before any await (R016, R017, R079). The
// casino lock is released in `finally` or, once the collector owns the hand, after the settlement / forfeit (R077).
// The dealer peeks for a natural before the player acts (R078). Every collector click is acknowledged with
// `deferUpdate()` as its FIRST await, before any database access, and the message is edited through `editReply`.
import { randomInt } from 'node:crypto';
import { SlashCommandBuilder, ActionRowBuilder, ButtonBuilder, ButtonStyle, ComponentType } from 'discord.js';
import { PERMISSION_LEVELS, COLORS, COIN_MAX } from '../../config/constants.js';
import { buildCustomId, parseCustomId } from '../../config/customIds.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import {
  tryLockCasino, unlockCasino, reserveBet, settleBet, economyConfig, gameChannelCheck,
  formatCoins, formatNet, replyFail, clampPayout,
} from '../../services/economyService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { applyGuildColor, neutralEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { formatNumber, joinWithLimit } from '../../utils/text.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const SUITS = ['♠️', '♥️', '♦️', '♣️'];
export const RANKS = ['A', '2', '3', '4', '5', '6', '7', '8', '9', '10', 'J', 'Q', 'K'];
/** The collector ends after this much silence (an abandoned hand is forfeited) ... */
export const IDLE_MS = 60_000;
/** ... or after this long in total (interaction tokens live 15 minutes). */
export const MAX_MS = 600_000;

// ── Pure game helpers (unit-tested) ──────────────────────────────────────────

const rankValue = (rank) => (rank === 'A' ? 11 : ['J', 'Q', 'K'].includes(rank) ? 10 : Number(rank));

/** @returns {Array<{ suit: string, rank: string, value: number }>} 52 cards, Fisher-Yates shuffled with rng(n) in 0..n-1 */
export function buildDeck(rng = randomInt) {
  const deck = SUITS.flatMap((suit) => RANKS.map((rank) => ({ suit, rank, value: rankValue(rank) })));
  for (let i = deck.length - 1; i > 0; i--) {
    const j = rng(i + 1);
    [deck[i], deck[j]] = [deck[j], deck[i]];
  }
  return deck;
}

// Hard total (every ace as 1) and whether one ace can still count as 11 without busting.
function totals(hand) {
  let hard = 0;
  let aces = 0;
  for (const card of hand) {
    if (card.rank === 'A') { hard += 1; aces++; } else hard += rankValue(card.rank);
  }
  return { hard, soft: aces > 0 && hard + 10 <= 21 };
}

/** Best total: aces count 11 and drop to 1 one at a time while the hand would bust. */
export function handValue(hand) {
  const { hard, soft } = totals(hand);
  return soft ? hard + 10 : hard;
}

/** True when an ace is still counted as 11. */
export function isSoft(hand) {
  return totals(hand).soft;
}

/** Exactly two cards totalling 21. */
export function isNatural(hand) {
  return hand.length === 2 && handValue(hand) === 21;
}

/** The dealer draws below 17 and on a soft 17 (H17). */
export function shouldDealerHit(hand) {
  const value = handValue(hand);
  return value < 17 || (value === 17 && isSoft(hand));
}

/**
 * The dealer's peek: settles the hand before the player acts when either side has a natural (R078).
 * @returns {null | { result: 'push-naturals' | 'blackjack' | 'dealer-blackjack', payout: number }} gross payout
 */
export function naturalOutcome(player, dealer, bet) {
  const p = isNatural(player);
  const d = isNatural(dealer);
  if (p && d) return { result: 'push-naturals', payout: bet };
  if (p) return { result: 'blackjack', payout: bet + Math.floor((bet * 3) / 2) }; // 3:2
  if (d) return { result: 'dealer-blackjack', payout: 0 };
  return null;
}

/**
 * Outcome once the dealer has played. Only called when nobody had a natural (the peek settled those), so a multi-card 21
 * can never be compared against a dealer natural. `stake` is the total escrowed (2x the bet after a double-down).
 * @returns {{ result: 'bust' | 'dealer-bust' | 'win' | 'lose' | 'push', payout: number }} gross payout
 */
export function finalOutcome(player, dealer, stake) {
  const p = handValue(player);
  const d = handValue(dealer);
  if (p > 21) return { result: 'bust', payout: 0 };
  if (d > 21) return { result: 'dealer-bust', payout: stake * 2 };
  if (p > d) return { result: 'win', payout: stake * 2 };
  if (p < d) return { result: 'lose', payout: 0 };
  return { result: 'push', payout: stake };
}

/** Result text and embed colour per outcome. */
export const RESULTS = Object.freeze({
  blackjack: { text: '🃏 Blackjack! Paid 3:2.', color: COLORS.SUCCESS },
  'push-naturals': { text: '🤝 Both have blackjack — push.', color: COLORS.NEUTRAL },
  'dealer-blackjack': { text: '🂡 Dealer has blackjack.', color: COLORS.ERROR },
  bust: { text: '💥 You busted.', color: COLORS.ERROR },
  'dealer-bust': { text: '🎉 Dealer busted — you win!', color: COLORS.SUCCESS },
  win: { text: '🎉 You win!', color: COLORS.SUCCESS },
  lose: { text: '😔 Dealer wins.', color: COLORS.ERROR },
  push: { text: '🤝 Push — your bet is returned.', color: COLORS.NEUTRAL },
});

// ── Views ────────────────────────────────────────────────────────────────────

const TITLE = '♠️ Blackjack';

function renderHand(hand, hideSecond = false) {
  return hand.map((c, i) => (hideSecond && i === 1 ? '🂠' : `${c.suit}${c.rank}`)).join('  ');
}

function handFields(player, dealer, revealed) {
  return [
    { name: `Dealer (${revealed ? handValue(dealer) : '?'})`, value: renderHand(dealer, !revealed) },
    { name: `You (${handValue(player)})`, value: renderHand(player) },
  ];
}

/** Hit, Stand and Double; Double is enabled only for a fresh two-card hand within the max bet. */
export function buildButtons(id, { canDouble = false, disabled = false } = {}) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('bj', id, 'hit')).setLabel('👊 Hit').setStyle(ButtonStyle.Primary).setDisabled(disabled),
    new ButtonBuilder().setCustomId(buildCustomId('bj', id, 'stand')).setLabel('✋ Stand').setStyle(ButtonStyle.Secondary).setDisabled(disabled),
    new ButtonBuilder().setCustomId(buildCustomId('bj', id, 'double')).setLabel('💰 Double').setStyle(ButtonStyle.Success).setDisabled(disabled || !canDouble),
  );
}

function progressEmbed(settings, { player, dealer, stake, canDouble }) {
  return applyGuildColor(neutralEmbed(TITLE, canDouble ? 'Hit, stand or double down.' : 'Hit or stand.'), settings)
    .addFields(...handFields(player, dealer, false), { name: 'Bet', value: formatCoins(stake, settings), inline: true });
}

/** Bet / Payout / Net / Balance (R240: the payout is the gross return, the net is signed, "before" comes from the reserve). */
function resultFields(settings, cfg, { stake, payout, before, settlement }) {
  const shown = clampPayout(payout, stake, cfg.maxPayout).payout;
  const balance = settlement.failed
    ? '⚠️ Payout failed — an admin has been notified in the bot logs.'
    : `${formatNumber(before)} → ${formatNumber(settlement.balance)} ${cfg.currencyName}${settlement.capped ? ' (server maximum)' : ''}`;
  return [
    { name: 'Bet', value: formatCoins(stake, settings), inline: true },
    { name: 'Payout', value: formatCoins(shown, settings), inline: true },
    { name: 'Net', value: formatNet(shown - stake, settings), inline: true },
    { name: 'Balance', value: balance, inline: true },
  ];
}

function finalEmbed(settings, cfg, { player, dealer, result, payout, stake, doubled, before, settlement }) {
  const { text, color } = RESULTS[result];
  let description = text + (doubled ? ' (doubled down)' : '');
  if (settlement.payoutCapped || clampPayout(payout, stake, cfg.maxPayout).capped) {
    description += `\nWinnings capped at ${formatCoins(cfg.maxPayout, settings)} by the server's max payout.`;
  }
  return neutralEmbed(TITLE, description).setColor(color)
    .addFields(...handFields(player, dealer, true), ...resultFields(settings, cfg, { stake, payout, before, settlement }));
}

function forfeitEmbed(settings, { player, dealer, stake }) {
  return neutralEmbed(TITLE, `⏰ You walked away from the table. Your bet of ${formatCoins(stake, settings)} is forfeited.`)
    .setColor(COLORS.WARNING)
    .addFields(...handFields(player, dealer, true), { name: 'Net', value: formatNet(-stake, settings), inline: true });
}

// ── Command ──────────────────────────────────────────────────────────────────

export const data = new SlashCommandBuilder()
  .setName('blackjack')
  .setDescription('Play blackjack against the dealer for coins.')
  .addIntegerOption((o) => o.setName('bet').setDescription('Coins to bet').setRequired(true).setMinValue(1).setMaxValue(COIN_MAX));

// Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
const loadSettings = (interaction) => peekSettings(interaction.guildId)
  ?? withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'casino settings').catch(() => null);

/** Test seam: the economy service and the shuffle are injected. The router calls execute(interaction, client) only. */
const DEFAULTS = Object.freeze({ loadSettings, tryLockCasino, unlockCasino, reserveBet, settleBet, newDeck: () => buildDeck() });

const goneOrWarn = (what) => (err) => {
  if (isInteractionGone(err)) logger.debug(`Blackjack: ${what}: interaction gone (${err?.code})`);
  else logger.warn(`Blackjack: ${what} failed`, err);
};

/**
 * Shared start sequence, steps 1-3 (§6.0): settings, channel rule and max bet, all without a database access and before
 * the acknowledgement. Replies privately and returns null when the game is refused.
 */
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

  // From here the lock is released in `finally` unless the collector owns it (its settlement / forfeit unlocks).
  let handedOff = false;
  let reserved = false;
  let settled = false;
  let stake = bet;
  try {
    await interaction.deferReply();

    let res;
    try {
      res = await deps.reserveBet(guildId, userId, bet, settings);
    } catch (err) {
      logger.error('Blackjack: reserving the bet failed', { guildId, userId, bet }, err);
      await replyFail(interaction, 'Economy Unavailable', 'Could not place your bet. Nothing was taken — please try again in a moment.');
      return;
    }
    if (!res.ok) {
      const [title, text] = refusal(res, settings, cfg);
      await replyFail(interaction, title, text);
      return;
    }
    reserved = true;
    const { before } = res;

    const deck = deps.newDeck();
    const draw = () => (deck.length ? deck.pop() : buildDeck().pop());
    const player = [draw(), draw()];
    const dealer = [draw(), draw()];

    // Credits the gross payout once; never throws (a failed credit is shown in the Balance field).
    const settle = async (payout, total) => {
      try {
        const r = await deps.settleBet(guildId, userId, payout, settings, { stake: total });
        return { failed: false, balance: r.balance, capped: Boolean(r.capped), payoutCapped: Boolean(r.payoutCapped) };
      } catch (err) {
        logger.error('Blackjack: settlement failed', { guildId, userId, payout, stake: total }, err);
        return { failed: true, balance: null, capped: false, payoutCapped: false };
      }
    };

    // Edits the hand message; a click that was acknowledged with deferUpdate can edit it too if the original token fails.
    const show = async (payload, click = null) => {
      try {
        await interaction.editReply(payload);
      } catch (err) {
        if (click) {
          try { await click.editReply(payload); return; } catch { /* fall through to the log below */ }
        }
        goneOrWarn('edit')(err);
      }
    };

    // Peek (R078): a natural settles the hand before the player acts.
    const natural = naturalOutcome(player, dealer, bet);
    if (natural) {
      settled = true;
      const settlement = await settle(natural.payout, bet);
      await show({
        embeds: [finalEmbed(settings, cfg, { player, dealer, result: natural.result, payout: natural.payout, stake: bet, doubled: false, before, settlement })],
        components: [],
      });
      return;
    }

    const canDouble = () => !settled && player.length === 2 && stake === bet && !(cfg.maxBet > 0 && bet * 2 > cfg.maxBet);
    let collector;
    try {
      const message = await interaction.editReply({
        embeds: [progressEmbed(settings, { player, dealer, stake, canDouble: canDouble() })],
        components: [buildButtons(interaction.id, { canDouble: canDouble() })],
      });
      collector = message.createMessageComponentCollector({ componentType: ComponentType.Button, idle: IDLE_MS, time: MAX_MS });
    } catch (err) {
      // The hand never reached the player (or has no collector): the stake goes back, once.
      logger.error('Blackjack: could not start the hand after the bet was reserved — refunding', { guildId, userId, bet }, err);
      settled = true;
      const settlement = await settle(bet, bet);
      if (settlement.failed) logger.error('Blackjack: refund failed after a start-up error', { guildId, userId, bet });
      await replyFail(interaction, 'Blackjack Unavailable', settlement.failed
        ? 'The hand could not be started and your bet could not be refunded automatically. An admin has been notified in the bot logs.'
        : 'The hand could not be started. Your bet has been refunded.');
      return;
    }

    let busy = false;

    // Settles the hand once: `settled` and collector.stop() come before any await (R016, R017, R079).
    const finish = async (click) => {
      if (settled) return;
      settled = true;
      collector.stop('done');
      try {
        if (handValue(player) <= 21) while (shouldDealerHit(dealer)) dealer.push(draw());
        const { result, payout } = finalOutcome(player, dealer, stake);
        const settlement = await settle(payout, stake);
        await show({
          embeds: [finalEmbed(settings, cfg, { player, dealer, result, payout, stake, doubled: stake !== bet, before, settlement })],
          components: [],
        }, click);
      } catch (err) {
        logger.error('Blackjack: finishing the hand failed', { guildId, userId, bet, stake }, err);
      } finally {
        deps.unlockCasino(guildId, userId);
      }
    };

    const act = async (click, action) => {
      if (settled) return;
      if (action === 'hit') {
        player.push(draw());
        if (handValue(player) >= 21) { await finish(click); return; } // bust, or 21: stand automatically
        await show({
          embeds: [progressEmbed(settings, { player, dealer, stake, canDouble: canDouble() })],
          components: [buildButtons(interaction.id, { canDouble: canDouble() })],
        }, click);
        return;
      }
      if (action === 'stand') { await finish(click); return; }
      if (action === 'double') {
        if (!canDouble()) return; // already acknowledged; the button was disabled
        let r;
        try {
          r = await deps.reserveBet(guildId, userId, bet, settings);
        } catch (err) {
          logger.error('Blackjack: reserving the double-down failed', { guildId, userId, bet }, err);
          await replyFail(click, 'Cannot Double Down', 'Something went wrong; nothing was taken. Hit or stand instead.');
          return;
        }
        if (!r.ok) {
          await replyFail(click, 'Cannot Double Down', r.code === 'INSUFFICIENT'
            ? `You need another ${formatCoins(bet, settings)} to double down.`
            : 'The second bet was refused. Hit or stand instead.');
          return;
        }
        if (settled) {
          // The collector ended during the await (the hand was forfeited): give the extra stake back.
          const refund = await settle(bet, bet);
          if (refund.failed) logger.error('Blackjack: refund of a late double-down failed', { guildId, userId, bet });
          return;
        }
        stake = bet * 2;
        player.push(draw());
        await finish(click);
      }
    };

    collector.on('collect', async (i) => {
      try {
        const { prefix, params } = parseCustomId(i.customId);
        if (prefix !== 'bj' || params[0] !== interaction.id) return;
        if (i.user.id !== userId) {
          await i.reply({ content: "❌ This isn't your game!", ephemeral: true }).catch(goneOrWarn('reply'));
          return;
        }
        // Concurrent clicks are dropped (never queued): the decision is synchronous, the acknowledgement is the FIRST await.
        const drop = settled || busy;
        if (!drop) busy = true;
        await i.deferUpdate().catch(goneOrWarn('acknowledge'));
        if (drop) return;
        try {
          await act(i, params[1]);
        } finally {
          busy = false;
        }
      } catch (err) {
        logger.error('Blackjack collect failed', { guildId, userId }, err);
        if (!i.deferred && !i.replied) await i.deferUpdate().catch(() => {});
      }
    });

    collector.on('end', (_collected, reason) => {
      // 'done': finish() is settling and unlocks when the payout is in. Anything else abandons the hand (idle, total time,
      // message or channel deleted): the escrowed stake stays taken (R018-R020).
      if (reason === 'done') return;
      try {
        deps.unlockCasino(guildId, userId);
        if (settled) return;
        settled = true;
        interaction.editReply({ embeds: [forfeitEmbed(settings, { player, dealer, stake })], components: [] })
          .catch(goneOrWarn('forfeit message'));
      } catch (err) {
        logger.error('Blackjack end handler failed', { guildId, userId }, err);
      }
    });

    handedOff = true;
  } catch (err) {
    if (reserved && !settled && !handedOff) {
      // Unexpected failure after the bet was escrowed and before the hand could be played: nothing was dealt to the player.
      logger.error('Blackjack failed after the bet was reserved', { guildId, userId, bet }, err);
      settled = true;
      try { await deps.settleBet(guildId, userId, bet, settings, { stake: bet }); } catch (e) { logger.error('Blackjack: refund failed', { guildId, userId, bet }, e); }
      await replyFail(interaction, 'Blackjack Unavailable', 'Something went wrong; your bet has been refunded.');
      return;
    }
    throw err;
  } finally {
    if (!handedOff) deps.unlockCasino(guildId, userId);
  }
}
