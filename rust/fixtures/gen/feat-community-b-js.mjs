// The JS side of feat-community-b's cross-language tests: the JS bot's own modmailService, boostService and
// autoThreadService against the test database (TEST_DATABASE_URL / TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   modmail
//     mm-reserve <guildId> <userId>            modmailService.reserveOpen
//     mm-attach <rowId> <threadId>             modmailService.attachThread
//     mm-close <rowId> <closedById> <reason>   modmailService.closeRow
//     mm-by-thread <threadId>                  modmailService.findByThread
//     mm-open-user <userId>                    modmailService.findOpenForUser
//     mm-open-guild <guildId> <userId>         modmailService.findOpenInGuild
//   boosts
//     bs-insert <guildId> <userId> <roleId>    the row createBoosterRole saves (prisma.boosterRole.create)
//     bs-end <guildId> <userId> <iso date>     boostService.markEnded
//     bs-clear <guildId> <userId>              boostService.clearEnded
//     bs-rows <guildId>                        boostService.listRows + countRows
//     bs-ensure <guildId>                      boostService.ensureSweepJob, then sweepJob
//     bs-job <guildId>                         boostService.sweepJob
//   auto threads
//     at-set <guildId> <json change>           autoThreadService.setThreadChannel
//     at-publish <guildId> <channelId>         autoThreadService.addPublishChannel
//     at-drop <guildId> <channelId>            autoThreadService.dropChannel
//     at-read <guildId>                        the stored lists through readEntries / readPublishIds (row read fresh)
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
for (const k of ['log', 'info', 'warn', 'debug']) console[k] = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const mm = await imp('src', 'services', 'modmailService.js');
const bs = await imp('src', 'services', 'boostService.js');
const at = await imp('src', 'services', 'autoThreadService.js');
const settings = await imp('src', 'services', 'settingsService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const [cmd, ...a] = process.argv.slice(2);
try {
  switch (cmd) {
    case 'mm-reserve': answer(await mm.reserveOpen(a[0], a[1])); break;
    case 'mm-attach': answer(await mm.attachThread(a[0], a[1])); break;
    case 'mm-close': answer(await mm.closeRow(a[0], { closedById: a[1] || null, reason: a[2] || null })); break;
    case 'mm-by-thread': answer(await mm.findByThread(a[0])); break;
    case 'mm-open-user': answer(await mm.findOpenForUser(a[0])); break;
    case 'mm-open-guild': answer(await mm.findOpenInGuild(a[0], a[1])); break;
    case 'bs-insert':
      await settings.ensureGuildSettings(a[0]);
      answer(await prisma.boosterRole.create({ data: { guildId: a[0], userId: a[1], roleId: a[2] } }));
      break;
    case 'bs-end': answer(await bs.markEnded(a[0], a[1], new Date(a[2]))); break;
    case 'bs-clear': answer(await bs.clearEnded(a[0], a[1])); break;
    case 'bs-rows': answer({ rows: await bs.listRows(a[0]), count: await bs.countRows(a[0]), one: await bs.loadRow(a[0], a[1] ?? '') }); break;
    case 'bs-ensure': answer({ ok: await bs.ensureSweepJob(a[0]), job: await bs.sweepJob(a[0]) }); break;
    case 'bs-job': answer(await bs.sweepJob(a[0])); break;
    case 'at-set': {
      const r = await at.setThreadChannel(a[0], JSON.parse(a[1]));
      answer({ created: r.created, entry: r.entry, stored: r.settings.autoThreadChannels });
      break;
    }
    case 'at-publish': {
      const r = await at.addPublishChannel(a[0], a[1]);
      answer({ added: r.added, stored: r.settings.autoPublishChannelIds });
      break;
    }
    case 'at-drop': answer(await at.dropChannel(a[0], a[1])); break;
    case 'at-read': {
      const row = await prisma.guildSettings.findUnique({ where: { guildId: a[0] } });
      answer({ entries: at.readEntries(row), publish: at.readPublishIds(row), raw: row?.autoThreadChannels ?? null, ids: row?.autoPublishChannelIds ?? null });
      break;
    }
    default: throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
