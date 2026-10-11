// Database access of the `balances` collection: the EconomyAccount rows of the guild, richest first, in pages. The writes
// are /economy-admin's and need no gateway, so they call economyService directly: credit (give), removeCoins (take),
// setBalance (set) and resetEconomy (reset). Each is one SQL statement clamped to the server's maximum balance, and a
// member's first touch creates their account with the legacy global balance unless that import is off (getAccount).
// Like /economy-admin, bots are refused (checked with Discord) and nothing requires the Economy module to be on.
import prisma from '../../database/client.js';
import { EconomyError, credit, economyConfig, formatCoins, removeCoins, resetEconomy, setBalance } from '../../services/economyService.js';
import { getSettings } from '../../services/settingsService.js';
import { RecordError } from './errors.js';
import { balanceItem } from './balances.js';
import { PAGE_SIZE, afterRankCursor, isUserId, loadProfiles, profileName, rankCursor, searchUserIds } from './levels.js';

const SELECT = { userId: true, balance: true, dailyStreak: true, lastDaily: true, lastWork: true, updatedAt: true };
const ORDER = [{ balance: 'desc' }, { userId: 'asc' }];

/** 1-based rank by balance; members with the same balance share it (getRank of economyService). */
const rankOf = (guildId, balance) => prisma.economyAccount.count({ where: { guildId, balance: { gt: balance } } }).then((n) => n + 1);

const currencyOf = (settings) => {
  const cfg = economyConfig(settings);
  return { name: cfg.currencyName, emoji: cfg.currencyEmoji };
};

/** Coins in a message, without markdown: "🪙 1,234 coins". */
const coins = (n, settings) => formatCoins(n, settings, { bold: false });

async function toItems(guildId, rows, { discord, logger, settings }) {
  if (!rows.length) return [];
  const balances = [...new Set(rows.map((r) => r.balance))];
  const [profiles, ranks] = await Promise.all([
    loadProfiles(discord, rows.map((r) => r.userId), logger),
    Promise.all(balances.map((b) => rankOf(guildId, b))).then((list) => new Map(balances.map((b, i) => [b, list[i]]))),
  ]);
  const currency = currencyOf(settings);
  return rows.map((row) => balanceItem(row, { rank: ranks.get(row.balance), profile: profiles.get(row.userId) ?? null, currency }));
}

async function readItem(guildId, userId, { discord, logger, settings }) {
  const row = await prisma.economyAccount.findUnique({ where: { guildId_userId: { guildId, userId } }, select: SELECT });
  return row ? (await toItems(guildId, [row], { discord, logger, settings }))[0] : null;
}

/** The settings, or null when they cannot be read (the list then shows the default currency). */
const settingsOrNull = (guildId) => getSettings(guildId).catch(() => null);

/**
 * /economy-admin refuses bots ("Bots can't hold coins or items."); the slash command knows from its user option, the
 * dashboard asks Discord.
 * @returns {Promise<object>} the user's profile
 * @throws {RecordError}
 */
async function checkHolder(ctx, userId, path) {
  if (typeof ctx?.discord?.getUsers !== 'function') {
    throw new RecordError('unavailable', 'The dashboard cannot check that user with Discord right now. Try again in a moment.');
  }
  let profile;
  try {
    profile = (await ctx.discord.getUsers([userId])).get(userId) ?? null;
  } catch {
    throw new RecordError('discord', 'Discord did not answer. Try again in a moment.');
  }
  if (!profile) throw new RecordError('invalid', 'There is no Discord user with that ID.', path ? { path } : {});
  if (profile.bot) throw new RecordError('invalid', "Bots can't hold coins or items.", path ? { path } : {});
  return profile;
}

function userIdOf(id) {
  if (!isUserId(id)) throw new RecordError('not_found', 'That member has no balance in this server.');
  return id;
}

const log = (ctx, guildId, what, userId) => {
  ctx?.logger?.info?.(`Dashboard economy ${what} for ${userId} in ${guildId} by ${ctx?.userId}`);
};

/** The item after a change; the account always exists after one of these writes. */
async function after(guildId, userId, ctx, settings) {
  const item = await readItem(guildId, userId, { discord: ctx?.discord, logger: ctx?.logger, settings });
  if (!item) throw new RecordError('not_found', 'That member has no balance in this server.');
  return item;
}

export const store = Object.freeze({
  /**
   * A page of balances, richest first. `query`: a user id or mention, or the start of a member's name.
   * @param {string} guildId
   * @param {{ query?: string, cursor?: string, discord?: object, logger?: object }} [opts]
   * @param {{ discord?: object, logger?: object }} [ctx]
   */
  async list(guildId, { query = '', cursor = '', ...rest } = {}, ctx = {}) {
    const discord = rest.discord ?? ctx.discord;
    const logger = rest.logger ?? ctx.logger;
    const settings = await settingsOrNull(guildId);
    const q = String(query ?? '').trim();
    if (q) {
      const ids = await searchUserIds(discord, guildId, q);
      if (!ids.length) return { items: [] };
      const rows = await prisma.economyAccount.findMany({ where: { guildId, userId: { in: ids } }, orderBy: ORDER, take: PAGE_SIZE, select: SELECT });
      return { items: await toItems(guildId, rows, { discord, logger, settings }) };
    }
    const rows = await prisma.economyAccount.findMany({
      where: { guildId, ...afterRankCursor('balance', cursor) }, orderBy: ORDER, take: PAGE_SIZE + 1, select: SELECT,
    });
    const page = rows.slice(0, PAGE_SIZE);
    const items = await toItems(guildId, page, { discord, logger, settings });
    const last = page[page.length - 1];
    return rows.length > PAGE_SIZE ? { items, nextCursor: rankCursor(last.balance, last.userId) } : { items };
  },

  async get(guildId, id, ctx = {}) {
    if (!isUserId(id)) return null;
    return readItem(guildId, id, { discord: ctx?.discord, logger: ctx?.logger, settings: await settingsOrNull(guildId) });
  },

  /** /economy-admin coins give for a member who may have no account yet. */
  async create(guildId, data, ctx) {
    await checkHolder(ctx, data.userId, 'userId');
    const settings = await getSettings(guildId);
    await credit(guildId, data.userId, data.amount, settings);
    log(ctx, guildId, `coins give ${data.amount}`, data.userId);
    return after(guildId, data.userId, ctx, settings);
  },

  async update() {
    throw new RecordError('invalid', 'Use Give coins, Remove coins, Set balance or Reset to change a member.');
  },

  async remove() {
    throw new RecordError('invalid', 'Use "Reset" to reset a member\'s coins or items.');
  },

  actions: Object.freeze({
    /** /economy-admin coins give: `{ amount }`. */
    async give(guildId, id, data, ctx) {
      const userId = userIdOf(id);
      const profile = await checkHolder(ctx, userId);
      const settings = await getSettings(guildId);
      const { balance, capped } = await credit(guildId, userId, data.amount, settings);
      log(ctx, guildId, `coins give ${data.amount}`, userId);
      return {
        item: await after(guildId, userId, ctx, settings),
        message: `Gave ${coins(data.amount, settings)} to ${profileName(profile, userId)}. New balance: ${coins(balance, settings)}`
          + `${capped ? ' (server maximum reached)' : ''}.`,
      };
    },

    /** /economy-admin coins remove: `{ amount }`, never below 0. */
    async take(guildId, id, data, ctx) {
      const userId = userIdOf(id);
      const profile = await checkHolder(ctx, userId);
      const settings = await getSettings(guildId);
      const { balance, removed } = await removeCoins(guildId, userId, data.amount, settings);
      log(ctx, guildId, `coins remove ${data.amount}`, userId);
      let message = `Removed ${coins(removed, settings)} from ${profileName(profile, userId)}. New balance: ${coins(balance, settings)}.`;
      if (removed < data.amount) message += ` (They only had ${coins(removed, settings)}.)`;
      return { item: await after(guildId, userId, ctx, settings), message };
    },

    /** /economy-admin coins set: `{ balance }`, at most the server's maximum balance. */
    async set(guildId, id, data, ctx) {
      const userId = userIdOf(id);
      const profile = await checkHolder(ctx, userId);
      const settings = await getSettings(guildId);
      const tooHigh = (max) => new RecordError('invalid', `The maximum balance on this server is ${coins(max, settings)}.`, { path: 'balance' });
      const { maxBalance } = economyConfig(settings);
      if (data.balance > maxBalance) throw tooHigh(maxBalance);
      try {
        await setBalance(guildId, userId, data.balance, settings);
      } catch (err) {
        if (err instanceof EconomyError && err.code === 'INVALID') throw tooHigh(err.data?.max ?? maxBalance);
        throw err;
      }
      log(ctx, guildId, `coins set ${data.balance}`, userId);
      return {
        item: await after(guildId, userId, ctx, settings),
        message: `${profileName(profile, userId)} now has ${coins(data.balance, settings)}.`,
      };
    },

    /** /economy-admin reset what:<coins|items|all> member:<this member>. */
    async reset(guildId, id, data, ctx) {
      const userId = userIdOf(id);
      await checkHolder(ctx, userId);
      const settings = await getSettings(guildId);
      const r = await resetEconomy({ guildId, userId, what: data.what, settings });
      log(ctx, guildId, `reset ${data.what}`, userId);
      const done = [];
      if (data.what === 'coins' || data.what === 'all') done.push(`${r.accounts} balance${r.accounts === 1 ? '' : 's'}`);
      if (data.what === 'items' || data.what === 'all') done.push(`${r.inventories} inventory entr${r.inventories === 1 ? 'y' : 'ies'}`);
      return { item: await after(guildId, userId, ctx, settings), message: `Reset ${done.join(' and ')}.` };
    },
  }),
});
