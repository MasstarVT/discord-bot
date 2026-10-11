// What the web dashboard and the bot share about dashboard tasks: actions the dashboard cannot do itself (they need the
// gateway client, e.g. banning with the moderation rules or posting a giveaway) are queued as `dashboard.task` jobs,
// run by the bot shard that has the guild, and answered through a short-lived Redis result. Pure.

export const TASK_JOB_TYPE = 'dashboard.task';

/** How long the bot keeps a task's result for the dashboard to read. */
export const TASK_RESULT_TTL_SECONDS = 900;

/** Redis key of a finished task's result `{ ok: true, message, data }` (the job row is gone once it succeeds). */
export const taskResultKey = (jobId) => `dashboard:task:${jobId}`;

/** The dedupe key of one task (unique per request). */
export const taskDedupeKey = (taskId) => `dashboard.task:${taskId}`;

export const TASK_KIND_RE = /^[a-z][a-zA-Z]{0,30}\.[a-z][a-zA-Z]{0,30}$/;

/** Most a task's message may hold (it is shown to the admin as is). */
export const TASK_MESSAGE_MAX = 500;

/**
 * An expected failure of a task: `message` is shown to the admin verbatim (e.g. "That member is not in the server.").
 * Anything else a task throws is logged and reported as a generic failure.
 */
export class TaskError extends Error {
  constructor(message) {
    super(message);
    this.name = 'TaskError';
  }
}
