// /autotime enable | disable | status | ignore add | ignore remove. Admin config command: replies are public (CLAUDE.md
// admin convention) and it keeps working while the Auto Timestamps module is off (gated = false). The answering itself
// is src/plugins/autotime.js.
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe, updateSettings, updateSettingsWith } from '../../services/settingsService.js';
import { successEmbed, errorEmbed, warnEmbed, guildEmbed } from '../../utils/embedBuilder.js';
import { safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import { normalizeTimezoneRoles } from '../../utils/timezones.js';
import { joinWithLimit } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = 'autotime';
export const gated = false;                          // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const MAX_IGNORED = LIMITS.AUTOTIME_IGNORED_CHANNELS;
const IGNORABLE_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildVoice,
  ChannelType.GuildStageVoice, ChannelType.GuildForum, ChannelType.GuildMedia, ChannelType.GuildCategory];

export const data = new SlashCommandBuilder()
  .setName('autotime')
  .setDescription('Answer times typed in chat ("fri 5pm") with a timestamp everyone sees in their own timezone.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('enable').setDescription('Turn Auto Timestamps on.'))
  .addSubcommand((s) => s.setName('disable').setDescription('Turn Auto Timestamps off.'))
  .addSubcommand((s) => s.setName('status').setDescription('Show who is answered and the ignored channels.'))
  .addSubcommandGroup((g) => g.setName('ignore').setDescription('Channels where times are never answered.')
    .addSubcommand((s) => s.setName('add').setDescription('Never answer times in a channel (a category covers its channels).')
      .addChannelOption((o) => o.setName('channel').setDescription('Channel or category.').setRequired(true).addChannelTypes(...IGNORABLE_TYPES)))
    .addSubcommand((s) => s.setName('remove').setDescription('Answer times in a channel again.')
      .addChannelOption((o) => o.setName('channel').setDescription('Channel or category.').setRequired(true).addChannelTypes(...IGNORABLE_TYPES))));

export const HOW_IT_WORKS = 'When a member types a time such as `fri 5pm`, `tomorrow at 18:30` or `7pm EST`, I reply with a '
  + 'Discord timestamp that everyone sees in their own timezone. Times are read in the timezone of the member who typed '
  + 'them: their own (`/timezone set`) or their timezone role. Members with neither are only answered when they name '
  + 'the zone (`7pm EST`); otherwise I tell them to pick a timezone role or use `/timezone set`.';

const MSG = {
  SETTINGS_UNAVAILABLE: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  COULD_NOT_SAVE: () => errorEmbed('Could Not Save', 'The change could not be saved. Please try again in a moment.'),
};

const OFF_NOTE = '\n\n⚠️ The **Auto Timestamps** module is off. Turn it on with `/autotime enable`.';

/** Final answer: overwrites the deferred reply. Never pings. */
function finish(interaction, embed) {
  return safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
}

/**
 * The line on timezone roles: how many are linked (and still exist), or how to set them up.
 * @param {object} settings
 * @param {import('discord.js').Guild} guild
 * @returns {string}
 */
export function timezoneRolesLine(settings, guild) {
  const count = normalizeTimezoneRoles(settings?.timezoneRoles).filter((l) => guild?.roles?.cache?.has(l.roleId)).length;
  if (count) return `**${count}** timezone role${count === 1 ? ' is' : 's are'} linked (\`/settings timezone-role list\`).`;
  return 'No timezone roles are linked yet. `/reactionrole create template:timezones-us` (or `timezones-world`) posts a '
    + 'panel that creates and links them, or link roles you have with `/settings timezone-role add`.';
}

// ── /autotime enable | disable ────────────────────────────────────────────────

async function runToggle(interaction, on) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  let settings;
  try {
    settings = await updateSettings(guildId, { autoTimeEnabled: on });
  } catch (err) {
    logger.error(`[autotime] ${guildId}: ${on ? 'enable' : 'disable'} failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (!on) return finish(interaction, successEmbed('Auto Timestamps Off', 'Times typed in chat are no longer answered. The ignored channels are kept.'));
  return finish(interaction, successEmbed('Auto Timestamps On', `${HOW_IT_WORKS}\n\n${timezoneRolesLine(settings, guild)}`));
}

// ── /autotime status ──────────────────────────────────────────────────────────

async function runStatus(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  if (!settings) return finish(interaction, MSG.SETTINGS_UNAVAILABLE());
  const ids = settings.autoTimeIgnoredChannelIds ?? [];
  const ignored = ids.length
    ? joinWithLimit(ids.map((id) => (guild.channels.cache.has(id) ? `<#${id}>` : `#deleted-channel (\`${id}\`)`)), { max: 1000 })
    : 'None';
  const embed = guildEmbed(settings, '🕒 Auto Timestamps', `${HOW_IT_WORKS}\n\n${timezoneRolesLine(settings, guild)}`)
    .addFields(
      { name: 'Status', value: isModuleEnabled(settings, 'autotime') ? '✅ On' : '❌ Off (`/autotime enable`)', inline: true },
      { name: `Ignored channels (${ids.length}/${MAX_IGNORED})`, value: ignored, inline: true },
    );
  return finish(interaction, embed);
}

// ── /autotime ignore add | remove ─────────────────────────────────────────────

async function runIgnoreAdd(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guildId } = interaction;
  const channel = interaction.options.getChannel('channel', true);
  let outcome = 'added';
  let settings;
  try {
    settings = await updateSettingsWith(guildId, (cur) => {
      const list = cur.autoTimeIgnoredChannelIds ?? [];
      if (list.includes(channel.id)) { outcome = 'exists'; return null; }
      if (list.length >= MAX_IGNORED) { outcome = 'limit'; return null; }
      return { autoTimeIgnoredChannelIds: [...list, channel.id] };
    });
  } catch (err) {
    logger.error(`[autotime] ${guildId}: ignore add failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (outcome === 'exists') return finish(interaction, warnEmbed('Already Ignored', `Times in <#${channel.id}> are already ignored.`));
  if (outcome === 'limit') {
    return finish(interaction, errorEmbed('Limit Reached', `You can ignore at most ${MAX_IGNORED} channels. Remove one with \`/autotime ignore remove\` first.`));
  }
  const where = channel.type === ChannelType.GuildCategory ? `any channel of **${channel.name}**` : `<#${channel.id}>`;
  const off = isModuleEnabled(settings, 'autotime') ? '' : OFF_NOTE;
  return finish(interaction, successEmbed('Channel Ignored', `Times typed in ${where} will not be answered.${off}`));
}

async function runIgnoreRemove(interaction) {
  if (!(await safeDefer(interaction))) return;
  const { guildId } = interaction;
  const id = interaction.options.getChannel('channel', true).id;
  let outcome = 'removed';
  let settings;
  try {
    settings = await updateSettingsWith(guildId, (cur) => {
      const list = cur.autoTimeIgnoredChannelIds ?? [];
      if (!list.includes(id)) { outcome = 'missing'; return null; }
      return { autoTimeIgnoredChannelIds: list.filter((x) => x !== id) };
    });
  } catch (err) {
    logger.error(`[autotime] ${guildId}: ignore remove failed`, err);
    return finish(interaction, MSG.COULD_NOT_SAVE());
  }
  if (outcome === 'missing') return finish(interaction, warnEmbed('Not Ignored', `<#${id}> is not on the ignore list.`));
  const off = isModuleEnabled(settings, 'autotime') ? '' : OFF_NOTE;
  return finish(interaction, successEmbed('Channel No Longer Ignored', `Times typed in <#${id}> are answered again.${off}`));
}

// ── Entry point ───────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  if (group === 'ignore') return sub === 'add' ? runIgnoreAdd(interaction) : runIgnoreRemove(interaction);
  if (sub === 'enable') return runToggle(interaction, true);
  if (sub === 'disable') return runToggle(interaction, false);
  if (sub === 'status') return runStatus(interaction);
}
