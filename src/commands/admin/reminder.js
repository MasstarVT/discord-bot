// /reminder (pkg-reminders): scheduled channel messages (MEE6 Reminders/Timers). Thin: validation, next-run maths,
// payloads and the scheduler job live in services/reminderService.js; the message editor submit is in plugins/reminders.js.
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import prisma from '../../database/client.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { buildCustomId } from '../../config/customIds.js';
import { getJob } from '../../services/scheduler.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import {
  DEFAULT_TEMPLATE,
  MODE_LABELS,
  ReminderUserError,
  buildSendPayload,
  computeNextRun,
  createReminderRow,
  deleteReminder,
  describeSchedule,
  findReminderByName,
  jobKey,
  moduleOffLine,
  pauseReminder,
  reminderWarnings,
  resolveTimeZone,
  summaryLines,
  syncReminderJob,
  validateReminderInput,
} from '../../services/reminderService.js';
import { TimeoutError, ackBudgetMs, withTimeout } from '../../utils/async.js';
import { buildTemplateModal, normalizeTemplate, validateTemplate } from '../../utils/customEmbed.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import {
  awaitConfirm,
  checkChannelUsable,
  isInteractionGone,
  permissionNames,
  pingableRoleIds,
  replyError,
  respondAutocomplete,
} from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { discordTimestamp } from '../../utils/time.js';
import { chunkLines, clampText, joinWithLimit, sanitizeName } from '../../utils/text.js';

export const module = 'reminders';
export const gated = false; // manageable while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

// ── Builder ───────────────────────────────────────────────────────────────────
// The payload budget is tight (about 1700 of 2000 characters): do not lengthen any description.

const MODE_CHOICES = [
  { name: 'Time of day on chosen days (MEE6 Single)', value: 'time' },
  { name: 'Every interval (MEE6 Recurring)', value: 'interval' },
  { name: 'Once (date/time or delay)', value: 'once' },
];
const D = {
  channel: 'Channel to post in',
  mode: 'How it repeats',
  time: 'Time mode: e.g. 09:00 or 9:30pm (server timezone)',
  weekdays: 'mon,wed,fri · weekdays · weekends · all (default)',
  interval: 'Interval mode: e.g. 30m, 2h, 1d (5m to 7d)',
  window_start: 'Interval mode: daily start HH:MM, or none',
  window_end: 'Interval mode: daily stop HH:MM, or none',
  at: 'Once mode: 18:00, 2026-12-24 09:00, or a delay like 2h',
  require_activity: 'Skip if nobody chatted since the last one (MEE6 anti-spam)',
  ping_role: 'Role to ping',
  message: 'Plain text; skips the editor',
};
function scheduleOptions(s, { forEdit }) {
  s.addChannelOption((o) => o.setName('channel').setDescription(D.channel).setRequired(!forEdit)
    .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement));
  s.addStringOption((o) => o.setName('mode').setDescription(D.mode).setRequired(!forEdit).addChoices(...MODE_CHOICES));
  s.addStringOption((o) => o.setName('time').setDescription(D.time).setMaxLength(10));
  s.addStringOption((o) => o.setName('weekdays').setDescription(D.weekdays).setMaxLength(60));
  s.addStringOption((o) => o.setName('interval').setDescription(D.interval).setMaxLength(20));
  s.addStringOption((o) => o.setName('window_start').setDescription(D.window_start).setMaxLength(10));
  s.addStringOption((o) => o.setName('window_end').setDescription(D.window_end).setMaxLength(10));
  s.addStringOption((o) => o.setName('at').setDescription(D.at).setMaxLength(40));
  s.addBooleanOption((o) => o.setName('require_activity').setDescription(D.require_activity));
  s.addRoleOption((o) => o.setName('ping_role').setDescription(D.ping_role));
  if (forEdit) s.addBooleanOption((o) => o.setName('clear_ping_role').setDescription('Remove the ping role'));
  s.addStringOption((o) => o.setName('message').setDescription(D.message).setMaxLength(2000));
  return s;
}
const nameOpt = (desc, ac = true) => (o) => {
  o.setName('name').setDescription(desc).setRequired(true).setMaxLength(32);
  if (ac) o.setAutocomplete(true); else o.setMinLength(1);
  return o;
};

export const data = new SlashCommandBuilder()
  .setName('reminder')
  .setDescription('Scheduled channel messages (MEE6 Reminders/Timers)')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => scheduleOptions(
    s.setName('create').setDescription('Create a reminder, then write its message')
      .addStringOption(nameOpt('Unique name, e.g. daily-standup', false)), { forEdit: false }))
  .addSubcommand((s) => scheduleOptions(
    s.setName('edit').setDescription('Change a reminder; name only = open the message editor')
      .addStringOption(nameOpt('Reminder')), { forEdit: true }))
  .addSubcommand((s) => s.setName('list').setDescription("List this server's reminders"))
  .addSubcommand((s) => s.setName('pause').setDescription('Pause a reminder').addStringOption(nameOpt('Reminder')))
  .addSubcommand((s) => s.setName('resume').setDescription('Resume a paused reminder').addStringOption(nameOpt('Reminder')))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete a reminder').addStringOption(nameOpt('Reminder')))
  .addSubcommand((s) => s.setName('test').setDescription('Post a reminder here now as a preview (no pings)').addStringOption(nameOpt('Reminder')))
  .addSubcommand((s) => s.setName('next').setDescription('Show when a reminder fires next').addStringOption(nameOpt('Reminder')));

// ── Shared helpers ────────────────────────────────────────────────────────────

const REMINDER_NAME_MAX = 32;
const UNAVAILABLE_TITLE = 'Temporarily Unavailable';

/** Loads the settings before the acknowledgement: the memo first, a cold load bounded by the ack budget. */
async function loadSettings(interaction) {
  let settings = peekSettings(interaction.guildId);
  if (!settings) {
    settings = await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'reminder settings').catch(() => null);
  }
  if (!settings) {
    await replyError(interaction, UNAVAILABLE_TITLE, 'Server settings could not be loaded. Please try again in a moment.');
    return null;
  }
  return settings;
}

/** The reminder named by the `name` option, or null after replying with the reason. Bounded by the ack budget. */
async function lookupReminder(interaction) {
  const raw = interaction.options.getString('name', true);
  let r;
  try {
    r = await withTimeout(findReminderByName(interaction.guildId, raw), ackBudgetMs(interaction), 'reminder lookup');
  } catch (err) {
    if (err instanceof TimeoutError) {
      await replyError(interaction, UNAVAILABLE_TITLE, 'That took too long. Please try again.');
      return null;
    }
    throw err;
  }
  if (!r) {
    await replyError(interaction, 'Reminder Not Found', `No reminder named **${clampText(raw, REMINDER_NAME_MAX)}**. See \`/reminder list\`.`);
    return null;
  }
  return r;
}

const given = (v) => (v === null ? undefined : v);

/** The raw schedule options as validateReminderInput wants them (undefined = not provided). */
function readScheduleInput(options) {
  return {
    mode: given(options.getString('mode')),
    time: given(options.getString('time')),
    weekdays: given(options.getString('weekdays')),
    interval: given(options.getString('interval')),
    windowStart: given(options.getString('window_start')),
    windowEnd: given(options.getString('window_end')),
    at: given(options.getString('at')),
    requireActivity: given(options.getBoolean('require_activity')),
  };
}

/** Replies with a Missing Permissions error and returns false when the bot cannot post in the channel. */
async function channelCheck(interaction, channel, { embed }) {
  const needs = ['ViewChannel', 'SendMessages', ...(embed ? ['EmbedLinks'] : [])];
  const r = checkChannelUsable(channel, { needs });
  if (r.ok) return true;
  const detail = r.missing.length
    ? `I can't post in <#${channel.id}>: I need **${permissionNames(r.missing).join(', ')}** there.`
    : (r.reason ?? "I can't post in that channel.");
  await replyError(interaction, 'Missing Permissions', detail);
  return false;
}

/** The ping-role rules at configuration time: not @everyone, and pingable by the member who sets it. */
async function checkPingRole(interaction, role, channel) {
  if (role.id === interaction.guild.id) {
    await replyError(interaction, 'Invalid Role', "@everyone can't be used as a ping role.");
    return false;
  }
  const { refused } = pingableRoleIds(channel, interaction.member, [role.id]);
  if (refused.length) {
    await replyError(interaction, 'Cannot Ping Role',
      `<@&${role.id}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#${channel.id}>.`);
    return false;
  }
  return true;
}

/** Validates a template; replies with Invalid Message and returns false on errors. */
async function templateCheck(interaction, tpl) {
  const v = validateTemplate(tpl, { requireSomething: true });
  if (v.ok) return true;
  await replyError(interaction, 'Invalid Message', joinWithLimit(v.errors, { sep: '\n', max: 1000 }));
  return false;
}

const modalFor = (r, name, template) => buildTemplateModal({
  customId: buildCustomId('reminder-modal', r.id),
  title: `Reminder: ${name}`,
  template,
});

const summaryText = (r, settings, next, warnings) => clampText(summaryLines(r, settings, next, warnings).join('\n'), 4096);

// ── create ────────────────────────────────────────────────────────────────────

async function createCmd(interaction) {
  const { guild, guildId } = interaction;
  const name = sanitizeName(interaction.options.getString('name', true), { max: REMINDER_NAME_MAX, fallback: '' });
  if (!name) return replyError(interaction, 'Invalid Name', 'Use letters, numbers, `-` or `_` in the name.');

  const settings = await loadSettings(interaction);
  if (!settings) return undefined;
  const tz = resolveTimeZone(settings.timezone);
  const now = new Date();

  const v = validateReminderInput(readScheduleInput(interaction.options), { tz, now, base: null });
  if (!v.ok) return replyError(interaction, 'Invalid Schedule', v.error);

  const channel = guild.channels.cache.get(interaction.options.getChannel('channel', true).id);
  if (!channel) return replyError(interaction, 'Channel Not Found', "I can't see that channel.");
  if (!(await channelCheck(interaction, channel, { embed: false }))) return undefined;

  const role = interaction.options.getRole('ping_role');
  if (role && !(await checkPingRole(interaction, role, channel))) return undefined;

  const message = interaction.options.getString('message');
  let template = { ...DEFAULT_TEMPLATE, buttons: [] };
  if (message !== null) {
    template = normalizeTemplate({ content: message });
    if (!(await templateCheck(interaction, template))) return undefined;
  }

  const args = {
    guildId, name, channelId: channel.id, template, fields: v.fields, pingRoleId: role?.id ?? null, createdBy: interaction.user.id, now,
  };

  if (message === null) {
    // The modal path: the reminder is saved first and is already active with the default text; the form then edits it.
    const createPromise = createReminderRow(args);
    // Scheduling runs whether or not the form opens. A cap or duplicate-name rejection is reported below, not logged.
    createPromise.then((row) => syncReminderJob(row, settings)).catch((err) => {
      if (!(err instanceof ReminderUserError)) logger.error('reminder create: scheduling failed', err);
    });
    let row;
    try {
      row = await withTimeout(createPromise, ackBudgetMs(interaction), 'reminder create');
    } catch (err) {
      if (err instanceof ReminderUserError) return replyError(interaction, err.title, err.message);
      if (err instanceof TimeoutError) {
        return replyError(interaction, 'Still Saving',
          `Saving took too long, so I could not open the message editor. Check \`/reminder list\` in a moment, then run \`/reminder edit name:${name}\` to write the message.`);
      }
      throw err;
    }
    // The modal is the first response. If the admin dismisses it, the default MEE6 text is what gets sent.
    return interaction.showModal(modalFor(row, name, { ...DEFAULT_TEMPLATE, buttons: [] }));
  }

  await interaction.deferReply();
  let row;
  try {
    row = await createReminderRow(args);
  } catch (err) {
    if (err instanceof ReminderUserError) return interaction.editReply({ embeds: [errorEmbed(err.title, err.message)] });
    throw err;
  }
  const next = await syncReminderJob(row, settings);
  const warnings = reminderWarnings(row, guild, template);
  return interaction.editReply({ embeds: [successEmbed('Reminder Created', summaryText(row, settings, next, warnings))] });
}

// ── edit ──────────────────────────────────────────────────────────────────────

async function editCmd(interaction) {
  const { guild } = interaction;
  const r = await lookupReminder(interaction);
  if (!r) return undefined;

  const provided = (interaction.options.data[0]?.options ?? []).filter((o) => o.name !== 'name');
  if (provided.length === 0) {
    // Name only: open the message editor (the modal is the first response).
    return interaction.showModal(modalFor(r, r.name, normalizeTemplate(r.template)));
  }

  const settings = await loadSettings(interaction);
  if (!settings) return undefined;
  const tz = resolveTimeZone(settings.timezone);
  const now = new Date();

  // Schedule options. With none given, the schedule is untouched (so a finished one-off can still change channel).
  const input = readScheduleInput(interaction.options);
  let fields = null;
  if (Object.values(input).some((x) => x !== undefined)) {
    const v = validateReminderInput(input, { tz, now, base: r });
    if (!v.ok) return replyError(interaction, 'Invalid Schedule', v.error);
    fields = v.fields;
  }

  const currentTpl = normalizeTemplate(r.template);
  const optChannel = interaction.options.getChannel('channel');
  let channel = null;
  if (optChannel) {
    channel = guild.channels.cache.get(optChannel.id);
    if (!channel) return replyError(interaction, 'Channel Not Found', "I can't see that channel.");
    if (!(await channelCheck(interaction, channel, { embed: Boolean(currentTpl.embed) }))) return undefined;
  }

  const role = interaction.options.getRole('ping_role');
  const clearRole = interaction.options.getBoolean('clear_ping_role') === true;
  if (role && clearRole) return replyError(interaction, 'Conflicting Options', 'Use either `ping_role` or `clear_ping_role`, not both.');
  if (role) {
    // The channel the role will be pinged in: the new one, or the reminder's current one.
    const effective = channel ?? guild.channels.cache.get(r.channelId);
    if (!effective) {
      return replyError(interaction, 'Channel Missing',
        `Its channel no longer exists. Set a new one with \`/reminder edit name:${r.name} channel:<#channel>\`.`);
    }
    if (!(await checkPingRole(interaction, role, effective))) return undefined;
  }

  const message = interaction.options.getString('message');
  let template = null;
  if (message !== null) {
    template = normalizeTemplate({ ...currentTpl, content: message });
    if (!(await templateCheck(interaction, template))) return undefined;
  }

  await interaction.deferReply();

  const update = { ...(fields ?? {}) };
  if (channel) {
    update.channelId = channel.id;
    if (channel.id !== r.channelId) update.lastMessageId = null; // the baseline of the activity gate belongs to the old channel
  }
  if (clearRole) update.pingRoleId = null;
  else if (role) update.pingRoleId = role.id;
  if (template) update.template = template;

  const updated = await prisma.reminder.update({ where: { id: r.id }, data: update });
  const next = updated.enabled ? await syncReminderJob(updated, settings) : null; // edit never changes `enabled`
  const warnings = reminderWarnings(updated, guild, template ?? currentTpl);
  return interaction.editReply({ embeds: [successEmbed('Reminder Updated', summaryText(updated, settings, next, warnings))] });
}

// ── list ──────────────────────────────────────────────────────────────────────

const isDone = (r) => !r.enabled && r.mode === 'ONE_SHOT' && Boolean(r.lastSentAt) && Boolean(r.runAt) && r.lastSentAt >= r.runAt;

async function listCmd(interaction) {
  const { guild, guildId } = interaction;
  const settings = await loadSettings(interaction);
  if (!settings) return undefined;
  const tz = resolveTimeZone(settings.timezone);
  await interaction.deferReply();

  const rows = await prisma.reminder.findMany({ where: { guildId }, orderBy: { name: 'asc' }, take: 200 });
  if (!rows.length) {
    return interaction.editReply({ embeds: [infoEmbed('Reminders', 'No reminders yet. Create one with `/reminder create`.')] });
  }

  const now = new Date();
  const entries = rows.map((r) => {
    const done = isDone(r);
    const channel = guild.channels.cache.get(r.channelId);
    let icon = '▶️';
    if (done) icon = '✅';
    else if (!r.enabled) icon = '⏸️';
    else if (!channel || !checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] }).ok) icon = '⚠️';

    let nextText;
    if (r.enabled) {
      const next = computeNextRun(r, tz, now);
      nextText = `next ${next ? discordTimestamp(next, 'R') : '—'}`;
    } else if (done) {
      nextText = `sent ${discordTimestamp(r.lastSentAt, 'R')}`;
    } else {
      nextText = 'paused';
    }
    if (r.lastSentAt && !done) nextText += ` · last sent ${discordTimestamp(r.lastSentAt, 'R')}`;

    const head = `${icon} **${r.name}** · <#${r.channelId}> · ${describeSchedule(r)}${r.requireActivity ? ' · 💬' : ''}${r.pingRoleId ? ` · <@&${r.pingRoleId}>` : ''}`;
    return `${head}\n↳ ${nextText}`;
  });

  const off = moduleOffLine(settings);
  const footer = `${rows.length}/${LIMITS.REMINDERS} reminders · Timezone: ${tz}`;
  const pages = chunkLines(entries, 3500).map((chunk) => guildEmbed(settings, '⏰ Reminders', (off ? `${off}\n\n` : '') + chunk, footer));
  if (pages.length === 1) return interaction.editReply({ embeds: pages });
  return paginate(interaction, pages);
}

// ── pause / resume / delete ───────────────────────────────────────────────────

async function pauseCmd(interaction) {
  const r = await lookupReminder(interaction);
  if (!r) return undefined;
  if (!r.enabled) return replyError(interaction, 'Already Paused', `**${r.name}** is already paused.`);
  await interaction.deferReply();
  await pauseReminder(r);
  return interaction.editReply({
    embeds: [successEmbed('Reminder Paused', `**${r.name}** will not send until you run \`/reminder resume\`.`)],
  });
}

async function resumeCmd(interaction) {
  const { guild } = interaction;
  const r = await lookupReminder(interaction);
  if (!r) return undefined;
  const settings = await loadSettings(interaction);
  if (!settings) return undefined;
  const tz = resolveTimeZone(settings.timezone);
  const now = new Date();
  const tpl = normalizeTemplate(r.template);

  if (r.mode === 'ONE_SHOT' && (!r.runAt || r.runAt.getTime() <= now.getTime() + 60_000)) {
    return replyError(interaction, 'Time Has Passed',
      `That one-time reminder's time has passed. Set a new time with \`/reminder edit name:${r.name} at:<when>\`, then resume it.`);
  }

  let channel = guild.channels.cache.get(r.channelId);
  if (!channel) {
    try {
      channel = await withTimeout(guild.channels.fetch(r.channelId), ackBudgetMs(interaction), 'reminder channel fetch');
    } catch (err) {
      if (err?.code !== 10003 && err?.code !== 50001) return replyError(interaction, UNAVAILABLE_TITLE, 'That took too long. Please try again.');
      channel = null;
    }
  }
  if (!channel) {
    return replyError(interaction, 'Channel Missing',
      `Its channel no longer exists. Set a new one with \`/reminder edit name:${r.name} channel:<#channel>\`.`);
  }
  if (!(await channelCheck(interaction, channel, { embed: Boolean(tpl.embed) }))) return undefined;
  if (computeNextRun(r, tz, now) === null) {
    return replyError(interaction, 'Never Fires', 'This schedule never fires. Check the weekdays and the time window with `/reminder edit`.');
  }

  await interaction.deferReply();
  // On a reminder that is already enabled this is a refresh: it re-syncs the job (e.g. after /settings timezone).
  const refresh = r.enabled;
  const updated = refresh ? r : await prisma.reminder.update({ where: { id: r.id }, data: { enabled: true } });
  const next = await syncReminderJob(updated, settings);
  return interaction.editReply({
    embeds: [successEmbed(refresh ? 'Schedule Refreshed' : 'Reminder Resumed', summaryText(updated, settings, next, reminderWarnings(updated, guild, tpl)))],
  });
}

async function deleteCmd(interaction) {
  const r = await lookupReminder(interaction);
  if (!r) return undefined;
  const ok = await awaitConfirm(interaction, {
    title: 'Delete reminder?',
    description: `**${r.name}** in <#${r.channelId}> will be deleted. This can't be undone.`,
    confirmLabel: 'Delete',
    ephemeral: true,
  });
  if (!ok) return undefined; // awaitConfirm already showed the outcome
  await deleteReminder(r);
  return interaction.followUp({ embeds: [successEmbed('Reminder Deleted', `**${r.name}** was deleted.`)] });
}

// ── test / next ───────────────────────────────────────────────────────────────

async function testCmd(interaction) {
  const { guild } = interaction;
  const r = await lookupReminder(interaction);
  if (!r) return undefined;
  await interaction.deferReply({ ephemeral: true });
  const settings = peekSettings(interaction.guildId) ?? await getSettingsSafe(interaction.guildId); // only the embed colour

  // A preview never touches lastSentAt, lastMessageId or the job, and works while the module is off.
  const target = guild.channels.cache.get(r.channelId) ?? interaction.channel;
  const payload = buildSendPayload(r, { guild, channel: target, settings, ping: false });
  const here = interaction.channel;
  const usable = checkChannelUsable(here, {
    needs: ['ViewChannel', 'SendMessages', ...(payload.embeds.length ? ['EmbedLinks'] : [])],
  });
  if (usable.ok) {
    try {
      await here.send(payload);
      return interaction.editReply({
        embeds: [successEmbed('Test Sent',
          `Posted a preview of **${r.name}** here. Scheduled sends go to <#${r.channelId}>; role pings are suppressed in tests.`)],
      });
    } catch (err) {
      logger.warn(`reminder test: send failed in ${here.id}: ${err?.message}`);
    }
  }
  return interaction.editReply({
    content: payload.content ?? undefined,
    embeds: [...payload.embeds, warnEmbed('Preview Only', "I can't post in this channel, so here is a private preview.")].slice(0, 10),
    components: payload.components,
    allowedMentions: { parse: [] },
  });
}

async function nextCmd(interaction) {
  const r = await lookupReminder(interaction);
  if (!r) return undefined;
  const settings = await loadSettings(interaction);
  if (!settings) return undefined;
  const tz = resolveTimeZone(settings.timezone);
  await interaction.deferReply();

  const lines = [`**${r.name}** → <#${r.channelId}>`, describeSchedule(r), `Timezone: ${tz}`];
  if (!r.enabled) {
    lines.push(isDone(r) ? `✅ Sent ${discordTimestamp(r.lastSentAt, 'R')}` : '⏸️ Paused — no upcoming runs.');
  } else {
    const job = await getJob(jobKey(r.id));
    let next;
    if (!job || (job.status !== 'PENDING' && job.status !== 'RUNNING')) {
      next = await syncReminderJob(r, settings);
      lines.push('🔧 The schedule was repaired.');
    } else {
      next = job.runAt;
    }
    if (next) {
      lines.push(`Next: ${discordTimestamp(next, 'F')} (${discordTimestamp(next, 'R')})`);
      if (r.mode !== 'ONE_SHOT') {
        const upcoming = [];
        let prev = next;
        for (let i = 0; i < 3; i++) {
          prev = computeNextRun(r, tz, prev);
          if (!prev) break;
          upcoming.push(discordTimestamp(prev, 'f'));
        }
        if (upcoming.length) lines.push(`Upcoming: ${upcoming.join(' · ')}`);
      }
    } else {
      lines.push('Next: —');
    }
    if (r.requireActivity) lines.push('💬 Skipped when nobody has chatted since the last reminder.');
  }
  const off = moduleOffLine(settings);
  if (off) lines.push(off);
  return interaction.editReply({ embeds: [guildEmbed(settings, '⏰ Next run', lines.join('\n'))] });
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

const HANDLERS = {
  create: createCmd,
  edit: editCmd,
  list: listCmd,
  pause: pauseCmd,
  resume: resumeCmd,
  delete: deleteCmd,
  test: testCmd,
  next: nextCmd,
};

export async function execute(interaction, client) {
  const sub = interaction.options.getSubcommand(false);
  try {
    const handler = Object.hasOwn(HANDLERS, sub) ? HANDLERS[sub] : null;
    if (!handler) return await replyError(interaction, 'Unknown Subcommand', 'That subcommand does not exist.');
    return await handler(interaction, client);
  } catch (err) {
    if (isInteractionGone(err)) return undefined;
    logger.error(`reminder ${sub} failed`, err);
    // replyError answers in the interaction's current state (reply, editReply after a defer, or followUp).
    return replyError(interaction, 'Something went wrong', 'I could not complete that. Please try again.');
  }
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

export async function autocomplete(interaction) {
  const focused = interaction.options.getFocused(true);
  if (focused.name !== 'name' || !interaction.guildId) return respondAutocomplete(interaction, []);
  const sub = interaction.options.getSubcommand(false);
  const q = sanitizeName(focused.value ?? '', { max: REMINDER_NAME_MAX, fallback: '' });
  const where = {
    guildId: interaction.guildId,
    ...(q ? { name: { contains: q, mode: 'insensitive' } } : {}),
    ...(sub === 'pause' ? { enabled: true } : {}),
  };
  const rows = await withTimeout(
    prisma.reminder.findMany({ where, select: { name: true, enabled: true, mode: true }, orderBy: { name: 'asc' }, take: 25 }),
    2000,
    'reminder autocomplete',
  ).catch(() => []);
  return respondAutocomplete(interaction, rows.map((r) => ({
    name: `${r.enabled ? '▶' : '⏸'} ${r.name} · ${MODE_LABELS[r.mode]}`,
    value: r.name,
  })));
}
