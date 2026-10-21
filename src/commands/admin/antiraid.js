// /antiraid status | lockdown start | lockdown end | config. Admin command: replies are public (CLAUDE.md admin
// convention) and it keeps working while the Anti-Raid module is off (gated = false), so a lockdown can be started or
// ended and the module set up first. Detection and the actions on joiners are src/plugins/antiraid.js.
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe, updateSettings } from '../../services/settingsService.js';
import {
  RAID, announceEnd, announceStart, buildEndEmbed, buildStartEmbed, configLines, endLockdown, isActive, loadState,
  permissionProblems, startLockdown,
} from '../../services/antiRaidService.js';
import { successEmbed, errorEmbed, infoEmbed, warnEmbed, guildEmbed } from '../../utils/embedBuilder.js';
import { safeDefer, safeReply, checkChannelUsable } from '../../utils/interactionHelpers.js';
import { clampText } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';
import logger from '../../utils/logger.js';

export const module = 'antiraid';
export const gated = false;                          // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const ACTION_CHOICES = [
  { name: 'Alert only', value: 'ALERT' },
  { name: 'Kick', value: 'KICK' },
  { name: 'Ban', value: 'BAN' },
];

export const data = new SlashCommandBuilder()
  .setName('antiraid')
  .setDescription('Raid protection: join floods, lockdowns and brand-new accounts.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('status').setDescription('Show the lockdown, settings and problems.'))
  .addSubcommandGroup((g) => g.setName('lockdown').setDescription('Start or end a lockdown by hand.')
    .addSubcommand((s) => s.setName('start').setDescription('Start a lockdown now.')
      .addIntegerOption((o) => o.setName('minutes').setDescription('Minutes (default: the setting).')
        .setMinValue(RAID.MINUTES.min).setMaxValue(RAID.MINUTES.max))
      .addStringOption((o) => o.setName('reason').setDescription('Why (shown in the alert).').setMaxLength(RAID.REASON_MAX)))
    .addSubcommand((s) => s.setName('end').setDescription('End it and undo what it changed.')))
  .addSubcommand((s) => s.setName('config').setDescription('Change the settings (no options: show them).')
    .addBooleanOption((o) => o.setName('enabled').setDescription('Detection on or off.'))
    .addIntegerOption((o) => o.setName('joins').setDescription('Joins that count as a raid.')
      .setMinValue(RAID.THRESHOLD.min).setMaxValue(RAID.THRESHOLD.max))
    .addIntegerOption((o) => o.setName('seconds').setDescription('Window the joins are counted in.')
      .setMinValue(RAID.SECONDS.min).setMaxValue(RAID.SECONDS.max))
    .addStringOption((o) => o.setName('action').setDescription('Members who join during a raid.').addChoices(...ACTION_CHOICES))
    .addBooleanOption((o) => o.setName('pause_invites').setDescription('Pause invites during a lockdown.'))
    .addBooleanOption((o) => o.setName('lock_chat').setDescription('Take Send Messages from @everyone.'))
    .addIntegerOption((o) => o.setName('minutes').setDescription('How long a lockdown lasts.')
      .setMinValue(RAID.MINUTES.min).setMaxValue(RAID.MINUTES.max))
    .addIntegerOption((o) => o.setName('min_account_age').setDescription('Days; younger accounts get young_action (0 = off).')
      .setMinValue(RAID.AGE_DAYS.min).setMaxValue(RAID.AGE_DAYS.max))
    .addStringOption((o) => o.setName('young_action').setDescription('Action for too-young accounts.').addChoices(...ACTION_CHOICES))
    .addChannelOption((o) => o.setName('alert_channel').setDescription('Alert channel (default: the mod log).')
      .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)));

const MSG = {
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'),
  COULD_NOT_END: () => errorEmbed('Could Not End Lockdown', 'The lockdown could not be ended. Please try again in a moment.'),
  CANNOT_USE: (reason) => errorEmbed('Cannot Use That Channel', `${reason} Pick another channel, or fix my permissions there.`),
};

const OFF_NOTE = '⚠️ The **Anti-Raid** module is off: raids are not detected and members who join are not actioned. '
  + 'Turn it on with `/antiraid config enabled:true` or `/module enable antiraid`.';

/** Final answer: overwrites the deferred reply. Never pings. */
function finish(interaction, embed) {
  return safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
}

// ── /antiraid status ──────────────────────────────────────────────────────────

async function runStatus(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());

  let row = null;
  let loaded = true;
  try {
    row = await loadState(guildId);
  } catch (err) {
    logger.warn(`[antiraid] ${guildId}: could not load the lockdown: ${err?.message}`);
    loaded = false;
  }
  let lockdown = 'Unavailable';
  if (loaded) {
    if (isActive(row)) {
      const until = new Date(row.lockedUntil);
      lockdown = `🔒 On until ${discordTimestamp(until, 'R')}, ${row.lockedById ? `started by <@${row.lockedById}>` : 'started by a detected raid'}`
        + `${row.reason ? `: ${clampText(row.reason, RAID.REASON_MAX)}` : ''}`;
    } else if (row?.lockedAt) {
      lockdown = '🔒 Ran out and is being ended';
    } else {
      lockdown = '🔓 Off';
    }
  }

  const embed = guildEmbed(settings, '🚨 Anti-Raid').addFields(
    { name: 'Detection', value: isModuleEnabled(settings, 'antiraid') ? '✅ On' : '❌ Off: `/antiraid config enabled:true`', inline: true },
    { name: 'Lockdown', value: lockdown, inline: true },
    { name: 'Settings', value: configLines(settings).join('\n') },
  );
  const problems = permissionProblems(guild, settings);
  const alertId = settings.antiRaidAlertChannelId ?? settings.modLogChannelId;
  if (!alertId) problems.push('No alert channel: set one with `/antiraid config alert_channel:`, or a moderation log channel.');
  else if (!checkChannelUsable(guild.channels.cache.get(alertId)).ok) problems.push(`I cannot post alerts in <#${alertId}>.`);
  if (loaded && !row?.lockedAt && row?.priorEveryoneSend != null) {
    problems.push('@everyone still lacks some send permissions from an earlier lockdown: run `/antiraid lockdown end` to give them back.');
  }
  if (problems.length) embed.addFields({ name: 'Needs attention', value: clampText(problems.map((p) => `• ${p}`).join('\n'), 1024) });
  return finish(interaction, embed);
}

// ── /antiraid lockdown start | end ────────────────────────────────────────────

async function runStart(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());

  const res = await startLockdown({
    guild, settings, startedById: interaction.user.id,
    reason: interaction.options.getString('reason'), minutes: interaction.options.getInteger('minutes'),
  });
  if (!res.ok) return finish(interaction, errorEmbed('Could Not Start Lockdown', res.error));
  if (!res.started) {
    return finish(interaction, warnEmbed('Already Locked Down',
      `A lockdown is already on until ${discordTimestamp(new Date(res.state.lockedUntil), 'R')}. End it with \`/antiraid lockdown end\`.`));
  }
  logger.info(`[antiraid] ${guildId}: lockdown started by ${interaction.user.id}`);

  const posted = await announceStart(guild, settings, res);
  if (posted?.channel?.id === interaction.channelId) {
    return finish(interaction, successEmbed('Lockdown Started', 'The alert above has the details and the button to end it.'));
  }
  const embed = buildStartEmbed({ state: res.state, settings, effects: res.effects, problems: permissionProblems(guild, settings) });
  const notes = [];
  if (!posted) notes.push('No alert was posted: I have no usable alert channel (`/antiraid config alert_channel:`).');
  if (!isModuleEnabled(settings, 'antiraid')) notes.push(OFF_NOTE);
  return safeReply(interaction, {
    content: notes.join('\n') || undefined, embeds: [embed], allowedMentions: { parse: [] },
  });
}

async function runEnd(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  let res;
  try {
    res = await endLockdown({ guild, settings });
  } catch (err) {
    logger.error(`[antiraid] ${guildId}: ending the lockdown failed`, err);
    return finish(interaction, MSG.COULD_NOT_END());
  }
  if (!res.wasActive && !res.restored && !res.pendingRestore) {
    return finish(interaction, infoEmbed('No Lockdown', 'There is no lockdown to end.'));
  }
  logger.info(`[antiraid] ${guildId}: lockdown ended by ${interaction.user.id}`);
  await announceEnd(guild, settings, res, { how: 'manual', endedById: interaction.user.id, exceptChannelId: interaction.channelId });
  return finish(interaction, buildEndEmbed({ how: 'manual', endedById: interaction.user.id, result: res }));
}

// ── /antiraid config ──────────────────────────────────────────────────────────

const OPTION_FIELDS = Object.freeze({
  enabled: ['getBoolean', 'antiRaidEnabled'],
  joins: ['getInteger', 'antiRaidJoinThreshold'],
  seconds: ['getInteger', 'antiRaidJoinSeconds'],
  action: ['getString', 'antiRaidAction'],
  pause_invites: ['getBoolean', 'antiRaidPauseInvites'],
  lock_chat: ['getBoolean', 'antiRaidLockChat'],
  minutes: ['getInteger', 'antiRaidLockdownMinutes'],
  min_account_age: ['getInteger', 'antiRaidMinAccountAgeDays'],
  young_action: ['getString', 'antiRaidYoungAction'],
});

async function runConfig(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId, options } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());

  const changes = {};
  for (const [option, [getter, field]] of Object.entries(OPTION_FIELDS)) {
    const value = options[getter](option);
    if (value !== null && value !== undefined) changes[field] = value;
  }
  const picked = options.getChannel('alert_channel');
  if (picked) {
    const channel = guild.channels.cache.get(picked.id) ?? null;
    const usable = checkChannelUsable(channel);
    if (!usable.ok) return finish(interaction, MSG.CANNOT_USE(usable.reason));
    changes.antiRaidAlertChannelId = channel.id;
  }

  if (!Object.keys(changes).length) {
    return finish(interaction, guildEmbed(settings, '🚨 Anti-Raid Settings',
      `${configLines(settings).join('\n')}\n\nChange them with the options of this command, or on the dashboard.`));
  }
  let updated;
  try {
    updated = await updateSettings(guildId, changes);
  } catch (err) {
    logger.error(`[antiraid] ${guildId}: config failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  logger.info(`[antiraid] ${guildId}: settings ${Object.keys(changes).join(', ')} changed by ${interaction.user.id}`);

  const parts = [configLines(updated).join('\n')];
  const problems = permissionProblems(guild, updated);
  if (problems.length) parts.push(problems.map((p) => `⚠️ ${p}`).join('\n'));
  if (!isModuleEnabled(updated, 'antiraid')) parts.push(OFF_NOTE);
  return finish(interaction, successEmbed('Anti-Raid Updated', parts.join('\n\n')));
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  if (group === 'lockdown') return sub === 'start' ? runStart(interaction) : runEnd(interaction);
  if (sub === 'status') return runStatus(interaction);
  if (sub === 'config') return runConfig(interaction);
}
