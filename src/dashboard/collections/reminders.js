// The `reminders` collection (Reminders / Timers): the Reminder rows behind /reminder, scheduled channel messages in
// three modes (a time of day on chosen weekdays, every interval inside an optional daily window, or once).
// Times of day are in the server's time zone (General settings); the one-off date is an absolute instant.
// Pure: nothing is read or connected at import time.
import { LIMITS } from '../../config/constants.js';
import { formatDurationLong } from '../../utils/parseDuration.js';
import { normalizeTemplate } from '../../utils/customEmbed.js';
import { WEEKDAY_NAMES } from '../../utils/time.js';
import { POSTABLE_CHANNEL_KINDS, SLUG_MESSAGE, SLUG_PATTERN } from './embeds.js';

/** The default message (reminderService.DEFAULT_TEMPLATE, MEE6's default text). */
export const DEFAULT_REMINDER_TEMPLATE = Object.freeze({ content: "Hey, I'm a reminder!", embed: null, buttons: [] });

export const MODE_OPTIONS = Object.freeze([
  { value: 'TIME_OF_DAY', label: 'At a time of day' },
  { value: 'INTERVAL', label: 'Every interval' },
  { value: 'ONE_SHOT', label: 'Once' },
]);

const DAY_LABELS = Object.freeze({
  sun: 'Sunday', mon: 'Monday', tue: 'Tuesday', wed: 'Wednesday', thu: 'Thursday', fri: 'Friday', sat: 'Saturday',
});
/** Monday first; the values are the stored day numbers' names (WEEKDAY_NAMES: 0 = sun). */
const WEEKDAY_OPTIONS = Object.freeze(['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'].map((v) => ({ value: v, label: DAY_LABELS[v] })));

export const collection = Object.freeze({
  key: 'reminders',
  module: 'reminders',
  label: 'Reminders',
  singular: 'reminder',
  description: 'Messages the bot posts on a schedule: at a time of day, every few minutes or hours, or once. '
    + 'Times of day use the server time zone from the General settings.',
  limit: LIMITS.REMINDERS,
  titleField: 'name',
  subtitleField: 'schedule',
  fields: Object.freeze([
    {
      field: 'name',
      type: 'text',
      label: 'Name',
      description: 'A short unique name, also used by /reminder in Discord. It cannot be changed later.',
      maxLength: 32,
      minLength: 1,
      pattern: SLUG_PATTERN,
      patternMessage: SLUG_MESSAGE,
      required: true,
    },
    {
      field: 'channelId',
      type: 'channel',
      label: 'Channel',
      description: 'Where the reminder is posted.',
      channelTypes: [...POSTABLE_CHANNEL_KINDS],
      required: true,
    },
    {
      field: 'template',
      type: 'template',
      label: 'Message',
      description: 'Server and channel variables work. Reminders have no member, so {user} variables are posted as written.',
      requireContent: true,
      placeholders: [],
      placeholderGroups: ['server', 'channel', 'other'],
      default: DEFAULT_REMINDER_TEMPLATE,
    },
    { field: 'mode', type: 'enum', label: 'Repeat', options: MODE_OPTIONS, default: 'TIME_OF_DAY' },
    {
      field: 'timeOfDay',
      type: 'time',
      label: 'Time of day',
      description: '"At a time of day": when it is posted, in the server time zone.',
      nullable: true,
      default: '09:00',
    },
    {
      field: 'weekdays',
      type: 'multiEnum',
      label: 'Days',
      description: '"At a time of day" and "Every interval": the days it runs on. None selected: every day.',
      options: WEEKDAY_OPTIONS,
      maxItems: 7,
      default: [],
    },
    {
      field: 'intervalMinutes',
      type: 'int',
      label: 'Interval',
      description: '"Every interval": time between posts, 5 minutes to 7 days (10080 minutes).',
      min: 5,
      max: 10_080,
      unit: 'minutes',
      nullable: true,
      default: 60,
    },
    {
      field: 'windowStart',
      type: 'time',
      label: 'Daily start',
      description: '"Every interval": only post from this time of day (needs an interval under 24 hours). Empty: from midnight.',
      nullable: true,
      default: null,
    },
    {
      field: 'windowEnd',
      type: 'time',
      label: 'Daily stop',
      description: '"Every interval": stop posting at this time of day. Empty: until midnight.',
      nullable: true,
      default: null,
    },
    {
      field: 'runAt',
      type: 'datetime',
      label: 'Date and time',
      description: '"Once": when it is posted (within one year).',
      nullable: true,
      default: null,
    },
    {
      field: 'requireActivity',
      type: 'boolean',
      label: 'Skip when the channel is quiet',
      description: 'Not posted unless someone chatted since the last reminder (not for "Once").',
      default: false,
    },
    {
      field: 'pingRoleId',
      type: 'role',
      label: 'Role to ping',
      description: 'Mentioned at the start of every reminder.',
      nullable: true,
      default: null,
    },
    { field: 'enabled', type: 'boolean', label: 'Enabled', description: 'Off: paused, nothing is posted.', default: true },
  ]),
  createOnly: Object.freeze(['name']),
  readOnly: Object.freeze([
    { field: 'status', type: 'text', label: 'Status', maxLength: 20, default: 'Active' },
    { field: 'schedule', type: 'text', label: 'Schedule', maxLength: 200, default: '' },
    { field: 'nextRunAt', type: 'datetime', label: 'Next post', nullable: true, default: null },
    { field: 'lastSentAt', type: 'datetime', label: 'Last posted', nullable: true, default: null },
    { field: 'timezone', type: 'text', label: 'Time zone', maxLength: 64, default: 'UTC' },
  ]),
  actions: Object.freeze([
    {
      id: 'test',
      label: 'Send a test',
      description: 'Posts the reminder once now as a preview. Roles are not pinged and the schedule is not touched.',
      fields: [
        {
          field: 'channelId',
          type: 'channel',
          label: 'Channel',
          description: 'Where to post the test. Empty: the reminder\'s channel.',
          channelTypes: [...POSTABLE_CHANNEL_KINDS],
          nullable: true,
          default: null,
        },
      ],
    },
  ]),
  check: checkReminder,
});

/**
 * What each mode needs (the full /reminder schedule rules run in the store: validateReminderInput).
 * @returns {null|{ path: string, message: string }}
 */
export function checkReminder(data, { mode, current } = {}) {
  const r = mode === 'update' && current ? { ...current, ...data } : data;
  if (r.mode === 'TIME_OF_DAY' && !r.timeOfDay) return { path: 'timeOfDay', message: 'Pick the time of day.' };
  if (r.mode === 'INTERVAL' && !Number.isInteger(r.intervalMinutes)) return { path: 'intervalMinutes', message: 'Set how often it repeats.' };
  if (r.mode === 'ONE_SHOT' && !r.runAt) return { path: 'runAt', message: 'Pick the date and time.' };
  if (r.mode === 'INTERVAL' && r.windowStart && r.windowStart === r.windowEnd) {
    return { path: 'windowEnd', message: "The daily start and stop can't be the same time." };
  }
  return null;
}

const CHOICE = Object.freeze({ TIME_OF_DAY: 'time', INTERVAL: 'interval', ONE_SHOT: 'once' });

/**
 * The form's schedule as the raw /reminder options validateReminderInput reads. Only the options of the chosen mode
 * are passed (the slash command refuses the others; the form always carries every field).
 * @param {{ mode: string, timeOfDay?: string|null, weekdays?: string[], intervalMinutes?: number|null,
 *   windowStart?: string|null, windowEnd?: string|null, runAt?: string|null, requireActivity?: boolean }} s form values
 * @returns {object} the slash-option strings (`at` is a Discord timestamp, an exact instant)
 */
export function scheduleInput(s) {
  const input = { mode: CHOICE[s.mode] };
  if (s.mode !== 'ONE_SHOT') {
    const days = weekdayNumbers(s.weekdays);
    input.weekdays = days.length ? days.join(',') : 'all';
    input.requireActivity = s.requireActivity === true;
  }
  if (s.mode === 'TIME_OF_DAY') input.time = s.timeOfDay ?? undefined;
  if (s.mode === 'INTERVAL') {
    input.interval = Number.isInteger(s.intervalMinutes) ? String(s.intervalMinutes) : undefined;
    input.windowStart = s.windowStart ?? 'none';
    input.windowEnd = s.windowEnd ?? 'none';
  }
  if (s.mode === 'ONE_SHOT' && s.runAt) input.at = `<t:${Math.floor(Date.parse(s.runAt) / 1000)}>`; // an exact instant
  return input;
}

// ── Items ────────────────────────────────────────────────────────────────────

/** Stored day numbers (0 = Sunday) -> option values, in the options' order. */
export function weekdayNames(days) {
  const set = new Set((Array.isArray(days) ? days : []).filter((d) => Number.isInteger(d) && d >= 0 && d <= 6));
  return WEEKDAY_OPTIONS.map((o) => o.value).filter((v) => set.has(WEEKDAY_NAMES.indexOf(v)));
}

/** Option values -> stored day numbers, sorted. */
export function weekdayNumbers(names) {
  return [...new Set((Array.isArray(names) ? names : []).map((n) => WEEKDAY_NAMES.indexOf(n)).filter((d) => d >= 0))].sort((a, b) => a - b);
}

function daysText(days) {
  const wd = [...new Set(days ?? [])].sort((a, b) => a - b);
  if (!wd.length || wd.length === 7) return 'every day';
  if (wd.join() === '1,2,3,4,5') return 'on weekdays';
  if (wd.join() === '0,6') return 'on weekends';
  return `on ${wd.map((d) => DAY_LABELS[WEEKDAY_NAMES[d]]).join(', ')}`;
}

/**
 * A one-line schedule for the list (describeSchedule without Discord timestamps).
 * @param {object} r a Reminder row
 * @param {string} tz
 */
export function scheduleText(r, tz) {
  switch (r.mode) {
    case 'TIME_OF_DAY':
      return `${daysText(r.weekdays).replace(/^./, (c) => c.toUpperCase())} at ${r.timeOfDay ?? '??:??'} (${tz})`;
    case 'INTERVAL': {
      let s = `Every ${formatDurationLong((r.intervalMinutes ?? 0) * 60_000)}`;
      if (r.windowStart || r.windowEnd) s += ` between ${r.windowStart ?? '00:00'} and ${r.windowEnd ?? '24:00'} (${tz})`;
      if (r.weekdays?.length && r.weekdays.length < 7) s += ` ${daysText(r.weekdays)}`;
      return s;
    }
    case 'ONE_SHOT':
      return r.runAt instanceof Date ? `Once at ${r.runAt.toISOString().slice(0, 16).replace('T', ' ')} UTC` : 'Once';
    default:
      return 'Unknown schedule';
  }
}

const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);

/** /reminder list's "done": a one-off that was sent and switched itself off. */
export const isDone = (r) => !r.enabled && r.mode === 'ONE_SHOT' && r.lastSentAt instanceof Date && r.runAt instanceof Date
  && r.lastSentAt >= r.runAt;

/**
 * A Reminder row as a collection item.
 * @param {object} row
 * @param {{ tz: string, nextRunAt?: Date|null }} extra the server's time zone and the computed next run
 */
export function reminderItem(row, { tz, nextRunAt = null }) {
  return {
    id: row.id,
    name: row.name,
    channelId: row.channelId,
    template: normalizeTemplate(row.template),
    mode: row.mode,
    timeOfDay: row.timeOfDay ?? null,
    weekdays: weekdayNames(row.weekdays),
    intervalMinutes: Number.isInteger(row.intervalMinutes) ? row.intervalMinutes : null,
    windowStart: row.windowStart ?? null,
    windowEnd: row.windowEnd ?? null,
    runAt: iso(row.runAt),
    requireActivity: row.requireActivity === true,
    pingRoleId: row.pingRoleId ?? null,
    enabled: row.enabled !== false,
    status: isDone(row) ? 'Sent' : row.enabled ? 'Active' : 'Paused',
    schedule: scheduleText(row, tz),
    nextRunAt: row.enabled ? iso(nextRunAt) : null,
    lastSentAt: iso(row.lastSentAt),
    timezone: tz,
  };
}
