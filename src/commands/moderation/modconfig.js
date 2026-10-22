import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import prisma from '../../database/client.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettingsSafe, peekSettings, updateSettings, updateSettingsWith } from '../../services/settingsService.js';
import { countOverdueBans, describeTier, normalizeEscalationTiers } from '../../services/moderationService.js';
import { ackBudgetMs, mapLimit, withTimeout } from '../../utils/async.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed } from '../../utils/embedBuilder.js';
import {
  checkAssignableRole,
  checkChannelUsable,
  isInteractionGone,
  missingBotPermissions,
  permissionNames,
  respondAutocomplete,
} from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { newChannelOption, newRoleOption, resolveNewOptions } from '../../utils/newTargets.js';
import { parseDuration } from '../../utils/parseDuration.js';
import { chunkLines, clampText, joinWithLimit } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';

export const module = 'moderation';
export const gated = false; // works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const ACTION_CHOICES = [
  { name: 'Timeout', value: 'TIMEOUT' },
  { name: 'Mute (mute role)', value: 'MUTE' },
  { name: 'Kick', value: 'KICK' },
  { name: 'Temporary ban', value: 'TEMPBAN' },
  { name: 'Permanent ban', value: 'BAN' },
];

export const data = new SlashCommandBuilder()
  .setName('modconfig')
  .setDescription('Moderator settings: escalation, immune and mod roles, mute role, case log, visibility.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommandGroup((g) => g.setName('escalation').setDescription('Automatic actions after N warnings (MEE6 automated actions).')
    .addSubcommand((s) => s.setName('add').setDescription('Add or update a tier: N warnings within a period, then an action.')
      .addIntegerOption((o) => o.setName('warns').setDescription('Active warnings needed to trigger it').setRequired(true).setMinValue(1).setMaxValue(100))
      .addStringOption((o) => o.setName('action').setDescription('Action to take').setRequired(true).addChoices(...ACTION_CHOICES))
      .addStringOption((o) => o.setName('within').setDescription('Only count warnings from this period, e.g. 7d (omit = no limit)').setMaxLength(32))
      .addStringOption((o) => o.setName('duration').setDescription('Action length, e.g. 1h or 3d (Timeout and Temporary ban need one)').setMaxLength(32)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove an escalation tier.')
      .addIntegerOption((o) => o.setName('tier').setDescription('Tier to remove').setRequired(true).setMinValue(1).setAutocomplete(true)))
    .addSubcommand((s) => s.setName('list').setDescription('List the escalation tiers.')))
  .addSubcommandGroup((g) => g.setName('immune').setDescription("Roles the bot and automod won't moderate.")
    .addSubcommand((s) => s.setName('add').setDescription('Make a role immune.')
      .addRoleOption((o) => o.setName('role').setDescription('Role to make immune').setRequired(true)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a role from the immune list.')
      .addRoleOption((o) => o.setName('role').setDescription('Role to remove').setRequired(true)))
    .addSubcommand((s) => s.setName('list').setDescription('List the immune roles.')))
  .addSubcommandGroup((g) => g.setName('modroles').setDescription('Roles that can use moderation commands.')
    .addSubcommand((s) => s.setName('add').setDescription('Give a role moderator access.')
      .addRoleOption((o) => o.setName('role').setDescription('Moderator role'))
      .addStringOption((o) => newRoleOption(o)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a moderator role.')
      .addRoleOption((o) => o.setName('role').setDescription('Role to remove').setRequired(true)))
    .addSubcommand((s) => s.setName('list').setDescription('List the moderator roles.')))
  .addSubcommand((s) => s.setName('muterole').setDescription('Set, create or clear the mute role (used for mutes over 28 days).')
    .addRoleOption((o) => o.setName('role').setDescription('Existing role to use as the mute role'))
    .addStringOption((o) => newRoleOption(o))
    .addBooleanOption((o) => o.setName('setup').setDescription('Create the role if needed and deny talking in every channel'))
    .addBooleanOption((o) => o.setName('clear').setDescription('Clear the mute role')))
  .addSubcommand((s) => s.setName('logchannel').setDescription('Set or clear the channel for moderation case logs.')
    .addChannelOption((o) => o.setName('channel').setDescription('Case log channel (omit to clear)').addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
    .addStringOption((o) => newChannelOption(o)))
  .addSubcommand((s) => s.setName('visibility').setDescription('Who sees moderation command replies (MEE6 command visibility).')
    .addStringOption((o) => o.setName('mode').setDescription('Visibility mode').setRequired(true).addChoices(
      { name: 'Public: everyone sees the action', value: 'PUBLIC' },
      { name: 'Anonymous: public notice, moderator hidden', value: 'ANONYMOUS' },
      { name: 'Private: only the moderator sees it', value: 'PRIVATE' })))
  .addSubcommand((s) => s.setName('dm').setDescription('Turn DMs to punished members on or off.')
    .addBooleanOption((o) => o.setName('enabled').setDescription('DM members when warned, muted, kicked or banned').setRequired(true)))
  .addSubcommand((s) => s.setName('show').setDescription('Show the current moderation settings.'));

/** Thrown inside an updateSettingsWith mutator (or before a write) for a message that is safe to show. */
class ModConfigError extends Error {}

const TIMEOUT_MAX_S = 2_419_200;
const YEAR_S = 31_536_000;

// ── Helpers ───────────────────────────────────────────────────────────────────

const rolesLine = (ids) => (ids.length ? joinWithLimit(ids.map((id) => `<@&${id}>`), { max: 1024 }) : 'None');

/** describeTier plus the "(counts warnings since ...)" note (kept out of describeTier: it doubles as an autocomplete name). */
function tierLine(t) {
  let line = describeTier(t);
  const since = t.countFrom ? Date.parse(t.countFrom) : NaN;
  if (Number.isFinite(since)) line += ` (counts warnings since ${discordTimestamp(new Date(since), 'd')})`;
  return line;
}

function tierListText(tiers) {
  return tiers.length ? tiers.map(tierLine).join('\n') : 'No escalation tiers. Add one with `/modconfig escalation add`.';
}

/** Whole seconds of a duration string, or null when it cannot be parsed. */
function parseSeconds(str) {
  const ms = parseDuration(str);
  return ms === null ? null : Math.round(ms / 1000);
}

// ── Escalation ────────────────────────────────────────────────────────────────

async function escalationAdd(interaction) {
  const warns = interaction.options.getInteger('warns', true);
  const action = interaction.options.getString('action', true);
  const withinStr = interaction.options.getString('within');
  const durationStr = interaction.options.getString('duration');

  let within = null;
  if (withinStr) {
    within = parseSeconds(withinStr);
    if (within === null || within < 60 || within > YEAR_S) {
      throw new ModConfigError('Invalid `within`: use a period between 1 minute and 1 year, e.g. `24h` or `7d`.');
    }
  }
  let dur = null;
  if (durationStr) {
    dur = parseSeconds(durationStr);
    if (dur === null) throw new ModConfigError('Invalid `duration`: try `10m`, `1h` or `3d`.');
  }

  if (action === 'TIMEOUT' && !(dur !== null && dur >= 60 && dur <= TIMEOUT_MAX_S)) {
    throw new ModConfigError('Timeout tiers need a duration between 1 minute and 28 days. For longer mutes use the Mute action with a mute role.');
  }
  if (action === 'TEMPBAN' && !(dur !== null && dur >= 60 && dur <= YEAR_S)) {
    throw new ModConfigError('Temporary ban tiers need a duration between 1 minute and 1 year.');
  }
  if (action === 'MUTE' && dur !== null && (dur < 60 || dur > YEAR_S)) {
    throw new ModConfigError('Mute tiers need a duration between 1 minute and 1 year, or none for "until unmuted".');
  }
  if ((action === 'KICK' || action === 'BAN') && durationStr) {
    throw new ModConfigError("Kick and permanent ban tiers don't take a duration (use Temporary ban for a timed ban).");
  }

  let replaced = false;
  const saved = await updateSettingsWith(interaction.guildId, (cur) => {
    // The mute role is read from the locked row: no other settings load may happen inside a mutator.
    if (action === 'MUTE' && !cur.muteRoleId && (dur === null || dur > TIMEOUT_MAX_S)) {
      throw new ModConfigError('Mute tiers that are permanent or longer than 28 days need a mute role. Set one with `/modconfig muterole` first.');
    }
    const tiers = normalizeEscalationTiers(cur.infractionEscalation);
    const i = tiers.findIndex((t) => t.warnCount === warns && t.withinSeconds === within);
    replaced = i >= 0;
    const base = { warnCount: warns, withinSeconds: within, action, durationSeconds: dur };
    if (replaced) {
      tiers[i] = { ...base, id: tiers[i].id, countFrom: tiers[i].countFrom }; // keeps its id and its countFrom
    } else {
      if (tiers.length >= LIMITS.ESCALATION_TIERS) {
        throw new ModConfigError(`You can have at most ${LIMITS.ESCALATION_TIERS} escalation tiers. Remove one first.`);
      }
      tiers.push({ ...base, id: Math.max(0, ...tiers.map((t) => t.id)) + 1, countFrom: null });
    }
    return { infractionEscalation: normalizeEscalationTiers(tiers) };
  });

  const tiers = normalizeEscalationTiers(saved.infractionEscalation);
  const mine = tiers.find((t) => t.warnCount === warns && t.withinSeconds === within);
  let text = `${mine ? describeTier(mine) : 'Saved.'}\n\n${tierListText(tiers)}`;
  if (!saved.modEnabled) text += '\n\nModeration is off: tiers apply after `/module enable moderation`.';
  await interaction.editReply({ embeds: [successEmbed(replaced ? 'Escalation Tier Updated' : 'Escalation Tier Added', clampText(text, 4000))] });
}

async function escalationRemove(interaction) {
  const id = interaction.options.getInteger('tier', true);
  const saved = await updateSettingsWith(interaction.guildId, (cur) => {
    const tiers = normalizeEscalationTiers(cur.infractionEscalation);
    const i = tiers.findIndex((t) => t.id === id);
    if (i < 0) throw new ModConfigError(`There is no tier T${id}. See \`/modconfig escalation list\`.`);
    tiers.splice(i, 1);
    return { infractionEscalation: normalizeEscalationTiers(tiers) };
  });
  const tiers = normalizeEscalationTiers(saved.infractionEscalation);
  await interaction.editReply({
    embeds: [successEmbed('Escalation Tier Removed', clampText(`Removed tier T${id}.\n\n${tierListText(tiers)}`, 4000))],
  });
}

async function escalationList(interaction, settings) {
  const tiers = normalizeEscalationTiers(settings?.infractionEscalation);
  const embed = guildEmbed(settings, '📈 Escalation Tiers', tiers.length ? null : 'No escalation tiers. Add one with `/modconfig escalation add`.',
    'Tiers fire once per window. Only active (not pardoned) warnings count.');
  chunkLines(tiers.map(tierLine), 1024).forEach((chunk, i) => embed.addFields({ name: i === 0 ? 'Tiers' : '​', value: chunk }));
  await interaction.editReply({ embeds: [embed] });
}

/**
 * Autocomplete for `escalation remove tier:`. Uses the settings memo first and bounds a cold load by the
 * acknowledgement budget (00-overview rule 21). Never throws.
 * @param {import('discord.js').AutocompleteInteraction} interaction
 * @returns {Promise<void>}
 */
export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    if (focused.name !== 'tier') return await respondAutocomplete(interaction, []);
    const settings = peekSettings(interaction.guildId)
      ?? await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'modconfig autocomplete settings').catch(() => null);
    const needle = String(focused.value ?? '').toLowerCase();
    const choices = normalizeEscalationTiers(settings?.infractionEscalation)
      .filter((t) => describeTier(t).toLowerCase().includes(needle))
      .map((t) => ({ name: describeTier(t), value: t.id }));
    return await respondAutocomplete(interaction, choices.slice(0, 25));
  } catch (err) {
    logger.debug(`modconfig autocomplete failed: ${err?.message ?? err}`);
    return undefined;
  }
}

// ── Immune roles / moderator roles (one shared implementation) ────────────────

const ROLE_LISTS = {
  immune: { field: 'immuneRoleIds', cap: LIMITS.IMMUNE_ROLES, label: 'Immune Roles' },
  modroles: { field: 'modRoleIds', cap: LIMITS.MOD_ROLES, label: 'Moderator Roles' },
};

async function roleListAdd(interaction, kind) {
  const { field, cap } = ROLE_LISTS[kind];
  const role = interaction.options.getRole('role', true);
  if (role.id === interaction.guildId) throw new ModConfigError('@everyone cannot be used here.');
  if (kind === 'modroles' && role.managed) throw new ModConfigError('That role is managed by an integration.');
  let outcome = 'added';
  await updateSettingsWith(interaction.guildId, (cur) => {
    const ids = [...(cur[field] ?? [])];
    if (ids.includes(role.id)) { outcome = 'exists'; return null; }
    if (ids.length >= cap) throw new ModConfigError(`You can have at most ${cap} roles here.`);
    outcome = 'added';
    return { [field]: [...ids, role.id] };
  });
  let text = outcome === 'exists' ? `<@&${role.id}> is already on the list.` : `Added <@&${role.id}>.`;
  if (kind === 'modroles') {
    text += `\n\nMembers with <@&${role.id}> can now use the bot's moderation commands. Discord shows /ban, /kick… only to members with the matching permission by default; to show them to this role, allow it under Server Settings → Integrations → ${interaction.client.user.username}.`;
  } else {
    text += '\n\nMembers with this role cannot be warned, muted, kicked or banned through the bot (except by the server owner) and are skipped by automod.';
  }
  await interaction.editReply({
    embeds: [outcome === 'exists' ? infoEmbed('Already Listed', text) : successEmbed('Role Added', clampText(text, 4000))],
  });
}

async function roleListRemove(interaction, kind) {
  const { field } = ROLE_LISTS[kind];
  const role = interaction.options.getRole('role', true);
  let outcome = 'removed';
  await updateSettingsWith(interaction.guildId, (cur) => {
    const ids = [...(cur[field] ?? [])];
    if (!ids.includes(role.id)) { outcome = 'missing'; return null; }
    outcome = 'removed';
    return { [field]: ids.filter((id) => id !== role.id) };
  });
  await interaction.editReply({
    embeds: [outcome === 'missing'
      ? infoEmbed('Not Listed', `<@&${role.id}> is not on the list.`)
      : successEmbed('Role Removed', `Removed <@&${role.id}>.`)],
  });
}

async function roleListShow(interaction, kind, current) {
  const { field, label } = ROLE_LISTS[kind];
  const guild = interaction.guild;
  let settings = current;
  let ids = [...(settings[field] ?? [])];
  let pruned = 0;
  if (ids.length && guild.available !== false) {
    // A role missing from the cache is not a deleted role (00-overview rule 20): confirm against the full REST role list.
    const roles = await guild.roles.fetch().catch(() => null);
    if (roles && roles.size > 0) {
      const stale = ids.filter((id) => !roles.has(id));
      if (stale.length) {
        settings = await updateSettingsWith(interaction.guildId, (cur) => ({ [field]: (cur[field] ?? []).filter((id) => !stale.includes(id)) }));
        pruned = stale.length;
        ids = [...(settings[field] ?? [])];
      }
    }
  }
  const lines = [];
  if (pruned) lines.push(`Removed ${pruned} deleted role(s).`);
  lines.push(ids.length ? joinWithLimit(ids.map((id) => `<@&${id}>`), { max: 4000 }) : 'None.');
  if (kind === 'modroles') lines.push('', 'Members with Kick, Ban or Moderate Members also count as moderators.');
  await interaction.editReply({ embeds: [guildEmbed(settings, `🛡️ ${label}`, clampText(lines.join('\n'), 4000))] });
}

// ── Mute role ─────────────────────────────────────────────────────────────────

const DENY = {
  SendMessages: false, SendMessagesInThreads: false, CreatePublicThreads: false, CreatePrivateThreads: false,
  AddReactions: false, Speak: false, SendVoiceMessages: false, SendPolls: false,
};
const OVERWRITE_TYPES = new Set([
  ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildVoice,
  ChannelType.GuildStageVoice, ChannelType.GuildForum, ChannelType.GuildMedia,
]);

/**
 * Denies talking for the mute role in every category and non-thread channel (children are included because API
 * edits to a category do not re-sync them). Never throws; per-channel failures are counted.
 * @returns {Promise<{ applied: number, failed: number }>}
 */
async function applyMuteOverwrites(guild, role) {
  const targets = [...guild.channels.cache.values()].filter(
    (ch) => !ch.isThread?.() && (ch.type === ChannelType.GuildCategory || OVERWRITE_TYPES.has(ch.type)),
  );
  let applied = 0;
  let failed = 0;
  const workable = [];
  for (const ch of targets) {
    if (missingBotPermissions(ch, ['ViewChannel', 'ManageRoles']).length) failed += 1;
    else workable.push(ch);
  }
  const results = await mapLimit(workable, 3, (ch) => ch.permissionOverwrites.edit(role, DENY, { reason: 'Mute role setup' }));
  for (const r of results) {
    if (r.status === 'fulfilled') applied += 1;
    else failed += 1;
  }
  return { applied, failed };
}

async function muteRoleCommand(interaction, settings) {
  const guild = interaction.guild;
  const role = interaction.options.getRole('role');
  const setup = interaction.options.getBoolean('setup') ?? false;
  const clear = interaction.options.getBoolean('clear') ?? false;

  if (clear) {
    await updateSettings(guild.id, { muteRoleId: null });
    const active = await prisma.infraction.count({ where: { guildId: guild.id, type: 'MUTE', active: true } });
    let text = 'The mute role was cleared. Mutes of 28 days or less use Discord timeouts.';
    if (active > 0) {
      text += `\n\n${active} role mute(s) are still active. Scheduled mutes still expire on time, but \`/unmute\` can no longer remove the old role; remove it by hand if needed.`;
    }
    await interaction.editReply({ embeds: [successEmbed('Mute Role Cleared', text)] });
    return;
  }

  let target = role;
  if (target) {
    const chk = checkAssignableRole(guild, target, { executor: interaction.member, allowDangerous: false });
    if (!chk.ok) throw new ModConfigError(chk.reason);
  } else if (setup) {
    if (missingBotPermissions(guild, ['ManageRoles']).length) {
      await interaction.editReply({ embeds: [errorEmbed('Missing Permissions', 'I need **Manage Roles** to create the mute role.')] });
      return;
    }
    target = guild.roles.cache.get(settings?.muteRoleId) ?? null; // reuse the configured role instead of creating a duplicate
    if (target) {
      const chk = checkAssignableRole(guild, target, { executor: interaction.member, allowDangerous: false });
      if (!chk.ok) throw new ModConfigError(chk.reason);
    } else {
      target = await guild.roles.create({
        name: 'Muted',
        permissions: [],
        reason: clampText(`Mute role created by ${interaction.user.username} via /modconfig`, 512),
      });
    }
  } else {
    const current = settings?.muteRoleId;
    const text = current
      ? (guild.roles.cache.has(current) ? `The mute role is <@&${current}>.` : 'The mute role is set, but the role no longer exists. Set a new one with `/modconfig muterole`.')
      : 'No mute role is set. Mutes of 28 days or less use Discord timeouts.';
    await interaction.editReply({ embeds: [guildEmbed(settings, '🔇 Mute Role', text)] });
    return;
  }

  await updateSettings(guild.id, { muteRoleId: target.id });
  const lines = [`**Mute role:** <@&${target.id}>`];
  if (setup) {
    const { applied, failed } = await applyMuteOverwrites(guild, target);
    lines.push(`**Channel overrides:** ${applied} applied${failed ? `, ${failed} failed (missing access)` : ''}`);
    lines.push('New channels need the override too: run `/modconfig muterole setup:true` again after adding channels.');
  }
  lines.push('Mutes of 28 days or less still use Discord timeouts; longer or permanent mutes use this role.');
  await interaction.editReply({ embeds: [successEmbed('Mute Role Saved', clampText(lines.join('\n'), 4000))] });
}

// ── Case log, visibility, DM ──────────────────────────────────────────────────

async function logChannelCommand(interaction) {
  const channel = interaction.options.getChannel('channel');
  if (!channel) {
    await updateSettings(interaction.guildId, { modLogChannelId: null });
    await interaction.editReply({ embeds: [successEmbed('Moderation Log', 'Case logging turned off.')] });
    return;
  }
  const u = checkChannelUsable(channel);
  if (!u.ok) {
    throw new ModConfigError(u.missing.length
      ? `I can't post in <#${channel.id}>: missing **${permissionNames(u.missing).join(', ')}**.`
      : `I can't post in <#${channel.id}>: ${u.reason}`);
  }
  await updateSettings(interaction.guildId, { modLogChannelId: channel.id });
  await channel.send({ embeds: [successEmbed('Moderation Log', 'Moderation cases will be posted here.')] }).catch(() => {});
  await interaction.editReply({
    embeds: [successEmbed('Moderation Log', `Case logs will go to <#${channel.id}>. They are posted even when the Audit Logs module is off.`)],
  });
}

const VISIBILITY_TEXT = {
  PUBLIC: 'Action confirmations are posted publicly.',
  ANONYMOUS: 'A public notice is posted without the moderator; the moderator gets a private confirmation. /case replies (history included) become private.',
  PRIVATE: 'Only the moderator sees confirmations. /case replies (history included) are private.',
};

async function visibilityCommand(interaction) {
  const mode = interaction.options.getString('mode', true);
  await updateSettings(interaction.guildId, { modVisibility: mode });
  await interaction.editReply({ embeds: [successEmbed('Command Visibility', `**${mode.charAt(0) + mode.slice(1).toLowerCase()}**\n${VISIBILITY_TEXT[mode]}`)] });
}

async function dmCommand(interaction) {
  const enabled = interaction.options.getBoolean('enabled', true);
  await updateSettings(interaction.guildId, { modDmTargets: enabled });
  await interaction.editReply({
    embeds: [successEmbed('Member DMs', enabled
      ? 'Members will be DMed when they are warned, timed out, muted, kicked or banned.'
      : 'Members will no longer be DMed about moderation actions.')],
  });
}

// ── Show ──────────────────────────────────────────────────────────────────────

async function showCommand(interaction, settings) {
  const guild = interaction.guild;
  const tiers = normalizeEscalationTiers(settings.infractionEscalation);

  let caseLog = 'Not set';
  if (settings.modLogChannelId) {
    const channel = guild.channels.cache.get(settings.modLogChannelId) ?? await guild.channels.fetch(settings.modLogChannelId).catch(() => null);
    caseLog = checkChannelUsable(channel).ok ? `<#${settings.modLogChannelId}>` : `<#${settings.modLogChannelId}> ⚠️ unusable`;
  }
  let muteRole = 'Not set';
  if (settings.muteRoleId) muteRole = guild.roles.cache.has(settings.muteRoleId) ? `<@&${settings.muteRoleId}>` : '⚠️ deleted';

  let escalation = 'None';
  if (tiers.length) {
    const chunks = chunkLines(tiers.map(tierLine), 1024);
    escalation = chunks[0];
    const shown = chunks[0].split('\n').length;
    if (shown < tiers.length) escalation = clampText(`${chunks[0]}\n… ${tiers.length - shown} more`, 1024);
  }

  const overdue = await countOverdueBans(guild.id);
  const embed = guildEmbed(settings, '🔨 Moderation Settings', overdue
    ? `⚠️ ${overdue} temporary ban(s) expired more than 30 days ago and were not lifted automatically. Use /unban to lift them.`
    : null);
  embed.addFields(
    { name: 'Module', value: settings.modEnabled ? '✅ On' : '❌ Off (enable with /module enable moderation)', inline: true },
    { name: 'Case log', value: clampText(caseLog, 1024), inline: true },
    { name: 'Visibility', value: settings.modVisibility ?? 'PUBLIC', inline: true },
    { name: 'DM members', value: settings.modDmTargets === false ? 'Off' : 'On', inline: true },
    { name: 'Mute role', value: clampText(muteRole, 1024), inline: true },
    { name: 'Moderator roles', value: rolesLine(settings.modRoleIds ?? []) },
    { name: 'Immune roles', value: rolesLine(settings.immuneRoleIds ?? []) },
    { name: 'Escalation', value: escalation },
  );
  await interaction.editReply({ embeds: [embed] });
}

// ── Entry point ───────────────────────────────────────────────────────────────

// Per subcommand for resolveNewOptions: pickers that need a value, and channels created hidden from @everyone.
const NEW_TARGETS = { 'modroles add': { required: ['role'] }, logchannel: { private: ['channel'] } };

/**
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @returns {Promise<void>}
 */
export async function execute(interaction) {
  await interaction.deferReply(); // public, like every admin config command; the first await (acknowledge within 3 s)
  try {
    const group = interaction.options.getSubcommandGroup(false);
    const sub = interaction.options.getSubcommand();
    if (!(await resolveNewOptions(interaction, data, NEW_TARGETS[group ? `${group} ${sub}` : sub]))) return;
    const needsSettings = ['list', 'show', 'muterole'].includes(sub);
    const settings = needsSettings ? await getSettingsSafe(interaction.guildId) : null;
    if (needsSettings && !settings) {
      await interaction.editReply({ embeds: [errorEmbed('Settings Unavailable', 'Server settings are temporarily unavailable. Please try again in a moment.')] });
      return;
    }

    if (group === 'escalation') {
      if (sub === 'add') await escalationAdd(interaction);
      else if (sub === 'remove') await escalationRemove(interaction);
      else await escalationList(interaction, settings);
    } else if (group === 'immune' || group === 'modroles') {
      if (sub === 'add') await roleListAdd(interaction, group);
      else if (sub === 'remove') await roleListRemove(interaction, group);
      else await roleListShow(interaction, group, settings);
    } else if (sub === 'muterole') await muteRoleCommand(interaction, settings);
    else if (sub === 'logchannel') await logChannelCommand(interaction);
    else if (sub === 'visibility') await visibilityCommand(interaction);
    else if (sub === 'dm') await dmCommand(interaction);
    else await showCommand(interaction, settings);
  } catch (err) {
    if (isInteractionGone(err)) return;
    let embed;
    if (err instanceof ModConfigError) {
      embed = errorEmbed('Not Saved', err.message);
    } else {
      logger.error('/modconfig failed', err);
      embed = errorEmbed('Not Saved', 'Saving failed. Please try again.');
    }
    await interaction.editReply({ embeds: [embed] }).catch(() => {});
  }
}
