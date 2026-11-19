// The JS side of feat-feeds' cross-language tests: the JS bot's own feedService and reminderService against the test
// database and Redis (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   feed-create <guildId> <json>          createFeedRow(json) + scheduleFeedJob (+5 min); the row and its job
//   feed-read <guildId> <idOrName>        findFeedForGuild + getJob
//   cache-write <url> <type> <xml|json>   fetchFeedSource through a stub fetch answering the body: fills feed:fetch:<sha1>
//   cache-read <url> <type>               fetchFeedSource with a fetch that fails: only the shared cache can answer
//   reddit-block <until ISO>              cache.set(REDDIT_BLOCK_KEY, { until })
//   rem-create <guildId> <json>           createReminderRow + syncReminderJob; the row and its job
//   rem-read <guildId> <name>             findReminderByName + getJob
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);
const S = await imp('src', 'services', 'feedService.js');
const R = await imp('src', 'services', 'reminderService.js');
const { getJob } = await imp('src', 'services', 'scheduler.js');
const { cache, default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');

const revive = (o) => {
  for (const k of ['runAt', 'createdAt', 'lastSentAt', 'lastCheckedAt', 'lastSuccessAt']) if (typeof o[k] === 'string') o[k] = new Date(o[k]);
  if (Array.isArray(o.probe?.items)) o.probe.items = o.probe.items.map((i) => ({ ...i, publishedAt: i.publishedAt ? new Date(i.publishedAt) : null }));
  return o;
};
const jobJson = (j) => (j ? { type: j.type, status: j.status, dedupeKey: j.dedupeKey, payload: j.payload, maxAttempts: j.maxAttempts, runAt: j.runAt.toISOString() } : null);
const resp = (status, body, headers = {}) => {
  const buffer = Buffer.from(body);
  return { ok: status >= 200 && status < 300, status, statusText: '', headers, url: 'https://cache.test/', buffer, text: () => body, json: () => JSON.parse(body) };
};

// The client does not queue commands while connecting: wait until it is ready (bounded).
if (process.env.TEST_REDIS_URL && redis.status !== 'ready') {
  await new Promise((r) => { redis.once('ready', r); setTimeout(r, 3000); });
}

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'feed-create') {
    const row = await S.createFeedRow({ guildId: args[0], ...revive(JSON.parse(args[1])) });
    await S.scheduleFeedJob(row, { runAt: new Date(Date.now() + 5 * 60_000) });
    answer({ row, job: jobJson(await getJob(S.feedJobKey(row.id))) });
  } else if (cmd === 'feed-read') {
    const row = await S.findFeedForGuild(args[0], args[1]);
    answer({ row, job: row ? jobJson(await getJob(S.feedJobKey(row.id))) : null });
  } else if (cmd === 'cache-write') {
    S.__setFetchImpl({ safeFetch: async () => resp(200, args[2], { 'content-type': args[1] === 'BLUESKY' ? 'application/json' : 'application/rss+xml', etag: '"js"' }) });
    const r = await S.fetchFeedSource({ type: args[1], fetchUrl: args[0], initialized: false }, { useCache: false });
    answer({ ok: r.ok, fromCache: r.fromCache, title: r.title ?? null, items: (r.items ?? []).map((i) => ({ ...i, publishedAt: i.publishedAt?.toISOString() ?? null })), raw: await cache.get(S.fetchCacheKey(args[0])) });
  } else if (cmd === 'cache-read') {
    S.__setFetchImpl({ safeFetch: async () => { throw new Error('the network must not be used'); } });
    const r = await S.fetchFeedSource({ type: args[1], fetchUrl: args[0], initialized: false }, { useCache: true });
    answer({ ok: r.ok, fromCache: r.fromCache ?? false, title: r.title ?? null, image: r.image ?? null, items: (r.items ?? []).map((i) => ({ ...i, publishedAt: i.publishedAt?.toISOString() ?? null })), error: r.error ?? null });
  } else if (cmd === 'reddit-block') {
    answer({ ok: await cache.set(S.REDDIT_BLOCK_KEY, { until: args[0] }, 600) });
  } else if (cmd === 'rem-create') {
    const input = revive(JSON.parse(args[1]));
    const settings = await prisma.guildSettings.findUnique({ where: { guildId: args[0] } });
    const tz = R.resolveTimeZone(settings?.timezone);
    const now = new Date();
    const v = R.validateReminderInput(input.schedule, { tz, now });
    if (!v.ok) throw new Error(v.error);
    const row = await R.createReminderRow({ guildId: args[0], name: input.name, channelId: input.channelId, template: input.template, fields: v.fields, pingRoleId: input.pingRoleId ?? null, createdBy: 'js', now });
    const at = await R.syncReminderJob(row, settings ?? (await prisma.guildSettings.findUnique({ where: { guildId: args[0] } })));
    answer({ row, at: at?.toISOString() ?? null, job: jobJson(await getJob(R.jobKey(row.id))) });
  } else if (cmd === 'rem-read') {
    const row = await R.findReminderByName(args[0], args[1]);
    answer({ row, job: row ? jobJson(await getJob(R.jobKey(row.id))) : null });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
