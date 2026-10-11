import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  WelcomeConfigError, buildSetupModal, buildTestPreview, checkConfigChannel, diagnoseWelcome,
  messageInfoEmbed, messageUpdateEmbed, saveSettings, settingsForAck, updateEmbedOptions, updateMessage,
  describeEmbedSettings,
} from '../../services/welcomeService.js';
import { getSettings } from '../../services/settingsService.js';
import { errorEmbed, successEmbed } from '../../utils/embedBuilder.js';
import logger from '../../utils/logger.js';

export const module = 'welcome';
export const gated = false; // configuration works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const TXT = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const thumb = (o) => o.setName('thumbnail').setDescription('Small corner image')
  .addChoices({ name: 'Member avatar', value: 'avatar' }, { name: 'Server icon', value: 'server' }, { name: 'None', value: 'none' });

export const data = new SlashCommandBuilder()
  .setName('goodbye')
  .setDescription('Goodbye message when a member leaves (MEE6 Goodbye message).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('enable').setDescription('Post a goodbye message when members leave.'))
  .addSubcommand((s) => s.setName('disable').setDescription('Stop goodbye messages.'))
  .addSubcommand((s) => s.setName('channel').setDescription('Set the goodbye channel.')
    .addChannelOption((o) => o.setName('channel').setDescription('Goodbye channel').setRequired(true).addChannelTypes(...TXT)))
  .addSubcommand((s) => s.setName('message').setDescription('Set the goodbye text (mentions do not ping).')
    .addStringOption((o) => o.setName('text').setDescription('Text, e.g. **{user.idname}** just left the server').setMaxLength(2000))
    .addBooleanOption((o) => o.setName('reset').setDescription('Use the default text')))
  .addSubcommand((s) => s.setName('embed').setDescription('Send the goodbye as text or as an embed.')
    .addBooleanOption((o) => o.setName('enabled').setDescription('true = embed, false = text only'))
    .addStringOption(thumb)
    .addBooleanOption((o) => o.setName('reset').setDescription('Use the default embed')))
  .addSubcommand((s) => s.setName('setup').setDescription('Edit the goodbye text and embed in a form.'))
  .addSubcommand((s) => s.setName('test').setDescription('Preview the goodbye message here.'));

const unavailable = () => errorEmbed('Temporarily Unavailable', 'Settings could not be loaded. Try again in a moment.');

// Lines about the channel only: the module and sub-toggle lines have their own wording here.
const channelWarnings = (guild, settings) => diagnoseWelcome(guild, settings, 'goodbye')
  .filter((line) => !/module is off|goodbye message is off/.test(line));

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  const { guild, guildId } = interaction;

  // The forms and the preview answer within the acknowledgement budget, so settings come from the memo
  // or a bounded lookup (rule 21), before any defer.
  if (sub === 'setup' || sub === 'test') {
    const settings = await settingsForAck(interaction);
    if (!settings) return interaction.reply({ embeds: [unavailable()], ephemeral: true });
    if (sub === 'setup') return interaction.showModal(buildSetupModal('goodbye', settings));
    return buildTestPreview(interaction, settings, 'goodbye');
  }

  await interaction.deferReply();
  try {
    let settings;
    try {
      settings = await getSettings(guildId);
    } catch (err) {
      logger.warn(`/goodbye could not load settings for ${guildId}: ${err?.message}`);
      return await interaction.editReply({ embeds: [unavailable()] });
    }

    if (sub === 'enable') {
      const after = await saveSettings(guildId, { goodbyeEnabled: true });
      const warnings = [];
      if (!after.welcomeEnabled) warnings.push('⚠️ The Welcome & Goodbye module is off. Nothing is posted until an admin runs `/welcome enable`.');
      warnings.push(...channelWarnings(guild, after));
      return await interaction.editReply({
        embeds: [successEmbed('Goodbye Enabled', ['A message is posted when a member leaves.', ...warnings].join('\n'))],
      });
    }

    if (sub === 'disable') {
      await saveSettings(guildId, { goodbyeEnabled: false });
      return await interaction.editReply({ embeds: [successEmbed('Goodbye Disabled', 'No goodbye messages will be posted.')] });
    }

    if (sub === 'channel') {
      const picked = interaction.options.getChannel('channel', true);
      const needs = ['ViewChannel', 'SendMessages', ...(settings.goodbyeUseEmbed ? ['EmbedLinks'] : [])];
      const check = await checkConfigChannel(guild, picked, needs);
      if (!check.ok) return await interaction.editReply({ embeds: [errorEmbed('Cannot Use That Channel', check.message)] });
      const after = await saveSettings(guildId, { goodbyeChannelId: picked.id });
      const lines = [`Goodbye messages will be posted in <#${picked.id}>.`];
      if (!after.welcomeEnabled) lines.push('⚠️ The Welcome & Goodbye module is off (`/welcome enable`).');
      if (!after.goodbyeEnabled) lines.push('⚠️ The goodbye message is off (`/goodbye enable`).');
      return await interaction.editReply({ embeds: [successEmbed('Goodbye Channel Set', lines.join('\n'))] });
    }

    if (sub === 'message') {
      const text = interaction.options.getString('text');
      const reset = interaction.options.getBoolean('reset') === true;
      if (text === null && !reset) {
        return await interaction.editReply({ embeds: [messageInfoEmbed('Goodbye Text', 'goodbye', settings)] });
      }
      const after = await updateMessage(guildId, 'goodbye', { text, reset });
      const ctx = { member: interaction.member, guild, channel: interaction.channel };
      return await interaction.editReply({
        embeds: [messageUpdateEmbed('Goodbye Text Updated', 'goodbye', after, { text, reset }, ctx)],
      });
    }

    if (sub === 'embed') {
      const enabled = interaction.options.getBoolean('enabled');
      const thumbnail = interaction.options.getString('thumbnail');
      const reset = interaction.options.getBoolean('reset') === true;
      const after = await updateEmbedOptions(guildId, 'goodbye', { enabled, thumbnail, reset });
      return await interaction.editReply({
        embeds: [successEmbed('Goodbye Embed Updated', describeEmbedSettings('goodbye', after))],
      });
    }
  } catch (err) {
    if (err instanceof WelcomeConfigError) {
      return interaction.editReply({ embeds: [errorEmbed(err.title ?? 'Not Saved', err.message)] });
    }
    throw err;
  }
  return undefined;
}
