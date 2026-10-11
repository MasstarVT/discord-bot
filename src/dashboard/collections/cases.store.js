// Database access of the `cases` collection: Infraction rows of the guild, newest first, in pages. Every write is done by
// the bot (dashboard tasks in src/services/dashboardTasks/moderation.js), because each one is a Discord action, a DM or a
// case-log post that needs the gateway client and the moderation rules of the slash commands:
//   create              → mod.action (the action, then the new case is read back)
//   update (reason/proof) → mod.editCase (/case reason | proof, with the case-log note)
//   clearWarnings       → mod.clearWarnings (/history clear)
// Cases are never deleted (no command deletes one).
import prisma from '../../database/client.js';
import { RecordError, taskOutcome } from './errors.js';
import { MAX_CASE_ID, actionPayload, caseItem, caseNumberQuery, reasonText } from './cases.js';
import { PAGE_SIZE, PENDING_CREATE, loadProfiles, searchUserIds } from './levels.js';

const CURSOR_RE = /^c(\d{1,10})$/;

const notFound = () => new RecordError('not_found', 'That case does not exist in this server.');

/** The case number of an item id, or null. */
function caseIdOf(id) {
  if (!/^\d{1,10}$/.test(String(id))) return null;
  const n = Number(id);
  return n >= 1 && n <= MAX_CASE_ID ? n : null;
}

function findCase(guildId, caseId) {
  return prisma.infraction.findUnique({ where: { guildId_caseId: { guildId, caseId } } });
}

/** Items with the names and avatars of their members and moderators. */
async function toItems(rows, discord, logger) {
  const profiles = await loadProfiles(discord, rows.flatMap((r) => [r.targetUserId, r.moderatorId]), logger);
  const now = Date.now();
  return rows.map((row) => caseItem(row, { profiles, now }));
}

function requireTasks(ctx) {
  if (typeof ctx?.runTask !== 'function') throw new RecordError('unavailable', 'The bot cannot be reached from the dashboard right now.');
}

async function itemFor(guildId, caseId, ctx) {
  const row = await findCase(guildId, caseId);
  if (!row) throw notFound();
  return (await toItems([row], ctx?.discord, ctx?.logger))[0];
}

export const store = Object.freeze({
  /**
   * A page of cases, newest first. `query`: a user id or mention, a case number ("12" or "#12"), or the start of a
   * member's name (Discord's member search: members still in the server).
   * @param {string} guildId
   * @param {{ query?: string, cursor?: string, discord?: object, logger?: object }} [opts]
   * @param {{ discord?: object, logger?: object }} [ctx]
   */
  async list(guildId, { query = '', cursor = '', ...rest } = {}, ctx = {}) {
    const discord = rest.discord ?? ctx.discord;
    const logger = rest.logger ?? ctx.logger;
    const where = { guildId };
    const q = String(query ?? '').trim();
    if (q) {
      const caseId = caseNumberQuery(q);
      if (caseId !== null) {
        const row = await findCase(guildId, caseId);
        return { items: row ? await toItems([row], discord, logger) : [] };
      }
      const ids = await searchUserIds(discord, guildId, q);
      if (!ids.length) return { items: [] };
      where.targetUserId = { in: ids };
    }
    const after = CURSOR_RE.exec(String(cursor ?? ''));
    if (after) where.caseId = { lt: Number(after[1]) };
    const rows = await prisma.infraction.findMany({ where, orderBy: { caseId: 'desc' }, take: PAGE_SIZE + 1 });
    const page = rows.slice(0, PAGE_SIZE);
    const items = await toItems(page, discord, logger);
    return rows.length > PAGE_SIZE ? { items, nextCursor: `c${page[page.length - 1].caseId}` } : { items };
  },

  async get(guildId, id, ctx = {}) {
    const caseId = caseIdOf(id);
    if (caseId === null) return null;
    const row = await findCase(guildId, caseId);
    return row ? (await toItems([row], ctx?.discord, ctx?.logger))[0] : null;
  },

  /** Performs the moderation action (task mod.action) and answers the new case. */
  async create(guildId, data, ctx) {
    requireTasks(ctx);
    const payload = actionPayload(data);
    const outcome = taskOutcome(await ctx.runTask('mod.action', payload));
    if (outcome.status === 'pending') throw new RecordError('unavailable', PENDING_CREATE);
    ctx.logger?.info?.(`Dashboard moderation ${payload.action} of ${payload.userId} in ${guildId} by ${ctx.userId}: ${outcome.message}`);
    const caseId = Number.isInteger(outcome.data?.caseId) ? outcome.data.caseId : null;
    let row = caseId ? await findCase(guildId, caseId) : null;
    // Without the task's data (its result expired in Redis), the newest case of that member is the one just made.
    row ??= await prisma.infraction.findFirst({ where: { guildId, targetUserId: payload.userId }, orderBy: { caseId: 'desc' } });
    if (!row) throw new RecordError('unavailable', `${outcome.message} Refresh the list to see the case.`);
    return (await toItems([row], ctx.discord, ctx.logger))[0];
  },

  /** Changes the reason and/or proof link (task mod.editCase, like /case reason and /case proof). */
  async update(guildId, id, data, ctx) {
    const caseId = caseIdOf(id);
    const row = caseId === null ? null : await findCase(guildId, caseId);
    if (!row) throw notFound();
    const payload = { caseId };
    if (Object.hasOwn(data, 'reason') && data.reason !== reasonText(row.reason)) payload.reason = data.reason;
    if (Object.hasOwn(data, 'proofUrl') && data.proofUrl !== null && data.proofUrl !== row.proofUrl) payload.proofUrl = data.proofUrl;
    if (Object.keys(payload).length === 1) return (await toItems([row], ctx?.discord, ctx?.logger))[0];
    requireTasks(ctx);
    const outcome = taskOutcome(await ctx.runTask('mod.editCase', payload));
    if (outcome.status === 'pending') throw new RecordError('unavailable', outcome.message);
    return itemFor(guildId, caseId, ctx);
  },

  async remove() {
    throw new RecordError('invalid', 'Moderation cases cannot be deleted. To pardon warnings, use "Clear warnings".');
  },

  actions: Object.freeze({
    /** /history clear for this case's member (task mod.clearWarnings: administrators only). */
    async clearWarnings(guildId, id, data, ctx) {
      const caseId = caseIdOf(id);
      const row = caseId === null ? null : await findCase(guildId, caseId);
      if (!row) throw notFound();
      requireTasks(ctx);
      const outcome = taskOutcome(await ctx.runTask('mod.clearWarnings', { userId: row.targetUserId, reason: data.clearReason ?? null }));
      return { item: await itemFor(guildId, caseId, ctx), message: outcome.message };
    },
  }),
});
