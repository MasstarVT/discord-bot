// /voice (pkg-temp-voice): the MEE6 /voice-ban, -claim, -clean, -hide, -kick, -limit, -lock, -rename, -reveal, -transfer, -unban and
// -unlock commands as subcommands. Thin: it defers (ephemeral, control actions are personal), finds the temporary channel the member is
// connected to and calls the service, where owner / moderator rules, bot permission checks and the rate limits live.
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import * as tv from '../../services/tempVoiceService.js';
import logger from '../../utils/logger.js';

export const module = 'tempvoice';
export const gated = true; // the router refuses it while the module is off
export const permissionLevel = PERMISSION_LEVELS.MEMBER; // owner / moderator rules are enforced in the service
// No setDefaultMemberPermissions (every member may own a channel) and no allowInDMs: buildCommandPayload registers it for servers only.

const memberOpt = (desc) => (o) => o.setName('member').setDescription(desc).setRequired(true);

export const data = new SlashCommandBuilder()
  .setName('voice')
  .setDescription('Control your temporary voice channel (MEE6 /voice-* commands).')
  .addSubcommand((s) => s.setName('ban').setDescription('Ban a member from your temporary voice channel (MEE6 /voice-ban).')
    .addUserOption(memberOpt('Member to ban')))
  .addSubcommand((s) => s.setName('claim').setDescription('Claim this temporary voice channel after its owner left (MEE6 /voice-claim).'))
  .addSubcommand((s) => s.setName('clean').setDescription('Moderators: delete every empty temporary voice channel (MEE6 /voice-clean).'))
  .addSubcommand((s) => s.setName('hide').setDescription('Hide your temporary voice channel (MEE6 /voice-hide).'))
  .addSubcommand((s) => s.setName('kick').setDescription('Disconnect a member from your temporary voice channel (MEE6 /voice-kick).')
    .addUserOption(memberOpt('Member to disconnect')))
  .addSubcommand((s) => s.setName('limit').setDescription('Change the member limit of your temporary voice channel (MEE6 /voice-limit).')
    .addIntegerOption((o) => o.setName('member_limit').setDescription('Maximum members (0 = no limit)').setRequired(true).setMinValue(0).setMaxValue(99)))
  .addSubcommand((s) => s.setName('lock').setDescription('Lock your temporary voice channel (MEE6 /voice-lock).'))
  .addSubcommand((s) => s.setName('rename').setDescription('Rename your temporary voice channel (MEE6 /voice-rename).')
    .addStringOption((o) => o.setName('name').setDescription('New channel name').setRequired(true).setMinLength(1).setMaxLength(100)))
  .addSubcommand((s) => s.setName('reveal').setDescription('Unhide your temporary voice channel (MEE6 /voice-reveal).'))
  .addSubcommand((s) => s.setName('transfer').setDescription('Give ownership to a member in the channel (MEE6 /voice-transfer).')
    .addUserOption(memberOpt('New owner (must be in the channel)')))
  .addSubcommand((s) => s.setName('unban').setDescription('Unban a member from your temporary voice channel (MEE6 /voice-unban).')
    .addUserOption(memberOpt('Member to unban')))
  .addSubcommand((s) => s.setName('unlock').setDescription('Unlock your temporary voice channel (MEE6 /voice-unlock).'));

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  try {
    if (!(await safeDefer(interaction, { ephemeral: true }))) return;
    const reply = (embed) => safeReply(interaction, { embeds: [embed] });
    // Memo first: the router already loaded the settings, so this normally costs no I/O.
    const settings = peekSettings(interaction.guildId) ?? await getSettingsSafe(interaction.guildId);

    if (sub === 'clean') {
      await reply(await tv.cleanEmptyChannels({ guild: interaction.guild, actor: interaction.member, settings }));
      return;
    }

    // /voice acts on the temporary channel the member is connected to.
    const channelId = interaction.member.voice?.channelId ?? null;
    if (!channelId) {
      await reply(tv.MSG.NOT_IN_TEMP());
      return;
    }
    const targetId = interaction.options.getUser('member')?.id ?? null;
    let value = null;
    if (sub === 'limit') value = interaction.options.getInteger('member_limit');
    else if (sub === 'rename') value = interaction.options.getString('name');

    const { embed } = await tv.runVoiceAction({
      guild: interaction.guild, actor: interaction.member, channelId, action: sub, targetId, value, settings, source: 'command',
    });
    await reply(embed);
  } catch (err) {
    logger.error(`voice ${sub} failed`, err);
    await safeReply(interaction, { embeds: [tv.MSG.INTERNAL()] });
  }
}
