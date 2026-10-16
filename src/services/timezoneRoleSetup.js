// Timezone role templates: finds or creates the roles of a TIMEZONE_ROLE_TEMPLATES entry (CST, EST, ...) and links each one
// to its zone in GuildSettings.timezoneRoles, so a Unique reaction-role panel of them sets /timestamp's timezone.
// Shared by /reactionrole create template:timezones-* and the dashboard's rr.timezoneTemplate task.
// Everything is checked before anything is created; roles are reused by name, so running it again after a failure is safe.
import { PermissionFlagsBits } from 'discord.js';
import { LIMITS } from '../config/constants.js';
import { checkAssignableRole } from '../utils/interactionHelpers.js';
import { withTimeout } from '../utils/async.js';
import { TIMEZONE_ROLE_TEMPLATES, normalizeTimezoneRoles } from '../utils/timezones.js';
import logger from '../utils/logger.js';
import { getSettingsSafe, updateSettingsWith } from './settingsService.js';

const CREATE_TIMEOUT_MS = 8_000;

/** A refusal with a message for the admin. */
export class TimezoneSetupError extends Error {}

/**
 * The guild's role of that name, ignoring case: not @everyone, not managed by an integration. The highest one when
 * several share the name.
 */
export function findRoleByName(guild, name) {
  const lower = name.toLowerCase();
  let best = null;
  for (const role of guild.roles.cache.values()) {
    if (role.id === guild.id || role.managed || role.name.toLowerCase() !== lower) continue;
    if (!best || role.position > best.position) best = role;
  }
  return best;
}

/**
 * Finds or creates the template's roles and links them to their zones.
 * @param {import('discord.js').Guild} guild
 * @param {string} templateKey - a TIMEZONE_ROLE_TEMPLATES key
 * @param {{ executor?: import('discord.js').GuildMember|null, reason?: string,
 *   settingsStore?: { get: typeof getSettingsSafe, updateWith: typeof updateSettingsWith } }} [opts] - settingsStore is for tests
 * @returns {Promise<{ roles: { roleId: string, name: string, zone: string, label: string, description: string,
 *   created: boolean }[], created: number, reused: number }>}
 * @throws {TimezoneSetupError} when a check fails (nothing was created then) or Discord refuses a role
 */
export async function setupTimezoneRoles(guild, templateKey, {
  executor = null, reason = 'Timezone roles template', settingsStore = { get: getSettingsSafe, updateWith: updateSettingsWith },
} = {}) {
  const template = Object.hasOwn(TIMEZONE_ROLE_TEMPLATES, templateKey) ? TIMEZONE_ROLE_TEMPLATES[templateKey] : null;
  if (!template) throw new TimezoneSetupError('That timezone template does not exist.');
  if (!guild.members?.me?.permissions?.has(PermissionFlagsBits.ManageRoles)) {
    throw new TimezoneSetupError('I need the **Manage Roles** permission to create the timezone roles.');
  }

  const settings = await settingsStore.get(guild.id);
  if (!settings) throw new TimezoneSetupError('Server settings could not be loaded. Please try again in a moment.');
  const live = new Set(guild.roles.cache.keys());
  const linked = new Set(normalizeTimezoneRoles(settings.timezoneRoles).filter((l) => live.has(l.roleId)).map((l) => l.roleId));

  // 1. Checks first: every role this template reuses must be one the bot can hand out, and the links must fit.
  const plan = template.roles.map((entry) => ({ entry, role: findRoleByName(guild, entry.name) }));
  for (const { entry, role } of plan) {
    if (!role) continue;
    const check = checkAssignableRole(guild, role, { executor });
    if (!check.ok) throw new TimezoneSetupError(`The existing **${entry.name}** role can't be used: ${check.reason} Rename or fix it, then try again.`);
  }
  const added = plan.filter(({ role }) => !role || !linked.has(role.id)).length;
  if (linked.size + added > LIMITS.TIMEZONE_ROLES) {
    throw new TimezoneSetupError(`This would make ${linked.size + added} timezone roles; the most is ${LIMITS.TIMEZONE_ROLES}. `
      + 'Remove some with `/settings timezone-role remove` first.');
  }

  // 2. Create the missing roles (no permissions, not mentionable), in order so they sort west to east.
  const roles = [];
  for (const { entry, role } of plan) {
    let target = role;
    if (!target) {
      try {
        target = await withTimeout(
          guild.roles.create({ name: entry.name, permissions: [], mentionable: false, hoist: false, reason }),
          CREATE_TIMEOUT_MS, 'create timezone role',
        );
      } catch (err) {
        logger.warn(`[timezone-roles] ${guild.id}: creating "${entry.name}" failed`, err);
        const made = roles.filter((r) => r.created).map((r) => r.name);
        throw new TimezoneSetupError(`Discord refused to create the **${entry.name}** role (${err?.message ?? 'unknown error'}).`
          + (made.length ? ` Already created: ${made.join(', ')}; running the template again reuses them.` : ''));
      }
    }
    roles.push({ roleId: target.id, name: entry.name, zone: entry.zone, label: entry.label, description: entry.description, created: !role });
  }

  // 3. Link every role to its zone (an existing link of that role is replaced).
  await settingsStore.updateWith(guild.id, (cur) => {
    const ids = new Set(roles.map((r) => r.roleId));
    const keep = normalizeTimezoneRoles(cur.timezoneRoles).filter((l) => !ids.has(l.roleId) && (!live.size || live.has(l.roleId)));
    return { timezoneRoles: [...keep, ...roles.map((r) => ({ roleId: r.roleId, zone: r.zone }))] };
  });

  const created = roles.filter((r) => r.created).length;
  logger.info(`[timezone-roles] ${guild.id}: template ${templateKey}, ${created} created, ${roles.length - created} reused`);
  return { roles, created, reused: roles.length - created };
}

/** "Created 4 roles and reused 2." */
export function describeSetup({ created, reused }) {
  const part = (n, verb) => `${verb} ${n} role${n === 1 ? '' : 's'}`;
  if (!reused) return `${part(created, 'Created')}.`;
  if (!created) return `${part(reused, 'Reused')} that already existed.`;
  return `${part(created, 'Created')} and reused ${reused} that already existed.`;
}
