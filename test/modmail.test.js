// Modmail tests. Offline (no Discord, Postgres or Redis): the pure rules, the ModmailThread access against an in-memory table,
// opening / relaying / closing against fake guilds, channels, threads and users, the DM flow (prompt, select, buttons),
// the staff relay, the cleanup hooks, the plugin declaration and the /modmail payload and subcommands.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { ChannelType, PermissionFlagsBits } from 'discord.js';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const svc = await import('../src/services/modmailService.js');
const mm = await import('../src/plugins/modmail.js');
const cmd = await import('../src/commands/admin/modmail.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { CUSTOM_ID_PREFIXES } = await import('../src/config/customIds.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { DEFAULT_TEXTS } = await import('../src/dashboard/fields.js');
const { updateSettings, peekSettings } = await import('../src/services/settingsService.js');

const { MODMAIL } = svc;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory tables ──────────────────────────────────────────────────────────

const threads = new Map();   // id -> ModmailThread row
const settingsTable = new Map();
let seq = 1;

const matches = (row, where = {}) => Object.entries(where).every(([k, v]) => {
  if (v && typeof v === 'object' && !(v instanceof Date)) {
    if ('in' in v) return v.in.includes(row[k]);
    if ('not' in v) return row[k] !== v.not;
  }
  return row[k] === v;
});
const all = (where) => [...threads.values()].filter((r) => matches(r, where));

const modmailThread = {
  findUnique: async ({ where }) => (all({ threadId: where.threadId })[0] ?? null),
  findFirst: async ({ where }) => (all(where)[0] ?? null),
  findMany: async ({ where, take }) => all(where).sort((a, b) => b.lastActivityAt - a.lastActivityAt).slice(0, take ?? 1000),
  create: async ({ data }) => {
    const row = {
      id: `row${seq++}`, status: 'OPEN', closedById: null, closedAt: null, closeReason: null,
      lastActivityAt: new Date(), createdAt: new Date(), updatedAt: new Date(), ...data,
    };
    threads.set(row.id, row);
    return { ...row };
  },
  update: async ({ where, data }) => {
    const row = threads.get(where.id);
    Object.assign(row, data);
    return { ...row };
  },
  updateMany: async ({ where, data }) => {
    const rows = all(where);
    for (const r of rows) Object.assign(r, data);
    return { count: rows.length };
  },
  deleteMany: async ({ where }) => {
    const rows = all(where);
    for (const r of rows) threads.delete(r.id);
    return { count: rows.length };
  },
};
const guildSettings = {
  findUnique: async ({ where }) => settingsTable.get(where.guildId) ?? null,
  upsert: async ({ where, update, create }) => {
    const row = { ...(settingsTable.get(where.guildId) ?? { guildId: where.guildId, ...create }), ...update };
    settingsTable.set(where.guildId, row);
    return { ...row };
  },
  update: async ({ where, data }) => {
    const row = { ...settingsTable.get(where.guildId), ...data };
    settingsTable.set(where.guildId, row);
    return { ...row };
  },
};
const tx = { $queryRaw: async () => [], $executeRaw: async () => 0, modmailThread, guildSettings };

const real = { modmailThread: prisma.modmailThread, guildSettings: prisma.guildSettings, tx: prisma.$transaction };
prisma.modmailThread = modmailThread;
prisma.guildSettings = guildSettings;
prisma.$transaction = async (fn) => fn(tx);

after(async () => {
  prisma.modmailThread = real.modmailThread;
  prisma.guildSettings = real.guildSettings;
  prisma.$transaction = real.tx;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

let nextId = 5000;
const errCode = (code, message = 'discord error') => Object.assign(new Error(message), { code });

function fakeUser(id, username = `user${id}`, { dmClosed = false } = {}) {
  const dm = {
    sent: [],
    async send(payload) { this.sent.push(payload); return { id: String(nextId++) }; },
    messages: {
      // A message id, or `{ after, limit }` (the messages right after one, newest first, like Discord's).
      fetch: async (arg) => {
        if (arg && typeof arg === 'object') {
          const after = BigInt(arg.after);
          return new Map([...dm.byId.values()].filter((m) => BigInt(m.id) > after).sort((a, b) => (BigInt(a.id) < BigInt(b.id) ? 1 : -1))
            .slice(0, arg.limit).map((m) => [m.id, m]));
        }
        return dm.byId.get(arg) ?? Promise.reject(errCode(10008));
      },
    },
    byId: new Map(),
  };
  return {
    id, username, displayName: username, bot: false, createdTimestamp: Date.now() - 400 * 86_400_000, dm, dmClosed,
    displayAvatarURL: () => 'https://cdn.example/a.png',
    sent: dm.sent,
    async send(payload) {
      if (this.dmClosed) throw errCode(50007, 'Cannot send messages to this user');
      return dm.send(payload);
    },
    createDM: async () => dm,
  };
}

function fakeDmMessage(user, content, extra = {}) {
  // `reactions` lists the bot's emoji; `reactions.cache` (not enumerable) is discord.js's view of them, `me` and `users.remove()` included.
  const reactions = [];
  Object.defineProperty(reactions, 'cache', { value: new Map() });
  const m = {
    id: String(nextId++), guildId: null, author: user, content, channel: user.dm, attachments: new Map(), stickers: new Map(), reactions,
    async react(emoji) {
      reactions.push(emoji);
      reactions.cache.set(emoji, {
        me: true,
        users: { remove: async () => { reactions.splice(reactions.indexOf(emoji), 1); reactions.cache.delete(emoji); } },
      });
    },
    ...extra,
  };
  user.dm.byId.set(m.id, m);
  return m;
}

const bit = (name) => PermissionFlagsBits[name];

function fakeThread(id, guild, { ownerId = 'BOT' } = {}) {
  const t = {
    id, guild, guildId: guild.id, ownerId, type: ChannelType.PrivateThread, archived: false, locked: false, sent: [], denied: new Set(),
    isThread: () => true,
    isTextBased: () => true,
    permissionsFor() { return { has: (b) => !t.denied.has(b) }; },
    async send(payload) { t.sent.push(payload); return { id: String(nextId++) }; },
    async setArchived(v) { t.archived = v; return t; },
    async setLocked(v) { t.locked = v; return t; },
    async delete() { t.deleted = true; guild.channels.cache.delete(id); },
  };
  return t;
}

function fakeChannel(id, guild, { denied = [] } = {}) {
  const denySet = new Set(denied.map(bit));
  const ch = {
    id, guild, guildId: guild.id, type: ChannelType.GuildText, denied: denySet, created: [],
    createFails: { private: null, public: null },
    isThread: () => false,
    isTextBased: () => true,
    permissionsFor() { return { has: (b) => !denySet.has(b) }; },
    threads: {
      async create(options) {
        ch.created.push(options);
        const fail = options.type === ChannelType.PrivateThread ? ch.createFails.private : ch.createFails.public;
        if (fail) throw fail;
        const t = fakeThread(`T${nextId++}`, guild);
        t.type = options.type;
        t.options = options;
        guild.channels.cache.set(t.id, t);
        return t;
      },
    },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function fakeGuild(id, name, { staffChannel = 'SC' } = {}) {
  const guild = {
    id, name, available: true, ownerId: 'OWNER', iconURL: () => null,
    channels: { cache: new Map(), fetch: async (cid) => guild.channels.cache.get(cid) ?? Promise.reject(errCode(10003, 'Unknown Channel')) },
    members: { me: { id: 'BOT' }, cache: new Map(), fetch: async (uid) => guild.members.cache.get(uid) ?? Promise.reject(errCode(10007, 'Unknown Member')) },
    roles: { cache: new Map() },
  };
  guild.memberCount = 40;
  guild.channel = fakeChannel(staffChannel, guild);
  return guild;
}

function addMember(guild, user, { roles = [], perms = [], joinedTimestamp = Date.now() - 30 * 86_400_000 } = {}) {
  const permSet = new Set(perms.map(bit));
  const member = {
    id: user.id, user, guild, displayName: user.username, joinedTimestamp,
    roles: { cache: new Map(roles.map((r) => [r, { id: r }])) },
    permissions: { has: (b) => permSet.has(b) },
    displayAvatarURL: () => 'https://cdn.example/m.png',
  };
  guild.members.cache.set(user.id, member);
  return member;
}

function addRole(guild, id, mentionable = true) {
  guild.roles.cache.set(id, { id, mentionable });
}

const baseSettings = (extra = {}) => ({
  modmailEnabled: true, modmailChannelId: 'SC', modmailPingRoleIds: [], modmailGreeting: null,
  modmailAnonymousReplies: false, modmailBlockedUserIds: [], modRoleIds: [], ...extra,
});
async function setSettings(guildId, data) {
  settingsTable.set(guildId, { guildId, ...baseSettings(), ...data });
  return updateSettings(guildId, { ...baseSettings(), ...data });
}

function fakeClient(...guilds) {
  const users = new Map();
  return {
    user: { id: 'BOT' },
    guilds: { cache: new Map(guilds.map((g) => [g.id, g])) },
    users: { fetch: async (id) => users.get(id) ?? Promise.reject(errCode(10013, 'Unknown User')) },
    addUser(u) { users.set(u.id, u); return u; },
  };
}

const embedOf = (payload, i = 0) => payload.embeds[i].toJSON();

function resetRows() {
  threads.clear();
}

// ── Pure rules ────────────────────────────────────────────────────────────────

test('the default greeting is the dashboard default', () => {
  assert.equal(svc.DEFAULT_GREETING, DEFAULT_TEXTS.modmailGreeting);
});

test('isInternalNote: only a message that starts with // is a note', () => {
  assert.equal(svc.isInternalNote('// ask the owner'), true);
  assert.equal(svc.isInternalNote('   //note'), true);
  assert.equal(svc.isInternalNote('see https://x.y // fine'), false);
  assert.equal(svc.isInternalNote('hello'), false);
  assert.equal(svc.isInternalNote(''), false);
  assert.equal(svc.isInternalNote(null), false);
});

test('blocked members and acceptsConversations', () => {
  const s = baseSettings({ modmailBlockedUserIds: ['B'] });
  assert.equal(svc.isBlocked(s, 'B'), true);
  assert.equal(svc.isBlocked(s, 'A'), false);
  assert.equal(svc.isBlocked(null, 'A'), false);
  assert.equal(svc.acceptsConversations(s, 'A'), true);
  assert.equal(svc.acceptsConversations(s, 'B'), false, 'blocked');
  assert.equal(svc.acceptsConversations({ ...s, modmailEnabled: false }, 'A'), false, 'module off');
  assert.equal(svc.acceptsConversations({ ...s, modmailChannelId: null }, 'A'), false, 'no channel');
  assert.equal(svc.acceptsConversations(null, 'A'), false);
});

test('attachments and stickers become links, long text is cut, empty messages are not relayed', () => {
  const msg = {
    content: 'x'.repeat(5000),
    attachments: new Map([['1', { name: 'cat [1].png', url: 'https://cdn.example/cat.png' }]]),
    stickers: new Map([['2', { name: 'wave' }]]),
  };
  const lines = svc.attachmentLines(msg);
  assert.deepEqual(lines, ['[cat 1.png](https://cdn.example/cat.png)', '[Sticker: wave]']);
  const parts = svc.relayParts(msg);
  assert.ok(parts.description.length <= MODMAIL.TEXT_MAX);
  assert.match(parts.attachments, /cdn\.example\/cat\.png/);
  assert.equal(svc.hasContent({ content: '  ' }), false);
  assert.equal(svc.hasContent({ content: '', attachments: new Map([['1', { name: 'a', url: 'https://x/a' }]]) }), true);
  assert.equal(svc.hasContent({ content: 'hi' }), true);
});

test('threadName is dash-safe and at most 100 characters', () => {
  assert.equal(svc.threadName({ username: 'Cool Kid!' }), 'modmail-cool-kid');
  assert.equal(svc.threadName({ username: '!!!' }), 'modmail-member');
  assert.ok(svc.threadName({ username: 'a'.repeat(300) }).length <= 100);
});

test('staffAuthor shows the name, or "Staff" when anonymous', () => {
  const guild = fakeGuild('G0', 'Guild');
  const member = addMember(guild, fakeUser('S1', 'sam'));
  assert.equal(svc.staffAuthor(member, guild, false).name, 'sam');
  assert.equal(svc.staffAuthor(member, guild, true).name, 'Staff');
  assert.equal(svc.staffAuthor(null, guild, false).name, 'Staff');
});

test('isStaffAuthor: a moderator, a ping-role holder or a thread manager, nobody else', () => {
  const guild = fakeGuild('G0b', 'Guild');
  const channel = guild.channel;
  const settings = baseSettings({ modRoleIds: ['MODROLE'], modmailPingRoleIds: ['PING'] });
  const mod = addMember(guild, fakeUser('m1'), { roles: ['MODROLE'] });
  const pinged = addMember(guild, fakeUser('m2'), { roles: ['PING'] });
  const kicker = addMember(guild, fakeUser('m3'), { perms: ['KickMembers'] });
  const rando = addMember(guild, fakeUser('m4'));
  assert.equal(svc.isStaffAuthor(mod, settings, channel), true);
  assert.equal(svc.isStaffAuthor(pinged, settings, channel), true);
  assert.equal(svc.isStaffAuthor(kicker, settings, channel), true);
  assert.equal(svc.isStaffAuthor(null, settings, channel), false);
  // The fake channel grants every permission unless denied, so "can manage threads" is what lets a rando through here.
  channel.denied.add(bit('ManageThreads'));
  assert.equal(svc.isStaffAuthor(rando, settings, channel), false);
});

// ── Database rules ────────────────────────────────────────────────────────────

test('reserveOpen: one OPEN row per (guild, member), a stale placeholder is replaced', async () => {
  resetRows();
  const first = await svc.reserveOpen('GR', 'U1');
  assert.ok(first.row);
  assert.match(first.row.threadId, /^pending-/);
  const again = await svc.reserveOpen('GR', 'U1');
  assert.equal(again.existing.id, first.row.id, 'the open row is returned, not a second one');
  assert.ok((await svc.reserveOpen('GR', 'U2')).row, 'another member is independent');
  assert.ok((await svc.reserveOpen('GR2', 'U1')).row, 'another guild is independent');
  assert.equal(all({ guildId: 'GR', userId: 'U1', status: 'OPEN' }).length, 1);

  threads.get(first.row.id).createdAt = new Date(Date.now() - MODMAIL.PENDING_STALE_MS - 1000);
  const replaced = await svc.reserveOpen('GR', 'U1');
  assert.ok(replaced.row, 'the placeholder of a dead attempt is replaced');
  assert.notEqual(replaced.row.id, first.row.id);
  assert.equal(all({ guildId: 'GR', userId: 'U1', status: 'OPEN' }).length, 1);

  // A closed conversation does not block a new one.
  await svc.attachThread(replaced.row.id, 'T-real');
  assert.equal(await svc.closeRow(replaced.row.id, { closedById: 'S', reason: 'done' }), true);
  assert.ok((await svc.reserveOpen('GR', 'U1')).row);
});

test('closeRow closes once; closeByThread and findOpenForUser skip placeholders', async () => {
  resetRows();
  const { row } = await svc.reserveOpen('GC', 'U1');
  assert.deepEqual(await svc.findOpenForUser('U1'), [], 'a placeholder is not a usable conversation');
  await svc.attachThread(row.id, 'T1');
  assert.equal((await svc.findOpenForUser('U1')).length, 1);
  assert.equal((await svc.findOpenInGuild('GC', 'U1')).threadId, 'T1');
  assert.equal(await svc.closeRow(row.id, { closedById: 'S', reason: 'x'.repeat(900) }), true);
  assert.equal(await svc.closeRow(row.id), false, 'a second close does nothing');
  const closed = threads.get(row.id);
  assert.equal(closed.status, 'CLOSED');
  assert.equal(closed.closedById, 'S');
  assert.ok(closed.closeReason.length <= MODMAIL.REASON_MAX);
  assert.ok(closed.closedAt instanceof Date);
  assert.equal(await svc.closeByThread('T1'), 0, 'already closed');
  assert.equal(await svc.findInGuild('OTHER', row.id), null, 'ids never cross guilds');
});

// ── Opening a conversation ────────────────────────────────────────────────────

async function world({ roles = [], settings = {} } = {}) {
  const guild = fakeGuild(`G${nextId++}`, 'Test **Guild**');
  const user = fakeUser(`U${nextId++}`, 'Alice');
  const member = addMember(guild, user);
  for (const [id, mentionable] of roles) addRole(guild, id, mentionable);
  const s = await setSettings(guild.id, settings);
  const client = fakeClient(guild);
  client.addUser(user);
  return { guild, user, member, settings: s, client, channel: guild.channel };
}

test('opening creates a private thread, pings only the configured roles and greets the member', async () => {
  resetRows();
  const w = await world({ roles: [['R1', true], ['R2', true]], settings: { modmailPingRoleIds: ['R1'] } });
  const msg = fakeDmMessage(w.user, 'hello staff', { attachments: new Map([['1', { name: 'a.png', url: 'https://cdn.example/a.png' }]]) });
  const res = await mm.openConversation({ guild: w.guild, member: w.member, user: w.user, settings: w.settings, message: msg });

  assert.equal(res.ok, true);
  assert.equal(res.relayed, true);
  const options = w.channel.created[0];
  assert.equal(options.type, ChannelType.PrivateThread);
  assert.equal(options.invitable, false);
  assert.equal(options.name, 'modmail-alice');

  const [intro, first] = res.thread.sent;
  assert.equal(intro.content, '<@&R1>');
  assert.deepEqual(intro.allowedMentions, { parse: [], roles: ['R1'], users: [] });
  const who = embedOf(intro).description;
  assert.match(who, new RegExp(`<@${w.user.id}> \\(\`${w.user.id}\``));
  assert.match(who, /Account created/);
  assert.match(who, /Joined the server/);
  assert.match(who, /\/\//, 'tells staff how to write an internal note');
  const closeButton = intro.components[0].toJSON().components[0];
  assert.equal(closeButton.custom_id, `modmail:close:${res.row.id}`);

  // The first message is relayed with its attachment link; nothing in it pings.
  assert.equal(embedOf(first).description, 'hello staff');
  assert.match(embedOf(first).fields[0].value, /cdn\.example\/a\.png/);
  assert.deepEqual(first.allowedMentions, { parse: [] });

  // The member got the default greeting with {server} rendered.
  const greeting = embedOf(w.user.sent[0]);
  assert.match(greeting.description, /The staff of \*\*Test \*\*Guild\*\*\*\* will answer you here soon/);

  const row = threads.get(res.row.id);
  assert.equal(row.threadId, res.thread.id);
  assert.equal(row.status, 'OPEN');
  assert.equal(all({ guildId: w.guild.id, userId: w.user.id, status: 'OPEN' }).length, 1);
});

test('a custom greeting is rendered with placeholders; a blank one falls back to the default', async () => {
  const w = await world();
  const custom = mm.greetingText({ modmailGreeting: 'Hi {user.name} from {server}!' }, w);
  assert.equal(custom, 'Hi Alice from Test **Guild**!');
  assert.match(mm.greetingText({ modmailGreeting: null }, w), /will answer you here soon/);
  assert.match(mm.greetingText({ modmailGreeting: '' }, w), /will answer you here soon/);
});

test('a role that cannot be pinged is not pinged and staff are told why', async () => {
  resetRows();
  const w = await world({ roles: [['R1', false]], settings: { modmailPingRoleIds: ['R1'] } });
  w.channel.denied.add(bit('MentionEveryone'));
  const res = await mm.openConversation({ guild: w.guild, member: w.member, user: w.user, settings: w.settings, message: fakeDmMessage(w.user, 'hi') });
  assert.equal(res.ok, true);
  const intro = res.thread.sent[0];
  assert.equal(intro.content, undefined);
  assert.deepEqual(intro.allowedMentions.roles, []);
  assert.match(embedOf(intro).description, /cannot ping <@&R1>/);
});

test('the open at most once per member: a second open finds the existing conversation', async () => {
  resetRows();
  const w = await world();
  const one = await mm.openConversation({ guild: w.guild, member: w.member, user: w.user, settings: w.settings, message: fakeDmMessage(w.user, 'a') });
  const two = await mm.openConversation({ guild: w.guild, member: w.member, user: w.user, settings: w.settings, message: fakeDmMessage(w.user, 'b') });
  assert.equal(one.ok, true);
  assert.equal(two.ok, false);
  assert.equal(two.code, 'EXISTS');
  assert.equal(two.data.row.id, one.row.id);
  assert.equal(w.channel.created.length, 1, 'no second thread');
});

test('parallel opens create one thread', async () => {
  resetRows();
  const w = await world();
  const open = () => mm.openConversation({ guild: w.guild, member: w.member, user: w.user, settings: w.settings, message: fakeDmMessage(w.user, 'x') });
  const results = await Promise.all([open(), open(), open()]);
  assert.equal(results.filter((r) => r.ok).length, 1);
  assert.equal(results.filter((r) => r.code === 'EXISTS').length, 2);
  assert.equal(w.channel.created.length, 1);
});

test('private threads failing falls back to a public thread and says so', async () => {
  resetRows();
  const w = await world();
  w.channel.createFails.private = errCode(50001, 'Missing Access');
  const res = await mm.openConversation({ guild: w.guild, member: w.member, user: w.user, settings: w.settings, message: fakeDmMessage(w.user, 'hi') });
  assert.equal(res.ok, true);
  assert.deepEqual(w.channel.created.map((o) => o.type), [ChannelType.PrivateThread, ChannelType.PublicThread]);
  assert.match(embedOf(res.thread.sent[0]).description, /\*\*public\*\* thread/);
});

test('without Create Private Threads the thread is public, and says so', async () => {
  resetRows();
  const w = await world();
  w.channel.denied.add(bit('CreatePrivateThreads'));
  const res = await mm.openConversation({ guild: w.guild, member: w.member, user: w.user, settings: w.settings, message: fakeDmMessage(w.user, 'hi') });
  assert.equal(res.ok, true);
  assert.deepEqual(w.channel.created.map((o) => o.type), [ChannelType.PublicThread]);
  assert.match(embedOf(res.thread.sent[0]).description, /Create Private Threads/);
});

test('refusals: no thread permissions, deleted channel, blocked member, module off', async () => {
  resetRows();
  const w = await world();
  w.channel.denied.add(bit('CreatePrivateThreads'));
  w.channel.denied.add(bit('CreatePublicThreads'));
  const noPerms = await mm.openConversation({ guild: w.guild, member: w.member, user: w.user, settings: w.settings, message: fakeDmMessage(w.user, 'hi') });
  assert.equal(noPerms.code, 'BOT_PERMS');
  assert.equal(threads.size, 0, 'no row is left behind');

  const w2 = await world({ settings: { modmailChannelId: 'GONE' } });
  assert.equal((await mm.openConversation({ guild: w2.guild, member: w2.member, user: w2.user, settings: w2.settings, message: fakeDmMessage(w2.user, 'hi') })).code, 'NO_CHANNEL', 'a channel that is not cached is refused');

  const w3 = await world();
  w3.guild.channels.cache.delete('SC');
  assert.equal((await mm.openConversation({ guild: w3.guild, member: w3.member, user: w3.user, settings: w3.settings, message: fakeDmMessage(w3.user, 'hi') })).code, 'NO_CHANNEL');

  const w4 = await world();
  const blocked = { ...w4.settings, modmailBlockedUserIds: [w4.user.id] };
  assert.equal((await mm.openConversation({ guild: w4.guild, member: w4.member, user: w4.user, settings: blocked, message: fakeDmMessage(w4.user, 'hi') })).code, 'BLOCKED');
  const off = { ...w4.settings, modmailEnabled: false };
  assert.equal((await mm.openConversation({ guild: w4.guild, member: w4.member, user: w4.user, settings: off, message: fakeDmMessage(w4.user, 'hi') })).code, 'BLOCKED');
  assert.equal(w4.channel.created.length, 0);
});

test('a thread that cannot be created drops the reserved row; a failing intro deletes the thread', async () => {
  resetRows();
  const w = await world();
  w.channel.createFails.private = errCode(50035, 'bad');
  w.channel.createFails.public = errCode(50013, 'no');
  const failed = await mm.openConversation({ guild: w.guild, member: w.member, user: w.user, settings: w.settings, message: fakeDmMessage(w.user, 'hi') });
  assert.equal(failed.code, 'CREATE_FAILED');
  assert.equal(threads.size, 0);

  const w2 = await world();
  const original = w2.channel.threads.create;
  let made = null;
  w2.channel.threads.create = async (o) => {
    made = await original.call(w2.channel.threads, o);
    made.send = async () => { throw errCode(50013); };
    return made;
  };
  const noIntro = await mm.openConversation({ guild: w2.guild, member: w2.member, user: w2.user, settings: w2.settings, message: fakeDmMessage(w2.user, 'hi') });
  assert.equal(noIntro.code, 'CREATE_FAILED');
  assert.equal(made.deleted, true);
  assert.equal(threads.size, 0);
});

// ── Relaying ──────────────────────────────────────────────────────────────────

async function openWorld(opts) {
  const w = await world(opts);
  const first = fakeDmMessage(w.user, 'first');
  const res = await mm.openConversation({ guild: w.guild, member: w.member, user: w.user, settings: w.settings, message: first });
  assert.equal(res.ok, true);
  return { ...w, row: threads.get(res.row.id), thread: res.thread };
}

test('member DMs go to the thread; a ✅ marks delivery', async () => {
  resetRows();
  const w = await openWorld();
  const before = w.thread.sent.length;
  const dmBefore = w.user.dm.sent.length;
  const dm = fakeDmMessage(w.user, 'more info');
  await mm.handleDirectMessage({ client: w.client, message: dm });
  assert.equal(w.thread.sent.length, before + 1);
  const post = w.thread.sent.at(-1);
  assert.equal(embedOf(post).description, 'more info');
  assert.equal(embedOf(post).author.name, 'Alice');
  assert.deepEqual(post.allowedMentions, { parse: [] });
  assert.deepEqual(dm.reactions, [MODMAIL.OK_EMOJI]);
  assert.equal(w.user.dm.sent.length, dmBefore, 'no prompt while a conversation is open');
});

test('a failed delivery gets ❌, an archived thread is reopened', async () => {
  resetRows();
  const w = await openWorld();
  w.thread.archived = true;
  const ok = fakeDmMessage(w.user, 'wake up');
  await mm.handleDirectMessage({ client: w.client, message: ok });
  assert.equal(w.thread.archived, false);
  assert.deepEqual(ok.reactions, [MODMAIL.OK_EMOJI]);

  const real = w.thread.send;
  w.thread.send = async () => { throw errCode(50083, 'Thread is archived'); };
  const bad = fakeDmMessage(w.user, 'nope');
  await mm.handleDirectMessage({ client: w.client, message: bad });
  assert.deepEqual(bad.reactions, [MODMAIL.FAIL_EMOJI]);
  assert.equal(threads.get(w.row.id).status, 'OPEN', 'a temporary failure keeps the conversation');
  w.thread.send = real;
});

test('a deleted thread closes the conversation and the member is asked again', async () => {
  resetRows();
  const w = await openWorld();
  w.guild.channels.cache.delete(w.thread.id);
  const dm = fakeDmMessage(w.user, 'anyone?');
  await mm.handleDirectMessage({ client: w.client, message: dm });
  assert.equal(threads.get(w.row.id).status, 'CLOSED');
  const prompt = w.user.dm.sent.at(-1);
  assert.match(embedOf(prompt).title, /Contact Staff/);
});

test('staff messages reach the member, with name or anonymous; ✅ on delivery', async () => {
  resetRows();
  const w = await openWorld({ roles: [], settings: { modRoleIds: ['MODROLE'] } });
  const staffUser = fakeUser('S1', 'Sam');
  const staff = addMember(w.guild, staffUser, { roles: ['MODROLE'] });
  const reactions = [];
  const staffMessage = (content, extra = {}) => ({
    id: String(nextId++), author: staffUser, member: staff, content, channel: w.thread, guildId: w.guild.id, attachments: new Map(), stickers: new Map(),
    react: async (e) => { reactions.push(e); }, ...extra,
  });
  const ctxFor = (message, settings = w.settings) => ({ client: w.client, guild: w.guild, guildId: w.guild.id, message, getSettings: async () => settings });
  const sentBefore = w.user.sent.length;

  await mm.handleStaffMessage(ctxFor(staffMessage('We are looking into it', { attachments: new Map([['1', { name: 'log.txt', url: 'https://cdn.example/log.txt' }]]) })));
  const dm = w.user.sent[sentBefore];
  assert.equal(embedOf(dm).description, 'We are looking into it');
  assert.equal(embedOf(dm).author.name, 'Sam');
  assert.match(embedOf(dm).fields[0].value, /log\.txt/);
  assert.deepEqual(dm.allowedMentions, { parse: [] });
  assert.deepEqual(reactions, [MODMAIL.OK_EMOJI]);

  // Anonymous replies show "Staff".
  await mm.handleStaffMessage(ctxFor(staffMessage('anon please'), { ...w.settings, modmailAnonymousReplies: true }));
  assert.equal(embedOf(w.user.sent.at(-1)).author.name, 'Staff');

  // Internal notes, empty messages and bot messages stay in the thread.
  const count = w.user.sent.length;
  await mm.handleStaffMessage(ctxFor(staffMessage('// do not tell them')));
  await mm.handleStaffMessage(ctxFor(staffMessage('')));
  await mm.handleStaffMessage(ctxFor(staffMessage('bot says hi', { author: { id: 'BOTX', bot: true } })));
  assert.equal(w.user.sent.length, count);
  assert.equal(mm.staffMessageFilter({ client: w.client, message: staffMessage('// note') }), false, 'the filter skips notes without a query');
});

test('staff relay: ❌ when the DM fails, only staff may speak, closed threads stay quiet', async () => {
  resetRows();
  const w = await openWorld({ settings: { modRoleIds: ['MODROLE'] } });
  const staffUser = fakeUser('S2', 'Stan');
  const staff = addMember(w.guild, staffUser, { roles: ['MODROLE'] });
  const rando = addMember(w.guild, fakeUser('X1', 'Rando'));
  w.thread.denied.add(bit('ManageThreads'));
  const reactions = [];
  const msg = (author, member, content) => ({
    id: String(nextId++), author, member, content, channel: w.thread, guildId: w.guild.id, attachments: new Map(), stickers: new Map(),
    react: async (e) => { reactions.push(e); },
  });
  const ctxFor = (message) => ({ client: w.client, guild: w.guild, guildId: w.guild.id, message, getSettings: async () => w.settings });

  // A non-staff member typing in the (public fallback) thread does not speak for staff.
  const sent = w.user.sent.length;
  await mm.handleStaffMessage(ctxFor(msg(rando.user, rando, 'I am staff, trust me')));
  assert.equal(w.user.sent.length, sent);
  assert.deepEqual(reactions, []);

  // DMs closed: ❌ and a note in the thread.
  w.user.dmClosed = true;
  const threadSent = w.thread.sent.length;
  await mm.handleStaffMessage(ctxFor(msg(staffUser, staff, 'hello?')));
  assert.deepEqual(reactions, [MODMAIL.FAIL_EMOJI]);
  assert.match(embedOf(w.thread.sent[threadSent]).description, /DMs are closed/);
  w.user.dmClosed = false;

  // A closed conversation is not relayed.
  await svc.closeRow(w.row.id);
  reactions.length = 0;
  await mm.handleStaffMessage(ctxFor(msg(staffUser, staff, 'too late')));
  assert.deepEqual(reactions, []);
});

test('a thread that is not a modmail thread is remembered and skipped', async () => {
  const guild = fakeGuild('GN', 'G');
  const stranger = fakeThread('TX-other', guild);
  const client = fakeClient(guild);
  const message = { id: '1', author: { id: 'A', bot: false }, content: 'hello', channel: stranger };
  const ctx = { client, guild, guildId: 'GN', message, getSettings: async () => baseSettings() };
  assert.equal(mm.staffMessageFilter(ctx), true);
  await mm.handleStaffMessage(ctx);
  assert.equal(mm.staffMessageFilter(ctx), false, 'known not to be modmail');
  // Threads owned by somebody else are never modmail threads (they are created by the bot).
  assert.equal(mm.staffMessageFilter({ ...ctx, message: { ...message, channel: fakeThread('TX-2', guild, { ownerId: 'SOMEONE' }) } }), false);
  assert.equal(mm.staffMessageFilter({ ...ctx, message: { ...message, channel: guild.channel } }), false, 'not a thread');
});

// ── The DM flow ───────────────────────────────────────────────────────────────

function dmWorld(count, { member = true } = {}) {
  const guilds = [];
  for (let i = 0; i < count; i++) guilds.push(fakeGuild(`GD${nextId++}`, `Guild ${String.fromCharCode(65 + i)}`));
  const user = fakeUser(`UD${nextId++}`, 'Dana');
  const client = fakeClient(...guilds);
  client.addUser(user);
  return { guilds, user, client, member };
}

test('no shared server with modmail: a short notice, at most once in a while', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(2);
  await setSettings(guilds[0].id, { modmailEnabled: false });   // module off
  await setSettings(guilds[1].id, {});                          // on, but the member is not in it
  const msg = fakeDmMessage(user, 'hello?');
  await mm.handleDirectMessage({ client, message: msg });
  assert.equal(user.dm.sent.length, 1);
  assert.match(embedOf(user.dm.sent[0]).title, /No Server Found/);
  const again = fakeDmMessage(user, 'hello again?');
  await mm.handleDirectMessage({ client, message: again });
  assert.equal(user.dm.sent.length, 1, 'throttled');
  assert.deepEqual(again.reactions, [], 'no ⏳: there is no prompt it could wait for');
});

test('one server: asks first with a button, and nothing is opened yet', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(2);
  await setSettings(guilds[0].id, {});
  await setSettings(guilds[1].id, { modmailEnabled: false });
  addMember(guilds[0], user);
  addMember(guilds[1], user);
  const msg = fakeDmMessage(user, 'I need help');
  await mm.handleDirectMessage({ client, message: msg });
  const prompt = user.dm.sent[0];
  assert.match(embedOf(prompt).description, /Guild A/);
  const [open, cancel] = prompt.components[0].toJSON().components;
  assert.equal(open.custom_id, `modmail:open:${guilds[0].id}:${msg.id}`);
  assert.equal(cancel.custom_id, 'modmail:cancel');
  assert.equal(threads.size, 0);
  assert.equal(guilds[0].channel.created.length, 0);
});

test('several servers: a select menu of at most 25, a blocked server is not offered', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(28);
  for (const g of guilds) {
    await setSettings(g.id, {});
    addMember(g, user);
  }
  await setSettings(guilds[0].id, { modmailBlockedUserIds: [user.id] });
  const msg = fakeDmMessage(user, 'help');
  await mm.handleDirectMessage({ client, message: msg });
  const menu = user.dm.sent[0].components[0].toJSON().components[0];
  assert.equal(menu.custom_id, `modmail-select:open:${msg.id}`);
  assert.equal(menu.options.length, MODMAIL.PICK_MAX);
  assert.ok(!menu.options.some((o) => o.value === guilds[0].id), 'the blocked server is not offered');
  assert.match(embedOf(user.dm.sent[0]).description, /Showing the first 25 of 27/);
});

test('the open button opens the conversation and relays the message; a double click acts once', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(1);
  const g = guilds[0];
  await setSettings(g.id, { modmailPingRoleIds: [] });
  addMember(g, user);
  const msg = fakeDmMessage(user, 'please help');
  await mm.handleDirectMessage({ client, message: msg });

  const edits = [];
  const click = () => ({
    customId: `modmail:open:${g.id}:${msg.id}`, user, channel: user.dm, message: { id: 'PROMPT1' }, deferred: false, replied: false,
    inGuild: () => false, inCachedGuild: () => false,
    async deferUpdate() { this.deferred = true; },
    async editReply(p) { edits.push(p); },
  });
  await mm.handleButton(click(), client);
  await mm.handleButton(click(), client);
  assert.equal(g.channel.created.length, 1, 'one thread for two clicks');
  assert.equal(edits.length, 1);
  assert.match(embedOf(edits[0]).title, /Conversation Opened/);
  assert.deepEqual(edits[0].components, []);
  const row = all({ guildId: g.id, userId: user.id })[0];
  assert.ok(row);
  const thread = g.channels.cache.get(row.threadId);
  assert.equal(embedOf(thread.sent.at(-1)).description, 'please help', 'the original message is relayed');
  assert.deepEqual(msg.reactions, [MODMAIL.OK_EMOJI]);
});

/** An interaction answering a DM prompt (a button press or, with `values`, a menu pick). */
function answerPrompt(user, customId, extra = {}) {
  const edits = [];
  const interaction = {
    customId, user, channel: user.dm, message: { id: `PROMPT-${nextId++}` }, deferred: false, replied: false,
    inGuild: () => false, async deferUpdate() { this.deferred = true; }, async editReply(p) { edits.push(p); }, ...extra,
  };
  return { interaction, edits };
}
const threadOf = (guild, userId) => guild.channels.cache.get(all({ guildId: guild.id, userId })[0].threadId);

test('DMs sent while the prompt is open get ⏳ and all reach staff when the member answers', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(1);
  const g = guilds[0];
  await setSettings(g.id, {});
  addMember(g, user);
  const first = fakeDmMessage(user, 'hi');
  const second = fakeDmMessage(user, 'I was scammed by X');
  const shot = fakeDmMessage(user, '', { attachments: new Map([['1', { name: 'proof.png', url: 'https://cdn.example/proof.png' }]]) });
  for (const message of [first, second, shot]) await mm.handleDirectMessage({ client, message });
  assert.equal(user.dm.sent.length, 1, 'one prompt, not one per message');
  assert.deepEqual(first.reactions, [], 'the prompt itself is the answer to the first message');
  assert.deepEqual(second.reactions, [MODMAIL.WAIT_EMOJI]);
  assert.deepEqual(shot.reactions, [MODMAIL.WAIT_EMOJI]);
  assert.equal(threads.size, 0, 'nothing is sent before the member agrees');

  const { interaction, edits } = answerPrompt(user, `modmail:open:${g.id}:${first.id}`);
  await mm.handleButton(interaction, client);
  const sent = threadOf(g, user.id).sent;
  assert.equal(sent.length, 4, 'intro, hi, the second message, the screenshot');
  assert.equal(embedOf(sent[1]).description, 'hi');
  assert.equal(embedOf(sent[2]).description, 'I was scammed by X');
  assert.match(embedOf(sent[3]).fields[0].value, /proof\.png/);
  for (const message of [first, second, shot]) assert.deepEqual(message.reactions, [MODMAIL.OK_EMOJI], 'delivered, and the ⏳ is gone');
  assert.match(embedOf(edits[0]).description, /Your 3 messages were sent/);
});

test('DMs that waited reach the conversation chosen from the menu when several are open', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(2);
  const rows = [];
  for (const g of guilds) {
    await setSettings(g.id, {});
    const member = addMember(g, user);
    const res = await mm.openConversation({ guild: g, member, user, settings: peekSettings(g.id), message: fakeDmMessage(user, `hello ${g.id}`) });
    rows.push({ row: threads.get(res.row.id), thread: res.thread });
  }
  const first = fakeDmMessage(user, 'which one?');
  const second = fakeDmMessage(user, 'and one more thing');
  const dmBefore = user.dm.sent.length;
  await mm.handleDirectMessage({ client, message: first });
  await mm.handleDirectMessage({ client, message: second });
  assert.equal(user.dm.sent.length, dmBefore + 1, 'one menu');
  assert.deepEqual(second.reactions, [MODMAIL.WAIT_EMOJI]);

  const pick = rows[0];
  const before = pick.thread.sent.length;
  const { interaction, edits } = answerPrompt(user, `modmail-select:relay:${first.id}`, { values: [pick.row.id] });
  await mm.handleSelect(interaction, client);
  assert.deepEqual(pick.thread.sent.slice(before).map((p) => embedOf(p).description), ['which one?', 'and one more thing']);
  assert.equal(rows[1].thread.sent.length, 2, 'the other conversation heard nothing');
  assert.deepEqual(first.reactions, [MODMAIL.OK_EMOJI]);
  assert.deepEqual(second.reactions, [MODMAIL.OK_EMOJI]);
  assert.match(embedOf(edits[0]).description, /Your 2 messages were sent/);
});

test('a conversation that opened meanwhile still gets the first message and the ones that waited', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(1);
  const g = guilds[0];
  await setSettings(g.id, {});
  const member = addMember(g, user);
  const first = fakeDmMessage(user, 'hi');
  const second = fakeDmMessage(user, 'details');
  await mm.handleDirectMessage({ client, message: first });
  await mm.handleDirectMessage({ client, message: second });
  const opened = await mm.openConversation({ guild: g, member, user, settings: peekSettings(g.id), message: fakeDmMessage(user, 'opened elsewhere') });
  assert.equal(opened.ok, true);

  const { interaction, edits } = answerPrompt(user, `modmail:open:${g.id}:${first.id}`);
  await mm.handleButton(interaction, client);
  assert.deepEqual(opened.thread.sent.slice(2).map((p) => embedOf(p).description), ['hi', 'details']);
  assert.deepEqual(second.reactions, [MODMAIL.OK_EMOJI]);
  assert.match(embedOf(edits[0]).title, /Message Sent/);
});

test('only the DMs marked ⏳ in a row after the prompt are sent: not refused ones, not ones a newer prompt owns', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(1);
  const g = guilds[0];
  await setSettings(g.id, {});
  addMember(g, user);
  const first = fakeDmMessage(user, 'hi');
  await mm.handleDirectMessage({ client, message: first });
  const waiting = fakeDmMessage(user, 'waiting');
  await mm.handleDirectMessage({ client, message: waiting });
  const stranger = fakeUser('OTHER', 'Eve');
  stranger.dm = user.dm;                                    // the same channel, somebody else's message
  const foreign = fakeDmMessage(stranger, 'not mine');
  await foreign.react(MODMAIL.WAIT_EMOJI);
  const refused = fakeDmMessage(user, 'flood');
  await refused.react(MODMAIL.FAIL_EMOJI);                 // the rate limit's answer
  const newer = fakeDmMessage(user, 'a newer prompt asked about this one');
  const behindNewer = fakeDmMessage(user, 'waits for the newer prompt');
  await behindNewer.react(MODMAIL.WAIT_EMOJI);

  const { interaction } = answerPrompt(user, `modmail:open:${g.id}:${first.id}`);
  await mm.handleButton(interaction, client);
  assert.deepEqual(threadOf(g, user.id).sent.slice(1).map((p) => embedOf(p).description), ['hi', 'waiting']);
  assert.deepEqual(refused.reactions, [MODMAIL.FAIL_EMOJI]);
  assert.deepEqual(newer.reactions, []);
  assert.deepEqual(behindNewer.reactions, [MODMAIL.WAIT_EMOJI]);
  assert.deepEqual(foreign.reactions, [MODMAIL.WAIT_EMOJI]);
});

test('answering a prompt sends at most CATCH_UP_MAX waiting DMs after the first, in order', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(1);
  const g = guilds[0];
  await setSettings(g.id, {});
  addMember(g, user);
  const first = fakeDmMessage(user, 'm0');
  await mm.handleDirectMessage({ client, message: first });
  const waiting = [];
  for (let i = 1; i <= MODMAIL.CATCH_UP_MAX + 2; i++) {
    const m = fakeDmMessage(user, `m${i}`);
    await m.react(MODMAIL.WAIT_EMOJI);
    waiting.push(m);
  }
  const { interaction } = answerPrompt(user, `modmail:open:${g.id}:${first.id}`);
  await mm.handleButton(interaction, client);
  const texts = threadOf(g, user.id).sent.slice(1).map((p) => embedOf(p).description);
  assert.deepEqual(texts, Array.from({ length: MODMAIL.CATCH_UP_MAX + 1 }, (_, i) => `m${i}`));
  assert.deepEqual(waiting.at(-1).reactions, [MODMAIL.WAIT_EMOJI], 'the rest keep waiting');
});

test('a failing look-up of the waiting DMs still sends the message the prompt was about', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(1);
  const g = guilds[0];
  await setSettings(g.id, {});
  addMember(g, user);
  const first = fakeDmMessage(user, 'hi');
  await mm.handleDirectMessage({ client, message: first });
  const realFetch = user.dm.messages.fetch;
  user.dm.messages.fetch = async (arg) => (typeof arg === 'object' ? Promise.reject(errCode(50013)) : realFetch(arg));
  const { interaction, edits } = answerPrompt(user, `modmail:open:${g.id}:${first.id}`);
  await mm.handleButton(interaction, client);
  assert.deepEqual(threadOf(g, user.id).sent.slice(1).map((p) => embedOf(p).description), ['hi']);
  assert.match(embedOf(edits[0]).title, /Conversation Opened/);
});

test('the open button re-checks the server (blocked in the meantime) and the original message', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(1);
  const g = guilds[0];
  await setSettings(g.id, {});
  addMember(g, user);
  const msg = fakeDmMessage(user, 'x');
  await setSettings(g.id, { modmailBlockedUserIds: [user.id] });
  const edits = [];
  const click = (messageId, id) => ({
    customId: `modmail:open:${g.id}:${messageId}`, user, channel: user.dm, message: { id }, deferred: false, replied: false,
    inGuild: () => false, async deferUpdate() { this.deferred = true; }, async editReply(p) { edits.push(p); },
  });
  await mm.handleButton(click(msg.id, 'P-blocked'), client);
  assert.match(embedOf(edits[0]).title, /Not Available/);
  assert.equal(threads.size, 0);

  await setSettings(g.id, {});
  await mm.handleButton(click('999999', 'P-gone'), client);
  assert.match(embedOf(edits[1]).title, /Message Not Found/);
  assert.equal(threads.size, 0, 'nothing opens without the message');
});

test('cancel opens nothing', async () => {
  const updates = [];
  const interaction = { customId: 'modmail:cancel', inGuild: () => false, update: async (p) => { updates.push(p); } };
  await mm.handleButton(interaction, fakeClient());
  assert.match(embedOf(updates[0]).title, /Cancelled/);
  assert.deepEqual(updates[0].components, []);
});

test('the server menu opens in the chosen server', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(2);
  for (const g of guilds) {
    await setSettings(g.id, {});
    addMember(g, user);
  }
  const msg = fakeDmMessage(user, 'billing question');
  const edits = [];
  await mm.handleSelect({
    customId: `modmail-select:open:${msg.id}`, values: [guilds[1].id], user, channel: user.dm, message: { id: 'PROMPT2' }, deferred: false, replied: false,
    inGuild: () => false, async deferUpdate() { this.deferred = true; }, async editReply(p) { edits.push(p); },
  }, client);
  assert.equal(guilds[0].channel.created.length, 0);
  assert.equal(guilds[1].channel.created.length, 1);
  assert.match(embedOf(edits[0]).title, /Conversation Opened/);
});

test('several open conversations: the member picks which one a message is for', async () => {
  resetRows();
  const { guilds, user, client } = dmWorld(2);
  const rows = [];
  for (const g of guilds) {
    await setSettings(g.id, {});
    const member = addMember(g, user);
    const res = await mm.openConversation({ guild: g, member, user, settings: peekSettings(g.id), message: fakeDmMessage(user, `hello ${g.id}`) });
    assert.equal(res.ok, true);
    rows.push({ row: threads.get(res.row.id), thread: res.thread });
  }
  const msg = fakeDmMessage(user, 'which one?');
  await mm.handleDirectMessage({ client, message: msg });
  const menu = user.dm.sent.at(-1).components[0].toJSON().components[0];
  assert.equal(menu.custom_id, `modmail-select:relay:${msg.id}`);
  assert.deepEqual(menu.options.map((o) => o.value).sort(), rows.map((r) => r.row.id).sort());
  assert.deepEqual(msg.reactions, [], 'nothing is relayed before the member chooses');

  const pick = rows[1];
  const sentBefore = pick.thread.sent.length;
  const edits = [];
  await mm.handleSelect({
    customId: `modmail-select:relay:${msg.id}`, values: [pick.row.id], user, channel: user.dm, message: { id: 'PROMPT3' }, deferred: false, replied: false,
    inGuild: () => false, async deferUpdate() { this.deferred = true; }, async editReply(p) { edits.push(p); },
  }, client);
  assert.equal(pick.thread.sent.length, sentBefore + 1);
  assert.equal(embedOf(pick.thread.sent.at(-1)).description, 'which one?');
  assert.equal(rows[0].thread.sent.length, 2, 'the other conversation heard nothing new (intro and first message)');
  assert.match(embedOf(edits[0]).title, /Message Sent/);
});

test('a member cannot pick someone else\'s conversation', async () => {
  resetRows();
  const w = await openWorld();
  const intruder = fakeUser('INTRUDER', 'Mallory');
  const msg = fakeDmMessage(intruder, 'hijack');
  const edits = [];
  await mm.handleSelect({
    customId: `modmail-select:relay:${msg.id}`, values: [w.row.id], user: intruder, channel: intruder.dm, message: { id: 'PROMPT4' }, deferred: false, replied: false,
    inGuild: () => false, async deferUpdate() { this.deferred = true; }, async editReply(p) { edits.push(p); },
  }, w.client);
  assert.match(embedOf(edits[0]).title, /Conversation Closed/);
  assert.equal(w.thread.sent.length, 2, 'only the intro and the first message');
});

test('a member who floods is cut off with ❌', async () => {
  resetRows();
  const w = await openWorld();
  const flooder = fakeUser('FLOOD', 'Flood');
  let last = null;
  for (let i = 0; i < MODMAIL.RATE_MAX + 2; i++) {
    last = fakeDmMessage(flooder, `spam ${i}`);
    await mm.handleDirectMessage({ client: w.client, message: last });
  }
  assert.deepEqual(last.reactions, [MODMAIL.FAIL_EMOJI]);
  assert.equal(mm.withinRate('someone-new'), true);
});

test('bot messages and empty messages never start anything', async () => {
  resetRows();
  const w = await openWorld();
  const sent = w.thread.sent.length;
  await mm.handleDirectMessage({ client: w.client, message: fakeDmMessage({ ...w.user, bot: true }, 'beep') });
  await mm.handleDirectMessage({ client: w.client, message: fakeDmMessage(w.user, '   ') });
  assert.equal(w.thread.sent.length, sent);
});

// ── Closing ───────────────────────────────────────────────────────────────────

test('closing: row closed once, member told, thread locked and archived', async () => {
  resetRows();
  const w = await openWorld();
  const res = await mm.closeConversation({ client: w.client, guild: w.guild, row: w.row, closedById: 'S1', reason: 'Resolved' });
  assert.equal(res.closed, true);
  assert.equal(res.notified, true);
  const dm = embedOf(w.user.sent.at(-1));
  assert.match(dm.description, /has ended/);
  assert.match(dm.description, /\*\*Reason:\*\* Resolved/);
  assert.equal(w.thread.locked, true);
  assert.equal(w.thread.archived, true);
  assert.match(embedOf(w.thread.sent.at(-1)).description, /closed by <@S1>/);
  assert.equal(threads.get(w.row.id).status, 'CLOSED');
  assert.equal((await mm.closeConversation({ client: w.client, guild: w.guild, row: w.row })).closed, false, 'already closed');
});

test('archiveThread only archives without Manage Threads and says so', async () => {
  const guild = fakeGuild('GA', 'G');
  const thread = fakeThread('TA', guild);
  thread.denied.add(bit('ManageThreads'));
  const note = await mm.archiveThread(thread);
  assert.equal(thread.locked, false);
  assert.equal(thread.archived, true);
  assert.match(note, /Manage Threads/);
});

test('the close button: moderators only, closes once', async () => {
  resetRows();
  const w = await openWorld({ settings: { modRoleIds: ['MODROLE'] } });
  const modUser = fakeUser('S3', 'Mia');
  const mod = addMember(w.guild, modUser, { roles: ['MODROLE'] });
  const plainUser = fakeUser('S4', 'Pat');
  const plain = addMember(w.guild, plainUser);
  const calls = [];
  const press = (member, user) => ({
    customId: `modmail:close:${w.row.id}`, user, member, guild: w.guild, guildId: w.guild.id, channel: w.thread, channelId: w.thread.id, createdTimestamp: Date.now(),
    deferred: false, replied: false,
    inGuild: () => true, inCachedGuild: () => true,
    async deferUpdate() { this.deferred = true; calls.push('defer'); },
    async editReply(p) { calls.push(['edit', p]); },
    async reply(p) { this.replied = true; calls.push(['reply', p]); return { resource: null }; },
    async followUp(p) { calls.push(['follow', p]); },
  });

  await mm.handleButton(press(plain, plainUser), w.client);
  assert.equal(threads.get(w.row.id).status, 'OPEN', 'a plain member cannot close');
  assert.ok(calls.some((c) => c[0] === 'reply'));

  calls.length = 0;
  await mm.handleButton(press(mod, modUser), w.client);
  assert.equal(threads.get(w.row.id).status, 'CLOSED');
  assert.equal(threads.get(w.row.id).closedById, modUser.id);
  assert.equal(w.thread.archived, true);
  const follow = calls.find((c) => c[0] === 'follow');
  assert.match(embedOf(follow[1]).description, /Closed by <@S3>/);

  calls.length = 0;
  await mm.handleButton(press(mod, modUser), w.client);
  assert.ok(calls.some((c) => c[0] === 'follow' && /already closed/.test(embedOf(c[1]).description)));
});

// ── Cleanup hooks ─────────────────────────────────────────────────────────────

test('threadDelete closes the conversation of the deleted thread', async () => {
  resetRows();
  const w = await openWorld();
  await mm.onThreadDeleted({ guildId: w.guild.id, thread: { id: w.thread.id } });
  assert.equal(threads.get(w.row.id).status, 'CLOSED');
  assert.match(threads.get(w.row.id).closeReason, /deleted/);
  await mm.onThreadDeleted({ guildId: w.guild.id, thread: { id: 'unknown' } });
});

test('channelDelete clears the staff channel and closes the conversations whose thread is gone', async () => {
  resetRows();
  const w = await openWorld();
  const stays = fakeThread('T-old-channel', w.guild);
  w.guild.channels.cache.set(stays.id, stays);
  const other = await svc.reserveOpen(w.guild.id, 'OTHER');
  await svc.attachThread(other.row.id, stays.id);

  // Another channel is deleted: nothing happens.
  await mm.onChannelDeleted({ guildId: w.guild.id, guild: w.guild, channel: { id: 'UNRELATED' }, getSettings: async () => peekSettings(w.guild.id) });
  assert.equal(peekSettings(w.guild.id).modmailChannelId, 'SC');

  // The staff channel and its thread go away.
  w.guild.channels.cache.delete('SC');
  w.guild.channels.cache.delete(w.thread.id);
  await mm.onChannelDeleted({ guildId: w.guild.id, guild: w.guild, channel: { id: 'SC' }, getSettings: async () => peekSettings(w.guild.id) });
  assert.equal(peekSettings(w.guild.id).modmailChannelId, null);
  assert.equal(threads.get(w.row.id).status, 'CLOSED');
  assert.equal(threads.get(other.row.id).status, 'OPEN', 'a thread that still exists keeps going');
});

// ── Declarations ──────────────────────────────────────────────────────────────

test('plugin declaration, hooks and components', async () => {
  const { plugin } = mm;
  assert.equal(plugin.id, 'modmail');
  assert.equal(plugin.module, 'modmail');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id]), [
    ['directMessage', 'modmail'], ['messageCreate', 'modmail'], ['channelDelete', 'modmail'], ['threadDelete', 'modmail'],
  ]);
  for (const h of plugin.hooks) assert.ok(HOOK_ORDER[h.event].includes(h.id), `${h.event} lists modmail`);
  const hook = (event) => plugin.hooks.find((h) => h.event === event);
  assert.equal(hook('directMessage').always, true);
  assert.equal(hook('directMessage').module, null, 'a DM has no guild to gate on');
  assert.equal(hook('messageCreate').always, undefined, 'the staff relay is module-gated');
  assert.equal(hook('channelDelete').always, true);
  assert.equal(hook('threadDelete').always, true);
  for (const h of plugin.hooks) assert.ok(!h.filter || h.filter.constructor.name !== 'AsyncFunction', 'filters are synchronous');
  assert.equal(HOOK_ORDER.messageCreate.indexOf('modmail'), HOOK_ORDER.messageCreate.indexOf('autotime') + 1);

  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix, c.module]), [['button', 'modmail', null], ['select', 'modmail-select', null]]);
  for (const c of plugin.components) {
    assert.equal(CUSTOM_ID_PREFIXES[c.prefix].owner, 'modmail');
    assert.equal(CUSTOM_ID_PREFIXES[c.prefix].kind, 'persistent');
  }
  assert.deepEqual(plugin.jobs, []);

  const dm = hook('directMessage');
  assert.equal(dm.filter({ message: { guildId: null, author: { bot: false } } }), true);
  assert.equal(dm.filter({ message: { guildId: 'G', author: { bot: false } } }), false);
  assert.equal(dm.filter({ message: { guildId: null, author: { bot: true } } }), false);

  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'modmail'), 'collectPlugins keeps it');
  assert.deepEqual(errors.filter((e) => String(e?.message ?? e).includes('modmail')), []);
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/modmail payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'modmail');
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ModerateMembers));
  assert.deepEqual(json.contexts, [0]);
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['setup', 1], ['reply', 1], ['close', 1], ['block', 1], ['unblock', 1], ['blocked', 1]]);
  const sub = Object.fromEntries(json.options.map((o) => [o.name, o]));
  assert.deepEqual(sub.setup.options.map((o) => [o.name, o.type, !!o.required]), [['channel', 7, true], ['ping_role', 8, false]]);
  assert.deepEqual(sub.setup.options[0].channel_types, [ChannelType.GuildText]);
  assert.deepEqual(sub.reply.options.map((o) => [o.name, o.type, !!o.required]), [['text', 3, true], ['anonymous', 5, false]]);
  assert.deepEqual(sub.close.options.map((o) => [o.name, o.type, !!o.required]), [['reason', 3, false]]);
  assert.deepEqual(sub.block.options.map((o) => [o.name, o.type, !!o.required]), [['user', 6, true], ['reason', 3, false]]);
  assert.deepEqual(sub.unblock.options.map((o) => [o.name, o.type, !!o.required]), [['user', 6, true]]);
  assert.deepEqual(sub.blocked.options ?? [], []);
  assert.ok(charCount(json) <= 1500, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'modmail');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.MODERATOR);
  assert.deepEqual(cmd.subcommandPermissions, { setup: PERMISSION_LEVELS.ADMIN });
});

// ── /modmail subcommands ──────────────────────────────────────────────────────

function fakeCommand(w, sub, options = {}, { channel = w.channel, member = null, user = null } = {}) {
  const me = user ?? fakeUser('MOD1', 'Mod');
  return {
    guildId: w.guild.id, guild: w.guild, channel, channelId: channel.id, member: member ?? addMember(w.guild, me, { perms: ['ModerateMembers'] }), user: me,
    deferred: false, replied: false, replies: [],
    options: {
      getSubcommand: () => sub,
      getChannel: (n) => options[n] ?? null,
      getRole: (n) => options[n] ?? null,
      getString: (n) => options[n] ?? null,
      getBoolean: (n) => options[n] ?? null,
      getUser: (n) => options[n] ?? null,
    },
    async deferReply(o) { this.deferred = true; this.ephemeral = Boolean(o?.ephemeral); },
    async editReply(p) { this.replies.push(p); return {}; },
    async followUp(p) { this.replies.push(p); return {}; },
    async reply(p) { this.replied = true; this.replies.push(p); return {}; },
  };
}
const lastEmbed = (i) => embedOf(i.replies.at(-1));

test('/modmail setup: validates the channel and role, saves, turns the module on', async () => {
  resetRows();
  const w = await world({ settings: { modmailEnabled: false, modmailChannelId: null } });
  addRole(w.guild, 'PING', true);
  addRole(w.guild, 'QUIET', false);

  const bad = fakeCommand(w, 'setup', { channel: { id: 'NOPE' } });
  await cmd.execute(bad, w.client);
  assert.match(lastEmbed(bad).title, /Cannot Use That Channel/);

  w.channel.denied.add(bit('EmbedLinks'));
  const noEmbeds = fakeCommand(w, 'setup', { channel: { id: 'SC' } });
  await cmd.execute(noEmbeds, w.client);
  assert.match(lastEmbed(noEmbeds).description, /Embed Links/);
  assert.equal(peekSettings(w.guild.id).modmailEnabled, false, 'nothing saved');
  w.channel.denied.delete(bit('EmbedLinks'));

  // The admin cannot ping a role that is not mentionable (and cannot mention all roles).
  w.channel.denied.add(bit('MentionEveryone'));
  const quiet = fakeCommand(w, 'setup', { channel: { id: 'SC' }, ping_role: { id: 'QUIET' } });
  await cmd.execute(quiet, w.client);
  assert.match(lastEmbed(quiet).title, /Cannot Ping That Role/);
  const everyone = fakeCommand(w, 'setup', { channel: { id: 'SC' }, ping_role: { id: w.guild.id } });
  await cmd.execute(everyone, w.client);
  assert.match(lastEmbed(everyone).description, /@everyone/);

  w.channel.denied.add(bit('ManageThreads'));
  const ok = fakeCommand(w, 'setup', { channel: { id: 'SC' }, ping_role: { id: 'PING' } });
  await cmd.execute(ok, w.client);
  assert.match(lastEmbed(ok).title, /Modmail Set Up/);
  assert.match(lastEmbed(ok).description, /Manage Threads/, 'a warning, not a refusal');
  assert.deepEqual(ok.replies.at(-1).allowedMentions, { parse: [] });
  const saved = peekSettings(w.guild.id);
  assert.equal(saved.modmailEnabled, true);
  assert.equal(saved.modmailChannelId, 'SC');
  assert.deepEqual([...saved.modmailPingRoleIds], ['PING']);

  // Without ping_role the current roles stay.
  const keep = fakeCommand(w, 'setup', { channel: { id: 'SC' } });
  await cmd.execute(keep, w.client);
  assert.deepEqual([...peekSettings(w.guild.id).modmailPingRoleIds], ['PING']);
});

test('/modmail block, blocked and unblock', async () => {
  resetRows();
  const w = await openWorld({ settings: { modRoleIds: ['MODROLE'] } });
  const target = w.user;
  const blockCmd = fakeCommand(w, 'block', { user: target, reason: 'spam' });
  await cmd.execute(blockCmd, w.client);
  assert.match(lastEmbed(blockCmd).title, /Member Blocked/);
  assert.match(lastEmbed(blockCmd).description, /open conversation was closed/);
  assert.deepEqual([...peekSettings(w.guild.id).modmailBlockedUserIds], [target.id]);
  assert.equal(threads.get(w.row.id).status, 'CLOSED');
  assert.match(threads.get(w.row.id).closeReason, /^Blocked: spam/);
  assert.doesNotMatch(embedOf(w.user.sent.at(-1)).description, /spam|Blocked/, 'the member is not told why');
  assert.equal(w.thread.archived, true);

  const again = fakeCommand(w, 'block', { user: target });
  await cmd.execute(again, w.client);
  assert.match(lastEmbed(again).title, /Already Blocked/);

  const self = fakeCommand(w, 'block', { user: fakeUser('MOD1') });
  await cmd.execute(self, w.client);
  assert.match(lastEmbed(self).title, /Cannot Block/);
  const bot = fakeCommand(w, 'block', { user: { ...fakeUser('B1'), bot: true } });
  await cmd.execute(bot, w.client);
  assert.match(lastEmbed(bot).title, /Cannot Block/);
  const staffUser = fakeUser('STAFF9', 'Boss');
  addMember(w.guild, staffUser, { roles: ['MODROLE'] });
  const staff = fakeCommand(w, 'block', { user: staffUser });
  await cmd.execute(staff, w.client);
  assert.match(lastEmbed(staff).description, /Staff cannot be blocked/);

  const list = fakeCommand(w, 'blocked');
  await cmd.execute(list, w.client);
  assert.equal(list.ephemeral, true);
  assert.match(lastEmbed(list).description, new RegExp(`<@${target.id}>`));
  assert.equal(lastEmbed(list).footer.text, `1 of ${LIMITS.MODMAIL_BLOCKED_USERS}`);

  // A blocked member is no longer offered the server.
  const dm = fakeDmMessage(target, 'let me in');
  await mm.handleDirectMessage({ client: w.client, message: dm });
  assert.match(embedOf(w.user.sent.at(-1)).title, /No Server Found/);

  const unblock = fakeCommand(w, 'unblock', { user: target });
  await cmd.execute(unblock, w.client);
  assert.match(lastEmbed(unblock).title, /Member Unblocked/);
  assert.deepEqual([...peekSettings(w.guild.id).modmailBlockedUserIds], []);
  const missing = fakeCommand(w, 'unblock', { user: target });
  await cmd.execute(missing, w.client);
  assert.match(lastEmbed(missing).title, /Not Blocked/);
  const empty = fakeCommand(w, 'blocked');
  await cmd.execute(empty, w.client);
  assert.match(lastEmbed(empty).description, /Nobody is blocked/);
});

test('/modmail block respects the list cap', async () => {
  resetRows();
  const w = await world({ settings: { modmailBlockedUserIds: Array.from({ length: LIMITS.MODMAIL_BLOCKED_USERS }, (_, i) => `X${i}`) } });
  const full = fakeCommand(w, 'block', { user: fakeUser('NEW1') });
  await cmd.execute(full, w.client);
  assert.match(lastEmbed(full).title, /Block List Full/);
  assert.equal(peekSettings(w.guild.id).modmailBlockedUserIds.length, LIMITS.MODMAIL_BLOCKED_USERS);
});

test('/modmail close and reply work only in an open modmail thread', async () => {
  resetRows();
  const w = await openWorld({ settings: { modRoleIds: ['MODROLE'] } });

  const outside = fakeCommand(w, 'close', {}, { channel: { ...w.channel, isThread: () => false } });
  await cmd.execute(outside, w.client);
  assert.match(lastEmbed(outside).title, /Not a Modmail Thread/);

  const reply = fakeCommand(w, 'reply', { text: 'Hello from staff', anonymous: true }, { channel: w.thread });
  await cmd.execute(reply, w.client);
  assert.match(lastEmbed(reply).title, /Reply Sent/);
  const dm = embedOf(w.user.sent.at(-1));
  assert.equal(dm.description, 'Hello from staff');
  assert.equal(dm.author.name, 'Staff');

  // The admin's anonymity setting cannot be turned off by a reply.
  await setSettings(w.guild.id, { modmailAnonymousReplies: true, modRoleIds: ['MODROLE'] });
  const named = fakeCommand(w, 'reply', { text: 'named?', anonymous: false }, { channel: w.thread });
  await cmd.execute(named, w.client);
  assert.equal(embedOf(w.user.sent.at(-1)).author.name, 'Staff');

  w.user.dmClosed = true;
  const failed = fakeCommand(w, 'reply', { text: 'hello?' }, { channel: w.thread });
  await cmd.execute(failed, w.client);
  assert.match(lastEmbed(failed).title, /Not Delivered/);
  w.user.dmClosed = false;

  // reply talks to a member, so it refuses while the module is off; close still works.
  await setSettings(w.guild.id, { modmailEnabled: false });
  const off = fakeCommand(w, 'reply', { text: 'x' }, { channel: w.thread });
  await cmd.execute(off, w.client);
  assert.equal(lastEmbed(off).title, '⚠️  Module Disabled');
  assert.equal(lastEmbed(off).description, 'The **Modmail** module is turned off on this server. An admin can enable it with `/module enable modmail`.');

  const close = fakeCommand(w, 'close', { reason: 'All done' }, { channel: w.thread });
  await cmd.execute(close, w.client);
  assert.match(lastEmbed(close).title, /Conversation Closed/);
  assert.match(lastEmbed(close).description, /All done/);
  assert.equal(threads.get(w.row.id).status, 'CLOSED');
  assert.equal(threads.get(w.row.id).closedById, 'MOD1');
  assert.match(embedOf(w.user.sent.at(-1)).description, /All done/);
  assert.equal(w.thread.archived, true);

  const again = fakeCommand(w, 'close', {}, { channel: w.thread });
  await cmd.execute(again, w.client);
  assert.match(lastEmbed(again).title, /Conversation Closed/);
  assert.match(lastEmbed(again).description, /already closed/);
});
