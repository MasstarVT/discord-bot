// /feed (pkg-feeds): RSS, YouTube, Reddit, Bluesky and podcast alerts (MEE6 social alerts). Thin: parsing, fetching,
// message building, the poll job and the DB helpers live in services/feedService.js and services/feedParsers.js.
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import prisma from '../../database/client.js';
import { LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { cancelJobs, getJob } from '../../services/scheduler.js';
import { getSettingsSafe, isModuleEnabled, peekSettings } from '../../services/settingsService.js';
import {
  BASE_INTERVAL_MS,
  FEED_CHANNEL_TYPES,
  FeedUserError,
  TYPE_EMOJI,
  TYPE_LABELS,
  buildPostPayload,
  createFeedRow,
  describeFetchError,
  feedJobKey,
  feedStatus,
  fetchFeedSource,
  findFeedForGuild,
  parseMessageOption,
  resolveFeedSource,
  scheduleFeedJob,
  sourceLink,
  templateWarnings,
  varHelp,
} from '../../services/feedService.js';
import {
  normalizeFeedUrl,
  parseBlueskyActorInput,
  parseSubredditInput,
  parseYouTubeChannelInput,
} from '../../services/feedParsers.js';
import { TimeoutError, ackBudgetMs, createThrottle, mapLimit, withTimeout } from '../../utils/async.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import {
  checkChannelUsable,
  isInteractionGone,
  pingableRoleIds,
  replyError,
  respondAutocomplete,
} from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { formatDurationLong } from '../../utils/parseDuration.js';
import { chunkLines, clampText } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';

export const module = 'feeds';
export const gated = false; // manageable while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN; // every subcommand; no subcommandPermissions
// No allowInDMs export: buildCommandPayload registers the command for servers only.

// ── Builder ───────────────────────────────────────────────────────────────────
// The payload budget is tight (about 2050 of 2400 characters): do not lengthen any description.

const FEED_CHANNEL_TYPES_OPT = [ChannelType.GuildText, ChannelType.GuildAnnouncement];
const D = {
  channel: 'Channel to post new items in',
  ping_role: 'Role to ping with each new item',
  message: 'Custom text with {title} {link} etc.; \\n = new line',
  feed: 'Feed to use (pick from the list)',
};
function common(s, { reposts = false } = {}) {
  s.addChannelOption((o) => o.setName('channel').setDescription(D.channel).setRequired(true)
    .addChannelTypes(...FEED_CHANNEL_TYPES_OPT));
  s.addRoleOption((o) => o.setName('ping_role').setDescription(D.ping_role));
  s.addStringOption((o) => o.setName('message').setDescription(D.message).setMaxLength(1000));
  if (reposts) s.addBooleanOption((o) => o.setName('include_reposts').setDescription('Also post reposts (default: off)'));
  return s;
}
const source = (name, desc, max) => (o) => o.setName(name).setDescription(desc).setRequired(true).setMaxLength(max);
const feedOpt = (o) => o.setName('feed').setDescription(D.feed).setRequired(true).setAutocomplete(true).setMaxLength(100);

export const data = new SlashCommandBuilder()
  .setName('feed')
  .setDescription('RSS, YouTube, Reddit, Bluesky and podcast alerts (MEE6 social alerts)')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommandGroup((g) => g.setName('add').setDescription('Follow a new feed')
    .addSubcommand((s) => common(s.setName('rss').setDescription('An RSS or Atom feed (MEE6 RSS Feeds)')
      .addStringOption(source('url', 'Feed URL, or a site that links to its feed', 500))))
    .addSubcommand((s) => common(s.setName('youtube').setDescription('New uploads from a YouTube channel (MEE6 YouTube Alerts)')
      .addStringOption(source('youtube_channel', 'UC… channel ID or youtube.com/channel/UC… link (@handles not supported)', 200))))
    .addSubcommand((s) => common(s.setName('reddit').setDescription('New posts in a subreddit (MEE6 Reddit Alerts, best-effort)')
      .addStringOption(source('subreddit', 'Subreddit, e.g. r/aww', 100))))
    .addSubcommand((s) => common(s.setName('bluesky').setDescription('New posts from a Bluesky account (MEE6 Bluesky Alerts)')
      .addStringOption(source('handle', 'Handle (alice.bsky.social), profile link or DID', 300)), { reposts: true }))
    .addSubcommand((s) => common(s.setName('podcast').setDescription('New episodes of a podcast (MEE6 Podcast Alerts)')
      .addStringOption(source('podcast', 'Podcast name to search, or its RSS feed URL', 300)))))
  .addSubcommand((s) => s.setName('list').setDescription("List this server's feeds and their status"))
  .addSubcommand((s) => s.setName('remove').setDescription('Stop following a feed').addStringOption(feedOpt))
  .addSubcommand((s) => s.setName('test').setDescription('Fetch a feed now and preview its newest item (changes nothing)').addStringOption(feedOpt))
  .addSubcommand((s) => s.setName('edit').setDescription('Change a feed: channel, ping role, message, reposts')
    .addStringOption(feedOpt)
    .addChannelOption((o) => o.setName('channel').setDescription('New channel').addChannelTypes(...FEED_CHANNEL_TYPES_OPT))
    .addRoleOption((o) => o.setName('ping_role').setDescription(D.ping_role))
    .addBooleanOption((o) => o.setName('clear_ping_role').setDescription('Stop pinging a role'))
    .addStringOption((o) => o.setName('message').setDescription(D.message).setMaxLength(1000))
    .addBooleanOption((o) => o.setName('reset_message').setDescription('Go back to the default message'))
    .addBooleanOption((o) => o.setName('include_reposts').setDescription('Bluesky only: also post reposts')))
  .addSubcommand((s) => s.setName('enable').setDescription('Turn a feed back on and reset its failures').addStringOption(feedOpt))
  .addSubcommand((s) => s.setName('disable').setDescription('Pause a feed (its settings are kept)').addStringOption(feedOpt));

// ── Shared helpers ────────────────────────────────────────────────────────────

const UNAVAILABLE_TITLE = 'Temporarily Unavailable';
const NEEDS = ['ViewChannel', 'SendMessages', 'EmbedLinks'];
const NO_MENTIONS = Object.freeze({ parse: [] });
const MODULE_OFF_NOTE = '⚠️ The **Social Feeds** module is off, so nothing is posted until an admin runs `/module enable feeds`.';
const moduleOffNote = (settings) => (isModuleEnabled(settings, 'feeds') ? null : MODULE_OFF_NOTE);
const testThrottle = createThrottle(15_000);

/** Loads the settings before the acknowledgement: the memo first, a cold load bounded by the ack budget (rule 21). */
async function loadSettings(interaction) {
  let settings = peekSettings(interaction.guildId);
  if (!settings) {
    settings = await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'feed settings').catch(() => null);
  }
  if (!settings) {
    await replyError(interaction, UNAVAILABLE_TITLE, 'Server settings could not be loaded. Please try again in a moment.');
    return null;
  }
  return settings;
}

/** The feed named by the `feed` option, or null after replying with the reason. Bounded by the ack budget. */
async function lookupFeed(interaction) {
  const raw = interaction.options.getString('feed', true);
  let feed;
  try {
    feed = await withTimeout(findFeedForGuild(interaction.guildId, raw), ackBudgetMs(interaction), 'feed lookup');
  } catch (err) {
    if (err instanceof TimeoutError) {
      await replyError(interaction, UNAVAILABLE_TITLE, 'That took too long. Please try again.');
      return null;
    }
    throw err;
  }
  if (!feed) {
    await replyError(interaction, 'Feed Not Found', `No feed matches **${clampText(raw, 60)}**. Pick one from the list, or see \`/feed list\`.`);
    return null;
  }
  return feed;
}

/** The target channel, checked for the permissions a post needs. Returns `{ channel }` or `{ error: [title, text] }`. */
function checkTargetChannel(interaction, option) {
  const channel = interaction.guild.channels.cache.get(option.id) ?? null;
  if (channel && !FEED_CHANNEL_TYPES.includes(channel.type)) {
    return { error: ['Invalid Channel', 'Feeds can only post in text and announcement channels.'] };
  }
  const u = checkChannelUsable(channel, { needs: NEEDS });
  if (!u.ok) return { error: ['Missing Permissions', u.reason ?? "I can't post in that channel."] };
  return { channel };
}

/** Bot-side ping check: a warning (not an error) when the bot cannot ping the role in that channel. */
function botPingWarning(guild, channel, role) {
  if (!role || role.mentionable) return null;
  const me = guild.members?.me;
  if (me && channel?.permissionsFor(me)?.has(PermissionFlagsBits.MentionEveryone)) return null;
  return `⚠️ <@&${role.id}> is not mentionable and I lack **Mention @everyone, @here and All Roles** in <#${channel?.id}>, so it won't actually ping.`;
}

/** The ping role, checked for the member (rule 23). Returns `{ warning? }` or `{ error: [title, text] }`. */
function checkPingRole(interaction, role, channel) {
  const guild = interaction.guild;
  if (role.id === guild.id) return { error: ['Invalid Role', "@everyone can't be used as a ping role. Use a role members can opt into."] };
  const { refused } = pingableRoleIds(channel, interaction.member, [role.id]);
  if (refused.length) {
    return { error: ['Role Not Pingable', `<@&${role.id}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#${channel.id}>.`] };
  }
  return { warning: botPingWarning(guild, channel, guild.roles.cache.get(role.id) ?? role) };
}

/** Awaits a DB read before the acknowledgement; null when it did not finish within the ack budget. */
async function withinAckBudget(interaction, promise, label) {
  try {
    return await withTimeout(promise, ackBudgetMs(interaction), label);
  } catch (err) {
    if (err instanceof TimeoutError) return null;
    throw err;
  }
}

const editError = (interaction, title, text) => interaction.editReply({ embeds: [errorEmbed(title, text)], allowedMentions: NO_MENTIONS });

async function handleError(interaction, err, group, sub) {
  if (isInteractionGone(err)) return;
  const user = err instanceof FeedUserError;
  if (!user) logger.error(`feed ${group ?? ''} ${sub} failed`, err);
  const title = user ? err.title : 'Something went wrong';
  const text = user ? err.message : 'I could not complete that. Please try again.';
  try {
    if (interaction.deferred || interaction.replied) await editError(interaction, title, text);
    else await replyError(interaction, title, text);
  } catch (e) {
    if (!isInteractionGone(e)) logger.warn(`feed ${sub}: could not report the error: ${e?.message}`);
  }
}

/** Lines 1-3 of the add / edit confirmation. */
function feedSummaryLines(feed, { interval = true } = {}) {
  const lines = [`${TYPE_EMOJI[feed.type]} **${escapeMarkdown(feed.displayName)}** → <#${feed.channelId}>`];
  if (interval) lines.push(`${TYPE_LABELS[feed.type]} · checked every ${formatDurationLong(BASE_INTERVAL_MS[feed.type])} · ${sourceLink(feed)}`);
  lines.push(feed.pingRoleId ? `Ping: <@&${feed.pingRoleId}>` : 'Ping: none');
  return lines;
}

// ── add ───────────────────────────────────────────────────────────────────────

const PODCAST_URL_RE = /^(https?:\/\/|feed:)|^[\w-]+(\.[\w-]+)+\/\S*/i;

/** Parses the source option of an `add` subcommand: `{ type, input }` or `{ error: [title, text] }`. */
function readAddSource(sub, options) {
  switch (sub) {
    case 'rss': {
      const r = normalizeFeedUrl(options.getString('url', true));
      return r.ok ? { type: 'RSS', input: { url: r.url } } : { error: ['Invalid URL', r.reason] };
    }
    case 'youtube': {
      const r = parseYouTubeChannelInput(options.getString('youtube_channel', true));
      if (r.ok) return { type: 'YOUTUBE', input: { channelId: r.channelId } };
      return { error: [r.handle ? 'Channel ID Needed' : 'Invalid Channel', r.reason] };
    }
    case 'reddit': {
      const r = parseSubredditInput(options.getString('subreddit', true));
      return r.ok ? { type: 'REDDIT', input: { name: r.name } } : { error: ['Invalid Subreddit', r.reason] };
    }
    case 'bluesky': {
      const r = parseBlueskyActorInput(options.getString('handle', true));
      return r.ok ? { type: 'BLUESKY', input: { actor: r.actor } } : { error: ['Invalid Handle', r.reason] };
    }
    case 'podcast': {
      const raw = options.getString('podcast', true).trim();
      if (PODCAST_URL_RE.test(raw)) {
        const r = normalizeFeedUrl(raw);
        return r.ok ? { type: 'PODCAST', input: { url: r.url } } : { error: ['Invalid URL', r.reason] };
      }
      if (raw.length < 2) return { error: ['Invalid Search', "Type at least 2 characters of the podcast's name, or paste its RSS feed URL."] };
      return { type: 'PODCAST', input: { query: raw } };
    }
    default:
      return { error: ['Unknown Feed Type', 'That kind of feed is not supported.'] };
  }
}

async function handleAdd(interaction, sub, settings) {
  const { guildId, options } = interaction;

  // Step A: cheap checks, before any defer (each failure is ephemeral).
  const src = readAddSource(sub, options);
  if (src.error) return replyError(interaction, src.error[0], src.error[1]);
  const { type, input } = src;

  const target = checkTargetChannel(interaction, options.getChannel('channel', true));
  if (target.error) return replyError(interaction, target.error[0], target.error[1]);
  const { channel } = target;

  const warnings = [];
  const role = options.getRole('ping_role');
  if (role) {
    const check = checkPingRole(interaction, role, channel);
    if (check.error) return replyError(interaction, check.error[0], check.error[1]);
    if (check.warning) warnings.push(check.warning);
  }

  const parsed = parseMessageOption(options.getString('message'));
  if (parsed.error) return replyError(interaction, 'Invalid Message', parsed.error);

  // Fast cap pre-check (createFeedRow enforces it again under a mutex). Skipped when the ack budget ran out.
  const existingCount = await withinAckBudget(interaction, prisma.feed.count({ where: { guildId } }), 'feed cap');
  if (existingCount !== null && existingCount >= LIMITS.FEEDS) {
    return replyError(interaction, 'Feed Limit Reached', `This server already follows ${LIMITS.FEEDS} feeds (the maximum). Remove one with \`/feed remove\` first.`);
  }
  const includeReposts = type === 'BLUESKY' ? options.getBoolean('include_reposts') ?? false : false;

  // Step B: acknowledge publicly; everything below may take up to about 25 s (network).
  await interaction.deferReply();

  // Step C: resolve and probe the source.
  const r = await resolveFeedSource(type, input);
  if (!r.ok) return editError(interaction, r.title, r.message);

  // Step D: duplicate.
  const dup = await prisma.feed.findFirst({ where: { guildId, type, source: r.source, channelId: channel.id } });
  if (dup) return editError(interaction, 'Already Following', `**${escapeMarkdown(dup.displayName)}** is already posted in <#${channel.id}>.`);

  // Step E: create (throws FeedUserError for the cap and for P2002).
  const row = await createFeedRow({
    guildId, type, source: r.source, displayName: r.displayName, fetchUrl: r.fetchUrl, channelId: channel.id,
    pingRoleId: role?.id ?? null, messageTemplate: parsed.template, includeReposts, createdBy: interaction.user.id, probe: r.probe,
  });

  // Step F: schedule the first check.
  const scheduleWarnings = [];
  try {
    const runAt = new Date(Date.now() + (row.initialized ? BASE_INTERVAL_MS[type] : 2 * 60_000));
    await scheduleFeedJob(row, { runAt });
  } catch (err) {
    logger.error(`feed add: could not schedule feed ${row.id}`, err);
    scheduleWarnings.push(`⚠️ I couldn't schedule the first check (database busy). Run \`/feed enable feed:${row.displayName}\` to retry.`);
  }

  // Step G: reply.
  const lines = feedSummaryLines(row);
  lines.push(row.initialized
    ? `✅ ${r.probe.items.length} existing item(s) recorded — only new ones will be posted.`
    : `⚠️ I couldn't fetch it yet: ${row.lastError}. I'll keep trying; see \`/feed list\`.`);
  lines.push(`Variables: ${varHelp(type)}`);
  lines.push(...r.warnings, ...warnings, ...templateWarnings(type, parsed.template), ...scheduleWarnings);
  const off = moduleOffNote(settings);
  if (off) lines.push(off);
  return interaction.editReply({ embeds: [successEmbed('Feed Added', clampText(lines.join('\n'), 4000))], allowedMentions: NO_MENTIONS });
}

// ── list ──────────────────────────────────────────────────────────────────────

async function handleList(interaction, settings) {
  await interaction.deferReply();
  const rows = await prisma.feed.findMany({ where: { guildId: interaction.guildId }, orderBy: { createdAt: 'asc' } });
  if (!rows.length) {
    return interaction.editReply({
      embeds: [infoEmbed('Feeds', 'No feeds yet. Add one with `/feed add rss`, `youtube`, `reddit`, `bluesky` or `podcast`.')],
      allowedMentions: NO_MENTIONS,
    });
  }
  const jobs = await mapLimit(rows, 5, (r) => getJob(feedJobKey(r.id)));
  const entries = rows.map((r, i) => {
    const job = jobs[i];
    const next = job.status === 'fulfilled' && job.value?.status === 'PENDING' ? job.value.runAt : null;
    const status = feedStatus(r);
    const extras = [];
    if (r.enabled && next) extras.push(`next check ${discordTimestamp(next, 'R')}`);
    if (r.lastPostedAt) extras.push(`last post ${discordTimestamp(r.lastPostedAt, 'R')}`);
    else if (r.lastCheckedAt) extras.push(`checked ${discordTimestamp(r.lastCheckedAt, 'R')}`);
    return `\`${i + 1}.\` ${TYPE_EMOJI[r.type]} **${escapeMarkdown(r.displayName)}** → <#${r.channelId}>${r.pingRoleId ? ` · <@&${r.pingRoleId}>` : ''}\n`
      + `↳ ${status.icon} ${clampText(status.text, 120)}${extras.length ? ` · ${extras.join(' · ')}` : ''}`;
  });
  const off = moduleOffNote(settings);
  const pages = chunkLines(entries, 3500).map((chunk) => guildEmbed(
    settings, '📰 Social Feeds', (off ? `${off}\n\n` : '') + chunk, `${rows.length}/${LIMITS.FEEDS} feeds`,
  ));
  if (pages.length === 1) return interaction.editReply({ embeds: [pages[0]], allowedMentions: NO_MENTIONS });
  return paginate(interaction, pages);
}

// ── remove ────────────────────────────────────────────────────────────────────

async function handleRemove(interaction) {
  const feed = await lookupFeed(interaction);
  if (!feed) return undefined;
  await interaction.deferReply();
  await prisma.feed.deleteMany({ where: { id: feed.id, guildId: interaction.guildId } });
  await cancelJobs({ dedupeKey: feedJobKey(feed.id) });
  return interaction.editReply({
    embeds: [successEmbed('Feed Removed', `**${escapeMarkdown(feed.displayName)}** will no longer be posted in <#${feed.channelId}>.`)],
    allowedMentions: NO_MENTIONS,
  });
}

// ── test ──────────────────────────────────────────────────────────────────────

async function handleTest(interaction) {
  const feed = await lookupFeed(interaction);
  if (!feed) return undefined;
  await interaction.deferReply({ ephemeral: true });
  if (!testThrottle(feed.id)) {
    return interaction.editReply({ embeds: [warnEmbed('Slow Down', 'This feed was just tested. Try again in a few seconds.')], allowedMentions: NO_MENTIONS });
  }
  const t0 = Date.now();
  // A fresh, unconditional fetch (a 304 would leave nothing to preview). It may refresh the shared cache; it never writes the Feed row.
  const res = await fetchFeedSource(feed, { useCache: false, conditional: false });
  if (!res.ok) {
    return editError(interaction, 'Fetch Failed', `${describeFetchError(feed.type, res.error)}\nNothing was changed.`);
  }
  const items = res.items ?? [];
  const eligible = items.filter((i) => feed.includeReposts || !i.isRepost);
  if (!eligible.length) {
    return interaction.editReply({ embeds: [infoEmbed('No Items', 'The feed answered but has no items to show yet.')], allowedMentions: NO_MENTIONS });
  }
  const guild = interaction.guild;
  const payload = buildPostPayload(feed, eligible[0], { guild, channel: guild.channels.cache.get(feed.channelId) ?? null, feedImage: res.image, ping: false });
  const status = feedStatus(feed);
  const lines = `Fetched ${items.length} item(s) in ${Date.now() - t0} ms. Above is how the newest one would look in <#${feed.channelId}>.\n`
    + `Status: ${status.icon} ${clampText(status.text, 200)}`;
  return interaction.editReply({
    content: payload.content,
    embeds: [...payload.embeds, infoEmbed('Feed Test', lines)],
    allowedMentions: NO_MENTIONS,
  });
}

// ── edit ──────────────────────────────────────────────────────────────────────

async function handleEdit(interaction, settings) {
  const feed = await lookupFeed(interaction);
  if (!feed) return undefined;
  const { guild, options } = interaction;
  const channelOpt = options.getChannel('channel');
  const roleOpt = options.getRole('ping_role');
  const clearRole = options.getBoolean('clear_ping_role') === true;
  const message = options.getString('message');
  const resetMessage = options.getBoolean('reset_message') === true;
  const reposts = options.getBoolean('include_reposts');

  if (roleOpt && clearRole) return replyError(interaction, 'Conflicting Options', 'Use either `ping_role` or `clear_ping_role`, not both.');
  if (message !== null && resetMessage) return replyError(interaction, 'Conflicting Options', 'Use either `message` or `reset_message`, not both.');
  if (reposts !== null && feed.type !== 'BLUESKY') return replyError(interaction, 'Not a Bluesky Feed', 'Only Bluesky feeds can include reposts.');

  const warnings = [];
  const data = {};
  let channel = guild.channels.cache.get(feed.channelId) ?? null;
  if (channelOpt) {
    const target = checkTargetChannel(interaction, channelOpt);
    if (target.error) return replyError(interaction, target.error[0], target.error[1]);
    channel = target.channel;
    if (channel.id !== feed.channelId) {
      data.channelId = channel.id;
      if (feed.failureCount === 0) data.lastError = null; // it was a channel problem
    }
  }
  let finalRole = feed.pingRoleId ? guild.roles.cache.get(feed.pingRoleId) ?? null : null;
  if (roleOpt) {
    if (!channel) return replyError(interaction, 'Channel Problem', 'Its channel no longer exists. Pick a new one with `channel:` in the same command.');
    const check = checkPingRole(interaction, roleOpt, channel);
    if (check.error) return replyError(interaction, check.error[0], check.error[1]);
    data.pingRoleId = roleOpt.id;
    finalRole = guild.roles.cache.get(roleOpt.id) ?? roleOpt;
  } else if (clearRole) {
    data.pingRoleId = null;
    finalRole = null;
  }
  // A channel change can break an existing ping role: warn (the bot-side check is not an error).
  if (finalRole && channel && (channelOpt || roleOpt)) {
    const w = botPingWarning(guild, channel, finalRole);
    if (w) warnings.push(w);
  }
  let template = null;
  if (message !== null) {
    const parsed = parseMessageOption(message);
    if (parsed.error) return replyError(interaction, 'Invalid Message', parsed.error);
    if (parsed.template === null) return replyError(interaction, 'Invalid Message', "The message can't be empty. Use reset_message to go back to the default.");
    template = parsed.template;
    data.messageTemplate = template;
  } else if (resetMessage) {
    data.messageTemplate = null;
  }
  if (reposts !== null && reposts !== feed.includeReposts) data.includeReposts = reposts;
  if (Object.keys(data).length === 0) return replyError(interaction, 'Nothing to Change', 'Give at least one option to change.');

  await interaction.deferReply();
  let updated;
  try {
    updated = await prisma.feed.update({ where: { id: feed.id }, data });
  } catch (err) {
    if (err?.code === 'P2002') return editError(interaction, 'Already Following', 'That channel already has this feed.');
    throw err;
  }
  const lines = feedSummaryLines(updated, { interval: false });
  lines.push(...warnings, ...templateWarnings(updated.type, template));
  if (!updated.enabled) lines.push(`⏸️ This feed is disabled — run \`/feed enable feed:${updated.displayName}\` to turn it on.`);
  const off = moduleOffNote(settings);
  if (off) lines.push(off);
  return interaction.editReply({ embeds: [successEmbed('Feed Updated', clampText(lines.join('\n'), 4000))], allowedMentions: NO_MENTIONS });
}

// ── enable / disable ──────────────────────────────────────────────────────────

async function handleEnable(interaction, settings) {
  const feed = await lookupFeed(interaction);
  if (!feed) return undefined;
  const channel = interaction.guild.channels.cache.get(feed.channelId) ?? null;
  const u = checkChannelUsable(channel, { needs: NEEDS });
  if (!u.ok) {
    return replyError(interaction, 'Channel Problem', `${u.reason ?? 'Its channel no longer exists.'} Pick a new one with \`/feed edit channel:\` first.`);
  }
  await interaction.deferReply();
  const wasDisabled = !feed.enabled;
  // Re-enabling a disabled feed resets `initialized`: the first check re-seeds, so items published while it was off are skipped.
  const patch = wasDisabled
    ? { enabled: true, failureCount: 0, lastError: null, disabledReason: null, initialized: false }
    : { failureCount: 0, lastError: null }; // already on: used to repair scheduling
  const updated = await prisma.feed.update({ where: { id: feed.id }, data: patch });
  await scheduleFeedJob(updated, { runAt: new Date(Date.now() + 10_000) });
  const lines = [`I'll check **${escapeMarkdown(updated.displayName)}** in a few seconds.${wasDisabled ? ' Items published while it was off are skipped; only new ones will be posted.' : ''}`];
  const off = moduleOffNote(settings);
  if (off) lines.push(off);
  return interaction.editReply({ embeds: [successEmbed('Feed Enabled', lines.join('\n'))], allowedMentions: NO_MENTIONS });
}

async function handleDisable(interaction) {
  const feed = await lookupFeed(interaction);
  if (!feed) return undefined;
  const name = escapeMarkdown(feed.displayName);
  if (!feed.enabled) return replyError(interaction, 'Already Paused', `**${name}** is already disabled: ${feed.disabledReason ?? 'paused'}.`);
  await interaction.deferReply();
  await prisma.feed.update({ where: { id: feed.id }, data: { enabled: false, disabledReason: 'Paused by an admin' } });
  await cancelJobs({ dedupeKey: feedJobKey(feed.id) });
  return interaction.editReply({
    embeds: [successEmbed('Feed Paused', `**${name}** is paused. Its settings are kept; turn it back on with \`/feed enable\`.`)],
    allowedMentions: NO_MENTIONS,
  });
}

// ── Entry points ──────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  try {
    const settings = await loadSettings(interaction);
    if (!settings) return;
    if (group === 'add') {
      await handleAdd(interaction, sub, settings);
      return;
    }
    switch (sub) {
      case 'list': await handleList(interaction, settings); break;
      case 'remove': await handleRemove(interaction); break;
      case 'test': await handleTest(interaction); break;
      case 'edit': await handleEdit(interaction, settings); break;
      case 'enable': await handleEnable(interaction, settings); break;
      case 'disable': await handleDisable(interaction); break;
      default: await replyError(interaction, 'Unknown Subcommand', 'That subcommand does not exist.');
    }
  } catch (err) {
    await handleError(interaction, err, group, sub);
  }
}

export async function autocomplete(interaction) {
  try {
    if (!interaction.memberPermissions?.has(PermissionFlagsBits.ManageGuild)) return respondAutocomplete(interaction, []);
    const q = String(interaction.options.getFocused() ?? '').trim().slice(0, 100);
    const rows = await prisma.feed.findMany({
      where: {
        guildId: interaction.guildId,
        ...(q ? { OR: [{ displayName: { contains: q, mode: 'insensitive' } }, { source: { contains: q, mode: 'insensitive' } }] } : {}),
      },
      orderBy: { createdAt: 'asc' },
      take: 25,
      select: { id: true, type: true, displayName: true, channelId: true, enabled: true },
    });
    return respondAutocomplete(interaction, rows.map((r) => ({
      name: `${r.enabled ? '' : '⏸️ '}${TYPE_EMOJI[r.type]} ${r.displayName} → #${interaction.guild?.channels.cache.get(r.channelId)?.name ?? 'deleted-channel'}`,
      value: r.id,
    })));
  } catch {
    return respondAutocomplete(interaction, []);
  }
}
