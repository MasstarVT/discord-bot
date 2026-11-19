// The JS side of feat-dashboard-tasks' cross-language tests: the JS dashboard's task runner (src/dashboard/tasks.js),
// the JS bot's task runner (src/services/dashboardTasks/index.js) and the services whose rows the tasks change, against
// the test database and Redis (TEST_DATABASE_URL / TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   queue <guildId> <kind> <actorId> <json payload>         scheduleJob exactly as createTaskRunner().run queues a task
//   outcome <guildId> <jobId> <kind> <actorId>              what the dashboard's runner reports for that job now
//   runner <guildId> <kind> <actorId> <json payload> <ms>   the whole dashboard runner: queue, wait up to <ms>, report
//   execute <jobId> <botOwnerId>                            the JS bot runs a queued dashboard.task row (runDashboardTask
//                                                           with a minimal guild), then reads the Redis result it stored
//   job <dedupeKey>                                         the ScheduledJob row of a dedupe key, as Prisma reads it
//   jobById <id>                                            the ScheduledJob row of an id
//   warn <guildId> <targetId> <moderatorId> <reason>        moderationService.createInfraction(WARN, silent)
//   cases <guildId> <targetId>                              the target's Infraction rows, oldest first
//   xpset <guildId> <userId> <xp>                           xpService.setXp
//   xp <guildId> <userId>                                   the UserLevel row
//   gstart <guildId> <channelId> <prize>                    giveawayService.startGiveaway with a stub channel
//   giveaway <id>                                           the Giveaway row and its end job
//   pstart <guildId> <channelId> <question>                 pollService.postPoll with a stub channel
//   poll <id>                                               the Poll row
//   rr <guildId> <groupId>                                  the ReactionRoleGroup row
//   ticketPanel <guildId> <panelId>                         the TicketPanel row
//   managersync <guildId> <panelId> <json roles>            queues ticket.managersync as tickets.js ticket.refresh does
import { randomUUID } from 'node:crypto';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
const [cmd, ...args] = process.argv.slice(2);
if (cmd === 'execute') process.env.BOT_OWNER_ID = args[1];
console.error = console.warn = console.log = console.debug = console.info = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);
const { ChannelType } = await imp('node_modules', 'discord.js', 'src', 'index.js');
const shared = await imp('src', 'services', 'dashboardTasks', 'shared.js');
const { createTaskRunner } = await imp('src', 'dashboard', 'tasks.js');
const { scheduleJob, getJob } = await imp('src', 'services', 'scheduler.js');
const { cache } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const quiet = { info() {}, warn() {}, error() {}, debug() {} };
const pickJob = (j) => (j ? {
  id: j.id, guildId: j.guildId, type: j.type, dedupeKey: j.dedupeKey, payload: j.payload, status: j.status, attempts: j.attempts,
  maxAttempts: j.maxAttempts, lastError: j.lastError,
} : null);
let seq = 0;
const nextId = () => `7${String(Date.now()).slice(-9)}${String(++seq).padStart(8, '0')}`;

function stubGuild(id) {
  return {
    id, name: 'Test', available: true,
    roles: { cache: new Map() },
    channels: { cache: new Map(), async fetch(cid) { throw Object.assign(new Error(`Unknown Channel ${cid}`), { code: 10003 }); } },
    members: { cache: new Map(), me: { id: '600000000000000999' }, async fetch() { throw Object.assign(new Error('Unknown Member'), { code: 10007 }); } },
  };
}

function stubChannel(guild, id) {
  const ch = {
    id, name: `chan-${id}`, type: ChannelType.GuildText, guild,
    isTextBased: () => true, isThread: () => false, permissionsFor: () => ({ has: () => true }), toString: () => `<#${id}>`,
    async send() {
      const mid = nextId();
      return { id: mid, url: `https://discord.com/channels/${guild.id}/${id}/${mid}`, async delete() {} };
    },
    messages: { async fetch(mid) { return { id: mid }; }, async edit(mid) { return { id: mid }; } },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

try {
  if (cmd === 'queue') {
    const [guildId, kind, actorId, raw] = args;
    // The call createTaskRunner().run makes (src/dashboard/tasks.js).
    const job = await scheduleJob({
      guildId, type: shared.TASK_JOB_TYPE, runAt: new Date(), payload: { kind, actorId, payload: JSON.parse(raw) ?? {} },
      dedupeKey: shared.taskDedupeKey(randomUUID()), maxAttempts: 1,
    });
    answer({ id: job.id, row: pickJob(await prisma.scheduledJob.findUnique({ where: { id: job.id } })) });
  } else if (cmd === 'outcome') {
    const [guildId, jobId, kind, actorId] = args;
    const runner = createTaskRunner({ logger: quiet, waitMs: 0, pollMs: 1, sleep: async () => {}, schedule: async () => ({ id: jobId }) });
    answer(await runner.run(guildId, kind, {}, { actorId }));
  } else if (cmd === 'runner') {
    const [guildId, kind, actorId, raw, ms] = args;
    const runner = createTaskRunner({ logger: quiet, waitMs: Number(ms), pollMs: 100 });
    answer(await runner.run(guildId, kind, JSON.parse(raw), { actorId }));
  } else if (cmd === 'execute') {
    const { runDashboardTask } = await imp('src', 'services', 'dashboardTasks', 'index.js');
    const row = await prisma.scheduledJob.findUnique({ where: { id: args[0] } });
    let error = null;
    try {
      await runDashboardTask({ id: row.id, guildId: row.guildId, payload: row.payload }, { client: {}, guild: stubGuild(row.guildId) });
    } catch (err) {
      error = { name: err.name, message: err.message };
    }
    answer({ row: pickJob(row), error, stored: await cache.get(shared.taskResultKey(row.id)) });
  } else if (cmd === 'job') {
    answer(pickJob(await getJob(args[0])));
  } else if (cmd === 'jobById') {
    answer(pickJob(await prisma.scheduledJob.findUnique({ where: { id: args[0] } })));
  } else if (cmd === 'warn') {
    const [guildId, targetId, moderatorId, reason] = args;
    const mod = await imp('src', 'services', 'moderationService.js');
    const inf = await mod.createInfraction(stubGuild(guildId), { id: targetId }, { id: moderatorId }, 'WARN', {
      reason, silent: true, skipEscalation: true, source: 'command',
    });
    answer({ caseId: inf.caseId });
  } else if (cmd === 'cases') {
    const rows = await prisma.infraction.findMany({ where: { guildId: args[0], targetUserId: args[1] }, orderBy: { caseId: 'asc' } });
    answer(rows.map((r) => ({ caseId: r.caseId, type: r.type, reason: r.reason, proofUrl: r.proofUrl, active: r.active, moderatorId: r.moderatorId })));
  } else if (cmd === 'xpset') {
    const xp = await imp('src', 'services', 'xpService.js');
    answer(await xp.setXp({ guildId: args[0], userId: args[1], xp: Number(args[2]) }));
  } else if (cmd === 'xp') {
    answer(await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId: args[0], userId: args[1] } } }));
  } else if (cmd === 'gstart') {
    const [guildId, channelId, prize] = args;
    const gs = await imp('src', 'services', 'giveawayService.js');
    const guild = stubGuild(guildId);
    const res = await gs.startGiveaway({
      guild, channel: stubChannel(guild, channelId), hostId: '600000000000000001', prize, description: null, winnerCount: 1,
      endsAt: new Date(Date.now() + 3_600_000), requiredRoleIds: [], deniedRoleIds: [], roleWeights: [], xpPrize: null, coinPrize: null, settings: null,
    });
    if (!res.ok) throw new Error(`startGiveaway: ${res.code}`);
    answer({ id: res.giveaway.id, messageId: res.giveaway.messageId });
  } else if (cmd === 'giveaway') {
    const gs = await imp('src', 'services', 'giveawayService.js');
    const g = await prisma.giveaway.findUnique({ where: { id: args[0] } });
    answer({ giveaway: g, job: pickJob(await getJob(gs.endJobKey(args[0]))) });
  } else if (cmd === 'pstart') {
    const [guildId, channelId, question] = args;
    const ps = await imp('src', 'services', 'pollService.js');
    const guild = stubGuild(guildId);
    const pending = { guildId, channelId, userId: '600000000000000001', maxChoices: 1, allowedRoleIds: [], deniedRoleIds: [], hideResults: false, durationMs: null };
    const out = await ps.postPoll({ client: { guilds: { cache: new Map() } }, guild, channel: stubChannel(guild, channelId), pending, question, options: ['A', 'B'], maxChoices: 1 });
    if (!out.ok) throw new Error(`postPoll: ${out.code}`);
    answer({ id: out.poll.id, messageId: out.poll.messageId });
  } else if (cmd === 'poll') {
    answer(await prisma.poll.findUnique({ where: { id: args[0] } }));
  } else if (cmd === 'rr') {
    answer(await prisma.reactionRoleGroup.findFirst({ where: { guildId: args[0], id: args[1] }, include: { items: { orderBy: { position: 'asc' } } } }));
  } else if (cmd === 'ticketPanel') {
    answer(await prisma.ticketPanel.findFirst({ where: { guildId: args[0], id: args[1] } }));
  } else if (cmd === 'managersync') {
    const [guildId, panelId, raw] = args;
    const ts = await imp('src', 'services', 'ticketService.js');
    const job = await scheduleJob({
      guildId, type: 'ticket.managersync', runAt: new Date(), dedupeKey: `ticket.managersync:${guildId}`,
      payload: ts.managerSyncPayload(panelId, JSON.parse(raw)), maxAttempts: 3, onConflict: 'merge', merge: ts.mergeManagerSync,
    });
    answer(pickJob(await prisma.scheduledJob.findUnique({ where: { id: job.id } })));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
