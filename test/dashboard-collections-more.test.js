// Dashboard record collections, part 2: level rewards, shop items, achievements, starboards, translated channels, voice hubs,
// counter channels and coin boosts (src/dashboard/collections/{levelRoles,shopItems,achievements,starboards,translateChannels,
// tempVoiceHubs,statChannels,economyBoosts}.js and their .store.js).
// Offline by default: descriptor lint, the validation of create and update requests (validateRecordInput) and guards that fail
// when a copied limit drifts from its service. With TEST_DATABASE_URL (a migrated database) the stores run against it too:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/dashboard-collections-more.test.js
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

const { COIN_MAX, LIMITS, XP_MAX } = await import('../src/config/constants.js');
const { MODULES } = await import('../src/config/modules.js');
const { FieldError } = await import('../src/dashboard/schema.js');
const { RecordError } = await import('../src/dashboard/collections/errors.js');
const registry = await import('../src/dashboard/collections/index.js');
const { COLLECTIONS, lintCollections, publicCollection, validateRecordInput } = registry;
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

const KEYS = ['levelRoles', 'shopItems', 'achievements', 'starboards', 'translateChannels', 'tempVoiceHubs', 'statChannels', 'economyBoosts'];

const ROLE = '300000000000000001';
const ROLE2 = '300000000000000002';
const CHAN = '400000000000000001';
const CHAN2 = '400000000000000002';
const CAT = '400000000000000003';

after(async () => {
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

/** The validated data of a request. */
const accept = (key, data, opts = { mode: 'create' }) => validateRecordInput(key, data, opts);

/** Asserts a FieldError (optionally at `path`, optionally matching `message`). */
function refuse(key, data, opts, { path, message } = {}) {
  assert.throws(() => validateRecordInput(key, data, opts), (err) => {
    assert.ok(err instanceof FieldError, `expected a FieldError, got ${err?.name}: ${err?.message}`);
    if (path !== undefined) assert.equal(err.path, path, `path of "${err.message}"`);
    if (message !== undefined) assert.match(err.message, message);
    return true;
  });
}

const create = { mode: 'create' };
const update = (current = null) => ({ mode: 'update', current });

// ── Descriptors ──────────────────────────────────────────────────────────────

describe('descriptors', () => {
  test('lintCollections has no problems for these collections', () => {
    const mine = lintCollections().filter((p) => KEYS.some((k) => p.startsWith(`${k}.`) || p.startsWith(`${k}(`) || p.startsWith(`${k}:`) || p.startsWith(`${k}!`)));
    assert.deepEqual(mine, []);
  });

  test('every collection is registered, real (not a placeholder) and tied to a module', () => {
    for (const key of KEYS) {
      const c = COLLECTIONS[key];
      assert.ok(c, `${key} is registered`);
      assert.equal(c.key, key);
      assert.ok(!c.placeholder, `${key} is still a placeholder`);
      assert.ok(Object.hasOwn(MODULES, c.module), `${key}: module ${c.module}`);
      assert.ok(c.fields.length > 0, `${key} has fields`);
      assert.ok(c.label && c.singular && c.description, `${key} has a label, a singular and a description`);
      assert.ok(Number.isInteger(c.limit) && c.limit > 0, `${key} has a limit`);
      for (const f of c.fields) {
        assert.ok(f.label, `${key}.${f.field} has a label`);
        assert.ok(f.description || f.type === 'boolean', `${key}.${f.field} has a description`);
      }
    }
  });

  test('limits come from LIMITS', () => {
    const expected = {
      levelRoles: LIMITS.LEVEL_ROLES, shopItems: LIMITS.SHOP_ITEMS, achievements: LIMITS.ACHIEVEMENTS, starboards: LIMITS.STARBOARDS,
      translateChannels: LIMITS.TRANSLATE_CHANNELS, tempVoiceHubs: LIMITS.TEMPVOICE_HUBS, statChannels: LIMITS.STAT_CHANNELS,
      economyBoosts: LIMITS.ECONOMY_BOOSTS,
    };
    for (const [key, limit] of Object.entries(expected)) assert.equal(COLLECTIONS[key].limit, limit, key);
  });

  test('title and subtitle fields exist, createOnly fields are fields, and the public copy is plain JSON', () => {
    for (const key of KEYS) {
      const c = COLLECTIONS[key];
      const names = new Set([...c.fields, ...c.readOnly].map((f) => f.field));
      assert.ok(names.has(c.titleField), `${key}: titleField ${c.titleField}`);
      if (c.subtitleField) assert.ok(names.has(c.subtitleField), `${key}: subtitleField ${c.subtitleField}`);
      for (const f of c.createOnly) assert.ok(c.fields.some((d) => d.field === f), `${key}: createOnly ${f}`);
      const pub = publicCollection(c);
      assert.deepEqual(JSON.parse(JSON.stringify(pub)), JSON.parse(JSON.stringify(pub)));
      assert.ok(!JSON.stringify(pub).includes('"check"'), `${key}: no server-only hooks in the public copy`);
      assert.ok(JSON.stringify(pub).length < 20_000, `${key}: public descriptor is small`);
    }
  });

  test('the channel and role kinds are the ones the commands offer', () => {
    const field = (key, name) => COLLECTIONS[key].fields.find((f) => f.field === name);
    assert.deepEqual(field('starboards', 'channelId').channelTypes, ['text', 'announcement']);
    assert.deepEqual(field('tempVoiceHubs', 'channelId').channelTypes, ['voice']);
    assert.deepEqual(field('tempVoiceHubs', 'categoryId').channelTypes, ['category']);
    assert.deepEqual(field('statChannels', 'channelId').channelTypes, ['voice']);
    assert.equal(field('levelRoles', 'roleId').assignable, true);
    assert.equal(field('shopItems', 'roleId').assignable, true);
    assert.equal(field('achievements', 'tiers').itemFields.find((f) => f.field === 'roleAdd').assignable, true);
    assert.equal(field('achievements', 'tiers').itemFields.find((f) => f.field === 'roleRemove').assignable, true);
    // roles that are only checked or used as a filter are never given by the bot
    assert.ok(!field('economyBoosts', 'roleId').assignable);
    assert.ok(!field('statChannels', 'roleId').assignable);
    assert.ok(!field('tempVoiceHubs', 'moderatorRoleIds').assignable);
  });
});

// ── Copied limits stay in step with their services ───────────────────────────

describe('drift guards', () => {
  test('level rewards: the reachable level and XP maths match xpService and the command', async () => {
    const lr = await import('../src/dashboard/collections/levelRoles.js');
    const xp = await import('../src/services/xpService.js');
    assert.equal(lr.highestReachableLevel(), xp.levelFromXp(XP_MAX));
    for (const level of [0, 1, 2, 10, 100, 500, 843, 999]) assert.equal(lr.totalXpForLevel(level), xp.totalXpForLevel(level), `level ${level}`);
    const cmd = await import('../src/commands/admin/levelconfig.js');
    const roleGroup = cmd.data.toJSON().options.find((o) => o.name === 'role');
    const add = roleGroup.options.find((o) => o.name === 'add');
    assert.equal(add.options.find((o) => o.name === 'level').max_value, lr.LEVEL_INPUT_MAX);
  });

  test('shop items and boosts: limits match economyService and the command', async () => {
    const eco = await import('../src/services/economyService.js');
    const shop = await import('../src/dashboard/collections/shopItems.js');
    const boosts = await import('../src/dashboard/collections/economyBoosts.js');
    assert.equal(shop.ITEM_NAME_MAX, eco.ITEM_NAME_MAX);
    assert.equal(shop.ITEM_DESCRIPTION_MAX, eco.ITEM_DESCRIPTION_MAX);
    assert.equal(shop.ITEM_MESSAGE_MAX, eco.ITEM_MESSAGE_MAX);
    assert.equal(shop.ITEM_IMAGE_MAX, eco.ITEM_IMAGE_MAX);
    assert.equal(shop.MAX_STOCK, eco.MAX_STOCK);
    assert.equal(boosts.BOOST_NAME_MAX, eco.BOOST_NAME_MAX);
    assert.equal(boosts.BOOST_PERCENT_MAX, eco.BOOST_PERCENT_MAX);
    const types = COLLECTIONS.shopItems.fields.find((f) => f.field === 'type').options.map((o) => o.value);
    assert.deepEqual(types, Object.keys(eco.ITEM_TYPE_LABELS));
    const periods = COLLECTIONS.economyBoosts.fields.find((f) => f.field === 'period').options.map((o) => o.value);
    assert.deepEqual(periods, Object.keys(eco.BOOST_PERIOD_LABELS));
    // the message variables are the ones itemVars fills in
    const vars = Object.keys(eco.itemVars({ name: 'a', price: 1 }, { settings: null })).map((k) => `{${k}}`).sort();
    assert.deepEqual([...shop.ITEM_MESSAGE_TOKENS].sort(), vars);
  });

  test('achievements: limits match achievementService', async () => {
    const svc = await import('../src/services/achievementService.js');
    const a = await import('../src/dashboard/collections/achievements.js');
    assert.equal(a.NAME_MAX, svc.NAME_MAX);
    assert.equal(a.DESCRIPTION_MAX, svc.DESCRIPTION_MAX);
    assert.equal(a.GOAL_INPUT_MAX, svc.GOAL_INPUT_MAX);
    assert.equal(a.MAX_REWARD_AMOUNT, svc.MAX_REWARD_AMOUNT);
    assert.equal(a.DEADLINE_MIN_MS, svc.DEADLINE_MIN_MS);
    assert.equal(a.DEADLINE_MAX_MS, svc.DEADLINE_MAX_MS);
    assert.deepEqual({ ...a.GOAL_MAX }, { ...svc.GOAL_MAX });
    assert.deepEqual({ ...a.TRIGGER_LABELS }, { ...svc.TRIGGER_LABELS });
    assert.deepEqual([...a.TIER_NAMES], [...svc.TIER_NAMES]);
    assert.deepEqual(COLLECTIONS.achievements.fields.find((f) => f.field === 'trigger').options.map((o) => o.value), [...svc.TRIGGERS]);
  });

  test('starboards: limits match starboardService and the command', async () => {
    const svc = await import('../src/services/starboardService.js');
    const sb = await import('../src/dashboard/collections/starboards.js');
    assert.equal(sb.MAX_EMOJIS_PER_BOARD, svc.MAX_EMOJIS_PER_BOARD);
    assert.equal(sb.MAX_IGNORED_CHANNELS, svc.MAX_IGNORED_CHANNELS);
    const cmd = await import('../src/commands/admin/starboard.js');
    const create = cmd.data.toJSON().options.find((o) => o.name === 'create');
    const opt = (name) => create.options.find((o) => o.name === name);
    const f = (name) => COLLECTIONS.starboards.fields.find((d) => d.field === name);
    assert.equal(f('threshold').min, opt('threshold').min_value);
    assert.equal(f('threshold').max, opt('threshold').max_value);
    assert.equal(f('maxMessageAgeDays').max, opt('max_age_days').max_value);
    assert.equal(f('name').maxLength, opt('name').max_length);
    // everything a board name may contain survives normalizeBoardName unchanged
    for (const name of ['stars', 'a-b_c', 'x9']) assert.equal(svc.normalizeBoardName(name), name);
  });

  test('translated channels: the language table and target count match translationService and the command', async () => {
    const svc = await import('../src/services/translationService.js');
    const tc = await import('../src/dashboard/collections/translateChannels.js');
    assert.deepEqual({ ...tc.TRANSLATE_LANGUAGES }, { ...svc.LANGUAGES });
    const cmd = await import('../src/commands/admin/autotranslate.js');
    const languageOptions = cmd.data.toJSON().options.filter((o) => /^language\d$/.test(o.name));
    assert.equal(tc.MAX_TARGETS, languageOptions.length);
    // every listed code is canonical for the service
    for (const code of Object.keys(tc.TRANSLATE_LANGUAGES)) assert.equal(svc.resolveLanguageInput(code), code, code);
  });

  test('voice hubs: limits match tempVoiceService and the command', async () => {
    const svc = await import('../src/services/tempVoiceService.js');
    const hubs = await import('../src/dashboard/collections/tempVoiceHubs.js');
    assert.equal(hubs.DEFAULT_NAME_FORMAT, svc.DEFAULT_NAME_FORMAT);
    assert.equal(hubs.MAX_HUB_ROLE_LIST, svc.MAX_HUB_ROLE_LIST);
    const cmd = await import('../src/commands/admin/voice-hub.js');
    const create = cmd.data.toJSON().options.find((o) => o.name === 'create');
    const opt = (name) => create.options.find((o) => o.name === name);
    const f = (name) => COLLECTIONS.tempVoiceHubs.fields.find((d) => d.field === name);
    assert.equal(f('userLimit').max, opt('limit').max_value);
    assert.equal(f('bitrate').min, opt('bitrate').min_value);
    assert.equal(f('bitrate').max, opt('bitrate').max_value);
    assert.equal(f('keepAliveMinutes').max, opt('keep_alive').max_value);
    assert.equal(f('ownershipLockMinutes').max, opt('ownership_lock').max_value);
    assert.equal(f('nameFormat').maxLength, opt('name_format').max_length);
  });

  test('counter channels: kinds, labels and default templates match statsService', async () => {
    const svc = await import('../src/services/statsService.js');
    const sc = await import('../src/dashboard/collections/statChannels.js');
    assert.deepEqual(JSON.parse(JSON.stringify(sc.KIND_INFO)), JSON.parse(JSON.stringify(svc.KIND_INFO)));
    assert.deepEqual(Object.keys(sc.KIND_INFO), [...svc.STAT_KINDS]);
    assert.equal(sc.MAX_NAME_LENGTH, svc.MAX_NAME_LENGTH);
    assert.equal(sc.normalizeTemplate('  a   b '), svc.normalizeTemplate('  a   b '));
    // every default template is valid for its own kind
    for (const [kind, info] of Object.entries(sc.KIND_INFO)) {
      assert.equal(svc.validateStatTemplate(info.template, { kind, roleName: 'Role' }).ok, true, kind);
    }
  });
});

// ── Level rewards ────────────────────────────────────────────────────────────

describe('levelRoles requests', () => {
  test('create takes a level and a role', () => {
    const { data, refs } = accept('levelRoles', { level: 5, roleId: ROLE });
    assert.deepEqual(data, { level: 5, roleId: ROLE });
    assert.deepEqual(refs.roles.map((r) => [r.id, r.assignable]), [[ROLE, true]]);
  });

  test('both fields are required', () => {
    refuse('levelRoles', { level: 5 }, create, { path: 'roleId' });
    refuse('levelRoles', { roleId: ROLE }, create, { path: 'level' });
  });

  test('levels outside 1 to 999 and non-integers are refused', () => {
    for (const level of [0, -1, 1000, 2.5, '5', null]) refuse('levelRoles', { level, roleId: ROLE }, create, { path: 'level' });
  });

  test('a level nobody can reach is refused with the highest reachable level', async () => {
    const { highestReachableLevel } = await import('../src/dashboard/collections/levelRoles.js');
    const top = highestReachableLevel();
    assert.ok(top > 100 && top < 999);
    assert.equal(accept('levelRoles', { level: top, roleId: ROLE }).data.level, top);
    refuse('levelRoles', { level: top + 1, roleId: ROLE }, create, { path: 'level', message: new RegExp(`highest reachable level is ${top}`) });
  });

  test('update changes the level or the role; unknown fields are refused', () => {
    assert.deepEqual(accept('levelRoles', { level: 7 }, update({ level: 5, roleId: ROLE })).data, { level: 7 });
    assert.deepEqual(accept('levelRoles', { roleId: ROLE2 }, update({ level: 5, roleId: ROLE })).data, { roleId: ROLE2 });
    refuse('levelRoles', { title: 'x' }, update({ level: 5, roleId: ROLE }), { path: 'title' });
    refuse('levelRoles', { level: 5000 }, update({ level: 5, roleId: ROLE }), { path: 'level' });
  });
});

// ── Shop items ───────────────────────────────────────────────────────────────

describe('shopItems requests', () => {
  test('a name and a price are enough; the rest takes its defaults', () => {
    const { data } = accept('shopItems', { name: 'Cookie', price: 50 });
    assert.deepEqual(data, {
      name: 'Cookie', price: 50, type: 'STATIC', roleId: null, description: null, imageUrl: null, useOnBuy: false, destroyOnUse: true,
      canBuyMultiple: true, maxPerUser: LIMITS.ITEM_COPIES_PER_USER, stock: null, buyMessage: null, useMessage: null, enabled: true,
    });
  });

  test('name and price limits', () => {
    refuse('shopItems', { price: 5 }, create, { path: 'name' });
    refuse('shopItems', { name: '', price: 5 }, create, { path: 'name' });
    refuse('shopItems', { name: 'x'.repeat(51), price: 5 }, create, { path: 'name' });
    refuse('shopItems', { name: 'two\nlines', price: 5 }, create, { path: 'name' });
    refuse('shopItems', { name: 'none', price: 5 }, create, { path: 'name' });
    refuse('shopItems', { name: ' None ', price: 5 }, create, { path: 'name' });
    refuse('shopItems', { name: 'a' }, create, { path: 'price' });
    for (const price of [0, -5, COIN_MAX + 1, 1.5]) refuse('shopItems', { name: 'a', price }, create, { path: 'price' });
    assert.equal(accept('shopItems', { name: 'a', price: COIN_MAX }).data.price, COIN_MAX);
  });

  test('a Role item needs a role, and only a Role item can have one', () => {
    refuse('shopItems', { name: 'a', price: 5, type: 'ROLE' }, create, { path: 'roleId' });
    const ok = accept('shopItems', { name: 'a', price: 5, type: 'ROLE', roleId: ROLE });
    assert.deepEqual(ok.refs.roles.map((r) => [r.id, r.assignable]), [[ROLE, true]]);
    refuse('shopItems', { name: 'a', price: 5, type: 'USABLE', roleId: ROLE }, create, { path: 'roleId' });
    refuse('shopItems', { name: 'a', price: 5, roleId: ROLE }, create, { path: 'roleId' });
  });

  test('a collectible cannot be used on purchase', () => {
    refuse('shopItems', { name: 'a', price: 5, useOnBuy: true }, create, { path: 'useOnBuy' });
    assert.equal(accept('shopItems', { name: 'a', price: 5, type: 'USABLE', useOnBuy: true }).data.useOnBuy, true);
  });

  test('stock: empty is unlimited, 0 only after creation', () => {
    assert.equal(accept('shopItems', { name: 'a', price: 5, stock: 3 }).data.stock, 3);
    assert.equal(accept('shopItems', { name: 'a', price: 5, stock: null }).data.stock, null);
    refuse('shopItems', { name: 'a', price: 5, stock: 0 }, create, { path: 'stock' });
    assert.equal(accept('shopItems', { stock: 0 }, update({ type: 'STATIC', roleId: null, stock: 5 })).data.stock, 0);
    refuse('shopItems', { stock: 1_000_001 }, update({ type: 'STATIC' }), { path: 'stock' });
  });

  test('text and number limits', () => {
    refuse('shopItems', { name: 'a', price: 5, description: 'x'.repeat(201) }, create, { path: 'description' });
    refuse('shopItems', { name: 'a', price: 5, buyMessage: 'x'.repeat(1001) }, create, { path: 'buyMessage' });
    refuse('shopItems', { name: 'a', price: 5, useMessage: 'x'.repeat(1001) }, create, { path: 'useMessage' });
    refuse('shopItems', { name: 'a', price: 5, imageUrl: 'ftp://example.com/a.png' }, create, { path: 'imageUrl' });
    refuse('shopItems', { name: 'a', price: 5, imageUrl: 'not a url' }, create, { path: 'imageUrl' });
    refuse('shopItems', { name: 'a', price: 5, maxPerUser: LIMITS.ITEM_COPIES_PER_USER + 1 }, create, { path: 'maxPerUser' });
    refuse('shopItems', { name: 'a', price: 5, maxPerUser: 0 }, create, { path: 'maxPerUser' });
    assert.equal(accept('shopItems', { name: 'a', price: 5, imageUrl: 'https://example.com/a.png' }).data.imageUrl, 'https://example.com/a.png');
    assert.equal(accept('shopItems', { name: 'a', price: 5, description: '   ' }).data.description, null);
  });

  test('update checks the merged item', () => {
    const role = { name: 'r', price: 5, type: 'ROLE', roleId: ROLE, useOnBuy: true, stock: null };
    // changing away from Role is fine (the store clears the role), keeping use-on-buy on a collectible is not
    assert.deepEqual(accept('shopItems', { type: 'USABLE' }, update(role)).data, { type: 'USABLE' });
    refuse('shopItems', { type: 'STATIC' }, update(role), { path: 'useOnBuy' });
    assert.deepEqual(accept('shopItems', { type: 'STATIC', useOnBuy: false }, update(role)).data, { type: 'STATIC', useOnBuy: false });
    refuse('shopItems', { roleId: null }, update(role), { path: 'roleId' });
    refuse('shopItems', { type: 'ROLE' }, update({ ...role, roleId: null }), { path: 'roleId' });
    refuse('shopItems', { createdAt: '2020-01-01T00:00:00.000Z' }, update(role), { path: 'createdAt' });
  });
});

// ── Coin boosts ──────────────────────────────────────────────────────────────

describe('economyBoosts requests', () => {
  test('create', () => {
    const { data } = accept('economyBoosts', { name: 'Nitro', percent: 50, period: 'DAILY', roleId: ROLE });
    assert.deepEqual(data, { name: 'Nitro', percent: 50, period: 'DAILY', roleId: ROLE, enabled: true });
    const open = accept('economyBoosts', { name: 'Everyone', percent: 5, period: 'ONE_TIME' }).data;
    assert.equal(open.roleId, null);
  });

  test('limits', () => {
    for (const percent of [0, 501, 1.5, '10']) refuse('economyBoosts', { name: 'a', percent, period: 'DAILY' }, create, { path: 'percent' });
    for (const period of ['YEARLY', 'daily', 5]) refuse('economyBoosts', { name: 'a', percent: 5, period }, create, { path: 'period' });
    refuse('economyBoosts', { name: 'x'.repeat(33), percent: 5, period: 'DAILY' }, create, { path: 'name' });
    refuse('economyBoosts', { name: '', percent: 5, period: 'DAILY' }, create, { path: 'name' });
    refuse('economyBoosts', { percent: 5, period: 'DAILY' }, create, { path: 'name' });
    assert.equal(accept('economyBoosts', { name: 'a', percent: 500, period: 'WEEKLY' }).data.percent, 500);
  });

  test('update cannot change the period', () => {
    assert.deepEqual(accept('economyBoosts', { percent: 80, enabled: false }, update()).data, { percent: 80, enabled: false });
    refuse('economyBoosts', { period: 'MONTHLY' }, update(), { path: 'period' });
    assert.equal(accept('economyBoosts', { roleId: null }, update()).data.roleId, null);
  });
});

// ── Achievements ─────────────────────────────────────────────────────────────

const tier = (goal, extra = {}) => ({ goal, roleAdd: null, roleRemove: null, xp: null, coins: null, ...extra });
const achievement = (extra = {}) => ({ name: 'Chatterbox', trigger: 'MESSAGES', tiers: [tier(100), tier(500)], ...extra });

describe('achievements requests', () => {
  test('create with tiers and rewards', () => {
    const { data, refs } = accept('achievements', achievement({
      description: 'Talk a lot', emoji: '💬', tiers: [tier(100, { roleAdd: ROLE, xp: 50 }), tier(500, { roleRemove: ROLE2, coins: 20 })],
    }));
    assert.equal(data.name, 'Chatterbox');
    assert.equal(data.trigger, 'MESSAGES');
    assert.equal(data.trackPastProgress, true);
    assert.equal(data.deadline, null);
    assert.equal(data.enabled, true);
    assert.deepEqual(data.tiers[0], tier(100, { roleAdd: ROLE, xp: 50 }));
    assert.deepEqual(refs.roles.map((r) => r.id).sort(), [ROLE, ROLE2]);
    assert.ok(refs.roles.every((r) => r.assignable));
  });

  test('name, trigger and tiers are required; the badge must be an emoji', () => {
    refuse('achievements', { trigger: 'MESSAGES', tiers: [tier(1)] }, create, { path: 'name' });
    refuse('achievements', { name: 'a', tiers: [tier(1)] }, create, { path: 'trigger' });
    refuse('achievements', { name: 'a', trigger: 'MESSAGES' }, create, { path: 'tiers' });
    refuse('achievements', achievement({ name: 'x'.repeat(51) }), create, { path: 'name' });
    refuse('achievements', achievement({ description: 'x'.repeat(201) }), create, { path: 'description' });
    refuse('achievements', achievement({ trigger: 'VOICE' }), create, { path: 'trigger' });
    refuse('achievements', achievement({ emoji: 'trophy' }), create, { path: 'emoji' });
    assert.equal(accept('achievements', achievement({ emoji: null })).data.emoji, null);
  });

  test('one to four tiers with goals that go up', () => {
    refuse('achievements', achievement({ tiers: [] }), create, { path: 'tiers' });
    refuse('achievements', achievement({ tiers: [tier(1), tier(2), tier(3), tier(4), tier(5)] }), create, { path: 'tiers' });
    assert.equal(accept('achievements', achievement({ tiers: [tier(1), tier(2), tier(3), tier(4)] })).data.tiers.length, 4);
    refuse('achievements', achievement({ tiers: [tier(100), tier(100)] }), create, { path: 'tiers.1.goal' });
    refuse('achievements', achievement({ tiers: [tier(100), tier(50)] }), create, { path: 'tiers.1.goal' });
    refuse('achievements', achievement({ tiers: [tier(0)] }), create, { path: 'tiers.0.goal' });
    refuse('achievements', achievement({ tiers: [tier(1.5)] }), create, { path: 'tiers.0.goal' });
    refuse('achievements', achievement({ tiers: [{ goal: 5, bonus: 1 }] }), create, { path: 'tiers.0.bonus' });
  });

  test('the highest goal depends on the trigger', () => {
    refuse('achievements', achievement({ trigger: 'LEVEL', tiers: [tier(1001)] }), create, { path: 'tiers.0.goal' });
    assert.equal(accept('achievements', achievement({ trigger: 'LEVEL', tiers: [tier(1000)] })).data.tiers[0].goal, 1000);
    refuse('achievements', achievement({ trigger: 'BOOST', tiers: [tier(1), tier(101)] }), create, { path: 'tiers.1.goal' });
    refuse('achievements', achievement({ trigger: 'PURCHASES', tiers: [tier(1_000_001)] }), create, { path: 'tiers.0.goal' });
    refuse('achievements', achievement({ tiers: [tier(10_000_001)] }), create, { path: 'tiers.0.goal' });
    assert.equal(accept('achievements', achievement({ tiers: [tier(10_000_000)] })).data.tiers[0].goal, 10_000_000);
  });

  test('rewards: amounts are limited and a tier cannot give and take the same role', () => {
    refuse('achievements', achievement({ tiers: [tier(1, { xp: 0 })] }), create, { path: 'tiers.0.xp' });
    refuse('achievements', achievement({ tiers: [tier(1, { coins: 1_000_001 })] }), create, { path: 'tiers.0.coins' });
    refuse('achievements', achievement({ tiers: [tier(1, { roleAdd: 'x' })] }), create, { path: 'tiers.0.roleAdd' });
    refuse('achievements', achievement({ tiers: [tier(1), tier(2, { roleAdd: ROLE, roleRemove: ROLE })] }), create, { path: 'tiers.1.roleRemove' });
    assert.equal(accept('achievements', achievement({ tiers: [tier(1, { xp: 1_000_000 })] })).data.tiers[0].xp, 1_000_000);
  });

  test('deadline: between a minute and five years ahead', () => {
    const at = (ms) => new Date(Date.now() + ms).toISOString();
    assert.ok(accept('achievements', achievement({ deadline: at(86_400_000) })).data.deadline);
    refuse('achievements', achievement({ deadline: at(-1000) }), create, { path: 'deadline' });
    refuse('achievements', achievement({ deadline: at(10_000) }), create, { path: 'deadline' });
    refuse('achievements', achievement({ deadline: at(6 * 365 * 86_400_000) }), create, { path: 'deadline' });
    refuse('achievements', achievement({ deadline: 'tomorrow-ish' }), create, { path: 'deadline' });
  });

  test('update: the trigger is fixed, goals are checked against the stored trigger', () => {
    const current = { trigger: 'LEVEL', tiers: [tier(10)], deadline: null };
    refuse('achievements', { trigger: 'MESSAGES' }, update(current), { path: 'trigger' });
    refuse('achievements', { tiers: [tier(1500)] }, update(current), { path: 'tiers.0.goal' });
    assert.deepEqual(accept('achievements', { tiers: [tier(20), tier(40)] }, update(current)).data.tiers.map((t) => t.goal), [20, 40]);
    assert.deepEqual(accept('achievements', { enabled: false }, update(current)).data, { enabled: false });
  });

  test('update: a deadline that is unchanged stays valid even when it has passed', () => {
    const past = new Date(Date.now() - 86_400_000).toISOString();
    const current = { trigger: 'MESSAGES', tiers: [tier(10)], deadline: past };
    assert.equal(accept('achievements', { deadline: past, enabled: true }, update(current)).data.deadline, past);
    refuse('achievements', { deadline: new Date(Date.now() - 1000).toISOString() }, update(current), { path: 'deadline' });
    assert.equal(accept('achievements', { deadline: null }, update(current)).data.deadline, null);
  });
});

// ── Starboards ───────────────────────────────────────────────────────────────

const board = (extra = {}) => ({ name: 'stars', channelId: CHAN, ...extra });

describe('starboards requests', () => {
  test('a name and a channel are enough; the rest takes the command defaults', () => {
    const { data, refs } = accept('starboards', board());
    assert.deepEqual(data, {
      name: 'stars', channelId: CHAN, emojis: ['⭐'], threshold: 3, selfStar: false, removeSelfStars: false, ignoreBots: true,
      removeBelowThreshold: true, autoReact: false, maxMessageAgeDays: 0, allowNsfw: false, ignoredChannelIds: [], enabled: true,
    });
    assert.deepEqual(refs.channels.map((c) => [c.id, c.kinds.join('+')]), [[CHAN, 'text+announcement']]);
  });

  test('names use lowercase letters, digits, - and _ (up to 32)', () => {
    for (const name of ['Stars', 'has space', 'ünï', 'a'.repeat(33), '', 'a!b']) refuse('starboards', board({ name }), create, { path: 'name' });
    assert.equal(accept('starboards', board({ name: 'a_b-9' })).data.name, 'a_b-9');
    refuse('starboards', { channelId: CHAN }, create, { path: 'name' });
    refuse('starboards', { name: 'a' }, create, { path: 'channelId' });
  });

  test('threshold and age limits', () => {
    for (const threshold of [0, 501, 2.5]) refuse('starboards', board({ threshold }), create, { path: 'threshold' });
    assert.equal(accept('starboards', board({ threshold: 500 })).data.threshold, 500);
    for (const maxMessageAgeDays of [-1, 3651]) refuse('starboards', board({ maxMessageAgeDays }), create, { path: 'maxMessageAgeDays' });
    assert.equal(accept('starboards', board({ maxMessageAgeDays: 3650 })).data.maxMessageAgeDays, 3650);
  });

  test('emojis: unicode, custom (<:name:id> or the id), empty for any, at most ten', () => {
    assert.deepEqual(accept('starboards', board({ emojis: ['⭐', '🌟', '❤️', '<:blob:123456789012345678>', '223456789012345678'] })).data.emojis,
      ['⭐', '🌟', '❤️', '<:blob:123456789012345678>', '223456789012345678']);
    assert.deepEqual(accept('starboards', board({ emojis: [] })).data.emojis, []);
    refuse('starboards', board({ emojis: ['star'] }), create, { path: 'emojis.0' });
    refuse('starboards', board({ emojis: ['⭐', ':blob:'] }), create, { path: 'emojis.1' });
    refuse('starboards', board({ emojis: ['⭐', '12345'] }), create, { path: 'emojis.1' });
    const eleven = ['⭐', '🌟', '❤️', '🔥', '👍', '🎉', '😂', '😮', '😢', '😡', '🙏'];
    refuse('starboards', board({ emojis: eleven }), create, { path: 'emojis' });
    assert.equal(accept('starboards', board({ emojis: eleven.slice(0, 10) })).data.emojis.length, 10);
  });

  test('counting and removing the author\'s own reaction exclude each other', () => {
    refuse('starboards', board({ selfStar: true, removeSelfStars: true }), create, { path: 'removeSelfStars' });
    assert.equal(accept('starboards', board({ selfStar: true })).data.selfStar, true);
    const current = { selfStar: true, removeSelfStars: false, channelId: CHAN };
    refuse('starboards', { removeSelfStars: true }, update(current), { path: 'removeSelfStars' });
    assert.equal(accept('starboards', { removeSelfStars: true, selfStar: false }, update(current)).data.removeSelfStars, true);
    refuse('starboards', { selfStar: true }, update({ selfStar: false, removeSelfStars: true }), { path: 'removeSelfStars' });
  });

  test('ignored channels: any kind of source channel, at most fifty', () => {
    const { refs } = accept('starboards', board({ ignoredChannelIds: [CHAN2, CAT] }));
    assert.equal(refs.channels.length, 3);
    const ignored = refs.channels.find((c) => c.id === CAT);
    assert.ok(ignored.kinds.includes('category') && ignored.kinds.includes('voice') && ignored.kinds.includes('forum'));
    const many = Array.from({ length: 51 }, (_, i) => `5000000000000000${String(i).padStart(2, '0')}`);
    refuse('starboards', board({ ignoredChannelIds: many }), create, { path: 'ignoredChannelIds' });
    assert.equal(accept('starboards', board({ ignoredChannelIds: many.slice(0, 50) })).data.ignoredChannelIds.length, 50);
  });

  test('update takes any subset and refuses read-only fields', () => {
    assert.deepEqual(accept('starboards', { threshold: 5, enabled: false }, update({ selfStar: false, removeSelfStars: false })).data,
      { threshold: 5, enabled: false });
    refuse('starboards', { reposts: 5 }, update({}), { path: 'reposts' });
  });
});

// ── Translated channels ──────────────────────────────────────────────────────

describe('translateChannels requests', () => {
  test('create takes a channel and one to five languages', () => {
    const { data, refs } = accept('translateChannels', { channelId: CHAN, targetLangs: ['es', 'ja', 'zh-tw'] });
    assert.deepEqual(data, { channelId: CHAN, targetLangs: ['es', 'ja', 'zh-tw'] });
    assert.deepEqual(refs.channels.map((c) => c.kinds.join('+')), ['text+announcement+voice+stage']);
  });

  test('languages must be known codes, at least one, at most five', () => {
    refuse('translateChannels', { channelId: CHAN, targetLangs: [] }, create, { path: 'targetLangs' });
    refuse('translateChannels', { channelId: CHAN }, create, { path: 'targetLangs' });
    refuse('translateChannels', { channelId: CHAN, targetLangs: ['xx'] }, create, { path: 'targetLangs.0' });
    refuse('translateChannels', { channelId: CHAN, targetLangs: ['Spanish'] }, create, { path: 'targetLangs.0' });
    refuse('translateChannels', { channelId: CHAN, targetLangs: ['es', 'fr', 'de', 'it', 'pt', 'nl'] }, create, { path: 'targetLangs' });
    assert.equal(accept('translateChannels', { channelId: CHAN, targetLangs: ['es', 'fr', 'de', 'it', 'pt'] }).data.targetLangs.length, 5);
    assert.deepEqual(accept('translateChannels', { channelId: CHAN, targetLangs: ['es', 'es'] }).data.targetLangs, ['es']);
    refuse('translateChannels', { targetLangs: ['es'] }, create, { path: 'channelId' });
    refuse('translateChannels', { channelId: 'general', targetLangs: ['es'] }, create, { path: 'channelId' });
  });

  test('update changes the languages only', () => {
    assert.deepEqual(accept('translateChannels', { targetLangs: ['de'] }, update()).data, { targetLangs: ['de'] });
    refuse('translateChannels', { channelId: CHAN2 }, update(), { path: 'channelId' });
    refuse('translateChannels', { targetLangs: [] }, update(), { path: 'targetLangs' });
  });
});

// ── Voice hubs ───────────────────────────────────────────────────────────────

describe('tempVoiceHubs requests', () => {
  test('a channel is enough; the rest takes the command defaults', () => {
    const { data, refs } = accept('tempVoiceHubs', { channelId: CHAN });
    assert.deepEqual(data, {
      channelId: CHAN, categoryId: null, nameFormat: "#{index} - {username}'s Channel", userLimit: 0, bitrate: null, keepAliveMinutes: 0,
      ownershipLockMinutes: 0, syncPermissions: true, createTextChannel: false, textChannelMembersOnly: true, ignoredRoleIds: [],
      moderatorRoleIds: [],
    });
    assert.deepEqual(refs.channels.map((c) => c.kinds.join('+')), ['voice']);
  });

  test('numbers use the command ranges', () => {
    const hub = (extra) => ({ channelId: CHAN, ...extra });
    for (const userLimit of [-1, 100, 1.5]) refuse('tempVoiceHubs', hub({ userLimit }), create, { path: 'userLimit' });
    for (const bitrate of [7, 385, 64.5]) refuse('tempVoiceHubs', hub({ bitrate }), create, { path: 'bitrate' });
    for (const keepAliveMinutes of [-1, 10081]) refuse('tempVoiceHubs', hub({ keepAliveMinutes }), create, { path: 'keepAliveMinutes' });
    for (const ownershipLockMinutes of [-1, 1441]) refuse('tempVoiceHubs', hub({ ownershipLockMinutes }), create, { path: 'ownershipLockMinutes' });
    assert.deepEqual(
      accept('tempVoiceHubs', hub({ userLimit: 99, bitrate: 384, keepAliveMinutes: 10080, ownershipLockMinutes: 1440 })).data.userLimit, 99);
    assert.equal(accept('tempVoiceHubs', hub({ bitrate: 8 })).data.bitrate, 8);
  });

  test('name format: 1 to 100 characters and no mentions', () => {
    const hub = (nameFormat) => ({ channelId: CHAN, nameFormat });
    refuse('tempVoiceHubs', hub(''), create, { path: 'nameFormat' });
    refuse('tempVoiceHubs', hub('x'.repeat(101)), create, { path: 'nameFormat' });
    refuse('tempVoiceHubs', hub("{user}'s room"), create, { path: 'nameFormat', message: /mentions/ });
    refuse('tempVoiceHubs', hub('{user.mention} room'), create, { path: 'nameFormat' });
    refuse('tempVoiceHubs', hub('room <@123456789012345678>'), create, { path: 'nameFormat' });
    assert.equal(accept('tempVoiceHubs', hub("{user.name}'s room #{index}")).data.nameFormat, "{user.name}'s room #{index}");
    refuse('tempVoiceHubs', { nameFormat: '{user}' }, update(), { path: 'nameFormat' });
  });

  test('role lists hold at most ten roles', () => {
    const roles = Array.from({ length: 11 }, (_, i) => `60000000000000000${i}`.slice(-18));
    refuse('tempVoiceHubs', { channelId: CHAN, ignoredRoleIds: roles }, create, { path: 'ignoredRoleIds' });
    refuse('tempVoiceHubs', { channelId: CHAN, moderatorRoleIds: roles }, create, { path: 'moderatorRoleIds' });
    const { refs } = accept('tempVoiceHubs', { channelId: CHAN, ignoredRoleIds: roles.slice(0, 10), moderatorRoleIds: [ROLE] });
    assert.equal(refs.roles.length, 11);
    assert.ok(refs.roles.every((r) => !r.assignable));
  });

  test('the category must be a category; the hub channel cannot change', () => {
    const { refs } = accept('tempVoiceHubs', { channelId: CHAN, categoryId: CAT });
    assert.deepEqual(refs.channels.map((c) => [c.id, c.kinds.join('+')]), [[CHAN, 'voice'], [CAT, 'category']]);
    refuse('tempVoiceHubs', { channelId: CHAN2 }, update(), { path: 'channelId' });
    assert.deepEqual(accept('tempVoiceHubs', { categoryId: null, userLimit: 5 }, update()).data, { categoryId: null, userLimit: 5 });
  });
});

// ── Counter channels ─────────────────────────────────────────────────────────

describe('statChannels requests', () => {
  test('create a members counter with the default name', () => {
    const { data, refs } = accept('statChannels', { kind: 'MEMBERS', channelId: CHAN });
    assert.deepEqual(data, { kind: 'MEMBERS', channelId: CHAN, roleId: null, template: null });
    assert.deepEqual(refs.channels.map((c) => c.kinds.join('+')), ['voice']);
  });

  test('role counters need a role; the other kinds refuse one', () => {
    refuse('statChannels', { kind: 'ROLE_MEMBERS', channelId: CHAN }, create, { path: 'roleId' });
    const ok = accept('statChannels', { kind: 'ROLE_MEMBERS', channelId: CHAN, roleId: ROLE, template: '{role}: {count}' });
    assert.equal(ok.data.roleId, ROLE);
    assert.deepEqual(ok.refs.roles.map((r) => [r.id, r.assignable, r.allowEveryone]), [[ROLE, false, false]]);
    refuse('statChannels', { kind: 'MEMBERS', channelId: CHAN, roleId: ROLE }, create, { path: 'roleId' });
  });

  test('the kind and the channel are required and valid', () => {
    refuse('statChannels', { channelId: CHAN }, create, { path: 'kind' });
    refuse('statChannels', { kind: 'FOLLOWERS', channelId: CHAN }, create, { path: 'kind' });
    refuse('statChannels', { kind: 'MEMBERS' }, create, { path: 'channelId' });
    for (const kind of Object.keys(COLLECTIONS.statChannels.fields.find((f) => f.field === 'kind').options.reduce((o, x) => ({ ...o, [x.value]: 1 }), {}))) {
      const roleId = kind === 'ROLE_MEMBERS' ? ROLE : undefined;
      assert.equal(accept('statChannels', { kind, channelId: CHAN, ...(roleId ? { roleId } : {}) }).data.kind, kind);
    }
  });

  test('the name needs {count}, allows {role} only for role counters and no random values', () => {
    const counter = (template, extra = {}) => ({ kind: 'MEMBERS', channelId: CHAN, template, ...extra });
    refuse('statChannels', counter('Members'), create, { path: 'template', message: /\{count\}/ });
    refuse('statChannels', counter('{role}: {count}'), create, { path: 'template', message: /Role members/ });
    refuse('statChannels', counter('{count} {random:1:5}'), create, { path: 'template', message: /Random/ });
    refuse('statChannels', counter('x'.repeat(101)), create, { path: 'template' });
    assert.equal(accept('statChannels', counter('👥  Members:   {COUNT}')).data.template, '👥  Members:   {COUNT}');
    assert.equal(accept('statChannels', counter('   ')).data.template, null);
  });

  test('update changes the name only; empty means the default', () => {
    const current = { kind: 'ROLE_MEMBERS', roleId: ROLE, channelId: CHAN, template: '{role}: {count}' };
    assert.deepEqual(accept('statChannels', { template: 'Staff: {count}' }, update(current)).data, { template: 'Staff: {count}' });
    assert.deepEqual(accept('statChannels', { template: null }, update(current)).data, { template: null });
    refuse('statChannels', { template: 'Staff' }, update(current), { path: 'template' });
    refuse('statChannels', { template: '{role}: {count}' }, update({ ...current, kind: 'MEMBERS', roleId: null }), { path: 'template' });
    refuse('statChannels', { kind: 'BOTS' }, update(current), { path: 'kind' });
    refuse('statChannels', { channelId: CHAN2 }, update(current), { path: 'channelId' });
    refuse('statChannels', { roleId: ROLE2 }, update(current), { path: 'roleId' });
  });

  test('the refresh action takes no input', () => {
    assert.deepEqual(accept('statChannels', {}, { mode: 'action', actionId: 'refresh' }).data, {});
    refuse('statChannels', { force: true }, { mode: 'action', actionId: 'refresh' }, { path: 'force' });
    assert.deepEqual(publicCollection(COLLECTIONS.statChannels).actions.map((a) => a.id), ['refresh']);
  });
});

// ── Stores against a real database (opt-in) ─────────────────────────────────

describe('stores (TEST_DATABASE_URL)', { skip: skipDb }, () => {
  const guilds = [];
  const quiet = { info() {}, warn() {}, error() {}, debug() {} };
  let n = 0;
  let stores;
  let settingsSvc;

  /** A fresh guild id (and its GuildSettings row) per test. */
  async function newGuild() {
    const id = `91${String(Date.now()).slice(-9)}${String(++n).padStart(7, '0')}`;
    guilds.push(id);
    await settingsSvc.ensureGuildSettings(id);
    return id;
  }
  const ctxFor = (extra = {}) => ({ userId: '700000000000000001', logger: quiet, discord: null, ...extra });
  const fakeDiscord = ({ channels = [], roles = [], memberCount = 42 } = {}) => ({
    getGuildResources: async (guildId) => ({ guild: { id: guildId, name: 'Test Guild', icon: null, memberCount }, channels, roles, emojis: [], bot: {} }),
  });
  const code = async (promise) => {
    try {
      await promise;
    } catch (err) {
      assert.ok(err instanceof RecordError, `expected a RecordError, got ${err?.stack ?? err}`);
      return err.code;
    }
    return 'ok';
  };
  const snow = (i) => `8${String(i).padStart(17, '0')}`;

  before(async () => {
    settingsSvc = await import('../src/services/settingsService.js');
    stores = {};
    for (const key of KEYS) stores[key] = (await import(`../src/dashboard/collections/${key}.store.js`)).store;
  });

  after(async () => {
    for (const guildId of guilds) await prisma.guildSettings.deleteMany({ where: { guildId } }).catch(() => {});
  });

  test('every store has the full interface', () => {
    for (const key of KEYS) {
      for (const fn of ['list', 'get', 'create', 'update', 'remove']) assert.equal(typeof stores[key][fn], 'function', `${key}.${fn}`);
      assert.equal(typeof stores[key].actions, 'object', `${key}.actions`);
    }
  });

  test('every raw statement the stores export is valid SQL', async () => {
    const lock = await import('../src/dashboard/collections/levelRoles.store.js');
    const ach = await import('../src/dashboard/collections/achievements.store.js');
    for (const { name, build } of [...lock.RAW_SQL, ...ach.RAW_SQL]) {
      await prisma.$transaction(async (tx) => { await tx.$queryRaw(build()); }).catch((err) => assert.fail(`${name}: ${err.message}`));
    }
  });

  test('what a store returns is accepted back as an update (the editor sends the whole form)', async () => {
    const g = await newGuild();
    const discord = fakeDiscord({ roles: [{ id: snow(40), name: 'Staff' }] });
    const t = (goal, extra = {}) => ({ goal, roleAdd: null, roleRemove: null, xp: null, coins: null, ...extra });
    const made = {
      levelRoles: await stores.levelRoles.create(g, { level: 3, roleId: snow(1) }, ctxFor()),
      shopItems: await stores.shopItems.create(g, {
        name: 'Gem', price: 5, type: 'ROLE', roleId: snow(2), description: 'Shiny', imageUrl: 'https://example.com/g.png', useOnBuy: true, destroyOnUse: false,
        canBuyMultiple: false, maxPerUser: 1, stock: 4, buyMessage: 'Bought {item}', useMessage: null, enabled: true,
      }, ctxFor()),
      economyBoosts: await stores.economyBoosts.create(g, { name: 'Boost', percent: 25, period: 'WEEKLY', roleId: snow(3), enabled: false }, ctxFor()),
      achievements: await stores.achievements.create(g, {
        name: 'Talker', description: 'Talk', emoji: '💬', trigger: 'REACTIONS', trackPastProgress: false, deadline: new Date(Date.now() + 86_400_000).toISOString(),
        enabled: true, tiers: [t(5, { roleAdd: snow(4) }), t(50, { roleRemove: snow(5) })],
      }, ctxFor()),
      starboards: await stores.starboards.create(g, {
        name: 'best', channelId: snow(6), emojis: ['⭐', '<:blob:123456789012345678>'], threshold: 4, selfStar: false, removeSelfStars: true, ignoreBots: false,
        removeBelowThreshold: false, autoReact: true, maxMessageAgeDays: 14, allowNsfw: false, ignoredChannelIds: [snow(7)], enabled: true,
      }, ctxFor()),
      translateChannels: await stores.translateChannels.create(g, { channelId: snow(8), targetLangs: ['es', 'zh-tw'] }, ctxFor()),
      tempVoiceHubs: await stores.tempVoiceHubs.create(g, {
        channelId: snow(9), categoryId: snow(10), nameFormat: 'Room {index}', userLimit: 4, bitrate: 128, keepAliveMinutes: 5, ownershipLockMinutes: 2,
        syncPermissions: false, createTextChannel: true, textChannelMembersOnly: false, ignoredRoleIds: [snow(11)], moderatorRoleIds: [snow(12)],
      }, ctxFor({ discord })),
      statChannels: await stores.statChannels.create(g, { kind: 'ROLE_MEMBERS', channelId: snow(13), roleId: snow(40), template: null }, ctxFor({ discord })),
    };
    for (const key of KEYS) {
      const item = await stores[key].get(g, made[key].id);
      assert.ok(item, key);
      const c = COLLECTIONS[key];
      const form = Object.fromEntries(c.fields.filter((f) => !c.createOnly.includes(f.field)).map((f) => [f.field, item[f.field]]));
      const { data } = accept(key, form, update(item));
      assert.deepEqual(data, form, `${key}: the validated form equals what was sent`);
      // and saving it changes nothing
      const saved = await stores[key].update(g, item.id, data, ctxFor({ discord }));
      assert.deepEqual(saved, item, `${key}: saving an unchanged form changes nothing`);
      for (const f of c.readOnly) assert.ok(Object.hasOwn(item, f.field), `${key}: item has read-only ${f.field}`);
      assert.equal(await stores[key].list(g).then((list) => list.some((x) => x.id === item.id)), true, `${key}: listed`);
    }
  });

  test('level rewards: create, the two uniques, the cap under concurrency, update, remove, other servers', async () => {
    const s = stores.levelRoles;
    const g = await newGuild();
    const g2 = await newGuild();
    const a = await s.create(g, { level: 5, roleId: snow(1) }, ctxFor());
    assert.deepEqual({ ...a, id: undefined }, { id: undefined, level: 5, roleId: snow(1), title: 'Level 5' });
    assert.equal(await code(s.create(g, { level: 5, roleId: snow(2) }, ctxFor())), 'conflict');
    assert.equal(await code(s.create(g, { level: 6, roleId: snow(1) }, ctxFor())), 'conflict');
    const b = await s.create(g, { level: 10, roleId: snow(2) }, ctxFor());
    assert.deepEqual((await s.list(g)).map((r) => r.level), [5, 10]);
    assert.equal((await s.update(g, b.id, { level: 12 }, ctxFor())).level, 12);
    assert.equal(await code(s.update(g, b.id, { level: 5 }, ctxFor())), 'conflict');
    assert.equal(await code(s.update(g, b.id, { roleId: snow(1) }, ctxFor())), 'conflict');
    assert.equal((await s.update(g, b.id, { level: 12, roleId: snow(2) }, ctxFor())).level, 12); // unchanged
    // another server's records are not found
    assert.equal(await s.get(g2, a.id), null);
    assert.equal(await code(s.update(g2, a.id, { level: 9 }, ctxFor())), 'not_found');
    assert.equal(await code(s.remove(g2, a.id, ctxFor())), 'not_found');
    await s.remove(g, a.id, ctxFor());
    assert.equal(await s.get(g, a.id), null);
    assert.equal(await code(s.remove(g, a.id, ctxFor())), 'not_found');
    // the cap holds when several requests arrive together
    const g3 = await newGuild();
    await prisma.levelRole.createMany({ data: Array.from({ length: LIMITS.LEVEL_ROLES - 2 }, (_, i) => ({ guildId: g3, level: i + 1, roleId: snow(100 + i) })) });
    const results = await Promise.all(Array.from({ length: 5 }, (_, i) => code(s.create(g3, { level: 200 + i, roleId: snow(500 + i) }, ctxFor()))));
    assert.equal(results.filter((r) => r === 'ok').length, 2, results.join());
    assert.equal(results.filter((r) => r === 'limit').length, 3, results.join());
    assert.equal(await prisma.levelRole.count({ where: { guildId: g3 } }), LIMITS.LEVEL_ROLES);
  });

  test('shop items: create, names are unique ignoring case, role items, stock, delete, caps', async () => {
    const s = stores.shopItems;
    const g = await newGuild();
    const g2 = await newGuild();
    const base = { price: 100, type: 'STATIC', roleId: null, description: null, imageUrl: null, useOnBuy: false, destroyOnUse: true,
      canBuyMultiple: true, maxPerUser: 200, stock: null, buyMessage: null, useMessage: null, enabled: true };
    const cookie = await s.create(g, { ...base, name: '  Cookie   Jar ' }, ctxFor());
    assert.equal(cookie.name, 'Cookie Jar');
    assert.match(cookie.summary, /^100 · collectible/);
    assert.equal(await code(s.create(g, { ...base, name: 'cookie jar' }, ctxFor())), 'conflict');
    assert.equal(await code(s.create(g, { ...base, name: 'none' }, ctxFor())), 'invalid');
    assert.equal((await s.create(g2, { ...base, name: 'Cookie Jar' }, ctxFor())).name, 'Cookie Jar'); // other server, same name
    const vip = await s.create(g, { ...base, name: 'VIP', type: 'ROLE', roleId: snow(1), useOnBuy: true, canBuyMultiple: false, maxPerUser: 1 }, ctxFor());
    assert.equal(vip.roleId, snow(1));
    // changing away from Role clears the role
    const plain = await s.update(g, vip.id, { type: 'USABLE' }, ctxFor());
    assert.equal(plain.type, 'USABLE');
    assert.equal(plain.roleId, null);
    assert.equal((await s.update(g, vip.id, { stock: 0 }, ctxFor())).stock, 0);
    assert.match((await s.get(g, vip.id)).summary, /sold out/);
    assert.equal(await code(s.update(g, vip.id, { name: 'COOKIE JAR' }, ctxFor())), 'conflict');
    assert.equal((await s.update(g, vip.id, { name: 'VIP' }, ctxFor())).name, 'VIP');
    assert.deepEqual((await s.list(g)).map((i) => i.name), ['Cookie Jar', 'VIP']);
    // other servers
    assert.equal(await code(s.update(g2, vip.id, { price: 1 }, ctxFor())), 'not_found');
    assert.equal(await code(s.remove(g2, vip.id, ctxFor())), 'not_found');
    // deleting removes the copies members own
    await prisma.inventoryItem.create({ data: { guildId: g, userId: snow(9), itemId: vip.id, quantity: 3 } });
    await s.remove(g, vip.id, ctxFor());
    assert.equal(await prisma.inventoryItem.count({ where: { itemId: vip.id } }), 0);
    // the cap
    const g3 = await newGuild();
    await prisma.shopItem.createMany({ data: Array.from({ length: LIMITS.SHOP_ITEMS }, (_, i) => ({ guildId: g3, name: `item ${i}`, price: 1 })) });
    assert.equal(await code(s.create(g3, { ...base, name: 'one too many' }, ctxFor())), 'limit');
  });

  test('coin boosts: create, unique names, edits keep the period, delete, cap under concurrency', async () => {
    const s = stores.economyBoosts;
    const g = await newGuild();
    const g2 = await newGuild();
    const boost = await s.create(g, { name: ' Nitro  boost ', percent: 50, period: 'DAILY', roleId: snow(1), enabled: true }, ctxFor());
    assert.equal(boost.name, 'Nitro boost');
    assert.equal(boost.period, 'DAILY');
    assert.equal(boost.uses, 0);
    assert.match(boost.summary, /^\+50% · daily · one role/);
    assert.equal(await code(s.create(g, { name: 'NITRO BOOST', percent: 5, period: 'DAILY', roleId: null, enabled: true }, ctxFor())), 'conflict');
    const open = await s.create(g, { name: 'Everyone', percent: 10, period: 'WEEKLY', roleId: null, enabled: false }, ctxFor());
    assert.equal(open.enabled, false);
    const edited = await s.update(g, boost.id, { percent: 80, roleId: null, enabled: false, name: 'Nitro' }, ctxFor());
    assert.deepEqual([edited.percent, edited.roleId, edited.enabled, edited.name, edited.period], [80, null, false, 'Nitro', 'DAILY']);
    assert.equal(await code(s.update(g, boost.id, { name: 'everyone' }, ctxFor())), 'conflict');
    assert.equal((await s.update(g, boost.id, { name: 'NITRO' }, ctxFor())).name, 'NITRO'); // its own name, other case
    // usage rows are counted
    await prisma.economyBoostUse.create({ data: { boostId: boost.id, userId: snow(5), periodKey: 'D:2026-01-01' } });
    assert.equal((await s.get(g, boost.id)).uses, 1);
    assert.equal((await s.list(g)).find((b) => b.id === boost.id).uses, 1);
    assert.equal(await code(s.update(g2, boost.id, { percent: 5 }, ctxFor())), 'not_found');
    assert.equal(await code(s.remove(g2, boost.id, ctxFor())), 'not_found');
    await s.remove(g, boost.id, ctxFor());
    assert.equal(await prisma.economyBoostUse.count({ where: { boostId: boost.id } }), 0);
    // the cap
    const g3 = await newGuild();
    const results = await Promise.all(Array.from({ length: LIMITS.ECONOMY_BOOSTS + 3 }, (_, i) =>
      code(s.create(g3, { name: `b${i}`, percent: 5, period: 'DAILY', roleId: null, enabled: true }, ctxFor()))));
    assert.equal(results.filter((r) => r === 'ok').length, LIMITS.ECONOMY_BOOSTS, results.join());
    assert.equal(results.filter((r) => r === 'limit').length, 3, results.join());
  });

  test('achievements: create, rewards need their module, tier edits fix up progress, delete, cap under concurrency', async () => {
    const s = stores.achievements;
    const g = await newGuild();
    const g2 = await newGuild();
    const t = (goal, extra = {}) => ({ goal, roleAdd: null, roleRemove: null, xp: null, coins: null, ...extra });
    const base = { description: null, emoji: null, trigger: 'MESSAGES', trackPastProgress: true, deadline: null, enabled: true };

    // XP and coin rewards need the Levels and Economy modules
    assert.equal(await code(s.create(g, { ...base, name: 'Xp', tiers: [t(10, { xp: 5 })] }, ctxFor())), 'invalid');
    await settingsSvc.updateSettings(g, { xpEnabled: true, economyEnabled: false });
    assert.equal((await s.create(g, { ...base, name: 'Xp', tiers: [t(10, { xp: 5 })] }, ctxFor())).tiers[0].xp, 5);
    assert.equal(await code(s.create(g, { ...base, name: 'Coins', tiers: [t(10, { coins: 5 })] }, ctxFor())), 'invalid');

    const a = await s.create(g, { ...base, name: 'Chatterbox', description: 'Talk', emoji: '💬', tiers: [t(10, { roleAdd: snow(1) }), t(20)] }, ctxFor());
    assert.deepEqual(a.tiers, [t(10, { roleAdd: snow(1) }), t(20)]);
    assert.equal(a.triggerLabel, 'Messages sent');
    assert.equal(await code(s.create(g, { ...base, name: 'CHATTERBOX', tiers: [t(1)] }, ctxFor())), 'conflict');
    assert.equal(await code(s.create(g, { ...base, name: 'Bad', tiers: [t(5), t(5)] }, ctxFor())), 'invalid');

    // progress of three members
    const row = (userId, tier, completed, almostNotified = true) => ({
      guildId: g, achievementId: a.id, userId, progress: tier * 10, tier, completedAt: completed ? new Date() : null, almostNotified,
    });
    await prisma.achievementProgress.createMany({ data: [row(snow(11), 2, true), row(snow(12), 1, false), row(snow(13), 0, false)] });
    const progress = async () => Object.fromEntries((await prisma.achievementProgress.findMany({ where: { achievementId: a.id } }))
      .map((p) => [p.userId, { tier: p.tier, done: p.completedAt !== null, almost: p.almostNotified }]));
    assert.equal((await s.get(g, a.id)).members, 3);

    // adding a tier lets finished members continue
    const added = await s.update(g, a.id, { tiers: [t(10, { roleAdd: snow(1) }), t(20), t(30)] }, ctxFor());
    assert.deepEqual(added.tiers.map((x) => x.goal), [10, 20, 30]);
    assert.deepEqual((await progress())[snow(11)], { tier: 2, done: false, almost: true });
    // changing a goal lets the "almost there" notice fire again for members working towards it
    await s.update(g, a.id, { tiers: [t(15, { roleAdd: snow(1) }), t(20), t(30)] }, ctxFor());
    const afterGoal = await progress();
    assert.equal(afterGoal[snow(13)].almost, false); // working towards tier 0
    assert.equal(afterGoal[snow(12)].almost, true);
    // removing tiers from the end clamps members and completes them
    await s.update(g, a.id, { tiers: [t(15, { roleAdd: snow(1) })] }, ctxFor());
    const afterRemove = await progress();
    // (like /achievement-config tier remove, members at the new top also get their "almost there" flag reset)
    assert.deepEqual(afterRemove[snow(11)], { tier: 1, done: true, almost: false });
    assert.deepEqual(afterRemove[snow(12)], { tier: 1, done: true, almost: false });
    assert.deepEqual(afterRemove[snow(13)], { tier: 0, done: false, almost: false });
    // scalar edits
    const deadline = new Date(Date.now() + 365 * 86_400_000).toISOString();
    const edited = await s.update(g, a.id, { name: 'Talker', enabled: false, trackPastProgress: false, deadline, emoji: null }, ctxFor());
    assert.deepEqual([edited.name, edited.enabled, edited.trackPastProgress, edited.deadline, edited.emoji], ['Talker', false, false, deadline, null]);
    assert.equal(await code(s.update(g, a.id, { deadline: '2099-01-01T00:00:00.000Z' }, ctxFor())), 'invalid'); // checked again in the store
    assert.equal(await code(s.update(g, a.id, { name: 'xp' }, ctxFor())), 'conflict');
    assert.equal(await code(s.update(g2, a.id, { enabled: true }, ctxFor())), 'not_found');
    assert.equal(await code(s.remove(g2, a.id, ctxFor())), 'not_found');
    // an unrelated edit still saves when a tier already holds an XP reward and the module is off
    const xp = (await s.list(g)).find((x) => x.name === 'Xp');
    await settingsSvc.updateSettings(g, { xpEnabled: false });
    assert.equal((await s.update(g, xp.id, { tiers: [t(10, { xp: 5 })], enabled: false }, ctxFor())).enabled, false);
    assert.equal(await code(s.update(g, xp.id, { tiers: [t(10, { xp: 6 })] }, ctxFor())), 'invalid');
    // delete cascades to progress
    await s.remove(g, a.id, ctxFor());
    assert.equal(await prisma.achievementProgress.count({ where: { achievementId: a.id } }), 0);
    // the cap
    const g3 = await newGuild();
    await prisma.achievement.createMany({
      data: Array.from({ length: LIMITS.ACHIEVEMENTS - 2 }, (_, i) => ({ guildId: g3, name: `a${i}`, trigger: 'MESSAGES', tiers: [{ goal: 1, rewards: [] }] })),
    });
    const results = await Promise.all(Array.from({ length: 5 }, (_, i) => code(s.create(g3, { ...base, name: `new${i}`, tiers: [t(1)] }, ctxFor()))));
    assert.equal(results.filter((r) => r === 'ok').length, 2, results.join());
    assert.equal(results.filter((r) => r === 'limit').length, 3, results.join());
  });

  test('starboards: create, unique names, edits incl. the ignore list, the NSFW rule, delete, cap', async () => {
    const s = stores.starboards;
    const g = await newGuild();
    const g2 = await newGuild();
    const base = { channelId: snow(1), emojis: ['⭐'], threshold: 3, selfStar: false, removeSelfStars: false, ignoreBots: true,
      removeBelowThreshold: true, autoReact: false, maxMessageAgeDays: 0, allowNsfw: false, ignoredChannelIds: [], enabled: true };
    const b = await s.create(g, { ...base, name: 'stars', emojis: ['⭐', '<:blob:123456789012345678>', '223456789012345678'] }, ctxFor());
    assert.deepEqual(b.emojis, ['⭐', '123456789012345678', '223456789012345678']); // custom emojis are stored by id
    assert.equal(b.reposts, 0);
    assert.equal(await code(s.create(g, { ...base, name: 'stars' }, ctxFor())), 'conflict');
    assert.equal((await s.create(g2, { ...base, name: 'stars' }, ctxFor())).name, 'stars'); // other server
    assert.equal(await code(s.create(g, { ...base, name: 'x', emojis: ['nope'] }, ctxFor())), 'invalid');
    assert.equal((await s.create(g, { ...base, name: 'any', emojis: [] }, ctxFor())).emojis.length, 0);

    // the ignore list changes by the difference, any number of channels at once
    const ids = [snow(21), snow(22), snow(23), snow(24)];
    assert.deepEqual((await s.update(g, b.id, { ignoredChannelIds: ids.slice(0, 3) }, ctxFor())).ignoredChannelIds.sort(), ids.slice(0, 3));
    assert.deepEqual((await s.update(g, b.id, { ignoredChannelIds: [ids[3], ids[0]] }, ctxFor())).ignoredChannelIds.sort(), [ids[0], ids[3]].sort());
    assert.deepEqual((await s.update(g, b.id, { ignoredChannelIds: [] }, ctxFor())).ignoredChannelIds, []);
    // scalars, rename, channel
    const edited = await s.update(g, b.id, { name: 'best-of', threshold: 7, channelId: snow(2), enabled: false, maxMessageAgeDays: 30 }, ctxFor());
    assert.deepEqual([edited.name, edited.threshold, edited.channelId, edited.enabled, edited.maxMessageAgeDays], ['best-of', 7, snow(2), false, 30]);
    assert.equal(await code(s.update(g, b.id, { name: 'any' }, ctxFor())), 'conflict');
    assert.equal(await code(s.update(g, b.id, { selfStar: true, removeSelfStars: true }, ctxFor())), 'invalid');
    assert.equal((await s.update(g, b.id, { emojis: ['🌟'] }, ctxFor())).emojis[0], '🌟');
    // an age-restricted source needs an age-restricted repost channel
    const discord = fakeDiscord({ channels: [{ id: snow(1), nsfw: false }, { id: snow(2), nsfw: true }] });
    assert.equal(await code(s.update(g, b.id, { allowNsfw: true, channelId: snow(1) }, ctxFor({ discord }))), 'invalid');
    assert.equal((await s.update(g, b.id, { allowNsfw: true, channelId: snow(2) }, ctxFor({ discord }))).allowNsfw, true);
    assert.equal(await code(s.create(g2, { ...base, name: 'nsfw', allowNsfw: true }, ctxFor({ discord }))), 'invalid');
    assert.equal(await code(s.create(g2, { ...base, name: 'nsfw', allowNsfw: true }, ctxFor({ discord: { getGuildResources: async () => { throw new Error('down'); } } }))), 'discord');
    // live reposts are counted
    const entry = await prisma.starboardEntry.create({ data: { starboardId: b.id, sourceChannelId: snow(5), sourceMessageId: snow(6), authorId: snow(7), boardMessageId: snow(8), count: 3 } });
    assert.equal((await s.get(g, b.id)).reposts, 1);
    assert.equal((await s.list(g)).find((x) => x.id === b.id).reposts, 1);
    assert.equal(await code(s.update(g2, b.id, { threshold: 4 }, ctxFor())), 'not_found');
    assert.equal(await code(s.remove(g2, b.id, ctxFor())), 'not_found');
    await s.remove(g, b.id, ctxFor());
    assert.equal(await prisma.starboardEntry.count({ where: { id: entry.id } }), 0);
    // the cap
    const g3 = await newGuild();
    const results = await Promise.all(Array.from({ length: LIMITS.STARBOARDS + 3 }, (_, i) => code(s.create(g3, { ...base, name: `b${i}` }, ctxFor()))));
    assert.equal(results.filter((r) => r === 'ok').length, LIMITS.STARBOARDS, results.join());
    assert.equal(results.filter((r) => r === 'limit').length, 3, results.join());
  });

  test('translated channels: create turns the module on, one row per channel, edits, delete, cap under concurrency', async () => {
    const s = stores.translateChannels;
    const g = await newGuild();
    const g2 = await newGuild();
    assert.notEqual((await settingsSvc.getSettings(g, { fresh: true })).translationEnabled, true);
    const a = await s.create(g, { channelId: snow(1), targetLangs: ['es', 'ja'] }, ctxFor());
    assert.deepEqual([a.targetLangs, a.languages], [['es', 'ja'], 'Spanish, Japanese']);
    assert.equal((await settingsSvc.getSettings(g, { fresh: true })).translationEnabled, true);
    assert.equal(await code(s.create(g, { channelId: snow(1), targetLangs: ['de'] }, ctxFor())), 'conflict');
    assert.equal(await code(s.create(g, { channelId: snow(2), targetLangs: ['xx'] }, ctxFor())), 'invalid');
    assert.equal((await s.create(g2, { channelId: snow(1), targetLangs: ['de'] }, ctxFor())).channelId, snow(1)); // other server
    assert.deepEqual((await s.update(g, a.id, { targetLangs: ['fr', 'zh-tw'] }, ctxFor())).targetLangs, ['fr', 'zh-tw']);
    assert.equal(await code(s.update(g2, a.id, { targetLangs: ['fr'] }, ctxFor())), 'not_found');
    assert.equal(await code(s.remove(g2, a.id, ctxFor())), 'not_found');
    await s.remove(g, a.id, ctxFor());
    assert.deepEqual(await s.list(g), []);
    assert.equal((await settingsSvc.getSettings(g, { fresh: true })).translationEnabled, true); // like /autotranslate, the module stays on
    const g3 = await newGuild();
    await prisma.translateChannel.createMany({ data: Array.from({ length: LIMITS.TRANSLATE_CHANNELS - 2 }, (_, i) => ({ guildId: g3, channelId: snow(100 + i), targetLangs: ['en'] })) });
    const results = await Promise.all(Array.from({ length: 5 }, (_, i) => code(s.create(g3, { channelId: snow(900 + i), targetLangs: ['es'] }, ctxFor()))));
    assert.equal(results.filter((r) => r === 'ok').length, 2, results.join());
    assert.equal(results.filter((r) => r === 'limit').length, 3, results.join());
  });

  test('voice hubs: create, one hub per channel, edits, refusing to delete open channels, cap', async () => {
    const s = stores.tempVoiceHubs;
    const g = await newGuild();
    const g2 = await newGuild();
    const base = { categoryId: null, nameFormat: "#{index} - {username}'s Channel", userLimit: 0, bitrate: null, keepAliveMinutes: 0,
      ownershipLockMinutes: 0, syncPermissions: true, createTextChannel: false, textChannelMembersOnly: true, ignoredRoleIds: [], moderatorRoleIds: [] };
    const hub = await s.create(g, { ...base, channelId: snow(1), bitrate: 96, categoryId: snow(30), moderatorRoleIds: [snow(40)] }, ctxFor({ discord: fakeDiscord() }));
    assert.deepEqual([hub.bitrate, hub.categoryId, hub.moderatorRoleIds, hub.activeChannels], [96, snow(30), [snow(40)], 0]);
    assert.equal((await prisma.tempVoiceHub.findUnique({ where: { id: hub.id } })).bitrate, 96_000);
    assert.equal(await code(s.create(g, { ...base, channelId: snow(1) }, ctxFor())), 'conflict');
    assert.equal(await code(s.create(g2, { ...base, channelId: snow(1) }, ctxFor())), 'conflict'); // a channel id is one hub, whichever server
    assert.equal(await code(s.create(g, { ...base, channelId: snow(2), nameFormat: "{user}'s room" }, ctxFor())), 'invalid');
    await prisma.tempVoiceChannel.create({ data: { guildId: g, hubId: hub.id, channelId: snow(50), ownerId: snow(51), index: 1 } });
    assert.equal(await code(s.create(g, { ...base, channelId: snow(50) }, ctxFor())), 'invalid'); // a temporary channel cannot be a hub
    assert.equal((await s.get(g, hub.id)).activeChannels, 1);
    const edited = await s.update(g, hub.id, { bitrate: null, categoryId: null, userLimit: 5, nameFormat: 'Room {index}', ignoredRoleIds: [snow(41)] }, ctxFor());
    assert.deepEqual([edited.bitrate, edited.categoryId, edited.userLimit, edited.nameFormat, edited.ignoredRoleIds], [null, null, 5, 'Room {index}', [snow(41)]]);
    assert.equal(await code(s.update(g, hub.id, { nameFormat: '{user}' }, ctxFor())), 'invalid');
    assert.equal(await code(s.update(g2, hub.id, { userLimit: 1 }, ctxFor())), 'not_found');
    assert.equal(await code(s.remove(g2, hub.id, ctxFor())), 'not_found');
    assert.equal(await code(s.remove(g, hub.id, ctxFor())), 'invalid'); // a temporary channel is still open
    assert.ok(await s.get(g, hub.id));
    await prisma.tempVoiceChannel.deleteMany({ where: { hubId: hub.id } });
    await s.remove(g, hub.id, ctxFor());
    assert.equal(await s.get(g, hub.id), null);
    const g3 = await newGuild();
    const results = await Promise.all(Array.from({ length: LIMITS.TEMPVOICE_HUBS + 3 }, (_, i) => code(s.create(g3, { ...base, channelId: snow(700 + i) }, ctxFor()))));
    assert.equal(results.filter((r) => r === 'ok').length, LIMITS.TEMPVOICE_HUBS, results.join());
    assert.equal(results.filter((r) => r === 'limit').length, 3, results.join());
  });

  test('counter channels: create schedules the refresh, one per kind, role counters, edits, delete keeps the channel', async () => {
    const s = stores.statChannels;
    const g = await newGuild();
    const g2 = await newGuild();
    const discord = fakeDiscord({ roles: [{ id: snow(40), name: 'Staff' }], memberCount: 120 });
    const job = () => prisma.scheduledJob.findFirst({ where: { guildId: g, type: 'stats.refresh', dedupeKey: `stats.refresh:${g}` } });
    assert.equal(await job(), null);
    const members = await s.create(g, { kind: 'MEMBERS', channelId: snow(1), roleId: null, template: null }, ctxFor({ discord }));
    assert.deepEqual([members.template, members.kindLabel, members.lastValue], ['Members: {count}', 'Members', null]);
    const scheduled = await job();
    assert.ok(scheduled, 'the refresh job exists');
    assert.ok(['PENDING', 'RUNNING'].includes(scheduled.status));
    assert.equal(await code(s.create(g, { kind: 'MEMBERS', channelId: snow(2), roleId: null, template: null }, ctxFor({ discord }))), 'conflict'); // one per kind
    assert.equal(await code(s.create(g, { kind: 'BOTS', channelId: snow(1), roleId: null, template: null }, ctxFor({ discord }))), 'conflict'); // one per channel
    assert.equal(await code(s.create(g, { kind: 'BOTS', channelId: snow(3), roleId: null, template: 'Bots' }, ctxFor({ discord }))), 'invalid');
    assert.equal(await code(s.create(g, { kind: 'BOTS', channelId: snow(3), roleId: null, template: '{nonsense} {count}' }, ctxFor({ discord }))), 'invalid');
    assert.equal((await s.create(g, { kind: 'BOTS', channelId: snow(3), roleId: null, template: '{server}: {count} bots' }, ctxFor({ discord }))).template, '{server}: {count} bots');
    assert.equal(await code(s.create(g, { kind: 'ROLE_MEMBERS', channelId: snow(4), roleId: g, template: null }, ctxFor({ discord }))), 'invalid'); // @everyone
    const staff = await s.create(g, { kind: 'ROLE_MEMBERS', channelId: snow(4), roleId: snow(40), template: null }, ctxFor({ discord }));
    assert.equal(staff.template, '{role}: {count}');
    assert.equal(await code(s.create(g, { kind: 'ROLE_MEMBERS', channelId: snow(5), roleId: snow(40), template: null }, ctxFor({ discord }))), 'conflict'); // same role
    assert.equal((await s.create(g, { kind: 'ROLE_MEMBERS', channelId: snow(5), roleId: snow(41), template: null }, ctxFor({ discord }))).roleId, snow(41));
    // big servers cannot use the member-list counters
    const big = fakeDiscord({ memberCount: 100_001 });
    assert.equal(await code(s.create(g, { kind: 'HUMANS', channelId: snow(6), roleId: null, template: null }, ctxFor({ discord: big }))), 'invalid');
    // edits: the name only; empty returns to the default
    assert.equal((await s.update(g, members.id, { template: 'Total: {count}' }, ctxFor({ discord }))).template, 'Total: {count}');
    assert.equal((await prisma.statChannel.findUnique({ where: { id: members.id } })).template, 'Total: {count}');
    assert.equal((await s.update(g, members.id, { template: null }, ctxFor({ discord }))).template, 'Members: {count}');
    assert.equal(await code(s.update(g, members.id, { template: '{role}: {count}' }, ctxFor({ discord }))), 'invalid');
    assert.equal(await code(s.update(g2, members.id, { template: 'x {count}' }, ctxFor())), 'not_found');
    assert.equal(await code(s.remove(g2, members.id, ctxFor())), 'not_found');
    // the refresh action pulls the job forward (and needs the module on)
    await settingsSvc.updateSettings(g, { statsEnabled: false });
    assert.equal(await code(s.actions.refresh(g, members.id, {}, ctxFor())), 'invalid');
    await settingsSvc.updateSettings(g, { statsEnabled: true });
    const pulled = await s.actions.refresh(g, members.id, {}, ctxFor());
    assert.equal(pulled.item.id, members.id);
    assert.match(pulled.message, /minute/);
    assert.ok((await job()).runAt.getTime() <= Date.now() + 6_000);
    assert.equal(await code(s.actions.refresh(g2, members.id, {}, ctxFor())), 'not_found');
    // delete: the rows go, the refresh job goes with the last row
    for (const item of await s.list(g)) {
      assert.ok(await job(), 'the job stays while counters remain');
      await s.remove(g, item.id, ctxFor());
    }
    assert.deepEqual(await s.list(g), []);
    assert.equal((await job())?.status ?? 'CANCELLED', 'CANCELLED');
    // the role-counter cap under concurrency
    const g3 = await newGuild();
    await prisma.statChannel.createMany({
      data: Array.from({ length: LIMITS.ROLE_STAT_CHANNELS - 2 }, (_, i) => ({ guildId: g3, channelId: snow(200 + i), kind: 'ROLE_MEMBERS', roleId: snow(300 + i), template: '{role}: {count}' })),
    });
    const results = await Promise.all(Array.from({ length: 5 }, (_, i) =>
      code(s.create(g3, { kind: 'ROLE_MEMBERS', channelId: snow(800 + i), roleId: snow(900 + i), template: null }, ctxFor({ discord })))));
    assert.equal(results.filter((r) => r === 'ok').length, 2, results.join());
    assert.equal(results.filter((r) => r === 'limit').length, 3, results.join());
    // the overall cap
    const g4 = await newGuild();
    await prisma.statChannel.createMany({
      data: Array.from({ length: LIMITS.STAT_CHANNELS }, (_, i) => ({ guildId: g4, channelId: snow(1000 + i), kind: 'ROLES', template: 'Roles: {count}' })),
    });
    assert.equal(await code(s.create(g4, { kind: 'MEMBERS', channelId: snow(2000), roleId: null, template: null }, ctxFor({ discord }))), 'limit');
  });
});
