import { EmbedBuilder } from 'discord.js';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import logger from '../utils/logger.js';
import { xpForLevel, COLORS } from '../config/constants.js';

// ── XP math helpers ───────────────────────────────────────────────────────────

/**
 * Returns the user's level given their total accumulated XP.
 * Each level requires xpForLevel(level) XP to advance.
 */
export function levelFromXp(totalXp) {
  let level = 0;
  let remaining = totalXp;
  while (remaining >= xpForLevel(level)) {
    remaining -= xpForLevel(level);
    level++;
  }
  return level;
}

/** XP accumulated within the current level (resets each level-up). */
export function xpIntoLevel(totalXp) {
  let level = 0;
  let remaining = totalXp;
  while (remaining >= xpForLevel(level)) {
    remaining -= xpForLevel(level);
    level++;
  }
  return remaining;
}

/** XP needed to advance from current level to next. */
export function xpToNextLevel(level) {
  return xpForLevel(level);
}

// ── Settings cache ────────────────────────────────────────────────────────────

async function getSettings(guildId) {
  const key = `guild:${guildId}:settings`;
  const cached = await cache.get(key);
  if (cached) return cached;
  const settings = await prisma.guildSettings.findUnique({ where: { guildId } });
  if (settings) await cache.set(key, settings, 300);
  return settings;
}

// ── Text XP ───────────────────────────────────────────────────────────────────

/**
 * Called from messageCreate. Awards XP to the author with:
 * - Per-user/guild Redis cooldown to prevent spam
 * - ±20% random variance on base rate
 * - Guild XP multiplier
 * - No-XP role/channel exemptions
 */
export async function grantXp(message, client) {
  const { guild, author, member, channelId } = message;
  if (!guild || !member) return;

  const guildId = guild.id;
  const userId  = author.id;

  const settings = await getSettings(guildId);
  if (!settings?.xpEnabled) return;

  // Exempt channels
  if (settings.noXpChannelIds?.includes(channelId)) return;

  // Exempt roles
  if (settings.noXpRoleIds?.length > 0) {
    const memberRoleIds = member.roles.cache.map((r) => r.id);
    if (settings.noXpRoleIds.some((id) => memberRoleIds.includes(id))) return;
  }

  // Cooldown check
  const cooldownKey = `xp:cooldown:${guildId}:${userId}`;
  if (await cache.get(cooldownKey)) return;

  // Calculate XP (base ± 20% variance)
  const base     = settings.xpRate ?? 15;
  const variance = Math.floor(base * 0.2);
  const earned   = base - variance + Math.floor(Math.random() * (variance * 2 + 1));
  const total    = Math.max(1, Math.floor(earned * (settings.xpMultiplier ?? 1.0)));

  // Set cooldown
  await cache.set(cooldownKey, '1', settings.xpCooldown ?? 60);

  // Upsert UserLevel
  const record = await prisma.userLevel.upsert({
    where:  { guildId_userId: { guildId, userId } },
    update: { xp: { increment: total }, messageCount: { increment: 1 }, lastXpAt: new Date() },
    create: { guildId, userId, xp: total, level: 0, messageCount: 1, lastXpAt: new Date() },
  });

  // Level-up check
  const newLevel = levelFromXp(record.xp);
  const oldLevel = levelFromXp(record.xp - total);

  if (newLevel > oldLevel) {
    await prisma.userLevel.update({
      where: { guildId_userId: { guildId, userId } },
      data:  { level: newLevel },
    });

    await sendLevelUp(message, newLevel, settings, client);
    await assignLevelRoles(member, newLevel, settings);
  }
}

// ── Level-up notification ─────────────────────────────────────────────────────

async function sendLevelUp(message, newLevel, settings, client) {
  const template = settings.levelUpMessage ?? '🎉 {mention} has reached **level {level}**!';
  const content  = template
    .replace('{mention}',     `<@${message.author.id}>`)
    .replace('{level}',       String(newLevel))
    .replace('{user}',        message.author.username)
    .replace('{server}',      message.guild.name);

  const embed = new EmbedBuilder()
    .setColor(COLORS.SUCCESS)
    .setDescription(content)
    .setThumbnail(message.author.displayAvatarURL({ size: 64 }))
    .setFooter({ text: message.guild.name });

  let channel = message.channel;

  if (settings.levelUpChannelId) {
    const configured = message.guild.channels.cache.get(settings.levelUpChannelId)
      ?? await message.guild.channels.fetch(settings.levelUpChannelId).catch(() => null);
    if (configured?.isTextBased()) channel = configured;
  }

  await channel.send({ embeds: [embed] }).catch((err) =>
    logger.warn(`xpService: could not send level-up message in ${channel.id}`, err.message)
  );
}

// ── Role rewards ──────────────────────────────────────────────────────────────

async function assignLevelRoles(member, newLevel, settings) {
  const levelRoles = await prisma.levelRole.findMany({
    where:   { guildId: member.guild.id },
    orderBy: { level: 'asc' },
  });
  if (levelRoles.length === 0) return;

  const earned = levelRoles.filter((lr) => lr.level <= newLevel);
  if (earned.length === 0) return;

  const stack = settings.levelRolesStack ?? true;

  if (stack) {
    for (const lr of earned) {
      if (!member.roles.cache.has(lr.roleId)) {
        await member.roles.add(lr.roleId, `Level ${lr.level} reward`).catch(() => null);
      }
    }
  } else {
    // Replace mode: keep only the highest-tier earned role
    const highest    = earned[earned.length - 1];
    const allRoleIds = levelRoles.map((lr) => lr.roleId);
    const toRemove   = allRoleIds.filter((id) => id !== highest.roleId && member.roles.cache.has(id));

    if (toRemove.length > 0) await member.roles.remove(toRemove, 'Level role replace').catch(() => null);
    if (!member.roles.cache.has(highest.roleId)) {
      await member.roles.add(highest.roleId, `Level ${highest.level} reward`).catch(() => null);
    }
  }
}

// ── Voice XP ──────────────────────────────────────────────────────────────────

/**
 * Called from voiceStateUpdate. Tracks join/leave times via Redis and
 * grants XP proportional to minutes spent in a non-AFK channel.
 */
export async function handleVoiceXp(oldState, newState, client) {
  const guild  = newState.guild;
  const member = newState.member;
  if (!member || member.user.bot) return;

  const guildId = guild.id;
  const userId  = member.id;

  const settings = await getSettings(guildId);
  if (!settings?.xpEnabled || !settings?.voiceXpEnabled) return;

  const oldChannel = oldState.channel;
  const newChannel = newState.channel;
  const afkId      = guild.afkChannelId;

  const voiceKey = `voice:start:${guildId}:${userId}`;

  // User joined a non-AFK voice channel
  if (!oldChannel && newChannel && newChannel.id !== afkId) {
    await cache.set(voiceKey, String(Date.now()), 86400); // 24h TTL
    return;
  }

  // User left voice (or moved to AFK)
  const didLeave = (oldChannel && !newChannel) || (oldChannel && newChannel?.id === afkId && oldChannel.id !== afkId);
  if (!didLeave) return;

  const startStr = await cache.get(voiceKey);
  if (!startStr) return;
  await cache.del(voiceKey);

  const minutes = Math.floor((Date.now() - parseInt(startStr, 10)) / 60_000);
  if (minutes < 1) return;

  const rate  = settings.voiceXpRate ?? 5;
  const total = Math.max(1, Math.floor(minutes * rate * (settings.xpMultiplier ?? 1.0)));

  // Exempt roles
  if (settings.noXpRoleIds?.length > 0) {
    const memberRoleIds = member.roles.cache.map((r) => r.id);
    if (settings.noXpRoleIds.some((id) => memberRoleIds.includes(id))) return;
  }

  const record = await prisma.userLevel.upsert({
    where:  { guildId_userId: { guildId, userId } },
    update: { xp: { increment: total }, voiceMinutes: { increment: minutes }, lastXpAt: new Date() },
    create: { guildId, userId, xp: total, level: 0, voiceMinutes: minutes, lastXpAt: new Date() },
  });

  const newLevel = levelFromXp(record.xp);
  const oldLevel = levelFromXp(record.xp - total);

  if (newLevel > oldLevel) {
    await prisma.userLevel.update({
      where: { guildId_userId: { guildId, userId } },
      data:  { level: newLevel },
    });
    // Voice level-ups get a DM to avoid spamming a text channel
    await member.send({
      embeds: [new EmbedBuilder()
        .setColor(COLORS.SUCCESS)
        .setDescription(`🎙️ Your voice activity in **${guild.name}** earned you enough XP to reach **level ${newLevel}**!`)
      ],
    }).catch(() => null);
    await assignLevelRoles(member, newLevel, settings);
  }
}

// ── Admin XP mutations ────────────────────────────────────────────────────────

export async function adminSetXp(guildId, userId, amount) {
  const record = await prisma.userLevel.upsert({
    where:  { guildId_userId: { guildId, userId } },
    update: { xp: amount, level: levelFromXp(amount) },
    create: { guildId, userId, xp: amount, level: levelFromXp(amount) },
  });
  await cache.del(`guild:${guildId}:settings`);
  return record;
}

export async function adminAddXp(guildId, userId, amount) {
  const existing = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId } } });
  const base = existing?.xp ?? 0;
  return adminSetXp(guildId, userId, Math.max(0, base + amount));
}

export async function adminResetXp(guildId, userId) {
  await prisma.userLevel.deleteMany({ where: { guildId, userId } });
}
