// Regression tests for the forwarded-message bypass: a forward has empty `content` and its text only in
// `messageSnapshots`, so automod's text rules, the repeated-text fingerprint, the invite-posted log and the
// delete log used to skip it. Everything now reads effectiveText (own content + every snapshot's content).
// The forwards are real discord.js Message objects (so the snapshot shape is the library's own); only the
// guild, channel and member around them are fakes. Offline: no Discord, Postgres or Redis needed.
import { test, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is never reached.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';
delete process.env.BOT_OWNER_ID; // the fakes must resolve as plain members

const { Client, Message, PermissionFlagsBits, PermissionsBitField, ChannelType } = await import('discord.js');
const { effectiveText, forwardedSnapshots } = await import('../src/utils/text.js');
const svc = await import('../src/services/automodService.js');
const ev = await import('../src/services/logEvents.js');
const logSvc = await import('../src/services/loggingService.js');
const { plugin } = await import('../src/plugins/logging.js');
const { default: redis, cache } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

const { runAutomod, evaluateContentRules, getAutomodConfig, resetAutomodState } = svc;

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};
const client = new Client({ intents: [] });
after(async () => {
  Object.assign(console, quiet);
  logSvc._resetLoggingStateForTests();
  await client.destroy();
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});
beforeEach(() => {
  resetAutomodState();
  logSvc._resetLoggingStateForTests();
});

const SNOW = (n) => `955090${String(n).padStart(12, '0')}`;
const GID = SNOW(1);
const UID = SNOW(2);
const OWNER = SNOW(3);
const BOT = SNOW(4);
const CH = SNOW(5);
const LOGCH = SNOW(6);
let nextId = 100;

/**
 * A real discord.js Message. With `snapshots` it is shaped like a forward: type Default, empty content, flags
 * HasSnapshot, a Forward message_reference and the text under message_snapshots. A snapshot is `{ content, files }`
 * where `files` are attachment names.
 */
function realMessage({ content = '', snapshots = null, id = SNOW(nextId++) } = {}) {
  const timestamp = new Date().toISOString();
  const data = {
    id, channel_id: CH, guild_id: GID, type: 0, content,
    author: { id: UID, username: 'someone', discriminator: '0', avatar: null },
    attachments: [], embeds: [], mentions: [], mention_roles: [], pinned: false, tts: false,
    timestamp, edited_timestamp: null, flags: 0,
  };
  if (snapshots) {
    data.flags = 16384;
    data.message_reference = { type: 1, message_id: SNOW(900), channel_id: SNOW(901), guild_id: GID };
    data.message_snapshots = snapshots.map((s) => ({
      message: {
        type: 0, content: s.content ?? '', embeds: [], timestamp, edited_timestamp: null, flags: 0, mentions: [], mention_roles: [],
        attachments: (s.files ?? []).map((name, i) => ({
          id: SNOW(700 + i), filename: name, size: 1, url: `https://cdn.example.com/${name}`, proxy_url: `https://cdn.example.com/${name}`,
          flags: name.startsWith('SPOILER_') ? 8 : 0, // AttachmentFlags.IsSpoiler
        })),
      },
    }));
  }
  return new Message(client, data);
}

// ── effectiveText ────────────────────────────────────────────────────────────

test('effectiveText: a plain message is exactly its content, a forward is its snapshot text', () => {
  assert.equal(effectiveText(realMessage({ content: 'hello there' })), 'hello there');
  assert.equal(effectiveText(realMessage({ content: '' })), '');
  const forward = realMessage({ snapshots: [{ content: 'join discord.gg/dummyinvite' }] });
  assert.equal(forward.content, '', 'a forward really has no content of its own');
  assert.equal(forward.messageSnapshots.size, 1);
  assert.equal(effectiveText(forward), 'join discord.gg/dummyinvite');
  assert.equal(effectiveText(realMessage({ content: 'look', snapshots: [{ content: 'a' }] })), 'look\na', 'own content comes first');
  assert.equal(effectiveText(realMessage({ snapshots: [{ content: '' }, { content: 'b' }] })), 'b', 'empty parts are dropped');
  assert.equal(forwardedSnapshots(forward).length, 1);
});

test('effectiveText: stubs, partials and odd snapshot values never throw', () => {
  for (const m of [undefined, null, {}, { content: null }, { partial: true }]) assert.equal(effectiveText(m), '', String(m));
  assert.equal(effectiveText({ content: 'plain' }), 'plain');
  assert.equal(effectiveText({ content: 42 }), '42');
  // Not a Collection: an array, a Map, and values that are not iterable at all.
  assert.equal(effectiveText({ content: '', messageSnapshots: [{ content: 'from array' }] }), 'from array');
  assert.equal(effectiveText({ content: 'x', messageSnapshots: new Map([['1', { content: 'from map' }]]) }), 'x\nfrom map');
  for (const bad of [null, 7, 'text', {}, true, { values: 'nope' }, [null, undefined, { content: null }]]) {
    assert.equal(effectiveText({ content: 'own', messageSnapshots: bad }), 'own', JSON.stringify(bad));
  }
  const hostile = { content: 'own', get messageSnapshots() { throw new Error('boom'); } };
  assert.doesNotThrow(() => effectiveText(hostile));
  assert.deepEqual(forwardedSnapshots({ messageSnapshots: 7 }), []);
});

// ── automod pipeline ─────────────────────────────────────────────────────────

/** The fakes around a message: guild, channel, member and a recording ctx (the same shape automod.test.js uses). */
function makeEnv({
  content = '', snapshots = null, rules, config, modLog = false, isEdit = false, oldMessage,
} = {}) {
  const sent = [];
  const logSent = [];
  const deletes = [];
  const stops = [];
  const guild = {
    id: GID, ownerId: OWNER, available: true, vanityURLCode: null,
    members: { me: { id: BOT }, fetch: async () => { throw Object.assign(new Error('Unknown Member'), { code: 10007 }); } },
    invites: { cache: new Map() },
    channels: { cache: new Map() },
  };
  const channel = {
    id: CH, name: 'general', parentId: null, guild,
    isThread: () => false,
    isTextBased: () => true,
    permissionsFor: () => ({ has: () => true }),
    send: async (payload) => { sent.push(payload); return { id: SNOW(50 + sent.length), delete: async () => {} }; },
  };
  if (modLog) {
    guild.channels.cache.set(LOGCH, {
      id: LOGCH, guild, isTextBased: () => true, isThread: () => false, permissionsFor: () => ({ has: () => true }),
      send: async (payload) => { logSent.push(payload); return {}; },
    });
  }
  const member = {
    id: UID, user: { id: UID, bot: false }, guild,
    permissions: { has: () => false },
    roles: { cache: new Map(), highest: { position: 0 } },
  };
  // channel, guild, member and delete are prototype getters/methods on the real Message: shadow them on the instance.
  const wire = (message) => Object.defineProperties(message, {
    guild: { value: guild },
    channel: { value: channel },
    member: { value: member },
    delete: { value: async () => { deletes.push(1); } },
  });
  const message = wire(realMessage({ content, snapshots }));
  const ctx = {
    guild, guildId: GID, client: { user: { id: BOT } }, message, member,
    getSettings: async () => ({
      automodEnabled: true,
      automodConfig: config ?? { version: 2, rules },
      modLogChannelId: modLog ? LOGCH : null,
      modRoleIds: [],
      immuneRoleIds: [],
    }),
    stop: (reason) => stops.push(reason),
    state: {},
  };
  if (isEdit) {
    ctx.oldKnown = oldMessage !== undefined;
    ctx.oldMessage = oldMessage === undefined ? null : wire(oldMessage);
  }
  return { ctx, guild, channel, message, sent, logSent, deletes, stops };
}

const BANNED = 'this is a forbiddenword';
const INVITE = 'join discord.gg/dummyinvite';
const LINK = 'visit https://blocked.example/x';
const ZALGO = 'Z\u{353}\u{354}\u{355}\u{351}\u{352}o';

const RULE_CASES = [
  ['badWords', BANNED, { version: 2, rules: { badWords: { action: 'delete' } }, badWords: ['forbiddenword'] }],
  ['invites', INVITE, { version: 2, rules: { invites: { action: 'delete' } } }],
  ['links', LINK, { version: 2, rules: { links: { action: 'delete' } }, blockedDomains: ['blocked.example'] }],
  ['caps', 'THIS IS ALL CAPITALS HERE', { version: 2, rules: { caps: { action: 'delete' } } }],
  ['emojis', '\u{1F600}'.repeat(10), { version: 2, rules: { emojis: { action: 'delete' } } }],
  ['zalgo', ZALGO, { version: 2, rules: { zalgo: { action: 'delete' } } }],
];

for (const [rule, text, config] of RULE_CASES) {
  test(`automod: a forward carrying ${rule} text is deleted exactly like the same text pasted`, async () => {
    resetAutomodState();
    const plain = makeEnv({ content: text, config, modLog: true });
    await runAutomod(plain.ctx, { isEdit: false });
    resetAutomodState();
    const forward = makeEnv({ snapshots: [{ content: text }], config, modLog: true });
    assert.equal(forward.message.content, '');
    await runAutomod(forward.ctx, { isEdit: false });

    assert.equal(plain.deletes.length, 1, 'baseline: the pasted text is deleted');
    assert.equal(forward.deletes.length, 1, 'the forward is deleted');
    assert.deepEqual(forward.stops, ['automod']);
    assert.deepEqual(forward.stops, plain.stops);
    assert.deepEqual(forward.ctx.state.automodAction, { rule, deleted: true });
    assert.deepEqual(forward.ctx.state.automodAction, plain.ctx.state.automodAction);
    assert.equal(forward.sent.length, 1, 'one in-channel notice');
    assert.equal(forward.sent[0].content, plain.sent[0].content);
    assert.equal(forward.logSent.length, 1, 'one mod-log embed');
    const embed = forward.logSent[0].embeds[0].toJSON();
    assert.match(embed.title, /Message Deleted/);
    assert.equal(embed.fields.find((f) => f.name === 'Content').value, text, 'the mod log shows the forwarded text');
    assert.deepEqual(embed.fields.map((f) => f.name), plain.logSent[0].embeds[0].toJSON().fields.map((f) => f.name));
  });
}

test('automod: a harmless forward and a harmless plain message are left alone', async () => {
  const config = { version: 2, rules: { badWords: { action: 'delete' }, invites: { action: 'delete' }, links: { action: 'delete' } }, badWords: ['forbiddenword'], blockedDomains: ['blocked.example'] };
  for (const env of [makeEnv({ snapshots: [{ content: 'see you at noon' }], config }), makeEnv({ content: 'see you at noon', config })]) {
    await runAutomod(env.ctx, { isEdit: false });
    assert.equal(env.deletes.length, 0);
    assert.deepEqual(env.stops, []);
    assert.equal(env.ctx.state.automodAction, undefined);
  }
  // A forward with no text at all (an image, a sticker) is not a text violation either.
  const empty = makeEnv({ snapshots: [{ content: '' }], config });
  await runAutomod(empty.ctx, { isEdit: false });
  assert.equal(empty.deletes.length, 0);
});

test('automod: a forward with a banned word, an invite and a blocked link is matched by all three rules', async () => {
  const config = { version: 2, rules: { badWords: { action: 'delete' }, invites: { action: 'delete' }, links: { action: 'delete' } }, badWords: ['forbiddenword'], blockedDomains: ['blocked.example'] };
  const text = `join discord.gg/dummyinvite or visit https://blocked.example/x it is a forbiddenword`;
  const env = makeEnv({ snapshots: [{ content: text }], config, modLog: true });
  await runAutomod(env.ctx, { isEdit: false });
  assert.equal(env.deletes.length, 1, 'one delete for three matching rules');
  assert.deepEqual(env.stops, ['automod']);
  const rules = ['badWords', 'invites', 'links'];
  const { config: cfg, matchers } = getAutomodConfig({ automodConfig: config });
  const matched = evaluateContentRules(env.message, rules, cfg, matchers, env.guild).map((m) => m.rule);
  assert.deepEqual(matched, rules);
  assert.match(env.logSent[0].embeds[0].toJSON().fields.find((f) => f.name === 'Rule').value, /Also matched/);
});

test('automod: a forward that also has its own text is judged on both', async () => {
  const config = { version: 2, rules: { badWords: { action: 'delete' } }, badWords: ['forbiddenword'] };
  const env = makeEnv({ content: 'look at this', snapshots: [{ content: BANNED }], config });
  await runAutomod(env.ctx, { isEdit: false });
  assert.equal(env.deletes.length, 1);
  const own = makeEnv({ content: BANNED, snapshots: [{ content: 'fine' }], config });
  await runAutomod(own.ctx, { isEdit: false });
  assert.equal(own.deletes.length, 1, 'the forward does not hide the member\'s own text either');
});

test('automod: the spoilers rule counts snapshot attachments and snapshot "||" pairs', async () => {
  const rules = { spoilers: { action: 'delete', threshold: 2 } };
  const files = makeEnv({ snapshots: [{ content: '', files: ['SPOILER_a.png', 'SPOILER_b.png'] }], rules });
  assert.equal(files.message.messageSnapshots.first().attachments.first().spoiler, true);
  await runAutomod(files.ctx, { isEdit: false });
  assert.equal(files.deletes.length, 1, 'two spoiler files in a forward');
  assert.deepEqual(files.ctx.state.automodAction, { rule: 'spoilers', deleted: true });
  const one = makeEnv({ snapshots: [{ content: '', files: ['SPOILER_a.png', 'plain.png'] }], rules });
  await runAutomod(one.ctx, { isEdit: false });
  assert.equal(one.deletes.length, 0, 'one spoiler file is under the limit');
  const pairs = makeEnv({ snapshots: [{ content: '||a|| and ||b||' }], rules });
  await runAutomod(pairs.ctx, { isEdit: false });
  assert.equal(pairs.deletes.length, 1, 'two "||" pairs in a forward');
  const split = makeEnv({ content: 'open ||', snapshots: [{ content: 'close ||' }], rules });
  await runAutomod(split.ctx, { isEdit: false });
  assert.equal(split.deletes.length, 0, 'a "||" in the member\'s text never pairs with one in the snapshot');
  const plain = makeEnv({ content: '||a|| and ||b||', rules });
  await runAutomod(plain.ctx, { isEdit: false });
  assert.equal(plain.deletes.length, 1, 'baseline: a plain message behaves as before');
});

test('automod: a mod-log entry counts the forwarded attachments too', async () => {
  const config = { version: 2, rules: { invites: { action: 'delete' } } };
  const env = makeEnv({ snapshots: [{ content: INVITE, files: ['a.png', 'b.png'] }], config, modLog: true });
  await runAutomod(env.ctx, { isEdit: false });
  assert.match(env.logSent[0].embeds[0].toJSON().fields.find((f) => f.name === 'Content').value, /2 attachment\(s\)/);
});

test('automod: edits read forwarded text the same way (R119 still excuses a rule the old text broke)', async () => {
  const config = { version: 2, rules: { invites: { action: 'delete' } } };
  const fresh = makeEnv({ snapshots: [{ content: INVITE }], config, isEdit: true, oldMessage: realMessage({ content: 'harmless' }) });
  await runAutomod(fresh.ctx, { isEdit: true });
  assert.equal(fresh.deletes.length, 1, 'new forwarded text that breaks a rule');
  const repeat = makeEnv({ snapshots: [{ content: INVITE }], config, isEdit: true, oldMessage: realMessage({ snapshots: [{ content: 'also discord.gg/other' }] }) });
  await runAutomod(repeat.ctx, { isEdit: true });
  assert.equal(repeat.deletes.length, 0, 'the old forwarded text already broke the rule');
});

test('automod: stubs without messageSnapshots, or with a non-Collection, keep the old behaviour and never throw', () => {
  const { config, matchers } = getAutomodConfig({
    automodConfig: { version: 2, rules: { badWords: { action: 'delete' } }, badWords: ['forbiddenword'] },
  });
  const run = (message) => evaluateContentRules(message, ['badWords', 'spoilers'], config, matchers, {}).map((m) => m.rule);
  assert.deepEqual(run({ content: BANNED }), ['badWords']);
  assert.deepEqual(run({ content: 'fine' }), []);
  assert.deepEqual(run({ content: '' }), []);
  assert.deepEqual(run({ content: 'fine', messageSnapshots: undefined }), []);
  assert.deepEqual(run({ content: 'fine', messageSnapshots: 5 }), []);
  assert.deepEqual(run({ content: 'fine', messageSnapshots: {} }), []);
  assert.deepEqual(run({ content: BANNED, messageSnapshots: {} }), ['badWords']);
  assert.deepEqual(run({ content: '', messageSnapshots: [{ content: BANNED }] }), ['badWords']);
  assert.deepEqual(run({ content: '', messageSnapshots: new Map([['1', { content: BANNED }]]) }), ['badWords']);
  assert.deepEqual(run(undefined), []);
});

// The cache is a plain object, so its Redis methods can be replaced for one test (as automod.test.js does).
function fakeWindow() {
  const sets = new Map();
  const calls = [];
  return {
    calls,
    available: () => true,
    slidingWindow: async (key, windowMs, member) => {
      calls.push({ key, windowMs, member });
      const set = sets.get(key) ?? new Set();
      set.add(member);
      sets.set(key, set);
      return set.size;
    },
  };
}

test('automod: repeated forwards of the same text are counted, under the same key as the pasted text', async () => {
  const original = { available: cache.available, slidingWindow: cache.slidingWindow };
  const fake = fakeWindow();
  cache.available = fake.available;
  cache.slidingWindow = fake.slidingWindow;
  try {
    const rules = { repeatedText: { action: 'delete', threshold: 2 } };
    const first = makeEnv({ snapshots: [{ content: 'Buy   now' }], rules });
    const second = makeEnv({ snapshots: [{ content: 'buy NOW' }], rules });
    const pasted = makeEnv({ content: 'buy now', rules });
    for (const env of [first, second]) await runAutomod(env.ctx, { isEdit: false });
    assert.deepEqual([first, second].map((e) => e.deletes.length), [0, 1], 'the second forward reaches the threshold');
    assert.equal(second.ctx.state.automodAction.rule, 'repeatedText');
    await runAutomod(pasted.ctx, { isEdit: false });
    assert.equal(fake.calls[0].key, fake.calls[2].key, 'a forward and the pasted text share one fingerprint');
    assert.match(fake.calls[0].key, new RegExp(`^automod:dup:${GID}:${UID}:[0-9a-f]{16}$`));
    // A forward with no text still never touches the window.
    const before = fake.calls.length;
    const image = makeEnv({ snapshots: [{ content: '', files: ['a.png'] }], rules });
    await runAutomod(image.ctx, { isEdit: false });
    assert.equal(fake.calls.length, before);
  } finally {
    Object.assign(cache, original);
  }
});

// ── logging ──────────────────────────────────────────────────────────────────

const hook = (event) => plugin.hooks.find((h) => h.event === event);
const LOG_PERMS = new PermissionsBitField(['ViewChannel', 'SendMessages', 'EmbedLinks', 'AttachFiles'].map((n) => PermissionFlagsBits[n]));

function logSetup() {
  const guild = {
    id: SNOW(20), memberCount: 10, vanityURLCode: null,
    members: { me: { id: 'bot', permissions: new PermissionsBitField([]), roles: { cache: new Map() } } },
    channels: { cache: new Map(), fetch: async () => null },
    voiceStates: { cache: new Map() },
  };
  const channel = {
    id: SNOW(21), guild, guildId: guild.id, type: ChannelType.GuildText, name: 'logs', sent: [], _perms: LOG_PERMS,
    isTextBased: () => true, isThread: () => false, isVoiceBased: () => false,
    permissionsFor: () => LOG_PERMS,
    send: async (payload) => { channel.sent.push(payload); },
  };
  guild.channels.cache.set(channel.id, channel);
  const settings = { guildId: guild.id, loggingEnabled: true, modLogChannelId: null, disabledLogEvents: [], logIgnoredChannelIds: [], messageLogChannelId: channel.id };
  return { guild, channel, settings };
}

const logCtx = (guild, settings, fields) => ({
  client: { user: { id: 'bot' } }, guild, guildId: guild.id, state: Object.create(null), getSettings: async () => settings, ...fields,
});

test('logging: the invite hook filter accepts a forward and still rejects everything it used to', () => {
  const { filter } = hook('messageCreate');
  assert.equal(filter({ message: realMessage({ snapshots: [{ content: INVITE }] }) }), true, 'a forwarded invite');
  assert.equal(filter({ message: realMessage({ snapshots: [{ content: 'https://discord.com/invite/abc' }] }) }), true);
  assert.equal(filter({ message: realMessage({ snapshots: [{ content: 'nothing to see' }] }) }), false, 'a harmless forward');
  assert.equal(filter({ message: realMessage({ snapshots: [{ content: '' }] }) }), false, 'a forward with no text');
  assert.equal(filter({ message: realMessage({ content: INVITE }) }), true, 'a pasted invite');
  assert.equal(filter({ message: realMessage({ content: 'hello' }) }), false);
  // Stubs and partials, exactly as before.
  assert.equal(filter({ message: { content: 'hello' } }), false);
  assert.equal(filter({ message: { content: 'discord.gg/abc' } }), true);
  assert.equal(filter({ message: { content: null } }), false);
  assert.equal(filter({ message: undefined }), false);
  assert.equal(filter({ message: { content: '', messageSnapshots: 7 } }), false);
  assert.equal(filter({ message: { content: '', messageSnapshots: new Map([['1', { content: INVITE }]]) } }), true);
});

test('logging: a forwarded invite fires the invite-posted log with the forwarded text', async () => {
  const { guild, channel, settings } = logSetup();
  guild.vanityURLCode = 'myserver';
  const forward = realMessage({ snapshots: [{ content: 'join discord.gg/dummyinvite and discord.gg/myserver' }] });
  await hook('messageCreate').run(logCtx(guild, settings, { message: forward }));
  await logSvc.flushLogQueue();
  assert.equal(channel.sent.length, 1, 'the invite-posted log fires');
  const embed = channel.sent[0].embeds[0];
  assert.equal(embed.title, '📨 Invite Posted');
  assert.equal(embed.description, 'join discord.gg/dummyinvite and discord.gg/myserver');
  const invites = embed.fields.find((f) => f.name === 'Invites').value;
  assert.match(invites, /discord\.gg\/dummyinvite/);
  assert.match(invites, /vanity URL/);

  // Disabled events stay off, and a plain message logs exactly as before.
  await hook('messageCreate').run(logCtx(guild, { ...settings, disabledLogEvents: ['invitePosted'] }, { message: forward }));
  await logSvc.flushLogQueue();
  assert.equal(channel.sent.length, 1);
  const plain = realMessage({ content: 'join discord.gg/dummyinvite' });
  await hook('messageCreate').run(logCtx(guild, settings, { message: plain }));
  await logSvc.flushLogQueue();
  assert.equal(channel.sent[1].embeds[0].description, 'join discord.gg/dummyinvite');
  assert.equal(ev.buildInvitePostedEmbed({ message: plain, codes: ['dummyinvite'] }).toJSON().description, 'join discord.gg/dummyinvite');
});

test('logging: the delete log shows the forwarded text, and a plain delete is unchanged', async () => {
  const text = 'a forbiddenword inside a forward';
  const forward = realMessage({ snapshots: [{ content: text }] });
  const embed = ev.buildMessageDeleteEmbed({ message: forward, now: Date.now() }).toJSON();
  assert.equal(embed.description, text);
  assert.notEqual(embed.description, '*[No text content]*');
  assert.equal(ev.buildMessageDeleteEmbed({ message: realMessage({ content: 'pasted' }) }).toJSON().description, 'pasted');
  assert.equal(ev.buildMessageDeleteEmbed({ message: realMessage({ snapshots: [{ content: '' }] }) }).toJSON().description, '*[No text content]*');
  assert.equal(ev.buildMessageDeleteEmbed({ message: realMessage() }).toJSON().description, '*[No text content]*');
  assert.match(ev.buildMessageDeleteEmbed({ message: { partial: true, id: '1' } }).toJSON().description, /not cached/);
  const long = realMessage({ snapshots: [{ content: 'x'.repeat(5000) }] });
  assert.ok(ev.buildMessageDeleteEmbed({ message: long }).toJSON().description.length <= 4000);

  // Through the hook, as a moderator deleting a forward by hand would trigger it.
  const { guild, channel, settings } = logSetup();
  await hook('messageDelete').run(logCtx(guild, settings, { message: forward, channel: { id: CH } }));
  await logSvc.flushLogQueue();
  assert.equal(channel.sent[0].embeds[0].description, text);
});

test('logging: the bulk-delete transcript carries forwarded text', () => {
  const forward = realMessage({ snapshots: [{ content: 'line one\nline two' }] });
  const plain = realMessage({ content: 'pasted' });
  const transcript = ev.buildBulkDeleteTranscript(new Map([[forward.id, forward], [plain.id, plain]]), { name: 'general' });
  assert.match(transcript, /: line one ⏎ line two/);
  assert.match(transcript, /: pasted/);
});
