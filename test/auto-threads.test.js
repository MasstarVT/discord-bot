// Auto Threads tests. Offline (no Discord, Postgres or Redis): the list parsing and the memo, thread names and the
// markup stripping, the crosspost limiter, thread creation and publishing against fake channels and messages, the hook
// filters and handlers, the list changes against an in-memory GuildSettings row, the Close button, the plugin
// declaration and the /autothread payload and replies.
import { test, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { ChannelType, MessageFlags, MessageType, PermissionFlagsBits } from 'discord.js';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const svc = await import('../src/services/autoThreadService.js');
const at = await import('../src/plugins/auto-threads.js');
const cmd = await import('../src/commands/admin/autothread.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { CUSTOM_ID_PREFIXES } = await import('../src/config/customIds.js');
const { PERMISSION_LEVELS, LIMITS } = await import('../src/config/constants.js');
const { getSettings, invalidateSettings } = await import('../src/services/settingsService.js');

const {
  AUTOTHREAD, normalizeEntry, readEntries, readPublishIds, channelIndex, listedChannel, plainText, contentTitle, threadName,
  unknownTokens, threadNeeds, createPublishLimiter, isPublishable, createThread, publishMessage, settled, closeRow,
  setThreadChannel, removeThreadChannel, addPublishChannel, removePublishChannel, dropChannel,
} = svc;
const { mayHandle, mayPublish, mayBeListed, onMessage, onBotMessage, onChannelDelete, handleClose, plugin } = at;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory GuildSettings ───────────────────────────────────────────────────

const settingsRows = new Map();
const stats = { updates: 0 };
const realGuildSettings = prisma.guildSettings;
const realTransaction = prisma.$transaction;
const settingsStub = {
  findUnique: async ({ where }) => (settingsRows.has(where.guildId) ? structuredClone(settingsRows.get(where.guildId)) : null),
  upsert: async ({ where, update, create }) => {
    const row = { ...(settingsRows.get(where.guildId) ?? { guildId: where.guildId, ...create }), ...update };
    settingsRows.set(where.guildId, row);
    return structuredClone(row);
  },
  update: async ({ where, data }) => {
    stats.updates++;
    const row = { ...settingsRows.get(where.guildId), ...data };
    settingsRows.set(where.guildId, row);
    return structuredClone(row);
  },
};
prisma.guildSettings = settingsStub;
prisma.$transaction = async (fn) => fn({
  $executeRaw: async () => 0,
  $queryRaw: async () => [],
  guildSettings: settingsStub,
});

after(async () => {
  prisma.guildSettings = realGuildSettings;
  prisma.$transaction = realTransaction;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

beforeEach(() => {
  stats.updates = 0;
  svc._publishLimiter.clear();
});

let guildCounter = 0;
const newGuildId = () => `9${String(++guildCounter).padStart(17, '0')}`;

/** Seeds the guild's GuildSettings row, drops the memo and loads it again, so `peekSettings` knows it. */
async function seed(guildId, over = {}) {
  settingsRows.set(guildId, {
    guildId, autoThreadsEnabled: true, autoThreadChannels: [], autoPublishChannelIds: [], embedColor: null, ...over,
  });
  await invalidateSettings(guildId);
  return getSettings(guildId);
}

// ── Fakes ─────────────────────────────────────────────────────────────────────

const discordErr = (code) => Object.assign(new Error(`Discord error ${code}`), { code });
const PERMS = ['ViewChannel', 'SendMessages', 'CreatePublicThreads', 'SendMessagesInThreads', 'ManageMessages', 'ManageThreads'];
let nextId = 100000;
const nextSnowflake = () => String(++nextId);

function fakeChannel({ id = nextSnowflake(), type = ChannelType.GuildText, guildId = 'G', perms = PERMS, name = 'general' } = {}) {
  return {
    id, type, name, guildId, thread: false,
    guild: { id: guildId, members: { me: { id: 'BOT' } } },
    permissionsFor: () => ({ has: (flag) => perms.some((k) => PermissionFlagsBits[k] === flag) }),
    isThread: () => false,
    isTextBased: () => true,
  };
}

function fakeMessage(channel, { content = 'hello there', authorId = 'U1', type = MessageType.Default, flags = [], displayName = 'Alice' } = {}) {
  const message = {
    id: nextSnowflake(), channel, channelId: channel.id, guildId: channel.guildId, guild: channel.guild, type, content, cleanContent: content,
    author: { id: authorId, username: 'alice', globalName: 'Alice G' },
    member: { displayName },
    createdAt: new Date('2026-10-21T23:30:00Z'),
    flags: { has: (f) => flags.includes(f) },
    poll: null, reference: null,
    startCalls: [], crossposted: 0, startError: null, crosspostError: null, thread: null,
    async startThread(options) {
      message.startCalls.push(options);
      if (message.startError) throw message.startError;
      const thread = { id: nextSnowflake(), parent: channel, sent: [], async send(p) { thread.sent.push(p); return { id: nextSnowflake() }; } };
      message.thread = thread;
      return thread;
    },
    async crosspost() {
      if (message.crosspostError) throw message.crosspostError;
      message.crossposted++;
      return message;
    },
  };
  return message;
}

const ctxFor = (guildId, message, settings, extra = {}) => ({
  guildId, message, channel: message.channel, getSettings: async () => settings, ...extra,
});

// ── Reading the lists ─────────────────────────────────────────────────────────

test('normalizeEntry: valid entries keep their shape, bad parts fall back to the defaults', () => {
  assert.deepEqual(normalizeEntry({ channelId: '123456789', name: ' Q: {content} ', archiveMinutes: '60', closeButton: true }),
    { channelId: '123456789', name: 'Q: {content}', archiveMinutes: '60', closeButton: true });
  assert.deepEqual(normalizeEntry({ channelId: '123456789' }),
    { channelId: '123456789', name: '{content}', archiveMinutes: '1440', closeButton: false });
  assert.equal(normalizeEntry({ channelId: '123456789', archiveMinutes: 4320 }).archiveMinutes, '4320', 'a number is read as its string');
  assert.equal(normalizeEntry({ channelId: '123456789', archiveMinutes: '999' }).archiveMinutes, '1440');
  assert.equal(normalizeEntry({ channelId: '123456789', name: '   ' }).name, '{content}');
  assert.equal(normalizeEntry({ channelId: '123456789', name: 'x'.repeat(300) }).name.length, 100);
  assert.equal(normalizeEntry({ channelId: '123456789', closeButton: 'yes' }).closeButton, false);
  for (const bad of [null, undefined, 5, 'str', [], {}, { channelId: 5 }, { channelId: 'abc' }, { channelId: '' }]) {
    assert.equal(normalizeEntry(bad), null, JSON.stringify(bad));
  }
});

test('readEntries and readPublishIds: skip junk, keep the first of a channel, respect the caps', () => {
  const good = (i) => ({ channelId: String(100000 + i), name: `n${i}`, archiveMinutes: '60', closeButton: false });
  const entries = readEntries({ autoThreadChannels: [good(1), null, 'x', { channelId: 'nope' }, { ...good(1), name: 'dupe' }, good(2)] });
  assert.deepEqual(entries.map((e) => e.name), ['n1', 'n2']);
  assert.deepEqual(readEntries(null), []);
  assert.deepEqual(readEntries({ autoThreadChannels: 'oops' }), []);
  const many = Array.from({ length: LIMITS.AUTOTHREAD_CHANNELS + 10 }, (_, i) => good(i));
  assert.equal(readEntries({ autoThreadChannels: many }).length, LIMITS.AUTOTHREAD_CHANNELS);

  assert.deepEqual(readPublishIds({ autoPublishChannelIds: ['123456', '123456', 'bad', 7, '654321'] }), ['123456', '654321']);
  assert.deepEqual(readPublishIds({}), []);
  const ids = Array.from({ length: LIMITS.AUTOPUBLISH_CHANNELS + 5 }, (_, i) => String(200000 + i));
  assert.equal(readPublishIds({ autoPublishChannelIds: ids }).length, LIMITS.AUTOPUBLISH_CHANNELS);
});

test('channelIndex: parsed once per settings object, the empty index for no settings; listedChannel answers from it', () => {
  const settings = Object.freeze({
    autoThreadsEnabled: true,
    autoThreadChannels: Object.freeze([Object.freeze({ channelId: '111111', name: 'a', archiveMinutes: '60', closeButton: false })]),
    autoPublishChannelIds: Object.freeze(['222222']),
  });
  const index = channelIndex(settings);
  assert.equal(channelIndex(settings), index, 'memoised on the settings object');
  assert.notEqual(channelIndex({ ...settings }), index, 'a replaced settings object is parsed again');
  assert.equal(index.threads.get('111111').name, 'a');
  assert.ok(index.publish.has('222222'));
  assert.equal(channelIndex(null).threads.size, 0);
  assert.equal(channelIndex(undefined).publish.size, 0);

  assert.equal(listedChannel(settings, '111111'), true);
  assert.equal(listedChannel(settings, '222222'), true);
  assert.equal(listedChannel(settings, '333333'), false);
  assert.equal(listedChannel({ ...settings, autoThreadsEnabled: false }, '111111'), false, 'module off');
  assert.equal(listedChannel(null, '111111'), undefined, 'unknown settings');
});

// ── Thread names ──────────────────────────────────────────────────────────────

test('plainText: markdown, mention tags, emoji, spoilers and headings are removed', () => {
  assert.equal(plainText('**Bold** and *italic* and ~~gone~~ and __under__'), 'Bold and italic and gone and under');
  assert.equal(plainText('use `code` here'), 'use code here');
  assert.equal(plainText('[the docs](https://example.com/x) please'), 'the docs please');
  assert.equal(plainText('# Heading text'), 'Heading text');
  assert.equal(plainText('> quoted words'), 'quoted words');
  assert.equal(plainText('-# small print'), 'small print');
  assert.equal(plainText('- a bullet'), 'a bullet');
  assert.equal(plainText('hey <@123456789012345678> and <@&123456789012345678> in <#123456789012345678>'), 'hey and in');
  assert.equal(plainText('hello @everyone and @here'), 'hello and');
  assert.equal(plainText('hello @\u200beveryone'), 'hello');
  assert.equal(plainText('thanks <:pog:123456789012345678> <a:wave:123456789012345678>'), 'thanks :pog: :wave:');
  assert.equal(plainText('the answer is ||a big spoiler|| ok'), 'the answer is ok');
  assert.equal(plainText('mail me: a@b.com'), 'mail me: a@b.com', 'an @ inside a word stays');
  assert.equal(plainText('@Bob can you look #general'), 'Bob can you look general', 'names left by cleanContent lose the marks');
  assert.equal(plainText('snake_case_name stays'), 'snake_case_name stays');
  assert.equal(plainText('tab\tand\u0000control'), 'tab and control');
  assert.equal(plainText(null), '');
  assert.ok(plainText('x'.repeat(5000)).length <= AUTOTHREAD.LINE_MAX, 'bounded input');
});

test('contentTitle: the first line that still has words', () => {
  assert.equal(contentTitle('First line\nSecond line'), 'First line');
  assert.equal(contentTitle('\n\n  \n**bold**\nnext'), 'bold');
  assert.equal(contentTitle('<@123456789012345678>\nreal text'), 'real text');
  assert.equal(contentTitle('😀😀'), '');
  assert.equal(contentTitle(''), '');
  assert.equal(contentTitle(undefined), '');
});

test('threadName: {user} {content} {channel} {date}, a clean name of at most 100 characters', () => {
  const channel = fakeChannel({ name: 'help-desk' });
  const base = { member: { displayName: 'Alice' }, channel, guild: { id: 'G', name: 'My Server' }, createdAt: new Date('2026-10-21T23:59:00Z') };
  assert.equal(threadName('{content}', { ...base, content: 'How do I reset?\nmore' }), 'How do I reset?');
  assert.equal(threadName('{user}: {content}', { ...base, content: 'Hi' }), 'Alice: Hi');
  assert.equal(threadName('{channel} {date}', { ...base, content: 'x' }), 'help-desk 2026-10-21', '{channel} is the name, {date} is UTC');
  assert.equal(threadName('{server} - {content}', { ...base, content: 'x' }), 'My Server - x', 'the usual placeholders work too');
  assert.equal(threadName('', { ...base, content: 'fallback template' }), 'fallback template');
  assert.equal(threadName('{content}', { ...base, content: '😀' }), 'Alice', 'nothing left: the display name');
  assert.equal(threadName('{content}', { ...base, content: '', member: null, user: { globalName: 'Glob', username: 'u' } }), 'Glob');
  assert.equal(threadName('{content}', { content: '' }), 'Thread');
  assert.equal(threadName('{content}', { ...base, content: 'y'.repeat(400) }).length <= 100, true);
  assert.equal(threadName('{content}', { ...base, content: 'word '.repeat(80) }).length <= 100, true);
  assert.equal(threadName('{user}', { ...base, member: { displayName: '<@123456789012345678> sneaky' }, content: '' }), 'sneaky');
  // A message that contains a placeholder is not read as one.
  assert.equal(threadName('{content}', { ...base, content: '{user} {server}' }), '{user} {server}');
  assert.equal(threadName('{content}', { ...base, content: 'line one\r\nline two' }), 'line one');
});

test('unknownTokens: only placeholders nothing knows', () => {
  assert.deepEqual(unknownTokens('{user} {content} {channel} {date} {server} {user.name}'), []);
  assert.deepEqual(unknownTokens('{nope} {content}'), ['{nope}']);
  assert.deepEqual(unknownTokens('plain'), []);
});

test('threadNeeds: the close button needs Send Messages in Threads', () => {
  assert.deepEqual(threadNeeds({ closeButton: false }), ['ViewChannel', 'CreatePublicThreads']);
  assert.deepEqual(threadNeeds({ closeButton: true }), ['ViewChannel', 'CreatePublicThreads', 'SendMessagesInThreads']);
});

// ── Crosspost limiter ─────────────────────────────────────────────────────────

test('createPublishLimiter: 10 per channel per sliding hour, counted per channel', () => {
  let now = 1_000_000;
  const limiter = createPublishLimiter({ now: () => now });
  for (let i = 0; i < 10; i++) assert.equal(limiter.take('A'), true, `take ${i}`);
  assert.equal(limiter.take('A'), false, 'the 11th waits');
  assert.equal(limiter.remaining('A'), 0);
  assert.equal(limiter.take('B'), true, 'another channel has its own window');
  now += AUTOTHREAD.PUBLISH_WINDOW_MS - 1;
  assert.equal(limiter.take('A'), false, 'still inside the hour');
  now += 2;
  assert.equal(limiter.take('A'), true, 'the window slid past the first ten');
  assert.equal(limiter.remaining('A'), 9);
});

test('createPublishLimiter: the number of tracked channels is bounded', () => {
  const limiter = createPublishLimiter({ maxChannels: 5 });
  for (let i = 0; i < 50; i++) limiter.take(`C${i}`);
  assert.ok(limiter.size <= 5, `size ${limiter.size}`);
  limiter.clear();
  assert.equal(limiter.size, 0);
});

test('isPublishable: member posts, replies and bot slash replies; not system messages, polls, forwards or crossposts', () => {
  const ch = fakeChannel({ type: ChannelType.GuildAnnouncement });
  assert.equal(isPublishable(fakeMessage(ch)), true);
  assert.equal(isPublishable(fakeMessage(ch, { type: MessageType.Reply })), true);
  assert.equal(isPublishable(fakeMessage(ch, { type: MessageType.ChatInputCommand })), true);
  assert.equal(isPublishable(fakeMessage(ch, { type: MessageType.ChannelPinnedMessage })), false);
  assert.equal(isPublishable(fakeMessage(ch, { flags: [MessageFlags.Crossposted] })), false, 'already published');
  assert.equal(isPublishable(fakeMessage(ch, { flags: [MessageFlags.IsCrosspost] })), false, 'is itself a crosspost');
  assert.equal(isPublishable({ ...fakeMessage(ch), poll: {} }), false);
  assert.equal(isPublishable(null), false);
});

// ── Starting threads ──────────────────────────────────────────────────────────

test('createThread: names the thread, archives per the entry, posts a close button for the starter without pinging', async () => {
  const channel = fakeChannel({ name: 'ideas' });
  const message = fakeMessage(channel, { content: '**Big** idea\nlong text', authorId: 'U42' });
  const entry = { channelId: channel.id, name: '{user}: {content}', archiveMinutes: '4320', closeButton: true };
  assert.equal(await createThread(message, entry), 'created');
  assert.equal(message.startCalls.length, 1);
  assert.deepEqual(message.startCalls[0], { name: 'Alice: Big idea', autoArchiveDuration: 4320, reason: 'Auto Threads: new message' });
  await settled();
  assert.equal(message.thread.sent.length, 1);
  const post = message.thread.sent[0];
  assert.deepEqual(post.allowedMentions, { parse: [] });
  const button = post.components[0].toJSON().components[0];
  assert.equal(button.custom_id, 'autothread:close:U42');
  assert.equal(button.label, 'Close thread');
  assert.ok(CUSTOM_ID_PREFIXES.autothread, 'the prefix is registered');
});

test('createThread: no close button unless the entry asks for one', async () => {
  const channel = fakeChannel();
  const message = fakeMessage(channel);
  assert.equal(await createThread(message, { name: '{content}', archiveMinutes: '1440', closeButton: false }), 'created');
  await settled();
  assert.equal(message.thread.sent.length, 0);
});

test('createThread: a missing permission skips quietly, without calling Discord', async () => {
  const noThreads = fakeChannel({ perms: ['ViewChannel', 'SendMessages'] });
  const m1 = fakeMessage(noThreads);
  assert.equal(await createThread(m1, { name: '{content}', archiveMinutes: '1440', closeButton: false }), 'perms');
  assert.equal(m1.startCalls.length, 0);

  const noThreadSend = fakeChannel({ perms: ['ViewChannel', 'CreatePublicThreads'] });
  const m2 = fakeMessage(noThreadSend);
  assert.equal(await createThread(m2, { name: '{content}', archiveMinutes: '1440', closeButton: true }), 'perms', 'the button needs Send Messages in Threads');
  assert.equal(m2.startCalls.length, 0);
  const m3 = fakeMessage(noThreadSend);
  assert.equal(await createThread(m3, { name: '{content}', archiveMinutes: '1440', closeButton: false }), 'created', 'without the button it is fine');
});

test('createThread: Discord errors never throw', async () => {
  const entry = { name: '{content}', archiveMinutes: '1440', closeButton: false };
  for (const [code, expected] of [[160004, 'failed'], [10008, 'failed'], [50013, 'perms'], [50001, 'perms'], [500, 'failed']]) {
    const message = fakeMessage(fakeChannel());
    message.startError = discordErr(code);
    assert.equal(await createThread(message, entry), expected, String(code));
  }
  const plain = fakeMessage(fakeChannel());
  plain.startError = new Error('network');
  assert.equal(await createThread(plain, entry), 'failed');
  // A thread channel is never threaded again.
  const inThread = fakeChannel();
  inThread.isThread = () => true;
  assert.equal(await createThread(fakeMessage(inThread), entry), 'skipped');
});

test('a failing close-button post is logged, never unhandled', async () => {
  const channel = fakeChannel();
  const message = fakeMessage(channel);
  message.startThread = async () => ({ id: '1', parent: channel, send: async () => { throw discordErr(50013); } });
  assert.equal(await createThread(message, { name: '{content}', archiveMinutes: '1440', closeButton: true }), 'created');
  await settled();
});

// ── Publishing ────────────────────────────────────────────────────────────────

test('publishMessage: crossposts a post in an announcement channel', async () => {
  const channel = fakeChannel({ type: ChannelType.GuildAnnouncement });
  const message = fakeMessage(channel);
  assert.equal(await publishMessage(message), 'published');
  assert.equal(message.crossposted, 1);
});

test('publishMessage: skips text channels, published posts and posts it may not publish', async () => {
  const text = fakeMessage(fakeChannel({ type: ChannelType.GuildText }));
  assert.equal(await publishMessage(text), 'skipped');
  assert.equal(text.crossposted, 0);

  const ann = fakeChannel({ type: ChannelType.GuildAnnouncement });
  const done = fakeMessage(ann, { flags: [MessageFlags.Crossposted] });
  assert.equal(await publishMessage(done), 'skipped');

  const noManage = fakeChannel({ type: ChannelType.GuildAnnouncement, perms: ['ViewChannel', 'SendMessages'] });
  const m = fakeMessage(noManage);
  assert.equal(await publishMessage(m), 'perms', 'posts by others need Manage Messages');
  assert.equal(m.crossposted, 0);
});

test('publishMessage: past the hourly limit the post stays unpublished and no request is made', async () => {
  const channel = fakeChannel({ type: ChannelType.GuildAnnouncement });
  const limiter = createPublishLimiter({ max: 3 });
  const results = [];
  const messages = [];
  for (let i = 0; i < 5; i++) {
    const m = fakeMessage(channel);
    messages.push(m);
    results.push(await publishMessage(m, { limiter }));
  }
  assert.deepEqual(results, ['published', 'published', 'published', 'limited', 'limited']);
  assert.deepEqual(messages.map((m) => m.crossposted), [1, 1, 1, 0, 0]);
  // A refused slot (no permission) is not burned.
  const noManage = fakeChannel({ type: ChannelType.GuildAnnouncement, perms: ['ViewChannel'] });
  await publishMessage(fakeMessage(noManage), { limiter });
  assert.equal(limiter.remaining(noManage.id), 3);
});

test('publishMessage: a message type Discord refuses (50068) gives its slot back', async () => {
  const channel = fakeChannel({ type: ChannelType.GuildAnnouncement });
  const limiter = createPublishLimiter({ max: 3 });
  const bad = fakeMessage(channel, { type: MessageType.ChatInputCommand });
  bad.crosspostError = discordErr(50068);
  assert.equal(await publishMessage(bad, { limiter }), 'failed');
  assert.equal(limiter.remaining(channel.id), 3, 'the slot is not spent');
  const ok = fakeMessage(channel);
  assert.equal(await publishMessage(ok, { limiter }), 'published');
  assert.equal(limiter.remaining(channel.id), 2);
});

test('publishMessage: Discord errors never throw', async () => {
  const channel = fakeChannel({ type: ChannelType.GuildAnnouncement });
  for (const [code, expected] of [[40033, 'failed'], [10008, 'failed'], [50068, 'failed'], [50013, 'perms'], [429, 'failed']]) {
    const m = fakeMessage(channel);
    m.crosspostError = discordErr(code);
    assert.equal(await publishMessage(m, { limiter: createPublishLimiter() }), expected, String(code));
  }
});

// ── Hook filters ──────────────────────────────────────────────────────────────

test('mayHandle: only listed channels once the settings are known; everything while they are not', async () => {
  const guildId = newGuildId();
  const listed = fakeChannel({ guildId });
  const other = fakeChannel({ guildId });
  const announce = fakeChannel({ guildId, type: ChannelType.GuildAnnouncement });
  await invalidateSettings(guildId);
  assert.equal(mayHandle({ guildId, message: fakeMessage(other) }), true, 'no memo: the handler decides');

  await seed(guildId, {
    autoThreadChannels: [{ channelId: listed.id, name: '{content}', archiveMinutes: '1440', closeButton: false }],
    autoPublishChannelIds: [announce.id],
  });
  assert.equal(mayHandle({ guildId, message: fakeMessage(listed) }), true);
  assert.equal(mayHandle({ guildId, message: fakeMessage(announce) }), true, 'a publish channel');
  assert.equal(mayHandle({ guildId, message: fakeMessage(other) }), false, 'an ordinary channel costs nothing');

  const thread = fakeChannel({ guildId, id: listed.id });
  thread.isThread = () => true;
  assert.equal(mayHandle({ guildId, message: fakeMessage(thread) }), false, 'never inside a thread');
  const forum = fakeChannel({ guildId, id: listed.id, type: ChannelType.GuildForum });
  assert.equal(mayHandle({ guildId, message: fakeMessage(forum) }), false, 'never a forum');
  const voice = fakeChannel({ guildId, id: listed.id, type: ChannelType.GuildVoice });
  assert.equal(mayHandle({ guildId, message: fakeMessage(voice) }), false);

  await seed(guildId, {
    autoThreadsEnabled: false,
    autoThreadChannels: [{ channelId: listed.id, name: '{content}', archiveMinutes: '1440', closeButton: false }],
  });
  assert.equal(mayHandle({ guildId, message: fakeMessage(listed) }), false, 'module off');
});

test('mayHandle never touches the database', async () => {
  const guildId = newGuildId();
  const channel = fakeChannel({ guildId });
  await seed(guildId, { autoThreadChannels: [{ channelId: channel.id, name: 'x', archiveMinutes: '60', closeButton: false }] });
  const before = { f: settingsStub.findUnique, u: settingsStub.upsert };
  let calls = 0;
  settingsStub.findUnique = async (...a) => { calls++; return before.f(...a); };
  settingsStub.upsert = async (...a) => { calls++; return before.u(...a); };
  try {
    for (let i = 0; i < 50; i++) mayHandle({ guildId, message: fakeMessage(channel) });
    for (let i = 0; i < 50; i++) mayHandle({ guildId, message: fakeMessage(fakeChannel({ guildId })) });
  } finally {
    settingsStub.findUnique = before.f;
    settingsStub.upsert = before.u;
  }
  assert.equal(calls, 0);
});

test('mayPublish: announcement channels only, and only listed ones once the settings are known', async () => {
  const guildId = newGuildId();
  const announce = fakeChannel({ guildId, type: ChannelType.GuildAnnouncement });
  const unlisted = fakeChannel({ guildId, type: ChannelType.GuildAnnouncement });
  const text = fakeChannel({ guildId });
  await invalidateSettings(guildId);
  assert.equal(mayPublish({ guildId, channel: announce }), true, 'no memo');
  assert.equal(mayPublish({ guildId, channel: text }), false, 'a text channel is never published');
  assert.equal(mayPublish({ guildId, channel: null }), false);
  assert.equal(mayPublish({ guildId, channel: announce, edited: true }), false, 'an edited post is never published again');

  await seed(guildId, { autoPublishChannelIds: [announce.id] });
  assert.equal(mayPublish({ guildId, channel: announce }), true);
  assert.equal(mayPublish({ guildId, channel: unlisted }), false);
  await seed(guildId, { autoThreadsEnabled: false, autoPublishChannelIds: [announce.id] });
  assert.equal(mayPublish({ guildId, channel: announce }), false, 'module off');
});

test('mayBeListed: a deleted channel that is in a list', async () => {
  const guildId = newGuildId();
  const a = fakeChannel({ guildId });
  const b = fakeChannel({ guildId });
  const c = fakeChannel({ guildId });
  await invalidateSettings(guildId);
  assert.equal(mayBeListed({ guildId, channel: a }), true, 'no memo');
  await seed(guildId, {
    autoThreadsEnabled: false, // cleanup also runs while the module is off
    autoThreadChannels: [{ channelId: a.id, name: 'x', archiveMinutes: '60', closeButton: false }],
    autoPublishChannelIds: [b.id],
  });
  assert.equal(mayBeListed({ guildId, channel: a }), true);
  assert.equal(mayBeListed({ guildId, channel: b }), true);
  assert.equal(mayBeListed({ guildId, channel: c }), false);
  assert.equal(mayBeListed({ guildId, channel: null }), false);
});

// ── Hook handlers ─────────────────────────────────────────────────────────────

test('onMessage: starts the thread of a listed channel and publishes in the background', async () => {
  const guildId = newGuildId();
  const channel = fakeChannel({ guildId, type: ChannelType.GuildAnnouncement });
  const settings = await seed(guildId, {
    autoThreadChannels: [{ channelId: channel.id, name: 'T: {content}', archiveMinutes: '60', closeButton: false }],
    autoPublishChannelIds: [channel.id],
  });
  const message = fakeMessage(channel, { content: 'release notes' });
  await onMessage(ctxFor(guildId, message, settings));
  await settled();
  assert.equal(message.startCalls[0].name, 'T: release notes');
  assert.equal(message.startCalls[0].autoArchiveDuration, 60);
  assert.equal(message.crossposted, 1);
});

test('onMessage: the crosspost does not hold the hook up', async () => {
  const guildId = newGuildId();
  const channel = fakeChannel({ guildId, type: ChannelType.GuildAnnouncement });
  const settings = await seed(guildId, { autoPublishChannelIds: [channel.id] });
  const message = fakeMessage(channel);
  let release;
  message.crosspost = () => new Promise((resolve) => { release = () => resolve(message); });
  await onMessage(ctxFor(guildId, message, settings)); // returns although the crosspost is still pending
  assert.equal(typeof release, 'function');
  release();
  await settled();
});

test('onMessage: nothing happens for an unlisted channel, with the module off or without settings', async () => {
  const guildId = newGuildId();
  const channel = fakeChannel({ guildId });
  const settings = await seed(guildId, {});
  const m1 = fakeMessage(channel);
  await onMessage(ctxFor(guildId, m1, settings));
  assert.equal(m1.startCalls.length, 0);

  const off = await seed(guildId, {
    autoThreadsEnabled: false, autoThreadChannels: [{ channelId: channel.id, name: 'x', archiveMinutes: '60', closeButton: false }],
  });
  const m2 = fakeMessage(channel);
  await onMessage(ctxFor(guildId, m2, off));
  assert.equal(m2.startCalls.length, 0);

  const m3 = fakeMessage(channel);
  await onMessage(ctxFor(guildId, m3, null));
  assert.equal(m3.startCalls.length, 0);
  await settled();
});

test('onBotMessage: publishes a bot or webhook post in a listed announcement channel only', async () => {
  const guildId = newGuildId();
  const listed = fakeChannel({ guildId, type: ChannelType.GuildAnnouncement });
  const other = fakeChannel({ guildId, type: ChannelType.GuildAnnouncement });
  const settings = await seed(guildId, { autoPublishChannelIds: [listed.id] });
  const m1 = fakeMessage(listed, { type: MessageType.ChatInputCommand });
  await onBotMessage(ctxFor(guildId, m1, settings));
  const m2 = fakeMessage(other);
  await onBotMessage(ctxFor(guildId, m2, settings));
  await settled();
  assert.equal(m1.crossposted, 1);
  assert.equal(m2.crossposted, 0);
  const off = await seed(guildId, { autoThreadsEnabled: false, autoPublishChannelIds: [listed.id] });
  const m3 = fakeMessage(listed);
  await onBotMessage(ctxFor(guildId, m3, off));
  await settled();
  assert.equal(m3.crossposted, 0, 'module off');
});

test('deferred bot replies: the "thinking" placeholder is never published, the edit that answers it is (review2-B)', async () => {
  const guildId = newGuildId();
  const listed = fakeChannel({ guildId, type: ChannelType.GuildAnnouncement });
  const settings = await seed(guildId, { autoPublishChannelIds: [listed.id] });
  assert.equal(isPublishable(fakeMessage(listed, { type: MessageType.ChatInputCommand, flags: [MessageFlags.Loading] })), false);

  // The placeholder arrives through messageCreate (not edited): the filter passes, the handler publishes nothing.
  const thinking = fakeMessage(listed, { type: MessageType.ChatInputCommand, flags: [MessageFlags.Loading] });
  assert.equal(mayPublish({ guildId, channel: listed }), true);
  await onBotMessage(ctxFor(guildId, thinking, settings));
  await settled();
  assert.equal(thinking.crossposted, 0, 'the placeholder is not published');

  // messageUpdate turns it into the answer: wasLoading true lets that one edit through; other edits stay out.
  const answer = fakeMessage(listed, { type: MessageType.ChatInputCommand });
  assert.equal(mayPublish({ guildId, channel: listed, edited: true, wasLoading: true }), true);
  assert.equal(mayPublish({ guildId, channel: listed, edited: true, wasLoading: false }), false, 'an ordinary edit');
  assert.equal(mayPublish({ guildId, channel: listed, edited: true, wasLoading: null }), false, 'old message unknown');
  assert.equal(mayPublish({ guildId, channel: listed, edited: true }), false);
  await onBotMessage(ctxFor(guildId, answer, settings));
  await settled();
  assert.equal(answer.crossposted, 1, 'the answer is published');
});

// ── Changing the lists ────────────────────────────────────────────────────────

test('setThreadChannel: adds with defaults, then changes only the options given', async () => {
  const guildId = newGuildId();
  await seed(guildId, {});
  const first = await setThreadChannel(guildId, { channelId: '555555' });
  assert.equal(first.created, true);
  assert.deepEqual(first.entry, { channelId: '555555', name: '{content}', archiveMinutes: '1440', closeButton: false });
  assert.equal(first.settings.autoThreadChannels.length, 1);

  const second = await setThreadChannel(guildId, { channelId: '555555', closeButton: true });
  assert.equal(second.created, false);
  assert.deepEqual(second.entry, { channelId: '555555', name: '{content}', archiveMinutes: '1440', closeButton: true });
  const third = await setThreadChannel(guildId, { channelId: '555555', name: 'Q {user}', archiveMinutes: '10080' });
  assert.deepEqual(third.entry, { channelId: '555555', name: 'Q {user}', archiveMinutes: '10080', closeButton: true });
  assert.equal(settingsRows.get(guildId).autoThreadChannels.length, 1, 'updated in place, not duplicated');
});

test('setThreadChannel: other entries stay exactly as the dashboard stored them; the cap is enforced', async () => {
  const guildId = newGuildId();
  const odd = { channelId: '777777', name: 'dash', archiveMinutes: 60, closeButton: true, extra: 'kept' };
  await seed(guildId, { autoThreadChannels: [odd] });
  await setThreadChannel(guildId, { channelId: '888888' });
  assert.deepEqual(settingsRows.get(guildId).autoThreadChannels[0], odd, 'untouched, even the odd number');

  const full = Array.from({ length: LIMITS.AUTOTHREAD_CHANNELS }, (_, i) => ({ channelId: String(300000 + i), name: 'x', archiveMinutes: '60', closeButton: false }));
  await seed(guildId, { autoThreadChannels: full });
  await assert.rejects(setThreadChannel(guildId, { channelId: '999999' }), (err) => err instanceof svc.AutoThreadError && err.code === 'LIMIT');
  assert.equal(settingsRows.get(guildId).autoThreadChannels.length, LIMITS.AUTOTHREAD_CHANNELS);
  // A channel already in a full list can still be changed.
  const changed = await setThreadChannel(guildId, { channelId: '300000', closeButton: true });
  assert.equal(changed.created, false);
});

test('removeThreadChannel: removes one, reports a missing one without writing', async () => {
  const guildId = newGuildId();
  await seed(guildId, { autoThreadChannels: [
    { channelId: '111111', name: 'a', archiveMinutes: '60', closeButton: false },
    { channelId: '222222', name: 'b', archiveMinutes: '60', closeButton: false },
  ] });
  assert.equal(await removeThreadChannel(guildId, '111111'), true);
  assert.deepEqual(settingsRows.get(guildId).autoThreadChannels.map((e) => e.channelId), ['222222']);
  stats.updates = 0;
  assert.equal(await removeThreadChannel(guildId, '111111'), false);
  assert.equal(stats.updates, 0, 'nothing to write');
});

test('publish list: add, repeat, cap, remove', async () => {
  const guildId = newGuildId();
  await seed(guildId, {});
  assert.equal((await addPublishChannel(guildId, '444444')).added, true);
  assert.equal((await addPublishChannel(guildId, '444444')).added, false);
  assert.deepEqual(settingsRows.get(guildId).autoPublishChannelIds, ['444444']);
  assert.equal(await removePublishChannel(guildId, '444444'), true);
  assert.equal(await removePublishChannel(guildId, '444444'), false);

  const full = Array.from({ length: LIMITS.AUTOPUBLISH_CHANNELS }, (_, i) => String(400000 + i));
  await seed(guildId, { autoPublishChannelIds: full });
  await assert.rejects(addPublishChannel(guildId, '999999'), (err) => err instanceof svc.AutoThreadError && err.code === 'LIMIT');
});

test('dropChannel / onChannelDelete: the channel leaves both lists, others stay, and no write when it is in neither', async () => {
  const guildId = newGuildId();
  const keep = { channelId: '222222', name: 'keep', archiveMinutes: '60', closeButton: false };
  const settings = await seed(guildId, {
    autoThreadsEnabled: false, // also while the module is off
    autoThreadChannels: [{ channelId: '111111', name: 'gone', archiveMinutes: '60', closeButton: false }, keep],
    autoPublishChannelIds: ['111111', '333333'],
  });
  const channel = fakeChannel({ guildId, id: '111111' });
  await onChannelDelete({ guildId, channel, getSettings: async () => settings });
  const row = settingsRows.get(guildId);
  assert.deepEqual(row.autoThreadChannels, [keep]);
  assert.deepEqual(row.autoPublishChannelIds, ['333333']);

  stats.updates = 0;
  const fresh = await getSettings(guildId);
  await onChannelDelete({ guildId, channel: fakeChannel({ guildId, id: '999999' }), getSettings: async () => fresh });
  assert.equal(stats.updates, 0, 'a channel in neither list writes nothing');
  assert.equal(await dropChannel(guildId, '999999'), false);
  assert.equal(await dropChannel(guildId, null), false);
});

// ── The Close button ──────────────────────────────────────────────────────────

function fakeThread({ ownerId = 'BOT', archived = false, canLock = true } = {}) {
  const thread = {
    id: 'T1', ownerId, archived, edits: [], isThread: () => true,
    permissionsFor: () => ({ has: (flag) => canLock && flag === PermissionFlagsBits.ManageThreads }),
    async edit(options) { thread.edits.push(options); thread.order.push('edit'); },
    order: [],
  };
  return thread;
}

function fakeClick({ userId = 'U1', starterId = 'U1', thread = fakeThread(), threadPerms = [], level = 'member', guildId = newGuildId() } = {}) {
  const calls = { updates: [], replies: [], followUps: [] };
  const interaction = {
    customId: `autothread:close:${starterId}`, guildId, calls,
    user: { id: userId, username: 'user' },
    member: {
      id: userId, displayName: 'Closer', guild: { ownerId: 'OWNER' },
      permissions: { has: () => false },
      roles: { cache: new Map(level === 'moderator' ? [['MODROLE', {}]] : []) },
    },
    memberPermissions: { has: (flag) => threadPerms.some((k) => PermissionFlagsBits[k] === flag) },
    channel: thread,
    client: { user: { id: 'BOT' } },
    guild: { members: { me: { id: 'BOT' } } },
    deferred: false, replied: false,
    inCachedGuild: () => true,
    async update(payload) { calls.updates.push(payload); interaction.replied = true; thread.order.push('update'); },
    async reply(payload) { calls.replies.push(payload); interaction.replied = true; },
    async followUp(payload) { calls.followUps.push(payload); },
  };
  return { interaction, calls, thread };
}

test('handleClose: the starter closes: the message is edited first, then the thread is archived and locked', async () => {
  const { interaction, calls, thread } = fakeClick();
  await handleClose(interaction);
  assert.equal(calls.updates.length, 1);
  assert.deepEqual(calls.updates[0].components, []);
  assert.match(calls.updates[0].content, /Thread closed by Closer/);
  assert.deepEqual(calls.updates[0].allowedMentions, { parse: [] });
  assert.deepEqual(thread.edits, [{ archived: true, locked: true, reason: 'Closed by U1' }]);
  assert.deepEqual(thread.order, ['update', 'edit']);
  assert.equal(calls.replies.length, 0);
});

test('handleClose: a member with Manage Threads may close it, anyone else may not', async () => {
  const allowed = fakeClick({ userId: 'U2', threadPerms: ['ManageThreads'] });
  await handleClose(allowed.interaction);
  assert.equal(allowed.thread.edits.length, 1);

  const denied = fakeClick({ userId: 'U2' });
  await handleClose(denied.interaction);
  assert.equal(denied.thread.edits.length, 0);
  assert.equal(denied.calls.updates.length, 0);
  assert.equal(denied.calls.replies.length, 1);
  assert.equal(denied.calls.replies[0].ephemeral, true);
  assert.match(denied.calls.replies[0].embeds[0].toJSON().description, /started this thread, or a moderator/);
});

test('handleClose: a bot Moderator may close it', async () => {
  const guildId = newGuildId();
  await seed(guildId, { modRoleIds: ['MODROLE'] });
  const { interaction, thread } = fakeClick({ userId: 'U3', level: 'moderator', guildId });
  await handleClose(interaction);
  assert.equal(thread.edits.length, 1);
});

test('handleClose: without Manage Threads the thread is archived but not locked, if the bot started it', async () => {
  const { interaction, thread } = fakeClick({ thread: fakeThread({ canLock: false, ownerId: 'BOT' }) });
  await handleClose(interaction);
  assert.deepEqual(thread.edits, [{ archived: true, reason: 'Closed by U1' }]);
});

test('handleClose: refuses when the bot can neither lock nor archive, when already closed, outside a thread or on a stale id', async () => {
  const cannot = fakeClick({ thread: fakeThread({ canLock: false, ownerId: 'SOMEONE' }) });
  await handleClose(cannot.interaction);
  assert.equal(cannot.thread.edits.length, 0);
  assert.equal(cannot.calls.updates.length, 0);
  assert.match(cannot.calls.replies[0].embeds[0].toJSON().description, /Manage Threads/);

  const closed = fakeClick({ thread: fakeThread({ archived: true }) });
  await handleClose(closed.interaction);
  assert.equal(closed.thread.edits.length, 0);
  assert.match(closed.calls.replies[0].embeds[0].toJSON().description, /already closed/);

  const notThread = fakeClick();
  notThread.interaction.channel = { isThread: () => false };
  await handleClose(notThread.interaction);
  assert.equal(notThread.calls.replies.length, 1);

  const stale = fakeClick();
  stale.interaction.customId = 'autothread:close';
  await handleClose(stale.interaction);
  assert.equal(stale.thread.edits.length, 0);
  assert.equal(stale.calls.replies.length, 1);
});

test('handleClose: a failing archive is reported privately, never thrown', async () => {
  const { interaction, calls, thread } = fakeClick();
  thread.edit = async () => { throw discordErr(50013); };
  await handleClose(interaction);
  assert.equal(calls.followUps.length, 1);
  assert.equal(calls.followUps[0].ephemeral, true);
});

test('closeRow: one secondary button with the starter in its customId', () => {
  const button = closeRow('U9').toJSON().components[0];
  assert.equal(button.custom_id, 'autothread:close:U9');
  assert.ok(button.custom_id.length <= 100);
});

// ── Plugin declaration ────────────────────────────────────────────────────────

test('the plugin declares its hooks in HOOK_ORDER, a gated close button and cleanup that runs while the module is off', async () => {
  assert.equal(plugin.id, 'auto-threads');
  assert.equal(plugin.module, 'autothreads');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id]), [
    ['messageCreate', 'auto-threads'], ['botMessage', 'auto-threads'], ['channelDelete', 'auto-threads'],
  ]);
  assert.deepEqual(plugin.hooks.filter((h) => h.always).map((h) => h.event), ['channelDelete']);
  for (const h of plugin.hooks) {
    assert.ok(HOOK_ORDER[h.event].includes('auto-threads'), h.event);
    assert.equal(typeof h.filter, 'function', `${h.event} has a sync filter`);
    assert.notEqual(h.filter.constructor.name, 'AsyncFunction');
  }
  const order = HOOK_ORDER.messageCreate;
  assert.ok(order.indexOf('auto-threads') > order.indexOf('automod'), 'after automod');
  assert.ok(order.indexOf('auto-threads') < order.indexOf('sticky'), 'before sticky');
  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix]), [['button', 'autothread']]);
  assert.equal(plugin.components[0].module, undefined, 'gated by the plugin module');
  assert.equal(CUSTOM_ID_PREFIXES.autothread.owner, 'auto-threads');
  assert.deepEqual(plugin.jobs, []);

  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'auto-threads'), 'collectPlugins keeps it');
  assert.equal(errors.filter((e) => e.includes('auto-threads')).length, 0);
});

// ── The /autothread command ───────────────────────────────────────────────────

// Discord caps the combined length of every name, description and choice value (the planned budget counts the same).
function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/autothread: payload, levels and gating', () => {
  assert.equal(cmd.module, 'autothreads');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'autothread');
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.ok(charCount(json) <= 1500, `payload is ${charCount(json)} characters (budget 1500)`);
  const top = json.options.map((o) => `${o.type === 2 ? 'group' : 'sub'}:${o.name}`);
  assert.deepEqual(top, ['sub:add', 'sub:remove', 'sub:list', 'group:publish']);
  const publish = json.options.find((o) => o.name === 'publish');
  assert.deepEqual(publish.options.map((o) => o.name), ['add', 'remove']);
  for (const sub of publish.options) assert.deepEqual(sub.options[0].channel_types, [ChannelType.GuildAnnouncement]);
  const add = json.options.find((o) => o.name === 'add');
  assert.deepEqual(add.options.map((o) => o.name), ['channel', 'name', 'archive', 'close_button']);
  assert.deepEqual(add.options[0].channel_types, [ChannelType.GuildText, ChannelType.GuildAnnouncement]);
  assert.deepEqual(add.options.find((o) => o.name === 'archive').choices.map((c) => c.value), ['60', '1440', '4320', '10080']);
  assert.equal(add.options.find((o) => o.name === 'name').max_length, 100);
});

function fakeCommand({ group = null, sub, channel, strings = {}, bools = {}, guildChannels = [] }) {
  const replies = [];
  const interaction = {
    guildId: channel.guildId, user: { id: 'ADMIN' }, replies,
    guild: { id: channel.guildId, channels: { cache: new Map([channel, ...guildChannels].map((c) => [c.id, c])) } },
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      getChannel: () => ({ id: channel.id }),
      getString: (name) => strings[name] ?? null,
      getBoolean: (name) => bools[name] ?? null,
    },
    deferred: false, replied: false,
    async deferReply() { interaction.deferred = true; interaction.ephemeral = false; },
    async editReply(payload) { replies.push(payload); interaction.replied = true; return payload; },
  };
  return interaction;
}
const text = (interaction) => {
  const e = interaction.replies.at(-1).embeds[0].toJSON();
  return `${e.title}\n${e.description}`;
};

test('/autothread add: saves the channel, shows the options and warns about what is missing', async () => {
  const guildId = newGuildId();
  await seed(guildId, { autoThreadsEnabled: false });
  const channel = fakeChannel({ guildId, perms: ['ViewChannel', 'SendMessages'] });
  const i = fakeCommand({ sub: 'add', channel, strings: { name: 'Q: {content}', archive: '4320' }, bools: { close_button: true } });
  await cmd.execute(i);
  assert.deepEqual(settingsRows.get(guildId).autoThreadChannels, [
    { channelId: channel.id, name: 'Q: {content}', archiveMinutes: '4320', closeButton: true },
  ]);
  const out = text(i);
  assert.match(out, /Auto Thread Added/);
  assert.match(out, /3 days/);
  assert.match(out, /Create Public Threads/);
  assert.match(out, /Send Messages in Threads/);
  assert.match(out, /module is off/i);
  assert.deepEqual(i.replies.at(-1).allowedMentions, { parse: [] });

  const again = fakeCommand({ sub: 'add', channel, bools: { close_button: false } });
  await cmd.execute(again);
  assert.match(text(again), /Auto Thread Updated/);
  assert.equal(settingsRows.get(guildId).autoThreadChannels.length, 1);
  assert.equal(settingsRows.get(guildId).autoThreadChannels[0].name, 'Q: {content}', 'unspecified options are kept');
});

test('/autothread add: refuses an unknown placeholder and a full list; a wrong channel type', async () => {
  const guildId = newGuildId();
  await seed(guildId, {});
  const channel = fakeChannel({ guildId });
  const typo = fakeCommand({ sub: 'add', channel, strings: { name: '{titel}' } });
  await cmd.execute(typo);
  assert.match(text(typo), /Unknown Placeholder/);
  assert.deepEqual(settingsRows.get(guildId).autoThreadChannels, []);

  const full = Array.from({ length: LIMITS.AUTOTHREAD_CHANNELS }, (_, i) => ({ channelId: String(500000 + i), name: 'x', archiveMinutes: '60', closeButton: false }));
  await seed(guildId, { autoThreadChannels: full });
  const over = fakeCommand({ sub: 'add', channel });
  await cmd.execute(over);
  assert.match(text(over), /Limit Reached/);

  const voice = fakeChannel({ guildId, type: ChannelType.GuildVoice });
  const wrong = fakeCommand({ sub: 'add', channel: voice });
  await cmd.execute(wrong);
  assert.match(text(wrong), /Wrong Channel Type/);
});

test('/autothread remove and list', async () => {
  const guildId = newGuildId();
  const channel = fakeChannel({ guildId });
  const ann = fakeChannel({ guildId, type: ChannelType.GuildAnnouncement, perms: ['ViewChannel'] });
  await seed(guildId, {
    autoThreadChannels: [{ channelId: channel.id, name: 'Help `{content}`', archiveMinutes: '60', closeButton: true }],
    autoPublishChannelIds: [ann.id],
  });
  const list = fakeCommand({ sub: 'list', channel, guildChannels: [ann] });
  await cmd.execute(list);
  const out = text(list);
  assert.match(out, new RegExp(`<#${channel.id}>`));
  assert.match(out, /1 hour/);
  assert.match(out, /close button/);
  assert.match(out, new RegExp(`<#${ann.id}>`));
  assert.match(out, /Manage Messages/, 'a publish channel without the permission is flagged');
  assert.doesNotMatch(out, /module is off/i);

  const missing = fakeCommand({ sub: 'remove', channel: fakeChannel({ guildId }) });
  await cmd.execute(missing);
  assert.match(text(missing), /Not In The List/);
  const rm = fakeCommand({ sub: 'remove', channel });
  await cmd.execute(rm);
  assert.match(text(rm), /Auto Thread Removed/);
  assert.deepEqual(settingsRows.get(guildId).autoThreadChannels, []);

  const empty = fakeCommand({ sub: 'list', channel });
  await seed(guildId, { autoThreadsEnabled: false });
  await cmd.execute(empty);
  assert.match(text(empty), /Nothing is set up yet/);
  assert.match(text(empty), /module is off/i);
});

test('/autothread publish add and remove: announcement channels only', async () => {
  const guildId = newGuildId();
  await seed(guildId, {});
  const ann = fakeChannel({ guildId, type: ChannelType.GuildAnnouncement });
  const add = fakeCommand({ group: 'publish', sub: 'add', channel: ann });
  await cmd.execute(add);
  assert.deepEqual(settingsRows.get(guildId).autoPublishChannelIds, [ann.id]);
  assert.match(text(add), /Auto Publish Added/);
  assert.match(text(add), /10 per channel per hour/);

  const repeat = fakeCommand({ group: 'publish', sub: 'add', channel: ann });
  await cmd.execute(repeat);
  assert.match(text(repeat), /Already Publishing/);

  const wrong = fakeCommand({ group: 'publish', sub: 'add', channel: fakeChannel({ guildId }) });
  await cmd.execute(wrong);
  assert.match(text(wrong), /Wrong Channel Type/);
  assert.deepEqual(settingsRows.get(guildId).autoPublishChannelIds, [ann.id]);

  const rm = fakeCommand({ group: 'publish', sub: 'remove', channel: ann });
  await cmd.execute(rm);
  assert.match(text(rm), /Auto Publish Removed/);
  const rm2 = fakeCommand({ group: 'publish', sub: 'remove', channel: ann });
  await cmd.execute(rm2);
  assert.match(text(rm2), /Not In The List/);
});
