import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { errorEmbed, successEmbed } from '../../utils/embedBuilder.js';
import {
  activeGames, validatePlay, applyPlay, checkGameOver,
  buildGameEmbed, RACK_ROW, startTurnTimer, endGame, TURN_TIMEOUT_MS,
} from '../../services/scrabbleService.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const COLS = 'ABCDEFGHIJKLMNO';

export const data = new SlashCommandBuilder()
  .setName('scrabble-play')
  .setDescription('Play a word in your active Scrabble game.')
  .addStringOption((o) =>
    o.setName('word').setDescription('Word to play (letters only)').setRequired(true).setMaxLength(15)
  )
  .addIntegerOption((o) =>
    o.setName('row').setDescription('Starting row (1–15)').setRequired(true).setMinValue(1).setMaxValue(15)
  )
  .addStringOption((o) =>
    o.setName('col').setDescription('Starting column (A–O)').setRequired(true).setMaxLength(1)
  )
  .addStringOption((o) =>
    o.setName('direction')
      .setDescription('Direction to place the word')
      .setRequired(true)
      .addChoices(
        { name: 'Horizontal', value: 'h' },
        { name: 'Vertical',   value: 'v' },
      )
  );

export async function execute(interaction) {
  const channelId = interaction.channelId;
  const game      = activeGames.get(channelId);

  if (!game)
    return interaction.reply({ embeds: [errorEmbed('No Active Game', 'There is no Scrabble game running in this channel.')], ephemeral: true });

  const word     = interaction.options.getString('word');
  const row      = interaction.options.getInteger('row');
  const rawCol   = interaction.options.getString('col').toUpperCase();
  const dir      = interaction.options.getString('direction');

  const col0 = COLS.indexOf(rawCol);
  if (col0 === -1)
    return interaction.reply({ embeds: [errorEmbed('Invalid Column', `**"${rawCol}"** is not a valid column. Use A–O.`)], ephemeral: true });

  const row0  = row - 1; // 0-indexed
  const valid = validatePlay(game, interaction.user.id, word, row0, col0, dir);

  if (!valid.valid)
    return interaction.reply({ embeds: [errorEmbed('Invalid Move', valid.error)], ephemeral: true });

  await interaction.deferReply({ ephemeral: true });

  const { wordScore, bingo } = applyPlay(game, interaction.user.id, word, row0, col0, dir, valid.newTiles);

  // Reset the turn timer
  clearTimeout(game.turnTimer);
  game.turnTimer = startTurnTimer(channelId, interaction.channel);

  // Update the board message
  if (game.gameMessage) {
    await game.gameMessage.edit({ embeds: [buildGameEmbed(game)], components: [RACK_ROW] }).catch(() => {});
  }

  // Confirm to the player
  const bonusText = bingo ? ' +50 bonus (bingo!) 🎉' : '';
  await interaction.editReply({
    embeds: [successEmbed('Word Played!', `**${word.toUpperCase()}** scored **${wordScore}** pts${bonusText}.`)],
  });

  // Check for game over (bag empty + previous player emptied their rack)
  const overEmbed = checkGameOver(game);
  if (overEmbed) {
    if (game.gameMessage) {
      await game.gameMessage.edit({ embeds: [buildGameEmbed(game)], components: [] }).catch(() => {});
    }
    await interaction.channel.send({ embeds: [overEmbed] }).catch(() => {});
    endGame(channelId);
  }
}
