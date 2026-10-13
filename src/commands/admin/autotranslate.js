import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import prisma from '../../database/client.js';
import { ensureGuildSettings, getSettings, updateSettings } from '../../services/settingsService.js';
import {
  getGuildTranslateConfig, isSupportedTarget, languageChoices, languageName, loadLanguageSupport, normalizeTargetList,
  providerLabel, providerName, refreshTranslateCache, suggestLanguage, warmLanguageSupport,
} from '../../services/translationService.js';
import { errorEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { checkChannelUsable, respondAutocomplete } from '../../utils/interactionHelpers.js';
import { clampText, joinWithLimit } from '../../utils/text.js';
import { newChannelOption, resolveNewOptions } from '../../utils/newTargets.js';
import logger from '../../utils/logger.js';

export const module = 'translation';
export const gated = false; // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const LANG_OPTIONS = ['language1', 'language2', 'language3', 'language4', 'language5'];
const CHANNEL_TYPES = [
  ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildVoice, ChannelType.GuildStageVoice,
  ChannelType.PublicThread, ChannelType.PrivateThread, ChannelType.AnnouncementThread,
];
const STALE_CHECKS_PER_RUN = 10;

export const data = new SlashCommandBuilder()
  .setName('autotranslate')
  .setDescription('Set up auto-translation for a channel (up to 5 target languages).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addBooleanOption((o) => o.setName('enable')
    .setDescription('Turn auto-translation on or off for the channel.').setRequired(true))
  .addStringOption((o) => o.setName('language1')
    .setDescription('Target language code or name (e.g. es, ja, zh-tw). Not needed to turn off.')
    .setMaxLength(40).setAutocomplete(true)) // not required, so turning it off needs no language (R209/R210)
  .addStringOption((o) => o.setName('language2')
    .setDescription('Another target language (optional).').setMaxLength(40).setAutocomplete(true))
  .addStringOption((o) => o.setName('language3')
    .setDescription('Another target language (optional).').setMaxLength(40).setAutocomplete(true))
  .addStringOption((o) => o.setName('language4')
    .setDescription('Another target language (optional).').setMaxLength(40).setAutocomplete(true))
  .addStringOption((o) => o.setName('language5')
    .setDescription('Another target language (optional).').setMaxLength(40).setAutocomplete(true))
  .addChannelOption((o) => o.setName('channel')
    .setDescription('Channel to configure (default: this channel).').addChannelTypes(...CHANNEL_TYPES))
  .addStringOption((o) => newChannelOption(o));

export async function autocomplete(interaction) {
  if (providerName() === 'libretranslate') warmLanguageSupport();
  await respondAutocomplete(interaction, languageChoices(interaction.options.getFocused()));
}

/**
 * The "Auto-translate channels (N)" field. Rows of channels Discord reports as deleted (10003) are
 * removed here, the only place such rows can be noticed (a deleted channel cannot be picked). A cache
 * miss alone never deletes anything (rule 20). Never throws; null when it cannot be built.
 */
async function channelsField(interaction) {
  try {
    const { guild, guildId } = interaction;
    let config = await getGuildTranslateConfig(guildId);
    if (guild?.available) {
      const stale = [...config.keys()].filter((id) => !guild.channels.cache.has(id)).slice(0, STALE_CHECKS_PER_RUN);
      let removed = 0;
      for (const channelId of stale) {
        try {
          await guild.channels.fetch(channelId);
        } catch (err) {
          if (err?.code === 10003) {
            await prisma.translateChannel.deleteMany({ where: { guildId, channelId } });
            removed++;
          }
        }
      }
      if (removed) {
        await refreshTranslateCache(guildId);
        config = await getGuildTranslateConfig(guildId);
      }
    }
    const lines = [...config].map(([channelId, langs]) => `<#${channelId}> → ${langs.join(', ')}`);
    return {
      name: `Auto-translate channels (${lines.length})`,
      value: joinWithLimit(lines, { sep: '\n', max: 1024 }) || 'None',
    };
  } catch (err) {
    logger.warn(`autotranslate: could not list the configured channels: ${err?.message ?? err}`);
    return null;
  }
}

export async function execute(interaction) {
  await interaction.deferReply();
  if (!(await resolveNewOptions(interaction, data))) return undefined;

  const { guildId } = interaction;
  const channel = interaction.options.getChannel('channel') ?? interaction.channel;
  if (!channel || !CHANNEL_TYPES.includes(channel.type)) {
    return interaction.editReply({
      embeds: [errorEmbed('Unsupported Channel', 'Auto-translation works in text, announcement, voice/stage chat and thread channels.')],
    });
  }
  const enable = interaction.options.getBoolean('enable');

  try {
    // ── Disable: the language options are ignored entirely, never validated ─────
    if (!enable) {
      const { count } = await prisma.translateChannel.deleteMany({ where: { guildId, channelId: channel.id } });
      await refreshTranslateCache(guildId);
      const embed = count === 0
        ? warnEmbed('Not Enabled', `Auto-translation is not turned on in <#${channel.id}>.`)
        : successEmbed('Auto-Translation Disabled', `Auto-translation is now off in <#${channel.id}>.`);
      // The module flag belongs to /module: with no channels left the hook only does a memo lookup.
      const field = await channelsField(interaction);
      if (field) embed.addFields(field);
      return interaction.editReply({ embeds: [embed] });
    }

    // ── Enable ───────────────────────────────────────────────────────────────────
    const inputs = LANG_OPTIONS.map((name) => interaction.options.getString(name)).filter((v) => v && v.trim());
    if (!inputs.length) {
      return interaction.editReply({
        embeds: [errorEmbed('No Languages', 'Pick at least one target language (language1–language5) to turn auto-translation on.')],
      });
    }

    const { valid, invalid } = normalizeTargetList(inputs);
    if (invalid.length) {
      const lines = invalid.map((input) => {
        const suggestion = suggestLanguage(input);
        return `• \`${clampText(String(input).trim(), 40)}\` is not a language I know.${suggestion ? ` Did you mean \`${suggestion}\` (${languageName(suggestion)})?` : ''}`;
      });
      lines.push('Use `/languages` to see every code.');
      return interaction.editReply({ embeds: [errorEmbed('Invalid Language', clampText(lines.join('\n'), 4096))] });
    }

    // Backend support (R174/R175): null = unknown or Google.
    const support = await loadLanguageSupport();
    const unsupported = valid.filter((code) => isSupportedTarget(code) === false);
    const targets = valid.filter((code) => isSupportedTarget(code) !== false);
    if (!targets.length) {
      return interaction.editReply({
        embeds: [errorEmbed('Language Not Supported', `None of those languages are supported by this bot's translation backend (${providerLabel()}). Use \`/languages\` to see the supported list.`)],
      });
    }

    // Replies need Read Message History. checkChannelUsable's reason already names the channel.
    const usable = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks', 'ReadMessageHistory'] });
    if (!usable.ok) {
      return interaction.editReply({
        embeds: [errorEmbed('Missing Permissions', `I can't post translations in <#${channel.id}>. ${usable.reason ?? `I need **${usable.missing.join(', ')}** there.`}`)],
      });
    }

    const existing = await prisma.translateChannel.findUnique({
      where: { guildId_channelId: { guildId, channelId: channel.id } },
      select: { id: true },
    });
    if (!existing) {
      const n = await prisma.translateChannel.count({ where: { guildId } });
      if (n >= LIMITS.TRANSLATE_CHANNELS) {
        return interaction.editReply({
          embeds: [errorEmbed('Limit Reached', `This server already has ${n} auto-translate channels (max ${LIMITS.TRANSLATE_CHANNELS}). Turn one off first.`)],
        });
      }
    }

    await ensureGuildSettings(guildId); // the parent row first (R213)
    await prisma.translateChannel.upsert({
      where: { guildId_channelId: { guildId, channelId: channel.id } },
      update: { targetLangs: targets },
      create: { guildId, channelId: channel.id, targetLangs: targets },
    });

    let moduleTurnedOn = false;
    const settings = await getSettings(guildId);
    if (settings.translationEnabled !== true) {
      await updateSettings(guildId, { translationEnabled: true });
      moduleTurnedOn = true;
    }
    await refreshTranslateCache(guildId);

    const list = targets.map((code) => `${languageName(code)} (\`${code}\`)`).join(', ');
    const embed = successEmbed('Auto-Translation Enabled', clampText(`Messages in <#${channel.id}> will be translated to: ${list}.`, 4096));
    if (unsupported.length) {
      embed.addFields({
        name: 'Skipped',
        value: joinWithLimit(unsupported.map((code) => `\`${code}\``), { max: 900 }) + ` — not supported by ${providerLabel()}.`,
      });
    }
    if (support === null && providerName() === 'libretranslate') {
      embed.addFields({
        name: 'Note',
        value: 'Could not check which languages the LibreTranslate server supports; unsupported ones will be skipped.',
      });
    }
    if (moduleTurnedOn) {
      embed.addFields({ name: 'Module', value: 'The **Auto-Translate** module was off and is now on.' });
    }
    const field = await channelsField(interaction);
    if (field) embed.addFields(field);
    return interaction.editReply({ embeds: [embed] });
  } catch (err) {
    logger.error('autotranslate failed', err);
    return interaction.editReply({
      embeds: [errorEmbed('Something Went Wrong', 'Could not save the auto-translate settings. Please try again.')],
    }).catch(() => {});
  }
}
