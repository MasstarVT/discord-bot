// The JS side of feat-events' cross-language tests: the JS bot's own eventService and rolePersistService against the
// test database (TEST_DATABASE_URL / TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-events-js.mjs create <guildId> <json data>        eventService.createEventRow (staff)
//   node rust/fixtures/gen/feat-events-js.mjs rsvp <eventId> <guildId> <userId> <status>   eventService.applyRsvp
//   node rust/fixtures/gen/feat-events-js.mjs grant <eventId> <userId>              EventRsvp.roleGranted = true
//   node rust/fixtures/gen/feat-events-js.mjs tally <eventId>                       eventService.getTally
//   node rust/fixtures/gen/feat-events-js.mjs schedule <eventId>                    eventService.scheduleEventJobs
//   node rust/fixtures/gen/feat-events-js.mjs close <eventId> <guildId> <status>    eventService.closeEvent (no guild cached)
//   node rust/fixtures/gen/feat-events-js.mjs find <eventId>                        the GuildEvent row with its RSVPs
//   node rust/fixtures/gen/feat-events-js.mjs list <guildId>                        eventService.listOpenEvents
//   node rust/fixtures/gen/feat-events-js.mjs ref <guildId> <raw>                   eventService.resolveEventRef
//   node rust/fixtures/gen/feat-events-js.mjs auto <guildId> <sub> <q> <hostId|->   eventService.findEventsForAutocomplete
//   node rust/fixtures/gen/feat-events-js.mjs jobs <guildId>                        the guild's ScheduledJob rows
//   node rust/fixtures/gen/feat-events-js.mjs rpSave <guildId> <userId> <json snap> <leftAt ISO>  rolePersistService.saveRow
//   node rust/fixtures/gen/feat-events-js.mjs rpLoad <guildId> <userId>            rolePersistService.loadRow
//   node rust/fixtures/gen/feat-events-js.mjs rpSkips <guildId> <userId> <json roleIds> <json settings>  lifecycleSkips
//   node rust/fixtures/gen/feat-events-js.mjs mute <guildId> <userId> <expiresAt ISO|->   an active MUTE infraction
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
const quiet = () => {};
console.error = console.warn = console.log = console.debug = console.info = quiet;

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const svc = await imp('src', 'services', 'eventService.js');
const rp = await imp('src', 'services', 'rolePersistService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const reviveDates = (o) => {
  for (const k of ['startsAt', 'endsAt']) if (typeof o[k] === 'string') o[k] = new Date(o[k]);
  return o;
};
const client = { guilds: { cache: new Map() }, channels: { cache: new Map() }, user: { id: '100000000000000002' } };

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'create') {
    const data = reviveDates(JSON.parse(args[1]));
    answer(await svc.createEventRow(args[0], data, { hostMax: null }));
  } else if (cmd === 'rsvp') {
    answer(await svc.applyRsvp({ eventId: args[0], guildId: args[1], userId: args[2], status: args[3] }));
  } else if (cmd === 'grant') {
    answer(await prisma.eventRsvp.updateMany({ where: { eventId: args[0], userId: args[1] }, data: { roleGranted: true } }));
  } else if (cmd === 'tally') {
    answer(await svc.getTally(args[0]));
  } else if (cmd === 'schedule') {
    const event = await prisma.guildEvent.findUnique({ where: { id: args[0] } });
    answer(await svc.scheduleEventJobs(event));
  } else if (cmd === 'close') {
    const res = await svc.closeEvent(client, args[0], { status: args[2], reason: 'manual', guildId: args[1] });
    answer({ ok: res.ok, code: res.code ?? null, status: res.event?.status ?? null });
  } else if (cmd === 'find') {
    const row = await prisma.guildEvent.findUnique({ where: { id: args[0] }, include: { rsvps: { orderBy: { userId: 'asc' } } } });
    answer(row);
  } else if (cmd === 'list') {
    const { events, going } = await svc.listOpenEvents(args[0]);
    answer({ ids: events.map((e) => e.id), going: Object.fromEntries(going) });
  } else if (cmd === 'ref') {
    answer((await svc.resolveEventRef(args[0], args[1]))?.id ?? null);
  } else if (cmd === 'auto') {
    const rows = await svc.findEventsForAutocomplete({ guildId: args[0], sub: args[1], q: args[2], hostId: args[3] === '-' ? null : args[3] });
    answer(rows.map((r) => r.id));
  } else if (cmd === 'jobs') {
    const rows = await prisma.scheduledJob.findMany({ where: { guildId: args[0] }, orderBy: { dedupeKey: 'asc' } });
    answer(rows.map((r) => ({ type: r.type, dedupeKey: r.dedupeKey, payload: r.payload, runAt: r.runAt })));
  } else if (cmd === 'rpSave') {
    await rp.saveRow(args[0], args[1], JSON.parse(args[2]), new Date(args[3]), null);
    answer(await rp.loadRow(args[0], args[1]));
  } else if (cmd === 'rpLoad') {
    answer(await rp.loadRow(args[0], args[1]));
  } else if (cmd === 'rpSkips') {
    const out = await rp.lifecycleSkips(args[0], args[1], JSON.parse(args[2]), JSON.parse(args[3]));
    answer({ skips: Object.fromEntries(out.skips), markers: [...out.markers].sort() });
  } else if (cmd === 'mute') {
    const last = await prisma.infraction.findFirst({ where: { guildId: args[0] }, orderBy: { caseId: 'desc' } });
    answer(await prisma.infraction.create({
      data: {
        guildId: args[0], caseId: (last?.caseId ?? 0) + 1, targetUserId: args[1], moderatorId: '600000000000000999', type: 'MUTE',
        active: true, expiresAt: args[2] === '-' ? null : new Date(args[2]),
      },
    }));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
