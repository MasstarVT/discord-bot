// Audit logging service (pkg-logging.md §3, §5, §6.1): the log-type map, the event registry, the settings predicates
// and sendLog(). sendLog never throws, resolves on enqueue and posts through a per-channel queue that coalesces
// bursts into messages of up to 10 embeds (about 1 s later). The embed builders live in logEvents.js (pure);
// the hooks that feed this service live in src/plugins/logging.js.
//
// No prisma and no redis here: settings go through settingsService only (a Redis outage falls through to Postgres,
// R146), and everything below is per shard and in memory (a dead-channel memo, the queues, a warning throttle).
import { ChannelType, Client, embedLength } from 'discord.js';
import { getSettingsSafe } from './settingsService.js';
import { warnEmbed } from '../utils/embedBuilder.js';
import { createThrottle } from '../utils/async.js';
import { checkChannelUsable, permissionNames } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { clampText } from '../utils/text.js';

// ── Log type → GuildSettings channel field ────────────────────────────────────

export const LOG_TYPES = Object.freeze({
  MESSAGE:    'messageLogChannelId',
  MEMBER:     'memberLogChannelId',
  SERVER:     'serverLogChannelId',
  VOICE:      'voiceLogChannelId',
  JOIN_LEAVE: 'joinLeaveChannelId',
  MOD:        'modLogChannelId',
});

export const LOG_TYPE_LABELS = Object.freeze({
  MESSAGE:    'Messages',
  MEMBER:     'Members',
  SERVER:     'Server',
  VOICE:      'Voice',
  JOIN_LEAVE: 'Joins & leaves',
  MOD:        'Moderation',
});

// ── Event registry (§3.2) ─────────────────────────────────────────────────────
// key -> { type, label, emoji }. Declared in display order: /logging status and autocomplete follow it.

const ev = (type, emoji, label) => Object.freeze({ type, emoji, label });

export const LOG_EVENTS = Object.freeze({
  messageDelete:     ev('MESSAGE',    '🗑️', 'Message deleted'),
  messageEdit:       ev('MESSAGE',    '✏️', 'Message edited'),
  messageBulkDelete: ev('MESSAGE',    '🧹', 'Messages bulk-deleted'),
  invitePosted:      ev('MESSAGE',    '📨', 'Invite posted'),
  memberRoles:       ev('MEMBER',     '🎭', 'Member roles changed'),
  memberNickname:    ev('MEMBER',     '📝', 'Nickname changed'),
  memberTimeout:     ev('MEMBER',     '⏱️', 'Member timed out / untimed'),
  memberAvatar:      ev('MEMBER',     '🖼️', 'Server avatar changed'),
  userUpdate:        ev('MEMBER',     '👤', 'User updated (name / avatar)'),
  memberBan:         ev('MEMBER',     '🔨', 'Member banned'),
  memberUnban:       ev('MEMBER',     '✅', 'Member unbanned'),
  memberJoin:        ev('JOIN_LEAVE', '📥', 'Member joined'),
  memberLeave:       ev('JOIN_LEAVE', '📤', 'Member left'),
  voiceJoin:         ev('VOICE',      '🔊', 'Voice channel joined'),
  voiceLeave:        ev('VOICE',      '🔇', 'Voice channel left'),
  voiceMove:         ev('VOICE',      '↔️', 'Voice channel switched'),
  voiceState:        ev('VOICE',      '🔔', 'Voice mute / deafen / stream'),
  channelCreate:     ev('SERVER',     '📢', 'Channel created'),
  channelUpdate:     ev('SERVER',     '🛠️', 'Channel updated'),
  channelDelete:     ev('SERVER',     '🗑️', 'Channel deleted'),
  threadCreate:      ev('SERVER',     '🧵', 'Thread created'),
  threadUpdate:      ev('SERVER',     '🛠️', 'Thread updated'),
  threadDelete:      ev('SERVER',     '🗑️', 'Thread deleted'),
  roleCreate:        ev('SERVER',     '➕', 'Role created'),
  roleUpdate:        ev('SERVER',     '🛠️', 'Role updated'),
  roleDelete:        ev('SERVER',     '➖', 'Role deleted'),
  serverUpdate:      ev('SERVER',     '🏠', 'Server edited'),
  emojiCreate:       ev('SERVER',     '😀', 'Emoji created'),
  emojiUpdate:       ev('SERVER',     '😀', 'Emoji updated'),
  emojiDelete:       ev('SERVER',     '😶', 'Emoji deleted'),
  inviteCreate:      ev('SERVER',     '🔗', 'Invite created'),
  inviteDelete:      ev('SERVER',     '🔗', 'Invite deleted / expired'),
});

export const LOG_EVENT_KEYS = Object.freeze(Object.keys(LOG_EVENTS));

/** The events the ignore list (logIgnoredChannelIds) applies to. */
export const MESSAGE_LOG_EVENTS = Object.freeze(['messageDelete', 'messageEdit', 'messageBulkDelete', 'invitePosted']);

export const DEAD_CHANNEL_TTL_MS = 600_000;
export const QUEUE_FLUSH_MS = 1_000;
export const QUEUE_MAX_ITEMS = 100;

const DEAD_CHANNEL_MAX = 5_000;
const TRANSIENT_FETCH_TTL_MS = 30_000; // a failed (non-404) channel fetch is retried after 30 s, not 10 min
const BATCH_MAX_EMBEDS = 10;
const BATCH_MAX_CHARS = 6_000;
const BASE_NEEDS = Object.freeze(['ViewChannel', 'SendMessages', 'EmbedLinks']);
// Unknown Channel, Missing Access, Missing Permissions: the channel is gone or we may not post there.
const HARD_SEND_CODES = new Set([10003, 50001, 50013]);

// ── Settings predicates ───────────────────────────────────────────────────────

/** @param {unknown} key @returns {boolean} */
export function isLogEventKey(key) {
  return typeof key === 'string' && Object.hasOwn(LOG_EVENTS, key);
}

/**
 * Whether an event is switched on. A missing settings row counts as "on" (nothing was disabled);
 * an unknown key is never enabled.
 * @param {{ disabledLogEvents?: string[] }|null|undefined} settings
 * @param {string} key
 * @returns {boolean}
 */
export function isLogEventEnabled(settings, key) {
  if (!isLogEventKey(key)) return false;
  const disabled = settings?.disabledLogEvents;
  return !(Array.isArray(disabled) && disabled.includes(key));
}

/**
 * Pure: the disabledLogEvents array after switching one event (or 'all') on or off. Unknown keys are
 * stripped, the result has no duplicates and follows LOG_EVENT_KEYS order.
 * @param {string[]|null|undefined} disabled
 * @param {string} name a LOG_EVENTS key or 'all'
 * @param {boolean} on true = the event is logged (removed from the disabled list)
 * @returns {string[]}
 */
export function toggleLogEvents(disabled, name, on) {
  if (name === 'all') return on ? [] : [...LOG_EVENT_KEYS];
  const set = new Set((Array.isArray(disabled) ? disabled : []).filter(isLogEventKey));
  if (isLogEventKey(name)) {
    if (on) set.delete(name);
    else set.add(name);
  }
  return LOG_EVENT_KEYS.filter((k) => set.has(k));
}

/**
 * Whether message events in this channel are ignored: the channel itself, its parent (a thread's channel or a
 * channel's category) or the parent's category (a thread in a channel under a category) is on the list.
 * @param {{ logIgnoredChannelIds?: string[] }|null|undefined} settings
 * @param {{ id?: string, parentId?: string|null, parent?: { parentId?: string|null }|null }|null|undefined} channel
 * @returns {boolean}
 */
export function isIgnoredChannel(settings, channel) {
  const list = settings?.logIgnoredChannelIds;
  if (!channel || !Array.isArray(list) || list.length === 0) return false;
  const candidates = [channel.id, channel.parentId, channel.parent?.parentId];
  return candidates.some((id) => id != null && list.includes(id));
}

/**
 * Pure: the GuildSettings fields to write when a channel is deleted: every log channel field (MOD included)
 * equal to the id becomes null and the id leaves logIgnoredChannelIds. Null when nothing changes.
 * @param {object|null|undefined} settings
 * @param {string} channelId
 * @returns {object|null}
 */
export function channelCleanupData(settings, channelId) {
  if (!settings || !channelId) return null;
  const data = {};
  for (const field of Object.values(LOG_TYPES)) {
    if (settings[field] === channelId) data[field] = null;
  }
  const ignored = settings.logIgnoredChannelIds;
  if (Array.isArray(ignored) && ignored.includes(channelId)) {
    data.logIgnoredChannelIds = ignored.filter((id) => id !== channelId);
  }
  return Object.keys(data).length ? data : null;
}

// ── Channel type label helper ─────────────────────────────────────────────────

const CHANNEL_TYPE_LABELS = {
  [ChannelType.GuildText]:          'Text Channel',
  [ChannelType.GuildVoice]:         'Voice Channel',
  [ChannelType.GuildCategory]:      'Category',
  [ChannelType.GuildAnnouncement]:  'Announcement Channel',
  [ChannelType.GuildStageVoice]:    'Stage Channel',
  [ChannelType.GuildForum]:         'Forum Channel',
  [ChannelType.GuildMedia]:         'Media Channel',
  [ChannelType.GuildDirectory]:     'Directory Channel',
  [ChannelType.PublicThread]:       'Public Thread',
  [ChannelType.PrivateThread]:      'Private Thread',
  [ChannelType.AnnouncementThread]: 'Announcement Thread',
};

export function channelTypeLabel(type) {
  return CHANNEL_TYPE_LABELS[type] ?? `Unknown (${type})`;
}

// ── Mention extraction (ghost pings) ──────────────────────────────────────────

/**
 * Returns an array of mention strings found in a message.
 * Skips bot users and (with `excludeUserId`) the message's own author. A partial message gives [].
 * @param {import('discord.js').Message|null} message
 * @param {{ excludeUserId?: string|null }} [opts]
 * @returns {string[]}
 */
export function extractMentions(message, { excludeUserId = null } = {}) {
  if (!message || message.partial) return [];
  const mentions = [];
  for (const [, user] of (message.mentions?.users ?? [])) {
    if (user.bot || (excludeUserId && user.id === excludeUserId)) continue;
    mentions.push(`<@${user.id}> (${user.tag ?? user.username})`);
  }
  for (const [, role] of (message.mentions?.roles ?? [])) {
    mentions.push(`<@&${role.id}> (${role.name})`);
  }
  if (message.mentions?.everyone) mentions.push('`@everyone` / `@here`');
  return mentions;
}

// ── Embed limits ──────────────────────────────────────────────────────────────

const LIMIT = Object.freeze({ title: 256, description: 4096, fieldName: 256, fieldValue: 1024, footer: 2048, author: 256, fields: 25, total: 6000 });

/**
 * Clamps a plain APIEmbed object to Discord's limits and returns a new object: at most 25 fields (24 plus a
 * "… and N more changes" field when there were more), every string cut to its maximum, empty field names and
 * values replaced by a zero-width space, and, while the whole embed exceeds 6000 characters, the last fields
 * are dropped and finally the description is cut.
 * @param {object} data
 * @returns {object}
 */
export function fitEmbedData(data) {
  const out = { ...data };
  if (out.title != null) out.title = clampText(out.title, LIMIT.title);
  if (out.description != null) out.description = clampText(out.description, LIMIT.description);
  if (out.footer?.text != null) out.footer = { ...out.footer, text: clampText(out.footer.text, LIMIT.footer) };
  if (out.author?.name != null) out.author = { ...out.author, name: clampText(out.author.name, LIMIT.author) };

  let fields = Array.isArray(out.fields) ? out.fields.filter((f) => f && typeof f === 'object') : [];
  if (fields.length > LIMIT.fields) {
    const omitted = fields.length - (LIMIT.fields - 1);
    fields = [...fields.slice(0, LIMIT.fields - 1), { name: '…', value: `and ${omitted} more changes` }];
  }
  fields = fields.map((f) => {
    const field = {
      name: clampText(f.name, LIMIT.fieldName) || '​',
      value: clampText(f.value, LIMIT.fieldValue) || '​',
    };
    if (f.inline) field.inline = true;
    return field;
  });
  if (fields.length) out.fields = fields;
  else delete out.fields;

  // Drop the trailing fields (keeping the first) until the total fits; the description goes last.
  while (embedLength(out) > LIMIT.total && Array.isArray(out.fields) && out.fields.length > 1) {
    out.fields = out.fields.slice(0, -1);
  }
  if (embedLength(out) > LIMIT.total && out.description) {
    const over = embedLength(out) - LIMIT.total;
    out.description = clampText(out.description, Math.max(0, out.description.length - over - 1));
  }
  if (embedLength(out) > LIMIT.total && Array.isArray(out.fields)) delete out.fields;
  return out;
}

// ── Options (§5.2) ────────────────────────────────────────────────────────────

/**
 * Normalises sendLog's 4th argument. A Client instance (the legacy `client` argument), null or any
 * non-object gives the defaults.
 * @param {unknown} opts
 * @returns {{ event: string|null, settings: object|null, files: object[] }}
 */
export function normalizeLogOpts(opts) {
  const defaults = { event: null, settings: null, files: [] };
  if (!opts || typeof opts !== 'object' || Array.isArray(opts)) return defaults;
  if (opts instanceof Client || (typeof opts.login === 'function' && typeof opts.isReady === 'function')) return defaults;
  return {
    event: typeof opts.event === 'string' ? opts.event : null,
    settings: opts.settings && typeof opts.settings === 'object' ? opts.settings : null,
    files: Array.isArray(opts.files) ? opts.files.filter(Boolean).slice(0, 1) : [],
  };
}

// ── In-process state (§3.4) ───────────────────────────────────────────────────

/** @type {Map<string, { guildId: string, until: number, reason: string }>} insertion order = age order */
const deadChannels = new Map();
/** @type {Map<string, { guild: object, channel: object|null, items: Array<{ data: object, files: object[], chars: number }>, timer: NodeJS.Timeout|null, flushing: boolean, promise: Promise<void>|null, dropped: number }>} */
const queues = new Map();
let warnThrottle = createThrottle(10 * 60_000);
const warnedTypes = new Set();

function markDead(guildId, channelId, reason, ttlMs = DEAD_CHANNEL_TTL_MS) {
  deadChannels.delete(channelId); // refresh the age order
  deadChannels.set(channelId, { guildId, until: Date.now() + ttlMs, reason });
  while (deadChannels.size > DEAD_CHANNEL_MAX) deadChannels.delete(deadChannels.keys().next().value);
  if (ttlMs === DEAD_CHANNEL_TTL_MS && warnThrottle(channelId)) {
    logger.warn(`Log channel ${channelId} in guild ${guildId} is unusable (${reason}); log posts to it are paused for 10 minutes.`);
  }
  dropQueued(channelId);
}

function dropQueued(channelId) {
  const q = queues.get(channelId);
  if (!q) return;
  q.items.length = 0;
  q.dropped = 0;
  if (!q.flushing) {
    if (q.timer) clearTimeout(q.timer);
    queues.delete(channelId);
  }
}

/** True while the channel is memoised as unusable. Expired entries are removed lazily. */
export function isLogChannelDead(channelId) {
  const entry = deadChannels.get(channelId);
  if (!entry) return false;
  if (entry.until > Date.now()) return true;
  deadChannels.delete(channelId);
  return false;
}

/**
 * Forgets the dead-channel memo of one channel, or of every channel of a guild (when channelId is null).
 * Called when permissions or settings may have been fixed.
 * @param {string} guildId
 * @param {string|null} [channelId]
 */
export function forgetDeadChannels(guildId, channelId = null) {
  if (channelId) {
    deadChannels.delete(channelId);
    return;
  }
  for (const [id, entry] of deadChannels) {
    if (entry.guildId === guildId) deadChannels.delete(id);
  }
}

/**
 * Marks a deleted log channel as dead WITHOUT the warning log, and drops its queued items.
 * @param {string} guildId
 * @param {string} channelId
 */
export function markLogChannelDeleted(guildId, channelId) {
  deadChannels.delete(channelId);
  deadChannels.set(channelId, { guildId, until: Date.now() + DEAD_CHANNEL_TTL_MS, reason: 'deleted' });
  while (deadChannels.size > DEAD_CHANNEL_MAX) deadChannels.delete(deadChannels.keys().next().value);
  dropQueued(channelId);
}

/**
 * Whether the bot may post embeds in a channel. When the bot's own member is not cached the
 * check is skipped (a cache miss is not a denial, rule 20): the send itself reports 50013.
 * @returns {{ ok: boolean, missing: string[], reason?: string }}
 */
function usable(guild, channel, needs = BASE_NEEDS) {
  if (!guild?.members?.me) return { ok: Boolean(channel?.isTextBased?.()), missing: [] };
  return checkChannelUsable(channel, { needs: [...needs] });
}

// ── Queue (§6.1) ──────────────────────────────────────────────────────────────

function schedule(channelId, q) {
  if (q.timer || q.flushing) return;
  q.timer = setTimeout(() => { void flush(channelId); }, QUEUE_FLUSH_MS);
  q.timer.unref?.();
}

function enqueue(guild, channel, item) {
  const channelId = channel.id;
  let q = queues.get(channelId);
  if (!q) {
    q = { guild, channel, items: [], timer: null, flushing: false, promise: null, dropped: 0 };
    queues.set(channelId, q);
  }
  q.guild = guild;
  q.channel = channel;
  if (q.items.length >= QUEUE_MAX_ITEMS) {
    q.items.shift();
    q.dropped++;
  }
  q.items.push(item);
  schedule(channelId, q);
}

function takeBatch(q) {
  const first = q.items.shift();
  const batch = [first];
  if (first.files.length) return batch; // an item with an attachment goes alone
  let chars = first.chars;
  while (q.items.length && !q.items[0].files.length && batch.length < BATCH_MAX_EMBEDS && chars + q.items[0].chars <= BATCH_MAX_CHARS) {
    const next = q.items.shift();
    chars += next.chars;
    batch.push(next);
  }
  return batch;
}

async function drain(channelId, q) {
  while (q.items.length) {
    if (isLogChannelDead(channelId)) { q.items.length = 0; q.dropped = 0; break; }

    // Re-resolve through the cache and fall back to the channel captured at enqueue time: a cache
    // miss alone never marks a channel dead (rule 20).
    const channel = q.guild?.channels?.cache?.get(channelId) ?? q.channel;
    const u = usable(q.guild, channel);
    if (!u.ok) {
      markDead(q.guild?.id ?? '', channelId, u.missing.length ? 'no-permission' : 'not-text');
      break;
    }

    if (q.dropped > 0) {
      const n = q.dropped;
      q.dropped = 0;
      const notice = warnEmbed('Log entries dropped',
        `${n} log entr${n === 1 ? 'y was' : 'ies were'} dropped because events arrived faster than Discord allows posting.`);
      try {
        await channel.send({ embeds: [notice.toJSON()], allowedMentions: { parse: [] } });
      } catch (err) {
        if (HARD_SEND_CODES.has(err?.code)) { markDead(q.guild?.id ?? '', channelId, String(err.code)); break; }
        logger.warn(`sendLog: failed to post the dropped-entries notice to ${channelId}: ${err?.message ?? err}`);
      }
    }

    const batch = takeBatch(q);
    try {
      await channel.send({ embeds: batch.map((i) => i.data), files: batch[0].files, allowedMentions: { parse: [] } });
    } catch (err) {
      if (HARD_SEND_CODES.has(err?.code)) {
        markDead(q.guild?.id ?? '', channelId, String(err.code));
        break;
      }
      // Dropped, not retried: a retry could duplicate a message that did arrive.
      logger.warn(`sendLog: failed to post ${batch.length} log embed(s) to ${channelId}: ${err?.message ?? err}`);
    }
  }
}

function flush(channelId) {
  const q = queues.get(channelId);
  if (!q) return Promise.resolve();
  if (q.timer) { clearTimeout(q.timer); q.timer = null; }
  if (q.flushing) return q.promise ?? Promise.resolve();
  q.flushing = true;
  q.promise = (async () => {
    try {
      await drain(channelId, q);
    } catch (err) {
      logger.error(`sendLog: flush of ${channelId} failed`, err instanceof Error ? err : new Error(String(err)));
    } finally {
      q.flushing = false;
      q.promise = null;
      if (queues.get(channelId) === q) {
        if (q.items.length) schedule(channelId, q); // entries arrived while posting
        else queues.delete(channelId);
      }
    }
  })();
  return q.promise;
}

/**
 * Flushes the queue of one channel (or of every channel) now. Used by tests and after a test post. Never throws.
 * @param {string|null} [channelId]
 * @returns {Promise<void>}
 */
export async function flushLogQueue(channelId = null) {
  try {
    if (channelId) await flush(channelId);
    else await Promise.all([...queues.keys()].map((id) => flush(id)));
  } catch (err) {
    logger.error('flushLogQueue failed', err instanceof Error ? err : new Error(String(err)));
  }
}

/** Clears the dead-channel memo, the queues (and their timers) and the warning throttle. Tests only. */
export function _resetLoggingStateForTests() {
  deadChannels.clear();
  for (const q of queues.values()) if (q.timer) clearTimeout(q.timer);
  queues.clear();
  warnedTypes.clear();
  warnThrottle = createThrottle(10 * 60_000);
}

// ── Channel health (§5.3) ─────────────────────────────────────────────────────

/**
 * Cache-only, synchronous health of a configured log channel, for /logging replies.
 * @param {import('discord.js').Guild} guild
 * @param {string|null|undefined} channelId
 * @returns {{ state: 'unset'|'ok'|'missing'|'no-permission'|'not-text', missing: string[], text: string }}
 */
export function getLogChannelHealth(guild, channelId) {
  if (!channelId) return { state: 'unset', missing: [], text: '' };
  const channel = guild?.channels?.cache?.get(channelId);
  if (!channel) return { state: 'missing', missing: [], text: 'channel not found (deleted?)' };
  if (!channel.isTextBased?.()) return { state: 'not-text', missing: [], text: 'not a text channel' };
  const u = usable(guild, channel);
  if (!u.ok) {
    return u.missing.length
      ? { state: 'no-permission', missing: u.missing, text: `missing ${permissionNames(u.missing).join(', ')}` }
      : { state: 'not-text', missing: [], text: 'not a text channel' };
  }
  return { state: 'ok', missing: [], text: '' };
}

// ── Core dispatch (§5.2) ──────────────────────────────────────────────────────

/**
 * Posts a log embed to the channel configured for `logType`. Never throws. Delivery is asynchronous: the
 * promise resolves once the embed is queued; the queue posts it within about a second, up to 10 embeds per
 * message. For every type except MOD it is a no-op when logging is off, the event is disabled, no channel is
 * set or the channel is unusable. MOD (the moderation log) ignores the logging toggles (decision D1).
 *
 * @param {import('discord.js').Guild} guild
 * @param {keyof typeof LOG_TYPES} logType e.g. 'MESSAGE', 'MEMBER'
 * @param {import('discord.js').EmbedBuilder|object} embed
 * @param {{ event?: string, settings?: object, files?: import('discord.js').AttachmentBuilder[] }|import('discord.js').Client} [opts]
 *   a Client (the legacy 4th argument) is accepted and ignored
 * @returns {Promise<void>}
 */
export async function sendLog(guild, logType, embed, opts) {
  try {
    if (!guild?.id) return;
    const field = Object.hasOwn(LOG_TYPES, logType) ? LOG_TYPES[logType] : null;
    if (!field) {
      if (!warnedTypes.has(String(logType))) {
        warnedTypes.add(String(logType));
        logger.warn(`sendLog: unknown log type "${logType}"`);
      }
      return;
    }
    const o = normalizeLogOpts(opts);

    const settings = o.settings ?? await getSettingsSafe(guild.id);
    if (!settings) return;
    if (logType !== 'MOD') {
      if (settings.loggingEnabled !== true) return;
      if (o.event && !isLogEventEnabled(settings, o.event)) return;
    }

    const channelId = settings[field];
    if (!channelId) return;
    if (isLogChannelDead(channelId)) return;

    let channel = guild.channels?.cache?.get(channelId);
    if (!channel) {
      // At most one REST call per dead channel per 10 minutes (R147). Only a definite "gone" answer marks
      // the channel dead; any other failure just skips this entry (a cache miss is not a deletion).
      try {
        channel = await guild.channels.fetch(channelId);
      } catch (err) {
        if (HARD_SEND_CODES.has(err?.code)) markDead(guild.id, channelId, 'missing');
        else markDead(guild.id, channelId, 'fetch-failed', TRANSIENT_FETCH_TTL_MS);
        return;
      }
      if (!channel) { markDead(guild.id, channelId, 'missing'); return; }
    }

    const u = usable(guild, channel);
    if (!u.ok) {
      markDead(guild.id, channelId, u.missing.length ? 'no-permission' : 'not-text');
      return;
    }

    const raw = typeof embed?.toJSON === 'function' ? embed.toJSON() : (embed && typeof embed === 'object' ? { ...embed } : null);
    if (!raw || !(raw.title || raw.description || (Array.isArray(raw.fields) && raw.fields.length))) return;
    const data = fitEmbedData(raw);

    let files = o.files;
    if (files.length && !usable(guild, channel, ['AttachFiles']).ok) {
      files = [];
      const fields = data.fields ?? [];
      if (fields.length < LIMIT.fields) {
        data.fields = [...fields, { name: 'Attachment omitted', value: 'I need the **Attach Files** permission in this channel to attach it.' }];
      }
    }

    enqueue(guild, channel, { data, files, chars: embedLength(data) });
  } catch (err) {
    logger.error('sendLog failed', err instanceof Error ? err : new Error(String(err)));
  }
}
