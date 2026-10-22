// Sticky message tests. Offline (no Discord, Postgres or Redis): the debounce scheduler (real, short timers), the sticky
// channel cache, rendering, the re-send flow against an in-memory StickyMessage table and fake channels, the admin
// operations, the hooks, the modal, the plugin declaration and the /sticky command.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const svc = await import('../src/services/stickyService.js');
const sticky = await import('../src/plugins/sticky.js');
const cmd = await import('../src/commands/admin/sticky.js');
const { PermissionFlagsBits } = await import('discord.js');
const { Prisma } = await import('@prisma/client');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { CUSTOM_ID_PREFIXES } = await import('../src/config/customIds.js');
const { updateSettings } = await import('../src/services/settingsService.js');

const {
  STICKY, StickyError, createScheduler, getChannels, peekChannels, invalidate, renderSticky, embedPart, resendSticky,
  setSticky, removeSticky, setStickyEnabled, clearLastMessage, dropChannel, noteMessage, cancelPending, _scheduler,
} = svc;
const { plugin, mayStick, mayBeSticky, mayHaveSticky, onMessage, onMessageDelete, onChannelDelete, buildStickyModal, handleStickyModal } = sticky;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const sleep = (ms) => new Promise((r) => { setTimeout(r, ms); });

// ── In-memory tables ──────────────────────────────────────────────────────────

const rows = new Map();                 // channelId -> StickyMessage row
const calls = { findMany: 0, upsert: 0 };
let hookBeforeUpdate = null;            // runs inside updateMany, to model a change made while a re-send is in flight
let failFindMany = false;
let findManyDelay = 0;
const real = { stickyMessage: prisma.stickyMessage, guildSettings: prisma.guildSettings, savedEmbed: prisma.savedEmbed };

const matches = (row, where = {}) => Object.entries(where).every(([k, v]) => row[k] === v);

prisma.stickyMessage = {
  findUnique: async ({ where }) => (rows.has(where.channelId) ? { ...rows.get(where.channelId) } : null),
  findFirst: async ({ where }) => {
    const row = [...rows.values()].find((r) => matches(r, where));
    return row ? { ...row } : null;
  },
  findMany: async ({ where }) => {
    calls.findMany += 1;
    if (findManyDelay) await sleep(findManyDelay);
    if (failFindMany) throw new Error('db down');
    return [...rows.values()].filter((r) => matches(r, where)).map((r) => ({ ...r }));
  },
  count: async ({ where }) => [...rows.values()].filter((r) => matches(r, where)).length,
  upsert: async ({ where, create, update }) => {
    calls.upsert += 1;
    const existing = rows.get(where.channelId);
    const row = existing ? { ...existing, ...update } : { lastMessageId: null, createdAt: new Date(rows.size), ...create };
    if (row.embed === Prisma.DbNull) row.embed = null; // Prisma.DbNull stands for SQL NULL
    rows.set(where.channelId, row);
    return { ...row };
  },
  updateMany: async ({ where, data }) => {
    if (hookBeforeUpdate) { const h = hookBeforeUpdate; hookBeforeUpdate = null; h(); }
    let count = 0;
    for (const row of rows.values()) if (matches(row, where)) { Object.assign(row, data); count += 1; }
    return { count };
  },
  deleteMany: async ({ where }) => {
    let count = 0;
    for (const [k, row] of rows) if (matches(row, where)) { rows.delete(k); count += 1; }
    return { count };
  },
};
prisma.guildSettings = { upsert: async ({ where, update }) => ({ guildId: where.guildId, ...update }) };
let savedEmbeds = {};
prisma.savedEmbed = { findUnique: async ({ where }) => savedEmbeds[where.guildId_name.name] ?? null };

after(async () => {
  Object.assign(prisma, real);
  Object.assign(console, quiet);
  _scheduler.clear();
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const seed = (guildId, enabled = true) => updateSettings(guildId, { stickyEnabled: enabled });
const addRow = (guildId, channelId, extra = {}) => {
  const row = {
    id: `id-${channelId}`, guildId, channelId, content: 'Read the rules', embed: null, lastMessageId: null, enabled: true,
    createdById: 'ADMIN', createdAt: new Date(rows.size), ...extra,
  };
  rows.set(channelId, row);
  return row;
};

// ── Fakes ─────────────────────────────────────────────────────────────────────

let nextId = 5000;
const log = [];
function fakeChannel(guildId, id, { deny = [], guild = null } = {}) {
  const denied = new Set(deny.map((k) => PermissionFlagsBits[k]));
  const channel = {
    id, guildId, guild: guild ?? { id: guildId, members: { me: {} } }, sent: [], deleted: [], lastMessageId: undefined,
    failDelete: null, failSend: null,
    permissionsFor: () => ({ has: (bit) => !denied.has(bit) }),
    isTextBased: () => true,
    isThread: () => false,
    messages: {
      async delete(messageId) {
        log.push(`delete ${messageId}`);
        if (channel.failDelete) throw Object.assign(new Error('delete failed'), { code: channel.failDelete });
        channel.deleted.push(messageId);
      },
    },
    async send(payload) {
      log.push('send');
      if (channel.failSend) throw Object.assign(new Error('send failed'), { code: channel.failSend });
      const message = { id: String(nextId++) };
      channel.sent.push(payload);
      channel.lastMessageId = message.id;
      return message;
    },
  };
  return channel;
}
function fakeGuild(id, channels) {
  const guild = { id, name: 'Test Guild', memberCount: 5, channels: { cache: new Map() }, members: { me: {} } };
  for (const c of channels) { c.guild = guild; guild.channels.cache.set(c.id, c); }
  return guild;
}
const fakeClient = (guilds) => ({ user: { id: 'BOT' }, guilds: { cache: new Map(guilds.map((g) => [g.id, g])) } });

function world(guildId, channelIds, opts = {}) {
  const channels = channelIds.map((id) => fakeChannel(guildId, id, opts));
  const guild = fakeGuild(guildId, channels);
  return { guild, channels, channel: channels[0], client: fakeClient([guild]) };
}

const ctxFor = (guildId, message, extra = {}) => ({ guildId, message, client: { user: { id: 'BOT' } }, ...extra });
const text = (payload) => payload.content;

// ── Scheduler ─────────────────────────────────────────────────────────────────

test('scheduler: one run once the channel has been quiet, timers do not keep the process alive', async () => {
  const runs = [];
  const timers = [];
  const realSetTimeout = globalThis.setTimeout;
  globalThis.setTimeout = (...args) => { const t = realSetTimeout(...args); if (args[1] === 30) timers.push(t); return t; }; // the scheduler's timers (the test's own sleeps use other delays)
  const s = createScheduler({ quietMs: 30, maxWaitMs: 200, run: (id, arg) => { runs.push([id, arg]); } });
  try {
    s.touch('C', 'a');
    await sleep(15); s.touch('C', 'b');
    await sleep(15); s.touch('C', 'c');
    assert.equal(runs.length, 0, 'still being talked in');
    await sleep(70);
    assert.deepEqual(runs, [['C', 'c']], 'one run, with the latest argument');
    assert.equal(s.size, 0, 'nothing left behind');
  } finally {
    globalThis.setTimeout = realSetTimeout;
  }
  assert.ok(timers.length >= 3);
  assert.ok(timers.every((t) => t.hasRef() === false), 'every timer is unref\'d');
});

test('scheduler: a channel that never goes quiet is served once the maximum wait is up', async () => {
  const runs = [];
  const s = createScheduler({ quietMs: 40, maxWaitMs: 100, run: () => { runs.push(Date.now()); } });
  const start = Date.now();
  for (let i = 0; i < 9; i += 1) { s.touch('C', null); await sleep(15); } // ~135 ms of chat, never quiet for 40 ms
  assert.equal(runs.length, 1, 'the first run came while the chat went on');
  assert.ok(runs[0] - start >= 90 && runs[0] - start < 135, `ran ${runs[0] - start} ms in`);
  await sleep(80);
  assert.equal(runs.length, 2, 'the rest of the chat gets its own run');
  assert.equal(s.size, 0);
});

test('scheduler: channels are independent; a touch during a run reruns afterwards; cancel and failures', async () => {
  const runs = [];
  let release = null;
  const s = createScheduler({
    quietMs: 10, maxWaitMs: 100,
    run: (id) => { runs.push(id); return id === 'SLOW' && runs.filter((r) => r === 'SLOW').length === 1 ? new Promise((r) => { release = r; }) : undefined; },
  });
  s.touch('A', null); s.touch('B', null);
  await sleep(40);
  assert.deepEqual(runs.sort(), ['A', 'B']);

  s.touch('SLOW', null);
  await sleep(30);
  assert.equal(runs.filter((r) => r === 'SLOW').length, 1, 'running');
  s.touch('SLOW', null); s.touch('SLOW', null); // while in flight: only marks it dirty
  await sleep(30);
  assert.equal(runs.filter((r) => r === 'SLOW').length, 1, 'one run in flight at a time');
  release();
  await sleep(40);
  assert.equal(runs.filter((r) => r === 'SLOW').length, 2, 'rerun after the first finished');
  assert.equal(s.size, 0);

  s.touch('X', null);
  assert.equal(s.has('X'), true);
  assert.equal(s.cancel('X'), true);
  assert.equal(s.cancel('X'), false);
  await sleep(30);
  assert.equal(runs.includes('X'), false);

  const failing = createScheduler({ quietMs: 5, maxWaitMs: 50, run: () => { throw new Error('boom'); } });
  failing.touch('F', null);
  await sleep(30);
  assert.equal(failing.size, 0, 'a failing run leaves nothing pending and rejects nothing');
});

// ── Sticky channel cache ──────────────────────────────────────────────────────

test('channel cache: one query per guild, an empty answer is remembered too, changes invalidate it', async () => {
  rows.clear();
  addRow('GC', 'C1');
  addRow('GC', 'C2', { enabled: false });
  invalidate('GC'); invalidate('GE');
  calls.findMany = 0;
  assert.equal(peekChannels('GC'), null, 'unknown until loaded');

  const first = await getChannels('GC');
  assert.deepEqual([...first], [['C1', true], ['C2', false]]);
  assert.equal((await getChannels('GC')).size, 2);
  assert.equal(peekChannels('GC').get('C1'), true);
  assert.equal(calls.findMany, 1, 'the second look-up is free');

  assert.equal((await getChannels('GE')).size, 0);
  await getChannels('GE');
  assert.equal(calls.findMany, 2, 'a guild without stickies is not asked again');

  invalidate('GC');
  assert.equal(peekChannels('GC'), null);
  await getChannels('GC');
  assert.equal(calls.findMany, 3);
});

test('channel cache: concurrent loads share one query, the memo expires, a failed load is not remembered', async () => {
  rows.clear();
  addRow('GD', 'C1');
  invalidate('GD');
  calls.findMany = 0;
  findManyDelay = 20;
  const [a, b] = await Promise.all([getChannels('GD'), getChannels('GD')]);
  findManyDelay = 0;
  assert.equal(calls.findMany, 1);
  assert.equal(a, b);

  const realNow = Date.now;
  try {
    Date.now = () => realNow() + STICKY.MEMO_TTL_MS + 1;
    assert.equal(peekChannels('GD'), null, 'expired');
  } finally {
    Date.now = realNow;
  }

  invalidate('GD');
  failFindMany = true;
  try {
    assert.equal(await getChannels('GD'), null);
  } finally {
    failFindMany = false;
  }
  assert.equal(peekChannels('GD'), null);
  assert.equal((await getChannels('GD')).size, 1, 'retried');

  // A change made while the list is loading wins over the stale answer.
  invalidate('GD');
  findManyDelay = 20;
  const loading = getChannels('GD');
  invalidate('GD');
  await loading;
  findManyDelay = 0;
  assert.equal(peekChannels('GD'), null, 'the stale answer was not remembered');
});

// ── Rendering ─────────────────────────────────────────────────────────────────

test('renderSticky: placeholders in the text, an embed, never any ping', () => {
  const { guild, channel } = world('GR', ['C1']);
  const out = renderSticky({ content: 'Welcome to {server} @everyone <@&1234> <@5678>', embed: null }, { guild, channel });
  assert.equal(text(out), 'Welcome to Test Guild @everyone <@&1234> <@5678>');
  assert.deepEqual(out.allowedMentions, { parse: [] });
  assert.deepEqual(out.embeds, []);

  const withEmbed = renderSticky({ content: null, embed: { embed: { title: 'Rules for {server}', description: 'Be kind' }, buttons: [] } }, { guild, channel });
  assert.equal(withEmbed.embeds.length, 1);
  assert.equal(withEmbed.embeds[0].toJSON().title, 'Rules for Test Guild');
  assert.deepEqual(withEmbed.allowedMentions, { parse: [] });
  assert.equal(withEmbed.content, undefined);

  assert.equal(renderSticky({ content: null, embed: null }, { guild, channel }), null, 'nothing to show');
  assert.equal(renderSticky({ content: '   ', embed: null }, { guild, channel }), null);
});

test('embedPart keeps a saved embed\'s embed and link buttons, and nothing when it has neither', () => {
  assert.equal(embedPart({ content: 'only text', embed: null, buttons: [] }), null);
  assert.equal(embedPart(null), null);
  const part = embedPart({ content: 'x', embed: { title: 'T' }, buttons: [{ label: 'Docs', url: 'https://example.com' }] });
  assert.equal(part.embed.title, 'T');
  assert.equal(part.buttons.length, 1);
});

// ── Re-sending ────────────────────────────────────────────────────────────────

test('resend: posts, then deletes the previous message before posting the next one', async () => {
  rows.clear(); log.length = 0;
  await seed('G1');
  const { client, channel } = world('G1', ['C1']);
  addRow('G1', 'C1');

  assert.equal(await resendSticky(client, 'G1', 'C1'), 'sent');
  const first = rows.get('C1').lastMessageId;
  assert.ok(first, 'saved');
  assert.deepEqual(log, ['send'], 'nothing to delete the first time');
  assert.equal(text(channel.sent[0]), 'Read the rules');
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [] }, 'a sticky never pings');

  log.length = 0;
  channel.lastMessageId = '9999'; // chat moved on
  assert.equal(await resendSticky(client, 'G1', 'C1'), 'sent');
  assert.deepEqual(log, [`delete ${first}`, 'send'], 'old one first, then the new one');
  assert.notEqual(rows.get('C1').lastMessageId, first);
  assert.equal(rows.get('C1').lastMessageId, channel.lastMessageId);
});

test('resend: an already-gone old message is fine; other delete failures do not stop the post', async () => {
  rows.clear();
  await seed('G2');
  const { client, channel } = world('G2', ['C1']);
  addRow('G2', 'C1', { lastMessageId: '111' });
  channel.failDelete = 10008; // unknown message
  assert.equal(await resendSticky(client, 'G2', 'C1'), 'sent');
  assert.equal(channel.sent.length, 1);

  rows.get('C1').lastMessageId = '222';
  channel.lastMessageId = '1';
  channel.failDelete = 50013; // missing permissions
  assert.equal(await resendSticky(client, 'G2', 'C1'), 'sent');
  assert.equal(channel.sent.length, 2, 'still posted');
});

test('resend: skipped while the sticky is already the last message, unless forced', async () => {
  rows.clear();
  await seed('G3');
  const { client, channel } = world('G3', ['C1']);
  addRow('G3', 'C1', { lastMessageId: '777' });
  channel.lastMessageId = '777';
  assert.equal(await resendSticky(client, 'G3', 'C1'), 'current');
  assert.equal(channel.sent.length, 0);
  assert.equal(await resendSticky(client, 'G3', 'C1', { force: true }), 'sent');
  assert.equal(channel.sent.length, 1);
});

test('resend: paused, module off, no row, deleted channel and empty sticky post nothing', async () => {
  rows.clear();
  await seed('G4'); await seed('G4OFF', false);
  const { client, channel } = world('G4', ['C1', 'C2', 'C3']);
  addRow('G4', 'C1', { enabled: false });
  assert.equal(await resendSticky(client, 'G4', 'C1'), 'paused');
  assert.equal(await resendSticky(client, 'G4', 'NOPE'), 'missing');
  addRow('G4', 'GHOST');
  addRow('G4', 'C3');
  assert.equal(await resendSticky(client, 'G4', 'GHOST'), 'gone', 'channel not in the guild cache');
  addRow('G4', 'C2', { content: null, embed: null });
  assert.equal(await resendSticky(client, 'G4', 'C2'), 'empty');

  const off = world('G4OFF', ['O1']);
  addRow('G4OFF', 'O1');
  assert.equal(await resendSticky(off.client, 'G4OFF', 'O1'), 'off');
  assert.equal(off.channel.sent.length + channel.sent.length, 0);
  assert.equal(await resendSticky(client, 'OTHERGUILD', 'C3'), 'missing', 'a row of another guild is not touched');
});

test('resend: missing permissions, and what a failing post does to the saved message id', async () => {
  rows.clear();
  await seed('G5');
  const mute = world('G5', ['C1'], { deny: ['SendMessages'] });
  addRow('G5', 'C1');
  assert.equal(await resendSticky(mute.client, 'G5', 'C1'), 'perms');
  assert.equal(mute.channel.sent.length, 0);

  // An embed needs Embed Links; plain text does not. Manage Messages is never needed.
  const noLinks = world('G5', ['C2'], { deny: ['EmbedLinks', 'ManageMessages'] });
  addRow('G5', 'C2', { content: null, embed: { embed: { title: 'T' }, buttons: [] } });
  assert.equal(await resendSticky(noLinks.client, 'G5', 'C2'), 'perms');
  addRow('G5', 'C3');
  const plain = world('G5', ['C3'], { deny: ['EmbedLinks', 'ManageMessages'] });
  assert.equal(await resendSticky(plain.client, 'G5', 'C3'), 'sent');

  const broken = world('G5', ['C4']);
  addRow('G5', 'C4', { lastMessageId: '123' });
  broken.channel.failSend = 50013;
  assert.equal(await resendSticky(broken.client, 'G5', 'C4'), 'perms');
  assert.equal(rows.get('C4').lastMessageId, null, 'the old message is gone, so its id is forgotten');
  broken.channel.failSend = 10003;
  assert.equal(await resendSticky(broken.client, 'G5', 'C4'), 'gone');
  broken.channel.failSend = 500;
  assert.equal(await resendSticky(broken.client, 'G5', 'C4'), 'failed');
});

test('resend: a sticky removed while its message was on the way is not left behind', async () => {
  rows.clear(); log.length = 0;
  await seed('G6');
  const { client, channel } = world('G6', ['C1']);
  addRow('G6', 'C1');
  hookBeforeUpdate = () => rows.delete('C1'); // /sticky remove lands between the post and the save
  assert.equal(await resendSticky(client, 'G6', 'C1'), 'missing');
  assert.equal(channel.deleted.length, 1, 'the new message was deleted again');
  assert.equal(channel.deleted[0], channel.lastMessageId);
});

// ── Admin operations ──────────────────────────────────────────────────────────

test('setSticky: creates, replaces and posts at once; the previous message goes first', async () => {
  rows.clear(); log.length = 0;
  await seed('G7');
  const { client, guild, channel } = world('G7', ['C1']);
  const created = await setSticky({ client, guild, channel, userId: 'ADMIN', content: 'First' });
  assert.deepEqual({ ...created }, { created: true, hasEmbed: false, status: 'sent' });
  assert.equal(rows.get('C1').content, 'First');
  assert.equal(rows.get('C1').createdById, 'ADMIN');
  assert.equal(rows.get('C1').enabled, true);
  const firstId = rows.get('C1').lastMessageId;

  log.length = 0;
  const updated = await setSticky({ client, guild, channel, userId: 'OTHER', content: 'Second' });
  assert.equal(updated.created, false);
  assert.equal(rows.get('C1').content, 'Second');
  assert.equal(rows.get('C1').createdById, 'ADMIN', 'the author stays');
  assert.deepEqual(log, [`delete ${firstId}`, 'send']);
  assert.equal(peekChannels('G7'), null, 'the cache was dropped');
});

test('setSticky: refuses empty, too long, unusable channels and the per-server cap', async () => {
  rows.clear();
  await seed('G8');
  const { client, guild, channel } = world('G8', ['C1']);
  const refused = async (args) => {
    try { await setSticky({ client, guild, channel, userId: 'A', ...args }); } catch (err) { return err; }
    return null;
  };
  const empty = await refused({ content: '   ' });
  assert.ok(empty instanceof StickyError);
  assert.equal(empty.code, 'INVALID');
  assert.match(empty.message, /empty/i);
  const long = await refused({ content: 'x'.repeat(2001) });
  assert.equal(long.code, 'TOO_LONG');
  assert.equal(rows.size, 0, 'nothing saved');

  const mute = world('G8', ['M1'], { deny: ['SendMessages'] });
  const perms = await refused({ client: mute.client, guild: mute.guild, channel: mute.channel, content: 'hi' });
  assert.equal(perms.code, 'PERMS');
  assert.match(perms.message, /Send Messages/);
  const links = await refused({ channel: world('G8', ['M2'], { deny: ['EmbedLinks'] }).channel, content: null, embed: { embed: { title: 'T' }, buttons: [] } });
  assert.equal(links.code, 'PERMS');
  assert.equal(rows.size, 0);

  for (let i = 0; i < LIMITS.STICKY_MESSAGES; i += 1) addRow('G8', `FULL${i}`);
  const limit = await refused({ content: 'one too many' });
  assert.equal(limit.code, 'LIMIT');
  assert.match(limit.message, new RegExp(String(LIMITS.STICKY_MESSAGES)));
  assert.equal(rows.has('C1'), false);
  // An existing sticky can still be edited at the cap.
  addRow('G8', 'C1');
  const edited = await setSticky({ client, guild, channel, userId: 'A', content: 'edit' });
  assert.equal(edited.created, false);
});

test('setSticky: the form edits the text and keeps the embed; a plain set replaces both', async () => {
  rows.clear();
  await seed('G9');
  const { client, guild, channel } = world('G9', ['C1']);
  const part = { embed: { title: 'Rules' }, buttons: [] };
  const withEmbed = await setSticky({ client, guild, channel, userId: 'A', content: 'Hello', embed: part });
  assert.equal(withEmbed.hasEmbed, true);
  assert.equal(rows.get('C1').embed.embed.title, 'Rules');

  const edited = await setSticky({ client, guild, channel, userId: 'A', content: 'New text', keepEmbed: true });
  assert.equal(edited.hasEmbed, true);
  assert.equal(rows.get('C1').content, 'New text');
  assert.equal(rows.get('C1').embed.embed.title, 'Rules');
  const embedOnly = await setSticky({ client, guild, channel, userId: 'A', content: '', keepEmbed: true });
  assert.equal(embedOnly.status, 'sent', 'the embed alone is enough');
  assert.equal(rows.get('C1').content, null);

  const replaced = await setSticky({ client, guild, channel, userId: 'A', content: 'Plain', embed: null });
  assert.equal(replaced.hasEmbed, false);
  assert.equal(rows.get('C1').embed, null);
});

test('setSticky saves while the module is off but posts nothing; a pending re-send is dropped', async () => {
  rows.clear();
  await seed('G10', false);
  const { client, guild, channel } = world('G10', ['C1']);
  noteMessage(client, 'G10', 'C1');
  assert.equal(_scheduler.has('C1'), true);
  const result = await setSticky({ client, guild, channel, userId: 'A', content: 'Soon' });
  assert.equal(result.status, 'off');
  assert.equal(channel.sent.length, 0);
  assert.equal(rows.get('C1').content, 'Soon');
  assert.equal(_scheduler.has('C1'), false);
});

test('removeSticky, pause and resume', async () => {
  rows.clear(); log.length = 0;
  await seed('G11');
  const { client, guild, channel } = world('G11', ['C1', 'C2']);
  await setSticky({ client, guild, channel, userId: 'A', content: 'One' });
  const posted = rows.get('C1').lastMessageId;

  const paused = await setStickyEnabled(client, 'G11', 'C1', false);
  assert.deepEqual({ ...paused }, { found: true, changed: true, status: 'paused' });
  assert.equal(rows.get('C1').enabled, false);
  assert.equal(rows.get('C1').lastMessageId, null);
  assert.deepEqual(channel.deleted, [posted], 'the posted message was deleted');
  assert.equal((await setStickyEnabled(client, 'G11', 'C1', false)).changed, false, 'pausing twice changes nothing');
  assert.equal((await getChannels('G11')).get('C1'), false);

  const resumed = await setStickyEnabled(client, 'G11', 'C1', true);
  assert.deepEqual({ ...resumed }, { found: true, changed: true, status: 'sent' });
  assert.equal(rows.get('C1').enabled, true);
  assert.ok(rows.get('C1').lastMessageId);
  assert.deepEqual(await setStickyEnabled(client, 'G11', 'C2', true), { found: false, changed: false, status: null });

  const live = rows.get('C1').lastMessageId;
  noteMessage(client, 'G11', 'C1');
  const removed = await removeSticky(client, 'G11', 'C1');
  assert.equal(removed.channelId, 'C1');
  assert.equal(rows.has('C1'), false);
  assert.ok(channel.deleted.includes(live), 'its message went with it');
  assert.equal(_scheduler.has('C1'), false);
  assert.equal(await removeSticky(client, 'G11', 'C1'), null);
});

// ── Hooks ─────────────────────────────────────────────────────────────────────

test('mayStick: only runs for sticky channels; unknown guilds pass and are loaded by the handler', async () => {
  rows.clear();
  await seed('H1'); await seed('H1OFF', false);
  addRow('H1', 'C1'); addRow('H1', 'C2', { enabled: false });
  invalidate('H1');
  const msg = (channelId) => ({ channelId, author: { id: 'U' } });

  assert.equal(mayStick(ctxFor('H1', msg('C1'))), true, 'no memo yet: the handler decides');
  await getChannels('H1');
  assert.equal(mayStick(ctxFor('H1', msg('C1'))), true);
  assert.equal(mayStick(ctxFor('H1', msg('C2'))), false, 'paused');
  assert.equal(mayStick(ctxFor('H1', msg('C3'))), false, 'not a sticky channel');
  assert.equal(mayStick(ctxFor('H1OFF', msg('C1'))), false, 'module off');
});

test('onMessage: schedules a re-send for a sticky channel only, and ordinary chat costs no query once cached', async () => {
  rows.clear();
  await seed('H2');
  const { client } = world('H2', ['C1']);
  addRow('H2', 'C1');
  invalidate('H2');
  calls.findMany = 0;
  const ctx = (channelId) => ({ guildId: 'H2', client, message: { channelId } });
  const idle = _scheduler.size;

  await onMessage(ctx('OTHER'));
  assert.equal(_scheduler.size, idle, 'no sticky there');
  assert.equal(calls.findMany, 1);
  for (let i = 0; i < 20; i += 1) await onMessage(ctx(i % 2 ? 'OTHER' : 'ELSEWHERE'));
  assert.equal(calls.findMany, 1, 'twenty ordinary messages, no further query');
  assert.equal(mayStick(ctx('OTHER')), false, 'and the filter rejects them before any handler runs');

  await onMessage(ctx('C1'));
  assert.equal(_scheduler.size, idle + 1);
  assert.equal(_scheduler.has('C1'), true);
  cancelPending('C1');
});

test('messageDelete: only the sticky message clears its id; our own deletes cost nothing', async () => {
  rows.clear();
  addRow('H3', 'C1', { lastMessageId: '100' });
  assert.equal(await clearLastMessage('H3', 'C1', '999'), 0, 'some other message');
  assert.equal(rows.get('C1').lastMessageId, '100');
  assert.equal(await clearLastMessage('H3', 'C1', '100'), 1);
  assert.equal(rows.get('C1').lastMessageId, null);

  // The delete our re-send makes must never clear the id of the message that replaced it.
  await seed('H3');
  const { client, channel } = world('H3', ['C1']);
  rows.get('C1').lastMessageId = '100';
  await resendSticky(client, 'H3', 'C1');
  const fresh = rows.get('C1').lastMessageId;
  assert.notEqual(fresh, '100');
  assert.equal(await clearLastMessage('H3', 'C1', '100'), 0, 'our own delete: skipped');
  assert.equal(rows.get('C1').lastMessageId, fresh);
  assert.equal(channel.deleted[0], '100');

  rows.get('C1').lastMessageId = '300';
  await onMessageDelete({ guildId: 'H3', message: { id: '300', channelId: 'C1' } });
  assert.equal(rows.get('C1').lastMessageId, null);
});

test('messageDelete / channelDelete filters', async () => {
  rows.clear();
  addRow('H4', 'C1');
  invalidate('H4');
  const own = { id: '1', channelId: 'C1', author: { id: 'BOT' } };
  const member = { id: '2', channelId: 'C1', author: { id: 'U' } };
  const partial = { id: '3', channelId: 'C1', partial: true };
  assert.equal(mayBeSticky(ctxFor('H4', own)), true);
  assert.equal(mayBeSticky(ctxFor('H4', partial)), true, 'an uncached message may be the sticky');
  assert.equal(mayBeSticky(ctxFor('H4', member)), false, 'a member\'s message never was');
  await getChannels('H4');
  assert.equal(mayBeSticky(ctxFor('H4', { ...own, channelId: 'ELSE' })), false);
  assert.equal(mayHaveSticky(ctxFor('H4', null, { channel: { id: 'C1' } })), true);
  assert.equal(mayHaveSticky(ctxFor('H4', null, { channel: { id: 'ELSE' } })), false);
});

test('channelDelete drops the row, the pending re-send and the cached entry', async () => {
  rows.clear();
  addRow('H5', 'C1'); addRow('H5', 'C2');
  await getChannels('H5');
  noteMessage({}, 'H5', 'C1');
  await onChannelDelete({ guildId: 'H5', channel: { id: 'C1' } });
  assert.equal(rows.has('C1'), false);
  assert.equal(rows.has('C2'), true);
  assert.equal(_scheduler.has('C1'), false);
  assert.equal(peekChannels('H5'), null);
  assert.equal(await dropChannel('H5', 'C1'), 0, 'idempotent');
  assert.equal(await dropChannel('H5', null), 0);
});

// ── Plugin declaration ────────────────────────────────────────────────────────

test('plugin declaration, hook order and the modal component', async () => {
  assert.equal(plugin.id, 'sticky');
  assert.equal(plugin.module, 'sticky');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id, h.always === true]), [
    ['messageCreate', 'sticky', false],
    ['messageDelete', 'sticky', true],
    ['channelDelete', 'sticky', true],
  ]);
  assert.equal(HOOK_ORDER.messageCreate.at(-1), 'sticky', 'last, after automod');
  assert.ok(HOOK_ORDER.messageDelete.includes('sticky'));
  assert.ok(HOOK_ORDER.channelDelete.includes('sticky'));
  for (const h of plugin.hooks) assert.equal(typeof h.run, 'function');

  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix, c.module]), [['modal', 'sticky-modal', null]]);
  assert.equal(CUSTOM_ID_PREFIXES['sticky-modal'].owner, 'sticky');
  assert.equal(CUSTOM_ID_PREFIXES['sticky-content'].kind, 'field');
  assert.deepEqual(plugin.jobs, []);

  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'sticky'), 'collectPlugins keeps it');
  assert.deepEqual(errors.filter((e) => /sticky/i.test(String(e?.message ?? e))), []);
});

test('the message hook only schedules: it returns without waiting for the quiet period', async () => {
  rows.clear();
  await seed('H6');
  addRow('H6', 'C1');
  invalidate('H6');
  const hook = plugin.hooks.find((h) => h.event === 'messageCreate');
  const started = Date.now();
  await hook.run({ guildId: 'H6', client: world('H6', ['C1']).client, message: { channelId: 'C1' } });
  assert.ok(Date.now() - started < STICKY.QUIET_MS / 2, 'well inside the hook timeout');
  assert.equal(_scheduler.has('C1'), true);
  cancelPending('C1');
});

// ── Modal ─────────────────────────────────────────────────────────────────────

test('the form: one paragraph prefilled with the current text', () => {
  const json = buildStickyModal('C42', 'Be kind').toJSON();
  assert.equal(json.custom_id, 'sticky-modal:C42');
  assert.equal(json.components.length, 1);
  const input = json.components[0].components[0];
  assert.equal(input.custom_id, 'sticky-content');
  assert.equal(input.style, 2);
  assert.equal(input.max_length, 2000);
  assert.equal(input.required, false);
  assert.equal(input.value, 'Be kind');
  assert.equal(buildStickyModal('C42').toJSON().components[0].components[0].value, undefined);
  assert.equal(buildStickyModal('C42', 'x'.repeat(3000)).toJSON().components[0].components[0].value.length, 2000);
});

function fakeInteraction(extra = {}) {
  const i = {
    createdTimestamp: Date.now(), deferred: false, replied: false, replies: [], edits: [], modals: [],
    user: { id: 'ADMIN' }, ephemeral: undefined,
    async reply(p) { this.replied = true; this.replies.push(p); return { resource: { message: {} } }; },
    async deferReply({ ephemeral } = {}) { this.deferred = true; this.ephemeral = Boolean(ephemeral); },
    async editReply(p) { this.edits.push(p); return {}; },
    async followUp(p) { this.edits.push(p); return {}; },
    async showModal(m) { this.modals.push(m); },
    ...extra,
  };
  return i;
}
// The embed that answered, with the status emoji the embed builders put before the title taken off.
const answer = (i) => {
  const json = (i.edits.at(-1) ?? i.replies.at(-1)).embeds[0].toJSON();
  return { ...json, title: json.title.replace(/^(?:✅|❌|⚠️|ℹ️|📌)\s+/u, '') };
};

test('the form submit: Admin level only, edits the text, keeps the embed, works while the module is off', async () => {
  rows.clear();
  await seed('M1', false);
  const { client, guild } = world('M1', ['C1']);
  const member = (admin) => ({ id: 'ADMIN', guild: { ownerId: 'x' }, permissions: { has: (bit) => admin && bit === PermissionFlagsBits.ManageGuild } });
  const submit = (admin, fields = { 'sticky-content': 'Hello {server}' }, customId = 'sticky-modal:C1') => fakeInteraction({
    customId, guildId: 'M1', guild, client, member: member(admin), inCachedGuild: () => true,
    fields: { getTextInputValue: (id) => { if (!(id in fields)) throw new Error('no such field'); return fields[id]; } },
  });

  const denied = submit(false);
  await handleStickyModal(denied);
  assert.equal(answer(denied).title, 'Insufficient Permissions');
  assert.equal(denied.replies[0].ephemeral, true);
  assert.equal(rows.size, 0, 'nothing saved');

  const ok = submit(true);
  await handleStickyModal(ok);
  assert.equal(ok.deferred, true);
  assert.equal(answer(ok).title, 'Sticky Message Set');
  assert.match(answer(ok).description, /module is off/);
  assert.deepEqual(ok.edits[0].allowedMentions, { parse: [] });
  assert.equal(rows.get('C1').content, 'Hello {server}');

  rows.get('C1').embed = { embed: { title: 'Rules' }, buttons: [] };
  const edit = submit(true, { 'sticky-content': 'Edited' });
  await handleStickyModal(edit);
  assert.equal(answer(edit).title, 'Sticky Message Updated');
  assert.equal(rows.get('C1').embed.embed.title, 'Rules', 'the embed stays');

  const empty = submit(true, { 'sticky-content': '' });
  rows.get('C1').embed = null;
  await handleStickyModal(empty);
  assert.equal(answer(empty).title, 'Nothing To Stick');

  const stale = submit(true, {});
  await handleStickyModal(stale);
  assert.equal(answer(stale).title, 'Form Expired');
  const gone = submit(true, { 'sticky-content': 'x' }, 'sticky-modal:DELETED');
  await handleStickyModal(gone);
  assert.equal(answer(gone).title, 'Channel Not Found');
  const dm = fakeInteraction({ customId: 'sticky-modal:C1', inCachedGuild: () => false });
  await handleStickyModal(dm);
  assert.equal(answer(dm).title, 'Form Expired');
});

test('the answer warns about variables a sticky cannot fill in, and stays quiet otherwise', async () => {
  rows.clear();
  await seed('M2', true);
  const { client, guild } = world('M2', ['C1']);
  const member = { id: 'ADMIN', guild: { ownerId: 'x' }, permissions: { has: (bit) => bit === PermissionFlagsBits.ManageGuild } };
  const submit = (text) => fakeInteraction({
    customId: 'sticky-modal:C1', guildId: 'M2', guild, client, member, inCachedGuild: () => true,
    fields: { getTextInputValue: () => text },
  });

  const fine = submit('Welcome to {server}, see {channel}');
  await handleStickyModal(fine);
  assert.doesNotMatch(answer(fine).description, /⚠️.*variables|posted as written/);

  const user = submit('Hi {user}, welcome to {server}');
  await handleStickyModal(user);
  assert.equal(answer(user).title, 'Sticky Message Updated');
  assert.match(answer(user).description, /no member, so user variables like `\{user\}` are posted as written/);
  assert.doesNotMatch(answer(user).description, /Unknown variables/);

  const unknown = submit('Hello {nope}');
  await handleStickyModal(unknown);
  assert.match(answer(unknown).description, /Unknown variables are shown as typed: `\{nope\}`/);
});

// ── /sticky ───────────────────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/sticky payload and declaration', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'sticky');
  assert.equal(json.default_member_permissions, '32');
  assert.deepEqual(json.contexts, [0]);
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['set', 1], ['remove', 1], ['list', 1], ['pause', 1], ['resume', 1]]);
  const sub = Object.fromEntries(json.options.map((o) => [o.name, o]));
  assert.deepEqual(sub.set.options.map((o) => [o.name, o.type, !!o.required]), [['channel', 7, true], ['text', 3, false], ['embed', 3, false]]);
  assert.deepEqual(sub.set.options[0].channel_types, [0, 5], 'text and announcement channels');
  assert.equal(sub.set.options[1].max_length, 2000);
  assert.equal(sub.set.options[2].autocomplete, true);
  for (const name of ['remove', 'pause', 'resume']) assert.deepEqual(sub[name].options.map((o) => [o.name, !!o.required]), [['channel', true]]);
  assert.deepEqual(sub.list.options ?? [], []);
  assert.ok(charCount(json) <= 1000, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'sticky');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(typeof cmd.autocomplete, 'function');
});

function slash(sub, { guildId, guild, client, channel = null, text: t = null, embed = null } = {}) {
  return fakeInteraction({
    guildId, guild, client,
    options: {
      getSubcommand: () => sub,
      getChannel: () => ({ id: channel }),
      getString: (name) => (name === 'text' ? t : embed),
    },
  });
}

test('/sticky set without text opens the form first (no defer); refusals answer instead', async () => {
  rows.clear();
  await seed('S1');
  const { client, guild } = world('S1', ['C1']);
  guild.channels.cache.set('NOSEND', fakeChannel('S1', 'NOSEND', { deny: ['SendMessages'], guild }));

  addRow('S1', 'C1', { content: 'Current text' });
  const i = slash('set', { guildId: 'S1', guild, client, channel: 'C1' });
  await cmd.execute(i);
  assert.equal(i.deferred, false, 'showModal is the first answer');
  assert.equal(i.modals.length, 1);
  const modal = i.modals[0].toJSON();
  assert.equal(modal.custom_id, 'sticky-modal:C1');
  assert.equal(modal.components[0].components[0].value, 'Current text', 'prefilled');

  const blocked = slash('set', { guildId: 'S1', guild, client, channel: 'NOSEND' });
  await cmd.execute(blocked);
  assert.equal(blocked.modals.length, 0);
  assert.equal(answer(blocked).title, 'Cannot Use That Channel');

  for (let n = 0; n < LIMITS.STICKY_MESSAGES; n += 1) addRow('S1', `FULL${n}`);
  guild.channels.cache.set('NEW', fakeChannel('S1', 'NEW', { guild }));
  const full = slash('set', { guildId: 'S1', guild, client, channel: 'NEW' });
  await cmd.execute(full);
  assert.equal(full.modals.length, 0);
  assert.equal(answer(full).title, 'Limit Reached');

  const unknown = slash('set', { guildId: 'S1', guild, client, channel: 'NOT-IN-CACHE' });
  await cmd.execute(unknown);
  assert.equal(unknown.modals.length, 0);
});

test('/sticky set with text, and with a saved embed', async () => {
  rows.clear(); savedEmbeds = {};
  await seed('S2');
  const { client, guild, channel } = world('S2', ['C1', 'C2']);

  const plain = slash('set', { guildId: 'S2', guild, client, channel: 'C1', text: 'Line one\\nLine two' });
  await cmd.execute(plain);
  assert.equal(plain.deferred, true);
  assert.equal(answer(plain).title, 'Sticky Message Set');
  assert.equal(rows.get('C1').content, 'Line one\nLine two', 'a typed \\n is a line break');
  assert.equal(text(channel.sent[0]), 'Line one\nLine two');

  const missing = slash('set', { guildId: 'S2', guild, client, channel: 'C2', embed: 'nope' });
  await cmd.execute(missing);
  assert.equal(answer(missing).title, 'Embed Not Found');
  assert.equal(rows.has('C2'), false);

  savedEmbeds.rules = { template: { content: 'Saved text', embed: { title: 'Server rules', description: 'Be kind' }, buttons: [] } };
  const withEmbed = slash('set', { guildId: 'S2', guild, client, channel: 'C2', embed: 'rules' });
  await cmd.execute(withEmbed);
  assert.equal(answer(withEmbed).title, 'Sticky Message Set');
  assert.match(answer(withEmbed).description, /with an embed/);
  assert.equal(rows.get('C2').embed.embed.title, 'Server rules');
  assert.equal(rows.get('C2').content, 'Saved text', 'the saved embed\'s own text is used when none is typed');
  assert.equal(guild.channels.cache.get('C2').sent[0].embeds[0].toJSON().title, 'Server rules');
});

test('/sticky remove, pause, resume and list', async () => {
  rows.clear();
  await seed('S3');
  const { client, guild, channel } = world('S3', ['C1', 'C2']);
  const run = async (sub, channelId) => {
    const i = slash(sub, { guildId: 'S3', guild, client, channel: channelId });
    await cmd.execute(i);
    return answer(i);
  };

  assert.equal((await run('remove', 'C1')).title, 'No Sticky Message');
  assert.equal((await run('pause', 'C1')).title, 'No Sticky Message');
  assert.equal((await run('resume', 'C1')).title, 'No Sticky Message');

  await setSticky({ client, guild, channel, userId: 'A', content: 'Rules `here` {server}' });
  await setSticky({ client, guild, channel: guild.channels.cache.get('C2'), userId: 'A', content: null, embed: { embed: { title: 'T' }, buttons: [] } });
  const paused = await run('pause', 'C2');
  assert.equal(paused.title, 'Sticky Message Paused');
  assert.match(paused.description, /deleted/);
  assert.match((await run('pause', 'C2')).description, /already paused/);

  const list = await run('list');
  assert.equal(list.title, 'Sticky Messages');
  assert.match(list.description, /✅ <#C1> `Rules here \{server\}`/);
  assert.match(list.description, /⏸️ <#C2> 🖼️ embed/);
  assert.equal(list.footer.text, `2 of ${LIMITS.STICKY_MESSAGES} used`);

  const resumed = await run('resume', 'C2');
  assert.equal(resumed.title, 'Sticky Message Resumed');
  assert.match(resumed.description, /posted again/);

  // A row whose channel is gone is cleaned up when listing.
  addRow('S3', 'DEAD');
  const after = await run('list');
  assert.doesNotMatch(after.description, /DEAD/);
  assert.equal(rows.has('DEAD'), false);

  const removed = await run('remove', 'C1');
  assert.equal(removed.title, 'Sticky Message Removed');
  assert.equal(rows.has('C1'), false);
  assert.ok(channel.deleted.length >= 1, 'its message was deleted');

  rows.clear();
  const empty = await run('list');
  assert.match(empty.description, /No sticky messages yet/);
});

test('/sticky list, pause and resume say when the module is off', async () => {
  rows.clear();
  await seed('S4', false);
  const { client, guild, channel } = world('S4', ['C1']);
  await setSticky({ client, guild, channel, userId: 'A', content: 'Soon' });
  const run = async (sub, channelId) => {
    const i = slash(sub, { guildId: 'S4', guild, client, channel: channelId });
    await cmd.execute(i);
    return answer(i);
  };
  assert.match((await run('list')).description, /module is off/);
  assert.match((await run('resume', 'C1')).description, /module is off/);
});

test('/sticky autocomplete suggests saved embeds', async () => {
  const responded = [];
  const base = {
    guildId: 'AC', responded: false,
    respond: async (choices) => { responded.push(choices); },
  };
  await cmd.autocomplete({ ...base, options: { getFocused: () => ({ name: 'text', value: '' }) } });
  assert.deepEqual(responded.pop(), []);
  const realFindMany = prisma.savedEmbed.findMany;
  prisma.savedEmbed.findMany = async () => [{ id: '1', name: 'rules' }, { id: '2', name: 'rules-long' }];
  try {
    await cmd.autocomplete({ ...base, options: { getFocused: () => ({ name: 'embed', value: 'ru' }) } });
  } finally {
    prisma.savedEmbed.findMany = realFindMany;
  }
  assert.deepEqual(responded.pop(), [{ name: 'rules', value: 'rules' }, { name: 'rules-long', value: 'rules-long' }]);
});
