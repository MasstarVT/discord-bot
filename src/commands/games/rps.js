import { SlashCommandBuilder, ActionRowBuilder, ButtonBuilder, ButtonStyle, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { infoEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const activeGames = new Set();

const CHOICES = ['rock', 'paper', 'scissors'];
const EMOJI   = { rock: '🪨', paper: '📄', scissors: '✂️' };
// BEATS[x] = what x defeats
const BEATS   = { rock: 'scissors', paper: 'rock', scissors: 'paper' };

function buildRow(id, disabled = false) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(`rps:${id}:rock`).setLabel('🪨 Rock').setStyle(ButtonStyle.Primary).setDisabled(disabled),
    new ButtonBuilder().setCustomId(`rps:${id}:paper`).setLabel('📄 Paper').setStyle(ButtonStyle.Primary).setDisabled(disabled),
    new ButtonBuilder().setCustomId(`rps:${id}:scissors`).setLabel('✂️ Scissors').setStyle(ButtonStyle.Primary).setDisabled(disabled),
  );
}

export const data = new SlashCommandBuilder()
  .setName('rps')
  .setDescription('Play Rock, Paper, Scissors against the bot!');

export async function execute(interaction) {
  const userId = interaction.user.id;

  if (activeGames.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active RPS game!', ephemeral: true });
  }
  activeGames.add(userId);

  await interaction.reply({
    embeds: [infoEmbed('Rock, Paper, Scissors', 'Choose your move! You have **30 seconds**.')],
    components: [buildRow(interaction.id)],
  });

  const collector = interaction.channel.createMessageComponentCollector({ time: 30_000 });

  collector.on('collect', async (i) => {
    // Ignore buttons from other concurrent games in this channel
    if (!i.customId.startsWith(`rps:${interaction.id}:`)) return;

    if (i.user.id !== userId) {
      return i.reply({ content: "❌ This isn't your game!", ephemeral: true });
    }

    const playerChoice = i.customId.split(':')[2];
    const botChoice    = CHOICES[Math.floor(Math.random() * 3)];

    let title, color;
    if (playerChoice === botChoice) {
      title = "🤝 It's a draw!";
      color = COLORS.NEUTRAL;
    } else if (BEATS[playerChoice] === botChoice) {
      title = '🎉 You win!';
      color = COLORS.SUCCESS;
    } else {
      title = '😔 You lose!';
      color = COLORS.ERROR;
    }

    const embed = new EmbedBuilder()
      .setColor(color)
      .setTitle(title)
      .setDescription(`You: ${EMOJI[playerChoice]} **${playerChoice}**\nBot: ${EMOJI[botChoice]} **${botChoice}**`)
      .setTimestamp();

    await i.update({ embeds: [embed], components: [buildRow(interaction.id, true)] });
    collector.stop('done');
  });

  collector.on('end', async (_, reason) => {
    activeGames.delete(userId);
    if (reason === 'done') return;
    await interaction.editReply({
      embeds: [infoEmbed('Rock, Paper, Scissors', "⏰ Time's up! No move was made.")],
      components: [buildRow(interaction.id, true)],
    }).catch(() => {});
  });
}
