// Persistent job scheduler (01-foundation §6). Jobs live in the ScheduledJob table, are claimed
// with FOR UPDATE SKIP LOCKED by the shard that owns the (available) guild, and survive restarts.
// Raw SQL follows 00-overview rule 22: SQL_NOW only; every other write goes through Prisma.
import { threadId } from 'node:worker_threads';
import { Prisma } from '@prisma/client';
import prisma, { SQL_NOW } from '../database/client.js';
import logger from '../utils/logger.js';
import { mapLimit, withTimeout } from '../utils/async.js';
import { isJobType } from '../config/jobTypes.js';
import { ensureGuildSettings } from './settingsService.js';

/** Throw from a handler to fail the job without retries. */
export class PermanentJobError extends Error {
  constructor(message) {
    super(message);
    this.name = 'PermanentJobError';
  }
}

export const POLL_INTERVAL_MS = 5_000;
export const CLAIM_BATCH = 25;
export const JOB_CONCURRENCY = 5;
export const JOB_TIMEOUT_MS = 60_000;
export const LOCK_MS = 5 * 60_000;
export const LIVE_STATUSES = Object.freeze(['PENDING', 'RUNNING']);

const GC_INTERVAL_MS = 60 * 60_000;
const NO_HANDLER_BACKOFF_MS = 10 * 60_000;
const RETRY_BASE_MS = 30_000;
const RETRY_MAX_MS = 60 * 60_000;
const LAST_ERROR_MAX = 2000;
const CONFLICT_MODES = Object.freeze(['replace', 'keep', 'earliest']);

/** @type {Map<string, Function>} */
const handlers = new Map();
const warnedMissing = new Set();
/** In-flight ticks, so stop() can wait for running jobs. */
const inflight = new Set();
let started = false;
let pollTimer = null;
let gcTimer = null;

// ── Raw SQL ───────────────────────────────────────────────────────────────────

const claimSql = (guildIds, owner) => Prisma.sql`
  WITH due AS (
    SELECT id FROM "ScheduledJob"
    WHERE "guildId" = ANY(${guildIds}::text[])
      AND "runAt" <= ${SQL_NOW}
      AND (status = 'PENDING' OR (status = 'RUNNING' AND "lockedUntil" < ${SQL_NOW}))
    ORDER BY "runAt" ASC
    LIMIT ${CLAIM_BATCH}
    FOR UPDATE SKIP LOCKED
  )
  UPDATE "ScheduledJob" j
  SET status = 'RUNNING', attempts = j.attempts + 1, "lockedBy" = ${owner},
      "lockedUntil" = ${SQL_NOW} + interval '5 minutes', "updatedAt" = ${SQL_NOW}
  FROM due WHERE j.id = due.id
  RETURNING j.*`;

const lockDedupeSql = (dedupeKey) => Prisma.sql`
  SELECT id, "runAt", status FROM "ScheduledJob" WHERE "dedupeKey" = ${dedupeKey} FOR UPDATE`;

const gcSql = () => Prisma.sql`
  DELETE FROM "ScheduledJob"
  WHERE status IN ('DONE','FAILED','CANCELLED') AND "updatedAt" < ${SQL_NOW} - interval '7 days'`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'scheduler.claim', build: () => claimSql(['0'], '0:0:0') },
  { name: 'scheduler.lockDedupe', build: () => lockDedupeSql('sample') },
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
 * @param {'replace'|'keep'|'earliest'} mode
 * @returns {'insert'|'replace'|'keep'}
 */
export function resolveConflict(existing, incoming, mode) {
  if (!existing) return 'insert';
  if (!LIVE_STATUSES.includes(existing.status)) return 'replace';
  if (mode === 'replace') return 'replace';
  if (mode === 'keep') return 'keep';
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
 * @param {'replace'|'keep'|'earliest'} [opts.onConflict='replace']
 * @returns {Promise<{ id: string, runAt: Date }>}
 * @throws {Error} on an unknown type, an invalid runAt, a payload that is not JSON-serialisable, or a DB failure
 */
export async function scheduleJob({ guildId, type, runAt, payload = {}, dedupeKey = null, maxAttempts = 5, onConflict = 'replace' }) {
  if (!isJobType(type)) throw new Error(`Unknown job type "${type}"`);
  if (!guildId || typeof guildId !== 'string') throw new Error('scheduleJob needs a guildId');
  const when = runAt instanceof Date ? runAt : new Date(runAt);
  if (Number.isNaN(when.getTime())) throw new Error(`Invalid runAt for job "${type}"`);
  if (!CONFLICT_MODES.includes(onConflict)) throw new Error(`Invalid onConflict "${onConflict}"`);
  if (!Number.isInteger(maxAttempts) || maxAttempts < 1) throw new Error('maxAttempts must be a positive integer');
  let body;
  try {
    const text = JSON.stringify(payload ?? {});
    if (text === undefined) throw new Error('payload serialises to undefined');
    body = JSON.parse(text);
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

// ── Claim and execute ─────────────────────────────────────────────────────────

const ownerOf = (client) => `${client.shard?.ids?.join(',') ?? '0'}:${process.pid}:${threadId}`;

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

  const handler = handlers.get(job.type);
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
    // The claim filter skips the guild until it is available again; runAt stays as it was.
    await release(job);
    return;
  }

  const ctx = { client, guild, lateByMs: Math.max(0, Date.now() - new Date(job.runAt).getTime()), attempt: job.attempts };
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

async function runTick(client) {
  try {
    const guildIds = availableGuildIds(client);
    if (!guildIds.length) return 0;
    const rows = await prisma.$queryRaw(claimSql(guildIds, ownerOf(client)));
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
 * @returns {Promise<number>} jobs claimed
 */
export function tick(client) {
  const run = runTick(client);
  inflight.add(run);
  const done = () => inflight.delete(run);
  run.then(done, done);
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
 * Starts the poll loop (a setTimeout chain, not setInterval, ±20 % jitter). Idempotent. On the
 * shard whose ids include 0 (or with no sharding) it also starts the hourly GC.
 * @param {import('discord.js').Client} client
 */
export function start(client) {
  if (started) return;
  started = true;
  const poll = () => {
    if (!started) return;
    pollTimer = setTimeout(async () => {
      try {
        await tick(client);
      } catch (err) {
        logger.error('Scheduler poll failed', err);
      } finally {
        poll();
      }
    }, jitter(POLL_INTERVAL_MS));
    pollTimer.unref?.();
  };
  poll();

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
  if (gcTimer) clearTimeout(gcTimer);
  pollTimer = null;
  gcTimer = null;
  if (!inflight.size) return;
  let timer;
  const timeout = new Promise((resolve) => { timer = setTimeout(resolve, timeoutMs); });
  try {
    await Promise.race([Promise.allSettled([...inflight]), timeout]);
  } finally {
    clearTimeout(timer);
  }
}
