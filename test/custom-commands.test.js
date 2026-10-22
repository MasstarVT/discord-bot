// Tests for pkg-custom-commands: custom commands (actions, arguments, allow/deny lists, cooldown scope,
// random replies, prefix invocation) and trigger hardening. The offline cases need no Discord, Postgres or
// Redis. Cases that query the database are skipped unless TEST_DATABASE_URL is set:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/custom-commands.test.js
import { test, describe, before, beforeEach, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ChannelType } from 'discord.js';

process.env.NODE_ENV = 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
const DB = process.env.TEST_DATABASE_URL;
if (DB) process.env.DATABASE_URL = DB;
else process.env.DATABASE_URL ??= 'postgresql://test:test@127.0.0.1:1/test';
// Nothing listens here: cache.* fails fast and returns its fallbacks (setNX gives null).
process.env.REDIS_URL = 'redis://127.0.0.1:1';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const svc = await import('../src/services/tagService.js');
const { markBotDeletion, wasDeletedByBot } = await import('../src/utils/deletionTracker.js');
const { PERMISSION_LEVELS, LIMITS } = await import('../src/config/constants.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const tagCmd = await import('../src/commands/utility/tag.js');
const { tagRolesAllow } = await import('../src/commands/utility/help.js');
const { plugin } = await import('../src/plugins/custom-commands.js');
const settingsSvc = await import('../src/services/settingsService.js');
const prisma = (await import('../src/database/client.js')).default;

const {
  normalizeTagName, validateNewTagName, effectivePrefix, parseInvocation, parseArgs, buildUsage, normalizeActions,
  getEffectiveActions, legacyActions, syncLegacyColumns, primaryMessageIndex, channelReplyIndex, pickResponse, planRun,
  buildMessagePayload, channelScopeIds, checkTagAccess, checkTagRoles, triggerAppliesHere, validateTriggerPattern, selectTrigger,
  matchesPlain, getTriggerSet, acquireCooldown, tagCooldownKey, runTag, handlePrefixInvocation, handleTriggers,
  isPossibleInvocation, hasTriggerContent, describeAction, actionLabel, cooldownLabel, codeSafe, oneLine, FAILURE_TEXT,
  TAG_NAME_RE, TagError, __testing,
} = svc;

const realGuildSettings = prisma.guildSettings;

after(async () => {
  prisma.guildSettings = realGuildSettings;
  Object.assign(console, quiet);
  (await import('../src/services/redis.js')).default.disconnect();
  await prisma.$disconnect().catch(() => {});
});

/** reset() restores the defaults, which count uses in the database; offline tests turn that off again. */
function resetSvc() {
  __testing.reset();
  __testing.configure({ countUses: false });
}

beforeEach(resetSvc);

// ── Fakes ─────────────────────────────────────────────────────────────────────

const GID = 'g';
const U = '111111111111111111';
const ROLE_A = '222222222222222222';
const ROLE_B = '666666666666666666';
const CH = '333333333333333333';
const CH2 = '777777777777777777';
const CAT = '444444444444444444';
const ME = '555555555555555555';
const OWNER = '999999999999999999';

const makeRole = (id, { position = 1, managed = false, mentionable = false, dangerous = false } = {}) => ({
  id, name: `role-${id.slice(0, 3)}`, position, managed, mentionable, permissions: { has: () => dangerous },
});

function makeGuild({ id = GID, roles = [], mePerms = true, mePosition = 100 } = {}) {
  return {
    id,
    name: 'G',
    memberCount: 5,
    ownerId: OWNER,
    iconURL: () => null,
    roles: { cache: new Map(roles.map((r) => [r.id, r])) },
    channels: { cache: new Map() },
    members: { me: { id: ME, permissions: { has: () => mePerms }, roles: { highest: { position: mePosition } } }, fetch: async () => null },
  };
}

function makeChannel(guild, id = CH, { parentId = null, thread = false, sendImpl = null, hasPerm = () => true } = {}) {
  const sent = [];
  const ch = {
    id,
    guild,
    parentId,
    name: `chan-${id.slice(0, 3)}`,
    type: ChannelType.GuildText,
    isThread: () => thread,
    isTextBased: () => true,
    permissionsFor: () => ({ has: hasPerm }),
    sent,
    send: async (payload) => {
      if (sendImpl) await sendImpl(payload);
      sent.push(payload);
      return { id: `s${sent.length}` };
    },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function makeUser(id = U) {
  const user = {
    id, username: 'ann', tag: 'ann', bot: false, displayAvatarURL: () => 'https://x/a.png', dms: [], dmError: null,
  };
  user.send = async (payload) => {
    if (user.dmError) throw user.dmError;
    user.dms.push(payload);
  };
  return user;
}

function makeMember(guild, user, roleIds = []) {
  const calls = [];
  const member = { id: user.id, user, displayName: 'Ann', guild, calls, roles: { cache: new Map(roleIds.map((id) => [id, { id }])) } };
  member.roles.add = async (id, reason) => { calls.push(['add', id, reason]); member.roles.cache.set(id, { id }); };
  member.roles.remove = async (id, reason) => { calls.push(['remove', id, reason]); member.roles.cache.delete(id); };
  return member;
}

const err = (code, message = 'x') => Object.assign(new Error(message), { code });

let msgSeq = 0;
function makeMessage({ content, channel, user, member, guild, deleteImpl = null }) {
  const id = `m${++msgSeq}`;
  const m = {
    id, content, channel, channelId: channel.id, guildId: guild.id, guild, author: user, member, deleted: 0, replies: [],
    delete: async () => { m.deleted++; if (deleteImpl) await deleteImpl(); },
    reply: async (payload) => { m.replies.push(payload); return { id: `r${m.replies.length}`, delete: async () => {} }; },
  };
  return m;
}

const SETTINGS = { customPrefix: '!', embedColor: null, customCommandsEnabled: true, autoTriggersEnabled: true };

function makeCtx({ guild, message, settings = SETTINGS }) {
  return {
    guildId: guild.id,
    guild,
    message,
    state: {},
    stopped: false,
    stop() { this.stopped = true; },
    getSettings: async () => settings,
  };
}

const tagRow = (over = {}) => ({
  id: 't1', guildId: GID, name: 'hi', content: 'Hello {user}! {1:friend}', useEmbed: false, enabled: true, actions: null,
  allowRoleIds: [], denyRoleIds: [], allowChannelIds: [], denyChannelIds: [], cooldownScope: 'NONE', cooldownSeconds: 0,
  deleteInvocation: true, description: null, uses: 0, createdBy: U, ...over,
});

const proj = (over = {}) => ({
  id: 'a', trigger: 'hello', response: 'Hi', matchType: 'CONTAINS', caseSensitive: false, cooldown: 0, deleteMessage: false,
  channelIds: [], exemptRoleIds: [], v: '2026-01-01T00:00:00.000Z', ...over,
});

/** A scenario with one guild, one text channel, one user and one member. */
function scenario({ roles = [], memberRoles = [] } = {}) {
  const guild = makeGuild({ roles });
  const channel = makeChannel(guild);
  const user = makeUser();
  const member = makeMember(guild, user, memberRoles);
  return { guild, channel, user, member };
}

function makeInteraction({ sub, group = null, options = {}, guild, member, channel, user, focused = null }) {
  const calls = { reply: [], deferReply: [], editReply: [], followUp: [], respond: [] };
  const i = {
    guild,
    guildId: guild.id,
    member,
    user,
    channel,
    client: { users: { fetch: async () => ({ tag: 'ann#0' }) } },
    createdTimestamp: Date.now(),
    deferred: false,
    replied: false,
    responded: false,
    calls,
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => group,
      getString: (n) => options[n] ?? null,
      getBoolean: (n) => options[n] ?? null,
      getInteger: (n) => options[n] ?? null,
      getChannel: (n) => options[n] ?? null,
      getRole: (n) => options[n] ?? null,
      getFocused: (full) => (full ? focused : (typeof focused === 'string' ? focused : focused?.value ?? '')),
    },
    reply: async (payload) => { calls.reply.push(payload); i.replied = true; },
    deferReply: async (payload) => { calls.deferReply.push(payload); i.deferred = true; i.ephemeral = payload?.ephemeral === true; },
    editReply: async (payload) => { calls.editReply.push(payload); },
    followUp: async (payload) => { calls.followUp.push(payload); },
    respond: async (choices) => { calls.respond.push(choices); i.responded = true; },
  };
  return i;
}

const lastPayload = (i) => [...i.calls.reply, ...i.calls.editReply].at(-1);
const embedData = (payload) => payload.embeds[0].data;
const titleOf = (i) => embedData(lastPayload(i)).title;
const textOf = (i) => embedData(lastPayload(i)).description ?? '';
const fieldsOf = (i) => embedData(lastPayload(i)).fields ?? [];

// ── 1. Names ──────────────────────────────────────────────────────────────────

test('1. names: normalisation, fixed points and strict validation of new names', () => {
  assert.equal(normalizeTagName('Hello World'), 'hello-world');
  assert.equal(normalizeTagName('FAQ!'), 'faq');
  assert.equal(normalizeTagName('!!!'), '');
  assert.equal(normalizeTagName('  Rules '), 'rules');
  assert.equal(normalizeTagName('a'.repeat(60)).length, 50);
  assert.equal(normalizeTagName(null), '');
  for (const stored of ['a', 'abc_def-1', 'x'.repeat(50), '0-9_']) {
    assert.match(stored, TAG_NAME_RE);
    assert.equal(normalizeTagName(stored), stored, `${stored} is a fixed point`);
  }
  assert.equal(validateNewTagName('c++').ok, false, 'lossy names are refused, not stripped');
  assert.deepEqual(validateNewTagName('Server Rules'), { ok: true, name: 'server-rules' });
  assert.equal(validateNewTagName('a'.repeat(33)).ok, false);
  assert.equal(validateNewTagName('a'.repeat(32)).ok, true);
  assert.equal(validateNewTagName('règles').ok, false);
  assert.equal(validateNewTagName('').ok, false);
});

// ── 2. Prefix parsing ─────────────────────────────────────────────────────────

test('2. prefix parsing and arguments', () => {
  assert.equal(effectivePrefix('!'), '!');
  assert.equal(effectivePrefix(''), null, 'an empty prefix means prefix commands are off');
  assert.equal(effectivePrefix(' !'), '!');
  assert.equal(effectivePrefix('abcdef'), '!');
  assert.equal(effectivePrefix('>>'), '>>');
  assert.equal(effectivePrefix(null), '!');
  assert.equal(effectivePrefix(undefined), '!');

  assert.deepEqual(parseInvocation('!rules a  b\nc', '!'), { name: 'rules', args: ['a', 'b', 'c'] });
  assert.equal(parseInvocation('!Rules', '!').name, 'rules');
  for (const bad of ['! rules', '!!', '?rules', '!ru.les', `!${'a'.repeat(51)}`]) {
    assert.equal(parseInvocation(bad, '!'), null, bad);
  }
  assert.deepEqual(parseInvocation('>>hi', '>>'), { name: 'hi', args: [] });
  assert.equal(parseInvocation('!hi', null), null, 'no prefix, no invocation');

  assert.deepEqual(parseArgs('  '), []);
  assert.equal(parseArgs(Array(150).fill('w').join(' ')).length, 100);
  assert.deepEqual(parseArgs('a "b c"'), ['a', '"b', 'c"'], 'quotes mean nothing');
});

// ── 3. Actions ────────────────────────────────────────────────────────────────

test('3. the action model', () => {
  assert.equal(normalizeActions(null), null);
  assert.equal(normalizeActions({ version: 2, actions: [] }), null, 'an unknown format is not an array');
  assert.equal(normalizeActions([]).length, 0);

  const mixed = normalizeActions([
    { type: 'reply', responses: ['hi'] },
    { type: 'reply', responses: ['', '  '] },
    { type: 'send', responses: ['x'] },
    { type: 'addRole', roleId: 'nope' },
    { type: 'wat', responses: ['x'] },
    null,
  ]);
  assert.deepEqual(mixed, [{ type: 'reply', responses: ['hi'], embed: false, dm: false }]);
  assert.equal(normalizeActions(Array(7).fill({ type: 'reply', responses: ['x'] })).length, 5, 'capped at LIMITS.TAG_ACTIONS');
  assert.equal(
    normalizeActions([{ type: 'reply', responses: Array(15).fill('x') }])[0].responses.length,
    LIMITS.TAG_RESPONSES,
  );

  const pings = normalizeActions([
    { type: 'reply', dm: true, responses: ['x'], pingRoleIds: [ROLE_A] },
    { type: 'reply', responses: ['x'], pingRoleIds: [ROLE_A, ROLE_A, ROLE_B, 'bad', '1'.repeat(17), '2'.repeat(17), '3'.repeat(17), '4'.repeat(17), '5'.repeat(17)] },
  ]);
  assert.equal(pings[0].pingRoleIds, undefined, 'a DM reply never pings');
  assert.equal(pings[1].pingRoleIds.length, 5);
  assert.equal(new Set(pings[1].pingRoleIds).size, 5, 'deduped');

  assert.deepEqual(
    getEffectiveActions({ actions: null, content: 'x', useEmbed: true }),
    [{ type: 'reply', dm: false, embed: true, responses: ['x'] }],
  );
  assert.deepEqual(getEffectiveActions({ id: 'u', guildId: 'g', actions: { version: 2 }, content: 'legacy', useEmbed: false }), legacyActions({ content: 'legacy' }));
  assert.deepEqual(
    syncLegacyColumns([{ type: 'addRole', roleId: ROLE_A }, { type: 'send', channelId: CH, responses: ['a', 'b'], embed: false }]),
    { content: 'a', useEmbed: false },
  );
  assert.deepEqual(syncLegacyColumns([{ type: 'addRole', roleId: ROLE_A }]), {});

  const list = [
    { type: 'reply', dm: true, responses: ['a'] },
    { type: 'send', channelId: CH, responses: ['b'] },
    { type: 'reply', dm: false, responses: ['c'] },
  ];
  assert.equal(primaryMessageIndex(list), 0);
  assert.equal(channelReplyIndex(list), 2);
  assert.equal(primaryMessageIndex([{ type: 'addRole', roleId: ROLE_A }]), -1);
  assert.equal(channelReplyIndex([]), -1);
});

// ── 4. Usage and arguments ────────────────────────────────────────────────────

test('4. usage text, argument plans and random picks', () => {
  const tag = (content) => ({ id: 't', guildId: GID, name: 'greet', content, useEmbed: false, actions: null });
  assert.equal(buildUsage(tag('Hi {1}, {2:friend} {3...}'), { prefix: '!' }), '!greet <1> [more…]');
  assert.equal(buildUsage(tag('Hi {1}, {2:friend} {3...}'), { slash: true }), '/tag use name:greet args:<1> [more…]');
  assert.equal(buildUsage({ ...tag('{1} and {2}'), name: 'x' }, { prefix: '!' }), '!x <1> <2>');
  assert.equal(buildUsage({ ...tag('{2}'), name: 'x' }, { prefix: '!' }), '!x <1> <2>', 'arguments are positional');
  assert.equal(buildUsage({ ...tag('no args'), name: 'x' }, { prefix: '!' }), '!x');
  assert.equal(buildUsage({ ...tag('{1}'), name: 'x' }, { prefix: null }), '/tag use name:x args:<1>', 'prefix off falls back to the slash form');

  assert.deepEqual(planRun(tag('{1} {2}'), { args: ['a'] }, () => 0).missing, [2]);
  assert.deepEqual(planRun(tag('{1} {2}'), { args: ['a', 'b'] }, () => 0).missing, []);
  assert.equal(pickResponse(['a', 'b', 'c'], () => 0.99), 'c');
  assert.equal(pickResponse(['a', 'b', 'c'], () => 0), 'a');
  assert.equal(pickResponse(['a'], () => 5), 'a', 'the index is clamped');
});

// ── 5. Payloads and the slash executor ────────────────────────────────────────

describe('5. payloads', () => {
  const user = makeUser();
  const guild = makeGuild();
  const member = makeMember(guild, user);
  const renderCtx = { user, member, guild };

  test('mentions: users only, roles only through pingRoleIds, never everyone', () => {
    const plain = buildMessagePayload({ type: 'reply', responses: ['x'] }, '{user} hi', renderCtx, null);
    assert.deepEqual(plain.allowedMentions, { parse: ['users'] });
    const ping = buildMessagePayload({ type: 'reply', responses: ['x'], pingRoleIds: [ROLE_A] }, '{user} hi', renderCtx, null);
    assert.deepEqual(ping.allowedMentions, { parse: ['users'], roles: [ROLE_A] });
    for (const p of [plain, ping]) {
      assert.ok(!p.allowedMentions.parse.includes('everyone'));
      assert.ok(!p.allowedMentions.parse.includes('roles'));
    }
    const filtered = buildMessagePayload({ type: 'reply', responses: ['x'], pingRoleIds: [ROLE_A] }, 'hi', renderCtx, null, { pingRoleIds: [] });
    assert.deepEqual(filtered.allowedMentions, { parse: ['users'] }, 'the send-time filter wins over the stored ids');
  });

  test('output is clamped after expansion and empty results give null', () => {
    const long = buildMessagePayload({ type: 'reply', responses: ['x'] }, `${'x'.repeat(1990)}{user.avatar}`, renderCtx, null);
    assert.ok(long.content.length <= 2000);
    const embed = buildMessagePayload({ type: 'reply', embed: true, responses: ['x'] }, `${'x'.repeat(4000)}{user.avatar}`, renderCtx, null);
    assert.ok(embed.embeds[0].data.description.length <= 4096);
    assert.equal(buildMessagePayload({ type: 'reply', responses: ['x'] }, '{server.icon}', renderCtx, null), null);
  });

  test('a legacy over-length row is clamped to 2000, not dropped (R030)', () => {
    const a = getEffectiveActions({ actions: null, content: 'y'.repeat(2500), useEmbed: false })[0];
    const p = buildMessagePayload(a, a.responses[0], renderCtx, null);
    assert.equal(p.content.length, 2000);
  });

  test('the guild embed colour is used for embed output, with a safe fallback', () => {
    const colour = buildMessagePayload({ type: 'reply', embed: true, responses: ['x'] }, 'hi', renderCtx, { embedColor: 0x123456 });
    assert.equal(colour.embeds[0].data.color, 0x123456);
    const bad = buildMessagePayload({ type: 'reply', embed: true, responses: ['x'] }, 'hi', renderCtx, { embedColor: -5 });
    assert.equal(bad.embeds[0].data.color, 0x5865f2);
  });
});

test('5. slash fallback: an unsendable primary reply never leaves the interaction unacknowledged', async () => {
  const { guild, channel, user, member } = scenario();
  const tag = tagRow({
    deleteInvocation: false,
    actions: [{ type: 'reply', responses: ['{server.icon}'] }, { type: 'addRole', roleId: ROLE_A }],
  });
  const interaction = makeInteraction({ sub: 'use', guild, member, channel, user });
  const res = await runTag(tag, {
    source: 'slash', interaction, guild, channel, member, user, args: [], settings: null, prefix: '!', checkPermissions: false,
  });
  assert.equal(res.status, 'ran');
  assert.equal(res.success, false);
  assert.equal(interaction.calls.reply.length, 0);
  assert.equal(interaction.calls.deferReply.length, 1);
  assert.equal(interaction.calls.deferReply[0].ephemeral, true);
  assert.equal(interaction.calls.editReply.length, 1);
  assert.equal(interaction.calls.followUp.length, 0);
  const text = embedData(interaction.calls.editReply[0]).description;
  assert.ok(text.includes(FAILURE_TEXT.EMPTY), text);
  assert.ok(text.includes(FAILURE_TEXT.ROLE_GONE), text);
});

test('5. slash: the primary reply goes out first, other actions follow, failures arrive as an ephemeral follow-up', async () => {
  const { guild, channel, user, member } = scenario();
  const tag = tagRow({
    deleteInvocation: false,
    actions: [{ type: 'addRole', roleId: ROLE_A }, { type: 'reply', responses: ['Hi {user}'] }, { type: 'reply', dm: true, responses: ['psst'] }],
  });
  const interaction = makeInteraction({ sub: 'use', guild, member, channel, user });
  const res = await runTag(tag, {
    source: 'slash', interaction, guild, channel, member, user, args: [], settings: null, prefix: '!', checkPermissions: false,
  });
  assert.equal(res.success, true);
  assert.equal(interaction.calls.deferReply.length, 0, 'no defer: the reply is the acknowledgement');
  assert.equal(interaction.calls.reply.length, 1);
  assert.equal(interaction.calls.reply[0].content, `Hi <@${U}>`);
  assert.deepEqual(interaction.calls.reply[0].allowedMentions, { parse: ['users'] });
  assert.equal(user.dms.length, 1);
  assert.equal(interaction.calls.followUp.length, 1);
  assert.equal(interaction.calls.followUp[0].ephemeral, true);
  assert.ok(embedData(interaction.calls.followUp[0]).description.includes(FAILURE_TEXT.ROLE_GONE));
});

// ── 6. Scope and access ───────────────────────────────────────────────────────

test('6. channel scope ids and the access matrix', () => {
  assert.deepEqual(channelScopeIds({ id: 't', isThread: () => true, parentId: 'c', parent: { parentId: 'cat' } }), ['t', 'c', 'cat']);
  assert.deepEqual(channelScopeIds({ id: 'c', isThread: () => false, parentId: 'cat' }), ['c', 'cat']);
  assert.deepEqual(channelScopeIds(null), []);

  const thread = { id: 't', isThread: () => true, parentId: 'c', parent: { parentId: 'cat' } };
  const channel = { id: 'c', isThread: () => false, parentId: 'cat' };
  const member = { roles: { cache: new Map([['r1', {}]]) } };
  const base = { allowRoleIds: [], denyRoleIds: [], allowChannelIds: [], denyChannelIds: [] };
  const check = (over, scope) => checkTagAccess({ ...base, ...over }, scope);

  assert.deepEqual(check({ denyChannelIds: ['c'] }, { member, channel: thread }), { ok: false, code: 'CHANNEL_DENIED' });
  assert.deepEqual(check({ allowChannelIds: ['cat'] }, { member, channel }), { ok: true });
  assert.deepEqual(check({ allowChannelIds: ['x'] }, { member, channel }), { ok: false, code: 'CHANNEL_NOT_ALLOWED' });
  assert.deepEqual(check({ denyRoleIds: ['r1'] }, { member, channel }), { ok: false, code: 'ROLE_DENIED' });
  assert.deepEqual(check({ allowRoleIds: ['r2'] }, { member, channel }), { ok: false, code: 'ROLE_NOT_ALLOWED' });
  assert.deepEqual(check({ allowRoleIds: ['r1'] }, { member, channel }), { ok: true });
  assert.deepEqual(check({ allowRoleIds: ['r2'] }, { member: null, channel }), { ok: false, code: 'ROLE_NOT_ALLOWED' });
  assert.deepEqual(check({}, { member: null, channel }), { ok: true });
  assert.deepEqual(check({ denyChannelIds: ['c'], allowRoleIds: ['r1'] }, { member, channel }), { ok: false, code: 'CHANNEL_DENIED' }, 'channel rules come first');
  // The role half alone decides what a member may read with /tag info, raw, list and action list.
  assert.deepEqual(checkTagRoles({ ...base, allowRoleIds: ['r2'], denyChannelIds: ['c'] }, member), { ok: false, code: 'ROLE_NOT_ALLOWED' });
  assert.deepEqual(checkTagRoles({ ...base, denyRoleIds: ['r1'] }, member), { ok: false, code: 'ROLE_DENIED' });
  assert.deepEqual(checkTagRoles({ ...base, allowRoleIds: ['r1'], denyChannelIds: ['c'] }, member), { ok: true }, 'channel rules do not hide a command');
  // /help keeps a copy (core must not import tagService); the two must agree.
  for (const over of [{}, { allowRoleIds: ['r1'] }, { allowRoleIds: ['r2'] }, { denyRoleIds: ['r1'] }, { denyRoleIds: ['r2'], allowRoleIds: ['r1'] }]) {
    for (const who of [member, null]) assert.equal(tagRolesAllow({ ...base, ...over }, who), checkTagRoles({ ...base, ...over }, who).ok, JSON.stringify(over));
  }

  assert.equal(triggerAppliesHere({ channelIds: ['c'], exemptRoleIds: [] }, { channel: thread, member }), true, 'a thread matches its parent (R282)');
  assert.equal(triggerAppliesHere({ channelIds: ['cat'], exemptRoleIds: [] }, { channel, member }), true, 'a category matches its channels');
  assert.equal(triggerAppliesHere({ channelIds: ['other'], exemptRoleIds: [] }, { channel, member }), false);
  assert.equal(triggerAppliesHere({ channelIds: [], exemptRoleIds: ['r1'] }, { channel, member }), false, 'an exempt role is skipped');
  assert.equal(triggerAppliesHere({ channelIds: [], exemptRoleIds: ['gone'] }, { channel, member }), true, 'stale ids never match');
});

// ── 7. Trigger validation and selection ───────────────────────────────────────

test('7. trigger pattern validation', () => {
  assert.equal(validateTriggerPattern('(a+)+', 'REGEX', false).ok, false);
  assert.equal(validateTriggerPattern('hello', 'CONTAINS', false).ok, true);
  assert.equal(validateTriggerPattern('', 'EXACT', false).ok, false);
  assert.equal(validateTriggerPattern('   ', 'EXACT', false).ok, false);
  assert.equal(validateTriggerPattern('x'.repeat(501), 'CONTAINS', false).ok, false);
  assert.equal(validateTriggerPattern('^hi\\s+there$', 'REGEX', true).ok, true);
  assert.equal(validateTriggerPattern('(a', 'REGEX', false).ok, false, 'syntax errors');
  assert.equal(validateTriggerPattern('(a)\\1', 'REGEX', false).ok, false, 'backreferences');
  assert.equal(validateTriggerPattern('x'.repeat(201), 'REGEX', false).ok, false, 'regexes are capped at 200 characters');
  assert.equal(validateTriggerPattern('x', 'WAT', false).ok, false);
});

test('7. trigger selection keeps creation order and runs regexes safely', async () => {
  const guild = makeGuild();
  const channel = makeChannel(guild);
  const scope = { channel, member: null };
  const pick = async (triggers, content, sc = scope) => {
    resetSvc();
    __testing.primeTriggers(GID, triggers);
    const set = await getTriggerSet(GID);
    return selectTrigger(set.entries, content, sc)?.id ?? null;
  };

  assert.equal(await pick([proj({ id: 'A', trigger: 'hello' }), proj({ id: 'B', trigger: 'hel+o', matchType: 'REGEX' })], 'hello'), 'A');
  assert.equal(await pick([proj({ id: 'A', trigger: 'hel+o', matchType: 'REGEX' }), proj({ id: 'B', trigger: 'hello' })], 'hello'), 'A');
  assert.equal(await pick([proj({ id: 'A', trigger: 'bye' }), proj({ id: 'B', trigger: 'h.llo', matchType: 'REGEX' })], 'hello'), 'B');
  assert.equal(
    await pick([proj({ id: 'A', trigger: 'hello', channelIds: ['other'] }), proj({ id: 'B', trigger: 'hell' })], 'hello'),
    'B',
    'a trigger outside its channel is skipped',
  );
  assert.equal(await pick([proj({ id: 'A', trigger: 'nope' })], 'hello'), null);

  assert.equal(matchesPlain(proj({ matchType: 'EXACT', trigger: 'hi' }), 'hi '), true, 'EXACT compares trimmed text');
  assert.equal(matchesPlain(proj({ matchType: 'EXACT', trigger: 'hi' }), 'hi there'), false);
  assert.equal(matchesPlain(proj({ trigger: 'HELLO' }), 'say hello'), true, 'case-insensitive by default');
  assert.equal(matchesPlain(proj({ trigger: 'hello', caseSensitive: true }), 'HELLO'), false);
  assert.equal(matchesPlain(proj({ matchType: 'STARTS_WITH', trigger: 'he' }), 'hello'), true);
  assert.equal(matchesPlain(proj({ matchType: 'ENDS_WITH', trigger: 'lo' }), 'hello'), true);
  assert.equal(matchesPlain(proj({ matchType: 'REGEX', trigger: 'h' }), 'hello'), false, 'regexes never go through matchesPlain');

  // A legacy row with a catastrophic pattern compiles to null and is skipped without blocking.
  __testing.reset();
  __testing.primeTriggers(GID, [proj({ id: 'L', trigger: '(a+)+$', matchType: 'REGEX' }), proj({ id: 'B', trigger: 'a' })]);
  const set = await getTriggerSet(GID);
  assert.equal(set.entries[0].re, null);
  const t0 = Date.now();
  const hit = selectTrigger(set.entries, `${'a'.repeat(40)}!`, scope);
  assert.ok(Date.now() - t0 < 500, 'returned quickly');
  assert.equal(hit.id, 'B');
});

test('7. R010/R048: many safe regexes cost one batched, time-limited call per message', async () => {
  const guild = makeGuild();
  const channel = makeChannel(guild);
  const triggers = Array.from({ length: 60 }, (_, i) => proj({ id: `r${i}`, trigger: `^word${i}\\b`, matchType: 'REGEX' }));
  __testing.primeTriggers(GID, triggers);
  const set = await getTriggerSet(GID);
  const t0 = Date.now();
  for (let i = 0; i < 20; i++) selectTrigger(set.entries, 'nothing that matches here', { channel, member: null });
  assert.ok(Date.now() - t0 < 1000, `20 messages x 60 regexes took ${Date.now() - t0} ms`);
  assert.equal(selectTrigger(set.entries, 'word42 is it', { channel, member: null }).id, 'r42');
});

// ── 8. Cooldown (Redis down) ──────────────────────────────────────────────────

test('8. cooldowns fail closed while Redis is down; keys follow the scope', async () => {
  assert.deepEqual(await acquireCooldown('tag:cd:g:t', 30), { ok: false, unavailable: true });
  assert.equal(tagCooldownKey({ id: 't', cooldownScope: 'NONE', cooldownSeconds: 0 }, 'g', 'u'), null);
  assert.equal(tagCooldownKey({ id: 't', cooldownScope: 'SERVER', cooldownSeconds: 30 }, 'g', 'u'), 'tag:cd:g:t');
  assert.equal(tagCooldownKey({ id: 't', cooldownScope: 'USER', cooldownSeconds: 30 }, 'g', 'u'), 'tag:cd:g:t:u');
  assert.equal(tagCooldownKey({ id: 't', cooldownScope: 'USER', cooldownSeconds: 0 }, 'g', 'u'), null, '0 seconds is no cooldown');
  assert.equal(cooldownLabel({ cooldownScope: 'NONE', cooldownSeconds: 0 }), 'None');
  assert.equal(cooldownLabel({ cooldownScope: 'SERVER', cooldownSeconds: 90 }), 'Server-wide · 1 minute 30 seconds');
  assert.equal(cooldownLabel({ cooldownScope: 'USER', cooldownSeconds: 3600 }), 'Per user · 1 hour');
});

// ── 9. The prefix hook ────────────────────────────────────────────────────────

describe('9. prefix hook', () => {
  /** Runs one invocation with fresh state. */
  async function invoke({ content = '!hi Bob', row = {}, rows = null, settings = SETTINGS, deleteImpl = null } = {}) {
    resetSvc();
    const { guild, channel, user, member } = scenario();
    __testing.primeTags(GID, rows ?? [tagRow(row)]);
    const message = makeMessage({ content, channel, user, member, guild, deleteImpl });
    const ctx = makeCtx({ guild, message, settings });
    await handlePrefixInvocation(ctx, { checkPermissions: false });
    return { ctx, message, channel, user };
  }

  test('runs the command, deletes the usage and stops the pipeline', async () => {
    const { ctx, message, channel } = await invoke();
    assert.equal(channel.sent.length, 1);
    assert.equal(channel.sent[0].content, `Hello <@${U}>! Bob`);
    assert.deepEqual(channel.sent[0].allowedMentions, { parse: ['users'] });
    assert.equal(message.deleted, 1);
    assert.equal(wasDeletedByBot(message.id), 'custom-command');
    assert.equal(ctx.stopped, true);
    assert.equal(ctx.state.customCommand.name, 'hi');
    assert.equal(ctx.state.customCommand.tagId, 't1');
  });

  test('an unknown name is silent and leaves the message to later hooks', async () => {
    const { ctx, channel } = await invoke({ content: '!unknown' });
    assert.equal(channel.sent.length, 0);
    assert.equal(ctx.stopped, false);
    assert.equal(ctx.state.customCommand, undefined);
  });

  test('another prefix, a disabled command and an off prefix do nothing', async () => {
    assert.equal((await invoke({ content: '?hi' })).channel.sent.length, 0);
    const disabled = await invoke({ row: { enabled: false } });
    assert.equal(disabled.channel.sent.length, 0);
    assert.equal(disabled.ctx.state.customCommand, undefined);
    const off = await invoke({ settings: { ...SETTINGS, customPrefix: '' } });
    assert.equal(off.channel.sent.length, 0, 'prefix commands are off');
    const custom = await invoke({ content: '>>hi Bob', settings: { ...SETTINGS, customPrefix: '>>' } });
    assert.equal(custom.channel.sent.length, 1);
  });

  test('a delete that fails keeps the pipeline going', async () => {
    const { ctx, channel, message } = await invoke({ deleteImpl: async () => { throw err(50013); } });
    assert.equal(channel.sent.length, 1);
    assert.equal(message.deleted, 1);
    assert.equal(ctx.stopped, false);
  });

  test('an already deleted usage (10008) counts as deleted', async () => {
    const { ctx } = await invoke({ deleteImpl: async () => { throw err(10008); } });
    assert.equal(ctx.stopped, true);
  });

  test('deleteInvocation off: no delete and no stop', async () => {
    const { ctx, channel, message } = await invoke({ row: { deleteInvocation: false } });
    assert.equal(channel.sent.length, 1);
    assert.equal(message.deleted, 0);
    assert.equal(ctx.stopped, false);
  });

  test('a command with a cooldown does not run while Redis is down (fail closed)', async () => {
    const { channel } = await invoke({ row: { cooldownScope: 'USER', cooldownSeconds: 30 } });
    assert.equal(channel.sent.length, 0);
  });

  test('access lists are enforced, silently', async () => {
    resetSvc();
    const { guild, channel, user, member } = scenario();
    __testing.primeTags(GID, [tagRow({ denyChannelIds: [channel.id] })]);
    const message = makeMessage({ content: '!hi', channel, user, member, guild });
    const ctx = makeCtx({ guild, message });
    await handlePrefixInvocation(ctx, { checkPermissions: false });
    assert.equal(channel.sent.length, 0);
    assert.equal(message.replies.length, 0);
    assert.equal(ctx.state.customCommand.name, 'hi', 'the message still counts as a command invocation');
  });

  test('missing arguments answer with the usage and delete themselves later', async () => {
    const { channel, message } = await invoke({ content: '!hi', row: { content: 'Hi {1}', deleteInvocation: false } });
    assert.equal(channel.sent.length, 0);
    assert.equal(message.replies.length, 1);
    const embed = message.replies[0].embeds[0].data;
    assert.match(embed.description, /Usage: `!hi <1>`/);
    assert.deepEqual(message.replies[0].allowedMentions, { parse: [], repliedUser: false });
  });

  test('a user is limited to one invocation per second', async () => {
    resetSvc();
    const { guild, channel, user, member } = scenario();
    __testing.primeTags(GID, [tagRow({ deleteInvocation: false })]);
    for (let n = 0; n < 2; n++) {
      const message = makeMessage({ content: '!hi', channel, user, member, guild });
      await handlePrefixInvocation(makeCtx({ guild, message }), { checkPermissions: false });
    }
    assert.equal(channel.sent.length, 1);
  });

  test('a message the bot already deleted is skipped', async () => {
    resetSvc();
    const { guild, channel, user, member } = scenario();
    __testing.primeTags(GID, [tagRow()]);
    const message = makeMessage({ content: '!hi', channel, user, member, guild });
    markBotDeletion(message.id, 'automod');
    await handlePrefixInvocation(makeCtx({ guild, message }), { checkPermissions: false });
    assert.equal(channel.sent.length, 0);
  });

  test('the sync filter', () => {
    assert.equal(isPossibleInvocation({ guildId: 'nomemo', message: { content: '!hi' } }), true);
    assert.equal(isPossibleInvocation({ guildId: 'nomemo', message: { content: 'h' } }), false);
    assert.equal(isPossibleInvocation({ guildId: 'nomemo', message: { content: '' } }), false);
    assert.equal(isPossibleInvocation({ guildId: 'nomemo', message: {} }), false);
  });
});

// ── 10. The trigger hook ──────────────────────────────────────────────────────

describe('10. trigger hook', () => {
  async function fire({
    content = 'well hello there', trig = {}, state = {}, sendImpl = null, deleteImpl = null, settings = SETTINGS, channelId = CH,
  } = {}) {
    const guild = makeGuild();
    const channel = makeChannel(guild, channelId, { sendImpl });
    const user = makeUser();
    const member = makeMember(guild, user);
    __testing.primeTriggers(GID, [proj({
      response: 'Hi {user} @everyone <@&222222222222222222>', deleteMessage: true, ...trig,
    })]);
    const message = makeMessage({ content, channel, user, member, guild, deleteImpl });
    const ctx = makeCtx({ guild, message, settings });
    Object.assign(ctx.state, state);
    await handleTriggers(ctx, { checkPermissions: false });
    return { ctx, message, channel };
  }

  test('sends a safe reply, then deletes the trigger message, without stopping the pipeline', async () => {
    const { ctx, message, channel } = await fire();
    assert.equal(channel.sent.length, 1);
    assert.deepEqual(channel.sent[0].allowedMentions, { parse: ['users'] });
    assert.ok(channel.sent[0].content.length <= 2000);
    assert.ok(channel.sent[0].content.startsWith(`Hi <@${U}>`));
    assert.equal(message.deleted, 1);
    assert.equal(wasDeletedByBot(message.id), 'trigger');
    assert.equal(ctx.stopped, false);
  });

  test('a response of 2000 characters is clamped after expansion (R221)', async () => {
    const { channel } = await fire({ trig: { response: `${'x'.repeat(1995)}{user}` } });
    assert.equal(channel.sent[0].content.length, 2000);
  });

  test('a message already answered by a custom command is skipped', async () => {
    const { channel } = await fire({ state: { customCommand: { tagId: 't1', name: 'hi' } } });
    assert.equal(channel.sent.length, 0);
  });

  test('a failed send never deletes the message', async () => {
    const { channel, message } = await fire({ sendImpl: async () => { throw err(50013); } });
    assert.equal(channel.sent.length, 0);
    assert.equal(message.deleted, 0);
  });

  test('a trigger with a cooldown does not fire while Redis is down (fail closed)', async () => {
    const { channel } = await fire({ trig: { cooldown: 30 } });
    assert.equal(channel.sent.length, 0);
  });

  test('triggers switched off in the settings stay silent', async () => {
    const { channel } = await fire({ settings: { ...SETTINGS, autoTriggersEnabled: false } });
    assert.equal(channel.sent.length, 0);
  });

  test('an empty result releases the cooldown path and sends nothing', async () => {
    const { channel } = await fire({ trig: { response: '{server.icon}' } });
    assert.equal(channel.sent.length, 0);
  });

  test('at most one reply per channel per second', async () => {
    const guild = makeGuild();
    const channel = makeChannel(guild);
    const user = makeUser();
    const member = makeMember(guild, user);
    __testing.primeTriggers(GID, [proj({ deleteMessage: false })]);
    for (let n = 0; n < 2; n++) {
      const message = makeMessage({ content: 'hello', channel, user, member, guild });
      await handleTriggers(makeCtx({ guild, message }), { checkPermissions: false });
    }
    assert.equal(channel.sent.length, 1);
  });

  test('the first applicable trigger wins even when another one matches too', async () => {
    const guild = makeGuild();
    const channel = makeChannel(guild);
    const user = makeUser();
    const member = makeMember(guild, user, [ROLE_A]);
    __testing.primeTriggers(GID, [
      proj({ id: 'x', trigger: 'hello', response: 'exempt', exemptRoleIds: [ROLE_A] }),
      proj({ id: 'y', trigger: 'hello', response: 'second' }),
      proj({ id: 'z', trigger: 'hello', response: 'third' }),
    ]);
    const message = makeMessage({ content: 'hello', channel, user, member, guild });
    await handleTriggers(makeCtx({ guild, message }), { checkPermissions: false });
    assert.deepEqual(channel.sent.map((p) => p.content), ['second']);
  });

  test('the sync filter', () => {
    assert.equal(hasTriggerContent({ message: { content: 'x' }, state: { customCommand: {} } }), false);
    assert.equal(hasTriggerContent({ message: { content: 'x' }, state: {} }), true);
    assert.equal(hasTriggerContent({ message: { content: '' }, state: {} }), false);
    assert.equal(hasTriggerContent({ message: {}, state: {} }), false);
  });
});

// ── 11. Payload budgets and exports ───────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('11. command payloads, exports and permission keys', () => {
  const tagJson = buildCommandPayload(tagCmd);
  assert.ok(charCount(tagJson) <= 3500, `/tag is ${charCount(tagJson)} characters`);
  assert.ok(tagJson.options.length <= 25);
  for (const o of tagJson.options) assert.ok((o.options?.length ?? 0) <= 25);

  assert.equal(tagCmd.module, 'customcommands');
  assert.equal(tagCmd.gated, false, 'the trigger group works while the module is off; execute() gates the rest');
  assert.equal(tagCmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.equal(tagJson.default_member_permissions, undefined, 'members must see /tag use');
  assert.equal(typeof tagCmd.autocomplete, 'function');

  const valid = new Set();
  for (const o of tagJson.options) {
    if (o.type === 1) valid.add(o.name);
    if (o.type === 2) for (const s of o.options) valid.add(`${o.name} ${s.name}`);
  }
  for (const key of Object.keys(tagCmd.subcommandPermissions)) assert.ok(valid.has(key), `${key} is a real subcommand`);
  for (const key of ['action add', 'action remove', 'response add', 'response remove', 'create', 'edit', 'delete', 'toggle', 'cooldown', 'permissions']) {
    assert.equal(tagCmd.subcommandPermissions[key], PERMISSION_LEVELS.ADMIN, key);
  }
  assert.equal(tagCmd.subcommandPermissions['action list'], PERMISSION_LEVELS.MEMBER);
  for (const sub of ['enable', 'disable', 'add', 'edit', 'remove', 'toggle', 'info', 'list']) {
    assert.equal(tagCmd.subcommandPermissions[`trigger ${sub}`], PERMISSION_LEVELS.ADMIN, `trigger ${sub}`);
  }
  for (const read of ['use', 'info', 'raw', 'list', 'placeholders']) {
    assert.ok(!(read in tagCmd.subcommandPermissions) || tagCmd.subcommandPermissions[read] === PERMISSION_LEVELS.MEMBER, read);
  }

  const trigGroup = tagJson.options.find((o) => o.name === 'trigger');
  assert.equal(trigGroup.type, 2);
  assert.deepEqual(trigGroup.options.map((o) => o.name), ['enable', 'disable', 'add', 'edit', 'remove', 'toggle', 'info', 'list']);
});

// ── 12. Plugin ────────────────────────────────────────────────────────────────

test('12. the plugin declares exactly the two hooks', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('custom-commands')), []);
  const p = plugins.find((x) => x.id === 'custom-commands');
  assert.ok(p, 'collected');
  assert.equal(p.module, 'customcommands');
  assert.equal(plugin.hooks.length, 2);
  const [prefixHook, triggerHook] = plugin.hooks;
  assert.equal(prefixHook.event, 'messageCreate');
  assert.equal(prefixHook.id, 'custom-commands');
  assert.equal(prefixHook.timeoutMs, 15000);
  assert.equal(triggerHook.event, 'messageCreate');
  assert.equal(triggerHook.id, 'custom-commands:triggers');
  assert.equal(plugin.module, 'customcommands');
  assert.deepEqual(plugin.components, []);
  assert.deepEqual(plugin.jobs, []);
  assert.equal(prefixHook.filter({ guildId: 'nomemo', message: { content: 'x' } }), false, 'too short to be an invocation');
});

// ── 13. Static checks ─────────────────────────────────────────────────────────

test('13. static checks on the owned files', () => {
  const files = ['src/services/tagService.js', 'src/plugins/custom-commands.js', 'src/commands/utility/tag.js'];
  for (const f of files) {
    const text = readFileSync(resolve(ROOT, f), 'utf8');
    assert.ok(!/new RegExp\(/.test(text), `${f}: no new RegExp`);
    assert.ok(!/\bconsole\./.test(text), `${f}: no console`);
    assert.ok(!/guildSettings\./.test(text), `${f}: no prisma.guildSettings`);
    assert.ok(!/cache\.del\(`guild:\$\{[^}]*\}:settings/.test(text), `${f}: no settings cache deletes`);
  }
  const service = readFileSync(resolve(ROOT, files[0]), 'utf8');
  assert.ok(!service.includes('matchesTrigger') && !service.includes('buildTagPayload'), 'the old exports are gone');
  assert.ok(!/LIMITS\.[A-Z_]+\s*\?\?/.test(service), 'no LIMITS fallbacks');
});

// ── 14. The executor: roles, DMs, sends, pings ────────────────────────────────

describe('14. runTag', () => {
  const run = (tag, ctx, extra = {}) => runTag(tag, {
    source: 'prefix', guild: ctx.guild, channel: ctx.channel, member: ctx.member, user: ctx.user,
    message: extra.message ?? makeMessage({ content: '!hi', ...ctx }), args: [], settings: null, prefix: '!', checkPermissions: false, ...extra,
  });

  test('roles are added and removed one at a time, with the command name as the reason', async () => {
    const sc = scenario({ roles: [makeRole(ROLE_A), makeRole(ROLE_B)], memberRoles: [ROLE_B] });
    const tag = tagRow({
      deleteInvocation: false,
      actions: [{ type: 'addRole', roleId: ROLE_A }, { type: 'removeRole', roleId: ROLE_B }, { type: 'toggleRole', roleId: ROLE_A }],
    });
    const res = await run(tag, sc);
    assert.equal(res.success, true);
    assert.deepEqual(sc.member.calls, [
      ['add', ROLE_A, 'Custom command hi'],
      ['remove', ROLE_B, 'Custom command hi'],
      ['remove', ROLE_A, 'Custom command hi'],
    ]);
    for (const [, id] of sc.member.calls) assert.equal(typeof id, 'string', 'never the array form');
  });

  test('a role the member already has (or lacks) is a success without an API call', async () => {
    const sc = scenario({ roles: [makeRole(ROLE_A), makeRole(ROLE_B)], memberRoles: [ROLE_A] });
    const res = await run(tagRow({ deleteInvocation: false, actions: [{ type: 'addRole', roleId: ROLE_A }, { type: 'removeRole', roleId: ROLE_B }] }), sc);
    assert.equal(res.success, true);
    assert.deepEqual(sc.member.calls, []);
  });

  test('unassignable, missing and managed roles fail individually', async () => {
    const sc = scenario({ roles: [makeRole(ROLE_A, { managed: true }), makeRole(ROLE_B, { position: 500 })] });
    const res = await run(tagRow({
      deleteInvocation: false,
      actions: [{ type: 'addRole', roleId: ROLE_A }, { type: 'addRole', roleId: ROLE_B }, { type: 'addRole', roleId: '888888888888888888' }],
    }), sc);
    assert.equal(res.success, false);
    assert.deepEqual(res.results.map((r) => r.code), ['ROLE_UNASSIGNABLE', 'ROLE_UNASSIGNABLE', 'ROLE_GONE']);
    assert.deepEqual(sc.member.calls, []);
  });

  test('Discord errors map to friendly codes', async () => {
    const sc = scenario({ roles: [makeRole(ROLE_A)] });
    sc.member.roles.add = async () => { throw err(50013); };
    sc.user.dmError = err(50007);
    sc.channel.send = async () => { throw err(10003); };
    const res = await run(tagRow({
      deleteInvocation: false,
      actions: [{ type: 'addRole', roleId: ROLE_A }, { type: 'reply', dm: true, responses: ['x'] }, { type: 'reply', responses: ['y'] }],
    }), sc);
    assert.deepEqual(res.results.map((r) => r.code), ['MISSING_PERMS', 'DM_CLOSED', 'CHANNEL_GONE']);
    assert.equal(res.success, false);
  });

  test('send actions post to the target channel and report gone or unusable ones', async () => {
    const sc = scenario();
    const target = makeChannel(sc.guild, CH2);
    const locked = makeChannel(sc.guild, '888888888888888881', { hasPerm: () => false });
    const res = await runTag(tagRow({
      deleteInvocation: false,
      actions: [
        { type: 'send', channelId: CH2, responses: ['to target'] },
        { type: 'send', channelId: '888888888888888882', responses: ['gone'] },
        { type: 'send', channelId: locked.id, responses: ['locked'] },
      ],
    }), {
      source: 'prefix', guild: sc.guild, channel: sc.channel, member: sc.member, user: sc.user, args: [], settings: null,
      prefix: '!', message: makeMessage({ content: '!hi', ...sc }), checkPermissions: true,
    });
    assert.deepEqual(res.results.map((r) => r.ok), [true, false, false]);
    assert.equal(res.results[1].code, 'CHANNEL_GONE');
    assert.equal(res.results[2].code, 'MISSING_PERMS');
    assert.equal(target.sent.length, 1);
    assert.equal(target.sent[0].content, 'to target');
    assert.equal(locked.sent.length, 0);
  });

  test('role pings follow pingableRoleIds at send time, with the bot as the sender', async () => {
    const reply = (ids) => ({ deleteInvocation: false, actions: [{ type: 'reply', responses: ['hi'], pingRoleIds: ids }] });
    // 1. not mentionable and the bot lacks Mention Everyone: dropped.
    let sc = scenario({ roles: [makeRole(ROLE_A)] });
    sc.channel.permissionsFor = () => ({ has: () => false });
    await run(tagRow(reply([ROLE_A])), sc);
    assert.deepEqual(sc.channel.sent[0].allowedMentions, { parse: ['users'] });
    // 2. mentionable: allowed.
    sc = scenario({ roles: [makeRole(ROLE_A, { mentionable: true })] });
    sc.channel.permissionsFor = () => ({ has: () => false });
    await run(tagRow(reply([ROLE_A])), sc);
    assert.deepEqual(sc.channel.sent[0].allowedMentions, { parse: ['users'], roles: [ROLE_A] });
    // 3. the bot can mention everyone: allowed.
    sc = scenario({ roles: [makeRole(ROLE_A)] });
    await run(tagRow(reply([ROLE_A])), sc);
    assert.deepEqual(sc.channel.sent[0].allowedMentions, { parse: ['users'], roles: [ROLE_A] });
    // 4. a deleted role is never pinged.
    sc = scenario({ roles: [] });
    await run(tagRow(reply([ROLE_A])), sc);
    assert.deepEqual(sc.channel.sent[0].allowedMentions, { parse: ['users'] });
  });

  test('a prefix reply respects the channel permissions when they are checked', async () => {
    const sc = scenario();
    sc.channel.permissionsFor = () => ({ has: () => false });
    const res = await run(tagRow({ deleteInvocation: false }), sc, { checkPermissions: true });
    assert.equal(res.results[0].code, 'MISSING_PERMS');
    assert.equal(sc.channel.sent.length, 0);
  });

  test('access denied, empty actions and missing arguments answer slash users ephemerally', async () => {
    const sc = scenario();
    const slash = (tag, args = []) => {
      const interaction = makeInteraction({ sub: 'use', ...sc });
      return runTag(tag, {
        source: 'slash', interaction, guild: sc.guild, channel: sc.channel, member: sc.member, user: sc.user, args, settings: null,
        prefix: '!', checkPermissions: false,
      }).then((res) => ({ res, interaction }));
    };
    let r = await slash(tagRow({ denyChannelIds: [sc.channel.id] }));
    assert.equal(r.res.status, 'denied');
    assert.equal(r.interaction.calls.reply[0].ephemeral, true);
    assert.match(embedData(r.interaction.calls.reply[0]).description, /can't be used in this channel/);

    r = await slash(tagRow({ allowRoleIds: [ROLE_A] }));
    assert.match(embedData(r.interaction.calls.reply[0]).description, /don't have a role/);

    r = await slash(tagRow({ actions: [] }));
    assert.equal(r.res.status, 'empty');
    assert.match(embedData(r.interaction.calls.reply[0]).description, /no actions/);

    r = await slash(tagRow({ content: 'Hi {1}', actions: null }));
    assert.equal(r.res.status, 'usage');
    assert.match(embedData(r.interaction.calls.reply[0]).description, /Usage: `\/tag use name:hi args:<1>`/);

    r = await slash(tagRow({ cooldownScope: 'USER', cooldownSeconds: 30 }));
    assert.equal(r.res.status, 'unavailable', 'fail closed while Redis is down');
    assert.equal(r.interaction.calls.reply[0].ephemeral, true);
    assert.equal(r.interaction.calls.reply.length, 1);
  });

  test('a hand-edited empty action list stays silent for prefix use', async () => {
    const sc = scenario();
    const res = await run(tagRow({ actions: [] }), sc);
    assert.equal(res.status, 'empty');
    assert.equal(sc.channel.sent.length, 0);
  });

  test('random replies pick one response per run', async () => {
    const sc = scenario();
    const tag = tagRow({ deleteInvocation: false, actions: [{ type: 'reply', responses: ['one', 'two', 'three'] }] });
    await run(tag, sc, { rng: () => 0.5 });
    assert.equal(sc.channel.sent[0].content, 'two');
  });

  test('a successful delete is reported through the result', async () => {
    const sc = scenario();
    const message = makeMessage({ content: '!hi', ...sc });
    const res = await run(tagRow(), sc, { message });
    assert.equal(res.deleted, true);
    assert.equal(message.deleted, 1);
  });

  test('a failed run neither deletes the usage nor counts', async () => {
    const sc = scenario();
    sc.channel.send = async () => { throw err(50013); };
    const message = makeMessage({ content: '!hi', ...sc });
    const res = await run(tagRow(), sc, { message });
    assert.equal(res.success, false);
    assert.equal(res.deleted, false);
    assert.equal(message.deleted, 0);
  });
});

// ── 14b. Cooldown branches with a working store ───────────────────────────────

test('14. cooldown paths with a working store: take, refuse, release and fall-through (R170, R284, R285)', async () => {
  const { cache } = await import('../src/services/redis.js');
  const real = { setNX: cache.setNX, get: cache.get, del: cache.del };
  const store = new Map();
  const deleted = [];
  cache.setNX = async (key, value) => {
    if (store.has(key)) return false;
    store.set(key, value);
    return true;
  };
  cache.get = async (key) => store.get(key) ?? null;
  cache.del = async (...keys) => {
    for (const key of keys.flat()) { deleted.push(key); store.delete(key); }
    return true;
  };
  try {
    // acquireCooldown: the first caller wins, the second sees the stored expiry.
    assert.deepEqual(await acquireCooldown('k1', 30), { ok: true });
    const second = await acquireCooldown('k1', 30);
    assert.equal(second.ok, false);
    assert.ok(second.until > Date.now(), 'the refusal carries the stored expiry');

    // USER scope: per user; the second run of the same user gets the notice with a relative time.
    store.clear();
    const sc = scenario();
    const other = makeUser('121212121212121212');
    const otherMember = makeMember(sc.guild, other);
    const tag = tagRow({ deleteInvocation: false, cooldownScope: 'USER', cooldownSeconds: 30 });
    const go = (user, member, message = makeMessage({ content: '!hi', ...sc, user, member })) => runTag(tag, {
      source: 'prefix', guild: sc.guild, channel: sc.channel, member, user, message, args: [], settings: null, prefix: '!', checkPermissions: false,
    }).then((res) => ({ res, message }));
    let r = await go(sc.user, sc.member);
    assert.equal(r.res.status, 'ran');
    assert.ok(store.has(`tag:cd:${GID}:t1:${U}`));
    r = await go(sc.user, sc.member);
    assert.equal(r.res.status, 'cooldown');
    assert.match(r.message.replies[0].embeds[0].data.description, /You can use this command again <t:\d+:R>/);
    r = await go(other, otherMember);
    assert.equal(r.res.status, 'ran', 'another user has their own cooldown');
    assert.equal(sc.channel.sent.length, 2);

    // SERVER scope: one key for everybody, and the text says so.
    store.clear();
    const serverTag = tagRow({ id: 't2', deleteInvocation: false, cooldownScope: 'SERVER', cooldownSeconds: 30 });
    const runServer = (user, member) => {
      const message = makeMessage({ content: '!hi', ...sc, user, member });
      return runTag(serverTag, {
        source: 'prefix', guild: sc.guild, channel: sc.channel, member, user, message, args: [], settings: null, prefix: '!', checkPermissions: false,
      }).then((res) => ({ res, message }));
    };
    assert.equal((await runServer(sc.user, sc.member)).res.status, 'ran');
    r = await runServer(other, otherMember);
    assert.equal(r.res.status, 'cooldown');
    assert.match(r.message.replies[0].embeds[0].data.description, /This command can be used again <t:\d+:R>/);

    // A run where every action failed does not burn the cooldown (R285).
    store.clear();
    deleted.length = 0;
    const failing = scenario();
    failing.channel.send = async () => { throw err(50013); };
    const failTag = tagRow({ id: 't3', deleteInvocation: false, cooldownScope: 'USER', cooldownSeconds: 30 });
    const failRun = () => runTag(failTag, {
      source: 'prefix', guild: failing.guild, channel: failing.channel, member: failing.member, user: failing.user,
      message: makeMessage({ content: '!hi', ...failing }), args: [], settings: null, prefix: '!', checkPermissions: false,
    });
    assert.equal((await failRun()).success, false);
    assert.ok(deleted.includes(`tag:cd:${GID}:t3:${U}`), 'the key was released');
    assert.equal((await failRun()).status, 'ran', 'so the next attempt is not on cooldown');

    // Slash users are told when the command can be used again, ephemerally.
    store.clear();
    const slashSc = scenario();
    const slashTag = tagRow({ id: 't4', deleteInvocation: false, cooldownScope: 'USER', cooldownSeconds: 60 });
    const slash = () => {
      const interaction = makeInteraction({ sub: 'use', ...slashSc });
      return runTag(slashTag, {
        source: 'slash', interaction, guild: slashSc.guild, channel: slashSc.channel, member: slashSc.member, user: slashSc.user,
        args: [], settings: null, prefix: '!', checkPermissions: false,
      }).then((res) => ({ res, interaction }));
    };
    await slash();
    const again = await slash();
    assert.equal(again.res.status, 'cooldown');
    assert.equal(again.interaction.calls.reply[0].ephemeral, true);
    assert.match(embedData(again.interaction.calls.reply[0]).description, /<t:\d+:R>/);

    // Triggers: the matching trigger on cooldown ends the scan; the next trigger does not fire instead (R284).
    store.clear();
    const guild = makeGuild();
    const user = makeUser();
    const member = makeMember(guild, user);
    __testing.primeTriggers(GID, [
      proj({ id: 'cd', trigger: 'hello', response: 'first', cooldown: 30 }),
      proj({ id: 'next', trigger: 'hello', response: 'second' }),
    ]);
    const chan1 = makeChannel(guild, CH);
    const chan2 = makeChannel(guild, CH2);
    await handleTriggers(makeCtx({ guild, message: makeMessage({ content: 'hello', channel: chan1, user, member, guild }) }), { checkPermissions: false });
    assert.deepEqual(chan1.sent.map((p) => p.content), ['first']);
    assert.ok(store.has(`trigger:cd:${GID}:cd:${U}`));
    await handleTriggers(makeCtx({ guild, message: makeMessage({ content: 'hello', channel: chan2, user, member, guild }) }), { checkPermissions: false });
    assert.equal(chan2.sent.length, 0, 'the second trigger does not take over');
    // A failed send releases the trigger cooldown.
    store.clear();
    deleted.length = 0;
    const chan3 = makeChannel(guild, '888888888888888883', { sendImpl: async () => { throw err(50013); } });
    await handleTriggers(makeCtx({ guild, message: makeMessage({ content: 'hello', channel: chan3, user, member, guild }) }), { checkPermissions: false });
    assert.ok(deleted.includes(`trigger:cd:${GID}:cd:${U}`));

    // clearTagCooldowns removes the server key.
    store.set(`tag:cd:${GID}:t9`, Date.now() + 1000);
    await svc.clearTagCooldowns(GID, 't9');
    assert.equal(store.has(`tag:cd:${GID}:t9`), false);
  } finally {
    Object.assign(cache, real);
  }
});

// ── 15. Display helpers ───────────────────────────────────────────────────────

test('15. display helpers', () => {
  const guild = makeGuild({ roles: [makeRole(ROLE_A)] });
  makeChannel(guild, CH);
  assert.equal(codeSafe('a`b'), 'aˋb');
  assert.equal(oneLine('a \n\n b\t c '), 'a b c');
  assert.equal(describeAction({ type: 'reply', dm: false, embed: true, responses: ['a', 'b'] }), '💬 Reply in channel · embed · 2 replies');
  assert.equal(describeAction({ type: 'reply', dm: true, responses: ['a'] }), '📨 Reply by DM · 1 reply');
  assert.equal(describeAction({ type: 'send', channelId: CH, responses: ['a'], pingRoleIds: [ROLE_A] }, guild), `📤 Send to <#${CH}> · 1 reply · pings <@&${ROLE_A}>`);
  assert.ok(describeAction({ type: 'send', channelId: CH2, responses: ['a'] }, guild).includes('⚠️ deleted'));
  assert.equal(describeAction({ type: 'addRole', roleId: ROLE_A }, guild), `➕ Give <@&${ROLE_A}>`);
  assert.ok(describeAction({ type: 'removeRole', roleId: ROLE_B }, guild).includes('⚠️ deleted'));
  assert.equal(describeAction({ type: 'toggleRole', roleId: ROLE_A }), `🔁 Toggle <@&${ROLE_A}>`);
  assert.equal(actionLabel({ type: 'send', channelId: CH }, guild), `Send to #chan-333`);
  assert.equal(actionLabel({ type: 'addRole', roleId: ROLE_B }, guild), 'Give (deleted)');
  assert.equal(actionLabel({ type: 'reply', dm: true }, guild), 'Reply by DM');
});

// ── 16. Commands without a database: validation paths ─────────────────────────

describe('16. command validation (no database needed)', () => {
  const GCMD = 'gcmd';
  before(() => {
    // The router loaded the settings already in production; here the settings service reads a stub.
    prisma.guildSettings = {
      findUnique: async () => ({ guildId: GCMD, customCommandsEnabled: true, autoTriggersEnabled: true, customPrefix: '!', embedColor: null }),
      upsert: async ({ update, create }) => ({
        guildId: GCMD, customCommandsEnabled: true, autoTriggersEnabled: true, customPrefix: '!', embedColor: null, ...create, ...update,
      }),
    };
  });
  after(() => { prisma.guildSettings = realGuildSettings; });

  const setup = (over = {}) => {
    const guild = makeGuild({ id: GCMD, roles: [makeRole(ROLE_A), makeRole(ROLE_B, { mentionable: true }), makeRole('300000000000000003', { position: 200 })] });
    const channel = makeChannel(guild);
    const user = makeUser();
    const member = makeMember(guild, user);
    member.roles.highest = { position: 50 };
    member.permissions = { has: () => false };
    return { guild, channel, user, member, ...over };
  };
  const tag = async (sc, sub, options, group = null) => {
    const i = makeInteraction({ sub, group, options, ...sc });
    await tagCmd.execute(i);
    return i;
  };
  const trig = async (sc, sub, options) => {
    const i = makeInteraction({ sub, group: 'trigger', options, ...sc });
    await tagCmd.execute(i);
    return i;
  };

  test('/tag create: invalid name, empty and too-long text (R030, R099, R102)', async () => {
    const sc = setup();
    let i = await tag(sc, 'create', { name: 'c++', content: 'x' });
    assert.equal(i.calls.deferReply[0].ephemeral, true);
    assert.match(titleOf(i), /Invalid Name/);
    i = await tag(sc, 'create', { name: 'ok', content: '   ' });
    assert.match(titleOf(i), /Empty/);
    i = await tag(sc, 'create', { name: 'ok', content: 'x'.repeat(2001) });
    assert.match(titleOf(i), /Too Long/);
    assert.match(textOf(i), /2,000 characters \(this one is 2001\)/);
    assert.match(textOf(i), /embed:True/);
    i = await tag(sc, 'create', { name: 'ok', content: 'x'.repeat(4001), embed: true });
    assert.match(textOf(i), /4,000 characters \(this one is 4001\)/);
  });

  test('/tag edit, cooldown and permissions validate before touching the database', async () => {
    const sc = setup();
    let i = await tag(sc, 'edit', { name: 'x' });
    assert.match(titleOf(i), /Nothing to Change/);
    i = await tag(sc, 'edit', { name: 'x', content: '  ' });
    assert.match(titleOf(i), /Empty/);

    i = await tag(sc, 'cooldown', { name: 'x', scope: 'SERVER' });
    assert.match(titleOf(i), /Duration Needed/);
    i = await tag(sc, 'cooldown', { name: 'x', scope: 'USER', duration: 'soon' });
    assert.match(titleOf(i), /Invalid Duration/);
    i = await tag(sc, 'cooldown', { name: 'x', scope: 'USER', duration: '500ms' });
    assert.match(titleOf(i), /Invalid Duration/);
    i = await tag(sc, 'cooldown', { name: 'x', scope: 'USER', duration: '400d' });
    assert.match(titleOf(i), /Too Long/);

    i = await tag(sc, 'permissions', { name: 'x', rule: 'allow' });
    assert.match(titleOf(i), /Nothing Selected/);
    i = await tag(sc, 'permissions', { name: 'x', rule: 'deny', role: { id: GCMD } });
    assert.match(titleOf(i), /Not Allowed/);
  });

  test('/tag action add refuses options that do not belong to the action', async () => {
    const sc = setup();
    const add = (options) => tag(sc, 'add', { name: 'x', ...options }, 'action');
    assert.match(titleOf(await add({ type: 'reply' })), /Missing Text/);
    assert.match(titleOf(await add({ type: 'reply', text: 'hi', role: { id: ROLE_A } })), /Wrong Option/);
    assert.match(titleOf(await add({ type: 'reply', text: 'hi', channel: { id: CH } })), /Wrong Option/);
    const dm = await add({ type: 'dm', text: 'hi', ping_role: { id: ROLE_B } });
    assert.match(titleOf(dm), /Wrong Option/);
    assert.match(textOf(dm), /DMs never ping roles/);
    assert.match(titleOf(await add({ type: 'send', text: 'hi' })), /Missing Option/);
    assert.match(titleOf(await add({ type: 'send', text: 'hi', channel: sc.channel, role: { id: ROLE_A } })), /Wrong Option/);
    assert.match(titleOf(await add({ type: 'addRole' })), /Missing Role/);
    const wrong = await add({ type: 'addRole', role: { id: ROLE_A }, text: 'hi' });
    assert.match(titleOf(wrong), /Wrong Option/);
    assert.match(textOf(wrong), /not used by role actions/);
    assert.match(titleOf(await add({ type: 'addRole', role: { id: ROLE_A }, embed: true })), /Wrong Option/);
    assert.match(titleOf(await add({ type: 'reply', text: 'x'.repeat(2001) })), /Too Long/);
  });

  test('/tag action add: role actions and pings go through the permission model (R031, R049, R169)', async () => {
    const sc = setup();
    const add = (options) => tag(sc, 'add', { name: 'x', ...options }, 'action');
    const above = await add({ type: 'addRole', role: sc.guild.roles.cache.get('300000000000000003') });
    assert.match(titleOf(above), /Role Not Allowed/);
    assert.match(textOf(above), /above your highest role|above my highest role/);
    const everyone = await add({ type: 'reply', text: 'hi', ping_role: { id: GCMD } });
    assert.match(textOf(everyone), /@everyone can never be pinged/);
    sc.channel.permissionsFor = () => ({ has: () => false }); // the member lacks Mention Everyone here
    const locked = await add({ type: 'reply', text: 'hi', ping_role: sc.guild.roles.cache.get(ROLE_A) });
    assert.match(titleOf(locked), /Not Allowed/);
    assert.match(textOf(locked), /Mention @everyone, @here and All Roles/);
  });

  test('/tag trigger add refuses unsafe, invalid and empty input (R010, R048)', async () => {
    const sc = setup();
    let i = await trig(sc, 'add', { pattern: '(a+)+$', response: 'x', match_type: 'REGEX' });
    assert.equal(i.calls.deferReply.length, 1);
    assert.equal(i.calls.deferReply[0]?.ephemeral, undefined, 'trigger replies are public');
    assert.match(titleOf(i), /Invalid Pattern/);
    assert.match(textOf(i), /Nested repetition/);
    i = await trig(sc, 'add', { pattern: '(a)\\1', response: 'x', match_type: 'REGEX' });
    assert.match(textOf(i), /Backreferences/);
    i = await trig(sc, 'add', { pattern: 'hello', response: '  ' });
    assert.match(titleOf(i), /Empty/);
    i = await trig(sc, 'add', { pattern: '', response: 'x' });
    assert.match(titleOf(i), /Invalid Pattern/);
  });

  test('/tag trigger enable and disable write through updateSettings with the right fields', async () => {
    const sc = setup();
    const writes = [];
    const prev = prisma.guildSettings;
    prisma.guildSettings = {
      ...prev,
      upsert: async (args) => {
        writes.push(args);
        return prev.upsert(args);
      },
    };
    try {
      let i = await trig(sc, 'enable', {});
      assert.match(titleOf(i), /Triggers On/);
      assert.deepEqual(writes.at(-1).update, { customCommandsEnabled: true, autoTriggersEnabled: true });
      i = await trig(sc, 'disable', {});
      assert.match(titleOf(i), /Triggers Off/);
      assert.deepEqual(writes.at(-1).update, { autoTriggersEnabled: false }, 'only the trigger switch');
      assert.match(textOf(i), /\/tag and prefix commands keep working/);
    } finally {
      prisma.guildSettings = prev;
    }
  });

  test('/tag placeholders is an ephemeral reply with the argument tokens', async () => {
    const sc = setup();
    const i = await tag(sc, 'placeholders', {});
    assert.equal(i.calls.deferReply.length, 0);
    const p = i.calls.reply[0];
    assert.equal(p.ephemeral, true);
    assert.deepEqual(p.allowedMentions, { parse: [] });
    const names = embedData(p).fields.map((f) => f.name);
    assert.deepEqual(names, ['Arguments', 'Random']);
    assert.match(embedData(p).footer.text, /Run with/);
  });
});

// ── 17. Database cases ────────────────────────────────────────────────────────

const skipDb = !DB && 'TEST_DATABASE_URL not set';
const DBG = '95502400000000001'; // guild ids of this file start with 955024
const DBG2 = '95502400000000002';
const DBG3 = '95502400000000003';

async function cleanupGuild(...ids) {
  for (const id of ids) {
    await prisma.customTag.deleteMany({ where: { guildId: id } });
    await prisma.autoTrigger.deleteMany({ where: { guildId: id } });
    await prisma.guildSettings.deleteMany({ where: { guildId: id } });
    await settingsSvc.invalidateSettings(id);
  }
}

// ── 16b. The module toggle (the command is not gated, execute() checks it) ───────

describe('16b. /tag while the Custom Commands module is off', () => {
  const GOFF = 'goff';
  before(() => {
    prisma.guildSettings = {
      findUnique: async () => ({ guildId: GOFF, customCommandsEnabled: false, autoTriggersEnabled: true, customPrefix: '!', embedColor: null }),
      upsert: async ({ update, create }) => ({
        guildId: GOFF, customCommandsEnabled: false, autoTriggersEnabled: true, customPrefix: '!', embedColor: null, ...create, ...update,
      }),
    };
  });
  after(() => { prisma.guildSettings = realGuildSettings; });

  const setup = () => {
    const guild = makeGuild({ id: GOFF, roles: [makeRole(ROLE_A)] });
    const channel = makeChannel(guild);
    const user = makeUser();
    const member = makeMember(guild, user);
    member.roles.highest = { position: 50 };
    member.permissions = { has: () => false };
    return { guild, channel, user, member };
  };

  test('every other subcommand refuses with the router wording, before any defer', async () => {
    for (const [sub, group, options] of [
      ['use', null, { name: 'x' }], ['placeholders', null, {}], ['list', null, {}], ['create', null, { name: 'ok', content: 'x' }],
      ['add', 'action', { name: 'x', type: 'reply', text: 'x' }],
    ]) {
      const i = makeInteraction({ sub, group, options, ...setup() });
      await tagCmd.execute(i);
      assert.equal(i.calls.deferReply.length, 0, `${group ?? ''} ${sub}: no defer`);
      const p = i.calls.reply[0];
      assert.match(embedData(p).title, /Module Disabled/, `${group ?? ''} ${sub}`);
      assert.equal(embedData(p).description, 'The **Custom Commands** module is turned off on this server. An admin can enable it with `/module enable customcommands`.');
    }
  });

  test('the name autocomplete answers nothing, and the trigger group still works', async () => {
    const i = makeInteraction({ sub: 'use', focused: { name: 'name', value: '' }, ...setup() });
    await tagCmd.autocomplete(i);
    assert.deepEqual(i.calls.respond[0], []);

    const t = makeInteraction({ sub: 'enable', group: 'trigger', options: {}, ...setup() });
    await tagCmd.execute(t);
    assert.equal(t.calls.deferReply.length, 1);
    assert.match(titleOf(t), /Triggers On/);
  });
});

describe('17. database', { skip: skipDb }, () => {
  before(async () => { await cleanupGuild(DBG, DBG2, DBG3); });
  after(async () => { await cleanupGuild(DBG, DBG2, DBG3); });

  const dbScenario = (gid) => {
    const guild = makeGuild({ id: gid, roles: [makeRole(ROLE_A, { position: 5 }), makeRole(ROLE_B, { position: 5, mentionable: true })] });
    const channel = makeChannel(guild);
    const user = makeUser();
    const member = makeMember(guild, user);
    member.roles.highest = { position: 50 };
    member.permissions = { has: () => true };
    return { guild, channel, user, member };
  };
  const tagRun = async (sc, sub, options, group = null) => {
    const i = makeInteraction({ sub, group, options, ...sc });
    await tagCmd.execute(i);
    return i;
  };
  const trigRun = async (sc, sub, options) => {
    const i = makeInteraction({ sub, group: 'trigger', options, ...sc });
    await tagCmd.execute(i);
    return i;
  };

  test('every RAW_SQL statement executes against the migrated schema', async () => {
    assert.equal(svc.RAW_SQL.length, 4);
    for (const { name, build } of svc.RAW_SQL) {
      const sql = build();
      assert.doesNotMatch(sql.sql.replace(/now\(\) AT TIME ZONE 'UTC'/gi, ''), /\bnow\(\)|CURRENT_TIMESTAMP/i, name);
      try {
        await prisma.$transaction(async (tx) => {
          if (/^\s*UPDATE/i.test(sql.sql)) await tx.$executeRaw(sql); else await tx.$queryRaw(sql);
        });
      } catch (e) {
        assert.fail(`${name}: ${e.message}`);
      }
    }
  });

  test('createTagRow: advisory lock, cap and duplicate names', async () => {
    await settingsSvc.ensureGuildSettings(DBG);
    const row = await svc.createTagRow({ guildId: DBG, name: 'one', content: 'x', createdBy: U });
    assert.equal(row.name, 'one');
    assert.equal(row.actions, null);
    await assert.rejects(svc.createTagRow({ guildId: DBG, name: 'one', content: 'y', createdBy: U }), (e) => e.code === 'P2002');
    // Parallel creates all succeed and stay distinct.
    const made = await Promise.all(['a', 'b', 'c', 'd'].map((n) => svc.createTagRow({ guildId: DBG, name: n, content: n, createdBy: U })));
    assert.equal(new Set(made.map((m) => m.id)).size, 4);
    // The cap: fill up to the maximum, then one more is refused.
    const have = await prisma.customTag.count({ where: { guildId: DBG } });
    await prisma.customTag.createMany({
      data: Array.from({ length: LIMITS.CUSTOM_TAGS - have }, (_, k) => ({ guildId: DBG, name: `fill-${k}`, content: 'x', createdBy: U })),
    });
    await assert.rejects(
      svc.createTagRow({ guildId: DBG, name: 'over', content: 'x', createdBy: U }),
      (e) => e instanceof TagError && e.code === 'LIMIT' && /500 custom commands/.test(e.message),
    );
    await prisma.customTag.deleteMany({ where: { guildId: DBG } });
  });

  test('createTriggerRow: cap of LIMITS.AUTO_TRIGGERS', async () => {
    await settingsSvc.ensureGuildSettings(DBG);
    await prisma.autoTrigger.createMany({
      data: Array.from({ length: LIMITS.AUTO_TRIGGERS }, (_, k) => ({ guildId: DBG, trigger: `t${k}`, response: 'r', createdBy: U })),
    });
    await assert.rejects(
      svc.createTriggerRow({ guildId: DBG, trigger: 'over', response: 'r', createdBy: U }),
      (e) => e instanceof TagError && e.code === 'LIMIT' && /100 triggers/.test(e.message),
    );
    await prisma.autoTrigger.deleteMany({ where: { guildId: DBG } });
    const ok = await svc.createTriggerRow({ guildId: DBG, trigger: 'fine', response: 'r', createdBy: U, channelIds: [CH], exemptRoleIds: [ROLE_A] });
    assert.deepEqual(ok.channelIds, [CH]);
    await prisma.autoTrigger.deleteMany({ where: { guildId: DBG } });
  });

  test('writeTag: optimistic concurrency, retry once, then CONFLICT', async () => {
    await settingsSvc.ensureGuildSettings(DBG);
    const row = await svc.createTagRow({ guildId: DBG, name: 'occ', content: 'x', createdBy: U });
    const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
    // 1. A plain write bumps updatedAt (the token) and returns the new row.
    await sleep(5);
    const w1 = await svc.writeTag(DBG, 'occ', () => ({ description: 'first' }));
    assert.equal(w1.description, 'first');
    assert.ok(w1.updatedAt.getTime() > row.updatedAt.getTime(), 'updateMany bumps @updatedAt');

    // 2. Another writer sneaks in right before the first attempt's write: the retry applies on top of it.
    const real = prisma.customTag;
    const sneakBefore = (times) => {
      let n = 0;
      prisma.customTag = new Proxy(real, {
        get(target, prop) {
          if (prop !== 'updateMany') return target[prop];
          return async (args) => {
            if (n++ < times) {
              await sleep(5);
              await real.update({ where: { id: args.where.id }, data: { content: `sneaky${n}` } });
            }
            return real.updateMany(args);
          };
        },
      });
      return () => { prisma.customTag = real; };
    };
    let attempts = 0;
    let restore = sneakBefore(1);
    try {
      const w2 = await svc.writeTag(DBG, 'occ', (t) => {
        attempts++;
        return { description: `saw ${t.content}` };
      });
      assert.equal(attempts, 2, 'retried once');
      assert.equal(w2.description, 'saw sneaky1', 'the retry applied on top of the other writer');
    } finally {
      restore();
    }

    // 3. Two lost races give CONFLICT.
    attempts = 0;
    restore = sneakBefore(2);
    try {
      await assert.rejects(
        svc.writeTag(DBG, 'occ', () => { attempts++; return { description: 'never' }; }),
        (e) => e instanceof TagError && e.code === 'CONFLICT',
      );
      assert.equal(attempts, 2, 'two attempts');
    } finally {
      restore();
    }
    assert.notEqual((await prisma.customTag.findFirst({ where: { guildId: DBG, name: 'occ' } })).description, 'never');

    await assert.rejects(svc.writeTag(DBG, 'missing', () => ({ description: 'x' })), (e) => e.code === 'NOT_FOUND');
    assert.equal((await svc.writeTag(DBG, 'occ', () => null)).name, 'occ', 'nothing to write returns the row');
    await prisma.customTag.deleteMany({ where: { guildId: DBG } });
  });

  test('writeTrigger: conflict when the row changed meanwhile', async () => {
    await settingsSvc.ensureGuildSettings(DBG);
    const row = await svc.createTriggerRow({ guildId: DBG, trigger: 'conflict', response: 'r', createdBy: U });
    await new Promise((r) => setTimeout(r, 5));
    await prisma.autoTrigger.update({ where: { id: row.id }, data: { response: 'changed' } });
    await assert.rejects(svc.writeTrigger(DBG, row, { cooldown: 5 }), (e) => e.code === 'CONFLICT');
    const fresh = await prisma.autoTrigger.findUnique({ where: { id: row.id } });
    const updated = await svc.writeTrigger(DBG, fresh, { cooldown: 5 });
    assert.equal(updated.cooldown, 5);
    await prisma.autoTrigger.deleteMany({ where: { guildId: DBG } });
  });

  test('uses counters are clamped to int4 and leave updatedAt alone', async () => {
    await settingsSvc.ensureGuildSettings(DBG);
    __testing.configure({ countUses: true });
    const t = await svc.createTagRow({ guildId: DBG, name: 'counter', content: 'x', createdBy: U });
    const tr = await svc.createTriggerRow({ guildId: DBG, trigger: 'counter', response: 'r', createdBy: U });
    await svc.incrementTagUses(t.id);
    await svc.incrementTriggerUses(tr.id);
    assert.equal((await prisma.customTag.findUnique({ where: { id: t.id } })).uses, 1);
    assert.equal((await prisma.autoTrigger.findUnique({ where: { id: tr.id } })).uses, 1);
    await prisma.customTag.update({ where: { id: t.id }, data: { uses: 2147483647 } });
    await prisma.autoTrigger.update({ where: { id: tr.id }, data: { uses: 2147483647 } });
    const before = await prisma.customTag.findUnique({ where: { id: t.id } });
    await svc.incrementTagUses(t.id);
    await svc.incrementTriggerUses(tr.id);
    const after1 = await prisma.customTag.findUnique({ where: { id: t.id } });
    assert.equal(after1.uses, 2147483647, 'clamped, no overflow error');
    assert.equal((await prisma.autoTrigger.findUnique({ where: { id: tr.id } })).uses, 2147483647);
    assert.equal(after1.updatedAt.getTime(), before.updatedAt.getTime(), 'raw SQL keeps updatedAt for real edits');
    await prisma.customTag.deleteMany({ where: { guildId: DBG } });
    await prisma.autoTrigger.deleteMany({ where: { guildId: DBG } });
  });

  test('resolveTrigger and triggerNumber: #n, ID, pattern and ambiguity (R014)', async () => {
    await settingsSvc.ensureGuildSettings(DBG);
    const base = Date.now() - 100_000;
    const mk = (n, trigger, extra = {}) => prisma.autoTrigger.create({
      data: { guildId: DBG, trigger, response: 'r', createdBy: U, createdAt: new Date(base + n * 1000), ...extra },
    });
    const a = await mk(1, 'alpha');
    const b = await mk(2, '42');
    const c = await mk(3, 'Same');
    const d = await mk(4, 'same');
    const rows = [a, b, c, d];
    assert.equal((await svc.resolveTrigger(DBG, '#1')).id, a.id);
    assert.equal((await svc.resolveTrigger(DBG, '2')).id, b.id, 'a number wins over a numeric pattern');
    assert.equal((await svc.resolveTrigger(DBG, '#4')).id, d.id);
    assert.equal((await svc.resolveTrigger(DBG, c.id)).id, c.id, 'an ID');
    assert.equal((await svc.resolveTrigger(DBG, 'ALPHA')).id, a.id, 'an exact pattern, case-insensitively');
    await assert.rejects(svc.resolveTrigger(DBG, 'same'), (e) => e.code === 'AMBIGUOUS');
    await assert.rejects(svc.resolveTrigger(DBG, '#0'), (e) => e.code === 'NOT_FOUND', '#0 never reaches skip: -1');
    await assert.rejects(svc.resolveTrigger(DBG, '#99'), (e) => e.code === 'NOT_FOUND');
    await assert.rejects(svc.resolveTrigger(DBG, 'nothing here'), (e) => e.code === 'NOT_FOUND' && /Use \/tag trigger list/.test(e.message));
    await assert.rejects(svc.resolveTrigger(DBG2, a.id), (e) => e.code === 'NOT_FOUND', 'another guild');
    for (let k = 0; k < rows.length; k++) assert.equal(await svc.triggerNumber(DBG, rows[k]), k + 1);
    await prisma.autoTrigger.deleteMany({ where: { guildId: DBG } });
  });

  test('trigger autocomplete filters in the database before take(25)', async () => {
    await settingsSvc.ensureGuildSettings(DBG);
    const base = Date.now() - 1_000_000;
    await prisma.autoTrigger.createMany({
      data: Array.from({ length: 40 }, (_, k) => ({
        guildId: DBG, trigger: k === 3 ? 'Needle in a haystack' : `filler ${k}`, response: k === 5 ? 'find the HIDDEN answer' : 'r',
        createdBy: U, createdAt: new Date(base + k * 1000), enabled: k !== 7,
      })),
    });
    const sc = dbScenario(DBG);
    const ask = async (focused) => {
      const i = makeInteraction({ sub: 'info', group: 'trigger', focused, ...sc });
      await tagCmd.autocomplete(i);
      return i.calls.respond[0];
    };
    const all = await ask('');
    assert.equal(all.length, 25, 'capped at 25');
    const needle = await ask('needle');
    assert.equal(needle.length, 1, 'an old trigger is found');
    assert.match(needle[0].name, /Needle in a haystack/);
    const hidden = await ask('hidden');
    assert.equal(hidden.length, 1, 'the response text is searched too');
    const row = await prisma.autoTrigger.findFirst({ where: { guildId: DBG, trigger: 'filler 7' } });
    const paused = await ask(row.id.slice(0, 24)); // cuids share long prefixes; 24 of 25 characters is unique
    assert.equal(paused.length, 1, 'an ID prefix');
    assert.match(paused[0].name, /^⏸️/);
    assert.equal(paused[0].value, row.id);
    await prisma.autoTrigger.deleteMany({ where: { guildId: DBG } });
  });

  test('getTriggerSet loads enabled triggers in creation order and caches projections', async () => {
    await settingsSvc.ensureGuildSettings(DBG);
    const base = Date.now() - 50_000;
    await prisma.autoTrigger.create({ data: { guildId: DBG, trigger: 'second', response: 'r', createdBy: U, createdAt: new Date(base + 2000) } });
    await prisma.autoTrigger.create({ data: { guildId: DBG, trigger: 'first', response: 'r', createdBy: U, createdAt: new Date(base + 1000) } });
    await prisma.autoTrigger.create({ data: { guildId: DBG, trigger: 'paused', response: 'r', createdBy: U, enabled: false } });
    await prisma.autoTrigger.create({ data: { guildId: DBG, trigger: '(a+)+$', matchType: 'REGEX', response: 'r', createdBy: U, createdAt: new Date(base + 3000) } });
    const set = await getTriggerSet(DBG);
    assert.deepEqual(set.entries.map((e) => e.t.trigger), ['first', 'second', '(a+)+$']);
    assert.equal(typeof set.entries[0].t.v, 'string');
    assert.equal(set.entries[2].re, null, 'a legacy unsafe regex is skipped');
    assert.equal((await getTriggerSet(DBG)), set, 'memo hit');
    await svc.invalidateTriggerCache(DBG);
    assert.notEqual(await getTriggerSet(DBG), set, 'invalidated');
    await prisma.autoTrigger.deleteMany({ where: { guildId: DBG } });
  });

  test('findTag normalises lookups and hasTagName reads the enabled names', async () => {
    await settingsSvc.ensureGuildSettings(DBG);
    await svc.createTagRow({ guildId: DBG, name: 'hello-world', content: 'x', createdBy: U });
    await svc.createTagRow({ guildId: DBG, name: 'off', content: 'x', createdBy: U });
    await prisma.customTag.updateMany({ where: { guildId: DBG, name: 'off' }, data: { enabled: false } });
    assert.equal((await svc.findTag(DBG, 'Hello World')).name, 'hello-world');
    assert.equal(await svc.findTag(DBG, '!!!'), null);
    assert.equal(await svc.findTag(DBG, 'nope'), null);
    assert.equal(await svc.hasTagName(DBG, 'hello-world'), true);
    assert.equal(await svc.hasTagName(DBG, 'off'), false, 'disabled commands are invisible to prefix use');
    await prisma.customTag.deleteMany({ where: { guildId: DBG } });
  });

  test('/tag: create, actions, random replies, permissions, cooldown, info, raw, list and delete', async () => {
    const sc = dbScenario(DBG2);
    await settingsSvc.ensureGuildSettings(DBG2);
    await settingsSvc.updateSettings(DBG2, { customCommandsEnabled: true });
    const run = (sub, options, group = null) => tagRun(sc, sub, options, group);

    let i = await run('create', { name: 'Welcome', content: 'Hi {user}, {1:friend}!', description: 'Greets people' });
    assert.match(titleOf(i), /Custom Command Created/);
    assert.match(textOf(i), /`!welcome`/);
    assert.match(textOf(i), /Usage: `!welcome \[more…\]`/);
    i = await run('create', { name: 'welcome', content: 'again' });
    assert.match(titleOf(i), /Already Exists/);

    // A legacy tag is materialised by the first action write.
    i = await run('add', { name: 'welcome', type: 'dm', text: 'Welcome to {server}!' }, 'action');
    assert.match(titleOf(i), /Action Added/);
    assert.match(textOf(i), /Action 2: 📨 Reply by DM/);
    let row = await prisma.customTag.findFirst({ where: { guildId: DBG2, name: 'welcome' } });
    assert.equal(row.actions.length, 2);
    assert.equal(row.content, 'Hi {user}, {1:friend}!', 'the mirror columns follow the primary message action');

    i = await run('add', { name: 'welcome', type: 'addRole', role: sc.guild.roles.cache.get(ROLE_A) }, 'action');
    assert.match(titleOf(i), /Action Added/);
    i = await run('add', { name: 'welcome', type: 'reply', text: 'ping', ping_role: sc.guild.roles.cache.get(ROLE_B), embed: true }, 'action');
    assert.match(titleOf(i), /Action Added/);
    row = await prisma.customTag.findFirst({ where: { guildId: DBG2, name: 'welcome' } });
    assert.equal(row.actions.length, 4);
    assert.deepEqual(row.actions[3].pingRoleIds, [ROLE_B]);
    i = await run('add', { name: 'welcome', type: 'removeRole', role: sc.guild.roles.cache.get(ROLE_A) }, 'action');
    assert.match(titleOf(i), /Action Added/);
    i = await run('add', { name: 'welcome', type: 'toggleRole', role: sc.guild.roles.cache.get(ROLE_A) }, 'action');
    assert.match(titleOf(i), /Limit Reached/);
    assert.match(textOf(i), /at most 5 actions/);

    i = await run('add', { name: 'welcome', text: 'Howdy {user}', action: 1 }, 'response');
    assert.match(titleOf(i), /Reply Added/);
    assert.match(textOf(i), /Action 1 now has 2 replies/);
    i = await run('add', { name: 'welcome', text: 'nope', action: 3 }, 'response');
    assert.match(titleOf(i), /No Message Action/, 'a role action has no replies');
    i = await run('remove', { name: 'welcome', action: 1, reply: 2 }, 'response');
    assert.match(titleOf(i), /Reply Removed/);
    i = await run('remove', { name: 'welcome', action: 1, reply: 1 }, 'response');
    assert.match(titleOf(i), /Last Reply/);
    i = await run('remove', { name: 'welcome', action: 1, reply: 9 }, 'response');
    assert.match(titleOf(i), /Not Found/);

    i = await run('list', { name: 'welcome' }, 'action');
    assert.match(titleOf(i), /Actions of welcome/);
    assert.match(textOf(i), /1\./);
    assert.match(textOf(i), /↳ `1`/);

    // edit: the primary message action is edited in place and the mirror columns follow; limits follow the embed flag (R030).
    i = await run('edit', { name: 'welcome', content: 'x'.repeat(2500) });
    assert.match(titleOf(i), /Too Long/, 'plain text is limited to 2000');
    i = await run('edit', { name: 'welcome', content: 'x'.repeat(2500), embed: true });
    assert.match(titleOf(i), /Custom Command Updated/, 'embed text may be up to 4000');
    row = await prisma.customTag.findFirst({ where: { guildId: DBG2, name: 'welcome' } });
    assert.equal(row.actions[0].responses[0].length, 2500);
    assert.equal(row.actions[0].embed, true);
    assert.equal(row.useEmbed, true, 'mirror column');
    i = await run('edit', { name: 'welcome', embed: false });
    assert.match(titleOf(i), /Too Long/, 'switching an over-long reply to plain text is refused');
    i = await run('edit', { name: 'welcome', content: 'Short {1}', embed: false, description: '-', delete_usage: true });
    assert.match(titleOf(i), /Custom Command Updated/);
    row = await prisma.customTag.findFirst({ where: { guildId: DBG2, name: 'welcome' } });
    assert.equal(row.description, null);
    assert.equal(row.deleteInvocation, true);
    assert.equal(row.content, 'Short {1}');
    assert.equal(row.useEmbed, false);

    i = await run('permissions', { name: 'welcome', rule: 'allow', role: sc.guild.roles.cache.get(ROLE_A), channel: sc.channel });
    assert.match(titleOf(i), /Permissions Updated/);
    assert.equal(fieldsOf(i).find((f) => f.name === 'Allowed roles').value, `<@&${ROLE_A}>`);
    i = await run('permissions', { name: 'welcome', rule: 'deny', role: sc.guild.roles.cache.get(ROLE_A) });
    row = await prisma.customTag.findFirst({ where: { guildId: DBG2, name: 'welcome' } });
    assert.deepEqual([row.allowRoleIds, row.denyRoleIds], [[], [ROLE_A]], 'deny moves the role out of the allow list');
    i = await run('permissions', { name: 'welcome', rule: 'remove', role: sc.guild.roles.cache.get(ROLE_A) });
    i = await run('permissions', { name: 'welcome', rule: 'clear' });
    row = await prisma.customTag.findFirst({ where: { guildId: DBG2, name: 'welcome' } });
    assert.deepEqual([row.allowChannelIds, row.denyRoleIds], [[], []]);

    i = await run('cooldown', { name: 'welcome', scope: 'USER', duration: '90s' });
    assert.match(textOf(i), /Per user · 1 minute 30 seconds/);
    row = await prisma.customTag.findFirst({ where: { guildId: DBG2, name: 'welcome' } });
    assert.deepEqual([row.cooldownScope, row.cooldownSeconds], ['USER', 90]);
    i = await run('cooldown', { name: 'welcome', scope: 'NONE', duration: '1h' });
    assert.match(textOf(i), /None.*duration ignored/);

    i = await run('info', { name: 'welcome' });
    assert.match(titleOf(i), /!welcome/);
    assert.ok(fieldsOf(i).some((f) => f.name === 'Usage'));
    assert.match(embedData(lastPayload(i)).footer.text, /^ID: /);

    i = await run('raw', { name: 'welcome' });
    assert.match(lastPayload(i).content, /Raw text of `welcome`/);
    assert.ok(lastPayload(i).files.some((f) => f.name === 'welcome-actions.json'), 'actions are attached as JSON');

    i = await run('list', {});
    assert.match(titleOf(i), /Custom Commands \(1\)/);
    assert.match(textOf(i), /`!welcome`/);
    assert.match(textOf(i), /⚙️×5/);

    i = await run('toggle', { name: 'welcome' });
    assert.match(titleOf(i), /Disabled/);
    i = await run('use', { name: 'welcome' });
    assert.match(titleOf(i), /Disabled/);
    assert.equal(i.calls.reply[0].ephemeral, true);
    i = await run('toggle', { name: 'welcome' });
    assert.match(titleOf(i), /Enabled/);

    i = await run('remove', { name: 'welcome', action: 5 }, 'action');
    assert.match(titleOf(i), /Action Removed/);
    i = await run('remove', { name: 'welcome', action: 9 }, 'action');
    assert.match(titleOf(i), /Not Found/);

    i = await run('delete', { name: 'welcome' });
    assert.match(titleOf(i), /Custom Command Deleted/);
    i = await run('delete', { name: 'welcome' });
    assert.match(titleOf(i), /Not Found/);
    i = await run('use', { name: 'welcome' });
    assert.match(titleOf(i), /Not Found/);
    i = await run('list', {});
    assert.match(titleOf(i), /No Custom Commands/);
  });

  test('/tag use end to end: arguments, uses and the DM path against the database', async () => {
    const sc = dbScenario(DBG2);
    await settingsSvc.ensureGuildSettings(DBG2);
    await settingsSvc.updateSettings(DBG2, { customCommandsEnabled: true });
    __testing.configure({ countUses: true });
    await tagRun(sc, 'create', { name: 'greet', content: 'Hello {1} and {2:you}' });
    let i = await tagRun(sc, 'use', { name: 'greet' });
    assert.equal(i.calls.reply[0].ephemeral, true, 'missing arguments: the usage reply');
    assert.match(embedData(i.calls.reply[0]).description, /Usage: `\/tag use name:greet args:<1> \[more…\]`/);
    i = await tagRun(sc, 'use', { name: 'Greet', args: 'Bob' });
    assert.equal(i.calls.reply[0].content, 'Hello Bob and you');
    assert.deepEqual(i.calls.reply[0].allowedMentions, { parse: ['users'] });
    await new Promise((r) => setTimeout(r, 100));
    assert.equal((await prisma.customTag.findFirst({ where: { guildId: DBG2, name: 'greet' } })).uses, 1, 'counted after success only');
    await prisma.customTag.deleteMany({ where: { guildId: DBG2 } });
  });

  test('/tag autocomplete: module gate, normalised query and member view', async () => {
    const sc = dbScenario(DBG3);
    await settingsSvc.ensureGuildSettings(DBG3);
    await settingsSvc.updateSettings(DBG3, { customCommandsEnabled: false });
    await svc.createTagRow({ guildId: DBG3, name: 'rules', content: 'x', description: 'The rules', createdBy: U });
    await svc.createTagRow({ guildId: DBG3, name: 'secret', content: 'x', createdBy: U });
    await prisma.customTag.updateMany({ where: { guildId: DBG3, name: 'secret' }, data: { enabled: false } });
    const ask = async (sub, value, group = null, member = sc.member) => {
      const i = makeInteraction({ sub, group, focused: { name: 'name', value }, ...sc, member });
      await tagCmd.autocomplete(i);
      return i.calls.respond[0];
    };
    assert.deepEqual(await ask('use', ''), [], 'module off: nothing');
    await settingsSvc.updateSettings(DBG3, { customCommandsEnabled: true });
    assert.deepEqual((await ask('use', 'RUL')).map((c) => c.value), ['rules'], 'the query is normalised');
    assert.equal((await ask('use', 'Rules')).length, 1);
    assert.equal((await ask('use', '')).length, 1, 'disabled commands are not offered for /tag use');
    assert.equal((await ask('edit', '')).length, 2, 'admins see disabled commands when editing');
    assert.match((await ask('edit', 'sec'))[0].name, /^⏸️ secret/);
    const plain = { ...sc.member, permissions: { has: () => false } };
    assert.equal((await ask('info', '', null, plain)).length, 1, 'members never see disabled commands');
    await svc.createTagRow({ guildId: DBG3, name: 'staff', content: 'x', createdBy: U, allowRoleIds: [ROLE_A] });
    assert.deepEqual((await ask('raw', '', null, plain)).map((c) => c.value), ['rules'], 'members never see commands their roles cannot run');
    const staff = { ...plain, roles: { cache: new Map([[ROLE_A, { id: ROLE_A }]]) } };
    assert.equal((await ask('raw', '', null, staff)).length, 2, 'a member with the allowed role sees it');
    assert.equal((await ask('edit', '')).length, 3, 'admins see every command');
    await prisma.customTag.deleteMany({ where: { guildId: DBG3 } });
  });

  test('/tag raw, info, list and action list hide role-restricted commands from members who lack the role', async () => {
    const sc = dbScenario(DBG3);
    await settingsSvc.ensureGuildSettings(DBG3);
    await settingsSvc.updateSettings(DBG3, { customCommandsEnabled: true });
    const SECRET = 'staff-only handbook link';
    await svc.createTagRow({ guildId: DBG3, name: 'handbook', content: SECRET, createdBy: U, allowRoleIds: [ROLE_A] });
    await svc.createTagRow({ guildId: DBG3, name: 'nodogs', content: `${SECRET} (deny)`, createdBy: U, denyRoleIds: [ROLE_B] });
    const plain = { ...sc.member, permissions: { has: () => false }, roles: { cache: new Map([[ROLE_B, { id: ROLE_B }]]) } };
    const asMember = { ...sc, member: plain };
    for (const [sub, name, group] of [['raw', 'handbook'], ['info', 'handbook'], ['list'], ['list', 'handbook', 'action'], ['raw', 'nodogs']]) {
      const i = await tagRun(asMember, sub, name ? { name } : {}, group ?? null);
      assert.ok(!JSON.stringify(i.calls).includes(SECRET), `${group ? `${group} ` : ''}${sub} ${name ?? ''} leaks nothing`);
    }
    const staff = { ...sc, member: { ...plain, roles: { cache: new Map([[ROLE_A, { id: ROLE_A }]]) } } };
    assert.ok(JSON.stringify((await tagRun(staff, 'raw', { name: 'handbook' })).calls).includes(SECRET), 'the allowed role can read it');
    assert.ok(JSON.stringify((await tagRun(sc, 'raw', { name: 'nodogs' })).calls).includes(SECRET), 'admins can read it');
    await prisma.customTag.deleteMany({ where: { guildId: DBG3 } });
  });

  test('/tag autocomplete of action and reply numbers', async () => {
    const sc = dbScenario(DBG3);
    await settingsSvc.ensureGuildSettings(DBG3);
    await settingsSvc.updateSettings(DBG3, { customCommandsEnabled: true });
    await tagRun(sc, 'create', { name: 'ac', content: 'first {user}' });
    await tagRun(sc, 'add', { name: 'ac', type: 'addRole', role: sc.guild.roles.cache.get(ROLE_A) }, 'action');
    await tagRun(sc, 'add', { name: 'ac', text: 'second reply', action: 1 }, 'response');
    const ask = async (sub, group, focused, options) => {
      const i = makeInteraction({ sub, group, focused, options, ...sc });
      await tagCmd.autocomplete(i);
      return i.calls.respond[0];
    };
    const actions = await ask('remove', 'action', { name: 'action', value: '' }, { name: 'ac' });
    assert.deepEqual(actions.map((c) => c.value), [1, 2]);
    assert.match(actions[0].name, /^1\. Reply in channel/);
    assert.match(actions[1].name, /^2\. Give @role-222/);
    const messagesOnly = await ask('add', 'response', { name: 'action', value: '' }, { name: 'ac' });
    assert.deepEqual(messagesOnly.map((c) => c.value), [1], 'only message actions can get random replies');
    const replies = await ask('remove', 'response', { name: 'reply', value: '' }, { name: 'ac', action: 1 });
    assert.deepEqual(replies.map((c) => c.value), [1, 2]);
    assert.match(replies[1].name, /^2\. second reply/);
    assert.deepEqual(await ask('remove', 'action', { name: 'action', value: '' }, { name: 'unknown' }), []);
    assert.deepEqual(await ask('remove', 'response', { name: 'reply', value: '' }, { name: 'ac', action: 2 }), [], 'a role action has no replies');
    await prisma.customTag.deleteMany({ where: { guildId: DBG3 } });
  });

  test('/tag trigger end to end: add, edit scope, toggle, list, info and remove', async () => {
    const sc = dbScenario(DBG3);
    await settingsSvc.ensureGuildSettings(DBG3);
    await settingsSvc.updateSettings(DBG3, { customCommandsEnabled: false, autoTriggersEnabled: true });
    const run = (sub, options) => trigRun(sc, sub, options);

    let i = await run('add', { pattern: 'hello', response: 'Hi {user} {nope}', match_type: 'CONTAINS', cooldown: 30, delete_message: true });
    assert.match(titleOf(i), /Trigger Added/);
    assert.match(textOf(i), /\*\*#1\*\* · ID `\w+`/);
    assert.match(textOf(i), /Unknown variables/);
    assert.match(textOf(i), /module is off/, 'module-off warning (R223)');
    assert.match(textOf(i), /\*\*Cooldown:\*\* 30s per user/);

    await settingsSvc.updateSettings(DBG3, { customCommandsEnabled: true });
    i = await run('add', { pattern: '^bye\\b', response: 'Bye', match_type: 'REGEX', channel: sc.channel });
    assert.match(titleOf(i), /Trigger Added/);
    assert.doesNotMatch(textOf(i), /module is off/);
    assert.match(textOf(i), new RegExp(`<#${CH}>`));

    await settingsSvc.updateSettings(DBG3, { autoTriggersEnabled: false });
    i = await run('add', { pattern: 'third', response: 'x' });
    assert.match(textOf(i), /Triggers are switched off/);
    await settingsSvc.updateSettings(DBG3, { autoTriggersEnabled: true });

    i = await run('list', {});
    assert.match(titleOf(i), /Triggers \(3\)/);
    assert.match(textOf(i), /`#1` ✅ \*\*\[Contains\]\*\* `hello`/);
    assert.match(textOf(i), /`#2`.*\[Regex\]/);
    assert.match(textOf(i), /ID `\w+`/);

    // edit: validation, the channel toggle, exempt roles, reset_scope.
    i = await run('edit', { trigger: '#2', pattern: '(a+)+$' });
    assert.match(titleOf(i), /Invalid Pattern/);
    i = await run('edit', { trigger: '#2' });
    assert.match(titleOf(i), /Nothing to Change/);
    i = await run('edit', { trigger: '#2', reset_scope: false });
    assert.match(titleOf(i), /Nothing to Change/);
    i = await run('edit', { trigger: '#2', channel: sc.channel });
    assert.match(titleOf(i), /Trigger Updated/);
    let row = await trigRunRow(DBG3, 2);
    assert.deepEqual(row.channelIds, [], 'the same channel is toggled off');
    i = await run('edit', { trigger: '#2', channel: sc.channel, exempt_role: sc.guild.roles.cache.get(ROLE_A), cooldown: 5 });
    row = await trigRunRow(DBG3, 2);
    assert.deepEqual([row.channelIds, row.exemptRoleIds, row.cooldown], [[CH], [ROLE_A], 5]);
    assert.match(textOf(i), /\*\*Exempt roles:\*\* <@&2/);
    i = await run('edit', { trigger: '#2', exempt_role: { id: DBG3 } });
    assert.match(titleOf(i), /Not Allowed/);
    i = await run('edit', { trigger: '#2', reset_scope: true });
    row = await trigRunRow(DBG3, 2);
    assert.deepEqual([row.channelIds, row.exemptRoleIds], [[], []]);
    i = await run('edit', { trigger: '#1', response: 'New {user}', case_sensitive: true, match_type: 'STARTS_WITH' });
    row = await trigRunRow(DBG3, 1);
    assert.deepEqual([row.response, row.caseSensitive, row.matchType], ['New {user}', true, 'STARTS_WITH']);

    // toggle and info.
    i = await run('toggle', { trigger: '#1' });
    assert.match(titleOf(i), /Paused/);
    i = await run('info', { trigger: '#1' });
    assert.match(titleOf(i), /Trigger #1/);
    assert.ok(fieldsOf(i).some((f) => f.name === 'Status' && f.value.includes('Paused')));
    assert.match(embedData(lastPayload(i)).footer.text, /Created by ann#0$/, 'the footer shows a tag, not a mention (R224)');
    assert.ok(fieldsOf(i).some((f) => f.name === 'Created by' && f.value === `<@${U}>`));
    i = await run('toggle', { trigger: '#1' });
    assert.match(titleOf(i), /Resumed/);

    // A legacy unsafe regex is flagged and never fires.
    const bad = await prisma.autoTrigger.create({ data: { guildId: DBG3, trigger: '(a+)+$', matchType: 'REGEX', response: 'r', createdBy: U } });
    i = await run('info', { trigger: bad.id });
    assert.ok(fieldsOf(i).some((f) => f.name === '⚠️ Never fires'));
    i = await run('list', {});
    assert.match(textOf(i), /never fires/);

    i = await run('remove', { trigger: bad.id });
    assert.match(titleOf(i), /Trigger Removed/);
    i = await run('remove', { trigger: bad.id });
    assert.match(titleOf(i), /Not Found/);
    await prisma.autoTrigger.deleteMany({ where: { guildId: DBG3 } });
    i = await run('list', {});
    assert.match(titleOf(i), /No Triggers/);
  });

  async function trigRunRow(guildId, n) {
    return (await prisma.autoTrigger.findMany({ where: { guildId }, orderBy: [{ createdAt: 'asc' }, { id: 'asc' }], skip: n - 1, take: 1 }))[0];
  }

  test('the trigger hook end to end: cache, cooldown fail-closed and the settings switch', async () => {
    const gid = DBG3;
    await settingsSvc.ensureGuildSettings(gid);
    await prisma.autoTrigger.deleteMany({ where: { guildId: gid } });
    await svc.createTriggerRow({ guildId: gid, trigger: 'ping', response: 'pong {user}', createdBy: U });
    await svc.invalidateTriggerCache(gid);
    const guild = makeGuild({ id: gid });
    const channel = makeChannel(guild);
    const user = makeUser();
    const member = makeMember(guild, user);
    const message = makeMessage({ content: 'a ping please', channel, user, member, guild });
    await handleTriggers(makeCtx({ guild, message }), { checkPermissions: false });
    assert.equal(channel.sent.length, 1);
    assert.equal(channel.sent[0].content, `pong <@${U}>`);
    await prisma.autoTrigger.deleteMany({ where: { guildId: gid } });
  });
});
