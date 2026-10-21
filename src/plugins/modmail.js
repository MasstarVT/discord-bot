// Plugin `modmail`: members DM the bot, each conversation becomes a thread in GuildSettings.modmailChannelId, and what staff
// write in that thread reaches the member's DMs.
//   member side  directMessage hook (ungated: a DM has no guild, so every server is checked here): relays into the open
//                conversation, or asks first (button for one server, select menu for several) and then opens one.
//   staff side   messageCreate hook: a message in an OPEN thread goes to the member (unless it starts with `//`).
//   cleanup      channelDelete / threadDelete close what was lost.
// The persistent `modmail` buttons and `modmail-select` menus are registered with `module: null`: a DM has no guild, so they
// re-check everything themselves. Rules and database access: src/services/modmailService.js; commands: /modmail.
import {
  ActionRowBuilder, ButtonBuilder, ButtonStyle, ChannelType, StringSelectMenuBuilder, ThreadAutoArchiveDuration, escapeMarkdown,
} from 'discord.js';
import { PERMISSION_LEVELS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { isModuleEnabled } from '../config/modules.js';
import { getSettingsSafe, peekSettings, updateSettings } from '../services/settingsService.js';
import {
  DEFAULT_GREETING, MODMAIL, STAFF_HELP, acceptsConversations, attachThread, closeByThread, closeRow, findByThread, findForUser,
  findInGuild, findOpenForUser, hasContent, isBlocked, isInternalNote, isStaffAuthor, listOpenInGuild, relayParts, releaseReserved,
  reserveOpen, runOpenLocked, staffAuthor, threadName, touch, whoLines,
} from '../services/modmailService.js';
import { TimeoutError, ackBudgetMs, createThrottle, mapLimit, withTimeout } from '../utils/async.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed } from '../utils/embedBuilder.js';
import {
  missingBotPermissions, permissionNames, pingableRoleIds, replyError, safeReply,
} from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { renderPlaceholders } from '../utils/placeholderParser.js';
import { clampText } from '../utils/text.js';

/** Unknown channel / message / member, missing access or permission: the target is gone, nothing to retry. */
const GONE_CODES = new Set([10003, 10007, 10008, 50001, 50013]);
/** "Cannot send messages to this user": DMs closed, or no server in common any more. */
const DM_CLOSED_CODE = 50007;

const promptThrottle = createThrottle(MODMAIL.PROMPT_EVERY_MS);
// A persistent button can be clicked twice: only the first click of a prompt message does the work.
const clickOnce = createThrottle(60_000);
const noServerNotice = createThrottle(30_000);

// ── Small helpers ─────────────────────────────────────────────────────────────

/** A member's relay budget: RATE_MAX messages per RATE_WINDOW_MS. */
const rate = new Map();
export function withinRate(userId, now = Date.now()) {
  if (rate.size > 2000) for (const [k, e] of rate) if (now >= e.resetAt) rate.delete(k);
  const entry = rate.get(userId);
  if (!entry || now >= entry.resetAt) {
    rate.set(userId, { count: 1, resetAt: now + MODMAIL.RATE_WINDOW_MS });
    return true;
  }
  entry.count += 1;
  return entry.count <= MODMAIL.RATE_MAX;
}

const settingsOf = async (guildId) => peekSettings(guildId) ?? getSettingsSafe(guildId);

/** Reacts, never throws. `inGuild`: a thread needs the permissions to react in it. */
async function react(message, emoji, { inGuild = false } = {}) {
  try {
    if (inGuild && missingBotPermissions(message.channel, ['AddReactions', 'ReadMessageHistory']).length) return;
    await message.react(emoji);
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.debug(`[modmail] could not react: ${err?.message}`);
  }
}

/** Takes the bot's own reaction off a message (the ⏳ of a DM that waited for a prompt). Never throws. */
async function unreact(message, emoji) {
  try {
    const reaction = message.reactions?.cache?.get?.(emoji);
    if (reaction?.me) await reaction.users.remove();
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.debug(`[modmail] could not remove a reaction: ${err?.message}`);
  }
}

/** Marks a relayed DM ✅ or ❌ and clears the ⏳ it may carry. */
async function markResult(message, ok) {
  await unreact(message, MODMAIL.WAIT_EMOJI);
  await react(message, ok ? MODMAIL.OK_EMOJI : MODMAIL.FAIL_EMOJI);
}

/**
 * The thread of a conversation. 'gone' only when Discord says it does not exist (rule 20: anything else may be temporary).
 * @returns {Promise<{ state: 'ok', thread: object } | { state: 'gone' } | { state: 'error' }>}
 */
export async function resolveThread(guild, threadId) {
  const cached = guild.channels?.cache?.get?.(threadId);
  if (cached) return { state: 'ok', thread: cached };
  try {
    const thread = await withTimeout(guild.channels.fetch(threadId), 5000, 'modmail thread fetch');
    return thread ? { state: 'ok', thread } : { state: 'gone' };
  } catch (err) {
    if (err?.code === 10003) return { state: 'gone' };
    logger.warn(`[modmail] ${guild.id}: could not fetch thread ${threadId}: ${err?.message}`);
    return { state: 'error' };
  }
}

async function sendDm(user, payload) {
  try {
    await user.send({ allowedMentions: { parse: [] }, ...payload });
    return { ok: true };
  } catch (err) {
    if (err?.code !== DM_CLOSED_CODE) logger.warn(`[modmail] could not DM ${user.id}: ${err?.message}`);
    return { ok: false, code: err?.code === DM_CLOSED_CODE ? 'DM_CLOSED' : 'FAILED' };
  }
}

// ── Embeds ────────────────────────────────────────────────────────────────────

/** A member's message as staff see it in the thread. */
export function memberMessageEmbed(settings, user, message) {
  const parts = relayParts(message);
  const embed = guildEmbed(settings, null, parts.description)
    .setAuthor({ name: clampText(user.displayName ?? user.username, 100), iconURL: user.displayAvatarURL?.() })
    .setFooter({ text: `ID ${user.id}` });
  if (parts.attachments) embed.addFields({ name: 'Attachments', value: parts.attachments });
  return embed;
}

/** Staff's message as the member sees it in their DMs. */
export function staffMessageEmbed(settings, guild, author, message) {
  const parts = relayParts(message);
  const embed = guildEmbed(settings, null, parts.description)
    .setAuthor({ name: author.name, iconURL: author.iconURL ?? undefined })
    .setFooter({ text: clampText(guild.name, 100), iconURL: guild.iconURL?.() ?? undefined });
  if (parts.attachments) embed.addFields({ name: 'Attachments', value: parts.attachments });
  return embed;
}

export function closedEmbed(guild, reason) {
  const why = reason ? `\n**Reason:** ${clampText(reason, MODMAIL.REASON_MAX)}` : '';
  return infoEmbed('Conversation Closed',
    `Your conversation with the staff of **${escapeMarkdown(guild.name)}** has ended.${why}\nSend me a message any time to start a new one.`);
}

/** The greeting the member gets when a conversation opens (placeholders rendered, never empty). */
export function greetingText(settings, { guild, member, user }) {
  const template = settings?.modmailGreeting || DEFAULT_GREETING;
  const text = renderPlaceholders(template, { guild, member, user }, { maxLength: 1000 });
  return text || renderPlaceholders(DEFAULT_GREETING, { guild, member, user }, { maxLength: 1000 });
}

// ── Finding the right server ──────────────────────────────────────────────────

/**
 * The member's OPEN conversations that can still be used: the server is cached, modmail is on and the member is not blocked.
 * @returns {Promise<Array<{ row: object, guild: object, settings: object }>>}
 */
export async function listOpenConversations(client, user) {
  const rows = await findOpenForUser(user.id);
  const out = [];
  for (const row of rows) {
    const guild = client.guilds.cache.get(row.guildId);
    if (!guild || guild.available === false) continue;
    const settings = await settingsOf(guild.id);
    if (!isModuleEnabled(settings, 'modmail') || isBlocked(settings, user.id)) continue;
    out.push({ row, guild, settings });
  }
  return out;
}

async function memberOf(guild, userId) {
  const cached = guild.members?.cache?.get?.(userId);
  if (cached) return cached;
  try {
    return await withTimeout(guild.members.fetch(userId), MODMAIL.MEMBER_FETCH_TIMEOUT_MS, 'modmail member fetch');
  } catch (err) {
    if (!(err instanceof TimeoutError) && err?.code !== 10007) logger.debug(`[modmail] ${guild.id}: member fetch failed: ${err?.message}`);
    return null;
  }
}

/** One server's answer for a member, or null: bot in it, modmail on, a staff channel set, not blocked, member of it. */
export async function serverFor(client, guildId, user) {
  const guild = client.guilds.cache.get(guildId);
  if (!guild || guild.available === false) return null;
  const settings = await settingsOf(guild.id);
  if (!acceptsConversations(settings, user.id) || !guild.channels.cache.has(settings.modmailChannelId)) return null;
  const member = await memberOf(guild, user.id);
  return member ? { guild, member, settings } : null;
}

/**
 * The servers a member can open a conversation in. Cached data first (settings memo, member cache); a member fetch only for a
 * server where modmail is on, each bounded, a few at a time, and no new server after SCAN_BUDGET_MS.
 * @returns {Promise<Array<{ guild: object, member: object, settings: object }>>}
 */
export async function findServers(client, user) {
  const started = Date.now();
  const results = await mapLimit([...client.guilds.cache.values()], MODMAIL.SCAN_CONCURRENCY, async (guild) => {
    if (Date.now() - started > MODMAIL.SCAN_BUDGET_MS) return null;
    return serverFor(client, guild.id, user);
  });
  return results.filter((r) => r.status === 'fulfilled' && r.value).map((r) => r.value)
    .sort((a, b) => a.guild.name.localeCompare(b.guild.name));
}

// ── Relaying ──────────────────────────────────────────────────────────────────

/**
 * Member -> staff: posts the member's message in the conversation's thread.
 * @returns {Promise<{ ok: true } | { ok: false, code: 'EMPTY'|'GONE'|'FAILED' }>}
 */
export async function relayToThread({ thread, row, settings, message }) {
  if (!hasContent(message)) return { ok: false, code: 'EMPTY' };
  try {
    if (thread.archived) await thread.setArchived(false).catch(() => {});
    await thread.send({ embeds: [memberMessageEmbed(settings, message.author, message)], allowedMentions: { parse: [] } });
    touch(row.id);
    return { ok: true };
  } catch (err) {
    if (err?.code === 10003) return { ok: false, code: 'GONE' };
    logger.warn(`[modmail] ${row.guildId}: could not post in thread ${row.threadId}: ${err?.message}`);
    return { ok: false, code: 'FAILED' };
  }
}

/**
 * Staff -> member: sends a staff message to the member's DMs.
 * @param {{ client: object, row: object, guild: object, settings: object, author: { name: string, iconURL: string|null }, message: object }} args
 * @returns {Promise<{ ok: true } | { ok: false, code: 'EMPTY'|'DM_CLOSED'|'FAILED' }>}
 */
export async function relayToMember({ client, row, guild, settings, author, message }) {
  if (!hasContent(message)) return { ok: false, code: 'EMPTY' };
  let user;
  try {
    user = await withTimeout(client.users.fetch(row.userId), 5000, 'modmail user fetch');
  } catch (err) {
    logger.warn(`[modmail] ${guild.id}: could not fetch member ${row.userId}: ${err?.message}`);
    return { ok: false, code: 'FAILED' };
  }
  const res = await sendDm(user, { embeds: [staffMessageEmbed(settings, guild, author, message)] });
  if (res.ok) touch(row.id);
  return res;
}

/** Delivers a member's DM into an existing conversation. 'GONE' (and the row closed) when the thread is gone. No reaction. */
async function deliverDm(conv, message) {
  const { row, guild, settings } = conv;
  const found = await resolveThread(guild, row.threadId);
  if (found.state === 'gone') {
    await closeRow(row.id, { reason: 'The thread was deleted.' }).catch(() => {});
    return 'GONE';
  }
  const res = found.state === 'ok' ? await relayToThread({ thread: found.thread, row, settings, message }) : { ok: false, code: 'FAILED' };
  if (res.code === 'GONE') {
    await closeRow(row.id, { reason: 'The thread was deleted.' }).catch(() => {});
    return 'GONE';
  }
  return res.ok ? 'OK' : 'FAILED';
}

/** `deliverDm`, reacting ✅ or ❌ on the message. Falls through to 'GONE' when the thread is gone. */
async function relayDm(conv, message) {
  const outcome = await deliverDm(conv, message);
  if (outcome !== 'GONE') await markResult(message, outcome === 'OK');
  return outcome;
}

/** `relayToThread` as 'OK' | 'FAILED' | 'GONE'. */
async function postToThread(args) {
  const res = await relayToThread(args);
  if (res.ok) return 'OK';
  return res.code === 'GONE' ? 'GONE' : 'FAILED';
}

// ── Opening a conversation ────────────────────────────────────────────────────

const OPEN_PERMS = ['ViewChannel', 'SendMessagesInThreads', 'EmbedLinks'];

/** What the staff channel needs from the bot. `errors` stop a setup, `warnings` are told to the admin. */
export function channelProblems(channel, settings = null) {
  const errors = [];
  const warnings = [];
  const lacking = missingBotPermissions(channel, OPEN_PERMS);
  if (lacking.length) errors.push(`I need **${permissionNames(lacking).join(', ')}** in <#${channel.id}>.`);
  const canPrivate = missingBotPermissions(channel, ['CreatePrivateThreads']).length === 0;
  const canPublic = missingBotPermissions(channel, ['CreatePublicThreads']).length === 0;
  if (!canPrivate && !canPublic) errors.push(`I need **Create Private Threads** in <#${channel.id}> to open conversations.`);
  else if (!canPrivate) warnings.push('I lack **Create Private Threads**, so conversations would open as **public** threads. Give me that permission.');
  if (missingBotPermissions(channel, ['ManageThreads']).length) {
    warnings.push('I lack **Manage Threads**, so I can archive closed conversations but not lock them.');
  }
  if (settings && !(settings.modmailPingRoleIds ?? []).length) {
    warnings.push('No role is pinged, so only members who can manage threads will see new private threads. Set one with `ping_role`.');
  }
  return { errors, warnings, canPrivate, canPublic };
}

async function createThread(channel, options) {
  try {
    return await channel.threads.create({ ...options, autoArchiveDuration: ThreadAutoArchiveDuration.OneWeek });
  } catch (err) {
    if (err?.code !== 50035) throw err; // invalid form body: that archive length needs a higher boost level
    return channel.threads.create({ ...options, autoArchiveDuration: ThreadAutoArchiveDuration.OneDay });
  }
}

function introPayload({ settings, user, member, row, guild, channel, isPublic, publicReason }) {
  const lines = whoLines(user, member);
  const pings = pingableRoleIds(channel, guild.members?.me ?? null, settings.modmailPingRoleIds ?? []);
  if (pings.refused.length) {
    lines.push(`⚠️ I cannot ping ${pings.refused.map((id) => `<@&${id}>`).join(', ')}: make the role mentionable or give me **Mention Everyone**.`);
  }
  if (isPublic) lines.push(`⚠️ ${publicReason}`);
  lines.push('', STAFF_HELP);
  const closeButton = new ButtonBuilder().setCustomId(buildCustomId('modmail', 'close', row.id))
    .setLabel('Close conversation').setEmoji('🔒').setStyle(ButtonStyle.Secondary);
  return {
    content: pings.allowed.length ? pings.allowed.map((id) => `<@&${id}>`).join(' ') : undefined,
    embeds: [guildEmbed(settings, '📬 New conversation', lines.join('\n'))],
    components: [new ActionRowBuilder().addComponents(closeButton)],
    allowedMentions: { parse: [], roles: pings.allowed, users: [] },
  };
}

/**
 * Opens a conversation: re-checks the server, reserves the one OPEN row (advisory lock), creates the thread (private, else
 * public), posts who the member is and pings the configured roles, greets the member and relays their first message.
 * @returns {Promise<{ ok: true, row: object, thread: object, relayed: boolean, notes: string[] }
 *   | { ok: false, code: 'BLOCKED'|'NO_CHANNEL'|'BOT_PERMS'|'OPENING'|'CREATE_FAILED'|'DB_ERROR'|'EXISTS', data?: object }>}
 */
export function openConversation({ guild, member, user, settings, message }) {
  return runOpenLocked(guild.id, user.id, async () => {
    if (!acceptsConversations(settings, user.id)) return { ok: false, code: 'BLOCKED' };
    const channel = guild.channels.cache.get(settings.modmailChannelId);
    if (!channel || channel.type !== ChannelType.GuildText) return { ok: false, code: 'NO_CHANNEL' };
    const problems = channelProblems(channel);
    if (problems.errors.length) {
      logger.warn(`[modmail] ${guild.id}: cannot open a conversation: ${problems.errors.join(' ')}`);
      return { ok: false, code: 'BOT_PERMS', data: { errors: problems.errors } };
    }

    let reserved;
    try {
      reserved = await reserveOpen(guild.id, user.id);
    } catch (err) {
      logger.error(`[modmail] ${guild.id}: reserving a conversation failed`, err);
      return { ok: false, code: 'DB_ERROR' };
    }
    if (reserved.existing) {
      return { ok: false, code: reserved.existing.threadId.startsWith(MODMAIL.PENDING_PREFIX) ? 'OPENING' : 'EXISTS', data: { row: reserved.existing } };
    }
    const { row } = reserved;

    const options = { name: threadName(user), reason: clampText(`Modmail conversation with ${user.username} (${user.id})`, 512) };
    let thread = null;
    let isPublic = false;
    let publicReason = '';
    let lastError = null;
    if (problems.canPrivate) {
      try {
        thread = await createThread(channel, { ...options, type: ChannelType.PrivateThread, invitable: false });
      } catch (err) {
        lastError = err;
        publicReason = 'Private threads could not be created here, so this conversation is in a **public** thread.';
        logger.warn(`[modmail] ${guild.id}: private thread failed (${err?.code ?? err?.message}), falling back to a public thread`);
      }
    } else {
      publicReason = 'I lack **Create Private Threads**, so this conversation is in a **public** thread.';
    }
    if (!thread && problems.canPublic) {
      try {
        thread = await createThread(channel, { ...options, type: ChannelType.PublicThread });
        isPublic = true;
      } catch (err) {
        lastError = err;
      }
    }
    if (!thread) {
      await releaseReserved(row.id);
      logger.warn(`[modmail] ${guild.id}: could not create a thread: ${lastError?.message}`);
      return { ok: false, code: 'CREATE_FAILED' };
    }

    try {
      await thread.send(introPayload({ settings, user, member, row, guild, channel, isPublic, publicReason }));
      await attachThread(row.id, thread.id);
    } catch (err) {
      logger.error(`[modmail] ${guild.id}: could not start the thread`, err);
      await thread.delete('Modmail conversation could not be saved').catch(() => {});
      await releaseReserved(row.id);
      return { ok: false, code: 'CREATE_FAILED' };
    }

    const notes = [];
    const greeting = await sendDm(user, {
      embeds: [guildEmbed(settings, `📬 ${clampText(guild.name, 200)}`, greetingText(settings, { guild, member, user }))
        .setFooter({ text: 'Reply to this message to talk to the staff.' })],
    });
    if (!greeting.ok) notes.push('The greeting could not be sent.');
    const relayed = (await relayToThread({ thread, row, settings, message })).ok;
    return { ok: true, row: { ...row, threadId: thread.id }, thread, relayed, notes };
  });
}

// ── Closing ───────────────────────────────────────────────────────────────────

/**
 * Closes a conversation's row and tells the member. Once only: `closed` is false when it was not OPEN any more.
 * @returns {Promise<{ closed: boolean, notified: boolean }>}
 */
export async function closeAndNotify({ client, guild, row, closedById = null, reason = null, memberReason = reason, notify = true }) {
  const closed = await closeRow(row.id, { closedById, reason });
  if (!closed || !notify) return { closed, notified: false };
  try {
    const user = await withTimeout(client.users.fetch(row.userId), 5000, 'modmail user fetch');
    return { closed, notified: (await sendDm(user, { embeds: [closedEmbed(guild, memberReason)] })).ok };
  } catch (err) {
    logger.debug(`[modmail] ${guild.id}: could not tell ${row.userId} the conversation closed: ${err?.message}`);
    return { closed, notified: false };
  }
}

/** Locks (when allowed) and archives a closed conversation's thread. Never throws. @returns {Promise<string|null>} a note for staff */
export async function archiveThread(thread) {
  if (!thread) return null;
  let note = null;
  try {
    if (!thread.locked && missingBotPermissions(thread, ['ManageThreads']).length === 0) await thread.setLocked(true);
    else if (!thread.locked) note = "I couldn't lock the thread (I need **Manage Threads**).";
  } catch (err) {
    note = "I couldn't lock the thread.";
    logger.debug(`[modmail] could not lock ${thread.id}: ${err?.message}`);
  }
  try {
    if (!thread.archived) await thread.setArchived(true);
  } catch (err) {
    note ??= "I couldn't archive the thread.";
    logger.debug(`[modmail] could not archive ${thread.id}: ${err?.message}`);
  }
  return note;
}

/** Closes a conversation, says so in its thread, then locks and archives it (used by block and the staff close button). */
export async function closeConversation({ client, guild, row, closedById = null, reason = null, memberReason = reason, text = null }) {
  const res = await closeAndNotify({ client, guild, row, closedById, reason, memberReason });
  if (!res.closed) return res;
  const found = await resolveThread(guild, row.threadId);
  if (found.state === 'ok') {
    const notice = text ?? `Conversation closed${closedById ? ` by <@${closedById}>` : ''}.${reason ? `\n**Reason:** ${clampText(reason, MODMAIL.REASON_MAX)}` : ''}`;
    await found.thread.send({ embeds: [successEmbed('Conversation Closed', notice)], allowedMentions: { parse: [] } }).catch(() => {});
    await archiveThread(found.thread);
  }
  return res;
}

// ── Member side: the DM flow ──────────────────────────────────────────────────

const optionsRow = (...buttons) => new ActionRowBuilder().addComponents(...buttons);

/** "Contact staff of X?" for one server. */
export function confirmPayload(guild, messageId) {
  return {
    embeds: [infoEmbed('Contact Staff?',
      `Do you want to send your message to the staff of **${escapeMarkdown(guild.name)}**? `
      + 'A private conversation opens; staff will see your name and your messages, and answer you here.')],
    components: [optionsRow(
      new ButtonBuilder().setCustomId(buildCustomId('modmail', 'open', guild.id, messageId)).setLabel('Send to staff').setEmoji('📨').setStyle(ButtonStyle.Primary),
      new ButtonBuilder().setCustomId(buildCustomId('modmail', 'cancel')).setLabel('Cancel').setStyle(ButtonStyle.Secondary),
    )],
    allowedMentions: { parse: [] },
  };
}

function pickPayload(mode, entries, messageId, { title, text }) {
  const shown = entries.slice(0, MODMAIL.PICK_MAX);
  const menu = new StringSelectMenuBuilder().setCustomId(buildCustomId('modmail-select', mode, messageId)).setPlaceholder('Choose a server')
    .addOptions(shown.map(({ guild, value }) => ({ label: clampText(guild.name, 100) || 'Server', value: String(value) })));
  const more = entries.length > shown.length ? `\nShowing the first ${shown.length} of ${entries.length} servers.` : '';
  return { embeds: [infoEmbed(title, `${text}${more}`)], components: [optionsRow(menu)], allowedMentions: { parse: [] } };
}

/** The server menu for starting a conversation (`modmail-select:open`). */
export const openPickPayload = (servers, messageId) => pickPayload('open', servers.map((s) => ({ guild: s.guild, value: s.guild.id })), messageId, {
  title: 'Which server?', text: 'Which server do you want to contact? Your message will be sent to its staff.',
});

/** The conversation menu when several are open (`modmail-select:relay`). */
export const relayPickPayload = (conversations, messageId) => pickPayload('relay', conversations.map((c) => ({ guild: c.guild, value: c.row.id })), messageId, {
  title: 'Which conversation?', text: 'You have several open conversations. Which one is this message for?',
});

async function sendNotice(channel, embed) {
  try {
    await channel.send({ embeds: [embed], allowedMentions: { parse: [] } });
  } catch (err) {
    logger.debug(`[modmail] could not reply in a DM: ${err?.message}`);
  }
}

// A DM that arrives while the prompt throttle holds is not asked about again: it waits for the member's answer to the prompt
// that is already out and is sent with it. `prompting` knows that such a prompt is really out (not a "No Server Found" notice).
/** @type {Map<string, number>} userId -> when the prompt throttle ends */
const prompting = new Map();
function startPrompting(userId, now = Date.now()) {
  if (prompting.size > 2000) for (const [k, until] of prompting) if (until <= now) prompting.delete(k);
  prompting.set(userId, now + MODMAIL.PROMPT_EVERY_MS);
}

/** ⏳ on a throttled DM: it will be sent once the member answers the prompt. */
async function markWaiting(message, now = Date.now()) {
  if ((prompting.get(message.author.id) ?? 0) > now) await react(message, MODMAIL.WAIT_EMOJI);
}

const UNAVAILABLE = () => errorEmbed('Try Again Later', "I couldn't reach the staff just now. Please try again in a moment.");

/**
 * A DM to the bot. Relays it into the member's open conversation, or asks where to open one. Never throws.
 * @param {{ client: object, message: object }} ctx directMessage HookContext
 */
export async function handleDirectMessage(ctx) {
  const { client, message } = ctx;
  const user = message?.author;
  if (!user || user.bot || !hasContent(message)) return;
  try {
    if (!withinRate(user.id)) {
      await react(message, MODMAIL.FAIL_EMOJI);
      return;
    }

    const open = await listOpenConversations(client, user);
    if (open.length === 1) {
      if ((await relayDm(open[0], message)) !== 'GONE') return;
    } else if (open.length > 1) {
      if (!promptThrottle(user.id)) return markWaiting(message);
      startPrompting(user.id);
      await message.channel.send(relayPickPayload(open, message.id));
      return;
    }

    // Nothing open: ask before opening anything.
    if (!promptThrottle(user.id)) return markWaiting(message);
    startPrompting(user.id);
    const servers = await findServers(client, user);
    if (!servers.length) {
      prompting.delete(user.id);
      if (noServerNotice(user.id)) {
        await sendNotice(message.channel, infoEmbed('No Server Found',
          "I couldn't find a server where you can contact staff through me. Staff can only be reached in servers we share that have modmail turned on."));
      }
      return;
    }
    await message.channel.send(servers.length === 1 ? confirmPayload(servers[0].guild, message.id) : openPickPayload(servers, message.id));
  } catch (err) {
    prompting.delete(user.id);
    logger.error(`[modmail] handling a DM from ${user.id} failed`, err);
    await sendNotice(message.channel, UNAVAILABLE());
  }
}

const dmText = {
  BLOCKED: () => errorEmbed('Not Available', 'You cannot contact that server through me right now.'),
  NO_CHANNEL: () => errorEmbed('Not Available', 'That server is not set up to receive messages right now.'),
  OPENING: () => infoEmbed('One Moment', 'Your conversation is being opened. Please send your message again in a moment.'),
};

async function finishPrompt(interaction, embed) {
  try {
    await interaction.editReply({ embeds: [embed], components: [] });
  } catch (err) {
    logger.debug(`[modmail] could not update a prompt: ${err?.message}`);
  }
}

async function safeDeferUpdate(interaction) {
  try {
    if (!interaction.deferred && !interaction.replied) await interaction.deferUpdate();
    return true;
  } catch (err) {
    logger.debug(`[modmail] deferUpdate failed: ${err?.message}`);
    return false;
  }
}

/** The member's own DM the prompt was about; null when it is gone or not theirs. */
async function fetchOriginal(interaction, messageId) {
  try {
    const channel = interaction.channel ?? await interaction.user.createDM();
    const message = await withTimeout(channel.messages.fetch(messageId), 5000, 'modmail original message');
    return message?.author?.id === interaction.user.id ? message : null;
  } catch {
    return null;
  }
}

const hasBotReaction = (message, emoji) => Boolean(message.reactions?.cache?.get?.(emoji)?.me);

/**
 * The DMs that waited for the member's answer: the run of their messages right after the prompting one that carry the bot's ⏳
 * (sent while the prompt throttle held), oldest first, at most CATCH_UP_MAX. The run ends at the first message without ⏳ (❌ from
 * the rate limit, or one that started a newer prompt of its own), so nothing the rate limit refused or a newer prompt owns is sent.
 * Never throws.
 */
async function fetchWaiting(interaction, messageId) {
  try {
    const channel = interaction.channel ?? await interaction.user.createDM();
    const batch = await withTimeout(channel.messages.fetch({ after: messageId, limit: MODMAIL.CATCH_UP_SCAN }), 5000, 'modmail waiting messages');
    const theirs = [...batch.values()]
      .filter((m) => m.author?.id === interaction.user.id && !m.author.bot && hasContent(m))
      .sort((a, b) => (BigInt(a.id) < BigInt(b.id) ? -1 : 1));
    const waiting = [];
    for (const m of theirs) {
      if (!hasBotReaction(m, MODMAIL.WAIT_EMOJI)) break;
      if (hasBotReaction(m, MODMAIL.OK_EMOJI)) continue;
      waiting.push(m);
      if (waiting.length >= MODMAIL.CATCH_UP_MAX) break;
    }
    return waiting;
  } catch (err) {
    logger.debug(`[modmail] could not look for waiting messages: ${err?.message}`);
    return [];
  }
}

/**
 * Sends `messages` one after another with `deliver` (→ 'OK' | 'FAILED' | 'GONE'), marking each ✅ or ❌ (and clearing its ⏳).
 * After a 'GONE' the rest are marked ❌ without trying.
 * @returns {Promise<{ sent: number, failed: number, gone: boolean }>}
 */
async function relayAll(messages, deliver) {
  const tally = { sent: 0, failed: 0, gone: false };
  for (const message of messages) {
    const outcome = tally.gone ? 'GONE' : await deliver(message);
    if (outcome === 'GONE') tally.gone = true;
    await markResult(message, outcome === 'OK');
    if (outcome === 'OK') tally.sent += 1;
    else tally.failed += 1;
  }
  return tally;
}

/** "Your message was sent" for one message, "Your 3 messages were sent" for several, and what could not be delivered. */
function sentText(sent, failed, suffix) {
  const head = sent > 1 ? `Your ${sent} messages were sent` : 'Your message was sent';
  const note = failed ? ` ${failed} could not be delivered (marked ${MODMAIL.FAIL_EMOJI}).` : '';
  return `${head} ${suffix}${note}`;
}

/** Opens a conversation in `guildId` for the member who answered a prompt, then relays the message the prompt was about
 *  and the DMs that waited for the answer. */
async function openFromPrompt(interaction, client, guildId, messageId) {
  if (!clickOnce(`${interaction.message?.id ?? interaction.id}`)) {
    await safeDeferUpdate(interaction);
    return;
  }
  if (!(await safeDeferUpdate(interaction))) return;
  try {
    const user = interaction.user;
    const server = await serverFor(client, guildId, user);
    if (!server) return finishPrompt(interaction, dmText.BLOCKED());
    const original = await fetchOriginal(interaction, messageId);
    if (!original) return finishPrompt(interaction, errorEmbed('Message Not Found', "I couldn't find your message any more. Please send it again."));

    const res = await openConversation({ guild: server.guild, member: server.member, user, settings: server.settings, message: original });
    if (res.ok) {
      await markResult(original, res.relayed);
      // The DMs sent while this prompt was open follow the first one into the new thread.
      const later = await relayAll(await fetchWaiting(interaction, messageId),
        (message) => postToThread({ thread: res.thread, row: res.row, settings: server.settings, message }));
      return finishPrompt(interaction, successEmbed('Conversation Opened',
        sentText((res.relayed ? 1 : 0) + later.sent, later.failed, `to the staff of **${escapeMarkdown(server.guild.name)}**. They will answer you here.`)));
    }
    if (res.code === 'EXISTS') {
      const found = await resolveThread(server.guild, res.data.row.threadId);
      const deliver = (message) => (found.state === 'ok'
        ? postToThread({ thread: found.thread, row: res.data.row, settings: server.settings, message })
        : 'FAILED');
      const tally = await relayAll([original, ...await fetchWaiting(interaction, messageId)], deliver);
      return finishPrompt(interaction, tally.sent
        ? successEmbed('Message Sent', sentText(tally.sent, tally.failed, 'to the staff.'))
        : UNAVAILABLE());
    }
    return finishPrompt(interaction, dmText[res.code]?.() ?? UNAVAILABLE());
  } catch (err) {
    logger.error(`[modmail] opening a conversation for ${interaction.user?.id} failed`, err);
    return finishPrompt(interaction, UNAVAILABLE());
  }
}

/** The member picked which of several open conversations a message belongs to. */
async function relayFromPrompt(interaction, client, rowId, messageId) {
  if (!clickOnce(`${interaction.message?.id ?? interaction.id}`)) {
    await safeDeferUpdate(interaction);
    return;
  }
  if (!(await safeDeferUpdate(interaction))) return;
  try {
    const user = interaction.user;
    const row = await findForUser(user.id, rowId);
    const guild = row ? client.guilds.cache.get(row.guildId) : null;
    const settings = guild ? await settingsOf(guild.id) : null;
    if (!row || row.status !== 'OPEN' || !guild || !isModuleEnabled(settings, 'modmail') || isBlocked(settings, user.id)) {
      return finishPrompt(interaction, errorEmbed('Conversation Closed', 'That conversation is no longer open. Send your message again to start a new one.'));
    }
    const original = await fetchOriginal(interaction, messageId);
    if (!original) return finishPrompt(interaction, errorEmbed('Message Not Found', "I couldn't find your message any more. Please send it again."));
    const conv = { row, guild, settings };
    const tally = await relayAll([original, ...await fetchWaiting(interaction, messageId)], (message) => deliverDm(conv, message));
    return finishPrompt(interaction, tally.sent
      ? successEmbed('Message Sent', sentText(tally.sent, tally.failed, `to the staff of **${escapeMarkdown(guild.name)}**.`))
      : errorEmbed('Not Sent', tally.gone ? 'That conversation was closed. Send your message again to start a new one.' : "I couldn't deliver your message. Please try again."));
  } catch (err) {
    logger.error(`[modmail] relaying for ${interaction.user?.id} failed`, err);
    return finishPrompt(interaction, UNAVAILABLE());
  }
}

/** `modmail-select:<open|relay>:<messageId>`: the member's server or conversation choice. */
export async function handleSelect(interaction, client) {
  const { params: [mode, messageId] } = parseCustomId(interaction.customId);
  const value = interaction.values?.[0];
  if (interaction.inGuild() || !value || !messageId) {
    return safeReply(interaction, { embeds: [errorEmbed('Menu Expired', 'This menu is no longer active.')], ephemeral: true });
  }
  if (mode === 'open') return openFromPrompt(interaction, client, value, messageId);
  if (mode === 'relay') return relayFromPrompt(interaction, client, value, messageId);
  return safeReply(interaction, { embeds: [errorEmbed('Menu Expired', 'This menu is no longer active.')], ephemeral: true });
}

// ── Staff side: the close button and the thread relay ─────────────────────────

/** The "Close conversation" button in a thread's first message. Moderators only (buttons are not covered by command levels). */
async function handleCloseButton(interaction, client, rowId) {
  if (!interaction.inCachedGuild()) {
    return safeReply(interaction, { embeds: [errorEmbed('Server Only', 'This button only works inside the server.')], ephemeral: true });
  }
  const { guild, guildId } = interaction;
  let settings = peekSettings(guildId);
  if (!settings) settings = await withTimeout(getSettingsSafe(guildId), ackBudgetMs(interaction), 'modmail close settings').catch(() => null);
  if (resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.MODERATOR) {
    return replyError(interaction, 'Insufficient Permissions', 'You need the Moderator level to close a conversation.');
  }
  if (!(await safeDeferUpdate(interaction))) return;

  const row = await findInGuild(guildId, rowId);
  if (!row || row.status !== 'OPEN' || row.threadId !== interaction.channelId) {
    await interaction.editReply({ components: [] }).catch(() => {});
    return safeReply(interaction, { embeds: [errorEmbed('Already Closed', 'This conversation is already closed.')], ephemeral: true });
  }
  const res = await closeAndNotify({ client, guild, row, closedById: interaction.user.id });
  await interaction.editReply({ components: [] }).catch(() => {});
  if (!res.closed) return safeReply(interaction, { embeds: [errorEmbed('Already Closed', 'This conversation is already closed.')], ephemeral: true });
  await safeReply(interaction, {
    embeds: [successEmbed('Conversation Closed', `Closed by <@${interaction.user.id}>.${res.notified ? '' : '\nThe member could not be notified (DMs closed).'}`)],
    allowedMentions: { parse: [] },
  });
  await archiveThread(interaction.channel);
}

/** `modmail:open:<guildId>:<messageId>` and `modmail:cancel` (DMs), `modmail:close:<rowId>` (a thread). */
export async function handleButton(interaction, client) {
  const { params: [action, a, b] } = parseCustomId(interaction.customId);
  if (action === 'close' && a) return handleCloseButton(interaction, client, a);
  if (!interaction.inGuild()) {
    if (action === 'open' && a && b) return openFromPrompt(interaction, client, a, b);
    if (action === 'cancel') {
      return interaction.update({ embeds: [infoEmbed('Cancelled', 'Nothing was sent.')], components: [] }).catch(() => {});
    }
  }
  return safeReply(interaction, { embeds: [errorEmbed('Button Expired', 'This button is no longer active.')], ephemeral: true });
}

// Threads that are known not to be modmail threads (a busy thread must not cost a query per message).
const notModmail = new Map();
const NOT_MODMAIL_TTL_MS = 5 * 60_000;
const NOT_MODMAIL_MAX = 5000;
function rememberNotModmail(threadId) {
  notModmail.set(threadId, Date.now() + NOT_MODMAIL_TTL_MS);
  if (notModmail.size > NOT_MODMAIL_MAX) notModmail.delete(notModmail.keys().next().value);
}
function knownNotModmail(threadId) {
  const until = notModmail.get(threadId);
  if (until === undefined) return false;
  if (until > Date.now()) return true;
  notModmail.delete(threadId);
  return false;
}

/**
 * Sync hook filter: a message in a thread that is not a note, and not one the bot's own threads rule out (modmail threads
 * are always created by the bot) or a thread already checked.
 */
export function staffMessageFilter(ctx) {
  const channel = ctx.message?.channel;
  if (!channel?.isThread?.() || isInternalNote(ctx.message.content)) return false;
  if (channel.ownerId && ctx.client?.user?.id && channel.ownerId !== ctx.client.user.id) return false;
  return !knownNotModmail(channel.id);
}

/** Staff -> member: a message in an OPEN modmail thread goes to the member's DMs; ✅ when delivered, ❌ when not. */
export async function handleStaffMessage(ctx) {
  const { message, client, guild } = ctx;
  if (message.author?.bot || message.webhookId) return;
  const thread = message.channel;
  const row = await findByThread(thread.id);
  if (!row) return rememberNotModmail(thread.id);
  if (row.status !== 'OPEN' || row.guildId !== guild.id) return;
  if (isInternalNote(message.content) || !hasContent(message)) return;

  const settings = await ctx.getSettings();
  if (!settings) return;
  const member = message.member ?? await guild.members.fetch(message.author.id).catch(() => null);
  if (!isStaffAuthor(member, settings, thread)) return;

  const author = staffAuthor(member, guild, settings.modmailAnonymousReplies === true);
  const res = await relayToMember({ client, row, guild, settings, author, message });
  await react(message, res.ok ? MODMAIL.OK_EMOJI : MODMAIL.FAIL_EMOJI, { inGuild: true });
  if (res.code === 'DM_CLOSED') {
    await thread.send({
      embeds: [errorEmbed('Not Delivered', "The member's DMs are closed, so they did not get that message. They can still write to me.")],
      allowedMentions: { parse: [] },
    }).catch(() => {});
  }
}

// ── Cleanup ───────────────────────────────────────────────────────────────────

/** A deleted thread closes its conversation. */
export async function onThreadDeleted(ctx) {
  const id = ctx.thread?.id ?? ctx.channel?.id;
  if (!id) return;
  const closed = await closeByThread(id);
  if (closed) logger.info(`[modmail] ${ctx.guildId}: thread ${id} was deleted, conversation closed`);
}

/**
 * The staff channel was deleted: the setting is cleared and the conversations whose thread Discord says is gone are closed
 * (threads of a deleted channel are deleted with it; conversations in an older staff channel keep going).
 */
export async function onChannelDeleted(ctx) {
  const settings = await ctx.getSettings();
  if (!settings || settings.modmailChannelId !== ctx.channel.id) return;
  await updateSettings(ctx.guildId, { modmailChannelId: null });
  logger.info(`[modmail] ${ctx.guildId}: the staff channel ${ctx.channel.id} was deleted, modmail has no channel now`);
  const rows = await listOpenInGuild(ctx.guildId);
  await mapLimit(rows, 3, async (row) => {
    if ((await resolveThread(ctx.guild, row.threadId)).state === 'gone') await closeByThread(row.threadId, 'The staff channel was deleted.');
  });
}

const hooks = [
  {
    // A DM has no guild, so the hook cannot be module-gated: it checks every server itself.
    event: 'directMessage',
    id: 'modmail',
    module: null,
    always: true,
    timeoutMs: 30_000,
    filter: (ctx) => Boolean(ctx.message) && !ctx.message.guildId && !ctx.message.author?.bot,
    run: handleDirectMessage,
  },
  {
    event: 'messageCreate',
    id: 'modmail', // HOOK_ORDER.messageCreate: after autotime
    filter: staffMessageFilter,
    run: handleStaffMessage,
  },
  {
    event: 'channelDelete',
    id: 'modmail',
    always: true,
    timeoutMs: 30_000,
    filter: (ctx) => {
      const settings = peekSettings(ctx.guildId);
      return !settings || settings.modmailChannelId === ctx.channel?.id;
    },
    run: onChannelDeleted,
  },
  {
    event: 'threadDelete',
    id: 'modmail',
    always: true,
    run: onThreadDeleted,
  },
];

export const plugin = {
  id: 'modmail',
  module: 'modmail',
  hooks,
  components: [
    // module: null: a DM has no guild, so there is no module toggle to read; every handler re-checks the server.
    { type: 'button', prefix: 'modmail', module: null, handler: handleButton },
    { type: 'select', prefix: 'modmail-select', module: null, handler: handleSelect },
  ],
  jobs: [],
  provides: {},
};
