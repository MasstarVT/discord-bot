// /event (module `events`): community events with RSVP buttons, an optional Discord Scheduled Event, a role for the members
// who are going and a reminder before the start. This file is thin: option parsing, validation and replies. Posting, RSVPs,
// the jobs, editing and cancelling live in services/eventService.js. `edit` opens a form (no I/O beyond a bounded lookup
// before showModal); every reply is private, the announcement and the reminder are the public output.
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import prisma from '../../database/client.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import {
  EVENT, attendeeLines, buildEditModal, cancelEvent, createEvent, findEventsForAutocomplete, getTally, isOpen, jumpUrl, listLine,
  listOpenEvents, loadZone, parseEventFields, remindAt, resolveEventRef,
} from '../../services/eventService.js';
import { TimeoutError, ackBudgetMs, withTimeout } from '../../utils/async.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import {
  checkAssignableRole, checkChannelUsable, isInteractionGone, respondAutocomplete, safeDefer, safeReply,
} from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { formatDuration } from '../../utils/parseDuration.js';
import { chunkLines, clampText } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';
import { zoneLabel, zoneSourceText } from '../../utils/timezones.js';

export const module = 'events';
export const gated = true;
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;
// No allowInDMs export: buildCommandPayload registers the command for servers only.

// ── Builder ───────────────────────────────────────────────────────────────────
// The payload budget is tight (about 1800 of 2000 characters): do not lengthen any description.

const eventOption = (o) => o.setName('event').setDescription('Event').setRequired(true).setAutocomplete(true).setMaxLength(100);
const TEXT_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement];

export const data = new SlashCommandBuilder()
  .setName('event')
  .setDescription('Run events with RSVP buttons, an optional Discord event and a reminder')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addSubcommand((s) => s.setName('create').setDescription('Post an event with RSVP buttons')
    .addStringOption((o) => o.setName('title').setDescription('Event name').setRequired(true).setMaxLength(EVENT.TITLE_MAX))
    .addStringOption((o) => o.setName('start').setDescription('When: fri 7pm, oct 30 18:00, in 3d').setRequired(true).setMaxLength(100))
    .addStringOption((o) => o.setName('duration').setDescription('How long: 2h, 1d (max 14d)').setMaxLength(40))
    .addStringOption((o) => o.setName('description').setDescription('What it is about').setMaxLength(EVENT.DESCRIPTION_MAX))
    .addStringOption((o) => o.setName('location').setDescription('Where (needed for a Discord event)').setMaxLength(EVENT.LOCATION_MAX))
    .addChannelOption((o) => o.setName('channel').setDescription('Where to post it').addChannelTypes(...TEXT_TYPES))
    .addRoleOption((o) => o.setName('role').setDescription('Role for members who are going'))
    .addIntegerOption((o) => o.setName('capacity').setDescription('Most members who can go')
      .setMinValue(1).setMaxValue(EVENT.CAPACITY_MAX))
    .addIntegerOption((o) => o.setName('reminder').setDescription('Minutes before the start to ping (0 = none)')
      .setMinValue(0).setMaxValue(EVENT.REMINDER_MAX)))
  .addSubcommand((s) => s.setName('edit').setDescription('Edit an event in a form')
    .addStringOption(eventOption)
    .addIntegerOption((o) => o.setName('capacity').setDescription('Most members who can go (0 = no limit)')
      .setMinValue(0).setMaxValue(EVENT.CAPACITY_MAX))
    .addIntegerOption((o) => o.setName('reminder').setDescription('Minutes before the start to ping (0 = none)')
      .setMinValue(0).setMaxValue(EVENT.REMINDER_MAX)))
  .addSubcommand((s) => s.setName('cancel').setDescription('Cancel an event').addStringOption(eventOption))
  .addSubcommand((s) => s.setName('list').setDescription('List the open events'))
  .addSubcommand((s) => s.setName('info').setDescription('Show an event').addStringOption(eventOption))
  .addSubcommand((s) => s.setName('attendees').setDescription('List who is going').addStringOption(eventOption));

// ── Helpers ───────────────────────────────────────────────────────────────────

const NO_MENTIONS = Object.freeze({ parse: [] });
const notFound = () => errorEmbed('Event Not Found', 'Pick an event from the list.');
const closed = () => warnEmbed('Event Closed', 'That event has ended or was cancelled.');

const STATUS_EMOJI = Object.freeze({ SCHEDULED: '🟢', ACTIVE: '🟢', ENDED: '🏁', CANCELLED: '🚫' });
const STATUS_TEXT = Object.freeze({ SCHEDULED: 'Scheduled', ACTIVE: 'Active', ENDED: 'Ended', CANCELLED: 'Cancelled' });

/** Can the member post in the channel (a channel they chose, so a moderator cannot post where they could not)? */
function memberCanPost(channel, member) {
  const need = channel.isThread?.() ? ['ViewChannel', 'SendMessagesInThreads'] : ['ViewChannel', 'SendMessages'];
  return channel.permissionsFor?.(member)?.has(need.map((k) => PermissionFlagsBits[k])) === true;
}

const eventRef = (interaction) => interaction.options.getString('event', true);

// ── create ────────────────────────────────────────────────────────────────────

async function create(interaction, client) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const { guild, options, member } = interaction;

  const { resolved, settings, personalUnknown } = await loadZone(interaction);
  const read = parseEventFields({
    title: options.getString('title', true), start: options.getString('start', true), duration: options.getString('duration'),
    description: options.getString('description'), location: options.getString('location'),
  }, { resolved });
  if (!read.ok) return reply(errorEmbed(read.title, read.text));
  const { values } = read;

  // Where to post: the option, else the configured announcement channel, else here.
  const picked = options.getChannel('channel');
  const channel = picked
    ? (guild.channels.cache.get(picked.id) ?? picked)
    : ((settings?.eventChannelId && guild.channels.cache.get(settings.eventChannelId))
      || interaction.channel || guild.channels.cache.get(interaction.channelId) || null);
  const usable = checkChannelUsable(channel);
  if (!usable.ok) return reply(errorEmbed('Cannot Post There', usable.reason ?? 'I cannot post in that channel.'));
  if (picked && !memberCanPost(channel, member)) {
    return reply(errorEmbed('Cannot Post There', `You cannot send messages in <#${channel.id}>.`));
  }

  const roleOpt = options.getRole('role');
  if (roleOpt) {
    // A Moderator-level caller must not hand out a role they could not give themselves (the role is added by the bot).
    const mayGive = member.id === guild.ownerId || member.permissions?.has(PermissionFlagsBits.ManageRoles) === true;
    if (!mayGive) return reply(errorEmbed('Invalid Role', 'You need the **Manage Roles** permission to attach a role to an event.'));
    const check = checkAssignableRole(guild, guild.roles.cache.get(roleOpt.id) ?? roleOpt, { executor: member });
    if (!check.ok) return reply(errorEmbed('Invalid Role', check.reason ?? 'That role cannot be given.'));
  }

  const fallback = Number.isInteger(settings?.eventReminderMinutes) ? settings.eventReminderMinutes : 15;
  const out = await createEvent({
    client, guild, channel, settings,
    data: {
      title: values.title, description: values.description, location: values.location, startsAt: values.startsAt, endsAt: values.endsAt,
      hostId: interaction.user.id, roleId: roleOpt?.id ?? null, capacity: options.getInteger('capacity') ?? null,
      reminderMinutes: Math.min(Math.max(options.getInteger('reminder') ?? fallback, 0), EVENT.REMINDER_MAX),
    },
  });
  if (!out.ok) {
    if (out.code === 'LIMIT') {
      return reply(errorEmbed('Too Many Events', `This server already has ${EVENT.ACTIVE_MAX} open events. Cancel one with \`/event cancel\` first.`));
    }
    return reply(errorEmbed('Could Not Create The Event', out.code === 'SEND'
      ? `I could not post in <#${channel.id}>. Check that I can send messages and embeds there.`
      : 'The event could not be saved. Please try again.'));
  }

  const { event, notes } = out;
  const lines = [
    `**${clampText(event.title, 100)}** is posted in <#${event.channelId}>${jumpUrl(event) ? `: [jump to it](${jumpUrl(event)})` : '.'}`,
    `Starts ${discordTimestamp(event.startsAt, 'F')} (${discordTimestamp(event.startsAt, 'R')}).`,
  ];
  if (!values.zoneFromInput) {
    lines.push(`The start was read in ${zoneLabel(resolved.zone)}, ${zoneSourceText(resolved)}.`);
    if (personalUnknown) lines.push('⚠️ Your saved timezone could not be loaded just now, so another one was used.');
  }
  if (event.discordEventId) lines.push('A Discord event was created too.');
  if (event.reminderMinutes > 0) {
    lines.push(remindAt(event).getTime() > Date.now()
      ? `Members who are going are pinged ${event.reminderMinutes} minutes before the start.`
      : 'The reminder time has already passed, so no reminder will be sent.');
  }
  for (const n of notes) lines.push(`⚠️ ${n}`);
  return reply(successEmbed('Event Created', lines.join('\n')));
}

// ── edit ──────────────────────────────────────────────────────────────────────

/** The form: nothing is acknowledged before showModal, so the lookups are bounded by the ack budget. */
async function edit(interaction) {
  const { guildId } = interaction;
  const deny = (embed) => safeReply(interaction, { embeds: [embed], ephemeral: true });
  let found;
  try {
    const budget = ackBudgetMs(interaction);
    found = await withTimeout(
      Promise.all([resolveEventRef(guildId, eventRef(interaction)), loadZone(interaction, { budgetMs: budget })]),
      budget,
      'event edit lookup',
    );
  } catch (err) {
    if (err instanceof TimeoutError) return deny(errorEmbed('Temporarily Unavailable', 'That took too long. Please try again.'));
    throw err;
  }
  const [event, { resolved }] = found;
  if (!event) return deny(notFound());
  if (!isOpen(event)) return deny(closed());
  try {
    await interaction.showModal(buildEditModal(event, resolved.zone, {
      capacity: interaction.options.getInteger('capacity'), reminder: interaction.options.getInteger('reminder'),
    }));
  } catch (err) {
    if (!isInteractionGone(err)) logger.error('event edit: showModal failed', err);
  }
  return undefined;
}

// ── cancel ────────────────────────────────────────────────────────────────────

async function cancel(interaction, client) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const { guildId } = interaction;
  const event = await resolveEventRef(guildId, eventRef(interaction));
  if (!event) return reply(notFound());
  const res = await cancelEvent(client, event.id, { guildId, reason: 'manual' });
  if (!res.ok) return reply(res.code === 'CLOSED' ? closed() : notFound());
  const lines = [`**${clampText(event.title, 100)}** is cancelled. Its post is updated and its reminder is removed.`];
  if (event.roleId) lines.push(`The <@&${event.roleId}> role is being taken back from the members who were going.`);
  if (res.discordFailed) lines.push('⚠️ I could not delete the linked Discord event: remove it by hand.');
  return reply(successEmbed('Event Cancelled', lines.join('\n')));
}

// ── list / info / attendees ───────────────────────────────────────────────────

async function list(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const { guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  const { events, going } = await listOpenEvents(guildId);
  if (!events.length) {
    return safeReply(interaction, { embeds: [infoEmbed('No Open Events', 'There are no open events. Post one with `/event create`.')] });
  }
  const lines = events.flatMap((e) => [listLine(e, going.get(e.id) ?? 0), '']).slice(0, -1);
  const title = `📅 Open events (${events.length}/${EVENT.ACTIVE_MAX})`;
  const pages = chunkLines(lines, 3800).map((c) => guildEmbed(settings, title, c));
  return paginate(interaction, pages, { ephemeral: true });
}

async function info(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const { guildId } = interaction;
  const event = await resolveEventRef(guildId, eventRef(interaction));
  if (!event) return reply(notFound());
  const [tally, settings] = await Promise.all([getTally(event.id), getSettingsSafe(guildId)]);
  const url = jumpUrl(event);
  const length = event.endsAt ? formatDuration(new Date(event.endsAt).getTime() - new Date(event.startsAt).getTime()) : null;
  const lines = [
    `${STATUS_EMOJI[event.status] ?? ''} **${STATUS_TEXT[event.status] ?? event.status}**`,
    `**When:** ${discordTimestamp(event.startsAt, 'F')} (${discordTimestamp(event.startsAt, 'R')})${length ? `, for ${length}` : ''}`,
    ...(event.location ? [`**Where:** ${clampText(event.location, 100)}`] : []),
    `**Host:** <@${event.hostId}>`,
    `**Post:** <#${event.channelId}>${url ? ` · [jump](${url})` : ' · the post was deleted'}`,
    ...(event.roleId ? [`**Role:** <@&${event.roleId}>`] : []),
    `**Going:** ${tally.going}${event.capacity ? `/${event.capacity}` : ''} · **Maybe:** ${tally.maybe} · **Can't go:** ${tally.declined}`,
    `**Reminder:** ${event.reminderMinutes > 0 ? `${event.reminderMinutes} min before${event.reminderSentAt ? ' (sent)' : ''}` : 'none'}`,
    ...(event.discordEventId ? [`**Discord event:** https://discord.com/events/${guildId}/${event.discordEventId}`] : []),
    `**ID:** \`${event.id}\``,
  ];
  return reply(guildEmbed(settings, `📅 ${clampText(event.title, 200)}`, [clampText(event.description ?? '', 1000), ...lines].filter(Boolean).join('\n')));
}

async function attendees(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const { guildId } = interaction;
  const event = await resolveEventRef(guildId, eventRef(interaction));
  if (!event) return safeReply(interaction, { embeds: [notFound()] });
  const [tally, settings, rsvps] = await Promise.all([
    getTally(event.id),
    getSettingsSafe(guildId),
    prisma.eventRsvp.findMany({
      where: { eventId: event.id }, orderBy: { updatedAt: 'asc' }, take: EVENT.ATTENDEES_MAX, select: { userId: true, status: true },
    }),
  ]);
  if (!rsvps.length) return safeReply(interaction, { embeds: [infoEmbed('No RSVPs Yet', 'No one has answered yet.')] });
  const head = `✅ ${tally.going} going · ❔ ${tally.maybe} maybe · ❌ ${tally.declined} can't go`;
  const total = tally.going + tally.maybe + tally.declined;
  const pages = chunkLines(attendeeLines(rsvps), 3700).map((c, i) => guildEmbed(
    settings, `👥 ${clampText(event.title, 200)}`,
    `**${head}**\n\n${c}${i === 0 && total > rsvps.length ? `\n… and ${total - rsvps.length} more` : ''}`,
  ));
  return paginate(interaction, pages, { ephemeral: true });
}

// ── execute / autocomplete ────────────────────────────────────────────────────

export async function execute(interaction, client) {
  switch (interaction.options.getSubcommand()) {
    case 'create': return create(interaction, client);
    case 'edit': return edit(interaction);
    case 'cancel': return cancel(interaction, client);
    case 'list': return list(interaction);
    case 'info': return info(interaction);
    case 'attendees': return attendees(interaction);
    default: return undefined;
  }
}

/** Read-only. Answers within the 3 s window or with no choices. */
export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    const sub = interaction.options.getSubcommand(false);
    if (focused.name !== 'event' || !sub) return await respondAutocomplete(interaction, []);
    const rows = await withTimeout(
      findEventsForAutocomplete({ guildId: interaction.guildId, sub, q: focused.value }),
      ackBudgetMs(interaction),
      'event autocomplete',
    );
    const choices = rows.map((e) => ({
      value: e.id,
      name: `${STATUS_EMOJI[e.status] ?? ''} ${clampText(e.title, 70)} · ${new Date(e.startsAt).toISOString().slice(0, 10)}`.trim(),
    }));
    return await respondAutocomplete(interaction, choices);
  } catch (err) {
    logger.warn('event autocomplete failed', err);
    return respondAutocomplete(interaction, []);
  }
}
