// /voicerole add | remove | any add | any remove | list | sync. Admin config command: replies are public (CLAUDE.md admin
// convention) and it keeps working while the Voice Roles module is off (gated = false). Every subcommand is ADMIN, so none
// needs the "module is off" refusal; only `sync` refuses while the module is off (the hooks that keep roles right are off
// too). The hooks are src/plugins/voice-roles.js, the rules src/services/voiceRoleService.js.
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import {
  VOICE_ROLES, linksOf, anyRolesOf, hasVoiceRoles, addLink, removeLink, addAnyRole, removeAnyRole, reconcileGuild,
} from '../../services/voiceRoleService.js';
import { successEmbed, errorEmbed, infoEmbed, warnEmbed, guildEmbed } from '../../utils/embedBuilder.js';
import {
  safeDefer, safeReply, missingBotPermissions, permissionNames, checkAssignableRole,
} from '../../utils/interactionHelpers.js';
import { chunkLines, joinWithLimit, formatNumber } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = 'voiceroles';
export const gated = false;                          // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const LINKABLE_TYPES = [ChannelType.GuildVoice, ChannelType.GuildStageVoice, ChannelType.GuildCategory];
const LINKABLE = new Set(LINKABLE_TYPES);

export const data = new SlashCommandBuilder()
  .setName('voicerole')
  .setDescription('Give members a role while they are in a voice channel.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('add').setDescription('Give a role to members in a voice channel or category.')
    .addChannelOption((o) => o.setName('channel').setDescription('A voice channel, stage or category.')
      .addChannelTypes(...LINKABLE_TYPES).setRequired(true))
    .addRoleOption((o) => o.setName('role').setDescription('A role made only for this.').setRequired(true)))
  .addSubcommand((s) => s.setName('remove').setDescription('Stop giving a role to a voice channel or category.')
    .addChannelOption((o) => o.setName('channel').setDescription('The channel or category.')
      .addChannelTypes(...LINKABLE_TYPES).setRequired(true))
    .addRoleOption((o) => o.setName('role').setDescription('The role.').setRequired(true)))
  .addSubcommandGroup((g) => g.setName('any').setDescription('Roles for every voice channel.')
    .addSubcommand((s) => s.setName('add').setDescription('Give a role to members in any voice channel.')
      .addRoleOption((o) => o.setName('role').setDescription('A role made only for this.').setRequired(true)))
    .addSubcommand((s) => s.setName('remove').setDescription('Stop giving a role in every voice channel.')
      .addRoleOption((o) => o.setName('role').setDescription('The role.').setRequired(true))))
  .addSubcommand((s) => s.setName('list').setDescription('Show the voice roles and any problems.'))
  .addSubcommand((s) => s.setName('sync').setDescription('Fix the roles of everyone now.'));

const MSG = {
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'),
  EVERYONE: () => errorEmbed('Invalid Role', '@everyone cannot be a voice role.'),
  BAD_CHANNEL: () => errorEmbed('Invalid Channel', 'Pick a voice channel, a stage channel or a category.'),
  LINKS_FULL: () => errorEmbed('List Full', `At most ${LIMITS.VOICE_ROLE_LINKS} channel roles can be set up. Remove one with \`/voicerole remove\` first.`),
  ANY_FULL: () => errorEmbed('List Full', `At most ${LIMITS.VOICE_ROLE_ANY} roles can be given in every voice channel. Remove one with \`/voicerole any remove\` first.`),
};

const OFF_NOTE = '\n\n⚠️ The **Voice Roles** module is off. Turn it on with `/module enable voiceroles`.';
const TAKES_ROLE_WARNING = 'The bot **takes this role away from anyone who is not in the linked channel**, so it must be a role made only for this.';
const WHEN_NOTE = 'It applies from each member\'s next voice join or move, or when you run `/voicerole sync`.';

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

/** An error embed when the role cannot be a voice role (by the bot or by the caller), otherwise null. */
function roleRefusal(interaction, role) {
  const { guild } = interaction;
  if (role.id === guild.id) return MSG.EVERYONE();
  const chk = checkAssignableRole(guild, role, { executor: interaction.member, allowDangerous: false });
  return chk.ok ? null : errorEmbed('Cannot Use That Role', chk.reason);
}

/** The module-off note for a reply, read after the change was saved. */
async function offNote(guildId) {
  const settings = await getSettingsSafe(guildId);
  return settings && !isModuleEnabled(settings, 'voiceroles') ? OFF_NOTE : '';
}

const roleOf = (interaction) => {
  const option = interaction.options.getRole('role', true);
  return interaction.guild.roles.cache.get(option.id) ?? option;
};
const channelOf = (interaction) => {
  const option = interaction.options.getChannel('channel', true);
  return interaction.guild.channels.cache.get(option.id) ?? option;
};
const whereText = (channel) => (channel.type === ChannelType.GuildCategory
  ? `any voice channel in the <#${channel.id}> category`
  : `<#${channel.id}>`);

// ── /voicerole add | remove ───────────────────────────────────────────────────

async function runAdd(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guildId } = interaction;
  const channel = channelOf(interaction);
  if (!LINKABLE.has(channel.type)) return finish(interaction, MSG.BAD_CHANNEL());
  const role = roleOf(interaction);
  const refusal = roleRefusal(interaction, role);
  if (refusal) return finish(interaction, refusal);

  let outcome;
  try {
    outcome = await addLink(guildId, channel.id, role.id);
  } catch (err) {
    logger.error(`[voice-roles] ${guildId}: add failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (outcome === 'full') return finish(interaction, MSG.LINKS_FULL());
  if (outcome === 'same') return finish(interaction, infoEmbed('No Change', `${whereText(channel)} already gives <@&${role.id}>.`));
  logger.info(`[voice-roles] ${guildId}: link ${channel.id} -> ${role.id} by ${interaction.user.id}`);
  return finish(interaction, successEmbed(
    'Voice Role Added',
    `Members in ${whereText(channel)} get <@&${role.id}> while they are there.\n\n⚠️ ${TAKES_ROLE_WARNING}\n${WHEN_NOTE}${await offNote(guildId)}`,
  ));
}

function removedText(role, stillManaged) {
  return stillManaged
    ? `<@&${role.id}> is still given somewhere else, so the bot keeps managing it.`
    : `The bot no longer manages <@&${role.id}>: members who have it keep it until you take it from them.`;
}

async function runRemove(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guildId } = interaction;
  const channel = channelOf(interaction);
  if (!LINKABLE.has(channel.type)) return finish(interaction, MSG.BAD_CHANNEL());
  const role = roleOf(interaction);
  let res;
  try {
    res = await removeLink(guildId, channel.id, role.id);
  } catch (err) {
    logger.error(`[voice-roles] ${guildId}: remove failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (res.outcome === 'same') return finish(interaction, infoEmbed('No Change', `${whereText(channel)} does not give <@&${role.id}>.`));
  logger.info(`[voice-roles] ${guildId}: unlink ${channel.id} -> ${role.id} by ${interaction.user.id}`);
  return finish(interaction, successEmbed(
    'Voice Role Removed',
    `${whereText(channel)} no longer gives <@&${role.id}>.\n${removedText(role, res.stillManaged)}${await offNote(guildId)}`,
  ));
}

// ── /voicerole any add | remove ───────────────────────────────────────────────

async function runAnyAdd(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guildId } = interaction;
  const role = roleOf(interaction);
  const refusal = roleRefusal(interaction, role);
  if (refusal) return finish(interaction, refusal);
  let outcome;
  try {
    outcome = await addAnyRole(guildId, role.id);
  } catch (err) {
    logger.error(`[voice-roles] ${guildId}: any add failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (outcome === 'full') return finish(interaction, MSG.ANY_FULL());
  if (outcome === 'same') return finish(interaction, infoEmbed('No Change', `<@&${role.id}> is already given in every voice channel.`));
  logger.info(`[voice-roles] ${guildId}: any-role ${role.id} added by ${interaction.user.id}`);
  return finish(interaction, successEmbed(
    'Voice Role Added',
    `Members in any voice channel get <@&${role.id}> while they are there.\n\n⚠️ The bot **takes this role away from anyone who is not in a voice channel**, so it must be a role made only for this.\n${WHEN_NOTE}${await offNote(guildId)}`,
  ));
}

async function runAnyRemove(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guildId } = interaction;
  const role = roleOf(interaction);
  let res;
  try {
    res = await removeAnyRole(guildId, role.id);
  } catch (err) {
    logger.error(`[voice-roles] ${guildId}: any remove failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (res.outcome === 'same') return finish(interaction, infoEmbed('No Change', `<@&${role.id}> is not given in every voice channel.`));
  logger.info(`[voice-roles] ${guildId}: any-role ${role.id} removed by ${interaction.user.id}`);
  return finish(interaction, successEmbed(
    'Voice Role Removed',
    `<@&${role.id}> is no longer given in every voice channel.\n${removedText(role, res.stillManaged)}${await offNote(guildId)}`,
  ));
}

// ── /voicerole list ───────────────────────────────────────────────────────────

/** A warning for a role the bot will not use (deleted, above the bot, managed, dangerous), or null when it is fine. */
function roleProblem(guild, roleId) {
  const role = guild.roles.cache.get(roleId);
  if (!role) return 'the role was deleted';
  const chk = checkAssignableRole(guild, role, { allowDangerous: false });
  return chk.ok ? null : chk.reason;
}

async function runList(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild } = interaction;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  const enabled = isModuleEnabled(settings, 'voiceroles');
  const links = linksOf(settings);
  const any = anyRolesOf(settings);

  const linkLines = links.map((l) => {
    const channel = guild.channels.cache.get(l.channelId);
    const problem = !channel ? 'the channel was deleted' : roleProblem(guild, l.roleId);
    const where = channel?.type === ChannelType.GuildCategory ? `📁 <#${l.channelId}>` : `<#${l.channelId}>`;
    return `${problem ? '⚠️' : '✅'} ${where} → <@&${l.roleId}>${problem ? ` — ${problem}` : ''}`;
  });
  const anyLines = any.map((id) => {
    const problem = roleProblem(guild, id);
    return `${problem ? '⚠️' : '✅'} <@&${id}>${problem ? ` — ${problem}` : ''}`;
  });

  const embed = guildEmbed(settings, '🎙️ Voice Roles').addFields({
    name: 'Status',
    value: enabled ? '✅ On' : '❌ Off — `/module enable voiceroles`',
    inline: true,
  }, {
    name: 'AFK channel',
    value: settings.voiceRoleIgnoreAfk === false ? 'Gets the roles too' : 'Gets no roles',
    inline: true,
  });
  const chunks = chunkLines(linkLines, 1024).slice(0, 3);
  if (chunks.length) {
    chunks.forEach((value, i) => embed.addFields({
      name: i === 0 ? `Channel roles (${links.length}/${LIMITS.VOICE_ROLE_LINKS})` : 'Channel roles (continued)',
      value,
    }));
  } else {
    embed.addFields({ name: `Channel roles (0/${LIMITS.VOICE_ROLE_LINKS})`, value: 'None — `/voicerole add`' });
  }
  embed.addFields({
    name: `Any voice channel (${any.length}/${LIMITS.VOICE_ROLE_ANY})`,
    value: anyLines.length ? joinWithLimit(anyLines, { sep: '\n', max: 1024 }) : 'None — `/voicerole any add`',
  });
  const missing = hasVoiceRoles(settings) ? missingBotPermissions(guild, ['ManageRoles']) : [];
  if (missing.length) embed.addFields({ name: 'Problem', value: `⚠️ I need **${permissionNames(missing).join(', ')}**.` });
  embed.setFooter({ text: 'The bot takes these roles from anyone not in the linked channel. Roles with powerful permissions, managed roles and roles above mine are never given.' });
  return finish(interaction, embed);
}

// ── /voicerole sync ───────────────────────────────────────────────────────────

async function runSync(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const settings = await loadSettingsOrFinish(interaction);
  if (!settings) return;
  if (!isModuleEnabled(settings, 'voiceroles')) {
    return finish(interaction, warnEmbed(
      'Module Is Off',
      'Voice roles are only kept up to date while the **Voice Roles** module is on. Turn it on with `/module enable voiceroles`, then run this again.',
    ));
  }
  if (!hasVoiceRoles(settings)) {
    return finish(interaction, infoEmbed('Nothing To Sync', 'No voice roles are set up yet. Use `/voicerole add` or `/voicerole any add`.'));
  }
  const missing = missingBotPermissions(guild, ['ManageRoles']);
  if (missing.length) {
    return finish(interaction, errorEmbed('Missing Permissions', `I need **${permissionNames(missing).join(', ')}** to change roles.`));
  }

  const res = await reconcileGuild(guild, settings);
  if (res.busy) return finish(interaction, infoEmbed('Already Running', 'A sync is already running on this server. Try again in a moment.'));
  logger.info(`[voice-roles] ${guildId}: sync by ${interaction.user.id}: checked ${res.checked}, +${res.added} -${res.removed}, ${res.failed} failed`);
  const lines = [
    `Checked **${formatNumber(res.checked)}** member${res.checked === 1 ? '' : 's'}: gave **${formatNumber(res.added)}** role${res.added === 1 ? '' : 's'} and took **${formatNumber(res.removed)}** away.`,
  ];
  if (res.failed) lines.push(`${formatNumber(res.failed)} member${res.failed === 1 ? '' : 's'} could not be updated. Check my role position and permissions with \`/voicerole list\`.`);
  if (res.truncated) lines.push(`Only the first ${formatNumber(VOICE_ROLES.RECONCILE_MAX)} members were checked. Run \`/voicerole sync\` again to continue.`);
  lines.push('Only members the bot has cached are checked, so a holder who is not cached is fixed on their next voice change.');
  return finish(interaction, (res.failed ? warnEmbed : successEmbed)('Voice Roles Synced', lines.join('\n')));
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  if (group === 'any') return sub === 'add' ? runAnyAdd(interaction) : runAnyRemove(interaction);
  if (sub === 'add') return runAdd(interaction);
  if (sub === 'remove') return runRemove(interaction);
  if (sub === 'list') return runList(interaction);
  if (sub === 'sync') return runSync(interaction);
}
