// Writes rust/fixtures/golden/feat-feeds/xml-fuzz.json: fast-xml-parser's tree (with the bot's options) and
// parseXmlFeed's output for generated feed documents and random mutations of the parsers.json documents (cut, insert,
// duplicate, swap). Fixed seed. Run from the repository root.
process.env.TZ = 'UTC';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { XMLParser } from 'fast-xml-parser';

const P = await import('../../../src/services/feedParsers.js');
const parser = new XMLParser(P.XML_PARSER_OPTIONS);
const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);
const feedOut = (xml) => {
  try {
    const f = P.parseXmlFeed(xml);
    return { ok: true, kind: f.kind, title: f.title, link: f.link, image: f.image, items: f.items.map((i) => ({ ...i, publishedAt: iso(i.publishedAt) })) };
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

let seed = 0x9e3779b9;
const rnd = (n) => { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; return (seed >>> 0) % n; };
const pick = (a) => a[rnd(a.length)];

const tokens = ['<', '>', '/>', '</', '<item>', '</item>', '<entry>', '</entry>', '<title>', '</title>', '<![CDATA[', ']]>', '&amp;', '&lt;',
  '&#x27;', '&#128512;', '&nbsp;', '&copy;', '&bogus;', '&', '"', "'", '=', ' a="1"', ' b', ' c=d', '<!--', '-->', '<?xml version="1.0"?>',
  '<?pi x?>', '<!DOCTYPE x>', '<x:y>', '</x:y>', '<media:thumbnail url="https://e.test/i.png"/>', 'é', '😀', '\u0000', '\r\n', '\t', ' ',
  '<link href="https://e.test/a"/>', '<link>https://e.test/b</link>', '<guid>g1</guid>', '<id>i1</id>', '<pubDate>Fri, 09 Oct 2026 10:00:00 GMT</pubDate>',
  '<updated>2026-10-09T10:00:00Z</updated>', '<description>&lt;p&gt;Hi&lt;/p&gt;</description>', '<constructor>x</constructor>', '<__proto__>y</__proto__>',
  '<hasOwnProperty>z</hasOwnProperty>', '<#text>q</#text>', '<a#b>', '<1x>', '<rdf:RDF>', '<channel>', '</channel>', '<rss>', '</rss>', '<feed>', '</feed>'];

const wrap = [
  (body) => `<rss version="2.0"><channel><title>T</title><link>https://e.test/</link>${body}</channel></rss>`,
  (body) => `<feed xmlns="http://www.w3.org/2005/Atom"><title type="html">A &amp; B</title>${body}</feed>`,
  (body) => `<rdf:RDF xmlns:rdf="x"><channel><title>R</title></channel>${body}</rdf:RDF>`,
  (body) => body,
];
const itemShapes = [
  () => `<item><title>${pick(['a', '<![CDATA[<b>x</b>]]>', '&amp;amp;', ''])}</title><link>${pick(['https://e.test/1', ' https://e.test/2 ', 'javascript:x', 'https://e.test/a b'])}</link>${pick(['<guid>g</guid>', '<guid isPermaLink="false">p</guid>', ''])}${pick(['<pubDate>Thu, 08 Oct 2026 10:00:00 +0200</pubDate>', '<dc:date>2026-10-08</dc:date>', '<pubDate>garbage</pubDate>', ''])}</item>`,
  () => `<entry><id>${pick(['yt:video:abc', 'tag:x', ''])}</id><title>${pick(['E', '<b>E</b>', ''])}</title><link rel="${pick(['alternate', 'self', 'enclosure', ''])}" href="https://e.test/${rnd(9)}"/>${pick(['<author><name>N</name></author>', '<author>plain</author>', ''])}${pick(['<published>2026-10-09T12:00:00+00:00</published>', '<updated>2026-10-09</updated>', ''])}${pick(['<media:group><media:thumbnail url="https://i.test/t.jpg"/></media:group>', '<content type="html">&lt;img src="https://i.test/c.png"&gt;</content>', ''])}</entry>`,
  () => `<item rdf:about="https://e.test/r${rnd(9)}"><title>R</title><link>https://e.test/r</link></item>`,
  () => `<item><enclosure url="https://p.test/${rnd(9)}.mp3" type="${pick(['audio/mpeg', 'image/png', ''])}"/><itunes:image href="https://p.test/i.jpg"/><title>Ep</title></item>`,
];

const cases = [];
for (let i = 0; i < 300; i++) {
  let body = '';
  for (let j = 0, n = rnd(5); j < n; j++) body += pick(itemShapes)();
  for (let j = 0, n = rnd(3); j < n; j++) {
    const at = rnd(body.length + 1);
    body = body.slice(0, at) + pick(tokens) + body.slice(at);
  }
  cases.push(pick(wrap)(body).toWellFormed());
}
// Nesting near maxNestedTags (40).
for (const depth of [38, 39, 40, 41, 42]) cases.push(`<rss><channel>${'<x>'.repeat(depth)}${'</x>'.repeat(depth)}<item><title>d</title></item></channel></rss>`);

const golden = JSON.parse(readFileSync('rust/fixtures/golden/feat-feeds/parsers.json', 'utf8'));
const docs = Object.values(golden.docs).map((d) => d.xml).filter((x) => x.length < 6000);
for (let i = 0; i < 400; i++) {
  let s = pick(docs);
  const op = rnd(4);
  const a = rnd(s.length + 1);
  const b = Math.min(s.length, a + rnd(40));
  if (op === 0) s = s.slice(0, a) + s.slice(b);
  else if (op === 1) s = s.slice(0, a) + pick(tokens) + s.slice(a);
  else if (op === 2) s = s.slice(0, b) + s.slice(a, b) + s.slice(b);
  else s = s.slice(0, a);
  cases.push(s.toWellFormed());
}

const out = cases.map((xml) => ({ xml, tree: treeOut(xml), feed: feedOut(xml) }));
mkdirSync('rust/fixtures/golden/feat-feeds', { recursive: true });
writeFileSync('rust/fixtures/golden/feat-feeds/xml-fuzz.json', `${JSON.stringify(out)}\n`);
console.log(`wrote ${out.length} documents`);
