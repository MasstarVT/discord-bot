// Role templates (ROLE_TEMPLATES): finds or creates a template's roles (He/Him, Red, CST, ...) and, for a timezone template,
// links each one to its zone in GuildSettings.timezoneRoles, so a Unique reaction-role panel of them sets /timestamp's timezone.
// Shared by /reactionrole create template:<key> and the dashboard's rr.roleTemplate task.
// Everything is checked before anything is created; roles are reused by name, so running it again after a failure is safe.
import { PermissionFlagsBits } from 'discord.js';
import { LIMITS } from '../config/constants.js';
import { checkAssignableRole } from '../utils/interactionHelpers.js';
import { withTimeout } from '../utils/async.js';
import { normalizeTimezoneRoles, TIMEZONE_ROLE_TEMPLATES, TIMEZONE_TEMPLATE_PAST_ZONES } from '../utils/timezones.js';
import { getRoleTemplate } from '../utils/roleTemplates.js';
import logger from '../utils/logger.js';
import { getSettingsSafe, updateSettingsWith } from './settingsService.js';
import * as rr from './reactionRoleService.js';

const CREATE_TIMEOUT_MS = 8_000;
/** Discord's role cap per server (counting @everyone here, to stay on the safe side). */
const DISCORD_ROLE_CAP = 250;

/** A refusal with a message for the admin. */
export class RoleTemplateError extends Error {}

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
 * Finds or creates the template's roles, and links them to their zones for a timezone template.
 * @param {import('discord.js').Guild} guild
 * @param {string} templateKey - a ROLE_TEMPLATES key
 * @param {{ executor?: import('discord.js').GuildMember|null, reason?: string,
 *   settingsStore?: { get: typeof getSettingsSafe, updateWith: typeof updateSettingsWith } }} [opts] - settingsStore is for tests
 * @returns {Promise<{ roles: { roleId: string, name: string, label: string, description: string|null, emoji: string|null,
 *   zone: string|null, created: boolean }[], created: number, reused: number }>}
 * @throws {RoleTemplateError} when a check fails (nothing was created then) or Discord refuses a role
 */
export async function setupRoleTemplate(guild, templateKey, {
  executor = null, reason = 'Role template', settingsStore = { get: getSettingsSafe, updateWith: updateSettingsWith },
} = {}) {
  const template = getRoleTemplate(templateKey);
  if (!template) throw new RoleTemplateError('That role template does not exist.');
  if (!guild.members?.me?.permissions?.has(PermissionFlagsBits.ManageRoles)) {
    throw new RoleTemplateError('I need the **Manage Roles** permission to create the template\'s roles.');
  }

  // 1. Checks first: every role this template reuses must be one the bot can hand out, and the new roles must fit.
  const plan = template.roles.map((entry) => ({ entry, role: findRoleByName(guild, entry.name) }));
  for (const { entry, role } of plan) {
    if (!role) continue;
    const check = checkAssignableRole(guild, role, { executor });
    if (!check.ok) throw new RoleTemplateError(`The existing **${entry.name}** role can't be used: ${check.reason} Rename or fix it, then try again.`);
  }
  const missing = plan.filter(({ role }) => !role).length;
  if (guild.roles.cache.size + missing > DISCORD_ROLE_CAP) {
    throw new RoleTemplateError(`This needs ${missing} new roles, but the server is close to Discord's limit of ${DISCORD_ROLE_CAP} roles. `
      + 'Delete some roles first.');
  }
  const live = new Set(guild.roles.cache.keys());
  if (template.linksZones) {
    const settings = await settingsStore.get(guild.id);
    if (!settings) throw new RoleTemplateError('Server settings could not be loaded. Please try again in a moment.');
    const linked = new Set(normalizeTimezoneRoles(settings.timezoneRoles).filter((l) => live.has(l.roleId)).map((l) => l.roleId));
    const added = plan.filter(({ role }) => !role || !linked.has(role.id)).length;
    if (linked.size + added > LIMITS.TIMEZONE_ROLES) {
      throw new RoleTemplateError(`This would make ${linked.size + added} timezone roles; the most is ${LIMITS.TIMEZONE_ROLES}. `
        + 'Remove some with `/settings timezone-role remove` first.');
    }
  }

  // 2. Create the missing roles (no permissions, not mentionable), in the template's order.
  const roles = [];
  for (const { entry, role } of plan) {
    let target = role;
    if (!target) {
      try {
        target = await withTimeout(
          guild.roles.create({
            name: entry.name, permissions: [], mentionable: false, hoist: false, reason,
            ...(entry.color ? { colors: { primaryColor: entry.color } } : {}),
          }),
          CREATE_TIMEOUT_MS, 'create template role',
        );
      } catch (err) {
        logger.warn(`[role-templates] ${guild.id}: creating "${entry.name}" failed`, err);
        const made = roles.filter((r) => r.created).map((r) => r.name);
        throw new RoleTemplateError(`Discord refused to create the **${entry.name}** role (${err?.message ?? 'unknown error'}).`
          + (made.length ? ` Already created: ${made.join(', ')}; running the template again reuses them.` : ''));
      }
    }
    roles.push({
      roleId: target.id, name: entry.name, label: entry.label, description: entry.description ?? null,
      emoji: entry.emoji ?? null, zone: entry.zone ?? null, created: !role,
    });
  }

  // 3. A timezone template links every role to its zone (an existing link of that role is replaced).
  if (template.linksZones) {
    await settingsStore.updateWith(guild.id, (cur) => {
      const ids = new Set(roles.map((r) => r.roleId));
      const keep = normalizeTimezoneRoles(cur.timezoneRoles).filter((l) => !ids.has(l.roleId) && (!live.size || live.has(l.roleId)));
      return { timezoneRoles: [...keep, ...roles.map((r) => ({ roleId: r.roleId, zone: r.zone }))] };
    });
  }

  const created = roles.filter((r) => r.created).length;
  logger.info(`[role-templates] ${guild.id}: template ${templateKey}, ${created} created, ${roles.length - created} reused`);
  return { roles, created, reused: roles.length - created };
}

// ── Upgrading timezone panels when a template gains roles ───────────────────────

const DEFAULT_SETTINGS_STORE = { get: getSettingsSafe, updateWith: updateSettingsWith };
const DEFAULT_PANEL_STORE = {
  list: rr.listUniquePanels, insertItems: rr.insertItems, sync: rr.syncPanel, invalidate: rr.invalidatePanelIndex,
};
const SMALLEST_PAST_SET = Math.min(...Object.values(TIMEZONE_TEMPLATE_PAST_ZONES).flat().map((zones) => zones.length));

/** The template key whose past zone set the panel's zones are exactly, or null. */
export function matchPastTemplate(zones) {
  if (zones.some((z) => !z)) return null;
  const have = new Set(zones);
  if (have.size !== zones.length) return null;
  for (const [key, sets] of Object.entries(TIMEZONE_TEMPLATE_PAST_ZONES)) {
    if (sets.some((set) => set.length === have.size && set.every((z) => have.has(z)))) return key;
  }
  return null;
}

/**
 * Adds a timezone template's newer roles to every panel made from an older version of it: a Unique dropdown or button
 * panel whose roles are linked to exactly one of TIMEZONE_TEMPLATE_PAST_ZONES. A panel an admin changed never matches and
 * an upgraded one no longer does, so this runs on every start (the reaction-roles guildReady hook). New roles are reused by
 * name when unlinked or linked to the same zone, or created; a failed check skips that panel before anything is created.
 * Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {{ settingsStore?: typeof DEFAULT_SETTINGS_STORE, panelStore?: typeof DEFAULT_PANEL_STORE }} [opts] - for tests
 * @returns {Promise<number>} how many panels were upgraded
 */
export async function upgradeTimezonePanels(guild, { settingsStore = DEFAULT_SETTINGS_STORE, panelStore = DEFAULT_PANEL_STORE } = {}) {
  try {
    const settings = await settingsStore.get(guild.id);
    const live = new Set(guild.roles.cache.keys());
    const links = normalizeTimezoneRoles(settings?.timezoneRoles).filter((l) => live.has(l.roleId));
    if (links.length < SMALLEST_PAST_SET) return 0; // most guilds: no query
    let upgraded = 0;
    for (const panel of await panelStore.list(guild.id)) {
      const zoneOf = new Map(links.map((l) => [l.roleId, l.zone]));
      const key = matchPastTemplate(panel.items.map((it) => zoneOf.get(it.roleId)));
      if (!key) continue;
      const added = await upgradePanel(guild, panel, key, { links, live, zoneOf, settings, settingsStore, panelStore });
      if (added) upgraded++;
    }
    return upgraded;
  } catch (err) {
    logger.warn(`[role-templates] ${guild?.id}: upgrading timezone panels failed`, err);
    return 0;
  }
}

/** One panel; `links` is updated in place with the links written. @returns {Promise<number>} roles added */
async function upgradePanel(guild, panel, key, { links, live, zoneOf, settings, settingsStore, panelStore }) {
  const skip = (why) => {
    logger.info(`[role-templates] ${guild.id}: timezone panel ${panel.id} (${key}) not upgraded: ${why}`);
    return 0;
  };
  const template = TIMEZONE_ROLE_TEMPLATES[key];
  const have = new Set(panel.items.map((it) => zoneOf.get(it.roleId)));
  const plan = template.roles.filter((entry) => !have.has(entry.zone)).map((entry) => ({ entry, role: findRoleByName(guild, entry.name) }));
  if (!plan.length) return 0;

  // 1. Checks first, as setupRoleTemplate.
  if (!guild.members?.me?.permissions?.has(PermissionFlagsBits.ManageRoles)) return skip('no Manage Roles');
  for (const { entry, role } of plan) {
    if (!role) continue;
    const linked = zoneOf.get(role.id);
    if (linked && linked !== entry.zone) return skip(`the ${entry.name} role is linked to ${linked}`);
    const check = checkAssignableRole(guild, role);
    if (!check.ok) return skip(`the ${entry.name} role can't be used: ${check.reason}`);
  }
  if (panel.items.length + plan.length > rr.MAX_ITEMS[panel.layout]) return skip('too many roles for the panel');
  const missing = plan.filter(({ role }) => !role).length;
  if (guild.roles.cache.size + missing > DISCORD_ROLE_CAP) return skip('close to the role cap');
  const newLinks = plan.filter(({ role }) => !role || !zoneOf.has(role.id)).length;
  if (links.length + newLinks > LIMITS.TIMEZONE_ROLES) return skip(`more than ${LIMITS.TIMEZONE_ROLES} timezone roles`);

  // 2. Create the missing roles; a failure leaves the panel as it was (created roles are reused next time).
  const roles = [];
  for (const { entry, role } of plan) {
    let target = role;
    if (!target) {
      try {
        target = await withTimeout(
          guild.roles.create({ name: entry.name, permissions: [], mentionable: false, hoist: false, reason: 'Timezone template update' }),
          CREATE_TIMEOUT_MS, 'create template role',
        );
      } catch (err) {
        logger.warn(`[role-templates] ${guild.id}: creating "${entry.name}" for panel ${panel.id} failed`, err);
        return 0;
      }
    }
    roles.push({ entry, roleId: target.id });
  }

  // 3. Link them, then add them to the panel in the template's order and re-render it.
  const ids = new Set(roles.map((r) => r.roleId));
  await settingsStore.updateWith(guild.id, (cur) => {
    const keep = normalizeTimezoneRoles(cur.timezoneRoles).filter((l) => !ids.has(l.roleId) && live.has(l.roleId));
    return { timezoneRoles: [...keep, ...roles.map((r) => ({ roleId: r.roleId, zone: r.entry.zone }))] };
  });
  for (const r of roles) {
    if (!zoneOf.has(r.roleId)) links.push({ roleId: r.roleId, zone: r.entry.zone });
  }
  const items = roles.map(({ entry, roleId }) => ({
    roleId, label: entry.label, emoji: null, style: 2, description: panel.layout === 'SELECT' ? entry.description : null,
  }));
  const roleOfZone = new Map([...panel.items.map((it) => [zoneOf.get(it.roleId), it.roleId]), ...roles.map((r) => [r.entry.zone, r.roleId])]);
  await panelStore.insertItems(panel, items, template.roles.map((entry) => roleOfZone.get(entry.zone)));
  await panelStore.invalidate(guild.id);
  const synced = await panelStore.sync(guild, panel.id, { settings });
  logger.info(`[role-templates] ${guild.id}: timezone panel ${panel.id} (${key}) gained ${roles.map((r) => r.entry.name).join(', ')}`
    + (synced.status === 'ok' ? '' : ` (message not updated: ${synced.reason ?? synced.status})`));
  return roles.length;
}

/** "Created 4 roles and reused 2." */
export function describeSetup({ created, reused }) {
  const part = (n, verb) => `${verb} ${n} role${n === 1 ? '' : 's'}`;
  if (!reused) return `${part(created, 'Created')}.`;
  if (!created) return `${part(reused, 'Reused')} that already existed.`;
  return `${part(created, 'Created')} and reused ${reused} that already existed.`;
}

/**
 * Names the reused roles, since a template makes them self-assignable (an existing "Events" role may unlock a private
 * channel); null when every role was created.
 */
export function describeReused({ roles }) {
  const names = roles.filter((r) => !r.created).map((r) => `**${r.name}**`);
  if (!names.length) return null;
  return `Existing role${names.length === 1 ? '' : 's'} now on the panel: ${names.join(', ')}. Members can now give `
    + `${names.length === 1 ? 'it' : 'them'} to themselves, so check ${names.length === 1 ? 'it doesn\'t' : 'they don\'t'} unlock anything private.`;
}
