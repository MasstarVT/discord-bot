// pkg-reminders tests (pkg-reminders.md §8 + the architect amendments): option validation, next-run maths (DST gap and
// overlap, windows, grids), the late-tolerance rule, the activity gate matrix, payload building (role pings), helpers, the
// /reminder payload, the plugin declaration. Offline by default. With TEST_DATABASE_URL (a migrated database) the DB cases
// also run: create/cap/name, the job sync, the fire handler against fakes (claim, gate, auto-pause), the channelDelete
// cleanup, the guildReady repair, the modal handler and the /reminder subcommands against fake interactions:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/reminders.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ChannelType, DiscordAPIError, PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const svc = await import('../src/services/reminderService.js');
const cmd = await import('../src/commands/admin/reminder.js');
const pluginMod = await import('../src/plugins/reminders.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { PERMISSION_LEVELS, LIMITS: GUILD_LIMITS } = await import('../src/config/constants.js');
const { buildCustomId, parseCustomId } = await import('../src/config/customIds.js');
const { getJob, cancelJobs } = await import('../src/services/scheduler.js');
const { updateSettings } = await import('../src/services/settingsService.js');

const {
  DEFAULT_CONTENT, DEFAULT_TEMPLATE, MODE_FROM_CHOICE, REMINDER_CHANNEL_TYPES, ReminderUserError, validateReminderInput,
  computeNextRun, jobRunAt, dueSlot, lateToleranceMs, activityAllowsSend, buildSendPayload, describeSchedule, summaryLines,
  isEmptyTemplate, hasUserTokens, reminderWarnings, jobKey, resolveTimeZone, findReminderByName, createReminderRow,
  syncReminderJob, pauseReminder, deleteReminder, autoDisable, disableRemindersForChannel, reconcileGuildReminders, fireReminder,
} = svc;

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const GUILD = '955015000000000001';
const GUILD2 = '955015000000000002';
const GUILD3 = '955015000000000003';
const GUILD4 = '955015000000000004';
const GUILD5 = '955015000000000005';
const GUILD6 = '955015000000000006';
const ALL_GUILDS = [GUILD, GUILD2, GUILD3, GUILD4, GUILD5, GUILD6];

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) {
    // The cascade removes Reminder and ScheduledJob rows. Settings rows are deleted only here (ensureGuildSettings memoises).
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: ALL_GUILDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

const D = (s) => new Date(s);
const iso = (d) => (d ? d.toISOString() : d);

/** A reminder as a plain object. */
function rem(over = {}) {
  return {
    id: 'r1', guildId: 'g', name: 'x', channelId: 'c', mode: 'TIME_OF_DAY', timeOfDay: '09:00', weekdays: [], intervalMinutes: null,
    windowStart: null, windowEnd: null, runAt: null, createdAt: D('2026-10-12T00:00:00Z'), requireActivity: false, pingRoleId: null,
    template: { content: 'hi', embed: null, buttons: [] }, enabled: true, lastSentAt: null, lastMessageId: null, ...over,
  };
}

// ── 1. validateReminderInput ─────────────────────────────────────────────────

const NOW = D('2026-10-12T10:00:00Z'); // a Monday
const V = (input, base = null, now = NOW) => validateReminderInput(input, { tz: 'UTC', now, base });

test('1.1 time mode needs time', () => {
  const r = V({ mode: 'time' });
  assert.equal(r.ok, false);
  assert.match(r.error, /needs `time`/);
});

test('1.2 options that do not apply are named', () => {
  const r = V({ mode: 'time', time: '09:00', interval: '2h' });
  assert.equal(r.ok, false);
  assert.match(r.error, /don't apply to \*\*Time of day\*\*/);
  assert.match(r.error, /`interval`/);
});

test('1.3-1.7 interval limits and parsing', () => {
  assert.equal(V({ mode: 'interval', interval: '3m' }).error, 'The shortest interval is 5 minutes.');
  assert.equal(V({ mode: 'interval', interval: '8d' }).error, 'The longest interval is 7 days.');
  assert.match(V({ mode: 'interval', interval: '1d', windowStart: '09:00' }).error, /under 24 hours/);
  assert.match(V({ mode: 'interval', interval: '2h', windowStart: '09:00', windowEnd: '09:00' }).error, /can't be the same time/);
  const ok = V({ mode: 'interval', interval: '90' });
  assert.equal(ok.ok, true);
  assert.equal(ok.fields.intervalMinutes, 90);
  assert.match(V({ mode: 'interval', interval: 'soon' }).error, /couldn't read that interval/);
  assert.match(V({ mode: 'interval' }).error, /needs `interval`/);
});

test('1.8-1.11 once mode', () => {
  const ok = V({ mode: 'once', at: '2h' });
  assert.equal(ok.ok, true);
  assert.equal(iso(ok.fields.runAt), '2026-10-12T12:00:00.000Z');
  assert.match(V({ mode: 'once', at: '2026-01-01 09:00' }).error, /has passed/);
  assert.equal(V({ mode: 'once', at: '2028-01-01 09:00' }).error, 'Pick a time within one year.');
  const bad = V({ mode: 'once', at: '2h', requireActivity: true });
  assert.match(bad.error, /require_activity/);
  assert.equal(V({ mode: 'once', at: '2h', requireActivity: false }).ok, true);
  assert.match(V({ mode: 'once', at: 'whenever' }).error, /couldn't read that time/);
  assert.match(V({ mode: 'once' }).error, /needs `at`/);
  assert.match(V({ mode: 'once', at: '2h', weekdays: 'mon' }).error, /`weekdays`/);
});

test('1.12-1.14 weekdays and time', () => {
  const a = V({ mode: 'time', time: '9:30pm', weekdays: 'weekdays' });
  assert.equal(a.ok, true);
  assert.equal(a.fields.timeOfDay, '21:30');
  assert.deepEqual(a.fields.weekdays, [1, 2, 3, 4, 5]);
  assert.deepEqual(V({ mode: 'time', time: '09:00', weekdays: 'all' }).fields.weekdays, []);
  assert.match(V({ mode: 'time', time: '09:00', weekdays: 'funday' }).error, /couldn't read those days/);
  assert.match(V({ mode: 'time', time: '25:00' }).error, /couldn't read that time of day/);
  assert.match(V({}).error, /Pick a `mode`/);
});

test('1.15 edit carry-over', () => {
  const base = rem({ mode: 'TIME_OF_DAY', timeOfDay: '09:00', weekdays: [1] });
  const r = V({ mode: 'interval', interval: '2h' }, base);
  assert.equal(r.ok, true);
  assert.equal(r.fields.timeOfDay, null);
  assert.deepEqual(r.fields.weekdays, [1]);
  assert.equal(r.fields.intervalMinutes, 120);
  // Same mode keeps what was not provided; a switch to once drops weekdays and activity.
  const keep = V({ time: '10:15' }, base);
  assert.equal(keep.ok, true);
  assert.deepEqual(keep.fields.weekdays, [1]);
  assert.equal(keep.fields.timeOfDay, '10:15');
  const once = V({ mode: 'once', at: '3h' }, rem({ requireActivity: true, weekdays: [2] }));
  assert.equal(once.ok, true);
  assert.deepEqual(once.fields.weekdays, []);
  assert.equal(once.fields.requireActivity, false);
  // Window words clear a carried window.
  const win = rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 60, windowStart: '09:00', windowEnd: '17:00' });
  const cleared = V({ windowStart: 'none', windowEnd: 'off' }, win);
  assert.equal(cleared.fields.windowStart, null);
  assert.equal(cleared.fields.windowEnd, null);
});

test('1.16 an expired one-shot is refused again on edit; a carried runAt is checked', () => {
  const past = rem({ mode: 'ONE_SHOT', timeOfDay: null, runAt: D('2026-10-12T09:00:00Z') });
  assert.match(V({ mode: 'once' }, past).error, /has passed/);
});

test('2.x never-fires regression and weekday/interval rules', () => {
  const fri = D('2026-10-16T10:00:00Z');
  assert.equal(V({ mode: 'interval', interval: '3d', weekdays: 'mon' }, null, fri).error, 'Weekdays can only be used with intervals of 1 day or less.');
  assert.equal(V({ mode: 'interval', interval: '7d', weekdays: 'tue' }, null, fri).error, 'Weekdays can only be used with intervals of 1 day or less.');
  assert.equal(V({ mode: 'interval', interval: '1d', weekdays: 'mon' }, null, fri).ok, true);
  assert.equal(MODE_FROM_CHOICE.once, 'ONE_SHOT');
});

// ── 2. computeNextRun: TIME_OF_DAY ───────────────────────────────────────────

test('2.1 time of day on a weekday', () => {
  const r = rem({ timeOfDay: '09:00', weekdays: [1] });
  assert.equal(iso(computeNextRun(r, 'UTC', D('2026-10-13T10:00:00Z'))), '2026-10-19T09:00:00.000Z');
});

test('2.2 DST gap: the first valid instant after the missing time', () => {
  const r = rem({ timeOfDay: '01:30' });
  const t = computeNextRun(r, 'Europe/London', D('2026-03-28T12:00:00Z'));
  assert.ok(t >= D('2026-03-29T01:00:00Z') && t <= D('2026-03-29T01:30:00Z'), iso(t));
  assert.equal(iso(computeNextRun(r, 'Europe/London', t)), '2026-03-30T00:30:00.000Z');
});

test('2.3 DST overlap: the earlier instant only (never twice)', () => {
  const r = rem({ timeOfDay: '01:30' });
  const t = computeNextRun(r, 'Europe/London', D('2026-10-24T12:00:00Z'));
  assert.equal(iso(t), '2026-10-25T00:30:00.000Z');
  assert.equal(iso(computeNextRun(r, 'Europe/London', t)), '2026-10-26T01:30:00.000Z');
});

// ── 3. computeNextRun: INTERVAL ──────────────────────────────────────────────

test('3.1 interval grid anchored on createdAt', () => {
  const r = rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 120, createdAt: D('2026-10-12T00:00:00Z') });
  assert.equal(iso(computeNextRun(r, 'UTC', D('2026-10-12T03:10:00Z'))), '2026-10-12T04:00:00.000Z');
});

test('3.2 interval with a daily window in a timezone', () => {
  const r = rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 180, windowStart: '09:00', windowEnd: '17:00' });
  assert.equal(iso(computeNextRun(r, 'Europe/Berlin', D('2026-10-12T07:10:00Z'))), '2026-10-12T10:00:00.000Z');
  assert.equal(iso(computeNextRun(r, 'Europe/Berlin', D('2026-10-12T15:30:00Z'))), '2026-10-13T07:00:00.000Z');
});

test('3.3 a window that crosses midnight', () => {
  const r = rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 60, windowStart: '22:00', windowEnd: '02:00' });
  assert.equal(iso(computeNextRun(r, 'UTC', D('2026-10-12T23:30:00Z'))), '2026-10-13T00:00:00.000Z');
  assert.equal(iso(computeNextRun(r, 'UTC', D('2026-10-13T01:30:00Z'))), '2026-10-13T22:00:00.000Z');
});

test('3.4 a weekly interval on the wrong weekday never fires', () => {
  const r = rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 10_080, weekdays: [2], createdAt: D('2026-10-12T00:00:00Z') });
  assert.equal(computeNextRun(r, 'UTC', D('2026-10-12T10:00:00Z')), null);
});

test('3.x computeNextRun: one-shot and bad rows', () => {
  const r = rem({ mode: 'ONE_SHOT', timeOfDay: null, runAt: D('2026-10-12T12:00:00Z') });
  assert.equal(iso(computeNextRun(r, 'UTC', D('2026-10-12T11:00:00Z'))), '2026-10-12T12:00:00.000Z');
  assert.equal(computeNextRun(r, 'UTC', D('2026-10-12T12:00:00Z')), null);
  assert.equal(computeNextRun(rem({ mode: 'INTERVAL', intervalMinutes: 3 }), 'UTC'), null);
  assert.equal(computeNextRun(rem({ mode: 'TIME_OF_DAY', timeOfDay: 'xx' }), 'UTC'), null);
  assert.equal(computeNextRun(rem({ mode: 'NOPE' }), 'UTC'), null);
});

// ── 4. dueSlot / 5. jobRunAt ─────────────────────────────────────────────────

test('4.1 dueSlot TIME_OF_DAY tolerance of 15 minutes', () => {
  const r = rem({ timeOfDay: '09:00' });
  assert.equal(iso(dueSlot(r, 'UTC', D('2026-10-12T09:00:04Z'))), '2026-10-12T09:00:00.000Z');
  assert.equal(iso(dueSlot(r, 'UTC', D('2026-10-12T09:14:59Z'))), '2026-10-12T09:00:00.000Z');
  assert.equal(dueSlot(r, 'UTC', D('2026-10-12T09:15:01Z')), null);
  assert.equal(dueSlot(r, 'UTC', D('2026-10-12T08:59:58Z')), null);
});

test('4.2 dueSlot INTERVAL tolerance is half the interval at most', () => {
  const r = rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 5, createdAt: D('2026-10-12T00:00:00Z') });
  assert.equal(iso(dueSlot(r, 'UTC', D('2026-10-12T00:05:10Z'))), '2026-10-12T00:05:00.000Z');
  assert.equal(dueSlot(r, 'UTC', D('2026-10-12T00:07:40Z')), null);
  assert.equal(lateToleranceMs(r), 150_000);
  assert.equal(lateToleranceMs(rem({ mode: 'INTERVAL', intervalMinutes: 600 })), 15 * 60_000);
});

test('4.3 dueSlot ONE_SHOT tolerance of 60 minutes', () => {
  const r = rem({ mode: 'ONE_SHOT', timeOfDay: null, runAt: D('2026-10-12T12:00:00Z') });
  assert.equal(iso(dueSlot(r, 'UTC', D('2026-10-12T12:59:00Z'))), '2026-10-12T12:00:00.000Z');
  assert.equal(dueSlot(r, 'UTC', D('2026-10-12T13:01:00Z')), null);
  assert.equal(dueSlot(r, 'UTC', D('2026-10-12T11:59:00Z')), null);
});

test('5 jobRunAt lets a recent one-shot run immediately and drops an old one', () => {
  const now = D('2026-10-12T12:00:00Z');
  assert.equal(jobRunAt(rem({ mode: 'ONE_SHOT', runAt: new Date(now.getTime() - 30 * 60_000) }), 'UTC', now).getTime(), now.getTime() - 30 * 60_000);
  assert.equal(jobRunAt(rem({ mode: 'ONE_SHOT', runAt: new Date(now.getTime() - 61 * 60_000) }), 'UTC', now), null);
  assert.equal(jobRunAt(rem({ mode: 'ONE_SHOT', runAt: null }), 'UTC', now), null);
});

// ── 6. activity gate ─────────────────────────────────────────────────────────

test('6 activityAllowsSend matrix', () => {
  const base = { requireActivity: true, lastMessageId: '100' };
  assert.equal(activityAllowsSend({ ...base, requireActivity: false }), true);
  assert.equal(activityAllowsSend({ ...base, lastMessageId: null }), true);
  assert.equal(activityAllowsSend({ ...base, channelLastMessageId: '100' }), false);
  assert.equal(activityAllowsSend({ ...base, recent: [{ bot: true }], cachedLast: { bot: true } }), false);
  assert.equal(activityAllowsSend({ ...base, recent: [{ bot: true }, { bot: false }] }), true);
  assert.equal(activityAllowsSend({ ...base, recent: [{ bot: false, system: true }] }), false);
  assert.equal(activityAllowsSend({ ...base, recent: null, cachedLast: null }), true);
  assert.equal(activityAllowsSend({ ...base, recent: null, cachedLast: { bot: true } }), false);
  assert.equal(activityAllowsSend({ ...base, recent: null, cachedLast: { bot: false } }), true);
});

// ── 7. buildSendPayload ──────────────────────────────────────────────────────

function fakeGuild(roles = [{ id: '42', mentionable: true }]) {
  const guild = {
    id: 'g', name: 'Guild', memberCount: 5, ownerId: '1', iconURL: () => null,
    roles: { cache: new Map(roles.map((r) => [r.id, r])) },
    members: { me: { id: 'bot' } },
  };
  return guild;
}
function fakeChannel(guild, botCanMentionAll = false) {
  return {
    id: 'c', name: 'general', type: 0, guild,
    permissionsFor: () => new PermissionsBitField(botCanMentionAll ? [PermissionFlagsBits.MentionEveryone] : []),
  };
}

test('7.1 content variables, ping prefix and allowedMentions', () => {
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const p = buildSendPayload(rem({ template: { content: 'Hi {server} in {channel} {user}' }, pingRoleId: '42' }),
    { guild, channel, settings: { embedColor: null } });
  assert.equal(p.content, '<@&42> Hi Guild in <#c> {user}');
  assert.deepEqual(p.allowedMentions, { parse: ['users'], roles: ['42'] });
});

test('7.2 a test send shows the role but pings nobody', () => {
  const guild = fakeGuild();
  const p = buildSendPayload(rem({ template: { content: 'Hi' }, pingRoleId: '42' }),
    { guild, channel: fakeChannel(guild), settings: null, ping: false });
  assert.ok(p.content.startsWith('<@&42>'));
  assert.deepEqual(p.allowedMentions, { parse: [] });
});

test('7.3 the ping prefix survives the 2000-character clamp', () => {
  const guild = fakeGuild();
  const p = buildSendPayload(rem({ template: { content: 'x'.repeat(2000) }, pingRoleId: '42' }),
    { guild, channel: fakeChannel(guild), settings: null });
  assert.ok(p.content.length <= 2000);
  assert.ok(p.content.startsWith('<@&42> '));
});

test('7.4 an empty template falls back to the MEE6 default text', () => {
  const guild = fakeGuild();
  const p = buildSendPayload(rem({ template: { content: null, embed: null } }), { guild, channel: fakeChannel(guild), settings: null });
  assert.equal(p.content, "Hey, I'm a reminder!");
  assert.equal(DEFAULT_CONTENT, "Hey, I'm a reminder!");
});

test('7.5 @everyone in the text never pings', () => {
  const guild = fakeGuild();
  const p = buildSendPayload(rem({ template: { content: '@everyone hi' } }), { guild, channel: fakeChannel(guild), settings: null });
  assert.ok(!p.allowedMentions.parse.includes('everyone'));
  assert.deepEqual(p.allowedMentions, { parse: ['users'], roles: [] });
});

test('7.6 a role that stopped being pingable is not pinged (the reminder still sends)', () => {
  const guild = fakeGuild([{ id: '42', mentionable: false }]);
  const noPing = buildSendPayload(rem({ template: { content: 'Hi' }, pingRoleId: '42' }), { guild, channel: fakeChannel(guild, false), settings: null });
  assert.deepEqual(noPing.allowedMentions, { parse: ['users'], roles: [] });
  assert.ok(noPing.content.startsWith('<@&42> '));
  const canPing = buildSendPayload(rem({ template: { content: 'Hi' }, pingRoleId: '42' }), { guild, channel: fakeChannel(guild, true), settings: null });
  assert.deepEqual(canPing.allowedMentions, { parse: ['users'], roles: ['42'] });
  const gone = buildSendPayload(rem({ template: { content: 'Hi' }, pingRoleId: '77' }), { guild, channel: fakeChannel(guild, true), settings: null });
  assert.deepEqual(gone.allowedMentions.roles, []);
});

test('7.7 an embed template uses the guild colour', () => {
  const guild = fakeGuild();
  const p = buildSendPayload(rem({ template: { content: null, embed: { title: 'T {server}', description: 'd' } } }),
    { guild, channel: fakeChannel(guild), settings: { embedColor: 0x123456 } });
  assert.equal(p.embeds.length, 1);
  assert.equal(p.embeds[0].data.title, 'T Guild');
  assert.equal(p.embeds[0].data.color, 0x123456);
});

// ── 8. helpers ───────────────────────────────────────────────────────────────

test('8 helpers', () => {
  assert.equal(isEmptyTemplate({ content: '  ', embed: { title: null } }), true);
  assert.equal(isEmptyTemplate({ content: 'a' }), false);
  assert.equal(isEmptyTemplate({ content: null, embed: { color: 5 } }), true);
  assert.equal(isEmptyTemplate({ content: null, embed: { description: 'd' } }), false);
  assert.equal(hasUserTokens({ content: 'hey {user.name}' }), true);
  assert.equal(hasUserTokens({ content: '{server}' }), false);
  assert.equal(hasUserTokens({ content: null, embed: { footer: { text: 'bye {user}' } } }), true);
  assert.equal(describeSchedule(rem({ weekdays: [1, 2, 3, 4, 5], timeOfDay: '09:00' })), 'Weekdays at 09:00');
  assert.equal(describeSchedule(rem({ weekdays: [], timeOfDay: '09:00' })), 'Every day at 09:00');
  assert.equal(describeSchedule(rem({ weekdays: [1, 3, 5], timeOfDay: '09:00' })), 'Mon, Wed, Fri at 09:00');
  assert.equal(describeSchedule(rem({ weekdays: [0, 6], timeOfDay: '09:00' })), 'Weekends at 09:00');
  const iv = describeSchedule(rem({ mode: 'INTERVAL', intervalMinutes: 120, windowStart: '09:00', windowEnd: '17:00' }));
  assert.ok(iv.startsWith('Every '));
  assert.ok(iv.includes('between 09:00 and 17:00'));
  assert.equal(describeSchedule(rem({ mode: 'INTERVAL', intervalMinutes: 60, weekdays: [1, 2, 3, 4, 5] })), 'Every 1 hour on weekdays');
  assert.equal(describeSchedule(rem({ mode: 'INTERVAL', intervalMinutes: 60, weekdays: [1, 3] })), 'Every 1 hour on Mon, Wed');
  assert.match(describeSchedule(rem({ mode: 'ONE_SHOT', runAt: D('2026-10-12T12:00:00Z') })), /^Once at <t:\d+:f>$/);
  assert.equal(jobKey('abc'), 'reminder:abc');
  const id = `c${'x'.repeat(24)}`;
  assert.deepEqual(parseCustomId(buildCustomId('reminder-modal', id)), { prefix: 'reminder-modal', params: [id] });
  assert.equal(resolveTimeZone('Nope/Zone'), 'UTC');
  assert.equal(resolveTimeZone('Europe/London'), 'Europe/London');
  assert.deepEqual(REMINDER_CHANNEL_TYPES, [ChannelType.GuildText, ChannelType.GuildAnnouncement]);
  assert.deepEqual({ ...DEFAULT_TEMPLATE }, { content: "Hey, I'm a reminder!", embed: null, buttons: [] });
  assert.equal(MODE_FROM_CHOICE.time, 'TIME_OF_DAY');
});

test('8.x summaryLines and reminderWarnings', () => {
  const r = rem({ requireActivity: true, pingRoleId: '42', channelId: 'c' });
  const on = { timezone: 'Europe/Paris', remindersEnabled: true };
  const lines = summaryLines(r, on, D('2026-10-12T12:00:00Z'), ['WARN']);
  assert.equal(lines[0], '**x** → <#c>');
  assert.equal(lines[1], 'Every day at 09:00');
  assert.match(lines[2], /^Next: <t:\d+:F> \(<t:\d+:R>\)$/);
  assert.ok(lines.includes('Timezone: Europe/Paris (change with `/settings timezone`)'));
  assert.ok(lines.includes('💬 Skipped when nobody has chatted since the last reminder.'));
  assert.ok(lines.includes('🔔 Pings <@&42>'));
  assert.ok(lines.includes('Use `/reminder test name:x` to preview it.'));
  assert.ok(lines.includes('WARN'));
  assert.ok(!lines.some((l) => l.includes('module is off')));
  const off = summaryLines(r, { timezone: 'UTC', remindersEnabled: false }, null);
  assert.ok(off.includes('Next: —'));
  assert.ok(off.at(-1).includes('/module enable reminders'));
  assert.ok(summaryLines(rem({ enabled: false }), on, null).some((l) => l.startsWith('⏸️ Paused')));
  // Settings unknown: no module-off line.
  assert.ok(!summaryLines(r, null, null).some((l) => l.includes('module is off')));

  const guild = fakeGuild();
  const channel = { ...fakeChannel(guild), permissionsFor: () => new PermissionsBitField([PermissionFlagsBits.ViewChannel]) };
  guild.channels = { cache: new Map([['c', channel]]) };
  const w = reminderWarnings(rem({ requireActivity: true, template: { content: 'hi {user}' } }), guild);
  assert.equal(w.length, 2);
  assert.match(w[0], /can't read message history in <#c>/);
  assert.match(w[1], /user variables/);
  assert.deepEqual(reminderWarnings(rem({ requireActivity: true }), { channels: { cache: new Map() } }), []); // channel not cached: skipped
  assert.deepEqual(reminderWarnings(rem(), null), []); // never throws
});

// ── 9. command payload ───────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('9 /reminder payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'reminder');
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.deepEqual(json.contexts, [0]);
  assert.equal(cmd.module, 'reminders');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(typeof cmd.execute, 'function');
  assert.equal(typeof cmd.autocomplete, 'function');
  assert.deepEqual(json.options.map((o) => o.name), ['create', 'edit', 'list', 'pause', 'resume', 'delete', 'test', 'next']);
  const sub = (n) => json.options.find((o) => o.name === n);
  assert.deepEqual(sub('create').options.map((o) => o.name),
    ['name', 'channel', 'mode', 'time', 'weekdays', 'interval', 'window_start', 'window_end', 'at', 'require_activity', 'ping_role', 'message']);
  assert.deepEqual(sub('edit').options.map((o) => o.name),
    ['name', 'channel', 'mode', 'time', 'weekdays', 'interval', 'window_start', 'window_end', 'at', 'require_activity', 'ping_role', 'clear_ping_role', 'message']);
  for (const n of ['edit', 'pause', 'resume', 'delete', 'test', 'next']) {
    assert.equal(sub(n).options.find((o) => o.name === 'name').autocomplete, true, n);
  }
  assert.ok(!sub('create').options.find((o) => o.name === 'name').autocomplete);
  assert.deepEqual(sub('list').options ?? [], []);
  assert.deepEqual(sub('create').options.find((o) => o.name === 'channel').channel_types, [0, 5]);
  assert.ok(charCount(json) <= 2000, `payload is ${charCount(json)} characters`);
});

// ── 10. plugin ───────────────────────────────────────────────────────────────

test('10 plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('reminders')), []);
  assert.ok(plugins.some((p) => p.id === 'reminders'));
  const { plugin } = pluginMod;
  assert.equal(plugin.id, 'reminders');
  assert.equal(plugin.module, 'reminders');
  assert.equal(plugin.components.length, 1);
  assert.equal(plugin.components[0].type, 'modal');
  assert.equal(plugin.components[0].prefix, 'reminder-modal');
  assert.equal(plugin.components[0].module, null);
  assert.equal(typeof plugin.components[0].handler, 'function');
  assert.equal(plugin.jobs[0].type, 'reminder.fire');
  const cd = plugin.hooks.find((h) => h.event === 'channelDelete');
  assert.equal(cd.id, 'reminders');
  assert.equal(cd.always, true);
  assert.equal(cd.filter({ channel: { type: ChannelType.GuildText } }), true);
  assert.equal(cd.filter({ channel: { type: ChannelType.GuildAnnouncement } }), true);
  assert.equal(cd.filter({ channel: { type: ChannelType.GuildVoice } }), false);
  assert.equal(cd.filter({ channel: { type: ChannelType.PublicThread } }), false);
  const gr = plugin.hooks.find((h) => h.event === 'guildReady');
  assert.equal(gr.id, 'reminders');
  assert.equal(gr.always, true);
});

test('10.x the old customId and handler names stay in the registry', () => {
  assert.equal(typeof pluginMod.handleReminderModal, 'function');
  assert.equal(GUILD_LIMITS.REMINDERS, 100);
  assert.ok(new ReminderUserError('T', 'm') instanceof Error);
});

// ═══ DB cases ════════════════════════════════════════════════════════════════

const BOT = '900000000000000099';
const ADMIN = '900000000000000010';
const bits = (...names) => new PermissionsBitField(names.map((n) => PermissionFlagsBits[n]));
const BOT_PERMS = ['ViewChannel', 'SendMessages', 'EmbedLinks', 'ReadMessageHistory'];
const apiError = (code, message = 'x') => new DiscordAPIError({ message, code }, code, 400, 'POST', 'https://discord.test/api', {});

function makeGuild({ id = GUILD, roles = [], noMe = false } = {}) {
  return {
    id, name: 'Test Guild', available: true, ownerId: ADMIN, memberCount: 5, iconURL: () => null,
    roles: { cache: new Map(roles.map((r) => [r.id, r])) },
    members: { me: noMe ? null : { id: BOT } },
    channels: { cache: new Map(), fetch: async () => { throw apiError(10003, 'unknown channel'); } },
  };
}

let msgSeq = 0;
function makeChannel(guild, { id, type = ChannelType.GuildText, perms = BOT_PERMS, name = 'general', lastMessageId = null, history = [], onSend = null } = {}) {
  const ch = {
    id, name, type, guild, lastMessageId, lastMessage: null, sent: [],
    isTextBased: () => true,
    isThread: () => false,
    permissionsFor: (m) => (m?.id === BOT ? bits(...perms) : bits(...(m?.chanPerms ?? ['ViewChannel']))),
    messages: { fetch: async () => new Map(history.map((m, i) => [String(i), m])) },
    async send(payload) {
      if (onSend) await onSend(payload);
      ch.sent.push(payload);
      return { id: `9${String(++msgSeq).padStart(8, '0')}` };
    },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function makeMember(guild, { id = ADMIN, perms = ['ManageGuild'], chanPerms = [] } = {}) {
  return { id, guild, permissions: bits(...perms), roles: { cache: new Map() }, chanPerms };
}

function makeInteraction({ guild, sub, opts = {}, channel = null, member = null }) {
  const calls = { reply: [], editReply: [], followUp: [], showModal: [], deferReply: [] };
  const get = (name) => (Object.hasOwn(opts, name) && opts[name] !== undefined ? opts[name] : null);
  const i = {
    guild, guildId: guild.id, channel, user: { id: ADMIN }, member: member ?? makeMember(guild),
    createdTimestamp: Date.now(), deferred: false, replied: false, ephemeral: null, calls,
    inCachedGuild: () => true,
    inGuild: () => true,
    options: {
      getSubcommand: () => sub,
      getString: (n) => get(n),
      getBoolean: (n) => get(n),
      getChannel: (n) => get(n),
      getRole: (n) => get(n),
      data: [{ name: sub, options: Object.entries(opts).filter(([, v]) => v !== undefined).map(([name, value]) => ({ name, value })) }],
    },
    async deferReply(o) { i.deferred = true; i.ephemeral = Boolean(o?.ephemeral); calls.deferReply.push(o ?? {}); },
    async reply(p) { i.replied = true; calls.reply.push(p); return { resource: { message: null } }; },
    async editReply(p) { calls.editReply.push(p); return {}; },
    async followUp(p) { calls.followUp.push(p); return {}; },
    async showModal(m) { i.replied = true; calls.showModal.push(m); },
  };
  return i;
}

const lastOut = (i) => i.calls.editReply.at(-1) ?? i.calls.reply.at(-1) ?? i.calls.followUp.at(-1);
const lastEmbed = (i) => lastOut(i)?.embeds?.[0]?.data ?? {};
const lastText = (i) => `${lastEmbed(i).title ?? ''}\n${lastEmbed(i).description ?? ''}`;

async function ensureGuild(guildId) {
  await prisma.guildSettings.upsert({ where: { guildId }, update: {}, create: { guildId } });
}

let seq = 0;
async function mkRow(guildId, channelId, over = {}) {
  seq += 1;
  return prisma.reminder.create({
    data: {
      guildId, name: `rem-${seq}`, channelId, template: { content: 'hi', embed: null, buttons: [] }, mode: 'INTERVAL', intervalMinutes: 5,
      weekdays: [], createdBy: ADMIN, createdAt: new Date(Date.now() - 10 * 60_000 - 20_000), ...over, // grid slot 20 s ago
    },
  });
}

const fields = (over = {}) => ({
  mode: 'TIME_OF_DAY', timeOfDay: '09:00', weekdays: [], intervalMinutes: null, windowStart: null, windowEnd: null, runAt: null, requireActivity: false, ...over,
});

test('DB: createReminderRow, name normalisation, cap and unique name', { skip: skipDb }, async () => {
  await ensureGuild(GUILD);
  const row = await createReminderRow({
    guildId: GUILD, name: 'daily-standup', channelId: '1', template: { ...DEFAULT_TEMPLATE, buttons: [] }, fields: fields(), createdBy: ADMIN, now: D('2026-10-12T10:00:00Z'),
  });
  assert.equal(row.enabled, true);
  assert.equal(row.timeOfDay, '09:00');
  assert.equal(iso(row.createdAt), '2026-10-12T10:00:00.000Z'); // createdAt is the INTERVAL anchor, set explicitly
  assert.equal(row.pingRoleId, null);

  // Compound unique lookup with a normalised name.
  assert.equal((await findReminderByName(GUILD, 'Daily Standup'))?.id, row.id);
  assert.equal(await findReminderByName(GUILD2, 'daily-standup'), null);
  assert.equal(await findReminderByName(GUILD, '!!!'), null);

  await assert.rejects(
    createReminderRow({ guildId: GUILD, name: 'daily-standup', channelId: '1', template: {}, fields: fields(), createdBy: ADMIN, now: new Date() }),
    (e) => e instanceof ReminderUserError && e.title === 'Name Taken' && /already exists/.test(e.message),
  );

  // The cap counts every row (paused ones too).
  await ensureGuild(GUILD2);
  await prisma.reminder.createMany({
    data: Array.from({ length: GUILD_LIMITS.REMINDERS }, (_, k) => ({
      guildId: GUILD2, name: `cap-${k}`, channelId: '1', template: {}, mode: 'TIME_OF_DAY', timeOfDay: '09:00', createdBy: ADMIN, enabled: k % 2 === 0,
    })),
  });
  await assert.rejects(
    createReminderRow({ guildId: GUILD2, name: 'one-too-many', channelId: '1', template: {}, fields: fields(), createdBy: ADMIN, now: new Date() }),
    (e) => e instanceof ReminderUserError && e.title === 'Limit Reached',
  );
});

test('DB: syncReminderJob replaces, cancels, and pause/delete cancel the job', { skip: skipDb }, async () => {
  await ensureGuild(GUILD3);
  const now = D('2026-10-12T10:00:00Z');
  const r = await mkRow(GUILD3, '1', { mode: 'TIME_OF_DAY', timeOfDay: '12:00', intervalMinutes: null });
  const at = await syncReminderJob(r, { timezone: 'UTC' }, { now });
  assert.equal(iso(at), '2026-10-12T12:00:00.000Z');
  const job = await getJob(jobKey(r.id));
  assert.equal(job.type, 'reminder.fire');
  assert.deepEqual(job.payload, { reminderId: r.id });
  assert.equal(job.maxAttempts, 10);
  assert.equal(job.status, 'PENDING');

  // A second sync (a new time) replaces the same row and bumps the version.
  const r2 = await prisma.reminder.update({ where: { id: r.id }, data: { timeOfDay: '15:30' } });
  await syncReminderJob(r2, { timezone: 'UTC' }, { now });
  const job2 = await getJob(jobKey(r.id));
  assert.equal(job2.id, job.id);
  assert.equal(job2.version, job.version + 1);
  assert.equal(iso(job2.runAt), '2026-10-12T15:30:00.000Z');

  // The guild timezone is used.
  await syncReminderJob(r2, { timezone: 'Asia/Tokyo' }, { now }); // 15:30 JST = 06:30Z the same day, already past at 10:00Z: tomorrow
  assert.equal(iso((await getJob(jobKey(r.id))).runAt), '2026-10-13T06:30:00.000Z');

  await pauseReminder(r2);
  assert.equal((await prisma.reminder.findUnique({ where: { id: r.id } })).enabled, false);
  assert.equal(await getJob(jobKey(r.id)), null);

  // Disabled rows and rows with nothing to schedule cancel and return null.
  assert.equal(await syncReminderJob({ ...r2, enabled: false }, null), null);
  const stale = await mkRow(GUILD3, '1', { mode: 'ONE_SHOT', runAt: new Date(Date.now() - 2 * 3_600_000), intervalMinutes: null });
  assert.equal(await syncReminderJob(stale, null), null);
  assert.equal(await getJob(jobKey(stale.id)), null);

  // delete cancels the job and is guild-scoped.
  const d = await mkRow(GUILD3, '1', { mode: 'TIME_OF_DAY', timeOfDay: '12:00', intervalMinutes: null });
  await syncReminderJob(d, null);
  await deleteReminder({ ...d, guildId: GUILD4 }); // wrong guild: nothing deleted
  assert.ok(await prisma.reminder.findUnique({ where: { id: d.id } }));
  await deleteReminder(d);
  assert.equal(await prisma.reminder.findUnique({ where: { id: d.id } }), null);
  assert.equal(await getJob(jobKey(d.id)), null);
});

test('DB: autoDisable pauses once and logs once', { skip: skipDb }, async () => {
  await ensureGuild(GUILD3);
  const r = await mkRow(GUILD3, '1');
  const warnings = [];
  const original = console.warn;
  console.warn = (...a) => warnings.push(a.join(' '));
  try {
    await autoDisable(r, 'because');
    await autoDisable(r, 'because');
  } finally {
    console.warn = original;
  }
  assert.equal((await prisma.reminder.findUnique({ where: { id: r.id } })).enabled, false);
  assert.equal(warnings.filter((w) => w.includes('because')).length, 1);
});

test('DB: disableRemindersForChannel pauses the channel\'s reminders and cancels their jobs', { skip: skipDb }, async () => {
  await ensureGuild(GUILD4);
  const a = await mkRow(GUILD4, 'chan-a');
  const b = await mkRow(GUILD4, 'chan-a');
  const c = await mkRow(GUILD4, 'chan-b');
  const paused = await mkRow(GUILD4, 'chan-a', { enabled: false });
  for (const r of [a, b, c, paused]) await syncReminderJob({ ...r, enabled: true }, null);
  assert.equal(await disableRemindersForChannel(GUILD4, 'chan-a'), 2);
  assert.equal(await disableRemindersForChannel(GUILD4, 'chan-a'), 0);
  const rows = Object.fromEntries((await prisma.reminder.findMany({ where: { guildId: GUILD4 } })).map((r) => [r.id, r.enabled]));
  assert.deepEqual([rows[a.id], rows[b.id], rows[c.id], rows[paused.id]], [false, false, true, false]);
  assert.equal(await getJob(jobKey(a.id)), null);
  assert.equal(await getJob(jobKey(b.id)), null);
  assert.ok(await getJob(jobKey(c.id)));
});

test('DB: reconcileGuildReminders repairs missing jobs and pauses finished ones', { skip: skipDb }, async () => {
  await ensureGuild(GUILD5);
  const lost = await mkRow(GUILD5, '1');
  const healthy = await mkRow(GUILD5, '1');
  const missed = await mkRow(GUILD5, '1', { mode: 'ONE_SHOT', runAt: new Date(Date.now() - 3 * 3_600_000), intervalMinutes: null });
  const never = await mkRow(GUILD5, '1', { mode: 'INTERVAL', intervalMinutes: 10_080, weekdays: [(new Date().getUTCDay() + 3) % 7] });
  await syncReminderJob(healthy, null);
  const before = await getJob(jobKey(healthy.id));
  await reconcileGuildReminders({ id: GUILD5 });
  assert.ok(await getJob(jobKey(lost.id)));
  assert.equal((await getJob(jobKey(healthy.id))).version, before.version); // untouched
  assert.equal((await prisma.reminder.findUnique({ where: { id: missed.id } })).enabled, false);
  assert.equal((await prisma.reminder.findUnique({ where: { id: never.id } })).enabled, false);
  assert.equal((await prisma.reminder.findUnique({ where: { id: lost.id } })).enabled, true);
  // No rows: returns quietly.
  await reconcileGuildReminders({ id: GUILD6 });
});

// ── The fire handler against fakes ───────────────────────────────────────────

const CH = '955015000000000100';
const fire = (guild, r) => fireReminder({ guildId: guild.id, payload: { reminderId: r.id } }, { client: {}, guild });
const row = (r) => prisma.reminder.findUnique({ where: { id: r.id } });

test('DB: fireReminder sends a due recurring reminder once (idempotent claim)', { skip: skipDb }, async () => {
  await ensureGuild(GUILD);
  const guild = makeGuild({ id: GUILD });
  const ch = makeChannel(guild, { id: CH });
  const r = await mkRow(GUILD, CH);
  const res = await fire(guild, r);
  assert.equal(ch.sent.length, 1);
  assert.equal(ch.sent[0].content, 'hi');
  assert.ok(res.nextRunAt > new Date());
  assert.equal(res.nextRunAt.getTime(), r.createdAt.getTime() + 15 * 60_000); // the next grid slot
  const after1 = await row(r);
  assert.ok(after1.lastSentAt);
  assert.match(after1.lastMessageId, /^9\d+$/);
  // The same occurrence again (a lock expiry re-run) sends nothing.
  const res2 = await fire(guild, r);
  assert.equal(ch.sent.length, 1);
  assert.equal(res2.nextRunAt.getTime(), res.nextRunAt.getTime());
});

test('DB: fireReminder is not due yet / deleted / paused / malformed', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD });
  const ch = makeChannel(guild, { id: CH });
  const early = await mkRow(GUILD, CH, { createdAt: new Date(Date.now() - 4 * 60_000) }); // the last slot is outside the tolerance, the next in 1 minute
  const res = await fire(guild, early);
  assert.equal(ch.sent.length, 0);
  assert.equal(res.nextRunAt.getTime(), early.createdAt.getTime() + 5 * 60_000);
  const paused = await mkRow(GUILD, CH, { enabled: false });
  assert.equal(await fire(guild, paused), undefined);
  assert.equal(await fire(guild, { id: 'does-not-exist' }), undefined);
  assert.equal(await fireReminder({ guildId: GUILD, payload: {} }, { client: {}, guild }), undefined);
  assert.equal(ch.sent.length, 0);
});

test('DB: fireReminder activity gate', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD });
  // Nothing after our last message (decided from the cached channel state, no fetch).
  const quietCh = makeChannel(guild, { id: `${CH}1`, lastMessageId: '100' });
  const a = await mkRow(GUILD, quietCh.id, { requireActivity: true, lastMessageId: '100' });
  assert.ok((await fire(guild, a)).nextRunAt);
  assert.equal(quietCh.sent.length, 0);
  assert.equal((await row(a)).lastSentAt, null);
  // A human posted since.
  const busyCh = makeChannel(guild, { id: `${CH}2`, lastMessageId: '105', history: [{ author: { bot: false } }] });
  const b = await mkRow(GUILD, busyCh.id, { requireActivity: true, lastMessageId: '100' });
  await fire(guild, b);
  assert.equal(busyCh.sent.length, 1);
  assert.notEqual((await row(b)).lastMessageId, '100');
  // Only bots / webhooks / system messages since: skipped.
  const botCh = makeChannel(guild, { id: `${CH}3`, lastMessageId: '105', history: [{ author: { bot: true } }, { author: { bot: false }, webhookId: 'w' }, { author: { bot: false }, system: true }] });
  const c = await mkRow(GUILD, botCh.id, { requireActivity: true, lastMessageId: '100' });
  await fire(guild, c);
  assert.equal(botCh.sent.length, 0);
  // Never sent here before: the first one goes out.
  const freshCh = makeChannel(guild, { id: `${CH}4`, lastMessageId: '105' });
  const d = await mkRow(GUILD, freshCh.id, { requireActivity: true });
  await fire(guild, d);
  assert.equal(freshCh.sent.length, 1);
});

test('DB: fireReminder send failures revert the claim or pause the reminder', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD });
  let failWith = null;
  const ch = makeChannel(guild, { id: `${CH}5`, onSend: async () => { if (failWith) throw failWith; } });
  const a = await mkRow(GUILD, ch.id);
  failWith = Object.assign(new Error('boom'), { code: 50013 });
  const res = await fire(guild, a);
  assert.ok(res.nextRunAt); // keeps rescheduling
  const reverted = await row(a);
  assert.equal(reverted.lastSentAt, null); // the claim was given back
  assert.equal(reverted.enabled, true);
  failWith = null;
  await fire(guild, a); // the same occurrence can still be sent after the failure
  assert.equal(ch.sent.length, 1);

  const b = await mkRow(GUILD, ch.id);
  failWith = apiError(50035, 'Invalid Form Body');
  assert.equal(await fire(guild, b), undefined);
  assert.equal((await row(b)).enabled, false);

  const c = await mkRow(GUILD, ch.id);
  failWith = apiError(10003, 'Unknown Channel');
  assert.equal(await fire(guild, c), undefined);
  assert.equal((await row(c)).enabled, false);
});

test('DB: fireReminder pauses on a gone or wrong channel and never on a cache miss alone', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD });
  // Not cached and the REST fetch says Unknown Channel: paused.
  const gone = await mkRow(GUILD, 'not-cached');
  assert.equal(await fire(guild, gone), undefined);
  assert.equal((await row(gone)).enabled, false);
  // Not cached and the fetch fails for another reason: skipped, still enabled.
  guild.channels.fetch = async () => { throw new Error('socket hang up'); };
  const flaky = await mkRow(GUILD, 'not-cached-2');
  assert.ok((await fire(guild, flaky)).nextRunAt);
  assert.equal((await row(flaky)).enabled, true);
  // A fetched channel of the wrong type: paused.
  const voice = makeChannel(guild, { id: `${CH}6`, type: ChannelType.GuildVoice });
  const v = await mkRow(GUILD, voice.id);
  assert.equal(await fire(guild, v), undefined);
  assert.equal((await row(v)).enabled, false);
});

test('DB: fireReminder permissions: View Channel lost pauses, missing Send skips, unknown self never pauses', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD });
  const noView = makeChannel(guild, { id: `${CH}7`, perms: ['SendMessages'] });
  const a = await mkRow(GUILD, noView.id);
  assert.equal(await fire(guild, a), undefined);
  assert.equal((await row(a)).enabled, false);

  const noSend = makeChannel(guild, { id: `${CH}8`, perms: ['ViewChannel'] });
  const b = await mkRow(GUILD, noSend.id);
  assert.ok((await fire(guild, b)).nextRunAt);
  assert.equal((await row(b)).enabled, true);
  assert.equal(noSend.sent.length, 0);

  const blind = makeGuild({ id: GUILD, noMe: true });
  const ch = makeChannel(blind, { id: `${CH}9` });
  const c = await mkRow(GUILD, ch.id);
  assert.ok((await fire(blind, c)).nextRunAt); // `me` is not cached: never pause on that
  assert.equal((await row(c)).enabled, true);

  // An embed needs Embed Links.
  const noEmbed = makeChannel(guild, { id: `${CH}10`, perms: ['ViewChannel', 'SendMessages'] });
  const d = await mkRow(GUILD, noEmbed.id, { template: { content: null, embed: { title: 'T' }, buttons: [] } });
  assert.ok((await fire(guild, d)).nextRunAt);
  assert.equal(noEmbed.sent.length, 0);
});

test('DB: fireReminder one-shot: sends then finishes, future waits, stale is paused, module off pauses', { skip: skipDb }, async () => {
  await ensureGuild(GUILD);
  const guild = makeGuild({ id: GUILD });
  const ch = makeChannel(guild, { id: `${CH}11` });
  const due = await mkRow(GUILD, ch.id, { mode: 'ONE_SHOT', intervalMinutes: null, runAt: new Date(Date.now() - 5000) });
  assert.equal(await fire(guild, due), undefined);
  assert.equal(ch.sent.length, 1);
  const done = await row(due);
  assert.equal(done.enabled, false);
  assert.ok(done.lastSentAt >= done.runAt);

  const future = await mkRow(GUILD, ch.id, { mode: 'ONE_SHOT', intervalMinutes: null, runAt: new Date(Date.now() + 3_600_000) });
  const res = await fire(guild, future);
  assert.equal(res.nextRunAt.getTime(), future.runAt.getTime());
  assert.equal(ch.sent.length, 1);

  const stale = await mkRow(GUILD, ch.id, { mode: 'ONE_SHOT', intervalMinutes: null, runAt: new Date(Date.now() - 2 * 3_600_000) });
  assert.equal(await fire(guild, stale), undefined);
  assert.equal((await row(stale)).enabled, false);
  assert.equal(ch.sent.length, 1);

  await updateSettings(GUILD, { remindersEnabled: false });
  try {
    const off = await mkRow(GUILD, ch.id, { mode: 'ONE_SHOT', intervalMinutes: null, runAt: new Date(Date.now() - 5000) });
    assert.equal(await fire(guild, off), undefined);
    assert.equal((await row(off)).enabled, false);
    assert.equal(ch.sent.length, 1); // never sent late

    // Recurring ones keep rescheduling without sending.
    const rec = await mkRow(GUILD, ch.id);
    assert.ok((await fire(guild, rec)).nextRunAt);
    assert.equal(ch.sent.length, 1);
    assert.equal((await row(rec)).enabled, true);
  } finally {
    await updateSettings(GUILD, { remindersEnabled: true });
  }
});

test('DB: fireReminder pings only a still-pingable role', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD, roles: [{ id: '42', mentionable: true }, { id: '43', mentionable: false }] });
  const ch = makeChannel(guild, { id: `${CH}12` });
  await fire(guild, await mkRow(GUILD, ch.id, { pingRoleId: '42' }));
  await fire(guild, await mkRow(GUILD, ch.id, { pingRoleId: '43' }));
  assert.deepEqual(ch.sent[0].allowedMentions, { parse: ['users'], roles: ['42'] });
  assert.ok(ch.sent[0].content.startsWith('<@&42> '));
  assert.deepEqual(ch.sent[1].allowedMentions, { parse: ['users'], roles: [] });
  assert.ok(ch.sent[1].content.startsWith('<@&43> ')); // the reminder still sends
});

test('DB: scheduled TIME_OF_DAY reminders follow a timezone change (the stale job is skipped, the next one set)', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD });
  const ch = makeChannel(guild, { id: `${CH}13` });
  const nowUtc = new Date();
  const hh = String(nowUtc.getUTCHours()).padStart(2, '0');
  const mm = String(nowUtc.getUTCMinutes()).padStart(2, '0');
  // "now" in UTC, but the guild zone is Tokyo: 09:00 hours away, so nothing is due.
  const r = await mkRow(GUILD, ch.id, { mode: 'TIME_OF_DAY', timeOfDay: `${hh}:${mm}`, intervalMinutes: null });
  await updateSettings(GUILD, { timezone: 'Asia/Tokyo' });
  try {
    const res = await fire(guild, r);
    assert.equal(ch.sent.length, 0);
    assert.ok(res.nextRunAt > nowUtc);
  } finally {
    await updateSettings(GUILD, { timezone: 'UTC' });
  }
});

// ── The modal handler ────────────────────────────────────────────────────────

function makeModal({ guild, reminderId, values = {}, member = null }) {
  const i = makeInteraction({ guild, sub: 'x', member });
  i.customId = buildCustomId('reminder-modal', reminderId);
  i.fields = { getTextInputValue: (id) => { if (!Object.hasOwn(values, id)) throw new Error('missing'); return values[id]; } };
  return i;
}
const FORM = (o = {}) => ({ 'tpl-content': '', 'tpl-title': '', 'tpl-description': '', 'tpl-color': '', 'tpl-footer': '', ...o });

test('DB: modal handler saves, defaults an empty form, refuses non-admins and other guilds', { skip: skipDb }, async () => {
  await ensureGuild(GUILD);
  const guild = makeGuild({ id: GUILD });
  const ch = makeChannel(guild, { id: `${CH}14` });
  const r = await mkRow(GUILD, ch.id, { template: { content: 'old', embed: null, buttons: [] } });
  const { handleReminderModal } = pluginMod;

  // Saved.
  const ok = makeModal({ guild, reminderId: r.id, values: FORM({ 'tpl-content': 'Stand-up in {channel}! {user}', 'tpl-title': 'Daily', 'tpl-color': '#ff0000' }) });
  await handleReminderModal(ok);
  assert.equal(ok.calls.deferReply.length, 1);
  const saved = await row(r);
  assert.equal(saved.template.content, 'Stand-up in {channel}! {user}');
  assert.equal(saved.template.embed.title, 'Daily');
  assert.equal(saved.template.embed.color, 0xff0000);
  assert.equal(lastEmbed(ok).title, '✅  Reminder Saved');
  assert.match(lastEmbed(ok).description, /user variables/); // {user} warning
  assert.equal(saved.enabled, true); // the form never changes `enabled`
  assert.ok(await getJob(jobKey(r.id))); // re-synced

  // An empty form falls back to the MEE6 default text.
  const empty = makeModal({ guild, reminderId: r.id, values: FORM() });
  await handleReminderModal(empty);
  assert.equal((await row(r)).template.content, "Hey, I'm a reminder!");
  assert.equal((await row(r)).template.embed, null);

  // An unreadable colour is refused, nothing saved.
  const bad = makeModal({ guild, reminderId: r.id, values: FORM({ 'tpl-content': 'new text', 'tpl-color': 'zzz' }) });
  await handleReminderModal(bad);
  assert.match(lastText(bad), /Invalid Message/);
  assert.equal((await row(r)).template.content, "Hey, I'm a reminder!");

  // Too long: refused.
  const long = makeModal({ guild, reminderId: r.id, values: FORM({ 'tpl-description': 'd'.repeat(3000), 'tpl-footer': 'f'.repeat(2100) }) });
  await handleReminderModal(long);
  assert.match(lastText(long), /Nothing was saved/);

  // An embed needs Embed Links.
  const poor = makeChannel(guild, { id: `${CH}15`, perms: ['ViewChannel', 'SendMessages'] });
  const r2 = await mkRow(GUILD, poor.id);
  const emb = makeModal({ guild, reminderId: r2.id, values: FORM({ 'tpl-title': 'T' }) });
  await handleReminderModal(emb);
  assert.match(lastText(emb), /Embed Links/);

  // Not an admin.
  const rando = makeModal({ guild, reminderId: r.id, values: FORM({ 'tpl-content': 'hacked' }), member: makeMember(guild, { id: '123', perms: [] }) });
  await handleReminderModal(rando);
  assert.match(lastText(rando), /Admin level/);
  assert.notEqual((await row(r)).template.content, 'hacked');

  // The id of a reminder of another guild is not found.
  await ensureGuild(GUILD2);
  const foreign = await mkRow(GUILD2, '1');
  const cross = makeModal({ guild, reminderId: foreign.id, values: FORM({ 'tpl-content': 'cross' }) });
  await handleReminderModal(cross);
  assert.match(lastText(cross), /no longer exists/);
  assert.notEqual((await row(foreign)).template.content, 'cross');

  // A stale form (no id).
  const stale = makeModal({ guild, reminderId: '', values: FORM() });
  await handleReminderModal(stale);
  assert.match(lastText(stale), /expired/);
});

test('DB: a paused reminder saved through the form stays paused and unscheduled', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD });
  const ch = makeChannel(guild, { id: `${CH}16` });
  const r = await mkRow(GUILD, ch.id, { enabled: false });
  await pluginMod.handleReminderModal(makeModal({ guild, reminderId: r.id, values: FORM({ 'tpl-content': 'later' }) }));
  assert.equal((await row(r)).enabled, false);
  assert.equal(await getJob(jobKey(r.id)), null);
});

// ── /reminder against fake interactions ─────────────────────────────────────

const exec = (i) => cmd.execute(i, {});
async function waitJob(id, ms = 2000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    const j = await getJob(jobKey(id));
    if (j) return j;
    await new Promise((r) => setTimeout(r, 50));
  }
  return null;
}

test('DB: /reminder create (form path) saves first, schedules in the background and opens the form', { skip: skipDb }, async () => {
  await ensureGuild(GUILD);
  const guild = makeGuild({ id: GUILD });
  const ch = makeChannel(guild, { id: `${CH}20` });
  const i = makeInteraction({ guild, sub: 'create', opts: { name: 'Morning Brief', channel: { id: ch.id }, mode: 'time', time: '9am', weekdays: 'weekdays' } });
  await exec(i);
  assert.equal(i.calls.showModal.length, 1);
  assert.equal(i.calls.deferReply.length, 0); // the form is the first response
  const m = i.calls.showModal[0].toJSON();
  assert.equal(parseCustomId(m.custom_id).prefix, 'reminder-modal');
  assert.equal(m.title, 'Reminder: morning-brief');
  const saved = await prisma.reminder.findUnique({ where: { guildId_name: { guildId: GUILD, name: 'morning-brief' } } });
  assert.equal(saved.template.content, "Hey, I'm a reminder!"); // active with the MEE6 default until the form is submitted
  assert.deepEqual(saved.weekdays, [1, 2, 3, 4, 5]);
  assert.equal(saved.timeOfDay, '09:00');
  assert.equal(parseCustomId(m.custom_id).params[0], saved.id);
  assert.ok(await waitJob(saved.id), 'the job is scheduled in the background');

  // A duplicate name is reported (ephemeral error), no form.
  const dup = makeInteraction({ guild, sub: 'create', opts: { name: 'morning-brief', channel: { id: ch.id }, mode: 'time', time: '9am' } });
  await exec(dup);
  assert.equal(dup.calls.showModal.length, 0);
  assert.match(lastText(dup), /Name Taken/);
  assert.equal(dup.calls.reply.at(-1).ephemeral, true);
});

test('DB: /reminder create validation and permission refusals', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD, roles: [{ id: '50', mentionable: false }, { id: '51', mentionable: true }, { id: GUILD, mentionable: false }] });
  const ch = makeChannel(guild, { id: `${CH}21` });
  const base = { channel: { id: ch.id }, mode: 'time', time: '09:00' };
  const run = async (opts, over = {}) => {
    const i = makeInteraction({ guild, sub: 'create', opts: { name: 'val', ...base, ...opts }, ...over });
    await exec(i);
    return i;
  };
  assert.match(lastText(await run({ name: '!!!' })), /Invalid Name/);
  assert.match(lastText(await run({ interval: '2h' })), /Invalid Schedule[\s\S]*don't apply/);
  // A role the member cannot ping (not mentionable and no Mention Everyone).
  assert.match(lastText(await run({ ping_role: { id: '50' } })), /Cannot Ping Role[\s\S]*not mentionable/);
  assert.match(lastText(await run({ ping_role: { id: GUILD } })), /Invalid Role/);
  // Missing bot permissions in the channel.
  const poor = makeChannel(guild, { id: `${CH}22`, perms: ['ViewChannel'] });
  assert.match(lastText(await run({ channel: { id: poor.id } })), /Missing Permissions[\s\S]*Send Messages/);
  // A member with Mention Everyone may pick any role.
  const loud = await run({ name: 'loud', ping_role: { id: '50' }, message: 'Hi all' }, { member: makeMember(guild, { chanPerms: ['ViewChannel', 'MentionEveryone'] }) });
  assert.match(lastText(loud), /Reminder Created/);
  assert.equal((await prisma.reminder.findUnique({ where: { guildId_name: { guildId: GUILD, name: 'loud' } } })).pingRoleId, '50');
  // An empty message is refused.
  assert.match(lastText(await run({ name: 'empty', message: '   ' })), /Invalid Message/);
});

test('DB: /reminder create with a message is public, and warns about user variables', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD, roles: [{ id: '51', mentionable: true }] });
  const ch = makeChannel(guild, { id: `${CH}23`, perms: ['ViewChannel', 'SendMessages'] });
  const i = makeInteraction({
    guild, sub: 'create',
    opts: { name: 'ping-me', channel: { id: ch.id }, mode: 'interval', interval: '30m', window_start: '09:00', window_end: '17:00', ping_role: { id: '51' }, require_activity: true, message: 'Hi {user}' },
  });
  await exec(i);
  assert.equal(i.calls.deferReply.length, 1);
  assert.ok(!i.calls.deferReply[0].ephemeral);
  const text = lastText(i);
  assert.match(text, /Reminder Created/);
  assert.match(text, /Every 30 minutes between 09:00 and 17:00/);
  assert.match(text, /🔔 Pings <@&51>/);
  assert.match(text, /can't read message history/); // the bot lacks Read Message History in this channel
  assert.match(text, /user variables/);
  const saved = await prisma.reminder.findUnique({ where: { guildId_name: { guildId: GUILD, name: 'ping-me' } } });
  assert.equal(saved.requireActivity, true);
  assert.equal(saved.windowStart, '09:00');
  assert.ok(await getJob(jobKey(saved.id)));
});

test('DB: /reminder edit: form path, schedule path, channel switch and role removal', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD, roles: [{ id: '51', mentionable: true }] });
  const ch = makeChannel(guild, { id: `${CH}24` });
  const other = makeChannel(guild, { id: `${CH}25` });
  const r = await mkRow(GUILD, ch.id, { name: 'editable', mode: 'TIME_OF_DAY', timeOfDay: '09:00', intervalMinutes: null, weekdays: [1], lastMessageId: '77' });
  await syncReminderJob(r, null);

  const nameOnly = makeInteraction({ guild, sub: 'edit', opts: { name: 'editable' } });
  await exec(nameOnly);
  assert.equal(nameOnly.calls.showModal.length, 1);
  assert.equal(nameOnly.calls.showModal[0].toJSON().title, 'Reminder: editable');

  const missing = makeInteraction({ guild, sub: 'edit', opts: { name: 'nope', time: '10:00' } });
  await exec(missing);
  assert.match(lastText(missing), /Reminder Not Found[\s\S]*nope/);

  const bad = makeInteraction({ guild, sub: 'edit', opts: { name: 'editable', interval: '2h' } });
  await exec(bad);
  assert.match(lastText(bad), /Invalid Schedule/);

  const sched = makeInteraction({ guild, sub: 'edit', opts: { name: 'editable', time: '10:30', ping_role: { id: '51' }, message: 'New text' } });
  await exec(sched);
  assert.match(lastText(sched), /Reminder Updated/);
  let saved = await row(r);
  assert.equal(saved.timeOfDay, '10:30');
  assert.deepEqual(saved.weekdays, [1]); // carried over
  assert.equal(saved.pingRoleId, '51');
  assert.equal(saved.template.content, 'New text');
  assert.equal(saved.enabled, true);
  assert.ok((await getJob(jobKey(r.id))).runAt > new Date());

  const move = makeInteraction({ guild, sub: 'edit', opts: { name: 'editable', channel: { id: other.id }, clear_ping_role: true } });
  await exec(move);
  saved = await row(r);
  assert.equal(saved.channelId, other.id);
  assert.equal(saved.lastMessageId, null); // the activity baseline belonged to the old channel
  assert.equal(saved.pingRoleId, null);

  const both = makeInteraction({ guild, sub: 'edit', opts: { name: 'editable', ping_role: { id: '51' }, clear_ping_role: true } });
  await exec(both);
  assert.match(lastText(both), /not both/);

  // A ping role with the reminder's channel gone from the cache is refused with a clear message.
  guild.channels.cache.delete(other.id);
  const gone = makeInteraction({ guild, sub: 'edit', opts: { name: 'editable', ping_role: { id: '51' } } });
  await exec(gone);
  assert.match(lastText(gone), /Channel Missing/);
});

test('DB: /reminder pause, resume (and refresh), next and list', { skip: skipDb }, async () => {
  await ensureGuild(GUILD6);
  const guild = makeGuild({ id: GUILD6 });
  const ch = makeChannel(guild, { id: `${CH}30` });
  const r = await mkRow(GUILD6, ch.id, { name: 'cycle', mode: 'TIME_OF_DAY', timeOfDay: '09:00', intervalMinutes: null });
  await syncReminderJob(r, null);

  const run = async (sub, opts = {}) => {
    const i = makeInteraction({ guild, sub, opts: { name: 'cycle', ...opts } });
    await exec(i);
    return i;
  };

  const nx = await run('next');
  assert.match(lastText(nx), /Next run[\s\S]*Next: <t:\d+:F>[\s\S]*Upcoming: <t:\d+:f> · <t:\d+:f> · <t:\d+:f>/);
  assert.doesNotMatch(lastText(nx), /repaired/);

  // A missing job is repaired by /reminder next.
  await cancelJobs({ dedupeKey: jobKey(r.id) });
  assert.match(lastText(await run('next')), /The schedule was repaired/);
  assert.ok(await getJob(jobKey(r.id)));

  const refresh = await run('resume');
  assert.match(lastText(refresh), /Schedule Refreshed/);

  const p = await run('pause');
  assert.match(lastText(p), /Reminder Paused/);
  assert.equal(await getJob(jobKey(r.id)), null);
  assert.match(lastText(await run('pause')), /Already Paused/);
  assert.match(lastText(await run('next')), /Paused — no upcoming runs/);

  const res = await run('resume');
  assert.match(lastText(res), /Reminder Resumed/);
  assert.equal((await row(r)).enabled, true);
  assert.ok(await getJob(jobKey(r.id)));

  // Resume refuses a one-time reminder whose time has passed, and a deleted channel.
  const once = await mkRow(GUILD6, ch.id, { name: 'once-old', mode: 'ONE_SHOT', intervalMinutes: null, runAt: new Date(Date.now() - 3_600_000), enabled: false });
  assert.match(lastText(await run('resume', { name: once.name })), /Time Has Passed/);
  // A one-time reminder that already fired is "sent", not "paused".
  const fired = await mkRow(GUILD6, ch.id, {
    name: 'once-fired', mode: 'ONE_SHOT', intervalMinutes: null, runAt: new Date(Date.now() - 7_200_000), lastSentAt: new Date(Date.now() - 7_200_000), enabled: false,
  });
  const sentText = lastText(await run('pause', { name: fired.name }));
  assert.match(sentText, /Already Sent/);
  assert.match(sentText, /already been sent/);
  await mkRow(GUILD6, 'deleted-channel', { name: 'orphan', enabled: false });
  assert.match(lastText(await run('resume', { name: 'orphan' })), /Channel Missing/);

  const list = await run('list');
  const text = lastText(list);
  assert.match(text, /Reminders/);
  assert.match(text, /▶️ \*\*cycle\*\* · <#\d+> · Every day at 09:00\n↳ next <t:\d+:R>/);
  assert.match(text, /⏸️ \*\*orphan\*\*/);
  assert.match(text, /⏸️ \*\*once-old\*\*|✅ \*\*once-old\*\*/);
  assert.match(lastEmbed(list).footer.text, /4\/100 reminders · Timezone: UTC/);

  const empty = makeInteraction({ guild: makeGuild({ id: GUILD5 }), sub: 'list' });
  await ensureGuild(GUILD5);
  await prisma.reminder.deleteMany({ where: { guildId: GUILD5 } });
  await exec(empty);
  assert.match(lastText(empty), /No reminders yet/);
});

test('DB: /reminder test posts a preview without touching state', { skip: skipDb }, async () => {
  const guild = makeGuild({ id: GUILD, roles: [{ id: '51', mentionable: true }] });
  const target = makeChannel(guild, { id: `${CH}40` });
  const here = makeChannel(guild, { id: `${CH}41` });
  const r = await mkRow(GUILD, target.id, { name: 'preview', pingRoleId: '51', template: { content: 'Hello {server}', embed: null, buttons: [] } });
  const before = await row(r);

  const i = makeInteraction({ guild, sub: 'test', channel: here, opts: { name: 'preview' } });
  await exec(i);
  assert.equal(i.calls.deferReply[0].ephemeral, true);
  assert.equal(here.sent.length, 1);
  assert.equal(target.sent.length, 0);
  assert.equal(here.sent[0].content, '<@&51> Hello Test Guild');
  assert.deepEqual(here.sent[0].allowedMentions, { parse: [] });
  assert.match(lastText(i), /Test Sent/);
  assert.deepEqual(await row(r), before); // lastSentAt, lastMessageId and enabled are untouched

  // Where the bot cannot post, a private preview is shown instead.
  const mute = makeChannel(guild, { id: `${CH}42`, perms: ['ViewChannel'] });
  const j = makeInteraction({ guild, sub: 'test', channel: mute, opts: { name: 'preview' } });
  await exec(j);
  assert.equal(mute.sent.length, 0);
  const out = j.calls.editReply.at(-1);
  assert.equal(out.content, '<@&51> Hello Test Guild');
  assert.deepEqual(out.allowedMentions, { parse: [] });
  assert.match(out.embeds.at(-1).data.title, /Preview Only/);
});

test('DB: autocomplete filters by name and, for pause, by enabled', { skip: skipDb }, async () => {
  await ensureGuild(GUILD4);
  await mkRow(GUILD4, '1', { name: 'ac-alpha' });
  await mkRow(GUILD4, '1', { name: 'ac-beta', enabled: false });
  await mkRow(GUILD4, '1', { name: 'zzz-other' });
  const ask = async (sub, value) => {
    let answer = null;
    await cmd.autocomplete({
      guildId: GUILD4, responded: false,
      options: { getFocused: () => ({ name: 'name', value }), getSubcommand: () => sub },
      respond: async (c) => { answer = c; },
    });
    return answer;
  };
  assert.deepEqual((await ask('edit', 'AC ')).map((c) => c.value), ['ac-alpha', 'ac-beta']);
  assert.deepEqual((await ask('pause', 'ac')).map((c) => c.value), ['ac-alpha']);
  const all = await ask('resume', '');
  assert.ok(all.length >= 3);
  assert.match(all.find((c) => c.value === 'ac-beta').name, /^⏸ ac-beta · Interval$/);
  // Another option focused: empty answer.
  let answer = null;
  await cmd.autocomplete({ guildId: GUILD4, responded: false, options: { getFocused: () => ({ name: 'mode', value: '' }), getSubcommand: () => 'edit' }, respond: async (c) => { answer = c; } });
  assert.deepEqual(answer, []);
});
