// Development seed — inserts a minimal GuildSettings row for local testing.
// Usage: npm run db:seed
import prisma from './client.js';
import logger from '../utils/logger.js';

const TEST_GUILD_ID = process.env.TEST_GUILD_ID || '000000000000000000';

async function main() {
  const existing = await prisma.guildSettings.findUnique({
    where: { guildId: TEST_GUILD_ID },
  });

  if (existing) {
    logger.info(`Seed skipped — GuildSettings already exists for ${TEST_GUILD_ID}`);
    return;
  }

  await prisma.guildSettings.create({
    data: { guildId: TEST_GUILD_ID },
  });

  logger.success(`Seeded GuildSettings for guild ${TEST_GUILD_ID}`);
}

main()
  .catch((e) => { logger.error('Seed failed', e); process.exit(1); })
  .finally(() => prisma.$disconnect());
