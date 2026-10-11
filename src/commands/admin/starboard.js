// /starboard create | edit | delete | list (pkg-starboard.md §4). Admin config command: replies are public (CLAUDE.md admin
// convention) except the delete prompt, and it keeps working while the Starboards module is off (gated = false).
import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { PERMISSION_LEVELS, LIMITS } from '../../config/constants.js';
import { successEmbed, errorEmbed, infoEmbed, applyGuildColor } from '../../utils/embedBuilder.js';
import {
  checkChannelUsable, permissionNames, respondAutocomplete, awaitConfirm, safeDefer, safeReply, missingBotPermissions,
} from '../../utils/interactionHelpers.js';
import { getSettingsSafe, isModuleEnabled } from '../../services/settingsService.js';
import * as sb from '../../services/starboardService.js';
import { joinWithLimit, clampText } from '../../utils/text.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import logger from '../../utils/logger.js';

export const module = 'starboard';
export const gated = false;                          // config works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const BOARD_CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const SOURCE_CHANNEL_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildVoice,
  ChannelType.GuildStageVoice, ChannelType.GuildForum, ChannelType.GuildMedia, ChannelType.GuildCategory];

function addSettingOptions(s) {
  return s
    .addStringOption((o) => o.setName('emoji').setDescription('Emojis that count, space-separated (default ⭐; "any" = every emoji)').setMaxLength(200))
    .addIntegerOption((o) => o.setName('threshold').setDescription('Reactions needed to repost (default 3)').setMinValue(1).setMaxValue(500))
    .addBooleanOption((o) => o.setName('self_star').setDescription("Count the author's own reaction (default False)"))
    .addBooleanOption((o) => o.setName('remove_self_stars').setDescription("Remove the author's own reaction (default False)"))
    .addBooleanOption((o) => o.setName('ignore_bots').setDescription('Ignore bot messages and bot reactions (default True)'))
    .addBooleanOption((o) => o.setName('remove_below').setDescription('Delete the repost if it falls below the threshold (default True)'))
    .addBooleanOption((o) => o.setName('auto_react').setDescription('React to the repost with the first emoji (default False)'))
    .addIntegerOption((o) => o.setName('max_age_days').setDescription('Ignore messages older than this many days (0 = no limit)').setMinValue(0).setMaxValue(3650))
    .addBooleanOption((o) => o.setName('allow_nsfw').setDescription('Repost from age-restricted channels (default False)'));
}

export const data = new SlashCommandBuilder()
  .setName('starboard')
  .setDescription('Starboards: repost popular messages to a channel (MEE6 Starboards).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => addSettingOptions(
    s.setName('create').setDescription('Create a starboard (up to 10).')
      .addStringOption((o) => o.setName('name').setDescription('Board name').setRequired(true).setMinLength(1).setMaxLength(32))
      .addChannelOption((o) => o.setName('channel').setDescription('Channel to repost to').setRequired(true).addChannelTypes(...BOARD_CHANNEL_TYPES))))
  .addSubcommand((s) => addSettingOptions(
    s.setName('edit').setDescription('Change a starboard, including its repost channel.')
      .addStringOption((o) => o.setName('name').setDescription('Board to edit').setRequired(true).setAutocomplete(true).setMaxLength(32))
      .addChannelOption((o) => o.setName('channel').setDescription('New repost channel').addChannelTypes(...BOARD_CHANNEL_TYPES)))
    .addChannelOption((o) => o.setName('ignore_channel').setDescription('Add a channel to the ignore list').addChannelTypes(...SOURCE_CHANNEL_TYPES))
    .addChannelOption((o) => o.setName('unignore_channel').setDescription('Remove a channel from the ignore list').addChannelTypes(...SOURCE_CHANNEL_TYPES))
    .addBooleanOption((o) => o.setName('enabled').setDescription('Turn this board on or off'))
    .addStringOption((o) => o.setName('rename').setDescription('New board name').setMinLength(1).setMaxLength(32)))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete a starboard (existing reposts stay).')
    .addStringOption((o) => o.setName('name').setDescription('Board to delete').setRequired(true).setAutocomplete(true).setMaxLength(32)))
  .addSubcommand((s) => s.setName('list').setDescription('List starboards and their settings.'));

// ── Shared pieces ─────────────────────────────────────────────────────────────

const MODULE_OFF_NOTE = '\n\n⚠️ The Starboards module is off, so nothing is reposted. Turn it on with `/module enable starboard`.';
const SELF_STAR_CONFLICT = 'Self-stars are counted on this board, so they cannot also be removed.';

const invalidNameEmbed = () => errorEmbed('Invalid Name', 'Board names may use letters a–z, digits, - and _ (max 32).');
const notFoundEmbed = (name) => errorEmbed('Not Found', `No starboard named **${clampText(name, 40)}**. See \`/starboard list\`.`);
const nameTakenEmbed = (name) => errorEmbed('Name Taken', `A starboard named **${name}** already exists.`);
const nsfwEmbed = () => errorEmbed('Channel Not Age-Restricted', 'To repost from NSFW channels, the starboard channel must be age-restricted too. Mark it as age-restricted first.');
const failedEmbed = () => errorEmbed('Something Went Wrong', 'I could not save that starboard. Please try again in a moment.');

/** option name, Starboard field, getter. Options a subcommand does not have read as null. */
const SETTING_OPTIONS = [
  ['threshold', 'threshold', 'getInteger'],
  ['self_star', 'selfStar', 'getBoolean'],
  ['remove_self_stars', 'removeSelfStars', 'getBoolean'],
  ['ignore_bots', 'ignoreBots', 'getBoolean'],
  ['remove_below', 'removeBelowThreshold', 'getBoolean'],
  ['auto_react', 'autoReact', 'getBoolean'],
  ['max_age_days', 'maxMessageAgeDays', 'getInteger'],
  ['allow_nsfw', 'allowNsfw', 'getBoolean'],
  ['enabled', 'enabled', 'getBoolean'],
];

/** The setting options the invoker actually gave, keyed by Starboard field. */
function readSettings(options) {
  const out = {};
  for (const [name, field, getter] of SETTING_OPTIONS) {
    const value = options[getter](name);
    if (value !== null) out[field] = value;
  }
  return out;
}

/** A channel option resolved through the guild cache first (an API-resolved channel has no permissionsFor). */
const resolveChannel = (interaction, option) => (option ? (interaction.guild.channels.cache.get(option.id) ?? option) : null);

async function loadModuleState(guildId) {
  const settings = await getSettingsSafe(guildId);
  return { settings, moduleOff: settings !== null && !isModuleEnabled(settings, 'starboard') };
}

/** An error embed when the bot cannot post (and react, with autoReact) in the board channel, else null. */
function boardChannelProblem(channel, { autoReact }) {
  const needs = ['ViewChannel', 'SendMessages', 'EmbedLinks'];
  if (autoReact) needs.push('AddReactions', 'ReadMessageHistory');
  const usable = checkChannelUsable(channel, { needs });
  if (usable.ok) return null;
  const detail = usable.missing.length
    ? `I need **${permissionNames(usable.missing).join(', ')}** in ${channel} to post starboard reposts.`
    : usable.reason;
  return errorEmbed('Missing Permissions', detail);
}

/** A reminder shown when remove_self_stars is on but the bot cannot remove reactions anywhere. */
const selfStarPermissionNote = (guild, board) => (board.removeSelfStars && missingBotPermissions(guild, 'ManageMessages').length
  ? '\n\n⚠️ Removing self-stars needs **Manage Messages** in the watched channels, which I do not have server-wide.'
  : '');

/** Why a board cannot post right now, computed live from the channel cache; null when it can. */
function boardProblem(guild, board) {
  const channel = guild.channels.cache.get(board.channelId);
  if (!channel) return 'Channel deleted — set a new one with /starboard edit';
  const usable = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks'] });
  if (usable.ok) return null;
  return `I can't post there (missing ${usable.missing.length ? permissionNames(usable.missing).join(', ') : usable.reason})`;
}

// ── Subcommands ───────────────────────────────────────────────────────────────

async function runCreate(interaction, client) {
  if (!(await safeDefer(interaction))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed] });
  try {
    const { moduleOff } = await loadModuleState(interaction.guildId);
    const name = sb.normalizeBoardName(interaction.options.getString('name', true));
    if (!name) return await reply(invalidNameEmbed());

    let emojis = ['⭐'];
    const emojiOption = interaction.options.getString('emoji');
    if (emojiOption !== null) {
      const parsed = sb.parseEmojiList(emojiOption, interaction.guild);
      if (!parsed.ok) return await reply(errorEmbed('Invalid Emoji', parsed.error));
      emojis = parsed.emojis;
    }

    const channel = resolveChannel(interaction, interaction.options.getChannel('channel', true));
    const settings = readSettings(interaction.options);
    if (settings.removeSelfStars === true && settings.selfStar === true) return await reply(errorEmbed('Invalid Options', SELF_STAR_CONFLICT));
    const problem = boardChannelProblem(channel, { autoReact: settings.autoReact === true });
    if (problem) return await reply(problem);
    if (settings.allowNsfw === true && !channel.nsfw) return await reply(nsfwEmbed());

    let board;
    try {
      board = await sb.createBoard(interaction.guildId, { name, channelId: channel.id, emojis, ...settings });
    } catch (err) {
      if (err instanceof sb.BoardLimitError) {
        return await reply(errorEmbed('Limit Reached', `A server can have at most ${LIMITS.STARBOARDS} starboards. Delete one with \`/starboard delete\` first.`));
      }
      if (sb.isUniqueViolation(err)) return await reply(nameTakenEmbed(name));
      throw err;
    }

    const description = `**${board.name}** reposts messages with **${board.threshold}+** ${sb.formatEmojiList(board.emojis, client)} reactions to ${channel}.\n`
      + `${sb.settingsSummary(board, { client })}${selfStarPermissionNote(interaction.guild, board)}${moduleOff ? MODULE_OFF_NOTE : ''}`;
    return await reply(successEmbed('Starboard Created', description));
  } catch (err) {
    logger.error('starboard create failed', err);
    return reply(failedEmbed());
  }
}

const onOff = (v) => (v ? 'on' : 'off');
/** Label, field, display (for the "old → new" lines of /starboard edit). */
const DIFF_FIELDS = [
  ['Name', 'name', (v) => String(v)],
  ['Channel', 'channelId', (v) => `<#${v}>`],
  ['Emojis', 'emojis', null],
  ['Threshold', 'threshold', (v) => String(v)],
  ['Self-star', 'selfStar', onOff],
  ['Remove self-stars', 'removeSelfStars', onOff],
  ['Bots', 'ignoreBots', (v) => (v ? 'ignored' : 'counted')],
  ['Below threshold', 'removeBelowThreshold', (v) => (v ? 'remove' : 'keep')],
  ['Auto-react', 'autoReact', onOff],
  ['Max age', 'maxMessageAgeDays', (v) => (v > 0 ? `${v} days` : 'no limit')],
  ['NSFW', 'allowNsfw', (v) => (v ? 'allowed' : 'blocked')],
  ['Enabled', 'enabled', onOff],
];

function describeChanges(before, after, client) {
  const lines = [];
  for (const [label, field, show] of DIFF_FIELDS) {
    if (JSON.stringify(before[field]) === JSON.stringify(after[field])) continue;
    const fmt = show ?? ((v) => sb.formatEmojiList(v, client));
    lines.push(`• **${label}:** ${fmt(before[field])} → ${fmt(after[field])}`);
  }
  return lines;
}

async function runEdit(interaction, client) {
  if (!(await safeDefer(interaction))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed] });
  const { guild, options } = interaction;
  let patchName = null;
  try {
    const { moduleOff } = await loadModuleState(interaction.guildId);
    const rawName = options.getString('name', true);
    const board = await sb.getBoardByName(guild.id, sb.normalizeBoardName(rawName));
    if (!board) return await reply(notFoundEmbed(rawName));

    const given = interaction.options.data[0]?.options ?? [];
    if (!given.some((o) => o.name !== 'name')) return await reply(errorEmbed('Nothing To Change', 'Pass at least one option to change.'));

    const patch = readSettings(options);
    const rename = options.getString('rename');
    if (rename !== null) {
      patchName = sb.normalizeBoardName(rename);
      if (!patchName) return await reply(invalidNameEmbed());
      patch.name = patchName;
    }
    const emojiOption = options.getString('emoji');
    if (emojiOption !== null) {
      const parsed = sb.parseEmojiList(emojiOption, guild);
      if (!parsed.ok) return await reply(errorEmbed('Invalid Emoji', parsed.error));
      patch.emojis = parsed.emojis;
    }
    const newChannel = resolveChannel(interaction, options.getChannel('channel'));
    if (newChannel) patch.channelId = newChannel.id;

    const effective = { ...board, ...patch };
    if (effective.selfStar && effective.removeSelfStars) return await reply(errorEmbed('Invalid Options', SELF_STAR_CONFLICT));

    // The channel the reposts will go to. A board channel missing from the cache is not treated as deleted: skip the checks.
    const targetChannel = newChannel ?? guild.channels.cache.get(board.channelId) ?? null;
    if (targetChannel && (newChannel || patch.autoReact === true)) {
      const problem = boardChannelProblem(targetChannel, { autoReact: effective.autoReact });
      if (problem) return await reply(problem);
    }
    if (targetChannel && effective.allowNsfw && !targetChannel.nsfw) return await reply(nsfwEmbed());

    const ignore = resolveChannel(interaction, options.getChannel('ignore_channel'));
    const unignore = resolveChannel(interaction, options.getChannel('unignore_channel'));
    if (ignore && unignore && ignore.id === unignore.id) {
      return await reply(errorEmbed('Invalid Options', 'Choose different channels for ignore_channel and unignore_channel.'));
    }
    if (ignore && !board.ignoredChannelIds.includes(ignore.id) && board.ignoredChannelIds.length >= sb.MAX_IGNORED_CHANNELS) {
      return await reply(errorEmbed('Limit Reached', `A starboard can ignore at most ${sb.MAX_IGNORED_CHANNELS} channels.`));
    }
    if (ignore) patch.addIgnored = ignore.id;
    if (unignore) patch.removeIgnored = unignore.id;

    let result;
    try {
      result = await sb.updateBoard(guild.id, board.id, patch);
    } catch (err) {
      if (err instanceof sb.NotFoundError) return await reply(notFoundEmbed(rawName));
      if (err instanceof sb.IgnoreLimitError) return await reply(errorEmbed('Limit Reached', `A starboard can ignore at most ${sb.MAX_IGNORED_CHANNELS} channels.`));
      if (err instanceof sb.SettingsConflictError) return await reply(errorEmbed('Invalid Options', SELF_STAR_CONFLICT));
      if (sb.isUniqueViolation(err)) return await reply(nameTakenEmbed(patchName ?? rawName));
      throw err;
    }
    const { before, after } = result;

    const lines = describeChanges(before, after, client);
    if (ignore) {
      lines.push(before.ignoredChannelIds.includes(ignore.id) ? `${ignore} was already ignored` : `+ ${ignore} ignored`);
    }
    if (unignore) {
      lines.push(before.ignoredChannelIds.includes(unignore.id) ? `− ${unignore} no longer ignored` : `${unignore} was not ignored`);
    }
    let description = `**${after.name}**\n${lines.length ? lines.join('\n') : 'No settings changed.'}`;
    if (before.channelId !== after.channelId) {
      description += `\nExisting reposts stay in <#${before.channelId}>; new reposts go to <#${after.channelId}>.`;
    }
    description += `\n\n${sb.settingsSummary(after, { client })}${selfStarPermissionNote(guild, after)}`;
    if (!after.enabled) description += `\n\nThis board is **off**. Turn it back on with \`/starboard edit name:${after.name} enabled:True\`.`;
    if (moduleOff) description += MODULE_OFF_NOTE;
    return await reply(successEmbed('Starboard Updated', description));
  } catch (err) {
    logger.error('starboard edit failed', err);
    return reply(failedEmbed());
  }
}

async function runDelete(interaction) {
  // Deferred first: a slow database cannot miss Discord's 3-second window, and awaitConfirm edits the deferred reply.
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  try {
    const rawName = interaction.options.getString('name', true);
    const board = await sb.getBoardByName(interaction.guildId, sb.normalizeBoardName(rawName));
    if (!board) {
      await safeReply(interaction, { embeds: [notFoundEmbed(rawName)] });
      return;
    }
    const confirmed = await awaitConfirm(interaction, {
      title: 'Delete starboard?',
      description: `Delete **${board.name}**? Reposts already in <#${board.channelId}> stay, but they stop updating.`,
      confirmLabel: 'Delete',
    });
    if (!confirmed) return;

    const deleted = await sb.deleteBoard(interaction.guildId, board.id);
    if (deleted === 0) {
      await safeReply(interaction, { embeds: [errorEmbed('Not Found', 'That starboard was already deleted.')], ephemeral: true });
      return;
    }
    await safeReply(interaction, {
      embeds: [successEmbed('Starboard Deleted', `**${board.name}** was deleted. Its reposts in <#${board.channelId}> were kept.`)],
    });
  } catch (err) {
    logger.error('starboard delete failed', err);
    await safeReply(interaction, { embeds: [errorEmbed('Something Went Wrong', 'I could not delete that starboard. Please try again in a moment.')], ephemeral: true });
  }
}

function listFieldValue(guild, board, client, ignoredMax) {
  const lines = [sb.settingsSummary(board, { client, reposts: board.reposts })];
  if (board.ignoredChannelIds.length) {
    lines.push(`Ignored: ${joinWithLimit(board.ignoredChannelIds.map((id) => `<#${id}>`), { max: ignoredMax })}`);
  }
  const problem = boardProblem(guild, board);
  if (problem) lines.push(`⚠️ ${problem}`);
  return lines.join('\n');
}

async function runList(interaction, client) {
  if (!(await safeDefer(interaction))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed] });
  try {
    const { settings, moduleOff } = await loadModuleState(interaction.guildId);
    const boards = await sb.listBoardsWithCounts(interaction.guildId);
    if (!boards.length) {
      return await reply(applyGuildColor(infoEmbed('Starboards', `No starboards yet. Create one with \`/starboard create\`.${moduleOff ? MODULE_OFF_NOTE : ''}`), settings));
    }

    const names = boards.map((b) => clampText(`🌟 ${b.name}${b.enabled ? '' : ' (off)'}`, 256));
    // Ten verbose boards would pass Discord's 6000-character embed total, so every value shares what is left.
    const perField = Math.min(1024, Math.floor((sb.LIST_EMBED_BUDGET - names.reduce((n, s) => n + s.length, 0)) / boards.length));
    const fields = boards.map((b, i) => {
      let value = listFieldValue(interaction.guild, b, client, 300);
      if (value.length > perField) value = listFieldValue(interaction.guild, b, client, 60);
      return { name: names[i], value: clampText(value, perField) };
    });
    const embed = applyGuildColor(infoEmbed('Starboards', `${boards.length}/${LIMITS.STARBOARDS} boards${moduleOff ? MODULE_OFF_NOTE : ''}`), settings).addFields(fields);
    return await reply(embed);
  } catch (err) {
    logger.error('starboard list failed', err);
    return reply(errorEmbed('Something Went Wrong', 'I could not load the starboards. Please try again in a moment.'));
  }
}

export async function execute(interaction, client) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'create') return runCreate(interaction, client);
  if (sub === 'edit') return runEdit(interaction, client);
  if (sub === 'delete') return runDelete(interaction, client);
  if (sub === 'list') return runList(interaction, client);
  return undefined;
}

export async function autocomplete(interaction) {
  const focused = String(interaction.options.getFocused() ?? '').toLowerCase().slice(0, 32);
  // Bounded by the 3 s answer window (rule 21); a slow database just gives an empty list.
  const rows = await withTimeout(sb.searchBoards(interaction.guildId, focused), ackBudgetMs(interaction), 'starboard autocomplete').catch(() => []);
  await respondAutocomplete(interaction, rows.map((b) => ({
    name: `${b.name} → #${interaction.guild.channels.cache.get(b.channelId)?.name ?? 'deleted-channel'}${b.enabled ? '' : ' (off)'}`,
    value: b.name,
  })));
}
