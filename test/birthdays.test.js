// pkg-birthdays tests (pkg-birthdays.md §8). Offline by default: date parsing, Feb 29, next occurrences, tick timing, chunking, templates,
// the plugin declaration, the command payloads and import purity.
// With TEST_DATABASE_URL (a migrated database) the DB cases also run: both raw claim statements, the claim / revert / conditional marker
// clear, saveBirthday (late, carry-over, tombstone), removeBirthday, the upcoming query (wrap-around), ensureTickJob, the role transfer,
// the whole tick against fake guilds, and both commands end to end:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/birthdays.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';
delete process.env.BOT_OWNER_ID;

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { PermissionFlagsBits: P, PermissionsBitField } = await import('discord.js');
const svc = await import('../src/services/birthdayService.js');
const birthdayCmd = await import('../src/commands/community/birthday.js');
const configCmd = await import('../src/commands/admin/birthday-config.js');
const { plugin } = await import('../src/plugins/birthdays.js');
const { default: redis, cache } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { JOB_TYPES } = await import('../src/config/jobTypes.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { ensureGuildSettings, updateSettings } = await import('../src/services/settingsService.js');
const { zonedParts } = await import('../src/utils/time.js');

const {
  DEFAULT_BIRTHDAY_MESSAGE, DEFAULT_BIRTHDAY_MESSAGE_AGE, MAX_EVENTS_PER_TICK, CONTINUE_MS, ERROR_RETRY_MS, NEXT_LIMIT,
  clampHour, resolveTimeZone, formatHour, isLeapYear, effectiveDay, isBirthdayOn, todayMatchers, nextOccurrence, ageOn,
  isEventEligible, partitionMarkers, pickTemplate, nextTickAt, tombstoneTtlSeconds, chunkAnnouncements, formatBirthday, codeBlock,
  parseBirthdayInput, renderBirthdayLine, tickKey, tombstoneKey, RAW_SQL,
} = svc;

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const GUILD_IDS = [];
let guildSeq = 0;
/** Guild ids unique to this package (port 55023); every one is deleted in after(). */
const G = () => {
  const id = `95502300${String(++guildSeq).padStart(10, '0')}`;
  GUILD_IDS.push(id);
  return id;
};
let userSeq = 0;
const U = () => `95502310${String(++userSeq).padStart(10, '0')}`;
const BOT = '900000000000000001';
const ADMIN = '900000000000000002';

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB && GUILD_IDS.length) {
    await prisma.scheduledJob.deleteMany({ where: { guildId: { in: GUILD_IDS } } }).catch(() => {});
    // The cascade removes the Birthday rows.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: GUILD_IDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

const d = (iso) => new Date(iso);
const NOW = d('2026-10-10T12:00:00Z');

// ── 1. parseBirthdayInput ─────────────────────────────────────────────────────

test('parseBirthdayInput: accepted forms', () => {
  const ok = (raw, opts = {}) => parseBirthdayInput(raw, { now: NOW, tz: 'UTC', ...opts });
  assert.deepEqual(ok('03-14'), { ok: true, month: 3, day: 14, year: null });
  assert.deepEqual(ok(' 3-4 '), { ok: true, month: 3, day: 4, year: null });
  assert.deepEqual(ok('1998-03-14'), { ok: true, month: 3, day: 14, year: 1998 });
  assert.deepEqual(ok('1998-3-4'), { ok: true, month: 3, day: 4, year: 1998 });
  assert.equal(ok('02-29').ok, true);
  assert.equal(ok('2000-02-29').ok, true);
  assert.equal(ok('2026-10-10').ok, true);
  assert.equal(ok('1998-03-14', { requireYear: true }).ok, true);
});

test('parseBirthdayInput: rejections carry a code and a message', () => {
  const bad = (raw, opts = {}) => parseBirthdayInput(raw, { now: NOW, tz: 'UTC', ...opts });
  const leap = bad('1999-02-29');
  assert.equal(leap.code, 'INVALID_DATE');
  assert.match(leap.message, /leap year/);
  for (const raw of ['02-30', '13-01', '00-10', '04-31', '1998-00-10']) assert.equal(bad(raw).code, 'INVALID_DATE', raw);
  assert.equal(bad('1899-01-01').code, 'YEAR_RANGE');
  assert.equal(bad('2027-01-01').code, 'YEAR_RANGE');
  assert.equal(bad('2026-12-01').code, 'FUTURE');
  assert.equal(bad('2026-10-11').code, 'FUTURE');
  for (const raw of ['14/03', '', 'March 14', '03.14', '1998/03/14', '12345-01-01', '03-14-1998']) assert.equal(bad(raw).code, 'FORMAT', raw);
  assert.equal(bad(undefined).code, 'FORMAT');
  assert.equal(bad('03-14', { requireYear: true }).code, 'YEAR_REQUIRED');
  assert.equal(bad('02-30', { requireYear: true }).code, 'INVALID_DATE', 'validity is checked before "required"');
});

test('parseBirthdayInput: the year range follows the guild-local year, not UTC', () => {
  // 2026-12-31T23:30Z is already 2027-01-01 in Kiritimati (UTC+14).
  const now = d('2026-12-31T23:30:00Z');
  assert.equal(parseBirthdayInput('2027-01-01', { now, tz: 'Pacific/Kiritimati' }).ok, true);
  assert.equal(parseBirthdayInput('2027-01-01', { now, tz: 'UTC' }).code, 'YEAR_RANGE');
  assert.equal(parseBirthdayInput('2027-01-02', { now, tz: 'Pacific/Kiritimati' }).code, 'FUTURE');
});

// ── 2. Feb 29 ────────────────────────────────────────────────────────────────

test('Feb 29 is celebrated on Feb 28 in non-leap years', () => {
  assert.equal(effectiveDay(2, 29, 2027), 28);
  assert.equal(effectiveDay(2, 29, 2028), 29);
  assert.equal(effectiveDay(3, 29, 2027), 29);
  assert.equal(isLeapYear(2028), true);
  assert.equal(isLeapYear(2100), false);
  assert.equal(isBirthdayOn({ month: 2, day: 29 }, { year: 2027, month: 2, day: 28 }), true);
  assert.equal(isBirthdayOn({ month: 2, day: 29 }, { year: 2028, month: 2, day: 28 }), false);
  assert.equal(isBirthdayOn({ month: 2, day: 29 }, { year: 2028, month: 2, day: 29 }), true);
  assert.equal(isBirthdayOn({ month: 2, day: 29 }, { year: 2027, month: 3, day: 1 }), false);
  assert.deepEqual(todayMatchers({ year: 2027, month: 2, day: 28 }), [{ month: 2, day: 28 }, { month: 2, day: 29 }]);
  assert.deepEqual(todayMatchers({ year: 2028, month: 2, day: 28 }), [{ month: 2, day: 28 }]);
  assert.deepEqual(todayMatchers({ year: 2028, month: 2, day: 29 }), [{ month: 2, day: 29 }]);
  assert.deepEqual(todayMatchers({ year: 2027, month: 10, day: 10 }), [{ month: 10, day: 10 }]);
});

// ── 3. nextOccurrence ─────────────────────────────────────────────────────────

test('nextOccurrence', () => {
  const local = { year: 2026, month: 10, day: 10 };
  assert.deepEqual(nextOccurrence({ month: 3, day: 14 }, local), { year: 2027, month: 3, day: 14 });
  assert.deepEqual(nextOccurrence({ month: 10, day: 10 }, local), { year: 2026, month: 10, day: 10 });
  assert.deepEqual(nextOccurrence({ month: 12, day: 31 }, local), { year: 2026, month: 12, day: 31 });
  assert.deepEqual(nextOccurrence({ month: 10, day: 9 }, local), { year: 2027, month: 10, day: 9 });
  assert.deepEqual(nextOccurrence({ month: 2, day: 29 }, { year: 2027, month: 3, day: 1 }), { year: 2028, month: 2, day: 29 });
  assert.deepEqual(nextOccurrence({ month: 2, day: 29 }, { year: 2026, month: 3, day: 1 }), { year: 2027, month: 2, day: 28 });
  assert.deepEqual(nextOccurrence({ month: 2, day: 29 }, { year: 2027, month: 2, day: 28 }), { year: 2027, month: 2, day: 28 });
});

// ── 4. ageOn ─────────────────────────────────────────────────────────────────

test('ageOn', () => {
  assert.equal(ageOn({ year: 2000 }, 2026), 26);
  assert.equal(ageOn({ year: null }, 2026), null);
  assert.equal(ageOn({}, 2026), null);
  assert.equal(ageOn({ year: 2026 }, 2026), null);
  assert.equal(ageOn({ year: 2027 }, 2026), null);
});

// ── 5. isEventEligible ────────────────────────────────────────────────────────

test('isEventEligible: the hour gate, the date and the once-per-year guard', () => {
  const at = (hour) => ({ year: 2026, month: 10, day: 10, hour });
  const row = (lastAnnouncedYear) => ({ month: 10, day: 10, lastAnnouncedYear });
  assert.equal(isEventEligible(row(null), at(8), 9), false);
  assert.equal(isEventEligible(row(null), at(9), 9), true);
  assert.equal(isEventEligible(row(null), at(17), 9), true, 'catch-up later the same day');
  assert.equal(isEventEligible(row(2025), at(9), 9), true);
  assert.equal(isEventEligible(row(2026), at(9), 9), false);
  assert.equal(isEventEligible(row(undefined), at(9), 9), true);
  assert.equal(isEventEligible({ month: 10, day: 11, lastAnnouncedYear: null }, at(23), 9), false);
  assert.equal(isEventEligible(row(null), at(0), 99), false, 'an invalid hour falls back to 9');
});

// ── 6. partitionMarkers ───────────────────────────────────────────────────────

test('partitionMarkers: only today\'s marker of an enabled module with a role is kept', () => {
  const local = { year: 2026, month: 10, day: 10 };
  const marker = (iso, month, day) => ({ id: 'x', userId: 'u', month, day, roleGrantedAt: d(iso) });
  const base = { enabled: true, roleId: 'r', local, tz: 'UTC' };
  const m = marker('2026-10-10T09:00:00Z', 10, 10);
  assert.deepEqual(partitionMarkers([m], base), { keep: [m], expired: [] });
  assert.deepEqual(partitionMarkers([m], { ...base, enabled: false }), { keep: [], expired: [m] });
  assert.deepEqual(partitionMarkers([m], { ...base, roleId: null }), { keep: [], expired: [m] });
  const yesterday = marker('2026-10-09T09:00:00Z', 10, 9);
  assert.deepEqual(partitionMarkers([yesterday], base), { keep: [], expired: [yesterday] });
  const stale = marker('2025-10-10T09:00:00Z', 10, 10);
  assert.deepEqual(partitionMarkers([stale], base), { keep: [], expired: [stale] }, 'a marker from last year is stale');
  // The marker's local date follows the zone: 23:30Z on Oct 9 is already Oct 10 in Pacific/Kiritimati.
  const edge = marker('2026-10-09T23:30:00Z', 10, 10);
  assert.equal(partitionMarkers([edge], { ...base, tz: 'Pacific/Kiritimati' }).keep.length, 1);
  assert.equal(partitionMarkers([edge], base).keep.length, 0);
  // Feb 29 member in a non-leap year: the marker of Feb 28 is today's.
  const leapling = marker('2027-02-28T09:00:00Z', 2, 29);
  assert.equal(partitionMarkers([leapling], { ...base, local: { year: 2027, month: 2, day: 28 } }).keep.length, 1);
});

// ── 7. nextTickAt ────────────────────────────────────────────────────────────

test('nextTickAt: just after the next local hour boundary', () => {
  const iso = (now, tz) => nextTickAt(d(now), tz).toISOString();
  assert.equal(iso('2026-10-10T10:20:00Z', 'UTC'), '2026-10-10T11:00:02.000Z');
  assert.equal(iso('2026-10-10T10:00:01Z', 'UTC'), '2026-10-10T11:00:02.000Z');
  assert.equal(iso('2026-10-10T10:20:00Z', 'Asia/Kolkata'), '2026-10-10T10:30:02.000Z');
  assert.equal(iso('2026-10-25T01:30:00Z', 'Europe/London'), '2026-10-25T02:00:02.000Z'); // the second 01:30 after fall-back
  assert.equal(iso('2026-03-29T00:30:00Z', 'Europe/London'), '2026-03-29T01:00:02.000Z');
  assert.equal(nextTickAt(NOW, 'UTC') instanceof Date, true);
  for (const tz of ['UTC', 'Asia/Kolkata', 'Asia/Kathmandu', 'Europe/London', 'America/New_York', 'Pacific/Kiritimati', 'Australia/Lord_Howe']) {
    for (let t = d('2026-03-28T00:00:00Z').getTime(); t < d('2026-04-01T00:00:00Z').getTime(); t += 17 * 60_000 + 13_000) {
      const now = new Date(t);
      const next = nextTickAt(now, tz).getTime();
      assert.ok(next > t, `${tz} ${now.toISOString()}: next must be after now`);
      assert.ok(next <= t + 3_600_000 + 2_000, `${tz} ${now.toISOString()}: within one hour and 2 s`);
    }
  }
});

test('nextTickAt: an invalid zone behaves as UTC', () => {
  assert.equal(nextTickAt(d('2026-10-10T10:20:00Z'), 'Not/AZone').toISOString(), '2026-10-10T11:00:02.000Z');
});

// ── 8. tombstoneTtlSeconds ────────────────────────────────────────────────────

test('tombstoneTtlSeconds: until the next local new year plus a day', () => {
  assert.equal(tombstoneTtlSeconds(d('2026-12-31T23:00:00Z'), 'UTC'), 3600 + 86_400);
  assert.equal(tombstoneTtlSeconds(d('2026-01-01T00:00:00Z'), 'UTC'), 365 * 86_400 + 86_400);
  for (const tz of ['UTC', 'Pacific/Kiritimati', 'America/New_York', 'Nope']) {
    assert.ok(tombstoneTtlSeconds(NOW, tz) >= 60);
    assert.ok(tombstoneTtlSeconds(d('2026-12-31T23:59:59Z'), tz) >= 60);
  }
});

// ── 9. chunkAnnouncements ─────────────────────────────────────────────────────

test('chunkAnnouncements: size and mention caps, nothing lost', () => {
  const entries = Array.from({ length: 120 }, (_, i) => ({ id: `id${i}`, userId: `u${i}`, text: `${String(i).padStart(3, '0')} ${'x'.repeat(36)}` }));
  assert.ok(entries.every((e) => e.text.length === 40));
  const chunks = chunkAnnouncements(entries);
  assert.ok(chunks.length >= 3);
  for (const c of chunks) {
    assert.ok(c.content.length <= 2000);
    assert.ok(c.userIds.length <= 50);
  }
  assert.equal(chunks.map((c) => c.content).join('\n'), entries.map((e) => e.text).join('\n'));
  assert.deepEqual(chunks.flatMap((c) => c.ids), entries.map((e) => e.id));
});

test('chunkAnnouncements: the 50-entry cap binds before the size cap for short lines', () => {
  const entries = Array.from({ length: 120 }, (_, i) => ({ id: `id${i}`, userId: `u${i}`, text: 'hi' }));
  const chunks = chunkAnnouncements(entries);
  assert.deepEqual(chunks.map((c) => c.ids.length), [50, 50, 20]);
});

test('chunkAnnouncements: a 2000 character line gets its own chunk; duplicate users are listed once', () => {
  const big = 'y'.repeat(2000);
  const chunks = chunkAnnouncements([
    { id: 'a', userId: 'u1', text: 'one' },
    { id: 'b', userId: 'u2', text: big },
    { id: 'c', userId: 'u3', text: 'three' },
  ]);
  assert.equal(chunks.length, 3);
  assert.equal(chunks[1].content, big);
  const dup = chunkAnnouncements([
    { id: 'a', userId: 'u1', text: 'one' },
    { id: 'b', userId: 'u1', text: 'two' },
  ]);
  assert.equal(dup.length, 1);
  assert.deepEqual(dup[0].userIds, ['u1']);
  assert.deepEqual(dup[0].ids, ['a', 'b']);
  assert.deepEqual(chunkAnnouncements([]), []);
});

// ── 10. templates ─────────────────────────────────────────────────────────────

const fakeMember = (id = '1', bot = false) => ({
  id, user: { id, username: 'a', bot, displayAvatarURL: () => '' }, displayName: 'A',
});
const fakeGuildInfo = { id: 'g', name: 'Guild', memberCount: 3, ownerId: '9' };

test('pickTemplate and renderBirthdayLine', () => {
  assert.equal(pickTemplate({ birthdayMessage: null }, null), DEFAULT_BIRTHDAY_MESSAGE);
  assert.equal(pickTemplate({ birthdayMessageAge: 'x {age}' }, 30), 'x {age}');
  assert.equal(pickTemplate(null, 5), DEFAULT_BIRTHDAY_MESSAGE_AGE);
  assert.equal(pickTemplate({ birthdayMessage: 'plain' }, null), 'plain');
  assert.equal(pickTemplate({ birthdayMessage: 'plain' }, 7), DEFAULT_BIRTHDAY_MESSAGE_AGE);

  const args = { member: fakeMember(), guild: fakeGuildInfo, channel: { id: 'c', name: 'general' } };
  assert.equal(renderBirthdayLine({ ...args, template: DEFAULT_BIRTHDAY_MESSAGE_AGE, age: 26 }), 'Happy birthday <@1>! They are now 26! 🎂');
  assert.equal(renderBirthdayLine({ ...args, template: DEFAULT_BIRTHDAY_MESSAGE, age: null }), 'Happy birthday <@1>! 🎂');
  assert.equal(renderBirthdayLine({ ...args, template: 'age=[{age}]', age: null }), 'age=[]');
  assert.equal(renderBirthdayLine({ ...args, template: '{server}: {user.name} in {channel.name}', age: null }), 'Guild: a in general');
  assert.equal(renderBirthdayLine({ ...args, template: '   ', age: null }), DEFAULT_BIRTHDAY_MESSAGE.replace('{user}', '<@1>'));
  assert.equal(renderBirthdayLine({ ...args, template: '   ', age: 4 }), 'Happy birthday <@1>! They are now 4! 🎂');
  assert.equal(renderBirthdayLine({ ...args, template: 'z'.repeat(5000), age: null }).length, 2000, 'each rendered line is clamped to 2000');
});

// ── 11. formatting ────────────────────────────────────────────────────────────

test('formatting helpers', () => {
  assert.equal(formatBirthday({ month: 3, day: 14 }), 'March 14');
  assert.equal(formatBirthday({ month: 3, day: 14 }, { short: true }), 'Mar 14');
  assert.equal(formatBirthday({ month: 3, day: 14, year: 1998 }, { withYear: true }), 'March 14, 1998');
  assert.equal(formatBirthday({ month: 3, day: 14, year: 1998 }), 'March 14', 'no year unless asked');
  assert.equal(formatBirthday({ month: 3, day: 14, year: null }, { withYear: true }), 'March 14');
  assert.equal(formatBirthday({ month: 12, day: 1 }, { short: true }), 'Dec 1');
  assert.equal(formatHour(9), '09:00');
  assert.equal(formatHour(24), '09:00');
  assert.equal(formatHour(0), '00:00');
  assert.equal(clampHour(24), 9);
  assert.equal(clampHour(0), 0);
  assert.equal(clampHour(23), 23);
  assert.equal(clampHour(-1), 9);
  assert.equal(clampHour(1.5), 9);
  assert.equal(clampHour(null), 9);
  assert.equal(resolveTimeZone({ timezone: 'Europe/Paris' }), 'Europe/Paris');
  assert.equal(resolveTimeZone({ timezone: 'nope' }), 'UTC');
  assert.equal(resolveTimeZone(null), 'UTC');
});

test('codeBlock cannot be broken out of', () => {
  for (const text of ['a```b', '``````', '````x`', '`', '```\n@everyone']) {
    const out = codeBlock(text);
    assert.ok(out.startsWith('```\n') && out.endsWith('\n```'));
    const inner = out.slice(4, -4);
    assert.ok(!inner.includes('```'), JSON.stringify(text));
  }
  assert.ok(codeBlock('x'.repeat(5000)).length <= 1000 + 8);
});

// ── 12. plugin ───────────────────────────────────────────────────────────────

test('plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('birthdays')), []);
  assert.ok(plugins.some((p) => p.id === 'birthdays'));
  assert.equal(plugin.id, 'birthdays');
  assert.equal(plugin.module, 'birthdays');
  assert.equal(plugin.hooks.length, 1);
  const { run, ...rest } = plugin.hooks[0];
  assert.deepEqual(rest, { event: 'guildReady', id: 'birthdays', always: true });
  assert.equal(typeof run, 'function');
  assert.deepEqual(plugin.components, []);
  assert.equal(plugin.jobs.length, 1);
  assert.equal(plugin.jobs[0].type, 'birthday.tick');
  assert.equal(typeof plugin.jobs[0].handler, 'function');
  assert.equal(JOB_TYPES['birthday.tick'].owner, 'birthdays');
  assert.equal(JOB_TYPES['birthday.tick'].recurring, true);
  assert.equal(tickKey('123'), 'birthday.tick:123');
  assert.equal(tombstoneKey('1', '2'), 'birthday:announced:1:2');
});

// ── 13. command payloads ─────────────────────────────────────────────────────

const charCount = (node) => {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const o of node.options ?? []) n += charCount(o);
  return n;
};

test('/birthday payload and exports', () => {
  const json = buildCommandPayload(birthdayCmd);
  assert.equal(json.name, 'birthday');
  assert.deepEqual(json.options.map((o) => o.name), ['set', 'forget', 'show', 'next']);
  assert.equal(json.default_member_permissions, undefined);
  assert.deepEqual(json.contexts, [0]);
  assert.ok(charCount(json) <= 600, `${charCount(json)} chars`);
  assert.equal(birthdayCmd.module, 'birthdays');
  assert.equal(birthdayCmd.gated, true);
  assert.equal(birthdayCmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.equal(birthdayCmd.allowInDMs, undefined);
  assert.equal(typeof birthdayCmd.execute, 'function');
  const set = json.options.find((o) => o.name === 'set');
  assert.equal(set.options[0].required, true);
  assert.equal(set.options[0].min_length, 3);
  assert.equal(set.options[0].max_length, 10);
});

test('/birthday-config payload and exports', () => {
  const json = buildCommandPayload(configCmd);
  assert.equal(json.name, 'birthday-config');
  assert.equal(json.options.length, 10);
  assert.deepEqual(json.options.map((o) => o.name),
    ['channel', 'role', 'hour', 'message', 'message-age', 'require-year', 'set-user', 'unset-user', 'show', 'test']);
  assert.equal(json.default_member_permissions, String(P.ManageGuild));
  assert.ok(charCount(json) <= 1300, `${charCount(json)} chars`);
  assert.equal(configCmd.module, 'birthdays');
  assert.equal(configCmd.gated, false);
  assert.equal(configCmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  const hour = json.options.find((o) => o.name === 'hour').options[0];
  assert.equal(hour.name, 'hour');
  assert.equal(hour.min_value, 0);
  assert.equal(hour.max_value, 23);
  for (const name of ['message', 'message-age']) {
    assert.equal(json.options.find((o) => o.name === name).options[0].max_length, 1000);
  }
});

// ── 14. import purity ────────────────────────────────────────────────────────

test('importing the service and the plugin starts no timer', async () => {
  const calls = [];
  const realTimeout = globalThis.setTimeout;
  const realInterval = globalThis.setInterval;
  globalThis.setTimeout = (...a) => { calls.push('setTimeout'); return realTimeout(...a); };
  globalThis.setInterval = (...a) => { calls.push('setInterval'); return realInterval(...a); };
  try {
    const stamp = `?purity=${Date.now()}`;
    const a = await import(`../src/services/birthdayService.js${stamp}`);
    const b = await import(`../src/plugins/birthdays.js${stamp}`);
    assert.equal(typeof a.runBirthdayTick, 'function');
    assert.equal(b.plugin.id, 'birthdays');
  } finally {
    globalThis.setTimeout = realTimeout;
    globalThis.setInterval = realInterval;
  }
  assert.deepEqual(calls, []);
});

test('RAW_SQL builders return Prisma.Sql without a bare now()', () => {
  assert.ok(RAW_SQL.length >= 2);
  const names = new Set();
  for (const { name, build } of RAW_SQL) {
    names.add(name);
    const sql = build();
    assert.equal(typeof sql.sql, 'string', name);
    assert.ok(!/(?<![\w.])now\(\)(?!\s*AT\s+TIME\s+ZONE\s+'UTC')/i.test(sql.sql), `${name}: bare now()`);
    assert.ok(!/CURRENT_TIMESTAMP/i.test(sql.sql), name);
    assert.ok(/"updatedAt"/.test(sql.sql), `${name} sets updatedAt explicitly`);
  }
  assert.equal(names.size, RAW_SQL.length);
  const withMarker = RAW_SQL.find((r) => r.name === 'birthdays.claimWithMarker').build().sql;
  const without = RAW_SQL.find((r) => r.name === 'birthdays.claim').build().sql;
  assert.ok(withMarker.includes('"roleGrantedAt"'));
  assert.ok(!without.includes('"roleGrantedAt"'));
});

test('guildReady hook does nothing without settings or configuration (no database work)', async () => {
  let asked = 0;
  await svc.onGuildReady({ getSettings: async () => { asked += 1; return null; }, guild: { id: 'x' } });
  await svc.onGuildReady({ getSettings: async () => ({ birthdayChannelId: null, birthdayRoleId: null }), guild: { id: 'x' } });
  assert.equal(asked, 1);
});

test('ensureTickJob without settings is a no-op and never throws', async () => {
  assert.equal(await svc.ensureTickJob('1', null), undefined);
  assert.equal(await svc.ensureTickJob('1', null, { soon: true }), undefined);
});

test('runBirthdayTick: an unavailable guild retries in five minutes', async () => {
  const before = Date.now();
  const r = await svc.runBirthdayTick({ guildId: 'x' }, { client: {}, guild: { available: false } });
  assert.ok(r.nextRunAt.getTime() >= before + ERROR_RETRY_MS - 1000 && r.nextRunAt.getTime() <= Date.now() + ERROR_RETRY_MS + 1000);
  const r2 = await svc.runBirthdayTick({ guildId: 'x' }, { client: {}, guild: undefined });
  assert.ok(r2.nextRunAt instanceof Date);
});

// ═══ Database cases ══════════════════════════════════════════════════════════

// ── Fakes ─────────────────────────────────────────────────────────────────────

const ALL = () => new PermissionsBitField(P.Administrator);

function mkGuild(id = G()) {
  const store = new Map(); // everybody who is "in the guild" (fetchable)
  const cacheMap = new Map(); // the members discord.js has cached
  const guild = {
    id,
    available: true,
    name: 'Test Guild',
    memberCount: 10,
    ownerId: '900000000000000099',
    channels: { cache: new Map() },
    roles: { cache: new Map() },
    members: {
      cache: cacheMap,
      store,
      fetchCalls: [],
      me: { id: BOT, permissions: new PermissionsBitField(P.ManageRoles), roles: { highest: { position: 10 } } },
      async fetch(arg) {
        if (typeof arg === 'string') {
          const m = store.get(arg);
          if (!m) throw Object.assign(new Error('Unknown Member'), { code: 10007 });
          return m;
        }
        guild.members.fetchCalls.push(arg);
        const out = new Map();
        for (const uid of arg.user) { const m = store.get(uid); if (m) out.set(uid, m); }
        return out;
      },
    },
  };
  return guild;
}

function mkChannel(guild, id = `95502320${String(guild.channels.cache.size + 1).padStart(10, '0')}`, { canSend = true } = {}) {
  const ch = {
    id, guild, type: 0, sent: [], failWith: null,
    isTextBased: () => true,
    isThread: () => false,
    permissionsFor: () => new PermissionsBitField(canSend ? [P.ViewChannel, P.SendMessages] : [P.ViewChannel]),
    toString: () => `<#${id}>`,
    async send(payload) {
      if (ch.failWith) throw ch.failWith;
      ch.sent.push(payload);
      return { id: 'msg' };
    },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function mkRole(guild, id = `95502330${String(guild.roles.cache.size + 1).padStart(10, '0')}`, { position = 5, perms = 0n } = {}) {
  const role = { id, position, managed: false, permissions: new PermissionsBitField(perms), toString: () => `<@&${id}>` };
  guild.roles.cache.set(id, role);
  return role;
}

function mkMember(guild, id = U(), { bot = false, roleIds = [], pending = false, cached = true, inGuild = true, position = 1 } = {}) {
  const roles = new Set(roleIds);
  const m = {
    id, guild, pending, calls: [], failAdd: null, failRemove: null,
    user: { id, bot, username: `user${id.slice(-3)}`, displayAvatarURL: () => '', toString: () => `<@${id}>` },
    displayName: `User ${id.slice(-3)}`,
    toString: () => `<@${id}>`,
    roles: {
      cache: roles,
      highest: { position },
      // Like discord.js, add() does NOT touch the cache: the gateway event does that later (tests emulate it).
      async add(roleId, reason) { m.calls.push(['add', roleId, reason]); if (m.failAdd) throw m.failAdd; },
      async remove(roleId, reason) { m.calls.push(['remove', roleId, reason]); if (m.failRemove) throw m.failRemove; roles.delete(roleId); },
    },
  };
  if (inGuild) guild.members.store.set(id, m);
  if (inGuild && cached) guild.members.cache.set(id, m);
  return m;
}

const err = (code) => Object.assign(new Error(`discord ${code}`), { code });

const BASE = { birthdaysEnabled: true, timezone: 'UTC', birthdayHour: 0, birthdayChannelId: null, birthdayRoleId: null, birthdayMessage: null, birthdayMessageAge: null, birthdayRequireYear: false };
async function setup(guild, data = {}) {
  await ensureGuildSettings(guild.id);
  return updateSettings(guild.id, { ...BASE, ...data });
}
const addRow = (guildId, userId, data) => prisma.birthday.create({ data: { guildId, userId, month: 1, day: 1, ...data } });
const rowOf = (guildId, userId) => prisma.birthday.findUnique({ where: { guildId_userId: { guildId, userId } } });
const localNow = (tz = 'UTC') => zonedParts(new Date(), tz);
const todayRow = (tz = 'UTC') => { const l = localNow(tz); return { month: l.month, day: l.day }; };
const otherDay = (tz = 'UTC') => {
  const l = localNow(tz);
  return { month: l.month === 12 ? 1 : l.month + 1, day: 1 };
};
const dayBefore = (tz = 'UTC') => {
  const l = zonedParts(new Date(Date.now() - 86_400_000), tz);
  return { month: l.month, day: l.day };
};

/** Runs fn with cache.get / cache.set replaced (the real ones fail open offline). */
async function withCache({ get = async () => null, set = async () => true } = {}, fn) {
  const real = { get: cache.get, set: cache.set };
  cache.get = get;
  cache.set = set;
  try {
    return await fn();
  } finally {
    cache.get = real.get;
    cache.set = real.set;
  }
}

// ── Raw SQL: the claim ────────────────────────────────────────────────────────

test('DB: every RAW_SQL statement executes against the migrated schema', { skip: skipDb }, async () => {
  for (const { name, build } of RAW_SQL) {
    const rows = await prisma.$queryRaw(build());
    assert.ok(Array.isArray(rows), name);
  }
});

test('DB: the claim marks rows once, with and without the role marker', { skip: skipDb }, async () => {
  const g = G();
  await ensureGuildSettings(g);
  const [a, b, c] = [U(), U(), U()];
  const ra = await addRow(g, a, { year: 1990 });
  const rb = await addRow(g, b, {});
  const rc = await addRow(g, c, { lastAnnouncedYear: 2026 });

  // Without a marker.
  const first = await svc.claimBirthdays({ guildId: g, ids: [ra.id, rc.id], year: 2026, setMarker: false });
  assert.deepEqual(first.map((r) => r.id), [ra.id], 'an already celebrated row is not returned');
  assert.equal(first[0].userId, a);
  assert.equal(first[0].year, 1990);
  const after = await rowOf(g, a);
  assert.equal(after.lastAnnouncedYear, 2026);
  assert.equal(after.roleGrantedAt, null);

  // A second claim of the same ids returns nothing: the at-most-once guard.
  assert.deepEqual(await svc.claimBirthdays({ guildId: g, ids: [ra.id], year: 2026, setMarker: false }), []);

  // With a marker; the marker is "now" in UTC.
  const before = Date.now();
  const second = await svc.claimBirthdays({ guildId: g, ids: [rb.id], year: 2026, setMarker: true });
  assert.equal(second.length, 1);
  assert.equal(second[0].year, null);
  const marked = await rowOf(g, b);
  assert.equal(marked.lastAnnouncedYear, 2026);
  assert.ok(Math.abs(marked.roleGrantedAt.getTime() - before) < 60_000, 'roleGrantedAt is the real UTC now, whatever the session time zone');
  assert.ok(Math.abs(marked.updatedAt.getTime() - before) < 60_000);

  // A concurrent double claim: every row comes back exactly once.
  const rows = await Promise.all(Array.from({ length: 5 }, (_, i) => addRow(g, `${a}-${i}`, {})));
  const ids = rows.map((r) => r.id);
  const results = await Promise.all([1, 2, 3].map(() => svc.claimBirthdays({ guildId: g, ids, year: 2026, setMarker: true })));
  assert.deepEqual(results.flat().map((r) => r.id).sort(), [...ids].sort());

  // The claim is guild-scoped: another guild's id list claims nothing.
  const other = G();
  await ensureGuildSettings(other);
  const ro = await addRow(other, U(), {});
  assert.deepEqual(await svc.claimBirthdays({ guildId: g, ids: [ro.id], year: 2026, setMarker: false }), []);
  assert.equal((await prisma.birthday.findUnique({ where: { id: ro.id } })).lastAnnouncedYear, null);
});

test('DB: revertClaims gives the claim back only for the claimed year; clearMarker is conditional', { skip: skipDb }, async () => {
  const g = G();
  await ensureGuildSettings(g);
  const r = await addRow(g, U(), {});
  await svc.claimBirthdays({ guildId: g, ids: [r.id], year: 2026, setMarker: true });

  await svc.revertClaims(g, [r.id], 2025); // wrong year: nothing happens
  assert.equal((await prisma.birthday.findUnique({ where: { id: r.id } })).lastAnnouncedYear, 2026);
  await svc.revertClaims(g, [r.id], 2026);
  assert.equal((await prisma.birthday.findUnique({ where: { id: r.id } })).lastAnnouncedYear, 2025);
  await svc.revertClaims(g, [], 2026); // no ids: no query, no error

  const read = await prisma.birthday.findUnique({ where: { id: r.id } });
  assert.ok(read.roleGrantedAt);
  // A marker re-set after it was read survives.
  await prisma.birthday.update({ where: { id: r.id }, data: { roleGrantedAt: new Date(read.roleGrantedAt.getTime() + 5000) } });
  assert.equal((await svc.clearMarker(read)).count, 0);
  assert.ok((await prisma.birthday.findUnique({ where: { id: r.id } })).roleGrantedAt);
  const fresh = await prisma.birthday.findUnique({ where: { id: r.id } });
  assert.equal((await svc.clearMarker(fresh)).count, 1);
  assert.equal((await prisma.birthday.findUnique({ where: { id: r.id } })).roleGrantedAt, null);
});

// ── saveBirthday / removeBirthday / queries ───────────────────────────────────

test('DB: saveBirthday creates, updates and carries the once-a-year guard over', { skip: skipDb }, async () => {
  const g = G();
  const guild = { id: g };
  const user = U();
  const settings = { timezone: 'UTC', birthdayHour: 9 };
  const at = (iso) => d(iso);
  const save = (date, now) => withCache({}, () => svc.saveBirthday({ guild, userId: user, date, settings, now }));

  // Created, not today.
  let r = await save({ month: 3, day: 14, year: 1998 }, at('2026-10-10T12:00:00Z'));
  assert.deepEqual([r.created, r.dateChanged, r.late, r.alreadyCelebrated], [true, true, false, false]);
  assert.equal(r.row.year, 1998);
  assert.equal(r.row.lastAnnouncedYear, null);

  // The same date again changes nothing (a pending celebration is never cancelled).
  r = await save({ month: 3, day: 14, year: 1998 }, at('2026-03-14T12:00:00Z'));
  assert.deepEqual([r.created, r.dateChanged, r.late, r.alreadyCelebrated], [false, false, false, false]);
  assert.equal(r.row.lastAnnouncedYear, null);

  // Moving the birthday to today after the announcement hour: late, counts from next year.
  r = await save({ month: 10, day: 10, year: null }, at('2026-10-10T12:00:00Z'));
  assert.deepEqual([r.created, r.dateChanged, r.late, r.alreadyCelebrated], [false, true, true, false]);
  assert.equal(r.row.lastAnnouncedYear, 2026);
  assert.equal(r.row.year, null);

  // A further change the same year: already celebrated.
  r = await save({ month: 11, day: 11, year: null }, at('2026-10-10T13:00:00Z'));
  assert.deepEqual([r.late, r.alreadyCelebrated], [false, true]);
  assert.equal(r.row.lastAnnouncedYear, 2026);

  // Next year the guard no longer applies.
  r = await save({ month: 11, day: 12, year: null }, at('2027-01-05T12:00:00Z'));
  assert.deepEqual([r.late, r.alreadyCelebrated], [false, false]);
  assert.equal(r.row.lastAnnouncedYear, 2026, 'the old value stays; any value below the year means "not yet"');

  // Before the announcement hour on the day itself, the celebration is still to come.
  const g2 = { id: G() };
  const early = await withCache({}, () => svc.saveBirthday({ guild: g2, userId: user, date: { month: 10, day: 10, year: null }, settings, now: at('2026-10-10T08:59:00Z') }));
  assert.deepEqual([early.late, early.alreadyCelebrated], [false, false]);
  assert.equal(early.row.lastAnnouncedYear, null);
});

test('DB: saveBirthday restores the guard from the tombstone after forget-then-set', { skip: skipDb }, async () => {
  const guild = { id: G() };
  const user = U();
  const settings = { timezone: 'UTC', birthdayHour: 9 };
  const gets = [];
  const now = d('2026-10-10T12:00:00Z');
  const hit = (key) => { gets.push(key); return Promise.resolve(2026); };

  const r = await withCache({ get: hit }, () => svc.saveBirthday({ guild, userId: user, date: { month: 5, day: 5, year: null }, settings, now }));
  assert.deepEqual(gets, [tombstoneKey(guild.id, user)]);
  assert.equal(r.created, true);
  assert.equal(r.row.lastAnnouncedYear, 2026);
  assert.equal(r.alreadyCelebrated, true);
  assert.equal(r.late, false);

  // Fail-open: a miss (or Redis down) carries nothing over; an old year is ignored too.
  const g2 = { id: G() };
  const miss = await withCache({ get: async () => null }, () => svc.saveBirthday({ guild: g2, userId: user, date: { month: 5, day: 5, year: null }, settings, now }));
  assert.equal(miss.row.lastAnnouncedYear, null);
  const g3 = { id: G() };
  const old = await withCache({ get: async () => 2025 }, () => svc.saveBirthday({ guild: g3, userId: user, date: { month: 5, day: 5, year: null }, settings, now }));
  assert.equal(old.row.lastAnnouncedYear, null);

  // The tombstone is read only when CREATING a row.
  const again = await withCache({ get: async () => { throw new Error('must not be read'); } }, () => svc.saveBirthday({ guild, userId: user, date: { month: 6, day: 6, year: null }, settings, now }));
  assert.equal(again.created, false);
});

test('DB: concurrent saves of the same member resolve to one row', { skip: skipDb }, async () => {
  const guild = { id: G() };
  const user = U();
  const settings = { timezone: 'UTC', birthdayHour: 9 };
  const now = d('2026-10-10T12:00:00Z');
  const results = await withCache({}, () => Promise.all([
    svc.saveBirthday({ guild, userId: user, date: { month: 1, day: 2, year: null }, settings, now }),
    svc.saveBirthday({ guild, userId: user, date: { month: 3, day: 4, year: 1999 }, settings, now }),
  ]));
  assert.equal(results.length, 2);
  assert.equal(await prisma.birthday.count({ where: { guildId: guild.id, userId: user } }), 1);
});

test('DB: removeBirthday takes the role off first, deletes, and keeps the tombstone', { skip: skipDb }, async () => {
  const guild = mkGuild();
  await ensureGuildSettings(guild.id);
  const role = mkRole(guild);
  const m = mkMember(guild, U(), { roleIds: [role.id] });
  await addRow(guild.id, m.id, { roleGrantedAt: new Date(), lastAnnouncedYear: 2026 });

  const res = await svc.removeBirthday({ guild, userId: m.id, settings: { birthdayRoleId: role.id } });
  assert.deepEqual(res, { removed: true });
  assert.deepEqual(m.calls, [['remove', role.id, 'Birthday removed']]);
  assert.equal(await rowOf(guild.id, m.id), null);
  assert.deepEqual(await svc.removeBirthday({ guild, userId: m.id, settings: null }), { removed: false });

  // No marker: the role is not touched. A departed member: the row still goes.
  const plain = mkMember(guild, U(), { roleIds: [role.id] });
  await addRow(guild.id, plain.id, {});
  await svc.removeBirthday({ guild, userId: plain.id, settings: { birthdayRoleId: role.id } });
  assert.deepEqual(plain.calls, []);
  const gone = U();
  await addRow(guild.id, gone, { roleGrantedAt: new Date() });
  assert.deepEqual(await svc.removeBirthday({ guild, userId: gone, settings: { birthdayRoleId: role.id } }), { removed: true });
  assert.equal(await rowOf(guild.id, gone), null);
  // A failing role removal never blocks the deletion.
  const stuck = mkMember(guild, U(), { roleIds: [role.id] });
  stuck.failRemove = err(50013);
  await addRow(guild.id, stuck.id, { roleGrantedAt: new Date() });
  assert.deepEqual(await svc.removeBirthday({ guild, userId: stuck.id, settings: { birthdayRoleId: role.id } }), { removed: true });
  assert.equal(await rowOf(guild.id, stuck.id), null);
});

test('DB: countBirthdays and hasMarkers', { skip: skipDb }, async () => {
  const g = G();
  await ensureGuildSettings(g);
  assert.deepEqual(await svc.countBirthdays(g), { total: 0, withoutYear: 0 });
  assert.equal(await svc.hasMarkers(g), false);
  await addRow(g, U(), { year: 1990 });
  await addRow(g, U(), {});
  const marked = await addRow(g, U(), { roleGrantedAt: new Date() });
  assert.deepEqual(await svc.countBirthdays(g), { total: 3, withoutYear: 2 });
  assert.equal(await svc.hasMarkers(g), true);
  await svc.clearMarker(marked);
  assert.equal(await svc.hasMarkers(g), false);
});

test('DB: getUpcomingBirthdays orders by next occurrence, wraps the year and skips bots and departed members', { skip: skipDb }, async () => {
  const guild = mkGuild();
  await ensureGuildSettings(guild.id);
  const mk = async (month, day, opts) => { const m = mkMember(guild, U(), opts); await addRow(guild.id, m.id, { month, day }); return m; };
  const today = await mk(10, 10);
  const tomorrow = await mk(10, 11);
  const xmas = await mk(12, 25);
  const leapling = await mk(2, 29);
  const spring = await mk(3, 14, { cached: false }); // found through the gateway fetch
  const earlier = await mk(10, 9);
  await mk(11, 1, { bot: true });
  await mk(11, 2, { inGuild: false });

  const list = await svc.getUpcomingBirthdays(guild, { timezone: 'UTC' }, { limit: 10, now: NOW });
  assert.deepEqual(list.map((e) => e.userId), [today, tomorrow, xmas, leapling, spring, earlier].map((m) => m.id));
  assert.equal(list[0].isToday, true);
  assert.equal(list[1].isToday, false);
  assert.deepEqual(list[3].occ, { year: 2027, month: 2, day: 28 }, 'Feb 29 is celebrated on Feb 28 in 2027');
  assert.equal(list[3].month, 2);
  assert.equal(list[3].day, 29);
  assert.deepEqual(list[5].occ, { year: 2027, month: 10, day: 9 });
  assert.equal(list[0].at.toISOString(), '2026-10-10T00:00:00.000Z');
  assert.ok(guild.members.fetchCalls.length >= 1, 'a cache miss goes to the gateway');

  const two = await svc.getUpcomingBirthdays(guild, { timezone: 'UTC' }, { limit: 2, now: NOW });
  assert.equal(two.length, 2);

  // A failed member fetch keeps every row.
  guild.members.fetch = async () => { throw new Error('timeout'); };
  const kept = await svc.getUpcomingBirthdays(guild, { timezone: 'UTC' }, { limit: 10, now: NOW });
  assert.ok(kept.length >= list.length);

  // Another guild never leaks in, and an empty guild gives an empty list.
  const empty = mkGuild();
  await ensureGuildSettings(empty.id);
  assert.deepEqual(await svc.getUpcomingBirthdays(empty, null, { now: NOW }), []);
});

test('DB: the upcoming query is bounded (30 candidates) and wraps after the year end', { skip: skipDb }, async () => {
  const guild = mkGuild();
  await ensureGuildSettings(guild.id);
  const ids = [];
  for (let i = 0; i < 40; i += 1) {
    const m = mkMember(guild, U());
    ids.push(m.id);
    await addRow(guild.id, m.id, { month: 1 + (i % 12), day: 1 + Math.floor(i / 12) });
  }
  const list = await svc.getUpcomingBirthdays(guild, { timezone: 'UTC' }, { limit: NEXT_LIMIT, now: d('2026-06-15T12:00:00Z') });
  assert.equal(list.length, NEXT_LIMIT);
  for (let i = 1; i < list.length; i += 1) {
    const a = list[i - 1].occ; const b = list[i].occ;
    assert.ok((a.year - b.year) || (a.month - b.month) || (a.day - b.day), 'sorted');
    assert.ok(a.year < b.year || (a.year === b.year && (a.month < b.month || (a.month === b.month && a.day <= b.day))));
  }
  assert.equal(list[0].occ.year, 2026);
});

// ── Scheduling ───────────────────────────────────────────────────────────────

test('DB: ensureTickJob schedules, keeps, revives, replaces and cancels', { skip: skipDb }, async () => {
  const g = G();
  await ensureGuildSettings(g);
  const key = tickKey(g);
  const job = () => prisma.scheduledJob.findUnique({ where: { dedupeKey: key } });
  const configured = { birthdayChannelId: '1', birthdayRoleId: null, timezone: 'UTC' };
  const none = { birthdayChannelId: null, birthdayRoleId: null, timezone: 'UTC' };

  await svc.ensureTickJob(g, none);
  assert.equal(await job(), null, 'nothing configured: no job');

  const start = Date.now();
  await svc.ensureTickJob(g, configured);
  const first = await job();
  assert.equal(first.type, 'birthday.tick');
  assert.equal(first.status, 'PENDING');
  assert.equal(first.guildId, g);
  assert.ok(first.runAt.getTime() > start && first.runAt.getTime() <= start + 3_600_000 + 3_000, 'aligned with the next hour boundary');
  assert.equal(first.runAt.getUTCMinutes(), 0);
  assert.equal(first.runAt.getUTCSeconds(), 2);

  await svc.ensureTickJob(g, configured);
  assert.equal((await job()).version, first.version, 'a pending job is left alone');

  await prisma.scheduledJob.update({ where: { dedupeKey: key }, data: { status: 'FAILED' } });
  await svc.ensureTickJob(g, configured);
  assert.equal((await job()).status, 'PENDING', 'a FAILED job is revived');

  await svc.ensureTickJob(g, configured, { soon: true });
  const soon = await job();
  assert.ok(soon.version > first.version);
  assert.ok(soon.runAt.getTime() <= Date.now() + 6_000, 'soon = a few seconds');

  // Nothing configured but a marker left: the job stays so the tick can clear it.
  await addRow(g, U(), { roleGrantedAt: new Date() });
  await svc.ensureTickJob(g, none);
  assert.ok(await job());
  await prisma.birthday.updateMany({ where: { guildId: g }, data: { roleGrantedAt: null } });
  await svc.ensureTickJob(g, none);
  assert.equal(await job(), null, 'nothing configured and no marker: the job is cancelled');
});

test('DB: the guildReady hook revives the job of a configured guild only', { skip: skipDb }, async () => {
  const g = G();
  await ensureGuildSettings(g);
  const job = () => prisma.scheduledJob.findUnique({ where: { dedupeKey: tickKey(g) } });
  await svc.onGuildReady({ guild: { id: g }, getSettings: async () => ({ birthdayChannelId: null, birthdayRoleId: null, timezone: 'UTC' }) });
  assert.equal(await job(), null);
  await svc.onGuildReady({ guild: { id: g }, getSettings: async () => ({ birthdayChannelId: null, birthdayRoleId: '5', timezone: 'UTC' }) });
  assert.equal((await job()).status, 'PENDING');
});

// ── transferBirthdayRole ─────────────────────────────────────────────────────

test('DB: transferBirthdayRole moves today\'s celebrants off the old role', { skip: skipDb }, async () => {
  const guild = mkGuild();
  await ensureGuildSettings(guild.id);
  const oldRole = mkRole(guild);
  const newRole = mkRole(guild);
  const has = mkMember(guild, U(), { roleIds: [oldRole.id] });
  const hasNot = mkMember(guild, U());
  const stuck = mkMember(guild, U(), { roleIds: [oldRole.id] });
  stuck.failRemove = err(50013);
  const fetched = mkMember(guild, U(), { roleIds: [oldRole.id], cached: false });
  for (const m of [has, hasNot, stuck, fetched]) await addRow(guild.id, m.id, { roleGrantedAt: new Date() });

  const res = await svc.transferBirthdayRole(guild, oldRole.id, newRole.id);
  assert.deepEqual(res, { removed: 2, failed: 1 });
  assert.deepEqual(has.calls, [['remove', oldRole.id, 'Birthday role changed']]);
  assert.deepEqual(hasNot.calls, []);
  assert.equal(await svc.hasMarkers(guild.id), true, 'with a new role the markers stay');

  const res2 = await svc.transferBirthdayRole(guild, oldRole.id, null);
  assert.equal(res2.failed, 1);
  assert.equal(await svc.hasMarkers(guild.id), false, 'role switched off: every marker is cleared');

  // An old role the bot can no longer manage counts every marked member as failed; a deleted one counts none.
  const g2 = mkGuild();
  await ensureGuildSettings(g2.id);
  const high = mkRole(g2, undefined, { position: 20 });
  const member = mkMember(g2, U(), { roleIds: [high.id] });
  await addRow(g2.id, member.id, { roleGrantedAt: new Date() });
  assert.deepEqual(await svc.transferBirthdayRole(g2, high.id, 'x'), { removed: 0, failed: 1 });
  assert.deepEqual(await svc.transferBirthdayRole(g2, 'deleted-role', 'x'), { removed: 0, failed: 0 });
  assert.deepEqual(member.calls, []);
});

// ── The tick ─────────────────────────────────────────────────────────────────

const tick = (guild, opts) => svc.tickGuild({}, guild, opts);
/** UTC guilds: the hourly cadence sits on :00:02, which neither the 60 s continuation nor the 5 min retry can match. */
function assertHourly(date, label) {
  assert.ok(date instanceof Date, label);
  assert.equal(date.getUTCMinutes(), 0, label);
  assert.equal(date.getUTCSeconds(), 2, label);
  assert.equal(date.getUTCMilliseconds(), 0, label);
  assert.ok(date.getTime() > Date.now() && date.getTime() <= Date.now() + 3_600_000 + 3_000, label);
}

test('DB: tick announces together, pings only the celebrants, grants the role once and never announces twice', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  const role = mkRole(guild);
  await setup(guild, { birthdayChannelId: ch.id, birthdayRoleId: role.id });

  const year = localNow().year;
  const a = mkMember(guild, `${U()}`);
  const b = mkMember(guild, `${U()}`);
  const c = mkMember(guild, U());
  const bot = mkMember(guild, U(), { bot: true });
  const gone = mkMember(guild, U(), { inGuild: false });
  await addRow(guild.id, a.id, { ...todayRow(), year: year - 30 });
  await addRow(guild.id, b.id, todayRow());
  await addRow(guild.id, c.id, otherDay());
  await addRow(guild.id, bot.id, todayRow());
  await addRow(guild.id, gone.id, todayRow());

  const tombstones = [];
  const before = Date.now();
  const res = await withCache({ set: async (k, v, ttl) => { tombstones.push([k, v, ttl]); return true; } }, () => tick(guild));
  assert.ok(res.nextRunAt.getTime() > before && res.nextRunAt.getTime() <= before + 3_600_000 + 3_000);

  assert.equal(ch.sent.length, 1, 'one batched message');
  const sent = ch.sent[0];
  assert.deepEqual(sent.allowedMentions.parse, []);
  assert.deepEqual([...sent.allowedMentions.users].sort(), [a.id, b.id].sort());
  assert.ok(sent.content.includes(`Happy birthday <@${a.id}>! They are now 30! 🎂`));
  assert.ok(sent.content.includes(`Happy birthday <@${b.id}>! 🎂`));
  assert.ok(!sent.content.includes(c.id) && !sent.content.includes(bot.id) && !sent.content.includes(gone.id));

  assert.deepEqual(a.calls, [['add', role.id, 'Birthday']]);
  assert.deepEqual(b.calls, [['add', role.id, 'Birthday']]);
  assert.deepEqual(c.calls, []);
  assert.deepEqual(bot.calls, []);

  for (const m of [a, b]) {
    const row = await rowOf(guild.id, m.id);
    assert.equal(row.lastAnnouncedYear, year);
    assert.ok(row.roleGrantedAt, 'role marker set');
  }
  for (const m of [c, bot, gone]) {
    const row = await rowOf(guild.id, m.id);
    assert.equal(row.lastAnnouncedYear, null, 'not claimed');
    assert.equal(row.roleGrantedAt, null);
  }
  assert.deepEqual(tombstones.map((t) => t[0]).sort(), [tombstoneKey(guild.id, a.id), tombstoneKey(guild.id, b.id)].sort());
  assert.ok(tombstones.every((t) => t[1] === year && t[2] >= 86_400));

  // The gateway tells us about the new role; a second run announces and grants nothing.
  a.roles.cache.add(role.id);
  b.roles.cache.add(role.id);
  await tick(guild);
  assert.equal(ch.sent.length, 1);
  assert.equal(a.calls.length, 1);

  // Maintenance: a celebrant who lost the role during the day gets it back.
  b.roles.cache.delete(role.id);
  await tick(guild);
  assert.equal(ch.sent.length, 1);
  assert.equal(b.calls.length, 2);
  assert.equal(a.calls.length, 1);

  // The departed member is picked up once they are back.
  const joined = mkMember(guild, gone.id);
  await tick(guild);
  assert.equal(ch.sent.length, 2);
  assert.ok(ch.sent[1].content.includes(joined.id));
});

test('DB: tick respects the hour, the module toggle and an empty configuration', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  await setup(guild, { birthdayChannelId: ch.id });
  const m = mkMember(guild, U());
  await addRow(guild.id, m.id, todayRow());

  const h = localNow().hour;
  if (h < 23) {
    await updateSettings(guild.id, { birthdayHour: h + 1 });
    await tick(guild);
    assert.equal(ch.sent.length, 0, 'before the announcement hour nothing is claimed');
    assert.equal((await rowOf(guild.id, m.id)).lastAnnouncedYear, null);
  }
  await updateSettings(guild.id, { birthdayHour: 0, birthdaysEnabled: false });
  const off = await tick(guild);
  assert.equal(ch.sent.length, 0, 'module off: no announcements');
  assert.equal((await rowOf(guild.id, m.id)).lastAnnouncedYear, null);
  assert.ok(off.nextRunAt instanceof Date, 'but the job keeps recurring while a channel is set');

  await updateSettings(guild.id, { birthdaysEnabled: true });
  await tick(guild);
  assert.equal(ch.sent.length, 1, 'catch-up: the hour has passed and the row was not celebrated yet');

  // Nothing configured and no marker: the job ends (undefined).
  const empty = mkGuild();
  await setup(empty, {});
  assert.equal(await tick(empty), undefined);
});

test('DB: tick removes expired roles even while the module is off, and clears stale markers', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  const role = mkRole(guild);
  await setup(guild, { birthdayChannelId: ch.id, birthdayRoleId: role.id, birthdaysEnabled: false });
  const yesterday = new Date(Date.now() - 24 * 3_600_000);

  const had = mkMember(guild, U(), { roleIds: [role.id] });
  const lost = mkMember(guild, U()); // already lost the role by hand
  const away = mkMember(guild, U(), { inGuild: false });
  const failing = mkMember(guild, U(), { roleIds: [role.id] });
  failing.failRemove = err(50013);
  const unknown = mkMember(guild, U(), { roleIds: [role.id] });
  unknown.failRemove = err(10011);
  const old = dayBefore();
  for (const m of [had, lost, away, failing, unknown]) await addRow(guild.id, m.id, { ...old, roleGrantedAt: yesterday });

  const res = await tick(guild);
  assert.ok(res.nextRunAt instanceof Date);
  assert.deepEqual(had.calls, [['remove', role.id, 'Birthday is over']]);
  assert.deepEqual(lost.calls, []);
  assert.equal((await rowOf(guild.id, had.id)).roleGrantedAt, null);
  assert.equal((await rowOf(guild.id, lost.id)).roleGrantedAt, null);
  assert.equal((await rowOf(guild.id, away.id)).roleGrantedAt, null, 'a departed member\'s marker is cleared');
  assert.ok((await rowOf(guild.id, failing.id)).roleGrantedAt, 'a permission failure keeps the marker for a retry');
  assert.equal((await rowOf(guild.id, unknown.id)).roleGrantedAt, null, 'Unknown Role: nothing left to remove');
  assert.ok(ch.sent.length === 0);
});

test('DB: tick clears markers when the role is unset or deleted, and gives up on a role it cannot manage', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  await setup(guild, { birthdayChannelId: ch.id, birthdayRoleId: 'deleted-role' });
  const m = mkMember(guild, U(), { roleIds: ['deleted-role'] });
  await addRow(guild.id, m.id, { ...dayBefore(), roleGrantedAt: new Date(Date.now() - 24 * 3_600_000) });
  await tick(guild);
  assert.equal((await rowOf(guild.id, m.id)).roleGrantedAt, null);
  assert.deepEqual(m.calls, []);

  // Role switched off (channel cleared too): the marker is cleared and the job ends.
  const g2 = mkGuild();
  await setup(g2, {});
  const m2 = mkMember(g2, U());
  await addRow(g2.id, m2.id, { ...dayBefore(), roleGrantedAt: new Date(Date.now() - 24 * 3_600_000) });
  assert.equal(await tick(g2), undefined);
  assert.equal((await rowOf(g2.id, m2.id)).roleGrantedAt, null);

  // A role above the bot cannot be removed: the marker is kept, and dropped after a week.
  const g3 = mkGuild();
  const role = mkRole(g3, undefined, { position: 50 });
  await setup(g3, { birthdayRoleId: role.id });
  const recent = mkMember(g3, U(), { roleIds: [role.id] });
  const ancient = mkMember(g3, U(), { roleIds: [role.id] });
  await addRow(g3.id, recent.id, { ...dayBefore(), roleGrantedAt: new Date(Date.now() - 24 * 3_600_000) });
  await addRow(g3.id, ancient.id, { ...dayBefore(), roleGrantedAt: new Date(Date.now() - 8 * 24 * 3_600_000) });
  await tick(g3);
  assert.ok((await rowOf(g3.id, recent.id)).roleGrantedAt);
  assert.equal((await rowOf(g3.id, ancient.id)).roleGrantedAt, null);
  assert.deepEqual(recent.calls, []);
});

test('DB: a failed send reverts the claim and the next run announces', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  const role = mkRole(guild);
  await setup(guild, { birthdayChannelId: ch.id, birthdayRoleId: role.id });
  const m = mkMember(guild, U());
  await addRow(guild.id, m.id, todayRow());
  const year = localNow().year;

  ch.failWith = err(50013);
  const res = await withCache({}, () => tick(guild));
  assert.equal(ch.sent.length, 0);
  assert.equal((await rowOf(guild.id, m.id)).lastAnnouncedYear, year - 1, 'claim given back');
  assert.deepEqual(m.calls, [['add', role.id, 'Birthday']], 'the role is granted anyway');
  assertHourly(res.nextRunAt, 'a send failure waits for the next hour');

  ch.failWith = null;
  m.roles.cache.add(role.id);
  await withCache({}, () => tick(guild));
  assert.equal(ch.sent.length, 1);
  assert.equal((await rowOf(guild.id, m.id)).lastAnnouncedYear, year);
  assert.equal(m.calls.length, 1, 'no second grant');

  // An unexpected error reverts too.
  const g2 = mkGuild();
  const ch2 = mkChannel(g2);
  await setup(g2, { birthdayChannelId: ch2.id });
  const m2 = mkMember(g2, U());
  await addRow(g2.id, m2.id, todayRow());
  ch2.failWith = new Error('socket hang up');
  await tick(g2);
  assert.equal((await rowOf(g2.id, m2.id)).lastAnnouncedYear, year - 1);
});

test('DB: without a usable channel the celebration is completed silently and the role is still granted', { skip: skipDb }, async () => {
  const year = localNow().year;
  for (const variant of ['deleted', 'no-send']) {
    const guild = mkGuild();
    const role = mkRole(guild);
    let channelId = 'deleted-channel';
    let ch = null;
    if (variant === 'no-send') { ch = mkChannel(guild, undefined, { canSend: false }); channelId = ch.id; }
    await setup(guild, { birthdayChannelId: channelId, birthdayRoleId: role.id });
    const m = mkMember(guild, U());
    await addRow(guild.id, m.id, todayRow());
    await tick(guild);
    const row = await rowOf(guild.id, m.id);
    assert.equal(row.lastAnnouncedYear, year, variant);
    assert.ok(row.roleGrantedAt, variant);
    assert.deepEqual(m.calls, [['add', role.id, 'Birthday']], variant);
    if (ch) assert.equal(ch.sent.length, 0);
  }
});

test('DB: a role with dangerous permissions is never granted, a pending member is skipped', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  const role = mkRole(guild, undefined, { perms: P.BanMembers });
  await setup(guild, { birthdayChannelId: ch.id, birthdayRoleId: role.id });
  const m = mkMember(guild, U());
  await addRow(guild.id, m.id, todayRow());
  await tick(guild);
  assert.equal(ch.sent.length, 1, 'the announcement still goes out');
  assert.deepEqual(m.calls, [], 'but the dangerous role is not granted');

  const g2 = mkGuild();
  const ch2 = mkChannel(g2);
  const role2 = mkRole(g2);
  await setup(g2, { birthdayChannelId: ch2.id, birthdayRoleId: role2.id });
  const pending = mkMember(g2, U(), { pending: true });
  await addRow(g2.id, pending.id, todayRow());
  await tick(g2);
  assert.equal(ch2.sent.length, 1);
  assert.deepEqual(pending.calls, []);
});

test('DB: a role grant that fails is retried by maintenance on the next tick', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const role = mkRole(guild);
  await setup(guild, { birthdayRoleId: role.id });
  const m = mkMember(guild, U());
  m.failAdd = err(50013);
  await addRow(guild.id, m.id, todayRow());
  await tick(guild);
  assert.equal(m.calls.length, 1);
  assert.ok((await rowOf(guild.id, m.id)).roleGrantedAt, 'the marker stays so the grant is retried');
  m.failAdd = null;
  await tick(guild);
  assert.equal(m.calls.length, 2);
  m.roles.cache.add(role.id);
  await tick(guild);
  assert.equal(m.calls.length, 2);
});

test('DB: tick claims at most MAX_EVENTS_PER_TICK and continues a minute later', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  await setup(guild, { birthdayChannelId: ch.id });
  const total = MAX_EVENTS_PER_TICK + 2;
  const ids = [];
  for (let i = 0; i < total; i += 1) {
    const m = mkMember(guild, U());
    ids.push(m.id);
    await addRow(guild.id, m.id, todayRow());
  }
  const year = localNow().year;
  const before = Date.now();
  const res = await tick(guild);
  assert.ok(res.nextRunAt.getTime() >= before + CONTINUE_MS - 1000 && res.nextRunAt.getTime() <= Date.now() + CONTINUE_MS + 1000);
  assert.equal(await prisma.birthday.count({ where: { guildId: guild.id, lastAnnouncedYear: year } }), MAX_EVENTS_PER_TICK);
  assert.ok(ch.sent.length >= 1);
  let users = 0;
  for (const s of ch.sent) {
    assert.ok(s.content.length <= 2000);
    assert.ok(s.allowedMentions.users.length <= 50);
    users += s.allowedMentions.users.length;
  }
  assert.equal(users, MAX_EVENTS_PER_TICK);

  const second = await tick(guild);
  assert.equal(await prisma.birthday.count({ where: { guildId: guild.id, lastAnnouncedYear: year } }), total);
  assertHourly(second.nextRunAt, 'all caught up: back to the hourly cadence');
});

test('DB: Feb 29 is celebrated on Feb 28 in non-leap years only', { skip: skipDb }, async () => {
  const run = async (iso) => {
    const guild = mkGuild();
    const ch = mkChannel(guild);
    await setup(guild, { birthdayChannelId: ch.id });
    const leap = mkMember(guild, U());
    const plain = mkMember(guild, U());
    const march = mkMember(guild, U());
    await addRow(guild.id, leap.id, { month: 2, day: 29 });
    await addRow(guild.id, plain.id, { month: 2, day: 28 });
    await addRow(guild.id, march.id, { month: 3, day: 1 });
    await tick(guild, { now: d(iso) });
    const mentioned = ch.sent.flatMap((s) => s.allowedMentions.users);
    return { leap: leap.id, plain: plain.id, march: march.id, mentioned };
  };
  const non = await run('2027-02-28T12:00:00Z');
  assert.deepEqual([...non.mentioned].sort(), [non.leap, non.plain].sort());
  const yes = await run('2028-02-28T12:00:00Z');
  assert.deepEqual(yes.mentioned, [yes.plain]);
  const day = await run('2028-02-29T12:00:00Z');
  assert.deepEqual(day.mentioned, [day.leap]);
});

test('DB: the tick follows the guild time zone', { skip: skipDb }, async () => {
  const tz = 'Pacific/Kiritimati'; // UTC+14: its local date is usually a day ahead of UTC
  const guild = mkGuild();
  const ch = mkChannel(guild);
  await setup(guild, { birthdayChannelId: ch.id, timezone: tz });
  const here = mkMember(guild, U());
  const utcDay = mkMember(guild, U());
  await addRow(guild.id, here.id, todayRow(tz));
  const utc = todayRow('UTC');
  const l = todayRow(tz);
  if (utc.month !== l.month || utc.day !== l.day) await addRow(guild.id, utcDay.id, utc);
  await tick(guild);
  assert.ok(ch.sent[0].allowedMentions.users.includes(here.id));
  if (utc.month !== l.month || utc.day !== l.day) assert.ok(!ch.sent[0].allowedMentions.users.includes(utcDay.id));
  const year = localNow(tz).year;
  assert.equal((await rowOf(guild.id, here.id)).lastAnnouncedYear, year);
});

test('DB: custom messages, age messages and mention safety', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  await setup(guild, {
    birthdayChannelId: ch.id,
    birthdayMessage: '@everyone {user.name} turns... who knows ({server})',
    birthdayMessageAge: '{user} is {age}! <@&123>',
  });
  const year = localNow().year;
  const old = mkMember(guild, U());
  const mystery = mkMember(guild, U());
  const baby = mkMember(guild, U());
  await addRow(guild.id, old.id, { ...todayRow(), year: year - 40 });
  await addRow(guild.id, mystery.id, todayRow());
  await addRow(guild.id, baby.id, { ...todayRow(), year }); // age 0 counts as unknown
  await tick(guild);
  const text = ch.sent.map((s) => s.content).join('\n');
  assert.ok(text.includes(`<@${old.id}> is 40! <@&123>`));
  assert.ok(text.includes(`${mystery.user.username} turns... who knows (Test Guild)`));
  assert.ok(text.includes(`${baby.user.username} turns... who knows (Test Guild)`));
  for (const s of ch.sent) {
    assert.deepEqual(s.allowedMentions.parse, [], '@everyone and roles never ping');
    assert.ok(!('roles' in s.allowedMentions));
  }
});

test('DB: runBirthdayTick: the running guard, the mutex and error handling', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  await setup(guild, { birthdayChannelId: ch.id });
  const m = mkMember(guild, U());
  await addRow(guild.id, m.id, todayRow());
  const job = { guildId: guild.id };
  const ctx = { client: {}, guild };
  const before = Date.now();
  const [r1, r2] = await Promise.all([svc.runBirthdayTick(job, ctx), svc.runBirthdayTick(job, ctx)]);
  assertHourly(r1.nextRunAt, 'the first run does the work');
  assert.ok(r2.nextRunAt.getTime() >= before + CONTINUE_MS - 1000 && r2.nextRunAt.getTime() <= before + CONTINUE_MS + 5000, 'the overlapping run defers');
  assert.equal(ch.sent.length, 1, 'announced once');

  // A broken guild object never makes the handler throw.
  const broken = await svc.runBirthdayTick(job, { client: {}, guild: { id: guild.id, available: true } });
  assert.ok(broken.nextRunAt.getTime() >= before + ERROR_RETRY_MS - 1000);
  // ...and the guard is released afterwards.
  const again = await svc.runBirthdayTick(job, ctx);
  assertHourly(again.nextRunAt, 'the guard is released');
});

// ── Commands end to end ──────────────────────────────────────────────────────

function mkInteraction(guild, { sub, user, member, options = {}, channel = null }) {
  const i = {
    guild, guildId: guild.id, user, member, channel,
    deferred: false, replied: false, ephemeral: undefined, edits: [], deferCalls: 0,
    inCachedGuild: () => true,
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => null,
      get: (n) => options[n] ?? null,
      getString: (n) => options[n] ?? null,
      getUser: (n) => options[n] ?? null,
      getMember: (n) => options[`${n}:member`] ?? null,
      getChannel: (n) => options[n] ?? null,
      getRole: (n) => options[n] ?? null,
      getInteger: (n) => options[n] ?? null,
      getBoolean: (n) => options[n] ?? null,
    },
    async deferReply({ ephemeral } = {}) { i.deferCalls += 1; i.deferred = true; i.ephemeral = Boolean(ephemeral); },
    async editReply(p) { i.edits.push(p); return { id: 'm' }; },
    async followUp(p) { i.edits.push(p); return { id: 'm' }; },
    async reply(p) { i.replied = true; i.edits.push(p); return {}; },
  };
  return i;
}
const lastEmbed = (i) => i.edits.at(-1).embeds[0].data;
const titleOf = (i) => lastEmbed(i).title;
const descOf = (i) => lastEmbed(i).description ?? '';

function mkUser(id = U(), bot = false) {
  return { id, bot, username: `name${id.slice(-3)}`, toString: () => `<@${id}>` };
}

function mkActor(guild, { admin = false } = {}) {
  const id = admin ? ADMIN : U();
  const member = mkMember(guild, id, { position: admin ? 30 : 1 });
  member.user.toString = () => `<@${id}>`;
  return { user: member.user, member };
}

test('DB: /birthday set, show, next and forget', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  await setup(guild, { birthdayChannelId: ch.id, birthdayHour: 9 });
  const me = mkActor(guild);
  const run = async (sub, options = {}, actor = me) => {
    const i = mkInteraction(guild, { sub, user: actor.user, member: actor.member, options });
    await birthdayCmd.execute(i, {});
    return i;
  };

  // set: private, saved, confirmed with the year (only here).
  let i = await run('set', { date: '1998-03-14' });
  assert.equal(i.ephemeral, true);
  assert.equal(i.deferCalls, 1);
  assert.match(titleOf(i), /Birthday Saved/);
  assert.match(descOf(i), /I'll celebrate your birthday on \*\*March 14, 1998\*\*/);
  assert.match(descOf(i), /announced in <#\d+> at \*\*09:00\*\* \(UTC\)|celebrated at|already celebrated/);
  assert.match(descOf(i), /Your age will be included/);
  assert.deepEqual(i.edits.at(-1).allowedMentions, { parse: [] });
  assert.equal((await rowOf(guild.id, me.user.id)).year, 1998);

  i = await run('set', { date: '03-15' });
  assert.match(titleOf(i), /Birthday Updated/);
  assert.match(descOf(i), /Your birthday is now \*\*March 15\*\*/);
  assert.ok(!/Your age will be included/.test(descOf(i)));

  i = await run('set', { date: '1999-02-29' });
  assert.match(titleOf(i), /Invalid Date/);
  assert.match(descOf(i), /leap year/);
  i = await run('set', { date: 'tomorrow' });
  assert.match(descOf(i), /MM-DD/);

  await updateSettings(guild.id, { birthdayRequireYear: true });
  i = await run('set', { date: '03-15' });
  assert.match(descOf(i), /birth year/);
  await updateSettings(guild.id, { birthdayRequireYear: false });

  // show: public, month and day only.
  i = await run('show', { date: undefined });
  assert.equal(i.ephemeral, false);
  assert.match(titleOf(i), /Birthday/);
  assert.match(descOf(i), /birthday is \*\*March 15\*\*/);
  assert.ok(!/1998|1999|age/i.test(descOf(i)), 'never the year or the age');
  assert.match(descOf(i), /It's today|Next: <t:\d+:D> \(<t:\d+:R>\)/);
  assert.match(lastEmbed(i).footer.text, /Server time zone: UTC/);

  const other = mkActor(guild);
  i = await run('show', { member: other.user, 'member:member': other.member });
  assert.match(titleOf(i), /No Birthday Saved/);
  assert.match(descOf(i), /hasn't shared their birthday/);
  i = await run('show', { member: other.user, 'member:member': null });
  assert.match(titleOf(i), /Not a Member/);
  const botUser = mkUser(U(), true);
  i = await run('show', { member: botUser, 'member:member': { id: botUser.id, user: botUser, toString: () => `<@${botUser.id}>` } });
  assert.match(titleOf(i), /No Birthday Saved/);

  // next: public list.
  const second = mkActor(guild);
  await run('set', { date: '12-24' }, second);
  i = await run('next');
  assert.equal(i.ephemeral, false);
  assert.match(titleOf(i), /Upcoming Birthdays/);
  assert.ok(descOf(i).includes(`<@${me.user.id}>`) && descOf(i).includes(`<@${second.user.id}>`));
  assert.ok(!/1998/.test(descOf(i)));
  const empty = mkGuild();
  await setup(empty, {});
  const lone = mkActor(empty);
  i = mkInteraction(empty, { sub: 'next', user: lone.user, member: lone.member });
  await birthdayCmd.execute(i, {});
  assert.match(titleOf(i), /No Birthdays Yet/);

  // forget: private.
  i = await run('forget');
  assert.equal(i.ephemeral, true);
  assert.match(titleOf(i), /Birthday Forgotten/);
  assert.equal(await rowOf(guild.id, me.user.id), null);
  i = await run('forget');
  assert.match(titleOf(i), /Nothing to Forget/);
  i = await run('show');
  assert.match(titleOf(i), /No Birthday Saved/);
  assert.match(descOf(i), /\/birthday set/);
});

test('DB: /birthday set today after the hour is "late"; forget-then-set cannot repeat the celebration', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  await setup(guild, { birthdayChannelId: ch.id, birthdayHour: 0 }); // the hour has always passed
  const me = mkActor(guild);
  const t = todayRow();
  const date = `${String(t.month).padStart(2, '0')}-${String(t.day).padStart(2, '0')}`;
  const run = async (sub, options = {}) => {
    const i = mkInteraction(guild, { sub, user: me.user, member: me.member, options });
    await withCache({}, () => birthdayCmd.execute(i, {}));
    return i;
  };
  let i = await run('set', { date });
  assert.match(descOf(i), /Today's birthdays were celebrated at \*\*00:00\*\*, so I'll celebrate you from next year/);
  assert.equal((await rowOf(guild.id, me.user.id)).lastAnnouncedYear, localNow().year);

  // The tick finds nothing to announce for a late registration.
  await tick(guild);
  assert.equal(ch.sent.length, 0);

  // Forget, then set again with the tombstone present.
  await run('forget');
  const tomb = [];
  i = mkInteraction(guild, { sub: 'set', user: me.user, member: me.member, options: { date } });
  await withCache({ get: async (k) => { tomb.push(k); return localNow().year; } }, () => birthdayCmd.execute(i, {}));
  assert.equal(tomb[0], tombstoneKey(guild.id, me.user.id));
  assert.match(descOf(i), /already celebrated this year/);
  await tick(guild);
  assert.equal(ch.sent.length, 0);
});

test('DB: /birthday-config channel, hour, require-year and the module-off note', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const text = mkChannel(guild);
  const quiet2 = mkChannel(guild, undefined, { canSend: false });
  await setup(guild, {});
  const admin = mkActor(guild, { admin: true });
  const run = async (sub, options = {}) => {
    const i = mkInteraction(guild, { sub, user: admin.user, member: admin.member, options, channel: text });
    await configCmd.execute(i, {});
    return i;
  };
  const job = () => prisma.scheduledJob.findUnique({ where: { dedupeKey: tickKey(guild.id) } });
  const settingsRow = () => prisma.guildSettings.findUnique({ where: { guildId: guild.id } });

  // Public replies (admin convention), mentions off.
  let i = await run('channel', { channel: text });
  assert.equal(i.ephemeral, false);
  assert.match(titleOf(i), /Birthday Channel Set/);
  assert.match(descOf(i), /at \*\*00:00\*\* \(UTC\)/);
  assert.deepEqual(i.edits.at(-1).allowedMentions, { parse: [] });
  assert.equal((await settingsRow()).birthdayChannelId, text.id);
  assert.ok(await job(), 'the tick job exists after a channel was set');

  i = await run('channel', { channel: quiet2 });
  assert.match(titleOf(i), /Cannot Use Channel/);
  assert.match(descOf(i), /Send Messages/);
  assert.equal((await settingsRow()).birthdayChannelId, text.id, 'not saved');

  i = await run('hour', { hour: 7 });
  assert.match(titleOf(i), /Announcement Hour Set/);
  assert.match(descOf(i), /\*\*07:00\*\*/);
  assert.equal((await settingsRow()).birthdayHour, 7);

  i = await run('require-year', { enabled: true });
  assert.match(titleOf(i), /Birth Year Required/);
  assert.equal((await settingsRow()).birthdayRequireYear, true);
  await addRow(guild.id, U(), {});
  i = await run('require-year', { enabled: true });
  assert.match(descOf(i), /1 saved birthday\(s\) without a year are kept/);
  i = await run('require-year', { enabled: false });
  assert.match(titleOf(i), /Birth Year Optional/);

  // The module is off: the command still works and says so.
  await updateSettings(guild.id, { birthdaysEnabled: false });
  i = await run('hour', { hour: 8 });
  assert.match(descOf(i), /module is off/);
  assert.match(descOf(i), /\/module enable birthdays/);
  assert.equal((await settingsRow()).birthdayHour, 8);
  await updateSettings(guild.id, { birthdaysEnabled: true });

  // Clearing the channel with no role cancels the job.
  i = await run('channel');
  assert.match(titleOf(i), /Birthday Announcements Off/);
  assert.equal((await settingsRow()).birthdayChannelId, null);
  assert.equal(await job(), null);
});

test('DB: /birthday-config hour reports a catch-up when the hour has already passed', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  await setup(guild, { birthdayChannelId: ch.id });
  const admin = mkActor(guild, { admin: true });
  const i = mkInteraction(guild, { sub: 'hour', user: admin.user, member: admin.member, options: { hour: 0 }, channel: ch });
  await configCmd.execute(i, {});
  assert.match(descOf(i), /haven't been celebrated yet will be celebrated in a moment/);
  const none = mkGuild();
  await setup(none, {});
  const a2 = mkActor(none, { admin: true });
  const j = mkInteraction(none, { sub: 'hour', user: a2.user, member: a2.member, options: { hour: 0 } });
  await configCmd.execute(j, {});
  assert.ok(!/in a moment/.test(descOf(j)), 'nothing configured: no catch-up promise');
});

test('DB: /birthday-config role: validation, transfer to a new role, and clearing', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  const roleA = mkRole(guild, undefined, { position: 5 });
  const roleB = mkRole(guild, undefined, { position: 6 });
  const tooHigh = mkRole(guild, undefined, { position: 40 }); // above the bot (10) and the admin (30)
  const risky = mkRole(guild, undefined, { position: 4, perms: P.KickMembers });
  await setup(guild, { birthdayChannelId: ch.id });
  const admin = mkActor(guild, { admin: true });
  const run = async (options) => {
    const i = mkInteraction(guild, { sub: 'role', user: admin.user, member: admin.member, options, channel: ch });
    await configCmd.execute(i, {});
    return i;
  };
  const settingsRow = () => prisma.guildSettings.findUnique({ where: { guildId: guild.id } });

  let i = await run({ role: tooHigh });
  assert.match(titleOf(i), /Cannot Use Role/);
  i = await run({ role: risky });
  assert.match(titleOf(i), /Cannot Use Role/);
  assert.match(descOf(i), /dangerous permissions/);
  assert.equal((await settingsRow()).birthdayRoleId, null, 'not saved');

  i = await run({ role: roleA });
  assert.match(titleOf(i), /Birthday Role Set/);
  assert.match(descOf(i), /until the end of that day/);
  assert.equal((await settingsRow()).birthdayRoleId, roleA.id);
  i = await run({ role: roleA });
  assert.match(titleOf(i), /No Change/);

  // Today's celebrant holds roleA; switching to roleB takes it away.
  const m = mkMember(guild, U(), { roleIds: [roleA.id] });
  await addRow(guild.id, m.id, { roleGrantedAt: new Date() });
  const stuck = mkMember(guild, U(), { roleIds: [roleA.id] });
  stuck.failRemove = err(50013);
  await addRow(guild.id, stuck.id, { roleGrantedAt: new Date() });
  i = await run({ role: roleB });
  assert.match(titleOf(i), /Birthday Role Set/);
  assert.match(descOf(i), /removed the previous birthday role from 1 member/);
  assert.match(descOf(i), /couldn't remove the previous birthday role from 1 member/);
  assert.deepEqual(m.calls, [['remove', roleA.id, 'Birthday role changed']]);
  assert.equal((await settingsRow()).birthdayRoleId, roleB.id);

  // Cleared: the markers go and, with the channel still set, the job stays.
  i = await run({});
  assert.match(titleOf(i), /Birthday Role Off/);
  assert.equal((await settingsRow()).birthdayRoleId, null);
  assert.equal(await svc.hasMarkers(guild.id), false);
  i = await run({});
  assert.match(titleOf(i), /Birthday Role Off/, 'clearing an unset role is harmless');
});

test('DB: /birthday-config message and message-age: preview, warnings, reset', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  await setup(guild, {});
  const admin = mkActor(guild, { admin: true });
  const run = async (sub, options = {}) => {
    const i = mkInteraction(guild, { sub, user: admin.user, member: admin.member, options, channel: ch });
    await configCmd.execute(i, {});
    return i;
  };
  const settingsRow = () => prisma.guildSettings.findUnique({ where: { guildId: guild.id } });

  let i = await run('message', { text: 'Cheers {user}! {nope} {age}' });
  assert.match(titleOf(i), /Birthday Message Updated/);
  assert.match(descOf(i), /Preview:\nCheers <@900000000000000002>!/);
  assert.match(descOf(i), /Unknown variables \(sent as typed\): `\{nope\}`/);
  assert.match(descOf(i), /`\{age\}` is always empty in this message/);
  assert.equal((await settingsRow()).birthdayMessage, 'Cheers {user}! {nope} {age}', 'warnings never block saving');

  i = await run('message-age', { text: 'Wow {age}!' });
  assert.match(titleOf(i), /Birthday Message \(With Age\) Updated/);
  assert.match(descOf(i), /Preview:\nWow 25!/);
  assert.ok(!/Tip:|Unknown/.test(descOf(i)));
  i = await run('message-age', { text: 'No number here' });
  assert.match(descOf(i), /Tip: add `\{age\}`/);

  i = await run('message', { text: '   ' });
  assert.match(titleOf(i), /Birthday Message Reset/);
  assert.ok(descOf(i).includes(DEFAULT_BIRTHDAY_MESSAGE));
  assert.equal((await settingsRow()).birthdayMessage, null);
  i = await run('message-age');
  assert.ok(descOf(i).includes(DEFAULT_BIRTHDAY_MESSAGE_AGE));
  assert.equal((await settingsRow()).birthdayMessageAge, null);
});

test('DB: /birthday-config set-user, unset-user, show and test', { skip: skipDb }, async () => {
  const guild = mkGuild();
  const ch = mkChannel(guild);
  const role = mkRole(guild);
  await setup(guild, { birthdayChannelId: ch.id, birthdayRoleId: role.id, birthdayRequireYear: true });
  const admin = mkActor(guild, { admin: true });
  const run = async (sub, options = {}) => {
    const i = mkInteraction(guild, { sub, user: admin.user, member: admin.member, options, channel: ch });
    await configCmd.execute(i, {});
    return i;
  };

  const target = mkMember(guild, U());
  // MM-DD is accepted even though members must give a year.
  let i = await run('set-user', { 'member:member': target, date: '04-05' });
  assert.match(titleOf(i), /Birthday Saved/);
  assert.match(descOf(i), /birthday is now \*\*April 5\*\*/);
  assert.ok(!/\b(19|20)\d{2}\b/.test(descOf(i)), 'the public reply never carries a year');
  assert.ok(!/age will be shown/.test(descOf(i)), 'no year given: no age in the announcement');
  i = await run('set-user', { 'member:member': target, date: '1990-04-06' });
  assert.match(descOf(i), /\*\*April 6\*\*/);
  assert.ok(!/1990/.test(descOf(i)));
  assert.match(descOf(i), /their age will be shown in the public announcement/, 'a year puts the age in the announcement and the admin is told');
  assert.equal((await rowOf(guild.id, target.id)).year, 1990);
  i = await run('set-user', { 'member:member': target, date: '13-40' });
  assert.match(titleOf(i), /Invalid Date/);
  i = await run('set-user', { 'member:member': null, date: '04-05' });
  assert.match(titleOf(i), /Not a Member/);
  i = await run('set-user', { 'member:member': mkMember(guild, U(), { bot: true }), date: '04-05' });
  assert.match(titleOf(i), /Not Allowed/);

  // show
  i = await run('show');
  assert.equal(i.ephemeral, false);
  assert.match(titleOf(i), /Birthday Settings/);
  const fields = Object.fromEntries(lastEmbed(i).fields.map((f) => [f.name, f.value]));
  assert.equal(fields.Module, '✅ On');
  assert.equal(fields.Channel, `<#${ch.id}>`);
  assert.equal(fields.Role, `<@&${role.id}>`);
  assert.equal(fields['Announcement hour'], '**00:00** (UTC)');
  assert.equal(fields['Birth year'], 'Required');
  assert.match(fields['Saved birthdays'], /^1 \(1 without a year\)$|^1$/);
  assert.match(fields['Next check'], /^<t:\d+:R>$/, 'configured: the job is scheduled (self-healed)');
  assert.ok(fields['Message (age unknown)'].includes(DEFAULT_BIRTHDAY_MESSAGE) && fields['Message (age unknown)'].includes('(default)'));
  assert.ok(fields['Message (age known)'].includes(DEFAULT_BIRTHDAY_MESSAGE_AGE));

  // Problems are reported: a deleted channel and role, a module that is off.
  guild.channels.cache.delete(ch.id);
  guild.roles.cache.delete(role.id);
  await updateSettings(guild.id, { birthdaysEnabled: false });
  i = await run('show');
  const f2 = Object.fromEntries(lastEmbed(i).fields.map((f) => [f.name, f.value]));
  assert.match(f2.Channel, /Deleted channel/);
  assert.match(f2.Role, /Deleted role/);
  assert.match(f2.Module, /Off/);
  await updateSettings(guild.id, { birthdaysEnabled: true });
  guild.channels.cache.set(ch.id, ch);
  guild.roles.cache.set(role.id, role);

  // test: private preview with both messages.
  i = await run('test');
  assert.equal(i.ephemeral, true);
  assert.match(titleOf(i), /Birthday Preview/);
  assert.ok(descOf(i).includes('Without a birth year'));
  assert.ok(descOf(i).includes(`Happy birthday <@${ADMIN}>! 🎂`));
  assert.ok(descOf(i).includes(`Happy birthday <@${ADMIN}>! They are now 25! 🎂`));
  assert.ok(descOf(i).includes(`Posted in <#${ch.id}>`));

  // unset-user also works for someone who is not in the guild any more.
  const ghost = mkUser(U());
  await addRow(guild.id, ghost.id, {});
  i = await run('unset-user', { member: ghost });
  assert.match(titleOf(i), /Birthday Removed/);
  assert.equal(await rowOf(guild.id, ghost.id), null);
  i = await run('unset-user', { member: ghost });
  assert.match(titleOf(i), /Nothing to Remove/);

  // A write failure answers "Could Not Save"; the interaction never throws.
  const realUpsert = prisma.birthday.upsert;
  prisma.birthday.upsert = async () => { throw new Error('db down'); };
  try {
    i = await run('set-user', { 'member:member': target, date: '05-05' });
    assert.match(titleOf(i), /Could Not Save/);
  } finally {
    prisma.birthday.upsert = realUpsert;
  }
});
