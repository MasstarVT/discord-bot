// Golden data for feat-translation, produced by the JS bot's own translationService, translateStatsService, autotime
// plugin and /timestamp, /autotime command helpers (no servers: fetch, the cache and Date.now are stubbed). Writes
// rust/fixtures/golden/feat-translation/golden.json.
//
//   node rust/fixtures/gen/feat-translation-golden.mjs
//
// Every case carries its inputs, so the Rust test runs the same call and compares the outputs.
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

const svc = await imp('src', 'services', 'translationService.js');
const stats = await imp('src', 'services', 'translateStatsService.js');
const autotime = await imp('src', 'plugins', 'autotime.js');
const autotimeCmd = await imp('src', 'commands', 'admin', 'autotime.js');
const ts = await imp('src', 'commands', 'utility', 'timestamp.js');
const { parseWhen } = await imp('src', 'utils', 'time.js');
const userTz = await imp('src', 'services', 'userTimezoneService.js');
const { cache } = await imp('src', 'services', 'redis.js');
const { default: redis } = await imp('src', 'services', 'redis.js');
const { __testing } = svc;

const out = {};

// ── Language codes and names ────────────────────────────────────────────────────────────────────────────────────
const codeInputs = [
  'iw', 'he', 'zh-CN', 'zh-TW', 'zh-Hant', 'zh-Hant-TW', 'zh-HK', 'zh-Hans', 'zh', 'zh_tw', 'zt', 'nb', 'nn', 'en-US', 'pt-BR',
  'pt-PT', 'pb', 'fil', 'ceb', 'jw', 'ji', 'in', 'auto', 'und', '', ' EN ', 'english', 'e', 'abcd', 'en-', 'en-us-x-abcdefghi',
  'sr-Latn', 'zh-SG', 'zh-MO', 'yue', 'haw', 'iw-IL', 'nb-NO', 'x1', 'es-419',
];
out.normalizeLangCode = codeInputs.map((input) => [input, svc.normalizeLangCode(input)]);
out.normalizeLangCode.push([null, svc.normalizeLangCode(null)], [12, svc.normalizeLangCode(12)]);
const resolveInputs = [...codeInputs, 'Japanese', 'japanese', ' Chinese ', 'traditional chinese', 'Chinese (Traditional)', 'tagalog', 'farsi',
  'Norwegian Bokmål', 'norwegian bokmal', 'burmese', 'Myanmar (Burmese)', 'jp', 'kr', 'xx', 'Klingon'];
out.resolveLanguageInput = resolveInputs.map((input) => [input, svc.resolveLanguageInput(input)]);
out.suggestLanguage = ['jp', 'KR', ' cn ', 'tw', 'us', 'gb', 'xx', 'ja', ''].map((input) => [input, svc.suggestLanguage(input)]);
const nameCodes = [...Object.keys(svc.LANGUAGES), 'ceb', 'jv', 'yue', 'haw', 'la', 'und', 'xyz', 'zza', 'sco', 'mi', 'qu', 'ny', 'ku'];
out.languageName = nameCodes.map((c) => [c, svc.languageName(c)]);
out.languageNameNull = svc.languageName(null);
out.langFlag = [...Object.keys(svc.LANGUAGES), 'xx', 'ceb'].map((c) => [c, svc.langFlag(c)]);
out.normalizeTargetList = [
  [['es', 'ES', ' es ', 'iw', 'he', 'xx']],
  [['Japanese', 'ja', 'zh-Hant', 'zh-tw', 'nb', 'jp']],
  [[]],
].map(([inputs]) => [inputs, svc.normalizeTargetList(inputs)]);
out.toGoogleCode = ['zh', 'zh-tw', 'ja', 'he'].map((c) => [c, svc.toGoogleCode(c)]);
out.toLtCode = [
  ['no', ['nb']], ['zh', ['zh-hans']], ['zh', ['zh']], ['zh-tw', ['zt']], ['zh-tw', []], ['he', []], ['he', ['iw']], ['tl', ['fil']],
  ['pt', ['pt-br']], ['pt', ['pt']], ['ja', ['ja']], ['no', []],
].map(([code, raw]) => [code, raw, svc.toLtCode(code, new Set(raw))]);
out.languagesSorted = Object.entries(svc.LANGUAGES).sort((a, b) => a[1].localeCompare(b[1])).map(([c]) => c);
out.languageChoices = ['', 'zh', 'trad', 'iw', 'ja', 'chin', 'e', 'x', 'pt', 'nor', 'ZH-TW', 'an', 'zzz', 'nb'].map((q) => [q, svc.languageChoices(q)]);

// ── Text preparation ────────────────────────────────────────────────────────────────────────────────────────────
const texts = [
  'https://a.com https://b.com', 'https://a.com\nhttps://b.com', '<https://a.com>', 'look https://a.com', 'https://a.com look', 'http://', '',
  '❤️', '👍🏽', '👨‍👩‍👧', '🇯🇵', '🇺🇸🇯🇵', '1️⃣', '<@123456789012345678>', '<@!123456789012345678>', '<@&123456789012345678>',
  '<#123456789012345678>', '<t:1700000000:R>', '@everyone', '<:pepe:123456789012345678>', '</ping sub:123456789012345678>', 'k', '42!!',
  '🏴󠁧󠁢󠁥󠁮󠁧󠁿 k 42!!', 'hola amigo ❤️', '好', '||secret words||', '||a|| b ||c||', '  many   spaces \n\n\n\n lines  \t end ',
  'Привет как дела', 'I love 寿司', 'check <a:dance:123456789012345678> this @here now', 'x y z', 'café ☕ ok',
  '<t:-1:F> and <t:123>', 'mention <@12345> short id', 'Done. Next!', '日本語のテキスト。次の文！',
];
out.text = texts.map((t) => ({
  input: t,
  isLinkOnly: svc.isLinkOnly(t),
  clean: svc.cleanForTranslation(t),
  translatable: svc.hasTranslatableText(t),
  prepared: svc.prepareAutoInput(t),
}));
out.isEcho = [['same text here', 'Same text, here!'], ['hola', 'hello'], ['¿Qué?', 'que'], ['a b', 'ab'], ['$5', '5']].map(([a, b]) => [a, b, svc.isEcho(a, b)]);
const longJa = '今日はとても暑いですね。'.repeat(125);
const longRu = 'Привет как дела '.repeat(125);
out.clampForRequest = [
  [longRu, 1000, 6000], ['a'.repeat(999) + '𠀀'.repeat(5), 1000, null], ['short', 1000, null], [longJa, 1000, 24000],
  ['word '.repeat(300), 1000, null], ['x'.repeat(1200), 1000, null], ['line one\nline two\n'.repeat(80), 500, 6000],
  ['Sentence one. Sentence two! '.repeat(60), 300, null],
].map(([text, maxCodePoints, maxEncodedBytes]) => ({
  text, maxCodePoints, maxEncodedBytes,
  out: svc.clampForRequest(text, maxEncodedBytes == null ? { maxCodePoints } : { maxCodePoints, maxEncodedBytes }),
}));
out.splitForRequests = [
  [longJa, 6000], ['😀'.repeat(1500), 6000], ['好'.repeat(3000), 24000], ['word '.repeat(3000), 6000], ['a\nb\nc', 2], ['', 10],
  ['abc', 1], ['Hi there. Bye now! '.repeat(400), 3000],
].map(([text, max]) => ({ text, max, out: svc.splitForRequests(text, max) }));
out.splitForEmbeds = [
  ['x'.repeat(4096)], ['word '.repeat(3000)], ['😀'.repeat(3000)], ['short'], ['line\n'.repeat(2000)], ['a'.repeat(20), 8, 2], ['   '],
  ['Ein Satz. '.repeat(900), 4000, 3],
].map(([text, max, parts]) => ({ text, max: max ?? null, parts: parts ?? null, out: svc.splitForEmbeds(text, max, parts) }));
out.balanceMarkdown = ['**bold', '```code', '`x', 'fine', '__u ~~s', '```a``` `b', '** **', '`a` `b'].map((s) => [s, svc.balanceMarkdown(s)]);

// ── Embeds and errors ───────────────────────────────────────────────────────────────────────────────────────────
const embedJson = (e) => {
  const d = e.toJSON ? e.toJSON() : e.data;
  const { timestamp, ...rest } = d;
  return { ...rest, hasTimestamp: timestamp !== undefined };
};
const long = 'palabra '.repeat(400);
out.autoEmbed = [
  { results: ['es', 'fr', 'de', 'it', 'pt'].map((target) => ({ target, text: long })), detected: 'en' },
  { results: [{ target: 'es', text: 'hola **mundo' }, { target: 'fr', text: 'bonjour' }], detected: 'en', hadSpoiler: true },
  { results: [{ target: 'zh-tw', text: 'x||y' }], detected: null, truncatedInput: true, pausedStop: true, hadSpoiler: true },
  { results: [{ target: 'ja', text: '```code ' + 'a'.repeat(3000) }], detected: 'ceb', settings: { embedColor: 0x123456 } },
].map((args) => ({ args, embed: embedJson(svc.buildAutoTranslateEmbed(args)) }));
out.autoReplyPayload = (() => {
  const p = svc.buildAutoReplyPayload(svc.buildAutoTranslateEmbed({ results: [{ target: 'es', text: 'hola' }], detected: 'en' }));
  return { allowedMentions: p.allowedMentions, failIfNotExists: p.failIfNotExists };
})();
out.formatTargetLine = [['zh-tw', 'x'], ['eo', 'y']].map(([t, b]) => [t, b, svc.formatTargetLine(t, b)]);

const NOW = Date.UTC(2026, 9, 16, 20, 0, 0);
const realNow = Date.now;
Date.now = () => NOW;
const errCases = [
  ['PAUSED', {}], ['PAUSED', { retryAfterMs: 90_000 }], ['RATE_LIMITED', { retryAfterMs: 1_800_000 }], ['RATE_LIMITED', { retryAfterMs: 30_000 }],
  ['UNSUPPORTED_LANGUAGE', { lang: 'cy' }], ['UNSUPPORTED_LANGUAGE', {}], ['EMPTY', {}], ['TOO_LONG', {}], ['TIMEOUT', {}], ['NETWORK', {}],
  ['HTTP_ERROR', { status: 500 }], ['BAD_RESPONSE', {}], [null, {}],
];
out.describeError = { now: NOW, cases: [] };
for (const provider of ['google', 'libretranslate']) {
  __testing.reset();
  __testing.configure({ provider, resultCache: false });
  for (const [code, extra] of errCases) {
    const err = code ? new svc.TranslationError(code, code, extra) : new Error('boom');
    const d = svc.describeTranslationError(err);
    out.describeError.cases.push({ provider, code, extra, level: d.level, log: d.log, embed: embedJson(d.embed) });
  }
}
Date.now = realNow;

// ── Requests, responses and cache writes (fetch and the cache stubbed) ──────────────────────────────────────────
const jsonRes = (body, status = 200, headers = {}) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json', ...headers } });
const store = new Map();
cache.get = async (key) => (store.has(key) ? store.get(key).value : null);
cache.set = async (key, value, ttl) => { store.set(key, { value: JSON.parse(JSON.stringify(value)), ttl }); return true; };
cache.setNX = async (key, value, ttl) => { if (store.has(key)) return false; store.set(key, { value: JSON.parse(JSON.stringify(value)), ttl }); return true; };
cache.del = async (key) => { store.delete(key); return true; };

async function scenario(name, config, responder, run) {
  __testing.reset();
  __testing.configure(config);
  store.clear();
  const calls = [];
  globalThis.fetch = async (url, init = {}) => {
    const headers = init.headers ?? {};
    calls.push({ url: String(url), method: init.method ?? 'GET', headers, body: init.body ?? null });
    return responder(String(url), init, calls.length);
  };
  let result;
  let error = null;
  try {
    result = await run();
  } catch (err) {
    error = { code: err.code ?? null, message: err.message, status: err.status ?? null, lang: err.lang ?? null, retryAfterMs: err.retryAfterMs ?? null };
  }
  const cacheOut = [...store].map(([key, { value, ttl }]) => ({ key, value, ttl }));
  return { name, config, calls, result: result ?? null, error, cache: cacheOut, paused: svc.translationPaused() };
}
const g = (detected, text = 'T') => () => jsonRes([[[text, 'S']], null, detected]);
out.requests = [
  await scenario('google get', { provider: 'google', resultCache: true }, g('ja', 'It is hot'), () => svc.translateText('今日はとても暑いですね', 'en')),
  await scenario('google post', { provider: 'google', resultCache: true }, g('zh-CN', 'translated'), () => svc.translateText('好'.repeat(1500), 'en', { maxChunks: 4 })),
  await scenario('google chunks', { provider: 'google', resultCache: true }, (u, i, n) => jsonRes([[[`T${n}`, 'S']], null, 'zh-CN']),
    () => svc.translateText('好'.repeat(3000), 'en', { maxChunks: 4 })),
  await scenario('google segments', { provider: 'google', resultCache: false }, () => jsonRes([[['Hello ', 'a'], ['world', 'b'], [null, 'c'], [5]], null, 'iw']),
    () => svc.translateText('שלום עולם', 'zh-tw', { source: 'he' })),
  await scenario('google 429', { provider: 'google', resultCache: false }, () => new Response('no', { status: 429, headers: { 'retry-after': '7200' } }),
    () => svc.translateText('hello there', 'es')),
  await scenario('google 500', { provider: 'google', resultCache: false }, () => new Response('no', { status: 500 }), () => svc.translateText('hello', 'es')),
  await scenario('google bad json', { provider: 'google', resultCache: false }, () => new Response('nope', { status: 200 }), () => svc.translateText('hello', 'es')),
  await scenario('google bad shape', { provider: 'google', resultCache: false }, () => jsonRes({ a: 1 }), () => svc.translateText('hello', 'es')),
  await scenario('google detect', { provider: 'google', resultCache: false }, g('en'), () => svc.detectLanguage('今天天氣很好，我們去公園玩吧 https://x.y')),
  await scenario('targets', { provider: 'google', resultCache: true }, g('ja'), () => svc.translateToTargets('今日はとても暑いですね', ['ja', 'en', 'es'])),
  await scenario('lt translate', { provider: 'libretranslate', ltUrl: 'http://lt.test', ltKey: 'secret', resultCache: true },
    () => jsonRes({ translatedText: 'x', detectedLanguage: { language: 'zh-Hant', confidence: 80 } }), () => svc.translateText('hi', 'no')),
  await scenario('lt unsupported', { provider: 'libretranslate', ltUrl: 'http://lt.test', resultCache: false },
    () => jsonRes({ error: 'zz is not supported as source language' }, 400), () => svc.translateText('hi', 'cy', { source: 'ja' })),
  await scenario('lt detect', { provider: 'libretranslate', ltUrl: 'http://lt.test', resultCache: false },
    () => jsonRes([{ language: 'nb', confidence: 90 }]), () => svc.detectLanguage('Dette er en norsk setning')),
  await scenario('lt 429', { provider: 'libretranslate', ltUrl: 'http://lt.test', resultCache: false },
    () => new Response('slow', { status: 429 }), () => svc.translateText('hi', 'es')),
];
delete globalThis.fetch;

// ── Translation counters ───────────────────────────────────────────────────────────────────────────────────────
out.statKeys = [
  ['es', ['ja', 'en', 'ja']], ['zh-tw', ['en']], [null, []], ['Spanish; DROP', ['EN', 42, null, 'fr']], ['es', null], ['abcd', ['en-us', 'pt-br-x']],
].map(([d, t]) => [d, t, stats.statKeys(d, t)]);
out.rawSql = [...svc.RAW_SQL, ...stats.RAW_SQL].map((e) => {
  const q = e.build();
  return { name: e.name, sql: q.sql, values: q.values };
});

// ── Time features ───────────────────────────────────────────────────────────────────────────────────────────────
const at = new Date('2026-10-23T22:00:00Z');
out.answerText = [
  [{ text: 'fri `5pm`\\', ms: at.getTime() }],
  [{ text: '   ', ms: at.getTime() }, { text: 'x'.repeat(80), ms: at.getTime() + 3_600_000 }],
].map((times) => [times, autotime.answerText(times.map((t) => ({ text: t.text, date: new Date(t.ms) })))]);
out.zoneHintText = [['tomorrow 5pm', true], ['fri 5pm', false], ['`x`', true]].map(([t, r]) => [t, r, autotime.zoneHintText(t, r)]);
out.isIgnoredChannel = [
  [{ autoTimeIgnoredChannelIds: ['C', 'CAT'] }, { id: 'C' }], [{ autoTimeIgnoredChannelIds: ['C', 'CAT'] }, { id: 'X', parentId: 'CAT' }],
  [{ autoTimeIgnoredChannelIds: ['C', 'CAT'] }, { id: 'T', parentId: 'P', parent: { parentId: 'CAT' } }], [{ autoTimeIgnoredChannelIds: [] }, { id: 'C' }],
  [null, { id: 'C' }], [{ autoTimeIgnoredChannelIds: ['C'] }, null],
].map(([s, c]) => [s, c, autotime.isIgnoredChannel(s, c)]);
out.howItWorks = autotimeCmd.HOW_IT_WORKS;
const CHI = 'America/Chicago';
const FRI = new Date('2026-10-16T20:00:00Z');
out.roughRelative = [-2 * 86_400_000, 20_000, 59_999, 60_000, 90_000, 3_600_000 * 4, 86_400_000 * 45, 31_536_000_000 * 2.5, -1].map((ms) => [ms, ts.roughRelative(ms)]);
out.futureDayExample = [[CHI, FRI.getTime()], ['Pacific/Auckland', Date.parse('2026-12-28T20:00:00Z')], ['UTC', Date.parse('2026-02-25T12:00:00Z')]]
  .map(([zone, ms]) => [zone, ms, ts.futureDayExample(zone, new Date(ms))]);
out.suggestWhen = [['fri 7pm', CHI, FRI.getTime()], ['', CHI, FRI.getTime()], ['banana', CHI, FRI.getTime()], ['banana', CHI, Date.parse('2026-12-28T12:00:00Z')],
  ['x'.repeat(60), CHI, Date.parse('2026-11-28T12:00:00Z')], ['7pm cst', 'Europe/London', FRI.getTime()], ['in 2h', 'UTC', FRI.getTime()]]
  .map(([q, zone, ms]) => [q, zone, ms, ts.suggestWhen(q, zone, new Date(ms))]);
const R1 = '111111111111111111';
out.timestampEmbed = [
  ['fri 7pm', CHI, { zone: CHI, source: 'role', roleId: R1, roleCount: 2 }, { style: 'R' }],
  ['fri 7pm', CHI, { zone: CHI, source: 'server', roleId: null, roleCount: 0 }, {}],
  ['7pm cst', 'UTC', { zone: 'UTC', source: 'default', roleId: null, roleCount: 0 }, { style: 'd' }],
  ['tomorrow 9am', 'Asia/Tokyo', { zone: 'Asia/Tokyo', source: 'personal', roleId: null, roleCount: 0 }, { personalUnknown: true, settings: { embedColor: 0xabcdef } }],
  ['fri 7pm', CHI, { zone: CHI, source: 'default', roleId: null, roleCount: 0 }, { personalUnknown: true }],
].map(([q, zone, resolved, opts]) => {
  const parsed = parseWhen(q, zone, { now: FRI });
  return { q, zone, now: FRI.getTime(), resolved, opts, embed: embedJson(ts.buildTimestampEmbed(parsed, resolved, opts)) };
});
const fakeGuild = { roles: { cache: new Map([[R1, { id: R1, position: 3 }]]) } };
out.timezoneRolesLine = [
  [{ timezoneRoles: [{ roleId: R1, zone: CHI }] }], [{ timezoneRoles: [] }], [{ timezoneRoles: [{ roleId: '222222222222222222', zone: CHI }] }],
  [{ timezoneRoles: [{ roleId: R1, zone: CHI }, { roleId: '222222222222222222', zone: 'UTC' }] }],
].map(([s]) => [s, autotimeCmd.timezoneRolesLine(s, fakeGuild)]);
out.userTimezoneKey = userTz.userTimezoneKey('123456789012345678');
out.styles = ts.STYLES;
out.examples = ts.EXAMPLES;

const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-translation');
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, 'golden.json'), `${JSON.stringify(out, null, 1)}\n`);
redis.disconnect();
process.exit(0);
