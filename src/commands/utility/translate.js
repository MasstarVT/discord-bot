import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  EMBED_MAX_PARTS, EMBED_PART_MAX, LANGUAGES, TRANSLATE_MAX_CHUNKS, TranslationError, cleanForTranslation,
  describeTranslationError, detectLanguage, getLanguages, hasTranslatableText, isSupportedTarget, languageChoices,
  languageName, loadLanguageSupport, providerLabel, providerName, resolveLanguageInput, splitForEmbeds, suggestLanguage,
  translateText, translationPaused, warmLanguageSupport,
} from '../../services/translationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { STAT_VIA, recordTranslation } from '../../services/translateStatsService.js';
import { applyGuildColor, errorEmbed, infoEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone, respondAutocomplete, safeReply } from '../../utils/interactionHelpers.js';
import { clampText } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = null; // core command, never gated
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
export const allowInDMs = true; // needs no guild, so it keeps working in DMs

export const data = new SlashCommandBuilder()
  .setName('translate')
  .setDescription('Translate text, detect its language or list the supported languages.')
  .addSubcommand((s) => s.setName('text').setDescription('Translate text into another language.')
    .addStringOption((o) => o.setName('text').setDescription('The text to translate')
      .setRequired(true).setMaxLength(1500))
    .addStringOption((o) => o.setName('target_language')
      .setDescription('Target language code or name (e.g. es, ja, zh-tw). See /translate languages.')
      .setRequired(true).setMaxLength(40).setAutocomplete(true)))
  .addSubcommand((s) => s.setName('detect').setDescription('Detect the language of a piece of text.')
    .addStringOption((o) => o.setName('text').setDescription('The text to analyse').setRequired(true).setMaxLength(1500)))
  .addSubcommand((s) => s.setName('languages').setDescription('List all supported translation language codes.'));

export async function autocomplete(interaction) {
  if (providerName() === 'libretranslate') warmLanguageSupport();
  await respondAutocomplete(interaction, languageChoices(interaction.options.getFocused()));
}

/** Replaces the public "thinking..." placeholder with a failure only the caller sees (the way /search does). */
async function replyFailure(interaction, embed) {
  let removed = false;
  try {
    await interaction.deleteReply();
    removed = true;
    await interaction.followUp({ embeds: [embed], ephemeral: true });
  } catch (err) {
    if (isInteractionGone(err)) { logger.debug(`/translate: private failure not sent, interaction gone (${err.code})`); return undefined; }
    logger.warn(`/translate: could not send the failure privately: ${err?.message ?? err}`);
    if (removed) return safeReply(interaction, { embeds: [embed], ephemeral: true });
    try {
      return await interaction.editReply({ embeds: [embed] });
    } catch (editErr) {
      if (!isInteractionGone(editErr)) logger.warn(`/translate: could not edit the reply either: ${editErr?.message ?? editErr}`);
      return undefined;
    }
  }
  return undefined;
}

async function executeText(interaction) {
  const text = interaction.options.getString('text').trim();
  const input = interaction.options.getString('target_language');

  const target = resolveLanguageInput(input);
  if (!target) {
    const suggestion = suggestLanguage(input);
    const hint = suggestion ? ` Did you mean \`${suggestion}\` (${languageName(suggestion)})?` : '';
    return interaction.reply({
      embeds: [errorEmbed('Invalid Language', `\`${clampText(input, 40)}\` is not a language I know.${hint} Use \`/translate languages\` to see every code.`)],
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
    return replyFailure(interaction, errorEmbed('Language Not Supported', `${languageName(target)} (\`${target}\`) isn't supported by this bot's translation backend (${providerLabel()}). Use \`/translate languages\` to see what is available.`));
  }

  // The network call is alone in the try, so a failure while building the reply is never reported as
  // "could not reach the service" (R105).
  let result;
  try {
    result = await translateText(text, target, { maxChunks: TRANSLATE_MAX_CHUNKS });
  } catch (err) {
    const d = describeTranslationError(err);
    if (d.log === 'warn') logger.warn(`/translate text: ${err.code} ${err.status ?? ''} ${err.message}`);
    else if (d.log === 'error') logger.error('/translate text failed', err);
    return replyFailure(interaction, d.embed);
  }

  const parts = splitForEmbeds(result.text, EMBED_PART_MAX, EMBED_MAX_PARTS);
  if (!parts.length) {
    return replyFailure(interaction, errorEmbed('Translation Failed', 'The translation service returned an empty result.'));
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
    if (interaction.guildId) {
      void recordTranslation({
        guildId: interaction.guildId, channelId: interaction.channelId, via: STAT_VIA.COMMAND,
        detected: result.detected, targets: result.detected === target ? [] : [target],
      });
    }
  } catch (err) {
    if (isInteractionGone(err)) { logger.debug(`/translate text: interaction gone (${err.code})`); return undefined; }
    throw err;
  }

  for (let i = 1; i < parts.length; i++) {
    try {
      await interaction.followUp({
        embeds: [applyGuildColor(infoEmbed(`Translation (${i + 1}/${parts.length})`, parts[i]), settings)],
        allowedMentions: { parse: [] },
      });
    } catch (err) {
      if (isInteractionGone(err)) logger.debug(`/translate text: follow-up not sent, interaction gone (${err.code})`);
      else logger.warn(`/translate text: follow-up failed: ${err?.message ?? err}`);
    }
  }
  return undefined;
}

async function executeDetect(interaction) {
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
    if (d.log === 'warn') logger.warn(`/translate detect: ${err.code} ${err.status ?? ''} ${err.message}`);
    else if (d.log === 'error') logger.error('/translate detect failed', err);
    return replyFailure(interaction, d.embed);
  }

  if (found.code === null) {
    return interaction.editReply({ embeds: [warnEmbed('Unknown Language', 'I could not tell which language that is. Try a longer sentence; it may also be a language the translator does not have (`/translate languages`).')] });
  }

  const settings = interaction.guildId ? await getSettingsSafe(interaction.guildId) : null;
  const embed = applyGuildColor(infoEmbed('Language Detected', `**${found.name}** (\`${found.code}\`)`), settings);
  if (!Object.hasOwn(LANGUAGES, found.code)) {
    embed.setFooter({ text: 'Detected only: this language is not available as a translation target.' });
  }
  return interaction.editReply({ embeds: [embed] });
}

async function executeLanguages(interaction) {
  await interaction.deferReply({ ephemeral: true }); // the LibreTranslate list may need a network call

  const support = await loadLanguageSupport(); // never throws; null for Google or when unknown
  const entries = Object.entries(getLanguages()).sort((a, b) => a[1].localeCompare(b[1]));
  const lines = entries.map(([code, name]) => `\`${code}\` — ${name}`);
  const settings = interaction.guildId ? await getSettingsSafe(interaction.guildId) : null;

  const embed = applyGuildColor(
    infoEmbed('Supported Languages', clampText(lines.join('\n'), 4096), `${entries.length} languages · backend: ${providerLabel()}`),
    settings,
  ).addFields({
    name: 'Also accepted',
    value: 'Aliases: `iw`→`he`, `nb`→`no`, `zh-cn`→`zh`, `zh-hant`→`zh-tw`, `fil`→`tl`. Names work too, e.g. `Japanese`.',
  });
  if (providerName() === 'libretranslate' && support === null) {
    embed.addFields({
      name: 'Note',
      value: "Could not load the LibreTranslate server's language list; some of these may be unavailable.",
    });
  }

  return interaction.editReply({ embeds: [embed] });
}

export async function execute(interaction) {
  switch (interaction.options.getSubcommand()) {
    case 'detect': return executeDetect(interaction);
    case 'languages': return executeLanguages(interaction);
    default: return executeText(interaction);
  }
}
