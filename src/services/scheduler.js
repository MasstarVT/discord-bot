// Persistent job scheduler (01-foundation §6). Jobs live in the ScheduledJob table, are claimed
// with FOR UPDATE SKIP LOCKED by the shard that owns the (available) guild, and survive restarts.
// Raw SQL follows 00-overview rule 22: SQL_NOW only; every other write goes through Prisma.
import { threadId } from 'node:worker_threads';
import { Prisma } from '@prisma/client';
import prisma, { SQL_NOW } from '../database/client.js';
import logger from '../utils/logger.js';
import { mapLimit, withTimeout } from '../utils/async.js';
import { INTERACTIVE_JOB_TYPES, isJobType } from '../config/jobTypes.js';
import { ensureGuildSettings } from './settingsService.js';

/** Throw from a handler to fail the job without retries. */
export class PermanentJobError extends Error {
  constructor(message) {
    super(message);
    this.name = 'PermanentJobError';
  }
}

export const POLL_INTERVAL_MS = 5_000;
/** Interactive jobs (INTERACTIVE_JOB_TYPES: someone waits for them) have their own loop, so slow jobs never delay them. */
export const INTERACTIVE_POLL_INTERVAL_MS = 1_000;
/**
 * The interactive loop polls every ~1 s only this long after it last claimed a job, then every ~5 s: dashboard tasks
 * are its only type and nobody uses the dashboard most of the time. The dashboard waits 12 s for a task (TASK_WAIT_MS).
 */
export const INTERACTIVE_ACTIVE_WINDOW_MS = 2 * 60_000;
export const INTERACTIVE_IDLE_POLL_INTERVAL_MS = 5_000;
/** An interactive job this late is failed instead of run: whoever asked has given up waiting. */
export const INTERACTIVE_MAX_LATE_MS = 60_000;
export const CLAIM_BATCH = 25;
export const JOB_CONCURRENCY = 5;
/** Interactive jobs one shard runs at once, at most one per guild (see fillInteractiveSlots). */
export const INTERACTIVE_CONCURRENCY = 10;
export const JOB_TIMEOUT_MS = 60_000;
export const LOCK_MS = 5 * 60_000;
export const LIVE_STATUSES = Object.freeze(['PENDING', 'RUNNING']);

const GC_INTERVAL_MS = 60 * 60_000;
const NO_HANDLER_BACKOFF_MS = 10 * 60_000;
const RETRY_BASE_MS = 30_000;
const RETRY_MAX_MS = 60 * 60_000;
const LAST_ERROR_MAX = 2000;
const CONFLICT_MODES = Object.freeze(['replace', 'keep', 'earliest', 'merge']);

/** A JSON round trip of `value` (a payload), so what is stored is exactly what a later claim reads. */
function serialisable(value) {
  const text = JSON.stringify(value ?? {});
  if (text === undefined) throw new Error('payload serialises to undefined');
  return JSON.parse(text);
}

/** @type {Map<string, Function>} */
const handlers = new Map();
const warnedMissing = new Set();
/** In-flight ticks, so stop() can wait for running jobs. */
const inflight = new Set();
/** Ordinary jobs the slot loop is running now (at most JOB_CONCURRENCY). */
let running = 0;
/** The claim in progress, if any, and whether another was asked for meanwhile (see requestFill). */
let filling = null;
let refillAsked = false;
/** The last claim filled every free slot, so more jobs may be due: finished jobs refill at once until one comes back short. */
let backlog = false;
/** The same for the interactive loop: jobs it runs now, the claim in progress, and when it last claimed one. */
let interactiveRunning = 0;
let interactiveFilling = null;
let interactiveRefillAsked = false;
let lastInteractiveClaim = -Infinity;
let started = false;
let pollTimer = null;
let interactiveTimer = null;
let gcTimer = null;

// ── Raw SQL ───────────────────────────────────────────────────────────────────

// Which guilds' jobs a claim may take. `guildIds`: this shard's available guilds. `shards`: every guild of these shards,
// by Discord's formula (guild_id >> 22) % shard_count, so the interactive loop's fast poll sends no guild list; null: all
// guilds (one process holds them all). The CASE keeps one malformed guildId from failing every claim.
const ownedSql = ({ guildIds, shards }) => {
  if (guildIds) return Prisma.sql`"guildId" = ANY(${guildIds}::text[])`;
  if (!shards) return Prisma.sql`TRUE`;
  return Prisma.sql`(CASE WHEN "guildId" ~ '^[0-9]{1,19}$' THEN ("guildId"::bigint >> 22) % ${shards.count}::bigint END) = ANY(${shards.ids}::bigint[])`;
};

// Interactive claims keep one job per guild in flight, so one guild's slow task never fills the interactive slots that every
// other guild's dashboard tasks wait for: a guild that already has one running (lock not expired) is skipped, and one claim
// takes only a guild's earliest due job. The rule is part of the same FOR UPDATE SKIP LOCKED statement as the claim, so every
// shard and process sees the same rows (a guild's interactive jobs go to one shard anyway); subselects lock nothing. A
// running row counts only for 90 s after its claim (JOB_TIMEOUT_MS plus slack): a handler cannot outlive that, so a row a
// crashed process left RUNNING does not hold its guild's dashboard tasks back for the whole 5 min lock.
const onePerGuildSql = () => Prisma.sql`
      AND NOT EXISTS (
        SELECT 1 FROM "ScheduledJob" r
        WHERE r."guildId" = j."guildId" AND r.type = ANY(${[...INTERACTIVE_JOB_TYPES]}::text[])
          AND r.status = 'RUNNING' AND r."lockedUntil" >= ${SQL_NOW} AND r."updatedAt" > ${SQL_NOW} - interval '90 seconds')
      AND NOT EXISTS (
        SELECT 1 FROM "ScheduledJob" e
        WHERE e."guildId" = j."guildId" AND e.type = ANY(${[...INTERACTIVE_JOB_TYPES]}::text[]) AND e.id <> j.id
          AND e."runAt" <= ${SQL_NOW} AND (e.status = 'PENDING' OR (e.status = 'RUNNING' AND e."lockedUntil" < ${SQL_NOW}))
          AND (e."runAt", e.id) < (j."runAt", j.id))`;

// `interactive`: only the interactive job types (their own loop, one per guild), otherwise every other type. `limit`: at
// most this many rows (both slot loops claim only as many jobs as they have free slots).
const claimSql = (owned, owner, { interactive = false, limit = CLAIM_BATCH } = {}) => Prisma.sql`
  WITH due AS (
    SELECT j.id FROM "ScheduledJob" j
    WHERE ${ownedSql(owned)}
      AND ${interactive ? Prisma.sql`type = ANY(${[...INTERACTIVE_JOB_TYPES]}::text[])` : Prisma.sql`NOT (type = ANY(${[...INTERACTIVE_JOB_TYPES]}::text[]))`}
      AND "runAt" <= ${SQL_NOW}
      AND (status = 'PENDING' OR (status = 'RUNNING' AND "lockedUntil" < ${SQL_NOW}))${interactive ? onePerGuildSql() : Prisma.empty}
    ORDER BY "runAt" ASC
    LIMIT ${limit}
    FOR UPDATE SKIP LOCKED
  )
  UPDATE "ScheduledJob" j
  SET status = 'RUNNING', attempts = j.attempts + 1, "lockedBy" = ${owner},
      "lockedUntil" = ${SQL_NOW} + interval '5 minutes', "updatedAt" = ${SQL_NOW}
  FROM due WHERE j.id = due.id
  RETURNING j.*`;

const lockDedupeSql = (dedupeKey) => Prisma.sql`
  SELECT id, "runAt", status, payload FROM "ScheduledJob" WHERE "dedupeKey" = ${dedupeKey} FOR UPDATE`;

const lockJobSql = (id) => Prisma.sql`
  SELECT id, version, payload FROM "ScheduledJob" WHERE id = ${id} FOR UPDATE`;

const gcSql = () => Prisma.sql`
  DELETE FROM "ScheduledJob"
  WHERE status IN ('DONE','FAILED','CANCELLED') AND "updatedAt" < ${SQL_NOW} - interval '7 days'`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'scheduler.claim', build: () => claimSql({ guildIds: ['0'] }, '0:0:0') },
  { name: 'scheduler.claimInteractive', build: () => claimSql({ shards: { count: 2, ids: [0] } }, '0:0:0', { interactive: true }) },
  { name: 'scheduler.claimInteractiveUnsharded', build: () => claimSql({ shards: null }, '0:0:0', { interactive: true }) },
  { name: 'scheduler.lockDedupe', build: () => lockDedupeSql('sample') },
  { name: 'scheduler.lockJob', build: () => lockJobSql('sample') },
  { name: 'scheduler.gc', build: () => gcSql() },
]);

// ── Handlers ──────────────────────────────────────────────────────────────────

/**
 * Registers the handler for a job type. A handler is
 * `async (job, ctx) => void | { nextRunAt: Date, payload?: object }`, where
 * job = { id, guildId, type, payload, runAt, attempts, maxAttempts, dedupeKey, version } and
 * ctx = { client, guild, lateByMs, attempt } (guild is cached and available; attempt is 1-based,
 * `attempt >= job.maxAttempts` means the final try). Handlers must be idempotent. ANY job type may
 * return { nextRunAt } to reschedule itself.
 * @param {string} type a JOB_TYPES key
 * @param {(job: object, ctx: object) => Promise<void|{ nextRunAt: Date, payload?: object }>} handler
 * @throws {Error} on an unknown type or a different handler already registered for it
 */
export function registerJobHandler(type, handler) {
  if (!isJobType(type)) throw new Error(`Unknown job type "${type}"`);
  if (typeof handler !== 'function') throw new TypeError(`Handler for job type "${type}" must be a function`);
  const existing = handlers.get(type);
  if (existing && existing !== handler) throw new Error(`A handler is already registered for job type "${type}"`);
  handlers.set(type, handler);
}

// ── Scheduling ────────────────────────────────────────────────────────────────

/**
 * Pure: what scheduleJob does when a job with the same dedupeKey may already exist.
 * A row that is not live (DONE, FAILED, CANCELLED) counts as absent and is replaced.
 * @param {{ status: string, runAt: Date }|null} existing
 * @param {{ runAt: Date }} incoming
 * @param {'replace'|'keep'|'earliest'|'merge'} mode
 * @returns {'insert'|'replace'|'keep'|'merge'}
 */
export function resolveConflict(existing, incoming, mode) {
  if (!existing) return 'insert';
  if (!LIVE_STATUSES.includes(existing.status)) return 'replace';
  if (mode === 'replace') return 'replace';
  if (mode === 'keep') return 'keep';
  if (mode === 'merge') return 'merge';
  if (mode === 'earliest') return incoming.runAt < existing.runAt ? 'replace' : 'keep';
  return 'replace';
}

/**
 * Creates (or, with a dedupeKey, upserts) a job. Calls ensureGuildSettings first.
 * @param {object} opts
 * @param {string} opts.guildId
 * @param {string} opts.type a JOB_TYPES key
 * @param {Date} opts.runAt
 * @param {object} [opts.payload] JSON-serialisable
 * @param {string|null} [opts.dedupeKey]
 * @param {number} [opts.maxAttempts=5]
 * @param {'replace'|'keep'|'earliest'|'merge'} [opts.onConflict='replace'] merge: a live row (PENDING or RUNNING) keeps its
 *   status, schedule and version and only its payload becomes `merge(existingPayload, payload)`, under the row lock, so a
 *   running job is never reset or started twice; a handler that settles itself with settleMergedJob picks the change up
 * @param {(existing: object, incoming: object) => object} [opts.merge] required with onConflict 'merge'
 * @returns {Promise<{ id: string, runAt: Date }>}
 * @throws {Error} on an unknown type, an invalid runAt, a payload that is not JSON-serialisable, or a DB failure
 */
export async function scheduleJob({ guildId, type, runAt, payload = {}, dedupeKey = null, maxAttempts = 5, onConflict = 'replace', merge = null }) {
  if (!isJobType(type)) throw new Error(`Unknown job type "${type}"`);
  if (!guildId || typeof guildId !== 'string') throw new Error('scheduleJob needs a guildId');
  const when = runAt instanceof Date ? runAt : new Date(runAt);
  if (Number.isNaN(when.getTime())) throw new Error(`Invalid runAt for job "${type}"`);
  if (!CONFLICT_MODES.includes(onConflict)) throw new Error(`Invalid onConflict "${onConflict}"`);
  if (onConflict === 'merge' && (typeof merge !== 'function' || !dedupeKey)) throw new Error("onConflict 'merge' needs a dedupeKey and a merge function");
  if (!Number.isInteger(maxAttempts) || maxAttempts < 1) throw new Error('maxAttempts must be a positive integer');
  let body;
  try {
    body = serialisable(payload);
  } catch (err) {
    throw new Error(`Job payload is not JSON-serialisable: ${err.message}`);
  }

  await ensureGuildSettings(guildId);

  if (!dedupeKey) {
    const row = await prisma.scheduledJob.create({ data: { guildId, type, runAt: when, payload: body, maxAttempts } });
    return { id: row.id, runAt: row.runAt };
  }

  // A unique-violation race (two inserters for the same new key) is retried once: the second
  // pass sees the winner's row and resolves the conflict normally.
  for (let attempt = 0; ; attempt++) {
    try {
      return await prisma.$transaction(async (tx) => {
        const [existing = null] = await tx.$queryRaw(lockDedupeSql(dedupeKey));
        const action = resolveConflict(existing, { runAt: when }, onConflict);
        if (action === 'keep') return { id: existing.id, runAt: existing.runAt };
        if (action === 'merge') {
          const row = await tx.scheduledJob.update({ where: { id: existing.id }, data: { payload: serialisable(merge(existing.payload ?? {}, body)) } });
          return { id: row.id, runAt: row.runAt };
        }
        if (action === 'replace') {
          const row = await tx.scheduledJob.update({
            where: { id: existing.id },
            data: {
              guildId, type, runAt: when, payload: body, maxAttempts,
              status: 'PENDING', attempts: 0, lastError: null, lockedBy: null, lockedUntil: null,
              version: { increment: 1 },
            },
          });
          return { id: row.id, runAt: row.runAt };
        }
        const row = await tx.scheduledJob.create({ data: { guildId, type, dedupeKey, runAt: when, payload: body, maxAttempts } });
        return { id: row.id, runAt: row.runAt };
      });
    } catch (err) {
      if (err?.code === 'P2002' && attempt === 0) continue;
      throw err;
    }
  }
}

/**
 * Deletes matching jobs whatever their status (a running handler's completion update then affects
 * 0 rows). At least one filter is required; `guildId` alone cancels every job of that guild.
 * An empty `types` or `ids` array matches nothing. Safe to call from inside the job's own handler.
 * Never throws.
 * @param {{ dedupeKey?: string, guildId?: string, type?: string, types?: string[], ids?: string[] }} filter
 * @returns {Promise<number>} rows deleted
 */
export async function cancelJobs({ dedupeKey, guildId, type, types, ids } = {}) {
  try {
    const where = {};
    if (dedupeKey) where.dedupeKey = dedupeKey;
    if (guildId) where.guildId = guildId;
    if (type || types) {
      const list = [...(types ?? []), ...(type ? [type] : [])];
      if (!list.length) return 0;
      where.type = { in: list };
    }
    if (ids) {
      if (!ids.length) return 0;
      where.id = { in: ids };
    }
    if (!Object.keys(where).length) {
      logger.warn('cancelJobs called without a filter; nothing cancelled');
      return 0;
    }
    const { count } = await prisma.scheduledJob.deleteMany({ where });
    return count;
  } catch (err) {
    logger.error('cancelJobs failed', err);
    return 0;
  }
}

/**
 * @param {string} dedupeKey
 * @returns {Promise<object|null>} the job row, or null (also on error)
 */
export async function getJob(dedupeKey) {
  try {
    return await prisma.scheduledJob.findUnique({ where: { dedupeKey } });
  } catch (err) {
    logger.error('getJob failed', err);
    return null;
  }
}

/** Keys per query of getJobsByKeys. */
const JOB_LOOKUP_CHUNK = 500;

/**
 * Many getJob calls in one query per 500 keys (the startup repairs check one key per row).
 * @param {string[]} dedupeKeys
 * @returns {Promise<Map<string, object>|null>} dedupeKey → job; a key with no job is absent. null on a database error,
 *   so a caller can tell "no jobs" from "could not look".
 */
export async function getJobsByKeys(dedupeKeys) {
  const keys = [...new Set(dedupeKeys ?? [])];
  const out = new Map();
  try {
    for (let i = 0; i < keys.length; i += JOB_LOOKUP_CHUNK) {
      const rows = await prisma.scheduledJob.findMany({ where: { dedupeKey: { in: keys.slice(i, i + JOB_LOOKUP_CHUNK) } } });
      for (const row of rows) out.set(row.dedupeKey, row);
    }
    return out;
  } catch (err) {
    logger.error('getJobsByKeys failed', err);
    return null;
  }
}

/**
 * The last step of a handler whose job takes scheduleJob 'merge' changes while it runs: under the row lock (so no merge can
 * slip in between the read and the write), hands the live payload to `next` and settles the job itself. `next` returns null
 * when nothing is left (the row is deleted), or the payload to run again with (the row goes back to PENDING at `runAt`, a
 * fresh claim). The handler then just returns: the scheduler's own settle finds the row gone or changed and does nothing.
 * Does nothing when the job was replaced or cancelled meanwhile.
 * @param {{ id: string, version: number }} job the job the handler was given
 * @param {(livePayload: object) => object|null} next
 * @param {Date} [runAt] when to run again
 * @returns {Promise<'done'|'again'|'gone'>}
 * @throws on a database failure (the scheduler then retries the job with its payload intact)
 */
export async function settleMergedJob(job, next, runAt = new Date()) {
  return prisma.$transaction(async (tx) => {
    const [live = null] = await tx.$queryRaw(lockJobSql(job.id));
    if (!live || live.version !== job.version) return 'gone';
    const payload = next(live.payload ?? {});
    if (payload === null) {
      await tx.scheduledJob.delete({ where: { id: job.id } });
      return 'done';
    }
    await tx.scheduledJob.update({
      where: { id: job.id },
      data: {
        status: 'PENDING', runAt, attempts: 0, lastError: null, lockedBy: null, lockedUntil: null,
        payload: serialisable(payload), version: { increment: 1 },
      },
    });
    return 'again';
  }, { timeout: 10_000 });
}

// ── Claim and execute ─────────────────────────────────────────────────────────

const ownerOf = (client) => `${client.shard?.ids?.join(',') ?? '0'}:${process.pid}:${threadId}`;

/** The shards this client runs, for claims by shard; null when this process holds every guild. */
function shardScope(client) {
  const count = Number(client.shard?.count ?? 1);
  const ids = (client.shard?.ids ?? []).map(Number).filter(Number.isInteger);
  return count > 1 && ids.length ? { count, ids } : null;
}

function availableGuildIds(client) {
  const ids = [];
  for (const [id, guild] of client.guilds.cache) if (guild?.available) ids.push(id);
  return ids;
}

const toJob = (row) => ({
  id: row.id, guildId: row.guildId, type: row.type, payload: row.payload, runAt: row.runAt,
  attempts: row.attempts, maxAttempts: row.maxAttempts, dedupeKey: row.dedupeKey, version: row.version,
});

const truncate = (text) => String(text).slice(0, LAST_ERROR_MAX);

/** Writes only while the job is still the version this run claimed (a replace or cancel wins). */
function settle(job, data) {
  return prisma.scheduledJob.updateMany({ where: { id: job.id, version: job.version }, data });
}

/** Gives the claim back without spending an attempt (the claim already incremented it). */
function release(job, extra = {}) {
  return settle(job, {
    status: 'PENDING', attempts: Math.max(job.attempts - 1, 0), lockedBy: null, lockedUntil: null, ...extra,
  });
}

async function fail(job, error, permanent) {
  const message = truncate(error?.message ?? error);
  if (permanent || job.attempts >= job.maxAttempts) {
    await settle(job, { status: 'FAILED', lastError: message, lockedBy: null, lockedUntil: null });
    return;
  }
  const delay = Math.min(RETRY_BASE_MS * 2 ** Math.max(job.attempts - 1, 0), RETRY_MAX_MS);
  await settle(job, {
    status: 'PENDING', runAt: new Date(Date.now() + delay), lastError: message, lockedBy: null, lockedUntil: null,
  });
}

async function runJob(client, row) {
  const job = toJob(row);

  if (job.attempts > job.maxAttempts) {
    await settle(job, {
      status: 'FAILED',
      lastError: `claimed ${job.attempts} times without completing (worker crashed or timed out)`,
      lockedBy: null, lockedUntil: null,
    });
    logger.error(`Job ${job.type} ${job.id} (guild ${job.guildId}) abandoned after ${job.attempts} claims`);
    return;
  }

  const interactive = INTERACTIVE_JOB_TYPES.includes(job.type);
  /** Fails an interactive job instead of letting it run later: whoever asked has been told it did not happen. */
  const refuse = (lastError) => settle(job, { status: 'FAILED', lastError, lockedBy: null, lockedUntil: null });

  const handler = handlers.get(job.type);
  if (!handler && interactive) {
    await refuse('This bot version cannot do that yet. Update the bot, then try again.');
    return;
  }
  if (!handler) {
    // A plugin may not have landed yet: wait without spending an attempt.
    await release(job, { runAt: new Date(Date.now() + NO_HANDLER_BACKOFF_MS) });
    if (!warnedMissing.has(job.type)) {
      warnedMissing.add(job.type);
      logger.warn(`No handler registered for job type ${job.type}; its jobs wait`);
    }
    return;
  }

  const guild = client.guilds.cache.get(job.guildId);
  if (!guild || guild.available === false) {
    if (interactive) {
      await refuse('Discord has this server unavailable right now, so nothing was done. Try again in a few minutes.');
      return;
    }
    // The claim filter skips the guild until it is available again; runAt stays as it was.
    await release(job);
    return;
  }

  const ctx = { client, guild, lateByMs: Math.max(0, Date.now() - new Date(job.runAt).getTime()), attempt: job.attempts };
  if (interactive && ctx.lateByMs > INTERACTIVE_MAX_LATE_MS) {
    await refuse('This request waited too long for the bot and was dropped, so nothing was done. Try again.');
    return;
  }
  let result;
  try {
    result = await withTimeout(Promise.resolve().then(() => handler(job, ctx)), JOB_TIMEOUT_MS, `job ${job.type}`);
    if (result && typeof result === 'object' && 'nextRunAt' in result) {
      const next = result.nextRunAt;
      if (!(next instanceof Date) || Number.isNaN(next.getTime())) throw new PermanentJobError('handler returned an invalid nextRunAt');
    }
  } catch (err) {
    logger.error(`Job ${job.type} ${job.id} (guild ${job.guildId}) failed on attempt ${job.attempts}/${job.maxAttempts}`, err);
    try {
      await fail(job, err, err instanceof PermanentJobError);
    } catch (dbErr) {
      logger.error(`Could not record the failure of job ${job.id}`, dbErr);
    }
    return;
  }

  try {
    if (result && typeof result === 'object' && 'nextRunAt' in result) {
      await settle(job, {
        status: 'PENDING', runAt: result.nextRunAt, attempts: 0, lastError: null,
        payload: result.payload ?? job.payload, lockedBy: null, lockedUntil: null, version: { increment: 1 },
      });
    } else {
      await prisma.scheduledJob.deleteMany({ where: { id: job.id, version: job.version } });
    }
  } catch (err) {
    // The row stays RUNNING and is re-claimed when its lock expires.
    logger.error(`Could not complete job ${job.type} ${job.id}`, err);
  }
}

async function runTick(client, { interactive = false } = {}) {
  try {
    // Interactive jobs are claimed by shard: a guild that is unavailable right now gets its job refused at once (runJob)
    // instead of a wait, and the poll does not send every guild id each second.
    let owned;
    if (interactive) {
      owned = { shards: shardScope(client) };
    } else {
      owned = { guildIds: availableGuildIds(client) };
      if (!owned.guildIds.length) return 0;
    }
    const rows = await prisma.$queryRaw(claimSql(owned, ownerOf(client), { interactive }));
    if (!rows.length) return 0;
    await mapLimit(rows, JOB_CONCURRENCY, (row) => runJob(client, row));
    return rows.length;
  } catch (err) {
    logger.error('Scheduler tick failed', err);
    return 0;
  }
}

/**
 * One claim-and-execute cycle (exported for tests). Never throws.
 * @param {import('discord.js').Client} client
 * @param {{ interactive?: boolean }} [opts] interactive: claim only INTERACTIVE_JOB_TYPES (their own loop); otherwise
 *   every other type
 * @returns {Promise<number>} jobs claimed
 */
export function tick(client, opts = {}) {
  const run = runTick(client, opts);
  inflight.add(run);
  const done = () => inflight.delete(run);
  run.then(done, done);
  return run;
}

/**
 * The ordinary jobs' loop: claims as many due jobs as there are free slots and starts them without waiting for them,
 * so one slow job (a feed poll, up to JOB_TIMEOUT_MS) never holds up the jobs due behind it. While the last claim
 * filled every free slot (a backlog), each finished job refills its slot at once, so the backlog drains without
 * waiting for the next poll; otherwise new work waits for the poll, which keeps the claim rate low when idle.
 * Exported for tests. Never throws.
 * @param {import('discord.js').Client} client
 * @returns {Promise<number>} jobs claimed
 */
export async function fillSlots(client) {
  const free = JOB_CONCURRENCY - running;
  if (!started || free <= 0) return 0;
  try {
    const guildIds = availableGuildIds(client);
    if (!guildIds.length) return 0;
    const rows = await prisma.$queryRaw(claimSql({ guildIds }, ownerOf(client), { limit: free }));
    backlog = rows.length === free;
    for (const row of rows) {
      running++;
      const run = runJob(client, row)
        .catch((err) => logger.error(`Job ${row.type} ${row.id} failed outside its handler`, err))
        .finally(() => {
          running--;
          inflight.delete(run);
          if (backlog) requestFill(client);
        });
      inflight.add(run);
    }
    return rows.length;
  } catch (err) {
    backlog = false; // the database is failing: leave the retry to the next poll
    logger.error('Scheduler tick failed', err);
    return 0;
  }
}

// Runs one claim at a time: a request while a claim is running is remembered and served right after it, so two
// claims never both count the same free slots.
function requestFill(client) {
  if (!started) return;
  if (filling) {
    refillAsked = true;
    return;
  }
  const run = fillSlots(client).finally(() => {
    inflight.delete(run);
    filling = null;
    if (refillAsked) {
      refillAsked = false;
      requestFill(client);
    }
  });
  filling = run;
  inflight.add(run);
}

/**
 * The interactive jobs' loop, built like fillSlots: claims only as many due interactive jobs as there are free interactive
 * slots (INTERACTIVE_CONCURRENCY) and starts them without waiting for them, so a slow dashboard task occupies one slot and
 * never the whole loop. The claim takes at most one job per guild and none for a guild that already has one running, so one
 * guild cannot fill the slots (claimSql). Every finished job refills at once, so a guild's next queued task starts as soon as
 * its last one is done. Exported for tests. Never throws.
 * @param {import('discord.js').Client} client
 * @returns {Promise<number>} jobs claimed
 */
export async function fillInteractiveSlots(client) {
  const free = INTERACTIVE_CONCURRENCY - interactiveRunning;
  if (!started || free <= 0) return 0;
  try {
    const rows = await prisma.$queryRaw(claimSql({ shards: shardScope(client) }, ownerOf(client), { interactive: true, limit: free }));
    if (rows.length) lastInteractiveClaim = Date.now();
    for (const row of rows) {
      interactiveRunning++;
      const run = runJob(client, row)
        .catch((err) => logger.error(`Job ${row.type} ${row.id} failed outside its handler`, err))
        .finally(() => {
          interactiveRunning--;
          inflight.delete(run);
          requestInteractiveFill(client);
        });
      inflight.add(run);
    }
    return rows.length;
  } catch (err) {
    logger.error('Scheduler tick failed', err);
    return 0;
  }
}

// One interactive claim at a time, like requestFill; resolves when that claim has been made (not when its jobs are done).
function requestInteractiveFill(client) {
  if (!started) return Promise.resolve(0);
  if (interactiveFilling) {
    interactiveRefillAsked = true;
    return interactiveFilling;
  }
  const run = fillInteractiveSlots(client).finally(() => {
    inflight.delete(run);
    interactiveFilling = null;
    if (interactiveRefillAsked) {
      interactiveRefillAsked = false;
      requestInteractiveFill(client);
    }
  });
  interactiveFilling = run;
  inflight.add(run);
  return run;
}

/** Hourly cleanup of finished jobs older than 7 days (shard 0 only). Never throws. @returns {Promise<number>} */
export async function gc() {
  try {
    return await prisma.$executeRaw(gcSql());
  } catch (err) {
    logger.error('Scheduler GC failed', err);
    return 0;
  }
}

// ── Lifecycle ─────────────────────────────────────────────────────────────────

const jitter = (ms) => Math.round(ms * (0.8 + Math.random() * 0.4));

/**
 * Pure. The interactive loop's next delay before jitter: INTERACTIVE_POLL_INTERVAL_MS within
 * INTERACTIVE_ACTIVE_WINDOW_MS of its last claim, otherwise INTERACTIVE_IDLE_POLL_INTERVAL_MS.
 * @param {number} sinceLastClaimMs Infinity when it has claimed nothing yet
 * @returns {number}
 */
export function interactivePollDelay(sinceLastClaimMs) {
  return sinceLastClaimMs < INTERACTIVE_ACTIVE_WINDOW_MS ? INTERACTIVE_POLL_INTERVAL_MS : INTERACTIVE_IDLE_POLL_INTERVAL_MS;
}

/**
 * Starts the poll loops (setTimeout chains, not setInterval, ±20 % jitter): every ~5 s the ordinary jobs' slot loop
 * (fillSlots; finished jobs also refill at once) and the interactive jobs every ~1 s for 2 min after one was claimed,
 * otherwise every ~5 s (interactivePollDelay). Idempotent. On the shard whose ids include 0 (or with no sharding) it
 * also starts the hourly GC.
 * @param {import('discord.js').Client} client
 */
export function start(client) {
  if (started) return;
  started = true;
  const poll = () => {
    if (!started) return;
    pollTimer = setTimeout(() => {
      requestFill(client);
      poll();
    }, jitter(POLL_INTERVAL_MS));
    pollTimer.unref?.();
  };
  poll();

  // Interactive jobs (someone waits for them) are claimed by their own, faster loop, never behind a slow job: it waits only
  // for the claim, and each job takes one interactive slot (fillInteractiveSlots).
  lastInteractiveClaim = -Infinity;
  const pollInteractive = () => {
    if (!started) return;
    interactiveTimer = setTimeout(async () => {
      try {
        await requestInteractiveFill(client);
      } catch (err) {
        logger.error('Scheduler poll failed', err);
      } finally {
        pollInteractive();
      }
    }, jitter(interactivePollDelay(Date.now() - lastInteractiveClaim)));
    interactiveTimer.unref?.();
  };
  pollInteractive();

  if (!client.shard || client.shard.ids?.includes(0)) {
    const sweep = () => {
      if (!started) return;
      gcTimer = setTimeout(async () => {
        try {
          await gc();
        } finally {
          sweep();
        }
      }, GC_INTERVAL_MS);
      gcTimer.unref?.();
    };
    sweep();
  }
}

/**
 * Clears the timers, then waits up to `timeoutMs` for in-flight jobs.
 * @param {{ timeoutMs?: number }} [opts]
 */
export async function stop({ timeoutMs = 5000 } = {}) {
  started = false;
  if (pollTimer) clearTimeout(pollTimer);
  if (interactiveTimer) clearTimeout(interactiveTimer);
  if (gcTimer) clearTimeout(gcTimer);
  pollTimer = null;
  interactiveTimer = null;
  gcTimer = null;
  if (!inflight.size) return;
  let timer;
  let timedOut = false;
  const timeout = new Promise((resolve) => { timer = setTimeout(() => { timedOut = true; resolve(); }, timeoutMs); });
  try {
    // A claim that was running when stop() was called starts its jobs afterwards: wait for those too.
    while (inflight.size && !timedOut) await Promise.race([Promise.allSettled([...inflight]), timeout]);
  } finally {
    clearTimeout(timer);
  }
}
