// Translation activity per channel (the TranslateStat table), shown on the dashboard's Auto-Translate page: how many
// messages were translated, the languages their authors wrote in (as the provider detected them) and the languages they
// were translated into. Auto-translate replies and /translate in a server count; DMs never do. Counts only, never text.
import { Prisma } from '@prisma/client';
import prisma, { SQL_NOW } from '../database/client.js';
import { ensureGuildSettings } from './settingsService.js';
import logger from '../utils/logger.js';

/** How a message was translated: an auto-translate reply or the /translate command. */
export const STAT_VIA = Object.freeze({ AUTO: 'auto', COMMAND: 'command' });
/** `from` counts the language the author wrote in, `to` each language it was translated into. */
export const STAT_DIRECTION = Object.freeze({ FROM: 'from', TO: 'to' });
/** The `from` language when the provider detected none. */
export const UNKNOWN_LANG = 'und';

const INT4_MAX = 2_147_483_647;
const LANG_RE = /^[a-z]{2,3}(?:-[a-z0-9]{2,8})?$/;
const SNOWFLAKE_RE = /^\d{15,25}$/;
const WARN_EVERY_MS = 10 * 60_000;
let lastWarnAt = 0;

/**
 * The counters one translated message adds to: [direction, lang] pairs, without duplicates and in a fixed order (two
 * concurrent upserts then lock their shared rows in the same order).
 * @param {string|null} detected canonical code, or null
 * @param {string[]} targets canonical codes of the translations that were shown
 * @returns {Array<[string, string]>}
 */
export function statKeys(detected, targets) {
  const from = typeof detected === 'string' && LANG_RE.test(detected) ? detected : UNKNOWN_LANG;
  const to = [...new Set((Array.isArray(targets) ? targets : []).filter((t) => typeof t === 'string' && LANG_RE.test(t)))].sort();
  return [[STAT_DIRECTION.FROM, from], ...to.map((lang) => [STAT_DIRECTION.TO, lang])];
}

function recordSql({ guildId, channelId, via, keys }) {
  const rows = keys.map(([direction, lang]) => Prisma.sql`(${guildId}, ${channelId}, ${via}, ${direction}, ${lang}, 1, ${SQL_NOW}, ${SQL_NOW})`);
  return Prisma.sql`
    INSERT INTO "TranslateStat" ("guildId", "channelId", "via", "direction", "lang", "count", "firstAt", "lastAt")
    VALUES ${Prisma.join(rows)}
    ON CONFLICT ("guildId", "channelId", "via", "direction", "lang") DO UPDATE
    SET "count" = LEAST("TranslateStat"."count"::bigint + 1, ${INT4_MAX})::int, "lastAt" = EXCLUDED."lastAt"`;
}

export const RAW_SQL = Object.freeze([
  {
    name: 'translateStats.record',
    build: () => recordSql({ guildId: '0', channelId: '0', via: STAT_VIA.AUTO, keys: statKeys('es', ['ja', 'en']) }),
  },
]);

/**
 * Counts one translated message. Never throws or rejects, so callers may leave it unawaited.
 * @param {{ guildId: string, channelId: string, via: string, detected: string|null, targets: string[] }} entry
 * @returns {Promise<void>}
 */
export async function recordTranslation({ guildId, channelId, via, detected, targets }) {
  try {
    if (!SNOWFLAKE_RE.test(String(guildId)) || !SNOWFLAKE_RE.test(String(channelId))) return;
    if (via !== STAT_VIA.AUTO && via !== STAT_VIA.COMMAND) return;
    await ensureGuildSettings(guildId);
    await prisma.$executeRaw(recordSql({ guildId, channelId, via, keys: statKeys(detected, targets) }));
  } catch (err) {
    if (Date.now() - lastWarnAt >= WARN_EVERY_MS) {
      lastWarnAt = Date.now();
      logger.warn(`translateStats: could not count a translation in ${guildId}: ${err?.message ?? err}`);
    }
  }
}
