// /autothread add | remove | list | publish add | publish remove. Admin config command: replies are public (CLAUDE.md
// admin convention) and it keeps working while the Auto Threads module is off (gated = false). The behaviour is
// src/plugins/auto-threads.js; the rules and the lists are src/services/autoThreadService.js.
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import {
  AUTOTHREAD, AutoThreadError, PUBLISH_NEEDS, addPublishChannel, missingNames, readEntries, readPublishIds,
  removePublishChannel, removeThreadChannel, setThreadChannel, threadNeeds, unknownTokens,
} from '../../services/autoThreadService.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import { clampText, joinWithLimit } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = 'autothreads';
export const gated = false;                          // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const THREAD_CHANNELS = [ChannelType.GuildText, ChannelType.GuildAnnouncement];

export const data = new SlashCommandBuilder()
  .setName('autothread')
  .setDescription('Start a thread on every message in a channel, and publish announcement posts.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('add').setDescription('Start a thread on every member message in a channel.')
    .addChannelOption((o) => o.setName('channel').setDescription('The channel.').setRequired(true).addChannelTypes(...THREAD_CHANNELS))
    .addStringOption((o) => o.setName('name').setDescription('Thread name: {user} {content} {channel} {date} (default {content}).')
      .setMinLength(1).setMaxLength(AUTOTHREAD.NAME_MAX))
    .addStringOption((o) => o.setName('archive').setDescription('Archive a thread after this long without messages (default 1 day).')
      .addChoices(...AUTOTHREAD.ARCHIVE_CHOICES.map((value) => ({ name: AUTOTHREAD.ARCHIVE_LABELS[value], value }))))
    .addBooleanOption((o) => o.setName('close_button').setDescription('Post a Close button in each thread (default off).')))
  .addSubcommand((s) => s.setName('remove').setDescription('Stop starting threads in a channel.')
    .addChannelOption((o) => o.setName('channel').setDescription('The channel.').setRequired(true).addChannelTypes(...THREAD_CHANNELS)))
  .addSubcommand((s) => s.setName('list').setDescription('Show the auto-thread and auto-publish channels.'))
  .addSubcommandGroup((g) => g.setName('publish').setDescription('Publish announcement posts to the servers that follow them.')
    .addSubcommand((s) => s.setName('add').setDescription('Publish every new post in an announcement channel.')
      .addChannelOption((o) => o.setName('channel').setDescription('The announcement channel.').setRequired(true).addChannelTypes(ChannelType.GuildAnnouncement)))
    .addSubcommand((s) => s.setName('remove').setDescription('Stop publishing posts in an announcement channel.')
      .addChannelOption((o) => o.setName('channel').setDescription('The announcement channel.').setRequired(true).addChannelTypes(ChannelType.GuildAnnouncement))));

const MSG = {
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'),
  NOT_LISTED: (channelId) => errorEmbed('Not In The List', `<#${channelId}> is not in that list.`),
  BAD_CHANNEL: (what) => errorEmbed('Wrong Channel Type', `Pick ${what}.`),
};

const OFF_NOTE = '⚠️ The **Auto Threads** module is off, so nothing happens yet. Turn it on with `/module enable autothreads`.';

/** Final answer: overwrites the deferred reply. Never pings. */
function finish(interaction, embed) {
  return safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
}

/** The option's channel as the guild's cached channel, or null. */
function pickedChannel(interaction) {
  const option = interaction.options.getChannel('channel', true);
  return interaction.guild.channels.cache.get(option.id) ?? null;
}

/** Notes appended to an answer: what is missing and whether the module is off. */
function notes({ settings, problems = [] }) {
  const out = problems.map((p) => `⚠️ ${p}`);
  if (settings && !isModuleEnabled(settings, 'autothreads')) out.push(OFF_NOTE);
  return out.length ? `\n\n${out.join('\n')}` : '';
}

const shown = (text) => clampText(String(text ?? '').replace(/[`\\]/g, '').replace(/\s+/g, ' ').trim(), 60) || '(empty)';

/** What the bot lacks for a thread channel, as a sentence ('' = nothing). */
function threadProblem(channel, entry) {
  if (!channel) return 'I cannot see that channel.';
  const missing = missingNames(channel, threadNeeds(entry));
  return missing.length ? `I need **${missing.join(', ')}** in <#${channel.id}> to start threads there.` : '';
}

/** What the bot lacks to publish in an announcement channel, as a sentence ('' = nothing). */
function publishProblem(channel) {
  if (!channel) return 'I cannot see that channel.';
  if (channel.type !== ChannelType.GuildAnnouncement) return `<#${channel.id}> is no longer an announcement channel.`;
  const missing = missingNames(channel, PUBLISH_NEEDS);
  return missing.length ? `I need **${missing.join(', ')}** in <#${channel.id}> to publish posts.` : '';
}

// ── /autothread add | remove ──────────────────────────────────────────────────

async function runAdd(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const channel = pickedChannel(interaction);
  if (!channel || !THREAD_CHANNELS.includes(channel.type)) return finish(interaction, MSG.BAD_CHANNEL('a text or announcement channel'));
  const name = interaction.options.getString('name')?.trim() || null;
  if (name) {
    const unknown = unknownTokens(name);
    if (unknown.length) {
      return finish(interaction, errorEmbed('Unknown Placeholder', `${unknown.slice(0, 3).map((t) => `\`${shown(t)}\``).join(', ')} is not a placeholder. Use \`{user}\`, \`{content}\`, \`{channel}\` or \`{date}\`.`));
    }
  }
  let result;
  try {
    result = await setThreadChannel(interaction.guildId, {
      channelId: channel.id,
      name,
      archiveMinutes: interaction.options.getString('archive'),
      closeButton: interaction.options.getBoolean('close_button'),
    });
  } catch (err) {
    if (err instanceof AutoThreadError) return finish(interaction, errorEmbed(err.title, err.message));
    logger.error(`[auto-threads] ${interaction.guildId}: add failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  const { entry, created, settings } = result;
  logger.info(`[auto-threads] ${interaction.guildId}: ${channel.id} ${created ? 'added' : 'updated'} by ${interaction.user.id}`);
  const lines = [
    `Every member message in <#${channel.id}> gets its own thread.`,
    `Name: \`${shown(entry.name)}\` · archives after ${AUTOTHREAD.ARCHIVE_LABELS[entry.archiveMinutes]} · close button ${entry.closeButton ? 'on' : 'off'}`,
  ];
  const note = notes({ settings, problems: [threadProblem(channel, entry)].filter(Boolean) });
  return finish(interaction, successEmbed(created ? 'Auto Thread Added' : 'Auto Thread Updated', `${lines.join('\n')}${note}`));
}

async function runRemove(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const channelId = interaction.options.getChannel('channel', true).id;
  let found;
  try {
    found = await removeThreadChannel(interaction.guildId, channelId);
  } catch (err) {
    logger.error(`[auto-threads] ${interaction.guildId}: remove failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!found) return finish(interaction, MSG.NOT_LISTED(channelId));
  logger.info(`[auto-threads] ${interaction.guildId}: ${channelId} removed by ${interaction.user.id}`);
  return finish(interaction, successEmbed('Auto Thread Removed', `<#${channelId}> no longer gets a thread on every message. Existing threads stay.`));
}

// ── /autothread publish add | remove ──────────────────────────────────────────

async function runPublishAdd(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const channel = pickedChannel(interaction);
  if (!channel || channel.type !== ChannelType.GuildAnnouncement) return finish(interaction, MSG.BAD_CHANNEL('an announcement channel'));
  let result;
  try {
    result = await addPublishChannel(interaction.guildId, channel.id);
  } catch (err) {
    if (err instanceof AutoThreadError) return finish(interaction, errorEmbed(err.title, err.message));
    logger.error(`[auto-threads] ${interaction.guildId}: publish add failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  const note = notes({ settings: result.settings, problems: [publishProblem(channel)].filter(Boolean) });
  if (!result.added) return finish(interaction, infoEmbed('Already Publishing', `New posts in <#${channel.id}> are already published.${note}`));
  logger.info(`[auto-threads] ${interaction.guildId}: publishing in ${channel.id}, added by ${interaction.user.id}`);
  return finish(interaction, successEmbed('Auto Publish Added',
    `New posts in <#${channel.id}> (members, bots and webhooks) are published to the servers that follow it. `
    + `Discord allows ${AUTOTHREAD.PUBLISH_MAX} per channel per hour; the rest stay unpublished.${note}`));
}

async function runPublishRemove(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const channelId = interaction.options.getChannel('channel', true).id;
  let found;
  try {
    found = await removePublishChannel(interaction.guildId, channelId);
  } catch (err) {
    logger.error(`[auto-threads] ${interaction.guildId}: publish remove failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!found) return finish(interaction, MSG.NOT_LISTED(channelId));
  logger.info(`[auto-threads] ${interaction.guildId}: publishing in ${channelId} removed by ${interaction.user.id}`);
  return finish(interaction, successEmbed('Auto Publish Removed', `New posts in <#${channelId}> are no longer published automatically.`));
}

// ── /autothread list ──────────────────────────────────────────────────────────

async function runList(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  const entries = readEntries(settings);
  const publishIds = readPublishIds(settings);
  const threadLines = entries.map((e) => {
    const problem = threadProblem(guild.channels.cache.get(e.channelId), e);
    return `<#${e.channelId}> · \`${shown(e.name)}\` · ${AUTOTHREAD.ARCHIVE_LABELS[e.archiveMinutes]}${e.closeButton ? ' · 🔒 close button' : ''}${problem ? `\n⚠️ ${problem}` : ''}`;
  });
  const publishLines = publishIds.map((id) => {
    const problem = publishProblem(guild.channels.cache.get(id));
    return `<#${id}>${problem ? `\n⚠️ ${problem}` : ''}`;
  });
  const enabled = isModuleEnabled(settings, 'autothreads');
  const parts = [];
  if (!enabled) parts.push(OFF_NOTE);
  if (!threadLines.length && !publishLines.length) {
    parts.push('Nothing is set up yet. Add a channel with `/autothread add`, or `/autothread publish add` for an announcement channel.');
  } else {
    parts.push(`**🧵 Threads (${entries.length}/${LIMITS.AUTOTHREAD_CHANNELS})**\n${joinWithLimit(threadLines, { sep: '\n', max: 1700 }) || 'None.'}`);
    parts.push(`**📣 Publishing (${publishIds.length}/${LIMITS.AUTOPUBLISH_CHANNELS})**\n${joinWithLimit(publishLines, { sep: '\n', max: 1500 }) || 'None.'}`);
  }
  return finish(interaction, guildEmbed(settings, '🧵 Auto Threads', parts.join('\n\n')));
}

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  if (group === 'publish') {
    if (sub === 'add') return runPublishAdd(interaction);
    if (sub === 'remove') return runPublishRemove(interaction);
    return undefined;
  }
  if (sub === 'add') return runAdd(interaction);
  if (sub === 'remove') return runRemove(interaction);
  if (sub === 'list') return runList(interaction);
  return undefined;
}
