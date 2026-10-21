// /rolepersist status | ignore add|remove | view | clear. Admin config command: replies are public (CLAUDE.md admin
// convention) and it keeps working while the Role Persistence module is off (gated = false). Every subcommand is ADMIN,
// so none needs the "module is off" refusal. The hooks are src/plugins/role-persist.js.
import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe, updateSettingsWith } from '../../services/settingsService.js';
import {
  retentionDays, expiresAt, isStale, loadRow, deleteRow, countRows, planRoles, clearWaiting,
} from '../../services/rolePersistService.js';
import { successEmbed, errorEmbed, infoEmbed, guildEmbed } from '../../utils/embedBuilder.js';
import { safeDefer, safeReply, missingBotPermissions, permissionNames } from '../../utils/interactionHelpers.js';
import { joinWithLimit, formatNumber } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = 'rolepersist';
export const gated = false;                          // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

export const data = new SlashCommandBuilder()
  .setName('rolepersist')
  .setDescription('Give members their roles back when they leave and rejoin.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('status').setDescription('Show the setup and how many members are saved.'))
  .addSubcommandGroup((g) => g.setName('ignore').setDescription('Roles that are never given back.')
    .addSubcommand((s) => s.setName('add').setDescription('Never give this role back.')
      .addRoleOption((o) => o.setName('role').setDescription('The role.').setRequired(true)))
    .addSubcommand((s) => s.setName('remove').setDescription('Give this role back again.')
      .addRoleOption((o) => o.setName('role').setDescription('The role.').setRequired(true))))
  .addSubcommand((s) => s.setName('view').setDescription('Show what is saved for a member.')
    .addUserOption((o) => o.setName('user').setDescription('The member who left.').setRequired(true)))
  .addSubcommand((s) => s.setName('clear').setDescription('Forget what is saved for a member.')
    .addUserOption((o) => o.setName('user').setDescription('The member who left.').setRequired(true)));

const MSG = {
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'),
  LOOKUP_FAILED: () => errorEmbed('Lookup Failed', 'The saved roles could not be read. Please try again in a moment.'),
  EVERYONE: () => errorEmbed('Invalid Role', '@everyone is never saved or given back, so it cannot be on this list.'),
  MANAGED: () => infoEmbed('Not Needed', 'That role is managed by an integration. Managed roles are never saved or given back.'),
  FULL: () => errorEmbed('List Full', `At most ${LIMITS.ROLEPERSIST_IGNORED_ROLES} roles can be on this list. Remove one with \`/rolepersist ignore remove\` first.`),
};

const OFF_NOTE = '\n\n⚠️ The **Role Persistence** module is off. Turn it on with `/module enable rolepersist`.';

/** Final answer: overwrites the deferred reply. Never pings. */
function finish(interaction, embed) {
  return safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
}

/** Loads settings, or answers "Settings Unavailable" and returns null. */
async function loadSettingsOrFinish(interaction) {
  const settings = await getSettingsSafe(interaction.guildId);
  if (!settings) await finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  return settings;
}

const stamp = (ms) => `<t:${Math.floor(ms / 1000)}:R>`;

// ── /rolepersist status ───────────────────────────────────────────────────────

async function runStatus(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const enabled = isModuleEnabled(settings, 'rolepersist');

  let saved = 'Unavailable';
  try {
    saved = formatNumber(await countRows(guildId));
  } catch (err) {
    logger.warn(`[role-persist] ${guildId}: could not count saved members: ${err?.message}`);
  }
  const ignored = settings.rolePersistIgnoredRoleIds ?? [];
  const needs = ['ManageRoles', ...(settings.rolePersistNicknames ? ['ManageNicknames'] : [])];
  const missing = missingBotPermissions(guild, needs);

  const embed = guildEmbed(settings, '🔁 Role Persistence').addFields(
    { name: 'Status', value: enabled ? '✅ On' : '❌ Off — `/module enable rolepersist`', inline: true },
    { name: 'Kept for', value: `${retentionDays(settings)} days`, inline: true },
    { name: 'Nicknames', value: settings.rolePersistNicknames ? '✅ Given back' : '❌ Not given back', inline: true },
    { name: 'Saved members', value: saved, inline: true },
    {
      name: `Never given back (${ignored.length}/${LIMITS.ROLEPERSIST_IGNORED_ROLES})`,
      value: ignored.length ? joinWithLimit(ignored.map((id) => `<@&${id}>`), { max: 1024 }) : 'None — `/rolepersist ignore add`',
    },
  );
  if (missing.length) embed.addFields({ name: 'Problem', value: `⚠️ I need **${permissionNames(missing).join(', ')}**.` });
  embed.setFooter({ text: 'Roles with powerful permissions, managed roles and roles above mine are never given back. Days and nicknames are set on the dashboard.' });
  return finish(interaction, embed);
}

// ── /rolepersist ignore add | remove ──────────────────────────────────────────

async function runIgnore(interaction, adding) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const option = interaction.options.getRole('role', true);
  const role = guild.roles.cache.get(option.id) ?? option;
  if (role.id === guild.id) return finish(interaction, MSG.EVERYONE());
  if (adding && role.managed) return finish(interaction, MSG.MANAGED());

  let outcome = null;
  try {
    await updateSettingsWith(guildId, (cur) => {
      const list = cur.rolePersistIgnoredRoleIds ?? [];
      const has = list.includes(role.id);
      if (adding) {
        if (has) { outcome = 'same'; return null; }
        if (list.length >= LIMITS.ROLEPERSIST_IGNORED_ROLES) { outcome = 'full'; return null; }
        outcome = 'changed';
        return { rolePersistIgnoredRoleIds: [...list, role.id] };
      }
      if (!has) { outcome = 'same'; return null; }
      outcome = 'changed';
      return { rolePersistIgnoredRoleIds: list.filter((id) => id !== role.id) };
    });
  } catch (err) {
    logger.error(`[role-persist] ${guildId}: ignore ${adding ? 'add' : 'remove'} failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (outcome === 'full') return finish(interaction, MSG.FULL());
  if (outcome === 'same') {
    return finish(interaction, infoEmbed('No Change', adding
      ? `<@&${role.id}> is already on the never-give-back list.`
      : `<@&${role.id}> is not on the never-give-back list.`));
  }
  logger.info(`[role-persist] ${guildId}: ${adding ? 'ignore add' : 'ignore remove'} ${role.id} by ${interaction.user.id}`);
  const settings = await getSettingsSafe(guildId);
  const off = settings && !isModuleEnabled(settings, 'rolepersist') ? OFF_NOTE : '';
  return finish(interaction, successEmbed(
    adding ? 'Role Ignored' : 'Role No Longer Ignored',
    (adding
      ? `<@&${role.id}> is never given back, even to members who already left with it.`
      : `<@&${role.id}> is given back again, including to members who already left with it.`) + off,
  ));
}

// ── /rolepersist view ─────────────────────────────────────────────────────────

async function runView(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const user = interaction.options.getUser('user', true);
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;

  let row;
  try {
    row = await loadRow(guildId, user.id);
  } catch (err) {
    logger.warn(`[role-persist] ${guildId}: could not read ${user.id}: ${err?.message}`);
    return finish(interaction, MSG.LOOKUP_FAILED());
  }
  if (!row) {
    return finish(interaction, infoEmbed('Nothing Saved', `Nothing is saved for <@${user.id}>. Roles are saved when a member leaves and dropped after ${retentionDays(settings)} days.`));
  }

  const { give, skipped } = planRoles(guild, row.roleIds, settings);
  const lines = [
    ...give.map((r) => `✅ <@&${r.id}>`),
    ...skipped.map((s) => (s.deleted ? `❌ deleted role (\`${s.roleId}\`)` : `⚠️ <@&${s.roleId}> — ${s.reason}`)),
  ];
  const stale = isStale(row, settings);
  const embed = guildEmbed(settings, '🔁 Saved Roles').addFields(
    { name: 'Member', value: `<@${user.id}>`, inline: true },
    { name: 'Left', value: stamp(new Date(row.leftAt).getTime()), inline: true },
    { name: stale ? 'Expired' : 'Expires', value: stale ? 'Dropped on the next sweep' : stamp(expiresAt(row, settings)), inline: true },
    { name: 'Roles', value: lines.length ? joinWithLimit(lines, { sep: '\n', max: 1024 }) : 'None' },
    { name: 'Nickname', value: row.nickname ? `\`${row.nickname}\`${settings.rolePersistNicknames ? '' : ' (nicknames are off)'}` : 'None', inline: true },
  );
  embed.setFooter({ text: '✅ given back on rejoin · ⚠️ skipped' });
  return finish(interaction, embed);
}

// ── /rolepersist clear ────────────────────────────────────────────────────────

async function runClear(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guildId } = interaction;
  const user = interaction.options.getUser('user', true);
  let removed;
  try {
    removed = await deleteRow(guildId, user.id);
  } catch (err) {
    logger.error(`[role-persist] ${guildId}: clear failed for ${user.id}`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  clearWaiting(guildId, user.id);
  if (!removed) return finish(interaction, infoEmbed('Nothing Saved', `Nothing was saved for <@${user.id}>.`));
  logger.info(`[role-persist] ${guildId}: cleared ${user.id} by ${interaction.user.id}`);
  return finish(interaction, successEmbed('Cleared', `<@${user.id}> will not get their old roles back when they rejoin.`));
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  if (group === 'ignore') return runIgnore(interaction, sub === 'add');
  if (sub === 'status') return runStatus(interaction);
  if (sub === 'view') return runView(interaction);
  if (sub === 'clear') return runClear(interaction);
}
