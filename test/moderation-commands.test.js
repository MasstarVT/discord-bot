// pkg-moderation-commands tests (pkg-moderation.md §9 items 13 and 15, restricted to the eight command files)
// plus behaviour cases that drive the commands through fake interactions and guilds.
// Offline by default (payloads, static scan, bounded-input errors). DB cases, which exercise executeModAction,
// the reply helpers and the Prisma queries of /case (history and clear-history included) and /warn, run with:
//   TEST_DATABASE_URL='<migrated db>' node --test --test-concurrency=1 test/moderation-commands.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is only reached with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const svc = await import('../src/services/moderationService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { updateSettings } = await import('../src/services/settingsService.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');

const COMMANDS = {
  ban: await import('../src/commands/moderation/ban.js'),
  case: await import('../src/commands/moderation/case.js'),
  kick: await import('../src/commands/moderation/kick.js'),
  softban: await import('../src/commands/moderation/softban.js'),
  timeout: await import('../src/commands/moderation/timeout.js'),
  unban: await import('../src/commands/moderation/unban.js'),
  unmute: await import('../src/commands/moderation/unmute.js'),
  warn: await import('../src/commands/moderation/warn.js'),
};

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};
after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const DAY = 86_400_000;

// ═════════════════════════════════════════════════════════════════════════════
// §9 item 13: command payloads
// ═════════════════════════════════════════════════════════════════════════════

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

const walkOptions = (node, fn) => {
  for (const o of node.options ?? []) { fn(o); walkOptions(o, fn); }
};

const PERMS = {
  ban: PermissionFlagsBits.BanMembers,
  kick: PermissionFlagsBits.KickMembers,
  softban: PermissionFlagsBits.BanMembers,
  timeout: PermissionFlagsBits.ModerateMembers,
  unban: PermissionFlagsBits.BanMembers,
  unmute: PermissionFlagsBits.ModerateMembers,
  warn: PermissionFlagsBits.ModerateMembers,
  case: PermissionFlagsBits.ModerateMembers,
};

test('command payloads: size, module, visibility defaults, option bounds', () => {
  assert.deepEqual(Object.keys(COMMANDS).sort(), Object.keys(PERMS).sort());
  for (const [name, mod] of Object.entries(COMMANDS)) {
    const json = buildCommandPayload(mod);
    assert.equal(json.name, name);
    assert.ok(charCount(json) < 4000, `/${name}: ${charCount(json)} characters`);
    assert.equal(mod.module, 'moderation', `/${name}: module`);
    assert.notEqual(mod.gated, false, `/${name} is gated by the moderation module`);
    assert.equal(mod.permissionLevel, PERMISSION_LEVELS.MODERATOR, `/${name}: permissionLevel`);
    assert.notEqual(mod.allowInDMs, true, `/${name} is guild-only`);
    assert.equal(json.default_member_permissions, String(PERMS[name]), `/${name}: setDefaultMemberPermissions`);
    assert.equal(typeof mod.execute, 'function');

    walkOptions(json, (o) => {
      if (o.type !== 3) return; // string options only (/case has a subcommand named reason)
      if (o.name === 'reason') assert.ok([512, ...(name === 'case' ? [1000] : [])].includes(o.max_length), `/${name} reason max_length`);
      if (o.name === 'proof') assert.equal(o.max_length, 1000, `/${name} proof max_length`);
    });
  }
  // Measured sizes stay close to the spec table (pkg-moderation.md §5); they only need to stay under 4000.
  const sizes = Object.fromEntries(Object.entries(COMMANDS).map(([n, m]) => [n, charCount(buildCommandPayload(m))]));
  assert.ok(sizes.case < 700 && sizes.ban < 500, JSON.stringify(sizes));
});

test('command payloads: /case, /ban, /timeout, /unban, /warn shapes', () => {
  const cas = buildCommandPayload(COMMANDS.case);
  assert.deepEqual(cas.options.map((o) => o.name), ['view', 'reason', 'proof', 'history', 'clear-history']);
  assert.deepEqual(COMMANDS.case.subcommandPermissions, { 'clear-history': PERMISSION_LEVELS.ADMIN });
  assert.equal(COMMANDS.case.subcommandPermissions['clear-history'], PERMISSION_LEVELS.ADMIN);
  const subOf = (name) => cas.options.find((o) => o.name === name);
  const type = subOf('history').options.find((o) => o.name === 'type');
  assert.equal(type.choices.length, 10);
  assert.ok(type.choices.some((c) => c.value === 'active') && type.choices.some((c) => c.value === 'UNMUTE'));
  assert.equal(subOf('history').options.find((o) => o.name === 'user').required, true);
  assert.equal(subOf('clear-history').options.find((o) => o.name === 'user').required, true);
  assert.equal(subOf('proof').options.find((o) => o.name === 'url').max_length, 1000, '/case proof url is bounded (R095)');
  assert.equal(subOf('reason').options.find((o) => o.name === 'reason').max_length, 1000, '/case reason is bounded (R095)');
  assert.equal(subOf('clear-history').options.find((o) => o.name === 'reason').max_length, 512, '/case clear-history reason keeps its 512 bound');
  for (const name of ['view', 'reason', 'proof']) assert.equal(subOf(name).options.find((o) => o.name === 'id').min_value, 1);

  const ban = buildCommandPayload(COMMANDS.ban);
  assert.deepEqual(ban.options.map((o) => o.name), ['user', 'reason', 'duration', 'delete_days', 'proof']);
  assert.equal(ban.options.find((o) => o.name === 'duration').max_length, 32);
  assert.equal(ban.options.find((o) => o.name === 'delete_days').max_value, 7);

  const softban = buildCommandPayload(COMMANDS.softban);
  assert.equal(softban.options.find((o) => o.name === 'delete_days').min_value, 1);

  const timeout = buildCommandPayload(COMMANDS.timeout);
  assert.equal(timeout.options.find((o) => o.name === 'duration').required, true);

  const unban = buildCommandPayload(COMMANDS.unban);
  const uid = unban.options.find((o) => o.name === 'user_id');
  assert.deepEqual([uid.min_length, uid.max_length, uid.required], [17, 32, true]);

  const warn = buildCommandPayload(COMMANDS.warn);
  assert.equal(warn.options.find((o) => o.name === 'reason').required, true);
});

test('every moderationService import of a command file is a real export', () => {
  for (const name of Object.keys(COMMANDS)) {
    const text = readFileSync(resolve(ROOT, `src/commands/moderation/${name}.js`), 'utf8');
    const m = text.match(/import\s*\{([^}]*)\}\s*from\s*'\.\.\/\.\.\/services\/moderationService\.js'/);
    assert.ok(m, `${name}.js imports from moderationService.js`);
    for (const imported of m[1].split(',').map((s) => s.trim()).filter(Boolean)) {
      assert.ok(imported in svc, `${name}.js imports ${imported}, which moderationService.js does not export`);
    }
  }
});

// ═════════════════════════════════════════════════════════════════════════════
// §9 item 15: static scan (command files only)
// ═════════════════════════════════════════════════════════════════════════════

test('static scan of the eight command files', () => {
  let infractionCalls = 0;
  for (const name of Object.keys(COMMANDS)) {
    const f = `src/commands/moderation/${name}.js`;
    const text = readFileSync(resolve(ROOT, f), 'utf8');
    assert.doesNotMatch(text, /setCustomId\(|customId\s*:/, `${f}: customId literal`);
    assert.doesNotMatch(text, /\bconsole\./, `${f}: console`);
    assert.doesNotMatch(text, /cache\.del\(/, `${f}: cache.del`);
    assert.doesNotMatch(text, /deleteMessageDays/, `${f}: deleteMessageDays`);
    assert.doesNotMatch(text, /roles\.add\(\[/, `${f}: array-form roles.add`);
    assert.doesNotMatch(text, /\bassertHierarchy\b/, `${f}: assertHierarchy (the service does the checks)`);
    assert.doesNotMatch(text, /LIMITS\.[A-Z_]+\s*\?\?/, `${f}: LIMITS fallback`);
    assert.match(text, /export const module = 'moderation'/, `${f}: explicit module export`);
    assert.match(text, /deferReply\(\{ ephemeral: true \}\)/, `${f}: ephemeral defer`);
    const n = (text.match(/createInfraction\(/g) ?? []).length;
    if (name === 'case') infractionCalls += n;
    else assert.equal(n, 0, `${f}: createInfraction( is only allowed in case.js (the pardon NOTE)`);
  }
  assert.equal(infractionCalls, 1, 'case.js records exactly one NOTE through createInfraction');
  const history = readFileSync(resolve(ROOT, 'src/commands/moderation/case.js'), 'utf8');
  assert.match(history, /paginate\(interaction, pages, \{ followUp: true \}\)/, '/case history pages through followUp after deleteReply (R055)');
  assert.ok(history.indexOf('deleteReply()') < history.indexOf('{ followUp: true }'), 'deleteReply comes first');
});

// ═════════════════════════════════════════════════════════════════════════════
// Fakes
// ═════════════════════════════════════════════════════════════════════════════

const BOT = '100000000000000001';
const OWNER = '100000000000000002';
const ALICE = '100000000000000003';
const MOD = '100000000000000005';
const bits = (...names) => new PermissionsBitField(names.map((n) => PermissionFlagsBits[n]));

function mkRole(id, position = 1, extra = {}) {
  return { id, position, managed: false, mentionable: false, permissions: new PermissionsBitField(), ...extra };
}

function mkUser(id, extra = {}) {
  const user = {
    id,
    username: `user${id.slice(-3)}`,
    tag: `user${id.slice(-3)}`,
    bot: false,
    dms: [],
    dmsDeleted: 0,
    displayAvatarURL: () => 'https://cdn.example/a.png',
    ...extra,
  };
  user.send ??= async (p) => { user.dms.push(p); return { delete: async () => { user.dmsDeleted += 1; } }; };
  return user;
}

function mkMember(guild, id, { position = 1, perms = [], roles = [], user = mkUser(id), ...flags } = {}) {
  const member = {
    id,
    guild,
    user,
    permissions: bits(...perms),
    roles: {
      cache: new Map(roles.map((r) => [r.id, r])),
      highest: { position, comparePositionTo(other) { return this.position - other.position; } },
      add: async () => {},
      remove: async () => {},
    },
    moderatable: true,
    kickable: true,
    bannable: true,
    manageable: true,
    isCommunicationDisabled: () => false,
    timeout: async () => {},
    kick: async () => {},
    ...flags,
  };
  return member;
}

function mkGuild(id) {
  const guild = { id, ownerId: OWNER, available: true, name: 'Test Guild' };
  guild.members = {
    cache: new Map(),
    me: null,
    fetchMe: async () => guild.members.me,
    fetch: async (uid) => {
      const m = guild.members.cache.get(uid);
      if (!m) throw Object.assign(new Error('Unknown Member'), { code: 10007 });
      return m;
    },
  };
  guild.roles = { cache: new Map(), fetch: async () => new Map(guild.roles.cache) };
  guild.channels = { cache: new Map(), fetch: async () => null };
  guild.client = {
    user: { id: BOT, username: 'Bot', tag: 'Bot#0' },
    users: { cache: new Map(), fetch: async (uid) => mkUser(uid) },
  };
  guild.banCalls = [];
  guild.unbanCalls = [];
  guild.bans = {
    fetch: async () => { throw Object.assign(new Error('Unknown Ban'), { code: 10026 }); },
    create: async (uid, o) => { guild.banCalls.push([uid, o]); },
    remove: async (uid, reason) => { guild.unbanCalls.push([uid, reason]); },
  };
  const me = mkMember(guild, BOT, { position: 100, perms: ['BanMembers', 'KickMembers', 'ModerateMembers', 'ManageRoles', 'ViewAuditLog'] });
  guild.members.me = me;
  guild.members.cache.set(BOT, me);
  // The invoking moderator: a member with the moderation permissions, above the targets in the hierarchy.
  const mod = mkMember(guild, MOD, { position: 50, perms: ['BanMembers', 'KickMembers', 'ModerateMembers'], user: mkUser(MOD, { username: 'moddy', tag: 'moddy' }) });
  guild.members.cache.set(MOD, mod);
  guild.moderator = mod;
  return guild;
}

function fakeChannel(usable = true) {
  const sent = [];
  return {
    id: '800000000000000001',
    guild: { members: { me: {} } },
    sent,
    isTextBased: () => true,
    isThread: () => false,
    permissionsFor: () => (usable ? bits('ViewChannel', 'SendMessages', 'EmbedLinks') : bits('ViewChannel')),
    send: async (p) => { sent.push(p); },
  };
}

/**
 * A chat-input interaction double. After `deleteReply`, `editReply` fails like Discord does (10008), which is
 * the R026-R028 failure mode. `confirm` ('yes' | 'no') answers an awaitConfirm dialog.
 */
function mkInteraction(guild, { sub = null, opts = {}, channel = null, confirm = null } = {}) {
  const calls = [];
  const collectors = [];
  let gone = false;
  let lastPayload = null;
  const message = {
    id: '700000000000000001',
    createdTimestamp: Date.now(),
    edit: async () => {},
    createMessageComponentCollector: () => {
      const collector = new EventEmitter();
      collector.stop = (reason) => collector.emit('end', new Map(), reason);
      collectors.push(collector);
      if (confirm && lastPayload?.components?.[0]) {
        const ids = lastPayload.components[0].components.map((c) => c.data.custom_id);
        const picked = { customId: confirm === 'yes' ? ids[0] : ids[1], update: async (p) => { calls.push(['pickedUpdate', p]); } };
        setImmediate(() => { collector.emit('collect', picked); collector.emit('end', new Map(), 'limit'); });
      }
      return collector;
    },
  };
  const read = (name) => (opts[name] === undefined ? null : opts[name]);
  const i = {
    calls,
    collectors,
    guild,
    guildId: guild.id,
    client: guild.client,
    user: guild.moderator.user,
    member: guild.moderator,
    channel,
    createdTimestamp: Date.now(),
    deferred: false,
    replied: false,
    ephemeral: undefined,
    isRepliable: () => true,
    options: {
      getUser: (n) => opts[n] ?? null,
      getString: read,
      getInteger: read,
      getSubcommand: () => sub,
    },
    deferReply: async (o) => { calls.push(['deferReply', o]); i.deferred = true; i.ephemeral = Boolean(o?.ephemeral); },
    editReply: async (p) => {
      if (gone) throw Object.assign(new Error('Unknown Message'), { code: 10008 });
      lastPayload = p;
      calls.push(['editReply', p]);
      return message;
    },
    deleteReply: async () => { calls.push(['deleteReply']); gone = true; },
    followUp: async (p) => { calls.push(['followUp', p]); return message; },
  };
  return i;
}

const names = (i) => i.calls.map((c) => c[0]);
const callsOf = (i, name) => i.calls.filter((c) => c[0] === name).map((c) => c[1]);
const jsonOf = (payload) => payload.embeds.map((e) => (typeof e.toJSON === 'function' ? e.toJSON() : e));
const lastEdit = (i) => jsonOf(callsOf(i, 'editReply').at(-1))[0];
const publicPost = (i) => jsonOf(callsOf(i, 'followUp')[0])[0];

async function run(name, guild, init) {
  const i = mkInteraction(guild, init);
  await COMMANDS[name].execute(i, guild.client);
  return i;
}

// ═════════════════════════════════════════════════════════════════════════════
// Offline behaviour: errors that are decided before any database work
// ═════════════════════════════════════════════════════════════════════════════

test('/ban and /timeout refuse a duration they cannot read, before touching the service', async () => {
  const guild = mkGuild('955004000000000001');
  const target = mkUser(ALICE);

  const ban = await run('ban', guild, { opts: { user: target, duration: 'banana' } });
  assert.deepEqual(names(ban), ['deferReply', 'editReply']);
  assert.deepEqual(ban.calls[0][1], { ephemeral: true });
  assert.match(lastEdit(ban).title, /Invalid Duration/);
  assert.match(lastEdit(ban).description, /banana/);
  assert.equal(guild.banCalls.length, 0, 'a typed but unreadable duration must not become a permanent ban');

  const timeout = await run('timeout', guild, { opts: { user: target, duration: '5 parsecs' } });
  assert.deepEqual(names(timeout), ['deferReply', 'editReply']);
  assert.match(lastEdit(timeout).title, /Invalid Duration/);

  // a backtick in the input cannot break the message markup
  const tick = await run('ban', guild, { opts: { user: target, duration: '`x`' } });
  assert.doesNotMatch(lastEdit(tick).description, /``x``/);
});

test('/case bounds its input before any query (R095)', async () => {
  const guild = mkGuild('955004000000000002');

  const huge = await run('case', guild, { sub: 'view', opts: { id: 3_000_000_000 } });
  assert.deepEqual(names(huge), ['deferReply', 'editReply']);
  assert.match(lastEdit(huge).title, /Not Found/);
  assert.match(lastEdit(huge).description, /#3000000000/);

  const badLink = await run('case', guild, { sub: 'proof', opts: { id: 1, url: 'ftp://example.com/x' } });
  assert.match(lastEdit(badLink).title, /Invalid Link/);
  assert.equal(lastEdit(badLink).description, svc.MSG.BAD_PROOF);
  const noScheme = await run('case', guild, { sub: 'proof', opts: { id: 1, url: 'javascript:alert(1)' } });
  assert.match(lastEdit(noScheme).title, /Invalid Link/);
  const spaced = await run('case', guild, { sub: 'proof', opts: { id: 1, url: 'https://example.com/a b' } });
  assert.match(lastEdit(spaced).title, /Invalid Link/);

  const blank = await run('case', guild, { sub: 'reason', opts: { id: 1, reason: '   ' } });
  assert.match(lastEdit(blank).title, /Invalid Input/);
});

// ═════════════════════════════════════════════════════════════════════════════
// DB cases: guild ids '9550040…' belong to this package
// ═════════════════════════════════════════════════════════════════════════════

const G = (n) => `9550040000000000${String(n).padStart(2, '0')}`;
const TEST_GUILDS = Array.from({ length: 30 }, (_, i) => G(i + 1));

async function cleanup() {
  await prisma.scheduledJob.deleteMany({ where: { guildId: { in: TEST_GUILDS } } });
  await prisma.guildSettings.deleteMany({ where: { guildId: { in: TEST_GUILDS } } });
}
if (DB) {
  await cleanup();
  after(cleanup);
}

const rowsOf = (guildId, where = {}) => prisma.infraction.findMany({ where: { guildId, ...where }, orderBy: { caseId: 'asc' } });
const jobOf = (key) => prisma.scheduledJob.findUnique({ where: { dedupeKey: key } });

/** Guild with ALICE as a member (bannable, kickable, moderatable) and a spy-able DM channel. */
function guildWithAlice(n) {
  const guild = mkGuild(G(n));
  const alice = mkMember(guild, ALICE, { position: 5, user: mkUser(ALICE) });
  guild.members.cache.set(ALICE, alice);
  return { guild, alice, aliceUser: alice.user };
}

test('DB: /ban bans first, records after, and replies publicly (R024, R094)', { skip: skipDb }, async () => {
  const { guild, aliceUser } = guildWithAlice(1);
  const i = await run('ban', guild, { opts: { user: aliceUser, reason: 'spamming links', delete_days: 3, proof: 'https://example.com/proof' } });

  assert.deepEqual(names(i), ['deferReply', 'deleteReply', 'followUp']);
  assert.deepEqual(i.calls[0][1], { ephemeral: true });
  assert.equal(guild.banCalls.length, 1);
  assert.equal(guild.banCalls[0][0], ALICE);
  assert.equal(guild.banCalls[0][1].deleteMessageSeconds, 3 * 86_400);
  assert.match(guild.banCalls[0][1].reason, /spamming links \| by moddy/);
  assert.equal(aliceUser.dms.length, 1, 'the DM goes out before the ban');

  const post = publicPost(i);
  assert.match(post.title, /User Banned$/);
  assert.match(post.description, /\*\*User:\*\* <@100000000000000003> `user003`/);
  assert.match(post.description, /\*\*Reason:\*\* spamming links/);
  assert.match(post.description, /\*\*Duration:\*\* Permanent/);
  assert.match(post.description, /\*\*Messages deleted:\*\* 3 day\(s\)/);
  assert.match(post.description, /\*\*Case:\*\* #1/);
  assert.doesNotMatch(post.description, /moddy/, 'the public lines never name the moderator');
  assert.deepEqual(callsOf(i, 'followUp')[0].allowedMentions, { parse: [] });

  const rows = await rowsOf(guild.id);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].type, 'BAN');
  assert.equal(rows[0].active, true);
  assert.equal(rows[0].proofUrl, 'https://example.com/proof');
  assert.equal(rows[0].expiresAt, null);
});

test('DB: /ban duration makes a temporary ban with an expiry job (R003, R023)', { skip: skipDb }, async () => {
  const { guild, aliceUser } = guildWithAlice(2);
  const i = await run('ban', guild, { opts: { user: aliceUser, duration: '2d' } });

  const post = publicPost(i);
  assert.match(post.title, /User Temporarily Banned/);
  assert.match(post.description, /\*\*Duration:\*\* 2 days/);
  assert.match(post.description, /\*\*Ends:\*\* <t:\d+:R>/);
  assert.match(post.description, /\*\*Reason:\*\* No reason provided/);
  assert.doesNotMatch(post.description, /Messages deleted/);

  const [row] = await rowsOf(guild.id);
  assert.equal(row.type, 'BAN');
  assert.ok(row.expiresAt > new Date(Date.now() + DAY));
  const job = await jobOf(`mod.unban:${guild.id}:${ALICE}`);
  assert.ok(job, 'the unban job exists');
  assert.equal(job.type, 'mod.unban');
  assert.equal(job.maxAttempts, 20);
  assert.equal(job.runAt.getTime(), row.expiresAt.getTime());

  // a duration the service rejects (over a year) is explained, and nothing happens
  const { guild: g2, aliceUser: a2 } = guildWithAlice(3);
  const tooLong = await run('ban', g2, { opts: { user: a2, duration: '3y' } });
  assert.deepEqual(names(tooLong), ['deferReply', 'editReply']);
  assert.match(lastEdit(tooLong).title, /Invalid Input/);
  assert.equal(g2.banCalls.length, 0);
  assert.equal((await rowsOf(g2.id)).length, 0);
});

test('DB: /ban that Discord refuses records nothing and takes the DM back (R024, R025, R094)', { skip: skipDb }, async () => {
  const { guild, aliceUser } = guildWithAlice(4);
  guild.bans.create = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  const i = await run('ban', guild, { opts: { user: aliceUser, reason: 'x' } });

  assert.deepEqual(names(i), ['deferReply', 'editReply'], 'the error stays ephemeral');
  assert.match(lastEdit(i).title, /Missing Permissions/);
  assert.equal(aliceUser.dmsDeleted, 1, 'the false DM is taken back');
  assert.equal((await rowsOf(guild.id)).length, 0, 'no orphaned case');

  // the bot lacks Ban Members: refused before anything is sent
  const { guild: g2, aliceUser: a2 } = guildWithAlice(5);
  g2.members.me.permissions = bits('KickMembers');
  const j = await run('ban', g2, { opts: { user: a2 } });
  assert.match(lastEdit(j).description, /Ban Members/);
  assert.equal(a2.dms.length, 0, 'no DM when the action cannot run');
  assert.equal((await rowsOf(g2.id)).length, 0);

  // already banned
  const { guild: g3, aliceUser: a3 } = guildWithAlice(6);
  g3.bans.fetch = async () => ({ user: { id: ALICE } });
  const k = await run('ban', g3, { opts: { user: a3 } });
  assert.match(lastEdit(k).title, /Nothing To Do/);
  assert.match(lastEdit(k).description, /already banned/);
  assert.equal(g3.banCalls.length, 0);
});

test('DB: /ban refuses the server owner and an equal-rank member through the service', { skip: skipDb }, async () => {
  const guild = mkGuild(G(7));
  const owner = mkMember(guild, OWNER, { position: 1 });
  guild.members.cache.set(OWNER, owner);
  const i = await run('ban', guild, { opts: { user: owner.user } });
  assert.match(lastEdit(i).description, /server owner/);
  assert.equal(guild.banCalls.length, 0);

  const peer = mkMember(guild, ALICE, { position: 50 });
  guild.members.cache.set(ALICE, peer);
  const j = await run('kick', guild, { opts: { user: peer.user } });
  assert.match(lastEdit(j).description, /highest role is above or equal to yours/);
  assert.equal((await rowsOf(guild.id)).length, 0);
});

test('DB: /kick', { skip: skipDb }, async () => {
  const { guild, alice, aliceUser } = guildWithAlice(8);
  const kicked = [];
  alice.kick = async (reason) => { kicked.push(reason); };
  const i = await run('kick', guild, { opts: { user: aliceUser, reason: 'rude' } });

  assert.deepEqual(names(i), ['deferReply', 'deleteReply', 'followUp']);
  assert.equal(kicked.length, 1);
  assert.match(kicked[0], /rude \| by moddy/);
  const post = publicPost(i);
  assert.match(post.title, /Member Kicked/);
  assert.match(post.description, /\*\*Case:\*\* #1/);
  assert.equal(aliceUser.dms.length, 1);
  const [row] = await rowsOf(guild.id);
  assert.equal(row.type, 'KICK');

  // a kick that Discord refuses leaves no case
  const { guild: g2, alice: a2, aliceUser: u2 } = guildWithAlice(9);
  a2.kick = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  const j = await run('kick', g2, { opts: { user: u2 } });
  assert.match(lastEdit(j).title, /Missing Permissions/);
  assert.equal((await rowsOf(g2.id)).length, 0);
  assert.equal(u2.dmsDeleted, 1);
});

test('DB: /softban refuses an already-banned user and never lifts the ban (R029)', { skip: skipDb }, async () => {
  const { guild, aliceUser } = guildWithAlice(10);
  guild.bans.fetch = async () => ({ user: { id: ALICE } });
  const i = await run('softban', guild, { opts: { user: aliceUser } });

  assert.deepEqual(names(i), ['deferReply', 'editReply']);
  assert.match(lastEdit(i).title, /Nothing To Do/);
  assert.equal(lastEdit(i).description, svc.MSG.SOFTBAN_BANNED);
  assert.equal(guild.banCalls.length, 0);
  assert.equal(guild.unbanCalls.length, 0, 'the standing ban is never lifted');
  assert.equal((await rowsOf(guild.id)).length, 0);
});

test('DB: /softban bans, unbans and reports an unban that failed (R029)', { skip: skipDb }, async () => {
  const { guild, aliceUser } = guildWithAlice(11);
  const i = await run('softban', guild, { opts: { user: aliceUser, reason: 'clean up' } });
  assert.deepEqual(names(i), ['deferReply', 'deleteReply', 'followUp']);
  assert.equal(guild.banCalls[0][1].deleteMessageSeconds, 7 * 86_400, 'default 7 days');
  assert.equal(guild.unbanCalls.length, 1);
  const post = publicPost(i);
  assert.match(post.title, /User Softbanned$/);
  assert.match(post.description, /\*\*Messages deleted:\*\* 7 day\(s\)/);
  assert.equal((await rowsOf(guild.id))[0].type, 'SOFTBAN');

  const { guild: g2, aliceUser: u2 } = guildWithAlice(12);
  g2.bans.remove = async () => { throw Object.assign(new Error('boom'), { code: 50013 }); };
  const j = await run('softban', g2, { opts: { user: u2, delete_days: 2 } });
  assert.deepEqual(names(j), ['deferReply', 'deleteReply', 'followUp', 'followUp']);
  assert.equal(g2.banCalls[0][1].deleteMessageSeconds, 2 * 86_400);
  const stuck = publicPost(j);
  assert.match(stuck.title, /User Banned \(Softban Incomplete\)/);
  const note = jsonOf(callsOf(j, 'followUp')[1])[0];
  assert.match(note.description, /automatic unban failed/);
  assert.equal(callsOf(j, 'followUp')[1].ephemeral, true, 'the warning is only for the moderator');
  const [row] = await rowsOf(g2.id);
  assert.equal(row.type, 'BAN', 'recorded as an active ban so /unban can lift it');
  assert.equal(row.active, true);
});

test('DB: /timeout uses a native timeout, or the mute role beyond 28 days', { skip: skipDb }, async () => {
  const { guild, alice, aliceUser } = guildWithAlice(13);
  const timeouts = [];
  alice.timeout = async (ms, reason) => { timeouts.push([ms, reason]); };
  const i = await run('timeout', guild, { opts: { user: aliceUser, duration: '10m', reason: 'cool off' } });

  assert.deepEqual(names(i), ['deferReply', 'deleteReply', 'followUp']);
  assert.equal(timeouts[0][0], 600_000);
  assert.match(timeouts[0][1], /cool off \| by moddy/);
  const post = publicPost(i);
  assert.match(post.title, /Member Timed Out/);
  assert.match(post.description, /\*\*Duration:\*\* 10 minutes/);
  assert.match(post.description, /\*\*Ends:\*\* <t:\d+:R>/);
  assert.ok(post.description.indexOf('Ends') < post.description.indexOf('Reason'), 'order: User, Duration, Ends, Reason, Case');
  const [row] = await rowsOf(guild.id);
  assert.equal(row.type, 'TIMEOUT');
  assert.ok(row.expiresAt > new Date());
  assert.ok(await jobOf(`mod.expire:${row.id}`), 'the expiry job exists (R023)');

  // re-timing out supersedes the first row
  await run('timeout', guild, { opts: { user: aliceUser, duration: '1h' } });
  const active = await rowsOf(guild.id, { type: 'TIMEOUT', active: true });
  assert.equal(active.length, 1);
  assert.equal(await jobOf(`mod.expire:${row.id}`), null);

  // over 28 days without a mute role: explained, nothing runs
  const { guild: g2, alice: a2, aliceUser: u2 } = guildWithAlice(14);
  const called = [];
  a2.timeout = async () => { called.push(1); };
  const noRole = await run('timeout', g2, { opts: { user: u2, duration: '30d' } });
  assert.deepEqual(names(noRole), ['deferReply', 'editReply']);
  assert.match(lastEdit(noRole).title, /Invalid Input/);
  assert.equal(called.length, 0);

  // over 28 days with a mute role: a role mute
  const { guild: g3, alice: a3, aliceUser: u3 } = guildWithAlice(15);
  const role = mkRole('900000000000000011', 5);
  g3.roles.cache.set(role.id, role);
  await updateSettings(g3.id, { muteRoleId: role.id });
  const added = [];
  a3.roles.add = async (r) => { added.push(r.id); };
  const muted = await run('timeout', g3, { opts: { user: u3, duration: '30d' } });
  assert.deepEqual(names(muted).slice(0, 3), ['deferReply', 'deleteReply', 'followUp']);
  assert.match(publicPost(muted).title, /Member Muted/);
  assert.deepEqual(added, [role.id]);
  assert.equal((await rowsOf(g3.id))[0].type, 'MUTE');
  assert.match(jsonOf(callsOf(muted, 'followUp')[1])[0].description, /mute role/i, 'the service note is shown to the moderator');
});

test('DB: /unban lifts the ban, closes the records and cancels the expiry job (R023)', { skip: skipDb }, async () => {
  const guild = mkGuild(G(16));
  // a temp ban recorded earlier plus a stale duplicate
  await svc.recordInfraction({ guildId: guild.id, targetUserId: ALICE, moderatorId: MOD, type: 'BAN', expiresAt: new Date(Date.now() + DAY) });
  await svc.recordInfraction({ guildId: guild.id, targetUserId: ALICE, moderatorId: MOD, type: 'BAN' });
  await prisma.scheduledJob.create({ data: { guildId: guild.id, type: 'mod.unban', dedupeKey: `mod.unban:${guild.id}:${ALICE}`, runAt: new Date(Date.now() + DAY), payload: { userId: ALICE } } });

  // someone who is not banned
  const nope = await run('unban', guild, { opts: { user_id: '100000000000000099', reason: 'appeal' } });
  assert.match(lastEdit(nope).title, /Nothing To Do/);
  assert.equal(guild.unbanCalls.length, 0);

  // the ban exists on Discord's side; a mention is accepted as well as a bare ID
  guild.bans.fetch = async () => ({ user: mkUser(ALICE, { tag: 'alice#0' }), reason: 'old' });
  const i = await run('unban', guild, { opts: { user_id: `<@!${ALICE}>`, reason: 'appeal accepted' } });
  assert.deepEqual(names(i), ['deferReply', 'deleteReply', 'followUp']);
  assert.equal(guild.unbanCalls.length, 1);
  assert.equal(guild.unbanCalls[0][0], ALICE);
  assert.match(guild.unbanCalls[0][1], /appeal accepted \| by moddy/);
  const post = publicPost(i);
  assert.match(post.title, /User Unbanned/);
  assert.match(post.description, /<@100000000000000003> `alice#0`/);
  assert.match(post.description, /\*\*Case:\*\* #\d+/);
  assert.match(post.description, /\*\*Ban records closed:\*\* 2/);
  assert.equal((await rowsOf(guild.id, { type: 'BAN', active: true })).length, 0);
  assert.equal((await rowsOf(guild.id, { type: 'UNBAN' })).length, 1);
  assert.equal(await jobOf(`mod.unban:${guild.id}:${ALICE}`), null);

  const bad = await run('unban', guild, { opts: { user_id: '12345' } });
  assert.match(lastEdit(bad).title, /Invalid Input/);
});

test('DB: /unmute removes the timeout and closes stale rows (R023)', { skip: skipDb }, async () => {
  const { guild, alice, aliceUser } = guildWithAlice(17);
  await svc.recordInfraction({ guildId: guild.id, targetUserId: ALICE, moderatorId: MOD, type: 'TIMEOUT', expiresAt: new Date(Date.now() + DAY) });
  let disabled = true;
  alice.isCommunicationDisabled = () => disabled;
  const timeouts = [];
  alice.timeout = async (ms, reason) => { timeouts.push([ms, reason]); disabled = false; };

  const i = await run('unmute', guild, { opts: { user: aliceUser, reason: 'served' } });
  assert.deepEqual(names(i), ['deferReply', 'deleteReply', 'followUp']);
  assert.equal(timeouts[0][0], null);
  const post = publicPost(i);
  assert.match(post.title, /Member Unmuted/);
  assert.match(post.description, /\*\*Removed:\*\* timeout/);
  assert.match(post.description, /\*\*Records closed:\*\* 1/);
  assert.equal((await rowsOf(guild.id, { type: 'TIMEOUT', active: true })).length, 0);
  assert.equal((await rowsOf(guild.id, { type: 'UNMUTE' })).length, 1);

  // nothing to unmute
  const again = await run('unmute', guild, { opts: { user: aliceUser } });
  assert.match(lastEdit(again).title, /Nothing To Do/);

  // a member who left: only the records are closed, and the moderator is told
  const away = mkGuild(G(18));
  await svc.recordInfraction({ guildId: away.id, targetUserId: ALICE, moderatorId: MOD, type: 'MUTE' });
  const j = await run('unmute', away, { opts: { user: mkUser(ALICE) } });
  assert.match(publicPost(j).description, /\*\*Removed:\*\* nothing \(records only\)/);
  assert.match(jsonOf(callsOf(j, 'followUp')[1])[0].description, /only the records were closed/);
});

test('DB: /warn counts active warnings and reports the escalation that fired', { skip: skipDb }, async () => {
  const { guild, alice, aliceUser } = guildWithAlice(19);
  await updateSettings(guild.id, {
    infractionEscalation: [{ id: 1, warnCount: 2, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom: null }],
  });
  const kicked = [];
  alice.kick = async (reason) => { kicked.push(reason); };

  const one = await run('warn', guild, { opts: { user: aliceUser, reason: 'first', proof: 'https://example.com/p' } });
  assert.deepEqual(names(one), ['deferReply', 'deleteReply', 'followUp']);
  const post1 = publicPost(one);
  assert.match(post1.title, /Warning Issued/);
  assert.match(post1.description, /\*\*Active warnings:\*\* 1/);
  assert.match(post1.description, /\*\*Case:\*\* #1/);
  assert.doesNotMatch(post1.description, /Escalation/);
  assert.equal(kicked.length, 0);

  const two = await run('warn', guild, { opts: { user: aliceUser, reason: 'second' } });
  const post2 = publicPost(two);
  assert.match(post2.description, /\*\*Active warnings:\*\* 2/);
  assert.match(post2.description, /\*\*Escalation T1:\*\* Kick applied \(case #3\)/);
  assert.equal(kicked.length, 1);

  // a pardon resets the active count
  await prisma.infraction.updateMany({ where: { guildId: guild.id, type: 'WARN' }, data: { active: false } });
  const three = await run('warn', guild, { opts: { user: aliceUser, reason: 'third' } });
  assert.match(publicPost(three).description, /\*\*Active warnings:\*\* 1/);

  // a user who is not in the server cannot be warned
  const ghost = await run('warn', guild, { opts: { user: mkUser('100000000000000077'), reason: 'x' } });
  assert.match(lastEdit(ghost).title, /Not Found/);
});

test('DB: visibility PRIVATE and ANONYMOUS are honoured by the action commands', { skip: skipDb }, async () => {
  const priv = guildWithAlice(20);
  await updateSettings(priv.guild.id, { modVisibility: 'PRIVATE' });
  const i = await run('kick', priv.guild, { opts: { user: priv.aliceUser } });
  assert.deepEqual(names(i), ['deferReply', 'editReply'], 'private: the moderator alone sees the result');
  assert.match(lastEdit(i).title, /Member Kicked/);

  const anon = guildWithAlice(21);
  await updateSettings(anon.guild.id, { modVisibility: 'ANONYMOUS' });
  const channel = fakeChannel();
  const j = await run('kick', anon.guild, { opts: { user: anon.aliceUser }, channel });
  assert.deepEqual(names(j), ['deferReply', 'editReply']);
  assert.equal(channel.sent.length, 1, 'the public notice is a channel message, not an interaction reply');
  assert.match(jsonOf(channel.sent[0])[0].title, /Member Kicked/);
  assert.doesNotMatch(JSON.stringify(channel.sent[0]), /moddy/);
});

// ── /case ─────────────────────────────────────────────────────────────────────

test('DB: /case view survives rows with very long text (R095) and shows the case', { skip: skipDb }, async () => {
  const guild = mkGuild(G(22));
  const base = await svc.recordInfraction({ guildId: guild.id, targetUserId: ALICE, moderatorId: MOD, type: 'BAN', expiresAt: new Date(Date.now() + DAY), reason: 'short' });
  // rows written by older versions are not bounded
  await prisma.infraction.update({ where: { id: base.id }, data: { reason: 'r'.repeat(3000), proofUrl: `https://example.com/${'p'.repeat(2900)}` } });

  const i = await run('case', guild, { sub: 'view', opts: { id: base.caseId } });
  assert.deepEqual(names(i), ['deferReply', 'deleteReply', 'followUp']);
  const payload = callsOf(i, 'followUp')[0];
  assert.deepEqual(payload.allowedMentions, { parse: [] });
  const embed = jsonOf(payload)[0];
  assert.match(embed.title, /Case #1 \| Temporary ban/);
  const byName = Object.fromEntries(embed.fields.map((f) => [f.name, f.value]));
  assert.ok(byName.Reason.length <= 1024);
  assert.ok(byName.Proof.length <= 1024);
  assert.equal(byName.Active, 'Yes');
  assert.match(byName.Target, /<@100000000000000003> `user003`/);
  assert.match(byName.Moderator, /<@100000000000000005>/);
  assert.match(byName.Created, /^<t:\d+:F>$/);
  assert.match(byName.Expires, /<t:\d+:R>/);
  assert.equal(embed.footer.text, `User ID: ${ALICE}`);
  assert.ok(JSON.stringify(embed).length < 6000);

  const missing = await run('case', guild, { sub: 'view', opts: { id: 99 } });
  assert.deepEqual(names(missing), ['deferReply', 'editReply']);
  assert.match(lastEdit(missing).title, /Not Found/);
  assert.match(lastEdit(missing).description, /Case #99/);

  // PRIVATE visibility: the lookup is an ephemeral edit
  await updateSettings(guild.id, { modVisibility: 'PRIVATE' });
  const priv = await run('case', guild, { sub: 'view', opts: { id: base.caseId } });
  assert.deepEqual(names(priv), ['deferReply', 'editReply']);
});

test('DB: /case reason and proof update the row, clamp, and post a mod-log note', { skip: skipDb }, async () => {
  const guild = mkGuild(G(23));
  const logChannel = fakeChannel();
  guild.channels.cache.set(logChannel.id, logChannel);
  await updateSettings(guild.id, { modLogChannelId: logChannel.id });
  const inf = await svc.recordInfraction({ guildId: guild.id, targetUserId: ALICE, moderatorId: MOD, type: 'WARN', reason: 'old' });

  const i = await run('case', guild, { sub: 'reason', opts: { id: inf.caseId, reason: '  new reason\nsecond line  ' } });
  assert.deepEqual(names(i), ['deferReply', 'deleteReply', 'followUp']);
  const post = publicPost(i);
  assert.match(post.title, /Case Updated/);
  assert.match(post.description, /Case #1 reason is now/);
  assert.equal((await prisma.infraction.findUnique({ where: { id: inf.id } })).reason, 'new reason\nsecond line');
  assert.equal(logChannel.sent.length, 1, 'the edit is noted in the case log');
  const note = jsonOf(logChannel.sent[0])[0];
  assert.match(note.title, /Case #1 edited/);
  assert.match(note.description, /<@100000000000000005>/);
  assert.match(note.description, /new reason/);

  const long = await run('case', guild, { sub: 'reason', opts: { id: inf.caseId, reason: 'z'.repeat(1500) } });
  assert.ok(long.calls.length);
  assert.ok((await prisma.infraction.findUnique({ where: { id: inf.id } })).reason.length <= 1000);

  const proof = await run('case', guild, { sub: 'proof', opts: { id: inf.caseId, url: 'https://example.com/evidence.png' } });
  assert.match(publicPost(proof).description, /proof link updated/);
  assert.equal((await prisma.infraction.findUnique({ where: { id: inf.id } })).proofUrl, 'https://example.com/evidence.png');
  assert.equal(logChannel.sent.length, 3);

  // an unknown case: Prisma P2025 becomes a Not Found reply
  const gone = await run('case', guild, { sub: 'reason', opts: { id: 4040, reason: 'x' } });
  assert.deepEqual(names(gone), ['deferReply', 'editReply']);
  assert.match(lastEdit(gone).title, /Not Found/);
  const goneProof = await run('case', guild, { sub: 'proof', opts: { id: 4040, url: 'https://example.com/x' } });
  assert.match(lastEdit(goneProof).title, /Not Found/);
});

// ── /case history and /case clear-history ──────────────────────────────────────────────────────────────────

async function seedCases(guildId, n, { type = 'WARN', userId = ALICE } = {}) {
  for (let k = 0; k < n; k++) {
    await svc.recordInfraction({ guildId, targetUserId: userId, moderatorId: MOD, type, reason: `case ${k + 1}\nwith a second line` });
  }
}

test('DB: /case history posts every page through followUp after deleteReply (R026, R027, R028, R055)', { skip: skipDb }, async () => {
  const guild = mkGuild(G(24));
  await seedCases(guild.id, 7);
  await svc.recordInfraction({ guildId: guild.id, targetUserId: ALICE, moderatorId: MOD, type: 'BAN', expiresAt: new Date(Date.now() + DAY) });

  const i = await run('case', guild, { sub: 'history', opts: { user: mkUser(ALICE) } });
  // the call order is the regression: no editReply may follow deleteReply
  assert.deepEqual(names(i), ['deferReply', 'deleteReply', 'followUp']);
  const payload = callsOf(i, 'followUp')[0];
  assert.equal(payload.embeds.length, 1);
  assert.equal(payload.components.length, 1, '8 cases make two pages, so the paginator buttons are attached');
  const page = jsonOf(payload)[0];
  assert.match(page.title, /📋 Infractions \| user003/);
  assert.match(page.description, /⚠️ 7× Warn/);
  assert.match(page.description, /⏳|🔨/);
  assert.match(page.description, /\*\*Active warnings:\*\* 7/);
  assert.equal(page.fields.length, 5);
  assert.match(page.fields[0].name, /Case #8 \| Temporary ban/);
  assert.match(page.fields[1].name, /Case #7 \| Warn 🔴 active/);
  assert.match(page.fields[0].value, /<t:\d+:D> by <@100000000000000005> · ends <t:\d+:R>/);
  assert.match(page.fields[1].value, /\n> case 7 with a second line$/, 'a multi-line reason stays on one quoted line');
  assert.match(page.footer.text, /^8 record\(s\) • Page 1\/2$/, 'the paginator appends its indicator to the footer');
  assert.ok(page.thumbnail?.url);
  assert.equal(i.collectors.length, 1, 'the paginator collector is attached to the follow-up message');

  // a single page still goes out as a plain follow-up, without buttons
  const small = mkGuild(G(25));
  await seedCases(small.id, 3);
  const j = await run('case', small, { sub: 'history', opts: { user: mkUser(ALICE) } });
  assert.deepEqual(names(j), ['deferReply', 'deleteReply', 'followUp']);
  assert.equal(callsOf(j, 'followUp')[0].components.length, 0);
  assert.equal(jsonOf(callsOf(j, 'followUp')[0])[0].fields.length, 3);
});

test('DB: /case history respects PRIVATE and ANONYMOUS visibility with an ephemeral edit', { skip: skipDb }, async () => {
  for (const [n, mode] of [[26, 'PRIVATE'], [27, 'ANONYMOUS']]) {
    const guild = mkGuild(G(n));
    await updateSettings(guild.id, { modVisibility: mode });
    await seedCases(guild.id, 6);
    const i = await run('case', guild, { sub: 'history', opts: { user: mkUser(ALICE) } });
    assert.deepEqual(names(i), ['deferReply', 'editReply'], mode);
    assert.equal(callsOf(i, 'editReply')[0].components.length, 1, `${mode}: two pages`);
    assert.equal(i.ephemeral, true);
  }
});

test('DB: /case history filters, and only explains a live ban when the member has no records (R246)', { skip: skipDb }, async () => {
  const guild = mkGuild(G(28));
  const fetchOpts = [];
  guild.bans.fetch = async (o) => { fetchOpts.push(o); return { user: { id: ALICE }, reason: 'raid' }; };
  await svc.recordInfraction({ guildId: guild.id, targetUserId: ALICE, moderatorId: MOD, type: 'BAN' });
  await svc.recordInfraction({ guildId: guild.id, targetUserId: ALICE, moderatorId: MOD, type: 'WARN', active: false });

  // the member has recorded cases (a ban and a pardoned warning); a filter that matches nothing
  const filtered = await run('case', guild, { sub: 'history', opts: { user: mkUser(ALICE), type: 'KICK' } });
  assert.deepEqual(names(filtered), ['deferReply', 'deleteReply', 'followUp']);
  const empty = publicPost(filtered);
  assert.match(empty.title, /No Records/);
  assert.match(empty.description, /\(filter: Kicks\)/);
  assert.doesNotMatch(empty.description, /currently banned/, 'a recorded ban is never called a pre-bot ban');
  assert.equal(fetchOpts.length, 0, 'the live lookup is not even made');

  const active = await run('case', guild, { sub: 'history', opts: { user: mkUser(ALICE), type: 'active' } });
  assert.match(publicPost(active).fields[0].name, /Case #1 \| Ban 🔴 active/);
  const warns = await run('case', guild, { sub: 'history', opts: { user: mkUser(ALICE), type: 'WARN' } });
  assert.equal(publicPost(warns).fields.length, 1);
  assert.match(publicPost(warns).fields[0].name, /Warn$/);

  // no records at all, but Discord shows a ban: now the hint is true
  const bare = await run('case', guild, { sub: 'history', opts: { user: mkUser('100000000000000066') } });
  const hint = publicPost(bare);
  assert.match(hint.title, /No Records/);
  assert.match(hint.description, /currently banned/);
  assert.match(hint.description, /raid/);
  assert.deepEqual(fetchOpts[0], { user: '100000000000000066', force: true, cache: false });

  // ... or a timeout that is not ours
  const guild2 = mkGuild(G(29));
  const ghost = mkMember(guild2, ALICE, { isCommunicationDisabled: () => true, communicationDisabledUntilTimestamp: Date.now() + 3_600_000 });
  guild2.members.cache.set(ALICE, ghost);
  const t = await run('case', guild2, { sub: 'history', opts: { user: mkUser(ALICE) } });
  assert.match(publicPost(t).description, /currently timed out/);
  assert.match(publicPost(t).description, /<t:\d+:R>/);

  // no records and nothing live: a plain answer
  const guild3 = mkGuild(G(30));
  const plain = await run('case', guild3, { sub: 'history', opts: { user: mkUser(ALICE) } });
  assert.equal(publicPost(plain).description, `No moderation history for <@${ALICE}>.`);
});

test('DB: /case history tells the moderator when the pages cannot be posted', { skip: skipDb }, async () => {
  const guild = mkGuild(G(24));
  const i = mkInteraction(guild, { sub: 'history', opts: { user: mkUser(ALICE) } });
  const original = i.followUp;
  let first = true;
  i.followUp = async (p) => {
    if (first) { first = false; throw Object.assign(new Error('Missing Access'), { code: 50001 }); }
    return original(p);
  };
  await COMMANDS.case.execute(i, guild.client);
  const err = callsOf(i, 'followUp').at(-1);
  assert.equal(err.ephemeral, true);
  assert.match(jsonOf(err)[0].title, /Action Failed/);
});

test('DB: /case clear-history pardons active warnings after a confirmation, as an administrator action', { skip: skipDb }, async () => {
  const guild = mkGuild(G(24)); // shares its rows with the first /case history test, so use a user of its own
  const bob = '100000000000000088';
  const bobUser = mkUser(bob);
  const nothing = await run('case', guild, { sub: 'clear-history', opts: { user: bobUser } });
  assert.deepEqual(names(nothing), ['deferReply', 'editReply']);
  assert.match(lastEdit(nothing).title, /Nothing To Clear/);
  assert.equal((await rowsOf(guild.id, { targetUserId: bob })).length, 0);

  await seedCases(guild.id, 3, { userId: bob });
  await svc.recordInfraction({ guildId: guild.id, targetUserId: bob, moderatorId: MOD, type: 'WARN', active: false });

  // cancelled: nothing changes and nothing is posted
  const cancelled = await run('case', guild, { sub: 'clear-history', opts: { user: bobUser }, confirm: 'no' });
  assert.equal(callsOf(cancelled, 'editReply').length, 1, 'only the confirmation dialog');
  assert.match(jsonOf(callsOf(cancelled, 'editReply')[0])[0].description, /\*\*3\*\* active warning/);
  assert.ok(!names(cancelled).includes('followUp'));
  assert.equal((await rowsOf(guild.id, { targetUserId: bob, type: 'WARN', active: true })).length, 3);
  assert.equal((await rowsOf(guild.id, { targetUserId: bob, type: 'NOTE' })).length, 0);

  // confirmed
  const done = await run('case', guild, { sub: 'clear-history', opts: { user: bobUser, reason: 'fresh start' }, confirm: 'yes' });
  assert.deepEqual(names(done).filter((n) => n !== 'pickedUpdate'), ['deferReply', 'editReply', 'deleteReply', 'followUp']);
  const post = publicPost(done);
  assert.match(post.title, /Warnings Cleared/);
  assert.match(post.description, /\*\*Warnings cleared:\*\* 3/);
  assert.match(post.description, /\*\*Reason:\*\* fresh start/);
  assert.match(post.description, /\*\*Case:\*\* #\d+/);
  assert.equal((await rowsOf(guild.id, { targetUserId: bob, type: 'WARN', active: true })).length, 0);
  assert.equal((await rowsOf(guild.id, { targetUserId: bob, type: 'WARN' })).length, 4, 'the cases stay in the history');
  const [note] = await rowsOf(guild.id, { targetUserId: bob, type: 'NOTE' });
  assert.equal(note.reason, '[Pardon] Cleared 3 warning(s): fresh start');
  assert.equal(note.active, false);
  assert.equal(note.moderatorId, MOD);
  assert.equal(bobUser.dms.length, 0, 'the pardon note sends no DM');
});
