// ── Feed service (pkg-feeds §7) ────────────────────────────────────────────────
// Keyless social feeds: RSS/Atom, YouTube channel Atom, Reddit .rss, the Bluesky public AppView and
// podcasts. Constants, pure helpers, fetching with a shared Redis cache, add-time resolution, the DB
// helpers and the `feed.poll` job handler. No interaction replies live here (the command owns them).
//
// Every outbound request goes through `safeFetch` / `safeFetchJson` (SSRF-safe, 10 s timeout, byte cap).
// State lives in Postgres (Feed + ScheduledJob rows): a restart loses nothing, and the first successful
// fetch of a feed only records the items already there (no backlog). Delivery is at most once: item ids
// are recorded before they are sent.
import { createHash, randomInt } from 'node:crypto';
import { ChannelType, escapeMarkdown } from 'discord.js';
import prisma from '../database/client.js';
import { LIMITS } from '../config/constants.js';
import { cache } from './redis.js';
import { cancelJobs, getJobsByKeys, scheduleJob } from './scheduler.js';
import { ensureGuildSettings, getSettingsSafe, isModuleEnabled } from './settingsService.js';
import { TimeoutError, createKeyedMutex, withTimeout } from '../utils/async.js';
import { neutralEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { checkChannelUsable, pingableRoleIds } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { PLACEHOLDERS, renderPlaceholders } from '../utils/placeholderParser.js';
import { DEFAULT_USER_AGENT, SafeFetchError, safeFetch, safeFetchJson } from '../utils/safeFetch.js';
import { clampText, joinWithLimit } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import {
  FeedParseError,
  decodeXmlBuffer,
  discoverFeedLinks,
  looksLikeHtml,
  parseBlueskyAuthorFeed,
  parseItunesSearch,
  parseXmlFeed,
} from './feedParsers.js';

// ── Constants ──────────────────────────────────────────────────────────────────

export const FEED_TYPES = Object.freeze(['RSS', 'YOUTUBE', 'REDDIT', 'BLUESKY', 'PODCAST']);
/** Per-source polling intervals: Bluesky 5 min, Reddit 10 min, RSS / YouTube 15 min, podcasts 30 min. */
export const BASE_INTERVAL_MS = Object.freeze({ RSS: 15 * 60_000, YOUTUBE: 15 * 60_000, REDDIT: 10 * 60_000, BLUESKY: 5 * 60_000, PODCAST: 30 * 60_000 });
export const MAX_BACKOFF_MS = 12 * 60 * 60_000;
export const REDDIT_BLOCK_MS = 60 * 60_000;
export const MAX_FAILURES = 20;
export const MAX_POSTS_PER_POLL = 5;
export const SEEN_CAP = 100;
export const WINDOW_SIZE = 50;
export const FETCH_TIMEOUT_MS = 10_000;
export const MAX_BYTES = Object.freeze({
  RSS: 2 * 1048576, YOUTUBE: 2 * 1048576, REDDIT: 2 * 1048576, BLUESKY: 2 * 1048576, PODCAST: 10 * 1048576,
});
export const SEND_TIMEOUT_MS = 8_000;
export const SEND_BUDGET_MS = 40_000; // from handler start; keeps the job under JOB_TIMEOUT_MS (60 s)
export const MODULE_OFF_RECHECK_MS = 30 * 60_000;
export const STALE_SLACK_MS = 24 * 60 * 60_000;
export const REDDIT_BLOCK_KEY = 'feed:block:reddit';
export const ITUNES_RL_KEY = 'feed:itunes:rl';
/** Proactive bot-wide Reddit budget (architect amendment D12): at most this many network fetches per minute. */
export const REDDIT_RL_KEY = 'feed:rl:reddit';
export const REDDIT_MAX_PER_MIN = 8;
const ITUNES_MAX_PER_MIN = 20;
export const FEED_CHANNEL_TYPES = Object.freeze([ChannelType.GuildText, ChannelType.GuildAnnouncement]);

export const feedJobKey = (id) => `feed.poll:${id}`;
export const fetchCacheKey = (url) => `feed:fetch:${createHash('sha1').update(url).digest('hex')}`;
export const cacheTtlSeconds = (type) => Math.max(60, Math.floor(BASE_INTERVAL_MS[type] / 1000) - 30);

/** An error whose title and message are meant for the admin (the command shows it and does not log it). */
export class FeedUserError extends Error {
  constructor(title, message) {
    super(message);
    this.name = 'FeedUserError';
    this.title = title;
  }
}

export const CARD_TYPES = new Set(['RSS', 'REDDIT', 'PODCAST']); // we attach our own embed card
export const FEED_VARS = Object.freeze({
  common: ['title', 'url', 'link', 'author', 'feed'],
  YOUTUBE: ['youtube_channel'],
  REDDIT: ['subreddit'],
  BLUESKY: ['bluesky_name', 'bluesky_handle'],
  PODCAST: ['podcast_title', 'episode_title'],
  RSS: [],
});
export const DEFAULT_TEMPLATES = Object.freeze({ // ours, not MEE6 verbatim (§1.2)
  RSS: '📰 New from **{feed}**',
  YOUTUBE: '📺 **{youtube_channel}** just posted a new video!\n{link}',
  REDDIT: '👽 New post in **r/{subreddit}**',
  BLUESKY: '🦋 **{bluesky_name}** (@{bluesky_handle}) posted on Bluesky\n{link}',
  PODCAST: '🎙️ New episode of **{podcast_title}**: {episode_title}',
});
export const TYPE_LABELS = Object.freeze({ RSS: 'RSS feed', YOUTUBE: 'YouTube channel', REDDIT: 'Subreddit', BLUESKY: 'Bluesky account', PODCAST: 'Podcast' });
export const TYPE_EMOJI = Object.freeze({ RSS: '📰', YOUTUBE: '📺', REDDIT: '👽', BLUESKY: '🦋', PODCAST: '🎙️' });
export const TYPE_COLORS = Object.freeze({ RSS: 0xF26522, YOUTUBE: 0xFF0000, REDDIT: 0xFF4500, BLUESKY: 0x1185FE, PODCAST: 0x8940FA });

/** The Bluesky public AppView (research ext §16). */
export const BSKY_APPVIEW = (process.env.BSKY_APPVIEW_URL || 'https://public.api.bsky.app').replace(/\/+$/, '');

const POST_CHANNEL_NEEDS = Object.freeze(['ViewChannel', 'SendMessages', 'EmbedLinks']);
const DID_RE = /^did:(plc|web):[A-Za-z0-9._:%-]{1,200}$/;

// ── Test seam ──────────────────────────────────────────────────────────────────
// ESM exports cannot be patched and safeFetch refuses loopback, so offline tests swap the two fetchers
// here. Production never calls this; the defaults are the SSRF-safe ones.
let net = { safeFetch, safeFetchJson };
/** @param {{ safeFetch?: Function, safeFetchJson?: Function }|null} impl null restores the defaults */
export function __setFetchImpl(impl) {
  net = { safeFetch, safeFetchJson, ...(impl ?? {}) };
}

// In-process Reddit budget, used only while Redis is unavailable (cache.setNX returned null).
const localRedditHits = [];
export function __resetLocalState() {
  localRedditHits.length = 0;
}
function localRedditAllow(now = Date.now()) {
  while (localRedditHits.length && now - localRedditHits[0] >= 60_000) localRedditHits.shift();
  if (localRedditHits.length >= REDDIT_MAX_PER_MIN) return false;
  localRedditHits.push(now);
  return true;
}

// ── Pure helpers ───────────────────────────────────────────────────────────────

/**
 * Retry-After header: digits are seconds, otherwise an HTTP date; anything else is null.
 * @param {unknown} value
 * @param {number} [now]
 * @returns {number|null} milliseconds
 */
export function parseRetryAfterMs(value, now = Date.now()) {
  if (value === undefined || value === null) return null;
  const s = String(value).trim();
  if (/^\d+$/.test(s)) return Number(s) * 1000;
  const at = Date.parse(s);
  return Number.isNaN(at) ? null : Math.max(0, at - now);
}

/**
 * When to poll next. Failures back off ×2 per consecutive failure up to ×32 (capped at 12 h); a Reddit
 * 403/429 waits at least an hour; Retry-After is honoured up to 6 h; ±10 % jitter; never under a minute.
 * @returns {Date}
 */
export function computeNextPollAt({ type, failureCount = 0, status = null, retryAfterMs = null, now = new Date(), rand = Math.random }) {
  const base = BASE_INTERVAL_MS[type] ?? 15 * 60_000;
  let delay = failureCount > 0 ? Math.min(base * 2 ** Math.min(failureCount, 5), MAX_BACKOFF_MS) : base;
  if (type === 'REDDIT' && (status === 403 || status === 429)) delay = Math.max(delay, REDDIT_BLOCK_MS);
  if (Number.isFinite(retryAfterMs) && retryAfterMs > 0) delay = Math.max(delay, Math.min(retryAfterMs, 6 * 60 * 60_000));
  delay = Math.round(delay * (0.9 + 0.2 * rand()));
  return new Date(now.getTime() + Math.max(delay, 60_000));
}

/** The newest WINDOW_SIZE items, de-duplicated by id; sorted by date only when every item has one. */
export function windowItems(items) {
  const seen = new Set();
  const unique = [];
  for (const item of items ?? []) {
    if (seen.has(item.id)) continue;
    seen.add(item.id);
    unique.push(item);
  }
  const allDated = unique.length > 0 && unique.every((i) => i.publishedAt instanceof Date);
  const ordered = allDated ? [...unique].sort((a, b) => b.publishedAt - a.publishedAt) : unique;
  return ordered.slice(0, WINDOW_SIZE);
}

/**
 * Decides what to post. `items` is the window, newest first. Every window id is recorded as seen
 * (reposts included). The first fetch of a feed (`!initialized`) records and posts nothing; an undated
 * feed whose ids all changed is treated as a GUID reset; dated items older than the previous success
 * (minus a day) are archive churn; at most `maxPost` newest items are posted, oldest first.
 */
export function selectNewItems({ items, seenIds = [], initialized, baseline = null, includeReposts = false, maxPost = MAX_POSTS_PER_POLL }) {
  const windowIds = [...new Set(items.map((i) => i.id))];
  const nextSeenIds = [...new Set([...windowIds, ...seenIds])].slice(0, SEEN_CAP);
  if (!initialized) return { toPost: [], nextSeenIds, reseed: true, skippedOld: 0, overflow: 0 };
  const seen = new Set(seenIds);
  const unseen = items.filter((i) => !seen.has(i.id));
  if (!unseen.length) return { toPost: [], nextSeenIds, reseed: false, skippedOld: 0, overflow: 0 };
  // Undated feed whose ids all changed (GUID reset): re-seed instead of reposting the archive.
  const overlap = items.some((i) => seen.has(i.id));
  const dated = items.some((i) => i.publishedAt);
  if (!overlap && !dated && seenIds.length > 0 && unseen.length > maxPost) {
    return { toPost: [], nextSeenIds, reseed: true, skippedOld: unseen.length, overflow: 0 };
  }
  // Dated items older than the previous success (minus a day of slack) are archive churn, not news.
  const cutoff = baseline ? baseline.getTime() - STALE_SLACK_MS : null;
  const fresh = unseen.filter((i) => !(cutoff !== null && i.publishedAt && i.publishedAt.getTime() < cutoff));
  const eligible = fresh.filter((i) => includeReposts || !i.isRepost);
  const toPost = eligible.slice(0, maxPost).reverse(); // newest maxPost, posted oldest-first
  return { toPost, nextSeenIds, reseed: false, skippedOld: unseen.length - fresh.length, overflow: eligible.length - toPost.length };
}

/** `` `{a}` `{b}` `` — the variables a message for this type can use. */
export function varHelp(type) {
  return [...FEED_VARS.common, ...(FEED_VARS[type] ?? [])].map((v) => `\`{${v}}\``).join(' ');
}

/**
 * Parses the `message` option: `\n` becomes a newline; blank or missing means "no template".
 * @param {unknown} raw
 * @returns {{ template: string|null, error?: string }}
 */
export function parseMessageOption(raw) {
  if (raw === null || raw === undefined) return { template: null };
  const t = String(raw).replace(/\\n/g, '\n').trim();
  if (!t) return { template: null };
  if (t.length > 1000) return { error: 'The message can be at most 1000 characters.', template: null };
  return { template: t };
}

let allowedTokensCache = null;
function allowedTokens(type) {
  if (!allowedTokensCache) {
    const builtin = new Set();
    for (const key of Object.keys(PLACEHOLDERS)) {
      const k = key.replace(/^\{|\}$/g, '').toLowerCase();
      if (k.startsWith('user') || k === 'random:x:y') continue; // no member in a feed; random:x:y is matched by pattern
      builtin.add(k);
    }
    allowedTokensCache = builtin;
  }
  return new Set([...allowedTokensCache, ...FEED_VARS.common, ...(FEED_VARS[type] ?? [])]);
}

/**
 * Warnings for a message template: unknown variables stay literal, and YouTube / Bluesky posts need a
 * link for Discord to show the preview.
 * @param {string} type @param {string|null} template
 * @returns {string[]}
 */
export function templateWarnings(type, template) {
  if (template === null || template === undefined) return [];
  const out = [];
  const tokens = [...new Set([...String(template).matchAll(/\{([^{}\n]{1,64})\}/g)].map((m) => m[1].toLowerCase()))];
  const allowed = allowedTokens(type);
  const bad = tokens.filter((t) => !allowed.has(t) && !/^random:-?\d{1,10}:-?\d{1,10}$/.test(t));
  if (bad.length) {
    let msg = `⚠️ These variables don't work for ${TYPE_LABELS[type]}s and will appear as typed: ${joinWithLimit(bad.map((t) => `\`{${t}}\``), { max: 500 })}`;
    if (bad.some((t) => t.startsWith('user'))) msg += ' (feeds have no member, so {user…} variables never work)';
    out.push(msg);
  }
  if ((type === 'YOUTUBE' || type === 'BLUESKY') && !/\{(link|url)\}/i.test(template)) {
    out.push("⚠️ This message has no {link}, so Discord won't show a preview of the post.");
  }
  return out;
}

// Third-party text: also neutralise masked links and line-leading headings / lists (off by default in discord.js).
const escapeThirdParty = (s) => escapeMarkdown(String(s ?? ''), { maskedLink: true, heading: true, bulletedList: true, numberedList: true });

/** The variables for a message template. Text values are markdown-escaped; URLs are not. */
export function buildFeedVars(feed, item, { cardStyle = false } = {}) {
  const esc = escapeThirdParty;
  const link = item.url ? (cardStyle ? `<${item.url}>` : item.url) : '';
  const vars = { title: esc(item.title), url: link, link, author: esc(item.author), feed: esc(feed.displayName) };
  switch (feed.type) {
    case 'YOUTUBE':
      vars.youtube_channel = esc(item.author || feed.displayName);
      break;
    case 'REDDIT':
      vars.subreddit = esc(String(feed.displayName).replace(/^r\//i, ''));
      break;
    case 'BLUESKY':
      vars.bluesky_name = esc(item.actorName || String(feed.displayName).replace(/^@/, ''));
      vars.bluesky_handle = esc(item.actorHandle || String(feed.displayName).replace(/^@/, ''));
      break;
    case 'PODCAST':
      vars.podcast_title = esc(feed.displayName);
      vars.episode_title = esc(item.title);
      break;
    default:
  }
  return vars;
}

/** The embed card for RSS, Reddit and podcast items. Invalid third-party URLs are dropped, never thrown. */
export function buildItemEmbed(feed, item, { feedImage = null } = {}) {
  const e = neutralEmbed(
    clampText(item.title, 256),
    item.summary ? escapeThirdParty(clampText(item.summary, 350)) : undefined, // a description renders masked links
    clampText(feed.displayName, 2048),
  ).setColor(TYPE_COLORS[feed.type]);
  e.setTimestamp(item.publishedAt instanceof Date && !Number.isNaN(item.publishedAt.getTime()) ? item.publishedAt : new Date());
  const safely = (fn) => { try { fn(); } catch { /* an embed URL discord.js rejects is simply left out */ } };
  if (item.url) safely(() => e.setURL(item.url));
  if (item.author) e.setAuthor({ name: clampText(item.author, 256) });
  const img = item.image ?? (feed.type === 'PODCAST' ? feedImage : null);
  if (img) safely(() => (feed.type === 'RSS' ? e.setImage(img) : e.setThumbnail(img)));
  return e;
}

/**
 * The message for one item. Text is clamped to 2000; feed text can never ping (`parse: []`), and the
 * ping role is allowed only when `pingableRoleIds` agrees for the bot in that channel (rule 23).
 * `ping: false` (the /feed test preview) shows the role mention but notifies nobody.
 * @returns {{ content: string|undefined, embeds: import('discord.js').EmbedBuilder[], allowedMentions: { parse: [], roles: string[] } }}
 */
export function buildPostPayload(feed, item, { guild, channel, feedImage = null, ping = true } = {}) {
  const card = CARD_TYPES.has(feed.type);
  const tpl = feed.messageTemplate?.trim() ? feed.messageTemplate : DEFAULT_TEMPLATES[feed.type];
  let content = renderPlaceholders(tpl, { guild, channel, vars: buildFeedVars(feed, item, { cardStyle: card }) }, { maxLength: 2000 });
  const roleOk = !!feed.pingRoleId && !!guild?.roles?.cache?.has(feed.pingRoleId); // a deleted role gets no prefix
  if (roleOk) content = clampText(`<@&${feed.pingRoleId}> ${content}`.trimEnd(), 2000);
  const embeds = card ? [buildItemEmbed(feed, item, { feedImage })] : [];
  if (!content.trim() && !embeds.length) content = clampText(item.url ?? item.title, 2000);
  const roles = ping && roleOk ? pingableRoleIds(channel, guild?.members?.me ?? null, [feed.pingRoleId]).allowed : [];
  return { content: content.trim() ? content : undefined, embeds, allowedMentions: { parse: [], roles } };
}

/** The new display name after a successful fetch, or null for no change. */
export function displayNameUpdate(feed, parsed) {
  if (feed.type === 'YOUTUBE' && feed.displayName === feed.source && parsed?.title) return clampText(parsed.title, 100);
  if (feed.type === 'BLUESKY') {
    const h = parsed?.items?.[0]?.actorHandle;
    if (h && `@${h}` !== feed.displayName) return clampText(`@${h}`, 100);
  }
  return null;
}

/** The human link for a feed (angle brackets suppress Discord's preview). */
export function sourceLink(feed) {
  switch (feed.type) {
    case 'YOUTUBE': return `<https://www.youtube.com/channel/${feed.source}>`;
    case 'REDDIT': return `<https://www.reddit.com/r/${feed.source}>`;
    case 'BLUESKY': return `<https://bsky.app/profile/${feed.source}>`;
    default: return `<${feed.fetchUrl}>`;
  }
}

/**
 * One line for a fetch error (≤ 200 characters). `err` is the normalised error of fetchAndParse.
 * @param {string} type @param {{ code?: string, status?: number, message?: string, apiError?: string }} err
 */
export function describeFetchError(type, err) {
  const e = err ?? {};
  let base;
  switch (e.code) {
    case 'HTTP': base = `HTTP ${e.status}`; break;
    case 'TIMEOUT': base = 'Timed out after 10 s'; break;
    case 'TOO_LARGE': base = `Feed is larger than ${(MAX_BYTES[type] ?? MAX_BYTES.RSS) / 1048576} MB`; break;
    case 'DNS_FAILED': base = 'Domain not found'; break;
    case 'BLOCKED_ADDRESS': base = 'That address is private or reserved (blocked)'; break;
    case 'TOO_MANY_REDIRECTS': base = 'Too many redirects'; break;
    case 'INVALID_URL': base = 'Invalid URL'; break;
    case 'NETWORK': base = `Network error (${clampText(e.message, 80)})`; break;
    case 'UNEXPECTED_304': base = 'The server answered "304 Not Modified" to a request that was not conditional, so it sent no feed'; break;
    case 'HTML_PAGE': base = 'The URL returned a web page, not a feed'; break;
    case 'NOT_A_FEED': base = 'Not an RSS or Atom feed'; break;
    case 'INVALID_XML': base = 'Feed XML could not be parsed'; break;
    default: base = clampText(e.message || 'Unknown error', 120);
  }
  let hint = '';
  const status = e.status;
  if (type === 'YOUTUBE' && status === 404) hint = "YouTube's public channel feed is returning 404 (a known YouTube outage)";
  else if (type === 'REDDIT' && status === 403) hint = "Reddit refused the request (private subreddit, or Reddit blocks this server's IP)";
  else if (type === 'REDDIT' && status === 429) hint = 'Reddit rate limit';
  else if (type === 'REDDIT' && e.code === 'HTML_PAGE') hint = 'Reddit returned a web page instead of the feed (probably blocking this server)';
  else if (type === 'BLUESKY' && status === 400) hint = `Bluesky account unavailable${e.apiError ? ` (${e.apiError})` : ''}`;
  else if (status === 404) hint = 'not found';
  else if (status === 410) hint = 'the feed was removed';
  else if (status === 401 || status === 403) hint = 'access denied';
  else if (status >= 500) hint = 'the site is having problems';
  return clampText(base + (hint ? ` — ${hint}` : ''), 200);
}

/** `{ icon, text }` for /feed list and /feed test. */
export function feedStatus(feed) {
  if (!feed.enabled) return { icon: '⏸️', text: `Disabled — ${feed.disabledReason || 'paused'}` };
  if (feed.failureCount > 0) return { icon: '⚠️', text: `Failing ${feed.failureCount}× — ${feed.lastError || 'unknown error'}` };
  if (feed.lastError) return { icon: '⚠️', text: feed.lastError };
  if (!feed.initialized) return { icon: '🕓', text: 'Waiting for the first check' };
  return { icon: '✅', text: 'OK' };
}

// ── Cache (de)serialisation ────────────────────────────────────────────────────

function serializeForCache(parsed) {
  return {
    v: 1,
    fetchedAt: new Date().toISOString(),
    title: parsed.title ?? null,
    link: parsed.link ?? null,
    image: parsed.image ?? null,
    items: parsed.items.slice(0, WINDOW_SIZE).map((i) => ({ ...i, publishedAt: i.publishedAt ? i.publishedAt.toISOString() : null })),
  };
}

function reviveCached(items) {
  const out = [];
  for (const i of items) {
    if (!i || typeof i.id !== 'string') continue;
    const at = i.publishedAt ? Date.parse(i.publishedAt) : NaN;
    out.push({ ...i, publishedAt: Number.isFinite(at) ? new Date(at) : null });
  }
  return out;
}

// ── Fetching ───────────────────────────────────────────────────────────────────

const userAgentFor = (type) => (type === 'REDDIT' ? process.env.REDDIT_USER_AGENT?.trim() || DEFAULT_USER_AGENT : DEFAULT_USER_AGENT);
const acceptFor = (type) => (type === 'BLUESKY'
  ? 'application/json'
  : 'application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.8');

function tryJsonError(res) {
  try {
    const e = res.json()?.error;
    return typeof e === 'string' ? clampText(e, 60) : undefined;
  } catch {
    return undefined;
  }
}

/**
 * Fetches and parses one feed URL. Never throws.
 * @param {string} url @param {string} type
 * @param {{ headers?: Record<string, string> }} [opts] extra request headers (conditional GET)
 * @returns {Promise<
 *   { ok: true, notModified: false, parsed: object, etag: string|null, lastModified: string|null, finalUrl: string }
 *   | { ok: true, notModified: true }
 *   | { ok: false, error: { code: string, status?: number, message: string, retryAfterMs?: number|null, apiError?: string, links?: string[] } }>}
 */
export async function fetchAndParse(url, type, { headers = {} } = {}) {
  const fail = (error) => ({ ok: false, error });
  let res;
  try {
    res = await net.safeFetch(url, {
      headers: { 'user-agent': userAgentFor(type), accept: acceptFor(type), ...headers },
      timeoutMs: FETCH_TIMEOUT_MS,
      maxBytes: MAX_BYTES[type],
      maxRedirects: 5,
    });
  } catch (err) {
    if (err instanceof SafeFetchError) {
      return fail({ code: err.code === 'HTTP_ERROR' ? 'HTTP' : err.code, status: err.status, message: err.message, retryAfterMs: err.retryAfterMs });
    }
    return fail({ code: 'NETWORK', message: clampText(err?.message ?? 'network error', 120) });
  }
  const resHeaders = res.headers ?? {};
  if (res.status === 304) {
    // Not modified only answers a conditional request. An unprompted 304 carries no feed: a bad response.
    const conditional = Object.entries(headers).some(([k, v]) => /^(if-none-match|if-modified-since)$/i.test(k) && String(v ?? '').trim() !== '');
    if (conditional) return { ok: true, notModified: true };
    return fail({ code: 'UNEXPECTED_304', status: 304, message: 'HTTP 304 without a conditional request' });
  }
  if (!res.ok) {
    return fail({
      code: 'HTTP',
      status: res.status,
      message: `HTTP ${res.status}`,
      retryAfterMs: [429, 503].includes(res.status) ? parseRetryAfterMs(resHeaders['retry-after']) : null,
      apiError: type === 'BLUESKY' ? tryJsonError(res) : undefined,
    });
  }

  let parsed;
  try {
    if (type === 'BLUESKY') {
      let json;
      try {
        json = res.json();
      } catch {
        return fail({ code: 'BAD_JSON', message: 'Bluesky sent invalid JSON' });
      }
      parsed = parseBlueskyAuthorFeed(json);
    } else {
      const body = decodeXmlBuffer(res.buffer, resHeaders['content-type']);
      if (looksLikeHtml(body, resHeaders['content-type'])) {
        return fail({ code: 'HTML_PAGE', message: 'Web page, not a feed', links: discoverFeedLinks(body, res.url ?? url) });
      }
      parsed = parseXmlFeed(body);
    }
  } catch (err) {
    if (err instanceof FeedParseError) return fail({ code: err.code, message: err.message });
    logger.warn(`feed parse failed unexpectedly for ${type}: ${err?.message}`);
    return fail({ code: 'INVALID_XML', message: 'Feed XML could not be parsed' });
  }

  return {
    ok: true,
    notModified: false,
    parsed: { ...parsed, items: windowItems(parsed.items) },
    etag: clampText(resHeaders.etag ?? '', 200) || null,
    lastModified: clampText(resHeaders['last-modified'] ?? '', 200) || null,
    finalUrl: res.url ?? url,
  };
}

/**
 * Bot-wide proactive Reddit budget (D12): true when this network fetch would exceed REDDIT_MAX_PER_MIN.
 * Redis down (`null`) falls back to an in-process counter, so the budget still holds for this shard.
 */
// One 60 s slot key per allowed fetch. A refused attempt claims no slot, so a herd of due Reddit feeds
// (end of a 429 block, overdue jobs after downtime) can't keep the budget full with its own refusals.
async function redditOverBudget() {
  for (let slot = 0; slot < REDDIT_MAX_PER_MIN; slot++) {
    const claimed = await cache.setNX(`${REDDIT_RL_KEY}:${slot}`, 1, 60);
    if (claimed === null) return !localRedditAllow();   // Redis down: in-process budget
    if (claimed) return false;
  }
  return true;
}

const budgetError = () => ({ code: 'RATE_BUDGET', message: 'Reddit checks are paused for about a minute (bot-wide limit)' });

/**
 * When Reddit last answered 429, every Reddit feed waits (feed:block:reddit). Fail-open: with Redis down no
 * block is known, so the fetch goes ahead (the proactive budget still applies).
 * @returns {Promise<number|null>} epoch ms the block lasts until, or null
 */
async function redditBlockedUntil() {
  const block = await cache.get(REDDIT_BLOCK_KEY);
  const until = block?.until ? Date.parse(block.until) : NaN;
  return Number.isFinite(until) && until > Date.now() ? until : null;
}

/**
 * Fetches a feed row's source: the shared Redis cache first, then the network (conditional GET for
 * initialised feeds). Never throws.
 * @param {object} feed a Feed row (type, fetchUrl, initialized, etag, lastModified)
 * @param {{ useCache?: boolean, conditional?: boolean }} [opts]
 * @returns {Promise<
 *   { ok: true, notModified: boolean, fromCache: boolean, title?: string|null, image?: string|null, items?: object[], etag?: string|null, lastModified?: string|null }
 *   | { ok: false, error: object, budget?: boolean }>}
 */
export async function fetchFeedSource(feed, { useCache = true, conditional = true } = {}) {
  const key = fetchCacheKey(feed.fetchUrl);
  if (useCache) {
    // Fail-open: when Redis is down cache.get returns null and every guild fetches for itself.
    const hit = await cache.get(key);
    if (hit?.v === 1 && Array.isArray(hit.items)) {
      return {
        ok: true, notModified: false, fromCache: true, title: hit.title ?? null, image: hit.image ?? null,
        items: reviveCached(hit.items), etag: null, lastModified: null,
      };
    }
  }
  if (feed.type === 'REDDIT') {
    // Covers /feed test too: an admin's preview never adds a request while Reddit asked us to slow down.
    const until = await redditBlockedUntil();
    if (until) {
      return { ok: false, budget: true, fromCache: false, error: { code: 'RATE_BUDGET', message: `Reddit asked this bot to slow down until ${new Date(until).toISOString()}` } };
    }
    if (await redditOverBudget()) return { ok: false, budget: true, fromCache: false, error: budgetError() };
  }
  const headers = {};
  if (conditional && feed.initialized) {
    if (feed.etag) headers['if-none-match'] = feed.etag;
    if (feed.lastModified) headers['if-modified-since'] = feed.lastModified;
  }
  const r = await fetchAndParse(feed.fetchUrl, feed.type, { headers });
  if (!r.ok) return { ok: false, fromCache: false, error: r.error };
  if (r.notModified) return { ok: true, notModified: true, fromCache: false };
  await cache.set(key, serializeForCache(r.parsed), cacheTtlSeconds(feed.type)); // best-effort; the result is ignored
  return {
    ok: true, notModified: false, fromCache: false, title: r.parsed.title, image: r.parsed.image,
    items: r.parsed.items, etag: r.etag, lastModified: r.lastModified,
  };
}

// ── Add-time resolution ────────────────────────────────────────────────────────

const NOT_A_FEED_PAGE = "That page is a website, not an RSS/Atom feed, and it doesn't link to one. Paste the feed URL itself (it often ends in /feed, /rss or .xml).";

function refuse(title, message) {
  return { ok: false, title, message };
}

/** RSS and PODCAST URLs: fetch, follow one autodiscovered link when the URL is a web page, and refuse politely. */
async function resolveXmlFeedUrl(startUrl, type) {
  let url = startUrl;
  let r = await fetchAndParse(url, type);
  if (!r.ok && r.error.code === 'HTML_PAGE') {
    if (!r.error.links?.length) return refuse('Not a Feed', NOT_A_FEED_PAGE);
    url = r.error.links[0];
    r = await fetchAndParse(url, type); // one hop only
    if (!r.ok && r.error.code === 'HTML_PAGE') return refuse('Not a Feed', NOT_A_FEED_PAGE);
  }
  if (!r.ok) {
    const code = r.error.code;
    if (code === 'TOO_LARGE') {
      return type === 'PODCAST'
        ? refuse('Feed Too Large', 'That podcast feed is bigger than 10 MB, which is too large to check regularly.')
        : refuse('Feed Too Large', "That feed is bigger than 2 MB, which is too large to check regularly. If it's a podcast, use `/feed add podcast` (allows up to 10 MB).");
    }
    if (code === 'NOT_A_FEED' || code === 'INVALID_XML' || code === 'BAD_JSON') return refuse('Not a Feed', "I couldn't read that as an RSS or Atom feed.");
    return refuse('Could Not Fetch', `I couldn't load that feed: ${describeFetchError(type, r.error)}. Check the URL and try again.`);
  }
  let displayName = r.parsed.title;
  if (!displayName) {
    try { displayName = new URL(url).hostname; } catch { displayName = 'Feed'; }
  }
  return {
    ok: true, source: url, fetchUrl: url, displayName,
    probe: { ok: true, items: r.parsed.items, etag: r.etag, lastModified: r.lastModified },
    warnings: [],
    image: r.parsed.image,
  };
}

async function setRedditBlock(error, now = new Date()) {
  const ttlMs = Math.min(Math.max(error.retryAfterMs ?? 15 * 60_000, 10 * 60_000), 60 * 60_000);
  const until = new Date(now.getTime() + ttlMs);
  await cache.set(REDDIT_BLOCK_KEY, { until: until.toISOString() }, Math.ceil(ttlMs / 1000));
  return until;
}

async function resolveYouTube({ channelId }) {
  const fetchUrl = `https://www.youtube.com/feeds/videos.xml?channel_id=${channelId}`;
  const r = await fetchAndParse(fetchUrl, 'YOUTUBE');
  if (r.ok && !r.notModified) {
    return {
      ok: true, source: channelId, fetchUrl, displayName: r.parsed.title || channelId,
      probe: { ok: true, items: r.parsed.items, etag: r.etag, lastModified: r.lastModified }, warnings: [],
    };
  }
  // Any failure, 404 included, is accepted: the public feed has been unreliable since late 2025.
  return {
    ok: true, source: channelId, fetchUrl, displayName: channelId,
    probe: { ok: false, error: r.error ?? { code: 'UNKNOWN', message: 'No response' } },
    warnings: ["⚠️ YouTube's public channel feed has been unreliable since late 2025 and often returns 404. I'll keep trying; if it never works, `/feed list` shows the error and the feed switches itself off after 20 failed checks."],
  };
}

async function resolveReddit({ name }) {
  const fetchUrl = `https://www.reddit.com/r/${name}/new/.rss`;
  const base = { ok: true, source: name.toLowerCase(), fetchUrl, displayName: `r/${name}` };
  const failed = (error, why) => ({
    ...base,
    probe: { ok: false, error },
    warnings: [`⚠️ Reddit feeds are best-effort and ${why} (${describeFetchError('REDDIT', error)}). I'll keep trying and show the status in \`/feed list\`.`],
  });
  // Respect the bot-wide block and budget even for an admin's add: skip the probe, the first poll seeds.
  const blockedUntil = await redditBlockedUntil();
  if (blockedUntil) {
    return failed({ code: 'RATE_BUDGET', message: `Reddit asked this bot to slow down until ${new Date(blockedUntil).toISOString()}` }, 'is rate limiting this bot right now');
  }
  if (await redditOverBudget()) return failed(budgetError(), 'this bot is at its Reddit request limit for the minute');
  const r = await fetchAndParse(fetchUrl, 'REDDIT');
  if (r.ok && !r.notModified) {
    return { ...base, probe: { ok: true, items: r.parsed.items, etag: r.etag, lastModified: r.lastModified }, warnings: [] };
  }
  const error = r.error ?? { code: 'UNKNOWN', message: 'No response' };
  if (error.status === 404) return refuse('Subreddit Not Found', `r/${name} doesn't exist, is banned, or can't be read.`);
  if (error.status === 429) await setRedditBlock(error);
  return failed(error, 'Reddit refused the request just now');
}

async function resolveBluesky({ actor }) {
  const profileUrl = `${BSKY_APPVIEW}/xrpc/app.bsky.actor.getProfile?actor=${encodeURIComponent(actor)}`;
  let profile;
  try {
    profile = await net.safeFetchJson(profileUrl, { headers: { 'user-agent': DEFAULT_USER_AGENT }, timeoutMs: FETCH_TIMEOUT_MS, maxBytes: 256 * 1024 });
  } catch (err) {
    if (err instanceof SafeFetchError && err.status === 400) return refuse('Account Not Found', `I couldn't find the Bluesky account **${clampText(actor, 100)}**.`);
    const e = err instanceof SafeFetchError
      ? { code: err.code === 'HTTP_ERROR' ? 'HTTP' : err.code, status: err.status, message: err.message }
      : { code: 'NETWORK', message: err?.message };
    return refuse('Bluesky Unavailable', `Bluesky didn't answer (${describeFetchError('BLUESKY', e)}). Please try again in a minute.`);
  }
  const did = profile?.did;
  if (typeof did !== 'string' || !DID_RE.test(did)) return refuse('Bluesky Unavailable', "Bluesky didn't answer (unexpected response). Please try again in a minute.");
  const handle = profile.handle && profile.handle !== 'handle.invalid' ? String(profile.handle) : did;
  const fetchUrl = `${BSKY_APPVIEW}/xrpc/app.bsky.feed.getAuthorFeed?actor=${encodeURIComponent(did)}&filter=posts_no_replies&limit=30`;
  const r = await fetchAndParse(fetchUrl, 'BLUESKY');
  const out = { ok: true, source: did, fetchUrl, displayName: `@${handle}` };
  if (r.ok && !r.notModified) {
    return { ...out, probe: { ok: true, items: r.parsed.items, etag: r.etag, lastModified: r.lastModified }, warnings: [] };
  }
  const error = r.error ?? { code: 'UNKNOWN', message: 'No response' };
  return { ...out, probe: { ok: false, error }, warnings: [`⚠️ I found the account but couldn't load its posts yet (${describeFetchError('BLUESKY', error)}). I'll keep trying.`] };
}

async function resolvePodcast({ url, query }) {
  if (url) return resolveXmlFeedUrl(url, 'PODCAST');
  const q = String(query ?? '').trim();
  // Fail-open on a null count (Redis down): a handful of admin searches cannot hurt Apple.
  const n = await cache.slidingWindow(ITUNES_RL_KEY, 60_000);
  if (n !== null && n > ITUNES_MAX_PER_MIN) return refuse('Podcast Search Busy', 'Too many podcast searches right now. Try again in a minute, or paste the RSS feed URL.');
  let json;
  try {
    json = await net.safeFetchJson(
      `https://itunes.apple.com/search?media=podcast&entity=podcast&limit=5&term=${encodeURIComponent(q)}`,
      { headers: { 'user-agent': DEFAULT_USER_AGENT }, timeoutMs: FETCH_TIMEOUT_MS, maxBytes: 512 * 1024 },
    );
  } catch (err) {
    const e = err instanceof SafeFetchError
      ? { code: err.code === 'HTTP_ERROR' ? 'HTTP' : err.code, status: err.status, message: err.message }
      : { code: 'NETWORK', message: err?.message };
    return refuse('Podcast Search Failed', `Apple's podcast search didn't answer (${describeFetchError('PODCAST', e)}). Try again, or paste the RSS feed URL.`);
  }
  const hit = parseItunesSearch(json)[0];
  if (!hit) return refuse('Podcast Not Found', `No podcast found for "${clampText(q, 80)}". Try its exact title, or paste its RSS feed URL.`);
  const r = await resolveXmlFeedUrl(hit.feedUrl, 'PODCAST');
  if (!r.ok) return r;
  return {
    ...r,
    displayName: hit.name || r.displayName,
    warnings: [...r.warnings, `Matched **${escapeMarkdown(hit.name)}**${hit.artist ? ` by ${escapeMarkdown(hit.artist)}` : ''}. Not the right show? Remove it and add its RSS feed URL instead.`],
  };
}

/**
 * Resolves what an admin typed into a pollable source and probes it once (the probe seeds the feed).
 * Never throws for expected failures.
 * @param {'RSS'|'YOUTUBE'|'REDDIT'|'BLUESKY'|'PODCAST'} type
 * @param {{ url?: string, channelId?: string, name?: string, actor?: string, query?: string }} input
 *   RSS `{ url }`, YOUTUBE `{ channelId }`, REDDIT `{ name }`, BLUESKY `{ actor }`, PODCAST `{ url }` or `{ query }`
 * @returns {Promise<{ ok: true, source: string, fetchUrl: string, displayName: string,
 *   probe: { ok: true, items: object[], etag: string|null, lastModified: string|null } | { ok: false, error: object },
 *   warnings: string[] } | { ok: false, title: string, message: string }>}
 */
export async function resolveFeedSource(type, input) {
  try {
    switch (type) {
      case 'RSS': return await resolveXmlFeedUrl(input.url, 'RSS');
      case 'YOUTUBE': return await resolveYouTube(input);
      case 'REDDIT': return await resolveReddit(input);
      case 'BLUESKY': return await resolveBluesky(input);
      case 'PODCAST': return await resolvePodcast(input);
      default: return refuse('Unknown Feed Type', 'That kind of feed is not supported.');
    }
  } catch (err) {
    logger.error(`feeds: resolving a ${type} source failed`, err);
    return refuse('Something went wrong', 'I could not check that source. Please try again.');
  }
}

// ── Database helpers ───────────────────────────────────────────────────────────

/**
 * A feed of this guild by id, else by an exact (case-insensitive) display name when exactly one matches.
 * Guild-scoped, so another guild's id never matches.
 */
export async function findFeedForGuild(guildId, raw) {
  const value = String(raw ?? '');
  const byId = await prisma.feed.findFirst({ where: { id: value, guildId } });
  if (byId) return byId;
  const byName = await prisma.feed.findMany({ where: { guildId, displayName: { equals: value.trim(), mode: 'insensitive' } }, take: 2 });
  return byName.length === 1 ? byName[0] : null;
}

const createMutex = createKeyedMutex();

/**
 * Creates a feed row under a per-guild mutex (cap check + insert). A successful probe seeds the row
 * (`initialized`, `seenIds`), so nothing already in the feed is ever posted.
 * @throws {FeedUserError} Feed Limit Reached, Already Following
 */
export async function createFeedRow({
  guildId, type, source, displayName, fetchUrl, channelId, pingRoleId = null, messageTemplate = null,
  includeReposts = false, createdBy, probe = null,
}) {
  return createMutex.run(guildId, async () => {
    if (await prisma.feed.count({ where: { guildId } }) >= LIMITS.FEEDS) {
      throw new FeedUserError('Feed Limit Reached', `This server already follows ${LIMITS.FEEDS} feeds (the maximum). Remove one with \`/feed remove\` first.`);
    }
    await ensureGuildSettings(guildId);
    const now = new Date();
    const seeded = !!probe?.ok;
    try {
      return await prisma.feed.create({
        data: {
          guildId, type, source, displayName: clampText(displayName, 100), fetchUrl, channelId, pingRoleId,
          messageTemplate, includeReposts: type === 'BLUESKY' && includeReposts, createdBy,
          initialized: seeded,
          seenIds: seeded ? [...new Set(probe.items.map((i) => i.id))].slice(0, SEEN_CAP) : [],
          etag: seeded ? probe.etag : null,
          lastModified: seeded ? probe.lastModified : null,
          lastCheckedAt: probe ? now : null,
          lastSuccessAt: seeded ? now : null,
          lastError: probe && !probe.ok ? describeFetchError(type, probe.error) : null,
        },
      });
    } catch (err) {
      if (err?.code === 'P2002') throw new FeedUserError('Already Following', `That feed is already posted in <#${channelId}>.`);
      throw err;
    }
  });
}

/** (Re)schedules the feed's poll job; `replace` bumps the version so an in-flight run finishes as a no-op. */
export function scheduleFeedJob(feed, { runAt }) {
  return scheduleJob({
    guildId: feed.guildId, type: 'feed.poll', runAt, payload: { feedId: feed.id },
    dedupeKey: feedJobKey(feed.id), maxAttempts: 10, onConflict: 'replace',
  });
}

/**
 * Posts one notice about feeds that were switched off: in the feed's channel when there is exactly one
 * feed and the channel is usable, else in the mod-log channel, else only the log has it. Never throws.
 */
export async function sendDisableNotice(guild, feeds, settings, { preferFeedChannel = true } = {}) {
  try {
    const usable = (ch) => !!ch && checkChannelUsable(ch, { needs: POST_CHANNEL_NEEDS }).ok;
    let target = null;
    if (preferFeedChannel && feeds.length === 1) {
      const ch = guild.channels.cache.get(feeds[0].channelId);
      if (usable(ch)) target = ch;
    }
    if (!target && settings?.modLogChannelId) {
      const ch = guild.channels.cache.get(settings.modLogChannelId);
      if (usable(ch)) target = ch;
    }
    if (!target) return false;
    const lines = feeds.map((f) => `${TYPE_EMOJI[f.type] ?? '📰'} **${escapeMarkdown(f.displayName)}** — ${f.disabledReason}`);
    const channelProblem = feeds.some((f) => /^(Its channel|I lost access)/.test(String(f.disabledReason)));
    const body = `${joinWithLimit(lines, { sep: '\n', max: 3500 })}\n${channelProblem
      ? 'Pick a new channel with `/feed edit channel:`, then run `/feed enable`.'
      : 'Fix the problem, then run `/feed enable`.'}`;
    await withTimeout(
      target.send({ embeds: [warnEmbed(feeds.length === 1 ? 'Feed Disabled' : 'Feeds Disabled', body)], allowedMentions: { parse: [] } }),
      SEND_TIMEOUT_MS,
      'feed notice',
    );
    return true;
  } catch (err) {
    logger.warn(`feeds: could not post the disable notice in guild ${guild?.id}: ${err?.message}`);
    return false;
  }
}

/**
 * Disables a feed and cancels its job. The conditional update guarantees exactly one notice per disable.
 * @returns {Promise<boolean>} true when this call disabled it
 */
export async function disableFeed(feed, reason, ctx, settings, { notify = false } = {}) {
  const { count } = await prisma.feed.updateMany({
    where: { id: feed.id, enabled: true },
    data: { enabled: false, disabledReason: clampText(reason, 200) },
  });
  if (count !== 1) return false; // someone else already disabled it: no second notice
  await cancelJobs({ dedupeKey: feedJobKey(feed.id) }); // harmless inside the handler (its completion update then affects 0 rows)
  logger.warn(`Feed "${feed.displayName}" (${feed.id}) in guild ${feed.guildId} was disabled: ${reason}`);
  if (notify) await sendDisableNotice(ctx.guild, [{ ...feed, disabledReason: reason }], settings);
  return true;
}

/** `channelDelete` hook: disables (keeps) the feeds posting there. The rows stay so an admin can re-point them. */
export async function disableFeedsForChannel(ctx) {
  const { guild, guildId, channel } = ctx;
  const rows = await prisma.feed.findMany({ where: { guildId, channelId: channel.id, enabled: true } });
  if (!rows.length) return 0;
  await prisma.feed.updateMany({
    where: { id: { in: rows.map((r) => r.id) }, enabled: true },
    data: { enabled: false, disabledReason: 'Its channel was deleted' },
  });
  for (const r of rows) await cancelJobs({ dedupeKey: feedJobKey(r.id) });
  logger.info(`Feeds: disabled ${rows.length} feed(s) in guild ${guildId}; channel ${channel.id} was deleted`);
  const settings = await ctx.getSettings();
  if (settings?.modLogChannelId && settings.modLogChannelId !== channel.id) {
    await sendDisableNotice(guild, rows.map((r) => ({ ...r, disabledReason: 'Its channel was deleted' })), settings, { preferFeedChannel: false });
  }
  return rows.length;
}

/**
 * `guildReady` hook: every enabled feed must have a live poll job. Healthy jobs are left alone; missing,
 * FAILED, DONE or CANCELLED ones are replaced with a random spread (no thundering herd after a restart).
 */
export async function ensureGuildFeedJobs(guild) {
  const rows = await prisma.feed.findMany({ where: { guildId: guild.id, enabled: true }, select: { id: true, guildId: true, type: true } });
  if (!rows.length) return;
  const jobs = await getJobsByKeys(rows.map((r) => feedJobKey(r.id)));
  if (!jobs) return; // could not look: replacing healthy jobs would reset their timers
  for (const r of rows) {
    try {
      const job = jobs.get(feedJobKey(r.id));
      if (job && (job.status === 'PENDING' || job.status === 'RUNNING')) continue; // healthy
      const spread = 30_000 + randomInt(BASE_INTERVAL_MS[r.type]);
      await scheduleFeedJob(r, { runAt: new Date(Date.now() + spread) });
    } catch (err) {
      logger.error(`feeds: could not ensure job for ${r.id}`, err);
    }
  }
}

// ── Polling ────────────────────────────────────────────────────────────────────

const recordChannelProblem = (feed, text, now) => prisma.feed.updateMany({
  where: { id: feed.id },
  data: { lastError: clampText(text, 200), lastCheckedAt: now },
});

async function onFetchFailure(feed, error, ctx, settings, now) {
  // Reddit 429: a bot-wide block (every Reddit feed waits), not a failure of this feed.
  if (feed.type === 'REDDIT' && error.status === 429) {
    const until = await setRedditBlock(error, now);
    await prisma.feed.updateMany({
      where: { id: feed.id },
      data: {
        lastError: clampText(`${describeFetchError('REDDIT', error)} — all Reddit feeds wait until ${discordTimestamp(until, 't')}`, 200),
        lastCheckedAt: now,
      },
    });
    return { nextRunAt: new Date(until.getTime() + randomInt(60_000)) };
  }
  const updated = await prisma.feed.update({
    where: { id: feed.id },
    data: { failureCount: { increment: 1 }, lastError: describeFetchError(feed.type, error), lastCheckedAt: now },
  }).catch(() => null);
  if (!updated) return undefined; // removed meanwhile
  if (updated.failureCount >= MAX_FAILURES) {
    await disableFeed(updated, `${MAX_FAILURES} failed checks in a row: ${updated.lastError}`, ctx, settings, { notify: true });
    return undefined;
  }
  return {
    nextRunAt: computeNextPollAt({
      type: feed.type, failureCount: updated.failureCount, status: error.status ?? null, retryAfterMs: error.retryAfterMs ?? null, now,
    }),
  };
}

async function deliverItems(feed, sel, res, channel, ctx, settings, { started, now }) {
  const retry = new Set();
  let posted = 0;
  let problem = null;
  for (let k = 0; k < sel.toPost.length; k++) {
    const item = sel.toPost[k];
    if (Date.now() - started > SEND_BUDGET_MS) { sel.toPost.slice(k).forEach((i) => retry.add(i.id)); break; }
    const payload = buildPostPayload(feed, item, { guild: ctx.guild, channel, feedImage: res.image, ping: true });
    try {
      await withTimeout(channel.send(payload), SEND_TIMEOUT_MS, `feed send ${feed.id}`);
      posted++;
    } catch (err) {
      if (err instanceof TimeoutError) continue; // may have landed: keep it recorded
      if (err?.code === 10003 || err?.code === 50001) {
        await disableFeed(feed, 'Its channel was deleted or I lost access to it', ctx, settings, { notify: true });
        return undefined; // remaining items are dropped
      }
      if (err?.code === 50035) { // permanent for this item
        logger.warn(`Feed ${feed.id}: Discord rejected item ${item.id}: ${err.message}`);
        continue;
      }
      problem = err?.code === 50013 ? `Can't post in #${channel.name}: Missing Permissions` : `Couldn't post: ${clampText(err?.message ?? 'error', 120)}`;
      sel.toPost.slice(k).forEach((i) => retry.add(i.id)); // 50013, 5xx, network: retry next poll
      break;
    }
  }
  const data = {};
  if (posted) data.lastPostedAt = new Date();
  if (retry.size) data.seenIds = sel.nextSeenIds.filter((id) => !retry.has(id));
  if (problem) data.lastError = clampText(problem, 200);
  if (Object.keys(data).length) {
    await prisma.feed.updateMany({ where: { id: feed.id }, data }).catch((e) => logger.warn(`feed ${feed.id} post-send update failed: ${e.message}`));
  }
  return { nextRunAt: computeNextPollAt({ type: feed.type, now }) };
}

async function runPoll(feed, settings, ctx, { started, now }) {
  // 1. Channel. A cache miss is never a deletion (rule 20): confirm with a REST fetch first.
  let channel = ctx.guild.channels.cache.get(feed.channelId);
  if (!channel) {
    const got = await ctx.guild.channels.fetch(feed.channelId).catch((e) => e);
    if (got instanceof Error) {
      if (got.code === 10003 || got.code === 50001) {
        await disableFeed(feed, 'Its channel was deleted or I lost access to it', ctx, settings, { notify: true });
        return undefined;
      }
      return { nextRunAt: new Date(now.getTime() + 5 * 60_000) }; // transient; nothing recorded
    }
    if (!got) {
      await disableFeed(feed, 'Its channel was deleted or I lost access to it', ctx, settings, { notify: true });
      return undefined;
    }
    channel = got;
  }
  if (!FEED_CHANNEL_TYPES.includes(channel.type)) {
    await disableFeed(feed, 'Its channel can no longer receive posts', ctx, settings, { notify: true });
    return undefined;
  }

  // 2. Permissions, before fetching: nothing is fetched that cannot be posted.
  const u = checkChannelUsable(channel, { needs: POST_CHANNEL_NEEDS });
  if (!u.ok) {
    if (u.missing.includes('ViewChannel')) {
      await disableFeed(feed, `I lost access to #${channel.name} (View Channel)`, ctx, settings, { notify: true });
      return undefined;
    }
    await recordChannelProblem(feed, `Can't post in #${channel.name}: missing ${u.missing.join(', ')}`, now);
    return { nextRunAt: computeNextPollAt({ type: feed.type, failureCount: 1, now }) }; // failureCount is NOT incremented
  }

  // 3. Reddit global block (set by any 429). Fail-open: Redis down means no block is known.
  if (feed.type === 'REDDIT') {
    const until = await redditBlockedUntil();
    if (until) return { nextRunAt: new Date(Math.max(until, started + 60_000) + randomInt(60_000)) };
  }

  // 4. Fetch.
  const res = await fetchFeedSource(feed, { useCache: true, conditional: true });
  if (!res.ok) {
    // Over the bot-wide Reddit budget: try again in a minute or two; nothing is recorded.
    if (res.budget) return { nextRunAt: new Date(Date.now() + 60_000 + randomInt(60_000)) };
    return onFetchFailure(feed, res.error, ctx, settings, now);
  }

  // 5. Not modified.
  if (res.notModified) {
    await prisma.feed.updateMany({ where: { id: feed.id }, data: { lastCheckedAt: now, lastSuccessAt: now, failureCount: 0, lastError: null } });
    return { nextRunAt: computeNextPollAt({ type: feed.type, now }) };
  }

  // 6. Select.
  const sel = selectNewItems({
    items: res.items, seenIds: feed.seenIds, initialized: feed.initialized,
    baseline: feed.lastSuccessAt ?? feed.createdAt, includeReposts: feed.includeReposts,
  });
  if (sel.reseed && feed.initialized) logger.info(`Feed ${feed.id}: ids all changed, re-seeded ${sel.skippedOld} item(s) without posting`);
  if (sel.overflow > 0) logger.info(`Feed ${feed.id}: skipped ${sel.overflow} older new item(s) over the per-poll cap`);

  // 7. Claim (optimistic, idempotent). Ids are recorded BEFORE sending: a crash loses items, never duplicates them.
  const rename = displayNameUpdate(feed, res);
  const { count } = await prisma.feed.updateMany({
    where: { id: feed.id, enabled: true, updatedAt: feed.updatedAt },
    data: {
      seenIds: sel.nextSeenIds, initialized: true,
      lastCheckedAt: now, lastSuccessAt: now, failureCount: 0, lastError: null,
      ...(res.fromCache ? {} : { etag: res.etag, lastModified: res.lastModified }),
      ...(rename ? { displayName: rename } : {}),
    },
  });
  if (count !== 1) return { nextRunAt: new Date(started + 60_000) }; // edited, paused, removed or a parallel run: re-evaluate

  // 8. Deliver.
  if (sel.toPost.length === 0) return { nextRunAt: computeNextPollAt({ type: feed.type, now }) };
  // The posts use the settings of the row as claimed (a rename changes only what is shown).
  return deliverItems(rename ? { ...feed, displayName: rename } : feed, sel, res, channel, ctx, settings, { started, now });
}

/**
 * The `feed.poll` job handler. Throws only when the initial DB read fails (the scheduler then retries
 * with backoff); every fetch, parse or Discord error ends in `{ nextRunAt }`, an auto-disable, or (for a
 * removed or paused feed) a dropped job (`undefined`).
 */
export async function pollFeedJob(job, ctx) {
  const started = Date.now();
  const now = new Date(started);
  const feedId = job.payload?.feedId;
  if (typeof feedId !== 'string') return undefined; // malformed → drop
  const feed = await prisma.feed.findFirst({ where: { id: feedId, guildId: job.guildId } }); // may throw → retry
  if (!feed || !feed.enabled) return undefined; // removed / paused → drop the job
  const settings = await getSettingsSafe(job.guildId);
  if (!settings) return { nextRunAt: new Date(started + 5 * 60_000) };
  if (!isModuleEnabled(settings, 'feeds')) return { nextRunAt: new Date(started + MODULE_OFF_RECHECK_MS) }; // no fetch while off
  try {
    return await runPoll(feed, settings, ctx, { started, now });
  } catch (err) {
    logger.error(`feed.poll ${feed.id} (guild ${feed.guildId}) failed unexpectedly`, err);
    return { nextRunAt: computeNextPollAt({ type: feed.type, failureCount: feed.failureCount, now }) };
  }
}
