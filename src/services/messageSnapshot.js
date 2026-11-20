// Redis message snapshots for the audit log. discord.js only keeps a few hours of messages in memory (and none after
// a restart), so a delete or an edit of an older message had no content to show. While the logging module is on, the
// messageCreate hook stores a small JSON copy of each human message here (key msgsnap:<channelId>:<messageId>, TTL
// MESSAGE_SNAPSHOT_TTL_DAYS, default 7), and the delete / edit / bulk-delete logs read it back when the message is partial.
// Redis is cache only: every function fails open (a miss, a write that did not happen) and never rejects.
import { LIMITS } from '../config/constants.js';
import logger from '../utils/logger.js';
import { clampText, effectiveText } from '../utils/text.js';
import { isIgnoredChannel, isLogChannelDead, isLogEventEnabled } from './loggingService.js';
import { cache } from './redis.js';
import { isModuleEnabled } from './settingsService.js';

const DEFAULT_TTL_DAYS = 7;
const MAX_TTL_DAYS = 30;
const URL_MAX = 1_000;

export const snapshotKey = (channelId, messageId) => `msgsnap:${channelId}:${messageId}`;

/** TTL in seconds from MESSAGE_SNAPSHOT_TTL_DAYS (default 7, clamped to 1-30 days). */
export function snapshotTtlSeconds() {
  const n = Number(process.env.MESSAGE_SNAPSHOT_TTL_DAYS);
  const days = Number.isFinite(n) && n > 0 ? Math.min(MAX_TTL_DAYS, Math.max(1, n)) : DEFAULT_TTL_DAYS;
  return Math.round(days * 86_400);
}

/**
 * Whether a new message should be snapshotted: logging module on, messageDelete or messageEdit logged to a message log
 * channel that is set and not known dead (the channel sendLog would post to), channel not ignored, a human author (no
 * bot, no webhook) and something to remember. Pure and synchronous (hook filters use it).
 */
export function shouldSnapshot(settings, message) {
  if (!settings || !message?.author || message.author.bot || message.webhookId) return false;
  if (!isModuleEnabled(settings, 'logging')) return false;
  if (!isLogEventEnabled(settings, 'messageDelete') && !isLogEventEnabled(settings, 'messageEdit')) return false;
  if (!settings.messageLogChannelId || isLogChannelDead(settings.messageLogChannelId)) return false;
  if (isIgnoredChannel(settings, message.channel ?? { id: message.channelId })) return false;
  return Boolean(message.content || message.attachments?.size || message.embeds?.length || message.messageSnapshots?.size);
}

/** The JSON stored for a message. Never throws on odd input. */
export function buildSnapshot(message) {
  const a = message.author;
  let avatar = null;
  try { avatar = a.displayAvatarURL?.() ?? null; } catch { /* no avatar */ }
  const attachments = [...(message.attachments?.values?.() ?? [])]
    .slice(0, LIMITS.MESSAGE_SNAPSHOT_ATTACHMENTS)
    .map((x) => ({
      name: clampText(String(x?.name ?? 'file'), 100),
      size: Number.isFinite(Number(x?.size)) ? Number(x.size) : 0,
      url: typeof x?.url === 'string' && x.url.length <= URL_MAX ? x.url : '',
    }));
  return {
    id: message.id,
    channelId: message.channelId ?? message.channel?.id,
    content: clampText(effectiveText(message), LIMITS.MESSAGE_SNAPSHOT_CHARS),
    author: {
      id: a.id,
      username: a.username ?? null,
      tag: a.tag ?? a.username ?? null,
      globalName: a.globalName ?? null,
      avatar,
      bot: Boolean(a.bot),
    },
    createdTimestamp: Number.isFinite(message.createdTimestamp) ? message.createdTimestamp : null,
    attachments,
    embeds: message.embeds?.length ?? 0,
  };
}

/** Writes (or overwrites) the snapshot; resolves true when written, never rejects. */
export async function storeSnapshot(message) {
  try {
    const snap = buildSnapshot(message);
    if (!snap.channelId || !snap.id) return false;
    return Boolean(await cache.set(snapshotKey(snap.channelId, snap.id), snap, snapshotTtlSeconds()));
  } catch (err) {
    logger.debug(`messageSnapshot: store failed: ${err?.message ?? err}`);
    return false;
  }
}

const valid = (s) => s && typeof s === 'object' && s.author?.id && s.id;

/** @returns {Promise<object|null>} the snapshot, or null (miss, Redis down, bad data) */
export async function loadSnapshot(channelId, messageId) {
  if (!channelId || !messageId) return null;
  try {
    const s = await cache.get(snapshotKey(channelId, messageId));
    return valid(s) ? s : null;
  } catch (err) {
    logger.debug(`messageSnapshot: load failed: ${err?.message ?? err}`);
    return null;
  }
}

/** One batched read. @returns {Promise<Map<string, object>>} messageId -> snapshot for the hits */
export async function loadSnapshots(channelId, messageIds) {
  const out = new Map();
  const ids = [...messageIds];
  if (!channelId || !ids.length) return out;
  try {
    const found = await cache.mget(ids.map((id) => snapshotKey(channelId, id)));
    ids.forEach((id, i) => { if (valid(found?.[i])) out.set(id, found[i]); });
  } catch (err) {
    logger.debug(`messageSnapshot: batch load failed: ${err?.message ?? err}`);
  }
  return out;
}

/** Removes snapshots (after a delete). Never rejects. */
export async function forgetSnapshots(channelId, messageIds) {
  try {
    if (!channelId) return;
    const ids = [messageIds].flat().filter(Boolean);
    if (ids.length) await cache.del(...ids.map((id) => snapshotKey(channelId, id)));
  } catch (err) {
    logger.debug(`messageSnapshot: forget failed: ${err?.message ?? err}`);
  }
}

/** A message-like object (what the embed builders read) from a snapshot. Never partial; mentions are not kept. */
export function snapshotToMessage(snap, guild = null) {
  const a = snap.author;
  const created = Number.isFinite(snap.createdTimestamp) ? snap.createdTimestamp : null;
  return {
    id: snap.id,
    partial: false,
    fromSnapshot: true,
    guild,
    channelId: snap.channelId,
    content: snap.content ?? '',
    author: {
      id: a.id, username: a.username ?? undefined, tag: a.tag ?? undefined, globalName: a.globalName ?? null, bot: Boolean(a.bot),
      displayAvatarURL: () => a.avatar ?? null,
    },
    createdTimestamp: created ?? undefined,
    createdAt: created ? new Date(created) : null,
    attachments: new Map((snap.attachments ?? []).map((x, i) => [String(i), x])),
    embeds: Array.from({ length: Number(snap.embeds) || 0 }, () => ({})),
    mentions: { users: new Map(), roles: new Map(), everyone: false },
    webhookId: null,
  };
}
