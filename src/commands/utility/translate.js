import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { translate, getLanguages } from '../../services/translationService.js';
import { infoEmbed, errorEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('translate')
  .setDescription('Translate text into another language.')
  .addStringOption((o) =>
    o.setName('text').setDescription('The text to translate').setRequired(true).setMaxLength(1500)
  )
  .addStringOption((o) =>
    o.setName('target_language')
      .setDescription('Target language code (e.g. es, fr, de). Use /languages for a full list.')
      .setRequired(true)
      .setAutocomplete(true)
  );

export async function autocomplete(interaction) {
  const focused = interaction.options.getFocused().toLowerCase();
  const languages = getLanguages();
  const choices = Object.entries(languages)
    .filter(([code, name]) => code.startsWith(focused) || name.toLowerCase().startsWith(focused))
    .slice(0, 25)
    .map(([code, name]) => ({ name: `${name} (${code})`, value: code }));
  await interaction.respond(choices);
}

export async function execute(interaction) {
  await interaction.deferReply();

  const text = interaction.options.getString('text');
  const targetLang = interaction.options.getString('target_language').toLowerCase().trim();

  const languages = getLanguages();
  if (!languages[targetLang]) {
    return interaction.editReply({
      embeds: [errorEmbed('Invalid Language', `\`${targetLang}\` is not a supported language code. Use \`/languages\` to see all available codes.`)],
    });
  }

  try {
    const { translatedText, sourceName, targetName } = await translate(text, targetLang);
    const embed = infoEmbed('Translation', translatedText)
      .addFields(
        { name: 'Source', value: sourceName, inline: true },
        { name: 'Target', value: targetName, inline: true },
      )
      .setFooter({ text: `Original: ${text.length > 100 ? text.slice(0, 97) + '…' : text}` });

    return interaction.editReply({ embeds: [embed] });
  } catch (err) {
    return interaction.editReply({
      embeds: [errorEmbed('Translation Failed', 'Could not reach the translation service. Please try again later.')],
    });
  }
}
