// Who added the bot to each guild (BotJoin), for the bot owner's dashboard. The bot records it on guildReady
// (src/plugins/bot-joins.js); the dashboard reads it with listBotJoins.
import { AuditLogEvent, PermissionFlagsBits } from 'discord.js';
import prisma from '../database/client.js';
import { ensureGuildSettings } from './settingsService.js';
import { sleep } from '../utils/async.js';
import logger from '../utils/logger.js';

/** Audit-log entries lag the gateway event a little. */
const AUDIT_LAG_MS = 1500;

const UNKNOWN = Object.freeze({ inviterId: null, inviterName: null, source: null });

/**
 * Finds who added the bot: the newest Bot Add audit-log entry for it (needs View Audit Log; Discord keeps entries
 * about 45 days), else the server's integration for the bot (needs Manage Server; kept while the bot is there).
 * Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {string} botId the bot's user id
 * @param {string} [appId] the bot's application id (the same as botId for every bot made since 2017)
 * @returns {Promise<{ inviterId: string|null, inviterName: string|null, source: 'audit'|'integration'|null }>}
 */
export async function findInviter(guild, botId, appId = botId) {
  try {
    const perms = guild.members.me?.permissions;
    if (perms?.has(PermissionFlagsBits.ViewAuditLog)) {
      const logs = await guild.fetchAuditLogs({ type: AuditLogEvent.BotAdd, limit: 10 }).catch(() => null);
      // Newest first, so a re-add finds the latest inviter.
      const entry = logs?.entries.find((e) => e.targetId === botId && e.executorId);
      if (entry) return { inviterId: entry.executorId, inviterName: entry.executor?.tag ?? null, source: 'audit' };
    }
    if (perms?.has(PermissionFlagsBits.ManageGuild)) {
      const integrations = await guild.fetchIntegrations().catch(() => null);
      const mine = integrations?.find((i) => i.user && (i.application?.id === appId || i.account?.id === botId));
      if (mine) return { inviterId: mine.user.id, inviterName: mine.user.tag ?? null, source: 'integration' };
    }
  } catch (err) {
    logger.warn(`Could not look up who added the bot to guild ${guild?.id}: ${err?.message ?? err}`);
  }
  return UNKNOWN;
}

/**
 * guildReady hook body. On a join it records who added the bot, replacing any older row (a re-add); on startup it
 * looks again only where the inviter is not known yet (no row, or the bot lacked the permissions), so a restart does
 * not query every server again. Never throws.
 * @param {{ client: import('discord.js').Client, guild: import('discord.js').Guild, reason: string }} ctx
 * @returns {Promise<void>}
 */
export async function recordBotJoin({ client, guild, reason }) {
  try {
    const botId = client.user?.id;
    if (!botId) return;
    if (reason === 'join') {
      await sleep(AUDIT_LAG_MS);
    } else {
      const row = await prisma.botJoin.findUnique({ where: { guildId: guild.id }, select: { inviterId: true } });
      if (row?.inviterId) return;
    }
    const found = await findInviter(guild, botId, client.application?.id ?? botId);
    const data = { ...found, joinedAt: guild.joinedAt ?? new Date() };
    await ensureGuildSettings(guild.id);
    await prisma.botJoin.upsert({ where: { guildId: guild.id }, create: { guildId: guild.id, ...data }, update: data });
  } catch (err) {
    logger.warn(`Could not record who added the bot to guild ${guild?.id}: ${err?.message ?? err}`);
  }
}

/**
 * The stored rows for these guilds, by guild id. Throws on a database error (the dashboard leaves the field out).
 * @param {string[]} guildIds
 * @returns {Promise<Map<string, { inviterId: string|null, inviterName: string|null, source: string|null, joinedAt: Date }>>}
 */
export async function listBotJoins(guildIds) {
  if (!guildIds.length) return new Map();
  const rows = await prisma.botJoin.findMany({
    where: { guildId: { in: guildIds } },
    select: { guildId: true, inviterId: true, inviterName: true, source: true, joinedAt: true },
  });
  return new Map(rows.map(({ guildId, ...row }) => [guildId, row]));
}
