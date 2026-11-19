// Generates the reference results of the dash-collections crate from the JS dashboard (src/dashboard/collections/*.js,
// not the *.store.js files, and src/dashboard/catalog.js):
//   rust/fixtures/golden/dash-collections/validate.json  validateRecordInput over a wide table of create, update and
//                                                       action inputs per collection (valid and invalid)
//   rust/fixtures/golden/dash-collections/helpers.json   every exported helper over a table of inputs: { fn, args, out }
//   rust/fixtures/golden/dash-collections/public.json    every collection as the front end sees it (publicCollection)
// Date.now() and `new Date()` are frozen at NOW so the time rules are deterministic.
// Pure: no database or Redis. Usage (repository root): node rust/fixtures/gen/dash-collections-golden.mjs
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';

// Zone-less date strings are local time in V8; the Rust port reads `TZ` (UTC when unset), so the goldens use UTC.
process.env.TZ = 'UTC';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const NOW = Date.parse('2026-06-01T12:00:00.000Z');
const RealDate = Date;
class FrozenDate extends RealDate {
  constructor(...args) {
    if (args.length === 0) super(NOW);
    else super(...args);
  }

  static now() {
    return NOW;
  }
}
globalThis.Date = FrozenDate;

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (p) => import(pathToFileURL(join(ROOT, p)).href);
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'dash-collections');
mkdirSync(OUT, { recursive: true });

const index = await imp('src/dashboard/collections/index.js');
const errors = await imp('src/dashboard/collections/errors.js');
const access = await imp('src/dashboard/collections/channelAccess.js');
const embeds = await imp('src/dashboard/collections/embeds.js');
const tags = await imp('src/dashboard/collections/tags.js');
const triggers = await imp('src/dashboard/collections/triggers.js');
const reminders = await imp('src/dashboard/collections/reminders.js');
const feeds = await imp('src/dashboard/collections/feeds.js');
const levelRoles = await imp('src/dashboard/collections/levelRoles.js');
const starboards = await imp('src/dashboard/collections/starboards.js');
const translateChannels = await imp('src/dashboard/collections/translateChannels.js');
const translateStats = await imp('src/dashboard/collections/translateStats.js');
const statChannels = await imp('src/dashboard/collections/statChannels.js');
const cases = await imp('src/dashboard/collections/cases.js');
const levels = await imp('src/dashboard/collections/levels.js');
const balances = await imp('src/dashboard/collections/balances.js');
const giveaways = await imp('src/dashboard/collections/giveaways.js');
const polls = await imp('src/dashboard/collections/polls.js');
const rr = await imp('src/dashboard/collections/reactionRoles.js');
const tp = await imp('src/dashboard/collections/ticketPanels.js');
const auto = await imp('src/dashboard/collections/automations.js');
const { renderPlaceholders } = await imp('src/utils/placeholderParser.js');
const { renderTemplate } = await imp('src/utils/customEmbed.js');

const json = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
const write = (name, data) => {
  const file = join(OUT, name);
  writeFileSync(file, `${JSON.stringify(data, null, 1)}\n`);
  process.stdout.write(`${file.slice(ROOT.length + 1)}\n`);
};

// ── Rows: Prisma JSON (dates as ISO strings) for Rust, Date objects for the JS ─────────────────────────────────
const dmmf = JSON.parse(readFileSync(join(ROOT, 'rust', 'fixtures', 'golden', 'prisma-dmmf.json'), 'utf8'));
const baseline = JSON.parse(readFileSync(join(ROOT, 'rust', 'fixtures', 'golden', 'mockdb', 'baseline.json'), 'utf8'));
const DATE_FIELDS = new Set(['createdAt', 'updatedAt', 'runAt', 'lastSentAt', 'endsAt', 'endedAt', 'expiresAt', 'lastXpAt',
  'lastDaily', 'lastWork', 'lastCheckedAt', 'lastPostedAt', 'firstAt', 'lastAt', 'messageMissingAt', 'deadline', 'lastRenamedAt']);
for (const m of dmmf.models) for (const f of m.fields) if (f.type === 'DateTime') DATE_FIELDS.add(f.name);
/** A JSON row as the JS functions get it from Prisma: top-level date strings as Date objects. */
const jsRow = (row) => {
  if (!row || typeof row !== 'object' || Array.isArray(row)) return row;
  const out = {};
  for (const [k, v] of Object.entries(row)) {
    if (DATE_FIELDS.has(k) && typeof v === 'string') out[k] = new Date(v);
    else if (Array.isArray(v) && v.every((x) => x && typeof x === 'object' && !Array.isArray(x))) out[k] = v.map(jsRow);
    else out[k] = v;
  }
  return out;
};
const rowsOf = (table) => {
  const b = baseline[table];
  return b ? [b.min, b.full].filter(Boolean) : [];
};

// ── helpers.json ────────────────────────────────────────────────────────────────────────────────────────────────
const cases_ = [];
/** Records `fn(...jsArgs)` with the JSON `args` the Rust side reads. */
function call(fn, args, run) {
  let out;
  try {
    out = { ok: json(run()) };
  } catch (err) {
    out = { throws: { name: err?.name ?? 'Error', code: err?.code ?? null, message: String(err?.message ?? err), path: err?.path ?? null } };
  }
  cases_.push({ fn, args: json(args), ...out });
}
async function callAsync(fn, args, run) {
  let out;
  try {
    out = { ok: json(await run()) };
  } catch (err) {
    out = { throws: { name: err?.name ?? 'Error', code: err?.code ?? null, message: String(err?.message ?? err), path: err?.path ?? null } };
  }
  cases_.push({ fn, args: json(args), ...out });
}

const G = '900000000000000002';
const C1 = '900000000000000101';
const C2 = '900000000000000102';
const M1 = '900000000000000201';
const U1 = '800000000000000001';
const U2 = '800000000000000002';
const R1 = '900000000000000301';
const R2 = '900000000000000302';
const R3 = '900000000000000303';
const E1 = '900000000000000401';
const ISO = '2026-05-30T10:20:30.400Z';
const ISO2 = '2026-07-01T00:00:00.000Z';

// errors.js
for (const r of [null, undefined, { status: 'done', message: 'ok', data: { a: 1 } }, { status: 'pending', message: 'wait', data: null },
  { status: 'failed', message: 'no' }, { status: 'offline', message: 'gone' }, { status: 'done', message: 'Done.' }]) {
  call('taskOutcome', [r], () => errors.taskOutcome(r));
}

// channelAccess.js
for (const p of ['0', '8', '9', ' 8 ', '', '0x8', '0b1000', '-8', '+8', 'abc', '8.0', '1e3', '2147483656', '18446744073709551615',
  '340282366920938463463374607431768211464', String(1n << 3n | 1n << 5n)]) {
  call('isChannelExempt', [{ owner: false, permissions: p }], () => access.isChannelExempt({ owner: false, permissions: p }));
}
call('isChannelExempt', [{ owner: true, permissions: '0' }], () => access.isChannelExempt({ owner: true, permissions: '0' }));
call('isChannelExempt', [null], () => access.isChannelExempt(null));
const accessChecks = [
  [{ channelId: C1, path: 'channelId' }],
  [{ channelId: null, path: 'a' }, { channelId: C1, needs: ['ViewChannel'], path: 'b' }, { channelId: C2, optional: true, path: 'c' }],
  [],
];
const taskAnswers = [
  { status: 'done', message: 'ok', data: { problems: [] } },
  { status: 'done', message: 'ok', data: { problems: [{ index: 1, text: 'You need View Channel in #staff.' }] } },
  { status: 'done', message: 'ok', data: { problems: [{ index: 0 }] } },
  { status: 'done', message: 'ok', data: { problems: [{ index: 5, text: 'x'.repeat(400) }] } },
  { status: 'done', message: 'ok', data: null },
  { status: 'pending', message: 'wait', data: null },
  { status: 'failed', message: 'You are not a member of this server.' },
  { status: 'offline', message: 'The bot did not pick this up in time.' },
];
for (const checks of accessChecks) {
  for (const answer of taskAnswers) {
    for (const member of [{ owner: false, permissions: '32' }, { owner: true, permissions: '0' }, { owner: false, permissions: '8' }]) {
      for (const lead of ['', 'The reply channel: ']) {
        const seen = [];
        await callAsync('requireChannelAccess', [checks, answer, member, lead, true], async () => {
          await access.requireChannelAccess({ member, runTask: async (kind, payload) => { seen.push([kind, payload]); return answer; } }, checks, { lead });
          return { seen };
        });
      }
    }
  }
  await callAsync('requireChannelAccess', [checks, null, { owner: false, permissions: '0' }, '', false], async () => {
    await access.requireChannelAccess({ member: { owner: false, permissions: '0' } }, checks);
    return { seen: [] };
  });
}

// embeds.js
for (const args of [[G, C1, M1], [G, C1, null], ['1', C1, M1], [G, '12345678901234567890', M1], [G, C1, '0']]) {
  call('messageUrl', args, () => embeds.messageUrl(...args));
}
for (const row of [...rowsOf('SavedEmbed'), { id: 'e1', guildId: G, name: 'x', template: null, channelId: C1, messageId: null, updatedAt: null }]) {
  call('embedItem', [row], () => embeds.embedItem(jsRow(row)));
}
for (const g of [{ id: G, icon: 'a'.repeat(32) }, { id: G, icon: `a_${'b'.repeat(32)}` }, { id: G, icon: null }, { id: '1', icon: 'a'.repeat(32) }, { id: G, icon: 'xyz' }]) {
  call('guildIconUrl', [g], () => embeds.guildIconUrl(g));
}
for (const u of [{ id: U1, avatar: 'c'.repeat(32) }, { id: U1, avatar: `a_${'d'.repeat(32)}` }, { id: U1 }, { id: 'nope' }, { id: '99999999999999999999' }]) {
  call('userAvatarUrl', [u], () => embeds.userAvatarUrl(u));
}
const RESOURCES = {
  guild: { id: G, name: 'Test Guild', icon: 'e'.repeat(32), memberCount: 1234, ownerId: U2, premiumTier: 2, premiumSubscriptionCount: 7, verificationLevel: 3 },
  channels: [{ id: C1, name: 'general', type: 0, kind: 'text', parentId: null, position: 0, nsfw: false },
    { id: C2, name: 'news', type: 5, kind: 'announcement', parentId: null, position: 1, nsfw: false }],
  roles: [
    { id: R1, name: 'Mentionable', color: null, position: 5, managed: false, everyone: false, mentionable: true, assignable: true, dangerous: false },
    { id: R2, name: 'Quiet', color: '#FF0000', position: 4, managed: false, everyone: false, mentionable: false, assignable: true, dangerous: true },
    { id: R3, name: 'Bot role', color: null, position: 9, managed: true, everyone: false, mentionable: false, assignable: false, dangerous: false },
    { id: G, name: '@everyone', color: null, position: 0, managed: false, everyone: true, mentionable: false, assignable: false, dangerous: false },
    { id: '900000000000000304', name: 'High', color: null, position: 20, managed: false, everyone: false, mentionable: false, assignable: false, dangerous: false },
    { id: '900000000000000305', name: '', color: null, position: 2, managed: false, everyone: false, assignable: true },
    { id: '900000000000000306', name: 'Perms', color: null, position: 3, managed: false, everyone: false, assignable: true, permissions: String(1n << 2n | 1n << 13n) },
    { id: '900000000000000307', name: 'Clean', color: null, position: 3, managed: false, everyone: false, assignable: true, permissions: '1024' },
  ],
  emojis: [{ id: E1, name: 'party', animated: false }, { id: '900000000000000402', name: 'spin', animated: true }],
  bot: { userId: '900000000000000999', topRolePosition: 10, manageRoles: true, createInvite: true },
};
const TOKENS = '{server} {server.name} {server.id} {server.member_count} {server.member_count.ordinal} {server.icon} {server.owner} '
  + '{server.boost_count} {server.boost_level} {server.verification_level} {server.created} {server.created_at} {channel} {channel.name} '
  + '{channel.id} {channel.type} {user} {user.name} {user.username} {user.tag} {user.id} {user.display_name} {user.avatar} '
  + '{user.created} {user.created_at} {user.mention} {member.joined} {unknown}';
for (const [res, opts] of [
  [RESOURCES, { channelId: C1, user: { id: U1, username: 'alice', globalName: 'Alice', avatar: 'c'.repeat(32) } }],
  [RESOURCES, { channelId: C2, userId: U1 }],
  [RESOURCES, { channelId: '900000000000000199', user: { id: U2, username: 'bob', globalName: null, avatar: null } }],
  [{ guild: { id: G } }, {}],
  [null, { userId: 'nope' }],
]) {
  call('renderContextFor', [res, opts, TOKENS], () => renderPlaceholders(TOKENS, embeds.renderContextFor(res, opts)));
}
for (const o of [{}, { users: false }, { roles: [R1, R1, 'bad', R2] }, { users: true, roles: Array.from({ length: 120 }, (_, i) => String(900000000000000500n + BigInt(i))) }]) {
  call('restAllowedMentions', [o], () => embeds.restAllowedMentions(o));
}
const tplCtx = embeds.renderContextFor(RESOURCES, { channelId: C1, userId: U1 });
for (const tpl of [{ content: 'Hi {user}', embed: { title: 'T {server}', description: 'D' }, buttons: [{ label: 'Go', url: 'https://example.com' }] },
  { content: '', embed: { title: 'Only embed' }, buttons: [] }, { content: 'plain', embed: null, buttons: [] }]) {
  for (const edit of [false, true]) {
    const rendered = renderTemplate(tpl, tplCtx);
    call('toRestMessage', [json(rendered), edit], () => embeds.toRestMessage(json(rendered), { edit }));
  }
}
call('toRestMessage', [null, true], () => embeds.toRestMessage(null, { edit: true }));
const MEMBERS = [null, { owner: false, permissions: '0' }, { owner: true, permissions: '0' }, { owner: false, permissions: '8' },
  { owner: false, permissions: String(1n << 17n) }, { owner: false, permissions: '0', highestRolePosition: 4 }, { owner: false, permissions: 'junk' }];
for (const m of MEMBERS) call('memberCanMentionAll', [m], () => embeds.memberCanMentionAll(m));
for (const [m, userId] of [[null, U2], [null, U1], [{ owner: false, permissions: '8' }, U2], [{ owner: true, permissions: '0' }, U1], [null, null]]) {
  call('memberFor', [m, userId], () => embeds.memberFor({ member: m, userId }, RESOURCES));
}
const ROLE_IDS = [R1, R2, R3, G, '900000000000000304', '900000000000000305', '900000000000000306', '900000000000000307', '900000000000000399'];
for (const id of ROLE_IDS) {
  for (const m of MEMBERS) {
    call('pingRoleProblem', [RESOURCES, id, m], () => embeds.pingRoleProblem(RESOURCES, id, m));
    call('assignRoleProblem', [RESOURCES, id, m], () => embeds.assignRoleProblem(RESOURCES, id, m));
  }
}
for (const m of MEMBERS) call('splitPingableRoles', [[...ROLE_IDS, R1], RESOURCES, m], () => embeds.splitPingableRoles([...ROLE_IDS, R1], RESOURCES, m));
const apiErr = (code, status, discordCode) => Object.assign(new Error(`HTTP ${status}`), { name: 'DiscordApiError', code, status, ...(discordCode ? { discordCode } : {}) });
for (const e of [['forbidden', 403], ['unavailable', 403, 50001], ['unavailable', 403, 50013], ['invalid', 400], ['unavailable', 400, 50035],
  ['not_found', 404], ['rate_limited', 429], ['unavailable', 500]]) {
  for (const info of [{ op: 'send' }, { op: 'send', embeds: false, channel: { id: C1, name: 'general' } }, { op: 'edit', channelId: C2 },
    { op: 'delete', channel: { id: C1 } }]) {
    call('discordFailure', [{ code: e[0], status: e[1], discordCode: e[2] ?? null }, info], () => {
      const out = embeds.discordFailure(apiErr(...e), info);
      return { code: out.code, message: out.message, path: out.path ?? null };
    });
  }
}

// tags.js
const storedTagActions = [
  { type: 'reply', dm: false, embed: true, responses: ['a', 2], pingRoleIds: [R1] },
  { type: 'reply', dm: true, responses: ['dm'], pingRoleIds: [R1] },
  { type: 'send', channelId: C1, responses: ['s'], embed: false },
  { type: 'addRole', roleId: R1, responses: ['ignored'] },
  { type: 'toggleRole' },
  {},
];
for (const a of storedTagActions) call('tags.actionToForm', [a], () => tags.actionToForm(a));
const formTagActions = [
  { type: 'reply', responses: [{ text: 'a' }, { text: 'b' }], embed: true, channelId: C1, roleId: R1, pingRoleIds: [R1, R1, R2, R3, '900000000000000304', '900000000000000305', '900000000000000306'] },
  { type: 'dm', responses: [{ text: 'x' }], embed: false, pingRoleIds: [R1] },
  { type: 'send', responses: [{ text: 'y' }], embed: false, channelId: C2, pingRoleIds: [] },
  { type: 'removeRole', roleId: R2, responses: [{ text: 'z' }] },
  { type: 'unknown', responses: [] },
];
for (const f of formTagActions) call('tags.actionToStored', [f], () => tags.actionToStored(f));
for (const row of rowsOf('CustomTag')) call('tags.tagItem', [row, storedTagActions], () => tags.tagItem(jsRow(row), storedTagActions));

// triggers.js
for (const row of rowsOf('AutoTrigger')) call('triggers.triggerItem', [row], () => triggers.triggerItem(jsRow(row)));

// reminders.js
for (const s of [
  { mode: 'TIME_OF_DAY', timeOfDay: '09:30', weekdays: ['mon', 'fri', 'mon', 'xyz'], requireActivity: true },
  { mode: 'TIME_OF_DAY', timeOfDay: null, weekdays: [] },
  { mode: 'INTERVAL', intervalMinutes: 90, windowStart: '08:00', windowEnd: null, weekdays: ['sun', 'sat'] },
  { mode: 'INTERVAL', intervalMinutes: 1.5, weekdays: null },
  { mode: 'ONE_SHOT', runAt: ISO2 },
  { mode: 'ONE_SHOT', runAt: 'garbage' },
  { mode: 'ONE_SHOT', runAt: null },
  { mode: 'OTHER' },
]) call('reminders.scheduleInput', [s], () => reminders.scheduleInput(s));
for (const d of [[0, 1, 2, 3, 4, 5, 6], [1, 3], [6, 0, 9, -1, 2.5], null]) call('reminders.weekdayNames', [d], () => reminders.weekdayNames(d));
for (const n of [['sun', 'mon', 'sun'], ['bad'], null, ['sat', 'fri']]) call('reminders.weekdayNumbers', [n], () => reminders.weekdayNumbers(n));
const reminderRows = [
  ...rowsOf('Reminder'),
  { id: 'r1', name: 'a', channelId: C1, mode: 'TIME_OF_DAY', timeOfDay: '07:05', weekdays: [1, 2, 3, 4, 5], enabled: true },
  { id: 'r2', name: 'b', channelId: C1, mode: 'TIME_OF_DAY', timeOfDay: null, weekdays: [0, 6], enabled: false },
  { id: 'r3', name: 'c', channelId: C1, mode: 'TIME_OF_DAY', timeOfDay: '12:00', weekdays: [2, 4], enabled: true },
  { id: 'r4', name: 'd', channelId: C1, mode: 'INTERVAL', intervalMinutes: 1500, windowStart: null, windowEnd: '18:00', weekdays: [1, 2], enabled: true },
  { id: 'r5', name: 'e', channelId: C1, mode: 'INTERVAL', intervalMinutes: null, weekdays: [], enabled: true },
  { id: 'r6', name: 'f', channelId: C1, mode: 'ONE_SHOT', runAt: ISO, lastSentAt: ISO2, enabled: false },
  { id: 'r7', name: 'g', channelId: C1, mode: 'ONE_SHOT', runAt: ISO2, lastSentAt: ISO, enabled: false },
  { id: 'r8', name: 'h', channelId: C1, mode: 'ONE_SHOT', runAt: null, enabled: true },
  { id: 'r9', name: 'i', channelId: C1, mode: 'WEIRD', enabled: true },
];
for (const row of reminderRows) {
  call('reminders.scheduleText', [row, 'Europe/Berlin'], () => reminders.scheduleText(jsRow(row), 'Europe/Berlin'));
  call('reminders.isDone', [row], () => reminders.isDone(jsRow(row)));
  call('reminders.reminderItem', [row, 'UTC', ISO2], () => reminders.reminderItem(jsRow(row), { tz: 'UTC', nextRunAt: new Date(ISO2) }));
}

// feeds.js
for (const [type, raw] of [['RSS', 'https://example.com/feed.xml'], ['RSS', 'example.com/rss'], ['RSS', 'not a url'], ['RSS', ''],
  ['YOUTUBE', 'UC1234567890123456789012'], ['YOUTUBE', 'https://www.youtube.com/channel/UCabcdefghijklmnopqrstuv'], ['YOUTUBE', '@handle'],
  ['YOUTUBE', '??'], ['REDDIT', 'r/rust'], ['REDDIT', 'https://reddit.com/r/programming'], ['REDDIT', '!!'],
  ['BLUESKY', 'alice.bsky.social'], ['BLUESKY', 'https://bsky.app/profile/bob.bsky.social'], ['BLUESKY', '@@'],
  ['PODCAST', 'https://feeds.example.com/podcast'], ['PODCAST', 'FEED://x'], ['PODCAST', 'pod.example.org/rss.xml'], ['PODCAST', 'a'],
  ['PODCAST', '  The Daily  '], ['OTHER', 'x']]) {
  call('feeds.parseFeedSource', [type, raw], () => feeds.parseFeedSource(type, raw));
}
for (const row of rowsOf('Feed')) call('feeds.feedItem', [row, 'OK', 'https://example.com'], () => feeds.feedItem(jsRow(row), { statusText: 'OK', sourceUrl: 'https://example.com' }));

// levelRoles.js
for (const n of [0, 1, 2, 10, 100, 500, 999]) call('levelRoles.totalXpForLevel', [n], () => levelRoles.totalXpForLevel(n));
call('levelRoles.highestReachableLevel', [], () => levelRoles.highestReachableLevel());

// starboards.js
for (const e of ['⭐', '<:star:900000000000000401>', '<a:spin:900000000000000402>', E1, 'abc', '', 'x⭐', 5, null, '👍🏽', '1️⃣']) {
  call('starboards.isEmojiEntry', [e], () => starboards.isEmojiEntry(e));
}

// translateChannels.js / translateStats.js
call('translateChannels.describeLanguages', [['es', 'ja', 'zz', 'zh-tw']], () => translateChannels.describeLanguages(['es', 'ja', 'zz', 'zh-tw']));
const statRows = [
  { channelId: C1, via: 'auto', direction: 'from', lang: 'es', count: 80, firstAt: '2026-01-02T00:00:00.000Z', lastAt: '2026-05-01T00:00:00.000Z' },
  { channelId: C1, via: 'auto', direction: 'from', lang: 'en', count: 38, firstAt: '2026-01-01T00:00:00.000Z', lastAt: '2026-04-01T00:00:00.000Z' },
  { channelId: C1, via: 'command', direction: 'from', lang: 'und', count: 3, firstAt: '2026-02-01T00:00:00.000Z', lastAt: '2026-05-02T00:00:00.000Z' },
  { channelId: C1, via: 'auto', direction: 'to', lang: 'fr', count: 118, firstAt: '2026-01-02T00:00:00.000Z', lastAt: '2026-05-01T00:00:00.000Z' },
  { channelId: C2, via: 'command', direction: 'from', lang: 'de', count: 1, firstAt: '2026-03-01T00:00:00.000Z', lastAt: '2026-03-01T00:00:00.000Z' },
  ...['ja', 'ko', 'pt', 'it', 'ru', 'pl', 'nl', 'sv', 'tr'].map((lang, i) => ({
    channelId: '900000000000000103', via: 'auto', direction: 'from', lang, count: 10 - i, firstAt: ISO, lastAt: ISO,
  })),
  { channelId: '900000000000000104', via: 'auto', direction: 'from', lang: 'es', count: 1, firstAt: ISO, lastAt: ISO2 },
  { channelId: '900000000000000105', via: 'auto', direction: 'from', lang: 'es', count: 1, firstAt: ISO, lastAt: ISO2 },
];
const configured = [{ channelId: '900000000000000106', targetLangs: ['en', 'es'] }, { channelId: C2, targetLangs: [] }, { channelId: C1, targetLangs: ['fr'] }];
call('translateStats.buildStatItems', [statRows, configured], () => translateStats.buildStatItems(statRows.map(jsRow), configured));
call('translateStats.buildStatItems', [rowsOf('TranslateStat'), []], () => translateStats.buildStatItems(rowsOf('TranslateStat').map(jsRow), []));

// statChannels.js
for (const s of ['  Members:\n {count}  ', null, '', 'a  b']) call('statChannels.normalizeTemplate', [s], () => statChannels.normalizeTemplate(s));

// cases.js
for (const d of [{ action: 'WARN', userId: U1, reason: 'r', proofUrl: 'https://x' }, { action: 'TIMEOUT', userId: U1, duration: '2h', proofUrl: null },
  { action: 'BAN', userId: U1, duration: null, deleteDays: 3 }, { action: 'BAN', userId: U1, duration: '7d' }, { action: 'SOFTBAN', userId: U1 },
  { action: 'SOFTBAN', userId: U1, softbanDeleteDays: 1 }, { action: 'UNBAN', userId: U1, proofUrl: 'https://y' }, { action: 'KICK', userId: U1, reason: null },
  { action: 'TIMEOUT', userId: U1, duration: 'junk' }]) {
  call('cases.actionPayload', [d], () => cases.actionPayload(d));
}
const caseRows = [
  ...rowsOf('Infraction'),
  { caseId: 5, type: 'BAN', targetUserId: U1, moderatorId: U2, reason: '  multi\n  line \r\n reason ', active: true, expiresAt: '2026-05-01T00:00:00.000Z', createdAt: '2026-04-01T00:00:00.000Z' },
  { caseId: 6, type: 'BAN', targetUserId: U1, moderatorId: U2, reason: null, active: false, expiresAt: null, createdAt: '2026-04-01T00:00:00.000Z' },
  { caseId: 7, type: 'TIMEOUT', targetUserId: U2, moderatorId: U1, reason: '', active: true, expiresAt: '2026-07-01T00:00:00.000Z', createdAt: '2026-06-01T11:00:00.000Z' },
  { caseId: 8, type: 'MUTE', targetUserId: U2, moderatorId: U1, reason: 'x', active: false, expiresAt: '2026-07-01T00:00:00.000Z', createdAt: '2026-06-01T11:00:00.000Z' },
  { caseId: 9, type: 'WARN', targetUserId: U2, moderatorId: '900000000000000999', reason: 'w', active: false, createdAt: ISO },
  { caseId: 10, type: 'KICK', targetUserId: '800000000000000099', moderatorId: U1, reason: 'k', active: true, createdAt: ISO },
  { caseId: 11, type: 'MUTE', targetUserId: U2, moderatorId: U1, reason: 'x', active: false, expiresAt: '2026-05-01T00:00:00.000Z', createdAt: '2026-04-01T00:00:00.000Z' },
];
const profiles = {
  [U1]: { id: U1, username: 'alice', globalName: 'Alice', avatarUrl: 'https://cdn.example/a.png', bot: false },
  [U2]: { id: U2, username: 'bob', globalName: null, avatarUrl: '', bot: false },
  '900000000000000999': { id: '900000000000000999', username: '', globalName: 'Gee', avatarUrl: null, bot: true },
};
const profileMap = new Map(Object.entries(profiles));
profileMap.set('800000000000000099', null);
for (const row of caseRows) {
  call('cases.caseType', [row], () => cases.caseType(jsRow(row)));
  call('cases.caseStatus', [row, NOW], () => cases.caseStatus(jsRow(row), NOW));
  call('cases.caseItem', [row, profiles, NOW], () => cases.caseItem(jsRow(row), { profiles: profileMap, now: NOW }));
}
for (const r of [null, '', 'a\n\nb', ' x ', 5]) call('cases.reasonText', [r], () => cases.reasonText(r));
for (const q of ['12', '#12', ' #7 ', '0', '2147483648', '12345678901', 'abc', '#', '', null]) call('cases.caseNumberQuery', [q], () => cases.caseNumberQuery(q));

// levels.js / balances.js
for (const c of [`100_${U1}`, `2147483648_${U1}`, `12_123`, 'x', '', `0_${U1}`, `01_${U1}`]) {
  call('levels.readRankCursor', [c], () => levels.readRankCursor(c));
  call('levels.afterRankCursor', ['xp', c], () => levels.afterRankCursor('xp', c));
}
for (const t of [0, 99, 100, 155, 1000, 123456, -5, 1.9, NaN, 1e9, '300']) call('levels.levelProgress', [t], () => levels.levelProgress(t));
const PROFILES = [null, profiles[U1], profiles[U2], profiles['900000000000000999'], { id: U1, username: 'same', globalName: 'same', avatarUrl: 'u' }];
for (const p of PROFILES) {
  call('levels.profileName', [p, U1], () => levels.profileName(p, U1));
  call('levels.profileName', [p, null], () => levels.profileName(p, null));
  call('levels.profileLabel', [p, U1], () => levels.profileLabel(p, U1));
  call('levels.avatarFor', [p, U1], () => levels.avatarFor(p, U1));
}
for (const n of [0, 1234, 1234567.891, -1, null, 0.5]) call('levels.fmt', [n], () => levels.fmt(n));
for (const row of [...rowsOf('UserLevel'), { userId: U1, xp: 5000, messageCount: null, voiceMinutes: 3, lastXpAt: ISO }]) {
  for (const [rank, p] of [[null, null], [1234, profiles[U1]], [0, profiles[U2]]]) {
    call('levels.levelItem', [row, rank, p], () => levels.levelItem(jsRow(row), { rank, profile: p }));
  }
}
call('levels.emptyLevelItem', [U1, profiles[U1]], () => levels.emptyLevelItem(U1, profiles[U1]));
call('levels.emptyLevelItem', [U2, null], () => levels.emptyLevelItem(U2, null));
for (const row of [...rowsOf('EconomyAccount'), { userId: U2, balance: 1234567, dailyStreak: null, lastDaily: null, lastWork: ISO, updatedAt: ISO }]) {
  for (const [rank, p, cur] of [[null, null, undefined], [3, profiles[U1], { name: 'gems', emoji: '💎' }]]) {
    call('balances.balanceItem', [row, rank, p, cur ?? null], () => balances.balanceItem(jsRow(row), { rank, profile: p, ...(cur ? { currency: cur } : {}) }));
  }
}

// giveaways.js
for (const d of [{}, { duration: '1h' }, { duration: '  ' }, { duration: '1h', endsAt: ISO2 }, { duration: 'x' }, { duration: '30s' }, { duration: '91d' },
  { endsAt: ISO2 }, { endsAt: '2026-06-01T12:00:30.000Z' }, { endsAt: '2025-01-01T00:00:00.000Z' }, { endsAt: 'garbage' }, { endsAt: '2027-01-01T00:00:00.000Z' }]) {
  for (const required of [true, false]) call('giveaways.endTimeProblem', [d, required], () => giveaways.endTimeProblem(d, { required }));
}
const giveawayRows = [
  ...rowsOf('Giveaway').map((r) => ({ ...r, _count: { entries: 3 } })),
  { id: 'g1', guildId: G, channelId: C1, messageId: M1, prize: 'P', status: 'ACTIVE', endsAt: ISO2, winnerIds: [], hostId: U1, _count: { entries: 1 },
    roleWeights: [{ roleId: R1, weight: 3 }, { roleId: 5, weight: 2 }, { roleId: R2, weight: 1.5 }, null], xpPrize: 0, coinPrize: 50, embedColor: 0, winnerCount: 1 },
  { id: 'g2', guildId: G, channelId: C1, messageId: null, prize: 'Q', status: 'ENDED', endsAt: ISO, endedAt: null, winnerIds: [U1, U2, 'x'], hostId: 'nobody', winnerCount: 2 },
  { id: 'g3', guildId: G, channelId: C1, prize: 'R', status: 'ENDED', endsAt: ISO, endedAt: ISO2, winnerIds: [U1], hostId: U2 },
  { id: 'g4', guildId: G, channelId: C1, prize: 'S', status: 'DRAFT', endsAt: ISO, _count: {} },
  { id: 'g5', guildId: G, channelId: C1, prize: 'T', status: 'CANCELLED', endsAt: 'bad' },
  { id: 'g6', guildId: G, channelId: C1, prize: 'U', status: 'WEIRD' },
];
const names = { [U1]: 'Alice', [U2]: 'Bob' };
for (const row of giveawayRows) {
  call('giveaways.giveawaySummary', [row], () => giveaways.giveawaySummary(jsRow(row)));
  call('giveaways.giveawayItem', [row, names], () => giveaways.giveawayItem(jsRow(row), { names: new Map(Object.entries(names)) }));
}

// polls.js
for (const d of [null, '', '  ', '10m', '30s', '31d', 'nope', '1w']) call('polls.durationProblem', [d], () => polls.durationProblem(d));
const pollRows = [
  ...rowsOf('Poll'),
  { id: 'p1', guildId: G, channelId: C1, messageId: M1, question: 'Q?', options: ['A', 'B', 'C'], status: 'ACTIVE', endsAt: null, authorId: U1, maxChoices: 1 },
  { id: 'p2', guildId: G, channelId: C1, messageId: null, question: 'Q2', options: ['Long option '.repeat(6), 'B', 'C', 'D', 'E'], status: 'ENDED', endsAt: ISO, authorId: 'x' },
];
const tallies = [null, { counts: [5, 3, 1], votes: 9, voters: 7 }, { counts: [2, 2, 1, 2, 2], votes: 9, voters: 9 }, { counts: [1], votes: 0, voters: 1 },
  { counts: [0, 0], votes: 0, voters: 0 }];
for (const row of pollRows) {
  for (const t of tallies) {
    call('polls.pollItem', [row, t, names], () => polls.pollItem(jsRow(row), { tally: t, names: new Map(Object.entries(names)) }));
  }
}

// reactionRoles.js
for (const e of ['<:party:900000000000000401>', '<a:spin:900000000000000402>', '❤️', '️', '', null, 7, '<:bad name:1>', 'x']) {
  call('rr.emojiKey', [e], () => rr.emojiKey(e));
}
const rrItems = [
  { roleId: R1, label: 'A', emoji: '❤️', style: 1, description: null },
  { roleId: R2, label: 'B', emoji: '❤', style: 2, description: 'd' },
  { roleId: R1, label: 'C', emoji: null, style: 3, description: null },
];
for (const layout of ['BUTTONS', 'SELECT', 'REACTIONS', undefined, 'X']) {
  for (const items of [rrItems, rrItems.slice(0, 2), [rrItems[0], { roleId: R3, emoji: null }], Array.from({ length: 26 }, (_, i) => ({ roleId: String(900000000000000500n + BigInt(i)), emoji: String(i) }))]) {
    call('rr.itemsProblem', [items, layout ?? null], () => rr.itemsProblem(items, layout));
  }
}
const oldRrRows = [
  { id: 'i1', roleId: R1, label: 'A', emoji: '❤️', style: 1, description: null, position: 0 },
  { id: 'i2', roleId: R2, label: 'B', emoji: '<:party:900000000000000401>', style: 1, description: null, position: 1 },
  { id: 'i3', roleId: R3, label: 'C', emoji: '⭐', style: 1, description: null, position: 2 },
];
for (const next of [
  [{ roleId: R2, label: 'B', emoji: '<:party:900000000000000401>', style: 1, description: null }, { roleId: R1, label: 'A2', emoji: '❤', style: 2, description: 'x' }],
  [{ roleId: R1, label: 'A', emoji: '❤️', style: 1, description: null }, { roleId: R2, label: 'B', emoji: '<:party:900000000000000401>', style: 1, description: null }, { roleId: R3, label: 'C', emoji: '⭐', style: 1, description: null }],
  [],
  [{ roleId: '900000000000000304', label: 'N', emoji: '⭐', style: 4 }],
]) call('rr.diffPanelItems', [oldRrRows, next], () => rr.diffPanelItems(oldRrRows, next));
const rrInput = [
  { roleId: R1, label: '  ', emoji: '<:old:900000000000000401>', style: 3, description: '  desc  ' },
  { roleId: '900000000000000305', label: null, emoji: '<a:x:900000000000000999>', style: 2, description: '' },
  { roleId: '900000000000000399', label: 'L'.repeat(90), emoji: '⭐', style: 4, description: 'd'.repeat(120) },
];
for (const layout of ['BUTTONS', 'SELECT', 'REACTIONS']) {
  for (const held of [[], ['900000000000000999']]) {
    call('rr.normalizeItems', [rrInput, layout, RESOURCES, held], () => rr.normalizeItems(rrInput, layout, RESOURCES, new Set(held)));
  }
}
const rrPanels = [
  ...rowsOf('ReactionRoleGroup').map((r) => ({ ...r, items: rowsOf('ReactionRoleItem') })),
  { id: 'p1', guildId: G, channelId: C1, messageId: '0', layout: 'SELECT', type: 'UNIQUE', title: 'T', items: [{ roleId: R1, label: 'x', position: 2, style: 7 }, { roleId: R2, label: 'y', position: 1, style: 3, emoji: '⭐' }] },
  { id: 'p2', guildId: G, channelId: C1, messageId: M1, layout: 'REACTIONS', type: 'VERIFY', title: 'T', attached: true, items: [{ roleId: R1, label: 'x' }] },
  { id: 'p3', guildId: G, channelId: C1, messageId: M1, layout: 'X', type: 'Y', title: 'T', messageMissingAt: ISO, items: [] },
];
for (const row of rrPanels) call('rr.panelItem', [row], () => rr.panelItem(jsRow(row)));

// ticketPanels.js
for (const s of ['SELECT', 'BUTTONS', null]) call('tp.defaultPanelTemplate', [s], () => tp.defaultPanelTemplate(s));
for (const s of ['Ticket {number}', '  Héllo  Wörld!! ', '__-x--y-__', 'ＡＢＣ　ｄｅｆ', '日本語 チケット', '💥💥', null, 'a'.repeat(150), 'ﬁ İstanbul ΣΑΣ']) {
  call('tp.sanitizeChannelName', [s], () => tp.sanitizeChannelName(s));
}
for (const f of ['ticket-{number}', '{ User.Name }-{topic}', '{nope}', '', ' ', '{number}'.repeat(12), '{}', '💥', '{number', 'a{b{number}}', null, '{user.display_name}']) {
  call('tp.validateNameFormat', [f], () => tp.validateNameFormat(f));
}
for (const t of [null, {}, { content: '  ' }, { content: 'x' }, { embed: { color: 5 } }, { embed: { title: 't' } }, { embed: { fields: [{ name: 'a', value: 'b' }] } },
  { embed: { image: 'https://x/y.png' } }, { embed: { footer: { text: 'f' } } }, { content: null, embed: null, buttons: [{ label: 'a', url: 'https://x' }] }]) {
  call('tp.isEmptyTemplate', [t], () => tp.isEmptyTemplate(t));
}
for (const [e, held] of [['<:party:900000000000000401>', []], ['<:renamed:900000000000000402>', []], ['<:gone:900000000000000999>', []],
  ['<:gone:900000000000000999>', ['900000000000000999']], ['✉️', []], [null, []]]) {
  call('tp.canonicalEmoji', [e, RESOURCES, held, 'options.0.emoji'], () => tp.canonicalEmoji(e, RESOURCES, new Set(held), 'options.0.emoji'));
}
const tpOptions = [
  { id: 'o1', label: 'Support', emoji: '<:party:900000000000000401>', description: '  help  ', buttonStyle: 1, categoryId: C1, introTemplate: { content: 'Hi', embed: null, buttons: [] } },
  { id: null, label: 'Billing', emoji: null, description: '', buttonStyle: 3, categoryId: null, introTemplate: { content: '', embed: { color: 3 }, buttons: [] } },
  { label: 'Other', emoji: '✉️', description: 'x'.repeat(130), buttonStyle: 2 },
];
call('tp.normalizeOptions', [tpOptions, RESOURCES, []], () => tp.normalizeOptions(tpOptions, RESOURCES, new Set()));
call('tp.normalizeOptions', [[{ label: 'X', emoji: '<:z:900000000000000998>', buttonStyle: 1 }], RESOURCES, []], () => tp.normalizeOptions([{ label: 'X', emoji: '<:z:900000000000000998>', buttonStyle: 1 }], RESOURCES, new Set()));
const oldOptions = [
  { id: 'o1', label: 'Support', emoji: null, description: null, buttonStyle: 1, categoryId: null, introTemplate: null, position: 0 },
  { id: 'o2', label: 'Billing', emoji: '💳', description: 'pay', buttonStyle: 2, categoryId: C1, introTemplate: { content: 'Hi' }, position: 1 },
  { id: 'o3', label: 'Old', emoji: null, description: null, buttonStyle: 1, categoryId: null, introTemplate: null, position: 2 },
];
const nextOptions = tp.normalizeOptions([
  { id: 'o2', label: 'Billing', emoji: '💳', description: 'pay', buttonStyle: 2, categoryId: C1, introTemplate: { content: 'Hi', embed: null, buttons: [] } },
  { label: 'SUPPORT', emoji: null, description: 'new', buttonStyle: 1, categoryId: null, introTemplate: null },
  { id: 'zzz', label: 'Brand new', emoji: null, buttonStyle: 4, introTemplate: { content: 'Welcome' } },
], RESOURCES, new Set());
call('tp.diffOptions', [oldOptions, json(nextOptions)], () => tp.diffOptions(oldOptions, nextOptions));
call('tp.diffOptions', [oldOptions, []], () => tp.diffOptions(oldOptions, []));
for (const [t, d] of [[tp.defaultPanelTemplate('SELECT'), [tp.defaultPanelTemplate('BUTTONS'), tp.defaultPanelTemplate('SELECT')]], [{ content: 'x' }, tp.DEFAULT_INTRO_TEMPLATE],
  [tp.DEFAULT_INTRO_TEMPLATE, tp.DEFAULT_INTRO_TEMPLATE]]) {
  call('tp.isDefaultTemplate', [json(t), Array.isArray(d) ? json(d) : [json(d)]], () => tp.isDefaultTemplate(t, d));
}
const tpPanels = [
  ...rowsOf('TicketPanel').map((r) => ({ ...r, options: rowsOf('TicketPanelOption') })),
  { id: 'tp1', guildId: G, name: 'Help', style: 'SELECT', channelId: C1, messageId: M1, template: null, introTemplate: null, managerRoleIds: [R1],
    options: [{ id: 'b', label: 'B', position: 1, buttonStyle: 9 }, { id: 'a', label: 'A', position: 1, introTemplate: { content: 'x' } }, { id: 'c', label: 'C', position: 0 }] },
  { id: 'tp2', guildId: G, name: 'Help2', style: 'BUTTONS', channelId: null, messageId: null, options: [{ id: 'x', label: 'Only' }] },
];
for (const row of tpPanels) call('tp.panelItem', [row, 4], () => tp.panelItem(jsRow(row), 4));

// automations.js
const storedConditions = [
  { type: 'CHANNEL_IN', channelIds: [C1] }, { type: 'CATEGORY_NOT_IN', categoryIds: [C2] }, { type: 'CONTAINS', words: ['a', 'b'] },
  { type: 'EXACT', texts: ['hi'] }, { type: 'MATCHES_REGEX', pattern: '^a', flags: 'mi' }, { type: 'MATCHES_REGEX' },
  { type: 'REACTED_WITH', emojis: ['⭐', '<:party:900000000000000401>'] }, { type: 'HAS_REACTIONS', min: 5, emoji: '⭐' }, { type: 'HAS_REACTIONS' },
  { type: 'IS_REPLY' }, { type: 'HAS_ALL_ROLES', roleIds: [R1, R2] }, { type: 'USER_IS', userIds: [U1] },
  { type: 'BUTTON_IS', buttons: [{ automationId: 'abcdefgh1', index: 2 }, { automationId: 'unknownid', index: 1 }] }, { type: 'NOPE' }, {}, { type: null },
];
const nameOf = (id) => (id === 'abcdefgh1' ? 'my-auto' : null);
for (const c of storedConditions) {
  call('auto.conditionToForm', [c], () => auto.conditionToForm(c, nameOf));
  const form = auto.conditionToForm(c, nameOf);
  call('auto.conditionToStored', [json(form)], () => auto.conditionToStored(form, (x) => (x === 'my-auto' ? 'abcdefgh1' : x)));
}
const storedActions = [
  { type: 'CREATE_THREAD', name: 'T', channelId: C1, autoArchiveMinutes: 60 }, { type: 'CREATE_THREAD' },
  { type: 'SEND_MESSAGE', content: 'hi', channelId: C1, button: { label: 'L', url: 'https://x' } },
  { type: 'SEND_MESSAGE', content: 'hi', button: { label: 'L', url: null, style: 'DANGER', index: 4 } },
  { type: 'REPLY', content: 'r', button: null }, { type: 'SEND_IN_THREAD', content: 's', button: { label: 'Z', index: 2 } },
  { type: 'REPOST', channelId: C2 }, { type: 'REACT', emoji: '⭐' }, { type: 'REMOVE_REACTION' }, { type: 'GIVE_ROLES', roleIds: [R1] },
  { type: 'ADD_TO_THREAD', userIds: [U1, U2] }, { type: 'SEND_EMBED', embedName: 'rules', channelId: null }, { type: 'GIVE_XP', amount: 50 },
  { type: 'TAKE_COINS' }, { type: 'PIN' }, {},
];
for (const a of storedActions) {
  call('auto.actionToForm', [a], () => auto.actionToForm(a));
  const form = auto.actionToForm(a);
  for (const idx of [null, 7]) call('auto.actionToStored', [json(form), idx], () => auto.actionToStored(form, idx));
}
call('auto.blankCondition', [], () => auto.blankCondition());
call('auto.blankAction', ['REPLY'], () => auto.blankAction('REPLY'));
for (const f of [['m', 'i', 'i', 'x', 's'], [], null, ['u']]) call('auto.flagsToStored', [f], () => auto.flagsToStored(f));
const btn = (kind, index = null) => ({ type: 'SEND_MESSAGE', content: 'x', button: { kind, label: 'b', url: kind === 'link' ? 'https://x' : null, style: 'PRIMARY', index } });
for (const [actions, opts] of [
  [[btn('automation'), btn('automation', 3), btn('link'), btn('none')], { existing: [3, 5], buttonSeq: 6 }],
  [[btn('automation', 4)], { existing: [3] }],
  [[btn('automation', 3), btn('automation', 3)], { existing: [3] }],
  [[btn('automation'), btn('automation')], { existing: [], buttonSeq: 98 }],
  [[btn('automation')], { existing: [], buttonSeq: 1.5 }],
  [[btn('automation')], { assign: false }],
  [[{ type: 'REPLY', content: 'x', button: { kind: 'automation', index: null } }], {}],
]) call('auto.planButtons', [actions, opts], () => auto.planButtons(actions, opts));
const shapes = [
  { trigger: 'NOPE', conditions: [], actions: [], cooldownSeconds: 0 },
  { trigger: 'MEMBER_JOINED', conditions: [{ type: 'CONTAINS' }, { type: 'IS_REPLY' }, { type: 'IS_REPLY' }, { type: 'IS_NOT_REPLY' }, { type: 'X' }],
    actions: [{ type: 'SEND_MESSAGE' }, { type: 'CREATE_THREAD' }, { type: 'ADD_TO_THREAD' }, { type: 'SEND_IN_THREAD' }, { type: 'PIN' }, { type: 'Q' },
      { type: 'REPLY', button: { url: null, index: 2 } }, { type: 'SEND_MESSAGE', channelId: C1, button: { url: null, index: 2 } }], cooldownSeconds: 1.5 },
  { trigger: 'MESSAGE_SENT', conditions: Array.from({ length: 11 }, () => ({ type: 'CHANNEL_IN' })), actions: Array.from({ length: 11 }, () => ({ type: 'PIN' })), cooldownSeconds: 604801 },
  { trigger: 'ROLE_GAINED', conditions: [{ type: 'GAINED_ROLE' }, { type: 'LOST_ROLE' }], actions: [{ type: 'SEND_EMBED' }, { type: 'GIVE_ROLES' }], cooldownSeconds: -1 },
];
for (const sh of shapes) call('auto.shapeProblems', [sh], () => auto.shapeProblems(sh));
const autoRows = [...rowsOf('Automation'), { id: 'a1', name: 'x', trigger: 'MESSAGE_SENT', conditions: storedConditions.slice(0, 3), actions: storedActions.slice(2, 5),
  cooldownSeconds: 30, enabled: true, uses: 5, createdBy: U1, createdAt: ISO, updatedAt: ISO2 }, { id: 'a2', name: 'y', trigger: 'WEIRD', conditions: [{ type: 'IS_REPLY' }], actions: [], createdBy: 'system' },
{ id: 'a3', name: 'z', trigger: null, conditions: null, actions: [{ type: 'PIN' }] }];
for (const row of autoRows) {
  call('auto.summaryOf', [row], () => auto.summaryOf(row));
  call('auto.automationItem', [row, ['p1'], null], () => auto.automationItem(jsRow(row), { nameOf, problems: ['p1'] }));
  call('auto.automationItem', [row, [], 'Creator'], () => auto.automationItem(jsRow(row), { nameOf, creator: 'Creator' }));
}
const autoCol = index.COLLECTIONS.automations;
for (const [item, keys] of [[{ actions: [{ type: 'GIVE_ROLES', roleIds: [R1] }, { type: 'TAKE_ROLES', roleIds: [R2, R1] }] }, ['actions.roleIds|' + R1, 'presetRoleId|' + R3]],
  [null, ['x|1']], [{ actions: [{ roleIds: [R1] }] }, []]]) {
  call('auto.refKeys', [item, keys], () => [...autoCol.refKeys(item, new Set(keys))]);
}
for (const [ref, data] of [[{ path: 'actions.1.roleIds.0', id: R1 }, { actions: [{ type: 'X' }, { type: 'TAKE_ROLES' }] }],
  [{ path: 'actions.5.roleIds.0', id: R1 }, { actions: [] }], [{ path: 'presetRoleId', id: R2 }, {}], [{ path: 'actions.0.roleIds', id: R2 }, null],
  [{ path: 'tiers.3.roleAdd', id: R2 }, {}]]) {
  call('auto.refKeyOf', [ref, data], () => autoCol.refKeyOf(ref, data));
}
call('auto.problemKey', ['abc'], () => auto.problemKey('abc'));

write('helpers.json', { now: NOW, cases: cases_ });

// ── public.json ─────────────────────────────────────────────────────────────────────────────────────────────────
write('public.json', index.COLLECTION_LIST.map((c) => json(index.publicCollection(c))));

// ── validate.json ───────────────────────────────────────────────────────────────────────────────────────────────
const TPL = { content: 'Hello {user}', embed: { title: 'T', description: 'D' }, buttons: [] };
/** A valid create per collection (the minimum the form sends, plus what the checks need). */
const BASE = {
  embeds: { name: 'server-rules', template: TPL },
  tags: { name: 'rules' },
  triggers: { trigger: 'hello', response: 'Hi!' },
  reminders: { name: 'daily-news', channelId: C1, mode: 'TIME_OF_DAY', timeOfDay: '09:00' },
  feeds: { type: 'RSS', source: 'https://example.com/feed.xml', channelId: C1 },
  levelRoles: { level: 5, roleId: R1 },
  shopItems: { name: 'Cookie', price: 100 },
  achievements: { name: 'Chatter', trigger: 'MESSAGES', tiers: [{ goal: 10 }, { goal: 100 }] },
  starboards: { name: 'stars', channelId: C1 },
  translateChannels: { channelId: C1, targetLangs: ['es', 'ja'] },
  translateStats: {},
  tempVoiceHubs: { channelId: C1 },
  statChannels: { kind: 'MEMBERS', channelId: C1 },
  economyBoosts: { name: 'weekend', percent: 50 },
  cases: { action: 'WARN', userId: U1, reason: 'spam' },
  levels: { userId: U1, amount: 100 },
  balances: { userId: U1, amount: 100 },
  giveaways: { prize: 'Nitro', channelId: C1, duration: '1d' },
  polls: { question: 'Pizza?', options: ['Yes', 'No'], channelId: C1 },
  reactionRoles: { channelId: C1, items: [{ roleId: R1 }] },
  ticketPanels: { name: 'Support' },
  automations: { name: 'greeter', trigger: 'MESSAGE_SENT', actions: [{ type: 'REPLY', content: 'Hi' }] },
};

/** Probe values for one descriptor, by type (plus a few that fit no type). */
function probesFor(d) {
  const common = [null, true, 0, -1, 1.5, 'abc', '', [], {}, '<@123>'];
  const byType = {
    text: [' x ', 'a'.repeat(5000), 'line\nbreak', 'Ünïcödé ✨', 'server-rules', 'UPPER', '-bad', 'a--b', '{user}', 'a\u0000b'],
    boolean: [false, 'true', 1],
    int: [1, 2, 3, 5, 7, 10, 100, 1000, 99999, 1e9, 2147483648, '5', 3.0, -0],
    float: [0.5, 2, 100],
    enum: [...(d.options ?? []).map((o) => o.value), 'NOPE'],
    multiEnum: [[...(d.options ?? []).slice(0, 2).map((o) => o.value)], ['NOPE'], (d.options ?? []).map((o) => o.value), [(d.options ?? [])[0]?.value, (d.options ?? [])[0]?.value]],
    channel: [C1, C2, '1', 123],
    channels: [[C1], [C1, C1, C2], ['bad'], Array.from({ length: 60 }, (_, i) => String(900000000000000600n + BigInt(i)))],
    role: [R1, R2, 'x'],
    roles: [[R1], [R1, R1], ['nope'], Array.from({ length: 30 }, (_, i) => String(900000000000000700n + BigInt(i)))],
    user: [U1, 'nope'],
    emoji: ['⭐', '<:party:900000000000000401>', 'abc', '👍🏽'],
    url: ['https://example.com/x.png', 'http://a.b', 'ftp://x', 'javascript:alert(1)'],
    datetime: ['2026-06-01T12:00:30.000Z', '2026-07-01T00:00:00.000Z', '2031-07-01T00:00:00.000Z', '2025-01-01T00:00:00.000Z', '2026-07-01'],
    time: ['09:30', '24:00', '9:3', '23:59'],
    color: ['#FF0000', 'ff00ff', 16777215, 16777216, '#GGG'],
    stringList: [['a', 'b'], ['a', 'a'], [''], ['⭐', E1], Array.from({ length: 30 }, (_, i) => `o${i}`), [1]],
    template: [TPL, { content: '', embed: null, buttons: [] }, { content: 'x'.repeat(3000) }],
    embed: [{ title: 't' }],
    timezone: ['Europe/Berlin', 'Nope/Zone'],
    objectList: [],
  };
  const out = [...common, ...(byType[d.type] ?? [])];
  if (d.type === 'objectList') {
    const item = Object.fromEntries((d.itemFields ?? []).map((f) => [f.field, f.default === undefined ? null : f.default]));
    out.push([item], [item, item], Array.from({ length: (d.maxItems ?? 5) + 1 }, () => item), [null], ['x']);
    for (const f of d.itemFields ?? []) {
      for (const p of probesFor(f).slice(0, 24)) out.push([{ ...item, [f.field]: p }]);
    }
  }
  if (d.type === 'object') for (const f of d.fields ?? []) for (const p of probesFor(f).slice(0, 12)) out.push({ [f.field]: p });
  return out;
}

const vcases = [];
function vcase(key, mode, data, { actionId, current } = {}) {
  let out;
  try {
    const r = index.validateRecordInput(key, data, { mode, actionId, current: current ?? null });
    out = { ok: json(r) };
  } catch (err) {
    out = { err: { name: err?.name ?? 'Error', code: err?.code ?? null, path: err?.path ?? null, message: String(err?.message ?? err) } };
  }
  vcases.push({ key, mode, actionId: actionId ?? null, data: json(data), current: current === undefined ? null : json(current), ...out });
}
const rawData = (text) => JSON.parse(text);

for (const c of index.COLLECTION_LIST) {
  const base = BASE[c.key] ?? {};
  vcase(c.key, 'create', base);
  let current = null;
  try {
    current = { id: 'x1', ...index.validateRecordInput(c.key, base, { mode: 'create' }).data };
  } catch {
    current = { id: 'x1', ...base };
  }
  for (const d of c.fields) {
    for (const p of probesFor(d)) {
      vcase(c.key, 'create', { ...base, [d.field]: p });
      vcase(c.key, 'update', { [d.field]: p }, { current });
    }
    const without = { ...base };
    delete without[d.field];
    vcase(c.key, 'create', without);
  }
  for (const k of ['zzz', 'a'.repeat(50), '0', 'constructor', 'prototype', 'id']) {
    vcase(c.key, 'create', { ...base, [k]: 1 });
    vcase(c.key, 'update', { [k]: 1 }, { current });
  }
  vcase(c.key, 'create', rawData('{"__proto__": 1}'));
  vcase(c.key, 'update', rawData('{"2": 1, "b": 2, "1": 3}'), { current });
  for (const bad of [null, [], 'x', 5]) vcase(c.key, 'create', bad);
  vcase(c.key, 'update', {}, { current });
  for (const f of c.createOnly ?? []) vcase(c.key, 'update', { [f]: base[f] ?? null }, { current });
  for (const a of c.actions ?? []) {
    vcase(c.key, 'action', {}, { actionId: a.id, current });
    for (const d of a.fields ?? []) for (const p of probesFor(d)) vcase(c.key, 'action', { [d.field]: p }, { actionId: a.id, current });
    vcase(c.key, 'action', { zzz: 1 }, { actionId: a.id, current });
  }
  vcase(c.key, 'action', {}, { actionId: 'nope', current });
}
vcase('nope', 'create', {});

// Hand cases for the check rules.
const H = (key, mode, data, opts) => vcase(key, mode, data, opts);
const tagReply = (o = {}) => ({ type: 'reply', responses: [{ text: 'hi' }], embed: false, channelId: null, roleId: null, pingRoleIds: [], ...o });
H('tags', 'create', { name: 'a', actions: [tagReply({ responses: [] })] });
H('tags', 'create', { name: 'a', actions: [tagReply({ responses: [{ text: 'x'.repeat(2500) }] })] });
H('tags', 'create', { name: 'a', actions: [tagReply({ embed: true, responses: [{ text: 'x'.repeat(2500) }] })] });
H('tags', 'create', { name: 'a', actions: [tagReply({ type: 'send' })] });
H('tags', 'create', { name: 'a', actions: [tagReply({ type: 'dm', pingRoleIds: [R1] })] });
H('tags', 'create', { name: 'a', actions: [tagReply({ type: 'addRole' })] });
H('tags', 'create', { name: 'a', allowRoleIds: [R1, R2], denyRoleIds: [R3, R2] });
H('tags', 'create', { name: 'a', allowChannelIds: [C1], denyChannelIds: [C1] });
H('tags', 'create', { name: 'a', cooldownScope: 'USER', cooldownSeconds: 0 });
H('tags', 'update', { cooldownScope: 'SERVER' }, { current: { cooldownSeconds: 0, actions: [tagReply()] } });
H('tags', 'update', { cooldownSeconds: 5 }, { current: { cooldownScope: 'USER', actions: [tagReply()] } });
H('triggers', 'create', { trigger: '(a+)+', response: 'x', matchType: 'REGEX' });
H('triggers', 'create', { trigger: '[a-z', response: 'x', matchType: 'REGEX', caseSensitive: true });
H('triggers', 'create', { trigger: '   ', response: 'x' });
H('triggers', 'update', { matchType: 'REGEX' }, { current: { trigger: '(x', caseSensitive: false } });
H('triggers', 'update', { response: 'only' }, { current: { trigger: '(x', matchType: 'REGEX' } });
H('reminders', 'create', { name: 'a', channelId: C1, mode: 'TIME_OF_DAY' });
H('reminders', 'create', { name: 'a', channelId: C1, mode: 'INTERVAL' });
H('reminders', 'create', { name: 'a', channelId: C1, mode: 'INTERVAL', intervalMinutes: 30, windowStart: '08:00', windowEnd: '08:00' });
H('reminders', 'create', { name: 'a', channelId: C1, mode: 'ONE_SHOT' });
H('reminders', 'create', { name: 'a', channelId: C1, mode: 'ONE_SHOT', runAt: ISO2 });
H('reminders', 'update', { mode: 'INTERVAL' }, { current: { intervalMinutes: 5 } });
for (const [type, source, extra] of [['YOUTUBE', '@@'], ['REDDIT', 'r/rust', { includeReposts: true }], ['BLUESKY', 'alice.bsky.social', { includeReposts: true }], ['PODCAST', 'a']]) {
  H('feeds', 'create', { type, source, channelId: C1, ...(extra ?? {}) });
}
H('feeds', 'update', { includeReposts: true }, { current: { type: 'RSS' } });
H('levelRoles', 'create', { level: 999, roleId: R1 });
H('levelRoles', 'update', { roleId: R2 }, { current: { level: 900 } });
H('levelRoles', 'update', { roleId: R2 }, { current: { level: 5 } });
H('shopItems', 'create', { name: ' NONE ', price: 1 });
H('shopItems', 'create', { name: 'r', price: 1, type: 'ROLE' });
H('shopItems', 'create', { name: 'r', price: 1, roleId: R1 });
H('shopItems', 'create', { name: 'r', price: 1, type: 'STATIC', useOnBuy: true });
H('shopItems', 'create', { name: 'r', price: 1, stock: 0 });
H('shopItems', 'update', { stock: 0 }, { current: { type: 'ITEM' } });
H('shopItems', 'update', { useOnBuy: true }, { current: { type: 'STATIC' } });
H('shopItems', 'update', { type: 'ROLE' }, { current: { roleId: R1 } });
H('achievements', 'create', { name: 'a', trigger: 'LEVEL', tiers: [{ goal: 5 }, { goal: 2000 }] });
H('achievements', 'create', { name: 'a', trigger: 'BOOST', tiers: [{ goal: 5 }, { goal: 5 }] });
H('achievements', 'create', { name: 'a', trigger: 'MESSAGES', tiers: [{ goal: 5, roleAdd: R1, roleRemove: R1 }] });
for (const deadline of ['2026-06-01T12:00:30.000Z', '2026-06-01T12:02:00.000Z', '2031-07-01T00:00:00.000Z', '2030-01-01T00:00:00.000Z']) {
  H('achievements', 'create', { name: 'a', trigger: 'MESSAGES', tiers: [{ goal: 5 }], deadline });
  H('achievements', 'update', { deadline }, { current: { trigger: 'MESSAGES', deadline: '2026-06-01T12:00:30.000Z' } });
}
H('achievements', 'update', { tiers: [{ goal: 2000 }] }, { current: { trigger: 'LEVEL' } });
H('starboards', 'create', { name: 's', channelId: C1, emojis: ['⭐', 'nope'] });
H('starboards', 'create', { name: 's', channelId: C1, selfStar: true, removeSelfStars: true });
H('starboards', 'update', { removeSelfStars: true }, { current: { selfStar: true } });
for (const nameFormat of ['{user}', '{ USER.mention }', '<@123>', '<#!5>', '{username}', '{ uſer }']) H('tempVoiceHubs', 'create', { channelId: C1, nameFormat });
H('statChannels', 'create', { kind: 'ROLE_MEMBERS', channelId: C1 });
H('statChannels', 'create', { kind: 'MEMBERS', channelId: C1, roleId: R1 });
for (const template of ['Members', '{COUNT} {Random}', '{count} {role}', '{Count}  members']) H('statChannels', 'create', { kind: 'MEMBERS', channelId: C1, template });
H('statChannels', 'update', { template: '{role}: {count}' }, { current: { kind: 'ROLE_MEMBERS' } });
for (const d of [{ action: 'WARN', userId: U1 }, { action: 'KICK', userId: U1, reason: 'x'.repeat(600) }, { action: 'TIMEOUT', userId: U1 },
  { action: 'TIMEOUT', userId: U1, duration: '30s' }, { action: 'TIMEOUT', userId: U1, duration: '2y' }, { action: 'BAN', userId: U1, duration: '2y' },
  { action: 'BAN', userId: U1, duration: 'junk' }, { action: 'BAN', userId: U1 }, { action: 'KICK', userId: U1, duration: 'junk' }]) H('cases', 'create', d);
H('cases', 'update', { reason: '' }, { current: { reason: 'x' } });
H('cases', 'update', { proofUrl: null }, { current: { proofUrl: 'https://x' } });
H('cases', 'update', { proofUrl: null }, { current: { proofUrl: null } });
for (const d of [{ prize: 'p', channelId: C1 }, { prize: 'p', channelId: C1, duration: '1h', endsAt: ISO2 }, { prize: 'p', channelId: C1, endsAt: '2025-01-01T00:00:00.000Z' },
  { prize: 'p', channelId: C1, endsAt: '2026-06-01T12:00:30.000Z' }, { prize: 'p', channelId: C1, duration: '100d' }, { prize: 'p', channelId: C1, duration: '1d', requiredRoleIds: [R1], deniedRoleIds: [R1] },
  { prize: 'p', channelId: C1, duration: '1d', roleWeights: [{ roleId: R1, weight: 2 }, { roleId: R1, weight: 3 }] }]) H('giveaways', 'create', d);
H('giveaways', 'update', { roleWeights: [{ roleId: R1, weight: 2 }, { roleId: R1, weight: 3 }] }, { current: {} });
for (const d of [{}, { duration: '1h' }, { endsAt: '2025-01-01T00:00:00.000Z' }]) H('giveaways', 'action', d, { actionId: 'restart', current: {} });
for (const d of [{ question: 'q', options: ['a'], channelId: C1 }, { question: 'q', options: ['a', 'b'], channelId: C1, duration: '10s' },
  { question: 'q', options: ['a', 'b'], channelId: C1, allowedRoleIds: [R1], deniedRoleIds: [R1] }]) H('polls', 'create', d);
H('reactionRoles', 'create', { channelId: C1, layout: 'REACTIONS', items: [{ roleId: R1 }] });
H('reactionRoles', 'create', { channelId: C1, layout: 'REACTIONS', items: [{ roleId: R1, emoji: '❤️' }, { roleId: R2, emoji: '❤' }] });
H('reactionRoles', 'create', { channelId: C1, items: [{ roleId: R1 }, { roleId: R1 }] });
H('reactionRoles', 'update', { description: 'new' }, { current: { attached: true, description: 'old', layout: 'BUTTONS' } });
H('reactionRoles', 'update', { description: 'old', title: 'x' }, { current: { attached: true, description: 'old' } });
H('reactionRoles', 'update', { items: [{ roleId: R1 }] }, { current: { layout: 'REACTIONS' } });
H('ticketPanels', 'create', { name: 'p', channelNameFormat: '{nope}' });
H('ticketPanels', 'create', { name: 'p', options: [{ label: 'A' }, { label: 'a' }] });
H('ticketPanels', 'create', { name: 'p', options: [{ label: 'A', id: 'o1' }, { label: 'B', id: 'o1' }] });
H('ticketPanels', 'update', { channelId: null }, { current: { channelId: C1 } });
const aCond = (type, o = {}) => ({ type, ...o });
for (const d of [
  { name: 'p', preset: 'welcomer' }, { name: 'p', preset: 'welcomer', presetChannelId: C1 }, { name: 'p', preset: 'starboard' },
  { name: 'p', preset: 'thread-joiner' }, { name: 'p', preset: 'commands-only' },
  { name: 'a', trigger: 'MESSAGE_SENT', conditions: [aCond('CHANNEL_IN')], actions: [] },
  { name: 'a', trigger: 'MESSAGE_SENT', conditions: [aCond('MATCHES_REGEX', { pattern: '(a+)+' })] },
  { name: 'a', trigger: 'MESSAGE_SENT', conditions: [aCond('MATCHES_REGEX', { pattern: '' })] },
  { name: 'a', trigger: 'MESSAGE_SENT', conditions: [aCond('HAS_ANY_ROLE')] },
  { name: 'a', trigger: 'MEMBER_JOINED', conditions: [aCond('CONTAINS', { words: ['x'] })] },
  { name: 'a', trigger: 'MEMBER_JOINED', actions: [{ type: 'SEND_MESSAGE', content: 'x' }] },
  { name: 'a', trigger: 'MEMBER_JOINED', actions: [{ type: 'CREATE_THREAD' }] },
  { name: 'a', trigger: 'MESSAGE_SENT', actions: [{ type: 'SEND_MESSAGE', content: ' ' }] },
  { name: 'a', trigger: 'MESSAGE_SENT', actions: [{ type: 'SEND_MESSAGE', content: 'x', button: { kind: 'link', label: 'l', url: '' } }] },
  { name: 'a', trigger: 'MESSAGE_SENT', actions: [{ type: 'SEND_MESSAGE', content: 'x', button: { kind: 'automation', label: '', url: null } }] },
  { name: 'a', trigger: 'MESSAGE_SENT', actions: [{ type: 'SEND_MESSAGE', content: 'x', button: { kind: 'automation', label: 'b', index: 2 } }] },
  { name: 'a', trigger: 'MESSAGE_SENT', actions: [{ type: 'REPOST' }] }, { name: 'a', trigger: 'MESSAGE_SENT', actions: [{ type: 'REACT' }] },
  { name: 'a', trigger: 'MESSAGE_SENT', actions: [{ type: 'GIVE_ROLES' }] }, { name: 'a', trigger: 'MESSAGE_SENT', actions: [{ type: 'SEND_EMBED' }] },
  { name: 'a', trigger: 'MESSAGE_SENT', actions: [], enabled: true }, { name: 'a', trigger: 'MESSAGE_SENT', actions: [], enabled: false },
]) H('automations', 'create', d);
H('automations', 'update', { enabled: true }, { current: { trigger: 'MESSAGE_SENT', actions: [], enabled: false } });
H('automations', 'update', { enabled: true, actions: [] }, { current: { trigger: 'MESSAGE_SENT', actions: [], enabled: true } });
H('automations', 'update', { actions: [{ type: 'SEND_MESSAGE', content: 'x', button: { kind: 'automation', label: 'b', index: 2 } }] },
  { current: { trigger: 'MESSAGE_SENT', actions: [{ type: 'SEND_MESSAGE', content: 'y', button: { kind: 'automation', label: 'b', index: 2 } }] } });
H('automations', 'update', { trigger: 'MEMBER_JOINED' }, { current: { trigger: 'MESSAGE_SENT', actions: [{ type: 'CREATE_THREAD', threadChannelId: null }] } });

write('validate.json', { now: NOW, cases: vcases });
process.stdout.write(`${cases_.length} helper cases, ${vcases.length} validation cases\n`);
process.exit(0);
