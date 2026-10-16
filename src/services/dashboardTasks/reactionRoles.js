// Dashboard tasks of the Reaction Roles module, run in the bot (src/services/dashboardTasks/index.js). The dashboard saves a
// panel in the database; these tasks do the part that needs the gateway client, with the same service functions and
// permission checks as /reactionrole create | add | remove | edit | repost | delete:
//   rr.publish  { groupId, mode: 'post' | 'sync', removedEmojis? }  post a panel that is not posted yet, or bring the posted
//               message (and the bot's reactions) up to date with the saved panel
//   rr.repost   { groupId, channelId? }                              post the panel again and clean up the old message
//   rr.delete   { groupId }                                          delete the panel and its message
//   rr.timezoneTemplate { groupId, template }                        create or reuse a timezone template's roles, link them to
//               their zones, add them to the (empty) panel and post it
// Every task re-checks that the dashboard user is still an admin (the level /reactionrole needs) and re-validates its payload.
import { ChannelType, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { checkChannelUsable, permissionNames } from '../../utils/interactionHelpers.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import { markBotDeletion } from '../../utils/deletionTracker.js';
import { clampText } from '../../utils/text.js';
import logger from '../../utils/logger.js';
import { getSettingsSafe } from '../settingsService.js';
import * as rr from '../reactionRoleService.js';
import { setupTimezoneRoles, describeSetup, TimezoneSetupError } from '../timezoneRoleSetup.js';
import { TIMEZONE_ROLE_TEMPLATES } from '../../utils/timezones.js';
import { TaskError } from './shared.js';

const ID_RE = /^[A-Za-z0-9_-]{1,40}$/;
const SNOWFLAKE_RE = /^\d{17,20}$/;
/** The bot's message of a panel that has not been posted yet (reactionRoles.store.js writes it). */
const UNPOSTED = '0';
const PANEL_CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const MAX_REMOVED_EMOJIS = 25;

// ── Shared checks ────────────────────────────────────────────────────────────

/** The settings, after checking the dashboard user may still change panels (the Admin level /reactionrole needs). */
async function authorize(guild, actor) {
  if (actor?.isBotOwner !== true && !actor?.member) throw new TaskError('You are no longer a member of this server.');
  const settings = await getSettingsSafe(guild.id);
  if (actor.isBotOwner === true) return settings;
  if (resolvePermissionLevel(actor.member, settings) < PERMISSION_LEVELS.ADMIN) {
    throw new TaskError('You need the Admin level (Manage Server) to change reaction role panels.');
  }
  return settings;
}

function groupIdOf(payload) {
  const id = payload?.groupId;
  if (typeof id !== 'string' || !ID_RE.test(id)) throw new TaskError('That panel is not valid.');
  return id;
}

async function loadGroup(guild, payload) {
  const group = await rr.getPanel(guild.id, groupIdOf(payload));
  if (!group) throw new TaskError('That panel no longer exists.');
  return group;
}

/** A channel to post in: text or announcement, of this server. */
async function postChannel(guild, id, { strictType }) {
  const channel = guild.channels.cache.get(id) ?? await guild.channels.fetch(id).catch(() => null);
  if (!channel) throw new TaskError('That channel does not exist in this server any more. Pick another one.');
  if (strictType && !PANEL_CHANNEL_TYPES.includes(channel.type)) {
    throw new TaskError('Panels can only be posted in text or announcement channels.');
  }
  return channel;
}

/** What the bot needs in the channel to post this panel (the same list /reactionrole create and repost use). */
const needsFor = (group) => [
  'ViewChannel', 'SendMessages', ...(group.plainText ? [] : ['EmbedLinks']),
  ...(group.layout === 'REACTIONS' ? ['AddReactions', 'ReadMessageHistory'] : []),
];

function requireUsable(channel, needs) {
  const check = checkChannelUsable(channel, { needs });
  if (check.ok) return;
  throw new TaskError(check.missing.length
    ? `I need ${permissionNames(check.missing).join(', ')} in #${channel?.name ?? 'that channel'}.`
    : `I can't post in #${channel?.name ?? 'that channel'}: ${check.reason}.`);
}

const lacksManageMessages = (channel, guild) => (
  channel?.permissionsFor?.(guild.members.me)?.has(PermissionFlagsBits.ManageMessages) !== true
);
const title = (group) => clampText(group.title, 100);

/** The text of the reaction failures of ensurePanelReactions, or null. */
function reactionFailures(res) {
  if (!res?.failed?.length) return null;
  return res.failed.slice(0, 5).map((f) => `I couldn't add the ${f.emoji} reaction: ${f.reason}.`).join(' ');
}

// ── Posting ──────────────────────────────────────────────────────────────────

/**
 * Posts the panel as a new message in `channel` and points the panel at it (the shared part of create and repost).
 * @returns {Promise<{ sent: import('discord.js').Message, warnings: string[] }>}
 */
async function postNew({ guild, group, channel, settings }) {
  requireUsable(channel, needsFor(group));
  let sent;
  try {
    sent = await channel.send(rr.buildPanelPayload({ ...group, attached: false }, { color: settings?.embedColor }));
  } catch (err) {
    throw new TaskError(`I couldn't post in #${channel.name}: ${rr.describeDiscordError(err)}.`);
  }
  let saved;
  try {
    saved = await rr.movePanel(guild.id, group.id, { channelId: channel.id, messageId: sent.id, attached: false });
  } catch (err) {
    logger.error('dashboard rr.publish failed to save the panel', err);
    saved = null;
  }
  if (!saved?.count) {
    markBotDeletion(sent.id, 'reaction-roles:rollback');
    await sent.delete().catch(() => {});
    throw new TaskError(saved
      ? 'That panel was deleted while it was being posted.'
      : 'I could not save the new message, so I removed it. Please try again.');
  }
  await rr.invalidatePanelIndex(guild.id);

  const warnings = [];
  if (group.layout === 'REACTIONS') {
    const failed = reactionFailures(await rr.ensurePanelReactions(sent, rr.indexItems(group)));
    if (failed) warnings.push(failed);
  }
  if (group.type === 'UNIQUE' && group.layout === 'REACTIONS' && lacksManageMessages(channel, guild)) {
    warnings.push("I don't have Manage Messages there, so I can't remove a member's old reaction when they pick another role.");
  }
  return { sent, warnings };
}

const done = (text, warnings, data) => ({ message: [text, ...warnings].join(' '), data });

// ── rr.publish ───────────────────────────────────────────────────────────────

/** Emojis to take off the message: the payload's list, minus any emoji a role on the panel still uses. */
function emojisToRemove(group, payload) {
  const list = Array.isArray(payload?.removedEmojis) ? payload.removedEmojis : [];
  const used = new Set(group.items.map((i) => rr.emojiKeyOfStored(i.emoji)).filter(Boolean));
  const out = [];
  for (const emoji of list.slice(0, MAX_REMOVED_EMOJIS)) {
    if (typeof emoji !== 'string' || emoji.length > 100) continue;
    const key = rr.emojiKeyOfStored(emoji);
    if (key && !used.has(key) && !out.includes(emoji)) out.push(emoji);
  }
  return out;
}

async function publish({ guild, client, actor, payload }) {
  const settings = await authorize(guild, actor);
  if (payload?.mode !== 'post' && payload?.mode !== 'sync') throw new TaskError('That request is not valid.');
  const group = await loadGroup(guild, payload);

  // A panel that was never posted is posted now (both modes: saving again retries a failed post).
  if (group.messageId === UNPOSTED) {
    const channel = await postChannel(guild, group.channelId, { strictType: true });
    const { sent, warnings } = await postNew({ guild, group, channel, settings });
    return done(`Posted "${title(group)}" in #${channel.name}.`, warnings, {
      groupId: group.id, channelId: channel.id, messageId: sent.id, url: sent.url,
    });
  }

  // Bring the posted message up to date (add / remove / edit). The index goes first, so our own reaction hooks never see
  // a half-updated panel.
  await rr.invalidatePanelIndex(guild.id);
  if (!group.attached) {
    const sync = await rr.syncPanel(guild, group, { settings });
    if (sync.status === 'missing') throw new TaskError('The panel message was deleted in Discord. Use "Repost" to post it again.');
    if (sync.status === 'failed') throw new TaskError(`I couldn't update the panel message: ${sync.reason}.`);
  }
  const notes = [];
  if (group.layout === 'REACTIONS') {
    const fm = await rr.fetchPanelMessage(guild, group);
    if (fm.status === 'ok') {
      for (const emoji of emojisToRemove(group, payload)) await rr.removePanelReaction(fm.message, emoji, { client }); // cosmetic
      const failed = reactionFailures(await rr.ensurePanelReactions(fm.message, rr.indexItems(group)));
      if (failed) throw new TaskError(failed);
    } else if (group.attached || fm.status === 'missing') {
      throw new TaskError(fm.status === 'missing'
        ? 'The panel message was deleted in Discord. Use "Repost" to post it again.'
        : `I couldn't reach the panel message: ${fm.reason}.`);
    }
    if (group.type === 'UNIQUE' && lacksManageMessages(guild.channels.cache.get(group.channelId), guild)) {
      notes.push("I don't have Manage Messages in that channel, so I can't remove a member's old reaction when they pick another role.");
    }
  }
  await rr.invalidatePanelIndex(guild.id);
  const what = group.attached ? 'the reactions on' : 'the panel message of';
  return done(`Updated ${what} "${title(group)}".`, notes, { groupId: group.id });
}

// ── rr.repost ────────────────────────────────────────────────────────────────

async function repost({ guild, client, actor, payload }) {
  const settings = await authorize(guild, actor);
  const group = await loadGroup(guild, payload);
  let channelId = null;
  if (payload?.channelId !== null && payload?.channelId !== undefined) {
    if (typeof payload.channelId !== 'string' || !SNOWFLAKE_RE.test(payload.channelId)) throw new TaskError('That channel is not valid.');
    channelId = payload.channelId;
  }

  const target = channelId
    ? await postChannel(guild, channelId, { strictType: true })
    : (guild.channels.cache.get(group.channelId) ?? await guild.channels.fetch(group.channelId).catch(() => null));
  if (!target) throw new TaskError("The panel's channel no longer exists. Pick a channel to post it in.");

  const unposted = group.messageId === UNPOSTED;
  const restoreOnly = group.attached && !channelId;
  let oldMessage = null;
  if (restoreOnly) {
    const fm = await rr.fetchPanelMessage(guild, group);
    if (fm.status === 'ok') {
      const res = await rr.ensurePanelReactions(fm.message, rr.indexItems(group));
      return done('Put the role reactions back on the attached message.', [reactionFailures(res)].filter(Boolean), { groupId: group.id });
    }
    if (fm.status === 'failed') throw new TaskError(`I couldn't reach the attached message: ${fm.reason}.`);
    // The attached message is gone: the panel becomes a message the bot posts below.
  }

  // Look the old message up before the panel moves: a fetch that finds nothing flags the panel as missing.
  if (!restoreOnly && !unposted) oldMessage = await rr.fetchPanelMessage(guild, group);

  const { sent, warnings } = await postNew({ guild, group, channel: target, settings });

  if (oldMessage?.status === 'ok' && oldMessage.message.id !== sent.id) {
    if (!group.attached) {
      markBotDeletion(oldMessage.message.id, 'reaction-roles:repost');
      await oldMessage.message.delete().catch(() => {});
    } else {
      await rr.removeOwnReactions(oldMessage.message, rr.indexItems(group), client); // its content is never touched
    }
  }
  const attachedNote = group.attached
    ? 'It is a message the bot posted now, so the dashboard can change its text. The original message was left alone.' : '';
  return done(`"${title(group)}" is now in #${target.name}.`, [attachedNote, ...warnings].filter(Boolean), {
    groupId: group.id, channelId: target.id, messageId: sent.id, url: sent.url,
  });
}

// ── rr.delete ────────────────────────────────────────────────────────────────

async function remove({ guild, client, actor, payload }) {
  await authorize(guild, actor);
  const group = await loadGroup(guild, payload);
  const name = title(group);

  await rr.deletePanel(guild.id, group.id);
  await rr.invalidatePanelIndex(guild.id);

  if (group.messageId === UNPOSTED) return done(`Deleted "${name}".`, [], { groupId: group.id });
  const fm = await rr.fetchPanelMessage(guild, group);
  if (fm.status !== 'ok') return done(`Deleted "${name}". Its message was already gone, or I couldn't reach it.`, [], { groupId: group.id });
  if (group.attached) {
    await rr.removeOwnReactions(fm.message, rr.indexItems(group), client); // never delete or edit an attached message
    return done(`Deleted "${name}". The original message was left alone.`, [], { groupId: group.id });
  }
  markBotDeletion(fm.message.id, 'reaction-roles:delete');
  const deleted = await fm.message.delete().then(() => true, () => false);
  return done(deleted
    ? `Deleted "${name}", along with its message.`
    : `Deleted "${name}". I couldn't delete its message, so delete it by hand.`, [], { groupId: group.id });
}

// ── rr.timezoneTemplate ──────────────────────────────────────────────────────

async function timezoneTemplate({ guild, client, actor, payload }) {
  await authorize(guild, actor);
  const key = payload?.template;
  if (typeof key !== 'string' || !Object.hasOwn(TIMEZONE_ROLE_TEMPLATES, key)) throw new TaskError('That template is not valid.');
  const group = await loadGroup(guild, payload);
  if (group.items.length) throw new TaskError('That panel already has roles.');
  let setup;
  try {
    // The channel first, so roles are never created for a panel that cannot be posted.
    requireUsable(await postChannel(guild, group.channelId, { strictType: true }), needsFor(group));
    setup = await setupTimezoneRoles(guild, key, {
      executor: actor.member ?? null, reason: `Timezone roles template (dashboard, by ${actor.id})`,
    });
  } catch (err) {
    // Nothing was posted: drop the empty panel here too, in case the dashboard stopped waiting for this task.
    await rr.deletePanel(guild.id, group.id).catch(() => {});
    await rr.invalidatePanelIndex(guild.id);
    if (err instanceof TimezoneSetupError) throw new TaskError(err.message.replace(/\*\*/g, ''));
    throw err;
  }
  const items = [];
  for (const r of setup.roles) {
    const description = group.layout === 'SELECT' ? r.description : null;
    items.push(await rr.addItem({ id: group.id, items }, { roleId: r.roleId, label: r.label, style: 2, description }));
  }
  await rr.invalidatePanelIndex(guild.id);
  const posted = await publish({ guild, client, actor, payload: { groupId: group.id, mode: 'post' } });
  return { ...posted, message: `${describeSetup(setup)} ${posted.message}` };
}

export const tasks = Object.freeze({
  'rr.publish': { run: publish },
  'rr.repost': { run: repost },
  'rr.delete': { run: remove },
  'rr.timezoneTemplate': { run: timezoneTemplate },
});
