// Temporary voice channels (pkg-temp-voice.md): join-to-create hubs, owner controls (/voice and a persistent control panel),
// keep-alive deletion, ownership claims, bans, a companion text channel and startup reconciliation (MEE6 "Temporary Channels").
// Everything lives here: constants and pure helpers, the guild routing memo, the per-hub / per-channel locks, every flow, the
// persistent component handlers, the scheduler job body, the channelDelete / guildReady / voiceStateUpdate hook bodies and the CRUD
// that /voice-hub uses. Side-effect free at import: no timers, no I/O, no queries. Module-level state is limited to Maps, one keyed
// mutex and a throttle (spec §3.4); all of it may vanish on restart because every customId carries the ids it needs and the
// durable state (rows, keep-alive jobs) lives in Postgres.
//
// Rule 20 (a cache miss is not a deletion): a row is purged only after a delete event, or after a REST fetch/delete answered
// 10003 while the guild is available (`confirmChannel`). Any other failure keeps the row.
import {
  ActionRowBuilder, ButtonBuilder, ButtonStyle, ChannelType, ModalBuilder, OverwriteType, PermissionFlagsBits,
  TextInputBuilder, TextInputStyle, UserSelectMenuBuilder, escapeMarkdown,
} from 'discord.js';
import { Prisma } from '@prisma/client';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import { ensureGuildSettings, getSettingsSafe, isModuleEnabled, peekSettings } from './settingsService.js';
import { cancelJobs, scheduleJob } from './scheduler.js';
import { LIMITS, PERMISSION_LEVELS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import {
  TimeoutError, ackBudgetMs, createKeyedMutex, createThrottle, mapLimit, withTimeout,
} from '../utils/async.js';
import { renderPlaceholders, renderPlaceholdersDetailed } from '../utils/placeholderParser.js';
import {
  applyGuildColor, errorEmbed, infoEmbed, neutralEmbed, successEmbed, warnEmbed,
} from '../utils/embedBuilder.js';
import {
  checkChannelUsable, isInteractionGone, missingBotPermissions, permissionNames, safeDefer, safeReply,
} from '../utils/interactionHelpers.js';
import { clampText, joinWithLimit } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import { markBotDeletion } from '../utils/deletionTracker.js';
import logger from '../utils/logger.js';

// ── Constants (spec §6.1) ─────────────────────────────────────────────────────

export const DEFAULT_NAME_FORMAT = "#{index} - {username}'s Channel";
export const HUB_CHANNEL_NAME = '➕ Join to Create';
export const MAX_HUB_ROLE_LIST = 10;
export const MAX_BANS_PER_CHANNEL = 50;
export const RENAME_WINDOW_MS = 600_000;
export const RENAME_MAX = 2;
export const RENAME_TIMEOUT_MS = 8_000;
export const CREATE_COOLDOWN_MS = 5_000;
export const INDEX_TTL_MS = 300_000;
export const TEXT_SYNC_DELAY_MS = 1_500;
export const DELETE_RETRY_MS = 5 * 60_000;
export const CLEANUP_JOB = 'tempvoice.cleanup';
export const cleanupKey = (channelId) => `tempvoice.cleanup:${channelId}`;
export const renameKey = (channelId) => `tempvoice:rename:${channelId}`;

const P = PermissionFlagsBits;

// Bitfields (bigint).
export const OWNER_PERMS = P.ManageChannels | P.MoveMembers | P.PrioritySpeaker | P.Connect | P.ViewChannel; // 17827088n
export const BOT_VOICE_PERMS = P.ViewChannel | P.Connect | P.ManageChannels | P.MoveMembers | P.SendMessages | P.EmbedLinks | P.ReadMessageHistory; // 17910800n
export const BOT_TEXT_PERMS = P.ViewChannel | P.SendMessages | P.EmbedLinks | P.ReadMessageHistory | P.ManageMessages | P.PinMessages | P.ManageChannels;
// Permission-name lists for missingBotPermissions().
export const REQUIRED_BOT_PERMS = Object.freeze(['ManageChannels', 'ManageRoles', 'MoveMembers', 'ViewChannel', 'Connect']);
export const VOICE_ACTIONS = Object.freeze(['ban', 'claim', 'hide', 'kick', 'limit', 'lock', 'rename', 'reveal', 'transfer', 'unban', 'unlock']);
export const BUTTON_ACTIONS = Object.freeze(['lock', 'unlock', 'hide', 'reveal', 'claim', 'limit', 'rename', 'kick', 'ban', 'transfer']);
export const SELECT_ACTIONS = Object.freeze(['kick', 'ban', 'unban', 'transfer']);
export const MODAL_ACTIONS = Object.freeze(['limit', 'rename']);

const INDEX_MEMO_MAX = 10_000;
const RENAME_STAMPS_MAX = 5_000;
const CREATE_PRUNE_MS = 60_000;
const SNOWFLAKE_RE = /^\d{17,20}$/;
const MENTION_TOKEN_RE = /<(?:@[!&]?|#)\d+>/g;
const HUB_CHANNEL_PERMS = Object.freeze(['ViewChannel', 'Connect', 'MoveMembers']);

export class HubLimitError extends Error {
  constructor() {
    super(`A server can have at most ${LIMITS.TEMPVOICE_HUBS} hubs`);
    this.name = 'HubLimitError';
  }
}

// ── Raw SQL (rule 22: exported builders; no timestamps are involved) ──────────

const guildSettingsLockSql = (guildId) => Prisma.sql`SELECT 1 AS ok FROM "GuildSettings" WHERE "guildId" = ${guildId} FOR UPDATE`;
const hubLockSql = (hubId, guildId) => Prisma.sql`SELECT 1 AS ok FROM "TempVoiceHub" WHERE "id" = ${hubId} AND "guildId" = ${guildId} FOR UPDATE`;

export const RAW_SQL = Object.freeze([
  { name: 'tempvoice.guildLock', build: () => guildSettingsLockSql('0') },
  { name: 'tempvoice.hubLock', build: () => hubLockSql('0', '0') },
]);

// ── Messages (spec §4.3) ──────────────────────────────────────────────────────
// Every value is a function `(params) => EmbedBuilder`. Params: `target` / `owner` (user id), `n`, `at` / `retryAt` (ms),
// `names` (permission display names), `channelId`, `code` (Discord error code).

const EMBED = { error: errorEmbed, warn: warnEmbed, success: successEmbed, info: infoEmbed };
const msg = (kind, title, text) => (p = {}) => EMBED[kind](title, typeof text === 'function' ? text(p) : text);
const ping = (id) => `<@${id}>`;

/** @param {number|string|null|undefined} code @returns {string} */
export function describeDiscordError(code) {
  switch (code) {
    case 50013: return 'missing permissions';
    case 50001: return 'no access';
    case 10003: return 'channel not found';
    case 30013: return 'the server has 500 channels';
    case 50035: return 'invalid request (is the category full?)';
    case 40032: return 'member not connected';
    default: return `error ${code ?? 'unknown'}`;
  }
}

export const MSG = Object.freeze({
  NOT_IN_TEMP: msg('error', 'Not In A Temporary Channel', 'Join your temporary voice channel first, then run this command.'),
  CHANNEL_GONE: msg('error', 'Channel Gone', 'This temporary channel no longer exists.'),
  NOT_ALLOWED: msg('error', 'Not Your Channel', 'Only the channel owner or a temp-voice moderator can do that.'),
  CLEAN_NOT_ALLOWED: msg('error', 'Insufficient Permissions', 'Only moderators can clean up temporary channels.'),
  TARGET_SELF: msg('error', 'Invalid Member', "You can't do that to yourself."),
  TARGET_OWNER: msg('error', 'Invalid Member', (p) => `${ping(p.target)} owns this channel. Transfer ownership first.`),
  TARGET_PROTECTED: msg('error', 'Protected Member', (p) => `${ping(p.target)} is a moderator or has an ignored role, so /voice commands don't affect them.`),
  TARGET_NOT_MEMBER: msg('error', 'Invalid Member', 'That user is not in this server.'),
  TARGET_NOT_CONNECTED: msg('error', 'Not In Channel', (p) => `${ping(p.target)} is not in this voice channel.`),
  TARGET_BOT: msg('error', 'Invalid Member', "Bots can't own a channel."),
  TARGET_ALREADY_OWNER: msg('warn', 'No Change', (p) => `${ping(p.target)} already owns this channel.`),
  ALREADY_BANNED: msg('warn', 'No Change', (p) => `${ping(p.target)} is already banned from this channel.`),
  NOT_BANNED: msg('warn', 'No Change', (p) => `${ping(p.target)} is not banned from this channel.`),
  BAN_LIMIT: msg('error', 'Limit Reached', `A channel can ban at most ${MAX_BANS_PER_CHANNEL} members.`),
  NO_CHANGE_LOCK: msg('warn', 'No Change', 'This channel is already locked.'),
  NO_CHANGE_UNLOCK: msg('warn', 'No Change', 'This channel is already unlocked.'),
  NO_CHANGE_HIDE: msg('warn', 'No Change', 'This channel is already hidden.'),
  NO_CHANGE_REVEAL: msg('warn', 'No Change', 'This channel is already visible.'),
  NO_CHANGE_NAME: msg('warn', 'No Change', "That is already the channel's name."),
  NO_CHANGE_LIMIT: msg('warn', 'No Change', (p) => `The member limit is already ${p.n ? p.n : 'off'}.`),
  CLAIM_ALREADY_OWNER: msg('warn', 'No Change', 'You already own this channel.'),
  CLAIM_NOT_CONNECTED: msg('error', 'Not In Channel', 'Join the channel first, then claim it.'),
  CLAIM_OWNER_PRESENT: msg('error', 'Owner Still Here', (p) => `${ping(p.owner)} is still in the channel.`),
  CLAIM_LOCKED: msg('error', 'Not Yet', (p) => `This channel can be claimed ${discordTimestamp(p.at, 'R')}.`),
  RENAME_INVALID: msg('error', 'Invalid Name', 'Channel names must be 1–100 characters.'),
  RENAME_RATE: msg('error', 'Slow Down', (p) => `Discord allows only 2 renames per channel every 10 minutes. Try again ${discordTimestamp(p.retryAt, 'R')}.`),
  RENAME_QUEUED: msg('warn', 'Rename Queued', 'Discord is rate-limiting renames of this channel. The new name will apply when the limit ends (up to 10 minutes).'),
  LIMIT_INVALID: msg('error', 'Invalid Limit', 'Enter a whole number from 0 to 99 (0 = no limit).'),
  BOT_MISSING: msg('error', 'Missing Permissions', (p) => `I need **${(p.names ?? []).join(', ')}** in <#${p.channelId}> to do that.`),
  DISCORD_ERROR: msg('error', 'Something Went Wrong', (p) => `Discord rejected that change (${describeDiscordError(p.code)}). Please try again in a moment.`),
  DB_ERROR: msg('error', 'Something Went Wrong', 'The channel changed, but I could not save it. Please try again.'),
  // Not in the spec table: the hub channel was deleted, so there is no permission template left to rebuild the overwrites from.
  HUB_GONE: msg('error', 'Hub Deleted', 'The hub this channel came from was deleted, so its permissions cannot be changed safely any more. Lock, unlock, hide, reveal, ban, unban, transfer and claim are unavailable here; kick, limit and rename still work.'),
  INTERNAL: msg('error', 'Something Went Wrong', 'Something went wrong. Please try again in a moment.'),
  NOT_IN_GUILD: msg('error', 'Server Only', 'This only works in a server.'),
  // Not in the spec table: a pre-acknowledgement lookup ran out of its 3-second budget (rule 21).
  TRY_AGAIN: msg('error', 'Something Went Wrong', 'That took too long. Please try again.'),
});

const channelLabel = (name) => `**${escapeMarkdown(clampText(name ?? 'this channel', 100))}**`;

/** The success embed of a voice action. `p`: { name, target, n, newName }. */
export function okEmbed(action, p = {}) {
  const name = channelLabel(p.name);
  switch (action) {
    case 'lock': return successEmbed('Channel Locked', `🔒 New members can no longer join ${name}.`);
    case 'unlock': return successEmbed('Channel Unlocked', `🔓 ${name} is open to join again.`);
    case 'hide': return successEmbed('Channel Hidden', `🙈 ${name} is hidden from the channel list.`);
    case 'reveal': return successEmbed('Channel Revealed', `👁️ ${name} is visible again.`);
    case 'limit': return successEmbed('Limit Updated', p.n > 0 ? `👥 ${name} now allows **${p.n}** members.` : `👥 ${name} has no member limit now.`);
    case 'rename': return successEmbed('Channel Renamed', `✏️ The channel is now called ${channelLabel(p.newName)}.`);
    case 'kick': return successEmbed('Member Kicked', `👢 Disconnected ${ping(p.target)} from ${name}.`);
    case 'ban': return successEmbed('Member Banned', `🔨 ${ping(p.target)} can no longer join ${name}.`);
    case 'unban': return successEmbed('Member Unbanned', `✅ ${ping(p.target)} can join ${name} again.`);
    case 'transfer': return successEmbed('Ownership Transferred', `👑 ${ping(p.target)} now owns ${name}.`);
    case 'claim': return successEmbed('Channel Claimed', `👑 You now own ${name}.`);
    case 'clean': return p.n > 0
      ? successEmbed('Channels Cleaned', `🧹 Deleted **${p.n}** empty temporary channel(s).`)
      : infoEmbed('Nothing To Clean', 'There are no empty temporary channels.');
    default: return MSG.INTERNAL();
  }
}

// ── Pure helpers (spec §6.1) ──────────────────────────────────────────────────

/** Smallest positive integer not in `used` (non-integers and values <= 0 are ignored). */
export function lowestFreeIndex(used) {
  const taken = new Set((used ?? []).filter((n) => Number.isInteger(n) && n > 0));
  let i = 1;
  while (taken.has(i)) i++;
  return i;
}

/** Collapses whitespace, trims, cuts to 100 code points. Returns null for an empty result. */
export function normalizeChannelName(input) {
  if (input == null) return null;
  const s = String(input).replace(/[\r\n\t]+/g, ' ').replace(/\s+/g, ' ').trim();
  if (!s) return null;
  const chars = [...s];
  if (chars.length <= 100) return s;
  return chars.slice(0, 100).join('').trimEnd();
}

/** The channel name for a new temporary channel; never empty, never contains a mention token. */
export function renderChannelName(format, { member, guild, index }) {
  const render = (fmt) => {
    const text = renderPlaceholders(
      fmt || DEFAULT_NAME_FORMAT,
      { member, guild, vars: { index, username: member?.displayName ?? '' } },
      { maxLength: 100 },
    );
    return normalizeChannelName(String(text ?? '').replace(MENTION_TOKEN_RE, ''));
  };
  return render(format) ?? render(DEFAULT_NAME_FORMAT) ?? `#${index}`;
}

/**
 * Validates a hub's name format.
 * @returns {{ ok: true, preview: string, warnings: string[] } | { ok: false, error: string }}
 */
export function validateNameFormat(format, { member, guild }) {
  const raw = String(format ?? '').trim();
  if (raw.length === 0 || raw.length > 100) return { ok: false, error: 'Name formats must be 1–100 characters.' };
  const r = renderPlaceholdersDetailed(
    raw,
    { member, guild, vars: { index: 1, username: member?.displayName ?? '' } },
    { maxLength: 100 },
  );
  if (/<[@#][!&]?\d+>/.test(r.text)) {
    return { ok: false, error: "Channel names can't contain mentions. Use {username} or {user.name} instead of {user}." };
  }
  const preview = normalizeChannelName(r.text);
  if (preview === null) return { ok: false, error: 'That format renders an empty name.' };
  const warnings = [];
  if (r.unknownTokens?.length) {
    const tokens = r.unknownTokens.map((t) => `\`${t.startsWith('{') && t.endsWith('}') ? t : `{${t}}`}\``);
    warnings.push(`Unknown placeholders are kept as typed: ${tokens.join(', ')}`);
  }
  return { ok: true, preview, warnings };
}

/** A Discord text-channel name derived from the voice channel's name. */
export function textChannelName(voiceName, index) {
  const s = String(voiceName ?? '')
    .normalize('NFKC')
    .toLowerCase()
    .replace(/\s+/gu, '-')
    .replace(/[^\p{L}\p{N}_-]/gu, '')
    .replace(/-{2,}/g, '-')
    .replace(/^-+|-+$/g, '');
  const cut = [...s].slice(0, 100).join('').replace(/^-+|-+$/g, '');
  return cut || `voice-${index}`;
}

/** The bitrate (bps) a new channel gets, or undefined for Discord's default. */
export function effectiveBitrate(stored, guildMax) {
  if (!Number.isInteger(stored) || stored <= 0) return undefined;
  const max = Number.isFinite(guildMax) && guildMax > 0 ? guildMax : stored;
  return Math.max(8000, Math.min(stored, max));
}

/** Parses the limit modal's text: a whole number 0-99, or null. */
export function parseLimitInput(str) {
  const s = String(str ?? '').trim();
  return /^\d{1,2}$/.test(s) ? Number(s) : null;
}

/**
 * @param {number[]} stamps epoch ms of earlier renames
 * @param {number} now
 * @returns {{ ok: true, recent: number[] } | { ok: false, retryAt: number, recent: number[] }}
 */
export function checkRenameWindow(stamps, now) {
  const recent = (stamps ?? [])
    .filter((t) => Number.isFinite(t) && t > now - RENAME_WINDOW_MS && t <= now)
    .sort((a, b) => a - b);
  if (recent.length >= RENAME_MAX) return { ok: false, retryAt: recent[recent.length - RENAME_MAX] + RENAME_WINDOW_MS, recent };
  return { ok: true, recent };
}

/** Whether `/voice claim` may succeed. */
export function claimStatus({ isOwner, actorConnected, ownerConnected, ownerLeftAt, lockMinutes, isMod, now }) {
  if (isOwner) return { ok: false, code: 'ALREADY_OWNER' };
  if (!actorConnected) return { ok: false, code: 'NOT_CONNECTED' };
  if (ownerConnected) return { ok: false, code: 'OWNER_PRESENT' };
  const claimableAt = (ownerLeftAt?.getTime?.() ?? now) + (Number(lockMinutes) || 0) * 60_000;
  if (!isMod && now < claimableAt) return { ok: false, code: 'LOCKED', claimableAt };
  return { ok: true };
}

/** The MSG key explaining why a kick/ban/unban/transfer target is refused, or null. */
export function targetProblem({
  action, actorId, targetId, ownerId, targetIsBot = false, targetProtected = false, targetConnected = false,
  isBanned = false, banCount = 0,
}) {
  switch (action) {
    case 'kick':
      if (targetId === actorId) return 'TARGET_SELF';
      if (targetId === ownerId) return 'TARGET_OWNER';
      if (targetProtected) return 'TARGET_PROTECTED';
      if (!targetConnected) return 'TARGET_NOT_CONNECTED';
      return null;
    case 'ban':
      if (targetId === actorId) return 'TARGET_SELF';
      if (targetId === ownerId) return 'TARGET_OWNER';
      if (targetProtected) return 'TARGET_PROTECTED';
      if (isBanned) return 'ALREADY_BANNED';
      if (banCount >= MAX_BANS_PER_CHANNEL) return 'BAN_LIMIT';
      return null;
    case 'unban':
      return isBanned ? null : 'NOT_BANNED';
    case 'transfer':
      if (targetId === ownerId) return 'TARGET_ALREADY_OWNER';
      if (targetIsBot) return 'TARGET_BOT';
      if (!targetConnected) return 'TARGET_NOT_CONNECTED';
      return null;
    default:
      return null;
  }
}

/** claim is open to anyone (its rules live in claimStatus); everything else needs the owner or a temp-voice moderator. */
export function canControl({ action, isOwner, isMod }) {
  if (action === 'claim') return true;
  return VOICE_ACTIONS.includes(action) ? Boolean(isOwner || isMod) : false;
}

const hasAnyRole = (member, ids) => (ids ?? []).some((id) => member?.roles?.cache?.has(id));

/** A moderator (MODERATOR level or a hub moderator role) may use /voice on any channel of the hub. */
export function isTempVoiceMod(member, hub, settings) {
  return resolvePermissionLevel(member, settings) >= PERMISSION_LEVELS.MODERATOR || hasAnyRole(member, hub?.moderatorRoleIds);
}

/** /voice commands do not affect moderators or members with an ignored role. */
export function isProtectedTarget(member, hub, settings) {
  return resolvePermissionLevel(member, settings) >= PERMISSION_LEVELS.MODERATOR
    || hasAnyRole(member, [...(hub?.moderatorRoleIds ?? []), ...(hub?.ignoredRoleIds ?? [])]);
}

/** Roles that stay able to see/join a locked or hidden channel: ignored + moderator roles + the guild's mod roles. */
export function exemptRoleIds(hub, settings, guild) {
  const all = new Set([...(hub?.ignoredRoleIds ?? []), ...(hub?.moderatorRoleIds ?? []), ...(settings?.modRoleIds ?? [])]);
  return [...all].filter((id) => id !== guild?.id && guild?.roles?.cache?.has(id));
}

/** Human (non-bot) members connected to a voice channel. */
export function countHumans(channel) {
  return channel?.members ? channel.members.filter((m) => !m.user?.bot).size : 0;
}

/** @returns {Array<{ id: string, type: number, allow: bigint, deny: bigint }>} */
export function snapshotOverwrites(channel) {
  if (!channel?.permissionOverwrites?.cache) return [];
  return [...channel.permissionOverwrites.cache.values()].map((o) => ({
    id: o.id, type: o.type, allow: BigInt(o.allow.bitfield), deny: BigInt(o.deny.bitfield),
  }));
}

/** What reconciliation should do with one row. Pure; `channelExists` must come from `confirmChannel`, never from a cache miss. */
export function reconcileDecision({ channelExists, humans, keepAliveMinutes, emptySince, now }) {
  if (!channelExists) return { action: 'purge' };
  if (humans > 0) return { action: 'occupied' };
  if (keepAliveMinutes === 0) return { action: 'delete' };
  const due = (emptySince?.getTime?.() ?? now) + keepAliveMinutes * 60_000;
  if (due <= now) return { action: 'delete' };
  return { action: 'schedule', runAt: due, setEmptySince: !emptySince };
}

/** @returns {number|null} */
export function discordErrorCode(err) {
  if (typeof err?.code === 'number') return err.code;
  const raw = err?.rawError?.code;
  return typeof raw === 'number' ? raw : null;
}

// ── Overwrites (spec §6.10, §6.11; pure) ──────────────────────────────────────

function overwriteMap(template) {
  const map = new Map();
  for (const o of template ?? []) {
    map.set(String(o.id), { id: String(o.id), type: o.type, allow: BigInt(o.allow ?? 0n), deny: BigInt(o.deny ?? 0n) });
  }
  const ensure = (id, type) => {
    const key = String(id);
    if (!map.has(key)) map.set(key, { id: key, type, allow: 0n, deny: 0n });
    return map.get(key);
  };
  const allowBits = (o, bits) => { o.allow |= bits; o.deny &= ~bits; };
  const denyBits = (o, bits) => { o.deny |= bits; o.allow &= ~bits; };
  return { map, ensure, allowBits, denyBits };
}

function finishOverwrites(map, grantable) {
  const out = [];
  for (const o of map.values()) {
    if (grantable !== null && grantable !== undefined) {
      // Discord only lets a bot allow or deny permissions it has itself; Manage Permissions is never granted by an overwrite.
      o.allow = o.allow & grantable & ~P.ManageRoles;
      o.deny &= grantable;
    }
    if (o.allow !== 0n || o.deny !== 0n) out.push(o);
  }
  return out.sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

/**
 * The whole overwrite list of a temporary voice channel, recomputed from (template + state).
 * @returns {Array<{ id: string, type: number, allow: bigint, deny: bigint }>} sorted by id, no empty entries
 */
export function computeVoiceOverwrites({
  everyoneId, botId, ownerId, template = [], locked = false, hidden = false, bannedUserIds = [],
  exemptRoleIds: exempt = [], grantable = null,
}) {
  const { map, ensure, allowBits, denyBits } = overwriteMap(template);
  const everyone = ensure(everyoneId, OverwriteType.Role);
  if (hidden) denyBits(everyone, P.ViewChannel);
  if (locked) denyBits(everyone, P.Connect);
  if (hidden || locked) {
    // A template allow (e.g. a category that denies @everyone and allows a Verified role) beats the @everyone deny, so it
    // would leave the channel open to that role: while locked or hidden only exempt roles (and the owner, below) keep it.
    const stripped = (hidden ? P.ViewChannel : 0n) | (locked ? P.Connect : 0n);
    for (const o of map.values()) {
      if (o.id !== everyoneId && !exempt.includes(o.id)) o.allow &= ~stripped;
    }
    for (const roleId of exempt) {
      if (roleId === everyoneId) continue;
      const o = ensure(roleId, OverwriteType.Role);
      if (hidden) allowBits(o, P.ViewChannel);
      if (locked) allowBits(o, P.Connect);
    }
  }
  for (const userId of bannedUserIds) {
    if (userId === ownerId || userId === botId) continue;
    denyBits(ensure(userId, OverwriteType.Member), P.Connect);
  }
  allowBits(ensure(ownerId, OverwriteType.Member), OWNER_PERMS);
  allowBits(ensure(botId, OverwriteType.Member), BOT_VOICE_PERMS);
  return finishOverwrites(map, grantable);
}

/** The overwrite list of the companion text channel (members-only visibility mirrors the voice channel's members). */
export function computeTextOverwrites({
  everyoneId, botId, ownerId, template = [], membersOnly = true, memberIds = [], bannedUserIds = [],
  exemptRoleIds: exempt = [], grantable = null,
}) {
  const { map, ensure, allowBits, denyBits } = overwriteMap(template);
  if (membersOnly) {
    denyBits(ensure(everyoneId, OverwriteType.Role), P.ViewChannel);
    for (const roleId of exempt) {
      if (roleId !== everyoneId) allowBits(ensure(roleId, OverwriteType.Role), P.ViewChannel);
    }
    for (const userId of memberIds) allowBits(ensure(userId, OverwriteType.Member), P.ViewChannel);
  } else {
    for (const userId of bannedUserIds) {
      if (userId === ownerId || userId === botId) continue;
      denyBits(ensure(userId, OverwriteType.Member), P.ViewChannel);
    }
  }
  allowBits(ensure(botId, OverwriteType.Member), BOT_TEXT_PERMS);
  return finishOverwrites(map, grantable);
}

/** `null` when the bot is Administrator (nothing is masked), else the bitfield the bot holds in `scope` (a channel) or the guild. */
export function grantableBits(guild, scope) {
  const me = guild.members.me;
  const perms = scope ? me.permissionsIn(scope) : me.permissions;
  return perms.has(P.Administrator) ? null : perms.bitfield;
}

// ── Control panel, pickers and the value modal (spec §6.12; pure builders) ────

const PANEL_ROW_1 = Object.freeze([
  ['lock', 'Lock', '🔒', ButtonStyle.Secondary], ['unlock', 'Unlock', '🔓', ButtonStyle.Secondary],
  ['hide', 'Hide', '🙈', ButtonStyle.Secondary], ['reveal', 'Reveal', '👁️', ButtonStyle.Secondary],
  ['claim', 'Claim', '👑', ButtonStyle.Primary],
]);
const PANEL_ROW_2 = Object.freeze([
  ['limit', 'Limit', '👥', ButtonStyle.Secondary], ['rename', 'Rename', '✏️', ButtonStyle.Secondary],
  ['kick', 'Kick', '👢', ButtonStyle.Secondary], ['ban', 'Ban', '🔨', ButtonStyle.Danger],
  ['transfer', 'Transfer', '🤝', ButtonStyle.Secondary],
]);

/** @returns {{ embeds: object[], components: object[], allowedMentions: { parse: [] } }} */
export function buildControlPanel({ channelId, ownerId, settings }) {
  const description = [
    `**Owner:** <@${ownerId}>`,
    '',
    'Use the buttons below or the `/voice` commands:',
    '🔒 **Lock** / 🔓 **Unlock**: stop or allow new members joining',
    '🙈 **Hide** / 👁️ **Reveal**: hide the channel from the channel list',
    '👑 **Claim**: take over after the owner leaves',
    '👥 **Limit** · ✏️ **Rename** · 👢 **Kick** · 🔨 **Ban** · 🤝 **Transfer**',
  ].join('\n');
  const embed = applyGuildColor(
    neutralEmbed('🔊 Temporary channel', description, 'Only the owner and moderators can use these controls.'),
    settings,
  );
  const row = (defs) => new ActionRowBuilder().addComponents(defs.map(([action, label, emoji, style]) => new ButtonBuilder()
    .setCustomId(buildCustomId('voice', action, channelId))
    .setLabel(label)
    .setEmoji(emoji)
    .setStyle(style)));
  return { embeds: [embed], components: [row(PANEL_ROW_1), row(PANEL_ROW_2)], allowedMentions: { parse: [] } };
}

/** The ephemeral user-select message behind the kick / ban / transfer buttons. */
export function buildMemberPicker(action, channelId, bannedUserIds = []) {
  const select = (act, placeholder) => new ActionRowBuilder().addComponents(new UserSelectMenuBuilder()
    .setCustomId(buildCustomId('voice-select', act, channelId))
    .setPlaceholder(placeholder)
    .setMinValues(1)
    .setMaxValues(1));
  let content;
  let components;
  if (action === 'kick') {
    content = 'Choose a member to disconnect.';
    components = [select('kick', 'Member to disconnect')];
  } else if (action === 'ban') {
    content = 'Choose a member to ban, or one to unban.';
    if (bannedUserIds.length) content += `\nBanned: ${joinWithLimit(bannedUserIds.map((u) => `<@${u}>`), { max: 1500 })}`;
    components = [select('ban', 'Member to ban'), select('unban', 'Member to unban')];
  } else {
    content = 'Choose the new owner (they must be in the channel).';
    components = [select('transfer', 'New owner')];
  }
  return { content, components, ephemeral: true, allowedMentions: { parse: [] } };
}

/** The limit / rename modal. */
export function buildValueModal(action, channelId, current) {
  const input = new TextInputBuilder().setCustomId(buildCustomId('voice-value')).setStyle(TextInputStyle.Short).setRequired(true);
  let title;
  if (action === 'limit') {
    title = 'Member limit';
    input.setLabel('Maximum members (0 = no limit)').setMinLength(1).setMaxLength(2).setValue(String(current ?? 0));
  } else {
    title = 'Rename channel';
    input.setLabel('New name').setMinLength(1).setMaxLength(100);
    const value = clampText(current ?? '', 100);
    if (value) input.setValue(value);
  }
  return new ModalBuilder()
    .setCustomId(buildCustomId('voice-modal', action, channelId))
    .setTitle(title)
    .addComponents(new ActionRowBuilder().addComponents(input));
}

// ── In-process state (spec §3.4; every table starts empty after a restart) ────

/** @type {Map<string, { at: number, hubs: Map<string, string>, temps: Set<string>, texts: Map<string, string>, categories: Set<string> }>} */
const indexMemo = new Map();
const indexLoads = new Map(); // guildId -> Promise (dedupes concurrent loads)
const indexGen = new Map(); // guildId -> write counter, so a load that raced a write is not memoised
const locks = createKeyedMutex(); // `hub:{hubId}` and `ch:{voiceChannelId}`; lock order: hub -> ch, never the reverse
const renameStamps = new Map(); // channelId -> number[] (epoch ms of successful renames)
const lastCreateAt = new Map(); // "guildId:userId" -> ms
const pendingRetries = new Set(); // "guildId:userId" keys that have a creation retry scheduled
const textSyncTimers = new Map(); // voiceChannelId -> Timeout
// At most one logger.warn per key per 10 minutes (missing permissions, full categories, ...).
const warnThrottle = createThrottle(600_000);

/** Runs a hook body off the dispatcher so `levels`, `logging` and `automations` are never delayed. */
export function runTask(label, fn) {
  Promise.resolve().then(fn).catch((err) => logger.error(`tempvoice ${label} failed`, err));
}

const QUIET_CODES = new Set([10003, 10007, 10008, 10013, 40032]);

/**
 * Wraps EVERY Discord REST call of this package. Never throws.
 * @template T
 * @param {string} label
 * @param {() => Promise<T>|T} fn
 * @param {string|null} [guildId] throttle key for the permission warnings
 * @returns {Promise<{ ok: true, value: T } | { ok: false, code: number|null, error: unknown }>}
 */
export async function discordCall(label, fn, guildId = null) {
  try {
    return { ok: true, value: await fn() };
  } catch (err) {
    const code = discordErrorCode(err);
    if (QUIET_CODES.has(code)) {
      logger.debug(`tempvoice ${label}: Discord error ${code}`);
    } else if (code === 50001 || code === 50013) {
      if (warnThrottle(`${label}:${guildId ?? '-'}`)) logger.warn(`tempvoice ${label}: ${describeDiscordError(code)} (guild ${guildId ?? '?'})`);
    } else {
      logger.warn(`tempvoice ${label} failed: ${err?.message ?? err}`);
    }
    return { ok: false, code, error: err };
  }
}

/** Deletes a channel by id (cached object when there is one), marking it as a bot deletion first. */
export function deleteDiscordChannel(guild, channelId, reason, label = 'delete channel') {
  markBotDeletion(channelId, 'tempvoice');
  const cached = guild.channels.cache.get(channelId);
  return discordCall(label, () => (cached ? cached.delete(reason) : guild.channels.delete(channelId, reason)), guild.id);
}

/**
 * Rule 20: whether a channel exists, decided by the cache or a REST fetch, never by a cache miss alone.
 * @returns {Promise<{ state: 'present', channel: object } | { state: 'gone', channel: null } | { state: 'unknown', channel: null, code?: number|null }>}
 *   `gone` only after a REST 10003 on an available guild; `unknown` means: keep every row.
 */
export async function confirmChannel(guild, channelId) {
  const cached = guild.channels.cache.get(channelId);
  if (cached) return { state: 'present', channel: cached };
  if (!guild.available) return { state: 'unknown', channel: null };
  const r = await discordCall('fetch channel', () => guild.channels.fetch(channelId), guild.id);
  if (r.ok && r.value) return { state: 'present', channel: r.value };
  if (!r.ok && r.code === 10003) return { state: 'gone', channel: null };
  return { state: 'unknown', channel: null, code: r.ok ? null : r.code };
}

// ── The guild index (spec §6.2) ───────────────────────────────────────────────

/** @returns {object|null} the fresh memo entry of a guild, or null (sync, no I/O) */
export function peekIndex(guildId) {
  const entry = guildId ? indexMemo.get(guildId) : undefined;
  return entry && Date.now() - entry.at < INDEX_TTL_MS ? entry : null;
}

/** Called after EVERY create/update/delete of a TempVoiceHub or TempVoiceChannel row. */
export function invalidateIndex(guildId) {
  indexMemo.delete(guildId);
  indexLoads.delete(guildId);
  indexGen.set(guildId, (indexGen.get(guildId) ?? 0) + 1);
}

async function doLoadIndex(guildId) {
  const gen = indexGen.get(guildId) ?? 0;
  try {
    const [hubs, temps] = await Promise.all([
      prisma.tempVoiceHub.findMany({ where: { guildId }, select: { id: true, channelId: true, categoryId: true } }),
      prisma.tempVoiceChannel.findMany({ where: { guildId }, select: { channelId: true, textChannelId: true } }),
    ]);
    const idx = {
      at: Date.now(),
      hubs: new Map(hubs.map((h) => [h.channelId, h.id])),
      temps: new Set(temps.map((t) => t.channelId)),
      texts: new Map(temps.filter((t) => t.textChannelId).map((t) => [t.textChannelId, t.channelId])),
      categories: new Set(hubs.map((h) => h.categoryId).filter(Boolean)),
    };
    const stale = (indexGen.get(guildId) ?? 0) !== gen;
    if (!stale) {
      indexMemo.delete(guildId);
      indexMemo.set(guildId, idx);
      while (indexMemo.size > INDEX_MEMO_MAX) indexMemo.delete(indexMemo.keys().next().value);
    }
    return { idx, stale };
  } catch (err) {
    // Fail closed: without the database no temp-voice action is taken (callers skip the event).
    if (warnThrottle(`index:${guildId}`)) logger.warn(`tempvoice: could not load the channel index of guild ${guildId}: ${err?.message ?? err}`);
    return null;
  }
}

/** @returns {Promise<object|null>} the routing index, or null on a database error. Never throws. */
export async function loadIndex(guildId) {
  let last = null;
  for (let attempt = 0; attempt < 3; attempt++) {
    const fresh = peekIndex(guildId);
    if (fresh) return fresh;
    let load = indexLoads.get(guildId);
    if (!load) {
      load = doLoadIndex(guildId).finally(() => {
        if (indexLoads.get(guildId) === load) indexLoads.delete(guildId);
      });
      indexLoads.set(guildId, load);
    }
    const res = await load;
    if (!res) return null;
    if (!res.stale) return res.idx;
    last = res.idx; // a write raced this load: load again
  }
  return last;
}

// ── Rows ──────────────────────────────────────────────────────────────────────

const loadRow = (channelId) => prisma.tempVoiceChannel.findUnique({ where: { channelId }, include: { hub: true } });

/** update() that ignores a row deleted meanwhile (P2025). */
async function updateRow(channelId, data) {
  try {
    return await prisma.tempVoiceChannel.update({ where: { channelId }, data });
  } catch (err) {
    if (err?.code === 'P2025') return null;
    throw err;
  }
}

/** Deletes a hub whose channel is gone (confirmed) once its last temporary channel closed. */
async function retireHubIfDone(guild, hubId, knownHub = null) {
  const hub = knownHub ?? await prisma.tempVoiceHub.findUnique({ where: { id: hubId }, select: { id: true, guildId: true, channelId: true } });
  if (!hub || hub.guildId !== guild.id) return false;
  const chk = await confirmChannel(guild, hub.channelId);
  if (chk.state !== 'gone') return false;
  if ((await prisma.tempVoiceChannel.count({ where: { hubId } })) > 0) return false;
  await prisma.tempVoiceHub.deleteMany({ where: { id: hubId } });
  invalidateIndex(guild.id);
  logger.info(`tempvoice: retired hub ${hubId} deleted (its channels are closed)`);
  return true;
}

/** Forgets a temp channel's row, jobs, limiter state and timers (caller holds `ch:` when it can). */
export async function purgeRow(guild, row) {
  // The voice channel is gone, so its companion text channel goes too. Callers that already deleted it leave nothing in the cache;
  // without this a purge triggered by a leave event, a panel click or the cleanup job (instead of channelDelete) orphans the chat.
  if (row.textChannelId && guild.channels.cache.has(row.textChannelId)) {
    await deleteDiscordChannel(guild, row.textChannelId, 'Temp voice: channel closed', 'delete text');
  }
  await prisma.tempVoiceChannel.deleteMany({ where: { channelId: row.channelId } });
  await cancelJobs({ dedupeKey: cleanupKey(row.channelId) });
  renameStamps.delete(row.channelId);
  const timer = textSyncTimers.get(row.channelId);
  if (timer) {
    clearTimeout(timer);
    textSyncTimers.delete(row.channelId);
  }
  await cache.del(renameKey(row.channelId));
  invalidateIndex(guild.id);
  try {
    await retireHubIfDone(guild, row.hubId, row.hub ? { id: row.hub.id, guildId: row.hub.guildId, channelId: row.hub.channelId } : null);
  } catch (err) {
    logger.warn(`tempvoice: could not check the hub of channel ${row.channelId}: ${err?.message ?? err}`);
  }
}

async function scheduleDeleteRetry(guildId, channelId) {
  try {
    await scheduleJob({
      guildId, type: CLEANUP_JOB, runAt: new Date(Date.now() + DELETE_RETRY_MS), payload: { channelId },
      dedupeKey: cleanupKey(channelId), maxAttempts: 5, onConflict: 'replace',
    });
  } catch (err) {
    logger.error(`tempvoice: could not schedule the cleanup of ${channelId}`, err);
  }
}

/**
 * Deletes a temporary channel (and its text channel) and forgets the row. Caller holds `ch:`.
 * The row is kept when Discord refuses (anything but 10003).
 * @returns {Promise<{ ok: boolean, code?: number|null }>}
 */
export async function deleteTempChannel(guild, row, reason) {
  if (!guild.available) return { ok: false, code: null };
  const r = await deleteDiscordChannel(guild, row.channelId, `Temp voice: ${reason}`, 'delete voice');
  if (!r.ok && r.code !== 10003) return { ok: false, code: r.code };
  if (row.textChannelId) await deleteDiscordChannel(guild, row.textChannelId, 'Temp voice: channel closed', 'delete text');
  await purgeRow(guild, row);
  return { ok: true };
}

// ── Companion text channel (spec §6.11) ───────────────────────────────────────

/** Debounced (1.5 s): bursts of joins and leaves coalesce into one overwrite PATCH. */
export function requestTextSync(guild, channelId) {
  if (textSyncTimers.has(channelId)) return;
  const timer = setTimeout(() => {
    textSyncTimers.delete(channelId);
    runTask('text-sync', () => locks.run(`ch:${channelId}`, () => syncTextOverwrites(guild, channelId)));
  }, TEXT_SYNC_DELAY_MS);
  timer.unref?.();
  textSyncTimers.set(channelId, timer);
}

/** Rewrites the companion text channel's overwrites from the voice channel's current members. Caller holds `ch:`. */
export async function syncTextOverwrites(guild, channelId) {
  const row = await loadRow(channelId);
  if (!row || !row.textChannelId || row.guildId !== guild.id) return;
  const vc = guild.channels.cache.get(channelId);
  if (!vc) return; // a cache miss is not a deletion; reconciliation / channelDelete decide
  let text = guild.channels.cache.get(row.textChannelId);
  if (!text) {
    const chk = await confirmChannel(guild, row.textChannelId);
    if (chk.state === 'gone') {
      await updateRow(channelId, { textChannelId: null, controlMessageId: null });
      invalidateIndex(guild.id);
    }
    if (chk.state !== 'present') return;
    text = chk.channel;
  }
  const settings = peekSettings(guild.id) ?? await getSettingsSafe(guild.id);
  const hubChannel = guild.channels.cache.get(row.hub.channelId);
  const template = snapshotOverwrites(row.hub.syncPermissions ? text.parent : hubChannel);
  const memberIds = [...vc.members.filter((m) => !m.user?.bot).keys()];
  await discordCall('text overwrites', () => text.permissionOverwrites.set(computeTextOverwrites({
    everyoneId: guild.id,
    botId: guild.members.me.id,
    ownerId: row.ownerId,
    template,
    membersOnly: row.hub.textChannelMembersOnly,
    memberIds,
    bannedUserIds: row.bannedUserIds,
    exemptRoleIds: exemptRoleIds(row.hub, settings, guild),
    grantable: grantableBits(guild, text.parent),
  })), guild.id);
}

// ── Voice overwrites (spec §6.10) ─────────────────────────────────────────────

/**
 * Rewrites the whole overwrite list of a temp channel from (template + state).
 * @param {{ hub: object, ownerId: string, locked: boolean, hidden: boolean, bannedUserIds: string[] }} state
 */
export async function applyVoiceOverwrites(guild, vc, state, settings, reason) {
  const hub = state.hub;
  const hubChannel = guild.channels.cache.get(hub.channelId);
  // A retired hub (its channel was deleted, its temporary channels are still open) has no template left to copy. Rewriting the whole
  // list from an empty one would silently drop what the hub restricted (e.g. a private hub's channels become visible to everyone).
  if (!hub.syncPermissions && !hubChannel) return { ok: false, code: null, hubGone: true };
  const parent = vc.parent ?? null;
  const template = snapshotOverwrites(hub.syncPermissions ? parent : hubChannel);
  return discordCall('overwrites', () => vc.permissionOverwrites.set(computeVoiceOverwrites({
    everyoneId: guild.id,
    botId: guild.members.me.id,
    ownerId: state.ownerId,
    template,
    locked: state.locked,
    hidden: state.hidden,
    bannedUserIds: state.bannedUserIds,
    exemptRoleIds: exemptRoleIds(hub, settings, guild),
    grantable: grantableBits(guild, parent),
  }), reason), guild.id);
}

// ── Control panel (spec §6.12) ────────────────────────────────────────────────

/** Posts the panel (and pins it in the companion text channel). Best effort; stores `controlMessageId`. Caller holds `ch:`. */
export async function postControlPanel(guild, row, voice, text) {
  const panelChannel = text ?? voice;
  if (!checkChannelUsable(panelChannel).ok) return;
  const settings = peekSettings(guild.id) ?? await getSettingsSafe(guild.id);
  const sent = await discordCall('post panel', () => panelChannel.send(buildControlPanel({ channelId: voice.id, ownerId: row.ownerId, settings })), guild.id);
  if (!sent.ok) return;
  await updateRow(voice.id, { controlMessageId: sent.value.id }).catch((err) => logger.warn(`tempvoice: could not store the panel message: ${err?.message ?? err}`));
  if (panelChannel === text) {
    const canPin = missingBotPermissions(text, ['PinMessages']).length === 0 || missingBotPermissions(text, ['ManageMessages']).length === 0;
    if (canPin) await discordCall('pin panel', () => sent.value.pin('Temp voice controls'), guild.id);
  }
}

/** Refreshes the pinned panel after an ownership change. Best effort. */
async function refreshControlPanel(guild, row, vc, newOwnerId, settings) {
  if (!row.controlMessageId) return;
  const panelChannel = (row.textChannelId ? guild.channels.cache.get(row.textChannelId) : null) ?? vc;
  const fetched = await discordCall('fetch panel', () => panelChannel.messages.fetch(row.controlMessageId), guild.id);
  if (!fetched.ok) {
    if (fetched.code === 10008) await updateRow(row.channelId, { controlMessageId: null }).catch(() => {});
    return;
  }
  const edited = await discordCall('edit panel', () => fetched.value.edit(buildControlPanel({ channelId: vc.id, ownerId: newOwnerId, settings })), guild.id);
  if (!edited.ok && edited.code === 10008) await updateRow(row.channelId, { controlMessageId: null }).catch(() => {});
}

// ── Hub join (spec §6.4) ──────────────────────────────────────────────────────

const stillInHub = (guild, member, hub) => guild.voiceStates.cache.get(member.id)?.channelId === hub.channelId;

/**
 * Creates the channel for one member. Caller holds `hub:{hubId}`. Returns what the panel needs, or null.
 * @returns {Promise<{ row: object, voice: object, text: object|null, settings: object|null }|null>}
 */
async function createForMember(guild, hubId, member, settings) {
  const hub = await prisma.tempVoiceHub.findUnique({ where: { id: hubId } });
  if (!hub || hub.guildId !== guild.id) return null;
  const hubChannel = guild.channels.cache.get(hub.channelId);
  if (!hubChannel) return null; // retired hub
  if (!stillInHub(guild, member, hub)) return null;

  // One channel per owner per hub: move the member back into it (the row and channel are checked under the channel lock).
  const existing = await prisma.tempVoiceChannel.findFirst({ where: { guildId: guild.id, hubId, ownerId: member.id } });
  if (existing) {
    const outcome = await locks.run(`ch:${existing.channelId}`, async () => {
      const row = await prisma.tempVoiceChannel.findUnique({ where: { channelId: existing.channelId } });
      if (!row) return 'gone';
      const chk = await confirmChannel(guild, row.channelId);
      if (chk.state === 'gone') {
        await purgeRow(guild, row);
        return 'gone';
      }
      if (chk.state !== 'present') return 'unknown';
      if (stillInHub(guild, member, hub)) {
        await discordCall('move back', () => member.voice.setChannel(chk.channel, 'Temp voice: back to own channel'), guild.id);
      }
      return 'present';
    });
    if (outcome !== 'gone') return null;
    // The row or its channel is gone: continue and create a new one.
  }

  // Creation cooldown (one retry when it ends).
  const key = `${guild.id}:${member.id}`;
  const nowMs = Date.now();
  const wait = CREATE_COOLDOWN_MS - (nowMs - (lastCreateAt.get(key) ?? 0));
  if (wait > 0) {
    if (!pendingRetries.has(key)) {
      pendingRetries.add(key);
      const timer = setTimeout(() => {
        pendingRetries.delete(key);
        runTask('hub-retry', () => queueHubJoin(guild, hubId, member, settings));
      }, wait);
      timer.unref?.();
    }
    return null;
  }

  const targetParent = (hub.categoryId ? guild.channels.cache.get(hub.categoryId) : null) ?? hubChannel.parent ?? null;
  const missing = [
    ...missingBotPermissions(targetParent ?? guild, REQUIRED_BOT_PERMS),
    ...missingBotPermissions(hubChannel, HUB_CHANNEL_PERMS),
  ];
  if (missing.length) {
    if (warnThrottle(`perms:${guild.id}`)) logger.warn(`tempvoice: missing ${[...new Set(missing)].join(', ')} in guild ${guild.id}; cannot create channels`);
    return null;
  }

  const rows = await prisma.tempVoiceChannel.findMany({ where: { hubId }, select: { index: true } });
  const index = lowestFreeIndex(rows.map((r) => r.index));
  const name = renderChannelName(hub.nameFormat, { member, guild, index });
  const grantable = grantableBits(guild, targetParent);
  const template = snapshotOverwrites(hub.syncPermissions ? targetParent : hubChannel);
  const exempt = exemptRoleIds(hub, settings, guild);
  const botId = guild.members.me.id;

  const created = await discordCall('create voice', () => guild.channels.create({
    name,
    type: ChannelType.GuildVoice,
    parent: targetParent?.id,
    userLimit: hub.userLimit,
    bitrate: effectiveBitrate(hub.bitrate, guild.maximumBitrate),
    permissionOverwrites: computeVoiceOverwrites({
      everyoneId: guild.id, botId, ownerId: member.id, template, locked: false, hidden: false,
      bannedUserIds: [], exemptRoleIds: exempt, grantable,
    }),
    reason: clampText(`Temporary channel for ${member.user.tag}`, 512),
  }), guild.id);
  if (!created.ok) {
    if (warnThrottle(`create:${guild.id}`)) {
      logger.warn(`tempvoice: could not create a channel in guild ${guild.id} (${describeDiscordError(created.code)})`);
    }
    return null;
  }
  const voice = created.value;
  lastCreateAt.set(key, Date.now());
  for (const [k, t] of lastCreateAt) if (Date.now() - t > CREATE_PRUNE_MS) lastCreateAt.delete(k);

  let text = null;
  if (hub.createTextChannel) {
    const t = await discordCall('create text', () => guild.channels.create({
      name: textChannelName(name, index),
      type: ChannelType.GuildText,
      parent: targetParent?.id,
      topic: clampText(`Text chat for 🔊 ${name}`, 1024),
      permissionOverwrites: computeTextOverwrites({
        everyoneId: guild.id, botId, ownerId: member.id, template, membersOnly: hub.textChannelMembersOnly,
        memberIds: [member.id], bannedUserIds: [], exemptRoleIds: exempt, grantable,
      }),
      reason: 'Temporary text channel',
    }), guild.id);
    if (t.ok) text = t.value;
  }

  let row;
  try {
    await ensureGuildSettings(guild.id);
    row = await prisma.tempVoiceChannel.create({
      data: { guildId: guild.id, hubId, channelId: voice.id, textChannelId: text?.id ?? null, ownerId: member.id, index },
    });
  } catch (err) {
    logger.error(`tempvoice: could not save the channel ${voice.id}`, err);
    await deleteDiscordChannel(guild, voice.id, 'Temp voice: could not save the channel', 'rollback voice');
    if (text) await deleteDiscordChannel(guild, text.id, 'Temp voice: could not save the channel', 'rollback text');
    return null;
  }
  invalidateIndex(guild.id);

  const moved = stillInHub(guild, member, hub)
    ? await discordCall('move', () => member.voice.setChannel(voice, 'Temp voice: created'), guild.id)
    : { ok: false, code: null };
  if (!moved.ok) {
    // The member left meanwhile (or Discord refused the move): the channel would stay empty forever.
    const closed = await locks.run(`ch:${voice.id}`, () => deleteTempChannel(guild, row, 'creator left'));
    if (!closed.ok) await scheduleDeleteRetry(guild.id, voice.id);
    return null;
  }
  return { row, voice, text, settings };
}

/** Serialises creation per hub; posts the control panel after the hub lock is released. */
export async function queueHubJoin(guild, hubId, member, settings) {
  const made = await locks.run(`hub:${hubId}`, () => createForMember(guild, hubId, member, settings));
  if (!made) return;
  await locks.run(`ch:${made.voice.id}`, async () => {
    // The channel may already be gone (the creator left at once): then there is nothing to post into.
    if (!guild.channels.cache.has(made.voice.id)) return;
    await postControlPanel(guild, made.row, made.voice, made.text);
  });
}

// ── Joining and leaving a temp channel (spec §6.5, §6.6) ──────────────────────

/** Caller holds `ch:{channelId}`. */
export async function onJoinTemp(guild, channelId, member) {
  const row = await loadRow(channelId);
  if (!row || row.guildId !== guild.id) return;
  const chk = await confirmChannel(guild, channelId);
  if (chk.state === 'gone') return purgeRow(guild, row);
  if (chk.state !== 'present') return undefined;

  const data = {};
  if (row.emptySince) {
    data.emptySince = null;
    await cancelJobs({ dedupeKey: cleanupKey(channelId) });
  }
  if (member.id === row.ownerId && row.ownerLeftAt) data.ownerLeftAt = null;

  if (row.bannedUserIds.includes(member.id)) {
    const settings = await getSettingsSafe(guild.id);
    if (!isProtectedTarget(member, row.hub, settings)) {
      // Discord already denies Connect; this covers members moved in by someone with Move Members.
      await discordCall('kick banned', () => member.voice.disconnect('Temp voice: banned from this channel'), guild.id);
    }
  }
  if (Object.keys(data).length) await updateRow(channelId, data);
  if (row.textChannelId && row.hub.textChannelMembersOnly) requestTextSync(guild, channelId);
  return undefined;
}

/** Caller holds `ch:{channelId}`. */
export async function onLeaveTemp(guild, channelId, member) {
  const row = await loadRow(channelId);
  if (!row || row.guildId !== guild.id) return;
  const chk = await confirmChannel(guild, channelId);
  if (chk.state === 'gone') return purgeRow(guild, row);
  if (chk.state !== 'present') return undefined;
  const channel = chk.channel;

  const humans = countHumans(channel); // discord.js updates its voice-state cache before the event, so the leaver is not counted
  const isOwner = member.id === row.ownerId;
  const now = new Date();

  if (humans === 0) {
    if (row.hub.keepAliveMinutes === 0) {
      const r = await deleteTempChannel(guild, row, 'empty');
      if (!r.ok) await scheduleDeleteRetry(guild.id, channelId);
      return undefined;
    }
    await updateRow(channelId, { emptySince: now, ownerLeftAt: isOwner ? (row.ownerLeftAt ?? now) : row.ownerLeftAt });
    try {
      await scheduleJob({
        guildId: guild.id, type: CLEANUP_JOB, runAt: new Date(now.getTime() + row.hub.keepAliveMinutes * 60_000),
        payload: { channelId }, dedupeKey: cleanupKey(channelId), maxAttempts: 5, onConflict: 'replace',
      });
    } catch (err) {
      logger.error(`tempvoice: could not schedule the keep-alive of ${channelId}; reconciliation will repair it`, err);
    }
    return undefined;
  }
  if (isOwner && !row.ownerLeftAt) await updateRow(channelId, { ownerLeftAt: now });
  if (row.textChannelId && row.hub.textChannelMembersOnly) requestTextSync(guild, channelId);
  return undefined;
}

// ── Job tempvoice.cleanup (spec §6.8) ─────────────────────────────────────────

/** Deletes a temp channel that stayed empty for keepAliveMinutes. Idempotent; never checks the module toggle. */
export async function handleCleanupJob(job, ctx) {
  const channelId = job?.payload?.channelId;
  if (typeof channelId !== 'string' || !SNOWFLAKE_RE.test(channelId)) return undefined;
  return locks.run(`ch:${channelId}`, async () => {
    const row = await loadRow(channelId);
    if (!row || row.guildId !== job.guildId) return undefined;
    const chk = await confirmChannel(ctx.guild, channelId);
    if (chk.state === 'gone') {
      await purgeRow(ctx.guild, row);
      return undefined;
    }
    if (chk.state !== 'present') return { nextRunAt: new Date(Date.now() + DELETE_RETRY_MS) }; // transient: look again later
    if (countHumans(chk.channel) > 0) {
      await updateRow(channelId, { emptySince: null }); // someone is back; their join normally cancels the job first
      return undefined;
    }
    const r = await deleteTempChannel(ctx.guild, row, 'keep-alive expired');
    if (!r.ok) throw new Error(`delete failed (${r.code ?? 'unknown'})`); // the scheduler retries with backoff
    return undefined;
  });
}

// ── Voice actions (spec §6.9) ─────────────────────────────────────────────────

const ACTION_PERMS = Object.freeze({
  lock: ['ManageChannels', 'ManageRoles'], unlock: ['ManageChannels', 'ManageRoles'],
  hide: ['ManageChannels', 'ManageRoles'], reveal: ['ManageChannels', 'ManageRoles'],
  ban: ['ManageChannels', 'ManageRoles'], unban: ['ManageChannels', 'ManageRoles'],
  transfer: ['ManageChannels', 'ManageRoles'], claim: ['ManageChannels', 'ManageRoles'],
  kick: ['MoveMembers'], limit: ['ManageChannels'], rename: ['ManageChannels'],
});

const failResult = (key, params) => ({ ok: false, embed: MSG[key](params) });

/**
 * Runs one /voice action or panel click. Serialised per channel; never throws.
 * @param {{ guild: object, actor: object, channelId: string, action: string, targetId?: string|null, value?: number|string|null,
 *   settings?: object|null, source?: 'command'|'button'|'select'|'modal' }} req
 * @returns {Promise<{ ok: boolean, embed: object }>}
 */
export async function runVoiceAction(req) {
  try {
    return await locks.run(`ch:${req.channelId}`, () => runVoiceActionLocked(req));
  } catch (err) {
    logger.error(`tempvoice ${req.action} failed`, err);
    return failResult('INTERNAL');
  }
}

async function recordRename(channelId, recent) {
  // Strictly increasing: renameChannel merges these with the Redis copy through a Set, which would fold two renames
  // stamped in the same millisecond into one and let a third through.
  const last = recent.length ? recent[recent.length - 1] : 0;
  const next = [...recent, Math.max(Date.now(), last + 1)];
  renameStamps.set(channelId, next);
  while (renameStamps.size > RENAME_STAMPS_MAX) renameStamps.delete(renameStamps.keys().next().value);
  await cache.set(renameKey(channelId), next, 600); // fail-open: Redis down keeps only the in-memory stamps
}

async function renameChannel({ guild, vc, row, value, reason }) {
  const name = normalizeChannelName(value);
  if (name === null) return failResult('RENAME_INVALID');
  if (name === vc.name) return failResult('NO_CHANGE_NAME');
  const fromRedis = await cache.get(renameKey(row.channelId));
  const stamps = [...new Set([...(renameStamps.get(row.channelId) ?? []), ...(Array.isArray(fromRedis) ? fromRedis : [])])];
  const w = checkRenameWindow(stamps, Date.now());
  if (!w.ok) return failResult('RENAME_RATE', { retryAt: w.retryAt });
  try {
    await withTimeout(vc.setName(name, reason), RENAME_TIMEOUT_MS, 'tempvoice rename');
  } catch (err) {
    if (err instanceof TimeoutError) {
      // discord.js is waiting out Discord's own rate limit; the call keeps running and applies the name later.
      await recordRename(row.channelId, w.recent);
      return failResult('RENAME_QUEUED');
    }
    logger.warn(`tempvoice rename failed in guild ${guild.id}: ${err?.message ?? err}`);
    return failResult('DISCORD_ERROR', { code: discordErrorCode(err) });
  }
  await recordRename(row.channelId, w.recent);
  return { ok: true, embed: okEmbed('rename', { newName: name }) };
}

/** Ownership change: overwrites, row, text visibility, panel refresh and a public notice. Caller holds `ch:`. */
async function changeOwner(guild, vc, row, newOwnerId, settings, reason) {
  const bannedUserIds = row.bannedUserIds.filter((id) => id !== newOwnerId);
  const applied = await applyVoiceOverwrites(guild, vc, { ...row, ownerId: newOwnerId, bannedUserIds }, settings, reason);
  if (!applied.ok) return { ok: false, key: applied.hubGone ? 'HUB_GONE' : 'DISCORD_ERROR', params: { code: applied.code } };
  try {
    await prisma.tempVoiceChannel.update({ where: { channelId: row.channelId }, data: { ownerId: newOwnerId, ownerLeftAt: null, bannedUserIds } });
  } catch (err) {
    logger.error(`tempvoice: could not save the new owner of ${row.channelId}`, err);
    return { ok: false, key: 'DB_ERROR' };
  }
  if (row.textChannelId) requestTextSync(guild, row.channelId);
  await refreshControlPanel(guild, row, vc, newOwnerId, settings);
  const panelChannel = (row.textChannelId ? guild.channels.cache.get(row.textChannelId) : null) ?? vc;
  if (checkChannelUsable(panelChannel).ok) {
    await discordCall('ownership notice', () => panelChannel.send({
      content: `👑 <@${newOwnerId}> is now the owner of this channel.`, allowedMentions: { parse: [] },
    }), guild.id);
  }
  return { ok: true };
}

async function runVoiceActionLocked({ guild, actor, channelId, action, targetId = null, value = null, settings = null, source = 'command' }) {
  if (!VOICE_ACTIONS.includes(action)) return failResult('INTERNAL');
  const row = await loadRow(channelId);
  if (!row || row.guildId !== guild.id) return failResult(source === 'command' ? 'NOT_IN_TEMP' : 'CHANNEL_GONE');
  const chk = await confirmChannel(guild, channelId);
  if (chk.state === 'gone') {
    await purgeRow(guild, row);
    return failResult('CHANNEL_GONE');
  }
  if (chk.state !== 'present') return failResult('DISCORD_ERROR', { code: chk.code ?? null });
  const vc = chk.channel;

  const isOwner = actor.id === row.ownerId;
  const isMod = isTempVoiceMod(actor, row.hub, settings);
  if (!canControl({ action, isOwner, isMod })) return failResult('NOT_ALLOWED');
  const missing = missingBotPermissions(vc, ACTION_PERMS[action]);
  if (missing.length) return failResult('BOT_MISSING', { names: permissionNames(missing), channelId });

  const reason = clampText(`Temp voice: ${action} by ${actor.user.tag}`, 512);
  const name = vc.name;

  /** Applies a state patch to the overwrites, then saves it. Returns an error result or null. */
  const applyAndSave = async (patch) => {
    const applied = await applyVoiceOverwrites(guild, vc, { ...row, ...patch }, settings, reason);
    if (!applied.ok) return failResult(applied.hubGone ? 'HUB_GONE' : 'DISCORD_ERROR', { code: applied.code });
    try {
      await prisma.tempVoiceChannel.update({ where: { channelId }, data: patch });
    } catch (err) {
      logger.error(`tempvoice: could not save ${action} for ${channelId}`, err);
      return failResult('DB_ERROR');
    }
    return null;
  };
  /** Fetches the member a kick / ban / transfer is aimed at, or returns the error result. */
  const fetchTarget = async () => {
    const target = await guild.members.fetch(targetId).catch(() => null);
    return target ? { target } : { error: failResult('TARGET_NOT_MEMBER') };
  };
  const refuse = (problem) => failResult(problem, { target: targetId, owner: row.ownerId });

  switch (action) {
    case 'lock':
    case 'unlock':
    case 'hide':
    case 'reveal': {
      const field = action === 'lock' || action === 'unlock' ? 'locked' : 'hidden';
      const next = action === 'lock' || action === 'hide';
      if (row[field] === next) return failResult(`NO_CHANGE_${action.toUpperCase()}`);
      const err = await applyAndSave({ [field]: next });
      return err ?? { ok: true, embed: okEmbed(action, { name }) };
    }
    case 'limit': {
      const n = value;
      if (!Number.isInteger(n) || n < 0 || n > 99) return failResult('LIMIT_INVALID');
      if (vc.userLimit === n) return failResult('NO_CHANGE_LIMIT', { n });
      const r = await discordCall('set limit', () => vc.setUserLimit(n, reason), guild.id);
      if (!r.ok) return failResult('DISCORD_ERROR', { code: r.code });
      return { ok: true, embed: okEmbed('limit', { name, n }) };
    }
    case 'rename':
      return renameChannel({ guild, vc, row, value, reason });
    case 'kick': {
      const { target, error } = await fetchTarget();
      if (error) return error;
      const problem = targetProblem({
        action, actorId: actor.id, targetId, ownerId: row.ownerId,
        targetProtected: isProtectedTarget(target, row.hub, settings), targetConnected: target.voice?.channelId === channelId,
      });
      if (problem) return refuse(problem);
      const r = await discordCall('kick', () => target.voice.disconnect(reason), guild.id);
      if (!r.ok) return r.code === 40032 ? refuse('TARGET_NOT_CONNECTED') : failResult('DISCORD_ERROR', { code: r.code });
      return { ok: true, embed: okEmbed('kick', { name, target: targetId }) };
    }
    case 'ban': {
      const { target, error } = await fetchTarget();
      if (error) return error;
      const problem = targetProblem({
        action, actorId: actor.id, targetId, ownerId: row.ownerId,
        targetProtected: isProtectedTarget(target, row.hub, settings),
        isBanned: row.bannedUserIds.includes(targetId), banCount: row.bannedUserIds.length,
      });
      if (problem) return refuse(problem);
      const err = await applyAndSave({ bannedUserIds: [...row.bannedUserIds, targetId] });
      if (err) return err;
      if (target.voice?.channelId === channelId && missingBotPermissions(vc, ['MoveMembers']).length === 0) {
        await discordCall('kick banned', () => target.voice.disconnect(reason), guild.id);
      }
      if (row.textChannelId) requestTextSync(guild, channelId);
      return { ok: true, embed: okEmbed('ban', { name, target: targetId }) };
    }
    case 'unban': {
      // No member fetch: the user may have left the server.
      const problem = targetProblem({ action, actorId: actor.id, targetId, ownerId: row.ownerId, isBanned: row.bannedUserIds.includes(targetId) });
      if (problem) return refuse(problem);
      const err = await applyAndSave({ bannedUserIds: row.bannedUserIds.filter((id) => id !== targetId) });
      if (err) return err;
      if (row.textChannelId) requestTextSync(guild, channelId);
      return { ok: true, embed: okEmbed('unban', { name, target: targetId }) };
    }
    case 'transfer': {
      const { target, error } = await fetchTarget();
      if (error) return error;
      const problem = targetProblem({
        action, actorId: actor.id, targetId, ownerId: row.ownerId,
        targetIsBot: target.user?.bot === true, targetConnected: target.voice?.channelId === channelId,
      });
      if (problem) return refuse(problem);
      const r = await changeOwner(guild, vc, row, target.id, settings, reason);
      return r.ok ? { ok: true, embed: okEmbed('transfer', { name, target: target.id }) } : failResult(r.key, r.params);
    }
    case 'claim': {
      const ownerConnected = vc.members.has(row.ownerId);
      let ownerLeftAt = row.ownerLeftAt;
      if (!ownerConnected && !ownerLeftAt) {
        // The owner left while the bot was not watching: the ownership-lock timer starts now.
        ownerLeftAt = new Date();
        await updateRow(channelId, { ownerLeftAt }).catch((err) => logger.warn(`tempvoice: could not store ownerLeftAt: ${err?.message ?? err}`));
      }
      const st = claimStatus({
        isOwner, actorConnected: actor.voice?.channelId === channelId, ownerConnected, ownerLeftAt,
        lockMinutes: row.hub.ownershipLockMinutes, isMod, now: Date.now(),
      });
      if (!st.ok) {
        switch (st.code) {
          case 'ALREADY_OWNER': return failResult('CLAIM_ALREADY_OWNER');
          case 'NOT_CONNECTED': return failResult('CLAIM_NOT_CONNECTED');
          case 'OWNER_PRESENT': return failResult('CLAIM_OWNER_PRESENT', { owner: row.ownerId });
          default: return failResult('CLAIM_LOCKED', { at: st.claimableAt });
        }
      }
      const r = await changeOwner(guild, vc, row, actor.id, settings, reason);
      return r.ok ? { ok: true, embed: okEmbed('claim', { name }) } : failResult(r.key, r.params);
    }
    default:
      return failResult('INTERNAL');
  }
}

// ── Hooks: voiceStateUpdate, channelDelete, guildReady (spec §5, §6.4, §6.14, §6.15) ──

/** Sync, cheap: skips mute/deafen/stream changes and, with a fresh memo, every event that touches no hub or temp channel. */
export function voiceFilter(ctx) {
  if (ctx?.change === 'state') return false;
  const idx = peekIndex(ctx?.guild?.id);
  if (!idx) return true;
  const oldId = ctx?.oldState?.channelId ?? null;
  const newId = ctx?.newState?.channelId ?? null;
  return Boolean((newId && idx.hubs.has(newId)) || (oldId && idx.temps.has(oldId)) || (newId && idx.temps.has(newId)));
}

/** Returns at once; the work is queued (`runTask`) so the other voice hooks are never delayed. */
export function onVoiceStateUpdate(ctx) {
  const { guild, member, oldState, newState } = ctx;
  const oldId = oldState?.channelId ?? null;
  const newId = newState?.channelId ?? null;
  if (oldId === newId) return;
  runTask('voice', async () => {
    // A fresh memo is used without awaiting, so consecutive events of one channel reach `ch:` in gateway order.
    const idx = peekIndex(guild.id) ?? await loadIndex(guild.id);
    if (!idx) return;
    if (oldId && idx.temps.has(oldId)) await locks.run(`ch:${oldId}`, () => onLeaveTemp(guild, oldId, member));
    if (newId && idx.temps.has(newId)) await locks.run(`ch:${newId}`, () => onJoinTemp(guild, newId, member));
    if (newId && idx.hubs.has(newId)) {
      const settings = await ctx.getSettings();
      if (isModuleEnabled(settings, 'tempvoice')) await queueHubJoin(guild, idx.hubs.get(newId), member, settings);
    }
  });
}

/** Sync filter: with a fresh memo only deletions of a hub, temp channel, companion text channel or hub category matter. */
export function channelDeleteFilter(ctx) {
  const id = ctx?.channel?.id;
  const idx = peekIndex(ctx?.guild?.id);
  if (!idx) return true;
  return Boolean(id && (idx.hubs.has(id) || idx.temps.has(id) || idx.texts.has(id) || idx.categories.has(id)));
}

/** A delete event is authoritative: no REST confirmation is needed here. */
export function onChannelDelete(ctx) {
  const { guild, channel } = ctx;
  const id = channel?.id;
  if (!id) return;
  runTask('channelDelete', async () => {
    const idx = await loadIndex(guild.id);
    if (!idx) return;
    if (idx.temps.has(id)) {
      await locks.run(`ch:${id}`, async () => {
        const row = await loadRow(id);
        if (!row) return;
        if (row.textChannelId) await deleteDiscordChannel(guild, row.textChannelId, 'Temp voice: channel closed', 'delete text');
        await purgeRow(guild, row);
      });
    }
    if (idx.texts.has(id)) {
      const voiceId = idx.texts.get(id);
      await locks.run(`ch:${voiceId}`, async () => {
        await prisma.tempVoiceChannel.updateMany({ where: { channelId: voiceId, textChannelId: id }, data: { textChannelId: null, controlMessageId: null } });
        invalidateIndex(guild.id);
      });
    }
    if (idx.hubs.has(id)) {
      const hubId = idx.hubs.get(id);
      const count = await prisma.tempVoiceChannel.count({ where: { hubId } });
      if (count === 0) await prisma.tempVoiceHub.deleteMany({ where: { id: hubId } });
      else logger.info(`tempvoice: hub ${hubId} retired (${count} channel(s) still open)`);
      invalidateIndex(guild.id);
    }
    if (idx.categories.has(id)) {
      await prisma.tempVoiceHub.updateMany({ where: { guildId: guild.id, categoryId: id }, data: { categoryId: null } });
      invalidateIndex(guild.id);
    }
  });
}

/** Queues the startup / join / availability reconciliation of one guild. */
export function onGuildReady(ctx) {
  runTask('reconcile', () => reconcileGuild(ctx.guild, ctx));
}

// ── Reconciliation (spec §6.15) ───────────────────────────────────────────────

async function reconcileRow(guild, rowIn, settings, now) {
  const row = await loadRow(rowIn.channelId);
  if (!row) return;
  const chk = await confirmChannel(guild, row.channelId);
  if (chk.state === 'unknown') return; // rule 20: keep the row
  const present = chk.state === 'present';
  const decision = reconcileDecision({
    channelExists: present, humans: present ? countHumans(chk.channel) : 0,
    keepAliveMinutes: row.hub.keepAliveMinutes, emptySince: row.emptySince, now,
  });

  switch (decision.action) {
    case 'purge':
      if (row.textChannelId) await deleteDiscordChannel(guild, row.textChannelId, 'Temp voice: channel closed', 'delete text');
      await purgeRow(guild, row);
      break;
    case 'delete': {
      const r = await deleteTempChannel(guild, row, 'empty after restart');
      if (!r.ok) await scheduleDeleteRetry(guild.id, row.channelId);
      break;
    }
    case 'schedule':
      if (decision.setEmptySince) await updateRow(row.channelId, { emptySince: new Date(now) });
      await scheduleJob({
        guildId: guild.id, type: CLEANUP_JOB, runAt: new Date(decision.runAt), payload: { channelId: row.channelId },
        dedupeKey: cleanupKey(row.channelId), maxAttempts: 5, onConflict: 'keep',
      });
      break;
    default: { // occupied
      const data = {};
      if (row.emptySince) {
        data.emptySince = null;
        await cancelJobs({ dedupeKey: cleanupKey(row.channelId) });
      }
      const ownerHere = chk.channel.members.has(row.ownerId);
      if (!ownerHere && !row.ownerLeftAt) data.ownerLeftAt = new Date(now);
      if (ownerHere && row.ownerLeftAt) data.ownerLeftAt = null;
      let textPresent = false;
      if (row.textChannelId) {
        const tc = await confirmChannel(guild, row.textChannelId);
        if (tc.state === 'gone') {
          data.textChannelId = null;
          data.controlMessageId = null;
        } else textPresent = tc.state === 'present';
      }
      if (Object.keys(data).length) await updateRow(row.channelId, data);
      if (textPresent && row.hub.textChannelMembersOnly) requestTextSync(guild, row.channelId);
    }
  }
}

/** Repairs rows after a restart or join, and creates channels for members already waiting in a hub. Never throws. */
export async function reconcileGuild(guild, ctx = null) {
  if (!guild?.available) return;
  let hubs;
  let rows;
  try {
    [hubs, rows] = await Promise.all([
      prisma.tempVoiceHub.findMany({ where: { guildId: guild.id } }),
      prisma.tempVoiceChannel.findMany({ where: { guildId: guild.id }, include: { hub: true } }),
    ]);
  } catch (err) {
    logger.error(`tempvoice: reconcile could not read guild ${guild.id}`, err);
    return;
  }
  if (!hubs.length && !rows.length) {
    invalidateIndex(guild.id);
    return;
  }
  const settings = ctx?.getSettings ? await ctx.getSettings() : await getSettingsSafe(guild.id);
  const now = Date.now();

  const results = await mapLimit(rows, 2, (row) => locks.run(`ch:${row.channelId}`, () => reconcileRow(guild, row, settings, now)));
  for (const r of results) if (r.status === 'rejected') logger.error('tempvoice: reconcile of a channel failed', r.reason);

  for (const hub of hubs) {
    try {
      if (await retireHubIfDone(guild, hub.id, hub)) continue;
      if (hub.categoryId && !guild.channels.cache.has(hub.categoryId)) {
        const c = await confirmChannel(guild, hub.categoryId);
        if (c.state === 'gone') {
          await prisma.tempVoiceHub.updateMany({ where: { id: hub.id }, data: { categoryId: null } });
          invalidateIndex(guild.id);
        }
      }
    } catch (err) {
      logger.error(`tempvoice: reconcile of hub ${hub.id} failed`, err);
    }
  }
  invalidateIndex(guild.id);

  // Members already waiting in a hub (they joined while the bot was offline).
  if (!isModuleEnabled(settings, 'tempvoice')) return;
  try {
    const live = await prisma.tempVoiceHub.findMany({ where: { guildId: guild.id } });
    for (const hub of live) {
      const hubChannel = guild.channels.cache.get(hub.channelId);
      if (!hubChannel?.members) continue;
      for (const member of [...hubChannel.members.values()]) {
        if (!member.user?.bot) await queueHubJoin(guild, hub.id, member, settings);
      }
    }
  } catch (err) {
    logger.error(`tempvoice: could not serve the members waiting in hubs of guild ${guild.id}`, err);
  }
}

// ── /voice clean (spec §6.16) ─────────────────────────────────────────────────

/** Deletes every temp channel of the server that has no human members. Moderators only. @returns {Promise<object>} an embed */
export async function cleanEmptyChannels({ guild, actor, settings }) {
  const hubs = await prisma.tempVoiceHub.findMany({ where: { guildId: guild.id }, select: { moderatorRoleIds: true } });
  const allowed = resolvePermissionLevel(actor, settings) >= PERMISSION_LEVELS.MODERATOR
    || hubs.some((h) => hasAnyRole(actor, h.moderatorRoleIds));
  if (!allowed) return MSG.CLEAN_NOT_ALLOWED();

  const rows = await prisma.tempVoiceChannel.findMany({ where: { guildId: guild.id }, include: { hub: true } });
  const results = await mapLimit(rows, 2, (row) => locks.run(`ch:${row.channelId}`, async () => {
    const fresh = await loadRow(row.channelId);
    if (!fresh) return false;
    const chk = await confirmChannel(guild, fresh.channelId);
    if (chk.state === 'gone') {
      await purgeRow(guild, fresh); // not counted
      return false;
    }
    if (chk.state !== 'present' || countHumans(chk.channel) > 0) return false;
    return (await deleteTempChannel(guild, fresh, 'cleaned up by a moderator')).ok;
  }));
  const n = results.filter((r) => r.status === 'fulfilled' && r.value === true).length;
  return okEmbed('clean', { n });
}

// ── Component handlers (spec §6.12) ───────────────────────────────────────────

const IMMEDIATE_ACTIONS = new Set(['lock', 'unlock', 'hide', 'reveal', 'claim']);

const ephemeralText = (interaction, content) => safeReply(interaction, { content, ephemeral: true });
const ephemeralEmbed = (interaction, embed) => safeReply(interaction, { embeds: [embed], ephemeral: true });
const settingsFor = async (guildId) => peekSettings(guildId) ?? await getSettingsSafe(guildId);

/**
 * Read-only check before a modal or picker is shown (showModal must be the first response, so nothing is deferred).
 * @returns {Promise<{ ok: true, row: object, vc: object } | { ok: false, embed: object }>}
 */
export async function checkControl({ guild, actor, channelId, action, settings }) {
  const row = await loadRow(channelId);
  if (!row || row.guildId !== guild.id) return { ok: false, embed: MSG.CHANNEL_GONE() };
  const vc = guild.channels.cache.get(channelId);
  if (!vc) return { ok: false, embed: MSG.CHANNEL_GONE() }; // no purge: a cache miss is not a deletion
  if (!canControl({ action, isOwner: actor.id === row.ownerId, isMod: isTempVoiceMod(actor, row.hub, settings) })) {
    return { ok: false, embed: MSG.NOT_ALLOWED() };
  }
  return { ok: true, row, vc };
}

async function replyInternal(interaction, label, err) {
  logger.error(`tempvoice ${label} failed`, err);
  await ephemeralEmbed(interaction, MSG.INTERNAL());
}

export async function handleButton(interaction) {
  try {
    if (!interaction.inCachedGuild?.()) return await ephemeralEmbed(interaction, MSG.NOT_IN_GUILD());
    const { params: [action, channelId] } = parseCustomId(interaction.customId);
    if (!BUTTON_ACTIONS.includes(action) || !SNOWFLAKE_RE.test(channelId ?? '')) return await ephemeralText(interaction, 'This button is no longer active.');
    const { guild, guildId } = interaction;

    if (IMMEDIATE_ACTIONS.has(action)) {
      if (!(await safeDefer(interaction, { ephemeral: true }))) return undefined;
      const settings = await settingsFor(guildId);
      const { embed } = await runVoiceAction({ guild, actor: interaction.member, channelId, action, settings, source: 'button' });
      return await safeReply(interaction, { embeds: [embed] });
    }

    // limit / rename / kick / ban / transfer: showModal and reply must be the FIRST response, so the lookups are bounded (rule 21).
    const settings = peekSettings(guildId)
      ?? await withTimeout(getSettingsSafe(guildId), ackBudgetMs(interaction), 'voice settings').catch(() => null);
    const budget = ackBudgetMs(interaction);
    if (budget <= 0) return await ephemeralEmbed(interaction, MSG.TRY_AGAIN());
    let pre;
    try {
      pre = await withTimeout(checkControl({ guild, actor: interaction.member, channelId, action, settings }), budget, 'voice precheck');
    } catch (err) {
      if (err instanceof TimeoutError) return await ephemeralEmbed(interaction, MSG.TRY_AGAIN());
      throw err;
    }
    if (!pre.ok) return await ephemeralEmbed(interaction, pre.embed);
    try {
      if (action === 'limit' || action === 'rename') {
        await interaction.showModal(buildValueModal(action, channelId, action === 'limit' ? pre.vc.userLimit : pre.vc.name));
      } else {
        await interaction.reply(buildMemberPicker(action, channelId, pre.row.bannedUserIds));
      }
    } catch (err) {
      if (!isInteractionGone(err)) throw err;
    }
    return undefined;
  } catch (err) {
    return replyInternal(interaction, 'button', err);
  }
}

export async function handleSelect(interaction) {
  try {
    if (!interaction.inCachedGuild?.()) return await ephemeralEmbed(interaction, MSG.NOT_IN_GUILD());
    const { params: [action, channelId] } = parseCustomId(interaction.customId);
    if (!SELECT_ACTIONS.includes(action) || !SNOWFLAKE_RE.test(channelId ?? '')) return await ephemeralText(interaction, 'This menu is no longer active.');
    try {
      await interaction.deferUpdate();
    } catch (err) {
      if (isInteractionGone(err)) return undefined;
      throw err;
    }
    const settings = await settingsFor(interaction.guildId);
    const { embed } = await runVoiceAction({
      guild: interaction.guild, actor: interaction.member, channelId, action, targetId: interaction.values?.[0] ?? null, settings, source: 'select',
    });
    await interaction.editReply({ content: '', embeds: [embed], components: [] }).catch((err) => {
      if (!isInteractionGone(err)) logger.warn(`tempvoice: could not update the picker: ${err?.message ?? err}`);
    });
    return undefined;
  } catch (err) {
    return replyInternal(interaction, 'select', err);
  }
}

export async function handleModal(interaction) {
  try {
    if (!interaction.inCachedGuild?.()) return await ephemeralEmbed(interaction, MSG.NOT_IN_GUILD());
    const { params: [action, channelId] } = parseCustomId(interaction.customId);
    if (!MODAL_ACTIONS.includes(action) || !SNOWFLAKE_RE.test(channelId ?? '')) {
      return await ephemeralText(interaction, 'This form has expired. Please run the command again.');
    }
    const raw = interaction.fields.getTextInputValue(buildCustomId('voice-value'));
    let value = raw;
    if (action === 'limit') {
      value = parseLimitInput(raw);
      if (value === null) return await ephemeralEmbed(interaction, MSG.LIMIT_INVALID());
    }
    if (!(await safeDefer(interaction, { ephemeral: true }))) return undefined;
    const settings = await settingsFor(interaction.guildId);
    const { embed } = await runVoiceAction({
      guild: interaction.guild, actor: interaction.member, channelId, action, value, settings, source: 'modal',
    });
    return await safeReply(interaction, { embeds: [embed] });
  } catch (err) {
    return replyInternal(interaction, 'modal', err);
  }
}

// ── /voice-hub: summaries and problems (spec §4.2.7) ──────────────────────────

const plural = (n, one) => `${n} ${one}`;
const rolesText = (ids) => (ids?.length ? joinWithLimit(ids.map((id) => `<@&${id}>`), { max: 300 }) : 'none');

/** The multi-line settings summary of a hub (used by create and list). */
export function hubSummary(hub, guild) {
  const hubChannel = guild?.channels?.cache?.get(hub.channelId);
  const where = hub.categoryId ? `<#${hub.categoryId}>` : (hubChannel?.parent ? "the hub's category" : 'top level');
  const textMode = !hub.createTextChannel ? 'off' : (hub.textChannelMembersOnly ? 'private' : 'public');
  return [
    `Hub: <#${hub.channelId}> · New channels in: ${where}`,
    `Name: \`${String(hub.nameFormat).replaceAll('`', 'ˋ')}\``,
    `Limit: ${hub.userLimit > 0 ? hub.userLimit : 'none'} · Bitrate: ${hub.bitrate ? `${Math.round(hub.bitrate / 1000)} kbps` : 'default'}`
      + ` · Keep alive: ${hub.keepAliveMinutes > 0 ? plural(hub.keepAliveMinutes, 'min') : 'delete at once'}`,
    `Ownership lock: ${hub.ownershipLockMinutes > 0 ? plural(hub.ownershipLockMinutes, 'min') : 'none'}`
      + ` · Permissions: ${hub.syncPermissions ? 'copied from category' : 'copied from hub channel'}`,
    `Text channel: ${textMode}`,
    `Mod roles: ${rolesText(hub.moderatorRoleIds)} · Ignored roles: ${rolesText(hub.ignoredRoleIds)}`,
  ].join('\n');
}

/** What currently stops a hub from working (display only: nothing is deleted because of it). */
export function hubProblems(hub, guild) {
  const problems = [];
  const hubChannel = guild.channels.cache.get(hub.channelId);
  if (!hubChannel) {
    problems.push('Hub channel deleted — this hub stops creating channels and disappears once its channels close. Use /voice-hub create to set up a new one.');
  }
  const targetParent = (hub.categoryId ? guild.channels.cache.get(hub.categoryId) : null) ?? hubChannel?.parent ?? null;
  const missing = missingBotPermissions(targetParent ?? guild, REQUIRED_BOT_PERMS);
  if (missing.length) {
    const where = targetParent ? `**${escapeMarkdown(clampText(targetParent.name, 100))}**` : 'the server';
    problems.push(`I'm missing ${permissionNames(missing).join(', ')} in ${where}`);
  }
  if (hubChannel) {
    const hubMissing = missingBotPermissions(hubChannel, HUB_CHANNEL_PERMS);
    if (hubMissing.length) problems.push(`I'm missing ${permissionNames(hubMissing).join(', ')} in the hub channel`);
  }
  if ((targetParent?.children?.cache?.size ?? 0) >= 50) problems.push('The category is full (50 channels)');
  return problems;
}

// ── CRUD used by /voice-hub (spec §6.19) ──────────────────────────────────────

export const countHubs = (guildId) => prisma.tempVoiceHub.count({ where: { guildId } });

export async function findHubByChannel(guildId, channelId) {
  const hub = await prisma.tempVoiceHub.findUnique({ where: { channelId } });
  return hub && hub.guildId === guildId ? hub : null;
}

export async function isTempChannel(channelId) {
  return (await prisma.tempVoiceChannel.count({ where: { channelId } })) > 0;
}

export const listHubs = (guildId) => prisma.tempVoiceHub.findMany({
  where: { guildId }, orderBy: { createdAt: 'asc' }, include: { _count: { select: { channels: true } } },
});

export const listHubChannels = (hubId) => prisma.tempVoiceChannel.findMany({ where: { hubId } });

/**
 * Creates a hub under the GuildSettings row lock so the per-guild cap holds under concurrency.
 * @throws {HubLimitError} at LIMITS.TEMPVOICE_HUBS; a P2002 (the channel is already a hub) propagates
 */
export async function createHub(guildId, data) {
  await ensureGuildSettings(guildId);
  const hub = await prisma.$transaction(async (tx) => {
    await tx.$queryRaw(guildSettingsLockSql(guildId));
    if (await tx.tempVoiceHub.count({ where: { guildId } }) >= LIMITS.TEMPVOICE_HUBS) throw new HubLimitError();
    return tx.tempVoiceHub.create({ data: { guildId, ...data } });
  });
  invalidateIndex(guildId);
  return hub;
}

/** @returns {Promise<object|null>} the updated hub, or null when it does not exist (for this guild) */
export async function updateHub(guildId, hubId, patch) {
  await prisma.tempVoiceHub.updateMany({ where: { id: hubId, guildId }, data: patch });
  const hub = await prisma.tempVoiceHub.findFirst({ where: { id: hubId, guildId } });
  invalidateIndex(guildId);
  return hub;
}

/**
 * Adds or removes one role id of `ignoredRoleIds` / `moderatorRoleIds` under a row lock.
 * @returns {Promise<'ok'|'exists'|'missing'|'limit'|'gone'>} `gone`: the hub no longer exists
 */
export async function updateHubRoleList(guildId, hubId, field, op, roleId) {
  if (field !== 'ignoredRoleIds' && field !== 'moderatorRoleIds') throw new Error(`Invalid role list field "${field}"`);
  if (op !== 'add' && op !== 'remove') throw new Error(`Invalid role list operation "${op}"`);
  return prisma.$transaction(async (tx) => {
    await tx.$queryRaw(hubLockSql(hubId, guildId));
    const hub = await tx.tempVoiceHub.findFirst({ where: { id: hubId, guildId }, select: { [field]: true } });
    if (!hub) return 'gone';
    const list = hub[field];
    if (op === 'add') {
      if (list.includes(roleId)) return 'exists';
      if (list.length >= MAX_HUB_ROLE_LIST) return 'limit';
      await tx.tempVoiceHub.update({ where: { id: hubId }, data: { [field]: [...list, roleId] } });
    } else {
      if (!list.includes(roleId)) return 'missing';
      await tx.tempVoiceHub.update({ where: { id: hubId }, data: { [field]: list.filter((id) => id !== roleId) } });
    }
    invalidateIndex(guildId);
    return 'ok';
  });
}

/**
 * Deletes a hub, its temporary channels (voice and text) and optionally the hub channel itself.
 * @returns {Promise<{ notFound?: true, deleted: number, failed: number, hubChannelFailed: boolean }>}
 */
export async function deleteHub(guild, hub, { deleteHubChannel = false, actor = null } = {}) {
  // Under the hub lock, so a channel being created for this hub cannot slip in between the listing and the (cascading) delete.
  const { rows, count } = await locks.run(`hub:${hub.id}`, async () => {
    const listed = await listHubChannels(hub.id);
    invalidateIndex(guild.id);
    const res = await prisma.tempVoiceHub.deleteMany({ where: { id: hub.id, guildId: guild.id } }); // the cascade removes the rows
    invalidateIndex(guild.id);
    return { rows: listed, count: res.count };
  });
  if (count === 0) return { notFound: true, deleted: 0, failed: 0, hubChannelFailed: false };

  const reason = clampText(`Temp voice hub deleted${actor?.tag ? ` by ${actor.tag}` : ''}`, 512);
  const results = await mapLimit(rows, 2, (row) => locks.run(`ch:${row.channelId}`, async () => {
    await cancelJobs({ dedupeKey: cleanupKey(row.channelId) });
    renameStamps.delete(row.channelId);
    const timer = textSyncTimers.get(row.channelId);
    if (timer) {
      clearTimeout(timer);
      textSyncTimers.delete(row.channelId);
    }
    await cache.del(renameKey(row.channelId));
    const v = await deleteDiscordChannel(guild, row.channelId, reason, 'delete voice'); // 10003 counts as deleted
    if (row.textChannelId) await deleteDiscordChannel(guild, row.textChannelId, reason, 'delete text');
    return v.ok || v.code === 10003;
  }));
  const deleted = results.filter((r) => r.status === 'fulfilled' && r.value === true).length;

  let hubChannelFailed = false;
  if (deleteHubChannel) {
    const r = await deleteDiscordChannel(guild, hub.channelId, reason, 'delete hub channel');
    hubChannelFailed = !r.ok && r.code !== 10003;
  }
  return { deleted, failed: rows.length - deleted, hubChannelFailed };
}
