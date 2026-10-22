// Plugin `sticky`: keeps a message at the bottom of a channel. A member message in a sticky channel schedules a re-send
// (it runs once the channel has been quiet for a few seconds); deleting the sticky message or its channel cleans up even
// while the module is off. Also the `sticky-modal` text form of `/sticky set`. Rules and state: src/services/stickyService.js.
import { ActionRowBuilder, ModalBuilder, TextInputBuilder, TextInputStyle } from 'discord.js';
import { PERMISSION_LEVELS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { isModuleEnabled } from '../config/modules.js';
import { unknownVariablesNote } from '../services/embedService.js';
import { peekSettings } from '../services/settingsService.js';
import {
  StickyError, setSticky, noteMessage, getChannels, peekChannels, clearLastMessage, dropChannel,
} from '../services/stickyService.js';
import { LIMITS as MESSAGE_LIMITS } from '../utils/customEmbed.js';
import { errorEmbed, successEmbed } from '../utils/embedBuilder.js';
import { replyError, safeDefer, safeReply } from '../utils/interactionHelpers.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import logger from '../utils/logger.js';

/** The text field of the `sticky-modal` form (a `field` customId: no parameters). */
export const STICKY_FIELD = 'sticky-content';

// ── Hook filters (sync: they run on every message) ────────────────────────────

/**
 * A member message that may need a re-send: the module is not known to be off and the memoised sticky channels (when
 * there are any) include a running one. Without a memo it passes and the handler loads the list.
 * @param {object} ctx HookContext with `message`
 */
export function mayStick(ctx) {
  const settings = peekSettings(ctx.guildId);
  if (settings && !isModuleEnabled(settings, 'sticky')) return false;
  const channels = peekChannels(ctx.guildId);
  return !channels || channels.get(ctx.message?.channelId) === true;
}

const hasStickyIn = (ctx, channelId) => {
  const channels = peekChannels(ctx.guildId);
  return !channels || (channelId != null && channels.has(channelId));
};

// The bot's own messages, or ones it never cached (the sticky message may have fallen out of the cache).
const isOwnOrUnknown = (ctx) => ctx.message?.partial === true || ctx.message?.author?.id === ctx.client?.user?.id;

/** messageDelete: a message of the bot (or an uncached one) in a channel that may have a sticky. */
export const mayBeSticky = (ctx) => isOwnOrUnknown(ctx) && hasStickyIn(ctx, ctx.message?.channelId ?? ctx.channel?.id);

/** channelDelete: a channel that may have a sticky. */
export const mayHaveSticky = (ctx) => hasStickyIn(ctx, ctx.channel?.id);

// ── Hook handlers ─────────────────────────────────────────────────────────────

/** Schedules the re-send for a message in a sticky channel; returns at once (the timer does the waiting). */
export async function onMessage(ctx) {
  const channelId = ctx.message?.channelId;
  if (!ctx.guildId || !channelId) return;
  const channels = await getChannels(ctx.guildId);
  if (channels?.get(channelId) === true) noteMessage(ctx.client, ctx.guildId, channelId);
}

/** The sticky message was deleted by someone else: forget its id (the next re-send just posts). */
export async function onMessageDelete(ctx) {
  await clearLastMessage(ctx.guildId, ctx.message?.channelId ?? ctx.channel?.id, ctx.message?.id);
}

/** The channel is gone: so is its sticky. */
export async function onChannelDelete(ctx) {
  await dropChannel(ctx.guildId, ctx.channel?.id);
}

// ── /sticky set: the text form and the answer ─────────────────────────────────

/**
 * The form `/sticky set` opens when no text is given: one paragraph prefilled with the current text.
 * @param {string} channelId
 * @param {string|null} [content]
 * @returns {ModalBuilder}
 */
export function buildStickyModal(channelId, content = null) {
  const input = new TextInputBuilder()
    .setCustomId(STICKY_FIELD)
    .setLabel('Sticky message text')
    .setStyle(TextInputStyle.Paragraph)
    .setMaxLength(MESSAGE_LIMITS.content)
    .setRequired(false)
    .setPlaceholder('Rules, links, a reminder … {server} and {channel} work.');
  if (typeof content === 'string' && content !== '') input.setValue(content.slice(0, MESSAGE_LIMITS.content));
  return new ModalBuilder()
    .setCustomId(buildCustomId('sticky-modal', channelId))
    .setTitle('Sticky message')
    .addComponents(new ActionRowBuilder().addComponents(input));
}

const STATUS_NOTES = Object.freeze({
  sent: 'It is posted now and moves back down once chat has been quiet for a few seconds.',
  current: 'It is already the last message there.',
  off: '⚠️ The **Sticky Messages** module is off, so nothing is posted yet. Turn it on with `/module enable sticky`.',
  perms: "⚠️ I can't post there right now. Check my permissions in the channel.",
});
const NOT_POSTED = 'It is saved but could not be posted just now; it is tried again after the next message.';

const USER_TOKEN_RE = /\{user(\.[a-z_]+)?\}/i;
const USER_TOKEN_WARNING = '\n⚠️ A sticky message has no member, so user variables like `{user}` are posted as written. Server and channel variables work.';

/**
 * Warning lines for variables a sticky cannot fill in: unknown ones, and `{user…}` (it renders with the server and the
 * channel only, so nobody is "the user"). '' when there is nothing to warn about.
 * @param {{ content?: string|null, embed?: { embed?: object|null, buttons?: object[] }|null }} part what was typed
 * @param {{ guild: object, channel: object }} renderCtx
 */
export function variableNotes({ content = null, embed = null } = {}, renderCtx) {
  try {
    const tpl = { content, embed: embed?.embed ?? null, buttons: embed?.buttons ?? [] };
    let notes = unknownVariablesNote(tpl, renderCtx);
    if (USER_TOKEN_RE.test(JSON.stringify(tpl))) notes += USER_TOKEN_WARNING;
    return notes;
  } catch (err) {
    logger.debug(`[sticky] variable check failed: ${err?.message}`); // the sticky is saved either way
    return '';
  }
}

/** The embed that answers a saved `/sticky set`. `notes` (variable warnings) is appended as is. */
export function setResultEmbed(channelId, { created, hasEmbed, status }, notes = '') {
  const head = `The sticky message in <#${channelId}> is ${created ? 'set' : 'updated'}${hasEmbed ? ' (with an embed)' : ''}.`;
  return successEmbed(created ? 'Sticky Message Set' : 'Sticky Message Updated', `${head}\n${STATUS_NOTES[status] ?? NOT_POSTED}${notes}`);
}

/**
 * Saves a sticky and answers the (deferred) interaction. Never throws.
 * @param {import('discord.js').ChatInputCommandInteraction|import('discord.js').ModalSubmitInteraction} interaction
 * @param {{ guild: object, channel: object, content?: string|null, embed?: object|null, keepEmbed?: boolean }} args
 */
export async function applySet(interaction, args) {
  let embed;
  try {
    const result = await setSticky({ client: interaction.client, userId: interaction.user.id, ...args });
    logger.info(`[sticky] ${args.guild.id}: ${args.channel.id} ${result.created ? 'set' : 'updated'} by ${interaction.user.id} (${result.status})`);
    embed = setResultEmbed(args.channel.id, result, variableNotes(args, { guild: args.guild, channel: args.channel }));
  } catch (err) {
    if (err instanceof StickyError) {
      embed = errorEmbed(err.title, err.message);
    } else {
      logger.error(`[sticky] ${args.guild.id}: set failed`, err);
      embed = errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.');
    }
  }
  return safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
}

/**
 * `sticky-modal:{channelId}` submit. Works while the module is off (admins set stickies up first); the router checks no
 * level for forms, so the Admin level is checked here.
 * @param {import('discord.js').ModalSubmitInteraction} interaction
 */
export async function handleStickyModal(interaction) {
  const { params: [channelId] } = parseCustomId(interaction.customId);
  if (!interaction.inCachedGuild() || !channelId) {
    return replyError(interaction, 'Form Expired', 'This form has expired. Please run `/sticky set` again.');
  }
  if (resolvePermissionLevel(interaction.member, peekSettings(interaction.guildId)) < PERMISSION_LEVELS.ADMIN) {
    return replyError(interaction, 'Insufficient Permissions', 'You need the Admin level to change sticky messages.');
  }
  let content;
  try {
    content = interaction.fields.getTextInputValue(STICKY_FIELD);
  } catch {
    return replyError(interaction, 'Form Expired', 'This form is out of date. Please run `/sticky set` again.');
  }
  const channel = interaction.guild.channels.cache.get(channelId);
  if (!channel) return replyError(interaction, 'Channel Not Found', 'That channel no longer exists.');

  if (!(await safeDefer(interaction))) return undefined;
  return applySet(interaction, { guild: interaction.guild, channel, content, keepEmbed: true });
}

export const plugin = {
  id: 'sticky',
  module: 'sticky',
  hooks: [
    {
      event: 'messageCreate',
      id: 'sticky', // HOOK_ORDER.messageCreate: last, after automod has removed what it removes
      filter: mayStick,
      run: onMessage,
    },
    // Cleanup runs while the module is off too: a row must not keep pointing at a message or channel that is gone.
    { event: 'messageDelete', id: 'sticky', always: true, filter: mayBeSticky, run: onMessageDelete },
    { event: 'channelDelete', id: 'sticky', always: true, filter: mayHaveSticky, run: onChannelDelete },
  ],
  components: [
    // sticky-modal:{channelId}: the admin text form, so it must work while the module is off.
    { type: 'modal', prefix: 'sticky-modal', module: null, handler: handleStickyModal },
  ],
  jobs: [],
  provides: {},
};
