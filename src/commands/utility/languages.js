import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { getLanguages, loadLanguageSupport, providerLabel, providerName } from '../../services/translationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { applyGuildColor, infoEmbed } from '../../utils/embedBuilder.js';
import { clampText } from '../../utils/text.js';

export const module = null; // core command, never gated
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
export const allowInDMs = true; // needs no guild, so it keeps working in DMs

export const data = new SlashCommandBuilder()
  .setName('languages')
  .setDescription('List all supported translation language codes.');

export async function execute(interaction) {
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
