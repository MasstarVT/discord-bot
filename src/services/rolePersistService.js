// Role Persistence: a member who leaves has their roles (and nickname) saved in PersistedMember; when they rejoin within
// GuildSettings.rolePersistDays the roles that are still safe to give are given back. This file holds the table access
// and the rules (what to save, what to give back, which gates hold a restore); the hooks and the restore itself are in
// src/plugins/role-persist.js, the admin command is src/commands/admin/rolepersist.js.
import prisma from '../database/client.js';
import { ensureGuildSettings } from './settingsService.js';
import { isCaptchaGated } from './welcomeService.js';
import { checkAssignableRole } from '../utils/interactionHelpers.js';
import { createKeyedMutex } from '../utils/async.js';
import { clampText } from '../utils/text.js';
import logger from '../utils/logger.js';

export const ROLEPERSIST = Object.freeze({
  DEFAULT_DAYS: 90,
  MIN_DAYS: 1,
  MAX_DAYS: 365,
  DAY_MS: 86_400_000,
  // Expired rows of a guild are deleted at most this often (when a member leaves).
  PRUNE_EVERY_MS: 3_600_000,
  NICKNAME_MAX: 32,
  // Members whose restore is waiting for Rules Screening / the captcha (in memory; see markWaiting).
  WAIT_MAX: 5000,
  WAIT_TTL_MS: 7 * 86_400_000,
  // A member update with an uncached old member is only looked at for members who joined this recently.
  RECENT_JOIN_MS: 86_400_000,
});

// One member's save / restore / clear run one at a time, so a join and a gate-clearing update never restore twice.
const lock = createKeyedMutex();

/** Runs `fn` while holding the member's lock. */
export function runLocked(guildId, userId, fn) {
  return lock.run(`${guildId}:${userId}`, fn);
}

// ── Settings ──────────────────────────────────────────────────────────────────

/** How many days a saved row stays valid (1..365). */
export function retentionDays(settings) {
  const d = Number(settings?.rolePersistDays);
  if (!Number.isFinite(d)) return ROLEPERSIST.DEFAULT_DAYS;
  return Math.min(ROLEPERSIST.MAX_DAYS, Math.max(ROLEPERSIST.MIN_DAYS, Math.trunc(d)));
}

/** When a row saved at `leftAt` stops being valid, as epoch ms. */
export function expiresAt(row, settings) {
  return new Date(row.leftAt).getTime() + retentionDays(settings) * ROLEPERSIST.DAY_MS;
}

/** True when the row is older than the retention window. */
export function isStale(row, settings, now = Date.now()) {
  return expiresAt(row, settings) <= now;
}

// ── Table ─────────────────────────────────────────────────────────────────────

/** The saved row of a member, or null. */
export function loadRow(guildId, userId) {
  return prisma.persistedMember.findUnique({ where: { guildId_userId: { guildId, userId } } });
}

/** Number of saved members in the guild. */
export function countRows(guildId) {
  return prisma.persistedMember.count({ where: { guildId } });
}

/** Deletes the member's row; returns how many rows went (0 or 1). Never fails on a missing row. */
export async function deleteRow(guildId, userId) {
  const res = await prisma.persistedMember.deleteMany({ where: { guildId, userId } });
  return res?.count ?? 0;
}

/**
 * Folds a new snapshot into a row that is still waiting to be restored: the role ids of both (no duplicates, the saved
 * ones first), the newer nickname when one is known and the older one otherwise. A row is only consumed by a successful
 * restore, so one that still exists belongs to a member who left again before it ran (captcha, Rules Screening): the
 * few roles they hold now must not replace the ones saved the first time.
 * @param {{ roleIds?: string[], nickname?: string|null }|null} old the row that exists, if any
 * @param {{ roleIds: string[], nickname: string|null }} snap
 * @returns {{ roleIds: string[], nickname: string|null }}
 */
export function mergeSnapshot(old, snap) {
  if (!old) return { roleIds: [...snap.roleIds], nickname: snap.nickname ?? null };
  return {
    roleIds: [...new Set([...(old.roleIds ?? []), ...snap.roleIds])],
    nickname: snap.nickname ?? old.nickname ?? null,
  };
}

/**
 * Saves the member's row: created, or merged into the one that is still there (see mergeSnapshot), with `leftAt`
 * refreshed. An expired row is replaced, not merged, so old roles never outlive the retention window. The caller holds
 * the member's lock (runLocked), because this reads the row before writing it.
 * @param {string} guildId
 * @param {string} userId
 * @param {{ roleIds: string[], nickname: string|null }} snap
 * @param {Date} [now]
 * @param {object|null} [settings] GuildSettings row; with it an expired row is not merged
 */
export async function saveRow(guildId, userId, snap, now = new Date(), settings = null) {
  await ensureGuildSettings(guildId);
  let old = await loadRow(guildId, userId);
  if (old && settings && isStale(old, settings, now.getTime())) old = null;
  const merged = mergeSnapshot(old, snap);
  const data = { roleIds: merged.roleIds, nickname: merged.nickname, leftAt: now };
  await prisma.persistedMember.upsert({
    where: { guildId_userId: { guildId, userId } },
    create: { guildId, userId, ...data },
    update: data,
  });
}

const pruned = new Map(); // guildId -> epoch ms of the last prune

/**
 * Deletes this guild's expired rows, at most once per guild per hour (in memory). Never throws.
 * @returns {Promise<number|null>} rows deleted, or null when it was skipped or failed
 */
export async function pruneExpired(guildId, settings, now = Date.now()) {
  const last = pruned.get(guildId);
  if (last !== undefined && now - last < ROLEPERSIST.PRUNE_EVERY_MS) return null;
  pruned.set(guildId, now); // set first: a failing delete is not retried on every leave
  try {
    const cutoff = new Date(now - retentionDays(settings) * ROLEPERSIST.DAY_MS);
    const res = await prisma.persistedMember.deleteMany({ where: { guildId, leftAt: { lt: cutoff } } });
    return res?.count ?? 0;
  } catch (err) {
    logger.warn(`[role-persist] ${guildId}: could not prune old rows: ${err?.message}`);
    return null;
  }
}

/** Forgets the prune stamps (tests). */
export function resetPruneStamps() {
  pruned.clear();
}

// ── What to save ──────────────────────────────────────────────────────────────

/**
 * The roles and nickname to save for a member who left, or null when there is nothing worth saving (so an older row is
 * kept). A partial member (not cached, roles unknown) is never saved. @everyone, managed roles, roles that no longer
 * exist and ignored roles are left out.
 * @param {import('discord.js').GuildMember} member
 * @param {import('discord.js').Guild} guild
 * @param {object} settings
 * @returns {{ roleIds: string[], nickname: string|null }|null}
 */
export function snapshotMember(member, guild, settings) {
  const held = member?.roles?.cache;
  if (!held) return null;
  if (member.partial === true && ![...held.keys()].some((id) => id !== guild.id)) return null;
  const ignored = new Set(settings?.rolePersistIgnoredRoleIds ?? []);
  const roleIds = [];
  for (const id of held.keys()) {
    if (id === guild.id || ignored.has(id)) continue;
    const role = guild.roles.cache.get(id);
    if (!role || role.managed) continue;
    roleIds.push(id);
  }
  const nick = typeof member.nickname === 'string' ? member.nickname.trim() : '';
  const nickname = nick ? clampText(nick, ROLEPERSIST.NICKNAME_MAX, '') : null;
  if (!roleIds.length && !nickname) return null;
  return { roleIds, nickname };
}

// ── What to give back ─────────────────────────────────────────────────────────

/**
 * Splits saved role ids into the roles to give back and the ones skipped (with a reason). A role is given back when it
 * still exists, is not ignored, is not the captcha role (never a way around the captcha), is not already held and
 * passes checkAssignableRole (not managed, below the bot's highest role, no dangerous permission).
 * @param {import('discord.js').Guild} guild
 * @param {string[]} roleIds
 * @param {object} settings
 * @param {{ held?: { has(id: string): boolean }|null }} [opts]
 * @returns {{ give: import('discord.js').Role[], skipped: { roleId: string, reason: string, deleted?: boolean }[] }}
 */
export function planRoles(guild, roleIds, settings, { held = null } = {}) {
  const ignored = new Set(settings?.rolePersistIgnoredRoleIds ?? []);
  const gateRoleId = settings?.captchaEnabled === true ? settings?.captchaRoleId ?? null : null;
  const give = [];
  const skipped = [];
  for (const id of new Set(roleIds ?? [])) {
    if (id === guild.id) continue;
    const role = guild.roles.cache.get(id);
    if (!role) {
      skipped.push({ roleId: id, reason: 'The role no longer exists.', deleted: true });
      continue;
    }
    if (ignored.has(id)) {
      skipped.push({ roleId: id, reason: 'The role is on the never-give-back list.' });
      continue;
    }
    if (gateRoleId && id === gateRoleId) {
      skipped.push({ roleId: id, reason: 'The captcha role is only given by the captcha.' });
      continue;
    }
    if (held?.has(id)) continue;
    const chk = checkAssignableRole(guild, role, { allowDangerous: false });
    if (chk.ok) give.push(role);
    else skipped.push({ roleId: id, reason: chk.reason ?? 'The role cannot be given.' });
  }
  return { give, skipped };
}

// ── Gates ─────────────────────────────────────────────────────────────────────

/**
 * What holds a restore back: 'pending' (Rules Screening not passed), 'captcha' (the captcha role exists, captcha is on
 * and the member does not have it yet), or null. Never bypasses either gate.
 * @param {import('discord.js').GuildMember} member
 * @param {object} settings
 * @param {{ captchaPending?: boolean }} [opts] the captcha join hook's flag
 * @returns {'pending'|'captcha'|null}
 */
export function gateOf(member, settings, { captchaPending = false } = {}) {
  if (member?.pending === true) return 'pending';
  if (captchaPending === true || isCaptchaGated(member, settings)) return 'captcha';
  return null;
}

// Members whose roles are waiting for a gate, so their later updates are noticed without a lookup per update.
const waiting = new Map(); // `${guildId}:${userId}` -> expiry (epoch ms)
const waitKey = (guildId, userId) => `${guildId}:${userId}`;

/** Remembers that a member's restore waits for a gate (bounded: the oldest entry goes first, entries expire). */
export function markWaiting(guildId, userId, now = Date.now()) {
  const key = waitKey(guildId, userId);
  waiting.delete(key);
  while (waiting.size >= ROLEPERSIST.WAIT_MAX) waiting.delete(waiting.keys().next().value);
  waiting.set(key, now + ROLEPERSIST.WAIT_TTL_MS);
}

/** Forgets a waiting member. */
export function clearWaiting(guildId, userId) {
  waiting.delete(waitKey(guildId, userId));
}

/** True while the member is waiting for a gate. */
export function isWaiting(guildId, userId, now = Date.now()) {
  const key = waitKey(guildId, userId);
  const until = waiting.get(key);
  if (until === undefined) return false;
  if (until <= now) {
    waiting.delete(key);
    return false;
  }
  return true;
}

/** Empties the waiting set (tests). */
export function resetWaiting() {
  waiting.clear();
}
