import { SlashCommandBuilder, ChannelType, PermissionFlagsBits, RESTJSONErrorCodes } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { cache } from '../../services/redis.js';
import {
  LINK_COOLDOWN_SECONDS, botCanTrack, getCodeState, deleteDiscordInvite, getInviteStats, getInviteLeaderboard,
  getJoinRecord, getPersonalInvite, savePersonalInvite,
} from '../../services/inviteTracker.js';
import { guildEmbed, errorEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { paginate } from '../../utils/paginator.js';
import { missingBotPermissions, permissionNames } from '../../utils/interactionHelpers.js';
import { clampText, formatNumber } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';
import { createKeyedMutex } from '../../utils/async.js';
import logger from '../../utils/logger.js';

export const module = 'invites';
export const gated = true;                                   // the router refuses it while the module is off
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
// No subcommandPermissions, no allowInDMs (guild-only), no default member permissions: everyone may run everything.

const LINK_CHANNEL_TYPES = [
  ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildVoice, ChannelType.GuildStageVoice,
];
const SUPPORTED = new Set(LINK_CHANNEL_TYPES);
const LEADERBOARD_PAGE_SIZE = 10;
const GONE_CODES = new Set([10062, 40060, 10015, 50027]);     // the interaction expired or was already answered
const UNKNOWN_INVITER = 'Unknown (joined before tracking, via Server Discovery, or the invite could not be determined)';

export const data = new SlashCommandBuilder()
  .setName('invites')
  .setDescription('Invite Tracker: /invites, /inviter, /invites-leaderboard and personal links')
  .addSubcommand((s) => s.setName('show')
    .setDescription('Invite stats (valid, left, fake) and who invited a member — /invites')
    .addUserOption((o) => o.setName('member').setDescription('Member to check (default: you)')))
  .addSubcommand((s) => s.setName('inviter')
    .setDescription('Who invited a member — /inviter')
    .addUserOption((o) => o.setName('member').setDescription('Member to look up').setRequired(true)))
  .addSubcommand((s) => s.setName('leaderboard')
    .setDescription('Top inviters by valid invites — /invites-leaderboard')
    .addIntegerOption((o) => o.setName('page').setDescription('Page (1–20)').setMinValue(1).setMaxValue(20)))
  .addSubcommand((s) => s.setName('link')
    .setDescription('Get your personal, never-expiring invite link')
    .addChannelOption((o) => o.setName('channel').setDescription('Channel the link opens (default: this one)')
      .addChannelTypes(...LINK_CHANNEL_TYPES)));

/** Serialises /invites link per member (double clicks must not create orphan invites). */
const linkMutex = createKeyedMutex();

// ── Helpers ───────────────────────────────────────────────────────────────────

/** editReply that ignores an interaction that can no longer be answered. */
async function edit(interaction, payload) {
  try {
    return await interaction.editReply(payload);
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.warn(`/invites reply failed: ${err?.message ?? err}`);
    return null;
  }
}

const unavailableEmbed = () => errorEmbed('Temporarily Unavailable', 'Invite stats could not be loaded. Please try again in a moment.');

const pl = (n) => (n === 1 ? '' : 's');

/** One-line description of a GuildMemberCache row for the "Invited by" field. */
function describeJoinRecord(record, guild) {
  if (!record || (!record.inviterId && !record.inviteCode)) return UNKNOWN_INVITER;
  const when = discordTimestamp(record.joinedAt, 'R');
  const code = record.inviteCode;
  let text;
  if (record.inviterId) text = `<@${record.inviterId}>${code ? ` · \`discord.gg/${code}\`` : ''} · joined ${when}`;
  else if (code === guild.vanityURLCode) text = `Vanity URL \`discord.gg/${code}\` · joined ${when}`;
  else text = `\`discord.gg/${code}\` (creator unknown) · joined ${when}`;
  if (record.leftAt) text += ` · left ${discordTimestamp(record.leftAt, 'R')}`;
  return clampText(text, 1024);
}

/** The sentence for /invites inviter. */
function describeInviter(record, targetId, guild) {
  if (!record || (!record.inviterId && !record.inviteCode)) {
    return `I don't know who invited <@${targetId}>. They may have joined before invite tracking started, through Server Discovery, or with an invite I couldn't determine.`;
  }
  const when = discordTimestamp(record.joinedAt, 'R');
  const code = record.inviteCode;
  let text;
  if (record.inviterId) text = `<@${targetId}> was invited by <@${record.inviterId}>${code ? ` with \`discord.gg/${code}\`` : ''} ${when}.`;
  else if (code === guild.vanityURLCode) text = `<@${targetId}> joined through the vanity URL \`discord.gg/${code}\` ${when}.`;
  else text = `<@${targetId}> joined with \`discord.gg/${code}\` ${when}, but I don't know who created that invite.`;
  if (record.leftAt) text += `\nThey left ${discordTimestamp(record.leftAt, 'R')}.`;
  return text;
}

// ── /invites show ─────────────────────────────────────────────────────────────

async function showStats(interaction) {
  await interaction.deferReply();
  const guild = interaction.guild;
  const target = interaction.options.getUser('member') ?? interaction.user;
  const targetMember = interaction.options.getMember('member') ?? (target.id === interaction.user.id ? interaction.member : null);
  const name = clampText(targetMember?.displayName ?? target.globalName ?? target.username, 100);

  let stats; let record; let personal; let settings;
  try {
    [stats, record, personal, settings] = await Promise.all([
      getInviteStats(guild.id, target.id),
      getJoinRecord(guild.id, target.id),
      getPersonalInvite(guild.id, target.id),
      getSettingsSafe(guild.id),
    ]);
  } catch (err) {
    logger.warn(`/invites show failed in ${guild.id}: ${err?.message ?? err}`);
    await edit(interaction, { embeds: [unavailableEmbed()] });
    return;
  }

  let description = `<@${target.id}> has **${formatNumber(stats.valid)}** valid invite${pl(stats.valid)}.`;
  if (stats.recent > 0) {
    description += `\n_${formatNumber(stats.recent)} of them joined less than 24 hours ago and become fake if they leave before then._`;
  }
  const footer = botCanTrack(guild)
    ? 'Valid = still here or stayed 24 h · Fake = left within 24 h'
    : '⚠️ Invite tracking is paused: I need the Manage Server permission.';
  const embed = guildEmbed(settings, `📨 Invites — ${name}`, description, footer).addFields(
    { name: '✅ Valid', value: formatNumber(stats.valid), inline: true },
    { name: '🏠 Still here', value: formatNumber(stats.active), inline: true },
    { name: '🚪 Left (after 24 h)', value: formatNumber(stats.left), inline: true },
    { name: '❌ Fake', value: formatNumber(stats.fake), inline: true },
    { name: '📥 Total joins', value: formatNumber(stats.total), inline: true },
    { name: '🙋 Invited by', value: describeJoinRecord(record, guild), inline: false },
  );

  if (personal) {
    const state = await getCodeState(guild, personal.code, { allowFetch: false, since: personal.createdAt.getTime() });
    embed.addFields({
      name: '🔗 Personal link',
      value: clampText(state === 'gone'
        ? 'Expired — run `/invites link` to get a new one.'
        : `https://discord.gg/${personal.code} (opens <#${personal.channelId}>)`, 1024),
      inline: false,
    });
  }
  await edit(interaction, { embeds: [embed] });
}

// ── /invites inviter ──────────────────────────────────────────────────────────

async function showInviter(interaction) {
  await interaction.deferReply();
  const guild = interaction.guild;
  const target = interaction.options.getUser('member', true);
  let record; let settings;
  try {
    [record, settings] = await Promise.all([getJoinRecord(guild.id, target.id), getSettingsSafe(guild.id)]);
  } catch (err) {
    logger.warn(`/invites inviter failed in ${guild.id}: ${err?.message ?? err}`);
    await edit(interaction, { embeds: [unavailableEmbed()] });
    return;
  }
  await edit(interaction, { embeds: [guildEmbed(settings, '🙋 Inviter', describeInviter(record, target.id, guild))] });
}

// ── /invites leaderboard ──────────────────────────────────────────────────────

const medal = (rank) => (rank === 1 ? '🥇' : rank === 2 ? '🥈' : rank === 3 ? '🥉' : `**#${rank}**`);

async function showLeaderboard(interaction) {
  await interaction.deferReply();
  const guild = interaction.guild;
  let rows; let settings;
  try {
    [rows, settings] = await Promise.all([
      getInviteLeaderboard(guild.id, { limit: 200, excludeIds: [interaction.client.user.id] }),
      getSettingsSafe(guild.id),
    ]);
  } catch (err) {
    logger.warn(`/invites leaderboard failed in ${guild.id}: ${err?.message ?? err}`);
    await edit(interaction, { embeds: [unavailableEmbed()] });
    return;
  }
  if (rows.length === 0) {
    await edit(interaction, { embeds: [guildEmbed(settings, '🏆 Invite Leaderboard', 'No invites have been tracked in this server yet.')] });
    return;
  }

  const myIndex = rows.findIndex((r) => r.inviterId === interaction.user.id);
  const footer = `Valid = still here or stayed 24 h${myIndex >= 0 ? ` · Your rank: #${myIndex + 1}` : ''}`;
  const title = `🏆 ${clampText(guild.name, 200)} — Invite Leaderboard`;
  const pages = [];
  for (let i = 0; i < rows.length && pages.length < 20; i += LEADERBOARD_PAGE_SIZE) {
    const lines = rows.slice(i, i + LEADERBOARD_PAGE_SIZE).map((r, j) => {
      const rank = i + j + 1;
      return `${medal(rank)} <@${r.inviterId}> — **${formatNumber(r.valid)}** invite${pl(r.valid)} (${formatNumber(r.left)} left · ${formatNumber(r.fake)} fake)`;
    });
    pages.push(guildEmbed(settings, title, lines.join('\n'), footer));
  }
  const page = interaction.options.getInteger('page');
  const startPage = Math.min((page ?? 1) - 1, pages.length - 1);
  await paginate(interaction, pages, { startPage });
}

// ── /invites link ─────────────────────────────────────────────────────────────

function linkReply(settings, { code, channelId, reused = false, replacedCode = null }) {
  const url = `https://discord.gg/${code}`;
  let footer;
  if (reused) footer = 'This is your existing link.';
  else if (replacedCode) footer = `Your previous link discord.gg/${replacedCode} was deleted.`;
  return {
    content: url,                                            // a plain URL, easy to copy on mobile
    embeds: [guildEmbed(settings, '🔗 Your personal invite link',
      `**${url}**\nOpens <#${channelId}> · never expires · unlimited uses.\n\n`
      + 'Everyone who joins with this link is credited to you. An invite counts while the member stays, '
      + 'or once they have stayed 24 hours; leaving sooner makes it fake.',
      footer)],
    allowedMentions: { parse: [] },
  };
}

/** The first failure for a candidate channel, or { ok: true }. */
function checkChannel(interaction, ch) {
  if (!ch || !SUPPORTED.has(ch.type)) return { ok: false, why: 'type' };
  const missing = missingBotPermissions(ch, ['ViewChannel', 'CreateInstantInvite']);
  if (missing.length) return { ok: false, why: 'bot', missing };
  const memberPerms = ch.permissionsFor(interaction.member);
  if (!memberPerms?.has([PermissionFlagsBits.ViewChannel, PermissionFlagsBits.CreateInstantInvite])) return { ok: false, why: 'member' };
  return { ok: true };
}

/** @returns {{ channel: import('discord.js').GuildChannel } | { error: import('discord.js').EmbedBuilder }} */
function resolveTargetChannel(interaction, explicit) {
  if (explicit) {
    const res = checkChannel(interaction, explicit);
    if (res.ok) return { channel: explicit };
    if (res.why === 'type') return { error: errorEmbed('Unsupported Channel', 'Invites can only open a text, announcement, voice or stage channel.') };
    if (res.why === 'bot') return { error: errorEmbed('Missing Permissions', `I need **${permissionNames(res.missing).join(', ')}** in <#${explicit.id}>.`) };
    return { error: errorEmbed('Not Allowed', `You don't have permission to create invites in <#${explicit.id}>.`) };
  }
  const here = interaction.channel;
  const candidates = [
    here && !here.isThread?.() ? here : null,
    here?.isThread?.() ? here.parent : null,
    interaction.guild.systemChannel,
  ];
  const seen = new Set();
  for (const ch of candidates) {
    if (!ch || seen.has(ch.id)) continue;
    seen.add(ch.id);
    if (checkChannel(interaction, ch).ok) return { channel: ch };
  }
  return {
    error: errorEmbed('No Channel Available',
      "I can't create an invite in this channel or the server's system channel. Pick a channel with the `channel` option where both of us can create invites."),
  };
}

function createInviteError(err, channel) {
  switch (err?.code) {
    case RESTJSONErrorCodes.MaximumNumberOfInvitesReached:
      return errorEmbed('Invite Limit Reached', "This server has reached Discord's limit of 1,000 invites. Ask an admin to delete unused invites, then try again.");
    case RESTJSONErrorCodes.MissingPermissions:
    case RESTJSONErrorCodes.MissingAccess:
      return errorEmbed('Missing Permissions', `I need **View Channel** and **Create Invite** in <#${channel.id}>.`);
    case RESTJSONErrorCodes.UnknownChannel:
      return errorEmbed('Channel Not Found', 'That channel no longer exists.');
    default:
      logger.error('/invites link: createInvite failed', err instanceof Error ? err : new Error(String(err)));
      return errorEmbed('Something Went Wrong', 'I could not create your invite link. Please try again later.');
  }
}

async function createLink(interaction) {
  await interaction.deferReply({ ephemeral: true });
  const guild = interaction.guild;
  const userId = interaction.user.id;
  const settings = await getSettingsSafe(guild.id);         // colour only
  const explicit = interaction.options.getChannel('channel');

  await linkMutex.run(`${guild.id}:${userId}`, async () => {
    let existing;
    try {
      existing = await getPersonalInvite(guild.id, userId);
    } catch (err) {
      logger.warn(`/invites link lookup failed in ${guild.id}: ${err?.message ?? err}`);
      await edit(interaction, { embeds: [unavailableEmbed()] });
      return;
    }

    // Reuse: an unknown state counts as alive, so a network error never creates a duplicate.
    let oldState = null;
    if (existing) {
      oldState = await getCodeState(guild, existing.code, { allowFetch: true, since: existing.createdAt.getTime() });
      if (oldState !== 'gone' && (!explicit || explicit.id === existing.channelId)) {
        await edit(interaction, linkReply(settings, { code: existing.code, channelId: existing.channelId, reused: true }));
        return;
      }
    }

    const target = resolveTargetChannel(interaction, explicit);
    if (target.error) {
      await edit(interaction, { embeds: [target.error] });
      return;
    }
    const channel = target.channel;

    // Cooldown (anti-spam). Redis down (null) fails OPEN: Discord's own limits still apply.
    const cdKey = `invites:link:cd:${guild.id}:${userId}`;
    const ok = await cache.setNX(cdKey, '1', LINK_COOLDOWN_SECONDS);
    if (ok === false) {
      await edit(interaction, { embeds: [warnEmbed('Slow Down', 'You just created an invite link. Try again in a minute.')] });
      return;
    }

    let invite;
    try {
      invite = await channel.createInvite({
        maxAge: 0, maxUses: 0, unique: true, temporary: false,
        reason: clampText(`Personal invite link for ${interaction.user.tag} (/invites link)`, 512),
      });
    } catch (err) {
      await cache.del(cdKey);                                 // a failed attempt must not lock the user out
      await edit(interaction, { embeds: [createInviteError(err, channel)] });
      return;
    }

    try {
      await savePersonalInvite({ guildId: guild.id, userId, code: invite.code, channelId: channel.id });
    } catch (err) {
      logger.warn(`/invites link save failed in ${guild.id}: ${err?.message ?? err}`);
      deleteDiscordInvite(guild, invite.code, 'Personal invite could not be saved').catch(() => {});
      await cache.del(cdKey);
      await edit(interaction, { embeds: [unavailableEmbed()] });
      return;
    }

    let replacedCode = null;
    if (existing && existing.code !== invite.code) {
      deleteDiscordInvite(guild, existing.code, 'Replaced by a new personal invite link').catch(() => {});
      if (oldState !== 'gone') replacedCode = existing.code;
    }
    await edit(interaction, linkReply(settings, { code: invite.code, channelId: channel.id, replacedCode }));
  });
}

// ── Entry point ───────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  try {
    if (sub === 'show') await showStats(interaction);
    else if (sub === 'inviter') await showInviter(interaction);
    else if (sub === 'leaderboard') await showLeaderboard(interaction);
    else if (sub === 'link') await createLink(interaction);
  } catch (err) {
    logger.error(`/invites ${sub} failed`, err instanceof Error ? err : new Error(String(err)));
    if (interaction.deferred || interaction.replied) {
      await edit(interaction, { embeds: [errorEmbed('Something Went Wrong', 'That command failed. Please try again later.')] });
    }
  }
}
