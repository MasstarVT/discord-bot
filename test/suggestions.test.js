// Suggestions tests. Offline (no Discord, Postgres or Redis): the pure rules, the numbered create (cooldown, thread, rollback),
// the vote transaction and its totals, the debounced edit, the review and delete flows, the hooks, the persistent components
// and the /suggestion payload and subcommands, all against in-memory Suggestion / SuggestionVote tables and fake channels.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const { PermissionFlagsBits, ChannelType } = await import('discord.js');
const svc = await import('../src/services/suggestionService.js');
const sugg = await import('../src/plugins/suggestions.js');
const cmd = await import('../src/commands/community/suggestion.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { CUSTOM_ID_PREFIXES } = await import('../src/config/customIds.js');
const { LIMITS, PERMISSION_LEVELS, DEFAULT_MODULE_SETTINGS } = await import('../src/config/constants.js');
const { MODULES } = await import('../src/config/modules.js');
const { updateSettings } = await import('../src/services/settingsService.js');

const {
  parseContent, cooldownRemainingMs, toggleVote, jumpUrl, isVotingOpen, STATUS_INFO, SUGGESTION, RAW_SQL,
  buildSuggestionEmbed, buildSuggestionComponents, buildSuggestionModal, buildReviewDm, submitProblem, moduleOffEmbed,
  createSuggestion, applyVote, handleVoteButton, handleSuggestionModal, requestRender, cancelRender, renderNow,
  reviewSuggestion, deleteSuggestion, onMessageDelete, onChannelDelete, __setRenderImpl, __setRenderInterval,
} = svc;
const { plugin } = sugg;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory tables ──────────────────────────────────────────────────────────

const suggestions = new Map(); // id -> row
const votes = new Map();       // `${suggestionId}|${userId}` -> row
const settingsRows = new Map();
const txLog = [];              // statements seen by $queryRaw, in order
let idSeq = 1;

const copy = (row) => (row ? { ...row } : null);
const matches = (row, where = {}) => Object.entries(where).every(([k, v]) => {
  if (v && typeof v === 'object' && !(v instanceof Date)) {
    if ('not' in v) return row[k] !== v.not;
    return false;
  }
  return row[k] === v;
});
const missing = () => Object.assign(new Error('Record not found'), { code: 'P2025' });

const suggestionModel = {
  findUnique: async ({ where }) => {
    if (where.id) return copy(suggestions.get(where.id));
    const g = where.guildId_number;
    return copy([...suggestions.values()].find((r) => r.guildId === g.guildId && r.number === g.number));
  },
  findFirst: async ({ where, orderBy }) => {
    const rows = [...suggestions.values()].filter((r) => matches(r, where));
    if (orderBy?.createdAt === 'desc') rows.sort((a, b) => b.createdAt - a.createdAt);
    return copy(rows[0]);
  },
  findMany: async ({ where }) => [...suggestions.values()].filter((r) => matches(r, where)).map(copy),
  create: async ({ data }) => {
    if ([...suggestions.values()].some((r) => r.guildId === data.guildId && r.number === data.number)) {
      throw Object.assign(new Error('Unique constraint failed'), { code: 'P2002' });
    }
    const row = {
      id: `s${idSeq++}`, status: 'OPEN', upvotes: 0, downvotes: 0, anonymous: false, messageId: null, threadId: null,
      reviewerId: null, reviewNote: null, reviewedAt: null, createdAt: new Date(), updatedAt: new Date(), ...data,
    };
    suggestions.set(row.id, row);
    return copy(row);
  },
  update: async ({ where, data }) => {
    const row = suggestions.get(where.id);
    if (!row) throw missing();
    Object.assign(row, data, { updatedAt: new Date() });
    return copy(row);
  },
  updateMany: async ({ where, data }) => {
    const rows = [...suggestions.values()].filter((r) => matches(r, where));
    for (const r of rows) Object.assign(r, data);
    return { count: rows.length };
  },
  deleteMany: async ({ where }) => {
    const rows = [...suggestions.values()].filter((r) => matches(r, where));
    for (const r of rows) {
      suggestions.delete(r.id);
      for (const k of [...votes.keys()]) if (k.startsWith(`${r.id}|`)) votes.delete(k);
    }
    return { count: rows.length };
  },
};
const voteKey = (where) => `${where.suggestionId_userId.suggestionId}|${where.suggestionId_userId.userId}`;
const voteModel = {
  findUnique: async ({ where }) => copy(votes.get(voteKey(where))),
  create: async ({ data }) => {
    votes.set(`${data.suggestionId}|${data.userId}`, { ...data });
    return copy(data);
  },
  update: async ({ where, data }) => {
    const row = votes.get(voteKey(where));
    if (!row) throw missing();
    Object.assign(row, data);
    return copy(row);
  },
  deleteMany: async ({ where }) => {
    const hit = votes.delete(`${where.suggestionId}|${where.userId}`);
    return { count: hit ? 1 : 0 };
  },
  count: async ({ where }) => [...votes.values()].filter((v) => v.suggestionId === where.suggestionId && v.value === where.value).length,
};
const queryRaw = async (q) => {
  const sql = q.sql;
  txLog.push(sql.includes('pg_advisory_xact_lock') ? `lock:${q.values[0]}` : sql.includes('FOR UPDATE') ? `rowlock:${q.values[0]}` : 'next');
  if (sql.includes('pg_advisory_xact_lock')) return [{ ok: 1 }];
  if (sql.includes('FOR UPDATE')) {
    const row = suggestions.get(q.values[0]);
    return row ? [{ status: row.status }] : [];
  }
  const max = Math.max(0, ...[...suggestions.values()].filter((r) => r.guildId === q.values[0]).map((r) => r.number));
  return [{ next: max + 1 }];
};
// Transactions run one at a time, like the advisory lock / row lock would make them.
let txChain = Promise.resolve();
const txClient = { suggestion: suggestionModel, suggestionVote: voteModel, $queryRaw: queryRaw };
const transaction = (fn) => {
  const run = txChain.then(() => fn(txClient));
  txChain = run.then(() => {}, () => {});
  return run;
};

const real = {
  suggestion: prisma.suggestion, suggestionVote: prisma.suggestionVote, guildSettings: prisma.guildSettings,
  $transaction: prisma.$transaction, $queryRaw: prisma.$queryRaw,
};
prisma.suggestion = suggestionModel;
prisma.suggestionVote = voteModel;
prisma.$queryRaw = queryRaw;
prisma.$transaction = transaction;
prisma.guildSettings = {
  findUnique: async ({ where }) => copy(settingsRows.get(where.guildId)),
  upsert: async ({ where, update }) => {
    const row = { ...(settingsRows.get(where.guildId) ?? { guildId: where.guildId }), ...update };
    settingsRows.set(where.guildId, row);
    return copy(row);
  },
};

after(async () => {
  Object.assign(prisma, real);
  __setRenderImpl();
  __setRenderInterval();
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

let nextId = 5000;
const sleep = (ms) => new Promise((r) => { setTimeout(r, ms); });

function fakeChannel(id, { guildId = 'G', deny = [], failSend = null, threadFails = false } = {}) {
  const denied = new Set(deny.map((k) => PermissionFlagsBits[k]));
  const ch = {
    id, guildId, sent: [], edits: [], deleted: [], threads: [], editError: null, deleteError: null,
    guild: { members: { me: {} } },
    permissionsFor: () => ({ has: (bit) => !denied.has(bit) }),
    isTextBased: () => true,
    isThread: () => false,
    async send(payload) {
      if (failSend) throw failSend;
      const message = {
        id: String(nextId++),
        deleted: false,
        async delete() { this.deleted = true; },
        async startThread(opts) {
          if (threadFails) throw Object.assign(new Error('boom'), { code: 50035 });
          const thread = { id: `T${nextId++}`, name: opts.name, deleted: false, async delete() { this.deleted = true; } };
          ch.threads.push(thread);
          return thread;
        },
      };
      ch.sent.push({ payload, message });
      return message;
    },
  };
  ch.messages = {
    async edit(messageId, payload) {
      if (ch.editError) throw ch.editError;
      ch.edits.push({ messageId, payload });
    },
    async delete(messageId) {
      if (ch.deleteError) throw ch.deleteError;
      ch.deleted.push(messageId);
    },
  };
  return ch;
}

function fakeWorld(guildId, channels = []) {
  const chCache = new Map(channels.map((c) => [c.id, c]));
  const members = new Map();
  const dms = [];
  const users = new Map();
  const guild = {
    id: guildId, name: 'Test Guild', available: true,
    channels: { cache: chCache, fetch: async (id) => chCache.get(id) ?? null },
    members: { cache: members, me: {} },
  };
  const client = {
    user: { id: 'BOT' },
    guilds: { cache: new Map([[guildId, guild]]) },
    channels: { cache: chCache, fetch: async (id) => { const c = chCache.get(id); if (!c) throw Object.assign(new Error('Unknown Channel'), { code: 10003 }); return c; } },
    users: {
      cache: users,
      fetch: async (id) => { const u = users.get(id); if (!u) throw new Error('no user'); return u; },
    },
  };
  const addUser = (id, name, { dmFails = false } = {}) => {
    const user = {
      id, username: name, displayName: name, displayAvatarURL: () => `https://cdn.test/${id}.png`,
      async send(payload) { if (dmFails) throw Object.assign(new Error('Cannot send messages to this user'), { code: 50007 }); dms.push({ to: id, payload }); },
    };
    users.set(id, user);
    members.set(id, { id, displayName: name, user, displayAvatarURL: user.displayAvatarURL });
    return user;
  };
  return { guild, client, addUser, dms };
}

const SETTINGS = Object.freeze({
  suggestionsEnabled: true, suggestionChannelId: 'CH', suggestionThreads: true, suggestionAllowAnonymous: false,
  suggestionDmOnUpdate: true, suggestionCooldownSeconds: 0,
});

/** Fake interaction: records replies. `vals` feeds the option getters; the customId (for components) and the fields too. */
function fakeInteraction({ guild, user, sub = null, vals = {}, customId = null, fields = {}, message = null } = {}) {
  const i = {
    id: String(nextId++), createdTimestamp: Date.now(), guild, guildId: guild?.id, user, member: { id: user?.id }, customId, message,
    deferred: false, replied: false, ephemeral: null, replies: [], modals: [],
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => null,
      get: (n) => (vals[n] === undefined ? null : { name: n, value: vals[n] }),
      getString: (n) => vals[n] ?? null,
      getBoolean: (n) => vals[n] ?? null,
      getInteger: (n) => vals[n] ?? null,
      getChannel: (n) => vals[n] ?? null,
    },
    fields: { getTextInputValue: (n) => fields[n] ?? '' },
    async deferReply({ ephemeral } = {}) { this.deferred = true; this.ephemeral = Boolean(ephemeral); },
    async reply(p) { this.replied = true; this.replies.push(p); return { resource: { message: null } }; },
    async editReply(p) { this.replies.push(p); },
    async followUp(p) { this.replies.push(p); },
    async showModal(m) { this.modals.push(m); },
  };
  return i;
}
const last = (i) => i.replies.at(-1);
const lastEmbed = (i) => last(i).embeds[0].toJSON();
const seedSettings = (guildId, data) => updateSettings(guildId, data);

// ── Pure rules ────────────────────────────────────────────────────────────────

test('parseContent trims and enforces the length limit', () => {
  assert.deepEqual(parseContent('  add a pizza channel \n'), { ok: true, content: 'add a pizza channel' });
  assert.deepEqual(parseContent('   '), { ok: false, error: 'empty' });
  assert.deepEqual(parseContent(null), { ok: false, error: 'empty' });
  assert.equal(parseContent('x'.repeat(LIMITS.SUGGESTION_LENGTH)).ok, true);
  assert.deepEqual(parseContent('x'.repeat(LIMITS.SUGGESTION_LENGTH + 1)), { ok: false, error: 'too-long' });
});

test('cooldownRemainingMs counts from the last suggestion; 0 or no history means none', () => {
  const now = 1_000_000;
  assert.equal(cooldownRemainingMs(new Date(now - 10_000), 60, now), 50_000);
  assert.equal(cooldownRemainingMs(new Date(now - 60_000), 60, now), 0);
  assert.equal(cooldownRemainingMs(new Date(now - 1), 0, now), 0);
  assert.equal(cooldownRemainingMs(null, 300, now), 0);
});

test('toggleVote: same removes, other switches, none adds', () => {
  assert.deepEqual(toggleVote(0, 1), { next: 1, result: 'added' });
  assert.deepEqual(toggleVote(1, 1), { next: 0, result: 'removed' });
  assert.deepEqual(toggleVote(1, -1), { next: -1, result: 'switched' });
  assert.deepEqual(toggleVote(-1, -1), { next: 0, result: 'removed' });
  assert.deepEqual(toggleVote(-1, 1), { next: 1, result: 'switched' });
});

test('voting closes on APPROVED, DENIED and IMPLEMENTED; OPEN and CONSIDERED stay open', () => {
  assert.deepEqual(['OPEN', 'CONSIDERED', 'APPROVED', 'DENIED', 'IMPLEMENTED'].map(isVotingOpen), [true, true, false, false, false]);
  for (const key of Object.keys(STATUS_INFO)) assert.ok(STATUS_INFO[key].label && STATUS_INFO[key].emoji, key);
});

test('jumpUrl needs a message id', () => {
  assert.equal(jumpUrl({ guildId: 'G', channelId: 'C', messageId: 'M' }), 'https://discord.com/channels/G/C/M');
  assert.equal(jumpUrl({ guildId: 'G', channelId: 'C', messageId: null }), null);
});

test('submitProblem: module off first (the router wording), then no channel, then anonymous', () => {
  const off = submitProblem({ ...SETTINGS, suggestionsEnabled: false });
  assert.equal(off.toJSON().description,
    `The **${MODULES.suggestions.label}** module is turned off on this server. An admin can enable it with \`/module enable suggestions\`.`);
  assert.equal(moduleOffEmbed().toJSON().description, off.toJSON().description);
  assert.match(submitProblem({ ...SETTINGS, suggestionChannelId: null }).toJSON().title, /No Suggestion Channel/);
  assert.match(submitProblem(SETTINGS, { anonymous: true }).toJSON().title, /Anonymous Not Allowed/);
  assert.equal(submitProblem({ ...SETTINGS, suggestionAllowAnonymous: true }, { anonymous: true }), null);
  assert.equal(submitProblem(SETTINGS), null);
});

const row = (over = {}) => ({
  id: 'abc123', guildId: 'G', number: 7, authorId: 'U1', anonymous: false, content: 'Add a pizza channel', status: 'OPEN',
  channelId: 'CH', messageId: 'M1', threadId: null, upvotes: 3, downvotes: 1, reviewerId: null, reviewNote: null, reviewedAt: null,
  createdAt: new Date(), ...over,
});

test('the post: author, content, status colour, counts, staff response', () => {
  const open = buildSuggestionEmbed(row(), { author: { name: 'Alice', iconURL: 'https://cdn.test/a.png' } }).toJSON();
  assert.equal(open.title, '💡 Suggestion #7');
  assert.equal(open.description, 'Add a pizza channel');
  assert.deepEqual([open.author.name, open.author.icon_url], ['Alice', 'https://cdn.test/a.png']);
  assert.deepEqual(open.fields.map((f) => [f.name, f.value]), [['Status', '🗳️ Open'], ['Votes', '👍 3  ·  👎 1']]);
  assert.equal(open.color, STATUS_INFO.OPEN.color);

  const done = buildSuggestionEmbed(row({ status: 'APPROVED', reviewerId: 'MOD', reviewNote: 'Great idea', reviewedAt: new Date() }), {}).toJSON();
  assert.equal(done.color, STATUS_INFO.APPROVED.color);
  assert.notEqual(done.color, open.color);
  assert.equal(done.fields[0].value, '✅ Approved');
  assert.equal(done.fields[2].value, 'Great idea\n— <@MOD>');
  assert.equal(buildSuggestionEmbed(row({ status: 'DENIED', reviewerId: 'MOD', reviewedAt: new Date() })).toJSON().fields[2].value, '*No note.*\n— <@MOD>');
});

test('an anonymous post leaks neither the author id nor the name', () => {
  const json = JSON.stringify(buildSuggestionEmbed(row({ anonymous: true, authorId: 'SECRET-ID' }), { author: { name: 'Alice' } }).toJSON());
  assert.ok(json.includes('"Anonymous"'));
  assert.ok(!json.includes('SECRET-ID'));
  assert.ok(!json.includes('Alice'));
  const buttons = JSON.stringify(buildSuggestionComponents(row({ anonymous: true, authorId: 'SECRET-ID' })).map((r) => r.toJSON()));
  assert.ok(!buttons.includes('SECRET-ID'));
});

test('buttons: customIds carry the id, labels carry the counts, closed suggestions are disabled', () => {
  const [open] = buildSuggestionComponents(row()).map((r) => r.toJSON());
  assert.deepEqual(open.components.map((c) => [c.custom_id, c.label, c.disabled ?? false]), [
    ['suggestion:abc123:up', '3', false], ['suggestion:abc123:down', '1', false],
  ]);
  for (const status of ['APPROVED', 'DENIED', 'IMPLEMENTED']) {
    const [closed] = buildSuggestionComponents(row({ status })).map((r) => r.toJSON());
    assert.ok(closed.components.every((c) => c.disabled === true), status);
  }
  const [considered] = buildSuggestionComponents(row({ status: 'CONSIDERED' })).map((r) => r.toJSON());
  assert.ok(considered.components.every((c) => !c.disabled));
});

test('the form: customId carries the anonymous flag, the text field is registered', () => {
  const anon = buildSuggestionModal(true).toJSON();
  const plain = buildSuggestionModal(false).toJSON();
  assert.equal(anon.custom_id, 'suggestion-modal:1');
  assert.equal(plain.custom_id, 'suggestion-modal:0');
  const input = anon.components[0].components[0];
  assert.equal(input.custom_id, 'suggestion-text');
  assert.equal(input.max_length, LIMITS.SUGGESTION_LENGTH);
  assert.equal(CUSTOM_ID_PREFIXES['suggestion-text'].kind, 'field');
});

test('the review DM names the status and quotes the suggestion', () => {
  const dm = buildReviewDm('My Server', row({ status: 'IMPLEMENTED', reviewNote: 'Live now' })).toJSON();
  assert.match(dm.title, /Your suggestion #7 was implemented/);
  assert.match(dm.description, /> Add a pizza channel/);
  assert.match(dm.description, /Live now/);
  assert.match(dm.description, /discord\.com\/channels\/G\/CH\/M1/);
  assert.equal(dm.footer.text, 'My Server');
});

test('RAW_SQL builders are unique, named suggestions.* and never use a bare now()', () => {
  const names = RAW_SQL.map((r) => r.name);
  assert.equal(new Set(names).size, names.length);
  assert.deepEqual(names.sort(), ['suggestions.lockRow', 'suggestions.nextNumber', 'suggestions.numberLock']);
  for (const { build } of RAW_SQL) {
    const q = build();
    assert.equal(typeof q.sql, 'string');
    assert.ok(!/\bnow\s*\(/i.test(q.sql));
  }
  assert.match(RAW_SQL.find((r) => r.name === 'suggestions.nextNumber').build().sql, /::int/);
  assert.match(RAW_SQL.find((r) => r.name === 'suggestions.lockRow').build().sql, /FOR UPDATE/);
});

// ── Create ────────────────────────────────────────────────────────────────────

test('create: numbers are per guild and sequential, the lock comes first, the post never pings', async () => {
  const chA = fakeChannel('CH', { guildId: 'GA' });
  const chB = fakeChannel('CH', { guildId: 'GB' });
  const a = fakeWorld('GA', [chA]);
  const b = fakeWorld('GB', [chB]);
  a.addUser('U1', 'Alice');
  b.addUser('U1', 'Alice');
  const settings = { ...SETTINGS, suggestionThreads: false };
  txLog.length = 0;
  const out = [];
  for (const w of [a, a, b, a]) {
    out.push(await createSuggestion({ client: w.client, guild: w.guild, user: w.client.users.cache.get('U1'), content: 'idea', settings }));
  }
  assert.deepEqual(out.map((o) => o.ok), [true, true, true, true]);
  assert.deepEqual(out.map((o) => o.suggestion.number), [1, 2, 1, 3]);
  assert.deepEqual(txLog.slice(0, 2), ['lock:suggestion-number:GA', 'next'], 'the per-guild lock is taken before the number is read');
  assert.equal(chA.sent.length, 3);
  assert.deepEqual(chA.sent[0].payload.allowedMentions, { parse: [] });
  assert.equal(out[0].suggestion.messageId, chA.sent[0].message.id);
  assert.equal(chA.sent[0].payload.embeds[0].toJSON().author.name, 'Alice');
  assert.equal(chA.sent[0].payload.components[0].toJSON().components[0].custom_id, `suggestion:${out[0].suggestion.id}:up`);
});

test('create: parallel submissions from different members get distinct numbers', async () => {
  const ch = fakeChannel('CH', { guildId: 'GP' });
  const w = fakeWorld('GP', [ch]);
  const users = ['A', 'B', 'C', 'D', 'E', 'F'].map((id) => w.addUser(id, id));
  const results = await Promise.all(users.map((user) => createSuggestion({
    client: w.client, guild: w.guild, user, content: `idea ${user.id}`, settings: { ...SETTINGS, suggestionThreads: false },
  })));
  assert.ok(results.every((r) => r.ok));
  assert.deepEqual(results.map((r) => r.suggestion.number).sort(), [1, 2, 3, 4, 5, 6]);
});

test('create: the cooldown refuses a second suggestion, per member, and allows it once it has passed', async () => {
  const ch = fakeChannel('CH', { guildId: 'GC' });
  const w = fakeWorld('GC', [ch]);
  const alice = w.addUser('U1', 'Alice');
  const bob = w.addUser('U2', 'Bob');
  const settings = { ...SETTINGS, suggestionThreads: false, suggestionCooldownSeconds: 300 };
  const first = await createSuggestion({ client: w.client, guild: w.guild, user: alice, content: 'one', settings });
  assert.equal(first.ok, true);
  const again = await createSuggestion({ client: w.client, guild: w.guild, user: alice, content: 'two', settings });
  assert.equal(again.ok, false);
  assert.equal(again.code, 'COOLDOWN');
  assert.ok(again.data.ms > 290_000 && again.data.ms <= 300_000);
  assert.equal(ch.sent.length, 1, 'nothing was posted');
  assert.equal((await createSuggestion({ client: w.client, guild: w.guild, user: bob, content: 'mine', settings })).ok, true, 'another member is unaffected');

  suggestions.get(first.suggestion.id).createdAt = new Date(Date.now() - 301_000);
  assert.equal((await createSuggestion({ client: w.client, guild: w.guild, user: alice, content: 'three', settings })).ok, true);
  const free = await createSuggestion({ client: w.client, guild: w.guild, user: alice, content: 'four', settings: { ...settings, suggestionCooldownSeconds: 0 } });
  assert.equal(free.ok, true, 'a zero cooldown means none');
});

test('create: refuses without a channel, anonymous when not allowed, and an unusable channel; anonymous is stored when allowed', async () => {
  const ch = fakeChannel('CH', { guildId: 'GR', deny: ['EmbedLinks'] });
  const w = fakeWorld('GR', [ch]);
  const user = w.addUser('U1', 'Alice');
  const base = { client: w.client, guild: w.guild, user, content: 'x' };
  assert.equal((await createSuggestion({ ...base, settings: { ...SETTINGS, suggestionChannelId: null } })).code, 'NO_CHANNEL');
  assert.equal((await createSuggestion({ ...base, anonymous: true, settings: SETTINGS })).code, 'ANONYMOUS');
  const blocked = await createSuggestion({ ...base, settings: SETTINGS });
  assert.equal(blocked.code, 'CHANNEL');
  assert.match(blocked.data.reason, /Embed Links/);
  assert.equal((await createSuggestion({ ...base, settings: { ...SETTINGS, suggestionChannelId: 'NOPE' } })).code, 'CHANNEL');
  assert.equal([...suggestions.values()].filter((r) => r.guildId === 'GR').length, 0, 'no row was left behind');

  const open = fakeChannel('CH2', { guildId: 'GR' });
  w.guild.channels.cache.set('CH2', open);
  const anon = await createSuggestion({
    ...base, anonymous: true, settings: { ...SETTINGS, suggestionChannelId: 'CH2', suggestionAllowAnonymous: true, suggestionThreads: false },
  });
  assert.equal(anon.ok, true);
  assert.equal(suggestions.get(anon.suggestion.id).anonymous, true);
  assert.equal(suggestions.get(anon.suggestion.id).authorId, 'U1', 'staff can still look the author up');
  assert.equal(open.sent[0].payload.embeds[0].toJSON().author.name, 'Anonymous');
});

test('create: a failed send removes the row, so it does not cost the cooldown or a number', async () => {
  const ch = fakeChannel('CH', { guildId: 'GF', failSend: Object.assign(new Error('Missing Permissions'), { code: 50013 }) });
  const w = fakeWorld('GF', [ch]);
  const user = w.addUser('U1', 'Alice');
  const settings = { ...SETTINGS, suggestionThreads: false, suggestionCooldownSeconds: 300 };
  const failed = await createSuggestion({ client: w.client, guild: w.guild, user, content: 'x', settings });
  assert.equal(failed.ok, false);
  assert.equal(failed.code, 'SEND');
  assert.match(failed.data.reason, /missing permissions/);
  assert.equal([...suggestions.values()].filter((r) => r.guildId === 'GF').length, 0);
  ch.send = fakeChannel('X').send.bind(ch);
  const retry = await createSuggestion({ client: w.client, guild: w.guild, user, content: 'x', settings });
  assert.equal(retry.ok, true);
  assert.equal(retry.suggestion.number, 1);
});

test('create: opens a thread named after the number, or says why not', async () => {
  const ch = fakeChannel('CH', { guildId: 'GT' });
  const w = fakeWorld('GT', [ch]);
  const user = w.addUser('U1', 'Alice');
  const ok = await createSuggestion({ client: w.client, guild: w.guild, user, content: 'x', settings: SETTINGS });
  assert.equal(ok.threadNote, null);
  assert.equal(ch.threads[0].name, 'Suggestion #1');
  assert.equal(suggestions.get(ok.suggestion.id).threadId, ch.threads[0].id);

  const noPerm = fakeChannel('CH', { guildId: 'GT2', deny: ['CreatePublicThreads'] });
  const w2 = fakeWorld('GT2', [noPerm]);
  const u2 = w2.addUser('U1', 'Alice');
  const skipped = await createSuggestion({ client: w2.client, guild: w2.guild, user: u2, content: 'x', settings: SETTINGS });
  assert.equal(skipped.ok, true, 'the suggestion is still posted');
  assert.match(skipped.threadNote, /Create Public Threads/);
  assert.equal(noPerm.threads.length, 0);

  const broken = fakeChannel('CH', { guildId: 'GT3', threadFails: true });
  const w3 = fakeWorld('GT3', [broken]);
  const u3 = w3.addUser('U1', 'Alice');
  const failed = await createSuggestion({ client: w3.client, guild: w3.guild, user: u3, content: 'x', settings: SETTINGS });
  assert.equal(failed.ok, true);
  assert.match(failed.threadNote, /couldn't open a discussion thread/);

  const off = await createSuggestion({ client: w.client, guild: w.guild, user, content: 'y', settings: { ...SETTINGS, suggestionThreads: false } });
  assert.equal(off.threadNote, null);
  assert.equal(ch.threads.length, 1, 'no thread when the option is off');
});

// ── Votes ─────────────────────────────────────────────────────────────────────

async function postOne(guildId, { settings = {}, world = null } = {}) {
  const ch = world?.channel ?? fakeChannel('CH', { guildId });
  const w = world?.w ?? fakeWorld(guildId, [ch]);
  const user = w.client.users.cache.get('AUTHOR') ?? w.addUser('AUTHOR', 'Author');
  const out = await createSuggestion({ client: w.client, guild: w.guild, user, content: 'vote on me', settings: { ...SETTINGS, suggestionThreads: false, ...settings } });
  assert.equal(out.ok, true);
  return { ch, w, s: out.suggestion };
}
const counts = (id) => {
  const rows = [...votes.values()].filter((v) => v.suggestionId === id);
  return { up: rows.filter((v) => v.value === 1).length, down: rows.filter((v) => v.value === -1).length };
};

test('votes: one per member; the same vote removes it, the other switches it; totals match SuggestionVote', async () => {
  const { s } = await postOne('GV1');
  const vote = (userId, value) => applyVote({ suggestionId: s.id, userId, value });

  let r = await vote('A', 1);
  assert.deepEqual([r.result, r.upvotes, r.downvotes], ['added', 1, 0]);
  r = await vote('B', 1);
  assert.deepEqual([r.result, r.upvotes, r.downvotes], ['added', 2, 0]);
  r = await vote('C', -1);
  assert.deepEqual([r.result, r.upvotes, r.downvotes], ['added', 2, 1]);
  r = await vote('A', 1);
  assert.deepEqual([r.result, r.upvotes, r.downvotes], ['removed', 1, 1], 'clicking the same vote again removes it');
  r = await vote('B', -1);
  assert.deepEqual([r.result, r.upvotes, r.downvotes], ['switched', 0, 2]);
  r = await vote('C', 1);
  assert.deepEqual([r.result, r.upvotes, r.downvotes], ['switched', 1, 1]);

  const stored = suggestions.get(s.id);
  assert.deepEqual({ up: stored.upvotes, down: stored.downvotes }, counts(s.id));
  assert.equal([...votes.values()].filter((v) => v.suggestionId === s.id && v.userId === 'B').length, 1, 'one row per member');
  assert.ok(txLog.includes(`rowlock:${s.id}`), 'the suggestion row is locked');
});

test('votes: a burst of concurrent votes keeps the totals consistent', async () => {
  const { s } = await postOne('GV2');
  const users = Array.from({ length: 25 }, (_, i) => `V${i}`);
  await Promise.all(users.map((u, i) => applyVote({ suggestionId: s.id, userId: u, value: i % 3 === 0 ? -1 : 1 })));
  await Promise.all(users.slice(0, 10).map((u) => applyVote({ suggestionId: s.id, userId: u, value: 1 })));
  const stored = suggestions.get(s.id);
  assert.deepEqual({ up: stored.upvotes, down: stored.downvotes }, counts(s.id));
  assert.equal(stored.upvotes + stored.downvotes, [...votes.values()].filter((v) => v.suggestionId === s.id).length);
});

test('votes: closed once APPROVED, DENIED or IMPLEMENTED; CONSIDERED keeps voting open; a deleted row is gone', async () => {
  const { s } = await postOne('GV3');
  for (const status of ['APPROVED', 'DENIED', 'IMPLEMENTED']) {
    suggestions.get(s.id).status = status;
    const r = await applyVote({ suggestionId: s.id, userId: 'A', value: 1 });
    assert.deepEqual([r.result, r.status], ['closed', status]);
  }
  assert.equal([...votes.values()].some((v) => v.suggestionId === s.id), false, 'a closed vote stores nothing');
  suggestions.get(s.id).status = 'CONSIDERED';
  assert.equal((await applyVote({ suggestionId: s.id, userId: 'A', value: 1 })).result, 'added');
  assert.equal((await applyVote({ suggestionId: 'missing', userId: 'A', value: 1 })).result, 'gone');
});

test('vote button: replies in words and asks for one render per change', async () => {
  const { w, s } = await postOne('GV4');
  const rendered = [];
  __setRenderInterval(0);
  __setRenderImpl(async (c, id) => { rendered.push(id); return 'ok'; });
  try {
    const click = async (userId, dir) => {
      const i = fakeInteraction({ guild: w.guild, user: { id: userId }, customId: `suggestion:${s.id}:${dir}` });
      await handleVoteButton(i, w.client);
      return i;
    };
    let i = await click('A', 'up');
    assert.equal(i.ephemeral, true);
    assert.match(last(i).content, /You upvoted suggestion #1/);
    i = await click('A', 'down');
    assert.match(last(i).content, /changed to 👎/);
    i = await click('A', 'down');
    assert.match(last(i).content, /vote on suggestion #1 was removed/);
    await sleep(20);
    assert.ok(rendered.length >= 1 && rendered.every((id) => id === s.id));

    suggestions.get(s.id).status = 'DENIED';
    i = await click('B', 'up');
    assert.match(last(i).content, /Voting on suggestion #1 is closed \(Denied\)/);
    assert.equal([...votes.values()].filter((v) => v.suggestionId === s.id && v.userId === 'B').length, 0);

    const bad = fakeInteraction({ guild: w.guild, user: { id: 'A' }, customId: 'suggestion:whatever:sideways' });
    await handleVoteButton(bad, w.client);
    assert.match(last(bad).content, /no longer active/);
    const gone = fakeInteraction({ guild: w.guild, user: { id: 'A' }, customId: 'suggestion:missing:up' });
    await handleVoteButton(gone, w.client);
    assert.match(last(gone).content, /no longer exists/);
    const other = fakeInteraction({ guild: { id: 'ELSEWHERE' }, user: { id: 'A' }, customId: `suggestion:${s.id}:up` });
    await handleVoteButton(other, w.client);
    assert.match(last(other).content, /no longer exists/, 'another guild cannot vote');
  } finally {
    __setRenderImpl();
    __setRenderInterval();
  }
});

// ── Debounced edit ────────────────────────────────────────────────────────────

test('render debounce: the first request renders at once, a burst collapses into one trailing render', async () => {
  const calls = [];
  __setRenderInterval(60);
  __setRenderImpl(async (c, id) => { calls.push([id, Date.now()]); await sleep(2); return 'ok'; });
  try {
    requestRender({}, 'D1');
    await sleep(20);
    assert.equal(calls.length, 1, 'rendered at once');
    for (let n = 0; n < 40; n++) requestRender({}, 'D1');
    await sleep(25);
    assert.equal(calls.length, 1, 'held inside the window');
    await sleep(120);
    assert.equal(calls.length, 2, 'one trailing render for the whole burst');
    assert.ok(calls[1][1] - calls[0][1] >= 55, 'at least one window apart');
    requestRender({}, 'D2');
    await sleep(15);
    assert.equal(calls.length, 3, 'another suggestion is independent');
  } finally {
    __setRenderImpl();
    __setRenderInterval();
  }
});

test('render debounce: cancelRender drops a queued render', async () => {
  const calls = [];
  __setRenderInterval(50);
  __setRenderImpl(async (c, id) => { calls.push(id); return 'ok'; });
  try {
    requestRender({}, 'D3');
    await sleep(15);
    requestRender({}, 'D3');
    cancelRender('D3');
    await sleep(100);
    assert.deepEqual(calls, ['D3']);
  } finally {
    __setRenderImpl();
    __setRenderInterval();
  }
});

test('a vote burst edits the post a few times, and the last edit shows the final counts', async () => {
  const { ch, w, s } = await postOne('GV5');
  __setRenderInterval(40);
  try {
    const users = Array.from({ length: 12 }, (_, i) => `B${i}`);
    for (const u of users) {
      const i = fakeInteraction({ guild: w.guild, user: { id: u }, customId: `suggestion:${s.id}:up` });
      await handleVoteButton(i, w.client);
    }
    await sleep(200);
    assert.ok(ch.edits.length >= 1 && ch.edits.length <= 4, `${ch.edits.length} edits for 12 votes`);
    const finalEmbed = ch.edits.at(-1).payload.embeds[0].toJSON();
    assert.equal(finalEmbed.fields[1].value, '👍 12  ·  👎 0');
    assert.equal(ch.edits.at(-1).payload.components[0].toJSON().components[0].label, '12');
    assert.equal(ch.edits.at(-1).messageId, suggestions.get(s.id).messageId);
    assert.deepEqual(ch.edits.at(-1).payload.allowedMentions, { parse: [] });
  } finally {
    __setRenderInterval();
  }
});

test('render: a message Discord no longer has clears the message id; no access keeps it', async () => {
  const { ch, w, s } = await postOne('GV6');
  ch.editError = Object.assign(new Error('Missing Permissions'), { code: 50013 });
  assert.equal(await renderNow(w.client, s.id), 'skip');
  assert.ok(suggestions.get(s.id).messageId, 'kept');
  ch.editError = Object.assign(new Error('Unknown Message'), { code: 10008 });
  assert.equal(await renderNow(w.client, s.id), 'gone');
  assert.equal(suggestions.get(s.id).messageId, null, 'a purged post is forgotten');
  assert.equal(await renderNow(w.client, s.id), 'skip', 'nothing to edit without a message');
  const t = await postOne('GV6b');
  t.ch.editError = new Error('network');
  assert.equal(await renderNow(t.w.client, t.s.id), 'error', 'unexpected errors are reported, never thrown');
});

// ── Review ────────────────────────────────────────────────────────────────────

test('review: status, note and reviewer are saved, the post is edited, the author gets a DM', async () => {
  const { ch, w, s } = await postOne('GR1');
  const mod = w.addUser('MOD', 'Mod');
  const out = await reviewSuggestion({
    client: w.client, guild: w.guild, number: 1, status: 'APPROVED', note: '  Great idea  ', reviewer: mod, settings: SETTINGS,
  });
  assert.equal(out.ok, true);
  assert.equal(out.previous, 'OPEN');
  assert.equal(out.edited, true);
  assert.equal(out.dm, 'sent');
  const stored = suggestions.get(s.id);
  assert.deepEqual([stored.status, stored.reviewNote, stored.reviewerId], ['APPROVED', 'Great idea', 'MOD']);
  assert.ok(stored.reviewedAt instanceof Date);

  const embed = ch.edits.at(-1).payload.embeds[0].toJSON();
  assert.equal(embed.color, STATUS_INFO.APPROVED.color);
  assert.equal(embed.fields[2].value, 'Great idea\n— <@MOD>');
  assert.ok(ch.edits.at(-1).payload.components[0].toJSON().components.every((c) => c.disabled), 'voting is closed');

  assert.equal(w.dms.length, 1);
  assert.equal(w.dms[0].to, 'AUTHOR');
  assert.match(w.dms[0].payload.embeds[0].toJSON().title, /Your suggestion #1 was approved/);
  assert.deepEqual(w.dms[0].payload.allowedMentions, { parse: [] });
});

test('review: consider keeps voting open; a later review replaces the note; no note clears it', async () => {
  const { ch, w, s } = await postOne('GR2');
  const mod = w.addUser('MOD', 'Mod');
  await reviewSuggestion({ client: w.client, guild: w.guild, number: 1, status: 'CONSIDERED', note: 'Thinking', reviewer: mod, settings: SETTINGS });
  assert.ok(ch.edits.at(-1).payload.components[0].toJSON().components.every((c) => !c.disabled));
  assert.equal((await applyVote({ suggestionId: s.id, userId: 'A', value: 1 })).result, 'added');
  await reviewSuggestion({ client: w.client, guild: w.guild, number: 1, status: 'DENIED', note: '', reviewer: mod, settings: SETTINGS });
  assert.deepEqual([suggestions.get(s.id).status, suggestions.get(s.id).reviewNote], ['DENIED', null]);
  assert.equal(ch.edits.at(-1).payload.embeds[0].toJSON().fields[2].value, '*No note.*\n— <@MOD>');
});

test('review: the DM is skipped when off or when the author reviews their own suggestion; DM failures are ignored', async () => {
  const { w } = await postOne('GR3');
  const mod = w.addUser('MOD', 'Mod');
  const off = await reviewSuggestion({
    client: w.client, guild: w.guild, number: 1, status: 'CONSIDERED', reviewer: mod, settings: { ...SETTINGS, suggestionDmOnUpdate: false },
  });
  assert.equal(off.dm, 'off');
  const self = await reviewSuggestion({ client: w.client, guild: w.guild, number: 1, status: 'APPROVED', reviewer: { id: 'AUTHOR' }, settings: SETTINGS });
  assert.equal(self.dm, 'off');
  assert.equal(w.dms.length, 0);

  const closed = fakeWorld('GR3b', [fakeChannel('CH', { guildId: 'GR3b' })]);
  closed.addUser('AUTHOR', 'Author', { dmFails: true });
  const created = await createSuggestion({
    client: closed.client, guild: closed.guild, user: closed.client.users.cache.get('AUTHOR'), content: 'x', settings: { ...SETTINGS, suggestionThreads: false },
  });
  assert.equal(created.ok, true);
  const failed = await reviewSuggestion({ client: closed.client, guild: closed.guild, number: 1, status: 'DENIED', reviewer: { id: 'MOD' }, settings: SETTINGS });
  assert.equal(failed.ok, true, 'a closed DM never fails the review');
  assert.equal(failed.dm, 'failed');
  assert.equal(suggestions.get(created.suggestion.id).status, 'DENIED');
});

test('review: unknown numbers are refused; a deleted post still saves the status', async () => {
  const { ch, w, s } = await postOne('GR4');
  const mod = w.addUser('MOD', 'Mod');
  assert.deepEqual(await reviewSuggestion({ client: w.client, guild: w.guild, number: 99, status: 'APPROVED', reviewer: mod, settings: SETTINGS }), { ok: false, code: 'NOT_FOUND' });
  suggestions.get(s.id).messageId = null;
  const out = await reviewSuggestion({ client: w.client, guild: w.guild, number: 1, status: 'IMPLEMENTED', reviewer: mod, settings: SETTINGS });
  assert.equal(out.ok, true);
  assert.equal(out.edited, false);
  assert.equal(suggestions.get(s.id).status, 'IMPLEMENTED');
  assert.equal(ch.edits.length, 0);
});

// ── Delete ────────────────────────────────────────────────────────────────────

test('delete: the post, the thread, the row and its votes go; unknown numbers are refused', async () => {
  const ch = fakeChannel('CH', { guildId: 'GD1' });
  const { w, s } = await postOne('GD1', { world: { channel: ch, w: fakeWorld('GD1', [ch]) }, settings: { suggestionThreads: true } });
  const withThread = suggestions.get(s.id);
  assert.ok(withThread.threadId);
  const thread = ch.threads[0];
  w.client.channels.cache.set(withThread.threadId, thread);
  await applyVote({ suggestionId: s.id, userId: 'A', value: 1 });
  assert.equal(votes.size > 0, true);

  assert.deepEqual(await deleteSuggestion({ client: w.client, guild: w.guild, number: 42 }), { ok: false, code: 'NOT_FOUND' });
  const out = await deleteSuggestion({ client: w.client, guild: w.guild, number: 1 });
  assert.equal(out.ok, true);
  assert.deepEqual(ch.deleted, [withThread.messageId]);
  assert.equal(thread.deleted, true);
  assert.equal(suggestions.has(s.id), false);
  assert.equal([...votes.values()].some((v) => v.suggestionId === s.id), false, 'votes cascade');
});

test('delete: a post Discord cannot delete refuses and changes nothing; an already-gone post is fine', async () => {
  const { ch, w, s } = await postOne('GD2');
  ch.deleteError = Object.assign(new Error('Missing Access'), { code: 50001 });
  const refused = await deleteSuggestion({ client: w.client, guild: w.guild, number: 1 });
  assert.equal(refused.ok, false);
  assert.equal(refused.code, 'MESSAGE');
  assert.equal(suggestions.has(s.id), true, 'the row is kept');
  ch.deleteError = Object.assign(new Error('Unknown Message'), { code: 10008 });
  assert.equal((await deleteSuggestion({ client: w.client, guild: w.guild, number: 1 })).ok, true);
  assert.equal(suggestions.has(s.id), false);
});

// ── Hooks ─────────────────────────────────────────────────────────────────────

test('messageDelete: the row keeps its data and only the message id is cleared', async () => {
  const { s } = await postOne('GH1');
  const before = suggestions.get(s.id);
  const hook = plugin.hooks.find((h) => h.event === 'messageDelete');
  await onMessageDelete({ guildId: 'OTHER', message: { id: before.messageId } });
  assert.equal(suggestions.get(s.id).messageId, before.messageId, 'another guild changes nothing');
  await onMessageDelete({ guildId: 'GH1', message: { id: 'unrelated' } });
  assert.equal(suggestions.get(s.id).messageId, before.messageId);
  await hook.run({ guildId: 'GH1', message: { id: before.messageId } });
  const after = suggestions.get(s.id);
  assert.equal(after.messageId, null);
  assert.deepEqual([after.content, after.number, after.upvotes], [before.content, before.number, before.upvotes]);
});

test('messageDelete filter: only the bot own or uncached messages reach the database', () => {
  const hook = plugin.hooks.find((h) => h.event === 'messageDelete');
  const ctx = (message) => ({ client: { user: { id: 'BOT' } }, message });
  assert.equal(hook.filter(ctx({ author: { id: 'BOT' } })), true);
  assert.equal(hook.filter(ctx({ partial: true })), true);
  assert.equal(hook.filter(ctx({ author: { id: 'SOMEONE' } })), false);
});

test('channelDelete: clears the suggestion channel setting, only for that channel; posts lose their message id', async () => {
  const { s } = await postOne('GH2');
  await seedSettings('GH2', { suggestionsEnabled: true, suggestionChannelId: 'CH' });
  const hook = plugin.hooks.find((h) => h.event === 'channelDelete');
  await onChannelDelete({ guildId: 'GH2', channel: { id: 'SOMETHING-ELSE', type: ChannelType.GuildText } });
  assert.equal(settingsRows.get('GH2').suggestionChannelId, 'CH');
  assert.ok(suggestions.get(s.id).messageId);
  assert.equal(hook.filter({ channel: { id: 'V', type: ChannelType.GuildVoice } }), false, 'voice channels never reach the database');
  assert.equal(hook.filter({ channel: { id: 'C', type: ChannelType.GuildCategory } }), false);
  assert.equal(hook.filter({ channel: { id: 'T', type: ChannelType.GuildText } }), true);
  assert.equal(hook.filter({ channel: { id: 'A', type: ChannelType.GuildAnnouncement } }), true);
  await hook.run({ guildId: 'GH2', channel: { id: 'CH', type: ChannelType.GuildText } });
  assert.equal(settingsRows.get('GH2').suggestionChannelId, null);
  assert.equal(suggestions.get(s.id).messageId, null);
  assert.equal(suggestions.get(s.id).content, 'vote on me', 'the data stays');
});

// ── Plugin declaration ────────────────────────────────────────────────────────

test('plugin declaration: hooks, components, ids and hook order', async () => {
  assert.equal(plugin.id, 'suggestions');
  assert.equal(plugin.module, 'suggestions');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id, h.always]), [['messageDelete', 'suggestions', true], ['channelDelete', 'suggestions', true]]);
  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix]), [['button', 'suggestion'], ['modal', 'suggestion-modal']]);
  assert.deepEqual(plugin.jobs, []);
  for (const c of plugin.components) {
    assert.equal(CUSTOM_ID_PREFIXES[c.prefix].owner, 'suggestions');
    assert.equal(CUSTOM_ID_PREFIXES[c.prefix].kind, 'persistent');
    assert.equal(typeof c.handler, 'function');
  }
  assert.ok(HOOK_ORDER.messageDelete.includes('suggestions'));
  assert.ok(HOOK_ORDER.channelDelete.includes('suggestions'));
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'suggestions'));
  assert.deepEqual(errors.filter((e) => String(e?.message ?? e).includes('suggestions')), []);
});

// ── /suggestion ───────────────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/suggestion payload and permission levels', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'suggestion');
  assert.deepEqual(json.contexts, [0]);
  assert.ok(!json.default_member_permissions, 'members must see /suggestion submit');
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [
    ['submit', 1], ['approve', 1], ['deny', 1], ['consider', 1], ['implement', 1], ['delete', 1], ['setup', 1],
  ]);
  const sub = Object.fromEntries(json.options.map((o) => [o.name, o]));
  assert.deepEqual(sub.submit.options.map((o) => [o.name, o.type, !!o.required, o.max_length]), [['text', 3, false, LIMITS.SUGGESTION_LENGTH], ['anonymous', 5, false, undefined]]);
  for (const name of ['approve', 'deny', 'consider', 'implement']) {
    assert.deepEqual(sub[name].options.map((o) => [o.name, o.type, !!o.required]), [['number', 4, true], ['note', 3, false]], name);
    assert.equal(sub[name].options[1].max_length, SUGGESTION.NOTE_MAX);
  }
  assert.deepEqual(sub.delete.options.map((o) => [o.name, o.type, !!o.required]), [['number', 4, true]]);
  assert.deepEqual(sub.setup.options.map((o) => [o.name, o.type]), [
    ['channel', 7], ['new_channel', 3], ['threads', 5], ['anonymous', 5], ['dm_on_update', 5], ['cooldown', 4],
  ]);
  assert.deepEqual(sub.setup.options[0].channel_types, [0, 5]);
  assert.deepEqual([sub.setup.options[5].min_value, sub.setup.options[5].max_value], [0, 86_400]);
  assert.ok(charCount(json) <= 2000, `payload is ${charCount(json)} chars`);
  assert.ok(JSON.stringify(json).length < 4000);

  assert.equal(cmd.module, 'suggestions');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.deepEqual({ ...cmd.subcommandPermissions }, {
    approve: PERMISSION_LEVELS.MODERATOR, deny: PERMISSION_LEVELS.MODERATOR, consider: PERMISSION_LEVELS.MODERATOR,
    implement: PERMISSION_LEVELS.MODERATOR, delete: PERMISSION_LEVELS.MODERATOR, setup: PERMISSION_LEVELS.ADMIN,
  });
  assert.equal(cmd.allowInDMs, undefined);
});

async function submitWorld(guildId, settings, { sub = 'submit', vals = {}, userId = 'U1' } = {}) {
  const ch = fakeChannel('CH', { guildId });
  const w = fakeWorld(guildId, [ch]);
  const user = w.addUser(userId, 'Alice');
  await seedSettings(guildId, settings);
  const interaction = fakeInteraction({ guild: w.guild, user, sub, vals });
  return { ch, w, user, interaction };
}

test('/suggestion submit refuses while the module is off, with the router wording', async () => {
  const { ch, w, interaction } = await submitWorld('GS1', { ...SETTINGS, suggestionsEnabled: false }, { vals: { text: 'hello' } });
  await cmd.execute(interaction, w.client);
  assert.equal(interaction.ephemeral, true);
  assert.equal(lastEmbed(interaction).description,
    'The **Suggestions** module is turned off on this server. An admin can enable it with `/module enable suggestions`.');
  assert.equal(ch.sent.length, 0);

  const form = (await submitWorld('GS1b', { ...SETTINGS, suggestionsEnabled: false })).interaction;
  await cmd.execute(form, w.client);
  assert.equal(form.modals.length, 0, 'no form while the module is off');
  assert.match(lastEmbed(form).description, /module is turned off/);
});

test('/suggestion submit refuses without a channel and anonymous when not allowed', async () => {
  const none = await submitWorld('GS2', { ...SETTINGS, suggestionChannelId: null }, { vals: { text: 'hello' } });
  await cmd.execute(none.interaction, none.w.client);
  assert.match(lastEmbed(none.interaction).title, /No Suggestion Channel/);

  const anon = await submitWorld('GS3', SETTINGS, { vals: { text: 'hello', anonymous: true } });
  await cmd.execute(anon.interaction, anon.w.client);
  assert.match(lastEmbed(anon.interaction).title, /Anonymous Not Allowed/);
  assert.equal(anon.ch.sent.length, 0);

  const form = await submitWorld('GS3b', SETTINGS, { vals: { anonymous: true } });
  await cmd.execute(form.interaction, form.w.client);
  assert.equal(form.interaction.modals.length, 0);
  assert.match(lastEmbed(form.interaction).title, /Anonymous Not Allowed/);
});

test('/suggestion submit with text posts it and answers with a link; too long and empty are refused', async () => {
  const { ch, w, interaction } = await submitWorld('GS4', { ...SETTINGS, suggestionThreads: false }, { vals: { text: '  A new #pizza channel  ' } });
  await cmd.execute(interaction, w.client);
  assert.equal(interaction.deferred, true);
  assert.equal(interaction.ephemeral, true);
  assert.match(lastEmbed(interaction).title, /Suggestion Posted/);
  assert.match(lastEmbed(interaction).description, /discord\.com\/channels\/GS4\/CH\/\d+/);
  assert.equal(ch.sent.length, 1);
  assert.equal(ch.sent[0].payload.embeds[0].toJSON().description, 'A new #pizza channel');

  const long = fakeInteraction({ guild: w.guild, user: w.client.users.cache.get('U1'), sub: 'submit', vals: { text: 'x'.repeat(LIMITS.SUGGESTION_LENGTH + 1) } });
  await cmd.execute(long, w.client);
  assert.match(lastEmbed(long).title, /Too Long/);
  const blank = fakeInteraction({ guild: w.guild, user: w.client.users.cache.get('U1'), sub: 'submit', vals: { text: '   ' } });
  await cmd.execute(blank, w.client);
  assert.match(lastEmbed(blank).title, /Empty Suggestion/);
  assert.equal(ch.sent.length, 1);
});

test('/suggestion submit refuses during the cooldown, before the form opens as well', async () => {
  const { ch, w, user, interaction } = await submitWorld('GS5', { ...SETTINGS, suggestionThreads: false, suggestionCooldownSeconds: 600 }, { vals: { text: 'first' } });
  await cmd.execute(interaction, w.client);
  assert.match(lastEmbed(interaction).title, /Suggestion Posted/);

  const second = fakeInteraction({ guild: w.guild, user, sub: 'submit', vals: { text: 'second' } });
  await cmd.execute(second, w.client);
  assert.match(lastEmbed(second).title, /Slow Down/);
  assert.match(lastEmbed(second).description, /<t:\d+:R>/);

  const form = fakeInteraction({ guild: w.guild, user, sub: 'submit' });
  await cmd.execute(form, w.client);
  assert.equal(form.modals.length, 0, 'no form during the cooldown');
  assert.match(lastEmbed(form).title, /Slow Down/);
  assert.equal(ch.sent.length, 1);
});

test('/suggestion submit without text opens the form as its first response; the form posts it', async () => {
  const { ch, w, user } = await submitWorld('GS6', { ...SETTINGS, suggestionThreads: false, suggestionAllowAnonymous: true });
  const open = fakeInteraction({ guild: w.guild, user, sub: 'submit', vals: { anonymous: true } });
  await cmd.execute(open, w.client);
  assert.equal(open.deferred, false);
  assert.equal(open.modals.length, 1);
  assert.equal(open.modals[0].toJSON().custom_id, 'suggestion-modal:1');

  const submitted = fakeInteraction({ guild: w.guild, user, customId: 'suggestion-modal:1', fields: { 'suggestion-text': 'From the form' } });
  await handleSuggestionModal(submitted, w.client);
  assert.match(lastEmbed(submitted).title, /Suggestion Posted/);
  assert.match(lastEmbed(submitted).description, /posted anonymously/);
  const posted = ch.sent[0].payload.embeds[0].toJSON();
  assert.equal(posted.description, 'From the form');
  assert.equal(posted.author.name, 'Anonymous');
});

test('the form re-checks everything on submit (module off, anonymous no longer allowed)', async () => {
  const { ch, w, user } = await submitWorld('GS7', { ...SETTINGS, suggestionsEnabled: false });
  const off = fakeInteraction({ guild: w.guild, user, customId: 'suggestion-modal:0', fields: { 'suggestion-text': 'hi' } });
  await handleSuggestionModal(off, w.client);
  assert.match(lastEmbed(off).description, /module is turned off/);
  await seedSettings('GS7', { suggestionsEnabled: true, suggestionAllowAnonymous: false });
  const anon = fakeInteraction({ guild: w.guild, user, customId: 'suggestion-modal:1', fields: { 'suggestion-text': 'hi' } });
  await handleSuggestionModal(anon, w.client);
  assert.match(lastEmbed(anon).title, /Anonymous Not Allowed/);
  assert.equal(ch.sent.length, 0);
});

test('/suggestion approve | deny | consider | implement review by number and show the author to staff only', async () => {
  const ch = fakeChannel('CH', { guildId: 'GS8' });
  const w = fakeWorld('GS8', [ch]);
  const author = w.addUser('AUTHOR', 'Author');
  const mod = w.addUser('MOD', 'Mod');
  await seedSettings('GS8', { ...SETTINGS, suggestionAllowAnonymous: true, suggestionThreads: false });
  const created = await createSuggestion({
    client: w.client, guild: w.guild, user: author, content: 'secret idea', anonymous: true, settings: { ...SETTINGS, suggestionAllowAnonymous: true, suggestionThreads: false },
  });
  assert.equal(created.ok, true);

  const approve = fakeInteraction({ guild: w.guild, user: mod, sub: 'approve', vals: { number: 1, note: 'Yes!' } });
  await cmd.execute(approve, w.client);
  assert.equal(approve.ephemeral, true, 'the reply names the anonymous author, so it is private');
  const embed = lastEmbed(approve);
  assert.match(embed.title, /Suggestion #1: Approved/);
  assert.match(embed.description, /Author: <@AUTHOR> \(anonymous on the post\)/);
  assert.match(embed.description, /Note: Yes!/);
  assert.match(embed.description, /The post was updated/);
  assert.match(embed.description, /The author was sent a DM/);
  assert.equal(suggestions.get(created.suggestion.id).status, 'APPROVED');
  assert.ok(!JSON.stringify(ch.edits.at(-1).payload.embeds[0].toJSON()).includes('AUTHOR'), 'the public post stays anonymous');

  for (const [sub, status] of [['implement', 'IMPLEMENTED'], ['consider', 'CONSIDERED'], ['deny', 'DENIED']]) {
    const i = fakeInteraction({ guild: w.guild, user: mod, sub, vals: { number: 1 } });
    await cmd.execute(i, w.client);
    assert.equal(suggestions.get(created.suggestion.id).status, status, sub);
  }
  const missingOne = fakeInteraction({ guild: w.guild, user: mod, sub: 'approve', vals: { number: 77 } });
  await cmd.execute(missingOne, w.client);
  assert.match(lastEmbed(missingOne).title, /Suggestion Not Found/);
});

test('/suggestion works for staff while the module is off, and says so', async () => {
  const ch = fakeChannel('CH', { guildId: 'GS9' });
  const w = fakeWorld('GS9', [ch]);
  const author = w.addUser('AUTHOR', 'Author');
  const mod = w.addUser('MOD', 'Mod');
  await seedSettings('GS9', { ...SETTINGS, suggestionsEnabled: false, suggestionThreads: false });
  await createSuggestion({ client: w.client, guild: w.guild, user: author, content: 'x', settings: { ...SETTINGS, suggestionThreads: false } });
  const i = fakeInteraction({ guild: w.guild, user: mod, sub: 'deny', vals: { number: 1 } });
  await cmd.execute(i, w.client);
  assert.match(lastEmbed(i).title, /Denied/);
  assert.match(lastEmbed(i).description, /module is off/);
});

test('/suggestion delete removes the post and the row', async () => {
  const ch = fakeChannel('CH', { guildId: 'GS10' });
  const w = fakeWorld('GS10', [ch]);
  const author = w.addUser('AUTHOR', 'Author');
  const mod = w.addUser('MOD', 'Mod');
  const created = await createSuggestion({ client: w.client, guild: w.guild, user: author, content: 'x', settings: { ...SETTINGS, suggestionThreads: false } });
  const i = fakeInteraction({ guild: w.guild, user: mod, sub: 'delete', vals: { number: 1 } });
  await cmd.execute(i, w.client);
  assert.match(lastEmbed(i).title, /Suggestion Deleted/);
  assert.equal(suggestions.has(created.suggestion.id), false);
  assert.deepEqual(ch.deleted, [created.suggestion.messageId]);
  const again = fakeInteraction({ guild: w.guild, user: mod, sub: 'delete', vals: { number: 1 } });
  await cmd.execute(again, w.client);
  assert.match(lastEmbed(again).title, /Suggestion Not Found/);
});

test('/suggestion setup saves the channel and options, checks the channel, and shows the settings when given none', async () => {
  const ch = fakeChannel('NEWCH', { guildId: 'GS11' });
  const bad = fakeChannel('NOPOST', { guildId: 'GS11', deny: ['SendMessages'] });
  const w = fakeWorld('GS11', [ch, bad]);
  const admin = w.addUser('ADMIN', 'Admin');
  await seedSettings('GS11', { suggestionsEnabled: false, suggestionChannelId: null, suggestionThreads: true, suggestionAllowAnonymous: false, suggestionDmOnUpdate: true, suggestionCooldownSeconds: 300 });

  const show = fakeInteraction({ guild: w.guild, user: admin, sub: 'setup' });
  await cmd.execute(show, w.client);
  assert.equal(show.ephemeral, false, 'admin config replies are public');
  assert.match(lastEmbed(show).description, /Channel: Not set/);
  assert.match(lastEmbed(show).description, /Module: ❌ Off/);

  const refused = fakeInteraction({ guild: w.guild, user: admin, sub: 'setup', vals: { channel: { id: 'NOPOST' } } });
  await cmd.execute(refused, w.client);
  assert.match(lastEmbed(refused).title, /Cannot Use That Channel/);
  assert.equal(settingsRows.get('GS11').suggestionChannelId, null);

  const set = fakeInteraction({
    guild: w.guild, user: admin, sub: 'setup',
    vals: { channel: { id: 'NEWCH' }, threads: false, anonymous: true, dm_on_update: false, cooldown: 30 },
  });
  await cmd.execute(set, w.client);
  const saved = settingsRows.get('GS11');
  assert.deepEqual(
    [saved.suggestionChannelId, saved.suggestionThreads, saved.suggestionAllowAnonymous, saved.suggestionDmOnUpdate, saved.suggestionCooldownSeconds],
    ['NEWCH', false, true, false, 30],
  );
  assert.equal(saved.suggestionsEnabled, false, 'setup does not turn the module on');
  const text = lastEmbed(set).description;
  assert.match(lastEmbed(set).title, /Suggestions Updated/);
  assert.match(text, /Channel: <#NEWCH>/);
  assert.match(text, /Cooldown: 30 seconds/);
  assert.match(text, /Module: ❌ Off/);

  const partial = fakeInteraction({ guild: w.guild, user: admin, sub: 'setup', vals: { cooldown: 0 } });
  await cmd.execute(partial, w.client);
  assert.equal(settingsRows.get('GS11').suggestionCooldownSeconds, 0);
  assert.equal(settingsRows.get('GS11').suggestionChannelId, 'NEWCH', 'unchanged options are kept');
});

test('/suggestion setup warns when threads are on but the bot cannot create them', async () => {
  const ch = fakeChannel('CHT', { guildId: 'GS12', deny: ['CreatePublicThreads'] });
  const w = fakeWorld('GS12', [ch]);
  const admin = w.addUser('ADMIN', 'Admin');
  await seedSettings('GS12', { suggestionsEnabled: true, suggestionChannelId: null });
  const i = fakeInteraction({ guild: w.guild, user: admin, sub: 'setup', vals: { channel: { id: 'CHT' }, threads: true } });
  await cmd.execute(i, w.client);
  const fields = lastEmbed(i).fields ?? [];
  assert.ok(fields.some((f) => f.name === 'Threads' && /Create Public Threads/.test(f.value)));
});

test('the dashboard defaults match the schema defaults the service relies on', () => {
  assert.equal(DEFAULT_MODULE_SETTINGS.suggestions, false);
  assert.equal(MODULES.suggestions.flag, 'suggestionsEnabled');
  assert.equal(MODULES.suggestions.configCommand, '/suggestion');
  assert.equal(SUGGESTION.RENDER_INTERVAL_MS, 2000);
});
