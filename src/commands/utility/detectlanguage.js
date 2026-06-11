import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { detectLanguage } from '../../services/translationService.js';
import { infoEmbed, errorEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('detectlanguage')
  .setDescription('Detect the language of a piece of text.')
  .addStringOption((o) =>
    o.setName('text').setDescription('The text to analyse').setRequired(true).setMaxLength(1500)
  );

export async function execute(interaction) {
  await interaction.deferReply();

  const text = interaction.options.getString('text');

  try {
    const { code, name } = await detectLanguage(text);
    return interaction.editReply({
      embeds: [infoEmbed('Language Detected', `**${name}** (\`${code}\`)`)],
    });
  } catch {
    return interaction.editReply({
      embeds: [errorEmbed('Detection Failed', 'Could not reach the translation service. Please try again later.')],
    });
  }
}
