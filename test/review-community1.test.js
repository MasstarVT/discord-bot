// Regression tests of the community1 review (welcome, reaction roles, invites, starboard, temp voice). Offline: Prisma delegates are
// stubbed in place and the Discord objects are minimal fakes (mirrors the style of test/temp-voice.test.js, without a database).
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = 'redis://127.0.0.1:1'; // nothing listens: the cache fails open
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';
delete process.env.BOT_OWNER_ID;

const { ChannelType, Collection, OverwriteType, PermissionFlagsBits: P, PermissionsBitField } = await import('discord.js');
const tv = await import('../src/services/tempVoiceService.js');
const { default: redis, cache } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const GID = '955021000000009001';
const BOT = '955021000000009002';
const OWNER = '955021000000009003';
const OTHER = '955021000000009004';
const HUB_CH = '955021000000009010';
const VOICE = '955021000000009011';
const TEXT = '955021000000009012';
const CAT = '955021000000009013';

const adminBits = () => new PermissionsBitField(P.Administrator);

/** A guild whose channel cache and REST delete are in memory. */
function makeGuild() {
  const cache = new Collection();
  const calls = { deleted: [], overwrites: [] };
  const guild = {
    id: GID,
    available: true,
    voiceStates: { cache: new Collection() },
    roles: { cache: new Collection() },
    channels: {
      cache,
      async fetch(id) {
        const err = new Error('Unknown Channel');
        err.code = 10003;
        err.id = id;
        throw err;
      },
      async delete(id) {
        cache.delete(id);
        calls.deleted.push(id);
      },
    },
    members: {
      me: { id: BOT, permissions: adminBits(), permissionsIn: () => adminBits() },
      async fetch(id) {
        const m = guild.fakeMembers.get(id);
        if (!m) throw Object.assign(new Error('Unknown Member'), { code: 10007 });
        return m;
      },
    },
    fakeMembers: new Map(),
  };
  return { guild, calls };
}

/** A channel with an overwrite spy. */
function addChannel(guild, calls, { id, type = ChannelType.GuildVoice, parent = null, overwrites = [] }) {
  const ch = {
    id,
    name: `ch-${id.slice(-3)}`,
    type,
    parent,
    guild,
    userLimit: 0,
    members: new Collection(),
    permissionOverwrites: {
      cache: new Collection(overwrites.map((o) => [o.id, { id: o.id, type: o.type, allow: { bitfield: BigInt(o.allow) }, deny: { bitfield: BigInt(o.deny) } }])),
      async set(list) {
        calls.overwrites.push({ id, list });
      },
    },
    permissionsFor: () => adminBits(),
    isTextBased: () => type === ChannelType.GuildText,
    isThread: () => false,
    delete: async () => guild.channels.delete(id),
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

const makeMember = (guild, id, { channelId = null } = {}) => {
  const m = {
    id,
    guild,
    displayName: `m${id.slice(-2)}`,
    user: { id, bot: false, tag: `m${id.slice(-2)}#0` },
    roles: { cache: new Collection() },
    permissions: new PermissionsBitField(0n),
    voice: { channelId },
  };
  guild.fakeMembers.set(id, m);
  return m;
};

const hubRow = (over = {}) => ({
  id: 'hub1', guildId: GID, channelId: HUB_CH, categoryId: null, syncPermissions: false, moderatorRoleIds: [], ignoredRoleIds: [],
  keepAliveMinutes: 0, ownershipLockMinutes: 0, createTextChannel: false, textChannelMembersOnly: true, ...over,
});

const tempRow = (hub, over = {}) => ({
  id: 'row1', guildId: GID, hubId: hub.id, channelId: VOICE, textChannelId: null, controlMessageId: null, ownerId: OWNER, index: 1,
  locked: false, hidden: false, bannedUserIds: [], ownerLeftAt: null, emptySince: null, hub, ...over,
});

const PRIVATE_HUB_OVERWRITES = [
  { id: GID, type: OverwriteType.Role, allow: 0n, deny: P.ViewChannel }, // @everyone cannot see the hub's channels
  { id: '955021000000009020', type: OverwriteType.Role, allow: P.ViewChannel | P.Connect, deny: 0n }, // the VIP role can
];

/** Replaces Prisma delegate methods in place and returns a function that restores them. */
function stub(target, methods) {
  const saved = {};
  for (const [name, fn] of Object.entries(methods)) {
    saved[name] = target[name];
    target[name] = fn;
  }
  return () => Object.assign(target, saved);
}

const hasDeny = (list, id, bit) => (list.find((o) => o.id === id)?.deny & bit) === bit;

// ── Temp voice: a retired hub must not wipe the permission template ──────────────

test('applyVoiceOverwrites refuses to rewrite from an empty template once the hub channel is deleted', async () => {
  const { guild, calls } = makeGuild();
  const vc = addChannel(guild, calls, { id: VOICE });
  const hub = hubRow();
  const state = { hub, ownerId: OWNER, locked: true, hidden: false, bannedUserIds: [] };

  const gone = await tv.applyVoiceOverwrites(guild, vc, state, null, 'test');
  assert.equal(gone.ok, false);
  assert.equal(gone.hubGone, true);
  assert.equal(calls.overwrites.length, 0, 'nothing is written without a template');

  addChannel(guild, calls, { id: HUB_CH, overwrites: PRIVATE_HUB_OVERWRITES });
  const ok = await tv.applyVoiceOverwrites(guild, vc, state, null, 'test');
  assert.equal(ok.ok, true);
  assert.equal(calls.overwrites.length, 1);
  const { list } = calls.overwrites[0];
  assert.ok(hasDeny(list, GID, P.ViewChannel), "the hub's @everyone restriction is kept");
  assert.ok(hasDeny(list, GID, P.Connect), 'lock still denies Connect');
  assert.ok(list.some((o) => o.id === '955021000000009020'), "the hub's VIP overwrite is kept");
});

test('a hub that copies its category still rewrites after the hub channel is gone', async () => {
  const { guild, calls } = makeGuild();
  const category = addChannel(guild, calls, { id: CAT, type: ChannelType.GuildCategory, overwrites: PRIVATE_HUB_OVERWRITES });
  const vc = addChannel(guild, calls, { id: VOICE, parent: category });
  const res = await tv.applyVoiceOverwrites(guild, vc, { hub: hubRow({ syncPermissions: true }), ownerId: OWNER, bannedUserIds: [] }, null, 'test');
  assert.equal(res.ok, true);
  assert.ok(hasDeny(calls.overwrites[0].list, GID, P.ViewChannel));
});

test('lock and transfer on a retired hub answer "Hub Deleted" and change nothing', async () => {
  const { guild, calls } = makeGuild();
  const vc = addChannel(guild, calls, { id: VOICE });
  const hub = hubRow();
  const row = tempRow(hub);
  const owner = makeMember(guild, OWNER, { channelId: VOICE });
  const other = makeMember(guild, OTHER, { channelId: VOICE });
  vc.members.set(OWNER, owner);
  vc.members.set(OTHER, other);
  const updates = [];
  const restore = stub(prisma.tempVoiceChannel, {
    findUnique: async () => row,
    update: async (args) => { updates.push(args); return { ...row, ...args.data }; },
  });
  try {
    const locked = await tv.runVoiceAction({ guild, actor: owner, channelId: VOICE, action: 'lock', settings: null, source: 'command' });
    assert.equal(locked.ok, false);
    assert.equal(locked.embed.data.title, tv.MSG.HUB_GONE().data.title);
    assert.match(locked.embed.data.title, /Hub Deleted/);

    const moved = await tv.runVoiceAction({ guild, actor: owner, channelId: VOICE, action: 'transfer', targetId: OTHER, settings: null, source: 'command' });
    assert.equal(moved.ok, false);
    assert.match(moved.embed.data.title, /Hub Deleted/);

    assert.equal(calls.overwrites.length, 0, 'the overwrites were not rewritten');
    assert.equal(updates.length, 0, 'the row was not changed');

    // With the hub channel back in the cache the same lock goes through.
    addChannel(guild, calls, { id: HUB_CH, overwrites: PRIVATE_HUB_OVERWRITES });
    const again = await tv.runVoiceAction({ guild, actor: owner, channelId: VOICE, action: 'lock', settings: null, source: 'command' });
    assert.equal(again.ok, true);
    assert.equal(calls.overwrites.length, 1);
    assert.deepEqual(updates.map((u) => u.data), [{ locked: true }]);
    assert.ok(hasDeny(calls.overwrites[0].list, GID, P.ViewChannel), 'the private hub stays private while locked');
  } finally {
    restore();
  }
});

test('a members-only companion text channel still follows the voice members on a retired hub', async () => {
  const { guild, calls } = makeGuild();
  const vc = addChannel(guild, calls, { id: VOICE });
  const text = addChannel(guild, calls, { id: TEXT, type: ChannelType.GuildText });
  const hub = hubRow({ createTextChannel: true, textChannelMembersOnly: true });
  const row = tempRow(hub, { textChannelId: TEXT });
  vc.members.set(OTHER, makeMember(guild, OTHER, { channelId: VOICE }));
  const LEFT = '955021000000009005';
  const restore = stub(prisma.tempVoiceChannel, { findUnique: async () => row });
  // The settings lookup (Redis, then Postgres) would wait for connections that never come.
  const restoreCache = stub(cache, { get: async () => null, setNX: async () => true });
  const restoreSettings = stub(prisma.guildSettings, { findUnique: async () => ({ guildId: GID, modRoleIds: [] }) });
  try {
    // The hub channel is gone: visibility must still be granted to members in the voice channel and withheld from everyone else.
    await tv.syncTextOverwrites(guild, VOICE);
    assert.equal(calls.overwrites.length, 1);
    const { id, list } = calls.overwrites[0];
    assert.equal(id, text.id);
    assert.ok(hasDeny(list, GID, P.ViewChannel), '@everyone cannot see the chat');
    const allowed = (userId) => ((list.find((o) => o.id === userId)?.allow ?? 0n) & P.ViewChannel) === P.ViewChannel;
    assert.equal(allowed(OTHER), true, 'a member in the voice channel can see it');
    assert.equal(allowed(LEFT), false, 'someone who left gets no access');
  } finally {
    restore();
    restoreCache();
    restoreSettings();
  }
});

// ── Temp voice: purging a row must not orphan the companion text channel ────────

test('purgeRow deletes a companion text channel that is still there, and only that', async () => {
  const { guild, calls } = makeGuild();
  addChannel(guild, calls, { id: HUB_CH });
  addChannel(guild, calls, { id: TEXT, type: ChannelType.GuildText });
  const hub = hubRow();
  const purged = [];
  const restoreRows = stub(prisma.tempVoiceChannel, { deleteMany: async (args) => { purged.push(args.where.channelId); return { count: 1 }; } });
  const restoreJobs = stub(prisma.scheduledJob, { deleteMany: async () => ({ count: 0 }) });
  try {
    // The voice channel is already gone (a leave event or a panel click found a 10003); the chat must follow it.
    await tv.purgeRow(guild, tempRow(hub, { textChannelId: TEXT }));
    assert.deepEqual(calls.deleted, [TEXT]);
    assert.deepEqual(purged, [VOICE]);

    // A caller that already deleted the text channel leaves nothing in the cache: no second delete.
    await tv.purgeRow(guild, tempRow(hub, { textChannelId: TEXT }));
    assert.deepEqual(calls.deleted, [TEXT]);
    assert.deepEqual(purged, [VOICE, VOICE]);

    // A row without a text channel deletes nothing.
    await tv.purgeRow(guild, tempRow(hub));
    assert.deepEqual(calls.deleted, [TEXT]);
  } finally {
    restoreRows();
    restoreJobs();
  }
});

test('computeVoiceOverwrites: lock and hide also close a role the template allows (only exempt roles keep it)', () => {
  const VERIFIED = '955021000000009101';
  const STAFF = '955021000000009102';
  const template = [
    { id: GID, type: OverwriteType.Role, allow: 0n, deny: P.ViewChannel | P.Connect },
    { id: VERIFIED, type: OverwriteType.Role, allow: P.ViewChannel | P.Connect | P.Speak, deny: 0n },
    { id: STAFF, type: OverwriteType.Role, allow: P.ViewChannel | P.Connect, deny: 0n },
  ];
  const base = { everyoneId: GID, botId: BOT, ownerId: OWNER, template, exemptRoleIds: [STAFF] };
  const by = (out) => Object.fromEntries(out.map((o) => [o.id, o]));

  const open = by(tv.computeVoiceOverwrites(base));
  assert.equal(open[VERIFIED].allow, P.ViewChannel | P.Connect | P.Speak, 'unlocked: the template is kept as is');

  const locked = by(tv.computeVoiceOverwrites({ ...base, locked: true }));
  assert.equal(locked[VERIFIED].allow & P.Connect, 0n, 'locked: the Verified role can no longer join');
  assert.equal(locked[VERIFIED].allow, P.ViewChannel | P.Speak, 'other bits are untouched');
  assert.equal(locked[STAFF].allow & P.Connect, P.Connect, 'an exempt role still joins');
  assert.equal(locked[OWNER].allow & P.Connect, P.Connect, 'the owner still joins');

  const hidden = by(tv.computeVoiceOverwrites({ ...base, hidden: true }));
  assert.equal(hidden[VERIFIED].allow & P.ViewChannel, 0n, 'hidden: the Verified role no longer sees it');
  assert.equal(hidden[VERIFIED].allow & P.Connect, P.Connect, 'hiding alone does not lock');
  assert.equal(template[1].allow, P.ViewChannel | P.Connect | P.Speak, 'the template itself is not mutated');
});

// ── Starboard: private channels never feed a board that @everyone can see ─────────

const sb = await import('../src/services/starboardService.js');
const { everyoneCanView, copyLeaksPrivate } = await import('../src/utils/privacyGuards.js');

const SBG = '955021000000010001';
const sbId = (n) => String(955021000000010100n + BigInt(n));
const sbGuild = { id: SBG, available: true, roles: { everyone: { id: SBG } }, channels: { cache: new Map() }, members: { me: { id: BOT } } };
const sbBits = (...flags) => new PermissionsBitField(flags.reduce((a, f) => a | f, 0n));

/** A text channel (or thread) whose permissions for @everyone are `view`; every other asker (the bot) can do everything. */
function sbChannel(id, { view = true, nsfw = false, thread = null, privateThread = false, extra = {} } = {}) {
  const ch = {
    id, guildId: SBG, guild: sbGuild, nsfw, name: `c${id.slice(-3)}`, parentId: thread?.id ?? null,
    type: privateThread ? ChannelType.PrivateThread : (thread ? ChannelType.PublicThread : ChannelType.GuildText),
    isTextBased: () => true, isThread: () => !!thread,
    parent: thread,
    permissionsFor(who) {
      if (thread) return thread.permissionsFor(who);
      const everyone = who === sbGuild.roles.everyone || who === SBG;
      return everyone ? sbBits(...(view ? [P.ViewChannel] : [])) : new PermissionsBitField(PermissionsBitField.All);
    },
    ...extra,
  };
  sbGuild.channels.cache.set(id, ch);
  return ch;
}

test('everyoneCanView and copyLeaksPrivate: hidden channels, private threads, unknowns', () => {
  const pub = sbChannel(sbId(1));
  const hidden = sbChannel(sbId(2), { view: false });
  const publicThread = sbChannel(sbId(3), { thread: pub });
  const hiddenThread = sbChannel(sbId(4), { thread: hidden });
  const privThread = sbChannel(sbId(5), { thread: pub, privateThread: true });
  assert.equal(everyoneCanView(pub), true);
  assert.equal(everyoneCanView(hidden), false);
  assert.equal(everyoneCanView(publicThread), true, 'a public thread follows its parent');
  assert.equal(everyoneCanView(hiddenThread), false);
  assert.equal(everyoneCanView(privThread), false, 'a private thread is hidden even in a public channel');
  assert.equal(everyoneCanView(null), null);
  assert.equal(everyoneCanView({ id: 'x', guild: sbGuild, type: ChannelType.GuildText, permissionsFor: () => null }), null);

  assert.equal(copyLeaksPrivate(pub, pub), false);
  assert.equal(copyLeaksPrivate(hidden, pub), true, 'private source, public target');
  assert.equal(copyLeaksPrivate(privThread, pub), true);
  assert.equal(copyLeaksPrivate(hidden, hidden), false, 'a private target may take private content');
  assert.equal(copyLeaksPrivate(hidden, privThread), false);
  assert.equal(copyLeaksPrivate(pub, hidden), false);
  assert.equal(copyLeaksPrivate(hidden, undefined), true, 'an unknown target counts as visible');
  assert.equal(copyLeaksPrivate(null, pub), true, 'an unknown source counts as hidden');
});

test('sourceSkipReason: a hidden source never feeds a public board; a private board may take it', () => {
  const board = { ignoredChannelIds: [], allowNsfw: false };
  const ids = new Set([sbId(10)]);
  const publicBoard = sbChannel(sbId(10));
  const privateBoard = sbChannel(sbId(11), { view: false });
  const hidden = sbChannel(sbId(12), { view: false });
  const pub = sbChannel(sbId(13));
  const priv = sbChannel(sbId(14), { thread: pub, privateThread: true });
  const reason = (channel, boardChannel) => sb.sourceSkipReason({ board, channel, boardChannelIds: ids, boardChannel });
  assert.equal(reason(hidden, publicBoard), 'private');
  assert.equal(reason(priv, publicBoard), 'private');
  assert.equal(reason(hidden, privateBoard), null);
  assert.equal(reason(priv, privateBoard), null);
  assert.equal(reason(pub, publicBoard), null);
  assert.equal(reason(pub, privateBoard), null);
  assert.equal(reason(hidden, undefined), 'private', 'a board channel that is not cached counts as visible');
});

/** A world with one board, a source channel and one star on a message; counts are taken from the raw reaction (self-star shortcut). */
function sbWorld({ boardView = true, sourceView = true, sourcePrivateThread = false, sourceNsfw = false }) {
  sbGuild.channels.cache.clear();
  const sent = [];
  const edits = [];
  const bc = sbChannel(sbId(20), {
    view: boardView,
    extra: {
      send: async (payload) => { sent.push(payload); return { id: sbId(90 + sent.length), delete: async () => {}, react: async () => {} }; },
      messages: { edit: async (id, payload) => { edits.push({ id, payload }); }, delete: async () => {} },
    },
  });
  const parent = sbChannel(sbId(21), { view: sourceView, nsfw: sourceNsfw });
  const source = sourcePrivateThread ? sbChannel(sbId(22), { thread: parent, privateThread: true }) : parent;
  const board = {
    id: 'sbboard1', name: 'main', channelId: bc.id, emojis: ['⭐'], threshold: 1, selfStar: true, ignoreBots: false,
    removeBelowThreshold: true, autoReact: false, maxMessageAgeDays: 0, allowNsfw: false, ignoredChannelIds: [], removeSelfStars: false, enabled: true,
  };
  sb.__setBoardsMemo(SBG, [board]);
  const star = { emoji: { id: null, name: '⭐', toString: () => '⭐' }, count: 2, me: false };
  const message = {
    id: sbId(30), channelId: source.id, channel: source, content: 'secret plans', system: false, partial: false,
    author: { id: sbId(31), bot: false, username: 'ann', displayName: 'Ann', displayAvatarURL: () => 'https://x/a.png' },
    member: null, attachments: new Map(), embeds: [], stickers: new Map(), url: `https://discord.com/channels/${SBG}/${source.id}/${sbId(30)}`,
    createdTimestamp: Date.now(), reactions: { cache: new Collection([['⭐', star]]) },
  };
  source.messages = { cache: new Map([[message.id, message]]), fetch: async () => message };
  const client = { user: { id: BOT }, emojis: { cache: new Map() }, channels: { fetch: async () => null } };
  sbGuild.client = client;
  return { bc, source, board, message, client, sent, edits };
}

const sbEntries = [];
const realEntry = prisma.starboardEntry;
prisma.starboardEntry = {
  findMany: async () => sbEntries.slice(),
  create: async ({ data }) => { sbEntries.push({ id: `e${sbEntries.length + 1}`, blocked: false, count: data.count, ...data }); return data; },
  updateMany: async () => ({ count: 1 }),
};
after(() => { prisma.starboardEntry = realEntry; sb.__setBoardsMemo(SBG, null); });

const sbSync = (w) => sb.syncMessage({ client: w.client, guild: sbGuild, channel: w.source, messageId: w.message.id, ensureMessage: async () => w.message });

test('syncMessage: a message starred in a hidden channel or private thread is not reposted to a public board', async () => {
  sbEntries.length = 0;
  const open = sbWorld({});
  await sbSync(open);
  assert.equal(open.sent.length, 1, 'a public source is reposted');

  for (const variant of [{ sourceView: false }, { sourcePrivateThread: true }]) {
    sbEntries.length = 0;
    const w = sbWorld(variant);
    await sbSync(w);
    assert.equal(w.sent.length, 0, `not reposted (${JSON.stringify(variant)})`);
    assert.equal(sbEntries.length, 0, 'and no entry was stored');
  }

  sbEntries.length = 0;
  const privateBoard = sbWorld({ boardView: false, sourceView: false });
  await sbSync(privateBoard);
  assert.equal(privateBoard.sent.length, 1, 'a private board may take posts from a private channel');
});

test('mirrorEdit: an edit is mirrored only while the source is still eligible (NSFW and privacy re-checked)', async () => {
  const until = async (fn, ms = 1000) => {
    const end = Date.now() + ms;
    while (!fn() && Date.now() < end) await new Promise((r) => setTimeout(r, 5));
  };
  const entry = { id: 'e1', starboardId: 'sbboard1', sourceMessageId: sbId(30), boardMessageId: sbId(95), blocked: false, count: 3 };
  const edit = (w) => {
    sbEntries.length = 0;
    sbEntries.push(entry);
    sb.onMessageUpdate({ client: w.client, guild: sbGuild, guildId: SBG, message: w.message, newMessage: w.message });
  };

  const open = sbWorld({});
  edit(open);
  await until(() => open.edits.length > 0);
  assert.equal(open.edits.length, 1, 'an eligible source is mirrored');
  assert.equal(open.edits[0].payload.embeds[0].toJSON().description, 'secret plans');

  for (const [label, variant] of [['hidden channel', { sourceView: false }], ['private thread', { sourcePrivateThread: true }], ['NSFW channel', { sourceNsfw: true }]]) {
    const w = sbWorld(variant);
    edit(w);
    await new Promise((r) => setTimeout(r, 80));
    assert.equal(w.edits.length, 0, `${label}: the repost is not edited`);
  }

  const privateBoard = sbWorld({ boardView: false, sourceView: false });
  edit(privateBoard);
  await until(() => privateBoard.edits.length > 0);
  assert.equal(privateBoard.edits.length, 1, 'a private board still mirrors a private source');
});
