// Regression tests for the automations / custom commands review (offline). Each case pins a bug that was fixed:
//   1. /automation import must not hand out a role above the importing admin's own highest role (action add refuses it);
//   2. a per-user automation cooldown is given back when the run it was taken for never started (run budget, full queue);
//   3. /tag info, raw and action list answer "not found" for a disabled command when the caller is not an admin.
//   4. a message in a channel @everyone cannot see is never copied (REPOST, {message.content}) into one it can;
//   5. the admin who configures an automation, tag or embed needs the channel access and role-ping rights it uses;
//   6. a lower admin cannot change an automation or tag that gives or takes a role at or above their own (commands and dashboard).
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL ??= 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const { ChannelType, Collection, PermissionFlagsBits } = await import('discord.js');
const svc = await import('../src/services/automationService.js');
const tagCmd = await import('../src/commands/utility/tag.js');
const { cache, default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const realGuildSettings = prisma.guildSettings;
const realCustomTag = prisma.customTag;
const realExecuteRaw = prisma.$executeRaw;
// The run counter bump is fire-and-forget SQL; nothing listens for it here.
prisma.$executeRaw = async () => 0;
after(async () => {
  prisma.$executeRaw = realExecuteRaw;
  prisma.guildSettings = realGuildSettings;
  prisma.customTag = realCustomTag;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const BOT = '900000000000000099';
const ID = (n) => String(100000000000000000n + BigInt(n));
const flush = () => new Promise((r) => setImmediate(r));
const perms = () => ({ has: () => true });

// ── 1. Import and the role hierarchy ─────────────────────────────────────────

function hierarchyGuild() {
  const roles = new Collection([
    [ID(1), { id: ID(1), position: 2 }],
    [ID(2), { id: ID(2), position: 5 }],
    [ID(3), { id: ID(3), position: 8 }],
  ]);
  return { id: ID(900), ownerId: ID(901), roles: { cache: roles } };
}
const admin = (position, id = ID(902)) => ({ id, roles: { highest: { position } } });

test('import: a role at or above the importer\'s highest role is refused, below is fine', () => {
  const guild = hierarchyGuild();
  const give = (...roleIds) => [{ type: 'SEND_MESSAGE', content: 'x' }, { type: 'GIVE_ROLES', roleIds }];
  assert.equal(svc.importRoleHierarchyProblem(guild, admin(5), give(ID(1))), null, 'below the importer');
  assert.match(svc.importRoleHierarchyProblem(guild, admin(5), give(ID(3))) ?? '', /^Action 2: That role is above your highest role\.$/);
  assert.match(svc.importRoleHierarchyProblem(guild, admin(5), give(ID(1), ID(2))) ?? '', /above your highest role/, 'equal position counts as above');
  assert.match(svc.importRoleHierarchyProblem(guild, admin(5), [{ type: 'TAKE_ROLES', roleIds: [ID(3)] }]) ?? '', /Action 1/, 'take roles too');
});

test('import: the owner is exempt, unknown roles are left to the reference check, bad input fails closed', () => {
  const guild = hierarchyGuild();
  const high = [{ type: 'GIVE_ROLES', roleIds: [ID(3)] }];
  assert.equal(svc.importRoleHierarchyProblem(guild, admin(1, guild.ownerId), high), null, 'owner');
  assert.equal(svc.importRoleHierarchyProblem(guild, admin(1), [{ type: 'GIVE_ROLES', roleIds: [ID(77)] }]), null, 'deleted role');
  assert.equal(svc.importRoleHierarchyProblem(guild, admin(1), [{ type: 'PIN' }, { type: 'REACT', emoji: '⭐' }]), null, 'no role actions');
  assert.equal(svc.importRoleHierarchyProblem(guild, admin(1), []), null);
  assert.equal(svc.importRoleHierarchyProblem(guild, null, high), null, 'no member: nothing to compare with');
  assert.match(svc.importRoleHierarchyProblem(guild, { id: ID(903), roles: {} }, high) ?? '', /could not be checked/);
  assert.match(svc.importRoleHierarchyProblem(guild, { id: ID(903), roles: { highest: { position: undefined } } }, high) ?? '', /could not be checked/);
});

test('import: /automation import runs the hierarchy check before it saves anything', () => {
  const src = readFileSync(resolve(ROOT, 'src/commands/admin/automation.js'), 'utf8');
  const body = src.slice(src.indexOf('async function importJson'));
  const check = body.indexOf('importRoleHierarchyProblem');
  assert.ok(check > 0, 'the import calls the check');
  assert.ok(check < body.indexOf('auto.replaceAutomation('), 'before the overwrite');
  assert.ok(check < body.indexOf('auto.createAutomation('), 'before the create');
  assert.match(body.slice(check, check + 300), /interaction\.member/);
});

// ── 2. Cooldowns are given back for runs that did not start ──────────────────

function fakeChannel(guild, id) {
  const sent = [];
  const ch = {
    id, type: ChannelType.GuildText, name: 'general', parentId: null, guild, sent,
    isTextBased: () => true, isThread: () => false, permissionsFor: () => perms(),
    send: async (payload) => { sent.push(payload); return { id: `sent${sent.length}` }; },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}
function fakeMember(guild, id) {
  const roles = new Collection();
  const member = {
    id, guild, displayName: `m${id.slice(-3)}`,
    user: { id, bot: false, username: `u${id.slice(-3)}`, displayAvatarURL: () => 'https://example.test/a.png' },
    roles: { cache: roles, async add() {}, async remove() {} },
  };
  guild.members.cache.set(id, member);
  return member;
}
function fakeGuild(id) {
  const guild = {
    id, available: true, ownerId: '1', name: 'G', memberCount: 3,
    channels: { cache: new Collection() },
    roles: { cache: new Collection() },
    members: { cache: new Collection(), me: { id: BOT, permissions: { has: () => true }, roles: { highest: { position: 50 } } } },
    emojis: { cache: new Collection() },
    client: { user: { id: BOT } },
  };
  return guild;
}
let seq = 0;
function fakeMessage(guild, channel, author) {
  const id = String(700000000000000000n + BigInt(++seq));
  return {
    id, content: 'hello', type: 0, author, channel, guild, system: false, partial: false, pinned: false, member: guild.members.cache.get(author.id) ?? null,
    url: `https://discord.com/channels/${guild.id}/${channel.id}/${id}`, createdTimestamp: 1_700_000_000_000,
    attachments: new Collection(), reactions: { cache: new Collection() },
  };
}
const lite = (id, actions, cooldownSeconds = 0) => ({
  id, name: id, trigger: 'MESSAGE_SENT', cooldownSeconds, conditions: [], actions: svc.normalizeList(actions, 'action').items,
});
const baseOf = (guild, extra) => ({
  guild, guildId: guild.id, client: { user: { id: BOT } }, depth: 0, getSettings: async () => null, ...extra,
});

/** Redis "works" for the cooldown: setNX always wins and every del is recorded. */
function stubCooldownCache() {
  const real = { setNX: cache.setNX, del: cache.del };
  const dels = [];
  cache.setNX = async () => true;
  cache.del = async (...keys) => { dels.push(...keys); };
  return { dels, restore() { cache.setNX = real.setNX; cache.del = real.del; } };
}

test('dispatch: a refused run (budget exhausted) gives the cooldown back', async () => {
  const stub = stubCooldownCache();
  try {
    const guild = fakeGuild('955028900000000001');
    const general = fakeChannel(guild, ID(40));
    const member = fakeMember(guild, ID(42));
    const send = [{ type: 'SEND_MESSAGE', content: 'x', channelId: general.id }];
    svc.__setAutomationMemo(guild.id, { MESSAGE_SENT: [lite('autofree01', send)] });
    const fire = () => svc.dispatch('MESSAGE_SENT', baseOf(guild, {
      trigger: 'MESSAGE_SENT', member, user: member.user, channel: general, message: fakeMessage(guild, general, member.user),
    }), { awaitRuns: true });
    for (let i = 0; i < svc.RUN_BUDGET_MAX; i++) await fire();
    assert.equal(general.sent.length, svc.RUN_BUDGET_MAX);

    svc.__setAutomationMemo(guild.id, { MESSAGE_SENT: [lite('autocool02', send, 3600)] });
    await fire();
    assert.equal(general.sent.length, svc.RUN_BUDGET_MAX, 'the budget refused the run');
    assert.deepEqual(stub.dels, [`automation:cd:${guild.id}:autocool02:${member.id}`], 'and the hour-long cooldown was not kept');
  } finally {
    stub.restore();
  }
});

test('dispatch: a run dropped by the full queue gives the cooldown back, queued runs keep theirs', async () => {
  const stub = stubCooldownCache();
  try {
    const guild = fakeGuild('955028900000000002');
    const general = fakeChannel(guild, ID(44));
    const members = Array.from({ length: svc.MAX_PENDING_RUNS + 3 }, (_, i) => fakeMember(guild, ID(200 + i)));
    svc.__setAutomationMemo(guild.id, {
      MESSAGE_SENT: [lite('autoqueue01', [{ type: 'SEND_MESSAGE', content: 'x', channelId: general.id }], 600)],
    });
    const gates = [];
    general.send = (p) => new Promise((done) => { gates.push(() => { general.sent.push(p); done({ id: 's' }); }); });
    // Not awaited: every run queues behind the first one, which is blocked in send().
    await Promise.all(members.map((member) => svc.dispatch('MESSAGE_SENT', baseOf(guild, {
      trigger: 'MESSAGE_SENT', member, user: member.user, channel: general, message: fakeMessage(guild, general, member.user),
    }))));
    for (let i = 0; i < 5; i++) await flush();
    const dropped = members.slice(svc.MAX_PENDING_RUNS).map((m) => `automation:cd:${guild.id}:autoqueue01:${m.id}`);
    assert.deepEqual([...stub.dels].sort(), [...dropped].sort(), 'only the three dropped members got their cooldown back');
    for (let i = 0; i < 60; i++) { gates.shift()?.(); await flush(); }
    assert.equal(general.sent.length, svc.MAX_PENDING_RUNS);
  } finally {
    stub.restore();
  }
});

// ── 3. /tag info, raw and action list hide disabled commands from members ────

const GCMD = '955028900000000003';
const tagRow = (over = {}) => ({
  id: 'tag1', guildId: GCMD, name: 'draft', enabled: false, content: 'unreleased announcement', useEmbed: false, actions: null,
  description: null, uses: 0, createdBy: ID(1), createdAt: new Date(), updatedAt: new Date(), cooldownScope: 'NONE', cooldownSeconds: 0,
  deleteInvocation: false, allowRoleIds: [], denyRoleIds: [], allowChannelIds: [], denyChannelIds: [], ...over,
});

function tagInteraction(sub, group, { admin: isAdmin }) {
  const guild = {
    id: GCMD, ownerId: ID(9), roles: { cache: new Map() }, channels: { cache: new Map() },
  };
  const member = {
    id: ID(5), guild, roles: { cache: new Map() }, permissions: { has: (bit) => isAdmin && bit === PermissionFlagsBits.ManageGuild },
  };
  const calls = { editReply: [], deferReply: [] };
  const i = {
    guild, guildId: GCMD, member, user: { id: member.id }, channel: { id: ID(6) }, client: {},
    createdTimestamp: Date.now(), deferred: false, replied: false, calls,
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => group,
      getString: (n) => (n === 'name' ? 'draft' : null),
      getBoolean: () => null, getInteger: () => null, getChannel: () => null, getRole: () => null,
    },
    deferReply: async (p) => { calls.deferReply.push(p); i.deferred = true; i.ephemeral = p?.ephemeral === true; },
    followUp: async (p) => { calls.editReply.push(p); },
    editReply: async (p) => { calls.editReply.push(p); },
    reply: async (p) => { calls.editReply.push(p); i.replied = true; },
  };
  return i;
}

test('/tag info, raw and action list: a disabled command is "not found" for a member, visible to an admin', async () => {
  prisma.guildSettings = {
    findUnique: async () => ({ guildId: GCMD, customCommandsEnabled: true, customPrefix: '!', embedColor: null, modRoleIds: [] }),
    upsert: async ({ update, create }) => ({ guildId: GCMD, customCommandsEnabled: true, customPrefix: '!', embedColor: null, modRoleIds: [], ...create, ...update }),
  };
  const rows = { draft: tagRow(), live: tagRow({ name: 'live', enabled: true }) };
  prisma.customTag = { findUnique: async ({ where }) => rows[where.guildId_name.name] ?? null };
  const run = async (sub, group, as, name = 'draft') => {
    const i = tagInteraction(sub, group, as);
    i.options.getString = (n) => (n === 'name' ? name : null);
    await tagCmd.execute(i);
    return i.calls.editReply.at(-1);
  };
  const text = (p) => `${p.content ?? ''}${p.embeds?.map((e) => `${e.data.title ?? ''} ${e.data.description ?? ''}`).join(' ') ?? ''}`;

  for (const [sub, group] of [['raw', null], ['info', null], ['list', 'action']]) {
    const asMember = await run(sub, group, { admin: false });
    assert.match(text(asMember), /Not Found/, `${group ? `${group} ` : ''}${sub}: member gets Not Found`);
    assert.doesNotMatch(text(asMember), /unreleased announcement/, `${sub}: the text stays hidden`);

    const asAdmin = await run(sub, group, { admin: true });
    assert.doesNotMatch(text(asAdmin), /Not Found/, `${sub}: an admin can still inspect it`);

    const live = await run(sub, group, { admin: false }, 'live');
    assert.doesNotMatch(text(live), /Not Found/, `${sub}: an enabled command stays readable by members`);
  }
  assert.match(text(await run('raw', null, { admin: true })), /unreleased announcement/);
});

// ── 4. Privacy and access: copies stay out of public channels, admins need the access they configure ───────

const { PermissionsBitField } = await import('discord.js');
const autoCmd = await import('../src/commands/admin/automation.js');
const embedCmd = await import('../src/commands/admin/embed.js');
const embedSvc = await import('../src/services/embedService.js');
const guards = await import('../src/utils/privacyGuards.js');
const realAutomation = prisma.automation;
const realTransaction = prisma.$transaction;
const realSavedEmbed = prisma.savedEmbed;
after(() => {
  prisma.automation = realAutomation;
  prisma.$transaction = realTransaction;
  prisma.savedEmbed = realSavedEmbed;
});

const bits = (...names) => new PermissionsBitField(names.reduce((a, n) => a | PermissionFlagsBits[n], 0n));
const ALL = () => new PermissionsBitField(PermissionsBitField.All);

function privGuild(id) {
  const guild = fakeGuild(id);
  guild.ownerId = ID(990);
  return guild;
}

/**
 * A text channel whose permissions depend on who asks: @everyone (the guild id or a role with that id) sees it when `everyone`,
 * the bot can do anything, and any other member gets exactly the `member` list.
 */
function accessChannel(guild, id, { everyone = true, member = ['ViewChannel', 'SendMessages'], thread = false } = {}) {
  const sent = [];
  const ch = {
    id, type: thread ? ChannelType.PublicThread : ChannelType.GuildText, name: `c${id.slice(-3)}`, guild, guildId: guild.id, parentId: null, sent,
    isTextBased: () => true, isThread: () => thread,
    permissionsFor(who) {
      if (who === guild.id || who?.id === guild.id) return everyone ? bits('ViewChannel', 'SendMessages') : bits();
      if (who?.id === BOT) return ALL();
      return bits(...member);
    },
    send: async (payload) => { sent.push(payload); return { id: `sent${sent.length}` }; },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function privRole(guild, id, { position = 1, mentionable = false } = {}) {
  const role = { id, name: `r${id.slice(-3)}`, position, managed: false, mentionable, permissions: { has: () => false } };
  guild.roles.cache.set(id, role);
  return role;
}

/** The admin who runs a command: `top` is their highest role position; `owner` / `administrator` are the exempt kinds. */
function adminMember(guild, id, { top = 5, owner = false, administrator = false } = {}) {
  return {
    id: owner ? guild.ownerId : id, guild, roles: { highest: { position: top } },
    permissions: { has: (bit) => administrator && bit === PermissionFlagsBits.Administrator },
  };
}

test('memberChannelProblem: View Channel and Send Messages are needed; the owner and Administrators are exempt', () => {
  const guild = privGuild('955028900000000020');
  const open = accessChannel(guild, ID(410));
  const readOnly = accessChannel(guild, ID(411), { member: ['ViewChannel'] });
  const hidden = accessChannel(guild, ID(412), { member: [] });
  const thread = accessChannel(guild, ID(413), { member: ['ViewChannel', 'SendMessages'], thread: true });
  const me = adminMember(guild, ID(420));
  assert.equal(guards.memberChannelProblem(open, me), null);
  assert.match(guards.memberChannelProblem(readOnly, me) ?? '', /\*\*Send Messages\*\* in <#\d+>/);
  assert.match(guards.memberChannelProblem(hidden, me) ?? '', /\*\*View Channel, Send Messages\*\*/);
  assert.match(guards.memberChannelProblem(thread, me) ?? '', /Send Messages in Threads/, 'a thread needs the thread permission');
  assert.equal(guards.memberChannelProblem(hidden, adminMember(guild, ID(421), { owner: true })), null, 'owner');
  assert.equal(guards.memberChannelProblem(hidden, adminMember(guild, ID(422), { administrator: true })), null, 'Administrator');
  assert.equal(guards.memberChannelProblem(null, me), null);
  assert.equal(guards.memberChannelProblem(hidden, null), null, 'no member: nothing to compare with');
  assert.notEqual(guards.memberChannelProblem(hidden, me, ['ViewChannel']), null);
  assert.equal(guards.memberChannelProblem(readOnly, me, ['ViewChannel']), null);
});

test('roleHierarchyProblem: a role at or above the highest role is refused; the owner is exempt; unreadable roles fail closed', () => {
  const guild = privGuild('955028900000000020');
  const low = privRole(guild, ID(425), { position: 2 });
  const mid = privRole(guild, ID(426), { position: 5 });
  const high = privRole(guild, ID(427), { position: 8 });
  const me = adminMember(guild, ID(428), { top: 5 });
  assert.equal(guards.roleHierarchyProblem(guild, me, [low.id]), null);
  assert.match(guards.roleHierarchyProblem(guild, me, [low.id, mid.id]) ?? '', /at or above your highest role/, 'equal counts as above');
  assert.match(guards.roleHierarchyProblem(guild, me, [high.id]) ?? '', new RegExp(high.id));
  assert.equal(guards.roleHierarchyProblem(guild, me, [ID(499)]), null, 'a deleted role');
  assert.equal(guards.roleHierarchyProblem(guild, me, []), null);
  assert.equal(guards.roleHierarchyProblem(guild, adminMember(guild, ID(429), { owner: true, top: 1 }), [high.id]), null, 'owner');
  assert.equal(guards.roleHierarchyProblem(guild, null, [high.id]), null);
  assert.match(guards.roleHierarchyProblem(guild, { id: ID(430), roles: {} }, [high.id]) ?? '', /could not be checked/);
});

test('automation config checks: channel access, role pings and the imported actions', async () => {
  const guild = privGuild('955028900000000020');
  const open = accessChannel(guild, ID(431));
  const secret = accessChannel(guild, ID(432), { member: [] });
  const chatty = accessChannel(guild, ID(433), { member: ['ViewChannel', 'SendMessages', 'MentionEveryone'] });
  const quiet = privRole(guild, ID(434));
  const loud = privRole(guild, ID(435), { mentionable: true });
  const me = adminMember(guild, ID(436));

  const actions = [{ type: 'SEND_MESSAGE', content: 'x', channelId: open.id }, { type: 'REPOST', channelId: secret.id }];
  assert.equal(svc.actionChannelAccessProblem(guild, me, actions.slice(0, 1)), null);
  assert.match(svc.actionChannelAccessProblem(guild, me, actions) ?? '', /^Action 2: You need \*\*View Channel, Send Messages\*\* in <#\d+> to do that\.$/);
  assert.match(svc.actionChannelAccessProblem(guild, me, actions, { first: 5 }) ?? '', /^Action 6:/);
  assert.equal(svc.actionChannelAccessProblem(guild, adminMember(guild, ID(437), { administrator: true }), actions), null);
  assert.equal(svc.actionChannelAccessProblem(guild, adminMember(guild, ID(438), { owner: true }), actions), null);
  assert.equal(svc.actionChannelAccessProblem(guild, me, [{ type: 'REPOST', channelId: ID(499) }]), null, 'a deleted channel is left to the reference check');
  assert.equal(svc.actionChannelAccessProblem(guild, me, [{ type: 'SEND_MESSAGE', content: 'x', channelId: null }, { type: 'PIN' }]), null, 'no target channel');
  assert.match(svc.actionChannelAccessProblem(guild, me, [{ type: 'CREATE_THREAD', channelId: secret.id }]) ?? '', /Create Public Threads/);
  assert.match(svc.actionChannelAccessProblem(guild, me, [{ type: 'SEND_EMBED', embedName: 'e', channelId: secret.id }]) ?? '', /^Action 1:/);

  assert.equal(svc.rolePingRefusal(me, open, `hi <@&${loud.id}>`), null, 'a mentionable role');
  assert.match(svc.rolePingRefusal(me, open, `hi <@&${quiet.id}>`) ?? '', /^<@&\d+> is not mentionable and you lack \*\*Mention @everyone, @here and All Roles\*\* in <#\d+>\.$/);
  assert.match(svc.rolePingRefusal(me, open, `<@&${quiet.id}> <@&${ID(498)}>`) ?? '', /are not mentionable/);
  assert.equal(svc.rolePingRefusal(me, chatty, `hi <@&${quiet.id}>`), null, 'Mention Everyone in the channel');
  assert.equal(svc.rolePingRefusal(null, open, `hi <@&${quiet.id}>`), null, 'no member to compare with');
  assert.equal(svc.rolePingRefusal(me, open, 'no roles here'), null);

  const reply = [{ type: 'SEND_MESSAGE', content: `<@&${quiet.id}>`, channelId: null }];
  assert.match(svc.importRolePingProblem(guild, me, reply, { fallbackChannel: open }) ?? '', /^Action 1: /);
  assert.equal(svc.importRolePingProblem(guild, me, reply, { fallbackChannel: chatty }), null);
  assert.equal(svc.importRolePingProblem(guild, me, [{ type: 'SEND_MESSAGE', content: `<@&${quiet.id}>`, channelId: chatty.id }], { fallbackChannel: open }), null, 'the action channel wins');

  const params = (type, opts, ctx) => svc.actionParamsFromOptions(type, opts, { guild, ...ctx });
  const refused = await params('SEND_MESSAGE', { text: `hi <@&${quiet.id}>`, channel: open }, { member: me });
  assert.equal(refused.ok, false);
  assert.equal(refused.errorKind, 'ping');
  assert.equal((await params('SEND_MESSAGE', { text: `hi <@&${loud.id}>`, channel: open }, { member: me })).ok, true);
  assert.equal((await params('SEND_MESSAGE', { text: `hi <@&${quiet.id}>`, channel: chatty }, { member: me })).ok, true);
  assert.equal((await params('REPLY', { text: `hi <@&${quiet.id}>` }, { member: me, channel: open })).errorKind, 'ping', 'a reply is checked where the admin works');
  assert.equal((await params('REPLY', { text: `hi <@&${quiet.id}>` }, { member: null, channel: open })).ok, true, 'no executor: not checked');
});

test('dispatch: REPOST and {message.content} never copy a message from a channel @everyone cannot see into one it can', async () => {
  const stub = stubCooldownCache();
  try {
    const guild = privGuild('955028900000000021');
    const hidden = accessChannel(guild, ID(440), { everyone: false });
    const open = accessChannel(guild, ID(441));
    const vault = accessChannel(guild, ID(442), { everyone: false });
    const member = fakeMember(guild, ID(443));
    let n = 0;
    const fire = async (source, actions) => {
      svc.__setAutomationMemo(guild.id, { MESSAGE_SENT: [lite(`autoleak${String(++n).padStart(2, '0')}`, actions)] });
      await svc.dispatch('MESSAGE_SENT', baseOf(guild, {
        trigger: 'MESSAGE_SENT', member, user: member.user, channel: source, message: fakeMessage(guild, source, member.user),
      }), { awaitRuns: true });
    };

    await fire(hidden, [{ type: 'REPOST', channelId: open.id }]);
    assert.equal(open.sent.length, 0, 'hidden source, public target: skipped');
    await fire(hidden, [{ type: 'REPOST', channelId: vault.id }]);
    assert.equal(vault.sent.length, 1, 'hidden source, hidden target: reposted');
    await fire(open, [{ type: 'REPOST', channelId: open.id }]);
    assert.equal(open.sent.length, 1, 'public source: reposted');

    await fire(hidden, [{ type: 'SEND_MESSAGE', content: 'said: {message.content}', channelId: open.id }]);
    assert.equal(open.sent.length, 1, 'a text that copies the content is skipped too');
    await fire(hidden, [{ type: 'SEND_MESSAGE', content: 'said: { Message.Old_Content }', channelId: open.id }]);
    assert.equal(open.sent.length, 1, 'whatever the spelling of the placeholder');
    await fire(hidden, [{ type: 'SEND_MESSAGE', content: 'someone posted in the vault', channelId: open.id }]);
    assert.equal(open.sent.length, 2, 'a text that does not copy the message is fine');
    await fire(hidden, [{ type: 'SEND_MESSAGE', content: 'said: {message.content}', channelId: vault.id }]);
    assert.equal(vault.sent.length, 2, 'private to private is fine');
  } finally {
    stub.restore();
  }
});

// ── /automation: a lower admin cannot touch an automation that hands out a role they outrank, and needs the channel access ──

const GA = '955028900000000022';
const stubSettings = (guildId) => {
  const row = { guildId, automationsEnabled: true, customCommandsEnabled: true, embedColor: null, customPrefix: '!', modRoleIds: [] };
  prisma.guildSettings = {
    findUnique: async () => row,
    upsert: async ({ update, create }) => ({ ...row, ...create, ...update }),
  };
};

function autoWorld(makeActions, { guildId = GA } = {}) {
  const guild = privGuild(guildId);
  const roles = { low: privRole(guild, ID(450), { position: 2 }), high: privRole(guild, ID(451), { position: 8 }) };
  const world = {
    guild, roles, row: null,
    open: accessChannel(guild, ID(453)),
    secret: accessChannel(guild, ID(454), { member: [] }),
    chatty: accessChannel(guild, ID(455), { member: ['ViewChannel', 'SendMessages', 'MentionEveryone'] }),
    quiet: privRole(guild, ID(456)),
    loud: privRole(guild, ID(457), { mentionable: true }),
  };
  world.row = {
    id: 'autorow00001', guildId, name: 'gate', trigger: 'MESSAGE_SENT', enabled: false, cooldownSeconds: 0, uses: 0, buttonSeq: 0,
    conditions: [], actions: svc.normalizeList(makeActions(roles), 'action').items, createdBy: ID(1), createdAt: new Date(), updatedAt: new Date(),
  };
  prisma.automation = {
    findUnique: async () => world.row,
    findMany: async () => [],
    update: async ({ data }) => { world.row = { ...world.row, ...data }; return world.row; },
  };
  prisma.$transaction = async (fn) => fn({ $queryRaw: async () => [{ id: world.row.id }], automation: prisma.automation });
  stubSettings(guildId);
  return world;
}

async function autoRun(world, member, { group = null, sub, opts = {} }) {
  const replies = [];
  const get = (name) => (Object.hasOwn(opts, name) ? opts[name] : null);
  const interaction = {
    guild: world.guild, guildId: world.guild.id, channel: world.open, replies, createdTimestamp: Date.now(), deferred: false, replied: false,
    user: { id: member.id, bot: false }, member,
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      get: (name) => (get(name) === null ? null : { name, value: get(name) }),
      getString: get, getInteger: get, getBoolean: get, getChannel: get, getRole: get, getUser: get, getMember: () => null, getAttachment: get,
    },
    async deferReply() { interaction.deferred = true; },
    async editReply(p) { replies.push(p); return p; },
    async followUp(p) { replies.push(p); return p; },
  };
  await autoCmd.execute(interaction);
  const embed = replies.at(-1)?.embeds?.[0]?.toJSON?.() ?? {};
  return { title: embed.title ?? '', description: embed.description ?? '' };
}

test('/automation: a lower admin cannot edit, enable, disable or change an automation that gives a role at or above their own', async () => {
  const w = autoWorld(({ high }) => [{ type: 'GIVE_ROLES', roleIds: [high.id] }]);
  const lower = adminMember(w.guild, ID(460), { top: 5 });
  const specs = [
    { sub: 'enable', opts: { name: 'gate' } },
    { sub: 'disable', opts: { name: 'gate' } },
    { sub: 'edit', opts: { name: 'gate', cooldown: 30 } },
    { group: 'condition', sub: 'add', opts: { name: 'gate', type: 'CONTAINS', text: 'x' } },
    { group: 'condition', sub: 'remove', opts: { name: 'gate', index: 1 } },
    { group: 'action', sub: 'add', opts: { name: 'gate', type: 'SEND_MESSAGE', text: 'hi' } },
    { group: 'action', sub: 'remove', opts: { name: 'gate', index: 1 } },
  ];
  for (const spec of specs) {
    const before = JSON.stringify(w.row);
    const r = await autoRun(w, lower, spec);
    assert.match(r.title, /Role Not Allowed/, `${spec.group ?? ''} ${spec.sub}: ${r.title} ${r.description}`);
    assert.match(r.description, /at or above your highest role/);
    assert.equal(JSON.stringify(w.row), before, `${spec.sub}: nothing was written`);
  }

  // An automation of roles below the admin's own is theirs to change; so is anything for the guild owner.
  const below = autoWorld(({ low }) => [{ type: 'GIVE_ROLES', roleIds: [low.id] }]);
  assert.match((await autoRun(below, adminMember(below.guild, ID(461), { top: 5 }), { sub: 'edit', opts: { name: 'gate', cooldown: 30 } })).title, /Automation Updated/);
  assert.match((await autoRun(below, adminMember(below.guild, ID(461), { top: 5 }), { sub: 'enable', opts: { name: 'gate' } })).title, /Automation Enabled/);
  const owned = autoWorld(({ high }) => [{ type: 'GIVE_ROLES', roleIds: [high.id] }]);
  const owner = adminMember(owned.guild, ID(462), { owner: true, top: 1 });
  assert.match((await autoRun(owned, owner, { sub: 'enable', opts: { name: 'gate' } })).title, /Automation Enabled/);
  assert.equal(owned.row.enabled, true);

  // An automation without role actions is not affected by the admin's roles.
  const plain = autoWorld(() => [{ type: 'PIN' }]);
  assert.match((await autoRun(plain, adminMember(plain.guild, ID(463), { top: 0 }), { sub: 'edit', opts: { name: 'gate', cooldown: 5 } })).title, /Automation Updated/);
});

test('/automation action add and presets: the admin needs View Channel and Send Messages in the target channel', async () => {
  const w = autoWorld(({ low }) => [{ type: 'GIVE_ROLES', roleIds: [low.id] }]);
  const me = adminMember(w.guild, ID(470), { top: 5 });
  const add = (member, opts) => autoRun(w, member, { group: 'action', sub: 'add', opts: { name: 'gate', ...opts } });

  for (const type of ['SEND_MESSAGE', 'REPOST', 'CREATE_THREAD']) {
    const r = await add(me, { type, channel: w.secret, text: 'hi' });
    assert.match(r.title, /No Access/, type);
    assert.match(r.description, /^Action 2: You need/, type);
  }
  assert.equal(w.row.actions.length, 1, 'nothing was added');
  assert.match((await add(me, { type: 'SEND_MESSAGE', channel: w.open, text: 'hi' })).title, /Action Added/);
  assert.match((await add(adminMember(w.guild, ID(471), { administrator: true }), { type: 'SEND_MESSAGE', channel: w.secret, text: 'hi' })).title, /Action Added/);
  assert.match((await add(adminMember(w.guild, ID(472), { owner: true }), { type: 'REPOST', channel: w.secret })).title, /Action Added/);

  // The presets post and repost too.
  for (const preset of ['welcomer', 'starboard']) {
    const r = await autoRun(w, me, { sub: 'create', opts: { preset, name: `p-${preset}`, channel: w.secret } });
    assert.match(r.title, /No Access/, preset);
  }
});

test('/automation: a role mention the admin could not ping is refused (it used to be a note on the success reply)', async () => {
  const w = autoWorld(({ low }) => [{ type: 'GIVE_ROLES', roleIds: [low.id] }]);
  const me = adminMember(w.guild, ID(475), { top: 5 });
  const add = (opts) => autoRun(w, me, { group: 'action', sub: 'add', opts: { name: 'gate', ...opts } });
  const refused = await add({ type: 'SEND_MESSAGE', channel: w.open, text: `Hey <@&${w.quiet.id}>` });
  assert.match(refused.title, /Role Not Pingable/);
  assert.match(refused.description, /not mentionable and you lack \*\*Mention @everyone, @here and All Roles\*\*/);
  assert.match((await add({ type: 'REPLY', text: `Hey <@&${w.quiet.id}>` })).title, /Role Not Pingable/, 'a reply is checked in the channel the admin works in');
  assert.equal(w.row.actions.length, 1, 'nothing was added');
  assert.match((await add({ type: 'SEND_MESSAGE', channel: w.open, text: `Hey <@&${w.loud.id}>` })).title, /Action Added/, 'a mentionable role');
  assert.match((await add({ type: 'SEND_MESSAGE', channel: w.chatty, text: `Hey <@&${w.quiet.id}>` })).title, /Action Added/, 'Mention Everyone in the channel');
  const preset = await autoRun(w, me, { sub: 'create', opts: { preset: 'welcomer', name: 'hello', channel: w.open, message: `Welcome <@&${w.quiet.id}>` } });
  assert.match(preset.title, /Role Not Pingable/);
});

test('/automation import: access, role pings and the automation being replaced are checked before anything is saved', () => {
  const src = readFileSync(resolve(ROOT, 'src/commands/admin/automation.js'), 'utf8');
  const body = src.slice(src.indexOf('async function importJson'));
  const writes = Math.min(body.indexOf('auto.replaceAutomation('), body.indexOf('auto.createAutomation('));
  for (const check of ['auto.actionChannelAccessProblem(', 'auto.importRolePingProblem(', 'auto.importRoleHierarchyProblem(guild, interaction.member, existing.actions)']) {
    const at = body.indexOf(check);
    assert.ok(at > 0 && at < writes, `${check} runs before the import writes`);
  }
});

// ── /tag: the same rules ─────────────────────────────────────────────────────

const GT = '955028900000000023';

function tagWorld(makeActions) {
  const guild = privGuild(GT);
  const roles = { low: privRole(guild, ID(480), { position: 2 }), high: privRole(guild, ID(481), { position: 8 }) };
  const world = {
    guild, roles, row: null, open: accessChannel(guild, ID(482)), secret: accessChannel(guild, ID(483), { member: [] }),
  };
  world.row = tagRow({
    id: 'tag-vip', guildId: GT, name: 'vip', enabled: true, content: 'hello', actions: makeActions(roles), updatedAt: new Date(1_700_000_000_000),
  });
  prisma.customTag = {
    findUnique: async () => world.row,
    updateMany: async ({ data }) => { world.row = { ...world.row, ...data, updatedAt: new Date(world.row.updatedAt.getTime() + 1) }; return { count: 1 }; },
  };
  stubSettings(GT);
  return world;
}

async function tagRun(world, member, sub, group, opts = {}) {
  const replies = [];
  const interaction = {
    guild: world.guild, guildId: GT, member, user: { id: member.id }, channel: world.open, client: {}, createdTimestamp: Date.now(), deferred: false, replied: false,
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => group,
      getString: (n) => opts[n] ?? null, getBoolean: (n) => opts[n] ?? null, getInteger: (n) => opts[n] ?? null,
      getChannel: (n) => opts[n] ?? null, getRole: (n) => opts[n] ?? null,
    },
    deferReply: async () => { interaction.deferred = true; },
    editReply: async (p) => { replies.push(p); },
    followUp: async (p) => { replies.push(p); },
    reply: async (p) => { replies.push(p); interaction.replied = true; },
  };
  await tagCmd.execute(interaction);
  const e = replies.at(-1)?.embeds?.[0]?.data ?? {};
  return { title: e.title ?? '', description: e.description ?? '' };
}

const vipActions = ({ high }) => [
  { type: 'addRole', roleId: high.id },
  { type: 'reply', dm: false, embed: false, responses: ['hello'] },
];

test('/tag: a lower admin cannot edit, toggle or change the actions of a command that gives a role at or above their own', async () => {
  const w = tagWorld(vipActions);
  const lower = adminMember(w.guild, ID(485), { top: 5 });
  const specs = [
    ['toggle', null, {}],
    ['edit', null, { content: 'changed' }],
    ['cooldown', null, { scope: 'SERVER', duration: '30s' }],
    ['permissions', null, { rule: 'allow', role: { id: w.roles.low.id } }],
    ['permissions', null, { rule: 'clear' }],
    ['add', 'action', { type: 'reply', text: 'more' }],
    ['remove', 'action', { action: 2 }],
    ['add', 'response', { text: 'another', action: 2 }],
  ];
  for (const [sub, group, opts] of specs) {
    const before = JSON.stringify(w.row);
    const r = await tagRun(w, lower, sub, group, { name: 'vip', ...opts });
    assert.match(r.title, /Role Not Allowed/, `${group ?? ''} ${sub}: ${r.title} ${r.description}`);
    assert.match(r.description, /at or above your highest role/);
    assert.equal(JSON.stringify(w.row), before, `${sub}: nothing was written`);
  }
  assert.equal(w.row.enabled, true);

  // The guild owner, and an admin who outranks the role, can.
  const owner = adminMember(w.guild, ID(486), { owner: true, top: 1 });
  assert.match((await tagRun(w, owner, 'toggle', null, { name: 'vip' })).title, /Custom Command Disabled/);
  const higher = adminMember(w.guild, ID(487), { top: 9 });
  assert.match((await tagRun(w, higher, 'toggle', null, { name: 'vip' })).title, /Custom Command Enabled/);

  // A command without role actions, or with roles below the admin, is open to a lower admin.
  const below = tagWorld(({ low }) => [{ type: 'addRole', roleId: low.id }, { type: 'reply', dm: false, embed: false, responses: ['hello'] }]);
  assert.match((await tagRun(below, adminMember(below.guild, ID(488), { top: 5 }), 'toggle', null, { name: 'vip' })).title, /Custom Command Disabled/);
  const plain = tagWorld(() => [{ type: 'reply', dm: false, embed: false, responses: ['hello'] }]);
  assert.match((await tagRun(plain, adminMember(plain.guild, ID(489), { top: 0 }), 'edit', null, { name: 'vip', content: 'changed' })).title, /Custom Command Updated/);
});

test('/tag action add send: the admin needs View Channel and Send Messages in the channel', async () => {
  const w = tagWorld(() => [{ type: 'reply', dm: false, embed: false, responses: ['hello'] }]);
  const me = adminMember(w.guild, ID(490), { top: 5 });
  const send = (member, channel) => tagRun(w, member, 'add', 'action', { name: 'vip', type: 'send', text: 'hi', channel });
  const denied = await send(me, w.secret);
  assert.match(denied.title, /No Access/);
  assert.match(denied.description, /View Channel, Send Messages/);
  assert.equal(w.row.actions.length, 1, 'nothing was added');
  assert.match((await send(me, w.open)).title, /Action Added/);
  assert.match((await send(adminMember(w.guild, ID(491), { administrator: true }), w.secret)).title, /Action Added/);
  assert.match((await send(adminMember(w.guild, ID(492), { owner: true }), w.secret)).title, /Action Added/);
});

// ── /embed: publish needs Send Messages, update and delete need View Channel ──

test('embeds: publish needs View Channel and Send Messages; update and delete need View Channel in the published channel', async () => {
  const GE = '955028900000000024';
  const guild = privGuild(GE);
  const open = accessChannel(guild, ID(500));
  const readOnly = accessChannel(guild, ID(501), { member: ['ViewChannel'] });
  const hidden = accessChannel(guild, ID(502), { member: [] });
  const me = adminMember(guild, ID(503));
  const row = { id: 'emb1', guildId: GE, name: 'rules', template: {}, channelId: hidden.id, messageId: ID(504) };

  assert.equal((await embedSvc.publishSavedEmbed({ row, channel: readOnly, member: me })).code, 'MEMBER_NO_ACCESS', 'it can see the channel but not post');
  assert.equal((await embedSvc.publishSavedEmbed({ row, channel: hidden, member: me })).code, 'MEMBER_NO_ACCESS');
  prisma.savedEmbed = { findUnique: async () => null };
  await assert.rejects(embedSvc.publishSavedEmbed({ row, channel: open, member: me }), (err) => err.code === 'NOTFOUND', 'with access it gets past the check');

  prisma.savedEmbed = { findUnique: async () => row };
  assert.equal((await embedSvc.updatePublishedEmbed({ row, guild, member: me })).code, 'MEMBER_NO_ACCESS', 'update of a message in a channel the member cannot see');

  const replies = [];
  const interaction = {
    guild, guildId: GE, member: me, user: { id: me.id }, createdTimestamp: Date.now(), deferred: false, replied: false,
    options: { getSubcommandGroup: () => null, getSubcommand: () => 'delete', getString: () => 'rules', getBoolean: () => false },
    reply: async (p) => { replies.push(p); interaction.replied = true; },
    editReply: async (p) => { replies.push(p); },
    followUp: async (p) => { replies.push(p); },
  };
  await embedCmd.execute(interaction, {});
  const e = replies.at(-1)?.embeds?.[0]?.data ?? {};
  assert.match(e.title ?? '', /No Access/, 'delete of an embed published in a channel the member cannot see');
  assert.match(e.description ?? '', /published in <#\d+>/);
});

// ── Dashboard: the real collections opt in to the stored-role check, and their items carry the roles it reads ──

test('dashboard: automations and custom commands opt in to checkStoredRoles; their items expose every assignable role', async () => {
  const { COLLECTIONS } = await import('../src/dashboard/collections/index.js');
  const { automationItem } = await import('../src/dashboard/collections/automations.js');
  const { tagItem } = await import('../src/dashboard/collections/tags.js');
  const { collectAssignableRoles } = await import('../src/dashboard/schema.js');
  assert.equal(COLLECTIONS.automations.checkStoredRoles, true);
  assert.equal(COLLECTIONS.tags.checkStoredRoles, true);
  assert.equal(COLLECTIONS.embeds.checkStoredRoles, undefined, 'other collections keep the old behaviour');

  const give = ID(601);
  const take = ID(602);
  const auto = automationItem({
    id: 'a1', name: 'gate', trigger: 'MESSAGE_SENT', enabled: false, cooldownSeconds: 0, uses: 0, conditions: [],
    actions: svc.normalizeList([{ type: 'GIVE_ROLES', roleIds: [give] }, { type: 'PIN' }, { type: 'TAKE_ROLES', roleIds: [take] }], 'action').items,
  });
  assert.deepEqual(collectAssignableRoles(COLLECTIONS.automations.fields, auto).map((r) => r.id).sort(), [give, take].sort());
  assert.deepEqual(collectAssignableRoles(COLLECTIONS.automations.fields, auto).map((r) => r.path).sort(), ['actions.0.roleIds.0', 'actions.2.roleIds.0']);

  const tag = tagItem({ id: 't1', name: 'vip', enabled: true }, [
    { type: 'reply', dm: false, embed: false, responses: ['hi'] },
    { type: 'toggleRole', roleId: give },
  ]);
  assert.deepEqual(collectAssignableRoles(COLLECTIONS.tags.fields, tag), [{ id: give, path: 'actions.1.roleId' }]);
});
