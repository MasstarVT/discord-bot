// /stats (pkg-stat-channels): configuration of the Statistics Channels (MEE6 configures this on its dashboard only).
// create / edit / delete / list / refresh / category. Validation, replies and calls into the service; the runtime behaviour (refresh
// engine, rename spacing, job, hooks) lives in services/statsService.js. Replies are public (CLAUDE.md admin convention).
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import {
  applyGuildColor, errorEmbed, infoEmbed, successEmbed, warnEmbed,
} from '../../utils/embedBuilder.js';
import { missingBotPermissions, permissionNames, respondAutocomplete } from '../../utils/interactionHelpers.js';
import { getSettingsSafe, isModuleEnabled, updateSettings } from '../../services/settingsService.js';
import * as stats from '../../services/statsService.js';
import {
  chunkLines, clampText, formatNumber,
} from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';
import logger from '../../utils/logger.js';

const { STAT_KINDS, KIND_INFO } = stats;

export const module = 'stats';
export const gated = false; // configuration works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;
// No subcommandPermissions and no allowInDMs: every subcommand is ADMIN and guild-only.

const KIND_CHOICES = STAT_KINDS.map((k) => ({ name: KIND_INFO[k].label, value: k }));

export const data = new SlashCommandBuilder()
  .setName('stats')
  .setDescription('Counter channels with live server stats (MEE6 Statistics Channels).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('create').setDescription('Create a counter channel.')
    .addStringOption((o) => o.setName('kind').setDescription('What to count').setRequired(true).addChoices(...KIND_CHOICES))
    .addStringOption((o) => o.setName('template').setDescription('Channel name with {count}, e.g. "Members: {count}"').setMinLength(1).setMaxLength(100))
    .addRoleOption((o) => o.setName('role').setDescription('Role to count (Role members only)'))
    .addChannelOption((o) => o.setName('category').setDescription('Category for this channel').addChannelTypes(ChannelType.GuildCategory)))
  .addSubcommand((s) => s.setName('edit').setDescription("Change a counter's name template.")
    .addStringOption((o) => o.setName('channel').setDescription('Counter channel').setRequired(true).setAutocomplete(true).setMaxLength(100))
    .addStringOption((o) => o.setName('template').setDescription('New name with {count}').setRequired(true).setMinLength(1).setMaxLength(100)))
  .addSubcommand((s) => s.setName('delete').setDescription('Remove a counter and its channel.')
    .addStringOption((o) => o.setName('channel').setDescription('Counter channel').setRequired(true).setAutocomplete(true).setMaxLength(100))
    .addBooleanOption((o) => o.setName('keep_channel').setDescription('Keep the channel, just stop updating it')))
  .addSubcommand((s) => s.setName('list').setDescription('List counters.'))
  .addSubcommand((s) => s.setName('refresh').setDescription('Update all counters now.'))
  .addSubcommand((s) => s.setName('category').setDescription('Set the default category for new counters.')
    .addChannelOption((o) => o.setName('category').setDescription('Category (omit to clear)').addChannelTypes(ChannelType.GuildCategory)));

const MODULE_OFF_NOTE = "\n\n⚠️ The Statistics Channels module is off, so counters don't update. Turn it on with `/module enable stats`.";
const moduleNote = (settings) => (settings && !isModuleEnabled(settings, 'stats') ? MODULE_OFF_NOTE : '');

const genericError = () => errorEmbed('Something Went Wrong', 'I could not complete that. Please try again in a moment.');
const notFound = () => errorEmbed('Not Found', "That isn't a counter channel. Pick one from the list, or see `/stats list`.");
const shownTemplate = (template, max = 100) => clampText(String(template).replaceAll('`', "'"), max);
const roleMention = (roleId) => ` (<@&${roleId}>)`;
const labelOf = (kind, roleId) => `${KIND_INFO[kind]?.label ?? kind}${roleId ? roleMention(roleId) : ''}`;

function categoryPermissionError(category) {
  const missing = missingBotPermissions(category, ['ViewChannel', 'ManageChannels']);
  if (!missing.length) return null;
  return errorEmbed('Missing Permissions', `I need **${permissionNames(missing).join(', ')}** in the **${clampText(category.name, 100)}** category.`);
}

// ── create ────────────────────────────────────────────────────────────────────

async function runCreate(interaction, client, settings) {
  const { guild, guildId, options } = interaction;
  const kind = options.getString('kind', true);
  const role = options.getRole('role');
  let category = options.getChannel('category');
  if (!STAT_KINDS.includes(kind)) return genericError();
  const label = KIND_INFO[kind].label;

  if (kind === 'ROLE_MEMBERS' && !role) {
    return errorEmbed('Role Required', 'Pick the role to count with the `role` option when using **Role members**.');
  }
  if (kind !== 'ROLE_MEMBERS' && role) {
    return errorEmbed('Role Not Used', 'The `role` option only applies to the **Role members** counter.');
  }
  // Role counters do not use checkAssignableRole: the role is counted, never assigned (deliberate exception to rule 13).
  if (role && role.id === guild.id) return errorEmbed('Invalid Role', 'Use the **Members** counter to count everyone.');

  if (stats.LIST_KINDS.has(kind) && guild.memberCount > stats.MEMBER_LIST_MAX_MEMBERS) {
    return errorEmbed(
      'Server Too Large',
      `**${label}** counters need the full member list, which I only load for servers with up to ${formatNumber(stats.MEMBER_LIST_MAX_MEMBERS)} members. Use **Members** instead.`,
    );
  }

  const v = stats.validateStatTemplate(options.getString('template') ?? KIND_INFO[kind].template, {
    kind, roleName: role?.name ?? null, guild,
  });
  if (!v.ok) return errorEmbed('Invalid Template', v.error);

  // Category: the option wins; else the saved default. Rule 20: a saved category missing from the cache is cleared only after
  // Discord confirms it is gone (10003); an unconfirmed lookup asks the admin to retry instead of guessing.
  let staleCategory = false;
  if (category) {
    category = guild.channels.cache.get(category.id) ?? category;
  } else if (settings?.statsCategoryId) {
    const chk = await stats.confirmChannel(guild, settings.statsCategoryId);
    if (chk.state === 'unknown') {
      return errorEmbed('Try Again', "I couldn't check the saved stats category right now. Please try again in a moment, or pick one with the `category` option.");
    }
    if (chk.state === 'present' && chk.channel.type === ChannelType.GuildCategory) {
      category = chk.channel;
    } else {
      staleCategory = true;
      updateSettings(guildId, { statsCategoryId: null }).catch((err) => logger.error('stats: could not clear the stats category', err));
    }
  }

  // Connect is needed because Discord lets a bot deny only permissions it has.
  const missing = missingBotPermissions(guild, ['ViewChannel', 'ManageChannels', 'Connect']);
  if (missing.length) {
    return errorEmbed('Missing Permissions', `I need **${permissionNames(missing).join(', ')}** in this server to create counter channels.`);
  }
  if (category) {
    const problem = categoryPermissionError(category);
    if (problem) return problem;
  }

  let result;
  try {
    result = await stats.createStatChannel({
      guild, client, kind, template: v.template, role, category, actorTag: interaction.user.tag,
    });
  } catch (err) {
    if (!(err instanceof stats.StatsError)) throw err;
    switch (err.code) {
      case 'LIMIT':
        return errorEmbed('Limit Reached', `A server can have at most ${LIMITS.STAT_CHANNELS} counter channels. Remove one with \`/stats delete\` first.`);
      case 'ROLE_LIMIT':
        return errorEmbed('Limit Reached', `A server can have at most ${LIMITS.ROLE_STAT_CHANNELS} role counters. Remove one with \`/stats delete\` first.`);
      case 'DUPLICATE':
        return errorEmbed('Already Exists', `<#${err.channelId}> already counts **${label}**${role ? ` for <@&${role.id}>` : ''}. Change its name with \`/stats edit\` instead.`);
      case 'MISSING_PERMISSIONS':
        return errorEmbed('Missing Permissions', 'Discord refused to create the channel. Check that I have **View Channel, Manage Channels** and **Connect** here and in the category.');
      case 'CHANNEL_LIMIT':
        return errorEmbed('Channel Limit Reached', "This server has reached Discord's limit of 500 channels.");
      case 'DISCORD_REJECTED':
        return errorEmbed('Channel Not Created', 'Discord rejected the new channel. The category may be full (50 channels max).');
      default:
        throw err;
    }
  }

  let desc = `<#${result.channel.id}> now shows **${label}**${role ? ` for <@&${role.id}>` : ''}.\n`
    + `**Template:** \`${shownTemplate(v.template)}\`\n`
    + 'It updates about every 10 minutes; Discord allows only 2 renames per channel every 10 minutes.';
  if (result.value === null) desc += '\n\nThe count is still loading and will appear on the next refresh.';
  if (staleCategory) desc += '\n\nThe saved stats category no longer exists, so the channel was created at the top level.';
  return successEmbed('Counter Created', desc + moduleNote(settings));
}

// ── edit ──────────────────────────────────────────────────────────────────────

async function runEdit(interaction, client, settings) {
  const { guild, options } = interaction;
  const row = await stats.findRowByRef(guild, options.getString('channel', true));
  if (!row) return notFound();
  const roleName = row.kind === 'ROLE_MEMBERS' ? (guild.roles.cache.get(row.roleId)?.name ?? 'role') : null;
  const v = stats.validateStatTemplate(options.getString('template', true), { kind: row.kind, roleName, guild });
  if (!v.ok) return errorEmbed('Invalid Template', v.error);
  try {
    await stats.editTemplate(guild, row, v.template);
  } catch (err) {
    if (err instanceof stats.StatsError && err.code === 'NOT_FOUND') return notFound();
    throw err;
  }
  const preview = stats.renderStatName(v.template, { count: row.lastValue, roleName, guild });
  return successEmbed(
    'Counter Updated',
    `<#${row.channelId}> will be renamed to **${preview}** on the next refresh (usually within a minute, or up to 5 minutes if it was renamed recently).${moduleNote(settings)}`,
  );
}

// ── delete ────────────────────────────────────────────────────────────────────

async function runDelete(interaction, client, settings) {
  const { guild, options } = interaction;
  const row = await stats.findRowByRef(guild, options.getString('channel', true));
  if (!row) return notFound();
  const keep = options.getBoolean('keep_channel') ?? false;
  try {
    await stats.deleteStatChannel(guild, row, { keepChannel: keep, actorTag: interaction.user.tag });
  } catch (err) {
    if (err instanceof stats.StatsError && err.code === 'MISSING_PERMISSIONS') {
      return errorEmbed(
        'Missing Permissions',
        `I couldn't delete <#${row.channelId}>: I need **View Channel, Manage Channels** there. Fix my permissions, or run this again with \`keep_channel:True\` to only stop updating it.`,
      );
    }
    throw err;
  }
  const label = labelOf(row.kind, row.roleId);
  const desc = keep
    ? `The **${label}** counter was removed. <#${row.channelId}> was kept and no longer updates.`
    : `The **${label}** counter was removed and its channel deleted.`;
  return successEmbed('Counter Removed', desc + moduleNote(settings));
}

// ── list ──────────────────────────────────────────────────────────────────────

const FIELDS_BUDGET = 5400; // embed total is 6000; leaves room for the title and the description

function rowLine(guild, row) {
  const peak = row.kind === 'PEAK_ONLINE' && row.peakValue != null ? ` · peak ${formatNumber(row.peakValue)}` : '';
  const renamed = row.lastRenamedAt ? `renamed ${discordTimestamp(row.lastRenamedAt, 'R')}` : 'not renamed yet';
  const showing = row.lastValue != null ? formatNumber(row.lastValue) : '—';
  let line = clampText(
    `**${labelOf(row.kind, row.roleId)}** · <#${row.channelId}> · \`${shownTemplate(row.template, 40)}\` · showing **${showing}**${peak} · ${renamed}`,
    240,
  );
  const problem = stats.inspectRow(guild, row);
  if (problem) line += `\n⚠️ ${stats.describeProblem(problem)}`;
  return line;
}

async function runList(interaction, client, settings) {
  const { guild, guildId } = interaction;
  const rows = await stats.listRows(guildId);
  const roleCount = rows.filter((r) => r.kind === 'ROLE_MEMBERS').length;
  const header = `${rows.length}/${LIMITS.STAT_CHANNELS} counters · ${roleCount}/${LIMITS.ROLE_STAT_CHANNELS} role counters\n`
    + `New counters go to: ${settings?.statsCategoryId ? `<#${settings.statsCategoryId}>` : 'top level'}\n`
    + 'Names refresh about every 10 minutes.'
    + moduleNote(settings);
  if (rows.length === 0) {
    return applyGuildColor(infoEmbed('Statistics Channels', `${header}\n\nNo counters yet. Create one with \`/stats create\`.`), settings);
  }
  const lines = [];
  let used = 0;
  let shown = 0;
  for (const row of rows) {
    const line = rowLine(guild, row);
    if (used + line.length + 1 > FIELDS_BUDGET) break; // never exceed the 6000-character embed budget
    lines.push(line);
    used += line.length + 1;
    shown++;
  }
  if (shown < rows.length) lines.push(`… and ${rows.length - shown} more`);
  const fields = chunkLines(lines, 1024).map((value, i) => ({ name: i === 0 ? 'Counters' : '​', value }));
  return applyGuildColor(infoEmbed('Statistics Channels', header).addFields(fields), settings);
}

// ── refresh ───────────────────────────────────────────────────────────────────

async function runRefresh(interaction, client, settings) {
  if (!settings) return errorEmbed('Temporarily Unavailable', 'Server settings could not be loaded. Please try again in a moment.');
  if (!isModuleEnabled(settings, 'stats')) {
    return warnEmbed('Module Disabled', 'The **Statistics Channels** module is turned off, so counters are not updated. Turn it on with `/module enable stats`.');
  }
  const out = await stats.refreshNow(interaction.guild);
  if (out.empty) return applyGuildColor(infoEmbed('Statistics Channels', 'No counters yet. Create one with `/stats create`.'), settings);
  if (out.busy) {
    return applyGuildColor(infoEmbed('Refresh Running', 'Counters are being updated right now. Check `/stats list` in a minute.'), settings);
  }
  const r = out.result;
  const lines = [`✅ Renamed: ${r.renamed}`];
  if (r.unchanged > 0) lines.push(`➖ Already up to date: ${r.unchanged}`);
  if (r.waiting.length > 0) {
    const next = Math.min(...r.waiting.map((w) => w.retryAt));
    lines.push(`⏳ Waiting for Discord's rename limit: ${r.waiting.length} (next ${discordTimestamp(next, 'R')})`);
  }
  if (r.removed > 0) lines.push(`🗑️ Removed (channel was deleted): ${r.removed}`);
  if (r.deferred > 0) lines.push(`⏸️ Not started (time budget): ${r.deferred}`);
  for (const s of r.skipped.slice(0, 10)) lines.push(`⚠️ <#${s.channelId}>: ${stats.describeProblem(s.problem)}`);
  if (r.skipped.length > 10) lines.push(`… and ${r.skipped.length - 10} more`);
  return successEmbed('Counters Refreshed', lines.join('\n'));
}

// ── category ──────────────────────────────────────────────────────────────────

async function runCategory(interaction, client, settings) {
  const { guild, guildId, options } = interaction;
  const picked = options.getChannel('category');
  if (picked) {
    const cat = guild.channels.cache.get(picked.id) ?? picked;
    const problem = categoryPermissionError(cat);
    if (problem) return problem;
    await updateSettings(guildId, { statsCategoryId: cat.id });
    return successEmbed(
      'Stats Category Set',
      `New counters will be created in **${clampText(cat.name, 100)}**. Existing counters stay where they are.${moduleNote(settings)}`,
    );
  }
  await updateSettings(guildId, { statsCategoryId: null });
  return successEmbed(
    'Stats Category Cleared',
    `New counters will be created at the top level, outside any category.${moduleNote(settings)}`,
  );
}

// ── Entry points ──────────────────────────────────────────────────────────────

const RUNNERS = Object.freeze({
  create: runCreate, edit: runEdit, delete: runDelete, list: runList, refresh: runRefresh, category: runCategory,
});

export async function execute(interaction, client) {
  const sub = interaction.options.getSubcommand();
  try {
    await interaction.deferReply();
    const settings = await getSettingsSafe(interaction.guildId);
    const run = RUNNERS[sub];
    const embed = run ? await run(interaction, client, settings) : genericError();
    await interaction.editReply({ embeds: [embed] });
  } catch (err) {
    logger.error(`stats ${sub} failed`, err);
    await interaction.editReply({ embeds: [genericError()] }).catch(() => {});
  }
}

export async function autocomplete(interaction) {
  const focused = String(interaction.options.getFocused() ?? '').toLowerCase().slice(0, 100);
  const rows = interaction.guildId ? await stats.listRowsSafe(interaction.guildId) : []; // [] on error; at most 25, ordered by createdAt
  const choices = rows.map((r) => {
    const ch = interaction.guild?.channels.cache.get(r.channelId);
    const role = r.roleId ? interaction.guild?.roles.cache.get(r.roleId) : null;
    return {
      name: `${ch?.name ?? 'unavailable channel'} — ${KIND_INFO[r.kind]?.label ?? r.kind}${role ? ` (@${role.name})` : ''}`,
      value: r.channelId,
    };
  }).filter((c) => !focused || c.name.toLowerCase().includes(focused));
  await respondAutocomplete(interaction, choices); // clamps to 25 choices and 100-character names
}
