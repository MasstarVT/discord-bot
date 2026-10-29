// Dashboard tasks of the Ticketing module, run in the bot (src/services/dashboardTasks/index.js). The dashboard saves a panel
// in the database; these tasks do the part that needs the gateway client, with the same service functions and permission
// checks as /ticket-panel publish | categories | transcripts | managers | delete:
//   ticket.check    { categoryIds, transcriptChannelId? }          the bot's permissions in categories and a transcript channel
//   ticket.publish  { panelId, channelId }                          post the panel message, or move / update it (publishPanel)
//   ticket.refresh  { panelId, message?, managersAdded?, managersRemoved? }
//                   update the posted message (refreshPanelMessage) and merge the changed manager roles into the guild's one
//                   ticket.managersync job, which back-fills existing tickets (ticketService.runManagerSyncJob), so the task stays short
//   ticket.delete   { panelId }                                     delete the panel and its message (deletePanel)
// Every task re-checks that the dashboard user is still an admin (the level /ticket-panel needs) and re-validates its payload.
import { ChannelType } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { checkChannelUsable, missingBotPermissions, permissionNames } from '../../utils/interactionHelpers.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import { clampText } from '../../utils/text.js';
import logger from '../../utils/logger.js';
import { scheduleJob } from '../scheduler.js';
import { getSettingsSafe } from '../settingsService.js';
import {
  OPEN_REQUIRED_PERMS, deletePanel, describeDiscordError, findPanel, invalidatePanel, liveManagerRoleIds, managerSyncPayload,
  mergeManagerSync, publishPanel, refreshPanelMessage,
} from '../ticketService.js';
import { TaskError } from './shared.js';

const ID_RE = /^[A-Za-z0-9_-]{1,40}$/;
const SNOWFLAKE_RE = /^\d{17,20}$/;
const POSTABLE = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
/** What the bot needs in a ticket category (the /ticket-panel categories and option add checks). */
const CATEGORY_PERMS = ['ViewChannel', 'ManageChannels', 'ManageRoles'];
/** What the bot needs to post transcripts (the /ticket-panel transcripts check). */
const TRANSCRIPT_PERMS = ['ViewChannel', 'SendMessages', 'EmbedLinks', 'AttachFiles'];
const MAX_CATEGORIES = LIMITS.TICKET_PANEL_OPTIONS + 3;

// ── Shared checks ────────────────────────────────────────────────────────────

/** The settings, after checking the dashboard user may still change ticket panels (the Admin level /ticket-panel needs). */
async function authorize(guild, actor) {
  if (actor?.isBotOwner !== true && !actor?.member) throw new TaskError('You are no longer a member of this server.');
  const settings = await getSettingsSafe(guild.id);
  if (actor.isBotOwner === true) return settings;
  if (resolvePermissionLevel(actor.member, settings) < PERMISSION_LEVELS.ADMIN) {
    throw new TaskError('You need the Admin level (Manage Server) to change ticket panels.');
  }
  return settings;
}

async function loadPanel(guild, payload) {
  const id = payload?.panelId;
  if (typeof id !== 'string' || !ID_RE.test(id)) throw new TaskError('That panel is not valid.');
  const panel = await findPanel(guild.id, id);
  if (!panel || panel.id !== id) throw new TaskError('That ticket panel no longer exists.');
  return panel;
}

function snowflake(value, what) {
  if (typeof value !== 'string' || !SNOWFLAKE_RE.test(value)) throw new TaskError(`${what} is not valid.`);
  return value;
}

async function channelOf(guild, id) {
  const channel = guild.channels.cache.get(id) ?? await guild.channels.fetch(id).catch(() => null);
  if (!channel) throw new TaskError('That channel does not exist in this server any more. Pick another one.');
  return channel;
}

/** A usable-channel failure as one sentence. */
function unusable(channel, check) {
  return check.missing.length
    ? `I need ${permissionNames(check.missing).join(', ')} in #${channel.name}.`
    : `I can't use #${channel.name}: ${check.reason}.`;
}

// ── ticket.check ─────────────────────────────────────────────────────────────

async function check({ guild, actor, payload }) {
  await authorize(guild, actor);
  const ids = Array.isArray(payload?.categoryIds) ? payload.categoryIds.slice(0, MAX_CATEGORIES) : [];
  for (const raw of [...new Set(ids)]) {
    const channel = await channelOf(guild, snowflake(raw, 'A category'));
    if (channel.type !== ChannelType.GuildCategory) throw new TaskError(`#${channel.name} is not a category.`);
    const missing = missingBotPermissions(channel, CATEGORY_PERMS);
    if (missing.length) throw new TaskError(`I need ${permissionNames(missing).join(', ')} in the "${channel.name}" category.`);
  }
  if (payload?.transcriptChannelId !== null && payload?.transcriptChannelId !== undefined) {
    const channel = await channelOf(guild, snowflake(payload.transcriptChannelId, 'The transcript channel'));
    if (!POSTABLE.includes(channel.type)) throw new TaskError('Transcripts can only be posted in text or announcement channels.');
    const usable = checkChannelUsable(channel, { needs: TRANSCRIPT_PERMS });
    if (!usable.ok) throw new TaskError(unusable(channel, usable));
  }
  return { message: 'The bot can use those channels.' };
}

// ── ticket.publish ───────────────────────────────────────────────────────────

/** The warnings that follow a successful publish: no manager roles, and an open category the bot cannot use. */
function publishWarnings(guild, panel) {
  const lines = [];
  if (liveManagerRoleIds(panel, guild).length === 0) {
    lines.push('This panel has no ticket manager roles, so only moderators and admins can see its tickets.');
  }
  const open = panel.openCategoryId ? guild.channels.cache.get(panel.openCategoryId) : null;
  if (open && missingBotPermissions(open, CATEGORY_PERMS).length) {
    lines.push(`I can't manage channels in the "${open.name}" category, so tickets would fail to open there.`);
  }
  return lines;
}

async function publish({ guild, actor, payload }) {
  const settings = await authorize(guild, actor);
  const panel = await loadPanel(guild, payload);
  const channel = await channelOf(guild, snowflake(payload?.channelId, 'That channel'));
  if (!POSTABLE.includes(channel.type)) throw new TaskError('Panels can only be posted in text or announcement channels.');
  const usable = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks'] });
  if (!usable.ok) throw new TaskError(unusable(channel, usable));
  const missing = missingBotPermissions(guild, OPEN_REQUIRED_PERMS);
  if (missing.length) {
    throw new TaskError(`To open tickets I need ${permissionNames(missing).join(', ')} on this server. Fix my role, then publish again.`);
  }

  invalidatePanel(panel.id); // the bot's own memo of the panel may predate the dashboard's changes
  const r = await publishPanel({ guild, panelId: panel.id, channel, settings });
  if (!r.ok) {
    throw new TaskError(`I couldn't post the panel: ${r.code === 'PANEL_GONE' ? 'the panel was deleted' : describeDiscordError(r.error)}.`);
  }
  const lead = `${r.mode === 'edited' ? 'Updated' : 'Posted'} "${clampText(panel.name, 80)}" in #${channel.name}.`;
  return {
    message: [lead, ...publishWarnings(guild, panel)].join(' '),
    data: { panelId: panel.id, channelId: channel.id, messageId: r.message.id, url: r.message.url, mode: r.mode },
  };
}

// ── ticket.refresh ───────────────────────────────────────────────────────────

/** The sentence for a failed refreshPanelMessage (refreshNote without the Discord markup). */
function refreshFailure(r, guild) {
  if (r.code === 'GONE') return 'The published panel message was deleted. Use "Publish" to post it again.';
  if (r.code === 'NO_ACCESS') {
    const name = guild.channels.cache.get(r.channelId)?.name;
    return `I couldn't update the published panel${name ? ` in #${name}` : ''} (missing access).`;
  }
  return `I couldn't update the published panel (${r.reason ?? 'unknown error'}).`;
}

/** A list of role ids from the payload, once each. */
function roleList(value) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) throw new TaskError('That request is not valid.');
  return [...new Set(value.slice(0, LIMITS.TICKET_MANAGER_ROLES).map((id) => snowflake(id, 'A role')))];
}

async function refresh({ guild, actor, payload }) {
  await authorize(guild, actor);
  const panel = await loadPanel(guild, payload);
  const added = roleList(payload?.managersAdded).filter((id) => panel.managerRoleIds.includes(id) && guild.roles.cache.has(id));
  const removed = roleList(payload?.managersRemoved).filter((id) => !panel.managerRoleIds.includes(id));
  invalidatePanel(panel.id);

  const parts = [];
  let failure = null;
  if (payload?.message !== false) {
    const r = await refreshPanelMessage(guild, panel.id);
    if (!r.ok) failure = refreshFailure(r, guild);
    else if (r.skipped) {
      if (added.length + removed.length === 0) {
        throw new TaskError('This panel is not published, so there is no message to update. Use "Publish" first.');
      }
      parts.push('The panel is not published, so there is no message to update.');
    } else parts.push('Updated the published panel message.');
  }

  // Existing tickets get the manager roles too (the newest 50), like /ticket-panel managers add | remove. That is up to 50
  // sequential REST calls per role, so the guild's one ticket.managersync job does it: a task holds an interactive scheduler
  // slot and the dashboard waits only TASK_WAIT_MS for it. The change is merged into that job (never a second copy, and a
  // running one is not reset); the job decides add or remove per role from the panel's managers when it runs.
  if (added.length + removed.length > 0) {
    try {
      await scheduleJob({
        guildId: guild.id, type: 'ticket.managersync', runAt: new Date(), dedupeKey: `ticket.managersync:${guild.id}`,
        payload: managerSyncPayload(panel.id, [...added, ...removed]), maxAttempts: 3, onConflict: 'merge', merge: mergeManagerSync,
      });
      parts.push('Existing tickets are being updated in the background (the 50 newest).');
    } catch (err) {
      logger.error(`Dashboard: could not queue the manager sync of panel ${panel.id}`, err);
      parts.push("I couldn't queue the update of existing tickets. Try saving the managers again.");
    }
  }
  if (failure) throw new TaskError([failure, ...parts].join(' '));
  return { message: parts.join(' ') || 'Done.', data: { panelId: panel.id } };
}

// ── ticket.delete ────────────────────────────────────────────────────────────

async function remove({ guild, actor, payload }) {
  await authorize(guild, actor);
  const panel = await loadPanel(guild, payload);
  const { open } = await deletePanel(guild, panel);
  const tail = open ? ` ${open} ticket(s) keep their channels; only moderators and admins can manage them now.` : '';
  const withMessage = panel.channelId && panel.messageId ? ' and its published message' : '';
  return { message: `Deleted "${clampText(panel.name, 80)}"${withMessage}.${tail}`, data: { panelId: panel.id } };
}

export const tasks = Object.freeze({
  'ticket.check': { run: check },
  'ticket.publish': { run: publish },
  'ticket.refresh': { run: refresh },
  'ticket.delete': { run: remove },
});
