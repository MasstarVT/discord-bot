// pkg-automations tests (pkg-automations.md §8 plus the architect amendments): pure helpers, the evaluator, presets, JSON import
// and export, the plugin and command payload, the runtime (dispatch, loop protection, the action runners) against fakes, the
// /automation option mapping, and the data layer. Offline by default. With TEST_DATABASE_URL (a migrated database) the DB cases also
// run, including a command-level walk through /automation with a fake interaction:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/automations.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres) and setNX answers "Redis down".
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ChannelType, Collection, MessageType, PermissionFlagsBits, RESTJSONErrorCodes } = await import('discord.js');
const svc = await import('../src/services/automationService.js');
const cmd = await import('../src/commands/admin/automation.js');
const { plugin } = await import('../src/plugins/automations.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { PERMISSION_LEVELS, LIMITS } = await import('../src/config/constants.js');
const { renderPlaceholders } = await import('../src/utils/placeholderParser.js');
const { wasDeletedByBot, markBotDeletion } = await import('../src/utils/deletionTracker.js');
const { TimeoutError } = await import('../src/utils/async.js');
const rewardsBus = await import('../src/services/rewards.js');
const { TRIGGERS, CONDITION_TYPES, ACTION_TYPES } = svc;

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
if (!process.env.AUTOMATIONS_TEST_VERBOSE) console.error = console.warn = console.log = console.debug = console.info = () => {};

const GUILD = '955028000000000001';
const GUILD2 = '955028000000000002';
const GUILD3 = '955028000000000003';

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) {
    // The cascade removes the Automation rows. Deleting the settings row only here: ensureGuildSettings memoises.
    await prisma.guildSettings.deleteMany({ where: { guildId: { startsWith: '955028' } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ────────────────────────────────────────────────────────────────────

const BOT = '900000000000000099';
const ID = (n) => String(100000000000000000n + BigInt(n));   // 18-digit fake snowflakes (BigInt: 1e17 + n is not exact as a double)
const flush = () => new Promise((r) => setImmediate(r));

const perms = (missing = []) => ({ has: (bit) => !missing.some((k) => PermissionFlagsBits[k] === bit) });

function fakeChannel(guild, id, { type = ChannelType.GuildText, name = 'general', parentId = null, missing = [], thread = false } = {}) {
  const sent = [];
  const ch = {
    id, type, name, parentId, guild, sent,
    isTextBased: () => true,
    isThread: () => thread,
    permissionsFor: () => perms(missing),
    send: async (payload) => { sent.push(payload); return { id: `sent${sent.length}` }; },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function fakeRole(guild, id, { position = 1, managed = false, mentionable = false } = {}) {
  const role = { id, name: `role-${id.slice(-3)}`, position, managed, mentionable, permissions: { has: () => false } };
  guild.roles.cache.set(id, role);
  return role;
}

function fakeMember(guild, id, roleIds = [], { bot = false } = {}) {
  const roles = new Collection(roleIds.map((r) => [r, { id: r }]));
  const calls = [];
  const member = {
    id, guild, calls,
    displayName: `Member ${id.slice(-3)}`,
    user: { id, bot, username: `user${id.slice(-3)}`, displayAvatarURL: () => 'https://example.test/a.png' },
    roles: {
      cache: roles,
      async add(roleId, reason) { calls.push(['add', roleId, reason]); roles.set(roleId, { id: roleId }); },
      async remove(roleId, reason) { calls.push(['remove', roleId, reason]); roles.delete(roleId); },
    },
  };
  guild.members.cache.set(id, member);
  return member;
}

function fakeGuild(id = GUILD) {
  const guild = {
    id, available: true, ownerId: '1', name: 'Test Guild', memberCount: 10,
    channels: { cache: new Collection() },
    roles: { cache: new Collection() },
    members: {
      cache: new Collection(),
      me: { id: BOT, permissions: { has: () => true }, roles: { highest: { position: 50 } } },
      fetch: async (mid) => { const m = guild.members.cache.get(mid); if (!m) throw Object.assign(new Error('Unknown Member'), { code: 10007 }); return m; },
    },
    emojis: { cache: new Collection() },
    client: { user: { id: BOT }, emojis: { cache: new Collection() } },
  };
  return guild;
}

let messageSeq = 0;
function fakeMessage(guild, channel, author, { content = 'hello world', type = MessageType.Default, attachments = [], reactions = [], pinned = false, system = false } = {}) {
  const id = String(700000000000000000n + BigInt(++messageSeq));
  const calls = { react: [], pin: 0, deleted: 0, thread: [] };
  const msg = {
    id, content, type, author, channel, guild, pinned, system, partial: false, hasThread: false, thread: null,
    member: guild.members.cache.get(author.id) ?? null,
    url: `https://discord.com/channels/${guild.id}/${channel.id}/${id}`,
    createdTimestamp: 1_700_000_000_000,
    attachments: new Collection(attachments.map((a, i) => [String(i), a])),
    reactions: { cache: new Collection(reactions.map((r, i) => [String(i), r])) },
    calls,
    async react(emoji) { calls.react.push(emoji); },
    async pin() { calls.pin++; },
    async delete() { calls.deleted++; },
    async startThread(opts) { calls.thread.push(opts); const t = { id: ID(555), name: opts.name, archived: false, locked: false, members: { added: [], async add(uid) { t.members.added.push(uid); } }, sent: [], isThread: () => true, isTextBased: () => true, permissionsFor: () => perms(), guild, async send(p) { t.sent.push(p); } }; return t; },
  };
  return msg;
}

/** An AutomationLite as getAutomations builds it. */
function lite(id, trigger, conditions, actions, { name = id, cooldownSeconds = 0 } = {}) {
  return {
    id, name, trigger, cooldownSeconds,
    conditions: svc.normalizeList(conditions, 'condition').items,
    actions: svc.normalizeList(actions, 'action').items,
  };
}

const baseOf = (guild, extra) => ({
  guild, guildId: guild.id, client: { user: { id: BOT } }, depth: 0, getSettings: async () => null, ...extra,
});

// ── 1-4: names, ids, words, emoji ────────────────────────────────────────────

test('normalizeAutomationName', () => {
  assert.equal(svc.normalizeAutomationName('My Rule!'), 'my-rule');
  assert.equal(svc.normalizeAutomationName('  Welcome  Bot '), 'welcome-bot');
  assert.equal(svc.normalizeAutomationName('🎉'), null);
  assert.equal(svc.normalizeAutomationName('--x--'), 'x');
  assert.equal(svc.normalizeAutomationName('a'.repeat(40)).length, 32);
  assert.equal(svc.normalizeAutomationName(42), null);
  assert.equal(svc.normalizeTypeInput(' gained-role '), 'GAINED_ROLE');
});

test('parseIdList', () => {
  assert.deepEqual(svc.parseIdList('<#111111111111111111> 222222222222222222', 'channel'), ['111111111111111111', '222222222222222222']);
  assert.deepEqual(svc.parseIdList('<@&333333333333333333>', 'role'), ['333333333333333333']);
  assert.deepEqual(svc.parseIdList('<@&333333333333333333>', 'user'), []);
  assert.deepEqual(svc.parseIdList('<@!444444444444444444> <@444444444444444444>', 'user'), ['444444444444444444']);
  assert.deepEqual(svc.parseIdList('junk 12', 'user'), []);
  assert.deepEqual(svc.parseIdList(null, 'user'), []);
});

test('parseWords', () => {
  assert.deepEqual(svc.parseWords('Hi, hello ,, HI'), ['Hi', 'hello']);
});

test('emoji parsing and keys', () => {
  assert.equal(svc.parseEmojiInput('⭐').value, '⭐');
  assert.equal(svc.parseEmojiInput('<:pog:123456789012345678>').ok, true);
  const resolver = (x) => (x === 'pog' ? { id: '123456789012345678', name: 'pog', animated: false } : null);
  assert.equal(svc.parseEmojiInput(':pog:', resolver).value, '<:pog:123456789012345678>');
  assert.equal(svc.parseEmojiInput(':nope:', resolver).ok, false);
  assert.equal(svc.parseEmojiInput('abc').error, 'That is not an emoji.');
  assert.equal(svc.parseEmojiInput('⭐⭐').ok, false);
  assert.equal(svc.parseEmojiInput('').ok, false);
  assert.equal(svc.emojiKeyOf('<a:xx:123456789012345678>'), '123456789012345678');
  assert.equal(svc.emojiKeyOf('❤️'), svc.emojiKeyOf('❤'));
  assert.equal(svc.isStoredEmoji('1️⃣'), true);
  assert.equal(svc.isStoredEmoji('1'), false);
});

test('attachmentKind', () => {
  assert.equal(svc.attachmentKind({ name: 'a.PNG', contentType: null }), 'IMAGE');
  assert.equal(svc.attachmentKind({ name: 'voice-message.ogg', contentType: 'audio/ogg' }), 'AUDIO');
  assert.equal(svc.attachmentKind({ name: 'x.bin', contentType: 'video/mp4' }), 'VIDEO');
  assert.equal(svc.attachmentKind({ name: 'notes.txt', contentType: 'text/plain; charset=utf-8' }), 'TEXT');
  assert.equal(svc.attachmentKind({ name: 'a.zip', contentType: 'application/zip' }), null);
  assert.equal(svc.attachmentKind({ name: 'clip.mp4', contentType: 'image/png' }), 'IMAGE');   // the content type wins
});

// ── 5-7: normalisers ─────────────────────────────────────────────────────────

test('normalizeCondition: valid params round-trip, bad params are refused', () => {
  const r1 = ID(1);
  const valid = [
    { type: 'CHANNEL_IN', channelIds: [r1] }, { type: 'CHANNEL_NOT_IN', channelIds: [r1] },
    { type: 'CATEGORY_IN', categoryIds: [r1] }, { type: 'CATEGORY_NOT_IN', categoryIds: [r1] },
    { type: 'CONTAINS', words: ['hi'] }, { type: 'EXACT', texts: ['hello'] }, { type: 'NOT_CONTAINS', words: ['x'] },
    { type: 'MATCHES_REGEX', pattern: '^hi', flags: 'i' }, { type: 'ATTACHMENT_AUDIO' }, { type: 'ATTACHMENT_IMAGE' },
    { type: 'ATTACHMENT_VIDEO' }, { type: 'ATTACHMENT_TEXT' }, { type: 'REACTED_WITH', emojis: ['⭐'] },
    { type: 'HAS_REACTIONS', min: 3, emoji: '⭐' }, { type: 'HAS_REACTIONS', min: 3, emoji: null }, { type: 'IS_REPLY' }, { type: 'IS_NOT_REPLY' },
    { type: 'HAS_ANY_ROLE', roleIds: [r1] }, { type: 'HAS_ALL_ROLES', roleIds: [r1] }, { type: 'HAS_NOT_ANY_ROLE', roleIds: [r1] },
    { type: 'HAS_NOT_ALL_ROLES', roleIds: [r1] }, { type: 'GAINED_ROLE', roleIds: [r1] }, { type: 'LOST_ROLE', roleIds: [r1] },
    { type: 'USER_IS', userIds: [r1] }, { type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefgh', index: 1 }] },
  ];
  assert.equal(valid.length, Object.keys(CONDITION_TYPES).length + 1);   // every type, HAS_REACTIONS twice
  for (const raw of valid) {
    const r = svc.normalizeCondition(raw);
    assert.equal(r.ok, true, JSON.stringify(raw));
    assert.deepEqual(r.value, raw);
    assert.deepEqual(svc.normalizeCondition(r.value).value, r.value);
  }
  assert.equal(svc.normalizeCondition({ type: 'CHANNEL_IN', channelIds: [] }).ok, false);
  assert.equal(svc.normalizeCondition({ type: 'CHANNEL_IN', channelIds: ['abc'] }).ok, false);
  assert.equal(svc.normalizeCondition({ type: 'CHANNEL_IN', channelIds: Array.from({ length: 26 }, (_, i) => ID(i)) }).ok, false);
  assert.deepEqual(svc.normalizeCondition(JSON.parse(`{"type":"CHANNEL_IN","channelIds":["${r1}","${r1}"],"extra":1,"__proto__":{"polluted":true}}`)).value, { type: 'CHANNEL_IN', channelIds: [r1] });
  assert.equal(svc.normalizeCondition({ type: 'CHANNEL_IN', channelIds: [r1], __proto__: { polluted: true } }).ok, false);   // a custom prototype is refused
  assert.deepEqual(svc.normalizeCondition({ type: 'CONTAINS', words: ['Hi', 'hi', ' HI '] }).value.words, ['Hi']);
  assert.equal(svc.normalizeCondition({ type: 'MATCHES_REGEX', pattern: '(a+)+' }).ok, false);
  assert.equal(svc.normalizeCondition({ type: 'MATCHES_REGEX', pattern: '^hi' }).value.flags, 'i');
  assert.equal(svc.normalizeCondition({ type: 'MATCHES_REGEX', pattern: 'a', flags: 'g' }).ok, false);
  assert.equal(svc.normalizeCondition({ type: 'HAS_REACTIONS', min: 0 }).ok, false);
  assert.equal(svc.normalizeCondition({ type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefgh', index: 100 }] }).ok, false);
  assert.equal(svc.normalizeCondition({ type: 'NOPE' }).ok, false);
  for (const evil of ['constructor', '__proto__', 'toString', 'hasOwnProperty']) assert.equal(svc.normalizeCondition({ type: evil }).ok, false, evil);
  assert.equal(svc.normalizeCondition(null).ok, false);
  assert.equal(svc.normalizeCondition([]).ok, false);
  // The name hint survives only when asked for.
  const hint = { type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefgh', index: 2, automationName: 'panel' }] };
  assert.equal(svc.normalizeCondition(hint).value.buttons[0].automationName, undefined);
  assert.equal(svc.normalizeCondition(hint, { keepHints: true }).value.buttons[0].automationName, 'panel');
});

test('normalizeAction: valid params, defaults and refusals', () => {
  const link = svc.normalizeAction({ type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', url: 'https://x.y', index: 5, style: 'DANGER' } });
  assert.equal(link.ok, true);
  assert.deepEqual(link.value.button, { label: 'Go', url: 'https://x.y', style: 'PRIMARY', index: null });
  assert.equal(svc.normalizeAction({ type: 'SEND_MESSAGE', content: '' }).ok, false);
  assert.equal(svc.normalizeAction({ type: 'SEND_MESSAGE', content: 'x'.repeat(2001) }).ok, false);
  assert.equal(svc.normalizeAction({ type: 'SEND_MESSAGE', content: 'x'.repeat(2000) }).ok, true);
  assert.equal(svc.normalizeAction({ type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', url: null } }).ok, false);
  assert.equal(svc.normalizeAction({ type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', url: 'ftp://x' } }).ok, false);
  const own = svc.normalizeAction({ type: 'REPLY', content: 'hi', button: { label: 'Go', url: null, index: 3 } });
  assert.deepEqual(own.value.button, { label: 'Go', url: null, style: 'PRIMARY', index: 3 });
  assert.equal(svc.normalizeAction({ type: 'REPLY', content: 'hi', button: { label: 'Go', index: 3, style: 'PURPLE' } }).ok, false);
  assert.equal(svc.normalizeAction({ type: 'CREATE_THREAD', name: 'x', autoArchiveMinutes: 30 }).ok, false);
  assert.equal(svc.normalizeAction({ type: 'CREATE_THREAD' }).value.autoArchiveMinutes, 1440);
  assert.equal(svc.normalizeAction({ type: 'CREATE_THREAD' }).value.name, svc.DEFAULT_THREAD_NAME);
  assert.equal(svc.normalizeAction({ type: 'GIVE_COINS', amount: 0 }).ok, false);
  assert.equal(svc.normalizeAction({ type: 'GIVE_COINS', amount: 1_000_001 }).ok, false);
  assert.equal(svc.normalizeAction({ type: 'GIVE_COINS', amount: 1_000_000 }).ok, true);
  assert.equal(svc.normalizeAction({ type: 'GIVE_ROLES', roleIds: Array.from({ length: 11 }, (_, i) => ID(i)) }).ok, false);
  assert.equal(svc.normalizeAction({ type: 'ADD_TO_THREAD', userIds: [] }).ok, true);
  assert.equal(svc.normalizeAction({ type: 'REPOST' }).ok, false);
  assert.equal(svc.normalizeAction({ type: 'REACT', emoji: 'nope' }).ok, false);
  assert.equal(svc.normalizeAction({ type: 'SEND_EMBED', embedName: 'My Embed' }).value.embedName, 'my-embed');
  for (const evil of ['constructor', '__proto__', 'toString']) assert.equal(svc.normalizeAction({ type: evil }).ok, false, evil);
  for (const t of Object.keys(ACTION_TYPES)) assert.equal(svc.normalizeAction({ type: t, content: 'x', roleIds: [ID(1)], amount: 5, emoji: '⭐', channelId: ID(2), embedName: 'e', userIds: [] }).ok, true, t);
  // normalizeList reports 1-based positions and drops the invalid ones.
  const l = svc.normalizeList([{ type: 'PIN' }, { type: 'NOPE' }, { type: 'DELETE' }], 'action');
  assert.equal(l.items.length, 2);
  assert.deepEqual(l.invalid.map((x) => x.index), [2]);
  assert.deepEqual(svc.normalizeList('nope', 'action'), { items: [], invalid: [] });
});

// ── 8: shape validation ──────────────────────────────────────────────────────

test('validateAutomationShape', () => {
  const shape = (trigger, conditions, actions, cooldownSeconds = 0) => svc.validateAutomationShape({
    trigger,
    conditions: svc.normalizeList(conditions, 'condition').items,
    actions: svc.normalizeList(actions, 'action').items,
    cooldownSeconds,
  });
  const msg = { type: 'SEND_MESSAGE', content: 'hi' };
  assert.deepEqual(shape('MESSAGE_SENT', [], [msg]), []);
  assert.match(shape('MEMBER_JOINED', [], [{ type: 'REPLY', content: 'x' }]).join('\n'), /Action 1: "Reply" doesn't work with the trigger "Member joined"/);
  assert.match(shape('ROLE_GAINED', [], [msg]).join('\n'), /this trigger doesn't happen in a channel, so pick a channel/);
  assert.deepEqual(shape('ROLE_GAINED', [], [{ ...msg, channelId: ID(1) }]), []);
  assert.match(shape('MEMBER_JOINED', [], [{ type: 'SEND_IN_THREAD', content: 'x' }]).join('\n'), /there is no thread here/);
  assert.deepEqual(shape('MEMBER_JOINED', [], [{ type: 'CREATE_THREAD', channelId: ID(1) }, { type: 'SEND_IN_THREAD', content: 'x' }]), []);
  assert.match(shape('MEMBER_JOINED', [], [{ type: 'CREATE_THREAD' }]).join('\n'), /no message to start a thread from/);
  assert.match(shape('BUTTON_CLICKED', [{ type: 'CONTAINS', words: ['a'] }], [msg]).join('\n'), /Condition 1: "Message contains words" doesn't work/);
  assert.match(shape('MESSAGE_SENT', [], [
    { type: 'SEND_MESSAGE', content: 'a', button: { label: 'A', url: null, index: 1 } },
    { type: 'SEND_MESSAGE', content: 'b', button: { label: 'B', url: null, index: 1 } },
  ]).join('\n'), /Action 2: button number 1 is used twice/);
  assert.match(shape('MESSAGE_SENT', [{ type: 'IS_REPLY' }, { type: 'IS_NOT_REPLY' }], [msg]).join('\n'), /can never both be true/);
  assert.match(shape('MESSAGE_SENT', [{ type: 'IS_REPLY' }], [msg]).concat(shape('MESSAGE_SENT', [], [])).join(''), /^$/);
  assert.match(shape('MESSAGE_SENT', [{ type: 'ATTACHMENT_IMAGE' }, { type: 'ATTACHMENT_IMAGE' }], [msg]).join('\n'), /Condition 2 is already there/);
  assert.match(shape('MESSAGE_SENT', [], Array.from({ length: 11 }, () => msg)).join('\n'), /at most 10 actions/);
  assert.match(shape('MESSAGE_SENT', [], [msg], 604_801).join('\n'), /cooldown/);
  assert.match(shape('MESSAGE_SENT', [{ type: 'HAS_REACTIONS', min: 1 }], [msg]).join('\n'), /Condition 1: "Message has at least N reactions" doesn't work/);
  assert.match(shape('NOPE', [], []).join(''), /Unknown trigger/);
  // The old trigger-specific tables.
  for (const t of Object.keys(TRIGGERS)) assert.equal(svc.T_ALL.has(t), true);
  assert.throws(() => svc.T_ALL.add('X'), TypeError);
});

// ── 9-10: evaluator ──────────────────────────────────────────────────────────

test('evaluateCondition and evaluateConditions', () => {
  const ev = (cond, facts) => svc.evaluateCondition(cond, facts);
  assert.equal(ev({ type: 'CHANNEL_IN', channelIds: ['c1'] }, { channelId: 'c1' }), true);
  assert.equal(ev({ type: 'CHANNEL_IN', channelIds: ['c1'] }, { channelId: 't9', parentId: 'c1' }), true);
  assert.equal(ev({ type: 'CHANNEL_NOT_IN', channelIds: ['c1'] }, { channelId: null }), false);
  assert.equal(ev({ type: 'CHANNEL_NOT_IN', channelIds: ['c1'] }, { channelId: 't9', parentId: 'c1' }), false);
  assert.equal(ev({ type: 'CATEGORY_NOT_IN', categoryIds: ['k'] }, { channelId: 'c', categoryId: null }), true);
  assert.equal(ev({ type: 'CATEGORY_IN', categoryIds: ['k'] }, { channelId: 'c', categoryId: null }), false);
  assert.equal(ev({ type: 'CONTAINS', words: ['cunt'] }, { content: 'Scunthorpe' }), false);
  assert.equal(ev({ type: 'CONTAINS', words: ['jerk'] }, { content: 'you jerk!' }), true);
  assert.equal(ev({ type: 'EXACT', texts: ['Hello'] }, { content: '  hello ' }), true);
  assert.equal(ev({ type: 'NOT_CONTAINS', words: ['x'] }, { content: null }), false);
  assert.equal(ev({ type: 'NOT_CONTAINS', words: ['x'] }, { content: 'abc' }), true);
  assert.equal(ev({ type: 'MATCHES_REGEX', pattern: '^\\d+$', flags: 'i' }, { content: '123' }), true);
  assert.equal(ev({ type: 'MATCHES_REGEX', pattern: '^\\d+$', flags: 'i' }, { content: 'a123' }), false);
  assert.equal(ev({ type: 'MATCHES_REGEX', pattern: '(a+)+', flags: 'i' }, { content: 'aaaa' }), false);   // no longer compiles
  assert.equal(ev({ type: 'ATTACHMENT_IMAGE' }, { attachments: [{ name: 'a.png' }] }), true);
  assert.equal(ev({ type: 'ATTACHMENT_IMAGE' }, { attachments: [] }), false);
  assert.equal(ev({ type: 'ATTACHMENT_IMAGE' }, { attachments: null }), false);
  assert.equal(ev({ type: 'REACTED_WITH', emojis: ['❤️'] }, { reactionKey: '❤' }), true);
  assert.equal(ev({ type: 'HAS_REACTIONS', min: 3, emoji: '⭐' }, { reactionCounts: { '⭐': 2, '🌟': 5 } }), false);
  assert.equal(ev({ type: 'HAS_REACTIONS', min: 3, emoji: null }, { reactionCounts: { '⭐': 2, '🌟': 5 } }), true);
  assert.equal(ev({ type: 'HAS_REACTIONS', min: 1, emoji: null }, { reactionCounts: null }), false);
  assert.equal(ev({ type: 'IS_REPLY' }, { isReply: true }), true);
  assert.equal(ev({ type: 'IS_REPLY' }, { isReply: null }), false);
  assert.equal(ev({ type: 'IS_NOT_REPLY' }, { isReply: false }), true);
  assert.equal(ev({ type: 'IS_NOT_REPLY' }, { isReply: null }), false);
  // Every role type over memberRoleIds [a, b] x three lists, then unavailable roles.
  const table = {
    HAS_ANY_ROLE: [true, true, false], HAS_ALL_ROLES: [true, false, false],
    HAS_NOT_ANY_ROLE: [false, false, true], HAS_NOT_ALL_ROLES: [false, true, true],
  };
  for (const [type, expected] of Object.entries(table)) {
    [['a'], ['a', 'c'], ['c']].forEach((roleIds, k) => {
      assert.equal(ev({ type, roleIds }, { memberRoleIds: ['a', 'b'] }), expected[k], `${type} ${roleIds}`);
      assert.equal(ev({ type, roleIds }, { memberRoleIds: null }), false, `${type} with no roles`);
    });
  }
  assert.equal(ev({ type: 'GAINED_ROLE', roleIds: ['a'] }, { changedRoleIds: ['a', 'z'] }), true);
  assert.equal(ev({ type: 'LOST_ROLE', roleIds: ['a'] }, { changedRoleIds: null }), false);
  assert.equal(ev({ type: 'USER_IS', userIds: ['u'] }, { userId: 'u' }), true);
  assert.equal(ev({ type: 'BUTTON_IS', buttons: [{ automationId: 'a', index: 1 }] }, { button: { automationId: 'a', index: 1 } }), true);
  assert.equal(ev({ type: 'BUTTON_IS', buttons: [{ automationId: 'a', index: 1 }] }, { button: { automationId: 'a', index: 2 } }), false);
  assert.equal(ev({ type: 'NOPE' }, {}), false);
  assert.equal(ev({ type: 'constructor' }, {}), false);
  assert.equal(svc.evaluateConditions([], {}), true);
  assert.equal(svc.evaluateConditions([{ type: 'USER_IS', userIds: ['u'] }, { type: 'NOPE' }], { userId: 'u' }), false);
});

test('splitConditions', () => {
  const { cheap, needsMessage } = svc.splitConditions([
    { type: 'CONTAINS', words: ['a'] }, { type: 'HAS_REACTIONS', min: 1 }, { type: 'ATTACHMENT_IMAGE' },
    { type: 'REACTED_WITH', emojis: ['⭐'] }, { type: 'CHANNEL_IN', channelIds: ['c'] }, { type: 'HAS_ANY_ROLE', roleIds: ['r'] },
  ]);
  assert.deepEqual(needsMessage.map((c) => c.type), ['CONTAINS', 'HAS_REACTIONS', 'ATTACHMENT_IMAGE']);
  assert.deepEqual(cheap.map((c) => c.type), ['REACTED_WITH', 'CHANNEL_IN', 'HAS_ANY_ROLE']);
});

// ── 11-13: diffs, budgets, trackers ──────────────────────────────────────────

test('roleDiff and voiceTriggers', () => {
  assert.deepEqual(svc.roleDiff(['g', 'a', 'b'], ['g', 'b', 'c'], 'g'), { gained: ['c'], lost: ['a'] });
  assert.deepEqual(svc.roleDiff(['a'], ['a', 'g'], 'g'), { gained: [], lost: [] });   // @everyone never counts
  assert.deepEqual(svc.voiceTriggers('join'), ['VOICE_JOINED']);
  assert.deepEqual(svc.voiceTriggers('leave'), ['VOICE_LEFT']);
  assert.deepEqual(svc.voiceTriggers('move'), ['VOICE_LEFT', 'VOICE_JOINED']);
  assert.deepEqual(svc.voiceTriggers('state'), []);
});

test('createRunBudget, createCauseTracker and createLocalExpiringSet', () => {
  const b = svc.createRunBudget({ max: 3, windowMs: 1000 });
  assert.deepEqual([b.take('g', 0), b.take('g', 0), b.take('g', 0), b.take('g', 0)], [true, true, true, false]);
  assert.equal(b.take('g', 1001), true);
  assert.equal(b.take('other', 0), true);

  const t = svc.createCauseTracker({ ttlMs: 100 });
  t.mark('g', 'u', 1, 0);
  t.mark('g', 'u', 2, 10);
  assert.equal(t.peek('g', 'u', 50), 2);
  assert.equal(t.peek('g', 'u', 200), 0);
  t.mark('g', 'u', 2, 300);
  t.mark('g', 'u', 1, 360);
  assert.equal(t.peek('g', 'u', 400), 2);
  assert.equal(t.peek('g', 'nobody', 0), 0);

  const s = svc.createLocalExpiringSet();
  assert.equal(s.add('k', 100, 0), true);
  assert.equal(s.add('k', 100, 50), false);
  assert.equal(s.has('k', 50), true);
  assert.equal(s.add('k', 100, 150), true);
  const small = svc.createLocalExpiringSet({ cap: 2 });
  small.add('a', 1000, 0); small.add('b', 1000, 1); small.add('c', 1000, 2);
  assert.equal(small.has('a', 3), false);   // the oldest was evicted
  assert.equal(small.has('c', 3), true);
});

// ── 14: buttons, add / append / remove ───────────────────────────────────────

test('nextButtonIndex, nextButtonSeq, addCondition, appendAction, removeAt', () => {
  const btn = (index) => ({ type: 'SEND_MESSAGE', content: 'x', channelId: null, button: { label: 'B', url: null, style: 'PRIMARY', index } });
  assert.equal(svc.nextButtonIndex([]), 1);
  assert.equal(svc.nextButtonIndex([btn(1), btn(4), { type: 'REPLY', content: 'x', button: { label: 'L', url: 'https://x.y', style: 'PRIMARY', index: null } }]), 5);
  assert.equal(svc.nextButtonIndex([btn(99)]), null);

  // The live assignment is the monotonic buttonSeq: a removed button's number is never reused.
  const row0 = { actions: [], buttonSeq: 0 };
  const a1 = svc.appendAction(row0, { type: 'SEND_MESSAGE', content: 'x', button: { label: 'B', url: null, style: 'PRIMARY', index: null } });
  assert.equal(a1.buttonIndex, 1);
  assert.equal(a1.actions[0].button.index, 1);
  const a2 = svc.appendAction({ actions: a1.actions, buttonSeq: a1.buttonSeq }, { type: 'SEND_MESSAGE', content: 'y', button: { label: 'C', url: null, style: 'PRIMARY', index: null } });
  assert.equal(a2.buttonIndex, 2);
  const removed = svc.removeAt(a2.actions, 2).list;
  const a3 = svc.appendAction({ actions: removed, buttonSeq: a2.buttonSeq }, { type: 'SEND_MESSAGE', content: 'z', button: { label: 'D', url: null, style: 'PRIMARY', index: null } });
  assert.equal(a3.buttonIndex, 3);
  const full = svc.appendAction({ actions: [], buttonSeq: 98 }, { type: 'SEND_MESSAGE', content: 'x', button: { label: 'B', url: null, style: 'PRIMARY', index: null } });
  assert.equal(full.buttonIndex, 99);
  assert.throws(() => svc.appendAction({ actions: [], buttonSeq: 99 }, { type: 'SEND_MESSAGE', content: 'x', button: { label: 'B', url: null, style: 'PRIMARY', index: null } }),
    (e) => e instanceof svc.AutomationError && e.message === 'This automation has used all 99 button numbers. Create a new automation for more buttons.');
  const link = svc.appendAction({ actions: [], buttonSeq: 5 }, { type: 'SEND_MESSAGE', content: 'x', button: { label: 'L', url: 'https://x.y' } });
  assert.equal(link.buttonIndex, null);
  assert.throws(() => svc.appendAction({ actions: Array.from({ length: 10 }, () => ({ type: 'PIN' })), buttonSeq: 0 }, { type: 'PIN' }), svc.AutomationError);
  assert.throws(() => svc.removeAt([{ type: 'PIN' }], 2), svc.AutomationError);

  // Conditions merge into the first list condition of the same type.
  const row = { trigger: 'MESSAGE_SENT', conditions: [{ type: 'HAS_ANY_ROLE', roleIds: [ID(1)] }] };
  const m = svc.addCondition(row, 'HAS_ANY_ROLE', { roleIds: [ID(1), ID(2)] });
  assert.deepEqual([m.merged, m.addedNew, m.index], [true, true, 1]);
  assert.deepEqual(m.conditions[0].roleIds, [ID(1), ID(2)]);
  const again = svc.addCondition(row, 'HAS_ANY_ROLE', { roleIds: [ID(1)] });
  assert.deepEqual([again.merged, again.addedNew], [true, false]);
  const added = svc.addCondition(row, 'HAS_ALL_ROLES', { roleIds: [ID(3)] });
  assert.deepEqual([added.merged, added.index], [false, 2]);
  const ten = Array.from({ length: 10 }, (_, i) => ({ type: 'CHANNEL_IN', channelIds: [ID(i)] }));
  assert.throws(() => svc.addCondition({ conditions: ten }, 'HAS_ANY_ROLE', { roleIds: [ID(1)] }), /at most 10 conditions/);
  const big = { conditions: [{ type: 'HAS_ANY_ROLE', roleIds: Array.from({ length: 25 }, (_, i) => ID(i)) }] };
  assert.throws(() => svc.addCondition(big, 'HAS_ANY_ROLE', { roleIds: [ID(99)] }), /That list can hold at most 25 entries/);
});

// ── 15: presets ──────────────────────────────────────────────────────────────

test('presets are normalised and shape-valid', () => {
  const ch = ID(10);
  const outs = [
    svc.buildPresetAutomation('welcomer', { channelId: ch }),
    svc.buildPresetAutomation('welcomer', { channelId: ch, roleId: ID(11), message: 'Hi {user}' }),
    svc.buildPresetAutomation('starboard', { channelId: ch }),
    svc.buildPresetAutomation('starboard', { channelId: ch, emoji: '<:pog:123456789012345678>', threshold: 5 }),
    svc.buildPresetAutomation('thread-joiner', { userId: ID(12) }),
    svc.buildPresetAutomation('thread-joiner', { userId: ID(12), channelId: ch }),
  ];
  for (const o of outs) {
    assert.deepEqual(svc.normalizeList(o.conditions, 'condition').invalid, []);
    assert.deepEqual(svc.normalizeList(o.actions, 'action').invalid, []);
    assert.deepEqual(svc.validateAutomationShape({ ...o, cooldownSeconds: 0 }), []);
  }
  assert.equal(outs[0].actions.length, 1);
  assert.equal(outs[0].actions[0].content, svc.WELCOMER_DEFAULT_TEXT);
  assert.equal(outs[1].actions.length, 2);
  assert.equal(outs[2].conditions[0].emojis[0], '⭐');
  assert.equal(outs[2].conditions[1].min, 3);
  assert.deepEqual(outs[2].conditions[2], { type: 'CHANNEL_NOT_IN', channelIds: [ch] });
  assert.equal(outs[3].conditions[1].min, 5);
  assert.deepEqual(outs[4].conditions, []);
  assert.equal(outs[5].conditions.length, 1);
  assert.throws(() => svc.buildPresetAutomation('nope', {}), svc.AutomationError);
  assert.throws(() => svc.buildPresetAutomation('welcomer', {}), svc.AutomationError);   // no channel: the shape rule refuses
});

// ── 16: JSON ─────────────────────────────────────────────────────────────────

const everyActionRows = () => {
  const r = ID(20); const u = ID(21); const c = ID(22);
  const button = { label: 'Go', url: null, style: 'SUCCESS', index: 1 };
  const first = {
    id: 'clxfirstrow01', name: 'first', trigger: 'REACTION_ADDED', enabled: true, cooldownSeconds: 30,
    conditions: [
      { type: 'REACTED_WITH', emojis: ['⭐', '<:pog:123456789012345678>'] }, { type: 'HAS_REACTIONS', min: 2, emoji: '⭐' },
      { type: 'CHANNEL_NOT_IN', channelIds: [c] }, { type: 'MATCHES_REGEX', pattern: '^\\w+', flags: 'im' },
      { type: 'BUTTON_IS', buttons: [{ automationId: 'clxsecondrow', index: 1 }] },
    ].slice(0, 4),
    actions: [
      { type: 'CREATE_THREAD', name: 'T {user.name}', channelId: null, autoArchiveMinutes: 60 },
      { type: 'SEND_MESSAGE', content: 'Hello {user}', channelId: c, button },
      { type: 'REPLY', content: 'Thanks', button: null },
      { type: 'REPOST', channelId: c },
      { type: 'PIN' },
      { type: 'REACT', emoji: '👍' },
      { type: 'REMOVE_REACTION', emoji: null },
      { type: 'GIVE_ROLES', roleIds: [r] },
      { type: 'TAKE_ROLES', roleIds: [r] },
      { type: 'ADD_TO_THREAD', userIds: [u] },
    ],
  };
  const second = {
    id: 'clxsecondrow', name: 'second', trigger: 'BUTTON_CLICKED', enabled: false, cooldownSeconds: 0,
    conditions: [{ type: 'BUTTON_IS', buttons: [{ automationId: 'clxfirstrow01', index: 1 }] }],
    actions: [
      { type: 'SEND_IN_THREAD', content: 'x', button: null }, { type: 'GIVE_COINS', amount: 5 }, { type: 'TAKE_COINS', amount: 5 },
      { type: 'GIVE_XP', amount: 10 }, { type: 'TAKE_XP', amount: 10 }, { type: 'SEND_EMBED', embedName: 'rules', channelId: c },
      { type: 'DELETE' }, { type: 'REPLY', content: 'ok', button: null },
    ],
  };
  return [first, second];
};

test('JSON export / import round trip', () => {
  const [first, second] = everyActionRows();
  // Row 'second' (BUTTON_CLICKED) cannot DELETE or hold REPOST: build it from the action types its trigger allows.
  second.actions = second.actions.filter((a) => a.type !== 'DELETE');
  const seen = new Set();
  for (const row of [first, { ...second }]) {
    for (const a of row.actions) seen.add(a.type);
    const json = svc.toExportJson(row, new Map([['clxsecondrow', { name: 'second' }]]));
    const parsed = svc.parseAutomationJson(json);
    assert.deepEqual(parsed.errors, [], row.name);
    assert.equal(parsed.ok, true);
    assert.equal(parsed.data.trigger, row.trigger);
    assert.equal(parsed.data.enabled, row.enabled);
    assert.equal(parsed.data.cooldownSeconds, row.cooldownSeconds);
    assert.equal(parsed.data.exportedId, row.id);
    assert.deepEqual(parsed.data.actions, row.actions);
    const strip = (conds) => svc.normalizeList(conds, 'condition').items;
    assert.deepEqual(strip(parsed.data.conditions), strip(row.conditions));
  }
  // Every action type except DELETE (which only fits live-message triggers) went through the round trip.
  const covered = new Set(seen);
  covered.add('DELETE');
  assert.deepEqual([...covered].sort(), Object.keys(ACTION_TYPES).sort());
  // The BUTTON_IS name hint is exported and read back.
  const json = JSON.parse(svc.toExportJson(second, new Map([['clxfirstrow01', { name: 'first' }]])));
  assert.equal(json.conditions[0].buttons[0].automationName, 'first');
  assert.equal(svc.parseAutomationJson(JSON.stringify(json)).data.conditions[0].buttons[0].automationName, 'first');
  assert.equal(Object.keys(json)[0], 'format');
});

test('parseAutomationJson refuses bad input strictly', () => {
  const ok = { format: 'automation', version: 1, trigger: 'MESSAGE_SENT', conditions: [], actions: [{ type: 'PIN' }] };
  assert.equal(svc.parseAutomationJson(JSON.stringify(ok)).ok, true);
  assert.match(svc.parseAutomationJson(' '.repeat(33_000)).errors[0], /32 KB/);
  assert.match(svc.parseAutomationJson(JSON.stringify({ ...ok, version: 2 })).errors[0], /Unsupported version/);
  assert.match(svc.parseAutomationJson(JSON.stringify({ ...ok, format: 'embed' })).errors[0], /not an automation export/);
  assert.equal(svc.parseAutomationJson('nope').errors[0], 'That is not valid JSON.');
  assert.match(svc.parseAutomationJson('[1,2]').errors[0], /must be an object/);
  assert.match(svc.parseAutomationJson(JSON.stringify({ ...ok, trigger: 'X' })).errors[0], /Unknown trigger "X"/);
  for (const evil of ['constructor', '__proto__', 'toString']) {
    assert.match(svc.parseAutomationJson(JSON.stringify({ ...ok, trigger: evil })).errors[0], /Unknown trigger/);
    assert.equal(svc.parseAutomationJson(JSON.stringify({ ...ok, actions: [{ type: evil }] })).ok, false, evil);
  }
  const r = svc.parseAutomationJson(JSON.stringify({ ...ok, actions: [{ type: 'PIN' }, { type: 'NOPE' }] }));
  assert.equal(r.ok, false);
  assert.match(r.errors[0], /^Action 2: /);
  assert.match(svc.parseAutomationJson(JSON.stringify({ ...ok, conditions: [{ type: 'CHANNEL_IN', channelIds: [] }] })).errors[0], /^Condition 1: /);
  assert.match(svc.parseAutomationJson(JSON.stringify({ ...ok, actions: Array.from({ length: 11 }, () => ({ type: 'PIN' })) })).errors[0], /At most 10 actions/);
  assert.match(svc.parseAutomationJson(JSON.stringify({ ...ok, conditions: Array.from({ length: 11 }, () => ({ type: 'IS_REPLY' })) })).errors[0], /At most 10 conditions/);
  assert.equal(svc.parseAutomationJson(JSON.stringify({ ...ok, enabled: 'yes' })).ok, false);
  assert.equal(svc.parseAutomationJson(JSON.stringify({ ...ok, cooldownSeconds: -1 })).ok, false);
  assert.equal(svc.parseAutomationJson(JSON.stringify({ ...ok, cooldownSeconds: 1.5 })).ok, false);
  assert.equal(svc.parseAutomationJson(JSON.stringify({ ...ok, conditions: 'x' })).ok, false);
  // A prototype-polluting key is data, not a prototype: nothing leaks into the stored objects.
  const sneaky = svc.parseAutomationJson('{"format":"automation","version":1,"trigger":"MESSAGE_SENT","actions":[{"type":"PIN","__proto__":{"admin":true},"constructor":"x"}]}');
  assert.equal(sneaky.ok, true);
  assert.deepEqual(sneaky.data.actions, [{ type: 'PIN' }]);
  assert.equal({}.admin, undefined);
  // exportedId is kept only when it looks like an id.
  assert.equal(svc.parseAutomationJson(JSON.stringify({ ...ok, exportedId: '../../x' })).data.exportedId, null);
  assert.equal(svc.parseAutomationJson(JSON.stringify({ ...ok, exportedId: 'clxabcdefgh' })).data.exportedId, 'clxabcdefgh');
  // Contradictions and shape errors are import errors too.
  assert.equal(svc.parseAutomationJson(JSON.stringify({ ...ok, trigger: 'MEMBER_JOINED', actions: [{ type: 'REPLY', content: 'x' }] })).ok, false);
});

// ── 17: variables, descriptions, error texts ─────────────────────────────────

test('buildVars, describe* and discordErrorText', () => {
  const guild = fakeGuild();
  const reactor = fakeMember(guild, ID(31));
  const author = { id: ID(32), username: 'authorname', bot: false };
  const ch = fakeChannel(guild, ID(33));
  const msg = fakeMessage(guild, ch, author, { content: 'starred text' });
  const run = { automation: { name: 'my-auto' }, thread: null };
  const base = { guild, user: reactor.user, member: reactor, channel: ch, message: msg, reaction: { key: '⭐', display: '⭐' }, roleIds: [ID(34)] };
  const vars = svc.buildVars(base, run);
  assert.equal(vars['message.author'], `<@${ID(32)}>`);
  assert.equal(vars['message.author.name'], 'authorname');
  assert.equal(vars['message.content'], 'starred text');
  assert.equal(vars.role, `<@&${ID(34)}>`);
  assert.equal(typeof vars.thread, 'function');
  assert.equal(vars.thread(), '');
  run.thread = { id: ID(35), name: 'the-thread' };   // set after building: the function reads it at render time
  assert.equal(vars.thread(), `<#${ID(35)}>`);
  assert.equal(vars['thread.name'](), 'the-thread');
  assert.equal(renderPlaceholders('{reaction} {automation}', { vars }), '⭐ my-auto');
  assert.equal(renderPlaceholders('{user} by {message.author}', { member: reactor, guild, vars }), `<@${ID(31)}> by <@${ID(32)}>`);

  const sampleCond = { CHANNEL_IN: { channelIds: [ID(1)] }, CATEGORY_IN: { categoryIds: [ID(1)] }, CONTAINS: { words: ['a'] }, EXACT: { texts: ['a'] },
    MATCHES_REGEX: { pattern: 'a`b', flags: 'i' }, REACTED_WITH: { emojis: ['⭐'] }, HAS_REACTIONS: { min: 1, emoji: null }, USER_IS: { userIds: [ID(1)] },
    BUTTON_IS: { buttons: [{ automationId: 'clxabcdefgh', index: 1 }] }, HAS_ANY_ROLE: { roleIds: [ID(1)] } };
  for (const type of Object.keys(CONDITION_TYPES)) {
    for (const g of [null, guild]) {
      const text = svc.describeCondition({ type, ...(sampleCond[type] ?? {}) }, { guild: g });
      assert.ok(text.length > 0 && text.length <= 200, type);
    }
  }
  const sampleAction = { CREATE_THREAD: { name: 'n', channelId: ID(1) }, SEND_MESSAGE: { content: 'x'.repeat(300), button: { label: 'L', url: null, index: 2 } }, REPLY: { content: 'x', button: null },
    REPOST: { channelId: ID(1) }, REACT: { emoji: '⭐' }, REMOVE_REACTION: { emoji: null }, GIVE_ROLES: { roleIds: [ID(1)] }, TAKE_ROLES: { roleIds: [ID(1)] },
    ADD_TO_THREAD: { userIds: [] }, SEND_IN_THREAD: { content: 'x', button: null }, GIVE_COINS: { amount: 5 }, TAKE_COINS: { amount: 5 }, GIVE_XP: { amount: 5 },
    TAKE_XP: { amount: 5 }, SEND_EMBED: { embedName: 'e', channelId: null } };
  for (const type of Object.keys(ACTION_TYPES)) {
    for (const g of [null, guild]) {
      const text = svc.describeAction({ type, ...(sampleAction[type] ?? {}) }, { guild: g });
      assert.ok(text.length > 0 && text.length <= 200, type);
    }
  }
  assert.match(svc.describeCondition({ type: 'CHANNEL_IN', channelIds: [ID(999)] }, { guild }), /deleted/);
  assert.equal(svc.describeCondition(null), 'Unknown condition');

  assert.equal(svc.discordErrorText({ code: 10008 }), null);
  assert.equal(svc.discordErrorText({ code: 10007 }), null);
  assert.equal(svc.discordErrorText({ code: 50013 }), "I'm missing a permission for this");
  assert.equal(svc.discordErrorText({ code: RESTJSONErrorCodes.ThreadLocked }), 'The thread is archived or locked');
  assert.equal(svc.discordErrorText(new TimeoutError('x')), 'The action took too long');
  assert.match(svc.discordErrorText({ code: 50035, message: 'bad' }), /^Discord rejected the message: bad/);
  assert.equal(svc.discordErrorText(new Error('boom')), 'boom');
});

// ── 18: plugin and filters ───────────────────────────────────────────────────

test('plugin declaration and hook filters', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('automations')), []);
  const p = plugins.find((x) => x.id === 'automations');
  assert.ok(p);
  assert.equal(p.module, 'automations');
  assert.equal(p.hooks.length, 8);
  for (const h of plugin.hooks) {
    assert.equal(h.id, 'automations');
    assert.ok(HOOK_ORDER[h.event].includes('automations'), h.event);
    assert.notEqual(h.always, true);
    assert.equal(typeof h.filter, 'function');
    assert.equal(typeof h.run, 'function');
  }
  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix]), [['button', 'automation']]);
  assert.deepEqual(plugin.jobs, []);
  assert.deepEqual(plugin.provides, {});

  svc.__setAutomationMemo('g', {});
  assert.equal(svc.filterMessageCreate({ guildId: 'g' }), false);
  svc.__setAutomationMemo('g', { MESSAGE_SENT: [lite('a1', 'MESSAGE_SENT', [], [{ type: 'PIN' }])] });
  assert.equal(svc.filterMessageCreate({ guildId: 'g' }), true);
  assert.equal(svc.filterMessageUpdate({ guildId: 'g' }), false);
  assert.equal(svc.filterMessageCreate({ guildId: 'never-seen' }), true);   // unknown guild: the run loads it
  assert.equal(svc.peekHasTrigger('g', 'constructor'), false);

  svc.__setAutomationMemo('g', { THREAD_CREATED: [lite('a2', 'THREAD_CREATED', [], [{ type: 'PIN' }])], MESSAGE_DELETED: [lite('a3', 'MESSAGE_DELETED', [], [{ type: 'PIN' }])] });
  assert.equal(svc.filterThreadCreate({ guildId: 'g', thread: { ownerId: ID(1) }, client: { user: { id: BOT } } }), true);
  assert.equal(svc.filterThreadCreate({ guildId: 'g', thread: { ownerId: BOT }, client: { user: { id: BOT } } }), false);
  assert.equal(svc.filterThreadCreate({ guildId: 'g', thread: { ownerId: null }, client: { user: { id: BOT } } }), false);
  const humanMsg = { author: { bot: false }, webhookId: null, system: false };
  assert.equal(svc.filterMessageDelete({ guildId: 'g', partial: false, message: humanMsg }), true);
  assert.equal(svc.filterMessageDelete({ guildId: 'g', partial: true, message: humanMsg }), false);
  assert.equal(svc.filterMessageDelete({ guildId: 'g', partial: false, message: { ...humanMsg, author: { bot: true } } }), false);
  assert.equal(svc.filterMessageDelete({ guildId: 'g', partial: false, message: { ...humanMsg, webhookId: '1' } }), false);
  assert.equal(svc.filterVoiceState({ guildId: 'g', change: 'state' }), false);
  assert.equal(svc.filterMemberUpdate({ guildId: 'g', oldKnown: false, newMember: { user: { bot: false } } }), false);
  assert.equal(svc.filterMemberAdd({ guildId: 'g', member: { user: { bot: true } } }), false);
  svc.__setAutomationMemo('g', {});
});

// ── 19: command payload ──────────────────────────────────────────────────────

test('/automation payload', async () => {
  const json = buildCommandPayload(cmd);
  const top = json.options;
  assert.equal(top.length, 12);
  assert.equal(top.filter((o) => o.type === 2).length, 2);
  const find = (...path) => path.reduce((node, name) => node.options.find((o) => o.name === name), { options: top });
  assert.equal(find('create').options.length, 10); // + new_channel, new_role (newTargets)
  assert.equal(find('action', 'add').options.length, 13);
  assert.equal(find('condition', 'add').options.length, 8);
  assert.equal(find('create-blank', 'trigger').choices.length, 11);
  assert.deepEqual(find('create-blank', 'trigger').choices.map((c) => c.value).sort(), Object.keys(TRIGGERS).sort());
  const enumValues = [...readFileSync(resolve(ROOT, 'prisma/schema.prisma'), 'utf8').match(/enum AutomationTrigger \{([^}]*)\}/)[1].matchAll(/^\s*([A-Z_]+)\s*$/gm)].map((m) => m[1]);
  assert.deepEqual(find('create-blank', 'trigger').choices.map((c) => c.value).sort(), enumValues.sort());
  const walk = (opts) => {
    let seenOptional = false;
    for (const o of opts) {
      assert.ok(o.description.length >= 1 && o.description.length <= 100, o.name);
      if (o.type === 1 || o.type === 2) { walk(o.options ?? []); continue; }
      if (o.required) assert.equal(seenOptional, false, `required after optional: ${o.name}`);
      else seenOptional = true;
    }
  };
  walk(top);
  const charCount = (node) => (node.name?.length ?? 0) + (node.description?.length ?? 0)
    + (node.choices ?? []).reduce((n, c) => n + c.name.length + String(c.value).length, 0)
    + (node.options ?? []).reduce((n, o) => n + charCount(o), 0);
  assert.ok(charCount(json) <= 2300, `payload is ${charCount(json)} characters`);
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.deepEqual(json.contexts, [0]);
  assert.equal(cmd.module, 'automations');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(typeof cmd.autocomplete, 'function');
  assert.equal(find('condition', 'remove', 'index').max_value, LIMITS.AUTOMATION_CONDITIONS);
  assert.equal(find('action', 'remove', 'index').max_value, LIMITS.AUTOMATION_ACTIONS);
});

// ── Runtime: dispatch and the action runners against fakes ───────────────────

function setup(guildId, { memberRoles = [] } = {}) {
  const guild = fakeGuild(guildId);
  const general = fakeChannel(guild, ID(40));
  const other = fakeChannel(guild, ID(41), { name: 'other' });
  const member = fakeMember(guild, ID(42), memberRoles);
  return { guild, general, other, member };
}

test('dispatch: conditions, placeholders, mentions and run counting', async () => {
  const { guild, general, other, member } = setup('955028100000000001');
  const msg = fakeMessage(guild, general, member.user, { content: 'Hello world' });
  svc.__setAutomationMemo(guild.id, {
    MESSAGE_SENT: [
      lite('autoone01', 'MESSAGE_SENT', [{ type: 'CONTAINS', words: ['hello'] }, { type: 'CHANNEL_IN', channelIds: [general.id] }],
        [{ type: 'SEND_MESSAGE', content: 'Hi {user}! @everyone <@&' + ID(60) + '> {message.content}', channelId: other.id, button: { label: 'Click {user.name}', url: null, style: 'SUCCESS', index: 4 } }]),
      lite('autotwo02', 'MESSAGE_SENT', [{ type: 'CONTAINS', words: ['nope'] }], [{ type: 'PIN' }]),
    ],
  });
  await svc.dispatch('MESSAGE_SENT', baseOf(guild, { trigger: 'MESSAGE_SENT', member, user: member.user, channel: general, message: msg }), { awaitRuns: true });
  assert.equal(other.sent.length, 1);
  const out = other.sent[0];
  assert.match(out.content, new RegExp(`^Hi <@${member.id}>!`));
  assert.match(out.content, /Hello world$/);
  assert.deepEqual(out.allowedMentions, { parse: ['users'] });   // the role is not pingable, so it is not in `roles`
  assert.equal(msg.calls.pin, 0);
  const button = out.components[0].toJSON().components[0];
  assert.equal(button.custom_id, 'automation:autoone01:4');
  assert.equal(button.label, `Click ${member.user.username}`);
  assert.equal(button.style, 3);
});

test('dispatch: pingable role mentions typed by the admin ping, {role} never does', async () => {
  const { guild, member } = setup('955028100000000002');
  const general = fakeChannel(guild, ID(44), { name: 'announce', missing: ['MentionEveryone'] });   // the bot cannot Mention Everyone here
  const pingable = fakeRole(guild, ID(61), { mentionable: true });
  fakeRole(guild, ID(62), { mentionable: false });
  const staff = fakeMember(guild, ID(63), [ID(62)]);
  svc.__setAutomationMemo(guild.id, {
    ROLE_GAINED: [lite('autorole01', 'ROLE_GAINED', [], [{ type: 'SEND_MESSAGE', content: `<@&${pingable.id}> <@&${ID(62)}> gained {role}`, channelId: general.id }])],
  });
  await svc.dispatch('ROLE_GAINED', baseOf(guild, { trigger: 'ROLE_GAINED', member: staff, user: staff.user, channel: null, roleIds: [ID(62)] }), { awaitRuns: true });
  assert.deepEqual(general.sent[0].allowedMentions, { parse: ['users'], roles: [pingable.id] });
  assert.ok(member);
});

test('dispatch: the per-user cooldown falls back to the process when Redis is down', async () => {
  const { guild, general, member } = setup('955028100000000003');
  const other = fakeMember(guild, ID(43));
  svc.__setAutomationMemo(guild.id, {
    MESSAGE_SENT: [lite('autocool01', 'MESSAGE_SENT', [], [{ type: 'SEND_MESSAGE', content: 'ran', channelId: general.id }], { cooldownSeconds: 30 })],
  });
  const fire = (m) => svc.dispatch('MESSAGE_SENT', baseOf(guild, {
    trigger: 'MESSAGE_SENT', member: m, user: m.user, channel: general, message: fakeMessage(guild, general, m.user),
  }), { awaitRuns: true });
  await fire(member);
  await fire(member);
  assert.equal(general.sent.length, 1);
  await fire(other);
  assert.equal(general.sent.length, 2);
});

test('dispatch: at most 30 runs per 10 seconds per guild', async () => {
  const { guild, general, member } = setup('955028100000000004');
  svc.__setAutomationMemo(guild.id, { MESSAGE_SENT: [lite('autobud01', 'MESSAGE_SENT', [], [{ type: 'SEND_MESSAGE', content: 'x', channelId: general.id }])] });
  for (let i = 0; i < 35; i++) {
    await svc.dispatch('MESSAGE_SENT', baseOf(guild, { trigger: 'MESSAGE_SENT', member, user: member.user, channel: general, message: fakeMessage(guild, general, member.user) }), { awaitRuns: true });
  }
  assert.equal(general.sent.length, svc.RUN_BUDGET_MAX);
});

test('dispatch: the queue holds at most 25 runs per guild', async () => {
  const { guild, general, member } = setup('955028100000000005');
  svc.__setAutomationMemo(guild.id, { MESSAGE_SENT: [lite('autoque01', 'MESSAGE_SENT', [], [{ type: 'SEND_MESSAGE', content: 'x', channelId: general.id }])] });
  const gate = { release: null };
  general.send = (p) => new Promise((resolveSend) => { gate.release ??= []; gate.release.push(() => { general.sent.push(p); resolveSend({ id: 's' }); }); });
  // Not awaited: all 28 dispatches queue behind the first run, which is blocked in send().
  const all = [];
  for (let i = 0; i < 28; i++) {
    all.push(svc.dispatch('MESSAGE_SENT', baseOf(guild, { trigger: 'MESSAGE_SENT', member, user: member.user, channel: general, message: fakeMessage(guild, general, member.user) })));
  }
  await Promise.all(all);
  await flush();
  // Release the blocked sends one by one until the queue drains.
  for (let i = 0; i < 40; i++) {
    const next = gate.release?.shift();
    if (next) next();
    await flush();
  }
  assert.equal(general.sent.length, svc.MAX_PENDING_RUNS);
});

test('role actions: one at a time, checkAssignableRole first, chain depth stops at 3', async () => {
  const { guild, general, member } = setup('955028100000000006');
  const roles = [ID(71), ID(72), ID(73), ID(74), ID(75)].map((id) => fakeRole(guild, id));
  const dangerous = fakeRole(guild, ID(76));
  dangerous.permissions = { has: () => true };   // Administrator and friends
  const higher = fakeRole(guild, ID(77), { position: 99 });
  svc.__setAutomationMemo(guild.id, {
    ROLE_GAINED: [
      lite('chain-a01', 'ROLE_GAINED', [{ type: 'GAINED_ROLE', roleIds: [ID(71)] }], [{ type: 'GIVE_ROLES', roleIds: [ID(72), ID(76), ID(77)] }]),
      lite('chain-b01', 'ROLE_GAINED', [{ type: 'GAINED_ROLE', roleIds: [ID(72)] }], [{ type: 'GIVE_ROLES', roleIds: [ID(73)] }]),
      lite('chain-c01', 'ROLE_GAINED', [{ type: 'GAINED_ROLE', roleIds: [ID(73)] }], [{ type: 'GIVE_ROLES', roleIds: [ID(74)] }]),
      lite('chain-d01', 'ROLE_GAINED', [{ type: 'GAINED_ROLE', roleIds: [ID(74)] }], [{ type: 'GIVE_ROLES', roleIds: [ID(75)] }]),
    ],
  });
  // Each step stands in for the guildMemberUpdate that Discord would send after the previous role change.
  const step = async (gainedRole) => {
    const oldRoles = [...member.roles.cache.keys()].filter((r) => r !== gainedRole);
    await svc.onMemberUpdate({
      guild, client: { user: { id: BOT } }, guildId: guild.id, oldKnown: true, getSettings: async () => null,
      oldMember: { roles: { cache: new Collection(oldRoles.map((r) => [r, { id: r }])) } },
      newMember: member,
    });
    for (let i = 0; i < 5; i++) await flush();
  };
  member.roles.cache.set(ID(71), { id: ID(71) });
  await step(ID(71));
  assert.deepEqual(member.calls.map((c) => c[1]), [ID(72)]);   // the dangerous and the too-high role were refused; no array form
  await step(ID(72));
  await step(ID(73));
  await step(ID(74));   // depth 3: automation D is dropped
  assert.deepEqual(member.calls.map((c) => c[1]), [ID(72), ID(73), ID(74)]);
  assert.equal(member.roles.cache.has(ID(75)), false);
  assert.ok(roles.length && higher);
  for (const c of member.calls) assert.equal(c[0], 'add');
  assert.match(member.calls[0][2], /^Automation: chain-a01$/);
});

test('role actions: TAKE_ROLES and a pending captcha', async () => {
  const { guild, general, member } = setup('955028100000000007', { memberRoles: [ID(81), ID(82)] });
  fakeRole(guild, ID(81)); fakeRole(guild, ID(82)); fakeRole(guild, ID(83));
  svc.__setAutomationMemo(guild.id, {
    MEMBER_JOINED: [lite('join-a001', 'MEMBER_JOINED', [], [{ type: 'GIVE_ROLES', roleIds: [ID(83)] }, { type: 'SEND_MESSAGE', content: 'welcome', channelId: general.id }])],
    ROLE_LOST: [lite('lost-a001', 'ROLE_LOST', [], [{ type: 'TAKE_ROLES', roleIds: [ID(81), ID(82), ID(83)] }])],
  });
  const join = (captchaPending) => svc.dispatch('MEMBER_JOINED', baseOf(guild, { trigger: 'MEMBER_JOINED', member, user: member.user, channel: null, captchaPending }), { awaitRuns: true });
  await join(true);
  assert.equal(member.calls.length, 0);                  // GIVE_ROLES is skipped while verification is pending
  assert.equal(general.sent.length, 1);                  // the rest of the automation still runs
  await join(false);
  assert.deepEqual(member.calls.map((c) => c.slice(0, 2)), [['add', ID(83)]]);
  await svc.dispatch('ROLE_LOST', baseOf(guild, { trigger: 'ROLE_LOST', member, user: member.user, channel: null, roleIds: [ID(1)] }), { awaitRuns: true });
  assert.deepEqual(member.calls.slice(1).map((c) => c.slice(0, 2)), [['remove', ID(81)], ['remove', ID(82)], ['remove', ID(83)]]);
});

test('REACTION_ADDED: bot-authored messages are allowed, system messages are not; the message is fetched lazily', async () => {
  const { guild, general, member } = setup('955028100000000008');
  const botUser = { id: BOT, bot: true, username: 'bot' };
  const botMsg = fakeMessage(guild, general, botUser, { content: 'panel' });
  const starred = fakeMessage(guild, general, member.user, { content: 'star me', reactions: [{ emoji: { id: null, name: '⭐' }, count: 3, me: false }] });
  svc.__setAutomationMemo(guild.id, {
    REACTION_ADDED: [lite('react-a01', 'REACTION_ADDED',
      [{ type: 'REACTED_WITH', emojis: ['⭐'] }, { type: 'HAS_REACTIONS', min: 3, emoji: '⭐' }],
      [{ type: 'REACT', emoji: '👍' }])],
  });
  let fetches = 0;
  const fire = (message, key = '⭐', ensure) => svc.dispatch('REACTION_ADDED', baseOf(guild, {
    trigger: 'REACTION_ADDED', member, user: member.user, channel: general, message: { id: message.id, partial: true },
    reaction: { key, display: key }, ensureMessage: ensure ?? (async () => { fetches++; return message; }),
  }), { awaitRuns: true });
  starred.reactions.cache.first().count = 3;
  await fire(starred, '🌟');
  assert.equal(fetches, 0);                       // the cheap condition failed: no fetch
  await fire(starred);
  assert.equal(fetches, 1);
  assert.deepEqual(starred.calls.react, ['👍']);
  botMsg.reactions.cache.set('0', { emoji: { id: null, name: '⭐' }, count: 3, me: false });
  await fire(botMsg);
  assert.deepEqual(botMsg.calls.react, ['👍']);   // allowed on a bot-authored message
  const sys = fakeMessage(guild, general, member.user, { system: true, reactions: [{ emoji: { id: null, name: '⭐' }, count: 3, me: false }] });
  await fire(sys);
  assert.deepEqual(sys.calls.react, []);
  const gone = await fire(starred, '⭐', async () => null);
  assert.equal(gone, undefined);
});

test('message actions: DELETE marks the deletion, PIN, REPLY, REPOST dedupe, threads', async () => {
  const { guild, general, other, member } = setup('955028100000000009');
  const img = { name: 'pic.png', contentType: 'image/png', url: 'https://cdn.test/pic.png' };
  const doc = { name: 'a.zip', contentType: 'application/zip', url: 'https://cdn.test/a.zip' };
  const msg = fakeMessage(guild, general, member.user, { content: 'look at this', attachments: [img, doc] });
  svc.__setAutomationMemo(guild.id, {
    MESSAGE_SENT: [
      lite('msg-repost1', 'MESSAGE_SENT', [{ type: 'ATTACHMENT_IMAGE' }], [{ type: 'REPOST', channelId: other.id }]),
      lite('msg-thread1', 'MESSAGE_SENT', [{ type: 'IS_NOT_REPLY' }], [
        { type: 'CREATE_THREAD', name: '{user.name} talk', channelId: null, autoArchiveMinutes: 60 },
        { type: 'ADD_TO_THREAD', userIds: [] },
        { type: 'SEND_IN_THREAD', content: 'welcome to {thread.name}' },
        { type: 'PIN' },
        { type: 'REPLY', content: 'noted' },
        { type: 'DELETE' },
        { type: 'REACT', emoji: '👍' },   // after DELETE: skipped
      ]),
    ],
  });
  const fire = () => svc.dispatch('MESSAGE_SENT', baseOf(guild, { trigger: 'MESSAGE_SENT', member, user: member.user, channel: general, message: msg }), { awaitRuns: true });
  await fire();
  await fire();   // the repost is deduped (Redis is down, so the in-process set decides)
  assert.equal(other.sent.length, 1);
  const embed = other.sent[0].embeds[0].toJSON();
  assert.equal(embed.description, 'look at this');
  assert.equal(embed.image.url, img.url);
  assert.equal(embed.footer.text, '#general');
  assert.equal(embed.author.name, member.displayName);
  assert.match(embed.fields[0].value, /a\.zip/);
  assert.deepEqual(other.sent[0].allowedMentions, { parse: [] });
  assert.equal(other.sent[0].components[0].toJSON().components[0].url, msg.url);

  assert.deepEqual(msg.calls.thread.map((t) => [t.name, t.autoArchiveDuration]), [[`${member.user.username} talk`, 60], [`${member.user.username} talk`, 60]]);
  assert.equal(msg.calls.pin, 2);
  assert.equal(msg.calls.deleted, 2);
  assert.equal(wasDeletedByBot(msg.id), 'automations');
  assert.deepEqual(msg.calls.react, []);
  assert.deepEqual(general.sent[0].reply, { messageReference: msg.id, failIfNotExists: false });   // REPLY ran before DELETE
});

test('message actions: REPLY after DELETE has no reference; bot deletions and bots never fire MESSAGE_DELETED', async () => {
  const { guild, general, member } = setup('955028100000000010');
  const msg = fakeMessage(guild, general, member.user, { content: 'x' });
  svc.__setAutomationMemo(guild.id, {
    MESSAGE_SENT: [lite('del-then01', 'MESSAGE_SENT', [], [{ type: 'DELETE' }, { type: 'REPLY', content: 'gone' }])],
    MESSAGE_DELETED: [lite('deleted-a1', 'MESSAGE_DELETED', [{ type: 'CONTAINS', words: ['bye'] }], [{ type: 'SEND_MESSAGE', content: 'saw {message.content} by {message.author.name}', channelId: general.id }])],
  });
  await svc.onMessageCreate({ guild, guildId: guild.id, client: { user: { id: BOT } }, message: msg, state: {}, getSettings: async () => null });
  for (let i = 0; i < 5; i++) await flush();
  assert.equal(msg.calls.deleted, 1);
  assert.equal(general.sent.length, 1);
  assert.equal(general.sent[0].reply, undefined);   // the message is gone: a reference would fail
  // The deletion above was made by the bot: MESSAGE_DELETED must not fire for it.
  const ctxFor = (m) => ({ guild, guildId: guild.id, client: { user: { id: BOT } }, message: m, partial: false, getSettings: async () => null });
  const byBot = fakeMessage(guild, general, member.user, { content: 'bye bye' });
  markBotDeletion(byBot.id, 'automod');
  await svc.onMessageDelete(ctxFor(byBot));
  for (let i = 0; i < 5; i++) await flush();
  assert.equal(general.sent.length, 1);
  const byMember = fakeMessage(guild, general, member.user, { content: 'bye bye' });
  await svc.onMessageDelete(ctxFor(byMember));
  for (let i = 0; i < 5; i++) await flush();
  assert.equal(general.sent.length, 2);
  assert.equal(general.sent[1].content, `saw bye bye by ${member.user.username}`);
  // A message automod deleted never reaches MESSAGE_SENT automations.
  const automodded = fakeMessage(guild, general, member.user, { content: 'x' });
  await svc.onMessageCreate({ guild, guildId: guild.id, client: { user: { id: BOT } }, message: automodded, state: { automodAction: { deleted: true } }, getSettings: async () => null });
  for (let i = 0; i < 5; i++) await flush();
  assert.equal(automodded.calls.deleted, 0);
});

test('BUTTON_CLICKED: the click is acknowledged, an ephemeral reply follows, other buttons are ignored', async () => {
  const { guild, general, member } = setup('955028100000000011');
  fakeRole(guild, ID(91));
  svc.__setAutomationMemo(guild.id, {
    BUTTON_CLICKED: [lite('accept-btn1', 'BUTTON_CLICKED', [{ type: 'BUTTON_IS', buttons: [{ automationId: 'panelrow01', index: 1 }] }],
      [{ type: 'GIVE_ROLES', roleIds: [ID(91)] }, { type: 'REPLY', content: 'Done, {user}!' }])],
  });
  const replies = [];
  let acks = 0;
  const click = (customId) => svc.onAutomationButton({
    customId, guild, guildId: guild.id, member, user: member.user, channel: general, message: fakeMessage(guild, general, { id: BOT, bot: true, username: 'bot' }),
    inCachedGuild: () => true, deferUpdate: async () => { acks++; }, followUp: async (p) => { replies.push(p); },
  }, { user: { id: BOT } });
  await click('automation:panelrow01:2');
  assert.deepEqual([acks, member.calls.length, replies.length], [1, 0, 0]);
  await click('automation:panelrow01:1');
  assert.equal(acks, 2);
  assert.deepEqual(member.calls.map((c) => c.slice(0, 2)), [['add', ID(91)]]);
  assert.equal(replies[0].ephemeral, true);
  assert.equal(replies[0].content, `Done, <@${member.id}>!`);
  await click('automation:bad:1');
  await click('automation:panelrow01:100');
  assert.equal(acks, 4);
  const dm = [];
  await svc.onAutomationButton({ inCachedGuild: () => false, reply: async (p) => { dm.push(p); } }, {});
  assert.equal(dm[0].ephemeral, true);
});

test('reward actions go through the rewards bus', async () => {
  const { guild, general, member } = setup('955028100000000012');
  const calls = [];
  rewardsBus.registerProvider('coins', {
    async grant(a) { calls.push(['grant', a.amount, a.userId, a.reason]); return { ok: true, total: 5 }; },
    async take() { return { ok: false, reason: 'insufficient' }; },
  }, 'test-automations');
  svc.__setAutomationMemo(guild.id, {
    VOICE_JOINED: [lite('voice-a001', 'VOICE_JOINED', [], [{ type: 'GIVE_COINS', amount: 5 }, { type: 'TAKE_COINS', amount: 5 }, { type: 'GIVE_XP', amount: 5 }])],
  });
  const vc = fakeChannel(guild, ID(95), { type: ChannelType.GuildVoice, name: 'Lounge' });
  await svc.onVoiceState({ guild, guildId: guild.id, client: { user: { id: BOT } }, change: 'join', member, oldState: { channel: null }, newState: { channel: vc }, getSettings: async () => null });
  for (let i = 0; i < 5; i++) await flush();
  assert.deepEqual(calls, [['grant', 5, member.id, 'automation:voice-a001']]);   // XP has no provider: skipped, not thrown
  assert.ok(general);
});

test('THREAD_CREATED: bots are ignored and the owner is resolved', async () => {
  const { guild, general, member } = setup('955028100000000013');
  const forum = fakeChannel(guild, ID(96), { name: 'forum' });
  const thread = { ...fakeChannel(guild, ID(97), { name: 'new-thread', thread: true, parentId: forum.id }), ownerId: member.id, archived: false, locked: false, members: { added: [], async add(id) { this.added.push(id); } } };
  svc.__setAutomationMemo(guild.id, {
    THREAD_CREATED: [lite('thread-a01', 'THREAD_CREATED', [{ type: 'CHANNEL_IN', channelIds: [forum.id] }], [{ type: 'ADD_TO_THREAD', userIds: [ID(98)] }, { type: 'SEND_IN_THREAD', content: 'hi {user}' }])],
  });
  fakeMember(guild, ID(98));
  await svc.onThreadCreate({ guild, guildId: guild.id, client: { user: { id: BOT } }, thread, getSettings: async () => null });
  for (let i = 0; i < 5; i++) await flush();
  assert.deepEqual(thread.members.added, [ID(98)]);
  assert.equal(thread.sent[0].content, `hi <@${member.id}>`);
  const botOwned = { ...thread, ownerId: ID(120) };
  fakeMember(guild, ID(120), [], { bot: true });
  await svc.onThreadCreate({ guild, guildId: guild.id, client: { user: { id: BOT } }, thread: botOwned, getSettings: async () => null });
  for (let i = 0; i < 5; i++) await flush();
  assert.equal(thread.sent.length, 1);
  assert.ok(general);
});

test('a failing action never stops the next one', async () => {
  const { guild, general, member } = setup('955028100000000014');
  const broken = fakeChannel(guild, ID(99), { name: 'broken' });
  broken.send = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  svc.__setAutomationMemo(guild.id, {
    MESSAGE_SENT: [lite('fail-a0001', 'MESSAGE_SENT', [], [
      { type: 'SEND_MESSAGE', content: 'first', channelId: broken.id },
      { type: 'SEND_MESSAGE', content: 'second', channelId: general.id },
      { type: 'SEND_MESSAGE', content: 'to a missing channel', channelId: ID(998) },
    ])],
  });
  await svc.dispatch('MESSAGE_SENT', baseOf(guild, { trigger: 'MESSAGE_SENT', member, user: member.user, channel: general, message: fakeMessage(guild, general, member.user) }), { awaitRuns: true });
  assert.deepEqual(general.sent.map((s) => s.content), ['second']);
});

test('CREATE_THREAD in a channel, REMOVE_REACTION, voice moves and MEMBER_JOINED bots', async () => {
  const { guild, general, other, member } = setup('955028100000000017');
  // A channel that can host threads.
  const created = [];
  const threads = [];
  general.threads = {
    async create(opts) { created.push(opts); const th = { id: ID(600), name: opts.name, archived: false, locked: false, sent: [], isThread: () => true, isTextBased: () => true, permissionsFor: () => perms(), guild, async send(p) { th.sent.push(p); } }; threads.push(th); return th; },
  };
  general.type = ChannelType.GuildAnnouncement;
  svc.__setAutomationMemo(guild.id, {
    MEMBER_JOINED: [lite('join-thr01', 'MEMBER_JOINED', [], [
      { type: 'CREATE_THREAD', name: 'Welcome {user.name}', channelId: general.id, autoArchiveMinutes: 4320 },
      { type: 'SEND_IN_THREAD', content: 'This is {thread.name} ({thread})' },
    ])],
    VOICE_JOINED: [lite('voice-j001', 'VOICE_JOINED', [], [{ type: 'SEND_MESSAGE', content: 'joined {voice.name}', channelId: other.id }])],
    VOICE_LEFT: [lite('voice-l001', 'VOICE_LEFT', [], [{ type: 'SEND_MESSAGE', content: 'left {voice.name}', channelId: other.id }])],
    REACTION_ADDED: [lite('react-r001', 'REACTION_ADDED', [{ type: 'REACTED_WITH', emojis: ['🚫'] }], [{ type: 'REMOVE_REACTION', emoji: null }])],
  });
  await svc.onMemberAdd({ guild, guildId: guild.id, client: { user: { id: BOT } }, member, state: {}, getSettings: async () => null });
  for (let i = 0; i < 5; i++) await flush();
  assert.deepEqual(created.map((c) => [c.name, c.autoArchiveDuration, c.type]), [[`Welcome ${member.user.username}`, 4320, ChannelType.AnnouncementThread]]);
  assert.equal(threads[0].sent[0].content, `This is Welcome ${member.user.username} (<#${ID(600)}>)`);   // {thread} is read at render time

  const a = fakeChannel(guild, ID(601), { type: ChannelType.GuildVoice, name: 'Lounge' });
  const b = fakeChannel(guild, ID(602), { type: ChannelType.GuildVoice, name: 'Games' });
  await svc.onVoiceState({ guild, guildId: guild.id, client: { user: { id: BOT } }, change: 'move', member, oldState: { channel: a }, newState: { channel: b }, getSettings: async () => null });
  for (let i = 0; i < 5; i++) await flush();
  assert.deepEqual(other.sent.map((s) => s.content), ['left Lounge', 'joined Games']);   // LEFT first, then JOINED

  const removed = [];
  const msg = fakeMessage(guild, general, member.user, { reactions: [{ emoji: { id: null, name: '🚫' }, count: 1, me: false, users: { remove: async (id) => { removed.push(id); } } }] });
  await svc.dispatch('REACTION_ADDED', baseOf(guild, {
    trigger: 'REACTION_ADDED', member, user: member.user, channel: general, message: msg, reaction: { key: '🚫', display: '🚫' }, ensureMessage: async () => msg,
  }), { awaitRuns: true });
  assert.deepEqual(removed, [member.id]);

  const botMember = fakeMember(guild, ID(603), [], { bot: true });
  assert.equal(svc.filterMemberAdd({ guildId: guild.id, member: botMember }), false);
});

// ── /automation option mapping ───────────────────────────────────────────────

test('conditionParamsFromOptions and actionParamsFromOptions', async () => {
  const { guild, general, other } = setup('955028100000000015');
  const category = fakeChannel(guild, ID(100), { type: ChannelType.GuildCategory, name: 'cat' });
  const role = fakeRole(guild, ID(101));
  fakeMember(guild, ID(102));
  const botMember = fakeMember(guild, ID(103), [], { bot: true });
  const executor = { id: ID(104), roles: { highest: { position: 99 } } };
  const ctx = { guild, member: executor, settings: null };
  const opts = (o = {}) => ({ channel: null, role: null, user: null, text: null, number: null, button: null, emoji: null, amount: null, embed: null, buttonLabel: null, buttonUrl: null, ...o });
  const cond = (type, o) => svc.conditionParamsFromOptions(type, opts(o), ctx);
  const act = (type, o) => svc.actionParamsFromOptions(type, opts(o), ctx);

  assert.deepEqual((await cond('CHANNEL_IN', { channel: general, text: `<#${other.id}>` })).params, { channelIds: [general.id, other.id] });
  assert.equal((await cond('CHANNEL_IN', { channel: category })).error, 'That is a category. Use "Category is one of" instead.');
  assert.equal((await cond('CHANNEL_IN', {})).error, 'Pick a channel or mention channels in text.');
  assert.deepEqual((await cond('CATEGORY_IN', { channel: category })).params, { categoryIds: [category.id] });
  assert.equal((await cond('CATEGORY_IN', { channel: general })).error, 'Pick a category (not a channel).');
  assert.equal((await cond('CHANNEL_IN', { text: `<#${ID(999)}>` })).error, `Unknown channel: ${ID(999)}`);
  assert.deepEqual((await cond('CONTAINS', { text: 'a, B,,a' })).params, { words: ['a', 'B'] });
  assert.equal((await cond('CONTAINS', { text: ' , ' })).error, 'Give words in text, separated by commas.');
  assert.equal((await cond('CONTAINS', { text: 'x'.repeat(101) })).error, 'Each word can be at most 100 characters.');
  assert.deepEqual((await cond('EXACT', { text: ' Hi there ' })).params, { texts: ['Hi there'] });
  assert.deepEqual((await cond('MATCHES_REGEX', { text: '/^hi/im' })).params, { pattern: '^hi', flags: 'im' });
  assert.deepEqual((await cond('MATCHES_REGEX', { text: '^hi' })).params, { pattern: '^hi', flags: 'i' });
  assert.match((await cond('MATCHES_REGEX', { text: '(a+)+' })).error, /^Invalid regex: /);
  assert.match((await cond('MATCHES_REGEX', { text: '/a/g' })).error, /^Invalid regex: /);
  assert.deepEqual((await cond('REACTED_WITH', { text: '⭐ 👍' })).params, { emojis: ['⭐', '👍'] });
  assert.equal((await cond('REACTED_WITH', { text: '' })).error, 'Give one or more emojis in text.');
  assert.deepEqual((await cond('HAS_REACTIONS', { number: 3, text: '⭐' })).params, { min: 3, emoji: '⭐' });
  assert.equal((await cond('HAS_REACTIONS', {})).error, 'Give the minimum count in number.');
  assert.deepEqual((await cond('HAS_ANY_ROLE', { role, text: `<@&${role.id}>` })).params, { roleIds: [role.id] });
  assert.equal((await cond('HAS_ANY_ROLE', { text: `<@&${guild.id}>` })).error, `Unknown role: ${guild.id}`);
  assert.equal((await cond('HAS_ANY_ROLE', {})).error, 'Pick a role or mention roles in text.');
  assert.deepEqual((await cond('USER_IS', { text: `<@${ID(102)}>` })).params, { userIds: [ID(102)] });
  assert.equal((await cond('USER_IS', { text: `<@${ID(777)}>` })).error, `Unknown user: ${ID(777)}`);
  assert.equal((await cond('BUTTON_IS', { button: 'nope' })).error, 'Pick a button from the list.');
  assert.deepEqual((await cond('IS_REPLY', { text: 'ignored', role })).ignored, ['role', 'text']);
  assert.equal((await cond('NOPE', {})).ok, false);

  assert.deepEqual((await act('CREATE_THREAD', {})).params, { name: svc.DEFAULT_THREAD_NAME, channelId: null, autoArchiveMinutes: 1440 });
  assert.equal((await act('CREATE_THREAD', { channel: category })).error, 'Pick a text or announcement channel.');
  const send = await act('SEND_MESSAGE', { text: 'hi', channel: general, buttonLabel: 'Go', buttonUrl: 'https://x.y' });
  assert.deepEqual(send.params, { content: 'hi', channelId: general.id, button: { label: 'Go', url: 'https://x.y', style: 'PRIMARY', index: null } });
  assert.deepEqual((await act('SEND_MESSAGE', { text: 'hi', buttonLabel: 'Go' })).params.button, { label: 'Go', url: null, style: 'PRIMARY', index: null });
  assert.equal((await act('SEND_MESSAGE', { text: 'hi', buttonUrl: 'https://x.y' })).error, 'Give button_label too.');
  assert.equal((await act('SEND_MESSAGE', { text: 'hi', buttonLabel: 'Go', buttonUrl: 'ftp://x' })).error, 'The button link must start with http:// or https://.');
  assert.equal((await act('SEND_MESSAGE', {})).error, 'Give the message in text.');
  const noPerm = fakeChannel(guild, ID(105), { name: 'locked', missing: ['SendMessages'] });
  const denied = await act('SEND_MESSAGE', { text: 'hi', channel: noPerm });
  assert.deepEqual([denied.ok, denied.errorKind, denied.missing], [false, 'perms', ['SendMessages']]);
  assert.equal((await act('REPOST', {})).error, 'Pick the channel to repost to.');
  assert.equal((await act('REPOST', { channel: noPerm })).errorKind, 'perms');
  assert.deepEqual((await act('REACT', { emoji: '👍' })).params, { emoji: '👍' });
  assert.equal((await act('REACT', {})).ok, false);
  assert.deepEqual((await act('REMOVE_REACTION', {})).params, { emoji: null });
  assert.deepEqual((await act('GIVE_ROLES', { role })).params, { roleIds: [role.id] });
  const everyone = await act('GIVE_ROLES', { text: `<@&${guild.id}>` });
  assert.equal(everyone.ok, false);
  const tooHigh = fakeRole(guild, ID(106), { position: 120 });
  const refused = await act('GIVE_ROLES', { role: tooHigh });
  assert.deepEqual([refused.ok, refused.errorKind], [false, 'role']);
  assert.deepEqual((await act('ADD_TO_THREAD', { text: `<@${ID(102)}>` })).params, { userIds: [ID(102)] });
  assert.equal((await act('ADD_TO_THREAD', { user: botMember.user })).error, 'Bots cannot be added to threads by automations.');
  assert.deepEqual((await act('ADD_TO_THREAD', {})).params, { userIds: [] });
  assert.equal((await act('GIVE_COINS', {})).error, 'Give the amount.');
  const noRewards = await act('GIVE_XP', { amount: 5 });
  assert.deepEqual([noRewards.errorKind, noRewards.rewardKind], ['reward', 'xp']);
  assert.deepEqual((await act('PIN', { text: 'x' })).ignored, ['text']);
  assert.equal((await act('NOPE', {})).ok, false);
});

test('resolveImportReferences flags what does not exist here', async () => {
  const { guild, general } = setup('955028100000000016');
  const role = fakeRole(guild, ID(110));
  const higher = fakeRole(guild, ID(111), { position: 200 });
  fakeMember(guild, ID(112));
  const data = {
    name: 'x', trigger: 'MESSAGE_SENT', enabled: true, cooldownSeconds: 0, exportedId: 'clxselfid001',
    conditions: svc.normalizeList([
      { type: 'CHANNEL_IN', channelIds: [general.id, ID(900)] }, { type: 'HAS_ANY_ROLE', roleIds: [role.id, ID(901)] },
      { type: 'USER_IS', userIds: [ID(112), ID(902)] }, { type: 'CATEGORY_IN', categoryIds: [general.id] },
    ], 'condition').items,
    actions: svc.normalizeList([
      { type: 'GIVE_ROLES', roleIds: [role.id, higher.id] }, { type: 'GIVE_XP', amount: 5 }, { type: 'SEND_MESSAGE', content: 'x', channelId: ID(903) },
    ], 'action').items,
  };
  const r = await svc.resolveImportReferences(guild, data, { settings: null, existingRow: null });
  const text = r.warnings.join('\n');
  assert.match(text, new RegExp(`Condition 1: Unknown channel ${ID(900)}`));
  assert.match(text, new RegExp(`Condition 2: Unknown role ${ID(901)}`));
  assert.match(text, new RegExp(`Unknown member ${ID(902)}`));
  assert.match(text, /Condition 4: .* is not a category/);
  assert.match(text, /Action 1: That role is above my highest role/);
  assert.match(text, /Action 2: XP rewards are not available \(module off\)/);
  assert.match(text, new RegExp(`Action 3: Unknown channel ${ID(903)}`));
  assert.equal(r.warnings.length, 7);
  assert.notEqual(r.data, data);
});

// ── Static guards ────────────────────────────────────────────────────────────

test('static scan of the owned files', () => {
  const files = ['src/services/automationService.js', 'src/commands/admin/automation.js', 'src/plugins/automations.js'];
  for (const f of files) {
    const text = readFileSync(resolve(ROOT, f), 'utf8');
    assert.doesNotMatch(text, /\bconsole\.(log|error|warn|info|debug)\(/, f);
    assert.doesNotMatch(text, /LIMITS\.[A-Z_]+\s*\?\?/, f);
    assert.doesNotMatch(text, /cache\.del\(['"`]guild:[^'"`]*settings/, f);
    assert.doesNotMatch(text, /guildSettings\.(update|upsert|create)/, f);
  }
  const service = readFileSync(resolve(ROOT, files[0]), 'utf8');
  // Every setNX call site documents its fail-open / fail-closed decision.
  for (const m of service.matchAll(/cache\.setNX\(/g)) {
    const before = service.slice(Math.max(0, m.index - 600), m.index);
    assert.match(before, /Redis down|Degrade decision/i, `setNX at ${m.index} lacks a degrade comment`);
  }
  // markBotDeletion precedes the only message delete.
  assert.ok(service.indexOf("markBotDeletion(message.id, 'automations')") < service.indexOf('await message.delete()'));
  assert.deepEqual(svc.RAW_SQL.map((r) => r.name).sort(), ['automation.incrementUses', 'automation.lockById', 'automation.lockByName', 'automation.lockGuildSettings']);
  for (const { build } of svc.RAW_SQL) {
    const q = build();
    assert.equal(typeof q.sql, 'string');
    assert.doesNotMatch(q.sql, /(?<![\w.])now\(\)/i);
  }
});

// ── DB cases (TEST_DATABASE_URL) ─────────────────────────────────────────────

const rowData = (name, extra = {}) => ({
  name, trigger: 'MESSAGE_SENT', conditions: [], actions: [{ type: 'PIN' }], enabled: true, cooldownSeconds: 0, ...extra,
});

test('DB: create enforces the cap under the lock, the unique name and the shape', { skip: skipDb }, async () => {
  const g = GUILD;
  await prisma.guildSettings.deleteMany({ where: { guildId: g } });
  const made = await svc.createAutomation(g, rowData('First Rule'), 'creator-1');
  assert.equal(made.name, 'first-rule');
  assert.equal(made.createdBy, 'creator-1');
  assert.equal(made.enabled, true);
  await assert.rejects(svc.createAutomation(g, rowData('first-rule'), 'creator-1'), (e) => svc.isUniqueViolation(e));
  await assert.rejects(svc.createAutomation(g, rowData('!!!'), 'creator-1'), (e) => e.code === 'NAME');
  await assert.rejects(svc.createAutomation(g, rowData('bad', { trigger: 'MEMBER_JOINED', actions: [{ type: 'REPLY', content: 'x' }] }), 'c'), svc.AutomationError);
  await assert.rejects(svc.createAutomation(g, rowData('nocreator'), ''), TypeError);
  const blank = await svc.createAutomation(g, rowData('blank', { actions: [], enabled: true }), 'c');
  assert.equal(blank.enabled, false);   // a row with no actions cannot be enabled
  // Fill to the cap concurrently: the lock lets exactly LIMITS.AUTOMATIONS rows through.
  const attempts = await Promise.allSettled(Array.from({ length: LIMITS.AUTOMATIONS + 5 }, (_, i) => svc.createAutomation(g, rowData(`bulk-${i}`), 'c')));
  assert.equal(await prisma.automation.count({ where: { guildId: g } }), LIMITS.AUTOMATIONS);
  const limited = attempts.filter((a) => a.status === 'rejected');
  assert.ok(limited.length >= 7);
  assert.ok(limited.every((a) => a.reason.code === 'LIMIT'));
  await prisma.automation.deleteMany({ where: { guildId: g, name: { startsWith: 'bulk-' } } });
});

test('DB: mutateAutomation serialises concurrent button adds; button numbers are never reused', { skip: skipDb }, async () => {
  const g = GUILD2;
  await svc.createAutomation(g, rowData('panel', { actions: [] , enabled: false }), 'c');
  const add = (label) => svc.mutateAutomation(g, 'panel', (cur) => {
    const r = svc.appendAction(cur, { type: 'SEND_MESSAGE', content: label, channelId: ID(1), button: { label, url: null, style: 'PRIMARY', index: null } });
    return { data: { actions: r.actions, buttonSeq: r.buttonSeq }, result: r };
  });
  const results = await Promise.all(['a', 'b', 'c', 'd'].map(add));
  const indexes = results.map((r) => r.result.buttonIndex).sort();
  assert.deepEqual(indexes, [1, 2, 3, 4]);
  const row = await svc.getAutomationByName(g, 'panel');
  assert.equal(row.actions.length, 4);
  assert.equal(row.buttonSeq, 4);
  // Remove the highest button, add another: the number is not reused.
  await svc.mutateAutomation(g, 'panel', (cur) => ({ data: { actions: svc.removeAt(cur.actions, 4).list } }));
  const again = await add('e');
  assert.equal(again.result.buttonIndex, 5);   // 4 was removed; buttonSeq stays at 4
  await assert.rejects(svc.mutateAutomation(g, 'missing', () => ({ data: {} })), (e) => e.code === 'NOTFOUND');
  // A shape error rolls back and changes nothing.
  await assert.rejects(svc.mutateAutomation(g, 'panel', () => ({ data: { actions: [{ type: 'ADD_TO_THREAD', userIds: [] }] , trigger: 'MEMBER_JOINED' } })), svc.AutomationError);
  assert.equal((await svc.getAutomationByName(g, 'panel')).actions.length, 4);
  // The ten-action cap holds under the lock.
  for (let i = 0; i < 6; i++) await svc.mutateAutomation(g, 'panel', (cur) => ({ data: { actions: svc.appendAction(cur, { type: 'PIN' }).actions } })).catch(() => {});
  assert.equal((await svc.getAutomationByName(g, 'panel')).actions.length, LIMITS.AUTOMATION_ACTIONS);
  // Rename onto a taken name: P2002 and the transaction rolls back.
  await svc.createAutomation(g, rowData('other'), 'c');
  await assert.rejects(svc.mutateAutomation(g, 'panel', () => ({ data: { name: 'other' } })), (e) => svc.isUniqueViolation(e));
  assert.ok(await svc.getAutomationByName(g, 'panel'));
  const buttons = await svc.listAutomationButtons(g);
  assert.deepEqual(buttons.map((b) => b.index), [1, 2, 3, 5]);
  assert.equal((await svc.searchAutomations(g, 'pan'))[0].name, 'panel');
  assert.equal((await svc.searchAutomations(g, 'zzz')).length, 0);
  assert.deepEqual((await svc.listAutomations(g)).map((r) => r.name).sort(), ['other', 'panel']);
});

test('DB: loading, caching and invalidation', { skip: skipDb }, async () => {
  const g = GUILD3;
  assert.deepEqual(await svc.getAutomations(g), {});
  await svc.createAutomation(g, rowData('on-one', { trigger: 'MESSAGE_EDITED' }), 'c');
  await svc.createAutomation(g, rowData('off-one', { trigger: 'MESSAGE_EDITED', enabled: false }), 'c');
  const loaded = await svc.getAutomations(g);   // createAutomation invalidated the memo
  assert.deepEqual(Object.keys(loaded), ['MESSAGE_EDITED']);
  assert.deepEqual(loaded.MESSAGE_EDITED.map((a) => a.name), ['on-one']);
  assert.equal(svc.peekHasTrigger(g, 'MESSAGE_EDITED'), true);
  assert.equal(svc.peekHasTrigger(g, 'MESSAGE_SENT'), false);
  await svc.mutateAutomation(g, 'off-one', () => ({ data: { enabled: true } }));
  assert.equal((await svc.getAutomations(g)).MESSAGE_EDITED.length, 2);
  // A row whose stored actions are all invalid is not cached; invalid elements are dropped, not fatal.
  await prisma.automation.update({ where: { guildId_name: { guildId: g, name: 'off-one' } }, data: { actions: [{ type: 'NOPE' }] } });
  await svc.invalidateAutomations(g);
  assert.deepEqual((await svc.getAutomations(g)).MESSAGE_EDITED.map((a) => a.name), ['on-one']);
  const hydrated = await svc.getAutomationByName(g, 'off-one');
  assert.deepEqual(hydrated.invalidActions.map((x) => x.index), [1]);
  assert.equal(hydrated.actions.length, 0);
  assert.equal(await svc.deleteAutomation(g, hydrated.id), 1);
  assert.equal(await svc.deleteAutomation(g, hydrated.id), 0);
  assert.equal((await svc.getAutomations(g)).MESSAGE_EDITED.length, 1);
});

test('DB: replace keeps uses and createdBy, buttonSeq only grows; the self reference is rewritten', { skip: skipDb }, async () => {
  const g = GUILD3;
  const btn = (index) => ({ type: 'SEND_MESSAGE', content: 'x', channelId: ID(1), button: { label: 'B', url: null, style: 'PRIMARY', index } });
  const created = await svc.createAutomation(g, rowData('selfref', {
    trigger: 'BUTTON_CLICKED',
    conditions: [{ type: 'BUTTON_IS', buttons: [{ automationId: 'clxexported1', index: 7 }] }],
    actions: [btn(7)],
  }), 'creator-x', { selfRefId: 'clxexported1' });
  assert.equal(created.buttonSeq, 7);
  assert.equal(created.conditions[0].buttons[0].automationId, created.id);
  await prisma.$executeRaw(svc.RAW_SQL.find((r) => r.name === 'automation.incrementUses').build());   // id '0': no row, no error
  await prisma.automation.update({ where: { id: created.id }, data: { uses: 41 } });
  const replaced = await svc.replaceAutomation(g, created.id, rowData('selfref-renamed', { trigger: 'MESSAGE_SENT', actions: [btn(2)], cooldownSeconds: 9 }));
  assert.equal(replaced.name, 'selfref-renamed');
  assert.equal(replaced.uses, 41);
  assert.equal(replaced.createdBy, 'creator-x');
  assert.equal(replaced.buttonSeq, 7);   // never shrinks
  assert.equal(replaced.cooldownSeconds, 9);
  await assert.rejects(svc.replaceAutomation(g, 'doesnotexist', rowData('x')), (e) => e.code === 'NOTFOUND');
  await assert.rejects(svc.replaceAutomation(g, created.id, rowData('on-one')), (e) => svc.isUniqueViolation(e));
  const refs = await svc.listAutomationRefs(g);
  assert.ok(refs.some((r) => r.id === created.id));
  const guild = fakeGuild(g);
  // resolveImportReferences: a remap by name is informational, a dangling reference is a warning, the self reference follows the row.
  const data = {
    exportedId: 'clxexported2',
    conditions: [{ type: 'BUTTON_IS', buttons: [
      { automationId: 'clxexported2', index: 1 }, { automationId: 'clxgoneother', index: 1, automationName: 'selfref-renamed' },
      { automationId: 'clxdangling1', index: 3 },
    ] }],
    actions: [{ type: 'PIN' }],
  };
  const r = await svc.resolveImportReferences(guild, data, { settings: null, existingRow: { id: created.id } });
  // The self reference and the name-hinted entry both end up on the row's id; the duplicate is collapsed.
  assert.deepEqual(r.data.conditions[0].buttons.map((b) => [b.automationId, b.index]), [[created.id, 1], ['clxdangling1', 3]]);
  assert.equal(r.info.length, 1);
  assert.equal(r.warnings.length, 1);
});

test('DB: raw statements run and bumpUses clamps at the Int maximum', { skip: skipDb }, async () => {
  const g = GUILD3;
  const row = await svc.createAutomation(g, rowData('counter'), 'c');
  // Every raw statement builder runs against the database (the id-bound ones match no row).
  for (const { name, build } of svc.RAW_SQL) {
    if (name === 'automation.incrementUses') await prisma.$executeRaw(build());
    else await prisma.$queryRaw(build());
  }
  // The executor itself: run an automation against a fake guild and read the counter back.
  const { guild, general, member } = setup(g);
  await prisma.automation.update({ where: { id: row.id }, data: { uses: 2147483647 } });
  await svc.executeAutomation(
    lite(row.id, 'MESSAGE_SENT', [], [{ type: 'SEND_MESSAGE', content: 'x', channelId: general.id }], { name: 'counter' }),
    baseOf(guild, { trigger: 'MESSAGE_SENT', member, user: member.user, channel: general, message: fakeMessage(guild, general, member.user) }),
  );
  await new Promise((r) => setTimeout(r, 300));
  assert.equal((await prisma.automation.findUnique({ where: { id: row.id } })).uses, 2147483647);   // clamped, no overflow error
  await prisma.automation.update({ where: { id: row.id }, data: { uses: 10 } });
  await svc.executeAutomation(
    lite(row.id, 'MESSAGE_SENT', [], [{ type: 'SEND_MESSAGE', content: 'x', channelId: general.id }], { name: 'counter' }),
    baseOf(guild, { trigger: 'MESSAGE_SENT', member, user: member.user, channel: general, message: fakeMessage(guild, general, member.user) }),
  );
  await new Promise((r) => setTimeout(r, 300));
  assert.equal((await prisma.automation.findUnique({ where: { id: row.id } })).uses, 11);
});

test('DB: SEND_EMBED renders a saved embed through the embeds contract', { skip: skipDb }, async () => {
  const embeds = await import('../src/services/embedService.js');
  const g = GUILD3;
  await embeds.createSavedEmbed(g, 'rules-embed', { content: 'Hello {user}', embed: { title: 'Rules for {user.name}', description: 'Be nice in {channel}' }, buttons: [] }, 'c');
  const { guild, general, member } = setup(g);
  await svc.executeAutomation(
    lite('embed-a001', 'MEMBER_JOINED', [], [{ type: 'SEND_EMBED', embedName: 'Rules Embed', channelId: general.id }, { type: 'SEND_EMBED', embedName: 'missing-embed', channelId: general.id }], { name: 'embed-auto' }),
    baseOf(guild, { trigger: 'MEMBER_JOINED', member, user: member.user, channel: null }),
  );
  assert.equal(general.sent.length, 1);
  assert.equal(general.sent[0].content, `Hello <@${member.id}>`);
  const embed = general.sent[0].embeds[0].toJSON();
  assert.equal(embed.title, `Rules for ${member.user.username}`);
  assert.equal(embed.description, `Be nice in <#${general.id}>`);   // {channel} is the target for a trigger with no channel
  assert.deepEqual(general.sent[0].allowedMentions, { parse: ['users'] });
  assert.deepEqual(await svc.searchSavedEmbedNames(g, 'rules'), ['rules-embed']);
  // The problems list notices an embed that disappeared.
  const row = { id: 'x', guildId: g, enabled: true, conditions: [], actions: [{ type: 'SEND_EMBED', embedName: 'missing-embed', channelId: general.id }], invalidConditions: [], invalidActions: [] };
  const problems = await svc.automationProblems(guild, row, null);
  assert.ok(problems.some((p) => /saved embed "missing-embed" was not found/.test(p)));
  await prisma.savedEmbed.deleteMany({ where: { guildId: g } });
});

// ── DB: /automation end to end with a fake interaction ───────────────────────

function fakeInteraction(guild, { group = null, sub, opts = {}, userId = ID(500) }) {
  const replies = [];
  const get = (name) => (Object.hasOwn(opts, name) ? opts[name] : null);
  const interaction = {
    guild, guildId: guild.id, replies, createdTimestamp: Date.now(), deferred: false, replied: false,
    user: { id: userId, bot: false },
    member: { id: userId, roles: { highest: { position: 99 } } },
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      get: (name) => (get(name) === null ? null : { name, value: get(name) }),
      getString: get, getInteger: get, getBoolean: get, getChannel: get, getRole: get, getUser: get, getMember: (n) => opts.__members?.[n] ?? null, getAttachment: get,
      getFocused: () => get('__focused'),
    },
    async deferReply() { interaction.deferred = true; },
    async editReply(p) { replies.push(p); return p; },
    async followUp(p) { replies.push(p); return p; },
  };
  return interaction;
}

const run = async (guild, spec) => {
  const interaction = fakeInteraction(guild, spec);
  await cmd.execute(interaction);
  const last = interaction.replies.at(-1);
  const embed = last?.embeds?.[0]?.toJSON?.() ?? {};
  return { title: embed.title ?? '', description: embed.description ?? '', fields: embed.fields ?? [], files: last?.files ?? [], interaction };
};

test('DB: /automation walks through create-blank, condition, action, enable, info, export and edit', { skip: skipDb }, async () => {
  const guild = fakeGuild(GUILD);
  const text = fakeChannel(guild, ID(510), { name: 'rules' });
  const role = fakeRole(guild, ID(511));
  await prisma.automation.deleteMany({ where: { guildId: GUILD } });
  await prisma.guildSettings.update({ where: { guildId: GUILD }, data: { automationsEnabled: false } });
  const { invalidateSettings } = await import('../src/services/settingsService.js');
  await invalidateSettings(GUILD);

  let r = await run(guild, { sub: 'create-blank', opts: { name: 'Rules Panel', trigger: 'BUTTON_CLICKED' } });
  assert.match(r.title, /Automation Created/);
  assert.match(r.description, /created \*\*off\*\*/);
  assert.match(r.description, /module is off/);   // automationsEnabled is false
  r = await run(guild, { sub: 'create-blank', opts: { name: 'rules-panel', trigger: 'BUTTON_CLICKED' } });
  assert.match(r.title, /Name Taken/);
  r = await run(guild, { sub: 'create-blank', opts: { name: '🎉', trigger: 'BUTTON_CLICKED' } });
  assert.match(r.title, /Invalid Name/);
  r = await run(guild, { sub: 'info', opts: { name: 'nope' } });
  assert.match(r.title, /Automation Not Found/);

  r = await run(guild, { sub: 'enable', opts: { name: 'rules-panel' } });
  assert.match(r.title, /No Actions/);
  r = await run(guild, { group: 'action', sub: 'add', opts: { name: 'rules-panel', type: 'Frobnicate' } });
  assert.match(r.title, /Unknown Type/);
  r = await run(guild, { group: 'action', sub: 'add', opts: { name: 'rules-panel', type: 'GIVE_ROLES', role } });
  assert.match(r.title, /Action Added/);
  assert.match(r.description, /This automation is \*\*off\*\*/);
  r = await run(guild, { group: 'action', sub: 'add', opts: { name: 'rules-panel', type: 'REACT', emoji: '👍' } });
  assert.match(r.title, /Not Available/);
  r = await run(guild, { group: 'action', sub: 'add', opts: { name: 'rules-panel', type: 'GIVE_XP', amount: 5 } });
  assert.match(r.title, /Not Available/);   // no levels provider here
  assert.match(r.description, /XP rewards need the Levels module/);
  r = await run(guild, { group: 'condition', sub: 'add', opts: { name: 'rules-panel', type: 'CONTAINS', text: 'x' } });
  assert.match(r.title, /Not Available/);

  // A second automation posts the button.
  r = await run(guild, { sub: 'create-blank', opts: { name: 'panel', trigger: 'MEMBER_JOINED' } });
  r = await run(guild, { group: 'action', sub: 'add', opts: { name: 'panel', type: 'SEND_MESSAGE', channel: text, text: 'Accept the rules, {user}', button_label: 'Accept', emoji: '⭐' } });
  assert.match(r.title, /Action Added/);
  assert.match(r.description, /Its button number is \*\*1\*\*/);
  assert.match(r.description, /Ignored for this type: `emoji`/);
  const panel = await svc.getAutomationByName(GUILD, 'panel');
  r = await run(guild, { group: 'condition', sub: 'add', opts: { name: 'rules-panel', type: 'BUTTON_IS', button: `${panel.id}:1` } });
  assert.match(r.title, /Condition Added/);
  r = await run(guild, { group: 'condition', sub: 'add', opts: { name: 'rules-panel', type: 'HAS_NOT_ANY_ROLE', role } });
  assert.match(r.title, /Condition Added/);
  r = await run(guild, { group: 'condition', sub: 'add', opts: { name: 'rules-panel', type: 'has not any role', text: `<@&${ID(512)}>` } });
  assert.match(r.title, /Invalid Value/);
  fakeRole(guild, ID(512));
  r = await run(guild, { group: 'condition', sub: 'add', opts: { name: 'rules-panel', type: 'HAS_NOT_ANY_ROLE', text: `<@&${ID(512)}>` } });
  assert.match(r.title, /Condition Updated/);
  assert.match(r.description, /Condition 2 of \*\*rules-panel\*\* is now/);
  r = await run(guild, { group: 'condition', sub: 'add', opts: { name: 'rules-panel', type: 'HAS_NOT_ANY_ROLE', text: `<@&${ID(512)}>` } });
  assert.match(r.title, /Already There/);

  r = await run(guild, { sub: 'enable', opts: { name: 'rules-panel' } });
  assert.match(r.title, /Automation Enabled/);
  r = await run(guild, { sub: 'enable', opts: { name: 'rules-panel' } });
  assert.match(r.title, /No Change/);
  r = await run(guild, { sub: 'info', opts: { name: 'rules-panel' } });
  assert.match(r.description, /🟢 On/);
  assert.deepEqual(r.fields.map((f) => f.name), ['If (2)', 'Do (1)']);
  assert.match(r.fields[0].value, /1\. Clicked this button/);
  r = await run(guild, { sub: 'export', opts: { name: 'rules-panel' } });
  assert.match(r.title, /Exported/);
  const exported = JSON.parse(r.files[0].attachment.toString('utf8'));
  assert.equal(exported.name, 'rules-panel');
  assert.equal(exported.conditions[0].buttons[0].automationName, 'panel');
  assert.equal(svc.parseAutomationJson(JSON.stringify(exported)).ok, true);

  r = await run(guild, { sub: 'edit', opts: { name: 'rules-panel' } });
  assert.match(r.title, /Nothing To Change/);
  r = await run(guild, { sub: 'edit', opts: { name: 'rules-panel', cooldown: 30 } });
  assert.match(r.description, /Cooldown:\*\* 0 s → 30 s/);
  r = await run(guild, { sub: 'edit', opts: { name: 'rules-panel', rename: 'panel' } });
  assert.match(r.title, /Name Taken/);
  r = await run(guild, { sub: 'edit', opts: { name: 'rules-panel', rename: 'Rules Gate' } });
  assert.match(r.description, /rules-panel → rules-gate/);

  r = await run(guild, { group: 'condition', sub: 'remove', opts: { name: 'rules-gate', index: 5 } });
  assert.match(r.title, /No Such Condition/);
  assert.match(r.description, /has 2 condition\(s\)/);
  r = await run(guild, { group: 'condition', sub: 'remove', opts: { name: 'rules-gate', index: 2 } });
  assert.match(r.title, /Condition Removed/);
  r = await run(guild, { group: 'action', sub: 'remove', opts: { name: 'rules-gate', index: 1 } });
  assert.match(r.title, /Action Removed/);
  assert.match(r.description, /turned \*\*off\*\*/);
  r = await run(guild, { sub: 'disable', opts: { name: 'rules-gate' } });
  assert.match(r.title, /No Change/);

  // The button number survives removal: remove the panel's button action and add another one.
  r = await run(guild, { group: 'action', sub: 'remove', opts: { name: 'panel', index: 1 } });
  r = await run(guild, { group: 'action', sub: 'add', opts: { name: 'panel', type: 'SEND_MESSAGE', channel: text, text: 'Again', button_label: 'Accept' } });
  assert.match(r.description, /Its button number is \*\*2\*\*/);

  // A typed role mention the admin could not ping is refused at configuration time (it used to be a note on the success reply).
  const quietChannel = fakeChannel(guild, ID(530), { name: 'quiet', missing: ['MentionEveryone'] });
  r = await run(guild, { group: 'action', sub: 'add', opts: { name: 'panel', type: 'SEND_MESSAGE', channel: quietChannel, text: `Hey <@&${role.id}>` } });
  assert.match(r.title, /Role Not Pingable/);
  assert.match(r.description, /not mentionable and you lack/);
  const open = fakeRole(guild, ID(514), { mentionable: true });
  r = await run(guild, { group: 'action', sub: 'add', opts: { name: 'panel', type: 'SEND_MESSAGE', channel: quietChannel, text: `Hey <@&${open.id}>` } });
  assert.match(r.title, /Action Added/, 'a mentionable role is fine');
});

test('DB: /automation create presets validate their inputs; autocomplete answers', { skip: skipDb }, async () => {
  const guild = fakeGuild(GUILD);
  const text = fakeChannel(guild, ID(520), { name: 'welcome' });
  const voice = fakeChannel(guild, ID(521), { type: ChannelType.GuildForum, name: 'forum' });
  const role = fakeRole(guild, ID(522));
  const high = fakeRole(guild, ID(523), { position: 300 });
  fakeMember(guild, ID(524));
  await prisma.automation.deleteMany({ where: { guildId: GUILD } });

  let r = await run(guild, { sub: 'create', opts: { preset: 'welcomer', name: 'welcome' } });
  assert.match(r.title, /Channel Needed/);
  r = await run(guild, { sub: 'create', opts: { preset: 'welcomer', name: 'welcome', channel: voice } });
  assert.match(r.title, /Wrong Channel Type/);
  r = await run(guild, { sub: 'create', opts: { preset: 'welcomer', name: 'welcome', channel: text, role: high } });
  assert.match(r.title, /Role Not Allowed/);
  r = await run(guild, { sub: 'create', opts: { preset: 'welcomer', name: 'welcome', channel: text, role, message: 'Hey {user}', threshold: 4 } });
  assert.match(r.title, /Automation Created/);
  assert.match(r.description, /Ignored for this type: `threshold`/);
  assert.match(r.description, /\*\*If:\*\* always/);
  r = await run(guild, { sub: 'create', opts: { preset: 'starboard', name: 'stars', channel: text, emoji: 'banana', threshold: 2 } });
  assert.match(r.title, /Invalid Emoji/);
  r = await run(guild, { sub: 'create', opts: { preset: 'starboard', name: 'stars', channel: text, threshold: 2 } });
  assert.match(r.title, /Automation Created/);
  r = await run(guild, { sub: 'create', opts: { preset: 'thread-joiner', name: 'joiner' } });
  assert.match(r.title, /Member Needed/);
  r = await run(guild, { sub: 'create', opts: { preset: 'thread-joiner', name: 'joiner', user: { id: ID(524), bot: false } } });
  assert.match(r.title, /Not A Member/);
  r = await run(guild, { sub: 'create', opts: { preset: 'thread-joiner', name: 'joiner', user: { id: ID(524), bot: true }, __members: { user: { id: ID(524) } } } });
  assert.match(r.description, /Pick a person, not a bot/);
  r = await run(guild, { sub: 'create', opts: { preset: 'thread-joiner', name: 'joiner', user: { id: ID(524), bot: false }, __members: { user: { id: ID(524) } } } });
  assert.match(r.title, /Automation Created/);

  const complete = async (spec) => {
    const i = fakeInteraction(guild, spec);
    let answered = null;
    i.responded = false;
    i.respond = async (choices) => { answered = choices; };
    await cmd.autocomplete(i);
    return answered;
  };
  const names = await complete({ sub: 'info', opts: { __focused: { name: 'name', value: 'sta' } } });
  assert.deepEqual(names.map((c) => c.value), ['stars']);
  assert.match(names[0].name, /Reaction added/);
  const types = await complete({ group: 'condition', sub: 'add', opts: { name: 'stars', __focused: { name: 'type', value: 'role' } } });
  assert.ok(types.every((c) => /ROLE/.test(c.value) || /role/i.test(c.name)));
  assert.equal(types.some((c) => c.value === 'CONTAINS'), false);
  const actTypes = await complete({ group: 'action', sub: 'add', opts: { name: 'stars', __focused: { name: 'type', value: '' } } });
  assert.equal(actTypes.some((c) => c.value === 'DELETE'), true);
  assert.equal(actTypes.find((c) => c.value === 'GIVE_XP').name, 'Give XP (module off)');
  assert.equal(actTypes.some((c) => c.value === 'SEND_IN_THREAD'), true);   // REACTION_ADDED is a thread-capable trigger
  const idx = await complete({ group: 'condition', sub: 'remove', opts: { name: 'stars', __focused: { name: 'index', value: '' } } });
  assert.deepEqual(idx.map((c) => c.value), [1, 2, 3]);
  const noRow = await complete({ group: 'action', sub: 'remove', opts: { name: 'ghost', __focused: { name: 'index', value: '' } } });
  assert.deepEqual(noRow, []);
  const embedNames = await complete({ group: 'action', sub: 'add', opts: { __focused: { name: 'embed', value: 'x' } } });
  assert.ok(Array.isArray(embedNames));
});
