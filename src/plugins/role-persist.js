// Plugin `role-persist`: when a member leaves, their roles (and nickname) are saved; when they rejoin within
// rolePersistDays the roles that are still safe to give are given back, one single-role call each with an audit-log reason.
// Never bypasses Rules Screening or the captcha: while either is open the restore waits and runs from guildMemberUpdate
// once it clears. Rules and table access: src/services/rolePersistService.js; admin command: /rolepersist.
import { PermissionFlagsBits } from 'discord.js';
import { peekSettings, updateSettingsWith } from '../services/settingsService.js';
import {
  ROLEPERSIST, runLocked, loadRow, saveRow, deleteRow, pruneExpired, snapshotMember, planRoles, gateOf,
  isStale, isWaiting, markWaiting, clearWaiting,
} from '../services/rolePersistService.js';
import { createThrottle } from '../utils/async.js';
import logger from '../utils/logger.js';

export const AUDIT_REASON = 'Role Persistence: roles from before the member left';

// "Unknown member" and "Unknown user": the member is gone again, nothing to restore.
const LEFT_CODES = new Set([10007, 10013]);
// "Unknown role" and "Missing permissions": this role cannot be given; the others still can.
const ROLE_FAIL_CODES = new Set([10011, 50013]);

const warnBlocked = createThrottle(3_600_000);
const partialLook = createThrottle(60_000);

// ── Filters (sync) ────────────────────────────────────────────────────────────

/** guildMemberRemove: people only (bots keep managed roles, which are never saved anyway). */
export function leaveFilter(ctx) {
  return Boolean(ctx.user) && ctx.user.bot !== true;
}

/** guildMemberAdd: people only. */
export function joinFilter(ctx) {
  return Boolean(ctx.member) && ctx.member.user?.bot !== true;
}

/**
 * guildMemberUpdate: only an update that may have cleared a gate is looked at, so ordinary role and nickname changes
 * cost nothing. That is a member we are waiting for, a Rules Screening pass, the captcha role being gained, or (the
 * old member was not cached, so nothing can be diffed) a recent joiner, at most once a minute each.
 * @param {object} ctx HookContext with `oldMember`, `newMember`, `oldKnown`
 */
export function gateMayHaveCleared(ctx) {
  const n = ctx.newMember;
  if (!n || n.user?.bot === true) return false;
  if (isWaiting(ctx.guildId, n.id)) return true;
  if (ctx.oldKnown === true) {
    const o = ctx.oldMember;
    if (o?.pending === true && n.pending !== true) return true;
    const settings = peekSettings(ctx.guildId);
    if (!settings) return (n.roles?.cache?.size ?? 0) > (o?.roles?.cache?.size ?? 0);
    const gateId = settings.captchaEnabled === true ? settings.captchaRoleId : null;
    return Boolean(gateId) && !o?.roles?.cache?.has(gateId) && Boolean(n.roles?.cache?.has(gateId));
  }
  const joined = n.joinedTimestamp;
  if (!joined || Date.now() - joined > ROLEPERSIST.RECENT_JOIN_MS || n.pending === true) return false;
  return partialLook(`${ctx.guildId}:${n.id}`);
}

/** roleDelete: skipped once the memoised settings show the role is not on the ignore list. */
export function roleDeleteFilter(ctx) {
  const s = peekSettings(ctx.guildId);
  return !s || (s.rolePersistIgnoredRoleIds ?? []).includes(ctx.role?.id);
}

// ── Leave ─────────────────────────────────────────────────────────────────────

/**
 * guildMemberRemove: saves the member's roles and nickname (upsert), then prunes this guild's expired rows (at most once
 * an hour). Nothing is saved for a partial member with no known roles, or when there is nothing to give back later:
 * an older row is then kept. When an older row is still there (the member left again before the captcha or Rules
 * Screening let the restore run) the new roles are merged into it, so the roles saved the first time are kept.
 */
export async function onLeave(ctx) {
  const { member, user, guild, guildId } = ctx;
  clearWaiting(guildId, user.id);
  const settings = await ctx.getSettings();
  if (!settings) return;
  const snap = snapshotMember(member, guild, settings);
  if (snap) await runLocked(guildId, user.id, () => saveRow(guildId, user.id, snap, new Date(), settings));
  await pruneExpired(guildId, settings);
}

// ── Rejoin ────────────────────────────────────────────────────────────────────

function canSetNickname(guild, member) {
  try {
    return Boolean(guild.members?.me?.permissions?.has(PermissionFlagsBits.ManageNicknames)) && member.manageable === true;
  } catch {
    return false;
  }
}

// Gives the roles one at a time, each with its own single-role PUT. The array form PATCHes the member's whole role list
// from the cache, so a role another hook added in between (the captcha's autoroles, say) would be taken off again; a
// single-role PUT only ever adds. A role that cannot be given is skipped and the rest still go. Returns the ids given,
// or 'left' / 'failed'.
async function addRoles(target, ids) {
  const given = [];
  for (const id of ids) {
    try {
      await target.roles.add(id, AUDIT_REASON);
      given.push(id);
    } catch (err) {
      if (LEFT_CODES.has(err?.code)) return 'left';
      if (!ROLE_FAIL_CODES.has(err?.code)) {
        logger.warn(`[role-persist] ${target.guild.id}: could not give role ${id} to ${target.id}: ${err?.message}`);
        return 'failed';
      }
    }
  }
  return given;
}

async function restoreLocked(member, settings, { captchaPending, now }) {
  const guild = member.guild;
  const guildId = guild.id;
  const userId = member.id;

  const row = await loadRow(guildId, userId);
  if (!row) {
    clearWaiting(guildId, userId);
    return { status: 'none' };
  }
  if (isStale(row, settings, now)) {
    await deleteRow(guildId, userId);
    clearWaiting(guildId, userId);
    return { status: 'stale' };
  }
  const gate = gateOf(member, settings, { captchaPending });
  if (gate) {
    markWaiting(guildId, userId, now);
    return { status: 'waiting', gate };
  }

  const plan = planRoles(guild, row.roleIds, settings, { held: member.roles.cache });
  if (row.roleIds.length && !guild.members?.me?.permissions?.has(PermissionFlagsBits.ManageRoles)) {
    // Fixable by an admin: keep the row (it still expires on its own).
    if (warnBlocked(guildId)) logger.warn(`[role-persist] ${guildId}: cannot restore roles, the bot lacks Manage Roles`);
    return { status: 'blocked' };
  }

  let given = [];
  if (plan.give.length) {
    const res = await addRoles(member, plan.give.map((r) => r.id));
    if (res === 'left') return { status: 'left' };
    if (res === 'failed') return { status: 'failed' };
    given = res;
  }

  let nickname = false;
  if (settings.rolePersistNicknames === true && row.nickname && !member.nickname && canSetNickname(guild, member)) {
    try {
      await member.setNickname(row.nickname, AUDIT_REASON);
      nickname = true;
    } catch (err) {
      if (LEFT_CODES.has(err?.code)) return { status: 'left' };
      logger.warn(`[role-persist] ${guildId}: could not restore the nickname of ${userId}: ${err?.message}`);
    }
  }

  await deleteRow(guildId, userId);
  clearWaiting(guildId, userId);
  if (given.length || nickname) {
    logger.info(`[role-persist] ${guildId}: restored ${given.length} role(s)${nickname ? ' and the nickname' : ''} for ${userId}`);
  }
  return { status: 'restored', given, skipped: plan.skipped, nickname };
}

/**
 * Gives a rejoining member their saved roles (and nickname) back, when a fresh row exists and no gate is open. Stale rows
 * are deleted. The row is deleted after a successful restore; a failed or blocked restore keeps it. Never throws.
 * @param {import('discord.js').GuildMember} member
 * @param {object} settings GuildSettings row
 * @param {{ captchaPending?: boolean, now?: number }} [opts]
 * @returns {Promise<{ status: 'none'|'stale'|'waiting'|'blocked'|'left'|'failed'|'restored', gate?: string,
 *   given?: string[], skipped?: object[], nickname?: boolean }>}
 */
export async function restoreMember(member, settings, { captchaPending = false, now = Date.now() } = {}) {
  try {
    return await runLocked(member.guild.id, member.id, () => restoreLocked(member, settings, { captchaPending, now }));
  } catch (err) {
    logger.error(`[role-persist] ${member?.guild?.id}: restore failed for ${member?.id}`, err);
    return { status: 'failed' };
  }
}

/** guildMemberAdd (after welcome and captcha): restores unless a gate is open. */
export async function onJoin(ctx) {
  const settings = await ctx.getSettings();
  if (!settings) return;
  await restoreMember(ctx.member, settings, { captchaPending: ctx.state?.captchaPending === true });
}

/** guildMemberUpdate: a gate (Rules Screening, captcha) may have cleared, so try the restore again. */
export async function onUpdate(ctx) {
  const settings = await ctx.getSettings();
  if (!settings) return;
  await restoreMember(ctx.newMember, settings);
}

// ── Role cleanup ──────────────────────────────────────────────────────────────

/** roleDelete: drops the role from the never-give-back list (saved rows just skip missing roles when restoring). */
export async function onRoleDelete(ctx) {
  const roleId = ctx.role?.id;
  const settings = await ctx.getSettings();
  if (!roleId || !(settings?.rolePersistIgnoredRoleIds ?? []).includes(roleId)) return;
  await updateSettingsWith(ctx.guildId, (cur) => ((cur.rolePersistIgnoredRoleIds ?? []).includes(roleId)
    ? { rolePersistIgnoredRoleIds: cur.rolePersistIgnoredRoleIds.filter((id) => id !== roleId) }
    : null));
}

export const plugin = {
  id: 'role-persist',
  module: 'rolepersist',
  hooks: [
    { event: 'guildMemberRemove', id: 'role-persist', filter: leaveFilter, run: onLeave },
    { event: 'guildMemberAdd', id: 'role-persist', filter: joinFilter, run: onJoin, timeoutMs: 20_000 },
    { event: 'guildMemberUpdate', id: 'role-persist', filter: gateMayHaveCleared, run: onUpdate, timeoutMs: 20_000 },
    // Cleanup runs even while the module is off, so a deleted role never lingers on the ignore list.
    { event: 'roleDelete', id: 'role-persist', always: true, filter: roleDeleteFilter, run: onRoleDelete },
  ],
  components: [],
  jobs: [],
  provides: {},
};
