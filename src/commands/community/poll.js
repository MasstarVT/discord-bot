// /poll (pkg-polls): MEE6 Polls (/poll, /poll-end). This file is thin: option parsing, validation and replies. Everything else
// (the form, voting, live results, ending) lives in services/pollService.js. `create` opens a modal and does no I/O before
// showModal; the poll is posted when the form is submitted (poll-modal handler in the plugin).
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import prisma from '../../database/client.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import {
  buildCreateModal, buildResultsEmbed, channelProblem, endPoll, findPollsForAutocomplete, getTally, listActivePolls,
  parseDurationOption, pendingPolls, pollListLines,
} from '../../services/pollService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone, respondAutocomplete, safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { formatDuration } from '../../utils/parseDuration.js';
import { chunkLines, clampText } from '../../utils/text.js';

export const module = 'polls';
export const gated = true;
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;
// No allowInDMs export: buildCommandPayload registers the command for servers only.

// ── Builder ───────────────────────────────────────────────────────────────────

export const data = new SlashCommandBuilder()
  .setName('poll')
  .setDescription('Create and manage polls (MEE6 /poll, /poll-end).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addSubcommand((s) => s.setName('create')
    .setDescription('Create a poll with 2–20 options; a form opens for the question and options.')
    .addStringOption((o) => o.setName('duration')
      .setDescription('How long it runs, e.g. 30m, 2h, 3d (max 30d). Empty = until /poll end').setMaxLength(32))
    .addIntegerOption((o) => o.setName('max_choices')
      .setDescription('Options each member may pick (default 1)').setMinValue(1).setMaxValue(LIMITS.POLL_OPTIONS))
    .addRoleOption((o) => o.setName('allowed_role').setDescription('Only members with this role can vote'))
    .addRoleOption((o) => o.setName('denied_role').setDescription('Members with this role cannot vote'))
    .addBooleanOption((o) => o.setName('hide_results').setDescription('Hide results until the poll ends (default: live)'))
    .addChannelOption((o) => o.setName('channel').setDescription('Where to post it (default: this channel)')
      .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildVoice,
        ChannelType.PublicThread, ChannelType.PrivateThread, ChannelType.AnnouncementThread)))
  .addSubcommand((s) => s.setName('end')
    .setDescription('End a poll in this channel and post the results (MEE6 /poll-end).')
    .addStringOption((o) => o.setName('poll').setDescription('Active poll in this channel').setRequired(true).setAutocomplete(true)))
  .addSubcommand((s) => s.setName('results')
    .setDescription("Show a poll's results privately, even while hidden.")
    .addStringOption((o) => o.setName('poll').setDescription('Poll on this server').setRequired(true).setAutocomplete(true)))
  .addSubcommand((s) => s.setName('list').setDescription('List the active polls on this server.'));

// ── Messages (pkg-polls.md §4.8) ──────────────────────────────────────────────

const DURATION_ERRORS = {
  invalid: 'Use a duration like `30m`, `2h`, `1d` or `1w`.',
  'too-short': 'Polls must run for at least 1 minute.',
  'too-long': 'Polls can run for at most 30 days.',
};
const notFound = () => errorEmbed('Poll Not Found', 'Pick a poll from the list.');

// ── create ────────────────────────────────────────────────────────────────────

/** Every check here is synchronous: showModal must be the first response and the router already spent part of the 3 s. */
async function create(interaction) {
  const { guild, options } = interaction;
  const deny = (embed) => safeReply(interaction, { embeds: [embed], ephemeral: true });

  const d = parseDurationOption(options.getString('duration'));
  if (d.error) return deny(errorEmbed('Invalid Duration', DURATION_ERRORS[d.error]));

  const maxChoices = options.getInteger('max_choices') ?? 1;
  const allowed = options.getRole('allowed_role');
  const denied = options.getRole('denied_role');
  const hideResults = options.getBoolean('hide_results') ?? false;
  if (denied?.id === interaction.guildId) return deny(errorEmbed('Invalid Roles', "You can't deny @everyone, or nobody could vote."));
  if (allowed && denied && allowed.id === denied.id) {
    return deny(errorEmbed('Invalid Roles', 'The allowed role and the denied role must be different.'));
  }
  const allowedRoleIds = allowed && allowed.id !== interaction.guildId ? [allowed.id] : []; // @everyone = no restriction

  // An option-resolved channel may be a raw API object (no permissionsFor): prefer the cached one.
  const picked = options.getChannel('channel');
  const channel = picked
    ? (guild.channels.cache.get(picked.id) ?? picked)
    : (interaction.channel ?? guild.channels.cache.get(interaction.channelId) ?? null);
  const problem = channelProblem(channel, interaction.member);
  if (problem) return deny(problem);

  pendingPolls.put(interaction.id, {
    guildId: interaction.guildId,
    channelId: channel.id,
    userId: interaction.user.id,
    maxChoices,
    allowedRoleIds,
    deniedRoleIds: denied ? [denied.id] : [],
    hideResults,
    durationMs: d.ms,
  });
  try {
    await interaction.showModal(buildCreateModal(interaction.id));
  } catch (err) {
    pendingPolls.take(interaction.id);
    if (!isInteractionGone(err)) logger.error('poll create: showModal failed', err);
  }
}

// ── end / results / list ──────────────────────────────────────────────────────

async function end(interaction, client) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed] });
  const { guildId } = interaction;
  const poll = await prisma.poll.findFirst({ where: { id: interaction.options.getString('poll', true), guildId } });
  if (!poll) return reply(notFound());
  const r = await endPoll(client, poll.id, { reason: 'manual', guildId });
  if (!r.ok) {
    return reply(r.code === 'ALREADY_ENDED' ? warnEmbed('Already Ended', 'That poll has already ended.') : notFound());
  }
  return reply(successEmbed('Poll Ended', r.posted
    ? `The results were posted in <#${poll.channelId}>.`
    : `The poll is closed, but I couldn't post the results in <#${poll.channelId}>.`));
}

async function results(interaction, client) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed] });
  const { guildId } = interaction;
  const id = interaction.options.getString('poll', true);
  let poll = await prisma.poll.findFirst({ where: { id, guildId } });
  if (!poll) return reply(notFound());
  // Self-heal: the end job is late or missing.
  if (poll.status === 'ACTIVE' && poll.endsAt && poll.endsAt.getTime() < Date.now() - 60_000) {
    await endPoll(client, poll.id, { reason: 'expired', guildId });
    poll = (await prisma.poll.findFirst({ where: { id, guildId } })) ?? poll;
  }
  const [tally, settings] = await Promise.all([getTally(poll), getSettingsSafe(guildId)]);
  return reply(buildResultsEmbed(poll, tally, { settings, live: poll.status === 'ACTIVE', messageExists: true }));
}

async function list(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const { guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  const { polls, votes } = await listActivePolls(guildId);
  if (!polls.length) {
    return safeReply(interaction, { embeds: [infoEmbed('No Active Polls', 'There are no active polls. Start one with `/poll create`.')] });
  }
  const title = `📊 Active polls (${polls.length}/${LIMITS.ACTIVE_POLLS})`;
  const pages = chunkLines(pollListLines(polls, votes), 4000).map((c) => guildEmbed(settings, title, c));
  await paginate(interaction, pages, { ephemeral: true });
}

// ── execute / autocomplete ────────────────────────────────────────────────────

export async function execute(interaction, client) {
  switch (interaction.options.getSubcommand()) {
    case 'create': return create(interaction);
    case 'end': return end(interaction, client);
    case 'results': return results(interaction, client);
    case 'list': return list(interaction);
    default: return undefined;
  }
}

/** Read-only: nothing is ended, edited or written. Answers within the 3 s window or with no choices. */
export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    const sub = interaction.options.getSubcommand(false);
    if (focused.name !== 'poll' || (sub !== 'end' && sub !== 'results')) return await respondAutocomplete(interaction, []);
    const q = String(focused.value ?? '').trim();
    const rows = await withTimeout(
      findPollsForAutocomplete({ guildId: interaction.guildId, channelId: interaction.channelId, sub, q }),
      ackBudgetMs(interaction),
      'poll autocomplete',
    );
    const channels = interaction.guild?.channels?.cache;
    const choices = rows.map((p) => ({
      value: p.id,
      name: sub === 'end'
        ? `${clampText(p.question, 75)} · ${formatDuration(Math.max(1000, Date.now() - p.createdAt.getTime())).split(' ')[0]} ago`
        : `${p.status === 'ACTIVE' ? '🟢' : '🏁'} ${clampText(p.question, 60)} · #${channels?.get(p.channelId)?.name ?? 'deleted-channel'}`,
    }));
    return await respondAutocomplete(interaction, choices);
  } catch (err) {
    logger.warn('poll autocomplete failed', err);
    return respondAutocomplete(interaction, []);
  }
}
