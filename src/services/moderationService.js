import { EmbedBuilder, PermissionFlagsBits } from 'discord.js';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import logger from '../utils/logger.js';
import { formatDuration } from '../utils/parseDuration.js';

// ── Action metadata ───────────────────────────────────────────────────────────

const ACTION_META = {
  WARN:     { emoji: '⚠️',  color: 0xfee75c, past: 'Warned',    active: false },
  TIMEOUT:  { emoji: '⏱️',  color: 0xff9800, past: 'Timed Out', active: true  },
  MUTE:     { emoji: '🔇',  color: 0xff9800, past: 'Muted',     active: true  },
  KICK:     { emoji: '👢',  color: 0xff5722, past: 'Kicked',    active: false },
  SOFTBAN:  { emoji: '🔨',  color: 0xf44336, past: 'Softbanned',active: false },
  BAN:      { emoji: '🔨',  color: 0xd32f2f, past: 'Banned',    active: true  },
  UNBAN:    { emoji: '✅',  color: 0x57f287, past: 'Unbanned',  active: false },
  UNMUTE:   { emoji: '🔊',  color: 0x57f287, past: 'Unmuted',   active: false },
  NOTE:     { emoji: '📝',  color: 0x5865f2, past: 'Noted',     active: false },
};

// ── Internal helpers ──────────────────────────────────────────────────────────

async function getSettings(guildId) {
  const key = `guild:${guildId}:settings`;
  const hit = await cache.get(key);
  if (hit) return hit;
  const s = await prisma.guildSettings.findUnique({ where: { guildId } });
  if (s) await cache.set(key, s, 300);
  return s;
}

/**
 * Validates that the bot and executor can act on the target (role hierarchy).
 * @throws {Error} with a user-facing message if hierarchy is violated
 */
export function assertHierarchy(guild, targetMember, executorMember) {
  if (!targetMember) return; // User not in guild (e.g. banning by ID) — OK
  const botTop      = guild.members.me?.roles.highest.position ?? 0;
  const targetTop   = targetMember.roles.highest.position;
  if (targetTop >= botTop) {
    throw new Error("I can't moderate that user — they have a higher or equal role to me.");
  }
  if (executorMember && targetTop >= executorMember.roles.highest.position) {
    throw new Error("You can't moderate that user — they have a higher or equal role to you.");
  }
  if (targetMember.id === guild.members.me?.id) {
    throw new Error("I can't moderate myself.");
  }
}

// ── DM helper ─────────────────────────────────────────────────────────────────

async function dmTarget(user, guild, type, reason, durationMs) {
  const meta = ACTION_META[type];
  try {
    const embed = new EmbedBuilder()
      .setColor(meta.color)
      .setTitle(`${meta.emoji} You have been ${meta.past} in ${guild.name}`)
      .addFields({ name: 'Reason', value: reason || 'No reason provided' });
    if (durationMs) embed.addFields({ name: 'Duration', value: formatDuration(durationMs) });
    await user.send({ embeds: [embed] });
  } catch {
    // DMs disabled — silently continue
  }
}

// ── Mod log ───────────────────────────────────────────────────────────────────

export async function sendModLog(guild, infraction, target, moderator, client) {
  const settings = await getSettings(guild.id);
  if (!settings?.modLogChannelId) return;

  const channel = guild.channels.cache.get(settings.modLogChannelId)
    ?? await guild.channels.fetch(settings.modLogChannelId).catch(() => null);
  if (!channel?.isTextBased()) return;

  const meta = ACTION_META[infraction.type] ?? ACTION_META.NOTE;
  const embed = new EmbedBuilder()
    .setColor(meta.color)
    .setTitle(`${meta.emoji} ${infraction.type} — Case #${infraction.caseId}`)
    .setThumbnail(target.displayAvatarURL?.() ?? target.avatarURL?.() ?? null)
    .addFields(
      { name: 'Target',    value: `<@${infraction.targetUserId}> \`${target.tag ?? target.id}\``, inline: true },
      { name: 'Moderator', value: `<@${infraction.moderatorId}> \`${moderator.tag ?? moderator.id}\``, inline: true },
      { name: 'Reason',    value: infraction.reason || 'No reason provided' },
    )
    .setFooter({ text: `Case #${infraction.caseId} • ID: ${infraction.targetUserId}` })
    .setTimestamp(infraction.createdAt);

  if (infraction.expiresAt) {
    embed.addFields({ name: 'Expires', value: `<t:${Math.floor(infraction.expiresAt.getTime() / 1000)}:R>` });
  }
  if (infraction.proofUrl) {
    embed.addFields({ name: 'Proof', value: infraction.proofUrl });
  }

  await channel.send({ embeds: [embed] }).catch((err) =>
    logger.error(`Failed to send mod log to channel ${settings.modLogChannelId}`, err)
  );
}

// ── Escalation engine ─────────────────────────────────────────────────────────

async function checkEscalation(guild, userId, client) {
  const settings = await getSettings(guild.id);
  if (!settings?.infractionEscalation) return;

  const tiers = Array.isArray(settings.infractionEscalation) ? settings.infractionEscalation : [];
  if (tiers.length === 0) return;

  const warnCount = await prisma.infraction.count({
    where: { guildId: guild.id, targetUserId: userId, type: 'WARN', active: true },
  });

  const tier = [...tiers].sort((a, b) => b.warnCount - a.warnCount)
    .find((t) => warnCount >= t.warnCount);
  if (!tier) return;

  const botUser   = client.user;
  const member    = await guild.members.fetch(userId).catch(() => null);
  const durationMs = tier.duration ? tier.duration * 1_000 : null;

  try {
    if (tier.action === 'TIMEOUT' && member && durationMs) {
      await member.timeout(durationMs, `Auto-escalation: ${warnCount} warnings`);
      await createInfraction(guild, { id: userId }, botUser, 'TIMEOUT', {
        reason: `Escalation: reached ${warnCount} warnings`,
        durationMs,
        skipEscalation: true,
        client,
      });
    } else if (tier.action === 'KICK' && member) {
      await member.kick(`Auto-escalation: ${warnCount} warnings`);
      await createInfraction(guild, { id: userId }, botUser, 'KICK', {
        reason: `Escalation: reached ${warnCount} warnings`,
        skipEscalation: true,
        client,
      });
    } else if (tier.action === 'BAN') {
      await guild.bans.create(userId, { reason: `Auto-escalation: ${warnCount} warnings` });
      await createInfraction(guild, { id: userId }, botUser, 'BAN', {
        reason: `Escalation: reached ${warnCount} warnings`,
        skipEscalation: true,
        client,
      });
    }
    logger.info(`Escalation: applied ${tier.action} to ${userId} in ${guild.id} (${warnCount} warns)`);
  } catch (err) {
    logger.error(`Escalation action failed for ${userId} in ${guild.id}`, err);
  }
}

// ── Core infraction factory ───────────────────────────────────────────────────

/**
 * Creates a database infraction record in a transaction (atomic case ID).
 * Does NOT execute any Discord API action — callers handle that.
 *
 * @param {import('discord.js').Guild} guild
 * @param {{ id: string }} target       - User object or plain { id }
 * @param {{ id: string, tag?: string }} moderator
 * @param {string} type                  - InfractionType enum value
 * @param {object} [opts]
 * @param {string}  [opts.reason]
 * @param {string}  [opts.proofUrl]
 * @param {number}  [opts.durationMs]    - Duration in ms (for timeouts/temp bans)
 * @param {boolean} [opts.skipEscalation] - Internal flag for escalation-spawned actions
 * @param {boolean} [opts.silent]        - Skip DM
 * @param {import('discord.js').Client} [opts.client] - Required for modlog + escalation
 * @returns {Promise<import('@prisma/client').Infraction>}
 */
export async function createInfraction(guild, target, moderator, type, opts = {}) {
  const { reason, proofUrl, durationMs, skipEscalation, silent, client } = opts;
  const meta = ACTION_META[type] ?? ACTION_META.NOTE;

  const expiresAt = durationMs ? new Date(Date.now() + durationMs) : null;

  // Guarantee the FK parent row exists — guilds that were already present
  // when the bot was deployed may not have gone through guildCreate.
  await prisma.guildSettings.upsert({
    where:  { guildId: guild.id },
    update: {},
    create: { guildId: guild.id },
  });

  const infraction = await prisma.$transaction(async (tx) => {
    const agg = await tx.infraction.aggregate({
      where: { guildId: guild.id },
      _max:  { caseId: true },
    });
    const caseId = (agg._max.caseId ?? 0) + 1;

    return tx.infraction.create({
      data: {
        caseId,
        guildId:      guild.id,
        targetUserId: target.id,
        moderatorId:  moderator.id,
        type,
        reason:    reason    ?? 'No reason provided',
        proofUrl:  proofUrl  ?? null,
        active:    meta.active,
        expiresAt,
      },
    });
  });

  // DM the target (best-effort)
  if (!silent && target.send) {
    await dmTarget(target, guild, type, infraction.reason, durationMs);
  } else if (!silent && target.id) {
    const user = await guild.client.users.fetch(target.id).catch(() => null);
    if (user) await dmTarget(user, guild, type, infraction.reason, durationMs);
  }

  // Send to mod log channel
  if (client) {
    const resolvedTarget = target.tag ? target : (await client.users.fetch(target.id).catch(() => ({ id: target.id })));
    await sendModLog(guild, infraction, resolvedTarget, moderator, client);
  }

  // Check escalation on warns (skip if this IS an escalation action)
  if (type === 'WARN' && !skipEscalation && client) {
    await checkEscalation(guild, target.id, client);
  }

  return infraction;
}

/**
 * Resolves active infractions of matching type and marks them inactive.
 * Used by /unban and /unmute.
 */
export async function resolveInfractions(guildId, userId, types) {
  await prisma.infraction.updateMany({
    where: { guildId, targetUserId: userId, type: { in: types }, active: true },
    data:  { active: false },
  });
}

/**
 * Imports bans that exist in Discord but have no active BAN infraction in the
 * DB. Called on guildCreate and at startup so pre-bot bans appear in history.
 *
 * @param {import('discord.js').Guild} guild
 * @param {string} botClientId  - client.user.id, used as the moderator ID on synthetic records
 */
export async function syncGuildBans(guild, botClientId) {
  let bans;
  try {
    bans = await guild.bans.fetch();
  } catch {
    // Missing BAN_MEMBERS permission — nothing we can do
    return;
  }
  if (bans.size === 0) return;

  // Find users who already have an active BAN infraction so we don't duplicate
  const existing = await prisma.infraction.findMany({
    where:  { guildId: guild.id, type: 'BAN', active: true },
    select: { targetUserId: true },
  });
  const knownIds = new Set(existing.map((r) => r.targetUserId));

  const newBans = [...bans.values()].filter((b) => !knownIds.has(b.user.id));
  if (newBans.length === 0) return;

  // Allocate sequential case IDs starting after the current max
  const agg = await prisma.infraction.aggregate({
    where: { guildId: guild.id },
    _max:  { caseId: true },
  });
  let nextCaseId = (agg._max.caseId ?? 0) + 1;

  await prisma.infraction.createMany({
    data: newBans.map((ban) => ({
      caseId:       nextCaseId++,
      guildId:      guild.id,
      targetUserId: ban.user.id,
      moderatorId:  botClientId,
      type:         'BAN',
      reason:       ban.reason ? `[Imported] ${ban.reason}` : '[Imported] Pre-bot ban — reason unknown',
      active:       true,
    })),
  });

  logger.info(`Synced ${newBans.length} pre-existing ban(s) for guild "${guild.name}" (${guild.id}).`);
}
