// The JS side of feat-economy's tests: the JS bot's own economyService. Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-economy-js.mjs pure [--write]      pure helpers on fixed inputs (no servers); --write also
//                                                                  writes rust/fixtures/golden/feat-economy/pure.json
//   node rust/fixtures/gen/feat-economy-js.mjs seed <g> <a> <b>   JS writes: accounts, a claim, items, a boost, a purchase
//   node rust/fixtures/gen/feat-economy-js.mjs read <g> <a> <b>   JS reads what is there (and claims once more)
//   node rust/fixtures/gen/feat-economy-js.mjs sql                 every RAW_SQL statement as Prisma builds it
// The database commands use TEST_DATABASE_URL / TEST_REDIS_URL.
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.LOG_LEVEL = 'error';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const svc = await imp('src', 'services', 'economyService.js');
const settingsSvc = await imp('src', 'services', 'settingsService.js');
const { renderPlaceholders } = await imp('src', 'utils', 'placeholderParser.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

// ── pure ──────────────────────────────────────────────────────────────────────

const SETTINGS = {
  none: null,
  defaults: {},
  big: { economyMaxBalance: 2_000_000_000, economyStartingBalance: 2_000_000_000, dailyMin: 300, dailyMax: 100 },
  deny: { economyGameChannelMode: 'DENY', economyMaxPayout: 750, economyMaxBet: -5, workCooldownMinutes: 0 },
  empty: { currencyName: '', currencyEmoji: '' },
  gems: {
    currencyName: 'gems', currencyEmoji: '💎', workMin: 500, workMax: 20, dailyMin: 0, workCooldownMinutes: 99_999,
    economyGameChannelIds: ['1', '2', '7'], economyStartingBalance: 5, economyMaxBalance: 3,
  },
};

const DATES = ['2026-06-30T23:30:00Z', '2026-01-01T12:00:00Z', '2025-12-29T12:00:00Z', '2021-01-03T12:00:00Z', '2026-12-28T12:00:00Z',
  '2024-02-29T23:59:59Z', '2027-01-01T05:00:00Z', '2026-03-29T00:30:00Z', '2026-10-25T00:30:00Z', '1999-12-31T23:00:00Z'];
const ZONES = ['UTC', 'Europe/London', 'America/Los_Angeles', 'Pacific/Kiritimati', 'Asia/Kolkata', 'Not/AZone'];
const EMOJIS = ['🪙', '<:coin:123456789012345678>', '<a:spin:123456789012345678>', '❤️', '👍🏽', 'abc', '<@123456789012345678>', '',
  '🪙 coins', '🇺🇸', '1️⃣', '👨‍👩‍👧', '🪙🪙🪙🪙🪙🪙🪙🪙🪙🪙🪙', '#️⃣', '  💎  ', '<:c:123456789012345678>', '<:coin:1234567890123456>',
  '©', '☺', '🏳️‍🌈', 'x🪙', '🪙x', '‍'];
const NAMES = ['  gold   coins ', '**x**', '@everyone', 'a'.repeat(33), 'a'.repeat(32), 'line\nbreak', '', '`code`', '<#1>', 'none', 'NONE',
  'None ', 'a'.repeat(51), 'a'.repeat(50), '   ', 'two\nlines', 'tab\there', 'émoji 🪙', ' nbsp ', 'back\\slash', 'r\rx',
  'Ⅻ', '🪙'.repeat(16), '🪙'.repeat(17), 'x'.repeat(25) + '🪙'.repeat(4)];

function pure() {
  const out = {};
  out.economyConfig = Object.fromEntries(Object.entries(SETTINGS).map(([k, s]) => [k, svc.economyConfig(s)]));
  out.formatCoins = [];
  for (const [k, s] of Object.entries(SETTINGS)) {
    for (const n of [0, 5, 1234, 1_000_000_000, -7]) {
      out.formatCoins.push({ settings: k, n, bold: svc.formatCoins(n, s), plain: svc.formatCoins(n, s, { bold: false }), net: svc.formatNet(n, s) });
    }
  }
  out.daily = [];
  for (const base of [1, 50, 51, 55, 100, 999_999]) {
    for (let streak = 0; streak <= 9; streak += 1) out.daily.push({ base, streak, amount: svc.dailyAmount(base, streak), label: svc.streakMultiplierLabel(streak) });
  }
  out.periodKeys = [];
  for (const d of DATES) {
    for (const tz of ZONES) {
      for (const period of ['ONE_TIME', 'DAILY', 'WEEKLY', 'MONTHLY']) out.periodKeys.push({ date: d, tz, period, key: svc.boostPeriodKey(period, tz, new Date(d)) });
    }
  }
  out.emoji = EMOJIS.map((s) => ({ input: s, result: svc.validateCurrencyEmoji(s) }));
  out.names = NAMES.map((s) => ({
    input: s, currency: svc.validateCurrencyName(s), item: svc.validateItemName(s), boost: svc.validateBoostName(s),
  }));
  const b = (id, percent, roleId, createdAt, enabled = true) => ({ id, percent, roleId, createdAt: new Date(createdAt), enabled });
  const boosts = [
    b('a', 50, null, '2026-01-02'), b('b', 100, 'R1', '2026-01-03'), b('c', 100, null, '2026-01-01'),
    b('d', 200, 'R2', '2026-01-04'), b('e', 300, null, '2026-01-05', false), b('f', 50, null, '2026-01-01'), b('g', 50, null, '2026-01-01'),
  ];
  out.orderBoosts = { boosts, cases: [[], ['R1'], ['R1', 'R2'], ['R9']].map((roles) => ({ roles, ids: svc.orderBoosts(boosts, roles).map((x) => x.id) })) };
  out.clampPayout = [[1000, 100, 500], [1000, 1000, 500], [2000, 1000, 500], [0, 1000, 500], [36_000, 1000, 0], [600, 100, 500]]
    .map(([p, s, m]) => ({ payout: p, stake: s, max: m, result: svc.clampPayout(p, s, m) }));
  out.boosted = [[120, 50], [101, 50], [100, 500], [7, 1], [0, 10], [999_999, 333]].map(([base, pct]) => ({ base, pct, amount: svc.boostedAmount(base, pct) }));
  out.itemVars = [
    { name: 'VIP', price: 500, quantity: 3, total: 1500, settings: 'none' },
    { name: 'Gem *x*', price: 1_234_567, quantity: 1, total: 1_234_567, settings: 'gems' },
  ].map((c) => {
    const vars = svc.itemVars({ name: c.name, price: c.price }, { quantity: c.quantity, total: c.total, settings: SETTINGS[c.settings] });
    return { ...c, vars, buy: renderPlaceholders(svc.DEFAULT_BUY_MESSAGE, { vars }), use: renderPlaceholders(svc.DEFAULT_USE_MESSAGE, { vars }) };
  });
  out.itemLabel = ['VIP', '**bold**', 'under_score', 'x'.repeat(120), '`tick`'].map((n) => ({ name: n, label: svc.itemLabel({ name: n }) }));
  out.ownership = [];
  for (const type of ['STATIC', 'USABLE', 'ROLE']) {
    for (const useOnBuy of [false, true]) {
      for (const destroyOnUse of [false, true]) {
        for (const [canBuyMultiple, maxPerUser] of [[false, 200], [true, 500], [true, 3]]) {
          const item = { type, useOnBuy, destroyOnUse, canBuyMultiple, maxPerUser };
          out.ownership.push({ ...item, max: svc.effectiveMaxOwned(item), toInventory: svc.goesToInventory(item) });
        }
      }
    }
  }
  const guild = { channels: { cache: new Map([['1', {}], ['2', {}]]) } };
  out.gameChannels = [];
  for (const mode of ['ALLOW', 'DENY']) {
    for (const ids of [[], ['1'], ['7'], ['7', '1'], ['1', '2']]) {
      for (const ch of [{ id: '1' }, { id: '3' }, { id: '7' }, { id: '9', parentId: '1', isThread: () => true }, { id: '9', parentId: '2', isThread: () => true }]) {
        out.gameChannels.push({
          mode, ids, channel: ch.id, parent: ch.parentId ?? null,
          result: svc.gameChannelCheck({ economyGameChannelIds: ids, economyGameChannelMode: mode }, ch, guild),
        });
      }
    }
  }
  out.streakWindows = [0, 1, 24, 47.99, 48, 48.01, 200].map((h) => {
    const now = new Date('2026-10-10T12:00:00Z');
    const last = new Date(now.getTime() - h * 3_600_000);
    return { hours: h, next: svc.nextDailyStreak(last, 3, now), active: svc.activeStreak(last, 5, now) };
  });
  out.constants = {
    PITY_THRESHOLD: svc.PITY_THRESHOLD, PITY_AMOUNT: svc.PITY_AMOUNT, PITY_COOLDOWN_MS: svc.PITY_COOLDOWN_MS,
    DAILY_COOLDOWN_MS: svc.DAILY_COOLDOWN_MS, DAILY_STREAK_WINDOW_MS: svc.DAILY_STREAK_WINDOW_MS,
    DAILY_MULTIPLIER_TENTHS: svc.DAILY_MULTIPLIER_TENTHS, ITEM_NAME_MAX: svc.ITEM_NAME_MAX, ITEM_DESCRIPTION_MAX: svc.ITEM_DESCRIPTION_MAX,
    ITEM_MESSAGE_MAX: svc.ITEM_MESSAGE_MAX, ITEM_IMAGE_MAX: svc.ITEM_IMAGE_MAX, MAX_STOCK: svc.MAX_STOCK, RANGE_MAX: svc.RANGE_MAX,
    WORK_COOLDOWN_MAX_MINUTES: svc.WORK_COOLDOWN_MAX_MINUTES, RICHEST_LIMIT: svc.RICHEST_LIMIT, GAME_CHANNELS_MAX: svc.GAME_CHANNELS_MAX,
    BOOST_NAME_MAX: svc.BOOST_NAME_MAX, BOOST_PERCENT_MAX: svc.BOOST_PERCENT_MAX, DEFAULT_BUY_MESSAGE: svc.DEFAULT_BUY_MESSAGE,
    DEFAULT_USE_MESSAGE: svc.DEFAULT_USE_MESSAGE, ITEM_TYPE_LABELS: svc.ITEM_TYPE_LABELS, BOOST_PERIOD_LABELS: svc.BOOST_PERIOD_LABELS,
    WORK_LINES: svc.WORK_LINES,
  };
  out.settings = SETTINGS;
  out.rawSql = svc.RAW_SQL.map(({ name, build }) => ({ name, sql: build().sql, values: build().values.map((v) => (typeof v === 'bigint' ? Number(v) : v)) }));
  return out;
}

// ── database ──────────────────────────────────────────────────────────────────

async function readAll(g, a, b) {
  const settings = await settingsSvc.getSettings(g, { fresh: true });
  return {
    peekA: await svc.peekAccount(g, a, settings),
    peekB: await svc.peekAccount(g, b, settings),
    inventoryA: await svc.getInventory(g, a),
    inventoryB: await svc.getInventory(g, b),
    searchB: await svc.searchInventory(g, b, 'e', { usableOnly: false }),
    items: await svc.listItems(g, { sort: 'name' }),
    search: await svc.searchItems(g, '', {}),
    boosts: await svc.listBoosts(g),
    top: await svc.topAccounts(g),
    rankA: await svc.getRank(g, (await svc.peekAccount(g, a, settings)).balance),
    count: await svc.countItems(g),
  };
}

async function seed(g, a, b) {
  await settingsSvc.updateSettings(g, { economyEnabled: true, dailyMin: 100, dailyMax: 100, workMin: 200, workMax: 200, timezone: 'Europe/London' });
  await prisma.userEconomy.upsert({ where: { userId: a }, create: { userId: a, balance: 777 }, update: { balance: 777 } });
  const settings = await settingsSvc.getSettings(g, { fresh: true });
  const account = await svc.getAccount(g, a, settings);
  await svc.credit(g, b, 50, settings);
  const boost = await svc.createBoost(g, { name: 'Base Boost', percent: 50, period: 'DAILY', createdBy: a });
  await svc.createBoost(g, { name: 'Role Boost', percent: 100, period: 'WEEKLY', roleId: '4242', createdBy: a });
  const daily = await svc.claimDaily(g, a, settings);
  const gem = await svc.createItem(g, { name: 'Gem', price: 100, stock: 5, description: 'Shiny', buyMessage: 'Enjoy {item}!' });
  const potion = await svc.createItem(g, { name: 'Elixir', price: 10, type: 'USABLE', destroyOnUse: true, maxPerUser: 3 });
  await svc.createItem(g, { name: 'Retired', price: 1, enabled: false });
  const purchase = await svc.purchaseItem({ guildId: g, userId: b, itemId: gem.id, quantity: 2, settings });
  await svc.giveItem({ guildId: g, userId: a, itemId: potion.id, quantity: 2 });
  return { account, boost, daily, gem, purchase, ...(await readAll(g, a, b)) };
}

async function read(g, a, b) {
  const all = await readAll(g, a, b);
  const settings = await settingsSvc.getSettings(g, { fresh: true });
  // The claims the Rust side made must be seen as such: a second /work is on cooldown, /pity is refused.
  return { ...all, workA: await svc.claimWork(g, a, settings), pityB: await svc.claimPity(g, b, settings) };
}

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'pure') {
    const out = pure();
    if (args.includes('--write')) {
      const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-economy');
      mkdirSync(dir, { recursive: true });
      writeFileSync(join(dir, 'pure.json'), `${JSON.stringify(out, null, 1)}\n`);
    }
    answer(out);
  } else if (cmd === 'seed') {
    answer(await seed(...args));
  } else if (cmd === 'read') {
    answer(await read(...args));
  } else if (cmd === 'sql') {
    answer(svc.RAW_SQL.map(({ name, build }) => ({ name, sql: build().sql })));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
