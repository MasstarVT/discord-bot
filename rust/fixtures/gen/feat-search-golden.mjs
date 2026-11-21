// Golden data for feat-search, produced by the JS bot's own searchService, afkService and liverole plugin (pure
// functions only, no servers). Writes rust/fixtures/golden/feat-search/{search,afk,liverole}.json.
//
//   node rust/fixtures/gen/feat-search-golden.mjs
//
// Every case carries its inputs, so the Rust test runs the same inputs through the Rust port and compares outputs.
// Embeds are reduced to { title, description, url, color, footer, thumbnail, fields, timestamp } (timestamp only when
// the code set one from data, never "now").
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};

const svc = await imp('src', 'services', 'searchService.js');
const afk = await imp('src', 'services', 'afkService.js');
const afkPlugin = await imp('src', 'plugins', 'afk.js');
const live = await imp('src', 'plugins', 'liverole.js');
const { default: redis } = await imp('src', 'services', 'redis.js');
const { SearchError } = svc;
const { escapeMarkdown } = await import('discord.js');

const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-search');
mkdirSync(OUT, { recursive: true });
const write = (name, data) => writeFileSync(join(OUT, `${name}.json`), `${JSON.stringify(data, null, 1)}\n`);

/** An embed reduced to the parts both sides set deterministically. */
function embed(e, { keepTimestamp = false } = {}) {
  const j = e.toJSON();
  const out = {};
  for (const k of ['title', 'description', 'url', 'color']) if (j[k] !== undefined && j[k] !== null) out[k] = j[k];
  if (j.footer?.text) out.footer = j.footer.text;
  if (j.thumbnail?.url) out.thumbnail = j.thumbnail.url;
  out.fields = (j.fields ?? []).map((f) => ({ name: f.name, value: f.value, inline: Boolean(f.inline) }));
  if (keepTimestamp && j.timestamp) out.timestamp = Date.parse(j.timestamp);
  return out;
}

// ── search ────────────────────────────────────────────────────────────────────

const NOW = 1_800_000_000_000;
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
const HUGE = {
  ...FRIEREN,
  title: { english: '*'.repeat(400), romaji: '_'.repeat(300), native: '~'.repeat(300) },
  description: '*'.repeat(5000),
  genres: Array.from({ length: 40 }, (_, i) => `Genre${i}${'g'.repeat(60)}`),
  studios: { nodes: Array.from({ length: 10 }, (_, i) => ({ name: `Studio ${i} ${'s'.repeat(120)}` })) },
};
const ODD = {
  id: 7.0, siteUrl: 'https://user:pw@anilist.co/x', isAdult: 'yes', format: 'NOVEL', status: 'HIATUS', episodes: -1, duration: 1.5,
  averageScore: null, meanScore: 101, genres: ['', 'A  b\u0007', 5, 'Mecha'], title: { english: '  ', romaji: 'Same', native: 'same' },
  description: null, coverImage: { extraLarge: 'http://x.example/a.png', large: 'https://x.example/b.png' }, startDate: { year: 2020 },
  endDate: { year: 2020 }, season: 'MONSOON', seasonYear: 2020, nextAiringEpisode: { episode: 3, airingAt: 1.5 }, studios: { nodes: [null, { name: 'S' }] },
};
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

const texts = [
  '', 'plain', '  Frieren\u0000\n  beyond  ', 'ｆｒｉｅｒｅｎ', 'x'.repeat(150), 'a​b', 'FRIEREN ', 'Ünïcödé  ÉÈ', 'tab\tand\r\nnewline',
  'emoji 😀😀 ok', '‮RTL', 'ﬁ ligature', 'Σίσυφος ΣΑΣ', '#25', 'Mr. Mime', "Farfetch'd", 'Nidoran♀', 'Flabébé', '😀'.repeat(120),
];
const htmls = [
  'A<br>B<br/>\n<br>C', '<i>Notes</i> &amp; &quot;x&quot; &#039;y&#039; &mdash; &#x2014;', '&amp;lt;b&amp;gt;', 'Plot ~!Snape kills!~ end',
  `${'x'.repeat(990)} ~!${'y'.repeat(50)}!~`, 'a\n\n\n\nb', '*bold* [x](https://e.com)', 'a &#0;&#xD800;&#1114112;b &bogus; &NBSP; &Hellip;',
  'hi @everyone @here &lt;@123&gt; &lt;#9&gt;\n# Heading\n> quote\n-# sub\n||fake||', '~! !~ empty spoiler ~!~!', 'line  \t\nnext\r\nwin\rmac',
  `${' '.repeat(3000)}x`, `${'['.repeat(3000)}](x)`, `${'~!'.repeat(2000)}`, '  ## two\n   ### three\n- # list heading\n#### four', '__under__ ~~strike~~ `code` ```block```',
  '<b>unclosed <a href="x">link</a> <@&123> <t:1:R> </cmd:1> <a:e:1> <id:home>', 'Text with ~!multi\nline!~ spoiler', '\\ backslash *',
];
const urbans = [
  'To discard an [item] at a [high] [velocity]', '[[a]](https://evil.example)', 'x'.repeat(3000), '# Big\n||spoil||', '@everyone @here <@123> <#9> <:e:1> <t:1:R>',
  'a\r\n\r\n\r\n\r\nb', `[${'w'.repeat(101)}]`, '[multi\nline]', '> quote\n-# small', '**bold** _it_', `${'['.repeat(2000)}x](y)`,
];

const search = {
  NOW,
  cleanQuery: texts.map((t) => [t, svc.cleanQuery(t)]),
  keyForm: texts.map((t) => [t, svc.keyForm(t)]),
  cacheKey: ['anime|sfw|frieren', 'anime|nsfw|frieren', 'manga|sfw|frieren', 'pokemon|#25', 'define|yeet it', 'ünï'].map((s) => [s, svc.cacheKey('anilist', s), svc.cacheKey('urban', s), svc.cacheKey('pokeapi', s)]),
  pokemonSlug: [
    'Pikachu', 'Mr. Mime', 'Mime Jr.', "Farfetch'd", 'Farfetch’d', 'Type: Null', 'Nidoran♀', 'Nidoran♂', 'Flabébé', 'Ho-Oh', 'porygon z',
    'Charizard Mega X', '025', '#25', '# 25', '10034', '0', '!!!', '', 'a'.repeat(41), '123456', '#', 'ｐｉｋａｃｈｕ', '  -tapu--koko-  ', 'Ｎｏ．１',
  ].map((t) => [t, svc.pokemonSlug(t)]),
  titleCase: ['mr-mime', 'charizard-mega-x', '', '--a--b', 'ho-oh'].map((t) => [t, svc.titleCase(t)]),
  stripAniListHtml: htmls.flatMap((t) => [[t, 1000, svc.stripAniListHtml(t, 1000)], [t, 60, svc.stripAniListHtml(t, 60)]]),
  cleanUrbanText: urbans.flatMap((t) => [[t, 1800, svc.cleanUrbanText(t, 1800)], [t, 100, svc.cleanUrbanText(t, 100)]]),
  stripUrbanBrackets: urbans.map((t) => [t, svc.stripUrbanBrackets(t)]),
  cleanFlavorText: ['When several of\nthese POKéMON\ngather, their\felectricity', 'pre­\nfix', 'a POKé BALL', 'soft­hyphen  x\t\vy'].map((t) => [t, svc.cleanFlavorText(t)]),
  defangMentions: ['@everyone @Here @heres', '<@1> <#2> <:a:3> <a:b:4> <t:5> <id:x> </c:6> <x>', 'mail@everyone.com'].map((t) => [t, svc.defangMentions(t)]),
  parseRetryAfter: ['60', '0', '9999', ' 12 ', new Date(NOW + 30_000).toUTCString(), new Date(NOW - 60_000).toUTCString(), 'abc', '1234567', '2026-11-18T00:00:00Z']
    .map((v) => [v, svc.parseRetryAfter(v, NOW)]),
  interpret: [
    { status: 429, headers: { 'retry-after': '42' }, body: null },
    { status: 200, headers: {}, body: { data: null, errors: [{ message: 'Too Many Requests.', status: 429 }] } },
    { status: 429, headers: { 'x-ratelimit-reset': String(NOW / 1000 + 20) }, body: null },
    { status: 200, headers: { 'x-ratelimit-remaining': '0' }, body: { data: { Page: { media: [] } } } },
    { status: 200, headers: { 'x-ratelimit-remaining': '0', 'x-ratelimit-reset': String(NOW / 1000 + 7) }, body: { data: { Page: { media: [] } } } },
    { status: 200, headers: { 'x-ratelimit-remaining': '29' }, body: { data: { Page: { media: [FRIEREN] } } } },
    { status: 200, headers: {}, body: { data: { Page: { media: [] } }, errors: [{ message: 'x' }] } },
    { status: 503, headers: {}, body: null },
    { status: 200, headers: {}, body: { data: {} } },
    { status: 200, headers: {}, body: { data: { Page: { media: [] } }, errors: [] } },
    { status: 404, headers: { 'x-ratelimit-remaining': '0' }, body: { errors: [{ status: 404 }] } },
  ].map((c) => [c, svc.interpretAniListResponse(c, NOW)]),
  projectAniList: [
    [[ADULT, FRIEREN, { id: 'x' }], 'ANIME', false], [[ADULT, FRIEREN, { id: 'x' }], 'ANIME', true],
    [[{ ...FRIEREN, siteUrl: 'http://anilist.co/x' }], 'ANIME', false], [[{ ...FRIEREN, siteUrl: 'https://evil.example/x' }], 'ANIME', false],
    [Array.from({ length: 7 }, (_, i) => ({ ...FRIEREN, id: 100 + i })), 'ANIME', false], [[HUGE], 'ANIME', false], [[ODD], 'MANGA', true],
    [[{ ...FRIEREN, status: 'RELEASING', nextAiringEpisode: { episode: 12, airingAt: NOW / 1000 + 3600 } }], 'ANIME', false],
    [[{ ...FRIEREN, type: 'MANGA', format: 'MANGA', status: 'RELEASING', chapters: null, volumes: 4, endDate: { year: null } }], 'MANGA', false],
    [[null, 5, [], { id: 3 }], 'ANIME', false],
  ].map(([media, type, allowAdult]) => [media, type, allowAdult, svc.projectAniListResults(media, { type, allowAdult })]),
  projectPokemon: [
    [PIKACHU, PIKACHU_SPECIES], [{ ...PIKACHU, name: 'pikachu-rock-star', id: 10080 }, PIKACHU_SPECIES], [PIKACHU, null], [{ id: 1, name: 'x' }, null],
    [null, null], [PIKACHU, { ...PIKACHU_SPECIES, is_legendary: true }], [PIKACHU, { ...PIKACHU_SPECIES, is_mythical: true, generation: { name: 'generation-viii' } }],
    [{ id: 386, name: 'deoxys-normal', species: { name: 'deoxys' }, types: [], abilities: [], stats: [] }, { id: 386, name: 'deoxys', names: [{ name: 'Deoxys', language: { name: 'en' } }] }],
    [{ ...PIKACHU, types: [{ slot: '2', type: { name: 'steel' } }, { slot: 1, type: { name: 'fairy' } }, { type: { name: '' } }], stats: [{ base_stat: -4, stat: { name: 'hp' } }, { base_stat: 1.5, stat: { name: 'speed' } }] }, null],
  ].map(([p, s]) => [p, s, svc.projectPokemon(p, s)]),
  projectUrban: [
    [YEET, { definition: '   ' }, null], [{ ...YEET, permalink: 'https://evil.example/x' }], Array.from({ length: 7 }, () => YEET), 'nope',
    [{ ...YEET, written_on: 'garbage' }], [{ ...YEET, word: '**@everyone**', definition: 'x'.repeat(5000), example: 'y'.repeat(5000), author: null, thumbs_up: 5.5 }],
    [{ definition: 'only', word: 42 }],
  ].map((l) => [l, svc.projectUrban(l)]),
  matchPokemonIndex: (() => {
    const entries = [['bulbasaur', 1], ['pikachu', 25], ['raichu', 26], ['mr-mime', 122], ['mr-rime', 866], ['pichu', 172]];
    return { entries, cases: [['pi', 25], ['chu', 25], ['Mr. M', 25], ['#2', 25], ['', 25], ['mr-m', 25], ['', 2], ['12', 25], ['zzz', 25]].map(([q, limit]) => [q, limit, svc.matchPokemonIndex(entries, q, limit)]) };
  })(),
  aniListEmbeds: (() => {
    const cases = [];
    const add = (results, type, settings, now) => cases.push([results, type, settings, now, svc.buildAniListEmbeds(results, { type, settings, now }).map((e) => embed(e))]);
    add(svc.projectAniListResults([FRIEREN], { type: 'ANIME' }), 'ANIME', { embedColor: 0xff7a00 }, 0);
    add(svc.projectAniListResults([FRIEREN], { type: 'ANIME' }), 'ANIME', null, 0);
    add(svc.projectAniListResults([{ ...FRIEREN, type: 'MANGA', format: 'MANGA', status: 'RELEASING', chapters: null, volumes: null, endDate: { year: null } }], { type: 'MANGA' }), 'MANGA', null, 0);
    const airing = svc.projectAniListResults([{ ...FRIEREN, status: 'RELEASING', nextAiringEpisode: { episode: 12, airingAt: NOW / 1000 + 3600 } }], { type: 'ANIME' });
    add(airing, 'ANIME', null, NOW);
    add(airing, 'ANIME', null, NOW + 7_200_000);
    add(svc.projectAniListResults([ADULT], { type: 'ANIME', allowAdult: true }), 'ANIME', null, 0);
    add(svc.projectAniListResults([HUGE], { type: 'ANIME' }), 'ANIME', null, 0);
    add(svc.projectAniListResults([ODD], { type: 'MANGA', allowAdult: true }), 'MANGA', null, 0);
    add(svc.projectAniListResults([{ ...ODD, type: 'ANIME', format: 'ONA', status: 'NOT_YET_RELEASED', startDate: { year: null } }], { type: 'ANIME', allowAdult: true }), 'ANIME', null, 0);
    add(svc.projectAniListResults(Array.from({ length: 3 }, (_, i) => ({ ...FRIEREN, id: i + 1, status: ['CANCELLED', 'RELEASING', 'X'][i], genres: [], studios: { nodes: [] } })), { type: 'ANIME' }), 'ANIME', null, 0);
    return cases;
  })(),
  pokemonEmbeds: [
    [PIKACHU, PIKACHU_SPECIES, null], [PIKACHU, null, { embedColor: 0x123456 }], [PIKACHU, { ...PIKACHU_SPECIES, is_legendary: true }, null],
    [{ id: 1, name: '**x**' }, { id: 1, is_mythical: true, generation: { name: 'generation-ii' }, genera: [{ genus: '_g_', language: { name: 'en' } }] }, null],
  ].map(([p, s, settings]) => {
    const info = svc.projectPokemon(p, s);
    return [info, settings, embed(svc.buildPokemonEmbed(info, settings))];
  }),
  urbanEmbeds: [
    [svc.projectUrban([YEET]), null], [svc.projectUrban([{ ...YEET, thumbs_up: 5, thumbs_down: 1234 }]), { embedColor: 1 }],
    [svc.projectUrban([{ ...YEET, word: '**@everyone**', definition: 'x'.repeat(5000), example: 'y'.repeat(5000), author: '' }]), null],
    [svc.projectUrban([{ ...YEET, example: '', written_on: null }, YEET]), null],
  ].map(([results, settings]) => [results, settings, svc.buildUrbanEmbeds(results, { settings }).map((e, i) => embed(e, { keepTimestamp: Boolean(results[i].writtenOn) }))]),
  noResults: [['anime', 'abc', false], ['anime', 'abc', true], ['manga', 'abc', true], ['urban', '**x**', true], ['pokemon', 'abc', true], ['urban', '@everyone', true], ['manga', 'x'.repeat(200), false]]
    .map(([k, q, a]) => [k, q, a, embed(svc.noResultsEmbed(k, q, { allowAdult: a }))]),
  errors: [
    ['RATE_LIMITED', null, 1_800_000_060_000], ['RATE_LIMITED', null, null], ['COOLDOWN', null, null], ['COOLDOWN', null, 1_800_000_060_000], ['TIMEOUT', null, null],
    ['UNAVAILABLE', 503, null], ['UNAVAILABLE', null, null], [null, null, null],
  ].map(([code, status, retryAt]) => {
    const err = code ? new SearchError(code, { api: 'anilist', status, retryAt }) : new Error('x');
    return [code, status, retryAt, embed(svc.searchErrorEmbed(err, 'AniList')), embed(svc.searchErrorEmbed(err, 'Urban Dictionary'))];
  }),
  requestBody: [['ANIME', 'Frieren', false], ['MANGA', 'Ünï "q"\\', true]].map(([t, q, a]) => {
    const variables = { search: q, type: t, perPage: svc.MAX_RESULTS };
    if (!a) variables.isAdult = false;
    return [t, q, a, JSON.stringify({ query: svc.ANILIST_QUERY, variables })];
  }),
  query: svc.ANILIST_QUERY,
  escapeMarkdown: [...texts, ...htmls, ...urbans, '[a](b) [c](d)\n[x](y)', '[](x)', '[a]()', '[a](b', 'a [b](c) d', '[[x](y)](z)',
    'http://x.y/a*b* _c_ <a:e_x:1> __u__', '***triple*** **bold** *it* ****', '`a` x', '\\*escaped\\*',
    '~~~ |||  [a](b) ', '  # h\n * # h\n   # no', '### three #### four', '[a😀](b😀)', '[ab](c) [d', 'x[a]\r(b) [c](d)']
    .map((t) => [t, escapeMarkdown(t), escapeMarkdown(t, { maskedLink: true, heading: true })]),
};
write('search', search);

// ── afk ───────────────────────────────────────────────────────────────────────

const since = NOW - (2 * 3_600_000 + 5 * 60_000);
const pings8 = Array.from({ length: 8 }, (_, i) => ({ channelId: 'C', messageId: `M${i}`, authorId: 'A', at: i }));
const afkOut = {
  NOW,
  cleanReason: ['  at\nthe   gym \t', '   ', 'x'.repeat(250), 'ok', 'tab nbsp'].map((t) => [t, afk.cleanReason(t)]),
  plainText: [['hi @everyone and @here <@123> <@&456> <#789> :smile:', 200], ['x'.repeat(500), 40], ['', 200], ['@EVERYONE <a:x:1>', 200], ['  multi \n line  ', 5]]
    .map(([t, m]) => [t, m, afk.plainText(t, m)]),
  nameWithoutPrefix: ['[AFK] Bob', 'Bob', '[AFK]Bob', ''].map((t) => [t, afk.nameWithoutPrefix(t)]),
  afkLength: [5 * 60_000, 2 * 3_600_000 + 5 * 60_000 + 9000, 30_000, -5, 0, 999, 1000, 3 * 86_400_000 + 7_200_000 + 1, 400 * 86_400_000].map((ms) => [ms, afk.afkLength(ms)]),
  readPings: [
    [{ channelId: 'C', messageId: 'M', authorId: 'A', at: 5 }, { channelId: 'C' }, null, 'x', { channelId: 'C', messageId: 'N', at: '7' }, { channelId: 'C', messageId: 'O', authorId: 3, at: 'x' }],
    {}, [],
  ].map((p) => [p, afk.readPings(p)]),
  prefixedNickname: ['Bob', '[AFK] Bob', 'n'.repeat(40), `${'a'.repeat(25)}😀😀`, '', 'x'.repeat(26), 'x'.repeat(27)].map((t) => [t, afk.prefixedNickname(t)]),
  restoredNickname: [['[AFK] Bobster', 'Bobster'], ['[AFK] Bob', null], ['[AFK] ', null], ['Robert', 'Bobster'], [null, 'Bobster'], ['[AFK] x', '']]
    .map(([n, p]) => [n, p, afk.restoredNickname(n, p) === undefined ? 'undefined' : afk.restoredNickname(n, p)]),
  ignored: [
    [['CAT', 'C1'], { id: 'C1' }], [['CAT', 'C1'], { id: 'C2', parentId: 'CAT' }], [['CAT', 'C1'], { id: 'T', parentId: 'C9', parent: { parentId: 'CAT' } }],
    [['CAT', 'C1'], { id: 'C2', parentId: 'OTHER' }], [[], { id: 'C1' }],
  ].map(([list, ch]) => [list, { id: ch.id, parentId: ch.parentId ?? null, grandparentId: ch.parent?.parentId ?? null }, afk.isIgnoredChannel({ afkIgnoredChannelIds: list }, ch)]),
  welcome: [[0, []], [12, pings8], [1, []], [2, pings8.slice(0, 2)], [3, 'bad']].map(([count, pings]) => [
    count, pings, afkPlugin.welcomeText({ name: 'Bob', guildId: 'G', row: { since: new Date(since), pingCount: count, pings }, now: NOW }),
  ]),
  notice: [['at the gym'], [null], ['@everyone <@1> \n two'], ['']].map(([reason]) => [reason, afkPlugin.noticeText({ name: 'Bob', row: { reason, since: new Date(since) } })]),
  since,
  shownName: [['[AFK] **Bob**'], ['@everyone'], ['x'.repeat(60)], [''], [null], ['[AFK] '], ['_under_ <@1>']].map(([n]) => [n, afkPlugin.shownName(n === null ? null : { displayName: n }, null)]),
  jumpLink: afk.jumpLink('G', 'C', 'M'),
};
write('afk', afkOut);

// ── liverole ──────────────────────────────────────────────────────────────────

const presence = (...activities) => ({ activities });
const streaming = (extra = {}) => ({ type: 1, name: 'Twitch', url: 'https://www.twitch.tv/someone', details: 'Ranked', state: 'Valorant', ...extra });
const results = [
  null, { ok: false, code: 'settings' }, { ok: false, code: 'off' }, { ok: false, code: 'no-role' }, { ok: false, code: 'role-missing' },
  { ok: false, code: 'cannot-manage', reason: 'That role is above my highest role.' }, { ok: false, code: 'presence-unavailable' }, { ok: false, code: 'error' },
  { ok: true, mode: 'sync', added: 0, removed: 0, failed: 0, deferred: 0, addsBlocked: null, complete: true },
  { ok: true, mode: 'sync', added: 1234, removed: 2, failed: 3, deferred: 4, addsBlocked: 'That role has dangerous permissions: Administrator.', complete: false },
  { ok: true, mode: 'clear', added: 0, removed: 0, failed: 0, deferred: 0, addsBlocked: null, complete: true },
  { ok: true, mode: 'clear', added: 0, removed: 5, failed: 0, deferred: 9, addsBlocked: null, complete: true },
];
const liveOut = {
  streamLabel: [presence(streaming()), presence(streaming({ name: '' })), presence(streaming({ name: '**Big**_' + 'x'.repeat(40) })), presence({ type: 0, name: 'Game' }), null]
    .map((p) => [p, live.streamLabel(p)]),
  safeStreamUrl: [
    'https://www.twitch.tv/x', 'http://twitch.tv/x', 'https://youtu.be/abc', 'https://m.youtube.com/watch?v=1', 'https://evil.example/twitch.tv',
    'https://twitch.tv.evil.example/', 'javascript:alert(1)', 'https://user:pw@twitch.tv/x', `https://twitch.tv/${'x'.repeat(600)}`, 'not a url', 'HTTPS://TWITCH.TV/Path',
    'https://twitch.tv', 'https://www.youtube.com/watch?v=a b',
  ].map((u) => [u, live.safeStreamUrl(u)]),
  streamVars: [
    streaming(), streaming({ url: 'https://evil.example/x', details: '  **Bold** title  ', state: '' }), streaming({ details: 'd'.repeat(300), state: 's'.repeat(300), name: '' }),
    streaming({ url: null, details: null, state: null }),
  ].map((a) => [a, live.streamVars(presence(a))]),
  planMember: ['sync', 'clear'].flatMap((mode) => [true, false].flatMap((s) => [true, false].flatMap((q) => [true, false].map((h) => [mode, s, q, h, live.planMember({ mode, streaming: s, qualifies: q, holds: h })])))),
  planSync: [
    ['sync', ['1', '2', '3'], ['2', '4', '4', '5'], ['6', '1', '7'], ['4', '2']],
    ['clear', ['1', '1', '2'], ['3'], ['4'], []],
    ['sync', [], [], [], []],
  ].map(([mode, holderIds, streamerIds, unknownIds, ok]) => [mode, holderIds, streamerIds, unknownIds, ok, live.planSync({ mode, holderIds, streamerIds, unknownIds, qualifies: (id) => ok.includes(id) })]),
  meets: [[null, true, ['1'], 'G'], ['G', true, [], 'G'], ['R', false, ['R'], 'G'], ['R', true, ['R'], 'G'], ['R', true, ['X'], 'G']]
    .map(([req, exists, roles, g]) => [req, exists, roles, g, live.meetsRequirement({ requiredRoleId: req, requiredRoleExists: exists, memberRoleIds: new Set(roles), guildId: g })]),
  classify: [10007, 10011, 50013, 50001, 500, null].map((code) => [code, live.classifyRoleError(code === null ? null : { code })]),
  formatSyncResult: results.map((r) => [r, live.formatSyncResult(r)]),
  problems: [
    { enabled: true, liveRoleId: null, roleExists: false, manage: null, dangerousNames: [], requiredRoleId: null, requiredRoleExists: true, holderCount: 0 },
    { enabled: false, liveRoleId: 'R', roleExists: false, manage: null, dangerousNames: [], requiredRoleId: 'Q', requiredRoleExists: false, holderCount: 3 },
    { enabled: true, liveRoleId: 'R', roleExists: true, manage: { ok: false, reason: 'That role is above my highest role.' }, dangerousNames: ['Administrator', 'Manage Roles'], requiredRoleId: null, requiredRoleExists: true, holderCount: 1500 },
    { enabled: false, liveRoleId: 'R', roleExists: true, manage: { ok: true }, dangerousNames: [], requiredRoleId: null, requiredRoleExists: true, holderCount: 0 },
  ].map((p) => [p, live.liveRoleProblems(p)]),
  shouldHandle: [
    [null, presence(streaming()), null], [null, presence(streaming()), { liveRoleEnabled: false, liveRoleId: 'R' }], [null, presence(streaming()), { liveRoleEnabled: true, liveRoleId: null }],
    [null, presence(streaming()), { liveRoleEnabled: true, liveRoleId: 'R' }], [presence(streaming()), presence(streaming()), null], [presence(streaming()), presence(), null],
  ].map(([o, n, p]) => [o, n, p, live.shouldHandlePresence(o, n, p)]),
  DEFAULT_LIVE_ANNOUNCE_MESSAGE: live.DEFAULT_LIVE_ANNOUNCE_MESSAGE,
  defersRemoval: ['sync', 'clear'].flatMap((mode) => ['presence', 'sync', 'recheck'].flatMap((source) => [true, false].flatMap((s) => [true, false]
    .map((q) => [mode, source, s, q, live.defersRemoval({ mode, source, streaming: s, qualifies: q })])))),
  STOP_GRACE_MS: live.LIVEROLE.STOP_GRACE_MS,
  recheckJobKey: live.recheckJobKey('111', '222'),
};
write('liverole', liveOut);

redis.disconnect();
process.exit(0);
