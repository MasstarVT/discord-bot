// /trigger: auto-responders for matching messages (pkg-custom-commands.md §4.2). Config command: it works
// while the Custom Commands module is off and warns when a trigger will not fire yet. Replies are public
// (CLAUDE.md admin convention). Regex patterns are validated by safeRegex at write time and executed in a
// time-limited sandbox at run time; the matching and the hook live in tagService.js.
import { SlashCommandBuilder, ChannelType, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import prisma from '../../database/client.js';
import { getSettings, updateSettings, ensureGuildSettings, isModuleEnabled } from '../../services/settingsService.js';
import {
  MATCH_LABELS, TRIGGER_SCOPE_MAX, TagError, codeSafe, createTriggerRow, formatIdList, invalidateTriggerCache, oneLine,
  resolveTrigger, triggerNumber, triggerQuarantined, validateTriggerPattern, writeTrigger,
} from '../../services/tagService.js';
import { renderPlaceholdersDetailed } from '../../utils/placeholderParser.js';
import { successEmbed, errorEmbed, infoEmbed, applyGuildColor } from '../../utils/embedBuilder.js';
import {
  respondAutocomplete, checkChannelUsable, missingBotPermissions, permissionNames,
} from '../../utils/interactionHelpers.js';
import { paginate } from '../../utils/paginator.js';
import { clampText, joinWithLimit } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = 'customcommands';
export const gated = false; // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const TRIG = (o) => o.setName('trigger').setDescription('Pattern, #number or ID').setRequired(true).setAutocomplete(true).setMaxLength(100);
const MATCH = (o) => o.setName('match_type').setDescription('Match type (default: contains)')
  .addChoices({ name: 'Contains', value: 'CONTAINS' }, { name: 'Exact', value: 'EXACT' }, { name: 'Starts with', value: 'STARTS_WITH' },
    { name: 'Ends with', value: 'ENDS_WITH' }, { name: 'Regex', value: 'REGEX' });
const TRIG_CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildForum, ChannelType.GuildVoice];

export const data = new SlashCommandBuilder()
  .setName('trigger')
  .setDescription('Auto-responders for matching messages (custom commands).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('enable').setDescription('Turn on custom commands and triggers.'))
  // Same length as the original text, which is budget-locked; `disable` now only pauses triggers (amendment 1).
  .addSubcommand((s) => s.setName('disable').setDescription('Turn off triggers; /tag keeps working.'))
  .addSubcommand((s) => s.setName('add').setDescription('Add a trigger.')
    .addStringOption((o) => o.setName('pattern').setDescription('Word, phrase or regex to watch for').setRequired(true).setMaxLength(500))
    .addStringOption((o) => o.setName('response').setDescription('Reply text (supports variables)').setRequired(true).setMaxLength(2000))
    .addStringOption((o) => MATCH(o))
    .addIntegerOption((o) => o.setName('cooldown').setDescription('Seconds per user (0 = none)').setMinValue(0).setMaxValue(86400))
    .addBooleanOption((o) => o.setName('delete_message').setDescription('Delete the matching message'))
    .addBooleanOption((o) => o.setName('case_sensitive').setDescription('Match case'))
    .addChannelOption((o) => o.setName('channel').setDescription('Only in this channel').addChannelTypes(...TRIG_CHANNEL_TYPES)))
  .addSubcommand((s) => s.setName('edit').setDescription('Change a trigger.')
    .addStringOption((o) => TRIG(o))
    .addStringOption((o) => o.setName('pattern').setDescription('New pattern').setMaxLength(500))
    .addStringOption((o) => o.setName('response').setDescription('New reply text').setMaxLength(2000))
    .addStringOption((o) => MATCH(o))
    .addIntegerOption((o) => o.setName('cooldown').setDescription('Seconds per user (0 = none)').setMinValue(0).setMaxValue(86400))
    .addBooleanOption((o) => o.setName('delete_message').setDescription('Delete the matching message'))
    .addBooleanOption((o) => o.setName('case_sensitive').setDescription('Match case'))
    .addChannelOption((o) => o.setName('channel').setDescription('Add or remove an allowed channel').addChannelTypes(...TRIG_CHANNEL_TYPES))
    .addRoleOption((o) => o.setName('exempt_role').setDescription('Add or remove an exempt role'))
    .addBooleanOption((o) => o.setName('reset_scope').setDescription('Clear channels and exempt roles first')))
  .addSubcommand((s) => s.setName('remove').setDescription('Delete a trigger.').addStringOption((o) => TRIG(o)))
  .addSubcommand((s) => s.setName('toggle').setDescription('Pause or resume a trigger.').addStringOption((o) => TRIG(o)))
  .addSubcommand((s) => s.setName('info').setDescription("Show a trigger's details.").addStringOption((o) => TRIG(o)))
  .addSubcommand((s) => s.setName('list').setDescription('List triggers with their numbers and IDs.'));

// ── Helpers ───────────────────────────────────────────────────────────────────

const NO_MENTIONS = Object.freeze({ parse: [] });
const PAGE_SIZE = 10;

const MODULE_OFF_NOTE = '⚠️ The **Custom Commands** module is off, so triggers do not fire yet. Turn it on with `/trigger enable` (or `/module enable customcommands`).';
const TRIGGERS_OFF_NOTE = '⚠️ Triggers are switched off, so this trigger does not fire yet. Turn them on with /trigger enable.';

/** The warning to append when a saved trigger would not fire yet (R223), or null. */
function offNote(settings) {
  if (!isModuleEnabled(settings, 'customcommands')) return MODULE_OFF_NOTE;
  if (settings?.autoTriggersEnabled === false) return TRIGGERS_OFF_NOTE;
  return null;
}

/** Fences inside a code block would end it. */
const neutraliseFences = (s) => String(s ?? '').replaceAll('```', '`​`​`');

const yesNo = (b) => (b ? 'Yes' : 'No');
const matchLabel = (row) => `${MATCH_LABELS[row.matchType] ?? row.matchType}${row.caseSensitive ? ' (case-sensitive)' : ''}`;
const cooldownText = (n) => (n ? `${n}s per user` : 'None');

/** A problem with posting in a trigger channel (forums need thread permissions), or null. */
function triggerChannelProblem(channel) {
  if (channel.type === ChannelType.GuildForum) {
    const missing = missingBotPermissions(channel, ['ViewChannel', 'SendMessagesInThreads']);
    return missing.length ? `I need **${permissionNames(missing).join(', ')}** in <#${channel.id}>.` : null;
  }
  const usable = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] });
  return usable.ok ? null : (usable.reason ?? `I cannot post in <#${channel.id}>.`);
}

/** The mention lists shown after a change. */
const channelsText = (row, guild) => formatIdList(row.channelIds, { kind: 'channel', guild, empty: 'All channels' });
const exemptText = (row, guild) => formatIdList(row.exemptRoleIds, { kind: 'role', guild, empty: 'None' });

const isUnsafe = (row) => row.matchType === 'REGEX' && !validateTriggerPattern(row.trigger, row.matchType, row.caseSensitive).ok;

const fail = (interaction, err) => interaction.editReply({ embeds: [errorEmbed(err.title, err.message)], allowedMentions: NO_MENTIONS });
const reply = (interaction, embed) => interaction.editReply({ embeds: [embed], allowedMentions: NO_MENTIONS });

// ── Execute ───────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  await interaction.deferReply(); // public, like every admin config command
  const settings = await getSettings(interaction.guildId);
  try {
    switch (interaction.options.getSubcommand()) {
      case 'enable': return await enable(interaction);
      case 'disable': return await disable(interaction);
      case 'add': return await add(interaction, settings);
      case 'edit': return await edit(interaction, settings);
      case 'remove': return await remove(interaction);
      case 'toggle': return await toggle(interaction, settings);
      case 'info': return await info(interaction, settings);
      case 'list': return await list(interaction, settings);
      default: return undefined;
    }
  } catch (err) {
    if (err instanceof TagError) return fail(interaction, err);
    throw err; // the router's generic error reply
  }
}

// ── enable / disable ──────────────────────────────────────────────────────────

async function enable(interaction) {
  await updateSettings(interaction.guildId, { customCommandsEnabled: true, autoTriggersEnabled: true });
  return reply(interaction, successEmbed('Triggers On', 'Custom commands and triggers now respond.'));
}

async function disable(interaction) {
  // Only the trigger switch: /tag and prefix commands keep working (turn the module off with /module).
  await updateSettings(interaction.guildId, { autoTriggersEnabled: false });
  return reply(interaction, successEmbed(
    'Triggers Off',
    'Triggers stop responding. /tag and prefix commands keep working; turn the whole module off with /module disable customcommands.',
  ));
}

// ── add ───────────────────────────────────────────────────────────────────────

async function add(interaction, settings) {
  const { options, guild, guildId } = interaction;
  const pattern = options.getString('pattern', true);
  const response = options.getString('response', true);
  const matchType = options.getString('match_type') ?? 'CONTAINS';
  const cooldown = options.getInteger('cooldown') ?? 0;
  const deleteMessage = options.getBoolean('delete_message') ?? false;
  const caseSensitive = options.getBoolean('case_sensitive') ?? false;
  const channel = options.getChannel('channel');

  const v = validateTriggerPattern(pattern, matchType, caseSensitive);
  if (!v.ok) return reply(interaction, errorEmbed('Invalid Pattern', v.reason));
  if (response.trim() === '') return reply(interaction, errorEmbed('Empty', 'The reply text cannot be empty.'));
  if (channel) {
    const problem = triggerChannelProblem(channel);
    if (problem) return reply(interaction, errorEmbed('Missing Permissions', problem));
  }

  await ensureGuildSettings(guildId);
  const row = await createTriggerRow({
    guildId, trigger: pattern, response, matchType, cooldown, deleteMessage, caseSensitive,
    channelIds: channel ? [channel.id] : [], exemptRoleIds: [], createdBy: interaction.user.id,
  });
  await invalidateTriggerCache(guildId);
  const n = await triggerNumber(guildId, row);

  const lines = [
    `**#${n}** · ID \`${row.id}\``,
    `**Pattern:** \`${codeSafe(clampText(pattern, 200))}\``,
    `**Match:** ${matchLabel(row)}`,
    `**Cooldown:** ${cooldownText(cooldown)}`,
    `**Delete message:** ${yesNo(deleteMessage)}`,
    `**Channels:** ${channel ? `<#${channel.id}>` : 'All channels'}`,
  ];
  const unknown = renderPlaceholdersDetailed(response, {
    member: interaction.member, user: interaction.user, guild, channel: interaction.channel,
  }).unknownTokens;
  if (unknown.length) {
    lines.push(`⚠️ Unknown variables (shown as typed): ${joinWithLimit(unknown.map((t) => `\`${codeSafe(t)}\``), { max: 400 })}`);
  }
  if (deleteMessage && missingBotPermissions(guild, 'ManageMessages').length) {
    lines.push('⚠️ I need **Manage Messages** to delete matching messages.');
  }
  const note = offNote(settings);
  if (note) lines.push(note);
  return reply(interaction, successEmbed('Trigger Added', lines.join('\n')));
}

// ── edit (R283) ───────────────────────────────────────────────────────────────

async function edit(interaction, settings) {
  const { options, guild, guildId } = interaction;
  const row = await resolveTrigger(guildId, options.getString('trigger', true));

  const pattern = options.getString('pattern');
  const response = options.getString('response');
  const matchType = options.getString('match_type');
  const cooldown = options.getInteger('cooldown');
  const deleteMessage = options.getBoolean('delete_message');
  const caseSensitive = options.getBoolean('case_sensitive');
  const channel = options.getChannel('channel');
  const exemptRole = options.getRole('exempt_role');
  const resetScope = options.getBoolean('reset_scope') === true;

  const nothing = [pattern, response, matchType, cooldown, deleteMessage, caseSensitive, channel, exemptRole]
    .every((x) => x === null || x === undefined) && !resetScope;
  if (nothing) return reply(interaction, errorEmbed('Nothing to Change', 'Give at least one option to change.'));

  const next = {
    pattern: pattern ?? row.trigger,
    matchType: matchType ?? row.matchType,
    caseSensitive: caseSensitive ?? row.caseSensitive,
  };
  if (pattern !== null || matchType !== null || caseSensitive !== null) {
    const v = validateTriggerPattern(next.pattern, next.matchType, next.caseSensitive);
    if (!v.ok) return reply(interaction, errorEmbed('Invalid Pattern', v.reason));
  }
  if (response !== null && response.trim() === '') return reply(interaction, errorEmbed('Empty', 'The reply text cannot be empty.'));

  // Scope: channels and exempt roles are toggled (added when absent, removed when present).
  let channelIds = resetScope ? [] : [...row.channelIds];
  let exemptRoleIds = resetScope ? [] : [...row.exemptRoleIds];
  if (channel) {
    if (channelIds.includes(channel.id)) {
      channelIds = channelIds.filter((id) => id !== channel.id); // removing never needs the permission check
    } else {
      const problem = triggerChannelProblem(channel);
      if (problem) return reply(interaction, errorEmbed('Missing Permissions', problem));
      channelIds.push(channel.id);
    }
  }
  if (exemptRole) {
    if (exemptRole.id === guildId) {
      return reply(interaction, errorEmbed('Not Allowed', '@everyone cannot be exempt; pause the trigger with /trigger toggle instead.'));
    }
    if (exemptRoleIds.includes(exemptRole.id)) exemptRoleIds = exemptRoleIds.filter((id) => id !== exemptRole.id);
    else exemptRoleIds.push(exemptRole.id);
  }
  if (channelIds.length > TRIGGER_SCOPE_MAX || exemptRoleIds.length > TRIGGER_SCOPE_MAX) {
    return reply(interaction, errorEmbed('Limit Reached', `A trigger can list at most ${TRIGGER_SCOPE_MAX} channels and ${TRIGGER_SCOPE_MAX} exempt roles.`));
  }

  const data = {};
  const lines = [];
  if (pattern !== null) { data.trigger = pattern; lines.push(`**Pattern:** \`${codeSafe(clampText(pattern, 200))}\``); }
  if (matchType !== null) { data.matchType = matchType; lines.push(`**Match:** ${MATCH_LABELS[matchType]}`); }
  if (caseSensitive !== null) { data.caseSensitive = caseSensitive; lines.push(`**Case-sensitive:** ${yesNo(caseSensitive)}`); }
  if (response !== null) { data.response = response; lines.push('**Response** updated.'); }
  if (cooldown !== null) { data.cooldown = cooldown; lines.push(`**Cooldown:** ${cooldownText(cooldown)}`); }
  if (deleteMessage !== null) { data.deleteMessage = deleteMessage; lines.push(`**Delete message:** ${yesNo(deleteMessage)}`); }
  if (resetScope || channel) data.channelIds = channelIds;
  if (resetScope || exemptRole) data.exemptRoleIds = exemptRoleIds;
  if (!Object.keys(data).length) return reply(interaction, errorEmbed('Nothing to Change', 'Give at least one option to change.'));

  const updated = await writeTrigger(guildId, row, data);
  await invalidateTriggerCache(guildId);

  const view = { ...row, ...updated };
  lines.push(`**Channels:** ${channelsText(view, guild)}`);
  lines.push(`**Exempt roles:** ${exemptText(view, guild)}`);
  const note = offNote(settings);
  if (note) lines.push(note);
  const n = await triggerNumber(guildId, row);
  return reply(interaction, successEmbed('Trigger Updated', `**#${n}** · ID \`${row.id}\`\n${lines.join('\n')}`));
}

// ── remove / toggle ───────────────────────────────────────────────────────────

async function remove(interaction) {
  const { guildId } = interaction;
  const row = await resolveTrigger(guildId, interaction.options.getString('trigger', true));
  const n = await triggerNumber(guildId, row);
  try {
    await prisma.autoTrigger.delete({ where: { id: row.id } });
  } catch (err) {
    if (err?.code === 'P2025') throw new TagError('NOT_FOUND', 'Not Found', 'That trigger was already removed.');
    throw err;
  }
  await invalidateTriggerCache(guildId);
  return reply(interaction, successEmbed('Trigger Removed', `#${n} \`${codeSafe(clampText(row.trigger, 100))}\` was deleted (${row.uses} fires).`));
}

async function toggle(interaction, settings) {
  const { guildId } = interaction;
  const row = await resolveTrigger(guildId, interaction.options.getString('trigger', true));
  const n = await triggerNumber(guildId, row);
  const enabled = !row.enabled;
  await writeTrigger(guildId, row, { enabled });
  await invalidateTriggerCache(guildId);
  const note = enabled ? offNote(settings) : null;
  const text = `#${n} \`${codeSafe(clampText(row.trigger, 100))}\` is now **${enabled ? 'active' : 'paused'}**.`;
  return reply(interaction, successEmbed(`Trigger ${enabled ? 'Resumed' : 'Paused'}`, note ? `${text}\n${note}` : text));
}

// ── info (R224) ───────────────────────────────────────────────────────────────

async function info(interaction, settings) {
  const { guildId, guild } = interaction;
  const row = await resolveTrigger(guildId, interaction.options.getString('trigger', true));
  const n = await triggerNumber(guildId, row);
  const creator = await interaction.client.users.fetch(row.createdBy).then((u) => u.tag).catch(() => row.createdBy);

  const embed = applyGuildColor(infoEmbed(`Trigger #${n}`, null), settings);
  embed.addFields(
    { name: 'Match type', value: matchLabel(row), inline: true },
    { name: 'Status', value: row.enabled ? '✅ Active' : '⏸️ Paused', inline: true },
    { name: 'Uses', value: Number(row.uses).toLocaleString('en-US'), inline: true },
    { name: 'Cooldown', value: cooldownText(row.cooldown), inline: true },
    { name: 'Delete message', value: yesNo(row.deleteMessage), inline: true },
    { name: 'Created by', value: `<@${row.createdBy}>`, inline: true },
    { name: 'Channels', value: channelsText(row, guild) },
    { name: 'Exempt roles', value: exemptText(row, guild) },
    { name: 'Pattern', value: `\`\`\`\n${neutraliseFences(clampText(row.trigger, 900))}\n\`\`\`` },
    { name: 'Response', value: clampText(row.response, 1024) || '—' },
  );
  if (row.matchType === 'REGEX') {
    const v = validateTriggerPattern(row.trigger, row.matchType, row.caseSensitive);
    if (!v.ok) {
      embed.addFields({ name: '⚠️ Never fires', value: clampText(`This regex is unsafe or invalid (${v.reason}) and is skipped. Fix it with /trigger edit.`, 1024) });
    } else if (triggerQuarantined({ ...row, guildId })) {
      embed.addFields({ name: '⚠️ Never fires', value: 'This regex timed out recently and is paused for up to 10 minutes.' });
    }
  }
  embed.setFooter({ text: clampText(`ID: ${row.id} · Created by ${creator}`, 2048) });
  embed.setTimestamp(row.createdAt);
  return interaction.editReply({ embeds: [embed], allowedMentions: NO_MENTIONS });
}

// ── list (R014) ───────────────────────────────────────────────────────────────

async function list(interaction, settings) {
  const rows = await prisma.autoTrigger.findMany({
    where: { guildId: interaction.guildId },
    orderBy: [{ createdAt: 'asc' }, { id: 'asc' }],
  });
  if (!rows.length) {
    return reply(interaction, infoEmbed('No Triggers', 'No triggers yet. Add one with `/trigger add`.'));
  }
  const entries = rows.map((row, i) => (
    `\`#${i + 1}\` ${row.enabled ? '✅' : '⏸️'} **[${MATCH_LABELS[row.matchType] ?? row.matchType}]** `
    + `\`${codeSafe(clampText(oneLine(row.trigger), 40))}\`${isUnsafe(row) ? ' ⚠️ never fires' : ''}\n`
    + `↳ ${clampText(oneLine(row.response), 60)} · ${row.uses} fires · ID \`${row.id}\``
  ));
  const pages = [];
  for (let i = 0; i < entries.length; i += PAGE_SIZE) {
    pages.push(applyGuildColor(infoEmbed(
      `⚡ Triggers (${rows.length})`,
      entries.slice(i, i + PAGE_SIZE).join('\n\n'),
      'Use #number or ID in /trigger edit|remove|toggle|info',
    ), settings));
  }
  if (pages.length === 1) return interaction.editReply({ embeds: [pages[0]], allowedMentions: NO_MENTIONS });
  return paginate(interaction, pages);
}

// ── Autocomplete (R014, R071, R222) ───────────────────────────────────────────

/** Filters in the database BEFORE take(25), so old triggers are found too. Not gated by the module. */
export async function autocomplete(interaction) {
  try {
    const q = interaction.options.getFocused().trim();
    const rows = await prisma.autoTrigger.findMany({
      where: {
        guildId: interaction.guildId,
        ...(q ? {
          OR: [
            { trigger: { contains: q, mode: 'insensitive' } },
            { response: { contains: q, mode: 'insensitive' } },
            { id: { startsWith: q } },
          ],
        } : {}),
      },
      orderBy: [{ createdAt: 'asc' }, { id: 'asc' }],
      take: 25,
      select: { id: true, trigger: true, matchType: true, enabled: true },
    });
    await respondAutocomplete(interaction, rows.map((r) => ({
      name: `${r.enabled ? '✅' : '⏸️'} [${MATCH_LABELS[r.matchType] ?? r.matchType}] ${clampText(oneLine(r.trigger), 70)}`,
      value: r.id,
    })));
  } catch (err) {
    logger.debug(`/trigger autocomplete failed: ${err?.message ?? err}`);
    await respondAutocomplete(interaction, []);
  }
}
