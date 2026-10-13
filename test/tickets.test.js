// pkg-tickets tests (pkg-tickets.md §8). Offline by default: permissions and the state machine, names, emoji, panel components, rows,
// overwrites, row classification, the rename window, templates, the transcript renderer, customIds, error text, the plugin
// declaration, the command payloads and the no-I/O paths of the component handlers.
// With TEST_DATABASE_URL (a migrated database) the DB cases also run: every RAW_SQL statement, panel/option/manager writes under
// their locks, the open flow with fake Discord objects (numbers, the per-user cap, categories, overwrites, failures), the lifecycle
// actions, delete, the channelDelete hook, autocomplete queries, the template form and reconcile:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/tickets.test.js
// NOTE: the database tests run on one Postgres connection (connection_limit=1), so concurrent calls are serialised by the pool:
// they prove the final state (distinct numbers, the cap held), not contention on the advisory locks.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';
delete process.env.BOT_OWNER_ID;

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ChannelType, Collection, OverwriteType, PermissionFlagsBits } = await import('discord.js');
const { Prisma } = await import('@prisma/client');
const svc = await import('../src/services/ticketService.js');
const ticketCmd = await import('../src/commands/community/ticket.js');
const panelCmd = await import('../src/commands/admin/ticket-panel.js');
const pluginMod = await import('../src/plugins/tickets.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { buildCustomId, parseCustomId } = await import('../src/config/customIds.js');
const { validateTemplate, renderTemplate } = await import('../src/utils/customEmbed.js');
const { ensureGuildSettings, updateSettings } = await import('../src/services/settingsService.js');

const {
  PENDING_PREFIX, PENDING_STALE_MS, MAX_ADDED_USERS, MEMBER_ALLOW, BOT_ALLOW, DEFAULT_INTRO_TEMPLATE, DEFAULT_OPTION, OPEN_REQUIRED_PERMS,
  isTicketStaff, canPerform, transitionFor, formatTicketNumber, sanitizeChannelName, validateNameFormat, renderChannelName,
  parseOptionEmoji, isUnicodeEmoji, classifyTicketRows, renameWindowCheck, buildTicketOverwrites, liveManagerRoleIds, resolveCategoryId,
  defaultPanelTemplate, templateUsesToken, clampTemplateForModal, isEmptyTemplate, buildPanelComponents, buildIntroRow, buildClosedRow,
  disableButtons, escapeHtml, formatBytes, normalizeMessage, renderTranscriptHtml, describeDiscordError, isCategoryFullError,
  buildReasonModal, errorPayload, panelErrorPayload, refreshNote, buildNotice, normalizePanelName, RAW_SQL,
} = svc;

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const G = (n) => `95502000000000${String(n).padStart(4, '0')}`; // guild ids unique to this package (port 55020)
const ALL_GUILDS = Array.from({ length: 100 }, (_, i) => G(i + 1));

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) {
    // The cascade removes TicketPanel, TicketPanelOption and Ticket rows.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: ALL_GUILDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

const fakeUser = { id: '1', username: 'bob', globalName: null, tag: 'bob#0' };

// ── 1. canPerform ─────────────────────────────────────────────────────────────

test('canPerform: the role matrix', () => {
  const owner = { isOwner: true, isStaff: false };
  const staff = { isOwner: false, isStaff: true };
  const both = { isOwner: true, isStaff: true };
  const nobody = { isOwner: false, isStaff: false };

  assert.deepEqual(canPerform('claim', owner), { ok: false, code: 'NOT_STAFF' });
  assert.deepEqual(canPerform('claim', staff), { ok: true });
  assert.deepEqual(canPerform('claim', both), { ok: false, code: 'OWN_TICKET' });
  assert.deepEqual(canPerform('claim', nobody), { ok: false, code: 'NOT_STAFF' });

  assert.equal(canPerform('close', owner).ok, true);
  assert.equal(canPerform('close', staff).ok, true);
  assert.equal(canPerform('close', both).ok, true);
  assert.deepEqual(canPerform('close', nobody), { ok: false, code: 'NOT_OWNER_OR_STAFF' });

  for (const action of ['reopen', 'delete', 'add', 'remove', 'transcript', 'rename']) {
    assert.deepEqual(canPerform(action, owner), { ok: false, code: 'NOT_STAFF' }, action);
    assert.deepEqual(canPerform(action, nobody), { ok: false, code: 'NOT_STAFF' }, action);
    assert.equal(canPerform(action, staff).ok, true, action);
    assert.equal(canPerform(action, both).ok, true, action);
  }
});

// ── 2. transitionFor ──────────────────────────────────────────────────────────

test('transitionFor: the state machine', () => {
  const me = { actorId: 'a', isAdmin: false };
  assert.equal(transitionFor('claim', { status: 'OPEN' }, me).to, 'CLAIMED');
  assert.equal(transitionFor('claim', { status: 'CLAIMED', claimedById: 'a' }, me).code, 'ALREADY_CLAIMED_SELF');
  const other = transitionFor('claim', { status: 'CLAIMED', claimedById: 'b' }, me);
  assert.equal(other.code, 'ALREADY_CLAIMED');
  assert.deepEqual(other.data, { claimedById: 'b' });
  const takeover = transitionFor('claim', { status: 'CLAIMED', claimedById: 'b' }, { actorId: 'a', isAdmin: true });
  assert.equal(takeover.ok, true);
  assert.equal(takeover.to, 'CLAIMED');
  assert.equal(takeover.takeover, true);
  assert.equal(transitionFor('claim', { status: 'CLOSED' }, me).code, 'IS_CLOSED');

  assert.equal(transitionFor('close', { status: 'OPEN' }, me).to, 'CLOSED');
  assert.equal(transitionFor('close', { status: 'CLAIMED' }, me).to, 'CLOSED');
  assert.equal(transitionFor('close', { status: 'CLOSED' }, me).code, 'ALREADY_CLOSED');

  assert.equal(transitionFor('reopen', { status: 'CLOSED', claimedById: 'x' }, me).to, 'CLAIMED');
  assert.equal(transitionFor('reopen', { status: 'CLOSED', claimedById: null }, me).to, 'OPEN');
  assert.equal(transitionFor('reopen', { status: 'OPEN' }, me).code, 'NOT_CLOSED');

  assert.equal(transitionFor('delete', { status: 'OPEN' }, { ...me, source: 'button' }).code, 'CLOSE_FIRST');
  assert.equal(transitionFor('delete', { status: 'CLOSED' }, { ...me, source: 'button' }).to, 'DELETED');
  assert.equal(transitionFor('delete', { status: 'OPEN' }, { ...me, source: 'command' }).to, 'DELETED');

  for (const action of ['add', 'remove', 'rename', 'transcript']) {
    assert.deepEqual(transitionFor(action, { status: 'CLOSED' }, me), { ok: true, to: null }, action);
  }
  assert.equal(transitionFor('bogus', { status: 'OPEN' }, me).ok, false);
});

// ── 3. isTicketStaff ──────────────────────────────────────────────────────────

test('isTicketStaff: manager roles and moderator levels', () => {
  const member = (has = () => false) => ({
    id: 'u', guild: { ownerId: 'o' }, permissions: { has }, roles: { cache: new Map([['r1', {}]]) },
  });
  assert.equal(isTicketStaff(member(), { managerRoleIds: ['r1'] }, null), true);
  assert.equal(isTicketStaff(member(), { managerRoleIds: ['r2'] }, null), false);
  assert.equal(isTicketStaff(member(), null, null), false, 'a deleted panel leaves moderators and up only');
  assert.equal(isTicketStaff(member(), undefined, null), false);
  assert.equal(isTicketStaff(null, { managerRoleIds: ['r1'] }, null), false);
  const mod = member((flag) => flag === PermissionFlagsBits.ModerateMembers);
  assert.equal(isTicketStaff(mod, { managerRoleIds: [] }, null), true);
  assert.equal(isTicketStaff(member(), { managerRoleIds: [] }, { modRoleIds: ['r1'] }), true, 'a configured mod role');
});

// ── 4. Names ──────────────────────────────────────────────────────────────────

test('formatTicketNumber, sanitizeChannelName, renderChannelName', () => {
  assert.equal(formatTicketNumber(1), '0001');
  assert.equal(formatTicketNumber(12345), '12345');
  assert.equal(sanitizeChannelName('Ticket 0001'), 'ticket-0001');
  assert.equal(sanitizeChannelName('山田 太郎'), '山田-太郎');
  assert.equal(sanitizeChannelName('!!!'), '');
  assert.equal(sanitizeChannelName('--a--b--'), 'a-b');
  assert.ok(sanitizeChannelName('a'.repeat(150)).length <= 100);
  assert.equal(sanitizeChannelName(null), '');

  assert.equal(renderChannelName('ticket-{number}', { number: 12, user: fakeUser }), 'ticket-0012');
  assert.equal(
    renderChannelName('{user.display_name}-{number}', { number: 3, user: { username: 'bob', globalName: 'Bob Smith', id: '1' }, member: null }),
    'bob-smith-0003',
  );
  assert.equal(renderChannelName('{topic}', { number: 7, topic: '***' }), 'ticket-0007');
  assert.equal(renderChannelName('{user.name}-{topic}', { number: 7, user: fakeUser, topic: 'Billing Help' }), 'bob-billing-help');
});

test('normalizePanelName', () => {
  assert.equal(normalizePanelName('  Support  '), 'Support');
  assert.equal(normalizePanelName(''), null);
  assert.equal(normalizePanelName('   '), null);
  assert.equal(normalizePanelName('a\nb'), null);
  assert.equal(normalizePanelName('x'.repeat(51)), null);
  assert.equal(normalizePanelName('x'.repeat(50)), 'x'.repeat(50));
});

// ── 5. validateNameFormat ─────────────────────────────────────────────────────

test('validateNameFormat', () => {
  assert.equal(validateNameFormat('ticket-{number}').ok, true);
  assert.equal(validateNameFormat('{number}').ok, true);
  assert.equal(validateNameFormat('{USER.NAME}').ok, true, 'tokens are case-insensitive');
  assert.equal(validateNameFormat('{user.id}-{topic}').ok, true);
  const unknown = validateNameFormat('{server}-{number}');
  assert.equal(unknown.ok, false);
  assert.ok(unknown.error.includes('`{server}`'));
  assert.equal(validateNameFormat('').ok, false);
  assert.equal(validateNameFormat('   ').ok, false);
  assert.equal(validateNameFormat('x'.repeat(91)).ok, false);
  assert.equal(validateNameFormat('x'.repeat(90)).ok, true);
  assert.equal(validateNameFormat('!!!').ok, false, 'no usable characters');
  assert.equal(validateNameFormat('  ticket-{number}  ').format, 'ticket-{number}');
});

// ── 6. parseOptionEmoji ───────────────────────────────────────────────────────

test('parseOptionEmoji', () => {
  const client = { emojis: { cache: new Map([['123456789012345678', {}]]) } };
  const guild = { emojis: { cache: new Map([['223456789012345678', { name: 'pog', toString: () => '<:pog:223456789012345678>' }]]) } };
  const p = (s) => parseOptionEmoji(s, { client, guild });

  assert.deepEqual(p('✉️'), { ok: true, emoji: '✉️' });
  assert.equal(p('👍🏽').ok, true);
  assert.equal(p('🇩🇪').ok, true, 'a flag is one grapheme');
  assert.equal(p('1️⃣').ok, true, 'a keycap');
  assert.deepEqual(p('<:x:123456789012345678>'), { ok: true, emoji: '<:x:123456789012345678>' });
  assert.deepEqual(p('<a:x:123456789012345678>'), { ok: true, emoji: '<a:x:123456789012345678>' });
  assert.equal(p('<:x:999999999999999999>').ok, false);
  assert.deepEqual(p(':POG:'), { ok: true, emoji: '<:pog:223456789012345678>' });
  assert.equal(p(':nope:').ok, false);
  assert.equal(p('ab').ok, false);
  assert.equal(p('✉️✉️').ok, false, 'two graphemes');
  assert.deepEqual(p('none'), { ok: true, emoji: null });
  assert.deepEqual(p('NONE'), { ok: true, emoji: null });
  assert.deepEqual(p(''), { ok: true, emoji: null });
  assert.equal(p('5').ok, false, 'a bare digit is not an emoji');
  assert.equal(isUnicodeEmoji('✉️'), true);
  assert.equal(isUnicodeEmoji('a'), false);
});

// ── 7. buildPanelComponents ───────────────────────────────────────────────────

test('buildPanelComponents: buttons', () => {
  const panel = { id: 'p'.repeat(25), style: 'BUTTONS' };
  const options = Array.from({ length: 25 }, (_, i) => ({ id: `o${i}`, label: `Option ${i}`, buttonStyle: (i % 4) + 1, position: i, emoji: null }));
  const rows = buildPanelComponents(panel, options).map((r) => r.toJSON());
  assert.equal(rows.length, 5);
  for (const row of rows) assert.equal(row.components.length, 5);
  const flat = rows.flatMap((r) => r.components);
  flat.forEach((b, i) => {
    assert.equal(b.custom_id, `ticket:open:${panel.id}:o${i}`);
    assert.ok(b.custom_id.length <= 100);
    assert.deepEqual(parseCustomId(b.custom_id), { prefix: 'ticket', params: ['open', panel.id, `o${i}`] });
    assert.equal(b.style, (i % 4) + 1);
  });

  const seven = buildPanelComponents(panel, options.slice(0, 7)).map((r) => r.toJSON());
  assert.deepEqual(seven.map((r) => r.components.length), [5, 2]);

  // Ordering follows position, then id.
  const shuffled = [{ id: 'b', label: 'B', buttonStyle: 1, position: 2 }, { id: 'a', label: 'A', buttonStyle: 1, position: 0 }, { id: 'c', label: 'C', buttonStyle: 1, position: 1 }];
  assert.deepEqual(buildPanelComponents(panel, shuffled)[0].toJSON().components.map((c) => c.label), ['A', 'C', 'B']);

  // A custom emoji the bot cannot use is omitted; a Unicode one stays.
  const emojis = [
    { id: 'x', label: 'X', buttonStyle: 1, position: 0, emoji: '<:gone:123456789012345678>' },
    { id: 'y', label: 'Y', buttonStyle: 1, position: 1, emoji: '✉️' },
  ];
  const [x, y] = buildPanelComponents(panel, emojis, { canUseEmoji: () => false })[0].toJSON().components;
  assert.equal(x.emoji, undefined);
  assert.equal(y.emoji.name, '✉️');
  const [x2] = buildPanelComponents(panel, emojis, { canUseEmoji: () => true })[0].toJSON().components;
  assert.equal(x2.emoji.id, '123456789012345678');
  // An invalid style falls back to Blue.
  assert.equal(buildPanelComponents(panel, [{ id: 'z', label: 'Z', buttonStyle: 9, position: 0 }])[0].toJSON().components[0].style, 1);
  assert.deepEqual(buildPanelComponents(panel, []), []);
});

test('buildPanelComponents: dropdown', () => {
  const panel = { id: 'p'.repeat(25), style: 'SELECT' };
  const options = Array.from({ length: 25 }, (_, i) => ({
    id: `o${i}`, label: `L${i}`, position: i, description: i === 0 ? 'd'.repeat(300) : null, emoji: i === 1 ? '<:x:123456789012345678>' : null,
  }));
  const rows = buildPanelComponents(panel, options, { canUseEmoji: () => false }).map((r) => r.toJSON());
  assert.equal(rows.length, 1);
  assert.equal(rows[0].components.length, 1);
  const menu = rows[0].components[0];
  assert.equal(menu.custom_id, `ticket-select:${panel.id}`);
  assert.equal(menu.min_values, 1);
  assert.equal(menu.max_values, 1);
  assert.equal(menu.options.length, 25);
  assert.deepEqual(menu.options.map((o) => o.value), options.map((o) => o.id));
  assert.ok(menu.options[0].description.length <= 100);
  assert.equal(menu.options[1].emoji, undefined, 'an unusable custom emoji is omitted');
  assert.equal(Object.hasOwn(menu.options[2], 'description'), false, 'absent keys are omitted');
});

// ── 8. Rows ───────────────────────────────────────────────────────────────────

test('buildIntroRow, buildClosedRow, disableButtons', () => {
  const id = 't'.repeat(25);
  const intro = buildIntroRow(id, { claimed: false }).toJSON();
  assert.deepEqual(intro.components.map((c) => c.custom_id), [`ticket:close:${id}`, `ticket:claim:${id}`]);
  assert.equal(intro.components[1].label, 'Claim');
  assert.notEqual(intro.components[1].disabled, true);
  const claimed = buildIntroRow(id, { claimed: true }).toJSON();
  assert.equal(claimed.components[1].disabled, true);
  assert.equal(claimed.components[1].label, 'Claimed');

  const closed = buildClosedRow(id).toJSON();
  assert.deepEqual(closed.components.map((c) => c.custom_id), [`ticket:reopen:${id}`, `ticket:transcript:${id}`, `ticket:delete:${id}`]);

  const rows = [buildIntroRow(id), { type: 1, components: [{ type: 2, style: 5, label: 'Link', url: 'https://example.com' }] }];
  const onlyClaim = disableButtons(rows, (c) => c.custom_id?.startsWith('ticket:claim:'));
  assert.equal(onlyClaim[0].components[0].disabled, undefined);
  assert.equal(onlyClaim[0].components[1].disabled, true);
  assert.equal(onlyClaim[1].components[0].disabled, undefined, 'link buttons are kept as they are');
  const all = disableButtons(rows);
  assert.equal(all[0].components.every((c) => c.disabled), true);
  assert.equal(all[1].components[0].disabled, undefined);
  assert.equal(rows[0].toJSON().components[0].disabled, undefined, 'the input is not mutated');
});

// ── 9. buildTicketOverwrites ──────────────────────────────────────────────────

test('buildTicketOverwrites', () => {
  const base = { guildId: 'G', botId: 'B', ownerId: 'O', managerRoleIds: ['R1', 'R2'], addedUserIds: ['U1'], closed: false };
  const open = buildTicketOverwrites(base);
  assert.deepEqual(open[0], { id: 'G', type: 0, deny: ['ViewChannel'] });
  const bot = open.find((o) => o.id === 'B');
  assert.equal(bot.type, 1);
  assert.ok(bot.allow.includes('ManageChannels'));
  assert.deepEqual(bot.allow, [...BOT_ALLOW]);
  assert.deepEqual(open.find((o) => o.id === 'O').allow, [...MEMBER_ALLOW]);
  assert.equal(open.find((o) => o.id === 'R1').type, OverwriteType.Role);
  assert.equal(open.find((o) => o.id === 'U1').type, OverwriteType.Member);
  for (const o of open) {
    assert.ok(o.type === 0 || o.type === 1, 'every entry has an explicit type');
    assert.equal([...(o.allow ?? []), ...(o.deny ?? [])].includes('ManageRoles'), false, 'ManageRoles is never in an overwrite');
  }

  const closed = buildTicketOverwrites({ ...base, closed: true });
  for (const id of ['O', 'U1']) {
    const o = closed.find((x) => x.id === id);
    assert.deepEqual(o.allow, ['ViewChannel', 'ReadMessageHistory']);
    assert.deepEqual(o.deny, ['SendMessages']);
  }
  assert.deepEqual(closed.find((x) => x.id === 'R1').allow, [...MEMBER_ALLOW], 'staff keep full access');

  const dup = buildTicketOverwrites({ ...base, addedUserIds: ['O', 'U1', 'U1'] });
  assert.equal(dup.filter((o) => o.id === 'O').length, 1);
  assert.equal(dup.filter((o) => o.id === 'U1').length, 1);
  assert.equal(buildTicketOverwrites({ guildId: 'G', botId: 'B', ownerId: 'O' }).length, 3, 'optional lists default to empty');
});

test('liveManagerRoleIds and resolveCategoryId', () => {
  const guild = { id: 'G', roles: { cache: new Map([['R1', {}], ['R2', {}], ['G', {}]]) } };
  assert.deepEqual(liveManagerRoleIds({ managerRoleIds: ['R1', 'gone', 'G', 'R2'] }, guild), ['R1', 'R2']);
  assert.deepEqual(liveManagerRoleIds(null, guild), []);
  const panel = { openCategoryId: 'po', claimedCategoryId: 'pc', closedCategoryId: null };
  assert.equal(resolveCategoryId('open', { panel, option: { categoryId: 'oo' } }), 'oo');
  assert.equal(resolveCategoryId('open', { panel, option: { categoryId: null } }), 'po');
  assert.equal(resolveCategoryId('claimed', { panel }), 'pc');
  assert.equal(resolveCategoryId('closed', { panel }), null);
  assert.equal(resolveCategoryId('open', {}), null);
});

// ── 10. classifyTicketRows ────────────────────────────────────────────────────

test('classifyTicketRows', () => {
  const now = 1_000_000;
  const rows = [
    { id: 'fresh', channelId: `${PENDING_PREFIX}1`, createdAt: new Date(now - 1000) },
    { id: 'stale', channelId: `${PENDING_PREFIX}2`, createdAt: new Date(now - PENDING_STALE_MS - 1) },
    { id: 'real', channelId: '100', createdAt: new Date(now) },
    { id: 'gone', channelId: '200', createdAt: new Date(now) },
  ];
  const r = classifyTicketRows(rows, (id) => id === '100', now);
  assert.deepEqual(r.live.map((x) => x.id), ['fresh', 'real']);
  assert.deepEqual(r.stalePending, ['stale']);
  assert.deepEqual(r.staleGone, ['gone']);
  assert.deepEqual(classifyTicketRows([], () => true, now), { live: [], stalePending: [], staleGone: [] });
  // The boundary: exactly PENDING_STALE_MS old is stale.
  assert.deepEqual(classifyTicketRows([{ id: 'edge', channelId: `${PENDING_PREFIX}3`, createdAt: now - PENDING_STALE_MS }], () => true, now).stalePending, ['edge']);
});

// ── 11. renameWindowCheck ─────────────────────────────────────────────────────

test('renameWindowCheck', () => {
  const now = 10_000_000;
  assert.deepEqual(renameWindowCheck([], now), { allowed: true, retryAt: null });
  const blocked = renameWindowCheck([now - 1000, now - 2000], now);
  assert.equal(blocked.allowed, false);
  assert.equal(blocked.retryAt.getTime(), now - 2000 + 600_000);
  assert.deepEqual(renameWindowCheck([now - 700_000, now - 1000], now), { allowed: true, retryAt: null });
  assert.equal(renameWindowCheck([now - 1000], now).allowed, true);
});

// ── 12. Templates ─────────────────────────────────────────────────────────────

test('templates: defaults, token scan, modal clamp, empty detection', () => {
  assert.equal(defaultPanelTemplate('BUTTONS').embed.title, 'How can we help?');
  assert.ok(defaultPanelTemplate('SELECT').embed.description.includes('Choose a topic'));
  assert.ok(defaultPanelTemplate('BUTTONS').embed.description.includes('Press a button'));
  assert.equal(validateTemplate(DEFAULT_INTRO_TEMPLATE).ok, true);
  assert.equal(validateTemplate(defaultPanelTemplate('BUTTONS')).ok, true);

  assert.equal(templateUsesToken({ content: 'hi {Managers}' }, 'managers'), true);
  assert.equal(templateUsesToken({ embed: { description: 'ping { managers } now' } }, 'managers'), true);
  assert.equal(templateUsesToken({ embed: { footer: { text: '{managers}' } } }, 'managers'), true);
  assert.equal(templateUsesToken({ content: 'hi' }, 'managers'), false);
  assert.equal(templateUsesToken(null, 'managers'), false);

  const clamped = clampTemplateForModal({ embed: { description: 'a'.repeat(4096), title: 't'.repeat(300) }, content: 'c'.repeat(2500) });
  assert.equal(clamped.embed.description.length, 4000);
  assert.equal(clamped.embed.title.length, 256);
  assert.equal(clamped.content.length, 2000);
  const original = { embed: { description: 'a'.repeat(4096) } };
  clampTemplateForModal(original);
  assert.equal(original.embed.description.length, 4096, 'the input is not mutated');

  assert.equal(isEmptyTemplate({ content: null, embed: null, buttons: [] }), true);
  assert.equal(isEmptyTemplate({ content: '   ', embed: { color: 0x123456 } }), true, 'a colour alone shows nothing');
  assert.equal(isEmptyTemplate({ content: 'hi' }), false);
  assert.equal(isEmptyTemplate({ embed: { title: 'x' } }), false);

  const intro = renderTemplate(DEFAULT_INTRO_TEMPLATE, {
    user: fakeUser, guild: { id: 'g', name: 'Guild' }, channel: { id: 'c', name: 'ticket-0001' }, vars: { number: '0001' },
  });
  assert.equal(intro.embeds[0].data.title, 'Ticket #0001');
  assert.equal(intro.content, '<@1>');
});

test('the default templates are frozen and never mutated by normalising', () => {
  assert.equal(Object.isFrozen(DEFAULT_INTRO_TEMPLATE), true);
  assert.equal(Object.isFrozen(DEFAULT_INTRO_TEMPLATE.embed), true);
  assert.equal(Object.isFrozen(DEFAULT_OPTION), true);
  assert.deepEqual({ ...DEFAULT_OPTION }, { label: 'Open Ticket', emoji: '✉️', buttonStyle: 1, position: 0 });
  assert.equal(isEmptyTemplate(DEFAULT_INTRO_TEMPLATE), false);
  clampTemplateForModal(DEFAULT_INTRO_TEMPLATE);
  assert.equal(DEFAULT_INTRO_TEMPLATE.buttons.length, 0);
});

// ── 13. Transcript ────────────────────────────────────────────────────────────

const baseMeta = () => ({
  num: '0007', guildName: 'My <Guild>', channelName: 'ticket-0007', panelName: 'Support', ownerTag: 'bob#0', ownerId: '1',
  openedAt: Date.UTC(2026, 0, 2, 3, 4), claimedTag: '', closedAt: null, reason: '', truncated: null, generatedAt: Date.UTC(2026, 0, 3), generatedByTag: 'mod#0',
});
const entry = (i, over = {}) => ({
  id: `m${i}`, authorId: '1', authorName: 'Bob', authorTag: 'bob#0', bot: false, createdTimestamp: Date.UTC(2026, 0, 2, 3, 5 + (i % 50)),
  editedTimestamp: null, system: false, systemLabel: '', content: `message ${i}`, replyToId: null, attachments: [], embeds: [], stickers: [], ...over,
});

test('escapeHtml and formatBytes', () => {
  assert.equal(escapeHtml('<script>&"\''), '&lt;script&gt;&amp;&quot;&#39;');
  assert.equal(escapeHtml(null), '');
  assert.equal(formatBytes(512), '512 B');
  assert.equal(formatBytes(1536), '1.5 KB');
  assert.equal(formatBytes(3.4 * 1024 * 1024), '3.4 MB');
});

test('renderTranscriptHtml: escaping, no remote loads, the CSP', () => {
  const nasty = entry(1, {
    authorName: '<b>x</b>',
    content: '<img src=x onerror=alert(1)>',
    attachments: [{ name: 'x"><script>', url: 'javascript:alert(1)', size: 10 }, { name: 'ok.png', url: 'https://cdn.discordapp.com/a.png?x="y"', size: 2048 }],
    embeds: [{ title: '<i>t</i>', description: '<u>d</u>', fields: [{ name: '<f>', value: '<v>' }], footer: '<foot>' }],
    stickers: ['<sticker>'],
    replyToId: 'm0" onmouseover="x',
  });
  const { html, included, omitted } = renderTranscriptHtml(baseMeta(), [nasty]);
  assert.equal(included, 1);
  assert.equal(omitted, 0);
  assert.ok(html.includes('&lt;img src=x'));
  assert.ok(!/<img/i.test(html));
  assert.ok(!/<script/i.test(html));
  assert.ok(!/javascript:/i.test(html));
  assert.ok(!/<[^>]*\ssrc=/i.test(html), 'no tag carries a src attribute');
  assert.ok(!/<link/i.test(html));
  assert.ok(!/<iframe/i.test(html));
  assert.ok(html.includes('Content-Security-Policy'));
  assert.ok(html.includes("default-src 'none'"));
  assert.ok(html.includes('My &lt;Guild&gt;'));
  assert.ok(html.includes('href="https://cdn.discordapp.com/a.png?x=&quot;y&quot;"'));
  assert.ok(html.includes('rel="noopener noreferrer nofollow"'));
  assert.ok(html.includes('2.0 KB'));
  assert.ok(html.includes('href="#m-m0&quot; onmouseover=&quot;x"'));
  // Every attribute value is escaped: no raw quote can close one early.
  assert.ok(!html.includes('onmouseover="x'));
});

test('renderTranscriptHtml: the size cap keeps the newest messages', () => {
  const entries = Array.from({ length: 200 }, (_, i) => entry(i, { content: 'x'.repeat(100) }));
  const r = renderTranscriptHtml(baseMeta(), entries, { maxBytes: 4096 });
  assert.ok(r.omitted > 0, 'something was omitted');
  assert.ok(r.included > 0, 'something fits');
  assert.equal(r.included + r.omitted, 200);
  assert.ok(r.html.includes('earlier messages are not included'));
  assert.ok(r.html.includes('id="m-m199"'), 'the newest message is kept');
  assert.ok(!r.html.includes('id="m-m0"'), 'the oldest is dropped');
  assert.ok(Buffer.byteLength(r.html) <= 4096);
  // Chronological order is kept within the suffix.
  const ids = [...r.html.matchAll(/id="m-m(\d+)"/g)].map((m) => Number(m[1]));
  assert.deepEqual(ids, [...ids].sort((a, b) => a - b));

  const all = renderTranscriptHtml(baseMeta(), entries);
  assert.equal(all.omitted, 0);
  assert.ok(!all.html.includes('earlier messages are not included'));
});

test('renderTranscriptHtml: truncation notes, time zones and empty input', () => {
  assert.ok(renderTranscriptHtml({ ...baseMeta(), truncated: 'limit' }, [entry(1)]).html.includes('Only the last 5000 messages'));
  assert.ok(renderTranscriptHtml({ ...baseMeta(), truncated: 'time' }, [entry(1)]).html.includes('time limit'));
  const zoned = renderTranscriptHtml(baseMeta(), [entry(1)], { timeZone: 'Asia/Tokyo' }).html;
  assert.ok(zoned.includes('Asia/Tokyo'));
  assert.ok(zoned.includes('12:06'), 'times are shown in the zone (03:06 UTC is 12:06 in Tokyo)');
  assert.ok(renderTranscriptHtml(baseMeta(), [entry(1)], { timeZone: 'Not/AZone' }).html.includes('<dd>UTC</dd>'));
  const empty = renderTranscriptHtml(baseMeta(), []);
  assert.equal(empty.included, 0);
  assert.ok(empty.html.includes('0 message(s)'));
  const sys = renderTranscriptHtml(baseMeta(), [entry(2, { system: true, systemLabel: 'pinned a message', content: '', bot: true })]).html;
  assert.ok(sys.includes('pinned a message'));
  assert.ok(sys.includes('class="msg bot sys"'));
});

test('normalizeMessage resolves mentions in a single pass', () => {
  const guild = {
    roles: { cache: new Map([['2', { name: 'mods' }]]) },
    channels: { cache: new Map([['3', { name: 'general' }]]) },
    members: { cache: new Map() },
  };
  const message = {
    id: 'm1',
    content: 'hi <@1> <@&2> <#3> <:pog:4> <t:0:R>',
    mentions: { users: new Map([['1', { username: 'bob' }]]), members: new Map() },
    author: { id: '1', username: 'bob', tag: 'bob#0', bot: false },
    createdTimestamp: 0,
    embeds: [],
    attachments: new Map(),
    stickers: new Map(),
    system: false,
  };
  const e = normalizeMessage(message, guild);
  assert.equal(e.content, 'hi @bob @mods #general :pog: 1970-01-01T00:00:00.000Z');
  assert.equal(e.authorName, 'bob');
  assert.equal(e.bot, false);

  // A substituted name is not expanded again.
  const tricky = normalizeMessage({
    ...message, content: '<@1> <#3>', mentions: { users: new Map([['1', { username: '<#3>' }]]), members: new Map() },
  }, guild);
  assert.equal(tricky.content, '@<#3> #general');

  // Unknown entities, display names and attachments.
  const rich = normalizeMessage({
    ...message,
    content: '<@9> <@&8> <#7> <t:99999999999999999:f>',
    mentions: { users: new Map(), members: new Map() },
    member: { displayName: 'Bobby' },
    attachments: new Map([['a', { name: 'a.png', url: 'https://cdn.discordapp.com/a.png', size: 5 }], ['b', { name: 'b', url: 'javascript:alert(1)', size: 1 }]]),
    stickers: new Map([['s', { name: 'wave' }]]),
    embeds: [{ title: 't', description: 'd', fields: [{ name: 'n', value: 'v' }], footer: { text: 'f' } }],
    reference: { messageId: 'm0' },
    editedTimestamp: 5,
    system: true,
    type: 6,
  }, guild);
  assert.equal(rich.content, '@9 @deleted-role #deleted-channel <t:99999999999999999:f>');
  assert.equal(rich.authorName, 'Bobby');
  assert.deepEqual(rich.attachments.map((a) => a.name), ['a.png'], 'a non-http attachment is dropped');
  assert.deepEqual(rich.stickers, ['wave']);
  assert.equal(rich.embeds[0].footer, 'f');
  assert.equal(rich.replyToId, 'm0');
  assert.equal(rich.systemLabel, 'pinned a message');
  assert.equal(normalizeMessage({ ...message, content: 'x'.repeat(5000) }, guild).content.length, 4000);
});

// ── 14. customIds ─────────────────────────────────────────────────────────────

test('customIds fit and round-trip', () => {
  const tpl = buildCustomId('ticket-modal', 'tpl', 'p'.repeat(25), 'intro', 'o'.repeat(25));
  assert.ok(tpl.length <= 100);
  assert.equal(parseCustomId(tpl).params[0], 'tpl');
  assert.equal(parseCustomId(tpl).params.length, 4);
  const reason = buildCustomId('ticket-modal', 'p'.repeat(25), 'o'.repeat(25));
  assert.equal(parseCustomId(reason).params.length, 2);

  const modal = buildReasonModal('p'.repeat(25), 'o'.repeat(25), 'x'.repeat(80)).toJSON();
  assert.ok(modal.title.length <= 45);
  const inputs = modal.components.flatMap((r) => r.components);
  assert.equal(inputs.length, 1);
  assert.equal(inputs[0].custom_id, 'ticket-reason');
  assert.equal(inputs[0].max_length, 1000);
  assert.equal(inputs[0].min_length, 1);
  assert.equal(inputs[0].required, true);
  assert.equal(modal.custom_id, reason);
});

// ── 15. Discord errors ────────────────────────────────────────────────────────

test('describeDiscordError and isCategoryFullError', () => {
  assert.equal(describeDiscordError({ code: 50013 }), "I'm missing permissions there");
  assert.equal(describeDiscordError({ code: 50001 }), "I'm missing permissions there");
  assert.equal(describeDiscordError({ code: 30013 }), 'the server has reached 500 channels');
  assert.equal(describeDiscordError({ code: 10003 }), 'the channel no longer exists');
  assert.equal(describeDiscordError({ code: 10008 }), 'the message no longer exists');
  assert.equal(describeDiscordError({ code: 50007 }), 'the user does not accept DMs');
  assert.equal(describeDiscordError({ code: 50035 }), 'Discord rejected the request');
  assert.equal(describeDiscordError({ status: 429 }), 'Discord is rate limiting me');
  assert.equal(describeDiscordError({ name: 'TimeoutError' }), 'Discord took too long to answer');
  assert.equal(describeDiscordError({ code: 12345 }), 'Discord error 12345');
  assert.equal(describeDiscordError(null), 'Discord error unknown');
  const full = { code: 50035, rawError: { errors: { parent_id: { _errors: [{ code: 'CHANNEL_PARENT_MAX_CHANNELS' }] } } } };
  assert.equal(isCategoryFullError(full), true);
  assert.equal(describeDiscordError(full), 'the category is full (50 channels)');
  assert.equal(isCategoryFullError({ code: 50035, rawError: { errors: {} } }), false);
  assert.equal(isCategoryFullError({ code: 10003 }), false);
});

test('error payloads: every code renders an embed and never pings', () => {
  const codes = [
    'PANEL_GONE', 'OPTION_GONE', 'OPEN_LIMIT', 'BOT_PERMS', 'GUILD_FULL', 'CREATE_FAILED', 'SLOW', 'THROTTLED', 'MODULE_OFF', 'DB_ERROR',
    'NOT_TICKET', 'TICKET_NOT_FOUND', 'TICKET_DELETED', 'NOT_STAFF', 'NOT_OWNER_OR_STAFF', 'OWN_TICKET', 'ALREADY_CLAIMED_SELF', 'ALREADY_CLAIMED',
    'IS_CLOSED', 'ALREADY_CLOSED', 'NOT_CLOSED', 'CLOSE_FIRST', 'CONFLICT', 'DELETING', 'RENAME_LIMIT', 'BAD_NAME', 'ADD_BOT', 'NOT_MEMBER',
    'IS_OWNER', 'ALREADY_ADDED', 'ADDED_LIMIT', 'NOT_ADDED', 'TRANSCRIPT_THROTTLED', 'TRANSCRIPT_FAILED', 'TRANSCRIPT_CHANNEL',
    'TRANSCRIPT_ABORT', 'DELETE_FAILED', 'UNKNOWN', 'NOT_A_CODE',
  ];
  for (const code of codes) {
    const p = errorPayload({ code, data: { ids: ['1'], max: 1, names: 'Manage Channels', retryAt: new Date(), reason: 'why', channelId: '5', userId: '7', claimedById: '8' } });
    assert.equal(p.embeds.length, 1, code);
    assert.deepEqual(p.allowedMentions, { parse: [] }, code);
    assert.ok(p.embeds[0].data.title, code);
  }
  assert.ok(errorPayload({ code: 'OPEN_LIMIT', data: { ids: ['55'], max: 1 } }).embeds[0].data.description.includes('<#55>'));
  const many = errorPayload({ code: 'OPEN_LIMIT', data: { ids: ['5', `${PENDING_PREFIX}x`], max: 2 } }).embeds[0].data.description;
  assert.ok(many.includes('2 open tickets') && many.includes('<#5>') && many.includes('being created'));
  // BOT_PERMS: the open wording, or the lifecycle wording when a channel is named.
  assert.ok(errorPayload({ code: 'BOT_PERMS', data: { names: 'Manage Channels', where: 'Tickets' } }).embeds[0].data.description.includes('to open tickets'));
  assert.ok(errorPayload({ code: 'BOT_PERMS', data: { names: 'Manage Channels', channelId: '9' } }).embeds[0].data.description.includes('<#9>'));
  assert.ok(errorPayload({ code: 'IS_OWNER', data: { op: 'remove' } }).embeds[0].data.description.includes("can't remove"));
  assert.ok(errorPayload({ code: 'IS_OWNER', data: { op: 'add' } }).embeds[0].data.title.includes('Already In Ticket'));

  for (const code of [
    'NAME_TAKEN', 'PANEL_LIMIT', 'PANEL_NOT_FOUND', 'BAD_FORMAT', 'BAD_PANEL_NAME', 'CHANNEL_UNUSABLE', 'TRANSCRIPT_CHANNEL_UNUSABLE', 'PERMS_SERVER',
    'PUBLISH_FAILED', 'BAD_LABEL', 'OPTION_LIMIT', 'OPTION_DUP', 'BAD_EMOJI', 'OPTION_NOT_FOUND', 'LAST_OPTION', 'EVERYONE_ROLE', 'ALREADY_MANAGER',
    'NOT_MANAGER', 'MANAGER_LIMIT', 'CATEGORY_PERMS', 'CONFLICT_CHANNEL', 'CONFLICT_CATEGORY', 'INVALID_MESSAGE', 'BAD_COMBO', 'NOT_ADMIN',
    'FORM_EXPIRED', 'PANEL_DELETED', 'SLOW', 'WHATEVER',
  ]) {
    const p = panelErrorPayload(code, { name: 'N', error: 'e', u: { missing: ['ManageRoles'] }, id: '1', missing: ['ManageRoles'], reason: 'r', label: 'L', roleId: '2', panel: 'P', names: 'X', category: 'C', errors: ['a', 'b'] });
    assert.equal(p.embeds.length, 1, code);
    assert.deepEqual(p.allowedMentions, { parse: [] }, code);
  }
  assert.ok(panelErrorPayload('PANEL_LIMIT').embeds[0].data.description.includes(String(LIMITS.TICKET_PANELS)));
  assert.ok(panelErrorPayload('MANAGER_LIMIT').embeds[0].data.description.includes(String(LIMITS.TICKET_MANAGER_ROLES)));
  assert.ok(panelErrorPayload('OPTION_LIMIT').embeds[0].data.description.includes(String(LIMITS.TICKET_PANEL_OPTIONS)));
  assert.ok(panelErrorPayload('CHANNEL_UNUSABLE', { u: { missing: [], reason: 'Channel not found' }, id: '1' }).embeds[0].data.description.includes('Channel not found'));
  assert.equal(panelErrorPayload('INVALID_MESSAGE', { errors: ['a', 'b'] }).embeds[0].data.description, '• a\n• b');

  assert.equal(refreshNote({ ok: true }), '');
  assert.equal(refreshNote(null), '');
  assert.ok(refreshNote({ ok: false, code: 'GONE' }).includes('/ticket-panel publish'));
  assert.ok(refreshNote({ ok: false, code: 'NO_ACCESS', channelId: '5' }).includes('<#5>'));
  assert.ok(refreshNote({ ok: false, code: 'ERROR', reason: 'boom' }).includes('boom'));
});

test('notices: titles, mentions and buttons', () => {
  const id = 't'.repeat(25);
  const claim = buildNotice('claim', { actorId: 'a' }, null);
  assert.ok(claim.embeds[0].data.description.includes('<@a> is handling'));
  assert.ok(buildNotice('claim', { actorId: 'a', previousId: 'b' }, null).embeds[0].data.description.includes('took over this ticket from <@b>'));
  const close = buildNotice('close', { actorId: 'a', ticketId: id, reason: 'done' }, null);
  assert.equal(close.embeds[0].data.fields[0].name, 'Reason');
  assert.equal(close.components[0].toJSON().components.length, 3);
  assert.equal(buildNotice('close', { actorId: 'a', ticketId: id }, null).embeds[0].data.fields, undefined);
  const reopen = buildNotice('reopen', { actorId: 'a', ticketId: id, claimed: true }, null);
  assert.equal(reopen.components[0].toJSON().components[1].disabled, true);
  assert.deepEqual(buildNotice('add', { actorId: 'a', userId: 'u' }, null).allowedMentions, { parse: [], users: ['u'] }, 'only the added member is pinged');
  assert.deepEqual(buildNotice('remove', { actorId: 'a', userId: 'u' }, null).allowedMentions, { parse: [] });
  assert.ok(buildNotice('rename', { actorId: 'a', channelId: 'c' }, null).embeds[0].data.description.includes('<#c>'));
  const del = buildNotice('delete', { actorId: 'a', transcriptChannelId: 't', dm: true }, null).embeds[0].data.description;
  assert.ok(del.includes('5 seconds') && del.includes('<#t>') && del.includes('DM'));
});

// ── 16. Plugin ────────────────────────────────────────────────────────────────

test('plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('tickets')), []);
  assert.ok(plugins.some((p) => p.id === 'tickets'));

  const { plugin } = pluginMod;
  assert.equal(plugin.id, 'tickets');
  assert.equal(plugin.module, 'tickets');
  assert.equal(plugin.hooks.length, 1);
  const [hook] = plugin.hooks;
  assert.equal(hook.event, 'channelDelete');
  assert.equal(hook.id, 'tickets');
  assert.equal(hook.always, true);
  assert.ok(HOOK_ORDER.channelDelete.includes('tickets'));
  assert.equal(typeof hook.run, 'function');
  assert.equal(hook.filter({ channel: { type: ChannelType.GuildText } }), true);
  assert.equal(hook.filter({ channel: { type: ChannelType.GuildCategory } }), true);
  assert.equal(hook.filter({ channel: { type: ChannelType.GuildAnnouncement } }), true);
  assert.equal(hook.filter({ channel: { type: ChannelType.GuildVoice } }), false);
  assert.equal(hook.filter({}), false);

  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix]), [['button', 'ticket'], ['select', 'ticket-select'], ['modal', 'ticket-modal']]);
  assert.equal(plugin.components.find((c) => c.prefix === 'ticket-modal').module, null);
  assert.equal(plugin.components.find((c) => c.prefix === 'ticket').module, undefined, 'buttons and selects use the plugin module gate');
  for (const c of plugin.components) assert.equal(typeof c.handler, 'function');
  assert.deepEqual(plugin.jobs, []);
});

// ── 17. Command payloads ──────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('command payloads', () => {
  const ticket = buildCommandPayload(ticketCmd);
  assert.equal(ticket.name, 'ticket');
  assert.equal(ticket.options.length, 8);
  assert.ok(ticket.options.every((o) => o.type === 1));
  assert.deepEqual(ticket.contexts, [0]);
  assert.equal(ticket.default_member_permissions ?? null, null);
  assert.ok(charCount(ticket) <= 800, `/ticket is ${charCount(ticket)} characters`);
  assert.equal(ticketCmd.module, 'tickets');
  assert.equal(ticketCmd.gated, true);
  assert.equal(ticketCmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.equal(ticketCmd.allowInDMs, undefined);
  assert.equal(typeof ticketCmd.execute, 'function');

  const panel = buildCommandPayload(panelCmd);
  assert.equal(panel.name, 'ticket-panel');
  assert.equal(panel.options.filter((o) => o.type === 1).length, 8);
  const groups = panel.options.filter((o) => o.type === 2);
  assert.deepEqual(groups.map((g) => [g.name, g.options.length]), [['option', 4], ['managers', 2]]);
  assert.equal(panel.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.ok(charCount(panel) <= 3400, `/ticket-panel is ${charCount(panel)} characters`);
  assert.equal(panelCmd.module, 'tickets');
  assert.equal(panelCmd.gated, false);
  assert.equal(panelCmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(typeof panelCmd.execute, 'function');
  assert.equal(typeof panelCmd.autocomplete, 'function');

  // Every autocomplete option is served by autocomplete(), and none also has choices.
  const walk = (opts) => opts.flatMap((o) => (o.options ? walk(o.options) : [o]));
  for (const o of walk(panel.options)) {
    if (o.autocomplete) assert.ok(['panel', 'option'].includes(o.name), o.name);
    assert.ok(!(o.autocomplete && o.choices));
  }
  // The literals documented in the spec.
  const find = (name, sub) => panel.options.find((o) => o.name === sub).options.find((o) => o.name === name);
  assert.deepEqual(find('style', 'create').choices.map((c) => c.value), ['BUTTONS', 'SELECT']);
  assert.deepEqual(panel.options.find((o) => o.name === 'option').options.find((s) => s.name === 'add').options.find((o) => o.name === 'color').choices.map((c) => c.value), [1, 2, 3, 4]);
  assert.ok(panel.options.find((o) => o.name === 'managers').description.includes(String(LIMITS.TICKET_MANAGER_ROLES)));
  assert.ok(panel.options.find((o) => o.name === 'option').options.find((s) => s.name === 'add').description.includes(String(LIMITS.TICKET_PANEL_OPTIONS)));
});

test('registry scan: the only literal customId is ticket-reason, in the service', () => {
  const files = ['src/services/ticketService.js', 'src/commands/community/ticket.js', 'src/commands/admin/ticket-panel.js', 'src/plugins/tickets.js'];
  const found = [];
  for (const f of files) {
    const text = readFileSync(join(ROOT, f), 'utf8');
    for (const re of [/setCustomId\(\s*[`'"]([^`'"]+)/g, /\bcustom_?[iI]d\s*:\s*[`'"]([^`'"]+)/g]) {
      for (const m of text.matchAll(re)) found.push(`${f}:${m[1]}`);
    }
  }
  assert.deepEqual(found, ['src/services/ticketService.js:ticket-reason']);
});

test('no console, no cache.* for settings, no lockPermissions() and every setParent passes lockPermissions: false', () => {
  const text = readFileSync(join(ROOT, 'src/services/ticketService.js'), 'utf8');
  assert.ok(!/\bconsole\./.test(text));
  assert.ok(!/cache\.(del|set|get)\(/.test(text.replace(/channels\.cache\.get|roles\.cache\.get|members\.cache\.get|emojis\.cache\.get|guild\.channels\.cache\.get/g, '')));
  assert.ok(!/\.lockPermissions\(/.test(text));
  const calls = [...text.matchAll(/setParent\(([^)]*)\)/g)].map((m) => m[1]);
  assert.ok(calls.length >= 1);
  for (const c of calls) assert.ok(c.includes('lockPermissions: false'), c);
  assert.ok(!/ManageRoles['"]\s*\]\s*,?\s*\/\/\s*overwrite/.test(text));
});

test('RAW_SQL builders return Prisma.Sql without a bare now()', () => {
  assert.equal(RAW_SQL.length, 5);
  for (const { name, build } of RAW_SQL) {
    const sql = build();
    assert.ok(sql instanceof Prisma.sql``.constructor, name);
    assert.ok(!/\bnow\(\)/i.test(sql.sql), name);
  }
});

// ── Component handlers without I/O ────────────────────────────────────────────

function fakeComponentInteraction(over = {}) {
  const calls = [];
  const i = {
    id: 'i1', customId: 'ticket:bogus', createdTimestamp: Date.now(), guildId: G(1), user: fakeUser, replied: false, deferred: false,
    inCachedGuild: () => true, isAnySelectMenu: () => false, isFromMessage: () => false,
    reply: async (p) => { calls.push(['reply', p]); i.replied = true; return { resource: { message: {} } }; },
    update: async (p) => { calls.push(['update', p]); i.replied = true; },
    followUp: async (p) => { calls.push(['followUp', p]); return {}; },
    showModal: async (m) => { calls.push(['showModal', m]); },
    deferReply: async () => { calls.push(['deferReply']); i.deferred = true; },
    deferUpdate: async () => { calls.push(['deferUpdate']); i.deferred = true; },
    guild: { id: G(1), channels: { cache: new Map() } },
    message: { components: [] },
    ...over,
  };
  i.calls = calls;
  return i;
}

test('buttons: an unknown action and a DM click answer without I/O', async () => {
  const stale = fakeComponentInteraction({ customId: 'ticket:bogus' });
  await svc.handleTicketButton(stale, {});
  assert.equal(stale.calls.length, 1);
  assert.equal(stale.calls[0][1].content, 'This button is no longer active.');
  assert.equal(stale.calls[0][1].ephemeral, true);

  const dm = fakeComponentInteraction({ customId: `ticket:close:${'t'.repeat(25)}`, inCachedGuild: () => false });
  await svc.handleTicketButton(dm, {});
  assert.equal(dm.calls[0][1].content, 'This only works in a server.');

  const dmOpen = fakeComponentInteraction({ customId: `ticket:open:${'p'.repeat(25)}:${'o'.repeat(25)}`, inCachedGuild: () => false });
  await svc.handleTicketButton(dmOpen, {});
  assert.equal(dmOpen.calls[0][1].content, 'This only works in a server.');
});

test('open click: with no acknowledgement budget and no memo the answer is "Try Again" (no database call)', async () => {
  const late = fakeComponentInteraction({
    customId: `ticket:open:${'p'.repeat(25)}:${'o'.repeat(25)}`,
    createdTimestamp: Date.now() - 10_000,
  });
  await svc.handleTicketButton(late, {});
  assert.equal(late.calls.length, 1);
  assert.equal(late.calls[0][0], 'reply');
  assert.equal(late.calls[0][1].embeds[0].data.title, '❌  Try Again');
  assert.equal(late.calls[0][1].ephemeral, true);

  // A dropdown is reset with update() first, then the error is an ephemeral follow-up.
  const select = fakeComponentInteraction({
    customId: `ticket-select:${'p'.repeat(25)}`, values: ['o'.repeat(25)], isAnySelectMenu: () => true, createdTimestamp: Date.now() - 10_000,
    message: { components: [{ type: 1 }] },
  });
  await svc.handleTicketSelect(select, {});
  assert.deepEqual(select.calls.map((c) => c[0]), ['update', 'followUp']);
  assert.equal(select.calls[1][1].ephemeral, true);

  const empty = fakeComponentInteraction({ customId: `ticket-select:${'p'.repeat(25)}`, values: [], isAnySelectMenu: () => true });
  await svc.handleTicketSelect(empty, {});
  assert.equal(empty.calls[1][1].embeds[0].data.title, '❌  Option Not Found');
});

test('/ticket-panel message: the option setting needs the intro type; a late interaction gets "Try Again"; no deferral', async () => {
  const mk = (strings, over = {}) => {
    const calls = [];
    const i = {
      id: 'x', guildId: G(1), createdTimestamp: Date.now(), replied: false, deferred: false,
      options: {
        getSubcommandGroup: () => null, getSubcommand: () => 'message',
        getString: (name) => strings[name] ?? null,
      },
      reply: async (p) => { calls.push(['reply', p]); i.replied = true; return { resource: { message: {} } }; },
      deferReply: async () => { calls.push(['deferReply']); },
      showModal: async () => { calls.push(['showModal']); },
      ...over,
    };
    i.calls = calls;
    return i;
  };
  const combo = mk({ panel: 'p', option: 'o' });
  await panelCmd.execute(combo, {});
  assert.deepEqual(combo.calls.map((c) => c[0]), ['reply']);
  assert.equal(combo.calls[0][1].embeds[0].data.title, '❌  Invalid Options');
  assert.equal(combo.calls[0][1].ephemeral, true);

  const late = mk({ panel: 'p', type: 'panel' }, { createdTimestamp: Date.now() - 10_000 });
  await panelCmd.execute(late, {});
  assert.deepEqual(late.calls.map((c) => c[0]), ['reply'], 'no deferReply and no database call');
  assert.equal(late.calls[0][1].embeds[0].data.title, '❌  Try Again');
});

test('template modal: a non-admin is refused before anything else', async () => {
  const calls = [];
  const i = {
    customId: `ticket-modal:tpl:${'p'.repeat(25)}:panel`, guildId: G(1), createdTimestamp: Date.now(), replied: false, deferred: false,
    inCachedGuild: () => true,
    guild: { id: G(1) },
    member: { id: 'u', guild: { ownerId: 'o' }, permissions: { has: () => false }, roles: { cache: new Map() } },
    reply: async (p) => { calls.push(['reply', p]); i.replied = true; return { resource: { message: {} } }; },
    deferReply: async () => { calls.push(['deferReply']); },
  };
  await svc.handleTicketModal(i, {});
  assert.deepEqual(calls.map((c) => c[0]), ['reply']);
  assert.equal(calls[0][1].embeds[0].data.title, '❌  Insufficient Permissions');
  assert.equal(calls[0][1].ephemeral, true);
});

test('constants: limits agree with the foundation', () => {
  assert.equal(MAX_ADDED_USERS, 20);
  assert.ok(OPEN_REQUIRED_PERMS.includes('ManageRoles'), 'the bot needs ManageRoles itself');
  assert.ok(!MEMBER_ALLOW.includes('ManageRoles') && !BOT_ALLOW.includes('ManageRoles'));
  assert.ok(Collection);
  assert.ok(Prisma && prisma && ensureGuildSettings && updateSettings);
});

// ═══════════════════════════════════════════════════════════════════════════════
// Database cases (TEST_DATABASE_URL): fake Discord objects around the real service and a migrated Postgres.
// ═══════════════════════════════════════════════════════════════════════════════

let snowflake = 800000000000000000n;
const nextId = () => String(snowflake++);
const discordError = (code, message = 'Discord error') => Object.assign(new Error(message), { code });

const realNow = Date.now;
/** Moves the clock forward so the 5 s open throttle lets sequential opens by the same member through. */
const skewClock = (ms) => { const base = Date.now; Date.now = () => base() + ms; };
const restoreClock = () => { Date.now = realNow; };

function makeChannel(guild, { id = nextId(), name = 'chan', type = ChannelType.GuildText, parent = null, overwrites = [], topic = null } = {}) {
  const ch = {
    id, name, type, parentId: parent, topic, guild, overwrites, sent: [], calls: [], store: [],
    permissionsFor: () => guild.botPerms,
    isTextBased: () => type === ChannelType.GuildText,
    isThread: () => false,
    send: async (payload) => {
      if (ch.sendError) throw ch.sendError;
      ch.sent.push(payload);
      const msg = {
        id: nextId(), url: `https://discord.com/channels/${guild.id}/${id}/m`,
        edit: async (p) => { ch.calls.push(['msg.edit', p]); msg.edited = p; },
        delete: async () => { ch.calls.push(['msg.delete']); },
      };
      ch.published = msg;
      return msg;
    },
    setParent: async (pid, opts) => { ch.calls.push(['setParent', pid, opts]); ch.parentId = pid; return ch; },
    setName: async (n) => { ch.calls.push(['setName', n]); ch.name = n; return ch; },
    delete: async (reason) => {
      if (ch.deleteError) throw ch.deleteError;
      ch.calls.push(['delete', reason]);
      guild.channels.cache.delete(id);
    },
    permissionOverwrites: {
      edit: async (who, perms, opts) => { if (ch.overwriteError) throw ch.overwriteError; ch.calls.push(['overwrite.edit', who, perms, opts]); },
      delete: async (who, reason) => { if (ch.overwriteError) throw ch.overwriteError; ch.calls.push(['overwrite.delete', who, reason]); },
    },
    messages: {
      fetch: async (arg) => {
        if (typeof arg === 'string') {
          if (ch.published?.id === arg) return ch.published;
          throw discordError(10008, 'Unknown Message');
        }
        const before = arg?.before;
        let list = [...ch.store].reverse(); // newest first
        if (before) list = list.slice(list.findIndex((m) => m.id === before) + 1);
        return new Collection(list.slice(0, arg?.limit ?? 50).map((m) => [m.id, m]));
      },
      delete: async (mid) => { ch.calls.push(['messages.delete', mid]); },
    },
  };
  return ch;
}

function makeWorld(guildId, { botCan = true } = {}) {
  const dms = [];
  const created = [];
  const guild = {
    id: guildId, name: 'Fake Guild', available: true, ownerId: 'owner-x', botPerms: { has: () => botCan },
    client: {
      user: { id: 'bot1', tag: 'bot#0001' }, emojis: { cache: new Collection() },
      users: { fetch: async (id) => ({ id, tag: `u${id}#0`, send: async (p) => { if (guild.dmError) throw guild.dmError; dms.push([id, p]); } }) },
    },
    members: { me: { id: 'bot1', permissions: { has: () => botCan } }, cache: new Collection() },
    roles: { cache: new Collection() },
    emojis: { cache: new Collection() },
    createErrors: [],
    fetchError: null,
    channels: {
      cache: new Collection(),
      create: async (opts) => {
        created.push(opts);
        if (guild.createErrors.length) throw guild.createErrors.shift();
        if (opts.parent && guild.fullParents?.has(opts.parent)) {
          throw Object.assign(discordError(50035), { rawError: { errors: { parent_id: { _errors: [{ code: 'CHANNEL_PARENT_MAX_CHANNELS' }] } } } });
        }
        const ch = makeChannel(guild, { name: opts.name, type: opts.type, parent: opts.parent ?? null, overwrites: opts.permissionOverwrites ?? [], topic: opts.topic ?? null });
        guild.channels.cache.set(ch.id, ch);
        return ch;
      },
      fetch: async (id) => {
        const hit = guild.channels.cache.get(id);
        if (hit) return hit;
        throw guild.fetchError ?? discordError(10003, 'Unknown Channel');
      },
    },
  };
  return {
    guild, client: guild.client, dms, created,
    role: (id, { mentionable = false } = {}) => { guild.roles.cache.set(id, { id, name: `role-${id}`, mentionable }); return id; },
    category: (name = 'Cat') => { const c = makeChannel(guild, { name, type: ChannelType.GuildCategory }); guild.channels.cache.set(c.id, c); return c; },
    text: (name = 'text') => { const c = makeChannel(guild, { name }); guild.channels.cache.set(c.id, c); return c; },
    user: (id, over = {}) => ({ id, username: `user${id}`, globalName: null, tag: `user${id}#0`, bot: false, ...over }),
    member: (id, { roles = [], admin = false, mod = false } = {}) => {
      const user = { id, username: `user${id}`, globalName: null, tag: `user${id}#0`, bot: false };
      const m = {
        id, displayName: `User ${id}`, user, guild,
        roles: { cache: new Collection(roles.map((r) => [r, { id: r }])) },
        permissions: { has: (p) => (admin && (p === PermissionFlagsBits.Administrator || p === PermissionFlagsBits.ManageGuild)) || (mod && p === PermissionFlagsBits.ModerateMembers) },
      };
      guild.members.cache.set(id, m);
      return m;
    },
    openInteraction: (member) => ({ id: nextId(), user: member.user, member, guild, guildId: guild.id, createdTimestamp: Date.now() }),
  };
}

async function newPanel(world, over = {}) {
  const r = await svc.createPanel({ guildId: world.guild.id, name: over.name ?? `Panel ${nextId()}`, ...over.create });
  assert.equal(r.ok, true);
  if (over.data) {
    const u = await svc.updatePanelFields(r.panel.id, world.guild.id, over.data);
    assert.equal(u.ok, true);
  }
  return svc.findPanel(world.guild.id, r.panel.id);
}

const ticketRow = (id) => prisma.ticket.findUnique({ where: { id } });

test('DB: every RAW_SQL statement executes against the migrated schema', { skip: skipDb }, async () => {
  for (const { name, build } of RAW_SQL) {
    const rows = await prisma.$queryRaw(build());
    assert.ok(Array.isArray(rows), name);
  }
  const [{ next }] = await prisma.$queryRaw(RAW_SQL[1].build());
  assert.equal(typeof next, 'number');
});

test('DB: createPanel enforces the name and the cap, also under parallel creates', { skip: skipDb }, async () => {
  const first = await svc.createPanel({ guildId: G(1), name: 'Support' });
  assert.equal(first.ok, true);
  assert.equal(first.panel.options.length, 1);
  assert.equal(first.panel.options[0].label, 'Open Ticket');
  assert.equal(first.panel.options[0].emoji, '✉️');
  assert.equal(first.panel.options[0].buttonStyle, 1);
  assert.equal(first.panel.style, 'BUTTONS');
  assert.equal(first.panel.maxOpenPerUser, 1);
  assert.equal(first.panel.channelNameFormat, 'ticket-{number}');

  const dup = await svc.createPanel({ guildId: G(1), name: 'SUPPORT' });
  assert.deepEqual([dup.ok, dup.code], [false, 'NAME_TAKEN']);

  for (let i = 0; i < 21; i++) assert.equal((await svc.createPanel({ guildId: G(1), name: `P${i}` })).ok, true);
  assert.equal(await prisma.ticketPanel.count({ where: { guildId: G(1) } }), 22);
  const results = await Promise.all(Array.from({ length: 6 }, (_, i) => svc.createPanel({ guildId: G(1), name: `Race${i}` })));
  assert.equal(results.filter((r) => r.ok).length, 3, 'exactly the three free slots');
  assert.equal(results.filter((r) => r.code === 'PANEL_LIMIT').length, 3);
  assert.equal(await prisma.ticketPanel.count({ where: { guildId: G(1) } }), LIMITS.TICKET_PANELS);

  // Another guild is independent.
  assert.equal((await svc.createPanel({ guildId: G(2), name: 'Support' })).ok, true);
});

test('DB: updatePanelFields, findPanel and the autocomplete queries', { skip: skipDb }, async () => {
  const w = makeWorld(G(3));
  const a = await newPanel(w, { name: 'Alpha Support' });
  const b = await newPanel(w, { name: 'Beta Billing', create: { style: 'SELECT' } });
  assert.equal(b.style, 'SELECT');

  assert.equal((await svc.findPanel(G(3), 'alpha support'))?.id, a.id, 'by name, case-insensitive');
  assert.equal((await svc.findPanel(G(3), a.id))?.id, a.id, 'by id');
  assert.equal(await svc.findPanel(G(4), a.id), null, 'guild-scoped');
  assert.equal(await svc.findPanel(G(3), 'nope'), null);

  assert.deepEqual(await svc.updatePanelFields(a.id, G(3), { name: 'BETA billing' }), { ok: false, code: 'NAME_TAKEN', data: { name: 'BETA billing' } });
  assert.equal((await svc.updatePanelFields(a.id, G(3), { name: 'ALPHA SUPPORT' })).ok, true, 'a case change of its own name is fine');
  assert.equal((await svc.updatePanelFields(a.id, G(3), { maxOpenPerUser: 3, askReason: true, channelNameFormat: '{user.name}-{number}' })).ok, true);
  const again = await svc.findPanel(G(3), a.id);
  assert.deepEqual([again.maxOpenPerUser, again.askReason, again.channelNameFormat], [3, true, '{user.name}-{number}']);
  assert.equal((await svc.updatePanelFields(a.id, G(4), { askReason: false })).code, 'PANEL_NOT_FOUND', 'another guild cannot write it');

  const all = await svc.findPanelsForAutocomplete(G(3), '');
  assert.deepEqual(all.map((p) => p.name), ['ALPHA SUPPORT', 'Beta Billing']);
  assert.deepEqual((await svc.findPanelsForAutocomplete(G(3), 'bill')).map((p) => p.name), ['Beta Billing']);
  assert.deepEqual(await svc.findPanelsForAutocomplete(G(3), 'zzz'), []);

  const byId = await svc.findOptionsForAutocomplete(G(3), a.id, '');
  assert.deepEqual(byId.map((o) => o.label), ['Open Ticket']);
  const byName = await svc.findOptionsForAutocomplete(G(3), 'alpha support', 'open');
  assert.equal(byName.length, 1);
  assert.deepEqual(await svc.findOptionsForAutocomplete(G(4), a.id, ''), [], 'guild-scoped through the relation filter');
  assert.deepEqual(await svc.findOptionsForAutocomplete(G(3), a.id, 'zzz'), []);
});

test('DB: options are capped, unique and ordered under their lock', { skip: skipDb }, async () => {
  const w = makeWorld(G(5));
  const panel = await newPanel(w, { name: 'Opts' });
  const opt = (label, extra = {}) => ({ label, emoji: null, description: null, buttonStyle: 1, categoryId: null, ...extra });

  const added = await svc.addOption(panel.id, G(5), opt('Billing', { emoji: '💰', description: 'Money', buttonStyle: 3 }));
  assert.equal(added.ok, true);
  assert.equal(added.option.position, 1);
  assert.equal(added.count, 2);
  assert.deepEqual(await svc.addOption(panel.id, G(5), opt('BILLING')), { ok: false, code: 'OPTION_DUP', data: { label: 'BILLING' } });
  assert.equal((await svc.addOption(panel.id, G(6), opt('X'))).code, 'PANEL_NOT_FOUND');

  // 22 more fit (up to 25 in total); the rest are refused, also in parallel.
  for (let i = 0; i < 22; i++) assert.equal((await svc.addOption(panel.id, G(5), opt(`O${i}`))).ok, true);
  const race = await Promise.all(Array.from({ length: 4 }, (_, i) => svc.addOption(panel.id, G(5), opt(`R${i}`))));
  assert.equal(race.filter((r) => r.ok).length, 1);
  assert.equal(race.filter((r) => r.code === 'OPTION_LIMIT').length, 3);
  const full = await svc.findPanel(G(5), panel.id);
  assert.equal(full.options.length, LIMITS.TICKET_PANEL_OPTIONS);
  assert.deepEqual(full.options.map((o) => o.position), [...full.options.map((o) => o.position)].sort((a, b) => a - b));
  assert.equal(new Set(full.options.map((o) => o.position)).size, 25, 'positions are distinct');

  const billing = full.options.find((o) => o.label === 'Billing');
  assert.deepEqual(await svc.updateOption(panel.id, G(5), billing.id, { label: 'o1' }), { ok: false, code: 'OPTION_DUP', data: { label: 'o1' } });
  assert.equal((await svc.updateOption(panel.id, G(5), billing.id, { label: 'BILLING', buttonStyle: 4, description: null })).ok, true);
  assert.equal((await svc.updateOption(panel.id, G(5), 'nope', { label: 'Z' })).code, 'OPTION_NOT_FOUND');
  assert.equal((await prisma.ticketPanelOption.findUnique({ where: { id: billing.id } })).buttonStyle, 4);

  assert.equal((await svc.removeOption(panel.id, G(5), billing.id)).ok, true);
  assert.equal((await svc.removeOption(panel.id, G(5), billing.id)).code, 'OPTION_NOT_FOUND');
  const solo = await newPanel(w, { name: 'Solo' });
  assert.equal((await svc.removeOption(solo.id, G(5), solo.options[0].id)).code, 'LAST_OPTION');
});

test('DB: manager roles are added, removed, capped and pruned under a row lock', { skip: skipDb }, async () => {
  const w = makeWorld(G(7));
  const panel = await newPanel(w, { name: 'Mgr' });
  for (let i = 0; i < 12; i++) w.role(`r${i}`);

  const add = (id) => svc.updateManagers(panel.id, w.guild, (cur) => {
    if (cur.includes(id)) return { ok: false, code: 'ALREADY_MANAGER' };
    if (cur.length >= LIMITS.TICKET_MANAGER_ROLES) return { ok: false, code: 'MANAGER_LIMIT' };
    return { ok: true, roles: [...cur, id] };
  });
  const results = await Promise.all(Array.from({ length: 12 }, (_, i) => add(`r${i}`)));
  assert.equal(results.filter((r) => r.ok).length, 10, 'exactly the cap, none lost');
  assert.equal(results.filter((r) => r.code === 'MANAGER_LIMIT').length, 2);
  const stored = (await svc.findPanel(G(7), panel.id)).managerRoleIds;
  assert.equal(new Set(stored).size, 10);

  // A deleted role is pruned by the next write.
  w.guild.roles.cache.delete(stored[0]);
  const removed = await svc.updateManagers(panel.id, w.guild, (cur) => ({ ok: true, roles: cur.filter((id) => id !== stored[1]) }));
  assert.equal(removed.ok, true);
  assert.deepEqual(removed.roles, stored.slice(2));
  assert.deepEqual((await svc.findPanel(G(7), panel.id)).managerRoleIds, stored.slice(2));
  assert.equal((await add(stored[2])).code, 'ALREADY_MANAGER');
  assert.equal((await svc.updateManagers('missing', w.guild, () => ({ ok: true, roles: [] }))).code, 'PANEL_NOT_FOUND');
  assert.equal((await svc.updateManagers(panel.id, { ...w.guild, id: G(8) }, () => ({ ok: true, roles: [] }))).code, 'PANEL_NOT_FOUND', 'guild-scoped');

  // An unavailable guild is never used as evidence that a role is gone.
  w.guild.available = false;
  const keep = await svc.updateManagers(panel.id, w.guild, (cur) => ({ ok: true, roles: cur }));
  w.guild.available = true;
  assert.deepEqual(keep.roles, stored.slice(2));
});

// ── The open flow ─────────────────────────────────────────────────────────────

const open = async (w, panel, member, { reason = null, option = panel.options[0], expectOk = true } = {}) => {
  const r = await svc.openTicket({ interaction: w.openInteraction(member), client: w.client, guild: w.guild, panel, option, reason, settings: null });
  if (expectOk) assert.equal(r.ok, true, `open failed: ${r.code}`);
  return r;
};

test('DB: opening a ticket creates a private numbered channel in an on-demand category', { skip: skipDb }, async () => {
  const w = makeWorld(G(10));
  w.role('R1');
  const panel = await newPanel(w, { name: 'Support', data: { managerRoleIds: ['R1'] } });
  const alice = w.member('7001');

  const r = await open(w, panel, alice);
  assert.equal(r.ticket.number, 1);
  assert.deepEqual(r.notes, []);

  // The category was created first, then the ticket channel inside it.
  assert.equal(w.created.length, 2);
  const [catCreate, chCreate] = w.created;
  assert.equal(catCreate.name, 'Tickets');
  assert.equal(catCreate.type, ChannelType.GuildCategory);
  assert.deepEqual(catCreate.permissionOverwrites.map((o) => [o.id, o.type]), [[G(10), 0], ['bot1', 1], ['R1', 0]]);
  const category = w.guild.channels.cache.find((c) => c.name === 'Tickets');
  assert.equal(chCreate.name, 'ticket-0001');
  assert.equal(chCreate.type, ChannelType.GuildText);
  assert.equal(chCreate.parent, category.id);
  assert.deepEqual(chCreate.permissionOverwrites, buildTicketOverwrites({
    guildId: G(10), botId: 'bot1', ownerId: '7001', managerRoleIds: ['R1'], addedUserIds: [], closed: false,
  }));
  assert.ok(chCreate.topic.includes('#0001') && chCreate.topic.includes('7001') && chCreate.topic.includes('Open Ticket'));
  assert.ok(chCreate.reason.includes('Ticket #0001'));

  // The intro pings the opener only.
  const intro = r.channel.sent[0];
  assert.equal(intro.content, '<@7001>');
  assert.deepEqual(intro.allowedMentions, { parse: [], users: ['7001'], roles: [] });
  assert.equal(intro.embeds[0].data.title, 'Ticket #0001');
  assert.deepEqual(intro.components[0].toJSON().components.map((c) => c.custom_id), [`ticket:close:${r.ticket.id}`, `ticket:claim:${r.ticket.id}`]);

  const row = await ticketRow(r.ticket.id);
  assert.deepEqual([row.status, row.channelId, row.ownerId, row.panelId, row.optionId, row.guildId], ['OPEN', r.channel.id, '7001', panel.id, panel.options[0].id, G(10)]);
  assert.equal((await svc.findPanel(G(10), panel.id)).openCategoryId, category.id, 'the on-demand category is stored on the panel');

  // The next ticket reuses the category and the next number.
  const bob = w.member('7002');
  const r2 = await open(w, panel, bob);
  assert.equal(r2.ticket.number, 2);
  assert.equal(w.created.length, 3, 'only the channel was created');
  assert.equal(w.created[2].parent, category.id);
  assert.equal(w.created[2].name, 'ticket-0002');
});

test('DB: parallel opens by different members get distinct numbers', { skip: skipDb }, async () => {
  const w = makeWorld(G(11));
  const panel = await newPanel(w, { name: 'Par' });
  const members = Array.from({ length: 6 }, (_, i) => w.member(`72${i}`));
  const results = await Promise.all(members.map((m) => open(w, panel, m)));
  const numbers = results.map((r) => r.ticket.number).sort((a, b) => a - b);
  assert.deepEqual(numbers, [1, 2, 3, 4, 5, 6]);
  assert.equal(new Set(results.map((r) => r.channel.id)).size, 6);
  assert.equal(new Set(w.created.filter((c) => c.type === ChannelType.GuildCategory).map((c) => c.name)).size, 1, 'one category for all');
  assert.equal(w.created.filter((c) => c.type === ChannelType.GuildCategory).length, 1, 'the category is created once under the panel mutex');
});

test('DB: maxOpenPerUser holds under parallel clicks of one member', { skip: skipDb }, async () => {
  const w = makeWorld(G(12));
  const panel = await newPanel(w, { name: 'Cap' });
  const alice = w.member('7101');

  // Make every Date.now() jump past the 5 s double-click throttle so all three clicks reach the database transaction.
  let t = realNow();
  Date.now = () => (t += 5100);
  let results;
  try {
    results = await Promise.all([1, 2, 3].map(() => open(w, panel, alice, { expectOk: false })));
  } finally {
    restoreClock();
  }
  assert.equal(results.filter((r) => r.ok).length, 1, 'exactly one ticket');
  assert.deepEqual(results.filter((r) => !r.ok).map((r) => r.code), ['OPEN_LIMIT', 'OPEN_LIMIT']);
  assert.equal(await prisma.ticket.count({ where: { guildId: G(12), ownerId: '7101' } }), 1);

  // With a higher limit a second ticket is fine and the third is refused; the refusal names the open channels.
  await svc.updatePanelFields(panel.id, G(12), { maxOpenPerUser: 2 });
  const fresh = await svc.findPanel(G(12), panel.id);
  skewClock(3_600_000); // the race left a far-future throttle stamp for this member
  const second = await open(w, fresh, alice);
  assert.equal(second.ticket.number, 2);
  skewClock(6000);
  const third = await open(w, fresh, alice, { expectOk: false });
  assert.equal(third.code, 'OPEN_LIMIT');
  assert.equal(third.data.ids.length, 2);
  assert.equal(third.data.max, 2);

  // The cap is per panel: another panel is a separate budget.
  const other = await newPanel(w, { name: 'Cap2' });
  skewClock(6000);
  assert.equal((await open(w, other, alice)).ok, true);
  restoreClock();
});

test('DB: the double-click throttle answers THROTTLED', { skip: skipDb }, async () => {
  const w = makeWorld(G(13));
  const panel = await newPanel(w, { name: 'Thr', data: { maxOpenPerUser: 5 } });
  const alice = w.member('7201');
  assert.equal((await open(w, panel, alice)).ok, true);
  const again = await open(w, panel, alice, { expectOk: false });
  assert.equal(again.code, 'THROTTLED');
  assert.equal(await prisma.ticket.count({ where: { guildId: G(13) } }), 1);
});

test('DB: opening stores the reason and shows it in the intro', { skip: skipDb }, async () => {
  const w = makeWorld(G(14));
  const panel = await newPanel(w, { name: 'Why' });
  const r = await open(w, panel, w.member('7301'), { reason: 'My <b>order</b> is late' });
  assert.equal((await ticketRow(r.ticket.id)).reason, 'My <b>order</b> is late');
  const field = r.channel.sent[0].embeds[0].data.fields.find((f) => f.name === 'Reason');
  assert.equal(field.value, 'My <b>order</b> is late');

  // A template that uses {reason} itself gets no extra field; one without an embed falls back to the message text.
  const tpl = await newPanel(w, { name: 'Why2', data: { introTemplate: { content: 'Reason: {reason}', embed: { title: 'T #{number}' }, buttons: [] } } });
  const r2 = await open(w, tpl, w.member('7302'), { reason: 'abc' });
  assert.equal(r2.channel.sent[0].content, 'Reason: abc');
  assert.equal(r2.channel.sent[0].embeds[0].data.fields, undefined);
  const plain = await newPanel(w, { name: 'Why3', data: { introTemplate: { content: 'Hello {user.display_name}', embed: null, buttons: [] } } });
  const r3 = await open(w, plain, w.member('7303'), { reason: 'xyz' });
  assert.equal(r3.channel.sent[0].content, 'Hello User 7303\n\n**Reason:** xyz');
  assert.equal(r3.channel.sent[0].embeds.length, 0);
});

test('DB: the channel name follows the panel format and the option label', { skip: skipDb }, async () => {
  const w = makeWorld(G(15));
  const panel = await newPanel(w, { name: 'Named', data: { channelNameFormat: '{user.name}-{topic}-{number}' } });
  const r = await open(w, panel, w.member('7401'));
  assert.equal(r.channel.name, 'user7401-open-ticket-0001');
});

test('DB: failures delete the reserved row and map to friendly codes', { skip: skipDb }, async () => {
  const w = makeWorld(G(16));
  const cat = w.category('Existing');
  const panel = await newPanel(w, { name: 'Fail', data: { openCategoryId: cat.id } });
  const mk = (id) => w.member(id);

  w.guild.createErrors.push(discordError(50013));
  const perms = await open(w, panel, mk('7501'), { expectOk: false });
  assert.equal(perms.code, 'BOT_PERMS');
  assert.equal(await prisma.ticket.count({ where: { guildId: G(16) } }), 0, 'the reserved row is gone');

  w.guild.createErrors.push(discordError(30013));
  assert.equal((await open(w, panel, mk('7502'), { expectOk: false })).code, 'GUILD_FULL');
  w.guild.createErrors.push(discordError(50035));
  const generic = await open(w, panel, mk('7503'), { expectOk: false });
  assert.equal(generic.code, 'CREATE_FAILED');
  assert.equal(generic.data.reason, 'Discord rejected the request');
  assert.equal(await prisma.ticket.count({ where: { guildId: G(16) } }), 0);

  // The number of a failed create is handed out again: it never had a channel.
  const ok = await open(w, panel, mk('7504'));
  assert.equal(ok.ticket.number, 1);

  // The category is full: one retry without a parent, with a note.
  w.guild.fullParents = new Set([cat.id]);
  const full = await open(w, panel, mk('7505'));
  assert.equal(full.ticket.number, 2);
  assert.ok(full.notes.some((n) => n.includes('full (50 channels)')));
  assert.equal(full.channel.parentId, null);
  assert.equal(w.created.at(-1).parent, null);
  w.guild.fullParents = null;
});

test('DB: missing permissions refuse the open before anything is created', { skip: skipDb }, async () => {
  const w = makeWorld(G(17), { botCan: false });
  const panel = await newPanel(w, { name: 'Perm' });
  const r = await open(w, panel, w.member('7601'), { expectOk: false });
  assert.equal(r.code, 'BOT_PERMS');
  assert.match(r.data.names, /Manage Channels/);
  assert.equal(await prisma.ticket.count({ where: { guildId: G(17) } }), 0);
  assert.equal(w.created.length, 0);

  // A configured category the bot cannot use names the category and leaves no row.
  const w2 = makeWorld(G(18));
  const cat = w2.category('Locked');
  cat.permissionsFor = () => ({ has: () => false });
  const panel2 = await newPanel(w2, { name: 'Perm2', data: { openCategoryId: cat.id } });
  const r2 = await open(w2, panel2, w2.member('7602'), { expectOk: false });
  assert.equal(r2.code, 'BOT_PERMS');
  assert.equal(r2.data.where, 'Locked');
  assert.equal(await prisma.ticket.count({ where: { guildId: G(18) } }), 0);
  assert.equal(w2.created.length, 0);
});

test('DB: a category with 50 channels is skipped with a note', { skip: skipDb }, async () => {
  const w = makeWorld(G(19));
  const cat = w.category('Full');
  for (let i = 0; i < 50; i++) { const c = w.text(`c${i}`); c.parentId = cat.id; }
  const panel = await newPanel(w, { name: 'Fullcat', data: { openCategoryId: cat.id } });
  const r = await open(w, panel, w.member('7701'));
  assert.ok(r.notes.some((n) => n.includes('full (50 channels)')));
  assert.equal(r.channel.parentId, null);
  assert.equal(w.created.length, 1);
  assert.equal(w.created[0].parent, null);
});

test('DB: a cache miss is not a deletion; only a confirmed 10003 frees a slot', { skip: skipDb }, async () => {
  const w = makeWorld(G(20));
  const panel = await newPanel(w, { name: 'Stale' });
  const alice = w.member('7801');
  const stale = (channelId, status = 'OPEN', createdAt = new Date()) => prisma.ticket.create({
    data: { guildId: G(20), panelId: panel.id, optionId: panel.options[0].id, number: 100 + Math.floor(Math.random() * 1e6), channelId, ownerId: '7801', status, createdAt },
  });

  // Not cached and Discord cannot say (an error other than 10003): the ticket counts as live.
  const unknown = await stale('4001');
  w.guild.fetchError = discordError(50001, 'Missing Access');
  const blocked = await open(w, panel, alice, { expectOk: false });
  assert.equal(blocked.code, 'OPEN_LIMIT');
  assert.equal((await ticketRow(unknown.id)).status, 'OPEN', 'nothing was written off');

  // An unavailable guild proves nothing either.
  w.guild.fetchError = null;
  w.guild.available = false;
  skewClock(6000);
  assert.equal((await open(w, panel, alice, { expectOk: false })).code, 'OPEN_LIMIT');
  assert.equal((await ticketRow(unknown.id)).status, 'OPEN');
  w.guild.available = true;

  // Discord confirms the channel is gone: the row is marked DELETED and the member can open again.
  skewClock(6000);
  const ok = await open(w, panel, alice);
  assert.equal((await ticketRow(unknown.id)).status, 'DELETED');
  assert.equal(ok.ok, true);

  // A fresh placeholder is a creation in progress; a stale one is removed.
  const fresh = await stale(`${PENDING_PREFIX}fresh`);
  skewClock(6000);
  assert.equal((await open(w, panel, alice, { expectOk: false })).code, 'OPEN_LIMIT', 'the real ticket and the placeholder both count');
  await prisma.ticket.deleteMany({ where: { id: ok.ticket.id } });
  skewClock(6000);
  assert.equal((await open(w, panel, alice, { expectOk: false })).code, 'OPEN_LIMIT', 'the fresh placeholder blocks');
  await prisma.ticket.update({ where: { id: fresh.id }, data: { createdAt: new Date(Date.now() - PENDING_STALE_MS - 1000) } });
  skewClock(6000);
  assert.equal((await open(w, panel, alice)).ok, true);
  assert.equal(await prisma.ticket.count({ where: { id: fresh.id } }), 0, 'the stale placeholder was removed');
  restoreClock();
});

test('DB: an option category overrides the panel category; a deleted one is cleared only when confirmed gone', { skip: skipDb }, async () => {
  const w = makeWorld(G(21));
  const panelCat = w.category('PanelCat');
  const optCat = w.category('OptCat');
  const panel = await newPanel(w, { name: 'Opt', data: { openCategoryId: panelCat.id } });
  const added = await svc.addOption(panel.id, G(21), { label: 'Special', emoji: null, description: null, buttonStyle: 2, categoryId: optCat.id });
  const fresh = await svc.findPanel(G(21), panel.id);
  const special = fresh.options.find((o) => o.id === added.option.id);

  const r = await open(w, fresh, w.member('7901'), { option: special });
  assert.equal(r.channel.parentId, optCat.id);
  assert.equal((await open(w, fresh, w.member('7902'))).channel.parentId, panelCat.id);

  // The option category disappears from the cache but Discord cannot confirm (non-10003): the panel category is used, nothing cleared.
  w.guild.channels.cache.delete(optCat.id);
  w.guild.fetchError = discordError(50001);
  const r3 = await open(w, fresh, w.member('7903'), { option: special });
  assert.equal(r3.channel.parentId, panelCat.id);
  assert.equal((await prisma.ticketPanelOption.findUnique({ where: { id: special.id } })).categoryId, optCat.id);

  // Confirmed gone (10003): cleared.
  w.guild.fetchError = null;
  const r4 = await open(w, fresh, w.member('7904'), { option: special });
  assert.equal(r4.channel.parentId, panelCat.id);
  assert.equal((await prisma.ticketPanelOption.findUnique({ where: { id: special.id } })).categoryId, null);
});

test('DB: ensureCategory reuses a same-named category, recreates a deleted one and reports failures', { skip: skipDb }, async () => {
  const w = makeWorld(G(22));
  const existing = w.category('claimed tickets'); // case-insensitive reuse
  const panel = await newPanel(w, { name: 'Cats' });

  const reused = await svc.ensureCategory(w.guild, panel.id, 'claimed');
  assert.equal(reused.category.id, existing.id);
  assert.equal(reused.reused, true);
  assert.equal(reused.created, false);
  assert.equal(w.created.length, 0);
  assert.equal((await svc.findPanel(G(22), panel.id)).claimedCategoryId, existing.id);

  const made = await svc.ensureCategory(w.guild, panel.id, 'closed');
  assert.equal(made.created, true);
  assert.equal(made.category.name, 'Closed Tickets');
  const again = await svc.ensureCategory(w.guild, panel.id, 'closed');
  assert.equal(again.category.id, made.category.id);
  assert.equal(again.created, false);
  assert.equal(w.created.length, 1);

  // Deleted on Discord's side (confirmed by a 10003 fetch): a new one is created and stored.
  w.guild.channels.cache.delete(made.category.id);
  const recreated = await svc.ensureCategory(w.guild, panel.id, 'closed');
  assert.equal(recreated.created, true);
  assert.notEqual(recreated.category.id, made.category.id);
  assert.equal((await svc.findPanel(G(22), panel.id)).closedCategoryId, recreated.category.id);

  // Discord cannot say: nothing is created or cleared, and the note says so.
  w.guild.channels.cache.delete(recreated.category.id);
  w.guild.fetchError = discordError(50001);
  const unsure = await svc.ensureCategory(w.guild, panel.id, 'closed');
  assert.equal(unsure.category, null);
  assert.ok(unsure.note);
  assert.equal((await svc.findPanel(G(22), panel.id)).closedCategoryId, recreated.category.id, 'the setting is untouched');
  w.guild.fetchError = null;

  // Creation failures become notes.
  const w2 = makeWorld(G(23));
  const p2 = await newPanel(w2, { name: 'Cats2' });
  w2.guild.createErrors.push(discordError(30013));
  assert.match((await svc.ensureCategory(w2.guild, p2.id, 'open')).note, /500 channels/);
  w2.guild.createErrors.push(discordError(50013));
  assert.match((await svc.ensureCategory(w2.guild, p2.id, 'open')).note, /couldn't create the "Tickets" category/);
  w2.guild.botPerms = { has: () => false };
  w2.guild.members.me.permissions = { has: () => false };
  assert.match((await svc.ensureCategory(w2.guild, p2.id, 'open')).note, /missing permissions/);
  assert.equal((await svc.ensureCategory(w2.guild, 'nope', 'open')).category, null);
});

test('DB: postIntro pings manager roles only when the template says {managers} and the role is pingable', { skip: skipDb }, async () => {
  const w = makeWorld(G(24));
  w.role('A', { mentionable: true });
  w.role('B', { mentionable: false });
  const ch = w.text('t');
  const ticket = { id: 'T1', number: 5, ownerId: '1', reason: null };
  const member = w.member('1');
  const send = async (panel, permissionsFor) => {
    ch.sent.length = 0;
    ch.permissionsFor = permissionsFor ?? (() => ({ has: () => true }));
    const ok = await svc.postIntro({ channel: ch, ticket, panel, option: null, member, settings: null });
    assert.equal(ok, true);
    return ch.sent[0];
  };
  const withToken = { managerRoleIds: ['A', 'B'], introTemplate: { content: '{managers} please help {user}', embed: { title: 'x' }, buttons: [] } };

  // The bot may mention everyone: both roles ping.
  assert.deepEqual((await send(withToken)).allowedMentions, { parse: [], users: ['1'], roles: ['A', 'B'] });
  assert.equal((await send(withToken)).content, '<@&A> <@&B> please help <@1>');
  // The bot lacks Mention Everyone: only the mentionable role pings.
  const noMentionAll = () => ({ has: (p) => p !== PermissionFlagsBits.MentionEveryone });
  assert.deepEqual((await send(withToken, noMentionAll)).allowedMentions.roles, ['A']);
  // Without the token nothing pings, even with managers.
  const noToken = { managerRoleIds: ['A', 'B'], introTemplate: { content: 'hello {user}', embed: null, buttons: [] } };
  assert.deepEqual((await send(noToken)).allowedMentions, { parse: [], users: ['1'], roles: [] });
  // The default intro never pings roles; a deleted role is not rendered.
  assert.deepEqual((await send({ managerRoleIds: ['A', 'gone'] })).allowedMentions.roles, []);
  assert.equal((await send({ managerRoleIds: ['A', 'gone'], introTemplate: { content: '{managers}' } })).content, '<@&A>');
  // @everyone can never come from a template: parse stays empty and the text is inert.
  const sneaky = await send({ managerRoleIds: [], introTemplate: { content: '@everyone @here', buttons: [] } });
  assert.deepEqual(sneaky.allowedMentions.parse, []);
  // An option intro beats the panel intro.
  ch.sent.length = 0;
  await svc.postIntro({ channel: ch, ticket, panel: { introTemplate: { content: 'panel' } }, option: { introTemplate: { content: 'option' }, label: 'L' }, member, settings: null });
  assert.equal(ch.sent[0].content, 'option');
  // A send failure is reported, not thrown.
  ch.sendError = discordError(50013);
  assert.equal(await svc.postIntro({ channel: ch, ticket, panel: null, option: null, member, settings: null }), false);
  ch.sendError = null;
});

// ── Lifecycle ─────────────────────────────────────────────────────────────────

/** A world with one panel (manager role R1, optional extras) and one open ticket by `alice`. */
async function ticketWorld(n, { data = {}, alice = '6001' } = {}) {
  const w = makeWorld(G(n));
  w.role('R1');
  const panel = await newPanel(w, { name: `Life ${n}`, data: { managerRoleIds: ['R1'], ...data } });
  const owner = w.member(alice);
  const r = await open(w, panel, owner);
  const ctx = (actor, extra = {}) => ({ client: w.client, guild: w.guild, ticket: { id: r.ticket.id }, actor, settings: null, source: 'command', ...extra });
  return { w, panel, owner, ticket: r.ticket, channel: r.channel, ctx, staff: w.member(`${alice}1`, { roles: ['R1'] }), admin: w.member(`${alice}2`, { admin: true }) };
}
const catId = (w, name) => w.guild.channels.cache.find((c) => c.name === name)?.id;
const lastSent = (ch) => ch.sent.at(-1);

test('DB: claim, close and reopen move the channel, edit access and post notices', { skip: skipDb }, async () => {
  const { w, owner, ticket, channel, ctx, staff, admin } = await ticketWorld(30);

  // Who may claim.
  assert.equal((await svc.claimTicket(ctx(owner))).code, 'NOT_STAFF');
  assert.equal((await svc.claimTicket(ctx(w.member('stranger')))).code, 'NOT_STAFF');
  const ownerStaff = w.member('6001', { admin: true });
  assert.equal((await svc.claimTicket(ctx(ownerStaff))).code, 'OWN_TICKET');
  assert.equal((await ticketRow(ticket.id)).status, 'OPEN', 'refusals change nothing');

  const claimed = await svc.claimTicket(ctx(staff));
  assert.deepEqual(claimed, { ok: true, notes: [] });
  const row = await ticketRow(ticket.id);
  assert.deepEqual([row.status, row.claimedById], ['CLAIMED', staff.id]);
  assert.equal(channel.parentId, catId(w, 'Claimed Tickets'));
  const move = channel.calls.find((c) => c[0] === 'setParent');
  assert.deepEqual(move[2].lockPermissions, false, 'the private overwrites are not replaced');
  assert.equal(lastSent(channel).embeds[0].data.title, '🙋 Ticket Claimed');
  assert.ok(lastSent(channel).embeds[0].data.description.includes(`<@${staff.id}>`));
  assert.deepEqual(lastSent(channel).allowedMentions, { parse: [] });

  assert.equal((await svc.claimTicket(ctx(staff))).code, 'ALREADY_CLAIMED_SELF');
  const other = w.member('6009', { roles: ['R1'] });
  const refused = await svc.claimTicket(ctx(other));
  assert.deepEqual([refused.code, refused.data], ['ALREADY_CLAIMED', { claimedById: staff.id }]);
  // An admin takes over.
  const took = await svc.claimTicket(ctx(admin));
  assert.equal(took.ok, true);
  assert.equal((await ticketRow(ticket.id)).claimedById, admin.id);
  assert.ok(lastSent(channel).embeds[0].data.description.includes(`took over this ticket from <@${staff.id}>`));

  // Close by the opener.
  const closed = await svc.closeTicket(ctx(owner, { reason: 'All sorted' }));
  assert.deepEqual(closed, { ok: true, notes: [] });
  const crow = await ticketRow(ticket.id);
  assert.deepEqual([crow.status, crow.closedById], ['CLOSED', owner.id]);
  assert.ok(crow.closedAt instanceof Date);
  assert.equal(channel.parentId, catId(w, 'Closed Tickets'));
  const edit = channel.calls.find((c) => c[0] === 'overwrite.edit');
  assert.deepEqual(edit.slice(1, 3), [owner.id, { ViewChannel: true, SendMessages: false, ReadMessageHistory: true }]);
  assert.equal(edit[3].type, OverwriteType.Member);
  const notice = lastSent(channel);
  assert.equal(notice.embeds[0].data.title, '🔒 Ticket Closed');
  assert.equal(notice.embeds[0].data.fields[0].value, 'All sorted');
  assert.deepEqual(notice.components[0].toJSON().components.map((c) => c.label), ['Reopen', 'Transcript', 'Delete']);
  assert.equal((await svc.closeTicket(ctx(owner))).code, 'ALREADY_CLOSED');
  assert.equal((await svc.claimTicket(ctx(staff))).code, 'IS_CLOSED');
  assert.equal((await svc.reopenTicket(ctx(owner))).code, 'NOT_STAFF', 'the opener cannot reopen');
  assert.equal((await svc.deleteTicket(ctx(owner))).code, 'NOT_STAFF', 'the opener cannot delete');

  // Reopen by staff: the ticket had been claimed, so it is CLAIMED again.
  channel.calls.length = 0;
  const reopened = await svc.reopenTicket(ctx(staff));
  assert.deepEqual(reopened, { ok: true, notes: [] });
  const rrow = await ticketRow(ticket.id);
  assert.deepEqual([rrow.status, rrow.closedById, rrow.closedAt], ['CLAIMED', null, null]);
  assert.equal(channel.parentId, catId(w, 'Claimed Tickets'));
  assert.deepEqual(channel.calls.find((c) => c[0] === 'overwrite.edit').slice(1, 3), [owner.id, { ViewChannel: true, SendMessages: true, ReadMessageHistory: true }]);
  const rnotice = lastSent(channel);
  assert.equal(rnotice.embeds[0].data.title, '🔓 Ticket Reopened');
  assert.equal(rnotice.components[0].toJSON().components[1].disabled, true, 'claimed: the Claim button says so');
  assert.equal((await svc.reopenTicket(ctx(staff))).code, 'NOT_CLOSED');
});

test('DB: reopening an unclaimed ticket returns it to the open category', { skip: skipDb }, async () => {
  const { w, owner, ticket, channel, ctx, staff } = await ticketWorld(31);
  const openCat = channel.parentId;
  assert.equal(openCat, catId(w, 'Tickets'));
  assert.equal((await svc.closeTicket(ctx(staff))).ok, true);
  assert.equal(channel.parentId, catId(w, 'Closed Tickets'));
  assert.equal((await svc.reopenTicket(ctx(staff))).ok, true);
  assert.equal((await ticketRow(ticket.id)).status, 'OPEN');
  assert.equal(channel.parentId, openCat);
  assert.equal(owner.id, ticket.ownerId);
});

test('DB: lifecycle actions refuse politely when the bot lacks permissions, and change nothing', { skip: skipDb }, async () => {
  const { w, ticket, channel, ctx, staff } = await ticketWorld(32);
  channel.permissionsFor = () => ({ has: () => false });
  const r = await svc.claimTicket(ctx(staff));
  assert.equal(r.code, 'BOT_PERMS');
  assert.equal(r.data.channelId, channel.id);
  assert.match(r.data.names, /Manage Channels/);
  assert.equal((await ticketRow(ticket.id)).status, 'OPEN');
  assert.ok(errorPayload(r).embeds[0].data.description.includes(`<#${channel.id}>`));
  channel.permissionsFor = () => w.guild.botPerms;

  // A failing overwrite edit after the status change is a note, not a failure.
  channel.overwriteError = discordError(50013);
  const closed = await svc.closeTicket(ctx(staff));
  assert.equal(closed.ok, true);
  assert.ok(closed.notes.some((n) => n.includes("couldn't update the permissions")));
  assert.equal((await ticketRow(ticket.id)).status, 'CLOSED');
  // A failing notice is a note too.
  channel.overwriteError = null;
  channel.sendError = discordError(50013);
  const reopened = await svc.reopenTicket(ctx(staff));
  assert.equal(reopened.ok, true);
  assert.ok(reopened.notes.some((n) => n.includes("couldn't post the notice")));
});

test('DB: concurrent claims are serialised: one wins, the other is told who has it', { skip: skipDb }, async () => {
  const { w, ticket, ctx, staff } = await ticketWorld(33);
  const other = w.member('6339', { roles: ['R1'] });
  const [a, b] = await Promise.all([svc.claimTicket(ctx(staff)), svc.claimTicket(ctx(other))]);
  const oks = [a, b].filter((r) => r.ok);
  const lost = [a, b].filter((r) => !r.ok);
  assert.equal(oks.length, 1);
  assert.equal(lost[0].code, 'ALREADY_CLAIMED');
  assert.equal((await ticketRow(ticket.id)).claimedById, lost[0].data.claimedById);
});

test('DB: add, remove and rename', { skip: skipDb }, async () => {
  const { w, ticket, channel, ctx, staff, owner } = await ticketWorld(34);
  const friend = w.member('6402');
  const friendUser = friend.user;

  assert.equal((await svc.addMember(ctx(owner, { user: friendUser, member: friend }))).code, 'NOT_STAFF');
  assert.equal((await svc.addMember(ctx(staff, { user: friendUser, member: null }))).code, 'NOT_MEMBER');
  assert.equal((await svc.addMember(ctx(staff, { user: { id: '99', bot: true }, member: friend }))).code, 'ADD_BOT');
  const own = await svc.addMember(ctx(staff, { user: owner.user, member: owner }));
  assert.deepEqual([own.code, own.data], ['IS_OWNER', { op: 'add' }]);

  const added = await svc.addMember(ctx(staff, { user: friendUser, member: friend }));
  assert.deepEqual(added, { ok: true, notes: [] });
  assert.deepEqual((await ticketRow(ticket.id)).addedUserIds, [friend.id]);
  const edit = channel.calls.filter((c) => c[0] === 'overwrite.edit').at(-1);
  assert.deepEqual(edit.slice(1, 3), [friend.id, { ViewChannel: true, SendMessages: true, ReadMessageHistory: true, AttachFiles: true, EmbedLinks: true }]);
  assert.equal(edit[3].type, OverwriteType.Member);
  assert.deepEqual(lastSent(channel).allowedMentions, { parse: [], users: [friend.id] }, 'the added member is pinged');
  assert.equal((await svc.addMember(ctx(staff, { user: friendUser, member: friend }))).code, 'ALREADY_ADDED');

  // Closing locks the added member as well; adding to a closed ticket gives read-only access.
  assert.equal((await svc.closeTicket(ctx(owner))).ok, true);
  const lockEdits = channel.calls.filter((c) => c[0] === 'overwrite.edit' && c[2].SendMessages === false).map((c) => c[1]);
  assert.deepEqual(lockEdits.sort(), [friend.id, owner.id].sort());
  const late = w.member('6403');
  assert.equal((await svc.addMember(ctx(staff, { user: late.user, member: late }))).ok, true);
  assert.equal(channel.calls.filter((c) => c[0] === 'overwrite.edit').at(-1)[2].SendMessages, false);

  // Remove.
  const ownerRemoved = await svc.removeMember(ctx(staff, { user: owner.user }));
  assert.deepEqual([ownerRemoved.code, ownerRemoved.data], ['IS_OWNER', { op: 'remove' }]);
  assert.equal((await svc.removeMember(ctx(staff, { user: { id: '1234' } }))).code, 'NOT_ADDED');
  assert.equal((await svc.removeMember(ctx(staff, { user: friendUser }))).ok, true);
  assert.deepEqual((await ticketRow(ticket.id)).addedUserIds, [late.id]);
  channel.overwriteError = discordError(10009);
  assert.equal((await svc.removeMember(ctx(staff, { user: late.user }))).ok, true, 'Unknown Overwrite counts as removed');
  assert.deepEqual((await ticketRow(ticket.id)).addedUserIds, []);
  channel.overwriteError = discordError(50013);
  assert.equal((await svc.addMember(ctx(staff, { user: friendUser, member: friend }))).code, 'BOT_PERMS');
  assert.deepEqual((await ticketRow(ticket.id)).addedUserIds, [], 'a failed overwrite is not recorded');
  channel.overwriteError = null;

  // The limit.
  await prisma.ticket.update({ where: { id: ticket.id }, data: { addedUserIds: Array.from({ length: MAX_ADDED_USERS }, (_, i) => `u${i}`) } });
  assert.equal((await svc.addMember(ctx(staff, { user: friendUser, member: friend }))).code, 'ADDED_LIMIT');
});

test('DB: rename sanitises, allows two per ten minutes and then names the retry time', { skip: skipDb }, async () => {
  const { ticket, channel, ctx, staff, owner } = await ticketWorld(35);
  assert.equal((await svc.renameTicket(ctx(owner, { name: 'mine' }))).code, 'NOT_STAFF');
  assert.equal((await svc.renameTicket(ctx(staff, { name: '!!!' }))).code, 'BAD_NAME');
  assert.equal((await svc.renameTicket(ctx(staff, { name: 'Billing Help!' }))).ok, true);
  assert.equal(channel.name, 'billing-help');
  assert.equal(lastSent(channel).embeds[0].data.title, '✏️ Ticket Renamed');
  assert.equal((await svc.renameTicket(ctx(staff, { name: 'second' }))).ok, true);
  const third = await svc.renameTicket(ctx(staff, { name: 'third' }));
  assert.equal(third.code, 'RENAME_LIMIT');
  assert.ok(third.data.retryAt instanceof Date && third.data.retryAt.getTime() > Date.now());
  assert.equal(channel.name, 'second');
  assert.ok(errorPayload(third).embeds[0].data.description.includes('<t:'));
  assert.equal((await ticketRow(ticket.id)).status, 'OPEN');
});

test('DB: a deleted or missing ticket answers TICKET_DELETED', { skip: skipDb }, async () => {
  const { w, ticket, channel, ctx, staff } = await ticketWorld(36);
  // The channel vanished and Discord confirms it (10003): the ticket is marked DELETED.
  w.guild.channels.cache.delete(channel.id);
  assert.equal((await svc.claimTicket(ctx(staff))).code, 'TICKET_DELETED');
  assert.equal((await ticketRow(ticket.id)).status, 'DELETED');
  assert.equal((await svc.claimTicket(ctx(staff))).code, 'TICKET_DELETED');
  assert.equal((await svc.claimTicket({ ...ctx(staff), ticket: { id: 'nope' } })).code, 'TICKET_DELETED');

  // Discord cannot say: the action fails but nothing is written off.
  const second = await ticketWorld(37);
  second.w.guild.channels.cache.delete(second.channel.id);
  second.w.guild.fetchError = discordError(50001);
  assert.equal((await svc.claimTicket(second.ctx(second.staff))).code, 'UNKNOWN');
  assert.equal((await ticketRow(second.ticket.id)).status, 'OPEN');
});

test('DB: transcripts: staff only, throttled, and built from the channel history', { skip: skipDb }, async () => {
  const { w, ticket, channel, owner, staff } = await ticketWorld(38);
  for (let i = 1; i <= 250; i++) {
    channel.store.push({
      id: `${i}`.padStart(5, '0'), content: i === 1 ? 'first <b>msg</b> <@6001>' : `msg ${i}`, createdTimestamp: Date.UTC(2026, 0, 1, 0, 0, i % 60),
      author: { id: '6001', username: 'alice', tag: 'alice#0', bot: false }, mentions: { users: new Map([['6001', { username: 'alice' }]]), members: new Map() },
      embeds: [], attachments: new Map(), stickers: new Map(), system: false, member: null,
    });
  }
  const args = (actor) => ({ guild: w.guild, ticket: { ...ticket, panel: { managerRoleIds: ['R1'], name: 'Life 38' } }, channel, settings: { timezone: 'Europe/London', embedColor: 0x123456 }, actor });
  const ownerTry = await svc.transcriptForActor(args(owner));
  assert.equal(ownerTry.code, 'NOT_STAFF');
  const ok = await svc.transcriptForActor(args(staff));
  assert.equal(ok.ok, true);
  const [file] = ok.payload.files;
  assert.equal(file.name, 'transcript-ticket-0001.html');
  const html = file.attachment.toString('utf8');
  assert.ok(html.includes('first &lt;b&gt;msg&lt;/b&gt; @alice'));
  assert.ok(html.includes('msg 250'), 'pages beyond the first 100 were read');
  assert.ok(html.indexOf('msg 2<') < html.indexOf('msg 250'), 'chronological');
  assert.ok(html.includes('Europe/London'));
  assert.equal(ok.payload.embeds[0].data.title, '📄 Ticket #0001 transcript');
  assert.equal(ok.payload.embeds[0].data.fields.find((f) => f.name === 'Messages').value, '250');
  assert.equal(ok.payload.embeds[0].data.color, 0x123456);
  assert.equal((await svc.transcriptForActor(args(staff))).code, 'TRANSCRIPT_THROTTLED');
  assert.equal((await svc.transcriptForActor({ ...args(staff), ticket: { ...ticket, status: 'DELETED' } })).code, 'TICKET_DELETED');

  // buildTranscript reports a channel it cannot read, and a history error.
  channel.permissionsFor = () => ({ has: () => false });
  const blind = await svc.buildTranscript({ guild: w.guild, ticket, channel, settings: null, generatedBy: staff.user });
  assert.deepEqual([blind.ok, blind.reason.startsWith('missing')], [false, true]);
  channel.permissionsFor = () => w.guild.botPerms;
  channel.messages.fetch = async () => { throw discordError(50013); };
  const broken = await svc.buildTranscript({ guild: w.guild, ticket, channel, settings: null, generatedBy: staff.user });
  assert.deepEqual([broken.ok, broken.reason], [false, "I'm missing permissions there"]);
  assert.equal(errorPayload({ code: 'TRANSCRIPT_FAILED', data: { reason: broken.reason } }).embeds[0].data.title, '❌  Transcript Failed');
});

test('DB: fetchTranscriptMessages pages, caps and returns chronological order', { skip: skipDb }, async () => {
  const w = makeWorld(G(39));
  const ch = w.text('t');
  for (let i = 1; i <= 230; i++) ch.store.push({ id: String(i).padStart(5, '0') });
  const all = await svc.fetchTranscriptMessages(ch);
  assert.equal(all.messages.length, 230);
  assert.equal(all.truncated, null);
  assert.equal(all.messages[0].id, '00001');
  assert.equal(all.messages.at(-1).id, '00230');

  const capped = await svc.fetchTranscriptMessages(ch, { max: 150 });
  assert.equal(capped.truncated, 'limit');
  assert.equal(capped.messages.length, 150);
  assert.equal(capped.messages.at(-1).id, '00230', 'the newest are kept');
  assert.equal(capped.messages[0].id, '00081');

  const exact = await svc.fetchTranscriptMessages(ch, { max: 230 });
  assert.equal(exact.truncated, null, 'exactly max messages with nothing more is not truncated');
  const timed = await svc.fetchTranscriptMessages(ch, { deadlineMs: -1 });
  assert.equal(timed.truncated, 'time');
  assert.equal(timed.messages.length, 100);
});

test('DB: delete: transcript to the channel and the opener, then the channel; a delete in progress blocks other actions', { skip: skipDb }, async () => {
  const scenarios = await Promise.all([40, 41, 42, 43, 44, 45].map((n) => ticketWorld(n)));
  const [full, unusable, dmClosed, failing, bare, gone] = scenarios;

  // A: transcript channel + DM + a concurrent close during the transcript.
  const tcA = full.w.text('transcripts');
  await svc.updatePanelFields(full.panel.id, G(40), { transcriptChannelId: tcA.id, dmTranscript: true });
  full.channel.store.push({
    id: '00001', content: 'hello', createdTimestamp: Date.now(), author: { id: '6001', username: 'alice', tag: 'alice#0', bot: false },
    mentions: { users: new Map(), members: new Map() }, embeds: [], attachments: new Map(), stickers: new Map(), system: false,
  });
  const savedA = [];
  const runA = svc.deleteTicket(full.ctx(full.staff, { onSaved: async (info) => { savedA.push(info); } }));
  while (savedA.length === 0) await new Promise((r) => setTimeout(r, 10));
  assert.equal((await svc.closeTicket(full.ctx(full.staff))).code, 'DELETING');
  assert.equal((await svc.reopenTicket(full.ctx(full.staff))).code, 'DELETING');
  assert.equal((await svc.deleteTicket(full.ctx(full.staff))).code, 'DELETING');
  assert.equal((await ticketRow(full.ticket.id)).status, 'OPEN', 'not DELETED until the channel is gone');
  assert.ok(full.w.guild.channels.cache.has(full.channel.id));
  assert.ok(savedA[0].text.includes('transcript was saved') && savedA[0].text.includes(`<#${tcA.id}>`) && savedA[0].text.includes('DM'));
  assert.equal(tcA.sent.length, 1);
  assert.equal(tcA.sent[0].files[0].name, 'transcript-ticket-0001.html');
  assert.equal(tcA.sent[0].embeds[0].data.title, '📄 Ticket #0001 transcript');
  assert.deepEqual(tcA.sent[0].allowedMentions, { parse: [] });
  assert.equal(full.w.dms.length, 1);
  assert.equal(full.w.dms[0][0], '6001');
  assert.equal(full.w.dms[0][1].files[0].name, 'transcript-ticket-0001.html');
  assert.ok(full.w.dms[0][1].embeds[0].data.footer.text.includes('Fake Guild'));
  assert.equal(lastSent(full.channel).embeds[0].data.title, '🗑️ Deleting Ticket');

  // B: an unusable transcript channel aborts before anything is changed.
  const tcB = unusable.w.text('transcripts');
  tcB.permissionsFor = () => ({ has: () => false });
  await svc.updatePanelFields(unusable.panel.id, G(41), { transcriptChannelId: tcB.id });
  const b = await svc.deleteTicket(unusable.ctx(unusable.staff));
  assert.equal(b.code, 'TRANSCRIPT_CHANNEL');
  assert.equal(b.data.channelId, tcB.id);
  assert.ok(unusable.w.guild.channels.cache.has(unusable.channel.id));
  assert.equal((await ticketRow(unusable.ticket.id)).status, 'OPEN');
  assert.equal(unusable.channel.sent.length, 1, 'only the intro: no notice was posted');
  assert.ok(errorPayload(b).embeds[0].data.description.includes('The ticket was not deleted'));
  // ... and a deleted-then-unconfirmed transcript channel (Discord cannot say) aborts too.
  await svc.updatePanelFields(unusable.panel.id, G(41), { transcriptChannelId: '123456789' });
  unusable.w.guild.fetchError = discordError(50001);
  assert.equal((await svc.deleteTicket(unusable.ctx(unusable.staff))).code, 'TRANSCRIPT_CHANNEL');
  unusable.w.guild.fetchError = null;

  // C: the opener has DMs closed: the delete goes on, the text carries a warning.
  dmClosed.w.guild.dmError = discordError(50007);
  await svc.updatePanelFields(dmClosed.panel.id, G(42), { dmTranscript: true });
  const savedC = [];
  // D: Discord refuses the delete: DELETE_FAILED, the ticket stays as it was.
  failing.channel.deleteError = discordError(50013);
  const tcD = failing.w.text('transcripts');
  await svc.updatePanelFields(failing.panel.id, G(43), { transcriptChannelId: tcD.id });
  // E: nothing configured: no transcript, the channel is deleted.
  const savedE = [];
  // F: the configured transcript channel was deleted (10003): the delete goes on without a channel copy.
  await svc.updatePanelFields(gone.panel.id, G(45), { transcriptChannelId: '555555555555555555' });
  const savedF = [];

  const [rA, rC, rD, rE, rF] = await Promise.all([
    runA,
    svc.deleteTicket(dmClosed.ctx(dmClosed.staff, { onSaved: async (i) => { savedC.push(i); } })),
    svc.deleteTicket(failing.ctx(failing.staff)),
    svc.deleteTicket(bare.ctx(bare.staff, { onSaved: async (i) => { savedE.push(i); } })),
    svc.deleteTicket(gone.ctx(gone.staff, { onSaved: async (i) => { savedF.push(i); } })),
  ]);

  assert.deepEqual(rA, { ok: true });
  const rowA = await ticketRow(full.ticket.id);
  assert.deepEqual([rowA.status, rowA.closedById !== null, rowA.closedAt !== null], ['DELETED', true, true]);
  assert.equal(full.w.guild.channels.cache.has(full.channel.id), false);
  assert.ok(full.channel.calls.some((c) => c[0] === 'delete'));
  // Once deleted, further actions answer TICKET_DELETED.
  assert.equal((await svc.claimTicket(full.ctx(full.staff))).code, 'TICKET_DELETED');

  assert.equal(rC.ok, true);
  assert.ok(savedC[0].text.includes('opener has DMs closed'));
  assert.equal(dmClosed.w.dms.length, 0);
  assert.equal((await ticketRow(dmClosed.ticket.id)).status, 'DELETED');

  assert.equal(rD.code, 'DELETE_FAILED');
  assert.equal(rD.data.transcriptSaved, true);
  assert.equal(tcD.sent.length, 1);
  assert.equal((await ticketRow(failing.ticket.id)).status, 'OPEN');
  assert.ok(errorPayload(rD).embeds[0].data.description.includes('The transcript was already saved'));
  // The ticket can be retried once permissions are fixed (the in-progress flag was released).
  failing.channel.deleteError = null;
  assert.equal((await svc.closeTicket(failing.ctx(failing.staff))).ok, true);

  assert.equal(rE.ok, true);
  assert.equal(bare.channel.sent.filter((m) => m.embeds?.[0]?.data?.title === '🗑️ Deleting Ticket').length, 1);
  assert.equal(savedE[0].text.includes('transcript'), false);
  assert.equal((await ticketRow(bare.ticket.id)).status, 'DELETED');

  assert.equal(rF.ok, true);
  assert.equal(savedF[0].text.includes('transcript was saved'), false);
  assert.equal((await ticketRow(gone.ticket.id)).status, 'DELETED');
});

test('DB: deleting a closed ticket via the opener-less path keeps closedBy/closedAt', { skip: skipDb }, async () => {
  const { ticket, ctx, staff } = await ticketWorld(46);
  assert.equal((await svc.closeTicket(ctx(staff))).ok, true);
  const before = await ticketRow(ticket.id);
  const r = await svc.deleteTicket(ctx(staff, { source: 'button' }));
  assert.equal(r.ok, true);
  const after = await ticketRow(ticket.id);
  assert.equal(after.status, 'DELETED');
  assert.equal(after.closedById, before.closedById);
  assert.equal(after.closedAt.getTime(), before.closedAt.getTime());
});

test('DB: the delete button needs the ticket closed first', { skip: skipDb }, async () => {
  const { ticket, ctx, staff } = await ticketWorld(47);
  const r = await svc.deleteTicket(ctx(staff, { source: 'button' }));
  assert.equal(r.code, 'CLOSE_FIRST');
  assert.equal((await ticketRow(ticket.id)).status, 'OPEN');
});

// ── channelDelete hook, reconcile, publish ────────────────────────────────────

test('DB: onChannelDelete marks tickets and clears dead references', { skip: skipDb }, async () => {
  const w = makeWorld(G(50));
  const panel = await newPanel(w, { name: 'Hook' });
  const cat = w.category('Tickets');
  const tc = w.text('transcripts');
  const pub = w.text('panel-channel');
  await svc.updatePanelFields(panel.id, G(50), { openCategoryId: cat.id, claimedCategoryId: cat.id, closedCategoryId: cat.id, transcriptChannelId: tc.id, channelId: pub.id, messageId: '999' });
  const added = await svc.addOption(panel.id, G(50), { label: 'WithCat', emoji: null, description: null, buttonStyle: 1, categoryId: cat.id });
  const r = await open(w, await svc.findPanel(G(50), panel.id), w.member('5001'));

  // A ticket channel deleted by hand.
  await svc.onChannelDelete({ guildId: G(50), channel: { id: r.channel.id, type: ChannelType.GuildText } });
  assert.equal((await ticketRow(r.ticket.id)).status, 'DELETED');
  assert.equal((await svc.findPanel(G(50), panel.id)).transcriptChannelId, tc.id, 'other references untouched');

  // The transcript channel, then the panel channel.
  await svc.onChannelDelete({ guildId: G(50), channel: { id: tc.id, type: ChannelType.GuildText } });
  assert.equal((await svc.findPanel(G(50), panel.id)).transcriptChannelId, null);
  await svc.onChannelDelete({ guildId: G(50), channel: { id: pub.id, type: ChannelType.GuildText } });
  const afterPub = await svc.findPanel(G(50), panel.id);
  assert.deepEqual([afterPub.channelId, afterPub.messageId], [null, null]);

  // A category: the three panel fields and the option field.
  const before = await svc.findPanel(G(50), panel.id);
  assert.equal(before.openCategoryId, cat.id);
  await svc.onChannelDelete({ guildId: G(50), channel: { id: cat.id, type: ChannelType.GuildCategory } });
  const cleared = await svc.findPanel(G(50), panel.id);
  assert.deepEqual([cleared.openCategoryId, cleared.claimedCategoryId, cleared.closedCategoryId], [null, null, null]);
  assert.equal(cleared.options.find((o) => o.id === added.option.id).categoryId, null);

  // Scoped to the guild: another guild's panel with the same channel id is untouched.
  const w2 = makeWorld(G(51));
  const other = await newPanel(w2, { name: 'Other', data: { openCategoryId: cat.id } });
  await svc.onChannelDelete({ guildId: G(50), channel: { id: cat.id, type: ChannelType.GuildCategory } });
  assert.equal((await svc.findPanel(G(51), other.id)).openCategoryId, cat.id);

  // It never throws, and an idempotent second call is fine.
  await svc.onChannelDelete({ guildId: G(50), channel: { id: r.channel.id, type: ChannelType.GuildText } });
  await svc.onChannelDelete({ guildId: G(50), channel: null });
});

test('DB: listPanelsView reconciles tickets, confirming gone channels with Discord first', { skip: skipDb }, async () => {
  const w = makeWorld(G(52));
  const panel = await newPanel(w, { name: 'Rec' });
  const mk = (n, channelId, createdAt = new Date()) => prisma.ticket.create({
    data: { guildId: G(52), panelId: panel.id, number: n, channelId, ownerId: `${n}`, status: 'OPEN', createdAt },
  });
  const live = w.text('live');
  const tLive = await mk(1, live.id);
  const tGone = await mk(2, '777000000000000001');
  const tUnknown = await mk(3, '777000000000000002');
  const tFresh = await mk(4, `${PENDING_PREFIX}fresh`);
  const tStale = await mk(5, `${PENDING_PREFIX}stale`, new Date(Date.now() - PENDING_STALE_MS - 5000));

  // Discord cannot say for any uncached channel: only the stale placeholder goes.
  w.guild.fetchError = discordError(50001);
  let view = await svc.listPanelsView(w.guild);
  assert.equal(view[0].active, 4, 'live, the two unconfirmed channels and the fresh placeholder');
  assert.equal(await prisma.ticket.count({ where: { id: tStale.id } }), 0);
  assert.equal((await ticketRow(tGone.id)).status, 'OPEN');

  // An unavailable guild is skipped entirely.
  w.guild.fetchError = null;
  w.guild.available = false;
  assert.deepEqual(await svc.reconcileGuildTickets(w.guild), { deleted: 0, pendingRemoved: 0 });
  w.guild.available = true;

  // Confirmed (10003): marked DELETED.
  view = await svc.listPanelsView(w.guild);
  assert.equal((await ticketRow(tGone.id)).status, 'DELETED');
  assert.equal((await ticketRow(tUnknown.id)).status, 'DELETED');
  assert.equal((await ticketRow(tLive.id)).status, 'OPEN');
  assert.equal((await ticketRow(tFresh.id)).status, 'OPEN');
  assert.equal(view[0].active, 2);
  assert.ok(svc.panelListLine(view[0], w.guild).includes('2 active ticket(s)'));
  assert.ok(svc.panelListLine(view[0], w.guild).includes('not published'));
});

test('DB: publishPanel posts, edits in place, moves and recovers from a deleted message', { skip: skipDb }, async () => {
  const w = makeWorld(G(53));
  w.role('R1');
  const panel = await newPanel(w, { name: 'Pub', data: { style: 'BUTTONS' } });
  await svc.addOption(panel.id, G(53), { label: 'Billing', emoji: '💰', description: 'x', buttonStyle: 3, categoryId: null });
  const a = w.text('panel-a');
  const b = w.text('panel-b');

  const posted = await svc.publishPanel({ guild: w.guild, panelId: panel.id, channel: a, settings: null });
  assert.equal(posted.ok, true);
  assert.equal(posted.mode, 'posted');
  const payload = a.sent[0];
  assert.equal(payload.embeds[0].data.title, 'How can we help?');
  assert.equal(payload.components[0].toJSON().components.length, 2);
  assert.deepEqual(payload.allowedMentions, { parse: [] });
  const stored = await svc.findPanel(G(53), panel.id);
  assert.deepEqual([stored.channelId, stored.messageId], [a.id, posted.message.id]);

  // Same channel: edited in place (content cleared so removed text goes away).
  const edited = await svc.publishPanel({ guild: w.guild, panelId: panel.id, channel: a, settings: null });
  assert.equal(edited.mode, 'edited');
  assert.equal(a.sent.length, 1);
  assert.equal(a.published.edited.content, '');

  // Another channel: a new post, the old message is deleted.
  const moved = await svc.publishPanel({ guild: w.guild, panelId: panel.id, channel: b, settings: null });
  assert.equal(moved.mode, 'posted');
  assert.ok(a.calls.some((c) => c[0] === 'messages.delete'));
  assert.equal((await svc.findPanel(G(53), panel.id)).channelId, b.id);

  // The message vanished (10008): a new one is posted.
  b.published = null;
  const repost = await svc.publishPanel({ guild: w.guild, panelId: panel.id, channel: b, settings: null });
  assert.equal(repost.mode, 'posted');
  assert.equal(b.sent.length, 2);

  // The dropdown style renders a select; a send failure is a failure, not a throw.
  await svc.updatePanelFields(panel.id, G(53), { style: 'SELECT' });
  const sel = await svc.publishPanel({ guild: w.guild, panelId: panel.id, channel: b, settings: null });
  assert.equal(sel.ok, true);
  assert.equal(b.published.edited.components[0].toJSON().components[0].type, 3);
  b.sendError = discordError(50013);
  b.published = null;
  const bad = await svc.publishPanel({ guild: w.guild, panelId: panel.id, channel: b, settings: null });
  assert.deepEqual([bad.ok, bad.error.code], [false, 50013]);
  assert.equal((await svc.publishPanel({ guild: w.guild, panelId: 'nope', channel: b, settings: null })).code, 'PANEL_GONE');
});

test('DB: refreshPanelMessage edits, clears on a confirmed deletion and reports access problems', { skip: skipDb }, async () => {
  const w = makeWorld(G(54));
  const panel = await newPanel(w, { name: 'Ref' });
  assert.deepEqual(await svc.refreshPanelMessage(w.guild, panel.id), { ok: true, skipped: true }, 'not published');
  const ch = w.text('panel');
  await svc.publishPanel({ guild: w.guild, panelId: panel.id, channel: ch, settings: null });

  await svc.addOption(panel.id, G(54), { label: 'Another', emoji: null, description: null, buttonStyle: 2, categoryId: null });
  assert.deepEqual(await svc.refreshPanelMessage(w.guild, panel.id), { ok: true });
  assert.equal(ch.published.edited.components[0].toJSON().components.length, 2);

  // No access (permissions).
  ch.permissionsFor = () => ({ has: () => false });
  const noAccess = await svc.refreshPanelMessage(w.guild, panel.id);
  assert.deepEqual([noAccess.ok, noAccess.code, noAccess.channelId], [false, 'NO_ACCESS', ch.id]);
  assert.match(svc.refreshNote(noAccess), /missing access/);
  ch.permissionsFor = () => w.guild.botPerms;

  // The message is gone (10008): the reference is cleared.
  const id = ch.published.id;
  ch.published = null;
  const gone = await svc.refreshPanelMessage(w.guild, panel.id);
  assert.deepEqual([gone.ok, gone.code], [false, 'GONE']);
  assert.equal((await svc.findPanel(G(54), panel.id)).messageId, null);
  assert.ok(id);

  // The channel vanished: only a confirmed 10003 clears; anything else is an error and keeps the reference.
  await svc.publishPanel({ guild: w.guild, panelId: panel.id, channel: ch, settings: null });
  w.guild.channels.cache.delete(ch.id);
  w.guild.fetchError = discordError(50001);
  const unsure = await svc.refreshPanelMessage(w.guild, panel.id);
  assert.deepEqual([unsure.ok, unsure.code], [false, 'ERROR']);
  assert.equal((await svc.findPanel(G(54), panel.id)).channelId, ch.id);
  w.guild.fetchError = null;
  const cleared = await svc.refreshPanelMessage(w.guild, panel.id);
  assert.equal(cleared.code, 'GONE');
  assert.equal((await svc.findPanel(G(54), panel.id)).channelId, null);
});

test('DB: syncManagerOverwrites back-fills the newest tickets and reports failures', { skip: skipDb }, async () => {
  const w = makeWorld(G(55));
  const panel = await newPanel(w, { name: 'Sync' });
  const chans = [];
  for (let i = 1; i <= 3; i++) {
    const c = w.text(`t${i}`);
    chans.push(c);
    await prisma.ticket.create({ data: { guildId: G(55), panelId: panel.id, number: i, channelId: c.id, ownerId: `${i}`, status: i === 3 ? 'CLOSED' : 'OPEN' } });
  }
  await prisma.ticket.create({ data: { guildId: G(55), panelId: panel.id, number: 4, channelId: 'x4', ownerId: '4', status: 'DELETED' } });
  await prisma.ticket.create({ data: { guildId: G(55), panelId: panel.id, number: 5, channelId: 'uncached', ownerId: '5', status: 'OPEN' } });
  chans[1].overwriteError = discordError(50013);

  const add = await svc.syncManagerOverwrites({ guild: w.guild, panelId: panel.id, roleId: 'R9', mode: 'add' });
  assert.deepEqual(add, { updated: 2, failed: 1, skipped: 1 });
  for (const c of [chans[0], chans[2]]) {
    const call = c.calls.find((x) => x[0] === 'overwrite.edit');
    assert.equal(call[1], 'R9');
    assert.deepEqual(call[2], { ViewChannel: true, SendMessages: true, ReadMessageHistory: true, AttachFiles: true, EmbedLinks: true });
    assert.equal(call[3].type, OverwriteType.Role);
  }
  chans[1].overwriteError = null;
  const remove = await svc.syncManagerOverwrites({ guild: w.guild, panelId: panel.id, roleId: 'R9', mode: 'remove' });
  assert.equal(remove.updated, 3);
  assert.ok(chans[0].calls.some((x) => x[0] === 'overwrite.delete' && x[1] === 'R9'));
});

test('DB: deletePanel removes the panel, its message and its options; tickets keep their rows', { skip: skipDb }, async () => {
  const w = makeWorld(G(56));
  const panel = await newPanel(w, { name: 'Del' });
  const ch = w.text('panel');
  await svc.publishPanel({ guild: w.guild, panelId: panel.id, channel: ch, settings: null });
  const t = await open(w, await svc.findPanel(G(56), panel.id), w.member('5601'));
  const { open: openCount } = await svc.deletePanel(w.guild, await svc.findPanel(G(56), panel.id));
  assert.equal(openCount, 1);
  assert.equal(await svc.findPanel(G(56), panel.id), null);
  assert.equal(await prisma.ticketPanelOption.count({ where: { panelId: panel.id } }), 0);
  assert.ok(ch.calls.some((c) => c[0] === 'messages.delete'));
  const row = await ticketRow(t.ticket.id);
  assert.deepEqual([row.panelId, row.optionId, row.status], [null, null, 'OPEN']);
  // Orphaned tickets still work: staff are moderators and up only, and no category moves happen.
  const staff = w.member('5602', { roles: ['R1'] });
  assert.equal((await svc.closeTicket({ client: w.client, guild: w.guild, ticket: { id: t.ticket.id }, actor: staff, settings: null, source: 'command' })).code, 'NOT_OWNER_OR_STAFF');
  const admin = w.member('5603', { admin: true });
  assert.equal((await svc.claimTicket({ client: w.client, guild: w.guild, ticket: { id: t.ticket.id }, actor: admin, settings: null, source: 'command' })).ok, true);
});

// ── Component handlers against the database ──────────────────────────────────

/** A fake interaction that records every call. Visibility follows deferReply like discord.js. */
function makeInteraction(w, member, over = {}) {
  const calls = [];
  const i = {
    id: nextId(), guild: w.guild, guildId: w.guild.id, user: member.user, member, createdTimestamp: Date.now(),
    channelId: over.channel?.id ?? null, replied: false, deferred: false, ephemeral: null, calls,
    inCachedGuild: () => true, isRepliable: () => true, isAnySelectMenu: () => false, isFromMessage: () => false,
    message: { components: [] },
    reply: async (p) => { calls.push(['reply', p]); i.replied = true; i.ephemeral = Boolean(p?.ephemeral); return { resource: { message: { id: 'reply' } } }; },
    deferReply: async (o) => { calls.push(['deferReply', o]); i.deferred = true; i.ephemeral = Boolean(o?.ephemeral); },
    deferUpdate: async () => { calls.push(['deferUpdate']); i.deferred = true; },
    update: async (p) => { calls.push(['update', p]); i.replied = true; },
    editReply: async (p) => { calls.push(['editReply', p]); i.replied = true; return { id: 'edit', createMessageComponentCollector: () => makeCollector(p) }; },
    followUp: async (p) => { calls.push(['followUp', p]); i.replied = true; return { id: 'follow', createMessageComponentCollector: () => makeCollector(p) }; },
    showModal: async (m) => { calls.push(['showModal', m]); },
    ...over,
  };
  return i;
}

// The confirm dialog (awaitConfirm) attaches a collector to the message it sent. `confirmAnswer` decides what the "user" clicks.
let confirmAnswer = 'yes';
function makeCollector(payload) {
  const first = payload?.components?.[0]?.components?.[0]?.data?.custom_id ?? '';
  const emitter = new EventEmitter();
  if (first.startsWith('confirm:')) {
    const nonce = first.split(':')[1];
    queueMicrotask(() => {
      emitter.emit('collect', { customId: `confirm:${nonce}:${confirmAnswer}`, update: async () => {} });
      emitter.emit('end');
    });
  }
  return emitter;
}

const embedOf = (call) => call[1].embeds[0].data;
const titles = (i) => i.calls.map((c) => c[1]?.embeds?.[0]?.data?.title).filter(Boolean);

test('DB: the Open button opens a ticket: defers first, then answers with a link', { skip: skipDb }, async () => {
  const w = makeWorld(G(60));
  const panel = await newPanel(w, { name: 'Click' });
  const member = w.member('9001');
  const i = makeInteraction(w, member, { customId: `ticket:open:${panel.id}:${panel.options[0].id}` });
  await svc.handleTicketButton(i, w.client);

  assert.deepEqual(i.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  assert.equal(i.calls[0][1].ephemeral, true);
  assert.equal(embedOf(i.calls[1]).title, '✅  Ticket Opened');
  const channel = [...w.guild.channels.cache.values()].find((c) => c.name === 'ticket-0001');
  assert.ok(embedOf(i.calls[1]).description.includes(`<#${channel.id}>`));
  assert.equal(await prisma.ticket.count({ where: { guildId: G(60), ownerId: '9001', status: 'OPEN' } }), 1);

  // A second click: the cap answers at once, before any acknowledgement, and creates nothing.
  const again = makeInteraction(w, member, { customId: `ticket:open:${panel.id}:${panel.options[0].id}` });
  await svc.handleTicketButton(again, w.client);
  assert.deepEqual(again.calls.map((c) => c[0]), ['reply']);
  assert.equal(embedOf(again.calls[0]).title, '⚠️  Ticket Already Open');
  assert.ok(embedOf(again.calls[0]).description.includes(`<#${channel.id}>`));
  assert.equal(again.calls[0][1].ephemeral, true);

  // A removed option and a deleted panel.
  const noOption = makeInteraction(w, w.member('9002'), { customId: `ticket:open:${panel.id}:gone` });
  await svc.handleTicketButton(noOption, w.client);
  assert.equal(embedOf(noOption.calls[0]).title, '❌  Option Not Found');
  const noPanel = makeInteraction(w, w.member('9003'), { customId: `ticket:open:nope:${panel.options[0].id}` });
  await svc.handleTicketButton(noPanel, w.client);
  assert.equal(embedOf(noPanel.calls[0]).title, '❌  Panel Not Found');
});

test('DB: the dropdown resets itself with update() and answers by follow-up', { skip: skipDb }, async () => {
  const w = makeWorld(G(61));
  const panel = await newPanel(w, { name: 'Select', create: { style: 'SELECT' } });
  const i = makeInteraction(w, w.member('9101'), {
    customId: `ticket-select:${panel.id}`, values: [panel.options[0].id], isAnySelectMenu: () => true, message: { components: [{ type: 1 }] },
  });
  await svc.handleTicketSelect(i, w.client);
  assert.deepEqual(i.calls.map((c) => c[0]), ['update', 'followUp']);
  assert.deepEqual(i.calls[0][1], { components: [{ type: 1 }] });
  assert.equal(i.calls[1][1].ephemeral, true);
  assert.equal(embedOf(i.calls[1]).title, '✅  Ticket Opened');
  assert.equal(await prisma.ticket.count({ where: { guildId: G(61) } }), 1);
});

test('DB: with ask_reason the click shows the form first and nothing else', { skip: skipDb }, async () => {
  const w = makeWorld(G(62));
  const panel = await newPanel(w, { name: 'Ask', create: { askReason: true } });
  const member = w.member('9201');
  const i = makeInteraction(w, member, { customId: `ticket:open:${panel.id}:${panel.options[0].id}` });
  await svc.handleTicketButton(i, w.client);
  assert.deepEqual(i.calls.map((c) => c[0]), ['showModal']);
  const modal = i.calls[0][1].toJSON();
  assert.equal(modal.custom_id, `ticket-modal:${panel.id}:${panel.options[0].id}`);
  assert.equal(await prisma.ticket.count({ where: { guildId: G(62) } }), 0);

  // The submit opens the ticket with the reason (a button click: the message is updated first).
  const submit = makeInteraction(w, member, {
    customId: modal.custom_id, isFromMessage: () => true, message: { components: [] },
    fields: { getTextInputValue: (id) => (id === 'ticket-reason' ? '  My order <3 is late  ' : '') },
  });
  await svc.handleTicketModal(submit, w.client);
  assert.deepEqual(submit.calls.map((c) => c[0]), ['update', 'followUp']);
  assert.equal(embedOf(submit.calls[1]).title, '✅  Ticket Opened');
  const row = await prisma.ticket.findFirst({ where: { guildId: G(62), ownerId: '9201' } });
  assert.equal(row.reason, 'My order <3 is late');

  // A slash-style submit (not from a message) defers instead; an empty reason is stored as null.
  const member2 = w.member('9202');
  const submit2 = makeInteraction(w, member2, { customId: modal.custom_id, fields: { getTextInputValue: () => '   ' } });
  await svc.handleTicketModal(submit2, w.client);
  assert.deepEqual(submit2.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  assert.equal((await prisma.ticket.findFirst({ where: { guildId: G(62), ownerId: '9202' } })).reason, null);

  // The module toggle is checked here because the prefix is not wrapped.
  await updateSettings(G(62), { ticketsEnabled: false });
  const off = makeInteraction(w, w.member('9203'), { customId: modal.custom_id, fields: { getTextInputValue: () => 'x' } });
  await svc.handleTicketModal(off, w.client);
  assert.deepEqual(off.calls.map((c) => c[0]), ['reply']);
  assert.equal(embedOf(off.calls[0]).title, '⚠️  Module Disabled');
  assert.equal(await prisma.ticket.count({ where: { guildId: G(62), ownerId: '9203' } }), 0);
  await updateSettings(G(62), { ticketsEnabled: true });

  // A stale form: option removed, panel removed.
  const stale = makeInteraction(w, w.member('9204'), { customId: `ticket-modal:${panel.id}:gone`, fields: { getTextInputValue: () => 'x' } });
  await svc.handleTicketModal(stale, w.client);
  assert.equal(embedOf(stale.calls.at(-1)).title, '❌  Option Not Found');
  const stale2 = makeInteraction(w, w.member('9205'), { customId: `ticket-modal:nope:${panel.options[0].id}`, fields: { getTextInputValue: () => 'x' } });
  await svc.handleTicketModal(stale2, w.client);
  assert.equal(embedOf(stale2.calls.at(-1)).title, '❌  Panel Not Found');
});

/** A lifecycle button click on a ticket's message. */
function buttonClick(tw, actor, action, over = {}) {
  return makeInteraction(tw.w, actor, {
    customId: `ticket:${action}:${tw.ticket.id}`, channel: tw.channel,
    message: { components: [buildIntroRow(tw.ticket.id)] }, ...over,
  });
}

test('DB: lifecycle buttons defer with deferUpdate, edit only the clicked message on success and follow up on errors', { skip: skipDb }, async () => {
  const tw = await ticketWorld(63);
  const { w, owner, staff, ticket } = tw;

  // The opener cannot claim: an ephemeral follow-up, the message is never overwritten.
  const denied = buttonClick(tw, owner, 'claim');
  await svc.handleTicketButton(denied, w.client);
  assert.deepEqual(denied.calls.map((c) => c[0]), ['deferUpdate', 'followUp']);
  assert.equal(denied.calls[1][1].ephemeral, true);
  assert.equal(embedOf(denied.calls[1]).title, '❌  Staff Only');

  // Staff claim: the Claim button is disabled on the clicked message.
  const claim = buttonClick(tw, staff, 'claim');
  await svc.handleTicketButton(claim, w.client);
  assert.deepEqual(claim.calls.map((c) => c[0]), ['deferUpdate', 'editReply']);
  const comps = claim.calls[1][1].components[0].components;
  assert.deepEqual(comps.map((c) => c.disabled ?? false), [false, true]);
  assert.equal((await ticketRow(ticket.id)).status, 'CLAIMED');

  // The opener closes: every button of the clicked message is disabled; the close notice carries the Reopen/Transcript/Delete row.
  const close = buttonClick(tw, owner, 'close');
  await svc.handleTicketButton(close, w.client);
  assert.deepEqual(close.calls.map((c) => c[0]), ['deferUpdate', 'editReply']);
  assert.equal(close.calls[1][1].components[0].components.every((c) => c.disabled), true);
  assert.equal((await ticketRow(ticket.id)).status, 'CLOSED');
  assert.equal(tw.channel.sent.at(-1).embeds[0].data.title, '🔒 Ticket Closed');

  // Closing again: an error follow-up and no edit.
  const twice = buttonClick(tw, owner, 'close');
  await svc.handleTicketButton(twice, w.client);
  assert.deepEqual(twice.calls.map((c) => c[0]), ['deferUpdate', 'followUp']);
  assert.equal(embedOf(twice.calls[1]).title, '⚠️  Already Closed');

  // The delete button needs staff, and refuses an open ticket. Reopen via the button first.
  const reopen = buttonClick(tw, staff, 'reopen', { message: { components: [svc.buildClosedRow(ticket.id)] } });
  await svc.handleTicketButton(reopen, w.client);
  assert.deepEqual(reopen.calls.map((c) => c[0]), ['deferUpdate', 'editReply']);
  assert.equal((await ticketRow(ticket.id)).status, 'CLAIMED');
  const early = buttonClick(tw, staff, 'delete', { message: { components: [svc.buildClosedRow(ticket.id)] } });
  await svc.handleTicketButton(early, w.client);
  assert.deepEqual(early.calls.map((c) => c[0]), ['deferUpdate', 'followUp']);
  assert.equal(embedOf(early.calls[1]).title, '⚠️  Close It First');
  assert.ok(w.guild.channels.cache.has(tw.channel.id));

  // A button copied into another channel, and an unknown ticket.
  const elsewhere = buttonClick(tw, staff, 'close', { channelId: 'other-channel' });
  await svc.handleTicketButton(elsewhere, w.client);
  assert.equal(embedOf(elsewhere.calls.at(-1)).title, '❌  Ticket Not Found');
  const unknown = buttonClick(tw, staff, 'close', { customId: 'ticket:close:nope' });
  await svc.handleTicketButton(unknown, w.client);
  assert.equal(embedOf(unknown.calls.at(-1)).title, '❌  Ticket Not Found');
});

test('DB: the Transcript button defers with a reply and answers with the file for staff only', { skip: skipDb }, async () => {
  const tw = await ticketWorld(64);
  const stranger = tw.w.member('stranger2');
  const denied = buttonClick(tw, tw.owner, 'transcript');
  await svc.handleTicketButton(denied, tw.w.client);
  assert.deepEqual(denied.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  assert.equal(embedOf(denied.calls[1]).title, '❌  Staff Only');
  assert.ok(stranger);

  const ok = buttonClick(tw, tw.staff, 'transcript');
  await svc.handleTicketButton(ok, tw.w.client);
  assert.deepEqual(ok.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  assert.equal(ok.calls[0][1].ephemeral, true);
  assert.equal(ok.calls[1][1].files[0].name, 'transcript-ticket-0001.html');

  const again = buttonClick(tw, tw.staff, 'transcript');
  await svc.handleTicketButton(again, tw.w.client);
  assert.equal(embedOf(again.calls[1]).title, '⚠️  Please Wait');
});

test('DB: the Delete button deletes a closed ticket', { skip: skipDb }, async () => {
  const tw = await ticketWorld(65);
  assert.equal((await svc.closeTicket(tw.ctx(tw.staff))).ok, true);
  const click = buttonClick(tw, tw.staff, 'delete', { message: { components: [svc.buildClosedRow(tw.ticket.id)] } });
  await svc.handleTicketButton(click, tw.w.client);
  assert.deepEqual(click.calls.map((c) => c[0]), ['deferUpdate'], 'nothing is sent to the interaction of a channel that is about to vanish');
  assert.equal((await ticketRow(tw.ticket.id)).status, 'DELETED');
  assert.equal(tw.w.guild.channels.cache.has(tw.channel.id), false);
});

// ── The template form ─────────────────────────────────────────────────────────

function templateSubmit(w, member, customId, values, over = {}) {
  const chan = over.channel ?? w.text('cmd-channel');
  return makeInteraction(w, member, {
    customId, channel: chan, channelId: chan.id,
    fields: { getTextInputValue: (id) => { if (!(id in values)) throw new Error(`no field ${id}`); return values[id]; } },
    ...over,
  });
}
const FORM = (o = {}) => ({ 'tpl-content': '', 'tpl-title': '', 'tpl-description': '', 'tpl-color': '', 'tpl-footer': '', ...o });

test('DB: the template form saves the panel message, the intro and the option intro', { skip: skipDb }, async () => {
  const w = makeWorld(G(66));
  const panel = await newPanel(w, { name: 'Forms' });
  const admin = w.member('8001', { admin: true });
  const pid = panel.id;

  const saved = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:panel`, FORM({ 'tpl-title': 'Hello {server}', 'tpl-description': 'Pick {nonsense} one', 'tpl-color': '#ff0000' }));
  await svc.handleTicketModal(saved, w.client);
  assert.deepEqual(saved.calls.map((c) => c[0]), ['deferReply', 'editReply', 'followUp']);
  assert.equal(saved.calls[0][1].ephemeral, false, 'admin replies are public');
  const reply = embedOf(saved.calls[1]);
  assert.equal(reply.title, '✅  Message Saved');
  assert.ok(reply.description.includes('Saved the panel message of **Forms**.'));
  assert.ok(reply.description.includes('Unknown variables (left as typed): {nonsense}'));
  assert.ok(reply.description.includes('Variables: {server} {server.member_count} {channel}'));
  const preview = saved.calls[2][1];
  assert.deepEqual(preview.allowedMentions, { parse: [] });
  assert.ok(preview.content.startsWith('**Preview**'));
  assert.equal(preview.embeds[0].data.title, 'Hello Fake Guild');
  const stored = (await svc.findPanel(G(66), pid)).template;
  assert.deepEqual([stored.embed.title, stored.embed.color, stored.buttons], ['Hello {server}', 0xff0000, []]);

  // The intro of the whole panel, then of one option.
  const intro = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:intro`, FORM({ 'tpl-content': '{user} welcome to {panel}', 'tpl-title': 'T#{number}' }));
  await svc.handleTicketModal(intro, w.client);
  assert.ok(embedOf(intro.calls[1]).description.includes('Saved the ticket intro of **Forms**.'));
  assert.ok(embedOf(intro.calls[1]).description.includes('Variables: {user} {user.display_name} {number}'));
  assert.equal(embedOf(intro.calls[1]).description.includes('Unknown variables'), false, 'intro variables are all known');
  assert.equal((await svc.findPanel(G(66), pid)).introTemplate.content, '{user} welcome to {panel}');
  const optId = panel.options[0].id;
  const optIntro = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:intro:${optId}`, FORM({ 'tpl-content': 'option intro' }));
  await svc.handleTicketModal(optIntro, w.client);
  assert.ok(embedOf(optIntro.calls[1]).description.includes('for **Open Ticket**'));
  assert.equal((await svc.findPanel(G(66), pid)).options[0].introTemplate.content, 'option intro');

  // ... and an opened ticket uses the option intro.
  const r = await open(w, await svc.findPanel(G(66), pid), w.member('8002'));
  assert.equal(r.channel.sent[0].content, 'option intro');

  // An empty form resets.
  const reset = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:panel`, FORM());
  await svc.handleTicketModal(reset, w.client);
  assert.equal(embedOf(reset.calls[1]).title, '✅  Message Reset');
  assert.equal((await svc.findPanel(G(66), pid)).template, null);
  const resetOpt = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:intro:${optId}`, FORM());
  await svc.handleTicketModal(resetOpt, w.client);
  assert.equal((await svc.findPanel(G(66), pid)).options[0].introTemplate, null);
});

test('DB: the template form refuses bad input, stale forms and unpingable {managers}', { skip: skipDb }, async () => {
  const w = makeWorld(G(67));
  w.role('PR', { mentionable: false });
  w.role('OK', { mentionable: true });
  const panel = await newPanel(w, { name: 'Bad', data: { managerRoleIds: ['PR'] } });
  const admin = w.member('8101', { admin: true });
  const pid = panel.id;
  const before = (await svc.findPanel(G(67), pid)).template;

  const badColor = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:panel`, FORM({ 'tpl-title': 'x', 'tpl-color': 'not-a-color' }));
  await svc.handleTicketModal(badColor, w.client);
  assert.equal(embedOf(badColor.calls[1]).title, '❌  Invalid Message');
  assert.ok(embedOf(badColor.calls[1]).description.includes('Colour must be a hex value'));
  assert.equal((await svc.findPanel(G(67), pid)).template, before, 'nothing was saved');

  const staleForm = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:panel`, { 'tpl-title': 'only one field' });
  await svc.handleTicketModal(staleForm, w.client);
  assert.ok(embedOf(staleForm.calls[1]).description.includes('out of date'));

  const badTarget = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:wat`, FORM({ 'tpl-title': 'x' }));
  await svc.handleTicketModal(badTarget, w.client);
  assert.equal(embedOf(badTarget.calls[1]).title, '❌  Form Expired');
  const gone = templateSubmit(w, admin, 'ticket-modal:tpl:nope:panel', FORM({ 'tpl-title': 'x' }));
  await svc.handleTicketModal(gone, w.client);
  assert.equal(embedOf(gone.calls[1]).title, '❌  Panel Not Found');
  const badOption = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:intro:nope`, FORM({ 'tpl-title': 'x' }));
  await svc.handleTicketModal(badOption, w.client);
  assert.equal(embedOf(badOption.calls[1]).title, '❌  Option Not Found');

  // {managers} needs a role the admin could ping themselves (the form is submitted from a channel where they lack Mention Everyone).
  const chan = w.text('plain');
  chan.permissionsFor = () => ({ has: () => false });
  const refused = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:intro`, FORM({ 'tpl-content': '{managers} help' }), { channel: chan });
  await svc.handleTicketModal(refused, w.client);
  assert.equal(embedOf(refused.calls[1]).title, '❌  Role Not Pingable');
  assert.ok(embedOf(refused.calls[1]).description.includes('<@&PR> is not mentionable'));
  assert.equal((await svc.findPanel(G(67), pid)).introTemplate, null);
  // Without the token the same intro saves; with a mentionable role it saves too.
  const fine = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:intro`, FORM({ 'tpl-content': 'help' }), { channel: chan });
  await svc.handleTicketModal(fine, w.client);
  assert.equal(embedOf(fine.calls[1]).title, '✅  Message Saved');
  await svc.updatePanelFields(pid, G(67), { managerRoleIds: ['OK'] });
  const pingable = templateSubmit(w, admin, `ticket-modal:tpl:${pid}:intro`, FORM({ 'tpl-content': '{managers} help' }), { channel: chan });
  await svc.handleTicketModal(pingable, w.client);
  assert.equal(embedOf(pingable.calls[1]).title, '✅  Message Saved');
  assert.equal((await svc.findPanel(G(67), pid)).introTemplate.content, '{managers} help');

  // A moderator (not admin) is refused even though modals bypass the router's level check.
  const mod = w.member('8102', { mod: true });
  const modTry = templateSubmit(w, mod, `ticket-modal:tpl:${pid}:panel`, FORM({ 'tpl-title': 'x' }));
  await svc.handleTicketModal(modTry, w.client);
  assert.deepEqual(modTry.calls.map((c) => c[0]), ['reply']);
  assert.equal(embedOf(modTry.calls[0]).title, '❌  Insufficient Permissions');
});

test('DB: the template form keeps working while the module is off, and refreshes a published panel', { skip: skipDb }, async () => {
  const w = makeWorld(G(68));
  const panel = await newPanel(w, { name: 'Off' });
  const ch = w.text('panel');
  await svc.publishPanel({ guild: w.guild, panelId: panel.id, channel: ch, settings: null });
  await updateSettings(G(68), { ticketsEnabled: false });
  const admin = w.member('8201', { admin: true });
  const i = templateSubmit(w, admin, `ticket-modal:tpl:${panel.id}:panel`, FORM({ 'tpl-title': 'New title', 'tpl-description': 'Fresh' }));
  await svc.handleTicketModal(i, w.client);
  assert.equal(embedOf(i.calls[1]).title, '✅  Message Saved');
  assert.equal(ch.published.edited.embeds[0].data.title, 'New title', 'the published message was refreshed');
  assert.equal(ch.published.edited.components[0].toJSON().components.length, 1, 'the buttons survive the edit');
  await updateSettings(G(68), { ticketsEnabled: true });
});

// ── The commands against the database ─────────────────────────────────────────

/** A fake slash-command interaction. `opts` maps option names to values by type. */
function command(w, member, { group = null, sub, strings = {}, ints = {}, bools = {}, channels = {}, roles = {}, users = {}, members = {}, focused = null, channel = null }) {
  const get = (map) => (name, required) => {
    if (name in map) return map[name];
    if (required) throw new Error(`required option ${name} missing`);
    return null;
  };
  const here = channel ?? w.text('cmd-here');
  const i = makeInteraction(w, member, {
    channel: here, channelId: here.id, client: w.client,
    options: {
      getSubcommandGroup: () => group, getSubcommand: () => sub,
      get: (name) => channels[name] ?? roles[name] ?? null,
      getString: get(strings), getInteger: get(ints), getBoolean: get(bools), getChannel: get(channels), getRole: get(roles),
      getUser: get(users), getMember: (n) => members[n] ?? null,
      getFocused: () => focused,
    },
    responded: false,
    respond: async (choices) => { i.responded = choices; },
  });
  return i;
}
const lastEmbed = (i) => [...i.calls].reverse().find((c) => c[1]?.embeds?.length)?.[1].embeds[0].data;

const runPanel = async (w, admin, spec) => {
  const i = command(w, admin, spec);
  await panelCmd.execute(i, w.client);
  return i;
};

test('DB: /ticket-panel create, edit, list and option commands', { skip: skipDb }, async () => {
  const w = makeWorld(G(70));
  const admin = w.member('7000', { admin: true });
  const target = w.text('panel-here');

  // create (not published)
  const created = await runPanel(w, admin, { sub: 'create', strings: { name: 'Support', style: 'SELECT', name_format: '{user.name}-{number}' }, bools: { ask_reason: true }, ints: { max_open: 2 } });
  assert.equal(created.calls[0][0], 'deferReply');
  assert.equal(created.calls[0][1].ephemeral, false, 'admin replies are public');
  const e1 = lastEmbed(created);
  assert.equal(e1.title, '✅  Panel Created');
  assert.ok(e1.description.includes('Created **Support** (Dropdown) with an **Open Ticket** ✉️ option.'));
  assert.ok(e1.description.includes('`/ticket-panel publish`') && e1.description.includes('Ticket Manager roles'));
  const panel = await svc.findPanel(G(70), 'support');
  assert.deepEqual([panel.style, panel.askReason, panel.maxOpenPerUser, panel.channelNameFormat], ['SELECT', true, 2, '{user.name}-{number}']);

  // name problems
  assert.equal(lastEmbed(await runPanel(w, admin, { sub: 'create', strings: { name: 'SUPPORT' } })).title, '❌  Name Taken');
  assert.equal(lastEmbed(await runPanel(w, admin, { sub: 'create', strings: { name: 'a\nb' } })).title, '❌  Invalid Name');
  const badFormat = lastEmbed(await runPanel(w, admin, { sub: 'create', strings: { name: 'X', name_format: '{server}' } }));
  assert.equal(badFormat.title, '❌  Invalid Name Format');
  assert.ok(badFormat.description.includes('`{server}`'));
  assert.equal(await svc.findPanel(G(70), 'X'), null);

  // create + publish in one go
  const both = await runPanel(w, admin, { sub: 'create', strings: { name: 'Both' }, channels: { channel: target } });
  assert.ok(lastEmbed(both).description.includes('Jump to panel'));
  assert.ok(target.sent.length === 1);
  assert.equal((await svc.findPanel(G(70), 'both')).channelId, target.id);
  // create + a channel the bot cannot post in: the panel exists, with a warning
  const mute = w.text('mute');
  mute.permissionsFor = () => ({ has: () => false });
  const warn = await runPanel(w, admin, { sub: 'create', strings: { name: 'Muted' }, channels: { channel: mute } });
  assert.equal(lastEmbed(warn).title, '✅  Panel Created');
  assert.ok(lastEmbed(warn).description.includes('created but not published'));
  assert.ok(await svc.findPanel(G(70), 'muted'));

  // edit: no options shows the details
  const details = lastEmbed(await runPanel(w, admin, { sub: 'edit', strings: { panel: panel.id } }));
  assert.equal(details.title, '🎫 Support');
  assert.deepEqual(details.fields.map((f) => f.name), ['Style', 'Published', 'Ask for a reason', 'Open tickets per member', 'Channel name', 'Manager roles', 'Categories', 'Transcripts', 'Options']);
  assert.equal(details.fields.find((f) => f.name === 'Manager roles').value, 'none — only moderators and admins');
  // edit: changes
  const edited = lastEmbed(await runPanel(w, admin, {
    sub: 'edit', strings: { panel: 'support', name: 'Help Desk', style: 'BUTTONS', name_format: 'help-{number}' }, bools: { ask_reason: false }, ints: { max_open: 3 },
  }));
  assert.equal(edited.title, '✅  Panel Updated');
  for (const part of ['Name: **Support** → **Help Desk**', 'Style: **Buttons**', 'Ask for a reason: **No**', 'Open tickets per member: **3**', 'Channel name: `help-{number}` — preview `help-0001`']) {
    assert.ok(edited.description.includes(part), part);
  }
  const after = await svc.findPanel(G(70), 'help desk');
  assert.deepEqual([after.style, after.askReason, after.maxOpenPerUser], ['BUTTONS', false, 3]);
  assert.equal(lastEmbed(await runPanel(w, admin, { sub: 'edit', strings: { panel: after.id, name: 'both' } })).title, '❌  Name Taken');
  assert.equal(lastEmbed(await runPanel(w, admin, { sub: 'edit', strings: { panel: after.id, name_format: '' } })).title, '❌  Invalid Name Format');
  assert.equal(lastEmbed(await runPanel(w, admin, { sub: 'edit', strings: { panel: 'nope', name: 'z' } })).title, '❌  Panel Not Found');

  // list
  const listed = await runPanel(w, admin, { sub: 'list' });
  const lt = lastEmbed(listed);
  assert.equal(lt.title, '🎫 Ticket panels (3/25)');
  assert.ok(lt.description.includes('**Help Desk** — Buttons · 1 option(s) · 0 manager role(s) · 0 active ticket(s) · not published'));
  assert.ok(lt.description.includes(`<#${target.id}>`));
  const empty = makeWorld(G(71));
  assert.equal(lastEmbed(await runPanel(empty, empty.member('7100', { admin: true }), { sub: 'list' })).title, 'ℹ️  No Ticket Panels');

  // options
  const addOpt = await runPanel(w, admin, { group: 'option', sub: 'add', strings: { panel: after.id, label: 'Billing', emoji: '💰', description: 'Invoices' }, ints: { color: 3 } });
  assert.equal(lastEmbed(addOpt).title, '✅  Option Added');
  assert.ok(lastEmbed(addOpt).description.includes('Added 💰 **Billing** to **Help Desk** (2/25).'));
  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'add', strings: { panel: after.id, label: 'billing' } })).title, '❌  Duplicate Option');
  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'add', strings: { panel: after.id, label: 'X', emoji: 'abc' } })).title, '❌  Invalid Emoji');
  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'add', strings: { panel: after.id, label: '   ' } })).title, '❌  Invalid Label');
  const lockedCat = w.category('Locked');
  lockedCat.permissionsFor = () => ({ has: () => false });
  const lockedTry = lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'add', strings: { panel: after.id, label: 'Cat' }, channels: { category: lockedCat } }));
  assert.equal(lockedTry.title, '❌  Missing Permissions');
  assert.ok(lockedTry.description.includes('**Locked**'));

  const listOpts = lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'list', strings: { panel: after.id } }));
  assert.equal(listOpts.title, '🎫 Help Desk — options (2/25)');
  assert.ok(listOpts.description.includes('**1.** ✉️ **Open Ticket** — Blue'));
  assert.ok(listOpts.description.includes('**2.** 💰 **Billing** — Green · Invoices'));

  const editOpt = await runPanel(w, admin, { group: 'option', sub: 'edit', strings: { panel: after.id, option: 'billing', label: 'Payments', emoji: 'none', description: 'none' }, ints: { color: 4 } });
  assert.equal(lastEmbed(editOpt).title, '✅  Option Updated');
  const opts = (await svc.findPanel(G(70), after.id)).options;
  assert.deepEqual([opts[1].label, opts[1].emoji, opts[1].description, opts[1].buttonStyle], ['Payments', null, null, 4]);
  const showOpt = lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'edit', strings: { panel: after.id, option: opts[1].id } }));
  assert.ok(showOpt.description.includes('**Payments** — Red'));
  const both2 = lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'edit', strings: { panel: after.id, option: opts[1].id }, channels: { category: w.category('X') }, bools: { clear_category: true } }));
  assert.equal(both2.title, '❌  Conflicting Options');
  const cat = w.category('OptCat');
  await runPanel(w, admin, { group: 'option', sub: 'edit', strings: { panel: after.id, option: opts[1].id }, channels: { category: cat } });
  assert.equal((await svc.findPanel(G(70), after.id)).options[1].categoryId, cat.id);
  await runPanel(w, admin, { group: 'option', sub: 'edit', strings: { panel: after.id, option: opts[1].id }, bools: { clear_category: true } });
  assert.equal((await svc.findPanel(G(70), after.id)).options[1].categoryId, null);
  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'edit', strings: { panel: after.id, option: 'zzz', label: 'q' } })).title, '❌  Option Not Found');
  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'edit', strings: { panel: after.id, option: opts[0].id, label: 'PAYMENTS' } })).title, '❌  Duplicate Option');

  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'remove', strings: { panel: after.id, option: 'payments' } })).title, '✅  Option Removed');
  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'option', sub: 'remove', strings: { panel: after.id, option: opts[0].id } })).title, '❌  Last Option');
});

test('DB: /ticket-panel publish, categories, transcripts, managers and delete', { skip: skipDb }, async () => {
  const w = makeWorld(G(72));
  const admin = w.member('7200', { admin: true });
  w.role('MGR', { mentionable: true });
  w.role('MGR2');
  const panel = await newPanel(w, { name: 'Cmds' });
  const target = w.text('post-here');

  // publish
  const pub = await runPanel(w, admin, { sub: 'publish', strings: { panel: panel.id }, channels: { channel: target } });
  assert.equal(lastEmbed(pub).title, '✅  Panel Published');
  assert.ok(lastEmbed(pub).description.includes('Ticket Manager roles'), 'warns about the missing manager roles');
  assert.equal(lastEmbed(await runPanel(w, admin, { sub: 'publish', strings: { panel: panel.id }, channels: { channel: target } })).title, '✅  Panel Updated');
  const mute = w.text('mute');
  mute.permissionsFor = () => ({ has: () => false });
  const cannot = lastEmbed(await runPanel(w, admin, { sub: 'publish', strings: { panel: panel.id }, channels: { channel: mute } }));
  assert.equal(cannot.title, '❌  Cannot Post There');
  assert.ok(cannot.description.includes('**View Channel'));
  // no server-wide permissions: refused before posting
  const w0 = makeWorld(G(73), { botCan: true });
  const p0 = await newPanel(w0, { name: 'NoPerms' });
  w0.guild.members.me.permissions = { has: (p) => p === PermissionFlagsBits.ManageChannels };
  const t0 = w0.text('t');
  const refused = await runPanel(w0, w0.member('7300', { admin: true }), { sub: 'publish', strings: { panel: p0.id }, channels: { channel: t0 } });
  assert.equal(lastEmbed(refused).title, '❌  Missing Permissions');
  assert.equal(t0.sent.length, 0);

  // managers
  const add = lastEmbed(await runPanel(w, admin, { group: 'managers', sub: 'add', strings: { panel: panel.id }, roles: { role: { id: 'MGR' } } }));
  assert.equal(add.title, '✅  Ticket Managers Updated');
  assert.ok(add.description.includes('<@&MGR> can now see and handle tickets from **Cmds**.'));
  assert.ok(add.description.includes('Manager roles: <@&MGR> (1/10)'));
  assert.deepEqual((await svc.findPanel(G(72), panel.id)).managerRoleIds, ['MGR']);
  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'managers', sub: 'add', strings: { panel: panel.id }, roles: { role: { id: 'MGR' } } })).title, '⚠️  Already a Manager');
  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'managers', sub: 'add', strings: { panel: panel.id }, roles: { role: { id: G(72) } } })).title, '❌  Invalid Role');
  // an open ticket gets the overwrite back-filled
  const t = await open(w, await svc.findPanel(G(72), panel.id), w.member('7201'));
  const add2 = lastEmbed(await runPanel(w, admin, { group: 'managers', sub: 'add', strings: { panel: panel.id }, roles: { role: { id: 'MGR2' } } }));
  assert.ok(add2.description.includes('Updated 1 existing ticket(s)'));
  assert.ok(t.channel.calls.some((c) => c[0] === 'overwrite.edit' && c[1] === 'MGR2'));
  const rm = lastEmbed(await runPanel(w, admin, { group: 'managers', sub: 'remove', strings: { panel: panel.id }, roles: { role: { id: 'MGR2' } } }));
  assert.ok(rm.description.includes('<@&MGR2> can no longer see and handle tickets'));
  assert.ok(t.channel.calls.some((c) => c[0] === 'overwrite.delete' && c[1] === 'MGR2'));
  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'managers', sub: 'remove', strings: { panel: panel.id }, roles: { role: { id: 'MGR2' } } })).title, '⚠️  Not a Manager');

  // {managers} in an intro makes an unpingable new manager refused
  await svc.updatePanelFields(panel.id, G(72), { introTemplate: { content: '{managers}', buttons: [] } });
  const here = w.text('where-admin-types');
  here.permissionsFor = () => ({ has: () => false });
  const refusedRole = lastEmbed(await runPanel(w, admin, { group: 'managers', sub: 'add', strings: { panel: panel.id }, roles: { role: { id: 'MGR2' } }, channel: here }));
  assert.equal(refusedRole.title, '❌  Role Not Pingable');
  assert.ok(!(await svc.findPanel(G(72), panel.id)).managerRoleIds.includes('MGR2'));
  // ... while a mentionable one is fine
  w.role('MGR3', { mentionable: true });
  assert.equal(lastEmbed(await runPanel(w, admin, { group: 'managers', sub: 'add', strings: { panel: panel.id }, roles: { role: { id: 'MGR3' } }, channel: here })).title, '✅  Ticket Managers Updated');

  // categories: show, set, create_missing
  const shown = lastEmbed(await runPanel(w, admin, { sub: 'categories', strings: { panel: panel.id } }));
  assert.ok(shown.description.includes('**Open:** <#'));
  assert.ok(shown.description.includes('**Claimed:** not set — I create "Claimed Tickets" when it is first needed'));
  const open1 = w.category('MyOpen');
  const locked = w.category('NoPerms');
  locked.permissionsFor = () => ({ has: () => false });
  assert.equal(lastEmbed(await runPanel(w, admin, { sub: 'categories', strings: { panel: panel.id }, channels: { closed: locked } })).title, '❌  Missing Permissions');
  const set = lastEmbed(await runPanel(w, admin, { sub: 'categories', strings: { panel: panel.id }, channels: { open: open1 }, bools: { create_missing: true } }));
  assert.equal(set.title, '✅  Categories Updated');
  assert.ok(set.description.includes(`**Open:** <#${open1.id}> (`));
  assert.ok(set.description.includes('Created **Claimed Tickets**.'));
  assert.ok(set.description.includes('Existing tickets are not moved.'));
  const stored = await svc.findPanel(G(72), panel.id);
  assert.equal(stored.openCategoryId, open1.id);
  assert.ok(stored.claimedCategoryId && stored.closedCategoryId);
  // a full category warns
  const fullCat = w.category('Full');
  for (let i = 0; i < 50; i++) { const c = w.text(`f${i}`); c.parentId = fullCat.id; }
  const warnFull = lastEmbed(await runPanel(w, admin, { sub: 'categories', strings: { panel: panel.id }, channels: { closed: fullCat } }));
  assert.ok(warnFull.description.includes('**Full** is full (50 channels)'));
  // create_missing reuses a same-named category
  const w3 = makeWorld(G(74));
  const reused = w3.category('Tickets');
  const p3 = await newPanel(w3, { name: 'Reuse' });
  const reuse = lastEmbed(await runPanel(w3, w3.member('7400', { admin: true }), { sub: 'categories', strings: { panel: p3.id }, bools: { create_missing: true } }));
  assert.ok(reuse.description.includes('Using the existing **Tickets** category.'));
  assert.equal((await svc.findPanel(G(74), p3.id)).openCategoryId, reused.id);

  // transcripts
  const showT = lastEmbed(await runPanel(w, admin, { sub: 'transcripts', strings: { panel: panel.id } }));
  assert.ok(showT.description.includes('**Channel:** off') && showT.description.includes('**DM the opener:** No'));
  const tc = w.text('transcripts');
  const setT = lastEmbed(await runPanel(w, admin, { sub: 'transcripts', strings: { panel: panel.id }, channels: { channel: tc }, bools: { dm_owner: true } }));
  assert.equal(setT.title, '✅  Transcripts Updated');
  assert.ok(setT.description.includes(`<#${tc.id}>`) && setT.description.includes('**DM the opener:** Yes'));
  const conflict = lastEmbed(await runPanel(w, admin, { sub: 'transcripts', strings: { panel: panel.id }, channels: { channel: tc }, bools: { disable: true } }));
  assert.equal(conflict.title, '❌  Conflicting Options');
  const noAttach = w.text('noattach');
  noAttach.permissionsFor = () => ({ has: () => false });
  assert.equal(lastEmbed(await runPanel(w, admin, { sub: 'transcripts', strings: { panel: panel.id }, channels: { channel: noAttach } })).title, '❌  Cannot Post Transcripts There');
  const off = lastEmbed(await runPanel(w, admin, { sub: 'transcripts', strings: { panel: panel.id }, bools: { disable: true } }));
  assert.ok(off.description.includes('**Channel:** off'));
  assert.equal((await svc.findPanel(G(72), panel.id)).transcriptChannelId, null);
  assert.equal((await svc.findPanel(G(72), panel.id)).dmTranscript, true, 'disable leaves the DM setting alone');

  // delete: cancel, then confirm
  confirmAnswer = 'no';
  const cancel = await runPanel(w, admin, { sub: 'delete', strings: { panel: panel.id } });
  assert.equal(lastEmbed(cancel).title, 'ℹ️  Cancelled');
  assert.ok(await svc.findPanel(G(72), panel.id));
  confirmAnswer = 'yes';
  const del = await runPanel(w, admin, { sub: 'delete', strings: { panel: panel.id } });
  assert.equal(lastEmbed(del).title, '✅  Panel Deleted');
  assert.ok(lastEmbed(del).description.includes('1 ticket(s) keep their channels'));
  assert.equal(await svc.findPanel(G(72), panel.id), null);
  assert.ok(target.calls.some((c) => c[0] === 'messages.delete'), 'the published message was deleted');
});

test('DB: /ticket-panel message opens the form with the right id, title and clamped prefill', { skip: skipDb }, async () => {
  const w = makeWorld(G(75));
  const admin = w.member('7500', { admin: true });
  const panel = await newPanel(w, { name: 'Forms Panel' });
  const opt = panel.options[0];
  await svc.updatePanelFields(panel.id, G(75), { template: { content: null, embed: { title: 'T', description: 'x'.repeat(4096), color: 0xabcdef }, buttons: [] } });

  const i = await runPanel(w, admin, { sub: 'message', strings: { panel: panel.id } });
  assert.deepEqual(i.calls.map((c) => c[0]), ['showModal'], 'no deferral: showModal is the first response');
  const modal = i.calls[0][1].toJSON();
  assert.equal(modal.custom_id, `ticket-modal:tpl:${panel.id}:panel`);
  assert.equal(modal.title, 'Panel message: Forms Panel');
  const inputs = modal.components.flatMap((r) => r.components);
  assert.deepEqual(inputs.map((c) => c.custom_id), ['tpl-content', 'tpl-title', 'tpl-description', 'tpl-color', 'tpl-footer']);
  assert.equal(inputs.find((c) => c.custom_id === 'tpl-description').value.length, 4000);
  assert.equal(inputs.find((c) => c.custom_id === 'tpl-color').value, '#ABCDEF');
  assert.equal(inputs.find((c) => c.custom_id === 'tpl-title').value, 'T');

  // The default panel message is prefilled when none is stored.
  const fresh = await newPanel(w, { name: 'Fresh' });
  const def = await runPanel(w, admin, { sub: 'message', strings: { panel: fresh.id } });
  assert.equal(def.calls[0][1].toJSON().components[1].components[0].value, 'How can we help?');

  // Intro forms: panel-wide and per option (found by label).
  const intro = await runPanel(w, admin, { sub: 'message', strings: { panel: panel.id, type: 'intro' } });
  const introModal = intro.calls[0][1].toJSON();
  assert.equal(introModal.custom_id, `ticket-modal:tpl:${panel.id}:intro`);
  assert.equal(introModal.title, 'Ticket intro: Forms Panel');
  assert.equal(introModal.components[0].components[0].value, '{user}');
  const optForm = await runPanel(w, admin, { sub: 'message', strings: { panel: panel.id, type: 'intro', option: 'open ticket' } });
  const optModal = optForm.calls[0][1].toJSON();
  assert.equal(optModal.custom_id, `ticket-modal:tpl:${panel.id}:intro:${opt.id}`);
  assert.equal(optModal.title, 'Intro: Open Ticket');
  assert.ok(optModal.custom_id.length <= 100);

  // Problems answer ephemerally without a form.
  const noOpt = await runPanel(w, admin, { sub: 'message', strings: { panel: panel.id, type: 'intro', option: 'zzz' } });
  assert.deepEqual(noOpt.calls.map((c) => c[0]), ['reply']);
  assert.equal(embedOf(noOpt.calls[0]).title, '❌  Option Not Found');
  const noPanel = await runPanel(w, admin, { sub: 'message', strings: { panel: 'nope' } });
  assert.equal(embedOf(noPanel.calls[0]).title, '❌  Panel Not Found');
  assert.equal(noPanel.calls[0][1].ephemeral, true);
});

test('DB: /ticket-panel autocomplete serves panels and options', { skip: skipDb }, async () => {
  const w = makeWorld(G(76));
  const admin = w.member('7600', { admin: true });
  const a = await newPanel(w, { name: 'Alpha' });
  await newPanel(w, { name: 'Beta', create: { style: 'SELECT' } });
  await svc.addOption(a.id, G(76), { label: 'Billing', emoji: '💰', description: null, buttonStyle: 1, categoryId: null });
  await svc.addOption(a.id, G(76), { label: 'Custom', emoji: '<:x:123456789012345678>', description: null, buttonStyle: 1, categoryId: null });

  const panels = command(w, admin, { sub: 'edit', focused: { name: 'panel', value: '' } });
  await panelCmd.autocomplete(panels, w.client);
  assert.deepEqual(panels.responded.map((c) => c.name), ['Alpha · buttons', 'Beta · dropdown']);
  const filtered = command(w, admin, { sub: 'edit', focused: { name: 'panel', value: 'bet' } });
  await panelCmd.autocomplete(filtered, w.client);
  assert.deepEqual(filtered.responded.map((c) => c.value.length > 10), [true]);

  const options = command(w, admin, { group: 'option', sub: 'edit', strings: { panel: 'alpha' }, focused: { name: 'option', value: '' } });
  await panelCmd.autocomplete(options, w.client);
  assert.deepEqual(options.responded.map((c) => c.name), ['✉️ Open Ticket', '💰 Billing', 'Custom']);
  const noPanel = command(w, admin, { group: 'option', sub: 'edit', focused: { name: 'option', value: '' } });
  await panelCmd.autocomplete(noPanel, w.client);
  assert.deepEqual(noPanel.responded, []);
  const other = command(w, admin, { sub: 'edit', focused: { name: 'label', value: '' } });
  await panelCmd.autocomplete(other, w.client);
  assert.deepEqual(other.responded, []);
  // Another guild sees nothing.
  const w2 = makeWorld(G(77));
  const foreign = command(w2, w2.member('7700', { admin: true }), { sub: 'edit', focused: { name: 'panel', value: '' } });
  await panelCmd.autocomplete(foreign, w2.client);
  assert.deepEqual(foreign.responded, []);
});

// ── /ticket ───────────────────────────────────────────────────────────────────

const runTicket = async (tw, actor, spec) => {
  const i = command(tw.w, actor, { channel: tw.channel, ...spec });
  await ticketCmd.execute(i, tw.w.client);
  return i;
};

test('DB: /ticket claim, close, reopen, add, remove, rename and transcript', { skip: skipDb }, async () => {
  const tw = await ticketWorld(80);
  const { w, owner, staff, ticket } = tw;

  const notHere = command(w, staff, { sub: 'claim', channel: w.text('elsewhere') });
  await ticketCmd.execute(notHere, w.client);
  assert.deepEqual(notHere.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  assert.equal(notHere.calls[0][1].ephemeral, true, 'every reply is private');
  assert.equal(lastEmbed(notHere).title, '❌  Not a Ticket');

  assert.equal(lastEmbed(await runTicket(tw, owner, { sub: 'claim' })).title, '❌  Staff Only');
  const claim = await runTicket(tw, staff, { sub: 'claim' });
  assert.equal(lastEmbed(claim).title, '✅  Done');
  assert.equal(lastEmbed(claim).description, 'Ticket claimed.');
  assert.equal((await ticketRow(ticket.id)).status, 'CLAIMED');

  const close = await runTicket(tw, owner, { sub: 'close', strings: { reason: '  all fixed  ' } });
  assert.equal(lastEmbed(close).description, 'Ticket closed.');
  assert.equal(tw.channel.sent.at(-1).embeds[0].data.fields[0].value, 'all fixed');
  assert.equal(lastEmbed(await runTicket(tw, owner, { sub: 'close' })).title, '⚠️  Already Closed');
  assert.equal(lastEmbed(await runTicket(tw, staff, { sub: 'reopen' })).description, 'Ticket reopened.');

  const friend = w.member('8802');
  const add = await runTicket(tw, staff, { sub: 'add', users: { user: friend.user }, members: { user: friend } });
  assert.equal(lastEmbed(add).description, 'Member added.');
  assert.equal(lastEmbed(await runTicket(tw, staff, { sub: 'add', users: { user: friend.user }, members: { user: friend } })).title, '⚠️  Already Added');
  assert.equal(lastEmbed(await runTicket(tw, staff, { sub: 'add', users: { user: { id: '1', bot: false } }, members: {} })).title, '❌  Member Not Found');
  assert.equal(lastEmbed(await runTicket(tw, staff, { sub: 'remove', users: { user: friend.user }, members: { user: friend } })).description, 'Member removed.');

  const rename = await runTicket(tw, staff, { sub: 'rename', strings: { name: 'New Name' } });
  assert.equal(lastEmbed(rename).description, 'Channel renamed.');
  assert.equal(tw.channel.name, 'new-name');
  assert.equal(lastEmbed(await runTicket(tw, owner, { sub: 'rename', strings: { name: 'x' } })).title, '❌  Staff Only');

  const transcript = await runTicket(tw, staff, { sub: 'transcript' });
  const reply = transcript.calls.at(-1);
  assert.equal(reply[1].files[0].name, 'transcript-ticket-0001.html');
  assert.equal(lastEmbed(await runTicket(tw, staff, { sub: 'transcript' })).title, '⚠️  Please Wait');
  assert.equal(lastEmbed(await runTicket(tw, owner, { sub: 'transcript' })).title, '❌  Staff Only');

  // Notes are shown under the success text.
  const noted = await ticketWorld(81);
  noted.channel.overwriteError = discordError(50013);
  const closedNote = await runTicket(noted, noted.owner, { sub: 'close' });
  assert.ok(lastEmbed(closedNote).description.startsWith('Ticket closed.\n\n⚠️ '));
});

test('DB: /ticket delete confirms an open ticket, skips the dialog for a closed one and never touches the interaction afterwards', { skip: skipDb }, async () => {
  const a = await ticketWorld(82);
  const b = await ticketWorld(83);
  const c = await ticketWorld(84);

  // Not staff.
  assert.equal(lastEmbed(await runTicket(a, a.owner, { sub: 'delete' })).title, '❌  Staff Only');
  // Cancel at the dialog.
  confirmAnswer = 'no';
  const cancelled = await runTicket(a, a.staff, { sub: 'delete' });
  assert.equal(lastEmbed(cancelled).title, 'ℹ️  Cancelled');
  assert.equal((await ticketRow(a.ticket.id)).status, 'OPEN');
  assert.ok(a.w.guild.channels.cache.has(a.channel.id));

  confirmAnswer = 'yes';
  const [open1, closed1] = await Promise.all([
    runTicket(b, b.staff, { sub: 'delete' }),
    (async () => { await svc.closeTicket(c.ctx(c.staff)); return runTicket(c, c.staff, { sub: 'delete' }); })(),
  ]);
  // Open ticket: the dialog was shown (editReply with the confirm buttons), then the saved notice.
  assert.ok(open1.calls.some((x) => x[1]?.components?.[0]?.components?.[0]?.data?.custom_id?.startsWith('confirm:')));
  assert.ok(titles(open1).includes('✅  Deleting Ticket'));
  // Closed ticket: no dialog at all.
  assert.equal(closed1.calls.some((x) => x[1]?.components?.[0]?.components?.[0]?.data?.custom_id?.startsWith('confirm:')), false);
  assert.ok(titles(closed1).includes('✅  Deleting Ticket'));
  for (const t of [b, c]) {
    assert.equal((await ticketRow(t.ticket.id)).status, 'DELETED');
    assert.equal(t.w.guild.channels.cache.has(t.channel.id), false);
  }
  assert.equal(titles(open1).filter((x) => x.includes('Delete Failed')).length, 0);
});

test('DB: /ticket delete reports a refused delete on the still-valid interaction', { skip: skipDb }, async () => {
  const tw = await ticketWorld(85);
  tw.channel.deleteError = discordError(50013);
  confirmAnswer = 'yes';
  const i = await runTicket(tw, tw.staff, { sub: 'delete' });
  assert.ok(titles(i).includes('❌  Delete Failed'));
  assert.equal((await ticketRow(tw.ticket.id)).status, 'OPEN');
});
