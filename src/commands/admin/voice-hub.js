// /voice-hub (pkg-temp-voice): configuration of the join-to-create hubs (MEE6 "Temporary Channels", which only has a dashboard).
// create / edit / delete / list plus ignore-role and mod-role groups. Validation, replies and calls into the service; the runtime
// behaviour (channel creation, ownership, keep-alive) lives in services/tempVoiceService.js.
// Replies are public (CLAUDE.md admin convention); `delete` defers ephemerally, confirms, then posts the result publicly.
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import {
  applyGuildColor, errorEmbed, infoEmbed, successEmbed, warnEmbed,
} from '../../utils/embedBuilder.js';
import {
  awaitConfirm, missingBotPermissions, permissionNames, safeDefer, safeReply,
} from '../../utils/interactionHelpers.js';
import { getSettingsSafe, isModuleEnabled } from '../../services/settingsService.js';
import * as tv from '../../services/tempVoiceService.js';
import { clampText } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = 'tempvoice';
export const gated = false; // configuration works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;
// No subcommandPermissions, no allowInDMs, no autocomplete: hubs are chosen with a voice-channel option.

const HUB_TYPES = [ChannelType.GuildVoice];

function addHubSettings(s) {
  return s
    .addChannelOption((o) => o.setName('category').setDescription("Category for new channels (default: the hub's)").addChannelTypes(ChannelType.GuildCategory))
    .addStringOption((o) => o.setName('name_format').setDescription("Name format (default #{index} - {username}'s Channel)").setMinLength(1).setMaxLength(100))
    .addIntegerOption((o) => o.setName('limit').setDescription('User limit (0 = no limit)').setMinValue(0).setMaxValue(99))
    .addIntegerOption((o) => o.setName('bitrate').setDescription('Bitrate in kbps (capped by boost level)').setMinValue(8).setMaxValue(384))
    .addIntegerOption((o) => o.setName('keep_alive').setDescription('Minutes an empty channel is kept (0 = delete at once)').setMinValue(0).setMaxValue(10080))
    .addIntegerOption((o) => o.setName('ownership_lock').setDescription('Minutes before an ownerless channel can be claimed').setMinValue(0).setMaxValue(1440))
    .addBooleanOption((o) => o.setName('text_channel').setDescription('Also create a companion text channel'))
    .addBooleanOption((o) => o.setName('text_private').setDescription('Text channel only for voice members (default True)'))
    .addBooleanOption((o) => o.setName('sync_permissions').setDescription("Copy the category's permissions (False: copy the hub's)"));
}
const hubOpt = (desc) => (o) => o.setName('hub').setDescription(desc).setRequired(true).addChannelTypes(...HUB_TYPES);
const roleOpt = (o) => o.setName('role').setDescription('Role').setRequired(true);

export const data = new SlashCommandBuilder()
  .setName('voice-hub')
  .setDescription('Join-to-create voice hubs (MEE6 Temporary Channels).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => addHubSettings(s.setName('create').setDescription('Create a join-to-create hub (up to 10).')
    .addChannelOption((o) => o.setName('channel').setDescription('Voice channel to use as the hub (omit to create one)').addChannelTypes(...HUB_TYPES))))
  .addSubcommand((s) => addHubSettings(s.setName('edit').setDescription("Change a hub's settings.")
    .addChannelOption(hubOpt('Hub to edit'))))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete a hub and its temporary channels.')
    .addChannelOption(hubOpt('Hub to delete'))
    .addBooleanOption((o) => o.setName('delete_channel').setDescription('Also delete the hub voice channel (default False)')))
  .addSubcommand((s) => s.setName('list').setDescription('List hubs and their settings.'))
  .addSubcommandGroup((g) => g.setName('ignore-role').setDescription('Roles that /voice commands do not affect')
    .addSubcommand((s) => s.setName('add').setDescription('Protect a role from /voice commands').addChannelOption(hubOpt('Hub')).addRoleOption(roleOpt))
    .addSubcommand((s) => s.setName('remove').setDescription('Stop protecting a role').addChannelOption(hubOpt('Hub')).addRoleOption(roleOpt)))
  .addSubcommandGroup((g) => g.setName('mod-role').setDescription('Roles that may use /voice on any temporary channel')
    .addSubcommand((s) => s.setName('add').setDescription('Add a temp-voice moderator role').addChannelOption(hubOpt('Hub')).addRoleOption(roleOpt))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove a temp-voice moderator role').addChannelOption(hubOpt('Hub')).addRoleOption(roleOpt)));

const MODULE_OFF = '\n\n⚠️ The Temporary Channels module is off, so hubs do nothing. Turn it on with `/module enable tempvoice`.';
const moduleNote = (settings) => (isModuleEnabled(settings, 'tempvoice') ? '' : MODULE_OFF);
const shown = (name) => escapeMarkdown(clampText(name ?? 'unknown', 100));
const CREATE_HINT = '`/voice-hub create`';

// ── Small formatters (the "old → new" lines of /voice-hub edit) ───────────────

const limitText = (n) => (n > 0 ? String(n) : 'none');
const keepText = (n) => (n > 0 ? `${n} min` : 'delete at once');
const lockText = (n) => (n > 0 ? `${n} min` : 'none');
const bitrateText = (bps) => (bps ? `${Math.round(bps / 1000)} kbps` : 'default');
const categoryText = (id) => (id ? `<#${id}>` : "hub's category");
const permsText = (sync) => (sync ? 'category' : 'hub channel');
const textMode = (h) => (!h.createTextChannel ? 'off' : (h.textChannelMembersOnly ? 'private' : 'public'));

const limitReached = () => errorEmbed(
  'Limit Reached',
  `A server can have at most ${LIMITS.TEMPVOICE_HUBS} hubs. Delete one with \`/voice-hub delete\` first.`,
);
const notAHub = (channel) => errorEmbed('Not A Hub', `${channel} is not a hub. See \`/voice-hub list\`.`);
const alreadyAHub = (channel) => errorEmbed('Already A Hub', `${channel} is already a hub. Change it with \`/voice-hub edit\`.`);
const internalError = () => errorEmbed('Something Went Wrong', 'I could not save that hub. Please try again in a moment.');

/** Name-format validation shared by create and edit. @returns {{ error?: object, preview?: string, warnings: string[] }} */
function checkNameFormat(nameFormat, interaction) {
  const v = tv.validateNameFormat(nameFormat ?? tv.DEFAULT_NAME_FORMAT, { member: interaction.member, guild: interaction.guild });
  if (!v.ok) return { error: errorEmbed('Invalid Name Format', v.error), warnings: [] };
  return { preview: v.preview, warnings: nameFormat != null ? v.warnings : [] };
}

/** The bot-permission errors for a hub that creates channels in `targetParent` (null = top level). @returns {object|null} an embed */
function permissionError(interaction, targetParent, wantsText) {
  const need = [...tv.REQUIRED_BOT_PERMS, ...(wantsText ? ['SendMessages', 'EmbedLinks'] : [])];
  const missing = missingBotPermissions(targetParent ?? interaction.guild, need);
  if (!missing.length) return null;
  const where = targetParent ? `**${shown(targetParent.name)}**` : 'this server';
  return errorEmbed('Missing Permissions', `I need **${permissionNames(missing).join(', ')}** in ${where} to create temporary channels.`);
}

function hubChannelPermissionError(hubChannel) {
  const missing = missingBotPermissions(hubChannel, ['ViewChannel', 'Connect', 'MoveMembers']);
  if (!missing.length) return null;
  return errorEmbed('Missing Permissions', `I need **${permissionNames(missing).join(', ')}** in ${hubChannel} to move members out of the hub.`);
}

function bitrateWarning(hub, guild) {
  if (hub.bitrate && hub.bitrate > guild.maximumBitrate) {
    return `This server currently allows up to ${guild.maximumBitrate / 1000} kbps; new channels use that until the boost level rises.`;
  }
  return null;
}

const warningLines = (warnings) => warnings.map((w) => `\n⚠️ ${w}`).join('');

// ── create ────────────────────────────────────────────────────────────────────

async function runCreate(interaction, settings) {
  const { guild, guildId, options } = interaction;
  const out = (embed) => interaction.editReply({ embeds: [embed] });
  const channel = options.getChannel('channel');
  const category = options.getChannel('category');
  const nameFormat = options.getString('name_format');
  const limit = options.getInteger('limit');
  const bitrate = options.getInteger('bitrate');
  const keepAlive = options.getInteger('keep_alive');
  const ownershipLock = options.getInteger('ownership_lock');
  const textChannel = options.getBoolean('text_channel');
  const textPrivate = options.getBoolean('text_private');
  const syncPermissions = options.getBoolean('sync_permissions');

  if (await tv.countHubs(guildId) >= LIMITS.TEMPVOICE_HUBS) return out(limitReached());
  if (channel) {
    if (await tv.findHubByChannel(guildId, channel.id)) return out(alreadyAHub(channel));
    if (await tv.isTempChannel(channel.id)) return out(errorEmbed('Temporary Channel', `${channel} is a temporary channel and cannot be a hub.`));
  }

  const nf = checkNameFormat(nameFormat, interaction);
  if (nf.error) return out(nf.error);

  const targetParent = category ?? channel?.parent ?? null;
  const problem = permissionError(interaction, targetParent, textChannel === true) ?? (channel ? hubChannelPermissionError(channel) : null);
  if (problem) return out(problem);

  let hubChannel = channel;
  let createdChannel = null;
  if (!hubChannel) {
    const made = await tv.discordCall('create hub channel', () => guild.channels.create({
      name: tv.HUB_CHANNEL_NAME,
      type: ChannelType.GuildVoice,
      parent: category?.id,
      reason: clampText(`Temp voice hub created by ${interaction.user.tag}`, 512),
    }), guildId);
    if (!made.ok) {
      return out(errorEmbed('Could Not Create Channel', `Discord refused to create the hub channel (${tv.describeDiscordError(made.code)}).`));
    }
    hubChannel = made.value;
    createdChannel = made.value;
  }
  const rollback = async () => {
    if (createdChannel) await tv.deleteDiscordChannel(guild, createdChannel.id, 'Temp voice: hub was not saved', 'rollback hub channel');
  };

  // Only the options that were given: undefined keys are omitted so the schema defaults apply.
  const hubData = {
    channelId: hubChannel.id,
    categoryId: category?.id,
    nameFormat: nameFormat?.trim(),
    userLimit: limit ?? undefined,
    bitrate: bitrate != null ? bitrate * 1000 : undefined,
    keepAliveMinutes: keepAlive ?? undefined,
    ownershipLockMinutes: ownershipLock ?? undefined,
    createTextChannel: textChannel ?? undefined,
    textChannelMembersOnly: textPrivate ?? undefined,
    syncPermissions: syncPermissions ?? undefined,
  };
  for (const key of Object.keys(hubData)) if (hubData[key] === undefined || hubData[key] === null) delete hubData[key];

  let hub;
  try {
    hub = await tv.createHub(guildId, hubData);
  } catch (err) {
    if (err instanceof tv.HubLimitError) {
      await rollback();
      return out(limitReached());
    }
    if (err?.code === 'P2002') {
      await rollback();
      return out(alreadyAHub(hubChannel));
    }
    await rollback();
    throw err;
  }

  const bw = bitrateWarning(hub, guild);
  const warnings = [...nf.warnings, ...(bw ? [bw] : [])];
  return out(successEmbed(
    'Hub Created',
    `Join ${hubChannel} to get your own voice channel.\n\n${tv.hubSummary(hub, guild)}\n\n**Preview:** ${escapeMarkdown(nf.preview)}${warningLines(warnings)}${moduleNote(settings)}`,
  ));
}

// ── edit ──────────────────────────────────────────────────────────────────────

async function runEdit(interaction, settings) {
  const { guild, guildId, options } = interaction;
  const out = (embed) => interaction.editReply({ embeds: [embed] });
  const hubChannel = options.getChannel('hub', true);
  const hub = await tv.findHubByChannel(guildId, hubChannel.id);
  if (!hub) return out(notAHub(hubChannel));

  const category = options.getChannel('category');
  const nameFormat = options.getString('name_format');
  const limit = options.getInteger('limit');
  const bitrate = options.getInteger('bitrate');
  const keepAlive = options.getInteger('keep_alive');
  const ownershipLock = options.getInteger('ownership_lock');
  const textChannel = options.getBoolean('text_channel');
  const textPrivate = options.getBoolean('text_private');
  const syncPermissions = options.getBoolean('sync_permissions');

  const given = [category, nameFormat, limit, bitrate, keepAlive, ownershipLock, textChannel, textPrivate, syncPermissions];
  if (given.every((v) => v === null || v === undefined)) return out(errorEmbed('Nothing To Change', 'Pass at least one setting to change.'));

  const nf = checkNameFormat(nameFormat, interaction);
  if (nf.error) return out(nf.error);

  const effectiveParent = category ?? (hub.categoryId ? guild.channels.cache.get(hub.categoryId) : null) ?? hubChannel.parent ?? null;
  const problem = permissionError(interaction, effectiveParent, (textChannel ?? hub.createTextChannel) === true);
  if (problem) return out(problem);

  const patch = {};
  if (category) patch.categoryId = category.id;
  if (nameFormat != null) patch.nameFormat = nameFormat.trim();
  if (limit != null) patch.userLimit = limit;
  if (bitrate != null) patch.bitrate = bitrate * 1000;
  if (keepAlive != null) patch.keepAliveMinutes = keepAlive;
  if (ownershipLock != null) patch.ownershipLockMinutes = ownershipLock;
  if (textChannel != null) patch.createTextChannel = textChannel;
  if (textPrivate != null) patch.textChannelMembersOnly = textPrivate;
  if (syncPermissions != null) patch.syncPermissions = syncPermissions;

  const updated = await tv.updateHub(guildId, hub.id, patch);
  if (!updated) return out(notAHub(hubChannel));

  const lines = [];
  if (updated.userLimit !== hub.userLimit) lines.push(`• **Limit:** ${limitText(hub.userLimit)} → ${limitText(updated.userLimit)}`);
  if (updated.keepAliveMinutes !== hub.keepAliveMinutes) lines.push(`• **Keep alive:** ${keepText(hub.keepAliveMinutes)} → ${keepText(updated.keepAliveMinutes)}`);
  if (updated.nameFormat !== hub.nameFormat) lines.push(`• **Name format:** \`${hub.nameFormat.replaceAll('`', 'ˋ')}\` → \`${updated.nameFormat.replaceAll('`', 'ˋ')}\``);
  if (updated.categoryId !== hub.categoryId) lines.push(`• **Category:** ${categoryText(hub.categoryId)} → ${categoryText(updated.categoryId)}`);
  if (updated.bitrate !== hub.bitrate) lines.push(`• **Bitrate:** ${bitrateText(hub.bitrate)} → ${bitrateText(updated.bitrate)}`);
  if (updated.ownershipLockMinutes !== hub.ownershipLockMinutes) lines.push(`• **Ownership lock:** ${lockText(hub.ownershipLockMinutes)} → ${lockText(updated.ownershipLockMinutes)}`);
  if (textMode(updated) !== textMode(hub)) lines.push(`• **Text channel:** ${textMode(hub)} → ${textMode(updated)}`);
  if (updated.syncPermissions !== hub.syncPermissions) lines.push(`• **Permissions:** ${permsText(hub.syncPermissions)} → ${permsText(updated.syncPermissions)}`);
  if (!lines.length) return out(warnEmbed('No Change', 'The hub already has those settings.'));

  const bw = bitrateWarning(updated, guild);
  const warnings = [...nf.warnings, ...(bw ? [bw] : [])];
  return out(successEmbed(
    'Hub Updated',
    `${lines.join('\n')}\n\nName, limit, bitrate, permissions and text-channel settings apply to channels created from now on. `
    + `Keep alive, ownership lock and roles apply to existing channels too.${warningLines(warnings)}${moduleNote(settings)}`,
  ));
}

// ── delete ────────────────────────────────────────────────────────────────────

async function runDelete(interaction) {
  const { guild, guildId, options } = interaction;
  const hubChannel = options.getChannel('hub', true);
  const deleteChannel = options.getBoolean('delete_channel') === true;
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;

  const hub = await tv.findHubByChannel(guildId, hubChannel.id);
  if (!hub) {
    await interaction.editReply({ embeds: [notAHub(hubChannel)] });
    return;
  }
  const rows = await tv.listHubChannels(hub.id);
  const occupied = rows.filter((r) => tv.countHumans(guild.channels.cache.get(r.channelId)) > 0).length;
  const confirmed = await awaitConfirm(interaction, {
    title: 'Delete hub?',
    description: `Delete the hub ${hubChannel}? Its **${rows.length}** temporary channel(s)${occupied ? ` (**${occupied}** with members in them)` : ''} will be deleted too.`
      + `${deleteChannel ? ' The hub channel itself will also be deleted.' : ''}`,
    confirmLabel: 'Delete',
  });
  if (!confirmed) return; // the prompt already shows the outcome

  const r = await tv.deleteHub(guild, hub, { deleteHubChannel: deleteChannel, actor: interaction.user });
  if (r.notFound) {
    await interaction.followUp({ embeds: [errorEmbed('Not A Hub', 'That hub was already deleted.')], ephemeral: true });
    return;
  }
  await interaction.followUp({
    embeds: [successEmbed(
      'Hub Deleted',
      `The hub **${shown(hubChannel.name)}** was deleted and **${r.deleted}** temporary channel(s) were removed.`
      + `${r.failed ? `\n⚠️ ${r.failed} channel(s) could not be deleted; delete them by hand.` : ''}`
      + `${r.hubChannelFailed ? '\n⚠️ I could not delete the hub channel.' : ''}`,
    )],
  });
}

// ── list ──────────────────────────────────────────────────────────────────────

async function runList(interaction, settings) {
  const { guild, guildId } = interaction;
  const out = (embed) => interaction.editReply({ embeds: [embed] });
  const hubs = await tv.listHubs(guildId);
  if (!hubs.length) return out(applyGuildColor(infoEmbed('Temporary Channel Hubs', `No hubs yet. Create one with ${CREATE_HINT}.`), settings));

  const embed = applyGuildColor(infoEmbed('Temporary Channel Hubs', `${hubs.length}/${LIMITS.TEMPVOICE_HUBS} hubs`), settings);
  // A message's embeds may hold 6000 characters in all: share them out.
  const perField = Math.min(1024, Math.floor(5400 / hubs.length) - 60);
  for (const hub of hubs.slice(0, LIMITS.TEMPVOICE_HUBS)) {
    const hubChannel = guild.channels.cache.get(hub.channelId);
    const name = hubChannel ? `🔊 #${hubChannel.name}` : '🔊 Deleted channel (retired)';
    const lines = [tv.hubSummary(hub, guild), `Active channels: ${hub._count?.channels ?? 0}`, ...tv.hubProblems(hub, guild).map((p) => `⚠️ ${p}`)];
    embed.addFields({ name: clampText(name, 256), value: clampText(lines.join('\n'), perField) });
  }
  return out(embed);
}

// ── ignore-role / mod-role ────────────────────────────────────────────────────

const ROLE_GROUPS = Object.freeze({
  'ignore-role': { field: 'ignoredRoleIds', label: 'an ignored role', plural: 'ignored roles' },
  'mod-role': { field: 'moderatorRoleIds', label: 'a moderator role', plural: 'moderator roles' },
});

async function runRoleList(interaction, group, sub) {
  const { guild, guildId, options } = interaction;
  const out = (embed) => interaction.editReply({ embeds: [embed] });
  const hubChannel = options.getChannel('hub', true);
  const role = options.getRole('role', true);
  const hub = await tv.findHubByChannel(guildId, hubChannel.id);
  if (!hub) return out(notAHub(hubChannel));
  // These roles are only read for authorization and overwrites, never assigned, so checkAssignableRole is not used.
  if (role.id === guild.id) return out(errorEmbed('Invalid Role', '@everyone cannot be used here.'));

  const { field, label, plural } = ROLE_GROUPS[group];
  const r = await tv.updateHubRoleList(guildId, hub.id, field, sub, role.id);
  switch (r) {
    case 'gone': return out(notAHub(hubChannel));
    case 'exists': return out(warnEmbed('No Change', `${role} is already ${label}.`));
    case 'missing': return out(warnEmbed('No Change', `${role} is not ${label}.`));
    case 'limit': return out(errorEmbed('Limit Reached', `A hub can have at most ${tv.MAX_HUB_ROLE_LIST} ${plural}.`));
    default: break;
  }
  const settings = await getSettingsSafe(guildId);
  const note = moduleNote(settings);
  if (group === 'ignore-role') {
    return out(sub === 'add'
      ? successEmbed('Ignored Role Added', `/voice commands no longer affect members with ${role} in channels from ${hubChannel}.${note}`)
      : successEmbed('Ignored Role Removed', `/voice commands affect members with ${role} again.${note}`));
  }
  return out(sub === 'add'
    ? successEmbed('Moderator Role Added', `Members with ${role} can use every /voice command on channels from ${hubChannel}.${note}`)
    : successEmbed('Moderator Role Removed', `${role} is no longer a temp-voice moderator role.${note}`));
}

// ── execute ───────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  try {
    if (sub === 'delete' && !group) {
      await runDelete(interaction);
      return;
    }
    if (!(await safeDefer(interaction))) return;
    const settings = await getSettingsSafe(interaction.guildId);
    if (group) await runRoleList(interaction, group, sub);
    else if (sub === 'create') await runCreate(interaction, settings);
    else if (sub === 'edit') await runEdit(interaction, settings);
    else await runList(interaction, settings);
  } catch (err) {
    logger.error(`voice-hub ${group ? `${group} ` : ''}${sub} failed`, err);
    await safeReply(interaction, { embeds: [internalError()], ephemeral: true });
  }
}
