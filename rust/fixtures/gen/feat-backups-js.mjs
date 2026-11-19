// The JS side of feat-backups' cross-language tests: the JS bot's own backup service against the test servers
// (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-backups-js.mjs create <guildId> <name>      createBackup of the ORIGINAL fake guild
//   node rust/fixtures/gen/feat-backups-js.mjs read <guildId> <backupId>    getBackup + parseSnapshot + a JS restore (fake guild "empty")
//   node rust/fixtures/gen/feat-backups-js.mjs start <guildId> <backupId>   startRestore (fake guild "empty"), then one batch saved as the job payload
//   node rust/fixtures/gen/feat-backups-js.mjs job <guildId>                getRestoreJob + normaliseRestoreState
//   node rust/fixtures/gen/feat-backups-js.mjs lists <guildId> <query>      listBackups, searchBackups, countBackups, newestAutomatic
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL;
process.env.REDIS_URL = process.env.TEST_REDIS_URL;
const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value, (_, x) => (typeof x === 'bigint' ? x.toString() : x))}\n`);

const { default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));
const svc = await imp('src', 'services', 'backupService.js');
const snap = await imp('src', 'utils', 'guildSnapshot.js');
const scheduler = await imp('src', 'services', 'scheduler.js');
const { makeGuild, resetIds, ORIGINAL, BOT_PERMS } = await imp('rust', 'fixtures', 'gen', 'feat-backups-fake.mjs');
const EMPTY = { botPerms: BOT_PERMS, roles: [{ id: '999', name: 'Bot', pos: 10 }], members: ['777', '778'] };

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'create') {
    const res = await svc.createBackup({ guild: makeGuild({ ...ORIGINAL, id: args[0] }), createdById: 'U1', name: args[1] });
    const row = await prisma.serverBackup.findFirst({ where: { id: res.backup.id } });
    answer({ res, data: row.data });
  } else if (cmd === 'read') {
    const row = await svc.getBackup(args[0], args[1], { withData: true });
    const info = await svc.getBackup(args[0], args[1]);
    const snapshot = snap.parseSnapshot(row.data);
    resetIds();
    const guild = makeGuild({ ...EMPTY, id: snapshot.guild.id });
    let state = snap.newRestoreState({ backupId: row.id, mode: 'missing', now: Date.now() - 1000 });
    for (let i = 0; i < 40; i += 1) {
      state = snap.normaliseRestoreState(JSON.parse(JSON.stringify(state)));
      const env = snap.makeEnv({ guild, snapshot, state, reason: 'test' });
      if ((await snap.runRestoreBatch(env)) === 'finish') break;
    }
    answer({ info, parsed: snapshot, counts: state.counts, idMap: state.idMap, label: svc.backupLabel(info) });
  } else if (cmd === 'start') {
    resetIds();
    const guild = makeGuild({ ...EMPTY, id: args[0] });
    const row = await svc.getBackup(args[0], args[1], { withData: true });
    const res = await svc.startRestore({ guild, backupId: row.id, snapshot: snap.parseSnapshot(row.data), mode: 'missing', startedById: 'OWNER', channelId: 'CH' });
    const job = await svc.getRestoreJob(args[0]);
    answer({ res, job: { type: job.type, dedupeKey: job.dedupeKey, maxAttempts: job.maxAttempts, status: job.status, payload: job.payload } });
  } else if (cmd === 'job') {
    const job = await svc.getRestoreJob(args[0]);
    answer({ job: job && { type: job.type, dedupeKey: job.dedupeKey, maxAttempts: job.maxAttempts, status: job.status, payload: job.payload }, state: job && snap.normaliseRestoreState(job.payload) });
  } else if (cmd === 'lists') {
    const [list, search, counts, newest] = await Promise.all([svc.listBackups(args[0]), svc.searchBackups(args[0], args[1] ?? ''), svc.countBackups(args[0]), svc.newestAutomatic(args[0])]);
    answer({ list: list.map((r) => r.id), search: search.map((r) => r.id), counts, newest: newest?.id ?? null });
  } else {
    answer({ error: `unknown command ${cmd}` });
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err) });
} finally {
  await prisma.$disconnect().catch(() => {});
  redis.disconnect();
  process.exit(0);
}
