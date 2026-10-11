// The `dashboard.task` kinds of the Polls module: what the web dashboard asks the bot to do with /poll's own service
// functions (pollService.js), so the message with its vote buttons, the end job, the results post and the cap behave as when
// a moderator uses the command.
//   poll.start   /poll create (the form's submit)  payload: question, options, channelId, maxChoices, duration, roles, hideResults
//   poll.end     /poll end                          payload: { pollId }
//   poll.delete  (dashboard only)                   payload: { pollId } ends it silently, removes its message and the record
// Every task re-checks what the router and the command check: the module is on, the dashboard user is still in the server and
// is at least a moderator, then the command's and the form's own rules. The factory takes test doubles (`deps`).
import prisma from '../../database/client.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { MODULES } from '../../config/modules.js';
import { markBotDeletion } from '../../utils/deletionTracker.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import { clampText } from '../../utils/text.js';
import logger from '../../utils/logger.js';
import * as pollService from '../pollService.js';
import { scheduleJob } from '../scheduler.js';
import { getSettingsSafe, isModuleEnabled } from '../settingsService.js';
import { TaskError } from './shared.js';

const SNOWFLAKE_RE = /^\d{17,20}$/;
const POLL_ID_RE = /^c[a-z0-9]{20,31}$/;
const MAX_GATE_ROLES = 10;
const INVALID = 'Those poll details were not valid. Reload the page and try again.';
const DURATION_ERRORS = Object.freeze({
  invalid: 'Use a length like 30m, 2h, 1d or 1w.',
  'too-short': 'Polls must run for at least 1 minute.',
  'too-long': 'Polls can run for at most 30 days.',
});
// Discord codes that mean "already gone" (10003 unknown channel, 10008 unknown message) or "cannot reach it".
const GONE_CODES = new Set([10003, 10008]);
const NO_ACCESS_CODES = new Set([50001, 50013, 50083]);

const utc = (d) => `${d.toISOString().slice(0, 16).replace('T', ' ')} UTC`;
const isId = (v) => typeof v === 'string' && SNOWFLAKE_RE.test(v);
const invalid = () => new TaskError(INVALID);

function idList(value) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value) || value.length > MAX_GATE_ROLES || !value.every(isId)) throw invalid();
  return [...new Set(value)];
}

function pollId(payload) {
  const id = payload?.pollId;
  if (typeof id !== 'string' || !POLL_ID_RE.test(id)) throw new TaskError('That poll no longer exists.');
  return id;
}

/** The text of an error embed from pollService (channelProblem), as plain text for a toast. */
function embedText(embed, guild) {
  const text = String(embed?.data?.description ?? embed?.description ?? 'I cannot post there.');
  return text.replace(/\*\*/g, '').replace(/<#(\d+)>/g, (m, id) => `#${guild.channels?.cache?.get(id)?.name ?? 'the channel'}`);
}

/**
 * Builds the poll tasks.
 * @param {{ service?: object, getSettings?: Function, db?: object, schedule?: Function, now?: () => Date }} [deps] test doubles
 */
export function createPollTasks(deps = {}) {
  const svc = { ...pollService, ...deps.service };
  const getSettings = deps.getSettings ?? getSettingsSafe;
  const db = deps.db ?? prisma;
  const schedule = deps.schedule ?? scheduleJob;

  /** The router's gate and the command's level: the module is on, the user is in the server and is a moderator. */
  async function access({ guild, actor }) {
    if (!actor?.member && !actor?.isBotOwner) throw new TaskError('You are no longer a member of this server.');
    const settings = await getSettings(guild.id);
    if (!settings) throw new TaskError('The server settings could not be loaded. Try again in a moment.');
    if (!isModuleEnabled(settings, 'polls')) {
      throw new TaskError(`The ${MODULES.polls.emoji} ${MODULES.polls.label} module is turned off on this server. Turn it on first.`);
    }
    const level = actor.isBotOwner ? PERMISSION_LEVELS.OWNER : resolvePermissionLevel(actor.member, settings);
    if (level < PERMISSION_LEVELS.MODERATOR) throw new TaskError('You need to be a moderator of this server to manage polls.');
    return settings;
  }

  async function loadPoll(guild, id) {
    const poll = await db.poll.findFirst({ where: { id, guildId: guild.id } });
    if (!poll) throw new TaskError('That poll no longer exists.');
    return poll;
  }

  const channelName = (guild, id) => guild.channels?.cache?.get(id)?.name ?? 'the channel';

  async function roleExists(guild, id) {
    if (guild.roles?.cache?.has(id)) return true;
    if (typeof guild.roles?.fetch !== 'function') return false;
    return Boolean(await guild.roles.fetch(id).catch(() => null));
  }

  // ── poll.start ──────────────────────────────────────────────────────────────

  async function start(ctx) {
    const { guild, client, actor, payload } = ctx;
    await access(ctx);

    // The payload is the dashboard's validated form; it is read again as untrusted JSON.
    const singleLines = Array.isArray(payload?.options) && payload.options.every((o) => typeof o === 'string' && !/[\r\n]/.test(o));
    if (typeof payload?.question !== 'string' || !singleLines) throw invalid();
    if (!isId(payload.channelId)) throw invalid();
    const requestedChoices = payload.maxChoices === undefined ? 1 : payload.maxChoices;
    if (!Number.isInteger(requestedChoices) || requestedChoices < 1 || requestedChoices > svc.POLL.OPTIONS_MAX) {
      throw new TaskError(`Pick between 1 and ${svc.POLL.OPTIONS_MAX} choices per member.`);
    }
    if (payload.duration !== undefined && payload.duration !== null && typeof payload.duration !== 'string') throw invalid();
    if (payload.hideResults !== undefined && typeof payload.hideResults !== 'boolean') throw invalid();
    const allowedIds = idList(payload.allowedRoleIds);
    const deniedIds = idList(payload.deniedRoleIds);

    const q = svc.validateQuestion(payload.question);
    const o = svc.parsePollOptions(payload.options.join('\n'));
    const problems = [...q.errors, ...o.errors];
    if (problems.length) throw new TaskError(`That poll is not valid. ${problems.join(' ')}`);

    const d = svc.parseDurationOption(payload.duration);
    if (d.error) throw new TaskError(DURATION_ERRORS[d.error]);

    if (deniedIds.includes(guild.id)) throw new TaskError("You can't block @everyone, or nobody could vote.");
    if (allowedIds.some((id) => deniedIds.includes(id))) throw new TaskError('The allowed roles and the blocked roles must be different.');
    for (const id of [...allowedIds, ...deniedIds]) {
      if (id !== guild.id && !(await roleExists(guild, id))) throw new TaskError('A role you picked no longer exists. Pick another one.');
    }
    const allowedRoleIds = allowedIds.filter((id) => id !== guild.id); // @everyone = no restriction

    const channel = guild.channels.cache.get(payload.channelId) ?? await guild.channels.fetch(payload.channelId).catch(() => null);
    // A bot owner outside the server has no member to check; the bot's own access is still checked.
    const problem = svc.channelProblem(channel, actor.member ?? guild.members?.me);
    if (problem) throw new TaskError(channel ? embedText(problem, guild) : 'That channel no longer exists. Pick another one.');

    const maxChoices = Math.min(requestedChoices, o.options.length);
    const pending = {
      guildId: guild.id,
      channelId: channel.id,
      userId: actor.id,
      maxChoices: requestedChoices,
      allowedRoleIds,
      deniedRoleIds: deniedIds,
      hideResults: payload.hideResults === true,
      durationMs: d.ms,
    };
    const created = await svc.createPoll(client, guild, pending, { question: q.question, options: o.options, maxChoices });
    if (!created.ok) throw new TaskError(`This server already has ${svc.POLL.ACTIVE_MAX} active polls. End one first.`);
    const { poll } = created;

    // Post, record the message id, schedule the end: the same steps (and clean-ups) as the form submit.
    let msg;
    try {
      const settings = await getSettings(guild.id);
      msg = await channel.send(svc.buildPollPayload(poll, svc.emptyTally(poll.options.length), { settings }));
    } catch (err) {
      logger.warn('dashboard poll: could not post the poll message', err);
      await db.poll.delete({ where: { id: poll.id } }).catch(() => {});
      throw new TaskError(`I couldn't post the poll in #${channel.name}: ${svc.sendFailureReason(err)}`);
    }
    try {
      await db.poll.update({ where: { id: poll.id }, data: { messageId: msg.id } });
    } catch (err) {
      logger.error('dashboard poll: could not store the poll message id', err);
      await msg.delete?.().catch(() => {});
      await db.poll.delete({ where: { id: poll.id } }).catch(() => {});
      throw new TaskError("I couldn't save the poll. Please try again in a moment.");
    }

    let timerFailed = false;
    if (d.ms) {
      try {
        await schedule({
          guildId: guild.id, type: svc.END_JOB_TYPE, runAt: poll.endsAt, payload: { pollId: poll.id }, dedupeKey: svc.endJobKey(poll.id),
        });
      } catch (err) {
        timerFailed = true;
        logger.error('dashboard poll: could not schedule poll.end', err);
      }
    }

    let message = `Posted the poll "${clampText(q.question, 80)}" in #${channel.name}.`;
    if (d.ms) message += ` It ends ${utc(poll.endsAt)}.`;
    if (maxChoices < requestedChoices) message += ` Choices per member was lowered to ${maxChoices} (the number of options).`;
    if (timerFailed) message += ' The end timer could not be scheduled, so end the poll by hand.';
    return { message, data: { pollId: poll.id } };
  }

  // ── poll.end ────────────────────────────────────────────────────────────────

  async function end(ctx) {
    const { guild, client, payload } = ctx;
    await access(ctx);
    const poll = await loadPoll(guild, pollId(payload));
    const r = await svc.endPoll(client, poll.id, { reason: 'manual', guildId: guild.id });
    if (!r.ok) throw new TaskError(r.code === 'ALREADY_ENDED' ? 'That poll has already ended.' : 'That poll no longer exists.');
    const where = channelName(guild, poll.channelId);
    return {
      message: r.posted
        ? `Ended "${clampText(poll.question, 80)}". The results were posted in #${where}.`
        : `The poll is closed, but I couldn't post the results in #${where}.`,
      data: { pollId: poll.id },
    };
  }

  // ── poll.delete ─────────────────────────────────────────────────────────────

  async function remove(ctx) {
    const { guild, client, payload } = ctx;
    await access(ctx);
    const poll = await loadPoll(guild, pollId(payload));

    // An active poll ends first, the way it does when its message is deleted: no results post, the end job and any queued
    // live render are dropped.
    if (poll.status === 'ACTIVE') await svc.endPoll(client, poll.id, { reason: 'deleted', guildId: guild.id });

    let note = '';
    if (poll.messageId) {
      try {
        const channel = guild.channels.cache.get(poll.channelId) ?? await guild.channels.fetch(poll.channelId);
        markBotDeletion(poll.messageId, 'polls');
        await channel.messages.delete(poll.messageId);
      } catch (err) {
        if (GONE_CODES.has(err?.code)) {
          // Already gone: nothing to delete.
        } else if (NO_ACCESS_CODES.has(err?.code)) {
          note = ' I could not delete its message (I am missing permissions there), so delete that by hand.';
        } else {
          logger.warn(`dashboard poll ${poll.id}: could not delete the message`, err);
          note = ' I could not delete its message, so delete that by hand.';
        }
      }
    }
    await db.poll.deleteMany({ where: { id: poll.id, guildId: guild.id } });
    return { message: `Deleted the poll "${clampText(poll.question, 80)}" and its votes.${note}`, data: { pollId: poll.id } };
  }

  return {
    'poll.start': { run: start },
    'poll.end': { run: end },
    'poll.delete': { run: remove },
  };
}

export const tasks = Object.freeze(createPollTasks());
