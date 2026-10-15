// Regression tests of the feeds review (feeds, search, translation, live role, stat channels). Offline: pure functions only.
import { test } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const P = await import('../src/services/feedParsers.js');
const S = await import('../src/services/feedService.js');
const Search = await import('../src/services/searchService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

test.after(async () => {
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const RSS = '<?xml version="1.0"?><rss version="2.0"><channel><title>t</title><item><title>a</title></item></channel></rss>';

test('looksLikeHtml: a body of comment repeats cannot make the check backtrack exponentially', () => {
  // `(<!--[\s\S]*?-->\s*)*` took exponential time on this shape: 24 repeats cost ~75 ms and each pair of repeats doubled it.
  const hostile = '<!---->'.repeat(140) + '<x>';
  const t0 = Date.now();
  assert.equal(P.looksLikeHtml(hostile, 'application/xml'), false);
  assert.equal(P.looksLikeHtml(hostile, 'text/html'), true);
  assert.equal(P.looksLikeHtml('<!-- a -->'.repeat(90) + '<html>', ''), true);
  assert.equal(P.looksLikeHtml('<!-- '.repeat(190), 'application/xml'), false);
  assert.ok(Date.now() - t0 < 500, `took ${Date.now() - t0} ms`);
});

test('looksLikeHtml: the rules are unchanged (comments, XML declaration, mislabelled feeds)', () => {
  assert.equal(P.looksLikeHtml('<!-- c -->\n<!DOCTYPE html><html>', ''), true);
  assert.equal(P.looksLikeHtml('<!-- c -->  <html lang="en">', 'application/xml'), true);
  assert.equal(P.looksLikeHtml(RSS, 'text/html'), false, 'a real feed labelled text/html is still a feed');
  assert.equal(P.looksLikeHtml(`<?xml version="1.0"?>\n<!-- generated -->\n<!-- twice -->\n<rss version="2.0"><channel/></rss>`, 'text/html'), false);
  assert.equal(P.looksLikeHtml('<feed xmlns="http://www.w3.org/2005/Atom"/>', 'text/html; charset=utf-8'), false);
  assert.equal(P.looksLikeHtml('<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml">', 'text/html'), true);
  assert.equal(P.looksLikeHtml('<p>hello</p>', 'text/html'), true);
  assert.equal(P.looksLikeHtml(RSS, 'application/rss+xml'), false);
  assert.equal(P.looksLikeHtml('<!-- never closed <html>', ''), false);
});

test('discoverFeedLinks: one huge attribute-like token cannot make the scan quadratic', () => {
  // `([^\s"'<>/=]+)\s*=` retried every start position of a 190 000-character token (about 14 s for one admin command).
  const t0 = Date.now();
  assert.deepEqual(P.discoverFeedLinks(`<link ${'a'.repeat(190_000)}>`, 'https://a.test/'), []);
  assert.deepEqual(P.discoverFeedLinks(`<link rel="alternate" type="application/rss+xml" ${'b'.repeat(5000)} href="/late.xml">`, 'https://a.test/'), [],
    'a tag longer than 4096 characters is not a link tag worth reading');
  assert.ok(Date.now() - t0 < 500, `took ${Date.now() - t0} ms`);
  assert.deepEqual(
    P.discoverFeedLinks('<link rel="alternate" type="application/rss+xml" title="x" href="/feed.xml">', 'https://a.test/blog/'),
    ['https://a.test/feed.xml'],
  );
});

test('buildItemEmbed: a feed summary cannot form a masked link or a heading in the embed description', () => {
  const feed = { type: 'RSS', displayName: 'Example' };
  const item = {
    id: 'i1', title: 'Hello', url: 'https://x.test/1', author: '', publishedAt: null, image: null, isRepost: false,
    summary: '[Claim your reward](https://evil.test) and *more*\n# Big',
  };
  const e = S.buildItemEmbed(feed, item);
  const text = e.data.description;
  assert.ok(text.startsWith('\\[Claim your reward]'), text);
  assert.ok(text.includes('\\*more\\*'), text);
  assert.ok(text.includes('\n\\# Big'), text);
  assert.equal(S.buildItemEmbed(feed, { ...item, summary: '' }).data.description, undefined, 'no summary, no description');
  assert.ok(S.buildItemEmbed(feed, { ...item, summary: 'x'.repeat(2000) }).data.description.length <= 351);
});

test('search text cleaners: a long third-party body is cut first, so no pass is quadratic', () => {
  // 200 000 spaces took ~13 s in stripAniListHtml; `[`.repeat(200 000) ~12 s in discord.js's masked-link escape; `~!x` repeats ~3.5 s.
  const t0 = Date.now();
  Search.stripAniListHtml(`${' '.repeat(200_000)}a`);
  Search.stripAniListHtml('~!x'.repeat(100_000));
  Search.stripAniListHtml('['.repeat(200_000));
  Search.cleanUrbanText('['.repeat(200_000), 1800);
  Search.cleanUrbanText('[a'.repeat(100_000), 1800);
  assert.ok(Date.now() - t0 < 1500, `took ${Date.now() - t0} ms`);
  // The visible part is unchanged: the text that survives the clamp comes from the start.
  assert.equal(Search.stripAniListHtml('First line<br>Second ~!secret!~ end'), 'First line\nSecond ||secret|| end');
  const out = Search.stripAniListHtml(`${'word '.repeat(300)}~!late spoiler`);
  assert.ok(out.length <= 1000 && out.startsWith('word word'), out.slice(0, 40));
  assert.equal(Search.cleanUrbanText('a [b] c', 100), 'a b c');
  assert.ok(Search.cleanUrbanText('x'.repeat(5000), 100).length <= 100);
});

// ── feeds: an unprompted 304 is not "not modified" ───────────────────────────

const fakeRes = (status, headers = {}) => ({
  ok: status >= 200 && status < 300, status, statusText: '', headers, url: 'https://feed.test/f.xml',
  buffer: Buffer.alloc(0), text: () => '', json: () => ({}),
});

test('fetchAndParse: a 304 means "not modified" only when a conditional header was sent', async () => {
  S.__setFetchImpl({ safeFetch: async () => fakeRes(304) });
  try {
    const bare = await S.fetchAndParse('https://feed.test/f.xml', 'RSS');
    assert.equal(bare.ok, false);
    assert.equal(bare.error.code, 'UNEXPECTED_304');
    assert.equal(bare.error.status, 304);
    assert.match(S.describeFetchError('RSS', bare.error), /304 Not Modified.*not conditional/);
    // other headers and empty validators are not a conditional request
    assert.equal((await S.fetchAndParse('https://feed.test/f.xml', 'RSS', { headers: { accept: '*/*', 'if-none-match': '' } })).ok, false);
    assert.deepEqual(await S.fetchAndParse('https://feed.test/f.xml', 'RSS', { headers: { 'if-none-match': '"v1"' } }), { ok: true, notModified: true });
    assert.deepEqual(await S.fetchAndParse('https://feed.test/f.xml', 'RSS', { headers: { 'If-Modified-Since': 'Fri, 09 Oct 2026 10:00:00 GMT' } }), { ok: true, notModified: true });
    // /feed add: the resolvers show the clear error instead of a generic one
    const added = await S.resolveFeedSource('RSS', { url: 'https://feed.test/f.xml' });
    assert.equal(added.ok, false);
    assert.equal(added.title, 'Could Not Fetch');
    assert.match(added.message, /304 Not Modified/);
    const yt = await S.resolveFeedSource('YOUTUBE', { channelId: 'UC1234567890123456789012' });
    assert.equal(yt.probe.ok, false);
    assert.equal(yt.probe.error.code, 'UNEXPECTED_304');
  } finally {
    S.__setFetchImpl(null);
  }
});

// ── translation: the capped channel save and the memo generation ─────────────

const T = await import('../src/services/translationService.js');
const { cache } = await import('../src/services/redis.js');
const { LIMITS } = await import('../src/config/constants.js');

test('saveTranslateChannel: takes the per-guild lock first, enforces the cap, updates or reports an existing row', async () => {
  const realTx = prisma.$transaction;
  const realDelegate = prisma.translateChannel;
  const order = [];
  const rows = [];
  const key = (g, c) => `${g}:${c}`;
  const tx = {
    $queryRaw: async (sql) => { order.push(['lock', sql.values?.[0]]); return []; },
    translateChannel: {
      findUnique: async ({ where }) => { order.push(['find']); const w = where.guildId_channelId; return rows.find((r) => key(r.guildId, r.channelId) === key(w.guildId, w.channelId)) ?? null; },
      count: async ({ where }) => { order.push(['count']); return rows.filter((r) => r.guildId === where.guildId).length; },
      upsert: async ({ where, update, create }) => {
        order.push(['upsert']);
        const w = where.guildId_channelId;
        const hit = rows.find((r) => key(r.guildId, r.channelId) === key(w.guildId, w.channelId));
        if (hit) return Object.assign(hit, update);
        const row = { id: rows.length + 1, ...create };
        rows.push(row);
        return row;
      },
    },
  };
  prisma.$transaction = async (fn) => fn(tx);
  try {
    const first = await T.saveTranslateChannel('g1', 'c1', ['es']);
    assert.deepEqual([first.ok, first.created], [true, true]);
    assert.deepEqual(order[0], ['lock', 'translate-channels:g1'], 'the lock is taken before anything is read');
    assert.deepEqual(order.map((o) => o[0]), ['lock', 'find', 'count', 'upsert']);

    const again = await T.saveTranslateChannel('g1', 'c1', ['de', 'ja']);
    assert.deepEqual([again.ok, again.created, again.row.targetLangs], [true, false, ['de', 'ja']]);
    const conflict = await T.saveTranslateChannel('g1', 'c1', ['fr'], { updateExisting: false });
    assert.deepEqual(conflict, { ok: false, code: 'EXISTS' });
    assert.deepEqual(rows[0].targetLangs, ['de', 'ja'], 'an existing row is left alone when updateExisting is false');

    for (let n = rows.length; n < LIMITS.TRANSLATE_CHANNELS; n++) rows.push({ id: 100 + n, guildId: 'g1', channelId: `x${n}`, targetLangs: ['en'] });
    const full = await T.saveTranslateChannel('g1', 'new', ['es']);
    assert.deepEqual(full, { ok: false, code: 'LIMIT', count: LIMITS.TRANSLATE_CHANNELS });
    assert.equal((await T.saveTranslateChannel('g1', 'c1', ['it'])).ok, true, 'editing a row of a full server is allowed');
    assert.equal((await T.saveTranslateChannel('g2', 'new', ['es'])).ok, true, 'the cap is per server');
    assert.equal(rows.filter((r) => r.guildId === 'g1').length, LIMITS.TRANSLATE_CHANNELS);

    assert.deepEqual(T.RAW_SQL.map((r) => r.name), ['translation.channelsLock']);
    assert.match(T.RAW_SQL[0].build().sql, /pg_advisory_xact_lock\(hashtext\(/);
  } finally {
    prisma.$transaction = realTx;
    prisma.translateChannel = realDelegate;
  }
});

test('getGuildTranslateConfig: a read that began before a refresh never memoises its stale list', async () => {
  T.__testing.reset();
  const realDelegate = prisma.translateChannel;
  const realCache = { get: cache.get, set: cache.set, setNX: cache.setNX, del: cache.del };
  const written = [];
  cache.get = async () => null;
  cache.set = async (k, v) => { written.push(['set', k, v]); return true; };
  cache.setNX = async (k, v) => { written.push(['setNX', k, v]); return true; };
  cache.del = async () => true;
  let release;
  const gate = new Promise((r) => { release = r; });
  let calls = 0;
  prisma.translateChannel = {
    findMany: async () => {
      calls++;
      if (calls === 1) { await gate; return [{ channelId: 'old', targetLangs: ['es'] }]; } // the slow reader
      return [{ channelId: 'new', targetLangs: ['de'] }];                                    // the refresh
    },
  };
  try {
    const reader = T.getGuildTranslateConfig('gen1');
    await new Promise((r) => setImmediate(r));
    await T.refreshTranslateCache('gen1'); // an /autotranslate change lands while the reader is still loading
    release();
    const stale = await reader;
    assert.deepEqual([...stale.keys()], ['old'], 'the overlapped call itself still answers with what it loaded');
    const next = await T.getGuildTranslateConfig('gen1');
    assert.deepEqual([...next.keys()], ['new'], 'but the memo holds the refreshed list');
    assert.equal(calls, 2, 'the second call came from the memo, not the database');
    assert.ok(!written.some(([op, , rows]) => op === 'setNX' && rows[0]?.channelId === 'old'), 'the stale list was not written to Redis either');
  } finally {
    Object.assign(cache, realCache);
    prisma.translateChannel = realDelegate;
    T.__testing.reset();
  }
});

const skipDb = process.env.TEST_DATABASE_URL ? false : 'TEST_DATABASE_URL is not set';

test('DB: saveTranslateChannel keeps the cap exact under concurrent saves', { skip: skipDb }, async () => {
  const { ensureGuildSettings } = await import('../src/services/settingsService.js');
  const guildId = `92${String(Date.now()).slice(-9)}0000001`;
  await ensureGuildSettings(guildId);
  try {
    await prisma.translateChannel.createMany({
      data: Array.from({ length: LIMITS.TRANSLATE_CHANNELS - 1 }, (_, i) => ({ guildId, channelId: `95${String(i).padStart(16, '0')}`, targetLangs: ['en'] })),
    });
    const results = await Promise.all(Array.from({ length: 6 }, (_, i) => T.saveTranslateChannel(guildId, `96${String(i).padStart(16, '0')}`, ['es'])));
    assert.equal(results.filter((r) => r.ok).length, 1, JSON.stringify(results.map((r) => (r.ok ? 'ok' : r.code))));
    assert.equal(await prisma.translateChannel.count({ where: { guildId } }), LIMITS.TRANSLATE_CHANNELS);
  } finally {
    await prisma.translateChannel.deleteMany({ where: { guildId } });
    await prisma.guildSettings.deleteMany({ where: { guildId } });
  }
});
