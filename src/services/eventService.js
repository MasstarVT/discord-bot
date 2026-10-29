// Events (module `events`): /event create posts an announcement with Going / Maybe / Can't go buttons, optionally mirrors it
// as a Discord Scheduled Event (one-way: ours -> Discord, the bot has no scheduled-event intent), gives a role to the members
// who are going, pings them shortly before the start (`event.remind`) and closes the event at its end (`event.end`), which
// also takes the role back. Everything durable is in Postgres (GuildEvent, EventRsvp); the only in-process state is the render
// throttle and the per-member RSVP mutex, both allowed to vanish on a restart. Side-effect free at import (Maps only).
//
// Status: SCHEDULED and ACTIVE both count as open. Nothing writes ACTIVE (there is no start job): an open event whose start
// has passed is shown as "happening now".
import { Prisma } from '@prisma/client';
import {
  ActionRowBuilder, AttachmentBuilder, ButtonBuilder, ButtonStyle, GuildScheduledEventEntityType,
  GuildScheduledEventPrivacyLevel, GuildScheduledEventStatus, ModalBuilder, TextInputBuilder, TextInputStyle, escapeMarkdown,
} from 'discord.js';
import prisma from '../database/client.js';
import { COLORS, LIMITS, PERMISSION_LEVELS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { cancelJobs, PermanentJobError, scheduleJob } from './scheduler.js';
import { ensureGuildSettings, getSettingsSafe, isModuleEnabled, peekSettings } from './settingsService.js';
import { getUserTimezone, resolveMemberTimezone } from './userTimezoneService.js';
import { ackBudgetMs, createKeyedMutex, mapLimit, withTimeout } from '../utils/async.js';
import { buildIcs, googleCalendarUrl, outlookCalendarUrl } from '../utils/calendar.js';
import { applyGuildColor, errorEmbed, guildEmbed, successEmbed, warnEmbed } from '../utils/embedBuilder.js';
import {
  checkAssignableRole, checkChannelUsable, missingBotPermissions, replyError, safeDefer, safeReply,
} from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { formatDuration, parseDuration } from '../utils/parseDuration.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { clampText, sanitizeName } from '../utils/text.js';
import { discordTimestamp, parseWhen, zonedParts } from '../utils/time.js';
import { zoneLabel, zoneSourceText } from '../utils/timezones.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const EVENT = Object.freeze({
  ACTIVE_MAX: LIMITS.EVENTS_ACTIVE,
  TITLE_MAX: 100, DESCRIPTION_MAX: 1000, LOCATION_MAX: 100, // Discord's own limits for a scheduled event
  CAPACITY_MAX: 10_000, REMINDER_MAX: 1440,
  DURATION_MIN_MS: 5 * 60_000, DURATION_MAX_MS: 14 * 86_400_000, HORIZON_MS: 365 * 86_400_000,
  DEFAULT_END_MS: 2 * 3_600_000,     // when to end an event with no duration
  DISCORD_END_MS: 3_600_000,         // the Discord event's end when there is no duration (external events need one)
  OVERDUE_GRACE_MS: 10 * 60_000,
  GOING_SHOWN: 15, LIST_SHOWN: 25, ATTENDEES_MAX: 1000,
  RENDER_INTERVAL_MS: 5_000, NAME_LOOKUP_MS: 4_000,
  REMIND_USERS_MAX: 500, REMIND_CHUNK_USERS: 80, REMIND_CHUNK_CHARS: 1900,
  STRIP_BATCH: 100, STRIP_CONCURRENCY: 3, STRIP_RUNS_MAX: 50, STRIP_RESUME_MS: 15_000,
});

export const REMIND_JOB_TYPE = 'event.remind';
export const END_JOB_TYPE = 'event.end';
export const remindJobKey = (eventId) => `event.remind:${eventId}`;
export const endJobKey = (eventId) => `event.end:${eventId}`;

export const OPEN_STATUSES = Object.freeze(['SCHEDULED', 'ACTIVE']);
export const STATUS_BY_ACTION = Object.freeze({ going: 'GOING', maybe: 'MAYBE', declined: 'DECLINED' });
export const RSVP_LABELS = Object.freeze({ GOING: 'Going', MAYBE: 'Maybe', DECLINED: "Can't go" });
export const RSVP_EMOJI = Object.freeze({ GOING: '✅', MAYBE: '❔', DECLINED: '❌' });

const NO_MENTIONS = Object.freeze({ parse: [] });
const TX_OPTS = Object.freeze({ timeout: 10_000, maxWait: 10_000 });
const WARN_EVERY_MS = 10 * 60_000;
const RENDER_SWEEP_AT = 500;
const RENDER_IDLE_MS = 10 * 60_000;
const SEQUENCE_EPOCH = Date.UTC(2020, 0, 1);
const CALENDAR_HINT = 'Add it to your calendar with the buttons below, or open the attached file in Apple Calendar or any other calendar app.';

// Discord API error codes we react to.
const UNKNOWN_CHANNEL = 10003;
const UNKNOWN_MESSAGE = 10008;
const UNKNOWN_MEMBER = 10007;
const UNKNOWN_SCHEDULED_EVENT = 10070;
const NO_ACCESS_CODES = new Set([50001, 50013, 50083]); // missing access / permissions / archived thread

// ── Raw SQL (no timestamps involved, so no SQL_NOW) ────────────────────────────

// One creator at a time per guild: the open-event cap is exact even when two processes briefly overlap.
const createLockSql = (guildId) => Prisma.sql`SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext(${`event-create:${guildId}`}))`;
// The event row locked FOR UPDATE: RSVPs, edits, cancel and end serialise on it, so the capacity check and a concurrent
// cancel can never interleave.
const eventLockSql = (eventId) => Prisma.sql`SELECT id, "guildId", status::text AS status, capacity FROM "GuildEvent" WHERE id = ${eventId} FOR UPDATE`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'events.createLock', build: () => createLockSql('0') },
  { name: 'events.rowLock', build: () => eventLockSql('0') },
]);

// ── Pure helpers ──────────────────────────────────────────────────────────────

export const isOpen = (event) => OPEN_STATUSES.includes(event?.status);

/** @returns {Date} when the event ends: its end, else start + 2 h (the end job's time). */
export function eventEnd(event) {
  return event.endsAt ? new Date(event.endsAt) : new Date(new Date(event.startsAt).getTime() + EVENT.DEFAULT_END_MS);
}

/** @returns {Date|null} when the reminder is due, or null for no reminder */
export function remindAt(event) {
  const m = Number(event.reminderMinutes);
  if (!Number.isFinite(m) || m <= 0) return null;
  return new Date(new Date(event.startsAt).getTime() - m * 60_000);
}

/**
 * @param {string|null|undefined} raw the `duration` option or form field
 * @returns {{ ms: number|null } | { error: 'invalid'|'too-short'|'too-long' }} blank means no duration
 */
export function parseDurationInput(raw) {
  if (raw == null || String(raw).trim() === '') return { ms: null };
  const ms = parseDuration(String(raw).trim());
  if (ms == null) return { error: 'invalid' };
  if (ms < EVENT.DURATION_MIN_MS) return { error: 'too-short' };
  if (ms > EVENT.DURATION_MAX_MS) return { error: 'too-long' };
  return { ms };
}

export const DURATION_ERRORS = Object.freeze({
  invalid: 'Use a duration like `90m`, `2h` or `1d`.',
  'too-short': 'An event must run for at least 5 minutes.',
  'too-long': 'An event can run for at most 14 days.',
});

/** @returns {'past'|'far'|null} why a start time is refused */
export function startProblem(date, now = Date.now()) {
  const ms = date instanceof Date ? date.getTime() : Number(date);
  if (!Number.isFinite(ms) || ms <= now) return 'past';
  if (ms > now + EVENT.HORIZON_MS) return 'far';
  return null;
}

/** 'YYYY-MM-DD HH:MM Region/City': what the edit form shows, and parseWhen reads back exactly. */
export function formatStartForEdit(date, zone) {
  const p = zonedParts(date, zone);
  const two = (n) => String(n).padStart(2, '0');
  return `${p.year}-${two(p.month)}-${two(p.day)} ${two(p.hour)}:${two(p.minute)} ${zone}`;
}

/** @returns {string} 'https://discord.com/channels/...' for the announcement, or '' without one */
export function jumpUrl(event) {
  return event.messageId ? `https://discord.com/channels/${event.guildId}/${event.channelId}/${event.messageId}` : '';
}

/** @returns {{ going: number, maybe: number, declined: number, goingIds: string[] }} */
export function tallyFromGroups(groups, goingIds = []) {
  const t = { going: 0, maybe: 0, declined: 0, goingIds };
  for (const g of groups ?? []) {
    const n = Number(g?._count?._all ?? g?._count ?? 0);
    if (g?.status === 'GOING') t.going = n;
    else if (g?.status === 'MAYBE') t.maybe = n;
    else if (g?.status === 'DECLINED') t.declined = n;
  }
  return t;
}

export const emptyTally = () => ({ going: 0, maybe: 0, declined: 0, goingIds: [] });

/**
 * The card shown in the channel: details, who is going, and (while open) the RSVP buttons.
 * `names`: id -> name of the members going (see memberNames), printed under their mentions.
 */
export function buildEventEmbed(event, tally = emptyTally(), { settings = null, now = Date.now(), names = null } = {}) {
  const start = new Date(event.startsAt);
  const title = clampText(event.title, 200);
  const embed = guildEmbed(settings, `📅 ${title}`, clampText(event.description ?? '', EVENT.DESCRIPTION_MAX) || undefined);
  const started = start.getTime() <= now;

  if (event.status === 'CANCELLED') embed.setColor(COLORS.ERROR).setTitle(`🚫 Cancelled: ${title}`);
  else if (event.status === 'ENDED') embed.setColor(COLORS.NEUTRAL).setTitle(`🏁 Ended: ${title}`);
  else applyGuildColor(embed, settings);

  const when = [discordTimestamp(start, 'F'), started && isOpen(event) ? '**Happening now**' : discordTimestamp(start, 'R')];
  if (event.endsAt) when.push(`Until ${discordTimestamp(event.endsAt, 'f')}`);
  const fields = [{ name: 'When', value: when.join('\n') }];
  if (event.location) fields.push({ name: 'Where', value: clampText(event.location, EVENT.LOCATION_MAX), inline: true });
  fields.push({ name: 'Host', value: `<@${event.hostId}>`, inline: true });
  if (event.roleId) fields.push({ name: 'Role', value: `<@&${event.roleId}>`, inline: true });

  const cap = event.capacity ? ` / ${event.capacity}` : '';
  const shown = tally.goingIds.slice(0, EVENT.GOING_SHOWN);
  const hidden = Math.max(0, tally.going - shown.length);
  // An embed carries no user data: an app that has not loaded a member shows their mention as a raw `<@id>`, so the names
  // follow in a subtext line.
  const shownNames = shown.map((id) => names?.get(id)).filter(Boolean).map((n) => escapeMarkdown(clampText(n, 32)));
  const goingText = shown.length
    ? clampText(`${shown.map((id) => `<@${id}>`).join(' ')}${hidden ? ` +${hidden} more` : ''}`
      + `${shownNames.length ? `\n-# ${shownNames.join(' · ')}` : ''}`, 1024)
    : 'No one yet';
  fields.push({ name: `✅ Going (${tally.going}${cap})`, value: goingText });
  fields.push({ name: '❔ Maybe', value: String(tally.maybe), inline: true });
  embed.addFields(fields);

  embed.setFooter({
    text: event.status === 'CANCELLED' ? 'This event was cancelled.'
      : event.status === 'ENDED' ? 'This event has ended.'
        : 'RSVP with the buttons below.',
  });
  return embed;
}

/** The three RSVP buttons and Add to calendar: `event:{eventId}:going|maybe|declined|calendar`. */
export function buildEventComponents(event, tally = emptyTally(), { disabled = !isOpen(event) } = {}) {
  const full = Boolean(event.capacity) && tally.going >= event.capacity;
  return [new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('event', event.id, 'going'))
      .setLabel(full ? 'Going (full)' : 'Going').setEmoji('✅').setStyle(ButtonStyle.Success).setDisabled(disabled || full),
    new ButtonBuilder().setCustomId(buildCustomId('event', event.id, 'maybe'))
      .setLabel('Maybe').setEmoji('❔').setStyle(ButtonStyle.Secondary).setDisabled(disabled),
    new ButtonBuilder().setCustomId(buildCustomId('event', event.id, 'declined'))
      .setLabel("Can't go").setEmoji('❌').setStyle(ButtonStyle.Danger).setDisabled(disabled),
    new ButtonBuilder().setCustomId(buildCustomId('event', event.id, 'calendar'))
      .setLabel('Add to calendar').setEmoji('📅').setStyle(ButtonStyle.Secondary).setDisabled(disabled),
  )];
}

export function buildEventPayload(event, tally, { settings = null, now = Date.now(), names = null } = {}) {
  return {
    embeds: [buildEventEmbed(event, tally, { settings, now, names })],
    components: buildEventComponents(event, tally),
    allowedMentions: NO_MENTIONS,
  };
}

/**
 * Reminder messages for the members going: users only, each message under 2000 characters and under the 100 ids
 * allowedMentions.users accepts. The first one carries the heading.
 * @returns {Array<{ content: string, userIds: string[] }>}
 */
export function reminderMessages(event, userIds, now = Date.now()) {
  const start = new Date(event.startsAt);
  const when = start.getTime() > now ? `starts ${discordTimestamp(start, 'R')}` : 'is starting';
  const link = jumpUrl(event);
  const head = `⏰ **${clampText(event.title, 100)}** ${when}.${link ? ` ${link}` : ''}`;
  const out = [];
  let cur = null;
  for (const id of userIds) {
    const mention = `<@${id}>`;
    if (cur && (cur.userIds.length >= EVENT.REMIND_CHUNK_USERS || cur.content.length + 1 + mention.length > EVENT.REMIND_CHUNK_CHARS)) {
      out.push(cur);
      cur = null;
    }
    if (!cur) cur = { content: out.length === 0 ? head : '', userIds: [] };
    cur.content += `${cur.content ? (cur.userIds.length ? ' ' : '\n') : ''}${mention}`;
    cur.userIds.push(id);
  }
  if (cur) out.push(cur);
  return out;
}

/** One line of /event list. */
export function listLine(event, going = 0) {
  const url = jumpUrl(event);
  const cap = event.capacity ? `/${event.capacity}` : '';
  return `**${clampText(event.title, 60)}** · ${discordTimestamp(event.startsAt, 'f')} (${discordTimestamp(event.startsAt, 'R')})\n`
    + `✅ ${going}${cap} · <#${event.channelId}>${url ? ` · [post](${url})` : ''} · \`${event.id}\``;
}

/** Lines of /event attendees: one member per line, grouped Going, Maybe, Can't go. */
export function attendeeLines(rsvps) {
  const lines = [];
  for (const status of ['GOING', 'MAYBE', 'DECLINED']) {
    for (const r of rsvps) if (r.status === status) lines.push(`${RSVP_EMOJI[status]} <@${r.userId}>`);
  }
  return lines;
}

/** Wording for a refused or no-op RSVP, shared by the button and the tests. */
export function rsvpReplyText(out, event, status) {
  const title = clampText(event?.title, 100);
  if (out.result === 'gone') return 'This event no longer exists.';
  if (out.result === 'closed') return 'This event is no longer open.';
  if (out.result === 'full') return `**${title}** is full (${event.capacity} going). Your RSVP was not changed.`;
  const base = out.result === 'same'
    ? `You are already marked as **${RSVP_LABELS[status]}** for **${title}**.`
    : `${RSVP_EMOJI[status]} You are marked as **${RSVP_LABELS[status]}** for **${title}**.`;
  return out.note ? `${base}\n⚠️ ${out.note}` : base;
}

// ── Add to calendar ───────────────────────────────────────────────────────────

/** When a calendar shows the event ending: its end, else start + 1 h (the Discord event's end too). */
export function shownEnd(event) {
  return event.endsAt ? new Date(event.endsAt) : new Date(new Date(event.startsAt).getTime() + EVENT.DISCORD_END_MS);
}

/**
 * Discord markup as plain text for a calendar: mentions by name from the guild's caches (no fetches, this runs inside the
 * ack budget), custom emoji as `:name:`, command mentions as `/name`, timestamps as UTC times.
 */
export function plainDiscordText(text, guild = null) {
  const userName = (id) => {
    const member = guild?.members?.cache?.get(id);
    return member?.displayName ?? member?.user?.username ?? guild?.client?.users?.cache?.get(id)?.username ?? 'someone';
  };
  return String(text ?? '')
    .replace(/<a?:(\w{1,32}):\d{17,20}>/g, ':$1:')
    .replace(/<@!?(\d{17,20})>/g, (_, id) => `@${userName(id)}`)
    .replace(/<@&(\d{17,20})>/g, (_, id) => `@${guild?.roles?.cache?.get(id)?.name ?? 'role'}`)
    .replace(/<#(\d{17,20})>/g, (_, id) => `#${guild?.channels?.cache?.get(id)?.name ?? 'channel'}`)
    .replace(/<\/([^:>\n]{1,96}):\d{17,20}>/g, '/$1')
    .replace(/<t:(-?\d{1,13})(?::[tTdDfFR])?>/g, (whole, s) => {
      const d = new Date(Number(s) * 1000);
      return Number.isNaN(d.getTime()) ? whole : `${d.toISOString().slice(0, 16).replace('T', ' ')} UTC`;
    });
}

/** The event as a calendar entry (utils/calendar.js). */
export function calendarEntry(event, guild = null) {
  const updated = event.updatedAt ? new Date(event.updatedAt).getTime() : NaN;
  return {
    uid: `${event.id}@events.discord-bot`,
    title: clampText(event.title, EVENT.TITLE_MAX),
    description: clampText(plainDiscordText(event.description, guild), EVENT.DESCRIPTION_MAX),
    location: event.location ? clampText(event.location, EVENT.LOCATION_MAX) : '',
    start: new Date(event.startsAt),
    end: shownEnd(event),
    url: jumpUrl(event),
    // Seconds since 2020: grows with every change to the row and stays well inside a 32-bit SEQUENCE.
    sequence: Number.isFinite(updated) ? Math.max(0, Math.floor((updated - SEQUENCE_EPOCH) / 1000)) : 0,
  };
}

/**
 * The private reply of Add to calendar, also sent with a first Going or Maybe: Google Calendar and Outlook.com links, and
 * an .ics file for every other calendar. A snapshot: a later edit or cancel does not reach the calendars it went into.
 */
export function buildCalendarPayload(event, { guild = null, now = Date.now(), content = null } = {}) {
  const entry = calendarEntry(event, guild);
  const name = `${sanitizeName(event.title, { max: 50, fallback: 'event' })}.ics`;
  return {
    content: content ?? `📅 **${clampText(event.title, 100)}**\n${CALENDAR_HINT}`,
    components: [new ActionRowBuilder().addComponents(
      new ButtonBuilder().setStyle(ButtonStyle.Link).setLabel('Google Calendar').setURL(googleCalendarUrl(entry)),
      new ButtonBuilder().setStyle(ButtonStyle.Link).setLabel('Outlook').setURL(outlookCalendarUrl(entry)),
    )],
    files: [new AttachmentBuilder(Buffer.from(buildIcs(entry, { now }), 'utf8'), { name })],
    allowedMentions: NO_MENTIONS,
  };
}

// ── Time zone ─────────────────────────────────────────────────────────────────

/**
 * The caller's zone: their own (/timezone set), else their timezone role, else the server's. Lookups are bounded; a slow
 * or failed one falls back to the next source (`personalUnknown` says so). Never throws.
 * @param {import('discord.js').BaseInteraction} interaction
 * @param {{ budgetMs?: number }} [opts]
 */
export async function loadZone(interaction, { budgetMs = 3000 } = {}) {
  const { guildId } = interaction;
  const [settings, personal] = await Promise.all([
    guildId ? (peekSettings(guildId) ?? withTimeout(getSettingsSafe(guildId), budgetMs, 'event settings').catch(() => null)) : null,
    withTimeout(getUserTimezone(interaction.user.id), budgetMs, 'event user timezone').catch(() => undefined),
  ]);
  const resolved = resolveMemberTimezone({ member: interaction.member, guild: interaction.guild, settings, personal: personal ?? null });
  return { resolved, settings, personalUnknown: personal === undefined };
}

// ── Test seams (no effect at import) ──────────────────────────────────────────

let jobs = { schedule: scheduleJob, cancel: cancelJobs };
/** Test hook: replaces the scheduler calls. Pass nothing to restore. */
export function __setJobImpl(impl) {
  jobs = impl ? { schedule: impl.schedule ?? scheduleJob, cancel: impl.cancel ?? cancelJobs } : { schedule: scheduleJob, cancel: cancelJobs };
}

const renderMutex = createKeyedMutex(); // shared by live renders and the final edits
const userMutex = createKeyedMutex();   // one RSVP (database + role) at a time per member and event
/** @type {Map<string, { lastAt: number, timer: any }>} */
const renderState = new Map();
const warnedAt = new Map();
let renderImpl = renderEventMessage;

/** Test hook: replaces the Discord render. Pass nothing to restore. */
export function __setRenderImpl(fn) {
  renderImpl = typeof fn === 'function' ? fn : renderEventMessage;
}

function warnThrottled(key, message, err) {
  const now = Date.now();
  if (now - (warnedAt.get(key) ?? 0) < WARN_EVERY_MS) return;
  if (warnedAt.size > 200) for (const [k, t] of warnedAt) if (now - t >= WARN_EVERY_MS) warnedAt.delete(k);
  warnedAt.set(key, now);
  logger.warn(message, err);
}

const guildFor = (client, guildId) => {
  const guild = client.guilds?.cache?.get(guildId);
  return guild && guild.available !== false ? guild : null;
};

const reasonOf = (err) => clampText(err?.message ?? 'unknown error', 120);

// ── Database ──────────────────────────────────────────────────────────────────

export async function getTally(eventId) {
  const [groups, going] = await Promise.all([
    prisma.eventRsvp.groupBy({ by: ['status'], where: { eventId }, _count: { _all: true } }),
    prisma.eventRsvp.findMany({
      where: { eventId, status: 'GOING' }, orderBy: { updatedAt: 'asc' }, take: EVENT.GOING_SHOWN, select: { userId: true },
    }),
  ]);
  return tallyFromGroups(groups, going.map((r) => r.userId));
}

/** An event of the guild by id, or by a pasted link to its announcement. */
export async function resolveEventRef(guildId, raw) {
  const text = String(raw ?? '').trim();
  if (!text) return null;
  const link = /\/channels\/(\d{17,20})\/(\d{17,20})\/(\d{17,20})\/?$/.exec(text);
  if (link) return link[1] === guildId ? prisma.guildEvent.findFirst({ where: { guildId, messageId: link[3] } }) : null;
  return prisma.guildEvent.findFirst({ where: { id: text.slice(0, 40), guildId } });
}

/** Open events of the guild (soonest first) with the number going. */
export async function listOpenEvents(guildId) {
  const events = await prisma.guildEvent.findMany({
    where: { guildId, status: { in: [...OPEN_STATUSES] } }, orderBy: { startsAt: 'asc' }, take: EVENT.ACTIVE_MAX,
  });
  const groups = events.length
    ? await prisma.eventRsvp.groupBy({ by: ['eventId'], where: { eventId: { in: events.map((e) => e.id) }, status: 'GOING' }, _count: { _all: true } })
    : [];
  const going = new Map(groups.map((g) => [g.eventId, Number(g._count?._all ?? 0)]));
  return { events, going };
}

/** Autocomplete rows: edit/cancel offer open events, info/attendees also the latest closed ones. */
export function findEventsForAutocomplete({ guildId, sub, q }) {
  const open = sub === 'edit' || sub === 'cancel';
  const query = String(q ?? '').trim().slice(0, 100);
  return prisma.guildEvent.findMany({
    where: {
      guildId,
      ...(open && { status: { in: [...OPEN_STATUSES] } }),
      ...(query && { title: { contains: query, mode: 'insensitive' } }),
    },
    orderBy: open ? { startsAt: 'asc' } : { startsAt: 'desc' },
    take: 25,
    select: { id: true, title: true, startsAt: true, status: true },
  });
}

/**
 * Inserts the event under the guild's open-event cap.
 * @returns {Promise<{ ok: true, event: object } | { ok: false, code: 'LIMIT' }>}
 */
export async function createEventRow(guildId, data) {
  await ensureGuildSettings(guildId);
  return prisma.$transaction(async (tx) => {
    await tx.$queryRaw(createLockSql(guildId));
    const open = await tx.guildEvent.count({ where: { guildId, status: { in: [...OPEN_STATUSES] } } });
    if (open >= EVENT.ACTIVE_MAX) return { ok: false, code: 'LIMIT' };
    const event = await tx.guildEvent.create({ data: { ...data, guildId } });
    return { ok: true, event };
  }, TX_OPTS);
}

// ── Discord scheduled event (one-way) ─────────────────────────────────────────

/** The options Discord needs for an external event: a location and an end. */
export function discordEventOptions(event) {
  const start = new Date(event.startsAt);
  return {
    name: clampText(event.title, EVENT.TITLE_MAX),
    description: clampText(event.description ?? '', EVENT.DESCRIPTION_MAX) || undefined,
    scheduledStartTime: start,
    scheduledEndTime: shownEnd(event),
    privacyLevel: GuildScheduledEventPrivacyLevel.GuildOnly,
    entityType: GuildScheduledEventEntityType.External,
    entityMetadata: { location: clampText(event.location, EVENT.LOCATION_MAX) },
  };
}

/**
 * Creates the Discord event when the setting is on, the event has a location and the bot may.
 * @returns {Promise<{ id: string } | { note: string } | null>} null when the setting is off
 */
export async function createDiscordEvent(guild, event, settings) {
  if ((settings?.eventCreateDiscordEvent ?? true) !== true) return null;
  if (!event.location) return { note: 'No Discord event was created: it needs a location.' };
  if (missingBotPermissions(guild, ['ManageEvents']).length) {
    return { note: 'No Discord event was created: I need the **Manage Events** permission.' };
  }
  try {
    const created = await guild.scheduledEvents.create({ ...discordEventOptions(event), reason: 'Event created with /event' });
    return { id: created.id };
  } catch (err) {
    logger.warn(`events: could not create the Discord event in ${guild.id}`, err);
    return { note: `I could not create the Discord event (${reasonOf(err)}).` };
  }
}

/** Edits the linked Discord event. @returns {Promise<string|null>} a note when it failed */
async function updateDiscordEvent(guild, event, { startChanged, endChanged }) {
  if (!event.discordEventId) return null;
  try {
    const opts = discordEventOptions(event);
    await guild.scheduledEvents.edit(event.discordEventId, {
      name: opts.name,
      description: opts.description ?? null,
      entityMetadata: opts.entityMetadata,
      // A started event cannot get a new start (Discord rejects the whole edit): the start goes only when it changed.
      ...(startChanged && { scheduledStartTime: opts.scheduledStartTime }),
      ...((startChanged || endChanged) && { scheduledEndTime: opts.scheduledEndTime }),
      reason: 'Event edited with /event',
    });
    return null;
  } catch (err) {
    if (err?.code === UNKNOWN_SCHEDULED_EVENT) {
      await prisma.guildEvent.updateMany({ where: { id: event.id }, data: { discordEventId: null } }).catch(() => {});
      return 'The linked Discord event no longer exists.';
    }
    logger.warn(`events: could not update the Discord event of ${event.id}`, err);
    return `I could not update the Discord event (${reasonOf(err)}).`;
  }
}

/**
 * Whether the linked Discord event was deleted or cancelled in Discord. A failed lookup counts as still there.
 * @returns {Promise<boolean>}
 */
async function discordEventCancelled(client, event) {
  if (!event.discordEventId) return false;
  const guild = guildFor(client, event.guildId);
  if (!guild) return false;
  try {
    const found = await guild.scheduledEvents.fetch(event.discordEventId);
    return found?.status === GuildScheduledEventStatus.Canceled;
  } catch (err) {
    return err?.code === UNKNOWN_SCHEDULED_EVENT;
  }
}

/** Deletes the linked Discord event. @returns {Promise<boolean>} false when it failed (a missing one counts as gone) */
async function deleteDiscordEvent(guild, event) {
  if (!event.discordEventId) return true;
  try {
    await guild.scheduledEvents.delete(event.discordEventId);
  } catch (err) {
    if (err?.code !== UNKNOWN_SCHEDULED_EVENT) {
      logger.warn(`events: could not delete the Discord event of ${event.id}`, err);
      return false;
    }
  }
  await prisma.guildEvent.updateMany({ where: { id: event.id }, data: { discordEventId: null } }).catch(() => {});
  return true;
}

// ── Jobs ──────────────────────────────────────────────────────────────────────

/**
 * Schedules `event.end` and, when it is still ahead, `event.remind`; cancels the reminder otherwise.
 * @returns {Promise<{ failed: boolean }>} failed: a job could not be written
 */
export async function scheduleEventJobs(event, now = Date.now()) {
  let failed = false;
  try {
    await jobs.schedule({
      guildId: event.guildId, type: END_JOB_TYPE, runAt: eventEnd(event), payload: { eventId: event.id }, dedupeKey: endJobKey(event.id),
    });
  } catch (err) {
    failed = true;
    logger.error(`events: could not schedule event.end for ${event.id}`, err);
  }
  try {
    const at = remindAt(event);
    if (at && !event.reminderSentAt && at.getTime() > now) {
      await jobs.schedule({
        guildId: event.guildId, type: REMIND_JOB_TYPE, runAt: at, payload: { eventId: event.id }, dedupeKey: remindJobKey(event.id),
      });
    } else {
      await jobs.cancel({ dedupeKey: remindJobKey(event.id) });
    }
  } catch (err) {
    failed = true;
    logger.error(`events: could not schedule event.remind for ${event.id}`, err);
  }
  return { failed };
}

// ── Posting and rendering ─────────────────────────────────────────────────────

async function resolveChannel(client, channelId) {
  const cached = client.channels?.cache?.get(channelId);
  if (cached) return { channel: cached };
  try {
    const channel = await client.channels.fetch(channelId);
    return channel ? { channel } : { noAccess: true };
  } catch (err) {
    if (err?.code === UNKNOWN_CHANNEL) return { gone: true };
    if (NO_ACCESS_CODES.has(err?.code)) return { noAccess: true };
    throw err;
  }
}

/**
 * Display names for the card: the member's name in the server, else the account name (someone who left). Best effort: an id
 * Discord does not answer for in time is left out, and the card shows only its mention.
 * @returns {Promise<Map<string, string>>}
 */
export async function memberNames(client, guild, ids) {
  const names = new Map();
  const lookup = async () => {
    const wanted = [...new Set(ids)].filter(Boolean);
    const missing = wanted.filter((id) => !guild.members.cache.has(id));
    const found = missing.length ? await guild.members.fetch({ user: missing }).catch(() => new Map()) : new Map();
    const left = [];
    for (const id of wanted) {
      const member = guild.members.cache.get(id) ?? found.get(id);
      if (member) names.set(id, member.displayName);
      else left.push(id);
    }
    await mapLimit(left, 3, async (id) => {
      const user = client.users.cache.get(id) ?? await client.users.fetch(id);
      if (user) names.set(id, user.displayName ?? user.username);
    });
  };
  await withTimeout(lookup(), EVENT.NAME_LOOKUP_MS, 'event names').catch(() => {});
  return names;
}

/**
 * Edits the announcement to the event's current state.
 * @returns {Promise<'ok'|'skip'|'gone'>} 'gone' = the message was deleted (its id is cleared)
 */
async function editPost(client, event, { settings = undefined, tally = undefined } = {}) {
  if (!event.messageId || !guildFor(client, event.guildId)) return 'skip';
  try {
    const found = await resolveChannel(client, event.channelId);
    if (found.gone) return 'skip'; // the channelDelete hook cancels its events
    if (found.noAccess) {
      warnThrottled(`render:${event.id}`, `event ${event.id}: cannot reach its channel to update the post`);
      return 'skip';
    }
    const [t, s] = await Promise.all([tally ?? getTally(event.id), settings === undefined ? getSettingsSafe(event.guildId) : settings]);
    const names = await memberNames(client, guildFor(client, event.guildId), t.goingIds);
    await found.channel.messages.edit(event.messageId, buildEventPayload(event, t, { settings: s, names }));
    return 'ok';
  } catch (err) {
    if (err?.code === UNKNOWN_MESSAGE) {
      await prisma.guildEvent.updateMany({ where: { id: event.id, messageId: event.messageId }, data: { messageId: null } }).catch(() => {});
      return 'gone';
    }
    if (err?.code === UNKNOWN_CHANNEL) return 'skip';
    if (NO_ACCESS_CODES.has(err?.code)) {
      warnThrottled(`render:${event.id}`, `event ${event.id}: cannot edit its post (Discord ${err.code})`);
      return 'skip';
    }
    throw err;
  }
}

async function renderEventMessage(client, eventId) {
  const event = await prisma.guildEvent.findUnique({ where: { id: eventId } });
  if (!event || !event.messageId || !isOpen(event)) return 'skip'; // the final edit belongs to close/cancel
  return editPost(client, event);
}

/**
 * Asks for the announcement to be re-rendered: at most one edit per event per 5 s. The first request after a quiet period
 * renders at once; requests inside the window collapse into one trailing render that reads the database when it runs.
 */
export function requestRender(client, eventId) {
  const s = renderState.get(eventId) ?? { lastAt: 0, timer: null };
  renderState.set(eventId, s);
  if (s.timer) return;
  if (renderState.size > RENDER_SWEEP_AT) {
    const now = Date.now();
    for (const [id, st] of renderState) if (!st.timer && id !== eventId && now - st.lastAt > RENDER_IDLE_MS) renderState.delete(id);
  }
  const wait = Math.max(0, s.lastAt + EVENT.RENDER_INTERVAL_MS - Date.now());
  s.timer = setTimeout(async () => {
    s.lastAt = Date.now();
    s.timer = null;
    try {
      await renderMutex.run(eventId, () => renderImpl(client, eventId));
    } catch (err) {
      logger.error(`event render failed (${eventId})`, err);
    }
  }, wait);
  s.timer.unref?.();
}

/** Drops a queued render (the event closed). */
export function cancelRender(eventId) {
  const s = renderState.get(eventId);
  if (s?.timer) clearTimeout(s.timer);
  renderState.delete(eventId);
}

/** Renders now (an edit): under the same mutex as the live and final renders. */
export function renderNow(client, eventId) {
  cancelRender(eventId);
  return renderMutex.run(eventId, () => renderImpl(client, eventId));
}

// ── Creating ──────────────────────────────────────────────────────────────────

/**
 * Ends (reason 'expired') the guild's open events whose end passed more than 10 minutes ago, so a late end job never holds
 * the open-event cap.
 * @returns {Promise<number>} events ended
 */
export async function reconcileOverdue(client, guildId) {
  const rows = await prisma.guildEvent.findMany({
    where: { guildId, status: { in: [...OPEN_STATUSES] } }, select: { id: true, startsAt: true, endsAt: true },
  });
  let ended = 0;
  for (const r of rows) {
    if (eventEnd(r).getTime() + EVENT.OVERDUE_GRACE_MS > Date.now()) continue;
    const res = await closeEvent(client, r.id, { status: 'ENDED', reason: 'expired', guildId });
    if (res.ok) {
      ended += 1;
      await stripAfterClose(client, res.event, { reason: 'expired' });
    }
  }
  return ended;
}

/**
 * Creates the event, posts the announcement, stores its message id, makes the Discord event and schedules the jobs. A failed
 * post or save removes the row again; a failed Discord event or job is reported in `notes`, not fatal.
 * @param {{ client: import('discord.js').Client, guild: import('discord.js').Guild, channel: object, settings: object|null,
 *   data: { title: string, description?: string|null, location?: string|null, startsAt: Date, endsAt?: Date|null, hostId: string,
 *   roleId?: string|null, capacity?: number|null, reminderMinutes: number } }} input
 * @returns {Promise<{ ok: true, event: object, notes: string[] } | { ok: false, code: 'LIMIT'|'SEND'|'SAVE', error?: unknown }>}
 */
export async function createEvent({ client, guild, channel, settings, data }) {
  const row = { ...data, channelId: channel.id };
  let created = await createEventRow(guild.id, row);
  if (!created.ok) {
    try {
      if ((await reconcileOverdue(client, guild.id)) > 0) created = await createEventRow(guild.id, row);
    } catch (err) {
      logger.warn('events: reconciling overdue events failed', err);
    }
    if (!created.ok) return { ok: false, code: 'LIMIT' };
  }
  let event = created.event;

  let msg;
  try {
    msg = await channel.send(buildEventPayload(event, emptyTally(), { settings }));
  } catch (err) {
    logger.warn(`events: could not post the announcement in ${guild.id}`, err);
    await prisma.guildEvent.delete({ where: { id: event.id } }).catch(() => {});
    return { ok: false, code: 'SEND', error: err };
  }
  try {
    event = await prisma.guildEvent.update({ where: { id: event.id }, data: { messageId: msg.id } });
  } catch (err) {
    logger.error('events: could not store the announcement id', err);
    await msg.delete?.().catch(() => {});
    await prisma.guildEvent.delete({ where: { id: event.id } }).catch(() => {});
    return { ok: false, code: 'SAVE', error: err };
  }

  const notes = [];
  const discord = await createDiscordEvent(guild, event, settings);
  if (discord?.id) {
    try {
      event = await prisma.guildEvent.update({ where: { id: event.id }, data: { discordEventId: discord.id } });
    } catch (err) {
      logger.error('events: could not store the Discord event id', err);
      // Unlinked first: should the id have been stored after all, the delete would otherwise cancel this event.
      await prisma.guildEvent.updateMany({ where: { id: event.id }, data: { discordEventId: null } }).catch(() => {});
      await guild.scheduledEvents.delete(discord.id).catch(() => {});
      notes.push('The Discord event could not be linked, so it was removed.');
    }
  } else if (discord?.note) {
    notes.push(discord.note);
  }

  const { failed } = await scheduleEventJobs(event);
  if (failed) notes.push('I could not schedule the reminder or the end of the event: edit the event to try again.');
  return { ok: true, event, notes };
}

// ── RSVP ──────────────────────────────────────────────────────────────────────

/**
 * Records one RSVP. The event row is locked FOR UPDATE, so the capacity check and a concurrent cancel or end are serialised.
 * Going needs a free place unless the member already is going.
 * @returns {Promise<{ result: 'gone'|'closed'|'full'|'same'|'set', previous?: string|null }>}
 */
export async function applyRsvp({ eventId, guildId, userId, status }) {
  return prisma.$transaction(async (tx) => {
    const [row = null] = await tx.$queryRaw(eventLockSql(eventId));
    if (!row || row.guildId !== guildId) return { result: 'gone' };
    if (!OPEN_STATUSES.includes(row.status)) return { result: 'closed' };
    const existing = await tx.eventRsvp.findUnique({ where: { eventId_userId: { eventId, userId } } });
    const previous = existing?.status ?? null;
    if (previous === status) return { result: 'same', previous };
    if (status === 'GOING' && row.capacity != null) {
      const going = await tx.eventRsvp.count({ where: { eventId, status: 'GOING' } }); // the caller is not going yet
      if (going >= row.capacity) return { result: 'full', previous };
    }
    await tx.eventRsvp.upsert({
      where: { eventId_userId: { eventId, userId } },
      create: { eventId, userId, status },
      update: { status },
    });
    return { result: 'set', previous };
  }, TX_OPTS);
}

/** Sets `EventRsvp.roleGranted`: true only once the bot really added the role, false once it took it back (or Role Persistence skipped it for a member who left holding it). */
const markRoleGranted = (eventId, userId, roleGranted) => prisma.eventRsvp.updateMany({ where: { eventId, userId }, data: { roleGranted } });

/**
 * Gives the event role to a member who is now going, takes it back from one who stopped going. The role is only ever taken
 * back from a member the bot gave it to (`roleGranted`): one who already held it keeps it.
 * @returns {Promise<string|null>} a note for the member when the role could not be changed
 */
export async function syncRsvpRole({ guild, member, event, status }) {
  const roleId = event.roleId;
  if (!roleId || !member?.roles) return null;
  const has = member.roles.cache?.has(roleId) === true;
  try {
    if (status === 'GOING') {
      if (has) return null;
      const check = checkAssignableRole(guild, guild.roles.cache.get(roleId));
      if (!check.ok) return `I could not give you the event role: ${check.reason}`;
      await member.roles.add(roleId, `Event RSVP: going to ${clampText(event.title, 60)}`);
      // Flag first, then look at the status: an end or cancel that commits after the flag is written finds this row in its
      // strip, one that committed before is seen by the read below, so the member cannot keep the role either way.
      let open = null; // null: could not tell
      try {
        await markRoleGranted(event.id, member.id, true);
        const fresh = await prisma.guildEvent.findUnique({ where: { id: event.id }, select: { status: true } });
        open = OPEN_STATUSES.includes(fresh?.status);
      } catch (err) {
        logger.warn(`events: could not record the role of ${member.id} for ${event.id}`, err);
      }
      if (open === true) return null;
      await member.roles.remove(roleId, `Event RSVP: ${clampText(event.title, 60)} is over`);
      await markRoleGranted(event.id, member.id, false).catch(() => {});
      return open === false
        ? 'This event just ended, so the event role was not kept.'
        : 'I could not save your event role, so it was taken back. Please try again.';
    }
    const row = await prisma.eventRsvp.findUnique({ where: { eventId_userId: { eventId: event.id, userId: member.id } } });
    if (row?.roleGranted === true) {
      if (has) await member.roles.remove(roleId, `Event RSVP: no longer going to ${clampText(event.title, 60)}`);
      await markRoleGranted(event.id, member.id, false);
    }
    return null;
  } catch (err) {
    logger.warn(`events: could not change the role of ${member.id} for ${event.id}`, err);
    return `I could not change your event role (${reasonOf(err)}).`;
  }
}

/**
 * `event:{eventId}:calendar`: the calendar links and file, privately. Defers first; the event is read from the database.
 * @param {import('discord.js').ButtonInteraction} interaction
 * @param {string} eventId
 */
async function handleCalendarButton(interaction, eventId) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (content) => safeReply(interaction, { content, allowedMentions: NO_MENTIONS });
  try {
    const event = await prisma.guildEvent.findFirst({ where: { id: eventId, guildId: interaction.guildId } });
    if (!event || !isOpen(event)) {
      await reply(rsvpReplyText({ result: event ? 'closed' : 'gone' }, event));
      return;
    }
    await safeReply(interaction, buildCalendarPayload(event, { guild: interaction.guild }));
  } catch (err) {
    logger.error('event calendar failed', err);
    await reply('Something went wrong making the calendar entry. Please try again.');
  }
}

/**
 * `event:{eventId}:going|maybe|declined` (and `calendar`, see handleCalendarButton). Defers first; everything after may
 * touch the database. A first Going or Maybe also gets the calendar links and file.
 * @param {import('discord.js').ButtonInteraction} interaction
 * @param {import('discord.js').Client} client
 */
export async function handleRsvpButton(interaction, client) {
  const { params: [eventId, action] } = parseCustomId(interaction.customId);
  if (eventId && action === 'calendar') {
    await handleCalendarButton(interaction, eventId);
    return;
  }
  const status = Object.hasOwn(STATUS_BY_ACTION, action ?? '') ? STATUS_BY_ACTION[action] : null;
  if (!eventId || !status) {
    await safeReply(interaction, { content: 'This button is no longer active.', ephemeral: true });
    return;
  }
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (content) => safeReply(interaction, { content, allowedMentions: NO_MENTIONS });
  try {
    const { guild, guildId, user } = interaction;
    const event = await prisma.guildEvent.findFirst({ where: { id: eventId, guildId } });
    if (!event || !isOpen(event)) {
      await reply(rsvpReplyText({ result: event ? 'closed' : 'gone' }, event, status));
      return;
    }

    const out = await userMutex.run(`${eventId}:${user.id}`, async () => {
      const res = await applyRsvp({ eventId, guildId, userId: user.id, status });
      if (res.result !== 'set' && res.result !== 'same') return res;
      let member = interaction.member?.roles?.cache ? interaction.member : null;
      member ??= await guild?.members.fetch(user.id).catch(() => null);
      // `same` on Going also repairs a role that an earlier click failed to give.
      const note = res.result === 'set' || status === 'GOING'
        ? await syncRsvpRole({ guild, member, event, status })
        : null;
      return { ...res, note };
    });
    const text = rsvpReplyText(out, event, status);
    // Only on joining: moving between Going and Maybe would send the file again every time.
    const joined = out.result === 'set' && status !== 'DECLINED' && out.previous !== 'GOING' && out.previous !== 'MAYBE';
    let payload = null;
    if (joined) {
      try {
        payload = buildCalendarPayload(event, { guild, content: `${text}\n${CALENDAR_HINT}` });
      } catch (err) {
        logger.warn(`events: no calendar entry for ${eventId}`, err);
      }
    }
    // A refused calendar reply must not cost the member the confirmation of an RSVP that was saved.
    if (!payload || !(await safeReply(interaction, payload))) await reply(text);
    if (out.result === 'set') requestRender(client, eventId);
  } catch (err) {
    logger.error('event rsvp failed', err);
    await reply('Something went wrong saving your RSVP. Please try again.');
  }
}

// ── Closing: end, cancel ──────────────────────────────────────────────────────

/**
 * Flips an open event to ENDED or CANCELLED (the conditional update picks exactly one winner among the end job, /event cancel,
 * the channel hook and the overdue sweep), cancels its jobs, edits the announcement with disabled buttons and, for a
 * cancel, deletes the Discord event. Every Discord step after the status change is caught here.
 * @param {import('discord.js').Client} client
 * @param {string} eventId
 * @param {{ status: 'ENDED'|'CANCELLED', reason: 'timer'|'manual'|'expired'|'channel-deleted'|'discord-cancelled', guildId?: string }} opts
 * @returns {Promise<{ ok: true, event: object|null, discordFailed: boolean }
 *   | { ok: false, code: 'NOT_FOUND'|'CLOSED', event: object|null }>} throws only when the status UPDATE fails
 */
export async function closeEvent(client, eventId, { status, reason, guildId = undefined }) {
  const { count } = await prisma.guildEvent.updateMany({
    where: { id: eventId, status: { in: [...OPEN_STATUSES] }, ...(guildId && { guildId }) },
    data: { status },
  });
  if (count === 0) {
    const row = await prisma.guildEvent.findFirst({ where: { id: eventId, ...(guildId && { guildId }) } });
    return { ok: false, code: row ? 'CLOSED' : 'NOT_FOUND', event: row ?? null };
  }

  let event = null;
  let discordFailed = false;
  try {
    cancelRender(eventId);
    await jobs.cancel({ dedupeKey: remindJobKey(eventId) });
    if (reason !== 'timer') await jobs.cancel({ dedupeKey: endJobKey(eventId) });
    event = await prisma.guildEvent.findUnique({ where: { id: eventId } });
    if (!event) return { ok: true, event, discordFailed };
    const guild = guildFor(client, event.guildId);
    if (!guild) {
      // An unavailable guild is never judged: the post keeps its buttons (they answer "no longer open").
      logger.warn(`event ${eventId} closed, but its guild is unavailable: the post was not updated`);
      return { ok: true, event, discordFailed };
    }
    if (status === 'CANCELLED') discordFailed = !(await deleteDiscordEvent(guild, event));
    if (reason !== 'channel-deleted') {
      try {
        const tally = await getTally(eventId);
        const settings = await getSettingsSafe(event.guildId);
        await renderMutex.run(eventId, () => editPost(client, event, { tally, settings }));
      } catch (err) {
        logger.warn(`event ${eventId}: could not edit the final post`, err);
      }
    }
  } catch (err) {
    logger.error(`event ${eventId}: finishing the close failed`, err);
  }
  return { ok: true, event, discordFailed };
}

/**
 * Takes the event role back from up to STRIP_BATCH RSVP'd members after the cursor (one at a time, a few in parallel).
 * @returns {Promise<{ more: string|null }>} more = the cursor for the next batch, null when done
 */
export async function stripRoles(client, event, { after = null, run = 0 } = {}) {
  if (!event?.roleId) return { more: null };
  const guild = guildFor(client, event.guildId);
  if (!guild) return { more: null };
  if (missingBotPermissions(guild, ['ManageRoles']).length) {
    logger.warn(`event ${event.id}: cannot take the event role back, Manage Roles is missing`);
    return { more: null };
  }
  // Only members the bot gave the role to: one who already held it keeps it. A flag stays set when the removal failed, and
  // for a member who is not in the server: that is Role Persistence's marker that this member's saved copy of the role
  // came from this event (it skips the role on their return and clears the flag; src/services/rolePersistService.js).
  const rows = await prisma.eventRsvp.findMany({
    where: { eventId: event.id, roleGranted: true, ...(after && { userId: { gt: after } }) },
    orderBy: { userId: 'asc' },
    take: EVENT.STRIP_BATCH + 1,
    select: { userId: true },
  });
  const batch = rows.slice(0, EVENT.STRIP_BATCH);
  await mapLimit(batch, EVENT.STRIP_CONCURRENCY, async ({ userId }) => {
    try {
      try {
        const member = guild.members.cache.get(userId) ?? await guild.members.fetch(userId);
        if (member?.roles?.cache?.has(event.roleId)) await member.roles.remove(event.roleId, 'Event is over');
      } catch (err) {
        if (err?.code !== UNKNOWN_MEMBER) throw err;
        return; // a member who left holds nothing; the flag stays as the marker (a cursor run never revisits the row)
      }
      await markRoleGranted(event.id, userId, false);
    } catch (err) {
      warnThrottled(`strip:${event.id}`, `event ${event.id}: could not take the role from ${userId}`, err);
    }
  });
  const more = rows.length > EVENT.STRIP_BATCH && run + 1 < EVENT.STRIP_RUNS_MAX ? batch[batch.length - 1].userId : null;
  return { more };
}

/** Strips the first batch of roles after a close and, when members remain, schedules the job that continues. */
async function stripAfterClose(client, event, { reason }) {
  if (!event?.roleId) return;
  try {
    const { more } = await stripRoles(client, event);
    if (more && reason !== 'timer') {
      await jobs.schedule({
        guildId: event.guildId, type: END_JOB_TYPE, runAt: new Date(Date.now() + EVENT.STRIP_RESUME_MS),
        payload: { eventId: event.id, after: more, run: 1 }, dedupeKey: endJobKey(event.id),
      });
    }
  } catch (err) {
    logger.warn(`event ${event.id}: taking the role back failed`, err);
  }
}

/**
 * Cancels an open event: status, jobs, post, Discord event and the role.
 * @returns {Promise<{ ok: true, event: object|null, discordFailed: boolean } | { ok: false, code: string }>}
 */
export async function cancelEvent(client, eventId, { guildId = undefined, reason = 'manual' } = {}) {
  const res = await closeEvent(client, eventId, { status: 'CANCELLED', reason, guildId });
  if (res.ok) await stripAfterClose(client, res.event, { reason });
  return res;
}

// ── Editing ───────────────────────────────────────────────────────────────────

/**
 * Applies already validated changes to an open event, reschedules its jobs, updates the Discord event and the post.
 * @param {{ client: object, guild: import('discord.js').Guild, event: object, changes: object }} input changes: any of
 *   title, description, location, startsAt, endsAt, capacity, reminderMinutes (undefined = unchanged)
 * @returns {Promise<{ ok: true, event: object, notes: string[] } | { ok: false, code: 'CLOSED' }>}
 */
export async function editEvent({ client, guild, event, changes }) {
  const data = Object.fromEntries(Object.entries(changes).filter(([, v]) => v !== undefined));
  const startChanged = data.startsAt !== undefined && data.startsAt.getTime() !== new Date(event.startsAt).getTime();
  const endChanged = data.endsAt !== undefined && (data.endsAt?.getTime() ?? null) !== (event.endsAt ? new Date(event.endsAt).getTime() : null);
  const reminderChanged = data.reminderMinutes !== undefined && data.reminderMinutes !== event.reminderMinutes;
  if (startChanged || reminderChanged) data.reminderSentAt = null; // a moved event reminds again
  const { count } = await prisma.guildEvent.updateMany({
    where: { id: event.id, guildId: event.guildId, status: { in: [...OPEN_STATUSES] } }, data,
  });
  if (count === 0) return { ok: false, code: 'CLOSED' };

  const updated = await prisma.guildEvent.findUnique({ where: { id: event.id } });
  const notes = [];
  if (!updated) return { ok: false, code: 'CLOSED' };
  if (startChanged || endChanged || reminderChanged) {
    const { failed } = await scheduleEventJobs(updated);
    if (failed) notes.push('I could not reschedule the reminder or the end of the event.');
  }
  const discordNote = await updateDiscordEvent(guild, updated, { startChanged, endChanged });
  if (discordNote) notes.push(discordNote);
  if (data.capacity != null) {
    const tally = await getTally(event.id);
    if (tally.going > data.capacity) notes.push(`${tally.going} members are already going: they keep their place.`);
  }
  try {
    await renderNow(client, event.id);
  } catch (err) {
    logger.warn(`event ${event.id}: could not update the post after an edit`, err);
    notes.push('I could not update the announcement.');
  }
  return { ok: true, event: updated, notes };
}

/** The 5-field form of /event edit: customId `event-modal:{eventId}:{capacity|-}:{reminder|-}`. */
export function buildEditModal(event, zone, { capacity = null, reminder = null } = {}) {
  const field = (id, label, style, { value, required, max, placeholder } = {}) => {
    const input = new TextInputBuilder().setCustomId(id).setLabel(label).setStyle(style).setRequired(Boolean(required));
    if (max) input.setMaxLength(max);
    if (value) input.setValue(clampText(value, max ?? 4000));
    if (placeholder) input.setPlaceholder(placeholder);
    return new ActionRowBuilder().addComponents(input);
  };
  const end = event.endsAt ? new Date(event.endsAt).getTime() - new Date(event.startsAt).getTime() : 0;
  return new ModalBuilder()
    .setCustomId(buildCustomId('event-modal', event.id, capacity ?? '-', reminder ?? '-'))
    .setTitle('Edit event')
    .addComponents(
      field('event-title', 'Title', TextInputStyle.Short, { value: event.title, required: true, max: EVENT.TITLE_MAX }),
      field('event-description', 'Description', TextInputStyle.Paragraph, { value: event.description ?? '', max: EVENT.DESCRIPTION_MAX }),
      field('event-start', 'Start', TextInputStyle.Short, {
        value: formatStartForEdit(event.startsAt, zone), required: true, max: 100, placeholder: 'fri 7pm, or 2026-10-30 18:00 Europe/London',
      }),
      field('event-duration', 'Duration (empty = none)', TextInputStyle.Short, {
        value: end > 0 ? formatDuration(end) : '', max: 40, placeholder: '2h',
      }),
      field('event-location', 'Location', TextInputStyle.Short, { value: event.location ?? '', max: EVENT.LOCATION_MAX }),
    );
}

// ── Reading the fields (create options and the edit form) ─────────────────────

const shown = (raw) => clampText(String(raw ?? '').replace(/[`\\]/g, '').trim(), 64) || '(empty)';

/**
 * Reads and checks the text fields of a new or edited event. `current` is the event being edited: its own start stays valid
 * when it already passed, and a Discord event keeps its location.
 * @param {{ title: string, description?: string|null, location?: string|null, start: string, duration?: string|null }} fields
 * @param {{ resolved: { zone: string, source: string, roleId: string|null }, now?: number, current?: object|null }} ctx
 * @returns {{ ok: true, values: { title: string, description: string|null, location: string|null, startsAt: Date,
 *   endsAt: Date|null, zone: string, zoneFromInput: boolean } } | { ok: false, title: string, text: string }}
 */
export function parseEventFields(fields, { resolved, now = Date.now(), current = null }) {
  const fail = (title, text) => ({ ok: false, title, text });
  const title = String(fields.title ?? '').trim();
  if (!title) return fail('Missing Title', 'Give the event a title.');
  if (title.length > EVENT.TITLE_MAX) return fail('Title Too Long', `The title can have at most ${EVENT.TITLE_MAX} characters.`);
  const description = String(fields.description ?? '').trim() || null;
  if (description && description.length > EVENT.DESCRIPTION_MAX) {
    return fail('Description Too Long', `The description can have at most ${EVENT.DESCRIPTION_MAX} characters.`);
  }
  const location = String(fields.location ?? '').trim() || null;
  if (location && location.length > EVENT.LOCATION_MAX) return fail('Location Too Long', `The location can have at most ${EVENT.LOCATION_MAX} characters.`);
  if (!location && current?.discordEventId) {
    return fail('Location Needed', 'This event is linked to a Discord event, which needs a location.');
  }

  const parsed = parseWhen(fields.start, resolved.zone, { now: new Date(now) });
  if (!parsed) {
    return fail('Could Not Read That Time', [
      `I couldn't read \`${shown(fields.start)}\` as a time.`,
      'Try `fri 7pm`, `tomorrow 18:30`, `oct 30 7:30pm`, `2026-10-30 19:00`, `in 3d` or `7pm CST`.',
      `Times are read in **${zoneLabel(resolved.zone)}**, ${zoneSourceText(resolved)}.`,
    ].join('\n'));
  }
  // Whole minutes only: the edit form shows the start to the minute, so a start with seconds ("in 3d") must not look changed
  // when the form comes back untouched. A start in the same minute as the stored one keeps the stored value.
  const typed = new Date(Math.floor(parsed.date.getTime() / 60_000) * 60_000);
  const unchanged = current && typed.getTime() === Math.floor(new Date(current.startsAt).getTime() / 60_000) * 60_000;
  const startsAt = unchanged ? new Date(current.startsAt) : typed;
  const problem = unchanged ? null : startProblem(startsAt, now);
  if (problem === 'past') {
    return fail('Start In The Past', `That is ${discordTimestamp(startsAt, 'f')}, which has already passed.`);
  }
  if (problem === 'far') return fail('Start Too Far Away', 'An event can start at most a year from now.');

  const d = parseDurationInput(fields.duration);
  if (d.error) return fail('Invalid Duration', DURATION_ERRORS[d.error]);
  const endsAt = d.ms ? new Date(startsAt.getTime() + d.ms) : null;
  return { ok: true, values: { title, description, location, startsAt, endsAt, zone: parsed.zone, zoneFromInput: parsed.zoneFromInput } };
}

/**
 * `event-modal:{eventId}:{capacity|-}:{reminder|-}`: the submitted /event edit form. The router checks levels for commands,
 * not for components, so the Moderator level is checked here.
 * @param {import('discord.js').ModalSubmitInteraction} interaction
 * @param {import('discord.js').Client} client
 */
export async function handleEditModal(interaction, client) {
  const { params: [eventId, capRaw, remRaw] } = parseCustomId(interaction.customId);
  if (!interaction.inCachedGuild?.() || !eventId) {
    return replyError(interaction, 'Form Expired', 'This form has expired. Please run `/event edit` again.');
  }
  const { guildId } = interaction;
  const settings = peekSettings(guildId)
    ?? await withTimeout(getSettingsSafe(guildId), ackBudgetMs(interaction), 'event modal settings').catch(() => null);
  if (resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.MODERATOR) {
    return replyError(interaction, 'Insufficient Permissions', 'You need the Moderator level to edit events.');
  }
  if (!(await safeDefer(interaction, { ephemeral: true }))) return undefined;
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  try {
    const event = await prisma.guildEvent.findFirst({ where: { id: eventId, guildId } });
    if (!event) return reply(errorEmbed('Event Not Found', 'This event no longer exists.'));
    if (!isOpen(event)) return reply(warnEmbed('Event Closed', 'This event has ended or was cancelled, so it cannot be edited.'));

    const { resolved } = await loadZone(interaction);
    const text = (id) => interaction.fields.getTextInputValue(id);
    const read = parseEventFields({
      title: text('event-title'), description: text('event-description'), start: text('event-start'),
      duration: text('event-duration'), location: text('event-location'),
    }, { resolved, current: event });
    if (!read.ok) return reply(errorEmbed(read.title, `${read.text}\nNothing was saved.`));

    const capacity = /^\d{1,5}$/.test(capRaw ?? '') ? Number(capRaw) : undefined; // 0 = no limit
    const reminder = /^\d{1,4}$/.test(remRaw ?? '') ? Number(remRaw) : undefined;
    const { values } = read;
    const out = await editEvent({
      client, guild: interaction.guild, event,
      changes: {
        title: values.title, description: values.description, location: values.location, startsAt: values.startsAt, endsAt: values.endsAt,
        ...(capacity !== undefined && { capacity: capacity === 0 ? null : capacity }),
        ...(reminder !== undefined && { reminderMinutes: reminder }),
      },
    });
    if (!out.ok) return reply(warnEmbed('Event Closed', 'This event ended or was cancelled before the form was saved.'));
    const lines = [`**${clampText(out.event.title, 100)}** starts ${discordTimestamp(out.event.startsAt, 'F')} (${discordTimestamp(out.event.startsAt, 'R')}).`];
    if (!values.zoneFromInput) lines.push(`The start was read in ${zoneLabel(resolved.zone)}, ${zoneSourceText(resolved)}.`);
    for (const n of out.notes) lines.push(`⚠️ ${n}`);
    return reply(successEmbed('Event Updated', lines.join('\n')));
  } catch (err) {
    logger.error('event edit failed', err);
    return reply(errorEmbed('Could Not Save', 'Something went wrong saving the event. Please try again.'));
  }
}

// ── Hooks and jobs ────────────────────────────────────────────────────────────

/** messageDelete: the announcement is gone; the event stays (and its reminders still run) without a post. */
export async function onMessageDelete(ctx) {
  const messageId = ctx.message?.id;
  if (!messageId || !ctx.guildId) return;
  await prisma.guildEvent.updateMany({ where: { guildId: ctx.guildId, messageId }, data: { messageId: null } });
}

/** channelDelete: every open event posted in the channel is cancelled. */
export async function onChannelDelete(ctx) {
  const channelId = ctx.channel?.id;
  if (!channelId || !ctx.guildId) return;
  const rows = await prisma.guildEvent.findMany({
    where: { guildId: ctx.guildId, channelId, status: { in: [...OPEN_STATUSES] } }, select: { id: true },
  });
  for (const r of rows) {
    try {
      await cancelEvent(ctx.client, r.id, { guildId: ctx.guildId, reason: 'channel-deleted' });
    } catch (err) {
      logger.error(`event ${r.id}: cancelling after a channel delete failed`, err);
    }
  }
}

/**
 * guildScheduledEventDelete, and guildScheduledEventUpdate to Canceled: the linked Discord event was cancelled in Discord, so
 * the event is cancelled too (post, jobs, roles). Only before the start; after it the end job closes the event. The bot's
 * own deletes come after its status change (or after unlinking), so they find nothing open.
 */
export async function onDiscordEventCancelled(ctx) {
  const discordEventId = ctx.scheduledEvent?.id;
  const guildId = ctx.guildId ?? ctx.guild?.id;
  if (!discordEventId || !guildId) return;
  const rows = await prisma.guildEvent.findMany({
    where: { guildId, discordEventId, status: { in: [...OPEN_STATUSES] }, startsAt: { gt: new Date() } }, select: { id: true },
  });
  for (const r of rows) {
    try {
      await cancelEvent(ctx.client, r.id, { guildId, reason: 'discord-cancelled' });
    } catch (err) {
      logger.error(`event ${r.id}: cancelling after its Discord event was cancelled failed`, err);
    }
  }
}

/** roleDelete: the event role is gone, so events stop giving it. */
export async function onRoleDelete(ctx) {
  const roleId = ctx.role?.id;
  const guildId = ctx.guildId ?? ctx.guild?.id;
  if (!roleId || !guildId) return;
  await prisma.guildEvent.updateMany({ where: { guildId, roleId }, data: { roleId: null } });
}

/** Scheduler handler for `event.remind`: one message pinging the members going (claimed first, so a retry never pings twice). */
export async function handleRemindJob(job, ctx) {
  const eventId = job.payload?.eventId;
  if (typeof eventId !== 'string' || !eventId) throw new PermanentJobError('event.remind: missing eventId');
  const event = await prisma.guildEvent.findFirst({ where: { id: eventId, guildId: job.guildId } });
  if (!event || !isOpen(event) || event.reminderSentAt) return;
  if (new Date(event.startsAt).getTime() <= Date.now()) return; // too late to be a reminder
  const settings = await getSettingsSafe(job.guildId);
  if (settings && !isModuleEnabled(settings, 'events')) return;
  // A Discord event cancelled while the bot was not listening (a restart) cancels the event instead of pinging.
  if (await discordEventCancelled(ctx.client, event)) {
    await cancelEvent(ctx.client, eventId, { guildId: job.guildId, reason: 'discord-cancelled' });
    return;
  }

  const { count } = await prisma.guildEvent.updateMany({
    where: { id: eventId, status: { in: [...OPEN_STATUSES] }, reminderSentAt: null }, data: { reminderSentAt: new Date() },
  });
  if (count === 0) return;
  const going = await prisma.eventRsvp.findMany({
    where: { eventId, status: 'GOING' }, orderBy: { updatedAt: 'asc' }, take: EVENT.REMIND_USERS_MAX, select: { userId: true },
  });
  if (!going.length) return;

  const found = await resolveChannel(ctx.client, event.channelId).catch(() => ({ noAccess: true }));
  if (!found.channel || !checkChannelUsable(found.channel, { needs: ['ViewChannel', 'SendMessages'] }).ok) {
    logger.warn(`event ${eventId}: cannot post the reminder in <#${event.channelId}>`);
    return;
  }
  for (const m of reminderMessages(event, going.map((r) => r.userId))) {
    try {
      await found.channel.send({ content: m.content, allowedMentions: { parse: [], users: m.userIds } });
    } catch (err) {
      logger.warn(`event ${eventId}: could not post the reminder`, err);
      return;
    }
  }
}

/**
 * Scheduler handler for `event.end`: ends the event (the status flip picks one winner), then takes the role back in batches;
 * a payload with `after` is the continuation of an earlier batch. A retry or a cancelled event still strips (idempotent).
 */
export async function handleEndJob(job, ctx) {
  const eventId = job.payload?.eventId;
  if (typeof eventId !== 'string' || !eventId) throw new PermanentJobError('event.end: missing eventId');
  const after = typeof job.payload?.after === 'string' ? job.payload.after : null;
  const run = Number.isInteger(job.payload?.run) ? job.payload.run : 0;

  let event;
  if (after === null) {
    const res = await closeEvent(ctx.client, eventId, { status: 'ENDED', reason: 'timer', guildId: job.guildId });
    event = res.event;
  } else {
    event = await prisma.guildEvent.findFirst({ where: { id: eventId, guildId: job.guildId } });
  }
  if (!event || isOpen(event)) return;
  const { more } = await stripRoles(ctx.client, event, { after, run });
  if (more) return { nextRunAt: new Date(Date.now() + EVENT.STRIP_RESUME_MS), payload: { eventId, after: more, run: run + 1 } };
  return undefined;
}
