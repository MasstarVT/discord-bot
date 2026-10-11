import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  LANGUAGES, TranslationError, cleanForTranslation, describeTranslationError, detectLanguage,
  hasTranslatableText, translationPaused,
} from '../../services/translationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { applyGuildColor, errorEmbed, infoEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import logger from '../../utils/logger.js';

export const module = null; // core command, never gated
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
export const allowInDMs = true; // needs no guild, so it keeps working in DMs

export const data = new SlashCommandBuilder()
  .setName('detectlanguage')
  .setDescription('Detect the language of a piece of text.')
  .addStringOption((o) => o.setName('text').setDescription('The text to analyse').setRequired(true).setMaxLength(1500));

export async function execute(interaction) {
  const text = interaction.options.getString('text');

  if (!hasTranslatableText(cleanForTranslation(text))) {
    return interaction.reply({ embeds: [errorEmbed('Nothing to Detect', 'That text has no words I can analyse.')], ephemeral: true });
  }
  if (translationPaused()) {
    return interaction.reply({ embeds: [describeTranslationError(new TranslationError('PAUSED')).embed], ephemeral: true });
  }

  await interaction.deferReply();

  let found;
  try {
    found = await detectLanguage(text);
  } catch (err) {
    const d = describeTranslationError(err);
    if (d.log === 'warn') logger.warn(`/detectlanguage: ${err.code} ${err.status ?? ''} ${err.message}`);
    else if (d.log === 'error') logger.error('/detectlanguage failed', err);
    return interaction.editReply({ embeds: [d.embed] });
  }

  if (found.code === null) {
    return interaction.editReply({ embeds: [warnEmbed('Unknown Language', 'I could not tell which language that is. Try a longer sentence.')] });
  }

  const settings = interaction.guildId ? await getSettingsSafe(interaction.guildId) : null;
  const embed = applyGuildColor(infoEmbed('Language Detected', `**${found.name}** (\`${found.code}\`)`), settings);
  if (!Object.hasOwn(LANGUAGES, found.code)) {
    embed.setFooter({ text: 'Detected only: this language is not available as a translation target.' });
  }
  return interaction.editReply({ embeds: [embed] });
}
