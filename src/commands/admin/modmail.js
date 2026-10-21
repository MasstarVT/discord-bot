// /modmail setup | reply | close | block | unblock | blocked. Staff command (Moderator; `setup` needs Admin) that keeps working
// while the Modmail module is off (gated = false), so an admin can set it up first. `reply` talks to a member, so it refuses
// while the module is off. The conversations themselves are src/plugins/modmail.js.
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import {
  ensureGuildSettings, getSettingsSafe, updateSettingsWith,
} from '../../services/settingsService.js';
import {
  MODMAIL, findByThread, findOpenInGuild, staffAuthor,
} from '../../services/modmailService.js';
import {
  archiveThread, channelProblems, closeAndNotify, closeConversation, relayToMember,
} from '../../plugins/modmail.js';
import { errorEmbed, guildEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { pingableRoleIds, safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import { clampText, joinWithLimit } from '../../utils/text.js';

export const module = 'modmail';
export const gated = false;                                   // staff can set modmail up before turning it on
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;
export const subcommandPermissions = { setup: PERMISSION_LEVELS.ADMIN };

export const data = new SlashCommandBuilder()
  .setName('modmail')
  .setDescription('Members DM the bot to reach staff privately; each conversation is a thread.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addSubcommand((s) => s.setName('setup').setDescription('Pick the staff channel for conversations and turn modmail on.')
    .addChannelOption((o) => o.setName('channel').setDescription('A staff-only text channel; conversations open as threads in it.')
      .addChannelTypes(ChannelType.GuildText).setRequired(true))
    .addRoleOption((o) => o.setName('ping_role').setDescription('Role pinged when a conversation opens (replaces the current ones).')))
  .addSubcommand((s) => s.setName('reply').setDescription('Reply to the member (use inside a modmail thread).')
    .addStringOption((o) => o.setName('text').setDescription('Your reply.').setRequired(true).setMaxLength(MODMAIL.TEXT_MAX))
    .addBooleanOption((o) => o.setName('anonymous').setDescription('Show you as "Staff" instead of your name.')))
  .addSubcommand((s) => s.setName('close').setDescription('Close this conversation (use inside a modmail thread).')
    .addStringOption((o) => o.setName('reason').setDescription('Shown to the member.').setMaxLength(MODMAIL.REASON_MAX)))
  .addSubcommand((s) => s.setName('block').setDescription('Stop a member from opening conversations.')
    .addUserOption((o) => o.setName('user').setDescription('The member to block.').setRequired(true))
    .addStringOption((o) => o.setName('reason').setDescription('Why (for the log).').setMaxLength(200)))
  .addSubcommand((s) => s.setName('unblock').setDescription('Let a blocked member open conversations again.')
    .addUserOption((o) => o.setName('user').setDescription('The member to unblock.').setRequired(true)))
  .addSubcommand((s) => s.setName('blocked').setDescription('List the blocked members.'));

const MSG = {
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'),
  NOT_A_THREAD: () => errorEmbed('Not a Modmail Thread', 'Use this command inside a modmail conversation thread.'),
  CLOSED: () => errorEmbed('Conversation Closed', 'This conversation is already closed.'),
  OFF: () => warnEmbed('Module Disabled', 'The **Modmail** module is turned off on this server. An admin can enable it with `/module enable modmail`.'),
};

/** Final answer: overwrites the deferred reply. Never pings. */
function finish(interaction, embed) {
  return safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
}

/** The OPEN conversation of the thread this command runs in, or the embed to answer with. */
async function conversationHere(interaction) {
  if (!interaction.channel?.isThread?.()) return { embed: MSG.NOT_A_THREAD() };
  const row = await findByThread(interaction.channelId);
  if (!row || row.guildId !== interaction.guildId) return { embed: MSG.NOT_A_THREAD() };
  if (row.status !== 'OPEN') return { embed: MSG.CLOSED() };
  return { row };
}

// ── /modmail setup ────────────────────────────────────────────────────────────

async function runSetup(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const option = interaction.options.getChannel('channel', true);
  const channel = guild.channels.cache.get(option.id) ?? null;
  if (!channel || channel.type !== ChannelType.GuildText) {
    return finish(interaction, errorEmbed('Cannot Use That Channel', 'Pick a text channel: conversations open as threads in it.'));
  }
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());

  const role = interaction.options.getRole('ping_role');
  if (role) {
    if (role.id === guildId) return finish(interaction, errorEmbed('Cannot Ping That Role', '@everyone cannot be the ping role.'));
    if (pingableRoleIds(channel, interaction.member, [role.id]).refused.length) {
      return finish(interaction, errorEmbed('Cannot Ping That Role',
        `<@&${role.id}> is not mentionable and you cannot mention every role. Make it mentionable, or pick another role.`));
    }
  }
  const pingRoleIds = role ? [role.id] : (settings.modmailPingRoleIds ?? []).slice(0, LIMITS.MODMAIL_PING_ROLES);

  const problems = channelProblems(channel, { ...settings, modmailPingRoleIds: pingRoleIds });
  if (problems.errors.length) {
    return finish(interaction, errorEmbed('Cannot Use That Channel', `${problems.errors.join('\n')}\nPick another channel, or fix my permissions there.`));
  }
  const warnings = [...problems.warnings];
  const botCheck = pingableRoleIds(channel, guild.members?.me ?? null, pingRoleIds);
  if (botCheck.refused.length) {
    warnings.push(`I cannot ping ${botCheck.refused.map((id) => `<@&${id}>`).join(', ')}: make the role mentionable or give me **Mention Everyone**.`);
  }

  try {
    await ensureGuildSettings(guildId);
    await updateSettingsWith(guildId, () => ({
      modmailChannelId: channel.id, modmailEnabled: true, ...(role ? { modmailPingRoleIds: [role.id] } : {}),
    }));
  } catch (err) {
    logger.error(`[modmail] ${guildId}: setup failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  logger.info(`[modmail] ${guildId}: staff channel ${settings.modmailChannelId ?? null} → ${channel.id} by ${interaction.user.id}`);

  const lines = [
    `Modmail is on. Members can DM me; each conversation opens as a thread in <#${channel.id}>.`,
    pingRoleIds.length ? `Pinged on a new conversation: ${pingRoleIds.map((id) => `<@&${id}>`).join(' ')}` : 'No role is pinged yet.',
    'Keep the channel staff-only; members never need access to it.',
    ...warnings.map((w) => `⚠️ ${w}`),
  ];
  return finish(interaction, successEmbed('Modmail Set Up', lines.join('\n')));
}

// ── /modmail reply ────────────────────────────────────────────────────────────

async function runReply(interaction, client) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  if (!isModuleEnabled(settings, 'modmail')) return finish(interaction, MSG.OFF());
  const here = await conversationHere(interaction);
  if (!here.row) return finish(interaction, here.embed);

  const text = interaction.options.getString('text', true);
  // The command can only turn anonymity on: the admin's setting is never bypassed.
  const anonymous = interaction.options.getBoolean('anonymous') === true || settings.modmailAnonymousReplies === true;
  const author = staffAuthor(interaction.member, guild, anonymous);
  const res = await relayToMember({ client, row: here.row, guild, settings, author, message: { content: text } });
  if (res.ok) return finish(interaction, successEmbed('Reply Sent', text, anonymous ? 'Sent to the member as "Staff".' : `Sent to the member as ${author.name}.`));
  if (res.code === 'DM_CLOSED') return finish(interaction, errorEmbed('Not Delivered', "The member's DMs are closed, so they did not get that reply. They can still write to me."));
  return finish(interaction, errorEmbed('Not Delivered', "I couldn't deliver that reply. Please try again in a moment."));
}

// ── /modmail close ────────────────────────────────────────────────────────────

async function runClose(interaction, client) {
  if (!(await safeDefer(interaction))) return;
  const { guild } = interaction;
  const here = await conversationHere(interaction);
  if (!here.row) return finish(interaction, here.embed);

  const reason = interaction.options.getString('reason');
  const res = await closeAndNotify({ client, guild, row: here.row, closedById: interaction.user.id, reason });
  if (!res.closed) return finish(interaction, MSG.CLOSED());
  const lines = [`Closed by <@${interaction.user.id}>.`];
  if (reason) lines.push(`**Reason:** ${clampText(reason, MODMAIL.REASON_MAX)}`);
  if (!res.notified) lines.push("The member could not be told (their DMs are closed).");
  await finish(interaction, successEmbed('Conversation Closed', lines.join('\n')));
  // Locking and archiving come last: a message cannot be posted in an archived thread.
  await archiveThread(interaction.channel);
}

// ── /modmail block | unblock | blocked ────────────────────────────────────────

async function runBlock(interaction, client) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const target = interaction.options.getUser('user', true);
  const reason = interaction.options.getString('reason');
  if (target.bot) return finish(interaction, errorEmbed('Cannot Block', 'Bots cannot open conversations anyway.'));
  if (target.id === interaction.user.id) return finish(interaction, errorEmbed('Cannot Block', 'You cannot block yourself.'));
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  const member = guild.members.cache.get(target.id);
  if (member && resolvePermissionLevel(member, settings) >= PERMISSION_LEVELS.MODERATOR) {
    return finish(interaction, errorEmbed('Cannot Block', 'Staff cannot be blocked.'));
  }

  let outcome = 'added';
  try {
    await updateSettingsWith(guildId, (current) => {
      const list = current.modmailBlockedUserIds ?? [];
      if (list.includes(target.id)) { outcome = 'already'; return null; }
      if (list.length >= LIMITS.MODMAIL_BLOCKED_USERS) { outcome = 'full'; return null; }
      return { modmailBlockedUserIds: [...list, target.id] };
    });
  } catch (err) {
    logger.error(`[modmail] ${guildId}: block failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (outcome === 'already') return finish(interaction, errorEmbed('Already Blocked', `<@${target.id}> is already blocked.`));
  if (outcome === 'full') return finish(interaction, errorEmbed('Block List Full', `At most ${LIMITS.MODMAIL_BLOCKED_USERS} members can be blocked. Unblock someone first.`));
  logger.info(`[modmail] ${guildId}: ${target.id} blocked by ${interaction.user.id}${reason ? ` (${clampText(reason, 200)})` : ''}`);

  // An open conversation ends with the block; the member is only told that it closed.
  let closed = false;
  try {
    const row = await findOpenInGuild(guildId, target.id);
    if (row) {
      closed = (await closeConversation({
        client, guild, row, closedById: interaction.user.id, reason: reason ? `Blocked: ${reason}` : 'Blocked',
        memberReason: null, text: 'Closed because the member was blocked.',
      })).closed;
    }
  } catch (err) {
    logger.warn(`[modmail] ${guildId}: could not close the conversation of a blocked member: ${err?.message}`);
  }
  return finish(interaction, successEmbed('Member Blocked',
    `<@${target.id}> can no longer open modmail conversations.${closed ? ' Their open conversation was closed.' : ''}`));
}

async function runUnblock(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guildId } = interaction;
  const target = interaction.options.getUser('user', true);
  let removed = false;
  try {
    await updateSettingsWith(guildId, (current) => {
      const list = current.modmailBlockedUserIds ?? [];
      if (!list.includes(target.id)) return null;
      removed = true;
      return { modmailBlockedUserIds: list.filter((id) => id !== target.id) };
    });
  } catch (err) {
    logger.error(`[modmail] ${guildId}: unblock failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!removed) return finish(interaction, errorEmbed('Not Blocked', `<@${target.id}> is not blocked.`));
  logger.info(`[modmail] ${guildId}: ${target.id} unblocked by ${interaction.user.id}`);
  return finish(interaction, successEmbed('Member Unblocked', `<@${target.id}> can open modmail conversations again.`));
}

async function runBlocked(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const settings = await getSettingsSafe(interaction.guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  const ids = settings.modmailBlockedUserIds ?? [];
  const hidden = Math.max(0, ids.length - MODMAIL.BLOCKED_LIST_MAX);
  const lines = ids.slice(0, MODMAIL.BLOCKED_LIST_MAX).map((id) => `<@${id}> \`${id}\``);
  if (hidden) lines.push(`… and ${hidden} more`);
  const embed = guildEmbed(settings, '🚫 Blocked Members', lines.length ? joinWithLimit(lines, { sep: '\n', max: 3800 }) : 'Nobody is blocked.',
    `${ids.length} of ${LIMITS.MODMAIL_BLOCKED_USERS}`);
  return finish(interaction, embed);
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'setup') return runSetup(interaction);
  if (sub === 'reply') return runReply(interaction, client);
  if (sub === 'close') return runClose(interaction, client);
  if (sub === 'block') return runBlock(interaction, client);
  if (sub === 'unblock') return runUnblock(interaction);
  if (sub === 'blocked') return runBlocked(interaction);
}
