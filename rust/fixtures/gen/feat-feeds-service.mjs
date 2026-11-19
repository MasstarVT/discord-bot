// Reference data for the feat-feeds service tests: keys, job payloads, customIds, the pure helpers of feedService.js
// and reminderService.js, run through the JS. Writes rust/fixtures/golden/feat-feeds/service.json.
// Run from the repository root: node rust/fixtures/gen/feat-feeds-service.mjs
process.env.TZ = 'UTC';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL ??= 'postgresql://x:x@127.0.0.1:1/x';
import { writeFileSync, mkdirSync } from 'node:fs';

const quiet = { error: console.error, warn: console.warn, log: console.log, info: console.info, debug: console.debug };
console.error = console.warn = console.info = console.debug = () => {};
const S = await import('../../../src/services/feedService.js');
const R = await import('../../../src/services/reminderService.js');
const { buildCustomId } = await import('../../../src/config/customIds.js');
const { default: redis } = await import('../../../src/services/redis.js');

const D = (s) => new Date(s);
const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);
const it = (id, at = null, extra = {}) => ({
  id, title: id, url: `https://x.test/${id}`, author: '', publishedAt: at ? D(at) : null, image: null,
  summary: '', isRepost: false, actorName: '', actorHandle: '', ...extra,
});
const TYPES = ['RSS', 'YOUTUBE', 'REDDIT', 'BLUESKY', 'PODCAST'];
const out = {};

out.keys = {
  feedJobKey: S.feedJobKey('clx0feed0001'),
  fetchCacheKey: ['https://a.test/', 'https://www.reddit.com/r/aww/new/.rss', 'https://ü.test/p?q=1'].map((u) => [u, S.fetchCacheKey(u)]),
  cacheTtlSeconds: Object.fromEntries(TYPES.map((t) => [t, S.cacheTtlSeconds(t)])),
  redditBlockKey: S.REDDIT_BLOCK_KEY, itunesKey: S.ITUNES_RL_KEY, redditSlots: Array.from({ length: S.REDDIT_MAX_PER_MIN }, (_, i) => `${S.REDDIT_RL_KEY}:${i}`),
  reminderJobKey: R.jobKey('clx0rem0001'),
  nonce: [['clx0rem0001', 0], ['clx0rem0001', 1760000000000], ['other', 1760000000123]].map(([id, t]) => [id, t, R.occurrenceNonce(id, new Date(t))]),
  reminderModal: buildCustomId('reminder-modal', 'clx0rem0001'),
  varHelp: Object.fromEntries(TYPES.map((t) => [t, S.varHelp(t)])),
};

const ERRORS = [
  { code: 'HTTP', status: 404 }, { code: 'HTTP', status: 403 }, { code: 'HTTP', status: 429 }, { code: 'HTTP', status: 400, apiError: 'InvalidRequest' },
  { code: 'HTTP', status: 400 }, { code: 'HTTP', status: 410 }, { code: 'HTTP', status: 401 }, { code: 'HTTP', status: 503 }, { code: 'TIMEOUT' }, { code: 'TOO_LARGE' },
  { code: 'DNS_FAILED' }, { code: 'BLOCKED_ADDRESS' }, { code: 'TOO_MANY_REDIRECTS' }, { code: 'INVALID_URL' }, { code: 'NETWORK', message: 'x'.repeat(100) },
  { code: 'UNEXPECTED_304', status: 304 }, { code: 'HTML_PAGE' }, { code: 'NOT_A_FEED' }, { code: 'INVALID_XML' }, { code: 'WHATEVER', message: 'odd' },
  { code: 'RATE_BUDGET', message: 'Reddit checks are paused for about a minute (bot-wide limit)' }, { code: 'X' }, { code: 'REDDIT', status: 403, message: 'm'.repeat(400) },
];
out.describe = TYPES.flatMap((t) => ERRORS.map((e) => [t, e, S.describeFetchError(t, e)]));

const TEMPLATES = ['{title} {subreddit} {youtube_channel} {user}', 'New video!', '{title} {server} {random:1:5} {channel.name} {random}', 'x {Link}', '{nope}',
  '{USER.NAME} {Feed} {episode_title} {bluesky_handle}', '{random:-5:10} {random:1:x} {a}{b}{c}{d}', `${Array.from({ length: 60 }, (_, i) => `{bad${i}}`).join(' ')}`, null];
out.templateWarnings = TYPES.flatMap((t) => TEMPLATES.map((tpl) => [t, tpl, S.templateWarnings(t, tpl)]));
out.parseMessageOption = ['a\\nb', '   ', null, 'x'.repeat(1001), ' hi\\n\\nthere '].map((m) => [m, S.parseMessageOption(m)]);
out.computeNextPollAt = [];
for (const t of TYPES) {
  for (const [failureCount, status, retryAfterMs] of [[0, null, null], [1, null, null], [5, null, null], [9, null, null], [1, 403, null], [0, 429, null], [1, null, 7_200_000], [0, null, 99 * 3_600_000]]) {
    for (const rand of [0, 0.5, 1]) {
      const now = D('2026-10-10T00:00:00Z');
      out.computeNextPollAt.push([t, failureCount, status, retryAfterMs, rand, S.computeNextPollAt({ type: t, failureCount, status, retryAfterMs, now, rand: () => rand }).getTime()]);
    }
  }
}
const baseline = D('2026-10-09T00:00:00Z');
const SEL = [
  { items: [it('c'), it('b')], initialized: false },
  { items: [it('c', '2026-10-10T00:00Z'), it('b', '2026-10-09T12:00Z'), it('a', '2026-10-09T06:00Z')], seenIds: ['b', 'a'], initialized: true, baseline },
  { items: [...Array.from({ length: 7 }, (_, i) => it(`n${7 - i}`, `2026-10-10T0${7 - i}:00Z`)), it('z', '2026-10-09T01:00Z')], seenIds: ['z'], initialized: true, baseline },
  { items: [it('old', '2026-10-01T00:00Z'), it('b', '2026-10-09T01:00Z')], seenIds: ['b'], initialized: true, baseline },
  { items: Array.from({ length: 8 }, (_, i) => it(`u${i}`)), seenIds: ['x', 'y'], initialized: true, baseline },
  { items: [it('r1', '2026-10-10T00:00Z', { isRepost: true }), it('p1', '2026-10-10T00:00Z'), it('p0', '2026-10-09T01:00Z')], seenIds: ['p0'], initialized: true, baseline },
  { items: [it('r1', '2026-10-10T00:00Z', { isRepost: true }), it('p1', '2026-10-10T00:00Z'), it('p0', '2026-10-09T01:00Z')], seenIds: ['p0'], initialized: true, baseline, includeReposts: true },
  { items: Array.from({ length: 50 }, (_, i) => it(`w${i}`, '2026-10-10T00:00Z')), seenIds: Array.from({ length: 100 }, (_, i) => `s${i}`), initialized: true, baseline },
  { items: [it('a')], seenIds: ['a'], initialized: true, baseline },
  { items: [it('a'), it('b'), it('c')], seenIds: ['q'], initialized: true, baseline: null },
];
const sel = (r) => ({ ...r, toPost: r.toPost.map((i) => i.id) });
out.selectNewItems = SEL.map((c) => [{ ...c, items: c.items.map((i) => ({ ...i, publishedAt: iso(i.publishedAt) })), baseline: iso(c.baseline) }, sel(S.selectNewItems(c))]);
out.windowItems = [
  [it('a', '2026-10-01T00:00Z'), it('b', '2026-10-03T00:00Z'), it('c', '2026-10-02T00:00Z')],
  [it('a', '2026-10-01T00:00Z'), it('b'), it('c', '2026-10-02T00:00Z')],
  [it('x'), it('x', '2026-10-01T00:00Z'), it('y', '2026-10-01T00:00Z')],
  [it('s', '2026-10-01T00:00Z'), it('t', '2026-10-01T00:00Z'), it('u', '2026-10-02T00:00Z')],
].map((items) => [items.map((i) => ({ ...i, publishedAt: iso(i.publishedAt) })), S.windowItems(items).map((i) => i.id)]);

const feedRow = (type, extra = {}) => ({ type, displayName: 'Feed', source: 'src', fetchUrl: 'https://f.test/x', messageTemplate: null, pingRoleId: null, ...extra });
const ITEMS = [
  it('i1', '2026-10-09T10:00:00Z', { title: 'a_b *c* [Click](https://evil.test) @everyone', url: 'https://x.test/1', author: '# Big', summary: '- list\n1. num\n## head [m](https://m.test)', image: 'https://x.test/i.png' }),
  it('i2', '2026-10-09T11:00:00Z', { title: 'Vid', url: 'https://www.youtube.com/watch?v=abc123', author: 'Chan' }),
  it('i3', '2026-10-09T12:00:00Z', { title: 'x'.repeat(3000), url: null, summary: 's'.repeat(600) }),
  it('i4', '2026-10-09T13:00:00Z', { title: 'Ep 1', url: 'https://p.test/1.mp3', isRepost: true, actorName: 'Alice', actorHandle: 'alice.bsky.social' }),
];
const FEEDS = [
  feedRow('RSS', { messageTemplate: '{title} {link}' }), feedRow('RSS', { displayName: '[Evil](https://evil.test)', messageTemplate: '{title}\n{author}\n{feed}' }),
  feedRow('YOUTUBE', { displayName: 'UCx' }), feedRow('REDDIT', { displayName: 'r/AskReddit' }), feedRow('BLUESKY', { displayName: '@alice.bsky.social' }),
  feedRow('BLUESKY', { displayName: '@bob.test' }), feedRow('PODCAST', { displayName: 'Pod' }), feedRow('YOUTUBE', { messageTemplate: '{user}' }),
  feedRow('RSS', { messageTemplate: '   ' }), feedRow('REDDIT', { displayName: 'R/Mixed', messageTemplate: '{subreddit} {url} {title}' }),
];
out.posts = [];
for (const f of FEEDS) {
  for (const i of ITEMS) {
    const p = S.buildPostPayload(f, i, { guild: null, channel: null, feedImage: 'https://p.test/art.jpg', ping: true });
    out.posts.push([f, { ...i, publishedAt: iso(i.publishedAt) }, { content: p.content ?? null, embeds: p.embeds.map((e) => e.toJSON()), allowedMentions: p.allowedMentions }]);
  }
}
out.displayNameUpdate = [
  [feedRow('YOUTUBE', { source: 'UCx', displayName: 'UCx' }), 'Chan\nnel', []], [feedRow('YOUTUBE', { source: 'UCx', displayName: 'Chan' }), 'Chan2', []],
  [feedRow('BLUESKY', { displayName: '@old.bsky.social' }), null, ['new.bsky.social']], [feedRow('BLUESKY', { displayName: '@a.bsky.social' }), null, ['a.bsky.social']],
  [feedRow('RSS', { displayName: 'x' }), 'y', []],
].map(([f, title, handles]) => [f, title, handles, S.displayNameUpdate(f, { title, items: handles.map((h) => ({ actorHandle: h })) })]);
out.feedStatus = [
  { enabled: false, disabledReason: 'x' }, { enabled: false }, { enabled: true, failureCount: 3, lastError: 'HTTP 404' }, { enabled: true, failureCount: 0, lastError: "Can't post" },
  { enabled: true, failureCount: 0, initialized: false }, { enabled: true, failureCount: 0, initialized: true }, { enabled: true, failureCount: 2 },
].map((f) => [f, S.feedStatus(f)]);
out.sourceLink = TYPES.map((t) => [t, S.sourceLink({ type: t, source: 'SRC', fetchUrl: 'https://f.test/feed' })]);
const now = Date.parse('2026-10-10T00:00:00Z');
out.retryAfter = [['120', null], [new Date(now + 30_000).toUTCString(), now], ['soon', null], [' 7 ', null], ['Thu, 01 Jan 1970 00:00:00 GMT', now], ['', null]].map(([v, n]) => [v, n, S.parseRetryAfterMs(v, n ?? now)]);
out.disableNotice = [
  [[{ type: 'RSS', displayName: 'A_b', disabledReason: '20 failed checks in a row: HTTP 500' }], 'single'],
  [[{ type: 'REDDIT', displayName: 'r/x', disabledReason: 'Its channel was deleted' }, { type: 'PODCAST', displayName: 'P', disabledReason: 'Its channel was deleted' }], 'many'],
];

// Reminders.
const rem = (over = {}) => ({
  id: 'r1', guildId: 'g', name: 'x', channelId: 'c', mode: 'TIME_OF_DAY', timeOfDay: '09:00', weekdays: [], intervalMinutes: null,
  windowStart: null, windowEnd: null, runAt: null, createdAt: D('2026-10-12T00:00:00Z'), requireActivity: false, pingRoleId: null,
  template: { content: 'hi', embed: null, buttons: [] }, enabled: true, lastSentAt: null, lastMessageId: null, ...over,
});
const fields = (f) => (f ? { ...f, runAt: iso(f.runAt) } : f);
const NOW = D('2026-10-12T10:00:00Z');
const INPUTS = [
  [{ mode: 'time' }], [{ mode: 'time', time: '09:00', interval: '2h' }], [{ mode: 'interval', interval: '3m' }], [{ mode: 'interval', interval: '8d' }],
  [{ mode: 'interval', interval: '1d', windowStart: '09:00' }], [{ mode: 'interval', interval: '2h', windowStart: '09:00', windowEnd: '09:00' }], [{ mode: 'interval', interval: '90' }],
  [{ mode: 'interval', interval: 'soon' }], [{ mode: 'interval' }], [{ mode: 'once', at: '2h' }], [{ mode: 'once', at: '2026-01-01 09:00' }], [{ mode: 'once', at: '2028-01-01 09:00' }],
  [{ mode: 'once', at: '2h', requireActivity: true }], [{ mode: 'once', at: '2h', requireActivity: false }], [{ mode: 'once', at: 'whenever' }], [{ mode: 'once' }],
  [{ mode: 'once', at: '2h', weekdays: 'mon' }], [{ mode: 'time', time: '9:30pm', weekdays: 'weekdays' }], [{ mode: 'time', time: '09:00', weekdays: 'all' }],
  [{ mode: 'time', time: '09:00', weekdays: 'funday' }], [{ mode: 'time', time: '25:00' }], [{}],
  [{ mode: 'interval', interval: '2h' }, rem({ mode: 'TIME_OF_DAY', timeOfDay: '09:00', weekdays: [1] })], [{ time: '10:15' }, rem({ weekdays: [1] })],
  [{ mode: 'once', at: '3h' }, rem({ requireActivity: true, weekdays: [2] })],
  [{ windowStart: 'none', windowEnd: 'off' }, rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 60, windowStart: '09:00', windowEnd: '17:00' })],
  [{ mode: 'once' }, rem({ mode: 'ONE_SHOT', timeOfDay: null, runAt: D('2026-10-12T09:00:00Z') })],
  [{ mode: 'interval', interval: '3d', weekdays: 'mon' }], [{ mode: 'interval', interval: '1d', weekdays: 'mon' }], [{ mode: 'interval', interval: '45m', windowStart: '22:00', windowEnd: '02:00', requireActivity: true }],
  [{ mode: 'interval', interval: '10080', weekdays: 'tue' }], [{ mode: 'time', time: 'noon', requireActivity: true }], [{ mode: 'interval', interval: '1h30m', windowEnd: '17:00' }],
];
out.validate = INPUTS.map(([input, base]) => {
  const v = R.validateReminderInput(input, { tz: 'Europe/Berlin', now: NOW, base: base ?? null });
  return [input, base ? { ...base, createdAt: iso(base.createdAt), runAt: iso(base.runAt) } : null, v.ok ? { ok: true, fields: fields(v.fields) } : v];
});
const SCHEDULES = [
  rem({ timeOfDay: '09:00', weekdays: [1] }), rem({ timeOfDay: '01:30' }), rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 120 }),
  rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 180, windowStart: '09:00', windowEnd: '17:00' }),
  rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 60, windowStart: '22:00', windowEnd: '02:00' }),
  rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 10_080, weekdays: [2] }), rem({ mode: 'ONE_SHOT', timeOfDay: null, runAt: D('2026-10-12T12:00:00Z') }),
  rem({ mode: 'INTERVAL', intervalMinutes: 3 }), rem({ timeOfDay: 'xx' }), rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 5 }),
  rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 60, weekdays: [1, 2, 3, 4, 5] }), rem({ mode: 'INTERVAL', timeOfDay: null, intervalMinutes: 60, weekdays: [1, 3] }),
  rem({ weekdays: [0, 6] }), rem({ weekdays: [1, 3, 5] }), rem({ mode: 'ONE_SHOT', runAt: null }), rem({ mode: 'INTERVAL', intervalMinutes: 90, windowStart: '08:00' }),
];
const TIMES = ['2026-10-12T00:05:10Z', '2026-10-12T07:10:00Z', '2026-10-12T09:00:04Z', '2026-10-12T09:14:59Z', '2026-10-12T09:15:01Z', '2026-10-12T12:59:00Z',
  '2026-10-12T15:30:00Z', '2026-10-12T23:30:00Z', '2026-03-28T12:00:00Z', '2026-10-24T12:00:00Z', '2026-10-25T00:30:00Z'];
out.schedules = SCHEDULES.map((r) => {
  const row = { ...r, createdAt: iso(r.createdAt), runAt: iso(r.runAt) };
  const runs = [];
  for (const tz of ['UTC', 'Europe/London', 'Europe/Berlin', 'America/New_York']) {
    for (const t of TIMES) {
      const at = D(t);
      runs.push([tz, t, iso(R.computeNextRun(r, tz, at)), iso(R.dueSlot(r, tz, at)), iso(R.jobRunAt(r, tz, at))]);
    }
  }
  return [row, R.describeSchedule(r), R.lateToleranceMs(r), runs];
});
const st = (o) => ({ requireActivity: true, lastMessageId: '100', ...o });
out.activity = [st({ requireActivity: false }), st({ lastMessageId: null }), st({ channelLastMessageId: '100' }), st({ recent: [{ bot: true }], cachedLast: { bot: true } }),
  st({ recent: [{ bot: true }, { bot: false }] }), st({ recent: [{ bot: false, system: true }] }), st({ recent: null, cachedLast: null }), st({ recent: null, cachedLast: { bot: true } }),
  st({ recent: null, cachedLast: { bot: false } }), st({ recent: [], cachedLast: { bot: false } }), st({ channelLastMessageId: '101' })].map((s) => [s, R.activityAllowsSend(s)]);
out.emptyTemplate = [{ content: '  ', embed: { title: null } }, { content: 'a' }, { content: null, embed: { color: 5 } }, { content: null, embed: { description: 'd' } },
  { content: null, embed: { fields: [{ name: 'a', value: 'b' }] } }, { content: null, embed: { author: { name: 'x' } } }, { content: null, embed: { footer: { text: ' ' } } },
  { content: null, embed: { image: 'https://i.test/a.png' } }, {}, { embed: 'str' }].map((t) => [t, R.isEmptyTemplate(t)]);
out.userTokens = [{ content: 'hey {user.name}' }, { content: '{server}' }, { content: null, embed: { footer: { text: 'bye {user}' } } }, { content: '{USER}' },
  { content: '{user.mention}' }, { content: '{username}' }, { content: null, embed: { title: '{user.avatar}', description: 'x' } }].map((t) => [t, R.hasUserTokens(t)]);
out.mayRetry = [[1, 10, 0, 900_000], [10, 10, 0, 900_000], [null, null, 0, 900_000], [5, 10, 0, 900_000], [5, 10, 480_000, 900_000], [3, 10, 3_000_000, 3_600_000], [9, 10, 0, 3_600_000]]
  .map(([attempt, maxAttempts, lateMs, toleranceMs]) => [attempt, maxAttempts, lateMs, toleranceMs, R.mayRetry({ attempt: attempt ?? undefined, maxAttempts: maxAttempts ?? undefined, lateMs, toleranceMs })]);
out.retryDelay = [0, 1, 2, 3, 5, 7, 8, 20].map((a) => [a, R.schedulerRetryDelayMs(a)]);
out.sendPayloads = [
  [rem({ template: { content: 'Hi {server} in {channel} {user}' }, pingRoleId: '42' }), true], [rem({ template: { content: 'Hi' }, pingRoleId: '42' }), false],
  [rem({ template: { content: 'x'.repeat(2000) }, pingRoleId: '42' }), true], [rem({ template: { content: null, embed: null } }), true],
  [rem({ template: { content: '@everyone hi' } }), true], [rem({ template: { content: null, embed: { title: 'T', description: 'd {channel}' }, buttons: [{ label: 'Go', url: 'https://go.test' }] } }), true],
].map(([r, ping]) => {
  const p = R.buildSendPayload(r, { guild: null, channel: null, settings: { embedColor: 0x123456 }, ping });
  return [{ ...r, createdAt: iso(r.createdAt) }, ping, { content: p.content ?? null, embeds: p.embeds.map((e) => (e.toJSON ? e.toJSON() : e)), components: (p.components ?? []).map((c) => (c.toJSON ? c.toJSON() : c)), allowedMentions: p.allowedMentions }];
});
out.summary = [
  [rem({ requireActivity: true, pingRoleId: '42' }), { timezone: 'Europe/Paris', remindersEnabled: true }, '2026-10-12T12:00:00Z', ['WARN']],
  [rem(), { timezone: 'UTC', remindersEnabled: false }, null, []], [rem({ enabled: false }), { timezone: 'Nope/Zone', remindersEnabled: true }, null, []], [rem(), null, null, []],
].map(([r, s, next, w]) => [{ ...r, createdAt: iso(r.createdAt) }, s, next, w, R.summaryLines(r, s, next ? D(next) : null, w)]);

mkdirSync('rust/fixtures/golden/feat-feeds', { recursive: true });
writeFileSync('rust/fixtures/golden/feat-feeds/service.json', `${JSON.stringify(out, null, 1)}\n`);
redis.disconnect();
Object.assign(console, quiet);
console.log('wrote service.json');
process.exit(0);
