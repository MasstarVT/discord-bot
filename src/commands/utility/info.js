import { SlashCommandBuilder, PermissionFlagsBits, ChannelType, Routes, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { guildEmbed, errorEmbed } from '../../utils/embedBuilder.js';
import { permissionNames, safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import { clampText, joinWithLimit, formatNumber } from '../../utils/text.js';
import { formatColor } from '../../utils/customEmbed.js';
import { withTimeout } from '../../utils/async.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import logger from '../../utils/logger.js';

export const module = null; // core command, never gated
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
// allowInDMs: default false (guild-only; /info server needs a guild anyway)

export const data = new SlashCommandBuilder()
  .setName('info')
  .setDescription('User, server and role info (MEE6 /user-info /server-info /role-info)')
  .addSubcommand((s) => s.setName('user').setDescription('Show info about a member (MEE6 /user-info)')
    .addUserOption((o) => o.setName('member').setDescription('Member to inspect (default: you)')))
  .addSubcommand((s) => s.setName('server').setDescription('Show info about this server (MEE6 /server-info)'))
  .addSubcommand((s) => s.setName('role').setDescription('Show info about a role (MEE6 /role-info)')
    .addRoleOption((o) => o.setName('role').setDescription('Role to inspect').setRequired(true)));

// ── Constants ──────────────────────────────────────────────────────────────────

export const KEY_PERMISSIONS = Object.freeze([
  'ManageGuild', 'ManageRoles', 'ManageChannels', 'ManageMessages',
  'ManageThreads', 'ManageWebhooks', 'ManageNicknames', 'ManageGuildExpressions', 'ManageEvents',
  'KickMembers', 'BanMembers', 'ModerateMembers', 'MentionEveryone', 'ViewAuditLog',
]);

export const VERIFICATION_LABELS = Object.freeze(['None', 'Low', 'Medium', 'High', 'Highest']);
export const CONTENT_FILTER_LABELS = Object.freeze(['Off', 'Members without roles', 'All members']);
export const USER_BADGES = Object.freeze({
  Staff: 'Discord Staff',
  Partner: 'Partnered Server Owner',
  Hypesquad: 'HypeSquad Events',
  BugHunterLevel1: 'Bug Hunter',
  BugHunterLevel2: 'Bug Hunter (Gold)',
  HypeSquadOnlineHouse1: 'HypeSquad Bravery',
  HypeSquadOnlineHouse2: 'HypeSquad Brilliance',
  HypeSquadOnlineHouse3: 'HypeSquad Balance',
  PremiumEarlySupporter: 'Early Supporter',
  VerifiedBot: 'Verified Bot',
  VerifiedDeveloper: 'Early Verified Bot Developer',
  CertifiedModerator: 'Moderator Programs Alumni',
  ActiveDeveloper: 'Active Developer',
});

const COUNTS_TTL_MS = 60_000;
const COUNTS_MAX_ENTRIES = 500;
const REST_TIMEOUT_MS = 3_000;

/** @type {Map<string, { at: number, members: number, online: number|null }>} */
const GUILD_COUNTS = new Map();
/** @type {Map<string, { at: number, counts: Map<string, number> }>} */
const ROLE_COUNTS = new Map();

function remember(map, key, value) {
  if (!map.has(key) && map.size >= COUNTS_MAX_ENTRIES) map.delete(map.keys().next().value);
  map.set(key, value);
}

// ── Pure helpers ───────────────────────────────────────────────────────────────

/**
 * The notable permissions a member or role holds, as display names, in KEY_PERMISSIONS order.
 * @param {import('discord.js').PermissionsBitField} perms
 * @returns {string[]}
 */
export function keyPermissionNames(perms) {
  if (!perms) return [];
  if (perms.has(PermissionFlagsBits.Administrator)) return ['Administrator (all permissions)'];
  const held = KEY_PERMISSIONS.filter((k) => perms.has(PermissionFlagsBits[k], false));
  return held.map((k) => permissionNames(k)[0]);
}

/**
 * Role mentions, highest first, @everyone excluded; never cuts a mention.
 * @param {Iterable<{ id: string, position: number }>} roles
 * @param {string} guildId
 * @param {number} [max=1024]
 * @returns {string} 'None' when there are no roles
 */
export function formatRoleMentions(roles, guildId, max = 1024) {
  const items = [...(roles ?? [])]
    .filter((r) => r.id !== guildId)
    .sort((a, b) => b.position - a.position)
    .map((r) => `<@&${r.id}>`);
  if (!items.length) return 'None';
  return joinWithLimit(items, { sep: ' ', max });
}

/**
 * @param {Iterable<{ type: number }>} channels - threads are ignored
 * @returns {{ text: number, voice: number, forum: number, category: number }}
 */
export function countChannels(channels) {
  const out = { text: 0, voice: 0, forum: 0, category: 0 };
  for (const c of channels ?? []) {
    switch (c.type) {
      case ChannelType.GuildText:
      case ChannelType.GuildAnnouncement:
        out.text++;
        break;
      case ChannelType.GuildVoice:
      case ChannelType.GuildStageVoice:
        out.voice++;
        break;
      case ChannelType.GuildForum:
      case ChannelType.GuildMedia:
        out.forum++;
        break;
      case ChannelType.GuildCategory:
        out.category++;
        break;
      default:
        break;
    }
  }
  return out;
}

const ts = (ms) => `<t:${Math.floor(ms / 1000)}:F>\n<t:${Math.floor(ms / 1000)}:R>`;
const esc = (s) => escapeMarkdown(String(s ?? ''));
const yes = (b) => (b ? 'Yes' : 'No');

// Discord also reports flags that only track its own rollouts ("..._MIGRATION_COMPLETE", experiments,
// legacy thread switches). They say nothing about the server, so /info server leaves them out.
export const INTERNAL_FEATURE_PATTERN = /_MIGRATION(_COMPLETE)?$|_EXPERIMENT$|_OVERRIDE$|^(CHANNEL_ICON_EMOJIS_GENERATED|GUILD_ONBOARDING_EVER_ENABLED|GUILD_ONBOARDING_HAS_PROMPTS|NEW_THREAD_PERMISSIONS|THREADS_ENABLED|TEXT_IN_VOICE_ENABLED|SEVEN_DAY_THREAD_ARCHIVE|THREE_DAY_THREAD_ARCHIVE|HAS_DIRECTORY_ENTRY)$/;
export const isPublicFeature = (feature) => !INTERNAL_FEATURE_PATTERN.test(String(feature).toUpperCase());

function accessLevelLabel(level, member, guild) {
  if (level >= PERMISSION_LEVELS.OWNER) return 'Bot Owner';
  if (level >= PERMISSION_LEVELS.ADMIN) return member.id === guild.ownerId ? 'Server Owner' : 'Admin'; // /help calls this level "Admin"
  if (level >= PERMISSION_LEVELS.MODERATOR) return 'Moderator';
  return 'Member';
}

// ── REST counts (cached 60 s, in-process) ──────────────────────────────────────

async function getGuildCounts(guild) {
  const hit = GUILD_COUNTS.get(guild.id);
  if (hit && Date.now() - hit.at < COUNTS_TTL_MS) return hit;
  // Raw REST so the cached Guild object is not re-patched.
  const res = await withTimeout(
    guild.client.rest.get(Routes.guild(guild.id), { query: new URLSearchParams({ with_counts: 'true' }) }),
    REST_TIMEOUT_MS,
    'info guild counts',
  ).catch(() => null);
  const entry = {
    at: Date.now(),
    members: res?.approximate_member_count ?? guild.memberCount,
    online: res?.approximate_presence_count ?? null,
    stickers: Array.isArray(res?.stickers) ? res.stickers.length : null, // bot.js keeps no sticker cache
  };
  remember(GUILD_COUNTS, guild.id, entry);
  return entry;
}

async function getRoleCounts(guild) {
  const hit = ROLE_COUNTS.get(guild.id);
  if (hit && Date.now() - hit.at < COUNTS_TTL_MS) return hit.counts;
  const fetched = await withTimeout(guild.roles.fetchMemberCounts(), REST_TIMEOUT_MS, 'info role counts');
  const counts = new Map([...fetched.entries()].map(([id, n]) => [String(id), Number(n)]));
  remember(ROLE_COUNTS, guild.id, { at: Date.now(), counts });
  return counts;
}

// ── Embed builders ─────────────────────────────────────────────────────────────

/**
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @param {object|null} settings
 * @returns {Promise<import('discord.js').EmbedBuilder>}
 */
export async function buildUserEmbed(interaction, settings) {
  const guild = interaction.guild;
  const user = interaction.options.getUser('member') ?? interaction.user;
  const member = guild.members.cache.get(user.id)
    ?? await withTimeout(guild.members.fetch(user.id), REST_TIMEOUT_MS, 'info member fetch').catch(() => null);
  const displayName = member?.displayName ?? user.globalName ?? user.username;

  const embed = guildEmbed(settings, clampText(displayName, 256))
    .setThumbnail((member ?? user).displayAvatarURL({ size: 256 }))
    .setFooter({ text: `User ID: ${user.id}` });
  if (member?.displayColor > 0) embed.setColor(member.displayColor);

  const fields = [
    {
      name: '👤 User',
      value: [
        `**Mention:** <@${user.id}>`,
        `**Username:** ${esc(user.username)}`,
        `**Display name:** ${esc(displayName)}`,
        `**Bot:** ${yes(user.bot)}`,
      ].join('\n'),
      inline: true,
    },
    { name: '📅 Account created', value: ts(user.createdTimestamp), inline: true },
    {
      name: '📥 Joined server',
      value: member?.joinedTimestamp != null ? ts(member.joinedTimestamp) : 'Not a member of this server',
      inline: true,
    },
  ];
  if (member?.premiumSinceTimestamp) {
    fields.push({ name: '💎 Boosting since', value: ts(member.premiumSinceTimestamp), inline: true });
  }
  // The timeout is moderation information: only moderators see it.
  if (member?.communicationDisabledUntilTimestamp > Date.now()
    && resolvePermissionLevel(interaction.member, settings) >= PERMISSION_LEVELS.MODERATOR) {
    fields.push({ name: '🔇 Timed out until', value: ts(member.communicationDisabledUntilTimestamp), inline: true });
  }
  const badges = (user.flags?.toArray?.() ?? []).filter((f) => Object.hasOwn(USER_BADGES, f)).map((f) => USER_BADGES[f]);
  if (badges.length) fields.push({ name: '🏅 Badges', value: clampText(badges.join(', '), 1024), inline: false });

  if (member) {
    const roleCount = member.roles.cache.filter((r) => r.id !== guild.id).size;
    fields.push({ name: `🎭 Roles [${roleCount}]`, value: formatRoleMentions(member.roles.cache.values(), guild.id), inline: false });
    const perms = keyPermissionNames(member.permissions);
    fields.push({ name: '🔑 Key permissions', value: perms.length ? joinWithLimit(perms, { max: 1024 }) : 'None', inline: false });
    fields.push({
      name: '🛡️ Access level',
      value: accessLevelLabel(resolvePermissionLevel(member, settings), member, guild),
      inline: true,
    });
  }
  return embed.addFields(fields);
}

/**
 * @param {import('discord.js').Guild} guild
 * @param {object|null} settings
 * @returns {Promise<import('discord.js').EmbedBuilder>}
 */
export async function buildServerEmbed(guild, settings) {
  const { members, online, stickers } = await getGuildCounts(guild);
  const embed = guildEmbed(settings, clampText(guild.name, 256)).setFooter({ text: `Server ID: ${guild.id}` });
  if (guild.description) embed.setDescription(clampText(escapeMarkdown(guild.description), 1024));
  const icon = guild.iconURL({ size: 256 });
  if (icon) embed.setThumbnail(icon);
  const banner = guild.bannerURL({ size: 1024 });
  if (banner) embed.setImage(banner);

  const ch = countChannels(guild.channels.cache.values());
  const emojis = [...guild.emojis.cache.values()];
  const animated = emojis.filter((e) => e.animated).length;
  const fields = [
    { name: '👑 Owner', value: `<@${guild.ownerId}>`, inline: true },
    { name: '📅 Created', value: ts(guild.createdTimestamp), inline: true },
    {
      name: '👥 Members',
      value: `${formatNumber(members)} total${online != null ? `\n🟢 ${formatNumber(online)} online` : ''}`,
      inline: true,
    },
    {
      name: '💬 Channels',
      value: `Text: ${ch.text}\nVoice: ${ch.voice}\nForum: ${ch.forum}\nCategories: ${ch.category}`,
      inline: true,
    },
    { name: '🎭 Roles', value: formatNumber(guild.roles.cache.size - 1), inline: true },
    {
      name: '😀 Emojis & stickers',
      value: `${emojis.length} emojis (${animated} animated)${stickers != null ? `\n${stickers} stickers` : ''}`,
      inline: true,
    },
    {
      name: '💎 Boosts',
      value: `Level ${guild.premiumTier} · ${guild.premiumSubscriptionCount ?? 0} boosts`,
      inline: true,
    },
    {
      name: '🛡️ Security',
      value: [
        `Verification: ${VERIFICATION_LABELS[guild.verificationLevel] ?? 'Unknown'}`,
        `Content filter: ${CONTENT_FILTER_LABELS[guild.explicitContentFilter] ?? 'Unknown'}`,
        `2FA for moderators: ${guild.mfaLevel === 1 ? 'Required' : 'Not required'}`,
      ].join('\n'),
      inline: true,
    },
    { name: '🌐 Locale', value: clampText(esc(guild.preferredLocale), 1024), inline: true },
  ];
  if (guild.vanityURLCode) {
    fields.push({ name: '🔗 Vanity URL', value: clampText(`discord.gg/${esc(guild.vanityURLCode)}`, 1024), inline: true });
  }
  const publicFeatures = (guild.features ?? []).filter(isPublicFeature);
  if (publicFeatures.length) {
    const list = publicFeatures
      .map((f) => String(f).toLowerCase().split('_').map((w) => w.charAt(0).toUpperCase() + w.slice(1)).join(' '))
      .sort();
    fields.push({ name: '✨ Features', value: joinWithLimit(list.map(esc), { max: 1024 }), inline: false });
  }
  return embed.addFields(fields);
}

/**
 * @param {import('discord.js').Guild} guild
 * @param {import('discord.js').Role} pickedRole
 * @param {object|null} settings
 * @returns {Promise<import('discord.js').EmbedBuilder>}
 */
export async function buildRoleEmbed(guild, pickedRole, settings) {
  const role = guild.roles.cache.get(pickedRole.id) ?? pickedRole;
  // role.color is deprecated in discord.js 14.27; read `colors` first.
  const primary = role.colors?.primaryColor ?? role.color ?? 0;
  const secondary = role.colors?.secondaryColor ?? null;

  const embed = guildEmbed(settings, clampText(`${role.unicodeEmoji ? `${role.unicodeEmoji} ` : ''}${role.name}`, 256))
    .setDescription(role.id === guild.id ? '@everyone' : `<@&${role.id}>`)
    .setFooter({ text: `Role ID: ${role.id}` });
  if (primary > 0) embed.setColor(primary);
  const icon = role.iconURL?.({ size: 256 });
  if (icon) embed.setThumbnail(icon);

  let memberText;
  if (role.id === guild.id) {
    memberText = formatNumber(guild.memberCount);
  } else {
    try {
      const counts = await getRoleCounts(guild);
      memberText = formatNumber(counts.get(role.id) ?? 0);
    } catch (err) {
      logger.debug(`info: role counts unavailable: ${err?.message}`);
      memberText = `${formatNumber(role.members?.size ?? 0)} (cached members only)`;
    }
  }

  const fields = [
    {
      name: '🎨 Colour',
      value: primary === 0 ? 'Default' : formatColor(primary) + (secondary ? ` → ${formatColor(secondary)}` : ''),
      inline: true,
    },
    { name: '👥 Members', value: memberText, inline: true },
    { name: '📊 Position', value: `${role.position} of ${guild.roles.cache.size - 1}`, inline: true },
    { name: '📅 Created', value: ts(role.createdTimestamp), inline: true },
    {
      name: '⚙️ Settings',
      value: `Hoisted: ${yes(role.hoist)}\nMentionable: ${yes(role.mentionable)}\nManaged: ${yes(role.managed)}`,
      inline: true,
    },
  ];
  if (role.managed || role.tags) {
    const tags = role.tags ?? {};
    let managedBy = 'Unknown';
    if (tags.botId) managedBy = `<@${tags.botId}> (bot role)`;
    else if (tags.premiumSubscriberRole) managedBy = 'Server Boosts';
    else if (tags.subscriptionListingId) managedBy = 'Server Subscription';
    else if (tags.guildConnections) managedBy = 'Linked Roles';
    else if (tags.integrationId) managedBy = 'An integration';
    fields.push({ name: '🔗 Managed by', value: managedBy, inline: true });
  }
  const perms = keyPermissionNames(role.permissions);
  fields.push({ name: '🔑 Key permissions', value: perms.length ? joinWithLimit(perms, { max: 1024 }) : 'None', inline: false });
  return embed.addFields(fields);
}

// ── Command ────────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  if (!(await safeDefer(interaction))) return;
  try {
    const guild = interaction.guild;
    if (!guild) {
      await safeReply(interaction, { embeds: [errorEmbed('Server Only', 'Use this command in a server.')] });
      return;
    }
    const settings = await getSettingsSafe(interaction.guildId); // null is fine: default colour, no mod roles

    let embed;
    switch (interaction.options.getSubcommand()) {
      case 'user':
        embed = await buildUserEmbed(interaction, settings);
        break;
      case 'server':
        embed = await buildServerEmbed(guild, settings);
        break;
      case 'role':
        embed = await buildRoleEmbed(guild, interaction.options.getRole('role', true), settings);
        break;
      default:
        await safeReply(interaction, { embeds: [errorEmbed('Unknown Subcommand', 'Use `/info user`, `/info server` or `/info role`.')] });
        return;
    }
    await safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
  } catch (err) {
    logger.error('info failed', err);
    await safeReply(interaction, { embeds: [errorEmbed('Info Unavailable', 'I could not load that information. Please try again.')] });
  }
}
