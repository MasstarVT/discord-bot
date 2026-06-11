import { SlashCommandBuilder, ActionRowBuilder, ButtonBuilder, ButtonStyle, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { errorEmbed } from '../../utils/embedBuilder.js';
import { getOrCreate, applyBet } from '../../services/economyService.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const activeGames = new Set();

const SUITS = ['♠️', '♥️', '♦️', '♣️'];
const RANKS = ['A', '2', '3', '4', '5', '6', '7', '8', '9', '10', 'J', 'Q', 'K'];

function buildDeck() {
  const deck = SUITS.flatMap((suit) =>
    RANKS.map((rank) => ({
      suit,
      rank,
      value: rank === 'A' ? 11 : ['J', 'Q', 'K'].includes(rank) ? 10 : Number(rank),
    }))
  );
  // Fisher-Yates shuffle
  for (let i = deck.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [deck[i], deck[j]] = [deck[j], deck[i]];
  }
  return deck;
}

function handValue(hand) {
  let total = hand.reduce((s, c) => s + c.value, 0);
  let aces  = hand.filter((c) => c.rank === 'A').length;
  while (total > 21 && aces-- > 0) total -= 10;
  return total;
}

// Dealer must hit on hard < 17 and on soft 17 (Ace still counted as 11)
function shouldDealerHit(hand) {
  const total = handValue(hand);
  if (total !== 17) return total < 17;
  const hardTotal = hand.reduce((s, c) => s + (c.rank === 'A' ? 1 : c.value), 0);
  return hardTotal < 17; // true = soft 17 → hit
}

function renderHand(hand, hideSecond = false) {
  return hand.map((c, i) => (hideSecond && i === 1 ? '🂠' : `${c.suit}${c.rank}`)).join('  ');
}

function makeEmbed(playerHand, dealerHand, revealDealer, bet, balanceLine = null) {
  const dVal = revealDealer ? handValue(dealerHand) : '?';
  const embed = new EmbedBuilder()
    .setColor(COLORS.INFO)
    .setTitle('♠️ Blackjack')
    .addFields(
      { name: `Dealer's Hand ${revealDealer ? `(${dVal})` : ''}`, value: renderHand(dealerHand, !revealDealer) },
      { name: `Your Hand (${handValue(playerHand)})`,              value: renderHand(playerHand) },
      { name: 'Bet', value: `${bet} credits`, inline: true },
    )
    .setTimestamp();
  if (balanceLine) embed.addFields({ name: 'Balance', value: balanceLine, inline: true });
  return embed;
}

function buildButtons(id, disabled = false) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(`bj:${id}:hit`).setLabel('👊 Hit').setStyle(ButtonStyle.Primary).setDisabled(disabled),
    new ButtonBuilder().setCustomId(`bj:${id}:stand`).setLabel('✋ Stand').setStyle(ButtonStyle.Secondary).setDisabled(disabled),
  );
}

export const data = new SlashCommandBuilder()
  .setName('blackjack')
  .setDescription('Play a hand of Blackjack against the dealer.')
  .addIntegerOption((o) =>
    o.setName('bet')
      .setDescription('Amount to bet (min 1)')
      .setRequired(true)
      .setMinValue(1)
  );

export async function execute(interaction) {
  const userId  = interaction.user.id;
  const bet     = interaction.options.getInteger('bet');
  const account = await getOrCreate(userId);

  if (account.balance < bet) {
    return interaction.reply({
      embeds: [errorEmbed('Insufficient Funds', `You only have **${account.balance}** credits. Use \`/daily\` to claim your daily reward.`)],
      ephemeral: true,
    });
  }

  if (activeGames.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active Blackjack game!', ephemeral: true });
  }
  activeGames.add(userId);

  const deck       = buildDeck();
  const playerHand = [deck.pop(), deck.pop()];
  const dealerHand = [deck.pop(), deck.pop()];

  // Natural blackjack — instant win before any interaction
  if (handValue(playerHand) === 21) {
    const payout     = Math.floor(bet * 2.5);
    const newBalance = await applyBet(userId, bet, payout);
    const embed = makeEmbed(playerHand, dealerHand, true, bet, `${account.balance} → ${newBalance}`)
      .setColor(COLORS.SUCCESS)
      .setTitle('♠️ Blackjack — Natural Blackjack! 🃏')
      .setFooter({ text: `You win ${payout} credits (2.5×)!` });
    await interaction.reply({ embeds: [embed], components: [buildButtons(interaction.id, true)] });
    activeGames.delete(userId);
    return;
  }

  await interaction.reply({
    embeds: [makeEmbed(playerHand, dealerHand, false, bet)],
    components: [buildButtons(interaction.id)],
  });

  const collector = interaction.channel.createMessageComponentCollector({ time: 60_000 });

  // Runs dealer, computes outcome, and edits the message with the final result
  const finishGame = async (i) => {
    while (shouldDealerHit(dealerHand)) dealerHand.push(deck.pop());

    const pVal = handValue(playerHand);
    const dVal = handValue(dealerHand);
    let outcome, payout, color;

    if      (pVal > 21)  { outcome = '💥 You busted! Dealer wins.'; payout = 0;       color = COLORS.ERROR;   }
    else if (dVal > 21)  { outcome = '🎉 Dealer busted! You win!';  payout = bet * 2; color = COLORS.SUCCESS; }
    else if (pVal > dVal){ outcome = '🎉 You win!';                 payout = bet * 2; color = COLORS.SUCCESS; }
    else if (pVal < dVal){ outcome = '😔 Dealer wins.';             payout = 0;       color = COLORS.ERROR;   }
    else                 { outcome = '🤝 Push — bet returned.';     payout = bet;     color = COLORS.NEUTRAL; }

    const newBalance = await applyBet(userId, bet, payout);
    const embed = makeEmbed(playerHand, dealerHand, true, bet, `${account.balance} → ${newBalance}`)
      .setColor(color)
      .setFooter({ text: outcome });

    await i.update({ embeds: [embed], components: [buildButtons(interaction.id, true)] });
    collector.stop('done');
  };

  collector.on('collect', async (i) => {
    if (!i.customId.startsWith(`bj:${interaction.id}:`)) return;
    if (i.user.id !== userId) return i.reply({ content: "❌ This isn't your game!", ephemeral: true });

    const action = i.customId.split(':')[2];

    if (action === 'hit') {
      playerHand.push(deck.pop());
      const pVal = handValue(playerHand);
      if (pVal >= 21) {
        await finishGame(i);
        return;
      }
      await i.update({ embeds: [makeEmbed(playerHand, dealerHand, false, bet)], components: [buildButtons(interaction.id)] });
    }

    if (action === 'stand') {
      await finishGame(i);
    }
  });

  collector.on('end', async (_, reason) => {
    activeGames.delete(userId);
    if (reason === 'done') return;
    await interaction.editReply({
      embeds: [makeEmbed(playerHand, dealerHand, true, bet).setColor(COLORS.WARNING).setFooter({ text: '⏰ Game timed out.' })],
      components: [buildButtons(interaction.id, true)],
    }).catch(() => {});
  });
}
