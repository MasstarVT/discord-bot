// Reference data for feat-events, produced by the JS bot's own eventService, rolePersistService and utils/calendar.js
// (no servers). Writes rust/fixtures/golden/feat-events/golden.json. Run from the repository root:
//   node rust/fixtures/gen/feat-events-golden.mjs
// Embeds, components and modals are their toJSON(); dates are ISO strings.
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
const quiet = () => {};
console.error = console.warn = console.log = console.debug = console.info = quiet;

const { PermissionsBitField } = await imp('node_modules', 'discord.js', 'src', 'index.js');
const svc = await imp('src', 'services', 'eventService.js');
const rp = await imp('src', 'services', 'rolePersistService.js');
const cal = await imp('src', 'utils', 'calendar.js');
const { buildCustomId } = await imp('src', 'config', 'customIds.js');
const { default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');

const json = (x) => JSON.parse(JSON.stringify(x));
const iso = (d) => (d instanceof Date ? (Number.isNaN(d.getTime()) ? null : d.toISOString()) : d ?? null);

const NOW = Date.parse('2026-10-20T12:00:00.000Z');
const HOUR = 3_600_000;
const DAY = 24 * HOUR;
const G = '900000000000000777';
const SETTINGS = { embedColor: 0x123456 };

/** A full GuildEvent row as Prisma returns it (every column), with `over` applied. */
function mkEvent(over = {}) {
  return {
    id: 'cevent000000000000000001', guildId: G, title: 'Movie night', description: 'Bring snacks', location: 'Voice 1',
    startsAt: new Date(NOW + DAY), endsAt: new Date(NOW + DAY + 2 * HOUR), hostId: '600000000000000001',
    channelId: '800000000000000001', messageId: '700000000000000001', roleId: '810000000000000001', capacity: 30,
    reminderMinutes: 15, reminderSentAt: null, reminderMessageIds: [], discordEventId: null, textChannelId: null,
    voiceChannelId: null, accessRoleId: null, status: 'SCHEDULED',
    createdAt: new Date(NOW - DAY), updatedAt: new Date('2026-10-19T08:30:15.250Z'), ...over,
  };
}
const ev = (e) => json(e);
const ids = (n, base = 600000000000000100n) => Array.from({ length: n }, (_, i) => String(base + BigInt(i)));
const tally = (going, maybe, declined, goingIds) => ({ going, maybe, declined, goingIds });

// ── Cards ─────────────────────────────────────────────────────────────────────
const cardCases = [];
const addCard = (name, e, t, opts = {}) => {
  const names = opts.names ? new Map(Object.entries(opts.names)) : null;
  cardCases.push({
    name, event: ev(e), tally: t, settings: opts.settings ?? null, names: opts.names ?? null,
    embed: svc.buildEventEmbed(e, t, { settings: opts.settings ?? null, now: NOW, names }).toJSON(),
    components: svc.buildEventComponents(e, t).map((r) => r.toJSON()),
  });
};
addCard('open', mkEvent(), tally(20, 3, 1, ids(15)));
addCard('empty', mkEvent({ location: null, roleId: null, capacity: null, endsAt: null, description: null }), svc.emptyTally());
addCard('cancelled', mkEvent({ status: 'CANCELLED' }), tally(2, 0, 0, ids(2)));
addCard('ended', mkEvent({ status: 'ENDED', textChannelId: '800000000000000002' }), tally(2, 1, 0, ids(2)));
addCard('started', mkEvent({ startsAt: new Date(NOW - HOUR), textChannelId: '800000000000000002', voiceChannelId: '800000000000000003' }), tally(1, 0, 0, ids(1)));
addCard('full', mkEvent({ capacity: 2 }), tally(2, 0, 0, ids(2)));
addCard('names', mkEvent({ capacity: null }), tally(4, 0, 0, ['600000000000000101', '600000000000000102', '600000000000000103']),
  { names: { '600000000000000101': 'Rev', '600000000000000103': 'Frosty_*Fan*' }, settings: SETTINGS });
addCard('long', mkEvent({ title: `🎉 ${'Party '.repeat(60)}`, description: 'd'.repeat(1500), location: 'L'.repeat(150) }), tally(0, 0, 0, []),
  { settings: SETTINGS });
addCard('longNames', mkEvent(), tally(15, 0, 0, ids(15)), { names: Object.fromEntries(ids(15).map((id) => [id, `${'Name'.repeat(10)}${id.slice(-2)}`])) });
addCard('active', mkEvent({ status: 'ACTIVE', textChannelId: '800000000000000002' }), tally(0, 2, 0, []));

// ── Reminders, lines, replies ──────────────────────────────────────────────────
const reminders = [
  { name: 'none', event: ev(mkEvent()), userIds: [], out: svc.reminderMessages(mkEvent(), [], NOW) },
  { name: 'two', event: ev(mkEvent()), userIds: ids(2), out: svc.reminderMessages(mkEvent(), ids(2), NOW) },
  { name: 'voiceStarted', event: ev(mkEvent({ startsAt: new Date(NOW - 1000), voiceChannelId: '800000000000000003', messageId: null })), userIds: ids(3),
    out: svc.reminderMessages(mkEvent({ startsAt: new Date(NOW - 1000), voiceChannelId: '800000000000000003', messageId: null }), ids(3), NOW) },
  { name: 'many', event: ev(mkEvent()), userIds: ids(500, 100000000000000000n), out: svc.reminderMessages(mkEvent(), ids(500, 100000000000000000n), NOW) },
  { name: 'longTitle', event: ev(mkEvent({ title: 'x'.repeat(150) })), userIds: ids(90), out: svc.reminderMessages(mkEvent({ title: 'x'.repeat(150) }), ids(90), NOW) },
];
const lines = {
  list: [
    { event: ev(mkEvent()), going: 4, out: svc.listLine(mkEvent(), 4) },
    { event: ev(mkEvent({ capacity: null, messageId: null, title: 'y'.repeat(80) })), going: 0, out: svc.listLine(mkEvent({ capacity: null, messageId: null, title: 'y'.repeat(80) }), 0) },
  ],
  attendees: svc.attendeeLines([{ userId: 'A', status: 'MAYBE' }, { userId: 'B', status: 'GOING' }, { userId: 'C', status: 'DECLINED' }, { userId: 'D', status: 'GOING' }]),
  replies: [],
};
const replyEvent = mkEvent({ textChannelId: '800000000000000002' });
for (const [out, status] of [
  [{ result: 'gone' }, 'GOING'], [{ result: 'closed' }, 'MAYBE'], [{ result: 'full' }, 'GOING'],
  [{ result: 'same' }, 'MAYBE'], [{ result: 'same' }, 'DECLINED'], [{ result: 'set' }, 'GOING'], [{ result: 'set', note: 'no role' }, 'GOING'],
  [{ result: 'set' }, 'DECLINED'], [{ result: 'set', note: 'a\n⚠️ b' }, 'MAYBE'],
]) {
  lines.replies.push({ out, status, withChat: true, text: svc.rsvpReplyText(out, replyEvent, status) });
  lines.replies.push({ out, status, withChat: false, text: svc.rsvpReplyText(out, mkEvent(), status) });
}
lines.replies.push({ out: { result: 'gone' }, status: 'GOING', noEvent: true, text: svc.rsvpReplyText({ result: 'gone' }, null, 'GOING') });

// ── Calendar ──────────────────────────────────────────────────────────────────
const guildNames = {
  members: { '600000000000000001': 'Hosty' }, users: { '600000000000000002': 'leftuser' },
  roles: { '810000000000000001': 'Gamers' }, channels: { '800000000000000001': 'general' },
};
const fakeGuild = {
  members: { cache: new Map(Object.entries(guildNames.members).map(([id, n]) => [id, { displayName: n }])) },
  client: { users: { cache: new Map(Object.entries(guildNames.users).map(([id, n]) => [id, { username: n }])) } },
  roles: { cache: new Map(Object.entries(guildNames.roles).map(([id, n]) => [id, { name: n }])) },
  channels: { cache: new Map(Object.entries(guildNames.channels).map(([id, n]) => [id, { name: n }])) },
};
const plainTexts = [
  'Hi <@600000000000000001> and <@!600000000000000002> and <@600000000000000009>',
  '<@&810000000000000001> <@&810000000000000009> in <#800000000000000001> / <#800000000000000009>',
  'Run </event create:123456789012345678> and <a:party:123456789012345678> <:x:12345678901234567>',
  'At <t:1792497600:F> or <t:1792497600> or <t:-5:R> or <t:99999999999999:R>',
  '<@123> short ids stay, <t:abc> too', '', null,
];
const calendar = {
  guildNames,
  plain: plainTexts.map((t) => ({ text: t, withGuild: svc.plainDiscordText(t, fakeGuild), without: svc.plainDiscordText(t, null) })),
  entries: [],
  payloads: [],
};
for (const e of [
  mkEvent({ description: 'Hi <@600000000000000001>, see <#800000000000000001>' }),
  mkEvent({ endsAt: null, location: null, messageId: null, description: null, updatedAt: new Date('2019-05-01T00:00:00Z') }),
  mkEvent({ title: '🎉'.repeat(80), description: '🍿 popcorn '.repeat(120), location: '🏠'.repeat(60) }),
  mkEvent({ title: 'Snacks, drinks; fun\\ and more', description: 'Line one\nLine two, with; stuff' }),
]) {
  const entry = svc.calendarEntry(e, fakeGuild);
  calendar.entries.push({ event: ev(e), entry: { ...entry, start: iso(entry.start), end: iso(entry.end) } });
  const p = svc.buildCalendarPayload(e, { guild: fakeGuild, now: NOW });
  calendar.payloads.push({
    event: ev(e), content: p.content, components: p.components.map((r) => r.toJSON()), fileName: p.files[0].name,
    ics: p.files[0].attachment.toString('utf8'), allowedMentions: p.allowedMentions,
  });
}
calendar.customContent = svc.buildCalendarPayload(mkEvent(), { now: NOW, content: 'hello' }).content;
const fitCases = [
  ['hello', 5], ['a😀b😀c😀d😀', 30], ['abcdef', 3], ['', 10], ['word '.repeat(50), 40], ['é'.repeat(20), 25], ['x  y   ', 6], ['🎉 party', 12],
];
calendar.fit = fitCases.map(([t, b]) => ({ text: t, budget: b, out: cal.fitEncoded(t, b) }));
const calEntry = (over = {}) => ({
  uid: 'ev1@events.discord-bot', title: 'Movie night', description: 'Bring snacks', location: 'Main hall',
  start: new Date('2026-11-05T19:00:00.000Z'), end: new Date('2026-11-05T20:30:00.000Z'),
  url: 'https://discord.com/channels/111111111111111111/222222222222222222/333333333333333333', sequence: 7, ...over,
});
calendar.links = [];
for (const [name, over, max] of [
  ['plain', {}, undefined], ['empty', { description: '', location: '', url: '' }, undefined],
  ['heavy', { title: '🎉'.repeat(50), description: '🍿 popcorn '.repeat(100), location: '🏠'.repeat(50) }, undefined],
  ['long', { description: 'word '.repeat(400) }, undefined], ['tiny', { description: 'x'.repeat(1000) }, 200],
  ['urlOnly', { description: '', url: 'https://example.com/a b?c=d&e' }, 120],
]) {
  const e = calEntry(over);
  calendar.links.push({
    name, entry: { ...e, start: iso(e.start), end: iso(e.end) }, max: max ?? null,
    google: cal.googleCalendarUrl(e, max), outlook: cal.outlookCalendarUrl(e, max),
    ics: cal.buildIcs(e, { now: Date.UTC(2026, 9, 22, 12, 0, 0) }),
  });
}
calendar.escape = ['a\\b;c,d\r\ne\nf', 'plain', '\r\r\n\n'].map((t) => ({ text: t, out: cal.icsEscape(t) }));
calendar.fold = [`SUMMARY:${'🎉 party '.repeat(40)}`, 'SHORT:x', `X:${'a'.repeat(200)}`, `Y:${'é'.repeat(80)}`].map((l) => ({ line: l, out: cal.icsFold(l) }));

// ── Edit panel and forms ───────────────────────────────────────────────────────
const panels = [];
for (const e of [
  mkEvent(), mkEvent({ messageId: null, location: null, description: null, capacity: null, reminderMinutes: 0, endsAt: null }),
  mkEvent({ reminderSentAt: new Date(NOW - HOUR), description: 'z'.repeat(400), endsAt: new Date(NOW + DAY + 90 * 60_000) }),
]) panels.push({ event: ev(e), settings: SETTINGS, out: json(svc.buildEditPanel(e, { settings: SETTINGS })) });
const modals = [];
for (const [e, zone] of [
  [mkEvent(), 'UTC'], [mkEvent(), 'America/Chicago'], [mkEvent({ endsAt: null, capacity: null, reminderMinutes: 0, description: null, location: null }), 'Asia/Kolkata'],
  [mkEvent({ startsAt: new Date('2026-03-08T07:30:00Z'), endsAt: new Date('2026-03-08T09:00:00Z') }), 'America/New_York'],
  [mkEvent({ title: 't'.repeat(100), description: 'd'.repeat(1000), location: 'l'.repeat(100), capacity: 10000, reminderMinutes: 1440 }), 'Europe/London'],
]) {
  for (const form of svc.EDIT_FORMS) modals.push({ event: ev(e), form, zone, out: svc.buildEditModal(e, form, zone).toJSON() });
}

// ── Times (DST edges) ──────────────────────────────────────────────────────────
const ZONES = ['UTC', 'America/New_York', 'America/Chicago', 'Europe/London', 'Australia/Lord_Howe', 'Asia/Kolkata', 'Pacific/Chatham', 'Etc/GMT-2', 'Australia/Sydney'];
const INSTANTS = [
  '2026-11-05T01:30:00Z', '2026-03-08T06:59:00Z', '2026-03-08T07:00:00Z', '2026-11-01T05:30:00Z', '2026-11-01T06:30:00Z',
  '2026-03-29T00:59:00Z', '2026-03-29T01:00:00Z', '2026-10-25T00:30:00Z', '2026-10-25T01:30:00Z', '2026-04-04T14:45:00Z', '2026-10-03T16:00:00Z',
];
const formatStart = [];
for (const zone of ZONES) {
  for (const at of INSTANTS) {
    const d = new Date(at);
    formatStart.push({ zone, at, text: svc.formatStartForEdit(d, zone), fields: svc.formatStartFields(d, zone) });
  }
}
const resolvedUtc = { zone: 'UTC', source: 'default', roleId: null, roleCount: 0 };
const outWhen = (r) => (r.ok ? { ok: true, values: { ...r.values, startsAt: iso(r.values.startsAt), endsAt: iso(r.values.endsAt) } } : r);
const outFields = (r) => (r.ok ? { ok: true, values: { ...r.values, startsAt: iso(r.values.startsAt), endsAt: iso(r.values.endsAt) } } : r);
const whenNow = Date.parse('2026-02-20T12:00:00Z');
const whenCases = [];
for (const zone of ['UTC', 'America/New_York', 'Europe/London', 'Australia/Lord_Howe', 'Asia/Kolkata', 'Pacific/Chatham']) {
  for (const start of [
    '2026-03-08 02:30', '2026-03-08 01:59', '2026-03-08 03:00', '2026-11-01 01:30', '2026-11-01 02:30', '2026-03-29 01:30',
    '2026-10-25 01:30', '2026-04-05 01:45', '2026-10-04 02:15', '7pm CST', 'in 3d', 'fri 7pm', 'tomorrow 18:30', 'oct 30 7:30pm',
    '2026-10-30 19:00 Europe/London', 'mar 8 2:30am',
  ]) {
    whenCases.push({ zone, start, now: whenNow, out: outWhen(svc.parseEventWhen({ start, duration: '90m' }, { resolved: { ...resolvedUtc, zone }, now: whenNow })) });
  }
}
// Error texts (no DST zones: the zone label is printed for the current date).
const fieldNow = Date.parse('2026-10-20T12:00:00Z');
const resolvedKolkata = { zone: 'Asia/Kolkata', source: 'server', roleId: null, roleCount: 0 };
const resolvedRole = { zone: 'UTC', source: 'role', roleId: '810000000000000005', roleCount: 1 };
const fieldCases = [];
const addFields = (name, fields, ctx) => {
  const c = { resolved: resolvedUtc, now: fieldNow, ...ctx };
  fieldCases.push({
    name, fields, resolved: c.resolved, zone: c.zone ?? null, current: c.current ? { startsAt: iso(c.current.startsAt), discordEventId: c.current.discordEventId ?? null } : null,
    examples: c.examples ?? null, now: fieldNow, out: outFields(svc.parseEventFields(fields, c)),
  });
};
addFields('ok', { title: ' Movie ', start: '2026-10-25 19:00', duration: '2h', description: ' hi ', location: ' Voice 1 ' }, {});
addFields('kolkata', { title: 'T', start: '2026-10-25 19:00' }, { resolved: resolvedKolkata });
addFields('named', { title: 'T', start: '2026-10-25 19:00 Europe/London' }, {});
addFields('past', { title: 'T', start: '2026-10-19 19:00' }, {});
addFields('far', { title: 'T', start: '2028-01-01 10:00' }, {});
addFields('unreadable', { title: 'T', start: 'blah `blah` \\x' }, {});
addFields('unreadableRole', { title: 'T', start: 'blah' }, { resolved: resolvedRole });
addFields('unreadableKolkata', { title: 'T', start: '' }, { resolved: resolvedKolkata });
addFields('unreadableOther', { title: 'T', start: 'zzz' }, { resolved: resolvedKolkata, zone: 'UTC', examples: 'EX' });
addFields('badDuration', { title: 'T', start: '2026-10-25 19:00', duration: '10d1y' }, {});
addFields('longDuration', { title: 'T', start: '2026-10-25 19:00', duration: '30d' }, {});
addFields('shortDuration', { title: 'T', start: '2026-10-25 19:00', duration: '1m' }, {});
addFields('noTitle', { title: '  ', start: '2026-10-25 19:00' }, {});
addFields('longTitle', { title: 'x'.repeat(101), start: '2026-10-25 19:00' }, {});
addFields('longDescription', { title: 'T', start: '2026-10-25 19:00', description: 'x'.repeat(1001) }, {});
addFields('longLocation', { title: 'T', start: '2026-10-25 19:00', location: 'x'.repeat(101) }, {});
addFields('keptPast', { title: 'T', start: '2026-10-19 19:00', location: 'Here' }, { current: { startsAt: new Date('2026-10-19T19:00:42Z'), discordEventId: 'DE1' } });
addFields('movedPast', { title: 'T', start: '2026-10-19 20:00', location: 'Here' }, { current: { startsAt: new Date('2026-10-19T19:00:00Z'), discordEventId: 'DE1' } });
addFields('needsLocation', { title: 'T', start: '2026-10-25 19:00', location: '' }, { current: { startsAt: new Date('2026-10-19T19:00:00Z'), discordEventId: 'DE1' } });
addFields('noLocationOk', { title: 'T', start: '2026-10-25 19:00', location: '' }, { current: { startsAt: new Date('2026-10-19T19:00:00Z'), discordEventId: null } });
addFields('seconds', { title: 'T', start: 'in 3d' }, {});
const limits = [['', ''], ['10', '30'], ['0', '0'], ['10001', '5'], ['abc', ''], ['5', '1441'], ['5', '-1'], [' 7 ', ' 60 '], ['00010', '0015'], ['1.5', '']]
  .map(([capacity, reminder]) => ({ capacity, reminder, out: svc.parseEventLimits({ capacity, reminder }) }));
const durations = ['', null, ' 2h ', '90m', '1m', '15d', '14d', 'soonish', '5m', '4m59s', '1h30m', '1w'].map((raw) => ({ raw, out: svc.parseDurationInput(raw) }));

// ── Small rules ────────────────────────────────────────────────────────────────
const rules = {
  constants: svc.EVENT,
  durationErrors: svc.DURATION_ERRORS,
  rsvpLabels: svc.RSVP_LABELS,
  rsvpEmoji: svc.RSVP_EMOJI,
  notYours: svc.NOT_YOURS_TEXT,
  editForms: svc.EDIT_FORMS,
  keys: ['cabc', 'E1'].map((id) => ({ id, remind: svc.remindJobKey(id), end: svc.endJobKey(id), voice: svc.voiceJobKey(id), cleanup: svc.cleanupJobKey(id) })),
  jobTypes: [svc.REMIND_JOB_TYPE, svc.END_JOB_TYPE, svc.VOICE_JOB_TYPE, svc.CLEANUP_JOB_TYPE],
  customIds: [
    buildCustomId('event', 'cabc', 'going'), buildCustomId('event-edit', 'cabc', 'when'), buildCustomId('event-modal', 'cabc', 'limits'),
  ],
  voiceLeadText: [0, 1, 2, 30, 1440].map((m) => ({ m, out: svc.voiceLeadText(m) })),
  voiceLead: [null, {}, { eventVoiceLeadMinutes: 0 }, { eventVoiceLeadMinutes: 45 }, { eventVoiceLeadMinutes: 2.5 }].map((s) => ({ settings: s, out: svc.voiceLead(s) })),
  voiceAt: [[NOW + DAY, 30], [NOW + 10 * 60_000, 30], [NOW - HOUR, 0], [NOW + 30 * 60_000, 30]].map(([s, lead]) => ({ startsAt: iso(new Date(s)), lead, out: iso(svc.voiceAt({ startsAt: new Date(s) }, lead, NOW)) })),
  ends: [mkEvent(), mkEvent({ endsAt: null })].map((e) => ({ event: ev(e), end: iso(svc.eventEnd(e)), shown: iso(svc.shownEnd(e)), remind: iso(svc.remindAt(e)) })),
  remindAt: [0, -5, 15, 1440].map((m) => ({ m, out: iso(svc.remindAt({ startsAt: new Date(NOW), reminderMinutes: m })) })),
  startProblem: [NOW - 1, NOW, NOW + 1000, NOW + 365 * DAY, NOW + 366 * DAY].map((ms) => ({ ms, out: svc.startProblem(new Date(ms), NOW) })),
  textChannelName: ['Movie night', 'Ünïcödé Café!!', '   ', '🎉🎉', 'a'.repeat(120), 'Big -- Party -- 2026'].map((t) => ({ title: t, out: svc.textChannelName(t) })),
  calendarFileName: ['Movie night', '🎉', 'x'.repeat(80)].map((t) => ({ title: t, out: svc.buildCalendarPayload(mkEvent({ title: t }), { now: NOW }).files[0].name })),
  jumpUrl: [mkEvent(), mkEvent({ messageId: null })].map((e) => ({ event: ev(e), out: svc.jumpUrl(e) })),
  tallyFromGroups: svc.tallyFromGroups([{ status: 'GOING', _count: { _all: 20 } }, { status: 'MAYBE', _count: { _all: 3 } }, { status: 'DECLINED', _count: 2 }], ['a']),
  discordEventOptions: [
    mkEvent({ title: 'T'.repeat(150), description: 'd'.repeat(1500), location: 'L'.repeat(150), endsAt: null }),
    mkEvent({ description: '', endsAt: new Date(NOW + DAY + 5 * HOUR) }),
  ].map((e) => {
    const o = svc.discordEventOptions(e);
    return { event: ev(e), out: { ...o, scheduledStartTime: iso(o.scheduledStartTime), scheduledEndTime: iso(o.scheduledEndTime) } };
  }),
  overwrites: (() => {
    const guild = { id: G, client: { user: { id: '100000000000000002' } } };
    const res = (o) => ({ id: o.id, type: o.type, allow: o.allow ? PermissionsBitField.resolve(o.allow).toString() : null, deny: o.deny ? PermissionsBitField.resolve(o.deny).toString() : null });
    return {
      text: svc.eventOverwrites(guild, '810000000000000077', ['ViewChannel', 'SendMessages', 'ReadMessageHistory']).map(res),
      voice: svc.eventOverwrites(guild, '810000000000000077', ['ViewChannel', 'Connect', 'Speak']).map(res),
    };
  })(),
  rawSql: svc.RAW_SQL.map((r) => ({ name: r.name, sql: r.build().sql, values: r.build().values })),
};

// ── Role persistence rules ─────────────────────────────────────────────────────
const rolePersist = {
  constants: rp.ROLEPERSIST,
  retention: [undefined, null, 'x', 0, 1, 30.7, 365, 400, -3, '45', '', true].map((d) => ({ ...(d === undefined ? {} : { days: d }), out: rp.retentionDays({ rolePersistDays: d }) })),
  merge: [
    [null, { roleIds: ['a', 'b'], nickname: null }],
    [{ roleIds: ['a', 'c'], nickname: 'Old' }, { roleIds: ['b', 'a'], nickname: null }],
    [{ roleIds: ['a'], nickname: 'Old' }, { roleIds: ['b'], nickname: 'New' }],
    [{ roleIds: [], nickname: null }, { roleIds: [], nickname: null }],
  ].map(([old, snap]) => ({ old, snap, out: rp.mergeSnapshot(old, snap) })),
  skipped: [
    {}, { rolePersistIgnoredRoleIds: ['1', '2'], voiceRoleAnyRoleIds: ['3', '1'], voiceRoleLinks: [{ roleId: '4' }, { roleId: 5 }, null, { channelId: 'x' }] },
    { voiceRoleLinks: 'nope' },
  ].map((s) => ({ settings: s, out: [...rp.skippedRoleIds(s)] })),
  owned: [
    {}, { birthdayRoleId: 'B', muteRoleId: 'M', modEnabled: true }, { birthdayRoleId: 'B', muteRoleId: 'M', modEnabled: false },
  ].map((s) => ({ settings: s, out: [...rp.moduleRoleReasons(s)] })),
  expiresAt: [[1, '2026-01-01T00:00:00.000Z'], [90, '2026-01-01T00:00:00.000Z']].map(([d, left]) => ({ days: d, leftAt: left, out: rp.expiresAt({ leftAt: new Date(left) }, { rolePersistDays: d }) })),
};

const out = {
  now: new Date(NOW).toISOString(), settings: SETTINGS, cards: cardCases, reminders, lines, calendar, panels, modals,
  formatStart, whenCases, fieldCases, limits, durations, rules, rolePersist,
};
const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-events');
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, 'golden.json'), `${JSON.stringify(json(out), null, 1)}\n`);
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.stdout.write('\nRESULT {"ok":true}\n');
process.exit(0);
