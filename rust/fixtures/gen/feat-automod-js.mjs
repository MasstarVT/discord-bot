// The JS side of feat-automod's cross-language tests: the JS bot's own automodService and antiRaidService.
// Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-automod-js.mjs call <json [[fn, ...args], ...] | @file> pure functions (no servers)
//   node rust/fixtures/gen/feat-automod-js.mjs keys <guildId> <userId> <json contents>  the Redis keys runAutomod builds
//   node rust/fixtures/gen/feat-automod-js.mjs spam <guildId> <userId> <n>           n messages through runAutomod (real Redis)
//   node rust/fixtures/gen/feat-automod-js.mjs cfgwrite <guildId> <json raw config>   updateSettings({ automodEnabled, automodConfig })
//   node rust/fixtures/gen/feat-automod-js.mjs cfgread <guildId>                      getSettings + normalizeConfig
//   node rust/fixtures/gen/feat-automod-js.mjs raidstart <guildId> <minutes>          startLockdown (fake guild, real tables and scheduler)
//   node rust/fixtures/gen/feat-automod-js.mjs raidread <guildId>                     loadState + getJob
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
delete process.env.BOT_OWNER_ID;

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value, (_k, v) => (typeof v === 'bigint' ? v.toString() : v))}\n`);
const quiet = () => {};
console.log = console.info = console.warn = console.error = console.debug = quiet;

const am = await imp('src', 'services', 'automodService.js');
const ar = await imp('src', 'services', 'antiRaidService.js');
const amCmd = await imp('src', 'commands', 'admin', 'automod.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis, cache } = await imp('src', 'services', 'redis.js');
const settingsService = await imp('src', 'services', 'settingsService.js');
const scheduler = await imp('src', 'services', 'scheduler.js');

const json = (x) => (x && typeof x.toJSON === 'function' ? x.toJSON() : x);

// A settings-like object for the anti-raid helpers.
const S = (over = {}) => ({
  guildId: 'G', antiRaidEnabled: true, antiRaidJoinThreshold: 10, antiRaidJoinSeconds: 10, antiRaidAction: 'ALERT',
  antiRaidPauseInvites: false, antiRaidLockChat: false, antiRaidLockdownMinutes: 30, antiRaidMinAccountAgeDays: 0,
  antiRaidYoungAction: 'KICK', antiRaidAlertChannelId: null, modLogChannelId: null, ...over,
});

const dates = (state) => ({ ...state, lockedAt: state.lockedAt ? new Date(state.lockedAt) : null, lockedUntil: state.lockedUntil ? new Date(state.lockedUntil) : null });

const FNS = {
  normalizeConfig: (raw) => am.normalizeConfig(raw),
  isLegacyConfig: (raw) => am.isLegacyConfig(raw),
  textForWords: (s) => am.textForWords(s),
  textForLinks: (s) => am.textForLinks(s),
  normalizeWord: (s) => am.normalizeWord(s),
  normalizeDomain: (s) => am.normalizeDomain(s),
  findInvites: (s) => am.findInvites(s),
  extractLinkHosts: (s) => am.extractLinkHosts(s),
  capsStats: (s) => am.capsStats(s),
  countEmojis: (s) => am.countEmojis(s),
  countSpoilers: (s, n) => am.countSpoilers(s, new Map(Array.from({ length: n }, (_, i) => [String(i), { spoiler: true }]))),
  maxCombiningRun: (s) => am.maxCombiningRun(s),
  duplicateFingerprint: (s) => am.duplicateFingerprint(s),
  isSpacelessWord: (s) => am.isSpacelessWord(s),
  resolveRuleKey: (s) => am.resolveRuleKey(s),
  formatThreshold: (k, v) => am.formatThreshold(k, v),
  describeRuleLine: (k, raw) => { const c = am.normalizeConfig(raw); return am.describeRuleLine(k, c.rules[k], c); },
  describeHoneypot: (raw) => am.describeHoneypot(am.normalizeConfig(raw)),
  describeLinksMode: (raw) => am.describeLinksMode(am.normalizeConfig(raw)),
  checkBadWords: (content, raw) => { const c = am.normalizeConfig(raw); return am.checkBadWords(content, c, am.buildMatchers(c)); },
  evaluate: (content, raw) => {
    const c = am.normalizeConfig(raw);
    return am.evaluateContentRules({ content }, am.activeRuleKeys(c), c, am.buildMatchers(c), null);
  },
  resolveOutcome: (matches) => am.resolveOutcome(matches),
  buildReason: (matches, channel) => am.buildReason(am.resolveOutcome(matches), channel),
  honeypotReason: (hp, channel) => am.buildReason(am.honeypotOutcome(hp), channel),
  honeypotOutcome: (hp) => am.honeypotOutcome(hp),
  buildPunishmentLogFields: (rule, detail, content, channelId) => am.buildPunishmentLogFields(rule, detail, content, channelId ? { id: channelId } : null),
  buildNoticeText: (p) => am.buildNoticeText(p),
  honeypotWarning: (action) => json(amCmd.honeypotWarningEmbed(action)),
  // Anti-raid.
  clampInt: (v, range, fb) => ar.clampInt(v, ar.RAID[range], fb),
  harsher: (a, b) => ar.harsher(a, b),
  parsePrior: (t) => ar.parsePrior(t),
  restoredBits: (cur, t) => ar.restoredBits(BigInt(cur), t),
  bitNames: (b) => ar.bitNames(BigInt(b)),
  detectionOf: (s) => ar.detectionOf(s),
  youngAccount: (created, s, now) => ar.youngAccount({ createdTimestamp: created }, s, now),
  configLines: (over) => ar.configLines(S(over)),
  startEmbed: (state, over, effects, trigger, problems) => json(ar.buildStartEmbed({ state: dates(state), settings: S(over), effects, trigger, problems })),
  endEmbed: (how, by, result) => json(ar.buildEndEmbed({ how, endedById: by, result })),
  youngEmbed: (id, tag, ageDays, minDays, action, more) => json(ar.buildYoungEmbed({ member: { user: { id, tag } }, ageDays, minDays, action, more })),
  endButtonRow: () => json(ar.endButtonRow()),
};

// The fakes around one message (the shape test/automod.test.js uses), with a recording, always-allowing Redis.
function fakeCtx(guildId, userId, content, settingsRow, messageId) {
  const channel = {
    id: '800000000000000005', name: 'general', parentId: null, isThread: () => false, isTextBased: () => true,
    permissionsFor: () => ({ has: () => true }), send: async () => ({ id: '1', delete: async () => {} }),
  };
  const guild = { id: guildId, ownerId: '800000000000000003', members: { me: { id: '800000000000000004' } }, invites: { cache: new Map() }, channels: { cache: new Map() } };
  channel.guild = guild;
  const member = { id: userId, user: { id: userId, bot: false }, guild, permissions: { has: () => false }, roles: { cache: new Map() } };
  const message = {
    id: messageId, content, author: { id: userId, bot: false, username: 'u', tag: 'u#0' }, member, channel, channelId: channel.id,
    attachments: new Map(), mentions: { users: new Map(), roles: new Map(), everyone: false }, delete: async () => {},
  };
  return { guild, guildId, client: { user: { id: '800000000000000004' } }, message, member, getSettings: async () => settingsRow, stop: () => {}, state: {} };
}

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'call') {
    const text = args[0].startsWith('@') ? readFileSync(args[0].slice(1), 'utf8') : args[0];
    answer(JSON.parse(text).map(([fn, ...a]) => {
      try {
        return { ok: json(FNS[fn](...a)) ?? null };
      } catch (err) {
        return { error: String(err?.message ?? err) };
      }
    }));
  } else if (cmd === 'keys') {
    const keys = [];
    cache.available = () => true;
    cache.slidingWindow = async (key, windowMs, member) => { keys.push({ key, windowMs, member }); return 1; };
    const row = { automodEnabled: true, modRoleIds: [], immuneRoleIds: [], automodConfig: { version: 2, rules: { spam: { action: 'delete' }, repeatedText: { action: 'delete' } } } };
    let n = 0;
    for (const content of JSON.parse(args[2])) await am.runAutomod(fakeCtx(args[0], args[1], content, row, `70000000000000000${n++}`));
    answer(keys);
  } else if (cmd === 'spam') {
    await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));
    const row = { automodEnabled: true, modRoleIds: [], immuneRoleIds: [], automodConfig: { version: 2, rules: { spam: { action: 'delete', threshold: 30 } } } };
    for (let i = 0; i < Number(args[2]); i++) await am.runAutomod(fakeCtx(args[0], args[1], 'x', row, `71000000000000000${i}`));
    answer({ key: `automod:rate:${args[0]}:${args[1]}`, type: await redis.type(`automod:rate:${args[0]}:${args[1]}`) });
  } else if (cmd === 'cfgwrite') {
    const row = await settingsService.updateSettings(args[0], { automodEnabled: true, automodConfig: JSON.parse(args[1]) });
    answer({ automodConfig: row.automodConfig, normalized: am.normalizeConfig(row.automodConfig) });
  } else if (cmd === 'cfgread') {
    const row = await settingsService.getSettings(args[0], { fresh: true });
    answer({ automodEnabled: row.automodEnabled, automodConfig: row.automodConfig, normalized: am.normalizeConfig(row.automodConfig) });
  } else if (cmd === 'raidstart') {
    // A guild that the bot cannot edit (no permissions): the lockdown stands, the effects fail, nothing is touched.
    const guild = {
      id: args[0], client: { user: { id: '800000000000000004' } }, incidentsData: null,
      members: { me: { permissions: { has: () => false }, roles: { cache: new Map() } } },
      roles: { everyone: { permissions: { bitfield: 0n } } }, channels: { cache: new Map(), fetch: async () => null },
    };
    const res = await ar.startLockdown({ guild, settings: S({ guildId: args[0], antiRaidLockChat: true, antiRaidPauseInvites: true }), startedById: '800000000000000009', reason: ' spam wave ', minutes: Number(args[1]) });
    const job = await scheduler.getJob(`antiraid.unlock:${args[0]}`);
    answer({ res, job: job && { type: job.type, dedupeKey: job.dedupeKey, payload: job.payload, runAt: job.runAt } });
  } else if (cmd === 'raidread') {
    const row = await ar.loadState(args[0]);
    const job = await scheduler.getJob(`antiraid.unlock:${args[0]}`);
    answer({ row, active: ar.isActive(row), job: job && { type: job.type, dedupeKey: job.dedupeKey, payload: job.payload, runAt: job.runAt, status: job.status } });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
