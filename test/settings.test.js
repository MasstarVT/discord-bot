// pkg-settings tests (pkg-settings.md §8 plus the architect amendments): the payload, the timezone, colour and
// prefix helpers, the overview embed, the autocomplete wiring, the pre-defer validation replies and a static scan.
// Offline by default. With TEST_DATABASE_URL (a migrated database) the end-to-end cases also run:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/settings.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails fast, and Postgres is only used with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';
const skipWhenDb = DB ? 'a database is configured; these cases need it to be unreachable' : false;

const { PermissionFlagsBits } = await import('discord.js');
const s = await import('../src/commands/admin/settings.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { PERMISSION_LEVELS, COLORS } = await import('../src/config/constants.js');
const { MODULE_NAMES, MODULES } = await import('../src/config/modules.js');
const { formatColor, parseColor } = await import('../src/utils/customEmbed.js');
const { isValidTimeZone } = await import('../src/utils/time.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { updateSettings, invalidateSettings } = await import('../src/services/settingsService.js');

// Silence Redis reconnect noise and the logger's own output while the tests run.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug };
console.error = console.warn = console.log = console.debug = () => {};
after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) await prisma.$disconnect().catch(() => {});
});

// Same measure as test/smoke.test.js.
function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

const SUMMER = new Date('2026-07-15T12:00:00Z');
const WINTER = new Date('2026-01-15T12:00:00Z');

// ── 1. Payload ──────────────────────────────────────────────────────────────────

test('payload: subcommands, limits, autocomplete wiring and exports', () => {
  const json = s.data.toJSON();
  assert.equal(json.name, 'settings');
  assert.deepEqual(json.options.map((o) => o.name), ['show', 'timezone', 'color', 'prefix', 'urban', 'timezone-role']);
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.equal(json.default_member_permissions, '32');
  assert.ok(charCount(json) <= 1500, `payload is ${charCount(json)} characters (budget 1500)`);
  for (const sub of json.options) assert.ok(sub.description.length <= 100, `${sub.name} description`);

  const opt = (sub, name) => json.options.find((o) => o.name === sub).options.find((o) => o.name === name);
  assert.equal(opt('timezone', 'zone').autocomplete, true);
  assert.equal(opt('timezone', 'zone').max_length, 64);
  assert.equal(opt('timezone', 'zone').required, true);
  assert.equal(opt('color', 'value').autocomplete, true);
  assert.equal(opt('color', 'value').max_length, 16);
  assert.equal(opt('prefix', 'prefix').min_length, 1);
  assert.equal(opt('prefix', 'prefix').max_length, 5);
  assert.ok(!opt('prefix', 'prefix').autocomplete);
  assert.equal(opt('urban', 'nsfw-only').type, 5); // Boolean
  assert.equal(opt('urban', 'nsfw-only').required, true);
  const group = json.options.find((o) => o.name === 'timezone-role');
  assert.deepEqual(group.options.map((o) => o.name), ['add', 'remove', 'list']);
  const add = group.options.find((o) => o.name === 'add');
  assert.equal(add.options.find((o) => o.name === 'zone').autocomplete, true);
  assert.ok(!add.options.find((o) => o.name === 'zone').required);

  assert.equal(s.module, null);
  assert.equal(s.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(s.gated, undefined);
  assert.equal(s.subcommandPermissions, undefined);
  assert.equal(s.allowInDMs, undefined);
  assert.equal(typeof s.execute, 'function');
  assert.equal(typeof s.autocomplete, 'function');
  assert.deepEqual(buildCommandPayload(s).contexts, [0]);
  assert.deepEqual(buildCommandPayload(s).integration_types, [0]);
});

test('MODULES.search is configured through /settings', () => {
  assert.equal(MODULES.search.configCommand, '/settings');
});

// ── 2. listTimeZones ─────────────────────────────────────────────────────────────

test('listTimeZones: UTC first, unique, valid, modern names, memoised', () => {
  const list = s.listTimeZones();
  assert.equal(list[0], 'UTC');
  assert.equal(new Set(list).size, list.length);
  assert.ok(list.length > 300, `only ${list.length} zones`);
  for (const z of list) {
    assert.ok(isValidTimeZone(z), z);
    assert.ok(z.length <= 100, z);
  }
  assert.ok(list.includes('Asia/Kolkata'));
  assert.ok(list.includes('Europe/Kyiv'));
  assert.ok(!list.includes('Asia/Calcutta'));
  assert.ok(!list.includes('Europe/Kiev'));
  assert.equal(s.listTimeZones(), list, 'built once');
  assert.ok(Object.isFrozen(list));
});

// ── 3. normalizeTimeZone ─────────────────────────────────────────────────────────

test('normalizeTimeZone: canonical names, aliases and refusals', () => {
  const zone = (input) => s.normalizeTimeZone(input);
  const ok = (input, expected) => assert.deepEqual(zone(input), { ok: true, zone: expected }, JSON.stringify(input));
  const bad = (input, code) => assert.deepEqual(zone(input), { ok: false, code }, JSON.stringify(input));

  ok('europe/london', 'Europe/London');
  ok('  Europe/Paris  ', 'Europe/Paris');
  ok('Asia/Kolkata', 'Asia/Kolkata');
  ok('asia/kolkata', 'Asia/Kolkata');
  ok('Asia/Calcutta', 'Asia/Kolkata');
  ok('Europe/Kiev', 'Europe/Kyiv');
  ok('US/Eastern', 'America/New_York');
  for (const u of ['utc', 'GMT', 'Etc/UTC', 'GMT+0']) ok(u, 'UTC');
  ok('Etc/GMT+5', 'Etc/GMT+5');

  for (const o of ['+05:30', 'UTC+2', 'utc +2', '−05:00']) bad(o, 'OFFSET');
  for (const a of ['EST', 'Japan']) bad(a, 'ABBREVIATION');
  for (const u of ['Mars/Base', 'x'.repeat(65)]) bad(u, 'UNKNOWN');
  for (const e of ['', '   ', null, undefined]) bad(e, 'EMPTY');

  // Whatever is accepted is storable: valid in Intl and at most 64 characters.
  for (const z of s.listTimeZones()) {
    const r = zone(z);
    assert.ok(r.ok && r.zone === z, z);
    assert.ok(isValidTimeZone(r.zone) && r.zone.length <= 64);
  }
});

// ── 4. utcOffsetLabel ────────────────────────────────────────────────────────────

test('utcOffsetLabel: fixed and DST-aware offsets', () => {
  assert.equal(s.utcOffsetLabel('UTC'), 'UTC+00:00');
  assert.equal(s.utcOffsetLabel('Asia/Kolkata'), 'UTC+05:30');
  assert.equal(s.utcOffsetLabel('Asia/Kathmandu'), 'UTC+05:45');
  assert.equal(s.utcOffsetLabel('America/New_York', WINTER), 'UTC-05:00');
  assert.equal(s.utcOffsetLabel('America/New_York', SUMMER), 'UTC-04:00');
  assert.equal(s.utcOffsetLabel('Etc/GMT+5'), 'UTC-05:00');
  assert.equal(s.utcOffsetLabel('Not/AZone'), 'UTC', 'never throws');
});

// ── 5. searchTimeZones ───────────────────────────────────────────────────────────

test('searchTimeZones: names, cities, offsets and the empty query', () => {
  const search = (q, o = {}) => s.searchTimeZones(q, { now: SUMMER, ...o });
  const values = (r) => r.map((c) => c.value);

  assert.equal(search('new york')[0].value, 'America/New_York');
  const london = search('london');
  assert.equal(london[0].value, 'Europe/London');
  assert.equal(london[0].name, 'Europe/London (UTC+01:00)');
  assert.ok(values(search('kolk')).includes('Asia/Kolkata'));

  const empty = search('');
  assert.equal(empty.length, 25);
  assert.equal(empty[0].value, 'UTC');

  const cur = search('', { current: 'Asia/Tokyo' });
  assert.equal(cur[0].value, 'Asia/Tokyo');
  assert.ok(cur[0].name.endsWith('· current'));
  assert.ok(cur.length <= 25);
  assert.equal(new Set(values(cur)).size, cur.length, 'no duplicate values');

  const half = values(search('+5:30'));
  assert.ok(half.includes('Asia/Kolkata') && half.includes('Asia/Colombo'));
  const minus3 = search('utc-3');
  assert.ok(minus3.length > 0);
  for (const c of minus3) assert.ok(c.name.includes('UTC-03'), c.name);
  assert.ok(values(search('-0')).includes('UTC'), '-0 also finds the UTC+00:00 zones');
  assert.deepEqual(search('+15'), []);
  assert.deepEqual(search('zzzz'), []);

  for (const q of ['', 'a', 'new york', '+5:30', 'utc-3', 'sao']) {
    const r = search(q);
    assert.ok(r.length <= 25, q);
    for (const c of r) assert.ok(c.name.length <= 100 && c.value.length <= 100, `${q}: ${c.name}`);
  }
});

// ── 6. describeLocalTime ─────────────────────────────────────────────────────────

test('describeLocalTime: local time of a zone, UTC for an invalid one', () => {
  const lt = s.describeLocalTime('Europe/London', SUMMER);
  assert.deepEqual(lt, { valid: true, zone: 'Europe/London', time: '13:00', date: '2026-07-15', weekday: 'Wednesday', offset: 'UTC+01:00' });
  const bad = s.describeLocalTime('Mars/Base', SUMMER);
  assert.equal(bad.valid, false);
  assert.equal(bad.zone, 'UTC');
  assert.equal(bad.time, '12:00');
});

// ── 7. parseColorInput / suggestColors ───────────────────────────────────────────

test('parseColorInput: hex, names, reset words, black and refusals', () => {
  const p = (x) => s.parseColorInput(x);
  assert.deepEqual(p('#5865F2'), { ok: true, color: 0x5865f2, adjusted: false });
  assert.equal(p('#fff').color, 0xffffff);
  assert.equal(p('Blurple').color, 0x5865f2);
  for (const r of ['reset', 'Default', ' none ', 'clear']) {
    assert.deepEqual(p(r), { ok: true, color: null, adjusted: false }, r);
  }
  assert.deepEqual(p('#000000'), { ok: true, color: 0x010101, adjusted: true });
  const black = parseColor('black');
  assert.deepEqual(p('black'), black === 0 ? { ok: true, color: 0x010101, adjusted: true } : { ok: true, color: black, adjusted: false });
  for (const x of ['nope', '', '#12345', null, undefined]) assert.deepEqual(p(x), { ok: false }, String(x));
  for (const x of ['#ffffff', '#010101', '0xABCDEF']) {
    const r = p(x);
    assert.ok(r.ok && r.color >= 1 && r.color <= 0xffffff, x);
  }
});

test('suggestColors: current, custom, reset and names', () => {
  const sc = s.suggestColors;
  const all = sc('');
  assert.ok(all.length <= 25);
  assert.ok(all.some((c) => c.value === 'reset'));
  for (const n of s.COLOR_NAMES) assert.ok(all.some((c) => c.value === n), n);

  assert.equal(sc('', { current: 0xff0000 })[0].name, `Current: ${formatColor(0xff0000)}`);
  const custom = sc('#ff0000');
  assert.equal(custom[0].name, `${formatColor(0xff0000)} (custom)`);
  assert.equal(custom[0].value, '#ff0000');
  assert.ok(!sc('nope').some((c) => c.name.includes('(custom)')));
  const re = sc('re').map((c) => c.value);
  assert.ok(re.includes('red') && re.includes('reset'));
  assert.ok(!sc('red').some((c) => c.name.includes('(custom)')), 'a colour name is not "custom"');
  for (const q of ['', 're', '#ff0000', 'blu', '#FFFFFFFFFFFFFFFFFFFF']) {
    for (const c of sc(q, { current: 0x123456 })) assert.ok(c.value.length <= 16, `${q}: ${c.value}`);
  }
});

// ── 8. validatePrefix ────────────────────────────────────────────────────────────

test('validatePrefix: accepts 1-5 plain code points, refuses the rest, and accepts off', () => {
  const v = s.validatePrefix;
  assert.deepEqual(v('!'), { ok: true, prefix: '!' });
  assert.deepEqual(v(' ?? '), { ok: true, prefix: '??' });
  assert.equal(v('⭐').ok, true);
  assert.equal(v('$$$$$').ok, true);
  assert.equal(v('⭐⭐⭐⭐⭐').ok, true, 'five code points');
  assert.deepEqual(v(''), { ok: false, code: 'EMPTY' });
  assert.deepEqual(v('   '), { ok: false, code: 'EMPTY' });
  assert.deepEqual(v('a b'), { ok: false, code: 'SPACES' });
  assert.deepEqual(v('abcdef'), { ok: false, code: 'TOO_LONG' });
  for (const bad of ['<', '/', '@', '#', ':', '\\', '`', '​!', '\u0007']) {
    assert.deepEqual(v(bad), { ok: false, code: 'INVALID_CHARS' }, JSON.stringify(bad));
  }
  // Amendment 2: `off` (any case) is stored as '' and disables prefix invocation.
  for (const off of ['off', 'OFF', ' Off ']) assert.deepEqual(v(off), { ok: true, prefix: '', off: true }, off);
  assert.deepEqual(v('offs'), { ok: true, prefix: 'offs' }, 'only the exact word switches it off');
});

// ── 9. buildOverviewEmbed ────────────────────────────────────────────────────────

const FLAGS = Object.fromEntries(MODULE_NAMES.map((k) => [MODULES[k].flag, false]));
const baseSettings = (extra = {}) => ({
  ...FLAGS, timezone: 'Europe/London', embedColor: null, customPrefix: '!', urbanNsfwOnly: true,
  modRoleIds: ['1', '2', '2'], customCommandsEnabled: false, modEnabled: true, xpEnabled: true, ...extra,
});
const fakeGuild = (name = 'Test **Guild**') => ({ name, roles: { cache: new Map([['1', {}]]) } });
const field = (e, name) => e.fields.find((f) => f.name.includes(name));

test('buildOverviewEmbed: timezone, colour, prefix, modules, urban and moderator roles', () => {
  const e = s.buildOverviewEmbed(baseSettings(), fakeGuild(), { now: SUMMER }).toJSON();
  assert.equal(e.color, COLORS.INFO);
  assert.equal(e.fields.length, 6);
  assert.ok(e.fields.length <= 25);
  const tz = field(e, 'Timezone').value;
  assert.ok(tz.includes('UTC+01:00') && tz.includes('13:00'), tz);
  assert.ok(field(e, 'Custom-command prefix').value.includes('/module enable customcommands'));
  assert.ok(field(e, 'Custom-command prefix').value.startsWith('`!`'));
  assert.ok(field(e, 'Modules').value.startsWith(`**2** of **${MODULE_NAMES.length}** enabled`), field(e, 'Modules').value);
  const roles = field(e, 'Moderator roles').value;
  assert.ok(roles.includes('<@&1>') && !roles.includes('<@&2>') && roles.includes('(+1 deleted role)'), roles);
  assert.ok(roles.includes('/modconfig'));
  assert.ok(e.description.includes('\\*\\*Guild\\*\\*'), e.description);
  assert.ok(JSON.stringify(e).length < 6000);
  for (const f of e.fields) assert.ok(f.value.length <= 1024 && f.name.length <= 256, f.name);

  // Urban Dictionary is read fail-closed: only an explicit false opens it.
  assert.ok(field(e, 'Search').value.includes('age-restricted channels only'));
  const noField = s.buildOverviewEmbed(baseSettings({ urbanNsfwOnly: undefined }), fakeGuild(), { now: SUMMER }).toJSON();
  assert.ok(field(noField, 'Search').value.includes('age-restricted channels only'), 'a missing value counts as true');
  const open = s.buildOverviewEmbed(baseSettings({ urbanNsfwOnly: false }), fakeGuild(), { now: SUMMER }).toJSON();
  assert.ok(field(open, 'Search').value.includes('everywhere'));
  assert.ok(!field(open, 'Search').value.includes('age-restricted'));

  const orange = s.buildOverviewEmbed(baseSettings({ embedColor: 0xff7a00 }), fakeGuild(), { now: SUMMER }).toJSON();
  assert.equal(orange.color, 0xff7a00);
  assert.ok(field(orange, 'Embed colour').value.includes(formatColor(0xff7a00)));
  assert.ok(field(e, 'Embed colour').value.includes(formatColor(COLORS.INFO)), 'default colour is named');

  const mars = s.buildOverviewEmbed(baseSettings({ timezone: 'Mars/Base' }), fakeGuild(), { now: SUMMER }).toJSON();
  assert.ok(field(mars, 'Timezone').value.includes('not a valid timezone'));
});

test('buildOverviewEmbed: prefix off, enabled custom commands, no mod roles, every module on', () => {
  const guild = fakeGuild();
  let e = s.buildOverviewEmbed(baseSettings({ customPrefix: '' }), guild, { now: SUMMER }).toJSON();
  assert.ok(field(e, 'Custom-command prefix').value.includes('Prefix: off'), field(e, 'Custom-command prefix').value);

  e = s.buildOverviewEmbed(baseSettings({ customPrefix: '?', customCommandsEnabled: true }), guild, { now: SUMMER }).toJSON();
  const prefix = field(e, 'Custom-command prefix').value;
  assert.ok(prefix.startsWith('`?`') && prefix.includes('Custom commands: on'), prefix);

  // A legacy row without the column still shows the default prefix.
  e = s.buildOverviewEmbed(baseSettings({ customPrefix: undefined }), guild, { now: SUMMER }).toJSON();
  assert.ok(field(e, 'Custom-command prefix').value.startsWith('`!`'));

  e = s.buildOverviewEmbed(baseSettings({ modRoleIds: [] }), guild, { now: SUMMER }).toJSON();
  assert.ok(field(e, 'Moderator roles').value.startsWith('None set'));
  e = s.buildOverviewEmbed(baseSettings({ modRoleIds: undefined }), guild, { now: SUMMER }).toJSON();
  assert.ok(field(e, 'Moderator roles').value.startsWith('None set'));

  // Every module on and 80 deleted roles: still within Discord's limits.
  const everything = Object.fromEntries(MODULE_NAMES.map((k) => [MODULES[k].flag, true]));
  const manyIds = Array.from({ length: 80 }, (_, i) => String(100000 + i));
  const wide = new Map(manyIds.map((id) => [id, {}]));
  e = s.buildOverviewEmbed(baseSettings({ ...everything, modRoleIds: manyIds }), { name: 'x'.repeat(200), roles: { cache: wide } }, { now: SUMMER }).toJSON();
  assert.ok(field(e, 'Modules').value.startsWith(`**${MODULE_NAMES.length}** of **${MODULE_NAMES.length}** enabled`));
  for (const f of e.fields) assert.ok(f.value.length <= 1024, f.name);
  assert.ok(JSON.stringify(e).length < 6000);
  assert.ok(!/<@&\d*$/.test(field(e, 'Moderator roles').value.split('\n')[0]), 'no mention is cut in half');
});

// ── 10. Autocomplete wiring ──────────────────────────────────────────────────────

function fakeInteraction({ sub = 'show', group = null, strings = {}, booleans = {}, roles = {}, focused = null, guildId = '1', guild = fakeGuild() } = {}) {
  const calls = { reply: [], deferReply: [], editReply: [], followUp: [], respond: [] };
  const it = {
    guildId, guild, user: { id: '42' },
    deferred: false, replied: false, ephemeral: null, responded: false,
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => group,
      getString: (name) => strings[name] ?? null,
      getRole: (name) => roles[name] ?? null,
      get: (name) => (roles[name] ? { value: roles[name].id } : null),
      getBoolean: (name) => booleans[name] ?? null,
      getFocused: () => { if (focused instanceof Error) throw focused; return focused; },
    },
    async reply(p) { it.replied = true; calls.reply.push(p); return { resource: { message: {} } }; },
    async deferReply(o) { it.deferred = true; it.ephemeral = Boolean(o?.ephemeral); calls.deferReply.push(o); },
    async editReply(p) { calls.editReply.push(p); return {}; },
    async followUp(p) { calls.followUp.push(p); return {}; },
    async respond(c) { it.responded = true; calls.respond.push(c); },
  };
  return { it, calls };
}

test('autocomplete: zone and colour suggestions, nothing for other options, always responds', async () => {
  let { it, calls } = fakeInteraction({ sub: 'timezone', focused: { name: 'zone', value: 'lond' } });
  await s.autocomplete(it);
  assert.equal(calls.respond.length, 1);
  assert.ok(calls.respond[0].length <= 25);
  assert.ok(calls.respond[0].some((c) => c.value === 'Europe/London'));

  ({ it, calls } = fakeInteraction({ sub: 'color', focused: { name: 'value', value: 're' } }));
  await s.autocomplete(it);
  assert.ok(calls.respond[0].some((c) => c.value === 'red') && calls.respond[0].some((c) => c.value === 'reset'));

  ({ it, calls } = fakeInteraction({ sub: 'prefix', focused: { name: 'prefix', value: '!' } }));
  await s.autocomplete(it);
  assert.deepEqual(calls.respond, [[]]);

  ({ it, calls } = fakeInteraction({ sub: 'timezone', focused: new Error('boom') }));
  await s.autocomplete(it);
  assert.deepEqual(calls.respond, [[]], 'a throwing option reader still answers');
});

test('timezone-role add: refusals before any defer, and the zone box suggests the role name first', async () => {
  const cst = { id: '700000000000000001', name: 'CST' };
  const guild = { ...fakeGuild(), roles: { cache: new Map([[cst.id, cst]]) } };
  const cases = [
    [{ id: '1', name: '@everyone' }, null, /Pick a Role/],
    [{ id: '700000000000000002', name: 'Gamers' }, null, /Which Timezone/],
    [cst, 'IST', /Ambiguous Timezone/],
    [cst, '+05:30', /Pick a City/],
  ];
  for (const [role, zone, title] of cases) {
    const { it, calls } = fakeInteraction({ group: 'timezone-role', sub: 'add', roles: { role }, strings: { zone }, guild });
    await s.execute(it);
    assert.equal(calls.deferReply.length, 0, role.name);
    assert.equal(calls.reply.length, 1, role.name);
    assert.equal(calls.reply[0].ephemeral, true);
    assert.match(embedOf(calls.reply[0]).title, title);
  }
  const { it, calls } = fakeInteraction({ group: 'timezone-role', sub: 'add', roles: { role: cst }, guild, focused: { name: 'zone', value: '' } });
  await s.autocomplete(it);
  assert.equal(calls.respond[0][0].value, 'America/Chicago');
  assert.match(calls.respond[0][0].name, /from the role name/);
});

// ── 11. Pre-defer validation replies ─────────────────────────────────────────────

const embedOf = (payload, i = 0) => payload.embeds[i].toJSON();

test('validation errors are ephemeral, sent before any defer and never touch settings', async () => {
  const cases = [
    ['timezone', { zone: '+05:30' }, 'Pick a City'],
    ['timezone', { zone: 'EST' }, 'Ambiguous Timezone'],
    ['timezone', { zone: 'Mars/Base' }, 'Unknown Timezone'],
    ['timezone', { zone: '   ' }, 'Missing Timezone'],
    ['color', { value: 'nope' }, 'Invalid Colour'],
    ['prefix', { prefix: 'a b' }, 'Invalid Prefix'],
    ['prefix', { prefix: 'abcdef' }, 'Invalid Prefix'],
    ['prefix', { prefix: '/' }, 'Invalid Prefix'],
  ];
  for (const [sub, strings, title] of cases) {
    const { it, calls } = fakeInteraction({ sub, strings });
    await s.execute(it);
    assert.equal(calls.deferReply.length, 0, `${sub} ${JSON.stringify(strings)} deferred`);
    assert.equal(calls.reply.length, 1, `${sub} ${JSON.stringify(strings)}`);
    assert.equal(calls.reply[0].ephemeral, true);
    assert.ok(embedOf(calls.reply[0]).title.includes(title), embedOf(calls.reply[0]).title);
  }
});

test('validation errors echo the input safely', async () => {
  const { it, calls } = fakeInteraction({ sub: 'timezone', strings: { zone: 'Evil`\\Zone/'.padEnd(64, 'x') } });
  await s.execute(it);
  const text = embedOf(calls.reply[0]).description;
  assert.ok(text.includes('is not a timezone I know'));
  assert.ok(!/``/.test(text) && !text.includes('\\'), text);

  const color = fakeInteraction({ sub: 'color', strings: { value: 'nope' } });
  await s.execute(color.it);
  assert.equal(
    embedOf(color.calls.reply[0]).description,
    '`nope` is not a colour. Use a hex code (`#5865F2` or `#fff`), a name (red, green, blue, blurple, yellow, orange, purple, pink, white, black, grey) or `reset`.',
  );
  const prefix = fakeInteraction({ sub: 'prefix', strings: { prefix: 'a b' } });
  await s.execute(prefix.it);
  assert.equal(embedOf(prefix.calls.reply[0]).description, "The prefix can't contain spaces.");
});

// ── 12. Database down (offline only) ─────────────────────────────────────────────

test('database down: show says Settings Unavailable, a write says Could Not Save', { skip: skipWhenDb }, async () => {
  const GID = 'settings-offline';
  const show = fakeInteraction({ sub: 'show', guildId: GID });
  await s.execute(show.it);
  assert.equal(show.calls.deferReply.length, 1);
  assert.ok(embedOf(show.calls.editReply[0]).title.includes('Settings Unavailable'));
  assert.deepEqual(show.calls.editReply[0].allowedMentions, { parse: [] });

  // One write is enough: every write shares saveSetting, and each attempt waits for Prisma's connection error.
  const { it, calls } = fakeInteraction({ sub: 'timezone', strings: { zone: 'Europe/London' }, guildId: GID });
  await s.execute(it);
  assert.equal(calls.editReply.length, 1);
  assert.ok(embedOf(calls.editReply[0]).title.includes('Could Not Save'), embedOf(calls.editReply[0]).title);
  assert.deepEqual(calls.editReply[0].allowedMentions, { parse: [] });
});

// ── 13. Static scan ──────────────────────────────────────────────────────────────

test('static scan: settingsService only, no prisma, no cache, no console, no side effects at import', () => {
  const text = readFileSync(resolve(ROOT, 'src/commands/admin/settings.js'), 'utf8');
  assert.ok(!/database\/client\.js/.test(text), 'no database client import');
  assert.ok(!/services\/redis\.js/.test(text), 'no redis import');
  assert.ok(!/\bprisma\b/i.test(text.replace(/\/\/.*$/gm, '')), 'no prisma usage');
  assert.ok(!/cache\.del|cache\.set|:settings`/.test(text), 'no direct settings cache writes');
  assert.ok(!/\bconsole\./.test(text), 'logger only');
  assert.ok(!/LIMITS\.[A-Z_]+\s*\?\?/.test(text));
  assert.ok(/updateSettings\(/.test(text) && /getSettingsSafe\(/.test(text) && /peekSettings\(/.test(text));
  // Module-level state is lazy: the zone list starts empty and the formatter cache is a plain Map
  // (both live in utils/timezones.js, which settings.js re-exports).
  const zones = readFileSync(resolve(ROOT, 'src/utils/timezones.js'), 'utf8');
  assert.match(zones, /let zoneList = null;/);
  assert.match(zones, /const offsetFormatters = new Map\(\);/);
});

// ── 14. End to end against a database (TEST_DATABASE_URL) ────────────────────────

const GID = '9550110000000001';
const titleOf = (payload, i = 0) => embedOf(payload, i).title;

async function cleanup() {
  await invalidateSettings(GID);
  await prisma.guildSettings.deleteMany({ where: { guildId: GID } });
}

test('DB: /settings writes through settingsService, answers No Change without writing, and shows the result', { skip: skipDb }, async () => {
  await cleanup();
  try {
    const guild = fakeGuild('End To End');
    const run = async (opts) => {
      const { it, calls } = fakeInteraction({ guildId: GID, guild, ...opts });
      await s.execute(it);
      assert.equal(calls.deferReply.length, 1, 'deferred publicly once');
      assert.ok(!calls.deferReply[0]?.ephemeral);
      assert.equal(calls.editReply.length, 1);
      assert.deepEqual(calls.editReply[0].allowedMentions, { parse: [] });
      return calls.editReply[0];
    };
    const stamp = async () => (await prisma.guildSettings.findUnique({ where: { guildId: GID } }));

    // Defaults on a fresh guild.
    let out = await run({ sub: 'show' });
    let e = embedOf(out);
    assert.equal(e.color, COLORS.INFO);
    assert.ok(field(e, 'Timezone').value.includes('`UTC`'));
    assert.ok(field(e, 'Custom-command prefix').value.startsWith('`!`'));
    assert.ok(field(e, 'Search').value.includes('age-restricted channels only'));
    const created = await stamp();
    assert.ok(created, 'the settings row exists');

    // Timezone: write, then No Change.
    out = await run({ sub: 'timezone', strings: { zone: 'europe/london' } });
    assert.ok(titleOf(out).includes('Timezone Updated'));
    assert.ok(embedOf(out).description.includes('**Europe/London**'));
    assert.ok(embedOf(out).description.includes('Previously: `UTC`.'));
    let row = await stamp();
    assert.equal(row.timezone, 'Europe/London');
    out = await run({ sub: 'timezone', strings: { zone: 'Europe/London' } });
    assert.ok(titleOf(out).includes('No Change'));
    assert.equal((await stamp()).updatedAt.getTime(), row.updatedAt.getTime(), 'no write on No Change');
    out = await run({ sub: 'timezone', strings: { zone: 'Asia/Calcutta' } });
    assert.equal((await stamp()).timezone, 'Asia/Kolkata', 'legacy names are stored under the modern name');

    // Colour: set (two embeds, preview in the new colour), black adjusts, reset, No Change.
    out = await run({ sub: 'color', strings: { value: '#ff7a00' } });
    assert.equal(out.embeds.length, 2);
    assert.ok(titleOf(out).includes('Embed Colour Updated'));
    assert.equal(embedOf(out, 1).color, 0xff7a00);
    assert.equal((await stamp()).embedColor, 0xff7a00);
    out = await run({ sub: 'color', strings: { value: '#FF7A00' } });
    assert.ok(titleOf(out).includes('No Change'));
    assert.equal(embedOf(out).color, 0xff7a00, 'No Change keeps the guild colour');
    out = await run({ sub: 'show' });
    assert.equal(embedOf(out).color, 0xff7a00, 'show is coloured with the guild colour');
    out = await run({ sub: 'color', strings: { value: '#000000' } });
    assert.ok(embedOf(out).description.includes('#010101'));
    assert.equal((await stamp()).embedColor, 0x010101);
    out = await run({ sub: 'color', strings: { value: 'reset' } });
    assert.ok(titleOf(out).includes('Embed Colour Reset'));
    assert.equal((await stamp()).embedColor, null);
    assert.equal(embedOf(out, 1).color, COLORS.INFO);
    out = await run({ sub: 'color', strings: { value: 'default' } });
    assert.ok(titleOf(out).includes('No Change'));

    // Prefix: set with the module warning, off, No Change, back on.
    await updateSettings(GID, { customCommandsEnabled: false });
    out = await run({ sub: 'prefix', strings: { prefix: '?' } });
    assert.ok(titleOf(out).includes('Prefix Updated'));
    assert.ok(embedOf(out).description.includes('`?rules`'));
    assert.ok(embedOf(out).description.includes('/module enable customcommands'));
    assert.equal((await stamp()).customPrefix, '?');
    out = await run({ sub: 'prefix', strings: { prefix: '?' } });
    assert.ok(titleOf(out).includes('No Change'));
    await updateSettings(GID, { customCommandsEnabled: true });
    out = await run({ sub: 'prefix', strings: { prefix: '.' } });
    assert.ok(!embedOf(out).description.includes('/module enable'), 'no warning when the module is on');
    out = await run({ sub: 'prefix', strings: { prefix: 'OFF' } });
    assert.equal(embedOf(out).description, 'Prefix commands are off. /tag use still works.');
    assert.equal((await stamp()).customPrefix, '', 'off stores the empty string');
    out = await run({ sub: 'prefix', strings: { prefix: 'off' } });
    assert.ok(titleOf(out).includes('No Change'));
    out = await run({ sub: 'show' });
    assert.ok(field(embedOf(out), 'Custom-command prefix').value.includes('Prefix: off'));
    out = await run({ sub: 'prefix', strings: { prefix: '!' } });
    assert.equal((await stamp()).customPrefix, '!');

    // Urban: relax, No Change, restrict again.
    out = await run({ sub: 'urban', booleans: { 'nsfw-only': false } });
    assert.ok(titleOf(out).includes('Urban Dictionary'));
    assert.equal(embedOf(out).description, '/search urban now works in every channel.');
    assert.equal((await stamp()).urbanNsfwOnly, false);
    out = await run({ sub: 'urban', booleans: { 'nsfw-only': false } });
    assert.ok(titleOf(out).includes('No Change'));
    out = await run({ sub: 'show' });
    assert.ok(field(embedOf(out), 'Search').value.includes('everywhere'));
    out = await run({ sub: 'urban', booleans: { 'nsfw-only': true } });
    assert.equal(embedOf(out).description, '/search urban now only works in age-restricted channels.');
    assert.equal((await stamp()).urbanNsfwOnly, true);
    await updateSettings(GID, { searchEnabled: false });
    out = await run({ sub: 'urban', booleans: { 'nsfw-only': false } });
    assert.ok(embedOf(out).description.includes('/module enable search'));
  } finally {
    await cleanup();
  }
});
