import { PrismaClient } from '@prisma/client';
import logger from '../utils/logger.js';

const isProduction = process.env.NODE_ENV === 'production';

const prisma = new PrismaClient({
  log: isProduction
    ? ['warn', 'error']
    : ['query', 'info', 'warn', 'error'],
});

// Graceful shutdown — prevents dangling connections on PM2 reload
process.on('SIGTERM', async () => {
  logger.info('SIGTERM received — disconnecting Prisma...');
  await prisma.$disconnect();
  process.exit(0);
});

export default prisma;
