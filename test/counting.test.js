// Counting tests. Offline (no Discord, Postgres or Redis): the parser and rules, the notes, the message and delete
// handlers against an in-memory CountingState table and fake channels, gateway-order judging, the plugin declaration
// and the /counting payload.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const svc = await import('../src/services/countingService.js');
const counting = await import('../src/plugins/counting.js');
const cmd = await import('../src/commands/admin/counting.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { updateSettings } = await import('../src/services/settingsService.js');

const { parseCount, currentState, judgeCount, COUNT_MAX } = svc;
const {
  looksLikeCount, countMath, inCountingChannel, ruinedText, deletedText, handleCount, handleDelete, plugin, COUNTING,
} = counting;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// In-memory CountingState table; GuildSettings rows always "exist".
const rows = new Map();
const realCountingState = prisma.countingState;
const realGuildSettings = prisma.guildSettings;
let loadDelay = () => 0;
prisma.countingState = {
  findUnique: async ({ where }) => {
    const ms = loadDelay();
    if (ms) await new Promise((r) => { setTimeout(r, ms); });
    return rows.has(where.guildId) ? { ...rows.get(where.guildId) } : null;
  },
  upsert: async ({ where, create, update }) => {
    const row = rows.has(where.guildId) ? { ...rows.get(where.guildId), ...update } : { ...create };
    rows.set(where.guildId, row);
    return { ...row };
  },
};
prisma.guildSettings = { upsert: async () => ({}) };

after(async () => {
  prisma.countingState = realCountingState;
  prisma.guildSettings = realGuildSettings;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

let nextId = 1000;
function fakeChannel(id, { allowed = true } = {}) {
  return {
    id, guildId: 'G', sent: [],
    guild: { members: { me: {} } },
    permissionsFor: () => ({ has: () => allowed }),
    isTextBased: () => true,
    async send(payload) { this.sent.push(payload); return { id: String(nextId++) }; },
  };
}
function fakeMessage(channel, userId, content) {
  return {
    id: String(nextId++), guildId: channel.guildId, channelId: channel.id, channel, content, author: { id: userId }, reactions: [],
    async react(emoji) { this.reactions.push(emoji); },
  };
}
const ctxFor = (guildId, message, settings) => ({ guildId, message, guild: { channels: { cache: new Map() } }, getSettings: async () => settings });
const noteText = (payload) => payload.embeds[0].toJSON().description;

// ── Pure rules ────────────────────────────────────────────────────────────────

test('parseCount: only a message that is just a number counts', () => {
  assert.equal(parseCount('1'), 1);
  assert.equal(parseCount('  42 \n'), 42);
  assert.equal(parseCount('007'), 7);
  assert.equal(parseCount('0'), 0);
  assert.equal(parseCount(String(COUNT_MAX)), COUNT_MAX);
  for (const chat of ['', ' ', '5 lol', '1.5', '-3', '+3', '**5**', '1,000', '1234567890', 'five', '٣']) {
    assert.equal(parseCount(chat), null, chat);
  }
  assert.equal(parseCount(null), null);
  assert.equal(parseCount(5), null);
  assert.ok(COUNT_MAX <= 2_147_483_647, 'fits the Int column');
});

test('parseCount: maths that comes to a whole number counts as it', () => {
  const sums = {
    '2+2': 4, '(3*4)-1': 11, ' 10 / 2 ': 5, '2^3': 8, '2^3^2': 512, '(-2)^2': 4, '10%3': 1, '5-(-1)': 6,
    '2 x 3': 6, '2×3': 6, '6÷2': 3, '7−2': 5, '2\\*3': 6, '1/49*49': 1, '1-1': 0, '-0': 0, '\n2 +\n2': 4,
    '999999998+1': COUNT_MAX, '(((((1)))))': 1,
  };
  for (const [math, n] of Object.entries(sums)) assert.equal(parseCount(math), n, math);
  for (const chat of [
    '10/3', '24/7', '10/27', '1/0', '0/0', '9^9^9', '-2^2', '1-2', '999999999+1', '2**3', '**5**', '2(3)', '()', '(1+1',
    '1+1)', '^0', '(^0)', '1+', 'x', '5x', '٣+1', '1.5+1', '2+2 lol', `1+${'1'.repeat(16)}`, `${'1+'.repeat(60)}1`,
    `${'('.repeat(60)}1${')'.repeat(60)}`,
  ]) {
    assert.equal(parseCount(chat), null, chat);
  }
  assert.equal(svc.evaluateMath('-2^2'), -4, 'a power binds tighter than the minus');
  assert.equal(Object.is(parseCount('-0'), 0), true, 'never -0');
});

test('currentState: a missing row or one from another channel starts from 0 and keeps the best run', () => {
  assert.deepEqual(currentState(null, 'C'), { count: 0, lastUserId: null, lastMessageId: null, best: 0 });
  const row = { channelId: 'C', count: 7, lastUserId: 'U', lastMessageId: 'M', best: 30 };
  assert.deepEqual(currentState(row, 'C'), { count: 7, lastUserId: 'U', lastMessageId: 'M', best: 30 });
  assert.deepEqual(currentState(row, 'OTHER'), { count: 0, lastUserId: null, lastMessageId: null, best: 30 });
});

test('judgeCount: the next number from someone else', () => {
  assert.deepEqual(judgeCount({ count: 0, lastUserId: null }, 'A', 1), { ok: true, expected: 1 });
  assert.deepEqual(judgeCount({ count: 4, lastUserId: 'A' }, 'B', 5), { ok: true, expected: 5 });
  assert.deepEqual(judgeCount({ count: 4, lastUserId: 'A' }, 'B', 6), { ok: false, reason: 'wrong', expected: 5 });
  assert.deepEqual(judgeCount({ count: 4, lastUserId: 'A' }, 'B', 4), { ok: false, reason: 'wrong', expected: 5 });
  assert.deepEqual(judgeCount({ count: 4, lastUserId: 'A' }, 'A', 5), { ok: false, reason: 'twice', expected: 5 });
  assert.deepEqual(judgeCount({ count: 0, lastUserId: null }, 'A', 2), { ok: false, reason: 'wrong', expected: 1 });
});

test('looksLikeCount and the notes', () => {
  assert.equal(looksLikeCount({ content: '12' }), true);
  assert.equal(looksLikeCount({ content: '  3' }), true);
  assert.equal(looksLikeCount({ content: 'hi 3' }), false);
  assert.equal(looksLikeCount({ content: ' (1+1)' }), true);
  assert.equal(looksLikeCount({ content: '**5**' }), false);
  assert.equal(looksLikeCount({}), false);
  assert.equal(looksLikeCount(null), false);
  const wrong = ruinedText({ userId: 'U', count: 41, reason: 'wrong', expected: 42, best: 50 });
  assert.match(wrong, /<@U> ruined it at \*\*41\*\*/);
  assert.match(wrong, /next number was \*\*42\*\*/);
  assert.match(wrong, /from \*\*1\*\*/);
  assert.match(wrong, /Best run: \*\*50\*\*/);
  assert.match(ruinedText({ userId: 'U', count: 3, reason: 'twice', expected: 4, best: 3 }), /twice in a row/);
  assert.match(ruinedText({ userId: 'U', count: 3, reason: 'wrong', expected: 4, best: 3, math: '2+3', got: 5 }),
    /ruined it at \*\*3\*\*! `2\+3` is \*\*5\*\*\. The next number was \*\*4\*\*/);
  assert.equal(countMath(' 2 +\n3 '), '2 + 3');
  assert.equal(countMath('007'), null);
  assert.match(deletedText({ userId: 'U', count: 9 }), /<@U> deleted their number \*\*9\*\*\. The next number is \*\*10\*\*/);
  assert.match(deletedText({ userId: null, count: 9 }), /^Someone deleted/);
});

// ── Message handling ──────────────────────────────────────────────────────────

test('a run: right numbers get ✅, counting twice in a row resets with a note', async () => {
  const ch = fakeChannel('C1');
  const settings = { countingEnabled: true, countingChannelId: 'C1' };
  const send = async (user, content) => {
    const m = fakeMessage(ch, user, content);
    await handleCount(ctxFor('G1', m, settings));
    return m;
  };

  assert.deepEqual((await send('A', '1')).reactions, [COUNTING.OK_EMOJI]);
  assert.deepEqual((await send('B', '2')).reactions, [COUNTING.OK_EMOJI]);
  const chat = await send('A', 'nice');
  assert.deepEqual(chat.reactions, [], 'chat is ignored');
  const three = await send('A', '3');
  assert.deepEqual(three.reactions, [COUNTING.OK_EMOJI]);
  assert.equal(rows.get('G1').count, 3);
  assert.equal(rows.get('G1').lastMessageId, three.id);
  assert.equal(rows.get('G1').best, 3);
  assert.equal(ch.sent.length, 0);

  const again = await send('A', '4');
  assert.deepEqual(again.reactions, [COUNTING.FAIL_EMOJI]);
  assert.deepEqual({ ...rows.get('G1'), updatedAt: undefined }, {
    guildId: 'G1', channelId: 'C1', count: 0, lastUserId: null, lastMessageId: null, best: 3, updatedAt: undefined,
  });
  assert.equal(ch.sent.length, 1);
  assert.match(noteText(ch.sent[0]), /<@A> ruined it at \*\*3\*\*! You can't count twice in a row/);
  assert.deepEqual(ch.sent[0].allowedMentions, { parse: [] });

  // The one who ruined it may start again.
  assert.deepEqual((await send('A', '1')).reactions, [COUNTING.OK_EMOJI]);
});

test('a wrong number resets the run; at 0 it only gets ❌ and a throttled hint', async () => {
  const ch = fakeChannel('C2');
  const settings = { countingEnabled: true, countingChannelId: 'C2' };
  const send = async (user, content) => {
    const m = fakeMessage(ch, user, content);
    await handleCount(ctxFor('G2', m, settings));
    return m;
  };

  const early = await send('A', '5');
  assert.deepEqual(early.reactions, [COUNTING.FAIL_EMOJI]);
  assert.equal(rows.has('G2'), false, 'nothing to reset at 0');
  assert.equal(ch.sent.length, 1);
  assert.match(noteText(ch.sent[0]), /starts at \*\*1\*\*/);
  await send('B', '0');
  assert.equal(ch.sent.length, 1, 'the hint is throttled');

  await send('A', '1');
  await send('B', '2');
  const wrong = await send('A', '4');
  assert.deepEqual(wrong.reactions, [COUNTING.FAIL_EMOJI]);
  assert.equal(rows.get('G2').count, 0);
  assert.equal(rows.get('G2').best, 2);
  assert.match(noteText(ch.sent.at(-1)), /<@A> ruined it at \*\*2\*\*! The next number was \*\*3\*\*/);
});

test('maths counts: the right sum gets ✅ and advances, a wrong one says what it came to', async () => {
  const ch = fakeChannel('C9');
  const settings = { countingEnabled: true, countingChannelId: 'C9' };
  const send = async (user, content) => {
    const m = fakeMessage(ch, user, content);
    await handleCount(ctxFor('G9', m, settings));
    return m;
  };

  assert.deepEqual((await send('A', '1')).reactions, [COUNTING.OK_EMOJI]);
  const two = await send('B', '(3*4)/6');
  assert.deepEqual(two.reactions, [COUNTING.OK_EMOJI]);
  assert.equal(rows.get('G9').count, 2);
  assert.equal(rows.get('G9').lastMessageId, two.id);
  assert.deepEqual((await send('A', '10/27')).reactions, [], 'a fraction is chat');
  assert.deepEqual((await send('A', '2^2 - 1')).reactions, [COUNTING.OK_EMOJI]);
  assert.equal(rows.get('G9').best, 3);

  const wrong = await send('B', '2 * 3');
  assert.deepEqual(wrong.reactions, [COUNTING.FAIL_EMOJI]);
  assert.equal(rows.get('G9').count, 0);
  assert.match(noteText(ch.sent.at(-1)), /<@B> ruined it at \*\*3\*\*! `2 \* 3` is \*\*6\*\*\. The next number was \*\*4\*\*/);
});

test('other channels, no channel and missing permissions', async () => {
  const ch = fakeChannel('C3');
  const elsewhere = fakeMessage(ch, 'A', '1');
  await handleCount(ctxFor('G3', elsewhere, { countingEnabled: true, countingChannelId: 'OTHER' }));
  await handleCount(ctxFor('G3', elsewhere, { countingEnabled: true, countingChannelId: null }));
  await handleCount(ctxFor('G3', elsewhere, null));
  assert.deepEqual(elsewhere.reactions, []);
  assert.equal(rows.has('G3'), false);

  // Without Add Reactions the count still advances, silently.
  const mute = fakeChannel('C4', { allowed: false });
  const m = fakeMessage(mute, 'A', '1');
  await handleCount(ctxFor('G4', m, { countingEnabled: true, countingChannelId: 'C4' }));
  assert.deepEqual(m.reactions, []);
  assert.equal(rows.get('G4').count, 1);
});

test('a new channel starts from 1 and keeps the best run', async () => {
  rows.set('G5', { guildId: 'G5', channelId: 'OLD', count: 12, lastUserId: 'A', lastMessageId: 'M', best: 20 });
  const ch = fakeChannel('NEW');
  const m = fakeMessage(ch, 'A', '1');
  await handleCount(ctxFor('G5', m, { countingEnabled: true, countingChannelId: 'NEW' }));
  assert.deepEqual(m.reactions, [COUNTING.OK_EMOJI]);
  assert.equal(rows.get('G5').channelId, 'NEW');
  assert.equal(rows.get('G5').count, 1);
  assert.equal(rows.get('G5').best, 20);
});

test('the hook judges counts in the order they arrive, even when loads finish out of order', async () => {
  const ch = fakeChannel('C6');
  const settings = { countingEnabled: true, countingChannelId: 'C6' };
  const hook = plugin.hooks.find((h) => h.event === 'messageCreate');
  const delays = [30, 0, 15, 0];
  loadDelay = () => delays.shift() ?? 0;
  try {
    const msgs = [['A', '1'], ['B', '2'], ['A', '3'], ['B', '4']].map(([u, c]) => fakeMessage(ch, u, c));
    await Promise.all(msgs.map((m) => hook.run(ctxFor('G6', m, settings))));
    assert.deepEqual(msgs.map((m) => m.reactions), msgs.map(() => [COUNTING.OK_EMOJI]));
    assert.equal(rows.get('G6').count, 4);
  } finally {
    loadDelay = () => 0;
  }
});

test('deleting the current count posts the next number', async () => {
  const ch = fakeChannel('C7');
  const settings = { countingEnabled: true, countingChannelId: 'C7' };
  const one = fakeMessage(ch, 'A', '1');
  const two = fakeMessage(ch, 'B', '2');
  await handleCount(ctxFor('G7', one, settings));
  await handleCount(ctxFor('G7', two, settings));

  await handleDelete(ctxFor('G7', { id: one.id, channelId: 'C7', channel: ch }, settings));
  assert.equal(ch.sent.length, 0, 'an older count is not the current one');
  await handleDelete(ctxFor('G7', { id: two.id, channelId: 'C7', channel: ch }, { ...settings, countingChannelId: 'X' }));
  assert.equal(ch.sent.length, 0, 'not the counting channel');
  await handleDelete(ctxFor('G7', { id: two.id, channelId: 'C7', channel: ch }, settings));
  assert.equal(ch.sent.length, 1);
  assert.match(noteText(ch.sent[0]), /<@B> deleted their number \*\*2\*\*\. The next number is \*\*3\*\*/);
  assert.equal(rows.get('G7').count, 2, 'the count is kept');
});

test('the hook filters skip other channels once the settings are memoised', async () => {
  // Without a memo the filter passes and the handler decides after loading the settings.
  assert.equal(inCountingChannel({ guildId: 'G8', message: { channelId: 'ANY' } }), true);

  const upsert = prisma.guildSettings.upsert;
  prisma.guildSettings.upsert = async ({ where, update }) => ({ guildId: where.guildId, ...update });
  try {
    await updateSettings('G8', { countingEnabled: true, countingChannelId: 'C8' });
  } finally {
    prisma.guildSettings.upsert = upsert;
  }
  const create = plugin.hooks.find((h) => h.event === 'messageCreate');
  const del = plugin.hooks.find((h) => h.event === 'messageDelete');
  const here = fakeMessage(fakeChannel('C8'), 'A', '5');
  const elsewhere = fakeMessage(fakeChannel('OTHER'), 'A', '5');
  assert.equal(create.filter(ctxFor('G8', here, {})), true);
  assert.equal(create.filter(ctxFor('G8', elsewhere, {})), false, 'a number in another channel never takes the lock');
  assert.equal(create.filter(ctxFor('G8', fakeMessage(fakeChannel('C8'), 'A', 'hi'), {})), false, 'chat is still skipped');
  assert.equal(del.filter(ctxFor('G8', { id: '1', channelId: 'C8' }, {})), true);
  assert.equal(del.filter(ctxFor('G8', { id: '1', channelId: 'OTHER' }, {})), false, 'deletes elsewhere are skipped');
});

test('the reaction is sent after the lock is released, so a slow reaction never holds up the next count', async () => {
  const ch = fakeChannel('C9');
  const settings = { countingEnabled: true, countingChannelId: 'C9' };
  const hook = plugin.hooks.find((h) => h.event === 'messageCreate');
  const slow = fakeMessage(ch, 'A', '1');
  let release = null;
  slow.react = (emoji) => new Promise((resolve) => { release = () => { slow.reactions.push(emoji); resolve(); }; });

  const first = hook.run(ctxFor('G9', slow, settings));
  const next = fakeMessage(ch, 'B', '2');
  await hook.run(ctxFor('G9', next, settings));
  assert.deepEqual(next.reactions, [COUNTING.OK_EMOJI], 'judged while the first reaction is still pending');
  assert.equal(rows.get('G9').count, 2);
  assert.equal(typeof release, 'function', 'the first reaction was started');
  release();
  await first;
  assert.deepEqual(slow.reactions, [COUNTING.OK_EMOJI]);
});

// ── Declarations ──────────────────────────────────────────────────────────────

test('plugin declaration and hook order', async () => {
  assert.equal(plugin.id, 'counting');
  assert.equal(plugin.module, 'counting');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id]), [['messageCreate', 'counting'], ['messageDelete', 'counting']]);
  assert.equal(HOOK_ORDER.messageCreate.indexOf('counting'), HOOK_ORDER.messageCreate.indexOf('automod') + 1, 'right after automod');
  assert.ok(HOOK_ORDER.messageDelete.includes('counting'));
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'counting'), 'collectPlugins keeps it');
  assert.deepEqual(errors.filter((e) => String(e?.message ?? e).includes('counting')), []);
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/counting payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'counting');
  assert.equal(json.default_member_permissions, '32');
  assert.deepEqual(json.contexts, [0]);
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['set', 1], ['reset', 1], ['status', 1]]);
  const sub = Object.fromEntries(json.options.map((o) => [o.name, o]));
  assert.deepEqual(sub.set.options.map((o) => [o.name, o.type, !!o.required]), [['channel', 7, false], ['new_channel', 3, false]]);
  assert.deepEqual(sub.reset.options.map((o) => [o.name, o.type, !!o.required, o.min_value, o.max_value]), [['number', 4, false, 0, COUNT_MAX]]);
  assert.deepEqual(sub.status.options ?? [], []);
  assert.ok(charCount(json) <= 700, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'counting');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
});
