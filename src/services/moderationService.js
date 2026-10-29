// Moderation service (pkg-moderation.md §4): case allocation, executeModAction / executeUnban /
// executeUnmute, immunity, DMs and the case log, time-windowed escalation, ban markers, ban import,
// expiry reconciliation, scheduler job handlers, external ban/unban handlers and reply helpers.
//
// Contract summary for the packages that build on this file:
//   executeModAction({ guild, target, moderator, action, reason?, durationMs?, deleteMessageSeconds?, proofUrl?,
//                      source, silent?, skipEscalation?, logFields? })  never throws, Discord action FIRST, then record
//   executeUnban / executeUnmute   never throw, relief actions (no immunity check)
//   isImmune(member, settings)     never throws
//   createInfraction(...)          records only (no Discord action); throws only on DB failure
// Every exported async function documents whether it throws. "Never throws" = catch, log, return a fallback.
import { Prisma } from '@prisma/client';
import { AuditLogEvent, PermissionFlagsBits } from 'discord.js';
import prisma from '../database/client.js';
import { isModuleEnabled } from '../config/modules.js';
import { LIMITS, PERMISSION_LEVELS } from '../config/constants.js';
import { ensureGuildSettings, getSettingsSafe } from './settingsService.js';
import { PermanentJobError, cancelJobs, getJobsByKeys, scheduleJob } from './scheduler.js';
import { createKeyedMutex, createThrottle, mapLimit, sleep, withTimeout } from '../utils/async.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import logger from '../utils/logger.js';
import { errorEmbed, neutralEmbed, successEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { formatDurationLong } from '../utils/parseDuration.js';
import {
  checkAssignableRole,
  checkChannelUsable,
  isInteractionGone,
  permissionNames,
} from '../utils/interactionHelpers.js';
import { clampText } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import { isHttpUrl } from '../utils/customEmbed.js';

// ── Constants (§4.1) ──────────────────────────────────────────────────────────

export const MOD_ACTIONS = Object.freeze(['WARN', 'TIMEOUT', 'MUTE', 'KICK', 'BAN', 'TEMPBAN', 'SOFTBAN']);
export const MOD_SOURCES = Object.freeze(['command', 'automod', 'escalation', 'captcha', 'dashboard', 'antiraid']);
/** Sources where a person picked the action (a slash command, or the web dashboard acting for its user): the moderator's
 * own roles, immunity rank and self-moderation are checked, and the DM may name them. */
const PERSON_SOURCES = new Set(['command', 'dashboard']);
const byPerson = (source) => PERSON_SOURCES.has(source);
export const MOD_ERROR_CODES = Object.freeze(['NOT_PERMITTED', 'HIERARCHY', 'IMMUNE', 'NOT_IN_GUILD', 'ALREADY', 'INVALID', 'DISCORD_ERROR']);
export const TIER_ACTIONS = Object.freeze(['TIMEOUT', 'MUTE', 'KICK', 'TEMPBAN', 'BAN']);
export const TIMEOUT_MAX_MS = 2_419_200_000; // 28 days (Discord maximum)
export const TIMED_ACTION_MIN_MS = 60_000; // 1 minute
export const TIMED_ACTION_MAX_MS = 31_536_000_000; // 365 days
export const DELETE_MESSAGES_MAX_S = 604_800; // 7 days
export const REASON_MAX = 1000; // stored reason / proof
export const AUDIT_REASON_MAX = 512; // Discord audit-log reason limit
/** Fallback `countFrom` for a legacy escalation tier without one (the migration normally stamps the real time). */
export const LEGACY_ESCALATION_SINCE = '2026-10-10T12:00:00.000Z';
/** Temp bans overdue by more than this are not lifted automatically (00-overview §15.4). */
export const OVERDUE_BAN_LIFT_LIMIT_MS = 30 * 86_400_000;
export const ACTION_PERMISSION = Object.freeze({
  WARN: null, TIMEOUT: 'ModerateMembers', MUTE: 'ManageRoles',
  KICK: 'KickMembers', BAN: 'BanMembers', TEMPBAN: 'BanMembers', SOFTBAN: 'BanMembers',
});
export const SOURCE_LABEL = Object.freeze({
  command: 'Command', automod: 'Automod', escalation: 'Escalation',
  automation: 'Automation', captcha: 'Captcha', antiraid: 'Anti-Raid', expiry: 'Expiry', external: 'External', dashboard: 'Dashboard',
});
export const ACTION_META = Object.freeze({
  WARN: { emoji: '⚠️', color: 0xfee75c, label: 'Warn', past: 'warned', active: true },
  TIMEOUT: { emoji: '⏱️', color: 0xff9800, label: 'Timeout', past: 'timed out', active: true },
  MUTE: { emoji: '🔇', color: 0xff9800, label: 'Mute', past: 'muted', active: true },
  KICK: { emoji: '👢', color: 0xff5722, label: 'Kick', past: 'kicked', active: false },
  SOFTBAN: { emoji: '🔨', color: 0xf44336, label: 'Softban', past: 'softbanned', active: false },
  BAN: { emoji: '🔨', color: 0xd32f2f, label: 'Ban', past: 'banned', active: true },
  TEMPBAN: { emoji: '⏳', color: 0xd32f2f, label: 'Temporary ban', past: 'temporarily banned', active: true }, // display only
  UNBAN: { emoji: '✅', color: 0x57f287, label: 'Unban', past: 'unbanned', active: false },
  UNMUTE: { emoji: '🔊', color: 0x57f287, label: 'Unmute', past: 'unmuted', active: false },
  NOTE: { emoji: '📝', color: 0x5865f2, label: 'Note', past: 'noted', active: false },
});
export const TIER_ACTION_LABEL = Object.freeze({
  TIMEOUT: 'Timeout', MUTE: 'Mute (mute role)', KICK: 'Kick', TEMPBAN: 'Temporary ban', BAN: 'Permanent ban',
});
export const ERROR_TITLES = Object.freeze({
  NOT_PERMITTED: 'Missing Permissions', HIERARCHY: 'Not Allowed', IMMUNE: 'Member Is Immune',
  NOT_IN_GUILD: 'Not Found', ALREADY: 'Nothing To Do', INVALID: 'Invalid Input', DISCORD_ERROR: 'Action Failed',
});

/**
 * Display type of an infraction row: a BAN with an expiry is shown as TEMPBAN (there is no TEMPBAN type in the DB).
 * @param {{ type?: string, expiresAt?: Date|null }|null} inf
 * @returns {string|undefined}
 */
export function displayType(inf) {
  return inf?.type === 'BAN' && inf.expiresAt ? 'TEMPBAN' : inf?.type;
}

/** User-facing messages, used verbatim (§4.1). Exported so the command package can reuse them. */
export const MSG = Object.freeze({
  BOT_SELF: "I can't moderate myself.",
  OWNER: "You can't moderate the server owner.",
  SELF: "You can't moderate yourself.",
  NOT_MEMBER: 'That user is not in this server.',
  BOT_ROLE: "I can't moderate that member: their highest role is above or equal to mine.",
  EXEC_ROLE: "You can't moderate that member: their highest role is above or equal to yours.",
  ADMIN_TIMEOUT: "Discord doesn't allow timing out members with the Administrator permission.",
  IMMUNE_AUTO: 'That member is immune to moderation (server owner, administrator or immune role).',
  IMMUNE_ROLE: 'That member has an immune role (see `/modconfig immune list`).',
  IMMUNE_ADMIN: "You can't moderate an administrator at or above your own permission level.",
  ALREADY_BANNED: 'That user is already banned.',
  SOFTBAN_BANNED: 'That user is already banned. A softban would lift their ban, so it was not run.',
  NO_ME: "I couldn't load my own server member. Try again in a moment.",
  NO_EXECUTOR: "I couldn't verify your roles. Please try again.",
  BAD_PROOF: 'Proof must be an http:// or https:// link (max 1000 characters).',
  MUTE_ROLE_GONE: 'The mute role no longer exists. Set a new one with `/modconfig muterole`.',
  INTERNAL: 'Something went wrong while moderating. Please try again.',
});

const SNOWFLAKE_RE = /^\d{17,20}$/;
const NOT_PERMITTED_FOR = (perm) => `I need the **${permissionNames([perm])[0]}** permission to do that.`;

// ── Raw SQL (00-overview rule 22) ─────────────────────────────────────────────

// Namespaced so it never contends with another package's per-guild advisory lock.
const lockCasesSql = (guildId) => Prisma.sql`SELECT pg_advisory_xact_lock(hashtext(${`infraction:${guildId}`}))`;

/** One entry per raw statement, with sample parameters (01-foundation §1). No timestamps are used in this file's raw SQL. */
export const RAW_SQL = Object.freeze([
  { name: 'moderation.lockCases', build: () => lockCasesSql('0') },
]);

// ── In-process state (§3.5; no timers, no I/O at import) ──────────────────────

const caseMutex = createKeyedMutex();
const targetMutex = createKeyedMutex();
const escalationMutex = createKeyedMutex();
const modLogWarnThrottle = createThrottle(600_000);
const muteWarnThrottle = createThrottle(600_000);
/** `${guildId}:${userId}:${'ban'|'unban'}` -> expiry in ms. Capped at BOT_MARK_CAP, oldest evicted. */
const botBanMarks = new Map();
const BOT_MARK_CAP = 10_000;

// ── Small pure helpers ────────────────────────────────────────────────────────

/**
 * Compares snowflakes as BigInt.
 * @param {Iterable<string>} ids
 * @returns {string|null} the largest id, or null for an empty input
 */
export function maxSnowflake(ids) {
  let best = null;
  for (const id of ids ?? []) {
    try {
      const v = BigInt(id);
      if (best === null || v > best) best = v;
    } catch { /* ignore a non-numeric id */ }
  }
  return best === null ? null : best.toString();
}

const toPosInt = (v) => {
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : null;
};

const isAction = (a) => MOD_ACTIONS.includes(a);

/**
 * Pure. Maps a discord.js / REST error to `{ code, error }`.
 * @param {any} err
 * @returns {{ code: string, error: string }}
 */
export function mapDiscordError(err) {
  switch (err?.code) {
    case 50013: return { code: 'NOT_PERMITTED', error: "Discord refused: I'm missing a permission or my role is too low." };
    case 50001: return { code: 'NOT_PERMITTED', error: "Discord refused: I can't access that." };
    case 10007: return { code: 'NOT_IN_GUILD', error: MSG.NOT_MEMBER };
    case 10013: return { code: 'INVALID', error: 'Unknown user.' };
    case 10011: return { code: 'INVALID', error: 'That role no longer exists.' };
    case 10026: return { code: 'ALREADY', error: 'That user is not banned.' };
    case 50035: return { code: 'INVALID', error: `Discord rejected the request: ${clampText(err?.message, 200)}` };
    default: return { code: 'DISCORD_ERROR', error: `Discord error: ${clampText(err?.message ?? 'unknown', 200)}` };
  }
}

/**
 * Pure. The audit-log reason for a Discord action (the case number is not known yet, so it is not in it).
 * @param {string} reason
 * @param {{ username?: string, tag?: string, id?: string }} moderator
 * @param {string} source
 * @returns {string} at most 512 characters
 */
export function buildAuditReason(reason, moderator, source) {
  let text;
  const by = moderator?.username ?? moderator?.tag ?? moderator?.id ?? 'unknown';
  if (source === 'command') text = `${reason} | by ${by}`;
  else if (source === 'dashboard') text = `[Dashboard] ${reason} | by ${by}`; // the bot is the audit-log executor
  else if (source === 'escalation') text = String(reason);
  else {
    // Automod and captcha reasons already carry their own tag ("[Automod] …", "Captcha: …"): never tag them twice.
    const label = SOURCE_LABEL[source] ?? source;
    const own = String(reason ?? '');
    text = own.startsWith(`[${label}]`) || own.startsWith(`${label}:`) ? own : `[${label}] ${own}`;
  }
  return clampText(text, AUDIT_REASON_MAX);
}

/**
 * Pure. Decides the action that will really run (§4.5).
 * @param {{ action: string, durationMs?: number|null, muteRoleId?: string|null }} input
 * @returns {{ ok: true, action: string, durationMs: number|null, note: string|null }
 *   | { ok: false, code: 'INVALID', error: string }}
 */
export function resolveEffectiveAction({ action, durationMs = null, muteRoleId = null } = {}) {
  const bad = (error) => ({ ok: false, code: 'INVALID', error });
  if (!isAction(action)) return bad('Unknown moderation action.');
  let dur = null;
  if (durationMs !== null && durationMs !== undefined) {
    if (typeof durationMs !== 'number' || !Number.isFinite(durationMs) || durationMs <= 0) return bad('Invalid duration.');
    dur = Math.round(durationMs);
  }
  let act = action;
  if (act === 'BAN' && dur !== null) act = 'TEMPBAN';

  if (act === 'TEMPBAN') {
    if (dur === null || dur < TIMED_ACTION_MIN_MS || dur > TIMED_ACTION_MAX_MS) {
      return bad('Temporary bans need a duration between 1 minute and 1 year.');
    }
    return { ok: true, action: 'TEMPBAN', durationMs: dur, note: null };
  }

  if (act === 'TIMEOUT') {
    if (dur === null) return bad('Timeouts need a duration.');
    if (dur < TIMED_ACTION_MIN_MS) return bad('Durations must be at least 1 minute.');
    if (dur > TIMEOUT_MAX_MS) {
      if (muteRoleId) {
        if (dur <= TIMED_ACTION_MAX_MS) {
          return { ok: true, action: 'MUTE', durationMs: dur, note: 'Longer than 28 days, so the mute role was used instead of a timeout.' };
        }
        return bad('Mutes can last at most 1 year.');
      }
      return bad('Timeouts can last at most 28 days. Set a mute role with `/modconfig muterole` for longer mutes.');
    }
    return { ok: true, action: 'TIMEOUT', durationMs: dur, note: null };
  }

  if (act === 'MUTE') {
    if (dur !== null) {
      if (dur < TIMED_ACTION_MIN_MS) return bad('Durations must be at least 1 minute.');
      if (dur > TIMED_ACTION_MAX_MS) return bad('Mutes can last at most 1 year.');
    }
    if (muteRoleId) return { ok: true, action: 'MUTE', durationMs: dur, note: null };
    if (dur !== null && dur <= TIMEOUT_MAX_MS) {
      return { ok: true, action: 'TIMEOUT', durationMs: dur, note: 'No mute role is set, so a timeout was used.' };
    }
    return bad('No mute role is configured. Set one with `/modconfig muterole`, or use a duration of 28 days or less.');
  }

  if (act === 'BAN') return { ok: true, action: 'BAN', durationMs: null, note: null };
  // WARN, KICK, SOFTBAN never carry a duration.
  return { ok: true, action: act, durationMs: null, note: null };
}

// ── Immunity (§4.4) ───────────────────────────────────────────────────────────

/**
 * True when the member has any of the guild's immune roles.
 * @param {import('discord.js').GuildMember} member
 * @param {{ immuneRoleIds?: string[] }|null} settings
 * @returns {boolean} never throws
 */
export function hasImmuneRole(member, settings) {
  try {
    const ids = settings?.immuneRoleIds ?? [];
    return ids.some((id) => member?.roles?.cache?.has(id));
  } catch {
    return false;
  }
}

/**
 * §10 contract. True for the guild owner, ADMIN level (or BOT_OWNER_ID) and members with an immune role.
 * @param {import('discord.js').GuildMember} member
 * @param {object|null} settings GuildSettings row
 * @returns {boolean} never throws
 */
export function isImmune(member, settings) {
  try {
    if (!member?.guild) return false;
    if (member.id === member.guild.ownerId) return true;
    if (resolvePermissionLevel(member, settings) >= PERMISSION_LEVELS.ADMIN) return true;
    return hasImmuneRole(member, settings);
  } catch {
    return false;
  }
}

// ── Preflight (§4.5) ──────────────────────────────────────────────────────────

const fail = (code, error) => ({ code, error });

/** Checks 1-3: the bot itself, the owner, the invoker. Returns null when fine. */
function checkIdentity({ guild, me, targetId, executor, source }) {
  if (me && targetId === me.id) return fail('HIERARCHY', MSG.BOT_SELF);
  if (targetId === guild?.ownerId) return fail('HIERARCHY', MSG.OWNER);
  if (byPerson(source) && executor && targetId === executor.id) return fail('INVALID', MSG.SELF);
  return null;
}

/** Check 8: the invoker must outrank the target (the owner always does). Returns null when fine. */
function checkExecutorRank({ guild, member, executor, source }) {
  if (byPerson(source) && executor && member && executor.id !== guild?.ownerId
    && member.roles.highest.comparePositionTo(executor.roles.highest) >= 0) {
    return fail('HIERARCHY', MSG.EXEC_ROLE);
  }
  return null;
}

/**
 * Pure (given objects). Returns null when the action is allowed, else `{ code, error }`.
 * `action` is the EFFECTIVE action (after resolveEffectiveAction).
 * @param {object} input
 * @returns {{ code: string, error: string }|null}
 */
export function preflightModAction({ action, guild, me, member, targetId, executor, settings, source, muteRole }) {
  const id = checkIdentity({ guild, me, targetId, executor, source });
  if (id) return id;
  if (['WARN', 'TIMEOUT', 'MUTE', 'KICK'].includes(action) && !member) return fail('NOT_IN_GUILD', MSG.NOT_MEMBER);

  const perm = ACTION_PERMISSION[action];
  if (perm && !me?.permissions?.has(PermissionFlagsBits[perm])) return fail('NOT_PERMITTED', NOT_PERMITTED_FOR(perm));

  // Immunity (policy §4.4)
  if (member) {
    if (!byPerson(source)) {
      if (isImmune(member, settings)) return fail('IMMUNE', MSG.IMMUNE_AUTO);
    } else if (!(executor && executor.id === guild?.ownerId)) {
      if (hasImmuneRole(member, settings)) return fail('IMMUNE', MSG.IMMUNE_ROLE);
      const targetLevel = resolvePermissionLevel(member, settings);
      const executorLevel = executor ? resolvePermissionLevel(executor, settings) : PERMISSION_LEVELS.MEMBER;
      if (targetLevel >= PERMISSION_LEVELS.ADMIN && targetLevel >= executorLevel) return fail('IMMUNE', MSG.IMMUNE_ADMIN);
    }
  }

  if (action === 'TIMEOUT' && member?.permissions?.has(PermissionFlagsBits.Administrator)) {
    return fail('HIERARCHY', MSG.ADMIN_TIMEOUT);
  }
  if (member) {
    if (action === 'TIMEOUT' && !member.moderatable) return fail('HIERARCHY', MSG.BOT_ROLE);
    if (action === 'KICK' && !member.kickable) return fail('HIERARCHY', MSG.BOT_ROLE);
    if (['BAN', 'TEMPBAN', 'SOFTBAN'].includes(action) && !member.bannable) return fail('HIERARCHY', MSG.BOT_ROLE);
    if (action === 'MUTE' && !member.manageable) return fail('HIERARCHY', MSG.BOT_ROLE);
  }
  if (action === 'MUTE') {
    if (!muteRole) return fail('INVALID', MSG.MUTE_ROLE_GONE);
    const chk = checkAssignableRole(guild, muteRole, { allowDangerous: false });
    if (!chk.ok) return fail('INVALID', `Mute role problem: ${chk.reason}`);
  }
  return checkExecutorRank({ guild, member, executor, source });
}

/**
 * Kept for compatibility (the legacy commands call it). Throws `Error(MSG.*)` using preflight checks 1-3,
 * 7b (ban semantics) and 8 (R038/R150). A missing target member is fine (banning by ID).
 * @param {import('discord.js').Guild} guild
 * @param {import('discord.js').GuildMember|null} targetMember
 * @param {import('discord.js').GuildMember|null} executorMember
 * @throws {Error} with a user-facing message when the action is not allowed
 */
export function assertHierarchy(guild, targetMember, executorMember) {
  if (!targetMember) return;
  const me = guild.members?.me ?? null;
  const identity = checkIdentity({ guild, me, targetId: targetMember.id, executor: executorMember, source: 'command' });
  if (identity) throw new Error(identity.error);
  if (!targetMember.bannable) throw new Error(MSG.BOT_ROLE);
  const rank = checkExecutorRank({ guild, member: targetMember, executor: executorMember, source: 'command' });
  if (rank) throw new Error(rank.error);
}

// ── Case allocation (§4.2) ────────────────────────────────────────────────────

/** Takes the per-guild case lock inside an interactive transaction (released at commit). */
async function lockGuildCases(tx, guildId) {
  await tx.$executeRaw(lockCasesSql(guildId));
}

/** Retries `fn` ONCE on a unique-violation / transaction-conflict style error, then re-throws. */
async function withDbRetry(fn) {
  try {
    return await fn();
  } catch (err) {
    if (['P2002', 'P2028', 'P2034'].includes(err?.code)) return fn();
    throw err;
  }
}

const TX_OPTS = { maxWait: 10_000, timeout: 15_000 };

/**
 * Inserts one infraction with the next per-guild caseId (advisory lock, no Discord call inside the transaction).
 * `escalationTierId` marks the case runEscalation creates for a tier; no other caller passes it.
 * @param {object} row
 * @returns {Promise<import('@prisma/client').Infraction>}
 * @throws on DB failure
 */
export async function recordInfraction({ guildId, targetUserId, moderatorId, type, reason = null, proofUrl = null, expiresAt = null, active = undefined, escalationTierId = null }) {
  const meta = ACTION_META[type];
  if (!meta) throw new Error(`Unknown infraction type "${type}"`);
  await ensureGuildSettings(guildId);
  return caseMutex.run(guildId, () => withDbRetry(() => prisma.$transaction(async (tx) => {
    await lockGuildCases(tx, guildId);
    const agg = await tx.infraction.aggregate({ where: { guildId }, _max: { caseId: true } });
    return tx.infraction.create({
      data: {
        caseId: (agg._max.caseId ?? 0) + 1,
        guildId,
        targetUserId,
        moderatorId,
        type,
        reason: clampText(String(reason ?? '').trim() || 'No reason provided', REASON_MAX),
        proofUrl: proofUrl ? clampText(proofUrl, REASON_MAX) : null,
        active: active ?? meta.active,
        expiresAt,
        escalationTierId: Number.isInteger(escalationTierId) && escalationTierId >= 1 ? escalationTierId : null,
      },
    });
  }, TX_OPTS)));
}

/**
 * Inserts many rows (ban import) in one locked transaction, allocating the block max+1 ... max+n.
 * Callers pass at most 500 rows per call.
 * @param {string} guildId
 * @param {Array<{ targetUserId: string, moderatorId: string, type: string, reason?: string|null, active?: boolean, expiresAt?: Date|null }>} rows
 * @returns {Promise<number>} rows inserted
 * @throws on DB failure
 */
export async function recordInfractionsBulk(guildId, rows) {
  if (!rows?.length) return 0;
  await ensureGuildSettings(guildId);
  return caseMutex.run(guildId, () => withDbRetry(() => prisma.$transaction(async (tx) => {
    await lockGuildCases(tx, guildId);
    const agg = await tx.infraction.aggregate({ where: { guildId }, _max: { caseId: true } });
    let next = (agg._max.caseId ?? 0) + 1;
    const data = rows.map((r) => ({
      caseId: next++,
      guildId,
      targetUserId: r.targetUserId,
      moderatorId: r.moderatorId,
      type: r.type,
      reason: clampText(String(r.reason ?? '').trim() || 'No reason provided', REASON_MAX),
      proofUrl: null,
      active: r.active ?? ACTION_META[r.type].active,
      expiresAt: r.expiresAt ?? null,
    }));
    const res = await tx.infraction.createMany({ data });
    return res.count;
  }, TX_OPTS)));
}

/**
 * Marks matching active rows inactive.
 * @param {string} guildId
 * @param {string} userId
 * @param {string[]} types InfractionType values
 * @returns {Promise<number>} rows changed
 * @throws on DB failure
 */
export async function resolveInfractions(guildId, userId, types) {
  const { count } = await prisma.infraction.updateMany({
    where: { guildId, targetUserId: userId, type: { in: types }, active: true },
    data: { active: false },
  });
  return count;
}

// ── Ban markers (§4.9) ────────────────────────────────────────────────────────

/**
 * Remembers (60 s) that the bot itself is about to ban/unban a user, so the ban hooks skip it.
 * @param {string} guildId
 * @param {string} userId
 * @param {'ban'|'unban'} kind
 * @param {number} [ttlMs=60000]
 */
export function markBotBanAction(guildId, userId, kind, ttlMs = 60_000) {
  const key = `${guildId}:${userId}:${kind}`;
  botBanMarks.delete(key); // re-insert so the eviction order follows the latest mark
  botBanMarks.set(key, Date.now() + ttlMs);
  while (botBanMarks.size > BOT_MARK_CAP) botBanMarks.delete(botBanMarks.keys().next().value);
}

/**
 * Non-destructive check of a marker; an expired one is dropped lazily.
 * @param {string} guildId
 * @param {string} userId
 * @param {'ban'|'unban'} kind
 * @returns {boolean}
 */
export function wasBotBanAction(guildId, userId, kind) {
  const key = `${guildId}:${userId}:${kind}`;
  const until = botBanMarks.get(key);
  if (until === undefined) return false;
  if (until <= Date.now()) {
    botBanMarks.delete(key);
    return false;
  }
  return true;
}

// ── DM and mod log (§4.8) ─────────────────────────────────────────────────────

/**
 * Best-effort DM to the target. Never throws.
 * @returns {Promise<import('discord.js').Message|null>} the sent message, or null (DMs closed, timeout, unknown user)
 */
export async function notifyTarget({ guild, user, action, reason, durationMs = null, expiresAt = null, caseId = null, moderator = null, showModerator = false }) {
  try {
    const u = typeof user?.send === 'function' ? user : await guild.client.users.fetch(user?.id).catch(() => null);
    if (!u) return null;
    const meta = ACTION_META[action] ?? ACTION_META.NOTE;
    const embed = neutralEmbed(clampText(`${meta.emoji} You were ${meta.past} in ${guild.name}`, 256)).setColor(meta.color);
    if (action === 'SOFTBAN') embed.setDescription('Your recent messages were removed. You can rejoin with a new invite.');
    const fields = [{ name: 'Reason', value: clampText(reason || 'No reason provided', 1024), inline: false }];
    if (durationMs) fields.push({ name: 'Duration', value: clampText(formatDurationLong(durationMs), 1024), inline: true });
    else if (action === 'MUTE') fields.push({ name: 'Duration', value: 'Until a moderator unmutes you', inline: true });
    if (expiresAt) {
      fields.push({ name: 'Ends', value: clampText(`${discordTimestamp(expiresAt, 'f')} (${discordTimestamp(expiresAt, 'R')})`, 1024), inline: true });
    }
    if (showModerator && moderator?.username) fields.push({ name: 'Moderator', value: clampText(moderator.username, 1024), inline: true });
    embed.addFields(fields);
    if (caseId) embed.setFooter({ text: `Case #${caseId}` });
    return await withTimeout(u.send({ embeds: [embed], allowedMentions: { parse: [] } }), 5000, 'moderation DM');
  } catch {
    return null; // 50007 (DMs closed), timeout, anything else
  }
}

const tick = (s) => String(s).replace(/`/g, "'");

/**
 * Pure. Builds the case embed.
 * @returns {import('discord.js').EmbedBuilder}
 */
export function buildModLogEmbed({ infraction, target, moderator, source = 'command', logFields = null }) {
  const dt = displayType(infraction);
  const meta = ACTION_META[dt] ?? ACTION_META.NOTE;
  const embed = neutralEmbed(clampText(`${meta.emoji} ${meta.label} | Case #${infraction.caseId}`, 256)).setColor(meta.color);
  const avatar = typeof target?.displayAvatarURL === 'function' ? target.displayAvatarURL() : null;
  if (avatar) embed.setThumbnail(avatar);
  const fields = [
    {
      name: 'Target',
      value: `<@${infraction.targetUserId}> \`${tick(clampText(target?.tag ?? target?.username ?? 'Unknown user', 100))}\``,
      inline: true,
    },
    {
      name: 'Moderator',
      value: `<@${infraction.moderatorId}> \`${tick(clampText(moderator?.tag ?? moderator?.username ?? infraction.moderatorId, 100))}\``,
      inline: true,
    },
  ];
  if (source !== 'command') fields.push({ name: 'Source', value: SOURCE_LABEL[source] ?? String(source), inline: true });
  fields.push({ name: 'Reason', value: clampText(infraction.reason || 'No reason provided', 1024), inline: false });
  if (infraction.expiresAt) {
    fields.push({
      name: 'Expires',
      value: `${discordTimestamp(infraction.expiresAt, 'f')} (${discordTimestamp(infraction.expiresAt, 'R')})`,
      inline: false,
    });
  }
  if (infraction.proofUrl) fields.push({ name: 'Proof', value: clampText(infraction.proofUrl, 1024), inline: false });
  if (Array.isArray(logFields)) {
    for (const f of logFields.slice(0, 3)) {
      fields.push({ name: clampText(f?.name, 256) || '—', value: clampText(f?.value, 1024) || '—', inline: false });
    }
  }
  embed.addFields(fields);
  embed.setFooter({ text: `User ID: ${infraction.targetUserId}` });
  embed.setTimestamp(infraction.createdAt ?? new Date());
  return embed;
}

/**
 * Posts an embed to the case-log channel. No-op without modLogChannelId. Independent of loggingEnabled (R039).
 * Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {import('discord.js').EmbedBuilder} embed
 * @returns {Promise<void>}
 */
export async function postModLog(guild, embed) {
  try {
    const settings = await getSettingsSafe(guild.id);
    const id = settings?.modLogChannelId;
    if (!id) return;
    const channel = guild.channels.cache.get(id) ?? await guild.channels.fetch(id).catch(() => null);
    const u = checkChannelUsable(channel);
    if (!u.ok) {
      if (modLogWarnThrottle(guild.id)) logger.warn(`Mod log channel unusable in ${guild.id}: ${u.reason ?? u.missing.join(', ')}`);
      return;
    }
    await channel.send({ embeds: [embed], allowedMentions: { parse: [] } })
      .catch((e) => logger.warn(`Mod log post failed in ${guild.id}: ${e?.message ?? e}`));
  } catch (err) {
    logger.warn(`Mod log failed in ${guild?.id}: ${err?.message ?? err}`);
  }
}

/**
 * Builds and posts the case embed. Never throws. `client` is accepted for compatibility and ignored.
 * @returns {Promise<void>}
 */
export async function sendModLog(guild, infraction, target, moderator, client, { source = 'command', logFields = null } = {}) {
  try {
    await postModLog(guild, buildModLogEmbed({ infraction, target, moderator, source, logFields }));
  } catch (err) {
    logger.warn(`Mod log embed failed in ${guild?.id}: ${err?.message ?? err}`);
  }
}

// ── createInfraction (§4.3) ───────────────────────────────────────────────────

const DM_TYPES = new Set(['WARN', 'TIMEOUT', 'MUTE', 'KICK', 'SOFTBAN', 'BAN']);

/**
 * Records only (no Discord action), then DMs + mod-logs best-effort, then escalation for WARN.
 * Used by the expiry jobs, the external ban/unban hooks, `/case clear-history`'s NOTE and the legacy automod path.
 * @param {import('discord.js').Guild} guild
 * @param {{ id: string }} target User or `{ id }`
 * @param {{ id: string }} moderator
 * @param {string} type InfractionType
 * @param {{ reason?: string, proofUrl?: string, durationMs?: number, expiresAt?: Date, skipEscalation?: boolean,
 *   silent?: boolean, client?: object, source?: string, logFields?: Array<{ name: string, value: string }> }} [opts]
 * @returns {Promise<import('@prisma/client').Infraction>}
 * @throws only on DB failure (DM, mod log and escalation are best-effort)
 */
export async function createInfraction(guild, target, moderator, type, opts = {}) {
  const expiresAt = opts.expiresAt ?? (opts.durationMs ? new Date(Date.now() + opts.durationMs) : null);
  const infraction = await recordInfraction({
    guildId: guild.id,
    targetUserId: target.id,
    moderatorId: moderator.id,
    type,
    reason: opts.reason,
    proofUrl: opts.proofUrl,
    expiresAt,
  });

  const source = opts.source ?? 'command';
  if (!opts.silent && DM_TYPES.has(type)) {
    try {
      if ((await getSettingsSafe(guild.id))?.modDmTargets !== false) {
        await notifyTarget({
          guild,
          user: target,
          action: displayType(infraction),
          reason: infraction.reason,
          durationMs: expiresAt ? Math.max(0, expiresAt.getTime() - Date.now()) || null : null,
          expiresAt,
          caseId: infraction.caseId,
          moderator,
          showModerator: false,
        });
      }
    } catch (err) {
      logger.warn(`Moderation DM failed in ${guild.id}: ${err?.message ?? err}`);
    }
  }
  try {
    const resolved = target?.tag || target?.username ? target : (await guild.client.users.fetch(target.id).catch(() => ({ id: target.id })));
    await sendModLog(guild, infraction, resolved, moderator, guild.client, { source, logFields: opts.logFields ?? null });
  } catch (err) {
    logger.warn(`Mod log failed in ${guild.id}: ${err?.message ?? err}`);
  }
  if (type === 'WARN' && !opts.skipEscalation) {
    try {
      await runEscalation(guild, target.id);
    } catch (err) {
      logger.error(`Escalation failed for ${target.id} in ${guild.id}`, err);
    }
  }
  return infraction;
}

// ── Escalation (§4.10) ────────────────────────────────────────────────────────

/**
 * Pure; never throws. Returns v2 tiers (§3.2): sorted, deduped, capped, ids stable, `countFrom` kept.
 * @param {unknown} raw GuildSettings.infractionEscalation
 * @returns {Array<{ id: number, warnCount: number, withinSeconds: number|null, action: string, durationSeconds: number|null, countFrom: string|null }>}
 */
export function normalizeEscalationTiers(raw) {
  try {
    if (!Array.isArray(raw)) return [];
    const entries = [];
    for (const e of raw) {
      if (!e || typeof e !== 'object' || Array.isArray(e)) continue;
      const legacy = !('withinSeconds' in e) && !('durationSeconds' in e);
      if (!Number.isInteger(e.warnCount) || e.warnCount < 1 || e.warnCount > 100) continue;
      let action = String(e.action ?? '').toUpperCase();
      if (!TIER_ACTIONS.includes(action)) continue;
      let dur = toPosInt(legacy ? e.duration : e.durationSeconds);
      let within = null;
      if (!legacy) {
        const w = Number(e.withinSeconds);
        within = Number.isInteger(w) && w >= 60 && w <= 31_536_000 ? w : null;
      }
      if (legacy && action === 'BAN' && dur) action = 'TEMPBAN';
      if (action === 'TIMEOUT') {
        if (dur === null) continue;
        dur = Math.min(Math.max(dur, 60), 2_419_200);
      } else if (action === 'TEMPBAN') {
        if (dur === null) continue;
        dur = Math.min(Math.max(dur, 60), 31_536_000);
      } else if (action === 'MUTE') {
        if (dur !== null) dur = Math.min(Math.max(dur, 60), 31_536_000);
      } else {
        dur = null;
      }
      const cf = e.countFrom instanceof Date ? e.countFrom.toISOString() : e.countFrom;
      let countFrom = typeof cf === 'string' && Number.isFinite(Date.parse(cf)) ? cf : null;
      if (legacy && countFrom === null) countFrom = LEGACY_ESCALATION_SINCE;
      entries.push({
        id: Number.isInteger(e.id) && e.id >= 1 ? e.id : null,
        warnCount: e.warnCount,
        withinSeconds: within,
        action,
        durationSeconds: dur,
        countFrom,
      });
    }

    // Assign ids: missing ones and later duplicates get max+1, +2, ... in original order.
    const taken = new Set();
    let next = Math.max(0, ...entries.map((t) => t.id ?? 0));
    for (const t of entries) {
      if (t.id !== null && !taken.has(t.id)) { taken.add(t.id); continue; }
      do { next += 1; } while (taken.has(next));
      t.id = next;
      taken.add(next);
    }

    // Dedupe on (warnCount, withinSeconds): the last occurrence wins.
    const byKey = new Map();
    for (const t of entries) byKey.set(`${t.warnCount}|${t.withinSeconds ?? 'null'}`, t);
    const list = [...byKey.values()];

    list.sort((a, b) => a.warnCount - b.warnCount
      || (a.withinSeconds ?? Infinity) - (b.withinSeconds ?? Infinity)
      || a.id - b.id);
    return list.slice(0, LIMITS.ESCALATION_TIERS).map((t) => ({
      id: t.id,
      warnCount: t.warnCount,
      withinSeconds: t.withinSeconds,
      action: t.action,
      durationSeconds: t.durationSeconds,
      countFrom: t.countFrom,
    }));
  } catch (err) {
    logger.warn(`normalizeEscalationTiers failed: ${err?.message ?? err}`);
    return [];
  }
}

/**
 * Pure. "T3: 5 warnings within 1 week → Temporary ban for 3 days" (at most 100 characters: autocomplete-safe).
 * @param {{ id: number, warnCount: number, withinSeconds: number|null, action: string, durationSeconds: number|null }} t
 * @returns {string}
 */
export function describeTier(t) {
  let s = `T${t.id}: ${t.warnCount} warning${t.warnCount === 1 ? '' : 's'}`;
  if (t.withinSeconds) s += ` within ${formatDurationLong(t.withinSeconds * 1000)}`;
  s += ` → ${TIER_ACTION_LABEL[t.action] ?? t.action}`;
  if (t.durationSeconds) s += ` for ${formatDurationLong(t.durationSeconds * 1000)}`;
  else if (t.action === 'MUTE') s += ' until unmuted';
  return clampText(s, 100);
}

export const TIER_SEVERITY = Object.freeze({ TIMEOUT: 1, MUTE: 2, KICK: 3, TEMPBAN: 4, BAN: 5 });

/** Start of the counting window for one tier, or null when no warn is eligible for a no-limit tier. */
function tierWindowStart(tier, sortedWarnTimes, now) {
  const parsed = tier.countFrom ? Date.parse(tier.countFrom) : NaN;
  const floor = Number.isFinite(parsed) ? parsed : -Infinity;
  if (tier.withinSeconds) return Math.max(now - tier.withinSeconds * 1000, floor);
  const oldest = sortedWarnTimes.find((t) => t >= floor);
  return oldest === undefined ? null : oldest;
}

/**
 * Pure. Picks the escalation tier to fire, or null.
 * - Only warns at or after a tier's `countFrom` exist for that tier (legacy tiers must not fire retroactively).
 * - A tier fires at most once per window; a tier that already fired is removed before the choice, so the most
 *   severe UN-fired matching tier fires (fallback is intentional).
 * - A tier with no time limit counts from the oldest still-active warn (a pardon restarts it).
 * @param {ReturnType<typeof normalizeEscalationTiers>} tiers
 * @param {number[]} warnTimes ms timestamps of ACTIVE warns
 * @param {Array<{ tierId: number, at: number }>} prior escalation infractions
 * @param {number} [now]
 * @returns {{ tier: object, count: number }|null}
 */
export function selectEscalationTier(tiers, warnTimes, prior, now = Date.now()) {
  const sorted = [...warnTimes].sort((a, b) => a - b);
  if (!sorted.length) return null;
  const candidates = [];
  for (const tier of tiers) {
    const windowStart = tierWindowStart(tier, sorted, now);
    if (windowStart === null) continue;
    const count = sorted.filter((t) => t >= windowStart).length;
    if (count < tier.warnCount) continue;
    if ((prior ?? []).some((p) => p.tierId === tier.id && p.at >= windowStart)) continue;
    candidates.push({ tier, count });
  }
  if (!candidates.length) return null;
  const dur = (t) => t.durationSeconds ?? (t.action === 'MUTE' ? Infinity : 0);
  candidates.sort((a, b) => (TIER_SEVERITY[b.tier.action] - TIER_SEVERITY[a.tier.action])
    || (dur(b.tier) - dur(a.tier))
    || (b.tier.warnCount - a.tier.warnCount)
    || (a.tier.id - b.tier.id));
  return candidates[0];
}

/**
 * Runs time-windowed escalation after a warn. Nothing is recorded on failure, so the next warn retries.
 * Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {string} userId
 * @returns {Promise<null|{ tierId: number, action: string, ok: boolean, caseId: number|null, error: string|null }>}
 */
export async function runEscalation(guild, userId) {
  try {
    const settings = await getSettingsSafe(guild.id);
    if (!isModuleEnabled(settings, 'moderation')) return null; // escalation is part of the module
    const tiers = normalizeEscalationTiers(settings?.infractionEscalation);
    if (!tiers.length) return null;
    return await escalationMutex.run(`${guild.id}:${userId}`, async () => {
      const warns = await prisma.infraction.findMany({
        where: { guildId: guild.id, targetUserId: userId, type: 'WARN', active: true },
        select: { createdAt: true },
        orderBy: { createdAt: 'desc' },
        take: 500,
      });
      if (!warns.length) return null;
      const warnTimes = warns.map((w) => w.createdAt.getTime());
      const sorted = [...warnTimes].sort((a, b) => a - b);
      const now = Date.now();
      const starts = tiers.map((t) => tierWindowStart(t, sorted, now)).filter((s) => s !== null);
      if (!starts.length) return null;
      const minStart = Math.min(...starts);
      // Which tiers already fired comes from the column runEscalation sets, never from reason text (a person can type or
      // edit "[Escalation T1]" into a reason).
      const rows = await prisma.infraction.findMany({
        where: {
          guildId: guild.id,
          targetUserId: userId,
          escalationTierId: { not: null },
          createdAt: { gte: new Date(minStart) },
        },
        select: { escalationTierId: true, createdAt: true },
      });
      const prior = rows.map((r) => ({ tierId: r.escalationTierId, at: r.createdAt.getTime() }));
      const pick = selectEscalationTier(tiers, warnTimes, prior, now);
      if (!pick) return null;
      const { tier, count } = pick;
      const reason = clampText(
        `[Escalation T${tier.id}] Reached ${count} active warning${count === 1 ? '' : 's'}${tier.withinSeconds ? ` within ${formatDurationLong(tier.withinSeconds * 1000)}` : ''}`,
        AUDIT_REASON_MAX,
      );
      const res = await executeModAction({
        guild,
        target: { id: userId },
        moderator: guild.client.user,
        action: tier.action,
        durationMs: tier.durationSeconds ? tier.durationSeconds * 1000 : null,
        reason,
        source: 'escalation',
        skipEscalation: true,
        escalationTierId: tier.id,
      });
      if (res.ok) logger.info(`Escalation T${tier.id} applied ${tier.action} to ${userId} in ${guild.id}`);
      else logger.info(`Escalation T${tier.id} (${tier.action}) not applied for ${userId} in ${guild.id}: ${res.code} ${res.error}`);
      return {
        tierId: tier.id,
        action: tier.action,
        ok: res.ok,
        caseId: res.ok ? res.infraction.caseId : null,
        error: res.ok ? null : res.error,
      };
    });
  } catch (err) {
    logger.error(`runEscalation failed for ${userId} in ${guild?.id}`, err);
    return null;
  }
}

// ── executeModAction (§4.6) ───────────────────────────────────────────────────

async function getMe(guild) {
  return guild.members.me ?? await guild.members.fetchMe().catch(() => null);
}

/**
 * Cached member, else a REST fetch. Returns null ONLY when Discord says the user is not a member (10007) or does
 * not exist (10013); any other failure (5xx, rate limit, timeout) is re-thrown, because a transient error must
 * never be read as "the member left" (00-overview rule 20).
 */
async function getMemberOrNull(guild, id) {
  const cached = guild.members.cache.get(id);
  if (cached) return cached;
  try {
    return await guild.members.fetch(id);
  } catch (err) {
    if (err?.code === 10007 || err?.code === 10013) return null;
    throw err;
  }
}

/**
 * Runs the Discord action FIRST, then records, DMs (per modDmTargets), mod-logs and schedules expiry (§10 contract).
 * Never throws; every failure is `{ ok: false, code, error }`. Does not refuse when `modEnabled` is false.
 * @param {object} [input]
 * @param {import('discord.js').Guild} input.guild
 * @param {import('discord.js').User|import('discord.js').GuildMember|{ id: string }} input.target
 * @param {import('discord.js').User} input.moderator `client.user` for automated actions
 * @param {'WARN'|'TIMEOUT'|'MUTE'|'KICK'|'BAN'|'TEMPBAN'|'SOFTBAN'} input.action
 * @param {string} [input.reason]
 * @param {number} [input.durationMs]
 * @param {number} [input.deleteMessageSeconds] 0..604800
 * @param {string} [input.proofUrl]
 * @param {'command'|'automod'|'escalation'|'captcha'|'dashboard'|'antiraid'} [input.source] 'dashboard': the web dashboard acting for
 *   its user (`moderator`), checked like a command
 * @param {boolean} [input.silent] no DMs
 * @param {boolean} [input.skipEscalation]
 * @param {number} [input.escalationTierId] runEscalation only: stored on the case so the tier counts as fired; ignored
 *   unless `source` is 'escalation' and the moderator is the bot
 * @param {Array<{ name: string, value: string }>} [input.logFields] at most 3, appended to the case embed
 * @returns {Promise<{ ok: true, infraction: object, action: string, dm: 'sent'|'failed'|'skipped', warnings: string[],
 *   escalation: null|{ tierId: number, action: string, ok: boolean, caseId: number|null, error: string|null } }
 *   | { ok: false, code: string, error: string, applied?: true }>}
 */
export async function executeModAction(input = {}) {
  try {
    const {
      guild, target, moderator, action, durationMs = null, deleteMessageSeconds = null,
      proofUrl = null, source = 'command', silent = false, skipEscalation = false, logFields = null,
    } = input;
    const invalid = (error) => ({ ok: false, code: 'INVALID', error });
    const tierId = source === 'escalation' && moderator?.id === guild?.client?.user?.id ? input.escalationTierId : null;

    // Step 0: validate input.
    if (!guild) return invalid('Missing guild.');
    if (!target?.id || !SNOWFLAKE_RE.test(String(target.id))) return invalid('Invalid target user.');
    if (!moderator?.id) return invalid('Missing moderator.');
    if (!MOD_SOURCES.includes(source)) return invalid('Unknown moderation source.');
    if (proofUrl && (!isHttpUrl(proofUrl) || String(proofUrl).length > REASON_MAX)) return invalid(MSG.BAD_PROOF);
    if (deleteMessageSeconds !== null && deleteMessageSeconds !== undefined
      && !(Number.isInteger(deleteMessageSeconds) && deleteMessageSeconds >= 0 && deleteMessageSeconds <= DELETE_MESSAGES_MAX_S)) {
      return invalid('Message deletion must be between 0 and 7 days.');
    }
    const reason = clampText(String(input.reason ?? '').trim() || 'No reason provided', REASON_MAX);

    // Step 1: settings (null is tolerated and means the schema defaults).
    const settings = await getSettingsSafe(guild.id);

    // Step 2: effective action.
    const eff = resolveEffectiveAction({ action, durationMs, muteRoleId: settings?.muteRoleId ?? null });
    if (!eff.ok) return { ok: false, code: eff.code, error: eff.error };

    // Step 3: perform, serialised per target.
    const result = await targetMutex.run(`${guild.id}:${target.id}`, async () => {
      // a. the bot member
      const me = await getMe(guild);
      if (!me) return { ok: false, code: 'DISCORD_ERROR', error: MSG.NO_ME };
      // b. the target member, d. the executor (a failed lookup is an error, never "not in the server")
      let member;
      let executor = null;
      try {
        member = (target.guild?.id === guild.id && typeof target.roles === 'object')
          ? target
          : await getMemberOrNull(guild, target.id);
        if (byPerson(source) && moderator.id !== guild.client.user.id) {
          executor = await getMemberOrNull(guild, moderator.id);
          if (!executor) return invalid(MSG.NO_EXECUTOR);
        }
      } catch (e) {
        return { ok: false, ...mapDiscordError(e) };
      }
      // c. the target user
      const user = member?.user ?? (typeof target.send === 'function' ? target : await guild.client.users.fetch(target.id).catch(() => null));
      // e. the mute role
      const muteRole = eff.action === 'MUTE' ? (guild.roles.cache.get(settings?.muteRoleId) ?? null) : null;
      // f. preflight
      const err = preflightModAction({ action: eff.action, guild, me, member, targetId: target.id, executor, settings, source, muteRole });
      if (err) return { ok: false, ...err };

      // g. ban state
      if (['BAN', 'TEMPBAN', 'SOFTBAN'].includes(eff.action)) {
        const state = await guild.bans.fetch({ user: target.id, force: true, cache: false })
          .then(() => 'banned', (e) => (e?.code === 10026 ? 'not-banned' : e));
        if (state === 'banned') {
          return { ok: false, code: 'ALREADY', error: eff.action === 'SOFTBAN' ? MSG.SOFTBAN_BANNED : MSG.ALREADY_BANNED };
        }
        if (state !== 'not-banned') {
          if (state?.code === 50013 || state?.code === 50001) {
            return { ok: false, code: 'NOT_PERMITTED', error: 'I need the **Ban Members** permission to check bans.' };
          }
          return { ok: false, code: 'DISCORD_ERROR', error: `Couldn't check the ban list: ${clampText(state?.message, 200)}` };
        }
      }

      // h. delivery settings
      const visibility = settings?.modVisibility ?? 'PUBLIC';
      const showModerator = byPerson(source) && visibility === 'PUBLIC';
      const dmEnabled = !silent && (settings?.modDmTargets ?? true);
      const expiresAt = eff.durationMs ? new Date(Date.now() + eff.durationMs) : null;
      const warnings = eff.note ? [eff.note] : [];
      let dm = 'skipped';

      // i. DM before the action (the user usually cannot receive it once they share no guild with the bot)
      let dmMessage = null;
      if (['KICK', 'BAN', 'TEMPBAN', 'SOFTBAN'].includes(eff.action) && dmEnabled) {
        dmMessage = user
          ? await notifyTarget({ guild, user, action: eff.action, reason, durationMs: eff.durationMs, expiresAt, caseId: null, moderator, showModerator })
          : null;
        dm = dmMessage ? 'sent' : 'failed';
      }

      // j. audit reason
      const auditReason = buildAuditReason(reason, moderator, source);

      // k. markers
      if (['BAN', 'TEMPBAN', 'SOFTBAN'].includes(eff.action)) markBotBanAction(guild.id, target.id, 'ban');
      if (eff.action === 'SOFTBAN') markBotBanAction(guild.id, target.id, 'unban');

      // l. the Discord action
      let softbanStuck = false;
      try {
        switch (eff.action) {
          case 'TIMEOUT': await member.timeout(eff.durationMs, auditReason); break;
          case 'MUTE': await member.roles.add(muteRole, auditReason); break; // one role, never the array form
          case 'KICK': await member.kick(auditReason); break;
          case 'BAN':
          case 'TEMPBAN':
            await guild.bans.create(target.id, { reason: auditReason, deleteMessageSeconds: deleteMessageSeconds ?? 0 });
            break;
          case 'SOFTBAN': {
            await guild.bans.create(target.id, { reason: auditReason, deleteMessageSeconds: deleteMessageSeconds ?? DELETE_MESSAGES_MAX_S });
            const unbanReason = clampText('Softban: immediate unban', 512);
            try {
              await guild.bans.remove(target.id, unbanReason);
            } catch {
              await sleep(1000);
              try {
                await guild.bans.remove(target.id, unbanReason);
              } catch (e2) {
                softbanStuck = true;
                warnings.push('The ban was applied, but the automatic unban failed, so the user is still banned. Use `/unban` to lift it.');
                logger.warn(`Softban unban failed for ${target.id} in ${guild.id}: ${e2?.message ?? e2}`);
              }
            }
            break;
          }
          default: break; // WARN: no Discord action
        }
      } catch (e) {
        if (dmMessage) await dmMessage.delete().catch(() => {}); // take back the false DM
        return { ok: false, ...mapDiscordError(e) };
      }

      // m. record the case
      const type = eff.action === 'TEMPBAN' || (eff.action === 'SOFTBAN' && softbanStuck) ? 'BAN' : eff.action;
      const recReason = softbanStuck ? clampText(`[Softban: unban failed] ${reason}`, REASON_MAX) : reason;
      let infraction;
      try {
        infraction = await recordInfraction({
          guildId: guild.id,
          targetUserId: target.id,
          moderatorId: moderator.id,
          type,
          reason: recReason,
          proofUrl,
          expiresAt: type === 'BAN' && eff.action !== 'TEMPBAN' ? null : expiresAt,
          escalationTierId: tierId,
        });
      } catch (e) {
        logger.error(`Could not record ${eff.action} of ${target.id} in ${guild.id}`, e);
        if (eff.action === 'TEMPBAN') {
          try {
            await scheduleJob({
              guildId: guild.id,
              type: 'mod.unban',
              runAt: expiresAt,
              payload: { userId: target.id, infractionId: null },
              dedupeKey: `mod.unban:${guild.id}:${target.id}`,
              maxAttempts: 20,
              onConflict: 'replace',
            });
          } catch (e2) {
            logger.error('Could not schedule the temp-ban expiry', e2);
          }
        }
        return {
          ok: false,
          code: 'DISCORD_ERROR',
          applied: true,
          error: `The ${ACTION_META[eff.action].label.toLowerCase()} was applied, but saving the case failed. Please record it manually.`,
        };
      }

      // n. supersede older TIMEOUT / MUTE rows (best-effort)
      if (type === 'TIMEOUT' || type === 'MUTE') {
        try {
          const old = await prisma.infraction.findMany({
            where: { guildId: guild.id, targetUserId: target.id, type, active: true, id: { not: infraction.id } },
            select: { id: true },
          });
          if (old.length) {
            await prisma.infraction.updateMany({ where: { id: { in: old.map((r) => r.id) } }, data: { active: false } });
            await mapLimit(old, 4, (r) => cancelJobs({ dedupeKey: `mod.expire:${r.id}` }));
          }
        } catch (e) {
          logger.warn(`Could not supersede older ${type} rows for ${target.id} in ${guild.id}: ${e?.message ?? e}`);
        }
      }

      // o. schedule the expiry (best-effort)
      try {
        if (eff.action === 'TEMPBAN') {
          await scheduleJob({
            guildId: guild.id,
            type: 'mod.unban',
            runAt: expiresAt,
            payload: { userId: target.id, infractionId: infraction.id },
            dedupeKey: `mod.unban:${guild.id}:${target.id}`,
            maxAttempts: 20,
            onConflict: 'replace',
          });
        } else if (type === 'BAN') {
          await cancelJobs({ dedupeKey: `mod.unban:${guild.id}:${target.id}` }); // a stale temp-ban job
        } else if (type === 'TIMEOUT' || (type === 'MUTE' && expiresAt)) {
          await scheduleJob({
            guildId: guild.id,
            type: 'mod.expire',
            runAt: expiresAt,
            payload: { infractionId: infraction.id, ...(type === 'MUTE' ? { roleId: muteRole.id } : {}) },
            dedupeKey: `mod.expire:${infraction.id}`,
            maxAttempts: 20,
            onConflict: 'replace',
          });
        }
      } catch (e) {
        logger.error(`Could not schedule the expiry of case #${infraction.caseId} in ${guild.id}`, e);
        warnings.push('The expiry could not be scheduled now; it will be retried when the bot restarts.');
      }

      // p. DM after the action
      if (['WARN', 'TIMEOUT', 'MUTE'].includes(eff.action) && dmEnabled) {
        const sent = user
          ? await notifyTarget({ guild, user, action: eff.action, reason, durationMs: eff.durationMs, expiresAt, caseId: infraction.caseId, moderator, showModerator })
          : null;
        dm = sent ? 'sent' : 'failed';
      }

      // q. mod log
      await sendModLog(guild, infraction, user ?? { id: target.id }, moderator, guild.client, { source, logFields });

      // r. result
      return { ok: true, infraction, action: eff.action, dm, warnings, escalation: null };
    });

    // Step 4: escalation, outside the target mutex.
    if (result.ok && result.infraction.type === 'WARN' && !skipEscalation && source !== 'escalation') {
      result.escalation = await runEscalation(guild, target.id);
    }
    return result;
  } catch (err) {
    logger.error('executeModAction failed', err);
    return { ok: false, code: 'DISCORD_ERROR', error: MSG.INTERNAL };
  }
}

// ── executeUnban / executeUnmute (§4.7) ───────────────────────────────────────

/**
 * Lifts a ban by user id. Relief action: no immunity check. Never throws.
 * @param {{ guild: object, userId: string, moderator: object, reason?: string|null, source?: string }} input
 * @returns {Promise<{ ok: true, infraction: object, user: object|null, cleared: number }
 *   | { ok: false, code: string, error: string, applied?: true }>}
 */
export async function executeUnban({ guild, userId, moderator, reason = null, source = 'command' } = {}) {
  try {
    if (!guild || !moderator?.id) return { ok: false, code: 'INVALID', error: 'Missing guild or moderator.' };
    if (!SNOWFLAKE_RE.test(String(userId ?? ''))) {
      return { ok: false, code: 'INVALID', error: 'Give a user ID (17–20 digits) or a mention.' };
    }
    const why = clampText(String(reason ?? '').trim() || 'No reason provided', REASON_MAX);
    return await targetMutex.run(`${guild.id}:${userId}`, async () => {
      const me = await getMe(guild);
      if (!me) return { ok: false, code: 'DISCORD_ERROR', error: MSG.NO_ME };
      if (!me.permissions.has(PermissionFlagsBits.BanMembers)) {
        return { ok: false, code: 'NOT_PERMITTED', error: NOT_PERMITTED_FOR('BanMembers') };
      }
      let ban;
      try {
        ban = await guild.bans.fetch({ user: userId, force: true, cache: false });
      } catch (e) {
        if (e?.code === 10026) {
          const cleared = await resolveInfractions(guild.id, userId, ['BAN']);
          await cancelJobs({ dedupeKey: `mod.unban:${guild.id}:${userId}` });
          return {
            ok: false,
            code: 'ALREADY',
            error: cleared ? `That user is not banned. ${cleared} stale ban record(s) were closed.` : 'That user is not banned.',
          };
        }
        if (e?.code === 50013 || e?.code === 50001) return { ok: false, code: 'NOT_PERMITTED', error: NOT_PERMITTED_FOR('BanMembers') };
        return { ok: false, ...mapDiscordError(e) };
      }
      markBotBanAction(guild.id, userId, 'unban');
      try {
        await guild.bans.remove(userId, buildAuditReason(why, moderator, source));
      } catch (e) {
        if (e?.code !== 10026) return { ok: false, ...mapDiscordError(e) };
      }
      const cleared = await resolveInfractions(guild.id, userId, ['BAN']);
      await cancelJobs({ dedupeKey: `mod.unban:${guild.id}:${userId}` });
      let infraction;
      try {
        infraction = await recordInfraction({ guildId: guild.id, targetUserId: userId, moderatorId: moderator.id, type: 'UNBAN', reason: why });
      } catch (e) {
        logger.error(`Could not record the unban of ${userId} in ${guild.id}`, e);
        return {
          ok: false,
          code: 'DISCORD_ERROR',
          applied: true,
          error: 'The unban was applied, but saving the case failed. Please record it manually.',
        };
      }
      await sendModLog(guild, infraction, ban.user ?? { id: userId }, moderator, guild.client, { source });
      return { ok: true, infraction, user: ban.user ?? null, cleared };
    });
  } catch (err) {
    logger.error('executeUnban failed', err);
    return { ok: false, code: 'DISCORD_ERROR', error: MSG.INTERNAL };
  }
}

/**
 * Removes a member's timeout and/or mute role and closes stale records. Relief action: no immunity check. Never throws.
 * @param {{ guild: object, target: object, moderator: object, reason?: string|null, source?: string }} input
 * @returns {Promise<{ ok: true, infraction: object, removed: string[], cleared: number, note: string|null }
 *   | { ok: false, code: string, error: string, applied?: true }>}
 */
export async function executeUnmute({ guild, target, moderator, reason = null, source = 'command' } = {}) {
  try {
    if (!guild || !target?.id || !moderator?.id) return { ok: false, code: 'INVALID', error: 'Missing guild, target or moderator.' };
    const why = clampText(String(reason ?? '').trim() || 'No reason provided', REASON_MAX);
    return await targetMutex.run(`${guild.id}:${target.id}`, async () => {
      const me = await getMe(guild);
      if (!me) return { ok: false, code: 'DISCORD_ERROR', error: MSG.NO_ME };
      const settings = await getSettingsSafe(guild.id);
      let member;
      let executor = null;
      try {
        member = (target.guild?.id === guild.id && typeof target.roles === 'object') ? target : await getMemberOrNull(guild, target.id);
        if (byPerson(source) && moderator.id !== guild.client.user.id) {
          executor = await getMemberOrNull(guild, moderator.id);
          if (!executor) return { ok: false, code: 'INVALID', error: MSG.NO_EXECUTOR };
        }
      } catch (e) {
        // Only 10007 means "not in the server": any other failure must not close records and leave the mute in place.
        return { ok: false, ...mapDiscordError(e) };
      }
      const rows = await prisma.infraction.findMany({
        where: { guildId: guild.id, targetUserId: target.id, type: { in: ['TIMEOUT', 'MUTE'] }, active: true },
        select: { id: true },
      });

      const closeRows = async () => {
        if (!rows.length) return 0;
        await prisma.infraction.updateMany({ where: { id: { in: rows.map((r) => r.id) } }, data: { active: false } });
        await mapLimit(rows, 4, (r) => cancelJobs({ dedupeKey: `mod.expire:${r.id}` }));
        return rows.length;
      };
      const record = async (text) => {
        try {
          return await recordInfraction({ guildId: guild.id, targetUserId: target.id, moderatorId: moderator.id, type: 'UNMUTE', reason: text });
        } catch (e) {
          logger.error(`Could not record the unmute of ${target.id} in ${guild.id}`, e);
          return null;
        }
      };
      const user = member?.user ?? (typeof target.send === 'function' ? target : { id: target.id });

      // Not in the guild: only the records can be closed.
      if (!member) {
        if (!rows.length) return { ok: false, code: 'NOT_IN_GUILD', error: MSG.NOT_MEMBER };
        const cleared = await closeRows();
        const infraction = await record(clampText(`${why} (records only)`, REASON_MAX));
        if (!infraction) {
          return { ok: false, code: 'DISCORD_ERROR', applied: true, error: 'The records were closed, but saving the case failed. Please record it manually.' };
        }
        await sendModLog(guild, infraction, user, moderator, guild.client, { source });
        return { ok: true, infraction, removed: [], cleared, note: 'The user is not in the server, so only the records were closed.' };
      }

      // Preflight checks 1-3 and 8; immunity is not checked.
      const err = checkIdentity({ guild, me, targetId: target.id, executor, source }) ?? checkExecutorRank({ guild, member, executor, source });
      if (err) return { ok: false, ...err };

      const timedOut = typeof member.isCommunicationDisabled === 'function' && member.isCommunicationDisabled();
      const roleId = settings?.muteRoleId ?? null;
      const hasRole = !!roleId && member.roles.cache.has(roleId);
      if (!timedOut && !hasRole && !rows.length) return { ok: false, code: 'ALREADY', error: 'That member is not timed out or muted.' };

      // Check everything before acting.
      if (timedOut) {
        if (!me.permissions.has(PermissionFlagsBits.ModerateMembers)) return { ok: false, code: 'NOT_PERMITTED', error: NOT_PERMITTED_FOR('ModerateMembers') };
        if (!member.moderatable) return { ok: false, code: 'HIERARCHY', error: MSG.BOT_ROLE };
      }
      let role = null;
      if (hasRole) {
        if (!me.permissions.has(PermissionFlagsBits.ManageRoles)) return { ok: false, code: 'NOT_PERMITTED', error: NOT_PERMITTED_FOR('ManageRoles') };
        role = guild.roles.cache.get(roleId) ?? null;
        const chk = checkAssignableRole(guild, role, { allowDangerous: true });
        if (!chk.ok) return { ok: false, code: 'HIERARCHY', error: chk.reason };
      }

      const audit = buildAuditReason(why, moderator, source);
      const removed = [];
      try {
        if (timedOut) { await member.timeout(null, audit); removed.push('timeout'); }
        if (hasRole) { await member.roles.remove(roleId, audit); removed.push('mute role'); }
      } catch (e) {
        return { ok: false, ...mapDiscordError(e) };
      }
      const cleared = await closeRows();
      const infraction = await record(why);
      if (!infraction) {
        return { ok: false, code: 'DISCORD_ERROR', applied: true, error: 'The unmute was applied, but saving the case failed. Please record it manually.' };
      }
      await sendModLog(guild, infraction, user, moderator, guild.client, { source });
      return { ok: true, infraction, removed, cleared, note: null };
    });
  } catch (err) {
    logger.error('executeUnmute failed', err);
    return { ok: false, code: 'DISCORD_ERROR', error: MSG.INTERNAL };
  }
}

// ── Ban import and reconciliation (§4.11) ─────────────────────────────────────

const RECONCILE_MARGIN_MS = 2_000;

/**
 * Imports Discord bans that have no active BAN row and closes rows whose ban is gone. Background-safe. Never throws.
 * The reconcile half runs only when the whole ban list was read and the guild is available (00-overview rule 20).
 * @param {import('discord.js').Guild} guild
 * @param {string} botClientId moderator id on imported rows
 * @returns {Promise<{ imported: number, closed: number, complete: boolean }|null>} null = no permission or failure
 */
export async function syncGuildBans(guild, botClientId) {
  try {
    const me = await getMe(guild);
    if (!me?.permissions?.has(PermissionFlagsBits.BanMembers)) return null;
    const startedAt = new Date();
    const banned = new Map();
    let after;
    let complete = false;
    try {
      for (let page = 0; page < 50; page++) {
        const bans = await withTimeout(guild.bans.fetch({ limit: 1000, after, cache: false }), 30_000, 'ban sync page');
        for (const [id, ban] of bans) banned.set(id, ban.reason ?? null);
        if (bans.size < 1000) { complete = true; break; }
        after = maxSnowflake(bans.keys());
      }
    } catch (err) {
      logger.warn(`Ban sync for ${guild.id} could not read the ban list: ${err?.message ?? err}`);
      return null;
    }

    const active = await prisma.infraction.findMany({
      where: { guildId: guild.id, type: 'BAN', active: true },
      select: { id: true, targetUserId: true, createdAt: true },
    });
    const known = new Set(active.map((r) => r.targetUserId));

    // Import
    const toImport = [...banned].filter(([userId]) => !known.has(userId)).map(([userId, reason]) => ({
      targetUserId: userId,
      moderatorId: botClientId,
      type: 'BAN',
      active: true,
      reason: clampText(reason ? `[Imported] ${reason}` : '[Imported] Pre-bot ban — reason unknown', REASON_MAX),
    }));
    let imported = 0;
    for (let i = 0; i < toImport.length; i += 500) imported += await recordInfractionsBulk(guild.id, toImport.slice(i, i + 500));

    // Reconcile
    let closed = 0;
    if (complete && guild.available !== false) {
      // A row younger than the fetch (plus a margin for clock/timer skew between the engine and Node) is never closed.
      const gone = active.filter((r) => r.createdAt.getTime() < startedAt.getTime() - RECONCILE_MARGIN_MS && !banned.has(r.targetUserId));
      if (gone.length) {
        const res = await prisma.infraction.updateMany({ where: { id: { in: gone.map((r) => r.id) } }, data: { active: false } });
        closed = res.count;
        await mapLimit([...new Set(gone.map((r) => r.targetUserId))], 4, (userId) => cancelJobs({ dedupeKey: `mod.unban:${guild.id}:${userId}` }));
      }
    }
    logger.info(`Ban sync for "${guild.name}" (${guild.id}): imported ${imported}, closed ${closed}${complete ? '' : ' (partial: reconcile skipped)'}`);
    return { imported, closed, complete };
  } catch (err) {
    logger.error(`Ban sync failed for ${guild?.id}`, err);
    return null;
  }
}

/**
 * Counts active BAN rows whose expiry passed more than OVERDUE_BAN_LIFT_LIMIT_MS ago (never lifted automatically).
 * Never throws.
 * @param {string} guildId
 * @returns {Promise<number>}
 */
export async function countOverdueBans(guildId) {
  try {
    return await prisma.infraction.count({
      where: { guildId, type: 'BAN', active: true, expiresAt: { not: null, lt: new Date(Date.now() - OVERDUE_BAN_LIFT_LIMIT_MS) } },
    });
  } catch (err) {
    logger.warn(`countOverdueBans failed for ${guildId}: ${err?.message ?? err}`);
    return 0;
  }
}

/**
 * Closes expired TIMEOUT rows and (re)schedules the expiry jobs of every active timed BAN/TIMEOUT/MUTE row.
 * Missing and FAILED jobs are revived. Temp bans overdue by more than 30 days are not scheduled (counted and
 * logged once). Never throws.
 * @param {import('discord.js').Guild} guild
 * @returns {Promise<{ scheduled: number, expired: number, overdue: number }>}
 */
export async function reconcileExpiries(guild) {
  const out = { scheduled: 0, expired: 0, overdue: 0 };
  try {
    const guildId = guild.id;
    const now = new Date();
    // Discord has already lifted these timeouts.
    out.expired = (await prisma.infraction.updateMany({
      where: { guildId, type: 'TIMEOUT', active: true, expiresAt: { lte: now } },
      data: { active: false },
    })).count;

    out.overdue = await countOverdueBans(guildId);
    if (out.overdue) {
      logger.info(`Guild ${guildId}: ${out.overdue} temporary ban(s) expired more than 30 days ago were not lifted automatically`);
    }

    // The overdue bans are excluded in the query so they can never fill the 500 slots.
    const cutoff = new Date(now.getTime() - OVERDUE_BAN_LIFT_LIMIT_MS);
    const rows = await prisma.infraction.findMany({
      where: {
        guildId,
        active: true,
        expiresAt: { not: null },
        OR: [{ type: { in: ['TIMEOUT', 'MUTE'] } }, { type: 'BAN', expiresAt: { gte: cutoff } }],
      },
      select: { id: true, type: true, targetUserId: true, expiresAt: true },
      orderBy: { expiresAt: 'asc' },
      take: 500,
    });

    const keyOf = (row) => (row.type === 'BAN' ? `mod.unban:${guildId}:${row.targetUserId}` : `mod.expire:${row.id}`);
    // One lookup for every row; without it nothing is replaced (a healthy job would lose its place).
    const jobs = rows.length ? await getJobsByKeys(rows.map(keyOf)) : new Map();
    if (!jobs) return out;
    const results = await mapLimit(rows, 4, async (row) => {
      try {
        const key = keyOf(row);
        const existing = jobs.get(key);
        if (existing && existing.status !== 'FAILED') return false; // pending or running
        if (row.type === 'BAN') {
          await scheduleJob({
            guildId,
            type: 'mod.unban',
            runAt: row.expiresAt,
            payload: { userId: row.targetUserId, infractionId: row.id },
            dedupeKey: key,
            maxAttempts: 20,
            onConflict: 'replace',
          });
        } else {
          await scheduleJob({
            guildId,
            type: 'mod.expire',
            runAt: row.expiresAt,
            payload: { infractionId: row.id },
            dedupeKey: key,
            maxAttempts: 20,
            onConflict: 'replace',
          });
        }
        return true;
      } catch (err) {
        logger.error(`reconcileExpiries: could not schedule case row ${row.id} in ${guildId}`, err);
        return false;
      }
    });
    out.scheduled = results.filter((r) => r.status === 'fulfilled' && r.value === true).length;
    logger.debug(`reconcileExpiries ${guildId}: scheduled ${out.scheduled}, expired ${out.expired}, overdue ${out.overdue}`);
  } catch (err) {
    logger.error(`reconcileExpiries failed for ${guild?.id}`, err);
  }
  return out;
}

// ── Job handlers (§4.12) ──────────────────────────────────────────────────────

const PERM_RETRY_MS = 3_600_000;
const PERM_RETRY_MAX = 168; // 7 days of hourly retries

/** Hourly back-off while a permission is missing; a permanent failure after 7 days. */
function permBackoff(job, what) {
  const permRetries = Number(job.payload?.permRetries) || 0;
  if (permRetries >= PERM_RETRY_MAX) throw new PermanentJobError(`${job.type}: missing ${what} for 7 days`);
  logger.warn(`${job.type} for guild ${job.guildId}: missing ${what}; retrying in 1 hour (${permRetries + 1}/${PERM_RETRY_MAX})`);
  return { nextRunAt: new Date(Date.now() + PERM_RETRY_MS), payload: { ...(job.payload ?? {}), permRetries: permRetries + 1 } };
}

/**
 * Scheduler handler for `mod.unban`: lifts a temporary ban at its expiry. Runs regardless of the module toggle.
 * Idempotent. Throws to make the scheduler retry; a `PermanentJobError` ends the job.
 * @param {{ type: string, guildId: string, payload: object }} job
 * @param {{ client: import('discord.js').Client, guild: import('discord.js').Guild }} ctx
 * @returns {Promise<void|{ nextRunAt: Date, payload: object }>}
 */
export async function runUnbanJob(job, { client, guild }) {
  const { userId, infractionId = null } = job.payload ?? {};
  if (!SNOWFLAKE_RE.test(String(userId ?? ''))) throw new PermanentJobError('mod.unban: bad payload');
  let inf = null;
  if (infractionId !== null && infractionId !== undefined) {
    inf = await prisma.infraction.findUnique({ where: { id: infractionId } });
    if (!inf || inf.guildId !== guild.id || !inf.active) return; // already lifted or resolved
    if (inf.expiresAt && inf.expiresAt.getTime() > Date.now() + 5_000) return { nextRunAt: inf.expiresAt }; // ran early
  }
  const me = await getMe(guild);
  if (!me) throw new Error('mod.unban: could not load the bot member');
  if (!me.permissions.has(PermissionFlagsBits.BanMembers)) return permBackoff(job, 'Ban Members');
  markBotBanAction(guild.id, userId, 'unban');
  const text = `Temporary ban expired${inf ? ` (case #${inf.caseId})` : ''}`;
  try {
    await guild.bans.remove(userId, clampText(text, 512));
  } catch (err) {
    if (err?.code === 50013 || err?.code === 50001) return permBackoff(job, 'Ban Members');
    if (err?.code !== 10026) throw err; // 10026: already unbanned, carry on
  }
  await resolveInfractions(guild.id, userId, ['BAN']); // before the record, so a second run is a no-op
  await createInfraction(guild, { id: userId }, client.user, 'UNBAN', { reason: text, silent: true, skipEscalation: true, source: 'expiry' });
  return undefined;
}

/**
 * Scheduler handler for `mod.expire`: closes an expired TIMEOUT / MUTE row (removing the mute role for MUTE).
 * Runs regardless of the module toggle. Idempotent. Throws to make the scheduler retry.
 * @param {{ type: string, guildId: string, payload: object }} job
 * @param {{ client: import('discord.js').Client, guild: import('discord.js').Guild }} ctx
 * @returns {Promise<void|{ nextRunAt: Date, payload?: object }>}
 */
export async function runExpireJob(job, { client, guild }) {
  const { infractionId, roleId = null } = job.payload ?? {};
  if (!Number.isInteger(infractionId)) throw new PermanentJobError('mod.expire: bad payload');
  const inf = await prisma.infraction.findUnique({ where: { id: infractionId } });
  if (!inf || inf.guildId !== guild.id || !inf.active) return;
  if (inf.expiresAt && inf.expiresAt.getTime() > Date.now() + 5_000) return { nextRunAt: inf.expiresAt }; // early, or the mute was extended

  if (inf.type === 'TIMEOUT') {
    // Discord lifts the timeout itself; no new case.
    await prisma.infraction.updateMany({ where: { id: inf.id, active: true }, data: { active: false } });
    return undefined;
  }

  if (inf.type === 'MUTE') {
    const rid = roleId ?? (await getSettingsSafe(guild.id))?.muteRoleId ?? null;
    // null only for 10007 / 10013; any other error is rethrown so the scheduler retries (the member may still hold the role)
    const member = await getMemberOrNull(guild, inf.targetUserId);
    if (member && rid && member.roles.cache.has(rid)) {
      const me = await getMe(guild);
      if (!me) throw new Error('mod.expire: could not load the bot member');
      const role = guild.roles.cache.get(rid) ?? null;
      if (!me.permissions.has(PermissionFlagsBits.ManageRoles) || (role && !checkAssignableRole(guild, role, { allowDangerous: true }).ok)) {
        return permBackoff(job, 'Manage Roles (or a role above the mute role)');
      }
      try {
        await member.roles.remove(rid, clampText(`Mute expired (case #${inf.caseId})`, 512));
      } catch (err) {
        if (err?.code !== 10011 && err?.code !== 10007) throw err; // role deleted / member left: continue
      }
    }
    const { count } = await prisma.infraction.updateMany({ where: { id: inf.id, active: true }, data: { active: false } });
    if (count === 1) {
      await createInfraction(guild, { id: inf.targetUserId }, client.user, 'UNMUTE', {
        reason: `Mute expired (case #${inf.caseId})`, silent: true, skipEscalation: true, source: 'expiry',
      });
    }
    return undefined;
  }

  await prisma.infraction.updateMany({ where: { id: inf.id, active: true }, data: { active: false } });
  return undefined;
}

// ── Mute re-apply on rejoin (amendments item 3) ───────────────────────────────

/**
 * guildMemberAdd hook body: gives a rejoining member their mute role back while a role mute is still active.
 * No new case, no DM, no mod-log post. Never throws.
 * @param {{ guild: import('discord.js').Guild, member: import('discord.js').GuildMember, getSettings: Function }} ctx
 * @returns {Promise<void>}
 */
export async function reapplyMuteOnJoin(ctx) {
  try {
    const { guild, member } = ctx;
    if (!guild || !member) return;
    const settings = await ctx.getSettings();
    if (!settings?.muteRoleId) return;
    const role = guild.roles.cache.get(settings.muteRoleId);
    if (!role) return;
    const inf = await prisma.infraction.findFirst({
      where: {
        guildId: guild.id,
        targetUserId: member.id,
        type: 'MUTE',
        active: true,
        OR: [{ expiresAt: null }, { expiresAt: { gt: new Date() } }],
      },
      orderBy: { createdAt: 'desc' },
    });
    if (!inf) return;
    const chk = checkAssignableRole(guild, role);
    if (!chk.ok) {
      if (muteWarnThrottle(guild.id)) logger.warn(`Could not re-apply the mute role in ${guild.id}: ${chk.reason}`);
      return;
    }
    await member.roles.add(role, clampText(`Re-applied mute after rejoin (case #${inf.caseId})`, 512));
  } catch (err) {
    logger.warn(`reapplyMuteOnJoin failed: ${err?.message ?? err}`);
  }
}

// ── External ban / unban hooks (§4.13) ────────────────────────────────────────

/**
 * Finds a recent audit-log entry for a target (waits 1.5 s: entries lag the gateway event). Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {number} type AuditLogEvent
 * @param {string} targetId
 * @param {number} [windowMs=30000]
 * @returns {Promise<import('discord.js').GuildAuditLogsEntry|null>}
 */
export async function findAuditEntry(guild, type, targetId, windowMs = 30_000) {
  try {
    if (!guild.members.me?.permissions.has(PermissionFlagsBits.ViewAuditLog)) return null;
    await sleep(1500);
    const logs = await guild.fetchAuditLogs({ type, limit: 6 }).catch(() => null);
    return logs?.entries.find((e) => e.targetId === targetId && Date.now() - e.createdTimestamp <= windowMs) ?? null;
  } catch {
    return null;
  }
}

/**
 * guildBanAdd hook body: records a ban made outside the bot as an `[External]` BAN case. The cheap checks run
 * inline; the audit-log lookup and the record are detached so the hook chain is never held up. Never throws.
 * @param {{ client: import('discord.js').Client, guild: import('discord.js').Guild, ban: object, user: import('discord.js').User }} ctx
 * @returns {Promise<void>}
 */
export async function recordExternalBan({ client, guild, ban, user }) {
  try {
    if (wasBotBanAction(guild.id, user.id, 'ban')) return;
    const hasRow = () => prisma.infraction.findFirst({
      where: { guildId: guild.id, targetUserId: user.id, type: 'BAN', active: true },
      select: { id: true },
    });
    if (await hasRow()) return;
    void (async () => {
      const entry = await findAuditEntry(guild, AuditLogEvent.MemberBanAdd, user.id);
      if (entry?.executorId === client.user.id) return; // our own ban, e.g. after a restart lost the marker
      if (await hasRow()) return; // our own record may have landed during the delay
      const banReason = entry?.reason ?? (ban?.partial ? (await ban.fetch().catch(() => null))?.reason : ban?.reason) ?? null;
      const moderator = entry?.executor ?? (entry?.executorId ? { id: entry.executorId } : client.user);
      await createInfraction(guild, user, moderator, 'BAN', {
        reason: `[External] ${banReason ?? 'No reason provided'}${entry ? '' : ' (moderator unknown)'}`,
        silent: true,
        skipEscalation: true,
        source: 'external',
      });
    })().catch((err) => logger.error('recordExternalBan failed', err));
  } catch (err) {
    logger.error('recordExternalBan failed', err);
  }
}

/**
 * guildBanRemove hook body: closes BAN rows and cancels the temp-ban job (always, even when the module is off), and
 * records an `[External]` UNBAN case when the module is on. Never throws.
 * @param {{ client: import('discord.js').Client, guild: import('discord.js').Guild, user: import('discord.js').User, settings: object|null }} input
 * @returns {Promise<void>}
 */
export async function handleExternalUnban({ client, guild, user, settings }) {
  try {
    const count = await resolveInfractions(guild.id, user.id, ['BAN']);
    await cancelJobs({ dedupeKey: `mod.unban:${guild.id}:${user.id}` });
    if (count === 0 || wasBotBanAction(guild.id, user.id, 'unban') || !isModuleEnabled(settings, 'moderation')) return;
    void (async () => {
      const entry = await findAuditEntry(guild, AuditLogEvent.MemberBanRemove, user.id);
      if (entry?.executorId === client.user.id) return;
      const moderator = entry?.executor ?? (entry?.executorId ? { id: entry.executorId } : client.user);
      await createInfraction(guild, user, moderator, 'UNBAN', {
        reason: `[External] ${entry?.reason ?? 'No reason provided'}${entry ? '' : ' (moderator unknown)'}`,
        silent: true,
        skipEscalation: true,
        source: 'external',
      });
    })().catch((err) => logger.error('handleExternalUnban failed', err));
  } catch (err) {
    logger.error('handleExternalUnban failed', err);
  }
}

// ── Reply helpers (§4.14) ─────────────────────────────────────────────────────

/**
 * Applies `modVisibility` to a successful action reply. The commands defer ephemerally first. Never throws.
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @param {{ modVisibility?: string }|null} settings
 * @param {{ title: string, lines: string[], warnings?: string[] }} reply
 * @returns {Promise<void>}
 */
export async function deliverModReply(interaction, settings, { title, lines, warnings = [] }) {
  try {
    const body = clampText(lines.join('\n'), 4000);
    const vis = settings?.modVisibility ?? 'PUBLIC';
    const withWarnings = (extra = '') => clampText([body, ...warnings.map((w) => `⚠️ ${w}`)].join('\n') + extra, 4096);
    if (vis === 'PRIVATE') {
      await interaction.editReply({ embeds: [successEmbed(title, withWarnings())] });
    } else if (vis === 'ANONYMOUS') {
      // The public post must not reveal the invoker, so it is a channel message, not an interaction follow-up.
      if (checkChannelUsable(interaction.channel).ok) {
        try {
          await interaction.channel.send({ embeds: [successEmbed(title, body)], allowedMentions: { parse: [] } });
          await interaction.editReply({ embeds: [successEmbed(title, withWarnings('\n\n*Posted in this channel without your name.*'))] });
        } catch (err) {
          if (isInteractionGone(err)) throw err;
          const more = [...warnings, "I couldn't post the public notice here."];
          await interaction.editReply({ embeds: [successEmbed(title, clampText([body, ...more.map((w) => `⚠️ ${w}`)].join('\n'), 4096))] });
        }
      } else {
        await interaction.editReply({ embeds: [successEmbed(title, withWarnings("\n\n*I couldn't post the public notice here (missing permissions).*"))] });
      }
    } else {
      await interaction.deleteReply().catch(() => {});
      await interaction.followUp({ embeds: [successEmbed(title, body)], allowedMentions: { parse: [] } });
      if (warnings.length) {
        await interaction.followUp({
          embeds: [warnEmbed('Note', clampText(warnings.map((w) => `• ${w}`).join('\n'), 4000))],
          ephemeral: true,
        });
      }
    }
  } catch (err) {
    if (!isInteractionGone(err)) logger.error('deliverModReply failed', err);
  }
}

/**
 * Lookup commands (/case): PUBLIC -> deleteReply + followUp(payload); ANONYMOUS / PRIVATE -> editReply(payload)
 * (the deferred reply is ephemeral). Never throws.
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @param {{ modVisibility?: string }|null} settings
 * @param {object} payload message payload
 * @returns {Promise<void>}
 */
export async function deliverLookup(interaction, settings, payload) {
  try {
    if ((settings?.modVisibility ?? 'PUBLIC') === 'PUBLIC') {
      await interaction.deleteReply().catch(() => {});
      await interaction.followUp(payload);
    } else {
      await interaction.editReply(payload);
    }
  } catch (err) {
    if (!isInteractionGone(err)) logger.error('deliverLookup failed', err);
  }
}

/**
 * editReply an error result (always ephemeral, because commands defer ephemerally). Never throws.
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @param {{ code?: string, error: string, applied?: boolean }} res
 * @returns {Promise<void>}
 */
export async function replyModError(interaction, res) {
  try {
    const embed = res?.applied
      ? warnEmbed('Done, But Not Recorded', res.error)
      : errorEmbed(ERROR_TITLES[res?.code] ?? 'Action Failed', res?.error ?? MSG.INTERNAL);
    await interaction.editReply({ embeds: [embed] });
  } catch (err) {
    if (!isInteractionGone(err)) logger.error('replyModError failed', err);
  }
}
