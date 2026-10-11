import { randomInt } from 'node:crypto';
import { SlashCommandBuilder, ActionRowBuilder, ButtonBuilder, ButtonStyle, ComponentType } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { buildCustomId, parseCustomId } from '../../config/customIds.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { errorEmbed, guildEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';

export const module = 'games';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

/** userId → true while the user has a game in progress. */
const activeGames = new Set();

export const CHOICES = ['rock', 'paper', 'scissors'];
export const EMOJI = { rock: '🪨', paper: '📄', scissors: '✂️' };
// BEATS[x] = what x defeats
export const BEATS = { rock: 'scissors', paper: 'rock', scissors: 'paper' };

/** @returns {'win'|'lose'|'draw'} the outcome for the player */
export function decide(player, bot) {
  if (player === bot) return 'draw';
  return BEATS[player] === bot ? 'win' : 'lose';
}

export function buildRow(id, disabled = false) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('rps', id, 'rock')).setLabel('🪨 Rock').setStyle(ButtonStyle.Primary).setDisabled(disabled),
    new ButtonBuilder().setCustomId(buildCustomId('rps', id, 'paper')).setLabel('📄 Paper').setStyle(ButtonStyle.Primary).setDisabled(disabled),
    new ButtonBuilder().setCustomId(buildCustomId('rps', id, 'scissors')).setLabel('✂️ Scissors').setStyle(ButtonStyle.Primary).setDisabled(disabled),
  );
}

export const data = new SlashCommandBuilder()
  .setName('rps')
  .setDescription('Play Rock, Paper, Scissors against the bot!');

const TITLE = '✂️ Rock, Paper, Scissors';

// Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
const loadSettings = (interaction) => peekSettings(interaction.guildId)
  ?? withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'rps settings').catch(() => null);

export async function execute(interaction) {
  const userId = interaction.user.id;

  if (activeGames.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active RPS game!', ephemeral: true });
  }
  const settings = await loadSettings(interaction);
  if (activeGames.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active RPS game!', ephemeral: true });
  }
  activeGames.add(userId);

  // R089: the collector's `end` releases the lock; release it here if anything throws before it exists.
  let handedOff = false;
  try {
    const response = await interaction.reply({
      embeds: [guildEmbed(settings, TITLE, 'Choose your move! You have **30 seconds**.')],
      components: [buildRow(interaction.id)],
    });

    // The filter checks only this game's prefix; the user is checked inside `collect`.
    const collector = response.createMessageComponentCollector({
      componentType: ComponentType.Button,
      filter: (i) => i.customId.startsWith(`rps:${interaction.id}:`),
      time: 30_000,
    });
    let played = false;

    collector.on('collect', async (i) => {
      try {
        if (i.user.id !== userId) {
          await i.reply({ content: "❌ This isn't your game!", ephemeral: true });
          return;
        }
        // R237: a second press before the first update lands is acknowledged and ignored.
        if (played) return void i.deferUpdate().catch(() => {});
        const choice = parseCustomId(i.customId).params[1];
        if (!CHOICES.includes(choice)) return void i.deferUpdate().catch(() => {});

        played = true;
        collector.stop('done'); // synchronously, before any await

        const bot = CHOICES[randomInt(CHOICES.length)];
        const description = `You: ${EMOJI[choice]} **${choice}**\nBot: ${EMOJI[bot]} **${bot}**`;
        const outcome = decide(choice, bot);
        const embed = outcome === 'win' ? successEmbed('You win!', description)
          : outcome === 'lose' ? errorEmbed('You lose!', description)
            : guildEmbed(settings, "🤝 It's a draw!", description);
        await i.update({ embeds: [embed], components: [buildRow(interaction.id, true)] });
      } catch (err) {
        if (isInteractionGone(err)) logger.debug('rps: interaction gone', err);
        else logger.error('RPS collect failed', err);
        if (i && !i.replied && !i.deferred) await i.deferUpdate().catch(() => {});
      }
    });

    collector.on('end', async (_collected, reason) => {
      try {
        activeGames.delete(userId);
        if (reason !== 'time') return;
        await interaction.editReply({
          embeds: [guildEmbed(settings, TITLE, "⏰ Time's up! No move was made.")],
          components: [buildRow(interaction.id, true)],
        }).catch(() => {});
      } catch (err) {
        logger.error('RPS end failed', err);
      }
    });

    handedOff = true;
  } finally {
    if (!handedOff) activeGames.delete(userId);
  }
}
