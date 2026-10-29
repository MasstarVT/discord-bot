// Database access of the `reminders` collection: the Reminder rows /reminder writes, validated by the same
// validateReminderInput, scheduled by the same syncReminderJob (`reminder.fire`, dedupe `reminder:{id}`, in the
// server's time zone) and removed by deleteReminder. The bot's in-process create lock does not reach this process, so
// the cap and unique name are guarded by a per-guild advisory lock here. The bot shard owning the guild picks the job
// up within about 5 s.
import { Prisma } from '@prisma/client';
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { cancelJobs } from '../../services/scheduler.js';
import { ensureGuildSettings, getSettingsSafe } from '../../services/settingsService.js';
import {
  buildSendPayload, computeNextRun, deleteReminder, jobKey, resolveTimeZone, syncReminderJob, validateReminderInput,
} from '../../services/reminderService.js';
import { normalizeTemplate } from '../../utils/customEmbed.js';
import { sanitizeName } from '../../utils/text.js';
import { RecordError } from './errors.js';
import { SEND_NEEDS, requireChannelAccess } from './channelAccess.js';
import {
  SLUG_MESSAGE, discordFailure, isDiscordApiError, memberFor, pingRoleProblem, renderContextFor, restAllowedMentions,
  toRestMessage,
} from './embeds.js';
import { reminderItem, scheduleInput } from './reminders.js';

const lockSql = (key) => Prisma.sql`SELECT 1 AS ok FROM (SELECT pg_advisory_xact_lock(hashtext(${key}))) AS l`;

/** Raw statements (test/db.test.js runs each one). */
export const RAW_SQL = Object.freeze([
  { name: 'dashboard.reminders.lockGuild', build: () => lockSql('reminder-create:0') },
]);

const notFound = () => new RecordError('not_found', 'That reminder no longer exists.');

async function findRow(guildId, id) {
  const row = await prisma.reminder.findFirst({ where: { id, guildId } });
  if (!row) throw notFound();
  return row;
}

async function settingsOf(guildId) {
  const settings = await getSettingsSafe(guildId);
  if (!settings) throw new RecordError('unavailable', 'The server settings could not be loaded. Try again in a moment.');
  return settings;
}

async function loadResources(ctx, guildId) {
  if (typeof ctx?.discord?.getGuildResources !== 'function') {
    throw new RecordError('unavailable', 'The server cannot be read from the dashboard right now.');
  }
  try {
    return await ctx.discord.getGuildResources(guildId);
  } catch (err) {
    if (isDiscordApiError(err)) throw new RecordError('discord', 'The server could not be loaded from Discord. Try again in a moment.');
    throw err;
  }
}

// validateReminderInput speaks in slash-option names; the form shows labels and highlights the field.
// First match wins: "With a daily window the interval must be under 24 hours" is about the interval.
const ERROR_PATHS = [
  [/interval/i, 'intervalMinutes'],
  [/window/i, 'windowEnd'],
  [/time of day|Time mode/i, 'timeOfDay'],
  [/days|Weekdays|never fires/i, 'weekdays'],
  [/that time|Once mode|has passed|within one year/i, 'runAt'],
];
const OPTION_LABELS = Object.freeze({
  time: 'time of day', at: 'date and time', interval: 'interval', mode: 'repeat mode', weekdays: 'days',
  window_start: 'daily start', window_end: 'daily stop',
});

function scheduleError(message) {
  const text = String(message).replace(/`([^`]+)`/g, (m, inner) => OPTION_LABELS[inner] ?? inner);
  const path = ERROR_PATHS.find(([re]) => re.test(message))?.[1];
  return new RecordError('invalid', text, path ? { path } : {});
}

function validateSchedule(s, { tz, now, base }) {
  const v = validateReminderInput(scheduleInput(s), { tz, now, base });
  if (!v.ok) throw scheduleError(v.error);
  return v.fields;
}

async function checkPingRole(guildId, roleId, ctx) {
  const resources = await loadResources(ctx, guildId);
  const problem = pingRoleProblem(resources, roleId, memberFor(ctx, resources));
  if (problem) throw new RecordError('invalid', problem, { path: 'pingRoleId' });
}

function toItem(row, settings, now = new Date()) {
  const tz = resolveTimeZone(settings?.timezone);
  let next = null;
  try {
    next = row.enabled ? computeNextRun(row, tz, now) : null;
  } catch {
    next = null;
  }
  return reminderItem(row, { tz, nextRunAt: next });
}

export const store = Object.freeze({
  async list(guildId) {
    const [rows, settings] = await Promise.all([
      prisma.reminder.findMany({ where: { guildId }, orderBy: { name: 'asc' }, take: LIMITS.REMINDERS }),
      getSettingsSafe(guildId),
    ]);
    const now = new Date();
    return rows.map((r) => toItem(r, settings, now));
  },

  async get(guildId, id) {
    const row = await prisma.reminder.findFirst({ where: { id, guildId } });
    return row ? toItem(row, await getSettingsSafe(guildId)) : null;
  },

  async create(guildId, data, ctx) {
    const name = data.name;
    if (sanitizeName(name, { max: 32, fallback: '' }) !== name) throw new RecordError('invalid', SLUG_MESSAGE, { path: 'name' });
    await ensureGuildSettings(guildId);
    const settings = await settingsOf(guildId);
    const tz = resolveTimeZone(settings.timezone);
    const now = new Date();
    const fields = validateSchedule(data, { tz, now, base: null });
    if (data.pingRoleId) await checkPingRole(guildId, data.pingRoleId, ctx);
    const template = normalizeTemplate(data.template);

    let row;
    try {
      row = await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(lockSql(`reminder-create:${guildId}`));
        if (await tx.reminder.count({ where: { guildId } }) >= LIMITS.REMINDERS) {
          throw new RecordError('limit', `This server already has the maximum of ${LIMITS.REMINDERS} reminders. Delete one first.`);
        }
        // createdAt is set explicitly: it anchors the INTERVAL grid (createReminderRow does the same).
        return tx.reminder.create({
          data: {
            guildId, name, channelId: data.channelId, template, ...fields, pingRoleId: data.pingRoleId ?? null,
            createdBy: String(ctx?.userId ?? '0'), enabled: data.enabled !== false, createdAt: now,
          },
        });
      });
    } catch (err) {
      if (err?.code === 'P2002') throw new RecordError('conflict', `A reminder named "${name}" already exists.`, { path: 'name' });
      throw err;
    }
    await syncReminderJob(row, settings, { now });
    return toItem(row, settings, now);
  },

  async update(guildId, id, data, ctx) {
    const row = await findRow(guildId, id);
    const settings = await settingsOf(guildId);
    const tz = resolveTimeZone(settings.timezone);
    const now = new Date();
    const before = reminderItem(row, { tz });
    const merged = { ...before, ...data };
    const patch = {};

    // The schedule is validated only when it changed (compared as the options of its mode, so the order of the days
    // or the hidden fields of other modes do not count), so a finished one-off can still get a new channel or text.
    if (JSON.stringify(scheduleInput(merged)) !== JSON.stringify(scheduleInput(before))) {
      Object.assign(patch, validateSchedule(merged, { tz, now, base: row }));
    }
    if (Object.hasOwn(data, 'channelId') && data.channelId !== row.channelId) {
      patch.channelId = data.channelId;
      patch.lastMessageId = null; // the activity gate's baseline belongs to the old channel
    }
    if (Object.hasOwn(data, 'template')) patch.template = normalizeTemplate(data.template);
    if (Object.hasOwn(data, 'pingRoleId') && data.pingRoleId !== row.pingRoleId) {
      if (data.pingRoleId) await checkPingRole(guildId, data.pingRoleId, ctx);
      patch.pingRoleId = data.pingRoleId;
    }
    if (Object.hasOwn(data, 'enabled') && data.enabled !== row.enabled) {
      if (data.enabled) {
        // /reminder resume: a one-off whose time passed, a schedule that never fires or a deleted channel stay paused.
        const next = { ...row, ...patch };
        if (next.mode === 'ONE_SHOT' && (!next.runAt || next.runAt.getTime() <= now.getTime() + 60_000)) {
          throw new RecordError('invalid', "That one-time reminder's time has passed. Pick a new date and time, then turn it on.", {
            path: 'runAt',
          });
        }
        if (computeNextRun(next, tz, now) === null) {
          throw new RecordError('invalid', 'This schedule never fires. Check the days and the daily window.', { path: 'weekdays' });
        }
        const resources = await loadResources(ctx, guildId);
        if (!(resources.channels ?? []).some((c) => c.id === next.channelId)) {
          throw new RecordError('invalid', 'Its channel no longer exists. Pick a new channel first.', { path: 'channelId' });
        }
      }
      patch.enabled = data.enabled;
    }

    if (Object.keys(patch).length) {
      const { count } = await prisma.reminder.updateMany({ where: { id, guildId }, data: patch });
      if (!count) throw notFound();
    }
    const updated = await findRow(guildId, id);
    // Make the job match the row, as every /reminder write does (pause cancels it).
    if (updated.enabled) await syncReminderJob(updated, settings, { now });
    else await cancelJobs({ dedupeKey: jobKey(updated.id) });
    return toItem(updated, settings, now);
  },

  async remove(guildId, id) {
    const row = await findRow(guildId, id);
    await deleteReminder(row); // guild-scoped delete + cancels `reminder:{id}`
  },

  actions: Object.freeze({
    /** /reminder test: posts a preview now (no pings, the schedule, lastSentAt and the job are untouched). */
    async test(guildId, id, data, ctx) {
      if (typeof ctx?.discord?.sendMessage !== 'function') {
        throw new RecordError('unavailable', 'Posting messages from the dashboard is not available right now.');
      }
      const row = await findRow(guildId, id);
      const resources = await loadResources(ctx, guildId);
      const channelId = data.channelId ?? row.channelId;
      const channel = (resources.channels ?? []).find((c) => c.id === channelId);
      if (!channel) throw new RecordError('invalid', 'That channel no longer exists. Pick another one.', { path: 'channelId' });
      // The bot posts now, in any channel the form names: the user needs to be able to post there themselves.
      await requireChannelAccess(ctx, [{ channelId, needs: SEND_NEEDS, path: 'channelId' }]);
      const settings = await getSettingsSafe(guildId);
      const { guild, channel: renderChannel } = renderContextFor(resources, { channelId });
      const payload = buildSendPayload(row, { guild, channel: renderChannel, settings, ping: false });
      const body = toRestMessage(payload, { allowedMentions: restAllowedMentions({ users: false }) });
      try {
        await ctx.discord.sendMessage(channelId, body);
      } catch (err) {
        throw discordFailure(err, { op: 'send', channel, embeds: body.embeds.length > 0 });
      }
      return { item: toItem(row, settings), message: `Posted a test of "${row.name}" in #${channel.name}. Roles were not pinged.` };
    },
  }),
});
