// The JS side of feat-translation's cross-language tests: the JS bot's own translationService,
// translateStatsService and userTimezoneService against the test database and Redis (TEST_DATABASE_URL,
// TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-translation-js.mjs save-channel <guildId> <channelId> <json langs>
//        ensureGuildSettings + saveTranslateChannel + refreshTranslateCache
//   node rust/fixtures/gen/feat-translation-js.mjs config <guildId>         getGuildTranslateConfig (Map as entries)
//   node rust/fixtures/gen/feat-translation-js.mjs rows <guildId>           translateChannel.findMany (channelId, targetLangs)
//   node rust/fixtures/gen/feat-translation-js.mjs cache-get <key>          cache.get(key)
//   node rust/fixtures/gen/feat-translation-js.mjs tz-set <userId> <zone>   setUserTimezone
//   node rust/fixtures/gen/feat-translation-js.mjs tz-clear <userId>        clearUserTimezone
//   node rust/fixtures/gen/feat-translation-js.mjs tz-get <userId> [cache]  getUserTimezone (null / undefined -> "undefined")
//   node rust/fixtures/gen/feat-translation-js.mjs stat-record <json entry> recordTranslation
//   node rust/fixtures/gen/feat-translation-js.mjs stats <guildId>          translateStat.findMany (sorted)
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
for (const k of ['log', 'info', 'warn', 'debug']) console[k] = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value === undefined ? 'undefined' : value)}\n`);

const svc = await imp('src', 'services', 'translationService.js');
const stats = await imp('src', 'services', 'translateStatsService.js');
const userTz = await imp('src', 'services', 'userTimezoneService.js');
const { ensureGuildSettings } = await imp('src', 'services', 'settingsService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis, cache } = await imp('src', 'services', 'redis.js');

// Wait for Redis so the first cache call is not a fail-open miss.
for (let i = 0; i < 50 && redis.status !== 'ready'; i++) await new Promise((r) => setTimeout(r, 100));

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'save-channel') {
    await ensureGuildSettings(args[0]);
    const r = await svc.saveTranslateChannel(args[0], args[1], JSON.parse(args[2]));
    await svc.refreshTranslateCache(args[0]);
    answer(r.ok ? { ok: true, created: r.created, targetLangs: r.row.targetLangs } : r);
  } else if (cmd === 'config') {
    answer([...(await svc.getGuildTranslateConfig(args[0]))]);
  } else if (cmd === 'rows') {
    answer(await prisma.translateChannel.findMany({ where: { guildId: args[0] }, select: { channelId: true, targetLangs: true }, orderBy: { channelId: 'asc' } }));
  } else if (cmd === 'cache-get') {
    answer(await cache.get(args[0]));
  } else if (cmd === 'tz-set') {
    await userTz.setUserTimezone(args[0], args[1]);
    answer({ ok: true });
  } else if (cmd === 'tz-clear') {
    answer(await userTz.clearUserTimezone(args[0]));
  } else if (cmd === 'tz-get') {
    answer(await userTz.getUserTimezone(args[0], { cacheOnly: args[1] === 'cache' }));
  } else if (cmd === 'stat-record') {
    await stats.recordTranslation(JSON.parse(args[0]));
    answer({ ok: true });
  } else if (cmd === 'stats') {
    const rows = await prisma.translateStat.findMany({ where: { guildId: args[0] } });
    rows.sort((a, b) => `${a.channelId}${a.via}${a.direction}${a.lang}`.localeCompare(`${b.channelId}${b.via}${b.direction}${b.lang}`));
    answer(rows.map(({ channelId, via, direction, lang, count }) => ({ channelId, via, direction, lang, count })));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
