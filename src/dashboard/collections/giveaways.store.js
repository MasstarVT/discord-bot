// Database access of the `giveaways` collection. Reads are plain queries; every write is a dashboard task the bot runs with
// /giveaway's own service (src/services/dashboardTasks/giveaways.js), because posting and ending a giveaway need the gateway
// client (the Participate message, the draw, the announcement, the prizes). The bot picks a task up within about 5 s; the
// request waits up to 12 s for it and answers "still working" after that.
// The list is paged: the first page holds every running and draft giveaway plus the newest finished ones, and `nextCursor`
// loads older finished ones.
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { RecordError, taskOutcome } from './errors.js';
import { giveawayItem } from './giveaways.js';

const OPEN = ['ACTIVE', 'DRAFT'];
const DONE = ['ENDED', 'CANCELLED'];
/** Finished giveaways per page. */
export const HISTORY_PAGE = 25;
const CURSOR_RE = /^(\d{1,15}):([A-Za-z0-9]{1,40})$/;
const ID_RE = /^[A-Za-z0-9_-]{1,40}$/;
const WITH_ENTRIES = { _count: { select: { entries: true } } };

/** Most user lookups one list makes (discord.getUsers takes at most 100). */
const NAME_LOOKUPS = 100;

const notFound = () => new RecordError('not_found', 'That giveaway no longer exists.');

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

/** Items for rows, host and winner names resolved in one lookup (hosts first, then winners in list order). */
async function itemsFor(rows, discord) {
  const ids = [...rows.map((r) => r.hostId), ...rows.flatMap((r) => r.winnerIds ?? [])];
  const names = await namesFor(discord, ids);
  return rows.map((r) => giveawayItem(r, { names }));
}

const findRow = (guildId, id, extra = {}) => (ID_RE.test(String(id))
  ? prisma.giveaway.findFirst({ where: { id, guildId }, ...extra })
  : null);

async function loadItem(guildId, id, discord = null) {
  const row = await findRow(guildId, id, { include: WITH_ENTRIES });
  return row ? (await itemsFor([row], discord))[0] : null;
}

/** The giveaway this dashboard user just started, for the rare case the task's answer carries no id. */
function findStarted(guildId, userId, since) {
  return prisma.giveaway.findFirst({
    where: { guildId, hostId: String(userId), createdAt: { gte: since } },
    orderBy: { createdAt: 'desc' },
    select: { id: true },
  });
}

function startPayload(d) {
  return {
    prize: d.prize,
    description: d.description ?? null,
    channelId: d.channelId,
    winnerCount: d.winnerCount,
    duration: d.duration ?? null,
    endsAt: d.endsAt ?? null,
    requiredRoleIds: d.requiredRoleIds ?? [],
    deniedRoleIds: d.deniedRoleIds ?? [],
    roleWeights: d.roleWeights ?? [],
    xpPrize: d.xpPrize ?? null,
    coinPrize: d.coinPrize ?? null,
    pingRoleId: d.pingRoleId ?? null,
    embedColor: d.embedColor ?? null,
  };
}

/** Runs a task about one existing giveaway and answers with the fresh item and the bot's message. */
async function act(guildId, id, ctx, kind, payload = {}) {
  const run = runner(ctx);
  if (!(await findRow(guildId, id, { select: { id: true } }))) throw notFound();
  const out = taskOutcome(await run(kind, { giveawayId: id, ...payload }));
  const item = await loadItem(guildId, id, ctx.discord);
  if (!item) throw notFound();
  return { item, message: out.message };
}

export const store = Object.freeze({
  /**
   * @param {string} guildId
   * @param {{ query?: string, cursor?: string, discord?: object }} [opts] discord: when the caller passes the REST client,
   *   hosts and winners show their names instead of ids
   */
  async list(guildId, { query = '', cursor = '', discord = null } = {}) {
    const q = String(query ?? '').trim().slice(0, 100);
    const search = q ? { prize: { contains: q, mode: 'insensitive' } } : {};
    let after = null;
    if (cursor) {
      const m = CURSOR_RE.exec(cursor);
      if (!m) return { items: [] };
      after = { at: new Date(Number(m[1])), id: m[2] };
    }

    let open = [];
    if (!after) {
      open = await prisma.giveaway.findMany({
        where: { guildId, status: { in: OPEN }, ...search },
        include: WITH_ENTRIES,
        orderBy: [{ endsAt: 'asc' }, { id: 'asc' }],
        take: LIMITS.ACTIVE_GIVEAWAYS + 50,
      });
      // Running ones (soonest end first) before drafts.
      open.sort((a, b) => (a.status === b.status ? 0 : a.status === 'ACTIVE' ? -1 : 1));
    }
    const done = await prisma.giveaway.findMany({
      where: {
        guildId,
        status: { in: DONE },
        ...search,
        ...(after ? { OR: [{ createdAt: { lt: after.at } }, { createdAt: after.at, id: { lt: after.id } }] } : {}),
      },
      include: WITH_ENTRIES,
      orderBy: [{ createdAt: 'desc' }, { id: 'desc' }],
      take: HISTORY_PAGE + 1,
    });
    const page = done.slice(0, HISTORY_PAGE);
    const last = page.at(-1);
    const items = await itemsFor([...open, ...page], discord);
    return done.length > HISTORY_PAGE && last ? { items, nextCursor: `${last.createdAt.getTime()}:${last.id}` } : { items };
  },

  async get(guildId, id) {
    return loadItem(guildId, id);
  },

  /** Starts a giveaway (/giveaway start). */
  async create(guildId, data, ctx) {
    const run = runner(ctx);
    const since = new Date(Date.now() - 2000);
    const out = taskOutcome(await run('giveaway.start', startPayload(data)));
    let id = typeof out.data?.giveawayId === 'string' ? out.data.giveawayId : null;
    // The answer is kept in Redis; without it (or while the bot is still working) look the new giveaway up.
    if (!id) id = (await findStarted(guildId, ctx.userId, since))?.id ?? null;
    const item = id ? await loadItem(guildId, id, ctx.discord) : null;
    if (!item) {
      throw new RecordError('unavailable', out.status === 'pending'
        ? out.message
        : 'The giveaway was started, but it could not be loaded. Reload the list.');
    }
    return item;
  },

  /** Changes the bonus odds (/giveaway odds add and remove): the only thing a posted giveaway lets you edit. */
  async update(guildId, id, data, ctx) {
    const run = runner(ctx);
    if (!(await findRow(guildId, id, { select: { id: true } }))) throw notFound();
    if (Object.hasOwn(data, 'roleWeights')) {
      taskOutcome(await run('giveaway.odds', { giveawayId: id, roleWeights: data.roleWeights }));
    }
    const item = await loadItem(guildId, id, ctx.discord);
    if (!item) throw notFound();
    return item;
  },

  async remove() {
    throw new RecordError('invalid', 'Giveaways cannot be deleted. Cancel a running one; finished ones stay as history.');
  },

  actions: Object.freeze({
    /** /giveaway end */
    end: (guildId, id, data, ctx) => act(guildId, id, ctx, 'giveaway.end'),
    /** /giveaway reroll */
    reroll: (guildId, id, data, ctx) => act(guildId, id, ctx, 'giveaway.reroll', { winners: data.winners }),
    /** /giveaway restart */
    restart: (guildId, id, data, ctx) => act(guildId, id, ctx, 'giveaway.restart', {
      channelId: data.channelId ?? null, duration: data.duration ?? null, endsAt: data.endsAt ?? null,
    }),
    /** /giveaway cancel */
    cancel: (guildId, id, data, ctx) => act(guildId, id, ctx, 'giveaway.cancel'),
  }),
});
