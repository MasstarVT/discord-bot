// The JS side of feat-tempvoice's cross-language tests: the JS bot's own tempVoiceService, voiceRoleService,
// scheduler, settings service and cache against the test servers (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one
// `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-tempvoice-js.mjs seed <guildId> <hubChannelId> <tempChannelId> <ownerId> <roleId>
//       createHub + updateHubRoleList + the TempVoiceChannel row createForMember writes + the keep-alive job onLeaveTemp
//       schedules + rename stamps in Redis + addLink / addAnyRole
//   node rust/fixtures/gen/feat-tempvoice-js.mjs read <guildId> <hubChannelId> <tempChannelId>
//       what the JS reads back: findHubByChannel, listHubs, the row with its hub, the job, the stamps, the voice roles
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
for (const k of ['log', 'info', 'warn', 'debug']) console[k] = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value, (_k, v) => (typeof v === 'bigint' ? v.toString() : v))}\n`);

const tv = await imp('src', 'services', 'tempVoiceService.js');
const vr = await imp('src', 'services', 'voiceRoleService.js');
const { scheduleJob } = await imp('src', 'services', 'scheduler.js');
const { ensureGuildSettings, getSettings, invalidateSettings } = await imp('src', 'services', 'settingsService.js');
const { cache } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const waitRedis = async () => {
  for (let i = 0; i < 50 && redis.status !== 'ready'; i++) await new Promise((r) => setTimeout(r, 100));
};

const [cmd, ...args] = process.argv.slice(2);
try {
  await waitRedis();
  if (cmd === 'seed') {
    const [guildId, hubChannelId, tempChannelId, ownerId, roleId] = args;
    await ensureGuildSettings(guildId);
    const hub = await tv.createHub(guildId, { channelId: hubChannelId, nameFormat: '{username} room', keepAliveMinutes: 5, bitrate: 64000, createTextChannel: true });
    const roleList = await tv.updateHubRoleList(guildId, hub.id, 'moderatorRoleIds', 'add', roleId);
    // The row createForMember writes, then the ban and emptiness onLeaveTemp / runVoiceAction record.
    const row = await prisma.tempVoiceChannel.create({ data: { guildId, hubId: hub.id, channelId: tempChannelId, textChannelId: null, ownerId, index: 1 } });
    const emptySince = new Date();
    await prisma.tempVoiceChannel.update({ where: { channelId: tempChannelId }, data: { bannedUserIds: ['555555555555555555'], emptySince, ownerLeftAt: emptySince, locked: true } });
    await scheduleJob({
      guildId, type: tv.CLEANUP_JOB, runAt: new Date(Date.now() - 1000), payload: { channelId: tempChannelId },
      dedupeKey: tv.cleanupKey(tempChannelId), maxAttempts: 5, onConflict: 'replace',
    });
    const stamps = [Date.now() - 2000, Date.now() - 1000];
    await cache.set(tv.renameKey(tempChannelId), stamps, 600);
    const link = await vr.addLink(guildId, hubChannelId, roleId);
    const any = await vr.addAnyRole(guildId, roleId);
    answer({ hub, roleList, row, stamps, link, any });
  } else if (cmd === 'read') {
    const [guildId, hubChannelId, tempChannelId] = args;
    await invalidateSettings(guildId);
    const hub = await tv.findHubByChannel(guildId, hubChannelId);
    const hubs = await tv.listHubs(guildId);
    const row = await prisma.tempVoiceChannel.findUnique({ where: { channelId: tempChannelId }, include: { hub: true } });
    const job = await prisma.scheduledJob.findFirst({ where: { dedupeKey: tv.cleanupKey(tempChannelId) } });
    const stamps = await cache.get(tv.renameKey(tempChannelId));
    const settings = await getSettings(guildId);
    answer({
      hub, hubs: hubs.map((h) => ({ id: h.id, channels: h._count.channels })), row,
      job: job && { type: job.type, payload: job.payload, dedupeKey: job.dedupeKey, maxAttempts: job.maxAttempts, guildId: job.guildId },
      stamps, links: vr.linksOf(settings), any: vr.anyRolesOf(settings), managed: [...vr.managedRoleIds(settings)],
      isTemp: await tv.isTempChannel(tempChannelId), count: await tv.countHubs(guildId),
    });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
