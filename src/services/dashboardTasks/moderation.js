// Moderation actions the web dashboard asks the bot to do (its `cases` list, src/dashboard/collections/cases.js). Each task
// does what the matching slash command does, with the dashboard user as the moderator:
//   mod.action          /warn /timeout /unmute /kick /softban /ban /unban → executeModAction / executeUnmute / executeUnban
//                       (source 'dashboard': the moderator's roles, immunity and self-moderation are checked like a
//                       command; DM per modDmTargets, case log, escalation, scheduled unban / expiry)
//   mod.editCase        /case reason | proof (and its case-log note)
//   mod.clearWarnings   /case clear-history (administrators): pardons the active warnings and records a NOTE case
// modVisibility only decides where a command's confirmation goes; the dashboard shows its own message and posts nothing in
// a chat channel. Commands that are refused while the Moderation module is off are refused here too.
import prisma from '../../database/client.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe } from '../settingsService.js';
import {
  DELETE_MESSAGES_MAX_S, MSG, REASON_MAX, TIER_ACTION_LABEL, createInfraction, executeModAction, executeUnban,
  executeUnmute, postModLog,
} from '../moderationService.js';
import { isHttpUrl } from '../../utils/customEmbed.js';
import { neutralEmbed } from '../../utils/embedBuilder.js';
import logger from '../../utils/logger.js';
import { formatDurationLong } from '../../utils/parseDuration.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import { clampText } from '../../utils/text.js';
import { TaskError } from './shared.js';

/** What `mod.action` accepts (the dashboard's case form). */
export const DASHBOARD_MOD_ACTIONS = Object.freeze(['WARN', 'TIMEOUT', 'UNMUTE', 'KICK', 'SOFTBAN', 'BAN', 'UNBAN']);
/** The action commands' `reason` option is at most 512 characters; /case reason allows REASON_MAX. */
export const COMMAND_REASON_MAX = 512;
const PROOF_ACTIONS = new Set(['WARN', 'TIMEOUT', 'KICK', 'SOFTBAN', 'BAN']);
const MAX_CASE_ID = 2_147_483_647;
const DAY_S = 86_400;
const SNOWFLAKE_RE = /^\d{17,20}$/;

const isObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);

/** Optional text: null/undefined/blank → null; else trimmed, at most `max` characters. */
function optionalText(value, max, what) {
  if (value === null || value === undefined) return null;
  if (typeof value !== 'string') throw new TaskError(`${what} must be text.`);
  const text = value.trim();
  if (!text) return null;
  if (text.length > max) throw new TaskError(`${what} can be at most ${max} characters.`);
  return text;
}

function userIdOf(payload) {
  const id = String(payload?.userId ?? '');
  if (!SNOWFLAKE_RE.test(id)) throw new TaskError('Pick a member, or paste a user ID.');
  return id;
}

/**
 * Re-validates a `mod.action` payload (the dashboard already did; never trust it).
 * @returns {{ action: string, userId: string, reason: string|null, durationMs: number|null,
 *   deleteMessageSeconds: number|null, proofUrl: string|null }}
 * @throws {TaskError}
 */
export function readActionPayload(payload) {
  if (!isObject(payload)) throw new TaskError('The request was malformed.');
  const { action } = payload;
  if (!DASHBOARD_MOD_ACTIONS.includes(action)) throw new TaskError('Unknown moderation action.');
  const userId = userIdOf(payload);
  const reason = optionalText(payload.reason, COMMAND_REASON_MAX, 'The reason');
  if (action === 'WARN' && !reason) throw new TaskError('A warning needs a reason.');

  let durationMs = null;
  if (action === 'TIMEOUT' || action === 'BAN') {
    const d = payload.durationMs;
    if (d !== null && d !== undefined && !(Number.isSafeInteger(d) && d > 0)) throw new TaskError('Invalid duration.');
    durationMs = d ?? null;
    if (action === 'TIMEOUT' && durationMs === null) throw new TaskError('Timeouts need a duration.');
  }

  let deleteMessageSeconds = null;
  if (action === 'BAN' || action === 'SOFTBAN') {
    const s = payload.deleteMessageSeconds ?? (action === 'BAN' ? 0 : DELETE_MESSAGES_MAX_S);
    const min = action === 'SOFTBAN' ? DAY_S : 0;
    if (!Number.isInteger(s) || s < min || s > DELETE_MESSAGES_MAX_S) {
      throw new TaskError(`Message deletion must be between ${min / DAY_S} and 7 days.`);
    }
    deleteMessageSeconds = s;
  }

  let proofUrl = null;
  if (PROOF_ACTIONS.has(action) && payload.proofUrl !== null && payload.proofUrl !== undefined) {
    proofUrl = typeof payload.proofUrl === 'string' ? payload.proofUrl.trim() : '';
    if (!isHttpUrl(proofUrl) || proofUrl.length > REASON_MAX) throw new TaskError(MSG.BAD_PROOF);
  }
  return { action, userId, reason, durationMs, deleteMessageSeconds, proofUrl };
}

/**
 * The checks the command router makes before a moderation command runs, for the dashboard user: still a member (the
 * moderation rules compare their roles), the module on (the commands are gated), and their permission level.
 * @returns {Promise<{ settings: object, member: import('discord.js').GuildMember }>}
 * @throws {TaskError}
 */
async function moderatorChecks(deps, guild, actor, level) {
  if (!actor?.member) {
    throw new TaskError(actor?.isBotOwner
      ? 'Join this server to moderate it: the bot compares your roles with the member\'s, as it does for the slash commands.'
      : 'You are no longer a member of this server.');
  }
  const settings = await deps.getSettingsSafe(guild.id);
  if (!settings) throw new TaskError("The bot couldn't load this server's settings. Try again in a moment.");
  if (!isModuleEnabled(settings, 'moderation')) throw new TaskError('The Moderation module is off. Turn it on to do that.');
  if (resolvePermissionLevel(actor.member, settings) < level) {
    throw new TaskError(level >= PERMISSION_LEVELS.ADMIN
      ? 'Only administrators (Manage Server) can do that.'
      : 'You need moderator rights in this server (Kick, Ban or Timeout Members, or a moderator role) to do that.');
  }
  return { settings, member: actor.member };
}

/** A user's name for a message: their server nickname, global name or username; the id when unknown. */
async function nameOf(guild, client, userId) {
  const member = guild.members?.cache?.get(userId) ?? null;
  if (member?.displayName) return member.displayName;
  const user = client?.users?.cache?.get(userId) ?? await client?.users?.fetch?.(userId).catch(() => null);
  return user?.globalName || user?.username || userId;
}

const caseRef = (inf) => `case #${inf.caseId}`;

/** The confirmation of a successful action, like the command's reply (title, case, duration, warnings). */
function actionMessage(input, res, name, { activeWarns = null } = {}) {
  const inf = res.infraction;
  const extra = [];
  let main;
  switch (input.action) {
    case 'WARN':
      main = `Warned ${name} (${caseRef(inf)}).`;
      if (activeWarns !== null) extra.push(`Active warnings: ${activeWarns}.`);
      if (res.escalation?.ok) {
        extra.push(`Escalation T${res.escalation.tierId}: ${TIER_ACTION_LABEL[res.escalation.action] ?? res.escalation.action} applied (case #${res.escalation.caseId}).`);
      }
      break;
    case 'TIMEOUT':
      main = res.action === 'MUTE'
        ? `Muted ${name} for ${formatDurationLong(input.durationMs)} (${caseRef(inf)}).`
        : `Timed out ${name} for ${formatDurationLong(input.durationMs)} (${caseRef(inf)}).`;
      break;
    case 'UNMUTE':
      main = `Unmuted ${name} (${caseRef(inf)}). Removed: ${res.removed?.join(' and ') || 'nothing (records only)'}.`;
      break;
    case 'KICK':
      main = `Kicked ${name} (${caseRef(inf)}).`;
      break;
    case 'SOFTBAN':
      main = inf.type === 'BAN'
        ? `Banned ${name}, but the softban's automatic unban failed (${caseRef(inf)}).`
        : `Softbanned ${name} (${caseRef(inf)}). Deleted ${input.deleteMessageSeconds / DAY_S} day(s) of messages.`;
      break;
    case 'BAN':
      main = res.action === 'TEMPBAN'
        ? `Banned ${name} for ${formatDurationLong(input.durationMs)} (${caseRef(inf)}).`
        : `Banned ${name} (${caseRef(inf)}).`;
      if (input.deleteMessageSeconds > 0) extra.push(`Deleted ${input.deleteMessageSeconds / DAY_S} day(s) of messages.`);
      break;
    case 'UNBAN':
      main = `Unbanned ${name} (${caseRef(inf)}).`;
      if (res.cleared > 1) extra.push(`Ban records closed: ${res.cleared}.`);
      break;
    default:
      main = `Done (${caseRef(inf)}).`;
  }
  const warnings = [...(res.dm === 'failed' ? ["I couldn't DM the user."] : []), ...(res.warnings ?? [])];
  if (res.note) warnings.push(res.note);
  if (res.escalation && !res.escalation.ok) {
    const e = res.escalation;
    warnings.push(`Escalation T${e.tierId} (${TIER_ACTION_LABEL[e.action] ?? e.action}) could not be applied: ${clampText(e.error ?? 'unknown error', 200)}`);
  }
  return [main, ...extra, ...warnings].join(' ');
}

/**
 * The moderation tasks, with their dependencies injectable for tests.
 * @param {object} [overrides] test doubles of the services used
 */
export function createModerationTasks(overrides = {}) {
  const deps = {
    prisma, getSettingsSafe, executeModAction, executeUnban, executeUnmute, createInfraction, postModLog, ...overrides,
  };

  /** mod.action: `{ action, userId, reason, durationMs, deleteMessageSeconds, proofUrl }` → `{ caseId, action, userId }`. */
  async function modAction({ guild, client, actor, payload }) {
    const input = readActionPayload(payload);
    const { member } = await moderatorChecks(deps, guild, actor, PERMISSION_LEVELS.MODERATOR);
    const moderator = member.user ?? { id: member.id };
    const name = await nameOf(guild, client, input.userId);
    let res;
    if (input.action === 'UNBAN') {
      res = await deps.executeUnban({ guild, userId: input.userId, moderator, reason: input.reason, source: 'dashboard' });
    } else if (input.action === 'UNMUTE') {
      res = await deps.executeUnmute({ guild, target: { id: input.userId }, moderator, reason: input.reason, source: 'dashboard' });
    } else {
      res = await deps.executeModAction({
        guild,
        target: { id: input.userId },
        moderator,
        action: input.action === 'BAN' && input.durationMs ? 'TEMPBAN' : input.action,
        reason: input.reason,
        durationMs: input.durationMs,
        deleteMessageSeconds: input.deleteMessageSeconds,
        proofUrl: input.proofUrl,
        source: 'dashboard',
      });
    }
    if (!res?.ok) throw new TaskError(res?.error || MSG.INTERNAL);

    let activeWarns = null;
    if (input.action === 'WARN') {
      // The count follows the action, so it includes this warning (and ignores pardoned ones), as /warn shows it.
      activeWarns = await deps.prisma.infraction.count({
        where: { guildId: guild.id, targetUserId: input.userId, type: 'WARN', active: true },
      }).catch((err) => {
        logger.warn(`Dashboard mod.action: could not count active warnings: ${err?.message ?? err}`);
        return null;
      });
    }
    return {
      message: actionMessage(input, res, name, { activeWarns }),
      data: { caseId: res.infraction.caseId, action: res.action ?? input.action, userId: input.userId },
    };
  }

  /** mod.editCase: `{ caseId, reason?, proofUrl? }` → `{ caseId }`, with a case-log note per change (/case). */
  async function editCase({ guild, actor, payload }) {
    if (!isObject(payload)) throw new TaskError('The request was malformed.');
    const caseId = payload.caseId;
    if (!Number.isInteger(caseId) || caseId < 1 || caseId > MAX_CASE_ID) throw new TaskError('That case does not exist.');
    const data = {};
    if (payload.reason !== undefined) {
      const reason = clampText(typeof payload.reason === 'string' ? payload.reason.trim() : '', REASON_MAX);
      if (!reason) throw new TaskError("The reason can't be empty.");
      data.reason = reason;
    }
    if (payload.proofUrl !== undefined) {
      const url = typeof payload.proofUrl === 'string' ? payload.proofUrl.trim() : '';
      if (!isHttpUrl(url) || url.length > REASON_MAX) throw new TaskError(MSG.BAD_PROOF);
      data.proofUrl = url;
    }
    if (!Object.keys(data).length) throw new TaskError('Nothing to change.');
    const { member } = await moderatorChecks(deps, guild, actor, PERMISSION_LEVELS.MODERATOR);

    try {
      await deps.prisma.infraction.update({ where: { guildId_caseId: { guildId: guild.id, caseId } }, data });
    } catch (err) {
      if (err?.code === 'P2025') throw new TaskError(`Case #${caseId} doesn't exist in this server.`);
      throw err;
    }
    const quote = (text) => String(text).replace(/\n/g, '\n> ');
    const notes = [];
    if (data.reason) notes.push(`Reason changed by <@${member.id}> on the dashboard:\n> ${quote(data.reason)}`);
    if (data.proofUrl) notes.push(`Proof link changed by <@${member.id}> on the dashboard:\n${data.proofUrl}`);
    for (const text of notes) {
      try {
        await deps.postModLog(guild, neutralEmbed(`📝 Case #${caseId} edited`, clampText(text, 4000)));
      } catch (err) {
        logger.warn(`Dashboard mod.editCase: case log note failed: ${err?.message ?? err}`);
      }
    }
    return { message: `Case #${caseId} updated.`, data: { caseId } };
  }

  /** mod.clearWarnings: `{ userId, reason }` → `{ cleared, caseId }` (/case clear-history, administrators only). */
  async function clearWarnings({ guild, client, actor, payload }) {
    if (!isObject(payload)) throw new TaskError('The request was malformed.');
    const userId = userIdOf(payload);
    const reason = optionalText(payload.reason, COMMAND_REASON_MAX, 'The reason');
    const { member } = await moderatorChecks(deps, guild, actor, PERMISSION_LEVELS.ADMIN);
    const name = await nameOf(guild, client, userId);

    const activeWarns = { guildId: guild.id, targetUserId: userId, type: 'WARN', active: true };
    if (await deps.prisma.infraction.count({ where: activeWarns }) === 0) throw new TaskError(`${name} has no active warnings.`);
    const { count } = await deps.prisma.infraction.updateMany({ where: activeWarns, data: { active: false } });
    const noteReason = clampText(`[Pardon] Cleared ${count} warning(s): ${reason || 'No reason provided'}`, REASON_MAX);

    // The pardon is the updateMany above; the NOTE only records it as a case (and posts the case log).
    let note = null;
    try {
      const target = client?.users?.cache?.get(userId) ?? await client?.users?.fetch?.(userId).catch(() => null) ?? { id: userId };
      note = await deps.createInfraction(guild, target, member.user ?? { id: member.id }, 'NOTE', {
        reason: noteReason, silent: true, skipEscalation: true, client, source: 'dashboard',
      });
    } catch (err) {
      logger.error(`Dashboard mod.clearWarnings: could not record the pardon note in ${guild.id}`, err);
    }
    const message = note
      ? `Cleared ${count} warning(s) for ${name} (case #${note.caseId}).`
      : `Cleared ${count} warning(s) for ${name}, but the pardon could not be saved as a case.`;
    return { message, data: { cleared: count, caseId: note?.caseId ?? null, userId } };
  }

  return Object.freeze({
    'mod.action': Object.freeze({ run: modAction }),
    'mod.editCase': Object.freeze({ run: editCase }),
    'mod.clearWarnings': Object.freeze({ run: clearWarnings }),
  });
}

export const tasks = createModerationTasks();
