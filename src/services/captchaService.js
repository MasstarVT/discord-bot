// Captcha verification (pkg-captcha). Everything lives here: pure helpers, the captcha image, the challenge
// flow, the panel, the lock-down helpers, the verify-existing batch, diagnostics and the hook, job and
// component bodies. /captcha (commands/admin/captcha.js) only builds the command and orchestrates replies.
//
// Data: GuildSettings.captcha* fields (read through settingsService only) and CaptchaVerification, a row per
// member that is "pending" (joined while the gate was on) or holds a live image challenge. This file is the
// only one that touches that table. A cache miss never deletes anything (rule 20): rows go away on verification,
// on a leave event, after a kick, or when a REST fetch says the member is unknown (10007 / 10013).
//
// The answer is never stored in clear text, never logged, and never appears in a customId, the message text,
// the embed or the attachment (static name, no description / alt text). Custom ids are all static.
import { createHash, randomInt, timingSafeEqual } from 'node:crypto';
import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createCanvas, GlobalFonts } from '@napi-rs/canvas';
import { Prisma } from '@prisma/client';
import {
  ActionRowBuilder, AttachmentBuilder, ButtonBuilder, ButtonStyle, ChannelType, MessageFlags, ModalBuilder,
  PermissionFlagsBits, PermissionsBitField, TextInputBuilder, TextInputStyle,
} from 'discord.js';
import prisma from '../database/client.js';
import {
  ensureGuildSettings, getSettings, getSettingsSafe, peekSettings, updateSettings, updateSettingsWith,
} from './settingsService.js';
import { cancelJobs, scheduleJob, PermanentJobError } from './scheduler.js';
import { cache } from './redis.js';
import { assignAutorole } from './welcomeService.js';
import { executeModAction } from './moderationService.js';
import { COLORS, PERMISSION_LEVELS } from '../config/constants.js';
import { MODULES } from '../config/modules.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { normalizeTemplate, parseTemplateModal, renderTemplate } from '../utils/customEmbed.js';
import {
  checkAssignableRole, checkChannelUsable, isInteractionGone, missingBotPermissions,
} from '../utils/interactionHelpers.js';
import { applyGuildColor, errorEmbed, infoEmbed, successEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { clampText, formatNumber, joinWithLimit } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import { formatDurationLong } from '../utils/parseDuration.js';
import { ackBudgetMs, createKeyedMutex, createThrottle, mapLimit, withTimeout } from '../utils/async.js';
import { fetchAllMembers } from '../utils/guildMembers.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const CAPTCHA_ALPHABET = 'ACEFHJKMNPRTWXY34679'; // 20 glyphs. Left out: 0 O D Q, 1 I L, 2 Z, 5 S, 8 B, G, U V, lowercase
export const CODE_LENGTH = 6;
export const CHALLENGE_TTL_MS = 10 * 60_000;
export const MAX_ATTEMPTS = 3;
export const TIMEOUT_MIN_MINUTES = 5;
export const TIMEOUT_MAX_MINUTES = 10_080; // 7 days
export const IMAGE_WIDTH = 320;
export const IMAGE_HEIGHT = 120;
export const IMAGE_FILE_NAME = 'captcha.png';
export const IMAGE_TIMEOUT_MS = 5_000;
export const USER_IMAGE_LIMIT = Object.freeze({ max: 10, windowMs: 600_000 });
export const GUILD_IMAGE_LIMIT = Object.freeze({ max: 120, windowMs: 60_000 });
export const KICK_DM_LIMIT = Object.freeze({ max: 30, windowMs: 60_000 });
export const VERIFY_EXISTING_BATCH = 2000;
export const DEFAULT_ROLE_NAME = 'verified'; // MEE6 "@verified"
export const DEFAULT_CHANNEL_NAME = 'verification'; // MEE6 "#verification"
export const BUTTON_STYLES = Object.freeze({
  SUCCESS: ButtonStyle.Success, PRIMARY: ButtonStyle.Primary, SECONDARY: ButtonStyle.Secondary, DANGER: ButtonStyle.Danger,
});
export const DEFAULT_VERIFY_BUTTON = Object.freeze({ label: 'Verify', style: 'SUCCESS' });
export const CUSTOM_IDS = Object.freeze({
  start: buildCustomId('captcha', 'start'),
  answer: buildCustomId('captcha', 'answer'),
  modalAnswer: buildCustomId('captcha-modal', 'answer'),
  modalPanel: buildCustomId('captcha-modal', 'panel'),
  answerField: buildCustomId('captcha-answer'),
});
export const PANEL_NEEDS = Object.freeze(['ViewChannel', 'SendMessages', 'EmbedLinks', 'ReadMessageHistory']);
export const STYLE_NAMES = Object.freeze({ SUCCESS: 'Green', PRIMARY: 'Blurple', SECONDARY: 'Grey', DANGER: 'Red' });
export const auditReason = (interaction, what) => clampText(`${what} by ${interaction.user.tag}`, 512);

export class CaptchaConfigError extends Error {
  constructor(message, title) {
    super(message);
    this.name = 'CaptchaConfigError';
    this.title = title;
  }
}

export const timeoutJobKey = (guildId, userId) => `captcha.timeout:${guildId}:${userId}`;

/** Our default panel (MEE6's verbatim default text is undocumented). No colour: embedColor / COLORS.INFO applies. */
export function defaultPanelTemplate() {
  return {
    content: null,
    embed: {
      title: '🛡️ Verification required',
      description: 'Welcome to **{server}**!\n\nTo make sure you are human, press **Verify** below. Once you pass, you get access to the rest of the server.',
      footer: { text: 'Only you can see your captcha.' },
    },
    buttons: [],
  };
}

// A full replacement payload: editing a challenge message with it removes the stale image and buttons.
const full = (embed) => ({ content: '', embeds: [embed], components: [], attachments: [] });

/** Messages shown to members (all ephemeral). Every key except the two strings is a factory (fresh timestamp). */
export const MSG = Object.freeze({
  MODULE_OFF: `The **${MODULES.captcha.label}** module is turned off on this server.`,
  TEMP_UNAVAILABLE: () => full(errorEmbed('Temporarily Unavailable', 'Server settings could not be loaded. Please try again in a moment.')),
  NOT_SET_UP: () => full(errorEmbed('Verification Unavailable', "Verification isn't set up correctly on this server. Please tell a server admin.")),
  ALREADY: () => full(successEmbed('Already Verified', "You're already verified.")),
  SCREENING_FIRST: () => full(warnEmbed('Rules Screening First', "Please complete this server's rules screening first (the prompt at the bottom of the channel list), then press **Verify** again.")),
  RATE_USER: () => full(warnEmbed('Slow Down', "You've asked for a lot of codes. Please try again in a few minutes.")),
  RATE_GUILD: () => full(warnEmbed('Busy', 'Lots of people are verifying right now. Please try again in a minute.')),
  IMAGE_FAILED: () => full(errorEmbed('Try Again', "I couldn't draw a captcha just now. Press **Verify** again in a moment.")),
  ROLE_FAILED: () => full(errorEmbed('Verification Failed', "I couldn't give you the verified role because I'm missing permissions. Please tell a server admin.")),
  VERIFIED: (guild) => full(successEmbed('Verified', `You now have access to **${clampText(guild.name, 100)}**. Welcome!`)),
  FONT_MISSING: 'The captcha font (assets/fonts/Nunito-Bold.ttf) is missing and no system font is available, so the one-click button is used instead.',
});

// ── In-process state (per shard, never authoritative; no timers, no I/O at import) ──────────

const userMutex = createKeyedMutex();
/** @type {Set<string>} `${guildId}:${userId}` while completeVerification runs */
const inFlight = new Set();
/** @type {Map<string, object>} one verify-existing batch per guild */
const bulkRuns = new Map();
const warnThrottle = createThrottle(10 * 60_000);
const fontState = { tried: false, family: 'sans-serif' };

function throttledWarn(key, message) {
  if (warnThrottle(key)) logger.warn(message);
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/** A random code. `rand(n)` returns an integer in [0, n); the default is crypto.randomInt. */
export function generateCode(rand = randomInt) {
  let code = '';
  for (let i = 0; i < CODE_LENGTH; i++) code += CAPTCHA_ALPHABET[rand(CAPTCHA_ALPHABET.length)];
  return code;
}

/** Upper-cases and drops spaces, dashes, underscores and dots. */
export function normalizeAnswer(input) {
  return String(input ?? '').toUpperCase().replace(/[\s\-_.]/g, '');
}

export function hashAnswer(guildId, userId, answer) {
  return createHash('sha256').update(`${guildId}:${userId}:${normalizeAnswer(answer)}`).digest('hex');
}

const HASH_RE = /^[0-9a-f]{64}$/i;

/** Constant-time comparison. Never throws; a malformed hash or a wrong-length answer is simply false. */
export function answerMatches(hash, guildId, userId, input) {
  try {
    if (typeof hash !== 'string' || !HASH_RE.test(hash)) return false;
    const n = normalizeAnswer(input);
    if (n.length !== CODE_LENGTH) return false;
    return timingSafeEqual(Buffer.from(hashAnswer(guildId, userId, n), 'hex'), Buffer.from(hash, 'hex'));
  } catch {
    return false;
  }
}

/**
 * Judges a submitted answer against the stored challenge row.
 * @returns {{ result: 'none'|'expired'|'exhausted'|'ok'|'wrong', attemptsLeft?: number }}
 */
export function evaluateAnswer({ row, guildId, userId, input, now = Date.now() }) {
  if (!row || !row.answerHash) return { result: 'none' };
  if (!row.expiresAt || new Date(row.expiresAt).getTime() <= now) return { result: 'expired' };
  if (row.attempts >= MAX_ATTEMPTS) return { result: 'exhausted', attemptsLeft: 0 };
  if (answerMatches(row.answerHash, guildId, userId, input)) return { result: 'ok' };
  const used = row.attempts + 1;
  if (used >= MAX_ATTEMPTS) return { result: 'exhausted', attemptsLeft: 0 };
  return { result: 'wrong', attemptsLeft: MAX_ATTEMPTS - used };
}

/** @returns {{ minutes: number }|{ error: string }} */
export function validateTimeoutMinutes(n) {
  if (!Number.isInteger(n)) return { error: 'Use a whole number of minutes.' };
  if (n === 0) return { minutes: 0 };
  if (n < TIMEOUT_MIN_MINUTES || n > TIMEOUT_MAX_MINUTES) {
    return { error: 'Use 0 (never) or between 5 and 10080 minutes (7 days).' };
  }
  return { minutes: n };
}

/** The deadline counts from the join but is never less than a minute away. */
export function timeoutRunAt(joinedTimestamp, minutes, now = Date.now()) {
  return new Date(Math.max(now + 60_000, (joinedTimestamp ?? now) + minutes * 60_000));
}

/** Reads the stored verify button; invalid or missing parts fall back to the defaults. Never throws. */
export function parseVerifyButton(raw) {
  const label = typeof raw?.label === 'string' && raw.label.trim() ? clampText(raw.label.trim(), 80) : DEFAULT_VERIFY_BUTTON.label;
  const style = typeof raw?.style === 'string' && Object.hasOwn(BUTTON_STYLES, raw.style) ? raw.style : DEFAULT_VERIFY_BUTTON.style;
  return { label, style };
}

/** The panel template (the stored `verifyButton` key is dropped by normalizeTemplate). Never throws. */
export function panelTemplate(settings) {
  const raw = settings?.captchaTemplate;
  if (raw && typeof raw === 'object') {
    const t = normalizeTemplate(raw);
    if (t.content || t.embed) return t;
  }
  return normalizeTemplate(defaultPanelTemplate());
}

/** The panel message: the template plus the Verify button row (always last). Never returns null. */
export function buildPanelPayload(settings, guild, channel) {
  const btn = parseVerifyButton(settings?.captchaTemplate?.verifyButton);
  const row = new ActionRowBuilder().addComponents(new ButtonBuilder()
    .setCustomId(CUSTOM_IDS.start).setLabel(btn.label).setStyle(BUTTON_STYLES[btn.style]).setEmoji('✅'));
  const opts = {
    defaultColor: Number.isInteger(settings?.embedColor) ? settings.embedColor : COLORS.INFO,
    allowedMentions: { parse: [] }, // the panel never pings
    extraComponents: [row],
  };
  const ctx = { guild, channel }; // the panel has no user: {user} tokens stay literal
  let payload = renderTemplate(panelTemplate(settings), ctx, opts);
  // A colour-only or otherwise invisible template renders to just the button: show the default text instead.
  if (!payload || (!payload.content && payload.embeds.length === 0)) {
    payload = renderTemplate(normalizeTemplate(defaultPanelTemplate()), ctx, opts);
  }
  return payload;
}

/** The ephemeral challenge message. The attachment has a static name and no description (alt text). */
export function buildChallengePayload({ settings, expiresAt, image, content = '' }) {
  const embed = applyGuildColor(infoEmbed(
    '🔐 Verification',
    `Type the **${CODE_LENGTH} characters** from the image, then press **Enter code**.\n`
    + `Capital letters and numbers only; case does not matter. You have **${MAX_ATTEMPTS} tries**. `
    + `This code expires ${discordTimestamp(expiresAt, 'R')}.`,
  ), settings).setImage(`attachment://${IMAGE_FILE_NAME}`);
  const row = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(CUSTOM_IDS.answer).setLabel('Enter code').setStyle(ButtonStyle.Primary).setEmoji('⌨️'),
    new ButtonBuilder().setCustomId(CUSTOM_IDS.start).setLabel('New image').setStyle(ButtonStyle.Secondary).setEmoji('🔄'),
  );
  return {
    content,
    embeds: [embed],
    components: [row],
    files: [new AttachmentBuilder(image, { name: IMAGE_FILE_NAME })],
    attachments: [], // removes the previous image when editing
  };
}

/** True when the verified role was just added. With an uncached old member the role being present is enough. */
export function isRoleNewlyAdded({ oldMember, newMember, oldKnown, roleId }) {
  return !!roleId && newMember.roles.cache.has(roleId) && (!oldKnown || !oldMember.roles.cache.has(roleId));
}

/** Derived from Discord: the server is locked while @everyone cannot view channels. */
export function isLocked(guild) {
  return !guild.roles.everyone.permissions.has(PermissionFlagsBits.ViewChannel);
}

export function everyoneCanView(channel) {
  return channel.permissionsFor(channel.guild.roles.everyone)?.has(PermissionFlagsBits.ViewChannel) === true;
}

/** True when the bot would still see channels without @everyone's View Channels (Administrator counts). */
export function botSeesWithoutEveryone(guild) {
  return guild.members.me.roles.cache.some((r) => r.id !== guild.id && r.permissions.has(PermissionFlagsBits.ViewChannel));
}

/** Which lock-down steps are still needed (true = needed). */
export function lockPlan(guild, role, channel) {
  return {
    channel: !(channel.permissionOverwrites.cache.get(guild.id)?.allow.has(PermissionFlagsBits.ViewChannel)),
    role: !role.permissions.has(PermissionFlagsBits.ViewChannel),
    everyone: !isLocked(guild),
  };
}

/** Members the timeout job never kicks locally: the owner, bots, moderators and above, immune roles. */
export function isKickExempt(member, settings) {
  if (member.id === member.guild.ownerId) return true;
  if (member.user?.bot) return true;
  if (resolvePermissionLevel(member, settings) >= PERMISSION_LEVELS.MODERATOR) return true;
  return (settings?.immuneRoleIds ?? []).some((id) => member.roles.cache.has(id));
}

/** Whether a role may be the verified role (everyone who passes receives it). */
export function checkVerifiedRole(guild, role, settings, { executor = null } = {}) {
  const chk = checkAssignableRole(guild, role, { executor, allowDangerous: false });
  if (!chk.ok) {
    let hint = '';
    if (/my highest role/i.test(chk.reason ?? '')) hint = ' Move my highest role above it in **Server Settings → Roles**.';
    else if (chk.dangerous?.length) hint = ' Everyone who verifies gets this role, so roles with these permissions are refused.';
    return { ok: false, reason: `${chk.reason}${hint}` };
  }
  if (settings?.modRoleIds?.includes(role.id)) {
    return { ok: false, reason: "That's a moderator role (/modconfig): everyone who verifies would become a moderator." };
  }
  if (settings?.immuneRoleIds?.includes(role.id)) {
    return { ok: false, reason: "That's an immune role (/modconfig): everyone who verifies would be immune to moderation." };
  }
  if (settings?.muteRoleId === role.id) return { ok: false, reason: "That's the mute role." };
  return { ok: true };
}

export function progressText(run) {
  return `**${formatNumber(run.done)}/${formatNumber(run.total)}** processed · ✅ ${formatNumber(run.ok)} given · `
    + `⚠️ ${formatNumber(run.failed)} failed · 🚪 ${formatNumber(run.left)} left`;
}

export function discordErrorText(err) {
  switch (err?.code) {
    case 50013: return 'I am missing a permission for that.';
    case 50001: return "I can't access that channel.";
    case 10003: return 'The channel no longer exists.';
    case 10011: return 'The role no longer exists.';
    case 10008: return 'The message no longer exists.';
    case 30013: return 'This server has reached the maximum number of channels or roles.';
    default: return clampText(err?.message ?? 'Unknown error', 200);
  }
}

// ── Captcha image ─────────────────────────────────────────────────────────────

const FONT_PATH = resolve(dirname(fileURLToPath(import.meta.url)), '../../assets/fonts/Nunito-Bold.ttf');

// Lazy, once per process. Without any font the canvas draws blank text (CLAUDE.md), so callers check
// isImageCaptchaAvailable() and fall back to the button.
function ensureFont() {
  if (fontState.tried) return fontState.family;
  fontState.tried = true;
  try {
    if (GlobalFonts.has('Nunito')) {
      fontState.family = 'Nunito'; // welcomeCard may have registered it already
    } else if (existsSync(FONT_PATH)) {
      GlobalFonts.registerFromPath(FONT_PATH, 'Nunito');
      fontState.family = GlobalFonts.has('Nunito') ? 'Nunito' : 'sans-serif';
    }
  } catch (err) {
    logger.warn(`Captcha font registration failed: ${err?.message}`);
  }
  return fontState.family;
}

export function isImageCaptchaAvailable() {
  try {
    ensureFont();
    return GlobalFonts.has('Nunito') || GlobalFonts.families.length > 0;
  } catch {
    return false;
  }
}

/**
 * Draws the code with noise and rotation and returns a PNG. Visual jitter uses Math.random and never
 * depends on the code; the code itself comes from crypto.randomInt. Throws on failure.
 * @param {string} code
 * @returns {Promise<Buffer>}
 */
export async function renderCaptchaImage(code) {
  ensureFont();
  const family = GlobalFonts.has('Nunito') ? 'Nunito' : fontState.family;
  const W = IMAGE_WIDTH;
  const H = IMAGE_HEIGHT;
  const r = (a, b) => a + Math.random() * (b - a);
  const canvas = createCanvas(W, H);
  const ctx = canvas.getContext('2d');
  ctx.fillStyle = `hsl(${randomInt(360)}, 40%, 92%)`;
  ctx.fillRect(0, 0, W, H);

  const curve = (width, stroke) => {
    ctx.lineWidth = width;
    ctx.strokeStyle = stroke;
    ctx.beginPath();
    ctx.moveTo(r(0, 20), r(0, H));
    ctx.bezierCurveTo(r(60, 140), r(0, H), r(180, 260), r(0, H), r(W - 20, W), r(0, H));
    ctx.stroke();
  };
  for (let i = 0; i < 4; i++) curve(r(1, 2), `hsla(${randomInt(360)}, 50%, 55%, 0.5)`);
  for (let i = 0; i < 150; i++) {
    ctx.fillStyle = `rgba(0,0,0,${r(0.1, 0.4)})`;
    ctx.fillRect(r(0, W), r(0, H), 2, 2);
  }

  const chars = [...String(code)];
  const slot = (W - 40) / CODE_LENGTH;
  ctx.textAlign = 'center';
  ctx.textBaseline = 'middle';
  for (let i = 0; i < chars.length; i++) {
    ctx.save();
    ctx.translate(20 + slot * (i + 0.5) + r(-5, 5), H / 2 + r(-12, 12));
    ctx.rotate(r(-0.4, 0.4));
    ctx.font = `bold ${randomInt(46, 57)}px ${family}, sans-serif`;
    ctx.fillStyle = `hsl(${randomInt(360)}, 60%, 28%)`;
    ctx.fillText(chars[i], 0, 0);
    ctx.restore();
  }

  for (let i = 0; i < 2; i++) curve(r(2, 3), `hsla(${randomInt(360)}, 50%, 30%, 0.6)`);
  return await canvas.encode('png'); // async encoder, off the main thread
}

// ── Settings for the first response ───────────────────────────────────────────

/**
 * Settings within the acknowledgement budget (rule 21): the memo first, then a bounded load.
 * Null when the budget ran out or Postgres is down. Never throws.
 */
export async function settingsForAck(interaction) {
  const hit = peekSettings(interaction.guildId);
  if (hit) return hit;
  try {
    return await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'captcha settings');
  } catch {
    return null;
  }
}

// Replies without throwing. `attachments` only exists on edits, so it is dropped here.
async function replyEphemeral(interaction, payload) {
  try {
    if (typeof payload === 'string') {
      await interaction.reply({ content: payload, ephemeral: true });
      return;
    }
    const { attachments: _drop, ...rest } = payload;
    await interaction.reply({ ...rest, ephemeral: true });
  } catch (err) {
    if (isInteractionGone(err)) logger.debug(`captcha reply: interaction gone (${err?.code})`);
    else logger.warn('captcha reply failed', err);
  }
}

const makeRespond = (interaction) => (payload) => interaction.editReply(payload).catch((err) => {
  if (isInteractionGone(err)) logger.debug(`captcha respond: interaction gone (${err?.code})`);
  else logger.warn('captcha respond failed', err);
});

// ── Rate limits ───────────────────────────────────────────────────────────────

/** Image caps. Both windows fail OPEN (null = Redis down): a cache outage must not lock members out. */
async function checkImageLimits(guildId, userId) {
  const g = await cache.slidingWindow(`captcha:img:${guildId}`, GUILD_IMAGE_LIMIT.windowMs);
  if (g !== null && g > GUILD_IMAGE_LIMIT.max) return MSG.RATE_GUILD(); // null: fail-open, issue the image
  const u = await cache.slidingWindow(`captcha:img:${guildId}:${userId}`, USER_IMAGE_LIMIT.windowMs);
  if (u !== null && u > USER_IMAGE_LIMIT.max) return MSG.RATE_USER(); // null: fail-open, issue the image
  return null;
}

// ── Challenge and verification ────────────────────────────────────────────────

/**
 * Draws a new code, stores only its hash and returns the challenge message. Renders BEFORE the write, so a
 * failed render keeps the old challenge. Throws on a render or database failure.
 */
export async function issueChallenge(member, settings, { content = '' } = {}) {
  const guildId = member.guild.id;
  const userId = member.id;
  const code = generateCode();
  const expiresAt = new Date(Date.now() + CHALLENGE_TTL_MS);
  const image = await withTimeout(renderCaptchaImage(code), IMAGE_TIMEOUT_MS, 'captcha image');
  await ensureGuildSettings(guildId);
  const answerHash = hashAnswer(guildId, userId, code);
  await prisma.captchaVerification.upsert({
    where: { guildId_userId: { guildId, userId } },
    create: { guildId, userId, answerHash, attempts: 0, expiresAt },
    update: { answerHash, attempts: 0, expiresAt },
  });
  return buildChallengePayload({ settings, expiresAt, image, content });
}

/**
 * Gives the verified role, clears the pending row and the timeout job, then the autoroles (role first, so
 * welcome's captcha gate is open). Never throws for Discord permission problems; database errors propagate.
 * @returns {Promise<{ ok: true, added: boolean, member: object } | { ok: false, payload: object }>}
 */
export async function completeVerification(member, settings, role) {
  const guild = member.guild;
  const key = `${guild.id}:${member.id}`;
  inFlight.add(key); // the guildMemberUpdate hook skips this member, so the autoroles are granted exactly once
  try {
    let m = member;
    let added = false;
    if (!m.roles.cache.has(role.id)) {
      try {
        m = await m.roles.add(role.id, 'Captcha verification passed'); // one role at a time (rule 13)
        added = true;
      } catch (err) {
        if (err?.code === 10011) return { ok: false, payload: MSG.NOT_SET_UP() };
        if (err?.code === 10007) return { ok: false, payload: MSG.NOT_SET_UP() }; // left mid-flow; nobody will see it
        if (err?.code === 50013 || err?.code === 50001) {
          throttledWarn(`${guild.id}:grant`, `Captcha: cannot give the verified role in ${guild.id}: ${discordErrorText(err)}`);
          return { ok: false, payload: MSG.ROLE_FAILED() };
        }
        throw err;
      }
    }
    const { count } = await prisma.captchaVerification.deleteMany({ where: { guildId: guild.id, userId: member.id } });
    if (count) await cancelJobs({ dedupeKey: timeoutJobKey(guild.id, member.id) }); // never throws
    if (added || count) await assignAutorole(m, { force: true }); // never throws; skips only the captcha gate
    return { ok: true, added, member: m };
  } finally {
    inFlight.delete(key);
  }
}

/** Members still waiting to verify (pending rows, including unfinished challenges). */
export async function countPending(guildId) {
  return prisma.captchaVerification.count({ where: { guildId } });
}

/** Restores the default panel text. A plain null is rejected for a Json column, so this uses DbNull. */
export async function resetPanelTemplate(guildId) {
  return updateSettings(guildId, { captchaTemplate: Prisma.DbNull });
}

// ── Panel and channel ─────────────────────────────────────────────────────────

function toConfigError(err, channel) {
  if (err?.code === 50001 || err?.code === 50013) {
    return new CaptchaConfigError(
      `I can't post in <#${channel.id}>: I need **View Channel, Send Messages, Embed Links and Read Message History** there.`,
      'Cannot Post Panel',
    );
  }
  return err;
}

/** Creates #verification: read-only for @everyone, writable for the bot. Errors propagate. */
export async function createVerificationChannel(guild, interaction) {
  const F = PermissionFlagsBits;
  return guild.channels.create({
    name: DEFAULT_CHANNEL_NAME,
    type: ChannelType.GuildText,
    position: 0,
    topic: 'Verify here to get access to the server.',
    permissionOverwrites: [
      {
        id: guild.id,
        allow: [F.ViewChannel, F.ReadMessageHistory],
        deny: [F.SendMessages, F.AddReactions, F.CreatePublicThreads, F.CreatePrivateThreads],
      },
      { id: guild.members.me.id, allow: [F.ViewChannel, F.SendMessages, F.EmbedLinks, F.ReadMessageHistory] },
    ],
    reason: auditReason(interaction, 'captcha setup'),
  });
}

/**
 * Posts the panel, or edits the previous one in place when it is in the same channel. A moved panel
 * deletes the old message best-effort. Throws CaptchaConfigError for permission problems.
 */
export async function publishPanel(guild, channel, settings, previous = {}) {
  const payload = buildPanelPayload(settings, guild, channel);
  if (previous.messageId && previous.channelId === channel.id) {
    try {
      const msg = await channel.messages.fetch(previous.messageId);
      await msg.edit({ content: '', ...payload });
      return msg;
    } catch (err) {
      if (err?.code !== 10008) throw toConfigError(err, channel);
      // The panel was deleted: fall through and post a new one.
    }
  }
  let msg;
  try {
    msg = await channel.send(payload);
  } catch (err) {
    throw toConfigError(err, channel);
  }
  if (previous.messageId && previous.channelId && previous.channelId !== channel.id) {
    guild.channels.cache.get(previous.channelId)?.messages?.delete(previous.messageId).catch(() => {});
  }
  return msg;
}

/** Re-renders the stored panel after a settings change. Never throws. */
export async function refreshPanel(guild, settings) {
  try {
    if (!settings.captchaChannelId || !settings.captchaMessageId) {
      return { ok: false, reason: 'No panel has been posted yet: run `/captcha setup`.' };
    }
    const channel = guild.channels.cache.get(settings.captchaChannelId);
    if (!channel) return { ok: false, reason: 'The verification channel was deleted: run `/captcha setup`.' };
    const msg = await channel.messages.fetch(settings.captchaMessageId);
    await msg.edit({ content: '', ...buildPanelPayload(settings, guild, channel) });
    return { ok: true };
  } catch (err) {
    if (err?.code === 10008) return { ok: false, reason: 'The panel message was deleted: run `/captcha setup` to post it again.' };
    if (err?.code === 50001 || err?.code === 50013) {
      return { ok: false, reason: `I can't edit the panel: check my permissions in <#${settings.captchaChannelId}>.` };
    }
    logger.warn(`Captcha panel refresh failed in ${guild.id}`, err);
    return { ok: false, reason: "The panel couldn't be updated right now." };
  }
}

// ── Lock-down ─────────────────────────────────────────────────────────────────

/**
 * Runs the lock-down steps in an order that never hides a channel from someone halfway, stopping at the
 * first failure. Never throws.
 * @returns {Promise<{ done: string[], error?: string }>}
 */
export async function applyLock(guild, { role, channel, plan, reason }) {
  const done = [];
  try {
    if (plan.channel) {
      await channel.permissionOverwrites.edit(guild.roles.everyone, { ViewChannel: true, ReadMessageHistory: true }, { reason });
      done.push('verification channel opened to @everyone');
    }
    if (plan.role) {
      await role.setPermissions(new PermissionsBitField(role.permissions).add(PermissionFlagsBits.ViewChannel), reason);
      done.push('View Channels given to the verified role');
    }
    if (plan.everyone) {
      await guild.roles.everyone.setPermissions(
        new PermissionsBitField(guild.roles.everyone.permissions).remove(PermissionFlagsBits.ViewChannel), reason,
      );
      done.push('View Channels removed from @everyone');
    }
    return { done };
  } catch (err) {
    const error = err?.code === 50013
      ? `Discord refused the change: I need **Manage Roles**, and my highest role must be above <@&${role.id}>.`
      : discordErrorText(err);
    return { done, error };
  }
}

/** Gives @everyone View Channels back. Never throws. */
export async function removeLock(guild, reason) {
  try {
    await guild.roles.everyone.setPermissions(
      new PermissionsBitField(guild.roles.everyone.permissions).add(PermissionFlagsBits.ViewChannel), reason,
    );
    return {};
  } catch (err) {
    return { error: err?.code === 50013 ? 'Discord refused the change: I need **Manage Roles**.' : discordErrorText(err) };
  }
}

// ── verify-existing ───────────────────────────────────────────────────────────

export const getBulkRun = (guildId) => bulkRuns.get(guildId) ?? null;
export const isBulkTarget = (guildId, userId) => bulkRuns.get(guildId)?.targets.has(userId) === true;

function etaText(run) {
  const elapsed = Date.now() - run.startedAt;
  if (run.done < 5 || elapsed <= 0 || run.done >= run.total) return '';
  const msLeft = ((run.total - run.done) / run.done) * elapsed;
  return `\n⏱️ About ${formatDurationLong(Math.max(1000, Math.ceil(msLeft / 1000) * 1000))} left.`;
}

// Progress goes to the deferred reply while its token lives (15 minutes), then to a channel message.
async function report(interaction, run, final) {
  try {
    const moreStill = run.remaining + (run.abort ? Math.max(0, run.total - run.done) : 0);
    let text = progressText(run);
    if (!final) text += etaText(run);
    if (run.error) text += `\n⛔ ${run.error}`;
    if (final) {
      if (run.abort && !run.error) text += '\nStopped.';
      text += `\nGave <@&${run.roleId}> to ${formatNumber(run.ok)} member(s).`;
      if (moreStill > 0) {
        text += ` ${formatNumber(moreStill)} more still need it: run \`/captcha verify-existing\` again to continue.`;
      }
    }
    const embed = final
      ? (run.error ? warnEmbed : successEmbed)(run.abort ? 'Batch Stopped' : 'Existing Members Verified', text)
      : infoEmbed('Verifying Existing Members', text);

    if (Date.now() - interaction.createdTimestamp < 14 * 60_000) {
      try {
        await interaction.editReply({ embeds: [embed] });
        if (final) await interaction.followUp({ embeds: [embed] }); // a new message notifies the admin
      } catch (err) {
        logger.debug(`verify-existing report: ${err?.message}`);
      }
    } else {
      const channel = interaction.channel;
      try {
        if (channel && checkChannelUsable(channel).ok) {
          if (!run.channelMessage) run.channelMessage = await channel.send({ embeds: [embed], allowedMentions: { parse: [] } });
          else await run.channelMessage.edit({ embeds: [embed] });
        }
      } catch (err) {
        logger.debug(`verify-existing channel report: ${err?.message}`);
      }
    }
    if (final) logger.info(`captcha verify-existing finished in ${interaction.guildId}: ${text.replace(/\n/g, ' ')}`);
  } catch (err) {
    logger.debug(`verify-existing report failed: ${err?.message}`);
  }
}

async function runBatch(interaction, run, role, pending, targets) {
  const guildId = interaction.guildId;
  let consecutiveForbidden = 0;
  let lastReport = Date.now();
  for (const m of targets) {
    if (run.abort) break;
    try {
      const updated = await m.roles.add(role.id, 'Captcha: verify existing members');
      run.ok++;
      consecutiveForbidden = 0;
      if (pending.has(m.id)) {
        // include_pending: finish their verification too
        await prisma.captchaVerification.deleteMany({ where: { guildId, userId: m.id } });
        await cancelJobs({ dedupeKey: timeoutJobKey(guildId, m.id) });
        await assignAutorole(updated, { force: updated.pending !== true });
      }
    } catch (err) {
      if (err?.code === 10007) {
        run.left++;
      } else if (err?.code === 10011) {
        run.abort = true;
        run.error = 'The verified role was deleted.';
      } else if (err?.code === 50013) {
        run.failed++;
        if (++consecutiveForbidden >= 5) {
          run.abort = true;
          run.error = 'I lost the Manage Roles permission, or the role is now above mine.';
        }
      } else {
        run.failed++;
        throttledWarn(`${guildId}:bulk`, `verify-existing: ${err?.message}`);
      }
    }
    run.done++;
    if (Date.now() - lastReport >= 15_000) {
      lastReport = Date.now();
      await report(interaction, run, false);
    }
  }
  await report(interaction, run, true);
}

/**
 * Starts the verify-existing batch for an already-deferred interaction and returns once it is running (the
 * batch is not awaited). At most one run per guild and VERIFY_EXISTING_BATCH members per run.
 */
export async function startVerifyExisting(interaction, settings, { includePending = false } = {}) {
  const guild = interaction.guild;
  const guildId = guild.id;
  const existing = bulkRuns.get(guildId);
  if (existing) return interaction.editReply({ embeds: [warnEmbed('Already Running', progressText(existing))] });

  const role = guild.roles.cache.get(settings.captchaRoleId ?? '');
  if (!role) {
    return interaction.editReply({ embeds: [errorEmbed('Set Up First', 'Set the verified role with `/captcha setup` or `/captcha role` first.')] });
  }
  const chk = checkVerifiedRole(guild, role, settings, { executor: interaction.member });
  if (!chk.ok) return interaction.editReply({ embeds: [errorEmbed('Cannot Use This Role', chk.reason)] });

  // Claim the guild's slot before the first await below, so two admins cannot start two runs.
  const run = {
    total: 0, done: 0, ok: 0, failed: 0, left: 0, abort: false, error: null,
    remaining: 0, roleId: role.id, targets: new Set(), startedAt: Date.now(), channelMessage: null,
  };
  bulkRuns.set(guildId, run);
  let started = false;
  try {
    await interaction.editReply({ embeds: [infoEmbed('Loading Members', 'Fetching the member list…')] });
    const fetched = await fetchAllMembers(guild, { timeoutMs: 120_000, maxMembers: 100_000 }); // never throws
    if (!fetched.ok) {
      const embed = fetched.reason === 'too-large'
        ? errorEmbed('Server Too Large', 'verify-existing works for servers with up to 100,000 members.')
        : errorEmbed('Could Not Load Members', 'Discord did not send the member list in time. Try again in a minute.');
      return await interaction.editReply({ embeds: [embed] });
    }
    const members = fetched.members;

    const rows = await prisma.captchaVerification.findMany({ where: { guildId }, select: { userId: true } });
    const pending = new Set(rows.map((r) => r.userId));

    // Stale rows (members who left while the bot was away). Rule 20: only after a complete, successful
    // fetch of an available guild.
    if (guild.available) {
      const stale = [...pending].filter((id) => !members.has(id));
      if (stale.length) {
        await prisma.captchaVerification.deleteMany({ where: { guildId, userId: { in: stale } } });
        await mapLimit(stale, 4, (id) => cancelJobs({ dedupeKey: timeoutJobKey(guildId, id) }));
        for (const id of stale) pending.delete(id);
      }
    }

    const needing = [...members.values()].filter((m) => !m.roles.cache.has(role.id) && (includePending || !pending.has(m.id)));
    const skippedPending = includePending ? 0 : [...members.values()].filter((m) => pending.has(m.id) && !m.roles.cache.has(role.id)).length;
    if (run.abort) {
      return await interaction.editReply({ embeds: [infoEmbed('Stopped', 'The batch was stopped before it started.')] });
    }
    if (needing.length === 0) {
      return await interaction.editReply({
        embeds: [successEmbed('Nothing To Do', `Everyone here already has <@&${role.id}>.`
          + (skippedPending ? ` ${skippedPending} member(s) still waiting to verify were skipped (use include_pending:true).` : ''))],
      });
    }

    // Bots are included, so a locked server keeps working for them.
    const targets = needing.slice(0, VERIFY_EXISTING_BATCH);
    run.total = targets.length;
    run.remaining = needing.length - targets.length;
    run.targets = new Set(targets.map((m) => m.id)); // only this run's members: the update hook still handles the rest
    run.startedAt = Date.now();
    await interaction.editReply({
      embeds: [infoEmbed('Verifying Existing Members',
        `Giving <@&${role.id}> to **${formatNumber(run.total)}** member(s) in the background. Progress shows here. Stop with \`/captcha verify-existing stop:true\`.`
        + (run.remaining ? `\n${formatNumber(run.remaining)} more will need another run.` : '')
        + (skippedPending ? `\nSkipped ${skippedPending} member(s) still waiting to verify.` : ''))],
    }).catch((err) => logger.debug(`verify-existing start reply: ${err?.message}`));

    started = true;
    void runBatch(interaction, run, role, pending, targets)
      .catch((err) => logger.error('captcha verify-existing failed', err))
      .finally(() => { if (bulkRuns.get(guildId) === run) bulkRuns.delete(guildId); });
    return null;
  } finally {
    if (!started && bulkRuns.get(guildId) === run) bulkRuns.delete(guildId);
  }
}

// ── Diagnostics and status ────────────────────────────────────────────────────

/** Human-readable problems with the current setup. Never throws. */
export async function diagnoseCaptcha(guild, settings, { checkPanel = true } = {}) {
  const problems = [];
  try {
    const locked = isLocked(guild);
    const role = settings.captchaRoleId ? guild.roles.cache.get(settings.captchaRoleId) : null;
    if (!settings.captchaRoleId) {
      problems.push('No verified role: run `/captcha setup`.');
    } else if (!role) {
      problems.push(`${locked ? '⛔ ' : ''}The verified role was deleted: run \`/captcha setup\` or \`/captcha role\`.`
        + `${locked ? ' New members cannot see any channel until this is fixed.' : ''}`);
    } else {
      const chk = checkVerifiedRole(guild, role, settings);
      if (!chk.ok) problems.push(`The verified role can't be given: ${chk.reason}`);
    }

    const channel = settings.captchaChannelId ? guild.channels.cache.get(settings.captchaChannelId) : null;
    let channelUsable = false;
    if (!settings.captchaChannelId) {
      problems.push('No verification channel: run `/captcha setup`.');
    } else if (!channel) {
      problems.push('The verification channel was deleted: run `/captcha setup`.');
    } else {
      const cu = checkChannelUsable(channel, { needs: PANEL_NEEDS });
      if (!cu.ok) problems.push(cu.reason);
      else channelUsable = true;
    }

    if (checkPanel && channelUsable) {
      if (!settings.captchaMessageId) {
        problems.push('The panel message is missing: run `/captcha setup` to post it again.');
      } else {
        try {
          await withTimeout(channel.messages.fetch(settings.captchaMessageId), 3000, 'captcha panel check');
        } catch (err) {
          if (err?.name === 'TimeoutError') problems.push('The panel message could not be checked right now.');
          else if (err?.code === 10008) problems.push('The panel message is missing: run `/captcha setup` to post it again.');
          else problems.push('The panel message could not be checked right now.');
        }
      }
    }

    if (locked && channel && !everyoneCanView(channel)) {
      problems.push(`⛔ @everyone can't see <#${channel.id}>, so new members can't verify. Run \`/captcha lock state:on\` to fix it.`);
    }
    if (locked && role && !role.permissions.has(PermissionFlagsBits.ViewChannel)) {
      problems.push(`Verified members can't see channels: run \`/captcha lock state:on\` to give <@&${role.id}> View Channels.`);
    }
    if (settings.captchaMode === 'IMAGE' && !isImageCaptchaAvailable()) problems.push(MSG.FONT_MISSING);
    if (settings.captchaTimeoutMinutes > 0 && missingBotPermissions(guild, ['KickMembers']).length) {
      problems.push("Unverified members can't be kicked: I need **Kick Members**.");
    }
  } catch (err) {
    logger.warn(`Captcha diagnostics failed in ${guild?.id}`, err);
  }
  return problems;
}

export async function buildStatusEmbed(guild, settings) {
  const embed = applyGuildColor(infoEmbed('🛡️ Verification'), settings);
  const problems = await diagnoseCaptcha(guild, settings);
  const role = settings.captchaRoleId ? guild.roles.cache.get(settings.captchaRoleId) : null;
  const channel = settings.captchaChannelId ? guild.channels.cache.get(settings.captchaChannelId) : null;

  let channelText = '⚠️ Not set';
  if (settings.captchaChannelId) {
    if (!channel) channelText = '⚠️ Deleted';
    else if (settings.captchaMessageId) channelText = `<#${channel.id}> · [panel](https://discord.com/channels/${guild.id}/${channel.id}/${settings.captchaMessageId})`;
    else channelText = `<#${channel.id}> · ⚠️ panel missing`;
  }
  const fontFallback = settings.captchaMode === 'IMAGE' && !isImageCaptchaAvailable();
  let pending = '—';
  try {
    pending = formatNumber(await countPending(guild.id));
  } catch (err) {
    logger.debug(`captcha status: pending count failed: ${err?.message}`);
  }

  embed.addFields(
    { name: 'Status', value: settings.captchaEnabled ? '✅ On' : '❌ Off (`/captcha enable`)', inline: true },
    {
      name: 'Mode',
      value: `${settings.captchaMode === 'BUTTON' ? '🔘 Button click' : '🖼️ Image code'}${fontFallback ? ' (button fallback: font missing)' : ''}`,
      inline: true,
    },
    {
      name: 'Verified role',
      value: !settings.captchaRoleId ? '⚠️ Not set' : (role ? `<@&${role.id}>` : `⚠️ Deleted (\`${settings.captchaRoleId}\`)`),
      inline: true,
    },
    { name: 'Channel', value: channelText, inline: true },
    {
      name: 'Kick unverified',
      value: settings.captchaTimeoutMinutes > 0 ? `after ${formatDurationLong(settings.captchaTimeoutMinutes * 60_000)}` : 'never',
      inline: true,
    },
    { name: 'Lock-down', value: isLocked(guild) ? '🔒 On' : '🔓 Off', inline: true },
    { name: 'Waiting to verify', value: pending, inline: true },
  );
  const run = getBulkRun(guild.id);
  if (run) embed.addFields({ name: 'Batch', value: progressText(run), inline: true });
  embed.addFields({
    name: 'Problems',
    value: problems.length ? joinWithLimit(problems.map((p) => `• ${p}`), { sep: '\n', max: 1024 }) : 'None',
  });
  return embed;
}

// ── Hook bodies ───────────────────────────────────────────────────────────────

async function autoVerifyBot(member, role, settings) {
  let m = member;
  if (checkVerifiedRole(member.guild, role, settings).ok) {
    try {
      m = await member.roles.add(role.id, 'Captcha: bots skip verification');
    } catch (err) {
      throttledWarn(`${member.guild.id}:botgrant`, `Captcha: cannot give the verified role to bot ${member.id} in ${member.guild.id}: ${discordErrorText(err)}`);
    }
  }
  await assignAutorole(m, { force: true });
}

/** guildMemberAdd (module-gated; bots arrive here too). */
export async function onMemberJoin(ctx) {
  const { member, guild } = ctx;
  const settings = await ctx.getSettings(); // memo hit: the runner loaded it
  if (!settings?.captchaEnabled) return;
  const role = settings.captchaRoleId ? guild.roles.cache.get(settings.captchaRoleId) : null;
  if (!role || member.roles.cache.has(role.id)) return; // a deleted role never gates (matches welcome's gate)
  if (member.user.bot) return autoVerifyBot(member, role, settings);
  ctx.state.captchaPending = true; // before any I/O: welcome must see it even if this hook is cut short
  try {
    await ensureGuildSettings(guild.id);
    await prisma.captchaVerification.upsert({
      where: { guildId_userId: { guildId: guild.id, userId: member.id } },
      create: { guildId: guild.id, userId: member.id },
      update: { answerHash: null, attempts: 0, expiresAt: null },
    });
  } catch (err) {
    logger.error(`Captcha: could not record ${member.id} in ${guild.id}`, err);
    return; // still gated; pressing Verify upserts the row later
  }
  if (settings.captchaTimeoutMinutes > 0) {
    try {
      await scheduleJob({
        guildId: guild.id,
        type: 'captcha.timeout',
        runAt: timeoutRunAt(member.joinedTimestamp, settings.captchaTimeoutMinutes),
        payload: { userId: member.id },
        dedupeKey: timeoutJobKey(guild.id, member.id),
        onConflict: 'replace', // a rejoin resets the deadline
      });
    } catch (err) {
      logger.error(`Captcha: could not schedule timeout for ${member.id} in ${guild.id}`, err);
    }
  }
}

/** guildMemberRemove (runs even while the module is off). `ctx.member` may be partial. */
export async function onMemberLeave(ctx) {
  const userId = ctx.user?.id ?? ctx.member?.id;
  if (!userId || !ctx.guildId) return;
  const { count } = await prisma.captchaVerification.deleteMany({ where: { guildId: ctx.guildId, userId } });
  if (count) await cancelJobs({ dedupeKey: timeoutJobKey(ctx.guildId, userId) });
}

/** guildMemberUpdate: the verified role given by someone else verifies a pending member. */
export async function onMemberUpdate(ctx) {
  const settings = await ctx.getSettings();
  const rid = settings?.captchaRoleId;
  if (!rid) return;
  const nm = ctx.newMember;
  if (!isRoleNewlyAdded({ oldMember: ctx.oldMember, newMember: nm, oldKnown: ctx.oldKnown, roleId: rid })) return;
  const key = `${ctx.guildId}:${nm.id}`;
  if (inFlight.has(key) || isBulkTarget(ctx.guildId, nm.id)) return;
  const { count } = await prisma.captchaVerification.deleteMany({ where: { guildId: ctx.guildId, userId: nm.id } });
  if (!count) return; // not pending: a role shuffle, verify-existing, or our own grant
  await cancelJobs({ dedupeKey: timeoutJobKey(ctx.guildId, nm.id) });
  // Still in Rules Screening: let welcome schedule its screening watch instead of forcing roles now.
  await assignAutorole(nm, { force: nm.pending !== true });
}

// ── Timeout job ───────────────────────────────────────────────────────────────

// A friendly DM before the kick. Best-effort: closed DMs (50007) are normal and never reported.
async function dmBeforeKick(member, span) {
  try {
    const n = await cache.slidingWindow(`captcha:dm:${member.guild.id}`, KICK_DM_LIMIT.windowMs);
    if (n !== null && n > KICK_DM_LIMIT.max) return; // over the cap: skip the DM. null (Redis down): fail-open, send it
    await withTimeout(member.send({
      content: `You were removed from **${clampText(member.guild.name, 100)}** because you didn't complete verification within ${span}. You're welcome to join again and verify.`,
      allowedMentions: { parse: [] },
    }), 3000, 'captcha kick dm');
  } catch (err) {
    logger.debug(`captcha kick DM not delivered: ${err?.message}`);
  }
}

/**
 * Scheduler handler for `captcha.timeout`. Idempotent: a second run finds no row or an unknown member.
 * The kick goes through moderation (an infraction and a mod-log case). `deps.executeModAction` is the test seam.
 * @param {{ payload?: { userId?: string } }} job
 * @param {{ guild: object, attempt?: number }} ctx
 * @param {{ executeModAction?: Function }} [deps]
 */
export async function runTimeoutJob(job, ctx, deps = {}) {
  const guild = ctx?.guild;
  const { userId } = job?.payload ?? {};
  if (typeof userId !== 'string' || !/^\d{17,20}$/.test(userId)) throw new PermanentJobError('captcha.timeout: invalid payload');
  const settings = await getSettings(guild.id); // a database failure throws: the scheduler retries
  if (!settings.captchaEnabled || !(settings.captchaTimeoutMinutes > 0)) return; // module off or "never": no kick, row kept
  const role = settings.captchaRoleId ? guild.roles.cache.get(settings.captchaRoleId) : null;
  if (!role) return; // misconfigured: never kick
  const where = { guildId: guild.id, userId };
  const row = await prisma.captchaVerification.findUnique({ where: { guildId_userId: where } });
  if (!row) return; // verified or left meanwhile

  let member;
  try {
    member = await guild.members.fetch({ user: userId, force: true });
  } catch (err) {
    // Rule 20: a REST answer of "unknown member / user" (the scheduler only hands us available guilds).
    if (err?.code === 10007 || err?.code === 10013) {
      await prisma.captchaVerification.deleteMany({ where });
      return;
    }
    throw err;
  }
  if (member.roles.cache.has(role.id)) {
    await prisma.captchaVerification.deleteMany({ where }); // verified while no event reached us
    return;
  }
  // Cheap local pre-checks, so nobody is told "you were removed" and then stays. executeModAction is authoritative.
  if (isKickExempt(member, settings)) return;
  if (!guild.members.me.permissions.has(PermissionFlagsBits.KickMembers) || !member.kickable) {
    throttledWarn(`${guild.id}:kick`, `Captcha timeout: cannot kick ${userId} in ${guild.id} (permission or hierarchy)`);
    return;
  }

  const span = formatDurationLong(settings.captchaTimeoutMinutes * 60_000);
  if ((ctx?.attempt ?? 1) <= 1) await dmBeforeKick(member, span); // a scheduler retry must not DM twice
  const run = deps.executeModAction ?? executeModAction;
  const res = await run({
    guild,
    target: member,
    moderator: guild.client.user,
    action: 'KICK',
    reason: clampText(`Captcha: not verified within ${span}`, 500),
    source: 'captcha',
    silent: true, // our own DM above; moderation does not DM again
    skipEscalation: true,
  });

  if (res?.ok) {
    await prisma.captchaVerification.deleteMany({ where });
    return;
  }
  if (res?.applied === true) {
    // The member was kicked but the case could not be saved: do not kick again, do not retry.
    throttledWarn(`${guild.id}:kickcase`, `Captcha timeout: ${userId} was kicked in ${guild.id} but the case was not saved: ${res.error}`);
    await prisma.captchaVerification.deleteMany({ where });
    return;
  }
  switch (res?.code) {
    case 'NOT_IN_GUILD':
      await prisma.captchaVerification.deleteMany({ where });
      return;
    case 'IMMUNE':
    case 'HIERARCHY':
    case 'NOT_PERMITTED':
      // No retry: the row stays until the member verifies or leaves.
      throttledWarn(`${guild.id}:kick`, `Captcha timeout: ${userId} not kicked in ${guild.id} (${res.code}): ${res.error}`);
      return;
    default:
      throw new Error(`captcha.timeout kick failed (${res?.code ?? 'unknown'}): ${res?.error ?? 'no result'}`);
  }
}

// ── Component handlers ────────────────────────────────────────────────────────

/** Buttons `captcha:start` and `captcha:answer` (module-gated by the plugin loader). */
export async function handleCaptchaButton(interaction) {
  const { params } = parseCustomId(interaction.customId);
  if (params[0] === 'answer') return showAnswerModal(interaction); // the first response; no I/O before it
  if (params[0] === 'start') return handleStart(interaction);
  return interaction.reply({ content: 'This button is no longer active.', ephemeral: true }).catch(() => {});
}

async function showAnswerModal(interaction) {
  const modal = new ModalBuilder()
    .setCustomId(CUSTOM_IDS.modalAnswer)
    .setTitle('Enter the code')
    .addComponents(new ActionRowBuilder().addComponents(new TextInputBuilder()
      .setCustomId(CUSTOM_IDS.answerField)
      .setLabel('Code from the image')
      .setStyle(TextInputStyle.Short)
      .setMinLength(1)
      .setMaxLength(16)
      .setRequired(true)
      .setPlaceholder('Type the 6 characters')));
  try {
    await interaction.showModal(modal);
  } catch (err) {
    if (isInteractionGone(err)) logger.debug(`captcha modal: interaction gone (${err?.code})`);
    else logger.warn('captcha modal failed', err);
  }
}

// Shared by Verify and the answer form. Returns the role to grant, or null when it has already answered.
async function preflight({ guild, member, settings, respond }) {
  const role = guild.roles.cache.get(settings.captchaRoleId ?? '');
  const chk = checkVerifiedRole(guild, role, settings);
  if (!chk.ok) {
    throttledWarn(`${guild.id}:role`, `Captcha role unusable in ${guild.id}: ${chk.reason}`);
    await respond(MSG.NOT_SET_UP());
    return null;
  }
  if (member.roles.cache.has(role.id)) {
    await completeVerification(member, settings, role); // finishes a leftover pending row; no role add
    await respond(MSG.ALREADY());
    return null;
  }
  return role;
}

async function handleStart(interaction) {
  if (!interaction.inCachedGuild()) {
    return interaction.reply({ content: 'This only works inside the server.', ephemeral: true }).catch(() => {});
  }
  const fromEphemeral = interaction.message?.flags?.has(MessageFlags.Ephemeral) === true;
  try {
    if (fromEphemeral) await interaction.deferUpdate(); // "New image" on a challenge
    else await interaction.deferReply({ ephemeral: true }); // the public panel
  } catch (err) {
    if (isInteractionGone(err)) return;
    throw err;
  }
  const respond = makeRespond(interaction);
  const { guild, guildId } = interaction;
  const userId = interaction.user.id;

  try {
    await userMutex.run(`${guildId}:${userId}`, async () => {
      const settings = await getSettingsSafe(guildId);
      if (!settings) return respond(MSG.TEMP_UNAVAILABLE());
      if (!settings.captchaEnabled) return respond({ content: MSG.MODULE_OFF, embeds: [], components: [], attachments: [] });
      const member = interaction.member;
      const role = await preflight({ guild, member, settings, respond });
      if (!role) return undefined;
      if (member.pending === true) return respond(MSG.SCREENING_FIRST());

      if (settings.captchaMode === 'BUTTON' || !isImageCaptchaAvailable()) {
        if (settings.captchaMode !== 'BUTTON') throttledWarn(`${guildId}:font`, 'Captcha font missing; using button mode');
        const r = await completeVerification(member, settings, role);
        return respond(r.ok ? MSG.VERIFIED(guild) : r.payload);
      }

      const limited = await checkImageLimits(guildId, userId);
      if (limited) return respond(limited);
      try {
        return await respond(await issueChallenge(member, settings));
      } catch (err) {
        logger.warn('Captcha image failed', err);
        return respond(MSG.IMAGE_FAILED());
      }
    });
  } catch (err) {
    logger.error(`Captcha start failed in ${guildId}`, err);
    await respond(MSG.TEMP_UNAVAILABLE());
  }
  return undefined;
}

/** Modals `captcha-modal:answer` (checks the module itself) and `captcha-modal:panel` (admin form). */
export async function handleCaptchaModal(interaction) {
  const { params } = parseCustomId(interaction.customId);
  if (params[0] === 'answer') return handleAnswerSubmit(interaction);
  if (params[0] === 'panel') return handlePanelModal(interaction);
  return interaction.reply({ content: 'This form has expired. Please run the command again.', ephemeral: true }).catch(() => {});
}

async function handleAnswerSubmit(interaction) {
  if (!interaction.inCachedGuild()) return undefined;
  const { guild, guildId } = interaction;
  // The modal prefix is not module-gated by the loader and this is the first response: bounded settings read.
  const settings = await settingsForAck(interaction);
  if (!settings) return replyEphemeral(interaction, MSG.TEMP_UNAVAILABLE());
  if (!settings.captchaEnabled) return replyEphemeral(interaction, MSG.MODULE_OFF);

  try {
    if (interaction.isFromMessage()) await interaction.deferUpdate();
    else await interaction.deferReply({ ephemeral: true });
  } catch (err) {
    if (isInteractionGone(err)) return undefined;
    throw err;
  }
  const respond = makeRespond(interaction);
  const userId = interaction.user.id;

  try {
    await userMutex.run(`${guildId}:${userId}`, async () => {
      const member = interaction.member;
      const role = await preflight({ guild, member, settings, respond });
      if (!role) return undefined;
      if (settings.captchaMode === 'BUTTON' || !isImageCaptchaAvailable()) {
        return respond({ content: 'Verification changed. Press **Verify** on the panel to continue.', embeds: [], components: [], attachments: [] });
      }

      let input = '';
      try {
        input = interaction.fields.getTextInputValue(CUSTOM_IDS.answerField);
      } catch {
        input = '';
      }
      const row = await prisma.captchaVerification.findUnique({ where: { guildId_userId: { guildId, userId } } });
      const ev = evaluateAnswer({ row, guildId, userId, input });

      if (ev.result === 'ok') {
        if (member.pending === true) return respond(MSG.SCREENING_FIRST()); // screening state changed mid-challenge
        const r = await completeVerification(member, settings, role);
        return respond(r.ok ? MSG.VERIFIED(guild) : r.payload);
      }
      if (ev.result === 'wrong') {
        // Matching on answerHash means a stale submit never counts against a newer challenge.
        await prisma.captchaVerification.updateMany({
          where: { guildId, userId, answerHash: row.answerHash },
          data: { attempts: { increment: 1 } },
        });
        const left = ev.attemptsLeft;
        return respond({ content: `❌ That code is not right. **${left}** ${left === 1 ? 'try' : 'tries'} left.` }); // only the text changes
      }

      const prefix = {
        exhausted: "❌ Too many wrong tries. Here's a new image.",
        expired: "⌛ That code expired. Here's a new image.",
        none: "That code is no longer valid. Here's a new image.",
      }[ev.result];
      const limited = await checkImageLimits(guildId, userId);
      if (limited) {
        await prisma.captchaVerification.updateMany({ where: { guildId, userId }, data: { answerHash: null, expiresAt: null } });
        return respond({ ...limited, content: '', components: [], attachments: [] });
      }
      try {
        return await respond(await issueChallenge(member, settings, { content: prefix }));
      } catch (err) {
        logger.warn('Captcha image failed', err);
        return respond({ ...MSG.IMAGE_FAILED(), content: '', components: [], attachments: [] });
      }
    });
  } catch (err) {
    logger.error(`Captcha answer failed in ${guildId}`, err);
    await respond(MSG.TEMP_UNAVAILABLE());
  }
  return undefined;
}

// The admin form from `/captcha message`. Admin-only; works while the module is off.
async function handlePanelModal(interaction) {
  if (!interaction.inCachedGuild()) return undefined;
  const { guild, guildId } = interaction;
  const settings = await settingsForAck(interaction);
  if (!settings) return replyEphemeral(interaction, MSG.TEMP_UNAVAILABLE());
  if (resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.ADMIN) {
    return replyEphemeral(interaction, { embeds: [errorEmbed('Insufficient Permissions', 'Only server admins can edit the verification panel.')] });
  }
  try {
    await interaction.deferReply(); // public (admin convention)
  } catch (err) {
    if (isInteractionGone(err)) return undefined;
    throw err;
  }
  const respond = makeRespond(interaction);
  const { template, errors } = parseTemplateModal(interaction, panelTemplate(settings));
  if (errors.length) {
    return respond({ embeds: [errorEmbed('Not Saved', clampText(errors.join('\n'), 4000))] });
  }
  const after = await updateSettingsWith(guildId, (cur) => ({
    captchaTemplate: { ...normalizeTemplate(template), verifyButton: parseVerifyButton(cur.captchaTemplate?.verifyButton) },
  }));
  const r = await refreshPanel(guild, after);
  return respond({ embeds: [successEmbed('Panel Updated', r.ok ? 'The verification panel was updated.' : `Saved. ${r.reason}`)] });
}
