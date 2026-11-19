// Writes rust/fixtures/golden/feat-feeds/fuzz.json: the user-input parsers, the item URL normaliser (feedParsers'
// private httpOrNull, copied here verbatim), stripHtml and oneLine on generated inputs, as the JS answers them.
// Deterministic (fixed seed), so the file only changes when the JS does.
import { writeFileSync, mkdirSync } from 'node:fs';
import {
  normalizeFeedUrl, parseYouTubeChannelInput, parseSubredditInput, parseBlueskyActorInput, stripHtml, oneLine, itemKey,
} from '../../../src/services/feedParsers.js';

const URL_MAX = 2048;
function httpOrNull(s) {
  if (typeof s !== 'string') return null;
  const v = s.trim();
  if (!/^https?:\/\//i.test(v) || v.length > URL_MAX) return null;
  try {
    const href = new URL(v).href;
    return href.length <= URL_MAX ? href : null;
  } catch {
    return null;
  }
}

let seed = 0x2545f491;
const rnd = (n) => { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; return ((seed >>> 0) % n); };
const pick = (a) => a[rnd(a.length)];

const schemes = ['http://', 'https://', 'HTTPS://', 'feed://', 'feed:', 'ftp://', '', 'javascript:', 'http:/', 'https:\\', ' http://', '<https://'];
const userinfo = ['', '', '', 'user@', 'u:p@', ':@', '@'];
const hosts = ['example.com', 'EXAMPLE.com', 'www.youtube.com', 'youtube.com', 'm.youtube.com', 'youtu.be', 'notyoutube.com', 'www.reddit.com',
  'old.reddit.com', 'reddit.com.evil.test', 'bsky.app', 'xn--bcher-kva.example', 'bücher.example', '127.0.0.1', '[::1]', '0x7f.1', '例え.テスト',
  'example.com.', 'a..b', 'exa mple.com', 'ex%41mple.com', 'example.com:8080', 'example.com:443', 'example.com:80', 'example.com:', 'foo_bar.test',
  'ÉXAMPLE.com', 'example.com\t', '1.2.3', '256.1.1.1', '0300.0250.0.1'];
const paths = ['', '/', '/feed', '/feed.xml', '/channel/UCabcdefghijklmnopqrstuv', '/channel/UCabcdefghijklmnopqrstu', '/@handle', '/c/name', '/user/name',
  '/r/aww', '/r/AskReddit/', '/r/a', '/u/someone', '/user/x/r/y', '/R/Pics/comments/1', '/a b', '/%7euser', '/../x', '/./y', '/ü', '/path?x=1',
  '/watch?channel_id=UCabcdefghijklmnopqrstuv', '/\back', '/a/%2e%2e/b', '/emoji/😀', '/\u0000x', '/x\ny'];
const queries = ['', '', '?a=b', '?channel_id=UCabcdefghijklmnopqrstuv', '?q=a b', '?x=%zz', '?é=1', '?#'];
const frags = ['', '', '#top', '#a b', '#'];
const bare = ['r/aww', '/r/pics', 'aww', 'a', 'AskReddit/', 'u/me', '/user/me', 'r/AskReddit/new', '@alice', 'alice', 'Alice.BSKY.social',
  'did:plc:abc123', 'did:web:example.com', 'did:key:x', 'https://bsky.app/profile/alice.bsky.social/post/1', 'bsky.app/profile/@bob?x',
  'UCabcdefghijklmnopqrstuv', '<UCabcdefghijklmnopqrstuv>', '  <r/aww>  ', 'x'.repeat(30), 'a-b.c', '-a.b', 'a.b-', '1.com', 'ab.c1',
  'xn--x.y', '', '   ', '<>', 'feed:', 'reddit.com', 'youtube.com', 'youtube.com/@x'];

const inputs = new Set(bare);
for (let i = 0; i < 700; i++) inputs.add(`${pick(schemes)}${pick(userinfo)}${pick(hosts)}${pick(paths)}${pick(queries)}${pick(frags)}`);
const list = [...inputs];

const htmlBits = ['<p>', '</p>', '<br>', '<br/>', '<script>x</script>', '<style>a{}</style>', '<SCRIPT >y', '&amp;', '&lt;', '&#39;', '&#x1F600;',
  '&nbsp;', '&bogus;', '&#0;', '&#xD800;', 'text', ' ', '\n', '\t', '<a href="x">link</a>', '<!-- c -->', '<!--', '<', '>', 'é', '😀', ' ',
  '<img src=x>', '<div\n>', '</div>', '&amp;lt;', '&quot;', '<li>', '<h1>H</h1>', '\r\n', '<p/>', '&#128512;', '&#1114112;'];
const html = [];
for (let i = 0; i < 400; i++) {
  let s = '';
  const n = 1 + rnd(8);
  for (let j = 0; j < n; j++) s += pick(htmlBits);
  html.push(s);
}
const ids = ['a', 'x'.repeat(150), 'x'.repeat(151), 'é'.repeat(150), '😀'.repeat(75), '😀'.repeat(76)];

const out = {
  url: list.map((s) => [s, httpOrNull(s)]),
  normalizeFeedUrl: list.map((s) => [s, normalizeFeedUrl(s)]),
  youtube: list.map((s) => [s, parseYouTubeChannelInput(s)]),
  subreddit: list.map((s) => [s, parseSubredditInput(s)]),
  bluesky: list.map((s) => [s, parseBlueskyActorInput(s)]),
  stripHtml: html.map((s) => [s, stripHtml(s)]),
  oneLine: html.map((s) => [s, oneLine(s)]),
  itemKey: ids.map((s) => [s, itemKey(s)]),
};
mkdirSync('rust/fixtures/golden/feat-feeds', { recursive: true });
writeFileSync('rust/fixtures/golden/feat-feeds/fuzz.json', `${JSON.stringify(out, null, 1)}\n`);
console.log(`wrote ${list.length} url inputs, ${html.length} html inputs`);
