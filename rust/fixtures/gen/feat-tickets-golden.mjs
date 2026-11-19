// Reference data for feat-tickets, produced by the JS ticket service itself (src/services/ticketService.js).
// Writes rust/fixtures/golden/feat-tickets/pure.json. Run from the repository root:
//   node rust/fixtures/gen/feat-tickets-golden.mjs
// Nothing here touches Discord, Postgres or Redis: the pure helpers run on plain objects, as test/tickets.test.js does.
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL ??= 'postgresql://test:test@127.0.0.1:1/test';
const quiet = () => {};
console.error = console.warn = console.log = console.debug = console.info = quiet;

const svc = await import('../../../src/services/ticketService.js');
const { buildCustomId } = await import('../../../src/config/customIds.js');
const { default: redis } = await import('../../../src/services/redis.js');

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-tickets');

// An embed as JSON without its timestamp (it is the time of the run).
const embedJson = (e) => {
  const j = JSON.parse(JSON.stringify(e.toJSON ? e.toJSON() : e.data ?? e));
  delete j.timestamp;
  return j;
};
const payloadJson = (p) => ({
  embeds: (p.embeds ?? []).map(embedJson),
  components: (p.components ?? []).map((r) => (r.toJSON ? r.toJSON() : r)),
  allowedMentions: p.allowedMentions ?? null,
});

// ── Error payloads ────────────────────────────────────────────────────────────
const errorCases = [
  ['PANEL_GONE'], ['OPTION_GONE'],
  ['OPEN_LIMIT', { ids: ['800000000000000001'], max: 1 }],
  ['OPEN_LIMIT', { ids: ['800000000000000001', 'pending:9'], max: 3 }],
  ['BOT_PERMS', { names: 'Manage Channels, Manage Roles', where: null }],
  ['BOT_PERMS', { names: 'Manage Roles', where: 'Tickets' }],
  ['BOT_PERMS', { names: 'Send Messages', channelId: '800000000000000002' }],
  ['GUILD_FULL'], ['CREATE_FAILED', { reason: 'Discord rejected the request' }], ['SLOW'], ['THROTTLED'], ['MODULE_OFF'], ['DB_ERROR'],
  ['NOT_TICKET'], ['TICKET_NOT_FOUND'], ['TICKET_DELETED'], ['NOT_STAFF'], ['NOT_OWNER_OR_STAFF'], ['OWN_TICKET'], ['ALREADY_CLAIMED_SELF'],
  ['ALREADY_CLAIMED', { claimedById: '800000000000000003' }], ['IS_CLOSED'], ['ALREADY_CLOSED'], ['NOT_CLOSED'], ['CLOSE_FIRST'], ['CONFLICT'],
  ['DELETING'], ['RENAME_LIMIT', { retryAt: new Date(Date.UTC(2026, 0, 2, 3, 4, 5)) }], ['BAD_NAME'], ['ADD_BOT'], ['NOT_MEMBER'],
  ['IS_OWNER', { op: 'remove' }], ['IS_OWNER', { op: 'add' }], ['ALREADY_ADDED', { userId: '800000000000000004' }], ['ADDED_LIMIT'],
  ['NOT_ADDED', { userId: '800000000000000004' }], ['TRANSCRIPT_THROTTLED'], ['TRANSCRIPT_FAILED', { reason: 'missing Read Message History' }],
  ['TRANSCRIPT_CHANNEL', { channelId: '800000000000000005', reason: 'missing Attach Files' }], ['TRANSCRIPT_ABORT', { reason: 'the channel no longer exists' }],
  ['DELETE_FAILED', { reason: "I'm missing permissions there", transcriptSaved: true }], ['DELETE_FAILED', { reason: 'x', transcriptSaved: false }],
  ['UNKNOWN'], ['SOMETHING_ELSE'],
];
const errors = errorCases.map(([code, data]) => ({
  code,
  data: data ? JSON.parse(JSON.stringify({ ...data, ...(data.retryAt ? { retryAt: data.retryAt.getTime() } : {}) })) : null,
  payload: payloadJson(svc.errorPayload({ code, data })),
}));

const u = (missing, reason) => ({ ok: false, missing, reason });
const panelCases = [
  ['NAME_TAKEN', { name: 'Support' }], ['PANEL_LIMIT'], ['PANEL_NOT_FOUND'], ['BAD_FORMAT', { error: 'The name format must be 1–90 characters.' }], ['BAD_PANEL_NAME'],
  ['CHANNEL_UNUSABLE', { u: u(['SendMessages', 'EmbedLinks'], 'x'), id: '800000000000000006' }],
  ['CHANNEL_UNUSABLE', { u: u([], 'Channel not found'), id: '800000000000000006' }],
  ['TRANSCRIPT_CHANNEL_UNUSABLE', { u: u(['AttachFiles'], 'x'), id: '800000000000000007' }],
  ['PERMS_SERVER', { missing: ['ManageChannels', 'ManageRoles'] }], ['PUBLISH_FAILED', { reason: 'the panel was deleted' }], ['BAD_LABEL'], ['OPTION_LIMIT'],
  ['OPTION_DUP', { label: 'Billing' }], ['BAD_EMOJI', { error: '"x" is not a single emoji.' }], ['OPTION_NOT_FOUND'], ['LAST_OPTION'], ['EVERYONE_ROLE'],
  ['ALREADY_MANAGER', { roleId: '800000000000000008', panel: 'Support' }], ['NOT_MANAGER', { roleId: '800000000000000008', panel: 'Support' }],
  ['MANAGER_LIMIT'], ['CATEGORY_PERMS', { names: 'Manage Roles', category: 'Tickets' }], ['CONFLICT_CHANNEL'], ['CONFLICT_CATEGORY'],
  ['INVALID_MESSAGE', { errors: ['a', 'b'] }], ['BAD_COMBO'], ['NOT_ADMIN'], ['FORM_EXPIRED'], ['PANEL_DELETED'], ['SLOW'], ['WHATEVER'],
];
const panelErrors = panelCases.map(([code, d]) => ({
  code,
  data: d ? (d.u ? { missing: d.u.missing, reason: d.u.reason, id: d.id } : d) : null,
  payload: payloadJson(svc.panelErrorPayload(code, d)),
}));

const refreshNotes = [null, { ok: true }, { ok: false, code: 'GONE' }, { ok: false, code: 'NO_ACCESS', channelId: '800000000000000009' }, { ok: false, code: 'ERROR', reason: 'x' }, { ok: false, code: 'ERROR' }]
  .map((r) => ({ r, note: svc.refreshNote(r) }));

// ── Notices ───────────────────────────────────────────────────────────────────
const settings = { embedColor: 0x123456 };
const T = 'ckticket00000000000000001';
const noticeCases = [
  ['claim', { actorId: '11', previousId: null }], ['claim', { actorId: '11', previousId: '12' }],
  ['close', { actorId: '11', ticketId: T, reason: null }], ['close', { actorId: '11', ticketId: T, reason: 'done here' }],
  ['reopen', { actorId: '11', ticketId: T, claimed: false }], ['reopen', { actorId: '11', ticketId: T, claimed: true }],
  ['add', { actorId: '11', userId: '800000000000000013' }], ['remove', { actorId: '11', userId: '800000000000000013' }],
  ['rename', { actorId: '11', channelId: '800000000000000014' }],
  ['delete', { actorId: '11', transcriptChannelId: null, dm: false }], ['delete', { actorId: '11', transcriptChannelId: '800000000000000015', dm: true }],
];
const notices = noticeCases.map(([kind, d]) => ({ kind, d, payload: payloadJson(svc.buildNotice(kind, d, settings)) }));

// ── Components ────────────────────────────────────────────────────────────────
const opt = (id, position, over = {}) => ({ id, label: `Option ${id}`, emoji: null, description: null, buttonStyle: 1, position, categoryId: null, introTemplate: null, panelId: 'p1', ...over });
const options = [
  opt('o3', 1, { emoji: '✉️', buttonStyle: 3 }),
  opt('o1', 0, { emoji: '<:pog:111111111111111111>', description: 'first one' }),
  opt('o2', 0, { emoji: '<a:gone:222222222222222222>', buttonStyle: 9 }),
  opt('o4', 2, { label: 'L'.repeat(120), description: 'D'.repeat(150) }),
  ...Array.from({ length: 8 }, (_, i) => opt(`x${i}`, 10 + i)),
];
const canUseEmoji = (e) => e.includes('111111111111111111');
const components = {
  options,
  buttons: svc.buildPanelComponents({ id: 'p1', style: 'BUTTONS' }, options, { canUseEmoji }).map((r) => r.toJSON()),
  select: svc.buildPanelComponents({ id: 'p1', style: 'SELECT' }, options, { canUseEmoji }).map((r) => r.toJSON()),
  empty: svc.buildPanelComponents({ id: 'p1', style: 'BUTTONS' }, []).map((r) => r.toJSON()),
  introOpen: svc.buildIntroRow(T).toJSON(),
  introClaimed: svc.buildIntroRow(T, { claimed: true }).toJSON(),
  closed: svc.buildClosedRow(T).toJSON(),
};
const rows = [svc.buildIntroRow(T).toJSON(), { type: 1, components: [{ type: 2, style: 5, label: 'Link', url: 'https://example.com' }] }];
components.disabledAll = svc.disableButtons(rows);
components.disabledClaim = svc.disableButtons(rows, (c) => String(c.custom_id ?? '').startsWith('ticket:claim:'));
components.reasonModal = svc.buildReasonModal('p'.repeat(25), 'o'.repeat(25), 'x'.repeat(80)).toJSON();
components.reasonModalShort = svc.buildReasonModal('p1', 'o1', 'Billing').toJSON();

const customIds = {
  open: buildCustomId('ticket', 'open', 'p1', 'o1'),
  close: buildCustomId('ticket', 'close', T),
  select: buildCustomId('ticket-select', 'p1'),
  reason: buildCustomId('ticket-modal', 'p1', 'o1'),
  tplPanel: buildCustomId('ticket-modal', 'tpl', 'p1', 'panel'),
  tplIntro: buildCustomId('ticket-modal', 'tpl', 'p1', 'intro'),
  tplIntroOption: buildCustomId('ticket-modal', 'tpl', 'p1', 'intro', 'o1'),
};

const overwrites = [
  { guildId: 'g', botId: 'b', ownerId: 'o', managerRoleIds: ['r1', 'r2'], addedUserIds: ['a1', 'o'], closed: false },
  { guildId: 'g', botId: 'b', ownerId: 'o', managerRoleIds: [], addedUserIds: ['a1'], closed: true },
  { guildId: 'g', botId: 'g', ownerId: 'o', managerRoleIds: ['r1', 'r1'] },
].map((i) => ({ input: { managerRoleIds: [], addedUserIds: [], closed: false, ...i }, out: svc.buildTicketOverwrites(i) }));

// ── Manager sync payloads ─────────────────────────────────────────────────────
const R = (n) => `9${String(n).padStart(17, '0')}`;
const syncCases = [
  [null, { panels: { p1: { roles: [R(1), R(2)] } } }],
  [{ panels: { p1: { roles: [R(1)], done: 7, rev: 2 } } }, { panels: { p1: { roles: [R(2), R(1)] }, p2: { roles: [R(3)] } } }],
  [{ panels: { 'bad id!': { roles: [R(1)] }, p3: { roles: ['nope', 5, R(4), R(4)], done: -1, rev: 1.5 } } }, { panels: { p3: { roles: [] } } }],
  [{ panels: { p1: { roles: [R(1)], done: 3.0, rev: 4 } } }, {}],
  ['junk', { panels: 'nope' }],
  [{ panels: { p9: { roles: Array.from({ length: 300 }, (_, i) => R(i + 1)) } } }, { panels: { p9: { roles: [R(500)] } } }],
];
const merges = syncCases.map(([a, b]) => ({ existing: a, incoming: b, out: svc.mergeManagerSync(a, b) }));
const syncPayload = svc.managerSyncPayload('p1', [R(1), 'x', R(1)]);

// ── Discord errors ────────────────────────────────────────────────────────────
const errorObjs = [
  { name: 'TimeoutError' }, { code: 50035, rawError: { errors: { parent_id: { _errors: [{ code: 'CHANNEL_PARENT_MAX_CHANNELS' }] } } } },
  { code: 50013 }, { code: 50001 }, { code: 10003 }, { code: 10008 }, { code: 30013 }, { code: 50035 }, { code: 50007 }, { status: 429 },
  { code: 12345 }, { status: 500 }, {},
];
const discordErrors = errorObjs.map((e) => ({
  input: { code: e.code ?? null, status: e.status ?? null, timeout: e.name === 'TimeoutError', errorCodes: e.rawError ? ['CHANNEL_PARENT_MAX_CHANNELS'] : [] },
  text: svc.describeDiscordError(e),
  categoryFull: svc.isCategoryFullError(e),
}));

// ── Names, numbers and emoji ──────────────────────────────────────────────────
const nameInputs = [
  'Hello World', '  --Spaces  and   Tabs\t--  ', 'ＦＵＬＬ　ｗｉｄｔｈ', 'Привет мир', '你好 世界', 'emoji 🎫 ticket', 'a_b-c__d---e', '___', '',
  'ÀÉÎÕÜ ñ ß', 'x'.repeat(130), "it's (a) [test]!", 'İstanbul ΣΑΣ', '١٢٣ arabic digits', '－－dash－－', 'tab\nnew\rline',
];
const sanitized = nameInputs.map((s) => ({ input: s, out: svc.sanitizeChannelName(s) }));
const formatInputs = [
  'ticket-{number}', '{user.name}-{number}', '{USER.ID}', '{ user.display_name }-{topic}', '{bogus}', '{}', '', '   ', 'x'.repeat(91), '🎫🎫',
  '{number', 'a{b{number}c}', '{user.username}', '{{number}}',
];
const formats = formatInputs.map((f) => ({ input: f, out: svc.validateNameFormat(f) }));
const user = { id: '800000000000000020', username: 'Bob.Smith', globalName: 'Bobby Tables', tag: 'Bob.Smith', bot: false };
const member = { displayName: 'Bobster ✨', user };
const renderCases = [
  ['ticket-{number}', 1, 'Billing'], ['{user.name}-{number}', 12, null], ['{user.display_name}-{topic}', 123, 'Bug Report'],
  ['{user.id}', 12345, null], ['🎫', 7, null], ['{topic}', 3, ''],
];
const rendered = renderCases.map(([fmt, n, topic]) => ({ format: fmt, number: n, topic, out: svc.renderChannelName(fmt, { number: n, user, member, topic: topic ?? undefined }) }));
const numbers = [0, 1, 12, 999, 1000, 9999, 10000, 123456].map((n) => ({ n, out: svc.formatTicketNumber(n) }));
const panelNames = ['Support', '  Trim me  ', '', 'x'.repeat(50), 'x'.repeat(51), 'two\nlines', '🎫'.repeat(25), '🎫'.repeat(26)].map((s) => ({ input: s, out: svc.normalizePanelName(s) }));

const emojiClient = { emojis: { cache: new Map([['111111111111111111', {}]]) } };
const pog = { id: '333333333333333333', name: 'Pog', animated: false, toString: () => '<:Pog:333333333333333333>' };
const dance = { id: '444444444444444444', name: 'dance', animated: true, toString: () => '<a:dance:444444444444444444>' };
const emojiGuild = { emojis: { cache: new Map([[pog.id, pog], [dance.id, dance]]) } };
const emojiInputs = [
  '', '  ', 'none', 'NONE', '✉️', '🎫', '👍🏽', '🇺🇸', '#️⃣', '1️⃣', '👨‍👩‍👧', '🏴󠁧󠁢󠁳󠁣󠁴󠁿', '❤', '❤️', '✉', 'ab', '🎫🎫', 'x',
  '<:pog:111111111111111111>', '<a:pog:111111111111111111>', '<:nope:555555555555555555>', ':pog:', ':DANCE:', ':missing:', ':a:',
  'a very long text that is not an emoji at all and goes on', '©', '©️', '™️', '🧑‍💻', '🏳️‍🌈', '*️⃣', '🫠',
];
const emojis = emojiInputs.map((s) => ({ input: s, out: svc.parseOptionEmoji(s, { client: emojiClient, guild: emojiGuild }), unicode: svc.isUnicodeEmoji(s) }));

// ── Rows, rename window, transitions ──────────────────────────────────────────
const now = Date.UTC(2026, 0, 2, 3, 4, 5);
const classifyRows = [
  { id: 'a', channelId: '1', createdAt: now - 1000 }, { id: 'b', channelId: 'pending:x', createdAt: now - 1000 },
  { id: 'c', channelId: 'pending:y', createdAt: now - 6 * 60_000 }, { id: 'd', channelId: '2', createdAt: now }, { id: 'e', channelId: 'pending:z', createdAt: now - 5 * 60_000 },
];
const classify = svc.classifyTicketRows(classifyRows, (id) => id === '1', now);
const renameCases = [[[], now], [[now - 1000], now], [[now - 1000, now - 2000], now], [[now - 11 * 60_000, now - 1000], now], [[now - 9 * 60_000, now - 1000, now - 500], now]]
  .map(([ts, n]) => {
    const r = svc.renameWindowCheck(ts, n);
    return { timestamps: ts, now: n, allowed: r.allowed, retryAt: r.retryAt ? r.retryAt.getTime() : null };
  });
const transitions = [];
for (const action of ['claim', 'close', 'reopen', 'delete', 'add', 'remove', 'rename', 'transcript', 'bogus']) {
  for (const status of ['OPEN', 'CLAIMED', 'CLOSED']) {
    for (const claimedById of [null, 'a1', 'a2']) {
      for (const isAdmin of [false, true]) {
        for (const source of ['button', 'command']) {
          transitions.push({ action, status, claimedById, isAdmin, source, out: svc.transitionFor(action, { status, claimedById }, { actorId: 'a1', isAdmin, source }) });
        }
      }
    }
  }
}
const perms = [];
for (const action of ['claim', 'close', 'reopen', 'delete', 'add', 'remove', 'transcript', 'rename']) {
  for (const isOwner of [false, true]) for (const isStaff of [false, true]) perms.push({ action, isOwner, isStaff, out: svc.canPerform(action, { isOwner, isStaff }) });
}

// ── Templates ─────────────────────────────────────────────────────────────────
const templates = {
  panelButtons: svc.defaultPanelTemplate('BUTTONS'),
  panelSelect: svc.defaultPanelTemplate('SELECT'),
  intro: JSON.parse(JSON.stringify(svc.DEFAULT_INTRO_TEMPLATE)),
  uses: [
    [{ content: 'hi {Managers}' }, 'managers'], [{ embed: { description: 'ping { managers } now' } }, 'managers'], [{ embed: { footer: { text: '{managers}' } } }, 'managers'],
    [{ content: 'hi' }, 'managers'], [null, 'managers'], [{ embed: { title: '{reason}' } }, 'reason'], [{ content: '{managers.x}' }, 'managers'],
  ].map(([t, tok]) => ({ tpl: t, token: tok, out: svc.templateUsesToken(t, tok) })),
  clamped: svc.clampTemplateForModal({ embed: { description: 'a'.repeat(4096), title: 't'.repeat(300), footer: { text: 'f'.repeat(2100) } }, content: 'c'.repeat(2500) }),
  empty: [
    { content: null, embed: null, buttons: [] }, { content: '   ', embed: { color: 0x123456 } }, { content: 'hi' }, { embed: { title: 'x' } }, { embed: { image: { url: 'https://x.y/z.png' } } }, null,
  ].map((t) => ({ tpl: t, out: svc.isEmptyTemplate(t) })),
};

// ── Transcripts ───────────────────────────────────────────────────────────────
const escapes = ['<script>&"\'', null, 'plain', 'a&amp;b'].map((s) => ({ input: s, out: svc.escapeHtml(s) }));
const bytes = [0, -5, 512, 1023, 1024, 1536, 1280, 10 * 1024 + 51, 3.4 * 1024 * 1024, 1024 ** 3 * 2.25, 1024 ** 2 - 1, Number.NaN].map((n) => ({ n: Number.isNaN(n) ? null : n, out: svc.formatBytes(n) }));
const baseMeta = () => ({
  num: '0007', guildName: 'My <Guild>', channelName: 'ticket-0007', panelName: 'Support', ownerTag: 'bob#0', ownerId: '1',
  openedAt: Date.UTC(2026, 0, 2, 3, 4), claimedTag: '', closedAt: null, reason: '', truncated: null, generatedAt: Date.UTC(2026, 8, 3, 23, 59), generatedByTag: 'mod#0',
});
const entry = (i, over = {}) => ({
  id: `m${i}`, authorId: '1', authorName: 'Bob', authorTag: 'bob#0', bot: false, createdTimestamp: Date.UTC(2026, 0, 2, 3, 5 + (i % 50)),
  editedTimestamp: null, system: false, systemLabel: '', content: `message ${i}`, replyToId: null, attachments: [], embeds: [], stickers: [], ...over,
});
const nasty = entry(1, {
  authorName: '<b>x</b>', content: '<img src=x onerror=alert(1)>', editedTimestamp: Date.UTC(2026, 5, 7, 8, 9),
  attachments: [{ name: 'x"><script>', url: 'javascript:alert(1)', size: 10 }, { name: 'ok.png', url: 'https://cdn.discordapp.com/a.png?x="y"', size: 2048 }, { name: 'zero', url: 'https://a.b/c', size: 0 }],
  embeds: [{ title: '<i>t</i>', description: '<u>d</u>', fields: [{ name: '<f>', value: '<v>' }], footer: '<foot>' }, { title: '', description: '', fields: [], footer: '' }],
  stickers: ['<sticker>'], replyToId: 'm0" onmouseover="x', bot: true,
});
const transcriptCases = [
  { name: 'nasty', meta: baseMeta(), entries: [nasty], tz: 'UTC' },
  { name: 'tokyo', meta: { ...baseMeta(), claimedTag: 'mod#0', closedAt: Date.UTC(2026, 10, 5, 22, 1), reason: 'needs <help>' }, entries: [entry(1), entry(2)], tz: 'Asia/Tokyo' },
  { name: 'newyork', meta: baseMeta(), entries: [entry(3)], tz: 'America/New_York' },
  { name: 'badzone', meta: baseMeta(), entries: [entry(1)], tz: 'Not/AZone' },
  { name: 'limit', meta: { ...baseMeta(), truncated: 'limit' }, entries: [entry(1)], tz: 'UTC' },
  { name: 'time', meta: { ...baseMeta(), truncated: 'time', panelName: '' }, entries: [entry(1)], tz: 'UTC' },
  { name: 'empty', meta: baseMeta(), entries: [], tz: 'Europe/London' },
  { name: 'system', meta: baseMeta(), entries: [entry(2, { system: true, systemLabel: 'pinned a message', content: '', bot: true }), entry(4, { system: true, systemLabel: '' })], tz: 'UTC' },
  { name: 'cap', meta: baseMeta(), entries: Array.from({ length: 200 }, (_, i) => entry(i, { content: 'x'.repeat(100) })), tz: 'UTC', maxBytes: 4096 },
  { name: 'unicode', meta: { ...baseMeta(), guildName: 'Ünïcødé 🎫' }, entries: [entry(5, { content: 'héllo 🎫 世界' })], tz: 'Australia/Lord_Howe' },
];
const transcripts = transcriptCases.map((c) => ({ ...c, out: svc.renderTranscriptHtml(c.meta, c.entries, { timeZone: c.tz, ...(c.maxBytes ? { maxBytes: c.maxBytes } : {}) }) }));

// normalizeMessage: raw Discord API messages, turned into the discord.js shapes the function reads.
const guildData = { roles: { '700000000000000002': 'mods' }, channels: { '700000000000000003': 'general' } };
const fakeGuild = {
  roles: { cache: new Map(Object.entries(guildData.roles).map(([id, name]) => [id, { name }])) },
  channels: { cache: new Map(Object.entries(guildData.channels).map(([id, name]) => [id, { name }])) },
  members: { cache: new Map() },
};
const rawUser = (id, username, over = {}) => ({ id, username, discriminator: '0', global_name: null, avatar: null, ...over });
const rawMessage = (over) => ({
  id: '700000000000000100', channel_id: '700000000000000003', author: rawUser('700000000000000001', 'bob'), content: '', timestamp: '2026-01-02T03:04:05.678000+00:00',
  edited_timestamp: null, tts: false, mention_everyone: false, mentions: [], mention_roles: [], attachments: [], embeds: [], pinned: false, type: 0, ...over,
});
const tagOf = (u) => (u.discriminator && u.discriminator !== '0' ? `${u.username}#${u.discriminator}` : u.username);
const toJsMessage = (raw) => ({
  id: raw.id,
  content: raw.content,
  mentions: {
    users: new Map(raw.mentions.map((m) => [m.id, { username: m.username }])),
    members: new Map(raw.mentions.filter((m) => m.member).map((m) => [m.id, { displayName: m.member.nick ?? m.global_name ?? m.username }])),
  },
  author: { id: raw.author.id, username: raw.author.username, globalName: raw.author.global_name, tag: tagOf(raw.author), bot: Boolean(raw.author.bot) },
  member: raw.member ? { displayName: raw.member.nick ?? raw.author.global_name ?? raw.author.username } : null,
  createdTimestamp: Date.parse(raw.timestamp),
  editedTimestamp: raw.edited_timestamp ? Date.parse(raw.edited_timestamp) : null,
  system: ![0, 19, 20, 23].includes(raw.type),
  type: raw.type,
  reference: raw.message_reference ? { messageId: raw.message_reference.message_id } : null,
  attachments: new Map(raw.attachments.map((a) => [a.id, { name: a.filename, url: a.url, size: a.size }])),
  embeds: raw.embeds.map((e) => ({ title: e.title, description: e.description, fields: e.fields ?? [], footer: e.footer })),
  stickers: new Map((raw.sticker_items ?? []).map((s) => [s.id, { name: s.name }])),
});
const rawMessages = [
  rawMessage({ content: 'hi <@700000000000000001> <@&700000000000000002> <#700000000000000003> <:pog:4> <t:0:R>', mentions: [rawUser('700000000000000001', 'bob')] }),
  rawMessage({ content: '<@700000000000000001> <#700000000000000003>', mentions: [rawUser('700000000000000001', '<#700000000000000003>')] }),
  rawMessage({
    content: '<@9> <@&8> <#7> <t:99999999999999999:f> <t:-5:t> <a:wave:12>',
    member: { nick: 'Bobby', roles: [], joined_at: '2026-01-01T00:00:00.000000+00:00', deaf: false, mute: false },
    attachments: [
      { id: '1', filename: 'a.png', url: 'https://cdn.discordapp.com/a.png', proxy_url: 'https://media.discordapp.net/a.png', size: 5 },
      { id: '2', filename: 'b', url: 'javascript:alert(1)', proxy_url: 'javascript:alert(1)', size: 1 },
    ],
    sticker_items: [{ id: '5', name: 'wave', format_type: 1 }],
    embeds: [{ title: 't', description: 'd', fields: [{ name: 'n', value: 'v', inline: false }], footer: { text: 'f' } }, {}],
    message_reference: { message_id: '700000000000000099', channel_id: '700000000000000003' },
    edited_timestamp: '2026-01-02T03:05:00.000000+00:00',
    type: 6,
  }),
  rawMessage({ content: 'x'.repeat(5000), author: rawUser('700000000000000005', 'ann', { global_name: 'Ann', bot: true }) }),
  rawMessage({ content: '<@!700000000000000006> joined', type: 7, author: rawUser('700000000000000006', 'new', { discriminator: '1234' }),
    mentions: [rawUser('700000000000000006', 'new', { global_name: 'Newbie', member: { nick: null, roles: [], joined_at: '2026-01-01T00:00:00.000000+00:00', deaf: false, mute: false } })] }),
  rawMessage({ content: 'reply', type: 19, message_reference: { message_id: '700000000000000098', channel_id: '700000000000000003' } }),
  rawMessage({ content: 'thread', type: 18 }),
];
const normalized = rawMessages.map((raw) => ({ raw, out: svc.normalizeMessage(toJsMessage(raw), fakeGuild) }));

const out = {
  errors, panelErrors, refreshNotes, notices, components, customIds, overwrites, merges, syncPayload, discordErrors, sanitized, formats, renderUser: { user, member },
  rendered, numbers, panelNames, emojis, classify: { now, rows: classifyRows, out: classify }, renameCases, transitions, perms, templates, escapes, bytes,
  transcripts, normalize: { guild: guildData, cases: normalized },
  managersWarning: svc.MANAGERS_WARNING,
};
mkdirSync(OUT, { recursive: true });
writeFileSync(join(OUT, 'pure.json'), `${JSON.stringify(out, null, 1)}\n`);
redis.disconnect();
process.stdout.write(`wrote ${join(OUT, 'pure.json')}\n`);
process.exit(0);
