// The dashboard's only path to GuildSettings: reads and writes go through settingsService (the single
// settings gateway), and writes repeat the side effects the matching slash commands have.
// Importing this file opens the Redis connection (through settingsService), so dashboard.js loads it
// with a dynamic import() only once the configuration is valid; tests inject fakes instead.
import { getSettings, updateSettings } from '../services/settingsService.js';
import { cache } from '../services/redis.js';

const voiceSessionKeys = (guildId) => `voice:start:${guildId}:*`;

/**
 * The guild's current settings row, bypassing this process's 30 s memo (the bot may have changed it).
 * @param {string} guildId
 * @returns {Promise<Readonly<object>>}
 * @throws when Postgres is unreachable
 */
export function readGuildSettings(guildId) {
  return getSettings(guildId, { fresh: true });
}

/**
 * Saves validated toggle changes (catalog.validateChanges) and mirrors the commands' side effects:
 * switching voice XP off (`/levelconfig voicexp enabled:false`) or the Levels module off
 * (`/levelconfig disable`) drops the open voice sessions without credit.
 * @param {string} guildId
 * @param {Record<string, boolean>} changes
 * @returns {Promise<Readonly<object>>} the row after the write
 * @throws on a database failure (nothing is saved)
 */
export async function writeToggles(guildId, changes) {
  const row = await updateSettings(guildId, changes);
  if (changes.voiceXpEnabled === false || changes.xpEnabled === false) {
    await cache.invalidate(voiceSessionKeys(guildId));
  }
  return row;
}
