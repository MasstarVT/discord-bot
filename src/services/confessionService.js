// Confessions (beyond MEE6): members post anonymously with /confession submit (or the Confess button of a panel), moderators
// can hold every confession for review, and only an admin can look an author up, which is written to the moderation log.
// This file holds the rules: the numbered row (per-guild number and cooldown under one advisory lock), the post and the review
// message, the claim-based approve / reject / delete, the author blocklist, the logged reveal and the cleanup hooks.
// src/plugins/confessions.js declares hooks and components; src/commands/community/confession.js is the command.
//
// ANONYMITY is the point of the module. The author (`authorId`) is read in exactly three places: the cooldown lookup, the
// block command and the reveal. No embed builder takes it, no message sent to a channel carries it, and the process log never
// prints it (Prisma errors echo their arguments, so database failures are logged by code only, see errTag).
import { Prisma } from '@prisma/client';
import {
  ActionRowBuilder, AttachmentBuilder, ButtonBuilder, ButtonStyle, EmbedBuilder, ModalBuilder, TextInputBuilder, TextInputStyle,
} from 'discord.js';
import prisma from '../database/client.js';
import { COLORS, LIMITS, PERMISSION_LEVELS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { MODULES } from '../config/modules.js';
import {
  ensureGuildSettings, getSettings, getSettingsSafe, isModuleEnabled, peekSettings, updateSettings, updateSettingsWith,
} from './settingsService.js';
import { ackBudgetMs, withTimeout } from '../utils/async.js';
import { markBotDeletion } from '../utils/deletionTracker.js';
import { errorEmbed, infoEmbed, neutralEmbed, successEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { readImageSize } from '../utils/imageSize.js';
import {
  checkChannelUsable, isInteractionGone, safeDefer, safeReply,
} from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { safeFetchImage } from '../utils/safeFetch.js';
import { clampText } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';

export const CONFESSION = Object.freeze({
  // Every image is re-uploaded as `<IMAGE_NAME>.<ext>`: the member's own file name can give them away.
  IMAGE_NAME: 'confession',
  IMAGE_TIMEOUT_MS: 10_000,
  // The post and the review message hold the text in an embed description.
  EMBED_TEXT_MAX: 4000,
});

const NO_MENTIONS = Object.freeze({ parse: [] });
const TX_OPTS = Object.freeze({ timeout: 10_000, maxWait: 10_000 });
const UNKNOWN_CHANNEL = 10003;
const UNKNOWN_MESSAGE = 10008;
const POST_NEEDS = Object.freeze(['ViewChannel', 'SendMessages', 'EmbedLinks']);
const EXT_BY_TYPE = Object.freeze({ png: 'png', jpeg: 'jpg', gif: 'gif', webp: 'webp' });

export const STATUS_INFO = Object.freeze({
  PENDING: Object.freeze({ label: 'Pending review', emoji: '⏳', color: COLORS.WARNING }),
  APPROVED: Object.freeze({ label: 'Approved', emoji: '✅', color: COLORS.SUCCESS }),
  REJECTED: Object.freeze({ label: 'Rejected', emoji: '❌', color: COLORS.ERROR }),
  DELETED: Object.freeze({ label: 'Deleted', emoji: '🗑️', color: COLORS.NEUTRAL }),
});

// ── Raw SQL (one entry per statement, with sample parameters) ─────────────────

const numberLockSql = (guildId) => Prisma.sql`SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext(${`confession-number:${guildId}`}))`;
const nextNumberSql = (guildId) => Prisma.sql`SELECT (COALESCE(MAX("number"), 0) + 1)::int AS next FROM "Confession" WHERE "guildId" = ${guildId}`;

export const RAW_SQL = Object.freeze([
  { name: 'confessions.numberLock', build: () => numberLockSql('0') },
  { name: 'confessions.nextNumber', build: () => nextNumberSql('0') },
]);

// ── Pure helpers ──────────────────────────────────────────────────────────────

/** Prisma echoes the arguments of a failed call (author id and text included), so errors are logged by name and code only. */
export const errTag = (err) => `${err?.name ?? 'Error'}${err?.code ? ` ${err.code}` : ''}`;

/** @returns {{ ok: true, content: string } | { ok: false, error: 'empty'|'too-long' }} */
export function parseContent(raw) {
  const content = String(raw ?? '').trim();
  if (!content) return { ok: false, error: 'empty' };
  if (content.length > LIMITS.CONFESSION_LENGTH) return { ok: false, error: 'too-long' };
  return { ok: true, content };
}

// Zero-width and soft-hyphen characters hide a link from a plain match.
const INVISIBLE = /[­​-‏⁠-⁤﻿]/g;
// A scheme, www., a Discord invite host, or a domain with a path (bit.ly/x). Static pattern, linear in the text length.
const LINK = /(?:[a-z][a-z0-9+.-]*:\/\/|\bwww\.|\bdiscord(?:app)?\.com\/invite\b|\bdiscord\.gg\b|\bdsc\.gg\b|\binvite\.gg\b|\b[a-z0-9-]+(?:\.[a-z0-9-]+)*\.[a-z]{2,24}\/\S)/i;

/** Whether the text has a URL or a Discord invite. */
export function containsLink(text) {
  return LINK.test(String(text ?? '').normalize('NFKC').replace(INVISIBLE, ''));
}

/** Milliseconds until the member may confess again (0 = now). */
export function cooldownRemainingMs(lastAt, cooldownSeconds, now = Date.now()) {
  const secs = Number(cooldownSeconds);
  if (!lastAt || !Number.isFinite(secs) || secs <= 0) return 0;
  return Math.max(0, new Date(lastAt).getTime() + secs * 1000 - now);
}

/** Link to the post, or null when the message is gone or not posted yet. */
export function jumpUrl(c) {
  return c?.messageId && c.channelId ? `https://discord.com/channels/${c.guildId}/${c.channelId}/${c.messageId}` : null;
}

/** The router's wording for a member command used while the module is off. */
export function moduleOffEmbed() {
  const m = MODULES.confessions;
  return warnEmbed('Module Disabled', `The **${m.label}** module is turned off on this server. An admin can enable it with \`/module enable confessions\`.`);
}

const noChannelEmbed = () => errorEmbed('No Confession Channel', 'Confessions are not set up yet. An admin can pick a channel with `/confession setup`.');
// Neutral on purpose: it must not say why (a block is never explained).
const blockedEmbed = () => errorEmbed("Can't Confess", "You can't post confessions on this server.");
const settingsUnavailableEmbed = () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.');
const dbErrorEmbed = () => errorEmbed('Could Not Save', 'Your confession could not be saved. Please try again in a moment.');
export const cooldownEmbed = (ms) => warnEmbed('Slow Down', `You can post another confession ${discordTimestamp(new Date(Date.now() + ms), 'R')}.`);

/**
 * What stops a member from confessing, from settings alone (the cooldown needs the database).
 * @returns {import('discord.js').EmbedBuilder|null} an error embed, or null
 */
export function submitProblem(settings, userId) {
  if (!isModuleEnabled(settings, 'confessions')) return moduleOffEmbed();
  if (!settings.confessionChannelId) return noChannelEmbed();
  if (userId && (settings.confessionBlockedUserIds ?? []).includes(userId)) return blockedEmbed();
  return null;
}

/** What is wrong with an attached image, from the option alone (null = fine, or nothing attached). */
export function imageProblem(settings, attachment) {
  if (!attachment) return null;
  if (!settings.confessionAllowImages) return errorEmbed('Images Not Allowed', 'This server does not allow images in confessions. Remove the `image` option.');
  const type = String(attachment.contentType ?? '').split(';')[0].trim().toLowerCase();
  if (type && (!type.startsWith('image/') || type === 'image/svg+xml')) {
    return errorEmbed('Not An Image', 'The file has to be a PNG, JPEG, GIF or WebP image.');
  }
  if (Number(attachment.size) > LIMITS.CONFESSION_IMAGE_BYTES) {
    return errorEmbed('Image Too Large', `Images can be at most **${Math.floor(LIMITS.CONFESSION_IMAGE_BYTES / 1_000_000)} MB**.`);
  }
  return null;
}

// ── Messages ──────────────────────────────────────────────────────────────────

/** The post: number, text, optional image. Nothing here can name the author (the row's authorId is never read). */
export function buildConfessionEmbed(c, { imageName = null } = {}) {
  const embed = new EmbedBuilder()
    .setColor(COLORS.PRIMARY)
    .setTitle(`🤫 Confession #${c.number}`)
    .setDescription(clampText(c.content, CONFESSION.EMBED_TEXT_MAX));
  // The time it is posted, not the time it was written: after a review the two differ, and the writing time would line the
  // post up with the author's activity.
  embed.setTimestamp(new Date());
  if (imageName) embed.setImage(`attachment://${imageName}`);
  return embed;
}

/** Replaces the "Status" field of a review embed. */
function withStatus(embed, text, color = null) {
  const fields = (embed.data.fields ?? []).filter((f) => f.name !== 'Status');
  embed.setFields([...fields, { name: 'Status', value: clampText(text, 1024) }]);
  if (color != null) embed.setColor(color);
  return embed;
}

/** The review message: the post plus a status line. It never shows the author. */
export function buildReviewEmbed(c, { imageName = null } = {}) {
  const embed = buildConfessionEmbed(c, { imageName }).setColor(STATUS_INFO.PENDING.color);
  if (c.createdAt) embed.setTimestamp(new Date(c.createdAt)); // staff see when it was submitted
  return withStatus(embed, `${STATUS_INFO.PENDING.emoji} ${STATUS_INFO.PENDING.label}`);
}

export function buildReviewComponents(id) {
  return [new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('confession', id, 'approve')).setStyle(ButtonStyle.Success).setLabel('Approve').setEmoji('✅'),
    new ButtonBuilder().setCustomId(buildCustomId('confession', id, 'reject')).setStyle(ButtonStyle.Danger).setLabel('Reject').setEmoji('❌'),
  )];
}

/** The form `/confession submit` and the panel button open. It has no image field. */
export function buildConfessionModal() {
  return new ModalBuilder()
    .setCustomId(buildCustomId('confession-modal'))
    .setTitle('Make a confession')
    .addComponents(new ActionRowBuilder().addComponents(
      new TextInputBuilder().setCustomId('confession-text').setLabel('Your confession').setStyle(TextInputStyle.Paragraph)
        .setRequired(true).setMinLength(1).setMaxLength(LIMITS.CONFESSION_LENGTH)
        .setPlaceholder('Say it here. Your name is not shown with it.'),
    ));
}

/** The panel message with the Confess button. */
export function buildPanelPayload() {
  const embed = new EmbedBuilder()
    .setColor(COLORS.PRIMARY)
    .setTitle('🤫 Confessions')
    .setDescription([
      'Got something to get off your chest? Press **Confess** and write it in the form. Your name is not shown with it.',
      'Members and moderators cannot see who wrote a confession. Server admins can look an author up when they have to, and that is always written to the moderation log.',
    ].join('\n\n'));
  const row = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('confession', 'new')).setStyle(ButtonStyle.Primary).setLabel('Confess').setEmoji('🤫'),
  );
  return { embeds: [embed], components: [row], allowedMentions: NO_MENTIONS };
}

// ── Discord helpers ───────────────────────────────────────────────────────────

/** @returns {Promise<{ channel: object } | { gone: true } | { noAccess: true }>} `gone` only for a REST 10003 */
async function resolveChannelById(client, channelId) {
  const cached = client.channels?.cache?.get(channelId);
  if (cached) return { channel: cached };
  try {
    const channel = await client.channels.fetch(channelId);
    return channel ? { channel } : { noAccess: true };
  } catch (err) {
    if (err?.code === UNKNOWN_CHANNEL) return { gone: true };
    if ([50001, 50013, 50083].includes(err?.code)) return { noAccess: true };
    throw err;
  }
}

function sendFailureReason(err) {
  const code = err?.code;
  if (code === 50013 || code === 50001) return "I'm missing permissions there.";
  if (code === UNKNOWN_CHANNEL) return 'the channel no longer exists.';
  return `Discord error ${code ?? 'unknown'}.`;
}

const deleteRowQuietly = (id) => prisma.confession.deleteMany({ where: { id } })
  .catch((err) => logger.warn(`[confessions] could not remove a reserved confession row (${errTag(err)})`));

/**
 * The channels a confession goes to: the post channel, and the review channel when one is set. Each must exist and let the bot
 * post (and attach files for an image).
 * @returns {Promise<{ ok: true, post: object, review: object|null } | { ok: false, reason: string }>}
 */
export async function resolveTargets(guild, settings, { image = false } = {}) {
  const needs = image ? [...POST_NEEDS, 'AttachFiles'] : POST_NEEDS;
  const find = async (id) => guild.channels.cache.get(id) ?? await guild.channels.fetch(id).catch(() => null);
  const post = await find(settings.confessionChannelId);
  if (!post) return { ok: false, reason: 'The confession channel no longer exists.' };
  const usable = checkChannelUsable(post, { needs });
  if (!usable.ok) return { ok: false, reason: usable.reason };
  let review = null;
  if (settings.confessionReviewChannelId) {
    review = await find(settings.confessionReviewChannelId);
    if (!review) return { ok: false, reason: 'The review channel no longer exists.' };
    const ok = checkChannelUsable(review, { needs });
    if (!ok.ok) return { ok: false, reason: ok.reason };
  }
  return { ok: true, post, review };
}

// ── Images ────────────────────────────────────────────────────────────────────

const defaultImageFetch = (url) => safeFetchImage(url, { maxBytes: LIMITS.CONFESSION_IMAGE_BYTES, timeoutMs: CONFESSION.IMAGE_TIMEOUT_MS });
let imageFetch = defaultImageFetch;

/** Test hook: replaces the download (tests are offline). Pass nothing to restore. */
export function __setImageFetch(fn) {
  imageFetch = typeof fn === 'function' ? fn : defaultImageFetch;
}

/**
 * Downloads an image through safeFetchImage (public hosts only, size cap, image content type) and names it neutrally.
 * @returns {Promise<{ ok: true, buffer: Buffer, name: string } | { ok: false, message: string }>}
 */
export async function downloadImage(url) {
  try {
    const buffer = await imageFetch(url);
    const type = readImageSize(buffer)?.type;
    if (!type) return { ok: false, message: 'That file is not a PNG, JPEG, GIF or WebP image.' };
    if (buffer.length > LIMITS.CONFESSION_IMAGE_BYTES) return { ok: false, message: 'The image is too large.' };
    return { ok: true, buffer, name: `${CONFESSION.IMAGE_NAME}.${EXT_BY_TYPE[type]}` };
  } catch (err) {
    logger.warn(`[confessions] image download failed (${err?.code ?? err?.name ?? 'error'})`);
    return { ok: false, message: 'I could not read that image. Try again, or post without it.' };
  }
}

const filesFor = (image) => (image ? [new AttachmentBuilder(image.buffer, { name: image.name })] : []);

// ── Create ────────────────────────────────────────────────────────────────────

/** Cooldown left in ms (0 = none), from the member's latest confession. `db` is prisma or a transaction client. */
async function cooldownLeft(db, guildId, userId, seconds) {
  if (!(seconds > 0)) return 0;
  const last = await db.confession.findFirst({
    where: { guildId, authorId: userId }, orderBy: { createdAt: 'desc' }, select: { createdAt: true },
  });
  return cooldownRemainingMs(last?.createdAt, seconds);
}

/** The member's cooldown left in ms. Read-only (for the pre-check before a form); the create re-checks under the lock. */
export const cooldownLeftMs = (guildId, userId, settings) => cooldownLeft(prisma, guildId, userId, settings?.confessionCooldownSeconds ?? 0);

/**
 * Creates a confession: reserves the numbered row (cooldown and number under one per-guild advisory lock, no Discord call inside
 * the transaction), then sends the review message (review channel set) or the post. The message is sent by the bot to the
 * channel, never as an interaction reply, so nothing public links it to the member. A failed send removes the row, so it never
 * costs the cooldown.
 * @returns {Promise<{ ok: true, confession: object, pending: boolean }
 *   | { ok: false, code: 'CHANNEL'|'COOLDOWN'|'SEND'|'DB_ERROR', data?: object }>}
 */
export async function createConfession({
  client, guild, user, content, image = null, settings, targets = null,
}) {
  const t = targets ?? await resolveTargets(guild, settings, { image: Boolean(image) });
  if (!t.ok) return { ok: false, code: 'CHANNEL', data: { reason: t.reason } };
  const pending = Boolean(t.review);

  let res;
  try {
    await ensureGuildSettings(guild.id);
    res = await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(numberLockSql(guild.id));
      const left = await cooldownLeft(tx, guild.id, user.id, settings.confessionCooldownSeconds ?? 0);
      if (left > 0) return { cooldown: left };
      const [{ next }] = await tx.$queryRaw(nextNumberSql(guild.id));
      const row = await tx.confession.create({
        data: {
          guildId: guild.id,
          number: next,
          authorId: user.id,
          content,
          hasImage: Boolean(image),
          status: pending ? 'PENDING' : 'APPROVED',
          channelId: t.post.id,
          createdAt: new Date(),
        },
      });
      return { row };
    }, TX_OPTS);
  } catch (err) {
    logger.error(`[confessions] ${guild.id}: reserving a confession failed (${errTag(err)})`);
    return { ok: false, code: 'DB_ERROR' };
  }
  if (res.cooldown) return { ok: false, code: 'COOLDOWN', data: { ms: res.cooldown } };
  const { row } = res;

  let message;
  try {
    message = pending
      ? await t.review.send({
        embeds: [buildReviewEmbed(row, { imageName: image?.name })],
        components: buildReviewComponents(row.id),
        files: filesFor(image),
        allowedMentions: NO_MENTIONS,
      })
      : await t.post.send({
        embeds: [buildConfessionEmbed(row, { imageName: image?.name })],
        files: filesFor(image),
        allowedMentions: NO_MENTIONS,
      });
  } catch (err) {
    await deleteRowQuietly(row.id);
    logger.warn(`[confessions] ${guild.id}: could not post confession #${row.number}: ${err?.message}`);
    return { ok: false, code: 'SEND', data: { reason: sendFailureReason(err) } };
  }

  // Guarded: a delete that raced in (status DELETED, or the row gone) must not get a message id back.
  let saved;
  try {
    saved = await prisma.confession.updateMany({
      where: { id: row.id, status: pending ? 'PENDING' : 'APPROVED' },
      data: pending ? { reviewMessageId: message.id } : { messageId: message.id },
    });
  } catch (err) {
    logger.error(`[confessions] ${guild.id}: could not save the message of confession #${row.number} (${errTag(err)})`);
    saved = null;
  }
  if (!saved || saved.count === 0) {
    markBotDeletion(message.id, 'confessions:rollback');
    await message.delete?.().catch(() => {});
    if (!saved) await deleteRowQuietly(row.id);
    return { ok: false, code: 'DB_ERROR' };
  }
  const confession = pending ? { ...row, reviewMessageId: message.id } : { ...row, messageId: message.id };
  return { ok: true, confession, pending };
}

const CREATE_FAILURES = {
  CHANNEL: (d) => errorEmbed('Cannot Post Confessions', `${d?.reason ?? 'The confession channel is unavailable.'} Ask an admin to check it with \`/confession setup\`.`),
  COOLDOWN: (d) => cooldownEmbed(d?.ms ?? 0),
  SEND: (d) => errorEmbed('Could Not Post', `I couldn't post your confession: ${d?.reason ?? 'unknown error.'}`),
  DB_ERROR: dbErrorEmbed,
};

/**
 * The whole submit flow after the interaction was deferred (ephemerally): module, content, link, image and channel checks, the
 * cooldown, the download, create, reply. Used by the slash command (text option) and the form. Never throws, and nothing in it
 * reaches the router's error log (which prints the user id).
 */
export async function submitFromInteraction(interaction, client, { content, attachment = null, settings }) {
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  try {
    if (!settings) return await reply(settingsUnavailableEmbed());
    const problem = submitProblem(settings, interaction.user.id);
    if (problem) return await reply(problem);
    const parsed = parseContent(content);
    if (!parsed.ok) {
      return await reply(parsed.error === 'empty'
        ? errorEmbed('Empty Confession', 'Write your confession first.')
        : errorEmbed('Too Long', `Confessions can be at most **${LIMITS.CONFESSION_LENGTH}** characters.`));
    }
    if (settings.confessionBlockLinks && containsLink(parsed.content)) {
      return await reply(errorEmbed('Links Not Allowed', 'Confessions on this server cannot contain links or Discord invites.'));
    }
    const imgProblem = imageProblem(settings, attachment);
    if (imgProblem) return await reply(imgProblem);

    const targets = await resolveTargets(interaction.guild, settings, { image: Boolean(attachment) });
    if (!targets.ok) return await reply(CREATE_FAILURES.CHANNEL({ reason: targets.reason }));

    const left = await cooldownLeft(prisma, interaction.guildId, interaction.user.id, settings.confessionCooldownSeconds ?? 0);
    if (left > 0) return await reply(cooldownEmbed(left));

    let image = null;
    if (attachment) {
      const dl = await downloadImage(attachment.url);
      if (!dl.ok) return await reply(errorEmbed('Image Problem', dl.message));
      image = dl;
    }

    const out = await createConfession({
      client, guild: interaction.guild, user: interaction.user, content: parsed.content, image, settings, targets,
    });
    if (!out.ok) return await reply(CREATE_FAILURES[out.code]?.(out.data) ?? dbErrorEmbed());

    const c = out.confession;
    const url = jumpUrl(c);
    const lines = out.pending
      ? [`Your confession was sent to the moderators for review as **#${c.number}**. It appears in <#${targets.post.id}> once it is approved.`]
      : [url ? `Your confession is live as [#${c.number}](${url}).` : `Your confession is live as #${c.number}.`];
    lines.push('Your name is not shown with it, and only you can see this message.');
    return await reply(successEmbed(out.pending ? 'Confession Sent For Review' : 'Confession Posted', lines.join('\n')));
  } catch (err) {
    logger.error(`[confessions] ${interaction.guildId}: submit failed (${errTag(err)})`);
    return reply(dbErrorEmbed()).catch(() => {});
  }
}

// ── Opening the form ──────────────────────────────────────────────────────────

/** Settings from the memo, or a bounded load that leaves the rest of the 3 s window. null = not in time. */
async function settingsWithinBudget(interaction) {
  const memo = peekSettings(interaction.guildId);
  if (memo) return memo;
  const budget = ackBudgetMs(interaction);
  if (budget <= 0) return null;
  return withTimeout(getSettingsSafe(interaction.guildId), budget, 'confession settings').catch(() => null);
}

/**
 * Opens the form as the FIRST response (only bounded lookups before it), or answers ephemerally with what stops the member.
 * Without settings in time the form opens anyway and the submit re-checks everything. Used by `/confession submit` without
 * text and by the panel button.
 */
export async function openConfessionForm(interaction) {
  const { guildId } = interaction;
  const settings = await settingsWithinBudget(interaction);
  let problem = settings ? submitProblem(settings, interaction.user.id) : null;
  if (!problem && settings && ackBudgetMs(interaction) > 0) {
    // The inner catch matters: when the timeout wins, withTimeout logs a later rejection of the lookup in full, and Prisma
    // errors echo their arguments (the author id). The lookup must never reject.
    const lookup = cooldownLeftMs(guildId, interaction.user.id, settings).catch((err) => {
      logger.warn(`[confessions] ${guildId}: cooldown lookup failed (${errTag(err)})`);
      return 0;
    });
    const left = await withTimeout(lookup, ackBudgetMs(interaction), 'confession cooldown').catch(() => 0);
    if (left > 0) problem = cooldownEmbed(left);
  }
  if (problem) {
    await safeReply(interaction, { embeds: [problem], ephemeral: true, allowedMentions: NO_MENTIONS });
    return;
  }
  try {
    await interaction.showModal(buildConfessionModal());
  } catch (err) {
    if (!isInteractionGone(err)) logger.error(`[confessions] ${guildId}: showModal failed (${errTag(err)})`);
  }
}

/** `confession-modal`: the form. Everything is checked again here. */
export async function handleConfessionModal(interaction, client) {
  const content = interaction.fields.getTextInputValue('confession-text');
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const settings = await getSettingsSafe(interaction.guildId);
  await submitFromInteraction(interaction, client, { content, settings });
}

// ── Review ────────────────────────────────────────────────────────────────────

/** Edits a review message: the status line, no buttons. The image stays (the embed is rebuilt from the message's own). Best effort. */
async function markReviewMessage(message, c, text, color) {
  if (!message?.edit) return false;
  try {
    const base = message.embeds?.[0] ? EmbedBuilder.from(message.embeds[0]) : buildConfessionEmbed(c);
    // A received embed names its image by CDN url (signed, it expires); the attachment itself stays on the message.
    const attached = message.attachments?.first?.();
    if (attached?.name && base.data.image) base.setImage(`attachment://${attached.name}`);
    withStatus(base, text, color);
    await message.edit({ embeds: [base], components: [], allowedMentions: NO_MENTIONS });
    return true;
  } catch (err) {
    logger.warn(`[confessions] could not update the review message of #${c.number}: ${err?.message}`);
    return false;
  }
}

const statusLine = (status, byId) => {
  const info = STATUS_INFO[status];
  return `${info.emoji} ${info.label}${byId ? ` by <@${byId}>` : ''} ${discordTimestamp(new Date(), 'R')}`;
};

/** A review message whose confession is already decided: strip the buttons and show how it ended. */
async function settleStaleMessage(message, c) {
  const live = (message?.components ?? []).some((r) => (r.components ?? []).some((b) => !b.disabled));
  if (live) await markReviewMessage(message, c, statusLine(c.status, c.status === 'DELETED' ? null : c.reviewerId), STATUS_INFO[c.status]?.color);
}

async function fetchFreshMessage(interaction) {
  try {
    const fresh = await interaction.channel?.messages?.fetch?.(interaction.message.id);
    return fresh ?? interaction.message;
  } catch {
    return interaction.message;
  }
}

/** Approve: copy the image from a fresh fetch of the review message, claim the row, post, mark. */
async function approve(interaction, client, c, settings) {
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  if (!settings?.confessionChannelId) {
    return reply(errorEmbed('No Confession Channel', 'Set one with `/confession setup` first. The confession stays pending.'));
  }
  const targets = await resolveTargets(interaction.guild, { ...settings, confessionReviewChannelId: null }, { image: c.hasImage });
  if (!targets.ok) return reply(errorEmbed('Cannot Post', `${targets.reason} The confession stays pending.`));

  let image = null;
  let warning = null;
  const message = await fetchFreshMessage(interaction);
  if (c.hasImage) {
    const attachment = message?.attachments?.first?.() ?? null;
    if (!attachment) {
      warning = 'The review message no longer has its image, so it was posted without one.';
    } else {
      const dl = await downloadImage(attachment.url);
      if (!dl.ok) return reply(errorEmbed('Image Problem', `${dl.message} The confession stays pending.`));
      image = dl;
    }
  }

  const claim = await prisma.confession.updateMany({
    where: { id: c.id, status: 'PENDING' },
    data: { status: 'APPROVED', reviewerId: interaction.user.id, reviewedAt: new Date() },
  });
  if (claim.count === 0) return reply(await alreadyDecided(interaction, c.id));

  let sent;
  try {
    sent = await targets.post.send({
      embeds: [buildConfessionEmbed(c, { imageName: image?.name })],
      files: filesFor(image),
      allowedMentions: NO_MENTIONS,
    });
  } catch (err) {
    // The post failed: hand the confession back to the queue.
    await prisma.confession.updateMany({
      where: { id: c.id, status: 'APPROVED', messageId: null },
      data: { status: 'PENDING', reviewerId: null, reviewedAt: null },
    }).catch(() => {});
    logger.warn(`[confessions] ${c.guildId}: could not post confession #${c.number}: ${err?.message}`);
    return reply(errorEmbed('Could Not Post', `I couldn't post it: ${sendFailureReason(err)} The confession stays pending.`));
  }

  const saved = await prisma.confession.updateMany({
    where: { id: c.id, status: 'APPROVED' },
    data: { messageId: sent.id, channelId: targets.post.id },
  }).catch((err) => {
    logger.error(`[confessions] ${c.guildId}: could not save the post of #${c.number} (${errTag(err)})`);
    return { count: 0 };
  });
  if (saved.count === 0) {
    // Deleted while it was being posted: take the post back.
    markBotDeletion(sent.id, 'confessions:rollback');
    await sent.delete?.().catch(() => {});
    return reply(warnEmbed('Confession Removed', `Confession #${c.number} was deleted while it was being posted, so nothing was published.`));
  }

  await markReviewMessage(message, c, statusLine('APPROVED', interaction.user.id), STATUS_INFO.APPROVED.color);
  logger.info(`[confessions] ${c.guildId}: #${c.number} approved by ${interaction.user.id}`);
  const url = jumpUrl({ ...c, messageId: sent.id, channelId: targets.post.id });
  const lines = [url ? `Confession #${c.number} is live ([jump](${url})).` : `Confession #${c.number} is live.`];
  if (warning) lines.push(`⚠️ ${warning}`);
  return reply(successEmbed('Approved', lines.join('\n')));
}

async function reject(interaction, c) {
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  const claim = await prisma.confession.updateMany({
    where: { id: c.id, status: 'PENDING' },
    data: { status: 'REJECTED', reviewerId: interaction.user.id, reviewedAt: new Date() },
  });
  if (claim.count === 0) return reply(await alreadyDecided(interaction, c.id));
  await markReviewMessage(await fetchFreshMessage(interaction), c, statusLine('REJECTED', interaction.user.id), STATUS_INFO.REJECTED.color);
  logger.info(`[confessions] ${c.guildId}: #${c.number} rejected by ${interaction.user.id}`);
  return reply(successEmbed('Rejected', `Confession #${c.number} was rejected and will not be posted.`));
}

/** Somebody else decided first: say how, and fix this message. */
async function alreadyDecided(interaction, id) {
  const now = await prisma.confession.findUnique({ where: { id } }).catch(() => null);
  if (now) await settleStaleMessage(interaction.message, now);
  const info = STATUS_INFO[now?.status];
  return infoEmbed('Already Reviewed', info ? `This confession was already ${info.label.toLowerCase()}.` : 'This confession was already reviewed.');
}

/** `confession:{id}:approve|reject`, the buttons of a review message. Moderators only, re-checked here. */
async function handleReviewButton(interaction, client, id, action) {
  const refuse = (embed) => safeReply(interaction, { embeds: [embed], ephemeral: true, allowedMentions: NO_MENTIONS });
  if (!interaction.inCachedGuild?.()) return refuse(errorEmbed('Server Only', 'This button only works in a server.'));
  const settings = await settingsWithinBudget(interaction);
  // ADMIN follows from Discord permissions and a moderator role from the settings, so missing settings never grant anything.
  if (resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.MODERATOR) {
    return refuse(errorEmbed('Insufficient Permissions', 'Only moderators can review confessions.'));
  }
  if (!(await safeDefer(interaction, { ephemeral: true }))) return undefined;
  const reply = (embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  try {
    const c = await prisma.confession.findUnique({ where: { id } });
    if (!c || c.guildId !== interaction.guildId) {
      return await reply(errorEmbed('Confession Not Found', 'This confession no longer exists.'));
    }
    if (c.status !== 'PENDING') {
      await settleStaleMessage(interaction.message, c);
      return await reply(infoEmbed('Already Reviewed', `Confession #${c.number} was already ${STATUS_INFO[c.status].label.toLowerCase()}.`));
    }
    return action === 'approve' ? await approve(interaction, client, c, settings ?? await getSettingsSafe(interaction.guildId)) : await reject(interaction, c);
  } catch (err) {
    logger.error(`[confessions] ${interaction.guildId}: review failed (${errTag(err)})`);
    return reply(errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.')).catch(() => {});
  }
}

/** All `confession` buttons: `confession:new` (the panel) and `confession:{id}:approve|reject` (a review message). */
export async function handleButton(interaction, client) {
  const { params } = parseCustomId(interaction.customId);
  if (params.length === 1 && params[0] === 'new') return openConfessionForm(interaction);
  const [id, action] = params;
  if (params.length !== 2 || !id || (action !== 'approve' && action !== 'reject')) {
    return safeReply(interaction, { content: 'This button is no longer active.', ephemeral: true });
  }
  return handleReviewButton(interaction, client, id, action);
}

// ── Staff: delete, block, reveal ──────────────────────────────────────────────

/**
 * Deletes a confession's post. The row is claimed first (status DELETED), so a racing approve cannot publish it; a post that
 * cannot be deleted gives the claim back and changes nothing. The row keeps its data.
 * @returns {Promise<{ ok: true, confession: object } | { ok: false, code: 'NOT_FOUND'|'ALREADY'|'CHANGED'|'MESSAGE', data?: object }>}
 */
export async function deleteConfession({ client, guild, number, settings = null }) {
  const c = await prisma.confession.findUnique({ where: { guildId_number: { guildId: guild.id, number } } });
  if (!c) return { ok: false, code: 'NOT_FOUND' };
  if (c.status === 'DELETED') return { ok: false, code: 'ALREADY' };
  const claim = await prisma.confession.updateMany({ where: { id: c.id, status: c.status }, data: { status: 'DELETED' } });
  if (claim.count === 0) return { ok: false, code: 'CHANGED' };

  if (c.messageId) {
    try {
      const found = await resolveChannelById(client, c.channelId);
      if (found.noAccess) throw Object.assign(new Error('no access'), { code: 50001 });
      if (found.channel) {
        markBotDeletion(c.messageId, 'confessions:delete');
        await found.channel.messages.delete(c.messageId);
      }
    } catch (err) {
      if (err?.code !== UNKNOWN_MESSAGE && err?.code !== UNKNOWN_CHANNEL) {
        await prisma.confession.updateMany({ where: { id: c.id, status: 'DELETED' }, data: { status: c.status } }).catch(() => {});
        logger.warn(`[confessions] ${guild.id}: could not delete the post of #${c.number}: ${err?.message}`);
        return { ok: false, code: 'MESSAGE', data: { reason: sendFailureReason(err) } };
      }
    }
    await prisma.confession.updateMany({ where: { id: c.id }, data: { messageId: null } });
  }

  // The review message (kept in the review channel the settings name; the row does not store it) shows the outcome and loses its buttons.
  if (c.reviewMessageId && settings?.confessionReviewChannelId) {
    try {
      const found = await resolveChannelById(client, settings.confessionReviewChannelId);
      const message = found.channel ? await found.channel.messages.fetch(c.reviewMessageId).catch(() => null) : null;
      if (message) await markReviewMessage(message, c, statusLine('DELETED', null), STATUS_INFO.DELETED.color);
    } catch {
      // best effort
    }
  }
  return { ok: true, confession: { ...c, status: 'DELETED', messageId: null } };
}

/**
 * Adds or removes the author of a confession in the blocklist, without ever showing who it is. The answer is the same whether or
 * not they were already (un)blocked, so two confessions cannot be linked to one author by asking. At the cap a block is refused
 * whoever the author is, for the same reason.
 * @returns {Promise<{ ok: true } | { ok: false, code: 'NOT_FOUND'|'FULL' }>}
 */
export async function setBlocked({ guildId, number, blocked }) {
  const c = await prisma.confession.findUnique({ where: { guildId_number: { guildId, number } }, select: { authorId: true } });
  if (!c) return { ok: false, code: 'NOT_FOUND' };
  const FULL = new Error('blocklist full');
  try {
    await updateSettingsWith(guildId, (current) => {
      const list = current.confessionBlockedUserIds ?? [];
      if (blocked) {
        if (list.length >= LIMITS.CONFESSION_BLOCKED_USERS) throw FULL;
        return list.includes(c.authorId) ? null : { confessionBlockedUserIds: [...list, c.authorId] };
      }
      return list.includes(c.authorId) ? { confessionBlockedUserIds: list.filter((id) => id !== c.authorId) } : null;
    });
  } catch (err) {
    if (err === FULL) return { ok: false, code: 'FULL' };
    throw err;
  }
  return { ok: true };
}

/**
 * Writes the reveal to the moderation log, then (and only then) returns the author. The entry names the admin and the number,
 * never the author, because every moderator can read the log. Without a usable moderation log channel nothing is revealed.
 * @returns {Promise<{ ok: true, confession: object }
 *   | { ok: false, code: 'NOT_FOUND'|'NO_LOG'|'LOG_FAILED', data?: object }>}
 */
export async function revealConfession({ guild, number, admin }) {
  const c = await prisma.confession.findUnique({ where: { guildId_number: { guildId: guild.id, number } } });
  if (!c) return { ok: false, code: 'NOT_FOUND' };
  const settings = await getSettingsSafe(guild.id);
  const logId = settings?.modLogChannelId;
  if (!logId) return { ok: false, code: 'NO_LOG', data: { reason: 'No moderation log channel is set.' } };
  const channel = guild.channels.cache.get(logId) ?? await guild.channels.fetch(logId).catch(() => null);
  const usable = checkChannelUsable(channel);
  if (!usable.ok) return { ok: false, code: 'NO_LOG', data: { reason: usable.reason ?? 'The moderation log channel is unusable.' } };
  const embed = neutralEmbed(
    '🕵️ Confession author revealed',
    `<@${admin.id}> looked up the author of **Confession #${number}**.`,
    `Admin ID: ${admin.id}`,
  ).setTimestamp(new Date());
  try {
    await channel.send({ embeds: [embed], allowedMentions: NO_MENTIONS });
  } catch (err) {
    logger.warn(`[confessions] ${guild.id}: the reveal of #${number} was not logged (${err?.code ?? err?.name}), so nothing was revealed`);
    return { ok: false, code: 'LOG_FAILED', data: { reason: sendFailureReason(err) } };
  }
  logger.info(`[confessions] ${guild.id}: author of #${number} revealed by ${admin.id}`);
  return { ok: true, confession: c };
}

/** Name for the reveal reply (the only place an author is shown). */
export async function describeAuthor(client, guild, authorId) {
  const member = guild?.members?.cache?.get?.(authorId) ?? null;
  let user = member?.user ?? client?.users?.cache?.get?.(authorId) ?? null;
  if (!user) user = await client?.users?.fetch?.(authorId).catch(() => null) ?? null;
  return {
    id: authorId,
    name: member?.displayName ?? user?.displayName ?? user?.username ?? null,
    inServer: Boolean(member),
  };
}

// ── Hooks ─────────────────────────────────────────────────────────────────────

/** messageDelete: a post or review message vanished. The row keeps its data; only the message id is cleared. */
export async function onMessageDelete(ctx) {
  const messageId = ctx.message?.id;
  const guildId = ctx.guildId;
  if (!messageId || !guildId) return;
  try {
    await prisma.confession.updateMany({ where: { guildId, messageId }, data: { messageId: null } });
    await prisma.confession.updateMany({ where: { guildId, reviewMessageId: messageId }, data: { reviewMessageId: null } });
  } catch (err) {
    logger.error(`[confessions] ${guildId}: clearing a deleted message failed (${errTag(err)})`);
  }
}

/**
 * channelDelete: the confession channel is unset (nobody can confess until an admin sets it up again). When the REVIEW channel is
 * deleted the confession channel is unset too: clearing only the review channel would make every confession public unreviewed.
 * Posts in the deleted channel lose their message id; confessions that were waiting for review lose theirs.
 */
export async function onChannelDelete(ctx) {
  const channelId = ctx.channel?.id;
  const guildId = ctx.guildId;
  if (!channelId || !guildId) return;
  try {
    const settings = await getSettings(guildId, { fresh: true });
    const update = {};
    if (settings?.confessionChannelId === channelId) update.confessionChannelId = null;
    if (settings?.confessionReviewChannelId === channelId) {
      update.confessionReviewChannelId = null;
      update.confessionChannelId = null;
      await prisma.confession.updateMany({ where: { guildId, status: 'PENDING', reviewMessageId: { not: null } }, data: { reviewMessageId: null } });
    }
    if (Object.keys(update).length) {
      await updateSettings(guildId, update);
      logger.info(`[confessions] ${guildId}: a confession channel was deleted, so ${Object.keys(update).join(' and ')} ${Object.keys(update).length > 1 ? 'are' : 'is'} unset`);
    }
    await prisma.confession.updateMany({ where: { guildId, channelId, messageId: { not: null } }, data: { messageId: null } });
  } catch (err) {
    logger.error(`[confessions] ${guildId}: channel delete cleanup failed (${errTag(err)})`);
  }
}
