// Reminders / Timers (pkg-reminders): scheduled channel messages on the shared DB-backed scheduler.
// Three modes: TIME_OF_DAY (MEE6 "Single"), INTERVAL (MEE6 "Recurring") and ONE_SHOT. This file holds the constants,
// the pure helpers (validation, next-run maths, the activity gate, payload building, descriptions), the DB helpers
// and the `reminder.fire` job handler. It never replies to an interaction; /reminder and the modal handler do that.
//
// State lives only in Postgres (Reminder + ScheduledJob rows): no Redis keys. The per-guild create lock is an
// in-process mutex, which is enough because a guild's commands always reach the same shard.
import { createHash } from 'node:crypto';
import { PermissionFlagsBits, ChannelType } from 'discord.js';
import prisma from '../database/client.js';
import { COLORS, LIMITS } from '../config/constants.js';
import { ensureGuildSettings, getSettingsSafe, isModuleEnabled } from './settingsService.js';
import { cancelJobs, getJob, scheduleJob } from './scheduler.js';
import { TimeoutError, createKeyedMutex, createThrottle, withTimeout } from '../utils/async.js';
import { normalizeTemplate, renderTemplate } from '../utils/customEmbed.js';
import { checkChannelUsable, pingableRoleIds } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { parseDuration, formatDurationLong } from '../utils/parseDuration.js';
import { clampText, sanitizeName } from '../utils/text.js';
import {
  WEEKDAY_NAMES,
  discordTimestamp,
  formatTimeOfDay,
  isValidTimeZone,
  nextIntervalRun,
  nextTimeOfDay,
  parseDateInput,
  parseTimeOfDay,
  parseWeekdays,
} from '../utils/time.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const DEFAULT_CONTENT = "Hey, I'm a reminder!";
export const DEFAULT_TEMPLATE = Object.freeze({ content: DEFAULT_CONTENT, embed: null, buttons: [] });
export const MODE_FROM_CHOICE = Object.freeze({ time: 'TIME_OF_DAY', interval: 'INTERVAL', once: 'ONE_SHOT' });
export const MODE_LABELS = Object.freeze({ TIME_OF_DAY: 'Time of day', INTERVAL: 'Interval', ONE_SHOT: 'Once' });
export const REMINDER_CHANNEL_TYPES = Object.freeze([ChannelType.GuildText, ChannelType.GuildAnnouncement]);
export const LATE_TOLERANCE_MS = 15 * 60_000;
export const ONE_SHOT_LATE_MS = 60 * 60_000;

/** Added beyond the spec's export table: shared reply text for the command, the modal handler and summaryLines. */
export const MODULE_OFF_LINE = '⚠️ The **Reminders** module is off, so nothing is sent until an admin runs `/module enable reminders`.';
const USER_TOKEN_WARNING = '⚠️ Reminders have no member, so user variables like `{user}` are posted as written. Server and channel variables work.';

const MINUTE_MS = 60_000;
const DAY_MS = 86_400_000;
const SEND_TIMEOUT_MS = 20_000;
const USER_TOKEN_RE = /\{user(\.[a-z_]+)?\}/i;

/** An error whose title and message are shown to the admin as they are (cap reached, name taken). */
export class ReminderUserError extends Error {
  constructor(title, message) {
    super(message);
    this.name = 'ReminderUserError';
    this.title = title;
  }
}

// ── Small pure helpers ────────────────────────────────────────────────────────

/** @param {unknown} tz @returns {string} the timezone, or 'UTC' when it is not a valid IANA name */
export function resolveTimeZone(tz) {
  return isValidTimeZone(tz) ? tz : 'UTC';
}

/** @param {string} id @returns {string} the scheduler dedupeKey of a reminder */
export const jobKey = (id) => `reminder:${id}`;

const validDate = (d) => d instanceof Date && !Number.isNaN(d.getTime());

/** @returns {string|null} the line shown when the Reminders module is off (null when on or when settings are unknown) */
export function moduleOffLine(settings) {
  return settings && !isModuleEnabled(settings, 'reminders') ? MODULE_OFF_LINE : null;
}

/** @returns {string} the line shown for a paused reminder */
export const pausedLine = (name) => `⏸️ Paused — run \`/reminder resume name:${name}\` to schedule it.`;

// ── Validation ────────────────────────────────────────────────────────────────

const has = (v) => v !== undefined && v !== null;
const NONE_WORDS = new Set(['none', 'off', 'clear', '-']);

function parseWindowBound(v) {
  if (NONE_WORDS.has(String(v).trim().toLowerCase())) return { value: null };
  const t = parseTimeOfDay(v);
  return t ? { value: formatTimeOfDay(t) } : null;
}

/**
 * Turns the raw /reminder options into the Reminder schedule columns.
 * The first error wins. `base` is the existing row on edit (carry-over of the options that were not provided).
 * @param {{ mode?: 'time'|'interval'|'once', time?: string, weekdays?: string, interval?: string,
 *   windowStart?: string, windowEnd?: string, at?: string, requireActivity?: boolean }} input undefined = not provided
 * @param {{ tz: string, now?: Date, base?: object|null }} ctx
 * @returns {{ ok: true, fields: { mode: string, timeOfDay: string|null, weekdays: number[], intervalMinutes: number|null,
 *   windowStart: string|null, windowEnd: string|null, runAt: Date|null, requireActivity: boolean } }
 *   | { ok: false, error: string }}
 */
export function validateReminderInput(input, { tz, now = new Date(), base = null } = {}) {
  const inp = input ?? {};
  const err = (error) => ({ ok: false, error });

  // 1. Mode.
  const mode = MODE_FROM_CHOICE[inp.mode] ?? base?.mode;
  if (!mode) return err('Pick a `mode`.');

  // 2. Options that do not apply to the mode (named as the slash options).
  const applies = {
    time: mode === 'TIME_OF_DAY',
    interval: mode === 'INTERVAL',
    windowStart: mode === 'INTERVAL',
    windowEnd: mode === 'INTERVAL',
    at: mode === 'ONE_SHOT',
    weekdays: mode !== 'ONE_SHOT',
  };
  const slashName = { time: 'time', interval: 'interval', windowStart: 'window_start', windowEnd: 'window_end', at: 'at', weekdays: 'weekdays' };
  const bad = Object.keys(applies).filter((k) => has(inp[k]) && !applies[k]).map((k) => slashName[k]);
  if (inp.requireActivity === true && mode === 'ONE_SHOT') bad.push('require_activity');
  if (bad.length) {
    return err(`These options don't apply to **${MODE_LABELS[mode]}** mode: ${bad.map((n) => `\`${n}\``).join(', ')}.`);
  }

  // 3. Carry-over from the existing row for options that were not provided.
  let weekdays = [];
  let timeOfDay = null;
  let intervalMinutes = null;
  let windowStart = null;
  let windowEnd = null;
  let runAt = null;
  let requireActivity = false;
  if (base) {
    if (base.mode !== 'ONE_SHOT' && mode !== 'ONE_SHOT') weekdays = [...(base.weekdays ?? [])];
    if (base.mode === 'TIME_OF_DAY' && mode === 'TIME_OF_DAY') timeOfDay = base.timeOfDay ?? null;
    if (base.mode === 'INTERVAL' && mode === 'INTERVAL') {
      intervalMinutes = base.intervalMinutes ?? null;
      windowStart = base.windowStart ?? null;
      windowEnd = base.windowEnd ?? null;
    }
    if (base.mode === 'ONE_SHOT' && mode === 'ONE_SHOT') runAt = base.runAt ?? null;
    if (mode !== 'ONE_SHOT') requireActivity = Boolean(base.requireActivity);
  }

  // 4. Parse the provided values, in this order.
  if (has(inp.time)) {
    const t = parseTimeOfDay(inp.time);
    if (!t) return err("I couldn't read that time of day. Try `09:00`, `9:30pm` or `21:30`.");
    timeOfDay = formatTimeOfDay(t);
  }
  if (has(inp.weekdays)) {
    const days = parseWeekdays(inp.weekdays);
    if (!days || days.length === 0) {
      return err("I couldn't read those days. Use names like `mon,wed,fri`, or `weekdays`, `weekends`, `all`.");
    }
    weekdays = days.length === 7 ? [] : days;
  }
  if (has(inp.interval)) {
    const raw = String(inp.interval).trim();
    if (/^\d{1,5}$/.test(raw)) {
      intervalMinutes = Number(raw);
    } else {
      const ms = parseDuration(raw);
      if (ms === null) return err("I couldn't read that interval. Try `30m`, `2h` or `1d`.");
      intervalMinutes = Math.round(ms / MINUTE_MS);
    }
  }
  for (const [key, setter] of [['windowStart', (v) => { windowStart = v; }], ['windowEnd', (v) => { windowEnd = v; }]]) {
    if (!has(inp[key])) continue;
    const parsed = parseWindowBound(inp[key]);
    if (!parsed) return err('Window times use HH:MM, e.g. `09:00`, or `none`.');
    setter(parsed.value);
  }
  if (has(inp.at)) {
    const d = parseDateInput(inp.at, tz, { now });
    if (!validDate(d)) return err("I couldn't read that time. Try `18:00`, `2026-12-24 09:00` or a delay like `2h30m`.");
    runAt = d;
  }
  if (has(inp.requireActivity) && mode !== 'ONE_SHOT') requireActivity = Boolean(inp.requireActivity);

  // 5. Required fields by mode.
  if (mode === 'TIME_OF_DAY' && !timeOfDay) return err('Time mode needs `time`, e.g. `09:00`.');
  if (mode === 'INTERVAL' && intervalMinutes === null) return err('Interval mode needs `interval`, e.g. `2h`.');
  if (mode === 'ONE_SHOT' && !runAt) return err('Once mode needs `at`, e.g. `18:00` or `2h`.');

  // 6. INTERVAL limits.
  if (mode === 'INTERVAL') {
    const minutes = intervalMinutes;
    if (minutes < 5) return err('The shortest interval is 5 minutes.');
    if (minutes > 10_080) return err('The longest interval is 7 days.');
    if ((windowStart !== null || windowEnd !== null) && minutes >= 1440) {
      return err('With a daily window the interval must be under 24 hours. For daily or weekly reminders use the Time of day mode.');
    }
    if (windowStart !== null && windowEnd !== null && windowStart === windowEnd) {
      return err("The window start and end can't be the same time.");
    }
    if (weekdays.length > 0 && minutes > 1440) return err('Weekdays can only be used with intervals of 1 day or less.');
  }

  // 7. ONE_SHOT limits (also for a carried-over runAt).
  if (mode === 'ONE_SHOT') {
    const at = runAt.getTime();
    if (at <= now.getTime() + MINUTE_MS) return err('That time has passed or is less than a minute away. Pick a later `at`.');
    if (at > now.getTime() + 366 * DAY_MS) return err('Pick a time within one year.');
  }

  // 8. Never-fires check.
  const fields = { mode, timeOfDay, weekdays, intervalMinutes, windowStart, windowEnd, runAt, requireActivity };
  const candidate = { ...fields, createdAt: base?.createdAt ?? now };
  if (computeNextRun(candidate, tz, now) === null) return err('This schedule never fires. Check the weekdays and the time window.');
  return { ok: true, fields };
}

// ── Next-run maths ────────────────────────────────────────────────────────────

/**
 * The next occurrence of a reminder strictly after `after`, or null. INTERVAL grids are anchored on `createdAt`
 * (never `updatedAt`, which changes on every write); a window (either bound) ignores the anchor.
 * @param {object} r a Reminder row (or a plain object with the same fields)
 * @param {string} tz
 * @param {Date} [after]
 * @returns {Date|null}
 */
export function computeNextRun(r, tz, after = new Date()) {
  switch (r.mode) {
    case 'TIME_OF_DAY': {
      const t = parseTimeOfDay(r.timeOfDay ?? '');
      if (!t) return null;
      const d = nextTimeOfDay({ tz, hour: t.hour, minute: t.minute, weekdays: r.weekdays ?? [], after });
      return validDate(d) ? d : null;
    }
    case 'INTERVAL':
      if (!Number.isInteger(r.intervalMinutes) || r.intervalMinutes < 5) return null;
      return nextIntervalRun({
        tz,
        intervalMinutes: r.intervalMinutes,
        weekdays: r.weekdays ?? [],
        windowStart: r.windowStart ?? null,
        windowEnd: r.windowEnd ?? null,
        after,
        anchor: r.createdAt ?? null,
      });
    case 'ONE_SHOT':
      return r.runAt && r.runAt.getTime() > after.getTime() ? r.runAt : null;
    default:
      return null;
  }
}

/**
 * When the job should run. A ONE_SHOT whose time passed less than an hour ago still runs (immediately).
 * @returns {Date|null}
 */
export function jobRunAt(r, tz, now = new Date()) {
  if (r.mode === 'ONE_SHOT') {
    if (!r.runAt) return null;
    return now.getTime() - r.runAt.getTime() <= ONE_SHOT_LATE_MS ? r.runAt : null;
  }
  return computeNextRun(r, tz, now);
}

/** @returns {number} how late an occurrence may still be sent (ms) */
export function lateToleranceMs(r) {
  if (r.mode === 'ONE_SHOT') return ONE_SHOT_LATE_MS;
  if (r.mode === 'INTERVAL') {
    const half = Math.floor(((Number(r.intervalMinutes) || 0) * MINUTE_MS) / 2);
    return Math.min(LATE_TOLERANCE_MS, half);
  }
  return LATE_TOLERANCE_MS;
}

/**
 * The occurrence of the CURRENT configuration that is due now (within the late tolerance), or null. The job handler
 * never trusts `job.runAt`: this one rule covers restart lateness, timezone changes, edits that race a claimed job
 * and clock skew. Known limitation: right after a timezone change the first occurrence may be skipped once.
 * @returns {Date|null}
 */
export function dueSlot(r, tz, now = new Date()) {
  if (r.mode === 'ONE_SHOT') {
    if (!r.runAt) return null;
    const late = now.getTime() - r.runAt.getTime();
    return late >= 0 && late <= ONE_SHOT_LATE_MS ? r.runAt : null;
  }
  const s = computeNextRun(r, tz, new Date(now.getTime() - lateToleranceMs(r)));
  return s && s.getTime() <= now.getTime() ? s : null;
}

// ── Templates and payloads ────────────────────────────────────────────────────

/**
 * True when a submitted template has no text, embed text, fields or images (a colour alone is not content).
 * @param {{ content?: string|null, embed?: object|null }} t
 * @returns {boolean}
 */
export function isEmptyTemplate(t) {
  const text = (v) => typeof v === 'string' && v.trim() !== '';
  if (text(t?.content)) return false;
  const e = t?.embed;
  if (!e) return true;
  return !(text(e.title) || text(e.description) || (Array.isArray(e.fields) && e.fields.length > 0)
    || text(e.image) || text(e.thumbnail) || text(e.author?.name) || text(e.footer?.text));
}

/** True when the template mentions a user variable (`{user}`, `{user.name}` ...), which a reminder cannot fill in. */
export function hasUserTokens(tpl) {
  try {
    const t = normalizeTemplate(tpl);
    return [t.content, t.embed?.title, t.embed?.description, t.embed?.footer?.text]
      .some((s) => typeof s === 'string' && USER_TOKEN_RE.test(s));
  } catch {
    return false;
  }
}

/**
 * The message payload of a reminder. Placeholders see the guild and channel only (no member: `{user…}` stays literal).
 * `@everyone` and `@here` never ping. The ping role goes through `allowedMentions.roles` only, and only while it is
 * still pingable by the bot (a role that stopped being pingable is not pinged; the reminder still sends).
 * @param {object} r Reminder row
 * @param {{ guild: object, channel: object, settings: object|null, ping?: boolean }} ctx
 * @returns {{ content?: string, embeds: object[], components: object[], allowedMentions: object }}
 */
export function buildSendPayload(r, { guild, channel, settings, ping = true }) {
  const tpl = normalizeTemplate(r.template);
  const roles = ping && r.pingRoleId ? pingableRoleIds(channel, guild?.members?.me ?? null, [r.pingRoleId]).allowed : [];
  const allowedMentions = ping ? { parse: ['users'], roles } : { parse: [] };
  const opts = { defaultColor: settings?.embedColor ?? COLORS.INFO, allowedMentions };
  const renderCtx = { guild, channel };
  const payload = renderTemplate(tpl, renderCtx, opts)
    ?? renderTemplate({ ...DEFAULT_TEMPLATE, buttons: [] }, renderCtx, opts);
  if (r.pingRoleId) {
    payload.content = clampText(`<@&${r.pingRoleId}> ${payload.content ?? ''}`.trimEnd(), 2000); // the prefix survives the clamp
  }
  payload.allowedMentions = allowedMentions;
  return payload;
}

// ── Descriptions ──────────────────────────────────────────────────────────────

function weekdayLabel(weekdays) {
  const wd = [...new Set(weekdays ?? [])].sort((a, b) => a - b);
  if (wd.length === 0) return 'Every day';
  if (wd.join() === '1,2,3,4,5') return 'Weekdays';
  if (wd.join() === '0,6') return 'Weekends';
  return wd.map((d) => {
    const n = WEEKDAY_NAMES[d] ?? '';
    return n.charAt(0).toUpperCase() + n.slice(1);
  }).join(', ');
}

/** @returns {string} e.g. 'Weekdays at 09:00', 'Every 2 hours between 09:00 and 17:00 on weekdays', 'Once at <t:…:f>' */
export function describeSchedule(r) {
  const wd = weekdayLabel(r.weekdays);
  switch (r.mode) {
    case 'TIME_OF_DAY':
      return `${wd} at ${r.timeOfDay ?? '??:??'}`;
    case 'INTERVAL': {
      let s = `Every ${formatDurationLong((r.intervalMinutes ?? 0) * MINUTE_MS)}`;
      if (r.windowStart || r.windowEnd) s += ` between ${r.windowStart ?? '00:00'} and ${r.windowEnd ?? '24:00'}`;
      if (r.weekdays?.length) s += ` on ${wd === 'Weekdays' || wd === 'Weekends' ? wd.toLowerCase() : wd}`;
      return s;
    }
    case 'ONE_SHOT':
      return r.runAt ? `Once at ${discordTimestamp(r.runAt, 'f')}` : 'Once';
    default:
      return 'Unknown schedule';
  }
}

/**
 * The confirmation lines of create / edit / resume / the modal. Joined by the caller (clamped to 4096).
 * @param {object} r Reminder row
 * @param {object|null} settings
 * @param {Date|null} next
 * @param {string[]} [warnings]
 * @returns {string[]}
 */
export function summaryLines(r, settings, next, warnings = []) {
  const tz = resolveTimeZone(settings?.timezone);
  const lines = [
    `**${r.name}** → <#${r.channelId}>`,
    describeSchedule(r),
    next ? `Next: ${discordTimestamp(next, 'F')} (${discordTimestamp(next, 'R')})` : 'Next: —',
  ];
  if (r.enabled === false) lines.push(pausedLine(r.name));
  lines.push(`Timezone: ${tz} (change with \`/settings timezone\`)`);
  if (r.requireActivity) lines.push('💬 Skipped when nobody has chatted since the last reminder.');
  if (r.pingRoleId) lines.push(`🔔 Pings <@&${r.pingRoleId}>`);
  lines.push(`Use \`/reminder test name:${r.name}\` to preview it.`);
  lines.push(...warnings);
  const off = moduleOffLine(settings);
  if (off) lines.push(off);
  return lines;
}

/**
 * Warnings for a saved (or about-to-be-saved) reminder. Synchronous, never throws. Channel-based warnings are skipped
 * when the channel is not cached. (The ping-role warning is gone: a non-pingable role is refused when it is set.)
 * @param {object} r
 * @param {object} guild
 * @param {unknown} [tpl]
 * @returns {string[]}
 */
export function reminderWarnings(r, guild, tpl = normalizeTemplate(r.template)) {
  const out = [];
  try {
    const channel = guild?.channels?.cache?.get(r.channelId);
    if (channel && r.requireActivity) {
      const me = guild.members?.me;
      const perms = me ? channel.permissionsFor?.(me) : null;
      if (perms && !perms.has(PermissionFlagsBits.ReadMessageHistory)) {
        out.push(`⚠️ I can't read message history in <#${r.channelId}>, so the activity check only sees the last cached message.`);
      }
    }
    if (hasUserTokens(tpl)) out.push(USER_TOKEN_WARNING);
  } catch (err) {
    logger.warn(`reminderWarnings failed: ${err?.message}`);
  }
  return out;
}

// ── Activity gate ─────────────────────────────────────────────────────────────

/**
 * MEE6 anti-spam: send only when at least one non-bot, non-system message came after our last reminder.
 * Fail-open when the history is unknown: a reminder that never sends because Discord was unreachable is worse
 * than one extra message.
 * @param {{ requireActivity?: boolean, lastMessageId?: string|null, channelLastMessageId?: string|null,
 *   recent?: Array<{ bot?: boolean, system?: boolean }>|null, cachedLast?: { bot?: boolean, system?: boolean }|null }} state
 * @returns {boolean}
 */
export function activityAllowsSend({ requireActivity, lastMessageId, channelLastMessageId = null, recent = null, cachedLast = null }) {
  if (!requireActivity || !lastMessageId) return true; // never sent here yet: send
  if (channelLastMessageId && channelLastMessageId === lastMessageId) return false; // nothing after our reminder
  const human = (m) => !!m && !m.bot && !m.system;
  if (Array.isArray(recent)) return recent.some(human) || human(cachedLast);
  return cachedLast ? human(cachedLast) : true; // history unknown: fail-open
}

// ── DB helpers ────────────────────────────────────────────────────────────────

/**
 * @param {string} guildId
 * @param {string} raw the name as typed (normalised with sanitizeName)
 * @returns {Promise<object|null>}
 * @throws on a DB failure
 */
export async function findReminderByName(guildId, raw) {
  const name = sanitizeName(raw, { max: 32, fallback: '' });
  if (!name) return null;
  return prisma.reminder.findUnique({ where: { guildId_name: { guildId, name } } });
}

const createLock = createKeyedMutex();

/**
 * Creates a reminder row under a per-guild lock (cap and unique name). `createdAt` is set explicitly because it is
 * the INTERVAL grid anchor.
 * @param {{ guildId: string, name: string, channelId: string, template: object, fields: object, pingRoleId?: string|null,
 *   createdBy: string, now: Date }} args
 * @returns {Promise<object>} the row
 * @throws {ReminderUserError} cap reached or name taken; any other error is a DB failure
 */
export async function createReminderRow({ guildId, name, channelId, template, fields, pingRoleId, createdBy, now }) {
  return createLock.run(guildId, async () => {
    await ensureGuildSettings(guildId);
    const count = await prisma.reminder.count({ where: { guildId } });
    if (count >= LIMITS.REMINDERS) {
      throw new ReminderUserError('Limit Reached',
        `This server already has the maximum of ${LIMITS.REMINDERS} reminders. Delete one with \`/reminder delete\` first.`);
    }
    try {
      return await prisma.reminder.create({
        data: {
          guildId, name, channelId, template, ...fields, pingRoleId: pingRoleId ?? null, createdBy, enabled: true, createdAt: now,
        },
      });
    } catch (err) {
      if (err?.code === 'P2002') {
        throw new ReminderUserError('Name Taken',
          `A reminder named **${name}** already exists. Use \`/reminder edit\` to change it or \`/reminder delete\` to remove it.`);
      }
      throw err;
    }
  });
}

/**
 * Makes the scheduler job match the row (replace, so an in-flight run of the old job becomes a no-op).
 * Throws on a DB failure.
 * @param {object} r Reminder row
 * @param {object|null} settings
 * @param {{ now?: Date }} [opts]
 * @returns {Promise<Date|null>} when it was scheduled, or null (nothing scheduled)
 */
export async function syncReminderJob(r, settings, { now = new Date() } = {}) {
  const key = jobKey(r.id);
  if (!r.enabled) {
    await cancelJobs({ dedupeKey: key });
    return null;
  }
  const runAt = jobRunAt(r, resolveTimeZone(settings?.timezone), now);
  if (!runAt) {
    await cancelJobs({ dedupeKey: key });
    return null;
  }
  await scheduleJob({
    guildId: r.guildId, type: 'reminder.fire', runAt, payload: { reminderId: r.id }, dedupeKey: key, maxAttempts: 10, onConflict: 'replace',
  });
  return runAt;
}

/** Pauses a reminder and cancels its job. */
export async function pauseReminder(r) {
  await prisma.reminder.update({ where: { id: r.id }, data: { enabled: false } });
  await cancelJobs({ dedupeKey: jobKey(r.id) });
}

/** Deletes a reminder (guild-scoped) and cancels its job. */
export async function deleteReminder(r) {
  await prisma.reminder.deleteMany({ where: { id: r.id, guildId: r.guildId } });
  await cancelJobs({ dedupeKey: jobKey(r.id) });
}

/**
 * Pauses a reminder that can no longer work and logs why, once (only the first caller sees count 1).
 * @param {object} r
 * @param {string} reason
 */
export async function autoDisable(r, reason) {
  const { count } = await prisma.reminder.updateMany({ where: { id: r.id, enabled: true }, data: { enabled: false } });
  if (count === 1) logger.warn(`Reminder "${r.name}" (${r.id}) in guild ${r.guildId} was paused automatically: ${reason}`);
}

/**
 * channelDelete cleanup: pauses the enabled reminders of a deleted channel and cancels their jobs. The rows stay,
 * so an admin can pick a new channel with /reminder edit and resume.
 * @returns {Promise<number>} how many were paused
 */
export async function disableRemindersForChannel(guildId, channelId) {
  const rows = await prisma.reminder.findMany({ where: { guildId, channelId, enabled: true }, select: { id: true } });
  if (!rows.length) return 0;
  await prisma.reminder.updateMany({ where: { id: { in: rows.map((x) => x.id) } }, data: { enabled: false } });
  for (const { id } of rows) await cancelJobs({ dedupeKey: jobKey(id) });
  logger.info(`Reminders: paused ${rows.length} reminder(s) in guild ${guildId}; channel ${channelId} was deleted`);
  return rows.length;
}

/**
 * guildReady self-healing: an enabled reminder with no live job (after a guild leave and rejoin, a lost job row)
 * gets its job back; one that can never fire again is paused.
 * @param {{ id: string }} guild
 */
export async function reconcileGuildReminders(guild) {
  const rows = await prisma.reminder.findMany({ where: { guildId: guild.id, enabled: true } });
  if (!rows.length) return;
  const settings = await getSettingsSafe(guild.id);
  if (!settings) return;
  for (const r of rows) {
    const job = await getJob(jobKey(r.id));
    if (job && (job.status === 'PENDING' || job.status === 'RUNNING')) continue;
    try {
      const at = await syncReminderJob(r, settings);
      if (!at) await autoDisable(r, r.mode === 'ONE_SHOT' ? 'it was missed while the bot was offline' : 'its schedule never fires');
    } catch (err) {
      logger.error(`reminders reconcile ${r.id}`, err);
    }
  }
}

// ── The job handler ───────────────────────────────────────────────────────────

const warnThrottle = createThrottle(6 * 60 * 60_000);

function warnOnce(r, kind, detail) {
  if (warnThrottle(`${r.id}:${kind}`)) logger.warn(`Reminder "${r.name}" (${r.id}) in guild ${r.guildId} skipped: ${detail}`);
}

function safeNext(r, tz, now) {
  try {
    return computeNextRun(r, tz, now);
  } catch {
    return null;
  }
}

/** The activity gate against Discord: true when a human posted after our last reminder (see activityAllowsSend). */
async function checkActivity(r, channel) {
  if (!r.lastMessageId) return true;
  if (channel.lastMessageId && channel.lastMessageId === r.lastMessageId) return false; // decided without a fetch
  const toLite = (m) => (m ? { bot: Boolean(m.author?.bot) || Boolean(m.webhookId), system: Boolean(m.system) } : null);
  const cachedLast = toLite(channel.lastMessage);
  let recent = null;
  const me = channel.guild?.members?.me;
  if (me && channel.permissionsFor?.(me)?.has(PermissionFlagsBits.ReadMessageHistory)) {
    try {
      const msgs = await channel.messages.fetch({ after: r.lastMessageId, limit: 50, cache: false });
      recent = [...msgs.values()].map(toLite);
    } catch (err) {
      warnOnce(r, 'history', err?.message);
    }
  }
  return activityAllowsSend({
    requireActivity: true, lastMessageId: r.lastMessageId, channelLastMessageId: channel.lastMessageId ?? null, recent, cachedLast,
  });
}

const GONE_CHANNEL_CODES = new Set([10003, 50001]); // Unknown Channel, Missing Access

// Send failures. Permanent: retrying cannot help (missing access / permissions, unknown channel / user, cannot DM, bad
// form body). Everything else is transient: a Discord 5xx, a 429, a network error, a timeout.
const PERMANENT_SEND_CODES = new Set([50001, 50013, 10003, 10013, 50007, 50035]);
// Mirrors scheduler.js: after failed attempt n the job waits 30 s * 2^(n-1), at most 1 h.
const SCHEDULER_RETRY_BASE_MS = 30_000;
const SCHEDULER_RETRY_MAX_MS = 60 * MINUTE_MS;
// Discord's nonce check only remembers "the past few minutes", so a send that timed out (and may still land) is retried
// only while the retry is this close; later the claim stays and the occurrence counts as sent (at most once).
const TIMEOUT_RETRY_MAX_DELAY_MS = MINUTE_MS;
const RETRY = Symbol('reminder.retry');

/** @returns {boolean} true when a failed send may succeed later (a timeout, 408, 429, 5xx, a network error) */
export function isTransientSendError(err) {
  if (err instanceof TimeoutError) return true;
  const code = err?.code;
  if (typeof code === 'number' && PERMANENT_SEND_CODES.has(code)) return false;
  const status = err?.status;
  if (Number.isInteger(status) && status >= 400 && status < 500) return status === 408 || status === 429;
  return true;
}

/** @returns {number} how long the scheduler waits after failed attempt `attempt` (1-based) */
export const schedulerRetryDelayMs = (attempt) => Math.min(SCHEDULER_RETRY_BASE_MS * 2 ** Math.max(attempt - 1, 0), SCHEDULER_RETRY_MAX_MS);

/**
 * Whether a failed attempt may ask the scheduler for another one: not the final attempt, and the retry still falls inside
 * the late tolerance of the occurrence. Without `attempt` / `maxAttempts` (not run by the scheduler) there is no retry.
 * @param {{ attempt?: number, maxAttempts?: number, lateMs: number, toleranceMs: number }} p
 * @returns {boolean}
 */
export function mayRetry({ attempt, maxAttempts, lateMs, toleranceMs }) {
  if (!Number.isInteger(attempt) || !Number.isInteger(maxAttempts) || attempt >= maxAttempts) return false;
  return lateMs + schedulerRetryDelayMs(attempt) < toleranceMs;
}

/** @returns {string} the Discord nonce of one occurrence (max 25 chars): a retry of the same occurrence is dropped as a duplicate */
export const occurrenceNonce = (reminderId, slot) => createHash('sha1').update(`${reminderId}:${slot.getTime()}`).digest('hex').slice(0, 25);

/** @returns {Promise<'sent'|'skipped'|'disabled'|'retry'>} 'retry' only when `retryable`: the caller asks the scheduler for another attempt */
async function deliver(r, slot, settings, ctx, { gate, retryable = false }) {
  const { guild } = ctx;

  // 1. Resolve the channel. A cache miss is not a deletion: only a REST answer of 10003 / 50001 pauses the reminder.
  let channel = guild.channels.cache.get(r.channelId);
  if (!channel) {
    try {
      channel = await guild.channels.fetch(r.channelId);
    } catch (err) {
      if (GONE_CHANNEL_CODES.has(err?.code)) {
        await autoDisable(r, 'its channel no longer exists or I lost access');
        return 'disabled';
      }
      if (retryable && isTransientSendError(err)) {
        logger.warn(`Reminder "${r.name}" (${r.id}) in guild ${r.guildId}: could not look up its channel (${err?.message}); retrying`);
        return 'retry';
      }
      warnOnce(r, 'fetch', err?.message);
      return 'skipped';
    }
  }
  if (!channel || !REMINDER_CHANNEL_TYPES.includes(channel.type)) {
    await autoDisable(r, 'its channel can no longer receive reminders');
    return 'disabled';
  }

  // 2. Payload.
  const payload = buildSendPayload(r, { guild, channel, settings, ping: true });

  // 3. Permissions. Without a cached `me` every permission looks missing, so never pause on that.
  const needs = ['ViewChannel', 'SendMessages', ...(payload.embeds.length ? ['EmbedLinks'] : [])];
  const u = checkChannelUsable(channel, { needs });
  if (!u.ok) {
    if (u.missing.includes('ViewChannel') && guild.members?.me) {
      await autoDisable(r, `I lost access to #${channel.name} (View Channel)`);
      return 'disabled';
    }
    warnOnce(r, 'perms', u.missing.length ? `missing ${u.missing.join(', ')} in #${channel.name}` : (u.reason ?? 'channel not usable'));
    return 'skipped';
  }

  // 4. Activity gate.
  if (gate && !(await checkActivity(r, channel))) return 'skipped';

  // 5. Claim the occurrence (idempotent: a job can run twice after a lock expiry).
  const claimedAt = new Date();
  const { count } = await prisma.reminder.updateMany({
    where: { id: r.id, enabled: true, OR: [{ lastSentAt: null }, { lastSentAt: { lt: slot } }] },
    data: { lastSentAt: claimedAt },
  });
  if (count !== 1) return 'skipped';

  // 6. Send. The nonce is the same on every attempt at this occurrence, so Discord drops a duplicate of a send that did land.
  try {
    const msg = await withTimeout(
      channel.send({ ...payload, nonce: occurrenceNonce(r.id, slot), enforceNonce: true }), SEND_TIMEOUT_MS, `reminder send ${r.id}`,
    );
    await prisma.reminder.update({ where: { id: r.id }, data: { lastMessageId: msg.id } })
      .catch((e) => logger.warn(`Reminder ${r.id}: could not store lastMessageId: ${e?.message}`));
    return 'sent';
  } catch (err) {
    const timedOut = err instanceof TimeoutError;
    const willRetry = retryable && isTransientSendError(err)
      && (!timedOut || schedulerRetryDelayMs(ctx.attempt) <= TIMEOUT_RETRY_MAX_DELAY_MS);
    // A timeout may still land, so its claim stays unless a quick retry is coming; any other failure gives the occurrence back.
    if (!timedOut || willRetry) {
      await prisma.reminder.updateMany({ where: { id: r.id, lastSentAt: claimedAt }, data: { lastSentAt: r.lastSentAt } }).catch(() => {});
    }
    if (GONE_CHANNEL_CODES.has(err?.code)) {
      await autoDisable(r, 'its channel no longer exists or I lost access');
      return 'disabled';
    }
    if (err?.code === 50035) {
      await autoDisable(r, `Discord rejected its message: ${clampText(err.message, 200)}`);
      return 'disabled';
    }
    if (willRetry) {
      logger.warn(`Reminder "${r.name}" (${r.id}) in guild ${r.guildId}: the send failed (${err?.message}); retrying`);
      return 'retry';
    }
    warnOnce(r, 'send', err?.message);
    return 'skipped';
  }
}

async function runRecurring(r, settings, tz, now, ctx) {
  const next = computeNextRun(r, tz, now);
  if (!next) {
    await autoDisable(r, 'its schedule never fires');
    return undefined;
  }
  const slot = dueSlot(r, tz, now);
  if (!slot) return { nextRunAt: next }; // not due yet, missed beyond the tolerance, or a timezone / edit mismatch
  if (!isModuleEnabled(settings, 'reminders')) return { nextRunAt: next }; // keep rescheduling without sending
  const retryable = mayRetry({
    attempt: ctx.attempt, maxAttempts: ctx.maxAttempts, lateMs: now.getTime() - slot.getTime(), toleranceMs: lateToleranceMs(r),
  });
  const outcome = await deliver(r, slot, settings, ctx, { gate: r.requireActivity, retryable });
  if (outcome === 'retry') return RETRY;
  return outcome === 'disabled' ? undefined : { nextRunAt: next };
}

async function runOneShot(r, settings, tz, now, ctx) {
  if (!r.runAt) {
    await autoDisable(r, 'it has no run time');
    return undefined;
  }
  if (r.runAt.getTime() > now.getTime()) return { nextRunAt: r.runAt }; // clock skew, or an edit moved it later
  if (now.getTime() - r.runAt.getTime() > ONE_SHOT_LATE_MS) {
    await autoDisable(r, 'it was missed by more than 60 minutes (bot offline)');
    return undefined;
  }
  if (!isModuleEnabled(settings, 'reminders')) {
    // A one-off message weeks later is worse than a skip, so it is never delayed until the module is back on.
    await autoDisable(r, 'the Reminders module was off at its time');
    return undefined;
  }
  const retryable = mayRetry({
    attempt: ctx.attempt, maxAttempts: ctx.maxAttempts, lateMs: now.getTime() - r.runAt.getTime(), toleranceMs: ONE_SHOT_LATE_MS,
  });
  const outcome = await deliver(r, r.runAt, settings, ctx, { gate: false, retryable });
  if (outcome === 'retry') return RETRY; // a transient failure: still enabled, the scheduler runs it again with backoff
  await prisma.reminder.updateMany({ where: { id: r.id }, data: { enabled: false } }); // finished, whatever the outcome
  return undefined;
}

/**
 * Scheduler handler of `reminder.fire`. Throws when the first DB read fails, and when a send failed for a transient reason
 * (Discord 5xx, 429, network error, timeout) with attempts left and the occurrence still inside its late tolerance: the
 * scheduler then retries with backoff (maxAttempts 10) and the claim of the occurrence was given back, so it is sent once.
 * It never throws on the final attempt, so a recurring reminder is not marked FAILED; a one-shot is finished then.
 * Every other failure is caught and a recurring reminder is rescheduled, so it never dies.
 * @param {{ guildId: string, maxAttempts?: number, payload: { reminderId?: string } }} job
 * @param {{ client: object, guild: object, attempt?: number }} ctx
 * @returns {Promise<{ nextRunAt: Date }|undefined>} undefined deletes the job
 */
export async function fireReminder(job, ctx) {
  const now = new Date();
  const reminderId = job.payload?.reminderId;
  if (typeof reminderId !== 'string') return undefined; // malformed: drop
  const r = await prisma.reminder.findFirst({ where: { id: reminderId, guildId: job.guildId } }); // may throw: retry
  if (!r || !r.enabled) return undefined; // deleted or paused: drop the job
  const settings = await getSettingsSafe(job.guildId);
  if (!settings) return { nextRunAt: new Date(now.getTime() + MINUTE_MS) }; // dueSlot decides on the retry
  const tz = resolveTimeZone(settings.timezone);
  const runCtx = { ...ctx, maxAttempts: job.maxAttempts };
  let result;
  try {
    result = r.mode === 'ONE_SHOT'
      ? await runOneShot(r, settings, tz, now, runCtx)
      : await runRecurring(r, settings, tz, now, runCtx);
  } catch (err) {
    logger.error(`reminder.fire ${r.id} (guild ${r.guildId}) failed unexpectedly`, err);
    const next = r.mode === 'ONE_SHOT' ? null : safeNext(r, tz, now);
    return { nextRunAt: validDate(next) ? next : new Date(now.getTime() + 15 * MINUTE_MS) };
  }
  // Outside the catch above, which would turn the retry into a reschedule.
  if (result === RETRY) throw new Error(`reminder ${r.id}: transient send failure, retrying (attempt ${ctx?.attempt}/${job.maxAttempts})`);
  return result;
}
