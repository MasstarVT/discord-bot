// Regression tests for three low-severity findings, offline (no Discord, Redis or Postgres):
//   1. /search: two members of one server could use up the bot-wide upstream budget and refuse every other server.
//   2. Backup restore: a same-named role or channel made by someone else was adopted, raised and given the original's overwrites.
//   3. newTargets: a private option typed by name silently reused an existing channel that more people than the admin could read.
import { test, after, afterEach, mock } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL ??= 'postgresql://test:test@127.0.0.1:1/test';
// Nothing listens here; the client retries in the background until after().
process.env.REDIS_URL = 'redis://127.0.0.1:1';

const {
  ApplicationCommandOptionType: T, AuditLogEvent, ChannelType, Collection, CommandInteractionOptionResolver,
  OverwriteType, PermissionFlagsBits: P, PermissionsBitField, SlashCommandBuilder,
} = await import('discord.js');
const search = await import('../src/services/searchService.js');
const snap = await import('../src/utils/guildSnapshot.js');
const { resolveNewOptions, reuseNote, newChannelOption } = await import('../src/utils/newTargets.js');
const { LIMITS } = await import('../src/config/constants.js');
const { default: redis } = await import('../src/services/redis.js');

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

after(() => {
  Object.assign(console, quiet);
  mock.timers.reset();
  search.__resetState();
  redis.disconnect();
});
afterEach(() => {
  mock.timers.reset();
  search.__resetState();
});

// ── 1. /search budget ─────────────────────────────────────────────────────────

const NOW = Math.floor(Date.UTC(2026, 9, 29, 12, 0, 0) / 60_000) * 60_000; // the start of a minute bucket
const BUCKET = Math.floor(NOW / 60_000);

/** In-memory `cache`. The cooldown (setNX) always lets a request through, so the minute slices are what is under test. */
function makeCache() {
  const store = new Map();
  const incrs = [];
  return {
    store,
    incrs,
    async get(k) { return store.has(k) ? structuredClone(store.get(k)) : null; },
    async set(k, v) { store.set(k, structuredClone(v)); return true; },
    async setNX() { return true; },
    async incr(k, ttl) {
      incrs.push({ k, ttl });
      const n = (store.get(k) ?? 0) + 1;
      store.set(k, n);
      return n;
    },
  };
}

/** A counting AniList safeFetch that answers "no results" (cached, but every query here is unique). */
function aniFetch() {
  const fn = async () => ({ ok: true, status: 200, headers: {}, json: () => ({ data: { Page: { media: [] } } }) });
  const wrapped = async (...args) => { wrapped.calls += 1; return fn(...args); };
  wrapped.calls = 0;
  return wrapped;
}

function setupSearch() {
  const cache = makeCache();
  const fetch = aniFetch();
  search.__resetState();
  search.__setDeps({ cache, safeFetch: fetch, peekSettings: () => null, getSettingsSafe: async () => null });
  return { cache, fetch };
}

const outcome = (p) => p.then(() => 'ok', (e) => e.code);

test('search: two members of one server cannot push another server into RATE_LIMITED', async () => {
  mock.timers.enable({ apis: ['Date'], now: NOW });
  const { cache, fetch } = setupSearch();
  const guildSlice = Math.floor(search.BUDGET_PER_MINUTE.anilist / LIMITS.SEARCH_GUILD_BUDGET_DIVISOR);

  // The attack: more unique lookups in a minute than the bot-wide budget, from two accounts of the same server.
  const results = [];
  for (let i = 0; i < search.BUDGET_PER_MINUTE.anilist + 6; i += 1) {
    results.push(await outcome(search.searchAniList('ANIME', `unique title ${i}`, { userId: i % 2 ? 'u2' : 'u1', guildId: 'gA' })));
  }
  assert.equal(fetch.calls, guildSlice, 'the server may send only its slice upstream');
  assert.equal(results.filter((r) => r === 'ok').length, guildSlice);
  assert.ok(results.includes('COOLDOWN'), 'the rest are refused by the slice');
  assert.equal(results.includes('RATE_LIMITED'), false, 'the bot-wide count is never reached');
  assert.equal(cache.store.get(`search:budget:anilist:${BUCKET}`), guildSlice, 'a request a slice refused used no bot-wide slot');

  // Victims in other servers still get an uncached lookup.
  assert.equal(await outcome(search.searchAniList('ANIME', 'victim one', { userId: 'u3', guildId: 'gB' })), 'ok');
  assert.equal(await outcome(search.searchAniList('ANIME', 'victim two', { userId: 'u4', guildId: 'gC' })), 'ok');
  assert.equal(fetch.calls, guildSlice + 2);

  // The attacking server stays refused for the rest of the minute, and is served again in the next one.
  assert.equal(await outcome(search.searchAniList('ANIME', 'still refused', { userId: 'u5', guildId: 'gA' })), 'COOLDOWN');
  mock.timers.setTime(NOW + 60_000);
  assert.equal(await outcome(search.searchAniList('ANIME', 'next minute', { userId: 'u1', guildId: 'gA' })), 'ok');

  // Every counter has a TTL, and a refusal says when to try again.
  assert.ok(cache.incrs.length > 0 && cache.incrs.every((c) => c.ttl === 120));
  mock.timers.setTime(NOW);
  const err = await search.searchAniList('ANIME', 'refused again', { userId: 'u1', guildId: 'gA' }).catch((e) => e);
  assert.equal(err.retryAt, NOW + 60_000);
});

test('search: one member is held to the per-user slice across servers; the bot-wide count stays the hard cap', async () => {
  mock.timers.enable({ apis: ['Date'], now: NOW });
  const { fetch } = setupSearch();
  const results = [];
  for (let i = 0; i < LIMITS.SEARCH_USER_PER_MINUTE + 4; i += 1) {
    results.push(await outcome(search.searchAniList('ANIME', `solo ${i}`, { userId: 'u1', guildId: `g${i % 3}` })));
  }
  assert.equal(fetch.calls, LIMITS.SEARCH_USER_PER_MINUTE);
  assert.equal(results.filter((r) => r === 'COOLDOWN').length, 4);

  // Many accounts spread over many servers can still reach the bot-wide limit, and that says RATE_LIMITED.
  search.__resetState();
  const wide = setupSearch();
  const codes = [];
  for (let i = 0; i < search.BUDGET_PER_MINUTE.anilist + 2; i += 1) {
    codes.push(await outcome(search.searchAniList('ANIME', `wide ${i}`, { userId: `user${i}`, guildId: `guild${i}` })));
  }
  assert.equal(wide.fetch.calls, search.BUDGET_PER_MINUTE.anilist);
  assert.deepEqual(codes.slice(-2), ['RATE_LIMITED', 'RATE_LIMITED']);
});

// ── 2. Backup restore adoption ────────────────────────────────────────────────

const BOT = '9999';
const EVERYONE = '900';
const REASON = 'Server backup restore test';
const SNAPSHOT = snap.parseSnapshot({
  version: 1,
  takenAt: '2026-10-29T10:00:00.000Z',
  guild: { id: EVERYONE, name: 'Test Server' },
  everyone: { permissions: '0' },
  roles: [{ id: '101', name: 'Moderator', permissions: String(P.KickMembers), position: 6 }],
  categories: [],
  channels: [
    // Still exists: it only gets the overwrite of the recreated Moderator role.
    { id: '201', type: ChannelType.GuildText, name: 'mods', position: 0, overwrites: [
      { id: EVERYONE, type: 'role', allow: '0', deny: String(P.ViewChannel) },
      { id: '101', type: 'role', allow: String(P.ViewChannel | P.SendMessages), deny: '0' },
    ] },
    // Deleted: the restore makes it again, private, with the Moderator role.
    { id: '202', type: ChannelType.GuildText, name: 'staff', position: 1, overwrites: [
      { id: EVERYONE, type: 'role', allow: '0', deny: String(P.ViewChannel) },
      { id: '101', type: 'role', allow: String(P.ViewChannel), deny: '0' },
    ] },
  ],
});

/** A fake guild with the manager methods a restore uses, an audit log, and a record of every mutating call. */
function restoreGuild({ auditFails = false, noAudit = false } = {}) {
  let seq = 7000;
  const log = { rolesCreated: [], channelsCreated: [], rolePositions: [], put: [], auditReads: 0 };
  const audit = [];
  const roleCache = new Map();
  const channelCache = new Map();
  const fakeRole = ({ id, name, pos = 1, created = 1 }) => ({
    id, name, colors: { primaryColor: 0 }, hoist: false, mentionable: false, permissions: { bitfield: 0n },
    rawPosition: pos, unicodeEmoji: null, managed: false, createdTimestamp: created,
  });
  const overwritesOf = (list) => new Map(list.map((o) => [o.id, { id: o.id, type: o.type ?? OverwriteType.Role, allow: { bitfield: o.allow ?? 0n }, deny: { bitfield: o.deny ?? 0n } }]));
  const fakeChannel = ({ id, name, created = 1, overwrites = [] }) => ({
    id, type: ChannelType.GuildText, name, parentId: null, rawPosition: 0, topic: null, nsfw: false, rateLimitPerUser: 0, createdTimestamp: created,
    manageable: true, permissionOverwrites: { cache: overwritesOf(overwrites) }, isThread: () => false,
  });
  const everyone = fakeRole({ id: EVERYONE, name: '@everyone', pos: 0 });
  roleCache.set(EVERYONE, everyone);
  roleCache.set('999', fakeRole({ id: '999', name: 'Bot', pos: 10 }));
  channelCache.set('201', fakeChannel({ id: '201', name: 'mods', overwrites: [{ id: EVERYONE, deny: P.ViewChannel }] }));

  const me = {
    id: BOT,
    permissions: new PermissionsBitField([P.ManageRoles, P.ManageChannels, P.ViewChannel, P.SendMessages]),
    roles: { highest: { rawPosition: 10 } },
  };
  const guild = {
    id: EVERYONE, ownerId: 'OWNER', available: true, features: [], maximumBitrate: 96000,
    roles: {
      cache: roleCache, everyone,
      async create(data) {
        log.rolesCreated.push(data);
        const role = fakeRole({ id: String(seq++), name: data.name, pos: 1, created: Date.now() });
        roleCache.set(role.id, role);
        audit.push({ event: AuditLogEvent.RoleCreate, targetId: role.id, executorId: BOT, reason: data.reason, createdTimestamp: Date.now() });
        return role;
      },
      async setPositions(list) {
        log.rolePositions.push(list);
        for (const e of list) roleCache.get(e.role).rawPosition = e.position;
      },
    },
    channels: {
      cache: channelCache,
      async create(data) {
        log.channelsCreated.push(data);
        const ch = fakeChannel({
          id: String(seq++), name: data.name, created: Date.now(),
          overwrites: (data.permissionOverwrites ?? []).map((o) => ({ id: o.id, type: o.type, allow: o.allow, deny: o.deny })),
        });
        channelCache.set(ch.id, ch);
        audit.push({ event: AuditLogEvent.ChannelCreate, targetId: ch.id, executorId: BOT, reason: data.reason, createdTimestamp: Date.now() });
        return ch;
      },
      async setPositions() {},
    },
    members: { me, cache: new Map(), async fetch(id) { return { id }; } },
    client: { rest: { async put(route, options) { log.put.push({ route, ...options }); return {}; } } },
    log, audit,
    /** Someone else made it: it is in the guild cache and in the audit log under another executor. */
    addStranger(kind, name) {
      const id = String(seq++);
      if (kind === 'role') roleCache.set(id, fakeRole({ id, name, pos: 3, created: Date.now() }));
      else channelCache.set(id, fakeChannel({ id, name, created: Date.now() }));
      audit.push({ event: kind === 'role' ? AuditLogEvent.RoleCreate : AuditLogEvent.ChannelCreate, targetId: id, executorId: 'STRANGER', reason: null, createdTimestamp: Date.now() });
      return id;
    },
    /** What an earlier try of this restore made before its id was saved: in the cache and in the audit log under the bot. */
    addOwnLate(kind, name, { reason = REASON } = {}) {
      const id = String(seq++);
      if (kind === 'role') roleCache.set(id, fakeRole({ id, name, pos: 1, created: Date.now() }));
      else channelCache.set(id, fakeChannel({ id, name, created: Date.now() }));
      audit.push({ event: kind === 'role' ? AuditLogEvent.RoleCreate : AuditLogEvent.ChannelCreate, targetId: id, executorId: BOT, reason, createdTimestamp: Date.now() });
      return id;
    },
  };
  if (!noAudit) {
    guild.fetchAuditLogs = async ({ type }) => {
      log.auditReads += 1;
      if (auditFails) throw Object.assign(new Error('Missing Permissions'), { code: 50013 });
      return { entries: new Map(audit.filter((e) => e.event === type).map((e) => [e.targetId, e])) };
    };
  }
  return guild;
}

/** Runs the restore like the job does (state through JSON between batches). `retry` and `stalls` mark the first batch only. */
async function restore(guild, { retry = false, stalls = 0 } = {}) {
  let state = snap.newRestoreState({ backupId: 'bk000001', mode: 'missing', startedById: 'OWNER', channelId: 'CH', now: Date.now() - 1000 });
  state.retry = retry;
  state.stalls = stalls;
  for (let i = 0; i < 40; i += 1) {
    state = snap.normaliseRestoreState(JSON.parse(JSON.stringify(state)));
    const env = snap.makeEnv({ guild, snapshot: SNAPSHOT, state, reason: REASON });
    const result = await snap.runRestoreBatch(env);
    state.retry = false;
    if (result === 'finish') return state;
  }
  throw new Error('the restore did not finish');
}

const roleTargets = (guild) => [...guild.log.rolePositions.flat().map((e) => e.role), ...guild.log.put.map((p) => p.route.split('/permissions/')[1])];
const channelTargets = (guild) => guild.log.put.map((p) => p.route.split('/')[2]);

// The attempt a restore makes: the first one, one after a timed-out create (stalls) and one after a crash (retry).
const ATTEMPTS = [
  ['first attempt', {}],
  ['after a timed-out create', { stalls: 1 }],
  ['after a crash (attempt 2)', { retry: true }],
];

for (const [label, attempt] of ATTEMPTS) {
  test(`restore (${label}): a same-named role made by someone else is not adopted, raised or given overwrites`, async () => {
    const guild = restoreGuild();
    const stranger = guild.addStranger('role', 'Moderator');
    const state = await restore(guild, attempt);
    assert.equal(guild.log.rolesCreated.length, 1, 'the role is made new');
    const made = state.idMap['101'];
    assert.ok(made && made !== stranger, 'the snapshot role maps to the new role');
    assert.equal(state.made.includes(stranger), false);
    assert.equal(roleTargets(guild).includes(stranger), false, 'neither moved nor given an overwrite');
    assert.ok(guild.log.put.some((p) => p.route === `/channels/201/permissions/${made}`), 'the new role gets the overwrite');
  });

  test(`restore (${label}): a same-named channel made by someone else is not adopted or written to`, async () => {
    const guild = restoreGuild();
    const stranger = guild.addStranger('channel', 'staff');
    const state = await restore(guild, attempt);
    assert.equal(guild.log.channelsCreated.filter((c) => c.name === 'staff').length, 1, 'the channel is made new');
    assert.ok(state.idMap['202'] && state.idMap['202'] !== stranger);
    assert.equal(channelTargets(guild).includes(stranger), false, 'nothing is written onto it');
    const made = guild.channels.cache.get(state.idMap['202']);
    assert.ok(made.permissionOverwrites.cache.has(EVERYONE), 'the new channel is private from the start');
  });
}

test('restore: what the bot made for this restore before its id was saved is still adopted (a stranger\'s twin does not shadow it)', async () => {
  for (const attempt of [{ stalls: 1 }, { retry: true }]) {
    const guild = restoreGuild();
    const stranger = guild.addStranger('role', 'Moderator'); // listed first in the cache
    const own = guild.addOwnLate('role', 'Moderator');
    const state = await restore(guild, attempt);
    assert.equal(guild.log.rolesCreated.length, 0, 'nothing is made twice');
    assert.equal(state.idMap['101'], own);
    assert.equal(roleTargets(guild).includes(stranger), false);
    assert.ok(guild.log.put.some((p) => p.route === `/channels/201/permissions/${own}`));
  }
  const guild = restoreGuild();
  const strangerChannel = guild.addStranger('channel', 'staff');
  const ownChannel = guild.addOwnLate('channel', 'staff');
  const state = await restore(guild, { retry: true });
  assert.equal(guild.log.channelsCreated.length, 0);
  assert.equal(state.idMap['202'], ownChannel);
  assert.equal(channelTargets(guild).includes(strangerChannel), false);
});

test('restore: never adopted on the first attempt, nor without an audit log that confirms the bot made it for this restore', async () => {
  // First attempt: even a role the bot made with this restore's reason is not looked for, and the log is not read.
  const first = restoreGuild();
  const early = first.addOwnLate('role', 'Moderator');
  const firstState = await restore(first);
  assert.equal(first.log.rolesCreated.length, 1);
  assert.notEqual(firstState.idMap['101'], early);
  assert.equal(first.log.auditReads, 0, 'no audit read when nothing may be adopted');

  // A retry that cannot confirm makes a new one: a made-twice role is safer than a stranger's.
  for (const guild of [restoreGuild({ auditFails: true }), restoreGuild({ noAudit: true })]) {
    const candidate = guild.addOwnLate('role', 'Moderator');
    const state = await restore(guild, { retry: true });
    assert.equal(guild.log.rolesCreated.length, 1, 'the log could not be read');
    assert.notEqual(state.idMap['101'], candidate);
    assert.equal(state.errors.length, 0, 'an unreadable audit log is not a restore error');
  }

  // Made by the bot, but for something else (a different reason), or before this restore started: not this restore's.
  const other = restoreGuild();
  const forSomeoneElse = other.addOwnLate('role', 'Moderator', { reason: '/liverole by someone (1)' });
  const state = await restore(other, { retry: true });
  assert.equal(other.log.rolesCreated.length, 1);
  assert.notEqual(state.idMap['101'], forSomeoneElse);
  const old = restoreGuild();
  const oldRole = old.addOwnLate('role', 'Moderator');
  old.audit.find((e) => e.targetId === oldRole).createdTimestamp = Date.now() - 3_600_000;
  assert.notEqual((await restore(old, { retry: true })).idMap['101'], oldRole);
});

test('restore job: the retry signal is kept in the payload and cleared after one batch', () => {
  const fresh = snap.newRestoreState({ backupId: 'bk000001', mode: 'missing' });
  assert.equal(fresh.retry, false);
  assert.equal(snap.normaliseRestoreState({ ...fresh, retry: true }).retry, true);
  assert.equal(snap.normaliseRestoreState({ ...fresh, retry: 'yes' }).retry, false, 'only a real true counts');
  assert.equal(snap.normaliseRestoreState(fresh).retry, false);
});

// ── 3. Private channel reuse by name ──────────────────────────────────────────

const data = new SlashCommandBuilder().setName('demo').setDescription('demo')
  .addSubcommand((s) => s.setName('log').setDescription('d')
    .addChannelOption((o) => o.setName('channel').setDescription('d').addChannelTypes(ChannelType.GuildText))
    .addStringOption((o) => newChannelOption(o)));

const ADMIN_ROLE = 'RADMIN';
const HELPER_ROLE = 'RHELPER';
const BOT_ROLE = 'RBOT';

/** `viewers`: ids that can view the channel, as discord.js would compute it (everyone: 'G', the invoker: 'U'). */
function channelOf(id, { viewers = [], overwrites = [] } = {}) {
  return {
    id, name: 'mod-logs', type: ChannelType.GuildText,
    permissionsFor: (target) => new PermissionsBitField(viewers.includes(typeof target === 'string' ? target : target.id) ? [P.ViewChannel] : []),
    permissionOverwrites: {
      cache: new Collection(overwrites.map((o) => [o.id, {
        id: o.id, type: o.type ?? OverwriteType.Role,
        allow: new PermissionsBitField(o.allow ?? []), deny: new PermissionsBitField(o.deny ?? []),
      }])),
    },
  };
}

function privateReuse(channel, { memberPerms = [P.ManageGuild] } = {}) {
  const created = [];
  const channels = new Collection([[channel.id, channel]]);
  const guild = {
    id: 'G',
    roles: {
      cache: new Collection([
        ['G', { id: 'G', name: '@everyone', managed: false, permissions: new PermissionsBitField([]) }],
        [ADMIN_ROLE, { id: ADMIN_ROLE, name: 'Admins', managed: false, permissions: new PermissionsBitField([P.ManageGuild]) }],
        [HELPER_ROLE, { id: HELPER_ROLE, name: 'Helpers', managed: false, permissions: new PermissionsBitField([P.ManageChannels]) }],
      ]),
      async create() { throw new Error('no role is made here'); },
    },
    channels: { cache: channels, async create(o) { created.push(o); return { id: 'NEW', name: o.name, type: o.type }; } },
    members: {
      me: { id: 'BOT', permissions: new PermissionsBitField([P.ViewChannel, P.SendMessages]), roles: { botRole: { id: BOT_ROLE } } },
      // Cached: another admin (Manage Server) and an ordinary member. Not cached but fetchable: a second admin. Anyone else: unknown member.
      cache: new Collection([
        ['M_ADMIN', { id: 'M_ADMIN', permissions: new PermissionsBitField([P.ManageGuild]) }],
        ['M2', { id: 'M2', permissions: new PermissionsBitField([P.ManageMessages]) }],
      ]),
      async fetch(id) {
        if (id === 'M_FETCHED') return { id, permissions: new PermissionsBitField([P.Administrator]) };
        throw Object.assign(new Error('Unknown Member'), { code: 10007 });
      },
    },
  };
  const replies = [];
  const interaction = {
    commandName: 'demo', guildId: 'G', guild,
    user: { id: 'U', username: 'admin' },
    member: { id: 'U', permissions: new PermissionsBitField(memberPerms) }, // Manage Server, not Administrator
    memberPermissions: new PermissionsBitField([...memberPerms, P.ManageChannels]),
    options: new CommandInteractionOptionResolver({}, [{ name: 'log', type: T.Subcommand, options: [{ name: 'new_channel', type: T.String, value: 'mod-logs' }] }], null),
    deferred: true, replied: false, ephemeral: false,
    async editReply(p) { replies.push(p); return {}; },
    async followUp(p) { replies.push(p); return {}; },
  };
  return { interaction, created, replies };
}

const title = (replies) => replies.at(-1)?.embeds?.[0]?.toJSON().title?.replace(/^[^A-Za-z]+/, '');
const description = (replies) => replies.at(-1)?.embeds?.[0]?.toJSON().description;
const resolvePrivate = (interaction) => resolveNewOptions(interaction, data, { private: ['channel'] });

const OK = [P.ViewChannel];
const REUSABLE = [
  ['a channel only the invoker, the bot and its role can see', { viewers: ['U'], overwrites: [{ id: 'G', deny: OK }, { id: 'BOT', type: OverwriteType.Member, allow: OK }, { id: 'U', type: OverwriteType.Member, allow: OK }, { id: BOT_ROLE, allow: OK }] }],
  ['a channel a Manage Server role can also see', { viewers: ['U'], overwrites: [{ id: 'G', deny: OK }, { id: ADMIN_ROLE, allow: OK }] }],
  ['a channel another Manage Server holder can also see (made for them earlier)', { viewers: ['U'], overwrites: [{ id: 'G', deny: OK }, { id: 'M_ADMIN', type: OverwriteType.Member, allow: OK }] }],
  ['a channel an Administrator who is not cached can also see (fetched)', { viewers: ['U'], overwrites: [{ id: 'G', deny: OK }, { id: 'M_FETCHED', type: OverwriteType.Member, allow: OK }] }],
];
const REFUSED = [
  ['a role that is not an admin role can see it', { viewers: ['U'], overwrites: [{ id: 'G', deny: OK }, { id: HELPER_ROLE, allow: OK }] }, /roles or members besides you/],
  ['an ordinary member can see it', { viewers: ['U'], overwrites: [{ id: 'G', deny: OK }, { id: 'M2', type: OverwriteType.Member, allow: OK }] }, /roles or members besides you/],
  ['a member that cannot be looked up can see it (fail closed)', { viewers: ['U'], overwrites: [{ id: 'G', deny: OK }, { id: 'M_GONE', type: OverwriteType.Member, allow: OK }] }, /roles or members besides you/],
  ['@everyone can see it', { viewers: ['G', 'U'], overwrites: [] }, /everyone can see it/],
];

for (const [label, spec] of REUSABLE) {
  test(`private option: ${label} is reused, and the reply says so`, async () => {
    const { interaction, created, replies } = privateReuse(channelOf('C1', spec));
    assert.equal(await resolvePrivate(interaction), true);
    assert.equal(interaction.options.getChannel('channel').id, 'C1');
    assert.equal(created.length, 0);
    assert.equal(replies.length, 0);
    assert.match(reuseNote(interaction), /Reused the existing <#C1> instead of creating/);
  });
}

for (const [label, spec, why] of REFUSED) {
  test(`private option: refused when ${label}`, async () => {
    const { interaction, created, replies } = privateReuse(channelOf('C2', spec));
    assert.equal(await resolvePrivate(interaction), false);
    assert.equal(title(replies), 'Channel Name Taken');
    assert.match(description(replies), why);
    assert.match(description(replies), /Pick it in `channel`/);
    assert.equal(created.length, 0, 'nothing is made either');
    assert.equal(reuseNote(interaction), null);
  });
}

test('private option: refused when the admin typing the name cannot see the channel, without pointing at it', async () => {
  const { interaction, created, replies } = privateReuse(channelOf('C3', { viewers: [], overwrites: [{ id: 'G', deny: OK }, { id: HELPER_ROLE, allow: OK }] }));
  assert.equal(await resolvePrivate(interaction), false);
  assert.equal(title(replies), 'Channel Name Taken');
  assert.match(description(replies), /you cannot see it/);
  assert.equal(description(replies).includes('C3'), false, 'the hidden channel is not named');
  assert.equal(created.length, 0);
});

test('private option: the same channel is still reused when the option is not private', async () => {
  const { interaction } = privateReuse(channelOf('C4', { viewers: ['G', 'U'] }));
  assert.equal(await resolveNewOptions(interaction, data), true);
  assert.equal(interaction.options.getChannel('channel').id, 'C4');
  assert.equal(reuseNote(interaction), null, 'only a private reuse is announced');
});
