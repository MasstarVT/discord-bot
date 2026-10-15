// Per-server economy (pkg-economy.md): accounts with the lazy global copy, escrowed bets, streak daily, work, pity,
// coin boosts, transfers, shop and inventory transactions, reset, the `coins` rewards provider and the casino lock.
// This is the only file of the package that touches Prisma.
//
// Invariants (pkg-economy.md §3.2):
//  - 0 <= EconomyAccount.balance <= economyConfig(settings).maxBalance <= COIN_MAX (1e9) < 2^31. Every balance write is
//    ONE SQL statement (or one transaction) using ::bigint maths and LEAST(..., cap) / a conditional WHERE. There is no
//    read-modify-write in JS, so int4 overflow and double spending are impossible (R007, R136, R207, R236). The one
//    exception to the per-server cap: refundPurchase returns the full price up to COIN_MAX, so a refund is never lost.
//  - UserEconomy (the legacy global wallet) is read only, and only by getAccount / peekAccount (the lazy copy).
//  - Never call anything that uses the global `prisma` (getAccount, ensureGuildSettings, getSettings, ...) from inside a
//    $transaction callback: the pool may hold a single connection, so such a call would wait for the transaction itself.
//    Everything that needs the pool runs BEFORE the transaction opens; inside it only `tx` is used.
import { escapeMarkdown } from 'discord.js';
import { Prisma } from '@prisma/client';
import { randomInt, randomUUID } from 'node:crypto';
import prisma, { SQL_NOW } from '../database/client.js';
import { COIN_MAX, LIMITS } from '../config/constants.js';
import { isModuleEnabled } from '../config/modules.js';
import { ensureGuildSettings, getSettingsSafe, updateSettings } from './settingsService.js';
import { sleep } from '../utils/async.js';
import { errorEmbed } from '../utils/embedBuilder.js';
import { isInteractionGone } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { clampText, formatNumber } from '../utils/text.js';
import { zonedParts } from '../utils/time.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const PITY_THRESHOLD = 100;
export const PITY_AMOUNT = 500;
export const PITY_COOLDOWN_MS = 12 * 60 * 60 * 1000;
export const DAILY_COOLDOWN_MS = 24 * 60 * 60 * 1000;
export const DAILY_STREAK_WINDOW_MS = 48 * 60 * 60 * 1000;
export const DAILY_MULTIPLIER_TENTHS = Object.freeze([10, 11, 12, 14, 18, 25, 40]); // day 1..6, then day 7+
export const ITEM_NAME_MAX = 50;
export const ITEM_DESCRIPTION_MAX = 200;
export const ITEM_MESSAGE_MAX = 1000;
export const ITEM_IMAGE_MAX = 500;
export const MAX_STOCK = 1_000_000;
export const RANGE_MAX = 1_000_000; // daily/work min/max upper bound
export const WORK_COOLDOWN_MAX_MINUTES = 10_080;
export const RICHEST_LIMIT = 100;
export const GAME_CHANNELS_MAX = LIMITS.ECONOMY_GAME_CHANNELS;
export const BOOST_NAME_MAX = 32;
export const BOOST_PERCENT_MAX = 500;
export const DEFAULT_BUY_MESSAGE = 'You bought **{quantity}× {item}** for {currency.emoji} **{coins}** {currency}!';
export const DEFAULT_USE_MESSAGE = '{user} used **{item}**!';
export const ITEM_TYPE_LABELS = Object.freeze({ STATIC: '📦 Collectible', USABLE: '✨ Usable', ROLE: '🎭 Role' });
export const BOOST_PERIOD_LABELS = Object.freeze({ ONE_TIME: 'One-time', DAILY: 'Daily', WEEKLY: 'Weekly', MONTHLY: 'Monthly' });
export const WORK_LINES = Object.freeze([
  'You delivered pizzas all over town',
  'You fixed a bug in production',
  "You walked the neighbours' dogs",
  'You streamed for six hours straight',
  'You moderated a very busy chat',
  'You baked bread at the local bakery',
  'You mowed lawns all afternoon',
  'You tutored a student in maths',
  'You drove a taxi through rush hour',
  'You washed cars at the car wash',
  'You sold lemonade on the corner',
  'You designed a logo for a client',
]);

/**
 * Expected failure of an economy operation.
 * codes: 'INVALID' | 'INSUFFICIENT' | 'MAX_BET' | 'OUT_OF_STOCK' | 'MAX_OWNED' | 'RECIPIENT_CAP' | 'NOT_OWNED' | 'ITEM_GONE'
 */
export class EconomyError extends Error {
  constructor(code, data = {}) {
    super(code);
    this.name = 'EconomyError';
    this.code = code;
    this.data = data;
  }
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/** @returns {number} `n` truncated and clamped to min..max; a non-finite value gives `min` */
export function clampInt(n, min, max) {
  return Number.isFinite(n) ? Math.min(max, Math.max(min, Math.trunc(n))) : min;
}

/** @returns {bigint} */
export function big(n) {
  return BigInt(Math.trunc(n));
}

const intOr = (v, fallback) => (Number.isInteger(v) ? v : fallback);

/**
 * Sanitised economy settings. Accepts null (every default). A missing or non-integer value falls back to the default
 * before clamping.
 * @param {object|null|undefined} settings GuildSettings row
 */
export function economyConfig(settings) {
  const s = settings ?? {};
  const maxBalance = clampInt(intOr(s.economyMaxBalance, COIN_MAX), 1, COIN_MAX);
  const startingBalance = clampInt(intOr(s.economyStartingBalance, 1000), 0, maxBalance);
  const maxBet = clampInt(intOr(s.economyMaxBet, 0), 0, COIN_MAX);
  const dailyMin = clampInt(intOr(s.dailyMin, 50), 1, RANGE_MAX);
  const dailyMax = Math.max(dailyMin, clampInt(intOr(s.dailyMax, 120), 1, RANGE_MAX));
  const workMin = clampInt(intOr(s.workMin, 100), 1, RANGE_MAX);
  const workMax = Math.max(workMin, clampInt(intOr(s.workMax, 300), 1, RANGE_MAX));
  return {
    currencyName: s.currencyName || 'coins',
    currencyEmoji: s.currencyEmoji || '🪙',
    startingBalance,
    maxBalance,
    maxBet,
    dailyMin,
    dailyMax,
    workMin,
    workMax,
    workCooldownMinutes: clampInt(intOr(s.workCooldownMinutes, 60), 1, WORK_COOLDOWN_MAX_MINUTES),
    gameChannelIds: [...(s.economyGameChannelIds ?? [])],
    gameChannelMode: s.economyGameChannelMode === 'DENY' ? 'DENY' : 'ALLOW',
    maxPayout: clampInt(intOr(s.economyMaxPayout, 0), 0, COIN_MAX),
  };
}

/** `🪙 **1,234** coins` */
export function formatCoins(n, settings, { bold = true } = {}) {
  const { currencyEmoji, currencyName } = economyConfig(settings);
  const b = bold ? '**' : '';
  return `${currencyEmoji} ${b}${formatNumber(n)}${b} ${currencyName}`;
}

/** `+🪙 **5** coins`, `−…` (U+2212) or `±…` */
export function formatNet(net, settings) {
  if (net > 0) return `+${formatCoins(net, settings)}`;
  if (net < 0) return `−${formatCoins(-net, settings)}`;
  return `±${formatCoins(0, settings)}`;
}

/** Streak multiplier in tenths for a streak day (1..6, then 7+). */
export function streakTenths(streak) {
  return DAILY_MULTIPLIER_TENTHS[Math.min(Math.max(Math.trunc(streak) || 1, 1), 7) - 1];
}

/** `×1`, `×1.1`, ... `×4` */
export function streakMultiplierLabel(streak) {
  return `×${streakTenths(streak) / 10}`;
}

/** Round-half-up of `base × multiplier`; identical to the SQL expression of claimDaily. */
export function dailyAmount(base, streak) {
  return Math.floor((base * streakTenths(streak) + 5) / 10);
}

/** Streak a claim made now would have (display/preview only; claimDaily decides in SQL). */
export function nextDailyStreak(lastDaily, currentStreak, now = new Date()) {
  return !lastDaily || now - lastDaily > DAILY_STREAK_WINDOW_MS ? 1 : Math.min(currentStreak, 999_999) + 1;
}

/** The streak that is still alive (0 when more than 48 h passed since the last claim). */
export function activeStreak(lastDaily, currentStreak, now = new Date()) {
  return lastDaily && now - lastDaily <= DAILY_STREAK_WINDOW_MS ? currentStreak : 0;
}

/**
 * Whether a casino game may run in `channel`. Only channels that still exist in the guild count; an empty (or fully
 * deleted) list means "everywhere" in both modes (fail-open, 00-overview rule 20). A thread counts as its parent.
 * @returns {{ ok: true } | { ok: false, mode: 'ALLOW', allowedIds: string[] } | { ok: false, mode: 'DENY', deniedIds: string[] }}
 */
export function gameChannelCheck(settings, channel, guild) {
  const cfg = economyConfig(settings);
  const ids = cfg.gameChannelIds.filter((id) => guild?.channels?.cache?.has(id));
  if (ids.length === 0) return { ok: true };
  const parentId = channel?.isThread?.() ? channel.parentId : null;
  const listed = ids.includes(channel?.id) || (parentId != null && ids.includes(parentId));
  if (cfg.gameChannelMode === 'DENY') return listed ? { ok: false, mode: 'DENY', deniedIds: ids } : { ok: true };
  return listed ? { ok: true } : { ok: false, mode: 'ALLOW', allowedIds: ids };
}

/**
 * Limits the gross payout of one game to `stake + maxPayout` (net winnings capped at maxPayout; 0 = no limit).
 * @returns {{ payout: number, capped: boolean }}
 */
export function clampPayout(payout, stake, maxPayout) {
  if (!(maxPayout > 0)) return { payout, capped: false };
  const limit = stake + maxPayout;
  return payout > limit ? { payout: limit, capped: true } : { payout, capped: false };
}

const CURRENCY_NAME_BAD = /[\r\n`*_~|<>@#\\]/;
/** @returns {{ ok: true, value: string } | { ok: false, reason: string }} */
export function validateCurrencyName(str) {
  const raw = String(str ?? '').trim();
  const value = raw.replace(/\s+/g, ' '); // collapsing whitespace would hide a newline, so the raw text is checked below
  if (value.length < 1 || value.length > 32) return { ok: false, reason: 'The currency name must be 1 to 32 characters.' };
  if (CURRENCY_NAME_BAD.test(raw)) {
    return { ok: false, reason: 'Use letters, numbers, spaces and simple punctuation only (no markdown, mentions or newlines).' };
  }
  return { ok: true, value };
}

const CUSTOM_EMOJI_RE = /^<a?:[A-Za-z0-9_]{2,32}:(\d{17,20})>$/;
const UNICODE_EMOJI_RE = /^(?:\p{Extended_Pictographic}|\p{Regional_Indicator})(?:\p{Extended_Pictographic}|\p{Regional_Indicator}|\p{Emoji_Modifier}|️|‍|⃣)*$/u;
/** @returns {{ ok: true, value: string, customId: string|null } | { ok: false, reason: string }} */
export function validateCurrencyEmoji(str) {
  const value = String(str ?? '').trim();
  const custom = CUSTOM_EMOJI_RE.exec(value);
  if (custom) return { ok: true, value, customId: custom[1] };
  if ([...value].length >= 1 && [...value].length <= 10 && UNICODE_EMOJI_RE.test(value)) return { ok: true, value, customId: null };
  return { ok: false, reason: 'That is not an emoji. Use a standard emoji like 🪙 or a custom emoji like <:coin:123…>.' };
}

/** @returns {{ ok: true, value: string } | { ok: false, reason: string }} */
export function validateItemName(str) {
  const raw = String(str ?? '').trim();
  const value = raw.replace(/\s+/g, ' '); // collapsing whitespace would hide a newline, so the raw text is checked
  if (value.length < 1 || value.length > ITEM_NAME_MAX) return { ok: false, reason: `The name must be 1 to ${ITEM_NAME_MAX} characters.` };
  if (/[\r\n]/.test(raw)) return { ok: false, reason: 'The name must be a single line.' };
  if (value.toLowerCase() === 'none') return { ok: false, reason: 'Pick a different name.' };
  return { ok: true, value };
}

/** @returns {{ ok: true, value: string } | { ok: false, reason: string }} same rules as an item name, up to BOOST_NAME_MAX */
export function validateBoostName(str) {
  const raw = String(str ?? '').trim();
  const value = raw.replace(/\s+/g, ' ');
  if (value.length < 1 || value.length > BOOST_NAME_MAX) return { ok: false, reason: `The name must be 1 to ${BOOST_NAME_MAX} characters.` };
  if (/[\r\n]/.test(raw)) return { ok: false, reason: 'The name must be a single line.' };
  return { ok: true, value };
}

/** How many copies of an item one member may own. */
export function effectiveMaxOwned(item) {
  return item.canBuyMultiple ? Math.min(item.maxPerUser, LIMITS.ITEM_COPIES_PER_USER) : 1;
}

/** An item that is used on purchase and destroyed by use never reaches the inventory. */
export function goesToInventory(item) {
  return !(item.useOnBuy && item.destroyOnUse && item.type !== 'STATIC');
}

/** Placeholder variables of the buy and use messages. */
export function itemVars(item, { quantity = 1, total = item.price, settings } = {}) {
  const cfg = economyConfig(settings);
  return {
    item: item.name,
    quantity: String(quantity),
    coins: formatNumber(total),
    price: formatNumber(item.price),
    currency: cfg.currencyName,
    'currency.emoji': cfg.currencyEmoji,
  };
}

/** Bold, escaped, clamped item name. */
export function itemLabel(item) {
  return `**${clampText(escapeMarkdown(item.name), 100)}**`;
}

// ── Boost helpers (pure) ──────────────────────────────────────────────────────

/**
 * The key that decides how often a boost may be used by one member: 'once', 'D:YYYY-MM-DD', 'W:YYYY-Www' (ISO week) or
 * 'M:YYYY-MM', in the guild's timezone.
 * @param {'ONE_TIME'|'DAILY'|'WEEKLY'|'MONTHLY'} period
 * @param {string} tz IANA timezone (invalid = UTC)
 * @param {Date} [now]
 */
export function boostPeriodKey(period, tz, now = new Date()) {
  if (period === 'ONE_TIME') return 'once';
  const p = zonedParts(now, tz);
  const mm = String(p.month).padStart(2, '0');
  if (period === 'MONTHLY') return `M:${String(p.year).padStart(4, '0')}-${mm}`;
  if (period === 'DAILY') return `D:${String(p.year).padStart(4, '0')}-${mm}-${String(p.day).padStart(2, '0')}`;
  // ISO 8601 week: the week belongs to the year that holds its Thursday.
  const d = new Date(Date.UTC(p.year, p.month - 1, p.day));
  d.setUTCDate(d.getUTCDate() - ((d.getUTCDay() + 6) % 7) + 3);
  const isoYear = d.getUTCFullYear();
  const week = 1 + Math.floor((d - Date.UTC(isoYear, 0, 1)) / 86_400_000 / 7);
  return `W:${String(isoYear).padStart(4, '0')}-W${String(week).padStart(2, '0')}`;
}

/**
 * Enabled boosts a member qualifies for (holds `roleId`, or everyone when it is null), highest percent first, then oldest.
 * @param {Array<{ id?: string, enabled?: boolean, roleId: string|null, percent: number, createdAt: Date|string|number }>} boosts
 * @param {Iterable<string>} memberRoleIds
 */
export function orderBoosts(boosts, memberRoleIds) {
  const roles = new Set(memberRoleIds ?? []);
  const time = (b) => new Date(b.createdAt).getTime() || 0;
  return (boosts ?? [])
    .filter((b) => b.enabled !== false && (b.roleId == null || roles.has(b.roleId)))
    .sort((a, b) => b.percent - a.percent || time(a) - time(b) || String(a.id ?? '').localeCompare(String(b.id ?? '')));
}

/** `base` multiplied by (100 + percent) %, rounded down. */
export function boostedAmount(base, percent) {
  return Math.floor((base * (100 + percent)) / 100);
}

// ── Raw SQL (every statement is a builder; RAW_SQL exports a sample of each) ──
// Prisma DateTime columns are timestamp(3) WITHOUT time zone holding UTC: every timestamp uses SQL_NOW. Raw UPDATEs set
// "updatedAt" (Prisma's @updatedAt is client-side only); raw INSERTs supply "id", "createdAt" and "updatedAt". Amounts are
// passed as bigint and cast; only int/text/timestamp columns are RETURNed.

const SQL = {
  insertAccount: (guildId, userId, balance, importedGlobal) => Prisma.sql`
    INSERT INTO "EconomyAccount" ("id","guildId","userId","balance","dailyStreak","importedGlobal","createdAt","updatedAt")
    VALUES (${randomUUID()}, ${guildId}, ${userId}, ${big(balance)}::bigint::int, 0, ${importedGlobal}, ${SQL_NOW}, ${SQL_NOW})
    ON CONFLICT ("guildId","userId") DO NOTHING`,

  credit: (guildId, userId, amount, cap) => Prisma.sql`
    UPDATE "EconomyAccount"
    SET "balance" = GREATEST("balance"::bigint, LEAST("balance"::bigint + ${big(amount)}::bigint, ${big(cap)}::bigint))::int,
        "updatedAt" = ${SQL_NOW}
    WHERE "guildId" = ${guildId} AND "userId" = ${userId}
    RETURNING "balance"`,

  debit: (guildId, userId, amount) => Prisma.sql`
    UPDATE "EconomyAccount"
    SET "balance" = ("balance"::bigint - ${big(amount)}::bigint)::int, "updatedAt" = ${SQL_NOW}
    WHERE "guildId" = ${guildId} AND "userId" = ${userId} AND "balance" >= ${big(amount)}::bigint
    RETURNING "balance"`,

  lockAccounts: (guildId, fromId, toId) => Prisma.sql`
    SELECT "id" FROM "EconomyAccount"
    WHERE "guildId" = ${guildId} AND "userId" IN (${fromId}, ${toId})
    ORDER BY "userId" FOR UPDATE`,

  creditChecked: (guildId, userId, amount, cap) => Prisma.sql`
    UPDATE "EconomyAccount"
    SET "balance" = ("balance"::bigint + ${big(amount)}::bigint)::int, "updatedAt" = ${SQL_NOW}
    WHERE "guildId" = ${guildId} AND "userId" = ${userId}
      AND "balance"::bigint + ${big(amount)}::bigint <= ${big(cap)}::bigint
    RETURNING "balance"`,

  removeCoins: (guildId, userId, amount) => Prisma.sql`
    WITH old AS (
      SELECT "id", "balance" FROM "EconomyAccount"
      WHERE "guildId" = ${guildId} AND "userId" = ${userId} FOR UPDATE
    )
    UPDATE "EconomyAccount" a
    SET "balance" = GREATEST(old."balance"::bigint - ${big(amount)}::bigint, 0)::int, "updatedAt" = ${SQL_NOW}
    FROM old WHERE a."id" = old."id"
    RETURNING a."balance" AS "balance", old."balance" AS "before"`,

  // base: unboosted base reward; boostPercent: 0 when no boost applies. The streak (ns) exists only inside this statement.
  claimDaily: (guildId, userId, base, cap, boostPercent) => Prisma.sql`
    WITH cur AS (
      SELECT "id",
             CASE WHEN "lastDaily" IS NULL OR "lastDaily" < ${SQL_NOW} - interval '48 hours'
                  THEN 1 ELSE LEAST("dailyStreak", 999999) + 1 END AS ns
      FROM "EconomyAccount"
      WHERE "guildId" = ${guildId} AND "userId" = ${userId}
        AND ("lastDaily" IS NULL OR "lastDaily" <= ${SQL_NOW} - interval '24 hours')
      FOR UPDATE
    )
    UPDATE "EconomyAccount" a SET
      "dailyStreak" = cur.ns,
      "balance" = GREATEST(a."balance"::bigint, LEAST(
          a."balance"::bigint
          + (((${big(base)}::bigint * (CASE LEAST(cur.ns, 7) WHEN 1 THEN 10 WHEN 2 THEN 11 WHEN 3 THEN 12
                                       WHEN 4 THEN 14 WHEN 5 THEN 18 WHEN 6 THEN 25 ELSE 40 END) + 5) / 10)
             * (100 + ${big(boostPercent)}::bigint)) / 100,
          ${big(cap)}::bigint))::int,
      "lastDaily" = ${SQL_NOW},
      "updatedAt" = ${SQL_NOW}
    FROM cur
    WHERE a."id" = cur."id"
    RETURNING a."balance" AS "balance", a."dailyStreak" AS "dailyStreak", a."lastDaily" AS "lastDaily"`,

  claimWork: (guildId, userId, amount, cap, cooldownMinutes) => Prisma.sql`
    UPDATE "EconomyAccount"
    SET "balance" = GREATEST("balance"::bigint, LEAST("balance"::bigint + ${big(amount)}::bigint, ${big(cap)}::bigint))::int,
        "lastWork" = ${SQL_NOW}, "updatedAt" = ${SQL_NOW}
    WHERE "guildId" = ${guildId} AND "userId" = ${userId}
      AND ("lastWork" IS NULL OR "lastWork" <= ${SQL_NOW} - make_interval(mins => ${cooldownMinutes}::int))
    RETURNING "balance", "lastWork"`,

  claimPity: (guildId, userId, cap) => Prisma.sql`
    UPDATE "EconomyAccount"
    SET "balance" = GREATEST("balance"::bigint, LEAST("balance"::bigint + ${big(PITY_AMOUNT)}::bigint, ${big(cap)}::bigint))::int,
        "lastPity" = ${SQL_NOW}, "updatedAt" = ${SQL_NOW}
    WHERE "guildId" = ${guildId} AND "userId" = ${userId}
      AND "balance" < ${PITY_THRESHOLD}::int
      AND ("lastPity" IS NULL OR "lastPity" <= ${SQL_NOW} - interval '12 hours')
    RETURNING "balance"`,

  insertBoostUse: (boostId, userId, periodKey) => Prisma.sql`
    INSERT INTO "EconomyBoostUse" ("id","boostId","userId","periodKey","createdAt")
    VALUES (${randomUUID()}, ${boostId}, ${userId}, ${periodKey}, ${SQL_NOW})
    ON CONFLICT ("boostId","userId","periodKey") DO NOTHING
    RETURNING "id"`,

  lockSettingsRow: (guildId) => Prisma.sql`SELECT 1 FROM "GuildSettings" WHERE "guildId" = ${guildId} FOR UPDATE`,

  reserveStock: (guildId, itemId, quantity) => Prisma.sql`
    UPDATE "ShopItem"
    SET "stock" = CASE WHEN "stock" IS NULL THEN NULL ELSE "stock" - ${quantity}::int END, "updatedAt" = ${SQL_NOW}
    WHERE "id" = ${itemId} AND "guildId" = ${guildId} AND "enabled" = true AND ("stock" IS NULL OR "stock" >= ${quantity}::int)
    RETURNING *`,

  addInventoryCapped: (guildId, userId, itemId, quantity, max) => Prisma.sql`
    INSERT INTO "InventoryItem" ("id","guildId","userId","itemId","quantity","createdAt","updatedAt")
    VALUES (${randomUUID()}, ${guildId}, ${userId}, ${itemId}, ${quantity}::int, ${SQL_NOW}, ${SQL_NOW})
    ON CONFLICT ("itemId","userId") DO UPDATE
    SET "quantity" = "InventoryItem"."quantity" + EXCLUDED."quantity", "updatedAt" = ${SQL_NOW}
    WHERE "InventoryItem"."quantity" + EXCLUDED."quantity" <= ${max}::int
    RETURNING "quantity"`,

  addInventory: (guildId, userId, itemId, quantity) => Prisma.sql`
    INSERT INTO "InventoryItem" ("id","guildId","userId","itemId","quantity","createdAt","updatedAt")
    VALUES (${randomUUID()}, ${guildId}, ${userId}, ${itemId}, ${quantity}::int, ${SQL_NOW}, ${SQL_NOW})
    ON CONFLICT ("itemId","userId") DO UPDATE
    SET "quantity" = "InventoryItem"."quantity" + EXCLUDED."quantity", "updatedAt" = ${SQL_NOW}
    RETURNING "quantity"`,

  restoreStock: (guildId, itemId, quantity) => Prisma.sql`
    UPDATE "ShopItem" SET "stock" = "stock" + ${quantity}::int, "updatedAt" = ${SQL_NOW}
    WHERE "id" = ${itemId} AND "guildId" = ${guildId} AND "stock" IS NOT NULL`,

  reduceInventory: (guildId, userId, itemId, quantity) => Prisma.sql`
    UPDATE "InventoryItem"
    SET "quantity" = GREATEST("quantity" - ${quantity}::int, 0), "updatedAt" = ${SQL_NOW}
    WHERE "itemId" = ${itemId} AND "userId" = ${userId} AND "guildId" = ${guildId}`,

  deleteEmptyInventory: (guildId, userId, itemId) => Prisma.sql`
    DELETE FROM "InventoryItem"
    WHERE "itemId" = ${itemId} AND "userId" = ${userId} AND "guildId" = ${guildId} AND "quantity" <= 0`,

  consumeItem: (guildId, userId, itemId) => Prisma.sql`
    UPDATE "InventoryItem"
    SET "quantity" = "quantity" - 1, "updatedAt" = ${SQL_NOW}
    WHERE "itemId" = ${itemId} AND "userId" = ${userId} AND "guildId" = ${guildId} AND "quantity" >= 1
    RETURNING "quantity"`,

  takeItem: (guildId, userId, itemId, quantity) => Prisma.sql`
    WITH old AS (
      SELECT "id", "quantity" FROM "InventoryItem"
      WHERE "itemId" = ${itemId} AND "userId" = ${userId} AND "guildId" = ${guildId} FOR UPDATE
    )
    UPDATE "InventoryItem" i
    SET "quantity" = GREATEST(old."quantity" - ${quantity}::int, 0), "updatedAt" = ${SQL_NOW}
    FROM old WHERE i."id" = old."id"
    RETURNING i."quantity" AS "remaining", old."quantity" AS "before"`,
};

/** One entry per raw statement with sample parameters (01-foundation §1, 00-overview rule 22). */
export const RAW_SQL = Object.freeze([
  { name: 'economy.insertAccount', build: () => SQL.insertAccount('0', '0', 0, false) },
  { name: 'economy.credit', build: () => SQL.credit('0', '0', 1, COIN_MAX) },
  { name: 'economy.debit', build: () => SQL.debit('0', '0', 1) },
  { name: 'economy.lockAccounts', build: () => SQL.lockAccounts('0', '1', '2') },
  { name: 'economy.creditChecked', build: () => SQL.creditChecked('0', '0', 1, COIN_MAX) },
  { name: 'economy.removeCoins', build: () => SQL.removeCoins('0', '0', 1) },
  { name: 'economy.claimDaily', build: () => SQL.claimDaily('0', '0', 100, COIN_MAX, 50) },
  { name: 'economy.claimWork', build: () => SQL.claimWork('0', '0', 100, COIN_MAX, 60) },
  { name: 'economy.claimPity', build: () => SQL.claimPity('0', '0', COIN_MAX) },
  { name: 'economy.insertBoostUse', build: () => SQL.insertBoostUse('0', '0', 'once') },
  { name: 'economy.lockSettingsRow', build: () => SQL.lockSettingsRow('0') },
  { name: 'economy.reserveStock', build: () => SQL.reserveStock('0', '0', 1) },
  { name: 'economy.addInventoryCapped', build: () => SQL.addInventoryCapped('0', '0', '0', 1, 200) },
  { name: 'economy.addInventory', build: () => SQL.addInventory('0', '0', '0', 1) },
  { name: 'economy.restoreStock', build: () => SQL.restoreStock('0', '0', 1) },
  { name: 'economy.reduceInventory', build: () => SQL.reduceInventory('0', '0', '0', 1) },
  { name: 'economy.deleteEmptyInventory', build: () => SQL.deleteEmptyInventory('0', '0', '0') },
  { name: 'economy.consumeItem', build: () => SQL.consumeItem('0', '0', '0') },
  { name: 'economy.takeItem', build: () => SQL.takeItem('0', '0', '0', 1) },
]);

const isForeignKeyError = (err) => err?.meta?.code === '23503' || err?.code === 'P2003';
const accountKey = (guildId, userId) => ({ guildId_userId: { guildId, userId } });

// ── Accounts ──────────────────────────────────────────────────────────────────

/**
 * The balance a not-yet-existing account would start with: the legacy global balance (clamped) unless the guild turned the
 * import off, else the starting balance.
 */
async function initialBalance(userId, settings) {
  const cfg = economyConfig(settings);
  if (settings?.economyImportGlobal === false) return { balance: cfg.startingBalance, importedGlobal: false };
  const legacy = await prisma.userEconomy.findUnique({ where: { userId }, select: { balance: true } });
  if (!legacy) return { balance: cfg.startingBalance, importedGlobal: false };
  return { balance: clampInt(legacy.balance, 0, cfg.maxBalance), importedGlobal: true };
}

/**
 * THE lazy global copy: returns the member's account on this server, creating it on first touch. A new account starts with
 * the member's legacy global UserEconomy balance (clamped to 0..economyMaxBalance), or economyStartingBalance when there is
 * none or the import was turned off. Only the balance is copied. Idempotent and race-safe: the insert is
 * ON CONFLICT DO NOTHING and the row is read back, so two concurrent first touches end up with the same single row.
 * @param {string} guildId
 * @param {string} userId
 * @param {object|null} settings GuildSettings row
 * @returns {Promise<object>} the EconomyAccount row
 * @throws on DB failure
 */
export async function getAccount(guildId, userId, settings) {
  const where = accountKey(guildId, userId);
  const existing = await prisma.economyAccount.findUnique({ where });
  if (existing) return existing;
  await ensureGuildSettings(guildId);
  const { balance, importedGlobal } = await initialBalance(userId, settings);
  await prisma.$executeRaw(SQL.insertAccount(guildId, userId, balance, importedGlobal));
  const row = await prisma.economyAccount.findUnique({ where });
  if (!row) throw new Error(`Economy account ${guildId}/${userId} is missing right after creation`);
  return row;
}

/**
 * Read-only preview for viewing ANOTHER member: the stored account (`exists: true`) or the balance getAccount would create.
 * Never creates rows.
 * @returns {Promise<{ balance: number, dailyStreak: number, lastDaily: Date|null, lastWork: Date|null, lastPity: Date|null, exists: boolean, importedGlobal: boolean }>}
 * @throws on DB failure
 */
export async function peekAccount(guildId, userId, settings) {
  const row = await prisma.economyAccount.findUnique({ where: accountKey(guildId, userId) });
  if (row) {
    return {
      balance: row.balance, dailyStreak: row.dailyStreak, lastDaily: row.lastDaily, lastWork: row.lastWork,
      lastPity: row.lastPity, exists: true, importedGlobal: row.importedGlobal,
    };
  }
  const { balance, importedGlobal } = await initialBalance(userId, settings);
  return { balance, dailyStreak: 0, lastDaily: null, lastWork: null, lastPity: null, exists: false, importedGlobal };
}

/** @returns {Promise<number>} 1-based rank by balance (ties share the better rank). Throws on DB failure. */
export async function getRank(guildId, balance) {
  return (await prisma.economyAccount.count({ where: { guildId, balance: { gt: balance } } })) + 1;
}

/**
 * Adds coins (clamped to the server maximum) in one statement.
 * @param {number} amount integer >= 0
 * @returns {Promise<{ balance: number, capped: boolean }>}
 * @throws EconomyError('INVALID') for a bad amount; DB failures
 */
export async function credit(guildId, userId, amount, settings) {
  if (!Number.isSafeInteger(amount) || amount < 0) throw new EconomyError('INVALID');
  const cap = economyConfig(settings).maxBalance;
  let rows = await prisma.$queryRaw(SQL.credit(guildId, userId, amount, cap));
  if (rows.length === 0) {
    await getAccount(guildId, userId, settings);
    rows = await prisma.$queryRaw(SQL.credit(guildId, userId, amount, cap));
  }
  if (rows.length === 0) throw new Error(`Economy credit found no account for ${guildId}/${userId}`);
  const balance = rows[0].balance;
  return { balance, capped: balance >= cap };
}

/**
 * Removes coins only when the balance covers them (never below 0). The account is created first when needed.
 * @param {number} amount integer >= 1
 * @returns {Promise<{ ok: true, balance: number } | { ok: false, balance: number }>}
 * @throws EconomyError('INVALID') for a bad amount; DB failures
 */
export async function debit(guildId, userId, amount, settings) {
  if (!Number.isSafeInteger(amount) || amount < 1) throw new EconomyError('INVALID');
  let rows = await prisma.$queryRaw(SQL.debit(guildId, userId, amount));
  if (rows.length) return { ok: true, balance: rows[0].balance };
  const acc = await getAccount(guildId, userId, settings);
  if (acc.balance >= amount) rows = await prisma.$queryRaw(SQL.debit(guildId, userId, amount)); // the account was just created
  if (rows.length) return { ok: true, balance: rows[0].balance };
  const now = await prisma.economyAccount.findUnique({ where: accountKey(guildId, userId), select: { balance: true } });
  return { ok: false, balance: now?.balance ?? acc.balance };
}

/**
 * Moves coins between two members in one transaction (both rows locked in userId order). A recipient that would exceed the
 * server maximum rolls the debit back: coins are never destroyed by a transfer.
 * @param {number} amount integer 1..COIN_MAX
 * @returns {Promise<{ ok: true, fromBalance: number, toBalance: number } | { ok: false, code: 'INSUFFICIENT'|'RECIPIENT_CAP', balance: number }>}
 * @throws EconomyError('INVALID') for the same member or a bad amount; DB failures
 */
export async function transfer(guildId, fromId, toId, amount, settings) {
  if (fromId === toId || !Number.isSafeInteger(amount) || amount < 1 || amount > COIN_MAX) throw new EconomyError('INVALID');
  const cap = economyConfig(settings).maxBalance;
  await getAccount(guildId, fromId, settings);
  await getAccount(guildId, toId, settings);
  try {
    return await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(SQL.lockAccounts(guildId, fromId, toId));
      const out = await tx.$queryRaw(SQL.debit(guildId, fromId, amount));
      if (out.length === 0) throw new EconomyError('INSUFFICIENT');
      const into = await tx.$queryRaw(SQL.creditChecked(guildId, toId, amount, cap));
      if (into.length === 0) throw new EconomyError('RECIPIENT_CAP');
      return { ok: true, fromBalance: out[0].balance, toBalance: into[0].balance };
    }, { timeout: 10_000, maxWait: 10_000 });
  } catch (err) {
    if (!(err instanceof EconomyError)) throw err;
    const row = await prisma.economyAccount.findUnique({ where: accountKey(guildId, fromId), select: { balance: true } });
    return { ok: false, code: err.code, balance: row?.balance ?? 0 };
  }
}

/**
 * Sets a balance (admin).
 * @param {number} amount integer 0..maxBalance
 * @throws EconomyError('INVALID', { max }) for an amount outside the range
 */
export async function setBalance(guildId, userId, amount, settings) {
  const cap = economyConfig(settings).maxBalance;
  if (!Number.isSafeInteger(amount) || amount < 0 || amount > cap) throw new EconomyError('INVALID', { max: cap });
  await getAccount(guildId, userId, settings);
  const row = await prisma.economyAccount.update({ where: accountKey(guildId, userId), data: { balance: amount } });
  return { balance: row.balance };
}

/**
 * Removes up to `amount` coins (admin), never below 0.
 * @returns {Promise<{ balance: number, removed: number }>}
 */
export async function removeCoins(guildId, userId, amount, settings) {
  if (!Number.isSafeInteger(amount) || amount < 1) throw new EconomyError('INVALID');
  await getAccount(guildId, userId, settings);
  const rows = await prisma.$queryRaw(SQL.removeCoins(guildId, userId, amount));
  if (rows.length === 0) throw new Error(`Economy removeCoins found no account for ${guildId}/${userId}`);
  return { balance: rows[0].balance, removed: rows[0].before - rows[0].balance };
}

/** Lowers every balance above `cap` to `cap` (after the maximum balance was reduced). @returns {Promise<number>} accounts changed */
export async function clampBalancesToCap(guildId, cap) {
  const { count } = await prisma.economyAccount.updateMany({ where: { guildId, balance: { gt: cap } }, data: { balance: cap } });
  return count;
}

/** @returns {Promise<Array<{ userId: string, balance: number }>>} richest first (balance > 0) */
export async function topAccounts(guildId, take = RICHEST_LIMIT) {
  return prisma.economyAccount.findMany({
    where: { guildId, balance: { gt: 0 } },
    orderBy: [{ balance: 'desc' }, { userId: 'asc' }],
    take,
    select: { userId: true, balance: true },
  });
}

// ── Casino lock and escrow ────────────────────────────────────────────────────
// This is the contract of economy-casino (pkg-economy.md §4.4). One casino game per member per server at a time; the lock
// lives in memory (a guild's interactions land on its own shard) and is lost on restart, which is harmless because the bet
// is already debited.

/** @type {Set<string>} */
const casinoLocks = new Set();
const lockKey = (guildId, userId) => `${guildId}:${userId}`;

/** Synchronous. @returns {boolean} false when the member already has a game running */
export function tryLockCasino(guildId, userId) {
  const key = lockKey(guildId, userId);
  if (casinoLocks.has(key)) return false;
  casinoLocks.add(key);
  return true;
}

/** Synchronous and idempotent. */
export function unlockCasino(guildId, userId) {
  casinoLocks.delete(lockKey(guildId, userId));
}

/** @returns {boolean} */
export function isCasinoLocked(guildId, userId) {
  return casinoLocks.has(lockKey(guildId, userId));
}

/**
 * Escrows a bet: debits it atomically BEFORE any card is dealt or wheel spun (R007, R136). Creates the account first when
 * needed (lazy global copy).
 * @param {number} bet integer 1..COIN_MAX
 * @returns {Promise<{ ok: true, balance: number, before: number }
 *   | { ok: false, code: 'INVALID' }
 *   | { ok: false, code: 'MAX_BET', maxBet: number }
 *   | { ok: false, code: 'INSUFFICIENT', balance: number }>} `balance` is after the debit, `before` is balance + bet
 * @throws on DB failure (nothing was debited)
 */
export async function reserveBet(guildId, userId, bet, settings) {
  if (!Number.isSafeInteger(bet) || bet < 1 || bet > COIN_MAX) return { ok: false, code: 'INVALID' };
  const cfg = economyConfig(settings);
  if (cfg.maxBet > 0 && bet > cfg.maxBet) return { ok: false, code: 'MAX_BET', maxBet: cfg.maxBet };
  const r = await debit(guildId, userId, bet, settings);
  if (!r.ok) return { ok: false, code: 'INSUFFICIENT', balance: r.balance };
  return { ok: true, balance: r.balance, before: r.balance + bet };
}

/**
 * Settles an escrowed game: credits the GROSS `payout` (stake included; 0 for a loss, the stake for a push). The net winnings
 * are limited to economyMaxPayout first (clampPayout) and the balance to the server maximum. A bet settles AT MOST ONCE: the
 * credit is retried (once) only when the failure proves the statement never reached the database (failedBeforeSend); any
 * other failure may mean the credit committed and only the response was lost, so it is logged and thrown, never repeated.
 * @param {number} payout integer >= 0
 * @param {{ stake: number }} opts `stake` is REQUIRED: the total amount escrowed for this game (2x the bet after a double-down)
 * @returns {Promise<{ balance: number, capped: boolean, payoutCapped: boolean }>} `capped`: the server maximum limited the
 *   balance; `payoutCapped`: the max payout limited the winnings
 * @throws EconomyError('INVALID') for a missing/non-integer stake or a bad payout; DB failures (see above)
 */
export async function settleBet(guildId, userId, payout, settings, { stake } = {}) {
  if (!Number.isSafeInteger(stake) || stake < 0 || !Number.isSafeInteger(payout) || payout < 0) throw new EconomyError('INVALID');
  const { payout: final, capped: payoutCapped } = clampPayout(payout, stake, economyConfig(settings).maxPayout);
  if (final === 0) {
    const row = await prisma.economyAccount.findUnique({ where: accountKey(guildId, userId), select: { balance: true } });
    return { balance: row?.balance ?? 0, capped: false, payoutCapped };
  }
  let result;
  try {
    result = await credit(guildId, userId, final, settings);
  } catch (first) {
    if (!failedBeforeSend(first)) {
      logger.error('Economy: settlement outcome unknown — check the balance, then pay manually if the payout is missing', { guildId, userId, payout: final }, first);
      throw first;
    }
    await sleep(250);
    try {
      result = await credit(guildId, userId, final, settings);
    } catch (err) {
      logger.error('Economy: settlement failed — outcome may be unknown, check the balance, then pay manually if the payout is missing', { guildId, userId, payout: final }, err);
      throw err;
    }
  }
  return { balance: result.balance, capped: result.capped, payoutCapped };
}

/**
 * True only for database errors that PROVE a statement never reached the server, so repeating it cannot apply it twice: the
 * client could not initialise or open a connection (PrismaClientInitializationError, P1001) or no pooled connection was free
 * (P2024). A timeout, a dropped connection (P1008, P1017), an unknown or plain error may have happened after the commit.
 * @param {unknown} err
 * @returns {boolean}
 */
export function failedBeforeSend(err) {
  if (err instanceof Prisma.PrismaClientInitializationError) return true;
  return err instanceof Prisma.PrismaClientKnownRequestError && (err.code === 'P1001' || err.code === 'P2024');
}

// ── Claims (atomic; R137, R138) ───────────────────────────────────────────────

class ClaimRollback extends Error {}

async function loadEnabledBoosts(guildId) {
  return prisma.economyBoost.findMany({ where: { guildId, enabled: true } });
}

/**
 * Runs `earn(tx, boost)` with the first qualifying boost the member has not used in this period, inside one transaction.
 * `earn` returns the statement's rows; zero rows (cooldown) roll the boost use back.
 * @returns {Promise<{ rows: any[], boost: object|null }>}
 */
async function withBoost(boosts, userId, tz, earn) {
  if (boosts.length === 0) return { rows: await earn(prisma, null), boost: null };
  try {
    return await prisma.$transaction(async (tx) => {
      let applied = null;
      for (const boost of boosts) {
        const key = boostPeriodKey(boost.period, tz, new Date());
        const used = await tx.$queryRaw(SQL.insertBoostUse(boost.id, userId, key));
        if (used.length) { applied = boost; break; }
      }
      const rows = await earn(tx, applied);
      if (rows.length === 0) throw new ClaimRollback('cooldown');
      return { rows, boost: applied };
    }, { timeout: 10_000, maxWait: 10_000 });
  } catch (err) {
    if (err instanceof ClaimRollback) return { rows: [], boost: null };
    throw err;
  }
}

const boostInfo = (boost) => (boost ? { name: boost.name, percent: boost.percent } : null);

/**
 * /daily: one atomic claim with the streak bonus (x1, 1.1, 1.2, 1.4, 1.8, 2.5, then 4 from day 7; the streak resets after
 * 48 h) and at most one coin boost.
 * @param {{ memberRoleIds?: Iterable<string> }} [opts] the member's role ids (decides which boosts qualify)
 * @returns {Promise<{ claimed: true, amount: number, base: number, streak: number, balance: number, capped: boolean, nextAt: Date, boost: { name: string, percent: number }|null }
 *   | { claimed: false, nextAt: Date, streak: number }>}
 * @throws on DB failure
 */
export async function claimDaily(guildId, userId, settings, { memberRoleIds = [] } = {}) {
  const cfg = economyConfig(settings);
  const cap = cfg.maxBalance;
  await getAccount(guildId, userId, settings);
  const boosts = orderBoosts(await loadEnabledBoosts(guildId), memberRoleIds);
  const base = randomInt(cfg.dailyMin, cfg.dailyMax + 1);
  const { rows, boost } = await withBoost(boosts, userId, settings?.timezone ?? 'UTC',
    (client, applied) => client.$queryRaw(SQL.claimDaily(guildId, userId, base, cap, applied?.percent ?? 0)));
  if (rows.length) {
    const row = rows[0];
    const raw = dailyAmount(base, row.dailyStreak);
    return {
      claimed: true,
      amount: boost ? boostedAmount(raw, boost.percent) : raw,
      base,
      streak: row.dailyStreak,
      balance: row.balance,
      capped: row.balance >= cap,
      nextAt: new Date(row.lastDaily.getTime() + DAILY_COOLDOWN_MS),
      boost: boostInfo(boost),
    };
  }
  const cur = await prisma.economyAccount.findUnique({ where: accountKey(guildId, userId), select: { lastDaily: true, dailyStreak: true } });
  const last = cur?.lastDaily ?? null;
  return {
    claimed: false,
    nextAt: new Date((last ? last.getTime() : Date.now()) + DAILY_COOLDOWN_MS),
    streak: activeStreak(last, cur?.dailyStreak ?? 0),
  };
}

/**
 * /work: one atomic claim per `workCooldownMinutes`, with at most one coin boost.
 * @param {{ memberRoleIds?: Iterable<string> }} [opts]
 * @returns {Promise<{ claimed: true, amount: number, balance: number, capped: boolean, nextAt: Date, line: string, boost: { name: string, percent: number }|null }
 *   | { claimed: false, nextAt: Date }>}
 * @throws on DB failure
 */
export async function claimWork(guildId, userId, settings, { memberRoleIds = [] } = {}) {
  const cfg = economyConfig(settings);
  const cap = cfg.maxBalance;
  await getAccount(guildId, userId, settings);
  const boosts = orderBoosts(await loadEnabledBoosts(guildId), memberRoleIds);
  const base = randomInt(cfg.workMin, cfg.workMax + 1);
  const line = WORK_LINES[randomInt(WORK_LINES.length)];
  let amount = base;
  const { rows, boost } = await withBoost(boosts, userId, settings?.timezone ?? 'UTC', (client, applied) => {
    amount = applied ? boostedAmount(base, applied.percent) : base;
    return client.$queryRaw(SQL.claimWork(guildId, userId, amount, cap, cfg.workCooldownMinutes));
  });
  if (rows.length) {
    const row = rows[0];
    return {
      claimed: true,
      amount,
      balance: row.balance,
      capped: row.balance >= cap,
      nextAt: new Date(row.lastWork.getTime() + cfg.workCooldownMinutes * 60_000),
      line,
      boost: boostInfo(boost),
    };
  }
  const cur = await prisma.economyAccount.findUnique({ where: accountKey(guildId, userId), select: { lastWork: true } });
  const last = cur?.lastWork ?? null;
  return { claimed: false, nextAt: new Date((last ? last.getTime() : Date.now()) + cfg.workCooldownMinutes * 60_000) };
}

/**
 * /pity: PITY_AMOUNT coins when the balance is below PITY_THRESHOLD, once per 12 h. Never boosted.
 * @returns {Promise<{ claimed: true, amount: number, balance: number } | { claimed: false, tooRich: true, balance: number } | { claimed: false, nextAt: Date }>}
 * @throws on DB failure
 */
export async function claimPity(guildId, userId, settings) {
  const cap = economyConfig(settings).maxBalance;
  await getAccount(guildId, userId, settings);
  const rows = await prisma.$queryRaw(SQL.claimPity(guildId, userId, cap));
  if (rows.length) return { claimed: true, amount: PITY_AMOUNT, balance: rows[0].balance };
  const cur = await prisma.economyAccount.findUnique({ where: accountKey(guildId, userId), select: { balance: true, lastPity: true } });
  if (!cur || cur.balance >= PITY_THRESHOLD) return { claimed: false, tooRich: true, balance: cur?.balance ?? 0 };
  const last = cur.lastPity?.getTime() ?? Date.now();
  return { claimed: false, nextAt: new Date(last + PITY_COOLDOWN_MS) };
}

// ── Shop and inventory ────────────────────────────────────────────────────────

/** @returns {Promise<object|null>} a shop item by id or (case-insensitive) name; always scoped to the guild */
export async function resolveItem(guildId, value, { enabledOnly = false } = {}) {
  const text = String(value ?? '').trim();
  if (!text) return null;
  return prisma.shopItem.findFirst({
    where: {
      guildId,
      ...(enabledOnly ? { enabled: true } : {}),
      OR: [{ id: text }, { name: { equals: text, mode: 'insensitive' } }],
    },
  });
}

/** @returns {Promise<Array<{ id: string, name: string, price: number, enabled: boolean }>>} autocomplete lookup (<= take rows) */
export async function searchItems(guildId, query, { enabledOnly = false, take = 25 } = {}) {
  return prisma.shopItem.findMany({
    where: {
      guildId,
      ...(enabledOnly ? { enabled: true } : {}),
      ...(query ? { name: { contains: query, mode: 'insensitive' } } : {}),
    },
    orderBy: [{ price: 'asc' }, { name: 'asc' }],
    take,
    select: { id: true, name: true, price: true, enabled: true },
  });
}

/** @param {{ enabledOnly?: boolean, sort?: 'price'|'name' }} [opts] @returns {Promise<object[]>} every matching item (<= 300) */
export async function listItems(guildId, { enabledOnly = false, sort = 'price' } = {}) {
  return prisma.shopItem.findMany({
    where: { guildId, ...(enabledOnly ? { enabled: true } : {}) },
    orderBy: sort === 'name' ? { name: 'asc' } : [{ price: 'asc' }, { name: 'asc' }],
  });
}

/** @returns {Promise<number>} */
export async function countItems(guildId) {
  return prisma.shopItem.count({ where: { guildId } });
}

/** @returns {Promise<object|null>} uniqueness check for create/rename */
export async function findItemByNameInsensitive(guildId, name, { excludeId } = {}) {
  return prisma.shopItem.findFirst({
    where: { guildId, name: { equals: name, mode: 'insensitive' }, ...(excludeId ? { id: { not: excludeId } } : {}) },
  });
}

/** @returns {Promise<number>} copies of an item the member owns here */
export async function getOwnedQuantity(guildId, userId, itemId) {
  const row = await prisma.inventoryItem.findUnique({ where: { itemId_userId: { itemId, userId } } });
  return row && row.guildId === guildId ? row.quantity : 0;
}

/** @returns {Promise<Array<object>>} inventory rows with `item: { id, name, type, price, enabled }`, by item name */
export async function getInventory(guildId, userId) {
  return prisma.inventoryItem.findMany({
    where: { guildId, userId, quantity: { gt: 0 } },
    include: { item: { select: { id: true, name: true, type: true, price: true, enabled: true } } },
    orderBy: { item: { name: 'asc' } },
  });
}

/** @returns {Promise<Array<object>>} autocomplete lookup of the member's own items (<= 25 rows) */
export async function searchInventory(guildId, userId, query, { usableOnly = true } = {}) {
  return prisma.inventoryItem.findMany({
    where: {
      guildId,
      userId,
      quantity: { gt: 0 },
      item: {
        ...(usableOnly ? { type: { in: ['USABLE', 'ROLE'] } } : {}),
        ...(query ? { name: { contains: query, mode: 'insensitive' } } : {}),
      },
    },
    include: { item: { select: { id: true, name: true } } },
    orderBy: { item: { name: 'asc' } },
    take: 25,
  });
}

/**
 * Buys an item in ONE transaction: reserve stock (also locks the item row), check the ownership limit, debit the price,
 * add to the inventory. Any EconomyError rolls everything back (stock, coins, inventory).
 * @returns {Promise<{ item: object, quantity: number, total: number, balance: number, owned: number|null, toInventory: boolean }>}
 *   `item` is the locked ShopItem row (current price, flags and post-purchase stock); `owned` is null for items that skip the inventory
 * @throws EconomyError OUT_OF_STOCK (also: disabled or deleted item), MAX_OWNED { max }, INSUFFICIENT { total }, ITEM_GONE; DB failures
 */
export async function purchaseItem({ guildId, userId, itemId, quantity, settings }) {
  if (!Number.isSafeInteger(quantity) || quantity < 1 || quantity > LIMITS.ITEM_COPIES_PER_USER) throw new EconomyError('INVALID');
  await ensureGuildSettings(guildId);
  await getAccount(guildId, userId, settings);
  try {
    return await prisma.$transaction(async (tx) => {
      const reserved = await tx.$queryRaw(SQL.reserveStock(guildId, itemId, quantity));
      if (reserved.length === 0) throw new EconomyError('OUT_OF_STOCK');
      const item = reserved[0];
      const toInventory = goesToInventory(item);
      const max = effectiveMaxOwned(item);
      if ((!item.canBuyMultiple && quantity > 1) || (toInventory && quantity > max)) throw new EconomyError('MAX_OWNED', { max });
      const total = item.price * quantity;
      const paid = await tx.$queryRaw(SQL.debit(guildId, userId, total));
      if (paid.length === 0) throw new EconomyError('INSUFFICIENT', { total });
      let owned = null;
      if (toInventory) {
        const inv = await tx.$queryRaw(SQL.addInventoryCapped(guildId, userId, item.id, quantity, max));
        if (inv.length === 0) throw new EconomyError('MAX_OWNED', { max });
        owned = inv[0].quantity;
      }
      return { item, quantity, total, balance: paid[0].balance, owned, toInventory };
    }, { timeout: 10_000, maxWait: 10_000 });
  } catch (err) {
    if (isForeignKeyError(err)) throw new EconomyError('ITEM_GONE');
    throw err;
  }
}

/**
 * Compensation when the post-commit role grant fails: restores stock, refunds the FULL price and removes the copies. The refund
 * is clamped to COIN_MAX only, never to the server's maximum balance: the buyer may have reached it since the purchase, and a
 * refund must not lose coins.
 * @throws logs and re-throws on failure
 */
export async function refundPurchase({ guildId, userId, item, quantity, total, toInventory, settings }) {
  try {
    await getAccount(guildId, userId, settings);
    await prisma.$transaction(async (tx) => {
      await tx.$executeRaw(SQL.restoreStock(guildId, item.id, quantity));
      await tx.$queryRaw(SQL.credit(guildId, userId, total, COIN_MAX));
      if (toInventory) {
        await tx.$executeRaw(SQL.reduceInventory(guildId, userId, item.id, quantity));
        await tx.$executeRaw(SQL.deleteEmptyInventory(guildId, userId, item.id));
      }
    }, { timeout: 10_000, maxWait: 10_000 });
  } catch (err) {
    logger.error('Economy: purchase refund failed — refund manually', { guildId, userId, itemId: item?.id, quantity, total }, err);
    throw err;
  }
}

/**
 * Removes one copy from the inventory (for /use).
 * @returns {Promise<{ remaining: number }>}
 * @throws EconomyError('NOT_OWNED') when the member has none (a concurrent use won the race)
 */
export async function consumeItem({ guildId, userId, itemId }) {
  const rows = await prisma.$queryRaw(SQL.consumeItem(guildId, userId, itemId));
  if (rows.length === 0) throw new EconomyError('NOT_OWNED');
  const remaining = rows[0].quantity;
  if (remaining === 0) await prisma.$executeRaw(SQL.deleteEmptyInventory(guildId, userId, itemId));
  return { remaining };
}

/** Undo of consumeItem (the role grant failed). Logs and swallows errors. */
export async function restoreItem({ guildId, userId, itemId, quantity = 1 }) {
  try {
    await prisma.$queryRaw(SQL.addInventory(guildId, userId, itemId, quantity)); // no cap check: it only gives back what was taken
  } catch (err) {
    logger.error('Economy: could not restore a consumed item', { guildId, userId, itemId, quantity }, err);
  }
}

/**
 * Admin spawn: adds copies, ignoring the item's own limits but never exceeding LIMITS.ITEM_COPIES_PER_USER.
 * @returns {Promise<{ owned: number }>}
 * @throws EconomyError MAX_OWNED { max, owned }, ITEM_GONE; DB failures
 */
export async function giveItem({ guildId, userId, itemId, quantity }) {
  if (!Number.isSafeInteger(quantity) || quantity < 1 || quantity > LIMITS.ITEM_COPIES_PER_USER) throw new EconomyError('INVALID');
  await ensureGuildSettings(guildId);
  // An item of another guild never gets into this guild's inventories (the foreign key alone would allow it).
  if (!(await prisma.shopItem.findFirst({ where: { id: itemId, guildId }, select: { id: true } }))) throw new EconomyError('ITEM_GONE');
  let rows;
  try {
    rows = await prisma.$queryRaw(SQL.addInventoryCapped(guildId, userId, itemId, quantity, LIMITS.ITEM_COPIES_PER_USER));
  } catch (err) {
    if (isForeignKeyError(err)) throw new EconomyError('ITEM_GONE');
    throw err;
  }
  if (rows.length === 0) {
    throw new EconomyError('MAX_OWNED', { max: LIMITS.ITEM_COPIES_PER_USER, owned: await getOwnedQuantity(guildId, userId, itemId) });
  }
  return { owned: rows[0].quantity };
}

/**
 * Admin removal of up to `quantity` copies (the row is deleted at 0).
 * @returns {Promise<{ removed: number, remaining: number }>}
 * @throws EconomyError('NOT_OWNED')
 */
export async function takeItem({ guildId, userId, itemId, quantity }) {
  if (!Number.isSafeInteger(quantity) || quantity < 1) throw new EconomyError('INVALID');
  const rows = await prisma.$queryRaw(SQL.takeItem(guildId, userId, itemId, quantity));
  if (rows.length === 0 || rows[0].before <= 0) throw new EconomyError('NOT_OWNED');
  const { remaining, before } = rows[0];
  if (remaining === 0) await prisma.$executeRaw(SQL.deleteEmptyInventory(guildId, userId, itemId));
  return { removed: before - remaining, remaining };
}

/**
 * Creates a shop item. The per-guild cap and the case-insensitive name are checked under a lock on the settings row, so two
 * concurrent creates cannot exceed the cap.
 * @throws EconomyError('INVALID', { reason: 'limit' | 'name-taken' })
 */
export async function createItem(guildId, data) {
  await ensureGuildSettings(guildId);
  try {
    return await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(SQL.lockSettingsRow(guildId));
      if ((await tx.shopItem.count({ where: { guildId } })) >= LIMITS.SHOP_ITEMS) throw new EconomyError('INVALID', { reason: 'limit' });
      const clash = await tx.shopItem.findFirst({ where: { guildId, name: { equals: data.name, mode: 'insensitive' } }, select: { id: true } });
      if (clash) throw new EconomyError('INVALID', { reason: 'name-taken' });
      return tx.shopItem.create({ data: { guildId, ...data } });
    }, { timeout: 10_000, maxWait: 10_000 });
  } catch (err) {
    if (err?.code === 'P2002') throw new EconomyError('INVALID', { reason: 'name-taken' });
    throw err;
  }
}

/**
 * Updates a shop item of this guild.
 * @throws EconomyError ITEM_GONE (missing or another guild's item), INVALID { reason: 'name-taken' }
 */
export async function updateItem(guildId, itemId, data) {
  try {
    return await prisma.$transaction(async (tx) => {
      // A rename is checked case-insensitively under the same lock as createItem.
      if (data.name !== undefined) await tx.$queryRaw(SQL.lockSettingsRow(guildId));
      const current = await tx.shopItem.findFirst({ where: { id: itemId, guildId }, select: { id: true } });
      if (!current) throw new EconomyError('ITEM_GONE');
      if (data.name !== undefined) {
        const clash = await tx.shopItem.findFirst({
          where: { guildId, id: { not: itemId }, name: { equals: data.name, mode: 'insensitive' } }, select: { id: true },
        });
        if (clash) throw new EconomyError('INVALID', { reason: 'name-taken' });
      }
      return tx.shopItem.update({ where: { id: itemId }, data });
    }, { timeout: 10_000, maxWait: 10_000 });
  } catch (err) {
    if (err?.code === 'P2002') throw new EconomyError('INVALID', { reason: 'name-taken' });
    if (err?.code === 'P2025') throw new EconomyError('ITEM_GONE');
    throw err;
  }
}

/** Deletes an item and (by cascade) every copy members own. @returns {Promise<{ copies: number }>} copies that were removed */
export async function deleteItem(guildId, itemId) {
  const agg = await prisma.inventoryItem.aggregate({ where: { itemId, guildId }, _sum: { quantity: true } });
  await prisma.shopItem.deleteMany({ where: { id: itemId, guildId } });
  return { copies: agg._sum.quantity ?? 0 };
}

// ── Boosts (managed by /economy-boost) ────────────────────────────────────────

/** @returns {Promise<Array<object & { _count: { uses: number } }>>} every boost of the guild, oldest first */
export async function listBoosts(guildId) {
  return prisma.economyBoost.findMany({ where: { guildId }, orderBy: [{ createdAt: 'asc' }, { name: 'asc' }], include: { _count: { select: { uses: true } } } });
}

/** @returns {Promise<Array<{ id: string, name: string, percent: number, enabled: boolean }>>} autocomplete lookup (<= 25 rows) */
export async function searchBoosts(guildId, query) {
  return prisma.economyBoost.findMany({
    where: { guildId, ...(query ? { name: { contains: query, mode: 'insensitive' } } : {}) },
    orderBy: { name: 'asc' },
    take: 25,
    select: { id: true, name: true, percent: true, enabled: true },
  });
}

/** @returns {Promise<object|null>} a boost by id or case-insensitive name */
export async function resolveBoost(guildId, value) {
  const text = String(value ?? '').trim();
  if (!text) return null;
  return prisma.economyBoost.findFirst({ where: { guildId, OR: [{ id: text }, { name: { equals: text, mode: 'insensitive' } }] } });
}

/**
 * Creates a boost (at most LIMITS.ECONOMY_BOOSTS per guild; names unique case-insensitively), under a lock on the settings row.
 * @param {{ name: string, percent: number, period: 'ONE_TIME'|'DAILY'|'WEEKLY'|'MONTHLY', roleId?: string|null, createdBy: string }} data
 * @throws EconomyError('INVALID', { reason: 'limit' | 'name-taken' | 'percent' })
 */
export async function createBoost(guildId, { name, percent, period, roleId = null, createdBy }) {
  if (!Number.isInteger(percent) || percent < 1 || percent > BOOST_PERCENT_MAX) throw new EconomyError('INVALID', { reason: 'percent' });
  if (!Object.hasOwn(BOOST_PERIOD_LABELS, period)) throw new EconomyError('INVALID', { reason: 'period' });
  await ensureGuildSettings(guildId);
  try {
    return await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(SQL.lockSettingsRow(guildId));
      if ((await tx.economyBoost.count({ where: { guildId } })) >= LIMITS.ECONOMY_BOOSTS) throw new EconomyError('INVALID', { reason: 'limit' });
      const clash = await tx.economyBoost.findFirst({ where: { guildId, name: { equals: name, mode: 'insensitive' } }, select: { id: true } });
      if (clash) throw new EconomyError('INVALID', { reason: 'name-taken' });
      return tx.economyBoost.create({ data: { guildId, name, percent, period, roleId: roleId ?? null, createdBy } });
    }, { timeout: 10_000, maxWait: 10_000 });
  } catch (err) {
    if (err?.code === 'P2002') throw new EconomyError('INVALID', { reason: 'name-taken' });
    throw err;
  }
}

/** @returns {Promise<boolean>} true when a boost was deleted (its uses cascade) */
export async function deleteBoost(guildId, boostId) {
  const { count } = await prisma.economyBoost.deleteMany({ where: { id: boostId, guildId } });
  return count > 0;
}

/** @returns {Promise<object|null>} the updated boost, or null when it no longer exists */
export async function setBoostEnabled(guildId, boostId, enabled) {
  const { count } = await prisma.economyBoost.updateMany({ where: { id: boostId, guildId }, data: { enabled } });
  return count > 0 ? prisma.economyBoost.findUnique({ where: { id: boostId } }) : null;
}

// ── Reset ─────────────────────────────────────────────────────────────────────

/**
 * Resets coins and/or inventories for everyone or one member. Coins go back to the starting balance (streaks and cooldowns
 * are cleared); rows are never deleted, because a deleted row would re-import the global balance. A reset of everyone's coins
 * also turns the global import off for members who never used the economy here. The shop catalogue is never touched.
 * @param {{ guildId: string, userId?: string|null, what: 'coins'|'items'|'all', settings: object|null }} args
 * @returns {Promise<{ accounts: number, inventories: number }>}
 * @throws on DB failure
 */
export async function resetEconomy({ guildId, userId = null, what, settings }) {
  const cfg = economyConfig(settings);
  let accounts = 0;
  let inventories = 0;
  if (what === 'coins' || what === 'all') {
    if (userId) await getAccount(guildId, userId, settings); // a single-member reset also stops that member's future import
    const res = await prisma.economyAccount.updateMany({
      where: { guildId, ...(userId ? { userId } : {}) },
      data: { balance: cfg.startingBalance, dailyStreak: 0, lastDaily: null, lastWork: null, lastPity: null },
    });
    accounts = res.count;
    if (!userId) await updateSettings(guildId, { economyImportGlobal: false });
  }
  if (what === 'items' || what === 'all') {
    const res = await prisma.inventoryItem.deleteMany({ where: { guildId, ...(userId ? { userId } : {}) } });
    inventories = res.count;
  }
  return { accounts, inventories };
}

// ── Rewards provider (registered by src/plugins/economy.js) ───────────────────

async function rewardChange(kind, { guild, userId, amount, reason = '' }) {
  try {
    const settings = await getSettingsSafe(guild.id);
    if (!settings) return { ok: false, reason: 'unavailable' };
    if (!isModuleEnabled(settings, 'economy')) return { ok: false, reason: 'module-disabled' };
    if (!Number.isSafeInteger(amount) || amount < 1 || amount > COIN_MAX) return { ok: false, reason: 'invalid-amount' };
    let result;
    if (kind === 'grant') {
      const { balance } = await credit(guild.id, userId, amount, settings);
      result = { ok: true, total: balance };
    } else {
      const r = await debit(guild.id, userId, amount, settings);
      result = r.ok ? { ok: true, total: r.balance } : { ok: false, reason: 'insufficient', total: r.balance };
    }
    logger.debug(`Economy reward ${kind} ${amount} → ${userId} in ${guild.id} (${reason})`);
    return result;
  } catch (err) {
    logger.error('Economy reward failed', err);
    return { ok: false, reason: 'error' };
  }
}

/**
 * `coins` rewards provider, grant. Coin boosts never apply. Never throws.
 * @returns {Promise<{ ok: boolean, total?: number, reason?: string }>}
 */
export function grantCoinsReward(args) {
  return rewardChange('grant', args);
}

/**
 * `coins` rewards provider, take: never drives a balance below 0 (`reason: 'insufficient'`, nothing changed). Never throws.
 * @returns {Promise<{ ok: boolean, total?: number, reason?: string }>}
 */
export function takeCoinsReward(args) {
  return rewardChange('take', args);
}

// ── UI helper ─────────────────────────────────────────────────────────────────

/**
 * Shows an error (or any embed) PRIVATELY whatever the interaction state: the "defer first, then private error" pattern
 * (R088). Pass `embed` for private warnings and notices (title/description are then ignored and may be null). Never throws.
 */
export async function replyFail(interaction, title, description, { embed = null } = {}) {
  try {
    const embeds = [embed ?? errorEmbed(title, description)];
    if (!interaction.deferred && !interaction.replied) {
      await interaction.reply({ embeds, ephemeral: true });
    } else if (interaction.ephemeral === false) {
      // Public deferral or reply: remove it and say it privately.
      await interaction.deleteReply().catch(() => {});
      await interaction.followUp({ embeds, ephemeral: true });
    } else if (interaction.ephemeral === true) {
      await interaction.editReply({ embeds, components: [] });
    } else {
      await interaction.followUp({ embeds, ephemeral: true }); // deferUpdate/update: never overwrite the clicked panel
    }
  } catch (err) {
    if (isInteractionGone(err)) logger.debug(`replyFail: interaction gone (${err?.code})`);
    else logger.warn('replyFail failed', err);
  }
}
