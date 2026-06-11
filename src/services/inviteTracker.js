import { cache } from './redis.js';
import logger from '../utils/logger.js';

const CACHE_TTL = 60 * 60; // 1 hour — refreshed on every join anyway

function cacheKey(guildId) {
  return `guild:${guildId}:invites`;
}

/**
 * Snapshots all current invites for a guild into Redis.
 * Silently skips if the bot lacks ManageGuild permission.
 *
 * @param {import('discord.js').Guild} guild
 */
export async function cacheGuildInvites(guild) {
  try {
    const invites  = await guild.invites.fetch();
    const snapshot = Object.fromEntries(invites.map((inv) => [inv.code, inv.uses ?? 0]));
    await cache.set(cacheKey(guild.id), snapshot, CACHE_TTL);
  } catch {
    // No MANAGE_GUILD permission or API error — invite tracking unavailable
  }
}

/**
 * Compares a pre-join invite snapshot with the current invite list to determine
 * which invite was used. Refreshes the cache after the comparison.
 *
 * @param {import('discord.js').Guild} guild
 * @returns {Promise<import('discord.js').Invite|null>}
 */
export async function getUsedInvite(guild) {
  const before = (await cache.get(cacheKey(guild.id))) ?? {};

  let current;
  try {
    current = await guild.invites.fetch();
  } catch {
    return null;
  }

  // Refresh cache with the post-join snapshot
  const updated = Object.fromEntries(current.map((inv) => [inv.code, inv.uses ?? 0]));
  await cache.set(cacheKey(guild.id), updated, CACHE_TTL);

  // Find the invite whose use count is now higher than our snapshot
  for (const [, invite] of current) {
    const prevUses = before[invite.code] ?? 0;
    if ((invite.uses ?? 0) > prevUses) return invite;
  }

  // Vanity URL join (no code in the invite list)
  if (guild.vanityURLCode) {
    try {
      const vanity = await guild.fetchVanityData();
      const prevUses = before[`__vanity__`] ?? 0;
      if (vanity.uses > prevUses) {
        await cache.set(cacheKey(guild.id), { ...updated, __vanity__: vanity.uses }, CACHE_TTL);
        return { code: guild.vanityURLCode, inviter: null, uses: vanity.uses };
      }
    } catch { /* vanity fetch not critical */ }
  }

  return null;
}
