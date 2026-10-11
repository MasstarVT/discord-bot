// The dashboard's way to have the bot do something that needs the gateway client (ban with the moderation rules, post a
// giveaway, sync level roles, ...): queue a `dashboard.task` job for the shard that has the guild, then wait a few
// seconds for its outcome. The bot runs it within ~5 s (src/services/dashboardTasks/). A task nobody picks up in time is
// cancelled, so it can never run later by surprise.
// Importing this file opens Redis (scheduler → settingsService), so only recordsBridge.js imports it.
import { randomUUID } from 'node:crypto';
import prisma from '../database/client.js';
import { cache } from '../services/redis.js';
import { scheduleJob } from '../services/scheduler.js';
import { TASK_JOB_TYPE, TASK_KIND_RE, taskDedupeKey, taskResultKey } from '../services/dashboardTasks/shared.js';

/** How long a request waits for the bot (the bot polls every 5 s; most tasks take under a second). */
export const TASK_WAIT_MS = 12_000;
export const TASK_POLL_MS = 400;

const defaultSleep = (ms) => new Promise((resolve) => { setTimeout(resolve, ms).unref?.(); });

/**
 * @param {{ logger: object, waitMs?: number, pollMs?: number, sleep?: (ms: number) => Promise<void>,
 *   db?: object, kv?: object, schedule?: typeof scheduleJob }} opts db/kv/schedule: test doubles
 */
export function createTaskRunner({
  logger, waitMs = TASK_WAIT_MS, pollMs = TASK_POLL_MS, sleep = defaultSleep, db = prisma, kv = cache, schedule = scheduleJob,
}) {
  /**
   * Queues a task for the bot and waits for it.
   * @param {string} guildId
   * @param {string} kind e.g. 'mod.action' (a key of TASKS in src/services/dashboardTasks/index.js)
   * @param {object} payload plain JSON for the task
   * @param {{ actorId: string, waitMs?: number }} opts actorId: the dashboard user the bot acts for
   * @returns {Promise<{ status: 'done'|'failed'|'pending'|'offline', message: string, data: object|null }>}
   *   done: finished; failed: the bot refused (message says why); pending: still running when the wait ended;
   *   offline: no shard picked it up in time, so it was cancelled
   * @throws on a database failure while queueing (nothing was queued)
   */
  async function run(guildId, kind, payload, { actorId, waitMs: wait = waitMs } = {}) {
    if (!TASK_KIND_RE.test(kind)) throw new TypeError(`invalid dashboard task kind ${kind}`);
    const { id } = await schedule({
      guildId,
      type: TASK_JOB_TYPE,
      runAt: new Date(),
      payload: { kind, actorId, payload: payload ?? {} },
      dedupeKey: taskDedupeKey(randomUUID()),
      maxAttempts: 1,
    });

    /** The outcome once the job finished, or null while it is queued or running. */
    const outcome = async () => {
      const row = await db.scheduledJob.findUnique({ where: { id }, select: { status: true, lastError: true } });
      if (!row) {
        // Success deletes the row; the message waits in Redis (a Redis outage only loses the wording).
        const result = await kv.get(taskResultKey(id));
        return { status: 'done', message: typeof result?.message === 'string' ? result.message : 'Done.', data: result?.data ?? null };
      }
      if (row.status === 'FAILED' || row.status === 'CANCELLED') {
        return { status: 'failed', message: row.lastError || 'The bot could not do that.', data: null };
      }
      return null;
    };

    const deadline = Date.now() + wait;
    for (;;) {
      const done = await outcome();
      if (done) return done;
      if (Date.now() >= deadline) break;
      await sleep(pollMs);
    }

    // Still queued: cancel it unless a shard claimed it in the meantime (only a PENDING row is deleted).
    const { count } = await db.scheduledJob.deleteMany({ where: { id, status: 'PENDING' } });
    if (count === 0) {
      const late = await outcome();
      if (late) return late;
    }
    if (count === 1) {
      logger?.warn?.(`Dashboard: task ${kind} in guild ${guildId} was not picked up in ${Math.round(wait / 1000)} s; cancelled`);
      return {
        status: 'offline',
        message: 'The bot did not pick this up in time, so nothing was done. Check that the bot is online, then try again.',
        data: null,
      };
    }
    return { status: 'pending', message: 'The bot is still working on it. Refresh in a moment to see the result.', data: null };
  }

  return { run };
}
