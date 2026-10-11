// Dashboard moderation and member lists: the `cases`, `levels` and `balances` collections
// (src/dashboard/collections/{cases,levels,balances}.js and their .store.js), the bot-side tasks they queue
// (src/services/dashboardTasks/{moderation,members}.js) and the 'dashboard' moderation source (moderationService.js).
// Offline by default: descriptors, request validation, pure item builders, the tasks with fake discord.js objects and
// stubbed services. With TEST_DATABASE_URL (a migrated database) the stores run against it, the moderation and XP tasks
// end to end (fake guild, real services):
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/dashboard-moderation.test.js
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is only used with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const { COIN_MAX, XP_MAX } = await import('../src/config/constants.js');
const { MODULES } = await import('../src/config/modules.js');
const { FieldError } = await import('../src/dashboard/schema.js');
const { RecordError } = await import('../src/dashboard/collections/errors.js');
const { COLLECTIONS, lintCollections, publicCollection, validateRecordInput } = await import('../src/dashboard/collections/index.js');
const cases = await import('../src/dashboard/collections/cases.js');
const levels = await import('../src/dashboard/collections/levels.js');
const balances = await import('../src/dashboard/collections/balances.js');
const svc = await import('../src/services/moderationService.js');
const xpService = await import('../src/services/xpService.js');
const modTasks = await import('../src/services/dashboardTasks/moderation.js');
const memberTasks = await import('../src/services/dashboardTasks/members.js');
const { TASKS } = await import('../src/services/dashboardTasks/index.js');
const { TaskError } = await import('../src/services/dashboardTasks/shared.js');
const xpCommand = await import('../src/commands/leveling/xp.js');
const economyAdmin = await import('../src/commands/admin/economy-admin.js');
const banCommand = await import('../src/commands/moderation/ban.js');
const caseCommand = await import('../src/commands/moderation/case.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

after(async () => {
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const KEYS = ['cases', 'levels', 'balances'];
const DAY = 86_400_000;

const BOT = '100000000000000001';
const OWNER = '100000000000000002';
const ALICE = '100000000000000003';
const BOB = '100000000000000004';
const CAROL = '100000000000000005';
const ROBOT = '100000000000000006';

// ── Helpers ──────────────────────────────────────────────────────────────────

const create = { mode: 'create' };
const update = (current = null) => ({ mode: 'update', current });
const action = (actionId, current = null) => ({ mode: 'action', actionId, current });
const accept = (key, data, opts = create) => validateRecordInput(key, data, opts).data;

function refuse(key, data, opts, { path, message } = {}) {
  assert.throws(() => validateRecordInput(key, data, opts), (err) => {
    assert.ok(err instanceof FieldError, `expected a FieldError, got ${err?.name}: ${err?.message}`);
    if (path !== undefined) assert.equal(err.path, path, `path of "${err.message}"`);
    if (message !== undefined) assert.match(err.message, message);
    return true;
  });
}

const bits = (...names) => new PermissionsBitField(names.map((n) => PermissionFlagsBits[n]));

function mkMember(guild, id, { position = 1, perms = [], roles = [], user = {}, ...flags } = {}) {
  return {
    id,
    guild,
    displayName: `member${id.slice(-2)}`,
    user: {
      id, username: `user${id.slice(-2)}`, globalName: null, tag: `user${id.slice(-2)}`, bot: false,
      send: async () => ({ delete: async () => {} }), displayAvatarURL: () => 'https://cdn.example/a.png', ...user,
    },
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
}

/** A discord.js-like guild: members in `members.cache`, users the client can fetch, an empty ban list. */
function mkGuild(id = '930000000000000001') {
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
  guild.roles = { cache: new Map() };
  guild.channels = { cache: new Map(), fetch: async () => null };
  const users = new Map();
  guild.client = {
    user: { id: BOT, username: 'Bot', tag: 'Bot#0' },
    users: {
      cache: users,
      fetch: async (uid) => users.get(uid) ?? { id: uid, username: `u${uid.slice(-3)}`, globalName: null, bot: uid === ROBOT, send: async () => ({ delete: async () => {} }) },
    },
  };
  guild.bans = {
    banned: new Set(),
    fetch: async ({ user }) => {
      if (guild.bans.banned.has(user)) return { user: { id: user, username: `u${user.slice(-3)}` }, reason: null };
      throw Object.assign(new Error('Unknown Ban'), { code: 10026 });
    },
    create: async (uid) => { guild.bans.banned.add(uid); },
    remove: async (uid) => { guild.bans.banned.delete(uid); },
  };
  const me = mkMember(guild, BOT, { position: 100, perms: ['BanMembers', 'KickMembers', 'ModerateMembers', 'ManageRoles'] });
  guild.members.me = me;
  guild.members.cache.set(BOT, me);
  return guild;
}

const add = (guild, member) => {
  guild.members.cache.set(member.id, member);
  return member;
};

/** A task's outcome as src/dashboard/tasks.js reports it. */
async function runLike(task, ctx) {
  try {
    const r = await task.run(ctx);
    return { status: 'done', message: r.message, data: r.data ?? null };
  } catch (err) {
    if (err instanceof TaskError) return { status: 'failed', message: err.message, data: null };
    throw err;
  }
}

async function taskRefusal(promise, re) {
  await assert.rejects(promise, (err) => {
    assert.ok(err instanceof TaskError, `expected a TaskError, got ${err?.stack ?? err}`);
    if (re) assert.match(err.message, re);
    return true;
  });
}

// ── Descriptors ──────────────────────────────────────────────────────────────

describe('descriptors', () => {
  test('lintCollections has no problems for these collections', () => {
    const mine = lintCollections().filter((p) => KEYS.some((k) => p.startsWith(`${k}.`) || p.startsWith(`${k}(`) || p.startsWith(`${k}:`) || p.startsWith(`${k}!`)));
    assert.deepEqual(mine, []);
  });

  test('registered, real, searchable member lists without a cap', () => {
    const modules = { cases: 'moderation', levels: 'leveling', balances: 'economy' };
    for (const key of KEYS) {
      const c = COLLECTIONS[key];
      assert.equal(c.key, key);
      assert.ok(!c.placeholder, `${key} is still a placeholder`);
      assert.equal(c.module, modules[key]);
      assert.ok(Object.hasOwn(MODULES, c.module));
      assert.equal(c.limit, null);
      assert.equal(c.searchable, true);
      assert.equal(c.canDelete, false, `${key}: nothing the commands do deletes a row`);
      const pub = publicCollection(c);
      assert.equal(pub.searchable, true);
      assert.ok(pub.searchPlaceholder);
      assert.equal(pub.avatarField, 'avatarUrl');
      const avatar = c.readOnly.find((d) => d.field === c.avatarField);
      assert.equal(avatar?.type, 'url', `${key}: the avatar is a read-only url field`);
      assert.ok([...c.fields, ...c.readOnly].some((d) => d.field === c.titleField));
      assert.ok([...c.fields, ...c.readOnly].some((d) => d.field === c.subtitleField));
      for (const f of c.fields) assert.ok(f.label && f.description, `${key}.${f.field} has a label and a description`);
      for (const a of c.actions) assert.ok(a.label && a.description, `${key}!${a.id}`);
      assert.equal(JSON.stringify(pub).includes('function'), false);
    }
    assert.notEqual(COLLECTIONS.cases.canEdit, false, '/case edits a reason and a proof link');
    assert.equal(COLLECTIONS.levels.canEdit, false);
    assert.equal(COLLECTIONS.balances.canEdit, false);
  });

  test('cases: showWhen fields point at the action and its values', () => {
    const c = COLLECTIONS.cases;
    const values = new Set(c.fields.find((f) => f.field === 'action').options.map((o) => o.value));
    for (const f of c.fields.filter((d) => d.showWhen)) {
      assert.equal(f.showWhen.field, 'action', f.field);
      for (const v of f.showWhen.in) assert.ok(values.has(v), `${f.field}: ${v}`);
    }
    assert.deepEqual([...values], modTasks.DASHBOARD_MOD_ACTIONS, 'the form offers exactly what mod.action does');
    assert.deepEqual(c.actions.map((a) => a.id), ['clearWarnings']);
    assert.equal(c.actions[0].danger, true);
  });

  test('limits mirror the services and the slash commands', () => {
    assert.equal(cases.COMMAND_REASON_MAX, modTasks.COMMAND_REASON_MAX);
    assert.equal(cases.REASON_MAX, svc.REASON_MAX);
    assert.equal(cases.TIMED_ACTION_MIN_MS, svc.TIMED_ACTION_MIN_MS);
    assert.equal(cases.TIMED_ACTION_MAX_MS, svc.TIMED_ACTION_MAX_MS);
    assert.equal(cases.DELETE_DAYS_MAX * 86_400, svc.DELETE_MESSAGES_MAX_S);
    assert.equal(levels.XP_STEP_MAX, memberTasks.XP_STEP_MAX);

    const opt = (json, path) => path.reduce((node, name) => node.options.find((o) => o.name === name), json);
    const ban = banCommand.data.toJSON();
    assert.equal(opt(ban, ['reason']).max_length, cases.COMMAND_REASON_MAX);
    assert.equal(opt(ban, ['delete_days']).max_value, cases.DELETE_DAYS_MAX);
    assert.equal(opt(caseCommand.data.toJSON(), ['reason', 'reason']).max_length, cases.REASON_MAX);
    const xp = xpCommand.data.toJSON();
    assert.equal(opt(xp, ['give', 'amount']).max_value, levels.XP_STEP_MAX);
    assert.equal(opt(xp, ['remove', 'amount']).max_value, levels.XP_STEP_MAX);
    assert.equal(opt(xp, ['set', 'amount']).max_value, XP_MAX);
    const eco = economyAdmin.data.toJSON();
    assert.equal(opt(eco, ['coins', 'give', 'amount']).max_value, COIN_MAX);
    assert.equal(opt(eco, ['coins', 'set', 'amount']).min_value, 0);
    for (const key of ['levels', 'balances']) assert.equal(COLLECTIONS[key].actions.find((a) => a.id === 'reset').danger, true);
  });
});

// ── Request validation ───────────────────────────────────────────────────────

describe('cases: validation', () => {
  const base = { action: 'KICK', userId: ALICE };

  test('a member is required', () => {
    refuse('cases', { action: 'KICK' }, create, { path: 'userId' });
    refuse('cases', { action: 'KICK', userId: null }, create, { path: 'userId' });
    refuse('cases', { action: 'KICK', userId: 'alice' }, create, { path: 'userId' });
    assert.equal(accept('cases', base).userId, ALICE);
  });

  test('the action must be one of the form', () => {
    refuse('cases', { ...base, action: 'MUTE' }, create, { path: 'action' });
    refuse('cases', { ...base, action: 'NOTE' }, create, { path: 'action' });
  });

  test('a warning needs a reason; new reasons are at most 512 characters like the commands', () => {
    refuse('cases', { action: 'WARN', userId: ALICE }, create, { path: 'reason', message: /needs a reason/ });
    refuse('cases', { action: 'WARN', userId: ALICE, reason: '   ' }, create, { path: 'reason' });
    assert.equal(accept('cases', { action: 'WARN', userId: ALICE, reason: ' spam ' }).reason, 'spam');
    refuse('cases', { ...base, reason: 'x'.repeat(513) }, create, { path: 'reason', message: /512/ });
    refuse('cases', { ...base, reason: 'two\nlines' }, create, { path: 'reason' });
    assert.equal(accept('cases', { ...base, reason: '' }).reason, null);
  });

  test('timeouts need a readable duration of 1 minute to 1 year', () => {
    const t = { action: 'TIMEOUT', userId: ALICE };
    refuse('cases', t, create, { path: 'duration', message: /needs a duration/ });
    refuse('cases', { ...t, duration: 'soon' }, create, { path: 'duration', message: /Couldn't read/ });
    refuse('cases', { ...t, duration: '30s' }, create, { path: 'duration', message: /at least 1 minute/ });
    refuse('cases', { ...t, duration: '2y' }, create, { path: 'duration', message: /at most 1 year/ });
    refuse('cases', { ...t, duration: 'x'.repeat(33) }, create, { path: 'duration' });
    assert.equal(accept('cases', { ...t, duration: '2h' }).duration, '2h');
    assert.ok(accept('cases', { ...t, duration: '60d' }), 'over 28 days: the bot uses the mute role or refuses');
  });

  test('a ban duration is optional (permanent) and at most 1 year', () => {
    assert.equal(accept('cases', { action: 'BAN', userId: ALICE }).duration, null);
    refuse('cases', { action: 'BAN', userId: ALICE, duration: '400d' }, create, { path: 'duration', message: /Temporary bans/ });
    refuse('cases', { action: 'BAN', userId: ALICE, duration: 'later' }, create, { path: 'duration' });
    refuse('cases', { action: 'BAN', userId: ALICE, deleteDays: 8 }, create, { path: 'deleteDays' });
    refuse('cases', { action: 'SOFTBAN', userId: ALICE, softbanDeleteDays: 0 }, create, { path: 'softbanDeleteDays' });
  });

  test('fields of other actions are ignored (hidden by showWhen), not refused', () => {
    assert.ok(accept('cases', { ...base, duration: 'garbage' }), 'a kick ignores the duration');
    const payload = cases.actionPayload(accept('cases', { action: 'UNBAN', userId: ALICE, duration: '1d', deleteDays: 3, proofUrl: 'https://x.test/p' }));
    assert.deepEqual(payload, { action: 'UNBAN', userId: ALICE, reason: null, durationMs: null, deleteMessageSeconds: null, proofUrl: null });
  });

  test('actionPayload builds what mod.action takes', () => {
    const p = (data) => cases.actionPayload(accept('cases', data));
    assert.deepEqual(p({ action: 'BAN', userId: ALICE, duration: '7d', deleteDays: 2, reason: 'raid', proofUrl: 'https://x.test/p' }), {
      action: 'BAN', userId: ALICE, reason: 'raid', durationMs: 7 * DAY, deleteMessageSeconds: 2 * 86_400, proofUrl: 'https://x.test/p',
    });
    assert.equal(p({ action: 'BAN', userId: ALICE }).deleteMessageSeconds, 0);
    assert.equal(p({ action: 'SOFTBAN', userId: ALICE }).deleteMessageSeconds, 7 * 86_400, '/softban deletes 7 days by default');
    assert.equal(p({ action: 'TIMEOUT', userId: ALICE, duration: '2h30m' }).durationMs, 9_000_000);
    assert.equal(p({ action: 'KICK', userId: ALICE }).deleteMessageSeconds, null);
    for (const data of [{ action: 'WARN', userId: ALICE, reason: 'x' }, { action: 'KICK', userId: ALICE }]) {
      assert.doesNotThrow(() => modTasks.readActionPayload(p(data)), 'the task accepts what the store sends');
    }
  });

  test('editing: reason (up to 1000, not empty) and proof link (replaced, never removed); the rest is fixed', () => {
    const current = { proofUrl: 'https://x.test/old', reason: 'old' };
    assert.equal(accept('cases', { reason: 'y'.repeat(1000) }, update(current)).reason.length, 1000);
    refuse('cases', { reason: 'y'.repeat(1001) }, update(current), { path: 'reason' });
    refuse('cases', { reason: '' }, update(current), { path: 'reason', message: /can't be empty/ });
    refuse('cases', { proofUrl: null }, update(current), { path: 'proofUrl', message: /not removed/ });
    assert.equal(accept('cases', { proofUrl: null }, update({ proofUrl: null })).proofUrl, null);
    refuse('cases', { proofUrl: 'ftp://x.test' }, update(current), { path: 'proofUrl' });
    for (const field of ['action', 'userId', 'duration', 'deleteDays']) {
      refuse('cases', { [field]: null }, update(current), { path: field, message: /cannot be changed/ });
    }
    refuse('cases', { caseNumber: 3 }, update(current), { path: 'caseNumber' });
  });

  test('clear warnings takes an optional reason of at most 512 characters', () => {
    assert.deepEqual(accept('cases', {}, action('clearWarnings')), { clearReason: null });
    refuse('cases', { clearReason: 'x'.repeat(513) }, action('clearWarnings'), { path: 'clearReason' });
  });
});

describe('levels and balances: validation', () => {
  test('levels: give 1 to 1,000,000 XP to a member; set 0 to XP_MAX; reset takes nothing', () => {
    assert.deepEqual(accept('levels', { userId: ALICE, amount: 100 }), { userId: ALICE, amount: 100 });
    refuse('levels', { userId: ALICE }, create, { path: 'amount' });
    refuse('levels', { amount: 5 }, create, { path: 'userId' });
    refuse('levels', { userId: ALICE, amount: 0 }, create, { path: 'amount' });
    refuse('levels', { userId: ALICE, amount: 1_000_001 }, create, { path: 'amount' });
    refuse('levels', { userId: ALICE, amount: 1.5 }, create, { path: 'amount' });
    refuse('levels', { amount: 0 }, action('give'), { path: 'amount' });
    refuse('levels', { amount: 1_000_001 }, action('take'), { path: 'amount' });
    assert.equal(accept('levels', { xp: 0 }, action('set')).xp, 0);
    assert.equal(accept('levels', { xp: XP_MAX }, action('set')).xp, XP_MAX);
    refuse('levels', { xp: XP_MAX + 1 }, action('set'), { path: 'xp' });
    refuse('levels', { amount: 1 }, action('reset'), { path: 'amount' });
    assert.deepEqual(accept('levels', {}, action('reset')), {});
    refuse('levels', { amount: 1 }, update(), { path: 'amount' });
  });

  test('balances: give 1 to COIN_MAX; set 0 to COIN_MAX; reset coins, items or both', () => {
    assert.deepEqual(accept('balances', { userId: ALICE, amount: 5 }), { userId: ALICE, amount: 5 });
    refuse('balances', { userId: ALICE, amount: 0 }, create, { path: 'amount' });
    refuse('balances', { userId: ALICE, amount: COIN_MAX + 1 }, create, { path: 'amount' });
    refuse('balances', { amount: 0 }, action('take'), { path: 'amount' });
    assert.equal(accept('balances', { balance: 0 }, action('set')).balance, 0);
    refuse('balances', { balance: -1 }, action('set'), { path: 'balance' });
    refuse('balances', {}, action('reset'), { path: 'what' });
    assert.equal(accept('balances', { what: 'coins' }, action('reset')).what, 'coins');
    assert.equal(accept('balances', { what: 'all' }, action('reset')).what, 'all');
    refuse('balances', { what: 'shop' }, action('reset'), { path: 'what' });
  });
});

// ── Pure helpers ─────────────────────────────────────────────────────────────

describe('items and helpers', () => {
  test('levelProgress matches xpService', () => {
    for (const xp of [0, 1, 99, 100, 155, 255, 1_000, 12_345, 1_000_000, XP_MAX]) {
      const p = levels.levelProgress(xp);
      assert.equal(p.level, xpService.levelFromXp(xp), `level of ${xp}`);
      assert.equal(p.into, xpService.xpIntoLevel(xp), `into of ${xp}`);
    }
  });

  test('rank cursors', () => {
    assert.equal(levels.rankCursor(1500, ALICE), `1500_${ALICE}`);
    assert.deepEqual(levels.readRankCursor(`1500_${ALICE}`), { value: 1500, userId: ALICE });
    for (const bad of ['', 'x', `-1_${ALICE}`, '1500_12', `1500_${ALICE}_x`, null]) assert.equal(levels.readRankCursor(bad), null, String(bad));
    assert.deepEqual(levels.afterRankCursor('xp', `10_${ALICE}`), { OR: [{ xp: { lt: 10 } }, { xp: 10, userId: { gt: ALICE } }] });
    assert.deepEqual(levels.afterRankCursor('xp', 'nope'), {});
  });

  test('member search: ids pass through, names go to Discord, failures say so', async () => {
    assert.deepEqual(await levels.searchUserIds(null, '1', ALICE), [ALICE]);
    assert.deepEqual(await levels.searchUserIds(null, '1', `<@!${ALICE}>`), [ALICE]);
    assert.deepEqual(await levels.searchUserIds(null, '1', 'alice'), [], 'no Discord client: ids only');
    const discord = { searchMembers: async (g, q) => (q === 'al' ? [{ id: ALICE }, { id: ALICE }, { id: 'bad' }] : []) };
    assert.deepEqual(await levels.searchUserIds(discord, '1', 'al'), [ALICE]);
    const down = { searchMembers: async () => { throw new Error('503'); } };
    await assert.rejects(levels.searchUserIds(down, '1', 'al'), (err) => err instanceof RecordError && err.code === 'discord');
  });

  test('profiles never fail a list', async () => {
    const logs = [];
    const map = await levels.loadProfiles({ getUsers: async () => { throw new Error('down'); } }, [ALICE], { warn: (m) => logs.push(m) });
    assert.equal(map.size, 0);
    assert.equal(logs.length, 1);
    assert.equal((await levels.loadProfiles(null, [ALICE])).size, 0);
    assert.equal((await levels.loadProfiles({ getUsers: async () => { throw new Error('x'); } }, [])).size, 0, 'no call without ids');
  });

  test('names and avatars', () => {
    const p = { id: ALICE, username: 'alice', globalName: 'Alice', avatarUrl: 'https://cdn.discordapp.com/avatars/a.png' };
    assert.equal(levels.profileName(p, ALICE), 'Alice');
    assert.equal(levels.profileLabel(p, ALICE), 'Alice (@alice)');
    assert.equal(levels.profileLabel({ ...p, globalName: null }, ALICE), '@alice');
    assert.equal(levels.profileLabel(null, ALICE), `Unknown user (${ALICE})`);
    assert.equal(levels.profileName(null, ALICE), ALICE);
    assert.equal(levels.avatarFor(p, ALICE), p.avatarUrl);
    assert.match(levels.avatarFor(null, ALICE), /^https:\/\/cdn\.discordapp\.com\/embed\/avatars\/\d\.png$/);
  });

  test('levelItem and balanceItem carry every field the descriptors name', () => {
    const profile = { id: ALICE, username: 'alice', globalName: null, avatarUrl: 'https://cdn.example/a.png' };
    const li = levels.levelItem({ userId: ALICE, xp: 1234, messageCount: 7, voiceMinutes: 3, lastXpAt: new Date(0) }, { rank: 2, profile });
    for (const d of [...COLLECTIONS.levels.fields, ...COLLECTIONS.levels.readOnly]) assert.ok(Object.hasOwn(li, d.field), `levels.${d.field}`);
    assert.equal(li.id, ALICE);
    assert.equal(li.level, xpService.levelFromXp(1234));
    assert.equal(li.summary, `Level ${li.level} · 1,234 XP · #2`);
    assert.equal(li.lastXpAt, new Date(0).toISOString());
    assert.equal(levels.emptyLevelItem(ALICE).xp, 0);

    const bi = balances.balanceItem({ userId: ALICE, balance: 1500, dailyStreak: 2, lastDaily: null, lastWork: null, updatedAt: null },
      { rank: 1, profile, currency: { name: 'gems', emoji: '💎' } });
    for (const d of [...COLLECTIONS.balances.fields, ...COLLECTIONS.balances.readOnly]) assert.ok(Object.hasOwn(bi, d.field), `balances.${d.field}`);
    assert.equal(bi.summary, '💎 1,500 gems · #1');
    assert.equal(bi.name, 'alice');
  });

  test('caseItem: type, status, duration and the read-only create fields', () => {
    const now = Date.UTC(2026, 9, 11, 12);
    const created = new Date(now - DAY);
    const row = (over) => ({
      caseId: 7, guildId: '1', targetUserId: ALICE, moderatorId: BOB, type: 'WARN', reason: 'spam\nmore', proofUrl: null,
      active: true, expiresAt: null, createdAt: created, ...over,
    });
    const profiles = new Map([[ALICE, { id: ALICE, username: 'alice', globalName: 'Alice', avatarUrl: 'https://cdn.example/a.png' }]]);
    const warn = cases.caseItem(row(), { profiles, now });
    for (const d of [...COLLECTIONS.cases.fields, ...COLLECTIONS.cases.readOnly]) assert.ok(Object.hasOwn(warn, d.field), `cases.${d.field}`);
    assert.equal(warn.id, '7');
    assert.equal(warn.title, '#7 · Warn');
    assert.equal(warn.reason, 'spam more');
    assert.equal(warn.status, 'active');
    assert.equal(warn.member, 'Alice (@alice)');
    assert.equal(warn.moderator, `Unknown user (${BOB})`);
    assert.equal(warn.summary, 'Alice · Active · 2026-10-10');
    assert.equal(cases.caseItem(row({ active: false }), { now }).status, 'pardoned');

    const temp = cases.caseItem(row({ type: 'BAN', expiresAt: new Date(created.getTime() + 7 * DAY + 1234) }), { now });
    assert.equal(temp.type, 'TEMPBAN');
    assert.equal(temp.action, 'BAN');
    assert.equal(temp.duration, '1w');
    assert.equal(temp.status, 'active');
    assert.equal(cases.caseItem(row({ type: 'BAN' }), { now }).duration, null);
    assert.equal(cases.caseItem(row({ type: 'BAN', active: false }), { now }).status, 'lifted');
    const mute = cases.caseItem(row({ type: 'MUTE', expiresAt: new Date(now - 1000) }), { now });
    assert.equal(mute.action, 'TIMEOUT');
    assert.equal(mute.status, 'ending');
    assert.equal(cases.caseItem(row({ type: 'TIMEOUT', active: false, expiresAt: new Date(now - 1000) }), { now }).status, 'expired');
    assert.equal(cases.caseItem(row({ type: 'KICK' }), { now }).status, null);
    assert.equal(cases.caseItem(row({ type: 'NOTE' }), { now }).action, null);
    assert.equal(cases.caseItem(row({ reason: null }), { now }).reason, 'No reason provided');
  });

  test('a case number in the search box', () => {
    assert.equal(cases.caseNumberQuery('12'), 12);
    assert.equal(cases.caseNumberQuery(' #12 '), 12);
    assert.equal(cases.caseNumberQuery('0'), null);
    assert.equal(cases.caseNumberQuery(ALICE), null, 'a user id is not a case number');
    assert.equal(cases.caseNumberQuery('alice'), null);
  });
});

// ── The 'dashboard' moderation source ────────────────────────────────────────

describe("moderationService: source 'dashboard'", () => {
  test('is a known source, labelled in the case log and the audit log', () => {
    assert.ok(svc.MOD_SOURCES.includes('dashboard'));
    assert.equal(svc.SOURCE_LABEL.dashboard, 'Dashboard');
    assert.equal(svc.buildAuditReason('spam', { username: 'mod' }, 'dashboard'), '[Dashboard] spam | by mod');
    assert.ok(svc.buildAuditReason('x'.repeat(600), { username: 'mod' }, 'dashboard').length <= 512);
    const embed = svc.buildModLogEmbed({
      infraction: { caseId: 1, targetUserId: ALICE, moderatorId: BOB, type: 'WARN', reason: 'r', createdAt: new Date() },
      target: { username: 'alice' }, moderator: { username: 'bob' }, source: 'dashboard',
    }).toJSON();
    assert.deepEqual(embed.fields.find((f) => f.name === 'Source')?.value, 'Dashboard');
  });

  test('is checked like a command: self, rank of the moderator, immune roles, admins', () => {
    const guild = mkGuild();
    const alice = mkMember(guild, ALICE, { position: 5 });
    const mod = mkMember(guild, BOB, { position: 10, perms: ['KickMembers'] });
    const pre = (over) => svc.preflightModAction({
      action: 'KICK', guild, me: guild.members.me, member: alice, targetId: ALICE, executor: mod, settings: null, source: 'dashboard', muteRole: null, ...over,
    });
    assert.equal(pre({}), null);
    assert.equal(pre({ executor: alice }).error, svc.MSG.SELF);
    assert.equal(pre({ executor: mkMember(guild, BOB, { position: 5, perms: ['KickMembers'] }) }).error, svc.MSG.EXEC_ROLE);
    const immune = { id: '900000000000000001', position: 1 };
    const settings = { immuneRoleIds: [immune.id], modRoleIds: [] };
    assert.equal(pre({ member: mkMember(guild, ALICE, { position: 5, roles: [immune] }), settings }).error, svc.MSG.IMMUNE_ROLE);
    assert.equal(pre({ member: mkMember(guild, ALICE, { position: 5, perms: ['Administrator'] }) }).error, svc.MSG.IMMUNE_ADMIN);
    // An automated source would refuse every admin outright; a person outranking them by level may act.
    assert.equal(pre({ member: mkMember(guild, ALICE, { position: 5, perms: ['Administrator'] }), source: 'automod' }).error, svc.MSG.IMMUNE_AUTO);
  });
});

// ── Tasks (fake discord.js objects, stubbed services) ────────────────────────

describe('task registry', () => {
  test('the moderation and XP kinds are registered', () => {
    for (const kind of ['mod.action', 'mod.editCase', 'mod.clearWarnings', 'xp.adjust']) assert.equal(typeof TASKS[kind]?.run, 'function', kind);
  });
});

describe('mod.action', () => {
  const settingsOn = { modEnabled: true, modRoleIds: [], immuneRoleIds: [] };

  function setup({ settings = settingsOn, result = null, count = 2 } = {}) {
    const guild = mkGuild();
    const admin = add(guild, mkMember(guild, BOB, { position: 50, perms: ['ManageGuild'] }));
    add(guild, mkMember(guild, ALICE, { position: 5, user: { globalName: 'Alice' } }));
    const calls = [];
    const record = (name) => async (input) => {
      calls.push({ name, input });
      return result ?? {
        ok: true, action: input.action, dm: 'sent', warnings: [], escalation: null, removed: ['timeout'], cleared: 1,
        infraction: { caseId: 12, type: input.action === 'TEMPBAN' ? 'BAN' : (input.action ?? 'UNBAN') },
      };
    };
    const tasks = modTasks.createModerationTasks({
      getSettingsSafe: async () => settings,
      executeModAction: record('executeModAction'),
      executeUnban: record('executeUnban'),
      executeUnmute: record('executeUnmute'),
      prisma: { infraction: { count: async () => count } },
    });
    const ctx = (payload, actor = { id: BOB, member: admin, isBotOwner: false }) => ({ guild, client: guild.client, actor, payload });
    return { guild, admin, calls, tasks, ctx };
  }

  test('a ban with a duration is a temporary ban by the dashboard user, source dashboard', async () => {
    const { tasks, ctx, calls, admin, guild } = setup();
    const r = await tasks['mod.action'].run(ctx({ action: 'BAN', userId: ALICE, reason: 'raid', durationMs: 7 * DAY, deleteMessageSeconds: 86_400, proofUrl: null }));
    assert.equal(calls.length, 1);
    const { input } = calls[0];
    assert.equal(input.action, 'TEMPBAN');
    assert.equal(input.source, 'dashboard');
    assert.equal(input.moderator, admin.user);
    assert.deepEqual(input.target, { id: ALICE });
    assert.equal(input.guild, guild);
    assert.equal(input.durationMs, 7 * DAY);
    assert.equal(input.deleteMessageSeconds, 86_400);
    assert.equal(r.message, 'Banned member03 for 1 week (case #12). Deleted 1 day(s) of messages.');
    assert.deepEqual(r.data, { caseId: 12, action: 'TEMPBAN', userId: ALICE });
  });

  test('a warning reports the active warnings, the escalation and a failed DM', async () => {
    const { tasks, ctx } = setup({
      result: {
        ok: true, action: 'WARN', dm: 'failed', warnings: [], infraction: { caseId: 3, type: 'WARN' },
        escalation: { tierId: 1, action: 'TIMEOUT', ok: true, caseId: 4, error: null },
      },
    });
    const r = await tasks['mod.action'].run(ctx({ action: 'WARN', userId: ALICE, reason: 'spam' }));
    assert.equal(r.message, "Warned member03 (case #3). Active warnings: 2. Escalation T1: Timeout applied (case #4). I couldn't DM the user.");
  });

  test('unban and unmute use their own service calls', async () => {
    const { tasks, ctx, calls } = setup();
    await tasks['mod.action'].run(ctx({ action: 'UNBAN', userId: CAROL, reason: null }));
    await tasks['mod.action'].run(ctx({ action: 'UNMUTE', userId: ALICE }));
    assert.deepEqual(calls.map((c) => c.name), ['executeUnban', 'executeUnmute']);
    assert.equal(calls[0].input.userId, CAROL);
    assert.equal(calls[0].input.source, 'dashboard');
    assert.deepEqual(calls[1].input.target, { id: ALICE });
  });

  test("the service's refusal is the task's message", async () => {
    const { tasks, ctx } = setup({ result: { ok: false, code: 'HIERARCHY', error: svc.MSG.EXEC_ROLE } });
    await taskRefusal(tasks['mod.action'].run(ctx({ action: 'KICK', userId: ALICE })), /highest role is above or equal to yours/);
  });

  test('who may act: a member, moderator level, module on', async () => {
    const { tasks, ctx, guild, calls } = setup();
    const kick = { action: 'KICK', userId: ALICE };
    await taskRefusal(tasks['mod.action'].run(ctx(kick, { id: CAROL, member: null, isBotOwner: false })), /no longer a member/);
    await taskRefusal(tasks['mod.action'].run(ctx(kick, { id: CAROL, member: null, isBotOwner: true })), /Join this server/);
    const plain = add(guild, mkMember(guild, CAROL, { position: 60 }));
    await taskRefusal(tasks['mod.action'].run(ctx(kick, { id: CAROL, member: plain, isBotOwner: false })), /moderator rights/);
    const off = setup({ settings: { ...settingsOn, modEnabled: false } });
    await taskRefusal(off.tasks['mod.action'].run(off.ctx(kick)), /module is off/);
    const down = setup({ settings: null });
    await taskRefusal(down.tasks['mod.action'].run(down.ctx(kick)), /couldn't load/);
    assert.equal(calls.length, 0);
  });

  test('the payload is checked again', async () => {
    const { tasks, ctx } = setup();
    const run = (payload) => tasks['mod.action'].run(ctx(payload));
    await taskRefusal(run(null), /malformed/);
    await taskRefusal(run({ action: 'MUTE', userId: ALICE }), /Unknown moderation action/);
    await taskRefusal(run({ action: 'KICK', userId: 'alice' }), /Pick a member/);
    await taskRefusal(run({ action: 'WARN', userId: ALICE, reason: ' ' }), /needs a reason/);
    await taskRefusal(run({ action: 'KICK', userId: ALICE, reason: 'x'.repeat(513) }), /512/);
    await taskRefusal(run({ action: 'TIMEOUT', userId: ALICE }), /need a duration/);
    await taskRefusal(run({ action: 'BAN', userId: ALICE, durationMs: -1 }), /Invalid duration/);
    await taskRefusal(run({ action: 'SOFTBAN', userId: ALICE, deleteMessageSeconds: 0 }), /between 1 and 7 days/);
    await taskRefusal(run({ action: 'BAN', userId: ALICE, deleteMessageSeconds: 8 * 86_400 }), /between 0 and 7 days/);
    await taskRefusal(run({ action: 'KICK', userId: ALICE, proofUrl: 'ftp://x' }), /http/);
    const p = modTasks.readActionPayload({ action: 'UNBAN', userId: ALICE, proofUrl: 'ftp://ignored', durationMs: 5 });
    assert.equal(p.proofUrl, null, '/unban takes no proof');
    assert.equal(p.durationMs, null);
  });
});

describe('mod.editCase and mod.clearWarnings', () => {
  function setup({ level = 'admin', count = 2, updateError = null } = {}) {
    const guild = mkGuild();
    const perms = level === 'admin' ? ['ManageGuild'] : ['KickMembers'];
    const actorMember = add(guild, mkMember(guild, BOB, { position: 50, perms }));
    const calls = { update: [], updateMany: [], notes: [], create: [] };
    const tasks = modTasks.createModerationTasks({
      getSettingsSafe: async () => ({ modEnabled: true, modRoleIds: [] }),
      postModLog: async (g, embed) => { calls.notes.push(embed.toJSON()); },
      createInfraction: async (...args) => { calls.create.push(args); return { caseId: 21 }; },
      prisma: {
        infraction: {
          update: async (q) => { if (updateError) throw updateError; calls.update.push(q); return {}; },
          count: async () => count,
          updateMany: async (q) => { calls.updateMany.push(q); return { count }; },
        },
      },
    });
    const ctx = (payload) => ({ guild, client: guild.client, actor: { id: BOB, member: actorMember, isBotOwner: false }, payload });
    return { guild, tasks, ctx, calls, actorMember };
  }

  test('/case reason and proof: one write, one case-log note per change', async () => {
    const { tasks, ctx, calls, guild } = setup({ level: 'mod' });
    const r = await tasks['mod.editCase'].run(ctx({ caseId: 4, reason: ' new reason ', proofUrl: 'https://x.test/p' }));
    assert.deepEqual(calls.update, [{ where: { guildId_caseId: { guildId: guild.id, caseId: 4 } }, data: { reason: 'new reason', proofUrl: 'https://x.test/p' } }]);
    assert.equal(calls.notes.length, 2);
    assert.equal(calls.notes[0].title, '📝 Case #4 edited');
    assert.match(calls.notes[0].description, /^Reason changed by <@\d+> on the dashboard:\n> new reason$/);
    assert.match(calls.notes[1].description, /Proof link changed/);
    assert.deepEqual(r, { message: 'Case #4 updated.', data: { caseId: 4 } });
  });

  test('/case refusals', async () => {
    const { tasks, ctx } = setup();
    await taskRefusal(tasks['mod.editCase'].run(ctx({ caseId: 0, reason: 'x' })), /does not exist/);
    await taskRefusal(tasks['mod.editCase'].run(ctx({ caseId: 1, reason: '  ' })), /can't be empty/);
    await taskRefusal(tasks['mod.editCase'].run(ctx({ caseId: 1, proofUrl: 'javascript:alert(1)' })), /http/);
    await taskRefusal(tasks['mod.editCase'].run(ctx({ caseId: 1 })), /Nothing to change/);
    const gone = setup({ updateError: Object.assign(new Error('not found'), { code: 'P2025' }) });
    await taskRefusal(gone.tasks['mod.editCase'].run(gone.ctx({ caseId: 9, reason: 'x' })), /Case #9 doesn't exist/);
  });

  test('/history clear: administrators only; pardons and records a silent NOTE', async () => {
    const mod = setup({ level: 'mod' });
    await taskRefusal(mod.tasks['mod.clearWarnings'].run(mod.ctx({ userId: ALICE })), /Only administrators/);

    const { tasks, ctx, calls, guild, actorMember } = setup({ count: 3 });
    const r = await tasks['mod.clearWarnings'].run(ctx({ userId: ALICE, reason: 'fresh start' }));
    const where = { guildId: guild.id, targetUserId: ALICE, type: 'WARN', active: true };
    assert.deepEqual(calls.updateMany, [{ where, data: { active: false } }]);
    const [g, target, moderator, type, opts] = calls.create[0];
    assert.equal(g, guild);
    assert.equal(target.id, ALICE);
    assert.equal(moderator, actorMember.user);
    assert.equal(type, 'NOTE');
    assert.deepEqual({ ...opts, client: undefined }, {
      reason: '[Pardon] Cleared 3 warning(s): fresh start', silent: true, skipEscalation: true, client: undefined, source: 'dashboard',
    });
    assert.equal(r.message, 'Cleared 3 warning(s) for u003 (case #21).');

    const none = setup({ count: 0 });
    await taskRefusal(none.tasks['mod.clearWarnings'].run(none.ctx({ userId: ALICE })), /has no active warnings/);
  });
});

describe('xp.adjust', () => {
  function setup({ settings = { levelRolesSyncOnDemotion: true }, results = {} } = {}) {
    const guild = mkGuild();
    const admin = add(guild, mkMember(guild, BOB, { position: 50, perms: ['ManageGuild'] }));
    const alice = add(guild, mkMember(guild, ALICE, { position: 5 }));
    guild.roles.cache.set('900000000000000011', { id: '900000000000000011', name: 'Regular' });
    const calls = [];
    const stub = (name, fallback) => async (...args) => {
      calls.push({ name, args });
      return Object.hasOwn(results, name) ? results[name] : fallback;
    };
    const tasks = memberTasks.createMemberTasks({
      getSettingsSafe: async () => settings,
      applyXpDelta: stub('applyXpDelta', { xp: 250, oldLevel: 1, newLevel: 2, created: false }),
      subtractXp: stub('subtractXp', { xp: 50, oldXp: 150, oldLevel: 1, newLevel: 0 }),
      setXp: stub('setXp', { xp: 500, oldLevel: 1, newLevel: 3, created: false }),
      resetXp: stub('resetXp', { oldLevel: 4 }),
      handleLevelChange: stub('handleLevelChange', { leveledUp: true, roles: { added: ['900000000000000011'], removed: [], failed: [] } }),
      syncLevelRoles: stub('syncLevelRoles', { added: [], removed: ['900000000000000011'], failed: [] }),
    });
    const ctx = (payload, actor = { id: BOB, member: admin, isBotOwner: false }) => ({ guild, client: guild.client, actor, payload });
    return { guild, alice, tasks, ctx, calls };
  }
  const run = (s, payload, actor) => s.tasks['xp.adjust'].run(s.ctx(payload, actor));

  test('give: the /xp pipeline without an announcement', async () => {
    const s = setup();
    const r = await run(s, { op: 'give', userId: ALICE, amount: 100 });
    assert.deepEqual(s.calls[0], { name: 'applyXpDelta', args: [{ guildId: s.guild.id, userId: ALICE, delta: 100 }] });
    const lc = s.calls[1].args[0];
    assert.equal(s.calls[1].name, 'handleLevelChange');
    assert.equal(lc.member, s.alice);
    assert.equal(lc.source, 'admin');
    assert.equal(lc.announce, false);
    assert.equal(lc.channel, null);
    assert.equal(r.message, 'Gave 100 XP to member03. Total: 250 XP (level 2). Level 1 → 2. Roles added: @Regular.');
    assert.deepEqual(r.data, { userId: ALICE, xp: 250, level: 2 });
  });

  test('give and set refuse bots and users who are not members; set 0 does not', async () => {
    const s = setup();
    await taskRefusal(run(s, { op: 'give', userId: ROBOT, amount: 5 }), /Bots don't earn XP/);
    await taskRefusal(run(s, { op: 'give', userId: CAROL, amount: 5 }), /isn't in this server/);
    await taskRefusal(run(s, { op: 'set', userId: CAROL, amount: 5 }), /isn't in this server/);
    await run(s, { op: 'set', userId: CAROL, amount: 0 });
    assert.deepEqual(s.calls.map((c) => c.name), ['setXp', 'handleLevelChange']);
  });

  test('take, set and reset: nothing to do is a refusal; reset syncs roles down', async () => {
    const empty = setup({ results: { subtractXp: null, setXp: null, resetXp: null } });
    await taskRefusal(run(empty, { op: 'take', userId: ALICE, amount: 5 }), /has no XP/);
    await taskRefusal(run(empty, { op: 'set', userId: ALICE, amount: 0 }), /already has 0 XP/);
    await taskRefusal(run(empty, { op: 'reset', userId: ALICE }), /has no XP/);

    const s = setup();
    assert.match((await run(s, { op: 'take', userId: ALICE, amount: 100 })).message, /^Removed 100 XP from member03\. Total: 50 XP \(level 0\)\./);
    const reset = await run(s, { op: 'reset', userId: ALICE });
    const sync = s.calls.find((c) => c.name === 'syncLevelRoles');
    assert.equal(sync.args[1], 0);
    assert.deepEqual(sync.args[3], { syncDown: true, reason: 'XP reset' });
    assert.equal(reset.message, 'Wiped XP and level for member03 (was level 4). Roles removed: @Regular.');

    const noSync = setup({ settings: { levelRolesSyncOnDemotion: false } });
    await run(noSync, { op: 'reset', userId: ALICE });
    assert.ok(!noSync.calls.some((c) => c.name === 'syncLevelRoles'));
  });

  test('administrators only (the bot owner too); the payload is checked again', async () => {
    const s = setup();
    const mod = add(s.guild, mkMember(s.guild, CAROL, { position: 40, perms: ['KickMembers'] }));
    await taskRefusal(run(s, { op: 'give', userId: ALICE, amount: 1 }, { id: CAROL, member: mod, isBotOwner: false }), /Only administrators/);
    await taskRefusal(run(s, { op: 'give', userId: ALICE, amount: 1 }, { id: CAROL, member: null, isBotOwner: false }), /no longer a member/);
    await run(s, { op: 'give', userId: ALICE, amount: 1 }, { id: CAROL, member: null, isBotOwner: true });
    await taskRefusal(run(s, { op: 'give', userId: ALICE, amount: 0 }), /between 1 and 1,000,000/);
    await taskRefusal(run(s, { op: 'set', userId: ALICE, amount: XP_MAX + 1 }), /between 0 and/);
    await taskRefusal(run(s, { op: 'double', userId: ALICE }), /Unknown XP change/);
    await taskRefusal(run(s, { op: 'reset', userId: '12' }), /Pick a member/);
    const settingsDown = setup({ settings: null });
    await taskRefusal(run(settingsDown, { op: 'reset', userId: ALICE }), /couldn't load/);
  });
});

// ── Stores against a real database (opt-in) ─────────────────────────────────

describe('stores (TEST_DATABASE_URL)', { skip: skipDb }, () => {
  const guilds = [];
  const quiet = { info() {}, warn() {}, error() {}, debug() {} };
  let n = 0;
  let stores;
  let settingsSvc;
  let economy;

  async function newGuild() {
    const id = `93${String(Date.now()).slice(-9)}${String(++n).padStart(7, '0')}`;
    guilds.push(id);
    await settingsSvc.ensureGuildSettings(id);
    return id;
  }

  const code = async (promise) => {
    try {
      await promise;
    } catch (err) {
      assert.ok(err instanceof RecordError, `expected a RecordError, got ${err?.stack ?? err}`);
      return err.code;
    }
    return 'ok';
  };
  const snow = (i) => `5${String(i).padStart(17, '0')}`;

  /** ctx.discord: users by id (bots marked), member search by username prefix. */
  function fakeDiscord(users) {
    const calls = { getUsers: 0, searchMembers: 0 };
    return {
      calls,
      async getUsers(ids) {
        calls.getUsers++;
        return new Map(ids.map((id) => [id, users[id] ? { id, globalName: null, avatarUrl: `https://cdn.example/${id}.png`, bot: false, ...users[id] } : null]));
      },
      async searchMembers(guildId, q) {
        calls.searchMembers++;
        return Object.entries(users).filter(([, u]) => u.username?.startsWith(q)).map(([id, u]) => ({ id, username: u.username }));
      },
    };
  }

  before(async () => {
    settingsSvc = await import('../src/services/settingsService.js');
    economy = await import('../src/services/economyService.js');
    stores = {};
    for (const key of KEYS) stores[key] = (await import(`../src/dashboard/collections/${key}.store.js`)).store;
  });

  after(async () => {
    for (const guildId of guilds) await prisma.guildSettings.deleteMany({ where: { guildId } }).catch(() => {});
  });

  test('every store has the full interface', () => {
    for (const key of KEYS) {
      for (const fn of ['list', 'get', 'create', 'update', 'remove']) assert.equal(typeof stores[key][fn], 'function', `${key}.${fn}`);
      assert.deepEqual(Object.keys(stores[key].actions).sort(), COLLECTIONS[key].actions.map((a) => a.id).sort(), `${key} actions`);
    }
  });

  test('balances: give, take, set, reset like /economy-admin; bots refused', async () => {
    const guildId = await newGuild();
    const discord = fakeDiscord({ [ALICE]: { username: 'alice' }, [ROBOT]: { username: 'robot', bot: true } });
    const ctx = { userId: BOB, logger: quiet, discord };
    const start = economy.economyConfig(await settingsSvc.getSettings(guildId)).startingBalance;

    const created = await stores.balances.create(guildId, { userId: ALICE, amount: 500 }, ctx);
    assert.equal(created.balance, start + 500);
    assert.equal(created.name, 'alice');
    assert.equal(created.rank, 1);
    assert.equal(await code(stores.balances.create(guildId, { userId: ROBOT, amount: 5 }, ctx)), 'invalid');
    assert.equal(await code(stores.balances.create(guildId, { userId: CAROL, amount: 5 }, ctx)), 'invalid', 'unknown user');
    assert.equal(await code(stores.balances.create(guildId, { userId: ALICE, amount: 5 }, { ...ctx, discord: null })), 'unavailable');

    const take = await stores.balances.actions.take(guildId, ALICE, { amount: COIN_MAX }, ctx);
    assert.equal(take.item.balance, 0);
    assert.match(take.message, /They only had/);

    await settingsSvc.updateSettings(guildId, { economyMaxBalance: 5000 });
    assert.equal(await code(stores.balances.actions.set(guildId, ALICE, { balance: 6000 }, ctx)), 'invalid');
    assert.equal((await stores.balances.actions.set(guildId, ALICE, { balance: 4000 }, ctx)).item.balance, 4000);
    const capped = await stores.balances.actions.give(guildId, ALICE, { amount: 5000 }, ctx);
    assert.equal(capped.item.balance, 5000);
    assert.match(capped.message, /server maximum reached/);

    const reset = await stores.balances.actions.reset(guildId, ALICE, { what: 'coins' }, ctx);
    assert.equal(reset.item.balance, Math.min(start, 5000));
    assert.equal(reset.message, 'Reset 1 balance.');
    assert.equal(await code(stores.balances.update(guildId, ALICE, {}, ctx)), 'invalid');
    assert.equal(await code(stores.balances.remove(guildId, ALICE, ctx)), 'invalid');
    assert.equal(await stores.balances.get(guildId, 'nope'), null);
    assert.equal((await stores.balances.get(guildId, ALICE)).balance, Math.min(start, 5000));
  });

  test('balances: richest first in pages, shared ranks, search', async () => {
    const guildId = await newGuild();
    const settings = await settingsSvc.getSettings(guildId);
    const users = {};
    for (let i = 1; i <= 30; i++) {
      users[snow(i)] = { username: `member${i}` };
      await economy.setBalance(guildId, snow(i), i === 30 ? 29 * 10 : i * 10, settings); // #29 and #30 tie
    }
    const discord = fakeDiscord(users);
    const first = await stores.balances.list(guildId, {}, { discord, logger: quiet });
    assert.equal(first.items.length, 25);
    assert.ok(first.nextCursor);
    assert.deepEqual(first.items.slice(0, 3).map((i) => [i.userId, i.rank]), [[snow(29), 1], [snow(30), 1], [snow(28), 3]]);
    const second = await stores.balances.list(guildId, { cursor: first.nextCursor }, { discord, logger: quiet });
    assert.equal(second.items.length, 5);
    assert.equal(second.nextCursor, undefined);
    assert.equal(second.items.at(-1).userId, snow(1));
    assert.equal(second.items.at(-1).rank, 30);
    assert.deepEqual((await stores.balances.list(guildId, { query: snow(3) }, { discord })).items.map((i) => i.userId), [snow(3)]);
    assert.deepEqual((await stores.balances.list(guildId, { query: 'member2' }, { discord })).items.map((i) => i.userId).slice(0, 2), [snow(29), snow(28)]);
    assert.deepEqual((await stores.balances.list(guildId, { query: 'nobody' }, { discord })).items, []);
  });

  /** The bot side of runTask: the real task with a fake guild and the real services. */
  function botRunner(guild, actor) {
    const calls = [];
    const runTask = async (kind, payload) => {
      calls.push({ kind, payload });
      return runLike(TASKS[kind], { guild, client: guild.client, actor, payload, taskId: 't1' });
    };
    return { runTask, calls };
  }

  test('levels: the leaderboard in pages; XP changes through xp.adjust', async () => {
    const guildId = await newGuild();
    const guild = mkGuild(guildId);
    const admin = add(guild, mkMember(guild, BOB, { position: 50, perms: ['ManageGuild'] }));
    const users = {};
    for (let i = 1; i <= 30; i++) {
      users[snow(i)] = { username: `member${i}` };
      add(guild, mkMember(guild, snow(i)));
      await xpService.applyXpDelta({ guildId, userId: snow(i), delta: i * 100 });
    }
    const discord = fakeDiscord(users);
    const { runTask, calls } = botRunner(guild, { id: BOB, member: admin, isBotOwner: false });
    const ctx = { userId: BOB, logger: quiet, discord, runTask };

    const first = await stores.levels.list(guildId, {}, { discord });
    assert.deepEqual(first.items.slice(0, 2).map((i) => [i.userId, i.rank, i.xp]), [[snow(30), 1, 3000], [snow(29), 2, 2900]]);
    const second = await stores.levels.list(guildId, { cursor: first.nextCursor }, { discord });
    assert.deepEqual(second.items.map((i) => i.rank), [26, 27, 28, 29, 30]);
    assert.equal(second.nextCursor, undefined);
    assert.equal((await stores.levels.list(guildId, { query: 'member3' }, { discord })).items[0].rank, 1, 'member30 leads');

    add(guild, mkMember(guild, ALICE));
    const created = await stores.levels.create(guildId, { userId: ALICE, amount: 5000 }, ctx);
    assert.equal(created.xp, 5000);
    assert.equal(created.rank, 1);
    assert.deepEqual(calls.at(-1), { kind: 'xp.adjust', payload: { op: 'give', amount: 5000, userId: ALICE } });
    const stored = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: ALICE } } });
    assert.equal(stored.level, xpService.levelFromXp(5000), 'handleLevelChange stored the new level');

    const take = await stores.levels.actions.take(guildId, ALICE, { amount: 1000 }, ctx);
    assert.equal(take.item.xp, 4000);
    assert.match(take.message, /^Removed 1,000 XP/);
    assert.equal((await stores.levels.actions.set(guildId, ALICE, { xp: 42 }, ctx)).item.xp, 42);
    const reset = await stores.levels.actions.reset(guildId, ALICE, {}, ctx);
    assert.equal(reset.item.xp, 0);
    assert.equal(await stores.levels.get(guildId, ALICE), null, 'the row is gone, like /xp reset');
    assert.equal(await code(stores.levels.actions.reset(guildId, ALICE, {}, ctx)), 'failed', 'nothing to reset');
    assert.equal(await code(stores.levels.create(guildId, { userId: CAROL, amount: 5 }, ctx)), 'failed', 'not a member');
    assert.equal(await code(stores.levels.create(guildId, { userId: ALICE, amount: 5 }, { ...ctx, runTask: async () => ({ status: 'pending', message: 'wait' }) })), 'unavailable');
    assert.equal(await code(stores.levels.create(guildId, { userId: ALICE, amount: 5 }, { ...ctx, runTask: async () => ({ status: 'offline', message: 'off' }) })), 'unavailable');
  });

  test('cases: actions through mod.action and executeModAction, newest first, edits and clear warnings', async () => {
    const guildId = await newGuild();
    const guild = mkGuild(guildId);
    const admin = add(guild, mkMember(guild, BOB, { position: 50, perms: ['ManageGuild', 'BanMembers', 'KickMembers', 'ModerateMembers'] }));
    add(guild, mkMember(guild, ALICE, { position: 5 }));
    add(guild, mkMember(guild, CAROL, { position: 60 }));
    const discord = fakeDiscord({ [ALICE]: { username: 'alice', globalName: 'Alice' }, [BOB]: { username: 'bob' } });
    const { runTask, calls } = botRunner(guild, { id: BOB, member: admin, isBotOwner: false });
    const ctx = { userId: BOB, logger: quiet, discord, runTask };
    const make = (data) => stores.cases.create(guildId, accept('cases', data), ctx);

    const warn = await make({ action: 'WARN', userId: ALICE, reason: 'spam' });
    assert.equal(warn.id, '1');
    assert.equal(warn.title, '#1 · Warn');
    assert.equal(warn.status, 'active');
    assert.equal(warn.member, 'Alice (@alice)');
    assert.equal(warn.moderator, '@bob');
    assert.equal(calls[0].kind, 'mod.action');

    const ban = await make({ action: 'BAN', userId: ALICE, duration: '7d', reason: 'raid', proofUrl: 'https://x.test/p' });
    assert.equal(ban.type, 'TEMPBAN');
    assert.equal(ban.duration, '1w');
    assert.equal(ban.proofUrl, 'https://x.test/p');
    assert.ok(guild.bans.banned.has(ALICE));
    const job = await prisma.scheduledJob.findUnique({ where: { dedupeKey: `mod.unban:${guildId}:${ALICE}` } });
    assert.equal(job?.type, 'mod.unban', 'the temporary ban is lifted by the scheduler');

    const failed = async (data) => {
      try {
        await make(data);
      } catch (err) {
        assert.ok(err instanceof RecordError);
        return [err.code, err.message];
      }
      return ['ok'];
    };
    assert.deepEqual(await failed({ action: 'KICK', userId: CAROL }), ['failed', svc.MSG.EXEC_ROLE]);
    assert.deepEqual(await failed({ action: 'KICK', userId: BOB }), ['failed', svc.MSG.SELF]);
    assert.deepEqual(await failed({ action: 'BAN', userId: ALICE }), ['failed', svc.MSG.ALREADY_BANNED]);
    const unban = await make({ action: 'UNBAN', userId: ALICE, reason: 'appeal' });
    assert.equal(unban.type, 'UNBAN');
    assert.equal((await stores.cases.get(guildId, '2')).status, 'lifted');

    // Newest first, in pages; searches.
    for (let i = 0; i < 30; i++) {
      await svc.recordInfraction({ guildId, targetUserId: CAROL, moderatorId: BOB, type: 'NOTE', reason: `note ${i}` });
    }
    const first = await stores.cases.list(guildId, {}, { discord });
    assert.equal(first.items.length, 25);
    assert.equal(first.items[0].caseNumber, 33);
    assert.equal(first.nextCursor, 'c9');
    const rest = await stores.cases.list(guildId, { cursor: first.nextCursor }, { discord });
    assert.deepEqual(rest.items.map((i) => i.caseNumber), [8, 7, 6, 5, 4, 3, 2, 1]);
    assert.deepEqual((await stores.cases.list(guildId, { query: '#2' }, { discord })).items.map((i) => i.id), ['2']);
    assert.deepEqual((await stores.cases.list(guildId, { query: ALICE }, { discord })).items.map((i) => i.id), ['3', '2', '1']);
    assert.deepEqual((await stores.cases.list(guildId, { query: 'ali' }, { discord })).items.map((i) => i.id), ['3', '2', '1']);
    assert.deepEqual((await stores.cases.list(guildId, { query: '999' }, { discord })).items, []);

    // /case reason and proof: only changes reach the bot.
    const before = calls.length;
    await stores.cases.update(guildId, '1', { reason: 'spam' }, ctx);
    assert.equal(calls.length, before, 'an unchanged reason is not sent');
    const edited = await stores.cases.update(guildId, '1', accept('cases', { reason: 'spam links', proofUrl: 'https://x.test/q' }, update(warn)), ctx);
    assert.equal(edited.reason, 'spam links');
    assert.equal(edited.proofUrl, 'https://x.test/q');
    assert.deepEqual(calls.at(-1), { kind: 'mod.editCase', payload: { caseId: 1, reason: 'spam links', proofUrl: 'https://x.test/q' } });
    assert.equal(await code(stores.cases.update(guildId, '999', { reason: 'x' }, ctx)), 'not_found');

    // /history clear
    const cleared = await stores.cases.actions.clearWarnings(guildId, '1', { clearReason: 'fresh start' }, ctx);
    assert.equal(cleared.item.status, 'pardoned');
    assert.match(cleared.message, /^Cleared 1 warning\(s\) for member03 \(case #34\)\.$/);
    const note = await stores.cases.get(guildId, '34');
    assert.equal(note.type, 'NOTE');
    assert.equal(note.reason, '[Pardon] Cleared 1 warning(s): fresh start');
    assert.equal(await code(stores.cases.actions.clearWarnings(guildId, '1', {}, ctx)), 'failed', 'no active warnings left');

    assert.equal(await code(stores.cases.remove(guildId, '1', ctx)), 'invalid');
    assert.equal(await stores.cases.get(guildId, 'abc'), null);
    assert.equal(await stores.cases.get('900000000000000999', '1'), null, 'another guild');
    const pending = { ...ctx, runTask: async () => ({ status: 'pending', message: 'wait' }) };
    assert.equal(await code(stores.cases.create(guildId, accept('cases', { action: 'KICK', userId: ALICE }), pending)), 'unavailable');
  });
});
