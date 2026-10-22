// pkg-economy tests (pkg-economy.md §11 items 1-14, 24, 26-28 adapted to this package, plus the architect amendments: coin boosts,
// game-channel deny mode, max payout). The casino games (items 15-23, 25) are tested by economy-casino.
// Offline by default. With TEST_DATABASE_URL (a migrated database) the DB cases also run: every raw statement (EXPLAIN), the lazy
// global copy, escrow, atomic claims, boosts, transfers, shop transactions, reset and the rewards provider.
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/economy.test.js
import { test, after, afterEach, mock } from 'node:test';
import { EventEmitter } from 'node:events';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is only used with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { PermissionFlagsBits } = await import('discord.js');
const { Prisma } = await import('@prisma/client');
const svc = await import('../src/services/economyService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const settingsSvc = await import('../src/services/settingsService.js');
const rewards = await import('../src/services/rewards.js');
const { COIN_MAX, LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { clearHooks, registerHook } = await import('../src/handlers/hookRegistry.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { renderPlaceholders } = await import('../src/utils/placeholderParser.js');
const { plugin } = await import('../src/plugins/economy.js');

const CMD_DIR = join(ROOT, 'src', 'commands');
const cmd = {};
for (const [key, file] of Object.entries({
  balance: 'economy/balance.js', daily: 'economy/daily.js', pity: 'economy/pity.js', work: 'economy/work.js', pay: 'economy/pay.js',
  richest: 'economy/richest.js', shop: 'economy/shop.js', inventory: 'economy/inventory.js', use: 'economy/use.js',
  admin: 'admin/economy-admin.js',
})) cmd[key] = await import(`../src/commands/${file}`);

const {
  dailyAmount, streakMultiplierLabel, nextDailyStreak, activeStreak, economyConfig, formatCoins, formatNet, validateCurrencyEmoji,
  validateCurrencyName, validateItemName, effectiveMaxOwned, goesToInventory, gameChannelCheck, tryLockCasino, unlockCasino,
  isCasinoLocked, itemVars, DEFAULT_BUY_MESSAGE, EconomyError, boostPeriodKey, orderBoosts, boostedAmount, clampPayout, RAW_SQL,
} = svc;

// Silence the Redis reconnect noise and the logger output of the failure-path tests.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const G = (n) => `955025000000000${String(n).padStart(4, '0')}`;
const U = (n) => `955025900000000${String(n).padStart(4, '0')}`;
const GUILDS = Array.from({ length: 25 }, (_, i) => G(i + 1));
const USERS = Array.from({ length: 60 }, (_, i) => U(i + 1));

after(async () => {
  Object.assign(console, quiet);
  clearHooks();
  mock.restoreAll();
  redis.disconnect();
  if (DB) {
    // Leave nothing behind in a shared test database (accounts, items, boosts and inventories cascade from the guild).
    for (const guildId of GUILDS) await prisma.guildSettings.deleteMany({ where: { guildId } }).catch(() => {});
    await prisma.userEconomy.deleteMany({ where: { userId: { in: USERS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});
afterEach(() => {
  clearHooks();
  mock.restoreAll();
});

// ═════════════════════════════════ pure helpers ═════════════════════════════════

test('daily amounts follow the streak table and round half up (item 1)', () => {
  assert.deepEqual([1, 2, 3, 4, 5, 6, 7, 8].map((s) => dailyAmount(100, s)), [100, 110, 120, 140, 180, 250, 400, 400]);
  assert.equal(dailyAmount(55, 2), 61);
  assert.equal(dailyAmount(50, 2), 55);
  assert.equal(dailyAmount(51, 2), 56);
});

test('streak multiplier labels (item 2)', () => {
  assert.equal(streakMultiplierLabel(1), '×1');
  assert.equal(streakMultiplierLabel(2), '×1.1');
  assert.equal(streakMultiplierLabel(6), '×2.5');
  assert.equal(streakMultiplierLabel(30), '×4');
});

test('streak windows: nextDailyStreak and activeStreak (item 3)', () => {
  const now = new Date('2026-10-10T12:00:00Z');
  const ago = (h) => new Date(now.getTime() - h * 3_600_000);
  assert.equal(nextDailyStreak(null, 0), 1);
  assert.equal(nextDailyStreak(ago(30), 3, now), 4);
  assert.equal(nextDailyStreak(ago(49), 3, now), 1);
  assert.equal(activeStreak(ago(49), 5, now), 0);
  assert.equal(activeStreak(ago(47), 5, now), 5);
  assert.equal(activeStreak(null, 5, now), 0);
});

test('economyConfig defaults and clamps (item 4; amendments 3 and 4)', () => {
  assert.deepEqual(economyConfig(null), {
    currencyName: 'coins', currencyEmoji: '🪙', startingBalance: 1000, maxBalance: 1_000_000_000, maxBet: 0,
    dailyMin: 50, dailyMax: 120, workMin: 100, workMax: 300, workCooldownMinutes: 60, gameChannelIds: [],
    gameChannelMode: 'ALLOW', maxPayout: 0,
  });
  const c = economyConfig({ economyMaxBalance: 5e9, economyStartingBalance: 2e9, dailyMin: 300, dailyMax: 100 });
  assert.equal(c.maxBalance, 1e9);
  assert.equal(c.startingBalance, 1e9);
  assert.equal(c.dailyMax, 300);
  const d = economyConfig({ economyGameChannelMode: 'DENY', economyMaxPayout: 750, workCooldownMinutes: 0.5, economyMaxBet: -5 });
  assert.equal(d.gameChannelMode, 'DENY');
  assert.equal(d.maxPayout, 750);
  assert.equal(d.workCooldownMinutes, 60); // a non-integer falls back to the default before clamping
  assert.equal(d.maxBet, 0);
  assert.equal(economyConfig({ economyMaxPayout: 5e9 }).maxPayout, COIN_MAX);
});

test('formatCoins and formatNet (item 5)', () => {
  assert.equal(formatCoins(1234, null), '🪙 **1,234** coins');
  assert.equal(formatCoins(5, { currencyName: 'gems', currencyEmoji: '💎' }, { bold: false }), '💎 5 gems');
  assert.ok(formatNet(-100, null).startsWith('−'));
  assert.ok(formatNet(50, null).startsWith('+'));
  assert.ok(formatNet(0, null).startsWith('±'));
});

test('currency emoji validation (item 6)', () => {
  for (const ok of ['🪙', '<:coin:123456789012345678>', '<a:spin:123456789012345678>', '❤️', '👍🏽']) assert.ok(validateCurrencyEmoji(ok).ok, ok);
  assert.equal(validateCurrencyEmoji('<:coin:123456789012345678>').customId, '123456789012345678');
  assert.equal(validateCurrencyEmoji('🪙').customId, null);
  for (const bad of ['abc', '<@123456789012345678>', '', '🪙 coins']) assert.equal(validateCurrencyEmoji(bad).ok, false, bad);
});

test('currency name validation (item 7)', () => {
  assert.deepEqual(validateCurrencyName('  gold   coins '), { ok: true, value: 'gold coins' });
  for (const bad of ['**x**', '@everyone', 'a'.repeat(33), 'line\nbreak', '', '`code`', '<#1>']) assert.equal(validateCurrencyName(bad).ok, false, bad);
});

test('item name validation (item 8)', () => {
  assert.deepEqual(validateItemName('  VIP   Pass '), { ok: true, value: 'VIP Pass' });
  assert.equal(validateItemName('none').ok, false);
  assert.equal(validateItemName('NONE').ok, false);
  assert.equal(validateItemName('a'.repeat(51)).ok, false);
  assert.equal(validateItemName('   ').ok, false);
  assert.equal(validateItemName('two\nlines').ok, false);
});

test('ownership helpers (items 9 and 10)', () => {
  assert.equal(effectiveMaxOwned({ canBuyMultiple: false, maxPerUser: 200 }), 1);
  assert.equal(effectiveMaxOwned({ canBuyMultiple: true, maxPerUser: 500 }), 200);
  assert.equal(effectiveMaxOwned({ canBuyMultiple: true, maxPerUser: 3 }), 3);
  assert.equal(goesToInventory({ type: 'ROLE', useOnBuy: true, destroyOnUse: true }), false);
  assert.equal(goesToInventory({ type: 'USABLE', useOnBuy: true, destroyOnUse: false }), true);
  assert.equal(goesToInventory({ type: 'STATIC', useOnBuy: true, destroyOnUse: true }), true);
});

test('gameChannelCheck: allow mode, thread parents and fail-open (item 11)', () => {
  const guild = { channels: { cache: new Map([['1', {}], ['2', {}]]) } };
  const check = (ids, channel, mode) => gameChannelCheck({ economyGameChannelIds: ids, economyGameChannelMode: mode }, channel, guild);
  assert.deepEqual(check([], { id: '3' }), { ok: true });
  assert.deepEqual(check(['1'], { id: '1' }), { ok: true });
  assert.deepEqual(check(['1'], { id: '3' }), { ok: false, mode: 'ALLOW', allowedIds: ['1'] });
  assert.deepEqual(check(['1'], { id: '9', parentId: '1', isThread: () => true }), { ok: true });
  assert.deepEqual(check(['1'], { id: '9', parentId: '2', isThread: () => true }), { ok: false, mode: 'ALLOW', allowedIds: ['1'] });
  assert.deepEqual(check(['7'], { id: '3' }), { ok: true }); // every configured channel was deleted: fail-open
  assert.deepEqual(check(['7', '1'], { id: '3' }), { ok: false, mode: 'ALLOW', allowedIds: ['1'] }); // only existing channels count
});

test('gameChannelCheck: deny mode (amendment 3)', () => {
  const guild = { channels: { cache: new Map([['1', {}], ['2', {}]]) } };
  const check = (ids, channel) => gameChannelCheck({ economyGameChannelIds: ids, economyGameChannelMode: 'DENY' }, channel, guild);
  assert.deepEqual(check(['1'], { id: '1' }), { ok: false, mode: 'DENY', deniedIds: ['1'] });
  assert.deepEqual(check(['1'], { id: '2' }), { ok: true });
  assert.deepEqual(check(['1'], { id: '9', parentId: '1', isThread: () => true }), { ok: false, mode: 'DENY', deniedIds: ['1'] });
  assert.deepEqual(check(['1'], { id: '9', parentId: '2', isThread: () => true }), { ok: true });
  assert.deepEqual(check([], { id: '1' }), { ok: true });
  assert.deepEqual(check(['7'], { id: '7' }), { ok: true }); // the listed channel was deleted: nothing is blocked
});

test('casino lock is per member and idempotent (item 12)', () => {
  assert.equal(tryLockCasino('g', 'u'), true);
  assert.equal(tryLockCasino('g', 'u'), false);
  assert.equal(isCasinoLocked('g', 'u'), true);
  assert.equal(tryLockCasino('g', 'other'), true);
  assert.equal(tryLockCasino('g2', 'u'), true);
  unlockCasino('g', 'u');
  unlockCasino('g', 'u');
  assert.equal(isCasinoLocked('g', 'u'), false);
  assert.equal(tryLockCasino('g', 'u'), true);
  for (const [g, u] of [['g', 'u'], ['g', 'other'], ['g2', 'u']]) unlockCasino(g, u);
});

test('item message variables and the default buy message (item 13)', () => {
  const vars = itemVars({ name: 'VIP', price: 500 }, { quantity: 3, total: 1500, settings: null });
  assert.deepEqual(vars, { item: 'VIP', quantity: '3', coins: '1,500', price: '500', currency: 'coins', 'currency.emoji': '🪙' });
  assert.equal(renderPlaceholders(DEFAULT_BUY_MESSAGE, { vars }), 'You bought **3× VIP** for 🪙 **1,500** coins!');
});

test('the old global-wallet exports are gone (item 14)', () => {
  for (const name of ['getOrCreate', 'applyBet', 'getBalance', 'STARTING_BALANCE', 'DAILY_AMOUNT']) assert.equal(svc[name], undefined, name);
});

test('clampPayout limits net winnings to maxPayout (amendment 4)', () => {
  assert.deepEqual(clampPayout(1000, 100, 500), { payout: 600, capped: true });
  assert.deepEqual(clampPayout(1000, 1000, 500), { payout: 1000, capped: false }); // a push is never cut
  assert.deepEqual(clampPayout(2000, 1000, 500), { payout: 1500, capped: true });
  assert.deepEqual(clampPayout(0, 1000, 500), { payout: 0, capped: false });
  assert.deepEqual(clampPayout(36_000, 1000, 0), { payout: 36_000, capped: false }); // 0 = no limit
});

test('boostedAmount rounds down (amendment 5)', () => {
  assert.equal(boostedAmount(120, 50), 180);
  assert.equal(boostedAmount(101, 50), 151);
  assert.equal(boostedAmount(100, 500), 600);
  assert.equal(boostedAmount(7, 1), 7);
});

test('boostPeriodKey in the guild timezone across a month boundary (amendment 5)', () => {
  const now = new Date('2026-06-30T23:30:00Z'); // 00:30 on 1 July in Europe/London (BST)
  assert.equal(boostPeriodKey('ONE_TIME', 'Europe/London', now), 'once');
  assert.equal(boostPeriodKey('DAILY', 'Europe/London', now), 'D:2026-07-01');
  assert.equal(boostPeriodKey('DAILY', 'UTC', now), 'D:2026-06-30');
  assert.equal(boostPeriodKey('MONTHLY', 'Europe/London', now), 'M:2026-07');
  assert.equal(boostPeriodKey('MONTHLY', 'UTC', now), 'M:2026-06');
  assert.equal(boostPeriodKey('WEEKLY', 'Europe/London', now), 'W:2026-W27'); // Wednesday 1 July 2026
  assert.equal(boostPeriodKey('WEEKLY', 'UTC', now), 'W:2026-W27');
  // ISO week years: 1 Jan 2026 is a Thursday (week 1); 3 Jan 2021 is still in 2020's week 53; Monday 28 Dec 2026 starts week 53.
  assert.equal(boostPeriodKey('WEEKLY', 'UTC', new Date('2026-01-01T12:00:00Z')), 'W:2026-W01');
  assert.equal(boostPeriodKey('WEEKLY', 'UTC', new Date('2025-12-29T12:00:00Z')), 'W:2026-W01');
  assert.equal(boostPeriodKey('WEEKLY', 'UTC', new Date('2021-01-03T12:00:00Z')), 'W:2020-W53');
  assert.equal(boostPeriodKey('WEEKLY', 'UTC', new Date('2026-12-28T12:00:00Z')), 'W:2026-W53');
  assert.equal(boostPeriodKey('DAILY', 'Not/AZone', now), 'D:2026-06-30'); // an invalid timezone is UTC
});

test('orderBoosts: role filter, disabled skipped, percent then age (amendment 5)', () => {
  const b = (id, percent, roleId, createdAt, enabled = true) => ({ id, percent, roleId, createdAt: new Date(createdAt), enabled });
  const boosts = [
    b('a', 50, null, '2026-01-02'), b('b', 100, 'R1', '2026-01-03'), b('c', 100, null, '2026-01-01'),
    b('d', 200, 'R2', '2026-01-04'), b('e', 300, null, '2026-01-05', false), b('f', 50, null, '2026-01-01'),
  ];
  assert.deepEqual(orderBoosts(boosts, ['R1']).map((x) => x.id), ['c', 'b', 'f', 'a']); // equal percent: the older boost first
  assert.deepEqual(orderBoosts(boosts, []).map((x) => x.id), ['c', 'f', 'a']);
  assert.deepEqual(orderBoosts(boosts, new Set(['R1', 'R2'])).map((x) => x.id), ['d', 'c', 'b', 'f', 'a']);
  assert.deepEqual(orderBoosts(null, []), []);
});

// ═════════════════════════════ escrow (no database needed) ═════════════════════════════

test('reserveBet validates before touching the database', async () => {
  for (const bet of [0, -5, 1.5, NaN, COIN_MAX + 1, '10']) assert.deepEqual(await svc.reserveBet('g', 'u', bet, null), { ok: false, code: 'INVALID' }, String(bet));
  assert.deepEqual(await svc.reserveBet('g', 'u', 1000, { economyMaxBet: 500 }), { ok: false, code: 'MAX_BET', maxBet: 500 });
});

test('settleBet requires an integer stake and a valid payout (amendment 4)', async () => {
  for (const opts of [undefined, {}, { stake: undefined }, { stake: 1.5 }, { stake: '100' }, { stake: NaN }]) {
    await assert.rejects(svc.settleBet('g', 'u', 0, null, opts), (e) => e instanceof EconomyError && e.code === 'INVALID', JSON.stringify(opts));
    await assert.rejects(svc.settleBet('g', 'u', 100, null, opts), (e) => e instanceof EconomyError && e.code === 'INVALID', JSON.stringify(opts));
  }
  for (const payout of [-1, 1.5, NaN]) {
    await assert.rejects(svc.settleBet('g', 'u', payout, null, { stake: 100 }), (e) => e instanceof EconomyError && e.code === 'INVALID');
  }
});

test('transfer, credit, debit validate amounts first', async () => {
  await assert.rejects(svc.transfer('g', 'a', 'a', 5, null), (e) => e.code === 'INVALID');
  await assert.rejects(svc.transfer('g', 'a', 'b', 0, null), (e) => e.code === 'INVALID');
  await assert.rejects(svc.transfer('g', 'a', 'b', COIN_MAX + 1, null), (e) => e.code === 'INVALID');
  await assert.rejects(svc.credit('g', 'a', -1, null), (e) => e.code === 'INVALID');
  await assert.rejects(svc.debit('g', 'a', 0, null), (e) => e.code === 'INVALID');
  await assert.rejects(svc.setBalance('g', 'a', 5, { economyMaxBalance: 4 }), (e) => e.code === 'INVALID' && e.data.max === 4);
});

// ═════════════════════════════ commands, plugin, payloads ═════════════════════════════

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('command modules: exports, permission levels and payload budgets (item 24)', () => {
  const budgets = { balance: 110, daily: 90, pity: 90, work: 70, pay: 150, richest: 70, shop: 400, inventory: 110, use: 120, admin: 3800 };
  for (const [key, budget] of Object.entries(budgets)) {
    const mod = cmd[key];
    assert.equal(mod.module, 'economy', key);
    assert.equal(typeof mod.execute, 'function', key);
    const json = buildCommandPayload(mod);
    assert.ok(charCount(json) <= budget, `${key}: ${charCount(json)} > ${budget}`);
    assert.deepEqual(json.contexts, [0], key);
    if (key === 'admin') {
      assert.equal(mod.gated, false, key);
      assert.equal(mod.permissionLevel, PERMISSION_LEVELS.ADMIN, key);
      assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild), key);
    } else {
      assert.equal(mod.permissionLevel, PERMISSION_LEVELS.MEMBER, key);
      assert.notEqual(mod.gated, false, key);
    }
    const hasAutocomplete = JSON.stringify(json).includes('"autocomplete":true');
    assert.equal(hasAutocomplete, typeof mod.autocomplete === 'function', `${key} autocomplete wiring`);
  }
  const boost = buildCommandPayload(cmd.admin).options.find((o) => o.name === 'boost');
  assert.equal(boost.type, 2);
  assert.deepEqual(boost.options.map((o) => o.name), ['add', 'remove', 'toggle', 'list']);
  assert.deepEqual(boost.options.find((o) => o.name === 'add').options.map((o) => o.name), ['name', 'percent', 'period', 'role']);
});

test('/economy-admin builder: option layout and the amendment options', () => {
  const json = buildCommandPayload(cmd.admin);
  const groups = Object.fromEntries(json.options.filter((o) => o.type === 2).map((g) => [g.name, g]));
  assert.deepEqual(Object.keys(groups).sort(), ['boost', 'coins', 'config', 'item']);
  const sub = (g, name) => groups[g].options.find((o) => o.name === name);
  const names = (s) => s.options.map((o) => o.name);
  assert.ok(names(sub('config', 'channels')).includes('mode'));
  assert.ok(names(sub('config', 'limits')).includes('max_payout'));
  assert.ok(json.options.some((o) => o.name === 'reset'));
  assert.deepEqual(groups.coins.options.map((o) => o.name), ['give', 'remove', 'set']);
});

test('plugin declaration and the rewards provider (item 26)', async () => {
  assert.equal(plugin.id, 'economy');
  assert.equal(plugin.module, 'economy');
  assert.deepEqual([plugin.hooks, plugin.components, plugin.jobs], [[], [], []]);
  assert.equal(typeof plugin.provides.coins.grant, 'function');
  assert.equal(typeof plugin.provides.coins.take, 'function');
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => /economy/.test(e)), []);
  assert.ok(plugins.some((p) => p.id === 'economy' && p.provides.coins));
  // The bus validates amounts itself and the provider only counts while the economy module is on.
  rewards.registerProvider('coins', plugin.provides.coins, 'economy');
  assert.equal(rewards.isRewardAvailable('coins', { economyEnabled: true }), true);
  assert.equal(rewards.isRewardAvailable('coins', { economyEnabled: false }), false);
  assert.deepEqual(await rewards.grant('coins', { guild: { id: 'g' }, userId: 'u', amount: 0 }), { ok: false, reason: 'invalid-amount' });
});

// Minimal interaction double: records every reply call.
function fakeInteraction({ group = null, guildId = G(1), user = { id: U(1), bot: false, username: 'tester', displayName: 'Tester' }, options = {}, sub = null, member, guild, channel } = {}) {
  const calls = [];
  const i = {
    guildId, user, channel: channel ?? { id: '555' }, channelId: channel?.id ?? '555', client: {}, guild: guild ?? { id: guildId },
    member: member ?? { displayName: 'Tester', roles: { cache: new Map() } },
    deferred: false, replied: false, ephemeral: null, createdTimestamp: Date.now(), calls,
    isRepliable: () => true,
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => group,
      getChannel: (n) => options[n] ?? null,
      get: (n) => options[n] ?? null,
      getUser: (n) => options[n] ?? null,
      getMember: (n) => options[`${n}:member`] ?? null,
      getString: (n) => options[n] ?? null,
      getRole: (n) => options[n] ?? null,
      getBoolean: (n) => options[n] ?? null,
      getInteger: (n) => options[n] ?? null,
      getFocused: () => options.focused ?? '',
    },
    deferReply: async (o) => { i.deferred = true; i.ephemeral = Boolean(o?.ephemeral); calls.push(['deferReply', o]); },
    reply: async (p) => { i.replied = true; i.ephemeral = Boolean(p?.ephemeral); calls.push(['reply', p]); return { resource: { message: {} } }; },
    editReply: async (p) => {
      i.replied = true;
      calls.push(['editReply', p]);
      // A message double whose collector answers awaitConfirm with a click on the first (confirm) button.
      return {
        id: 'm1',
        createMessageComponentCollector: () => {
          const collector = new EventEmitter();
          const customId = p?.components?.[0]?.components?.[0]?.data?.custom_id;
          setImmediate(() => {
            if (customId) collector.emit('collect', { customId, update: async () => {} });
            collector.emit('end', new Map(), 'limit');
          });
          return collector;
        },
      };
    },
    deleteReply: async () => { calls.push(['deleteReply']); },
    followUp: async (p) => { calls.push(['followUp', p]); return {}; },
  };
  return i;
}
const titleOf = (call) => call[1].embeds[0].data.title;

test('/pity is refused privately while a casino game runs, before any database access', async () => {
  assert.equal(tryLockCasino(G(1), U(1)), true);
  try {
    const i = fakeInteraction();
    await cmd.pity.execute(i);
    assert.deepEqual(i.calls.map((c) => c[0]), ['reply']);
    assert.equal(i.calls[0][1].ephemeral, true);
    assert.match(titleOf(i.calls[0]), /Game In Progress/);
  } finally {
    unlockCasino(G(1), U(1));
  }
});

test('/pay refuses yourself, bots and non-members privately before deferring (R088)', async () => {
  const bot = { id: U(2), bot: true, username: 'bot' };
  const human = { id: U(3), bot: false, username: 'human' };
  const cases = [
    [{ member: { id: U(1), bot: false, username: 'tester' }, amount: 5 }, /yourself/],
    [{ member: bot, amount: 5 }, /Bots can't hold/],
    [{ member: human, amount: 5 }, /not a member/],
  ];
  for (const [options, pattern] of cases) {
    const i = fakeInteraction({ options });
    await cmd.pay.execute(i);
    assert.deepEqual(i.calls.map((c) => c[0]), ['reply']);
    assert.equal(i.calls[0][1].ephemeral, true);
    assert.match(i.calls[0][1].embeds[0].data.description, pattern);
  }
});

test('/balance and /inventory refuse bot targets privately', async () => {
  for (const mod of [cmd.balance, cmd.inventory]) {
    const i = fakeInteraction({ options: { user: { id: U(2), bot: true, username: 'bot' } } });
    await mod.execute(i);
    assert.deepEqual(i.calls.map((c) => c[0]), ['reply']);
    assert.match(i.calls[0][1].embeds[0].data.description, /Bots can't hold/);
  }
});

test('replyFail shows the error privately whatever the interaction state', async () => {
  const fresh = fakeInteraction();
  await svc.replyFail(fresh, 'Oops', 'Nope');
  assert.deepEqual(fresh.calls.map((c) => c[0]), ['reply']);
  assert.equal(fresh.calls[0][1].ephemeral, true);

  const publicDefer = fakeInteraction();
  await publicDefer.deferReply();
  await svc.replyFail(publicDefer, 'Oops', 'Nope');
  assert.deepEqual(publicDefer.calls.map((c) => c[0]), ['deferReply', 'deleteReply', 'followUp']);
  assert.equal(publicDefer.calls[2][1].ephemeral, true);

  const privateDefer = fakeInteraction();
  await privateDefer.deferReply({ ephemeral: true });
  await svc.replyFail(privateDefer, 'Oops', 'Nope');
  assert.deepEqual(privateDefer.calls.map((c) => c[0]), ['deferReply', 'editReply']);

  const warning = { data: { title: 'custom' } };
  const withEmbed = fakeInteraction();
  await svc.replyFail(withEmbed, null, null, { embed: warning });
  assert.equal(withEmbed.calls[0][1].embeds[0], warning);

  const gone = fakeInteraction();
  gone.reply = async () => { throw Object.assign(new Error('Unknown interaction'), { code: 10062 }); };
  await svc.replyFail(gone, 'x', 'y'); // never throws
});

// ═════════════════════════════════ static scans ═════════════════════════════════

function walk(dir) {
  return readdirSync(dir).flatMap((f) => {
    const p = join(dir, f);
    return statSync(p).isDirectory() ? walk(p) : p.endsWith('.js') ? [p] : [];
  });
}
const read = (p) => readFileSync(p, 'utf8');
const MY_COMMANDS = ['economy/balance', 'economy/daily', 'economy/pity', 'economy/work', 'economy/pay', 'economy/richest', 'economy/shop',
  'economy/inventory', 'economy/use', 'admin/economy-admin'].map((f) => join(CMD_DIR, `${f}.js`));

test('UserEconomy is never written anywhere in src (item 27)', () => {
  const bad = /userEconomy\.(create|update|upsert|delete|updateMany|deleteMany|createMany)/;
  for (const file of walk(join(ROOT, 'src'))) assert.doesNotMatch(read(file), bad, file);
});

test('economyService: bigint maths, SQL_NOW, no unsafe SQL; only the service touches Prisma (item 28, adapted)', () => {
  const code = read(join(ROOT, 'src', 'services', 'economyService.js'));
  assert.match(code, /LEAST\(/);
  assert.match(code, /::bigint/);
  // Adaptation: the UTC `AT TIME ZONE` fragment now lives in SQL_NOW (src/database/client.js), which the service interpolates.
  assert.match(code, /import prisma, \{ SQL_NOW \} from '\.\.\/database\/client\.js'/);
  assert.doesNotMatch(code, /\$queryRawUnsafe|\$executeRawUnsafe/);
  assert.doesNotMatch(code, /(?<![\w.])now\(\)/i);
  assert.doesNotMatch(code, /\bconsole\./);
  assert.doesNotMatch(code, /cache\.del\(/);
  for (const file of MY_COMMANDS) {
    const text = read(file);
    assert.doesNotMatch(text, /database\/client\.js|@prisma\/client/, file);
    assert.doesNotMatch(text, /\bconsole\./, file);
    assert.doesNotMatch(text, /cache\.del\(/, file);
    assert.doesNotMatch(text, /userEconomy|getOrCreate|applyBet/, file);
  }
  assert.doesNotMatch(read(join(ROOT, 'src', 'plugins', 'economy.js')), /database\/client\.js/);
});

test('every command defers before its first settings or service call (R088)', () => {
  for (const file of MY_COMMANDS) {
    const text = read(file);
    const execute = text.slice(text.indexOf('export async function execute'));
    const defer = execute.indexOf('deferReply(');
    const firstAwait = execute.indexOf('await getSettings(');
    assert.ok(defer > -1 && firstAwait > -1 && defer < firstAwait, `${file}: deferReply must come before getSettings`);
  }
});

// ═══════════════════════════════════ DB cases ═══════════════════════════════════

// Settings doubles carry only the fields economyConfig reads; the GuildSettings row exists for the foreign keys.
const S = (over = {}) => ({ economyEnabled: true, dailyMin: 100, dailyMax: 100, workMin: 200, workMax: 200, ...over });
const account = (guildId, userId) => prisma.economyAccount.findUnique({ where: { guildId_userId: { guildId, userId } } });
const seedAccount = async (guildId, userId, data = {}) => {
  await settingsSvc.ensureGuildSettings(guildId);
  return prisma.economyAccount.upsert({
    where: { guildId_userId: { guildId, userId } },
    create: { guildId, userId, balance: 0, ...data },
    update: { balance: 0, dailyStreak: 0, lastDaily: null, lastWork: null, lastPity: null, ...data },
  });
};
const hoursAgo = (h) => new Date(Date.now() - h * 3_600_000);

test('every raw statement is valid SQL (EXPLAIN)', { skip: skipDb }, async () => {
  await settingsSvc.ensureGuildSettings(G(1));
  assert.ok(RAW_SQL.length >= 19);
  for (const { name, build } of RAW_SQL) await prisma.$queryRaw(Prisma.sql`EXPLAIN ${build()}`).catch((err) => assert.fail(`${name}: ${err.message}`));
});

test('lazy global copy: legacy balance, starting balance, clamps and the import switch', { skip: skipDb }, async () => {
  const g = G(2);
  await settingsSvc.ensureGuildSettings(g);
  await prisma.userEconomy.deleteMany({ where: { userId: { in: [U(1), U(2), U(3), U(4)] } } });
  await prisma.userEconomy.createMany({ data: [
    { userId: U(1), balance: 777 }, { userId: U(2), balance: -50 }, { userId: U(3), balance: 5000 },
  ] });

  const a = await svc.getAccount(g, U(1), S());
  assert.equal(a.balance, 777);
  assert.equal(a.importedGlobal, true);
  assert.equal((await svc.getAccount(g, U(1), S())).id, a.id); // idempotent: the second call reads the row
  assert.equal((await svc.getAccount(g, U(2), S())).balance, 0); // a negative legacy balance is clamped to 0
  assert.equal((await svc.getAccount(g, U(3), S({ economyMaxBalance: 1000 }))).balance, 1000);
  const fresh = await svc.getAccount(g, U(4), S({ economyStartingBalance: 321 }));
  assert.deepEqual([fresh.balance, fresh.importedGlobal], [321, false]);

  // Another server copies the same legacy balance again (per-server accounts, binding user decision).
  assert.equal((await svc.getAccount(G(3), U(1), S())).balance, 777);
  // The import switch: no legacy read, the starting balance applies.
  assert.deepEqual(await svc.getAccount(G(4), U(1), S({ economyImportGlobal: false, economyStartingBalance: 55 })).then((r) => [r.balance, r.importedGlobal]), [55, false]);

  // UserEconomy is never written: the legacy rows are untouched and no row was created for the fresh user.
  assert.equal((await prisma.userEconomy.findUnique({ where: { userId: U(1) } })).balance, 777);
  assert.equal(await prisma.userEconomy.findUnique({ where: { userId: U(4) } }), null);
});

test('lazy global copy is race-safe and peekAccount never creates rows', { skip: skipDb }, async () => {
  const g = G(5);
  await prisma.userEconomy.deleteMany({ where: { userId: U(5) } });
  await prisma.userEconomy.create({ data: { userId: U(5), balance: 400 } });
  await settingsSvc.ensureGuildSettings(g);
  const peek = await svc.peekAccount(g, U(5), S());
  assert.deepEqual([peek.exists, peek.balance, peek.importedGlobal, peek.dailyStreak], [false, 400, true, 0]);
  assert.equal(await account(g, U(5)), null);
  const rows = await Promise.all(Array.from({ length: 6 }, () => svc.getAccount(g, U(5), S())));
  assert.equal(new Set(rows.map((r) => r.id)).size, 1);
  assert.equal(await prisma.economyAccount.count({ where: { guildId: g, userId: U(5) } }), 1);
  assert.equal((await svc.peekAccount(g, U(5), S())).exists, true);
});

test('credit clamps to the server maximum and debit never goes below zero (R207, R236)', { skip: skipDb }, async () => {
  const g = G(6);
  await seedAccount(g, U(6), { balance: 500 });
  assert.deepEqual(await svc.credit(g, U(6), 100, S()), { balance: 600, capped: false });
  assert.deepEqual(await svc.credit(g, U(6), 36_000_000_000, S()), { balance: COIN_MAX, capped: true }); // far past int4
  assert.deepEqual(await svc.credit(g, U(6), 5, S({ economyMaxBalance: 2000 })), { balance: COIN_MAX, capped: true }); // a credit never lowers a balance
  await seedAccount(g, U(6), { balance: 250 });
  assert.deepEqual(await svc.debit(g, U(6), 300, S()), { ok: false, balance: 250 });
  assert.deepEqual(await svc.debit(g, U(6), 250, S()), { ok: true, balance: 0 });
  assert.equal((await svc.credit(g, U(6), 0, S())).balance, 0);
  // Missing accounts are created (lazy copy) by credit and debit alike.
  assert.equal((await svc.credit(g, U(7), 10, S({ economyStartingBalance: 90 }))).balance, 100);
  assert.deepEqual(await svc.debit(g, U(8), 5000, S({ economyStartingBalance: 90 })), { ok: false, balance: 90 });
});

test('concurrent debits never overspend (R007, R136)', { skip: skipDb }, async () => {
  const g = G(7);
  await seedAccount(g, U(9), { balance: 250 });
  const results = await Promise.all(Array.from({ length: 6 }, () => svc.debit(g, U(9), 100, S())));
  assert.equal(results.filter((r) => r.ok).length, 2);
  assert.equal((await account(g, U(9))).balance, 50);
});

test('reserveBet escrows before play and settleBet pays the gross payout (R007, R240)', { skip: skipDb }, async () => {
  const g = G(8);
  await seedAccount(g, U(10), { balance: 1000 });
  assert.deepEqual(await svc.reserveBet(g, U(10), 400, S()), { ok: true, balance: 600, before: 1000 });
  assert.deepEqual(await svc.reserveBet(g, U(10), 700, S()), { ok: false, code: 'INSUFFICIENT', balance: 600 });
  assert.deepEqual(await svc.reserveBet(g, U(10), 300, S({ economyMaxBet: 200 })), { ok: false, code: 'MAX_BET', maxBet: 200 });
  assert.deepEqual(await svc.settleBet(g, U(10), 800, S(), { stake: 400 }), { balance: 1400, capped: false, payoutCapped: false });
  assert.deepEqual(await svc.settleBet(g, U(10), 0, S(), { stake: 400 }), { balance: 1400, capped: false, payoutCapped: false }); // a loss pays nothing
  // The maximum payout limits net winnings: a push is untouched and a 2000 payout on a 1000 stake returns 1500.
  const capped = S({ economyMaxPayout: 500 });
  await seedAccount(g, U(10), { balance: 0 });
  assert.equal((await svc.settleBet(g, U(10), 1000, capped, { stake: 1000 })).balance, 1000);
  const big = await svc.settleBet(g, U(10), 2000, capped, { stake: 1000 });
  assert.deepEqual(big, { balance: 2500, capped: false, payoutCapped: true });
  // The server maximum limits the balance.
  assert.deepEqual(await svc.settleBet(g, U(10), 36_000_000_000, S(), { stake: 1_000_000_000 }), { balance: COIN_MAX, capped: true, payoutCapped: false });
});

test('two games cannot spend the same coins even without the lock (R007)', { skip: skipDb }, async () => {
  const g = G(9);
  await seedAccount(g, U(11), { balance: 1000 });
  const reserved = await Promise.all([svc.reserveBet(g, U(11), 1000, S()), svc.reserveBet(g, U(11), 1000, S())]);
  assert.deepEqual(reserved.map((r) => r.ok).sort(), [false, true]);
  assert.equal((await account(g, U(11))).balance, 0);
});

test('transfer: success, insufficient funds and a recipient at the cap roll back cleanly', { skip: skipDb }, async () => {
  const g = G(10);
  await seedAccount(g, U(12), { balance: 500 });
  await seedAccount(g, U(13), { balance: 100 });
  assert.deepEqual(await svc.transfer(g, U(12), U(13), 200, S()), { ok: true, fromBalance: 300, toBalance: 300 });
  assert.deepEqual(await svc.transfer(g, U(12), U(13), 301, S()), { ok: false, code: 'INSUFFICIENT', balance: 300 });
  // Recipient cap: nothing is destroyed, the sender keeps the coins.
  assert.deepEqual(await svc.transfer(g, U(12), U(13), 250, S({ economyMaxBalance: 500 })), { ok: false, code: 'RECIPIENT_CAP', balance: 300 });
  assert.equal((await account(g, U(12))).balance, 300);
  assert.equal((await account(g, U(13))).balance, 300);
  // New recipients get an account (lazy copy) first; opposite transfers at the same time do not deadlock.
  assert.equal((await svc.transfer(g, U(12), U(14), 10, S({ economyStartingBalance: 0 }))).toBalance, 10);
  const both = await Promise.all([svc.transfer(g, U(12), U(13), 5, S()), svc.transfer(g, U(13), U(12), 7, S())]);
  assert.ok(both.every((r) => r.ok));
  assert.equal((await account(g, U(12))).balance + (await account(g, U(13))).balance + (await account(g, U(14))).balance, 600);
});

test('setBalance, removeCoins, clampBalancesToCap, topAccounts and getRank', { skip: skipDb }, async () => {
  const g = G(11);
  await seedAccount(g, U(15), { balance: 100 });
  await seedAccount(g, U(16), { balance: 900 });
  await seedAccount(g, U(17), { balance: 900 });
  await seedAccount(g, U(18), { balance: 0 });
  assert.deepEqual(await svc.removeCoins(g, U(15), 40, S()), { balance: 60, removed: 40 });
  assert.deepEqual(await svc.removeCoins(g, U(15), 1_000_000, S()), { balance: 0, removed: 60 });
  assert.deepEqual(await svc.setBalance(g, U(15), 250, S()), { balance: 250 });
  await assert.rejects(svc.setBalance(g, U(15), 5000, S({ economyMaxBalance: 1000 })), (e) => e.code === 'INVALID');
  assert.deepEqual((await svc.topAccounts(g)).map((r) => r.userId), [U(16), U(17), U(15)]); // balance desc, userId asc, no zero balances
  assert.equal(await svc.getRank(g, 900), 1);
  assert.equal(await svc.getRank(g, 250), 3);
  assert.equal(await svc.clampBalancesToCap(g, 500), 2);
  assert.equal((await account(g, U(16))).balance, 500);
});

test('daily: streak, multipliers, the 48 h window, the cap and no double claims (R137)', { skip: skipDb }, async () => {
  const g = G(12);
  await seedAccount(g, U(19));
  const day1 = await svc.claimDaily(g, U(19), S());
  assert.deepEqual([day1.claimed, day1.amount, day1.base, day1.streak, day1.balance, day1.boost], [true, 100, 100, 1, 100, null]);
  assert.ok(Math.abs(day1.nextAt - (Date.now() + 86_400_000)) < 5000);
  const again = await svc.claimDaily(g, U(19), S());
  assert.equal(again.claimed, false);
  assert.equal(again.streak, 1);
  assert.equal((await account(g, U(19))).balance, 100);

  // Consecutive days: amounts follow the table, the streak survives up to 48 h.
  const expected = [[2, 110], [3, 120], [4, 140], [5, 180], [6, 250], [7, 400], [8, 400]];
  let balance = 100;
  for (const [streak, amount] of expected) {
    await prisma.economyAccount.update({ where: { guildId_userId: { guildId: g, userId: U(19) } }, data: { lastDaily: hoursAgo(25) } });
    const r = await svc.claimDaily(g, U(19), S());
    balance += amount;
    assert.deepEqual([r.claimed, r.streak, r.amount, r.balance], [true, streak, amount, balance], `day ${streak}`);
  }
  // More than 48 h since the last claim: the streak restarts.
  await prisma.economyAccount.update({ where: { guildId_userId: { guildId: g, userId: U(19) } }, data: { lastDaily: hoursAgo(49) } });
  const reset = await svc.claimDaily(g, U(19), S());
  assert.deepEqual([reset.claimed, reset.streak, reset.amount], [true, 1, 100]);
  // Exactly the cap.
  await prisma.economyAccount.update({ where: { guildId_userId: { guildId: g, userId: U(19) } }, data: { lastDaily: hoursAgo(25), balance: 950 } });
  const capped = await svc.claimDaily(g, U(19), S({ economyMaxBalance: 1000 }));
  assert.deepEqual([capped.claimed, capped.balance, capped.capped], [true, 1000, true]);
});

test('daily: three concurrent claims pay once (R137)', { skip: skipDb }, async () => {
  const g = G(13);
  await seedAccount(g, U(20));
  const results = await Promise.all([1, 2, 3].map(() => svc.claimDaily(g, U(20), S())));
  assert.equal(results.filter((r) => r.claimed).length, 1);
  assert.equal((await account(g, U(20))).balance, 100);
  assert.equal((await account(g, U(20))).dailyStreak, 1);
});

test('work: cooldown, custom cooldown and concurrent claims (R138)', { skip: skipDb }, async () => {
  const g = G(14);
  await seedAccount(g, U(21));
  const first = await svc.claimWork(g, U(21), S());
  assert.deepEqual([first.claimed, first.amount, first.balance, first.boost], [true, 200, 200, null]);
  assert.match(first.line, /^You /);
  assert.ok(Math.abs(first.nextAt - (Date.now() + 3_600_000)) < 5000);
  assert.equal((await svc.claimWork(g, U(21), S())).claimed, false);
  await prisma.economyAccount.update({ where: { guildId_userId: { guildId: g, userId: U(21) } }, data: { lastWork: hoursAgo(0.5) } });
  assert.equal((await svc.claimWork(g, U(21), S({ workCooldownMinutes: 20 }))).claimed, true); // 30 min ago, 20 min cooldown
  await seedAccount(g, U(22));
  const results = await Promise.all([1, 2, 3, 4].map(() => svc.claimWork(g, U(22), S())));
  assert.equal(results.filter((r) => r.claimed).length, 1);
  assert.equal((await account(g, U(22))).balance, 200);
});

test('pity: threshold, cooldown and concurrency (R137)', { skip: skipDb }, async () => {
  const g = G(15);
  await seedAccount(g, U(23), { balance: 50 });
  assert.deepEqual(await svc.claimPity(g, U(23), S()), { claimed: true, amount: 500, balance: 550 });
  const rich = await svc.claimPity(g, U(23), S());
  assert.deepEqual(rich, { claimed: false, tooRich: true, balance: 550 });
  await prisma.economyAccount.update({ where: { guildId_userId: { guildId: g, userId: U(23) } }, data: { balance: 10 } });
  const cooling = await svc.claimPity(g, U(23), S());
  assert.equal(cooling.claimed, false);
  assert.ok(cooling.nextAt > new Date());
  await prisma.economyAccount.update({ where: { guildId_userId: { guildId: g, userId: U(23) } }, data: { lastPity: hoursAgo(13) } });
  assert.equal((await svc.claimPity(g, U(23), S())).claimed, true);
  await seedAccount(g, U(24), { balance: 5 });
  const parallel = await Promise.all([1, 2, 3].map(() => svc.claimPity(g, U(24), S())));
  assert.equal(parallel.filter((r) => r.claimed).length, 1);
  assert.equal((await account(g, U(24))).balance, 505);
});

test('boosts: create limits, apply once per period, roll back on cooldown (amendment 5)', { skip: skipDb }, async () => {
  const g = G(16);
  await settingsSvc.ensureGuildSettings(g);
  const create = (over) => svc.createBoost(g, { name: 'Boost', percent: 50, period: 'DAILY', createdBy: 'admin', ...over });
  await assert.rejects(create({ percent: 0 }), (e) => e.code === 'INVALID' && e.data.reason === 'percent');
  await assert.rejects(create({ percent: 501 }), (e) => e.data.reason === 'percent');
  const base = await create({ name: 'Base', percent: 50, period: 'DAILY' });
  await assert.rejects(create({ name: 'BASE' }), (e) => e.data.reason === 'name-taken');
  const once = await create({ name: 'Welcome', percent: 100, period: 'ONE_TIME', roleId: '777' });
  const rows = await svc.listBoosts(g);
  assert.deepEqual(rows.map((r) => [r.name, r._count.uses]), [['Base', 0], ['Welcome', 0]]);
  assert.equal((await svc.resolveBoost(g, 'welcome')).id, once.id);
  assert.deepEqual((await svc.searchBoosts(g, 'wel')).map((r) => r.name), ['Welcome']);

  // Without the role only the 50 % boost qualifies: 100 -> 150, and it is used up for the day.
  await seedAccount(g, U(25));
  const claim1 = await svc.claimDaily(g, U(25), S(), { memberRoleIds: [] });
  assert.deepEqual([claim1.claimed, claim1.amount, claim1.boost], [true, 150, { name: 'Base', percent: 50 }]);
  assert.equal((await account(g, U(25))).balance, 150);
  // A claim on cooldown must not burn a boost use (the transaction rolls back).
  const usesBefore = await prisma.economyBoostUse.count({ where: { boost: { guildId: g } } });
  assert.equal((await svc.claimDaily(g, U(25), S(), { memberRoleIds: ['777'] })).claimed, false);
  assert.equal(await prisma.economyBoostUse.count({ where: { boost: { guildId: g } } }), usesBefore);
  // Next claim today: the 100 % one-time boost (role 777) wins; streak 2 pays x1.1 = 110, then +100 % = 220.
  await prisma.economyAccount.update({ where: { guildId_userId: { guildId: g, userId: U(25) } }, data: { lastDaily: hoursAgo(25) } });
  const claim2 = await svc.claimDaily(g, U(25), S(), { memberRoleIds: ['777'] });
  assert.deepEqual([claim2.claimed, claim2.streak, claim2.amount, claim2.boost?.name, claim2.balance], [true, 2, 220, 'Welcome', 370]);
  // The one-time boost is gone and the daily boost was already used today: no boost on the third claim.
  await prisma.economyAccount.update({ where: { guildId_userId: { guildId: g, userId: U(25) } }, data: { lastDaily: hoursAgo(25) } });
  const claim3 = await svc.claimDaily(g, U(25), S(), { memberRoleIds: ['777'] });
  assert.deepEqual([claim3.claimed, claim3.streak, claim3.amount, claim3.boost], [true, 3, 120, null]);

  // /work uses the same rule: an enabled boost applies once per period, a disabled one never.
  await seedAccount(g, U(26));
  const work1 = await svc.claimWork(g, U(26), S());
  assert.deepEqual([work1.claimed, work1.amount, work1.boost?.name], [true, 300, 'Base']);
  await prisma.economyAccount.update({ where: { guildId_userId: { guildId: g, userId: U(26) } }, data: { lastWork: hoursAgo(2) } });
  assert.equal((await svc.claimWork(g, U(26), S())).boost, null); // already used this period
  await seedAccount(g, U(27));
  assert.equal((await svc.setBoostEnabled(g, base.id, false)).enabled, false);
  assert.equal((await svc.claimWork(g, U(27), S())).boost, null);
  assert.equal(await svc.setBoostEnabled(g, 'missing', true), null);

  // At most LIMITS.ECONOMY_BOOSTS per guild; deleting cascades the uses.
  for (let n = 0; n < LIMITS.ECONOMY_BOOSTS - 2; n += 1) await create({ name: `Extra ${n}`, percent: n + 1 });
  await assert.rejects(create({ name: 'One too many' }), (e) => e.data.reason === 'limit');
  assert.equal(await svc.deleteBoost(g, once.id), true);
  assert.equal(await svc.deleteBoost(g, once.id), false);
  assert.equal(await prisma.economyBoostUse.count({ where: { boostId: once.id } }), 0);
});

test('boost uses are recorded per period key (D:, once) and are unique per member', { skip: skipDb }, async () => {
  const g = G(17);
  await settingsSvc.ensureGuildSettings(g);
  const b = await svc.createBoost(g, { name: 'Keys', percent: 10, period: 'WEEKLY', createdBy: 'a' });
  await seedAccount(g, U(28));
  await svc.claimWork(g, U(28), S({ timezone: 'Europe/London' }));
  const uses = await prisma.economyBoostUse.findMany({ where: { boostId: b.id } });
  assert.equal(uses.length, 1);
  assert.match(uses[0].periodKey, /^W:\d{4}-W\d{2}$/);
  assert.equal(uses[0].userId, U(28));
});

test('shop: purchases are atomic (stock, price, limit, inventory) and roll back on failure', { skip: skipDb }, async () => {
  const g = G(18);
  await seedAccount(g, U(29), { balance: 1000 });
  const item = await svc.createItem(g, { name: 'Gem', price: 100, type: 'STATIC', stock: 3, canBuyMultiple: true, maxPerUser: 4 });
  const buy = (userId, quantity = 1, itemId = item.id) => svc.purchaseItem({ guildId: g, userId, itemId, quantity, settings: S() });

  const r1 = await buy(U(29), 2);
  assert.deepEqual([r1.total, r1.balance, r1.owned, r1.toInventory, r1.item.stock], [200, 800, 2, true, 1]);
  // Not enough stock / not enough coins / over the per-member limit: everything rolls back.
  await assert.rejects(buy(U(29), 2), (e) => e.code === 'OUT_OF_STOCK');
  await svc.updateItem(g, item.id, { stock: 10, price: 900 });
  await assert.rejects(buy(U(29), 1), (e) => e.code === 'INSUFFICIENT' && e.data.total === 900);
  await svc.updateItem(g, item.id, { price: 1 });
  await assert.rejects(buy(U(29), 3), (e) => e.code === 'MAX_OWNED' && e.data.max === 4); // owns 2, limit 4
  assert.deepEqual([(await svc.resolveItem(g, item.id)).stock, (await account(g, U(29))).balance, await svc.getOwnedQuantity(g, U(29), item.id)], [10, 800, 2]);
  assert.equal((await buy(U(29), 2)).owned, 4);
  await assert.rejects(buy(U(29), 1), (e) => e.code === 'MAX_OWNED'); // the conflict path is capped too
  assert.equal((await account(g, U(29))).balance, 798);
  // Disabled and unknown items are "out of stock" for buyers.
  await svc.updateItem(g, item.id, { enabled: false });
  await assert.rejects(buy(U(29), 1), (e) => e.code === 'OUT_OF_STOCK');
  await assert.rejects(buy(U(29), 1, 'missing'), (e) => e.code === 'OUT_OF_STOCK');
  await assert.rejects(svc.purchaseItem({ guildId: g, userId: U(29), itemId: item.id, quantity: 0, settings: S() }), (e) => e.code === 'INVALID');
});

test('shop: the last copy goes to exactly one of two concurrent buyers', { skip: skipDb }, async () => {
  const g = G(19);
  await seedAccount(g, U(30), { balance: 500 });
  await seedAccount(g, U(31), { balance: 500 });
  const item = await svc.createItem(g, { name: 'Last One', price: 100, stock: 1 });
  const results = await Promise.allSettled([U(30), U(31)].map((u) => svc.purchaseItem({ guildId: g, userId: u, itemId: item.id, quantity: 1, settings: S() })));
  assert.equal(results.filter((r) => r.status === 'fulfilled').length, 1);
  assert.equal(results.find((r) => r.status === 'rejected').reason.code, 'OUT_OF_STOCK');
  assert.equal((await svc.resolveItem(g, item.id)).stock, 0);
  assert.equal((await account(g, U(30))).balance + (await account(g, U(31))).balance, 900);
});

test('shop: a double click cannot exceed the one-per-member limit', { skip: skipDb }, async () => {
  const g = G(20);
  await seedAccount(g, U(32), { balance: 1000 });
  const item = await svc.createItem(g, { name: 'Unique', price: 100, canBuyMultiple: false });
  const results = await Promise.allSettled([1, 2].map(() => svc.purchaseItem({ guildId: g, userId: U(32), itemId: item.id, quantity: 1, settings: S() })));
  assert.equal(results.filter((r) => r.status === 'fulfilled').length, 1);
  assert.equal((await account(g, U(32))).balance, 900);
  assert.equal(await svc.getOwnedQuantity(g, U(32), item.id), 1);
});

test('shop: use-on-buy role items skip the inventory; refundPurchase restores everything', { skip: skipDb }, async () => {
  const g = G(21);
  await seedAccount(g, U(33), { balance: 1000 });
  const role = await svc.createItem(g, { name: 'VIP', price: 300, type: 'ROLE', roleId: '4242', useOnBuy: true, destroyOnUse: true, canBuyMultiple: false, stock: 5 });
  const res = await svc.purchaseItem({ guildId: g, userId: U(33), itemId: role.id, quantity: 1, settings: S() });
  assert.deepEqual([res.toInventory, res.owned, res.balance, res.item.stock], [false, null, 700, 4]);
  assert.equal(await svc.getOwnedQuantity(g, U(33), role.id), 0);
  await svc.refundPurchase({ guildId: g, userId: U(33), item: res.item, quantity: 1, total: res.total, toInventory: res.toInventory, settings: S() });
  assert.deepEqual([(await account(g, U(33))).balance, (await svc.resolveItem(g, role.id)).stock], [1000, 5]);

  // A refund of an inventory purchase also removes the copies (and the row at 0).
  const gem = await svc.createItem(g, { name: 'Gem', price: 50, stock: 2 });
  const bought = await svc.purchaseItem({ guildId: g, userId: U(33), itemId: gem.id, quantity: 2, settings: S() });
  assert.equal(bought.owned, 2);
  await svc.refundPurchase({ guildId: g, userId: U(33), item: bought.item, quantity: 2, total: bought.total, toInventory: true, settings: S() });
  assert.equal(await prisma.inventoryItem.count({ where: { guildId: g, userId: U(33), itemId: gem.id } }), 0);
  assert.deepEqual([(await account(g, U(33))).balance, (await svc.resolveItem(g, gem.id)).stock], [1000, 2]);
});

test('shop: a refund returns the full price even when the buyer reached the server maximum meanwhile', { skip: skipDb }, async () => {
  const g = G(21);
  const capped = S({ economyMaxBalance: 1000 });
  await seedAccount(g, U(51), { balance: 1000 });
  const item = await svc.createItem(g, { name: 'Cap Refund', price: 300, type: 'ROLE', roleId: '4243', useOnBuy: true, destroyOnUse: true, canBuyMultiple: false, stock: 3 });
  const res = await svc.purchaseItem({ guildId: g, userId: U(51), itemId: item.id, quantity: 1, settings: capped });
  assert.equal(res.balance, 700);
  await svc.setBalance(g, U(51), 1000, capped); // the buyer earned their way back to the server maximum before the refund
  await svc.refundPurchase({ guildId: g, userId: U(51), item: res.item, quantity: 1, total: res.total, toInventory: res.toInventory, settings: capped });
  assert.equal((await account(g, U(51))).balance, 1300, 'the refund is not clamped to economyMaxBalance');
  assert.equal((await svc.resolveItem(g, item.id)).stock, 3);
});

test('inventory: consume, restore, give (hard cap), take, search and delete cascade', { skip: skipDb }, async () => {
  const g = G(22);
  await seedAccount(g, U(34), { balance: 0 });
  const potion = await svc.createItem(g, { name: 'Potion', price: 10, type: 'USABLE', destroyOnUse: true });
  const relic = await svc.createItem(g, { name: 'Relic', price: 10, type: 'STATIC' });
  assert.deepEqual(await svc.giveItem({ guildId: g, userId: U(34), itemId: potion.id, quantity: 2 }), { owned: 2 });
  await svc.giveItem({ guildId: g, userId: U(34), itemId: relic.id, quantity: 1 });
  assert.deepEqual(await svc.consumeItem({ guildId: g, userId: U(34), itemId: potion.id }), { remaining: 1 });
  assert.deepEqual(await svc.consumeItem({ guildId: g, userId: U(34), itemId: potion.id }), { remaining: 0 });
  assert.equal(await prisma.inventoryItem.count({ where: { guildId: g, userId: U(34), itemId: potion.id } }), 0); // rows at 0 are deleted
  await assert.rejects(svc.consumeItem({ guildId: g, userId: U(34), itemId: potion.id }), (e) => e.code === 'NOT_OWNED');
  await svc.restoreItem({ guildId: g, userId: U(34), itemId: potion.id });
  assert.equal(await svc.getOwnedQuantity(g, U(34), potion.id), 1);
  // Admin gifts ignore the item's own limits but never exceed the hard cap.
  await assert.rejects(svc.giveItem({ guildId: g, userId: U(34), itemId: potion.id, quantity: LIMITS.ITEM_COPIES_PER_USER }),
    (e) => e.code === 'MAX_OWNED' && e.data.owned === 1 && e.data.max === LIMITS.ITEM_COPIES_PER_USER);
  await assert.rejects(svc.giveItem({ guildId: g, userId: U(34), itemId: 'missing', quantity: 1 }), (e) => e.code === 'ITEM_GONE');
  assert.deepEqual(await svc.takeItem({ guildId: g, userId: U(34), itemId: potion.id, quantity: 5 }), { removed: 1, remaining: 0 });
  await assert.rejects(svc.takeItem({ guildId: g, userId: U(34), itemId: potion.id, quantity: 1 }), (e) => e.code === 'NOT_OWNED');
  // Listing and search.
  assert.deepEqual((await svc.getInventory(g, U(34))).map((r) => [r.item.name, r.quantity]), [['Relic', 1]]);
  await svc.giveItem({ guildId: g, userId: U(34), itemId: potion.id, quantity: 3 });
  assert.deepEqual((await svc.searchInventory(g, U(34), 'pot')).map((r) => r.item.name), ['Potion']);
  assert.deepEqual((await svc.searchInventory(g, U(34), 'rel')).map((r) => r.item.name), []); // collectibles cannot be used
  assert.deepEqual((await svc.searchInventory(g, U(34), 'rel', { usableOnly: false })).map((r) => r.item.name), ['Relic']);
  // Deleting an item removes every copy and reports how many there were.
  assert.deepEqual(await svc.deleteItem(g, potion.id), { copies: 3 });
  assert.equal(await prisma.inventoryItem.count({ where: { itemId: potion.id } }), 0);
  assert.deepEqual(await svc.deleteItem(g, potion.id), { copies: 0 });
});

test('shop items: unique names, caps, guild scoping and lookups', { skip: skipDb }, async () => {
  const g = G(23);
  const other = G(24);
  const a = await svc.createItem(g, { name: 'Sword', price: 50 });
  await svc.createItem(other, { name: 'Sword', price: 70 });
  await assert.rejects(svc.createItem(g, { name: 'SWORD', price: 1 }), (e) => e.data.reason === 'name-taken');
  assert.equal((await svc.findItemByNameInsensitive(g, 'sWoRd')).id, a.id);
  assert.equal(await svc.findItemByNameInsensitive(g, 'sword', { excludeId: a.id }), null);
  assert.equal((await svc.resolveItem(g, '  sword ')).id, a.id);
  assert.equal(await svc.resolveItem(other, a.id), null); // an id from another guild never resolves
  await assert.rejects(svc.giveItem({ guildId: other, userId: U(34), itemId: a.id, quantity: 1 }), (e) => e.code === 'ITEM_GONE');
  assert.equal(await svc.resolveItem(g, a.id, { enabledOnly: true }).then((r) => r?.id), a.id);
  await svc.updateItem(g, a.id, { enabled: false });
  assert.equal(await svc.resolveItem(g, a.id, { enabledOnly: true }), null);
  await assert.rejects(svc.updateItem(other, a.id, { price: 1 }), (e) => e.code === 'ITEM_GONE');
  await assert.rejects(svc.updateItem(g, 'missing', { price: 1 }), (e) => e.code === 'ITEM_GONE');
  const shield = await svc.createItem(g, { name: 'Shield', price: 20 });
  await assert.rejects(svc.updateItem(g, shield.id, { name: 'sword' }), (e) => e.data.reason === 'name-taken');
  assert.equal(await svc.countItems(g), 2);
  assert.deepEqual((await svc.searchItems(g, 'S')).map((r) => r.name), ['Shield', 'Sword']);
  assert.deepEqual((await svc.searchItems(g, 'S', { enabledOnly: true })).map((r) => r.name), ['Shield']);
  assert.deepEqual((await svc.listItems(g, { sort: 'name' })).map((r) => r.name), ['Shield', 'Sword']);
  assert.deepEqual((await svc.listItems(g, { enabledOnly: true })).map((r) => r.name), ['Shield']);
  // The shop cap.
  await prisma.shopItem.createMany({ data: Array.from({ length: LIMITS.SHOP_ITEMS - 2 }, (_, n) => ({ guildId: g, name: `Filler ${n}`, price: 1 })) });
  await assert.rejects(svc.createItem(g, { name: 'Over the cap', price: 1 }), (e) => e.data.reason === 'limit');
});

test('resetEconomy: coins, items, one member, and the import switch', { skip: skipDb }, async () => {
  const g = G(1);
  await prisma.userEconomy.deleteMany({ where: { userId: { in: [U(40), U(41)] } } });
  await prisma.userEconomy.create({ data: { userId: U(40), balance: 9000 } });
  await seedAccount(g, U(35), { balance: 5000, dailyStreak: 4, lastDaily: new Date(), lastWork: new Date(), lastPity: new Date() });
  await seedAccount(g, U(36), { balance: 800, dailyStreak: 2, lastDaily: new Date() });
  const item = await svc.createItem(g, { name: 'ResetMe', price: 5 });
  await svc.giveItem({ guildId: g, userId: U(35), itemId: item.id, quantity: 2 });
  await svc.giveItem({ guildId: g, userId: U(36), itemId: item.id, quantity: 1 });
  const settings = S({ economyStartingBalance: 250 });

  // One member: coins only. The shop catalogue and everybody else stay.
  assert.deepEqual(await svc.resetEconomy({ guildId: g, userId: U(35), what: 'coins', settings }), { accounts: 1, inventories: 0 });
  const one = await account(g, U(35));
  assert.deepEqual([one.balance, one.dailyStreak, one.lastDaily, one.lastWork, one.lastPity], [250, 0, null, null, null]);
  assert.equal((await account(g, U(36))).balance, 800);
  assert.equal((await settingsSvc.getSettings(g, { fresh: true })).economyImportGlobal, true); // a single-member reset leaves the switch alone
  // A member who never touched the economy gets an account first, so a later global import cannot undo the reset.
  assert.deepEqual(await svc.resetEconomy({ guildId: g, userId: U(41), what: 'coins', settings }), { accounts: 1, inventories: 0 });
  assert.equal((await account(g, U(41))).balance, 250);

  assert.deepEqual(await svc.resetEconomy({ guildId: g, userId: U(36), what: 'items', settings }), { accounts: 0, inventories: 1 });
  assert.equal(await svc.getOwnedQuantity(g, U(35), item.id), 2);
  // Everyone, coins and items: balances to the starting balance, the catalogue is kept, the import switch turns off.
  const all = await svc.resetEconomy({ guildId: g, what: 'all', settings });
  assert.equal(all.inventories, 1);
  assert.ok(all.accounts >= 3);
  assert.equal(await prisma.inventoryItem.count({ where: { guildId: g } }), 0);
  assert.ok(await svc.resolveItem(g, item.id));
  assert.equal((await settingsSvc.getSettings(g, { fresh: true })).economyImportGlobal, false);
  // Members who never used the economy here now start at the starting balance instead of their old global balance.
  const latecomer = await svc.getAccount(g, U(40), { ...(await settingsSvc.getSettings(g, { fresh: true })), economyStartingBalance: 250 });
  assert.deepEqual([latecomer.balance, latecomer.importedGlobal], [250, false]);
});

test('coins rewards provider: grant, take, insufficient, disabled module and bad amounts', { skip: skipDb }, async () => {
  const g = G(2);
  await settingsSvc.updateSettings(g, { economyEnabled: true, economyMaxBalance: 1000, economyStartingBalance: 100 });
  const guild = { id: g };
  assert.deepEqual(await svc.grantCoinsReward({ guild, userId: U(42), amount: 50, reason: 'test' }), { ok: true, total: 150 });
  assert.deepEqual(await svc.grantCoinsReward({ guild, userId: U(42), amount: 5000 }), { ok: true, total: 1000 }); // clamped to the server maximum, still ok
  assert.deepEqual(await svc.takeCoinsReward({ guild, userId: U(42), amount: 400 }), { ok: true, total: 600 });
  assert.deepEqual(await svc.takeCoinsReward({ guild, userId: U(42), amount: 601 }), { ok: false, reason: 'insufficient', total: 600 });
  assert.equal((await account(g, U(42))).balance, 600);
  for (const amount of [0, -1, 1.5, COIN_MAX + 1]) assert.equal((await svc.grantCoinsReward({ guild, userId: U(42), amount })).reason, 'invalid-amount');
  await settingsSvc.updateSettings(g, { economyEnabled: false });
  assert.deepEqual(await svc.grantCoinsReward({ guild, userId: U(42), amount: 5 }), { ok: false, reason: 'module-disabled' });
  assert.deepEqual(await svc.takeCoinsReward({ guild, userId: U(42), amount: 5 }), { ok: false, reason: 'module-disabled' });
  assert.equal((await account(g, U(42))).balance, 600);
  await settingsSvc.updateSettings(g, { economyEnabled: true, economyMaxBalance: 1_000_000_000 });
});

// ═══════════════════════ commands against the database (fake interactions) ═══════════════════════

test('/daily and /work: claim, then a private cooldown notice after deferring (R088)', { skip: skipDb }, async () => {
  const g = G(3);
  await settingsSvc.updateSettings(g, { economyEnabled: true, dailyMin: 100, dailyMax: 100, workMin: 200, workMax: 200 });
  await seedAccount(g, U(43));
  for (const [mod, title, cooldown] of [[cmd.daily, 'Daily Reward Claimed', 'Already Claimed'], [cmd.work, 'Shift Complete', 'Still Tired']]) {
    const first = fakeInteraction({ guildId: g, user: { id: U(43), bot: false, username: 'u' } });
    await mod.execute(first);
    assert.deepEqual(first.calls.map((c) => c[0]), ['deferReply', 'editReply']);
    assert.match(titleOf(first.calls[1]), new RegExp(title));
    const second = fakeInteraction({ guildId: g, user: { id: U(43), bot: false, username: 'u' } });
    await mod.execute(second);
    assert.deepEqual(second.calls.map((c) => c[0]), ['deferReply', 'deleteReply', 'followUp']);
    assert.equal(second.calls[2][1].ephemeral, true);
    assert.match(titleOf(second.calls[2]), new RegExp(cooldown));
  }
  const pity = fakeInteraction({ guildId: g, user: { id: U(43), bot: false, username: 'u' } });
  await cmd.pity.execute(pity); // 300 coins: too rich
  assert.match(titleOf(pity.calls[2]), /Pity Not Available/);
});

test('/pay moves coins and refuses an overdraft privately', { skip: skipDb }, async () => {
  const g = G(3);
  await seedAccount(g, U(44), { balance: 500 });
  await seedAccount(g, U(45), { balance: 10 });
  const recipient = { id: U(45), bot: false, username: 'rec', toString: () => `<@${U(45)}>` };
  const member = { id: U(45) };
  const ok = fakeInteraction({ guildId: g, user: { id: U(44), bot: false, username: 'payer' }, options: { member: recipient, 'member:member': member, amount: 200 } });
  await cmd.pay.execute(ok);
  assert.deepEqual(ok.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  assert.match(titleOf(ok.calls[1]), /Coins Sent/);
  assert.deepEqual([(await account(g, U(44))).balance, (await account(g, U(45))).balance], [300, 210]);
  const broke = fakeInteraction({ guildId: g, user: { id: U(44), bot: false, username: 'payer' }, options: { member: recipient, 'member:member': member, amount: 301 } });
  await cmd.pay.execute(broke);
  assert.deepEqual(broke.calls.map((c) => c[0]), ['deferReply', 'deleteReply', 'followUp']);
  assert.match(titleOf(broke.calls[2]), /Insufficient Funds/);
  assert.equal((await account(g, U(44))).balance, 300);
});

test('/shop buy: purchase, economyPurchase hook with channel, and a refund when the role cannot be granted', { skip: skipDb }, async () => {
  const g = G(3);
  await settingsSvc.updateSettings(g, { economyEnabled: true });
  await seedAccount(g, U(46), { balance: 1000 });
  const gem = await svc.createItem(g, { name: 'Shiny Gem', price: 100, stock: 5, buyMessage: 'Enjoy {quantity}x {item}!' });
  const vip = await svc.createItem(g, { name: 'VIP Role', price: 400, type: 'ROLE', roleId: '4242', useOnBuy: true, destroyOnUse: true, canBuyMultiple: false });
  const events = [];
  registerHook({ event: 'economyPurchase', id: 'achievements', run: (ctx) => { events.push(ctx); } }, { pluginId: 'achievements' });

  const channel = { id: '555', name: 'shop' };
  const buyer = (options, rolesAdd) => fakeInteraction({
    guildId: g, user: { id: U(46), bot: false, username: 'buyer' }, channel, sub: 'buy', options,
    guild: {
      id: g, members: { me: { permissions: { has: () => true }, roles: { highest: { position: 10 } } } },
      roles: { cache: new Map([['4242', { id: '4242', position: 1, managed: false, permissions: { has: () => false }, toString: () => '<@&4242>' }]]) },
    },
    member: { displayName: 'Buyer', roles: { cache: new Map(), add: rolesAdd ?? (async () => {}) } },
  });

  const first = buyer({ item: gem.id, quantity: 2 });
  await cmd.shop.execute(first);
  assert.deepEqual(first.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  const embed = first.calls[1][1].embeds[0].data;
  assert.match(embed.title, /Purchase Complete/);
  assert.match(embed.description, /Enjoy 2x Shiny Gem!/);
  assert.deepEqual(events.map((e) => [e.userId, e.quantity, e.totalPrice, e.item.id, e.channel]), [[U(46), 2, 200, gem.id, channel]]);
  assert.equal((await account(g, U(46))).balance, 800);

  // The role cannot be granted after the commit: coins and stock come back, nothing is announced.
  events.length = 0;
  const failing = buyer({ item: vip.id }, async () => { throw new Error('Missing Permissions'); });
  await cmd.shop.execute(failing);
  assert.deepEqual(failing.calls.map((c) => c[0]), ['deferReply', 'deleteReply', 'followUp']);
  assert.match(titleOf(failing.calls[2]), /Purchase Refunded/);
  assert.deepEqual([(await account(g, U(46))).balance, events.length], [800, 0]);

  // A working grant: the role is added once, the buy and use messages are rendered, the inventory stays empty.
  const added = [];
  const working = buyer({ item: vip.id }, async (...args) => { added.push(args); });
  await cmd.shop.execute(working);
  assert.deepEqual(added.map((a) => a[0]), ['4242']);
  assert.match(working.calls[1][1].embeds[0].data.description, /VIP Role/);
  assert.equal(await svc.getOwnedQuantity(g, U(46), vip.id), 0);
  assert.equal(events.length, 1);
  // Unknown item names answer privately.
  const unknown = buyer({ item: 'nope' });
  await cmd.shop.execute(unknown);
  assert.match(titleOf(unknown.calls[2]), /Item Not Found/);
});

test('/shop buy: two use-on-buy role buys sent together charge once (serialised, checked on a fresh member)', { skip: skipDb }, async () => {
  const g = G(25);
  await settingsSvc.updateSettings(g, { economyEnabled: true });
  await seedAccount(g, U(47), { balance: 1000 });
  const vip = await svc.createItem(g, { name: 'VIP Race', price: 400, type: 'ROLE', roleId: '4343', useOnBuy: true, destroyOnUse: true, canBuyMultiple: false });
  const live = new Map(); // the member's roles as Discord holds them (what a forced fetch returns)
  const add = async (roleId) => { await new Promise((r) => setTimeout(r, 20)); live.set(roleId, { id: roleId }); };
  const buyer = () => fakeInteraction({
    guildId: g, user: { id: U(47), bot: false, username: 'racer' }, sub: 'buy', options: { item: vip.id },
    guild: {
      id: g,
      members: {
        me: { permissions: { has: () => true }, roles: { highest: { position: 10 } } },
        fetch: async () => ({ roles: { cache: new Map(live) } }),
      },
      roles: { cache: new Map([['4343', { id: '4343', position: 1, managed: false, permissions: { has: () => false }, toString: () => '<@&4343>' }]]) },
    },
    // Both payloads were built before either grant landed: neither shows the role.
    member: { displayName: 'Racer', roles: { cache: new Map(), add } },
  });
  const [a, b] = [buyer(), buyer()];
  await Promise.all([cmd.shop.execute(a), cmd.shop.execute(b)]);
  assert.equal((await account(g, U(47))).balance, 600, 'charged once');
  const titles = [a, b].map((i) => titleOf(i.calls[i.calls.length - 1]));
  assert.equal(titles.filter((t) => /Purchase Complete/.test(t)).length, 1);
  assert.equal(titles.filter((t) => /Already Owned/.test(t)).length, 1);
});

test('/use consumes a copy, gives the item back when the role grant fails, and refuses collectibles', { skip: skipDb }, async () => {
  const g = G(3);
  await seedAccount(g, U(47), { balance: 0 });
  const potion = await svc.createItem(g, { name: 'Elixir', price: 5, type: 'USABLE', destroyOnUse: true, useMessage: '{user.name} drank {item}.' });
  const relic = await svc.createItem(g, { name: 'Trinket', price: 5, type: 'STATIC' });
  const badge = await svc.createItem(g, { name: 'Badge', price: 5, type: 'ROLE', roleId: '4243', destroyOnUse: true });
  for (const it of [potion, relic, badge]) await svc.giveItem({ guildId: g, userId: U(47), itemId: it.id, quantity: 2 });
  const guild = {
    id: g, members: { me: { permissions: { has: () => true }, roles: { highest: { position: 10 } } } },
    roles: { cache: new Map([['4243', { id: '4243', position: 1, managed: false, permissions: { has: () => false }, toString: () => '<@&4243>' }]]) },
  };
  const user = { id: U(47), bot: false, username: 'drinker' };
  const run = (item, rolesAdd) => {
    const i = fakeInteraction({ guildId: g, user, options: { item }, guild, member: { displayName: 'D', roles: { cache: new Map(), add: rolesAdd ?? (async () => {}) } } });
    return cmd.use.execute(i).then(() => i);
  };
  const used = await run(potion.id);
  assert.match(titleOf(used.calls[1]), /Item Used/);
  assert.equal(used.calls[1][1].embeds[0].data.fields[0].value, '1');
  assert.equal(await svc.getOwnedQuantity(g, U(47), potion.id), 1);
  assert.match((await run(relic.id)).calls[2][1].embeds[0].data.title, /Not Usable/);
  assert.equal(await svc.getOwnedQuantity(g, U(47), relic.id), 2);
  const failed = await run(badge.id, async () => { throw new Error('Missing Permissions'); });
  assert.match(titleOf(failed.calls[2]), /Could Not Use/);
  assert.equal(await svc.getOwnedQuantity(g, U(47), badge.id), 2); // returned
  const granted = await run(badge.id);
  assert.match(titleOf(granted.calls[1]), /Item Used/);
  assert.equal(await svc.getOwnedQuantity(g, U(47), badge.id), 1);
  assert.match((await run('ghost')).calls[2][1].embeds[0].data.title, /Item Not Found/);
});

test('/economy-admin boost add, toggle, list and remove', { skip: skipDb }, async () => {
  const g = G(4);
  await settingsSvc.updateSettings(g, { economyEnabled: true });
  const admin = { id: U(48), bot: false, username: 'admin' };
  const run = (sub, options = {}) => {
    const i = fakeInteraction({ guildId: g, user: admin, group: 'boost', sub, options });
    return cmd.admin.execute(i).then(() => i);
  };
  const added = await run('add', { name: 'Happy Hour', percent: 25, period: 'daily' });
  assert.deepEqual(added.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  assert.match(titleOf(added.calls[1]), /Boost Added/);
  const dup = await run('add', { name: 'happy hour', percent: 5, period: 'weekly' });
  assert.match(titleOf(dup.calls[2]), /Name Taken/);
  const listed = await run('list');
  assert.match(listed.calls[1][1].embeds[0].data.description, /Happy Hour.*\+25%.*Daily.*everyone.*on/);
  const toggled = await run('toggle', { name: 'Happy Hour' });
  assert.match(titleOf(toggled.calls[1]), /Boost Disabled/);
  assert.equal((await svc.resolveBoost(g, 'Happy Hour')).enabled, false);
  const removed = await run('remove', { name: 'Happy Hour' });
  assert.match(titleOf(removed.calls[1]), /Boost Removed/);
  assert.match(titleOf((await run('remove', { name: 'Happy Hour' })).calls[2]), /Boost Not Found/);
  assert.match((await run('list')).calls[1][1].embeds[0].data.description, /No boosts yet/);
});

test('/economy-admin: coins, item lifecycle, limits, casino channels and reset', { skip: skipDb }, async () => {
  const g = G(5);
  await settingsSvc.updateSettings(g, { economyEnabled: true, economyStartingBalance: 500 });
  const admin = { id: U(49), bot: false, username: 'admin' };
  const target = { id: U(50), bot: false, username: 'target', displayName: 'Target', toString: () => `<@${U(50)}>` };
  const channels = new Map([['601', {}], ['602', {}]]);
  const guild = { id: g, channels: { cache: channels }, emojis: { cache: new Map() }, roles: { cache: new Map() } };
  const run = (group, sub, options = {}) => {
    const i = fakeInteraction({ guildId: g, user: admin, group, sub, options, guild });
    return cmd.admin.execute(i).then(() => i);
  };
  const titles = (i) => i.calls.filter((c) => c[1]?.embeds).map((c) => c[1].embeds[0].data.title ?? '');

  // coins
  assert.match(titles(await run('coins', 'give', { member: target, amount: 250 })).join(), /Coins Added/);
  assert.equal((await account(g, U(50))).balance, 750); // 500 starting balance + 250
  assert.match(titles(await run('coins', 'remove', { member: target, amount: 5000 })).join(), /Coins Removed/);
  assert.equal((await account(g, U(50))).balance, 0);
  assert.match(titles(await run('coins', 'set', { member: target, amount: 400 })).join(), /Balance Set/);
  await settingsSvc.updateSettings(g, { economyMaxBalance: 1000 });
  assert.match(titles(await run('coins', 'set', { member: target, amount: 2000 })).join(), /Too High/);
  assert.equal((await account(g, U(50))).balance, 400);
  const bot = await run('coins', 'give', { member: { id: U(2), bot: true, username: 'bot' }, amount: 5 });
  assert.deepEqual(bot.calls.map((c) => c[0]), ['reply']);

  // config limits: lowering the maximum balance clamps balances
  await svc.setBalance(g, U(50), 900, S({ economyMaxBalance: 1000 }));
  const limits = await run('config', 'limits', { max_balance: 600, max_payout: 250 });
  assert.match(limits.calls[1][1].embeds[0].data.description, /1 balance was lowered/);
  assert.equal((await account(g, U(50))).balance, 600);
  const fresh = await settingsSvc.getSettings(g, { fresh: true });
  assert.deepEqual([fresh.economyMaxBalance, fresh.economyMaxPayout], [600, 250]);
  assert.match(titles(await run('config', 'limits', { starting_balance: 700 })).join(), /Invalid Limits/);
  assert.match(titles(await run('config', 'daily', { min: 90, max: 10 })).join(), /Invalid Range/);
  assert.match(titles(await run('config', 'currency', { name: '**x**' })).join(), /Invalid Currency/);
  await run('config', 'currency', { name: 'gems', emoji: '💎' });
  const currency = await settingsSvc.getSettings(g, { fresh: true });
  assert.deepEqual([currency.currencyName, currency.currencyEmoji], ['gems', '💎']);

  // casino channels: allow list, then deny mode in the same call
  const added = await run('config', 'channels', { action: 'add', channel: { id: '601' } });
  assert.match(added.calls[1][1].embeds[0].data.description, /allowed only in: <#601>/);
  assert.match(titles(await run('config', 'channels', { action: 'add', channel: { id: '601' } })).join(), /Not Changed/);
  const denied = await run('config', 'channels', { action: 'add', channel: { id: '602' }, mode: 'deny' });
  assert.match(denied.calls[1][1].embeds[0].data.description, /blocked in: <#601>, <#602>/);
  const deny = await settingsSvc.getSettings(g, { fresh: true });
  assert.deepEqual([deny.economyGameChannelMode, [...deny.economyGameChannelIds]], ['DENY', ['601', '602']]);
  assert.deepEqual(gameChannelCheck(deny, { id: '601' }, guild), { ok: false, mode: 'DENY', deniedIds: ['601', '602'] });
  const listed = await run('config', 'channels', { action: 'list' });
  assert.match(listed.calls[1][1].embeds[0].data.description, /blocked in/);
  await run('config', 'channels', { action: 'clear', mode: 'allow' });
  const cleared = await settingsSvc.getSettings(g, { fresh: true });
  assert.deepEqual([cleared.economyGameChannelMode, [...cleared.economyGameChannelIds]], ['ALLOW', []]);
  const show = await run('config', 'show');
  assert.ok(show.calls[1][1].embeds[0].data.fields.find((f) => f.name === 'Maximum payout').value.includes('250'));

  // items: create, validation, edit, give, remove, list, delete (with confirmation)
  await settingsSvc.updateSettings(g, { economyMaxBalance: 1_000_000_000 });
  const created = await run('item', 'create', { name: 'Trophy', price: 150, description: 'Shiny', stock: 3 });
  assert.match(titles(created).join(), /Item Created/);
  const trophy = await svc.resolveItem(g, 'trophy');
  assert.deepEqual([trophy.type, trophy.canBuyMultiple, trophy.useOnBuy, trophy.stock, trophy.description], ['STATIC', true, false, 3, 'Shiny']);
  assert.match(titles(await run('item', 'create', { name: 'TROPHY', price: 1 })).join(), /Invalid Item/);
  assert.match(titles(await run('item', 'create', { name: 'Crown', price: 1, type: 'ROLE' })).join(), /Invalid Item/); // a Role item needs a role
  assert.match(titles(await run('item', 'create', { name: 'Static', price: 1, use_on_buy: true })).join(), /Invalid Item/);
  assert.match(titles(await run('item', 'edit', { item: trophy.id })).join(), /Nothing To Change/);
  const edited = await run('item', 'edit', { item: trophy.id, price: 99, description: 'none', stock: 0 });
  assert.match(titles(edited).join(), /Item Updated/);
  const after1 = await svc.resolveItem(g, trophy.id);
  assert.deepEqual([after1.price, after1.description, after1.stock], [99, null, 0]);
  assert.match(titles(await run('item', 'edit', { item: trophy.id, name: 'trophy' })).join(), /Item Updated/); // same item: allowed
  assert.match(titles(await run('item', 'give', { member: target, item: trophy.id, quantity: 4 })).join(), /Item Given/);
  assert.equal(await svc.getOwnedQuantity(g, U(50), trophy.id), 4);
  assert.match(titles(await run('item', 'remove', { member: target, item: trophy.id, quantity: 9 })).join(), /Item Removed/);
  assert.match(titles(await run('item', 'remove', { member: target, item: trophy.id })).join(), /Not Owned/);
  assert.match(titles(await run('item', 'give', { member: target, item: 'ghost' })).join(), /Item Not Found/);
  assert.match((await run('item', 'list')).calls[1][1].embeds[0].data.description, /trophy.*99/i);
  await svc.giveItem({ guildId: g, userId: U(50), itemId: trophy.id, quantity: 2 });
  const del = await run('item', 'delete', { item: trophy.id });
  assert.deepEqual(del.calls.map((c) => c[0]), ['deferReply', 'editReply', 'followUp']);
  assert.match(titles(del).join(), /Item Deleted/);
  assert.equal(await svc.resolveItem(g, trophy.id), null);

  // reset: confirmation text, public result, the import switch
  const everyone = await run(null, 'reset', { what: 'coins' });
  assert.deepEqual(everyone.calls.map((c) => c[0]), ['deferReply', 'editReply', 'followUp']);
  assert.equal(everyone.calls[0][1].ephemeral, true);
  assert.match(everyone.calls[1][1].embeds[0].data.description, /Members who never used the economy here will start at/);
  assert.match(titles(everyone).join(), /Economy Reset/);
  assert.equal((await account(g, U(50))).balance, 500);
  assert.equal((await settingsSvc.getSettings(g, { fresh: true })).economyImportGlobal, false);
});
