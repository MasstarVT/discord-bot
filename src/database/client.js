import { PrismaClient } from '@prisma/client';

const isProduction = process.env.NODE_ENV === 'production';

const prisma = new PrismaClient({
  log: isProduction
    ? ['warn', 'error']
    : ['query', 'info', 'warn', 'error'],
});

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
