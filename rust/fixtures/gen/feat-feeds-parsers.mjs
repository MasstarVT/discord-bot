// Reference data for the feat-feeds parser parity tests: runs the JS feed parsers (and fast-xml-parser itself, with
// the bot's options) over the JS test fixtures plus awkward documents, and writes the inputs with the JS results to
// rust/fixtures/golden/feat-feeds/parsers.json. Run from the repository root: node rust/fixtures/gen/feat-feeds-parsers.mjs
process.env.TZ = 'UTC'; // V8 reads zone-less dates as local time; the Rust port uses TZ (unset = UTC)
import { writeFileSync, mkdirSync } from 'node:fs';
import { XMLParser } from 'fast-xml-parser';

const P = await import('../../../src/services/feedParsers.js');
const parser = new XMLParser(P.XML_PARSER_OPTIONS);

const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);
const items = (f) => f.items.map((i) => ({ ...i, publishedAt: iso(i.publishedAt) }));
const feedOut = (fn) => {
  try {
    const f = fn();
    return { ok: true, kind: f.kind, title: f.title, link: f.link, image: f.image, items: items(f) };
  } catch (err) {
    return { ok: false, code: err.code ?? 'THROW' };
  }
};
const treeOut = (xml) => {
  try {
    return { ok: true, tree: parser.parse(P.stripDoctype(xml)) };
  } catch {
    return { ok: false };
  }
};

// The fixtures of test/feeds.test.js.
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
const NASTY = '<?xml version="1.0"?><!DOCTYPE x [<!ENTITY a "]>"><!ENTITY b "&a;&a;&a;&a;"><!ENTITY c "&b;&b;&b;&b;&b;&b;">]>'
  + '<!DOCTYPE y [<!ENTITY d "&c;&c;&c;">]><rss><channel><title>&c;&d;</title></channel></rss>';
const ODD = `<rss><channel><title>t</title>
    <item></item>
    <item><title>Only title</title></item>
    <item><guid>${'g'.repeat(300)}</guid><title>Long</title><link>not a url</link></item>
    <item><title>Space</title><link>https://x.test/a b</link></item>
  </channel></rss>`;
const nest = (n) => `<rss><channel><title>deep</title>${'<x>'.repeat(n)}${'</x>'.repeat(n)}</channel></rss>`;
const item = (inner) => `<rss><channel><title>T</title><item>${inner}</item></channel></rss>`;

const DATES = [
  'Fri, 09 Oct 2026 10:00:00 GMT', 'Fri, 09 Oct 2026 10:00:00 +0000', 'Fri, 09 Oct 2026 10:00:00 +0200', 'Fri, 9 Oct 2026 10:00:00 -0730',
  'Fri, 09 Oct 2026 10:00:00 EST', 'Fri, 09 Oct 2026 10:00:00 PDT', 'Fri, 09 Oct 2026 10:00:00 UT', 'Fri, 09 Oct 2026 10:00:00 Z',
  'Fri, 09 Oct 2026 10:00 GMT', '09 Oct 26 10:00:00 GMT', '9 Oct 2026', 'Oct 9, 2026', 'October 9, 2026 10:00 PM', 'Friday, October 9, 2026 10:00 pm',
  '2026-10-09', '2026-10-09T12:00:00Z', '2026-10-09T12:00:00+00:00', '2026-10-09T12:00:00.123456Z', '2026-10-09T12:00:00.1Z', '2026-10-09T12:00Z',
  '2026-10-09T24:00:00Z', '2026-10-09T24:00:01Z', '2026-10-09T12:00:00', '2026-10-09 12:00', '2026-10-09 12:00:00 +0100', '2026/10/09 12:00:00',
  '10/09/2026', '10/9', 'Oct 9', '2026-10-09T12:00:00 +0000', '+002026-10-09T00:00:00Z', '-000000-01-01T00:00:00Z', '2026-13-45', '2026-02-30',
  'Fri, 32 Oct 2026', 'Fri Oct 09 2026 10:00:00 GMT+0200 (Central European Summer Time)', 'Thu, 01 Jan 1970 00:00:00 GMT', 'Sat, 31 Dec 1949 00:00:00 GMT',
  '12:00', 'garbage', '', '  ', '1700000000000', '2026', '2026-10', 'Fri, 09 Oct 2026 10:00:00 GMT+2', 'Fri, 09 Oct 2026 10:00:00 GMT-08:00',
  '2026-10-09T12:00:00-0130', 'Fri, 09 Oct 2026 10:00:00.5 GMT', '9 October 2026 10:00:00.250', '2026-10-09T12:00:00+25:00', 'Oct 2026 9',
  'Mon, 1 Jan 2001 25:00:00 GMT', 'Mon, 1 Jan 2001 12:00 AM', 'Mon, 1 Jan 2001 12:00 PM', 'Mon, 1 Jan 2001 13:00 PM', '1 Jan 99', '1 Jan 49',
  'Fri, 09 Oct 2026 10:00:00 (comment) GMT', 'T12', 'Fri,09-Oct-2026 10:00:00 GMT', '2026-10-09T12:00:00.Z', 'May 5', 'Sept 5, 2026',
];

const DOCS = {
  rss: RSS, youtube: YT, reddit: REDDIT, rdf: RDF, podcast: POD, lolz: LOLZ, nasty: NASTY, odd: ODD,
  entityOutsideDoctype: '<rss><channel><title>t</title><!ENTITY z "q"></channel></rss>',
  unterminatedDoctype: '<!DOCTYPE x [<!ENTITY a "b"><rss><channel><title>t</title></channel></rss>',
  html: '<html><body>hi</body></html>', notXml: 'not xml', selfClosingRss: '<rss/>', note: '<note>hi</note>',
  deep40: nest(38), deep41: nest(39), deep45: nest(45),
  cdataMix: item('<title>a <![CDATA[ <b>B</b> ]]> c</title><guid>1</guid><description><![CDATA[]]></description>'),
  cdataLink: item('<title>x</title><link><![CDATA[https://c.test/p?a=1&b=2]]></link>'),
  entities: item('<title>&nbsp;&mdash;&#x1F600;&#0;&#55296;&#1114112;&bogus;&amp;lt; R&D &#65abc; &#x0x41; &#; &#x; &euro;&yen;</title><guid>e</guid>'),
  xml11: `<?xml version="1.1"?>${item('<title>a&#1;b&#2;</title><guid>v</guid>')}`,
  xml10ctrl: item('<title>a&#1;b&#9;c</title><guid>v</guid>'),
  namespaces: `<rss xmlns:content="http://purl.org/rss/1.0/modules/content/" xmlns:media="http://search.yahoo.com/mrss/" xmlns:dc="http://purl.org/dc/elements/1.1/"><channel><title>NS</title><link>https://ns.test/</link>
<image><url>https://ns.test/logo.png</url></image>
<item><title>N1</title><guid isPermaLink="true">https://ns.test/1</guid><content:encoded><![CDATA[<p>Full <i>body</i></p><script>alert(1)</script>]]></content:encoded><media:content url="https://ns.test/i.jpg" medium="image"/><dc:date>2026-10-09T08:00:00Z</dc:date></item>
<item><title>N2</title><enclosure url="https://ns.test/pic.png" type="IMAGE/PNG"/><enclosure url="https://ns.test/a.mp3" type="audio/mpeg"/><updated>Fri, 09 Oct 2026 07:00:00 GMT</updated></item>
<item><enclosure url="https://ns.test/only.mp3"/></item>
<item><guid>https://ns.test/permalink</guid></item>
<item><guid isPermaLink="false">https://ns.test/nope</guid><title>G</title></item>
</channel></rss>`,
  atomLinks: `<feed><title type="html">&lt;b&gt;Atom&lt;/b&gt;</title><link rel="self" href="https://a.test/self"/><link href="https://a.test/"/><logo>https://a.test/logo.png</logo>
<entry><title>E1</title><link rel="enclosure" href="https://a.test/e.mp3"/><link rel="alternate" type="text/html" href="https://a.test/e1"/><id>urn:e1</id><updated>2026-10-09T12:00:00Z</updated><summary>Sum &amp; mary</summary><author><name>Writer</name></author></entry>
<entry><title>E2</title><link rel="related" href="https://a.test/rel"/><id>urn:e2</id><content type="xhtml"><div>XHTML <b>content</b></div></content><author>Bare</author></entry>
<entry><title>E3</title><link>https://a.test/textlink</link><link href="https://a.test/href"/></entry>
</feed>`,
  missing: '<rss><channel><item><title>no channel title</title><guid>m</guid></item></channel></rss>',
  dates: `<rss><channel><title>D</title>${DATES.slice(0, 30).map((d, i) => `<item><guid>d${i}</guid><pubDate>${d}</pubDate></item>`).join('')}</channel></rss>`,
  unclosed: '<rss><channel><title>Open<item><title>i1</title><guid>1</guid>',
  strayClose: '</x><rss><channel></y><title>S</title></channel></rss></z>',
  comments: item('<title>a<!-- c -->b <!-- d --> c</title><guid>c</guid>'),
  piInBody: item('<?pi stuff?><title>p</title><guid>p</guid>'),
  attrs: `<rss><channel><title>A</title><item><guid	isPermaLink	=	'false'	>t1</guid><link href='https://q.test/1' href="https://q.test/2"/><enclosure url = " https://q.test/e.mp3 " type="audio/mpeg" bare/></item></channel></rss>`,
  bangElement: item('<!foo>x</!foo><title>b</title><guid>b</guid>'),
  dangerous: item('<toString>ts</toString><title>d</title><guid>d</guid>'),
  constructorTag: item('<constructor>c</constructor><guid>c</guid>'),
  textTag: item('<#text/><title>t</title><guid>t</guid>'),
  multipleRoots: '<feed><title>one</title></feed><feed><title>two</title></feed>',
  rdfItems: `<rdf:RDF xmlns:rdf="r"><channel><title>R</title></channel><item rdf:about="https://r.test/1"><title>1</title><dc:date>2026-10-01T00:00:00Z</dc:date></item><item rdf:about="https://r.test/2"/></rdf:RDF>`,
  crlf: '<rss>\r\n<channel>\r<title>Line\r\nBreaks</title>\r\n<item><title>a\rb</title><guid>x</guid></item></channel></rss>',
  fiveDoctypes: `${'<!DOCTYPE a>'.repeat(5)}<rss><channel><title>five</title></channel></rss>`,
  lowercaseDoctype: '<!doctype rss><rss><channel><title>lc</title></channel></rss>',
  tagNoClose: '<rss><channel><title>t</title><item',
  closeNoGt: '<rss><channel><title>t</title></channel',
  cdataUnclosed: '<rss><channel><title><![CDATA[x</title></channel></rss>',
  commentUnclosed: '<rss><channel><title>t</title><!-- x </channel></rss>',
  piUnclosed: '<?xml version="1.0"<rss/>',
  emptyItems: '<rss><channel><title>E</title><item/><item>just text</item><item><title></title></item></channel></rss>',
  htmlTitle: item('<title>&lt;script&gt;bad()&lt;/script&gt;Clean &lt;br/&gt;text&lt;/p&gt;more</title><guid>h</guid><author>/u/someone</author>'),
  longSummary: item(`<title>L</title><guid>l</guid><description>${'word '.repeat(300)} submitted by /u/x [link]</description>`),
  imageCandidates: item('<title>I</title><guid>i</guid><media:thumbnail url="ftp://bad"/><media:group><media:thumbnail url="https://img.test/g.jpg"/></media:group>'),
  itunesImage: item('<title>I</title><guid>i2</guid><itunes:image href="https://img.test/it.jpg"/>'),
  nestedTextAndChildren: item('<title>before<b>bold</b>after</title><guid>n</guid>'),
  numericTitle: item('<title>2024</title><guid>2025</guid>'),
  unicode: item('<title>Café ☕ 𝓤𝓷𝓲𝓬𝓸𝓭𝓮</title><guid>u</guid><link>https://ünïcode.test/päth?q=ü</link>'),
};

const out = { docs: {}, dates: {}, misc: {} };
for (const [name, xml] of Object.entries(DOCS)) {
  out.docs[name] = { xml, tree: treeOut(xml), feed: feedOut(() => P.parseXmlFeed(xml)) };
}
for (const d of DATES) {
  const t = Date.parse(d);
  out.dates[d] = Number.isFinite(t) ? t : null;
}

const BSKY = { feed: [
  { post: { uri: 'at://did:plc:abc/app.bsky.feed.post/3k2', author: { did: 'did:plc:abc', handle: 'alice.bsky.social', displayName: 'Alice' },
    record: { text: 'Hello world\nsecond line', createdAt: '2026-10-09T12:00:00Z' }, indexedAt: '2026-10-09T12:00:01Z',
    embed: { $type: 'app.bsky.embed.images#view', images: [{ thumb: 'https://cdn.bsky.app/t.jpg', fullsize: 'https://cdn.bsky.app/f.jpg' }] } } },
  { post: { uri: 'at://did:plc:zzz/app.bsky.feed.post/9x', author: { did: 'did:plc:zzz', handle: 'bob.bsky.social', displayName: 'Bob' },
    record: { text: 'Bob post' }, indexedAt: '2026-10-08T00:00:00Z' },
  reason: { $type: 'app.bsky.feed.defs#reasonRepost', by: { did: 'did:plc:abc', handle: 'alice.bsky.social', displayName: 'Alice' }, indexedAt: '2026-10-09T13:00:00Z' } },
  { post: { uri: 'at://did:plc:q/app.bsky.feed.post/r1', author: { did: 'did:plc:q', handle: 'handle.invalid' }, record: {}, indexedAt: '2026-10-09T00:00:00Z' } },
  { post: { uri: 'at://did:plc:p/app.bsky.feed.post/pin' }, reason: { $type: 'app.bsky.feed.defs#reasonPin' } },
  { post: { uri: 'at://x/y/ext', author: [], record: { text: '\n  \n' }, embed: { $type: 'app.bsky.embed.external#view', external: { thumb: 'https://e.test/t.png' } }, indexedAt: 'bad' } },
  { post: { uri: 'at://x/y/rwm', author: { did: '' , displayName: 7 }, record: { text: 'media', createdAt: 'Fri, 09 Oct 2026 10:00:00 GMT' }, embed: { $type: 'app.bsky.embed.recordWithMedia#view', media: { images: [{ thumb: 'https://m.test/t.jpg' }] } } } },
  { post: { uri: 'at://x/y/vid', author: { handle: 'v.test' }, record: { text: 'video' }, embed: { $type: 'app.bsky.embed.video#view', thumbnail: 'https://v.test/t.jpg' } },
    reason: { $type: 'app.bsky.feed.defs#reasonRepost', by: [], indexedAt: 12345 } },
  { post: { uri: 5 } }, null, 'str',
] };
out.misc.bluesky = feedOut(() => P.parseBlueskyAuthorFeed(BSKY));
out.misc.blueskyInput = BSKY;
out.misc.blueskyBad = [feedOut(() => P.parseBlueskyAuthorFeed({})), feedOut(() => P.parseBlueskyAuthorFeed(null)), feedOut(() => P.parseBlueskyAuthorFeed({ feed: 'x' }))];
const ITUNES = { results: [
  { collectionName: 'A', feedUrl: 'ftp://x' },
  { collectionName: 'B', artistName: 'Z', feedUrl: 'https://b.test/rss', artworkUrl100: 'https://a.test/i.jpg' },
  { trackName: 'T', feedUrl: 'HTTP://t.test/f', artworkUrl600: 'nope', collectionViewUrl: 'https://apple.test/c' },
  { collectionName: null, trackName: 5, artistName: null, feedUrl: 'https://n.test/' }, 'x', null,
] };
out.misc.itunesInput = ITUNES;
out.misc.itunes = P.parseItunesSearch(ITUNES);

const HTML_SAMPLES = [
  '<p>A&amp;B</p><script>x</script>&#233;&#x41;', '&amp;#65; &#0; &#xD800; &#1114112; &nbsp;x', 'a<br/>b<br>c</div>d<style>p{}</style>e',
  '<SCRIPT type="x">bad</SCRIPT >ok<script>never closed', '<scripty>kept</scripty>', ' lots   of\t\tspace \n\n\n\n lines ', '<<<>>> < b >', '&quot;&apos;&lt;&gt;&#x1f600;',
  '<li>one</li><li>two</li>', 'x<br  />y<BR/>z',
];
out.misc.stripHtml = HTML_SAMPLES.map((s) => [s, P.stripHtml(s)]);
out.misc.oneLine = ['a\nb', ' x\t\u0000y\u0085z w ', 'café\r\n bar'].map((s) => [s, P.oneLine(s)]);
const HTML_TYPES = [
  ['<!DOCTYPE html><html>', ''], ['x', 'text/html; charset=utf-8'], [RSS, 'application/rss+xml'], [RSS, 'text/html'],
  ['<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml">', 'text/html'], ['<!-- c --> <!-- d --><html>', ''],
  ['<!-- c --><rss>', 'text/html'], [`${'<!---->'.repeat(50)}x`, 'text/html'], ['<feed xmlns="a">', 'application/xhtml+xml'], ['  <HTML lang="en">', ''],
];
out.misc.looksLikeHtml = HTML_TYPES.map(([b, t]) => [b, t, P.looksLikeHtml(b, t)]);
const DISCOVER = [
  ['<head><link rel="alternate" type="application/rss+xml" href="/feed.xml"><link rel="alternate" type="application/feed+json" href="/f.json"></head>', 'https://site.test/blog/'],
  ['<link href=\'a.xml?x=1&amp;y=2\' type="APPLICATION/ATOM+XML" rel="Alternate nofollow"><link rel="stylesheet" href="s.css"><link rel="alternate" type="application/rss+xml" href="javascript:alert(1)">', 'https://site.test/blog/'],
  [Array.from({ length: 9 }, (_, i) => `<link rel="alternate" type="application/rss+xml" href="/f${i}.xml">`).join(''), 'https://s.test/'],
  ['<link rel=alternate type=application/rss+xml href=//cdn.test/x.rss><link rel="alternate" type="application/rss+xml" href="//cdn.test/x.rss">', 'https://s.test/'],
  ['<link rel="alternate" type="application/rss+xml" href="/x">', 'not a base'],
];
out.misc.discover = DISCOVER.map(([h, b]) => [h, b, P.discoverFeedLinks(h, b)]);
out.misc.stripDoctype = ['<!DOCTYPE a [ <!ENTITY x "]>"> ]><r/>', '<!doctype html><p>', 'a<!DOCTYPE b "x>y">c', '<!DOCTYPE unterminated [ <r/>'].map((s) => [s, P.stripDoctype(s)]);
const B = (s) => Buffer.from(s);
const BUFFERS = [
  [Buffer.concat([B('<?xml version="1.0" encoding="ISO-8859-1"?><rss><channel><title>Caf'), Buffer.from([0xe9]), B('</title></channel></rss>')]), 'application/xml'],
  [Buffer.concat([B('<?xml version="1.0" encoding="ISO-8859-1"?><rss><channel><title>Caf'), Buffer.from([0xe9]), B('</title></channel></rss>')]), 'application/xml; charset=iso-8859-1'],
  [Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), B('<rss/>')]), ''],
  [B('<rss/>'), 'text/xml; charset=nonsense-9'],
  [Buffer.from([0xff, 0xfe, 0x3c, 0x00, 0x61, 0x00]), 'text/xml; charset=utf-16le'],
  [Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf, 0xef, 0xbb, 0xbf]), B('x')]), 'text/xml'],
  [B('caf\xe9'), 'text/xml; charset="windows-1252"'], [B('<?xml encoding=\'shift_jis\'?>'), ''], [B('x'), 'text/xml; charset=replacement'],
];
out.misc.decode = BUFFERS.map(([buf, ct]) => [buf.toString('base64'), ct, P.decodeXmlBuffer(buf, ct)]);
out.misc.itemKey = ['a'.repeat(150), 'a'.repeat(151), 'é'.repeat(151)].map((s) => [s, P.itemKey(s)]);
const URLS = ['example.com/feed', 'feed://example.com/rss', 'feed:https://example.com/rss', '<https://x.test/a#frag>', 'ftp://x', 'https://u:p@x.test/', '   ',
  `https://x.test/${'a'.repeat(600)}`, 'javascript:alert(1)', 'HTTPS://Example.COM:443/A b', 'http://[::1]:8080/x', 'https://ex ample.com', 'mailto:x@y.z', 'https://x.test/?q=1#'];
out.misc.normalizeFeedUrl = URLS.map((u) => [u, P.normalizeFeedUrl(u)]);
const id = (c) => `UC${c.repeat(22)}`;
const YTS = [id('a'), `https://www.youtube.com/channel/${id('b')}/videos`, `https://www.youtube.com/feeds/videos.xml?channel_id=${id('c')}`, '@mkbhd', 'youtube.com/@mkbhd',
  'https://www.youtube.com/c/foo', 'https://youtube.com/user/foo', 'https://www.youtube.com/watch?v=abc', 'UCshort', `https://evil.test/youtube.com/channel/${id('d')}`,
  'https://youtu.be/abc', '', `<${id('e')}>`, `m.youtube.com/channel/${id('f')}`, 'https://notyoutube.com/channel/x'];
out.misc.youtube = YTS.map((s) => [s, P.parseYouTubeChannelInput(s)]);
const SUBS = ['r/aww', '/r/aww/', 'aww', 'https://www.reddit.com/r/aww/new/', 'r/aww/top', 'old.reddit.com/r/aww', 'r/AskReddit', 'u/spez', '/user/spez',
  'https://reddit.com/user/spez', 'a', 'bad-name', 'https://evil.test/r/aww', 'https://www.reddit.com/', '', 'R/Pics', 'https://www.reddit.com/R/foo_bar', 'x'.repeat(22)];
out.misc.subreddit = SUBS.map((s) => [s, P.parseSubredditInput(s)]);
const ACTORS = ['@Alice.bsky.social', 'alice', 'https://bsky.app/profile/bob.example.com/post/1', 'https://bsky.app/profile/did:plc:abc123?x=1', 'did:plc:abc123',
  'not a handle!', '', 'did:web:example.com', 'a.b', '-bad.bsky.social', `${'a'.repeat(64)}.bsky.social`, 'ALICE.BSKY.SOCIAL', 'x.123'];
out.misc.bluesky_actor = ACTORS.map((s) => [s, P.parseBlueskyActorInput(s)]);

mkdirSync('rust/fixtures/golden/feat-feeds', { recursive: true });
writeFileSync('rust/fixtures/golden/feat-feeds/parsers.json', `${JSON.stringify(out, null, 1)}\n`);
console.log(`wrote ${Object.keys(out.docs).length} documents, ${DATES.length} dates`);
