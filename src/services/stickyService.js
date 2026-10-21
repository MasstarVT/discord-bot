// Sticky messages: a message kept at the bottom of a channel. When members talk in a sticky channel the bot waits for the
// channel to go quiet, deletes its previous sticky message and posts it again. One StickyMessage row per channel (text
// and/or a saved embed's embed), one pending timer and one re-send at a time per channel (this shard: a guild lives on
// one shard). Hook handling and the modal: src/plugins/sticky.js, the command: src/commands/admin/sticky.js.
import { Prisma } from '@prisma/client';
import prisma from '../database/client.js';
import { LIMITS } from '../config/constants.js';
import { isModuleEnabled } from '../config/modules.js';
import { ensureGuildSettings, getSettingsSafe } from './settingsService.js';
import { resolveDefaultColor } from './embedService.js';
import { createKeyedMutex, createThrottle } from '../utils/async.js';
import {
  LIMITS as MESSAGE_LIMITS, normalizeTemplate, renderTemplate, validateTemplate,
} from '../utils/customEmbed.js';
import { checkChannelUsable, missingBotPermissions, permissionNames } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';

export const STICKY = Object.freeze({
  // A re-send waits until the channel has been quiet this long...
  QUIET_MS: 3_000,
  // ...but never longer than this after the first message that asked for it, so a busy channel still gets it.
  MAX_WAIT_MS: 15_000,
  // How long a guild's list of sticky channels is remembered (it is dropped at once by every change made here).
  MEMO_TTL_MS: 60_000,
  MEMO_MAX_GUILDS: 2_000,
  // A message the bot deleted itself is remembered this long, so its messageDelete event costs no query.
  SELF_DELETE_TTL_MS: 60_000,
  // A channel the bot cannot post in is complained about in the log at most this often.
  PERMS_WARN_MS: 60 * 60_000,
});

// Unknown channel, missing access or permission: the target is gone, nothing to retry.
const GONE_CODES = new Set([10003, 50001, 50013]);
const UNKNOWN_MESSAGE = 10008;

/** An admin-facing refusal: `title` and the message are shown as they are. */
export class StickyError extends Error {
  constructor(code, title, message) {
    super(message);
    this.name = 'StickyError';
    this.code = code;
    this.title = title;
  }
}

// ── Re-send scheduler ─────────────────────────────────────────────────────────

/**
 * Debounces re-sends per channel. `touch` (a message arrived) arms one unref'd timer that fires `quietMs` after the last
 * touch, but at most `maxWaitMs` after the first one. While `run` is in flight another touch only marks the channel
 * dirty; the next window opens when `run` finishes. `run` never makes the scheduler reject.
 * @param {{ quietMs?: number, maxWaitMs?: number, run: (id: string, arg: any) => Promise<unknown>|unknown, now?: () => number }} opts
 */
export function createScheduler({ quietMs = STICKY.QUIET_MS, maxWaitMs = STICKY.MAX_WAIT_MS, run, now = Date.now }) {
  /** @type {Map<string, { firstAt: number, timer: NodeJS.Timeout|null, running: boolean, dirty: boolean, arg: any }>} */
  const states = new Map();

  function arm(id, st) {
    if (st.timer) clearTimeout(st.timer);
    const delay = Math.max(0, Math.min(quietMs, st.firstAt + maxWaitMs - now()));
    st.timer = setTimeout(() => fire(id, st), delay);
    st.timer.unref?.();
  }

  function fire(id, st) {
    st.timer = null;
    st.running = true;
    let result;
    try {
      result = Promise.resolve(run(id, st.arg));
    } catch (err) {
      result = Promise.reject(err);
    }
    result
      .catch((err) => logger.warn(`[sticky] re-send in ${id} failed: ${err?.message ?? err}`))
      .then(() => {
        st.running = false;
        if (states.get(id) !== st) return; // cancelled meanwhile
        if (st.dirty) {
          st.dirty = false;
          st.firstAt = now();
          arm(id, st);
        } else {
          states.delete(id);
        }
      });
  }

  return {
    /** A message arrived in the channel. `arg` is handed to `run` (the latest one wins). */
    touch(id, arg) {
      let st = states.get(id);
      if (!st) {
        st = { firstAt: now(), timer: null, running: false, dirty: false, arg };
        states.set(id, st);
      }
      st.arg = arg;
      if (st.running) st.dirty = true;
      else arm(id, st);
    },
    /** Drops the pending timer (an in-flight run still finishes). @returns {boolean} whether there was anything */
    cancel(id) {
      const st = states.get(id);
      if (!st) return false;
      if (st.timer) clearTimeout(st.timer);
      states.delete(id);
      return true;
    },
    has: (id) => states.has(id),
    get size() { return states.size; },
    clear() {
      for (const st of states.values()) if (st.timer) clearTimeout(st.timer);
      states.clear();
    },
  };
}

// One re-send (or admin change) at a time per channel.
const lock = createKeyedMutex();
/** Runs `fn` while holding the channel's sticky lock. */
export const runExclusive = (channelId, fn) => lock.run(channelId, fn);

const scheduler = createScheduler({
  run: (channelId, { client, guildId }) => runExclusive(channelId, () => resendSticky(client, guildId, channelId)),
});

/** A member message arrived in a sticky channel: schedule its re-send. */
export function noteMessage(client, guildId, channelId) {
  scheduler.touch(channelId, { client, guildId });
}

/** Drops the channel's pending re-send. */
export function cancelPending(channelId) {
  return scheduler.cancel(channelId);
}

/** Tests only: the scheduler's state. */
export const _scheduler = scheduler;

// ── Sticky channel cache (in memory, TTL) ─────────────────────────────────────

/** @type {Map<string, { at: number, channels: Map<string, boolean> }>} guildId -> channelId -> enabled */
const memo = new Map();
/** @type {Map<string, Promise<Map<string, boolean>|null>>} */
const loading = new Map();
/** guildId -> sequence number of its last invalidation; a load that began before it is not remembered. */
const invalidatedAt = new Map();
let sequence = 0;

/**
 * The guild's sticky channels from the memo (channelId -> enabled), without any I/O. Null when unknown or expired.
 * @param {string} guildId
 * @returns {Map<string, boolean>|null}
 */
export function peekChannels(guildId) {
  const hit = memo.get(guildId);
  if (!hit) return null;
  if (Date.now() - hit.at >= STICKY.MEMO_TTL_MS) {
    memo.delete(guildId);
    return null;
  }
  return hit.channels;
}

function remember(guildId, channels) {
  memo.delete(guildId);
  memo.set(guildId, { at: Date.now(), channels });
  if (memo.size > STICKY.MEMO_MAX_GUILDS) {
    const now = Date.now();
    for (const [id, v] of memo) if (now - v.at >= STICKY.MEMO_TTL_MS) memo.delete(id);
    while (memo.size > STICKY.MEMO_MAX_GUILDS) memo.delete(memo.keys().next().value);
  }
}

/**
 * The guild's sticky channels (channelId -> enabled): the memo, else one query (shared by concurrent callers). An empty
 * map is remembered too. Null when the database failed (not remembered).
 * @param {string} guildId
 * @returns {Promise<Map<string, boolean>|null>}
 */
export async function getChannels(guildId) {
  const hit = peekChannels(guildId);
  if (hit) return hit;
  const pending = loading.get(guildId);
  if (pending) return pending;
  const startedAt = sequence;
  const promise = (async () => {
    try {
      const rows = await prisma.stickyMessage.findMany({ where: { guildId }, select: { channelId: true, enabled: true } });
      const channels = new Map(rows.map((r) => [r.channelId, r.enabled === true]));
      if ((invalidatedAt.get(guildId) ?? 0) <= startedAt) remember(guildId, channels); // not when a change came meanwhile
      return channels;
    } catch (err) {
      logger.warn(`[sticky] ${guildId}: could not load the sticky channels: ${err?.message ?? err}`);
      return null;
    }
  })();
  loading.set(guildId, promise);
  const clear = () => { if (loading.get(guildId) === promise) loading.delete(guildId); };
  promise.then(clear, clear);
  return promise;
}

/** Forgets the guild's sticky channels (after any change). */
export function invalidate(guildId) {
  memo.delete(guildId);
  loading.delete(guildId);
  invalidatedAt.set(guildId, ++sequence);
  if (invalidatedAt.size > STICKY.MEMO_MAX_GUILDS * 2) invalidatedAt.delete(invalidatedAt.keys().next().value);
}

// ── Rendering ─────────────────────────────────────────────────────────────────

/**
 * What to keep of a saved embed's template: its embed and link buttons (its text is the sticky's own text).
 * @param {unknown} template
 * @returns {{ embed: object|null, buttons: object[] }|null} null when it has neither
 */
export function embedPart(template) {
  const t = normalizeTemplate(template);
  return t.embed || t.buttons.length ? { embed: t.embed, buttons: t.buttons } : null;
}

/**
 * The message to post for a row: text through the placeholders ({server}, {channel}; nobody is "the user" here), the
 * embed, and no pings whatsoever. Null when nothing is left to show.
 * @param {{ content?: string|null, embed?: unknown }} row
 * @param {{ guild?: object, channel?: object, settings?: object|null }} ctx
 */
export function renderSticky(row, { guild, channel, settings } = {}) {
  const part = row.embed ? normalizeTemplate(row.embed) : { embed: null, buttons: [] };
  return renderTemplate(
    { content: row.content ?? null, embed: part.embed, buttons: part.buttons },
    { guild, channel },
    { defaultColor: resolveDefaultColor(settings), allowedMentions: { parse: [] } },
  );
}

/** The permissions the bot needs to keep a sticky in a channel. Deleting its own old message needs none. */
export function stickyNeeds(hasEmbed) {
  return hasEmbed ? ['ViewChannel', 'SendMessages', 'EmbedLinks'] : ['ViewChannel', 'SendMessages'];
}

// ── Re-sending ────────────────────────────────────────────────────────────────

/** @type {Map<string, number>} ids of sticky messages this process deleted itself */
const selfDeleted = new Map();

function markSelfDeleted(id) {
  const now = Date.now();
  if (selfDeleted.size > 200) {
    for (const [k, t] of selfDeleted) if (now - t >= STICKY.SELF_DELETE_TTL_MS) selfDeleted.delete(k);
  }
  selfDeleted.set(id, now);
}

/**
 * True when the bot deleted this message itself (once: the mark is consumed).
 * @param {string} messageId
 */
export function consumeSelfDeleted(messageId) {
  const t = selfDeleted.get(messageId);
  if (t === undefined) return false;
  selfDeleted.delete(messageId);
  return Date.now() - t < STICKY.SELF_DELETE_TTL_MS;
}

/** Deletes a sticky message, ignoring one that is already gone. @returns {Promise<boolean>} whether it was deleted */
async function deleteMessage(channel, messageId) {
  if (!channel?.messages || !messageId) return false;
  markSelfDeleted(messageId);
  try {
    await channel.messages.delete(messageId);
    return true;
  } catch (err) {
    selfDeleted.delete(messageId);
    if (err?.code !== UNKNOWN_MESSAGE) logger.warn(`[sticky] ${channel.guildId ?? ''}: could not delete ${messageId}: ${err?.message ?? err}`);
    return false;
  }
}

const permsWarn = createThrottle(STICKY.PERMS_WARN_MS);

/**
 * Deletes the channel's previous sticky message and posts it again, saving the new message id. Call it under
 * `runExclusive(channelId, ...)`. Never throws on Discord errors; a database error does throw.
 * @param {import('discord.js').Client} client
 * @param {string} guildId
 * @param {string} channelId
 * @param {{ force?: boolean }} [opts] `force`: also when the sticky already is the channel's last message
 * @returns {Promise<'sent'|'current'|'missing'|'paused'|'off'|'gone'|'perms'|'empty'|'failed'>}
 */
export async function resendSticky(client, guildId, channelId, { force = false } = {}) {
  const row = await prisma.stickyMessage.findUnique({ where: { channelId } });
  if (!row || row.guildId !== guildId) {
    invalidate(guildId);
    return 'missing';
  }
  if (!row.enabled) return 'paused';
  const guild = client?.guilds?.cache?.get(guildId);
  const channel = guild?.channels?.cache?.get(channelId);
  if (!channel) return 'gone'; // channelDelete drops the row

  const settings = await getSettingsSafe(guildId);
  if (!settings) return 'failed';
  if (!isModuleEnabled(settings, 'sticky')) return 'off';

  const payload = renderSticky(row, { guild, channel, settings });
  if (!payload) {
    logger.warn(`[sticky] ${guildId}: the sticky in ${channelId} renders to nothing`);
    return 'empty';
  }
  const missing = missingBotPermissions(channel, stickyNeeds(payload.embeds.length > 0));
  if (missing.length) {
    if (permsWarn(channelId)) logger.warn(`[sticky] ${guildId}: cannot post in ${channelId}: missing ${missing.join(', ')}`);
    return 'perms';
  }
  if (!force && row.lastMessageId && channel.lastMessageId === row.lastMessageId) return 'current';

  if (row.lastMessageId) await deleteMessage(channel, row.lastMessageId);
  let sent;
  try {
    sent = await channel.send(payload);
  } catch (err) {
    if (row.lastMessageId) {
      await prisma.stickyMessage.updateMany({ where: { channelId, lastMessageId: row.lastMessageId }, data: { lastMessageId: null } });
    }
    if (GONE_CODES.has(err?.code)) return err.code === 50013 ? 'perms' : 'gone';
    logger.warn(`[sticky] ${guildId}: could not post in ${channelId}: ${err?.message ?? err}`);
    return 'failed';
  }
  const { count } = await prisma.stickyMessage.updateMany({ where: { channelId, guildId }, data: { lastMessageId: sent.id } });
  if (count === 0) {
    // Removed while the message was on its way: do not leave it behind.
    await deleteMessage(channel, sent.id);
    return 'missing';
  }
  return 'sent';
}

// ── Admin operations ──────────────────────────────────────────────────────────

const guildLock = createKeyedMutex();

/** The guild's sticky rows, oldest first. */
export function listStickies(guildId) {
  return prisma.stickyMessage.findMany({ where: { guildId }, orderBy: { createdAt: 'asc' } });
}

/** One channel's sticky row, or null. */
export function getSticky(guildId, channelId) {
  return prisma.stickyMessage.findFirst({ where: { guildId, channelId } });
}

/** How many channels have a sticky in the guild. */
export function countStickies(guildId) {
  return prisma.stickyMessage.count({ where: { guildId } });
}

/**
 * Creates or replaces a channel's sticky, enables it and posts it now (the old message is deleted first).
 * `keepEmbed` keeps the current embed (the text form edits the text only); otherwise `embed` is stored.
 * @param {{ client: object, guild: object, channel: object, userId: string, content?: string|null,
 *   embed?: { embed: object|null, buttons: object[] }|null, keepEmbed?: boolean }} args
 * @returns {Promise<{ created: boolean, hasEmbed: boolean, status: string }>}
 * @throws {StickyError} for a refusal worth showing; a database error
 */
export async function setSticky({ client, guild, channel, userId, content = null, embed = null, keepEmbed = false }) {
  const guildId = guild.id;
  const channelId = channel.id;
  const text = typeof content === 'string' && content.trim() !== '' ? content : null;
  if (text && text.length > MESSAGE_LIMITS.content) {
    throw new StickyError('TOO_LONG', 'Message Too Long', `The text is ${text.length} characters; the limit is ${MESSAGE_LIMITS.content}.`);
  }

  const existing = await getSticky(guildId, channelId);
  const part = keepEmbed ? (existing?.embed ? embedPart(existing.embed) : null) : embed;
  const check = validateTemplate({ content: text, embed: part?.embed ?? null, buttons: part?.buttons ?? [] }, { requireSomething: true });
  if (!check.ok) {
    throw new StickyError('INVALID', 'Nothing To Stick', check.errors.slice(0, 3).join('\n'));
  }

  const usable = checkChannelUsable(channel, { needs: stickyNeeds(Boolean(part?.embed)) });
  if (!usable.ok) throw new StickyError('PERMS', 'Cannot Use That Channel', `${usable.reason} Fix my permissions there, or pick another channel.`);

  await ensureGuildSettings(guildId);
  const created = await guildLock.run(guildId, async () => {
    const row = await getSticky(guildId, channelId);
    if (!row && (await countStickies(guildId)) >= LIMITS.STICKY_MESSAGES) {
      throw new StickyError('LIMIT', 'Limit Reached', `A server can have at most **${LIMITS.STICKY_MESSAGES}** sticky messages. Remove one with \`/sticky remove\` first.`);
    }
    const data = { content: text, embed: part ? part : Prisma.DbNull, enabled: true };
    await prisma.stickyMessage.upsert({
      where: { channelId },
      create: { guildId, channelId, createdById: userId, ...data },
      update: data,
    });
    return !row;
  });
  invalidate(guildId);
  cancelPending(channelId);

  let status;
  try {
    status = await runExclusive(channelId, () => resendSticky(client, guildId, channelId, { force: true }));
  } catch (err) {
    logger.warn(`[sticky] ${guildId}: posting the new sticky in ${channelId} failed: ${err?.message ?? err}`);
    status = 'failed';
  }
  return { created, hasEmbed: Boolean(part?.embed), status };
}

/**
 * Removes a channel's sticky and deletes its message.
 * @returns {Promise<object|null>} the removed row, or null when the channel had none
 */
export async function removeSticky(client, guildId, channelId) {
  cancelPending(channelId);
  return runExclusive(channelId, async () => {
    const row = await getSticky(guildId, channelId);
    if (!row) return null;
    await prisma.stickyMessage.deleteMany({ where: { guildId, channelId } });
    invalidate(guildId);
    const channel = client?.guilds?.cache?.get(guildId)?.channels?.cache?.get(channelId);
    if (channel && row.lastMessageId) await deleteMessage(channel, row.lastMessageId);
    return row;
  });
}

/**
 * Pauses (the posted message is deleted) or resumes (it is posted again at once) a channel's sticky.
 * @returns {Promise<{ found: boolean, changed: boolean, status: string|null }>}
 */
export async function setStickyEnabled(client, guildId, channelId, enabled) {
  cancelPending(channelId);
  return runExclusive(channelId, async () => {
    const row = await getSticky(guildId, channelId);
    if (!row) return { found: false, changed: false, status: null };
    const changed = row.enabled !== enabled;
    if (changed) {
      await prisma.stickyMessage.updateMany({
        where: { guildId, channelId },
        data: enabled ? { enabled: true } : { enabled: false, lastMessageId: null },
      });
      invalidate(guildId);
    }
    if (!enabled) {
      const channel = client?.guilds?.cache?.get(guildId)?.channels?.cache?.get(channelId);
      if (channel && row.lastMessageId) await deleteMessage(channel, row.lastMessageId);
      return { found: true, changed, status: 'paused' };
    }
    return { found: true, changed, status: await resendSticky(client, guildId, channelId) };
  });
}

// ── Cleanup (hooks) ───────────────────────────────────────────────────────────

/**
 * The sticky message of a channel was deleted by someone else: forget its id. Only a row that still points at this
 * message changes, so the delete of a replaced message (ours) never clears the new one.
 */
export async function clearLastMessage(guildId, channelId, messageId) {
  if (!guildId || !channelId || !messageId || consumeSelfDeleted(messageId)) return 0;
  const { count } = await prisma.stickyMessage.updateMany({
    where: { guildId, channelId, lastMessageId: messageId },
    data: { lastMessageId: null },
  });
  return count;
}

/** A channel was deleted: its sticky row goes with it. */
export async function dropChannel(guildId, channelId) {
  if (!guildId || !channelId) return 0;
  cancelPending(channelId);
  const { count } = await prisma.stickyMessage.deleteMany({ where: { guildId, channelId } });
  if (count > 0) invalidate(guildId);
  return count;
}

/** Shows which of the bot's permissions a channel lacks, for `/sticky list`. @returns {string[]} permission names */
export function missingFor(channel, hasEmbed) {
  return permissionNames(missingBotPermissions(channel, stickyNeeds(hasEmbed)));
}
