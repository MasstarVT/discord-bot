// Prints the SQL Prisma generates for the feeds/reminders queries whose shape is not obvious (port reference only).
import { PrismaClient } from '@prisma/client';
const prisma = new PrismaClient({ datasources: { db: { url: process.env.TEST_DATABASE_URL } }, log: [{ emit: 'event', level: 'query' }] });
const out = [];
prisma.$on('query', (e) => out.push(e.query.slice(0,60) + " PARAMS " + e.params));
const g = '900000000000000001';
await prisma.feed.findMany({ where: { guildId: g, displayName: { equals: 'a_b%', mode: 'insensitive' } }, take: 2 });
await prisma.feed.findMany({ where: { guildId: g, OR: [{ displayName: { contains: 'a_b%', mode: 'insensitive' } }, { source: { contains: 'q', mode: 'insensitive' } }] }, orderBy: { createdAt: 'asc' }, take: 25, select: { id: true, type: true, displayName: true, channelId: true, enabled: true } });
await prisma.feed.update({ where: { id: 'nope' }, data: { failureCount: { increment: 1 }, lastError: 'x', lastCheckedAt: new Date() } }).catch(() => null);
await prisma.reminder.updateMany({ where: { id: 'nope', enabled: true, OR: [{ lastSentAt: null }, { lastSentAt: { lt: new Date() } }] }, data: { lastSentAt: new Date() } });
await prisma.reminder.findMany({ where: { guildId: g, name: { contains: 'x_', mode: 'insensitive' }, enabled: true }, select: { name: true, enabled: true, mode: true }, orderBy: { name: 'asc' }, take: 25 });
await prisma.feed.updateMany({ where: { id: { in: ['a', 'b'] }, enabled: true }, data: { enabled: false, disabledReason: 'x' } });
console.log(out.join('\n---\n'));
await prisma.$disconnect();
