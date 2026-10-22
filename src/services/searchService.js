// ── Search Anything (pkg-search) ───────────────────────────────────────────────
// /search anime and /search manga (AniList GraphQL), /search pokemon (PokeAPI) and /search urban (Urban Dictionary). Every API
// is keyless. Everything with logic lives here; commands/search/search.js only declares the builders and routes.
//
// Outbound traffic is deliberately polite. Every request goes through safeFetch / safeFetchJson (SSRF guard, 8 s
// timeout, size cap) and passes, in this order:
//   Redis cache (projected objects only) -> single-flight per query -> per-source pause (429 / Retry-After /
//   X-RateLimit-Remaining) -> 3 s per-user fetch cooldown -> per-source requests-per-minute budget -> the request.
// Cache hits and single-flight followers consume nothing. Errors are never cached. Autocomplete never touches the network.
//
// Redis is fail-open everywhere: `cache` never throws, a miss falls through to the network, a failed write is ignored, and
// the cooldown / shared pause / budget fall back to the in-process copies. Nothing here deletes keys.
//
// Third-party text (synopses, definitions, titles) is stripped of HTML, markdown-escaped and defanged (mentions, Discord
// tokens), then clamped, before it is cached or rendered. Replies carry only embeds, so nothing can ping.
import { createHash } from 'node:crypto';
import { escapeMarkdown } from 'discord.js';
import { cache } from './redis.js';
import { getSettingsSafe, peekSettings, isModuleEnabled } from './settingsService.js';
import { safeFetch, safeFetchJson, SafeFetchError } from '../utils/safeFetch.js';
import { guildEmbed, warnEmbed, errorEmbed } from '../utils/embedBuilder.js';
import { clampText, joinWithLimit, formatNumber, progressBar } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import { safeDefer, safeReply, replyError, respondAutocomplete, isInteractionGone } from '../utils/interactionHelpers.js';
import { paginate } from '../utils/paginator.js';
import { createThrottle, withTimeout, ackBudgetMs } from '../utils/async.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const SEARCH_TIMEOUT_MS = 8_000;
export const MAX_RESULTS = 5;
export const FETCH_COOLDOWN_SECONDS = 3;
export const CACHE_TTL = Object.freeze({ anilist: 6 * 3600, urban: 6 * 3600, pokeapi: 7 * 24 * 3600, negative: 3600 });
export const MAX_BYTES = Object.freeze({ anilist: 512 * 1024, urban: 1024 * 1024, pokeapi: 3 * 1024 * 1024, pokeIndex: 1024 * 1024 });
export const ANILIST_URL = 'https://graphql.anilist.co';
export const POKEAPI_BASE = 'https://pokeapi.co/api/v2';
export const URBAN_DEFINE_URL = 'https://api.urbandictionary.com/v0/define';
export const ANILIST_PAUSE_KEY = 'search:anilist:pause';
export const POKEAPI_INDEX_KEY = 'search:pokeapi:index';
/** Lookups (not requests) that may go to the network per minute, per source, across every shard. */
export const BUDGET_PER_MINUTE = Object.freeze({ anilist: 24, urban: 30, pokeapi: 30 });
export const STAT_ORDER = Object.freeze(['hp', 'attack', 'defense', 'special-attack', 'special-defense', 'speed']);
export const STAT_LABELS = Object.freeze({
  hp: 'HP', attack: 'Attack', defense: 'Defense', 'special-attack': 'Sp. Atk', 'special-defense': 'Sp. Def', speed: 'Speed',
});
export const FORMAT_LABELS = Object.freeze({
  TV: 'TV', TV_SHORT: 'TV Short', MOVIE: 'Movie', SPECIAL: 'Special', OVA: 'OVA', ONA: 'ONA', MUSIC: 'Music',
  MANGA: 'Manga', NOVEL: 'Light Novel', ONE_SHOT: 'One Shot',
});
export const SEASON_LABELS = Object.freeze({ WINTER: 'Winter', SPRING: 'Spring', SUMMER: 'Summer', FALL: 'Fall' });

const ZWSP = '​';
const PAUSE_MAX_MS = 300_000;
const INDEX_MEMO_MS = 24 * 3600 * 1000;
const INDEX_RETRY_MS = 10 * 60 * 1000;

export const ANILIST_QUERY = `query ($search: String, $type: MediaType, $perPage: Int, $isAdult: Boolean) {
  Page(page: 1, perPage: $perPage) {
    media(search: $search, type: $type, isAdult: $isAdult, sort: [SEARCH_MATCH, POPULARITY_DESC]) {
      id siteUrl isAdult type format status episodes duration chapters volumes
      season seasonYear averageScore meanScore genres
      title { romaji english native }
      description(asHtml: false)
      coverImage { extraLarge large }
      startDate { year } endDate { year }
      nextAiringEpisode { airingAt episode }
      studios(isMain: true) { nodes { name } }
    }
  }
}`;

export class SearchError extends Error {
  /** code: 'RATE_LIMITED' | 'COOLDOWN' | 'TIMEOUT' | 'UNAVAILABLE' */
  constructor(code, { api = null, status = null, retryAt = null } = {}) {
    super(`${api ?? 'search'}: ${code}`);
    this.name = 'SearchError';
    this.code = code;
    this.api = api;
    this.status = status;
    this.retryAt = retryAt;
  }
}

// ── Test seam ─────────────────────────────────────────────────────────────────
// The only way to run this module offline under plain `node --test` (no module mocking). Production code never calls it.

const realDeps = Object.freeze({ cache, safeFetch, safeFetchJson, peekSettings, getSettingsSafe });
const deps = { ...realDeps };

/** Tests only: replaces any of cache, safeFetch, safeFetchJson, peekSettings, getSettingsSafe. */
export function __setDeps(overrides = {}) {
  Object.assign(deps, overrides);
}

/** Tests only: restores the real dependencies and clears every piece of in-process state. */
export function __resetState() {
  Object.assign(deps, realDeps);
  inflight.clear();
  pausedUntil.clear();
  localBudgets.clear();
  indexMemo = null;
  indexRefresh = null;
  indexFailedAt = 0;
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/** Single-line, NFKC, control/format characters and runs of whitespace collapsed, at most 100 code points. */
export function cleanQuery(input) {
  const s = String(input ?? '')
    .normalize('NFKC')
    .replace(/\p{C}+/gu, ' ')
    .replace(/\s+/gu, ' ')
    .trim();
  return Array.from(s).slice(0, 100).join('');
}

export function keyForm(input) {
  return cleanQuery(input).toLowerCase();
}

export function cacheKey(api, source) {
  return `search:${api}:${createHash('sha1').update(String(source), 'utf8').digest('hex')}`;
}

export function readEnvelope(value) {
  return value && typeof value === 'object' && value.v === 1 && Array.isArray(value.results) ? value : null;
}

/** Age-restricted means the channel (or, for a thread, its parent) is flagged NSFW. Anything unknown is not. */
export function isAgeRestrictedChannel(channel) {
  if (!channel) return false;
  if (typeof channel.isThread === 'function' && channel.isThread()) return channel.parent?.nsfw === true;
  return channel.nsfw === true;
}

export function pokemonSlug(input) {
  let s = String(input ?? '').normalize('NFKC').trim().toLowerCase();
  const num = /^#?\s*(\d{1,5})$/.exec(s);
  if (num) {
    const id = Number.parseInt(num[1], 10);
    return id >= 1 ? { kind: 'id', id } : null;
  }
  s = s.normalize('NFD').replace(/\p{M}+/gu, '') // flabébé -> flabebe
    .replace(/♀/g, '-f').replace(/♂/g, '-m') // nidoran♀ -> nidoran-f
    .replace(/['’‘`.]/g, '') // farfetch'd -> farfetchd, mr. mime -> mr mime
    .replace(/[^a-z0-9-]+/g, '-') // spaces, ':' and the rest -> '-'
    .replace(/-{2,}/g, '-').replace(/^-+|-+$/g, '');
  if (!s || s.length > 40) return null;
  return { kind: 'name', slug: s };
}

export function titleCase(slug) {
  return String(slug ?? '').split('-').filter(Boolean).map((w) => w[0].toUpperCase() + w.slice(1)).join(' ');
}

/** The URL when it is https, credential-free, at most 2048 characters and (optionally) on an allowed host; else null. */
export function httpsUrl(u, { hosts = null } = {}) {
  try {
    if (typeof u !== 'string' || u.length === 0 || u.length > 2048) return null;
    const url = new URL(u);
    if (url.protocol !== 'https:') return null;
    if (url.username || url.password) return null;
    if (hosts && !hosts.includes(url.hostname)) return null;
    return u;
  } catch {
    return null;
  }
}

const NAMED_ENTITIES = Object.freeze({
  amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ', mdash: '—', ndash: '–', hellip: '…',
  rsquo: '’', lsquo: '‘', rdquo: '”', ldquo: '“',
});

/**
 * Neutralises text that Discord would turn into a mention or another rendered token (@everyone, <@id>, <#id>, <:emoji:id>,
 * <t:…>, </command>) by inserting a zero-width space. Embeds never ping; this keeps third-party text from posing as one.
 */
export function defangMentions(text) {
  return String(text ?? '')
    .replace(/@(everyone|here)\b/gi, `@${ZWSP}$1`)
    .replace(/<(?=@|#|:|a:|t:|id:|\/)/gi, `<${ZWSP}`);
}

/** Markdown-escapes a block of third-party text: no bold, links, headings, spoilers, quotes or subtext can form. */
function escapeBlock(text) {
  return escapeMarkdown(text, { maskedLink: true, heading: true }).replace(/^([ \t]*)(>|-#)/gm, '$1\\$2');
}

const countSpoilerMarks = (s) => (s.match(/\|\|/g) ?? []).length;

/** AniList description (HTML-ish, with ~!spoiler!~) to safe Discord markdown, at most `max` characters. */
export function stripAniListHtml(text, max = 1000) {
  // Only the first `max` characters survive, so a longer source is cut first: several passes below (and discord.js's masked-link
  // escape) are quadratic on a long run of spaces, `[` or `~!`, and the API allows a body of up to 512 KB.
  let s = String(text ?? '').slice(0, max * 10).replace(/\r\n?/g, '\n');
  s = s.replace(/<br\s*\/?>/gi, '\n');
  s = s.replace(/<[^<>]{0,200}>/g, '');
  s = s.replace(/&(#x[0-9a-f]{1,6}|#\d{1,7}|[a-z]{2,8});/gi, (whole, body) => {
    if (body[0] === '#') {
      const n = body[1].toLowerCase() === 'x' ? Number.parseInt(body.slice(2), 16) : Number.parseInt(body.slice(1), 10);
      if (!(n > 0 && n <= 0x10ffff) || (n >= 0xd800 && n <= 0xdfff)) return '';
      return String.fromCodePoint(n);
    }
    const key = body.toLowerCase();
    return Object.hasOwn(NAMED_ENTITIES, key) ? NAMED_ENTITIES[key] : whole;
  });
  s = defangMentions(s);
  const parts = s.split(/~!([\s\S]*?)!~/);
  s = parts
    .map((seg, i) => (i % 2 === 1 ? (seg.trim() ? `||${escapeBlock(seg)}||` : '') : escapeBlock(seg)))
    .join('');
  s = s.replace(/[ \t]+\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
  s = clampText(s, max - 2);
  if (countSpoilerMarks(s) % 2 === 1) s += '||';
  return s;
}

export function stripUrbanBrackets(text) {
  return String(text ?? '').replace(/\[([^[\]\n]{1,100})\]/g, (_, inner) => inner);
}

export function cleanUrbanText(text, max) {
  let s = String(text ?? '').slice(0, max * 8).replace(/\r\n?/g, '\n'); // same bound as stripAniListHtml
  s = stripUrbanBrackets(s);
  s = defangMentions(s);
  s = escapeBlock(s);
  s = s.replace(/\n{3,}/g, '\n\n').trim();
  return clampText(s, max);
}

export function cleanFlavorText(text) {
  return String(text ?? '')
    .replace(/­\n/g, '')
    .replace(/­/g, '')
    .replace(/[\f\n\r\t\v]+/g, ' ')
    .replace(/ {2,}/g, ' ')
    .replace(/POKéMON/g, 'Pokémon')
    .replace(/POKé BALL/g, 'Poké Ball')
    .trim();
}

/** Seconds (1-300) from a Retry-After value (delta seconds or an HTTP date), or null. */
export function parseRetryAfter(value, now = Date.now()) {
  if (value === undefined || value === null) return null;
  const s = String(value);
  let secs;
  if (/^\s*\d{1,6}\s*$/.test(s)) {
    secs = Number.parseInt(s, 10);
  } else {
    const t = Date.parse(s);
    if (!Number.isFinite(t)) return null;
    secs = Math.ceil((t - now) / 1000);
  }
  return Math.min(300, Math.max(1, secs));
}

/** Header lookup that ignores case; an array value yields its first element. */
function header(h, name) {
  if (!h || typeof h !== 'object') return undefined;
  let v = h[name];
  if (v === undefined) {
    const k = Object.keys(h).find((key) => key.toLowerCase() === name);
    v = k === undefined ? undefined : h[k];
  }
  return Array.isArray(v) ? v[0] : v;
}

function resetSecondsFrom(reset, now) {
  if (!Number.isFinite(reset) || reset * 1000 <= now) return null;
  return Math.min(300, Math.max(1, Math.ceil(reset - now / 1000)));
}

/**
 * Classifies an AniList HTTP response. `pauseUntil` (epoch ms) is set whenever the next request would be refused:
 * on 429 (or a GraphQL 429), and softly when this response used the last request of the window.
 * @returns {{ kind: 'rate-limited', pauseUntil: number } | { kind: 'error', status: number, pauseUntil: number|null }
 *   | { kind: 'ok', media: any[], pauseUntil: number|null }}
 */
export function interpretAniListResponse({ status, headers, body }, now = Date.now()) {
  const remaining = Number.parseInt(header(headers, 'x-ratelimit-remaining'), 10);
  const reset = Number.parseInt(header(headers, 'x-ratelimit-reset'), 10);
  const gqlStatus = Array.isArray(body?.errors) ? body.errors.map((e) => e?.status).find(Number.isInteger) : undefined;

  if (status === 429 || gqlStatus === 429) {
    const retry = parseRetryAfter(header(headers, 'retry-after'), now) ?? resetSecondsFrom(reset, now) ?? 60;
    return { kind: 'rate-limited', pauseUntil: now + retry * 1000 };
  }
  const pauseUntil = remaining === 0 ? now + (resetSecondsFrom(reset, now) ?? 60) * 1000 : null;
  if (status < 200 || status >= 300) return { kind: 'error', status, pauseUntil };
  if (Array.isArray(body?.errors) && body.errors.length > 0) return { kind: 'error', status, pauseUntil };
  const media = body?.data?.Page?.media;
  if (!Array.isArray(media)) return { kind: 'error', status, pauseUntil };
  return { kind: 'ok', media, pauseUntil };
}

const oneLine = (s) => String(s ?? '').replace(/[\p{C}\s]+/gu, ' ').trim();
const nonNegInt = (x) => (Number.isInteger(x) && x >= 0 ? x : null);
const stringOrNull = (x) => (typeof x === 'string' && x ? x : null);
const labelOf = (map, key) => (typeof key === 'string' && Object.hasOwn(map, key) ? map[key] : null);

/** Raw AniList `media` array to at most MAX_RESULTS projected, cache-safe objects (adult titles dropped unless allowed). */
export function projectAniListResults(media, { type, allowAdult = false } = {}) {
  const out = [];
  if (!Array.isArray(media)) return out;
  for (const m of media) {
    if (out.length >= MAX_RESULTS) break;
    if (!m || typeof m !== 'object' || !Number.isInteger(m.id)) continue;
    if (!allowAdult && m.isAdult === true) continue; // the second, client-side check
    const title = clampText(oneLine(m.title?.english) || oneLine(m.title?.romaji) || oneLine(m.title?.native) || 'Untitled', 256);
    const seen = new Set([title.toLowerCase()]);
    const altTitles = [];
    for (const alt of [m.title?.romaji, m.title?.native]) {
      const a = oneLine(alt);
      if (!a || seen.has(a.toLowerCase())) continue;
      seen.add(a.toLowerCase());
      altTitles.push(clampText(a, 120));
    }
    const score = m.averageScore ?? m.meanScore;
    const nextEp = m.nextAiringEpisode;
    out.push({
      id: m.id,
      type,
      url: httpsUrl(m.siteUrl, { hosts: ['anilist.co'] }),
      title,
      altTitles,
      format: stringOrNull(m.format),
      status: stringOrNull(m.status),
      episodes: nonNegInt(m.episodes),
      duration: nonNegInt(m.duration),
      chapters: nonNegInt(m.chapters),
      volumes: nonNegInt(m.volumes),
      season: stringOrNull(m.season),
      seasonYear: nonNegInt(m.seasonYear),
      startYear: nonNegInt(m.startDate?.year),
      endYear: nonNegInt(m.endDate?.year),
      score: Number.isInteger(score) && score >= 0 && score <= 100 ? score : null,
      genres: (Array.isArray(m.genres) ? m.genres : []).filter((g) => typeof g === 'string' && g).map((g) => clampText(oneLine(g), 40)).slice(0, 20),
      studios: (Array.isArray(m.studios?.nodes) ? m.studios.nodes : []).map((n) => n?.name).filter((n) => typeof n === 'string' && n)
        .map((n) => clampText(oneLine(n), 80)).slice(0, 5),
      isAdult: m.isAdult === true,
      synopsis: stripAniListHtml(m.description, 1000),
      cover: httpsUrl(m.coverImage?.extraLarge) ?? httpsUrl(m.coverImage?.large),
      nextEpisode: Number.isInteger(nextEp?.episode) && nextEp.episode > 0 && Number.isInteger(nextEp?.airingAt) && nextEp.airingAt > 0
        ? { episode: nextEp.episode, airingAt: nextEp.airingAt }
        : null,
    });
  }
  return out;
}

const englishEntry = (list, textKey) => (Array.isArray(list) ? list : []).filter((e) => e?.language?.name === 'en' && typeof e?.[textKey] === 'string');
const SLUG_RE = /^[a-z0-9-]{1,60}$/;

/** PokeAPI `pokemon` + `pokemon-species` payloads (species may be null) to a small cache-safe object. Never throws. */
export function projectPokemon(pokemon, species) {
  const p = pokemon && typeof pokemon === 'object' ? pokemon : {};
  const sp = species && typeof species === 'object' ? species : null;
  const slug = String(p.name ?? '');
  const speciesName = typeof sp?.name === 'string' ? sp.name : '';

  const base = englishEntry(sp?.names, 'name')[0]?.name || titleCase(speciesName || slug) || 'Unknown';
  const isForm = Boolean(sp && speciesName && slug !== speciesName && slug.startsWith(`${speciesName}-`));
  const name = clampText(isForm ? `${base} (${titleCase(slug.slice(speciesName.length + 1))})` : base, 200);

  const bySlot = (a, b) => (Number(a?.slot) || 0) - (Number(b?.slot) || 0);
  const types = (Array.isArray(p.types) ? [...p.types] : []).sort(bySlot)
    .map((t) => t?.type?.name).filter((n) => typeof n === 'string' && n).map(titleCase);
  const abilities = (Array.isArray(p.abilities) ? [...p.abilities] : []).sort(bySlot)
    .filter((a) => typeof a?.ability?.name === 'string' && a.ability.name)
    .map((a) => ({ name: titleCase(a.ability.name), hidden: a.is_hidden === true }));
  const statList = Array.isArray(p.stats) ? p.stats : [];
  const stats = STAT_ORDER.map((key) => {
    const v = statList.find((s) => s?.stat?.name === key)?.base_stat;
    return { key, value: Number.isInteger(v) && v >= 0 ? v : 0 };
  });

  const flavorRaw = englishEntry(sp?.flavor_text_entries, 'flavor_text').at(-1)?.flavor_text;
  const flavor = flavorRaw === undefined ? '' : clampText(cleanFlavorText(flavorRaw), 600);
  const generation = /^generation-([a-z0-9]+)$/.exec(String(sp?.generation?.name ?? ''))?.[1];

  return {
    id: Number.isInteger(p.id) ? p.id : 0,
    dex: Number.isInteger(sp?.id) ? sp.id : (Number.isInteger(p.id) ? p.id : 0),
    name,
    slug,
    genus: englishEntry(sp?.genera, 'genus')[0]?.genus ?? null,
    flavor: flavor || null,
    types,
    abilities,
    stats,
    height: nonNegInt(p.height) ?? 0,
    weight: nonNegInt(p.weight) ?? 0,
    sprite: httpsUrl(p.sprites?.other?.['official-artwork']?.front_default)
      ?? httpsUrl(p.sprites?.other?.home?.front_default)
      ?? httpsUrl(p.sprites?.front_default),
    generation: generation ? `Generation ${generation.toUpperCase()}` : null,
    legendary: sp?.is_legendary === true,
    mythical: sp?.is_mythical === true,
  };
}

/** Urban Dictionary `list` to at most MAX_RESULTS cleaned definitions, in the API's order. */
export function projectUrban(list) {
  const out = [];
  if (!Array.isArray(list)) return out;
  for (const d of list) {
    if (out.length >= MAX_RESULTS) break;
    if (!d || typeof d !== 'object' || typeof d.definition !== 'string' || !d.definition.trim()) continue;
    const written = typeof d.written_on === 'string' ? Date.parse(d.written_on) : Number.NaN;
    out.push({
      word: clampText(cleanQuery(d.word) || 'Untitled', 256),
      definition: cleanUrbanText(d.definition, 1800),
      example: cleanUrbanText(d.example, 1000),
      author: clampText(cleanQuery(d.author), 64),
      url: httpsUrl(d.permalink, { hosts: ['www.urbandictionary.com', 'urbandictionary.com'] }),
      thumbsUp: nonNegInt(d.thumbs_up) ?? 0,
      thumbsDown: nonNegInt(d.thumbs_down) ?? 0,
      writtenOn: Number.isFinite(written) ? new Date(written).toISOString() : null,
    });
  }
  return out;
}

/** Autocomplete choices from the species index: prefix matches first, then substring matches, each in Pokédex order. */
export function matchPokemonIndex(entries, query, limit = 25) {
  if (!Array.isArray(entries)) return [];
  const q = String(query ?? '').normalize('NFKC').trim().toLowerCase().replace(/^#/, '')
    .normalize('NFD').replace(/\p{M}+/gu, '')
    .replace(/♀/g, '-f').replace(/♂/g, '-m')
    .replace(/['’‘`.]/g, '')
    .replace(/[^a-z0-9-]+/g, '-')
    .replace(/-{2,}/g, '-').replace(/^-+|-+$/g, '');
  let picked;
  if (q === '') {
    picked = entries;
  } else if (/^\d+$/.test(q)) {
    picked = entries.filter(([, id]) => String(id).startsWith(q));
  } else {
    picked = [
      ...entries.filter(([name]) => name.startsWith(q)),
      ...entries.filter(([name]) => !name.startsWith(q) && name.includes(q)),
    ];
  }
  return picked.slice(0, limit).map(([name, id]) => ({ name: `${titleCase(name)} (#${id})`, value: name }));
}

// ── Embeds ────────────────────────────────────────────────────────────────────

const esc = (s) => escapeMarkdown(String(s ?? ''));

function statusLabel(type, status) {
  switch (status) {
    case 'FINISHED': return 'Finished';
    case 'RELEASING': return type === 'MANGA' ? 'Publishing' : 'Airing';
    case 'NOT_YET_RELEASED': return type === 'MANGA' ? 'Not yet published' : 'Not yet aired';
    case 'CANCELLED': return 'Cancelled';
    case 'HIATUS': return 'On hiatus';
    default: return 'Unknown';
  }
}

function yearRange(m) {
  if (!m.startYear) return '—';
  if (m.endYear && m.endYear !== m.startYear) return `${m.startYear}–${m.endYear}`;
  if (m.status === 'RELEASING') return `${m.startYear}–present`;
  return `${m.startYear}`;
}

/** One embed per AniList result (at most MAX_RESULTS). */
export function buildAniListEmbeds(results, { type, settings = null, now = Date.now() } = {}) {
  return (Array.isArray(results) ? results : []).slice(0, MAX_RESULTS).map((m) => {
    const isManga = (m.type ?? type) === 'MANGA';
    const alt = m.altTitles?.length ? `*${escapeMarkdown(m.altTitles.join(' · '))}*` : '';
    const description = clampText([alt, m.synopsis || '*No synopsis available.*'].filter(Boolean).join('\n\n'), 4096);
    const footer = `Source: AniList${m.isAdult ? ' · 🔞 18+' : ''}`;
    const e = guildEmbed(settings ?? {}, esc(m.title), description, footer);
    if (m.url) e.setURL(m.url);
    if (m.cover) e.setThumbnail(m.cover);

    const dash = (v) => (v ? clampText(v, 1024) : '—');
    const fields = [{ name: '⭐ Score', value: m.score === null || m.score === undefined ? '—' : `${m.score}%`, inline: true }];
    if (isManga) {
      fields.push(
        { name: '📖 Format', value: dash(labelOf(FORMAT_LABELS, m.format)), inline: true },
        { name: '📡 Status', value: statusLabel('MANGA', m.status), inline: true },
        { name: '📄 Chapters', value: `${m.chapters ?? '?'}`, inline: true },
        { name: '📚 Volumes', value: `${m.volumes ?? '?'}`, inline: true },
        { name: '🗓️ Published', value: yearRange(m), inline: true },
      );
    } else {
      let episodes = `${m.episodes ?? '?'}${m.duration ? ` × ${m.duration} min` : ''}`;
      if (m.nextEpisode && m.nextEpisode.airingAt * 1000 > now) {
        episodes += `\nEp ${m.nextEpisode.episode} ${discordTimestamp(new Date(m.nextEpisode.airingAt * 1000), 'R')}`;
      }
      const seasonLabel = labelOf(SEASON_LABELS, m.season);
      fields.push(
        { name: '📺 Format', value: dash(labelOf(FORMAT_LABELS, m.format)), inline: true },
        { name: '📡 Status', value: statusLabel('ANIME', m.status), inline: true },
        { name: '🎞️ Episodes', value: clampText(episodes, 1024), inline: true },
        { name: '🗓️ Season', value: seasonLabel && m.seasonYear ? `${seasonLabel} ${m.seasonYear}` : yearRange(m), inline: true },
        { name: '🏢 Studio', value: dash(joinWithLimit((m.studios ?? []).map(esc), { max: 1024 })), inline: true },
      );
    }
    fields.push({ name: '🏷️ Genres', value: dash(joinWithLimit((m.genres ?? []).map(esc), { max: 1024 })), inline: false });
    return e.addFields(fields);
  });
}

export function buildPokemonEmbed(info, settings = null) {
  const description = [info.genus ? `*${esc(info.genus)}*` : null, info.flavor ? esc(info.flavor) : null].filter(Boolean).join('\n\n')
    || '*No Pokédex entry available.*';
  const footer = ['Source: PokeAPI', info.generation, info.legendary ? 'Legendary' : info.mythical ? 'Mythical' : null].filter(Boolean).join(' · ');
  const e = guildEmbed(settings ?? {}, clampText(`${esc(info.name)} · #${info.dex}`, 256), description, footer);
  if (info.sprite) e.setThumbnail(info.sprite);

  const stats = Array.isArray(info.stats) ? info.stats : [];
  const sum = stats.reduce((n, s) => n + (Number(s.value) || 0), 0);
  const block = `\`\`\`\n${stats.map(({ key, value }) => (
    `${(STAT_LABELS[key] ?? key).padEnd(7)} ${String(value).padStart(3)} ${progressBar(Math.min(1, value / 255), 10)}`
  )).join('\n')}\n${'Total'.padEnd(7)} ${String(sum).padStart(3)}\n\`\`\``;
  return e.addFields(
    { name: 'Type', value: clampText(info.types?.join(' / ') || '—', 1024), inline: true },
    {
      name: 'Abilities',
      value: clampText((info.abilities ?? []).map((a) => (a.hidden ? `${a.name} *(hidden)*` : a.name)).join(', ') || '—', 1024),
      inline: true,
    },
    { name: 'Height', value: `${((info.height || 0) / 10).toFixed(1)} m`, inline: true },
    { name: 'Weight', value: `${((info.weight || 0) / 10).toFixed(1)} kg`, inline: true },
    { name: 'Base stats', value: block, inline: false },
  );
}

/** One embed per definition (at most MAX_RESULTS). */
export function buildUrbanEmbeds(results, { settings = null } = {}) {
  return (Array.isArray(results) ? results : []).slice(0, MAX_RESULTS).map((d) => {
    const e = guildEmbed(settings ?? {}, esc(d.word), d.definition, `Urban Dictionary · by ${d.author || 'anonymous'}`);
    if (d.url) e.setURL(d.url);
    if (d.example) e.addFields({ name: 'Example', value: clampText(d.example, 1024), inline: false });
    if (d.thumbsUp + d.thumbsDown > 0) {
      e.addFields({ name: 'Votes', value: `👍 ${formatNumber(d.thumbsUp)} · 👎 ${formatNumber(d.thumbsDown)}`, inline: true });
    }
    if (d.writtenOn) e.setTimestamp(new Date(d.writtenOn));
    return e;
  });
}

export function noResultsEmbed(kind, query, { allowAdult = true } = {}) {
  const q = escapeMarkdown(defangMentions(clampText(cleanQuery(query), 100)));
  switch (kind) {
    case 'pokemon':
      return warnEmbed('No Results', `No Pokémon found for **${q}**. Check the spelling or try its Pokédex number.`);
    case 'urban':
      return warnEmbed('No Results', `Urban Dictionary has no definition for **${q}**.`);
    default: {
      const hint = allowAdult ? '' : '\nAdult titles are only shown in age-restricted channels.';
      return warnEmbed('No Results', `No ${kind === 'manga' ? 'manga' : 'anime'} found for **${q}**.${hint}`);
    }
  }
}

export function searchErrorEmbed(err, label) {
  if (err instanceof SearchError) {
    switch (err.code) {
      case 'RATE_LIMITED': {
        const when = Number.isFinite(err.retryAt) ? discordTimestamp(new Date(err.retryAt), 'R') : 'in a minute';
        return warnEmbed('Slow Down', `${label} is limiting how often we can search. Try again ${when}.`);
      }
      case 'COOLDOWN':
        return warnEmbed('Slow Down', 'You are searching too fast. Wait a few seconds and try again.');
      case 'TIMEOUT':
        return errorEmbed('Search Timed Out', `${label} did not answer within 8 seconds. Please try again later.`);
      case 'UNAVAILABLE':
        return errorEmbed('Search Unavailable', `${label} is not responding right now${Number.isInteger(err.status) ? ` (HTTP ${err.status})` : ''}. Please try again later.`);
      default:
        break;
    }
  }
  return errorEmbed('Search Failed', 'Something went wrong while searching. Please try again later.');
}

// ── Pause, budget, cooldown, cache (internal) ─────────────────────────────────

const inflight = new Map(); // cacheKey -> Promise<results>
const warnThrottle = createThrottle(60_000); // one warning per api per minute
const pausedUntil = new Map(); // api -> epoch ms (per shard)
const localBudgets = new Map(); // api -> { bucket, n } (used only while Redis is unavailable)

const pauseKey = (api) => (api === 'anilist' ? ANILIST_PAUSE_KEY : `search:${api}:pause`);

/** Throws RATE_LIMITED while the source is paused. Shared pause: fail-open (Redis down -> only the in-process pause applies). */
async function assertNotPaused(api, now = Date.now()) {
  const local = pausedUntil.get(api) ?? 0;
  if (local > now) throw new SearchError('RATE_LIMITED', { api, retryAt: local });
  const shared = await deps.cache.get(pauseKey(api));
  if (Number.isFinite(shared) && shared > now && shared <= now + PAUSE_MAX_MS) {
    pausedUntil.set(api, shared);
    throw new SearchError('RATE_LIMITED', { api, retryAt: shared });
  }
}

function setPause(api, untilMs) {
  const clamped = Math.min(untilMs, Date.now() + PAUSE_MAX_MS);
  if (!(clamped > (pausedUntil.get(api) ?? 0))) return;
  pausedUntil.set(api, clamped);
  const ttl = Math.max(1, Math.ceil((clamped - Date.now()) / 1000));
  Promise.resolve(deps.cache.set(pauseKey(api), clamped, ttl)).catch(() => {}); // fail-open; cache never rejects
}

/** Per-source requests-per-minute budget, shared through Redis. Fail-open to an in-process counter. */
async function consumeBudget(api, now = Date.now()) {
  const limit = BUDGET_PER_MINUTE[api];
  if (!limit) return;
  const bucket = Math.floor(now / 60_000);
  let n = await deps.cache.incr(`search:budget:${api}:${bucket}`, 120);
  if (n === null || n === undefined) {
    const cur = localBudgets.get(api);
    n = cur && cur.bucket === bucket ? cur.n + 1 : 1;
    localBudgets.set(api, { bucket, n });
  }
  if (n > limit) throw new SearchError('RATE_LIMITED', { api, retryAt: (bucket + 1) * 60_000 });
}

/** Maps anything thrown by a fetch to a SearchError. A 429 also pauses the source (Retry-After when sent, else 60 s). */
function toSearchError(err, api) {
  if (err instanceof SearchError) return err;
  if (err instanceof SafeFetchError) {
    if (err.code === 'TIMEOUT') return new SearchError('TIMEOUT', { api });
    if (err.code === 'HTTP_ERROR') {
      if (err.status === 429) {
        const wait = Number.isFinite(err.retryAfterMs) ? Math.min(PAUSE_MAX_MS, Math.max(1000, err.retryAfterMs)) : 60_000;
        const retryAt = Date.now() + wait;
        setPause(api, retryAt);
        return new SearchError('RATE_LIMITED', { api, status: 429, retryAt });
      }
      // A 2xx here means "not valid JSON", which is not worth showing as an HTTP status.
      return new SearchError('UNAVAILABLE', { api, status: Number.isInteger(err.status) && err.status >= 400 ? err.status : null });
    }
    return new SearchError('UNAVAILABLE', { api });
  }
  logger.error('search: unexpected fetch error', err);
  return new SearchError('UNAVAILABLE', { api });
}

/**
 * Cache -> single-flight -> pause -> user cooldown -> budget -> fetch. Redis is fail-open at every step.
 * Errors are never cached; empty results are cached for an hour.
 */
async function cachedLookup({ api, source, ttl, userId, fetcher }) {
  const key = cacheKey(api, source);
  const hit = readEnvelope(await deps.cache.get(key)); // fail-open: null (miss or Redis down) -> fetch
  if (hit) return { results: hit.results, cached: true };
  const running = inflight.get(key);
  if (running) return { results: await running, cached: false };

  await assertNotPaused(api);
  if (userId) {
    const ok = await deps.cache.setNX(`search:cd:${userId}`, 1, FETCH_COOLDOWN_SECONDS); // fail-open: null -> proceed
    if (ok === false) throw new SearchError('COOLDOWN', { api });
  }
  await consumeBudget(api);

  // The awaits above let another user start the same lookup; join it instead of fetching twice. From here to
  // inflight.set() nothing yields, so exactly one flight exists per key.
  const raced = inflight.get(key);
  if (raced) return { results: await raced, cached: false };
  const p = (async () => {
    const results = await fetcher();
    await deps.cache.set(key, { v: 1, results }, results.length ? ttl : CACHE_TTL.negative); // false on failure: ignored
    return results;
  })();
  inflight.set(key, p);
  try {
    return { results: await p, cached: false };
  } finally {
    if (inflight.get(key) === p) inflight.delete(key);
  }
}

// ── AniList ───────────────────────────────────────────────────────────────────

/**
 * @param {'ANIME'|'MANGA'} type
 * @param {string} query
 * @param {{ allowAdult?: boolean, userId?: string|null }} [opts]
 * @returns {Promise<{ results: object[], cached: boolean }>}
 * @throws {SearchError}
 */
export async function searchAniList(type, query, { allowAdult = false, userId = null } = {}) {
  if (type !== 'ANIME' && type !== 'MANGA') throw new TypeError(`searchAniList: bad type ${type}`);
  const q = cleanQuery(query);
  if (!q) return { results: [], cached: false };
  const variables = { search: q, type, perPage: MAX_RESULTS };
  if (!allowAdult) variables.isAdult = false; // age-restricted channels omit the key entirely, never null

  return cachedLookup({
    api: 'anilist',
    source: `${type.toLowerCase()}|${allowAdult ? 'nsfw' : 'sfw'}|${keyForm(q)}`,
    ttl: CACHE_TTL.anilist,
    userId,
    async fetcher() {
      let res;
      try {
        res = await deps.safeFetch(ANILIST_URL, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
          body: JSON.stringify({ query: ANILIST_QUERY, variables }),
          timeoutMs: SEARCH_TIMEOUT_MS,
          maxBytes: MAX_BYTES.anilist,
        });
      } catch (err) {
        throw toSearchError(err, 'anilist');
      }
      let body = null;
      try { body = res.json(); } catch { body = null; }
      const v = interpretAniListResponse({ status: res.status, headers: res.headers, body }, Date.now());
      if (v.pauseUntil) setPause('anilist', v.pauseUntil);
      if (v.kind === 'rate-limited') throw new SearchError('RATE_LIMITED', { api: 'anilist', status: 429, retryAt: v.pauseUntil });
      // A GraphQL error arrives with HTTP 200; "(HTTP 200)" would only confuse people.
      if (v.kind === 'error') throw new SearchError('UNAVAILABLE', { api: 'anilist', status: v.status >= 400 ? v.status : null });
      return projectAniListResults(v.media, { type, allowAdult });
    },
  });
}

// ── PokeAPI ───────────────────────────────────────────────────────────────────

/** The parsed JSON, or null on a 404. Throws SearchError. */
async function pokeGet(path) {
  try {
    return await deps.safeFetchJson(`${POKEAPI_BASE}${path}`, {
      timeoutMs: SEARCH_TIMEOUT_MS,
      maxBytes: MAX_BYTES.pokeapi,
      headers: { Accept: 'application/json' },
    });
  } catch (err) {
    if (err instanceof SafeFetchError && err.code === 'HTTP_ERROR' && err.status === 404) return null;
    throw toSearchError(err, 'pokeapi');
  }
}

/**
 * @param {{ kind: 'id', id: number } | { kind: 'name', slug: string }} parsed - from pokemonSlug
 * @param {{ userId?: string|null }} [opts]
 * @returns {Promise<{ results: object[], cached: boolean }>} results is [] when nothing matches
 * @throws {SearchError}
 */
export async function getPokemon(parsed, { userId = null } = {}) {
  const x = parsed.kind === 'id' ? String(parsed.id) : parsed.slug;
  return cachedLookup({
    api: 'pokeapi',
    source: parsed.kind === 'id' ? `pokemon|#${parsed.id}` : `pokemon|${parsed.slug}`,
    ttl: CACHE_TTL.pokeapi,
    userId,
    async fetcher() {
      let species = await pokeGet(`/pokemon-species/${encodeURIComponent(x)}`);
      let pokemon = null;
      if (species) {
        // Species whose default form has a suffix (deoxys -> deoxys-normal): URLs are built from names, never followed.
        const defName = species.varieties?.find?.((v) => v?.is_default)?.pokemon?.name;
        if (typeof defName === 'string' && SLUG_RE.test(defName)) pokemon = await pokeGet(`/pokemon/${defName}`);
      } else {
        pokemon = await pokeGet(`/pokemon/${encodeURIComponent(x)}`); // forms and form ids: charizard-mega-x, 10034
        const speciesName = pokemon?.species?.name;
        if (typeof speciesName === 'string' && SLUG_RE.test(speciesName)) species = await pokeGet(`/pokemon-species/${speciesName}`);
      }
      return pokemon ? [projectPokemon(pokemon, species)] : [];
    },
  });
}

// ── Pokémon name index (autocomplete only) ────────────────────────────────────

let indexMemo = null; // { at, entries }
let indexRefresh = null; // Promise while a refresh runs (single-flight)
let indexFailedAt = 0;

async function refreshPokemonIndex() {
  try {
    await assertNotPaused('pokeapi');
    let body;
    try {
      body = await deps.safeFetchJson(`${POKEAPI_BASE}/pokemon-species?limit=2000`, {
        timeoutMs: SEARCH_TIMEOUT_MS,
        maxBytes: MAX_BYTES.pokeIndex,
      });
    } catch (err) {
      throw toSearchError(err, 'pokeapi'); // a 429 pauses PokeAPI for every lookup
    }
    const entries = [];
    for (const r of Array.isArray(body?.results) ? body.results : []) {
      const id = Number(/\/pokemon-species\/(\d+)\/?$/.exec(String(r?.url ?? ''))?.[1]);
      if (typeof r?.name === 'string' && /^[a-z0-9-]{1,40}$/.test(r.name) && Number.isInteger(id) && id > 0) entries.push([r.name, id]);
    }
    entries.sort((a, b) => a[1] - b[1]);
    if (entries.length === 0) throw new Error('empty Pokémon index');
    indexMemo = { at: Date.now(), entries };
    await deps.cache.set(POKEAPI_INDEX_KEY, { v: 1, entries }, CACHE_TTL.pokeapi); // fail-open: the memo still serves
  } catch (err) {
    indexFailedAt = Date.now();
    if (warnThrottle('pokeindex')) logger.warn('search: Pokémon index refresh failed', err);
    throw err;
  }
}

/** The species index, or null when it is not loaded yet. Never throws and never waits for the network. */
export async function getPokemonIndex() {
  try {
    const now = Date.now();
    if (indexMemo && now - indexMemo.at < INDEX_MEMO_MS) return indexMemo.entries;
    const v = await deps.cache.get(POKEAPI_INDEX_KEY); // fail-open: null -> refresh in the background
    if (v?.v === 1 && Array.isArray(v.entries) && v.entries.length > 0) {
      indexMemo = { at: now, entries: v.entries };
      return v.entries;
    }
    if (!indexRefresh && now - indexFailedAt > INDEX_RETRY_MS) {
      indexRefresh = refreshPokemonIndex().catch(() => {}).finally(() => { indexRefresh = null; });
    }
    return indexMemo?.entries ?? null;
  } catch (err) {
    logger.debug(`search: getPokemonIndex failed: ${err?.message}`);
    return null;
  }
}

// ── Urban Dictionary ──────────────────────────────────────────────────────────

/**
 * @param {string} term
 * @param {{ userId?: string|null }} [opts]
 * @returns {Promise<{ results: object[], cached: boolean }>}
 * @throws {SearchError}
 */
export async function defineUrban(term, { userId = null } = {}) {
  const t = cleanQuery(term);
  if (!t) return { results: [], cached: false };
  return cachedLookup({
    api: 'urban',
    source: `define|${keyForm(t)}`,
    ttl: CACHE_TTL.urban,
    userId,
    async fetcher() {
      let body;
      try {
        body = await deps.safeFetchJson(`${URBAN_DEFINE_URL}?term=${encodeURIComponent(t)}`, {
          timeoutMs: SEARCH_TIMEOUT_MS,
          maxBytes: MAX_BYTES.urban,
          headers: { Accept: 'application/json' },
        });
      } catch (err) {
        throw toSearchError(err, 'urban');
      }
      if (!Array.isArray(body?.list)) throw new SearchError('UNAVAILABLE', { api: 'urban' });
      return projectUrban(body.list);
    },
  });
}

// ── Command flows ─────────────────────────────────────────────────────────────

/** Removes the public "thinking..." placeholder and posts the failure privately. */
async function replyFailure(interaction, embed) {
  try {
    await interaction.deleteReply();
    await interaction.followUp({ embeds: [embed], ephemeral: true, allowedMentions: { parse: [] } });
  } catch (err) {
    if (isInteractionGone(err)) return;
    logger.warn('search: could not send ephemeral failure, editing the reply instead', err);
    await safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
  }
}

async function sendPages(interaction, pages) {
  try {
    await paginate(interaction, pages, { timeout: 120_000, footer: 'append' }); // one page: no buttons
  } catch (err) {
    logger.error('search: paginate failed', err);
  }
}

async function failWith(interaction, err, { api, label, command }) {
  if (err instanceof SearchError) {
    if (err.code !== 'COOLDOWN' && warnThrottle(api)) {
      logger.warn(`search: ${api} ${err.code}${err.status ? ` (HTTP ${err.status})` : ''}`);
    }
  } else {
    logger.error(`search: /search ${command} failed`, err);
  }
  await replyFailure(interaction, searchErrorEmbed(err, label));
}

/** Settings before the defer: the router's memo hit, else bounded by the acknowledgement budget. null when unavailable. */
async function settingsBeforeAck(interaction) {
  const guildId = interaction.guildId;
  if (!guildId) return null;
  const hit = deps.peekSettings(guildId);
  if (hit) return hit;
  try {
    return await withTimeout(deps.getSettingsSafe(guildId), ackBudgetMs(interaction), 'search settings');
  } catch {
    return null;
  }
}

/** /search anime and /search manga. */
export async function executeAniList(interaction, type) {
  const command = type === 'MANGA' ? 'manga' : 'anime';
  const query = cleanQuery(interaction.options.getString('query', true));
  if (!query) {
    await replyError(interaction, 'Invalid Search', 'Type a title to search for.');
    return;
  }
  const settings = await settingsBeforeAck(interaction);
  const allowAdult = isAgeRestrictedChannel(interaction.channel);
  if (!(await safeDefer(interaction))) return;

  let results;
  try {
    ({ results } = await searchAniList(type, query, { allowAdult, userId: interaction.user.id }));
  } catch (err) {
    await failWith(interaction, err, { api: 'anilist', label: 'AniList', command });
    return;
  }
  if (results.length === 0) {
    await replyFailure(interaction, noResultsEmbed(command, query, { allowAdult }));
    return;
  }
  let pages;
  try {
    pages = buildAniListEmbeds(results, { type, settings, now: Date.now() });
  } catch (err) {
    await failWith(interaction, err, { api: 'anilist', label: 'AniList', command });
    return;
  }
  await sendPages(interaction, pages);
}

/** /search pokemon. */
export async function executePokemon(interaction) {
  const raw = interaction.options.getString('name', true);
  const parsed = pokemonSlug(raw);
  if (!parsed) {
    await replyError(interaction, 'Invalid Pokémon', 'Type a Pokémon name (for example `pikachu`) or a Pokédex number (for example `25`).');
    return;
  }
  const settings = await settingsBeforeAck(interaction);
  if (!(await safeDefer(interaction))) return;

  let results;
  try {
    ({ results } = await getPokemon(parsed, { userId: interaction.user.id }));
  } catch (err) {
    await failWith(interaction, err, { api: 'pokeapi', label: 'PokeAPI', command: 'pokemon' });
    return;
  }
  if (results.length === 0) {
    await replyFailure(interaction, noResultsEmbed('pokemon', raw));
    return;
  }
  let embed;
  try {
    embed = buildPokemonEmbed(results[0], settings);
  } catch (err) {
    await failWith(interaction, err, { api: 'pokeapi', label: 'PokeAPI', command: 'pokemon' });
    return;
  }
  await safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });
}

export async function autocompletePokemon(interaction) {
  try {
    const focused = String(interaction.options.getFocused() ?? '');
    const s = interaction.guildId ? deps.peekSettings(interaction.guildId) : null;
    if (s && !isModuleEnabled(s, 'search')) {
      await respondAutocomplete(interaction, []);
      return;
    }
    const entries = await getPokemonIndex();
    await respondAutocomplete(interaction, entries ? matchPokemonIndex(entries, focused, 25) : []);
  } catch (err) {
    logger.debug(`search: pokemon autocomplete failed: ${err?.message}`);
    await respondAutocomplete(interaction, []);
  }
}

/** /search urban. Only age-restricted channels unless the guild turned urbanNsfwOnly off (fail closed on anything unknown). */
export async function executeUrban(interaction) {
  const term = cleanQuery(interaction.options.getString('term', true));
  if (!term) {
    await replyError(interaction, 'Invalid Search', 'Type a word or phrase to define.');
    return;
  }
  const settings = await settingsBeforeAck(interaction);
  const nsfwOnly = settings?.urbanNsfwOnly !== false;
  if (nsfwOnly && !isAgeRestrictedChannel(interaction.channel)) {
    await replyError(
      interaction,
      'Age-Restricted Channel Required',
      'Urban Dictionary definitions can only be shown in age-restricted channels on this server.',
    );
    return;
  }
  if (!(await safeDefer(interaction))) return;

  let results;
  try {
    ({ results } = await defineUrban(term, { userId: interaction.user.id }));
  } catch (err) {
    await failWith(interaction, err, { api: 'urban', label: 'Urban Dictionary', command: 'urban' });
    return;
  }
  if (results.length === 0) {
    await replyFailure(interaction, noResultsEmbed('urban', term));
    return;
  }
  let pages;
  try {
    pages = buildUrbanEmbeds(results, { settings });
  } catch (err) {
    await failWith(interaction, err, { api: 'urban', label: 'Urban Dictionary', command: 'urban' });
    return;
  }
  await sendPages(interaction, pages);
}
