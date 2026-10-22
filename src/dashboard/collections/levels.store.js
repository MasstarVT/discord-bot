// Database access of the `levels` collection: the UserLevel rows of the guild ordered like the leaderboard (XP
// descending, then user id), in pages. Every change runs in the bot (task `xp.adjust`, the same xpService calls as /levelconfig xp),
// because reward roles and the `levelUp` hook need the gateway; the store then reads the member's row back.
import prisma from '../../database/client.js';
import { RecordError, taskOutcome } from './errors.js';
import {
  PAGE_SIZE, PENDING_CREATE, afterRankCursor, emptyLevelItem, isUserId, levelItem, loadProfiles, rankCursor, searchUserIds,
} from './levels.js';

const SELECT = { userId: true, xp: true, messageCount: true, voiceMinutes: true, lastXpAt: true };
const ORDER = [{ xp: 'desc' }, { userId: 'asc' }];

/** 1-based leaderboard position (rankWhere of xpService: more XP, or the same XP and a smaller id, ranks ahead). */
const rankOf = (guildId, row) => prisma.userLevel
  .count({ where: { guildId, OR: [{ xp: { gt: row.xp } }, { xp: row.xp, userId: { lt: row.userId } }] } })
  .then((n) => n + 1);

/**
 * Items for rows. `contiguous`: the rows follow each other on the leaderboard, so one count gives every rank.
 */
async function toItems(guildId, rows, { discord, logger, contiguous }) {
  if (!rows.length) return [];
  const [profiles, ranks] = await Promise.all([
    loadProfiles(discord, rows.map((r) => r.userId), logger),
    contiguous
      ? rankOf(guildId, rows[0]).then((first) => rows.map((_, i) => first + i))
      : Promise.all(rows.map((r) => rankOf(guildId, r))),
  ]);
  return rows.map((row, i) => levelItem(row, { rank: ranks[i], profile: profiles.get(row.userId) ?? null }));
}

async function readItem(guildId, userId, ctx = {}) {
  const row = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId } }, select: SELECT });
  if (!row) return null;
  return (await toItems(guildId, [row], { discord: ctx?.discord, logger: ctx?.logger, contiguous: true }))[0];
}

function requireTasks(ctx) {
  if (typeof ctx?.runTask !== 'function') throw new RecordError('unavailable', 'The bot cannot be reached from the dashboard right now.');
}

/**
 * Runs one xp.adjust for a member: `{ item, message, status }` with the item as it is now (a reset member reads as 0 XP;
 * while the task is still pending, the stored row and the "still working" message).
 */
async function adjust(guildId, userId, payload, ctx) {
  requireTasks(ctx);
  const outcome = taskOutcome(await ctx.runTask('xp.adjust', { ...payload, userId }));
  let item = await readItem(guildId, userId, ctx);
  if (!item) {
    const profiles = await loadProfiles(ctx?.discord, [userId], ctx?.logger);
    item = emptyLevelItem(userId, profiles.get(userId) ?? null);
  }
  return { item, message: outcome.message, status: outcome.status };
}

/** A row id is the member's user id (the server already answered 404 for a member without a row). */
function userIdOf(id) {
  if (!isUserId(id)) throw new RecordError('not_found', 'That member has no XP in this server.');
  return id;
}

export const store = Object.freeze({
  /**
   * A page of the leaderboard. `query`: a user id or mention, or the start of a member's name (Discord's member search).
   * @param {string} guildId
   * @param {{ query?: string, cursor?: string, discord?: object, logger?: object }} [opts]
   * @param {{ discord?: object, logger?: object }} [ctx]
   */
  async list(guildId, { query = '', cursor = '', ...rest } = {}, ctx = {}) {
    const discord = rest.discord ?? ctx.discord;
    const logger = rest.logger ?? ctx.logger;
    const q = String(query ?? '').trim();
    if (q) {
      const ids = await searchUserIds(discord, guildId, q);
      if (!ids.length) return { items: [] };
      const rows = await prisma.userLevel.findMany({ where: { guildId, userId: { in: ids } }, orderBy: ORDER, take: PAGE_SIZE, select: SELECT });
      return { items: await toItems(guildId, rows, { discord, logger, contiguous: false }) };
    }
    const rows = await prisma.userLevel.findMany({
      where: { guildId, ...afterRankCursor('xp', cursor) }, orderBy: ORDER, take: PAGE_SIZE + 1, select: SELECT,
    });
    const page = rows.slice(0, PAGE_SIZE);
    const items = await toItems(guildId, page, { discord, logger, contiguous: true });
    const last = page[page.length - 1];
    return rows.length > PAGE_SIZE ? { items, nextCursor: rankCursor(last.xp, last.userId) } : { items };
  },

  async get(guildId, id, ctx = {}) {
    return isUserId(id) ? readItem(guildId, id, ctx) : null;
  },

  /** Gives XP to a member (/levelconfig xp give) and answers their row. */
  async create(guildId, data, ctx) {
    const { item, message, status } = await adjust(guildId, data.userId, { op: 'give', amount: data.amount }, ctx);
    if (status === 'pending') throw new RecordError('unavailable', PENDING_CREATE, { saved: true });
    ctx.logger?.info?.(`Dashboard XP give ${data.amount} to ${data.userId} in ${guildId} by ${ctx.userId}: ${message}`);
    return item;
  },

  async update() {
    throw new RecordError('invalid', 'Use Give XP, Remove XP, Set XP or Reset XP to change a member.');
  },

  async remove() {
    throw new RecordError('invalid', 'Use "Reset XP" to wipe a member\'s XP.');
  },

  actions: Object.freeze({
    /** /levelconfig xp give: `{ amount }`. */
    give: (guildId, id, data, ctx) => adjust(guildId, userIdOf(id), { op: 'give', amount: data.amount }, ctx),
    /** /levelconfig xp remove: `{ amount }`. */
    take: (guildId, id, data, ctx) => adjust(guildId, userIdOf(id), { op: 'take', amount: data.amount }, ctx),
    /** /levelconfig xp set: `{ xp }`. */
    set: (guildId, id, data, ctx) => adjust(guildId, userIdOf(id), { op: 'set', amount: data.xp }, ctx),
    /** /levelconfig xp reset. */
    reset: (guildId, id, data, ctx) => adjust(guildId, userIdOf(id), { op: 'reset' }, ctx),
  }),
});
