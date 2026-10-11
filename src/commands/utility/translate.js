import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  EMBED_MAX_PARTS, EMBED_PART_MAX, TRANSLATE_MAX_CHUNKS, TranslationError, cleanForTranslation,
  describeTranslationError, hasTranslatableText, isSupportedTarget, languageChoices, languageName,
  loadLanguageSupport, providerLabel, providerName, resolveLanguageInput, splitForEmbeds, suggestLanguage,
  translateText, translationPaused, warmLanguageSupport,
} from '../../services/translationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { applyGuildColor, errorEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone, respondAutocomplete } from '../../utils/interactionHelpers.js';
import { clampText } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = null; // core command, never gated
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
export const allowInDMs = true; // needs no guild, so it keeps working in DMs

export const data = new SlashCommandBuilder()
  .setName('translate')
  .setDescription('Translate text into another language.')
  .addStringOption((o) => o.setName('text').setDescription('The text to translate')
    .setRequired(true).setMaxLength(1500))
  .addStringOption((o) => o.setName('target_language')
    .setDescription('Target language code or name (e.g. es, ja, zh-tw). See /languages.')
    .setRequired(true).setMaxLength(40).setAutocomplete(true));

export async function autocomplete(interaction) {
  if (providerName() === 'libretranslate') warmLanguageSupport();
  await respondAutocomplete(interaction, languageChoices(interaction.options.getFocused()));
}

export async function execute(interaction) {
  const text = interaction.options.getString('text').trim();
  const input = interaction.options.getString('target_language');

  const target = resolveLanguageInput(input);
  if (!target) {
    const suggestion = suggestLanguage(input);
    const hint = suggestion ? ` Did you mean \`${suggestion}\` (${languageName(suggestion)})?` : '';
    return interaction.reply({
      embeds: [errorEmbed('Invalid Language', `\`${clampText(input, 40)}\` is not a language I know.${hint} Use \`/languages\` to see every code.`)],
      ephemeral: true,
    });
  }
  if (!hasTranslatableText(cleanForTranslation(text))) {
    return interaction.reply({ embeds: [errorEmbed('Nothing to Translate', 'That text has no words to translate.')], ephemeral: true });
  }
  if (translationPaused()) {
    return interaction.reply({ embeds: [describeTranslationError(new TranslationError('PAUSED')).embed], ephemeral: true });
  }

  await interaction.deferReply();

  await loadLanguageSupport(); // never throws
  if (isSupportedTarget(target) === false) {
    return interaction.editReply({
      embeds: [errorEmbed('Language Not Supported', `${languageName(target)} (\`${target}\`) isn't supported by this bot's translation backend (${providerLabel()}). Use \`/languages\` to see what is available.`)],
    });
  }

  // The network call is alone in the try, so a failure while building the reply is never reported as
  // "could not reach the service" (R105).
  let result;
  try {
    result = await translateText(text, target, { maxChunks: TRANSLATE_MAX_CHUNKS });
  } catch (err) {
    const d = describeTranslationError(err);
    if (d.log === 'warn') logger.warn(`/translate: ${err.code} ${err.status ?? ''} ${err.message}`);
    else if (d.log === 'error') logger.error('/translate failed', err);
    return interaction.editReply({ embeds: [d.embed] });
  }

  const parts = splitForEmbeds(result.text, EMBED_PART_MAX, EMBED_MAX_PARTS);
  if (!parts.length) {
    return interaction.editReply({ embeds: [errorEmbed('Translation Failed', 'The translation service returned an empty result.')] });
  }

  const settings = interaction.guildId ? await getSettingsSafe(interaction.guildId) : null;
  const first = applyGuildColor(
    infoEmbed(parts.length > 1 ? `Translation (1/${parts.length})` : 'Translation', parts[0]),
    settings,
  ).addFields(
    { name: 'Source', value: result.detected ? languageName(result.detected) : 'Unknown', inline: true },
    { name: 'Target', value: languageName(target), inline: true },
  );
  if (result.detected === target) {
    first.addFields({ name: 'Note', value: `The text already looks like ${languageName(target)}.` });
  }
  first.setFooter({ text: `Original: ${clampText(text.replace(/\s+/g, ' '), 100)}` });

  try {
    await interaction.editReply({ embeds: [first] });
  } catch (err) {
    if (isInteractionGone(err)) { logger.debug(`/translate: interaction gone (${err.code})`); return undefined; }
    throw err;
  }

  for (let i = 1; i < parts.length; i++) {
    try {
      await interaction.followUp({
        embeds: [applyGuildColor(infoEmbed(`Translation (${i + 1}/${parts.length})`, parts[i]), settings)],
        allowedMentions: { parse: [] },
      });
    } catch (err) {
      if (isInteractionGone(err)) logger.debug(`/translate: follow-up not sent, interaction gone (${err.code})`);
      else logger.warn(`/translate: follow-up failed: ${err?.message ?? err}`);
    }
  }
  return undefined;
}
