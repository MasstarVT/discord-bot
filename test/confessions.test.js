// Confessions tests. Offline (no Discord, Postgres or Redis): the pure rules, the numbered create (cooldown, review queue,
// rollback), images (neutral file name, fresh copy on approve), the claim-based approve / reject / delete, the author blocklist,
// the logged reveal, setup and panel, the hooks, the plugin declaration and the /confession payload, all against an in-memory
// Confession table and fake channels. The last tests are the anonymity scans: every flow is run and everything a member or
// moderator could read (posts, review messages and their edits, staff replies, the moderation log, the process log) is searched
// for the author.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const { PermissionFlagsBits, ChannelType } = await import('discord.js');
const svc = await import('../src/services/confessionService.js');
const plug = await import('../src/plugins/confessions.js');
const cmd = await import('../src/commands/community/confession.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { CUSTOM_ID_PREFIXES } = await import('../src/config/customIds.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { MODULES } = await import('../src/config/modules.js');
const { updateSettings } = await import('../src/services/settingsService.js');
const { wasDeletedByBot } = await import('../src/utils/deletionTracker.js');

const {
  parseContent, containsLink, cooldownRemainingMs, jumpUrl, imageProblem, submitProblem, moduleOffEmbed, STATUS_INFO, RAW_SQL,
  buildConfessionEmbed, buildReviewEmbed, buildReviewComponents, buildConfessionModal, buildPanelPayload,
  createConfession, deleteConfession, setBlocked, revealConfession, onMessageDelete, onChannelDelete, handleButton,
  handleConfessionModal, __setImageFetch,
} = svc;
const { plugin } = plug;

// The console is captured, not silenced: the anonymity tests read what the process log would have printed.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
let logLines = [];
const capture = (...args) => { logLines.push(args.map(String).join(' ')); };
console.error = console.warn = console.log = console.debug = console.info = capture;

// ── In-memory tables ──────────────────────────────────────────────────────────

const confessions = new Map(); // id -> row
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

const confessionModel = {
  failCreate: null,
  findUnique: async ({ where }) => {
    if (where.id) return copy(confessions.get(where.id));
    const g = where.guildId_number;
    return copy([...confessions.values()].find((r) => r.guildId === g.guildId && r.number === g.number));
  },
  findFirst: async ({ where, orderBy }) => {
    const rows = [...confessions.values()].filter((r) => matches(r, where));
    if (orderBy?.createdAt === 'desc') rows.sort((a, b) => b.createdAt - a.createdAt);
    return copy(rows[0]);
  },
  findMany: async ({ where }) => [...confessions.values()].filter((r) => matches(r, where)).map(copy),
  create: async ({ data }) => {
    if (confessionModel.failCreate) throw confessionModel.failCreate;
    if ([...confessions.values()].some((r) => r.guildId === data.guildId && r.number === data.number)) {
      throw Object.assign(new Error('Unique constraint failed'), { code: 'P2002' });
    }
    const row = {
      id: `c${idSeq++}`, status: 'PENDING', hasImage: false, channelId: null, messageId: null, reviewMessageId: null,
      reviewerId: null, reviewedAt: null, createdAt: new Date(), ...data,
    };
    confessions.set(row.id, row);
    return copy(row);
  },
  updateMany: async ({ where, data }) => {
    const rows = [...confessions.values()].filter((r) => matches(r, where));
    for (const r of rows) Object.assign(r, data);
    return { count: rows.length };
  },
  deleteMany: async ({ where }) => {
    const rows = [...confessions.values()].filter((r) => matches(r, where));
    for (const r of rows) confessions.delete(r.id);
    return { count: rows.length };
  },
};
const queryRaw = async (q) => {
  const sql = q.sql;
  if (sql.includes('pg_advisory_xact_lock')) {
    txLog.push(`lock:${q.values[0]}`);
    return [{ ok: 1 }];
  }
  if (sql.includes('FOR UPDATE')) return [{ ok: 1 }]; // GuildSettings row lock
  txLog.push('next');
  const max = Math.max(0, ...[...confessions.values()].filter((r) => r.guildId === q.values[0]).map((r) => r.number));
  return [{ next: max + 1 }];
};
const guildSettingsModel = {
  findUnique: async ({ where }) => copy(settingsRows.get(where.guildId)),
  upsert: async ({ where, update }) => {
    const row = { ...(settingsRows.get(where.guildId) ?? { guildId: where.guildId }), ...update };
    settingsRows.set(where.guildId, row);
    return copy(row);
  },
  update: async ({ where, data }) => {
    const row = { ...(settingsRows.get(where.guildId) ?? { guildId: where.guildId }), ...data };
    settingsRows.set(where.guildId, row);
    return copy(row);
  },
};
// Transactions run one at a time, like the advisory lock / row lock would make them.
let txChain = Promise.resolve();
const txClient = { confession: confessionModel, guildSettings: guildSettingsModel, $queryRaw: queryRaw, $executeRaw: async () => 1 };
const transaction = (fn) => {
  const run = txChain.then(() => fn(txClient));
  txChain = run.then(() => {}, () => {});
  return run;
};

const real = {
  confession: prisma.confession, guildSettings: prisma.guildSettings, $transaction: prisma.$transaction, $queryRaw: prisma.$queryRaw,
};
prisma.confession = confessionModel;
prisma.guildSettings = guildSettingsModel;
prisma.$queryRaw = queryRaw;
prisma.$transaction = transaction;

after(async () => {
  Object.assign(prisma, real);
  __setImageFetch();
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

let nextId = 7000;
const EVERYONE = { id: 'EVERYONE-ROLE' };
const cdn = new Map(); // url -> Buffer (what the offline image download returns)
__setImageFetch(async (url) => {
  const b = cdn.get(url);
  if (!b) throw Object.assign(new Error('not found'), { code: 'HTTP_ERROR' });
  return b;
});

const pngBuf = () => {
  const b = Buffer.alloc(33);
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).copy(b);
  b.writeUInt32BE(13, 8);
  b.write('IHDR', 12, 'latin1');
  b.writeUInt32BE(10, 16);
  b.writeUInt32BE(10, 20);
  return b;
};
const gifBuf = () => {
  const b = Buffer.alloc(16);
  b.write('GIF89a', 0, 'latin1');
  b.writeUInt16LE(4, 6);
  b.writeUInt16LE(4, 8);
  return b;
};

const json = (p) => JSON.stringify({ ...p, files: p.files?.map((f) => f.name) });

function makeMessage(ch, payload) {
  const msg = {
    id: String(nextId++), channel: ch, deleted: false,
    embeds: (payload.embeds ?? []).map((e) => structuredClone(e.toJSON ? e.toJSON() : e)),
    components: (payload.components ?? []).map((r) => structuredClone(r.toJSON ? r.toJSON() : r)),
    files: payload.files ?? [],
    async edit(p) {
      if (ch.editError) throw ch.editError;
      ch.edits.push({ id: msg.id, payload: p, json: json(p) });
      if (p.embeds) msg.embeds = p.embeds.map((e) => structuredClone(e.toJSON ? e.toJSON() : e));
      if (p.components) msg.components = p.components.map((r) => structuredClone(r.toJSON ? r.toJSON() : r));
      return msg;
    },
    async delete() { msg.deleted = true; ch.deleted.push(msg.id); },
  };
  const list = msg.files.map((f) => {
    const url = `https://cdn.test/${msg.id}/${f.name}`;
    cdn.set(url, f.attachment);
    return { name: f.name, url };
  });
  msg.attachments = { first: () => list[0], size: list.length };
  return msg;
}

function fakeChannel(id, { guildId = 'G', deny = [], hidden = false, type = ChannelType.GuildText } = {}) {
  const denied = new Set(deny.map((k) => PermissionFlagsBits[k]));
  const ch = {
    id, guildId, type, sent: [], edits: [], deleted: [], store: new Map(), editError: null, deleteError: null, failSend: null,
    guild: { members: { me: {} }, roles: { everyone: EVERYONE } },
    permissionsFor: (who) => (who === EVERYONE ? { has: () => !hidden } : { has: (bit) => !denied.has(bit) }),
    isTextBased: () => true,
    isThread: () => false,
    async send(payload) {
      if (ch.failSend) throw ch.failSend;
      const message = makeMessage(ch, payload);
      ch.store.set(message.id, message);
      ch.sent.push({ payload, message, json: json(payload) });
      return message;
    },
  };
  ch.messages = {
    async delete(messageId) {
      if (ch.deleteError) throw ch.deleteError;
      ch.deleted.push(messageId);
      const m = ch.store.get(messageId);
      if (m) m.deleted = true;
    },
    async fetch(messageId) {
      const m = ch.store.get(messageId);
      if (!m) throw Object.assign(new Error('Unknown Message'), { code: 10008 });
      return m;
    },
  };
  return ch;
}

function fakeWorld(guildId, channels = []) {
  const chCache = new Map(channels.map((c) => [c.id, c]));
  const members = new Map();
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
    users: { cache: users, fetch: async (id) => { const u = users.get(id); if (!u) throw new Error('no user'); return u; } },
  };
  const addUser = (id, username, { displayName = username, inServer = true } = {}) => {
    const user = { id, username, displayName, displayAvatarURL: () => `https://cdn.test/${id}.png` };
    users.set(id, user);
    if (inServer) members.set(id, { id, displayName, user });
    return user;
  };
  return { guild, client, addUser };
}

const memberFor = (id, level) => ({
  id, guild: { ownerId: 'OWNER' }, roles: { cache: new Map() },
  permissions: {
    has: (bit) => (level === 'admin' && (bit === PermissionFlagsBits.Administrator || bit === PermissionFlagsBits.ManageGuild))
      || (level === 'mod' && bit === PermissionFlagsBits.KickMembers),
  },
});

/** Fake interaction: records replies. `vals` feeds the option getters; the customId (components) and the fields too. */
function fakeInteraction({
  guild, user, sub = null, vals = {}, customId = null, fields = {}, message = null, channel = null, level = 'member',
} = {}) {
  const i = {
    id: String(nextId++), createdTimestamp: Date.now(), guild, guildId: guild?.id, user, member: memberFor(user?.id, level), customId, message, channel,
    deferred: false, replied: false, ephemeral: null, replies: [], modals: [],
    inCachedGuild: () => true,
    inGuild: () => true,
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => null,
      getString: (n) => vals[n] ?? null,
      getBoolean: (n) => vals[n] ?? null,
      getInteger: (n) => vals[n] ?? null,
      getChannel: (n) => vals[n] ?? null,
      getAttachment: (n) => vals[n] ?? null,
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
/** Everything an interaction answered stayed private to the invoker. */
const isEphemeral = (i) => i.ephemeral === true || (i.replied && i.replies.every((r) => r.ephemeral === true));

const SETTINGS = Object.freeze({
  confessionsEnabled: true, confessionChannelId: 'POST', confessionReviewChannelId: null, confessionAllowImages: false,
  confessionBlockLinks: true, confessionCooldownSeconds: 0, confessionBlockedUserIds: [], modLogChannelId: null,
});
const AUTHOR = 'U-AUTHOR-77';
const NEEDLES = [AUTHOR, 'AliceSecret', 'Alice Secret', 'alice_selfie'];

/** A guild with post, review and log channels, the author and a moderator, and the settings seeded. */
async function setupWorld(guildId, settings = {}, { review = false } = {}) {
  const post = fakeChannel('POST', { guildId });
  const rev = fakeChannel('REV', { guildId });
  const log = fakeChannel('LOG', { guildId });
  const w = fakeWorld(guildId, [post, rev, log]);
  const author = w.addUser(AUTHOR, 'AliceSecret', { displayName: 'Alice Secret' });
  w.addUser('MOD1', 'ModMo');
  w.addUser('MOD2', 'ModTwo');
  w.addUser('ADMIN1', 'AdminAd');
  await updateSettings(guildId, { ...SETTINGS, ...(review ? { confessionReviewChannelId: 'REV' } : {}), ...settings });
  return { post, rev, log, w, author, guildId };
}

const submitCmd = async (world, { user = world.author, vals = {}, sub = 'submit' } = {}) => {
  const i = fakeInteraction({ guild: world.w.guild, user, sub, vals });
  await cmd.execute(i, world.w.client);
  return i;
};
const staffCmd = async (world, sub, vals = {}, { userId = 'MOD1', level = 'mod' } = {}) => {
  const i = fakeInteraction({ guild: world.w.guild, user: world.w.client.users.cache.get(userId), sub, vals, level });
  await cmd.execute(i, world.w.client);
  return i;
};
const rowOf = (guildId, number) => [...confessions.values()].find((r) => r.guildId === guildId && r.number === number);
const reviewClick = async (world, number, action, { userId = 'MOD1', level = 'mod' } = {}) => {
  const row = rowOf(world.guildId, number);
  const message = world.rev.store.get(row.reviewMessageId) ?? { id: 'GONE', components: [], embeds: [] };
  const i = fakeInteraction({
    guild: world.w.guild, user: world.w.client.users.cache.get(userId), customId: `confession:${row.id}:${action}`, message, channel: world.rev, level,
  });
  await handleButton(i, world.w.client);
  return i;
};
const imageOption = (name = 'IMG_alice_selfie.png', buf = pngBuf(), contentType = 'image/png') => {
  const url = `https://cdn.test/member/${name}`;
  cdn.set(url, buf);
  return { url, name, contentType, size: buf.length };
};

// ── Pure rules ────────────────────────────────────────────────────────────────

test('parseContent trims and enforces the length limit', () => {
  assert.deepEqual(parseContent('  I ate the last slice \n'), { ok: true, content: 'I ate the last slice' });
  assert.deepEqual(parseContent('   '), { ok: false, error: 'empty' });
  assert.deepEqual(parseContent(null), { ok: false, error: 'empty' });
  assert.equal(parseContent('x'.repeat(LIMITS.CONFESSION_LENGTH)).ok, true);
  assert.deepEqual(parseContent('x'.repeat(LIMITS.CONFESSION_LENGTH + 1)), { ok: false, error: 'too-long' });
});

test('containsLink: URLs and Discord invites, also disguised ones; ordinary text is fine', () => {
  for (const text of [
    'see https://example.com', 'HTTP://x.io/a', 'go to www.example.org now', 'discord.gg/abc123', 'join discord.com/invite/abc',
    'discordapp.com/invite/abc', 'dsc.gg/x', 'bit.ly/abc', 'example.com/page', 'ftp://host/file',
    'ht\u200btps://evil.example', 'https\u200b://x.y', 'ｈｔｔｐｓ：／／ｘ．ｙ', '[click](https://x.y)', 'visit discord\u200b.gg/abc',
  ]) assert.equal(containsLink(text), true, text);
  for (const text of [
    'I cried at the end of the movie', 'cats/dogs, both are great', 'it was 3.5/5', 'and/or maybe...', 'e.g. this one', 'etc./something',
    'I live in the U.S. and love it', 'Meeting at 5.30 pm', '',
  ]) assert.equal(containsLink(text), false, text);
  assert.equal(containsLink(null), false);
  assert.equal(containsLink('a.'.repeat(1000)), false, 'long dotted text stays fast and clean');
});

test('cooldownRemainingMs counts from the last confession; 0 or no history means none', () => {
  const now = 1_000_000;
  assert.equal(cooldownRemainingMs(new Date(now - 10_000), 60, now), 50_000);
  assert.equal(cooldownRemainingMs(new Date(now - 60_000), 60, now), 0);
  assert.equal(cooldownRemainingMs(new Date(now - 1), 0, now), 0);
  assert.equal(cooldownRemainingMs(null, 300, now), 0);
});

test('jumpUrl needs a message id', () => {
  assert.equal(jumpUrl({ guildId: 'G', channelId: 'C', messageId: 'M' }), 'https://discord.com/channels/G/C/M');
  assert.equal(jumpUrl({ guildId: 'G', channelId: 'C', messageId: null }), null);
});

test('submitProblem: module off first (the router wording), then no channel, then a neutral block', () => {
  const off = submitProblem({ ...SETTINGS, confessionsEnabled: false }, AUTHOR);
  assert.equal(off.toJSON().description,
    `The **${MODULES.confessions.label}** module is turned off on this server. An admin can enable it with \`/module enable confessions\`.`);
  assert.equal(moduleOffEmbed().toJSON().description, off.toJSON().description);
  assert.match(submitProblem({ ...SETTINGS, confessionChannelId: null }, AUTHOR).toJSON().title, /No Confession Channel/);
  const blocked = submitProblem({ ...SETTINGS, confessionBlockedUserIds: [AUTHOR] }, AUTHOR).toJSON();
  assert.equal(blocked.description, "You can't post confessions on this server.");
  assert.equal(submitProblem({ ...SETTINGS, confessionBlockedUserIds: ['other'] }, AUTHOR), null);
  assert.equal(submitProblem(SETTINGS, AUTHOR), null);
});

test('imageProblem: not allowed, not an image, too big; unknown type passes to the download check', () => {
  const att = { contentType: 'image/png', size: 100 };
  assert.match(imageProblem({ ...SETTINGS }, att).toJSON().title, /Images Not Allowed/);
  const allow = { ...SETTINGS, confessionAllowImages: true };
  assert.equal(imageProblem(allow, null), null);
  assert.equal(imageProblem(allow, att), null);
  assert.equal(imageProblem(allow, { contentType: null, size: 5 }), null);
  assert.match(imageProblem(allow, { contentType: 'text/plain', size: 5 }).toJSON().title, /Not An Image/);
  assert.match(imageProblem(allow, { contentType: 'image/svg+xml', size: 5 }).toJSON().title, /Not An Image/);
  assert.match(imageProblem(allow, { contentType: 'image/png', size: LIMITS.CONFESSION_IMAGE_BYTES + 1 }).toJSON().title, /Image Too Large/);
});

test('RAW_SQL entries build with unique names', () => {
  assert.deepEqual(RAW_SQL.map((q) => q.name), ['confessions.numberLock', 'confessions.nextNumber']);
  for (const q of RAW_SQL) assert.match(q.build().sql, /pg_advisory_xact_lock|"Confession"/);
});

// ── Messages ──────────────────────────────────────────────────────────────────

const row = (over = {}) => ({
  id: 'abc123', guildId: 'G', number: 7, authorId: AUTHOR, content: 'I ate the last slice', hasImage: false, status: 'PENDING',
  channelId: 'POST', messageId: null, reviewMessageId: null, createdAt: new Date(), ...over,
});

test('the post and the review message carry the number and the text, never the author', () => {
  const post = buildConfessionEmbed(row(), { imageName: 'confession.png' }).toJSON();
  assert.equal(post.title, '🤫 Confession #7');
  assert.equal(post.description, 'I ate the last slice');
  assert.equal(post.image.url, 'attachment://confession.png');
  assert.ok(post.timestamp);
  assert.ok(Math.abs(new Date(post.timestamp) - Date.now()) < 5000, 'stamped with the posting time, not the writing time');
  assert.equal(post.author, undefined);
  assert.equal(post.footer, undefined);
  assert.equal(post.fields, undefined);

  const written = new Date('2026-01-01T00:00:00Z');
  const review = buildReviewEmbed(row({ createdAt: written })).toJSON();
  assert.equal(review.timestamp, written.toISOString(), 'staff see when it was submitted');
  assert.notEqual(buildConfessionEmbed(row({ createdAt: written })).toJSON().timestamp, written.toISOString());
  assert.equal(review.fields.find((f) => f.name === 'Status').value, '⏳ Pending review');
  for (const e of [post, review, buildReviewComponents('abc123')[0].toJSON()]) {
    for (const n of NEEDLES) assert.equal(JSON.stringify(e).includes(n), false, n);
  }
  const [approve, rejectBtn] = buildReviewComponents('abc123')[0].toJSON().components;
  assert.deepEqual([approve.custom_id, rejectBtn.custom_id], ['confession:abc123:approve', 'confession:abc123:reject']);
});

test('the form, the panel and the status table', () => {
  const modal = buildConfessionModal().toJSON();
  assert.equal(modal.custom_id, 'confession-modal');
  assert.equal(modal.components[0].components[0].custom_id, 'confession-text');
  assert.equal(modal.components[0].components[0].max_length, LIMITS.CONFESSION_LENGTH);
  assert.equal(modal.components.length, 1, 'the form has no image field');

  const panel = buildPanelPayload();
  assert.equal(panel.components[0].toJSON().components[0].custom_id, 'confession:new');
  assert.deepEqual(panel.allowedMentions, { parse: [] });
  assert.match(panel.embeds[0].toJSON().description, /moderation log/);
  for (const k of ['PENDING', 'APPROVED', 'REJECTED', 'DELETED']) assert.ok(STATUS_INFO[k].label && STATUS_INFO[k].emoji, k);
});

// ── Create ────────────────────────────────────────────────────────────────────

test('createConfession posts at once without a review channel: numbered, locked, saved, no pings', async () => {
  const world = await setupWorld('GC1');
  txLog.length = 0;
  const a = await createConfession({ client: world.w.client, guild: world.w.guild, user: world.author, content: 'first', settings: SETTINGS });
  world.w.addUser('U2', 'Bob');
  const b = await createConfession({ client: world.w.client, guild: world.w.guild, user: world.w.client.users.cache.get('U2'), content: 'second', settings: SETTINGS });
  assert.deepEqual([a.ok, a.pending, a.confession.number, b.confession.number], [true, false, 1, 2]);
  assert.deepEqual(txLog, ['lock:confession-number:GC1', 'next', 'lock:confession-number:GC1', 'next'], 'cooldown and number under the lock');
  assert.equal(world.post.sent.length, 2);
  assert.equal(world.rev.sent.length, 0);
  const sent = world.post.sent[0];
  assert.equal(sent.payload.embeds[0].toJSON().description, 'first');
  assert.deepEqual(sent.payload.allowedMentions, { parse: [] });
  const saved = rowOf('GC1', 1);
  assert.deepEqual([saved.status, saved.messageId, saved.channelId, saved.authorId], ['APPROVED', sent.message.id, 'POST', AUTHOR]);
  assert.equal(jumpUrl(a.confession), `https://discord.com/channels/GC1/POST/${sent.message.id}`);
});

test('a review channel holds the confession as PENDING with buttons and posts nothing public', async () => {
  const world = await setupWorld('GC2', {}, { review: true });
  const out = await createConfession({
    client: world.w.client, guild: world.w.guild, user: world.author, content: 'needs a look', settings: { ...SETTINGS, confessionReviewChannelId: 'REV' },
  });
  assert.deepEqual([out.ok, out.pending], [true, true]);
  assert.equal(world.post.sent.length, 0);
  assert.equal(world.rev.sent.length, 1);
  const r = rowOf('GC2', 1);
  assert.deepEqual([r.status, r.messageId, r.reviewMessageId], ['PENDING', null, world.rev.sent[0].message.id]);
  const msg = world.rev.sent[0].payload;
  assert.equal(msg.components[0].toJSON().components.length, 2);
  assert.equal(msg.embeds[0].toJSON().description, 'needs a look');
});

test('createConfession: cooldown per member, refused and not counted; a failed send leaves no row and costs nothing', async () => {
  const world = await setupWorld('GC3', { confessionCooldownSeconds: 600 });
  const settings = { ...SETTINGS, confessionCooldownSeconds: 600 };
  const args = { client: world.w.client, guild: world.w.guild, user: world.author, settings };
  world.post.failSend = Object.assign(new Error('Missing Permissions'), { code: 50013 });
  const failed = await createConfession({ ...args, content: 'x' });
  assert.deepEqual([failed.ok, failed.code], [false, 'SEND']);
  assert.match(failed.data.reason, /missing permissions/i);
  assert.equal([...confessions.values()].filter((r) => r.guildId === 'GC3').length, 0);

  world.post.failSend = null;
  assert.equal((await createConfession({ ...args, content: 'one' })).ok, true, 'the failed send did not start the cooldown');
  const second = await createConfession({ ...args, content: 'two' });
  assert.deepEqual([second.ok, second.code], [false, 'COOLDOWN']);
  assert.ok(second.data.ms > 590_000);
  world.w.addUser('U9', 'Cara');
  assert.equal((await createConfession({ ...args, user: world.w.client.users.cache.get('U9'), content: 'other member' })).ok, true);
  assert.equal(world.post.sent.length, 2);
});

test('createConfession: an unusable channel is refused; a delete that races the save takes the post back', async () => {
  const world = await setupWorld('GC4');
  const noPerms = fakeChannel('POST', { guildId: 'GC4', deny: ['SendMessages'] });
  world.w.guild.channels.cache.set('POST', noPerms);
  const refused = await createConfession({ client: world.w.client, guild: world.w.guild, user: world.author, content: 'x', settings: SETTINGS });
  assert.deepEqual([refused.ok, refused.code], [false, 'CHANNEL']);
  assert.match(refused.data.reason, /Send Messages/);

  world.w.guild.channels.cache.set('POST', world.post);
  const realUpdateMany = confessionModel.updateMany;
  confessionModel.updateMany = async () => ({ count: 0 }); // the row was deleted between the send and the save
  const raced = await createConfession({ client: world.w.client, guild: world.w.guild, user: world.author, content: 'x', settings: SETTINGS });
  confessionModel.updateMany = realUpdateMany;
  assert.deepEqual([raced.ok, raced.code], [false, 'DB_ERROR']);
  assert.equal(world.post.sent[0].message.deleted, true);
});

// ── /confession submit ────────────────────────────────────────────────────────

test('submit posts it, answers ephemerally with a link, and the invocation never shows', async () => {
  const world = await setupWorld('GS1');
  const i = await submitCmd(world, { vals: { text: '  I never returned the book  ' } });
  assert.equal(i.deferred, true);
  assert.equal(i.ephemeral, true, 'deferred ephemerally: nothing public may hint who used the command');
  assert.match(lastEmbed(i).title, /Confession Posted/);
  assert.match(lastEmbed(i).description, /discord\.com\/channels\/GS1\/POST\/\d+/);
  assert.equal(world.post.sent.length, 1, 'the bot posted it itself, not as the interaction reply');
  assert.equal(world.post.sent[0].payload.embeds[0].toJSON().description, 'I never returned the book');
  assert.equal(isEphemeral(i), true);
});

test('submit with a review channel says it was sent for review and posts nothing yet', async () => {
  const world = await setupWorld('GS2', {}, { review: true });
  const i = await submitCmd(world, { vals: { text: 'please review me' } });
  assert.match(lastEmbed(i).title, /Sent For Review/);
  assert.match(lastEmbed(i).description, /<#POST>/);
  assert.equal(world.post.sent.length, 0);
  assert.equal(world.rev.sent.length, 1);
});

test('submit refusals are ephemeral and post nothing: module off, no channel, blocked, empty, too long, links, cooldown', async () => {
  const off = await setupWorld('GR1', { confessionsEnabled: false });
  const iOff = await submitCmd(off, { vals: { text: 'hello' } });
  assert.equal(lastEmbed(iOff).description,
    'The **Confessions** module is turned off on this server. An admin can enable it with `/module enable confessions`.');
  const formOff = await submitCmd(off);
  assert.equal(formOff.modals.length, 0, 'no form while the module is off');
  assert.match(lastEmbed(formOff).description, /module is turned off/);
  assert.equal(off.post.sent.length, 0);

  const none = await setupWorld('GR2', { confessionChannelId: null });
  assert.match(lastEmbed(await submitCmd(none, { vals: { text: 'hello' } })).title, /No Confession Channel/);

  const blocked = await setupWorld('GR3', { confessionBlockedUserIds: [AUTHOR] });
  const iBlocked = await submitCmd(blocked, { vals: { text: 'hello' } });
  assert.equal(lastEmbed(iBlocked).description, "You can't post confessions on this server.");
  const formBlocked = await submitCmd(blocked);
  assert.equal(formBlocked.modals.length, 0, 'a blocked member gets no form either');
  assert.equal(blocked.post.sent.length, 0);

  const ok = await setupWorld('GR4', { confessionCooldownSeconds: 600 });
  assert.match(lastEmbed(await submitCmd(ok, { vals: { text: '   ' } })).title, /Empty Confession/);
  assert.match(lastEmbed(await submitCmd(ok, { vals: { text: 'x'.repeat(LIMITS.CONFESSION_LENGTH + 1) } })).title, /Too Long/);
  for (const text of ['see https://x.example', 'join discord.gg/abc']) {
    assert.match(lastEmbed(await submitCmd(ok, { vals: { text } })).title, /Links Not Allowed/, text);
  }
  assert.equal(ok.post.sent.length, 0, 'refusals do not start the cooldown');
  assert.match(lastEmbed(await submitCmd(ok, { vals: { text: 'first' } })).title, /Posted/);
  const second = await submitCmd(ok, { vals: { text: 'second' } });
  assert.match(lastEmbed(second).title, /Slow Down/);
  assert.match(lastEmbed(second).description, /<t:\d+:R>/);
  const formCooldown = await submitCmd(ok);
  assert.equal(formCooldown.modals.length, 0, 'no form during the cooldown');
  assert.match(lastEmbed(formCooldown).title, /Slow Down/);
  assert.equal(ok.post.sent.length, 1);
  for (const i of [iOff, formOff, formBlocked, second, formCooldown]) assert.equal(isEphemeral(i), true);
});

test('links are allowed when the setting is off', async () => {
  const world = await setupWorld('GR5', { confessionBlockLinks: false });
  assert.match(lastEmbed(await submitCmd(world, { vals: { text: 'see https://x.example' } })).title, /Posted/);
});

test('submit without text opens the form as its first response; the form posts it and checks everything again', async () => {
  const world = await setupWorld('GF1');
  const open = await submitCmd(world);
  assert.equal(open.deferred, false);
  assert.equal(open.modals.length, 1);
  assert.equal(open.modals[0].toJSON().custom_id, 'confession-modal');

  const submitted = fakeInteraction({ guild: world.w.guild, user: world.author, customId: 'confession-modal', fields: { 'confession-text': 'From the form' } });
  await handleConfessionModal(submitted, world.w.client);
  assert.equal(submitted.ephemeral, true);
  assert.match(lastEmbed(submitted).title, /Confession Posted/);
  assert.equal(world.post.sent[0].payload.embeds[0].toJSON().description, 'From the form');

  const links = fakeInteraction({ guild: world.w.guild, user: world.author, customId: 'confession-modal', fields: { 'confession-text': 'visit www.example.com' } });
  await handleConfessionModal(links, world.w.client);
  assert.match(lastEmbed(links).title, /Links Not Allowed/);

  await updateSettings('GF1', { confessionBlockedUserIds: [AUTHOR] });
  const late = fakeInteraction({ guild: world.w.guild, user: world.author, customId: 'confession-modal', fields: { 'confession-text': 'too late' } });
  await handleConfessionModal(late, world.w.client);
  assert.equal(lastEmbed(late).description, "You can't post confessions on this server.");
  assert.equal(world.post.sent.length, 1);
});

test('the panel button opens the form, or refuses ephemerally', async () => {
  const world = await setupWorld('GF2');
  const click = fakeInteraction({ guild: world.w.guild, user: world.author, customId: 'confession:new' });
  await handleButton(click, world.w.client);
  assert.equal(click.modals.length, 1);
  assert.equal(click.modals[0].toJSON().custom_id, 'confession-modal');

  await updateSettings('GF2', { confessionBlockedUserIds: [AUTHOR] });
  const blocked = fakeInteraction({ guild: world.w.guild, user: world.author, customId: 'confession:new' });
  await handleButton(blocked, world.w.client);
  assert.equal(blocked.modals.length, 0);
  assert.equal(last(blocked).ephemeral, true);

  const stale = fakeInteraction({ guild: world.w.guild, user: world.author, customId: 'confession:bogus:zzz' });
  await handleButton(stale, world.w.client);
  assert.match(last(stale).content, /no longer active/);
});

// ── Images ────────────────────────────────────────────────────────────────────

test('an image is downloaded and re-uploaded under a neutral name; the original file name is never used', async () => {
  const world = await setupWorld('GI1', { confessionAllowImages: true });
  const i = await submitCmd(world, { vals: { text: 'look at this', image: imageOption('IMG_alice_selfie.png') } });
  assert.match(lastEmbed(i).title, /Confession Posted/);
  const sent = world.post.sent[0];
  assert.deepEqual(sent.payload.files.map((f) => f.name), ['confession.png']);
  assert.equal(sent.payload.embeds[0].toJSON().image.url, 'attachment://confession.png');
  assert.equal(rowOf('GI1', 1).hasImage, true);
  assert.equal(sent.json.includes('alice_selfie'), false);

  // The format follows the bytes, not the name the member gave it.
  const gif = await submitCmd(world, { vals: { text: 'moving', image: imageOption('IMG_alice_selfie.png', gifBuf(), 'image/gif') } });
  assert.match(lastEmbed(gif).title, /Posted/);
  assert.deepEqual(world.post.sent[1].payload.files.map((f) => f.name), ['confession.gif']);
});

test('image refusals: not allowed, not an image, too big, a download that fails, bytes that are not an image, no text', async () => {
  const off = await setupWorld('GI2');
  assert.match(lastEmbed(await submitCmd(off, { vals: { text: 'x', image: imageOption() } })).title, /Images Not Allowed/);

  const world = await setupWorld('GI3', { confessionAllowImages: true });
  const text = await submitCmd(world, { vals: { text: 'x', image: imageOption('a.txt', Buffer.from('hi'), 'text/plain') } });
  assert.match(lastEmbed(text).title, /Not An Image/);
  const big = await submitCmd(world, { vals: { text: 'x', image: { ...imageOption(), size: LIMITS.CONFESSION_IMAGE_BYTES + 1 } } });
  assert.match(lastEmbed(big).title, /Image Too Large/);
  const dead = await submitCmd(world, { vals: { text: 'x', image: { url: 'https://cdn.test/missing.png', name: 'm.png', contentType: 'image/png', size: 10 } } });
  assert.match(lastEmbed(dead).title, /Image Problem/);
  const junk = await submitCmd(world, { vals: { text: 'x', image: imageOption('j.png', Buffer.from('not an image at all, just text'), 'image/png') } });
  assert.match(lastEmbed(junk).title, /Image Problem/);
  const noText = await submitCmd(world, { vals: { image: imageOption() } });
  assert.match(lastEmbed(noText).title, /Add The Text/);
  assert.equal(noText.modals.length, 0);
  assert.equal(world.post.sent.length, 0);
  assert.equal([...confessions.values()].filter((r) => r.guildId === 'GI3').length, 0, 'no row for a refused image');
  for (const i of [text, big, dead, junk, noText]) assert.equal(isEphemeral(i), true);
});

test('an image needs the bot to attach files in the channel', async () => {
  const world = await setupWorld('GI4', { confessionAllowImages: true });
  world.w.guild.channels.cache.set('POST', fakeChannel('POST', { guildId: 'GI4', deny: ['AttachFiles'] }));
  const i = await submitCmd(world, { vals: { text: 'x', image: imageOption() } });
  assert.match(lastEmbed(i).title, /Cannot Post Confessions/);
  assert.match(lastEmbed(i).description, /Attach Files/);
});

test('the review message carries the image; approving copies it from a fresh fetch of that message', async () => {
  const world = await setupWorld('GI5', { confessionAllowImages: true }, { review: true });
  await submitCmd(world, { vals: { text: 'with picture', image: imageOption('IMG_alice_selfie.png') } });
  assert.deepEqual(world.rev.sent[0].payload.files.map((f) => f.name), ['confession.png']);
  assert.equal(world.rev.sent[0].message.embeds[0].image.url, 'attachment://confession.png');

  const click = await reviewClick(world, 1, 'approve');
  assert.match(lastEmbed(click).title, /Approved/);
  const posted = world.post.sent[0];
  assert.deepEqual(posted.payload.files.map((f) => f.name), ['confession.png']);
  assert.equal(posted.payload.files[0].attachment.equals(pngBuf()), true);
  assert.equal(posted.payload.embeds[0].toJSON().image.url, 'attachment://confession.png');
});

test('approving without the image on the review message posts without it and warns; a failing download keeps it pending', async () => {
  const world = await setupWorld('GI6', { confessionAllowImages: true }, { review: true });
  await submitCmd(world, { vals: { text: 'one', image: imageOption() } });
  await submitCmd(world, { vals: { text: 'two', image: imageOption() } });
  world.rev.store.get(rowOf('GI6', 1).reviewMessageId).attachments = { first: () => undefined, size: 0 };
  const lostImage = await reviewClick(world, 1, 'approve');
  assert.match(lastEmbed(lostImage).description, /without one/);
  assert.equal(world.post.sent[0].payload.files.length, 0);
  assert.equal(rowOf('GI6', 1).status, 'APPROVED');

  const msg = world.rev.store.get(rowOf('GI6', 2).reviewMessageId);
  msg.attachments = { first: () => ({ name: 'confession.png', url: 'https://cdn.test/expired.png' }), size: 1 };
  const failed = await reviewClick(world, 2, 'approve');
  assert.match(lastEmbed(failed).title, /Image Problem/);
  assert.equal(rowOf('GI6', 2).status, 'PENDING', 'still waiting');
  assert.equal(world.post.sent.length, 1);
});

// ── Review ────────────────────────────────────────────────────────────────────

test('approve posts the confession, marks the row and the review message (no buttons, reviewer shown)', async () => {
  const world = await setupWorld('GV1', {}, { review: true });
  await submitCmd(world, { vals: { text: 'a thing I did' } });
  const click = await reviewClick(world, 1, 'approve');
  assert.equal(click.ephemeral, true);
  assert.match(lastEmbed(click).title, /Approved/);
  assert.equal(world.post.sent.length, 1);
  const post = world.post.sent[0];
  assert.equal(post.payload.embeds[0].toJSON().title, '🤫 Confession #1');
  const r = rowOf('GV1', 1);
  assert.deepEqual([r.status, r.reviewerId, r.messageId, r.channelId], ['APPROVED', 'MOD1', post.message.id, 'POST']);
  assert.ok(r.reviewedAt);
  const reviewMessage = world.rev.store.get(r.reviewMessageId);
  assert.deepEqual(reviewMessage.components, []);
  const status = reviewMessage.embeds[0].fields.find((f) => f.name === 'Status').value;
  assert.match(status, /Approved by <@MOD1>/);
  assert.equal(reviewMessage.embeds[0].description, 'a thing I did');
});

test('reject marks the row and the review message and posts nothing', async () => {
  const world = await setupWorld('GV2', {}, { review: true });
  await submitCmd(world, { vals: { text: 'not ok' } });
  const click = await reviewClick(world, 1, 'reject');
  assert.match(lastEmbed(click).title, /Rejected/);
  assert.equal(world.post.sent.length, 0);
  const r = rowOf('GV2', 1);
  assert.deepEqual([r.status, r.reviewerId, r.messageId], ['REJECTED', 'MOD1', null]);
  const m = world.rev.store.get(r.reviewMessageId);
  assert.deepEqual(m.components, []);
  assert.match(m.embeds[0].fields.find((f) => f.name === 'Status').value, /Rejected by <@MOD1>/);
});

test('only moderators may review: a member is refused and nothing changes', async () => {
  const world = await setupWorld('GV3', {}, { review: true });
  await submitCmd(world, { vals: { text: 'x' } });
  world.w.addUser('NOSY', 'Nosy');
  const click = await reviewClick(world, 1, 'approve', { userId: 'NOSY', level: 'member' });
  assert.match(lastEmbed(click).title, /Insufficient Permissions/);
  assert.equal(last(click).ephemeral, true);
  assert.equal(rowOf('GV3', 1).status, 'PENDING');
  assert.equal(world.post.sent.length, 0);
  assert.equal(world.rev.edits.length, 0);
});

test('a second moderator, also a double click at once, cannot post it twice', async () => {
  const world = await setupWorld('GV4', {}, { review: true });
  await submitCmd(world, { vals: { text: 'once only' } });
  const [a, b] = await Promise.all([reviewClick(world, 1, 'approve'), reviewClick(world, 1, 'approve', { userId: 'MOD2' })]);
  assert.equal(world.post.sent.length, 1);
  const titles = [lastEmbed(a).title, lastEmbed(b).title].sort();
  assert.match(titles[0], /Already Reviewed|Approved/);
  assert.ok(titles.some((t) => /Approved/.test(t)) && titles.some((t) => /Already Reviewed/.test(t)), titles.join(' | '));

  const later = await reviewClick(world, 1, 'reject', { userId: 'MOD2' });
  assert.match(lastEmbed(later).title, /Already Reviewed/);
  assert.equal(rowOf('GV4', 1).status, 'APPROVED');
});

test('approve without a confession channel stays pending; a failing post hands it back to the queue', async () => {
  const world = await setupWorld('GV5', {}, { review: true });
  await submitCmd(world, { vals: { text: 'x' } });
  await updateSettings('GV5', { confessionChannelId: null });
  const none = await reviewClick(world, 1, 'approve');
  assert.match(lastEmbed(none).title, /No Confession Channel/);
  assert.equal(rowOf('GV5', 1).status, 'PENDING');

  await updateSettings('GV5', { confessionChannelId: 'POST' });
  world.post.failSend = Object.assign(new Error('Missing Permissions'), { code: 50013 });
  const failed = await reviewClick(world, 1, 'approve');
  assert.match(lastEmbed(failed).title, /Could Not Post/);
  const r = rowOf('GV5', 1);
  assert.deepEqual([r.status, r.reviewerId, r.reviewedAt], ['PENDING', null, null]);
  assert.equal(world.rev.store.get(r.reviewMessageId).components.length, 1, 'the buttons stay');

  world.post.failSend = null;
  assert.match(lastEmbed(await reviewClick(world, 1, 'approve')).title, /Approved/);
  assert.equal(world.post.sent.length, 1);
});

test('review buttons of a confession that was deleted meanwhile lose their buttons', async () => {
  const world = await setupWorld('GV6', {}, { review: true });
  await submitCmd(world, { vals: { text: 'x' } });
  const msg = world.rev.store.get(rowOf('GV6', 1).reviewMessageId);
  rowOf('GV6', 1).status = 'DELETED'; // deleted by a moderator; the message edit was lost
  const click = await reviewClick(world, 1, 'approve');
  assert.match(lastEmbed(click).title, /Already Reviewed/);
  assert.deepEqual(msg.components, []);
  assert.equal(world.post.sent.length, 0);
});

// ── Delete ────────────────────────────────────────────────────────────────────

test('delete removes the post, marks the row DELETED (data stays) and the bot is named as the deleter', async () => {
  const world = await setupWorld('GD1');
  await submitCmd(world, { vals: { text: 'to be removed' } });
  const messageId = rowOf('GD1', 1).messageId;
  const i = await staffCmd(world, 'delete', { number: 1 });
  assert.equal(i.ephemeral, true);
  assert.match(lastEmbed(i).title, /Confession Deleted/);
  assert.deepEqual(world.post.deleted, [messageId]);
  assert.equal(wasDeletedByBot(messageId), 'confessions:delete');
  const r = rowOf('GD1', 1);
  assert.deepEqual([r.status, r.messageId, r.content, r.authorId], ['DELETED', null, 'to be removed', AUTHOR]);

  assert.match(lastEmbed(await staffCmd(world, 'delete', { number: 1 })).title, /Already Deleted/);
  assert.match(lastEmbed(await staffCmd(world, 'delete', { number: 99 })).title, /Confession Not Found/);
});

test('delete of a pending confession closes its review message, and it can no longer be approved', async () => {
  const world = await setupWorld('GD2', {}, { review: true });
  await submitCmd(world, { vals: { text: 'pending one' } });
  const msg = world.rev.store.get(rowOf('GD2', 1).reviewMessageId);
  await staffCmd(world, 'delete', { number: 1 });
  assert.deepEqual(msg.components, []);
  assert.match(msg.embeds[0].fields.find((f) => f.name === 'Status').value, /Deleted/);
  assert.equal(rowOf('GD2', 1).status, 'DELETED');
  const click = await reviewClick(world, 1, 'approve');
  assert.match(lastEmbed(click).title, /Already Reviewed/);
  assert.equal(world.post.sent.length, 0);
});

test('delete: a post that cannot be deleted changes nothing; one that is already gone is fine', async () => {
  const world = await setupWorld('GD3');
  await submitCmd(world, { vals: { text: 'sticky' } });
  world.post.deleteError = Object.assign(new Error('Missing Permissions'), { code: 50013 });
  const refused = await staffCmd(world, 'delete', { number: 1 });
  assert.match(lastEmbed(refused).title, /Could Not Delete/);
  assert.match(lastEmbed(refused).description, /Nothing was changed/);
  assert.equal(rowOf('GD3', 1).status, 'APPROVED');
  assert.ok(rowOf('GD3', 1).messageId);

  world.post.deleteError = Object.assign(new Error('Unknown Message'), { code: 10008 });
  assert.match(lastEmbed(await staffCmd(world, 'delete', { number: 1 })).title, /Confession Deleted/);
  assert.equal(rowOf('GD3', 1).status, 'DELETED');
});

test('delete while an approval is posting: the late post is taken back', async () => {
  const world = await setupWorld('GD4', {}, { review: true });
  await submitCmd(world, { vals: { text: 'racing' } });
  const original = world.post.send.bind(world.post);
  world.post.send = async (payload) => {
    const m = await original(payload);
    await deleteConfession({ client: world.w.client, guild: world.w.guild, number: 1, settings: SETTINGS }); // lands mid-post
    return m;
  };
  const click = await reviewClick(world, 1, 'approve');
  assert.match(lastEmbed(click).title, /Confession Removed/);
  assert.equal(world.post.sent[0].message.deleted, true);
  assert.equal(rowOf('GD4', 1).status, 'DELETED');
  assert.equal(rowOf('GD4', 1).messageId, null);
});

// ── Block and unblock ─────────────────────────────────────────────────────────

const masked = (i) => JSON.stringify({ ...lastEmbed(i), timestamp: undefined }).replace(/#\d+/g, '#N');

test('block and unblock never show who, and answer the same whatever the state was', async () => {
  const world = await setupWorld('GB1');
  world.w.addUser('U2', 'Bob');
  await submitCmd(world, { vals: { text: 'one' } });                                   // #1 by AUTHOR
  await submitCmd(world, { vals: { text: 'two' } });                                   // #2 by AUTHOR
  await submitCmd(world, { user: world.w.client.users.cache.get('U2'), vals: { text: 'three' } }); // #3 by U2

  const b1 = await staffCmd(world, 'block', { number: 1 });
  const b2 = await staffCmd(world, 'block', { number: 2 });   // same author, already blocked
  const b3 = await staffCmd(world, 'block', { number: 3 });   // another author
  assert.equal(b1.ephemeral, true);
  assert.equal(masked(b1), masked(b2));
  assert.equal(masked(b1), masked(b3));
  assert.match(lastEmbed(b1).title, /Author Blocked/);
  assert.deepEqual(settingsRows.get('GB1').confessionBlockedUserIds, [AUTHOR, 'U2'], 'no duplicate for the second confession');

  const u1 = await staffCmd(world, 'unblock', { number: 1 });
  const u2 = await staffCmd(world, 'unblock', { number: 2 }); // already unblocked
  assert.equal(masked(u1), masked(u2));
  assert.match(lastEmbed(u1).title, /Author Unblocked/);
  assert.deepEqual(settingsRows.get('GB1').confessionBlockedUserIds, ['U2']);

  assert.match(lastEmbed(await staffCmd(world, 'block', { number: 99 })).title, /Confession Not Found/);
  for (const i of [b1, b2, b3, u1, u2]) for (const n of [...NEEDLES, 'U2', 'Bob']) assert.equal(JSON.stringify(i.replies).includes(n), false, n);
});

test('a blocked member cannot confess until unblocked; setup shows neither names nor a count (a count would tell two authors apart)', async () => {
  const world = await setupWorld('GB2');
  await submitCmd(world, { vals: { text: 'first' } });
  await staffCmd(world, 'block', { number: 1 });
  assert.equal(lastEmbed(await submitCmd(world, { vals: { text: 'again' } })).description, "You can't post confessions on this server.");
  const setup = await staffCmd(world, 'setup', {}, { userId: 'ADMIN1', level: 'admin' });
  assert.match(lastEmbed(setup).description, /Blocked members: not listed/);
  assert.doesNotMatch(lastEmbed(setup).description, /Blocked members: \d/);
  assert.equal(JSON.stringify(setup.replies).includes(AUTHOR), false);
  await staffCmd(world, 'unblock', { number: 1 });
  assert.match(lastEmbed(await submitCmd(world, { vals: { text: 'back' } })).title, /Posted/);
  assert.equal(world.post.sent.length, 2);
});

test('the blocklist is capped: a block is refused at the cap whoever the author is, an unblock still works', async () => {
  const world = await setupWorld('GB3');
  await submitCmd(world, { vals: { text: 'one' } });
  const full = Array.from({ length: LIMITS.CONFESSION_BLOCKED_USERS }, (_, n) => `X${n}`);
  await updateSettings('GB3', { confessionBlockedUserIds: [...full, AUTHOR].slice(0, LIMITS.CONFESSION_BLOCKED_USERS) });
  const refused = await staffCmd(world, 'block', { number: 1 });
  assert.match(lastEmbed(refused).title, /Blocklist Full/);
  assert.equal(settingsRows.get('GB3').confessionBlockedUserIds.length, LIMITS.CONFESSION_BLOCKED_USERS);
  const out = await setBlocked({ guildId: 'GB3', number: 1, blocked: false });
  assert.equal(out.ok, true);
});

// ── Reveal ────────────────────────────────────────────────────────────────────

test('reveal writes the moderation log first, shows the author only to the admin, and the entry names no author', async () => {
  const world = await setupWorld('GX1', { modLogChannelId: 'LOG' });
  await submitCmd(world, { vals: { text: 'who wrote this' } });
  const i = await staffCmd(world, 'reveal', { number: 1 }, { userId: 'ADMIN1', level: 'admin' });
  assert.equal(i.ephemeral, true);
  const reply = JSON.stringify(i.replies);
  assert.match(reply, new RegExp(AUTHOR));
  assert.match(reply, /Alice Secret/);
  assert.match(lastEmbed(i).title, /Author Revealed/);

  assert.equal(world.log.sent.length, 1);
  const entry = world.log.sent[0].json;
  assert.match(entry, /<@ADMIN1>/);
  assert.match(entry, /Confession #1/);
  assert.deepEqual(world.log.sent[0].payload.allowedMentions, { parse: [] });
  for (const n of NEEDLES) assert.equal(entry.includes(n), false, `the log entry must not contain ${n}`);

  const again = await staffCmd(world, 'reveal', { number: 1 }, { userId: 'ADMIN1', level: 'admin' });
  assert.match(lastEmbed(again).title, /Author Revealed/);
  assert.equal(world.log.sent.length, 2, 'every reveal is logged');
});

test('reveal is refused, and reveals nothing, without a usable moderation log', async () => {
  const world = await setupWorld('GX2');
  await submitCmd(world, { vals: { text: 'secret' } });
  const noLog = await staffCmd(world, 'reveal', { number: 1 }, { userId: 'ADMIN1', level: 'admin' });
  assert.match(lastEmbed(noLog).title, /Not Revealed/);
  assert.match(lastEmbed(noLog).description, /No moderation log channel is set/);
  assert.match(lastEmbed(noLog).description, /modconfig logchannel/);

  await updateSettings('GX2', { modLogChannelId: 'LOG' });
  world.w.guild.channels.cache.set('LOG', fakeChannel('LOG', { guildId: 'GX2', deny: ['SendMessages'] }));
  const unusable = await staffCmd(world, 'reveal', { number: 1 }, { userId: 'ADMIN1', level: 'admin' });
  assert.match(lastEmbed(unusable).title, /Not Revealed/);

  const failing = fakeChannel('LOG', { guildId: 'GX2' });
  failing.failSend = Object.assign(new Error('Missing Access'), { code: 50001 });
  world.w.guild.channels.cache.set('LOG', failing);
  const failed = await staffCmd(world, 'reveal', { number: 1 }, { userId: 'ADMIN1', level: 'admin' });
  assert.match(lastEmbed(failed).title, /Not Revealed/);
  assert.match(lastEmbed(failed).description, /log entry/);

  for (const r of [noLog, unusable, failed]) for (const n of NEEDLES) assert.equal(JSON.stringify(r.replies).includes(n), false, n);
  assert.match(lastEmbed(await staffCmd(world, 'reveal', { number: 99 }, { userId: 'ADMIN1', level: 'admin' })).title, /Confession Not Found/);
  const direct = await revealConfession({ guild: world.w.guild, number: 1, admin: { id: 'ADMIN1' } });
  assert.deepEqual([direct.ok, direct.code], [false, 'LOG_FAILED']);
});

test('reveal says when the author has left the server', async () => {
  const world = await setupWorld('GX3', { modLogChannelId: 'LOG' });
  await submitCmd(world, { vals: { text: 'bye' } });
  world.w.guild.members.cache.delete(AUTHOR);
  const i = await staffCmd(world, 'reveal', { number: 1 }, { userId: 'ADMIN1', level: 'admin' });
  assert.match(lastEmbed(i).description, /no longer in this server/);
});

// ── Setup and panel ───────────────────────────────────────────────────────────

test('setup sets the channels, shows the settings with no options, and refuses bad picks', async () => {
  const world = await setupWorld('GU1', { confessionChannelId: null });
  const admin = (vals) => staffCmd(world, 'setup', vals, { userId: 'ADMIN1', level: 'admin' });

  const none = await admin({});
  assert.equal(none.ephemeral, true);
  assert.match(lastEmbed(none).description, /Channel: Not set/);

  const set = await admin({ channel: world.post });
  assert.match(lastEmbed(set).title, /Confessions Updated/);
  assert.equal(settingsRows.get('GU1').confessionChannelId, 'POST');

  const review = await admin({ review_channel: world.rev });
  assert.equal(settingsRows.get('GU1').confessionReviewChannelId, 'REV');
  assert.match(JSON.stringify(review.replies), /can be seen by @everyone/, 'a review channel everyone can see is flagged');

  const same = await admin({ review_channel: world.post });
  assert.match(lastEmbed(same).title, /Same Channel/);
  assert.equal(settingsRows.get('GU1').confessionReviewChannelId, 'REV');

  const both = await admin({ review_channel: world.rev, no_review: true });
  assert.match(lastEmbed(both).title, /Pick One/);

  const off = await admin({ no_review: true });
  assert.equal(settingsRows.get('GU1').confessionReviewChannelId, null);
  assert.match(lastEmbed(off).description, /posted at once/);

  const deniedChannel = fakeChannel('DENY', { guildId: 'GU1', deny: ['EmbedLinks'] });
  world.w.guild.channels.cache.set('DENY', deniedChannel);
  const unusable = await admin({ channel: deniedChannel });
  assert.match(lastEmbed(unusable).title, /Cannot Use That Channel/);
  assert.equal(settingsRows.get('GU1').confessionChannelId, 'POST');

  const priv = fakeChannel('PRIV', { guildId: 'GU1', hidden: true });
  world.w.guild.channels.cache.set('PRIV', priv);
  const quietReview = await admin({ review_channel: priv });
  assert.doesNotMatch(JSON.stringify(quietReview.replies), /can be seen by @everyone/);
});

test('panel posts the Confess button; an unusable channel is refused', async () => {
  const world = await setupWorld('GU2');
  const ok = await staffCmd(world, 'panel', {}, { userId: 'ADMIN1', level: 'admin' }); // here: no channel on the fake, so pick one
  assert.match(lastEmbed(ok).title, /Cannot Use That Channel/);

  const target = fakeChannel('PANEL', { guildId: 'GU2' });
  world.w.guild.channels.cache.set('PANEL', target);
  const done = await staffCmd(world, 'panel', { channel: target }, { userId: 'ADMIN1', level: 'admin' });
  assert.match(lastEmbed(done).title, /Panel Posted/);
  assert.equal(target.sent.length, 1);
  assert.equal(target.sent[0].payload.components[0].toJSON().components[0].custom_id, 'confession:new');

  const denied = fakeChannel('NOPE', { guildId: 'GU2', deny: ['SendMessages'] });
  world.w.guild.channels.cache.set('NOPE', denied);
  assert.match(lastEmbed(await staffCmd(world, 'panel', { channel: denied }, { userId: 'ADMIN1', level: 'admin' })).title, /Cannot Use That Channel/);
});

// ── Hooks ─────────────────────────────────────────────────────────────────────

const hook = (event) => plugin.hooks.find((h) => h.event === event);

test('messageDelete clears the message ids of a deleted post or review message; the row keeps its data', async () => {
  const world = await setupWorld('GH1', {}, { review: true });
  await submitCmd(world, { vals: { text: 'keep me' } });
  await reviewClick(world, 1, 'approve');
  await submitCmd(world, { vals: { text: 'pending' } });
  const r1 = rowOf('GH1', 1);
  const r2 = rowOf('GH1', 2);
  const [post, review2] = [r1.messageId, r2.reviewMessageId];

  const h = hook('messageDelete');
  assert.equal(h.always, true);
  assert.equal(h.filter({ client: world.w.client, message: { partial: true } }), true, 'an uncached message may be ours');
  assert.equal(h.filter({ client: world.w.client, message: { author: { id: 'BOT' } } }), true);
  assert.equal(h.filter({ client: world.w.client, message: { author: { id: AUTHOR } } }), false, 'members\' messages never reach the database');

  await h.run({ guildId: 'GH1', message: { id: 'unrelated' } });
  assert.equal(rowOf('GH1', 1).messageId, post);
  await h.run({ guildId: 'GH1', message: { id: post } });
  await h.run({ guildId: 'GH1', message: { id: review2 } });
  assert.equal(rowOf('GH1', 1).messageId, null);
  assert.equal(rowOf('GH1', 2).reviewMessageId, null);
  assert.deepEqual([rowOf('GH1', 1).content, rowOf('GH1', 1).status, rowOf('GH1', 2).status], ['keep me', 'APPROVED', 'PENDING']);
  await onMessageDelete({ guildId: null, message: { id: post } }); // no guild: ignored
});

test('channelDelete unsets the confession channel and clears the posts in it', async () => {
  const world = await setupWorld('GH2');
  await submitCmd(world, { vals: { text: 'post me' } });
  const h = hook('channelDelete');
  assert.equal(h.always, true);
  assert.equal(h.filter({ channel: { type: ChannelType.GuildVoice } }), false);
  assert.equal(h.filter({ channel: { type: ChannelType.GuildCategory } }), false);
  assert.equal(h.filter({ channel: { type: ChannelType.GuildText } }), true);
  assert.equal(h.filter({ channel: { type: ChannelType.GuildAnnouncement } }), true);

  await h.run({ guildId: 'GH2', channel: { id: 'ELSEWHERE', type: ChannelType.GuildText } });
  assert.equal(settingsRows.get('GH2').confessionChannelId, 'POST');
  assert.ok(rowOf('GH2', 1).messageId);
  await h.run({ guildId: 'GH2', channel: { id: 'POST', type: ChannelType.GuildText } });
  assert.equal(settingsRows.get('GH2').confessionChannelId, null);
  assert.equal(rowOf('GH2', 1).messageId, null);
  assert.equal(rowOf('GH2', 1).content, 'post me', 'the data stays');
});

test('deleting the review channel unsets BOTH channels (confessions must not turn public unreviewed) and orphans the queue', async () => {
  const world = await setupWorld('GH3', {}, { review: true });
  await submitCmd(world, { vals: { text: 'waiting' } });
  assert.ok(rowOf('GH3', 1).reviewMessageId);
  await onChannelDelete({ guildId: 'GH3', channel: { id: 'REV', type: ChannelType.GuildText } });
  assert.equal(settingsRows.get('GH3').confessionReviewChannelId, null);
  assert.equal(settingsRows.get('GH3').confessionChannelId, null);
  assert.equal(rowOf('GH3', 1).reviewMessageId, null);
  assert.equal(rowOf('GH3', 1).status, 'PENDING');
  assert.match(lastEmbed(await submitCmd(world, { vals: { text: 'now?' } })).title, /No Confession Channel/);
  assert.equal(world.post.sent.length, 0);
});

// ── Plugin declaration and payload ────────────────────────────────────────────

test('plugin declaration: hooks, components, ids and hook order', async () => {
  assert.equal(plugin.id, 'confessions');
  assert.equal(plugin.module, 'confessions');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id, h.always]), [['messageDelete', 'confessions', true], ['channelDelete', 'confessions', true]]);
  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix]), [['button', 'confession'], ['modal', 'confession-modal']]);
  assert.deepEqual(plugin.jobs, []);
  for (const c of plugin.components) {
    assert.equal(CUSTOM_ID_PREFIXES[c.prefix].owner, 'confessions');
    assert.equal(CUSTOM_ID_PREFIXES[c.prefix].kind, 'persistent');
    assert.equal(typeof c.handler, 'function');
  }
  assert.equal(CUSTOM_ID_PREFIXES['confession-text'].kind, 'field');
  assert.ok(HOOK_ORDER.messageDelete.includes('confessions'));
  assert.ok(HOOK_ORDER.channelDelete.includes('confessions'));
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'confessions'));
  assert.deepEqual(errors.filter((e) => String(e?.message ?? e).includes('confessions')), []);
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/confession payload and permission levels', () => {
  const payload = buildCommandPayload(cmd);
  assert.equal(payload.name, 'confession');
  assert.deepEqual(payload.contexts, [0]);
  assert.ok(!payload.default_member_permissions, 'members must see /confession submit');
  assert.deepEqual(payload.options.map((o) => [o.name, o.type]), [
    ['submit', 1], ['panel', 1], ['delete', 1], ['block', 1], ['unblock', 1], ['reveal', 1], ['setup', 1],
  ]);
  const sub = Object.fromEntries(payload.options.map((o) => [o.name, o]));
  assert.deepEqual(sub.submit.options.map((o) => [o.name, o.type, !!o.required, o.max_length]), [['text', 3, false, LIMITS.CONFESSION_LENGTH], ['image', 11, false, undefined]]);
  assert.deepEqual(sub.panel.options.map((o) => [o.name, o.type]), [['channel', 7]]);
  for (const name of ['delete', 'block', 'unblock', 'reveal']) {
    assert.deepEqual(sub[name].options.map((o) => [o.name, o.type, !!o.required]), [['number', 4, true]], name);
  }
  assert.deepEqual(sub.setup.options.map((o) => [o.name, o.type]), [['channel', 7], ['review_channel', 7], ['no_review', 5]]);
  assert.deepEqual(sub.setup.options[0].channel_types, [0, 5]);
  assert.ok(charCount(payload) <= 2000, `payload is ${charCount(payload)} chars`);
  assert.ok(JSON.stringify(payload).length < 4000);

  assert.equal(cmd.module, 'confessions');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.deepEqual({ ...cmd.subcommandPermissions }, {
    panel: PERMISSION_LEVELS.ADMIN, delete: PERMISSION_LEVELS.MODERATOR, block: PERMISSION_LEVELS.MODERATOR,
    unblock: PERMISSION_LEVELS.MODERATOR, reveal: PERMISSION_LEVELS.ADMIN, setup: PERMISSION_LEVELS.ADMIN,
  });
  assert.equal(cmd.allowInDMs, undefined);
});

// ── Anonymity ─────────────────────────────────────────────────────────────────

test('ANONYMITY: nothing a member or moderator can read names the author, in any flow', async () => {
  logLines = [];
  const world = await setupWorld('GZ1', { confessionAllowImages: true, modLogChannelId: 'LOG' }, { review: true });
  const staffReplies = [];
  const keep = (i) => { staffReplies.push(...i.replies); return i; };

  // 1. Submit with an image into the review queue; approve it.
  await submitCmd(world, { vals: { text: 'one with a picture', image: imageOption('IMG_alice_selfie.png') } });
  keep(await reviewClick(world, 1, 'approve'));
  // 2. Submit and reject.
  await submitCmd(world, { vals: { text: 'two' } });
  keep(await reviewClick(world, 2, 'reject'));
  // 3. Submit, delete while pending.
  await submitCmd(world, { vals: { text: 'three' } });
  keep(await staffCmd(world, 'delete', { number: 3 }));
  // 4. Submit through the form, approve, delete the post.
  const form = fakeInteraction({ guild: world.w.guild, user: world.author, customId: 'confession-modal', fields: { 'confession-text': 'four from the form' } });
  await handleConfessionModal(form, world.w.client);
  keep(await reviewClick(world, 4, 'approve'));
  keep(await staffCmd(world, 'delete', { number: 4 }));
  // 5. Staff tools: block, unblock, block again, setup, panel.
  keep(await staffCmd(world, 'block', { number: 1 }));
  keep(await staffCmd(world, 'block', { number: 2 }));
  keep(await staffCmd(world, 'unblock', { number: 2 }));
  keep(await staffCmd(world, 'setup', {}, { userId: 'ADMIN1', level: 'admin' }));
  const panelChannel = fakeChannel('PANEL', { guildId: 'GZ1' });
  world.w.guild.channels.cache.set('PANEL', panelChannel);
  keep(await staffCmd(world, 'panel', { channel: panelChannel }, { userId: 'ADMIN1', level: 'admin' }));
  // 6. Hooks.
  await onMessageDelete({ guildId: 'GZ1', message: { id: rowOf('GZ1', 1).messageId ?? 'x' } });
  // 7. Reveal: the ONE place the author appears, in the admin's ephemeral reply.
  const reveal = await staffCmd(world, 'reveal', { number: 1 }, { userId: 'ADMIN1', level: 'admin' });
  assert.match(JSON.stringify(reveal.replies), new RegExp(AUTHOR), 'sanity: the reveal does show the author to the admin');

  const visible = JSON.stringify({
    post: world.post.sent.map((s) => s.json), postEdits: world.post.edits.map((e) => e.json),
    review: world.rev.sent.map((s) => s.json), reviewEdits: world.rev.edits.map((e) => e.json),
    reviewMessages: [...world.rev.store.values()].map((m) => ({ e: m.embeds, c: m.components, f: m.files.map((f) => f.name) })),
    log: world.log.sent.map((s) => s.json), panel: panelChannel.sent.map((s) => s.json),
    staffReplies,
  });
  for (const n of NEEDLES) assert.equal(visible.includes(n), false, `staff-visible output contains ${n}`);
  assert.ok(world.post.sent.length >= 2 && world.rev.sent.length >= 4 && world.log.sent.length === 1, 'the scan covered the flows');
  assert.equal(world.log.sent[0].json.includes(AUTHOR), false);

  const printed = logLines.join('\n');
  for (const n of NEEDLES) assert.equal(printed.includes(n), false, `the process log contains ${n}`);
  assert.ok(logLines.some((l) => l.includes('revealed by ADMIN1')), 'the reveal is logged with the admin, not the author');
});

test('ANONYMITY: a database error that echoes its arguments is logged by code only', async () => {
  logLines = [];
  const world = await setupWorld('GZ2');
  confessionModel.failCreate = Object.assign(
    new Error(`Invalid \`prisma.confession.create()\` invocation: data: { authorId: '${AUTHOR}', content: 'a secret text' }`),
    { code: 'P1001', name: 'PrismaClientInitializationError' },
  );
  const i = await submitCmd(world, { vals: { text: 'a secret text' } });
  confessionModel.failCreate = null;
  assert.match(lastEmbed(i).title, /Could Not Save/);
  const printed = logLines.join('\n');
  assert.ok(printed.includes('reserving a confession failed'), 'the failure is logged');
  assert.equal(printed.includes(AUTHOR), false);
  assert.equal(printed.includes('a secret text'), false);
  assert.equal(world.post.sent.length, 0);
});

test('ANONYMITY: the form pre-check logs a failing or slow cooldown lookup by code only', async () => {
  logLines = [];
  const world = await setupWorld('GZ3', { confessionCooldownSeconds: 600 });
  const realFind = confessionModel.findFirst;
  confessionModel.findFirst = async () => {
    await new Promise((r) => { setTimeout(r, 400); });
    throw Object.assign(new Error(`Invalid \`prisma.confession.findFirst()\` invocation: where: { guildId: 'GZ3', authorId: '${AUTHOR}' }`), { code: 'P2024' });
  };
  // 2 s old: about 200 ms of acknowledgement budget are left, so the timeout wins and the lookup rejects 200 ms after it.
  const click = fakeInteraction({ guild: world.w.guild, user: world.author, customId: 'confession:new' });
  click.createdTimestamp = Date.now() - 2000;
  await handleButton(click, world.w.client);
  await new Promise((r) => { setTimeout(r, 600); });
  confessionModel.findFirst = realFind;
  assert.equal(click.modals.length, 1, 'the form still opens when the lookup is not in time');
  const printed = logLines.join('\n');
  assert.equal(printed.includes(AUTHOR), false, printed);
  assert.ok(printed.includes('cooldown lookup failed'));
});

test('ANONYMITY: the service never reads authorId outside cooldown, block and reveal', async () => {
  const { readFileSync } = await import('node:fs');
  const files = ['src/services/confessionService.js', 'src/commands/community/confession.js', 'src/plugins/confessions.js']
    .map((f) => readFileSync(new URL(`../${f}`, import.meta.url), 'utf8'));
  const uses = files.flatMap((src, n) => src.split('\n').map((line, ln) => ({ line, file: n, ln })).filter((l) => /authorId/.test(l.line) && !/^\s*(\/\/|\*)/.test(l.line)));
  // create (writes it), cooldown (reads it), block (reads it), reveal result, describeAuthor and the reveal reply, the submit-time check.
  assert.ok(uses.length <= 14, `authorId appears on ${uses.length} code lines:\n${uses.map((u) => u.line.trim()).join('\n')}`);
  for (const u of uses) assert.doesNotMatch(u.line, /Embed|setDescription|setTitle|\.send\(|embeds:/, u.line);
});
