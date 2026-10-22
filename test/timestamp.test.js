// /timestamp, /timezone and /settings timezone-role: the free-form time parser, timezone input and
// role-name reading, the resolution order, the reply builders and the command payloads. Offline.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails fast and Postgres is never reached.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const quiet = { log: console.log, info: console.info, warn: console.warn, error: console.error, debug: console.debug };
for (const k of Object.keys(quiet)) console[k] = () => {};

const { parseWhen, zoneFromAbbreviation, zoneFromOffset, ZONE_ABBREVIATIONS, isValidTimeZone } = await import('../src/utils/time.js');
const tz = await import('../src/utils/timezones.js');
const ts = await import('../src/commands/utility/timestamp.js');
const tzCmd = await import('../src/commands/utility/timezone.js');
const settingsCmd = await import('../src/commands/admin/settings.js');
const userTz = await import('../src/services/userTimezoneService.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { default: redis } = await import('../src/services/redis.js');

after(() => {
  Object.assign(console, quiet);
  redis.disconnect();
});

const CHI = 'America/Chicago';
const FRI_3PM_CHICAGO = new Date('2026-10-16T20:00:00Z'); // Friday Oct 16 2026, 15:00 CDT
const iso = (q, zone = CHI, now = FRI_3PM_CHICAGO) => parseWhen(q, zone, { now })?.date.toISOString() ?? null;

// ── parseWhen ─────────────────────────────────────────────────────────────────

test('weekdays: today counts while the time is ahead, next skips today, either word order', () => {
  assert.equal(iso('fri 7pm'), '2026-10-17T00:00:00.000Z'); // today 19:00 CDT
  assert.equal(iso('7pm fri'), '2026-10-17T00:00:00.000Z');
  assert.equal(iso('friday at 19:00'), '2026-10-17T00:00:00.000Z');
  assert.equal(iso('fri 2pm'), '2026-10-23T19:00:00.000Z'); // 2 pm already passed today
  assert.equal(iso('next fri 7pm'), '2026-10-24T00:00:00.000Z');
  assert.equal(iso('7pm next fri'), '2026-10-24T00:00:00.000Z');
  assert.equal(iso('mon'), '2026-10-19T05:00:00.000Z'); // midnight Monday
  // Wraps the week: on a Saturday, "fri" is six days later.
  assert.equal(iso('fri 9am', CHI, new Date('2026-10-17T15:00:00Z')), '2026-10-23T14:00:00.000Z');
  assert.equal(iso('fri 25:00'), null);
});

test('day words, month names and the year rules', () => {
  assert.equal(iso('tomorrow 9am'), '2026-10-17T14:00:00.000Z');
  assert.equal(iso('tmrw 7:30 pm'), '2026-10-18T00:30:00.000Z');
  assert.equal(iso('today'), '2026-10-16T05:00:00.000Z'); // past times are kept
  assert.equal(iso('yesterday noon'), '2026-10-15T17:00:00.000Z');
  assert.equal(iso('oct 20 9am'), '2026-10-20T14:00:00.000Z');
  assert.equal(iso('20 oct'), '2026-10-20T05:00:00.000Z');
  assert.equal(iso('oct 16'), '2026-10-16T05:00:00.000Z'); // today, not next year
  assert.equal(iso('oct 10'), '2027-10-10T05:00:00.000Z'); // already over this year
  assert.equal(iso('october 20th, 2027 7:30pm'), '2027-10-21T00:30:00.000Z');
  assert.equal(iso('feb 29'), '2028-02-29T06:00:00.000Z');
  assert.equal(iso('feb 30 2027'), null);
  // New Year: "tomorrow" on Dec 31 is Jan 1 of the next year.
  assert.equal(iso('tomorrow 10:00', 'UTC', new Date('2026-12-31T12:00:00Z')), '2027-01-01T10:00:00.000Z');
});

test('daylight saving: a gap gives the first valid instant, an overlap the earlier one', () => {
  assert.equal(iso('mar 8 2026 2:30am'), '2026-03-08T08:00:00.000Z'); // 02:30 does not exist; 03:00 CDT
  assert.equal(iso('nov 1 2026 1:30am'), '2026-11-01T06:30:00.000Z'); // first 01:30 (CDT)
  assert.equal(iso('jul 4 2027 noon'), '2027-07-04T17:00:00.000Z'); // CDT
  assert.equal(iso('jan 4 2027 noon'), '2027-01-04T18:00:00.000Z'); // CST
});

test('a trailing zone wins over the default and is reported', () => {
  const cst = parseWhen('7pm cst', 'Europe/London', { now: FRI_3PM_CHICAGO });
  assert.deepEqual([cst.date.toISOString(), cst.zone, cst.zoneFromInput], ['2026-10-17T00:00:00.000Z', CHI, true]);
  assert.equal(parseWhen('7pm EST', CHI, { now: FRI_3PM_CHICAGO }).zone, 'America/New_York');
  assert.equal(parseWhen('tmrw 7:30 pm pt', CHI, { now: FRI_3PM_CHICAGO }).zone, 'America/Los_Angeles');
  const off = parseWhen('fri 7pm utc +2', CHI, { now: FRI_3PM_CHICAGO });
  assert.deepEqual([off.date.toISOString(), off.zone], ['2026-10-23T17:00:00.000Z', 'Etc/GMT-2']); // 7 pm already passed there
  const city = parseWhen('2026-10-20 18:00 europe/london', CHI, { now: FRI_3PM_CHICAGO });
  assert.deepEqual([city.date.toISOString(), city.zone], ['2026-10-20T17:00:00.000Z', 'Europe/London']);
  const plain = parseWhen('fri 7pm', CHI, { now: FRI_3PM_CHICAGO });
  assert.deepEqual([plain.zone, plain.zoneFromInput], [CHI, false]);
});

test('parseDateInput forms still work, junk does not', () => {
  assert.equal(iso('now'), FRI_3PM_CHICAGO.toISOString());
  assert.equal(iso('in 2h'), '2026-10-16T22:00:00.000Z');
  assert.equal(iso('<t:1792278000:F>'), '2026-10-17T23:00:00.000Z');
  assert.equal(iso('at 7pm'), '2026-10-17T00:00:00.000Z');
  for (const bad of ['', 'est', 'banana 7pm', '10/12', 'x'.repeat(101), '0001-01-01']) assert.equal(iso(bad), null, bad);
  assert.equal(parseWhen('fri 7pm', 'Not/AZone', { now: FRI_3PM_CHICAGO }).zone, 'UTC');
});

// ── Abbreviations, offsets and typed zones ────────────────────────────────────

test('every abbreviation names a real Region/City zone, and none is ambiguous', () => {
  for (const [abbr, zone] of Object.entries(ZONE_ABBREVIATIONS)) {
    assert.ok(isValidTimeZone(zone) && zone.includes('/'), `${abbr} -> ${zone}`);
  }
  for (const ambiguous of ['ist', 'ast']) assert.equal(Object.hasOwn(ZONE_ABBREVIATIONS, ambiguous), false);
  assert.equal(zoneFromAbbreviation(' CST '), CHI);
  assert.equal(zoneFromAbbreviation('gmt'), 'UTC');
  assert.equal(zoneFromAbbreviation('toString'), null);
  assert.equal(zoneFromOffset('UTC+2'), 'Etc/GMT-2');
  assert.equal(zoneFromOffset('gmt-5'), 'Etc/GMT+5');
  assert.equal(zoneFromOffset('utc+05:00'), 'Etc/GMT-5');
  assert.equal(zoneFromOffset('utc+0'), 'UTC');
  for (const bad of ['utc+5:30', 'utc+15', 'utc-13', '+2', 'utc']) assert.equal(zoneFromOffset(bad), null, bad);
});

test('parseZoneInput: abbreviations and Region/City accepted, offsets and ambiguous names refused', () => {
  assert.deepEqual(tz.parseZoneInput('cst'), { ok: true, zone: CHI, abbreviation: 'CST' });
  assert.deepEqual(tz.parseZoneInput('america/chicago'), { ok: true, zone: CHI, abbreviation: null });
  assert.deepEqual(tz.parseZoneInput('UTC'), { ok: true, zone: 'UTC', abbreviation: null });
  assert.equal(tz.parseZoneInput('IST').code, 'AMBIGUOUS');
  assert.deepEqual(tz.parseZoneInput('ist').options, ['Asia/Kolkata', 'Europe/Dublin', 'Asia/Jerusalem']);
  assert.equal(tz.parseZoneInput('-6').code, 'OFFSET');
  assert.equal(tz.parseZoneInput('Japan').code, 'ABBREVIATION');
  assert.equal(tz.parseZoneInput('Mars/Olympus').code, 'UNKNOWN');
  assert.equal(tz.parseZoneInput('').code, 'EMPTY');
  for (const code of ['EMPTY', 'UNKNOWN', 'OFFSET', 'ABBREVIATION']) {
    const [title, text] = tz.zoneInputError('x`y', { code });
    assert.ok(title && text && !text.includes('x`y'), code);
  }
  assert.match(tz.zoneInputError('IST', tz.parseZoneInput('IST'))[1], /Asia\/Kolkata/);
});

test('zoneFromRoleName reads abbreviations first, then a whole-hour offset', () => {
  assert.equal(tz.zoneFromRoleName('CST'), CHI);
  assert.equal(tz.zoneFromRoleName('🇺🇸 Central (CST/CDT)'), CHI);
  assert.equal(tz.zoneFromRoleName('PST (UTC-8)'), 'America/Los_Angeles');
  assert.equal(tz.zoneFromRoleName('GMT+1'), 'Etc/GMT-1');
  assert.equal(tz.zoneFromRoleName('UTC'), 'UTC');
  assert.equal(tz.zoneFromRoleName('EST or PST'), null); // two zones
  assert.equal(tz.zoneFromRoleName('Gamers'), null);
  assert.equal(tz.zoneFromRoleName(undefined), null);
});

test('searchTimeZones only puts abbreviations first when asked', () => {
  assert.equal(tz.searchTimeZones('cst', { abbreviations: true })[0].value, CHI);
  assert.match(tz.searchTimeZones('cst', { abbreviations: true })[0].name, /· CST$/);
  assert.equal(tz.searchTimeZones('cst').some((c) => c.value === CHI), false); // /settings timezone is unchanged
  assert.equal(settingsCmd.searchTimeZones, tz.searchTimeZones); // re-exported for the dashboard and tests
});

// ── Timezone roles and resolution ─────────────────────────────────────────────

const R1 = '111111111111111111';
const R2 = '222222222222222222';

test('normalizeTimezoneRoles keeps valid, unique links in order', () => {
  assert.deepEqual(tz.normalizeTimezoneRoles(null), []);
  assert.deepEqual(tz.normalizeTimezoneRoles([
    { roleId: R1, zone: CHI }, { roleId: R1, zone: 'UTC' }, { roleId: 'abc', zone: CHI },
    { roleId: R2, zone: 'Not/AZone' }, null, { roleId: R2, zone: 'Europe/London' },
  ]), [{ roleId: R1, zone: CHI }, { roleId: R2, zone: 'Europe/London' }]);
});

test('resolveTimezone: own zone, then highest timezone role, then server, then UTC', () => {
  const settings = { timezone: 'Europe/Paris', timezoneRoles: [{ roleId: R1, zone: CHI }, { roleId: R2, zone: 'Asia/Tokyo' }] };
  const position = (id) => (id === R2 ? 5 : 1);
  assert.deepEqual(tz.resolveTimezone({ personal: 'Europe/London', memberRoleIds: [R1], settings }),
    { zone: 'Europe/London', source: 'personal', roleId: R1, roleCount: 1 });
  assert.deepEqual(tz.resolveTimezone({ memberRoleIds: [R1, R2], rolePosition: position, settings }),
    { zone: 'Asia/Tokyo', source: 'role', roleId: R2, roleCount: 2 });
  assert.equal(tz.resolveTimezone({ memberRoleIds: ['333333333333333333'], settings }).source, 'server');
  assert.deepEqual(tz.resolveTimezone({ settings: { timezone: 'bogus' } }), { zone: 'UTC', source: 'default', roleId: null, roleCount: 0 });
  assert.equal(tz.resolveTimezone().zone, 'UTC');
});

test('resolveMemberTimezone reads cached members and raw interaction members', () => {
  const settings = { timezone: 'UTC', timezoneRoles: [{ roleId: R1, zone: CHI }] };
  const cached = { roles: { cache: new Map([[R1, {}]]) } };
  assert.equal(userTz.resolveMemberTimezone({ member: cached, settings }).zone, CHI);
  assert.equal(userTz.resolveMemberTimezone({ member: { roles: [R1] }, settings }).zone, CHI);
  assert.equal(userTz.resolveMemberTimezone({ member: null, settings }).source, 'server');
  assert.deepEqual(userTz.memberRoleIds(undefined), []);
});

test('getUserTimezone with cacheOnly and Redis down answers unknown, never throws', async () => {
  userTz.__resetUserTimezoneMemo();
  assert.equal(userTz.peekUserTimezone('1'), undefined);
  assert.equal(await userTz.getUserTimezone('1', { cacheOnly: true }), undefined);
});

// ── Replies ───────────────────────────────────────────────────────────────────

test('suggestWhen previews a reading, or examples, or a hint', () => {
  const [one] = ts.suggestWhen('fri 7pm', CHI, FRI_3PM_CHICAGO);
  assert.equal(one.value, 'fri 7pm');
  assert.match(one.name, /^Fri, Oct 16, 2026, 7:00 PM · America\/Chicago \(UTC-05:00\) · in 4 hours$/);
  const examples = ts.suggestWhen('', CHI, FRI_3PM_CHICAGO);
  assert.equal(examples.length, ts.EXAMPLES.length);
  assert.ok(examples.every((c) => c.name.length <= 100 && ts.EXAMPLES.includes(c.value)));
  const [hint] = ts.suggestWhen('banana', CHI, FRI_3PM_CHICAGO);
  assert.equal(hint.value, 'banana');
  assert.match(hint.name, /Can't read/);
  // The date in the hint is computed from now, so it is never in the past.
  assert.match(hint.name, /Try: fri 7pm · tomorrow 18:30 · oct 23 9am · in 2h$/);
  const [late] = ts.suggestWhen('banana', CHI, new Date('2026-12-28T12:00:00Z'));
  assert.match(late.name, /· jan 4 9am · in 2h$/);
  assert.ok(ts.suggestWhen('x'.repeat(60), CHI, new Date('2026-11-28T12:00:00Z'))[0].name.length <= 100);
  assert.deepEqual(ts.futureDayExample(CHI, FRI_3PM_CHICAGO), { words: 'oct 23', iso: '2026-10-23' });
  assert.deepEqual(ts.futureDayExample('Pacific/Auckland', new Date('2026-12-28T20:00:00Z')), { words: 'jan 5', iso: '2027-01-05' }); // already Dec 29 there
  assert.equal(ts.roughRelative(-2 * 86_400_000), '2 days ago');
  assert.equal(ts.roughRelative(20_000), 'now');
});

test('the private reply names the source and lists every style, the chosen one first', () => {
  const parsed = parseWhen('fri 7pm', CHI, { now: FRI_3PM_CHICAGO });
  const resolved = { zone: CHI, source: 'role', roleId: R1, roleCount: 2 };
  const text = ts.buildTimestampEmbed(parsed, resolved, { style: 'R' }).data.description;
  assert.match(text, new RegExp(`your <@&${R1}> role`));
  assert.match(text, /several timezone roles/);
  const codes = [...text.matchAll(/`<t:\d+:([tTdDfFR])>`/g)].map((m) => m[1]);
  assert.deepEqual(codes, ['R', 'F', 'f', 'D', 'd', 't', 'T']);
  const server = ts.buildTimestampEmbed(parsed, { zone: CHI, source: 'server', roleId: null, roleCount: 0 }).data.description;
  assert.match(server, /\/timezone set/);
  const typed = parseWhen('7pm cst', 'UTC', { now: FRI_3PM_CHICAGO });
  assert.match(ts.buildTimestampEmbed(typed, { zone: 'UTC', source: 'default', roleId: null, roleCount: 0 }).data.description,
    /the timezone in your text/);
});

// ── Payloads ──────────────────────────────────────────────────────────────────

test('payloads: core member commands, and /settings has the timezone-role group', () => {
  for (const cmd of [ts, tzCmd]) {
    assert.equal(cmd.module, null);
    assert.equal(cmd.permissionLevel, 0);
    assert.ok(JSON.stringify(buildCommandPayload(cmd)).length < 4000);
  }
  const when = ts.data.toJSON().options.find((o) => o.name === 'when');
  assert.deepEqual([when.required, when.autocomplete, when.max_length], [true, true, 100]);
  assert.deepEqual(ts.data.toJSON().options.find((o) => o.name === 'style').choices.map((c) => c.value), ts.STYLES.map((s) => s.value));
  assert.deepEqual(tzCmd.data.toJSON().options.map((o) => o.name), ['set', 'show', 'clear']);
  const group = settingsCmd.data.toJSON().options.find((o) => o.name === 'timezone-role');
  assert.deepEqual(group.options.map((o) => o.name), ['add', 'remove', 'list']);
  assert.ok(JSON.stringify(buildCommandPayload(settingsCmd)).length < 4000);
});
