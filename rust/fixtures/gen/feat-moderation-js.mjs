// The JS side of feat-moderation's cross-language tests: the JS bot's own moderationService (and the pure helpers of
// the moderation commands) against the test servers (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>`
// line.
//
//   node rust/fixtures/gen/feat-moderation-js.mjs pure <json>                 pure helpers on the given inputs (no servers)
//   node rust/fixtures/gen/feat-moderation-js.mjs record <guildId> <json rows> recordInfraction for each row
//   node rust/fixtures/gen/feat-moderation-js.mjs reconcile <guildId>          reconcileExpiries({ id }) (writes ScheduledJob rows)
//   node rust/fixtures/gen/feat-moderation-js.mjs schedule-unban <guildId> <userId> <infractionId|null> <runAtMs>
//                                                                              the job executeModAction schedules for a TEMPBAN
//   node rust/fixtures/gen/feat-moderation-js.mjs schedule-expire <guildId> <infractionId> <roleId|null> <runAtMs>
//   node rust/fixtures/gen/feat-moderation-js.mjs read <guildId>               the guild's Infraction and ScheduledJob rows via Prisma
//   node rust/fixtures/gen/feat-moderation-js.mjs resolve <guildId> <userId>   resolveInfractions(guildId, userId, ['BAN'])
//   node rust/fixtures/gen/feat-moderation-js.mjs overdue <guildId>            countOverdueBans
//   node rust/fixtures/gen/feat-moderation-js.mjs settings-update <guildId> <json>  updateSettings
//   node rust/fixtures/gen/feat-moderation-js.mjs cache <op> <key>             cache.setNX(key, '1', 21600) / cache.get(key)
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const svc = await imp('src', 'services', 'moderationService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis, cache } = await imp('src', 'services', 'redis.js');

const [cmd, ...args] = process.argv.slice(2);

/** Every pure helper the Rust port must agree with, run on the given inputs. */
async function pure(input) {
  const purge = await imp('src', 'commands', 'moderation', 'purge.js');
  const slow = await imp('src', 'commands', 'moderation', 'slowmode.js');
  const out = {};
  out.effective = (input.effective ?? []).map((i) => svc.resolveEffectiveAction(i));
  out.tiers = (input.tiers ?? []).map((raw) => svc.normalizeEscalationTiers(raw));
  out.describe = (input.describe ?? []).map((t) => svc.describeTier(t));
  out.select = (input.select ?? []).map(({ tiers, warnTimes, prior, now }) => svc.selectEscalationTier(tiers, warnTimes, prior, now));
  out.audit = (input.audit ?? []).map(({ reason, moderator, source }) => svc.buildAuditReason(reason, moderator, source));
  out.mapErr = (input.mapErr ?? []).map((e) => svc.mapDiscordError(e));
  out.maxSnowflake = (input.maxSnowflake ?? []).map((ids) => svc.maxSnowflake(ids));
  out.modLog = (input.modLog ?? []).map(({ infraction, target, moderator, source, logFields }) => {
    const inf = { ...infraction, expiresAt: infraction.expiresAt ? new Date(infraction.expiresAt) : null, createdAt: new Date(infraction.createdAt) };
    const t = target ? { ...target, displayAvatarURL: target.avatar ? () => target.avatar : undefined } : target;
    return svc.buildModLogEmbed({ infraction: inf, target: t, moderator, source, logFields }).toJSON();
  });
  out.slowmode = (input.slowmode ?? []).map((s) => slow.parseSlowmode(s));
  out.formatSlowmode = (input.formatSlowmode ?? []).map((s) => slow.formatSlowmode(s));
  out.slowmodeEmbed = (input.slowmodeEmbed ?? []).map((a) => { const j = slow.buildSlowmodeEmbed(a).toJSON(); delete j.timestamp; return j; });
  out.purgeReport = (input.purgeReport ?? []).map((r) => { const j = purge.buildPurgeReport(r).toJSON(); delete j.timestamp; return j; });
  out.filters = (input.filters ?? []).map(({ message, opts }) => purge.matchesFilters(
    { ...message, attachments: { size: message.attachments ?? 0 } }, opts,
  ));
  return out;
}

try {
  if (cmd === 'pure') {
    answer(await pure(JSON.parse(args[0])));
  } else if (cmd === 'record') {
    const rows = JSON.parse(args[1]);
    const out = [];
    for (const r of rows) {
      out.push(await svc.recordInfraction({ ...r, guildId: args[0], expiresAt: r.expiresAt ? new Date(r.expiresAt) : null }));
    }
    answer(out);
  } else if (cmd === 'reconcile') {
    answer(await svc.reconcileExpiries({ id: args[0] }));
  } else if (cmd === 'schedule-unban' || cmd === 'schedule-expire') {
    const scheduler = await imp('src', 'services', 'scheduler.js');
    const [guildId, a, b, runAtMs] = args;
    const job = cmd === 'schedule-unban'
      ? { type: 'mod.unban', payload: { userId: a, infractionId: b === 'null' ? null : Number(b) }, dedupeKey: `mod.unban:${guildId}:${a}` }
      : { type: 'mod.expire', payload: { infractionId: Number(a), ...(b === 'null' ? {} : { roleId: b }) }, dedupeKey: `mod.expire:${a}` };
    const res = await scheduler.scheduleJob({ ...job, guildId, runAt: new Date(Number(runAtMs)), maxAttempts: 20, onConflict: 'replace' });
    answer({ id: res.id });
  } else if (cmd === 'read') {
    const infractions = await prisma.infraction.findMany({ where: { guildId: args[0] }, orderBy: { caseId: 'asc' } });
    const jobs = await prisma.scheduledJob.findMany({ where: { guildId: args[0] }, orderBy: { dedupeKey: 'asc' } });
    answer({ infractions, jobs });
  } else if (cmd === 'resolve') {
    answer(await svc.resolveInfractions(args[0], args[1], ['BAN']));
  } else if (cmd === 'overdue') {
    answer(await svc.countOverdueBans(args[0]));
  } else if (cmd === 'settings-update') {
    const settings = await imp('src', 'services', 'settingsService.js');
    answer(await settings.updateSettings(args[0], JSON.parse(args[1])));
  } else if (cmd === 'cache') {
    await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));
    answer(args[0] === 'setnx' ? await cache.setNX(args[1], '1', 21_600) : await cache.get(args[1]));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
