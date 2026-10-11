// ── Feed parsers (pkg-feeds §6) ────────────────────────────────────────────────
// Pure parsing and input normalisation for the social feeds: the XML normaliser (RSS 2.0, Atom,
// RSS 1.0 / RDF), the Bluesky and iTunes JSON normalisers, charset decoding, HTML detection and
// autodiscovery, and the user-input parsers. No database, Redis, Discord or service imports.
//
// Hostile input: feeds are third-party. Every scan below is linear (no regex that can backtrack
// quadratically on `<<<<` or an unclosed `<script`), the DOCTYPE is removed before parsing so an
// entity declaration can never expand, and the parser walks own properties only.
import { createHash } from 'node:crypto';
import { XMLParser } from 'fast-xml-parser';
import { clampText } from '../utils/text.js';

export const ITEM_ID_MAX = 150;
const MAX_ENTRIES = 200;
const URL_MAX = 2048;
const HTML_SCAN_MAX = 200_000;
const STRIP_INPUT_MAX = 200_000;

export class FeedParseError extends Error {
  /** @param {'NOT_A_FEED'|'INVALID_XML'|string} code @param {string} message */
  constructor(code, message) {
    super(message);
    this.name = 'FeedParseError';
    this.code = code;
  }
}

/**
 * fast-xml-parser v5 options. `removeNSPrefix` flattens yt:videoId, media:thumbnail, itunes:image,
 * dc:creator, content:encoded and rdf:about, so every reader below uses the bare local names.
 */
export const XML_PARSER_OPTIONS = Object.freeze({
  ignoreAttributes: false,
  attributeNamePrefix: '@_',
  removeNSPrefix: true,
  textNodeName: '#text',
  parseTagValue: false, // keep "2024" a string
  parseAttributeValue: false,
  trimValues: true,
  processEntities: true,
  htmlEntities: true,
  ignoreDeclaration: true,
  ignorePiTags: true,
  allowBooleanAttributes: true,
});
const parser = new XMLParser(XML_PARSER_OPTIONS); // allocation only; no I/O

// ── Small helpers ──────────────────────────────────────────────────────────────

const arr = (x) => (x == null ? [] : Array.isArray(x) ? x : [x]);
const isObj = (x) => x !== null && typeof x === 'object';
/** Own-property read: never walks the prototype chain, tolerates non-objects. */
const own = (o, k) => (isObj(o) && Object.hasOwn(o, k) ? o[k] : undefined);
const attr = (o, name) => {
  const v = own(o, `@_${name}`);
  return typeof v === 'string' ? v.trim() : '';
};

/** Text of a parsed node: a string or number, or the `#text` of an object. */
function text(x) {
  if (typeof x === 'string') return x.trim();
  if (typeof x === 'number') return String(x).trim();
  const t = own(x, '#text');
  return typeof t === 'string' || typeof t === 'number' ? String(t).trim() : '';
}

/** http(s) only, normalised through URL (spaces become %20), at most 2048 characters. */
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

function dateOrNull(s) {
  if (typeof s !== 'string' && typeof s !== 'number') return null;
  const n = Date.parse(String(s));
  return Number.isFinite(n) ? new Date(n) : null;
}

/** The item link: RSS `<link>text</link>` first, then an Atom alternate link, then any http link. */
function pickLink(linkNode) {
  const nodes = arr(linkNode);
  for (const n of nodes) {
    if (typeof n === 'string' || typeof n === 'number' || (isObj(n) && own(n, '#text') !== undefined)) {
      const u = httpOrNull(text(n));
      if (u) return u;
    }
  }
  for (const n of nodes) {
    if (!isObj(n)) continue;
    const rel = attr(n, 'rel').toLowerCase();
    if (rel === '' || rel === 'alternate') {
      const u = httpOrNull(attr(n, 'href'));
      if (u) return u;
    }
  }
  for (const n of nodes) {
    const u = httpOrNull(attr(n, 'href'));
    if (u) return u;
  }
  return null;
}

/** Stable item key: ids over ITEM_ID_MAX characters are replaced by their SHA-1. */
export function itemKey(id) {
  const s = String(id ?? '');
  return s.length <= ITEM_ID_MAX ? s : `sha1:${createHash('sha1').update(s).digest('hex')}`;
}

// ── HTML to text ───────────────────────────────────────────────────────────────

/**
 * Removes `<tag …>…</tag>` blocks in one linear pass; an unclosed block drops the rest. Case-insensitive
 * regexes scan the original string (lower-casing first can shift indexes for some Unicode characters).
 */
function removeBlocks(s, tag) {
  const openRe = new RegExp(`<${tag}(?=[\\s>/])`, 'gi');
  const closeRe = new RegExp(`</${tag}[^<>]*>`, 'gi');
  let out = '';
  let pos = 0;
  for (;;) {
    openRe.lastIndex = pos;
    const o = openRe.exec(s);
    if (!o) { out += s.slice(pos); break; }
    out += s.slice(pos, o.index);
    closeRe.lastIndex = o.index + o[0].length;
    const c = closeRe.exec(s);
    if (!c) break;
    pos = c.index + c[0].length;
  }
  return out;
}

const NAMED_ENTITIES = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ' };

/** Decodes the common named and numeric entities in ONE pass, so `&amp;#65;` stays `&#65;`. */
function decodeEntities(s) {
  return s.replace(/&(?:#(\d{1,7})|#[xX]([0-9a-fA-F]{1,6})|(amp|lt|gt|quot|apos|nbsp));/g, (m, dec, hex, name) => {
    if (name) return NAMED_ENTITIES[name];
    const cp = dec !== undefined ? Number(dec) : parseInt(hex, 16);
    if (!Number.isInteger(cp) || cp <= 0 || cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) return '';
    try { return String.fromCodePoint(cp); } catch { return ''; }
  });
}

/**
 * HTML to plain text: scripts and styles go, block ends become newlines, tags go, entities are
 * decoded once, whitespace is collapsed. Never throws; input is cut to 200 000 characters first.
 * @param {unknown} input
 * @returns {string}
 */
export function stripHtml(input) {
  if (input == null) return '';
  let s = String(input);
  if (s.length > STRIP_INPUT_MAX) s = s.slice(0, STRIP_INPUT_MAX);
  s = removeBlocks(removeBlocks(s, 'script'), 'style');
  s = s.replace(/<br\s*\/?>/gi, '\n').replace(/<\/(?:p|div|li)>/gi, '\n');
  s = s.replace(/<[^<>]+>/g, ''); // `[^<>]` keeps the scan linear on `<<<<`
  s = decodeEntities(s);
  return s.replace(/[ \t]+/g, ' ').replace(/[ \t]*\n[ \t]*/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
}

// ── Decoding, HTML detection and autodiscovery ─────────────────────────────────

/**
 * Decodes a response body: the charset of the Content-Type header, else the XML declaration, else
 * UTF-8. An unknown label falls back to UTF-8; a leading BOM is removed.
 * @param {Buffer} buffer
 * @param {string} [contentType]
 * @returns {string}
 */
export function decodeXmlBuffer(buffer, contentType = '') {
  let label = /charset=["']?([\w.:-]+)/i.exec(String(contentType ?? ''))?.[1];
  if (!label) label = /^<\?xml[^>]*encoding=["']([\w.:-]+)["']/i.exec(buffer.subarray(0, 200).toString('latin1'))?.[1];
  if (!label) label = 'utf-8';
  let out;
  try {
    out = new TextDecoder(label, { fatal: false }).decode(buffer);
  } catch {
    out = new TextDecoder('utf-8', { fatal: false }).decode(buffer);
  }
  return out.charCodeAt(0) === 0xfeff ? out.slice(1) : out;
}

/** True for an HTML page (content type, or a leading doctype / html tag). */
export function looksLikeHtml(textBody, contentType = '') {
  const head = String(textBody ?? '').slice(0, 1000).trim();
  if (/text\/html|application\/xhtml/i.test(String(contentType ?? ''))) {
    // Some servers label a real feed text/html: trust the body when it starts like one.
    return !/^(<\?xml[^>]*\?>\s*)?(<!--[\s\S]*?-->\s*)*<(rss|feed|rdf:RDF)[\s>]/i.test(head);
  }
  return /^(<!--[\s\S]*?-->\s*)*<!doctype html|^(<!--[\s\S]*?-->\s*)*<html[\s>]/i.test(head);
}

const ATTR_RE = /([^\s"'<>/=]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'<>]+))/g;

/**
 * RSS and Atom links an HTML page advertises (`<link rel="alternate" type="application/rss+xml">`),
 * at most 5, in document order. JSON Feed is not supported and is ignored.
 * @param {string} html
 * @param {string} baseUrl
 * @returns {string[]}
 */
export function discoverFeedLinks(html, baseUrl) {
  const found = [];
  const head = String(html ?? '').slice(0, HTML_SCAN_MAX);
  for (const tag of head.matchAll(/<link\b[^<>]*>/gi)) {
    const attrs = {};
    for (const m of tag[0].matchAll(ATTR_RE)) attrs[m[1].toLowerCase()] ??= m[2] ?? m[3] ?? m[4] ?? '';
    if (!/(^|\s)alternate(\s|$)/i.test(attrs.rel ?? '')) continue;
    const type = String(attrs.type ?? '').trim().toLowerCase();
    if (type !== 'application/rss+xml' && type !== 'application/atom+xml') continue;
    if (!attrs.href) continue;
    try {
      const u = new URL(decodeEntities(attrs.href.trim()), baseUrl);
      if ((u.protocol === 'http:' || u.protocol === 'https:') && !found.includes(u.href)) found.push(u.href);
    } catch { /* unusable href */ }
    if (found.length >= 5) break;
  }
  return found;
}

/**
 * Removes every DOCTYPE (with its internal subset) in one linear scan, so an entity declaration can
 * never reach the parser. An unterminated DOCTYPE drops the rest of the document.
 * @param {string} xml
 * @returns {string}
 */
export function stripDoctype(xml) {
  let s = String(xml ?? '');
  for (let round = 0; round < 4; round++) {
    const start = s.search(/<!DOCTYPE/i);
    if (start === -1) return s;
    let depth = 0;
    let quote = '';
    let end = -1;
    for (let i = start + 9; i < s.length; i++) {
      const c = s[i];
      if (quote) { if (c === quote) quote = ''; continue; }
      if (c === '"' || c === "'") { if (depth > 0) quote = c; continue; }
      if (c === '[') depth++;
      else if (c === ']') depth = Math.max(0, depth - 1);
      else if (c === '>' && depth === 0) { end = i; break; }
    }
    s = end === -1 ? s.slice(0, start) : s.slice(0, start) + s.slice(end + 1);
  }
  return s;
}

// ── XML normaliser ─────────────────────────────────────────────────────────────

function firstHttp(...candidates) {
  for (const c of candidates) {
    const u = httpOrNull(c);
    if (u) return u;
  }
  return null;
}

function entryAuthor(e) {
  let a = text(arr(own(e, 'creator'))[0]);
  if (!a) {
    for (const node of arr(own(e, 'author'))) {
      a = text(own(node, 'name')) || text(node);
      if (a) break;
    }
  }
  return clampText(stripHtml(a.replace(/^\//, '')), 256);
}

function entryImage(e) {
  const enclosures = arr(own(e, 'enclosure'));
  const group = arr(own(e, 'group'))[0];
  const candidates = [
    attr(arr(own(e, 'thumbnail'))[0], 'url'),
    attr(arr(own(group, 'thumbnail'))[0], 'url'),
  ];
  for (const c of arr(own(e, 'content'))) {
    if (!isObj(c)) continue;
    if (attr(c, 'medium') === 'image' || attr(c, 'type').toLowerCase().startsWith('image/')) { candidates.push(attr(c, 'url')); break; }
  }
  const imgEnc = enclosures.find((en) => attr(en, 'type').toLowerCase().startsWith('image/'));
  if (imgEnc) candidates.push(attr(imgEnc, 'url'));
  candidates.push(attr(arr(own(e, 'image'))[0], 'href'));
  return firstHttp(...candidates);
}

function entrySummary(e) {
  const group = arr(own(e, 'group'))[0];
  const sources = [
    text(own(e, 'encoded')), text(own(e, 'description')), text(own(e, 'summary')),
    text(arr(own(e, 'content'))[0]), text(own(group, 'description')),
  ];
  for (const raw of sources) {
    if (!raw) continue;
    // The footer scan is bounded: a regex starting with `\s*` is quadratic on a long whitespace run.
    const plain = stripHtml(raw).slice(0, 5000).replace(/\s*submitted by\s+\/?u\/\S+[\s\S]*$/i, '').trim();
    if (plain) return clampText(plain, 500);
  }
  return '';
}

function normalizeEntry(e) {
  if (!isObj(e)) return null;
  const title = clampText(stripHtml(text(arr(own(e, 'title'))[0])), 256) || '(untitled)';
  const guid = arr(own(e, 'guid'))[0];
  const enclosure = arr(own(e, 'enclosure'))[0];
  const link = pickLink(own(e, 'link'));
  const published = text(own(e, 'pubDate')) || text(own(e, 'published')) || text(own(e, 'updated'));

  const rawId = text(guid) || text(arr(own(e, 'id'))[0]) || attr(e, 'about') || link
    || attr(enclosure, 'url') || (title !== '(untitled)' || published ? `${title}|${published}` : '');
  if (!rawId) return null;

  const guidUrl = attr(guid, 'isPermaLink').toLowerCase() !== 'false' ? httpOrNull(text(guid)) : null;
  let publishedAt = null;
  for (const c of [own(e, 'pubDate'), own(e, 'date'), own(e, 'published'), own(e, 'updated')]) {
    publishedAt = dateOrNull(text(arr(c)[0]));
    if (publishedAt) break;
  }

  return {
    id: itemKey(rawId),
    title,
    url: link ?? guidUrl ?? httpOrNull(attr(enclosure, 'url')),
    author: entryAuthor(e),
    publishedAt,
    image: entryImage(e),
    summary: entrySummary(e),
    isRepost: false,
    actorName: '',
    actorHandle: '',
  };
}

/**
 * Parses an RSS 2.0, Atom or RSS 1.0 (RDF) document.
 * @param {string} xml
 * @returns {{ kind: 'rss'|'atom'|'rdf', title: string|null, link: string|null, image: string|null, items: object[] }}
 * @throws {FeedParseError} INVALID_XML or NOT_A_FEED
 */
export function parseXmlFeed(xml) {
  const source = stripDoctype(xml);
  // Defence in depth: an entity declaration outside a DOCTYPE is never legitimate.
  if (/<!ENTITY/i.test(source)) throw new FeedParseError('INVALID_XML', 'Feed XML could not be parsed');
  let doc;
  try {
    doc = parser.parse(source);
  } catch {
    throw new FeedParseError('INVALID_XML', 'Feed XML could not be parsed');
  }
  let kind;
  let channel;
  let rawItems;
  if (isObj(doc) && Object.hasOwn(doc, 'rss')) {
    kind = 'rss';
    channel = arr(own(doc.rss, 'channel'))[0];
    rawItems = arr(own(channel, 'item'));
  } else if (isObj(doc) && Object.hasOwn(doc, 'feed')) {
    kind = 'atom';
    channel = doc.feed;
    rawItems = arr(own(doc.feed, 'entry'));
  } else if (isObj(doc) && Object.hasOwn(doc, 'RDF')) {
    kind = 'rdf';
    channel = arr(own(doc.RDF, 'channel'))[0];
    rawItems = arr(own(doc.RDF, 'item'));
  }
  if (!kind || !isObj(channel)) throw new FeedParseError('NOT_A_FEED', 'Not an RSS or Atom feed');

  const imageCandidates = [];
  for (const im of arr(own(channel, 'image'))) imageCandidates.push(attr(im, 'href'), text(own(im, 'url')));
  imageCandidates.push(text(own(channel, 'logo')), text(own(channel, 'icon')));

  return {
    kind,
    title: clampText(stripHtml(text(arr(own(channel, 'title'))[0])), 256) || null,
    link: pickLink(own(channel, 'link')),
    image: firstHttp(...imageCandidates),
    items: rawItems.slice(0, MAX_ENTRIES).map(normalizeEntry).filter(Boolean),
  };
}

// ── Bluesky and iTunes normalisers ─────────────────────────────────────────────

const goodHandle = (h) => (typeof h === 'string' && h && h !== 'handle.invalid' ? h : null);

function bskyImage(embed) {
  if (!isObj(embed)) return null;
  switch (embed.$type) {
    case 'app.bsky.embed.images#view': return firstHttp(embed.images?.[0]?.fullsize, embed.images?.[0]?.thumb);
    case 'app.bsky.embed.external#view': return firstHttp(embed.external?.thumb);
    case 'app.bsky.embed.recordWithMedia#view': return firstHttp(embed.media?.images?.[0]?.fullsize, embed.media?.images?.[0]?.thumb);
    case 'app.bsky.embed.video#view': return firstHttp(embed.thumbnail);
    default: return null;
  }
}

/**
 * Normalises an `app.bsky.feed.getAuthorFeed` response (pins are dropped, reposts are flagged).
 * @param {any} json
 * @returns {{ kind: 'bluesky', title: null, link: null, image: null, items: object[] }}
 * @throws {FeedParseError} NOT_A_FEED
 */
export function parseBlueskyAuthorFeed(json) {
  if (!json || !Array.isArray(json.feed)) throw new FeedParseError('NOT_A_FEED', 'Unexpected Bluesky response');
  const items = [];
  for (const entry of json.feed.slice(0, 100)) {
    const post = entry?.post;
    if (typeof post?.uri !== 'string') continue;
    const reasonType = typeof entry.reason?.$type === 'string' ? entry.reason.$type : '';
    if (reasonType === 'app.bsky.feed.defs#reasonPin') continue;
    const isRepost = reasonType === 'app.bsky.feed.defs#reasonRepost';
    const author = isObj(post.author) ? post.author : {};
    const actor = isRepost && isObj(entry.reason.by) ? entry.reason.by : isRepost ? {} : author;
    const rkey = post.uri.split('/').pop();
    const body = typeof post.record?.text === 'string' ? post.record.text : '';
    const image = bskyImage(post.embed);
    const profile = goodHandle(author.handle) ?? (typeof author.did === 'string' ? author.did : null);
    // `indexedAt` is set by the server; `record.createdAt` is client-controlled and only a fallback.
    const when = (isRepost ? dateOrNull(entry.reason?.indexedAt) : dateOrNull(post.indexedAt)) ?? dateOrNull(post.record?.createdAt);
    items.push({
      id: itemKey(isRepost ? `repost:${post.uri}:${entry.reason?.indexedAt ?? ''}` : post.uri),
      title: clampText(body.split('\n').find((l) => l.trim())?.trim() || (image ? '(image)' : '(post)'), 256),
      url: profile ? httpOrNull(`https://bsky.app/profile/${profile}/post/${rkey}`) : null,
      author: clampText(author.displayName || (goodHandle(author.handle) ? `@${author.handle}` : ''), 256),
      publishedAt: when,
      image,
      summary: clampText(body, 500),
      isRepost,
      actorName: clampText(actor.displayName || goodHandle(actor.handle) || '', 256),
      actorHandle: goodHandle(actor.handle) ?? (typeof actor.did === 'string' ? actor.did : ''),
    });
  }
  return { kind: 'bluesky', title: null, link: null, image: null, items };
}

/**
 * Normalises an iTunes Search response to the podcasts that expose an http(s) feed URL.
 * @param {any} json
 * @returns {Array<{ name: string, artist: string, feedUrl: string, artwork: string|null, url: string|null }>}
 */
export function parseItunesSearch(json) {
  const out = [];
  for (const r of arr(json?.results)) {
    if (typeof r?.feedUrl !== 'string' || !/^https?:\/\//i.test(r.feedUrl)) continue;
    out.push({
      name: String(r.collectionName ?? r.trackName ?? ''),
      artist: String(r.artistName ?? ''),
      feedUrl: r.feedUrl,
      artwork: firstHttp(r.artworkUrl600, r.artworkUrl100),
      url: firstHttp(r.collectionViewUrl),
    });
  }
  return out;
}

// ── User-input parsers ─────────────────────────────────────────────────────────

const cleanInput = (raw) => String(raw ?? '').trim().replace(/^<(.*)>$/, '$1').trim();

/** @param {unknown} raw @returns {{ ok: true, url: string } | { ok: false, reason: string }} */
export function normalizeFeedUrl(raw) {
  let s = cleanInput(raw);
  if (!s) return { ok: false, reason: 'Give a feed URL.' };
  s = s.replace(/^feed:(\/\/)?/i, '');
  if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(s)) s = `https://${s}`;
  let u;
  try {
    u = new URL(s);
  } catch {
    return { ok: false, reason: "That doesn't look like a URL." };
  }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') return { ok: false, reason: 'Only http and https feed URLs work.' };
  if (u.username || u.password) return { ok: false, reason: 'URLs with a username or password are not allowed.' };
  u.hash = '';
  const href = u.href;
  if (href.length > 500) return { ok: false, reason: 'That URL is too long (max 500 characters).' };
  return { ok: true, url: href };
}

const YT_ID = /^UC[A-Za-z0-9_-]{22}$/;
const YT_HANDLE_REASON = "YouTube @handles and /c/ or /user/ links can't be looked up without a YouTube API key, which this bot doesn't use. "
  + 'Use the channel ID instead: it starts with **UC** and is 24 characters long (on YouTube: the channel → About → Share → **Copy channel ID**).';
const YT_INVALID_REASON = "That isn't a YouTube channel ID. Use the 24-character ID starting with **UC**, or a youtube.com/channel/UC… link.";

/** @param {unknown} raw @returns {{ ok: true, channelId: string } | { ok: false, handle: boolean, reason: string }} */
export function parseYouTubeChannelInput(raw) {
  const s = cleanInput(raw);
  const invalid = { ok: false, handle: false, reason: YT_INVALID_REASON };
  const handle = { ok: false, handle: true, reason: YT_HANDLE_REASON };
  if (YT_ID.test(s)) return { ok: true, channelId: s };
  if (s.startsWith('@')) return handle;
  if (!/youtube\.com|youtu\.be/i.test(s)) return invalid;
  let u;
  try {
    u = new URL(/^[a-z][a-z0-9+.-]*:\/\//i.test(s) ? s : `https://${s}`);
  } catch {
    return invalid;
  }
  const host = u.hostname.toLowerCase();
  if (host !== 'youtube.com' && !host.endsWith('.youtube.com')) return invalid;
  const parts = u.pathname.split('/');
  if (parts[1] === 'channel' && YT_ID.test(parts[2] ?? '')) return { ok: true, channelId: parts[2] };
  const q = u.searchParams.get('channel_id');
  if (q && YT_ID.test(q)) return { ok: true, channelId: q };
  if (u.pathname.startsWith('/@') || u.pathname.startsWith('/c/') || u.pathname.startsWith('/user/')) return handle;
  return invalid;
}

const SUB_INVALID = "That isn't a valid subreddit name (2–21 letters, digits or underscores).";
const SUB_PROFILE = 'Only subreddits are supported, not user profiles.';

/** @param {unknown} raw @returns {{ ok: true, name: string } | { ok: false, reason: string }} */
export function parseSubredditInput(raw) {
  const original = cleanInput(raw);
  let s = original;
  if (/reddit\.com/i.test(s)) {
    let u;
    try {
      u = new URL(/^[a-z][a-z0-9+.-]*:\/\//i.test(s) ? s : `https://${s}`);
    } catch {
      return { ok: false, reason: SUB_INVALID };
    }
    const host = u.hostname.toLowerCase();
    if (host !== 'reddit.com' && !host.endsWith('.reddit.com')) return { ok: false, reason: SUB_INVALID };
    const segs = u.pathname.split('/').filter(Boolean);
    if (/^(u|user)$/i.test(segs[0] ?? '')) return { ok: false, reason: SUB_PROFILE };
    const at = segs.findIndex((x) => x.toLowerCase() === 'r');
    if (at === -1 || !segs[at + 1]) return { ok: false, reason: SUB_INVALID };
    s = segs[at + 1];
  } else {
    if (/^\/?(u|user)\//i.test(original)) return { ok: false, reason: SUB_PROFILE };
    s = s.replace(/^\/?r\//i, '').replace(/\/+$/, '').split('/')[0];
  }
  if (!/^[A-Za-z0-9][A-Za-z0-9_]{1,20}$/.test(s)) return { ok: false, reason: SUB_INVALID };
  return { ok: true, name: s };
}

const BSKY_DID = /^did:(plc|web):[A-Za-z0-9._:%-]{1,200}$/;
const BSKY_HANDLE = /^(?=.{3,253}$)([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

/** @param {unknown} raw @returns {{ ok: true, actor: string } | { ok: false, reason: string }} */
export function parseBlueskyActorInput(raw) {
  let s = cleanInput(raw);
  const at = s.indexOf('bsky.app/profile/');
  if (at !== -1) s = s.slice(at + 'bsky.app/profile/'.length).split(/[/?#]/)[0];
  s = s.replace(/^@/, '');
  if (BSKY_DID.test(s)) return { ok: true, actor: s };
  s = s.toLowerCase();
  if (!s.includes('.')) s += '.bsky.social';
  if (!BSKY_HANDLE.test(s)) {
    return { ok: false, reason: "That isn't a Bluesky handle. Use something like **alice.bsky.social**, a bsky.app profile link, or a did:plc: ID." };
  }
  return { ok: true, actor: s };
}
