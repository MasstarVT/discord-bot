// pkg-reaction-roles tests (pkg-reaction-roles.md section 9 plus the architect amendments). Offline by default: emoji and link
// parsing, the role-change planner, payload builders, the panel index filters, the reaction hook against fake Discord objects,
// the command payload, the plugin declaration and the static checks. With TEST_DATABASE_URL (a migrated database) the DB cases
// also run: every service query, the panel index, the component handlers, the prune/sync engine and every /reactionrole
// subcommand against fake interactions:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/reaction-roles.test.js
// NOTE: the database tests run on one Postgres connection (connection_limit=1).
import { test, after, mock } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { readFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { Collection, PermissionFlagsBits, MessageFlags } = await import('discord.js');
const rr = await import('../src/services/reactionRoleService.js');
const cmd = await import('../src/commands/admin/reactionrole.js');
const pluginMod = await import('../src/plugins/reaction-roles.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { ensureGuildSettings, updateSettings } = await import('../src/services/settingsService.js');
const { markBotDeletion } = await import('../src/utils/deletionTracker.js');

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const G = (n) => `95501900000000${String(n).padStart(4, '0')}`; // guild ids unique to this package (port 55019)
const ALL_GUILDS = Array.from({ length: 80 }, (_, i) => G(i + 1));

after(async () => {
  Object.assign(console, quiet);
  await rr.__idle();
  redis.disconnect();
  if (DB) await prisma.guildSettings.deleteMany({ where: { guildId: { in: ALL_GUILDS } } }).catch(() => {});
  await prisma.$disconnect().catch(() => {});
});

// ── Helpers and fakes ─────────────────────────────────────────────────────────

const sid = (prefix, n) => `${prefix}${String(n).padStart(18 - prefix.length, '0')}`; // 18-digit snowflake-like id
const BOT = sid('6', 1);
const USER = (n) => sid('7', n);
const ROLE = (n) => sid('4', n);
let seq = 0;
const nextId = () => sid('5', ++seq);
const discordErr = (code) => Object.assign(new Error(`Discord ${code}`), { code });
const ADMIN_BITS = (deny = []) => ({ has: (bit) => !deny.some((k) => PermissionFlagsBits[k] === bit) });

function fakeRole(id, name, { position = 1, managed = false, editable = true, dangerous = [] } = {}) {
  return {
    id, name, position, managed, editable,
    permissions: { has: (bit) => dangerous.some((k) => PermissionFlagsBits[k] === bit) },
    toString() { return `<@&${id}>`; },
  };
}

function fakeReaction(message, stored, me = true) {
  const m = /^<a?:(\w+):(\d+)>$/.exec(stored);
  const emoji = m ? { id: m[2], name: m[1] } : { id: null, name: stored };
  const reaction = {
    emoji, me, message, removedUsers: [], removedAll: false,
    users: { remove: async (uid) => { reaction.removedUsers.push(uid); } },
    remove: async () => { reaction.removedAll = true; message.reactions.cache.delete(emoji.id ?? emoji.name); },
  };
  return reaction;
}

function fakeMessage(id, channel, payload = {}) {
  const message = {
    id, channelId: channel.id, channel, guild: channel.guild, guildId: channel.guild?.id,
    content: payload.content ?? '', embeds: payload.embeds ?? [], payload, edits: [], reacted: [], deleted: false,
    url: `https://discord.com/channels/${channel.guild?.id}/${channel.id}/${id}`,
    reactions: { cache: new Collection() }, editError: null, reactError: null,
    async edit(p) { if (message.editError) throw message.editError; message.edits.push(p); return message; },
    async react(e) {
      if (message.reactError) throw message.reactError;
      message.reacted.push(e);
      const r = fakeReaction(message, e);
      message.reactions.cache.set(r.emoji.id ?? r.emoji.name, r);
    },
    async delete() { message.deleted = true; channel._messages.delete(id); },
  };
  return message;
}

function fakeChannel(id, { name = 'roles', deny = [] } = {}) {
  const channel = {
    id, name, _messages: new Map(), sent: [], sendError: null,
    isTextBased: () => true, isThread: () => false,
    permissionsFor: () => ADMIN_BITS(deny),
    toString() { return `<#${id}>`; },
    messages: { fetch: async (mid) => { const m = channel._messages.get(mid); if (!m) throw discordErr(10008); return m; } },
    async send(payload) {
      if (channel.sendError) throw channel.sendError;
      const m = fakeMessage(nextId(), channel, payload);
      channel._messages.set(m.id, m);
      channel.sent.push(m);
      return m;
    },
  };
  return channel;
}

function fakeGuild(id, { roles = [], channels = [], emojis = [] } = {}) {
  const me = { id: BOT, permissions: { has: () => true }, roles: { highest: { position: 50 } } };
  const guild = { id, ownerId: USER(999), available: true, members: { me } };
  guild.roles = { cache: new Collection(roles.map((r) => [r.id, r])), fetch: async (rid) => guild.roles.cache.get(rid) ?? null };
  guild.channels = {
    cache: new Collection(channels.map((c) => [c.id, c])),
    fetch: async (cid) => { const c = guild.channels.cache.get(cid); if (!c) throw discordErr(10003); return c; },
  };
  guild.emojis = { cache: new Collection(emojis.map((e) => [e.id, e])) };
  for (const c of channels) c.guild = guild;
  return guild;
}

/** A member whose role state lives on the "server": the cache is only refreshed by fetch(), like the real cache. */
function fakeMember(id, held = []) {
  const server = new Set(held);
  const member = { id, calls: [], server, failRole: new Map() };
  const refresh = () => { member.roles.cache = new Collection([...server].map((r) => [r, { id: r }])); };
  const op = (kind) => async (rid, reason) => {
    if (Array.isArray(rid)) throw new Error('the array form must never be used');
    if (member.failRole.has(rid)) throw discordErr(member.failRole.get(rid));
    member.calls.push([kind, rid, reason]);
    if (kind === 'add') server.add(rid); else server.delete(rid);
  };
  member.roles = { cache: null, add: op('add'), remove: op('remove'), highest: { position: 100 } };
  member.fetch = async () => { refresh(); return member; };
  refresh();
  return member;
}

function fakeEmoji(id, name, over = {}) {
  return { id, name, available: true, roles: { cache: new Collection() }, toString() { return `<:${name}:${id}>`; }, ...over };
}

// ── 1-3. Emoji keys and validation helpers ────────────────────────────────────

test('1. normalizeEmojiKey and emojiKeyOfStored', () => {
  assert.equal(rr.normalizeEmojiKey('❤️'), rr.normalizeEmojiKey('❤'));
  assert.equal(rr.emojiKeyOfStored('<:pog:123456789012345678>'), '123456789012345678');
  assert.equal(rr.emojiKeyOfStored('<a:pog:123456789012345678>'), '123456789012345678');
  assert.equal(rr.emojiKeyOfStored('❤️'), '❤');
  assert.equal(rr.emojiKeyOfStored(null), null);
  assert.equal(rr.emojiKeyOfStored(''), null);
});

test('2. isUnicodeEmoji accepts one emoji only', () => {
  for (const ok of ['⭐', '❤️', '❤', '👍🏽', '👨‍👩‍👧', '🇬🇧', '1️⃣', '#️⃣', '🏴󠁧󠁢󠁥󠁮󠁧󠁿', '🏳️‍🌈', '❤️‍🔥', '🫠', '✅', '🔔', '©️', '🎮']) {
    assert.equal(rr.isUnicodeEmoji(ok), true, `should accept ${ok}`);
  }
  for (const bad of ['a', '1', '#', 'abc', ':smile:', '100%', '✅✅', '<:x:1>', '', ' ']) {
    assert.equal(rr.isUnicodeEmoji(bad), false, `should reject "${bad}"`);
  }
  assert.equal(rr.isUnicodeEmoji(null), false);
});

test('3. componentEmoji never throws and drops invalid emoji', () => {
  assert.deepEqual(rr.componentEmoji('<:pog:123456789012345678>'), { id: '123456789012345678', name: 'pog', animated: false });
  assert.deepEqual(rr.componentEmoji('<a:dance:123456789012345678>'), { id: '123456789012345678', name: 'dance', animated: true });
  assert.deepEqual(rr.componentEmoji('✅'), { name: '✅' });
  for (const bad of [':smile:', 'abc', ':x:', '100%', null, undefined, '', '<:x:1>']) assert.equal(rr.componentEmoji(bad), null, String(bad));
});

test('4. parseEmojiInput', () => {
  const party = fakeEmoji('555555555555555555', 'party');
  const noRole = fakeEmoji('666666666666666666', 'locked', { roles: { cache: new Collection([[ROLE(1), {}]]) } });
  const gone = fakeEmoji('777777777777777777', 'gone', { available: false });
  const guild = { emojis: { cache: new Collection([[party.id, party]]) } };
  const client = {
    emojis: { cache: new Collection([[party.id, party], [noRole.id, { ...noRole, guild: { members: { me: { roles: { cache: new Collection() } } } } }], [gone.id, gone]]) },
  };
  assert.deepEqual(rr.parseEmojiInput('✅', { guild, client }), { ok: true, emoji: '✅', key: '✅' });
  assert.deepEqual(rr.parseEmojiInput('', { guild, client }), { ok: true, emoji: null, key: null });
  assert.deepEqual(rr.parseEmojiInput(null, { guild, client }), { ok: true, emoji: null, key: null });
  assert.equal(rr.parseEmojiInput('✅✅', { guild, client }).error, 'Use one emoji per role.');
  const byName = rr.parseEmojiInput(':party:', { guild, client });
  assert.deepEqual(byName, { ok: true, emoji: '<:party:555555555555555555>', key: '555555555555555555' });
  assert.deepEqual(rr.parseEmojiInput(':PARTY:', { guild, client }).ok, true); // case-insensitive fallback
  assert.match(rr.parseEmojiInput(':nope:', { guild, client }).error, /can't find a server emoji called :nope:/);
  assert.match(rr.parseEmojiInput('nope', { guild, client }).error, /can't find a server emoji called :nope:/);
  assert.match(rr.parseEmojiInput('<:x:999999999999999999>', { guild, client }).error, /I can't use that emoji/);
  assert.match(rr.parseEmojiInput('<:gone:777777777777777777>', { guild, client }).error, /unavailable/);
  assert.match(rr.parseEmojiInput('<:locked:666666666666666666>', { guild, client }).error, /limited to roles/);
  assert.equal(rr.parseEmojiInput('555555555555555555', { guild, client }).emoji, '<:party:555555555555555555>');
  assert.equal(rr.parseEmojiInput('hello world', { guild, client }).error, '"hello world" is not an emoji.');
  assert.equal(rr.parseEmojiInput('x'.repeat(100) + ' y', { guild, client }).ok, false);
  assert.doesNotThrow(() => rr.parseEmojiInput({ toString() { throw new Error('boom'); } }, { guild, client }));
});

test('5. parseMessageReference', () => {
  const ctx = { guildId: '111111111111111111', channelId: '333333333333333333' };
  const link = (host, tail = '') => `https://${host}/channels/111111111111111111/222222222222222222/444444444444444444${tail}`;
  for (const host of ['discord.com', 'ptb.discord.com', 'canary.discord.com', 'discordapp.com']) {
    assert.deepEqual(rr.parseMessageReference(link(host), ctx), { ok: true, channelId: '222222222222222222', messageId: '444444444444444444' }, host);
    assert.equal(rr.parseMessageReference(`<${link(host)}>`, ctx).ok, true);
  }
  assert.equal(rr.parseMessageReference(link('discord.com', '?x=1'), ctx).ok, true);
  assert.equal(rr.parseMessageReference('https://discord.com/channels/999999999999999999/222222222222222222/444444444444444444', ctx).error, 'That message is in a different server.');
  assert.deepEqual(rr.parseMessageReference('111111111111111111-222222222222222222', ctx), { ok: true, channelId: '111111111111111111', messageId: '222222222222222222' });
  assert.deepEqual(rr.parseMessageReference('444444444444444444', ctx), { ok: true, channelId: '333333333333333333', messageId: '444444444444444444' });
  assert.equal(rr.parseMessageReference('hello', ctx).ok, false);
  assert.equal(rr.parseMessageReference('https://discord.com/channels/@me/222222222222222222/444444444444444444', ctx).ok, false);
  assert.equal(rr.parseMessageReference('', ctx).ok, false);
});

test('5b. defaultAttachTitle, resolveItem and itemChoiceName', () => {
  assert.equal(rr.defaultAttachTitle({ content: '\n**Rules** here\nmore' }, 'general'), 'Rules here');
  assert.equal(rr.defaultAttachTitle({ content: '', embeds: [{ title: 'Embedded' }] }, 'general'), 'Embedded');
  assert.equal(rr.defaultAttachTitle({ content: '  ' }, 'general'), 'Message in #general');
  assert.equal(rr.defaultAttachTitle({ content: 'x'.repeat(500) }, 'g').length, 100);
  const group = { items: [{ id: 'i1', roleId: ROLE(1), label: 'News' }, { id: 'i2', roleId: ROLE(2), label: 'Events' }] };
  assert.equal(rr.resolveItem(group, 'i2').roleId, ROLE(2));
  assert.equal(rr.resolveItem(group, ROLE(1)).id, 'i1');
  assert.equal(rr.resolveItem(group, `<@&${ROLE(2)}>`).id, 'i2');
  assert.equal(rr.resolveItem(group, ' news ').id, 'i1');
  assert.equal(rr.resolveItem(group, 'nothing'), null);
  const guild = fakeGuild(G(1), { roles: [fakeRole(ROLE(1), 'News')] });
  assert.equal(rr.itemChoiceName({ roleId: ROLE(1), label: 'News', emoji: '🔔' }, guild), '🔔 News · @News');
  assert.equal(rr.itemChoiceName({ roleId: ROLE(9), label: 'Gone', emoji: ':bad:' }, guild), 'Gone · deleted role');
  assert.ok(rr.itemChoiceName({ roleId: ROLE(1), label: 'x'.repeat(300) }, guild).length <= 100);
});

test('6. panelChoiceName stays within 100 characters', () => {
  const name = rr.panelChoiceName({ title: 'T'.repeat(256), layout: 'REACTIONS', itemCount: 3 });
  assert.ok(name.length <= 100);
  assert.ok(name.endsWith(' · Emoji reactions · 3 role(s)'));
  assert.equal(rr.panelChoiceName({ title: 'News', layout: 'BUTTONS', itemCount: 0 }), 'News · Buttons · 0 role(s)');
});

// ── 7. The planner (section 6.5) ──────────────────────────────────────────────

const P4 = ['A', 'B', 'C', 'D'];
const plan = (type, source, { held = [], target = [], max = null, panel = P4 } = {}) =>
  rr.planRoleChange({ type, source, panelRoleIds: panel, heldRoleIds: held, targetRoleIds: target, maxRoles: max });
const expectPlan = (p, { add = [], remove = [], skipped = [], reason = null }) => {
  assert.deepEqual({ add: p.toAdd, remove: p.toRemove, skipped: p.skipped, reason: p.reason }, { add, remove, skipped, reason });
};

test('7a. planner: NORMAL', () => {
  expectPlan(plan('NORMAL', 'button', { held: ['A'], target: ['A'] }), { remove: ['A'] });
  expectPlan(plan('NORMAL', 'button', { target: ['A'] }), { add: ['A'] });
  expectPlan(plan('NORMAL', 'button', { held: ['A'], target: ['B'], max: 1 }), { reason: 'max' });
  expectPlan(plan('NORMAL', 'button', { held: ['A'], target: ['A'], max: 1 }), { remove: ['A'] });
  expectPlan(plan('NORMAL', 'react', { held: ['A'], target: ['A'] }), { reason: 'no-change' });
  expectPlan(plan('NORMAL', 'react', { target: ['A'] }), { add: ['A'] });
  expectPlan(plan('NORMAL', 'react', { held: ['A', 'B'], target: ['C'], max: 2 }), { reason: 'max' });
  expectPlan(plan('NORMAL', 'unreact', { held: ['A'], target: ['A'] }), { remove: ['A'] });
  expectPlan(plan('NORMAL', 'unreact', { target: ['A'] }), { reason: 'no-change' });
  // R044/R159: the select applies the selection as a set
  expectPlan(plan('NORMAL', 'select', { held: ['A', 'B'], target: ['A', 'B', 'C'] }), { add: ['C'] });
  expectPlan(plan('NORMAL', 'select', { held: ['A', 'B'], target: ['A'] }), { remove: ['B'] });
  expectPlan(plan('NORMAL', 'select', { held: ['A', 'B'], target: [] }), { remove: ['A', 'B'] });
  expectPlan(plan('NORMAL', 'select', { held: ['A'], target: ['A'] }), { reason: 'no-change' });
  // R160: the cap counts the roles that stay, not the roles being removed
  expectPlan(plan('NORMAL', 'select', { held: ['A', 'B'], target: ['A', 'C'], max: 2 }), { add: ['C'], remove: ['B'] });
  expectPlan(plan('NORMAL', 'select', { target: ['A', 'B', 'C'], max: 2 }), { add: ['A', 'B'], skipped: ['C'] });
  expectPlan(plan('NORMAL', 'select', { held: ['A', 'B'], target: ['A', 'B', 'C'], max: 2 }), { skipped: ['C'], reason: 'max' });
});

test('7b. planner: UNIQUE', () => {
  const swap = plan('UNIQUE', 'button', { held: ['B'], target: ['A'] });
  expectPlan(swap, { add: ['A'], remove: ['B'] });
  assert.deepEqual(Object.keys(swap).slice(0, 2), ['toAdd', 'toRemove']);
  expectPlan(plan('UNIQUE', 'button', { held: ['A'], target: ['A'] }), { remove: ['A'] });
  expectPlan(plan('UNIQUE', 'button', { held: ['A', 'B'], target: ['A'] }), { remove: ['A', 'B'] });
  expectPlan(plan('UNIQUE', 'select', { held: ['A'], target: [] }), { remove: ['A'] });
  expectPlan(plan('UNIQUE', 'select', { held: ['A'], target: ['C'] }), { add: ['C'], remove: ['A'] });
  expectPlan(plan('UNIQUE', 'select', { held: ['A'], target: ['A'] }), { reason: 'no-change' });
  expectPlan(plan('UNIQUE', 'react', { held: ['B'], target: ['A'] }), { add: ['A'], remove: ['B'] });
  expectPlan(plan('UNIQUE', 'react', { held: ['A'], target: ['A'] }), { reason: 'no-change' });
  expectPlan(plan('UNIQUE', 'unreact', { held: ['A'], target: ['A'] }), { remove: ['A'] });
  expectPlan(plan('UNIQUE', 'unreact', { held: ['B'], target: ['A'] }), { reason: 'no-change' });
});

test('7c. planner: VERIFY and REVERSE', () => {
  expectPlan(plan('VERIFY', 'button', { held: ['A'], target: ['A'] }), { reason: 'already' });
  expectPlan(plan('VERIFY', 'button', { target: ['A'] }), { add: ['A'] });
  expectPlan(plan('VERIFY', 'select', { held: ['A'], target: ['A', 'B'] }), { add: ['B'] });
  expectPlan(plan('VERIFY', 'select', { held: ['A'], target: ['A'] }), { reason: 'already' });
  expectPlan(plan('VERIFY', 'react', { target: ['A'] }), { add: ['A'] });
  expectPlan(plan('VERIFY', 'react', { held: ['A'], target: ['A'] }), { reason: 'no-change' });
  expectPlan(plan('VERIFY', 'unreact', { held: ['A'], target: ['A'] }), { reason: 'no-change' });
  expectPlan(plan('REVERSE', 'button', { held: ['A'], target: ['A'] }), { remove: ['A'] });
  expectPlan(plan('REVERSE', 'button', { target: ['A'] }), { reason: 'not-held' });
  expectPlan(plan('REVERSE', 'select', { held: ['A'], target: ['A', 'B'] }), { remove: ['A'] });
  expectPlan(plan('REVERSE', 'select', { held: ['A'], target: ['B'] }), { reason: 'not-held' });
  expectPlan(plan('REVERSE', 'react', { held: ['A'], target: ['A'] }), { remove: ['A'] });
  expectPlan(plan('REVERSE', 'react', { target: ['A'] }), { reason: 'no-change' });
  expectPlan(plan('REVERSE', 'unreact', { target: ['A'] }), { add: ['A'] });
  expectPlan(plan('REVERSE', 'unreact', { held: ['A'], target: ['A'] }), { reason: 'no-change' });
});

test('7d. planner: targets outside the panel change nothing (R163)', () => {
  for (const type of rr.RR_TYPES) {
    for (const source of ['button', 'react', 'unreact']) {
      expectPlan(plan(type, source, { target: ['Z'], held: ['A'] }), { reason: 'no-change' });
    }
  }
  expectPlan(plan('NORMAL', 'button', { target: [] }), { reason: 'no-change' });
  expectPlan(plan('VERIFY', 'select', { target: [] }), { reason: 'no-change' });
  expectPlan(plan('REVERSE', 'select', { target: [] }), { reason: 'no-change' });
  // held roles outside the panel are ignored
  expectPlan(plan('UNIQUE', 'button', { held: ['Z'], target: ['A'] }), { add: ['A'] });
});

// ── 8-10. Payload builders ────────────────────────────────────────────────────

const mkItem = (i, over = {}) => ({
  id: `item${i}`, groupId: 'g1', customId: `rr:g1:${ROLE(i)}`, roleId: ROLE(i), label: `Role ${i}`, emoji: null, style: 1, position: i, description: null, ...over,
});
const mkGroup = (n, over = {}) => ({
  id: 'g1', title: 'Roles', description: null, type: 'NORMAL', layout: 'BUTTONS', maxRoles: null, plainText: false, messageContent: null,
  placeholder: null, items: Array.from({ length: n }, (_, i) => mkItem(i + 1)), ...over,
});
const serialise = (payload) => ({ components: payload.components.map((r) => r.toJSON()), embeds: payload.embeds.map((e) => e.toJSON()) });

test('8a. buildPanelPayload: BUTTONS', () => {
  const payload = rr.buildPanelPayload(mkGroup(7));
  const { components, embeds } = serialise(payload);
  assert.deepEqual(components.map((r) => r.components.length), [5, 2]);
  assert.equal(components[0].components[0].custom_id, `rr:g1:${ROLE(1)}`);
  assert.equal(embeds.length, 1);
  assert.equal(embeds[0].footer.text, 'Press a button again to remove its role');
  assert.deepEqual(payload.allowedMentions, { parse: [] });
  assert.equal(payload.content, null);
  const big = serialise(rr.buildPanelPayload(mkGroup(40)));
  assert.equal(big.components.length, 5); // 25 buttons at most
});

test('8b. buildPanelPayload: SELECT', () => {
  const normal = serialise(rr.buildPanelPayload(mkGroup(5, { layout: 'SELECT', maxRoles: 3 }))).components[0].components[0];
  assert.equal(normal.max_values, 3);
  assert.equal(normal.min_values, 0);
  assert.equal(normal.custom_id, 'rr-select:g1');
  assert.equal(normal.placeholder, 'Choose the roles you want');
  assert.equal(normal.options.length, 5);
  const unique = serialise(rr.buildPanelPayload(mkGroup(5, { layout: 'SELECT', type: 'UNIQUE' }))).components[0].components[0];
  assert.equal(unique.max_values, 1);
  const custom = serialise(rr.buildPanelPayload(mkGroup(2, { layout: 'SELECT', placeholder: 'Pick!' }))).components[0].components[0];
  assert.equal(custom.placeholder, 'Pick!');
  const described = rr.buildSelectRow(mkGroup(2, { layout: 'SELECT', items: [mkItem(1, { description: 'About news' }), mkItem(2)] })).toJSON();
  assert.equal(described.components[0].options[0].description, 'About news');
  assert.equal(described.components[0].options[1].description, undefined);
});

test('8c. buildPanelPayload: REACTIONS, text mode, limits and bad data', () => {
  const reactions = rr.buildPanelPayload(mkGroup(1, { layout: 'REACTIONS', items: [mkItem(1, { label: 'Verify', emoji: '✅' })] }));
  assert.deepEqual(reactions.components, []);
  assert.match(reactions.embeds[0].toJSON().description, /✅ — \*\*Verify\*\*/);
  const text = rr.buildPanelPayload(mkGroup(2, { plainText: true }));
  assert.deepEqual(text.embeds, []);
  assert.ok(text.content.length <= 2000);
  assert.ok(text.content.includes('-# Press a button again'));
  assert.ok(text.content.includes('**Roles**'));
  const withText = rr.buildPanelPayload(mkGroup(1, { plainText: true, messageContent: 'Hello everyone', layout: 'REACTIONS', items: [mkItem(1, { label: 'News', emoji: '🔔' })] }));
  assert.ok(withText.content.startsWith('Hello everyone'));
  assert.ok(withText.content.includes('🔔 — **News**'));
  assert.ok(!withText.content.includes('**Roles**'));
  const withContent = rr.buildPanelPayload(mkGroup(1, { messageContent: 'Top text' }));
  assert.equal(withContent.content, 'Top text');
  assert.equal(withContent.embeds.length, 1);
  const empty = rr.buildPanelPayload(mkGroup(0, { description: 'D'.repeat(4000) }));
  assert.ok(empty.embeds[0].toJSON().description.length <= 4096);
  assert.ok(empty.embeds[0].toJSON().description.includes('No roles have been added') || empty.embeds[0].toJSON().description.endsWith('…'));
  const hugeText = rr.buildPanelPayload(mkGroup(0, { plainText: true, description: 'D'.repeat(4000), title: 'T'.repeat(400) }));
  assert.ok(hugeText.content.length <= 2000);
  const bad = rr.buildPanelPayload(mkGroup(1, { items: [mkItem(1, { emoji: ':x:' })] }));
  assert.doesNotThrow(() => serialise(bad));
  assert.equal(serialise(bad).components[0].components[0].emoji, undefined);
  const badSelect = serialise(rr.buildPanelPayload(mkGroup(1, { layout: 'SELECT', items: [mkItem(1, { emoji: '100%' })] })));
  assert.equal(badSelect.components[0].components[0].options[0].emoji, undefined);
  const custom = serialise(rr.buildPanelPayload(mkGroup(1, { items: [mkItem(1, { emoji: '<a:wave:123456789012345678>' })] })));
  assert.deepEqual(custom.components[0].components[0].emoji, { id: '123456789012345678', name: 'wave', animated: true });
  assert.equal(rr.buildPanelPayload(mkGroup(0), { color: 0xff0000 }).embeds[0].toJSON().color, 0xff0000);
  assert.equal(rr.buildPanelPayload(mkGroup(0), { color: -5 }).embeds[0].toJSON().color, 0x5865f2);
  for (const type of rr.RR_TYPES) {
    for (const layout of rr.RR_LAYOUTS) {
      const p = rr.buildPanelPayload(mkGroup(3, { type, layout, maxRoles: type === 'NORMAL' ? 2 : null, items: [1, 2, 3].map((i) => mkItem(i, { emoji: '🔔' })) }));
      assert.deepEqual(p.allowedMentions, { parse: [] });
      assert.doesNotThrow(() => serialise(p), `${type}/${layout}`);
    }
  }
  assert.match(rr.panelFooter(mkGroup(0, { maxRoles: 2 })), /Max 2 role\(s\)$/);
  assert.equal(rr.panelFooter(mkGroup(0, { type: 'REVERSE', layout: 'SELECT' })), 'Picking a role removes it');
});

test('9. buildSelectRow marks selected roles as default', () => {
  const group = mkGroup(3, { layout: 'SELECT' });
  assert.equal(rr.buildSelectRow(mkGroup(0, { layout: 'SELECT' })), null);
  const row = rr.buildSelectRow(group, { selectedRoleIds: [ROLE(1)], placeholder: 'Adjust' }).toJSON().components[0];
  assert.deepEqual(row.options.map((o) => o.default), [true, false, false]);
  assert.equal(row.placeholder, 'Adjust');
  // more defaults than max_values would be rejected by Discord
  const capped = rr.buildSelectRow(mkGroup(3, { layout: 'SELECT', maxRoles: 1 }), { selectedRoleIds: [ROLE(1), ROLE(2)] }).toJSON().components[0];
  assert.equal(capped.options.filter((o) => o.default).length, 1);
});

test('10. describeDiscordError', () => {
  const expected = {
    10003: 'the channel no longer exists', 10008: 'the message no longer exists', 10011: 'that role no longer exists',
    10014: "I can't use that emoji", 30010: 'that message already has 20 different reactions', 50001: "I can't see that channel",
    50005: "I can't edit a message someone else sent", 50013: "I'm missing permissions there", 50035: 'Discord rejected the panel content',
    50083: 'the thread is archived',
  };
  for (const [code, text] of Object.entries(expected)) {
    assert.equal(rr.describeDiscordError(discordErr(Number(code))), text);
    assert.equal(rr.describeDiscordError({ code: Number(code) }), text);
  }
  assert.equal(rr.describeDiscordError(discordErr(12345)), 'Discord error 12345');
  assert.equal(rr.describeDiscordError(new Error('x')), 'Discord error unknown');
  assert.equal(rr.describeDiscordError(null), 'Discord error unknown');
  assert.equal(rr.discordErrorCode({ code: 'ECONNRESET' }), null);
});

test('11. reaction suppression', () => {
  rr.suppressReactionRemoval('m1', 'u1', 'k');
  assert.equal(rr.consumeReactionSuppression('m1', 'u1', 'k'), true);
  assert.equal(rr.consumeReactionSuppression('m1', 'u1', 'k'), false);
  assert.equal(rr.consumeReactionSuppression('m1', 'u2', 'k'), false);
  const t0 = Date.now();
  const now = mock.method(Date, 'now', () => t0);
  try {
    rr.suppressReactionRemoval('m2', 'u1', 'k');
    now.mock.mockImplementation(() => t0 + rr.SUPPRESS_TTL_MS + 1);
    assert.equal(rr.consumeReactionSuppression('m2', 'u1', 'k'), false);
    now.mock.mockImplementation(() => t0);
    rr.suppressReactionRemoval('m3', 'u1', 'k');
    now.mock.mockImplementation(() => t0 + rr.SUPPRESS_TTL_MS - 1);
    assert.equal(rr.consumeReactionSuppression('m3', 'u1', 'k'), true);
  } finally {
    now.mock.restore();
  }
});

test('12. hook filters follow the panel index', () => {
  const g = G(70);
  const ctx = (messageId, guildId = g) => ({ guildId, message: { id: messageId } });
  rr.__resetPanelIndex();
  assert.equal(rr.reactionHookFilter({ guildId: null, message: { id: 'm' } }), false);
  assert.equal(rr.reactionHookFilter({ guildId: g, message: null }), false);
  assert.equal(rr.reactionHookFilter(ctx('m1')), true, 'unknown memo -> run loads it');
  assert.equal(rr.messageDeleteFilter(ctx('m1')), true);
  rr.__setPanelIndex(g, []);
  assert.equal(rr.reactionHookFilter(ctx('m1')), false);
  assert.equal(rr.messageDeleteFilter(ctx('m1')), false);
  rr.__setPanelIndex(g, [
    { id: 'p1', channelId: 'c', messageId: 'rm', title: 'T', type: 'NORMAL', layout: 'REACTIONS', maxRoles: null, attached: false, items: [] },
    { id: 'p2', channelId: 'c', messageId: 'bm', title: 'T', type: 'NORMAL', layout: 'BUTTONS', maxRoles: null, attached: false, items: [] },
  ]);
  assert.equal(rr.reactionHookFilter(ctx('rm')), true);
  assert.equal(rr.reactionHookFilter(ctx('bm')), false);
  assert.equal(rr.reactionHookFilter(ctx('other')), false);
  assert.equal(rr.messageDeleteFilter(ctx('bm')), true);
  assert.equal(rr.messageDeleteFilter(ctx('other')), false);
  const t0 = Date.now();
  const now = mock.method(Date, 'now', () => t0 + rr.INDEX_MEMO_TTL_MS + 1);
  try {
    assert.equal(rr.reactionHookFilter(ctx('other')), true, 'a stale memo makes run reload it');
    assert.equal(rr.peekPanelIndex(g).fresh, false);
  } finally {
    now.mock.restore();
  }
  rr.__resetPanelIndex();
});

// ── 13. List pages ────────────────────────────────────────────────────────────

test('13. buildListPages paginates and stays under 6000 characters', () => {
  const panels = Array.from({ length: 25 }, (_, i) => ({
    id: `p${i}`, title: `${i}-${'T'.repeat(256)}`, type: 'UNIQUE', layout: 'REACTIONS', maxRoles: 25, attached: true, plainText: true,
    channelId: sid('3', 1), messageId: sid('5', i + 1), itemCount: 20, messageMissingAt: new Date(),
  }));
  const pages = cmd.buildListPages(panels, { guildId: G(1), total: panels.length, settings: { embedColor: 0x123456 } });
  assert.equal(pages.length, 3);
  for (const page of pages) {
    const json = page.toJSON();
    assert.ok(json.fields.length <= 10);
    const total = (json.title?.length ?? 0) + json.fields.reduce((n, f) => n + f.name.length + f.value.length, 0) + (json.footer?.text.length ?? 0);
    assert.ok(total <= 6000, `page has ${total} characters`);
    for (const f of json.fields) assert.ok(f.name.length <= 256 && f.value.length <= 1024);
    assert.equal(json.color, 0x123456);
  }
  const first = pages[0].toJSON().fields[0];
  assert.ok(first.name.startsWith('1. '));
  assert.ok(first.value.includes('⚠️ message deleted — run /reactionrole repost'));
  assert.ok(first.value.includes('attached') && first.value.includes('text only') && first.value.includes('max 25'));
  assert.ok(pages[2].toJSON().fields[0].name.startsWith('21. '));
  assert.deepEqual(cmd.buildListPages([], { guildId: G(1) }), []);
});

// ── 14. The command payload ───────────────────────────────────────────────────

const charCount = (node) => {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
};

test('14. command payload and exports', () => {
  const json = buildCommandPayload(cmd);
  assert.deepEqual(json.options.map((o) => o.name), ['create', 'attach', 'add', 'remove', 'edit', 'repost', 'delete', 'list']);
  for (const sub of json.options) {
    assert.ok((sub.options ?? []).length <= 12, `${sub.name}: ${sub.options?.length} options`);
    assert.ok(sub.description.length >= 1 && sub.description.length <= 100);
    for (const o of sub.options ?? []) assert.ok(o.description.length >= 1 && o.description.length <= 100, `${sub.name}.${o.name}`);
  }
  assert.ok(charCount(json) <= 2200, `payload is ${charCount(json)} characters`);
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.deepEqual(json.contexts, [0]);
  assert.equal(cmd.module, 'reactionroles');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(typeof cmd.autocomplete, 'function');
  assert.equal(typeof cmd.execute, 'function');
  const opts = (sub) => Object.fromEntries(json.options.find((o) => o.name === sub).options.map((o) => [o.name, o]));
  assert.equal(opts('create').channel.required, true);
  assert.equal(opts('create').title.max_length, 256);
  assert.equal(opts('create').description.max_length, 4000);
  assert.equal(opts('create').placeholder.max_length, 150);
  assert.equal(opts('add').panel.autocomplete, true);
  assert.equal(opts('add').description.max_length, 100);
  assert.equal(opts('remove').item.autocomplete, true);
  assert.equal(opts('edit').max_roles.min_value, 0);
});

// ── 15. The plugin ────────────────────────────────────────────────────────────

test('15. plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('reaction-roles')), []);
  const p = plugins.find((x) => x.id === 'reaction-roles');
  assert.ok(p);
  const { plugin } = pluginMod;
  assert.equal(plugin.id, 'reaction-roles');
  assert.equal(plugin.module, 'reactionroles');
  for (const h of plugin.hooks) assert.equal(h.id, 'reaction-roles');
  assert.deepEqual(plugin.hooks.map((h) => h.event).sort(), [
    'messageDelete', 'messageReactionAdd', 'messageReactionRemove', 'messageReactionRemoveAll', 'messageReactionRemoveEmoji', 'roleDelete',
  ]);
  assert.equal(plugin.hooks.find((h) => h.event === 'messageDelete').always, true);
  assert.equal(plugin.hooks.find((h) => h.event === 'roleDelete').always, true);
  assert.deepEqual(plugin.components.map((c) => `${c.type}:${c.prefix}`).sort(), ['button:rr', 'select:rr-select']);
  assert.equal(plugin.components.find((c) => c.prefix === 'rr').handler, rr.handleRoleButton);
  assert.equal(plugin.components.find((c) => c.prefix === 'rr-select').handler, rr.handleRoleSelect);
  assert.deepEqual(plugin.jobs, []);
});

// ── 16. Static checks ─────────────────────────────────────────────────────────

test('16. static checks on the owned files', () => {
  const service = readFileSync(new URL('../src/services/reactionRoleService.js', import.meta.url), 'utf8');
  const command = readFileSync(new URL('../src/commands/admin/reactionrole.js', import.meta.url), 'utf8');
  const plugin = readFileSync(new URL('../src/plugins/reaction-roles.js', import.meta.url), 'utf8');
  for (const [name, text] of [['service', service], ['command', command], ['plugin', plugin]]) {
    assert.ok(!/roles\.(add|remove)\(\s*\[/.test(text), `${name}: array-form role call`);
    assert.ok(!/roles\.set\(/.test(text), `${name}: roles.set`);
    assert.ok(!text.includes(':settings'), `${name}: touches the settings cache key`);
    assert.ok(!text.includes("cache.del('guild:"), `${name}: deletes a guild cache key directly`);
    assert.ok(!/prisma\.guildSettings/.test(text), `${name}: writes GuildSettings`);
    assert.ok(!/\bconsole\./.test(text), `${name}: console`);
  }
  assert.ok(!/export (async )?function registerHandlers/.test(service));
  assert.equal(rr.registerHandlers, undefined);
  assert.ok(!/database\/client|services\/redis/.test(command), 'the command must not import Prisma or Redis');
});

// ── Role changes (fake Discord objects, no database) ──────────────────────────

test('17. applyRoleChange adds first, one id at a time, and stops on a failed add', async () => {
  const member = fakeMember(USER(1), [ROLE(2)]);
  const res = await rr.applyRoleChange(member, { toAdd: [ROLE(1)], toRemove: [ROLE(2)] }, { reason: 'why' });
  assert.deepEqual(member.calls, [['add', ROLE(1), 'why'], ['remove', ROLE(2), 'why']]);
  assert.deepEqual({ added: res.added, removed: res.removed, failed: res.failed }, { added: [ROLE(1)], removed: [ROLE(2)], failed: null });

  const stuck = fakeMember(USER(2), [ROLE(2)]);
  stuck.failRole.set(ROLE(1), 50013);
  const failed = await rr.applyRoleChange(stuck, { toAdd: [ROLE(1)], toRemove: [ROLE(2)] }, { reason: 'x' });
  assert.deepEqual(failed.failed, { roleId: ROLE(1), op: 'add', code: 50013 });
  assert.deepEqual(stuck.calls, [], 'a failed swap keeps the old role (R162)');
  assert.ok(stuck.server.has(ROLE(2)));

  const idem = fakeMember(USER(3), [ROLE(1)]);
  const same = await rr.applyRoleChange(idem, { toAdd: [ROLE(1)], toRemove: [ROLE(2)] }, { reason: 'x' });
  assert.deepEqual(idem.calls, []);
  assert.deepEqual(same.added.concat(same.removed), []);
});

test('18. checkRolesUsable', () => {
  const guild = fakeGuild(G(2), { roles: [fakeRole(ROLE(1), 'ok'), fakeRole(ROLE(2), 'high', { editable: false })] });
  assert.deepEqual(rr.checkRolesUsable(guild, [ROLE(1), ROLE(2), ROLE(3)]), { missing: [ROLE(3)], blocked: [ROLE(2)], noManageRoles: false });
  guild.members.me.permissions.has = () => false;
  const none = rr.checkRolesUsable(guild, [ROLE(1)]);
  assert.deepEqual(none, { missing: [], blocked: [ROLE(1)], noManageRoles: true });
});

/** A reaction hook context for processReaction (primed index, no database). */
function reactionWorld(n, { type = 'NORMAL', maxRoles = null, roles = 4, deny = [] } = {}) {
  const guildId = G(n);
  const roleList = Array.from({ length: roles }, (_, i) => fakeRole(ROLE(n * 10 + i + 1), `Role ${i + 1}`));
  const channel = fakeChannel(sid('3', n), { deny });
  const guild = fakeGuild(guildId, { roles: roleList, channels: [channel] });
  const message = fakeMessage(sid('5', 9000 + n), channel);
  channel._messages.set(message.id, message);
  const emojis = ['🔔', '🎉', '🎮', '⭐'];
  const items = roleList.map((r, i) => ({ roleId: r.id, emoji: emojis[i], key: rr.emojiKeyOfStored(emojis[i]) }));
  const panel = { id: `panel${n}`, channelId: channel.id, messageId: message.id, title: 'Panel', type, layout: 'REACTIONS', maxRoles, attached: false, items };
  rr.__setPanelIndex(guildId, [panel]);
  const world = { guildId, guild, channel, message, panel, roles: roleList, emojis, items };
  world.ctx = (member, emoji, { missingMember = false } = {}) => {
    const reaction = fakeReaction(message, emoji, false);
    message.reactions.cache.set(reaction.emoji.id ?? reaction.emoji.name, reaction);
    return {
      guildId, guild, client: { user: { id: BOT } }, message, reaction, user: { id: member.id }, emojiKey: reaction.emoji.id ?? reaction.emoji.name,
      getMember: async () => (missingMember ? null : member), ensureMessage: async () => message,
    };
  };
  return world;
}

test('19a. reaction hook: NORMAL react adds the role and unreact removes it', async () => {
  const w = reactionWorld(11);
  const member = fakeMember(USER(1));
  await rr.processReaction(w.ctx(member, '🔔'), 'react');
  assert.deepEqual(member.calls.map((c) => c.slice(0, 2)), [['add', w.roles[0].id]]);
  assert.match(member.calls[0][2], /^Reaction role: Panel/);
  await rr.processReaction(w.ctx(member, '🔔'), 'unreact');
  assert.deepEqual(member.calls.map((c) => c.slice(0, 2)), [['add', w.roles[0].id], ['remove', w.roles[0].id]]);
  // a foreign emoji and a foreign message are left alone
  await rr.processReaction(w.ctx(member, '🍕'), 'react');
  const ctx = w.ctx(member, '🔔');
  ctx.message = { id: sid('5', 1) };
  await rr.processReaction(ctx, 'react');
  assert.equal(member.calls.length, 2);
  // the user left
  await rr.processReaction(w.ctx(member, '🔔', { missingMember: true }), 'react');
  assert.equal(member.calls.length, 2);
  rr.__resetPanelIndex();
});

test('19b. reaction hook: UNIQUE swaps roles, removes the old reaction and ignores that removal', async () => {
  const w = reactionWorld(12, { type: 'UNIQUE' });
  const member = fakeMember(USER(2), [w.roles[1].id]);
  const old = fakeReaction(w.message, '🎉', false);
  w.message.reactions.cache.set('🎉', old);
  await rr.processReaction(w.ctx(member, '🔔'), 'react');
  assert.deepEqual(member.calls.map((c) => c.slice(0, 2)), [['add', w.roles[0].id], ['remove', w.roles[1].id]], 'adds before removes');
  assert.deepEqual(old.removedUsers, [member.id], 'the swapped-out reaction is taken back');
  const before = member.calls.length;
  await rr.processReaction(w.ctx(member, '🎉'), 'unreact'); // the bot's own removal: no role logic
  assert.equal(member.calls.length, before);
  // a real un-react afterwards still works
  await rr.processReaction(w.ctx(member, '🔔'), 'unreact');
  assert.equal(member.calls.length, before + 1);
  rr.__resetPanelIndex();
});

test('19c. reaction hook: a failed UNIQUE add keeps the old role', async () => {
  const w = reactionWorld(13, { type: 'UNIQUE' });
  const member = fakeMember(USER(3), [w.roles[1].id]);
  member.failRole.set(w.roles[0].id, 50013);
  await rr.processReaction(w.ctx(member, '🔔'), 'react');
  assert.deepEqual(member.calls, []);
  assert.ok(member.server.has(w.roles[1].id));
  rr.__resetPanelIndex();
});

test('19d. reaction hook: the role cap takes the reaction back', async () => {
  const w = reactionWorld(14, { maxRoles: 1 });
  const member = fakeMember(USER(4), [w.roles[1].id]);
  const ctx = w.ctx(member, '🔔');
  await rr.processReaction(ctx, 'react');
  assert.deepEqual(member.calls, []);
  assert.deepEqual(ctx.reaction.removedUsers, [member.id]);
  assert.equal(rr.consumeReactionSuppression(w.message.id, member.id, '🔔'), true, 'the removal is marked as ours');
  // without Manage Messages the reaction stays and nothing else happens
  const w2 = reactionWorld(15, { maxRoles: 1, deny: ['ManageMessages'] });
  const member2 = fakeMember(USER(5), [w2.roles[1].id]);
  const ctx2 = w2.ctx(member2, '🔔');
  await rr.processReaction(ctx2, 'react');
  assert.deepEqual(ctx2.reaction.removedUsers, []);
  assert.deepEqual(member2.calls, []);
  rr.__resetPanelIndex();
});

test('19e. reaction hook: VERIFY and REVERSE semantics', async () => {
  const v = reactionWorld(16, { type: 'VERIFY' });
  const member = fakeMember(USER(6));
  await rr.processReaction(v.ctx(member, '🔔'), 'react');
  await rr.processReaction(v.ctx(member, '🔔'), 'unreact');
  assert.deepEqual(member.calls.map((c) => c[0]), ['add'], 'unreacting never removes a VERIFY role');
  const r = reactionWorld(17, { type: 'REVERSE' });
  const m2 = fakeMember(USER(7), [r.roles[0].id]);
  await rr.processReaction(r.ctx(m2, '🔔'), 'react');
  await rr.processReaction(r.ctx(m2, '🔔'), 'unreact');
  assert.deepEqual(m2.calls.map((c) => c[0]), ['remove', 'add'], 'reacting removes, removing the reaction gives it back');
  rr.__resetPanelIndex();
});

test('19f. reaction hook: unmanageable roles and unverifiable deletions change nothing (rule 20)', async () => {
  const w = reactionWorld(18);
  w.roles[0].editable = false;
  const member = fakeMember(USER(8));
  await rr.processReaction(w.ctx(member, '🔔'), 'react');
  assert.deepEqual(member.calls, []);
  // the role is missing from the cache and REST cannot tell: not pruned, not assigned
  const w2 = reactionWorld(19);
  w2.guild.roles.cache.delete(w2.roles[0].id);
  w2.guild.roles.fetch = async () => { throw new Error('network down'); };
  const m2 = fakeMember(USER(9));
  await rr.processReaction(w2.ctx(m2, '🔔'), 'react');
  assert.deepEqual(m2.calls, []);
  // the panel index is unknown and cannot be loaded: fail closed
  rr.__resetPanelIndex();
  const orphan = reactionWorld(20);
  rr.__resetPanelIndex();
  const m3 = fakeMember(USER(10));
  await rr.processReaction(orphan.ctx(m3, '🔔'), 'react');
  assert.deepEqual(m3.calls, []);
  rr.__resetPanelIndex();
});

test('19g. hook bodies return immediately and never throw', async () => {
  const ctx = { guildId: G(21), message: { id: 'x' }, user: { id: 'u' }, emojiKey: '🔔', getMember: async () => { throw new Error('boom'); } };
  rr.__setPanelIndex(G(21), []);
  assert.equal(rr.onReactionAdd(ctx), undefined);
  assert.equal(rr.onReactionRemove(ctx), undefined);
  assert.equal(rr.onReactionRemoveAll(ctx), undefined);
  assert.equal(rr.onReactionRemoveEmoji(ctx), undefined);
  await rr.__idle();
  rr.__resetPanelIndex();
});

test('19h. clear-reaction hooks re-add the bot reactions', async () => {
  const w = reactionWorld(22);
  rr.onReactionRemoveAll({ guildId: w.guildId, message: w.message, ensureMessage: async () => w.message });
  await rr.__idle();
  assert.deepEqual(w.message.reacted, w.emojis, 'every item emoji is back, in order');
  rr.onReactionRemoveAll({ guildId: w.guildId, message: w.message, ensureMessage: async () => w.message });
  await rr.__idle();
  assert.equal(w.message.reacted.length, 4, 'throttled and already present');
  const w2 = reactionWorld(23);
  rr.onReactionRemoveEmoji({ guildId: w2.guildId, message: w2.message, emojiKey: '🎉', ensureMessage: async () => w2.message });
  rr.onReactionRemoveEmoji({ guildId: w2.guildId, message: w2.message, emojiKey: '🍕', ensureMessage: async () => w2.message });
  await rr.__idle();
  assert.deepEqual(w2.message.reacted, ['🎉']);
  rr.__resetPanelIndex();
});

test('20. reaction helpers: ensurePanelReactions, removePanelReaction, removeOwnReactions', async () => {
  const channel = fakeChannel(sid('3', 40));
  const guild = fakeGuild(G(3), { channels: [channel] });
  const message = fakeMessage(nextId(), channel);
  channel._messages.set(message.id, message);
  const bad = await rr.ensurePanelReactions(message, [{ emoji: '🔔' }, { emoji: ':x:' }, { emoji: '🎉' }]);
  assert.equal(bad.added, 2);
  assert.deepEqual(message.reacted, ['🔔', '🎉']);
  message.reactError = discordErr(10014);
  const failed = await rr.ensurePanelReactions(message, [{ emoji: '⭐' }]);
  assert.deepEqual(failed.failed, [{ emoji: '⭐', code: 10014, reason: "I can't use that emoji" }]);
  message.reactError = null;
  const client = { user: { id: BOT }, guilds: { cache: new Collection([[guild.id, guild]]) } };
  const bell = message.reactions.cache.get('🔔');
  assert.deepEqual(await rr.removePanelReaction(message, '🔔', { client }), { ok: true, all: true });
  assert.equal(bell.removedAll, true);
  const party = message.reactions.cache.get('🎉');
  channel.permissionsFor = () => ADMIN_BITS(['ManageMessages']);
  assert.deepEqual(await rr.removePanelReaction(message, '🎉', { client }), { ok: true, all: false });
  assert.deepEqual(party.removedUsers, [BOT]);
  assert.deepEqual(await rr.removePanelReaction(message, '🍕', { client }), { ok: true, all: false });
  await rr.removeOwnReactions(message, [{ emoji: '🎉' }], client);
  assert.deepEqual(party.removedUsers, [BOT, BOT]);
});

// ── Database cases ────────────────────────────────────────────────────────────

const group0 = (guildId, over = {}) => ({
  id: randomUUID(), guildId, channelId: sid('3', 1), messageId: sid('5', ++seq), title: 'Panel', description: null, type: 'NORMAL', layout: 'BUTTONS',
  maxRoles: null, plainText: false, messageContent: null, attached: false, placeholder: null, items: [], ...over,
});

async function seedPanel(guildId, { roles = [], ...over } = {}) {
  await ensureGuildSettings(guildId);
  const id = over.id ?? randomUUID();
  const items = roles.map((r, i) => rr.draftItem(id, { roleId: r.id ?? r, label: r.label ?? `Role ${i + 1}`, emoji: r.emoji ?? null, style: 1, description: r.description ?? null }, i));
  const draft = group0(guildId, { id, items, ...over });
  await rr.createPanel(draft);
  return draft;
}

test('D1. createPanel, getPanel, addItem, removeItem and the unique customId', { skip: skipDb }, async () => {
  const guildId = G(30);
  const draft = await seedPanel(guildId, { roles: [ROLE(1), ROLE(2), ROLE(3)], title: 'First' });
  const got = await rr.getPanel(guildId, draft.id);
  assert.equal(got.items.length, 3);
  assert.deepEqual(got.items.map((i) => i.position), [0, 1, 2]);
  assert.equal(got.items[0].customId, `rr:${draft.id}:${ROLE(1)}`);
  assert.equal(got.messageMissingAt, null);
  assert.equal(await rr.getPanel(G(31), draft.id), null, 'scoped to the guild');
  assert.equal(await rr.countPanels(guildId), 1);
  assert.deepEqual(await rr.findPanelByMessage(guildId, draft.messageId), { id: draft.id, title: 'First' });
  assert.equal(await rr.findPanelByMessage(guildId, sid('5', 1)), null);

  const added = await rr.addItem(got, { roleId: ROLE(4), label: 'Four', emoji: '🔔', style: 3, description: 'About four' });
  assert.equal(added.position, 3);
  assert.equal(added.description, 'About four');
  await assert.rejects(() => rr.addItem(got, { roleId: ROLE(4), label: 'Dup' }), (err) => err.code === 'P2002');
  const fresh = await rr.getPanel(guildId, draft.id);
  await rr.removeItem(fresh, fresh.items[1].id);
  const after = await rr.getPanel(guildId, draft.id);
  assert.deepEqual(after.items.map((i) => [i.roleId, i.position]), [[ROLE(1), 0], [ROLE(3), 1], [ROLE(4), 2]]);

  await assert.rejects(() => rr.createPanel({ ...draft, id: randomUUID(), messageId: sid('5', ++seq), items: [rr.draftItem(draft.id, { roleId: ROLE(1), label: 'x' }, 0)] }));
  assert.equal(await rr.countPanels(guildId), 1, 'a failed create leaves no group behind (transaction)');
});

test('D2. updatePanel, movePanel, deletePanel are scoped to the guild; messageMissingAt set and clear', { skip: skipDb }, async () => {
  const guildId = G(32);
  const other = G(33);
  const draft = await seedPanel(guildId, { roles: [ROLE(1)] });
  assert.equal(await rr.updatePanel(other, draft.id, { title: 'Hacked' }), null);
  assert.equal((await rr.getPanel(guildId, draft.id)).title, 'Panel');
  const updated = await rr.updatePanel(guildId, draft.id, { title: 'New', description: null, placeholder: 'Pick', maxRoles: 3, plainText: true, messageContent: 'Hi' });
  assert.deepEqual([updated.title, updated.placeholder, updated.maxRoles, updated.plainText, updated.messageContent], ['New', 'Pick', 3, true, 'Hi']);
  assert.equal(updated.items.length, 1);

  const guild = fakeGuild(guildId);
  await rr.markPanelMissing(guild, updated);
  const flagged = await rr.getPanel(guildId, draft.id);
  assert.ok(flagged.messageMissingAt instanceof Date);
  const firstFlag = flagged.messageMissingAt.getTime();
  await new Promise((r) => setTimeout(r, 15));
  await rr.markPanelMissing(guild, flagged);
  assert.equal((await rr.getPanel(guildId, draft.id)).messageMissingAt.getTime(), firstFlag, 'the first marker is kept');
  await rr.markPanelMissing({ ...guild, available: false }, { id: draft.id });
  assert.equal((await rr.getPanel(guildId, draft.id)).messageMissingAt.getTime(), firstFlag);

  await rr.movePanel(other, draft.id, { channelId: sid('3', 7), messageId: sid('5', 77), attached: true });
  assert.equal((await rr.getPanel(guildId, draft.id)).attached, false, 'another guild cannot move it');
  await rr.movePanel(guildId, draft.id, { channelId: sid('3', 7), messageId: sid('5', 77), attached: true });
  const moved = await rr.getPanel(guildId, draft.id);
  assert.deepEqual([moved.channelId, moved.messageId, moved.attached, moved.messageMissingAt], [sid('3', 7), sid('5', 77), true, null]);

  await rr.deletePanel(other, draft.id);
  assert.ok(await rr.getPanel(guildId, draft.id));
  await rr.deletePanel(guildId, draft.id);
  assert.equal(await rr.getPanel(guildId, draft.id), null);
  assert.equal(await prisma.reactionRoleItem.count({ where: { groupId: draft.id } }), 0, 'items cascade');
});

test('D3. searchPanels, searchItems and listPanels', { skip: skipDb }, async () => {
  const guildId = G(34);
  await ensureGuildSettings(guildId);
  const made = [];
  for (let i = 0; i < 30; i++) {
    made.push(await seedPanel(guildId, { title: i === 3 ? 'Special NEWS panel' : `Panel ${i}`, roles: i === 3 ? [ROLE(1), ROLE(2)] : [] }));
    await new Promise((r) => setTimeout(r, 2));
  }
  const all = await rr.searchPanels(guildId, '');
  assert.equal(all.length, 25);
  assert.equal(all[0].id, made[29].id, 'newest first');
  const byTitle = await rr.searchPanels(guildId, 'news');
  assert.deepEqual(byTitle.map((r) => [r.id, r.itemCount, r.layout]), [[made[3].id, 2, 'BUTTONS']]);
  assert.equal((await rr.searchPanels(guildId, made[7].id)).length, 1, 'by id');
  assert.equal((await rr.searchPanels(guildId, made[8].messageId)).length, 1, 'by message id');
  assert.equal((await rr.searchPanels(guildId, 'zzz-nothing')).length, 0);
  assert.equal((await rr.searchPanels(G(35), '')).length, 0, 'scoped to the guild');
  const old = await rr.searchPanels(guildId, 'Panel 0');
  assert.ok(old.some((r) => r.id === made[0].id), 'an old panel is found by a DB-side filter (R062)');

  const full = await rr.getPanel(guildId, made[3].id);
  const labels = full.items.map((i) => i.label);
  assert.equal((await rr.searchItems(guildId, made[3].id, '', [])).length, 2);
  assert.deepEqual((await rr.searchItems(guildId, made[3].id, 'ROLE 2', [])).map((i) => i.label), [labels[1]]);
  assert.deepEqual((await rr.searchItems(guildId, made[3].id, 'zzz', [ROLE(1)])).map((i) => i.roleId), [ROLE(1)], 'role-name matches by id');
  assert.deepEqual((await rr.searchItems(guildId, made[3].id, ROLE(2), [])).map((i) => i.roleId), [ROLE(2)]);
  assert.equal((await rr.searchItems(G(35), made[3].id, '', [])).length, 0, 'scoped to the guild');
  assert.deepEqual(Object.keys((await rr.searchItems(guildId, made[3].id, '', []))[0]).sort(), ['emoji', 'id', 'label', 'roleId']);

  const listed = await rr.listPanels(guildId);
  assert.equal(listed.length, 30);
  assert.equal(listed[0].id, made[29].id);
  assert.equal(listed.find((p) => p.id === made[3].id).itemCount, 2);
  assert.ok('messageMissingAt' in listed[0] && 'plainText' in listed[0]);
});

test('D4. the panel index loads from Postgres with Redis down and survives an invalidation race', { skip: skipDb }, async () => {
  const guildId = G(36);
  rr.__resetPanelIndex();
  const a = await seedPanel(guildId, { layout: 'REACTIONS', roles: [{ id: ROLE(1), emoji: '❤️' }, { id: ROLE(2), emoji: '<:pog:123456789012345678>' }] });
  assert.equal(rr.peekPanelIndex(guildId), null);
  const index = await rr.getPanelIndex(guildId);
  const panel = index.get(a.messageId);
  assert.equal(panel.layout, 'REACTIONS');
  assert.deepEqual(panel.items.map((i) => i.key), ['❤', '123456789012345678']);
  assert.equal(rr.peekPanelIndex(guildId).fresh, true);
  assert.equal(await rr.getPanelIndex(guildId), index, 'served from the memo');

  await rr.invalidatePanelIndex(guildId);
  assert.equal(rr.peekPanelIndex(guildId), null);
  const loading = rr.getPanelIndex(guildId);
  const second = rr.getPanelIndex(guildId);
  await rr.invalidatePanelIndex(guildId); // while the load is in flight
  const [afterLoad, again] = await Promise.all([loading, second]);
  assert.ok(afterLoad.has(a.messageId) && again.has(a.messageId));
  assert.equal(rr.peekPanelIndex(guildId), null, 'a load that was invalidated meanwhile is not memoised');
  assert.ok((await rr.getPanelIndex(guildId)).has(a.messageId));
  assert.ok(rr.peekPanelIndex(guildId));
  rr.__resetPanelIndex();
});

test('D5. pruneItemsForRole removes items across panels, renumbers and re-renders', { skip: skipDb }, async () => {
  const guildId = G(37);
  const channel = fakeChannel(sid('3', 37));
  const guild = fakeGuild(guildId, { roles: [fakeRole(ROLE(2), 'two')], channels: [channel] });
  const makePanel = async (layout, roles) => {
    const draft = await seedPanel(guildId, { layout, roles, channelId: channel.id });
    const message = fakeMessage(draft.messageId, channel);
    channel._messages.set(message.id, message);
    return { draft, message };
  };
  const p1 = await makePanel('BUTTONS', [ROLE(1), ROLE(2), ROLE(3)]);
  const p2 = await makePanel('REACTIONS', [{ id: ROLE(2), emoji: '🔔' }, { id: ROLE(4), emoji: '🎉' }]);
  p2.message.reactions.cache.set('🔔', fakeReaction(p2.message, '🔔'));
  const p3 = await makePanel('BUTTONS', [ROLE(5)]);
  const client = { user: { id: BOT } };

  assert.equal(await rr.pruneItemsForRole(guild, ROLE(2), { client }), 2);
  await rr.__idle();
  const a = await rr.getPanel(guildId, p1.draft.id);
  assert.deepEqual(a.items.map((i) => [i.roleId, i.position]), [[ROLE(1), 0], [ROLE(3), 1]]);
  const b = await rr.getPanel(guildId, p2.draft.id);
  assert.deepEqual(b.items.map((i) => [i.roleId, i.position]), [[ROLE(4), 0]]);
  assert.equal(p1.message.edits.length, 1, 'the panel message was re-rendered');
  assert.equal(p1.message.edits[0].components[0].toJSON().components.length, 2);
  assert.equal(p3.message.edits.length, 0, 'an unrelated panel is not touched');
  assert.equal(p2.message.reactions.cache.has('🔔'), false, 'the reaction of the removed item is cleared');
  assert.equal(await rr.pruneItemsForRole(guild, ROLE(2), { client }), 0);
  assert.equal(await rr.pruneItemsForRole({ ...guild, available: false }, ROLE(1), { client }), 0, 'rule 20: never while the guild is unavailable');
  assert.equal(await rr.pruneItemsForRole(fakeGuild(G(38)), ROLE(1), { client }), 0, 'other guilds are not touched');
  assert.equal((await rr.getPanel(guildId, p1.draft.id)).items.length, 2);
  // the hook form
  await rr.onRoleDelete({ guild, role: { id: ROLE(1) }, client });
  assert.equal((await rr.getPanel(guildId, p1.draft.id)).items.length, 1);
  await rr.__idle();
});

test('D6. syncPanel, fetchPanelMessage and the missing marker', { skip: skipDb }, async () => {
  const guildId = G(39);
  const channel = fakeChannel(sid('3', 39));
  const guild = fakeGuild(guildId, { channels: [channel] });
  const draft = await seedPanel(guildId, { roles: [ROLE(1)], channelId: channel.id, title: 'Sync me' });
  const message = fakeMessage(draft.messageId, channel);
  channel._messages.set(message.id, message);

  const ok = await rr.syncPanel(guild, draft.id, { settings: { embedColor: 0xabcdef } });
  assert.deepEqual(ok, { status: 'ok' });
  assert.equal(message.edits[0].embeds[0].toJSON().color, 0xabcdef);
  assert.deepEqual(message.edits[0].allowedMentions, { parse: [] });
  assert.deepEqual(await rr.syncPanel(guild, randomUUID()), { status: 'skipped' });

  message.editError = discordErr(50035);
  assert.deepEqual(await rr.syncPanel(guild, draft.id), { status: 'failed', code: 50035, reason: 'Discord rejected the panel content' });
  message.editError = discordErr(50005);
  assert.equal((await rr.syncPanel(guild, draft.id)).status, 'failed');
  message.editError = discordErr(10008);
  assert.equal((await rr.syncPanel(guild, draft.id)).status, 'missing');
  assert.ok((await rr.getPanel(guildId, draft.id)).messageMissingAt, 'a 10008 flags the panel');
  message.editError = null;
  assert.equal((await rr.syncPanel(guild, draft.id)).status, 'ok');
  assert.equal((await rr.getPanel(guildId, draft.id)).messageMissingAt, null, 'a successful sync clears the flag');

  channel._messages.clear();
  assert.equal((await rr.fetchPanelMessage(guild, await rr.getPanel(guildId, draft.id))).status, 'missing');
  guild.channels.cache.delete(channel.id);
  const noChannel = await rr.fetchPanelMessage(guild, await rr.getPanel(guildId, draft.id));
  assert.deepEqual([noChannel.status, noChannel.code], ['missing', 10003]);
  guild.channels.fetch = async () => { throw discordErr(50001); };
  assert.equal((await rr.fetchPanelMessage(guild, await rr.getPanel(guildId, draft.id))).status, 'failed');
  await prisma.reactionRoleGroup.updateMany({ where: { id: draft.id }, data: { messageMissingAt: null } });
  guild.channels.fetch = async () => { throw new Error('socket hang up'); };
  assert.equal((await rr.fetchPanelMessage(guild, await rr.getPanel(guildId, draft.id))).status, 'failed', 'a cache miss with an unknown REST error is not a deletion');
  assert.equal((await rr.getPanel(guildId, draft.id)).messageMissingAt, null, 'not flagged on an unknown error');

  const attached = await seedPanel(guildId, { attached: true, layout: 'REACTIONS', channelId: channel.id });
  assert.deepEqual(await rr.syncPanel(guild, attached.id), { status: 'skipped' }, 'attached messages are never edited');
});

test('D7. messageDelete flags the panel unless the bot deleted it', { skip: skipDb }, async () => {
  const guildId = G(40);
  const draft = await seedPanel(guildId, { roles: [ROLE(1)] });
  const other = await seedPanel(guildId, { roles: [ROLE(1)] });
  rr.__resetPanelIndex();
  const guild = fakeGuild(guildId);
  const ctx = (messageId) => ({ guildId, guild, message: { id: messageId } });
  await rr.onMessageDelete(ctx(sid('5', 1)));
  assert.equal((await rr.getPanel(guildId, draft.id)).messageMissingAt, null);
  markBotDeletion(other.messageId, 'reaction-roles:delete');
  await rr.onMessageDelete(ctx(other.messageId));
  assert.equal((await rr.getPanel(guildId, other.id)).messageMissingAt, null, 'our own deletions are ignored');
  markBotDeletion(draft.messageId, 'purge');
  await rr.onMessageDelete(ctx(draft.messageId));
  assert.ok((await rr.getPanel(guildId, draft.id)).messageMissingAt, "another feature's deletion still flags the panel");
  rr.__resetPanelIndex();
});

// ── Component handlers (database) ─────────────────────────────────────────────

function componentInteraction({ guild, member, customId, values, personal = false }) {
  const i = {
    customId, guild, guildId: guild.id, member, user: { id: member.id }, values,
    message: { flags: { has: (f) => personal && f === MessageFlags.Ephemeral } },
    deferred: false, replied: false, ephemeral: null, replies: [], kind: [],
  };
  i.deferReply = async ({ ephemeral } = {}) => { i.deferred = true; i.ephemeral = Boolean(ephemeral); i.kind.push('deferReply'); };
  i.deferUpdate = async () => { i.deferred = true; i.kind.push('deferUpdate'); };
  i.editReply = async (p) => { i.replied = true; i.replies.push({ via: 'editReply', ...p }); };
  i.followUp = async (p) => { i.replies.push({ via: 'followUp', ...p }); };
  i.reply = async (p) => { i.replied = true; i.replies.push({ via: 'reply', ...p }); };
  return i;
}

async function componentWorld(n, { type = 'NORMAL', layout = 'BUTTONS', maxRoles = null, roles = 4, held = [], roleOver = {} } = {}) {
  const guildId = G(n);
  const roleList = Array.from({ length: roles }, (_, i) => fakeRole(ROLE(n * 10 + i + 1), `Role ${i + 1}`, roleOver[i] ?? {}));
  const guild = fakeGuild(guildId, { roles: roleList });
  const draft = await seedPanel(guildId, { type, layout, maxRoles, roles: roleList.map((r) => ({ id: r.id, label: r.name })) });
  const member = fakeMember(USER(n), held.map((i) => roleList[i].id));
  const click = (roleIndex, over = {}) => componentInteraction({ guild, member, customId: `rr:${draft.id}:${roleList[roleIndex].id}`, ...over });
  const choose = (indexes, over = {}) => componentInteraction({ guild, member, customId: `rr-select:${draft.id}`, values: indexes.map((i) => roleList[i].id), ...over });
  return { guildId, guild, draft, roles: roleList, member, click, choose };
}
const lastReply = (i) => i.replies.at(-1);
const client0 = { user: { id: BOT } };

test('D8. buttons: NORMAL toggles with a fresh member read, replies ephemerally', { skip: skipDb }, async () => {
  const w = await componentWorld(41);
  const first = w.click(0);
  await rr.handleRoleButton(first, client0);
  assert.deepEqual(first.kind, ['deferReply']);
  assert.equal(first.ephemeral, true);
  assert.equal(lastReply(first).via, 'editReply');
  assert.match(lastReply(first).content, /✅ Added: <@&\d+>/);
  assert.deepEqual(lastReply(first).allowedMentions, { parse: [] });
  // a fast second click uses a stale interaction.member snapshot, but the handler re-reads the member
  const second = w.click(0);
  await rr.handleRoleButton(second, client0);
  assert.match(lastReply(second).content, /➖ Removed/);
  assert.deepEqual(w.member.calls.map((c) => c[0]), ['add', 'remove']);
  // R163: a role that is not on the panel
  const stranger = componentInteraction({ guild: w.guild, member: w.member, customId: `rr:${w.draft.id}:${ROLE(1)}` });
  await rr.handleRoleButton(stranger, client0);
  assert.equal(lastReply(stranger).content, '⚠️ That option is no longer on this panel.');
  // an unknown or foreign panel
  const gone = componentInteraction({ guild: w.guild, member: w.member, customId: `rr:${randomUUID()}:${w.roles[0].id}` });
  await rr.handleRoleButton(gone, client0);
  assert.equal(lastReply(gone).content, '⚠️ This panel is no longer active.');
  const foreign = componentInteraction({ guild: fakeGuild(G(42)), member: w.member, customId: `rr:${w.draft.id}:${w.roles[0].id}` });
  await rr.handleRoleButton(foreign, client0);
  assert.equal(lastReply(foreign).content, '⚠️ This panel is no longer active.');
  assert.equal(w.member.calls.length, 2);
});

test('D8b. panels posted before the rewrite keep working (cuid group ids, legacy customIds, bad stored emoji)', { skip: skipDb }, async () => {
  const guildId = G(56);
  const roleList = [fakeRole(ROLE(561), 'Old one'), fakeRole(ROLE(562), 'Old two')];
  const guild = fakeGuild(guildId, { roles: roleList });
  const legacyId = 'ckx0legacy000000000000001';
  await ensureGuildSettings(guildId);
  await prisma.reactionRoleGroup.create({
    data: {
      id: legacyId, guildId, channelId: sid('3', 56), messageId: sid('5', 56), title: 'Legacy', type: 'NORMAL', layout: 'BUTTONS',
      items: { create: roleList.map((r, i) => ({ customId: `rr:${legacyId}:${r.id}`, roleId: r.id, label: r.name, emoji: i === 0 ? ':x:' : null, position: i })) },
    },
  });
  const member = fakeMember(USER(56));
  const click = componentInteraction({ guild, member, customId: `rr:${legacyId}:${roleList[0].id}` });
  await rr.handleRoleButton(click, client0);
  assert.match(lastReply(click).content, /✅ Added/);
  const stored = await rr.getPanel(guildId, legacyId);
  assert.doesNotThrow(() => rr.buildPanelPayload(stored).components.map((r) => r.toJSON()), 'a legacy bad emoji no longer freezes the panel (R013)');
  assert.equal(rr.buildPanelPayload(stored).components[0].toJSON().components[0].emoji, undefined);
});

test('D9. buttons: UNIQUE swap, VERIFY, REVERSE, the cap and unmanageable roles', { skip: skipDb }, async () => {
  const u = await componentWorld(43, { type: 'UNIQUE', held: [1] });
  await rr.handleRoleButton(u.click(0), client0);
  assert.deepEqual(u.member.calls.map((c) => c.slice(0, 2)), [['add', u.roles[0].id], ['remove', u.roles[1].id]]);

  const v = await componentWorld(44, { type: 'VERIFY', held: [0] });
  const again = v.click(0);
  await rr.handleRoleButton(again, client0);
  assert.match(lastReply(again).content, /You already have <@&\d+>\./);
  assert.equal(v.member.calls.length, 0);

  const r = await componentWorld(45, { type: 'REVERSE' });
  const notHeld = r.click(0);
  await rr.handleRoleButton(notHeld, client0);
  assert.match(lastReply(notHeld).content, /You don't have <@&\d+>, so there's nothing to remove\./);

  const m = await componentWorld(46, { maxRoles: 1, held: [1] });
  const capped = m.click(0);
  await rr.handleRoleButton(capped, client0);
  assert.equal(lastReply(capped).content, 'You can hold at most **1** role(s) from this panel. Remove one first.');

  const high = await componentWorld(47, { roleOver: { 0: { editable: false } } });
  const blocked = high.click(0);
  await rr.handleRoleButton(blocked, client0);
  assert.match(lastReply(blocked).content, /above my highest role/);
  assert.equal(high.member.calls.length, 0);
  high.guild.members.me.permissions.has = () => false;
  const noManage = high.click(1);
  await rr.handleRoleButton(noManage, client0);
  assert.match(lastReply(noManage).content, /Manage Roles/);

  const failing = await componentWorld(48);
  failing.member.failRole.set(failing.roles[0].id, 50013);
  const failed = failing.click(0);
  await rr.handleRoleButton(failed, client0);
  assert.match(lastReply(failed).content, /I couldn't add <@&\d+>: I'm missing permission to manage it\./);
  assert.ok(!/Discord|Error/.test(lastReply(failed).content), 'raw API errors are never shown (R068)');
});

test('D10. a deleted role is confirmed over REST, pruned and reported', { skip: skipDb }, async () => {
  const w = await componentWorld(49);
  w.guild.roles.cache.delete(w.roles[0].id); // not in the cache: only REST can tell
  w.guild.roles.fetch = async () => { throw new Error('network down'); };
  const unsure = w.click(0);
  await rr.handleRoleButton(unsure, client0);
  assert.match(lastReply(unsure).content, /couldn't check that role/);
  assert.equal((await rr.getPanel(w.guildId, w.draft.id)).items.length, 4, 'an unverifiable miss is never treated as a deletion');
  w.guild.roles.fetch = async () => null; // REST: 10011
  const gone = w.click(0);
  await rr.handleRoleButton(gone, client0);
  assert.equal(lastReply(gone).content, 'That role no longer exists, so I removed it from this panel.');
  await rr.__idle();
  assert.equal((await rr.getPanel(w.guildId, w.draft.id)).items.length, 3);
  assert.equal(w.member.calls.length, 0);
});

test('D11. selects: the selection is applied as a set (R044/R159/R160)', { skip: skipDb }, async () => {
  const w = await componentWorld(50, { layout: 'SELECT', held: [0, 1] });
  const pick = w.choose([0, 2]);
  await rr.handleRoleSelect(pick, client0);
  assert.deepEqual(pick.kind, ['deferReply']);
  assert.deepEqual(w.member.calls.map((c) => c.slice(0, 2)), [['add', w.roles[2].id], ['remove', w.roles[1].id]]);
  const reply = lastReply(pick);
  assert.equal(reply.via, 'editReply');
  assert.match(reply.content, /✅ Added[\s\S]*➖ Removed/);
  assert.match(reply.content, /Your current roles from this panel are selected below/);
  const menu = reply.components[0].toJSON().components[0];
  assert.equal(menu.placeholder, 'Adjust your roles from this panel');
  assert.deepEqual(menu.options.map((o) => o.default), [true, false, true, false], 'the personal menu preselects the roles held now');
  assert.deepEqual(reply.allowedMentions, { parse: [] });

  const same = w.choose([0, 2]);
  await rr.handleRoleSelect(same, client0);
  assert.equal(w.member.calls.length, 2, 'selecting what is already held changes nothing');
  assert.match(lastReply(same).content, /No changes/);

  const none = w.choose([]);
  await rr.handleRoleSelect(none, client0);
  assert.deepEqual(w.member.calls.slice(2).map((c) => c[0]).sort(), ['remove', 'remove']);
  assert.deepEqual(lastReply(none).components[0].toJSON().components[0].options.map((o) => o.default), [false, false, false, false]);

  const outside = w.choose([]);
  outside.values = [ROLE(1)];
  await rr.handleRoleSelect(outside, client0);
  assert.equal(lastReply(outside).content, '⚠️ That option is no longer on this panel.');
});

test('D12. selects: the personal menu edits its own message, caps and the unique swap', { skip: skipDb }, async () => {
  const w = await componentWorld(51, { layout: 'SELECT', maxRoles: 2, held: [0, 1] });
  const personal = w.choose([0, 2], { personal: true });
  await rr.handleRoleSelect(personal, client0);
  assert.deepEqual(personal.kind, ['deferUpdate']);
  assert.equal(personal.replies.length, 1);
  assert.equal(lastReply(personal).via, 'editReply', 'the personal copy is updated, not duplicated');
  assert.deepEqual(w.member.calls.map((c) => c.slice(0, 2)), [['add', w.roles[2].id], ['remove', w.roles[1].id]]);

  const cap = await componentWorld(52, { layout: 'SELECT', maxRoles: 2 });
  const over = cap.choose([0, 1, 2]);
  await rr.handleRoleSelect(over, client0);
  assert.match(lastReply(over).content, /✅ Added[\s\S]*⚠️ Not added \(limit of \*\*2\*\*\)/);
  assert.equal(cap.member.calls.length, 2);

  const uniq = await componentWorld(53, { layout: 'SELECT', type: 'UNIQUE', held: [0] });
  const swap = uniq.choose([1]);
  await rr.handleRoleSelect(swap, client0);
  assert.deepEqual(uniq.member.calls.map((c) => c.slice(0, 2)), [['add', uniq.roles[1].id], ['remove', uniq.roles[0].id]]);
  assert.equal(lastReply(swap).components[0].toJSON().components[0].max_values, 1);

  const verify = await componentWorld(54, { layout: 'SELECT', type: 'VERIFY' });
  const get = verify.choose([0, 1]);
  await rr.handleRoleSelect(get, client0);
  assert.equal(verify.member.calls.length, 2);
  assert.deepEqual(lastReply(get).components, [], 'no personal menu for Verify and Reverse panels');
});

test('D13. handlers never throw: a broken interaction answers with an ephemeral notice', { skip: skipDb }, async () => {
  const w = await componentWorld(55);
  const i = w.click(0);
  i.member = { id: w.member.id, roles: { cache: new Collection() }, fetch: async () => { throw new Error('boom'); } };
  w.guild.roles.cache.get = () => { throw new Error('cache exploded'); };
  await assert.doesNotReject(() => rr.handleRoleButton(i, client0));
  assert.match(lastReply(i).content, /Something went wrong/);
});

// ── The command (database, fake interactions) ─────────────────────────────────

function commandWorld(n, { channelDeny = [], roles = [] } = {}) {
  const channel = fakeChannel(sid('3', n * 10 + 1), { deny: channelDeny });
  const roleList = [fakeRole(ROLE(n * 10 + 1), 'News'), fakeRole(ROLE(n * 10 + 2), 'Events'), ...roles];
  const party = fakeEmoji('555555555555555555', 'party');
  const guild = fakeGuild(G(n), { roles: roleList, channels: [channel], emojis: [party] });
  const admin = fakeMember(USER(900 + n));
  admin.roles.highest = { position: 100 };
  const client = { user: { id: BOT }, emojis: { cache: new Collection([[party.id, party]]) }, guilds: { cache: new Collection([[guild.id, guild]]) } };
  const world = { guild, channel, roles: roleList, admin, client, guildId: G(n) };
  world.run = async (sub, opts = {}, { confirm = 'yes', channelId = channel.id } = {}) => {
    const lookup = (name) => (opts[name] === undefined ? null : opts[name]);
    const i = {
      guild, guildId: guild.id, channelId, member: admin, user: { id: admin.id },
      deferred: false, replied: false, ephemeral: null, replies: [], followUps: [], confirmAnswer: confirm,
      isRepliable: () => true,
      options: {
        getSubcommand: () => sub, getString: lookup, getBoolean: lookup, getInteger: lookup, getRole: lookup, getChannel: lookup,
        getFocused: () => opts.__focused,
      },
    };
    i.deferReply = async ({ ephemeral } = {}) => { i.deferred = true; i.ephemeral = Boolean(ephemeral); };
    i.editReply = async (p) => {
      i.replied = true;
      i.replies.push(p);
      const ids = (p.components ?? []).flatMap((row) => row.components.map((b) => b.toJSON().custom_id));
      return {
        id: nextId(), url: 'https://discord.com/x',
        createMessageComponentCollector: () => {
          const c = new EventEmitter();
          setTimeout(() => {
            const pick = ids.find((x) => x.endsWith(i.confirmAnswer === 'yes' ? ':yes' : ':no'));
            c.emit('collect', { customId: pick, user: i.user, update: async () => {} });
            c.emit('end', new Collection(), 'limit');
          }, 0);
          return c;
        },
      };
    };
    i.followUp = async (p) => { i.followUps.push(p); return {}; };
    i.respond = async (choices) => { i.choices = choices; i.responded = true; };
    await cmd.execute(i, client);
    return i;
  };
  world.ac = async (sub, focusedName, value, opts = {}) => {
    const i = {
      guild, guildId: guild.id, responded: false,
      options: { getFocused: () => ({ name: focusedName, value }), getString: (name) => (opts[name] === undefined ? null : opts[name]), getSubcommand: () => sub },
      respond: async (choices) => { i.choices = choices; i.responded = true; },
    };
    await cmd.autocomplete(i);
    return i.choices;
  };
  return world;
}
const embedOf = (i) => i.replies.at(-1).embeds[0].toJSON();
const titleOf = (i) => embedOf(i).title;

test('D14. /reactionrole create: buttons, templates and validation', { skip: skipDb }, async () => {
  const w = commandWorld(60);
  const created = await w.run('create', { channel: w.channel, title: 'Pick roles', role: w.roles[0], layout: 'BUTTONS', max_roles: 2 });
  assert.equal(titleOf(created), '✅  Panel Created');
  assert.match(embedOf(created).description, /First role: <@&\d+>\./);
  const [panel] = await rr.listPanels(w.guildId);
  assert.deepEqual([panel.title, panel.type, panel.layout, panel.maxRoles, panel.attached, panel.itemCount], ['Pick roles', 'NORMAL', 'BUTTONS', 2, false, 1]);
  const sent = w.channel.sent[0];
  assert.equal(sent.payload.components.length, 1);
  assert.equal(sent.payload.embeds[0].toJSON().title, 'Pick roles');
  assert.equal((await rr.getPanel(w.guildId, panel.id)).messageId, sent.id);

  const w61 = commandWorld(61);
  const alien = await w61.run('create', { channel: fakeChannel(sid('3', 619)), template: 'verify' });
  assert.equal(titleOf(alien), '❌  Invalid Channel', 'a channel that is not in the guild cache is refused');

  const w2 = commandWorld(62);
  const verify = await w2.run('create', { channel: w2.channel, template: 'verify', role: w2.roles[0] });
  assert.equal(titleOf(verify), '✅  Panel Created');
  const [v] = await rr.listPanels(w2.guildId);
  assert.deepEqual([v.title, v.type, v.layout], ['✅ Verification', 'VERIFY', 'BUTTONS']);
  const full = await rr.getPanel(w2.guildId, v.id);
  assert.deepEqual([full.items[0].label, full.items[0].emoji, full.items[0].style], ['Verify', '✅', 3]);
  const notify = await w2.run('create', { channel: w2.channel, template: 'notifications', layout: 'REACTIONS', role: w2.roles[1] });
  assert.equal(titleOf(notify), '✅  Panel Created');
  assert.deepEqual(w2.channel.sent[1].reacted, ['🔔'], 'the bot pre-reacts with the item emoji');
  assert.match(w2.channel.sent[1].payload.embeds[0].toJSON().description, /🔔 — \*\*Events\*\*/);

  const noEmoji = await w2.run('create', { channel: w2.channel, layout: 'REACTIONS', role: w2.roles[0] });
  assert.equal(titleOf(noEmoji), '❌  Emoji Needed');
  const orphanEmoji = await w2.run('create', { channel: w2.channel, emoji: '🎉' });
  assert.equal(titleOf(orphanEmoji), '❌  Missing Role');
  const badEmoji = await w2.run('create', { channel: w2.channel, role: w2.roles[0], emoji: ':nope:' });
  assert.equal(titleOf(badEmoji), '❌  Invalid Emoji');
  const ignoredMax = await w2.run('create', { channel: w2.channel, type: 'UNIQUE', max_roles: 3, layout: 'REACTIONS', role: w2.roles[0], emoji: '🎉' });
  assert.match(embedOf(ignoredMax).description, /`max_roles` only applies to Normal panels/);
  assert.equal(w2.channel.sent.length, 3);

  const locked = commandWorld(63, { channelDeny: ['EmbedLinks'] });
  const denied = await locked.run('create', { channel: locked.channel });
  assert.equal(titleOf(denied), '❌  Missing Permissions');
  assert.match(embedOf(denied).description, /Embed Links/);
  const textOnly = await locked.run('create', { channel: locked.channel, plain_text: true, message: 'Hello' });
  assert.equal(titleOf(textOnly), '✅  Panel Created');
  assert.deepEqual(locked.channel.sent[0].payload.embeds, []);
});

test('D15. /reactionrole create: role checks, rollback and the module warning', { skip: skipDb }, async () => {
  const high = fakeRole(ROLE(640), 'Boss', { position: 60 });
  const managed = fakeRole(ROLE(641), 'Bot role', { managed: true });
  const w = commandWorld(64, { roles: [high, managed] });
  const refusedHigh = await w.run('create', { channel: w.channel, role: high });
  assert.equal(titleOf(refusedHigh), '❌  Role Not Allowed');
  assert.match(embedOf(refusedHigh).description, /above my highest role/);
  const refusedManaged = await w.run('create', { channel: w.channel, role: managed });
  assert.match(embedOf(refusedManaged).description, /managed by an integration/);
  assert.equal(w.channel.sent.length, 0);
  assert.equal(await rr.countPanels(w.guildId), 0);

  w.admin.roles.highest = { position: 1 };
  const aboveAdmin = await w.run('create', { channel: w.channel, role: fakeRole(ROLE(642), 'Mid', { position: 5 }) });
  assert.equal(titleOf(aboveAdmin), '❌  Role Not Allowed');
  w.admin.roles.highest = { position: 100 };

  const dangerous = fakeRole(ROLE(643), 'Mod', { dangerous: ['BanMembers', 'ManageMessages'] });
  w.guild.roles.cache.set(dangerous.id, dangerous);
  const declined = await w.run('create', { channel: w.channel, role: dangerous }, { confirm: 'no' });
  assert.equal(titleOf(declined), 'ℹ️  Cancelled');
  assert.equal(w.channel.sent.length, 0);
  const accepted = await w.run('create', { channel: w.channel, role: dangerous }, { confirm: 'yes' });
  assert.equal(titleOf(accepted), '✅  Panel Created');
  const confirmCard = accepted.replies[0].embeds[0].toJSON();
  assert.match(confirmCard.description, /Ban Members, Manage Messages/);

  const failing = commandWorld(65);
  failing.channel.sendError = discordErr(50013);
  const noPost = await failing.run('create', { channel: failing.channel });
  assert.equal(titleOf(noPost), '❌  Could Not Post');
  assert.match(embedOf(noPost).description, /missing permissions there/);

  const off = commandWorld(66);
  await updateSettings(off.guildId, { reactionRolesEnabled: false });
  const warned = await off.run('create', { channel: off.channel });
  assert.match(embedOf(warned).description, /Reaction Roles module is off/);
  assert.ok(embedOf(warned).description.trimEnd().endsWith('`/module enable reactionroles`.'), 'the module warning comes last');
});

test('D16. /reactionrole create enforces the panel cap', { skip: skipDb }, async () => {
  const w = commandWorld(67);
  const { LIMITS } = await import('../src/config/constants.js');
  await ensureGuildSettings(w.guildId);
  await prisma.reactionRoleGroup.createMany({
    data: Array.from({ length: LIMITS.REACTION_ROLE_PANELS }, (_, i) => ({ id: randomUUID(), guildId: w.guildId, channelId: '1', messageId: String(i + 1), title: `P${i}` })),
  });
  const full = await w.run('create', { channel: w.channel });
  assert.equal(titleOf(full), '❌  Panel Limit Reached');
  assert.equal(w.channel.sent.length, 0);
  const attach = await w.run('attach', { message_link: sid('5', 1) });
  assert.equal(titleOf(attach), '❌  Message Not Found');
});

test('D17. /reactionrole add and remove: buttons, dropdowns and reactions', { skip: skipDb }, async () => {
  const w = commandWorld(68);
  await w.run('create', { channel: w.channel, title: 'Buttons' });
  await w.run('create', { channel: w.channel, title: 'Menu', layout: 'SELECT', placeholder: 'Choose wisely' });
  await w.run('create', { channel: w.channel, title: 'Emoji', layout: 'REACTIONS', type: 'UNIQUE' });
  const [react, menu, buttons] = await rr.listPanels(w.guildId);
  assert.deepEqual([buttons.title, menu.title, react.title], ['Buttons', 'Menu', 'Emoji']);
  const msgOf = (panel) => w.channel._messages.get(panel.messageId);

  const added = await w.run('add', { panel: buttons.id, role: w.roles[0], label: 'The News', style: 'Success', emoji: '🔔' });
  assert.equal(titleOf(added), '✅  Role Added');
  const b = await rr.getPanel(w.guildId, buttons.id);
  assert.deepEqual([b.items[0].label, b.items[0].style, b.items[0].emoji], ['The News', 3, '🔔']);
  assert.equal(msgOf(buttons).edits.at(-1).components[0].toJSON().components[0].label, 'The News');
  assert.equal(titleOf(await w.run('add', { panel: buttons.id, role: w.roles[0] })), '❌  Already Added');
  assert.equal(titleOf(await w.run('add', { panel: randomUUID(), role: w.roles[1] })), '❌  Panel Not Found');
  assert.equal(titleOf(await w.run('add', { panel: buttons.id, role: w.roles[1], emoji: 'nope' })), '❌  Invalid Emoji');
  const styled = await w.run('add', { panel: menu.id, role: w.roles[1], style: 'Danger', description: 'Event pings' });
  assert.match(embedOf(styled).description, /`style` only applies to button panels/);
  const described = await w.run('add', { panel: buttons.id, role: w.roles[1], description: 'ignored' });
  assert.match(embedOf(described).description, /`description` only applies to dropdown panels/);
  const m = await rr.getPanel(w.guildId, menu.id);
  assert.equal(m.items[0].description, 'Event pings');
  assert.equal(m.placeholder, 'Choose wisely');
  const menuJson = msgOf(menu).edits.at(-1).components[0].toJSON().components[0];
  assert.equal(menuJson.placeholder, 'Choose wisely');
  assert.equal(menuJson.options[0].description, 'Event pings');

  // reactions
  assert.equal(titleOf(await w.run('add', { panel: react.id, role: w.roles[0] })), '❌  Emoji Needed');
  const r1 = await w.run('add', { panel: react.id, role: w.roles[0], emoji: '🔔' });
  assert.equal(titleOf(r1), '✅  Role Added');
  assert.deepEqual(msgOf(react).reacted, ['🔔']);
  assert.match(msgOf(react).edits.at(-1).embeds[0].toJSON().description, /🔔 — \*\*News\*\*/);
  assert.equal(titleOf(await w.run('add', { panel: react.id, role: w.roles[1], emoji: '🔔' })), '❌  Emoji In Use');
  const custom = await w.run('add', { panel: react.id, role: w.roles[1], emoji: ':party:' });
  assert.match(embedOf(custom).description, /with <:party:555555555555555555>/);
  assert.deepEqual(msgOf(react).reacted, ['🔔', '<:party:555555555555555555>']);
  assert.ok((await rr.getPanelIndex(w.guildId)).get(react.messageId).items.length === 2, 'the index is refreshed after add');

  // a reaction Discord refuses undoes the add
  const undo = commandWorld(69);
  await undo.run('create', { channel: undo.channel, layout: 'REACTIONS' });
  const [undoPanel] = await rr.listPanels(undo.guildId);
  undo.channel._messages.get(undoPanel.messageId).reactError = discordErr(10014);
  const refused = await undo.run('add', { panel: undoPanel.id, role: undo.roles[0], emoji: '🔔' });
  assert.equal(titleOf(refused), '❌  Could Not Add Reaction');
  assert.equal((await rr.getPanel(undo.guildId, undoPanel.id)).items.length, 0);
  // a payload Discord rejects undoes the add too
  await undo.run('create', { channel: undo.channel, title: 'Plain' });
  const plain = (await rr.listPanels(undo.guildId))[0];
  undo.channel._messages.get(plain.messageId).editError = discordErr(50035);
  assert.equal(titleOf(await undo.run('add', { panel: plain.id, role: undo.roles[0] })), '❌  Panel Update Failed');
  assert.equal((await rr.getPanel(undo.guildId, plain.id)).items.length, 0);

  // remove: by item, also for a deleted role
  const removed = await w.run('remove', { panel: buttons.id, item: b.items[0].id });
  assert.equal(titleOf(removed), '✅  Role Removed');
  assert.equal((await rr.getPanel(w.guildId, buttons.id)).items.length, 1);
  const reactPanel = await rr.getPanel(w.guildId, react.id);
  w.guild.roles.cache.delete(reactPanel.items[1].roleId);
  const reaction = msgOf(react).reactions.cache.get('555555555555555555');
  const gone = await w.run('remove', { panel: react.id, item: reactPanel.items[1].id });
  assert.match(embedOf(gone).description, /\(deleted role\)/);
  assert.equal(reaction.removedAll, true, "the item's reaction is cleared");
  assert.equal((await rr.getPanel(w.guildId, react.id)).items.length, 1);
  assert.equal(titleOf(await w.run('remove', { panel: react.id, item: 'nothing' })), '❌  Not On Panel');
  assert.equal(rr.peekPanelIndex(w.guildId), null, 'every mutation invalidates the panel index');
});

test('D18. /reactionrole add and edit report a deleted panel message', { skip: skipDb }, async () => {
  const w = commandWorld(70);
  await w.run('create', { channel: w.channel, title: 'Lost' });
  const [panel] = await rr.listPanels(w.guildId);
  w.channel._messages.delete(panel.messageId);
  const added = await w.run('add', { panel: panel.id, role: w.roles[0] });
  assert.equal(titleOf(added), '✅  Role Added');
  assert.match(embedOf(added).description, /The panel message no longer exists\. Run `\/reactionrole repost panel:Lost`/);
  assert.ok((await rr.getPanel(w.guildId, panel.id)).messageMissingAt);
  const listed = await w.run('list');
  assert.match(JSON.stringify(listed.replies.at(-1).embeds[0].toJSON()), /message deleted — run \/reactionrole repost/);

  const reposted = await w.run('repost', { panel: panel.id });
  assert.equal(titleOf(reposted), '✅  Panel Reposted');
  const after = await rr.getPanel(w.guildId, panel.id);
  assert.equal(after.messageMissingAt, null);
  assert.equal(after.messageId, w.channel.sent.at(-1).id);
  assert.equal(after.items.length, 1);
  assert.equal(w.channel.sent.at(-1).payload.components.length, 1, 'the new message carries the buttons');
});

test('D19. /reactionrole edit', { skip: skipDb }, async () => {
  const w = commandWorld(71);
  await w.run('create', { channel: w.channel, title: 'Edit me', layout: 'SELECT', max_roles: 3, role: w.roles[0] });
  const [panel] = await rr.listPanels(w.guildId);
  const message = w.channel._messages.get(panel.messageId);
  assert.equal(titleOf(await w.run('edit', { panel: panel.id })), '❌  Nothing To Change');
  const edited = await w.run('edit', { panel: panel.id, title: 'Renamed', description: 'New text', message: 'Top line', placeholder: 'Choose' });
  assert.equal(titleOf(edited), '✅  Panel Updated');
  assert.match(embedOf(edited).description, /title, description, message, placeholder/);
  let p = await rr.getPanel(w.guildId, panel.id);
  assert.deepEqual([p.title, p.description, p.messageContent, p.placeholder], ['Renamed', 'New text', 'Top line', 'Choose']);
  assert.equal(message.edits.at(-1).content, 'Top line');
  assert.equal(message.edits.at(-1).embeds[0].toJSON().title, 'Renamed');
  await w.run('edit', { panel: panel.id, description: '-', message: '-', placeholder: '-' });
  p = await rr.getPanel(w.guildId, panel.id);
  assert.deepEqual([p.description, p.messageContent, p.placeholder], [null, null, null]);
  assert.equal(message.edits.at(-1).content, null);
  await w.run('edit', { panel: panel.id, plain_text: true });
  assert.deepEqual(message.edits.at(-1).embeds, []);
  assert.ok(message.edits.at(-1).content.includes('-# '));
  const toUnique = await w.run('edit', { panel: panel.id, type: 'UNIQUE', max_roles: 2 });
  assert.match(embedOf(toUnique).description, /`max_roles` only applies to Normal panels/);
  p = await rr.getPanel(w.guildId, panel.id);
  assert.deepEqual([p.type, p.maxRoles], ['UNIQUE', null]);
  await w.run('edit', { panel: panel.id, type: 'NORMAL', max_roles: 4 });
  assert.equal((await rr.getPanel(w.guildId, panel.id)).maxRoles, 4);
  await w.run('edit', { panel: panel.id, max_roles: 0 });
  assert.equal((await rr.getPanel(w.guildId, panel.id)).maxRoles, null);
  assert.equal(titleOf(await w.run('edit', { panel: randomUUID(), title: 'x' })), '❌  Panel Not Found');

  await w.run('create', { channel: w.channel, title: 'Buttons' });
  const buttons = (await rr.listPanels(w.guildId))[0];
  const ignored = await w.run('edit', { panel: buttons.id, placeholder: 'Nope' });
  assert.equal(titleOf(ignored), '❌  Nothing To Change');
  assert.match(embedOf(ignored).description, /`placeholder` only applies to dropdown panels/);
  assert.equal(titleOf(await w.run('edit', { panel: buttons.id, title: '' })), '✅  Panel Updated');
  assert.equal((await rr.getPanel(w.guildId, buttons.id)).title, 'Reaction Roles', 'an empty title becomes the default');
});

test('D20. /reactionrole attach binds an existing message and never edits it', { skip: skipDb }, async () => {
  const w = commandWorld(72);
  const other = fakeChannel(sid('3', 729));
  other.guild = w.guild;
  w.guild.channels.cache.set(other.id, other);
  const original = fakeMessage(nextId(), other, { content: '**Welcome** to the server\nsecond line' });
  other._messages.set(original.id, original);
  const link = `https://discord.com/channels/${w.guildId}/${other.id}/${original.id}`;

  assert.equal(titleOf(await w.run('attach', { message_link: 'nonsense' })), '❌  Invalid Message');
  assert.equal(titleOf(await w.run('attach', { message_link: `https://discord.com/channels/${G(79)}/${other.id}/${original.id}` })), '❌  Invalid Message');
  assert.equal(titleOf(await w.run('attach', { message_link: `https://discord.com/channels/${w.guildId}/${other.id}/${sid('5', 424242)}` })), '❌  Message Not Found');
  const attached = await w.run('attach', { message_link: link, type: 'UNIQUE' });
  assert.equal(titleOf(attached), '✅  Panel Attached');
  const [panel] = await rr.listPanels(w.guildId);
  assert.deepEqual([panel.title, panel.type, panel.layout, panel.attached, panel.itemCount, panel.messageId], ['Welcome to the server', 'UNIQUE', 'REACTIONS', true, 0, original.id]);
  assert.equal(titleOf(await w.run('attach', { message_link: link })), '❌  Already a Panel');

  const added = await w.run('add', { panel: panel.id, role: w.roles[0], emoji: '🔔' });
  assert.equal(titleOf(added), '✅  Role Added');
  assert.deepEqual(original.reacted, ['🔔']);
  assert.deepEqual(original.edits, [], 'the attached message is never edited');
  const edited = await w.run('edit', { panel: panel.id, description: 'x' });
  assert.equal(titleOf(edited), '❌  Attached Panel');
  assert.equal(titleOf(await w.run('edit', { panel: panel.id, title: 'Rules panel', max_roles: 2 })), '✅  Panel Updated');

  // repost without a channel only restores the bot's reactions
  original.reactions.cache.clear();
  const restored = await w.run('repost', { panel: panel.id });
  assert.equal(titleOf(restored), '✅  Reactions Restored');
  assert.deepEqual(original.reacted, ['🔔', '🔔']);
  assert.equal(w.channel.sent.length, 0);

  // delete leaves the message alone
  const del = await w.run('delete', { panel: panel.id }, { confirm: 'yes' });
  assert.equal(original.deleted, false);
  assert.equal(await rr.getPanel(w.guildId, panel.id), null);
  assert.match(JSON.stringify(del.followUps[0].embeds[0].toJSON()), /The original message was left alone/);
});

test('D21. /reactionrole repost converts an attached panel to a bot panel', { skip: skipDb }, async () => {
  const w = commandWorld(73);
  const original = fakeMessage(nextId(), w.channel, { content: 'Hello' });
  w.channel._messages.set(original.id, original);
  await w.run('attach', { message_link: original.id });
  const [panel] = await rr.listPanels(w.guildId);
  await w.run('add', { panel: panel.id, role: w.roles[0], emoji: '🔔' });
  const moved = await w.run('repost', { panel: panel.id, channel: w.channel });
  assert.equal(titleOf(moved), '✅  Panel Reposted');
  assert.match(embedOf(moved).description, /now a message I posted/);
  const after = await rr.getPanel(w.guildId, panel.id);
  assert.equal(after.attached, false);
  assert.equal(after.messageId, w.channel.sent.at(-1).id);
  assert.deepEqual(w.channel.sent.at(-1).reacted, ['🔔']);
  assert.equal(original.deleted, false, 'the original message is never deleted');
  assert.equal(original.edits.length, 0);

  // a normal repost deletes the old message and marks the deletion as ours
  const old = w.channel.sent.at(-1);
  const again = await w.run('repost', { panel: panel.id });
  assert.equal(titleOf(again), '✅  Panel Reposted');
  assert.equal(old.deleted, true);
  assert.equal(await rr.findPanelByMessage(w.guildId, old.id), null);
  assert.equal(titleOf(await w.run('repost', { panel: randomUUID() })), '❌  Panel Not Found');
});

test('D22. /reactionrole delete and list', { skip: skipDb }, async () => {
  const w = commandWorld(74);
  const empty = await w.run('list');
  assert.equal(titleOf(empty), 'ℹ️  No Panels');
  await w.run('create', { channel: w.channel, title: 'Doomed', role: w.roles[0] });
  await w.run('create', { channel: w.channel, title: 'Kept' });
  const listed = await w.run('list');
  assert.match(embedOf(listed).title, /Reaction Role Panels \(2\)/);
  assert.equal(embedOf(listed).fields.length, 2);
  const doomed = (await rr.listPanels(w.guildId)).find((p) => p.title === 'Doomed');
  const message = w.channel._messages.get(doomed.messageId);

  const cancelled = await w.run('delete', { panel: doomed.id }, { confirm: 'no' });
  assert.equal(titleOf(cancelled), 'ℹ️  Cancelled');
  assert.ok(await rr.getPanel(w.guildId, doomed.id));
  assert.equal(message.deleted, false);

  const done = await w.run('delete', { panel: doomed.id }, { confirm: 'yes' });
  assert.equal(done.ephemeral, true, 'the delete prompt is private');
  assert.equal(await rr.getPanel(w.guildId, doomed.id), null);
  assert.equal(message.deleted, true);
  assert.match(JSON.stringify(done.followUps[0].embeds[0].toJSON()), /was deleted, along with its message/);
  assert.equal(await prisma.reactionRoleItem.count({ where: { groupId: doomed.id } }), 0);

  // the message was already gone: the follow-up says so instead of claiming it was removed (R164)
  const kept = (await rr.listPanels(w.guildId))[0];
  w.channel._messages.delete(kept.messageId);
  const gone = await w.run('delete', { panel: kept.id }, { confirm: 'yes' });
  assert.match(JSON.stringify(gone.followUps[0].embeds[0].toJSON()), /already gone, or I couldn't delete it/);
  assert.equal(titleOf(await w.run('delete', { panel: randomUUID() })), '❌  Panel Not Found');
});

test('D23. autocomplete: panels and items', { skip: skipDb }, async () => {
  const w = commandWorld(75);
  for (let i = 0; i < 30; i++) {
    await seedPanel(w.guildId, { title: i === 2 ? 'Findable ' + 'x'.repeat(200) : `Panel ${i}`, layout: 'REACTIONS' });
  }
  const all = await w.ac('add', 'panel', '');
  assert.equal(all.length, 25);
  for (const c of all) assert.ok(c.name.length <= 100 && c.value.length <= 100);
  const found = await w.ac('add', 'panel', 'findable');
  assert.equal(found.length, 1);
  assert.ok(found[0].name.length <= 100 && found[0].name.endsWith(' · Emoji reactions · 0 role(s)'));
  assert.deepEqual(await w.ac('add', 'panel', 'nothing-matches'), []);

  const draft = await seedPanel(w.guildId, { title: 'Items', roles: [{ id: w.roles[0].id, label: 'Alpha', emoji: '🔔' }, { id: w.roles[1].id, label: 'Beta' }, { id: ROLE(7501), label: 'Orphan' }] });
  const items = await w.ac('remove', 'item', '', { panel: draft.id });
  assert.equal(items.length, 3);
  assert.equal(items[0].name, '🔔 Alpha · @News');
  assert.equal(items[2].name, 'Orphan · deleted role');
  assert.deepEqual((await w.ac('remove', 'item', 'events', { panel: draft.id })).map((c) => c.name), ['Beta · @Events'], 'matches the role name too');
  assert.deepEqual(await w.ac('remove', 'item', '', {}), []);
  assert.deepEqual(await w.ac('remove', 'other', 'x', { panel: draft.id }), []);
  assert.deepEqual(await w.ac('remove', 'item', '', { panel: randomUUID() }), []);
});

test('D24. the cache-miss and hook flows read the database through the index with Redis down', { skip: skipDb }, async () => {
  const w = commandWorld(76);
  await w.run('create', { channel: w.channel, layout: 'REACTIONS', role: w.roles[0], emoji: '🔔', type: 'UNIQUE' });
  const [panel] = await rr.listPanels(w.guildId);
  const message = w.channel._messages.get(panel.messageId);
  rr.__resetPanelIndex();
  const member = fakeMember(USER(76));
  const reaction = fakeReaction(message, '🔔', false);
  await rr.processReaction({
    guildId: w.guildId, guild: w.guild, client: w.client, message, reaction, user: { id: member.id }, emojiKey: '🔔',
    getMember: async () => member, ensureMessage: async () => message,
  }, 'react');
  assert.deepEqual(member.calls.map((c) => c[0]), ['add'], 'the index was loaded from Postgres');
  rr.__resetPanelIndex();
});
