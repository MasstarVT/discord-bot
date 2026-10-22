// pkg-search tests (pkg-search.md §8), offline. No case touches the network, Redis or Postgres: the service's test seam
// (__setDeps) swaps in an in-memory cache, a fake safeFetch / safeFetchJson and fake settings, and fake interactions drive
// the command flows end to end (validation, NSFW gate, defer-before-fetch, ephemeral failures, pagination).
import { test, after, afterEach, mock } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL ??= 'postgresql://test:test@127.0.0.1:1/test';
// Nothing listens here; the client retries in the background until after().
process.env.REDIS_URL = 'redis://127.0.0.1:1';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const svc = await import('../src/services/searchService.js');
const { SearchError } = svc;
const { SafeFetchError } = await import('../src/utils/safeFetch.js');
const { PERMISSION_LEVELS, COLORS } = await import('../src/config/constants.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { default: redis } = await import('../src/services/redis.js');
const searchCmd = await import('../src/commands/search/search.js');
// /anime, /manga, /pokemon and /urban are `/search` subcommands now. These shims run the merged execute / autocomplete with
// the subcommand the old command became, so the call sites below read as before (the fakes carry no getSubcommand).
const asSub = (name) => ({
  execute: (i) => { i.options.getSubcommand = () => name; return searchCmd.execute(i); },
  autocomplete: (i) => { i.options.getSubcommand = () => name; return searchCmd.autocomplete(i); },
});
const cmds = Object.fromEntries(['anime', 'manga', 'pokemon', 'urban'].map((n) => [n, asSub(n)]));

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

after(() => {
  Object.assign(console, quiet);
  mock.timers.reset();
  svc.__resetState();
  redis.disconnect();
});
afterEach(() => {
  mock.timers.reset();
  svc.__resetState();
});

// ── Helpers ───────────────────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

/** In-memory stand-in for the `cache` API. `down` makes it behave like an unreachable Redis (every call falls back). */
function makeCache({ down = false, seed = {} } = {}) {
  const store = new Map(Object.entries(seed));
  const sets = [];
  return {
    store,
    sets,
    async get(k) { return down || !store.has(k) ? null : structuredClone(store.get(k)); },
    async set(k, v, ttl) {
      if (down) return false;
      store.set(k, structuredClone(v));
      sets.push({ k, v: structuredClone(v), ttl });
      return true;
    },
    async setNX(k, v) {
      if (down) return null;
      if (store.has(k)) return false;
      store.set(k, v);
      return true;
    },
    async incr(k) {
      if (down) return null;
      const n = (store.get(k) ?? 0) + 1;
      store.set(k, n);
      return n;
    },
  };
}

const res = ({ status = 200, headers = {}, body = null } = {}) => ({
  ok: status >= 200 && status < 300,
  status,
  headers,
  json() {
    if (body === null) throw new SyntaxError('not json');
    return structuredClone(body);
  },
});

/** safeFetch fake for AniList: handler(n, call) returns a res() or an Error to throw. */
function aniFetch(handler) {
  const calls = [];
  const fn = async (url, opts) => {
    const call = { url, opts, body: JSON.parse(opts.body) };
    calls.push(call);
    const r = await handler(calls.length, call);
    if (r instanceof Error) throw r;
    return r;
  };
  fn.calls = calls;
  return fn;
}

/** safeFetchJson fake: routes maps a URL (without the host) to a value, or to an Error to throw. */
function jsonFetch(routes) {
  const calls = [];
  const fn = async (url, opts) => {
    calls.push({ url, opts });
    const path = url.replace('https://pokeapi.co/api/v2', '').replace('https://api.urbandictionary.com', '');
    const hit = typeof routes === 'function' ? await routes(path, url) : routes[path];
    if (hit === undefined) throw new SafeFetchError('HTTP_ERROR', 404);
    if (hit instanceof Error) throw hit;
    return structuredClone(hit);
  };
  fn.calls = calls;
  return fn;
}

const NOW = 1_800_000_000_000;

// Fixtures (pkg-search.md §8 items 11, 12 and 14)
const FRIEREN = {
  id: 154587, siteUrl: 'https://anilist.co/anime/154587', isAdult: false, type: 'ANIME', format: 'TV',
  status: 'FINISHED', episodes: 28, duration: 24, chapters: null, volumes: null, season: 'FALL', seasonYear: 2023,
  averageScore: 91, meanScore: 91, genres: ['Adventure', 'Drama', 'Fantasy'],
  title: { romaji: 'Sousou no Frieren', english: 'Frieren: Beyond Journey’s End', native: '葬送のフリーレン' },
  description: 'The adventure is over but life goes on for an elf mage.\n<br><br>\n(Source: Crunchyroll)',
  coverImage: { extraLarge: 'https://s4.anilist.co/file/anilistcdn/media/anime/cover/large/bx154587-qQTzQnEJJ3oB.jpg', large: null },
  startDate: { year: 2023 }, endDate: { year: 2024 }, nextAiringEpisode: null, studios: { nodes: [{ name: 'MADHOUSE' }] },
};
const ADULT = { ...FRIEREN, id: 1, isAdult: true, title: { romaji: 'Adult Title', english: null, native: null } };
const BAD = { id: 'x' };

const PIKACHU = {
  id: 25, name: 'pikachu', height: 4, weight: 60, species: { name: 'pikachu' },
  types: [{ slot: 1, type: { name: 'electric' } }],
  abilities: [{ slot: 3, is_hidden: true, ability: { name: 'lightning-rod' } }, { slot: 1, is_hidden: false, ability: { name: 'static' } }],
  stats: [['hp', 35], ['attack', 55], ['defense', 40], ['special-attack', 50], ['special-defense', 50], ['speed', 90]]
    .map(([n, v]) => ({ base_stat: v, stat: { name: n } })),
  sprites: {
    front_default: 'https://raw.githubusercontent.com/PokeAPI/sprites/master/sprites/pokemon/25.png',
    other: { 'official-artwork': { front_default: 'https://raw.githubusercontent.com/PokeAPI/sprites/master/sprites/pokemon/other/official-artwork/25.png' } },
  },
};
const PIKACHU_SPECIES = {
  id: 25, name: 'pikachu', is_legendary: false, is_mythical: false, generation: { name: 'generation-i' },
  names: [{ name: 'Pikachu', language: { name: 'en' } }], genera: [{ genus: 'Mouse Pokémon', language: { name: 'en' } }],
  varieties: [{ is_default: true, pokemon: { name: 'pikachu', url: 'https://pokeapi.co/api/v2/pokemon/25/' } }],
  flavor_text_entries: [
    { flavor_text: 'When several of\nthese POKéMON\ngather.', language: { name: 'en' } },
    { flavor_text: 'Ignored', language: { name: 'ja' } },
    { flavor_text: 'Possesses cheek sacs in which it stores electricity.', language: { name: 'en' } },
  ],
};
const YEET = {
  word: 'Yeet', definition: 'To discard an [item] at a [high] [velocity]', example: '[Alex] yeets the can',
  author: 'Le anonymous ', permalink: 'https://www.urbandictionary.com/define.php?term=Yeet&defid=12339851',
  thumbs_up: 0, thumbs_down: 0, written_on: '2018-04-22T00:00:00.000Z', play_sound_url: 'https://cdn.example/x.mp3',
};

const aniBody = (media) => ({ status: 200, headers: { 'x-ratelimit-remaining': '29' }, body: { data: { Page: { media } } } });
const field = (embed, word) => embed.fields.find((f) => f.name.includes(word));

// ── 1. Payloads ───────────────────────────────────────────────────────────────

test('command payload: /search, its four subcommands and the single required option of each', () => {
  const expected = { anime: ['query', 100], manga: ['query', 100], pokemon: ['name', 40], urban: ['term', 100] };
  const mod = searchCmd;
  const json = mod.data.toJSON();
  assert.equal(json.name, 'search');
  assert.equal(mod.module, 'search');
  assert.equal(mod.gated, true);
  assert.equal(mod.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.ok(json.default_member_permissions === undefined || json.default_member_permissions === null);
  assert.deepEqual(buildCommandPayload(mod).contexts, [0]);
  assert.ok(charCount(json) <= 430, `search: ${charCount(json)} chars`);
  assert.equal(typeof mod.execute, 'function');
  assert.equal(typeof mod.autocomplete, 'function');
  assert.equal(mod.allowInDMs, undefined);
  assert.deepEqual(json.options.map((o) => o.name), Object.keys(expected));
  for (const sub of json.options) {
    const [optName, maxLen] = expected[sub.name];
    assert.equal(sub.type, 1);
    assert.equal(sub.options.length, 1);
    const o = sub.options[0];
    assert.equal(o.name, optName);
    assert.equal(o.type, 3);
    assert.equal(o.required, true);
    assert.equal(o.min_length, 1);
    assert.equal(o.max_length, maxLen);
    assert.equal(Boolean(o.autocomplete), sub.name === 'pokemon');
  }
});

test('/search autocomplete answers nothing outside /search pokemon', async () => {
  const i = fakeInteraction({ options: { focused: 'pi' } });
  i.options.getSubcommand = () => 'anime';
  await searchCmd.autocomplete(i);
  assert.deepEqual(sent(i, 'respond'), []);
});

// ── 2-3. cleanQuery, keyForm, cacheKey ────────────────────────────────────────

test('cleanQuery and keyForm', () => {
  assert.equal(svc.cleanQuery('  Frieren\u0000\n  beyond  '), 'Frieren beyond');
  assert.equal(svc.cleanQuery('ｆｒｉｅｒｅｎ'), 'frieren');
  assert.equal(svc.cleanQuery('x'.repeat(150)).length, 100);
  assert.equal(svc.cleanQuery(null), '');
  assert.equal(svc.cleanQuery('a​b'), 'a b');
  assert.equal(svc.keyForm('FRIEREN '), 'frieren');
});

test('cacheKey is a stable sha1 and the adult mode / type change it', () => {
  const k = svc.cacheKey('anilist', 'anime|sfw|frieren');
  assert.match(k, /^search:anilist:[0-9a-f]{40}$/);
  assert.equal(k, svc.cacheKey('anilist', 'anime|sfw|frieren'));
  const keys = new Set(['anime|sfw|frieren', 'anime|nsfw|frieren', 'manga|sfw|frieren'].map((s) => svc.cacheKey('anilist', s)));
  assert.equal(keys.size, 3);
});

// ── 4-5. pokemonSlug, isAgeRestrictedChannel ──────────────────────────────────

test('pokemonSlug', () => {
  const name = (slug) => ({ kind: 'name', slug });
  const cases = [
    ['Pikachu', name('pikachu')], ['Mr. Mime', name('mr-mime')], ['Mime Jr.', name('mime-jr')],
    ["Farfetch'd", name('farfetchd')], ['Farfetch’d', name('farfetchd')], ['Type: Null', name('type-null')],
    ['Nidoran♀', name('nidoran-f')], ['Nidoran♂', name('nidoran-m')], ['Flabébé', name('flabebe')],
    ['Ho-Oh', name('ho-oh')], ['porygon z', name('porygon-z')], ['Charizard Mega X', name('charizard-mega-x')],
    ['025', { kind: 'id', id: 25 }], ['#25', { kind: 'id', id: 25 }], ['# 25', { kind: 'id', id: 25 }],
    ['10034', { kind: 'id', id: 10034 }],
    ['0', null], ['!!!', null], ['', null], ['a'.repeat(41), null], [null, null],
  ];
  for (const [input, want] of cases) assert.deepEqual(svc.pokemonSlug(input), want, `input ${JSON.stringify(input)}`);
});

test('isAgeRestrictedChannel', () => {
  const f = svc.isAgeRestrictedChannel;
  assert.equal(f(null), false);
  assert.equal(f({ nsfw: true }), true);
  assert.equal(f({ nsfw: false }), false);
  assert.equal(f({}), false);
  assert.equal(f({ isThread: () => true, parent: { nsfw: true } }), true);
  assert.equal(f({ isThread: () => true, parent: { nsfw: false } }), false);
  assert.equal(f({ isThread: () => true, parent: null, nsfw: true }), false);
  assert.equal(f({ isThread: () => false, nsfw: true }), true);
});

// ── 6-8. Text cleaning ────────────────────────────────────────────────────────

test('stripAniListHtml', () => {
  const f = svc.stripAniListHtml;
  const br = f('A<br>B<br/>\n<br>C');
  assert.ok(br.includes('A\nB'));
  assert.ok(!br.includes('<'));
  assert.equal(f('<i>Notes</i> &amp; &quot;x&quot; &#039;y&#039; &mdash; &#x2014;'), 'Notes & "x" \'y\' — —');
  assert.equal(f('&amp;lt;b&amp;gt;'), '&lt;b&gt;'); // single pass
  assert.equal(f('Plot ~!Snape kills!~ end'), 'Plot ||Snape kills|| end');
  const cut = f(`${'x'.repeat(990)} ~!${'y'.repeat(50)}!~`, 1000);
  assert.ok(cut.length <= 1000);
  assert.equal((cut.match(/\|\|/g) ?? []).length % 2, 0);
  assert.equal(f('a\n\n\n\nb'), 'a\n\nb');
  assert.equal(f(null), '');
  const md = f('*bold* [x](https://e.com)');
  assert.ok(md.includes('\\*bold\\*'));
  assert.ok(md.includes('\\[x]'));
  assert.equal(f('a &#0;&#xD800;&#1114112;b &bogus;'), 'a b &bogus;');
});

test('stripAniListHtml neutralises mentions, markdown headings, quotes and spoiler marks', () => {
  const out = svc.stripAniListHtml('hi @everyone @here &lt;@123&gt; &lt;#9&gt;\n# Heading\n> quote\n-# sub\n||fake||');
  assert.ok(!out.includes('@everyone') && !out.includes('@here'));
  assert.ok(!out.includes('<@') && !out.includes('<#'));
  assert.ok(out.includes('\\# Heading'));
  assert.ok(out.includes('\\> quote'));
  assert.ok(out.includes('\\-# sub'));
  assert.ok(out.includes('\\|\\|fake\\|\\|'));
});

test('stripUrbanBrackets and cleanUrbanText', () => {
  assert.equal(svc.stripUrbanBrackets('To discard an [item] at a [high] [velocity]'), 'To discard an item at a high velocity');
  assert.ok(svc.cleanUrbanText('[[a]](https://evil.example)', 100).includes('\\['));
  assert.ok(svc.cleanUrbanText('x'.repeat(3000), 1800).length <= 1800);
  const h = svc.cleanUrbanText('# Big\n||spoil||', 100);
  assert.ok(h.startsWith('\\#'));
  assert.ok(h.includes('\\|\\|'));
  const m = svc.cleanUrbanText('@everyone @here <@123> <#9> <:e:1> <t:1:R>', 200);
  assert.ok(!m.includes('@everyone') && !m.includes('@here'));
  assert.ok(!/<[@#:t]/.test(m));
  assert.equal(svc.cleanUrbanText(null, 10), '');
});

test('cleanFlavorText', () => {
  assert.equal(svc.cleanFlavorText('When several of\nthese POKéMON\ngather, their\felectricity'), 'When several of these Pokémon gather, their electricity');
  assert.equal(svc.cleanFlavorText('pre­\nfix'), 'prefix');
  assert.equal(svc.cleanFlavorText('a POKé BALL'), 'a Poké Ball');
});

// ── 9-10. Rate-limit parsing ──────────────────────────────────────────────────

test('parseRetryAfter', () => {
  const now = Date.now();
  assert.equal(svc.parseRetryAfter('60'), 60);
  assert.equal(svc.parseRetryAfter('0'), 1);
  assert.equal(svc.parseRetryAfter('9999'), 300);
  const d = svc.parseRetryAfter(new Date(now + 30_000).toUTCString(), now);
  assert.ok(d >= 29 && d <= 31, String(d));
  assert.equal(svc.parseRetryAfter(new Date(now - 60_000).toUTCString(), now), 1);
  assert.equal(svc.parseRetryAfter('abc'), null);
  assert.equal(svc.parseRetryAfter(undefined), null);
  assert.equal(svc.parseRetryAfter(null), null);
});

test('interpretAniListResponse', () => {
  const f = (x) => svc.interpretAniListResponse(x, NOW);
  const empty = { data: { Page: { media: [] } } };
  let v = f({ status: 429, headers: { 'retry-after': '42' }, body: null });
  assert.equal(v.kind, 'rate-limited');
  assert.equal(v.pauseUntil, NOW + 42_000);
  v = f({ status: 200, headers: {}, body: { data: null, errors: [{ message: 'Too Many Requests.', status: 429 }] } });
  assert.equal(v.kind, 'rate-limited');
  assert.equal(v.pauseUntil, NOW + 60_000);
  v = f({ status: 429, headers: { 'x-ratelimit-reset': String(NOW / 1000 + 20) }, body: null });
  assert.equal(v.pauseUntil, NOW + 20_000);
  v = f({ status: 200, headers: { 'X-RateLimit-Remaining': '0' }, body: empty });
  assert.equal(v.kind, 'ok');
  assert.equal(v.pauseUntil, NOW + 60_000);
  v = f({ status: 200, headers: { 'x-ratelimit-remaining': '29' }, body: empty });
  assert.equal(v.kind, 'ok');
  assert.equal(v.pauseUntil, null);
  assert.equal(f({ status: 200, headers: {}, body: { ...empty, errors: [{ message: 'x' }] } }).kind, 'error');
  v = f({ status: 503, headers: {}, body: null });
  assert.equal(v.kind, 'error');
  assert.equal(v.status, 503);
  assert.equal(f({ status: 200, headers: {}, body: { data: {} } }).kind, 'error');
});

// ── 11. AniList projection and embeds ─────────────────────────────────────────

test('projectAniListResults', () => {
  const r = svc.projectAniListResults([ADULT, FRIEREN, BAD], { type: 'ANIME', allowAdult: false });
  assert.equal(r.length, 1);
  assert.equal(r[0].title, 'Frieren: Beyond Journey’s End');
  assert.deepEqual(r[0].altTitles, ['Sousou no Frieren', '葬送のフリーレン']);
  assert.equal(r[0].score, 91);
  assert.equal(r[0].startYear, 2023);
  assert.equal(r[0].endYear, 2024);
  assert.deepEqual(r[0].studios, ['MADHOUSE']);
  assert.ok(r[0].synopsis.includes('(Source: Crunchyroll)'));
  assert.ok(!r[0].synopsis.includes('<br'));
  assert.equal(r[0].url, FRIEREN.siteUrl);
  assert.equal(r[0].cover, FRIEREN.coverImage.extraLarge);

  const adult = svc.projectAniListResults([ADULT, FRIEREN, BAD], { type: 'ANIME', allowAdult: true });
  assert.equal(adult.length, 2);
  assert.equal(adult[0].isAdult, true);

  for (const siteUrl of ['http://anilist.co/x', 'https://evil.example/x']) {
    assert.equal(svc.projectAniListResults([{ ...FRIEREN, siteUrl }], { type: 'ANIME' })[0].url, null);
  }
  const seven = Array.from({ length: 7 }, (_, i) => ({ ...FRIEREN, id: 100 + i }));
  assert.equal(svc.projectAniListResults(seven, { type: 'ANIME' }).length, 5);
  assert.deepEqual(svc.projectAniListResults(null, { type: 'ANIME' }), []);
  // Cache-safe: a JSON round trip changes nothing.
  assert.deepEqual(JSON.parse(JSON.stringify(r)), r);
});

test('buildAniListEmbeds: anime', () => {
  const projected = svc.projectAniListResults([FRIEREN], { type: 'ANIME', allowAdult: false });
  const e = svc.buildAniListEmbeds(projected, { type: 'ANIME', settings: { embedColor: 0xff7a00 }, allowAdult: false, now: 0 })[0].toJSON();
  assert.equal(e.color, 0xff7a00);
  assert.equal(e.title, 'Frieren: Beyond Journey’s End');
  assert.equal(e.thumbnail.url, FRIEREN.coverImage.extraLarge);
  assert.equal(e.url, FRIEREN.siteUrl);
  assert.equal(e.fields.length, 7);
  assert.equal(field(e, 'Episodes').value, '28 × 24 min');
  assert.equal(field(e, 'Season').value, 'Fall 2023');
  assert.equal(field(e, 'Status').value, 'Finished');
  assert.equal(field(e, 'Score').value, '91%');
  assert.equal(field(e, 'Format').value, 'TV');
  assert.equal(field(e, 'Studio').value, 'MADHOUSE');
  assert.equal(field(e, 'Genres').value, 'Adventure, Drama, Fantasy');
  assert.equal(e.footer.text, 'Source: AniList');
  assert.ok(JSON.stringify(e).length < 6000);
  assert.equal(svc.buildAniListEmbeds(projected, { type: 'ANIME', settings: null, now: 0 })[0].toJSON().color, COLORS.INFO);
});

test('buildAniListEmbeds: manga, next episode, adult footer and worst-case size', () => {
  const manga = svc.projectAniListResults(
    [{ ...FRIEREN, type: 'MANGA', format: 'MANGA', status: 'RELEASING', chapters: null, volumes: null, endDate: { year: null } }],
    { type: 'MANGA', allowAdult: false },
  );
  const m = svc.buildAniListEmbeds(manga, { type: 'MANGA', settings: null, now: 0 })[0].toJSON();
  assert.equal(field(m, 'Status').value, 'Publishing');
  assert.equal(field(m, 'Chapters').value, '?');
  assert.equal(field(m, 'Published').value, '2023–present');
  assert.equal(field(m, 'Format').value, 'Manga');
  assert.equal(m.fields.length, 7);

  const airing = svc.projectAniListResults(
    [{ ...FRIEREN, status: 'RELEASING', nextAiringEpisode: { episode: 12, airingAt: NOW / 1000 + 3600 } }],
    { type: 'ANIME' },
  );
  const a = svc.buildAniListEmbeds(airing, { type: 'ANIME', now: NOW })[0].toJSON();
  assert.equal(field(a, 'Episodes').value, `28 × 24 min\nEp 12 <t:${NOW / 1000 + 3600}:R>`);
  const past = svc.buildAniListEmbeds(airing, { type: 'ANIME', now: NOW + 7_200_000 })[0].toJSON();
  assert.equal(field(past, 'Episodes').value, '28 × 24 min');

  const nsfw = svc.projectAniListResults([ADULT], { type: 'ANIME', allowAdult: true });
  assert.ok(svc.buildAniListEmbeds(nsfw, { type: 'ANIME', now: 0 })[0].toJSON().footer.text.includes('18+'));

  const huge = svc.projectAniListResults([{
    ...FRIEREN,
    title: { english: '*'.repeat(400), romaji: '_'.repeat(300), native: '~'.repeat(300) },
    description: '*'.repeat(5000),
    genres: Array.from({ length: 40 }, (_, i) => `Genre${i}${'g'.repeat(60)}`),
    studios: { nodes: Array.from({ length: 10 }, (_, i) => ({ name: `Studio ${i} ${'s'.repeat(120)}` })) },
  }], { type: 'ANIME' });
  const h = svc.buildAniListEmbeds(huge, { type: 'ANIME', now: 0 })[0].toJSON();
  assert.ok(JSON.stringify(h).length < 6000, String(JSON.stringify(h).length));
  assert.ok(h.title.length <= 256 && h.description.length <= 4096);
  for (const f of h.fields) assert.ok(f.value.length >= 1 && f.value.length <= 1024);
});

// ── 12. Pokémon projection and embed ──────────────────────────────────────────

test('projectPokemon and buildPokemonEmbed', () => {
  const p = svc.projectPokemon(PIKACHU, PIKACHU_SPECIES);
  assert.equal(p.name, 'Pikachu');
  assert.equal(p.dex, 25);
  assert.deepEqual(p.types, ['Electric']);
  assert.deepEqual(p.abilities, [{ name: 'Static', hidden: false }, { name: 'Lightning Rod', hidden: true }]);
  assert.equal(p.flavor, 'Possesses cheek sacs in which it stores electricity.');
  assert.equal(p.genus, 'Mouse Pokémon');
  assert.equal(p.generation, 'Generation I');
  assert.ok(p.sprite.endsWith('/official-artwork/25.png'));
  assert.deepEqual(JSON.parse(JSON.stringify(p)), p);

  const rock = svc.projectPokemon({ ...PIKACHU, name: 'pikachu-rock-star', id: 10080 }, PIKACHU_SPECIES);
  assert.equal(rock.name, 'Pikachu (Rock Star)');
  assert.equal(rock.dex, 25);

  const noSpecies = svc.projectPokemon(PIKACHU, null);
  assert.equal(noSpecies.name, 'Pikachu');
  assert.equal(noSpecies.flavor, null);
  assert.equal(noSpecies.genus, null);
  assert.deepEqual(svc.projectPokemon({ id: 1, name: 'x' }, null).types, []);
  assert.doesNotThrow(() => svc.projectPokemon(null, null));

  const e = svc.buildPokemonEmbed(p, null).toJSON();
  assert.equal(e.title, 'Pikachu · #25');
  assert.deepEqual(e.fields.map((f) => f.name), ['Type', 'Abilities', 'Height', 'Weight', 'Base stats']);
  assert.equal(field(e, 'Height').value, '0.4 m');
  assert.equal(field(e, 'Weight').value, '6.0 kg');
  assert.equal(field(e, 'Abilities').value, 'Static, Lightning Rod *(hidden)*');
  const stats = field(e, 'Base stats').value;
  assert.ok(stats.startsWith('```'));
  assert.ok(stats.includes('Total'));
  assert.ok(stats.includes('320'));
  assert.equal(e.footer.text, 'Source: PokeAPI · Generation I');
  assert.equal(e.thumbnail.url, p.sprite);
  assert.ok(e.description.includes('*Mouse Pokémon*'));

  const legendary = svc.projectPokemon(PIKACHU, { ...PIKACHU_SPECIES, is_legendary: true });
  assert.ok(svc.buildPokemonEmbed(legendary).toJSON().footer.text.endsWith('Legendary'));
  assert.equal(svc.buildPokemonEmbed(noSpecies).toJSON().description, '*No Pokédex entry available.*');
});

// ── 13. matchPokemonIndex ─────────────────────────────────────────────────────

test('matchPokemonIndex', () => {
  const entries = [['bulbasaur', 1], ['pikachu', 25], ['raichu', 26], ['mr-mime', 122], ['mr-rime', 866], ['pichu', 172]];
  const values = (q, limit) => svc.matchPokemonIndex(entries, q, limit).map((c) => c.value);
  assert.deepEqual(values('pi'), ['pikachu', 'pichu']);
  assert.deepEqual(values('chu'), ['pikachu', 'raichu', 'pichu']);
  assert.equal(values('Mr. M')[0], 'mr-mime');
  assert.deepEqual(values('#2').slice(0, 2), ['pikachu', 'raichu']);
  assert.deepEqual(values(''), entries.map(([n]) => n));
  assert.equal(svc.matchPokemonIndex(entries, 'mr-m')[0].name, 'Mr Mime (#122)');
  assert.equal(values('', 2).length, 2);
  assert.deepEqual(svc.matchPokemonIndex(null, 'x'), []);
});

// ── 14. Urban ─────────────────────────────────────────────────────────────────

test('projectUrban and buildUrbanEmbeds', () => {
  const r = svc.projectUrban([YEET, { definition: '   ' }, null]);
  assert.equal(r.length, 1);
  assert.equal(r[0].definition, 'To discard an item at a high velocity');
  assert.equal(r[0].example, 'Alex yeets the can');
  assert.equal(r[0].author, 'Le anonymous');
  assert.equal(r[0].url, YEET.permalink);
  assert.equal(r[0].writtenOn, '2018-04-22T00:00:00.000Z');
  assert.ok(!Object.keys(r[0]).some((k) => k.includes('sound')));
  assert.equal(svc.projectUrban([{ ...YEET, permalink: 'https://evil.example/x' }])[0].url, null);
  assert.equal(svc.projectUrban(Array.from({ length: 7 }, () => YEET)).length, 5);
  assert.deepEqual(svc.projectUrban('nope'), []);
  assert.equal(svc.projectUrban([{ ...YEET, written_on: 'garbage' }])[0].writtenOn, null);

  const e = svc.buildUrbanEmbeds(r, { settings: null })[0].toJSON();
  assert.equal(e.title, 'Yeet');
  assert.ok(field(e, 'Example'));
  assert.equal(field(e, 'Votes'), undefined);
  assert.ok(e.timestamp.startsWith('2018-04-22'));
  assert.equal(e.footer.text, 'Urban Dictionary · by Le anonymous');
  assert.equal(e.url, YEET.permalink);

  const voted = svc.buildUrbanEmbeds(svc.projectUrban([{ ...YEET, thumbs_up: 5 }]), { settings: null })[0].toJSON();
  assert.equal(field(voted, 'Votes').value, '👍 5 · 👎 0');

  const evil = svc.projectUrban([{ ...YEET, word: '**@everyone**', definition: 'x'.repeat(5000), example: 'y'.repeat(5000) }]);
  const big = svc.buildUrbanEmbeds(evil)[0].toJSON();
  assert.ok(JSON.stringify(big).length < 6000);
  assert.ok(big.title.startsWith('\\*\\*'));
  assert.ok(big.description.length <= 1800);
  assert.ok(field(big, 'Example').value.length <= 1000);
});

// ── 15-16. Error and no-result embeds ─────────────────────────────────────────

test('searchErrorEmbed', () => {
  const f = (err, label = 'AniList') => svc.searchErrorEmbed(err, label).toJSON();
  const rl = f(new SearchError('RATE_LIMITED', { api: 'anilist', retryAt: 1_800_000_060_000 }));
  assert.ok(rl.title.includes('Slow Down'));
  assert.ok(rl.description.includes('<t:1800000060:R>'));
  assert.ok(f(new SearchError('RATE_LIMITED', { api: 'anilist' })).description.includes('in a minute'));
  assert.ok(f(new SearchError('COOLDOWN')).description.includes('too fast'));
  const to = f(new SearchError('TIMEOUT'));
  assert.ok(to.title.includes('Search Timed Out'));
  assert.ok(to.description.includes('8 seconds'));
  assert.ok(f(new SearchError('UNAVAILABLE', { status: 503 })).description.includes('(HTTP 503)'));
  assert.ok(!f(new SearchError('UNAVAILABLE')).description.includes('HTTP'));
  assert.ok(f(new Error('x')).title.includes('Search Failed'));
});

test('noResultsEmbed', () => {
  const d = (...a) => svc.noResultsEmbed(...a).toJSON().description;
  assert.ok(d('anime', 'abc', { allowAdult: false }).includes('age-restricted'));
  assert.ok(!d('anime', 'abc', { allowAdult: true }).includes('age-restricted'));
  assert.ok(d('manga', 'abc').includes('No manga found'));
  assert.ok(d('urban', '**x**').includes('\\*\\*x\\*\\*'));
  assert.ok(d('pokemon', 'abc').includes('Pokédex number'));
  assert.ok(!d('urban', '@everyone').includes('@everyone'));
});

// ── Lookup behaviour: cache, filters, pauses, cooldown, budget ─────────────────

function setup({ cache = makeCache(), safeFetch, safeFetchJson, settings = null } = {}) {
  svc.__resetState();
  svc.__setDeps({
    cache,
    ...(safeFetch ? { safeFetch } : {}),
    ...(safeFetchJson ? { safeFetchJson } : {}),
    peekSettings: () => settings,
    getSettingsSafe: async () => settings,
  });
  return cache;
}

test('AniList request: safe fetch options, adult variable and the 6 h cache', async () => {
  const fetch = aniFetch(() => res(aniBody([FRIEREN])));
  const cache = setup({ safeFetch: fetch });

  const first = await svc.searchAniList('ANIME', '  Frieren ', { allowAdult: false });
  assert.equal(first.cached, false);
  assert.equal(first.results.length, 1);
  const call = fetch.calls[0];
  assert.equal(call.url, svc.ANILIST_URL);
  assert.equal(call.opts.method, 'POST');
  assert.equal(call.opts.timeoutMs, 8000);
  assert.equal(call.opts.maxBytes, 512 * 1024);
  assert.equal(call.body.query, svc.ANILIST_QUERY);
  assert.ok(svc.ANILIST_QUERY.includes('sort: [SEARCH_MATCH, POPULARITY_DESC]'));
  assert.deepEqual(call.body.variables, { search: 'Frieren', type: 'ANIME', perPage: 5, isAdult: false });
  const write = cache.sets.find((s) => s.k === svc.cacheKey('anilist', 'anime|sfw|frieren'));
  assert.equal(write.ttl, 21_600);
  assert.equal(write.v.v, 1);

  const second = await svc.searchAniList('ANIME', 'FRIEREN', { allowAdult: false });
  assert.equal(second.cached, true);
  assert.equal(fetch.calls.length, 1);

  // The adult mode is part of the key, and NSFW channels omit isAdult entirely (never null).
  const nsfw = await svc.searchAniList('ANIME', 'frieren', { allowAdult: true });
  assert.equal(nsfw.cached, false);
  assert.equal(fetch.calls.length, 2);
  assert.equal('isAdult' in fetch.calls[1].body.variables, false);
  assert.ok(cache.store.has(svc.cacheKey('anilist', 'anime|nsfw|frieren')));

  // MANGA is a separate key and type.
  await svc.searchAniList('MANGA', 'frieren');
  assert.equal(fetch.calls[2].body.variables.type, 'MANGA');
  await assert.rejects(() => svc.searchAniList('NOVEL', 'x'), TypeError);
  assert.deepEqual(await svc.searchAniList('ANIME', '   '), { results: [], cached: false });
  assert.equal(fetch.calls.length, 3);
});

test('AniList: adult titles in a server response are dropped again client-side; empty results cache for an hour', async () => {
  const fetch = aniFetch((n) => res(aniBody(n === 1 ? [ADULT, FRIEREN] : [])));
  const cache = setup({ safeFetch: fetch });
  const sfw = await svc.searchAniList('ANIME', 'mixed', { allowAdult: false });
  assert.deepEqual(sfw.results.map((r) => r.id), [FRIEREN.id]);
  const none = await svc.searchAniList('ANIME', 'nothing here', { allowAdult: false });
  assert.deepEqual(none.results, []);
  const write = cache.sets.find((s) => s.k === svc.cacheKey('anilist', 'anime|sfw|nothing here'));
  assert.equal(write.ttl, 3600);
  assert.equal((await svc.searchAniList('ANIME', 'nothing here', { allowAdult: false })).cached, true);
});

test('AniList 429: RATE_LIMITED, shared pause written, misses blocked, cache hits still served', async () => {
  const fetch = aniFetch((n) => (n === 1 ? res(aniBody([FRIEREN])) : res({ status: 429, headers: { 'retry-after': '42' }, body: null })));
  const cache = setup({ safeFetch: fetch });
  await svc.searchAniList('ANIME', 'primed'); // fetch #1 fills the cache

  const before = Date.now();
  const err = await svc.searchAniList('ANIME', 'other').then(() => null, (e) => e); // fetch #2 -> 429
  assert.ok(err instanceof SearchError);
  assert.equal(err.code, 'RATE_LIMITED');
  assert.ok(err.retryAt >= before + 41_000 && err.retryAt <= Date.now() + 43_000);
  const pause = cache.sets.find((s) => s.k === svc.ANILIST_PAUSE_KEY);
  assert.ok(pause && pause.ttl >= 41 && pause.ttl <= 43);

  const blocked = await svc.searchAniList('ANIME', 'third').then(() => null, (e) => e);
  assert.equal(blocked.code, 'RATE_LIMITED');
  assert.equal(fetch.calls.length, 2, 'a paused source is never contacted');
  assert.equal((await svc.searchAniList('ANIME', 'primed')).cached, true);
});

test('AniList: X-RateLimit-Remaining 0 returns the results and pauses the next miss', async () => {
  const fetch = aniFetch(() => res({ status: 200, headers: { 'X-RateLimit-Remaining': '0' }, body: { data: { Page: { media: [FRIEREN] } } } }));
  setup({ safeFetch: fetch });
  assert.equal((await svc.searchAniList('ANIME', 'last one')).results.length, 1);
  const err = await svc.searchAniList('ANIME', 'next one').then(() => null, (e) => e);
  assert.equal(err.code, 'RATE_LIMITED');
  assert.equal(fetch.calls.length, 1);
});

test('AniList: a pause stored in Redis by another shard is honoured', async () => {
  const fetch = aniFetch(() => res(aniBody([FRIEREN])));
  setup({ safeFetch: fetch, cache: makeCache({ seed: { [svc.ANILIST_PAUSE_KEY]: Date.now() + 30_000 } }) });
  const err = await svc.searchAniList('ANIME', 'x').then(() => null, (e) => e);
  assert.equal(err.code, 'RATE_LIMITED');
  assert.equal(fetch.calls.length, 0);
  // A pause key that is stale or absurdly far away is ignored.
  setup({ safeFetch: fetch, cache: makeCache({ seed: { [svc.ANILIST_PAUSE_KEY]: Date.now() + 10_000_000 } }) });
  assert.equal((await svc.searchAniList('ANIME', 'x')).results.length, 1);
});

test('AniList failures are mapped and never cached', async () => {
  const outcomes = [
    new SafeFetchError('TIMEOUT', 'slow'),
    res({ status: 500, body: null }),
    res({ status: 200, body: { errors: [{ message: 'boom' }], data: null } }),
    new SafeFetchError('NETWORK', 'down'),
    res(aniBody([FRIEREN])),
  ];
  const fetch = aniFetch((n) => outcomes[n - 1]);
  const cache = setup({ safeFetch: fetch });
  const codes = [];
  for (let i = 0; i < 4; i++) {
    const err = await svc.searchAniList('ANIME', 'flaky').then(() => null, (e) => e);
    codes.push([err.code, err.status]);
  }
  assert.deepEqual(codes, [['TIMEOUT', null], ['UNAVAILABLE', 500], ['UNAVAILABLE', null], ['UNAVAILABLE', null]]);
  assert.equal(cache.sets.length, 0, 'failures write nothing');
  assert.equal(cache.sets.some((s) => s.k === svc.cacheKey('anilist', 'anime|sfw|flaky')), false);
  const ok = await svc.searchAniList('ANIME', 'flaky'); // the same query succeeds once the API recovers
  assert.equal(ok.results.length, 1);
});

test('per-user fetch cooldown: a second fetch inside 3 s is refused, cache hits are not throttled', async () => {
  const fetch = aniFetch(() => res(aniBody([FRIEREN])));
  const cache = setup({ safeFetch: fetch });
  await svc.searchAniList('ANIME', 'one', { userId: 'u1' });
  assert.ok(cache.store.has('search:cd:u1'));
  const err = await svc.searchAniList('ANIME', 'two', { userId: 'u1' }).then(() => null, (e) => e);
  assert.equal(err.code, 'COOLDOWN');
  assert.equal(fetch.calls.length, 1);
  assert.equal((await svc.searchAniList('ANIME', 'one', { userId: 'u1' })).cached, true);
  assert.equal((await svc.searchAniList('ANIME', 'two', { userId: 'u2' })).cached, false);
});

test('single-flight: concurrent identical lookups share one request', async () => {
  const fetch = aniFetch(async () => {
    await new Promise((r) => setTimeout(r, 25));
    return res(aniBody([FRIEREN]));
  });
  setup({ safeFetch: fetch });
  const all = await Promise.all(['a', 'b', 'c'].map((u) => svc.searchAniList('ANIME', 'same query', { userId: u })));
  assert.equal(fetch.calls.length, 1);
  assert.ok(all.every((r) => r.results.length === 1));
});

test('per-source budget: the 25th network lookup in a minute is refused (Redis up and down)', async () => {
  for (const down of [false, true]) {
    mock.timers.enable({ apis: ['Date'], now: NOW });
    const fetch = aniFetch(() => res(aniBody([FRIEREN])));
    setup({ safeFetch: fetch, cache: makeCache({ down }) });
    const limit = svc.BUDGET_PER_MINUTE.anilist;
    for (let i = 0; i < limit; i++) await svc.searchAniList('ANIME', `q${i}`);
    const err = await svc.searchAniList('ANIME', 'one too many').then(() => null, (e) => e);
    assert.equal(err.code, 'RATE_LIMITED', `down=${down}`);
    assert.equal(fetch.calls.length, limit);
    assert.equal(err.retryAt, (Math.floor(NOW / 60_000) + 1) * 60_000);
    mock.timers.reset();
  }
});

test('Redis down: lookups still work (fail-open) and nothing throws', async () => {
  const fetch = aniFetch(() => res(aniBody([FRIEREN])));
  setup({ safeFetch: fetch, cache: makeCache({ down: true }) });
  assert.equal((await svc.searchAniList('ANIME', 'x', { userId: 'u1' })).results.length, 1);
  assert.equal((await svc.searchAniList('ANIME', 'x', { userId: 'u1' })).results.length, 1); // no cooldown, no cache
  assert.equal(fetch.calls.length, 2);
});

// ── PokeAPI lookup ────────────────────────────────────────────────────────────

test('PokeAPI: species first, then the default variety; cached for a week', async () => {
  const fetch = jsonFetch({ '/pokemon-species/pikachu': PIKACHU_SPECIES, '/pokemon/pikachu': PIKACHU });
  const cache = setup({ safeFetchJson: fetch });
  const r = await svc.getPokemon(svc.pokemonSlug('Pikachu'));
  assert.equal(r.results[0].name, 'Pikachu');
  assert.deepEqual(fetch.calls.map((c) => c.url), [
    'https://pokeapi.co/api/v2/pokemon-species/pikachu', 'https://pokeapi.co/api/v2/pokemon/pikachu',
  ]);
  assert.equal(fetch.calls[0].opts.timeoutMs, 8000);
  assert.equal(fetch.calls[0].opts.maxBytes, 3 * 1024 * 1024);
  assert.equal(cache.sets.at(-1).ttl, 604_800);
  assert.equal(cache.sets.at(-1).k, svc.cacheKey('pokeapi', 'pokemon|pikachu'));
  assert.equal((await svc.getPokemon({ kind: 'name', slug: 'pikachu' })).cached, true);
  assert.equal(fetch.calls.length, 2);
  // Only the projected object is cached.
  assert.equal(JSON.stringify(cache.sets.at(-1).v).length < 2048, true);
});

test('PokeAPI: URLs are built from names, never followed from the payload', async () => {
  const deoxysSpecies = {
    id: 386, name: 'deoxys', names: [{ name: 'Deoxys', language: { name: 'en' } }],
    varieties: [{ is_default: true, pokemon: { name: 'deoxys-normal', url: 'https://evil.example/steal' } }],
  };
  const deoxys = { id: 386, name: 'deoxys-normal', species: { name: 'deoxys', url: 'https://evil.example/s' }, types: [], abilities: [], stats: [] };
  const fetch = jsonFetch({ '/pokemon-species/deoxys': deoxysSpecies, '/pokemon/deoxys-normal': deoxys });
  setup({ safeFetchJson: fetch });
  const r = await svc.getPokemon({ kind: 'name', slug: 'deoxys' });
  assert.equal(r.results[0].name, 'Deoxys (Normal)');
  assert.equal(r.results[0].dex, 386);
  assert.ok(fetch.calls.every((c) => c.url.startsWith('https://pokeapi.co/api/v2/')));

  // A hostile default-variety name is not fetched at all.
  const evilSpecies = { ...deoxysSpecies, varieties: [{ is_default: true, pokemon: { name: '../../etc/passwd' } }] };
  const fetch2 = jsonFetch({ '/pokemon-species/evil': evilSpecies });
  setup({ safeFetchJson: fetch2 });
  assert.deepEqual((await svc.getPokemon({ kind: 'name', slug: 'evil' })).results, []);
  assert.equal(fetch2.calls.length, 1);
});

test('PokeAPI: form names and ids fall back to /pokemon/{x} then the species; unknown is cached negatively', async () => {
  const mega = { id: 10034, name: 'charizard-mega-x', species: { name: 'charizard' }, height: 17, weight: 1105, types: [{ slot: 1, type: { name: 'fire' } }], abilities: [], stats: [] };
  const charizard = { id: 6, name: 'charizard', names: [{ name: 'Charizard', language: { name: 'en' } }] };
  const fetch = jsonFetch({ '/pokemon/charizard-mega-x': mega, '/pokemon-species/charizard': charizard, '/pokemon/10034': mega });
  const cache = setup({ safeFetchJson: fetch });
  const r = await svc.getPokemon({ kind: 'name', slug: 'charizard-mega-x' });
  assert.equal(r.results[0].name, 'Charizard (Mega X)');
  assert.equal(r.results[0].dex, 6);
  assert.ok(fetch.calls.length <= 3);
  assert.deepEqual(fetch.calls.map((c) => c.url.replace('https://pokeapi.co/api/v2', '')), [
    '/pokemon-species/charizard-mega-x', '/pokemon/charizard-mega-x', '/pokemon-species/charizard',
  ]);
  assert.equal((await svc.getPokemon({ kind: 'id', id: 10034 })).results[0].slug, 'charizard-mega-x');

  const none = await svc.getPokemon({ kind: 'name', slug: 'nopemon' });
  assert.deepEqual(none.results, []);
  assert.equal(cache.sets.at(-1).ttl, 3600);
  assert.equal(cache.sets.at(-1).k, svc.cacheKey('pokeapi', 'pokemon|nopemon'));
});

test('PokeAPI 429 pauses the source using Retry-After; other HTTP errors are UNAVAILABLE', async () => {
  const limited = new SafeFetchError('HTTP_ERROR', 429, { retryAfterMs: 90_000 });
  const fetch = jsonFetch(() => limited);
  const cache = setup({ safeFetchJson: fetch });
  const err = await svc.getPokemon({ kind: 'name', slug: 'mew' }).then(() => null, (e) => e);
  assert.equal(err.code, 'RATE_LIMITED');
  assert.ok(err.retryAt > Date.now() + 80_000 && err.retryAt <= Date.now() + 91_000);
  assert.ok(cache.sets.some((s) => s.k === 'search:pokeapi:pause' && s.ttl >= 89 && s.ttl <= 91));
  const again = await svc.getPokemon({ kind: 'name', slug: 'ditto' }).then(() => null, (e) => e);
  assert.equal(again.code, 'RATE_LIMITED');
  assert.equal(fetch.calls.length, 1);

  setup({ safeFetchJson: jsonFetch(() => new SafeFetchError('HTTP_ERROR', 502)) });
  const bad = await svc.getPokemon({ kind: 'name', slug: 'mew' }).then(() => null, (e) => e);
  assert.deepEqual([bad.code, bad.status], ['UNAVAILABLE', 502]);
  setup({ safeFetchJson: jsonFetch(() => new SafeFetchError('HTTP_ERROR', 'Response is not valid JSON', { status: 200 })) });
  assert.equal((await svc.getPokemon({ kind: 'name', slug: 'mew' }).then(() => null, (e) => e)).status, null);
});

test('PokeAPI: a network or DNS failure is retried once; a second one is UNAVAILABLE', async () => {
  let n = 0;
  const flaky = jsonFetch((path) => {
    n += 1;
    if (n === 1) return new SafeFetchError('NETWORK', 'Network error: ECONNRESET');
    return { '/pokemon-species/pikachu': PIKACHU_SPECIES, '/pokemon/pikachu': PIKACHU }[path];
  });
  setup({ safeFetchJson: flaky });
  assert.equal((await svc.getPokemon({ kind: 'name', slug: 'pikachu' })).results[0].name, 'Pikachu');
  assert.equal(flaky.calls.length, 3, 'the species request ran twice, then the default variety');

  const down = jsonFetch(() => new SafeFetchError('DNS_FAILED', 'Could not resolve pokeapi.co.'));
  setup({ safeFetchJson: down });
  const err = await svc.getPokemon({ kind: 'name', slug: 'mew' }).then(() => null, (e) => e);
  assert.deepEqual([err.code, err.status], ['UNAVAILABLE', null]);
  assert.equal(down.calls.length, 2);

  const slow = jsonFetch(() => new SafeFetchError('TIMEOUT', 'slow'));
  setup({ safeFetchJson: slow });
  assert.equal((await svc.getPokemon({ kind: 'name', slug: 'mew' }).then(() => null, (e) => e)).code, 'TIMEOUT');
  assert.equal(slow.calls.length, 1, 'a timeout is not retried');
});

// ── Pokémon index and autocomplete ─────────────────────────────────────────────

const INDEX_BODY = {
  results: [
    { name: 'pikachu', url: 'https://pokeapi.co/api/v2/pokemon-species/25/' },
    { name: 'bulbasaur', url: 'https://pokeapi.co/api/v2/pokemon-species/1/' },
    { name: 'Bad Name!', url: 'https://pokeapi.co/api/v2/pokemon-species/2/' },
    { name: 'nourl', url: 'https://pokeapi.co/api/v2/pokemon/3/' },
  ],
};
const waitFor = async (fn) => {
  for (let i = 0; i < 100; i++) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, 5));
  }
  return null;
};

test('Pokémon index: read from Redis, else refreshed in the background (never inline), with a retry back-off', async () => {
  const seeded = makeCache({ seed: { [svc.POKEAPI_INDEX_KEY]: { v: 1, entries: [['pikachu', 25]] } } });
  const fetch = jsonFetch({});
  setup({ cache: seeded, safeFetchJson: fetch });
  assert.deepEqual(await svc.getPokemonIndex(), [['pikachu', 25]]);
  assert.equal(fetch.calls.length, 0);

  const cache = makeCache();
  const fetch2 = jsonFetch({ '/pokemon-species?limit=2000': INDEX_BODY });
  setup({ cache, safeFetchJson: fetch2 });
  assert.equal(await svc.getPokemonIndex(), null, 'the first call never waits for the network');
  const entries = await waitFor(() => svc.getPokemonIndex());
  assert.deepEqual(entries, [['bulbasaur', 1], ['pikachu', 25]]);
  assert.equal(fetch2.calls.length, 1);
  assert.equal(fetch2.calls[0].opts.maxBytes, 1024 * 1024);
  assert.equal(cache.sets.find((s) => s.k === svc.POKEAPI_INDEX_KEY).ttl, 604_800);

  const failing = jsonFetch(() => new SafeFetchError('NETWORK', 'down'));
  setup({ safeFetchJson: failing });
  assert.equal(await svc.getPokemonIndex(), null);
  await waitFor(() => failing.calls.length > 0);
  await new Promise((r) => setTimeout(r, 20));
  await svc.getPokemonIndex();
  await svc.getPokemonIndex();
  assert.equal(failing.calls.length, 2, 'one refresh (its request tried twice), then none for 10 minutes');
});

function fakeInteraction({ options = {}, channel = { nsfw: false }, guildId = 'g1', userId = 'u1', log = [] } = {}) {
  const message = { id: 'm1', createMessageComponentCollector: () => ({ on() { return this; } }) };
  const i = {
    log,
    guildId,
    user: { id: userId },
    channel,
    createdTimestamp: Date.now(),
    deferred: false,
    replied: false,
    ephemeral: null,
    responded: false,
    options: { getString: (n) => options[n], getFocused: () => options.focused ?? '' },
    isRepliable: () => true,
    async reply(p) { log.push(['reply', p]); i.replied = true; return { resource: { message } }; },
    async deferReply(p) { log.push(['deferReply', p]); i.deferred = true; i.ephemeral = Boolean(p?.ephemeral); },
    async editReply(p) { log.push(['editReply', p]); return message; },
    async deleteReply() { log.push(['deleteReply']); },
    async followUp(p) { log.push(['followUp', p]); return message; },
    async respond(c) { log.push(['respond', c]); i.responded = true; },
  };
  return i;
}
const names = (i) => i.log.map((e) => e[0]);
const sent = (i, kind) => i.log.find((e) => e[0] === kind)?.[1];

test('/search pokemon autocomplete: suggestions from the index, [] when the module is off or the index is missing', async () => {
  const index = { v: 1, entries: [['bulbasaur', 1], ['pikachu', 25], ['pichu', 172]] };
  setup({ cache: makeCache({ seed: { [svc.POKEAPI_INDEX_KEY]: index } }), settings: { searchEnabled: true } });
  const i = fakeInteraction({ options: { focused: 'pi' } });
  await cmds.pokemon.autocomplete(i);
  assert.deepEqual(sent(i, 'respond'), [{ name: 'Pikachu (#25)', value: 'pikachu' }, { name: 'Pichu (#172)', value: 'pichu' }]);

  setup({ cache: makeCache({ seed: { [svc.POKEAPI_INDEX_KEY]: index } }), settings: { searchEnabled: false } });
  const off = fakeInteraction({ options: { focused: 'pi' } });
  await cmds.pokemon.autocomplete(off);
  assert.deepEqual(sent(off, 'respond'), []);

  setup({ cache: makeCache({ down: true }), safeFetchJson: jsonFetch({}), settings: null });
  const none = fakeInteraction({ options: { focused: 'pi' } });
  await cmds.pokemon.autocomplete(none);
  assert.deepEqual(sent(none, 'respond'), []);
  await new Promise((r) => setTimeout(r, 25)); // let the failed background refresh settle before the state is reset
});

// ── Urban lookup ──────────────────────────────────────────────────────────────

test('Urban: request, negative cache, bad shape and 429', async () => {
  const fetch = jsonFetch({ '/v0/define?term=Yeet%20it': { list: [YEET] }, '/v0/define?term=zzzz': { list: [] }, '/v0/define?term=broken': { nope: 1 } });
  const cache = setup({ safeFetchJson: fetch });
  const r = await svc.defineUrban('Yeet  it');
  assert.equal(r.results[0].word, 'Yeet');
  assert.equal(fetch.calls[0].url, 'https://api.urbandictionary.com/v0/define?term=Yeet%20it');
  assert.equal(fetch.calls[0].opts.maxBytes, 1024 * 1024);
  assert.equal(cache.sets.at(-1).k, svc.cacheKey('urban', 'define|yeet it'));
  assert.equal(cache.sets.at(-1).ttl, 21_600);
  assert.equal((await svc.defineUrban('yeet IT')).cached, true);

  assert.deepEqual((await svc.defineUrban('zzzz')).results, []);
  assert.equal(cache.sets.at(-1).ttl, 3600);
  const bad = await svc.defineUrban('broken').then(() => null, (e) => e);
  assert.equal(bad.code, 'UNAVAILABLE');
  assert.equal(cache.sets.some((s) => s.k === svc.cacheKey('urban', 'define|broken')), false);
  assert.deepEqual(await svc.defineUrban('  '), { results: [], cached: false });

  const limited = jsonFetch(() => new SafeFetchError('HTTP_ERROR', 429));
  setup({ safeFetchJson: limited });
  const rl = await svc.defineUrban('a').then(() => null, (e) => e);
  assert.equal(rl.code, 'RATE_LIMITED');
  assert.ok(rl.retryAt >= Date.now() + 59_000);
  assert.equal((await svc.defineUrban('b').then(() => null, (e) => e)).code, 'RATE_LIMITED');
  assert.equal(limited.calls.length, 1);
});

// ── Command flows (fake interactions) ─────────────────────────────────────────

test('/search urban: refused (ephemeral, before any defer or fetch) outside age-restricted channels while urbanNsfwOnly is on', async () => {
  for (const settings of [null, {}, { urbanNsfwOnly: true }]) {
    const fetch = jsonFetch({ '/v0/define?term=yeet': { list: [YEET] } });
    setup({ safeFetchJson: fetch, settings });
    const i = fakeInteraction({ options: { term: 'yeet' }, channel: { nsfw: false } });
    await cmds.urban.execute(i);
    assert.deepEqual(names(i), ['reply']);
    const p = sent(i, 'reply');
    assert.equal(p.ephemeral, true);
    assert.ok(p.embeds[0].toJSON().title.includes('Age-Restricted Channel Required'));
    assert.equal(fetch.calls.length, 0);
  }
  // A thread follows its parent; a missing channel counts as not age-restricted.
  setup({ settings: null });
  const thread = fakeInteraction({ options: { term: 'yeet' }, channel: { isThread: () => true, parent: { nsfw: false }, nsfw: true } });
  await cmds.urban.execute(thread);
  assert.deepEqual(names(thread), ['reply']);
  const missing = fakeInteraction({ options: { term: 'yeet' }, channel: null });
  await cmds.urban.execute(missing);
  assert.deepEqual(names(missing), ['reply']);
});

test('/search urban: allowed in an age-restricted channel, or anywhere once the guild turned urbanNsfwOnly off', async () => {
  for (const [settings, channel] of [[null, { nsfw: true }], [{ urbanNsfwOnly: false }, { nsfw: false }], [{ urbanNsfwOnly: true }, { isThread: () => true, parent: { nsfw: true } }]]) {
    const fetch = jsonFetch({ '/v0/define?term=Yeet': { list: [YEET, { ...YEET, word: 'Yeet 2' }] } });
    setup({ safeFetchJson: fetch, settings });
    const i = fakeInteraction({ options: { term: ' Yeet ' }, channel });
    await cmds.urban.execute(i);
    assert.deepEqual(names(i), ['deferReply', 'editReply'], JSON.stringify(settings));
    assert.equal(sent(i, 'deferReply').ephemeral, false);
    const p = sent(i, 'editReply');
    assert.equal(p.embeds[0].toJSON().title, 'Yeet');
    assert.equal(p.components.length, 1, 'two definitions page with buttons');
    assert.equal(fetch.calls.length, 1);
  }
});

test('/search urban: invalid term, no result and service errors reply privately; the placeholder is deleted', async () => {
  setup({ settings: { urbanNsfwOnly: false } });
  const empty = fakeInteraction({ options: { term: '  \u0000 ' } });
  await cmds.urban.execute(empty);
  assert.deepEqual(names(empty), ['reply']);
  assert.ok(sent(empty, 'reply').embeds[0].toJSON().title.includes('Invalid Search'));

  setup({ safeFetchJson: jsonFetch({ '/v0/define?term=nothing': { list: [] } }), settings: { urbanNsfwOnly: false } });
  const none = fakeInteraction({ options: { term: 'nothing' } });
  await cmds.urban.execute(none);
  assert.deepEqual(names(none), ['deferReply', 'deleteReply', 'followUp']);
  const f = sent(none, 'followUp');
  assert.equal(f.ephemeral, true);
  assert.deepEqual(f.allowedMentions, { parse: [] });
  assert.ok(f.embeds[0].toJSON().title.includes('No Results'));

  setup({ safeFetchJson: jsonFetch(() => new SafeFetchError('TIMEOUT', 'slow')), settings: { urbanNsfwOnly: false } });
  const slow = fakeInteraction({ options: { term: 'slow' } });
  await cmds.urban.execute(slow);
  assert.deepEqual(names(slow), ['deferReply', 'deleteReply', 'followUp']);
  assert.ok(sent(slow, 'followUp').embeds[0].toJSON().title.includes('Search Timed Out'));
});

test('/search anime and /search manga: the interaction is deferred before the network is touched', async () => {
  for (const [cmd, type] of [[cmds.anime, 'ANIME'], [cmds.manga, 'MANGA']]) {
    const order = [];
    const fetch = aniFetch((n, call) => {
      order.push(`fetch:${call.body.variables.type}`);
      return res(aniBody([FRIEREN]));
    });
    setup({ safeFetch: fetch });
    const i = fakeInteraction({ options: { query: 'frieren' } });
    const deferReply = i.deferReply;
    i.deferReply = async (p) => { order.push('defer'); return deferReply(p); };
    await cmd.execute(i);
    assert.deepEqual(order, ['defer', `fetch:${type}`]);
  }
});

test('/search anime: SFW channel gets pages without adult titles; NSFW channel omits isAdult and may show them', async () => {
  const fetchSfw = aniFetch(() => res(aniBody([ADULT, FRIEREN, { ...FRIEREN, id: 2 }])));
  setup({ safeFetch: fetchSfw, settings: { embedColor: 0x123456 } });
  const sfw = fakeInteraction({ options: { query: 'frieren' }, channel: { nsfw: false } });
  await cmds.anime.execute(sfw);
  assert.deepEqual(names(sfw), ['deferReply', 'editReply']);
  const p = sent(sfw, 'editReply');
  const e = p.embeds[0].toJSON();
  assert.equal(e.color, 0x123456);
  assert.equal(e.title, 'Frieren: Beyond Journey’s End');
  assert.ok(e.footer.text.endsWith('Page 1/2'));
  assert.equal(p.components.length, 1);
  assert.equal(fetchSfw.calls[0].body.variables.isAdult, false);

  const fetchNsfw = aniFetch(() => res(aniBody([ADULT, FRIEREN])));
  setup({ safeFetch: fetchNsfw });
  const nsfw = fakeInteraction({ options: { query: 'frieren' }, channel: { nsfw: true } });
  await cmds.manga.execute(nsfw);
  assert.equal('isAdult' in fetchNsfw.calls[0].body.variables, false);
  assert.equal(fetchNsfw.calls[0].body.variables.type, 'MANGA');
  assert.ok(sent(nsfw, 'editReply').embeds[0].toJSON().footer.text.includes('18+'));

  // One result: no buttons.
  setup({ safeFetch: aniFetch(() => res(aniBody([FRIEREN]))) });
  const one = fakeInteraction({ options: { query: 'solo' } });
  await cmds.anime.execute(one);
  assert.deepEqual(sent(one, 'editReply').components, []);
});

test('/search anime: invalid query, no results (with the adult hint), rate limit and cooldown answer privately', async () => {
  setup({ safeFetch: aniFetch(() => res(aniBody([]))) });
  const invalid = fakeInteraction({ options: { query: '   ' } });
  await cmds.anime.execute(invalid);
  assert.deepEqual(names(invalid), ['reply']);
  assert.equal(sent(invalid, 'reply').ephemeral, true);

  const none = fakeInteraction({ options: { query: 'zzzzqqqq' }, channel: { nsfw: false } });
  await cmds.anime.execute(none);
  assert.deepEqual(names(none), ['deferReply', 'deleteReply', 'followUp']);
  const d = sent(none, 'followUp').embeds[0].toJSON();
  assert.ok(d.title.includes('No Results'));
  assert.ok(d.description.includes('**zzzzqqqq**'));
  assert.ok(d.description.includes('age-restricted'));

  setup({ safeFetch: aniFetch(() => res({ status: 429, headers: { 'retry-after': '30' }, body: null })) });
  const limited = fakeInteraction({ options: { query: 'limited' } });
  await cmds.anime.execute(limited);
  const rl = sent(limited, 'followUp');
  assert.equal(rl.ephemeral, true);
  assert.ok(rl.embeds[0].toJSON().description.includes('<t:'));

  const cooled = fakeInteraction({ options: { query: 'other' }, userId: 'same-user' });
  setup({ safeFetch: aniFetch(() => res(aniBody([FRIEREN]))) });
  await cmds.anime.execute(cooled);
  const again = fakeInteraction({ options: { query: 'another' }, userId: 'same-user' });
  await cmds.anime.execute(again);
  assert.ok(sent(again, 'followUp').embeds[0].toJSON().description.includes('too fast'));
});

test('/search anime: settings that are not in the memo are loaded within the acknowledgement budget, or skipped', async () => {
  const fetch = aniFetch(() => res(aniBody([FRIEREN])));
  setup({ safeFetch: fetch });
  svc.__setDeps({ peekSettings: () => null, getSettingsSafe: () => new Promise(() => {}) }); // never resolves
  const i = fakeInteraction({ options: { query: 'frieren' } });
  i.createdTimestamp = Date.now() - 2_200; // ~0 ms of budget left
  const started = Date.now();
  await cmds.anime.execute(i);
  assert.ok(Date.now() - started < 1000);
  assert.deepEqual(names(i), ['deferReply', 'editReply']);
  assert.equal(sent(i, 'editReply').embeds[0].toJSON().color, COLORS.INFO);
});

test('/search pokemon: invalid input, a found Pokémon, and no match', async () => {
  setup({ safeFetchJson: jsonFetch({ '/pokemon-species/pikachu': PIKACHU_SPECIES, '/pokemon/pikachu': PIKACHU }) });
  const bad = fakeInteraction({ options: { name: '!!!' } });
  await cmds.pokemon.execute(bad);
  assert.deepEqual(names(bad), ['reply']);
  assert.ok(sent(bad, 'reply').embeds[0].toJSON().title.includes('Invalid Pokémon'));

  const ok = fakeInteraction({ options: { name: 'Pikachu' } });
  await cmds.pokemon.execute(ok);
  assert.deepEqual(names(ok), ['deferReply', 'editReply']);
  const p = sent(ok, 'editReply');
  assert.equal(p.embeds[0].toJSON().title, 'Pikachu · #25');
  assert.deepEqual(p.allowedMentions, { parse: [] });

  const missing = fakeInteraction({ options: { name: 'Nopemon' }, userId: 'other-user' });
  await cmds.pokemon.execute(missing);
  assert.deepEqual(names(missing), ['deferReply', 'deleteReply', 'followUp']);
  assert.ok(sent(missing, 'followUp').embeds[0].toJSON().description.includes('**Nopemon**'));
});

// ── 17. Static checks ─────────────────────────────────────────────────────────

test('static checks on the two source files', () => {
  const files = ['src/services/searchService.js', 'src/commands/search/search.js'];
  for (const f of files) {
    const text = readFileSync(join(ROOT, f), 'utf8');
    assert.ok(!/database\/client\.js/.test(text), `${f} imports the database client`);
    assert.ok(!text.includes('cache.del('), `${f} deletes cache keys`);
    assert.ok(!/\bconsole\./.test(text), `${f} uses console`);
    assert.ok(!text.includes('setCustomId(') && !text.includes('customId:'), `${f} declares a customId`);
    assert.ok(!/\bprisma\b/i.test(text.replace(/\/\/.*$/gm, '')), `${f} mentions prisma in code`);
  }
  const svcText = readFileSync(join(ROOT, files[0]), 'utf8');
  assert.ok(svcText.includes('sort: [SEARCH_MATCH, POPULARITY_DESC]'));
  // Every outbound request goes through safeFetch / safeFetchJson.
  assert.ok(!/\bfetch\(/.test(svcText.replace(/safeFetch(Json)?\(/g, '').replace(/deps\.\w+\(/g, '')));
  assert.ok(!/from 'node:(https?|net|dns)'|from 'undici'|require\(/.test(svcText));
});
