import { Prisma, PrismaClient } from '@prisma/client';

// SQL query logging is opt-in (R258): NODE_ENV no longer controls it, so an unset or copied
// NODE_ENV=development stays quiet unless PRISMA_LOG_QUERIES=1.
const prisma = new PrismaClient({
  log: process.env.PRISMA_LOG_QUERIES === '1'
    ? ['query', 'info', 'warn', 'error']
    : ['warn', 'error'],
});

// ── Raw SQL time helpers (00-overview rule 22) ───────────────────────────────
// Prisma DateTime columns are timestamp(3) WITHOUT time zone and hold UTC; a bare NOW call is
// timestamptz and is converted with the session TimeZone, which Prisma never sets.

/** UTC "now" as timestamp WITHOUT time zone — matches Prisma DateTime columns whatever the session TimeZone is. */
export const SQL_NOW = Prisma.sql`(now() AT TIME ZONE 'UTC')`;

/**
 * A JS Date as a UTC timestamp-without-time-zone SQL fragment.
 * @param {Date} date
 * @returns {Prisma.Sql}
 * @throws {TypeError} for anything but a valid Date
 */
export function sqlUtc(date) {
  if (!(date instanceof Date) || Number.isNaN(date.getTime())) throw new TypeError('sqlUtc expects a valid Date');
  return Prisma.sql`(${date.toISOString()}::timestamptz AT TIME ZONE 'UTC')`;
}

/**
 * Closes the Prisma connection pool. Shutdown is coordinated by the single
 * SIGTERM/SIGINT handler in index.js (which reaches each shard through
 * client.shutdown() in src/bot.js), so this module registers no signal
 * handlers of its own.
 */
export async function disconnect() {
  await prisma.$disconnect();
}

export default prisma;
