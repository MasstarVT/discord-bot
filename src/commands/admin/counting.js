// /counting set | reset | status. Admin config command: replies are public (CLAUDE.md admin convention) and it keeps
// working while the Counting module is off (gated = false). The game itself is src/plugins/counting.js.
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe, updateSettings } from '../../services/settingsService.js';
import { COUNT_MAX, runLocked, loadState, saveState, currentState } from '../../services/countingService.js';
import { successEmbed, errorEmbed, infoEmbed, guildEmbed } from '../../utils/embedBuilder.js';
import { safeDefer, safeReply, checkChannelUsable } from '../../utils/interactionHelpers.js';
import { newChannelOption, resolveNewOptions } from '../../utils/newTargets.js';
import { formatNumber } from '../../utils/text.js';
import logger from '../../utils/logger.js';
import { postNote } from '../../plugins/counting.js';

export const module = 'counting';
export const gated = false;                          // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

// What the bot needs in the counting channel: read counts, react to them and post notes.
const CHANNEL_NEEDS = ['ViewChannel', 'SendMessages', 'EmbedLinks', 'AddReactions', 'ReadMessageHistory'];

export const data = new SlashCommandBuilder()
  .setName('counting')
  .setDescription('A channel where members count up from 1, one number each.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('set').setDescription('Pick the counting channel and turn counting on.')
    .addChannelOption((o) => o.setName('channel').setDescription('The counting channel.').addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
    .addStringOption((o) => newChannelOption(o)))
  .addSubcommand((s) => s.setName('reset').setDescription('Set the count back to 0, or to the last number to carry on from.')
    .addIntegerOption((o) => o.setName('number').setDescription('The last number counted (default 0: start again from 1).')
      .setMinValue(0).setMaxValue(COUNT_MAX)))
  .addSubcommand((s) => s.setName('status').setDescription('Show the counting channel, the next number and the best run.'));

const RULES = 'Count up from **1**, one number per message. Nobody counts twice in a row, '
  + 'and a wrong number starts the count again from **1**.';

const MSG = {
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'),
  NO_CHANNEL: () => errorEmbed('No Counting Channel', 'Pick one first with `/counting set`.'),
  CANNOT_USE: (reason) => errorEmbed('Cannot Use That Channel', `${reason} Pick another channel, or fix my permissions there.`),
};

const OFF_NOTE = '\n\n⚠️ The **Counting** module is off. Turn it on with `/counting set` or `/module enable counting`.';

/** Final answer: overwrites the deferred reply. Never pings. */
function finish(interaction, embed) {
  return safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
}

// ── /counting set ─────────────────────────────────────────────────────────────

async function runSet(interaction) {
  if (!(await safeDefer(interaction))) return;
  if (!(await resolveNewOptions(interaction, data, { required: ['channel'] }))) return;
  const { guild, guildId } = interaction;
  const option = interaction.options.getChannel('channel', true);
  const channel = guild.channels.cache.get(option.id) ?? null;
  const usable = checkChannelUsable(channel, { needs: CHANNEL_NEEDS });
  if (!usable.ok) return finish(interaction, MSG.CANNOT_USE(usable.reason));

  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  const moved = settings.countingChannelId !== channel.id;

  try {
    await runLocked(guildId, async () => {
      // A new channel starts from 1; picking the current channel again keeps its count.
      if (moved) {
        const best = (await loadState(guildId))?.best ?? 0;
        await saveState(guildId, { channelId: channel.id, count: 0, lastUserId: null, lastMessageId: null, best });
      }
      await updateSettings(guildId, { countingChannelId: channel.id, countingEnabled: true });
    });
  } catch (err) {
    logger.error(`[counting] ${guildId}: set failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  logger.info(`[counting] ${guildId}: channel ${settings.countingChannelId ?? null} → ${channel.id} by ${interaction.user.id}`);

  if (moved) await postNote(channel, infoEmbed('Counting', RULES));
  const head = moved ? `Counting is on in <#${channel.id}>. The first number is **1**.` : `Counting is on in <#${channel.id}>.`;
  return finish(interaction, successEmbed('Counting Channel Set', `${head}\n${RULES}`));
}

// ── /counting reset ───────────────────────────────────────────────────────────

async function runReset(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  const channelId = settings.countingChannelId;
  if (!channelId) return finish(interaction, MSG.NO_CHANNEL());
  const number = interaction.options.getInteger('number') ?? 0;

  try {
    await runLocked(guildId, async () => {
      // The best run is only ever set by counting, never by an admin picking a number.
      const { best } = currentState(await loadState(guildId), channelId);
      await saveState(guildId, { channelId, count: number, lastUserId: null, lastMessageId: null, best });
    });
  } catch (err) {
    logger.error(`[counting] ${guildId}: reset failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  logger.info(`[counting] ${guildId}: count set to ${number} by ${interaction.user.id}`);

  const next = `The next number is **${formatNumber(number + 1)}**.`;
  const channel = guild.channels.cache.get(channelId);
  if (channel) await postNote(channel, infoEmbed('Count Reset', `An admin set the count to **${formatNumber(number)}**. ${next}`));
  const off = isModuleEnabled(settings, 'counting') ? '' : OFF_NOTE;
  return finish(interaction, successEmbed('Count Reset', `${next} (in <#${channelId}>)${off}`));
}

// ── /counting status ──────────────────────────────────────────────────────────

async function runStatus(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  const channelId = settings.countingChannelId;
  const enabled = isModuleEnabled(settings, 'counting');

  let row = null;
  let loaded = true;
  try {
    row = await loadState(guildId);
  } catch (err) {
    logger.warn(`[counting] ${guildId}: could not load the count: ${err?.message}`);
    loaded = false;
  }
  const state = currentState(row, channelId);
  const channel = channelId ? guild.channels.cache.get(channelId) : null;

  const embed = guildEmbed(settings, '🔢 Counting').addFields(
    { name: 'Status', value: enabled ? '✅ On' : '❌ Off — `/counting set`', inline: true },
    {
      name: 'Channel',
      value: !channelId ? 'Not set — `/counting set`' : (channel ? `<#${channelId}>` : `⚠️ Deleted (\`${channelId}\`)`),
      inline: true,
    },
    { name: 'Next number', value: loaded ? formatNumber(state.count + 1) : 'Unavailable', inline: true },
    { name: 'Last counted by', value: state.lastUserId ? `<@${state.lastUserId}>` : '—', inline: true },
    { name: 'Best run', value: loaded ? formatNumber(state.best) : 'Unavailable', inline: true },
  );
  if (channel) {
    const usable = checkChannelUsable(channel, { needs: CHANNEL_NEEDS });
    if (!usable.ok) embed.addFields({ name: 'Problem', value: `⚠️ ${usable.reason}` });
  }
  return finish(interaction, embed);
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'set') return runSet(interaction);
  if (sub === 'reset') return runReset(interaction);
  if (sub === 'status') return runStatus(interaction);
}
