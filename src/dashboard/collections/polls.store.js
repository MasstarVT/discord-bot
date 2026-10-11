// Database access of the `polls` collection. Reads are plain queries (vote counts included); every write is a dashboard
// task the bot runs with /poll's own service (src/services/dashboardTasks/polls.js), because posting and ending a poll need
// the gateway client (the vote buttons, the final edit, the results post). The bot picks a task up within about 5 s; the
// request waits up to 12 s for it and answers "still working" after that.
// The list is paged: the first page holds every active poll plus the newest ended ones, and `nextCursor` loads older ones.
import { Prisma } from '@prisma/client';
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { RecordError, taskOutcome } from './errors.js';
import { pollItem } from './polls.js';

/** Ended polls per page. */
export const HISTORY_PAGE = 25;
const CURSOR_RE = /^(\d{1,15}):([A-Za-z0-9]{1,40})$/;
const ID_RE = /^[A-Za-z0-9_-]{1,40}$/;
const NAME_LOOKUPS = 100;

// Distinct voters per poll (a multi-choice voter has several vote rows). One statement for the whole page.
const votersSql = (ids) => Prisma.sql`SELECT "pollId", COUNT(DISTINCT "userId")::int AS voters FROM "PollVote"
  WHERE "pollId" IN (${Prisma.join(ids)}) GROUP BY "pollId"`;

/** Raw statements (test/db.test.js runs each one). */
export const RAW_SQL = Object.freeze([
  { name: 'dashboard.polls.voters', build: () => votersSql(['0']) },
]);

const notFound = () => new RecordError('not_found', 'That poll no longer exists.');

function runner(ctx) {
  if (typeof ctx?.runTask !== 'function') throw new RecordError('unavailable', 'The bot cannot be reached from the dashboard right now.');
  return ctx.runTask;
}

/** Display names of users by id, best effort: no lookup client or a Discord failure leaves the raw ids. */
async function namesFor(discord, ids) {
  const wanted = [...new Set(ids)].slice(0, NAME_LOOKUPS);
  const names = new Map();
  if (!wanted.length || typeof discord?.getUsers !== 'function') return names;
  try {
    const users = await discord.getUsers(wanted);
    for (const [id, user] of users) {
      const name = user?.globalName || user?.username;
      if (name) names.set(id, name);
    }
  } catch {
    // names are decoration
  }
  return names;
}

/** Vote counts of polls: pollId -> { counts, votes, voters }. */
async function talliesFor(rows) {
  const out = new Map();
  if (!rows.length) return out;
  const ids = rows.map((r) => r.id);
  const [groups, voters] = await Promise.all([
    prisma.pollVote.groupBy({ by: ['pollId', 'optionIndex'], where: { pollId: { in: ids } }, _count: { _all: true } }),
    prisma.$queryRaw(votersSql(ids)),
  ]);
  for (const r of rows) out.set(r.id, { counts: new Array(r.options.length).fill(0), votes: 0, voters: 0 });
  for (const g of groups) {
    const t = out.get(g.pollId);
    if (t && Number.isInteger(g.optionIndex) && g.optionIndex >= 0 && g.optionIndex < t.counts.length) {
      t.counts[g.optionIndex] = Number(g._count?._all) || 0;
      t.votes += t.counts[g.optionIndex];
    }
  }
  for (const v of voters) {
    const t = out.get(v.pollId);
    if (t) t.voters = Number(v.voters) || 0;
  }
  return out;
}

async function itemsFor(rows, discord) {
  const [tallies, names] = await Promise.all([talliesFor(rows), namesFor(discord, rows.map((r) => r.authorId))]);
  return rows.map((r) => pollItem(r, { tally: tallies.get(r.id), names }));
}

const findRow = (guildId, id) => (ID_RE.test(String(id)) ? prisma.poll.findFirst({ where: { id, guildId } }) : null);

async function loadItem(guildId, id, discord = null) {
  const row = await findRow(guildId, id);
  return row ? (await itemsFor([row], discord))[0] : null;
}

/** The poll this dashboard user just posted, for the rare case the task's answer carries no id. */
function findStarted(guildId, userId, since) {
  return prisma.poll.findFirst({
    where: { guildId, authorId: String(userId), createdAt: { gte: since } },
    orderBy: { createdAt: 'desc' },
    select: { id: true },
  });
}

function startPayload(d) {
  return {
    question: d.question,
    options: d.options ?? [],
    channelId: d.channelId,
    maxChoices: d.maxChoices,
    duration: d.duration ?? null,
    allowedRoleIds: d.allowedRoleIds ?? [],
    deniedRoleIds: d.deniedRoleIds ?? [],
    hideResults: d.hideResults === true,
  };
}

export const store = Object.freeze({
  /**
   * @param {string} guildId
   * @param {{ query?: string, cursor?: string, discord?: object }} [opts] discord: when the caller passes the REST client,
   *   the author shows a name instead of an id
   */
  async list(guildId, { query = '', cursor = '', discord = null } = {}) {
    const q = String(query ?? '').trim().slice(0, 100);
    const search = q ? { question: { contains: q, mode: 'insensitive' } } : {};
    let after = null;
    if (cursor) {
      const m = CURSOR_RE.exec(cursor);
      if (!m) return { items: [] };
      after = { at: new Date(Number(m[1])), id: m[2] };
    }

    const active = after
      ? []
      : await prisma.poll.findMany({
        where: { guildId, status: 'ACTIVE', ...search },
        orderBy: [{ createdAt: 'desc' }, { id: 'desc' }],
        take: LIMITS.ACTIVE_POLLS + 50,
      });
    const ended = await prisma.poll.findMany({
      where: {
        guildId,
        status: 'ENDED',
        ...search,
        ...(after ? { OR: [{ createdAt: { lt: after.at } }, { createdAt: after.at, id: { lt: after.id } }] } : {}),
      },
      orderBy: [{ createdAt: 'desc' }, { id: 'desc' }],
      take: HISTORY_PAGE + 1,
    });
    const page = ended.slice(0, HISTORY_PAGE);
    const last = page.at(-1);
    const items = await itemsFor([...active, ...page], discord);
    return ended.length > HISTORY_PAGE && last ? { items, nextCursor: `${last.createdAt.getTime()}:${last.id}` } : { items };
  },

  async get(guildId, id) {
    return loadItem(guildId, id);
  },

  /** Posts a poll (/poll create). */
  async create(guildId, data, ctx) {
    const run = runner(ctx);
    const since = new Date(Date.now() - 2000);
    const out = taskOutcome(await run('poll.start', startPayload(data)));
    let id = typeof out.data?.pollId === 'string' ? out.data.pollId : null;
    // The answer is kept in Redis; without it (or while the bot is still working) look the new poll up.
    if (!id) id = (await findStarted(guildId, ctx.userId, since))?.id ?? null;
    const item = id ? await loadItem(guildId, id, ctx.discord) : null;
    if (!item) {
      throw new RecordError('unavailable', out.status === 'pending'
        ? out.message
        : 'The poll was posted, but it could not be loaded. Reload the list.');
    }
    return item;
  },

  async update() {
    throw new RecordError('invalid', 'A posted poll cannot be edited. End it, or delete it and post a new one.');
  },

  /** Removes the poll, its votes and its message (ending it silently first when it is still active). */
  async remove(guildId, id, ctx) {
    const run = runner(ctx);
    if (!(await findRow(guildId, id))) throw notFound();
    taskOutcome(await run('poll.delete', { pollId: id }));
  },

  actions: Object.freeze({
    /** /poll end */
    async end(guildId, id, data, ctx) {
      const run = runner(ctx);
      if (!(await findRow(guildId, id))) throw notFound();
      const out = taskOutcome(await run('poll.end', { pollId: id }));
      const item = await loadItem(guildId, id, ctx.discord);
      if (!item) throw notFound();
      return { item, message: out.message };
    },
  }),
});
