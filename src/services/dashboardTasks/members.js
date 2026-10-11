// Member XP changes the web dashboard asks the bot to do (its `levels` list, src/dashboard/collections/levels.js):
//   xp.adjust   { op: 'give'|'take'|'set'|'reset', userId, amount? }: /xp give (one member) | remove | set | reset
// The same xpService calls as /xp: one atomic SQL statement per change (clamped to XP_MAX), then handleLevelChange (the
// stored level, reward roles, the `levelUp` hook; announce off: admin changes never post a level-up message), and for a
// reset the role sync on demotion. Like /xp (gated = false, ADMIN), it works while the Levels module is off.
// Coins need no task: their writes are Discord-free, so the dashboard's `balances` store calls economyService directly.
import { PERMISSION_LEVELS, XP_MAX } from '../../config/constants.js';
import { getSettingsSafe } from '../settingsService.js';
import { applyXpDelta, handleLevelChange, resetXp, setXp, subtractXp, syncLevelRoles } from '../xpService.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import { TaskError } from './shared.js';

export const XP_OPS = Object.freeze(['give', 'take', 'set', 'reset']);
/** /xp give and /xp remove: 1 to 1,000,000; /xp set: 0 to XP_MAX. */
export const XP_STEP_MAX = 1_000_000;
const SNOWFLAKE_RE = /^\d{17,20}$/;

const fmt = (n) => Number(n ?? 0).toLocaleString('en-US');

/**
 * Re-validates an `xp.adjust` payload.
 * @returns {{ op: string, userId: string, amount: number|null }}
 * @throws {TaskError}
 */
export function readXpPayload(payload) {
  if (payload === null || typeof payload !== 'object' || Array.isArray(payload)) throw new TaskError('The request was malformed.');
  const { op } = payload;
  if (!XP_OPS.includes(op)) throw new TaskError('Unknown XP change.');
  const userId = String(payload.userId ?? '');
  if (!SNOWFLAKE_RE.test(userId)) throw new TaskError('Pick a member, or paste a user ID.');
  if (op === 'reset') return { op, userId, amount: null };
  const [min, max] = op === 'set' ? [0, XP_MAX] : [1, XP_STEP_MAX];
  const amount = payload.amount;
  if (!Number.isInteger(amount) || amount < min || amount > max) throw new TaskError(`The amount must be between ${fmt(min)} and ${fmt(max)} XP.`);
  return { op, userId, amount };
}

/** The member (null when not in the server) and the user (null when unknown to Discord). */
async function lookup(guild, client, userId) {
  const member = guild.members?.cache?.get(userId) ?? await guild.members?.fetch?.(userId).catch(() => null) ?? null;
  const user = member?.user ?? client?.users?.cache?.get(userId) ?? await client?.users?.fetch?.(userId).catch(() => null) ?? null;
  return { member, user };
}

const nameOf = ({ member, user }, userId) => member?.displayName || user?.globalName || user?.username || userId;
const roleName = (guild, id) => {
  const name = guild.roles?.cache?.get(id)?.name;
  return name ? `@${name}` : id;
};

/** The level change and reward-role lines of an admin change (changeLines of /xp, without mentions). */
function changeText(guild, result, lc) {
  const lines = [];
  if (result.newLevel !== result.oldLevel) lines.push(`Level ${result.oldLevel} → ${result.newLevel}.`);
  const roles = lc?.roles;
  if (roles?.added?.length) lines.push(`Roles added: ${roles.added.map((id) => roleName(guild, id)).join(', ')}.`);
  if (roles?.removed?.length) lines.push(`Roles removed: ${roles.removed.map((id) => roleName(guild, id)).join(', ')}.`);
  if (roles?.failed?.length) {
    lines.push(`Could not change: ${roles.failed.map((f) => roleName(guild, f.roleId)).join(', ')} (check the bot's role position and Manage Roles).`);
  }
  return lines.length ? ` ${lines.join(' ')}` : '';
}

/**
 * The member tasks, with their dependencies injectable for tests.
 * @param {object} [overrides] test doubles of the services used
 */
export function createMemberTasks(overrides = {}) {
  const deps = { getSettingsSafe, applyXpDelta, subtractXp, setXp, resetXp, handleLevelChange, syncLevelRoles, ...overrides };

  /** xp.adjust → `{ userId, xp, level }` after the change. */
  async function adjustXp({ guild, client, actor, payload }) {
    const { op, userId, amount } = readXpPayload(payload);
    if (!actor?.member && !actor?.isBotOwner) throw new TaskError('You are no longer a member of this server.');
    const settings = await deps.getSettingsSafe(guild.id);
    if (!settings) throw new TaskError("The bot couldn't load this server's settings. Try again in a moment.");
    // /xp is an ADMIN command; the bot owner (OWNER level) passes even without being a member.
    if (!actor.isBotOwner && resolvePermissionLevel(actor.member, settings) < PERMISSION_LEVELS.ADMIN) {
      throw new TaskError('Only administrators (Manage Server) can change XP.');
    }
    const guildId = guild.id;
    const who = await lookup(guild, client, userId);
    const name = nameOf(who, userId);
    const change = (result) => deps.handleLevelChange({
      guild, member: who.member, userId, result, settings, source: 'admin', channel: null, announce: false,
    });

    if (op === 'give') {
      if (who.user?.bot) throw new TaskError("Bots don't earn XP.");
      if (!who.member) throw new TaskError("That user isn't in this server.");
      const r = await deps.applyXpDelta({ guildId, userId, delta: amount });
      const lc = await change(r);
      let message = `Gave ${fmt(amount)} XP to ${name}. Total: ${fmt(r.xp)} XP (level ${r.newLevel}).${changeText(guild, r, lc)}`;
      if (r.xp === XP_MAX) message += ` XP is capped at ${fmt(XP_MAX)}.`;
      return { message, data: { userId, xp: r.xp, level: r.newLevel } };
    }

    if (op === 'take') {
      const r = await deps.subtractXp({ guildId, userId, amount });
      if (!r || r.oldXp === 0) throw new TaskError(`${name} has no XP in this server.`);
      const lc = await change(r);
      return {
        message: `Removed ${fmt(r.oldXp - r.xp)} XP from ${name}. Total: ${fmt(r.xp)} XP (level ${r.newLevel}).${changeText(guild, r, lc)}`,
        data: { userId, xp: r.xp, level: r.newLevel },
      };
    }

    if (op === 'set') {
      if (amount > 0) {
        if (who.user?.bot) throw new TaskError("Bots don't earn XP.");
        if (!who.member) throw new TaskError("That user isn't in this server.");
      }
      const r = await deps.setXp({ guildId, userId, xp: amount });
      if (!r) throw new TaskError(`${name} already has 0 XP.`);
      const lc = await change(r);
      return {
        message: `${name} now has ${fmt(r.xp)} XP (level ${r.newLevel}).${changeText(guild, r, lc)}`,
        data: { userId, xp: r.xp, level: r.newLevel },
      };
    }

    // reset
    const old = await deps.resetXp(guildId, userId);
    if (!old) throw new TaskError(`${name} has no XP in this server.`);
    let removed = '';
    if (settings.levelRolesSyncOnDemotion && who.member) {
      const roles = await deps.syncLevelRoles(who.member, 0, settings, { syncDown: true, reason: 'XP reset' });
      if (roles?.removed?.length) removed = ` Roles removed: ${roles.removed.map((id) => roleName(guild, id)).join(', ')}.`;
    }
    return { message: `Wiped XP and level for ${name} (was level ${old.oldLevel}).${removed}`, data: { userId, xp: 0, level: 0 } };
  }

  return Object.freeze({
    'xp.adjust': Object.freeze({ run: adjustXp }),
  });
}

export const tasks = createMemberTasks();
