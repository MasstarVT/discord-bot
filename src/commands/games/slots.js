import { SlashCommandBuilder, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { errorEmbed } from '../../utils/embedBuilder.js';
import { getOrCreate, applyBet } from '../../services/economyService.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

// Rarer symbols have lower weight and higher payout multipliers
const SYMBOLS = [
  { emoji: '💎', weight: 1,  multiplier: 5   },
  { emoji: '7️⃣', weight: 2,  multiplier: 4   },
  { emoji: '🍇', weight: 3,  multiplier: 3   },
  { emoji: '🍒', weight: 4,  multiplier: 2.5 },
  { emoji: '🍋', weight: 5,  multiplier: 2   },
  { emoji: '🍎', weight: 6,  multiplier: 1.5 },
];

const TOTAL_WEIGHT = SYMBOLS.reduce((s, sym) => s + sym.weight, 0);

function weightedRandom() {
  let r = Math.random() * TOTAL_WEIGHT;
  for (const sym of SYMBOLS) {
    r -= sym.weight;
    if (r <= 0) return sym;
  }
  return SYMBOLS[SYMBOLS.length - 1];
}

// Winning lines: 3 rows + 2 diagonals (indices into the flat 9-cell grid)
const LINES = [
  [0, 1, 2], // row 0
  [3, 4, 5], // row 1
  [6, 7, 8], // row 2
  [0, 4, 8], // diagonal ↘
  [2, 4, 6], // diagonal ↙
];

function checkWins(grid) {
  return LINES.filter(([a, b, c]) => grid[a].emoji === grid[b].emoji && grid[a].emoji === grid[c].emoji)
              .map(([a, b, c]) => ({ a, b, c, symbol: grid[a] }));
}

function renderGrid(grid, wins) {
  const rows = [];
  for (let r = 0; r < 3; r++) {
    const line  = [r * 3, r * 3 + 1, r * 3 + 2];
    const cells = line.map((i) => grid[i].emoji).join(' ');
    const isWin = wins.some((w) => w.a === line[0] && w.b === line[1] && w.c === line[2]);
    rows.push(`${cells}${isWin ? '  ◀ WIN' : ''}`);
  }

  const diagWins = [];
  if (wins.some((w) => w.a === 0 && w.b === 4 && w.c === 8)) diagWins.push('↘ Diagonal: WIN');
  if (wins.some((w) => w.a === 2 && w.b === 4 && w.c === 6)) diagWins.push('↙ Diagonal: WIN');

  return rows.join('\n') + (diagWins.length ? '\n' + diagWins.join('\n') : '');
}

export const data = new SlashCommandBuilder()
  .setName('slots')
  .setDescription('Spin the 3×3 slot machine!')
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

  const grid = Array.from({ length: 9 }, () => weightedRandom());
  const wins = checkWins(grid);

  const totalMultiplier = wins.reduce((s, w) => s + w.symbol.multiplier, 0);
  const payout          = Math.floor(bet * totalMultiplier);
  const newBalance      = await applyBet(userId, bet, payout);

  const gridText   = renderGrid(grid, wins);
  const resultText = payout > 0
    ? `🎉 You won **${payout}** credits! (**${totalMultiplier}×**)`
    : '❌ No match — better luck next time!';
  const color = payout > bet ? COLORS.SUCCESS : payout > 0 ? COLORS.INFO : COLORS.ERROR;

  const embed = new EmbedBuilder()
    .setColor(color)
    .setTitle('🎰 Slot Machine')
    .setDescription(`\`\`\`\n${gridText}\n\`\`\`\n${resultText}`)
    .addFields(
      { name: 'Bet',     value: `${bet} credits`,     inline: true },
      { name: 'Balance', value: `${account.balance} → ${newBalance} credits`, inline: true },
    )
    .setTimestamp();

  await interaction.reply({ embeds: [embed] });
}
