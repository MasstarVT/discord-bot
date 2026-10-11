// pkg-feeds tests (pkg-feeds.md §9 + the architect amendments). Offline by default: fixtures and a mocked fetch seam,
// no Discord, no network. With TEST_DATABASE_URL (a migrated database) the DB cases also run: the updatedAt-guarded
// claim, failure counting and auto-disable, P2002, the cap under the mutex, the one-notice disable, the channelDelete
// cleanup and the job repair, against fakes:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/feeds.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ChannelType, PermissionFlagsBits } = await import('discord.js');
const P = await import('../src/services/feedParsers.js');
const S = await import('../src/services/feedService.js');
const cmd = await import('../src/commands/admin/feed.js');
const pluginMod = await import('../src/plugins/feeds.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { PERMISSION_LEVELS, LIMITS } = await import('../src/config/constants.js');
const { SafeFetchError } = await import('../src/utils/safeFetch.js');

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

after(async () => {
  S.__setFetchImpl(null);
  redis.disconnect();
  Object.assign(console, quiet);
});

const D = (s) => new Date(s);
const it = (id, iso = null, extra = {}) => ({
  id, title: id, url: `https://x.test/${id}`, author: '', publishedAt: iso ? D(iso) : null, image: null,
  summary: '', isRepost: false, actorName: '', actorHandle: '', ...extra,
});

// ── Fixtures (§9.1) ───────────────────────────────────────────────────────────

const RSS = `<?xml version="1.0"?>
<rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:atom="http://www.w3.org/2005/Atom"><channel>
<title>Example &amp; Co</title><link>https://example.com/</link><atom:link href="https://example.com/feed.xml" rel="self"/>
<item><title><![CDATA[Hello <b>World</b>]]></title><link>https://example.com/2</link><guid isPermaLink="false">post-2</guid>
<pubDate>Fri, 09 Oct 2026 10:00:00 GMT</pubDate><dc:creator>Ann</dc:creator><description>&lt;p&gt;Second&lt;/p&gt;</description></item>
<item><title>First</title><link>https://example.com/1</link><pubDate>Thu, 08 Oct 2026 10:00:00 GMT</pubDate></item>
</channel></rss>`;
const YT = `<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns:media="http://search.yahoo.com/mrss/" xmlns="http://www.w3.org/2005/Atom">
<title>Chan</title><entry><id>yt:video:abc123</id><yt:videoId>abc123</yt:videoId><title>Vid</title>
<link rel="alternate" href="https://www.youtube.com/watch?v=abc123"/><author><name>Chan</name></author>
<published>2026-10-09T12:00:00+00:00</published><media:group><media:thumbnail url="https://i.ytimg.com/vi/abc123/hqdefault.jpg" width="480" height="360"/></media:group></entry></feed>`;
const REDDIT = `<feed xmlns="http://www.w3.org/2005/Atom" xmlns:media="http://search.yahoo.com/mrss/"><title>newest submissions : aww</title>
<entry><author><name>/u/kit</name></author><content type="html">&lt;p&gt;so cute&lt;/p&gt; submitted by &lt;a href="https://www.reddit.com/user/kit"&gt; /u/kit &lt;/a&gt; [link] [comments]</content>
<id>t3_xyz</id><media:thumbnail url="https://b.thumbs.redditmedia.com/x.jpg"/><link href="https://www.reddit.com/r/aww/comments/xyz/cat/"/>
<published>2026-10-09T12:00:00+00:00</published><title>Cat</title></entry></feed>`;
const RDF = `<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns="http://purl.org/rss/1.0/">
<channel rdf:about="https://r.example/"><title>RDF Feed</title><link>https://r.example/</link></channel>
<item rdf:about="https://r.example/a"><title>A</title><link>https://r.example/a</link></item></rdf:RDF>`;
const POD = `<rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Pod</title><itunes:image href="https://p.example/art.jpg"/>
<item><title>Ep 1</title><enclosure url="https://p.example/1.mp3" type="audio/mpeg" length="1"/><guid isPermaLink="false">ep-1</guid>
<itunes:author>Host</itunes:author><pubDate>Fri, 09 Oct 2026 10:00:00 GMT</pubDate></item></channel></rss>`;
const LOLZ = '<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">]><rss><channel><title>&lol2;</title></channel></rss>';
const BSKY = { feed: [
  { post: { uri: 'at://did:plc:abc/app.bsky.feed.post/3k2', author: { did: 'did:plc:abc', handle: 'alice.bsky.social', displayName: 'Alice' },
    record: { text: 'Hello world\nsecond line', createdAt: '2026-10-09T12:00:00Z' }, indexedAt: '2026-10-09T12:00:01Z',
    embed: { $type: 'app.bsky.embed.images#view', images: [{ thumb: 'https://cdn.bsky.app/t.jpg', fullsize: 'https://cdn.bsky.app/f.jpg' }] } } },
  { post: { uri: 'at://did:plc:zzz/app.bsky.feed.post/9x', author: { did: 'did:plc:zzz', handle: 'bob.bsky.social', displayName: 'Bob' },
    record: { text: 'Bob post' }, indexedAt: '2026-10-08T00:00:00Z' },
  reason: { $type: 'app.bsky.feed.defs#reasonRepost', by: { did: 'did:plc:abc', handle: 'alice.bsky.social', displayName: 'Alice' }, indexedAt: '2026-10-09T13:00:00Z' } },
] };
const HTML_PAGE = '<!DOCTYPE html><html><head><title>Blog</title>'
  + '<link rel="alternate" type="application/atom+xml" href="/atom.xml"></head><body>hello</body></html>';

// ── 9.2.1–9.2.7 XML normaliser ────────────────────────────────────────────────

test('parseXmlFeed: RSS 2.0 (title, self link ignored, CDATA, entities, dates, ids)', () => {
  const f = P.parseXmlFeed(RSS);
  assert.equal(f.kind, 'rss');
  assert.equal(f.title, 'Example & Co');
  assert.equal(f.link, 'https://example.com/');
  assert.equal(f.items.length, 2);
  const [a, b] = f.items;
  assert.equal(a.id, 'post-2');
  assert.equal(a.title, 'Hello World');
  assert.equal(a.url, 'https://example.com/2');
  assert.equal(a.author, 'Ann');
  assert.equal(a.summary, 'Second');
  assert.equal(a.publishedAt.toISOString(), '2026-10-09T10:00:00.000Z');
  assert.equal(b.id, 'https://example.com/1');
});

test('parseXmlFeed: YouTube Atom, Reddit Atom, RDF and podcast shapes', () => {
  const yt = P.parseXmlFeed(YT);
  assert.equal(yt.kind, 'atom');
  assert.equal(yt.title, 'Chan');
  assert.equal(yt.items[0].id, 'yt:video:abc123');
  assert.equal(yt.items[0].url, 'https://www.youtube.com/watch?v=abc123');
  assert.equal(yt.items[0].author, 'Chan');
  assert.equal(yt.items[0].image, 'https://i.ytimg.com/vi/abc123/hqdefault.jpg');

  const rd = P.parseXmlFeed(REDDIT).items[0];
  assert.equal(rd.id, 't3_xyz');
  assert.equal(rd.author, 'u/kit');
  assert.equal(rd.summary, 'so cute');
  assert.equal(rd.image, 'https://b.thumbs.redditmedia.com/x.jpg');
  assert.equal(rd.url, 'https://www.reddit.com/r/aww/comments/xyz/cat/');

  const rdf = P.parseXmlFeed(RDF);
  assert.equal(rdf.kind, 'rdf');
  assert.equal(rdf.title, 'RDF Feed');
  assert.equal(rdf.items[0].id, 'https://r.example/a');

  const pod = P.parseXmlFeed(POD);
  assert.equal(pod.image, 'https://p.example/art.jpg');
  assert.equal(pod.items[0].id, 'ep-1');
  assert.equal(pod.items[0].url, 'https://p.example/1.mp3');
  assert.equal(pod.items[0].author, 'Host');
  assert.equal(pod.items[0].image, null);
});

test('parseXmlFeed: entity bombs never expand (billion laughs, nasty DOCTYPE variants)', () => {
  const t0 = Date.now();
  assert.ok(!P.parseXmlFeed(LOLZ).title.includes('lollol'));
  assert.ok(Date.now() - t0 < 200);

  // `]` and `>` inside entity values, nested references, a second DOCTYPE, an entity outside any DOCTYPE.
  const nasty = '<?xml version="1.0"?><!DOCTYPE x [<!ENTITY a "]>"><!ENTITY b "&a;&a;&a;&a;"><!ENTITY c "&b;&b;&b;&b;&b;&b;">]>'
    + '<!DOCTYPE y [<!ENTITY d "&c;&c;&c;">]><rss><channel><title>&c;&d;</title></channel></rss>';
  const t1 = Date.now();
  try {
    const f = P.parseXmlFeed(nasty);
    assert.ok(!(f.title ?? '').includes(']>'), 'entity expanded');
  } catch (err) {
    assert.ok(err instanceof P.FeedParseError);
  }
  assert.ok(Date.now() - t1 < 200);
  assert.throws(() => P.parseXmlFeed('<rss><channel><title>t</title><!ENTITY z "q"></channel></rss>'), P.FeedParseError);
  // Unterminated DOCTYPE: the rest of the document is dropped, so nothing is a feed.
  assert.throws(() => P.parseXmlFeed('<!DOCTYPE x [<!ENTITY a "b"><rss><channel><title>t</title></channel></rss>'), P.FeedParseError);
});

test('parseXmlFeed: not a feed', () => {
  assert.throws(() => P.parseXmlFeed('<html><body>hi</body></html>'), (e) => e instanceof P.FeedParseError && e.code === 'NOT_A_FEED');
  assert.throws(() => P.parseXmlFeed('not xml'), (e) => e instanceof P.FeedParseError && ['NOT_A_FEED', 'INVALID_XML'].includes(e.code));
  assert.throws(() => P.parseXmlFeed('<rss/>'), P.FeedParseError);
});

test('parseXmlFeed: hostile text does not hang (quadratic regex shapes)', () => {
  const big = '<'.repeat(150_000);
  const feed = `<rss><channel><title>t</title><item><title>${'&lt;'.repeat(20)}</title><guid>1</guid>`
    + `<description>${'&lt;script '.repeat(10_000)}</description></item></channel></rss>`;
  const t0 = Date.now();
  P.stripHtml(big);
  P.stripHtml('<script>'.repeat(30_000));
  P.stripHtml(' \n'.repeat(80_000));
  P.discoverFeedLinks('<link'.repeat(40_000), 'https://a.test/');
  P.stripDoctype('<!DOCTYPE'.repeat(30_000));
  P.parseXmlFeed(feed);
  assert.ok(Date.now() - t0 < 2000, `took ${Date.now() - t0} ms`);
});

test('parseXmlFeed: tolerates odd entries (no id, undated, namespaced, huge ids)', () => {
  const xml = `<rss><channel><title>t</title>
    <item></item>
    <item><title>Only title</title></item>
    <item><guid>${'g'.repeat(300)}</guid><title>Long</title><link>not a url</link></item>
    <item><title>Space</title><link>https://x.test/a b</link></item>
  </channel></rss>`;
  const f = P.parseXmlFeed(xml);
  assert.equal(f.items.length, 3);
  assert.equal(f.items[0].id, 'Only title|');
  assert.match(f.items[1].id, /^sha1:[0-9a-f]{40}$/);
  assert.equal(f.items[1].url, null);
  assert.equal(f.items[2].url, 'https://x.test/a%20b');
});

// ── 9.2.8–9 decoding, HTML ────────────────────────────────────────────────────

test('decodeXmlBuffer: declared charsets and BOM', () => {
  const latin = Buffer.concat([
    Buffer.from('<?xml version="1.0" encoding="ISO-8859-1"?><rss><channel><title>Caf'),
    Buffer.from([0xe9]),
    Buffer.from('</title></channel></rss>'),
  ]);
  assert.equal(P.parseXmlFeed(P.decodeXmlBuffer(latin, 'application/xml')).title, 'Café');
  assert.equal(P.parseXmlFeed(P.decodeXmlBuffer(latin, 'application/xml; charset=iso-8859-1')).title, 'Café');
  const bom = Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), Buffer.from('<rss/>')]);
  assert.equal(P.decodeXmlBuffer(bom, ''), '<rss/>');
  assert.equal(P.decodeXmlBuffer(Buffer.from('<rss/>'), 'text/xml; charset=nonsense-9'), '<rss/>');
});

test('looksLikeHtml and discoverFeedLinks', () => {
  assert.equal(P.looksLikeHtml('<!DOCTYPE html><html>', ''), true);
  assert.equal(P.looksLikeHtml('x', 'text/html; charset=utf-8'), true);
  assert.equal(P.looksLikeHtml(RSS, 'application/rss+xml'), false);
  assert.equal(P.looksLikeHtml(RSS, 'text/html'), false, 'a real feed mislabelled text/html is still a feed');
  assert.equal(P.looksLikeHtml('<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml">', 'text/html'), true);
  assert.deepEqual(P.discoverFeedLinks(
    '<head><link rel="alternate" type="application/rss+xml" href="/feed.xml"><link rel="alternate" type="application/feed+json" href="/f.json"></head>',
    'https://site.test/blog/',
  ), ['https://site.test/feed.xml']);
  assert.deepEqual(P.discoverFeedLinks(
    '<link href=\'a.xml?x=1&amp;y=2\' type="APPLICATION/ATOM+XML" rel="Alternate nofollow"><link rel="stylesheet" href="s.css"><link rel="alternate" type="application/rss+xml" href="javascript:alert(1)">',
    'https://site.test/blog/',
  ), ['https://site.test/blog/a.xml?x=1&y=2']);
  const many = Array.from({ length: 9 }, (_, i) => `<link rel="alternate" type="application/rss+xml" href="/f${i}.xml">`).join('');
  assert.equal(P.discoverFeedLinks(many, 'https://s.test/').length, 5);
});

// ── 9.2.10–13 JSON normalisers, item keys, HTML text ──────────────────────────

test('parseBlueskyAuthorFeed', () => {
  const f = P.parseBlueskyAuthorFeed(BSKY);
  assert.equal(f.kind, 'bluesky');
  assert.equal(f.items.length, 2);
  const [a, b] = f.items;
  assert.equal(a.id, 'at://did:plc:abc/app.bsky.feed.post/3k2');
  assert.equal(a.title, 'Hello world');
  assert.equal(a.url, 'https://bsky.app/profile/alice.bsky.social/post/3k2');
  assert.equal(a.image, 'https://cdn.bsky.app/f.jpg');
  assert.equal(a.isRepost, false);
  assert.equal(a.actorHandle, 'alice.bsky.social');
  assert.equal(b.isRepost, true);
  assert.ok(b.id.startsWith('repost:at://did:plc:zzz/'));
  assert.equal(b.author, 'Bob');
  assert.equal(b.actorName, 'Alice');
  assert.equal(b.publishedAt.toISOString(), '2026-10-09T13:00:00.000Z');
  const pinned = { feed: [{ ...BSKY.feed[0], reason: { $type: 'app.bsky.feed.defs#reasonPin' } }, BSKY.feed[1]] };
  assert.equal(P.parseBlueskyAuthorFeed(pinned).items.length, 1);
  assert.throws(() => P.parseBlueskyAuthorFeed({}), (e) => e.code === 'NOT_A_FEED');
  // handle.invalid falls back to the DID in the URL
  const inv = { feed: [{ post: { uri: 'at://did:plc:q/app.bsky.feed.post/r1', author: { did: 'did:plc:q', handle: 'handle.invalid' }, record: {}, indexedAt: '2026-10-09T00:00:00Z' } }] };
  const one = P.parseBlueskyAuthorFeed(inv).items[0];
  assert.equal(one.url, 'https://bsky.app/profile/did:plc:q/post/r1');
  assert.equal(one.title, '(post)');
  assert.equal(one.actorHandle, 'did:plc:q');
});

test('parseItunesSearch', () => {
  const r = P.parseItunesSearch({ results: [
    { collectionName: 'A', feedUrl: 'ftp://x' },
    { collectionName: 'B', artistName: 'Z', feedUrl: 'https://b.test/rss', artworkUrl100: 'https://a.test/i.jpg' },
  ] });
  assert.equal(r.length, 1);
  assert.equal(r[0].name, 'B');
  assert.equal(r[0].artist, 'Z');
  assert.equal(r[0].feedUrl, 'https://b.test/rss');
  assert.equal(r[0].artwork, 'https://a.test/i.jpg');
  assert.deepEqual(P.parseItunesSearch(null), []);
});

test('itemKey and stripHtml', () => {
  assert.equal(P.itemKey('a'.repeat(150)), 'a'.repeat(150));
  const long = P.itemKey('a'.repeat(151));
  assert.match(long, /^sha1:[0-9a-f]{40}$/);
  assert.equal(P.itemKey('a'.repeat(151)), long);
  assert.equal(P.ITEM_ID_MAX, 150);
  assert.equal(P.stripHtml('<p>A&amp;B</p><script>x</script>&#233;&#x41;'), 'A&B\néA');
  assert.equal(P.stripHtml('&amp;#65; &#0; &#xD800; &#1114112; &nbsp;x'), '&#65; x');
  assert.equal(P.stripHtml('a<br/>b<br>c</div>d<style>p{}</style>e'), 'a\nb\nc\nde');
  assert.equal(P.stripHtml(null), '');
});

// ── 9.2.14 input parsers ──────────────────────────────────────────────────────

test('normalizeFeedUrl', () => {
  assert.equal(P.normalizeFeedUrl('example.com/feed').url, 'https://example.com/feed');
  assert.equal(P.normalizeFeedUrl('feed://example.com/rss').url, 'https://example.com/rss');
  assert.equal(P.normalizeFeedUrl('feed:https://example.com/rss').url, 'https://example.com/rss');
  assert.equal(P.normalizeFeedUrl('<https://x.test/a#frag>').url, 'https://x.test/a');
  assert.equal(P.normalizeFeedUrl('ftp://x').ok, false);
  assert.equal(P.normalizeFeedUrl('https://u:p@x.test/').ok, false);
  assert.equal(P.normalizeFeedUrl('   ').ok, false);
  assert.equal(P.normalizeFeedUrl(`https://x.test/${'a'.repeat(600)}`).ok, false);
  assert.equal(P.normalizeFeedUrl('javascript:alert(1)').ok, false);
});

test('parseYouTubeChannelInput', () => {
  const id = (c) => `UC${c.repeat(22)}`;
  assert.deepEqual(P.parseYouTubeChannelInput(id('a')), { ok: true, channelId: id('a') });
  assert.equal(P.parseYouTubeChannelInput(`https://www.youtube.com/channel/${id('b')}/videos`).channelId, id('b'));
  assert.equal(P.parseYouTubeChannelInput(`https://www.youtube.com/feeds/videos.xml?channel_id=${id('c')}`).channelId, id('c'));
  for (const h of ['@mkbhd', 'youtube.com/@mkbhd', 'https://www.youtube.com/c/foo', 'https://youtube.com/user/foo']) {
    const r = P.parseYouTubeChannelInput(h);
    assert.equal(r.ok, false, h);
    assert.equal(r.handle, true, h);
  }
  for (const bad of ['https://www.youtube.com/watch?v=abc', 'UCshort', 'https://evil.test/youtube.com/channel/' + id('d'), 'https://youtu.be/abc', '']) {
    const r = P.parseYouTubeChannelInput(bad);
    assert.equal(r.ok, false, bad);
    assert.equal(r.handle, false, bad);
  }
});

test('parseSubredditInput', () => {
  for (const ok of ['r/aww', '/r/aww/', 'aww', 'https://www.reddit.com/r/aww/new/', 'r/aww/top', 'old.reddit.com/r/aww']) {
    assert.equal(P.parseSubredditInput(ok).name, 'aww', ok);
  }
  assert.equal(P.parseSubredditInput('r/AskReddit').name, 'AskReddit');
  for (const bad of ['u/spez', '/user/spez', 'https://reddit.com/user/spez', 'a', 'bad-name', 'https://evil.test/r/aww', 'https://www.reddit.com/', '']) {
    assert.equal(P.parseSubredditInput(bad).ok, false, bad);
  }
});

test('parseBlueskyActorInput', () => {
  assert.equal(P.parseBlueskyActorInput('@Alice.bsky.social').actor, 'alice.bsky.social');
  assert.equal(P.parseBlueskyActorInput('alice').actor, 'alice.bsky.social');
  assert.equal(P.parseBlueskyActorInput('https://bsky.app/profile/bob.example.com/post/1').actor, 'bob.example.com');
  assert.equal(P.parseBlueskyActorInput('https://bsky.app/profile/did:plc:abc123?x=1').actor, 'did:plc:abc123');
  assert.equal(P.parseBlueskyActorInput('did:plc:abc123').actor, 'did:plc:abc123');
  assert.equal(P.parseBlueskyActorInput('not a handle!').ok, false);
  assert.equal(P.parseBlueskyActorInput('').ok, false);
});

// ── 9.2.15–25 pure service helpers ────────────────────────────────────────────

test('windowItems', () => {
  const dated = S.windowItems([it('a', '2026-10-01T00:00Z'), it('b', '2026-10-03T00:00Z'), it('c', '2026-10-02T00:00Z')]);
  assert.deepEqual(dated.map((i) => i.id), ['b', 'c', 'a']);
  const mixed = S.windowItems([it('a', '2026-10-01T00:00Z'), it('b'), it('c', '2026-10-02T00:00Z')]);
  assert.deepEqual(mixed.map((i) => i.id), ['a', 'b', 'c']);
  assert.equal(S.windowItems(Array.from({ length: 60 }, (_, i) => it(`i${i}`))).length, 50);
  assert.equal(S.windowItems([it('x'), it('x'), it('y')]).length, 2, 'duplicate ids keep the first');
});

test('selectNewItems', () => {
  const baseline = D('2026-10-09T00:00:00Z');
  const ids = (a) => a.map((i) => i.id);
  // 1. first fetch seeds and posts nothing
  let r = S.selectNewItems({ items: [it('c'), it('b')], initialized: false });
  assert.deepEqual(r.toPost, []);
  assert.equal(r.reseed, true);
  assert.deepEqual(r.nextSeenIds, ['c', 'b']);
  // 2. one new item
  r = S.selectNewItems({
    items: [it('c', '2026-10-10T00:00Z'), it('b', '2026-10-09T12:00Z'), it('a', '2026-10-09T06:00Z')],
    seenIds: ['b', 'a'], initialized: true, baseline,
  });
  assert.deepEqual(ids(r.toPost), ['c']);
  assert.deepEqual(r.nextSeenIds.slice(0, 3), ['c', 'b', 'a']);
  // 3. flood: newest 5, oldest first
  const flood = Array.from({ length: 7 }, (_, i) => it(`n${7 - i}`, `2026-10-10T0${7 - i}:00Z`));
  r = S.selectNewItems({ items: [...flood, it('z', '2026-10-09T01:00Z')], seenIds: ['z'], initialized: true, baseline });
  assert.deepEqual(ids(r.toPost), ['n3', 'n4', 'n5', 'n6', 'n7']);
  assert.equal(r.overflow, 2);
  // 4. stale (archive churn)
  r = S.selectNewItems({
    items: [it('old', '2026-10-01T00:00Z'), it('b', '2026-10-09T01:00Z')], seenIds: ['b'], initialized: true, baseline,
  });
  assert.deepEqual(r.toPost, []);
  assert.equal(r.skippedOld, 1);
  assert.ok(r.nextSeenIds.includes('old'));
  // 5. undated GUID reset
  r = S.selectNewItems({ items: Array.from({ length: 8 }, (_, i) => it(`u${i}`)), seenIds: ['x', 'y'], initialized: true, baseline });
  assert.equal(r.reseed, true);
  assert.deepEqual(r.toPost, []);
  // 6. reposts
  const items = [it('r1', '2026-10-10T00:00Z', { isRepost: true }), it('p1', '2026-10-10T00:00Z'), it('p0', '2026-10-09T01:00Z')];
  r = S.selectNewItems({ items, seenIds: ['p0'], initialized: true, baseline, includeReposts: false });
  assert.deepEqual(ids(r.toPost), ['p1']);
  assert.ok(r.nextSeenIds.includes('r1'));
  r = S.selectNewItems({ items, seenIds: ['p0'], initialized: true, baseline, includeReposts: true });
  assert.equal(r.toPost.length, 2);
  // 7. cap
  const seen = Array.from({ length: 100 }, (_, i) => `s${i}`);
  const fresh = Array.from({ length: 50 }, (_, i) => it(`w${i}`, '2026-10-10T00:00Z'));
  r = S.selectNewItems({ items: fresh, seenIds: seen, initialized: true, baseline });
  assert.equal(r.nextSeenIds.length, 100);
  assert.deepEqual(r.nextSeenIds.slice(0, 50), fresh.map((i) => i.id));
  // nothing new
  r = S.selectNewItems({ items: [it('a')], seenIds: ['a'], initialized: true, baseline });
  assert.deepEqual(r.toPost, []);
  assert.equal(r.reseed, false);
});

test('computeNextPollAt', () => {
  const now = D('2026-10-10T00:00:00Z');
  const min = (o) => (S.computeNextPollAt({ now, rand: () => 0.5, ...o }) - now) / 60_000;
  assert.equal(min({ type: 'RSS' }), 15);
  assert.equal(min({ type: 'RSS', failureCount: 1 }), 30);
  assert.equal(min({ type: 'RSS', failureCount: 5 }), 480);
  assert.equal(min({ type: 'RSS', failureCount: 9 }), 480);
  assert.equal(min({ type: 'PODCAST', failureCount: 5 }), 720);
  assert.equal(min({ type: 'REDDIT', failureCount: 1, status: 403 }), 60);
  assert.equal(min({ type: 'REDDIT', failureCount: 0 }), 10);
  assert.equal(min({ type: 'BLUESKY' }), 5);
  assert.equal(min({ type: 'RSS', failureCount: 1, retryAfterMs: 7_200_000 }), 120);
  assert.equal(min({ type: 'RSS', retryAfterMs: 99 * 3_600_000 }), 360, 'Retry-After is honoured up to 6 h');
  assert.equal((S.computeNextPollAt({ type: 'RSS', now, rand: () => 0 }) - now) / 60_000, 13.5);
  assert.equal((S.computeNextPollAt({ type: 'RSS', now, rand: () => 1 }) - now) / 60_000, 16.5);
  assert.ok(S.computeNextPollAt({ type: 'NOPE', now }) > now);
});

test('describeFetchError', () => {
  const d = S.describeFetchError;
  assert.match(d('YOUTUBE', { code: 'HTTP', status: 404 }), /HTTP 404.*YouTube/);
  assert.match(d('REDDIT', { code: 'HTTP', status: 403 }), /HTTP 403.*Reddit/);
  assert.match(d('REDDIT', { code: 'HTTP', status: 429 }), /Reddit rate limit/);
  assert.equal(d('RSS', { code: 'TIMEOUT' }), 'Timed out after 10 s');
  assert.ok(d('RSS', { code: 'TOO_LARGE' }).includes('2 MB'));
  assert.ok(d('PODCAST', { code: 'TOO_LARGE' }).includes('10 MB'));
  assert.match(d('BLUESKY', { code: 'HTTP', status: 400, apiError: 'InvalidRequest' }), /unavailable \(InvalidRequest\)/);
  assert.match(d('RSS', { code: 'HTTP', status: 503 }), /having problems/);
  assert.match(d('RSS', { code: 'HTTP', status: 410 }), /removed/);
  assert.match(d('RSS', { code: 'HTTP', status: 401 }), /access denied/);
  assert.match(d('RSS', { code: 'DNS_FAILED' }), /Domain not found/);
  assert.match(d('RSS', { code: 'WHATEVER', message: 'odd' }), /odd/);
  assert.ok(d('RSS', { code: 'NETWORK', message: 'x'.repeat(500) }).length <= 200);
  for (const code of ['HTTP', 'TIMEOUT', 'TOO_LARGE', 'DNS_FAILED', 'BLOCKED_ADDRESS', 'NETWORK', 'HTML_PAGE', 'NOT_A_FEED', 'INVALID_XML', 'X']) {
    assert.ok(d('REDDIT', { code, status: 403, message: 'm'.repeat(400) }).length <= 200);
  }
});

test('feedStatus', () => {
  assert.deepEqual(S.feedStatus({ enabled: false, disabledReason: 'x' }), { icon: '⏸️', text: 'Disabled — x' });
  assert.equal(S.feedStatus({ enabled: false }).text, 'Disabled — paused');
  assert.equal(S.feedStatus({ enabled: true, failureCount: 3, lastError: 'HTTP 404' }).text, 'Failing 3× — HTTP 404');
  assert.equal(S.feedStatus({ enabled: true, failureCount: 0, lastError: "Can't post" }).icon, '⚠️');
  assert.equal(S.feedStatus({ enabled: true, failureCount: 0, initialized: false }).icon, '🕓');
  assert.deepEqual(S.feedStatus({ enabled: true, failureCount: 0, initialized: true }), { icon: '✅', text: 'OK' });
});

// Fakes. They differ slightly from the spec's (§9.2 case 20): pingableRoleIds (rule 23) reads `channel.guild`,
// and the role's id / mentionable flag.
const allPerms = { has: () => true };
const guild = {
  id: 'g', name: 'Guild', memberCount: 5, ownerId: '1', iconURL: () => null,
  roles: { cache: new Map([['42', { id: '42', mentionable: true }], ['43', { id: '43', mentionable: false }]]) },
  members: { me: { id: 'bot' } },
};
const channel = { id: 'c', name: 'news', type: 0, guild, permissionsFor: () => allPerms };

test('buildPostPayload', () => {
  const rssFeed = { type: 'RSS', displayName: 'Example', messageTemplate: '{title} {link}', pingRoleId: null };
  let p = S.buildPostPayload(rssFeed, it('i1', null, { title: 'a_b *c*', url: 'https://x.test/1' }), { guild, channel });
  assert.equal(p.content, 'a\\_b \\*c\\* <https://x.test/1>');
  assert.equal(p.embeds.length, 1);
  assert.equal(p.embeds[0].data.title, 'a_b *c*');
  assert.equal(p.embeds[0].data.url, 'https://x.test/1');
  assert.deepEqual(p.allowedMentions, { parse: [], roles: [] });

  const yt = { type: 'YOUTUBE', displayName: 'UCx', messageTemplate: null, pingRoleId: null };
  p = S.buildPostPayload(yt, it('v', null, { title: 'Vid', url: 'https://www.youtube.com/watch?v=abc123', author: 'Chan' }), { guild, channel });
  assert.ok(p.content.includes('**Chan** just posted'));
  assert.ok(p.content.includes('\nhttps://www.youtube.com/watch?v=abc123'));
  assert.ok(!p.content.includes('<https://'));
  assert.equal(p.embeds.length, 0);

  const pinged = { ...rssFeed, pingRoleId: '42' };
  p = S.buildPostPayload(pinged, it('i1'), { guild, channel });
  assert.ok(p.content.startsWith('<@&42> '));
  assert.deepEqual(p.allowedMentions, { parse: [], roles: ['42'] });
  p = S.buildPostPayload(pinged, it('i1'), { guild, channel, ping: false });
  assert.ok(p.content.startsWith('<@&42> '));
  assert.deepEqual(p.allowedMentions, { parse: [], roles: [] });

  // A role the bot cannot ping (not mentionable, no Mention Everyone in the channel) never reaches allowedMentions.
  const noPing = { ...channel, permissionsFor: () => ({ has: (bit) => bit !== PermissionFlagsBits.MentionEveryone }) };
  p = S.buildPostPayload({ ...rssFeed, pingRoleId: '43' }, it('i1'), { guild, channel: noPing });
  assert.deepEqual(p.allowedMentions, { parse: [], roles: [] });
  p = S.buildPostPayload({ ...rssFeed, pingRoleId: '43' }, it('i1'), { guild, channel });
  assert.deepEqual(p.allowedMentions, { parse: [], roles: ['43'] }, 'with Mention Everyone the bot may ping it');

  p = S.buildPostPayload({ ...rssFeed, pingRoleId: '99' }, it('i1'), { guild, channel });
  assert.ok(!p.content.includes('<@&'));
  assert.deepEqual(p.allowedMentions.roles, []);

  p = S.buildPostPayload({ ...rssFeed, messageTemplate: '{title}' }, it('i1', null, { title: '@everyone <@123> hi' }), { guild, channel });
  assert.deepEqual(p.allowedMentions.parse, []);

  p = S.buildPostPayload({ ...rssFeed, messageTemplate: '{title}' }, it('i1', null, { title: 'x'.repeat(3000) }), { guild, channel });
  assert.ok(p.content.length <= 2000);
  assert.ok(p.embeds[0].data.title.length <= 256);

  const bsky = { type: 'BLUESKY', displayName: '@alice.bsky.social', messageTemplate: null, pingRoleId: null };
  p = S.buildPostPayload(bsky, P.parseBlueskyAuthorFeed(BSKY).items[1], { guild, channel });
  assert.ok(p.content.includes('**Alice** (@alice.bsky.social)'));

  p = S.buildPostPayload({ type: 'REDDIT', displayName: 'r/AskReddit', messageTemplate: null, pingRoleId: null }, it('i1'), { guild, channel });
  assert.equal(p.content, '👽 New post in **r/AskReddit**');

  // a template that renders to nothing falls back to the link
  p = S.buildPostPayload({ ...yt, messageTemplate: '{user}' }, it('v', null, { url: 'https://y.test/v' }), { guild, channel });
  assert.ok(p.content);

  p = S.buildPostPayload(rssFeed, it('i1', null, { url: 'https://x.test/ok', image: 'https://x.test/i.png' }), { guild, channel });
  assert.equal(p.embeds[0].data.image.url, 'https://x.test/i.png');
  const podcast = { type: 'PODCAST', displayName: 'Pod', messageTemplate: null, pingRoleId: null };
  p = S.buildPostPayload(podcast, it('e1', null, { title: 'Ep 1' }), { guild, channel, feedImage: 'https://p.test/art.jpg' });
  assert.equal(p.embeds[0].data.thumbnail.url, 'https://p.test/art.jpg');
  assert.equal(p.content, '🎙️ New episode of **Pod**: Ep 1');
});

test('templateWarnings, parseMessageOption, displayNameUpdate, parseRetryAfterMs', () => {
  const w = S.templateWarnings('REDDIT', '{title} {subreddit} {youtube_channel} {user}');
  assert.equal(w.length, 1);
  assert.ok(w[0].includes('{youtube_channel}') && w[0].includes('{user}') && w[0].includes('{user…}'));
  assert.ok(S.templateWarnings('YOUTUBE', 'New video!').some((x) => x.includes('no {link}')));
  assert.deepEqual(S.templateWarnings('RSS', '{title} {server} {random:1:5} {channel.name} {random}'), []);
  assert.deepEqual(S.templateWarnings('YOUTUBE', 'x {Link}'), []);
  assert.deepEqual(S.templateWarnings('RSS', null), []);

  assert.deepEqual(S.parseMessageOption('a\\nb'), { template: 'a\nb' });
  assert.deepEqual(S.parseMessageOption('   '), { template: null });
  assert.deepEqual(S.parseMessageOption(null), { template: null });
  assert.ok(S.parseMessageOption('x'.repeat(1001)).error);

  assert.equal(S.displayNameUpdate({ type: 'YOUTUBE', source: 'UCx', displayName: 'UCx' }, { title: 'Chan', items: [] }), 'Chan');
  assert.equal(S.displayNameUpdate({ type: 'YOUTUBE', source: 'UCx', displayName: 'Chan' }, { title: 'Chan2', items: [] }), null);
  assert.equal(S.displayNameUpdate({ type: 'BLUESKY', displayName: '@old.bsky.social' }, { items: [{ actorHandle: 'new.bsky.social' }] }), '@new.bsky.social');
  assert.equal(S.displayNameUpdate({ type: 'BLUESKY', displayName: '@a.bsky.social' }, { items: [{ actorHandle: 'a.bsky.social' }] }), null);
  assert.equal(S.displayNameUpdate({ type: 'RSS', displayName: 'x' }, { title: 'y', items: [] }), null);

  const now = Date.parse('2026-10-10T00:00:00Z');
  assert.equal(S.parseRetryAfterMs('120'), 120000);
  assert.equal(S.parseRetryAfterMs(new Date(now + 30_000).toUTCString(), now), 30000);
  assert.equal(S.parseRetryAfterMs('soon'), null);
  assert.equal(S.parseRetryAfterMs(undefined), null);
});

test('keys, constants and sourceLink', () => {
  assert.equal(S.feedJobKey('abc'), 'feed.poll:abc');
  assert.match(S.fetchCacheKey('https://a.test/'), /^feed:fetch:[0-9a-f]{40}$/);
  assert.equal(S.cacheTtlSeconds('BLUESKY'), 270);
  assert.equal(S.cacheTtlSeconds('REDDIT'), 570);
  assert.equal(S.cacheTtlSeconds('PODCAST'), 1770);
  assert.equal(S.REDDIT_MAX_PER_MIN, 8);
  assert.equal(S.MAX_BYTES.PODCAST, 10 * 1048576);
  assert.equal(S.MAX_BYTES.RSS, 2 * 1048576);
  assert.equal(S.sourceLink({ type: 'YOUTUBE', source: 'UCx' }), '<https://www.youtube.com/channel/UCx>');
  assert.equal(S.sourceLink({ type: 'RSS', fetchUrl: 'https://a.test/f' }), '<https://a.test/f>');
  assert.equal(S.varHelp('PODCAST'), '`{title}` `{url}` `{link}` `{author}` `{feed}` `{podcast_title}` `{episode_title}`');
  assert.deepEqual([...S.FEED_CHANNEL_TYPES], [ChannelType.GuildText, ChannelType.GuildAnnouncement]);
});

// ── 9.2.26–27 command payload and plugin ─────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/feed payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'feed');
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.deepEqual(json.contexts, [0]);
  assert.equal(cmd.module, 'feeds');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(cmd.subcommandPermissions, undefined);
  assert.equal(cmd.allowInDMs, undefined);
  assert.equal(typeof cmd.autocomplete, 'function');
  assert.deepEqual(json.options.map((o) => o.name), ['add', 'list', 'remove', 'test', 'edit', 'enable', 'disable']);
  const add = json.options[0];
  assert.equal(add.type, 2);
  assert.deepEqual(add.options.map((o) => o.name), ['rss', 'youtube', 'reddit', 'bluesky', 'podcast']);
  const names = (o) => (o.options ?? []).map((x) => x.name);
  const sub = (n) => json.options.find((o) => o.name === n);
  assert.deepEqual(names(add.options[0]), ['url', 'channel', 'ping_role', 'message']);
  assert.deepEqual(names(add.options[1]), ['youtube_channel', 'channel', 'ping_role', 'message']);
  assert.deepEqual(names(add.options[2]), ['subreddit', 'channel', 'ping_role', 'message']);
  assert.deepEqual(names(add.options[3]), ['handle', 'channel', 'ping_role', 'message', 'include_reposts']);
  assert.deepEqual(names(add.options[4]), ['podcast', 'channel', 'ping_role', 'message']);
  assert.deepEqual(names(sub('edit')), ['feed', 'channel', 'ping_role', 'clear_ping_role', 'message', 'reset_message', 'include_reposts']);
  for (const n of ['remove', 'test', 'enable', 'disable']) assert.deepEqual(names(sub(n)), ['feed']);
  const all = [...add.options, ...json.options.filter((o) => o.name !== 'add')];
  for (const s of all) {
    for (const o of s.options ?? []) {
      if (o.name === 'channel') assert.deepEqual(o.channel_types, [0, 5]);
      if (o.name === 'feed') assert.equal(o.autocomplete, true);
    }
  }
  assert.ok(charCount(json) <= 2400, `payload is ${charCount(json)} characters`);
  assert.ok(add.options[0].options.find((o) => o.name === 'message').description.includes('\\n'));
});

test('plugin declaration', async () => {
  const { errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => /feeds/.test(e)), []);
  const { plugin } = pluginMod;
  assert.equal(plugin.id, 'feeds');
  assert.equal(plugin.module, 'feeds');
  assert.deepEqual(plugin.components, []);
  assert.deepEqual(plugin.provides, {});
  assert.equal(plugin.jobs.length, 1);
  assert.equal(plugin.jobs[0].type, 'feed.poll');
  assert.equal(plugin.jobs[0].handler, S.pollFeedJob);
  assert.deepEqual(plugin.hooks.map((h) => ({ event: h.event, id: h.id, always: h.always })), [
    { event: 'guildReady', id: 'feeds', always: true },
    { event: 'channelDelete', id: 'feeds', always: true },
  ]);
  const filter = plugin.hooks[1].filter;
  assert.equal(filter({ channel: { type: 0 } }), true);
  assert.equal(filter({ channel: { type: 5 } }), true);
  assert.equal(filter({ channel: { type: 2 } }), false);
  assert.equal(filter({}), false);
});

// ── Network paths through the fetch seam (no sockets) ─────────────────────────

const resp = (status, body = '', headers = {}, url = 'https://feed.test/f.xml') => {
  const buffer = Buffer.from(typeof body === 'string' ? body : JSON.stringify(body));
  return {
    ok: status >= 200 && status < 300, status, statusText: '', headers, url, buffer,
    text: () => buffer.toString('utf8'), json: () => JSON.parse(buffer.toString('utf8')),
  };
};
/** Installs a fetch stub; `route(url, opts)` returns a response or throws. Returns the recorded calls. */
function stubFetch(route) {
  const calls = [];
  S.__setFetchImpl({
    safeFetch: async (url, opts) => { calls.push({ url, opts }); return route(url, opts); },
    safeFetchJson: async (url, opts) => {
      calls.push({ url, opts, json: true });
      const r = await route(url, opts);
      if (!r.ok) throw new SafeFetchError('HTTP_ERROR', r.status, { status: r.status });
      return r.json();
    },
  });
  return calls;
}
const XML_HEADERS = { 'content-type': 'application/rss+xml', etag: '"abc"', 'last-modified': 'Fri, 09 Oct 2026 10:00:00 GMT' };

test('the default fetcher is the SSRF-safe one (loopback and private addresses are refused)', async () => {
  S.__setFetchImpl(null);
  for (const url of ['http://127.0.0.1:1/feed', 'http://[::1]/feed', 'http://169.254.169.254/latest', 'http://localhost/feed']) {
    const r = await S.fetchAndParse(url, 'RSS');
    assert.equal(r.ok, false, url);
    assert.equal(r.error.code, 'BLOCKED_ADDRESS', url);
  }
});

test('fetchAndParse: request options, parsing, validators, window', async () => {
  const calls = stubFetch(() => resp(200, RSS, XML_HEADERS));
  const r = await S.fetchAndParse('https://feed.test/f.xml', 'RSS', { headers: { 'if-none-match': '"old"' } });
  assert.equal(r.ok, true);
  assert.equal(r.notModified, false);
  assert.equal(r.parsed.items.length, 2);
  assert.deepEqual(r.parsed.items.map((i) => i.id), ['post-2', 'https://example.com/1'], 'newest first');
  assert.equal(r.etag, '"abc"');
  assert.equal(r.lastModified, 'Fri, 09 Oct 2026 10:00:00 GMT');
  const { opts } = calls[0];
  assert.equal(opts.timeoutMs, 10_000);
  assert.equal(opts.maxBytes, 2 * 1048576);
  assert.equal(opts.maxRedirects, 5);
  assert.equal(opts.headers['if-none-match'], '"old"');
  assert.match(opts.headers['user-agent'], /DiscordBot/);
  assert.match(opts.headers.accept, /rss\+xml/);
  await S.fetchAndParse('https://feed.test/p.xml', 'PODCAST');
  assert.equal(calls[1].opts.maxBytes, 10 * 1048576);
  await S.fetchAndParse('https://api.test/x', 'BLUESKY');
  assert.equal(calls[2].opts.headers.accept, 'application/json');
});

test('fetchAndParse: Reddit uses REDDIT_USER_AGENT when set', async () => {
  const calls = stubFetch(() => resp(200, REDDIT, XML_HEADERS));
  const old = process.env.REDDIT_USER_AGENT;
  process.env.REDDIT_USER_AGENT = 'discord-bot/1.0 (by /u/tester)';
  try {
    await S.fetchAndParse('https://www.reddit.com/r/aww/new/.rss', 'REDDIT');
    await S.fetchAndParse('https://feed.test/f.xml', 'RSS');
  } finally {
    if (old === undefined) delete process.env.REDDIT_USER_AGENT; else process.env.REDDIT_USER_AGENT = old;
  }
  assert.equal(calls[0].opts.headers['user-agent'], 'discord-bot/1.0 (by /u/tester)');
  assert.notEqual(calls[1].opts.headers['user-agent'], 'discord-bot/1.0 (by /u/tester)');
});

test('fetchAndParse: 304, HTTP errors, Retry-After, thrown SafeFetchErrors, HTML, bad bodies', async () => {
  const run = async (response, type = 'RSS') => {
    stubFetch(async () => { if (response instanceof Error) throw response; return response; });
    return S.fetchAndParse('https://feed.test/f.xml', type);
  };
  assert.deepEqual(await run(resp(304)), { ok: true, notModified: true });

  let r = await run(resp(429, 'slow down', { 'retry-after': '120' }), 'REDDIT');
  assert.equal(r.ok, false);
  assert.equal(r.error.code, 'HTTP');
  assert.equal(r.error.status, 429);
  assert.equal(r.error.retryAfterMs, 120_000);
  r = await run(resp(404), 'YOUTUBE');
  assert.equal(r.error.status, 404);
  assert.equal(r.error.retryAfterMs, null);
  r = await run(resp(400, { error: 'InvalidRequest' }), 'BLUESKY');
  assert.equal(r.error.apiError, 'InvalidRequest');

  r = await run(new SafeFetchError('TOO_LARGE', 'big'));
  assert.equal(r.error.code, 'TOO_LARGE');
  r = await run(new SafeFetchError('TIMEOUT', 'slow'));
  assert.equal(r.error.code, 'TIMEOUT');
  r = await run(new SafeFetchError('HTTP_ERROR', 502));
  assert.equal(r.error.code, 'HTTP');
  assert.equal(r.error.status, 502);
  r = await run(new TypeError('boom'));
  assert.equal(r.error.code, 'NETWORK');

  r = await run(resp(200, HTML_PAGE, { 'content-type': 'text/html; charset=utf-8' }, 'https://blog.test/'));
  assert.equal(r.error.code, 'HTML_PAGE');
  assert.deepEqual(r.error.links, ['https://blog.test/atom.xml']);
  r = await run(resp(200, '<html><body>hi</body></html>', { 'content-type': 'application/xml' }));
  assert.equal(r.error.code, 'HTML_PAGE', 'an html body is recognised even with an xml content type');
  r = await run(resp(200, '<note>hi</note>', { 'content-type': 'application/xml' }));
  assert.equal(r.error.code, 'NOT_A_FEED');
  r = await run(resp(200, '{"feed": 1}', {}), 'BLUESKY');
  assert.equal(r.error.code, 'NOT_A_FEED');
  r = await run(resp(200, 'not json', {}), 'BLUESKY');
  assert.equal(r.error.code, 'BAD_JSON');
  r = await run(resp(200, BSKY, {}), 'BLUESKY');
  assert.equal(r.parsed.items.length, 2);
});

const feedRow = (extra = {}) => ({
  id: 'f1', guildId: 'g', type: 'RSS', source: 'https://feed.test/f.xml', displayName: 'Feed', fetchUrl: 'https://feed.test/f.xml',
  channelId: 'c', initialized: true, etag: '"abc"', lastModified: 'Fri, 09 Oct 2026 10:00:00 GMT', ...extra,
});

test('fetchFeedSource: conditional headers only for initialised feeds; 304; cache fails open', async () => {
  const calls = stubFetch(() => resp(200, RSS, XML_HEADERS));
  let r = await S.fetchFeedSource(feedRow());
  assert.equal(r.ok, true);
  assert.equal(r.fromCache, false, 'Redis is down: fail-open to the network');
  assert.equal(calls[0].opts.headers['if-none-match'], '"abc"');
  assert.equal(calls[0].opts.headers['if-modified-since'], 'Fri, 09 Oct 2026 10:00:00 GMT');
  assert.equal(r.items.length, 2);
  assert.equal(r.etag, '"abc"');

  await S.fetchFeedSource(feedRow({ initialized: false }));
  assert.equal(calls[1].opts.headers['if-none-match'], undefined);
  await S.fetchFeedSource(feedRow(), { conditional: false });
  assert.equal(calls[2].opts.headers['if-none-match'], undefined);

  stubFetch(() => resp(304));
  r = await S.fetchFeedSource(feedRow());
  assert.deepEqual({ ok: r.ok, notModified: r.notModified }, { ok: true, notModified: true });

  stubFetch(() => resp(500));
  r = await S.fetchFeedSource(feedRow());
  assert.equal(r.ok, false);
  assert.equal(r.error.status, 500);
});

test('fetchFeedSource: bot-wide Reddit budget (in-process fallback while Redis is down)', async () => {
  S.__resetLocalState();
  const calls = stubFetch(() => resp(200, REDDIT, XML_HEADERS));
  const reddit = feedRow({ type: 'REDDIT', fetchUrl: 'https://www.reddit.com/r/aww/new/.rss' });
  for (let i = 0; i < S.REDDIT_MAX_PER_MIN; i++) assert.equal((await S.fetchFeedSource(reddit)).ok, true);
  assert.equal(calls.length, S.REDDIT_MAX_PER_MIN);
  const over = await S.fetchFeedSource(reddit);
  assert.equal(over.ok, false);
  assert.equal(over.budget, true, 'the ninth fetch in a minute is refused without touching the network');
  assert.equal(calls.length, S.REDDIT_MAX_PER_MIN);
  assert.ok(S.describeFetchError('REDDIT', over.error).length > 0);
  // other sources are not affected
  assert.equal((await S.fetchFeedSource(feedRow())).ok, true);
  S.__resetLocalState();
  assert.equal((await S.fetchFeedSource(reddit)).ok, true);
});

test('resolveFeedSource: RSS (direct, autodiscovery, refusals)', async () => {
  const route = (url) => {
    if (url === 'https://blog.test/') return resp(200, HTML_PAGE, { 'content-type': 'text/html' }, url);
    if (url === 'https://blog.test/atom.xml') return resp(200, YT, XML_HEADERS, url);
    if (url === 'https://plain.test/') return resp(200, '<html><body>no feeds</body></html>', { 'content-type': 'text/html' }, url);
    if (url === 'https://loop.test/') return resp(200, `<html><head><link rel="alternate" type="application/rss+xml" href="/again"></head></html>`, { 'content-type': 'text/html' }, url);
    if (url === 'https://loop.test/again') return resp(200, '<html></html>', { 'content-type': 'text/html' }, url);
    if (url === 'https://big.test/') throw new SafeFetchError('TOO_LARGE', 'big');
    if (url === 'https://bad.test/') return resp(200, '<note/>', {});
    if (url === 'https://gone.test/') return resp(404);
    return resp(200, RSS, XML_HEADERS, url);
  };
  stubFetch(route);
  let r = await S.resolveFeedSource('RSS', { url: 'https://feed.test/f.xml' });
  assert.equal(r.ok, true);
  assert.equal(r.source, 'https://feed.test/f.xml');
  assert.equal(r.fetchUrl, 'https://feed.test/f.xml');
  assert.equal(r.displayName, 'Example & Co');
  assert.equal(r.probe.ok, true);
  assert.equal(r.probe.items.length, 2);
  assert.equal(r.probe.etag, '"abc"');

  r = await S.resolveFeedSource('RSS', { url: 'https://blog.test/' });
  assert.equal(r.ok, true);
  assert.equal(r.fetchUrl, 'https://blog.test/atom.xml', 'one autodiscovery hop');
  assert.equal(r.displayName, 'Chan');

  r = await S.resolveFeedSource('RSS', { url: 'https://plain.test/' });
  assert.deepEqual([r.ok, r.title], [false, 'Not a Feed']);
  assert.match(r.message, /website, not an RSS\/Atom feed/);
  r = await S.resolveFeedSource('RSS', { url: 'https://loop.test/' });
  assert.deepEqual([r.ok, r.title], [false, 'Not a Feed']);
  r = await S.resolveFeedSource('RSS', { url: 'https://big.test/' });
  assert.equal(r.title, 'Feed Too Large');
  assert.match(r.message, /2 MB/);
  r = await S.resolveFeedSource('PODCAST', { url: 'https://big.test/' });
  assert.match(r.message, /10 MB/);
  r = await S.resolveFeedSource('RSS', { url: 'https://bad.test/' });
  assert.equal(r.title, 'Not a Feed');
  r = await S.resolveFeedSource('RSS', { url: 'https://gone.test/' });
  assert.equal(r.title, 'Could Not Fetch');
  assert.match(r.message, /HTTP 404/);

  // a hostname-only fallback when the feed has no title
  stubFetch(() => resp(200, '<rss><channel><item><guid>1</guid></item></channel></rss>', {}));
  r = await S.resolveFeedSource('RSS', { url: 'https://notitle.test/rss' });
  assert.equal(r.displayName, 'notitle.test');
});

test('resolveFeedSource: YouTube (accepted even on 404), Reddit, Bluesky', async () => {
  const id = `UC${'a'.repeat(22)}`;
  stubFetch(() => resp(200, YT, XML_HEADERS));
  let r = await S.resolveFeedSource('YOUTUBE', { channelId: id });
  assert.equal(r.ok, true);
  assert.equal(r.source, id);
  assert.equal(r.fetchUrl, `https://www.youtube.com/feeds/videos.xml?channel_id=${id}`);
  assert.equal(r.displayName, 'Chan');
  assert.equal(r.probe.ok, true);
  assert.deepEqual(r.warnings, []);

  stubFetch(() => resp(404, 'nope'));
  r = await S.resolveFeedSource('YOUTUBE', { channelId: id });
  assert.equal(r.ok, true, 'a 404 at add time is accepted');
  assert.equal(r.displayName, id);
  assert.equal(r.probe.ok, false);
  assert.equal(r.probe.error.status, 404);
  assert.match(r.warnings[0], /unreliable since late 2025/);

  S.__resetLocalState();
  stubFetch(() => resp(200, REDDIT, XML_HEADERS));
  r = await S.resolveFeedSource('REDDIT', { name: 'Aww' });
  assert.equal(r.ok, true);
  assert.equal(r.source, 'aww');
  assert.equal(r.displayName, 'r/Aww');
  assert.equal(r.fetchUrl, 'https://www.reddit.com/r/Aww/new/.rss');
  assert.equal(r.probe.ok, true);
  stubFetch(() => resp(404));
  r = await S.resolveFeedSource('REDDIT', { name: 'nothere' });
  assert.deepEqual([r.ok, r.title], [false, 'Subreddit Not Found']);
  stubFetch(() => resp(403, 'blocked'));
  r = await S.resolveFeedSource('REDDIT', { name: 'aww' });
  assert.equal(r.ok, true, 'a 403 is accepted: Reddit is best-effort');
  assert.equal(r.probe.ok, false);
  assert.match(r.warnings[0], /best-effort/);
  stubFetch(() => resp(429, 'slow', { 'retry-after': '30' }));
  r = await S.resolveFeedSource('REDDIT', { name: 'aww' });
  assert.equal(r.ok, true);
  assert.equal(r.probe.error.status, 429);
  // over the bot-wide budget: the probe is skipped, nothing is fetched
  S.__resetLocalState();
  const calls = stubFetch(() => resp(200, REDDIT, XML_HEADERS));
  for (let i = 0; i < S.REDDIT_MAX_PER_MIN; i++) await S.fetchFeedSource(feedRow({ type: 'REDDIT', fetchUrl: 'https://www.reddit.com/r/x/new/.rss' }));
  const before = calls.length;
  r = await S.resolveFeedSource('REDDIT', { name: 'aww' });
  assert.equal(r.ok, true);
  assert.equal(r.probe.ok, false);
  assert.equal(calls.length, before, 'no Reddit request when over budget');
  S.__resetLocalState();

  const did = 'did:plc:abc123';
  const routeBsky = (url) => {
    if (url.includes('getProfile')) {
      if (url.includes('missing')) return resp(400, { error: 'InvalidRequest', message: 'Profile not found' });
      if (url.includes('weird')) return resp(200, { did: 'not-a-did' });
      if (url.includes('down')) return resp(502);
      return resp(200, { did, handle: 'alice.bsky.social' });
    }
    return resp(200, BSKY);
  };
  const bsky = stubFetch(routeBsky);
  r = await S.resolveFeedSource('BLUESKY', { actor: 'alice.bsky.social' });
  assert.equal(r.ok, true);
  assert.equal(r.source, did);
  assert.equal(r.displayName, '@alice.bsky.social');
  assert.equal(r.fetchUrl, `${S.BSKY_APPVIEW}/xrpc/app.bsky.feed.getAuthorFeed?actor=${encodeURIComponent(did)}&filter=posts_no_replies&limit=30`);
  assert.equal(r.probe.items.length, 2);
  assert.equal(bsky[0].opts.maxBytes, 256 * 1024);
  r = await S.resolveFeedSource('BLUESKY', { actor: 'missing.bsky.social' });
  assert.deepEqual([r.ok, r.title], [false, 'Account Not Found']);
  r = await S.resolveFeedSource('BLUESKY', { actor: 'weird.bsky.social' });
  assert.equal(r.title, 'Bluesky Unavailable');
  r = await S.resolveFeedSource('BLUESKY', { actor: 'down.bsky.social' });
  assert.equal(r.title, 'Bluesky Unavailable');
  stubFetch((url) => (url.includes('getProfile') ? resp(200, { did, handle: 'handle.invalid' }) : resp(503)));
  r = await S.resolveFeedSource('BLUESKY', { actor: did });
  assert.equal(r.ok, true);
  assert.equal(r.displayName, `@${did}`);
  assert.equal(r.probe.ok, false);
  assert.match(r.warnings[0], /couldn't load its posts yet/);
});

test('resolveFeedSource: podcasts (iTunes search, then the feed)', async () => {
  const search = { results: [{ collectionName: 'The Pod', artistName: 'Host', feedUrl: 'https://pod.test/rss', artworkUrl600: 'https://pod.test/a.jpg' }] };
  const calls = stubFetch((url) => {
    if (url.startsWith('https://itunes.apple.com/search')) {
      if (url.includes('nothing')) return resp(200, { results: [] });
      if (url.includes('broken')) return resp(500);
      return resp(200, search);
    }
    return resp(200, POD, XML_HEADERS, url);
  });
  let r = await S.resolveFeedSource('PODCAST', { query: 'the pod' });
  assert.equal(r.ok, true);
  assert.equal(r.displayName, 'The Pod');
  assert.equal(r.source, 'https://pod.test/rss');
  assert.equal(r.probe.items.length, 1);
  assert.match(r.warnings[0], /Matched \*\*The Pod\*\* by Host/);
  assert.equal(calls[0].url, 'https://itunes.apple.com/search?media=podcast&entity=podcast&limit=5&term=the%20pod');
  assert.equal(calls[0].opts.maxBytes, 512 * 1024);
  assert.equal(calls[1].opts.maxBytes, 10 * 1048576);

  r = await S.resolveFeedSource('PODCAST', { query: 'nothing' });
  assert.deepEqual([r.ok, r.title], [false, 'Podcast Not Found']);
  r = await S.resolveFeedSource('PODCAST', { query: 'broken' });
  assert.equal(r.title, 'Podcast Search Failed');
  r = await S.resolveFeedSource('PODCAST', { url: 'https://pod.test/rss' });
  assert.equal(r.ok, true);
  assert.equal(r.displayName, 'Pod');
  assert.deepEqual(r.warnings, []);
  r = await S.resolveFeedSource('NOPE', {});
  assert.equal(r.ok, false);
});

test('resolveFeedSource never throws (unexpected failures are caught)', async () => {
  S.__setFetchImpl({ safeFetch: async () => { throw new Error('kaboom'); } });
  const r = await S.resolveFeedSource('RSS', { url: 'https://x.test/' });
  assert.equal(r.ok, false);
  assert.equal(r.title, 'Could Not Fetch');
  S.__setFetchImpl({ safeFetch: () => { throw new RangeError('sync throw'); } });
  const r2 = await S.resolveFeedSource('YOUTUBE', { channelId: `UC${'b'.repeat(22)}` });
  assert.equal(r2.ok, true);
  assert.equal(r2.probe.ok, false);
});

// ── DB cases (TEST_DATABASE_URL) ──────────────────────────────────────────────

const { getJob, cancelJobs } = await import('../src/services/scheduler.js');
const { updateSettings } = await import('../src/services/settingsService.js');

/** Builds a feed-shaped RSS document whose items are fresh (inside the stale window). */
const rssXml = (items, title = 'Example') => `<rss version="2.0"><channel><title>${title}</title>${items.map((i) => (
  `<item><title>${i.title ?? i.id}</title><guid isPermaLink="false">${i.id}</guid><link>https://example.com/${i.id}</link>`
  + `<pubDate>${new Date(i.at ?? Date.now() - 3_600_000).toUTCString()}</pubDate></item>`
)).join('')}</channel></rss>`;
const rssRes = (items, headers = XML_HEADERS) => resp(200, rssXml(items), headers);

function fakeGuild(id) {
  const g = {
    id, available: true, name: 'Test', memberCount: 3, ownerId: '1', iconURL: () => null,
    roles: { cache: new Map() }, members: { me: { id: 'bot' } },
    channels: { cache: new Map(), fetch: async () => null },
  };
  return g;
}
function fakeChannel(guild, { id = '900000000000000001', name = 'news', type = 0 } = {}) {
  const denied = new Set();
  const sent = [];
  const ch = {
    id, name, type, guild, sent, denied, failWith: null,
    isTextBased: () => true,
    isThread: () => false,
    permissionsFor: () => ({ has: (bit) => !denied.has(bit) }),
    send: async (payload) => {
      if (ch.failWith) throw ch.failWith;
      sent.push(payload);
      return { id: String(sent.length) };
    },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}
const discordErr = (code, message = 'discord error') => Object.assign(new Error(message), { code });

const GUILDS = Array.from({ length: 14 }, (_, i) => `955030${String(i + 1).padStart(12, '0')}`);
const [G_CREATE, G_FIND, G_POLL, G_FAIL, G_CHAN, G_MISC, G_CMD, G_CAP, G_MODOFF, G_DEL, G_JOBS, G_RACE, G_REDDIT, G_SPARE] = GUILDS;
after(async () => {
  if (DB) {
    await prisma.scheduledJob.deleteMany({ where: { guildId: { in: GUILDS } } }).catch(() => {});
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: GUILDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

/** A guild, a channel and a way to build poll jobs/contexts for a feed row. */
function harness(guildId, chanOpts) {
  const guild = fakeGuild(guildId);
  const channel = fakeChannel(guild, chanOpts);
  return {
    guild, channel,
    ctx: { client: {}, guild, lateByMs: 0, attempt: 1 },
    job: (feed) => ({ id: 'job', guildId, type: 'feed.poll', payload: { feedId: feed.id }, attempts: 1, maxAttempts: 10 }),
    poll: (feed) => S.pollFeedJob({ id: 'job', guildId, type: 'feed.poll', payload: { feedId: feed.id }, attempts: 1, maxAttempts: 10 }, { client: {}, guild, lateByMs: 0, attempt: 1 }),
    row: (feed) => prisma.feed.findUnique({ where: { id: feed.id } }),
  };
}
const mkFeed = (h, guildId, extra = {}) => S.createFeedRow({
  guildId, type: 'RSS', source: `https://feed.test/${Math.random().toString(36).slice(2)}.xml`, displayName: 'Test feed',
  fetchUrl: 'https://feed.test/f.xml', channelId: h.channel.id, createdBy: 'u1', ...extra,
});
const within = (date, fromMs, toMs, now = Date.now()) => date.getTime() >= now + fromMs && date.getTime() <= now + toMs;

test('createFeedRow: seeded by a good probe, unseeded by a failed one, P2002 mapped', { skip: skipDb }, async () => {
  const items = [it('a'), it('b'), it('b')];
  const row = await S.createFeedRow({
    guildId: G_CREATE, type: 'RSS', source: 'https://s.test/1', displayName: 'x'.repeat(300), fetchUrl: 'https://s.test/1',
    channelId: '1', createdBy: 'u', probe: { ok: true, items, etag: '"e"', lastModified: 'lm' },
  });
  assert.equal(row.initialized, true);
  assert.deepEqual(row.seenIds, ['a', 'b']);
  assert.equal(row.etag, '"e"');
  assert.equal(row.displayName.length, 100);
  assert.ok(row.lastSuccessAt && row.lastCheckedAt);
  assert.equal(row.lastError, null);

  const failed = await S.createFeedRow({
    guildId: G_CREATE, type: 'YOUTUBE', source: 'UCx', displayName: 'UCx', fetchUrl: 'https://y.test', channelId: '1', createdBy: 'u',
    probe: { ok: false, error: { code: 'HTTP', status: 404, message: 'HTTP 404' } },
  });
  assert.equal(failed.initialized, false);
  assert.deepEqual(failed.seenIds, []);
  assert.equal(failed.lastSuccessAt, null);
  assert.ok(failed.lastCheckedAt);
  assert.match(failed.lastError, /HTTP 404/);
  assert.equal(failed.failureCount, 0);

  await assert.rejects(
    S.createFeedRow({ guildId: G_CREATE, type: 'RSS', source: 'https://s.test/1', displayName: 'dup', fetchUrl: 'x', channelId: '1', createdBy: 'u' }),
    (e) => e instanceof S.FeedUserError && e.title === 'Already Following',
  );
  const rb = await S.createFeedRow({
    guildId: G_CREATE, type: 'RSS', source: 'https://s.test/9', displayName: 'r', fetchUrl: 'x', channelId: '1', includeReposts: true, createdBy: 'u',
  });
  assert.equal(rb.includeReposts, false, 'includeReposts is Bluesky-only');
});

test('createFeedRow: the per-guild cap holds under concurrent adds', { skip: skipDb }, async () => {
  await S.createFeedRow({ guildId: G_CAP, type: 'RSS', source: 'seed', displayName: 's', fetchUrl: 'x', channelId: '1', createdBy: 'u' });
  await prisma.feed.createMany({
    data: Array.from({ length: LIMITS.FEEDS - 3 }, (_, i) => ({
      guildId: G_CAP, type: 'RSS', source: `bulk-${i}`, displayName: `b${i}`, fetchUrl: 'x', channelId: '1', createdBy: 'u',
    })),
  });
  assert.equal(await prisma.feed.count({ where: { guildId: G_CAP } }), LIMITS.FEEDS - 2);
  const results = await Promise.allSettled(Array.from({ length: 6 }, (_, i) => S.createFeedRow({
    guildId: G_CAP, type: 'RSS', source: `race-${i}`, displayName: `r${i}`, fetchUrl: 'x', channelId: '1', createdBy: 'u',
  })));
  assert.equal(results.filter((r) => r.status === 'fulfilled').length, 2);
  const rejected = results.filter((r) => r.status === 'rejected');
  assert.equal(rejected.length, 4);
  assert.ok(rejected.every((r) => r.reason instanceof S.FeedUserError && r.reason.title === 'Feed Limit Reached'));
  assert.equal(await prisma.feed.count({ where: { guildId: G_CAP } }), LIMITS.FEEDS);
});

test('findFeedForGuild: by id, by unique name, guild-scoped', { skip: skipDb }, async () => {
  const mk = (guildId, source, displayName) => S.createFeedRow({ guildId, type: 'RSS', source, displayName, fetchUrl: 'x', channelId: '1', createdBy: 'u' });
  const a = await mk(G_FIND, 's1', 'Alpha Feed');
  await mk(G_FIND, 's2', 'Twin');
  await mk(G_FIND, 's3', 'twin');
  const other = await mk(G_SPARE, 's1', 'Other');
  assert.equal((await S.findFeedForGuild(G_FIND, a.id)).id, a.id);
  assert.equal((await S.findFeedForGuild(G_FIND, 'alpha feed')).id, a.id);
  assert.equal(await S.findFeedForGuild(G_FIND, 'twin'), null, 'ambiguous names match nothing');
  assert.equal(await S.findFeedForGuild(G_FIND, other.id), null, "another guild's id never matches");
  assert.equal(await S.findFeedForGuild(G_FIND, 'nope'), null);
});

test('poll: the first fetch seeds without posting; later polls post only new items', { skip: skipDb }, async () => {
  const h = harness(G_POLL);
  const feed = await mkFeed(h, G_POLL);
  const calls = stubFetch(() => rssRes([{ id: 'p1' }, { id: 'p2' }]));
  let out = await h.poll(feed);
  assert.equal(h.channel.sent.length, 0, 'no backlog dump');
  assert.ok(within(out.nextRunAt, 13 * 60_000, 17 * 60_000));
  assert.equal(calls[0].opts.headers['if-none-match'], undefined, 'no conditional headers before the first success');
  let row = await h.row(feed);
  assert.equal(row.initialized, true);
  assert.deepEqual([...row.seenIds].sort(), ['p1', 'p2']);
  assert.equal(row.failureCount, 0);
  assert.equal(row.lastError, null);
  assert.equal(row.etag, '"abc"');
  assert.ok(row.lastSuccessAt);
  assert.equal(row.lastPostedAt, null);

  stubFetch(() => rssRes([{ id: 'p3', title: 'Brand new' }, { id: 'p1' }, { id: 'p2' }]));
  out = await h.poll(feed);
  assert.equal(h.channel.sent.length, 1);
  assert.ok(h.channel.sent[0].content.includes('Test feed'));
  assert.equal(h.channel.sent[0].embeds[0].data.title, 'Brand new');
  assert.deepEqual(h.channel.sent[0].allowedMentions, { parse: [], roles: [] });
  assert.ok(within(out.nextRunAt, 13 * 60_000, 17 * 60_000));
  row = await h.row(feed);
  assert.ok(row.seenIds.includes('p3'));
  assert.ok(row.lastPostedAt);

  // The same state again (a restart, a re-run): nothing is reposted.
  await h.poll(feed);
  assert.equal(h.channel.sent.length, 1);

  // Conditional headers go out once initialised, and a 304 refreshes the success time.
  const c2 = stubFetch(() => resp(304));
  await prisma.feed.update({ where: { id: feed.id }, data: { failureCount: 3, lastError: 'HTTP 500' } });
  out = await h.poll(feed);
  assert.equal(c2[0].opts.headers['if-none-match'], '"abc"');
  assert.ok(within(out.nextRunAt, 13 * 60_000, 17 * 60_000), 'a 304 is a success: no backoff');
  row = await h.row(feed);
  assert.equal(row.failureCount, 0);
  assert.equal(row.lastError, null);
});

test('poll: the updatedAt claim stops a parallel run from posting twice', { skip: skipDb }, async () => {
  const h = harness(G_RACE);
  const feed = await mkFeed(h, G_RACE, { probe: { ok: true, items: [it('q1')], etag: null, lastModified: null } });
  stubFetch(() => rssRes([{ id: 'q2' }, { id: 'q1' }]));
  const [a, b] = await Promise.all([h.poll(feed), h.poll(feed)]);
  assert.equal(h.channel.sent.length, 1, 'exactly one of two parallel runs posts');
  assert.ok(a?.nextRunAt && b?.nextRunAt);

  // A stale claim (the row changed since the run read it) posts nothing and retries in a minute.
  await prisma.feed.update({ where: { id: feed.id }, data: { seenIds: ['q1'] } });
  const stale = await prisma.feed.findUnique({ where: { id: feed.id } });
  await prisma.feed.update({ where: { id: feed.id }, data: { messageTemplate: 'edited {title}' } });
  stubFetch(() => rssRes([{ id: 'q9' }, { id: 'q1' }]));
  const realFind = prisma.feed.findFirst;
  prisma.feed.findFirst = async () => stale; // the run read the row before the edit
  let out;
  try {
    out = await h.poll(feed);
  } finally {
    prisma.feed.findFirst = realFind;
  }
  assert.equal(h.channel.sent.length, 1, 'no new post');
  assert.ok(within(out.nextRunAt, 50_000, 70_000));
  const row = await h.row(feed);
  assert.ok(!row.seenIds.includes('q9'), 'nothing was recorded either');
});

test('poll: at most 5 posts, oldest first, everything recorded', { skip: skipDb }, async () => {
  const h = harness(G_POLL);
  const feed = await mkFeed(h, G_POLL, { probe: { ok: true, items: [it('old')], etag: null, lastModified: null } });
  const base = Date.now() - 7_200_000;
  const items = Array.from({ length: 8 }, (_, i) => ({ id: `n${8 - i}`, at: base + (8 - i) * 60_000 })); // n8 newest
  stubFetch(() => rssRes([...items, { id: 'old', at: base - 3_600_000 }]));
  const before = h.channel.sent.length;
  await h.poll(feed);
  const posted = h.channel.sent.slice(before).map((p) => p.embeds[0].data.title);
  assert.deepEqual(posted, ['n4', 'n5', 'n6', 'n7', 'n8']);
  const row = await h.row(feed);
  for (let i = 1; i <= 8; i++) assert.ok(row.seenIds.includes(`n${i}`));
});

test('poll: a failed send un-records the item and is retried; 50035 skips it for good', { skip: skipDb }, async () => {
  const h = harness(G_POLL);
  const feed = await mkFeed(h, G_POLL, { probe: { ok: true, items: [it('base')], etag: null, lastModified: null } });
  stubFetch(() => rssRes([{ id: 'r1' }, { id: 'base' }]));
  h.channel.failWith = discordErr(50013, 'Missing Permissions');
  const sentBefore = h.channel.sent.length;
  const out = await h.poll(feed);
  assert.ok(out.nextRunAt);
  let row = await h.row(feed);
  assert.ok(!row.seenIds.includes('r1'), 'un-recorded');
  assert.match(row.lastError, /Can't post in #news: Missing Permissions/);
  assert.equal(row.failureCount, 0, 'channel problems never count as failures');
  assert.equal(row.enabled, true);

  h.channel.failWith = null;
  await h.poll(feed);
  assert.equal(h.channel.sent.length, sentBefore + 1, 'retried next poll');
  row = await h.row(feed);
  assert.ok(row.seenIds.includes('r1'));
  assert.equal(row.lastError, null, 'a successful poll clears the channel problem');

  stubFetch(() => rssRes([{ id: 'bad1' }, { id: 'r1' }, { id: 'base' }]));
  h.channel.failWith = discordErr(50035, 'Invalid Form Body');
  await h.poll(feed);
  row = await h.row(feed);
  assert.ok(row.seenIds.includes('bad1'), 'a permanently rejected item stays recorded');
  assert.equal(row.lastError, null);
});

test('poll: source failures back off, count, and the 20th disables with exactly one notice', { skip: skipDb }, async () => {
  const h = harness(G_FAIL);
  const feed = await mkFeed(h, G_FAIL, { probe: { ok: true, items: [it('x')], etag: null, lastModified: null } });
  await prisma.feed.update({ where: { id: feed.id }, data: { failureCount: 18, lastError: 'HTTP 500' } });
  stubFetch(() => resp(500));
  let out = await h.poll(feed);
  let row = await h.row(feed);
  assert.equal(row.failureCount, 19);
  assert.match(row.lastError, /HTTP 500 — the site is having problems/);
  assert.ok(within(out.nextRunAt, 7 * 3_600_000, 9 * 3_600_000), 'capped exponential backoff (8 h at ×32)');
  assert.equal(h.channel.sent.length, 0);

  out = await h.poll(feed);
  assert.equal(out, undefined, 'the job is dropped');
  row = await h.row(feed);
  assert.equal(row.enabled, false);
  assert.match(row.disabledReason, /^20 failed checks in a row: HTTP 500/);
  assert.equal(h.channel.sent.length, 1, 'exactly one notice');
  assert.equal(h.channel.sent[0].embeds[0].data.title, '⚠️  Feed Disabled');
  assert.match(h.channel.sent[0].embeds[0].data.description, /\/feed enable/);

  assert.equal(await h.poll(feed), undefined);
  assert.equal(h.channel.sent.length, 1, 'a disabled feed is never polled again');

  // The conditional disable is what guarantees one notice, even under a race.
  const feed2 = await mkFeed(h, G_FAIL);
  const r = await Promise.all([S.disableFeed(feed2, 'x', h.ctx, null), S.disableFeed(feed2, 'x', h.ctx, null)]);
  assert.deepEqual(r.sort(), [false, true]);
});

test('poll: Reddit 429 waits without counting a failure; over-budget polls touch nothing', { skip: skipDb }, async () => {
  const h = harness(G_REDDIT);
  const reddit = await mkFeed(h, G_REDDIT, {
    type: 'REDDIT', source: 'aww', displayName: 'r/aww', fetchUrl: 'https://www.reddit.com/r/aww/new/.rss',
    probe: { ok: true, items: [it('t3_a')], etag: null, lastModified: null },
  });
  S.__resetLocalState();
  stubFetch(() => resp(429, 'slow', { 'retry-after': '1800' }));
  const out = await h.poll(reddit);
  assert.ok(within(out.nextRunAt, 29 * 60_000, 32 * 60_000), 'Retry-After 30 min is honoured');
  let row = await h.row(reddit);
  assert.equal(row.failureCount, 0);
  assert.match(row.lastError, /Reddit rate limit — all Reddit feeds wait until <t:\d+:t>/);

  stubFetch(() => resp(403, 'blocked'));
  const out403 = await h.poll(reddit);
  row = await h.row(reddit);
  assert.equal(row.failureCount, 1);
  assert.ok(within(out403.nextRunAt, 54 * 60_000, 66 * 60_000), 'a Reddit 403 backs off at least an hour');

  // Budget exhausted: a poll returns 60-120 s out and records nothing.
  await prisma.feed.update({ where: { id: reddit.id }, data: { failureCount: 0, lastError: null, lastCheckedAt: null } });
  S.__resetLocalState();
  const calls = stubFetch(() => resp(200, REDDIT, XML_HEADERS));
  for (let i = 0; i < S.REDDIT_MAX_PER_MIN; i++) await S.fetchFeedSource({ ...reddit, fetchUrl: `https://www.reddit.com/r/b${i}/new/.rss` });
  const used = calls.length;
  const limited = await h.poll(reddit);
  assert.equal(calls.length, used, 'no Reddit request when over budget');
  assert.ok(within(limited.nextRunAt, 59_000, 121_000));
  row = await h.row(reddit);
  assert.equal(row.failureCount, 0);
  assert.equal(row.lastError, null);
  assert.equal(row.lastCheckedAt, null);
  S.__resetLocalState();
});

test('poll: channel problems (missing, no access, no permission, transient) never count as failures', { skip: skipDb }, async () => {
  const h = harness(G_CHAN);
  const feed = await mkFeed(h, G_CHAN, { probe: { ok: true, items: [it('c1')], etag: null, lastModified: null } });
  const calls = stubFetch(() => rssRes([{ id: 'c2' }, { id: 'c1' }]));

  h.channel.denied.add(PermissionFlagsBits.SendMessages);
  let out = await h.poll(feed);
  assert.equal(calls.length, 0, 'nothing is fetched that cannot be posted');
  let row = await h.row(feed);
  assert.equal(row.failureCount, 0);
  assert.match(row.lastError, /Can't post in #news: missing SendMessages/);
  assert.ok(within(out.nextRunAt, 25 * 60_000, 35 * 60_000), 'one backoff step');
  h.channel.denied.delete(PermissionFlagsBits.SendMessages);

  h.channel.denied.add(PermissionFlagsBits.ViewChannel);
  assert.equal(await h.poll(feed), undefined);
  row = await h.row(feed);
  assert.equal(row.enabled, false);
  assert.match(row.disabledReason, /lost access to #news \(View Channel\)/);
  h.channel.denied.delete(PermissionFlagsBits.ViewChannel);

  // A channel missing from the cache is confirmed with a REST fetch before anything is disabled (rule 20).
  const feed2 = await mkFeed(h, G_CHAN, { source: 'https://feed.test/second' });
  h.guild.channels.cache.delete(h.channel.id);
  let fetched = 0;
  h.guild.channels.fetch = async () => { fetched++; throw new Error('gateway hiccup'); };
  out = await h.poll(feed2);
  assert.equal(fetched, 1);
  assert.ok(within(out.nextRunAt, 4 * 60_000, 6 * 60_000));
  assert.equal((await h.row(feed2)).enabled, true, 'a transient error disables nothing');

  h.guild.channels.fetch = async () => { throw discordErr(10003, 'Unknown Channel'); };
  assert.equal(await h.poll(feed2), undefined);
  row = await h.row(feed2);
  assert.equal(row.enabled, false);
  assert.match(row.disabledReason, /channel was deleted or I lost access/);

  const feed3 = await mkFeed(h, G_CHAN, { source: 'https://feed.test/third' });
  h.guild.channels.fetch = async () => ({ ...h.channel, type: 2 }); // became a voice channel
  assert.equal(await h.poll(feed3), undefined);
  assert.match((await h.row(feed3)).disabledReason, /can no longer receive posts/);
});

test('poll: module off, paused, removed and malformed jobs do no work', { skip: skipDb }, async () => {
  const h = harness(G_MODOFF);
  const feed = await mkFeed(h, G_MODOFF, { probe: { ok: true, items: [it('m1')], etag: null, lastModified: null } });
  const calls = stubFetch(() => rssRes([{ id: 'm2' }, { id: 'm1' }]));
  await updateSettings(G_MODOFF, { feedsEnabled: false });
  let out = await h.poll(feed);
  assert.equal(calls.length, 0, 'no fetch while the module is off');
  assert.ok(within(out.nextRunAt, 29 * 60_000, 31 * 60_000));
  await updateSettings(G_MODOFF, { feedsEnabled: true });

  await prisma.feed.update({ where: { id: feed.id }, data: { enabled: false } });
  assert.equal(await h.poll(feed), undefined);
  assert.equal(await h.poll({ id: 'does-not-exist' }), undefined);
  assert.equal(await S.pollFeedJob({ guildId: G_MODOFF, payload: {} }, h.ctx), undefined);
  assert.equal(await S.pollFeedJob({ guildId: G_SPARE, payload: { feedId: feed.id } }, h.ctx), undefined, "another guild's job never reaches the row");
  assert.equal(calls.length, 0);
});

test('poll: throws only when the initial read fails (the scheduler then retries)', { skip: skipDb }, async () => {
  const h = harness(G_MISC);
  const real = prisma.feed.findFirst;
  prisma.feed.findFirst = async () => { throw new Error('db down'); };
  try {
    await assert.rejects(S.pollFeedJob({ guildId: G_MISC, payload: { feedId: 'x' } }, h.ctx), /db down/);
  } finally {
    prisma.feed.findFirst = real;
  }
  // Any later failure is contained: an unexpected exception becomes a backed-off reschedule.
  const feed = await mkFeed(h, G_MISC, { probe: { ok: true, items: [it('z')], etag: null, lastModified: null } });
  S.__setFetchImpl({ safeFetch: async () => ({ ok: true, status: 200, headers: {}, get buffer() { throw new Error('exploded'); } }) });
  const out = await h.poll(feed);
  assert.ok(out.nextRunAt instanceof Date);
});

test('poll: display names refresh and Bluesky reposts follow includeReposts', { skip: skipDb }, async () => {
  const h = harness(G_MISC);
  const yt = await mkFeed(h, G_MISC, {
    type: 'YOUTUBE', source: `UC${'c'.repeat(22)}`, displayName: `UC${'c'.repeat(22)}`, fetchUrl: 'https://y.test/f',
  });
  stubFetch(() => resp(200, YT, XML_HEADERS));
  await h.poll(yt);
  assert.equal((await h.row(yt)).displayName, 'Chan');
  assert.equal(h.channel.sent.length, 0);

  const bsky = await mkFeed(h, G_MISC, {
    type: 'BLUESKY', source: 'did:plc:abc', displayName: '@old.bsky.social', fetchUrl: 'https://b.test/f',
    probe: { ok: true, items: [], etag: null, lastModified: null },
  });
  const day = new Date().toISOString();
  const fresh = JSON.parse(JSON.stringify(BSKY));
  fresh.feed[0].post.indexedAt = day;
  fresh.feed[1].reason.indexedAt = day;
  stubFetch(() => resp(200, fresh, {}));
  await h.poll(bsky);
  let row = await h.row(bsky);
  assert.equal(row.displayName, '@alice.bsky.social', 'the handle is refreshed from the posts');
  assert.equal(h.channel.sent.length, 1, 'only the original post; the repost is recorded but not posted');
  assert.ok(row.seenIds.some((id) => id.startsWith('repost:')));

  const withReposts = await mkFeed(h, G_MISC, {
    type: 'BLUESKY', source: 'did:plc:abc2', displayName: '@alice.bsky.social', fetchUrl: 'https://b.test/f2', includeReposts: true,
    probe: { ok: true, items: [], etag: null, lastModified: null },
  });
  const before = h.channel.sent.length;
  await h.poll(withReposts);
  assert.equal(h.channel.sent.length, before + 2);
  assert.ok(h.channel.sent.at(-1).content.includes('**Alice** (@alice.bsky.social)'));
});

test('channelDelete cleanup disables and keeps the rows, with one notice in the mod-log channel', { skip: skipDb }, async () => {
  const h = harness(G_DEL);
  const mod = fakeChannel(h.guild, { id: '900000000000000002', name: 'mod-log' });
  const gone = '900000000000000003';
  const a = await mkFeed(h, G_DEL, { channelId: gone, source: 'https://feed.test/d1' });
  const b = await mkFeed(h, G_DEL, { channelId: gone, source: 'https://feed.test/d2' });
  const keep = await mkFeed(h, G_DEL, { source: 'https://feed.test/d3' });
  for (const f of [a, b, keep]) await S.scheduleFeedJob(f, { runAt: new Date(Date.now() + 60_000) });
  await updateSettings(G_DEL, { modLogChannelId: mod.id });

  const settings = await prisma.guildSettings.findUnique({ where: { guildId: G_DEL } });
  const ctx = { guild: h.guild, guildId: G_DEL, channel: { id: gone }, getSettings: async () => settings };
  assert.equal(await S.disableFeedsForChannel(ctx), 2);
  assert.equal((await h.row(a)).enabled, false);
  assert.equal((await h.row(a)).disabledReason, 'Its channel was deleted');
  assert.equal((await h.row(keep)).enabled, true);
  assert.equal(await getJob(S.feedJobKey(a.id)), null);
  assert.ok(await getJob(S.feedJobKey(keep.id)), "other channels' jobs are untouched");
  assert.equal(mod.sent.length, 1);
  assert.match(mod.sent[0].embeds[0].data.title, /Feeds Disabled/);
  assert.match(mod.sent[0].embeds[0].data.description, /edit channel:/);
  assert.equal(await S.disableFeedsForChannel(ctx), 0, 'idempotent');
  assert.equal(mod.sent.length, 1);
});

test('guildReady repairs: missing, FAILED and DONE jobs are replaced; healthy and disabled ones are left alone', { skip: skipDb }, async () => {
  const h = harness(G_JOBS);
  const mk = (n) => mkFeed(h, G_JOBS, { source: `https://feed.test/j${n}` });
  const healthy = await mk(1);
  const missing = await mk(2);
  const failed = await mk(3);
  const paused = await mk(4);
  await prisma.feed.update({ where: { id: paused.id }, data: { enabled: false } });
  const runAt = new Date(Date.now() + 5 * 3_600_000);
  await S.scheduleFeedJob(healthy, { runAt });
  await S.scheduleFeedJob(failed, { runAt });
  await prisma.scheduledJob.update({ where: { dedupeKey: S.feedJobKey(failed.id) }, data: { status: 'FAILED' } });

  await S.ensureGuildFeedJobs(h.guild);
  const jh = await getJob(S.feedJobKey(healthy.id));
  assert.equal(jh.runAt.getTime(), runAt.getTime(), 'a healthy job is not touched');
  const jm = await getJob(S.feedJobKey(missing.id));
  assert.equal(jm.status, 'PENDING');
  assert.ok(jm.runAt.getTime() >= Date.now() + 29_000);
  assert.equal(jm.maxAttempts, 10);
  assert.deepEqual(jm.payload, { feedId: missing.id });
  const jf = await getJob(S.feedJobKey(failed.id));
  assert.equal(jf.status, 'PENDING', 'a FAILED job is replaced');
  assert.equal(await getJob(S.feedJobKey(paused.id)), null);

  await S.scheduleFeedJob(missing, { runAt: new Date(Date.now() + 60_000) });
  assert.equal(await prisma.scheduledJob.count({ where: { dedupeKey: S.feedJobKey(missing.id) } }), 1, 'dedupeKey keeps one row');
  await cancelJobs({ guildId: G_JOBS });
});

// ── /feed with fake interactions ──────────────────────────────────────────────

function fakeInteraction({ guild, sub, group = null, opts = {}, can = true, member = { id: 'u1' } }) {
  const log = { replies: [], edits: [], deferred: null, respond: null };
  const i = {
    guildId: guild.id, guild, user: { id: 'u1' }, member, createdTimestamp: Date.now(), deferred: false, replied: false,
    memberPermissions: { has: () => can },
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      getString: (n) => opts[n] ?? null,
      getChannel: (n) => opts[n] ?? null,
      getRole: (n) => opts[n] ?? null,
      getBoolean: (n) => opts[n] ?? null,
      getFocused: () => opts.focused ?? '',
    },
    deferReply: async (o) => { i.deferred = true; log.deferred = o ?? {}; },
    reply: async (p) => { i.replied = true; log.replies.push(p); return {}; },
    editReply: async (p) => { log.edits.push(p); return {}; },
    respond: async (c) => { log.respond = c; },
  };
  return { i, log };
}
const lastEmbed = (log) => (log.edits.at(-1) ?? log.replies.at(-1)).embeds[0].data;

test('/feed add rss: validation, probe, seeding, job, duplicates', { skip: skipDb }, async () => {
  const h = harness(G_CMD);
  h.guild.roles.cache.set('555', { id: '555', mentionable: true });
  h.guild.roles.cache.set('556', { id: '556', mentionable: false });
  h.guild.roles.cache.set(G_CMD, { id: G_CMD, mentionable: false }); // @everyone
  const run = async (sub, opts, group = 'add', extra = {}) => {
    const { i, log } = fakeInteraction({ guild: h.guild, sub, group, opts, ...extra });
    await cmd.execute(i);
    return log;
  };
  stubFetch(() => rssRes([{ id: 'a1' }, { id: 'a2' }]));

  let log = await run('rss', { url: 'ftp://x', channel: h.channel });
  assert.match(lastEmbed(log).title, /Invalid URL/);
  assert.equal(log.replies[0].ephemeral, true);
  log = await run('rss', { url: 'https://feed.test/f.xml', channel: h.channel, ping_role: { id: G_CMD } });
  assert.match(lastEmbed(log).title, /Invalid Role/);
  // The member lacks Mention Everyone in the channel and the role is not mentionable: refused (rule 23).
  const noMention = { ...h.channel, permissionsFor: (who) => ({ has: (bit) => (who?.id === 'bot' || bit !== PermissionFlagsBits.MentionEveryone) }) };
  h.guild.channels.cache.set(h.channel.id, noMention);
  log = await run('rss', { url: 'https://feed.test/f.xml', channel: noMention, ping_role: { id: '556', mentionable: false } });
  assert.match(lastEmbed(log).title, /Role Not Pingable/);
  assert.match(lastEmbed(log).description, /<@&556> is not mentionable and you lack \*\*Mention @everyone/);
  h.guild.channels.cache.set(h.channel.id, h.channel);
  log = await run('rss', { url: 'https://feed.test/f.xml', channel: h.channel, message: 'x'.repeat(5) });
  assert.equal(log.replies.length + log.edits.length, 1);
  assert.equal(await prisma.feed.count({ where: { guildId: G_CMD } }), 1, 'only the valid add created a row');
  await prisma.feed.deleteMany({ where: { guildId: G_CMD } });

  h.channel.denied.add(PermissionFlagsBits.EmbedLinks);
  log = await run('rss', { url: 'https://feed.test/f.xml', channel: h.channel });
  assert.match(lastEmbed(log).title, /Missing Permissions/);
  h.channel.denied.delete(PermissionFlagsBits.EmbedLinks);

  log = await run('rss', { url: 'https://feed.test/f.xml', channel: h.channel, ping_role: { id: '555', mentionable: true }, message: 'New: {title} {youtube_channel}' });
  assert.equal(log.deferred && log.deferred.ephemeral, undefined, 'admin results are public');
  const e = lastEmbed(log);
  assert.match(e.title, /Feed Added/);
  assert.match(e.description, /📰 \*\*Example\*\* → <#900000000000000001>/);
  assert.match(e.description, /RSS feed · checked every 15 minutes · <https:\/\/feed\.test\/f\.xml>/);
  assert.match(e.description, /Ping: <@&555>/);
  assert.match(e.description, /✅ 2 existing item\(s\) recorded — only new ones will be posted\./);
  assert.match(e.description, /Variables: `\{title\}`/);
  assert.match(e.description, /don't work for RSS feed feeds/);
  assert.ok(!e.description.includes('module is off'));
  assert.deepEqual(log.edits.at(-1).allowedMentions, { parse: [] });
  const row = await prisma.feed.findFirst({ where: { guildId: G_CMD } });
  assert.equal(row.initialized, true);
  assert.deepEqual([...row.seenIds].sort(), ['a1', 'a2']);
  assert.equal(row.pingRoleId, '555');
  assert.equal(row.createdBy, 'u1');
  const job = await getJob(S.feedJobKey(row.id));
  assert.equal(job.status, 'PENDING');
  assert.ok(within(job.runAt, 14 * 60_000, 16 * 60_000), 'a seeded feed first runs after one interval');
  assert.equal(h.channel.sent.length, 0, 'adding a feed posts nothing');

  log = await run('rss', { url: 'https://feed.test/f.xml', channel: h.channel });
  assert.match(lastEmbed(log).title, /Already Following/);

  // a YouTube 404 is accepted: unseeded, 2 minute first check, visible error
  stubFetch(() => resp(404));
  log = await run('youtube', { youtube_channel: `UC${'d'.repeat(22)}`, channel: h.channel });
  assert.match(lastEmbed(log).title, /Feed Added/);
  assert.match(lastEmbed(log).description, /couldn't fetch it yet: HTTP 404 — YouTube/);
  assert.match(lastEmbed(log).description, /unreliable since late 2025/);
  assert.match(lastEmbed(log).description, /no \{link\}|Variables/);
  const yt = await prisma.feed.findFirst({ where: { guildId: G_CMD, type: 'YOUTUBE' } });
  assert.equal(yt.initialized, false);
  assert.match(yt.lastError, /HTTP 404/);
  assert.ok(within((await getJob(S.feedJobKey(yt.id))).runAt, 100_000, 140_000));

  log = await run('youtube', { youtube_channel: '@mkbhd', channel: h.channel });
  assert.match(lastEmbed(log).title, /Channel ID Needed/);
  log = await run('reddit', { subreddit: 'u/spez', channel: h.channel });
  assert.match(lastEmbed(log).title, /Invalid Subreddit/);
  log = await run('bluesky', { handle: 'not a handle!', channel: h.channel });
  assert.match(lastEmbed(log).title, /Invalid Handle/);
  log = await run('podcast', { podcast: 'a', channel: h.channel });
  assert.match(lastEmbed(log).title, /Invalid Search/);
  stubFetch(() => resp(200, { results: [] }));
  log = await run('podcast', { podcast: 'zzzz', channel: h.channel });
  assert.match(lastEmbed(log).title, /Podcast Not Found/);

  // module off: the note is appended to success replies
  await updateSettings(G_CMD, { feedsEnabled: false });
  const { i: li, log: llog } = fakeInteraction({ guild: h.guild, sub: 'list' });
  await cmd.execute(li);
  assert.match(llog.edits.at(-1).embeds[0].data.description, /module is off.*\/module enable feeds/);
  await updateSettings(G_CMD, { feedsEnabled: true });
});

test('/feed list, test, edit, disable, enable, remove, autocomplete', { skip: skipDb }, async () => {
  const guildId = G_MISC;
  const h = harness(guildId);
  h.guild.roles.cache.set('777', { id: '777', mentionable: true });
  const other = fakeChannel(h.guild, { id: '900000000000000009', name: 'other' });
  await prisma.feed.deleteMany({ where: { guildId } });
  const run = async (sub, opts, extra = {}) => {
    const { i, log } = fakeInteraction({ guild: h.guild, sub, opts, ...extra });
    await cmd.execute(i);
    return { log, i };
  };
  let { log } = await run('list', {});
  assert.match(lastEmbed(log).description, /No feeds yet/);

  const feed = await mkFeed(h, guildId, { displayName: 'My Blog', probe: { ok: true, items: [it('l1')], etag: null, lastModified: null } });
  await S.scheduleFeedJob(feed, { runAt: new Date(Date.now() + 600_000) });
  await prisma.feed.update({ where: { id: feed.id }, data: { pingRoleId: '777', lastPostedAt: new Date() } });
  const bad = await mkFeed(h, guildId, { type: 'YOUTUBE', source: 'UCzzz', displayName: 'Broken', fetchUrl: 'https://y.test' });
  await prisma.feed.update({ where: { id: bad.id }, data: { failureCount: 3, lastError: 'HTTP 404 — YouTube' } });
  ({ log } = await run('list', {}));
  const d = lastEmbed(log);
  assert.equal(d.title, '📰 Social Feeds');
  assert.match(d.description, /`1\.` 📰 \*\*My Blog\*\* → <#900000000000000001> · <@&777>\n↳ ✅ OK · next check <t:\d+:R> · last post <t:\d+:R>/);
  assert.match(d.description, /`2\.` 📺 \*\*Broken\*\*.*\n↳ ⚠️ Failing 3× — HTTP 404 — YouTube/);
  assert.equal(d.footer.text, `2/${LIMITS.FEEDS} feeds`);

  // autocomplete
  const ac = fakeInteraction({ guild: h.guild, sub: 'remove', opts: { focused: 'blog' } });
  await cmd.autocomplete(ac.i);
  assert.deepEqual(ac.log.respond, [{ name: '📰 My Blog → #news', value: feed.id }]);
  const acDenied = fakeInteraction({ guild: h.guild, sub: 'remove', can: false });
  await cmd.autocomplete(acDenied.i);
  assert.deepEqual(acDenied.log.respond, []);

  // test: ephemeral, throttled, never writes the row
  const before = await prisma.feed.findUnique({ where: { id: feed.id } });
  stubFetch(() => rssRes([{ id: 'l2', title: 'Newest' }, { id: 'l1' }]));
  let r = await run('test', { feed: feed.id });
  assert.equal(r.log.deferred.ephemeral, true);
  assert.equal(r.log.edits.at(-1).embeds[0].data.title, 'Newest');
  assert.match(r.log.edits.at(-1).content, /^<@&777> /);
  assert.deepEqual(r.log.edits.at(-1).allowedMentions, { parse: [] });
  assert.match(r.log.edits.at(-1).embeds[1].data.description, /Fetched 2 item\(s\) in \d+ ms/);
  const after = await prisma.feed.findUnique({ where: { id: feed.id } });
  assert.equal(after.updatedAt.getTime(), before.updatedAt.getTime(), 'test changes nothing');
  r = await run('test', { feed: feed.id });
  assert.match(lastEmbed(r.log).title, /Slow Down/);
  stubFetch(() => resp(500));
  r = await run('test', { feed: bad.id });
  assert.match(lastEmbed(r.log).title, /Fetch Failed/);
  assert.match(lastEmbed(r.log).description, /Nothing was changed/);
  r = await run('test', { feed: 'missing' });
  assert.match(lastEmbed(r.log).title, /Feed Not Found/);

  // edit
  r = await run('edit', { feed: feed.id });
  assert.match(lastEmbed(r.log).title, /Nothing to Change/);
  r = await run('edit', { feed: feed.id, ping_role: { id: '777', mentionable: true }, clear_ping_role: true });
  assert.match(lastEmbed(r.log).title, /Conflicting Options/);
  r = await run('edit', { feed: feed.id, message: 'x', reset_message: true });
  assert.match(lastEmbed(r.log).title, /Conflicting Options/);
  r = await run('edit', { feed: feed.id, include_reposts: true });
  assert.match(lastEmbed(r.log).title, /Not a Bluesky Feed/);
  r = await run('edit', { feed: feed.id, message: '   ' });
  assert.match(lastEmbed(r.log).title, /Invalid Message/);
  r = await run('edit', { feed: feed.id, channel: other, message: 'Hi {title}\\n{link}', clear_ping_role: true });
  assert.match(lastEmbed(r.log).title, /Feed Updated/);
  assert.match(lastEmbed(r.log).description, /→ <#900000000000000009>/);
  assert.match(lastEmbed(r.log).description, /Ping: none/);
  let row = await prisma.feed.findUnique({ where: { id: feed.id } });
  assert.equal(row.channelId, other.id);
  assert.equal(row.pingRoleId, null);
  assert.equal(row.messageTemplate, 'Hi {title}\n{link}');
  r = await run('edit', { feed: feed.id, reset_message: true });
  row = await prisma.feed.findUnique({ where: { id: feed.id } });
  assert.equal(row.messageTemplate, null);
  await mkFeed(h, guildId, { channelId: h.channel.id, source: row.source, displayName: 'twin' }); // same source in the old channel
  r = await run('edit', { feed: feed.id, channel: h.channel });
  assert.match(lastEmbed(r.log).title, /Already Following/);
  await prisma.feed.deleteMany({ where: { guildId, displayName: 'twin' } });

  // disable / enable / remove
  r = await run('disable', { feed: feed.id });
  assert.match(lastEmbed(r.log).title, /Feed Paused/);
  row = await prisma.feed.findUnique({ where: { id: feed.id } });
  assert.equal(row.enabled, false);
  assert.equal(row.disabledReason, 'Paused by an admin');
  assert.equal(await getJob(S.feedJobKey(feed.id)), null);
  r = await run('disable', { feed: feed.id });
  assert.match(lastEmbed(r.log).title, /Already Paused/);
  assert.equal(r.log.replies.length, 1);

  other.denied.add(PermissionFlagsBits.SendMessages);
  r = await run('enable', { feed: feed.id });
  assert.match(lastEmbed(r.log).title, /Channel Problem/);
  other.denied.delete(PermissionFlagsBits.SendMessages);
  r = await run('enable', { feed: feed.id });
  assert.match(lastEmbed(r.log).title, /Feed Enabled/);
  assert.match(lastEmbed(r.log).description, /Items published while it was off are skipped/);
  row = await prisma.feed.findUnique({ where: { id: feed.id } });
  assert.deepEqual([row.enabled, row.initialized, row.failureCount, row.lastError, row.disabledReason], [true, false, 0, null, null]);
  assert.ok(within((await getJob(S.feedJobKey(feed.id))).runAt, 5_000, 15_000));

  // enabling an already-enabled feed only resets failures and repairs the job
  await prisma.feed.update({ where: { id: bad.id }, data: { initialized: true } });
  r = await run('enable', { feed: bad.id });
  row = await prisma.feed.findUnique({ where: { id: bad.id } });
  assert.deepEqual([row.failureCount, row.lastError, row.initialized], [0, null, true]);
  assert.doesNotMatch(lastEmbed(r.log).description, /skipped/);

  r = await run('remove', { feed: 'my blog' });
  assert.match(lastEmbed(r.log).title, /Feed Removed/);
  assert.equal(await prisma.feed.findUnique({ where: { id: feed.id } }), null);
  assert.equal(await getJob(S.feedJobKey(feed.id)), null);
  await cancelJobs({ guildId });
});

test('/feed: errors are reported, not thrown; unexpected ones are logged generically', { skip: skipDb }, async () => {
  const h = harness(G_SPARE);
  const { i, log } = fakeInteraction({ guild: h.guild, sub: 'rss', group: 'add', opts: { url: 'https://feed.test/x', channel: h.channel } });
  stubFetch(() => rssRes([{ id: 'e1' }]));
  const real = prisma.feed.findFirst;
  prisma.feed.findFirst = async () => { throw new Error('connection lost'); };
  try {
    await cmd.execute(i);
  } finally {
    prisma.feed.findFirst = real;
  }
  assert.match(log.edits.at(-1).embeds[0].data.title, /Something went wrong/);
  assert.match(log.edits.at(-1).embeds[0].data.description, /Please try again/);
  assert.ok(!JSON.stringify(log.edits.at(-1)).includes('connection lost'), 'internals are not shown');
});

// ── Hardening extras ──────────────────────────────────────────────────────────

test('third-party text cannot form masked links, headings or mentions in a post', () => {
  const feed = { type: 'RSS', displayName: '[Evil](https://evil.test)', messageTemplate: '{title}\n{author}\n{feed}', pingRoleId: null };
  const p = S.buildPostPayload(feed, it('i1', null, { title: '[Click here](https://evil.test) @everyone <@&42>', author: '# Big' }), { guild, channel });
  assert.ok(p.content.startsWith('\\[Click here](https://evil.test) @everyone <@&42>'), p.content);
  assert.ok(p.content.includes('\n\\# Big\n'), 'a line-leading # is escaped');
  assert.ok(p.content.includes('\\[Evil]'), 'the feed name too');
  assert.deepEqual(p.allowedMentions, { parse: [], roles: [] }, 'a role mention inside feed text never pings');
});

test('a Reddit 429 block stops fetches (polls, adds and /feed test) without a request', async () => {
  const { cache } = await import('../src/services/redis.js');
  const realGet = cache.get;
  const calls = stubFetch(() => resp(200, REDDIT, XML_HEADERS));
  const until = new Date(Date.now() + 20 * 60_000).toISOString();
  cache.get = async (key) => (key === S.REDDIT_BLOCK_KEY ? { until } : null);
  try {
    S.__resetLocalState();
    const reddit = feedRow({ type: 'REDDIT', fetchUrl: 'https://www.reddit.com/r/aww/new/.rss' });
    const r = await S.fetchFeedSource(reddit, { useCache: false });
    assert.equal(r.ok, false);
    assert.equal(r.budget, true);
    const added = await S.resolveFeedSource('REDDIT', { name: 'aww' });
    assert.equal(added.ok, true);
    assert.equal(added.probe.ok, false);
    assert.equal(calls.length, 0);
    // other sources are untouched by the block
    assert.equal((await S.fetchFeedSource(feedRow())).ok, true);
    assert.equal(calls.length, 1);
  } finally {
    cache.get = realGet;
  }
});

test('shared cache hit: no request, no validators written (fake Redis)', async () => {
  const { cache } = await import('../src/services/redis.js');
  const store = new Map();
  const real = { get: cache.get, set: cache.set };
  cache.get = async (key) => (store.has(key) ? JSON.parse(store.get(key)) : null);
  cache.set = async (key, value) => { store.set(key, JSON.stringify(value)); return true; };
  try {
    const calls = stubFetch(() => resp(200, RSS, XML_HEADERS));
    const first = await S.fetchFeedSource(feedRow());
    assert.equal(first.fromCache, false);
    assert.equal(calls.length, 1);
    const key = S.fetchCacheKey('https://feed.test/f.xml');
    const cached = JSON.parse(store.get(key));
    assert.equal(cached.v, 1);
    assert.equal(typeof cached.items[0].publishedAt, 'string', 'dates are ISO strings in the cache');
    assert.ok(!('buffer' in cached) && !JSON.stringify(cached).includes('<rss'), 'the raw body is never cached');
    const second = await S.fetchFeedSource(feedRow({ id: 'f2' }));
    assert.equal(second.fromCache, true);
    assert.equal(calls.length, 1, 'a second guild reuses the fetch');
    assert.ok(second.items[0].publishedAt instanceof Date);
    assert.equal(second.etag, null);
    // errors and 304s are never cached
    store.clear();
    stubFetch(() => resp(500));
    await S.fetchFeedSource(feedRow());
    stubFetch(() => resp(304));
    await S.fetchFeedSource(feedRow());
    assert.equal(store.size, 0);
    // a Reddit fetch claims one of REDDIT_MAX_PER_MIN 60 s slot keys through Redis when it is up
    const slots = new Set();
    const claims = [];
    const realNx = cache.setNX;
    cache.setNX = async (key, value, ttl) => {
      if (!key.startsWith('feed:rl:reddit:')) return realNx.call(cache, key, value, ttl);
      claims.push([key, ttl]);
      if (slots.has(key)) return false;
      slots.add(key);
      return true;
    };
    try {
      store.clear();
      stubFetch(() => resp(200, REDDIT, XML_HEADERS));
      const reddit = feedRow({ type: 'REDDIT', fetchUrl: 'https://www.reddit.com/r/aww/new/.rss' });
      assert.equal((await S.fetchFeedSource(reddit)).ok, true);
      assert.deepEqual(claims, [['feed:rl:reddit:0', 60]]);
      assert.equal((await S.fetchFeedSource(reddit)).fromCache, true);
      assert.equal(claims.length, 1, 'a cache hit costs no budget');
      // fill every slot: the next network fetch is refused, and the refusal claims no slot
      for (let i = 0; i < S.REDDIT_MAX_PER_MIN; i++) slots.add(`feed:rl:reddit:${i}`);
      store.clear();
      const over = await S.fetchFeedSource(reddit);
      assert.equal(over.budget, true);
      assert.equal(slots.size, S.REDDIT_MAX_PER_MIN, 'a refused attempt claims no slot');
      // a slot expiring frees exactly one fetch
      slots.delete('feed:rl:reddit:3');
      store.clear();
      assert.equal((await S.fetchFeedSource(reddit)).ok, true);
    } finally {
      cache.setNX = realNx;
    }
  } finally {
    Object.assign(cache, real);
  }
});

test('poll: the claim itself moves updatedAt, so a duplicate run of the same job posts nothing', { skip: skipDb }, async () => {
  const h = harness(G_RACE);
  const feed = await mkFeed(h, G_RACE, { source: 'https://feed.test/claim', probe: { ok: true, items: [it('k1')], etag: null, lastModified: null } });
  const snapshot = await h.row(feed);
  const sentBefore = h.channel.sent.length;
  stubFetch(() => rssRes([{ id: 'k2' }, { id: 'k1' }]));
  const real = prisma.feed.findFirst;
  prisma.feed.findFirst = async () => snapshot; // both runs read the row as it was before either claimed
  let first;
  let second;
  try {
    first = await h.poll(feed);
    second = await h.poll(feed);
  } finally {
    prisma.feed.findFirst = real;
  }
  assert.equal(h.channel.sent.length, sentBefore + 1, 'exactly one post');
  assert.ok(within(first.nextRunAt, 13 * 60_000, 17 * 60_000));
  assert.ok(within(second.nextRunAt, 50_000, 70_000), 'the second claim lost and retries in a minute');
  const row = await h.row(feed);
  assert.ok(row.updatedAt.getTime() > snapshot.updatedAt.getTime(), 'the claim bumped updatedAt');
  assert.ok(row.seenIds.includes('k2'));
});
