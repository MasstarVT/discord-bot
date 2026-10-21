// Suggestions (beyond MEE6): members post ideas with /suggestion submit, everyone votes with the Up/Down buttons and staff
// approve, deny, consider or implement them. This file holds the rules: the post (embed + buttons), the numbered row
// (per-guild number and cooldown under one advisory lock), the vote transaction (row lock, one vote per member, totals
// recounted from SuggestionVote), the debounced message edit, the review (status, note, reviewer, author DM) and the
// delete. src/plugins/suggestions.js declares the hooks/components; src/commands/community/suggestion.js is the command.
import { Prisma } from '@prisma/client';
import {
  ActionRowBuilder, ButtonBuilder, ButtonStyle, EmbedBuilder, ModalBuilder, TextInputBuilder, TextInputStyle,
  ThreadAutoArchiveDuration,
} from 'discord.js';
import prisma from '../database/client.js';
import { COLORS, LIMITS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { MODULES } from '../config/modules.js';
import { ensureGuildSettings, getSettings, getSettingsSafe, isModuleEnabled, updateSettings } from './settingsService.js';
import { createKeyedMutex } from '../utils/async.js';
import { markBotDeletion } from '../utils/deletionTracker.js';
import { errorEmbed, successEmbed, warnEmbed } from '../utils/embedBuilder.js';
import {
  checkChannelUsable, missingBotPermissions, permissionNames, safeDefer, safeReply,
} from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { clampText } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';

export const SUGGESTION = Object.freeze({
  // Message edits per suggestion: one at once, then at most one more per window (the trailing one reads the database).
  RENDER_INTERVAL_MS: 2000,
  // Review note: also the length of the command option; an embed field holds 1024.
  NOTE_MAX: 1000,
  ANONYMOUS_NAME: 'Anonymous',
});

const NO_MENTIONS = Object.freeze({ parse: [] });
const TX_OPTS = Object.freeze({ timeout: 10_000, maxWait: 10_000 });
const RENDER_SWEEP_AT = 500;
const RENDER_IDLE_MS = 10 * 60_000;
const WARN_EVERY_MS = 10 * 60_000;
const UNKNOWN_CHANNEL = 10003;
const UNKNOWN_MESSAGE = 10008;
const NO_ACCESS_CODES = new Set([50001, 50013, 50083]); // missing access / permissions / archived thread
const POST_NEEDS = Object.freeze(['ViewChannel', 'SendMessages', 'EmbedLinks']);

export const STATUS_INFO = Object.freeze({
  OPEN: Object.freeze({ label: 'Open', emoji: '🗳️', color: COLORS.INFO, dm: 'is open for votes' }),
  CONSIDERED: Object.freeze({ label: 'Under consideration', emoji: '🤔', color: COLORS.WARNING, dm: 'is under consideration' }),
  APPROVED: Object.freeze({ label: 'Approved', emoji: '✅', color: COLORS.SUCCESS, dm: 'was approved' }),
  DENIED: Object.freeze({ label: 'Denied', emoji: '❌', color: COLORS.ERROR, dm: 'was denied' }),
  IMPLEMENTED: Object.freeze({ label: 'Implemented', emoji: '🚀', color: 0x9b59b6, dm: 'was implemented' }),
});
/** Voting closes once staff decided; CONSIDERED keeps it open. */
export const CLOSED_STATUSES = Object.freeze(new Set(['APPROVED', 'DENIED', 'IMPLEMENTED']));
export const isVotingOpen = (status) => !CLOSED_STATUSES.has(status);

// ── Raw SQL (one entry per statement, with sample parameters) ─────────────────

const numberLockSql = (guildId) => Prisma.sql`SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext(${`suggestion-number:${guildId}`}))`;
const nextNumberSql = (guildId) => Prisma.sql`SELECT (COALESCE(MAX("number"), 0) + 1)::int AS next FROM "Suggestion" WHERE "guildId" = ${guildId}`;
const lockRowSql = (id) => Prisma.sql`SELECT "status"::text AS status FROM "Suggestion" WHERE "id" = ${id} FOR UPDATE`;

export const RAW_SQL = Object.freeze([
  { name: 'suggestions.numberLock', build: () => numberLockSql('0') },
  { name: 'suggestions.nextNumber', build: () => nextNumberSql('0') },
  { name: 'suggestions.lockRow', build: () => lockRowSql('0') },
]);

// ── Pure helpers ──────────────────────────────────────────────────────────────

/** @returns {{ ok: true, content: string } | { ok: false, error: 'empty'|'too-long' }} */
export function parseContent(raw) {
  const content = String(raw ?? '').trim();
  if (!content) return { ok: false, error: 'empty' };
  if (content.length > LIMITS.SUGGESTION_LENGTH) return { ok: false, error: 'too-long' };
  return { ok: true, content };
}

/** Milliseconds until the member may post again (0 = now). */
export function cooldownRemainingMs(lastAt, cooldownSeconds, now = Date.now()) {
  const secs = Number(cooldownSeconds);
  if (!lastAt || !Number.isFinite(secs) || secs <= 0) return 0;
  return Math.max(0, new Date(lastAt).getTime() + secs * 1000 - now);
}

/**
 * One vote per member: the same vote again removes it, the other one switches it.
 * @param {number} current 1, -1 or 0 (no vote)
 * @param {1|-1} value
 * @returns {{ next: number, result: 'added'|'switched'|'removed' }}
 */
export function toggleVote(current, value) {
  if (current === value) return { next: 0, result: 'removed' };
  return { next: value, result: current === 0 ? 'added' : 'switched' };
}

/** Link to the post, or null when the message is gone. */
export function jumpUrl(s) {
  return s?.messageId ? `https://discord.com/channels/${s.guildId}/${s.channelId}/${s.messageId}` : null;
}

/** The router's wording for a member command used while the module is off. */
export function moduleOffEmbed() {
  const m = MODULES.suggestions;
  return warnEmbed('Module Disabled', `The **${m.label}** module is turned off on this server. An admin can enable it with \`/module enable suggestions\`.`);
}

const noChannelEmbed = () => errorEmbed('No Suggestion Channel', 'Suggestions are not set up yet. An admin can pick a channel with `/suggestion setup`.');

/**
 * What stops a member from submitting, from settings alone (the cooldown needs the database).
 * @returns {import('discord.js').EmbedBuilder|null} an error embed, or null
 */
export function submitProblem(settings, { anonymous = false } = {}) {
  if (!isModuleEnabled(settings, 'suggestions')) return moduleOffEmbed();
  if (!settings.suggestionChannelId) return noChannelEmbed();
  if (anonymous && !settings.suggestionAllowAnonymous) {
    return errorEmbed('Anonymous Not Allowed', 'This server does not allow anonymous suggestions. Leave `anonymous` off.');
  }
  return null;
}

export const cooldownEmbed = (ms) => warnEmbed(
  'Slow Down',
  `You can post another suggestion ${discordTimestamp(new Date(Date.now() + ms), 'R')}.`,
);

// ── Message ───────────────────────────────────────────────────────────────────

/**
 * The post. An anonymous suggestion shows no name, avatar or id anywhere (staff read the author from the database).
 * @param {object} s Suggestion row
 * @param {{ author?: { name: string, iconURL?: string|null }|null }} [opts]
 */
export function buildSuggestionEmbed(s, { author = null } = {}) {
  const info = STATUS_INFO[s.status] ?? STATUS_INFO.OPEN;
  const embed = new EmbedBuilder()
    .setColor(info.color)
    .setTitle(`💡 Suggestion #${s.number}`)
    .setDescription(clampText(s.content, 4000))
    .addFields(
      { name: 'Status', value: `${info.emoji} ${info.label}`, inline: true },
      { name: 'Votes', value: `👍 ${s.upvotes ?? 0}  ·  👎 ${s.downvotes ?? 0}`, inline: true },
    );
  if (s.createdAt) embed.setTimestamp(new Date(s.createdAt));
  if (s.anonymous) embed.setAuthor({ name: SUGGESTION.ANONYMOUS_NAME });
  else if (author?.name) embed.setAuthor(author.iconURL ? { name: clampText(author.name, 256), iconURL: author.iconURL } : { name: clampText(author.name, 256) });
  if (s.reviewedAt || s.reviewerId) {
    const note = s.reviewNote ? clampText(s.reviewNote, SUGGESTION.NOTE_MAX) : '*No note.*';
    embed.addFields({ name: `${info.emoji} Staff response`, value: `${note}${s.reviewerId ? `\n— <@${s.reviewerId}>` : ''}` });
  }
  return embed;
}

/** Up and Down buttons carrying the counts; disabled once voting is closed. */
export function buildSuggestionComponents(s) {
  const closed = !isVotingOpen(s.status);
  const button = (dir, emoji, count) => new ButtonBuilder()
    .setCustomId(buildCustomId('suggestion', s.id, dir))
    .setStyle(ButtonStyle.Secondary)
    .setEmoji(emoji)
    .setLabel(String(count ?? 0))
    .setDisabled(closed);
  return [new ActionRowBuilder().addComponents(button('up', '👍', s.upvotes), button('down', '👎', s.downvotes))];
}

export function buildSuggestionPayload(s, opts = {}) {
  return { embeds: [buildSuggestionEmbed(s, opts)], components: buildSuggestionComponents(s) };
}

/** The form `/suggestion submit` opens without text; the anonymous choice rides in the customId. */
export function buildSuggestionModal(anonymous = false) {
  return new ModalBuilder()
    .setCustomId(buildCustomId('suggestion-modal', anonymous ? '1' : '0'))
    .setTitle('Make a suggestion')
    .addComponents(new ActionRowBuilder().addComponents(
      new TextInputBuilder().setCustomId('suggestion-text').setLabel('Your suggestion').setStyle(TextInputStyle.Paragraph)
        .setRequired(true).setMinLength(1).setMaxLength(LIMITS.SUGGESTION_LENGTH)
        .setPlaceholder('What would you like to see on this server?'),
    ));
}

/** The DM an author gets when staff change their suggestion. */
export function buildReviewDm(guildName, s) {
  const info = STATUS_INFO[s.status] ?? STATUS_INFO.OPEN;
  const lines = [`> ${clampText(s.content, 300).replace(/\n/g, '\n> ')}`];
  if (s.reviewNote) lines.push('', `**Staff note:** ${clampText(s.reviewNote, SUGGESTION.NOTE_MAX)}`);
  const url = jumpUrl(s);
  if (url) lines.push('', `[View the suggestion](${url})`);
  return new EmbedBuilder()
    .setColor(info.color)
    .setTitle(clampText(`${info.emoji} Your suggestion #${s.number} ${info.dm}`, 256))
    .setDescription(lines.join('\n'))
    .setFooter({ text: clampText(guildName ?? 'Suggestions', 100) })
    .setTimestamp();
}

// ── Discord helpers ───────────────────────────────────────────────────────────

/** Name and avatar for the post. Anonymous posts never look the author up. */
export async function authorFor(client, guild, s) {
  if (s.anonymous) return { name: SUGGESTION.ANONYMOUS_NAME, iconURL: null };
  const member = guild?.members?.cache?.get?.(s.authorId);
  if (member) return { name: member.displayName ?? member.user?.username ?? 'Member', iconURL: member.displayAvatarURL?.() ?? null };
  let user = client?.users?.cache?.get?.(s.authorId) ?? null;
  if (!user) user = await client?.users?.fetch?.(s.authorId).catch(() => null) ?? null;
  if (!user) return { name: 'Unknown member', iconURL: null };
  return { name: user.displayName ?? user.username ?? 'Member', iconURL: user.displayAvatarURL?.() ?? null };
}

/** @returns {Promise<{ channel: object } | { gone: true } | { noAccess: true }>} `gone` only for a REST 10003 */
async function resolveChannel(client, channelId) {
  const cached = client.channels?.cache?.get(channelId);
  if (cached) return { channel: cached };
  try {
    const channel = await client.channels.fetch(channelId);
    return channel ? { channel } : { noAccess: true };
  } catch (err) {
    if (err?.code === UNKNOWN_CHANNEL) return { gone: true };
    if (NO_ACCESS_CODES.has(err?.code)) return { noAccess: true };
    throw err;
  }
}

const guildFor = (client, guildId) => {
  const guild = client.guilds?.cache?.get(guildId);
  return guild && guild.available !== false ? guild : null;
};

function sendFailureReason(err) {
  const code = err?.code;
  if (code === 50013 || code === 50001) return "I'm missing permissions there.";
  if (code === 50083) return 'that thread is archived.';
  if (code === UNKNOWN_CHANNEL) return 'the channel no longer exists.';
  return `Discord error ${code ?? 'unknown'}.`;
}

const deleteRowQuietly = (id) => prisma.suggestion.deleteMany({ where: { id } })
  .catch((err) => logger.warn('[suggestions] could not remove a reserved suggestion row', err));

// ── Create ────────────────────────────────────────────────────────────────────

/** Cooldown left in ms (0 = none), from the member's latest suggestion. `db` is prisma or a transaction client. */
async function cooldownLeft(db, guildId, userId, seconds) {
  if (!(seconds > 0)) return 0;
  const last = await db.suggestion.findFirst({
    where: { guildId, authorId: userId }, orderBy: { createdAt: 'desc' }, select: { createdAt: true },
  });
  return cooldownRemainingMs(last?.createdAt, seconds);
}

/** The member's cooldown left in ms. Read-only (for the pre-check before a modal); the create re-checks under the lock. */
export const cooldownLeftMs = (guildId, userId, settings) => cooldownLeft(prisma, guildId, userId, settings?.suggestionCooldownSeconds ?? 0);

/**
 * Posts a suggestion: reserves the numbered row (cooldown + number under one per-guild advisory lock, no Discord call inside
 * the transaction), sends the post, then opens the thread. A failed send removes the row, so it never costs the cooldown.
 * @returns {Promise<{ ok: true, suggestion: object, threadNote: string|null }
 *   | { ok: false, code: 'NO_CHANNEL'|'ANONYMOUS'|'CHANNEL'|'COOLDOWN'|'SEND'|'DB_ERROR', data?: object }>}
 */
export async function createSuggestion({ client, guild, user, content, anonymous = false, settings }) {
  const channelId = settings?.suggestionChannelId;
  if (!channelId) return { ok: false, code: 'NO_CHANNEL' };
  if (anonymous && !settings.suggestionAllowAnonymous) return { ok: false, code: 'ANONYMOUS' };

  let channel = guild.channels.cache.get(channelId) ?? null;
  if (!channel) channel = await guild.channels.fetch(channelId).catch(() => null);
  if (!channel) return { ok: false, code: 'CHANNEL', data: { reason: `<#${channelId}> no longer exists.` } };
  const usable = checkChannelUsable(channel, { needs: POST_NEEDS });
  if (!usable.ok) return { ok: false, code: 'CHANNEL', data: { reason: usable.reason } };

  let res;
  try {
    await ensureGuildSettings(guild.id);
    res = await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(numberLockSql(guild.id));
      const left = await cooldownLeft(tx, guild.id, user.id, settings.suggestionCooldownSeconds ?? 0);
      if (left > 0) return { cooldown: left };
      const [{ next }] = await tx.$queryRaw(nextNumberSql(guild.id));
      const row = await tx.suggestion.create({
        data: {
          guildId: guild.id, number: next, authorId: user.id, anonymous, content, channelId: channel.id, createdAt: new Date(),
        },
      });
      return { row };
    }, TX_OPTS);
  } catch (err) {
    logger.error(`[suggestions] ${guild.id}: reserving a suggestion failed`, err);
    return { ok: false, code: 'DB_ERROR' };
  }
  if (res.cooldown) return { ok: false, code: 'COOLDOWN', data: { ms: res.cooldown } };
  const { row } = res;

  let message;
  try {
    const author = await authorFor(client, guild, row);
    message = await channel.send({ ...buildSuggestionPayload(row, { author }), allowedMentions: NO_MENTIONS });
  } catch (err) {
    await deleteRowQuietly(row.id);
    logger.warn(`[suggestions] ${guild.id}: could not post suggestion #${row.number}: ${err?.message}`);
    return { ok: false, code: 'SEND', data: { reason: sendFailureReason(err) } };
  }

  let saved;
  try {
    saved = await prisma.suggestion.update({ where: { id: row.id }, data: { messageId: message.id } });
  } catch (err) {
    logger.error(`[suggestions] ${guild.id}: could not save the message of suggestion #${row.number}`, err);
    markBotDeletion(message.id, 'suggestions:rollback');
    await message.delete?.().catch(() => {});
    await deleteRowQuietly(row.id);
    return { ok: false, code: 'DB_ERROR' };
  }

  let threadNote = null;
  if (settings.suggestionThreads) {
    const missing = missingBotPermissions(channel, ['CreatePublicThreads']);
    if (missing.length) {
      threadNote = `I couldn't open a discussion thread: I need **${permissionNames(missing).join(', ')}** in <#${channel.id}>.`;
    } else {
      try {
        const thread = await message.startThread({
          name: `Suggestion #${row.number}`,
          autoArchiveDuration: ThreadAutoArchiveDuration.OneWeek,
          reason: `Suggestion #${row.number}`,
        });
        saved = await prisma.suggestion.update({ where: { id: row.id }, data: { threadId: thread.id } });
      } catch (err) {
        logger.warn(`[suggestions] ${guild.id}: could not open a thread for #${row.number}: ${err?.message}`);
        threadNote = "I couldn't open a discussion thread for it.";
      }
    }
  }
  return { ok: true, suggestion: saved, threadNote };
}

const CREATE_FAILURES = {
  NO_CHANNEL: noChannelEmbed,
  ANONYMOUS: () => errorEmbed('Anonymous Not Allowed', 'This server does not allow anonymous suggestions. Leave `anonymous` off.'),
  CHANNEL: (d) => errorEmbed('Cannot Post Suggestions', `${d?.reason ?? 'The suggestion channel is unavailable.'} Ask an admin to check it with \`/suggestion setup\`.`),
  COOLDOWN: (d) => cooldownEmbed(d?.ms ?? 0),
  SEND: (d) => errorEmbed('Could Not Post', `I couldn't post your suggestion: ${d?.reason ?? 'unknown error.'}`),
  DB_ERROR: () => errorEmbed('Could Not Save', 'Your suggestion could not be saved. Please try again in a moment.'),
};

/**
 * The whole submit flow after the interaction was deferred: module, content and channel checks, create, reply. Used by the
 * slash command (text option) and the modal. Never throws.
 */
export async function submitFromInteraction(interaction, client, { content, anonymous = false, settings }) {
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  try {
    if (!settings) return reply(errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'));
    const problem = submitProblem(settings, { anonymous });
    if (problem) return reply(problem);
    const parsed = parseContent(content);
    if (!parsed.ok) {
      return reply(parsed.error === 'empty'
        ? errorEmbed('Empty Suggestion', 'Write your suggestion first.')
        : errorEmbed('Too Long', `Suggestions can be at most **${LIMITS.SUGGESTION_LENGTH}** characters.`));
    }
    const out = await createSuggestion({
      client, guild: interaction.guild, user: interaction.user, content: parsed.content, anonymous, settings,
    });
    if (!out.ok) return reply(CREATE_FAILURES[out.code]?.(out.data) ?? CREATE_FAILURES.DB_ERROR());
    const s = out.suggestion;
    const url = jumpUrl(s);
    const lines = [url ? `Your suggestion is live as [#${s.number}](${url}).` : `Your suggestion is live as #${s.number}.`];
    if (anonymous) lines.push('It was posted anonymously.');
    if (out.threadNote) lines.push(`⚠️ ${out.threadNote}`);
    return reply(successEmbed('Suggestion Posted', lines.join('\n')));
  } catch (err) {
    logger.error('[suggestions] submit failed', err);
    return reply(CREATE_FAILURES.DB_ERROR());
  }
}

// ── Votes ─────────────────────────────────────────────────────────────────────

/**
 * The vote transaction. FOR UPDATE on the suggestion row serialises votes of one suggestion and against a review (a vote
 * either commits before the status changes or sees it closed). The totals are recounted from SuggestionVote, so they cannot
 * drift. Only `tx` is used inside.
 * @param {{ suggestionId: string, userId: string, value: 1|-1 }} args
 * @returns {Promise<{ result: 'gone' } | { result: 'closed', status: string }
 *   | { result: 'added'|'switched'|'removed', value: number, upvotes: number, downvotes: number }>}
 */
export async function applyVote({ suggestionId, userId, value }) {
  return prisma.$transaction(async (tx) => {
    const [row] = await tx.$queryRaw(lockRowSql(suggestionId));
    if (!row) return { result: 'gone' };
    if (!isVotingOpen(row.status)) return { result: 'closed', status: row.status };
    const key = { suggestionId_userId: { suggestionId, userId } };
    const existing = await tx.suggestionVote.findUnique({ where: key });
    const { next, result } = toggleVote(existing?.value ?? 0, value);
    if (next === 0) await tx.suggestionVote.deleteMany({ where: { suggestionId, userId } });
    else if (existing) await tx.suggestionVote.update({ where: key, data: { value: next } });
    else await tx.suggestionVote.create({ data: { suggestionId, userId, value: next } });
    const upvotes = await tx.suggestionVote.count({ where: { suggestionId, value: 1 } });
    const downvotes = await tx.suggestionVote.count({ where: { suggestionId, value: -1 } });
    await tx.suggestion.update({ where: { id: suggestionId }, data: { upvotes, downvotes } });
    return { result, value: next, upvotes, downvotes };
  }, TX_OPTS);
}

const voteReply = (number, dir, out) => {
  const emoji = dir === 'up' ? '👍' : '👎';
  if (out.result === 'added') return `${emoji} You ${dir === 'up' ? 'upvoted' : 'downvoted'} suggestion #${number}.`;
  if (out.result === 'switched') return `🔁 Your vote on suggestion #${number} changed to ${emoji}.`;
  return `🗑️ Your vote on suggestion #${number} was removed.`;
};

/** `suggestion:{id}:up|down`. Defers first; everything after may touch the database. */
export async function handleVoteButton(interaction, client) {
  const { params: [id, dir] } = parseCustomId(interaction.customId);
  if (!id || (dir !== 'up' && dir !== 'down')) {
    await safeReply(interaction, { content: 'This button is no longer active.', ephemeral: true });
    return;
  }
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (content) => safeReply(interaction, { content, allowedMentions: NO_MENTIONS });
  try {
    const s = await prisma.suggestion.findUnique({ where: { id } });
    if (!s || s.guildId !== interaction.guildId) {
      await reply('This suggestion no longer exists.');
      return;
    }
    const closedText = `Voting on suggestion #${s.number} is closed (${STATUS_INFO[s.status]?.label ?? s.status}).`;
    if (!isVotingOpen(s.status)) {
      // The post still shows live buttons (an edit was lost): fix it.
      const live = (interaction.message?.components ?? []).some((r) => (r.components ?? []).some((c) => !c.disabled));
      if (live) requestRender(client, s.id);
      await reply(closedText);
      return;
    }
    const out = await applyVote({ suggestionId: s.id, userId: interaction.user.id, value: dir === 'up' ? 1 : -1 });
    if (out.result === 'gone') await reply('This suggestion no longer exists.');
    else if (out.result === 'closed') {
      requestRender(client, s.id);
      await reply(closedText);
    } else {
      await reply(voteReply(s.number, dir, out));
      requestRender(client, s.id);
    }
  } catch (err) {
    logger.error('[suggestions] vote failed', err);
    await reply('Something went wrong while saving your vote. Please try again.');
  }
}

// ── Modal ─────────────────────────────────────────────────────────────────────

/** `suggestion-modal:{0|1}`: the form from `/suggestion submit`. Everything is checked again here. */
export async function handleSuggestionModal(interaction, client) {
  const { params: [anon] } = parseCustomId(interaction.customId);
  const content = interaction.fields.getTextInputValue('suggestion-text');
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const settings = await getSettingsSafe(interaction.guildId);
  await submitFromInteraction(interaction, client, { content, anonymous: anon === '1', settings });
}

// ── Debounced message edit ────────────────────────────────────────────────────

const renderMutex = createKeyedMutex();
const renderState = new Map();
const warnedAt = new Map();
let renderIntervalMs = SUGGESTION.RENDER_INTERVAL_MS;

function warnThrottled(key, message) {
  const now = Date.now();
  if (now - (warnedAt.get(key) ?? 0) < WARN_EVERY_MS) return;
  if (warnedAt.size > 200) for (const [k, t] of warnedAt) if (now - t >= WARN_EVERY_MS) warnedAt.delete(k);
  warnedAt.set(key, now);
  logger.warn(message);
}

/**
 * Edits the suggestion's post with the current row. The message id is cleared when Discord says the message or channel is
 * gone (this also heals a purge, which has no hook here).
 * @returns {Promise<'ok'|'skip'|'gone'>}
 */
async function renderSuggestionMessage(client, id) {
  const s = await prisma.suggestion.findUnique({ where: { id } });
  if (!s || !s.messageId) return 'skip';
  const guild = guildFor(client, s.guildId);
  if (!guild) return 'skip';
  const forget = async () => {
    await prisma.suggestion.updateMany({ where: { id, messageId: s.messageId }, data: { messageId: null } });
    return 'gone';
  };
  try {
    const found = await resolveChannel(client, s.channelId);
    if (found.gone) return await forget();
    if (found.noAccess) {
      warnThrottled(`render:${id}`, `[suggestions] ${s.guildId}: cannot reach the channel of suggestion #${s.number} to update it`);
      return 'skip';
    }
    const author = await authorFor(client, guild, s);
    await found.channel.messages.edit(s.messageId, { ...buildSuggestionPayload(s, { author }), allowedMentions: NO_MENTIONS });
    return 'ok';
  } catch (err) {
    if (err?.code === UNKNOWN_MESSAGE || err?.code === UNKNOWN_CHANNEL) return forget();
    if (NO_ACCESS_CODES.has(err?.code)) {
      warnThrottled(`render:${id}`, `[suggestions] ${s.guildId}: cannot edit suggestion #${s.number} (Discord ${err.code})`);
      return 'skip';
    }
    throw err;
  }
}

let renderImpl = renderSuggestionMessage;

/** Test hook: replaces the Discord render (no effect at import). Pass nothing to restore. */
export function __setRenderImpl(fn) {
  renderImpl = typeof fn === 'function' ? fn : renderSuggestionMessage;
}

/** Test hook: the debounce window in ms. Pass nothing to restore. */
export function __setRenderInterval(ms) {
  renderIntervalMs = Number.isFinite(ms) && ms >= 0 ? ms : SUGGESTION.RENDER_INTERVAL_MS;
}

/** Renders now under the suggestion's mutex (reviews). Never throws. @returns {Promise<'ok'|'skip'|'gone'|'error'>} */
export async function renderNow(client, id) {
  try {
    return await renderMutex.run(id, () => renderImpl(client, id));
  } catch (err) {
    logger.error(`[suggestions] render failed (${id})`, err);
    return 'error';
  }
}

/**
 * Asks for the post to be re-rendered: the first request after a quiet period renders at once, requests inside the window
 * collapse into one trailing render that reads the database when it runs (so the last vote is always shown). The timer is
 * cleared before the render starts, so a vote that commits meanwhile queues another trailing render instead of being lost.
 */
export function requestRender(client, id) {
  const s = renderState.get(id) ?? { lastAt: 0, timer: null };
  renderState.set(id, s);
  if (s.timer) return;
  if (renderState.size > RENDER_SWEEP_AT) {
    const now = Date.now();
    for (const [key, st] of renderState) if (!st.timer && key !== id && now - st.lastAt > RENDER_IDLE_MS) renderState.delete(key);
  }
  const wait = Math.max(0, s.lastAt + renderIntervalMs - Date.now());
  s.timer = setTimeout(() => {
    s.lastAt = Date.now();
    s.timer = null;
    renderNow(client, id).catch(() => {});
  }, wait);
  s.timer.unref?.();
}

/** Drops a queued render (the suggestion was deleted or lost its message). */
export function cancelRender(id) {
  const s = renderState.get(id);
  if (s?.timer) clearTimeout(s.timer);
  renderState.delete(id);
}

// ── Review and delete ─────────────────────────────────────────────────────────

/** Sends the author the review DM. DM failures (closed DMs) are ignored. @returns {Promise<'sent'|'failed'>} */
async function dmAuthor(client, guild, s) {
  try {
    const user = client.users.cache?.get?.(s.authorId) ?? await client.users.fetch(s.authorId);
    await user.send({ embeds: [buildReviewDm(guild?.name, s)], allowedMentions: NO_MENTIONS });
    return 'sent';
  } catch {
    return 'failed';
  }
}

/**
 * Sets a suggestion's status with the reviewer and an optional note, edits the post and DMs the author.
 * @returns {Promise<{ ok: true, suggestion: object, previous: string, edited: boolean, dm: 'sent'|'failed'|'off' }
 *   | { ok: false, code: 'NOT_FOUND' }>}
 */
export async function reviewSuggestion({ client, guild, number, status, note, reviewer, settings }) {
  const existing = await prisma.suggestion.findUnique({ where: { guildId_number: { guildId: guild.id, number } } });
  if (!existing) return { ok: false, code: 'NOT_FOUND' };
  const text = String(note ?? '').trim();
  const row = await prisma.suggestion.update({
    where: { id: existing.id },
    data: {
      status, reviewerId: reviewer.id, reviewNote: text ? clampText(text, SUGGESTION.NOTE_MAX) : null, reviewedAt: new Date(),
    },
  });
  const edited = (await renderNow(client, row.id)) === 'ok';
  let dm = 'off';
  if (settings?.suggestionDmOnUpdate && row.authorId !== reviewer.id) dm = await dmAuthor(client, guild, row);
  return { ok: true, suggestion: row, previous: existing.status, edited, dm };
}

/**
 * Deletes a suggestion: the post goes first (a failure other than "already gone" refuses and changes nothing), then the
 * discussion thread (best effort) and the row (votes cascade).
 * @returns {Promise<{ ok: true, suggestion: object } | { ok: false, code: 'NOT_FOUND'|'MESSAGE', data?: object }>}
 */
export async function deleteSuggestion({ client, guild, number }) {
  const s = await prisma.suggestion.findUnique({ where: { guildId_number: { guildId: guild.id, number } } });
  if (!s) return { ok: false, code: 'NOT_FOUND' };
  cancelRender(s.id);
  if (s.messageId) {
    try {
      const found = await resolveChannel(client, s.channelId);
      if (found.noAccess) return { ok: false, code: 'MESSAGE', data: { reason: `I can't reach <#${s.channelId}>.` } };
      if (found.channel) {
        markBotDeletion(s.messageId, 'suggestions:delete');
        await found.channel.messages.delete(s.messageId);
      }
    } catch (err) {
      if (err?.code !== UNKNOWN_MESSAGE && err?.code !== UNKNOWN_CHANNEL) {
        logger.warn(`[suggestions] ${guild.id}: could not delete the post of #${s.number}: ${err?.message}`);
        return { ok: false, code: 'MESSAGE', data: { reason: sendFailureReason(err) } };
      }
    }
  }
  if (s.threadId) {
    try {
      const thread = client.channels.cache?.get?.(s.threadId) ?? await client.channels.fetch(s.threadId);
      await thread?.delete?.(`Suggestion #${s.number} deleted`);
    } catch (err) {
      logger.debug(`[suggestions] ${guild.id}: thread of #${s.number} not deleted: ${err?.message}`);
    }
  }
  await prisma.suggestion.deleteMany({ where: { id: s.id } });
  return { ok: true, suggestion: s };
}

// ── Hooks ─────────────────────────────────────────────────────────────────────

/** messageDelete: the post vanished. The row keeps its data; only the message id is cleared. */
export async function onMessageDelete(ctx) {
  const messageId = ctx.message?.id;
  if (!messageId || !ctx.guildId) return;
  try {
    const rows = await prisma.suggestion.findMany({ where: { guildId: ctx.guildId, messageId }, select: { id: true } });
    if (!rows.length) return;
    await prisma.suggestion.updateMany({ where: { guildId: ctx.guildId, messageId }, data: { messageId: null } });
    for (const r of rows) cancelRender(r.id);
  } catch (err) {
    logger.error(`[suggestions] ${ctx.guildId}: clearing a deleted post failed`, err);
  }
}

/** channelDelete: the suggestion channel is gone, so it is unset; its posts lose their message id. */
export async function onChannelDelete(ctx) {
  const channelId = ctx.channel?.id;
  const guildId = ctx.guildId;
  if (!channelId || !guildId) return;
  try {
    const settings = await getSettings(guildId, { fresh: true });
    if (settings?.suggestionChannelId === channelId) {
      await updateSettings(guildId, { suggestionChannelId: null });
      logger.info(`[suggestions] ${guildId}: the suggestion channel was deleted, so it is unset`);
    }
    const rows = await prisma.suggestion.findMany({ where: { guildId, channelId, messageId: { not: null } }, select: { id: true } });
    if (rows.length) {
      await prisma.suggestion.updateMany({ where: { guildId, channelId, messageId: { not: null } }, data: { messageId: null } });
      for (const r of rows) cancelRender(r.id);
    }
  } catch (err) {
    logger.error(`[suggestions] ${guildId}: channel delete cleanup failed`, err);
  }
}
