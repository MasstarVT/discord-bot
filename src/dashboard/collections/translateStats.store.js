// Database access for the `translateStats` collection: reads the TranslateStat counters and the TranslateChannel rows of a
// server. Read-only; the bot writes the counters (src/services/translateStatsService.js).
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { RecordError } from './errors.js';
import { buildStatItems } from './translateStats.js';

/** Counter rows read per server (channels x languages x via x direction stays far below this). */
const MAX_ROWS = 20_000;
const STAT_SELECT = { channelId: true, via: true, direction: true, lang: true, count: true, firstAt: true, lastAt: true };
const READ_ONLY = 'Translation activity is counted by the bot and cannot be changed here.';

async function load(guildId, channelId) {
  const where = channelId ? { guildId, channelId } : { guildId };
  const [rows, configured] = await Promise.all([
    prisma.translateStat.findMany({ where, select: STAT_SELECT, take: MAX_ROWS }),
    prisma.translateChannel.findMany({ where, select: { channelId: true, targetLangs: true }, take: LIMITS.TRANSLATE_CHANNELS }),
  ]);
  return buildStatItems(rows, configured);
}

export const store = Object.freeze({
  list(guildId) {
    return load(guildId);
  },

  async get(guildId, id) {
    return (await load(guildId, id))[0] ?? null;
  },

  async create() { throw new RecordError('invalid', READ_ONLY); },
  async update() { throw new RecordError('invalid', READ_ONLY); },
  async remove() { throw new RecordError('invalid', READ_ONLY); },

  actions: Object.freeze({}),
});
