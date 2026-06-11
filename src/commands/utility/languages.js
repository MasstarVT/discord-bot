import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { getLanguages } from '../../services/translationService.js';
import { infoEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('languages')
  .setDescription('List all supported translation language codes.');

export async function execute(interaction) {
  const languages = getLanguages();
  const sorted = Object.entries(languages).sort((a, b) => a[1].localeCompare(b[1]));
  const lines = sorted.map(([code, name]) => `\`${code}\` — ${name}`);
  const description = lines.join('\n');

  return interaction.reply({
    embeds: [infoEmbed('Supported Languages', description, `${sorted.length} languages available`)],
    ephemeral: true,
  });
}
