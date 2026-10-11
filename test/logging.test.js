// pkg-logging tests (pkg-logging.md §9): registry and pure helpers, embed builders and diffs, sendLog (dead-channel memo,
// batching, queue cap, permissions, files), the /logging payload, the plugin declaration and its hooks run against fake
// contexts, and (with TEST_DATABASE_URL) the DB cases: /logging end to end against a real settings row, the channelDelete
// cleanup and the bot-managed channel lookup.
// Offline by default. DB cases: TEST_DATABASE_URL='<migrated db>' node --test --test-concurrency=1 test/logging.test.js
import { test, after, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back (setNX gives null), and Postgres is only reached with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { Client, PermissionFlagsBits, PermissionsBitField, AttachmentBuilder, ChannelType, embedLength } = await import('discord.js');
const svc = await import('../src/services/loggingService.js');
const ev = await import('../src/services/logEvents.js');
const { plugin, INVITE_HINT_RE } = await import('../src/plugins/logging.js');
const loggingCommand = await import('../src/commands/admin/logging.js');
const { default: redis, cache } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { PERMISSION_LEVELS, LIMITS } = await import('../src/config/constants.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { updateSettings, getSettings } = await import('../src/services/settingsService.js');
const { markBotDeletion } = await import('../src/utils/deletionTracker.js');
const { neutralEmbed } = await import('../src/utils/embedBuilder.js');

const {
  LOG_TYPES, LOG_TYPE_LABELS, LOG_EVENTS, LOG_EVENT_KEYS, MESSAGE_LOG_EVENTS, QUEUE_MAX_ITEMS,
  isLogEventKey, isLogEventEnabled, toggleLogEvents, isIgnoredChannel, channelCleanupData, extractMentions,
  sendLog, normalizeLogOpts, getLogChannelHealth, forgetDeadChannels, markLogChannelDeleted, isLogChannelDead,
  flushLogQueue, fitEmbedData, _resetLoggingStateForTests,
} = svc;

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};
after(async () => {
  if (DB) await prisma.guildSettings.deleteMany({ where: { guildId: { startsWith: '95500700000000' } } }).catch(() => {});
  Object.assign(console, quiet);
  _resetLoggingStateForTests();
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});
afterEach(() => _resetLoggingStateForTests());

const MINUTE = 60_000;
const HOUR = 3_600_000;
const DAY = 86_400_000;
const hook = (event) => plugin.hooks.find((h) => h.event === event);
const perms = (...names) => new PermissionsBitField(names.map((n) => PermissionFlagsBits[n]));
const ALL_PERMS = ['ViewChannel', 'SendMessages', 'EmbedLinks', 'AttachFiles'];

// ── Fakes ────────────────────────────────────────────────────────────────────

function fakeUser(id = '111111111111111111', extra = {}) {
  return {
    id, bot: false, username: `user${id.slice(-3)}`, tag: `user${id.slice(-3)}`, discriminator: '0', globalName: null,
    createdTimestamp: Date.now() - 400 * DAY, createdAt: new Date(Date.now() - 400 * DAY),
    displayAvatarURL: () => `https://cdn.discordapp.com/avatars/${id}/a.png`, ...extra,
  };
}

function fakeGuild(id = '955007000000000001') {
  return { id, memberCount: 1234, vanityURLCode: null, members: { me: { id: 'bot', permissions: new PermissionsBitField([]), roles: { cache: new Map() } } }, channels: { cache: new Map(), fetch: async () => null }, voiceStates: { cache: new Map() } };
}

function fakeChannel(guild, id, flags = ALL_PERMS, extra = {}) {
  const ch = {
    id, guild, guildId: guild.id, type: ChannelType.GuildText, name: `chan-${id.slice(-3)}`, sent: [], _perms: perms(...flags),
    isTextBased: () => true, isThread: () => false, isVoiceBased: () => false,
    permissionsFor: () => ch._perms,
    send: async (payload) => { ch.sent.push(payload); },
    ...extra,
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function fakeMessage(o = {}) {
  const author = o.author === undefined ? fakeUser() : o.author;
  const created = o.createdTimestamp ?? Date.now() - 5 * MINUTE;
  return {
    id: o.id ?? '800000000000000001', partial: o.partial ?? false, content: 'content' in o ? o.content : 'hello',
    author, channelId: o.channelId ?? '700000000000000001', channel: o.channel ?? null, guild: o.guild ?? null, webhookId: null,
    createdTimestamp: created, createdAt: new Date(created), editedTimestamp: o.editedTimestamp ?? null,
    attachments: new Map((o.attachments ?? []).map((a, i) => [String(i), a])), embeds: o.embeds ?? [],
    url: 'https://discord.com/channels/1/2/3',
    mentions: {
      users: new Map((o.mentionUsers ?? []).map((u) => [u.id, u])),
      roles: new Map((o.mentionRoles ?? []).map((r) => [r.id, r])),
      everyone: Boolean(o.everyone),
    },
  };
}

function fakeMember(o = {}) {
  const id = o.id ?? '222222222222222222';
  const until = o.until ?? null;
  const user = fakeUser(id);
  return {
    id, user, guild: { id: o.guildId ?? '955007000000000001', memberCount: 50 }, nickname: o.nickname ?? null, avatar: o.avatar ?? null,
    partial: o.partial ?? false, joinedTimestamp: o.joinedTimestamp ?? null,
    roles: { cache: new Map((o.roles ?? []).map((r) => [r, { id: r, position: 1 }])) },
    avatarURL: () => (o.avatar ? `https://cdn.discordapp.com/guilds/1/users/${id}/avatars/${o.avatar}.png` : null),
    communicationDisabledUntilTimestamp: until, communicationDisabledUntil: until == null ? null : new Date(until),
    isCommunicationDisabled() { return until != null && until > Date.now(); },
  };
}

function fakeCtx(guild, settings, fields = {}) {
  return { client: { user: { id: 'bot' } }, guild, guildId: guild.id, state: Object.create(null), getSettings: async () => settings, ...fields };
}

function logSetup(flags = ALL_PERMS, { type = 'messageLogChannelId', channelId = '700000000000000010' } = {}) {
  const guild = fakeGuild();
  const channel = fakeChannel(guild, channelId, flags);
  const settings = { guildId: guild.id, loggingEnabled: true, modLogChannelId: null, disabledLogEvents: [], logIgnoredChannelIds: [], [type]: channel.id };
  return { guild, channel, settings };
}

const tick = (embed) => (embed.toJSON ? embed.toJSON() : embed);
const fieldOf = (embed, name) => tick(embed).fields?.find((f) => f.name === name);
const firstEmbed = (channel, n = 0) => channel.sent[n].embeds[0];

// ── 1. Registry ──────────────────────────────────────────────────────────────

test('LOG_EVENTS registry', () => {
  assert.equal(LOG_EVENT_KEYS.length, 32); // 28 of the spec table + userUpdate (architect amendment 1) + 3 thread events (fix round)
  assert.deepEqual(LOG_EVENT_KEYS, Object.keys(LOG_EVENTS));
  for (const key of LOG_EVENT_KEYS) {
    assert.match(key, /^[a-z][a-zA-Z]{2,31}$/);
    const e = LOG_EVENTS[key];
    assert.ok(Object.hasOwn(LOG_TYPES, e.type) && e.type !== 'MOD', `${key} type`);
    assert.ok(e.label.length <= 40, `${key} label`);
    assert.ok(e.emoji, `${key} emoji`);
    assert.ok(Object.isFrozen(e));
  }
  assert.ok(Object.isFrozen(LOG_EVENTS) && Object.isFrozen(LOG_TYPES) && Object.isFrozen(LOG_EVENT_KEYS));
  for (const key of MESSAGE_LOG_EVENTS) assert.equal(LOG_EVENTS[key].type, 'MESSAGE');
  assert.deepEqual([...MESSAGE_LOG_EVENTS], ['messageDelete', 'messageEdit', 'messageBulkDelete', 'invitePosted']);
  // Every key named in §6, plus userUpdate.
  for (const key of ['messageDelete', 'messageEdit', 'messageBulkDelete', 'invitePosted', 'memberRoles', 'memberNickname',
    'memberTimeout', 'memberAvatar', 'userUpdate', 'memberBan', 'memberUnban', 'memberJoin', 'memberLeave', 'voiceJoin', 'voiceLeave',
    'voiceMove', 'voiceState', 'channelCreate', 'channelUpdate', 'channelDelete', 'roleCreate', 'roleUpdate', 'roleDelete',
    'serverUpdate', 'emojiCreate', 'emojiUpdate', 'emojiDelete', 'inviteCreate', 'inviteDelete',
    'threadCreate', 'threadUpdate', 'threadDelete']) {
    assert.ok(isLogEventKey(key), key);
  }
  for (const key of ['threadCreate', 'threadUpdate', 'threadDelete']) assert.equal(LOG_EVENTS[key].type, 'SERVER', key);
  assert.equal(LOG_EVENTS.userUpdate.type, 'MEMBER');
  assert.equal(LOG_TYPE_LABELS.MOD, 'Moderation');
  assert.deepEqual(Object.keys(LOG_TYPES), ['MESSAGE', 'MEMBER', 'SERVER', 'VOICE', 'JOIN_LEAVE', 'MOD']);
});

test('isLogEventEnabled / isLogEventKey', () => {
  assert.equal(isLogEventEnabled(null, 'messageEdit'), true);
  assert.equal(isLogEventEnabled(undefined, 'messageEdit'), true);
  assert.equal(isLogEventEnabled({}, 'messageEdit'), true);
  assert.equal(isLogEventEnabled(null, 'nope'), false);
  assert.equal(isLogEventEnabled({ disabledLogEvents: ['messageEdit'] }, 'messageEdit'), false);
  assert.equal(isLogEventEnabled({ disabledLogEvents: ['messageEdit'] }, 'messageDelete'), true);
  assert.equal(isLogEventKey('toString'), false); // not an own key
  assert.equal(isLogEventKey(7), false);
});

test('toggleLogEvents', () => {
  const off = toggleLogEvents([], 'messageEdit', false);
  assert.deepEqual(off, ['messageEdit']);
  assert.deepEqual(toggleLogEvents(off, 'messageEdit', true), []);
  assert.deepEqual(toggleLogEvents([], 'all', false), [...LOG_EVENT_KEYS]);
  assert.equal(toggleLogEvents([], 'all', false).length, LOG_EVENT_KEYS.length);
  assert.deepEqual(toggleLogEvents(['messageEdit', 'junk'], 'all', true), []);
  // Junk and duplicates are stripped, and the result follows the registry order.
  assert.deepEqual(toggleLogEvents(['junk', 'roleDelete', 'roleDelete', 'messageEdit'], 'memberBan', false), ['messageEdit', 'memberBan', 'roleDelete']);
  assert.deepEqual(toggleLogEvents(['junk'], '', true), []);
  assert.deepEqual(toggleLogEvents(null, 'memberBan', false), ['memberBan']);
  assert.deepEqual(toggleLogEvents(['memberBan'], 'memberBan', false), ['memberBan']); // already off: no duplicate
});

test('isIgnoredChannel', () => {
  const s = { logIgnoredChannelIds: ['C1', 'CAT'] };
  assert.equal(isIgnoredChannel(s, { id: 'C1' }), true);
  assert.equal(isIgnoredChannel(s, { id: 'T1', parentId: 'C1' }), true); // a thread of an ignored channel
  assert.equal(isIgnoredChannel(s, { id: 'T1', parentId: 'C9', parent: { parentId: 'CAT' } }), true); // thread -> channel -> category
  assert.equal(isIgnoredChannel(s, { id: 'C2', parentId: 'CAT' }), true); // a channel under an ignored category
  assert.equal(isIgnoredChannel(s, { id: 'C2', parentId: 'C3' }), false);
  assert.equal(isIgnoredChannel(s, null), false);
  assert.equal(isIgnoredChannel({ logIgnoredChannelIds: [] }, { id: 'C1' }), false);
  assert.equal(isIgnoredChannel(null, { id: 'C1' }), false);
});

test('channelCleanupData', () => {
  assert.deepEqual(channelCleanupData({ serverLogChannelId: 'X', modLogChannelId: 'X', messageLogChannelId: 'Y' }, 'X'), { serverLogChannelId: null, modLogChannelId: null });
  assert.deepEqual(channelCleanupData({ logIgnoredChannelIds: ['A', 'X', 'B'] }, 'X'), { logIgnoredChannelIds: ['A', 'B'] });
  assert.deepEqual(channelCleanupData({ voiceLogChannelId: 'X', logIgnoredChannelIds: ['X'] }, 'X'), { voiceLogChannelId: null, logIgnoredChannelIds: [] });
  assert.equal(channelCleanupData({ serverLogChannelId: 'Y', logIgnoredChannelIds: ['A'] }, 'X'), null);
  assert.equal(channelCleanupData(null, 'X'), null);
});

test('findInviteCodes and the hook filter regex', () => {
  assert.deepEqual(ev.findInviteCodes('join discord.gg/abc and https://discord.com/invite/Xy-9 and discordapp.com/invite/abc'), ['abc', 'Xy-9']);
  assert.deepEqual(ev.findInviteCodes('discord.gg/'), []);
  assert.deepEqual(ev.findInviteCodes('discord.gg/a'), []); // codes are at least 2 characters
  const many = Array.from({ length: 12 }, (_, i) => `discord.gg/code${i}`).join(' ');
  assert.equal(ev.findInviteCodes(many).length, 10);
  assert.deepEqual(ev.findInviteCodes(undefined), []);
  assert.equal(INVITE_HINT_RE.test('hello'), false);
  assert.equal(INVITE_HINT_RE.test('see DISCORD.GG/abc'), true);
  assert.equal(INVITE_HINT_RE.test('https://discord.com/invite/abc'), true);
});

test('extractMentions skips bots, the excluded user and partials', () => {
  const alice = fakeUser('111111111111111111');
  const bob = fakeUser('222222222222222222');
  const robot = fakeUser('333333333333333333', { bot: true });
  const m = fakeMessage({ author: alice, mentionUsers: [alice, bob, robot], mentionRoles: [{ id: '444444444444444444', name: 'Mods' }], everyone: true });
  assert.deepEqual(extractMentions(m), [`<@${alice.id}> (${alice.tag})`, `<@${bob.id}> (${bob.tag})`, '<@&444444444444444444> (Mods)', '`@everyone` / `@here`']);
  assert.deepEqual(extractMentions(m, { excludeUserId: alice.id }).length, 3);
  assert.deepEqual(extractMentions({ partial: true }), []);
  assert.deepEqual(extractMentions(null), []);
});

// ── 2. Ghost pings ───────────────────────────────────────────────────────────

test('isGhostPing (R118/R267)', () => {
  const role = { id: '444444444444444444', name: 'Mods' };
  const now = Date.now();
  const base = { author: fakeUser('111111111111111111'), mentionRoles: [role], createdTimestamp: now - 5 * MINUTE };
  assert.equal(ev.isGhostPing({ message: fakeMessage(base), now }).ghost, true);
  assert.equal(ev.isGhostPing({ message: fakeMessage(base), now }).mentions.length, 1);
  assert.equal(ev.isGhostPing({ message: fakeMessage(base), botReason: 'automod', now }).ghost, false);
  assert.equal(ev.isGhostPing({ message: fakeMessage({ ...base, createdTimestamp: now - 2 * HOUR }), now }).ghost, false);
  assert.equal(ev.isGhostPing({ message: fakeMessage({ ...base, createdTimestamp: undefined }), now }).ghost, true); // default is 5 min old
  const self = fakeUser('111111111111111111');
  assert.equal(ev.isGhostPing({ message: fakeMessage({ author: self, mentionUsers: [self], createdTimestamp: now - MINUTE }), now }).ghost, false);
  const robot = fakeUser('333333333333333333', { bot: true });
  assert.equal(ev.isGhostPing({ message: fakeMessage({ mentionUsers: [robot], createdTimestamp: now - MINUTE }), now }).ghost, false);
  assert.equal(ev.isGhostPing({ message: fakeMessage({ ...base, partial: true }), now }).ghost, false);
  assert.deepEqual(ev.isGhostPing({ message: { partial: true }, now }), { ghost: false, mentions: [] });
});

// ── 3. Member updates ────────────────────────────────────────────────────────

test('diffMemberUpdate: a partial old member never produces fake role / nickname diffs (R033/R111)', () => {
  const oldMember = fakeMember({ roles: [], nickname: null, partial: true });
  const newMember = fakeMember({ roles: ['a', 'b'], nickname: 'Ally', avatar: 'hash' });
  const d = ev.diffMemberUpdate({ oldMember, newMember, oldKnown: false });
  assert.equal(d.roles, null);
  assert.equal(d.nickname, null);
  assert.equal(d.avatar, null);
  assert.equal(d.timeout, null);
});

test('diffMemberUpdate: timeout table (R112/R261)', () => {
  const now = Date.now();
  const past = now - HOUR;
  const future = now + HOUR;
  const run = (oldUntil, newUntil, oldKnown = true) => ev.diffMemberUpdate({
    oldMember: fakeMember({ until: oldUntil }), newMember: fakeMember({ until: newUntil }), oldKnown,
  }).timeout;

  const added = run(past, future);
  assert.equal(added.kind, 'added'); // the stale past timestamp does not hide a re-timeout
  assert.equal(added.until.getTime(), future);
  assert.equal(run(null, future).kind, 'added');
  assert.deepEqual(run(future, null), { kind: 'removed' });
  assert.equal(run(past, null), null); // a past timestamp that is merely cleared is not a lifted timeout
  assert.equal(run(past, past + 1), null);
  assert.equal(run(null, null), null);
  const changed = run(future, future + HOUR);
  assert.equal(changed.kind, 'changed');
  assert.equal(changed.from.getTime(), future);
  assert.equal(changed.until.getTime(), future + HOUR);
  assert.equal(run(future, future), null);
  const unknown = run(null, future, false);
  assert.equal(unknown.kind, 'active-unknown');
  assert.equal(run(null, past, false), null);
});

test('diffMemberUpdate: roles, nickname, avatar when the old member is known', () => {
  const guildId = '955007000000000001';
  const oldMember = fakeMember({ roles: [guildId, 'r1', 'r2'], nickname: 'Old', avatar: null });
  const newMember = fakeMember({ roles: [guildId, 'r2', 'r3', 'r4'], nickname: null, avatar: 'abc' });
  const d = ev.diffMemberUpdate({ oldMember, newMember, oldKnown: true });
  assert.deepEqual(d.roles, { added: ['r3', 'r4'], removed: ['r1'] });
  assert.deepEqual(d.nickname, { before: 'Old', after: null });
  assert.equal(d.avatar.before, null);
  assert.match(d.avatar.after, /avatars\/abc\.png$/);
  assert.equal(ev.diffMemberUpdate({ oldMember, newMember: oldMember, oldKnown: true }).roles, null);
  // @everyone is never reported as added.
  const everyoneOnly = ev.diffMemberUpdate({ oldMember: fakeMember({ roles: [] }), newMember: fakeMember({ roles: [guildId] }), oldKnown: true });
  assert.equal(everyoneOnly.roles, null);
});

test('buildMemberRolesEmbed: 60 added roles stay within one field without cutting a mention (R260)', () => {
  const ids = Array.from({ length: 60 }, (_, i) => `${100000000000000000n + BigInt(i)}`);
  const embed = ev.buildMemberRolesEmbed({ member: fakeMember(), roles: { added: ids, removed: [] } });
  const value = fieldOf(embed, '✅ Added').value;
  assert.ok(value.length <= 1024);
  const parts = value.split(', ');
  assert.match(parts.at(-1), /^… and \d+ more$/);
  for (const p of parts.slice(0, -1)) assert.match(p, /^<@&\d{18}>$/);
});

test('member embeds: nickname, timeout kinds, avatar', () => {
  const member = fakeMember({ id: '222222222222222222' });
  const nick = tick(ev.buildNicknameEmbed({ member, nickname: { before: null, after: 'Ally' } }));
  assert.equal(nick.title, '📝 Nickname Changed');
  assert.match(nick.fields.find((f) => f.name === 'Before').value, /\(no nickname\)/);
  assert.equal(nick.fields.find((f) => f.name === 'After').value, 'Ally');

  const until = new Date(Date.now() + HOUR);
  const added = tick(ev.buildTimeoutEmbed({ member, timeout: { kind: 'added', until } }));
  assert.equal(added.title, '⏱️ Member Timed Out');
  assert.match(added.fields.find((f) => f.name === 'Until').value, /<t:\d+:f> \(<t:\d+:R>\)/);
  const removed = tick(ev.buildTimeoutEmbed({ member, timeout: { kind: 'removed' } }));
  assert.equal(removed.title, '🔊 Timeout Removed');
  assert.equal(removed.description, 'The timeout was lifted before it expired.');
  const changed = tick(ev.buildTimeoutEmbed({ member, timeout: { kind: 'changed', from: until, until } }));
  assert.equal(changed.title, '⏱️ Timeout Changed');
  assert.ok(changed.fields.some((f) => f.name === 'Before') && changed.fields.some((f) => f.name === 'After'));
  const unknown = tick(ev.buildTimeoutEmbed({ member, timeout: { kind: 'active-unknown', until } }));
  assert.match(unknown.footer.text, /previous state not cached$/);

  const avatar = tick(ev.buildAvatarEmbed({ member, avatar: { before: null, after: 'https://cdn.discordapp.com/x.png' } }));
  assert.equal(avatar.title, '🖼️ Server Avatar Changed');
  assert.equal(avatar.thumbnail.url, 'https://cdn.discordapp.com/x.png');
  assert.equal(avatar.fields.find((f) => f.name === 'Before').value, '(none)');
});

test('diffUser / buildUserUpdateEmbed', () => {
  const a = fakeUser('111111111111111111', { username: 'old', globalName: 'Old Name' });
  const b = fakeUser('111111111111111111', { username: 'new', globalName: 'Old Name', displayAvatarURL: () => 'https://cdn.discordapp.com/new.png' });
  const d = ev.diffUser(a, b);
  assert.deepEqual(d.username, { before: 'old', after: 'new' });
  assert.equal(d.displayName, null);
  assert.ok(d.avatar);
  const embed = tick(ev.buildUserUpdateEmbed({ user: b, diff: d }));
  assert.equal(embed.thumbnail.url, 'https://cdn.discordapp.com/new.png');
  assert.ok(embed.fields.some((f) => f.name === 'Username'));
  assert.ok(embed.fields.some((f) => f.value === 'Avatar changed'));
  assert.equal(ev.buildUserUpdateEmbed({ user: a, diff: ev.diffUser(a, a) }), null);
});

// ── 4. Message embeds ────────────────────────────────────────────────────────

test('buildMessageEditEmbed: Before is Not cached only for a partial old message (R268)', () => {
  const author = fakeUser();
  const newMessage = fakeMessage({ author, content: 'after text', editedTimestamp: Date.now() });
  const partial = ev.buildMessageEditEmbed({ oldMessage: { partial: true, content: null }, newMessage, oldKnown: false });
  assert.equal(fieldOf(partial, 'Before').value, '*Not cached*');
  assert.equal(fieldOf(partial, 'After').value, 'after text');
  const empty = ev.buildMessageEditEmbed({ oldMessage: fakeMessage({ author, content: '' }), newMessage, oldKnown: true });
  assert.equal(fieldOf(empty, 'Before').value, '*[No text content]*');
  const text = ev.buildMessageEditEmbed({ oldMessage: fakeMessage({ author, content: 'before text' }), newMessage, oldKnown: true });
  assert.equal(fieldOf(text, 'Before').value, 'before text');
  assert.equal(tick(text).title, '✏️ Message Edited');
});

test('buildMessageEditEmbed: a removed mention on a fresh message is a ghost ping', () => {
  const author = fakeUser('111111111111111111');
  const victim = fakeUser('222222222222222222');
  const oldMessage = fakeMessage({ author, content: 'hey <@222222222222222222>', mentionUsers: [victim] });
  const newMessage = fakeMessage({ author, content: 'hey', editedTimestamp: Date.now() });
  const embed = ev.buildMessageEditEmbed({ oldMessage, newMessage, oldKnown: true });
  assert.match(tick(embed).title, /Ghost Ping Detected — Message Edited/);
  assert.match(fieldOf(embed, '⚠️ Removed Mentions').value, /<@222222222222222222>/);
  // An hour-old message is not a ghost ping.
  const old = fakeMessage({ author, content: 'hey', createdTimestamp: Date.now() - 2 * HOUR, editedTimestamp: Date.now() });
  assert.equal(tick(ev.buildMessageEditEmbed({ oldMessage: fakeMessage({ author, mentionUsers: [victim], createdTimestamp: Date.now() - 2 * HOUR }), newMessage: old, oldKnown: true })).title, '✏️ Message Edited');
});

test('buildMessageDeleteEmbed: partial, plain, attachments, bot reason, limits', () => {
  const partial = tick(ev.buildMessageDeleteEmbed({ message: { id: '8', partial: true, channelId: '7' } }));
  assert.equal(partial.title, '🗑️ Message Deleted');
  assert.match(partial.description, /not cached/);
  assert.equal(partial.footer.text, 'Msg ID: 8');
  assert.equal(partial.author, undefined);

  const withFiles = ev.buildMessageDeleteEmbed({ message: fakeMessage({ content: '', attachments: [{ name: 'cat.png', size: 2_500_000 }, { name: 'a.txt', size: 100 }] }) });
  assert.equal(tick(withFiles).description, '*[No text content]*');
  assert.equal(fieldOf(withFiles, 'Attachments (2)').value, 'cat.png (2.4 MB)\na.txt (100 B)');

  const bot = tick(ev.buildMessageDeleteEmbed({ message: fakeMessage({ mentionRoles: [{ id: '444444444444444444', name: 'Mods' }] }), botReason: 'automod' }));
  assert.equal(bot.title, '🗑️ Message Deleted (removed by the bot: automod)');
  assert.equal(bot.fields.some((f) => f.name === '⚠️ Ghost Pinged'), false);

  const huge = ev.buildMessageDeleteEmbed({ message: fakeMessage({ content: 'x'.repeat(10_000), attachments: Array.from({ length: 200 }, (_, i) => ({ name: `file-${i}-${'n'.repeat(80)}.png`, size: 1 })) }) });
  assert.ok(tick(huge).description.length <= 4096);
  assert.ok(fieldOf(huge, 'Attachments (200)').value.length <= 1024);
});

test('buildBulkDeleteTranscript: sorted, partial lines, capped (R266)', () => {
  const alice = fakeUser('111111111111111111');
  const t = Date.UTC(2026, 0, 1, 12, 0, 0);
  const msgs = new Map([
    ['3', fakeMessage({ id: '3', author: alice, content: 'third\nline', createdTimestamp: t + 2000, attachments: [{ url: 'https://cdn/x.png' }] })],
    ['p', { id: 'p', partial: true }],
    ['1', fakeMessage({ id: '1', author: alice, content: 'first', createdTimestamp: t })],
    ['2', fakeMessage({ id: '2', author: fakeUser('333333333333333333', { bot: true }), content: 'second', createdTimestamp: t + 1000 })],
  ]);
  const lines = ev.buildBulkDeleteTranscript(msgs, { name: 'general' }).split('\n');
  assert.equal(lines[0], 'Bulk delete — 4 messages — channel general');
  assert.match(lines[1], /^\[2026-01-01T12:00:00\.000Z\] user111 \(111111111111111111\): first$/);
  assert.match(lines[2], /\[BOT\]: second$/);
  assert.match(lines[3], /third ⏎ line \[attachments: https:\/\/cdn\/x\.png\]$/);
  assert.equal(lines[4], '[unknown time] (not cached) message p');

  const big = new Map();
  for (let i = 0; i < 20_000; i++) big.set(String(i), fakeMessage({ id: String(i), content: 'y'.repeat(100), createdTimestamp: t + i }));
  const text = ev.buildBulkDeleteTranscript(big, { name: 'x' });
  assert.ok(text.length <= 1_000_100, `length ${text.length}`);
  assert.match(text, /truncated \(\d+ more messages\)$/);
});

test('buildBulkDeleteEmbed: counts, authors and the file', () => {
  const alice = fakeUser('111111111111111111');
  const bob = fakeUser('222222222222222222');
  const msgs = new Map([
    ['1', fakeMessage({ id: '1', author: alice })], ['2', fakeMessage({ id: '2', author: alice })], ['3', fakeMessage({ id: '3', author: bob })],
    ['4', { id: '4', partial: true }],
  ]);
  const { embed, file } = ev.buildBulkDeleteEmbed({ channel: { id: '700000000000000001', name: 'general' }, messages: msgs, botReason: '/purge by x (1)' });
  const e = tick(embed);
  assert.match(e.description, /^\*\*4\*\* messages were deleted in <#700000000000000001> \(by the bot: \/purge by x \(1\)\)\.$/);
  assert.equal(fieldOf(embed, 'Cached').value, '3 of 4 messages had cached content.');
  assert.equal(fieldOf(embed, 'Authors').value, '<@111111111111111111> ×2, <@222222222222222222> ×1');
  assert.equal(fieldOf(embed, 'Transcript').value, 'Attached as a text file.');
  assert.ok(file instanceof AttachmentBuilder);
  assert.match(file.name, /^bulk-delete-700000000000000001-\d+\.txt$/);

  const none = ev.buildBulkDeleteEmbed({ channel: { id: '7' }, messages: new Map([['1', { id: '1', partial: true }]]) });
  assert.equal(none.file, null);
  assert.equal(fieldOf(none.embed, 'Transcript'), undefined);
});

test('buildInvitePostedEmbed marks the vanity URL', () => {
  const embed = ev.buildInvitePostedEmbed({ message: fakeMessage({ content: 'discord.gg/Mine and discord.gg/other' }), codes: ['Mine', 'other'], vanityCode: 'mine' });
  assert.equal(fieldOf(embed, 'Invites').value, "discord.gg/Mine (this server's vanity URL)\ndiscord.gg/other");
  assert.equal(tick(embed).title, '📨 Invite Posted');
});

// ── 5. Join / leave / ban / voice ────────────────────────────────────────────

test('buildMemberJoinEmbed: invite field has three states (amendment 4)', () => {
  const member = fakeMember({ id: '222222222222222222' });
  assert.equal(fieldOf(ev.buildMemberJoinEmbed({ member, invite: undefined }), 'Invite'), undefined); // invites module off
  assert.equal(fieldOf(ev.buildMemberJoinEmbed({ member, invite: null }), 'Invite').value, 'Unknown');
  assert.equal(fieldOf(ev.buildMemberJoinEmbed({ member, invite: { code: 'x', inviterId: null, vanity: true } }), 'Invite').value, 'Vanity URL');
  assert.equal(fieldOf(ev.buildMemberJoinEmbed({ member, invite: { code: 'abc', inviterId: '999999999999999999', vanity: false } }), 'Invite').value, 'discord.gg/abc by <@999999999999999999>');
  assert.equal(fieldOf(ev.buildMemberJoinEmbed({ member, invite: { code: 'abc', inviterId: null } }), 'Invite').value, 'discord.gg/abc');
  assert.equal(fieldOf(ev.buildMemberJoinEmbed({ member }), 'Member Count').value, '#50');
});

test('buildMemberJoinEmbed: new accounts and bots', () => {
  const fresh = fakeMember({ id: '222222222222222222' });
  fresh.user.createdTimestamp = Date.now() - 2 * DAY;
  fresh.user.createdAt = new Date(fresh.user.createdTimestamp);
  const e = tick(ev.buildMemberJoinEmbed({ member: fresh }));
  assert.equal(e.title, '⚠️ New Account Joined');
  assert.match(e.description, /less than 7 days ago/);
  const bot = fakeMember({ id: '333333333333333333' });
  bot.user.bot = true;
  bot.user.createdTimestamp = Date.now() - DAY;
  assert.equal(tick(ev.buildMemberJoinEmbed({ member: bot })).title, '🤖 Bot Added');
  assert.equal(tick(ev.buildMemberJoinEmbed({ member: fakeMember() })).title, '📥 Member Joined');
});

test('buildMemberLeaveEmbed: partial members and role lists', () => {
  const full = fakeMember({ roles: ['955007000000000001', 'r1', 'r2'], joinedTimestamp: Date.now() - 3 * DAY - 2 * HOUR });
  const e = ev.buildMemberLeaveEmbed({ member: full });
  assert.equal(fieldOf(e, 'Time in Server').value, '3d 2h');
  assert.equal(fieldOf(e, 'Roles').value, '<@&r1>, <@&r2>');
  const partial = fakeMember({ partial: true, roles: ['r1'] });
  const p = ev.buildMemberLeaveEmbed({ member: partial });
  assert.equal(fieldOf(p, 'Roles'), undefined);
  assert.equal(fieldOf(p, 'Time in Server').value, 'Unknown');
  assert.equal(ev.formatTimeInServer(30_000), 'Just joined');
  assert.equal(ev.formatTimeInServer(5 * MINUTE), '5m');
});

test('buildBanEmbed', () => {
  const user = fakeUser('222222222222222222');
  const ban = ev.buildBanEmbed({ user, executorId: 'bot', reason: 'spam', botId: 'bot' });
  assert.equal(tick(ban).title, '🔨 Member Banned');
  assert.equal(fieldOf(ban, 'By').value, '<@bot> (via bot command)');
  assert.equal(fieldOf(ban, 'Reason').value, 'spam');
  const unban = ev.buildBanEmbed({ user, unban: true });
  assert.equal(tick(unban).title, '✅ Member Unbanned');
  assert.equal(fieldOf(unban, 'By').value, 'Unknown');
  assert.equal(fieldOf(unban, 'Reason').value, 'No reason given');
  assert.ok(fieldOf(ev.buildBanEmbed({ user, reason: 'r'.repeat(3000) }), 'Reason').value.length <= 1024);
});

test('classifyVoice classifies by channel ids (R269)', () => {
  assert.deepEqual(
    ev.classifyVoice({ channelId: 'A', channel: null }, { channelId: null }),
    { kind: 'leave', oldId: 'A', newId: null, changes: [] },
  );
  assert.equal(ev.classifyVoice({ channelId: null }, { channelId: 'B' }).kind, 'join');
  assert.equal(ev.classifyVoice({ channelId: 'A' }, { channelId: 'B' }).kind, 'move');
  const state = ev.classifyVoice({ channelId: 'A', selfMute: false }, { channelId: 'A', selfMute: true });
  assert.equal(state.kind, 'state');
  assert.deepEqual(state.changes, ['Self-muted']);
  assert.equal(ev.classifyVoice({ channelId: 'A', selfMute: true }, { channelId: 'A', selfMute: true }).kind, null);
  assert.equal(ev.classifyVoice({ channelId: null }, { channelId: null }).kind, null);
  const many = ev.classifyVoice(
    { channelId: 'A', serverMute: false, serverDeaf: true, selfDeaf: false, selfMute: true, streaming: false },
    { channelId: 'A', serverMute: true, serverDeaf: false, selfDeaf: true, selfMute: false, streaming: true },
  );
  assert.deepEqual(many.changes, ['Server muted', 'Server undeafened', 'Self-deafened', 'Self-unmuted', 'Started streaming']);
});

test('buildVoiceEmbed renders deleted channels from ids', () => {
  const guild = fakeGuild();
  fakeChannel(guild, '700000000000000002');
  const member = fakeMember({ id: '222222222222222222' });
  const leave = tick(ev.buildVoiceEmbed({ member, guild, c: ev.classifyVoice({ channelId: '700000000000000001' }, { channelId: null }) }));
  assert.equal(leave.title, '🔇 Left Voice');
  assert.equal(leave.description, '<@222222222222222222> left <#700000000000000001> (deleted channel)');
  const move = tick(ev.buildVoiceEmbed({ member, guild, c: ev.classifyVoice({ channelId: '700000000000000002' }, { channelId: '700000000000000003' }) }));
  assert.equal(move.description, '<@222222222222222222> moved from <#700000000000000002> → <#700000000000000003> (deleted channel)');
  assert.equal(ev.buildVoiceEmbed({ member, guild, c: { kind: null } }), null);
});

// ── 6. Channel / role / guild / emoji / invite ───────────────────────────────

const overwrite = (id, allow, deny, type = 0) => ({ id, type, allow: new PermissionsBitField(allow), deny: new PermissionsBitField(deny) });
const chan = (o = {}) => ({
  id: '700000000000000001', name: 'general', type: ChannelType.GuildText, topic: null, nsfw: false, rateLimitPerUser: 0, parentId: null, position: 1,
  permissionOverwrites: { cache: new Map() }, ...o,
});

test('diffChannel: position-only changes produce no lines', () => {
  assert.deepEqual(ev.diffChannel(chan({ position: 1 }), chan({ position: 7 })), []);
});

test('diffChannel: name, topic, slowmode, nsfw, category, voice settings', () => {
  const lines = ev.diffChannel(
    chan({ topic: 'a', rateLimitPerUser: 0, parentId: 'c1', parent: { name: 'Old cat' } }),
    chan({ name: 'new-name', topic: 'b', nsfw: true, rateLimitPerUser: 3600, parentId: 'c2', parent: { name: 'New cat' } }),
  );
  const get = (n) => lines.find((l) => l.name === n)?.value;
  assert.equal(get('Name'), '`general` → `new-name`');
  assert.match(get('Topic'), /\*\*Before:\*\* a\n\*\*After:\*\* b/);
  assert.equal(get('NSFW'), 'Yes');
  assert.equal(get('Slowmode'), 'off → 1 hour');
  assert.equal(get('Category'), 'Old cat → New cat');
  const voice = ev.diffChannel(chan({ bitrate: 64000, userLimit: 0, rtcRegion: null }), chan({ bitrate: 96000, userLimit: 5, rtcRegion: 'us-east' }));
  const v = (n) => voice.find((l) => l.name === n)?.value;
  assert.equal(v('Bitrate'), '64 kbps → 96 kbps');
  assert.equal(v('User Limit'), 'unlimited → 5');
  assert.equal(v('Region'), 'automatic → us-east');
  assert.equal(ev.diffChannel(chan({ rateLimitPerUser: 10 }), chan({ rateLimitPerUser: 0 })).find((l) => l.name === 'Slowmode').value, '10 seconds → off');
});

test('diffChannel: permission overwrites', () => {
  const view = PermissionFlagsBits.ViewChannel;
  const send = PermissionFlagsBits.SendMessages;
  const attach = PermissionFlagsBits.AttachFiles;
  const embed = PermissionFlagsBits.EmbedLinks;
  const oldC = chan({ permissionOverwrites: { cache: new Map([
    ['r1', overwrite('r1', embed, attach)],      // changes
    ['r2', overwrite('r2', view, 0n)],           // removed
    ['r3', overwrite('r3', view, 0n)],           // unchanged
  ]) } });
  const newC = chan({ permissionOverwrites: { cache: new Map([
    ['r1', overwrite('r1', view | send, attach | embed)], // +view +send allowed, embed allow->deny, attach stays denied
    ['r3', overwrite('r3', view, 0n)],
    ['u9', overwrite('u9', view, send, 1)],      // added member overwrite
  ]) } });
  const value = ev.diffChannel(oldC, newC).find((l) => l.name === 'Permissions').value;
  const lines = value.split('\n');
  assert.equal(lines.length, 3);
  assert.ok(lines.includes('<@&r1>: ✅ View Channel, Send Messages · ❌ Embed Links'));
  assert.ok(lines.includes('<@&r2>: removed'));
  assert.ok(lines.includes('<@u9>: added · ✅ View Channel · ❌ Send Messages'));
  // A reset to neutral is shown with the revert marker.
  const reset = ev.diffChannel(
    chan({ permissionOverwrites: { cache: new Map([['r1', overwrite('r1', embed, 0n)]]) } }),
    chan({ permissionOverwrites: { cache: new Map([['r1', overwrite('r1', 0n, 0n)]]) } }),
  ).find((l) => l.name === 'Permissions').value;
  assert.equal(reset, '<@&r1>: ↩️ Embed Links');
});

test('channel embeds', () => {
  const lines = ev.diffChannel(chan(), chan({ name: 'renamed' }));
  const update = tick(ev.buildChannelUpdateEmbed({ channel: chan({ name: 'renamed' }), lines }));
  assert.equal(update.title, '🛠️ Channel Updated');
  assert.equal(update.description, '<#700000000000000001> `renamed`');
  assert.equal(ev.buildChannelUpdateEmbed({ channel: chan(), lines: [] }), null);

  const created = ev.buildChannelCreateEmbed({ channel: chan({ topic: 'hello', parent: { name: 'Cat' } }) });
  assert.equal(fieldOf(created, 'Type').value, 'Text Channel');
  assert.equal(fieldOf(created, 'Category').value, 'Cat');
  assert.equal(fieldOf(created, 'Topic').value, 'hello');

  const deleted = ev.buildChannelDeleteEmbed({ channel: chan({ name: 'gone' }), occupants: ['u1', 'u2'], botReason: 'tempvoice' });
  assert.equal(fieldOf(deleted, 'Connected Members (2)').value, '<@u1>, <@u2>');
  assert.equal(fieldOf(deleted, 'Deleted By').value, 'the bot (tempvoice)');
  assert.equal(fieldOf(deleted, 'Name').value, '`gone`');
});

const thread = (o = {}) => ({
  id: '700000000000000777', name: 'bug reports', type: ChannelType.PublicThread, parentId: '700000000000000001', ownerId: '222222222222222222',
  autoArchiveDuration: 1440, rateLimitPerUser: 0, locked: false, archived: false, invitable: null, appliedTags: [], ...o,
});

test('thread diff: logs name, lock, slowmode, auto-archive, invitable and tags; archiving alone is silent', () => {
  assert.deepEqual(ev.diffThread(thread(), thread()), []);
  assert.deepEqual(ev.diffThread(thread({ archived: false }), thread({ archived: true })), [], 'auto-archive noise is not logged');
  const lines = ev.diffThread(
    thread({ invitable: false, appliedTags: ['t1'] }),
    thread({
      name: 'solved', locked: true, rateLimitPerUser: 30, autoArchiveDuration: 10080, invitable: true, appliedTags: ['t2'],
      parent: { availableTags: [{ id: 't2', name: 'Solved' }] },
    }),
  );
  const get = (n) => lines.find((l) => l.name === n)?.value;
  assert.equal(get('Name'), '`bug reports` → `solved`');
  assert.equal(get('Locked'), 'Locked');
  assert.equal(get('Slowmode'), 'off → 30 seconds');
  assert.equal(get('Auto-archive'), '24 hours → 1 week');
  assert.equal(get('Invitable'), 'Yes');
  assert.equal(get('Tags'), '+ Solved\n- t1');
  assert.equal(ev.diffThread(thread({ locked: true }), thread({ locked: false })).find((l) => l.name === 'Locked').value, 'Unlocked');
  // Unknown (partial) fields are never compared.
  assert.deepEqual(ev.diffThread({ id: '1' }, { id: '1', name: 'x', locked: true }), []);
});

test('thread embeds', () => {
  const created = ev.buildThreadCreateEmbed({ thread: thread({ rateLimitPerUser: 60 }) });
  assert.equal(tick(created).title, '🧵 Thread Created');
  assert.equal(fieldOf(created, 'Thread').value, '<#700000000000000777> `bug reports`');
  assert.equal(fieldOf(created, 'Type').value, 'Public Thread');
  assert.equal(fieldOf(created, 'Channel').value, '<#700000000000000001>');
  assert.equal(fieldOf(created, 'Started By').value, '<@222222222222222222>');
  assert.equal(fieldOf(created, 'Auto-archive').value, '24 hours');
  assert.equal(fieldOf(created, 'Slowmode').value, '1 minute');
  assert.equal(tick(created).footer.text, 'Thread ID: 700000000000000777');
  // A thread with no known owner or parent still renders.
  const bare = ev.buildThreadCreateEmbed({ thread: { id: '9', type: ChannelType.PrivateThread } });
  assert.equal(fieldOf(bare, 'Type').value, 'Private Thread');
  assert.equal(fieldOf(bare, 'Started By'), undefined);

  const lines = ev.diffThread(thread(), thread({ name: 'renamed' }));
  const updated = ev.buildThreadUpdateEmbed({ thread: thread({ name: 'renamed' }), lines });
  assert.equal(tick(updated).title, '🧵 Thread Updated');
  assert.equal(tick(updated).description, '<#700000000000000777> `renamed`');
  assert.equal(ev.buildThreadUpdateEmbed({ thread: thread(), lines: [] }), null);

  const deleted = ev.buildThreadDeleteEmbed({ thread: thread({ guild: { channels: { cache: new Map([['700000000000000001', {}]]) } } }), botReason: 'automation' });
  assert.equal(tick(deleted).title, '🗑️ Thread Deleted');
  assert.equal(fieldOf(deleted, 'Name').value, '`bug reports`');
  assert.equal(fieldOf(deleted, 'Channel').value, '<#700000000000000001>');
  assert.equal(fieldOf(deleted, 'Deleted By').value, 'the bot (automation)');
  const nameless = ev.buildThreadDeleteEmbed({ thread: { id: '9', type: ChannelType.PublicThread } }); // an uncached name
  assert.equal(fieldOf(nameless, 'Name').value, '`unknown`');
});

const role = (o = {}) => ({ id: '444444444444444444', name: 'Mods', hexColor: '#000000', hoist: false, mentionable: false, icon: null, unicodeEmoji: null, position: 1, managed: false, permissions: new PermissionsBitField([]), createdAt: new Date(), ...o });

test('diffRole: position-only is empty; permission add and remove', () => {
  assert.deepEqual(ev.diffRole(role({ position: 1 }), role({ position: 9 })), []);
  const lines = ev.diffRole(
    role({ permissions: new PermissionsBitField([PermissionFlagsBits.ManageMessages, PermissionFlagsBits.KickMembers]) }),
    role({ name: 'Staff', hexColor: '#ff0000', hoist: true, permissions: new PermissionsBitField([PermissionFlagsBits.ManageMessages, PermissionFlagsBits.BanMembers]) }),
  );
  const get = (n) => lines.find((l) => l.name === n)?.value;
  assert.equal(get('Permissions Added'), 'Ban Members');
  assert.equal(get('Permissions Removed'), 'Kick Members');
  assert.equal(get('Name'), '`Mods` → `Staff`');
  assert.equal(get('Color'), '`#000000` → `#ff0000`');
  assert.equal(get('Hoisted'), 'No → Yes');
  assert.equal(get('Icon'), undefined);
  assert.equal(ev.diffRole(role({ icon: null }), role({ icon: 'abc' })).find((l) => l.name === 'Icon').value, 'changed');
});

test('role embeds', () => {
  const created = ev.buildRoleCreateEmbed({ role: role({ managed: true, permissions: new PermissionsBitField([PermissionFlagsBits.ManageMessages]) }) });
  assert.equal(tick(created).title, '➕ Role Created');
  assert.equal(fieldOf(created, 'Permissions').value, 'Manage Messages');
  assert.equal(fieldOf(created, 'Managed').value, 'Yes (integration or bot role)');
  assert.equal(fieldOf(ev.buildRoleCreateEmbed({ role: role() }), 'Permissions').value, 'None');
  assert.equal(tick(ev.buildRoleDeleteEmbed({ role: role() })).title, '➖ Role Deleted');
  assert.equal(ev.buildRoleUpdateEmbed({ role: role(), lines: [] }), null);
});

const guildOf = (o = {}) => ({
  id: '955007000000000001', name: 'Guild', icon: null, banner: null, splash: null, description: null, ownerId: '1', verificationLevel: 0, explicitContentFilter: 0,
  defaultMessageNotifications: 0, afkChannelId: null, afkTimeout: 300, systemChannelId: null, rulesChannelId: null, publicUpdatesChannelId: null,
  vanityURLCode: null, preferredLocale: 'en-US', premiumTier: 0, mfaLevel: 0, nsfwLevel: 0, premiumProgressBarEnabled: false, premiumSubscriptionCount: 0, ...o,
});

test('diffGuild', () => {
  assert.deepEqual(ev.diffGuild(guildOf({ premiumSubscriptionCount: 1 }), guildOf({ premiumSubscriptionCount: 14 })), []);
  const lines = ev.diffGuild(guildOf(), guildOf({ name: 'Renamed', verificationLevel: 2, afkTimeout: 900, afkChannelId: '5', ownerId: '2', icon: 'abc', mfaLevel: 1 }));
  const get = (n) => lines.find((l) => l.name === n)?.value;
  assert.equal(get('Name'), '`Guild` → `Renamed`');
  assert.equal(get('Verification Level'), 'None → Medium');
  assert.equal(get('AFK Timeout'), '5 minutes → 15 minutes');
  assert.equal(get('AFK Channel'), '*(none)* → <#5>');
  assert.equal(get('Owner'), '<@1> → <@2>');
  assert.equal(get('Icon'), 'changed');
  assert.equal(get('2FA Requirement'), 'None → Elevated');
  assert.equal(ev.buildGuildUpdateEmbed({ oldGuild: guildOf(), newGuild: guildOf(), lines: [] }), null);
});

test('emoji and invite embeds', () => {
  const emoji = (o = {}) => ({ id: '1', name: 'party', animated: false, roles: { cache: new Map() }, imageURL: () => 'https://cdn.discordapp.com/emojis/1.png', toString: () => '<:party:1>', ...o });
  const created = ev.buildEmojiCreateEmbed({ emoji: emoji({ roles: { cache: new Map([['r1', {}]]) } }) });
  assert.equal(fieldOf(created, 'Emoji').value, '<:party:1> `:party:`');
  assert.equal(fieldOf(created, 'Restricted To').value, '<@&r1>');
  assert.equal(tick(created).thumbnail.url, 'https://cdn.discordapp.com/emojis/1.png');
  assert.deepEqual(ev.diffEmoji(emoji(), emoji()), []);
  const lines = ev.diffEmoji(emoji(), emoji({ name: 'party2', roles: { cache: new Map([['r1', {}]]) } }));
  assert.equal(lines[0].value, '`party` → `party2`');
  assert.equal(lines[1].value, '✅ <@&r1>');
  assert.equal(ev.buildEmojiUpdateEmbed({ emoji: emoji(), lines: [] }), null);
  assert.equal(fieldOf(ev.buildEmojiDeleteEmbed({ emoji: emoji() }), 'Name').value, '`:party:`');

  const invite = { code: 'abc', channel: { id: '7' }, inviter: fakeUser('111111111111111111'), maxUses: 0, expiresAt: null, temporary: false };
  const ic = ev.buildInviteCreateEmbed({ invite });
  assert.equal(fieldOf(ic, 'Max Uses').value, '∞');
  assert.equal(fieldOf(ic, 'Expires').value, 'Never');
  assert.equal(tick(ic).footer.text, 'Code: abc');
  assert.equal(fieldOf(ev.buildInviteDeleteEmbed({ invite: { code: 'abc', channel: null } }), 'Channel').value, 'Unknown');
});

// ── 7. fitEmbed ──────────────────────────────────────────────────────────────

test('fitEmbed: 25 fields and 6000 characters', () => {
  const e = neutralEmbed('t');
  e.data.fields = Array.from({ length: 30 }, (_, i) => ({ name: `n${i}`, value: `v${i}` }));
  const fitted = tick(ev.fitEmbed(e));
  assert.equal(fitted.fields.length, 25);
  assert.equal(fitted.fields.at(-1).name, '…');
  assert.equal(fitted.fields.at(-1).value, 'and 6 more changes');

  const big = neutralEmbed('title').setDescription('d'.repeat(4000));
  big.data.fields = Array.from({ length: 10 }, (_, i) => ({ name: `n${i}`, value: 'v'.repeat(1000) }));
  assert.ok(ev.fitEmbed(big) === big);
  assert.ok(embedLength(big.toJSON()) <= 6000);

  const d = fitEmbedData({ title: 'x'.repeat(500), description: 'd'.repeat(5000), footer: { text: 'f'.repeat(1000) } });
  assert.equal(d.title.length, 256);
  assert.equal(d.description.length, 4096);
  assert.equal(d.footer.text.length, 1000);
  assert.ok(embedLength(d) <= 6000);
  const f = fitEmbedData({ fields: [{ name: '', value: '' }, { name: 'n'.repeat(400), value: 'v'.repeat(2000), inline: true }], footer: { text: 'f'.repeat(3000) } });
  assert.equal(f.fields[0].name, '\u200b');
  assert.equal(f.fields[0].value, '\u200b');
  assert.equal(f.fields[1].name.length, 256);
  assert.equal(f.fields[1].value.length, 1024);
  assert.equal(f.fields[1].inline, true);
  assert.equal(f.footer.text.length, 2048);
  assert.ok(embedLength(f) <= 6000);
  // Over 6000 in total: trailing fields go first, then the description is cut.
  const g = fitEmbedData({ title: 't', description: 'd'.repeat(4096), fields: Array.from({ length: 5 }, (_, i) => ({ name: `n${i}`, value: 'v'.repeat(1000) })) });
  assert.ok(embedLength(g) <= 6000);
  assert.ok(g.fields.length >= 1 && g.fields.length < 5);
});


// ── 8. sendLog ───────────────────────────────────────────────────────────────

const plain = (title = 't', description = 'd') => neutralEmbed(title, description);

test('sendLog: logging off, MOD exemption, disabled events', async () => {
  const { guild, channel, settings } = logSetup();
  await sendLog(guild, 'MESSAGE', plain(), { settings: { ...settings, loggingEnabled: false } });
  await flushLogQueue();
  assert.equal(channel.sent.length, 0);

  // Decision D1: the moderation log ignores loggingEnabled and the event toggles.
  const mod = logSetup(ALL_PERMS, { type: 'modLogChannelId', channelId: '700000000000000020' });
  await sendLog(mod.guild, 'MOD', plain(), { settings: { ...mod.settings, loggingEnabled: false, disabledLogEvents: [...LOG_EVENT_KEYS] }, event: 'messageDelete' });
  await flushLogQueue();
  assert.equal(mod.channel.sent.length, 1);

  await sendLog(guild, 'MESSAGE', plain(), { settings: { ...settings, disabledLogEvents: ['messageEdit'] }, event: 'messageEdit' });
  await flushLogQueue();
  assert.equal(channel.sent.length, 0);

  await sendLog(guild, 'MESSAGE', plain(), { settings, event: 'messageEdit' });
  await sendLog(guild, 'VOICE', plain(), { settings }); // no voice channel configured
  await flushLogQueue();
  assert.equal(channel.sent.length, 1);
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [] });
  assert.equal(channel.sent[0].embeds.length, 1);
  assert.equal(channel.sent[0].embeds[0].title, 't');
});

test('sendLog: a missing channel is fetched once and then remembered as dead (R147)', async () => {
  const guild = fakeGuild();
  let fetches = 0;
  guild.channels.fetch = async () => { fetches++; return null; };
  const settings = { loggingEnabled: true, messageLogChannelId: '700000000000000099', disabledLogEvents: [] };
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  await flushLogQueue();
  assert.equal(fetches, 1);
  assert.equal(isLogChannelDead('700000000000000099'), true);
  forgetDeadChannels(guild.id);
  assert.equal(isLogChannelDead('700000000000000099'), false);
});

test('sendLog: a failed fetch that is not a definite "gone" is remembered only briefly (rule 20)', async (t) => {
  t.mock.timers.enable({ apis: ['Date'], now: Date.now() });
  const guild = fakeGuild();
  let fetches = 0;
  guild.channels.fetch = async () => { fetches++; throw Object.assign(new Error('boom'), { code: 500 }); };
  const settings = { loggingEnabled: true, messageLogChannelId: '700000000000000098', disabledLogEvents: [] };
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  assert.equal(fetches, 1); // skipped for a moment, so an outage is not one REST call per event
  assert.equal(isLogChannelDead('700000000000000098'), true);
  t.mock.timers.tick(31_000);
  assert.equal(isLogChannelDead('700000000000000098'), false); // not the 10-minute dead-channel TTL

  // A definite answer (10003 Unknown Channel) is remembered for the full 10 minutes.
  guild.channels.fetch = async () => { throw Object.assign(new Error('Unknown Channel'), { code: 10003 }); };
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  t.mock.timers.tick(31_000);
  assert.equal(isLogChannelDead('700000000000000098'), true);
  t.mock.timers.tick(10 * 60_000);
  assert.equal(isLogChannelDead('700000000000000098'), false);
});

test('sendLog: a missing permission marks the channel dead until it is forgotten (R147)', async () => {
  const { guild, channel, settings } = logSetup(['ViewChannel']);
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  await flushLogQueue();
  assert.equal(channel.sent.length, 0);
  assert.equal(isLogChannelDead(channel.id), true);
  channel._perms = perms(...ALL_PERMS);
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  await flushLogQueue();
  assert.equal(channel.sent.length, 0); // still remembered
  forgetDeadChannels(guild.id);
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  await flushLogQueue();
  assert.equal(channel.sent.length, 1);
});

test('sendLog: an unknown bot member never marks a channel dead (rule 20)', async () => {
  const { guild, channel, settings } = logSetup(['ViewChannel']);
  guild.members.me = null;
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  await flushLogQueue();
  assert.equal(isLogChannelDead(channel.id), false);
  assert.equal(channel.sent.length, 1); // the send is attempted; Discord would answer 50013 if it were not allowed
});

test('sendLog: 12 embeds queued in one tick go out as 10 + 2', async () => {
  const { guild, channel, settings } = logSetup();
  await Promise.all(Array.from({ length: 12 }, (_, i) => sendLog(guild, 'MESSAGE', plain(`e${i}`), { settings })));
  assert.equal(channel.sent.length, 0); // nothing before the flush
  await flushLogQueue();
  assert.deepEqual(channel.sent.map((p) => p.embeds.length), [10, 2]);
  assert.equal(channel.sent[0].embeds[0].title, 'e0');
  assert.equal(channel.sent[1].embeds[1].title, 'e11');
});

test('sendLog: batches also respect the 6000 character cap', async () => {
  const { guild, channel, settings } = logSetup();
  for (let i = 0; i < 4; i++) await sendLog(guild, 'MESSAGE', plain(`e${i}`, 'x'.repeat(2500)), { settings });
  await flushLogQueue();
  assert.deepEqual(channel.sent.map((p) => p.embeds.length), [2, 2]);
});

test('sendLog: the queue is capped and says how many entries were dropped', async () => {
  const { guild, channel, settings } = logSetup();
  for (let i = 0; i < QUEUE_MAX_ITEMS + 5; i++) await sendLog(guild, 'MESSAGE', plain(`e${i}`), { settings });
  await flushLogQueue();
  const first = channel.sent[0].embeds[0];
  assert.match(first.title, /Log entries dropped/);
  assert.match(first.description, /^5 log entries were dropped/);
  assert.equal(channel.sent[1].embeds[0].title, 'e5'); // the oldest 5 were dropped
  assert.equal(channel.sent.length, 1 + QUEUE_MAX_ITEMS / 10);
});

test('sendLog: a rejected send with 50013 marks the channel dead; other errors drop the batch only', async () => {
  const { guild, channel, settings } = logSetup();
  let calls = 0;
  channel.send = async () => { calls++; throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  await flushLogQueue();
  assert.equal(calls, 1);
  assert.equal(isLogChannelDead(channel.id), true);
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  await flushLogQueue();
  assert.equal(calls, 1); // a no-op now

  forgetDeadChannels(guild.id);
  channel.send = async () => { calls++; throw Object.assign(new Error('server error'), { code: 500 }); };
  await sendLog(guild, 'MESSAGE', plain(), { settings });
  await flushLogQueue();
  assert.equal(calls, 2);
  assert.equal(isLogChannelDead(channel.id), false); // a transient failure is not a dead channel
});

test('sendLog: an attachment goes alone, and is omitted without Attach Files', async () => {
  const { guild, channel, settings } = logSetup();
  const file = new AttachmentBuilder(Buffer.from('hi'), { name: 'a.txt' });
  await sendLog(guild, 'MESSAGE', plain('a'), { settings });
  await sendLog(guild, 'MESSAGE', plain('b'), { settings, files: [file] });
  await sendLog(guild, 'MESSAGE', plain('c'), { settings });
  await flushLogQueue();
  assert.deepEqual(channel.sent.map((p) => p.embeds.map((e) => e.title)), [['a'], ['b'], ['c']]);
  assert.equal(channel.sent[1].files.length, 1);

  const noAttach = logSetup(['ViewChannel', 'SendMessages', 'EmbedLinks']);
  await sendLog(noAttach.guild, 'MESSAGE', plain('b'), { settings: noAttach.settings, files: [file] });
  await flushLogQueue();
  assert.equal(noAttach.channel.sent[0].files.length, 0);
  assert.equal(noAttach.channel.sent[0].embeds[0].fields.at(-1).name, 'Attachment omitted');
});

test('sendLog: bad input never throws and never posts', async () => {
  const { guild, channel, settings } = logSetup();
  await sendLog(null, 'MESSAGE', plain(), { settings });
  await sendLog(undefined, 'MESSAGE', plain(), { settings });
  await sendLog(guild, 'NOPE', plain(), { settings });
  await sendLog(guild, 'MESSAGE', null, { settings });
  await sendLog(guild, 'MESSAGE', {}, { settings });
  await sendLog(guild, 'MESSAGE', plain(), { settings: { ...settings, messageLogChannelId: null } });
  await flushLogQueue();
  assert.equal(channel.sent.length, 0);
  // A plain APIEmbed object is accepted too.
  await sendLog(guild, 'MESSAGE', { title: 'raw', description: 'x'.repeat(5000) }, { settings });
  await flushLogQueue();
  assert.equal(channel.sent[0].embeds[0].description.length, 4096);
});

test('normalizeLogOpts: a Client or a non-object is the legacy argument', () => {
  const defaults = { event: null, settings: null, files: [] };
  const client = new Client({ intents: [] });
  try {
    assert.deepEqual(normalizeLogOpts(client), defaults);
  } finally {
    client.destroy();
  }
  assert.deepEqual(normalizeLogOpts(null), defaults);
  assert.deepEqual(normalizeLogOpts(undefined), defaults);
  assert.deepEqual(normalizeLogOpts('x'), defaults);
  assert.deepEqual(normalizeLogOpts([]), defaults);
  assert.equal(normalizeLogOpts({ event: 'messageEdit' }).event, 'messageEdit');
  assert.equal(normalizeLogOpts({ event: 7 }).event, null);
  const s = { loggingEnabled: true };
  assert.equal(normalizeLogOpts({ settings: s }).settings, s);
  assert.equal(normalizeLogOpts({ files: [{}, {}] }).files.length, 1);
});

test('getLogChannelHealth, markLogChannelDeleted', () => {
  const { guild, channel } = logSetup(['ViewChannel']);
  assert.equal(getLogChannelHealth(guild, null).state, 'unset');
  assert.deepEqual(getLogChannelHealth(guild, '700000000000000555'), { state: 'missing', missing: [], text: 'channel not found (deleted?)' });
  const noPerm = getLogChannelHealth(guild, channel.id);
  assert.equal(noPerm.state, 'no-permission');
  assert.match(noPerm.text, /^missing Send Messages, Embed Links$/);
  channel._perms = perms(...ALL_PERMS);
  assert.equal(getLogChannelHealth(guild, channel.id).state, 'ok');
  channel.isTextBased = () => false;
  assert.equal(getLogChannelHealth(guild, channel.id).state, 'not-text');

  assert.equal(isLogChannelDead('c1'), false);
  markLogChannelDeleted(guild.id, 'c1');
  assert.equal(isLogChannelDead('c1'), true);
  forgetDeadChannels(guild.id, 'c1');
  assert.equal(isLogChannelDead('c1'), false);
  markLogChannelDeleted(guild.id, 'c2');
  markLogChannelDeleted('other-guild', 'c3');
  forgetDeadChannels(guild.id);
  assert.equal(isLogChannelDead('c2'), false);
  assert.equal(isLogChannelDead('c3'), true);
});

// ── 9. Command payload ───────────────────────────────────────────────────────

const charCount = (node) => {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
};

test('/logging payload', () => {
  const json = buildCommandPayload(loggingCommand);
  assert.equal(json.name, 'logging');
  assert.ok(charCount(json) <= 1300, `payload is ${charCount(json)} characters`);
  assert.deepEqual(json.options.map((o) => o.name), ['enable', 'disable', 'status', 'channel', 'event', 'ignore']);
  const channelSub = json.options.find((o) => o.name === 'channel');
  const type = channelSub.options.find((o) => o.name === 'type');
  assert.equal(type.choices.length, 6);
  assert.ok(!type.choices.some((c) => /^mod/.test(c.value)));
  const eventSub = json.options.find((o) => o.name === 'event');
  assert.equal(eventSub.options.find((o) => o.name === 'name').autocomplete, true);
  assert.equal(eventSub.options.find((o) => o.name === 'name').max_length, 32);
  assert.deepEqual(json.options.find((o) => o.name === 'ignore').options.map((o) => o.name), ['add', 'remove', 'list']);
  assert.equal(loggingCommand.module, 'logging');
  assert.equal(loggingCommand.gated, false);
  assert.equal(loggingCommand.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(typeof loggingCommand.autocomplete, 'function');
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.deepEqual(json.contexts, [0]); // guild only
  assert.equal(LIMITS.LOG_IGNORED_CHANNELS, 100);
});

function fakeInteraction(guild, { sub, group = null, strings = {}, channels = {}, focused = '', created = Date.now() } = {}) {
  const replies = [];
  const choices = [];
  return {
    guild, guildId: guild.id, createdTimestamp: created, replies, responded: false, choices,
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      getString: (name) => strings[name] ?? null,
      getChannel: (name) => channels[name] ?? null,
      getFocused: () => focused,
    },
    deferReply: async () => {},
    editReply: async (payload) => { replies.push(payload); },
    respond: async (list) => { choices.push(...list); },
  };
}

test('/logging autocomplete: filter, order, and a settings miss means everything is on', async () => {
  const guild = fakeGuild('955007000000000999'); // never loaded: the memo misses, and an expired ack budget skips the lookup
  const expired = Date.now() - 10_000;
  const all = fakeInteraction(guild, { created: expired });
  await loggingCommand.autocomplete(all);
  assert.equal(all.choices.length, 25);
  assert.deepEqual(all.choices[0], { name: 'All events', value: 'all' });
  assert.equal(all.choices[1].value, LOG_EVENT_KEYS[0]);
  assert.ok(all.choices.slice(1).every((c) => c.name.startsWith('✅ ')));
  assert.ok(all.choices.every((c) => c.name.length <= 100 && c.value.length <= 100));

  const ban = fakeInteraction(guild, { focused: ' BAN ', created: expired });
  await loggingCommand.autocomplete(ban);
  assert.deepEqual(ban.choices.map((c) => c.value), ['memberBan', 'memberUnban']);
  const type = fakeInteraction(guild, { focused: 'voice', created: expired });
  await loggingCommand.autocomplete(type);
  assert.deepEqual(type.choices.map((c) => c.value), ['voiceJoin', 'voiceLeave', 'voiceMove', 'voiceState']);
  const user = fakeInteraction(guild, { focused: 'user', created: expired });
  await loggingCommand.autocomplete(user);
  assert.ok(user.choices.some((c) => c.value === 'userUpdate'));
});

// ── 10. Plugin ───────────────────────────────────────────────────────────────

test('plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('logging')), []);
  assert.ok(plugins.some((p) => p.id === 'logging' && p.module === 'logging'));
  assert.equal(plugin.id, 'logging');
  assert.equal(plugin.module, 'logging');
  assert.equal(plugin.hooks.length, 26); // 22 of the spec + userUpdate:logging (architect amendment 7) + 3 thread hooks (fix round)
  assert.ok(plugin.hooks.every((h) => h.id === 'logging'));
  assert.equal(hook('channelDelete').always, true);
  assert.equal(plugin.hooks.filter((h) => h.always).length, 1);
  for (const e of ['threadCreate', 'threadUpdate', 'threadDelete']) assert.ok(hook(e) && !hook(e).always, e); // module-gated like the rest
  assert.deepEqual([...HOOK_ORDER.threadCreate], ['logging', 'automations']);
  assert.equal(hook('messageDeleteBulk').timeoutMs, 15_000);
  assert.ok(hook('userUpdate'));
  // Every HOOK_ORDER slot of logging is declared, and nothing else.
  const expected = Object.entries(HOOK_ORDER).filter(([, ids]) => ids.includes('logging')).map(([event]) => event).sort();
  assert.deepEqual(plugin.hooks.map((h) => h.event).sort(), expected);
  assert.deepEqual(plugin.components, []);
  assert.deepEqual(plugin.jobs, []);
  const filter = hook('messageCreate').filter;
  assert.equal(filter({ message: { content: 'hello' } }), false);
  assert.equal(filter({ message: { content: 'discord.gg/abc' } }), true);
  assert.equal(filter({ message: { content: null } }), false);
  assert.equal(filter({ message: undefined }), false);
});

// ── 11. Hooks against fake contexts ──────────────────────────────────────────

test('messageDelete hook: ghost ping, bot deletion, bot authors, ignored channels, partials', async () => {
  const { guild, channel, settings } = logSetup();
  const victim = fakeUser('222222222222222222');
  const base = { mentionUsers: [victim], guild, channelId: channel.id };
  const run = (message, s = settings) => hook('messageDelete').run(fakeCtx(guild, s, { message, channel: { id: message.channelId } }));

  await run(fakeMessage({ ...base, id: '801' }));
  await flushLogQueue();
  assert.match(firstEmbed(channel).title, /Ghost Ping Detected — Message Deleted/);
  assert.match(firstEmbed(channel).fields.find((f) => f.name === '⚠️ Ghost Pinged').value, /<@222222222222222222>/);

  markBotDeletion('802', 'automod');
  await run(fakeMessage({ ...base, id: '802' }));
  await flushLogQueue();
  assert.equal(firstEmbed(channel, 1).title, '🗑️ Message Deleted (removed by the bot: automod)');
  assert.equal(firstEmbed(channel, 1).fields.some((f) => f.name === '⚠️ Ghost Pinged'), false);

  const before = channel.sent.length;
  await run(fakeMessage({ ...base, id: '803', author: fakeUser('333333333333333333', { bot: true }) }));                  // bot author
  await run(fakeMessage({ ...base, id: '804' }), { ...settings, logIgnoredChannelIds: [channel.id] });                      // ignored channel
  await run(fakeMessage({ ...base, id: '805' }), { ...settings, disabledLogEvents: ['messageDelete'] });                   // event off
  markBotDeletion('806', 'purge');
  await run({ id: '806', partial: true, channelId: channel.id, guild });                                                    // partial + bot reason
  await flushLogQueue();
  assert.equal(channel.sent.length, before);

  await run({ id: '807', partial: true, channelId: channel.id, guild });
  await flushLogQueue();
  assert.match(firstEmbed(channel, before).description, /not cached/);
});

test('messageUpdate hook: defensive checks and a normal edit', async () => {
  const { guild, channel, settings } = logSetup();
  const author = fakeUser();
  const run = (fields) => hook('messageUpdate').run(fakeCtx(guild, settings, fields));
  const same = fakeMessage({ author, content: 'same', channelId: channel.id });
  await run({ oldMessage: same, newMessage: same, oldKnown: true });
  await run({ oldMessage: { partial: true }, newMessage: fakeMessage({ author, content: 'x' }), oldKnown: false }); // no editedTimestamp
  await flushLogQueue();
  assert.equal(channel.sent.length, 0);
  await run({ oldMessage: fakeMessage({ author, content: 'a' }), newMessage: fakeMessage({ author, content: 'b', editedTimestamp: Date.now(), channel: { id: channel.id } }), oldKnown: true });
  await flushLogQueue();
  assert.equal(firstEmbed(channel).title, '✏️ Message Edited');
});

test('messageDeleteBulk hook attaches the transcript and labels purge deletions', async () => {
  const { guild, channel, settings } = logSetup();
  const messages = new Map([['901', fakeMessage({ id: '901' })], ['902', fakeMessage({ id: '902' })]]);
  markBotDeletion('902', '/purge by mod (1)');
  const source = { id: '700000000000000001', name: 'general', guild };
  await hook('messageDeleteBulk').run(fakeCtx(guild, settings, { messages, channel: source }));
  await flushLogQueue();
  assert.match(firstEmbed(channel).description, /\(by the bot: \/purge by mod \(1\)\)/);
  assert.equal(channel.sent[0].files.length, 1);
  assert.match(channel.sent[0].files[0].name, /^bulk-delete-/);

  await hook('messageDeleteBulk').run(fakeCtx(guild, { ...settings, logIgnoredChannelIds: [source.id] }, { messages, channel: source }));
  await flushLogQueue();
  assert.equal(channel.sent.length, 1);
});

test('messageCreate hook logs an invite with the vanity label and is skipped when disabled', async () => {
  const { guild, channel, settings } = logSetup();
  guild.vanityURLCode = 'myserver';
  const message = fakeMessage({ content: 'join discord.gg/myserver now', channelId: channel.id, channel: { id: channel.id } });
  await hook('messageCreate').run(fakeCtx(guild, settings, { message }));
  await flushLogQueue();
  assert.equal(firstEmbed(channel).title, '📨 Invite Posted');
  assert.match(firstEmbed(channel).fields.find((f) => f.name === 'Invites').value, /vanity URL/);
  await hook('messageCreate').run(fakeCtx(guild, { ...settings, disabledLogEvents: ['invitePosted'] }, { message }));
  await flushLogQueue();
  assert.equal(channel.sent.length, 1);
});

test('guildMemberAdd hook passes the invite state through untouched', async () => {
  const { guild, channel, settings } = logSetup(ALL_PERMS, { type: 'joinLeaveChannelId' });
  const member = fakeMember({ id: '222222222222222222' });
  const run = async (invite) => {
    const ctx = fakeCtx(guild, settings, { member, user: member.user });
    if (invite !== 'unset') ctx.state.invite = invite;
    await hook('guildMemberAdd').run(ctx);
    await flushLogQueue();
    return channel.sent.at(-1).embeds[0].fields.find((f) => f.name === 'Invite')?.value;
  };
  assert.equal(await run('unset'), undefined);
  assert.equal(await run(null), 'Unknown');
  assert.equal(await run({ code: 'abc', inviterId: '999999999999999999', vanity: false }), 'discord.gg/abc by <@999999999999999999>');
  assert.equal(await run({ code: 'v', inviterId: null, vanity: true }), 'Vanity URL');
});

test('guildMemberUpdate hook: partial old members log nothing fake, timeouts dedupe, parts are separate embeds', async () => {
  const { guild, channel, settings } = logSetup(ALL_PERMS, { type: 'memberLogChannelId' });
  const run = (oldMember, newMember, oldKnown, s = settings) => hook('guildMemberUpdate').run(fakeCtx(guild, s, { oldMember, newMember, member: newMember, oldKnown }));

  // R033 / R111: a partial clone against a full member must not report its whole state as new.
  await run(fakeMember({ partial: true }), fakeMember({ roles: ['a', 'b'], nickname: 'Ally' }), false);
  await flushLogQueue();
  assert.equal(channel.sent.length, 0);

  // Previous state unknown but a timeout is active: logged once; a second update with the same expiry is a duplicate.
  const until = Date.now() + HOUR;
  const realSetNX = cache.setNX;
  const seen = new Set();
  cache.setNX = async (key) => { if (seen.has(key)) return false; seen.add(key); return true; };
  try {
    await run(fakeMember({ partial: true }), fakeMember({ until }), false);
    await run(fakeMember({ partial: true }), fakeMember({ until }), false);
    await flushLogQueue();
    assert.equal(channel.sent.length, 1);
    assert.equal(firstEmbed(channel).title, '⏱️ Member Timed Out');
    assert.match(firstEmbed(channel).footer.text, /previous state not cached/);
    assert.equal(seen.size, 1);
    assert.match([...seen][0], new RegExp(`^log:timeout:${guild.id}:222222222222222222:${Math.floor(until / 1000)}$`));
  } finally {
    cache.setNX = realSetNX;
  }

  // Redis down (setNX -> null): fail open, the entry is logged.
  await run(fakeMember({ partial: true }), fakeMember({ until: until + 1000 }), false);
  await flushLogQueue();
  assert.equal(channel.sent.length, 2);

  // Known old member: roles, nickname and timeout produce one embed each, in this order.
  const oldM = fakeMember({ roles: ['r1'], nickname: 'Old' });
  const newM = fakeMember({ roles: ['r1', 'r2'], nickname: 'New', until: Date.now() + 2 * HOUR });
  await run(oldM, newM, true);
  await flushLogQueue();
  assert.deepEqual(channel.sent.slice(2).flatMap((p) => p.embeds.map((e) => e.title)), ['🎭 Member Roles Changed', '📝 Nickname Changed', '⏱️ Member Timed Out']);

  // A disabled part is skipped, the others still log.
  const before = channel.sent.length;
  await run(fakeMember({ roles: ['r1'], nickname: 'Old' }), fakeMember({ roles: ['r1', 'r2'], nickname: 'New' }), true, { ...settings, disabledLogEvents: ['memberRoles'] });
  await flushLogQueue();
  assert.deepEqual(channel.sent.slice(before).flatMap((p) => p.embeds.map((e) => e.title)), ['📝 Nickname Changed']);
});

test('guildMemberUpdate hook: the bot member changing clears the dead-channel memo', async () => {
  const { guild, channel, settings } = logSetup(['ViewChannel'], { type: 'memberLogChannelId' });
  await sendLog(guild, 'MEMBER', neutralEmbed('x', 'y'), { settings });
  assert.equal(isLogChannelDead(channel.id), true);
  const bot = fakeMember({ id: 'bot' });
  await hook('guildMemberUpdate').run(fakeCtx(guild, settings, { oldMember: bot, newMember: bot, member: bot, oldKnown: true }));
  assert.equal(isLogChannelDead(channel.id), false);
});

test('voiceStateUpdate hook logs the leave of a deleted channel (R269)', async () => {
  const { guild, channel, settings } = logSetup(ALL_PERMS, { type: 'voiceLogChannelId' });
  const member = fakeMember({ id: '222222222222222222' });
  const run = (oldState, newState, s = settings) => hook('voiceStateUpdate').run(fakeCtx(guild, s, { oldState, newState, member }));
  await run({ channelId: '700000000000000777', channel: null }, { channelId: null, channel: null });
  await run({ channelId: 'A', selfMute: false }, { channelId: 'A', selfMute: false }); // nothing changed
  await run({ channelId: null }, { channelId: channel.id }, { ...settings, disabledLogEvents: ['voiceJoin'] }); // disabled
  await flushLogQueue();
  assert.equal(channel.sent.length, 1);
  assert.equal(firstEmbed(channel).title, '🔇 Left Voice');
  assert.match(firstEmbed(channel).description, /\(deleted channel\)$/);
});

test('userUpdate hook logs name and avatar changes and ignores identical users', async () => {
  const { guild, channel, settings } = logSetup(ALL_PERMS, { type: 'memberLogChannelId' });
  const a = fakeUser('111111111111111111', { username: 'old' });
  const b = fakeUser('111111111111111111', { username: 'new' });
  await hook('userUpdate').run(fakeCtx(guild, settings, { oldUser: a, newUser: a, user: a }));
  await flushLogQueue();
  assert.equal(channel.sent.length, 0);
  await hook('userUpdate').run(fakeCtx(guild, settings, { oldUser: a, newUser: b, user: b }));
  await hook('userUpdate').run(fakeCtx(guild, { ...settings, disabledLogEvents: ['userUpdate'] }, { oldUser: a, newUser: b, user: b }));
  await flushLogQueue();
  assert.equal(channel.sent.length, 1);
  assert.equal(firstEmbed(channel).title, '👤 User Updated');
});

test('ban hooks: without View Audit Log the executor is Unknown; the entry goes to the MEMBER log', async () => {
  const { guild, channel, settings } = logSetup(ALL_PERMS, { type: 'memberLogChannelId' });
  const user = fakeUser('222222222222222222');
  await hook('guildBanAdd').run(fakeCtx(guild, settings, { ban: { reason: 'raid' }, user }));
  await hook('guildBanRemove').run(fakeCtx(guild, settings, { ban: { reason: null }, user }));
  await flushLogQueue();
  assert.deepEqual(channel.sent.flatMap((p) => p.embeds.map((e) => e.title)), ['🔨 Member Banned', '✅ Member Unbanned']);
  assert.equal(channel.sent[0].embeds[0].fields.find((f) => f.name === 'Reason').value, 'raid');
  assert.equal(channel.sent[0].embeds[0].fields.find((f) => f.name === 'By').value, 'Unknown');
});

test('ban hook looks the executor up in the audit log when it may', async () => {
  const { guild, channel, settings } = logSetup(ALL_PERMS, { type: 'memberLogChannelId' });
  guild.members.me.permissions = new PermissionsBitField([PermissionFlagsBits.ViewAuditLog]);
  const user = fakeUser('222222222222222222');
  let asked;
  const entries = [
    { targetId: 'someone-else', executorId: 'x', createdTimestamp: Date.now() },
    { targetId: user.id, executorId: 'bot', reason: 'via audit', createdTimestamp: Date.now() },
  ];
  guild.fetchAuditLogs = async (opts) => { asked = opts; return { entries: { find: (fn) => entries.find(fn) } }; };
  await hook('guildBanAdd').run(fakeCtx(guild, settings, { ban: { reason: 'raid' }, user }));
  await flushLogQueue();
  assert.equal(asked.limit, 5);
  assert.equal(channel.sent[0].embeds[0].fields.find((f) => f.name === 'By').value, '<@bot> (via bot command)');
  assert.equal(channel.sent[0].embeds[0].fields.find((f) => f.name === 'Reason').value, 'via audit');
});

test('role, guild, emoji, invite and channelCreate hooks log and honour the toggles', async () => {
  const { guild, channel, settings } = logSetup(ALL_PERMS, { type: 'serverLogChannelId' });
  const run = (event, fields, s = settings) => hook(event).run(fakeCtx(guild, s, fields));
  await run('roleCreate', { role: role() });
  await run('roleDelete', { role: role() });
  await run('roleUpdate', { oldRole: role({ position: 1 }), newRole: role({ position: 5 }) }); // position-only: nothing
  await run('roleUpdate', { oldRole: role(), newRole: role({ name: 'Renamed' }) });
  await run('guildUpdate', { oldGuild: guildOf(), newGuild: guildOf({ premiumSubscriptionCount: 4 }) }); // nothing
  await run('guildUpdate', { oldGuild: guildOf(), newGuild: guildOf({ name: 'New' }) });
  await run('channelCreate', { channel: chan() });
  await run('channelUpdate', { oldChannel: chan({ position: 1 }), newChannel: chan({ position: 2 }) }); // nothing
  await run('inviteCreate', { invite: { code: 'abc', channel: { id: '7' }, inviter: null, maxUses: 5, expiresAt: null, temporary: true } });
  await run('inviteDelete', { invite: { code: 'abc', channel: null } });
  await run('emojiDelete', { emoji: { id: '1', name: 'e', imageURL: () => 'https://cdn.discordapp.com/emojis/1.png', toString: () => '<:e:1>', roles: { cache: new Map() } } });
  await run('roleCreate', { role: role() }, { ...settings, disabledLogEvents: ['roleCreate'] });
  await flushLogQueue();
  assert.deepEqual(channel.sent.flatMap((p) => p.embeds.map((e) => e.title)), [
    '➕ Role Created', '➖ Role Deleted', '🛠️ Role Updated', '🏠 Server Edited', '📢 Channel Created', '🔗 Invite Created',
    '🔗 Invite Deleted / Expired', '😶 Emoji Deleted',
  ]);
});

test('thread hooks log create, real updates and delete into the server log, and honour the toggles', async () => {
  const { guild, channel, settings } = logSetup(ALL_PERMS, { type: 'serverLogChannelId' });
  const run = (event, fields, s = settings) => hook(event).run(fakeCtx(guild, s, fields));
  await run('threadCreate', { thread: thread() });
  await run('threadUpdate', { oldThread: thread(), newThread: thread({ archived: true }) }); // auto-archive: nothing
  await run('threadUpdate', { oldThread: thread(), newThread: thread({ locked: true }) });
  markBotDeletion('700000000000000777', 'automation');
  await run('threadDelete', { thread: thread() });
  // Switched off per event: nothing more is logged.
  const off = { ...settings, disabledLogEvents: ['threadCreate', 'threadUpdate', 'threadDelete'] };
  await run('threadCreate', { thread: thread() }, off);
  await run('threadUpdate', { oldThread: thread(), newThread: thread({ locked: true }) }, off);
  await run('threadDelete', { thread: thread() }, off);
  // The message ignore list does not apply to thread events.
  await run('threadCreate', { thread: thread({ id: '700000000000000778' }) }, { ...settings, logIgnoredChannelIds: ['700000000000000001', '700000000000000778'] });
  await flushLogQueue();
  const embeds = channel.sent.flatMap((p) => p.embeds);
  assert.deepEqual(embeds.map((e) => e.title), ['🧵 Thread Created', '🧵 Thread Updated', '🗑️ Thread Deleted', '🧵 Thread Created']);
  assert.equal(embeds[1].fields.find((f) => f.name === 'Locked').value, 'Locked');
  assert.equal(embeds[2].fields.find((f) => f.name === 'Deleted By').value, 'the bot (automation)');
});

test('roleUpdate housekeeping: a change to the bot role forgets dead channels', async () => {
  const { guild, channel, settings } = logSetup(['ViewChannel'], { type: 'serverLogChannelId' });
  await sendLog(guild, 'SERVER', neutralEmbed('x', 'y'), { settings });
  assert.equal(isLogChannelDead(channel.id), true);
  guild.members.me.roles.cache.set('444444444444444444', {});
  await hook('roleUpdate').run(fakeCtx(guild, { ...settings, disabledLogEvents: ['roleUpdate'] }, { oldRole: role(), newRole: role() }));
  assert.equal(isLogChannelDead(channel.id), false);
});

test('channelDelete hook (offline part): marks a deleted log channel and logs the voice occupants', async () => {
  const { guild, channel, settings } = logSetup(ALL_PERMS, { type: 'serverLogChannelId' });
  const voice = chan({ id: '700000000000000444', type: ChannelType.GuildVoice, name: 'Lounge', isVoiceBased: () => true });
  guild.voiceStates.cache.set('u1', { id: 'u1', channelId: voice.id });
  guild.voiceStates.cache.set('u2', { id: 'u2', channelId: 'other' });
  markBotDeletion(voice.id, 'tempvoice');
  await hook('channelDelete').run(fakeCtx(guild, settings, { channel: voice }));
  await flushLogQueue();
  const e = firstEmbed(channel);
  assert.equal(e.title, '🗑️ Channel Deleted');
  assert.equal(e.fields.find((f) => f.name === 'Connected Members (1)').value, '<@u1>');
  assert.equal(e.fields.find((f) => f.name === 'Deleted By').value, 'the bot (tempvoice)');

});

// ── 12. Static scan ──────────────────────────────────────────────────────────

test('owned source files follow the package rules', () => {
  const files = ['src/services/loggingService.js', 'src/services/logEvents.js', 'src/plugins/logging.js', 'src/commands/admin/logging.js'];
  for (const f of files) {
    const text = readFileSync(resolve(ROOT, f), 'utf8');
    assert.doesNotMatch(text, /console\./, `${f}: console.*`);
    assert.doesNotMatch(text, /\.slice\(0, 512\)/, `${f}: slice(0, 512)`);
    assert.doesNotMatch(text, /cache\.del\(`guild:/, `${f}: cache.del of settings`);
    assert.doesNotMatch(text, /LIMITS\.[A-Z_]+\s*\?\?/, `${f}: LIMITS fallback`);
    assert.doesNotMatch(text, /new EmbedBuilder/, `${f}: raw EmbedBuilder`);
    assert.doesNotMatch(text, /prisma\.guildSettings/, `${f}: direct settings write`);
  }
  // loggingService reaches neither prisma nor redis (settings go through settingsService only).
  const service = readFileSync(resolve(ROOT, files[0]), 'utf8');
  assert.doesNotMatch(service, /from '\.\.\/database\/client\.js'|from '\.\/redis\.js'/);
  // logEvents does no I/O.
  const events = readFileSync(resolve(ROOT, files[1]), 'utf8');
  assert.doesNotMatch(events, /prisma|redis|fetch\(|sendLog|getSettings/);
});

// ── 13. DB cases ─────────────────────────────────────────────────────────────

const fresh = (n) => `95500700000000${String(n).padStart(4, '0')}`;

async function dbGuild(id, extra = {}) {
  const guild = fakeGuild(id);
  await prisma.guildSettings.upsert({ where: { guildId: id }, update: {}, create: { guildId: id } });
  await updateSettings(id, { loggingEnabled: false, messageLogChannelId: null, memberLogChannelId: null, serverLogChannelId: null, voiceLogChannelId: null, joinLeaveChannelId: null, modLogChannelId: null, disabledLogEvents: [], logIgnoredChannelIds: [], ...extra });
  return guild;
}

const embedOf = (interaction) => interaction.replies.at(-1).embeds[0].toJSON();

test('/logging end to end against a real settings row', { skip: skipDb }, async () => {
  const guild = await dbGuild(fresh(1));
  const ch = fakeChannel(guild, fresh(11));
  const noPerm = fakeChannel(guild, fresh(12), ['ViewChannel']);
  const exec = async (opts) => {
    const i = fakeInteraction(guild, opts);
    await loggingCommand.execute(i);
    return i;
  };
  const row = () => getSettings(guild.id, { fresh: true });

  // enable without a channel: warns that nothing will be posted
  let i = await exec({ sub: 'enable' });
  assert.equal(embedOf(i).title, '⚠️  Audit Logs Enabled — No Channel Yet');
  assert.equal((await row()).loggingEnabled, true);

  // enable with an unusable channel writes nothing
  await exec({ sub: 'disable' });
  assert.equal((await row()).loggingEnabled, false);
  i = await exec({ sub: 'enable', channels: { channel: noPerm } });
  assert.equal(embedOf(i).title, '❌  Missing Permissions');
  assert.match(embedOf(i).description, /Send Messages, Embed Links/);
  assert.equal((await row()).loggingEnabled, false);
  assert.equal((await row()).messageLogChannelId, null);

  // enable with a channel sets all five fields
  i = await exec({ sub: 'enable', channels: { channel: ch } });
  assert.equal(embedOf(i).title, '✅  Audit Logs Enabled');
  assert.match(embedOf(i).description, new RegExp(`Every log type will be posted in <#${ch.id}>`));
  let s = await row();
  assert.equal(s.loggingEnabled, true);
  for (const f of ['messageLogChannelId', 'memberLogChannelId', 'serverLogChannelId', 'voiceLogChannelId', 'joinLeaveChannelId']) assert.equal(s[f], ch.id);
  assert.equal(s.modLogChannelId, null);

  // channel: set one type, refuse an unusable channel, clear a type
  const other = fakeChannel(guild, fresh(13));
  await exec({ sub: 'channel', strings: { type: 'voice' }, channels: { channel: other } });
  s = await row();
  assert.equal(s.voiceLogChannelId, other.id);
  assert.equal(s.messageLogChannelId, ch.id);
  i = await exec({ sub: 'channel', strings: { type: 'message' }, channels: { channel: noPerm } });
  assert.equal(embedOf(i).title, '❌  Missing Permissions');
  assert.equal((await row()).messageLogChannelId, ch.id);
  i = await exec({ sub: 'channel', strings: { type: 'joinleave' } });
  assert.equal(embedOf(i).title, '✅  Log Channel Cleared');
  assert.equal((await row()).joinLeaveChannelId, null);
  i = await exec({ sub: 'channel', strings: { type: 'bogus' }, channels: { channel: ch } });
  assert.equal(embedOf(i).title, '❌  Unknown Type');
  i = await exec({ sub: 'channel', strings: { type: 'all' }, channels: { channel: other } });
  assert.match(embedOf(i).description, /^\*\*All\*\* logs will be posted in/);
  assert.equal((await row()).messageLogChannelId, other.id);

  // The Attach Files and "currently off" hints.
  const noAttach = fakeChannel(guild, fresh(14), ['ViewChannel', 'SendMessages', 'EmbedLinks']);
  await exec({ sub: 'disable' });
  i = await exec({ sub: 'channel', strings: { type: 'message' }, channels: { channel: noAttach } });
  assert.match(embedOf(i).description, /Attach Files/);
  assert.match(embedOf(i).description, /currently \*\*off\*\*/);
  await exec({ sub: 'enable' });

  // event: single, all, already-in-state, unknown
  i = await exec({ sub: 'event', strings: { name: 'messageEdit', state: 'off' } });
  assert.equal(embedOf(i).title, '✅  Log Event Updated');
  assert.equal(embedOf(i).description, `${LOG_EVENTS.messageEdit.emoji} **Message edited** is now **off**.`);
  assert.deepEqual((await row()).disabledLogEvents, ['messageEdit']);
  i = await exec({ sub: 'event', strings: { name: 'messageEdit', state: 'off' } });
  assert.match(embedOf(i).description, /^It was already off\./);
  i = await exec({ sub: 'event', strings: { name: ' roleDelete ', state: 'off' } });
  assert.deepEqual((await row()).disabledLogEvents, ['messageEdit', 'roleDelete']);
  i = await exec({ sub: 'event', strings: { name: 'all', state: 'off' } });
  assert.equal(embedOf(i).description, 'Every log event is now **off**.');
  assert.equal((await row()).disabledLogEvents.length, LOG_EVENT_KEYS.length);
  i = await exec({ sub: 'event', strings: { name: 'all', state: 'on' } });
  assert.deepEqual((await row()).disabledLogEvents, []);
  i = await exec({ sub: 'event', strings: { name: 'nope', state: 'on' } });
  assert.equal(embedOf(i).title, '❌  Unknown Event');
  // Junk keys already in the row are stripped by the next write.
  await updateSettings(guild.id, { disabledLogEvents: ['junk', 'memberBan'] });
  await exec({ sub: 'event', strings: { name: 'voiceMove', state: 'off' } });
  assert.deepEqual((await row()).disabledLogEvents, ['memberBan', 'voiceMove']);

  // status renders every section within limits
  i = await exec({ sub: 'status' });
  const status = embedOf(i);
  assert.equal(status.title, '📜 Audit Logs');
  assert.equal(status.fields.find((f) => f.name === 'Status').value, '✅ On');
  assert.match(status.fields.find((f) => f.name === 'Channels').value, /\*\*Messages\*\* → <#\d+> ✅/);
  assert.match(status.fields.find((f) => f.name === 'Channels').value, /\*\*Moderation\*\* → not set/);
  assert.match(status.fields.find((f) => f.name === 'Disabled events').value, /Member banned/);
  assert.equal(status.fields.find((f) => f.name === 'Ignored channels').value, 'None.');
  assert.equal(status.footer.text, `${LOG_EVENT_KEYS.length - 2}/${LOG_EVENT_KEYS.length} events on · MEE6: Moderator › Audit logs`);
  await updateSettings(guild.id, { messageLogChannelId: fresh(15), modLogChannelId: fresh(16) });
  i = await exec({ sub: 'status' });
  assert.match(embedOf(i).fields.find((f) => f.name === 'Channels').value, /\*\*Messages\*\* → <#\d+> ⚠️ channel not found \(deleted\?\)/);
  assert.match(embedOf(i).fields.find((f) => f.name === 'Channels').value, /\*\*Moderation\*\* → <#\d+> \(set with `\/modconfig logchannel`\)/);
});

test('/logging ignore add, remove, list and the limit', { skip: skipDb }, async () => {
  const guild = await dbGuild(fresh(2));
  const a = fakeChannel(guild, fresh(21));
  const cat = fakeChannel(guild, fresh(22), ALL_PERMS, { type: ChannelType.GuildCategory, name: 'Staff' });
  const exec = async (sub, channel) => {
    const i = fakeInteraction(guild, { group: 'ignore', sub, channels: { channel } });
    await loggingCommand.execute(i);
    return embedOf(i);
  };
  const row = () => getSettings(guild.id, { fresh: true });

  assert.equal((await exec('list')).description, 'No channels are ignored.');
  assert.equal((await exec('add', a)).title, '✅  Channel Ignored');
  assert.equal((await exec('add', a)).title, '⚠️  Already Ignored');
  const added = await exec('add', cat);
  assert.match(added.description, /in every channel of \*\*Staff\*\*/);
  assert.deepEqual((await row()).logIgnoredChannelIds, [a.id, cat.id]);
  const list = await exec('list');
  assert.equal(list.description, `<#${a.id}>\n<#${cat.id}>`);
  assert.equal(list.footer.text, `2/${LIMITS.LOG_IGNORED_CHANNELS}`);
  assert.equal((await exec('remove', a)).title, '✅  Channel No Longer Ignored');
  assert.equal((await exec('remove', a)).title, '⚠️  Not Ignored');
  assert.deepEqual((await row()).logIgnoredChannelIds, [cat.id]);

  // deleted channels are listed by id
  await updateSettings(guild.id, { logIgnoredChannelIds: ['955007000000009999'] });
  assert.match((await exec('list')).description, /#deleted-channel \(`955007000000009999`\)/);

  // the limit
  const ids = Array.from({ length: LIMITS.LOG_IGNORED_CHANNELS }, (_, n) => `9550070000${String(n).padStart(8, '0')}`);
  await updateSettings(guild.id, { logIgnoredChannelIds: ids });
  const over = await exec('add', fakeChannel(guild, '955007999999999991'));
  assert.equal(over.title, '❌  Limit Reached');
  assert.equal((await row()).logIgnoredChannelIds.length, LIMITS.LOG_IGNORED_CHANNELS);
  const bigList = await exec('list');
  assert.ok(bigList.description.length <= 4000);
});

test('/logging: concurrent ignore adds lose nothing (updateSettingsWith)', { skip: skipDb }, async () => {
  const guild = await dbGuild(fresh(3));
  const channels = Array.from({ length: 6 }, (_, n) => fakeChannel(guild, fresh(30 + n)));
  await Promise.all(channels.map((channel) => loggingCommand.execute(fakeInteraction(guild, { group: 'ignore', sub: 'add', channels: { channel } }))));
  const s = await getSettings(guild.id, { fresh: true });
  assert.deepEqual([...s.logIgnoredChannelIds].sort(), channels.map((c) => c.id).sort());
});

test('/logging reports "Could Not Save" when the write fails', { skip: skipDb }, async () => {
  const guild = fakeGuild(fresh(7));
  const channel = fakeChannel(guild, fresh(41));
  channel.id = 12345; // not a string: the settings write is rejected, which must end in the friendly error
  guild.channels.cache.set(12345, channel);
  const i = fakeInteraction(guild, { sub: 'enable', channels: { channel } });
  await loggingCommand.execute(i);
  assert.equal(embedOf(i).title, '❌  Could Not Save');
  assert.match(embedOf(i).description, /Something went wrong while saving/);
});

test('channelDelete hook cleans the settings of a deleted log channel (R147, moderation log included)', { skip: skipDb }, async () => {
  const guild = await dbGuild(fresh(4));
  const X = fresh(51);
  const Y = fresh(52);
  await updateSettings(guild.id, { loggingEnabled: true, serverLogChannelId: X, modLogChannelId: X, messageLogChannelId: Y, logIgnoredChannelIds: [X, Y] });
  const settings = await getSettings(guild.id, { fresh: true });
  const channel = chan({ id: X, name: 'old-logs' });
  await hook('channelDelete').run(fakeCtx(guild, settings, { channel }));
  const after = await getSettings(guild.id, { fresh: true });
  assert.equal(after.serverLogChannelId, null);
  assert.equal(after.modLogChannelId, null);
  assert.equal(after.messageLogChannelId, Y);
  assert.deepEqual(after.logIgnoredChannelIds, [Y]);
  assert.equal(isLogChannelDead(X), true);

  // Unrelated channel: nothing changes.
  const before = await getSettings(guild.id, { fresh: true });
  await hook('channelDelete').run(fakeCtx(guild, before, { channel: chan({ id: fresh(53) }) }));
  const same = await getSettings(guild.id, { fresh: true });
  assert.equal(same.messageLogChannelId, Y);
  assert.deepEqual(same.logIgnoredChannelIds, [Y]);
});

test('channelDelete hook: cleanup still runs while the module is off, and nothing is logged', { skip: skipDb }, async () => {
  const guild = await dbGuild(fresh(5));
  const log = fakeChannel(guild, fresh(61));
  const gone = fresh(62);
  await updateSettings(guild.id, { loggingEnabled: false, serverLogChannelId: log.id, joinLeaveChannelId: gone });
  const settings = await getSettings(guild.id, { fresh: true });
  await hook('channelDelete').run(fakeCtx(guild, settings, { channel: chan({ id: gone }) }));
  await flushLogQueue();
  assert.equal(log.sent.length, 0);
  assert.equal((await getSettings(guild.id, { fresh: true })).joinLeaveChannelId, null);
});

test('channelUpdate hook skips name-only renames of stat channels (bot-managed)', { skip: skipDb }, async () => {
  const guild = await dbGuild(fresh(6));
  const log = fakeChannel(guild, fresh(71));
  await updateSettings(guild.id, { loggingEnabled: true, serverLogChannelId: log.id });
  const settings = await getSettings(guild.id, { fresh: true });
  const statId = fresh(72);
  const plainId = fresh(73);
  await prisma.statChannel.create({ data: { guildId: guild.id, channelId: statId, kind: 'MEMBERS', template: 'Members: {count}' } });
  const rename = (id) => hook('channelUpdate').run(fakeCtx(guild, settings, { oldChannel: chan({ id, name: 'Members: 1' }), newChannel: chan({ id, name: 'Members: 2' }) }));

  await rename(statId);
  await flushLogQueue();
  assert.equal(log.sent.length, 0);
  await rename(plainId);
  await flushLogQueue();
  assert.equal(log.sent.length, 1);

  // A stat channel's other changes are still logged.
  await hook('channelUpdate').run(fakeCtx(guild, settings, { oldChannel: chan({ id: statId, name: 'a', topic: 'x' }), newChannel: chan({ id: statId, name: 'b', topic: 'y' }) }));
  await flushLogQueue();
  assert.equal(log.sent.length, 2);

  // Temp-voice channels are bot-managed as well.
  const hub = await prisma.tempVoiceHub.create({ data: { guildId: guild.id, channelId: fresh(74) } });
  const tempId = fresh(75);
  await prisma.tempVoiceChannel.create({ data: { guildId: guild.id, hubId: hub.id, channelId: tempId, ownerId: '1', index: 1 } });
  await rename(tempId);
  await flushLogQueue();
  assert.equal(log.sent.length, 2);
});
