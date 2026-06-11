import { ChannelType } from 'discord.js';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import logger from '../utils/logger.js';

// ── Log type → GuildSettings channel field ────────────────────────────────────

export const LOG_TYPES = {
  MESSAGE:    'messageLogChannelId',
  MEMBER:     'memberLogChannelId',
  SERVER:     'serverLogChannelId',
  VOICE:      'voiceLogChannelId',
  JOIN_LEAVE: 'joinLeaveChannelId',
  MOD:        'modLogChannelId',
};

// ── Channel type label helper ─────────────────────────────────────────────────

const CHANNEL_TYPE_LABELS = {
  [ChannelType.GuildText]:          'Text Channel',
  [ChannelType.GuildVoice]:         'Voice Channel',
  [ChannelType.GuildCategory]:      'Category',
  [ChannelType.GuildAnnouncement]:  'Announcement Channel',
  [ChannelType.GuildStageVoice]:    'Stage Channel',
  [ChannelType.GuildForum]:         'Forum Channel',
  [ChannelType.GuildMedia]:         'Media Channel',
  [ChannelType.GuildDirectory]:     'Directory Channel',
  [ChannelType.PublicThread]:       'Public Thread',
  [ChannelType.PrivateThread]:      'Private Thread',
  [ChannelType.AnnouncementThread]: 'Announcement Thread',
};

export function channelTypeLabel(type) {
  return CHANNEL_TYPE_LABELS[type] ?? `Unknown (${type})`;
}

// ── Settings cache ─────────────────────────────────────────────────────────────

async function getSettings(guildId) {
  const key = `guild:${guildId}:settings`;
  const hit = await cache.get(key);
  if (hit) return hit;
  const s = await prisma.guildSettings.findUnique({ where: { guildId } });
  if (s) await cache.set(key, s, 300);
  return s;
}

// ── Ghost ping extraction ─────────────────────────────────────────────────────

/**
 * Returns an array of mention strings found in a message.
 * Skips bot mentions.
 */
export function extractMentions(message) {
  if (!message || message.partial) return [];
  const mentions = [];
  for (const [, user] of (message.mentions?.users ?? [])) {
    if (!user.bot) mentions.push(`<@${user.id}> (${user.tag})`);
  }
  for (const [, role] of (message.mentions?.roles ?? [])) {
    mentions.push(`<@&${role.id}> (${role.name})`);
  }
  if (message.mentions?.everyone) mentions.push('`@everyone` / `@here`');
  return mentions;
}

// ── Core dispatch ─────────────────────────────────────────────────────────────

/**
 * Sends a log embed to the configured channel for the given log type.
 * Silently skips if logging is disabled or no channel is configured.
 *
 * @param {import('discord.js').Guild} guild
 * @param {keyof typeof LOG_TYPES} logType  e.g. 'MESSAGE', 'MEMBER'
 * @param {import('discord.js').EmbedBuilder} embed
 * @param {import('discord.js').Client} client
 */
export async function sendLog(guild, logType, embed, client) {
  let settings;
  try {
    settings = await getSettings(guild.id);
  } catch (err) {
    logger.error(`loggingService: settings fetch failed for guild ${guild.id}`, err);
    return;
  }

  if (!settings?.loggingEnabled) return;

  const field     = LOG_TYPES[logType];
  const channelId = settings[field];
  if (!channelId) return;

  const channel = guild.channels.cache.get(channelId)
    ?? await guild.channels.fetch(channelId).catch(() => null);

  if (!channel?.isTextBased()) return;

  await channel.send({ embeds: [embed] }).catch((err) =>
    logger.error(`loggingService: failed to send ${logType} log to ${channelId}`, err)
  );
}
