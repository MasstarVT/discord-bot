// Events tests. Offline (no Discord, Postgres or Redis): the pure rules (times, embeds, buttons, reminder chunks, field
// parsing, the calendar entry and payload), the service against in-memory GuildEvent / EventRsvp tables and fake guilds (create, Discord event, jobs, RSVP
// capacity under concurrency, roles, end, cancel, edit, reminder, cleanup hooks), the edit form, the plugin declaration and
// the /event payload.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const svc = await import('../src/services/eventService.js');
const { plugin } = await import('../src/plugins/events.js');
const cmd = await import('../src/commands/community/event.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { JOB_TYPES } = await import('../src/config/jobTypes.js');
const { CUSTOM_ID_PREFIXES } = await import('../src/config/customIds.js');
const { PERMISSION_LEVELS, LIMITS } = await import('../src/config/constants.js');
const { updateSettings } = await import('../src/services/settingsService.js');
const { parseWhen } = await import('../src/utils/time.js');
const { GuildScheduledEventStatus, PermissionFlagsBits } = await import('discord.js');

const { EVENT } = svc;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const HOUR = 3_600_000;
const DAY = 24 * HOUR;
const future = (ms) => new Date(Date.now() + ms);
const sleep = (ms) => new Promise((r) => { setTimeout(r, ms); });

// ── In-memory tables ──────────────────────────────────────────────────────────

const real = {
  userTimezone: prisma.userTimezone, guildEvent: prisma.guildEvent, eventRsvp: prisma.eventRsvp, guildSettings: prisma.guildSettings,
  $transaction: prisma.$transaction, $queryRaw: prisma.$queryRaw,
};
const events = new Map();
let rsvps = [];
let seq = 1;
let tick = 1;
let txTail = Promise.resolve();
const clone = (r) => (r ? { ...r } : null);

function matches(row, where = {}) {
  return Object.entries(where).every(([k, v]) => {
    if (v && typeof v === 'object' && !(v instanceof Date)) {
      if ('in' in v) return v.in.includes(row[k]);
      if ('gt' in v) return row[k] > v.gt;
      if ('contains' in v) return String(row[k]).toLowerCase().includes(String(v.contains).toLowerCase());
    }
    return row[k] === v;
  });
}
const sorted = (rows, orderBy) => {
  const [[key, dir] = []] = Object.entries(Array.isArray(orderBy) ? orderBy[0] : (orderBy ?? {}));
  if (!key) return rows;
  return [...rows].sort((a, b) => (a[key] > b[key] ? 1 : a[key] < b[key] ? -1 : 0) * (dir === 'desc' ? -1 : 1));
};

prisma.guildEvent = {
  create: async ({ data }) => {
    const row = {
      id: `E${seq++}`, description: null, location: null, endsAt: null, messageId: null, roleId: null, capacity: null,
      reminderMinutes: 15, reminderSentAt: null, discordEventId: null, status: 'SCHEDULED', createdAt: new Date(), ...data,
    };
    events.set(row.id, row);
    return clone(row);
  },
  count: async ({ where }) => [...events.values()].filter((r) => matches(r, where)).length,
  findFirst: async ({ where }) => clone([...events.values()].find((r) => matches(r, where))),
  findUnique: async ({ where }) => clone(events.get(where.id)),
  findMany: async ({ where, orderBy, take }) => sorted([...events.values()].filter((r) => matches(r, where)), orderBy).slice(0, take ?? 1e9).map(clone),
  update: async ({ where, data }) => Object.assign(events.get(where.id), data) && clone(events.get(where.id)),
  updateMany: async ({ where, data }) => {
    let count = 0;
    for (const r of events.values()) if (matches(r, where)) { Object.assign(r, data); count += 1; }
    return { count };
  },
  delete: async ({ where }) => { events.delete(where.id); return {}; },
};
prisma.eventRsvp = {
  findUnique: async ({ where }) => clone(rsvps.find((r) => r.eventId === where.eventId_userId.eventId && r.userId === where.eventId_userId.userId)),
  count: async ({ where }) => rsvps.filter((r) => matches(r, where)).length,
  upsert: async ({ where, create, update }) => {
    const { eventId, userId } = where.eventId_userId;
    const row = rsvps.find((r) => r.eventId === eventId && r.userId === userId);
    if (row) Object.assign(row, update, { updatedAt: tick++ });
    else rsvps.push({ roleGranted: false, ...create, updatedAt: tick++ });
    return {};
  },
  updateMany: async ({ where, data }) => {
    const hit = rsvps.filter((r) => matches(r, where));
    for (const r of hit) Object.assign(r, data);
    return { count: hit.length };
  },
  groupBy: async ({ by, where }) => {
    const groups = new Map();
    for (const r of rsvps.filter((x) => matches(x, where))) groups.set(r[by[0]], (groups.get(r[by[0]]) ?? 0) + 1);
    return [...groups].map(([k, n]) => ({ [by[0]]: k, _count: { _all: n } }));
  },
  findMany: async ({ where, orderBy, take }) => sorted(rsvps.filter((r) => matches(r, where)), orderBy).slice(0, take ?? 1e9).map(clone),
};
prisma.guildSettings = { upsert: async ({ where, update }) => ({ guildId: where.guildId, ...update }) };
// Personal timezones (/timezone set): no row means none saved.
const tzRows = new Map();
prisma.userTimezone = { findUnique: async ({ where }) => (tzRows.has(where.userId) ? { zone: tzRows.get(where.userId) } : null) };
prisma.$queryRaw = async (sql) => {
  if (String(sql.sql).includes('FROM "GuildEvent"')) {
    const r = events.get(sql.values[0]);
    return r ? [{ id: r.id, guildId: r.guildId, status: r.status, capacity: r.capacity }] : [];
  }
  return [{ ok: 1 }];
};
// A real transaction serialises on the row lock; the stub serialises the whole callback.
prisma.$transaction = (fn) => {
  const run = txTail.then(() => fn(prisma));
  txTail = run.then(() => {}, () => {});
  return run;
};

const jobCalls = { schedule: [], cancel: [] };
svc.__setJobImpl({
  schedule: async (j) => { jobCalls.schedule.push(j); return { id: 'J', runAt: j.runAt }; },
  cancel: async (f) => { jobCalls.cancel.push(f); return 1; },
});
const renders = [];
svc.__setRenderImpl(async (_client, id) => { renders.push(id); return 'ok'; });

after(async () => {
  Object.assign(prisma, real);
  svc.__setJobImpl();
  svc.__setRenderImpl();
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

function reset() {
  events.clear();
  rsvps = [];
  jobCalls.schedule.length = 0;
  jobCalls.cancel.length = 0;
  renders.length = 0;
}

async function configure(guildId, data = {}) {
  await updateSettings(guildId, { eventsEnabled: true, eventChannelId: null, eventCreateDiscordEvent: true, eventReminderMinutes: 15, ...data });
}

// ── Fakes ─────────────────────────────────────────────────────────────────────

let nextId = 1000;
function fakeMember(id, { roleIds = [], mod = false, manageRoles = false } = {}) {
  const cache = new Map(roleIds.map((r) => [r, true]));
  return {
    id, roleLog: [], guild: { ownerId: 'OWNER' },
    permissions: { has: (b) => (mod && b === PermissionFlagsBits.ModerateMembers) || (manageRoles && b === PermissionFlagsBits.ManageRoles) },
    roles: {
      cache,
      highest: { position: 3 },
      async add(roleId, reason) { cache.set(roleId, true); this.log?.push(['add', roleId, reason]); },
      async remove(roleId, reason) { cache.delete(roleId); this.log?.push(['remove', roleId, reason]); },
    },
  };
}
function fakeGuild({ id = 'G', manageEvents = true, manageRoles = true, eventFails = false } = {}) {
  const members = new Map();
  const roles = new Map([['R1', { id: 'R1', position: 2, managed: false, permissions: { has: () => false } }]]);
  const guild = {
    id, available: true, ownerId: 'OWNER', members: { cache: members }, roles: { cache: roles }, channels: { cache: new Map() },
    scheduledEvents: {
      created: [], edited: [], deleted: [],
      async create(o) { if (eventFails) throw Object.assign(new Error('boom'), { code: 50035 }); this.created.push(o); return { id: 'DE1' }; },
      async edit(eventId, o) { this.edited.push([eventId, o]); return {}; },
      async delete(eventId) { this.deleted.push(eventId); },
    },
  };
  guild.members.me = {
    id: 'BOT',
    permissions: { has: (b) => (b === PermissionFlagsBits.ManageEvents ? manageEvents : b === PermissionFlagsBits.ManageRoles ? manageRoles : true) },
    roles: { highest: { position: 10 } },
  };
  guild.members.fetch = async (uid) => {
    if (members.has(uid)) return members.get(uid);
    throw Object.assign(new Error('Unknown Member'), { code: 10007 });
  };
  return guild;
}
function fakeChannel(guild, id = 'C1', { sendFails = false } = {}) {
  const ch = {
    id, guildId: guild.id, guild, sent: [], edits: [],
    permissionsFor: () => ({ has: () => true }),
    isTextBased: () => true,
    isThread: () => false,
    async send(payload) {
      if (sendFails) throw Object.assign(new Error('Missing Access'), { code: 50001 });
      this.sent.push(payload);
      return { id: String(nextId++), delete: async () => {} };
    },
    messages: { edit: async (mid, payload) => { ch.edits.push([mid, payload]); } },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}
function fakeClient(guild) {
  return {
    user: { id: 'BOT' },
    guilds: { cache: new Map([[guild.id, guild]]) },
    channels: { cache: guild.channels.cache, fetch: async (cid) => { throw Object.assign(new Error('Unknown Channel'), { code: 10003, cid }); } },
  };
}
function fakeInteraction(extra = {}) {
  return {
    createdTimestamp: Date.now(), deferred: false, replied: false, ephemeral: undefined, replies: [], modals: [],
    async deferReply({ ephemeral }) { this.deferred = true; this.ephemeral = Boolean(ephemeral); },
    async editReply(p) { this.replies.push(p); return {}; },
    async reply(p) { this.replied = true; this.replies.push(p); return {}; },
    async followUp(p) { this.replies.push(p); return {}; },
    async showModal(m) { this.modals.push(m); },
    ...extra,
  };
}
const embedText = (payload) => payload.embeds.map((e) => `${e.toJSON().title ?? ''}\n${e.toJSON().description ?? ''}`).join('\n');

/** An open event row in the in-memory table. */
async function seedEvent(over = {}) {
  return prisma.guildEvent.create({
    data: {
      guildId: 'G', title: 'Movie night', hostId: 'HOST', channelId: 'C1', startsAt: future(2 * DAY), messageId: `M${seq}`, ...over,
    },
  });
}
/** An RSVP row; `extra` carries `roleGranted: true` for a member the bot gave the event role to. */
const going = (eventId, userId, status = 'GOING', extra = {}) => prisma.eventRsvp.upsert({
  where: { eventId_userId: { eventId, userId } }, create: { eventId, userId, status, ...extra }, update: { status, ...extra },
});

const resolvedUtc = { zone: 'UTC', source: 'default', roleId: null, roleCount: 0 };

// ── Pure rules ────────────────────────────────────────────────────────────────

test('parseDurationInput: blank is none, 5 minutes to 14 days otherwise', () => {
  assert.deepEqual(svc.parseDurationInput(''), { ms: null });
  assert.deepEqual(svc.parseDurationInput(null), { ms: null });
  assert.deepEqual(svc.parseDurationInput(' 2h '), { ms: 2 * HOUR });
  assert.deepEqual(svc.parseDurationInput('90m'), { ms: 90 * 60_000 });
  assert.deepEqual(svc.parseDurationInput('1m'), { error: 'too-short' });
  assert.deepEqual(svc.parseDurationInput('15d'), { error: 'too-long' });
  assert.deepEqual(svc.parseDurationInput('14d'), { ms: 14 * DAY });
  assert.deepEqual(svc.parseDurationInput('soonish'), { error: 'invalid' });
});

test('startProblem refuses the past and more than a year ahead', () => {
  const now = 1_800_000_000_000;
  assert.equal(svc.startProblem(new Date(now - 1), now), 'past');
  assert.equal(svc.startProblem(new Date(now), now), 'past');
  assert.equal(svc.startProblem(new Date(now + 1000), now), null);
  assert.equal(svc.startProblem(new Date(now + 366 * DAY), now), 'far');
  assert.equal(svc.startProblem(new Date('nope'), now), 'past');
});

test('eventEnd and remindAt', () => {
  const startsAt = new Date('2026-12-01T18:00:00Z');
  assert.equal(svc.eventEnd({ startsAt, endsAt: null }).toISOString(), '2026-12-01T20:00:00.000Z', 'no duration: start + 2 h');
  assert.equal(svc.eventEnd({ startsAt, endsAt: new Date('2026-12-01T19:00:00Z') }).toISOString(), '2026-12-01T19:00:00.000Z');
  assert.equal(svc.remindAt({ startsAt, reminderMinutes: 15 }).toISOString(), '2026-12-01T17:45:00.000Z');
  assert.equal(svc.remindAt({ startsAt, reminderMinutes: 0 }), null);
});

test('formatStartForEdit reads back through parseWhen in any zone', () => {
  const date = new Date('2026-11-05T01:30:00Z');
  for (const zone of ['UTC', 'America/Chicago', 'Europe/London', 'Asia/Kolkata', 'Etc/GMT-2', 'Australia/Sydney']) {
    const text = svc.formatStartForEdit(date, zone);
    assert.match(text, /^\d{4}-\d\d-\d\d \d\d:\d\d \S+$/, text);
    assert.equal(parseWhen(text, 'UTC')?.date.getTime(), date.getTime(), text);
  }
});

test('parseEventFields: zone, past, far, duration and Discord-event rules', () => {
  const now = Date.parse('2026-10-20T12:00:00Z');
  const ok = svc.parseEventFields({ title: ' Movie ', start: '2026-10-25 19:00', duration: '2h', description: ' hi ', location: ' Voice 1 ' }, { resolved: resolvedUtc, now });
  assert.equal(ok.ok, true);
  assert.equal(ok.values.title, 'Movie');
  assert.equal(ok.values.description, 'hi');
  assert.equal(ok.values.location, 'Voice 1');
  assert.equal(ok.values.startsAt.toISOString(), '2026-10-25T19:00:00.000Z');
  assert.equal(ok.values.endsAt.toISOString(), '2026-10-25T21:00:00.000Z');

  const chicago = svc.parseEventFields({ title: 'T', start: '2026-10-25 19:00' }, { resolved: { ...resolvedUtc, zone: 'America/Chicago' }, now });
  assert.equal(chicago.values.startsAt.toISOString(), '2026-10-26T00:00:00.000Z', "read in the caller's zone");
  assert.equal(chicago.values.endsAt, null);
  const named = svc.parseEventFields({ title: 'T', start: '2026-10-25 19:00 Europe/London' }, { resolved: resolvedUtc, now });
  assert.equal(named.values.zoneFromInput, true);

  const bad = (fields, ctx = {}) => svc.parseEventFields({ title: 'T', start: '2026-10-25 19:00', ...fields }, { resolved: resolvedUtc, now, ...ctx });
  assert.equal(bad({ start: '2026-10-19 19:00' }).title, 'Start In The Past');
  assert.equal(bad({ start: '2028-01-01 10:00' }).title, 'Start Too Far Away');
  assert.equal(bad({ start: 'blah blah' }).title, 'Could Not Read That Time');
  assert.equal(bad({ duration: '10d1y' }).title, 'Invalid Duration');
  assert.equal(bad({ duration: '30d' }).title, 'Invalid Duration');
  assert.equal(bad({ title: '  ' }).title, 'Missing Title');
  assert.equal(bad({ title: 'x'.repeat(101) }).title, 'Title Too Long');
  assert.equal(bad({ description: 'x'.repeat(1001) }).title, 'Description Too Long');
  assert.equal(bad({ location: 'x'.repeat(101) }).title, 'Location Too Long');

  // Editing: a start that already passed stays valid when unchanged; a blank location is refused with a Discord event.
  const current = { startsAt: new Date('2026-10-19T19:00:00Z'), discordEventId: 'DE1' };
  assert.equal(bad({ start: '2026-10-19 19:00', location: 'Here' }, { current }).ok, true);
  assert.equal(bad({ start: '2026-10-19 20:00', location: 'Here' }, { current }).title, 'Start In The Past');
  assert.equal(bad({ location: '' }, { current }).title, 'Location Needed');
  assert.equal(bad({ location: '' }, { current: { ...current, discordEventId: null } }).ok, true);
});

test('tallyFromGroups and the embed: counts, first 15 going, capacity, mentions', () => {
  const t = svc.tallyFromGroups([{ status: 'GOING', _count: { _all: 20 } }, { status: 'MAYBE', _count: { _all: 3 } }], Array.from({ length: 15 }, (_, i) => `U${i}`));
  assert.deepEqual([t.going, t.maybe, t.declined, t.goingIds.length], [20, 3, 0, 15]);
  const event = {
    id: 'E1', guildId: 'G', channelId: 'C', title: 'Movie night', description: 'Bring snacks', location: 'Voice 1', hostId: 'HOST',
    roleId: 'R1', capacity: 30, startsAt: future(DAY), endsAt: future(DAY + 2 * HOUR), status: 'SCHEDULED',
  };
  const json = svc.buildEventEmbed(event, t).toJSON();
  const field = (name) => json.fields.find((f) => f.name.startsWith(name));
  assert.equal(json.title, '📅 Movie night');
  assert.equal(json.description, 'Bring snacks');
  assert.match(field('When').value, /<t:\d+:F>\n<t:\d+:R>\nUntil <t:\d+:f>/);
  assert.equal(field('Where').value, 'Voice 1');
  assert.equal(field('Host').value, '<@HOST>');
  assert.equal(field('Role').value, '<@&R1>');
  assert.equal(field('✅ Going').name, '✅ Going (20 / 30)');
  assert.equal((field('✅ Going').value.match(/<@U\d+>/g) ?? []).length, 15);
  assert.match(field('✅ Going').value, /\+5 more$/);
  assert.equal(field('❔ Maybe').value, '3');
  assert.equal(json.footer.text, 'RSVP with the buttons below.');

  const empty = svc.buildEventEmbed({ ...event, location: null, roleId: null, capacity: null }, svc.emptyTally()).toJSON();
  assert.equal(empty.fields.find((f) => f.name.startsWith('✅')).value, 'No one yet');
  assert.equal(empty.fields.find((f) => f.name === 'Where'), undefined);

  const cancelled = svc.buildEventEmbed({ ...event, status: 'CANCELLED' }, t).toJSON();
  assert.equal(cancelled.title, '🚫 Cancelled: Movie night');
  assert.equal(svc.buildEventEmbed({ ...event, status: 'ENDED' }, t).toJSON().title, '🏁 Ended: Movie night');
  const now = svc.buildEventEmbed({ ...event, startsAt: new Date(Date.now() - HOUR) }, t).toJSON();
  assert.match(now.fields[0].value, /Happening now/);
});

test('the Going list keeps its mentions and names them in a subtext line (an app may not resolve a mention in an embed)', async () => {
  const event = { id: 'E1', title: 'Game night', hostId: 'HOST', startsAt: future(DAY), status: 'SCHEDULED' };
  const t = svc.tallyFromGroups([{ status: 'GOING', _count: { _all: 4 } }], ['U1', 'U2', 'U3']);
  const names = new Map([['U1', 'Rev'], ['U3', 'Frosty_*Fan*']]);
  const json = svc.buildEventEmbed(event, t, { names }).toJSON();
  const field = (name) => json.fields.find((f) => f.name.startsWith(name));
  assert.equal(field('✅ Going').value, '<@U1> <@U2> <@U3> +1 more\n-# Rev · Frosty\\_\\*Fan\\*');
  assert.equal(field('Host').value, '<@HOST>');
  assert.equal(svc.buildEventEmbed(event, t).toJSON().fields.find((f) => f.name.startsWith('✅')).value, '<@U1> <@U2> <@U3> +1 more');

  // Lookup: cached member, fetched member, then the account name of someone who left; an unknown id is left out.
  const guild = {
    members: {
      cache: new Map([['U1', { displayName: 'Rev' }]]),
      fetch: async ({ user }) => new Map(user.filter((id) => id === 'U2').map((id) => [id, { displayName: 'Mass' }])),
    },
  };
  const client = { users: { cache: new Map(), fetch: async (id) => (id === 'LEFT' ? { displayName: 'Frosty' } : Promise.reject(new Error('Unknown User'))) } };
  const got = await svc.memberNames(client, guild, ['U1', 'U2', 'LEFT', 'GHOST', 'LEFT']);
  assert.deepEqual(Object.fromEntries(got), { U1: 'Rev', U2: 'Mass', LEFT: 'Frosty' });
  // A failed member fetch still names the cached members and falls back to the account names.
  const broken = { members: { cache: new Map([['U1', { displayName: 'Rev' }]]), fetch: async () => { throw new Error('gateway down'); } } };
  assert.deepEqual(Object.fromEntries(await svc.memberNames(client, broken, ['U1', 'LEFT'])), { U1: 'Rev', LEFT: 'Frosty' });
  assert.equal((await svc.memberNames(client, null, ['U1'])).size, 0);
});

test('buttons: customIds, disabled when closed, Going disabled when full', () => {
  const event = { id: 'E1', status: 'SCHEDULED', capacity: 2 };
  const row = (tally, e = event) => svc.buildEventComponents(e, tally)[0].toJSON().components;
  const open = row({ going: 1, maybe: 0, declined: 0, goingIds: [] });
  assert.deepEqual(open.map((b) => b.custom_id), ['event:E1:going', 'event:E1:maybe', 'event:E1:declined', 'event:E1:calendar']);
  assert.deepEqual(open.map((b) => Boolean(b.disabled)), [false, false, false, false]);
  const full = row({ going: 2, maybe: 0, declined: 0, goingIds: [] });
  assert.deepEqual(full.map((b) => Boolean(b.disabled)), [true, false, false, false], 'a full event can still be added to a calendar');
  assert.equal(full[0].label, 'Going (full)');
  for (const status of ['ENDED', 'CANCELLED']) {
    assert.deepEqual(row(svc.emptyTally(), { ...event, status }).map((b) => Boolean(b.disabled)), [true, true, true, true]);
  }
  for (const b of open) assert.ok(b.custom_id.length <= 100);
  assert.ok(Object.hasOwn(CUSTOM_ID_PREFIXES, 'event'));
});

test('reminderMessages: users only, chunked under 2000 characters and 100 ids, heading first', () => {
  const event = { title: 'Movie night', startsAt: future(HOUR), guildId: 'G', channelId: 'C', messageId: '123456789012345678' };
  assert.deepEqual(svc.reminderMessages(event, []), []);
  const one = svc.reminderMessages(event, ['111111111111111111', '222222222222222222']);
  assert.equal(one.length, 1);
  assert.match(one[0].content, /^⏰ \*\*Movie night\*\* starts <t:\d+:R>\./);
  assert.match(one[0].content, /<@111111111111111111> <@222222222222222222>$/);
  assert.deepEqual(one[0].userIds, ['111111111111111111', '222222222222222222']);

  const ids = Array.from({ length: 500 }, (_, i) => String(100000000000000000n + BigInt(i)));
  const many = svc.reminderMessages(event, ids);
  assert.ok(many.length > 1);
  assert.deepEqual(many.flatMap((m) => m.userIds), ids, 'every member once, in order');
  for (const m of many) {
    assert.ok(m.content.length < 2000, `${m.content.length} chars`);
    assert.ok(m.userIds.length <= 100);
    assert.equal(m.content.match(/<@\d+>/g).length, m.userIds.length);
  }
  assert.match(many[0].content, /^⏰/);
  assert.doesNotMatch(many[1].content, /^⏰/);
});

test('discordEventOptions: external, location, end defaults to start + 1 h, limits clamped', () => {
  const startsAt = new Date('2026-12-01T18:00:00Z');
  const o = svc.discordEventOptions({ title: 'T'.repeat(150), description: 'd'.repeat(1500), location: 'L'.repeat(150), startsAt, endsAt: null });
  assert.equal(o.name.length, 100);
  assert.equal(o.description.length, 1000);
  assert.equal(o.entityMetadata.location.length, 100);
  assert.equal(o.entityType, 3);
  assert.equal(o.privacyLevel, 2);
  assert.equal(o.scheduledEndTime.toISOString(), '2026-12-01T19:00:00.000Z');
  const e = svc.discordEventOptions({ title: 'T', description: '', location: 'L', startsAt, endsAt: new Date('2026-12-01T22:00:00Z') });
  assert.equal(e.description, undefined);
  assert.equal(e.scheduledEndTime.toISOString(), '2026-12-01T22:00:00.000Z');
});

test('listLine, attendeeLines and rsvpReplyText', () => {
  const event = { id: 'E1', title: 'Movie', startsAt: future(DAY), channelId: 'C', guildId: 'G', messageId: '5', capacity: 10 };
  const line = svc.listLine(event, 4);
  assert.match(line, /\*\*Movie\*\* · <t:\d+:f> \(<t:\d+:R>\)\n✅ 4\/10 · <#C> · \[post\]\(https:\/\/discord\.com\/channels\/G\/C\/5\) · `E1`/);
  assert.deepEqual(svc.attendeeLines([{ userId: 'A', status: 'MAYBE' }, { userId: 'B', status: 'GOING' }, { userId: 'C', status: 'DECLINED' }]),
    ['✅ <@B>', '❔ <@A>', '❌ <@C>']);
  assert.match(svc.rsvpReplyText({ result: 'full' }, { title: 'Movie', capacity: 10 }, 'GOING'), /is full \(10 going\)/);
  assert.match(svc.rsvpReplyText({ result: 'set', note: 'no role' }, event, 'GOING'), /Going.*\n⚠️ no role/);
  assert.match(svc.rsvpReplyText({ result: 'same' }, event, 'MAYBE'), /already marked as \*\*Maybe\*\*/);
  assert.equal(svc.rsvpReplyText({ result: 'gone' }, null, 'GOING'), 'This event no longer exists.');
});

// ── Creating ──────────────────────────────────────────────────────────────────

const baseData = (over = {}) => ({
  title: 'Movie night', description: 'Snacks', location: 'Voice 1', startsAt: future(2 * DAY), endsAt: future(2 * DAY + 2 * HOUR),
  hostId: 'HOST', roleId: null, capacity: null, reminderMinutes: 15, ...over,
});

test('createEvent: row, post with buttons, stored ids, Discord event, both jobs', async () => {
  reset();
  await configure('G');
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const data = baseData();
  const out = await svc.createEvent({ client: fakeClient(guild), guild, channel, settings: { eventCreateDiscordEvent: true }, data });
  assert.equal(out.ok, true);
  assert.deepEqual(out.notes, []);
  const { event } = out;
  assert.equal(channel.sent.length, 1);
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [] });
  assert.equal(channel.sent[0].components[0].toJSON().components.length, 4, 'three RSVP buttons and Add to calendar');
  assert.ok(event.messageId, 'message id stored');
  assert.equal(event.discordEventId, 'DE1');
  assert.equal(guild.scheduledEvents.created[0].entityMetadata.location, 'Voice 1');
  assert.equal(guild.scheduledEvents.created[0].scheduledEndTime.getTime(), data.endsAt.getTime());
  const end = jobCalls.schedule.find((j) => j.type === 'event.end');
  const remind = jobCalls.schedule.find((j) => j.type === 'event.remind');
  assert.equal(end.dedupeKey, `event.end:${event.id}`);
  assert.equal(end.runAt.getTime(), data.endsAt.getTime());
  assert.equal(remind.dedupeKey, `event.remind:${event.id}`);
  assert.equal(remind.runAt.getTime(), data.startsAt.getTime() - 15 * 60_000);
  assert.deepEqual(end.payload, { eventId: event.id });
});

test('createEvent: no duration ends at start + 2 h; reminder 0 or already past is cancelled, not scheduled', async () => {
  reset();
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const startsAt = future(10 * 60_000);
  const out = await svc.createEvent({ client: fakeClient(guild), guild, channel, settings: null, data: baseData({ startsAt, endsAt: null, reminderMinutes: 15 }) });
  assert.equal(out.ok, true);
  assert.equal(jobCalls.schedule.find((j) => j.type === 'event.end').runAt.getTime(), startsAt.getTime() + 2 * HOUR);
  assert.equal(jobCalls.schedule.some((j) => j.type === 'event.remind'), false, 'the reminder time has passed');
  assert.deepEqual(jobCalls.cancel, [{ dedupeKey: `event.remind:${out.event.id}` }]);

  jobCalls.schedule.length = 0;
  await svc.createEvent({ client: fakeClient(guild), guild, channel, settings: null, data: baseData({ reminderMinutes: 0 }) });
  assert.equal(jobCalls.schedule.some((j) => j.type === 'event.remind'), false);
});

test('createEvent: the Discord event needs the setting, a location and Manage Events; failures are reported, not fatal', async () => {
  reset();
  const run = async (guildOpts, settings, data) => {
    const guild = fakeGuild(guildOpts);
    const out = await svc.createEvent({ client: fakeClient(guild), guild, channel: fakeChannel(guild), settings, data: baseData(data) });
    return { out, guild };
  };
  const off = await run({}, { eventCreateDiscordEvent: false }, {});
  assert.equal(off.out.ok, true);
  assert.deepEqual([off.out.notes, off.guild.scheduledEvents.created.length], [[], 0]);

  const noLocation = await run({}, { eventCreateDiscordEvent: true }, { location: null });
  assert.match(noLocation.out.notes[0], /needs a location/);
  assert.equal(noLocation.out.event.discordEventId, null);

  const noPerm = await run({ manageEvents: false }, { eventCreateDiscordEvent: true }, {});
  assert.match(noPerm.out.notes[0], /Manage Events/);
  assert.equal(noPerm.guild.scheduledEvents.created.length, 0);

  const failed = await run({ eventFails: true }, { eventCreateDiscordEvent: true }, {});
  assert.equal(failed.out.ok, true, 'the event is still created');
  assert.match(failed.out.notes[0], /could not create the Discord event/);
  assert.ok(failed.out.event.messageId);
});

test('createEvent: a failed post removes the row again', async () => {
  reset();
  const guild = fakeGuild();
  const out = await svc.createEvent({ client: fakeClient(guild), guild, channel: fakeChannel(guild, 'C1', { sendFails: true }), settings: null, data: baseData() });
  assert.deepEqual([out.ok, out.code], [false, 'SEND']);
  assert.equal(events.size, 0);
  assert.equal(jobCalls.schedule.length, 0);
});

test('createEvent: at most EVENTS_ACTIVE open events; closed ones do not count; overdue ones are ended to make room', async () => {
  reset();
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  for (let i = 0; i < EVENT.ACTIVE_MAX; i += 1) await seedEvent({ title: `E${i}` });
  assert.equal(EVENT.ACTIVE_MAX, LIMITS.EVENTS_ACTIVE);
  const refused = await svc.createEvent({ client, guild, channel, settings: null, data: baseData() });
  assert.deepEqual([refused.ok, refused.code], [false, 'LIMIT']);
  assert.equal(channel.sent.length, 0, 'nothing is posted');

  const [first] = [...events.values()];
  first.status = 'CANCELLED';
  assert.equal((await svc.createEvent({ client, guild, channel, settings: null, data: baseData() })).ok, true, 'a closed event frees a place');

  // Full again: one of them is long over (its end job never ran).
  const overdue = [...events.values()].find((e) => e.status === 'SCHEDULED');
  overdue.startsAt = future(-3 * DAY);
  overdue.endsAt = future(-3 * DAY + HOUR);
  const healed = await svc.createEvent({ client, guild, channel, settings: null, data: baseData({ title: 'After' }) });
  assert.equal(healed.ok, true, 'the overdue event was ended and made room');
  assert.equal(overdue.status, 'ENDED');
});

// ── RSVP ──────────────────────────────────────────────────────────────────────

function rsvpInteraction(eventId, action, userId, { guild, member } = {}) {
  return fakeInteraction({
    customId: `event:${eventId}:${action}`, guildId: guild.id, guild, user: { id: userId },
    member: member ?? fakeMember(userId),
  });
}
const lastReply = (i) => i.replies.at(-1)?.content ?? '';

test('RSVP: capacity is exact under concurrency, leaving frees a place, same answer is a no-op', async () => {
  reset();
  const guild = fakeGuild();
  const client = fakeClient(guild);
  const event = await seedEvent({ capacity: 2 });
  const results = await Promise.all(['A', 'B', 'C', 'D', 'E'].map((u) => svc.applyRsvp({ eventId: event.id, guildId: 'G', userId: u, status: 'GOING' })));
  assert.equal(results.filter((r) => r.result === 'set').length, 2, 'exactly the capacity');
  assert.equal(results.filter((r) => r.result === 'full').length, 3);
  assert.equal(rsvps.filter((r) => r.status === 'GOING').length, 2);

  const goingUser = rsvps.find((r) => r.status === 'GOING').userId;
  assert.deepEqual(await svc.applyRsvp({ eventId: event.id, guildId: 'G', userId: goingUser, status: 'GOING' }), { result: 'same', previous: 'GOING' });
  assert.equal((await svc.applyRsvp({ eventId: event.id, guildId: 'G', userId: goingUser, status: 'MAYBE' })).result, 'set');
  assert.equal((await svc.applyRsvp({ eventId: event.id, guildId: 'G', userId: 'Z', status: 'GOING' })).result, 'set', 'the freed place');
  assert.equal((await svc.applyRsvp({ eventId: event.id, guildId: 'G', userId: 'Y', status: 'MAYBE' })).result, 'set', 'maybe ignores the cap');
  assert.equal((await svc.applyRsvp({ eventId: event.id, guildId: 'OTHER', userId: 'Y', status: 'GOING' })).result, 'gone', 'another guild');
  assert.equal((await svc.applyRsvp({ eventId: 'nope', guildId: 'G', userId: 'Y', status: 'GOING' })).result, 'gone');
  events.get(event.id).status = 'ENDED';
  assert.equal((await svc.applyRsvp({ eventId: event.id, guildId: 'G', userId: 'Q', status: 'GOING' })).result, 'closed');
  assert.ok(client);
});

test('RSVP button: gives the role on Going, takes it back on leaving, asks for a render, replies privately', async () => {
  reset();
  const guild = fakeGuild();
  const client = fakeClient(guild);
  const event = await seedEvent({ roleId: 'R1' });
  const member = fakeMember('U1');
  member.roles.log = [];
  guild.members.cache.set('U1', member);

  const i1 = rsvpInteraction(event.id, 'going', 'U1', { guild, member });
  await svc.handleRsvpButton(i1, client);
  assert.equal(i1.ephemeral, true, 'deferred privately');
  assert.match(lastReply(i1), /Going.*Movie night/);
  assert.deepEqual(i1.replies.at(-1).allowedMentions, { parse: [] });
  assert.equal(member.roles.cache.has('R1'), true);
  assert.equal(rsvps[0].status, 'GOING');
  assert.equal(rsvps[0].roleGranted, true, 'the bot gave the role, so it is flagged');
  await sleep(30);
  assert.deepEqual(renders, [event.id], 'one render requested');

  const same = rsvpInteraction(event.id, 'going', 'U1', { guild, member });
  await svc.handleRsvpButton(same, client);
  assert.match(lastReply(same), /already marked as \*\*Going\*\*/);

  const leave = rsvpInteraction(event.id, 'maybe', 'U1', { guild, member });
  await svc.handleRsvpButton(leave, client);
  assert.equal(member.roles.cache.has('R1'), false, 'the role is taken back');
  assert.deepEqual(member.roles.log.map((l) => l[0]), ['add', 'remove']);
  assert.equal(rsvps[0].roleGranted, false, 'the flag is cleared with the role');
});

test('calendar entry: plain text from cached names, 1 h end without a duration, stable uid, sequence from updatedAt', () => {
  const guild = fakeGuild();
  guild.members.cache.set('111111111111111111', { displayName: 'Ana' });
  guild.channels.cache.set('333333333333333333', { name: 'general' });
  guild.roles.cache.set('777777777777777777', { name: 'Gamers' });
  const text = svc.plainDiscordText(
    'Hi <@111111111111111111> <@!444444444444444444> <@&777777777777777777> <@&888888888888888888> <#333333333333333333> '
    + '<:pog:555555555555555555> </play:666666666666666666> <t:1793905200:F>',
    guild,
  );
  assert.equal(text, 'Hi @Ana @someone @Gamers @role #general :pog: /play 2026-11-05 19:00 UTC');
  assert.equal(svc.plainDiscordText('<t:9999999999999>'), '<t:9999999999999>', 'an impossible time is left as it is');

  const startsAt = new Date('2026-11-05T19:00:00Z');
  const base = { id: 'E1', guildId: 'G', channelId: 'C1', messageId: 'M1', title: 'Movie night', startsAt, description: null };
  const entry = svc.calendarEntry(base);
  assert.equal(entry.uid, 'E1@events.discord-bot');
  assert.equal(entry.end.getTime(), startsAt.getTime() + HOUR, 'same end as the Discord event');
  assert.equal(entry.url, 'https://discord.com/channels/G/C1/M1');
  assert.equal(entry.description, '');
  assert.equal(entry.sequence, 0);
  const ends = new Date('2026-11-05T22:00:00Z');
  assert.equal(svc.calendarEntry({ ...base, endsAt: ends }).end.getTime(), ends.getTime());
  const s1 = svc.calendarEntry({ ...base, updatedAt: new Date('2026-10-22T00:00:00Z') }).sequence;
  const s2 = svc.calendarEntry({ ...base, updatedAt: new Date('2026-10-23T00:00:00Z') }).sequence;
  assert.ok(s2 > s1 && s2 < 2 ** 31);
  assert.equal(svc.discordEventOptions({ ...base, location: 'Hall' }).scheduledEndTime.getTime(), entry.end.getTime());
});

test('calendar payload: Google and Outlook link buttons within 512 characters, an .ics file, no mentions', () => {
  const event = {
    id: 'E1', guildId: 'G', channelId: 'C1', messageId: 'M1', title: '🎉 Big night: party/after-party', location: 'Hall',
    description: '🍿'.repeat(500), startsAt: future(DAY), status: 'SCHEDULED',
  };
  const payload = svc.buildCalendarPayload(event);
  const buttons = payload.components[0].toJSON().components;
  assert.deepEqual(buttons.map((b) => b.label), ['Google Calendar', 'Outlook']);
  for (const b of buttons) {
    assert.equal(b.style, 5, 'link button');
    assert.ok(b.url.length <= 512);
  }
  assert.equal(payload.files.length, 1);
  assert.equal(payload.files[0].name, 'big-night-partyafter-party.ics');
  assert.match(payload.files[0].attachment.toString('utf8'), /^BEGIN:VCALENDAR\r\n[\s\S]*UID:E1@events\.discord-bot\r\n/);
  assert.deepEqual(payload.allowedMentions, { parse: [] });
  assert.match(payload.content, /Big night/);
  assert.equal(svc.buildCalendarPayload({ ...event, title: '🎉🎉' }).files[0].name, 'event.ics');
});

test('Add to calendar button: private reply with links and file; closed and unknown events are refused', async () => {
  reset();
  const guild = fakeGuild();
  const client = fakeClient(guild);
  const event = await seedEvent();
  const i = rsvpInteraction(event.id, 'calendar', 'U1', { guild });
  await svc.handleRsvpButton(i, client);
  assert.equal(i.ephemeral, true, 'deferred privately');
  assert.equal(i.replies.length, 1);
  assert.equal(i.replies[0].files.length, 1);
  assert.equal(i.replies[0].components[0].toJSON().components.length, 2);
  assert.equal(rsvps.length, 0, 'not an RSVP');
  await sleep(10);
  assert.deepEqual(renders, [], 'the post is not re-rendered');

  events.get(event.id).status = 'CANCELLED';
  const closed = rsvpInteraction(event.id, 'calendar', 'U1', { guild });
  await svc.handleRsvpButton(closed, client);
  assert.equal(lastReply(closed), 'This event is no longer open.');
  assert.equal(closed.replies[0].files, undefined);
  const gone = rsvpInteraction('nope', 'calendar', 'U1', { guild });
  await svc.handleRsvpButton(gone, client);
  assert.equal(lastReply(gone), 'This event no longer exists.');
});

test('RSVP: a first Going or Maybe also offers the calendar; switching between them or Declined does not', async () => {
  reset();
  const guild = fakeGuild();
  const client = fakeClient(guild);
  const event = await seedEvent();
  const click = async (action, user = 'U1') => {
    const i = rsvpInteraction(event.id, action, user, { guild });
    await svc.handleRsvpButton(i, client);
    return i.replies.at(-1);
  };
  const first = await click('going');
  assert.match(first.content, /Going.*Movie night[\s\S]*calendar/);
  assert.equal(first.files.length, 1);
  assert.equal((await click('maybe')).files, undefined, 'Going -> Maybe');
  assert.equal((await click('maybe')).files, undefined, 'same answer');
  assert.equal((await click('declined')).files, undefined);
  assert.equal((await click('maybe')).files.length, 1, 'back from Declined');
  assert.equal((await click('declined', 'U2')).files, undefined, 'a first Declined');
});

test('RSVP button: Maybe never touches roles; a full event and a closed event are refused; bad ids are inactive', async () => {
  reset();
  const guild = fakeGuild();
  const client = fakeClient(guild);
  const event = await seedEvent({ roleId: 'R1', capacity: 1 });
  const a = fakeMember('A');
  await svc.handleRsvpButton(rsvpInteraction(event.id, 'maybe', 'A', { guild, member: a }), client);
  assert.equal(a.roles.cache.has('R1'), false);

  await svc.handleRsvpButton(rsvpInteraction(event.id, 'going', 'B', { guild }), client);
  const c = rsvpInteraction(event.id, 'going', 'C', { guild });
  await svc.handleRsvpButton(c, client);
  assert.match(lastReply(c), /is full \(1 going\)/);
  assert.equal(rsvps.some((r) => r.userId === 'C'), false);

  events.get(event.id).status = 'CANCELLED';
  const closed = rsvpInteraction(event.id, 'going', 'D', { guild });
  await svc.handleRsvpButton(closed, client);
  assert.equal(lastReply(closed), 'This event is no longer open.');

  const gone = rsvpInteraction('missing', 'going', 'D', { guild });
  await svc.handleRsvpButton(gone, client);
  assert.equal(lastReply(gone), 'This event no longer exists.');

  const bad = fakeInteraction({ customId: 'event:E1:flying', guildId: 'G', guild, user: { id: 'D' } });
  await svc.handleRsvpButton(bad, client);
  assert.equal(bad.replies[0].content, 'This button is no longer active.');
  assert.equal(bad.replies[0].ephemeral, true);
});

test('RSVP button: a role the bot cannot give is reported, the RSVP still counts', async () => {
  reset();
  const guild = fakeGuild({ manageRoles: false });
  const client = fakeClient(guild);
  const event = await seedEvent({ roleId: 'R1' });
  const member = fakeMember('U1');
  const i = rsvpInteraction(event.id, 'going', 'U1', { guild, member });
  await svc.handleRsvpButton(i, client);
  assert.match(lastReply(i), /\n⚠️ I could not give you the event role: I need the Manage Roles permission\./);
  assert.equal(member.roles.cache.has('R1'), false);
  assert.equal(rsvps[0].status, 'GOING');
});

test('RSVP button: a member with a role and no Going record (a failed earlier click) is repaired by clicking Going again', async () => {
  reset();
  const guild = fakeGuild();
  const client = fakeClient(guild);
  const event = await seedEvent({ roleId: 'R1' });
  await going(event.id, 'U1');
  const member = fakeMember('U1'); // is going in the database, has no role
  const i = rsvpInteraction(event.id, 'going', 'U1', { guild, member });
  await svc.handleRsvpButton(i, client);
  assert.equal(member.roles.cache.has('R1'), true);
});

test('RSVP button: a member who already held the role keeps it when they leave Going (the bot never gave it)', async () => {
  reset();
  const guild = fakeGuild();
  const client = fakeClient(guild);
  const event = await seedEvent({ roleId: 'R1' });
  const member = fakeMember('U1', { roleIds: ['R1'] }); // had the role before, for another reason
  member.roles.log = [];
  guild.members.cache.set('U1', member);

  await svc.handleRsvpButton(rsvpInteraction(event.id, 'going', 'U1', { guild, member }), client);
  assert.equal(rsvps[0].status, 'GOING');
  assert.equal(rsvps[0].roleGranted, false, 'the bot added nothing');
  await svc.handleRsvpButton(rsvpInteraction(event.id, 'declined', 'U1', { guild, member }), client);
  assert.equal(rsvps[0].status, 'DECLINED');
  assert.equal(member.roles.cache.has('R1'), true, 'still theirs');
  assert.deepEqual(member.roles.log, [], 'the bot neither added nor removed it');
});

test('RSVP button: an event that ends between the RSVP and the role being added does not leave the member with the role', async () => {
  reset();
  const guild = fakeGuild();
  const client = fakeClient(guild);
  const event = await seedEvent({ roleId: 'R1' });
  const member = fakeMember('U1');
  member.roles.log = [];
  const add = member.roles.add;
  // The end job (and its strip, which found no flagged member yet) commits while the role is being added.
  member.roles.add = async function slowAdd(roleId, reason) {
    events.get(event.id).status = 'ENDED';
    return add.call(this, roleId, reason);
  };
  guild.members.cache.set('U1', member);

  const i = rsvpInteraction(event.id, 'going', 'U1', { guild, member });
  await svc.handleRsvpButton(i, client);
  assert.equal(member.roles.cache.has('R1'), false, 'the role was taken straight back');
  assert.deepEqual(member.roles.log.map((l) => l[0]), ['add', 'remove']);
  assert.equal(rsvps[0].roleGranted, false);
  assert.match(lastReply(i), /just ended/);

  // The same click while the event is still open keeps the role and flags it.
  const open = await seedEvent({ roleId: 'R1' });
  const other = fakeMember('U2');
  guild.members.cache.set('U2', other);
  await svc.handleRsvpButton(rsvpInteraction(open.id, 'going', 'U2', { guild, member: other }), client);
  assert.equal(other.roles.cache.has('R1'), true);
  assert.equal(rsvps.find((r) => r.userId === 'U2').roleGranted, true);
});

// ── End, cancel, strip ────────────────────────────────────────────────────────

test('closeEvent: one winner, the post is edited with disabled buttons, jobs are cancelled', async () => {
  reset();
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const event = await seedEvent();
  await going(event.id, 'A');
  const [a, b] = await Promise.all([
    svc.closeEvent(client, event.id, { status: 'ENDED', reason: 'manual', guildId: 'G' }),
    svc.closeEvent(client, event.id, { status: 'CANCELLED', reason: 'manual', guildId: 'G' }),
  ]);
  assert.equal([a, b].filter((r) => r.ok).length, 1);
  assert.equal([a, b].find((r) => !r.ok).code, 'CLOSED');
  const stored = events.get(event.id).status;
  assert.ok(stored === 'ENDED' || stored === 'CANCELLED');
  assert.equal(channel.edits.length, 1);
  const buttons = channel.edits[0][1].components[0].toJSON().components;
  assert.deepEqual(buttons.map((x) => Boolean(x.disabled)), [true, true, true, true]);
  assert.ok(jobCalls.cancel.some((c) => c.dedupeKey === `event.remind:${event.id}`));
  assert.ok(jobCalls.cancel.some((c) => c.dedupeKey === `event.end:${event.id}`));
  assert.equal((await svc.closeEvent(client, 'nope', { status: 'ENDED', reason: 'manual' })).code, 'NOT_FOUND');
});

test('cancelEvent: CANCELLED, Discord event deleted, post edited, roles taken back', async () => {
  reset();
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const event = await seedEvent({ roleId: 'R1', discordEventId: 'DE1' });
  for (const u of ['A', 'B']) {
    guild.members.cache.set(u, fakeMember(u, { roleIds: ['R1'] }));
    await going(event.id, u, 'GOING', { roleGranted: true });
  }
  const out = await svc.cancelEvent(client, event.id, { guildId: 'G' });
  assert.equal(out.ok, true);
  assert.equal(events.get(event.id).status, 'CANCELLED');
  assert.deepEqual(guild.scheduledEvents.deleted, ['DE1']);
  assert.equal(events.get(event.id).discordEventId, null);
  assert.match(channel.edits[0][1].embeds[0].toJSON().title, /Cancelled/);
  assert.equal(guild.members.cache.get('A').roles.cache.has('R1'), false);
  assert.equal(guild.members.cache.get('B').roles.cache.has('R1'), false);
  assert.equal((await svc.cancelEvent(client, event.id, { guildId: 'G' })).code, 'CLOSED');
  assert.equal((await svc.cancelEvent(client, event.id, { guildId: 'OTHER' })).code, 'NOT_FOUND', 'guild-scoped');
});

test('cancelEvent and stripRoles only take the role from members the bot gave it to, and clear the flag', async () => {
  reset();
  const guild = fakeGuild();
  fakeChannel(guild);
  const client = fakeClient(guild);
  const event = await seedEvent({ roleId: 'R1' });
  const log = [];
  // A: the bot gave the role. B: held it already (not flagged). C: flagged, but the removal fails. D: flagged, left the server.
  for (const u of ['A', 'B', 'C']) {
    const m = fakeMember(u, { roleIds: ['R1'] });
    m.roles.log = log;
    guild.members.cache.set(u, m);
  }
  guild.members.cache.get('C').roles.remove = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  await going(event.id, 'A', 'GOING', { roleGranted: true });
  await going(event.id, 'B', 'GOING');
  await going(event.id, 'C', 'MAYBE', { roleGranted: true });
  await going(event.id, 'D', 'GOING', { roleGranted: true });

  const out = await svc.cancelEvent(client, event.id, { guildId: 'G' });
  assert.equal(out.ok, true);
  assert.equal(guild.members.cache.get('A').roles.cache.has('R1'), false, 'A: taken back');
  assert.equal(guild.members.cache.get('B').roles.cache.has('R1'), true, 'B: never given by the bot, kept');
  assert.equal(guild.members.cache.get('C').roles.cache.has('R1'), true, 'C: the removal failed');
  assert.deepEqual(log.map((l) => l[0]), ['remove'], 'one removal, for A');
  const flag = (u) => rsvps.find((r) => r.userId === u).roleGranted;
  assert.deepEqual(['A', 'B', 'C', 'D'].map(flag), [false, false, true, false], 'cleared, except where the removal failed (a later run retries)');
});

test('stripRoles is bounded: one batch per run with a cursor, never above the run cap', async () => {
  reset();
  const guild = fakeGuild();
  const client = fakeClient(guild);
  const event = await seedEvent({ roleId: 'R1' });
  const total = EVENT.STRIP_BATCH + 20;
  for (let i = 0; i < total; i += 1) {
    const id = `U${String(i).padStart(4, '0')}`;
    guild.members.cache.set(id, fakeMember(id, { roleIds: ['R1'] }));
    await going(event.id, id, 'GOING', { roleGranted: true });
  }
  const held = () => [...guild.members.cache.values()].filter((m) => m.roles?.cache?.has('R1')).length;
  const first = await svc.stripRoles(client, event);
  assert.equal(held(), 20, 'only one batch was handled');
  assert.ok(first.more, 'a cursor for the rest');
  const second = await svc.stripRoles(client, event, { after: first.more, run: 1 });
  assert.equal(held(), 0);
  assert.equal(second.more, null);
  assert.equal((await svc.stripRoles(client, event, { after: first.more, run: EVENT.STRIP_RUNS_MAX })).more, null);
  assert.deepEqual(await svc.stripRoles(client, { ...event, roleId: null }), { more: null });
  const noPerm = fakeGuild({ manageRoles: false });
  assert.deepEqual(await svc.stripRoles(fakeClient(noPerm), { ...event, guildId: noPerm.id }), { more: null });
});

test('event.end job: ends the event, strips the role, continues with a cursor while members remain, then stops', async () => {
  reset();
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const event = await seedEvent({ roleId: 'R1' });
  const total = EVENT.STRIP_BATCH + 5;
  for (let i = 0; i < total; i += 1) {
    const id = `U${String(i).padStart(4, '0')}`;
    guild.members.cache.set(id, fakeMember(id, { roleIds: ['R1'] }));
    await going(event.id, id, 'GOING', { roleGranted: true });
  }
  const job = { guildId: 'G', payload: { eventId: event.id } };
  const next = await svc.handleEndJob(job, { client });
  assert.equal(events.get(event.id).status, 'ENDED');
  assert.match(channel.edits[0][1].embeds[0].toJSON().title, /Ended/);
  assert.ok(next?.nextRunAt instanceof Date);
  assert.equal(next.payload.eventId, event.id);
  assert.equal(typeof next.payload.after, 'string');
  assert.equal(next.payload.run, 1);

  const done = await svc.handleEndJob({ guildId: 'G', payload: next.payload }, { client });
  assert.equal(done, undefined);
  assert.equal([...guild.members.cache.values()].filter((m) => m.roles?.cache?.has('R1')).length, 0);
  assert.equal(channel.edits.length, 1, 'the continuation does not edit the post again');

  // A retry of the first run (the status already flipped) walks the members from the start and does not throw; the flags are
  // cleared once the roles are back, so there is nothing left to take.
  const retry = await svc.handleEndJob(job, { client });
  assert.equal(retry, undefined);
  assert.equal(rsvps.filter((r) => r.roleGranted).length, 0, 'every flag is cleared');
  assert.equal(channel.edits.length, 1, 'the retry does not edit the post again');
  await assert.rejects(() => svc.handleEndJob({ guildId: 'G', payload: {} }, { client }), /missing eventId/);
});

// ── Reminder ──────────────────────────────────────────────────────────────────

test('event.remind job: pings the members going once, users only, and marks it sent', async () => {
  reset();
  await configure('G');
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const event = await seedEvent({ startsAt: future(20 * 60_000) });
  await going(event.id, '111111111111111111', 'GOING');
  await going(event.id, '222222222222222222', 'GOING');
  await going(event.id, '333333333333333333', 'MAYBE');
  await going(event.id, '444444444444444444', 'DECLINED');
  const job = { guildId: 'G', payload: { eventId: event.id } };
  await svc.handleRemindJob(job, { client, guild });
  assert.equal(channel.sent.length, 1);
  const msg = channel.sent[0];
  assert.match(msg.content, /<@111111111111111111>/);
  assert.match(msg.content, /<@222222222222222222>/);
  assert.doesNotMatch(msg.content, /3333|4444/);
  assert.deepEqual(msg.allowedMentions, { parse: [], users: ['111111111111111111', '222222222222222222'] });
  assert.ok(events.get(event.id).reminderSentAt instanceof Date);

  await svc.handleRemindJob(job, { client, guild });
  assert.equal(channel.sent.length, 1, 'a retry never pings twice');
});

test('event.remind job: chunks a long list; skips a closed event, a started one, nobody going and a module that is off', async () => {
  reset();
  await configure('G');
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const big = await seedEvent({ startsAt: future(HOUR) });
  for (let i = 0; i < 200; i += 1) await going(big.id, String(100000000000000000n + BigInt(i)));
  await svc.handleRemindJob({ guildId: 'G', payload: { eventId: big.id } }, { client, guild });
  assert.ok(channel.sent.length >= 3);
  for (const m of channel.sent) {
    assert.ok(m.content.length < 2000);
    assert.ok(m.allowedMentions.users.length <= 100);
    assert.deepEqual(m.allowedMentions.parse, []);
  }

  const run = async (over, rsvp = true) => {
    const e = await seedEvent({ startsAt: future(HOUR), ...over });
    if (rsvp) await going(e.id, '555555555555555555');
    const before = channel.sent.length;
    await svc.handleRemindJob({ guildId: 'G', payload: { eventId: e.id } }, { client, guild });
    return channel.sent.length - before;
  };
  assert.equal(await run({ status: 'CANCELLED' }), 0);
  assert.equal(await run({ startsAt: future(-HOUR) }), 0, 'already started');
  assert.equal(await run({}, false), 0, 'nobody going');
  assert.equal(await run({ reminderSentAt: new Date() }), 0);
  await configure('G', { eventsEnabled: false });
  assert.equal(await run({}), 0, 'the module is off');
  await configure('G');
  assert.equal(await run({}), 1);
  await assert.rejects(() => svc.handleRemindJob({ guildId: 'G', payload: {} }, { client, guild }), /missing eventId/);
});

test('event.remind job: a Discord event deleted or cancelled while the bot was away cancels the event instead of pinging', async () => {
  reset();
  await configure('G');
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  guild.scheduledEvents.fetch = async (id) => {
    if (id === 'GONE') throw Object.assign(new Error('Unknown Guild Scheduled Event'), { code: 10070 });
    if (id === 'FLAKY') throw Object.assign(new Error('Service Unavailable'), { status: 503 });
    return { id, status: id === 'CXL' ? GuildScheduledEventStatus.Canceled : GuildScheduledEventStatus.Scheduled };
  };
  const run = async (discordEventId) => {
    const e = await seedEvent({ startsAt: future(HOUR), discordEventId });
    await going(e.id, '555555555555555555');
    const before = channel.sent.length;
    await svc.handleRemindJob({ guildId: 'G', payload: { eventId: e.id } }, { client, guild });
    return { sent: channel.sent.length - before, row: events.get(e.id) };
  };
  for (const id of ['GONE', 'CXL']) {
    const { sent, row } = await run(id);
    assert.equal(sent, 0, `${id}: no reminder`);
    assert.equal(row.status, 'CANCELLED', `${id}: cancelled`);
    assert.equal(row.reminderSentAt, null);
    assert.ok(jobCalls.cancel.some((c) => c.dedupeKey === `event.end:${row.id}`), `${id}: the end job is removed`);
  }
  assert.equal((await run('OK')).sent, 1, 'a Discord event still scheduled');
  assert.equal((await run('FLAKY')).sent, 1, 'a failed lookup still reminds');
  assert.equal((await run(null)).sent, 1, 'no Discord event');
});

// ── Editing ───────────────────────────────────────────────────────────────────

test('editEvent: updates, reschedules both jobs, resets the reminder, updates the Discord event and the post', async () => {
  reset();
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const event = await seedEvent({ discordEventId: 'DE1', location: 'Voice 1', reminderSentAt: new Date(), endsAt: null });
  const startsAt = future(5 * DAY);
  const endsAt = new Date(startsAt.getTime() + 3 * HOUR);
  const out = await svc.editEvent({ client, guild, event, changes: { title: 'Renamed', location: 'Voice 2', startsAt, endsAt, capacity: 1, description: null } });
  assert.equal(out.ok, true);
  assert.equal(events.get(event.id).title, 'Renamed');
  assert.equal(events.get(event.id).reminderSentAt, null, 'a moved event reminds again');
  assert.equal(jobCalls.schedule.find((j) => j.type === 'event.end').runAt.getTime(), endsAt.getTime());
  assert.equal(jobCalls.schedule.find((j) => j.type === 'event.remind').runAt.getTime(), startsAt.getTime() - 15 * 60_000);
  const [id, opts] = guild.scheduledEvents.edited[0];
  assert.equal(id, 'DE1');
  assert.equal(opts.name, 'Renamed');
  assert.equal(opts.entityMetadata.location, 'Voice 2');
  assert.equal(opts.scheduledStartTime.getTime(), startsAt.getTime());
  assert.deepEqual(renders, [event.id], 'the post is re-rendered at once');
  assert.ok(channel);

  // Only text changed: the jobs and the Discord times stay as they are.
  jobCalls.schedule.length = 0;
  guild.scheduledEvents.edited.length = 0;
  const current = events.get(event.id);
  const text = await svc.editEvent({ client, guild, event: current, changes: { title: 'Again', startsAt: current.startsAt, endsAt: current.endsAt } });
  assert.equal(text.ok, true);
  assert.equal(jobCalls.schedule.length, 0);
  assert.equal('scheduledStartTime' in guild.scheduledEvents.edited[0][1], false);

  events.get(event.id).status = 'ENDED';
  assert.deepEqual(await svc.editEvent({ client, guild, event: current, changes: { title: 'Late' } }), { ok: false, code: 'CLOSED' });
  assert.equal(events.get(event.id).title, 'Again');
});

test('editEvent: moving only the end of a started event does not resend the start Discord would reject', async () => {
  reset();
  const guild = fakeGuild();
  fakeChannel(guild);
  const client = fakeClient(guild);
  const startsAt = new Date(Date.now() - HOUR); // already started
  const event = await seedEvent({
    discordEventId: 'DE1', location: 'Voice 1', status: 'ACTIVE', startsAt, endsAt: future(HOUR),
  });
  const endsAt = future(3 * HOUR);
  const out = await svc.editEvent({ client, guild, event, changes: { startsAt, endsAt } });
  assert.equal(out.ok, true);
  assert.equal(events.get(event.id).endsAt.getTime(), endsAt.getTime());
  const sent = guild.scheduledEvents.edited[0][1];
  assert.equal('scheduledStartTime' in sent, false, 'the start did not change, so it is not sent');
  assert.equal(sent.scheduledEndTime.getTime(), endsAt.getTime(), 'the new end is');

  // A moved start still sends both times.
  guild.scheduledEvents.edited.length = 0;
  const later = future(5 * DAY);
  const moved = await svc.editEvent({ client, guild, event: events.get(event.id), changes: { startsAt: later, endsAt: new Date(later.getTime() + HOUR) } });
  assert.equal(moved.ok, true);
  assert.equal(guild.scheduledEvents.edited[0][1].scheduledStartTime.getTime(), later.getTime());
  assert.equal(guild.scheduledEvents.edited[0][1].scheduledEndTime.getTime(), later.getTime() + HOUR);
});

test('editEvent: lowering the capacity below the going count keeps them and says so; a vanished Discord event is unlinked', async () => {
  reset();
  const guild = fakeGuild();
  fakeChannel(guild);
  guild.scheduledEvents.edit = async () => { throw Object.assign(new Error('Unknown Guild Scheduled Event'), { code: 10070 }); };
  const client = fakeClient(guild);
  const event = await seedEvent({ discordEventId: 'DE9', location: 'X' });
  for (const u of ['A', 'B', 'C']) await going(event.id, u);
  const out = await svc.editEvent({ client, guild, event, changes: { capacity: 1 } });
  assert.equal(out.ok, true);
  assert.ok(out.notes.some((n) => /3 members are already going/.test(n)));
  assert.ok(out.notes.some((n) => /no longer exists/.test(n)));
  assert.equal(events.get(event.id).discordEventId, null);
  assert.equal(events.get(event.id).capacity, 1);
});

test('buildEditModal: the five fields, prefilled, with the start in the editor zone and the change carried in the customId', () => {
  const event = {
    id: 'E1', title: 'Movie', description: 'Snacks', location: 'Voice 1', startsAt: new Date('2026-11-05T01:30:00Z'),
    endsAt: new Date('2026-11-05T03:30:00Z'),
  };
  const json = svc.buildEditModal(event, 'America/Chicago', { capacity: 0, reminder: null }).toJSON();
  assert.equal(json.custom_id, 'event-modal:E1:0:-');
  const inputs = json.components.map((r) => r.components[0]);
  assert.deepEqual(inputs.map((i) => i.custom_id), ['event-title', 'event-description', 'event-start', 'event-duration', 'event-location']);
  for (const id of ['event-title', 'event-description', 'event-start', 'event-duration', 'event-location']) {
    assert.equal(Object.hasOwn(CUSTOM_ID_PREFIXES, id), true);
    assert.equal(CUSTOM_ID_PREFIXES[id].kind, 'field');
  }
  assert.equal(inputs[2].value, '2026-11-04 19:30 America/Chicago');
  assert.equal(inputs[3].value, '2h');
  assert.equal(inputs[0].value, 'Movie');
  assert.equal(json.custom_id.length <= 100, true);
  const blank = svc.buildEditModal({ ...event, endsAt: null, description: null, location: null }, 'UTC').toJSON();
  assert.equal(blank.custom_id, 'event-modal:E1:-:-');
  assert.equal(blank.components[3].components[0].value, undefined);
});

function modalInteraction(eventId, fields, { mod = true, customId = `event-modal:${eventId}:-:-`, guild = fakeGuild() } = {}) {
  return fakeInteraction({
    customId, guildId: 'G', guild, user: { id: 'MOD' }, member: fakeMember('MOD', { mod }),
    inCachedGuild: () => true,
    fields: { getTextInputValue: (id) => fields[id] ?? '' },
  });
}

test('edit form: needs the Moderator level (the router does not check components), saves, refuses a past start', async () => {
  reset();
  await configure('G');
  const guild = fakeGuild();
  fakeChannel(guild);
  const client = fakeClient(guild);
  const event = await seedEvent();
  const fields = {
    'event-title': 'Renamed', 'event-description': '', 'event-start': svc.formatStartForEdit(future(4 * DAY), 'UTC'),
    'event-duration': '90m', 'event-location': 'Voice 3',
  };

  const member = modalInteraction(event.id, fields, { mod: false, guild });
  await svc.handleEditModal(member, client);
  assert.match(embedText(member.replies[0]), /Moderator level/);
  assert.equal(events.get(event.id).title, 'Movie night', 'nothing changed');

  const past = modalInteraction(event.id, { ...fields, 'event-start': '2020-01-01 10:00' }, { guild });
  await svc.handleEditModal(past, client);
  assert.match(embedText(past.replies[0]), /Start In The Past/);
  assert.match(embedText(past.replies[0]), /Nothing was saved/);

  const ok = modalInteraction(event.id, fields, { guild, customId: `event-modal:${event.id}:0:30` });
  await svc.handleEditModal(ok, client);
  assert.match(embedText(ok.replies[0]), /Event Updated/);
  const row = events.get(event.id);
  assert.equal(row.title, 'Renamed');
  assert.equal(row.location, 'Voice 3');
  assert.equal(row.capacity, null, '0 = no limit');
  assert.equal(row.reminderMinutes, 30);
  assert.equal(row.endsAt.getTime() - row.startsAt.getTime(), 90 * 60_000);

  const expired = modalInteraction(event.id, fields, { guild, customId: 'event-modal' });
  await svc.handleEditModal(expired, client);
  assert.match(embedText(expired.replies[0]), /Form Expired/);

  events.get(event.id).status = 'CANCELLED';
  const closed = modalInteraction(event.id, fields, { guild });
  await svc.handleEditModal(closed, client);
  assert.match(embedText(closed.replies[0]), /Event Closed/);
});

test('a start typed as "in 3d" is stored to the whole minute', () => {
  const now = Date.parse('2026-10-20T12:00:37.123Z');
  assert.notEqual(parseWhen('in 3d', 'UTC', { now: new Date(now) }).date.getTime() % 60_000, 0, 'parseWhen keeps the seconds');
  const out = svc.parseEventFields({ title: 'T', start: 'in 3d', duration: '2h' }, { resolved: resolvedUtc, now });
  assert.equal(out.values.startsAt.toISOString(), '2026-10-23T12:00:00.000Z');
  assert.equal(out.values.endsAt.toISOString(), '2026-10-23T14:00:00.000Z');
});

test('edit form returned untouched: a start with seconds (upcoming or already started) is not a change', async () => {
  reset();
  await configure('G');
  const guild = fakeGuild();
  fakeChannel(guild);
  const client = fakeClient(guild);
  const sent = new Date(Date.now() - 5 * 60_000);
  const formFields = (event, over = {}) => {
    const inputs = svc.buildEditModal(event, 'UTC').toJSON().components.map((r) => r.components[0]);
    return { ...Object.fromEntries(inputs.map((i) => [i.custom_id, i.value ?? ''])), ...over };
  };

  for (const startsAt of [new Date(future(2 * DAY).getTime() + 37_123), new Date(Date.now() - HOUR + 12_345)]) {
    jobCalls.schedule.length = 0;
    guild.scheduledEvents.edited.length = 0;
    const event = await seedEvent({ startsAt, endsAt: null, discordEventId: 'DE1', location: 'Voice 1', reminderSentAt: sent, reminderMinutes: 15 });
    const submit = async (customId, over) => {
      const i = modalInteraction(event.id, formFields(event, over), { guild, customId });
      await svc.handleEditModal(i, client);
      return embedText(i.replies[0]);
    };

    assert.match(await submit(`event-modal:${event.id}:-:-`, { 'event-title': 'Renamed' }), /Event Updated/);
    const row = events.get(event.id);
    assert.equal(row.title, 'Renamed');
    assert.equal(row.startsAt.getTime(), startsAt.getTime(), 'the stored start is kept as it was');
    assert.equal(row.reminderSentAt?.getTime(), sent.getTime(), 'no second ping');
    assert.equal(jobCalls.schedule.length, 0, 'no job is rescheduled');
    assert.equal('scheduledStartTime' in guild.scheduledEvents.edited[0][1], false, 'the Discord times are not resent');

    // The same reminder value is not a change either; a different one is.
    await submit(`event-modal:${event.id}:-:15`, {});
    assert.equal(events.get(event.id).reminderSentAt?.getTime(), sent.getTime());
    assert.equal(jobCalls.schedule.length, 0);
    await submit(`event-modal:${event.id}:-:30`, {});
    assert.equal(events.get(event.id).reminderMinutes, 30);
    assert.equal(events.get(event.id).reminderSentAt, null);
  }
});

// ── Cleanup hooks ─────────────────────────────────────────────────────────────

test('messageDelete: the post is forgotten, the event stays open', async () => {
  reset();
  const event = await seedEvent({ messageId: 'M42' });
  const other = await seedEvent({ messageId: 'M43' });
  await svc.onMessageDelete({ guildId: 'G', message: { id: 'M42' } });
  assert.equal(events.get(event.id).messageId, null);
  assert.equal(events.get(event.id).status, 'SCHEDULED');
  assert.equal(events.get(other.id).messageId, 'M43');
  await svc.onMessageDelete({ guildId: 'OTHER', message: { id: 'M43' } });
  assert.equal(events.get(other.id).messageId, 'M43', 'guild-scoped');
  await svc.onMessageDelete({ guildId: 'G', message: {} });
});

test('channelDelete: open events posted there are cancelled (jobs, Discord event, roles); others are untouched', async () => {
  reset();
  const guild = fakeGuild();
  const client = fakeClient(guild);
  const doomed = await seedEvent({ channelId: 'GONE', roleId: 'R1', discordEventId: 'DE5' });
  const ended = await seedEvent({ channelId: 'GONE', status: 'ENDED' });
  const elsewhere = await seedEvent({ channelId: 'C9' });
  guild.members.cache.set('A', fakeMember('A', { roleIds: ['R1'] }));
  await going(doomed.id, 'A', 'GOING', { roleGranted: true });
  await svc.onChannelDelete({ client, guildId: 'G', channel: { id: 'GONE' } });
  assert.equal(events.get(doomed.id).status, 'CANCELLED');
  assert.equal(events.get(ended.id).status, 'ENDED');
  assert.equal(events.get(elsewhere.id).status, 'SCHEDULED');
  assert.deepEqual(guild.scheduledEvents.deleted, ['DE5']);
  assert.equal(guild.members.cache.get('A').roles.cache.has('R1'), false);
  assert.ok(jobCalls.cancel.some((c) => c.dedupeKey === `event.end:${doomed.id}`));
});

test('Discord event cancelled in Discord: its open event is cancelled before the start; others and the bot\'s own deletes are untouched', async () => {
  reset();
  await configure('G');
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const doomed = await seedEvent({ discordEventId: 'DE7', roleId: 'R1' });
  const started = await seedEvent({ discordEventId: 'DE8', startsAt: future(-HOUR) });
  const elsewhere = await seedEvent({ guildId: 'OTHER', discordEventId: 'DE7' });
  const unlinked = await seedEvent();
  guild.members.cache.set('A', fakeMember('A', { roleIds: ['R1'] }));
  await going(doomed.id, 'A', 'GOING', { roleGranted: true });

  await svc.onDiscordEventCancelled({ client, guildId: 'G', scheduledEvent: { id: 'DE7' } });
  assert.equal(events.get(doomed.id).status, 'CANCELLED');
  assert.ok(jobCalls.cancel.some((c) => c.dedupeKey === `event.remind:${doomed.id}`), 'the reminder is removed');
  assert.equal(guild.members.cache.get('A').roles.cache.has('R1'), false, 'the role is taken back');
  assert.equal(channel.edits.length, 1, 'the post shows the cancel');
  assert.equal(events.get(elsewhere.id).status, 'SCHEDULED', 'guild-scoped');
  assert.equal(events.get(unlinked.id).status, 'SCHEDULED');

  await svc.onDiscordEventCancelled({ client, guildId: 'G', scheduledEvent: { id: 'DE8' } });
  assert.equal(events.get(started.id).status, 'SCHEDULED', 'a started event is left to its end job');

  // /event cancel deletes the Discord event after its status change: the echo finds nothing open.
  const own = await seedEvent({ discordEventId: 'DE9' });
  await svc.cancelEvent(client, own.id, { guildId: 'G' });
  const edits = channel.edits.length;
  await svc.onDiscordEventCancelled({ client, guild, scheduledEvent: { id: 'DE9' } });
  assert.equal(channel.edits.length, edits, 'no second close');
  await svc.onDiscordEventCancelled({ client, guildId: 'G', scheduledEvent: {} });

  const update = plugin.hooks.find((h) => h.event === 'guildScheduledEventUpdate');
  assert.equal(update.filter({ scheduledEvent: { status: GuildScheduledEventStatus.Canceled } }), true);
  assert.equal(update.filter({ scheduledEvent: { status: GuildScheduledEventStatus.Scheduled } }), false);
  assert.equal(update.filter({ scheduledEvent: { status: GuildScheduledEventStatus.Completed } }), false);
});

test('roleDelete: events stop giving the role', async () => {
  reset();
  const a = await seedEvent({ roleId: 'R1' });
  const b = await seedEvent({ roleId: 'R2' });
  await svc.onRoleDelete({ guildId: 'G', role: { id: 'R1' } });
  assert.equal(events.get(a.id).roleId, null);
  assert.equal(events.get(b.id).roleId, 'R2');
  await svc.onRoleDelete({ guild: { id: 'G' }, role: { id: 'R2' } });
  assert.equal(events.get(b.id).roleId, null);
});

// ── Render throttle ───────────────────────────────────────────────────────────

test('requestRender: the first request renders at once, a burst collapses, later requests wait out the window, cancel drops them', async () => {
  reset();
  const mine = (id) => renders.filter((x) => x === id).length;
  svc.requestRender({}, 'EX1');
  svc.requestRender({}, 'EX1');
  svc.requestRender({}, 'EX1');
  await sleep(40);
  assert.equal(mine('EX1'), 1, 'one render for the burst');
  svc.requestRender({}, 'EX1');
  await sleep(40);
  assert.equal(mine('EX1'), 1, 'inside the window the next render is queued, not run');
  svc.cancelRender('EX1');
  svc.requestRender({}, 'EX2');
  svc.cancelRender('EX2');
  await sleep(40);
  assert.equal(mine('EX2'), 0, 'a cancelled render never runs');
});

// ── Autocomplete and queries ──────────────────────────────────────────────────

test('autocomplete: edit and cancel offer open events, info offers closed ones too; filtered by title', async () => {
  reset();
  await seedEvent({ title: 'Movie night' });
  await seedEvent({ title: 'Game night', status: 'ENDED' });
  await seedEvent({ title: 'Other guild', guildId: 'OTHER' });
  const run = async (sub, value) => {
    let choices = null;
    await cmd.autocomplete({
      guildId: 'G', createdTimestamp: Date.now(),
      options: { getFocused: () => ({ name: 'event', value }), getSubcommand: () => sub },
      respond: async (c) => { choices = c; },
      responded: false,
    });
    return choices;
  };
  assert.deepEqual((await run('edit', '')).map((c) => c.name.slice(0, 14)), ['🟢 Movie night']);
  assert.equal((await run('info', '')).length, 2);
  assert.equal((await run('info', 'game')).length, 1);
  assert.match((await run('info', 'game'))[0].name, /^🏁 Game night · \d{4}-\d\d-\d\d$/);
  const { events: open, going: counts } = await svc.listOpenEvents('G');
  assert.equal(open.length, 1);
  assert.equal(counts.size, 0);
});

test('resolveEventRef: an id of this guild, or a link to its post; nothing from another guild', async () => {
  reset();
  const event = await seedEvent({ messageId: '123456789012345678' });
  assert.equal((await svc.resolveEventRef('G', event.id)).id, event.id);
  assert.equal((await svc.resolveEventRef('G', ` ${event.id} `)).id, event.id);
  assert.equal(await svc.resolveEventRef('OTHER', event.id), null);
  assert.equal((await svc.resolveEventRef('G', 'https://discord.com/channels/G/C1/123456789012345678')) ?? null, null, 'ids are snowflakes');
  const snow = await seedEvent({ guildId: '111111111111111111', messageId: '333333333333333333' });
  assert.equal((await svc.resolveEventRef('111111111111111111', 'https://discord.com/channels/111111111111111111/222222222222222222/333333333333333333')).id, snow.id);
  assert.equal(await svc.resolveEventRef('999999999999999999', 'https://discord.com/channels/111111111111111111/222222222222222222/333333333333333333'), null);
  assert.equal(await svc.resolveEventRef('G', ''), null);
});

// ── Declarations ──────────────────────────────────────────────────────────────

test('RAW_SQL builders return Prisma.Sql with the row lock', () => {
  assert.deepEqual(svc.RAW_SQL.map((r) => r.name), ['events.createLock', 'events.rowLock']);
  for (const { build } of svc.RAW_SQL) {
    const q = build();
    assert.equal(typeof q.sql, 'string');
    assert.doesNotMatch(q.sql, /\bnow\(\)/i);
  }
  assert.match(svc.RAW_SQL[1].build().sql, /FROM "GuildEvent" WHERE id = \? FOR UPDATE/);
});

test('plugin declaration: hooks (always), components, jobs match the registries', async () => {
  assert.equal(plugin.id, 'events');
  assert.equal(plugin.module, 'events');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id, h.always]), [
    ['messageDelete', 'events', true], ['channelDelete', 'events', true], ['roleDelete', 'events', true],
    ['guildScheduledEventUpdate', 'events', true], ['guildScheduledEventDelete', 'events', true],
  ]);
  for (const h of plugin.hooks) {
    assert.ok(HOOK_ORDER[h.event].includes('events'), `${h.event} lists events`);
    assert.equal(typeof h.run, 'function');
  }
  assert.equal(HOOK_ORDER.roleDelete.indexOf('events') < HOOK_ORDER.roleDelete.indexOf('role-persist'), true);
  const del = plugin.hooks.find((h) => h.event === 'messageDelete');
  assert.equal(del.filter({ message: { author: { id: 'BOT' } }, client: { user: { id: 'BOT' } } }), true);
  assert.equal(del.filter({ message: { partial: true }, client: { user: { id: 'BOT' } } }), true);
  assert.equal(del.filter({ message: { author: { id: 'U' } }, client: { user: { id: 'BOT' } } }), false);
  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix]), [['button', 'event'], ['modal', 'event-modal']]);
  for (const c of plugin.components) assert.equal(CUSTOM_ID_PREFIXES[c.prefix].owner, 'events');
  assert.deepEqual(plugin.jobs.map((j) => j.type), ['event.remind', 'event.end']);
  assert.equal(JOB_TYPES['event.remind'].dedupe, 'event.remind:{eventId}');
  assert.equal(JOB_TYPES['event.end'].dedupe, 'event.end:{eventId}');
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'events'), 'collectPlugins keeps it');
  assert.deepEqual(errors.filter((e) => /\bevents\b/.test(String(e?.message ?? e))), []);
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/event payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'event');
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ModerateMembers));
  assert.deepEqual(json.contexts, [0]);
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['create', 1], ['edit', 1], ['cancel', 1], ['list', 1], ['info', 1], ['attendees', 1]]);
  const sub = Object.fromEntries(json.options.map((o) => [o.name, o]));
  assert.deepEqual(sub.create.options.map((o) => o.name), ['title', 'start', 'duration', 'description', 'location', 'channel', 'role', 'capacity', 'reminder']);
  assert.deepEqual(sub.create.options.map((o) => Boolean(o.required)), [true, true, false, false, false, false, false, false, false]);
  assert.deepEqual(sub.create.options.find((o) => o.name === 'channel').channel_types, [0, 5]);
  assert.equal(sub.create.options.find((o) => o.name === 'capacity').max_value, EVENT.CAPACITY_MAX);
  assert.equal(sub.create.options.find((o) => o.name === 'reminder').min_value, 0);
  for (const name of ['edit', 'cancel', 'info', 'attendees']) {
    const o = sub[name].options.find((x) => x.name === 'event');
    assert.equal(o.autocomplete, true, name);
    assert.equal(o.required, true, name);
  }
  assert.deepEqual(sub.edit.options.map((o) => o.name), ['event', 'capacity', 'reminder']);
  assert.ok(charCount(json) <= 2000, `payload is ${charCount(json)} chars`);
  assert.ok(charCount(json) <= 4000);
  assert.equal(cmd.module, 'events');
  assert.equal(cmd.gated, true);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.MODERATOR);
  assert.equal(typeof cmd.autocomplete, 'function');
});

// ── Commands ──────────────────────────────────────────────────────────────────

function optionsOf(values) {
  const get = (k) => values[k] ?? null;
  return {
    getSubcommand: () => values.__sub,
    getString: (k, req) => { if (req && get(k) == null) throw new Error(`missing ${k}`); return get(k); },
    getInteger: get, getChannel: get, getRole: get,
  };
}

test('/event create: reads the start in the caller zone, posts, replies privately; refuses the past, a bad role and a locked channel', async () => {
  reset();
  await configure('G', { eventReminderMinutes: 30 });
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const mod = fakeMember('MOD', { mod: true, manageRoles: true });
  const run = async (values, extra = {}) => {
    const interaction = fakeInteraction({
      guildId: 'G', guild, channel, channelId: 'C1', user: { id: 'MOD' }, member: mod,
      options: optionsOf({ __sub: 'create', title: 'Movie night', start: svc.formatStartForEdit(future(3 * DAY), 'UTC'), ...values }),
      ...extra,
    });
    await cmd.execute(interaction, client);
    return interaction;
  };

  const ok = await run({ duration: '2h', location: 'Voice 1', capacity: 12 });
  assert.equal(ok.ephemeral, true);
  assert.match(embedText(ok.replies[0]), /Event Created/);
  assert.match(embedText(ok.replies[0]), /pinged 30 minutes before/, 'the server default reminder');
  assert.equal(channel.sent.length, 1);
  const row = [...events.values()][0];
  assert.deepEqual([row.title, row.capacity, row.hostId, row.reminderMinutes, row.channelId], ['Movie night', 12, 'MOD', 30, 'C1']);
  assert.equal(guild.scheduledEvents.created.length, 1);

  const past = await run({ start: '2020-01-01 10:00' });
  assert.match(embedText(past.replies[0]), /Start In The Past/);
  assert.equal(channel.sent.length, 1);

  const bad = await run({ role: { id: 'R1' } }, { member: Object.assign(fakeMember('MOD', { mod: true, manageRoles: true }), { roles: { cache: new Map(), highest: { position: 1 } } }) });
  assert.match(embedText(bad.replies[0]), /Invalid Role/);
  assert.match(embedText(bad.replies[0]), /above your highest role/);

  const good = await run({ role: { id: 'R1' }, reminder: 0, title: 'With role' });
  assert.match(embedText(good.replies[0]), /Event Created/);
  assert.equal([...events.values()].find((e) => e.title === 'With role').roleId, 'R1');
  assert.equal([...events.values()].find((e) => e.title === 'With role').reminderMinutes, 0);

  const soon = await run({ title: 'Soon', start: svc.formatStartForEdit(future(10 * 60_000), 'UTC') });
  assert.match(embedText(soon.replies[0]), /Event Created/);
  assert.match(embedText(soon.replies[0]), /reminder time has already passed/);
  assert.doesNotMatch(embedText(soon.replies[0]), /are pinged/);

  const locked = fakeChannel(guild, 'C2');
  locked.permissionsFor = (who) => ({ has: () => who?.id === 'BOT' });
  const noPost = await run({ channel: { id: 'C2' } });
  assert.match(embedText(noPost.replies[0]), /You cannot send messages in <#C2>/);
  assert.equal(locked.sent.length, 0);
});

test('/event create: attaching a role needs Manage Roles (the owner passes); a moderator without it cannot hand a role out', async () => {
  reset();
  await configure('G');
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const run = async (member, values = {}) => {
    const interaction = fakeInteraction({
      guildId: 'G', guild, channel, channelId: 'C1', user: { id: member.id }, member,
      options: optionsOf({ __sub: 'create', title: 'Role event', start: svc.formatStartForEdit(future(3 * DAY), 'UTC'), ...values }),
    });
    await cmd.execute(interaction, client);
    return embedText(interaction.replies[0]);
  };

  const modOnly = await run(fakeMember('MOD', { mod: true }), { role: { id: 'R1' } });
  assert.match(modOnly, /Invalid Role/);
  assert.match(modOnly, /Manage Roles/);
  assert.equal(events.size, 0, 'nothing was created');
  assert.equal(channel.sent.length, 0, 'nothing was posted');

  assert.match(await run(fakeMember('MOD', { mod: true }), {}), /Event Created/, 'no role: a moderator may still create');
  assert.equal([...events.values()].at(-1).roleId, null);

  assert.match(await run(fakeMember('MOD2', { mod: true, manageRoles: true }), { role: { id: 'R1' } }), /Event Created/);
  assert.equal([...events.values()].at(-1).roleId, 'R1');

  assert.match(await run(fakeMember('OWNER', { mod: true }), { role: { id: 'R1' } }), /Event Created/, 'the owner passes');
  assert.equal([...events.values()].at(-1).roleId, 'R1');
});

test('/event create: the announcement channel from the settings is used, then the current channel', async () => {
  reset();
  await configure('G2', { eventChannelId: 'ANN' });
  const guild = fakeGuild({ id: 'G2' });
  const here = fakeChannel(guild, 'HERE');
  const ann = fakeChannel(guild, 'ANN');
  const client = fakeClient(guild);
  const make = (title) => fakeInteraction({
    guildId: 'G2', guild, channel: here, channelId: 'HERE', user: { id: 'MOD' }, member: fakeMember('MOD', { mod: true }),
    options: optionsOf({ __sub: 'create', title, start: svc.formatStartForEdit(future(2 * DAY), 'UTC') }),
  });
  await cmd.execute(make('A'), client);
  assert.deepEqual([ann.sent.length, here.sent.length], [1, 0]);
  await configure('G2', { eventChannelId: 'DELETED' });
  await cmd.execute(make('B'), client);
  assert.deepEqual([ann.sent.length, here.sent.length], [1, 1], 'falls back to the current channel');
});

test('/event create reads a start with no zone in the caller timezone (saved zone, else the server zone)', async () => {
  reset();
  const { zonedToUtc } = await import('../src/utils/time.js');
  const guild = fakeGuild({ id: 'G3' });
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const day = future(3 * DAY).toISOString().slice(0, 10);
  const [year, month, dayOfMonth] = day.split('-').map(Number);
  const run = async (userId, settings) => {
    await configure('G3', settings);
    const before = events.size;
    await cmd.execute(fakeInteraction({
      guildId: 'G3', guild, channel, channelId: 'C1', user: { id: userId }, member: fakeMember(userId, { mod: true }),
      options: optionsOf({ __sub: 'create', title: `T ${userId}`, start: `${day} 19:00` }),
    }), client);
    assert.equal(events.size, before + 1, 'created');
    return [...events.values()].at(-1).startsAt.getTime();
  };
  tzRows.set('TZ1', 'America/Chicago');
  assert.equal(await run('TZ1', { timezone: 'Asia/Tokyo' }), zonedToUtc({ year, month, day: dayOfMonth, hour: 19 }, 'America/Chicago').getTime());
  assert.equal(await run('TZ2', { timezone: 'Asia/Tokyo' }), zonedToUtc({ year, month, day: dayOfMonth, hour: 19 }, 'Asia/Tokyo').getTime());
});

test('/event cancel, info, list and attendees: private replies and the guild scope', async () => {
  reset();
  await configure('G');
  const guild = fakeGuild();
  const channel = fakeChannel(guild);
  const client = fakeClient(guild);
  const event = await seedEvent({ title: 'Movie night', capacity: 5, messageId: '123456789012345678', location: 'Voice 1' });
  await going(event.id, 'A');
  await going(event.id, 'B', 'MAYBE');
  const call = async (sub, values = {}) => {
    const i = fakeInteraction({ guildId: 'G', guild, channel, user: { id: 'MOD' }, member: fakeMember('MOD', { mod: true }), options: optionsOf({ __sub: sub, ...values }) });
    await cmd.execute(i, client);
    return i;
  };

  const info = await call('info', { event: event.id });
  assert.equal(info.ephemeral, true);
  const text = embedText(info.replies[0]);
  assert.match(text, /Movie night/);
  assert.match(text, /\*\*Going:\*\* 1\/5 · \*\*Maybe:\*\* 1/);
  assert.match(text, /\*\*Where:\*\* Voice 1/);
  assert.match(text, new RegExp(event.id));

  const attendees = await call('attendees', { event: event.id });
  assert.ok(attendees.replies.length >= 1);
  const attendeesText = JSON.stringify(attendees.replies[0].embeds?.map((e) => e.toJSON?.() ?? e) ?? attendees.replies[0]);
  assert.match(attendeesText, /<@A>/);
  assert.match(attendeesText, /<@B>/);

  const list = await call('list');
  assert.match(JSON.stringify(list.replies[0].embeds.map((e) => e.toJSON?.() ?? e)), /Open events \(1\/25\)/);

  const missing = await call('info', { event: 'nope' });
  assert.match(embedText(missing.replies[0]), /Event Not Found/);

  const cancel = await call('cancel', { event: event.id });
  assert.match(embedText(cancel.replies[0]), /Event Cancelled/);
  assert.equal(events.get(event.id).status, 'CANCELLED');
  const again = await call('cancel', { event: event.id });
  assert.match(embedText(again.replies[0]), /Event Closed/);
  const none = await call('list');
  assert.match(embedText(none.replies[0]), /No Open Events/);
});

test('/event edit: opens the form with the five fields (no deferral), refuses a closed or unknown event', async () => {
  reset();
  await configure('G');
  const guild = fakeGuild();
  const event = await seedEvent({ title: 'Movie night' });
  const call = async (values) => {
    const i = fakeInteraction({ guildId: 'G', guild, user: { id: 'MOD' }, member: fakeMember('MOD', { mod: true }), options: optionsOf({ __sub: 'edit', ...values }) });
    await cmd.execute(i, fakeClient(guild));
    return i;
  };
  const ok = await call({ event: event.id, capacity: 8 });
  assert.equal(ok.deferred, false, 'showModal is the first response');
  assert.equal(ok.modals.length, 1);
  const json = ok.modals[0].toJSON();
  assert.equal(json.custom_id, `event-modal:${event.id}:8:-`);
  assert.equal(json.components.length, 5);

  const unknown = await call({ event: 'nope' });
  assert.equal(unknown.modals.length, 0);
  assert.match(embedText(unknown.replies[0]), /Event Not Found/);
  assert.equal(unknown.replies[0].ephemeral, true);
  events.get(event.id).status = 'ENDED';
  const closed = await call({ event: event.id });
  assert.match(embedText(closed.replies[0]), /Event Closed/);
});
