// /boost: boosters make a role of their own (`role create|edit|delete`); admins run `config`, `test`, `list` and `remove`
// (subcommandPermissions). The command is not gated, so admins can set it up before turning the module on; every member
// subcommand checks the module itself and refuses with the router's wording while it is off. The rules (name checks,
// placement, the role calls, the sweep) live in services/boostService.js; the hooks are src/plugins/boosts.js.
import { ChannelType, SlashCommandBuilder } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettingsSafe, isModuleEnabled, updateSettings } from '../../services/settingsService.js';
import {
  BOOST, createBoosterRole, deleteOwnRole, editBoosterRole, ensureSweepJob, graceMs, listRows, memberProblem,
  nitroBoosterRole, placementFor, postThankYou, problemEmbed, removeMemberRole, renderThankYou,
} from '../../services/boostService.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { checkChannelUsable, missingBotPermissions, permissionNames, safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import { joinWithLimit } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = 'boosts';
export const gated = false;                                  // `config` and the admin tools work while the module is off
export const permissionLevel = PERMISSION_LEVELS.MEMBER;     // role create | edit | delete
export const subcommandPermissions = Object.freeze({
  config: PERMISSION_LEVELS.ADMIN,
  test: PERMISSION_LEVELS.ADMIN,
  list: PERMISSION_LEVELS.ADMIN,
  remove: PERMISSION_LEVELS.ADMIN,
});
// No setDefaultMemberPermissions and no allowInDMs: boosters must see `role`; guild only.

const looks = (s) => s
  .addStringOption((o) => o.setName('color').setDescription('Hex colour, like #ff8800').setMaxLength(7))
  .addStringOption((o) => o.setName('icon_emoji').setDescription('One standard emoji as the role icon').setMaxLength(BOOST.EMOJI_MAX))
  .addAttachmentOption((o) => o.setName('icon_image').setDescription('PNG or JPEG role icon'));

export const data = new SlashCommandBuilder()
  .setName('boost')
  .setDescription('Boosters can make a role of their own. Admins set it up.')
  .addSubcommandGroup((g) => g.setName('role').setDescription('Your own booster role.')
    .addSubcommand((s) => looks(s.setName('create').setDescription('Make your booster role.')
      .addStringOption((o) => o.setName('name').setDescription('Role name').setRequired(true).setMaxLength(LIMITS.BOOSTER_ROLE_NAME))))
    .addSubcommand((s) => looks(s.setName('edit').setDescription('Change your booster role.')
      .addStringOption((o) => o.setName('name').setDescription('New role name').setMaxLength(LIMITS.BOOSTER_ROLE_NAME))))
    .addSubcommand((s) => s.setName('delete').setDescription('Delete your booster role.')))
  .addSubcommand((s) => s.setName('config').setDescription('Show or change the boost settings.')
    .addChannelOption((o) => o.setName('channel').setDescription('Where the thank-you is posted')
      .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
    .addBooleanOption((o) => o.setName('custom_roles').setDescription('Let boosters make their own role'))
    .addRoleOption((o) => o.setName('anchor').setDescription('Booster roles go just below this role (unset: just above the Nitro Booster role)'))
    .addIntegerOption((o) => o.setName('grace_days').setDescription('Days a role stays after the boost ends (0 = removed at once)')
      .setMinValue(0).setMaxValue(BOOST.GRACE_MAX_DAYS)))
  .addSubcommand((s) => s.setName('test').setDescription('Post a preview of the thank-you.'))
  .addSubcommand((s) => s.setName('list').setDescription('List the booster roles.'))
  .addSubcommand((s) => s.setName('remove').setDescription('Delete a member\'s booster role.')
    .addUserOption((o) => o.setName('user').setDescription('The member').setRequired(true)));

const NO_MENTIONS = Object.freeze({ parse: [] });
const yesNo = (v) => (v ? '✅ On' : '❌ Off');

const finish = (interaction, embed) => safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });

// ── role create | edit | delete (members) ─────────────────────────────────────

async function roleCommand(interaction, sub) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return undefined;
  const { guild, guildId, member, options } = interaction;
  const settings = await getSettingsSafe(guildId);
  // Deleting is allowed after the boost ended (the role is still there during the grace period); the rest needs a boost.
  const blocked = memberProblem(settings, member, { needBoosting: sub !== 'delete' });
  if (blocked) return finish(interaction, problemEmbed(blocked));

  if (sub === 'delete') {
    const res = await deleteOwnRole({ guild, member });
    return finish(interaction, res.ok ? successEmbed('Booster Role Deleted', 'Your booster role was deleted.') : problemEmbed(res));
  }

  const input = {
    guild, member, name: options.getString('name'), color: options.getString('color'),
    iconEmoji: options.getString('icon_emoji'), iconImage: options.getAttachment('icon_image'),
  };
  if (sub === 'create') {
    const res = await createBoosterRole({ ...input, settings });
    if (!res.ok) return finish(interaction, problemEmbed(res));
    const lines = [`Your role <@&${res.role.id}> is ready and you have it now. Change it any time with \`/boost role edit\`.`];
    if (!res.moved) lines.push('⚠️ I could not move it to its spot in the role list, so another role may hide its colour. Ask an admin to check my role position.');
    return finish(interaction, successEmbed('Booster Role Created', lines.join('\n')));
  }
  const res = await editBoosterRole(input);
  if (!res.ok) return finish(interaction, problemEmbed(res));
  return finish(interaction, successEmbed('Booster Role Updated', `Changed the ${res.changed.join(' and ')} of <@&${res.role.id}>.`));
}

// ── config (admin) ────────────────────────────────────────────────────────────

function configEmbed(guild, settings, title, rowCount) {
  const channelId = settings.boostChannelId;
  const anchorId = settings.boostRoleAnchorId;
  const grace = Math.round(graceMs(settings) / BOOST.DAY_MS);
  const nitro = nitroBoosterRole(guild);
  const placed = anchorId ? `just below <@&${anchorId}>`
    : nitro ? `just above <@&${nitro.id}> (the Nitro Booster role)` : 'at the bottom of the role list';
  const embed = guildEmbed(settings, title, [
    `Module: ${isModuleEnabled(settings, 'boosts') ? '✅ On' : '❌ Off, turn it on with `/module enable boosts`'}`,
    `Thank-you channel: ${channelId ? `<#${channelId}>` : 'Not set (no thank-yous)'}`,
    `Booster roles: ${yesNo(settings.boostCustomRoles)}${rowCount === null ? '' : ` (${rowCount} made)`}`,
    `Booster roles go: ${placed}`,
    `Kept after the boost ends: ${grace > 0 ? `${grace} day${grace === 1 ? '' : 's'}` : 'removed at once'}`,
  ].join('\n'));
  const warnings = [];
  if (channelId) {
    const usable = checkChannelUsable(guild.channels.cache.get(channelId), { needs: ['ViewChannel', 'SendMessages'] });
    if (!usable.ok) warnings.push(`⚠️ ${usable.reason ?? 'I cannot post in the thank-you channel.'}`);
  }
  if (settings.boostCustomRoles) {
    const spot = placementFor(guild, settings);
    if (!spot.ok) {
      warnings.push({
        BOT_PERMS: `⚠️ I need **${permissionNames(missingBotPermissions(guild, ['ManageRoles'])).join(', ') || 'Manage Roles'}** to make booster roles.`,
        BOT_LOW: '⚠️ My highest role is too low to put a role below it. Move my role up.',
        ANCHOR_HIGH: '⚠️ The role booster roles go below is above my highest role. Pick a lower one.',
      }[spot.code]);
    }
  }
  if (warnings.length) embed.addFields({ name: 'Problems', value: warnings.join('\n').slice(0, 1024) });
  embed.setFooter({ text: guild.features?.includes('ROLE_ICONS') ? 'Role icons are unlocked.' : 'Role icons need Boost level 2. The message text is set on the dashboard.' });
  return embed;
}

async function config(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const { guild, guildId, options } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, problemEmbed({ code: 'SETTINGS' }));

  const update = {};
  const picked = options.getChannel('channel');
  const custom = options.getBoolean('custom_roles');
  const anchor = options.getRole('anchor');
  const days = options.getInteger('grace_days');
  if (picked) {
    const usable = checkChannelUsable(guild.channels.cache.get(picked.id) ?? null, { needs: ['ViewChannel', 'SendMessages'] });
    if (!usable.ok) return finish(interaction, errorEmbed('Cannot Use That Channel', `${usable.reason} Pick another channel, or fix my permissions there.`));
    update.boostChannelId = picked.id;
  }
  if (custom !== null) update.boostCustomRoles = custom;
  if (anchor) {
    if (anchor.id === guild.id) return finish(interaction, errorEmbed('Invalid Role', '@everyone cannot be used here. Pick another role.'));
    const top = guild.members?.me?.roles?.highest;
    if (top && anchor.position > top.position) {
      return finish(interaction, errorEmbed('Role Too High', 'That role is above my highest role, so I could not put roles below it. Pick a lower role, or move my role up.'));
    }
    update.boostRoleAnchorId = anchor.id;
  }
  if (days !== null) update.boostRoleGraceDays = Math.min(BOOST.GRACE_MAX_DAYS, Math.max(0, days));

  let rows = [];
  try {
    rows = await listRows(guildId);
  } catch (err) {
    logger.warn(`[boosts] ${guildId}: could not count the booster roles: ${err?.message}`);
    rows = null;
  }
  if (rows?.length) await ensureSweepJob(guildId);
  const count = rows ? rows.length : null;
  if (!Object.keys(update).length) return finish(interaction, configEmbed(guild, settings, '💎 Server Boosts', count));

  let saved;
  try {
    saved = await updateSettings(guildId, update);
  } catch (err) {
    logger.error(`[boosts] ${guildId}: config failed`, err);
    return finish(interaction, errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'));
  }
  logger.info(`[boosts] ${guildId}: config changed ${Object.keys(update).join(', ')} by ${interaction.user.id}`);
  return finish(interaction, configEmbed(guild, saved, '✅ Server Boosts Updated', count));
}

// ── test | list | remove (admin) ──────────────────────────────────────────────

async function preview(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const { guild, guildId, member } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, problemEmbed({ code: 'SETTINGS' }));
  const off = isModuleEnabled(settings, 'boosts') ? '' : '\n\n⚠️ The **Server Boosts** module is off, so real thank-yous are not posted. Turn it on with `/module enable boosts`.';
  const res = await postThankYou(guild, member, settings, { preview: true });
  if (res.ok) return finish(interaction, successEmbed('Preview Posted', `Posted in <#${res.channelId}>.${off}`));
  if (res.reason === 'no-channel') {
    return finish(interaction, infoEmbed('No Thank-you Channel',
      `Set one with \`/boost config channel:\`. This is what members will see:\n\n${renderThankYou(settings, member, guild, { maxLength: 1500, preview: true })}${off}`));
  }
  return finish(interaction, errorEmbed('Could Not Post', `${res.text ?? 'The preview could not be posted.'}${off}`));
}

async function list(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, problemEmbed({ code: 'SETTINGS' }));
  let rows;
  try {
    rows = await listRows(guildId);
  } catch (err) {
    logger.warn(`[boosts] ${guildId}: could not list the booster roles: ${err?.message}`);
    return finish(interaction, errorEmbed('Lookup Failed', 'The booster roles could not be read. Please try again in a moment.'));
  }
  if (!rows.length) return finish(interaction, infoEmbed('No Booster Roles', 'Nobody has made a booster role yet.'));
  await ensureSweepJob(guildId);
  const lines = rows.map((r) => {
    const role = guild.roles.cache.has(r.roleId) ? `<@&${r.roleId}>` : `deleted role \`${r.roleId}\``;
    const state = r.boostEndedAt ? `boost ended <t:${Math.floor(new Date(r.boostEndedAt).getTime() / 1000)}:R>` : 'boosting';
    return `${role} · <@${r.userId}> · ${state}`;
  });
  return finish(interaction, guildEmbed(settings, `💎 Booster Roles (${rows.length})`, joinWithLimit(lines, { sep: '\n', max: 4000 })));
}

async function remove(interaction) {
  if (!(await safeDefer(interaction))) return undefined;
  const user = interaction.options.getUser('user', true);
  const res = await removeMemberRole({ guild: interaction.guild, userId: user.id, actorId: interaction.user.id });
  if (res.ok) {
    logger.info(`[boosts] ${interaction.guildId}: booster role of ${user.id} removed by ${interaction.user.id}`);
    return finish(interaction, successEmbed('Booster Role Removed', `Deleted the booster role of <@${user.id}>.`));
  }
  if (res.code === 'NO_ROLE') return finish(interaction, infoEmbed('Nothing To Remove', `<@${user.id}> has no booster role.`));
  if (res.code === 'BOT_ROLE') {
    return finish(interaction, errorEmbed('Could Not Delete', 'I could not delete that role: I need **Manage Roles** and my highest role must be above it.'));
  }
  return finish(interaction, problemEmbed(res));
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  if (group === 'role') return roleCommand(interaction, sub);
  if (sub === 'config') return config(interaction);
  if (sub === 'test') return preview(interaction);
  if (sub === 'list') return list(interaction);
  if (sub === 'remove') return remove(interaction);
  return undefined;
}
