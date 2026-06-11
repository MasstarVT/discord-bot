import { EmbedBuilder, AttachmentBuilder } from 'discord.js';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import { parsePlaceholders } from '../utils/placeholderParser.js';
import { generateWelcomeCard } from '../utils/welcomeCard.js';
import { getUsedInvite } from './inviteTracker.js';
import logger from '../utils/logger.js';

// ── Settings cache ─────────────────────────────────────────────────────────────

async function getSettings(guildId) {
  const key = `guild:${guildId}:settings`;
  const hit = await cache.get(key);
  if (hit) return hit;
  const s = await prisma.guildSettings.findUnique({ where: { guildId } });
  if (s) await cache.set(key, s, 300);
  return s;
}

// ── Welcome embed builder ──────────────────────────────────────────────────────

/**
 * Reconstructs a welcome EmbedBuilder from the stored JSON config.
 * Applies placeholder parsing to all text fields.
 *
 * welcomeEmbed JSON shape:
 * {
 *   title:       string,
 *   description: string,
 *   color:       number,
 *   thumbnail:   "user_avatar" | "server_icon" | null,
 *   footer:      string,
 *   card:        boolean   ← generate canvas welcome card
 * }
 */
function buildWelcomeEmbed(cfg, member, guild) {
  const embed = new EmbedBuilder()
    .setColor(cfg.color ?? 0x57f287)
    .setTimestamp();

  if (cfg.title) {
    embed.setTitle(parsePlaceholders(cfg.title, member, guild));
  }
  if (cfg.description) {
    embed.setDescription(parsePlaceholders(cfg.description, member, guild));
  }
  if (cfg.footer) {
    embed.setFooter({ text: parsePlaceholders(cfg.footer, member, guild) });
  }
  if (cfg.thumbnail === 'user_avatar') {
    embed.setThumbnail(member.user.displayAvatarURL({ size: 256 }));
  } else if (cfg.thumbnail === 'server_icon' && guild.iconURL()) {
    embed.setThumbnail(guild.iconURL({ size: 256 }));
  }

  return embed;
}

function defaultWelcomeEmbed(member, guild) {
  return new EmbedBuilder()
    .setColor(0x57f287)
    .setTitle(`👋 Welcome to ${guild.name}!`)
    .setDescription(
      `Hey ${member}, welcome to **${guild.name}**!\n` +
      `You are our **${guild.memberCount.toLocaleString()}th** member. Enjoy your stay!`
    )
    .setThumbnail(member.user.displayAvatarURL({ size: 256 }))
    .setTimestamp();
}

// ── Send welcome message ───────────────────────────────────────────────────────

/**
 * Sends the configured welcome message (embed + optional canvas card) to the
 * welcome channel. Also logs which invite was used if invite tracking is enabled.
 *
 * @param {import('discord.js').GuildMember} member
 * @param {import('discord.js').Client} client
 */
export async function sendWelcome(member, client) {
  const { guild } = member;
  const settings  = await getSettings(guild.id);

  if (!settings?.welcomeEnabled || !settings.welcomeChannelId) return;

  const channel = guild.channels.cache.get(settings.welcomeChannelId)
    ?? await guild.channels.fetch(settings.welcomeChannelId).catch(() => null);
  if (!channel?.isTextBased()) return;

  const cfg   = settings.welcomeEmbed ?? {};
  const embed = Object.keys(cfg).length > 0
    ? buildWelcomeEmbed(cfg, member, guild)
    : defaultWelcomeEmbed(member, guild);

  const payload = { embeds: [embed] };

  // Canvas card attachment
  if (cfg.card !== false) {
    try {
      const buffer     = await generateWelcomeCard(member, guild);
      const attachment = new AttachmentBuilder(buffer, { name: 'welcome.png' });
      embed.setImage('attachment://welcome.png');
      payload.files = [attachment];
    } catch (err) {
      logger.warn(`Welcome card generation failed for ${member.user.tag}: ${err.message}`);
    }
  }

  // Invite tracking
  try {
    const usedInvite = await getUsedInvite(guild);
    if (usedInvite) {
      await prisma.guildMemberCache.upsert({
        where:  { guildId_userId: { guildId: guild.id, userId: member.id } },
        update: { joinedAt: new Date(), inviterId: usedInvite.inviter?.id ?? null, inviteCode: usedInvite.code },
        create: { guildId: guild.id, userId: member.id, joinedAt: new Date(), inviterId: usedInvite.inviter?.id ?? null, inviteCode: usedInvite.code },
      });
    }
  } catch (err) {
    logger.warn(`Invite tracking failed for ${member.user.tag}: ${err.message}`);
  }

  await channel.send(payload).catch((err) =>
    logger.error(`Failed to send welcome message in guild ${guild.id}`, err)
  );
}

// ── Autorole ──────────────────────────────────────────────────────────────────

/**
 * Assigns configured autoroles to a member.
 * Respects pending status (Discord member screening) and delay setting.
 *
 * @param {import('discord.js').GuildMember} member
 * @param {import('discord.js').Client} client
 */
export async function assignAutorole(member, client) {
  const { guild } = member;
  const settings  = await getSettings(guild.id);

  if (!settings?.autoroleIds?.length) return;

  // If the member hasn't passed Rules Screening yet, defer until they do.
  // The guildMemberUpdate event handles the pending → false transition.
  if (member.pending) return;

  const delayMs = (settings.autoroleDelay ?? 0) * 1_000;

  const assign = async () => {
    // Re-fetch member to ensure they're still in the server
    const freshMember = await guild.members.fetch(member.id).catch(() => null);
    if (!freshMember) return;

    for (const roleId of settings.autoroleIds) {
      if (freshMember.roles.cache.has(roleId)) continue;
      await freshMember.roles.add(roleId, 'Autorole on join').catch((err) =>
        logger.warn(`Failed to assign autorole ${roleId} to ${member.user.tag}: ${err.message}`)
      );
    }
  };

  if (delayMs > 0) {
    setTimeout(assign, delayMs);
  } else {
    await assign();
  }
}
