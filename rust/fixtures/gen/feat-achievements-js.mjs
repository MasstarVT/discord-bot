// The JS side of feat-achievements' cross-language tests: the JS bot's own achievementService and birthdayService
// against the test servers (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   pure <json [{ fn, args }]>                 pure helpers of both services (no servers)
//   pure-file <path>                           the same, the JSON read from a file (long sweeps)
//   ach-create <guildId> <json data>           createAchievement + refreshAchievementCache; returns { row, cached }
//   ach-record <guildId> <userId> <trigger> <inc> <floor>   getAchievementIndex + recordProgress (stub guild)
//   ach-progress <guildId> <userId>            the member's AchievementProgress rows
//   ach-view <guildId> <userId>                listAchievements + loadMemberView
//   ach-index <guildId>                        getAchievementIndex (fresh process: reads the Redis copy)
//   ach-cache <guildId>                        cache.get(achievementsKey)
//   ach-reset <guildId> <userId> <json ids>    resetMemberProgress
//   ach-keys <guildId> <userId>                the reaction and boost dedupe keys the hooks build (captured setNX)
//   bd-save <guildId> <userId> <json date> <iso now>   saveBirthday with the guild's settings
//   bd-get <guildId> <userId>                  getBirthday
//   bd-tomb-set <guildId> <userId> <year> <iso now>    the tick's tombstone write
//   bd-tomb-get <guildId> <userId>             cache.get(tombstoneKey)
//   bd-ensure <guildId>                        ensureTickJob(guildId, settings); returns getJob(tickKey)
//   bd-job <guildId>                           getJob(tickKey)
//   bd-claim <guildId> <json ids> <year> <marker>      claimBirthdays
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const ach = await imp('src', 'services', 'achievementService.js');
const bd = await imp('src', 'services', 'birthdayService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis, cache } = await imp('src', 'services', 'redis.js');
const settingsService = await imp('src', 'services', 'settingsService.js');
const scheduler = await imp('src', 'services', 'scheduler.js');
const { renderPlaceholders } = await imp('src', 'utils', 'placeholderParser.js');

const date = (v) => (v == null ? v : new Date(v));
const iso = (d) => (d instanceof Date ? d.toISOString() : d);

// Every pure call the Rust sweep checks. Dates travel as ISO strings or epoch ms.
const PURE = {
  parseGoals: ([input, trigger]) => ach.parseGoals(input, trigger),
  normalizeAchievementName: ([s]) => ach.normalizeAchievementName(s),
  normalizeBadgeEmoji: ([s, custom, allowClear]) => ach.normalizeBadgeEmoji(s, { hasCustomEmoji: () => custom, allowClear }),
  parseDeadline: ([input, tz, now, allowClear]) => {
    const r = ach.parseDeadline(input, tz, { now: new Date(now), allowClear });
    return r.ok ? { ok: true, value: iso(r.value) } : r;
  },
  normalizeTiers: ([raw]) => ach.normalizeTiers(raw),
  validateTiers: ([tiers, trigger]) => ach.validateTiers(tiers, trigger),
  evaluateProgress: ([tiers, row]) => ach.evaluateProgress(tiers, row),
  formatRewards: ([rewards, roles, settings]) => ach.formatRewards(rewards, roles ? { roles: { cache: new Map(roles.map((r) => [r, {}])) } } : null, settings),
  formatMemberEntry: ([a, view, now]) => ach.formatMemberEntry(a, view, now),
  displayProgress: ([a, row, ul, member]) => ach.displayProgress(a, row, ul, member),
  goalText: ([trigger, goal]) => ach.goalText(trigger, goal),
  isBoostStart: ([ctx, now]) => ach.isBoostStart(ctx, now),
  mergeRewards: ([existing, incoming, given]) => ach.mergeRewards(existing, incoming, new Set(given)),
  announcement: ([a, tierIndex, progress, userId, displayName, rewardsText, template]) => {
    const vars = ach.buildAnnouncementVars({ achievement: a, tierIndex, progress, userId, displayName, rewardsText });
    return { vars, text: renderPlaceholders(template, { vars }) };
  },
  parseBirthdayInput: ([raw, requireYear, now, tz]) => bd.parseBirthdayInput(raw, { requireYear, now: new Date(now), tz }),
  nextTickAt: ([now, tz]) => bd.nextTickAt(new Date(now), tz).toISOString(),
  tombstoneTtlSeconds: ([now, tz]) => bd.tombstoneTtlSeconds(new Date(now), tz),
  partitionMarkers: ([markers, opts]) => {
    const r = bd.partitionMarkers(markers.map((m) => ({ ...m, roleGrantedAt: new Date(m.roleGrantedAt) })), opts);
    return { keep: r.keep.map((m) => m.id), expired: r.expired.map((m) => m.id) };
  },
  chunkAnnouncements: ([entries]) => bd.chunkAnnouncements(entries),
  codeBlock: ([text]) => bd.codeBlock(text),
  formatBirthday: ([d, opts]) => bd.formatBirthday(d, opts),
  nextOccurrence: ([row, local]) => bd.nextOccurrence(row, local),
};

const stubGuild = (id) => ({
  id, name: 'Stub', available: true,
  members: { cache: new Map(), fetch: async () => { throw Object.assign(new Error('Unknown Member'), { code: 10007 }); } },
  roles: { cache: new Map() }, channels: { cache: new Map() },
});

const [cmd, ...args] = process.argv.slice(2);
// The cache fails fast while disconnected: wait for the connection before using it (the servers commands only).
if (cmd !== 'pure' && cmd !== 'pure-file' && process.env.TEST_REDIS_URL) {
  await Promise.race([
    new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r))),
    new Promise((r) => setTimeout(r, 5000)),
  ]);
}
try {
  if (cmd === 'pure' || cmd === 'pure-file') {
    const text = cmd === 'pure' ? args[0] : readFileSync(args[0], 'utf8');
    answer(JSON.parse(text).map(({ fn, args: a }) => PURE[fn](a)));
  } else if (cmd === 'ach-create') {
    const data = JSON.parse(args[1]);
    await settingsService.ensureGuildSettings(args[0]);
    const r = await ach.createAchievement(args[0], { ...data, deadline: date(data.deadline) ?? null });
    await ach.refreshAchievementCache(args[0]);
    answer({ ...r, cached: await cache.get(ach.achievementsKey(args[0])) });
  } else if (cmd === 'ach-record') {
    const [g, userId, trigger, inc, floor] = args;
    const idx = await ach.getAchievementIndex(g);
    const settings = await settingsService.getSettings(g);
    const ctx = { guildId: g, guild: stubGuild(g), client: { users: { fetch: async () => null } } };
    const rows = await ach.recordProgress({ ctx, settings, userId, trigger, achievements: idx.byTrigger[trigger], inc: Number(inc), floor: Number(floor) });
    await new Promise((r) => setTimeout(r, 200));
    answer(rows);
  } else if (cmd === 'ach-progress') {
    answer(await prisma.achievementProgress.findMany({ where: { guildId: args[0], userId: args[1] }, orderBy: { achievementId: 'asc' } }));
  } else if (cmd === 'ach-view') {
    const view = await ach.loadMemberView(args[0], args[1], null);
    answer({ list: await ach.listAchievements(args[0]), view });
  } else if (cmd === 'ach-index') {
    const idx = await ach.getAchievementIndex(args[0]);
    answer(idx ? idx.byTrigger : null);
  } else if (cmd === 'ach-cache') {
    answer(await cache.get(ach.achievementsKey(args[0])));
  } else if (cmd === 'ach-reset') {
    await ach.resetMemberProgress(args[0], args[1], JSON.parse(args[2]));
    answer({ ok: true });
  } else if (cmd === 'ach-keys') {
    const [g, userId] = args;
    const keys = [];
    cache.setNX = async (key) => { keys.push(key); return false; };
    const settings = await settingsService.getSettings(g);
    const guild = stubGuild(g);
    await ach.onReactionAdd({ guildId: g, guild, getSettings: async () => settings, user: { id: userId, bot: false }, message: { id: '700000000000000001', channelId: '1' }, emojiKey: '⭐' });
    await ach.onReactionAdd({ guildId: g, guild, getSettings: async () => settings, user: { id: userId, bot: false }, message: { id: '700000000000000002', channelId: '1' }, emojiKey: '123456789012345678' });
    await ach.onMemberUpdate({ guildId: g, guild, getSettings: async () => settings, oldKnown: true, newMember: { id: userId, premiumSinceTimestamp: 1767225600123, user: { bot: false } } });
    answer(keys);
  } else if (cmd === 'bd-save') {
    const settings = await settingsService.getSettings(args[0]);
    const r = await bd.saveBirthday({ guild: { id: args[0] }, userId: args[1], date: JSON.parse(args[2]), settings, now: new Date(args[3]) });
    answer(r);
  } else if (cmd === 'bd-get') {
    answer(await bd.getBirthday(args[0], args[1]));
  } else if (cmd === 'bd-tomb-set') {
    const [g, userId, year, now] = args;
    const ttl = bd.tombstoneTtlSeconds(new Date(now), 'UTC');
    answer({ ok: await cache.set(bd.tombstoneKey(g, userId), Number(year), ttl), ttl });
  } else if (cmd === 'bd-tomb-get') {
    answer({ value: await cache.get(bd.tombstoneKey(args[0], args[1])) ?? null });
  } else if (cmd === 'bd-ensure') {
    const settings = await settingsService.getSettings(args[0]);
    await bd.ensureTickJob(args[0], settings);
    answer(await scheduler.getJob(bd.tickKey(args[0])));
  } else if (cmd === 'bd-job') {
    answer(await scheduler.getJob(bd.tickKey(args[0])));
  } else if (cmd === 'bd-claim') {
    answer(await bd.claimBirthdays({ guildId: args[0], ids: JSON.parse(args[1]), year: Number(args[2]), setMarker: args[3] === 'true' }));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
