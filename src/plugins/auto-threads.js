// Plugin `auto-threads`: every member message in a configured text or announcement channel gets a thread (optionally with
// a Close button), and new posts in the announcement channels of GuildSettings.autoPublishChannelIds are published to
// the servers that follow them. The lists come from the settings memo, so an ordinary message costs no query. The
// `autothread:close:{starterUserId}` button closes a thread. Rules and state: src/services/autoThreadService.js.
import { ChannelType, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../config/constants.js';
import { parseCustomId } from '../config/customIds.js';
import { isModuleEnabled } from '../config/modules.js';
import { peekSettings } from '../services/settingsService.js';
import {
  channelIndex, createThread, detach, dropChannel, listedChannel, plainText, publishMessage,
} from '../services/autoThreadService.js';
import { isInteractionGone, replyError } from '../utils/interactionHelpers.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import logger from '../utils/logger.js';

const THREADABLE = new Set([ChannelType.GuildText, ChannelType.GuildAnnouncement]);

// ── Hook filters (sync: they run on every message) ────────────────────────────

/**
 * A member message in a text or announcement channel (not a thread or forum) that may need a thread or a crosspost: the
 * settings memo, when there is one, says the module is on and lists the channel. Without a memo it passes and the
 * handler loads the settings.
 * @param {object} ctx HookContext with `message`
 */
export function mayHandle(ctx) {
  const channel = ctx.message?.channel ?? ctx.channel;
  if (!channel || channel.isThread?.() || !THREADABLE.has(channel.type)) return false;
  return listedChannel(peekSettings(ctx.guildId), channel.id) !== false;
}

/**
 * botMessage: a post by another bot or a webhook in an announcement channel that may be published. The hook sees every
 * bot message of every server, so this is a type check and a memo lookup. Edits are skipped, except the one that turns a
 * deferred reply's "thinking" placeholder (never published, see isPublishable) into the answer (`wasLoading`).
 * @param {object} ctx HookContext with `channel`
 */
export function mayPublish(ctx) {
  if (ctx.edited === true && ctx.wasLoading !== true) return false; // an edit of a post is never published again
  const channel = ctx.channel ?? ctx.message?.channel;
  if (channel?.type !== ChannelType.GuildAnnouncement) return false;
  const settings = peekSettings(ctx.guildId);
  if (!settings) return true;
  return isModuleEnabled(settings, 'autothreads') && channelIndex(settings).publish.has(channel.id);
}

/** channelDelete: a channel that may be in a list. */
export function mayBeListed(ctx) {
  const id = ctx.channel?.id;
  if (!id) return false;
  const settings = peekSettings(ctx.guildId);
  if (!settings) return true;
  const index = channelIndex(settings);
  return index.threads.has(id) || index.publish.has(id);
}

// ── Hook handlers ─────────────────────────────────────────────────────────────

/** messageCreate: publish the post (in the background) and start its thread. Never throws. */
export async function onMessage(ctx) {
  const { message } = ctx;
  if (!message?.channelId) return;
  const settings = await ctx.getSettings();
  if (!settings || !isModuleEnabled(settings, 'autothreads')) return;
  const index = channelIndex(settings);
  const entry = index.threads.get(message.channelId);
  // The crosspost can sit in the REST queue; the hooks after this one (sticky) must not wait for it.
  if (index.publish.has(message.channelId)) detach(publishMessage(message));
  if (entry) await createThread(message, entry);
}

/** botMessage: publish the post of a bot or webhook in the background. */
export async function onBotMessage(ctx) {
  const { message } = ctx;
  if (!message?.channelId) return;
  const settings = await ctx.getSettings();
  if (!settings || !isModuleEnabled(settings, 'autothreads')) return;
  if (!channelIndex(settings).publish.has(message.channelId)) return;
  detach(publishMessage(message));
}

/** channelDelete: the channel is gone, so it leaves both lists (also while the module is off). */
export async function onChannelDelete(ctx) {
  const channelId = ctx.channel?.id;
  const settings = await ctx.getSettings();
  if (settings) {
    const index = channelIndex(settings);
    if (!index.threads.has(channelId) && !index.publish.has(channelId)) return;
  }
  if (await dropChannel(ctx.guildId, channelId)) logger.info(`[auto-threads] ${ctx.guildId}: ${channelId} removed (the channel was deleted)`);
}

// ── The Close button ──────────────────────────────────────────────────────────

const notice = (interaction, title, text) => replyError(interaction, title, text, { ephemeral: true });

/**
 * `autothread:close:{starterUserId}`: archives (and locks, when the bot may) the thread. Allowed for the member who
 * started the thread, a member with Manage Threads there and the bot's Moderators. The message is edited first (an
 * archived thread no longer takes edits), then the thread is closed.
 * @param {import('discord.js').ButtonInteraction} interaction
 */
export async function handleClose(interaction) {
  const { params: [action, starterId] } = parseCustomId(interaction.customId);
  const thread = interaction.channel;
  if (action !== 'close' || !starterId || !interaction.inCachedGuild() || !thread?.isThread?.()) {
    return notice(interaction, 'Button Expired', 'This button is no longer active.');
  }
  const allowed = interaction.user.id === starterId
    || interaction.memberPermissions?.has(PermissionFlagsBits.ManageThreads) === true
    || resolvePermissionLevel(interaction.member, peekSettings(interaction.guildId)) >= PERMISSION_LEVELS.MODERATOR;
  if (!allowed) return notice(interaction, 'Not Allowed', 'Only the person who started this thread, or a moderator, can close it.');
  if (thread.archived) return notice(interaction, 'Already Closed', 'This thread is already closed.');

  const me = interaction.guild.members.me;
  const canLock = Boolean(me && thread.permissionsFor?.(me)?.has(PermissionFlagsBits.ManageThreads));
  if (!canLock && thread.ownerId !== interaction.client.user?.id) {
    return notice(interaction, 'Cannot Close', 'I need **Manage Threads** here to close this thread.');
  }

  const who = plainText(interaction.member?.displayName ?? interaction.user.username) || 'a member';
  try {
    await interaction.update({ content: `🔒 Thread closed by ${who}.`, components: [], allowedMentions: { parse: [] } });
  } catch (err) {
    if (isInteractionGone(err)) return undefined;
    logger.warn(`[auto-threads] ${interaction.guildId}: could not update the close message: ${err?.message ?? err}`);
  }
  try {
    await thread.edit({ archived: true, ...(canLock ? { locked: true } : {}), reason: `Closed by ${interaction.user.id}` });
  } catch (err) {
    logger.warn(`[auto-threads] ${interaction.guildId}: could not close ${thread.id}: ${err?.message ?? err}`);
    await interaction.followUp({ content: 'I could not close this thread. Check my permissions here.', ephemeral: true }).catch(() => {});
  }
  return undefined;
}

export const plugin = {
  id: 'auto-threads',
  module: 'autothreads',
  hooks: [
    // HOOK_ORDER.messageCreate: after automod has removed what it removes, before sticky.
    { event: 'messageCreate', id: 'auto-threads', filter: mayHandle, run: onMessage },
    { event: 'botMessage', id: 'auto-threads', filter: mayPublish, run: onBotMessage },
    // Cleanup runs while the module is off too: a list must not keep a channel that is gone.
    { event: 'channelDelete', id: 'auto-threads', always: true, filter: mayBeListed, run: onChannelDelete },
  ],
  // Gated by the foundation wrapper (plugin.module): while the module is off the button answers "turned off".
  components: [
    { type: 'button', prefix: 'autothread', handler: handleClose },
  ],
  jobs: [],
  provides: {},
};
