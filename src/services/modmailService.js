// Modmail: members DM the bot and every conversation becomes a thread in the staff channel (GuildSettings.modmailChannelId).
// This file holds the rules that need no Discord connection (what counts as an internal note, who may speak for staff, how a
// message is shown to the other side, the thread name) and the ModmailThread table access. At most one OPEN row exists per
// (guild, member): a row is reserved under a per-(guild, member) advisory lock BEFORE the thread is created, with a
// placeholder thread id, and the real id is attached afterwards (the same shape as tickets). The Discord work (opening,
// relaying, closing) is in src/plugins/modmail.js.
// Side-effect free at import: no timers, no I/O.
import { PermissionFlagsBits } from 'discord.js';
import { Prisma } from '@prisma/client';
import { randomUUID } from 'node:crypto';
import prisma from '../database/client.js';
import { PERMISSION_LEVELS } from '../config/constants.js';
import { isModuleEnabled } from '../config/modules.js';
import { ensureGuildSettings } from './settingsService.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { clampText, joinWithLimit, sanitizeName } from '../utils/text.js';
import { createKeyedMutex } from '../utils/async.js';
import logger from '../utils/logger.js';

/** The greeting sent while GuildSettings.modmailGreeting is null (the same text as DEFAULT_TEXTS.modmailGreeting). */
export const DEFAULT_GREETING = 'Thanks for your message! The staff of **{server}** will answer you here soon.';

export const MODMAIL = Object.freeze({
  /** A staff message that starts with this stays in the thread: an internal note. */
  NOTE_PREFIX: '//',
  OK_EMOJI: '✅',
  FAIL_EMOJI: '❌',
  /** On a DM that arrived while a "which server?" prompt was open: it is sent once the member answers it. */
  WAIT_EMOJI: '⏳',
  /** Answering a prompt also sends at most this many of the member's DMs that waited for it. */
  CATCH_UP_MAX: 10,
  /** Messages looked at after the prompting one (the bot's own prompts sit in between). */
  CATCH_UP_SCAN: 30,
  /** A select menu holds at most 25 options. */
  PICK_MAX: 25,
  /** Placeholder thread id of a row whose thread is still being created. */
  PENDING_PREFIX: 'pending-',
  /** A placeholder this old belongs to an attempt that died; the next open replaces it. */
  PENDING_STALE_MS: 2 * 60_000,
  /** One member fetch while looking for the servers a member shares with the bot. */
  MEMBER_FETCH_TIMEOUT_MS: 2_500,
  /** No new server is looked at after this long (the hook has a 10 s limit). */
  SCAN_BUDGET_MS: 6_000,
  SCAN_CONCURRENCY: 5,
  /** Embed description cap (Discord allows 4096; the rest of the embed needs room inside the 6000 total). */
  TEXT_MAX: 4000,
  THREAD_NAME_MAX: 100,
  REASON_MAX: 500,
  /** A member's relayed messages: at most RATE_MAX per RATE_WINDOW_MS. */
  RATE_MAX: 10,
  RATE_WINDOW_MS: 10_000,
  /** The "open a conversation?" question is asked at most this often per member. */
  PROMPT_EVERY_MS: 8_000,
  /** Rows listed by /modmail blocked. */
  BLOCKED_LIST_MAX: 50,
});

/** The staff-side permission that makes a member count as staff in a modmail thread. */
export const STAFF_THREAD_PERMISSION = PermissionFlagsBits.ManageThreads;

// ── Pure rules ────────────────────────────────────────────────────────────────

/** Staff messages starting with `//` are internal notes: never sent to the member. */
export function isInternalNote(content) {
  return typeof content === 'string' && content.trimStart().startsWith(MODMAIL.NOTE_PREFIX);
}

export function isBlocked(settings, userId) {
  return Array.isArray(settings?.modmailBlockedUserIds) && settings.modmailBlockedUserIds.includes(userId);
}

/** Whether a guild takes new conversations from `userId`: module on, a staff channel set, member not blocked. */
export function acceptsConversations(settings, userId) {
  return isModuleEnabled(settings, 'modmail') && Boolean(settings.modmailChannelId) && !isBlocked(settings, userId);
}

/** A placeholder thread id (a row whose thread is still being created). */
export const isPendingThreadId = (threadId) => typeof threadId === 'string' && threadId.startsWith(MODMAIL.PENDING_PREFIX);

const valuesOf = (collection) => {
  if (!collection) return [];
  if (typeof collection.values === 'function') return [...collection.values()];
  return Array.isArray(collection) ? collection : [];
};

/** Markdown-safe link text: brackets would end the link early. */
const linkText = (name) => clampText(String(name ?? 'file').replace(/[[\]\\`*_~|<>]/g, '').trim() || 'file', 60, '');

/**
 * One line per attachment and sticker of a message: `[name](url)` links, `[Sticker: name]`.
 * @param {{ attachments?: any, stickers?: any }} message
 * @returns {string[]}
 */
export function attachmentLines(message) {
  const lines = [];
  for (const a of valuesOf(message?.attachments)) {
    if (a?.url) lines.push(`[${linkText(a.name)}](${a.url})`);
  }
  for (const s of valuesOf(message?.stickers)) lines.push(`[Sticker: ${linkText(s?.name ?? 'sticker')}]`);
  return lines;
}

/** Whether a message has anything to relay (text, a file or a sticker). */
export function hasContent(message) {
  return Boolean(String(message?.content ?? '').trim()) || attachmentLines(message).length > 0;
}

/**
 * The fields of a relayed message embed: the text (cut to fit) and an "Attachments" field.
 * @returns {{ description: string, attachments: string|null }}
 */
export function relayParts(message) {
  const text = String(message?.content ?? '').trim();
  const lines = attachmentLines(message);
  return {
    description: clampText(text, MODMAIL.TEXT_MAX),
    attachments: lines.length ? joinWithLimit(lines, { sep: '\n', max: 1000 }) : null,
  };
}

/** Thread title: `modmail-<username>`, lowercase and dash-safe, at most 100 characters. */
export function threadName(user) {
  return clampText(`modmail-${sanitizeName(user?.username, { max: 40, fallback: 'member' })}`, MODMAIL.THREAD_NAME_MAX, '');
}

/**
 * Who speaks for staff in the member's DMs: the staff member's name, or "Staff" when replies are anonymous.
 * @param {{ displayName?: string, user?: object }|null} member
 * @param {object} guild
 * @param {boolean} anonymous
 * @returns {{ name: string, iconURL: string|null }}
 */
export function staffAuthor(member, guild, anonymous) {
  if (anonymous || !member) {
    return { name: 'Staff', iconURL: guild?.iconURL?.() ?? null };
  }
  const name = member.displayName ?? member.user?.username ?? 'Staff';
  return { name: clampText(name, 100), iconURL: member.displayAvatarURL?.() ?? member.user?.displayAvatarURL?.() ?? null };
}

/**
 * Whether a message author in a modmail thread may speak for staff. A public fallback thread can be read (and typed in) by
 * everyone who sees the staff channel, so being able to type there is not enough: the author must be a moderator, hold one
 * of the ping roles, or be able to manage threads in the channel.
 * @param {import('discord.js').GuildMember|null} member
 * @param {object|null} settings
 * @param {import('discord.js').GuildBasedChannel|null} channel
 */
export function isStaffAuthor(member, settings, channel) {
  if (!member) return false;
  try {
    if (resolvePermissionLevel(member, settings) >= PERMISSION_LEVELS.MODERATOR) return true;
    const pings = settings?.modmailPingRoleIds ?? [];
    if (pings.some((id) => member.roles?.cache?.has?.(id))) return true;
    return Boolean(channel?.permissionsFor?.(member)?.has(STAFF_THREAD_PERMISSION));
  } catch {
    return false;
  }
}

/** The "who" lines of a new thread's first message. */
export function whoLines(user, member) {
  const lines = [`**Member:** <@${user.id}> (\`${user.id}\`)`];
  if (user.createdTimestamp) lines.push(`**Account created:** <t:${Math.floor(user.createdTimestamp / 1000)}:R>`);
  if (member?.joinedTimestamp) lines.push(`**Joined the server:** <t:${Math.floor(member.joinedTimestamp / 1000)}:R>`);
  return lines;
}

/** The note a staff member sees under the "who" lines. */
export const STAFF_HELP = `Messages you write here go to the member. Start one with \`${MODMAIL.NOTE_PREFIX}\` for an internal note. `
  + 'End the conversation with the button below or `/modmail close`.';

// ── Database ──────────────────────────────────────────────────────────────────

const openLockSql = (guildId, userId) => Prisma.sql`SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext(${`modmail-open:${guildId}:${userId}`}))`;

// Opens of one member in one guild run one at a time in this process too, so a double click cannot even start two threads.
const openMutex = createKeyedMutex();

/** Runs `fn` while holding the (guild, member) open lock of this process. */
export function runOpenLocked(guildId, userId, fn) {
  return openMutex.run(`${guildId}:${userId}`, fn);
}

/** The thread row for a Discord thread id (any status), or null. */
export function findByThread(threadId) {
  return prisma.modmailThread.findUnique({ where: { threadId: String(threadId) } });
}

/** A row by its id inside a guild (an id from another guild is simply not found), or null. */
export function findInGuild(guildId, id) {
  return prisma.modmailThread.findFirst({ where: { id: String(id), guildId } });
}

/** A row by id for a member (the DM buttons carry row ids), or null. */
export function findForUser(userId, id) {
  return prisma.modmailThread.findFirst({ where: { id: String(id), userId } });
}

/** The OPEN conversation of a member in one guild that has a real thread, or null. */
export async function findOpenInGuild(guildId, userId) {
  const rows = await prisma.modmailThread.findMany({ where: { guildId, userId, status: 'OPEN' }, orderBy: { lastActivityAt: 'desc' }, take: 5 });
  return rows.find((r) => !isPendingThreadId(r.threadId)) ?? null;
}

/** The member's OPEN conversations that have a real thread, most recent first. */
export async function findOpenForUser(userId, limit = MODMAIL.PICK_MAX) {
  const rows = await prisma.modmailThread.findMany({
    where: { userId, status: 'OPEN' }, orderBy: { lastActivityAt: 'desc' }, take: limit + 5,
  });
  return rows.filter((r) => !isPendingThreadId(r.threadId)).slice(0, limit);
}

/**
 * Reserves the one OPEN row of a member in a guild, under an advisory lock. The thread does not exist yet: the row carries a
 * placeholder thread id until attachThread. An existing OPEN row is returned instead of a new one; a placeholder left by an
 * attempt that died long ago is replaced.
 * @returns {Promise<{ row: object } | { existing: object }>}
 * @throws on database failure
 */
export async function reserveOpen(guildId, userId) {
  await ensureGuildSettings(guildId);
  return prisma.$transaction(async (tx) => {
    await tx.$queryRaw(openLockSql(guildId, userId));
    const open = await tx.modmailThread.findMany({ where: { guildId, userId, status: 'OPEN' } });
    for (const row of open) {
      const stale = isPendingThreadId(row.threadId) && Date.now() - new Date(row.createdAt).getTime() > MODMAIL.PENDING_STALE_MS;
      if (stale) await tx.modmailThread.deleteMany({ where: { id: row.id } });
      else return { existing: row };
    }
    const row = await tx.modmailThread.create({ data: { guildId, userId, threadId: `${MODMAIL.PENDING_PREFIX}${randomUUID()}` } });
    return { row };
  }, { timeout: 10_000, maxWait: 5_000 });
}

/** Points a reserved row at its real thread. */
export function attachThread(id, threadId) {
  return prisma.modmailThread.update({ where: { id }, data: { threadId, lastActivityAt: new Date() } });
}

/** Drops a reserved row whose thread could not be created. Never throws. */
export function releaseReserved(id) {
  return prisma.modmailThread.deleteMany({ where: { id } }).catch((err) => logger.warn(`[modmail] could not drop a reserved row: ${err?.message}`));
}

/**
 * OPEN -> CLOSED, once: a second caller (a double click, the thread deleted at the same moment) gets false.
 * @returns {Promise<boolean>} whether this call closed it
 */
export async function closeRow(id, { closedById = null, reason = null } = {}) {
  const res = await prisma.modmailThread.updateMany({
    where: { id, status: 'OPEN' },
    data: { status: 'CLOSED', closedById, closedAt: new Date(), closeReason: reason ? clampText(reason, MODMAIL.REASON_MAX) : null },
  });
  return res.count > 0;
}

/** Closes the OPEN row of a deleted thread. @returns {Promise<number>} rows closed */
export async function closeByThread(threadId, reason = 'The thread was deleted.') {
  const res = await prisma.modmailThread.updateMany({
    where: { threadId: String(threadId), status: 'OPEN' },
    data: { status: 'CLOSED', closedAt: new Date(), closeReason: reason },
  });
  return res.count;
}

/** The OPEN rows of a guild (at most `limit`). */
export function listOpenInGuild(guildId, limit = 200) {
  return prisma.modmailThread.findMany({ where: { guildId, status: 'OPEN' }, take: limit });
}

/** Keeps lastActivityAt current. Fire and forget: never throws. */
export function touch(id) {
  prisma.modmailThread.updateMany({ where: { id }, data: { lastActivityAt: new Date() } })
    .catch((err) => logger.debug(`[modmail] touch failed: ${err?.message}`));
}
