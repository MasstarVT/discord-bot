// Auto Timestamps tests. Offline (no Discord, Postgres or Redis): the time scanner (what counts as a time and what never
// does), reading spans in a zone, the author's zone, the message handler against fake channels and an in-memory
// UserTimezone table, the hook filter, the plugin declaration and the /autotime payload.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const tm = await import('../src/utils/timeMentions.js');
const autotime = await import('../src/plugins/autotime.js');
const cmd = await import('../src/commands/admin/autotime.js');
const userTz = await import('../src/services/userTimezoneService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { updateSettings } = await import('../src/services/settingsService.js');

const { extractTimeSpans, readTimeSpans, mayMentionTime } = tm;
const { plugin, AUTOTIME, answerText, zoneHintText, authorZone, timesToAnswer, handleMessage, isIgnoredChannel, mayAnswer } = autotime;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// In-memory UserTimezone table. Users whose id starts with U-fail make the lookup fail.
const zones = new Map();
const realUserTimezone = prisma.userTimezone;
prisma.userTimezone = {
  findUnique: async ({ where }) => {
    if (where.userId.startsWith('U-fail')) throw new Error('database down');
    return zones.has(where.userId) ? { userId: where.userId, zone: zones.get(where.userId) } : null;
  },
};

after(async () => {
  prisma.userTimezone = realUserTimezone;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const CHI = 'America/Chicago';
const NY = 'America/New_York';
const ROLE_CST = '111111111111111111';
// Tuesday, Oct 20 2026, 10:00 in Chicago (CDT, UTC-5).
const NOW = new Date('2026-10-20T15:00:00Z');
const iso = (d) => d.toISOString();

// ── Scanner ───────────────────────────────────────────────────────────────────

const spanOf = (text) => extractTimeSpans(text).map(({ day, time, zone }) => ({ day, time, zone }));

test('extractTimeSpans: times with am/pm, and 24-hour times, noon or midnight next to a day, a zone or "at"', () => {
  const cases = [
    ['friday 5pm', { day: 'friday', time: '5pm', zone: null }],
    ['lets do Fri at 5 PM?', { day: 'fri', time: '5pm', zone: null }],
    ['5pm on friday', { day: 'friday', time: '5pm', zone: null }],
    ['tomorrow 18:30', { day: 'tomorrow', time: '18:30', zone: null }],
    ['meet at 09:15', { day: null, time: '09:15', zone: null }],
    ['7pm cst', { day: null, time: '7pm', zone: CHI }],
    ['7pm EST tomorrow', { day: 'tomorrow', time: '7pm', zone: NY }],
    ['5pm on friday EST', { day: 'friday', time: '5pm', zone: NY }],
    ['next mon 9am', { day: 'next mon', time: '9am', zone: null }],
    ['oct 20 7:30pm', { day: 'oct 20', time: '7:30pm', zone: null }],
    ['October 23rd at 5.30 p.m.', { day: 'october 23rd', time: '5:30pm', zone: null }],
    ['20th of october 9am', { day: '20th october', time: '9am', zone: null }],
    ['2026-10-25 17:00', { day: '2026-10-25', time: '17:00', zone: null }],
    ['tonight 9pm', { day: 'today', time: '9pm', zone: null }],
    ['at noon', { day: null, time: 'noon', zone: null }],
    ['midnight utc', { day: null, time: 'midnight', zone: 'UTC' }],
    ['5pm utc+2', { day: null, time: '5pm', zone: 'Etc/GMT-2' }],
    ['5pm EAT', { day: null, time: '5pm', zone: 'Africa/Nairobi' }],
  ];
  for (const [text, want] of cases) assert.deepEqual(spanOf(text), [want], text);

  assert.deepEqual(spanOf('3pm-5pm').map((s) => s.time), ['3pm', '5pm'], 'a range gives both ends');
  assert.equal(extractTimeSpans('5pm eat dinner')[0].text, '5pm', 'a lower-case word is not a zone, nor part of the time');
  assert.equal(extractTimeSpans('5pm eat dinner')[0].zone, null);
});

test('extractTimeSpans: ordinary chat, ambiguous times and hidden text are never read', () => {
  const never = [
    '5 apples', '2-3', 'in 2 days', 'in 2h', 'tomorrow', 'friday at 5', 'now', '4:30', 'at 9:00', '10:30 tomorrow',
    'level 12:30', '18:30 works', 'noon', 'midnight snack', '13:45:10', 'v1.5pm', 'top 5 amazing', '5pm utc+5:30',
    '```\nfri 5pm\n```', 'say `fri 5pm`', '||fri 5pm||', '> fri 5pm', '>>> quoted\nfri 5pm', 'https://example.com/5pm',
    '<t:1792537200:F> or fri 5pm', '<@123456789012345678> 5pm',
  ];
  for (const text of never) {
    const spans = extractTimeSpans(text);
    if (text === '<@123456789012345678> 5pm') assert.deepEqual(spanOf(text), [{ day: null, time: '5pm', zone: null }], 'only the mention is skipped');
    else assert.deepEqual(spans, [], text);
  }
  assert.equal(mayMentionTime(null), false);
  assert.equal(mayMentionTime('no time here'), false);
  assert.equal(mayMentionTime('around 5 pm'), true);
  assert.equal(extractTimeSpans('1pm 2pm 3pm 4pm', { max: 2 }).length, 2);
});

test('readTimeSpans: reads in the given zone, today within the grace, the next one otherwise', () => {
  const read = (text, zone = CHI, opts = {}) => readTimeSpans(extractTimeSpans(text), zone, { now: NOW, ...opts });
  assert.equal(iso(read('friday 5pm')[0].date), '2026-10-23T22:00:00.000Z');
  assert.equal(iso(read('5pm')[0].date), '2026-10-20T22:00:00.000Z', 'later today');
  assert.equal(iso(read('9:30am')[0].date), '2026-10-20T14:30:00.000Z', 'passed 30 minutes ago: still today');
  assert.equal(iso(read('8am')[0].date), '2026-10-21T13:00:00.000Z', 'passed 2 hours ago: tomorrow');
  assert.deepEqual(read('today 8am'), [], 'a day in the past is dropped');
  assert.deepEqual(read('2028-01-01 5pm'), [], 'over a year ahead is dropped');
  assert.equal(iso(read('tomorrow 18:30')[0].date), '2026-10-21T23:30:00.000Z');
  assert.equal(iso(read('5pm on friday EST')[0].date), '2026-10-23T21:00:00.000Z', 'the zone in the text wins');

  const [named] = read('7pm cst', NY);
  assert.equal(named.zone, CHI);
  assert.equal(named.zoneFromInput, true);
  assert.equal(read('fri 5pm')[0].zoneFromInput, false);

  assert.deepEqual(read('fri 5pm', null), [], 'no zone of the author: nothing to read it in');
  assert.deepEqual(read('fri 5pm or 6pm EST', null).map((t) => t.text), ['6pm EST'], 'a time that names its zone is still read');
  assert.equal(read('fri 5pm, yes fri 5pm').length, 1, 'repeats are dropped');
  assert.equal(read('1pm 2pm 3pm 4pm 5pm').length, 3, 'max 3 by default');
});

// ── The author's zone ─────────────────────────────────────────────────────────

const SETTINGS = Object.freeze({ autoTimeEnabled: true, timezone: 'Europe/Berlin', timezoneRoles: [{ roleId: ROLE_CST, zone: CHI }], autoTimeIgnoredChannelIds: [] });
const fakeGuild = { roles: { cache: new Map([[ROLE_CST, { id: ROLE_CST, position: 3 }]]) }, channels: { cache: new Map() } };

let nextId = 5000;
function fakeChannel(id, { allowed = true, parentId = null, parent = null, thread = false } = {}) {
  return {
    id, guildId: 'G', parentId, parent,
    guild: { members: { me: {} } },
    permissionsFor: () => ({ has: () => allowed }),
    isThread: () => thread,
  };
}
function fakeMessage(channel, userId, content, roles = []) {
  return {
    id: String(nextId++), guildId: 'G', channelId: channel.id, channel, content, guild: fakeGuild,
    author: { id: userId }, member: { roles: { cache: new Map(roles.map((r) => [r, { id: r }])) } }, replies: [],
    async reply(payload) { this.replies.push(payload); return { id: String(nextId++) }; },
  };
}
const ctxFor = (message, settings = SETTINGS) => ({ guildId: 'G', guild: fakeGuild, message, getSettings: async () => settings });

test('authorZone: their own zone, else their timezone role, never the server timezone', async () => {
  userTz.__resetUserTimezoneMemo();
  zones.set('U-own', 'Asia/Tokyo');
  const ch = fakeChannel('C-zone');
  assert.equal(await authorZone(ctxFor(fakeMessage(ch, 'U-own', 'x', [ROLE_CST])), SETTINGS), 'Asia/Tokyo', 'own zone beats the role');
  assert.equal(await authorZone(ctxFor(fakeMessage(ch, 'U-role', 'x', [ROLE_CST])), SETTINGS), CHI);
  assert.equal(await authorZone(ctxFor(fakeMessage(ch, 'U-none', 'x')), SETTINGS), null, 'the server timezone is not used');
  assert.equal(await authorZone(ctxFor(fakeMessage(ch, 'U-fail-zone', 'x', [ROLE_CST])), SETTINGS), CHI, 'a failed lookup: the role still counts');
  assert.equal(await authorZone(ctxFor(fakeMessage(ch, 'U-fail-zone2', 'x')), SETTINGS), undefined, 'a failed lookup and no role: unknown');
});

// ── The message handler ───────────────────────────────────────────────────────

test('a member with a timezone role gets a reply with timestamps and no pings', async () => {
  const ch = fakeChannel('C-reply');
  const m = fakeMessage(ch, 'U-reply', 'raid tomorrow at 8pm?', [ROLE_CST]);
  await handleMessage(ctxFor(m));
  assert.equal(m.replies.length, 1);
  const { content, allowedMentions, failIfNotExists } = m.replies[0];
  assert.deepEqual(allowedMentions, { parse: [], repliedUser: false });
  assert.equal(failIfNotExists, false);
  assert.match(content, /^🕒 `tomorrow at 8pm` → <t:\d+:F> \(<t:\d+:R>\)\n-# Everyone sees this in their own timezone\.$/);
  assert.doesNotMatch(content, /Chicago/, "the author's zone is not shown");
});

test('members without a zone are answered for times that name one, and told how to set one for the rest', async () => {
  const plain = fakeMessage(fakeChannel('C-nozone'), 'U-nozone', 'see you tomorrow 5pm');
  await handleMessage(ctxFor(plain));
  assert.equal(plain.replies.length, 1);
  assert.deepEqual(plain.replies[0].allowedMentions, { parse: [], repliedUser: false });
  assert.equal(plain.replies[0].content, zoneHintText('tomorrow 5pm', true));
  assert.equal(plain.replies[0].content,
    "⚠️ I can't convert `tomorrow 5pm` without your timezone. Pick a timezone role, or save yours with `/timezone set`.");

  const named = fakeMessage(fakeChannel('C-nozone2'), 'U-nozone', 'see you tomorrow 5pm EST');
  await handleMessage(ctxFor(named));
  assert.equal(named.replies.length, 1);
  assert.doesNotMatch(named.replies[0].content, /timezone set/, 'every time named its zone: no note');

  const again = fakeMessage(fakeChannel('C-nozone3'), 'U-nozone', 'or fri 6pm');
  await handleMessage(ctxFor(again));
  assert.deepEqual(again.replies, [], `the note is sent once per member per server every ${AUTOTIME.ZONE_HINT_COOLDOWN_MS} ms`);
  const elsewhere = fakeMessage(fakeChannel('C-nozone4'), 'U-nozone', 'or fri 6pm');
  elsewhere.guildId = 'G2';
  await handleMessage({ ...ctxFor(elsewhere), guildId: 'G2' });
  assert.equal(elsewhere.replies.length, 1, 'another server gets its own note');
});

test('the timezone note: roles offered only when linked, added to a reply with times, never on a failed lookup', async () => {
  const noRoles = fakeMessage(fakeChannel('C-hint1'), 'U-hint1', 'fri 5pm');
  await handleMessage(ctxFor(noRoles, { ...SETTINGS, timezoneRoles: [] }));
  assert.equal(noRoles.replies[0].content, "⚠️ I can't convert `fri 5pm` without your timezone. Save yours with `/timezone set`.");
  const goneRole = fakeMessage(fakeChannel('C-hint2'), 'U-hint2', 'fri 5pm');
  await handleMessage(ctxFor(goneRole, { ...SETTINGS, timezoneRoles: [{ roleId: '222222222222222222', zone: CHI }] }));
  assert.doesNotMatch(goneRole.replies[0].content, /timezone role/, 'a linked role that was deleted does not count');

  const mixed = fakeMessage(fakeChannel('C-hint3'), 'U-hint3', 'fri 5pm or sat 6pm EST');
  await handleMessage(ctxFor(mixed));
  const lines = mixed.replies[0].content.split('\n');
  assert.equal(lines.length, 3);
  assert.match(lines[0], /^🕒 `sat 6pm EST` → <t:\d+:F>/);
  assert.equal(lines[2], zoneHintText('fri 5pm', true));

  const failed = fakeMessage(fakeChannel('C-hint4'), 'U-fail-hint', 'fri 5pm');
  await handleMessage(ctxFor(failed));
  assert.deepEqual(failed.replies, [], 'they may have a zone saved: no note');

  const ignored = fakeMessage(fakeChannel('C-hint-ign'), 'U-hint5', 'fri 5pm');
  await handleMessage(ctxFor(ignored, { ...SETTINGS, autoTimeIgnoredChannelIds: ['C-hint-ign'] }));
  assert.deepEqual(ignored.replies, [], 'an ignored channel');
  const noPerm = fakeMessage(fakeChannel('C-hint-perm', { allowed: false }), 'U-hint6', 'fri 5pm');
  await handleMessage(ctxFor(noPerm));
  assert.deepEqual(noPerm.replies, [], 'no permission');
});

test('ignored channels, categories and thread parents; missing permissions; no settings', async () => {
  const settings = { ...SETTINGS, autoTimeIgnoredChannelIds: ['C-ign', 'CAT-ign'] };
  const say = async (channel) => {
    const m = fakeMessage(channel, `U-${channel.id}`, 'tomorrow 5pm', [ROLE_CST]);
    await handleMessage(ctxFor(m, settings));
    return m.replies.length;
  };
  assert.equal(await say(fakeChannel('C-ign')), 0, 'the channel');
  assert.equal(await say(fakeChannel('C-in-cat', { parentId: 'CAT-ign' })), 0, 'a channel of the category');
  assert.equal(await say(fakeChannel('T-1', { thread: true, parentId: 'C-ign' })), 0, 'a thread of the channel');
  assert.equal(await say(fakeChannel('T-2', { thread: true, parentId: 'C-x', parent: { parentId: 'CAT-ign' } })), 0, 'a thread under the category');
  assert.equal(await say(fakeChannel('C-ok')), 1);
  assert.equal(await say(fakeChannel('C-noperm', { allowed: false })), 0, 'no permission: no reply, no throw');

  const m = fakeMessage(fakeChannel('C-nosettings'), 'U-x', 'tomorrow 5pm', [ROLE_CST]);
  await handleMessage(ctxFor(m, null));
  assert.deepEqual(m.replies, []);
  assert.equal(isIgnoredChannel(null, { id: 'C' }), false);
  assert.equal(isIgnoredChannel(settings, null), false);
});

test('cooldowns: one answer per member per channel at a time, and the same instant once per channel', async () => {
  const ch = fakeChannel('C-cool');
  const first = fakeMessage(ch, 'U-a', 'fri 5pm?', [ROLE_CST]);
  const burst = fakeMessage(ch, 'U-a', 'or sat 6pm?', [ROLE_CST]);
  const echo = fakeMessage(ch, 'U-b', 'yes fri 5pm works', [ROLE_CST]);
  const other = fakeMessage(ch, 'U-c', 'I can do fri 5pm or sun 3pm', [ROLE_CST]);
  for (const m of [first, burst, echo, other]) await handleMessage(ctxFor(m));
  assert.equal(first.replies.length, 1);
  assert.deepEqual(burst.replies, [], `the same member within ${AUTOTIME.MEMBER_COOLDOWN_MS} ms`);
  assert.deepEqual(echo.replies, [], 'the same instant again');
  assert.equal(other.replies.length, 1);
  assert.match(other.replies[0].content, /^🕒 `sun 3pm`/, 'only the new time is answered');
  assert.equal(other.replies[0].content.split('\n').length, 2);
});

test('timesToAnswer caps the answer and skips messages that already hold a timestamp', async () => {
  const ch = fakeChannel('C-cap');
  const many = fakeMessage(ch, 'U-cap', 'tomorrow 1pm 2pm 3pm 4pm 5pm', [ROLE_CST]);
  assert.equal((await timesToAnswer(ctxFor(many), { now: NOW })).length, LIMITS.AUTOTIME_PER_MESSAGE);
  const stamped = fakeMessage(ch, 'U-cap2', 'it is <t:1792537200:F>, so fri 5pm', [ROLE_CST]);
  assert.deepEqual(await timesToAnswer(ctxFor(stamped), { now: NOW }), []);
});

test('answerText echoes the typed text safely', () => {
  const date = new Date('2026-10-23T22:00:00Z');
  const text = answerText([{ text: 'fri `5pm`\\', date }]);
  assert.equal(text, '🕒 `fri 5pm` → <t:1792792800:F> (<t:1792792800:R>)\n-# Everyone sees this in their own timezone.');
});

test('the hook filter skips text with no time, and ignored channels once the settings are memoised', async () => {
  const hook = plugin.hooks.find((h) => h.event === 'messageCreate');
  assert.equal(hook.filter, mayAnswer);
  assert.equal(mayAnswer({ guildId: 'G-memo', message: { content: 'hello', channel: { id: 'C' } } }), false);
  assert.equal(mayAnswer({ guildId: 'G-memo', message: { content: 'fri 5pm', channel: { id: 'C-ign' } } }), true, 'no memo: the handler decides');

  const upsert = prisma.guildSettings.upsert;
  prisma.guildSettings.upsert = async ({ where, update }) => ({ guildId: where.guildId, ...update });
  try {
    await updateSettings('G-memo', { autoTimeEnabled: true, autoTimeIgnoredChannelIds: ['C-ign', 'CAT-ign'] });
  } finally {
    prisma.guildSettings.upsert = upsert;
  }
  const ctx = (channel, content = 'fri 5pm') => ({ guildId: 'G-memo', message: { content, channel } });
  assert.equal(mayAnswer(ctx({ id: 'C-any' })), true);
  assert.equal(mayAnswer(ctx({ id: 'C-ign' })), false, 'an ignored channel never reaches the handler');
  assert.equal(mayAnswer(ctx({ id: 'T', parentId: 'C-x', parent: { parentId: 'CAT-ign' } })), false, 'nor a thread under an ignored category');
  assert.equal(mayAnswer(ctx({ id: 'C-any' }, 'hello')), false);
});

// ── Declarations ──────────────────────────────────────────────────────────────

test('plugin declaration and hook order', async () => {
  assert.equal(plugin.id, 'autotime');
  assert.equal(plugin.module, 'autotime');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id]), [['messageCreate', 'autotime']]);
  assert.ok(HOOK_ORDER.messageCreate.indexOf('autotime') > HOOK_ORDER.messageCreate.indexOf('automod'), 'after automod');
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'autotime'), 'collectPlugins keeps it');
  assert.deepEqual(errors.filter((e) => String(e?.message ?? e).includes('autotime')), []);
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/autotime payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'autotime');
  assert.equal(json.default_member_permissions, '32');
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['enable', 1], ['disable', 1], ['status', 1], ['ignore', 2]]);
  const ignore = json.options.find((o) => o.name === 'ignore');
  assert.deepEqual(ignore.options.map((o) => [o.name, o.options.map((x) => [x.name, x.type, !!x.required])]),
    [['add', [['channel', 7, true]]], ['remove', [['channel', 7, true]]]]);
  assert.ok(charCount(json) <= 900, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'autotime');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
});

test('timezoneRolesLine counts linked roles that still exist, or says how to add them', () => {
  assert.match(cmd.timezoneRolesLine(SETTINGS, fakeGuild), /\*\*1\*\* timezone role is linked/);
  assert.match(cmd.timezoneRolesLine({ timezoneRoles: [] }, fakeGuild), /No timezone roles are linked yet/);
  assert.match(cmd.timezoneRolesLine({ timezoneRoles: [{ roleId: '222222222222222222', zone: CHI }] }, fakeGuild), /No timezone roles/);
});
