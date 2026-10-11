// Polls (pkg-polls.md): MEE6 /poll + /poll-end. Modal-built polls with 2-20 options, persistent button (single choice) or
// select (multi choice) voting routed through componentRegistry, role allow/deny checked at vote time, atomic votes
// (advisory lock per (poll, user) + FOR SHARE on the poll row), live results throttled to one edit per poll per 5 s and a
// scheduler-timed end (`poll.end`). Everything durable is in Postgres; the only in-process state is the pending create forms
// and the render throttle, both allowed to vanish on a restart. Side-effect free at import (Maps only: no timers, no I/O).
import { Prisma } from '@prisma/client';
import {
  ActionRowBuilder, ButtonBuilder, ButtonStyle, MessageFlags, ModalBuilder, PermissionFlagsBits,
  StringSelectMenuBuilder, StringSelectMenuOptionBuilder, TextInputBuilder, TextInputStyle,
} from 'discord.js';
import prisma from '../database/client.js';
import { LIMITS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { cancelJobs, PermanentJobError, scheduleJob } from './scheduler.js';
import { ensureGuildSettings, getSettingsSafe, isModuleEnabled } from './settingsService.js';
import { createKeyedMutex } from '../utils/async.js';
import { errorEmbed, guildEmbed, successEmbed } from '../utils/embedBuilder.js';
import { checkChannelUsable, isInteractionGone, permissionNames, safeDefer, safeReply } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { parseDuration } from '../utils/parseDuration.js';
import { clampText, formatNumber, joinWithLimit, progressBar } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const POLL = Object.freeze({
  QUESTION_MAX: 256, OPTION_MAX: 80, OPTIONS_MIN: 2, OPTIONS_MAX: LIMITS.POLL_OPTIONS,
  ACTIVE_MAX: LIMITS.ACTIVE_POLLS, OPTIONS_INPUT_MAX: 2000,
  DURATION_MIN_MS: 60_000, DURATION_MAX_MS: 30 * 86_400_000,
  PENDING_TTL_MS: 15 * 60_000, PENDING_MAX: 500,
  RENDER_INTERVAL_MS: 5_000, BUTTONS_PER_ROW: 5, BAR_WIDTH: 10,
});

export const END_JOB_TYPE = 'poll.end';
export const endJobKey = (pollId) => `poll.end:${pollId}`;

const NO_MENTIONS = Object.freeze({ parse: [] });
const TX_OPTS = Object.freeze({ timeout: 10_000, maxWait: 10_000 });
const RENDER_SWEEP_AT = 500;
const RENDER_IDLE_MS = 10 * 60_000;
const WARN_EVERY_MS = 10 * 60_000;

// Discord API error codes we react to.
const UNKNOWN_CHANNEL = 10003;
const UNKNOWN_MESSAGE = 10008;
const NO_ACCESS_CODES = new Set([50001, 50013, 50083]); // missing access / permissions / archived thread

// ── Raw SQL (00-overview rule 22: no timestamps are involved, so no SQL_NOW) ───────────

const createLockSql = (guildId) => Prisma.sql`SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext(${`poll-create:${guildId}`}))`;
const voteLockSql = (pollId, userId) => Prisma.sql`SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext(${`poll-vote:${pollId}:${userId}`}))`;
const pollStatusSql = (pollId) => Prisma.sql`SELECT status::text AS status FROM "Poll" WHERE id = ${pollId} FOR SHARE`;
const tallyTotalsSql = (pollId) => Prisma.sql`SELECT COUNT(*)::int AS votes, COUNT(DISTINCT "userId")::int AS voters FROM "PollVote" WHERE "pollId" = ${pollId}`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'polls.createLock', build: () => createLockSql('0') },
  { name: 'polls.voteLock', build: () => voteLockSql('0', '0') },
  { name: 'polls.status', build: () => pollStatusSql('0') },
  { name: 'polls.totals', build: () => tallyTotalsSql('0') },
]);

// ── Pure helpers ──────────────────────────────────────────────────────────────

/**
 * @param {string|null|undefined} raw the `duration` option
 * @returns {{ ms: number|null } | { error: 'invalid'|'too-short'|'too-long' }}
 */
export function parseDurationOption(raw) {
  if (raw == null || String(raw).trim() === '') return { ms: null };
  const ms = parseDuration(String(raw).trim());
  if (ms == null) return { error: 'invalid' };
  if (ms < POLL.DURATION_MIN_MS) return { error: 'too-short' };
  if (ms > POLL.DURATION_MAX_MS) return { error: 'too-long' };
  return { ms };
}

/** @returns {{ question: string, errors: string[] }} */
export function validateQuestion(raw) {
  const question = String(raw ?? '').trim();
  const errors = [];
  if (!question) errors.push('The question is empty.');
  else if (question.length > POLL.QUESTION_MAX) errors.push(`The question is ${question.length} characters (max ${POLL.QUESTION_MAX}).`);
  return { question, errors };
}

const LIST_MARKER_RE = /^(?:[-*•]|\d{1,2}[.)])\s+/;

/**
 * One option per line; a leading list marker ("1.", "2)", "-", "*", "•") is stripped. Every problem is collected.
 * @returns {{ options: string[], errors: string[] }}
 */
export function parsePollOptions(raw) {
  const options = String(raw ?? '')
    .split(/\r?\n/)
    .map((line) => line.trim().replace(LIST_MARKER_RE, '').trim())
    .filter(Boolean);
  const errors = [];
  if (options.length < POLL.OPTIONS_MIN) errors.push('Add at least 2 options (one per line).');
  if (options.length > POLL.OPTIONS_MAX) errors.push(`You entered ${options.length} options (max ${POLL.OPTIONS_MAX}).`);
  let tooLong = 0;
  options.forEach((o, i) => {
    if (o.length <= POLL.OPTION_MAX) return;
    tooLong += 1;
    if (tooLong <= 5) errors.push(`Option ${i + 1} is ${o.length} characters (max ${POLL.OPTION_MAX}).`);
    else if (tooLong === 6) errors.push('…');
  });
  const first = new Map();
  options.forEach((o, j) => {
    const key = o.toLowerCase();
    if (first.has(key)) errors.push(`Option ${j + 1} repeats option ${first.get(key) + 1}.`);
    else first.set(key, j);
  });
  return { options, errors };
}

/** @returns {string[]} the role ids of a member object (cached GuildMember or API member with an id array) */
export function memberRoleIds(member) {
  if (Array.isArray(member?.roles)) return member.roles;
  return [...(member?.roles?.cache?.keys() ?? [])];
}

/**
 * Deny wins over allow. A deleted allowed role is ignored, but an allow list that pointed ONLY at deleted roles refuses
 * everyone instead of opening the poll up.
 * @param {string[]} roleIds roles the voter holds
 * @param {{ allowedRoleIds?: string[], deniedRoleIds?: string[] }} poll
 * @param {Set<string>|null} [existingRoleIds] the guild's current role ids
 * @returns {{ ok: true } | { ok: false, reason: 'denied'|'not-allowed'|'allowed-role-gone', roleId?: string }}
 */
export function canVote(roleIds, poll, existingRoleIds = null) {
  const held = new Set(roleIds ?? []);
  const denied = (poll.deniedRoleIds ?? []).find((id) => held.has(id));
  if (denied) return { ok: false, reason: 'denied', roleId: denied };
  const configured = poll.allowedRoleIds ?? [];
  const allowed = existingRoleIds ? configured.filter((id) => existingRoleIds.has(id)) : configured;
  if (configured.length > 0 && allowed.length === 0) return { ok: false, reason: 'allowed-role-gone' };
  if (allowed.length > 0 && !allowed.some((id) => held.has(id))) return { ok: false, reason: 'not-allowed', roleId: allowed[0] };
  return { ok: true };
}

/**
 * Toggle one option for a voter.
 * @param {number[]} current the voter's current option indexes
 * @returns {{ next: number[], result: 'added'|'removed'|'switched'|'limit' }}
 */
export function applyVoteToggle(current, idx, maxChoices) {
  const cur = [...current].sort((a, b) => a - b);
  if (cur.includes(idx)) return { next: cur.filter((i) => i !== idx), result: 'removed' };
  if (maxChoices === 1) return { next: [idx], result: cur.length > 0 ? 'switched' : 'added' };
  if (cur.length >= maxChoices) return { next: cur, result: 'limit' };
  return { next: [...cur, idx].sort((a, b) => a - b), result: 'added' };
}

/** @returns {number[]|null} the validated, sorted, de-duplicated selection, or null when any value is bad */
export function normalizeSelection(values, optionCount, maxChoices) {
  const out = new Set();
  for (const v of values ?? []) {
    if (!/^\d{1,2}$/.test(String(v))) return null;
    const n = Number(v);
    if (n >= optionCount) return null;
    out.add(n);
  }
  if (out.size > maxChoices) return null;
  return [...out].sort((a, b) => a - b);
}

export const emptyTally = (n) => ({ counts: new Array(n).fill(0), votes: 0, voters: 0 });

/** @returns {{ counts: number[], votes: number, voters: number }} */
export function tallyFromGroups(n, groups, { votes = 0, voters = 0 } = {}) {
  const counts = new Array(n).fill(0);
  for (const g of groups ?? []) {
    const i = g?.optionIndex;
    if (Number.isInteger(i) && i >= 0 && i < n) counts[i] = Number(g._count?._all) || 0;
  }
  // The two queries are not one snapshot: never report fewer votes than the bars add up to.
  const sum = counts.reduce((a, b) => a + b, 0);
  return { counts, votes: Math.max(Number(votes) || 0, sum), voters: Number(voters) || 0 };
}

/** @returns {number[]} indexes with the highest count, [] when nobody voted */
export function winners(counts) {
  const max = Math.max(0, ...counts);
  return max === 0 ? [] : counts.flatMap((c, i) => (c === max ? [i] : []));
}

/** Percentages are relative to the total number of votes, so they add up to about 100 even in multi-choice polls. */
export const percent = (count, total) => (total ? Math.round((count * 100) / total) : 0);

const plural = (n, word) => `${word}${n === 1 ? '' : 's'}`;

/** @returns {string} one block per option, joined with newlines */
export function formatOptionLines(options, tally, { hidden = false, ended = false } = {}) {
  const counts = tally?.counts ?? [];
  const total = Math.max(Number(tally?.votes) || 0, counts.reduce((a, b) => a + b, 0));
  const win = ended && !hidden ? new Set(winners(counts)) : new Set();
  return options.map((opt, i) => {
    if (hidden) return `**${i + 1}.** ${opt}`;
    const c = counts[i] ?? 0;
    const bar = progressBar(total ? c / total : 0, POLL.BAR_WIDTH);
    return `**${i + 1}.** ${win.has(i) ? '🏆 ' : ''}${opt}\n${bar} **${percent(c, total)}%** · ${formatNumber(c)} ${plural(c, 'vote')}`;
  }).join('\n');
}

/** @returns {string} a button label: "1. Yes" for short option sets, otherwise just the number */
export function buttonLabel(options, i) {
  const compact = options.length <= 10 && options.every((o) => o.length <= 25);
  return clampText(compact ? `${i + 1}. ${options[i]}` : String(i + 1), 80);
}

/**
 * Vote components: one button per option (single choice) or one select (multi choice).
 * @param {{ id: string, options: string[], maxChoices: number }} poll
 * @param {{ disabled?: boolean, selected?: number[]|null }} [opts] `selected` pre-fills the select (personal ballot)
 * @returns {ActionRowBuilder[]}
 */
export function buildPollComponents(poll, { disabled = false, selected = null } = {}) {
  if (poll.maxChoices === 1) {
    const rows = [];
    poll.options.forEach((_, i) => {
      if (i % POLL.BUTTONS_PER_ROW === 0) rows.push(new ActionRowBuilder());
      rows[rows.length - 1].addComponents(
        new ButtonBuilder()
          .setCustomId(buildCustomId('poll', 'vote', poll.id, String(i)))
          .setLabel(buttonLabel(poll.options, i))
          .setStyle(ButtonStyle.Secondary)
          .setDisabled(disabled),
      );
    });
    return rows;
  }
  const select = new StringSelectMenuBuilder()
    .setCustomId(buildCustomId('poll-select', poll.id))
    .setMinValues(0)
    .setMaxValues(poll.maxChoices)
    .setPlaceholder(disabled ? 'This poll has ended' : `Choose up to ${poll.maxChoices} answers`)
    .setDisabled(disabled)
    .addOptions(poll.options.map((opt, i) => new StringSelectMenuOptionBuilder()
      .setLabel(clampText(`${i + 1}. ${opt}`, 100))
      .setValue(String(i))
      .setDefault(selected?.includes(i) ?? false)));
  return [new ActionRowBuilder().addComponents(select)];
}

const votesLine = (poll, tally) => `🗳️ **${formatNumber(tally.votes)}** ${plural(tally.votes, 'vote')} from **${formatNumber(tally.voters)}** ${plural(tally.voters, 'member')} · ${poll.maxChoices === 1 ? 'one choice per member' : `up to ${poll.maxChoices} choices per member`}`;
const endsLine = (endsAt) => `⏰ Ends ${discordTimestamp(endsAt, 'R')} (${discordTimestamp(endsAt, 'f')})`;

/** @returns {import('discord.js').EmbedBuilder} the poll message embed */
export function buildPollEmbed(poll, tally, { settings = null, ended = poll.status === 'ENDED' } = {}) {
  const hidden = poll.hideResults && !ended;
  const lines = [formatOptionLines(poll.options, tally, { hidden, ended }), ''];
  if (ended) lines.push(`🏁 **Poll ended**${poll.endsAt ? ` ${discordTimestamp(poll.endsAt, 'R')}` : ''}`);
  lines.push(votesLine(poll, tally));
  if (!ended) lines.push(poll.endsAt ? endsLine(poll.endsAt) : '⏰ Open until a moderator ends it');
  if (!ended && poll.hideResults) lines.push('🔒 Results are hidden until the poll ends.');
  const allowed = poll.allowedRoleIds ?? [];
  const denied = poll.deniedRoleIds ?? [];
  if (allowed.length) lines.push(`✅ Only ${allowed.map((r) => `<@&${r}>`).join(', ')} can vote.`);
  if (denied.length) lines.push(`🚫 ${denied.map((r) => `<@&${r}>`).join(', ')} can't vote.`);
  lines.push(`👤 Started by <@${poll.authorId}>`);
  let footer;
  if (ended) footer = 'This poll has ended';
  else if (poll.maxChoices === 1) footer = 'Click a button to vote · click it again to remove your vote';
  else footer = 'Pick your answers in the menu · each pick replaces your previous votes';
  return guildEmbed(settings, clampText(`📊 ${poll.question}`, 256), clampText(lines.join('\n'), 4096), footer)
    .setTimestamp(poll.createdAt);
}

/** @returns {{ embeds: object[], components: ActionRowBuilder[], allowedMentions: object }} */
export function buildPollPayload(poll, tally, { settings = null, ended = poll.status === 'ENDED' } = {}) {
  return {
    embeds: [buildPollEmbed(poll, tally, { settings, ended })],
    components: buildPollComponents(poll, { disabled: ended }),
    allowedMentions: NO_MENTIONS,
  };
}

/** @returns {string|null} the message link, or null before the first send */
export function jumpUrl(poll) {
  return poll.messageId ? `https://discord.com/channels/${poll.guildId}/${poll.channelId}/${poll.messageId}` : null;
}

function summaryLine(poll, tally) {
  const w = winners(tally.counts);
  if (!w.length) return 'No votes were cast.';
  const c = tally.counts[w[0]];
  if (w.length === 1) {
    return `🏆 **${clampText(poll.options[w[0]], POLL.OPTION_MAX)}** won with **${formatNumber(c)}** ${plural(c, 'vote')} (${percent(c, tally.votes)}%).`;
  }
  const names = w.map((i) => `**${clampText(poll.options[i], POLL.OPTION_MAX)}**`);
  return `🤝 Tie between ${joinWithLimit(names, { max: 1024 })} with **${formatNumber(c)}** ${plural(c, 'vote')} each.`;
}

/**
 * The results embed: the posted message when a poll ends (`live: false`) and `/poll results` (private peek).
 * @returns {import('discord.js').EmbedBuilder}
 */
export function buildResultsEmbed(poll, tally, { settings = null, live = false, messageExists = true } = {}) {
  const lines = [`**${clampText(poll.question, 512)}**`];
  if (!live) lines.push(summaryLine(poll, tally));
  lines.push('', formatOptionLines(poll.options, tally, { hidden: false, ended: !live }), '', votesLine(poll, tally));
  if (live && poll.endsAt) lines.push(endsLine(poll.endsAt));
  if (live && poll.hideResults) lines.push("🔒 Members can't see these results until the poll ends.");
  const url = messageExists ? jumpUrl(poll) : null;
  if (url) lines.push(`[Jump to the poll](${url})`);
  return guildEmbed(settings, live ? '📊 Live results' : '📊 Poll results', clampText(lines.join('\n'), 4096));
}

/**
 * Map with insertion order: `put` sweeps expired entries and evicts the oldest beyond `max`; `take` deletes and returns the
 * value (null when missing or older than `ttlMs`).
 * @returns {{ put: (key: string, value: any) => void, take: (key: string) => any, size: () => number }}
 */
export function createPendingStore({ ttlMs, max, now = Date.now }) {
  const map = new Map();
  return {
    put(key, value) {
      const t = now();
      for (const [k, e] of map) {
        if (t - e.at > ttlMs) map.delete(k);
        else break; // insertion order = age order
      }
      map.delete(key);
      map.set(key, { value, at: t });
      while (map.size > max) map.delete(map.keys().next().value);
    },
    take(key) {
      const e = map.get(key);
      if (!e) return null;
      map.delete(key);
      return now() - e.at > ttlMs ? null : e.value;
    },
    size: () => map.size,
  };
}

export const pendingPolls = createPendingStore({ ttlMs: POLL.PENDING_TTL_MS, max: POLL.PENDING_MAX });

/** The create modal; the nonce is the /poll create interaction id. */
export function buildCreateModal(nonce) {
  return new ModalBuilder()
    .setCustomId(buildCustomId('poll-modal', nonce))
    .setTitle('Create a poll')
    .addComponents(
      new ActionRowBuilder().addComponents(
        new TextInputBuilder().setCustomId('poll-question').setLabel('Question').setStyle(TextInputStyle.Short)
          .setRequired(true).setMinLength(1).setMaxLength(POLL.QUESTION_MAX).setPlaceholder('What should we play tonight?'),
      ),
      new ActionRowBuilder().addComponents(
        new TextInputBuilder().setCustomId('poll-options').setLabel('Options (one per line, 2–20)').setStyle(TextInputStyle.Paragraph)
          .setRequired(true).setMinLength(3).setMaxLength(POLL.OPTIONS_INPUT_MAX).setPlaceholder('Option 1\nOption 2\nOption 3'),
      ),
    );
}

/**
 * The checks every poll location must pass, for the bot and for the moderator (messages C1-C3).
 * @returns {import('discord.js').EmbedBuilder|null} an error embed, or null when the channel is fine
 */
export function channelProblem(channel, member) {
  if (!channel) return errorEmbed('Channel Not Found', "I can't see that channel. Pick one with the `channel` option.");
  const usable = checkChannelUsable(channel);
  if (!usable.ok) {
    return errorEmbed('Cannot Post There', usable.missing.length
      ? `I need **${permissionNames(usable.missing).join(', ')}** in <#${channel.id}> to post a poll.`
      : `${usable.reason}.`);
  }
  const need = [PermissionFlagsBits.ViewChannel, channel.isThread?.() ? PermissionFlagsBits.SendMessagesInThreads : PermissionFlagsBits.SendMessages];
  if (!channel.permissionsFor?.(member)?.has(need)) {
    return errorEmbed('Cannot Post There', `You need permission to view and send messages in <#${channel.id}>.`);
  }
  return null;
}

/** @returns {string} why `channel.send` failed, as a sentence fragment (message P1) */
export function sendFailureReason(err) {
  const code = err?.code;
  if (code === 50013 || code === 50001) return "I'm missing permissions there.";
  if (code === 50083) return 'that thread is archived.';
  if (code === UNKNOWN_CHANNEL) return 'the channel no longer exists.';
  return `Discord error ${code ?? 'unknown'}.`;
}

// ── Tally ─────────────────────────────────────────────────────────────────────

/** @returns {Promise<{ counts: number[], votes: number, voters: number }>} */
export async function getTally(poll) {
  const [groups, rows] = await Promise.all([
    prisma.pollVote.groupBy({ by: ['optionIndex'], where: { pollId: poll.id }, _count: { _all: true } }),
    prisma.$queryRaw(tallyTotalsSql(poll.id)),
  ]);
  return tallyFromGroups(poll.options.length, groups, rows?.[0] ?? { votes: 0, voters: 0 });
}

// ── Creating ──────────────────────────────────────────────────────────────────

/**
 * Inserts the poll under the guild cap. The advisory lock makes the cap exact even when two processes briefly overlap.
 * @returns {Promise<{ ok: true, poll: object } | { ok: false, code: 'LIMIT' }>}
 */
export async function createPollRow(guildId, pending, { question, options, maxChoices }) {
  await ensureGuildSettings(guildId);
  return prisma.$transaction(async (tx) => {
    await tx.$queryRaw(createLockSql(guildId));
    const active = await tx.poll.count({ where: { guildId, status: 'ACTIVE' } });
    if (active >= POLL.ACTIVE_MAX) return { ok: false, code: 'LIMIT' };
    const endsAt = pending.durationMs ? new Date(Date.now() + pending.durationMs) : null;
    const poll = await tx.poll.create({
      data: {
        guildId, channelId: pending.channelId, authorId: pending.userId, question, options, maxChoices,
        allowedRoleIds: pending.allowedRoleIds ?? [], deniedRoleIds: pending.deniedRoleIds ?? [],
        hideResults: pending.hideResults === true, endsAt,
      },
    });
    return { ok: true, poll };
  }, TX_OPTS);
}

/**
 * Ends (reason 'deleted') every active poll of the guild whose channel is gone. Acts only on a REST 10003 while the guild is
 * available (rule 20): a channel missing from the cache is never judged deleted.
 * @returns {Promise<number>} polls ended
 */
export async function reconcileDeletedChannels(client, guild) {
  if (!guild?.available) return 0;
  const rows = await prisma.poll.findMany({ where: { guildId: guild.id, status: 'ACTIVE' }, select: { id: true, channelId: true } });
  const byChannel = new Map();
  for (const r of rows) {
    if (!byChannel.has(r.channelId)) byChannel.set(r.channelId, []);
    byChannel.get(r.channelId).push(r.id);
  }
  let ended = 0;
  for (const [channelId, ids] of byChannel) {
    if (guild.channels.cache.has(channelId)) continue;
    try {
      await guild.channels.fetch(channelId);
      continue; // it exists (archived threads are often uncached)
    } catch (err) {
      if (err?.code !== UNKNOWN_CHANNEL || !guild.available) continue;
    }
    for (const id of ids) {
      const r = await endPoll(client, id, { reason: 'deleted', guildId: guild.id });
      if (r.ok) ended += 1;
    }
  }
  return ended;
}

/**
 * Creates the row; on LIMIT it first reconciles polls whose channel was deleted, then retries once.
 * @returns {Promise<{ ok: true, poll: object } | { ok: false, code: 'LIMIT' }>}
 */
export async function createPoll(client, guild, pending, input) {
  let res = await createPollRow(guild.id, pending, input);
  if (!res.ok) {
    try {
      if ((await reconcileDeletedChannels(client, guild)) > 0) res = await createPollRow(guild.id, pending, input);
    } catch (err) {
      logger.warn('poll: reconciling deleted channels failed', err);
    }
  }
  return res;
}

const echoSafe = (s) => String(s ?? '').replaceAll('```', '`​``');

/** The "copy it to try again" block appended to expired/invalid form errors; keeps the whole description within 4096. */
function echoBlock(question, optionsRaw, baseLength) {
  const head = '\n\nYour input (copy it to try again):\n```\n';
  const tail = '\n```';
  const room = Math.max(0, 4096 - baseLength - head.length - tail.length);
  return head + clampText(`${echoSafe(question)}\n---\n${echoSafe(optionsRaw)}`, room) + tail;
}

// ── Modal submit ──────────────────────────────────────────────────────────────

/** `poll-modal:{nonce}`: validates the form, posts the poll and schedules its end. */
export async function handleCreateModal(interaction, client) {
  const { params: [nonce] } = parseCustomId(interaction.customId);
  const question = interaction.fields.getTextInputValue('poll-question');
  const optionsRaw = interaction.fields.getTextInputValue('poll-options');
  const fail = (embed) => safeReply(interaction, { embeds: [embed], ephemeral: true });

  const pending = pendingPolls.take(nonce);
  if (!pending) {
    const base = 'This poll form has expired (forms last 15 minutes and do not survive a bot restart). Run `/poll create` again.';
    await fail(errorEmbed('Form Expired', base + echoBlock(question, optionsRaw, base.length)));
    return;
  }
  if (pending.userId !== interaction.user.id || pending.guildId !== interaction.guildId) {
    await fail(errorEmbed('Form Expired', 'This form is not yours. Run `/poll create` to start a poll.'));
    return;
  }

  const q = validateQuestion(question);
  const o = parsePollOptions(optionsRaw);
  const problems = [...q.errors, ...o.errors];
  if (problems.length) {
    const base = clampText(problems.map((e) => `• ${e}`).join('\n'), 1500);
    await fail(errorEmbed('Invalid Poll', base + echoBlock(question, optionsRaw, base.length)));
    return;
  }

  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed] });
  const { guild } = interaction;
  const guildId = interaction.guildId;

  const maxChoices = Math.min(pending.maxChoices, o.options.length);
  const clamped = maxChoices < pending.maxChoices;

  const channel = guild.channels.cache.get(pending.channelId) ?? await guild.channels.fetch(pending.channelId).catch(() => null);
  const problem = channelProblem(channel, interaction.member);
  if (problem) {
    await reply(problem);
    return;
  }

  const created = await createPoll(client, guild, pending, { question: q.question, options: o.options, maxChoices });
  if (!created.ok) {
    await reply(errorEmbed('Too Many Polls', `This server already has **${POLL.ACTIVE_MAX}** active polls. End one with \`/poll end\` first.`));
    return;
  }
  const { poll } = created;

  let msg;
  try {
    const settings = await getSettingsSafe(guildId);
    msg = await channel.send(buildPollPayload(poll, emptyTally(poll.options.length), { settings }));
  } catch (err) {
    logger.warn('poll: could not post the poll message', err);
    await prisma.poll.delete({ where: { id: poll.id } }).catch(() => {});
    await reply(errorEmbed('Could Not Post Poll', `I couldn't post the poll in <#${channel.id}>: ${sendFailureReason(err)}`));
    return;
  }
  try {
    await prisma.poll.update({ where: { id: poll.id }, data: { messageId: msg.id } });
  } catch (err) {
    logger.error('poll: could not store the poll message id', err);
    await msg.delete?.().catch(() => {});
    await prisma.poll.delete({ where: { id: poll.id } }).catch(() => {});
    await reply(errorEmbed('Could Not Post Poll', "I couldn't save the poll. Please try again in a moment."));
    return;
  }
  const posted = { ...poll, messageId: msg.id };

  let timerFailed = false;
  if (pending.durationMs) {
    try {
      await scheduleJob({
        guildId, type: END_JOB_TYPE, runAt: poll.endsAt, payload: { pollId: poll.id }, dedupeKey: endJobKey(poll.id),
      });
    } catch (err) {
      timerFailed = true;
      logger.error('poll: could not schedule poll.end', err);
    }
  }

  const lines = [`Your poll is live in <#${channel.id}>. [Jump to poll](${jumpUrl(posted)})`];
  if (pending.durationMs) lines.push(`It ends ${discordTimestamp(poll.endsAt, 'R')}.`);
  if (clamped) lines.push(`Max choices was lowered to **${maxChoices}** (the number of options).`);
  if (timerFailed) lines.push('⚠️ The end timer could not be scheduled. End the poll with `/poll end`.');
  await reply(successEmbed('Poll Posted', lines.join('\n')));
}

// ── Voting ────────────────────────────────────────────────────────────────────

const EXPIRED_TEXT = 'This poll has ended.';

/**
 * The vote transaction. The advisory lock on (poll, user) guards "one option in a single-choice poll" (the unique index only
 * covers (pollId, userId, optionIndex)); FOR SHARE on the poll row serialises votes against endPoll's UPDATE: a vote either
 * commits before the poll ends (and is in the final tally) or sees ENDED and is refused. Only `tx` is used inside.
 * @param {{ pollId: string, userId: string, maxChoices: number, idx?: number, set?: number[] }} args `set` replaces the vote set
 * @returns {Promise<{ result: 'gone'|'ended' } | { result: string, next: number[], changed: boolean }>}
 */
export async function applyVote({ pollId, userId, maxChoices, idx = null, set = null }) {
  return prisma.$transaction(async (tx) => {
    await tx.$queryRaw(voteLockSql(pollId, userId));
    const [row] = await tx.$queryRaw(pollStatusSql(pollId));
    if (!row) return { result: 'gone' };
    if (row.status !== 'ACTIVE') return { result: 'ended' };
    const current = (await tx.pollVote.findMany({ where: { pollId, userId }, select: { optionIndex: true } }))
      .map((v) => v.optionIndex).sort((a, b) => a - b);
    let next;
    let result;
    if (set) {
      next = set;
      result = set.length ? 'set' : 'cleared';
    } else {
      ({ next, result } = applyVoteToggle(current, idx, maxChoices));
    }
    if (result === 'limit') return { result, next: current, changed: false };
    const del = current.filter((i) => !next.includes(i));
    const add = next.filter((i) => !current.includes(i));
    if (del.length) await tx.pollVote.deleteMany({ where: { pollId, userId, optionIndex: { in: del } } });
    if (add.length) await tx.pollVote.createMany({ data: add.map((i) => ({ pollId, userId, optionIndex: i })), skipDuplicates: true });
    return { result, next, changed: del.length > 0 || add.length > 0 };
  }, TX_OPTS);
}

/** Re-renders the message of an already ended poll whose components are still live (the process died mid-end). */
async function healEndedMessage(interaction, poll) {
  try {
    const msg = interaction.message;
    if (!msg || msg.id !== poll.messageId) return;
    const live = (msg.components ?? []).some((row) => (row.components ?? []).some((c) => !c.disabled));
    if (!live) return;
    await renderMutex.run(poll.id, async () => {
      const tally = await getTally(poll);
      const settings = await getSettingsSafe(poll.guildId);
      await msg.edit(buildPollPayload(poll, tally, { settings, ended: true }));
    });
  } catch (err) {
    logger.warn(`poll: could not finalise the message of ended poll ${poll.id}`, err);
  }
}

/**
 * Steps shared by the button and the select: the poll exists, is open, the option exists and the voter may vote.
 * @returns {Promise<{ poll: object } | { fail: string, terminal: boolean }>}
 */
async function prepareVote(interaction, client, pollId, { idx = null, heal = true } = {}) {
  const poll = await prisma.poll.findUnique({ where: { id: pollId } });
  if (!poll || poll.guildId !== interaction.guildId) return { fail: 'This poll no longer exists.', terminal: true };
  if (poll.status === 'ENDED') {
    if (heal) await healEndedMessage(interaction, poll);
    return { fail: EXPIRED_TEXT, terminal: true };
  }
  if (poll.endsAt && poll.endsAt.getTime() <= Date.now()) {
    // The end job is late or missing: end it now (the conditional update picks one winner).
    await endPoll(client, poll.id, { reason: 'expired', guildId: poll.guildId });
    return { fail: EXPIRED_TEXT, terminal: true };
  }
  if (idx != null && idx >= poll.options.length) return { fail: 'That option no longer exists.', terminal: false };
  if (!interaction.member) return { fail: "I can't check your roles right now. Please try again in a moment.", terminal: false };
  const existing = interaction.guild?.roles?.cache?.size ? new Set(interaction.guild.roles.cache.keys()) : null;
  const check = canVote(memberRoleIds(interaction.member), poll, existing);
  if (!check.ok) {
    if (check.reason === 'not-allowed') return { fail: `Only members with <@&${check.roleId}> can vote in this poll.`, terminal: false };
    if (check.reason === 'denied') return { fail: `Members with <@&${check.roleId}> can't vote in this poll.`, terminal: false };
    return { fail: 'The role allowed to vote in this poll no longer exists. Ask a moderator to end it with /poll end.', terminal: false };
  }
  return { poll };
}

const optionName = (poll, i) => `**${i + 1}. ${clampText(poll.options[i], POLL.OPTION_MAX)}**`;
const listOf = (poll, indexes) => joinWithLimit(indexes.map((i) => optionName(poll, i)), { max: 1800 });
const VOTE_FAILED = 'Something went wrong while saving your vote. Please try again.';

function buttonReply(poll, idx, out) {
  const multi = poll.maxChoices > 1;
  if (out.result === 'limit') return `You can pick at most **${poll.maxChoices}** options. Remove a vote first.`;
  if (!multi) {
    if (out.result === 'added') return `✅ You voted for ${optionName(poll, idx)}.`;
    if (out.result === 'switched') return `🔁 Your vote changed to ${optionName(poll, idx)}.`;
    return `🗑️ Your vote for ${optionName(poll, idx)} was removed.`;
  }
  const head = out.result === 'added' ? '✅ Vote added.' : '🗑️ Vote removed.';
  return out.next.length
    ? `${head} Your votes (${out.next.length}/${poll.maxChoices}): ${listOf(poll, out.next)}`
    : `${head} You have no votes in this poll.`;
}

/** `poll:vote:{pollId}:{optionIndex}`: toggles one option. Defers first; everything after may touch the database. */
export async function handleVoteButton(interaction, client) {
  const { params: [action, pollId, idxRaw] } = parseCustomId(interaction.customId);
  if (action !== 'vote' || !pollId || !/^\d{1,2}$/.test(idxRaw ?? '')) {
    await safeReply(interaction, { content: 'This button is no longer active.', ephemeral: true });
    return;
  }
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (content) => safeReply(interaction, { content, allowedMentions: NO_MENTIONS });
  try {
    const idx = Number(idxRaw);
    const prep = await prepareVote(interaction, client, pollId, { idx });
    if (prep.fail) {
      await reply(prep.fail);
      return;
    }
    const { poll } = prep;
    const out = await applyVote({ pollId, userId: interaction.user.id, maxChoices: poll.maxChoices, idx });
    if (out.result === 'gone') await reply('This poll no longer exists.');
    else if (out.result === 'ended') await reply(EXPIRED_TEXT);
    else {
      await reply(buttonReply(poll, idx, out));
      if (out.changed) requestRender(client, pollId);
    }
  } catch (err) {
    logger.error('poll vote failed', err);
    await reply(VOTE_FAILED);
  }
}

/**
 * `poll-select:{pollId}`: the submitted selection replaces the member's votes. Reached from the shared poll message or from the
 * personal copy of the menu in the member's ephemeral confirmation (edited in place).
 */
export async function handleVoteSelect(interaction, client) {
  const { params: [pollId] } = parseCustomId(interaction.customId);
  const fromEphemeral = interaction.message?.flags?.has?.(MessageFlags.Ephemeral) === true;
  try {
    if (fromEphemeral) await interaction.deferUpdate();
    else if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  } catch (err) {
    if (isInteractionGone(err)) logger.debug(`poll select: interaction gone (${err?.code})`);
    else logger.warn('poll select: could not acknowledge', err);
    return;
  }
  // deferUpdate leaves safeReply with followUp, which would post a new message: the ballot is edited in place.
  const edit = async (payload) => {
    try {
      return await interaction.editReply({ allowedMentions: NO_MENTIONS, ...payload });
    } catch (err) {
      if (isInteractionGone(err)) logger.debug(`poll select: interaction gone (${err?.code})`);
      else logger.warn('poll select: reply failed', err);
      return null;
    }
  };
  try {
    if (!pollId) {
      await edit({ content: 'This menu is no longer active.', components: [] });
      return;
    }
    const prep = await prepareVote(interaction, client, pollId, { heal: !fromEphemeral });
    if (prep.fail) {
      await edit(prep.terminal ? { content: prep.fail, components: [] } : { content: prep.fail });
      return;
    }
    const { poll } = prep;
    const set = normalizeSelection(interaction.values, poll.options.length, poll.maxChoices);
    if (!set) {
      await edit({ content: 'That selection is not valid for this poll.' });
      return;
    }
    const out = await applyVote({ pollId, userId: interaction.user.id, maxChoices: poll.maxChoices, set });
    if (out.result === 'gone') await edit({ content: 'This poll no longer exists.', components: [] });
    else if (out.result === 'ended') await edit({ content: EXPIRED_TEXT, components: [] });
    else {
      await edit({
        content: set.length
          ? `✅ Your votes (${set.length}/${poll.maxChoices}): ${listOf(poll, set)}`
          : '🗑️ Your votes were cleared.',
        components: buildPollComponents(poll, { selected: set }),
      });
      if (out.changed) requestRender(client, pollId);
    }
  } catch (err) {
    logger.error('poll vote failed', err);
    await edit({ content: VOTE_FAILED });
  }
}

// ── Live results (render throttle) ────────────────────────────────────────────

const renderMutex = createKeyedMutex(); // shared by live renders and endPoll's final edit
/** @type {Map<string, { lastAt: number, timer: any }>} */
const renderState = new Map();
const warnedAt = new Map();
let renderImpl = renderPollMessage;

/** Test hook: replaces the Discord render (no effect at import). Pass nothing to restore. */
export function __setRenderImpl(fn) {
  renderImpl = typeof fn === 'function' ? fn : renderPollMessage;
}

function warnThrottled(key, message, err) {
  const now = Date.now();
  if (now - (warnedAt.get(key) ?? 0) < WARN_EVERY_MS) return;
  if (warnedAt.size > 200) for (const [k, t] of warnedAt) if (now - t >= WARN_EVERY_MS) warnedAt.delete(k);
  warnedAt.set(key, now);
  logger.warn(message, err);
}

/**
 * Asks for the poll message to be re-rendered: at most one edit per poll per 5 s. The first request after a quiet period
 * renders at once; requests inside the window collapse into one trailing render that reads the database when it runs, so the
 * last vote is always shown. `timer` is cleared before the render starts, so a vote that commits while a render is in flight
 * queues another trailing render instead of being dropped.
 */
export function requestRender(client, pollId) {
  const s = renderState.get(pollId) ?? { lastAt: 0, timer: null };
  renderState.set(pollId, s);
  if (s.timer) return; // a trailing render is already queued
  if (renderState.size > RENDER_SWEEP_AT) {
    const now = Date.now();
    for (const [id, st] of renderState) if (!st.timer && id !== pollId && now - st.lastAt > RENDER_IDLE_MS) renderState.delete(id);
  }
  const wait = Math.max(0, s.lastAt + POLL.RENDER_INTERVAL_MS - Date.now());
  s.timer = setTimeout(async () => {
    s.lastAt = Date.now();
    s.timer = null;
    try {
      const outcome = await renderMutex.run(pollId, () => renderImpl(client, pollId));
      if (outcome === 'gone') await endPoll(client, pollId, { reason: 'deleted' }); // outside the mutex
    } catch (err) {
      logger.error(`poll render failed (${pollId})`, err);
    }
  }, wait);
  s.timer.unref?.();
}

/** Drops a queued render (the poll ended). */
export function cancelRender(pollId) {
  const s = renderState.get(pollId);
  if (s?.timer) clearTimeout(s.timer);
  renderState.delete(pollId);
}

const guildFor = (client, guildId) => {
  const guild = client.guilds?.cache?.get(guildId);
  return guild && guild.available !== false ? guild : null;
};

/**
 * Finds a poll's channel. `gone` is reported only for a REST 10003 (rule 20: a cache miss is not a deletion).
 * @returns {Promise<{ channel: object } | { gone: true } | { noAccess: true }>}
 */
async function resolvePollChannel(client, poll) {
  const cached = client.channels?.cache?.get(poll.channelId);
  if (cached) return { channel: cached };
  try {
    const channel = await client.channels.fetch(poll.channelId);
    return channel ? { channel } : { noAccess: true };
  } catch (err) {
    if (err?.code === UNKNOWN_CHANNEL) return { gone: true };
    if (NO_ACCESS_CODES.has(err?.code)) return { noAccess: true };
    throw err;
  }
}

/**
 * Edits the poll message with fresh counts.
 * @returns {Promise<'ok'|'skip'|'gone'>} 'gone' = the message or channel was deleted (the poll should end)
 */
async function renderPollMessage(client, pollId) {
  const poll = await prisma.poll.findUnique({ where: { id: pollId } });
  // The final edit belongs to endPoll; both run under renderMutex, so a slow live render can never overwrite it.
  if (!poll || !poll.messageId || poll.status !== 'ACTIVE') return 'skip';
  if (!guildFor(client, poll.guildId)) return 'skip';
  try {
    const found = await resolvePollChannel(client, poll);
    if (found.gone) return 'gone';
    if (found.noAccess) {
      warnThrottled(`render:${pollId}`, `poll ${pollId}: cannot reach its channel to update the results`);
      return 'skip';
    }
    const tally = await getTally(poll);
    const settings = await getSettingsSafe(poll.guildId);
    await found.channel.messages.edit(poll.messageId, buildPollPayload(poll, tally, { settings }));
    return 'ok';
  } catch (err) {
    if (err?.code === UNKNOWN_MESSAGE || err?.code === UNKNOWN_CHANNEL) return 'gone';
    if (NO_ACCESS_CODES.has(err?.code)) {
      warnThrottled(`render:${pollId}`, `poll ${pollId}: cannot edit its message (Discord ${err.code})`);
      return 'skip';
    }
    throw err;
  }
}

// ── Ending ────────────────────────────────────────────────────────────────────

/**
 * Ends a poll: the conditional update picks exactly one winner among the end job, /poll end, the delete hooks and the vote
 * self-heal. Every Discord step after the status change is caught here, so a retry never repeats the edit or the post.
 * @param {import('discord.js').Client} client
 * @param {string} pollId
 * @param {{ reason: 'manual'|'timer'|'expired'|'deleted', guildId?: string }} opts
 * @returns {Promise<{ ok: true, poll: object|null, tally: object|null, posted: boolean, messageGone: boolean }
 *   | { ok: false, code: 'NOT_FOUND'|'ALREADY_ENDED' }>} Throws only when the status UPDATE fails (DB error).
 */
export async function endPoll(client, pollId, { reason, guildId = undefined } = {}) {
  const now = new Date();
  const { count } = await prisma.poll.updateMany({
    where: { id: pollId, status: 'ACTIVE', ...(guildId && { guildId }) },
    data: { status: 'ENDED', endsAt: now },
  });
  if (count === 0) {
    const row = await prisma.poll.findFirst({ where: { id: pollId, ...(guildId && { guildId }) }, select: { id: true } });
    return { ok: false, code: row ? 'ALREADY_ENDED' : 'NOT_FOUND' };
  }

  let poll = null;
  let tally = null;
  let posted = false;
  let messageGone = false;
  try {
    cancelRender(pollId);
    if (reason !== 'timer') await cancelJobs({ dedupeKey: endJobKey(pollId) });
    poll = await prisma.poll.findUnique({ where: { id: pollId } });
    if (!poll) return { ok: true, poll, tally, posted, messageGone };
    tally = await getTally(poll); // read after the UPDATE: every committed vote is in, no later vote can be
    const settings = await getSettingsSafe(poll.guildId);
    if (reason === 'deleted') return { ok: true, poll, tally, posted, messageGone: true };

    const guild = guildFor(client, poll.guildId);
    if (!guild) {
      // Rule 20: an unavailable guild is never judged. The message keeps live buttons until the first click heals it.
      logger.warn(`poll ${pollId} ended, but its guild is unavailable: the message was not updated`);
      return { ok: true, poll, tally, posted, messageGone };
    }

    // Final edit.
    let channel = null;
    let channelGone = false;
    try {
      const found = await resolvePollChannel(client, poll);
      if (found.gone) channelGone = true;
      else if (found.channel) channel = found.channel;
    } catch (err) {
      logger.warn(`poll ${pollId}: could not look up its channel`, err);
    }
    if (channelGone) messageGone = true;
    else if (channel && poll.messageId) {
      try {
        await renderMutex.run(pollId, () => channel.messages.edit(poll.messageId, buildPollPayload(poll, tally, { settings, ended: true })));
      } catch (err) {
        if (err?.code === UNKNOWN_MESSAGE) messageGone = true;
        else if (err?.code === UNKNOWN_CHANNEL) { channelGone = true; messageGone = true; channel = null; }
        else logger.warn(`poll ${pollId}: could not edit the final results onto its message`, err);
      }
    } else if (channel) {
      messageGone = true;
    }

    // Results post: not while the module is off (settings === null counts as on: posting results is harmless).
    if (channel && !channelGone && (isModuleEnabled(settings, 'polls') || settings === null)) {
      try {
        if (checkChannelUsable(channel).ok) {
          const canRef = !messageGone && poll.messageId
            && channel.permissionsFor?.(channel.guild.members.me)?.has(PermissionFlagsBits.ReadMessageHistory) === true;
          await channel.send({
            embeds: [buildResultsEmbed(poll, tally, { settings, live: false, messageExists: !messageGone })],
            allowedMentions: NO_MENTIONS,
            ...(canRef && { reply: { messageReference: poll.messageId, failIfNotExists: false } }),
          });
          posted = true;
        }
      } catch (err) {
        logger.warn(`poll ${pollId}: could not post the results`, err);
      }
    }
  } catch (err) {
    logger.error(`poll ${pollId}: finishing the end failed`, err);
  }
  return { ok: true, poll, tally, posted, messageGone };
}

// ── Hooks and the job ─────────────────────────────────────────────────────────

/** messageDelete: the poll message was deleted, so the poll ends without a results message. */
export async function onMessageDelete(ctx) {
  const messageId = ctx.message?.id;
  if (!messageId || !ctx.guildId) return;
  const p = await prisma.poll.findFirst({ where: { guildId: ctx.guildId, messageId, status: 'ACTIVE' }, select: { id: true } });
  if (p) await endPoll(ctx.client, p.id, { reason: 'deleted', guildId: ctx.guildId });
}

/** messageDeleteBulk (e.g. /purge). */
export async function onMessageDeleteBulk(ctx) {
  const ids = [...(ctx.messages?.keys?.() ?? [])];
  if (!ids.length || !ctx.guildId) return;
  const rows = await prisma.poll.findMany({
    where: { guildId: ctx.guildId, status: 'ACTIVE', messageId: { in: ids } }, select: { id: true },
  });
  for (const p of rows) {
    try {
      await endPoll(ctx.client, p.id, { reason: 'deleted', guildId: ctx.guildId });
    } catch (err) {
      logger.error(`poll ${p.id}: ending after a bulk delete failed`, err);
    }
  }
}

/** channelDelete: every active poll posted in the channel ends. */
export async function onChannelDelete(ctx) {
  const channelId = ctx.channel?.id;
  if (!channelId || !ctx.guildId) return;
  const rows = await prisma.poll.findMany({
    where: { guildId: ctx.guildId, channelId, status: 'ACTIVE' }, select: { id: true },
  });
  for (const p of rows) {
    try {
      await endPoll(ctx.client, p.id, { reason: 'deleted', guildId: ctx.guildId });
    } catch (err) {
      logger.error(`poll ${p.id}: ending after a channel delete failed`, err);
    }
  }
}

/** Scheduler handler for `poll.end`. ALREADY_ENDED / NOT_FOUND are no-ops; a DB failure throws so the scheduler retries. */
export async function handleEndJob(job, ctx) {
  const pollId = job.payload?.pollId;
  if (typeof pollId !== 'string' || !pollId) throw new PermanentJobError('poll.end: missing pollId');
  await endPoll(ctx.client, pollId, { reason: 'timer', guildId: job.guildId });
}

// ── Command queries (used by commands/community/poll.js) ──────────────────────────

/**
 * Autocomplete rows: `end` = active polls in `channelId`; `results` = any poll of the guild. Filtered in the database.
 * @returns {Promise<Array<{ id: string, question: string, createdAt: Date, status?: string, channelId?: string }>>}
 */
export function findPollsForAutocomplete({ guildId, channelId, sub, q }) {
  const where = {
    guildId,
    ...(sub === 'end' ? { channelId, status: 'ACTIVE' } : {}),
    ...(q ? { question: { contains: q, mode: 'insensitive' } } : {}),
  };
  return prisma.poll.findMany({
    where,
    orderBy: { createdAt: 'desc' },
    take: 25,
    select: sub === 'end'
      ? { id: true, question: true, createdAt: true }
      : { id: true, question: true, createdAt: true, status: true, channelId: true },
  });
}

/** @returns {Promise<{ polls: object[], votes: Map<string, number> }>} the active polls of a guild with their vote totals */
export async function listActivePolls(guildId) {
  const polls = await prisma.poll.findMany({
    where: { guildId, status: 'ACTIVE' }, orderBy: { createdAt: 'asc' }, take: POLL.ACTIVE_MAX,
  });
  const groups = polls.length
    ? await prisma.pollVote.groupBy({ by: ['pollId'], where: { pollId: { in: polls.map((p) => p.id) } }, _count: { _all: true } })
    : [];
  return { polls, votes: new Map(groups.map((g) => [g.pollId, g._count._all])) };
}

/** `/poll list` pages: one line per poll, split to fit an embed description. */
export function pollListLines(polls, votes, now = Date.now()) {
  return polls.map((p, i) => {
    const link = jumpUrl(p);
    const title = clampText(p.question, 80).replace(/([[\]])/g, '\\$1');
    const n = votes.get(p.id) ?? 0;
    let endText = 'no end time';
    if (p.endsAt) endText = p.endsAt.getTime() > now ? `ends ${discordTimestamp(p.endsAt, 'R')}` : 'ending…';
    return `**${i + 1}.** ${link ? `[${title}](${link})` : title} — <#${p.channelId}> · ${formatNumber(n)} ${plural(n, 'vote')} · ${endText}`;
  });
}
