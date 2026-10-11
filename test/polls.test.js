// pkg-polls tests (pkg-polls.md §8). Offline by default: option/question/duration parsing, the role matrix, vote maths, tally and
// formatting, components, embeds, the pending-form store, the render throttle (mock timers), the plugin declaration, the
// command payload, /poll create (no I/O before showModal) and the form-expired / invalid-form replies.
// With TEST_DATABASE_URL (a migrated database) the DB cases also run: every RAW_SQL statement, the exact guild cap under
// parallel creates, the reconcile-on-LIMIT path, voting (single, multi, roles, concurrent clicks, after the end, the self-heal),
// endPoll (one winner, edits, results post, deleted message / channel, module off, unavailable guild), the three delete hooks,
// the poll.end job, the create modal end to end and the /poll subcommands against fake interactions:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/polls.test.js
// NOTE: the database tests run on one Postgres connection (connection_limit=1), so concurrent clicks are serialised by the
// pool: they prove the final state (no double votes, exact cap, a single end), not contention on the advisory locks.
import { test, after, mock } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ChannelType, PermissionFlagsBits, ComponentType } = await import('discord.js');
const { Prisma } = await import('@prisma/client');
const svc = await import('../src/services/pollService.js');
const cmd = await import('../src/commands/community/poll.js');
const pluginMod = await import('../src/plugins/polls.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { parseCustomId } = await import('../src/config/customIds.js');
const { getJob, scheduleJob, PermanentJobError } = await import('../src/services/scheduler.js');
const { ensureGuildSettings, updateSettings } = await import('../src/services/settingsService.js');

const {
  POLL, parsePollOptions, validateQuestion, parseDurationOption, canVote, memberRoleIds, applyVoteToggle, normalizeSelection,
  emptyTally, tallyFromGroups, winners, percent, formatOptionLines, buttonLabel, buildPollComponents, buildPollEmbed,
  buildPollPayload, buildResultsEmbed, jumpUrl, createPendingStore, pendingPolls, buildCreateModal, channelProblem,
  requestRender, cancelRender, __setRenderImpl, createPollRow, createPoll, applyVote, endPoll, getTally, handleVoteButton,
  handleVoteSelect, handleCreateModal, handleEndJob, onMessageDelete, onMessageDeleteBulk, onChannelDelete, pollListLines,
  listActivePolls, findPollsForAutocomplete, endJobKey, RAW_SQL,
} = svc;

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const G = (n) => `95501700000000${String(n).padStart(4, '0')}`; // guild ids unique to this package (port 55017)
const ALL_GUILDS = Array.from({ length: 60 }, (_, i) => G(i + 1));

after(async () => {
  Object.assign(console, quiet);
  __setRenderImpl();
  redis.disconnect();
  if (DB) {
    // The cascade removes Poll, PollVote and ScheduledJob rows.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: ALL_GUILDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

// ── Helpers ───────────────────────────────────────────────────────────────────

const sid = (prefix, n) => `${prefix}${String(n).padStart(18 - prefix.length, '0')}`; // 18-digit snowflake-like id
const uid = (n) => sid('7', n);
const rid = (n) => sid('8', n);
const HOST = uid(999);
const BOT = sid('6', 1);
let seq = 0;
const nextId = () => sid('5', ++seq);
const flush = () => new Promise((r) => setImmediate(r));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const cuid = 'c'.repeat(25);

const discordErr = (code) => Object.assign(new Error(`Discord ${code}`), { code });

// ── Fakes (just enough of discord.js for the service) ────────────────────────

function makeMember(id, roleIds = []) {
  return { id, roles: { cache: new Map(roleIds.map((r) => [r, { id: r }])) } };
}

function makeGuild(id, { roles = [] } = {}) {
  const guild = {
    id,
    available: true,
    roles: { cache: new Map([[id, { id }], ...roles.map((r) => [r, { id: r }])]) },
    channels: {
      cache: new Map(),
      fetchable: new Map(), // exists but not cached
      async fetch(cid) {
        const c = guild.channels.cache.get(cid) ?? guild.channels.fetchable.get(cid);
        if (!c) throw discordErr(10003);
        return c;
      },
    },
    members: { me: { id: BOT } },
  };
  return guild;
}

function makeMessage(channel, id) {
  const m = { id, channel, async delete() { channel.store.delete(id); } };
  channel.store.set(id, m);
  return m;
}

function makeChannel(guild, id, { type = ChannelType.GuildText } = {}) {
  const store = new Map();
  const ch = {
    id, name: `chan-${id}`, type, guild, store,
    botPerms: true, memberPerms: true, sent: [], edits: [], failSend: null, failEdit: null,
    isTextBased: () => true,
    isThread: () => false,
    permissionsFor: (who) => ({ has: () => (who === guild.members.me ? ch.botPerms : ch.memberPerms) }),
    async send(payload) {
      if (ch.failSend) throw ch.failSend;
      const message = makeMessage(ch, nextId());
      ch.sent.push({ ...payload, id: message.id });
      return message;
    },
    messages: {
      async edit(mid, payload) {
        if (ch.failEdit) throw ch.failEdit;
        if (!store.has(mid)) throw discordErr(10008);
        ch.edits.push({ id: mid, ...payload });
        return store.get(mid);
      },
    },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function makeClient(guild) {
  return {
    user: { id: BOT },
    guilds: { cache: new Map([[guild.id, guild]]) },
    channels: {
      cache: guild.channels.cache,
      async fetch(cid) { return guild.channels.fetch(cid); },
    },
  };
}

async function makeEnv(no, { roles = [] } = {}) {
  const guildId = G(no);
  await ensureGuildSettings(guildId);
  const guild = makeGuild(guildId, { roles });
  const channel = makeChannel(guild, nextId());
  const client = makeClient(guild);
  return { guildId, guild, channel, client };
}

/** A poll row whose message exists in the fake channel. */
const madePolls = [];
async function makePoll(env, { ghostMessage = false, ...over } = {}) {
  const messageId = over.messageId === undefined ? nextId() : over.messageId;
  const poll = await prisma.poll.create({
    data: {
      guildId: env.guildId, channelId: env.channel.id, messageId, authorId: HOST, question: 'Best pizza?',
      options: ['Margherita', 'Pepperoni', 'Hawaiian'], maxChoices: 1, allowedRoleIds: [], deniedRoleIds: [],
      hideResults: false, endsAt: null, status: 'ACTIVE', ...over,
    },
  });
  madePolls.push(poll.id);
  // ghostMessage: the row points at a message that does not exist in the fake channel (deleted behind our back).
  if (messageId && !ghostMessage && !env.channel.store.has(messageId)) makeMessage(env.channel, messageId);
  return poll;
}
/** Cancels every queued render of this file's polls and restores the real render (so no timer fires after a test). */
const stopRenders = () => {
  for (const id of madePolls) cancelRender(id);
  __setRenderImpl();
};
/** Polls `fn` until it is truthy (the render runs on a timer). */
async function waitFor(fn, timeoutMs = 3000) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    if (await fn()) return true;
    await sleep(15);
  }
  return false;
}

const votesOf = async (pollId, userId) => (await prisma.pollVote.findMany({
  where: { pollId, ...(userId ? { userId } : {}) }, orderBy: [{ userId: 'asc' }, { optionIndex: 'asc' }],
})).map((v) => (userId ? v.optionIndex : `${v.userId}:${v.optionIndex}`));
const reload = (id) => prisma.poll.findUnique({ where: { id } });

/** Base for every fake interaction: deferReply/editReply/reply/deferUpdate record into `replies`. */
function baseInteraction(env, { userId = HOST, roleIds = [], customId = null } = {}) {
  const i = {
    id: nextId(),
    customId,
    user: { id: userId },
    guildId: env.guildId,
    guild: env.guild,
    channel: env.channel,
    channelId: env.channel?.id,
    member: makeMember(userId, roleIds),
    createdTimestamp: Date.now(),
    deferred: false,
    replied: false,
    ephemeral: undefined,
    replies: [],
    updates: 0,
    isRepliable: () => true,
    inCachedGuild: () => true,
    async deferReply({ ephemeral }) { i.deferred = true; i.ephemeral = ephemeral; },
    async deferUpdate() { i.deferred = true; i.updates += 1; },
    async editReply(p) { i.replies.push(p); return { id: 'r' }; },
    async followUp(p) { i.replies.push({ followUp: true, ...p }); return { id: 'f' }; },
    async reply(p) { i.replied = true; i.replies.push(p); return { resource: { message: { id: 'r' } } }; },
  };
  return i;
}

function makeClick(env, pollId, idx, userId, roleIds = [], message = null) {
  const i = baseInteraction(env, { userId, roleIds, customId: `poll:vote:${pollId}:${idx}` });
  i.message = message;
  return i;
}

function makeSelect(env, pollId, userId, values, { roleIds = [], ephemeral = false, message = null } = {}) {
  const i = baseInteraction(env, { userId, roleIds, customId: `poll-select:${pollId}` });
  i.values = values;
  i.message = message ?? { id: nextId(), flags: { has: () => ephemeral }, components: [] };
  if (ephemeral) i.message.flags = { has: () => true };
  return i;
}

function makeModal(env, nonce, question, optionsRaw, { userId = HOST } = {}) {
  const i = baseInteraction(env, { userId, customId: `poll-modal:${nonce}` });
  i.fields = { getTextInputValue: (k) => (k === 'poll-question' ? question : optionsRaw) };
  return i;
}

function makeCmd(env, sub, opts = {}, { userId = HOST } = {}) {
  const i = baseInteraction(env, { userId });
  i.commandName = 'poll';
  i.modals = [];
  i.responded = null;
  i.options = {
    getSubcommand: () => sub,
    getString: (n) => opts[n] ?? null,
    getInteger: (n) => opts[n] ?? null,
    getBoolean: (n) => opts[n] ?? null,
    getRole: (n) => opts[n] ?? null,
    getChannel: (n) => opts[n] ?? null,
    getFocused: () => opts.focused ?? { name: 'poll', value: '' },
  };
  i.showModal = async (m) => { i.modals.push(m); };
  i.respond = async (c) => { i.responded = c; };
  return i;
}

const lastReply = (i) => i.replies.at(-1);
const embedOf = (i) => lastReply(i)?.embeds?.[0]?.data;
const textOf = (i) => embedOf(i)?.description ?? lastReply(i)?.content ?? '';
const titleOf = (i) => embedOf(i)?.title ?? '';
const pending = (over = {}) => ({
  guildId: G(1), channelId: nextId(), userId: HOST, maxChoices: 1, allowedRoleIds: [], deniedRoleIds: [],
  hideResults: false, durationMs: null, ...over,
});

// ── 1. parsePollOptions ───────────────────────────────────────────────────────

test('1. parsePollOptions', () => {
  assert.deepEqual(parsePollOptions('A\nB'), { options: ['A', 'B'], errors: [] });
  assert.deepEqual(parsePollOptions('A\r\n\r\n  B  \n'), { options: ['A', 'B'], errors: [] });
  assert.deepEqual(parsePollOptions('1. Pizza\n2) Burger\n- Tacos\n• Sushi\n* Ramen').options, ['Pizza', 'Burger', 'Tacos', 'Sushi', 'Ramen']);
  assert.deepEqual(parsePollOptions('1.5 hours\n2020 was great').options, ['1.5 hours', '2020 was great']);
  assert.deepEqual(parsePollOptions('only one').errors, ['Add at least 2 options (one per line).']);
  assert.deepEqual(parsePollOptions('').errors, ['Add at least 2 options (one per line).']);
  const many = Array.from({ length: 21 }, (_, i) => `Option number ${i}`).join('\n');
  assert.ok(parsePollOptions(many).errors.some((e) => e.includes('21 options (max 20)')));
  assert.equal(parsePollOptions(Array.from({ length: 20 }, (_, i) => `o${i}`).join('\n')).errors.length, 0);
  assert.ok(parsePollOptions(`${'x'.repeat(81)}\nB`).errors.includes('Option 1 is 81 characters (max 80).'));
  assert.equal(parsePollOptions(`${'x'.repeat(80)}\nB`).errors.length, 0);
  assert.ok(parsePollOptions('Yes\nyes').errors.includes('Option 2 repeats option 1.'));
  // At most five "too long" lines, then an ellipsis.
  const long = Array.from({ length: 8 }, (_, i) => `${'y'.repeat(81)}${i}`).join('\n');
  const errs = parsePollOptions(long).errors;
  assert.equal(errs.filter((e) => e.includes('characters (max 80)')).length, 5);
  assert.ok(errs.includes('…'));
  // A marker only counts when followed by whitespace: "-1" and "3x" stay.
  assert.deepEqual(parsePollOptions('-1\n3x').options, ['-1', '3x']);
});

// ── 2. validateQuestion ───────────────────────────────────────────────────────

test('2. validateQuestion', () => {
  assert.deepEqual(validateQuestion('  ').errors, ['The question is empty.']);
  assert.ok(validateQuestion('q'.repeat(257)).errors[0].includes('257'));
  assert.deepEqual(validateQuestion(' Hi '), { question: 'Hi', errors: [] });
  assert.equal(validateQuestion('q'.repeat(256)).errors.length, 0);
});

// ── 3. parseDurationOption ────────────────────────────────────────────────────

test('3. parseDurationOption', () => {
  assert.deepEqual(parseDurationOption(''), { ms: null });
  assert.deepEqual(parseDurationOption(null), { ms: null });
  assert.deepEqual(parseDurationOption('   '), { ms: null });
  assert.deepEqual(parseDurationOption('30m'), { ms: 1_800_000 });
  assert.deepEqual(parseDurationOption('1m'), { ms: 60_000 });
  assert.deepEqual(parseDurationOption('30d'), { ms: 30 * 86_400_000 });
  assert.deepEqual(parseDurationOption('30s'), { error: 'too-short' });
  assert.deepEqual(parseDurationOption('31d'), { error: 'too-long' });
  assert.deepEqual(parseDurationOption('abc'), { error: 'invalid' });
});

// ── 4. canVote ────────────────────────────────────────────────────────────────

test('4. canVote matrix', () => {
  assert.deepEqual(canVote(['x'], { allowedRoleIds: [], deniedRoleIds: [] }), { ok: true });
  assert.deepEqual(canVote(['a'], { allowedRoleIds: ['a'], deniedRoleIds: [] }), { ok: true });
  assert.deepEqual(canVote(['b'], { allowedRoleIds: ['a'], deniedRoleIds: [] }), { ok: false, reason: 'not-allowed', roleId: 'a' });
  assert.deepEqual(canVote(['a', 'd'], { allowedRoleIds: ['a'], deniedRoleIds: ['d'] }), { ok: false, reason: 'denied', roleId: 'd' });
  assert.deepEqual(canVote(['d'], { allowedRoleIds: [], deniedRoleIds: ['d'] }), { ok: false, reason: 'denied', roleId: 'd' });
  assert.deepEqual(canVote(['b'], { allowedRoleIds: ['a'], deniedRoleIds: [] }, new Set(['b'])), { ok: false, reason: 'allowed-role-gone' });
  assert.deepEqual(canVote(['c'], { allowedRoleIds: ['a', 'c'], deniedRoleIds: [] }, new Set(['c'])), { ok: true });
  // A deleted denied role is simply never held; a deleted allowed role is ignored while another remains.
  assert.deepEqual(canVote([], { allowedRoleIds: [], deniedRoleIds: ['gone'] }, new Set(['x'])), { ok: true });
  assert.deepEqual(canVote(['b'], { allowedRoleIds: ['a', 'b'], deniedRoleIds: [] }, new Set(['b'])), { ok: true });
  assert.deepEqual(canVote(['b'], { allowedRoleIds: ['a', 'c'], deniedRoleIds: [] }, new Set(['a', 'c'])), { ok: false, reason: 'not-allowed', roleId: 'a' });
  assert.deepEqual(memberRoleIds({ roles: ['1', '2'] }), ['1', '2']);
  assert.deepEqual(memberRoleIds({ roles: { cache: new Map([['3', {}]]) } }), ['3']);
  assert.deepEqual(memberRoleIds(null), []);
});

// ── 5. applyVoteToggle ────────────────────────────────────────────────────────

test('5. applyVoteToggle', () => {
  assert.deepEqual(applyVoteToggle([], 2, 1), { next: [2], result: 'added' });
  assert.deepEqual(applyVoteToggle([2], 2, 1), { next: [], result: 'removed' });
  assert.deepEqual(applyVoteToggle([1], 2, 1), { next: [2], result: 'switched' });
  assert.deepEqual(applyVoteToggle([0, 1], 2, 2), { next: [0, 1], result: 'limit' });
  assert.deepEqual(applyVoteToggle([0], 1, 3), { next: [0, 1], result: 'added' });
  assert.deepEqual(applyVoteToggle([0, 1], 0, 3), { next: [1], result: 'removed' });
  assert.deepEqual(applyVoteToggle([3], 1, 3), { next: [1, 3], result: 'added' }, 'next is sorted');
});

// ── 6. normalizeSelection ─────────────────────────────────────────────────────

test('6. normalizeSelection', () => {
  assert.deepEqual(normalizeSelection(['0', '2', '2'], 3, 3), [0, 2]);
  assert.equal(normalizeSelection(['3'], 3, 3), null);
  assert.equal(normalizeSelection(['0', '1', '2'], 3, 2), null);
  assert.equal(normalizeSelection(['x'], 3, 3), null);
  assert.equal(normalizeSelection(['-1'], 3, 3), null);
  assert.equal(normalizeSelection(['001'], 3, 3), null);
  assert.deepEqual(normalizeSelection([], 3, 3), []);
  assert.deepEqual(normalizeSelection(['2', '0'], 3, 3), [0, 2]);
});

// ── 7. Tally / format ─────────────────────────────────────────────────────────

test('7. tally and formatOptionLines', () => {
  const groups = [{ optionIndex: 0, _count: { _all: 2 } }, { optionIndex: 7, _count: { _all: 9 } }];
  assert.deepEqual(tallyFromGroups(3, groups, { votes: 2, voters: 2 }).counts, [2, 0, 0]);
  assert.equal(tallyFromGroups(3, groups, { votes: 0, voters: 0 }).votes, 2, 'never fewer votes than the bars add up to');
  assert.deepEqual(emptyTally(3), { counts: [0, 0, 0], votes: 0, voters: 0 });
  assert.deepEqual(winners([3, 3, 1]), [0, 1]);
  assert.deepEqual(winners([0, 0]), []);
  assert.equal(percent(1, 3), 33);
  assert.equal(percent(0, 0), 0);
  assert.equal(percent(2, 2), 100);

  const hidden = formatOptionLines(['A', 'B'], { counts: [1, 0], votes: 1 }, { hidden: true });
  assert.ok(!hidden.includes('%'));
  const shown = formatOptionLines(['A', 'B'], { counts: [1, 0], votes: 1 }, { hidden: false, ended: true });
  const [lineA, lineB] = [shown.split('\n').slice(0, 2).join('\n'), shown.split('\n').slice(2).join('\n')];
  assert.ok(lineA.includes('🏆') && lineA.includes('100%') && lineA.includes('1 vote'));
  assert.ok(!lineB.includes('🏆') && lineB.includes('0%') && lineB.includes('0 votes'));
  assert.ok(!formatOptionLines(['A', 'B'], { counts: [1, 0], votes: 1 }, { ended: false }).includes('🏆'), 'no trophy while live');

  const opts = Array.from({ length: 20 }, (_, i) => `${i}${'z'.repeat(79)}`);
  const big = formatOptionLines(opts, { counts: opts.map((_, i) => i * 1000), votes: 190_000 }, { ended: true });
  assert.ok(big.length < 4096, `20 long options fit one embed description (${big.length})`);
});

// ── 8. Components ─────────────────────────────────────────────────────────────

test('8. components', () => {
  const opts20 = Array.from({ length: 20 }, (_, i) => `Option ${i + 1}`);
  const rows = buildPollComponents({ id: cuid, options: opts20, maxChoices: 1 });
  assert.equal(rows.length, 4);
  assert.deepEqual(rows.map((r) => r.components.length), [5, 5, 5, 5]);
  for (const b of rows.flatMap((r) => r.components)) {
    const id = b.toJSON().custom_id;
    assert.match(id, /^poll:vote:c{25}:\d{1,2}$/);
    assert.ok(id.length <= 100);
    assert.deepEqual(parseCustomId(id).params.slice(0, 2), ['vote', cuid]);
    assert.equal(b.toJSON().style, 2);
  }
  assert.equal(buildPollComponents({ id: cuid, options: opts20.slice(0, 7), maxChoices: 1 }).map((r) => r.components.length).join(), '5,2');

  const multi = buildPollComponents({ id: cuid, options: opts20, maxChoices: 3 }, { selected: [1] });
  assert.equal(multi.length, 1);
  const sel = multi[0].components[0].toJSON();
  assert.equal(sel.type, ComponentType.StringSelect);
  assert.equal(sel.custom_id, `poll-select:${cuid}`);
  assert.equal(sel.min_values, 0);
  assert.equal(sel.max_values, 3);
  assert.equal(sel.options.length, 20);
  assert.deepEqual(sel.options.filter((o) => o.default).map((o) => o.value), ['1']);
  assert.equal(sel.placeholder, 'Choose up to 3 answers');
  assert.equal(sel.options[0].label, '1. Option 1');

  for (const poll of [{ id: cuid, options: opts20, maxChoices: 1 }, { id: cuid, options: opts20, maxChoices: 3 }]) {
    const off = buildPollComponents(poll, { disabled: true }).flatMap((r) => r.components);
    assert.ok(off.every((c) => c.toJSON().disabled === true));
  }
  assert.equal(buildPollComponents({ id: cuid, options: opts20, maxChoices: 3 }, { disabled: true })[0].components[0].toJSON().placeholder, 'This poll has ended');

  assert.equal(buttonLabel(['Yes', 'No'], 0), '1. Yes');
  assert.equal(buttonLabel(Array.from({ length: 11 }, (_, i) => `o${i}`), 0), '1');
  assert.equal(buttonLabel(['x'.repeat(26), 'b'], 0), '1', 'a long option falls back to the number');
  // A select option label is cut to 100 characters.
  const longSel = buildPollComponents({ id: cuid, options: ['a'.repeat(80), 'b'], maxChoices: 2 })[0].components[0].toJSON();
  assert.ok(longSel.options.every((o) => o.label.length <= 100));
});

// ── 9. Embeds ─────────────────────────────────────────────────────────────────

test('9. embeds', () => {
  const base = {
    id: cuid, guildId: G(1), channelId: '1', messageId: '2', authorId: HOST, question: 'q'.repeat(256), options: ['A', 'B'],
    maxChoices: 1, allowedRoleIds: [], deniedRoleIds: [], hideResults: false, endsAt: null, status: 'ACTIVE', createdAt: new Date(1_700_000_000_000),
  };
  const tally = { counts: [2, 1], votes: 3, voters: 3 };
  const live = buildPollEmbed(base, tally, { settings: null }).data;
  assert.ok(live.title.length <= 256);
  assert.ok(live.description.includes('67%') && live.description.includes('33%'));
  assert.ok(live.description.includes('Open until a moderator ends it'));
  assert.ok(live.description.includes('**3** votes from **3** members · one choice per member'));
  assert.ok(live.description.includes(`<@${HOST}>`));
  assert.equal(live.footer.text, 'Click a button to vote · click it again to remove your vote');
  assert.equal(live.timestamp, new Date(1_700_000_000_000).toISOString(), 'the timestamp is the creation time');

  const hidden = buildPollEmbed({ ...base, hideResults: true }, tally, { settings: null }).data;
  assert.ok(!hidden.description.includes('%'));
  assert.ok(hidden.description.includes('Results are hidden until the poll ends.'));
  assert.ok(hidden.description.includes('**3** votes'), 'totals are still shown');

  const ended = buildPollEmbed({ ...base, status: 'ENDED', hideResults: true, endsAt: new Date(1_700_000_100_000) }, tally, { settings: null }).data;
  assert.ok(ended.description.includes('Poll ended'));
  assert.ok(ended.description.includes('<t:1700000100:R>'));
  assert.ok(ended.description.includes('%'), 'hidden results are revealed at the end');
  assert.equal(ended.footer.text, 'This poll has ended');

  const roles = buildPollEmbed({ ...base, allowedRoleIds: ['11'], deniedRoleIds: ['22'], maxChoices: 2, endsAt: new Date(1_800_000_000_000) }, tally, { settings: null }).data;
  assert.ok(roles.description.includes('Only <@&11> can vote.') && roles.description.includes("<@&22> can't vote."));
  assert.ok(roles.description.includes('up to 2 choices per member'));
  assert.ok(roles.description.includes('Ends <t:1800000000:R> (<t:1800000000:f>)'));
  assert.equal(roles.footer.text, 'Pick your answers in the menu · each pick replaces your previous votes');

  assert.equal(buildPollEmbed({ ...base, options: ['A', 'B'] }, emptyTally(2), { settings: { embedColor: 0x123456 } }).data.color, 0x123456);

  const none = buildResultsEmbed({ ...base, question: 'Q?' }, { counts: [0, 0], votes: 0, voters: 0 }, { settings: null }).data;
  assert.ok(none.description.includes('No votes were cast.'));
  assert.equal(none.title, '📊 Poll results');
  const tie = buildResultsEmbed({ ...base, options: ['A', 'B', 'C'] }, { counts: [2, 2, 1], votes: 5, voters: 5 }, { settings: null }).data;
  assert.ok(tie.description.includes('Tie between **A** and') || tie.description.includes('Tie between **A**, **B**'));
  assert.ok(tie.description.includes('with **2** votes each.'));
  const one = buildResultsEmbed({ ...base, options: ['A', 'B'] }, { counts: [3, 1], votes: 4, voters: 4 }, { settings: null }).data;
  assert.ok(one.description.includes('🏆 **A** won with **3** votes (75%).'));
  assert.ok(one.description.includes('[Jump to the poll](https://discord.com/channels/'));
  const gone = buildResultsEmbed({ ...base }, { counts: [3, 1], votes: 4, voters: 4 }, { settings: null, messageExists: false }).data;
  assert.ok(!gone.description.includes('Jump to the poll'));
  const peek = buildResultsEmbed({ ...base, hideResults: true, endsAt: new Date(1_800_000_000_000) }, { counts: [3, 1], votes: 4, voters: 4 }, { settings: null, live: true }).data;
  assert.equal(peek.title, '📊 Live results');
  assert.ok(peek.description.includes("🔒 Members can't see these results until the poll ends."));
  assert.ok(peek.description.includes('75%'), 'a moderator sees hidden numbers');
  assert.ok(!peek.description.includes('won with'));

  assert.equal(jumpUrl({ guildId: '1', channelId: '2', messageId: '3' }), 'https://discord.com/channels/1/2/3');
  assert.equal(jumpUrl({ guildId: '1', channelId: '2', messageId: null }), null);

  const payload = buildPollPayload(base, tally, { settings: null });
  assert.deepEqual(payload.allowedMentions, { parse: [] });
  assert.equal(payload.components.length, 1);
  assert.ok(payload.components[0].components.every((c) => !c.toJSON().disabled));
  assert.ok(buildPollPayload(base, tally, { settings: null, ended: true }).components[0].components.every((c) => c.toJSON().disabled));
});

// ── 10. Pending store ─────────────────────────────────────────────────────────

test('10. createPendingStore', () => {
  let t = 1000;
  const store = createPendingStore({ ttlMs: 100, max: 2, now: () => t });
  const v = { a: 1 };
  store.put('a', v);
  assert.equal(store.take('a'), v);
  assert.equal(store.take('a'), null, 'single use');
  store.put('b', 1);
  t += 101;
  assert.equal(store.take('b'), null, 'older than the TTL');
  store.put('1', 1); store.put('2', 2); store.put('3', 3);
  assert.equal(store.size(), 2);
  assert.equal(store.take('1'), null, 'the oldest is evicted');
  assert.equal(store.take('3'), 3);
  store.put('x', 1);
  t += 101;
  store.put('y', 2);
  assert.equal(store.size(), 1, 'put sweeps expired entries');
  assert.equal(pendingPolls.take('never-put'), null);
});

// ── 11. Render throttle ───────────────────────────────────────────────────────

test('11. requestRender: one immediate render and one trailing render per 5 s', async () => {
  mock.timers.enable({ apis: ['setTimeout', 'Date'], now: 1_000_000 });
  try {
    const calls = [];
    __setRenderImpl(async (_c, id) => { calls.push(id); return 'ok'; });
    const client = {};
    requestRender(client, 'p-throttle');
    mock.timers.tick(0);
    await flush();
    assert.equal(calls.length, 1, 'the first request renders at once');

    mock.timers.tick(500);
    requestRender(client, 'p-throttle');
    mock.timers.tick(500);
    requestRender(client, 'p-throttle'); // collapses into the queued trailing render
    await flush();
    assert.equal(calls.length, 1);

    mock.timers.tick(3999); // t = 4999
    await flush();
    assert.equal(calls.length, 1);
    mock.timers.tick(1); // t = 5000
    await flush();
    assert.equal(calls.length, 2, 'exactly one trailing render');
    mock.timers.tick(20_000);
    await flush();
    assert.equal(calls.length, 2);

    // cancelRender clears a queued render.
    requestRender(client, 'p-throttle'); // inside the window of the render at t=5000? lastAt is 5000 and now is 25000 -> immediate
    mock.timers.tick(0);
    await flush();
    assert.equal(calls.length, 3);
    mock.timers.tick(100);
    requestRender(client, 'p-throttle');
    cancelRender('p-throttle');
    mock.timers.tick(10_000);
    await flush();
    assert.equal(calls.length, 3, 'a cancelled render never runs');

    // A request that arrives while a render is in flight queues another one instead of being dropped.
    let release;
    let started = 0;
    __setRenderImpl(() => { started += 1; return new Promise((r) => { release = () => r('ok'); }); });
    requestRender(client, 'p-inflight');
    mock.timers.tick(0);
    await flush();
    assert.equal(started, 1);
    requestRender(client, 'p-inflight'); // the vote committed after the render read the tally
    release();
    await flush();
    mock.timers.tick(5000);
    await flush();
    assert.equal(started, 2, 'the late vote gets its own trailing render');
    release();
    cancelRender('p-inflight');
  } finally {
    __setRenderImpl();
    mock.timers.reset();
  }
});

// ── 12. Plugin ────────────────────────────────────────────────────────────────

test('12. plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('polls')), []);
  assert.ok(plugins.some((p) => p.id === 'polls'));
  const p = pluginMod.plugin;
  assert.equal(p.id, 'polls');
  assert.equal(p.module, 'polls');
  assert.ok(p.hooks.some((h) => h.event === 'messageDelete' && h.id === 'polls' && h.always === true));
  const filter = p.hooks.find((h) => h.event === 'messageDelete').filter;
  assert.equal(filter({ message: { partial: true }, client: { user: { id: 'b' } } }), true);
  assert.equal(filter({ message: { author: { id: 'b' } }, client: { user: { id: 'b' } } }), true);
  assert.equal(filter({ message: { author: { id: 'x' } }, client: { user: { id: 'b' } } }), false);
  // HOOK_ORDER lists polls for all three events, so all three hooks exist (and only those).
  for (const event of ['messageDelete', 'messageDeleteBulk', 'channelDelete']) {
    assert.equal(HOOK_ORDER[event].includes('polls'), p.hooks.some((h) => h.event === event && h.id === 'polls'), event);
  }
  assert.ok(p.hooks.every((h) => h.always === true));
  assert.deepEqual(p.components.map((c) => `${c.type}:${c.prefix}`).sort(), ['button:poll', 'modal:poll-modal', 'select:poll-select']);
  assert.ok(p.components.every((c) => typeof c.handler === 'function'));
  assert.equal(p.jobs.length, 1);
  assert.equal(p.jobs[0].type, 'poll.end');
  assert.deepEqual(p.provides, {});
  await assert.rejects(() => p.jobs[0].handler({ payload: {} }, { client: {} }), PermanentJobError);
});

// ── 13. Command payload ───────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('13. command payload and exports', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'poll');
  assert.deepEqual(json.options.map((o) => o.name), ['create', 'end', 'results', 'list']);
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ModerateMembers));
  assert.deepEqual(json.contexts, [0]);
  assert.ok(charCount(json) <= 900, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'polls');
  assert.equal(cmd.gated, true);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.MODERATOR);
  assert.equal(cmd.allowInDMs, undefined);
  assert.equal(typeof cmd.autocomplete, 'function');
  const create = json.options.find((o) => o.name === 'create');
  assert.deepEqual(create.options.map((o) => o.name), ['duration', 'max_choices', 'allowed_role', 'denied_role', 'hide_results', 'channel']);
  assert.ok(create.options.every((o) => !o.required));
  assert.equal(create.options.find((o) => o.name === 'max_choices').max_value, LIMITS.POLL_OPTIONS);
  for (const sub of ['end', 'results']) {
    const o = json.options.find((x) => x.name === sub).options[0];
    assert.equal(o.name, 'poll');
    assert.equal(o.required, true);
    assert.equal(o.autocomplete, true);
  }
});

// ── 14. RAW_SQL builders ──────────────────────────────────────────────────────

test('14. RAW_SQL builders are Prisma.Sql without a bare now()', () => {
  assert.equal(RAW_SQL.length, 4);
  for (const { name, build } of RAW_SQL) {
    const q = build();
    assert.ok(q instanceof Prisma.sql``.constructor, name);
    assert.ok(!/\bnow\s*\(/i.test(q.sql) && !/CURRENT_TIMESTAMP/i.test(q.sql), name);
  }
  assert.ok(RAW_SQL.find((r) => r.name === 'polls.status').build().sql.includes('FOR SHARE'));
});

// ── 15. /poll create (no I/O before showModal) ────────────────────────────────

test('15. /poll create validates, stores the form and shows the modal without touching the database', async () => {
  const guild = makeGuild(G(2));
  const channel = makeChannel(guild, nextId());
  const env = { guildId: G(2), guild, channel };
  const run = async (opts, mutate) => {
    const i = makeCmd(env, 'create', opts);
    mutate?.(i);
    await cmd.execute(i, makeClient(guild));
    return i;
  };

  for (const [duration, expected] of [['abc', 'Use a duration like'], ['30s', 'at least 1 minute'], ['31d', 'at most 30 days']]) {
    const i = await run({ duration });
    assert.equal(i.modals.length, 0);
    assert.equal(titleOf(i), '❌  Invalid Duration');
    assert.ok(textOf(i).includes(expected));
    assert.equal(lastReply(i).ephemeral, true);
  }

  let i = await run({ denied_role: { id: G(2) } });
  assert.ok(textOf(i).includes("can't deny @everyone"));
  i = await run({ allowed_role: { id: rid(1) }, denied_role: { id: rid(1) } });
  assert.ok(textOf(i).includes('must be different'));
  assert.equal(i.modals.length, 0);

  channel.botPerms = false;
  i = await run({});
  assert.equal(titleOf(i), '❌  Cannot Post There');
  assert.ok(textOf(i).includes(`in <#${channel.id}> to post a poll.`) && textOf(i).includes('**'));
  channel.botPerms = true;
  channel.memberPerms = false;
  i = await run({});
  assert.ok(textOf(i).includes('You need permission to view and send messages'));
  channel.memberPerms = true;
  i = await run({}, (x) => { x.channel = null; x.channelId = 'unknown'; });
  assert.equal(titleOf(i), '❌  Channel Not Found');

  // Success: the form is stored under the interaction id and the modal is shown.
  const before = pendingPolls.size();
  const picked = makeChannel(guild, nextId());
  i = await run({ duration: '2h', max_choices: 3, allowed_role: { id: G(2) }, denied_role: { id: rid(2) }, hide_results: true, channel: { id: picked.id } });
  assert.equal(i.replies.length, 0);
  assert.equal(i.modals.length, 1);
  assert.equal(pendingPolls.size(), before + 1);
  const modal = i.modals[0].toJSON();
  assert.equal(modal.custom_id, `poll-modal:${i.id}`);
  assert.equal(modal.title, 'Create a poll');
  assert.deepEqual(modal.components.map((r) => r.components[0].custom_id), ['poll-question', 'poll-options']);
  assert.equal(modal.components[0].components[0].max_length, 256);
  assert.equal(modal.components[1].components[0].style, 2);
  assert.equal(modal.components[1].components[0].min_length, 3);
  const stored = pendingPolls.take(i.id);
  assert.deepEqual(stored, {
    guildId: G(2), channelId: picked.id, userId: HOST, maxChoices: 3, allowedRoleIds: [], deniedRoleIds: [rid(2)],
    hideResults: true, durationMs: 7_200_000,
  });

  // showModal failing removes the stored form; an expired interaction is silent.
  i = makeCmd(env, 'create', {});
  i.showModal = async () => { throw discordErr(10062); };
  await cmd.execute(i, makeClient(guild));
  assert.equal(pendingPolls.take(i.id), null);
});

test('15b. buildCreateModal and channelProblem', () => {
  const m = buildCreateModal('123').toJSON();
  assert.equal(m.custom_id, 'poll-modal:123');
  assert.equal(m.components[0].components[0].placeholder, 'What should we play tonight?');
  assert.equal(m.components[1].components[0].placeholder, 'Option 1\nOption 2\nOption 3');
  const guild = makeGuild(G(2));
  const ch = makeChannel(guild, nextId());
  assert.equal(channelProblem(ch, makeMember(HOST)), null);
  ch.botPerms = false;
  assert.ok(channelProblem(ch, makeMember(HOST)).data.description.startsWith('I need **'));
  ch.botPerms = true;
  const noText = { ...ch, isTextBased: () => false, guild: ch.guild, permissionsFor: ch.permissionsFor };
  assert.ok(channelProblem(noText, makeMember(HOST)).data.description.startsWith('That is not a text channel'));
  assert.equal(channelProblem(null, makeMember(HOST)).data.title, '❌  Channel Not Found');
});

// ── 16. Modal submit: expired / foreign / invalid forms (no database) ────────

test('16. poll-modal: expired, foreign and invalid forms', async () => {
  const env = { guildId: G(3), guild: makeGuild(G(3)), channel: null };
  env.channel = makeChannel(env.guild, nextId());

  let i = makeModal(env, 'unknown-nonce', 'My question', 'A\nB');
  await handleCreateModal(i, makeClient(env.guild));
  assert.equal(titleOf(i), '❌  Form Expired');
  assert.ok(textOf(i).includes('Your input (copy it to try again):'));
  assert.ok(textOf(i).includes('My question\n---\nA\nB'));
  assert.equal(lastReply(i).ephemeral, true);

  pendingPolls.put('n-foreign', pending({ guildId: G(3), userId: uid(5) }));
  i = makeModal(env, 'n-foreign', 'q', 'A\nB', { userId: HOST });
  await handleCreateModal(i, makeClient(env.guild));
  assert.ok(textOf(i).includes('This form is not yours'));
  assert.equal(pendingPolls.take('n-foreign'), null, 'the nonce is consumed even for a stranger');

  pendingPolls.put('n-other-guild', pending({ guildId: G(4) }));
  i = makeModal(env, 'n-other-guild', 'q', 'A\nB');
  await handleCreateModal(i, makeClient(env.guild));
  assert.ok(textOf(i).includes('This form is not yours'));

  // Invalid: every problem listed, the text echoed with code fences defused, single use.
  pendingPolls.put('n-bad', pending({ guildId: G(3) }));
  i = makeModal(env, 'n-bad', '   ', '```evil```\n```evil```');
  await handleCreateModal(i, makeClient(env.guild));
  assert.equal(titleOf(i), '❌  Invalid Poll');
  const d = textOf(i);
  assert.ok(d.includes('• The question is empty.'));
  assert.ok(d.includes('• Option 2 repeats option 1.'));
  const fenceCount = (d.match(/```/g) ?? []).length;
  assert.equal(fenceCount, 2, 'only our own fence remains');
  assert.ok(d.length <= 4096);
  assert.equal(pendingPolls.take('n-bad'), null);

  // A huge echo stays within the description limit.
  pendingPolls.put('n-huge', pending({ guildId: G(3) }));
  i = makeModal(env, 'n-huge', 'x'.repeat(3000), 'y'.repeat(3000));
  await handleCreateModal(i, makeClient(env.guild));
  assert.ok(textOf(i).length <= 4096);
  assert.ok(textOf(i).endsWith('\n```'));
});

// ── 17. Voting helpers that need no database ──────────────────────────────────

test('17. the vote button rejects malformed customIds without touching the database', async () => {
  const env = { guildId: G(3), guild: makeGuild(G(3)), channel: null };
  for (const customId of ['poll:other:abc:1', 'poll:vote:abc:x', 'poll:vote', 'poll:vote:abc:123']) {
    const i = baseInteraction(env, { customId });
    await handleVoteButton(i, {});
    assert.equal(lastReply(i).content, 'This button is no longer active.', customId);
    assert.equal(lastReply(i).ephemeral, true);
  }
});

// ═══════════════════════════ Database cases ═══════════════════════════════════

test('DB: every RAW_SQL statement executes against the migrated schema', { skip: skipDb }, async () => {
  for (const { name, build } of RAW_SQL) {
    const rows = await prisma.$queryRaw(build());
    assert.ok(Array.isArray(rows), name);
  }
  const [lock] = await prisma.$transaction((tx) => tx.$queryRaw(RAW_SQL[0].build()));
  assert.equal(lock.ok, 1);
});

test('DB: the guild cap is exact under parallel creates', { skip: skipDb }, async () => {
  const env = await makeEnv(1);
  const results = await Promise.all(Array.from({ length: POLL.ACTIVE_MAX + 5 }, (_, i) => createPollRow(
    env.guildId, pending({ guildId: env.guildId, channelId: env.channel.id }), { question: `q${i}`, options: ['a', 'b'], maxChoices: 1 },
  )));
  assert.equal(results.filter((r) => r.ok).length, POLL.ACTIVE_MAX);
  assert.equal(results.filter((r) => !r.ok && r.code === 'LIMIT').length, 5);
  assert.equal(await prisma.poll.count({ where: { guildId: env.guildId, status: 'ACTIVE' } }), POLL.ACTIVE_MAX);
  // Ended polls do not count.
  const one = await prisma.poll.findFirst({ where: { guildId: env.guildId } });
  await prisma.poll.update({ where: { id: one.id }, data: { status: 'ENDED' } });
  const again = await createPollRow(env.guildId, pending({ guildId: env.guildId, channelId: env.channel.id }), { question: 'late', options: ['a', 'b'], maxChoices: 1 });
  assert.equal(again.ok, true);
  assert.equal(again.poll.endsAt, null);
  assert.deepEqual(again.poll.allowedRoleIds, []);
  const timed = await createPollRow(env.guildId, pending({ guildId: env.guildId, durationMs: 3_600_000 }), { question: 'x', options: ['a', 'b'], maxChoices: 1 });
  assert.equal(timed.ok, false, 'the cap is full again');
});

test('DB: on LIMIT, polls in deleted channels are reconciled once and the create retried', { skip: skipDb }, async () => {
  const env = await makeEnv(2);
  const hidden = nextId();
  env.guild.channels.fetchable.set(hidden, { id: hidden }); // exists, just not cached
  const ghost = nextId(); // fetch -> 10003
  const rows = [];
  for (let i = 0; i < POLL.ACTIVE_MAX; i += 1) {
    let channelId = env.channel.id;
    if (i === 0) channelId = ghost;
    if (i === 1) channelId = hidden;
    rows.push({ guildId: env.guildId, channelId, authorId: HOST, question: `q${i}`, options: ['a', 'b'], messageId: nextId() });
  }
  await prisma.poll.createMany({ data: rows });
  const res = await createPoll(env.client, env.guild, pending({ guildId: env.guildId, channelId: env.channel.id }), { question: 'new', options: ['a', 'b'], maxChoices: 1 });
  assert.equal(res.ok, true);
  assert.equal((await prisma.poll.findFirst({ where: { guildId: env.guildId, channelId: ghost } })).status, 'ENDED');
  assert.equal((await prisma.poll.findFirst({ where: { guildId: env.guildId, channelId: hidden } })).status, 'ACTIVE', 'an uncached channel is not a deleted channel');
  assert.equal(env.channel.sent.length, 0, 'a deleted-channel poll gets no message');
  // Nothing left to reconcile: LIMIT.
  const full = await createPoll(env.client, env.guild, pending({ guildId: env.guildId, channelId: env.channel.id }), { question: 'again', options: ['a', 'b'], maxChoices: 1 });
  assert.deepEqual(full, { ok: false, code: 'LIMIT' });

  // An unavailable guild is never judged.
  env.guild.available = false;
  assert.equal(await svc.reconcileDeletedChannels(env.client, env.guild), 0);
  env.guild.available = true;
});

test('DB: single choice: add, switch, remove; replies show only the voter\'s own pick', { skip: skipDb }, async () => {
  const env = await makeEnv(3);
  const poll = await makePoll(env);
  const renders = [];
  __setRenderImpl(async (_c, id) => { renders.push(id); return 'ok'; });
  try {
    const click = async (idx, user = uid(1)) => {
      const i = makeClick(env, poll.id, idx, user);
      await handleVoteButton(i, env.client);
      assert.equal(i.deferred, true);
      assert.equal(i.ephemeral, true, 'acknowledged privately before any database work');
      return i;
    };
    let i = await click(0);
    assert.equal(lastReply(i).content, '✅ You voted for **1. Margherita**.');
    assert.deepEqual(lastReply(i).allowedMentions, { parse: [] });
    assert.deepEqual(await votesOf(poll.id, uid(1)), [0]);
    i = await click(2);
    assert.equal(lastReply(i).content, '🔁 Your vote changed to **3. Hawaiian**.');
    assert.deepEqual(await votesOf(poll.id, uid(1)), [2]);
    i = await click(2);
    assert.equal(lastReply(i).content, '🗑️ Your vote for **3. Hawaiian** was removed.');
    assert.deepEqual(await votesOf(poll.id, uid(1)), []);
    i = await click(5);
    assert.equal(lastReply(i).content, 'That option no longer exists.');
    await click(1, uid(2));
    await click(1, uid(3));
    await click(0, uid(4));
    const tally = await getTally(poll);
    assert.deepEqual(tally, { counts: [1, 2, 0], votes: 3, voters: 3 });
    await sleep(30);
    assert.ok(renders.length >= 1 && renders.every((id) => id === poll.id), 'a change asks for a render');
    // Another guild's poll is not reachable through this guild's click.
    const foreign = makeClick({ ...env, guildId: G(9) }, poll.id, 0, uid(1));
    await handleVoteButton(foreign, env.client);
    assert.equal(lastReply(foreign).content, 'This poll no longer exists.');
    const missing = makeClick(env, 'nope', 0, uid(1));
    await handleVoteButton(missing, env.client);
    assert.equal(lastReply(missing).content, 'This poll no longer exists.');
  } finally {
    stopRenders();
  }
});

test('DB: concurrent clicks never leave a member with two votes in a single-choice poll', { skip: skipDb }, async () => {
  const env = await makeEnv(4);
  const poll = await makePoll(env, { options: ['A', 'B', 'C', 'D'] });
  __setRenderImpl(async () => 'ok');
  try {
    const clicks = Array.from({ length: 24 }, (_, n) => makeClick(env, poll.id, n % 4, uid(1)));
    await Promise.all(clicks.map((c) => handleVoteButton(c, env.client)));
    assert.ok(clicks.every((c) => c.replies.length === 1 && typeof lastReply(c).content === 'string'), 'every click is answered');
    assert.ok((await votesOf(poll.id, uid(1))).length <= 1);
    // 12 different members at once: 12 votes.
    const many = Array.from({ length: 12 }, (_, n) => makeClick(env, poll.id, 0, uid(100 + n)));
    await Promise.all(many.map((c) => handleVoteButton(c, env.client)));
    const tally = await getTally(poll);
    assert.equal(tally.counts[0] >= 12, true);
    assert.equal(tally.votes, tally.counts.reduce((a, b) => a + b, 0));
    // The same member clicking the same option 9 times in parallel ends with 0 or 1 vote, never a duplicate row.
    const same = Array.from({ length: 9 }, () => makeClick(env, poll.id, 3, uid(2)));
    await Promise.all(same.map((c) => handleVoteButton(c, env.client)));
    assert.ok((await votesOf(poll.id, uid(2))).length <= 1);
  } finally {
    stopRenders();
  }
});

test('DB: multi choice: the select replaces the vote set, the limit and the personal ballot', { skip: skipDb }, async () => {
  const env = await makeEnv(5);
  const poll = await makePoll(env, { options: ['A', 'B', 'C', 'D'], maxChoices: 2 });
  __setRenderImpl(async () => 'ok');
  try {
    // From the shared message: deferReply ephemeral, editReply with the personal ballot.
    let i = makeSelect(env, poll.id, uid(1), ['0', '2']);
    await handleVoteSelect(i, env.client);
    assert.equal(i.ephemeral, true);
    assert.equal(i.updates, 0);
    assert.deepEqual(await votesOf(poll.id, uid(1)), [0, 2]);
    assert.equal(lastReply(i).content, '✅ Your votes (2/2): **1. A**, **3. C**');
    let ballot = lastReply(i).components[0].components[0].toJSON();
    assert.equal(ballot.custom_id, `poll-select:${poll.id}`);
    assert.deepEqual(ballot.options.filter((o) => o.default).map((o) => o.value), ['0', '2']);

    // From the personal ballot: deferUpdate and the same message is edited in place (no followUp).
    i = makeSelect(env, poll.id, uid(1), ['1'], { ephemeral: true });
    await handleVoteSelect(i, env.client);
    assert.equal(i.updates, 1);
    assert.equal(i.replies.length, 1);
    assert.ok(!lastReply(i).followUp);
    assert.deepEqual(await votesOf(poll.id, uid(1)), [1], 'the new selection replaces the old one');
    ballot = lastReply(i).components[0].components[0].toJSON();
    assert.deepEqual(ballot.options.filter((o) => o.default).map((o) => o.value), ['1']);

    i = makeSelect(env, poll.id, uid(1), []);
    await handleVoteSelect(i, env.client);
    assert.equal(lastReply(i).content, '🗑️ Your votes were cleared.');
    assert.deepEqual(await votesOf(poll.id, uid(1)), []);

    i = makeSelect(env, poll.id, uid(1), ['0', '1', '2']);
    await handleVoteSelect(i, env.client);
    assert.equal(lastReply(i).content, 'That selection is not valid for this poll.');
    i = makeSelect(env, poll.id, uid(1), ['9']);
    await handleVoteSelect(i, env.client);
    assert.equal(lastReply(i).content, 'That selection is not valid for this poll.');
    assert.deepEqual(await votesOf(poll.id, uid(1)), []);

    // The toggle path (applyVote with idx) honours maxChoices.
    const a = await applyVote({ pollId: poll.id, userId: uid(2), maxChoices: 2, idx: 0 });
    const b = await applyVote({ pollId: poll.id, userId: uid(2), maxChoices: 2, idx: 1 });
    const c = await applyVote({ pollId: poll.id, userId: uid(2), maxChoices: 2, idx: 2 });
    assert.deepEqual([a.result, b.result, c.result], ['added', 'added', 'limit']);
    assert.deepEqual(c.next, [0, 1]);
    assert.equal(c.changed, false);
    const d = await applyVote({ pollId: poll.id, userId: uid(2), maxChoices: 2, idx: 0 });
    assert.equal(d.result, 'removed');
    assert.deepEqual(await votesOf(poll.id, uid(2)), [1]);
    // A select on an ended poll from the ballot removes the ballot.
    await endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId });
    i = makeSelect(env, poll.id, uid(1), ['0'], { ephemeral: true });
    await handleVoteSelect(i, env.client);
    assert.equal(lastReply(i).content, 'This poll has ended.');
    assert.deepEqual(lastReply(i).components, []);
    assert.deepEqual(await votesOf(poll.id, uid(1)), []);
  } finally {
    stopRenders();
  }
});

test('DB: allowed and denied roles are checked at vote time', { skip: skipDb }, async () => {
  const env = await makeEnv(6, { roles: [rid(1), rid(2)] });
  const poll = await makePoll(env, { allowedRoleIds: [rid(1)], deniedRoleIds: [rid(2)] });
  __setRenderImpl(async () => 'ok');
  try {
    let i = makeClick(env, poll.id, 0, uid(1), []);
    await handleVoteButton(i, env.client);
    assert.equal(lastReply(i).content, `Only members with <@&${rid(1)}> can vote in this poll.`);
    i = makeClick(env, poll.id, 0, uid(2), [rid(1), rid(2)]);
    await handleVoteButton(i, env.client);
    assert.equal(lastReply(i).content, `Members with <@&${rid(2)}> can't vote in this poll.`);
    i = makeClick(env, poll.id, 0, uid(3), [rid(1)]);
    await handleVoteButton(i, env.client);
    assert.ok(lastReply(i).content.startsWith('✅ You voted'));
    assert.deepEqual(await votesOf(poll.id), [`${uid(3)}:0`]);

    // The allowed role was deleted: nobody can vote (the poll is not opened up).
    const gone = await makePoll(env, { allowedRoleIds: [rid(7)] });
    i = makeClick(env, gone.id, 0, uid(3), [rid(7)]);
    await handleVoteButton(i, env.client);
    assert.ok(lastReply(i).content.startsWith('The role allowed to vote in this poll no longer exists.'));
    assert.deepEqual(await votesOf(gone.id), []);

    // Select path, same rules, answered in place.
    const multi = await makePoll(env, { maxChoices: 2, allowedRoleIds: [rid(1)] });
    const s = makeSelect(env, multi.id, uid(1), ['0']);
    await handleVoteSelect(s, env.client);
    assert.equal(lastReply(s).content, `Only members with <@&${rid(1)}> can vote in this poll.`);
    assert.equal(lastReply(s).components, undefined, 'a refusal on the shared menu keeps nothing to edit');
  } finally {
    stopRenders();
  }
});

test('DB: votes after the end are refused; an ended poll with live buttons is healed; an overdue poll is ended', { skip: skipDb }, async () => {
  const env = await makeEnv(7);
  __setRenderImpl(async () => 'ok');
  try {
    const poll = await makePoll(env);
    const r = await endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId });
    assert.equal(r.ok, true);
    const liveMessage = { id: poll.messageId, components: [{ components: [{ disabled: false }] }], edits: [], async edit(p) { this.edits.push(p); } };
    let i = makeClick(env, poll.id, 0, uid(1), [], liveMessage);
    await handleVoteButton(i, env.client);
    assert.equal(lastReply(i).content, 'This poll has ended.');
    assert.deepEqual(await votesOf(poll.id), []);
    assert.equal(liveMessage.edits.length, 1, 'the stale message is re-rendered with disabled components');
    assert.ok(liveMessage.edits[0].components[0].components.every((c) => c.toJSON().disabled));
    // Already disabled, or someone else's message: no edit.
    const doneMessage = { id: poll.messageId, components: [{ components: [{ disabled: true }] }], edits: [], async edit(p) { this.edits.push(p); } };
    i = makeClick(env, poll.id, 0, uid(1), [], doneMessage);
    await handleVoteButton(i, env.client);
    assert.equal(doneMessage.edits.length, 0);
    const otherMessage = { id: nextId(), components: [{ components: [{ disabled: false }] }], edits: [], async edit(p) { this.edits.push(p); } };
    i = makeClick(env, poll.id, 0, uid(1), [], otherMessage);
    await handleVoteButton(i, env.client);
    assert.equal(otherMessage.edits.length, 0);

    // endsAt passed but the end job never ran: the click ends the poll.
    const overdue = await makePoll(env, { endsAt: new Date(Date.now() - 5000) });
    i = makeClick(env, overdue.id, 0, uid(1));
    await handleVoteButton(i, env.client);
    assert.equal(lastReply(i).content, 'This poll has ended.');
    assert.equal((await reload(overdue.id)).status, 'ENDED');
    assert.equal(env.channel.sent.filter((s) => s.embeds[0].data.title === '📊 Poll results').length >= 2, true, 'results were posted');
  } finally {
    stopRenders();
  }
});

test('DB: a vote racing the end is either counted in the final results or refused', { skip: skipDb }, async () => {
  const env = await makeEnv(8);
  __setRenderImpl(async () => 'ok');
  try {
    const poll = await makePoll(env, { options: ['A', 'B'] });
    const clicks = Array.from({ length: 10 }, (_, n) => makeClick(env, poll.id, n % 2, uid(10 + n)));
    const [ended] = await Promise.all([
      endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId }),
      ...clicks.map((c) => handleVoteButton(c, env.client)),
    ]);
    assert.equal(ended.ok, true);
    const rows = await prisma.pollVote.count({ where: { pollId: poll.id } });
    const confirmed = clicks.filter((c) => lastReply(c).content.startsWith('✅')).length;
    const refused = clicks.filter((c) => lastReply(c).content === 'This poll has ended.').length;
    assert.equal(confirmed + refused, 10);
    assert.equal(rows, confirmed, 'every confirmed vote is stored, no refused vote is');
    assert.equal(ended.tally.votes, rows, 'the final tally has every committed vote');
  } finally {
    stopRenders();
  }
});

test('DB: endPoll has exactly one winner, edits the message and posts the results', { skip: skipDb }, async () => {
  const env = await makeEnv(9);
  const poll = await makePoll(env, { endsAt: new Date(Date.now() + 3_600_000) });
  await prisma.pollVote.createMany({ data: [{ pollId: poll.id, userId: uid(1), optionIndex: 1 }, { pollId: poll.id, userId: uid(2), optionIndex: 1 }, { pollId: poll.id, userId: uid(3), optionIndex: 0 }] });
  await scheduleJob({ guildId: env.guildId, type: 'poll.end', runAt: poll.endsAt, payload: { pollId: poll.id }, dedupeKey: endJobKey(poll.id) });
  assert.ok(await getJob(endJobKey(poll.id)));

  const results = await Promise.all(Array.from({ length: 5 }, () => endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId })));
  assert.equal(results.filter((r) => r.ok).length, 1);
  assert.equal(results.filter((r) => !r.ok && r.code === 'ALREADY_ENDED').length, 4);
  const win = results.find((r) => r.ok);
  assert.equal(win.posted, true);
  assert.equal(win.messageGone, false);
  assert.deepEqual(win.tally, { counts: [1, 2, 0], votes: 3, voters: 3 });

  const row = await reload(poll.id);
  assert.equal(row.status, 'ENDED');
  assert.ok(row.endsAt.getTime() <= Date.now() + 1000 && row.endsAt.getTime() > Date.now() - 60_000, 'endsAt is overwritten with the actual end');
  assert.equal(env.channel.edits.length, 1);
  assert.equal(env.channel.edits[0].id, poll.messageId);
  assert.ok(env.channel.edits[0].components[0].components.every((c) => c.toJSON().disabled));
  assert.ok(env.channel.edits[0].embeds[0].data.description.includes('Poll ended'));
  assert.equal(env.channel.sent.length, 1);
  const sent = env.channel.sent[0];
  assert.equal(sent.embeds[0].data.title, '📊 Poll results');
  assert.ok(sent.embeds[0].data.description.includes('🏆 **Pepperoni** won with **2** votes (67%).'));
  assert.deepEqual(sent.allowedMentions, { parse: [] });
  assert.deepEqual(sent.reply, { messageReference: poll.messageId, failIfNotExists: false });
  assert.equal(await getJob(endJobKey(poll.id)), null, 'a manual end cancels the end timer');

  assert.deepEqual(await endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId }), { ok: false, code: 'ALREADY_ENDED' });
  assert.deepEqual(await endPoll(env.client, 'nope', { reason: 'manual', guildId: env.guildId }), { ok: false, code: 'NOT_FOUND' });
  const other = await makePoll(env);
  assert.deepEqual(await endPoll(env.client, other.id, { reason: 'manual', guildId: G(50) }), { ok: false, code: 'NOT_FOUND' }, 'guild scoped');
  assert.equal((await reload(other.id)).status, 'ACTIVE');
});

test('DB: endPoll with a deleted message, a deleted channel, the module off, send failures and an unavailable guild', { skip: skipDb }, async () => {
  const env = await makeEnv(10);

  // reason 'deleted': no edit, no post.
  let poll = await makePoll(env);
  let r = await endPoll(env.client, poll.id, { reason: 'deleted', guildId: env.guildId });
  assert.deepEqual([r.ok, r.posted, r.messageGone], [true, false, true]);
  assert.equal(env.channel.edits.length + env.channel.sent.length, 0);

  // The message is gone (10008 on the edit): results are still posted, without a reply reference or a link.
  poll = await makePoll(env, { ghostMessage: true });
  r = await endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId });
  assert.deepEqual([r.ok, r.posted, r.messageGone], [true, true, true]);
  assert.equal(env.channel.sent.length, 1);
  assert.equal(env.channel.sent[0].reply, undefined);
  assert.ok(!env.channel.sent[0].embeds[0].data.description.includes('Jump to the poll'));

  // The module is off: the poll message is finalised, but no results message is posted.
  await updateSettings(env.guildId, { pollsEnabled: false });
  poll = await makePoll(env);
  const sentBefore = env.channel.sent.length;
  r = await endPoll(env.client, poll.id, { reason: 'timer', guildId: env.guildId });
  assert.deepEqual([r.ok, r.posted], [true, false]);
  assert.equal(env.channel.sent.length, sentBefore);
  assert.equal(env.channel.edits.at(-1).id, poll.messageId);
  await updateSettings(env.guildId, { pollsEnabled: true });

  // A failing send never throws; the poll is still ended.
  poll = await makePoll(env);
  env.channel.failSend = discordErr(50013);
  r = await endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId });
  assert.deepEqual([r.ok, r.posted], [true, false]);
  assert.equal((await reload(poll.id)).status, 'ENDED');
  env.channel.failSend = null;

  // The bot cannot post there any more.
  poll = await makePoll(env);
  env.channel.botPerms = false;
  const sent = env.channel.sent.length;
  r = await endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId });
  assert.equal(r.posted, false);
  assert.equal(env.channel.sent.length, sent);
  env.channel.botPerms = true;

  // An unreachable edit (missing permissions) is not a deletion: the results are still attempted.
  poll = await makePoll(env);
  env.channel.failEdit = discordErr(50013);
  r = await endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId });
  assert.deepEqual([r.ok, r.posted, r.messageGone], [true, true, false]);
  env.channel.failEdit = null;

  // The channel was deleted (REST 10003): no edit, no post.
  poll = await makePoll(env, { channelId: nextId() });
  r = await endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId });
  assert.deepEqual([r.ok, r.posted, r.messageGone], [true, false, true]);

  // An unavailable guild is never judged: the poll ends in the database, nothing is touched on Discord.
  poll = await makePoll(env);
  env.guild.available = false;
  const edits = env.channel.edits.length;
  r = await endPoll(env.client, poll.id, { reason: 'manual', guildId: env.guildId });
  assert.deepEqual([r.ok, r.posted], [true, false]);
  assert.equal(env.channel.edits.length, edits);
  assert.equal((await reload(poll.id)).status, 'ENDED');
  env.guild.available = true;
});

test('DB: the live render edits the message with fresh counts, skips ended polls and ends polls whose message is gone', { skip: skipDb }, async () => {
  const env = await makeEnv(11);
  const poll = await makePoll(env);
  await prisma.pollVote.createMany({ data: [{ pollId: poll.id, userId: uid(1), optionIndex: 2 }] });
  requestRender(env.client, poll.id);
  assert.ok(await waitFor(() => env.channel.edits.length === 1), 'the first request renders at once');
  assert.equal(env.channel.edits[0].id, poll.messageId);
  assert.ok(env.channel.edits[0].embeds[0].data.description.includes('100%'));
  assert.ok(env.channel.edits[0].components[0].components.every((c) => !c.toJSON().disabled));
  cancelRender(poll.id);

  // Ended polls are never re-rendered by the live path (the final edit belongs to endPoll).
  await prisma.poll.update({ where: { id: poll.id }, data: { status: 'ENDED' } });
  requestRender(env.client, poll.id);
  await sleep(150);
  assert.equal(env.channel.edits.length, 1);
  cancelRender(poll.id);

  // The message was deleted behind our back: the render ends the poll silently.
  const lost = await makePoll(env, { ghostMessage: true });
  requestRender(env.client, lost.id);
  assert.ok(await waitFor(async () => (await reload(lost.id)).status === 'ENDED'));
  assert.equal(env.channel.sent.length, 0, 'no results post for a deleted poll');
  cancelRender(lost.id);

  // Missing permissions: warned, skipped, the poll stays active.
  const noPerm = await makePoll(env);
  env.channel.failEdit = discordErr(50013);
  requestRender(env.client, noPerm.id);
  await sleep(150);
  assert.equal((await reload(noPerm.id)).status, 'ACTIVE');
  env.channel.failEdit = null;
  cancelRender(noPerm.id);

  // An unavailable guild: skipped.
  const asleep = await makePoll(env);
  const edits = env.channel.edits.length;
  env.guild.available = false;
  requestRender(env.client, asleep.id);
  await sleep(150);
  env.guild.available = true;
  assert.equal(env.channel.edits.length, edits);
  assert.equal((await reload(asleep.id)).status, 'ACTIVE');
  cancelRender(asleep.id);
  stopRenders();
});

test('DB: the delete hooks end the poll without a results message', { skip: skipDb }, async () => {
  const env = await makeEnv(12);
  const other = makeChannel(env.guild, nextId());
  const ctx = (fields) => ({ client: env.client, guildId: env.guildId, guild: env.guild, ...fields });

  const p1 = await makePoll(env);
  await onMessageDelete(ctx({ message: { id: p1.messageId } }));
  assert.equal((await reload(p1.id)).status, 'ENDED');
  await onMessageDelete(ctx({ message: { id: p1.messageId } })); // already ended: no-op
  await onMessageDelete(ctx({ message: { id: nextId() } }));
  await onMessageDelete(ctx({ message: {} }));
  const p0 = await makePoll(env);
  await onMessageDelete({ client: env.client, guildId: G(50), message: { id: p0.messageId } });
  assert.equal((await reload(p0.id)).status, 'ACTIVE', 'another guild\'s delete event does not reach it');

  const b1 = await makePoll(env);
  const b2 = await makePoll(env);
  const b3 = await makePoll(env);
  await onMessageDeleteBulk(ctx({ messages: new Map([[b1.messageId, {}], [b2.messageId, {}], [nextId(), {}]]) }));
  assert.deepEqual([(await reload(b1.id)).status, (await reload(b2.id)).status, (await reload(b3.id)).status], ['ENDED', 'ENDED', 'ACTIVE']);
  await onMessageDeleteBulk(ctx({ messages: new Map() }));

  const c1 = await makePoll(env, { channelId: other.id });
  const c2 = await makePoll(env, { channelId: other.id });
  const c3 = await makePoll(env);
  await onChannelDelete(ctx({ channel: { id: other.id } }));
  assert.deepEqual([(await reload(c1.id)).status, (await reload(c2.id)).status, (await reload(c3.id)).status], ['ENDED', 'ENDED', 'ACTIVE']);
  assert.equal(env.channel.sent.length + env.channel.edits.length + other.sent.length + other.edits.length, 0);
});

test('DB: the poll.end job ends the poll, is idempotent and keeps its own row', { skip: skipDb }, async () => {
  const env = await makeEnv(13);
  const ctx = { client: env.client };
  await assert.rejects(() => handleEndJob({ guildId: env.guildId, payload: {} }, ctx), PermanentJobError);
  await assert.rejects(() => handleEndJob({ guildId: env.guildId, payload: { pollId: 5 } }, ctx), PermanentJobError);
  await handleEndJob({ guildId: env.guildId, payload: { pollId: 'does-not-exist' } }, ctx); // NOT_FOUND: no-op

  const poll = await makePoll(env, { endsAt: new Date(Date.now() + 60_000) });
  await scheduleJob({ guildId: env.guildId, type: 'poll.end', runAt: poll.endsAt, payload: { pollId: poll.id }, dedupeKey: endJobKey(poll.id) });
  await handleEndJob({ guildId: env.guildId, payload: { pollId: poll.id }, dedupeKey: endJobKey(poll.id) }, ctx);
  assert.equal((await reload(poll.id)).status, 'ENDED');
  assert.equal(env.channel.sent.length, 1);
  assert.ok(await getJob(endJobKey(poll.id)), 'the scheduler marks its own row done');
  await handleEndJob({ guildId: env.guildId, payload: { pollId: poll.id } }, ctx); // re-run: ALREADY_ENDED, nothing more is posted
  assert.equal(env.channel.sent.length, 1);
  assert.equal(env.channel.edits.length, 1);
  await handleEndJob({ guildId: G(50), payload: { pollId: poll.id } }, ctx);
});

test('DB: poll-modal posts the poll, stores the message id and schedules the end', { skip: skipDb }, async () => {
  const env = await makeEnv(14);
  const put = (nonce, over = {}) => pendingPolls.put(nonce, pending({ guildId: env.guildId, channelId: env.channel.id, ...over }));

  put('m1', { durationMs: 3_600_000, hideResults: true, allowedRoleIds: [rid(1)], deniedRoleIds: [rid(2)] });
  let i = makeModal(env, 'm1', '  Best pizza?  ', '1. Margherita\n2. Pepperoni\n- Hawaiian');
  await handleCreateModal(i, env.client);
  assert.equal(i.deferred, true);
  assert.equal(i.ephemeral, true);
  assert.equal(titleOf(i), '✅  Poll Posted');
  assert.equal(env.channel.sent.length, 1);
  const sent = env.channel.sent[0];
  const row = await prisma.poll.findFirst({ where: { guildId: env.guildId } });
  assert.deepEqual(
    { question: row.question, options: row.options, maxChoices: row.maxChoices, hide: row.hideResults, allowed: row.allowedRoleIds, denied: row.deniedRoleIds, messageId: row.messageId, authorId: row.authorId, channelId: row.channelId },
    { question: 'Best pizza?', options: ['Margherita', 'Pepperoni', 'Hawaiian'], maxChoices: 1, hide: true, allowed: [rid(1)], denied: [rid(2)], messageId: sent.id, authorId: HOST, channelId: env.channel.id },
  );
  assert.equal(sent.components[0].components.length, 3);
  assert.deepEqual(sent.allowedMentions, { parse: [] });
  assert.ok(!sent.embeds[0].data.description.includes('%'), 'hidden results show no numbers');
  assert.ok(textOf(i).includes(`Your poll is live in <#${env.channel.id}>. [Jump to poll](https://discord.com/channels/${env.guildId}/${env.channel.id}/${sent.id})`));
  assert.ok(textOf(i).includes(`It ends <t:${Math.floor(row.endsAt.getTime() / 1000)}:R>.`));
  const job = await getJob(endJobKey(row.id));
  assert.equal(job.type, 'poll.end');
  assert.equal(job.guildId, env.guildId);
  assert.equal(job.runAt.getTime(), row.endsAt.getTime());
  assert.deepEqual(job.payload, { pollId: row.id });
  assert.equal(pendingPolls.take('m1'), null, 'the form is single use');

  // Max choices larger than the number of options is lowered; no duration = no job.
  put('m2', { maxChoices: 5 });
  i = makeModal(env, 'm2', 'Two options', 'A\nB');
  await handleCreateModal(i, env.client);
  assert.ok(textOf(i).includes('Max choices was lowered to **2** (the number of options).'));
  assert.ok(!textOf(i).includes('It ends'));
  const lowered = await prisma.poll.findFirst({ where: { guildId: env.guildId, question: 'Two options' } });
  assert.equal(lowered.maxChoices, 2);
  assert.equal(lowered.endsAt, null);
  assert.equal(await getJob(endJobKey(lowered.id)), null);
  assert.equal(env.channel.sent.at(-1).components[0].components[0].toJSON().type, ComponentType.StringSelect);

  // The bot cannot post at submit time (permissions changed since the command ran).
  put('m3');
  env.channel.botPerms = false;
  i = makeModal(env, 'm3', 'No perms', 'A\nB');
  await handleCreateModal(i, env.client);
  assert.equal(titleOf(i), '❌  Cannot Post There');
  env.channel.botPerms = true;
  assert.equal(await prisma.poll.count({ where: { guildId: env.guildId, question: 'No perms' } }), 0);

  // The channel vanished.
  put('m4', { channelId: nextId() });
  i = makeModal(env, 'm4', 'Gone channel', 'A\nB');
  await handleCreateModal(i, env.client);
  assert.equal(titleOf(i), '❌  Channel Not Found');

  // A failing send removes the row again and explains why.
  put('m5');
  env.channel.failSend = discordErr(50013);
  i = makeModal(env, 'm5', 'Send fails', 'A\nB');
  await handleCreateModal(i, env.client);
  assert.equal(titleOf(i), '❌  Could Not Post Poll');
  assert.ok(textOf(i).includes("I'm missing permissions there."));
  assert.equal(await prisma.poll.count({ where: { guildId: env.guildId, question: 'Send fails' } }), 0);
  env.channel.failSend = discordErr(50083);
  put('m5b');
  i = makeModal(env, 'm5b', 'Archived', 'A\nB');
  await handleCreateModal(i, env.client);
  assert.ok(textOf(i).includes('that thread is archived.'));
  env.channel.failSend = null;

  // The cap.
  const have = await prisma.poll.count({ where: { guildId: env.guildId, status: 'ACTIVE' } });
  await prisma.poll.createMany({
    data: Array.from({ length: POLL.ACTIVE_MAX - have }, (_, n) => ({ guildId: env.guildId, channelId: env.channel.id, authorId: HOST, question: `fill${n}`, options: ['a', 'b'], messageId: nextId() })),
  });
  put('m6');
  i = makeModal(env, 'm6', 'One too many', 'A\nB');
  await handleCreateModal(i, env.client);
  assert.equal(titleOf(i), '❌  Too Many Polls');
  assert.ok(textOf(i).includes(`**${POLL.ACTIVE_MAX}** active polls`));
  assert.equal(await prisma.poll.count({ where: { guildId: env.guildId, question: 'One too many' } }), 0);
});

test('DB: /poll end, /poll results and /poll list', { skip: skipDb }, async () => {
  const env = await makeEnv(15);
  const poll = await makePoll(env, { hideResults: true, endsAt: new Date(Date.now() + 3_600_000) });
  await prisma.pollVote.createMany({ data: [{ pollId: poll.id, userId: uid(1), optionIndex: 0 }, { pollId: poll.id, userId: uid(2), optionIndex: 0 }, { pollId: poll.id, userId: uid(3), optionIndex: 1 }] });

  // results: a moderator sees hidden numbers.
  let i = makeCmd(env, 'results', { poll: poll.id });
  await cmd.execute(i, env.client);
  assert.equal(i.ephemeral, true);
  assert.equal(titleOf(i), '📊 Live results');
  assert.ok(textOf(i).includes('67%') && textOf(i).includes("🔒 Members can't see these results until the poll ends."));
  assert.equal((await reload(poll.id)).status, 'ACTIVE', 'results never end a live poll');

  // list
  const polls = await listActivePolls(env.guildId);
  assert.equal(polls.polls.length, 1);
  assert.equal(polls.votes.get(poll.id), 3);
  i = makeCmd(env, 'list');
  await cmd.execute(i, env.client);
  assert.equal(titleOf(i), '📊 Active polls (1/50)');
  assert.ok(textOf(i).includes('[Best pizza?](https://discord.com/channels/'));
  assert.ok(textOf(i).includes(`<#${env.channel.id}> · 3 votes · ends <t:`));

  // end: wrong id, then the real one, then again.
  i = makeCmd(env, 'end', { poll: 'nope' });
  await cmd.execute(i, env.client);
  assert.equal(titleOf(i), '❌  Poll Not Found');
  i = makeCmd({ ...env, guildId: G(50) }, 'end', { poll: poll.id });
  await cmd.execute(i, env.client);
  assert.equal(titleOf(i), '❌  Poll Not Found', 'guild scoped');
  i = makeCmd(env, 'end', { poll: poll.id });
  await cmd.execute(i, env.client);
  assert.equal(i.ephemeral, true);
  assert.equal(titleOf(i), '✅  Poll Ended');
  assert.equal(textOf(i), `The results were posted in <#${env.channel.id}>.`);
  assert.equal(env.channel.sent.length, 1);
  i = makeCmd(env, 'end', { poll: poll.id });
  await cmd.execute(i, env.client);
  assert.equal(titleOf(i), '⚠️  Already Ended');
  env.channel.botPerms = false;
  const second = await makePoll(env);
  i = makeCmd(env, 'end', { poll: second.id });
  await cmd.execute(i, env.client);
  assert.equal(textOf(i), `The poll is closed, but I couldn't post the results in <#${env.channel.id}>.`);
  env.channel.botPerms = true;

  // results of an ended poll shows the outcome and no "live" lines; empty list.
  i = makeCmd(env, 'results', { poll: poll.id });
  await cmd.execute(i, env.client);
  assert.equal(titleOf(i), '📊 Poll results');
  assert.ok(textOf(i).includes('won with **2** votes'));
  i = makeCmd(env, 'list');
  await cmd.execute(i, env.client);
  assert.equal(titleOf(i), 'ℹ️  No Active Polls');

  // results self-heal: overdue by more than a minute ends the poll first.
  const overdue = await makePoll(env, { endsAt: new Date(Date.now() - 120_000) });
  i = makeCmd(env, 'results', { poll: overdue.id });
  await cmd.execute(i, env.client);
  assert.equal(titleOf(i), '📊 Poll results');
  assert.equal((await reload(overdue.id)).status, 'ENDED');
  const justOver = await makePoll(env, { endsAt: new Date(Date.now() - 10_000) });
  i = makeCmd(env, 'results', { poll: justOver.id });
  await cmd.execute(i, env.client);
  assert.equal(titleOf(i), '📊 Live results');
  assert.equal((await reload(justOver.id)).status, 'ACTIVE', 'less than a minute late: left for the job');
});

test('DB: autocomplete for end and results', { skip: skipDb }, async () => {
  const env = await makeEnv(16);
  const otherChannel = makeChannel(env.guild, nextId());
  await makePoll(env, { question: 'Favourite colour?' });
  await makePoll(env, { question: 'Favourite colour of cars?', channelId: otherChannel.id });
  await makePoll(env, { question: 'Pineapple on pizza?', status: 'ENDED' });
  await makePoll(await makeEnv(17), { question: 'Favourite elsewhere' });

  const ask = async (sub, value) => {
    const i = makeCmd(env, sub, { focused: { name: 'poll', value } });
    await cmd.autocomplete(i);
    return i.responded;
  };
  let c = await ask('end', '');
  assert.equal(c.length, 1, 'end: only active polls of this channel');
  assert.match(c[0].name, /^Favourite colour\? · \d+s ago$/);
  c = await ask('end', 'FAVOURITE');
  assert.equal(c.length, 1);
  c = await ask('results', '');
  assert.equal(c.length, 3, 'results: every poll of the guild, nothing from another guild');
  assert.ok(c.some((x) => x.name.startsWith('🏁 Pineapple on pizza? · #')));
  assert.ok(c.some((x) => x.name.startsWith('🟢 Favourite colour of cars? · #chan-')));
  c = await ask('results', 'colour');
  assert.equal(c.length, 2);
  assert.ok(c.every((x) => typeof x.value === 'string' && x.value.length <= 100));
  const rows = await findPollsForAutocomplete({ guildId: env.guildId, channelId: env.channel.id, sub: 'results', q: 'pine' });
  assert.equal(rows.length, 1);
  assert.deepEqual(Object.keys(rows[0]).sort(), ['channelId', 'createdAt', 'id', 'question', 'status']);

  // Another option focused, or no subcommand match: nothing. A failing query answers [] instead of throwing.
  const i = makeCmd(env, 'end', { focused: { name: 'other', value: '' } });
  await cmd.autocomplete(i);
  assert.deepEqual(i.responded, []);
  const j = makeCmd(env, 'list', { focused: { name: 'poll', value: '' } });
  await cmd.autocomplete(j);
  assert.deepEqual(j.responded, []);
});

test('pollListLines formats one line per poll', () => {
  const now = Date.now();
  const mk = (over) => ({ id: over.id, guildId: '1', channelId: '2', messageId: '3', question: 'Q [x]', endsAt: null, ...over });
  const lines = pollListLines([
    mk({ id: 'a' }),
    mk({ id: 'b', endsAt: new Date(now + 60_000) }),
    mk({ id: 'c', endsAt: new Date(now - 60_000), messageId: null, question: 'z'.repeat(100) }),
  ], new Map([['a', 1], ['b', 1234]]), now);
  assert.equal(lines[0], '**1.** [Q \\[x\\]](https://discord.com/channels/1/2/3) — <#2> · 1 vote · no end time');
  assert.ok(lines[1].includes('· 1,234 votes · ends <t:'));
  assert.ok(lines[2].includes(`${'z'.repeat(79)}…`) && lines[2].includes('· 0 votes · ending…') && !lines[2].includes(']('));
});
