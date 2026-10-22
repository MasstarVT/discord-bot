// Auto Threads: a thread on every member message in chosen channels, and auto-publishing of announcement posts. The
// lists live in GuildSettings (autoThreadChannels, autoPublishChannelIds), so the hot path reads them from the settings
// memo and never queries Postgres. Hook handling and the close button: src/plugins/auto-threads.js, the command:
// src/commands/admin/autothread.js.
import {
  ActionRowBuilder, ButtonBuilder, ButtonStyle, ChannelType, MessageFlags, MessageReferenceType, MessageType,
} from 'discord.js';
import { LIMITS } from '../config/constants.js';
import { buildCustomId } from '../config/customIds.js';
import { isModuleEnabled } from '../config/modules.js';
import { updateSettingsWith } from './settingsService.js';
import { createThrottle } from '../utils/async.js';
import { missingBotPermissions, permissionNames } from '../utils/interactionHelpers.js';
import { renderPlaceholders, renderPlaceholdersDetailed } from '../utils/placeholderParser.js';
import { clampText } from '../utils/text.js';
import logger from '../utils/logger.js';

export const AUTOTHREAD = Object.freeze({
  // GuildSettings stores the archive time as one of these strings (the dashboard's enum).
  ARCHIVE_CHOICES: Object.freeze(['60', '1440', '4320', '10080']),
  ARCHIVE_LABELS: Object.freeze({ 60: '1 hour', 1440: '1 day', 4320: '3 days', 10080: '1 week' }),
  DEFAULT_ARCHIVE: '1440',
  DEFAULT_NAME: '{content}',
  // Discord's limit for a thread name.
  NAME_MAX: 100,
  // How much of a message's first line is read for {content} (cut before the markup is stripped).
  LINE_MAX: 300,
  // How many lines are looked at to find the first one with text.
  LINES_MAX: 5,
  // A channel the bot cannot use is complained about in the log at most this often.
  WARN_MS: 60 * 60_000,
  // Discord allows about this many crossposts per channel per hour; past it the post stays unpublished.
  PUBLISH_MAX: 10,
  PUBLISH_WINDOW_MS: 60 * 60_000,
  PUBLISH_MAX_CHANNELS: 1_000,
  CLOSE_TEXT: 'Done here? The person who started this thread, or a moderator, can close it.',
});

// Unknown channel or message, missing access or permission: the target is gone, nothing to retry.
const PERMISSION_CODES = new Set([50001, 50013]);
// 10008 unknown message, 160004 the message already has a thread, 40033 already crossposted, 50068 invalid message type.
const HARMLESS_CODES = new Set([10003, 10008, 160004, 40033, 50068]);

/** An admin-facing refusal: `title` and the message are shown as they are. */
export class AutoThreadError extends Error {
  constructor(code, title, message) {
    super(message);
    this.name = 'AutoThreadError';
    this.code = code;
    this.title = title;
  }
}

// ── Reading the lists (defensive: the dashboard and older code write these too) ──────────────────────────────

const SNOWFLAKE = /^\d{5,25}$/;
const isId = (v) => typeof v === 'string' && SNOWFLAKE.test(v);

/**
 * One stored entry as `{ channelId, name, archiveMinutes ('60'|'1440'|'4320'|'10080'), closeButton }`, or null when it
 * names no channel. Anything else that is off falls back to the default.
 * @param {unknown} raw
 */
export function normalizeEntry(raw) {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw) || !isId(raw.channelId)) return null;
  const name = typeof raw.name === 'string' ? raw.name.trim() : '';
  const archive = String(raw.archiveMinutes ?? '');
  return {
    channelId: raw.channelId,
    name: name !== '' ? name.slice(0, AUTOTHREAD.NAME_MAX) : AUTOTHREAD.DEFAULT_NAME,
    archiveMinutes: AUTOTHREAD.ARCHIVE_CHOICES.includes(archive) ? archive : AUTOTHREAD.DEFAULT_ARCHIVE,
    closeButton: raw.closeButton === true,
  };
}

/** @returns {{ channelId: string, name: string, archiveMinutes: string, closeButton: boolean }[]} valid entries, first of a channel wins, capped */
export function readEntries(settings) {
  const list = Array.isArray(settings?.autoThreadChannels) ? settings.autoThreadChannels : [];
  const seen = new Set();
  const out = [];
  for (const raw of list) {
    const entry = normalizeEntry(raw);
    if (!entry || seen.has(entry.channelId)) continue;
    seen.add(entry.channelId);
    out.push(entry);
    if (out.length >= LIMITS.AUTOTHREAD_CHANNELS) break;
  }
  return out;
}

/** @returns {string[]} the channel ids to publish in, valid and unique, capped */
export function readPublishIds(settings) {
  const list = Array.isArray(settings?.autoPublishChannelIds) ? settings.autoPublishChannelIds : [];
  return [...new Set(list.filter(isId))].slice(0, LIMITS.AUTOPUBLISH_CHANNELS);
}

const EMPTY_INDEX = Object.freeze({ threads: new Map(), publish: new Set() });
// GuildSettings objects are frozen and replaced on every change, so the parsed lists need no invalidation.
const indexes = new WeakMap();

/**
 * The guild's lists as lookups: `threads` (channelId -> entry) and `publish` (a Set of ids). Parsed once per settings
 * object. Synchronous and free of I/O: the hook filters call it on every message.
 * @param {object|null|undefined} settings
 */
export function channelIndex(settings) {
  if (!settings || typeof settings !== 'object') return EMPTY_INDEX;
  let index = indexes.get(settings);
  if (!index) {
    index = { threads: new Map(readEntries(settings).map((e) => [e.channelId, e])), publish: new Set(readPublishIds(settings)) };
    indexes.set(settings, index);
  }
  return index;
}

/** Whether the module is on and the channel is in either list. Settings unknown (not memoised yet): undefined. */
export function listedChannel(settings, channelId) {
  if (!settings) return undefined;
  if (!isModuleEnabled(settings, 'autothreads')) return false;
  const index = channelIndex(settings);
  return index.threads.has(channelId) || index.publish.has(channelId);
}

// ── Thread names ──────────────────────────────────────────────────────────────

/**
 * Message text as plain words: markdown, mention and emoji tags, spoilers and quote/heading marks removed.
 * @param {unknown} text one line
 * @returns {string}
 */
export function plainText(text) {
  return String(text ?? '')
    .slice(0, AUTOTHREAD.LINE_MAX)
    .replace(/\|\|[\s\S]*?\|\|/g, ' ')                       // a spoiler must not leak into a title
    .replace(/<a?:(\w{2,32}):\d{15,25}>/g, ':$1:')             // custom emoji
    .replace(/<(?:@[!&]?|#)\d{15,25}>/g, ' ')                  // raw mention tags
    .replace(/<\/([\w -]{1,32}):\d{15,25}>/g, '/$1')           // slash command mentions
    .replace(/<t:-?\d{1,15}(?::[a-zA-Z])?>/g, ' ')             // timestamps
    .replace(/@\u200b?(everyone|here)\b/gi, ' ')
    .replace(/^\s*(?:-#|#{1,3}|>{1,3}|[-*+])\s+/, '')           // heading, subtext, quote, bullet
    .replace(/\[([^\]]{1,200})\]\(\s*<?[^)\s]{1,500}>?\s*\)/g, '$1') // [text](url)
    .replace(/```[a-z0-9+-]*/gi, ' ')
    .replace(/`([^`]+)`/g, '$1')
    .replace(/\*\*([^*]+)\*\*|\*([^*]+)\*|~~([^~]+)~~|__([^_]+)__/g, (_m, a, b, c, d) => a ?? b ?? c ?? d)
    .replace(/(^|[\s(])_([^_\s][^_]*?)_(?=$|[\s).,!?])/g, '$1$2')
    .replace(/(^|[\s([])[@#]/g, '$1')                          // "@Name" and "#channel" left by cleanContent
    .replace(/[<>*~|\\]/g, ' ')
    .replace(/[\u0000-\u001f\u007f\u200b-\u200f\u2028\u2029]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
}

/**
 * The first line of the message that has any words left once its markup is removed ('' when none).
 * @param {unknown} text usually `message.cleanContent`
 */
export function contentTitle(text) {
  const lines = String(text ?? '').split(/\r?\n/, AUTOTHREAD.LINES_MAX);
  for (const line of lines) {
    const plain = plainText(line);
    if (/[\p{L}\p{N}]/u.test(plain)) return plain;
  }
  return '';
}

/**
 * The thread's name: the template through the placeholder engine (`{user}` is the display name, `{content}` the start of
 * the message, `{channel}` the channel's name and `{date}` the UTC date, YYYY-MM-DD; the usual `{server}` etc. work too),
 * whitespace collapsed, at most 100 characters. A name that comes out empty is the author's display name.
 * @param {string} template
 * @param {{ member?: object|null, user?: object|null, guild?: object|null, channel?: object|null, content?: string,
 *   createdAt?: Date|number|null }} source
 * @returns {string}
 */
export function threadName(template, { member = null, user = null, guild = null, channel = null, content = '', createdAt = null } = {}) {
  const author = plainText(member?.displayName ?? user?.globalName ?? user?.username ?? '');
  const when = createdAt ? new Date(createdAt) : new Date();
  const date = Number.isFinite(when.getTime()) ? when.toISOString().slice(0, 10) : new Date().toISOString().slice(0, 10);
  const vars = { user: author, content: contentTitle(content), channel: plainText(channel?.name ?? ''), date };
  const tidy = (s) => clampText(String(s ?? '').replace(/[\u0000-\u001f\u007f\u2028\u2029]/g, ' ').replace(/\s+/g, ' ').trim(), AUTOTHREAD.NAME_MAX, '');
  const rendered = tidy(renderPlaceholders(template || AUTOTHREAD.DEFAULT_NAME, { member, user, guild, channel, vars }));
  return rendered || tidy(author) || 'Thread';
}

/** The placeholders in a name template that nothing knows (so `/autothread add` can refuse a typo). */
export function unknownTokens(template) {
  return renderPlaceholdersDetailed(template, { vars: { user: '', content: '', channel: '', date: '' } }).unknownTokens;
}

// ── Permissions ───────────────────────────────────────────────────────────────

/** What the bot needs in a configured channel to start threads (and post the close button inside them). */
export const threadNeeds = (entry) => (entry?.closeButton
  ? ['ViewChannel', 'CreatePublicThreads', 'SendMessagesInThreads']
  : ['ViewChannel', 'CreatePublicThreads']);

/** What the bot needs in an announcement channel to publish posts made by others. */
export const PUBLISH_NEEDS = Object.freeze(['ViewChannel', 'SendMessages', 'ManageMessages']);

/** The display names of the permissions the bot lacks in the channel for the given needs ([] = fine). */
export function missingNames(channel, needs) {
  return permissionNames(missingBotPermissions(channel, needs));
}

const warnThrottle = createThrottle(AUTOTHREAD.WARN_MS);

/** Logs a problem with a channel at most once per hour per kind. */
function warnOnce(kind, channel, text) {
  if (warnThrottle(`${kind}:${channel?.id}`)) logger.warn(`[auto-threads] ${channel?.guildId ?? channel?.guild?.id ?? ''}: ${text}`);
}

// ── Starting threads ──────────────────────────────────────────────────────────

/** @returns {import('discord.js').ActionRowBuilder} the row with the Close button, for the thread started by `starterId` */
export function closeRow(starterId) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder()
      .setCustomId(buildCustomId('autothread', 'close', starterId))
      .setLabel('Close thread')
      .setEmoji('🔒')
      .setStyle(ButtonStyle.Secondary),
  );
}

/** Posts the Close button in a new thread. Never throws and never pings. */
export async function postCloseButton(thread, starterId) {
  try {
    await thread.send({ content: AUTOTHREAD.CLOSE_TEXT, components: [closeRow(starterId)], allowedMentions: { parse: [] } });
    return true;
  } catch (err) {
    if (!HARMLESS_CODES.has(err?.code)) warnOnce('close', thread?.parent ?? thread, `could not post the close button: ${err?.message ?? err}`);
    return false;
  }
}

// Detached work (the close button, crossposts): the hook must not wait for it, because the hooks after this one
// (sticky) would wait with it. Tracked so tests can await it.
const inflight = new Set();

/** Runs `promise` in the background; a rejection is logged, never unhandled. @returns {Promise<void>} */
export function detach(promise) {
  const tracked = Promise.resolve(promise)
    .catch((err) => logger.warn(`[auto-threads] background task failed: ${err?.message ?? err}`))
    .then(() => { inflight.delete(tracked); });
  inflight.add(tracked);
  return tracked;
}

/** Resolves when every detached task has finished (tests). */
export const settled = async () => { while (inflight.size) await Promise.all([...inflight]); };

/**
 * Starts the thread for one member message. The close button is posted in the background.
 * @param {import('discord.js').Message} message
 * @param {{ name: string, archiveMinutes: string, closeButton: boolean }} entry
 * @returns {Promise<'created'|'perms'|'skipped'|'failed'>}
 */
export async function createThread(message, entry) {
  const channel = message.channel;
  if (!channel || channel.isThread?.()) return 'skipped';
  const missing = missingNames(channel, threadNeeds(entry));
  if (missing.length) {
    warnOnce('perms', channel, `cannot start threads in ${channel.id}: missing ${missing.join(', ')}`);
    return 'perms';
  }
  let thread;
  try {
    thread = await message.startThread({
      name: threadName(entry.name, {
        member: message.member, user: message.author, guild: message.guild ?? channel.guild, channel,
        content: message.cleanContent ?? message.content ?? '', createdAt: message.createdAt,
      }),
      autoArchiveDuration: Number(entry.archiveMinutes),
      reason: 'Auto Threads: new message',
    });
  } catch (err) {
    if (PERMISSION_CODES.has(err?.code)) {
      warnOnce('perms', channel, `cannot start threads in ${channel.id}: Discord refused (${err.code})`);
      return 'perms';
    }
    if (!HARMLESS_CODES.has(err?.code)) warnOnce('fail', channel, `could not start a thread in ${channel.id}: ${err?.message ?? err}`);
    return 'failed';
  }
  if (entry.closeButton && thread) detach(postCloseButton(thread, message.author?.id ?? message.authorId));
  return 'created';
}

// ── Publishing ────────────────────────────────────────────────────────────────

/**
 * Counts crossposts per channel over a sliding window (in memory: it starts empty after a restart, so the first hour
 * after one may exceed what Discord allows by a few and meet a 429). Attempts are counted, not successes.
 * @param {{ max?: number, windowMs?: number, maxChannels?: number, now?: () => number }} [opts]
 */
export function createPublishLimiter({
  max = AUTOTHREAD.PUBLISH_MAX, windowMs = AUTOTHREAD.PUBLISH_WINDOW_MS, maxChannels = AUTOTHREAD.PUBLISH_MAX_CHANNELS, now = Date.now,
} = {}) {
  /** @type {Map<string, number[]>} */
  const hits = new Map();
  const prune = (list, t) => { while (list.length && list[0] <= t - windowMs) list.shift(); };
  return {
    /** Takes a slot for the channel. @returns {boolean} false when the window is full */
    take(channelId) {
      const t = now();
      const list = hits.get(channelId) ?? [];
      prune(list, t);
      if (list.length >= max) {
        hits.set(channelId, list);
        return false;
      }
      list.push(t);
      hits.delete(channelId); // re-insert: the map stays in least-recently-used order
      hits.set(channelId, list);
      if (hits.size > maxChannels) {
        for (const [id, l] of hits) {
          prune(l, t);
          if (l.length === 0) hits.delete(id);
        }
        while (hits.size > maxChannels) hits.delete(hits.keys().next().value);
      }
      return true;
    },
    /** Gives back the channel's newest slot (a request Discord refused as invalid did not publish anything). */
    release(channelId) {
      hits.get(channelId)?.pop();
    },
    /** Slots left in the channel's window. */
    remaining(channelId) {
      const list = hits.get(channelId) ?? [];
      prune(list, now());
      return Math.max(0, max - list.length);
    },
    get size() { return hits.size; },
    clear() { hits.clear(); },
  };
}

const publishLimiter = createPublishLimiter();
/** Tests only: the shared limiter. */
export const _publishLimiter = publishLimiter;

const PUBLISHABLE_TYPES = new Set([MessageType.Default, MessageType.Reply, MessageType.ChatInputCommand, MessageType.ContextMenuCommand]);

/**
 * Whether the message is a post Discord lets a channel publish (not a system message, a poll, a forward, a post that is
 * itself a crosspost, or a bot's "thinking" placeholder: a deferred reply is published when the edit turns it into the
 * answer, see mayPublish in src/plugins/auto-threads.js).
 */
export function isPublishable(message) {
  if (!message || !PUBLISHABLE_TYPES.has(message.type) || message.poll) return false;
  if (message.reference?.type === MessageReferenceType.Forward) return false;
  const flags = message.flags;
  if (typeof flags?.has === 'function'
    && (flags.has(MessageFlags.Crossposted) || flags.has(MessageFlags.IsCrosspost) || flags.has(MessageFlags.Loading))) return false;
  return true;
}

/**
 * Publishes one post of an announcement channel to the servers that follow it.
 * @param {import('discord.js').Message} message
 * @param {{ limiter?: ReturnType<typeof createPublishLimiter> }} [opts]
 * @returns {Promise<'published'|'skipped'|'perms'|'limited'|'failed'>}
 */
export async function publishMessage(message, { limiter = publishLimiter } = {}) {
  const channel = message?.channel;
  if (channel?.type !== ChannelType.GuildAnnouncement || !isPublishable(message)) return 'skipped';
  const missing = missingNames(channel, PUBLISH_NEEDS);
  if (missing.length) {
    warnOnce('publish-perms', channel, `cannot publish in ${channel.id}: missing ${missing.join(', ')}`);
    return 'perms';
  }
  if (!limiter.take(channel.id)) {
    warnOnce('publish-limit', channel, `${channel.id}: ${AUTOTHREAD.PUBLISH_MAX} posts were published in the last hour; the next ones stay unpublished until the window clears`);
    return 'limited';
  }
  try {
    await message.crosspost();
    return 'published';
  } catch (err) {
    if (PERMISSION_CODES.has(err?.code)) {
      warnOnce('publish-perms', channel, `cannot publish in ${channel.id}: Discord refused (${err.code})`);
      return 'perms';
    }
    // Invalid message type: Discord published nothing, so the post does not use up one of the hour's slots.
    if (err?.code === 50068) limiter.release(channel.id);
    if (!HARMLESS_CODES.has(err?.code)) warnOnce('publish-fail', channel, `could not publish in ${channel.id}: ${err?.message ?? err}`);
    return 'failed';
  }
}

// ── Changing the lists (the command and channelDelete) ────────────────────────

const listOf = (current) => (Array.isArray(current?.autoThreadChannels) ? current.autoThreadChannels : []);
const idsOf = (current) => (Array.isArray(current?.autoPublishChannelIds) ? current.autoPublishChannelIds : []);

/**
 * Adds a channel to the thread list, or changes the options given for one already there (the rest stay as they are).
 * Other entries are kept exactly as stored.
 * @param {string} guildId
 * @param {{ channelId: string, name?: string|null, archiveMinutes?: string|null, closeButton?: boolean|null }} change
 * @returns {Promise<{ created: boolean, entry: object, settings: object }>}
 * @throws {AutoThreadError} when the list is full
 */
export async function setThreadChannel(guildId, { channelId, name = null, archiveMinutes = null, closeButton = null }) {
  let result;
  const settings = await updateSettingsWith(guildId, (current) => {
    const list = listOf(current);
    const at = list.findIndex((e) => e?.channelId === channelId);
    if (at === -1 && list.length >= LIMITS.AUTOTHREAD_CHANNELS) {
      throw new AutoThreadError('LIMIT', 'Limit Reached', `A server can have at most **${LIMITS.AUTOTHREAD_CHANNELS}** auto-thread channels. Remove one with \`/autothread remove\` first.`);
    }
    const base = (at === -1 ? null : normalizeEntry(list[at]))
      ?? { channelId, name: AUTOTHREAD.DEFAULT_NAME, archiveMinutes: AUTOTHREAD.DEFAULT_ARCHIVE, closeButton: false };
    const entry = {
      channelId,
      name: name ?? base.name,
      archiveMinutes: archiveMinutes ?? base.archiveMinutes,
      closeButton: closeButton ?? base.closeButton,
    };
    result = { created: at === -1, entry };
    return { autoThreadChannels: at === -1 ? [...list, entry] : list.map((e, i) => (i === at ? entry : e)) };
  });
  return { ...result, settings };
}

/** Removes a channel from the thread list. @returns {Promise<boolean>} whether it was there */
export async function removeThreadChannel(guildId, channelId) {
  let found = false;
  await updateSettingsWith(guildId, (current) => {
    const list = listOf(current);
    const next = list.filter((e) => e?.channelId !== channelId);
    found = next.length !== list.length;
    return found ? { autoThreadChannels: next } : {};
  });
  return found;
}

/**
 * Adds an announcement channel to the publish list.
 * @returns {Promise<{ added: boolean, settings: object }>} `added` false when it was already there
 * @throws {AutoThreadError} when the list is full
 */
export async function addPublishChannel(guildId, channelId) {
  let added = false;
  const settings = await updateSettingsWith(guildId, (current) => {
    const ids = idsOf(current);
    if (ids.includes(channelId)) return {};
    if (ids.length >= LIMITS.AUTOPUBLISH_CHANNELS) {
      throw new AutoThreadError('LIMIT', 'Limit Reached', `A server can publish in at most **${LIMITS.AUTOPUBLISH_CHANNELS}** channels. Remove one with \`/autothread publish remove\` first.`);
    }
    added = true;
    return { autoPublishChannelIds: [...ids, channelId] };
  });
  return { added, settings };
}

/** Removes a channel from the publish list. @returns {Promise<boolean>} whether it was there */
export async function removePublishChannel(guildId, channelId) {
  let found = false;
  await updateSettingsWith(guildId, (current) => {
    const ids = idsOf(current);
    found = ids.includes(channelId);
    return found ? { autoPublishChannelIds: ids.filter((id) => id !== channelId) } : {};
  });
  return found;
}

/**
 * Drops a (deleted) channel from both lists. Writes nothing when it is in neither.
 * @returns {Promise<boolean>} whether anything changed
 */
export async function dropChannel(guildId, channelId) {
  if (!guildId || !channelId) return false;
  let changed = false;
  await updateSettingsWith(guildId, (current) => {
    const list = listOf(current);
    const ids = idsOf(current);
    const data = {};
    const threads = list.filter((e) => e?.channelId !== channelId);
    if (threads.length !== list.length) data.autoThreadChannels = threads;
    if (ids.includes(channelId)) data.autoPublishChannelIds = ids.filter((id) => id !== channelId);
    changed = Object.keys(data).length > 0;
    return data;
  });
  return changed;
}
