// Runs `dashboard.task` jobs in the bot: every kind the web dashboard may queue, collected from the feature modules in
// this directory (each exports `tasks = { '<area>.<verb>': { run } }`), and the job handler the dashboard-tasks plugin
// registers. A task runs once (maxAttempts 1): its result, or the reason it failed, goes back to the waiting dashboard.
import { PermanentJobError } from '../scheduler.js';
import { cache } from '../redis.js';
import logger from '../../utils/logger.js';
import { TASK_MESSAGE_MAX, TASK_RESULT_TTL_SECONDS, TaskError, taskResultKey } from './shared.js';
import { tasks as moderation } from './moderation.js';
import { tasks as members } from './members.js';
import { tasks as giveaways } from './giveaways.js';
import { tasks as polls } from './polls.js';
import { tasks as reactionRoles } from './reactionRoles.js';
import { tasks as tickets } from './tickets.js';

/**
 * A task: `run({ guild, client, actor, payload, taskId })` → `{ message, data? }` or a thrown TaskError.
 *   actor = { id, member (GuildMember|null when not in the guild), isBotOwner }: the dashboard user who asked.
 * @typedef {{ run: (ctx: object) => Promise<{ message: string, data?: object }> }} DashboardTask
 */

/** kind -> task. A kind is claimed by exactly one module. */
export const TASKS = (() => {
  const out = {};
  for (const group of [moderation, members, giveaways, polls, reactionRoles, tickets]) {
    for (const [kind, task] of Object.entries(group)) {
      if (Object.hasOwn(out, kind)) throw new Error(`dashboard task ${kind} is declared twice`);
      out[kind] = Object.freeze(task);
    }
  }
  return Object.freeze(out);
})();

const clampMessage = (text) => String(text ?? '').slice(0, TASK_MESSAGE_MAX);

async function resolveActor(guild, actorId) {
  const isBotOwner = Boolean(process.env.BOT_OWNER_ID) && actorId === process.env.BOT_OWNER_ID;
  let member = null;
  if (typeof actorId === 'string' && /^\d{17,20}$/.test(actorId)) {
    member = await guild.members.fetch(actorId).catch(() => null);
  }
  return { id: actorId, member, isBotOwner };
}

/**
 * The `dashboard.task` job handler. Expected failures (TaskError) and unknown kinds fail the job permanently with the
 * message the dashboard shows; anything else is logged and fails it with a generic message.
 * @param {{ id: string, guildId: string, payload: object }} job
 * @param {{ client: import('discord.js').Client, guild: import('discord.js').Guild }} ctx
 */
export function runDashboardTask(job, ctx) {
  return executeTask(TASKS, job, ctx);
}

/**
 * Runs one task from `tasks` (TASKS, or a test double): the outcome is stored for the dashboard, a failure becomes a
 * PermanentJobError whose message the dashboard shows.
 * @param {Record<string, DashboardTask>} tasks
 */
export async function executeTask(tasks, job, { client, guild }) {
  const { kind, actorId, payload = {} } = job.payload ?? {};
  const task = typeof kind === 'string' && Object.hasOwn(tasks, kind) ? tasks[kind] : null;
  if (!task) throw new PermanentJobError(`This bot version cannot do "${String(kind).slice(0, 40)}" yet. Update the bot.`);

  let result;
  try {
    const actor = await resolveActor(guild, actorId);
    result = await task.run({ guild, client, actor, payload, taskId: job.id });
  } catch (err) {
    if (err instanceof TaskError) throw new PermanentJobError(clampMessage(err.message));
    logger.error(`Dashboard task ${kind} (job ${job.id}, guild ${guild.id}) failed`, err);
    throw new PermanentJobError('The bot hit an error doing that. Check its logs, then try again.');
  }
  const message = clampMessage(result?.message || 'Done.');
  await cache.set(taskResultKey(job.id), { ok: true, message, data: result?.data ?? null }, TASK_RESULT_TTL_SECONDS);
  logger.info(`Dashboard task ${kind} done in guild ${guild.id} for ${actorId}: ${message}`);
}
