// /logging (pkg-logging.md §4): turn audit logging on or off, pick the log channel of each log type, switch single
// log events on or off and manage the list of channels whose message events are never logged.
// Writes go through settingsService only (R008/R009); the moderation log channel belongs to /modconfig logchannel.
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import {
  LOG_EVENTS,
  LOG_EVENT_KEYS,
  LOG_TYPES,
  LOG_TYPE_LABELS,
  forgetDeadChannels,
  getLogChannelHealth,
  isLogEventEnabled,
  isLogEventKey,
  toggleLogEvents,
} from '../../services/loggingService.js';
import { getSettings, getSettingsSafe, peekSettings, updateSettings, updateSettingsWith } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { errorEmbed, guildEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { checkChannelUsable, permissionNames, respondAutocomplete } from '../../utils/interactionHelpers.js';
import { newChannelOption, resolveNewOptions, reuseNote } from '../../utils/newTargets.js';
import logger from '../../utils/logger.js';
import { joinWithLimit } from '../../utils/text.js';

export const module = 'logging';
export const gated = false; // works while the module is off, so an admin can turn it on
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const LOG_CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const IGNORABLE_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildVoice,
  ChannelType.GuildStageVoice, ChannelType.GuildForum, ChannelType.GuildMedia, ChannelType.GuildCategory];

export const TYPE_CHOICES = [
  { name: 'All log types (one channel for everything)', value: 'all' },
  { name: 'Messages (edits, deletes, invites)', value: 'message' },
  { name: 'Members (roles, nicknames, timeouts, bans)', value: 'member' },
  { name: 'Server (channels, roles, emojis, invites)', value: 'server' },
  { name: 'Voice (join, leave, move)', value: 'voice' },
  { name: 'Joins and leaves', value: 'joinleave' },
];

export const TYPE_VALUE_TO_KEYS = Object.freeze({
  all: ['MESSAGE', 'MEMBER', 'SERVER', 'VOICE', 'JOIN_LEAVE'],
  message: ['MESSAGE'],
  member: ['MEMBER'],
  server: ['SERVER'],
  voice: ['VOICE'],
  joinleave: ['JOIN_LEAVE'],
});

const CONFIGURABLE_KEYS = TYPE_VALUE_TO_KEYS.all; // MOD is never offered (decision D1)
const BASE_NEEDS = ['ViewChannel', 'SendMessages', 'EmbedLinks'];
const MAX_IGNORED = LIMITS.LOG_IGNORED_CHANNELS;

const ATTACH_HINT = 'ℹ️ Give me **Attach Files** there to get bulk-delete transcripts.';
const OFF_HINT = 'ℹ️ Audit logs are currently **off**. Turn them on with `/logging enable`.';

export const data = new SlashCommandBuilder()
  .setName('logging')
  .setDescription('Audit logs: turn on/off, pick log channels, toggle events (MEE6 Moderator › Audit logs).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('enable').setDescription('Turn audit logging on.')
    .addChannelOption((o) => o.setName('channel').setDescription('Optional: send every log type to this channel.')
      .addChannelTypes(...LOG_CHANNEL_TYPES))
    .addStringOption((o) => newChannelOption(o)))
  .addSubcommand((s) => s.setName('disable').setDescription('Turn audit logging off (settings are kept).'))
  .addSubcommand((s) => s.setName('status').setDescription('Show log channels, disabled events and ignored channels.'))
  .addSubcommand((s) => s.setName('channel').setDescription('Set or clear the channel for a log type.')
    .addStringOption((o) => o.setName('type').setDescription('Which logs go to the channel.').setRequired(true).addChoices(...TYPE_CHOICES))
    .addChannelOption((o) => o.setName('channel').setDescription('Log channel (leave empty to clear).').addChannelTypes(...LOG_CHANNEL_TYPES))
    .addStringOption((o) => newChannelOption(o)))
  .addSubcommand((s) => s.setName('event').setDescription('Turn one log event (or all) on or off.')
    .addStringOption((o) => o.setName('name').setDescription('Log event, or "all".').setRequired(true).setAutocomplete(true).setMaxLength(32))
    .addStringOption((o) => o.setName('state').setDescription('on or off').setRequired(true)
      .addChoices({ name: 'on', value: 'on' }, { name: 'off', value: 'off' })))
  .addSubcommandGroup((g) => g.setName('ignore').setDescription('Channels whose message events are never logged.')
    .addSubcommand((s) => s.setName('add').setDescription('Stop logging message events in a channel or category.')
      .addChannelOption((o) => o.setName('channel').setDescription('Channel or category to ignore.').setRequired(true).addChannelTypes(...IGNORABLE_TYPES)))
    .addSubcommand((s) => s.setName('remove').setDescription('Log message events in a channel again.')
      .addChannelOption((o) => o.setName('channel').setDescription('Channel or category to stop ignoring.').setRequired(true).addChannelTypes(...IGNORABLE_TYPES)))
    .addSubcommand((s) => s.setName('list').setDescription('List ignored channels.')));

// ── Helpers ───────────────────────────────────────────────────────────────────

/**
 * The option's channel as a cached guild channel (the interaction may carry only the raw API shape, which has no
 * permission methods).
 */
async function resolveChannel(interaction, option) {
  if (!option) return null;
  const cached = interaction.guild.channels.cache.get(option.id);
  if (cached) return cached;
  return interaction.guild.channels.fetch(option.id).catch(() => null);
}

/**
 * Null when the bot can post in the channel, otherwise the sentence to show.
 * `checkChannelUsable(...).missing` holds PermissionFlagsBits keys, shown through permissionNames().
 */
function describeUsable(channel, needs = BASE_NEEDS) {
  const u = checkChannelUsable(channel, { needs });
  if (u.ok) return null;
  return u.missing.length
    ? `I need **${permissionNames(u.missing).join(', ')}** in <#${channel.id}> to post logs there.`
    : (u.reason ?? `I cannot post in <#${channel.id}>.`);
}

const lacksAttach = (channel) => !checkChannelUsable(channel, { needs: ['AttachFiles'] }).ok;
const reply = (interaction, embed) => interaction.editReply({ embeds: [embed] });

// ── Subcommands ───────────────────────────────────────────────────────────────

async function subEnable(interaction) {
  const guildId = interaction.guildId;
  const option = interaction.options.getChannel('channel');
  let channel = null;
  if (option) {
    channel = await resolveChannel(interaction, option);
    const problem = channel ? describeUsable(channel) : 'I cannot see that channel.';
    if (problem) return reply(interaction, errorEmbed('Missing Permissions', problem));
  }

  const patch = { loggingEnabled: true };
  if (channel) for (const type of CONFIGURABLE_KEYS) patch[LOG_TYPES[type]] = channel.id;
  const settings = await updateSettings(guildId, patch);
  forgetDeadChannels(guildId);

  const configured = CONFIGURABLE_KEYS.filter((k) => settings[LOG_TYPES[k]]);
  if (!channel && configured.length === 0) {
    return reply(interaction, warnEmbed('Audit Logs Enabled — No Channel Yet',
      'Logging is on, but no log channel is set, so nothing will be posted. Use `/logging channel` or `/logging enable channel:#logs`.'));
  }

  const lines = channel
    ? [`Every log type will be posted in <#${channel.id}>.`]
    : configured.map((k) => `**${LOG_TYPE_LABELS[k]}** → <#${settings[LOG_TYPES[k]]}>`);
  for (const k of configured) {
    const health = getLogChannelHealth(interaction.guild, settings[LOG_TYPES[k]]);
    if (health.state !== 'ok') lines.push(`⚠️ **${LOG_TYPE_LABELS[k]}**: ${health.text}`);
  }
  const reused = reuseNote(interaction); // a typed new_channel name that matched an existing private channel
  if (reused) lines.push(reused);
  if (channel && lacksAttach(channel)) lines.push(ATTACH_HINT);
  return reply(interaction, successEmbed('Audit Logs Enabled', lines.join('\n')));
}

async function subDisable(interaction) {
  await updateSettings(interaction.guildId, { loggingEnabled: false });
  return reply(interaction, successEmbed('Audit Logs Disabled',
    'No audit logs will be posted. Your log channels, event toggles and ignored channels are kept. The moderation log (`/modconfig logchannel`) is not affected.'));
}

async function subStatus(interaction) {
  const settings = await getSettings(interaction.guildId);
  const embed = guildEmbed(settings, '📜 Audit Logs', null);

  const channelLines = CONFIGURABLE_KEYS.map((k) => {
    const id = settings[LOG_TYPES[k]];
    if (!id) return `**${LOG_TYPE_LABELS[k]}** → not set`;
    const health = getLogChannelHealth(interaction.guild, id);
    return health.state === 'ok'
      ? `**${LOG_TYPE_LABELS[k]}** → <#${id}> ✅`
      : `**${LOG_TYPE_LABELS[k]}** → <#${id}> ⚠️ ${health.text}`;
  });
  channelLines.push(settings.modLogChannelId
    ? `**Moderation** → <#${settings.modLogChannelId}> (set with \`/modconfig logchannel\`)`
    : '**Moderation** → not set (`/modconfig logchannel`)');

  const disabled = LOG_EVENT_KEYS.filter((k) => !isLogEventEnabled(settings, k));
  const ignored = settings.logIgnoredChannelIds ?? [];

  embed.addFields(
    { name: 'Status', value: settings.loggingEnabled ? '✅ On' : '❌ Off — turn it on with `/logging enable`' },
    { name: 'Channels', value: channelLines.join('\n') },
    {
      name: 'Disabled events',
      value: disabled.length
        ? joinWithLimit(disabled.map((k) => `${LOG_EVENTS[k].emoji} ${LOG_EVENTS[k].label}`), { sep: '\n', max: 1024 })
        : 'None — every event is logged.',
    },
    {
      name: 'Ignored channels',
      value: ignored.length ? joinWithLimit(ignored.map((id) => `<#${id}>`), { sep: ', ', max: 1024 }) : 'None.',
    },
  );
  embed.setFooter({ text: `${LOG_EVENT_KEYS.length - disabled.length}/${LOG_EVENT_KEYS.length} events on · MEE6: Moderator › Audit logs` });
  return reply(interaction, embed);
}

async function subChannel(interaction) {
  const guildId = interaction.guildId;
  const type = interaction.options.getString('type', true);
  const keys = Object.hasOwn(TYPE_VALUE_TO_KEYS, type) ? TYPE_VALUE_TO_KEYS[type] : null;
  if (!keys) return reply(interaction, errorEmbed('Unknown Type', 'Pick a log type from the list.'));
  const labels = type === 'all' ? 'All' : keys.map((k) => LOG_TYPE_LABELS[k]).join(', ');

  const option = interaction.options.getChannel('channel');
  if (!option) {
    await updateSettings(guildId, Object.fromEntries(keys.map((k) => [LOG_TYPES[k], null])));
    return reply(interaction, successEmbed('Log Channel Cleared', `**${labels}** logs will no longer be posted.`));
  }

  const channel = await resolveChannel(interaction, option);
  const problem = channel ? describeUsable(channel) : 'I cannot see that channel.';
  if (problem) return reply(interaction, errorEmbed('Missing Permissions', problem));

  const settings = await updateSettings(guildId, Object.fromEntries(keys.map((k) => [LOG_TYPES[k], channel.id])));
  forgetDeadChannels(guildId, channel.id);

  const lines = [`**${labels}** logs will be posted in <#${channel.id}>.`];
  const reused = reuseNote(interaction);
  if (reused) lines.push(reused);
  if (keys.includes('MESSAGE') && lacksAttach(channel)) lines.push(ATTACH_HINT);
  if (!settings.loggingEnabled) lines.push(OFF_HINT);
  return reply(interaction, successEmbed('Log Channel Set', lines.join('\n')));
}

async function subEvent(interaction) {
  const name = interaction.options.getString('name', true).trim();
  const state = interaction.options.getString('state', true);
  if (name !== 'all' && !isLogEventKey(name)) {
    return reply(interaction, errorEmbed('Unknown Event', 'Pick an event from the list. `/logging status` shows which events are off.'));
  }
  const on = state === 'on';

  let already = false;
  const settings = await updateSettingsWith(interaction.guildId, (cur) => {
    const before = toggleLogEvents(cur.disabledLogEvents, '', true); // the sanitised current list
    const after = toggleLogEvents(cur.disabledLogEvents, name, on);
    already = before.length === after.length && before.every((k, i) => k === after[i]);
    return { disabledLogEvents: after };
  });

  const lines = [name === 'all'
    ? `Every log event is now **${state}**.`
    : `${LOG_EVENTS[name].emoji} **${LOG_EVENTS[name].label}** is now **${state}**.`];
  if (already) lines[0] = `It was already ${state}. ${lines[0]}`;
  if (!settings.loggingEnabled) lines.push(OFF_HINT);
  return reply(interaction, successEmbed('Log Event Updated', lines.join('\n')));
}

async function subIgnoreAdd(interaction) {
  const channel = interaction.options.getChannel('channel', true);
  const id = channel.id;
  let outcome = 'added';
  await updateSettingsWith(interaction.guildId, (cur) => {
    const list = [...(cur.logIgnoredChannelIds ?? [])];
    if (list.includes(id)) { outcome = 'exists'; return null; }
    if (list.length >= MAX_IGNORED) { outcome = 'limit'; return null; }
    return { logIgnoredChannelIds: [...list, id] };
  });

  if (outcome === 'exists') return reply(interaction, warnEmbed('Already Ignored', `Message events in <#${id}> are already ignored.`));
  if (outcome === 'limit') {
    return reply(interaction, errorEmbed('Limit Reached', `You can ignore at most ${MAX_IGNORED} channels. Remove one with \`/logging ignore remove\` first.`));
  }
  const where = channel.type === ChannelType.GuildCategory ? `in every channel of **${channel.name}**` : `in <#${id}>`;
  return reply(interaction, successEmbed('Channel Ignored',
    `Deleted, edited and bulk-deleted messages and posted invites ${where} will no longer be logged.`));
}

async function subIgnoreRemove(interaction) {
  const id = interaction.options.getChannel('channel', true).id;
  let outcome = 'removed';
  await updateSettingsWith(interaction.guildId, (cur) => {
    const list = [...(cur.logIgnoredChannelIds ?? [])];
    if (!list.includes(id)) { outcome = 'missing'; return null; }
    return { logIgnoredChannelIds: list.filter((x) => x !== id) };
  });

  if (outcome === 'missing') return reply(interaction, warnEmbed('Not Ignored', `<#${id}> is not on the ignore list.`));
  return reply(interaction, successEmbed('Channel No Longer Ignored', `Message events in <#${id}> are logged again.`));
}

async function subIgnoreList(interaction) {
  const settings = await getSettings(interaction.guildId);
  const ids = settings.logIgnoredChannelIds ?? [];
  const body = ids.length
    ? joinWithLimit(ids.map((id) => (interaction.guild.channels.cache.has(id) ? `<#${id}>` : `#deleted-channel (\`${id}\`)`)), { sep: '\n', max: 4000 })
    : 'No channels are ignored.';
  return reply(interaction, guildEmbed(settings, '🙈 Ignored Channels', body, `${ids.length}/${MAX_IGNORED}`));
}

// ── Entry points ──────────────────────────────────────────────────────────────

export async function execute(interaction) {
  await interaction.deferReply();
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  const label = group ? `${group} ${sub}` : sub;
  try {
    if (!(await resolveNewOptions(interaction, data, { private: ['channel'] }))) return undefined;
    if (group === 'ignore') {
      if (sub === 'add') return await subIgnoreAdd(interaction);
      if (sub === 'remove') return await subIgnoreRemove(interaction);
      return await subIgnoreList(interaction);
    }
    switch (sub) {
      case 'enable': return await subEnable(interaction);
      case 'disable': return await subDisable(interaction);
      case 'status': return await subStatus(interaction);
      case 'channel': return await subChannel(interaction);
      case 'event': return await subEvent(interaction);
      default: return undefined;
    }
  } catch (err) {
    logger.error(`/logging ${label} failed`, err instanceof Error ? err : new Error(String(err)));
    return interaction
      .editReply({ embeds: [errorEmbed('Could Not Save', 'Something went wrong while saving. Please try again in a moment.')] })
      .catch(() => {});
  }
}

export async function autocomplete(interaction) {
  const focused = interaction.options.getFocused().toLowerCase().trim();
  // Rule 21: the answer is due in 3 s, so the settings lookup is memo-first and bounded. A null result means
  // every event is shown as on.
  let settings = peekSettings(interaction.guildId);
  if (!settings) {
    settings = await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'logging autocomplete settings')
      .catch(() => null);
  }

  const candidates = [{ name: 'All events', value: 'all', search: 'all events' }];
  for (const key of LOG_EVENT_KEYS) {
    const { label, type } = LOG_EVENTS[key];
    const on = isLogEventEnabled(settings, key);
    candidates.push({
      name: `${on ? '✅' : '❌'} ${label} — ${LOG_TYPE_LABELS[type]}`,
      value: key,
      search: `${key} ${label} ${LOG_TYPE_LABELS[type]}`.toLowerCase(),
    });
  }
  const filtered = candidates.filter((c) => !focused || c.search.includes(focused));
  return respondAutocomplete(interaction, filtered.slice(0, 25).map(({ name, value }) => ({ name, value })));
}
