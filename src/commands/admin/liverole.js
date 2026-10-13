// /liverole set | require | enable | disable | status (pkg-liverole.md §4). Admin config command: replies are public (CLAUDE.md admin
// convention) and it keeps working while the Live Role module is off (gated = false). The engine lives in src/plugins/liverole.js.
import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { MODULES } from '../../config/modules.js';
import { getSettingsSafe, updateSettings } from '../../services/settingsService.js';
import { successEmbed, errorEmbed, infoEmbed, guildEmbed } from '../../utils/embedBuilder.js';
import {
  safeDefer, safeReply, checkAssignableRole, awaitConfirm, isInteractionGone, permissionNames,
} from '../../utils/interactionHelpers.js';
import { joinWithLimit, formatNumber } from '../../utils/text.js';
import { withTimeout } from '../../utils/async.js';
import { newRoleOption, resolveNewOptions } from '../../utils/newTargets.js';
import logger from '../../utils/logger.js';
import {
  LIVEROLE, isStreaming, streamLabel, countLosingHolders,
  loadMembers, syncGuild, formatSyncResult, liveRoleProblems,
} from '../../plugins/liverole.js';

export const module = 'liverole';
export const gated = false;                          // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

export const data = new SlashCommandBuilder()
  .setName('liverole')
  .setDescription('Give a role while members stream (MEE6 Twitch Alerts live-streaming role).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('set').setDescription('Set the role members get while they are live.')
    .addRoleOption((o) => o.setName('role').setDescription('A dedicated role, e.g. @Live.'))
    .addStringOption((o) => newRoleOption(o)))
  .addSubcommand((s) => s.setName('require').setDescription('Only give the live role to members with a role.')
    .addRoleOption((o) => o.setName('role').setDescription('Required role (leave empty for everyone).')))
  .addSubcommand((s) => s.setName('enable').setDescription('Turn the live role on and re-sync it.'))
  .addSubcommand((s) => s.setName('disable').setDescription('Turn the live role off.')
    .addBooleanOption((o) => o.setName('remove_roles').setDescription('Remove it from members who have it now (default: yes).')))
  .addSubcommand((s) => s.setName('status').setDescription('Show the setup and who is live now.'));

// ── Shared pieces ─────────────────────────────────────────────────────────────

const MSG = {
  EVERYONE: () => errorEmbed('Invalid Role', "@everyone can't be the live role. Pick or create a dedicated role such as @Live."),
  SAME_AS_REQUIRED: () => errorEmbed('Invalid Role', "The live role can't also be the required role. Change the requirement with `/liverole require`."),
  REQUIRED_IS_LIVE: () => errorEmbed('Invalid Role', "The required role can't be the live role itself."),
  CANNOT_USE: (reason) => errorEmbed('Cannot Use That Role', `${reason} Pick another role, or fix my role position and permissions.`),
  NO_ROLE: () => errorEmbed('No Live Role Set', 'Pick the role first with `/liverole set role:@Live` (or create it with `new_role:Live`), then run `/liverole enable`.'),
  ROLE_DELETED: () => errorEmbed('Live Role Missing', 'The live role was deleted. Pick a new one with `/liverole set`.'),
  TOO_MANY: (n, roleId, partial) => errorEmbed(
    'Use a Dedicated Role',
    `<@&${roleId}> is held by **${formatNumber(n)}** members who are not live${partial}. They would all lose it. Create a dedicated one with \`/liverole set new_role:Live\`.`,
  ),
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The setting could not be saved. Please try again in a moment.'),
};

const OFF_NOTE = 'ℹ️ The live role is **off**. Turn it on with `/liverole enable`.';

/** Final answer: overwrites the deferred reply (which is also the confirm message when one was shown). Never pings. */
async function finish(interaction, payload) {
  const p = { ...payload, components: [], allowedMentions: { parse: [] } };
  try {
    return await interaction.editReply(p);
  } catch (err) {
    if (isInteractionGone(err)) return null;
    return safeReply(interaction, p);
  }
}

/** Loads settings, or answers "Settings Unavailable" and returns null. */
async function loadSettingsOrFinish(interaction) {
  const settings = await getSettingsSafe(interaction.guildId);
  if (!settings) await finish(interaction, { embeds: [MSG.SETTINGS_UNAVAILABLE()] });
  return settings;
}

/** Writes settings through settingsService; answers "Could Not Save" and returns null on failure. */
async function save(interaction, sub, data, field, oldValue) {
  const guildId = interaction.guildId;
  try {
    const row = await updateSettings(guildId, data);
    logger.info(`[liverole] ${guildId}: ${field} ${JSON.stringify(oldValue ?? null)} → ${JSON.stringify(data[field] ?? null)} by ${interaction.user.id}`);
    return row;
  } catch (err) {
    logger.error(`[liverole] ${sub} save failed`, err);
    await finish(interaction, { embeds: [MSG.COULD_NOT_SAVE()] });
    return null;
  }
}

/** Runs a sync and waits for it, but not forever: null means it is still running (syncGuild never rejects). */
function boundedSync(guild, opts) {
  return withTimeout(syncGuild(guild, opts), LIVEROLE.COMMAND_SYNC_WAIT_MS, 'liverole command sync').catch(() => null);
}

/** The guild's own role object for a role option; null when there is none or the guild does not have it cached. */
function resolveRole(interaction, required) {
  const opt = interaction.options.getRole('role', required);
  return opt ? (interaction.guild.roles.cache.get(opt.id) ?? null) : null;
}

const describeRule = (roleId, requiredId) => `Members get <@&${roleId}> while Discord shows them as **Streaming** (Twitch or YouTube)`
  + (requiredId ? ` and they have <@&${requiredId}>.` : '.');

// ── /liverole set ─────────────────────────────────────────────────────────────

async function runSet(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  if (!(await resolveNewOptions(interaction, data, { required: ['role'] }))) return;
  const { guild } = interaction;
  const role = resolveRole(interaction, true);

  if (!role) return finish(interaction, { embeds: [MSG.CANNOT_USE('Role not found.')] });
  if (role.id === guild.id) return finish(interaction, { embeds: [MSG.EVERYONE()] });
  if (role.id === settings.liveRoleRequiredRoleId) return finish(interaction, { embeds: [MSG.SAME_AS_REQUIRED()] });
  // Anyone who can show a Streaming status gets this role, so it must never carry Administrator, Manage Roles or similar.
  const check = checkAssignableRole(guild, role, { executor: interaction.member, allowDangerous: false });
  if (!check.ok) return finish(interaction, { embeds: [MSG.CANNOT_USE(check.reason)] });

  if (role.id === settings.liveRoleId) {
    return finish(interaction, {
      embeds: [infoEmbed('No Change', `<@&${role.id}> is already the live role. Use \`/liverole enable\` to re-sync it.`)],
    });
  }

  // Dedicated-role guard: the sweep strips the role from every holder who is not streaming, so a populated role would be emptied.
  const complete = await loadMembers(guild);
  const n = countLosingHolders(guild, role, settings);
  if (n > LIVEROLE.SET_MAX_STRIP) {
    return finish(interaction, { embeds: [MSG.TOO_MANY(n, role.id, complete ? '' : ' (counting cached members only)')] });
  }
  if (n > 0) {
    const ok = await awaitConfirm(interaction, {
      title: 'Use this role as the live role?',
      description: `**${formatNumber(n)}** member(s) have <@&${role.id}> but are not live`
        + `${settings.liveRoleRequiredRoleId ? ' or lack the required role' : ''}. `
        + 'While the live role is on, they will lose it. A dedicated role (for example a new @Live role) is recommended.',
      confirmLabel: 'Use this role', cancelLabel: 'Cancel', danger: true,
    });
    if (!ok) return; // awaitConfirm already showed Cancelled / Timed out; nothing was written
  }

  const previousId = settings.liveRoleId;
  const row = await save(interaction, 'set', { liveRoleId: role.id }, 'liveRoleId', previousId);
  if (!row) return;

  const enabled = row.liveRoleEnabled === true;
  const result = enabled ? await boundedSync(guild, { mode: 'sync', reason: 'set' }) : undefined;

  // The previous role is never touched: the bot no longer manages it, and stripping it could hit members who got it for other reasons.
  const oldHolders = previousId && previousId !== role.id
    ? (guild.roles.cache.get(previousId)?.members.filter((m) => !m.user.bot).size ?? 0)
    : 0;

  const lines = [describeRule(role.id, settings.liveRoleRequiredRoleId)];
  if (enabled) lines.push(formatSyncResult(result));
  else lines.push(OFF_NOTE + (n > 0 ? ` Members who have <@&${role.id}> but are not live will lose it then.` : ''));
  if (oldHolders > 0) {
    lines.push(`ℹ️ **${formatNumber(oldHolders)}** member(s) still have the previous live role <@&${previousId}>. I no longer manage it, so remove it from them or delete that role.`);
  }
  return finish(interaction, { embeds: [successEmbed('Live Role Set', lines.join('\n'))] });
}

// ── /liverole require ─────────────────────────────────────────────────────────

async function runRequire(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const { guild } = interaction;
  const role = resolveRole(interaction, false);
  const requiredId = (!role || role.id === guild.id) ? null : role.id;

  if (requiredId && requiredId === settings.liveRoleId) return finish(interaction, { embeds: [MSG.REQUIRED_IS_LIVE()] });
  if (requiredId === (settings.liveRoleRequiredRoleId ?? null)) {
    return finish(interaction, {
      embeds: [infoEmbed('No Change', requiredId ? `<@&${requiredId}> is already required.` : 'Everyone who streams already qualifies.')],
    });
  }

  // No checkAssignableRole here: the bot never assigns the required role, so managed roles and roles above the bot are valid.
  const row = await save(interaction, 'require', { liveRoleRequiredRoleId: requiredId }, 'liveRoleRequiredRoleId', settings.liveRoleRequiredRoleId);
  if (!row) return;

  const sync = row.liveRoleEnabled && row.liveRoleId;
  const result = sync ? await boundedSync(guild, { mode: 'sync', reason: 'require' }) : undefined;
  const head = requiredId ? `Only members with <@&${requiredId}> get the live role.` : 'Everyone who streams can get the live role.';
  return finish(interaction, {
    embeds: [successEmbed('Live Role Requirement Updated', `${head}\n${sync ? formatSyncResult(result) : OFF_NOTE}`)],
  });
}

// ── /liverole enable ──────────────────────────────────────────────────────────

async function runEnable(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const { guild } = interaction;

  if (!settings.liveRoleId) return finish(interaction, { embeds: [MSG.NO_ROLE()] });
  const role = guild.roles.cache.get(settings.liveRoleId);
  if (!role) return finish(interaction, { embeds: [MSG.ROLE_DELETED()] });
  const check = checkAssignableRole(guild, role, { executor: interaction.member, allowDangerous: false });
  if (!check.ok) return finish(interaction, { embeds: [MSG.CANNOT_USE(check.reason)] });

  const wasOn = settings.liveRoleEnabled === true;
  if (!wasOn) {
    // Stale holders can exist after /module disable liverole or "disable remove_roles:false". No hard refusal here: the role was
    // accepted by `set`, and refusing could leave an admin unable to turn the feature back on.
    await loadMembers(guild);
    const n = countLosingHolders(guild, role, settings);
    if (n > 0) {
      const ok = await awaitConfirm(interaction, {
        title: 'Turn the live role on?',
        description: `**${formatNumber(n)}** member(s) have <@&${role.id}> but are not live. Turning the live role on removes it from them.`,
        confirmLabel: 'Turn on', cancelLabel: 'Cancel', danger: true,
      });
      if (!ok) return;
    }
    const row = await save(interaction, 'enable', { [MODULES.liverole.flag]: true }, MODULES.liverole.flag, false);
    if (!row) return;
  }

  const result = await boundedSync(guild, { mode: 'sync', reason: wasOn ? 'resync' : 'enable' });
  const lines = [describeRule(role.id, settings.liveRoleRequiredRoleId), formatSyncResult(result)];
  const reqId = settings.liveRoleRequiredRoleId;
  if (reqId && !guild.roles.cache.has(reqId)) {
    lines.push('⚠️ The required role was deleted, so nobody qualifies. Change it with `/liverole require`.');
  }
  return finish(interaction, {
    embeds: [successEmbed(wasOn ? 'Live Role Already On — Resynced' : 'Live Role Enabled', lines.join('\n'))],
  });
}

// ── /liverole disable ─────────────────────────────────────────────────────────

async function runDisable(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const { guild } = interaction;
  const removeRoles = interaction.options.getBoolean('remove_roles') ?? true;

  const wasOn = settings.liveRoleEnabled === true;
  if (wasOn) {
    const row = await save(interaction, 'disable', { [MODULES.liverole.flag]: false }, MODULES.liverole.flag, true);
    if (!row) return;
  }

  const clear = removeRoles && !!settings.liveRoleId;
  // Clear mode removes the role from every non-bot holder, including members who are streaming right now.
  const result = clear ? await boundedSync(guild, { mode: 'clear', reason: 'disable' }) : undefined;

  const lines = ['Nobody will get the live role when they go live. The live role and the required role stay saved.'];
  if (clear) lines.push(formatSyncResult(result));
  if (!removeRoles && settings.liveRoleId) {
    lines.push(`Members who have <@&${settings.liveRoleId}> now keep it. Run \`/liverole disable\` again to remove it from them.`);
  }
  return finish(interaction, {
    embeds: [successEmbed(wasOn ? 'Live Role Disabled' : 'Live Role Already Off', lines.join('\n'))],
  });
}

// ── /liverole status ──────────────────────────────────────────────────────────

async function runStatus(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const { guild } = interaction;

  // Caches only (no member fetch), so status is always fast.
  const role = settings.liveRoleId ? (guild.roles.cache.get(settings.liveRoleId) ?? null) : null;
  const manage = role ? checkAssignableRole(guild, role, { allowDangerous: true }) : null;
  const dangerous = manage?.dangerous ?? [];
  const reqId = settings.liveRoleRequiredRoleId;
  const reqExists = !reqId || guild.roles.cache.has(reqId);
  const holders = role ? [...role.members.filter((m) => !m.user.bot).values()] : [];
  const streamers = [...guild.presences.cache.values()].filter((p) => isStreaming(p) && !p.member?.user?.bot);
  const enabled = settings.liveRoleEnabled === true;

  const embed = guildEmbed(
    settings,
    '🔴 Live Role',
    'Gives a role to members while Discord shows them as **Streaming**, which happens when a Twitch or YouTube account they connected goes live.',
    'Counts use cached members · MEE6: Twitch Alerts › live-streaming role',
  );

  let liveNow = '—';
  if (role) {
    const lines = [`**${formatNumber(holders.length)}** member(s) have the live role · **${formatNumber(streamers.length)}** member(s) are streaming`];
    for (const m of holders.slice(0, LIVEROLE.STATUS_LIST_MAX)) {
      lines.push(`<@${m.id}> · ${streamLabel(guild.presences.cache.get(m.id)) ?? 'not streaming'}`);
    }
    liveNow = joinWithLimit(lines, { sep: '\n', max: 1024 });
  }

  embed.addFields(
    { name: 'Status', value: enabled ? '✅ On' : '❌ Off — `/liverole enable`', inline: true },
    {
      name: 'Live role',
      value: !settings.liveRoleId ? 'Not set — `/liverole set`' : (role ? `<@&${role.id}>` : `⚠️ Deleted (\`${settings.liveRoleId}\`)`),
      inline: true,
    },
    {
      name: 'Required role',
      value: !reqId ? 'None — everyone who streams' : (reqExists ? `<@&${reqId}>` : `⚠️ Deleted (\`${reqId}\`)`),
      inline: true,
    },
    { name: 'Live now', value: liveNow },
  );

  const problems = liveRoleProblems({
    enabled,
    liveRoleId: settings.liveRoleId,
    roleExists: !!role,
    manage,
    dangerousNames: permissionNames(dangerous),
    requiredRoleId: reqId,
    requiredRoleExists: reqExists,
    holderCount: holders.length,
  });
  if (problems.length) embed.addFields({ name: 'Problems', value: joinWithLimit(problems, { sep: '\n', max: 1024 }) });

  return finish(interaction, { embeds: [embed] });
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'set') return runSet(interaction);
  if (sub === 'require') return runRequire(interaction);
  if (sub === 'enable') return runEnable(interaction);
  if (sub === 'disable') return runDisable(interaction);
  if (sub === 'status') return runStatus(interaction);
}
