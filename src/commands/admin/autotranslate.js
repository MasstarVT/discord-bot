import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { getLanguages, invalidateTranslateCache } from '../../services/translationService.js';
import { successEmbed, errorEmbed } from '../../utils/embedBuilder.js';
import prisma from '../../database/client.js';
import { cache } from '../../services/redis.js';

export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const LANG_OPTIONS = ['language1', 'language2', 'language3', 'language4', 'language5'];

export const data = new SlashCommandBuilder()
  .setName('autotranslate')
  .setDescription('Configure auto-translation for this channel.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addBooleanOption((o) =>
    o.setName('enable').setDescription('Enable or disable auto-translation for this channel.').setRequired(true)
  )
  .addStringOption((o) =>
    o.setName('language1').setDescription('Target language to translate messages into.').setRequired(true).setAutocomplete(true)
  )
  .addStringOption((o) =>
    o.setName('language2').setDescription('Additional target language (optional).').setAutocomplete(true)
  )
  .addStringOption((o) =>
    o.setName('language3').setDescription('Additional target language (optional).').setAutocomplete(true)
  )
  .addStringOption((o) =>
    o.setName('language4').setDescription('Additional target language (optional).').setAutocomplete(true)
  )
  .addStringOption((o) =>
    o.setName('language5').setDescription('Additional target language (optional).').setAutocomplete(true)
  );

export async function autocomplete(interaction) {
  const focused = interaction.options.getFocused().toLowerCase();
  const languages = getLanguages();

  const choices = Object.entries(languages)
    .filter(([code, name]) => !focused || code.startsWith(focused) || name.toLowerCase().startsWith(focused))
    .slice(0, 25)
    .map(([code, name]) => ({ name: `${name} (${code})`, value: code }));

  await interaction.respond(choices);
}

export async function execute(interaction) {
  await interaction.deferReply();

  const { guildId, channelId } = interaction;
  const enable = interaction.options.getBoolean('enable');

  const langs = LANG_OPTIONS
    .map((name) => interaction.options.getString(name)?.toLowerCase().trim())
    .filter(Boolean);

  const languages = getLanguages();
  const invalid = langs.filter((l) => !languages[l]);
  if (invalid.length > 0) {
    return interaction.editReply({
      embeds: [errorEmbed('Invalid Language', `Unknown code(s): ${invalid.map((l) => `\`${l}\``).join(', ')}. Use \`/languages\` to see all valid codes.`)],
    });
  }

  if (!enable) {
    await prisma.translateChannel.deleteMany({ where: { guildId, channelId } });

    const remaining = await prisma.translateChannel.count({ where: { guildId } });
    if (remaining === 0) {
      await prisma.guildSettings.upsert({
        where:  { guildId },
        update: { translationEnabled: false },
        create: { guildId, translationEnabled: false },
      });
      await cache.del(`guild:${guildId}:settings`);
    }

    await invalidateTranslateCache(guildId);
    return interaction.editReply({
      embeds: [successEmbed('Auto-Translation Disabled', `Auto-translation has been turned off for <#${channelId}>.`)],
    });
  }

  if (langs.length === 0) {
    return interaction.editReply({
      embeds: [errorEmbed('No Languages', 'Provide at least one target language when enabling auto-translation.')],
    });
  }

  await prisma.translateChannel.upsert({
    where:  { guildId_channelId: { guildId, channelId } },
    update: { targetLangs: langs },
    create: { guildId, channelId, targetLangs: langs },
  });

  await prisma.guildSettings.upsert({
    where:  { guildId },
    update: { translationEnabled: true },
    create: { guildId, translationEnabled: true },
  });

  await cache.del(`guild:${guildId}:settings`);
  await invalidateTranslateCache(guildId);

  const langList = langs.map((l) => `${languages[l]} (\`${l}\`)`).join(', ');
  return interaction.editReply({
    embeds: [successEmbed('Auto-Translation Enabled', `Messages in <#${channelId}> will now be automatically translated to: ${langList}`)],
  });
}
