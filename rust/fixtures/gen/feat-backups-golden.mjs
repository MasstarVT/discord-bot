// Reference data for feat-backups, produced by the JS code itself (no servers needed):
//   node rust/fixtures/gen/feat-backups-golden.mjs
// writes rust/fixtures/golden/feat-backups/golden.json: snapshot capture and parse, restore-state normalisation, permission
// masking, whole restores of fake guilds (the fake of test/backups.test.js, with its call log), previews, the summary,
// preview, status and list texts, customIds, and the /emoji parsers and texts.
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};

const { AuditLogEvent, ChannelType, OverwriteType, PermissionsBitField } = await import('discord.js');
const snap = await imp('src', 'utils', 'guildSnapshot.js');
const svc = await imp('src', 'services', 'backupService.js');
const emoji = await imp('src', 'commands', 'admin', 'emoji.js');
const { buildCustomId } = await imp('src', 'config', 'customIds.js');
const { SafeFetchError } = await imp('src', 'utils', 'safeFetch.js');
const { TimeoutError } = await imp('src', 'utils', 'async.js');

const J = (v) => JSON.parse(JSON.stringify(v, (_, x) => (typeof x === 'bigint' ? x.toString() : x)));
const B = (s) => BigInt(s ?? '0');

const fake = await import(pathToFileURL(join(ROOT, 'rust', 'fixtures', 'gen', 'feat-backups-fake.mjs')).href);
const { makeGuild, resetIds, bp, BOT_PERMS, MOD_PERMS, MEMBER_PERMS, EVERYONE_PERMS, ORIGINAL } = fake;

const SNAPSHOT = snap.captureGuild(makeGuild(ORIGINAL), new Date('2026-10-21T10:00:00Z'));
const roundTrip = (v) => JSON.parse(JSON.stringify(v));

async function restoreAll(guild, { mode = 'missing', snapshot = SNAPSHOT, idMap = {}, maxBatches = 60 } = {}) {
  let state = snap.newRestoreState({ backupId: 'bk000001', mode, startedById: 'OWNER', channelId: 'CH', idMap, now: Date.now() - 1000 });
  let batches = 0;
  for (;;) {
    state = snap.normaliseRestoreState(roundTrip(state));
    const env = snap.makeEnv({ guild, snapshot: snap.parseSnapshot(roundTrip(snapshot)), state, reason: 'test' });
    const result = await snap.runRestoreBatch(env);
    state.retry = false;
    batches += 1;
    if (result === 'finish' || batches >= maxBatches) return { state, batches };
  }
}

function guildState(guild) {
  return {
    roles: [...guild.roles.cache.values()].map((r) => ({ id: r.id, name: r.name, color: r.colors.primaryColor, hoist: r.hoist, mentionable: r.mentionable, permissions: String(r.permissions.bitfield), rawPosition: r.rawPosition })),
    channels: [...guild.channels.cache.values()].map((c) => ({
      id: c.id, type: c.type, name: c.name, parentId: c.parentId, rawPosition: c.rawPosition, topic: c.topic, nsfw: c.nsfw, rateLimitPerUser: c.rateLimitPerUser,
      bitrate: c.bitrate, userLimit: c.userLimit,
      overwrites: [...c.permissionOverwrites.cache.values()].map((o) => ({ id: o.id, type: o.type, allow: String(o.allow.bitfield), deny: String(o.deny.bitfield) })),
    })),
  };
}

const SCENARIOS = {
  missing: { guild: { botPerms: BOT_PERMS, roles: [{ id: '101', name: 'Admin (renamed)', perms: bp('ViewChannel'), pos: 8 }, { id: '104', name: 'SomeBot', managed: true, pos: 2 }, { id: '999', name: 'Bot', pos: 10 }],
    channels: [{ id: '206', type: 15, name: 'help (renamed)', pos: 2 }], members: ['777'], everyonePerms: bp('ViewChannel') } },
  full: { mode: 'full', guild: { botPerms: BOT_PERMS, roles: [
    { id: '101', name: 'Admins', color: 0, perms: bp('ViewChannel'), pos: 4 }, { id: '102', name: 'Mod', mentionable: true, perms: MOD_PERMS, pos: 5 },
    { id: '103', name: 'Member', perms: MEMBER_PERMS, pos: 3 }, { id: '104', name: 'SomeBot', managed: true, pos: 7 }, { id: '150', name: 'Newcomer', pos: 6 },
    { id: '160', name: 'Boss', pos: 12 }, { id: '999', name: 'Bot', pos: 10 }],
  channels: [
    { id: '201', type: 4, name: 'Information', pos: 3 },
    { id: '202', name: 'chat', parent: null, pos: 5, topic: 'old', slow: 0, overwrites: [{ id: '102', allow: '0', deny: '0' }, { id: '888', type: 1, deny: bp('ViewChannel') }] },
    { id: '203', type: 2, name: 'Lounge', parent: '201', pos: 1, bitrate: 64000, limit: 5 },
    { id: '206', type: 15, name: 'help', pos: 9 },
  ], members: ['777', '778'], everyonePerms: bp('ViewChannel', 'SendMessages') } },
  empty: { guild: { botPerms: BOT_PERMS, roles: [{ id: '999', name: 'Bot', pos: 10 }], members: ['777', '778'] } },
  limited: { guild: { botPerms: BOT_PERMS, roles: [{ id: '999', name: 'Bot', pos: 5 }], botTop: 5 } },
  admin: { guild: { botPerms: bp('Administrator'), roles: [{ id: '999', name: 'Bot', pos: 10 }], members: ['777'] } },
  noperms: { guild: { botPerms: bp('ViewChannel', 'SendMessages'), roles: [{ id: '999', name: 'Bot', pos: 10 }] } },
  norolesperm: { guild: { botPerms: bp('ManageChannels', 'ViewChannel'), roles: [{ id: '999', name: 'Bot', pos: 10 }] } },
  errors: { guild: { botPerms: BOT_PERMS, roles: [{ id: '999', name: 'Bot', pos: 10 }], fail: { roleCreate: true, channelCreate: true } } },
  recreated: { guild: { botPerms: BOT_PERMS, roles: [{ id: '101', name: 'Admin', pos: 8 }, { id: '103', name: 'Member', pos: 3 }, { id: '104', name: 'SomeBot', managed: true, pos: 2 }, { id: '999', name: 'Bot', pos: 10 }],
    channels: [
      { id: '201', type: 4, name: 'Info', pos: 0 },
      { id: '202', name: 'general', parent: '201', pos: 0, overwrites: [{ id: '104', allow: bp('SendMessages') }] },
      { id: '203', type: 2, name: 'Lounge', parent: '201', pos: 1 },
      { id: '206', type: 15, name: 'help', pos: 2 },
    ], members: ['777', '778'] } },
  fullAgain: { mode: 'full', guild: { botPerms: BOT_PERMS, roles: [{ id: '101', name: 'Admin', color: 0xff0000, hoist: true, perms: bp('ManageRoles'), pos: 8 }, { id: '102', name: 'Mod', mentionable: true, perms: MOD_PERMS, pos: 6 },
    { id: '103', name: 'Member', perms: MEMBER_PERMS, pos: 3 }, { id: '104', name: 'SomeBot', managed: true, pos: 2 }, { id: '999', name: 'Bot', pos: 10 }],
  channels: ORIGINAL.channels.filter((c) => !c.thread), everyonePerms: EVERYONE_PERMS, members: ['777', '778'] } },
};

const restores = {};
for (const [name, sc] of Object.entries(SCENARIOS)) {
  resetIds();
  const guild = makeGuild({ ...sc.guild });
  const previews = {
    missing: J(snap.previewRestore({ guild, snapshot: SNAPSHOT, mode: 'missing' })),
    full: J(snap.previewRestore({ guild, snapshot: SNAPSHOT, mode: 'full' })),
  };
  const { state, batches } = await restoreAll(guild, { mode: sc.mode ?? 'missing' });
  restores[name] = {
    mode: sc.mode ?? 'missing',
    spec: sc.guild,
    previews,
    state: J({ ...state, startedAt: 0 }),
    batches,
    log: J({
      create: guild.log.create.map(([kind, d]) => [kind, { ...d, permissionOverwrites: d.permissionOverwrites?.map((o) => ({ ...o })) }]),
      edit: guild.log.edit,
      put: guild.log.put.map((p) => ({ route: p.route, body: p.body, reason: p.reason })),
      rolePositions: guild.log.rolePositions,
      channelPositions: guild.log.channelPositions,
      fetches: guild.log.fetches,
    }),
    after: guildState(guild),
    summary: J(svc.buildSummaryEmbed(state).toJSON()),
    summaryStopped: J(svc.buildSummaryEmbed(state, { stopped: 'The backup was deleted, so the restore stopped.' }).toJSON()),
    previewEmbed: J(svc.buildPreviewEmbed({ name: 'Nightly', createdAt: new Date('2026-10-21T10:00:00Z'), automatic: false }, previews[sc.mode ?? 'missing']).toJSON()),
  };
}

// ── Parsing ──
const MESSY = [
  null, [], 'x', { version: 2 }, { version: '1' }, { version: 1.0 }, {},
  { ...roundTrip(SNAPSHOT), roles: [...roundTrip(SNAPSHOT).roles, { id: 'nope', name: 'bad' }, null, { id: '5', name: '', color: 1.5, permissions: 99, position: 3.0, unicodeEmoji: '' }],
    channels: [{ id: '300', type: 0, name: 'x', overwrites: [{ id: '5', type: 'role', allow: '3', deny: '7' }, { id: 'zz' }, { id: '6', type: 'member', allow: 8, deny: '123456789012345678901' }] },
      { id: '301', type: 11, name: 'thread' }, { id: '302', type: 2, name: 'v', bitrate: 7999, userLimit: 100, rateLimitPerUser: 5 },
      { id: '303', type: 15, name: '', topic: '', tags: [{ name: 'a'.repeat(30), emojiName: '' }, { name: '' }, null, { name: 't', moderated: true, emojiName: '🔥' }], defaultAutoArchiveDuration: 61 }],
    categories: [{ id: '400', type: 0, name: 'notcat' }, { id: '401', type: 4, name: 'cat', position: 2 }],
    idMap: { 101: '999', abc: '1', 102: 'x', 9: '8' }, skipped: { channels: -1 }, guild: { id: 900, name: 'n'.repeat(150) }, everyone: { permissions: [5] } },
  { version: 1, guild: null, everyone: null, takenAt: 5 },
];
const parse = MESSY.map((raw) => {
  try { return { ok: J(snap.parseSnapshot(raw)) }; } catch (err) { return { error: err.message }; }
});

const PAYLOADS = [
  null, 'x', {}, { backupId: '', mode: 'missing', phase: 'roles' }, { backupId: 'b', mode: 'bad', phase: 'roles' }, { backupId: 'b', mode: 'full', phase: 'nope' },
  { backupId: 'b', mode: 'full', phase: 'channels', cursor: 3.0, stalls: -1, retry: 'yes', idMap: { 1: '2', x: '3' }, made: ['1', 2, 'a', '33'],
    counts: { rolesCreated: 2, bogus: 3, errors: 1.5 }, errors: ['e', 3, 'f'], dropped: 'x', startedById: 5, channelId: 'C', startedAt: 1234 },
  { backupId: 'b', mode: 'missing', phase: 'finish', retry: true, startedAt: -5, dropped: '8' },
];
const normalise = PAYLOADS.map((p) => {
  const s = snap.normaliseRestoreState(p);
  return s ? { ...s, startedAt: p?.startedAt === 1234 ? s.startedAt : 'now' } : null;
});

const MASK = [['8', '3', '0'], ['8', '3', '8'], ['0', '3', '12'], ['6', '2', '1'], ['123456789', '987654321', '55555']].map(([w, b, c]) => ({ in: [w, b, c], out: J(snap.maskBits(B(w), B(b), B(c))) }));
const BITS = ['12', 'x', '', '00012', '123456789012345678901', '18446744073709551616', 12, -1, 1.5, 9007199254740993, null, true, [5], ['7'], {}].map((v) => ({ in: v === undefined ? null : v, out: String(snap.bits(v)) }));

const NAMES = ['  manual\n0 ', 'a\u0000\u0001b', '\t\t', '', 'x'.repeat(80), 'é  ü z', '😀'.repeat(40), 'line1\r\nline2', '\u007fdel'];

const rows = [
  { id: 'cm1abc', name: 'Nightly', automatic: false, createdAt: new Date('2026-10-21T14:05:33Z'), roleCount: 3, channelCount: 1234 },
  { id: 'cm2abc', name: null, automatic: true, createdAt: new Date('2026-01-02T03:04:05Z'), roleCount: 0, channelCount: 0 },
  { id: 'cm3abc', name: 'Named auto', automatic: true, createdAt: new Date('2026-01-02T03:04:05Z'), roleCount: 1, channelCount: 2 },
  { id: 'cm4abc', name: '', automatic: false, createdAt: new Date('2026-01-02T03:04:05Z'), roleCount: 1, channelCount: 2 },
];
const labels = rows.map((r) => svc.backupLabel(r));
const listEmbed = J(svc.buildListEmbed(null, rows).toJSON());
const listEmpty = J(svc.buildListEmbed(null, []).toJSON());
const infoEmbed = J(svc.buildInfoEmbed(null, { ...rows[0], sizeBytes: 12345, createdById: 'U1' }, snap.parseSnapshot(roundTrip(SNAPSHOT))).toJSON());

const liveState = snap.newRestoreState({ backupId: 'bk1', mode: 'full', startedById: 'OWNER', channelId: 'C', now: 1_700_000_000_000 });
liveState.counts = { rolesCreated: 1234, categoriesCreated: 1, channelsCreated: 2 };
liveState.phase = 'channelPositions';
const status = {
  none: svc.restoreStatusText(null),
  live: svc.restoreStatusText({ status: 'RUNNING', payload: liveState }),
  liveMissing: svc.restoreStatusText({ status: 'PENDING', payload: { ...liveState, mode: 'missing', startedById: null, phase: 'finish' } }),
  failed: svc.restoreStatusText({ status: 'FAILED', payload: liveState, lastError: 'boom '.repeat(60) }),
  failedNull: svc.restoreStatusText({ status: 'FAILED', payload: liveState, lastError: null }),
  done: svc.restoreStatusText({ status: 'DONE', payload: liveState }),
};
const statusEmbeds = [
  { settings: { backupsEnabled: true, backupIntervalHours: 0, backupKeep: 7 }, counts: { manual: 1, automatic: 2 }, newest: null, restoreJob: null, problems: [] },
  { settings: { backupsEnabled: false, backupIntervalHours: 1, backupKeep: 99 }, counts: { manual: 10, automatic: 0 }, newest: null, restoreJob: null, problems: ['a', 'b'] },
  { settings: { backupsEnabled: true, backupIntervalHours: 12, backupKeep: 3 }, counts: { manual: 0, automatic: 1234 }, newest: { createdAt: new Date('2020-01-01T00:00:00Z') }, restoreJob: null, problems: [] },
].map((input) => ({ input: J(input), embed: J(svc.buildStatusEmbed(input).toJSON()) }));

// ── /emoji ──
const SOURCES = [
  '', '   ', '<:blob:123456789012345678>', '<a:dance_2:123456789012345678>', '<:x:12345>', '<https://example.com/a.png>', 'https://cdn.discordapp.com/emojis/123456789012345678.gif',
  'https://cdn.discordapp.com/emojis/123456789012345678.png?name=hello%20world&animated=true', 'https://media.discordapp.net/emojis/123456789012345678',
  'https://cdn.discordapp.com/emojis/123456789012345678.svg', 'HTTPS://Example.com/Path/My%20Cool-Pic.PNG?x=1', 'https://example.com/12345.png', 'https://example.com/',
  'https://exa mple.com/x.png', 'http://[::1]/x.png', 'https://example.com/%E0%A4%A.png', 'https://example.com/a.png?name=good_name', 'https://example.com/caf%C3%A9.gif',
  '123456789012345678', ':blob:', ':blob~2:', 'blob', '😀', '🇫🇷', '©', '#', 'hello 😀', ':a:', '<:blob:123456789012345678> extra', 'https://example.com/a+b.png?name=a+b',
];
const parsed = SOURCES.map((s) => ({ in: s, out: emoji.parseEmojiSource(s) }));
const SANITIZE = ['café', '  hi there ', 'a.b-c d', 'ÅngströmÆ', '😀x', 'ﬁ', '①②', 'x'.repeat(40), 'a__b', '', 'Ⅻ', 'ß', 'İ'];
const sanitize = SANITIZE.map((s) => ({ in: s, out: emoji.sanitizeEmojiName(s) }));
const URLS = ['https://e.com/a/b/cool_pic.png', 'https://e.com/123.png', 'https://e.com/', 'https://e.com/x.png?name=ok_name', 'https://e.com/%ZZ.png', 'https://e.com/a.toolongext', 'https://e.com/a', 'notaurl', 'https://e.com/caf%C3%A9-%F0%9F%98%80.png', 'https://e.com/a.b.c.png'];
const nameUrls = URLS.map((u) => ({ in: u, out: emoji.nameFromUrl(u) }));
const resolveNames = [
  { given: 'my emoji', source: { kind: 'url', nameHint: null } }, { given: 'ok_name', source: null }, { given: 'x', source: null }, { given: '  spaced  ', source: null },
  { given: null, source: { kind: 'custom', name: 'blob' } }, { given: null, source: { kind: 'custom', name: null } }, { given: null, source: { kind: 'url', nameHint: '12345' } },
  { given: null, source: { kind: 'url', nameHint: 'pic' } }, { given: 'é', source: null },
].map((a) => ({ in: a, out: emoji.resolveEmojiName(a) }));
const taken = new Set(['blob', 'blob_2', ...Array.from({ length: 98 }, (_, i) => `full_${i + 2}`), 'full', 'x'.repeat(32).toLowerCase()]);
const freeNames = ['blob', 'new', 'full', 'x'.repeat(32), 'BLOB'].map((b) => ({ in: b, out: emoji.pickFreeName(b, taken) }));
const EMOJIS = [
  { id: '111111111111111111', name: 'blob', animated: false, managed: false, available: true, roles: { cache: { size: 0 } } },
  { id: '222222222222222222', name: 'Blob', animated: true, managed: false, available: true, roles: { cache: { size: 2 } } },
  { id: '333333333333333333', name: 'party_blob', animated: false, managed: true, available: false, roles: { cache: { size: 1 } } },
  { id: '444444444444444444', name: 'zeta', animated: false, managed: false, available: true, roles: { cache: { size: 0 } } },
  { id: '555555555555555555', name: 'alpha_blob', animated: true, managed: false, available: true, roles: { cache: { size: 0 } } },
  { id: '666666666666666666', name: 'dup', animated: false, managed: false, available: true, roles: { cache: { size: 0 } } },
  { id: '777777777777777777', name: 'dup', animated: false, managed: false, available: true, roles: { cache: { size: 0 } } },
];
const QUERIES = ['', 'blob', 'BLOB', ':blob:', 'Blob', '<:x:222222222222222222>', '<:x:999999999999999999>', '111111111111111111', '999999999999999999', 'dup', 'nothing', 'DUP', ':zeta'];
const resolved = QUERIES.map((q) => ({ in: q, out: (() => { const r = emoji.resolveGuildEmoji(EMOJIS, q); return r.ok ? { ok: true, id: r.emoji.id } : r; })() }));
const choices = ['', 'blob', 'b-l', '2222222222222222222', 'id 333333333333333333', 'ZE', 'a'].map((q) => ({ in: q, out: emoji.emojiChoices(EMOJIS, q) }));
const slots = [[0, []], [1, []], [2, ['MORE_EMOJI']], [3, ['MORE_EMOJI']], [7, []], [0, ['MORE_EMOJI']]].map(([t, f]) => ({ in: [t, f], limit: emoji.emojiSlotLimit(t, f), slots: emoji.countEmojiSlots(EMOJIS, { premiumTier: t, features: f }) }));
const pages = emoji.buildListPages(EMOJIS, emoji.countEmojiSlots(EMOJIS, { premiumTier: 1 }), { perPage: 3 });
const lines = EMOJIS.map((e) => emoji.formatEmojiLine(e));
const sniffInputs = [
  [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0], [0xff, 0xd8, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0], [...Buffer.from('GIF89a'), 0, 0, 0, 0, 0, 0],
  [...Buffer.from('RIFF'), 0, 0, 0, 0, ...Buffer.from('WEBP'), ...Buffer.from('VP8X'), 0, 0, 0, 0, 2], [...Buffer.from('RIFF'), 0, 0, 0, 0, ...Buffer.from('WEBP'), ...Buffer.from('VP8X'), 0, 0, 0, 0, 0],
  [...Buffer.from('RIFF'), 0, 0, 0, 0, ...Buffer.from('WEBP'), ...Buffer.from('VP8L'), 0, 0, 0, 0, 2], [...Buffer.from('<svg xmlns=')], [1, 2, 3],
].map((b) => ({ in: b, out: emoji.sniffImage(Buffer.from(b)) }));
const fetchErrs = [
  ['INVALID_URL', 'Only http and https links are allowed.'], ['BLOCKED_ADDRESS'], ['DNS_FAILED'], ['TIMEOUT'], ['TOO_LARGE'], ['TOO_MANY_REDIRECTS'], ['HTTP_ERROR', 404], ['HTTP_ERROR', 500], ['HTTP_ERROR'],
  ['BAD_CONTENT_TYPE'], ['BAD_IMAGE'], ['NETWORK'],
].flatMap(([code, extra]) => [false, true].map((fromCdn) => {
  const err = code === 'HTTP_ERROR' ? new SafeFetchError(code, extra ?? undefined) : new SafeFetchError(code, extra);
  if (code === 'HTTP_ERROR') err.status = extra ?? undefined;
  return { code, message: err.message, status: err.status ?? null, fromCdn, out: emoji.describeFetchError(err, { fromCdn }) };
}));
const emojiErrs = [30008, 30018, 50013, 50001, 50045, 50046, 50138, 50035, 10014, 99999, null].flatMap((code) => ['add', 'delete'].map((action) => ({
  code, action, message: code === 50035 ? 'Invalid Form Body x'.repeat(30) : code === null ? '' : 'Some error',
  out: emoji.describeEmojiError(Object.assign(new Error(code === 50035 ? 'Invalid Form Body x'.repeat(30) : code === null ? '' : 'Some error'), { code }), { action }),
})));
const timeouts = ['add', 'delete'].map((action) => emoji.describeEmojiError(new TimeoutError('emoji create'), { action }));

const out = {
  snapshot: SNAPSHOT,
  snapshotBytes: snap.snapshotBytes(SNAPSHOT),
  snapshotBytesTricky: (() => { const s = roundTrip(SNAPSHOT); s.guild.name = 'é\u0001"\\\n😀 '; return { data: s, bytes: snap.snapshotBytes(s) }; })(),
  original: ORIGINAL,
  stats: snap.snapshotStats(SNAPSHOT),
  restores,
  parse,
  messy: MESSY,
  payloads: PAYLOADS,
  normalise,
  newState: snap.newRestoreState({ backupId: 'bk1', mode: 'missing', startedById: null, channelId: 'C', idMap: { 1: '2', x: 'y' }, now: 5 }),
  mask: MASK,
  bits: BITS,
  allBits: String(PermissionsBitField.All),
  bitNames: ['8', '1099511627775', '0', '268435456'].map((v) => ({ in: v, out: svc.bitNames(B(v)) })),
  cleanName: NAMES.map((n) => ({ in: n, out: svc.cleanName(n) })),
  stamps: ['2026-10-21T14:05:33.123Z', '2026-01-02T03:04:05Z'].map((d) => ({ in: d, out: svc.stampOf(new Date(d)) })),
  rows: J(rows),
  labels,
  listEmbed,
  listEmpty,
  infoEmbed,
  status,
  statusEmbeds,
  missingPerms: [['Manage Roles'], ['Manage Channels'], ['Manage Roles', 'Manage Channels']].map((m) => ({ in: m, out: J(svc.missingPermissionsEmbed(m).toJSON()) })),
  ownerOnly: J(svc.ownerOnlyEmbed().toJSON()),
  customIds: { restore: buildCustomId('backup', 'restore', 'cm0abcdefghijkl', 'full'), cancel: buildCustomId('backup', 'cancel') },
  keys: { auto: svc.autoJobKey('123'), restore: svc.restoreJobKey('123') },
  buttons: J(svc.restoreButtons('cmabc', 'missing').toJSON()),
  emoji: { parsed, sanitize, nameUrls, resolveNames, freeNames, emojis: EMOJIS, resolved, choices, slots, pages, lines, sniff: sniffInputs, fetchErrs, emojiErrs, timeouts,
    cdn: [emoji.emojiCdnUrl('1', false), emoji.emojiCdnUrl('1', true, { size: 96 })] },
};
const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-backups');
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, 'golden.json'), `${JSON.stringify(J(out), null, 1)}\n`);
process.stdout.write('wrote rust/fixtures/golden/feat-backups/golden.json\n');
process.exit(0);
