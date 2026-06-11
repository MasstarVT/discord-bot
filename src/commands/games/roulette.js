import { SlashCommandBuilder, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { errorEmbed } from '../../utils/embedBuilder.js';
import { getOrCreate, applyBet } from '../../services/economyService.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

// Standard European roulette red numbers (0 is green, rest split red/black)
const RED_NUMBERS = new Set([1, 3, 5, 7, 9, 12, 14, 16, 18, 19, 21, 23, 25, 27, 30, 32, 34, 36]);

function wheelColor(n) {
  if (n === 0) return { label: 'Green', emoji: '🟢' };
  return RED_NUMBERS.has(n)
    ? { label: 'Red',   emoji: '🔴' }
    : { label: 'Black', emoji: '⚫' };
}

// Returns the total amount returned to the player (0 = loss)
function calcPayout(space, winNumber, bet) {
  const { label } = wheelColor(winNumber);
  const isEven    = winNumber !== 0 && winNumber % 2 === 0;
  const isOdd     = winNumber !== 0 && winNumber % 2 !== 0;
  const numBet    = parseInt(space, 10);

  if (space === 'red'   && label === 'Red')            return bet * 2;
  if (space === 'black' && label === 'Black')          return bet * 2;
  if (space === 'even'  && isEven)                     return bet * 2;
  if (space === 'odd'   && isOdd)                      return bet * 2;
  if (space === 'green' && winNumber === 0)            return bet * 18;
  if (!isNaN(numBet)    && numBet === winNumber)       return bet * 36;
  return 0;
}

// Parses and normalises the space input; returns null if invalid
function parseSpace(raw) {
  const s = raw.toLowerCase().trim();
  if (['red', 'black', 'green', 'even', 'odd'].includes(s)) return s;
  const n = parseInt(s, 10);
  if (!isNaN(n) && n >= 0 && n <= 36) return String(n);
  return null;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const SPIN_FRAMES = [
  '🎰  Spinning...       🔴 ⚫ 🟢 🔴 ⚫ 🔴 ⚫ 🟢 ⚫ 🔴',
  '🎰  Slowing down...   ⚫ 🟢 🔴 ⚫ 🔴 🟢 ⚫ 🔴 🟢',
  '🎰  Almost there...   🔴 🟢 ⚫ 🔴 🟢 ⚫ 🔴',
];

export const data = new SlashCommandBuilder()
  .setName('roulette')
  .setDescription('Spin the European roulette wheel!')
  .addIntegerOption((o) =>
    o.setName('bet')
      .setDescription('Amount to bet (min 1)')
      .setRequired(true)
      .setMinValue(1)
  )
  .addStringOption((o) =>
    o.setName('space')
      .setDescription('Where to bet: red, black, green, even, odd, or a number (0–36)')
      .setRequired(true)
  );

export async function execute(interaction) {
  const userId   = interaction.user.id;
  const bet      = interaction.options.getInteger('bet');
  const rawSpace = interaction.options.getString('space');
  const space    = parseSpace(rawSpace);

  if (!space) {
    return interaction.reply({
      embeds: [errorEmbed('Invalid Space', `**"${rawSpace}"** is not a valid bet. Use \`red\`, \`black\`, \`green\`, \`even\`, \`odd\`, or a number from \`0\` to \`36\`.`)],
      ephemeral: true,
    });
  }

  const account = await getOrCreate(userId);
  if (account.balance < bet) {
    return interaction.reply({
      embeds: [errorEmbed('Insufficient Funds', `You only have **${account.balance}** credits. Use \`/daily\` to claim your daily reward.`)],
      ephemeral: true,
    });
  }

  // Defer so we have time to play the spin animation
  await interaction.deferReply();

  // Spin animation — three frames with short pauses
  for (const frame of SPIN_FRAMES) {
    await interaction.editReply({
      embeds: [new EmbedBuilder().setColor(COLORS.INFO).setTitle(frame).setTimestamp()],
    });
    await sleep(1500);
  }

  // Determine outcome
  const winNumber        = Math.floor(Math.random() * 37); // 0–36
  const { label, emoji } = wheelColor(winNumber);
  const payout           = calcPayout(space, winNumber, bet);
  const won              = payout > 0;
  const newBalance       = await applyBet(userId, bet, payout);

  // Display the bet space in a readable format
  const spaceDisplay = ['red', 'black', 'green', 'even', 'odd'].includes(space)
    ? space.charAt(0).toUpperCase() + space.slice(1)
    : `Number ${space}`;

  const color      = won ? COLORS.SUCCESS : COLORS.ERROR;
  const resultText = won
    ? `🎉 You win **${payout}** credits!`
    : `😔 You lose. Better luck next time.`;

  const embed = new EmbedBuilder()
    .setColor(color)
    .setTitle(`🎰 The ball lands on **${winNumber}** ${emoji} ${label}!`)
    .setDescription(resultText)
    .addFields(
      { name: 'Your Bet', value: `${spaceDisplay} — ${bet} credits`, inline: true },
      { name: 'Payout',   value: won ? `${payout} credits (${payout / bet}×)` : '0 credits', inline: true },
      { name: 'Balance',  value: `${account.balance} → ${newBalance} credits`, inline: true },
    )
    .setTimestamp();

  await interaction.editReply({ embeds: [embed] });
}
