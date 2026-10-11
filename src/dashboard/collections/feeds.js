// The `feeds` collection (Social Feeds): the Feed rows behind /feed, which post new RSS/Atom items, YouTube uploads,
// subreddit posts, Bluesky posts and podcast episodes in a channel. The source is entered exactly as /feed add takes
// it and parsed by the same pure parsers (src/services/feedParsers.js); the store then resolves and probes it.
// Pure: nothing is read or connected at import time.
import { LIMITS } from '../../config/constants.js';
import {
  normalizeFeedUrl, parseBlueskyActorInput, parseSubredditInput, parseYouTubeChannelInput,
} from '../../services/feedParsers.js';
import { POSTABLE_CHANNEL_KINDS } from './embeds.js';

export const FEED_TYPE_OPTIONS = Object.freeze([
  { value: 'RSS', label: 'RSS or Atom feed' },
  { value: 'YOUTUBE', label: 'YouTube channel' },
  { value: 'REDDIT', label: 'Subreddit' },
  { value: 'BLUESKY', label: 'Bluesky account' },
  { value: 'PODCAST', label: 'Podcast' },
]);

/** The variables of a feed message (feedService.FEED_VARS): common ones, then the type-specific ones. */
export const FEED_PLACEHOLDERS = Object.freeze([
  '{title}', '{url}', '{link}', '{author}', '{feed}', '{youtube_channel}', '{subreddit}', '{bluesky_name}',
  '{bluesky_handle}', '{podcast_title}', '{episode_title}',
]);

/** The message option's limit (feedService.parseMessageOption). */
export const FEED_MESSAGE_MAX = 1000;

export const collection = Object.freeze({
  key: 'feeds',
  module: 'feeds',
  label: 'Social feeds',
  singular: 'feed',
  description: 'Post new RSS items, YouTube uploads, Reddit posts, Bluesky posts and podcast episodes in a channel. '
    + 'Each feed is checked every 5 to 30 minutes; only items published after it was added are posted.',
  limit: LIMITS.FEEDS,
  titleField: 'displayName',
  subtitleField: 'statusText',
  fields: Object.freeze([
    { field: 'type', type: 'enum', label: 'Kind of feed', options: FEED_TYPE_OPTIONS, default: 'RSS', required: true },
    {
      field: 'source',
      type: 'text',
      label: 'Source',
      description: 'RSS: the feed URL or a site that links to its feed. YouTube: the UC… channel ID or a youtube.com/channel/UC… '
        + 'link (@handles are not supported). Reddit: the subreddit, e.g. r/aww. Bluesky: a handle, profile link or DID. '
        + 'Podcast: its name to search for, or its RSS feed URL. It cannot be changed later.',
      maxLength: 500,
      minLength: 1,
      required: true,
    },
    {
      field: 'channelId',
      type: 'channel',
      label: 'Channel',
      description: 'Where new items are posted.',
      channelTypes: [...POSTABLE_CHANNEL_KINDS],
      required: true,
    },
    { field: 'pingRoleId', type: 'role', label: 'Role to ping', description: 'Mentioned with every new item.', nullable: true, default: null },
    {
      field: 'messageTemplate',
      type: 'text',
      label: 'Message',
      description: 'Empty: the default message for this kind of feed. Variables: {title} {link} {author} {feed}, plus '
        + '{youtube_channel} (YouTube), {subreddit} (Reddit), {bluesky_name} {bluesky_handle} (Bluesky), '
        + '{podcast_title} {episode_title} (podcasts). Feeds have no member, so {user} variables do not work.',
      multiline: true,
      maxLength: FEED_MESSAGE_MAX,
      nullable: true,
      placeholders: [...FEED_PLACEHOLDERS],
      default: null,
    },
    { field: 'includeReposts', type: 'boolean', label: 'Include reposts', description: 'Bluesky only: also post reposts.', default: false },
    {
      field: 'enabled',
      type: 'boolean',
      label: 'Enabled',
      description: 'Off: paused, the settings are kept. Turning it back on resets its failures; items published while it was off are skipped.',
      default: true,
    },
  ]),
  createOnly: Object.freeze(['type', 'source']),
  readOnly: Object.freeze([
    { field: 'displayName', type: 'text', label: 'Name', maxLength: 100, default: '' },
    { field: 'statusText', type: 'text', label: 'Status', maxLength: 300, default: '' },
    { field: 'sourceUrl', type: 'url', label: 'Source link', nullable: true, default: null },
    { field: 'lastCheckedAt', type: 'datetime', label: 'Last checked', nullable: true, default: null },
    { field: 'lastPostedAt', type: 'datetime', label: 'Last post', nullable: true, default: null },
    { field: 'createdAt', type: 'datetime', label: 'Added', nullable: true, default: null },
  ]),
  actions: Object.freeze([
    {
      id: 'test',
      label: 'Check now',
      description: 'Fetches the feed once and shows its newest item. Nothing is posted or changed.',
      fields: [],
    },
  ]),
  check: checkFeed,
});

const PODCAST_URL_RE = /^(https?:\/\/|feed:)|^[\w-]+(\.[\w-]+)+\/\S*/i; // the /feed add podcast rule

/**
 * Parses a source as /feed add does (readAddSource): what resolveFeedSource takes, or why not.
 * @param {string} type
 * @param {string} raw
 * @returns {{ ok: true, input: object } | { ok: false, message: string }}
 */
export function parseFeedSource(type, raw) {
  const plain = (s) => String(s ?? '').replace(/\*\*/g, '');
  switch (type) {
    case 'RSS': {
      const r = normalizeFeedUrl(raw);
      return r.ok ? { ok: true, input: { url: r.url } } : { ok: false, message: plain(r.reason) };
    }
    case 'YOUTUBE': {
      const r = parseYouTubeChannelInput(raw);
      return r.ok ? { ok: true, input: { channelId: r.channelId } } : { ok: false, message: plain(r.reason) };
    }
    case 'REDDIT': {
      const r = parseSubredditInput(raw);
      return r.ok ? { ok: true, input: { name: r.name } } : { ok: false, message: plain(r.reason) };
    }
    case 'BLUESKY': {
      const r = parseBlueskyActorInput(raw);
      return r.ok ? { ok: true, input: { actor: r.actor } } : { ok: false, message: plain(r.reason) };
    }
    case 'PODCAST': {
      const s = String(raw ?? '').trim();
      if (PODCAST_URL_RE.test(s)) {
        const r = normalizeFeedUrl(s);
        return r.ok ? { ok: true, input: { url: r.url } } : { ok: false, message: plain(r.reason) };
      }
      if (s.length < 2) return { ok: false, message: "Type at least 2 characters of the podcast's name, or paste its RSS feed URL." };
      return { ok: true, input: { query: s } };
    }
    default:
      return { ok: false, message: 'That kind of feed is not supported.' };
  }
}

/**
 * The source must parse for its type (on create), and only Bluesky feeds include reposts.
 * @returns {null|{ path: string, message: string }}
 */
export function checkFeed(data, { mode, current } = {}) {
  const f = mode === 'update' && current ? { ...current, ...data } : data;
  if (mode === 'create') {
    const r = parseFeedSource(f.type, f.source);
    if (!r.ok) return { path: 'source', message: r.message };
  }
  if (f.includeReposts === true && f.type !== 'BLUESKY') return { path: 'includeReposts', message: 'Only Bluesky feeds can include reposts.' };
  return null;
}

const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);

/**
 * A Feed row as a collection item.
 * @param {object} row
 * @param {{ statusText: string, sourceUrl: string|null }} extra feedStatus / sourceLink of feedService
 */
export function feedItem(row, { statusText = '', sourceUrl = null } = {}) {
  return {
    id: row.id,
    type: row.type,
    source: row.source,
    channelId: row.channelId,
    pingRoleId: row.pingRoleId ?? null,
    messageTemplate: row.messageTemplate ?? null,
    includeReposts: row.includeReposts === true,
    enabled: row.enabled !== false,
    displayName: row.displayName,
    statusText,
    sourceUrl,
    lastCheckedAt: iso(row.lastCheckedAt),
    lastPostedAt: iso(row.lastPostedAt),
    createdAt: iso(row.createdAt),
  };
}
