// Security regressions for the feeds module (audit 2026-10-28): the XML parser must not rebuild an ancestor
// path per node (jPath) or accept pathological depth, and a third party's feed title must never forge a
// log line. Offline: parser fixtures, the fetch seam and a captured console; no timing thresholds.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { XMLParser } from 'fast-xml-parser';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through).
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL ??= 'postgresql://test:test@127.0.0.1:1/test';

const P = await import('../src/services/feedParsers.js');
const S = await import('../src/services/feedService.js');
const { default: logger } = await import('../src/utils/logger.js');
const { default: redis } = await import('../src/services/redis.js');

// Silence the Redis reconnect noise (logger prints through console); captureConsole() swaps in a collector.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

after(() => {
  S.__setFetchImpl(null);
  redis.disconnect();
  Object.assign(console, quiet);
});

/** Runs fn with console.<method> collecting its first argument; returns the lines it printed. */
function captureConsole(method, fn) {
  const saved = console[method];
  const lines = [];
  console[method] = (line) => { lines.push(String(line)); };
  try { fn(); } finally { console[method] = saved; }
  return lines;
}

const RSS = `<?xml version="1.0"?>
<rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:atom="http://www.w3.org/2005/Atom"><channel>
<title>Example &amp; Co</title><link>https://example.com/</link><atom:link href="https://example.com/feed.xml" rel="self"/>
<item><title><![CDATA[Hello <b>World</b>]]></title><link>https://example.com/2</link><guid isPermaLink="false">post-2</guid>
<pubDate>Fri, 09 Oct 2026 10:00:00 GMT</pubDate><dc:creator>Ann</dc:creator><description>&lt;p&gt;Second&lt;/p&gt;</description></item>
<item><title>First</title><link>https://example.com/1</link><pubDate>Thu, 08 Oct 2026 10:00:00 GMT</pubDate></item>
</channel></rss>`;
const ATOM = `<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns:media="http://search.yahoo.com/mrss/" xmlns="http://www.w3.org/2005/Atom">
<title>Chan</title><entry><id>yt:video:abc123</id><yt:videoId>abc123</yt:videoId><title>Vid</title>
<link rel="alternate" href="https://www.youtube.com/watch?v=abc123"/><author><name>Chan</name></author>
<published>2026-10-09T12:00:00+00:00</published><media:group><media:thumbnail url="https://i.ytimg.com/vi/abc123/hqdefault.jpg" width="480" height="360"/></media:group></entry></feed>`;
const RDF = `<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns="http://purl.org/rss/1.0/">
<channel rdf:about="https://r.example/"><title>RDF Feed</title><link>https://r.example/</link></channel>
<item rdf:about="https://r.example/a"><title>A</title><link>https://r.example/a</link></item></rdf:RDF>`;

/** An RSS document with `depth` nested elements (named `name`) between <channel> and a leaf. */
const nested = (depth, name = 'x') =>
  `<rss><channel><title>Deep</title>${`<${name}>`.repeat(depth)}<a>1</a>${`</${name}>`.repeat(depth)}</channel></rss>`;

const resp = (status, body = '', headers = {}, url = 'https://feed.test/f.xml') => {
  const buffer = Buffer.from(body);
  return { ok: status >= 200 && status < 300, status, statusText: '', headers, url, buffer, text: () => buffer.toString('utf8') };
};
const serve = (body) => S.__setFetchImpl({ safeFetch: async () => resp(200, body, { 'content-type': 'application/rss+xml' }) });

// ── Parser options (feedparsers.xml-parser-jpath-cpu-amplification) ────────────

test('XML_PARSER_OPTIONS: jPath is off and nesting is capped well below the library default', () => {
  assert.equal(P.XML_PARSER_OPTIONS.jPath, false);
  assert.equal(typeof P.XML_PARSER_OPTIONS.maxNestedTags, 'number');
  assert.ok(P.XML_PARSER_OPTIONS.maxNestedTags >= 10 && P.XML_PARSER_OPTIONS.maxNestedTags <= 50, String(P.XML_PARSER_OPTIONS.maxNestedTags));
  assert.ok(Object.isFrozen(P.XML_PARSER_OPTIONS));
});

test('jPath:false parses normal RSS, Atom and RDF exactly as jPath:true does', () => {
  const legacy = new XMLParser({ ...P.XML_PARSER_OPTIONS, jPath: true, maxNestedTags: 100 });
  const current = new XMLParser(P.XML_PARSER_OPTIONS);
  for (const [name, xml] of [['RSS', RSS], ['Atom', ATOM], ['RDF', RDF]]) {
    assert.deepEqual(current.parse(xml), legacy.parse(xml), name);
  }
  const f = P.parseXmlFeed(RSS);
  assert.equal(f.kind, 'rss');
  assert.equal(f.title, 'Example & Co');
  assert.deepEqual(f.items.map((i) => i.title), ['Hello World', 'First']);
  assert.equal(P.parseXmlFeed(ATOM).kind, 'atom');
  assert.equal(P.parseXmlFeed(RDF).items.length, 1);
});

test('a document nested deeper than maxNestedTags is rejected as INVALID_XML; ordinary depth still parses', () => {
  const rejected = (xml) => assert.throws(() => P.parseXmlFeed(xml), (e) => e instanceof P.FeedParseError && e.code === 'INVALID_XML');
  rejected(nested(P.XML_PARSER_OPTIONS.maxNestedTags + 20));
  rejected(nested(98, 'y'.repeat(2000))); // the audited shape: ~98 long-named ancestors
  assert.equal(P.parseXmlFeed(nested(20)).title, 'Deep');
});

test('too-deep XML is a handled failure through fetchAndParse, the poll/test fetch and /feed add', async () => {
  serve(nested(P.XML_PARSER_OPTIONS.maxNestedTags + 20));

  const fetched = await S.fetchAndParse('https://feed.test/f.xml', 'RSS');
  assert.equal(fetched.ok, false);
  assert.equal(fetched.error.code, 'INVALID_XML');
  assert.match(S.describeFetchError('RSS', fetched.error), /could not be parsed/i);

  // feed.poll and /feed test fetch through fetchFeedSource: an error result is what bumps the failure count.
  const polled = await S.fetchFeedSource({ type: 'RSS', fetchUrl: 'https://feed.test/f.xml', initialized: false }, { useCache: false });
  assert.equal(polled.ok, false);
  assert.equal(polled.error.code, 'INVALID_XML');

  // /feed add (and the dashboard) go through resolveFeedSource: a polite refusal, never a throw.
  const added = await S.resolveFeedSource('RSS', { url: 'https://feed.test/f.xml' });
  assert.equal(added.ok, false);
  assert.equal(added.title, 'Not a Feed');
  assert.match(added.message, /couldn't read that as an RSS or Atom feed/i);

  serve(RSS);
  assert.equal((await S.fetchAndParse('https://feed.test/f.xml', 'RSS')).ok, true);
});

// ── Single-line names and log records (feedService/disableFeed/displayName-control-chars-to-logger) ──

test('oneLine collapses newlines, other controls and whitespace runs', () => {
  assert.equal(P.oneLine('Legit feed\n[2026-10-28] [ERROR] forged'), 'Legit feed [2026-10-28] [ERROR] forged');
  assert.equal(P.oneLine('a\r\nb c\x1b[31md\x7fe\tf  g'), 'a b c [31md e f g');
  assert.equal(P.oneLine('\n\x00\n'), '');
  assert.equal(P.oneLine(null), '');
});

test('a feed title with a newline is stored as one line (resolveFeedSource, displayNameUpdate)', async () => {
  serve('<rss><channel><title><![CDATA[Legit feed\n[2026-10-28 21:18:47] [ERROR] forged]]></title><item><title>x</title></item></channel></rss>');
  const r = await S.resolveFeedSource('RSS', { url: 'https://feed.test/f.xml' });
  assert.equal(r.ok, true);
  assert.equal(r.displayName, 'Legit feed [2026-10-28 21:18:47] [ERROR] forged');
  assert.ok(!/[\r\n]/.test(r.displayName));

  assert.equal(S.displayNameUpdate({ type: 'YOUTUBE', source: 'UC1', displayName: 'UC1' }, { title: 'A\nB\r\nC' }), 'A B C');
});

test('logger escapes control characters so a title with a newline stays one log line', () => {
  const title = 'Legit feed\n[2026-10-28 21:18:47] [ERROR] forged operator-visible line';
  const [warned] = captureConsole('warn', () => logger.warn(`Feed "${title}" (feed-1) in guild 1 was disabled: 20 failed checks`));
  assert.ok(!/[\r\n]/.test(warned), JSON.stringify(warned));
  assert.ok(warned.includes('Legit feed\\n[2026-10-28 21:18:47] [ERROR] forged operator-visible line'), warned);

  const mixed = 'a\rb\x1b[31mc\x7fd\x00e\tf';
  const [info] = captureConsole('log', () => logger.info(mixed));
  assert.ok(info.includes('a\\rb\\x1b[31mc\\x7fd\\x00e\\x09f'), info);
  assert.ok(!/[\x00-\x08\x0a-\x0d\x7f]/.test(info.replace(/\x1b\[[0-9;]*m/g, '')), JSON.stringify(info));
});

test('logger escapes Error messages and leaves ordinary text and objects alone', () => {
  const lines = captureConsole('error', () => logger.error('failed:', new Error('bad\r\nthing [ERROR] forged')));
  assert.ok(!/[\r\n]/.test(lines[0]), JSON.stringify(lines[0]));
  assert.ok(lines[0].includes('failed: bad\\r\\nthing [ERROR] forged'), lines[0]);

  const [plain] = captureConsole('log', () => logger.info('Feed "Example & Co" posted 3 items', { id: 'a\nb' }, 42));
  assert.ok(plain.includes('Feed "Example & Co" posted 3 items'), plain);
  assert.ok(plain.includes("{ id: 'a\\nb' } 42"), plain);
  assert.ok(!plain.includes('\n'));
});
