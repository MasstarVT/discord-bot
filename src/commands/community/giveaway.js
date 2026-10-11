// /giveaway (pkg-giveaways): MEE6 Giveaways with a persistent Participate button, role-based odds, required and denied
// roles, XP/coin prizes through the rewards bus and up to 90 winners. This file is thin: option parsing, validation and
// replies. Every flow (start, end, reroll, cancel, restart, odds, the button, the end job) lives in
// services/giveawayService.js. Every reply is ephemeral; the giveaway message and the announcements are the public output.
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import prisma from '../../database/client.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettings } from '../../services/settingsService.js';
import {
  DEFAULT_BONUS_WEIGHT, ENTRANTS_SHOWN, LIST_SHOWN, MAX_PRIZE_AMOUNT, MAX_ROLE_WEIGHTS, MIN_DURATION_MS, POST_NEEDS,
  STATUS_EMOJI, STATUS_TEXT, STATUS_TEXT_SHORT,
  cancelGiveaway, endGiveaway, fetchChannel, formatListLine, normalizeRoleWeights, rerollGiveaway, resolveGiveawayRef,
  restartGiveaway, rewardUnavailableMessage, startGiveaway, updateOdds, validateEndsAt,
} from '../../services/giveawayService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { parseColor } from '../../utils/customEmbed.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import {
  awaitConfirm, checkChannelUsable, permissionNames, pingableRoleIds, respondAutocomplete, safeDefer,
} from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { formatDuration } from '../../utils/parseDuration.js';
import { clampText, formatNumber, joinWithLimit } from '../../utils/text.js';
import { discordTimestamp, parseDateInput } from '../../utils/time.js';

export const module = 'giveaways';
export const gated = true;
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;
// No allowInDMs export: buildCommandPayload registers the command for servers only.

// ── Builder ───────────────────────────────────────────────────────────────────
// The payload budget is tight (about 1900 of 2000 characters): do not lengthen any description.

const gw = (o) => o.setName('giveaway').setDescription('Pick a giveaway, or paste its ID or message link')
  .setRequired(true).setAutocomplete(true).setMaxLength(100);
const TEXT_TYPES = [ChannelType.GuildText, ChannelType.GuildAnnouncement];

export const data = new SlashCommandBuilder()
  .setName('giveaway')
  .setDescription('Run giveaways with a Participate button (MEE6 Giveaways, /reroll)')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addSubcommand((s) => s.setName('start').setDescription('Start a giveaway')
    .addStringOption((o) => o.setName('prize').setDescription('What the winners get').setRequired(true).setMaxLength(256))
    .addStringOption((o) => o.setName('duration').setDescription('Length or end time: 30m, 2d, 1w, or 2026-12-24 18:00 (server timezone)').setRequired(true).setMaxLength(40))
    .addIntegerOption((o) => o.setName('winners').setDescription('Number of winners (1-90, default 1)').setMinValue(1).setMaxValue(LIMITS.GIVEAWAY_WINNERS))
    .addChannelOption((o) => o.setName('channel').setDescription('Channel to post in (default: this one)').addChannelTypes(...TEXT_TYPES))
    .addRoleOption((o) => o.setName('required_role').setDescription('Only members with this role can enter'))
    .addRoleOption((o) => o.setName('denied_role').setDescription('Members with this role cannot enter'))
    .addRoleOption((o) => o.setName('bonus_role').setDescription('Members with this role get better odds'))
    .addIntegerOption((o) => o.setName('bonus_weight').setDescription('Entries each bonus_role member counts as (2-10, default 2)').setMinValue(2).setMaxValue(10))
    .addIntegerOption((o) => o.setName('xp').setDescription('XP for each winner (needs the Levels module)').setMinValue(1).setMaxValue(MAX_PRIZE_AMOUNT))
    .addIntegerOption((o) => o.setName('coins').setDescription('Coins for each winner (needs the Economy module)').setMinValue(1).setMaxValue(MAX_PRIZE_AMOUNT))
    .addRoleOption((o) => o.setName('ping_role').setDescription('Role to ping when the giveaway starts'))
    .addStringOption((o) => o.setName('description').setDescription('Extra text shown in the giveaway').setMaxLength(1000))
    .addStringOption((o) => o.setName('color').setDescription('Embed colour, e.g. #5865F2 or blurple').setMaxLength(16)))
  .addSubcommand((s) => s.setName('end').setDescription('End a giveaway now and draw its winners').addStringOption(gw))
  .addSubcommand((s) => s.setName('reroll').setDescription('Draw new winners for an ended giveaway (MEE6 /reroll)').addStringOption(gw)
    .addIntegerOption((o) => o.setName('winners').setDescription('How many new winners (default 1)').setMinValue(1).setMaxValue(LIMITS.GIVEAWAY_WINNERS)))
  .addSubcommand((s) => s.setName('cancel').setDescription('Cancel a giveaway without drawing winners').addStringOption(gw))
  .addSubcommand((s) => s.setName('restart').setDescription('Republish a draft giveaway (its message was deleted)').addStringOption(gw)
    .addStringOption((o) => o.setName('duration').setDescription('New length or end time (default: keep the old end time)').setMaxLength(40))
    .addChannelOption((o) => o.setName('channel').setDescription('Channel to post in (default: the original one)').addChannelTypes(...TEXT_TYPES)))
  .addSubcommand((s) => s.setName('list').setDescription("List this server's giveaways")
    .addStringOption((o) => o.setName('status').setDescription('Which giveaways to show (default: open)').addChoices(
      { name: 'Open (active and draft)', value: 'open' }, { name: 'Ended', value: 'ended' },
      { name: 'Cancelled', value: 'cancelled' }, { name: 'All', value: 'all' })))
  .addSubcommand((s) => s.setName('entrants').setDescription('Show who entered a giveaway').addStringOption(gw))
  .addSubcommandGroup((g) => g.setName('odds').setDescription('Adjust winning odds by role')
    .addSubcommand((s) => s.setName('add').setDescription('Give a role better odds in a giveaway').addStringOption(gw)
      .addRoleOption((o) => o.setName('role').setDescription('Role that gets better odds').setRequired(true))
      .addIntegerOption((o) => o.setName('weight').setDescription('Entries each member of the role counts as (2-10)').setRequired(true).setMinValue(2).setMaxValue(10)))
    .addSubcommand((s) => s.setName('remove').setDescription("Remove a role's bonus odds from a giveaway").addStringOption(gw)
      .addRoleOption((o) => o.setName('role').setDescription('Role to remove').setRequired(true))));

// ── Helpers ───────────────────────────────────────────────────────────────────

const GENERIC_ERROR = 'Discord or the database did not respond. Please try again in a moment.';
const NOT_FOUND_TEXT = "I couldn't find that giveaway. Pick one from the list, or paste its ID, message ID or message link.";

/** Replies with an error embed. Never throws (the interaction may have expired). */
async function fail(interaction, title, text, extra = {}) {
  try {
    await interaction.editReply({ embeds: [errorEmbed(title, text)], ...extra });
  } catch (err) {
    logger.debug(`giveaway: could not send the error reply: ${err?.message}`);
  }
}

const mentionsOf = (ids) => joinWithLimit(ids.map((id) => `<@${id}>`), { max: 3500 });

async function resolveRef(interaction, extra = {}) {
  const g = await resolveGiveawayRef(interaction.guildId, interaction.options.getString('giveaway', true));
  if (!g) await fail(interaction, 'Giveaway Not Found', NOT_FOUND_TEXT, extra);
  return g;
}

/** start steps 1-3, shared with restart. @returns {Promise<boolean>} false when it already replied */
async function checkPostChannel(interaction, channel) {
  if (!TEXT_TYPES.includes(channel?.type)) {
    await fail(interaction, 'Unsupported Channel', 'Giveaways can be posted in text or announcement channels only. Run this in one, or pick one with `channel`.');
    return false;
  }
  if (!channel.permissionsFor(interaction.member)?.has([PermissionFlagsBits.ViewChannel, PermissionFlagsBits.SendMessages])) {
    await fail(interaction, 'No Access', `You can't send messages in ${channel}.`);
    return false;
  }
  const u = checkChannelUsable(channel, { needs: [...POST_NEEDS] });
  if (!u.ok) {
    await fail(
      interaction,
      'Missing Permissions',
      u.missing.length ? `I need **${permissionNames(u.missing).join(', ')}** in ${channel} to run a giveaway there.` : u.reason,
    );
    return false;
  }
  return true;
}

const optionChannel = (interaction) => {
  const picked = interaction.options.getChannel('channel');
  return picked ? (interaction.guild.channels.cache.get(picked.id) ?? picked) : null;
};

/** Parses `duration` in the server timezone and validates it. @returns {Date|null} null when it already replied */
async function parseEndsAt(interaction, settings, text) {
  const now = new Date();
  const endsAt = parseDateInput(text, settings.timezone, { now });
  const v = validateEndsAt(endsAt, now, settings.timezone);
  if (!v.ok) {
    await fail(interaction, v.title, v.message);
    return null;
  }
  return endsAt;
}

const failStartOrRestart = (interaction, res, channel) => {
  if (res.code === 'LIMIT') {
    return fail(interaction, 'Giveaway Limit Reached', `This server already has ${LIMITS.ACTIVE_GIVEAWAYS} running or draft giveaways. End or cancel one first.`);
  }
  if (res.code === 'SEND_FAILED') return fail(interaction, 'Could Not Post', `I couldn't post the giveaway in ${channel}. Check my permissions there.`);
  return fail(interaction, 'Something Went Wrong', GENERIC_ERROR);
};

// ── Subcommands ───────────────────────────────────────────────────────────────

async function start(interaction, client, settings) {
  const { guild, options } = interaction;

  // 1-3. Channel, executor access, bot permissions.
  const channel = optionChannel(interaction) ?? interaction.channel;
  if (!(await checkPostChannel(interaction, channel))) return;

  // 4. Duration.
  const endsAt = await parseEndsAt(interaction, settings, options.getString('duration', true));
  if (!endsAt) return;

  // 5. Winners.
  const winnerCount = options.getInteger('winners') ?? 1;

  // 6. Required and denied roles. Managed roles are allowed (for example the Server Booster role): nothing is assigned.
  const requiredRole = options.getRole('required_role');
  if (requiredRole && requiredRole.id === guild.id) {
    return fail(interaction, 'Invalid Role', 'Choose a specific role for `required_role`, not @everyone.');
  }
  const deniedRole = options.getRole('denied_role');
  if (deniedRole && deniedRole.id === guild.id) {
    return fail(interaction, 'Invalid Role', 'Choose a specific role for `denied_role`, not @everyone.');
  }
  if (requiredRole && deniedRole && requiredRole.id === deniedRole.id) {
    return fail(interaction, 'Invalid Role', 'The same role cannot be both required and denied.');
  }
  const requiredRoleIds = requiredRole ? [requiredRole.id] : [];
  const deniedRoleIds = deniedRole ? [deniedRole.id] : [];

  // 7. Bonus odds.
  const bonusRole = options.getRole('bonus_role');
  const bonusWeight = options.getInteger('bonus_weight');
  if (bonusWeight != null && !bonusRole) return fail(interaction, 'Missing Role', 'Pick a `bonus_role` to use `bonus_weight`.');
  if (bonusRole && bonusRole.id === guild.id) {
    return fail(interaction, 'Invalid Role', 'Bonus odds for @everyone would change nothing. Choose a specific role.');
  }
  const roleWeights = bonusRole ? [{ roleId: bonusRole.id, weight: bonusWeight ?? DEFAULT_BONUS_WEIGHT }] : [];

  // 8. XP and coin prizes.
  const xpPrize = options.getInteger('xp');
  const coinPrize = options.getInteger('coins');
  for (const [kind, amount] of [['xp', xpPrize], ['coins', coinPrize]]) {
    if (amount == null) continue;
    const err = rewardUnavailableMessage(kind, settings);
    if (err) return fail(interaction, 'Prize Unavailable', err);
  }

  // 9. Ping role (00-overview rule 23: checked for the bot and for the executor).
  const pingRole = options.getRole('ping_role');
  if (pingRole) {
    if (pingRole.id === guild.id) return fail(interaction, 'Invalid Role', "I never ping @everyone. Choose a role for `ping_role`.");
    if (pingableRoleIds(channel, guild.members.me, [pingRole.id]).refused.length) {
      return fail(interaction, 'Cannot Ping Role', `I can't ping ${pingRole} in ${channel}. Make the role mentionable, or give me **Mention @everyone, @here and All Roles** there.`);
    }
    if (pingableRoleIds(channel, interaction.member, [pingRole.id]).refused.length) {
      return fail(interaction, 'Cannot Ping Role', `You don't have permission to ping ${pingRole}.`);
    }
  }

  // 10. Colour.
  const colorStr = options.getString('color');
  if (colorStr && parseColor(colorStr) === null) {
    return fail(interaction, 'Invalid Colour', 'Use a hex colour like `#5865F2` or a name like `blurple`.');
  }
  const embedColor = colorStr ? parseColor(colorStr) : null;

  // 11. Text.
  const prize = options.getString('prize', true).trim();
  if (!prize) return fail(interaction, 'Invalid Prize', 'The prize cannot be empty.');
  const description = options.getString('description')?.trim() || null;

  // 12-13. Start and reply.
  const res = await startGiveaway({
    client, guild, channel, hostId: interaction.user.id, prize, description, winnerCount, endsAt, requiredRoleIds, deniedRoleIds,
    roleWeights, xpPrize, coinPrize, pingRoleId: pingRole?.id ?? null, embedColor, settings,
  });
  if (!res.ok) return failStartOrRestart(interaction, res, channel);
  return interaction.editReply({
    embeds: [successEmbed(
      'Giveaway Started',
      `**${clampText(prize, 200)}** is live in ${channel} and ends ${discordTimestamp(endsAt, 'R')}.\n[Jump to the giveaway](${res.message.url}) · ID \`${res.giveaway.id}\``,
    )],
  });
}

async function end(interaction, client) {
  const g = await resolveRef(interaction);
  if (!g) return undefined;
  if (g.status !== 'ACTIVE') {
    if (g.status === 'DRAFT') {
      return fail(interaction, 'Not Running', 'That giveaway is a draft (its message was deleted). Use `/giveaway restart` to republish it first.');
    }
    return fail(interaction, 'Not Running', `That giveaway is ${STATUS_TEXT[g.status]}.`);
  }
  const res = await endGiveaway({ client, guild: interaction.guild, giveawayId: g.id, source: 'command' });
  const prize = clampText(g.prize, 200);
  if (res.ok) {
    let text = res.winnerIds.length ? `Winners of **${prize}**: ${mentionsOf(res.winnerIds)}` : `**${prize}** ended with no valid entrants.`;
    if (res.payWarnings.length) text += `\n\n${res.payWarnings.join('\n')}`;
    if (res.announced === false) text += '\n\nI could not post the announcement in the giveaway channel.';
    return interaction.editReply({ embeds: [successEmbed('Giveaway Ended', text)] });
  }
  switch (res.code) {
    case 'NOT_ACTIVE':
    case 'NOT_FOUND':
      return interaction.editReply({ embeds: [warnEmbed('Already Ended', 'That giveaway was already ended.')] });
    case 'DRAFTED':
      return interaction.editReply({
        embeds: [warnEmbed('Message Missing', 'The giveaway message or its channel was deleted, so the giveaway was moved to drafts. Use `/giveaway restart` to republish it.')],
      });
    case 'NO_ACCESS':
      return fail(
        interaction,
        'Missing Permissions',
        res.missing?.length
          ? `I need **${permissionNames(res.missing).join(', ')}** in <#${g.channelId}> to end this giveaway.`
          : (res.reason ?? `I can't use <#${g.channelId}> to end this giveaway.`),
      );
    case 'RESOLVE_FAILED':
      return fail(interaction, 'Try Again', "I couldn't check every entrant right now (Discord did not answer in time). Please try again in a minute.");
    default:
      return fail(interaction, 'Something Went Wrong', GENERIC_ERROR);
  }
}

async function reroll(interaction, client) {
  const g = await resolveRef(interaction);
  if (!g) return undefined;
  if (g.status !== 'ENDED') {
    if (g.status === 'ACTIVE') return fail(interaction, 'Still Running', 'That giveaway is still running. Use `/giveaway end` to end it now.');
    return fail(interaction, 'Cannot Reroll', 'Only ended giveaways can be rerolled.');
  }
  const count = interaction.options.getInteger('winners') ?? 1;
  const res = await rerollGiveaway({ client, guild: interaction.guild, giveawayId: g.id, count });
  if (res.ok) {
    let text = `New winner${res.newWinnerIds.length === 1 ? '' : 's'} of **${clampText(g.prize, 200)}**: ${mentionsOf(res.newWinnerIds)}`;
    if (g.xpPrize || g.coinPrize) text += '\nThey receive the prize too; earlier winners keep theirs.';
    if (res.payWarnings.length) text += `\n\n${res.payWarnings.join('\n')}`;
    if (res.announced === false) text += '\n\nI could not post the announcement in the giveaway channel.';
    return interaction.editReply({ embeds: [successEmbed('Giveaway Rerolled', text)] });
  }
  switch (res.code) {
    case 'NO_ENTRANTS':
      return interaction.editReply({ embeds: [warnEmbed('No One Left', 'There are no other eligible entrants to draw from.')] });
    case 'NOT_ENDED':
    case 'NOT_FOUND':
      return fail(interaction, 'Cannot Reroll', 'Only ended giveaways can be rerolled.');
    case 'CONFLICT':
      return fail(interaction, 'Try Again', 'That giveaway changed while I was drawing. Please try again.');
    case 'RESOLVE_FAILED':
      return fail(interaction, 'Try Again', "I couldn't check every entrant right now (Discord did not answer in time). Please try again in a minute.");
    default:
      return fail(interaction, 'Something Went Wrong', GENERIC_ERROR);
  }
}

async function cancel(interaction) {
  const g = await resolveRef(interaction);
  if (!g) return undefined;
  if (!['ACTIVE', 'DRAFT'].includes(g.status)) return fail(interaction, 'Cannot Cancel', `That giveaway is ${STATUS_TEXT[g.status]}.`);
  const prize = clampText(g.prize, 200);
  const ok = await awaitConfirm(interaction, {
    title: 'Cancel this giveaway?',
    description: `**${prize}** will be cancelled and no winners will be drawn. This cannot be undone.`,
    confirmLabel: 'Cancel giveaway',
    cancelLabel: 'Keep it',
    danger: true,
    ephemeral: true,
  });
  if (!ok) return interaction.editReply({ embeds: [infoEmbed('Kept', 'The giveaway was not cancelled.')], components: [] });
  const res = await cancelGiveaway({ guild: interaction.guild, giveawayId: g.id });
  if (res.ok) return interaction.editReply({ embeds: [successEmbed('Giveaway Cancelled', `**${prize}** was cancelled.`)], components: [] });
  return fail(interaction, 'Cannot Cancel', 'That giveaway already ended or was cancelled.', { components: [] });
}

async function restart(interaction, client, settings) {
  const { guild, options } = interaction;
  const g = await resolveRef(interaction);
  if (!g) return undefined;
  if (g.status !== 'DRAFT') {
    return fail(interaction, 'Not a Draft', 'Only draft giveaways can be restarted. A giveaway becomes a draft when its message is deleted.');
  }

  const channel = optionChannel(interaction) ?? await fetchChannel(guild, g.channelId);
  if (!channel) return fail(interaction, 'Channel Missing', 'The original channel no longer exists. Pick one with `channel`.');
  if (!(await checkPostChannel(interaction, channel))) return undefined;

  let endsAt;
  const durationText = options.getString('duration');
  if (durationText) {
    endsAt = await parseEndsAt(interaction, settings, durationText);
    if (!endsAt) return undefined;
  } else if (g.endsAt.getTime() - Date.now() >= MIN_DURATION_MS) {
    endsAt = g.endsAt;
  } else {
    return fail(interaction, 'Duration Needed', "This giveaway's end time has passed. Give a new `duration`.");
  }

  const res = await restartGiveaway({ client, guild, giveawayId: g.id, channel, endsAt, settings });
  if (res.ok) {
    return interaction.editReply({
      embeds: [successEmbed(
        'Giveaway Restarted',
        `**${clampText(g.prize, 200)}** is live again in ${channel} and ends ${discordTimestamp(endsAt, 'R')}. Existing entries were kept.\n[Jump to the giveaway](${res.message.url})`,
      )],
    });
  }
  if (res.code === 'NOT_DRAFT' || res.code === 'CONFLICT') {
    return fail(interaction, 'Try Again', 'That giveaway changed meanwhile. Check `/giveaway list` and try again.');
  }
  return failStartOrRestart(interaction, res, channel);
}

const LIST_FILTERS = {
  open: (guildId) => ({ guildId, status: { in: ['ACTIVE', 'DRAFT'] } }),
  ended: (guildId) => ({ guildId, status: 'ENDED' }),
  cancelled: (guildId) => ({ guildId, status: 'CANCELLED' }),
  all: (guildId) => ({ guildId }),
};

async function list(interaction, client, settings) {
  const status = interaction.options.getString('status') ?? 'open';
  const where = (LIST_FILTERS[status] ?? LIST_FILTERS.open)(interaction.guildId);
  const rows = await prisma.giveaway.findMany({
    where,
    orderBy: { createdAt: 'desc' },
    take: LIST_SHOWN,
    select: { id: true, prize: true, status: true, channelId: true, endsAt: true, endedAt: true, _count: { select: { entries: true } } },
  });
  if (!rows.length) {
    return interaction.editReply({ embeds: [infoEmbed('No Giveaways', 'There are no giveaways to show. Start one with `/giveaway start`.')] });
  }
  const lines = rows.map(formatListLine);
  const pages = [];
  for (let i = 0; i < lines.length; i += 10) {
    let text = lines.slice(i, i + 10).join('\n\n');
    if (i === 0 && rows.length === LIST_SHOWN) text = `_Showing the ${LIST_SHOWN} most recent._\n\n${text}`;
    pages.push(guildEmbed(settings, '🎉 Giveaways', text));
  }
  return paginate(interaction, pages, { ephemeral: true });
}

async function entrants(interaction, client, settings) {
  const g = await resolveRef(interaction);
  if (!g) return undefined;
  const total = await prisma.giveawayEntry.count({ where: { giveawayId: g.id } });
  const rows = await prisma.giveawayEntry.findMany({
    where: { giveawayId: g.id }, orderBy: { createdAt: 'asc' }, take: ENTRANTS_SHOWN, select: { userId: true, createdAt: true },
  });
  const prize = clampText(g.prize, 200);
  if (!rows.length) return interaction.editReply({ embeds: [infoEmbed('No Entrants', `Nobody has entered **${prize}** yet.`)] });

  const weights = normalizeRoleWeights(g.roleWeights);
  let header = `**Status:** ${STATUS_TEXT_SHORT[g.status]}`;
  if (weights.length) header += ` · **Bonus odds:** ${weights.map((w) => `<@&${w.roleId}> ×${w.weight}`).join(', ')}`;
  const footer = `${formatNumber(total)} entrant${total === 1 ? '' : 's'}${total > ENTRANTS_SHOWN ? ' · showing the first 1,000' : ''}`;
  const lines = rows.map((r, i) => `${i + 1}. <@${r.userId}> · ${discordTimestamp(r.createdAt, 'R')}${g.winnerIds.includes(r.userId) ? ' 🏆' : ''}`);
  const pages = [];
  for (let i = 0; i < lines.length; i += 20) {
    pages.push(guildEmbed(settings, `🎉 Entrants: ${prize}`, `${header}\n\n${lines.slice(i, i + 20).join('\n')}`, footer));
  }
  return paginate(interaction, pages, { ephemeral: true });
}

async function oddsAdd(interaction, client, settings) {
  const g = await resolveRef(interaction);
  if (!g) return undefined;
  if (!['ACTIVE', 'DRAFT'].includes(g.status)) return fail(interaction, 'Not Open', 'Odds can only be changed while a giveaway is running or in drafts.');
  const role = interaction.options.getRole('role', true);
  if (role.id === interaction.guild.id) return fail(interaction, 'Invalid Role', 'Choose a specific role, not @everyone.');
  const weight = interaction.options.getInteger('weight', true);
  const res = await updateOdds({ guild: interaction.guild, giveawayId: g.id, op: 'add', roleId: role.id, weight, settings });
  if (res.ok) {
    const list = res.roleWeights.map((w) => `<@&${w.roleId}> ×${w.weight}`).join(', ');
    return interaction.editReply({
      embeds: [successEmbed('Odds Updated', `Members with ${role} now count as **${weight}** entries in **${clampText(g.prize, 200)}**.\n\n**Bonus odds:** ${list}`)],
    });
  }
  if (res.code === 'LIMIT') return fail(interaction, 'Too Many Roles', `A giveaway can have at most ${MAX_ROLE_WEIGHTS} bonus-odds roles.`);
  if (res.code === 'NOT_OPEN') return fail(interaction, 'Not Open', 'Odds can only be changed while a giveaway is running or in drafts.');
  return fail(interaction, 'Giveaway Not Found', NOT_FOUND_TEXT);
}

async function oddsRemove(interaction, client, settings) {
  const g = await resolveRef(interaction);
  if (!g) return undefined;
  if (!['ACTIVE', 'DRAFT'].includes(g.status)) return fail(interaction, 'Not Open', 'Odds can only be changed while a giveaway is running or in drafts.');
  const role = interaction.options.getRole('role', true);
  if (role.id === interaction.guild.id) return fail(interaction, 'Invalid Role', 'Choose a specific role, not @everyone.');
  const res = await updateOdds({ guild: interaction.guild, giveawayId: g.id, op: 'remove', roleId: role.id, settings });
  if (res.ok) {
    return interaction.editReply({ embeds: [successEmbed('Odds Updated', `${role} no longer has bonus odds in **${clampText(g.prize, 200)}**.`)] });
  }
  if (res.code === 'NOT_SET') return fail(interaction, 'Not Set', `${role} has no bonus odds in this giveaway.`);
  if (res.code === 'NOT_OPEN') return fail(interaction, 'Not Open', 'Odds can only be changed while a giveaway is running or in drafts.');
  return fail(interaction, 'Giveaway Not Found', NOT_FOUND_TEXT);
}

const HANDLERS = { start, end, reroll, cancel, restart, list, entrants, 'odds add': oddsAdd, 'odds remove': oddsRemove };

// ── Execute ───────────────────────────────────────────────────────────────────

export async function execute(interaction, client) {
  // Every reply is ephemeral; acknowledge before anything else (no lookup precedes the defer).
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  const key = group ? `${group} ${sub}` : sub;
  try {
    const handler = HANDLERS[key];
    if (!handler) {
      await fail(interaction, 'Unknown Subcommand', 'This subcommand no longer exists.');
      return;
    }
    const settings = await getSettings(interaction.guildId);
    await handler(interaction, client, settings);
  } catch (err) {
    logger.error(`giveaway ${key} failed`, err);
    await fail(interaction, 'Something Went Wrong', GENERIC_ERROR, { components: [] });
  }
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

const AUTOCOMPLETE_STATUSES = {
  end: ['ACTIVE'],
  reroll: ['ENDED'],
  cancel: ['ACTIVE', 'DRAFT'],
  restart: ['DRAFT'],
  entrants: ['ACTIVE', 'DRAFT', 'ENDED', 'CANCELLED'],
  'odds add': ['ACTIVE', 'DRAFT'],
  'odds remove': ['ACTIVE', 'DRAFT'],
};

export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    if (focused.name !== 'giveaway') return respondAutocomplete(interaction, []);
    const group = interaction.options.getSubcommandGroup(false);
    const sub = interaction.options.getSubcommand(false);
    const statuses = AUTOCOMPLETE_STATUSES[group ? `${group} ${sub}` : sub];
    if (!statuses || !interaction.guildId) return respondAutocomplete(interaction, []);
    const budget = ackBudgetMs(interaction);
    if (budget <= 0) return respondAutocomplete(interaction, []);

    const q = String(focused.value ?? '').trim().slice(0, 100);
    const rows = await withTimeout(prisma.giveaway.findMany({
      where: {
        guildId: interaction.guildId,
        status: { in: statuses },
        ...(q ? { OR: [{ prize: { contains: q, mode: 'insensitive' } }, { id: { startsWith: q } }, { messageId: q }] } : {}),
      },
      orderBy: { createdAt: 'desc' },
      take: 25,
      select: { id: true, prize: true, status: true, channelId: true, endsAt: true },
    }), budget, 'giveaway autocomplete');
    const channels = interaction.guild?.channels.cache;
    return respondAutocomplete(interaction, rows.map((r) => {
      let tail = 'cancelled';
      if (r.status === 'ACTIVE') tail = `ends in ${formatDuration(Math.max(1000, r.endsAt.getTime() - Date.now()))}`;
      else if (r.status === 'DRAFT') tail = 'draft';
      else if (r.status === 'ENDED') tail = 'ended';
      return {
        name: clampText(`${STATUS_EMOJI[r.status]} ${r.prize} · #${channels?.get(r.channelId)?.name ?? 'deleted-channel'} · ${tail}`, 100),
        value: r.id,
      };
    }));
  } catch (err) {
    logger.debug(`giveaway autocomplete failed: ${err?.message}`);
    return respondAutocomplete(interaction, []);
  }
}
