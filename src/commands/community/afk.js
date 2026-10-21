// /afk: members mark themselves away (set, clear); moderators list the AFK members and clear one (list, remove).
// Gated by the AFK module: the router refuses every subcommand while it is off. Replies are ephemeral and never ping;
// the public side (announcing AFK members, welcoming them back) is the messageCreate hook in src/plugins/afk.js.
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import { safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import { successEmbed, infoEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { discordTimestamp } from '../../utils/time.js';
import { clampText } from '../../utils/text.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import {
  AFK, cleanReason, plainText, afkLength, setAfk, clearAfk, listStatuses, addNicknamePrefix, removeNicknamePrefix,
} from '../../services/afkService.js';
import logger from '../../utils/logger.js';

export const module = 'afk';
export const gated = true; // the router refuses it while the module is off
export const permissionLevel = PERMISSION_LEVELS.MEMBER; // set, clear
export const subcommandPermissions = Object.freeze({
  remove: PERMISSION_LEVELS.MODERATOR,
  list: PERMISSION_LEVELS.MODERATOR,
});
// No setDefaultMemberPermissions (every member may go AFK) and no allowInDMs: buildCommandPayload registers it for servers only.

export const data = new SlashCommandBuilder()
  .setName('afk')
  .setDescription('Go AFK: others who mention you are told, and it ends when you write.')
  .addSubcommand((s) => s.setName('set').setDescription('Mark yourself AFK.')
    .addStringOption((o) => o.setName('reason').setDescription('Why you are away').setMaxLength(LIMITS.AFK_REASON_LENGTH)))
  .addSubcommand((s) => s.setName('clear').setDescription('End your AFK.'))
  .addSubcommand((s) => s.setName('remove').setDescription('Moderators: end a member\'s AFK.')
    .addUserOption((o) => o.setName('user').setDescription('The member').setRequired(true)))
  .addSubcommand((s) => s.setName('list').setDescription('Moderators: list the AFK members.'));

const NO_PINGS = Object.freeze({ parse: [] });

const nickProblem = (problem) => (problem === 'cannot'
  ? "I couldn't add [AFK] to your nickname: I need Manage Nicknames and a role above yours."
  : problem === 'failed' ? "I couldn't change your nickname just now." : null);

async function memberFor(interaction, userId) {
  if (interaction.member?.id === userId) return interaction.member;
  const { guild } = interaction;
  return guild.members.cache.get(userId) ?? await guild.members.fetch(userId).catch(() => null);
}

async function runSet(interaction, settings) {
  const reason = cleanReason(interaction.options.getString('reason'));
  const nick = await addNicknamePrefix(interaction.member, settings);
  try {
    await setAfk({ guildId: interaction.guildId, userId: interaction.user.id, reason, nickChanged: nick.changed, previousNick: nick.previousNick });
  } catch (err) {
    // No row means nothing would ever take the prefix off again: undo it before the error reply.
    if (nick.changed) await removeNicknamePrefix(interaction.member, nick.previousNick);
    throw err;
  }
  const lines = [
    reason ? `You are now AFK: ${plainText(reason)}` : 'You are now AFK.',
    'Anyone who mentions you is told. Your next message ends it.',
  ];
  const problem = nickProblem(nick.problem);
  if (problem) lines.push(`-# ${problem}`);
  return successEmbed(`${AFK.EMOJI} AFK`, lines.join('\n'));
}

async function runClear(interaction) {
  const row = await clearAfk(interaction.guildId, interaction.user.id);
  if (!row) return infoEmbed('Not AFK', 'You are not AFK.');
  if (row.nickChanged) await removeNicknamePrefix(interaction.member, row.previousNick);
  return successEmbed('Welcome back', `You were AFK for **${afkLength(Date.now() - new Date(row.since).getTime())}**.`);
}

async function runRemove(interaction) {
  const target = interaction.options.getUser('user', true);
  const row = await clearAfk(interaction.guildId, target.id);
  if (!row) return infoEmbed('Not AFK', `${plainText(target.globalName ?? target.username, 40)} is not AFK.`);
  if (row.nickChanged) await removeNicknamePrefix(await memberFor(interaction, target.id), row.previousNick);
  return successEmbed('AFK removed', `<@${target.id}> is no longer AFK.`);
}

async function runList(interaction) {
  const { rows, total } = await listStatuses(interaction.guildId);
  if (!rows.length) return infoEmbed(`${AFK.EMOJI} AFK members`, 'Nobody is AFK.');
  const lines = rows.map((r) => `<@${r.userId}>${r.reason ? `: ${plainText(r.reason, 120)}` : ''} (since ${discordTimestamp(r.since, 'R')})`);
  const more = total > rows.length ? `\n-# … and ${total - rows.length} more` : '';
  return infoEmbed(`${AFK.EMOJI} AFK members (${total})`, clampText(lines.join('\n'), 3800) + more);
}

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  try {
    if (!(await safeDefer(interaction, { ephemeral: true }))) return;
    // Memo first: the router already loaded the settings, so this normally costs no I/O.
    const settings = peekSettings(interaction.guildId) ?? await getSettingsSafe(interaction.guildId);
    let embed;
    if (sub === 'set') embed = await runSet(interaction, settings);
    else if (sub === 'clear') embed = await runClear(interaction);
    else if (sub === 'remove') embed = await runRemove(interaction);
    else embed = await runList(interaction);
    await safeReply(interaction, { embeds: [embed], allowedMentions: NO_PINGS });
  } catch (err) {
    logger.error(`afk ${sub} failed`, err);
    await safeReply(interaction, { embeds: [warnEmbed('Something went wrong', 'Please try again in a moment.')], allowedMentions: NO_PINGS });
  }
}
