import { EmbedBuilder } from 'discord.js';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import logger from '../utils/logger.js';
import { parsePlaceholders } from '../utils/placeholderParser.js';
import { COLORS } from '../config/constants.js';

// ── Shared settings fetch ─────────────────────────────────────────────────────

async function getSettings(guildId) {
  const key    = `guild:${guildId}:settings`;
  const cached = await cache.get(key);
  if (cached) return cached;
  const settings = await prisma.guildSettings.findUnique({ where: { guildId } });
  if (settings) await cache.set(key, settings, 300);
  return settings;
}

// ── Trigger cache ─────────────────────────────────────────────────────────────

const TRIGGER_CACHE_TTL = 300; // 5 minutes

async function getGuildTriggers(guildId) {
  const key    = `guild:${guildId}:triggers`;
  const cached = await cache.get(key);
  if (cached) return cached;
  const triggers = await prisma.autoTrigger.findMany({
    where:   { guildId, enabled: true },
    orderBy: { createdAt: 'asc' },
  });
  await cache.set(key, triggers, TRIGGER_CACHE_TTL);
  return triggers;
}

export async function invalidateTriggerCache(guildId) {
  await cache.del(`guild:${guildId}:triggers`);
}

// ── Pattern matching ──────────────────────────────────────────────────────────

function matchesTrigger(trigger, content) {
  const flags   = trigger.caseSensitive ? '' : 'i';
  const subject = trigger.caseSensitive ? content : content.toLowerCase();
  const pattern = trigger.caseSensitive ? trigger.trigger : trigger.trigger.toLowerCase();

  switch (trigger.matchType) {
    case 'EXACT':       return subject === pattern;
    case 'CONTAINS':    return subject.includes(pattern);
    case 'STARTS_WITH': return subject.startsWith(pattern);
    case 'ENDS_WITH':   return subject.endsWith(pattern);
    case 'REGEX': {
      try {
        return new RegExp(trigger.trigger, flags).test(content);
      } catch {
        return false;
      }
    }
    default:
      return false;
  }
}

// ── Tag response builder ──────────────────────────────────────────────────────

/**
 * Builds a message payload from a tag's stored content.
 *
 * @param {object} tag  CustomTag record
 * @param {import('discord.js').GuildMember} member
 * @param {import('discord.js').Guild} guild
 * @returns {{ content?: string, embeds?: EmbedBuilder[] }}
 */
export function buildTagPayload(tag, member, guild) {
  const text = parsePlaceholders(tag.content, member, guild);
  if (tag.useEmbed) {
    return {
      embeds: [new EmbedBuilder().setColor(COLORS.INFO).setDescription(text).setTimestamp()],
    };
  }
  return { content: text };
}

// ── Auto-trigger engine ───────────────────────────────────────────────────────

/**
 * Runs all enabled triggers against an incoming message.
 * First matching trigger fires; the rest are skipped.
 *
 * @param {import('discord.js').Message} message
 * @param {import('discord.js').Client} client
 */
export async function runTriggers(message, client) {
  if (!message.guild || !message.content || message.author.bot) return;

  const { guild, member, content, channelId, author } = message;
  const guildId = guild.id;

  const settings = await getSettings(guildId);
  if (!settings?.customCommandsEnabled) return;

  const triggers = await getGuildTriggers(guildId);
  if (triggers.length === 0) return;

  for (const trigger of triggers) {
    if (!matchesTrigger(trigger, content)) continue;

    // Channel restriction
    if (trigger.channelIds.length > 0 && !trigger.channelIds.includes(channelId)) continue;

    // Exempt roles
    if (trigger.exemptRoleIds.length > 0) {
      const memberRoles = member?.roles.cache.map((r) => r.id) ?? [];
      if (trigger.exemptRoleIds.some((id) => memberRoles.includes(id))) continue;
    }

    // Per-user cooldown
    if (trigger.cooldown > 0) {
      const cdKey = `trigger:cd:${guildId}:${trigger.id}:${author.id}`;
      if (await cache.get(cdKey)) continue;
      await cache.set(cdKey, '1', trigger.cooldown);
    }

    // Delete original message
    if (trigger.deleteMessage) {
      await message.delete().catch(() => null);
    }

    // Send response
    const text = parsePlaceholders(trigger.response, member, guild);
    await message.channel.send({ content: text }).catch((err) =>
      logger.warn(`tagService: trigger response failed in ${channelId}`, err.message)
    );

    // Increment uses (fire-and-forget)
    prisma.autoTrigger.update({ where: { id: trigger.id }, data: { uses: { increment: 1 } } })
      .catch(() => null);

    // Only fire the first matching trigger
    break;
  }
}
