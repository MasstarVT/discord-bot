// Plugin `afk`: ends a member's AFK when they write, and tells whoever mentions an AFK member that they are away.
//  (1) The author is AFK (and the channel is not ignored, and the AFK is older than a few seconds): the row is deleted,
//      "[AFK] " comes off their nickname and a "Welcome back" reply with the AFK length and jump links to the pings
//      they got deletes itself shortly after.
//  (2) Each AFK member the message mentions (at most 5, never the author) is recorded as a ping and announced with one
//      short reply: "<name> is AFK: <reason> (since <t:..:R>)", at most once a minute per channel and member.
// Nothing here pings: replies send `allowedMentions: { parse: [] }`. Chat from members who are not AFK and who mention
// nobody who is never reaches Postgres: the sync filter checks the memoised set of AFK ids (src/services/afkService.js).
import { MessageFlags, escapeMarkdown } from 'discord.js';
import { missingBotPermissions } from '../utils/interactionHelpers.js';
import { discordTimestamp } from '../utils/time.js';
import logger from '../utils/logger.js';
import {
  AFK, GONE_CODES, peekAfkIds, loadAfkIds, loadStatus, loadStatuses, clearAfk, forgetAfkMember, recordPing, removeNicknamePrefix,
  isIgnoredChannel, mayNotify, plainText, nameWithoutPrefix, afkLength, readPings, jumpLink,
} from '../services/afkService.js';

/** The name shown for a member: nickname or display name without our prefix, markdown escaped, never a mention. */
export function shownName(member, user) {
  const raw = member?.displayName ?? user?.globalName ?? user?.username ?? 'Someone';
  return escapeMarkdown(plainText(nameWithoutPrefix(raw), 40)) || 'Someone';
}

/**
 * The reply to a member who is back.
 * @param {{ name: string, row: { since: Date, pingCount: number, pings: unknown }, guildId: string, now?: number }} args
 * @returns {string}
 */
export function welcomeText({ name, row, guildId, now = Date.now() }) {
  const lines = [`👋 Welcome back, **${name}**! You were AFK for **${afkLength(now - row.since.getTime())}**.`];
  const count = Number(row.pingCount) || 0;
  if (count > 0) {
    const links = readPings(row.pings).slice(-AFK.MAX_LINKS).map((p, i) => `[${i + 1}](${jumpLink(guildId, p.channelId, p.messageId)})`);
    lines.push(`You were mentioned **${count}** time${count === 1 ? '' : 's'}${links.length ? `: ${links.join(' · ')}` : '.'}`);
  }
  return lines.join('\n');
}

/**
 * The line that tells others a member is away.
 * @param {{ name: string, row: { reason: string|null, since: Date } }} args
 * @returns {string}
 */
export function noticeText({ name, row }) {
  const reason = row.reason ? plainText(row.reason) : '';
  return `${AFK.EMOJI} **${name}** is AFK${reason ? `: ${reason}` : ''} (since ${discordTimestamp(row.since, 'R')})`;
}

/** Whether the bot may answer in the channel. */
function canReply(channel) {
  const send = channel?.isThread?.() ? 'SendMessagesInThreads' : 'SendMessages';
  return missingBotPermissions(channel, ['ViewChannel', send, 'ReadMessageHistory']).length === 0;
}

/**
 * Replies to the message without pinging anyone or unfurling links; never throws. With `deleteAfterMs` the reply
 * deletes itself (an unref'd timer, so a pending delete never keeps the process alive).
 */
async function reply(message, content, deleteAfterMs = 0) {
  if (!canReply(message.channel)) return;
  try {
    const sent = await message.reply({
      content,
      allowedMentions: { parse: [], repliedUser: false },
      failIfNotExists: false,
      flags: MessageFlags.SuppressEmbeds,
    });
    if (deleteAfterMs > 0 && typeof sent?.delete === 'function') {
      const timer = setTimeout(() => { Promise.resolve(sent.delete()).catch(() => {}); }, deleteAfterMs);
      timer.unref?.();
    }
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.warn(`[afk] ${message.guildId}: could not reply: ${err?.message}`);
  }
}

async function memberOf(message) {
  if (message.member) return message.member;
  try {
    return await message.guild?.members?.fetch(message.author.id);
  } catch {
    return null;
  }
}

/** The author wrote: ends their AFK. */
async function welcomeBack(ctx, welcomeDeleteMs) {
  const { message, guildId } = ctx;
  const row = await loadStatus(guildId, message.author.id);
  if (!row) {
    forgetAfkMember(guildId, message.author.id); // the memo was behind: drop the id
    return;
  }
  if (Date.now() - new Date(row.since).getTime() <= AFK.GRACE_MS) return;
  const cleared = await clearAfk(guildId, message.author.id);
  if (!cleared) return; // /afk clear got there first
  const member = await memberOf(message);
  const text = welcomeText({ name: shownName(member, message.author), row: cleared, guildId });
  await Promise.all([
    cleared.nickChanged ? removeNicknamePrefix(member, cleared.previousNick) : null,
    reply(message, text, welcomeDeleteMs),
  ]);
}

/** The message mentions AFK members: records the pings and announces them. */
async function announce(ctx, ids) {
  const { message, guildId } = ctx;
  const authorId = message.author.id;
  const targets = [];
  for (const id of message.mentions?.users?.keys() ?? []) {
    if (id !== authorId && ids.has(id)) targets.push(id);
    if (targets.length >= AFK.MAX_NOTICES) break;
  }
  if (!targets.length) return;
  const rows = await loadStatuses(guildId, targets);
  const ping = { channelId: message.channelId, messageId: message.id, authorId };
  await Promise.all(rows.map((r) => recordPing(guildId, r.userId, ping)));
  if (!rows.length || !canReply(message.channel)) return;
  const lines = [];
  for (const row of rows) {
    if (!mayNotify(message.channelId, row.userId)) continue;
    const user = message.mentions.users.get(row.userId);
    lines.push(noticeText({ name: shownName(message.mentions.members?.get(row.userId), user), row }));
  }
  if (lines.length) await reply(message, lines.join('\n'));
}

/**
 * Sync hook filter: false for the common case, where the author is not AFK and mentions nobody who is. With no
 * memoised set yet it passes, and the handler loads it.
 * @param {object} ctx HookContext with `message`
 * @returns {boolean}
 */
export function mayConcernAfk(ctx) {
  const ids = peekAfkIds(ctx.guildId);
  if (!ids) return true;
  if (ids.size === 0) return false;
  const { message } = ctx;
  if (ids.has(message?.author?.id)) return true;
  for (const id of message?.mentions?.users?.keys() ?? []) if (ids.has(id)) return true;
  return false;
}

/**
 * Handles one message: the author's return, then the mentions of AFK members. Each part is isolated, so one failing
 * does not stop the other.
 * @param {object} ctx HookContext with `message`, `guildId`, `getSettings`
 * @param {{ welcomeDeleteMs?: number }} [opts]
 */
export async function handleMessage(ctx, { welcomeDeleteMs = AFK.WELCOME_DELETE_MS } = {}) {
  const { message, guildId } = ctx;
  const settings = await ctx.getSettings();
  if (!settings || isIgnoredChannel(settings, message.channel)) return;
  const ids = await loadAfkIds(guildId);
  if (ids.size === 0) return;
  if (ids.has(message.author.id)) {
    await welcomeBack(ctx, welcomeDeleteMs).catch((err) => logger.warn(`[afk] ${guildId}: welcome back failed: ${err?.message}`));
  }
  await announce(ctx, ids).catch((err) => logger.warn(`[afk] ${guildId}: announcing failed: ${err?.message}`));
}

export const plugin = {
  id: 'afk',
  module: 'afk',
  hooks: [
    {
      event: 'messageCreate',
      id: 'afk', // HOOK_ORDER.messageCreate: after automod has removed what it removes
      filter: mayConcernAfk,
      run: (ctx) => handleMessage(ctx),
    },
  ],
  components: [],
  jobs: [],
  provides: {},
};
