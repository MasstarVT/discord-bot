import { EmbedBuilder } from 'discord.js';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import { createInfraction } from './moderationService.js';
import logger from '../utils/logger.js';

// ── Regex patterns ────────────────────────────────────────────────────────────

const INVITE_RE   = /discord(?:\.gg|(?:app)?\.com\/invite)\/([a-z0-9-]{2,32})/i;
const URL_RE      = /https?:\/\/[^\s]+|(?:^|\s)(?:[a-z0-9-]+\.)+[a-z]{2,}(?:\/[^\s]*)?/i;
const MENTION_RE  = /<@[!&]?(\d+)>|@everyone|@here/g;

// ── Settings cache ─────────────────────────────────────────────────────────────

async function getSettings(guildId) {
  const key = `guild:${guildId}:settings`;
  const hit = await cache.get(key);
  if (hit) return hit;
  const s = await prisma.guildSettings.findUnique({ where: { guildId } });
  if (s) await cache.set(key, s, 300);
  return s;
}

// ── Individual checks ─────────────────────────────────────────────────────────

function checkInvites(content) {
  return INVITE_RE.test(content);
}

function checkLinks(content, blockedDomains = []) {
  if (!URL_RE.test(content)) return false;
  if (blockedDomains.length === 0) return true;
  const lower = content.toLowerCase();
  return blockedDomains.some((d) => lower.includes(d.toLowerCase()));
}

function checkBadWords(content, badWords = [], customPatterns = []) {
  const lower = content.toLowerCase();
  if (badWords.some((w) => lower.includes(w.toLowerCase()))) return true;
  for (const pattern of customPatterns) {
    try {
      if (new RegExp(pattern, 'i').test(content)) return true;
    } catch {
      // Ignore malformed patterns stored in DB
    }
  }
  return false;
}

function checkMassMention(message, threshold = 5) {
  const mentions = [...(message.content.matchAll(MENTION_RE) ?? [])];
  return mentions.length >= threshold;
}

async function checkDuplicate(message, threshold = 3) {
  if (threshold <= 1) return false;
  const key     = `automod:dup:${message.guild.id}:${message.author.id}`;
  const recent  = (await cache.get(key)) ?? [];
  const content = message.content.trim().toLowerCase();

  const count = recent.filter((c) => c === content).length + 1;
  const updated = [...recent.slice(-19), content]; // keep last 20
  await cache.set(key, updated, 10); // 10-second rolling window

  return count >= threshold;
}

// ── Log & action helpers ───────────────────────────────────────────────────────

async function sendAutomodLog(guild, message, triggerName, client) {
  const settings = await getSettings(guild.id);
  const channelId = settings?.modLogChannelId;
  if (!channelId) return;

  const channel = guild.channels.cache.get(channelId)
    ?? await guild.channels.fetch(channelId).catch(() => null);
  if (!channel?.isTextBased()) return;

  const embed = new EmbedBuilder()
    .setColor(0xff9800)
    .setTitle('🤖 Automod — Message Deleted')
    .addFields(
      { name: 'User',    value: `<@${message.author.id}> \`${message.author.tag}\``, inline: true },
      { name: 'Channel', value: `<#${message.channelId}>`, inline: true },
      { name: 'Trigger', value: triggerName },
      { name: 'Content', value: message.content.slice(0, 1024) || '*[empty]*' },
    )
    .setFooter({ text: `User ID: ${message.author.id}` })
    .setTimestamp();

  await channel.send({ embeds: [embed] }).catch(() => null);
}

async function applyAutomodAction(message, triggerName, cfg, client) {
  // Delete the offending message first
  await message.delete().catch(() => null);

  // Warn the user in-channel briefly
  const warning = await message.channel.send({
    content: `<@${message.author.id}> Your message was removed by automod: **${triggerName}**.`,
  }).catch(() => null);
  if (warning) setTimeout(() => warning.delete().catch(() => null), 6_000);

  // Log to mod channel
  await sendAutomodLog(message.guild, message, triggerName, client);

  // Apply configured action
  const action = cfg.automodAction ?? 'WARN';
  if (action === 'NONE') return;

  try {
    await createInfraction(
      message.guild,
      message.author,
      client.user,
      action,
      {
        reason:   `Automod: ${triggerName}`,
        silent:   true,
        client,
      },
    );
  } catch (err) {
    logger.error('Automod infraction creation failed', err);
  }
}

// ── Main entry ────────────────────────────────────────────────────────────────

/**
 * Runs all automod checks against an incoming message.
 * Called from src/events/messageCreate.js.
 *
 * @param {import('discord.js').Message} message
 * @param {import('discord.js').Client} client
 */
export async function runAutomod(message, client) {
  const settings = await getSettings(message.guild.id);
  if (!settings?.automodEnabled) return;

  const cfg = settings.automodConfig ?? {};

  // Exempt roles — skip if member has any exempt role
  const exemptRoles = cfg.exemptRoleIds ?? [];
  if (exemptRoles.length > 0 && message.member?.roles.cache.some((r) => exemptRoles.includes(r.id))) return;

  // Exempt channels
  const exemptChannels = cfg.exemptChannelIds ?? [];
  if (exemptChannels.includes(message.channelId)) return;

  const content = message.content;

  if (cfg.filterInvites && checkInvites(content)) {
    return applyAutomodAction(message, 'Discord Invite Link', cfg, client);
  }

  if (cfg.filterLinks && checkLinks(content, cfg.blockedDomains ?? [])) {
    return applyAutomodAction(message, 'Blocked Link', cfg, client);
  }

  if (cfg.filterBadWords && checkBadWords(content, cfg.badWords ?? [], cfg.badWordPatterns ?? [])) {
    return applyAutomodAction(message, 'Filtered Word', cfg, client);
  }

  const mentionThreshold = cfg.massMentionThreshold ?? 5;
  if (mentionThreshold > 0 && checkMassMention(message, mentionThreshold)) {
    return applyAutomodAction(message, 'Mass Mention', cfg, client);
  }

  const dupThreshold = cfg.duplicateThreshold ?? 0;
  if (dupThreshold > 1 && await checkDuplicate(message, dupThreshold)) {
    return applyAutomodAction(message, 'Duplicate Spam', cfg, client);
  }
}
