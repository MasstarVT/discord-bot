// Offline tests for pkg-translation-fixes: no Discord, Postgres, Redis or network. `fetch` is stubbed per
// test and restored afterwards. Run with `node --test test/translation-fixes.test.js`.
import { test, describe, beforeEach, afterEach, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ChannelType, PermissionFlagsBits } from 'discord.js';

process.env.NODE_ENV = 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL ??= 'postgresql://test:test@127.0.0.1:1/test';
// Nothing listens here: cache.* fails fast and returns its fallbacks.
process.env.REDIS_URL = 'redis://127.0.0.1:1';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const svc = await import('../src/services/translationService.js');
const { markBotDeletion } = await import('../src/utils/deletionTracker.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const translateCmd = await import('../src/commands/utility/translate.js');
const autotranslateCmd = await import('../src/commands/admin/autotranslate.js');
const { plugin } = await import('../src/plugins/translation-fixes.js');
const prisma = (await import('../src/database/client.js')).default;
const { LIMITS } = await import('../src/config/constants.js');

const {
  LANGUAGES, TranslationError, TRANSLATE_MAX_CHUNKS, __testing,
} = svc;

const realFetch = globalThis.fetch;
const realDelegates = {
  translateChannel: prisma.translateChannel, guildSettings: prisma.guildSettings, $transaction: prisma.$transaction, $queryRaw: prisma.$queryRaw,
};

after(async () => {
  globalThis.fetch = realFetch;
  Object.assign(prisma, realDelegates);
  Object.assign(console, quiet);
  (await import('../src/services/redis.js')).default.disconnect();
});

beforeEach(() => {
  __testing.reset();
  __testing.configure({ provider: 'google', resultCache: false });
});

afterEach(() => {
  globalThis.fetch = realFetch;
});

// ── Helpers ───────────────────────────────────────────────────────────────────

const jsonRes = (body, status = 200, headers = {}) => new Response(JSON.stringify(body), {
  status, headers: { 'content-type': 'application/json', ...headers },
});

/** Installs a fake fetch; `handler(url, init, n)` returns a Response. Returns the recorded calls. */
function installFetch(handler) {
  const calls = [];
  globalThis.fetch = async (url, init) => {
    const call = { url: String(url), init: init ?? {}, n: calls.length + 1 };
    calls.push(call);
    return handler(call.url, call.init, call.n);
  };
  return calls;
}

const param = (url, name) => new URL(url).searchParams.get(name);
/** The text a Google request carries: the `q` of a GET url or of a POST form body. */
const qOf = (call) => (call.init.method === 'POST'
  ? decodeURIComponent(String(call.init.body).replace(/^q=/, ''))
  : param(call.url, 'q'));
/** Google answer: a fixed translation and a detected source language. */
const googleFake = (detected, translated = 'T') => () => jsonRes([[[translated, 'S']], null, detected]);
const echoFake = (detected) => (url, init) => jsonRes([[[qOf({ url, init }), 'S']], null, detected]);
const charCount = (node) => (node.name?.length ?? 0) + (node.description?.length ?? 0)
  + (node.choices ?? []).reduce((n, c) => n + c.name.length + String(c.value).length, 0)
  + (node.options ?? []).reduce((n, o) => n + charCount(o), 0);

// ── 1. ReDoS (R011, R012) ─────────────────────────────────────────────────────

describe('link-only check', () => {
  test('is linear on hostile input', () => {
    for (const input of ['http://'.repeat(5000) + ' x', 'http://a'.repeat(500) + ' !']) {
      const t0 = performance.now();
      assert.equal(svc.isLinkOnly(input), false);
      assert.ok(performance.now() - t0 < 50, 'took too long');
    }
  });

  test('truth table', () => {
    const table = [
      ['https://a.com https://b.com', true], ['https://a.com\nhttps://b.com', true], ['<https://a.com>', true],
      ['look https://a.com', false], ['https://a.com look', false], ['http://', false], ['', false],
    ];
    for (const [input, expected] of table) assert.equal(svc.isLinkOnly(input), expected, input);
  });

  test('the old regex and the CJK hint are gone from the source', () => {
    const src = readFileSync(resolve(ROOT, 'src/services/translationService.js'), 'utf8');
    assert.ok(!src.includes('(https?:\\/\\/\\S+\\s*)+'));
    assert.ok(!src.includes('hasCjk'));
    assert.equal([...src.matchAll(/zh-CN/g)].length, 1); // only in toGoogleCode
  });
});

// ── 2. Residue (R172, R173) ───────────────────────────────────────────────────

describe('auto-translate input', () => {
  test('emoji, mention, timestamp and link residue is ignored', () => {
    const residue = ['❤️', '👍🏽', '👨‍👩‍👧', '🇯🇵', '🇺🇸🇯🇵', '1️⃣', '<@123456789012345678>', '<@!123456789012345678>',
      '<@&123456789012345678>', '<#123456789012345678>', '<t:1700000000:R>', '@everyone', '<:pepe:123456789012345678>',
      '</ping sub:123456789012345678>', 'k', '42!!', '🏴󠁧󠁢󠁥󠁮󠁧󠁿 k 42!!'];
    for (const s of residue) assert.equal(svc.prepareAutoInput(s), null, JSON.stringify(s));
  });

  test('words survive, single CJK characters count', () => {
    assert.equal(svc.prepareAutoInput('hola amigo ❤️').text, 'hola amigo');
    assert.notEqual(svc.prepareAutoInput('好'), null);
    assert.equal(svc.prepareAutoInput('||secret words||').hadSpoiler, true);
  });
});

// ── 3 / 4. Codes (R050, R176, R177, R179, R211, R212, R286, R287) ─────────────

describe('language codes', () => {
  test('normalizeLangCode', () => {
    const table = {
      iw: 'he', he: 'he', 'zh-CN': 'zh', 'zh-TW': 'zh-tw', 'zh-Hant': 'zh-tw', 'zh-Hant-TW': 'zh-tw', 'zh-HK': 'zh-tw',
      'zh-Hans': 'zh', zt: 'zh-tw', nb: 'no', 'en-US': 'en', 'pt-BR': 'pt', fil: 'tl', ceb: 'ceb',
    };
    for (const [input, expected] of Object.entries(table)) assert.equal(svc.normalizeLangCode(input), expected, input);
    for (const bad of ['auto', 'und', '', null, undefined, 12]) assert.equal(svc.normalizeLangCode(bad), null, String(bad));
  });

  test('names and resolution', () => {
    assert.equal(svc.languageName('he'), 'Hebrew');
    assert.equal(svc.languageName('zh-tw'), 'Chinese (Traditional)');
    assert.equal(svc.languageName('ceb'), 'Cebuano');
    assert.equal(svc.languageName(null), 'Unknown');
    assert.equal(svc.resolveLanguageInput('ZH-TW'), 'zh-tw');
    assert.equal(svc.resolveLanguageInput('zh-Hant'), 'zh-tw');
    assert.equal(svc.resolveLanguageInput('iw'), 'he');
    assert.equal(svc.resolveLanguageInput('nb'), 'no');
    assert.equal(svc.resolveLanguageInput('Japanese'), 'ja');
    assert.equal(svc.resolveLanguageInput('jp'), null);
    assert.equal(svc.suggestLanguage('jp'), 'ja');
    assert.equal(Object.keys(LANGUAGES).length, 74);
    assert.equal(svc.langFlag('zh-tw'), '🇹🇼');
  });

  test('normalizeTargetList removes duplicates', () => {
    assert.deepEqual(svc.normalizeTargetList(['es', 'ES', ' es ', 'iw', 'he', 'xx']), { valid: ['es', 'he'], invalid: ['xx'] });
  });
});

// ── 5. Provider maps (R174, R175) ─────────────────────────────────────────────

test('provider code maps', () => {
  assert.equal(svc.toGoogleCode('zh'), 'zh-CN');
  assert.equal(svc.toGoogleCode('zh-tw'), 'zh-TW');
  assert.equal(svc.toGoogleCode('ja'), 'ja');
  assert.equal(svc.toLtCode('no', new Set(['nb'])), 'nb');
  assert.equal(svc.toLtCode('zh', new Set(['zh-hans'])), 'zh-Hans');
  assert.equal(svc.toLtCode('zh', new Set(['zh'])), 'zh');
  assert.equal(svc.toLtCode('zh-tw', new Set(['zt'])), 'zt');
  assert.equal(svc.toLtCode('he', new Set()), 'he');
});

// ── 6. Sizing (R178) ──────────────────────────────────────────────────────────

describe('request sizing', () => {
  test('splitForRequests keeps every piece small and lossless', () => {
    const input = '今日はとても暑いですね。'.repeat(125);
    const pieces = svc.splitForRequests(input, 6000);
    assert.ok(pieces.length >= 2);
    for (const p of pieces) {
      assert.ok(encodeURIComponent(p.text).length <= 6000);
      assert.ok(p.text.isWellFormed());
    }
    assert.equal(pieces.map((p) => p.text + p.sep).join(''), input);

    const emoji = svc.splitForRequests('😀'.repeat(1500), 6000);
    for (const p of emoji) {
      assert.ok(encodeURIComponent(p.text).length <= 6000);
      assert.ok(p.text.isWellFormed());
    }
    assert.equal(emoji.map((p) => p.text).join(''), '😀'.repeat(1500));
  });

  test('clampForRequest respects both limits', () => {
    const r = svc.clampForRequest('Привет как дела '.repeat(125), { maxCodePoints: 1000, maxEncodedBytes: 6000 });
    assert.equal(r.truncated, true);
    assert.ok(encodeURIComponent(r.text).length <= 6000);
    assert.ok(Array.from(r.text).length <= 1000);
    assert.ok(svc.clampForRequest('a'.repeat(999) + '𠀀'.repeat(5), { maxCodePoints: 1000 }).text.isWellFormed());
    assert.deepEqual(svc.clampForRequest('short', { maxCodePoints: 1000 }), { text: 'short', truncated: false });
  });
});

// ── 7. Embeds (R104, R105, R182, R289) ────────────────────────────────────────

describe('embed formatting', () => {
  test('splitForEmbeds', () => {
    assert.deepEqual(svc.splitForEmbeds('x'.repeat(4096)).map((p) => p.length), [4000, 96]);
    const words = svc.splitForEmbeds('word '.repeat(3000));
    assert.equal(words.length, 3);
    assert.ok(words.every((p) => p.length <= 4000));
    assert.ok(words[2].endsWith('…'));
    assert.ok(svc.splitForEmbeds('😀'.repeat(3000)).every((p) => p.isWellFormed()));
  });

  test('buildAutoTranslateEmbed stays inside Discord limits', () => {
    const long = 'palabra '.repeat(400);
    const results = ['es', 'fr', 'de', 'it', 'pt'].map((target) => ({ target, text: long }));
    const embed = svc.buildAutoTranslateEmbed({ results, detected: 'en' });
    const description = embed.data.description;
    assert.ok(description.length <= 4096);
    assert.ok(description.isWellFormed());
    for (const line of description.split('\n')) assert.ok(line.endsWith('…'), line.slice(-10));
    assert.equal(embed.data.footer.text, 'Detected: English');

    const spoiler = svc.buildAutoTranslateEmbed({ results: [{ target: 'es', text: 'hola **mundo' }, { target: 'fr', text: 'bonjour' }], detected: 'en', hadSpoiler: true });
    for (const line of spoiler.data.description.split('\n')) {
      const body = line.split(' — ')[1];
      assert.ok(body.startsWith('||') && body.endsWith('||'), line);
    }
  });

  test('balanceMarkdown', () => {
    assert.equal(svc.balanceMarkdown('**bold'), '**bold**');
    assert.equal(svc.balanceMarkdown('```code'), '```code\n```');
    assert.equal(svc.balanceMarkdown('`x'), '`x`');
    assert.equal(svc.balanceMarkdown('fine'), 'fine');
  });
});

// ── 8. One request per target, no detect request (R050, R051, R176, R180, R181) ─

describe('translateToTargets (Google)', () => {
  const japanese = '今日はとても暑いですね';

  test('Japanese: [en, ja] is one request with sl=auto', async () => {
    const calls = installFetch(googleFake('ja'));
    const out = await svc.translateToTargets(japanese, ['en', 'ja']);
    assert.equal(calls.length, 1);
    assert.equal(param(calls[0].url, 'sl'), 'auto');
    assert.equal(param(calls[0].url, 'tl'), 'en');
    assert.deepEqual(out.results.map((r) => r.target), ['en']);
    assert.equal(out.detected, 'ja');
  });

  test('Japanese: [ja, en] drops the ja line and reuses the detected source', async () => {
    const calls = installFetch(googleFake('ja'));
    const out = await svc.translateToTargets(japanese, ['ja', 'en']);
    assert.equal(calls.length, 2);
    assert.equal(param(calls[0].url, 'sl'), 'auto');
    assert.equal(param(calls[0].url, 'tl'), 'ja');
    assert.equal(param(calls[1].url, 'sl'), 'ja');
    assert.equal(param(calls[1].url, 'tl'), 'en');
    assert.deepEqual(out.results.map((r) => r.target), ['en']);
    for (const c of calls) assert.ok(!c.url.includes('sl=zh-CN'), 'no Chinese source hint for Japanese');
  });

  test('Hebrew reported as iw is treated as he', async () => {
    const calls = installFetch(googleFake('iw'));
    const out = await svc.translateToTargets('שלום לכולם', ['he', 'en']);
    assert.equal(calls.length, 2);
    assert.deepEqual(out.results.map((r) => r.target), ['en']);
    assert.equal(out.detected, 'he');
    assert.equal(param(calls[1].url, 'sl'), 'he');
  });

  test('Spanish with [en] is one request (was two)', async () => {
    const calls = installFetch(googleFake('es'));
    const out = await svc.translateToTargets('hola a todos', ['en']);
    assert.equal(calls.length, 1);
    assert.equal(out.results.length, 1);
  });

  test('Traditional Chinese is its own target and source code', async () => {
    const calls = installFetch(googleFake('zh-TW'));
    const out = await svc.translateToTargets('今天天氣很好', ['zh-tw', 'en']);
    assert.equal(param(calls[0].url, 'tl'), 'zh-TW');
    assert.equal(out.detected, 'zh-tw');
    assert.equal(param(calls[1].url, 'sl'), 'zh-TW');
  });

  test('Google calling Traditional Chinese "en" does not drop the English target (seen live)', async () => {
    const calls = installFetch(googleFake('en'));
    const out = await svc.translateToTargets('今天天氣很好，我們去公園玩吧', ['en', 'es']);
    assert.equal(calls.length, 2);
    assert.deepEqual(out.results.map((r) => r.target), ['en', 'es']);
    assert.equal(out.detected, 'zh-tw');
    assert.equal(param(calls[1].url, 'sl'), 'zh-TW');
  });

  test('an impossible detection is rejected, a plausible one is kept', async () => {
    installFetch(googleFake('en'));
    assert.equal((await svc.translateText('Привет всем', 'es')).detected, null);
    assert.equal((await svc.translateText('hello there', 'es')).detected, 'en');
    assert.equal((await svc.translateText('I love 寿司', 'es')).detected, 'en');
    assert.equal((await svc.detectLanguage('今天天氣很好，我們去公園玩吧')).code, 'zh-tw');
  });

  test('an echo is dropped', async () => {
    installFetch(echoFake('en'));
    const out = await svc.translateToTargets('same text here', ['es']);
    assert.deepEqual(out.results, []);
  });
});

// ── 9. Breaker (R255) and the client fallback ─────────────────────────────────

describe('circuit breaker', () => {
  test('every client rate-limited trips the breaker and later calls make no request', async () => {
    const calls = installFetch(() => new Response('<html>Sorry...</html>', { status: 429 }));
    await assert.rejects(svc.translateText('hello there', 'es'), { code: 'RATE_LIMITED' });
    assert.equal(calls.length, 1, 'one request, then the breaker trips');
    assert.equal(svc.translationPaused(), true);
    assert.ok(svc.pauseRemainingMs() > 0);

    const before = calls.length;
    const err = await svc.translateText('hello there', 'es').catch((e) => e);
    assert.equal(err.code, 'PAUSED');
    assert.equal(calls.length, before);
    assert.match(svc.describeTranslationError(err).embed.data.title, /Translation Paused/);
  });

  test('translateToTargets stops cleanly on a 429', async () => {
    installFetch(() => new Response('', { status: 429 }));
    const out = await svc.translateToTargets('hello there', ['es', 'fr']);
    assert.equal(out.pausedStop, true);
    assert.deepEqual(out.results, []);
  });

  test('only the gtx client is used; a 429 pauses instead of switching clients', async () => {
    const calls = installFetch(() => new Response('Sorry', { status: 429 }));
    const err = await svc.translateText('hello', 'es').catch((e) => e);
    assert.equal(err.code, 'RATE_LIMITED');
    assert.equal(calls.length, 1);
    assert.match(calls[0].url, /client=gtx/);
    assert.equal(svc.translationPaused(), true);
  });

  test('error mapping', () => {
    const d = (code) => svc.describeTranslationError(new TranslationError(code));
    assert.match(d('UNSUPPORTED_LANGUAGE').embed.data.title, /Language Not Supported/);
    assert.match(d('TIMEOUT').embed.data.title, /Translation Failed/);
    assert.equal(d('NETWORK').log, 'warn');
    assert.equal(svc.describeTranslationError(new Error('boom')).log, 'error');
  });
});

// ── 10. Long input: GET up to 6,000 bytes, POST above (R178) ──────────────────

describe('long input (Google)', () => {
  test('short text is a GET with the text in the query', async () => {
    const calls = installFetch(googleFake('ja'));
    await svc.translateText('今日はとても暑いですね', 'en');
    assert.equal(calls[0].init.method, undefined);
    assert.equal(param(calls[0].url, 'q'), '今日はとても暑いですね');
    assert.ok(calls[0].url.length < 6_300);
  });

  test('1,500 CJK characters are one POST with the text in the body', async () => {
    const input = '好'.repeat(1500);
    const calls = installFetch(googleFake('zh-CN', 'translated'));
    const r = await svc.translateText(input, 'en', { maxChunks: TRANSLATE_MAX_CHUNKS });
    assert.equal(calls.length, 1);
    assert.equal(calls[0].init.method, 'POST');
    assert.ok(calls[0].url.length < 200, 'the URL carries no text');
    assert.equal(qOf(calls[0]), input);
    assert.match(calls[0].init.headers['Content-Type'], /x-www-form-urlencoded/);
    assert.equal(r.text, 'translated');
    assert.equal(r.detected, 'zh');
  });

  test('a refused POST is reported as an HTTP error (no client switching)', async () => {
    const calls = installFetch(() => new Response('bad', { status: 400 }));
    const err = await svc.translateText('好'.repeat(1500), 'en').catch((e) => e);
    assert.equal(err.code, 'HTTP_ERROR');
    assert.equal(calls.length, 1);
  });

  test('text above the POST cap is chunked, and refused when maxChunks is too small', async () => {
    const input = '好'.repeat(3000); // 27,000 encoded bytes
    let n = 0;
    const calls = installFetch(() => jsonRes([[[`T${++n}`, 'S']], null, 'zh-CN']));
    const r = await svc.translateText(input, 'en', { maxChunks: TRANSLATE_MAX_CHUNKS });
    assert.ok(calls.length >= 2 && calls.length <= TRANSLATE_MAX_CHUNKS);
    assert.equal(r.text, Array.from({ length: calls.length }, (_, i) => `T${i + 1}`).join(''));
    for (const c of calls) assert.ok(qOf(c).isWellFormed());

    const before = calls.length;
    await assert.rejects(svc.translateText(input, 'en', { maxChunks: 1 }), { code: 'TOO_LONG' });
    assert.equal(calls.length, before);
  });
});

// ── 11. LibreTranslate (R174, R175) ───────────────────────────────────────────

describe('LibreTranslate', () => {
  const lt = { provider: 'libretranslate', ltUrl: 'http://lt.test', resultCache: false };

  test('language list drives support and code mapping', async () => {
    __testing.configure(lt);
    const posts = [];
    const calls = installFetch((url, init) => {
      if (url.endsWith('/languages')) return jsonRes([{ code: 'en' }, { code: 'nb' }, { code: 'zh-Hans' }, { code: 'zh-Hant' }, { code: 'he' }]);
      posts.push(JSON.parse(init.body));
      return jsonRes({ translatedText: 'x', detectedLanguage: { language: 'zh-Hant' } });
    });
    const support = await svc.loadLanguageSupport();
    assert.deepEqual([...support].sort(), ['en', 'he', 'no', 'zh', 'zh-tw']);
    assert.equal(svc.isSupportedTarget('no'), true);
    assert.equal(svc.isSupportedTarget('cy'), false);
    assert.deepEqual(Object.keys(svc.getLanguages()).sort(), ['en', 'he', 'no', 'zh', 'zh-tw']);

    const r = await svc.translateText('hi', 'no');
    assert.equal(posts[0].target, 'nb');
    assert.equal(posts[0].source, 'auto');
    assert.equal(r.detected, 'zh-tw');
    await svc.translateText('hi', 'zh-tw', { source: 'zh-tw' });
    assert.equal(posts[1].target, 'zh-Hant');
    assert.equal(posts[1].source, 'zh-Hant');

    const before = calls.length;
    await assert.rejects(svc.translateText('hi', 'cy'), { code: 'UNSUPPORTED_LANGUAGE' });
    assert.equal(calls.length, before, 'no request for an unsupported language');
    assert.match(svc.providerLabel(), /LibreTranslate/);
  });

  test('a failing list is not fatal and a runtime rejection is remembered', async () => {
    __testing.configure(lt);
    installFetch((url) => (url.endsWith('/languages')
      ? new Response('down', { status: 500 })
      : jsonRes({ error: 'cy is not supported' }, 400)));
    assert.equal(await svc.loadLanguageSupport(), null);
    assert.equal(svc.isSupportedTarget('cy'), null);
    assert.equal(svc.getLanguages(), LANGUAGES);
    await assert.rejects(svc.translateText('hi', 'cy'), { code: 'UNSUPPORTED_LANGUAGE', lang: 'cy' });
    assert.equal(svc.isSupportedTarget('cy'), false);
  });

  test('the API key is sent and /detect maps the answer', async () => {
    __testing.configure({ ...lt, ltKey: 'secret' });
    const calls = installFetch(() => jsonRes([{ language: 'nb', confidence: 90 }]));
    const found = await svc.detectLanguage('Dette er en norsk setning');
    assert.deepEqual(found, { code: 'no', name: 'Norwegian' });
    assert.equal(JSON.parse(calls[0].init.body).api_key, 'secret');
    assert.match(calls[0].url, /http:\/\/lt\.test\/detect$/);
  });
});

// ── 12. The hook body, fully offline (R011, R183, R184, R264, R265, R288) ─────

describe('handleAutoTranslate', () => {
  let seq = 0;
  const makeMessage = (over = {}) => {
    const replies = [];
    const message = {
      id: `m${++seq}`, channelId: 'c1', content: 'good morning everyone', system: false, author: { bot: false },
      webhookId: null, reply: async (p) => { replies.push(p); }, ...over,
    };
    return { message, replies };
  };
  const ctxFor = (message, over = {}) => ({ guildId: 'g1', message, state: {}, getSettings: async () => null, ...over });

  beforeEach(() => __testing.primeChannelConfig('g1', { c1: ['es', 'en'] }));

  test('one request, one reply, no pings', async () => {
    const calls = installFetch(googleFake('en', 'Buenos días a todos'));
    const { message, replies } = makeMessage();
    await svc.handleAutoTranslate(ctxFor(message), { checkPermissions: false });
    assert.equal(calls.length, 1);
    assert.equal(param(calls[0].url, 'tl'), 'es');
    assert.equal(replies.length, 1);
    assert.deepEqual(replies[0].allowedMentions, { parse: [], repliedUser: false });
    assert.equal(replies[0].failIfNotExists, true);
    assert.equal(replies[0].embeds.length, 1);
    assert.equal(replies[0].embeds[0].data.footer.text, 'Detected: English');
    assert.match(replies[0].embeds[0].data.description, /Buenos días a todos/);
  });

  test('same-language target gets no request and no reply', async () => {
    __testing.primeChannelConfig('g1', { c1: ['es'] });
    const calls = installFetch(googleFake('es'));
    const { message, replies } = makeMessage({ content: 'hola a todos, buenos días' });
    await svc.handleAutoTranslate(ctxFor(message), { checkPermissions: false });
    assert.equal(calls.length, 1);
    assert.equal(replies.length, 0);
  });

  const skipped = {
    'link-only message': { content: 'https://a.com https://b.com' },
    'emoji only': { content: '❤️' },
    'system message': { system: true },
    'bot author': { author: { bot: true } },
    'webhook message': { webhookId: '123' },
    'unconfigured channel': { channelId: 'c2' },
  };
  for (const [label, over] of Object.entries(skipped)) {
    test(`skips: ${label}`, async () => {
      const calls = installFetch(googleFake('en'));
      const { message, replies } = makeMessage(over);
      await svc.handleAutoTranslate(ctxFor(message), { checkPermissions: false });
      assert.equal(calls.length, 0);
      assert.equal(replies.length, 0);
    });
  }

  test('skips a message automod deleted (ctx.state) and one marked as bot-deleted', async () => {
    const calls = installFetch(googleFake('en'));
    const a = makeMessage();
    await svc.handleAutoTranslate(ctxFor(a.message, { state: { automodAction: { rule: 'x', deleted: true } } }), { checkPermissions: false });
    const b = makeMessage();
    markBotDeletion(b.message.id, 'automod');
    await svc.handleAutoTranslate(ctxFor(b.message), { checkPermissions: false });
    assert.equal(calls.length, 0);
    assert.equal(a.replies.length + b.replies.length, 0);
  });

  test('a reply to a deleted message does not throw', async () => {
    installFetch(googleFake('en'));
    const { message } = makeMessage({ reply: async () => { throw Object.assign(new Error('Unknown Message'), { code: 10008 }); } });
    await assert.doesNotReject(svc.handleAutoTranslate(ctxFor(message), { checkPermissions: false }));
  });

  test('a missing-permission channel makes no request', async () => {
    const calls = installFetch(googleFake('en'));
    const { message, replies } = makeMessage({ channel: null });
    await svc.handleAutoTranslate(ctxFor(message));
    assert.equal(calls.length, 0);
    assert.equal(replies.length, 0);
  });

  test('buildAutoReplyPayload never pings', () => {
    const payload = svc.buildAutoReplyPayload(svc.buildAutoTranslateEmbed({ results: [{ target: 'es', text: 'hola' }], detected: 'en' }));
    assert.equal(payload.allowedMentions.repliedUser, false);
    assert.deepEqual(payload.allowedMentions.parse, []);
  });
});

// ── 13. Command modules and payloads ──────────────────────────────────────────

describe('command payloads', () => {
  test('/autotranslate', () => {
    const json = buildCommandPayload(autotranslateCmd);
    assert.deepEqual(json.options.map((o) => o.name), ['enable', 'language1', 'language2', 'language3', 'language4', 'language5', 'channel', 'new_channel']);
    assert.equal(json.options[0].required, true);
    assert.ok(!json.options[1].required, 'language1 is optional so /autotranslate enable:false needs no language');
    assert.ok(charCount(json) <= 550, `${charCount(json)} chars`);
    assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
    assert.equal(autotranslateCmd.module, 'translation');
    assert.equal(autotranslateCmd.gated, false);
    assert.equal(autotranslateCmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
    assert.equal(typeof autotranslateCmd.autocomplete, 'function');
    assert.ok(!autotranslateCmd.allowInDMs);
  });

  test('/translate: text, detect and languages', () => {
    const json = buildCommandPayload(translateCmd);
    assert.equal(json.name, 'translate');
    assert.ok(charCount(json) <= 395, `/${json.name}: ${charCount(json)} > 395`);
    assert.equal(translateCmd.module, null);
    assert.equal(translateCmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
    assert.equal(translateCmd.allowInDMs, true, `/${json.name} keeps working in DMs`);
    assert.equal(json.contexts, undefined, 'registered for DMs too');
    assert.equal(typeof translateCmd.autocomplete, 'function');
    assert.deepEqual(json.options.map((o) => o.name), ['text', 'detect', 'languages']);
    assert.ok(json.options.every((o) => o.type === 1));
    const [text, detect, languages] = json.options;
    assert.deepEqual(text.options.map((o) => [o.name, o.required, o.max_length, Boolean(o.autocomplete)]),
      [['text', true, 1500, false], ['target_language', true, 40, true]]);
    assert.deepEqual(detect.options.map((o) => [o.name, o.required, o.max_length]), [['text', true, 1500]]);
    assert.deepEqual(languages.options ?? [], []);
  });
});

// ── 14. Plugin ────────────────────────────────────────────────────────────────

test('plugin loads cleanly with exactly one hook', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('translation-fixes')), []);
  const mine = plugins.find((p) => p.id === 'translation-fixes');
  assert.ok(mine);
  assert.equal(mine.module, 'translation');
  assert.deepEqual(mine.components, []);
  assert.deepEqual(mine.jobs, []);
  assert.equal(mine.hooks.length, 1);
  const [hook] = mine.hooks;
  assert.equal(hook.event, 'messageCreate');
  assert.equal(hook.id, 'translation-fixes');
  assert.equal(hook.timeoutMs, 45_000);
  assert.equal(plugin.hooks[0].filter({ message: { content: '' } }), false);
  assert.equal(plugin.hooks[0].filter({ message: { content: 'hola' } }), true);
});

// ── 15. Autocomplete ──────────────────────────────────────────────────────────

test('languageChoices', () => {
  const zh = svc.languageChoices('zh').map((c) => c.value);
  assert.deepEqual(zh.slice(0, 2), ['zh', 'zh-tw']);
  assert.ok(svc.languageChoices('trad').some((c) => c.value === 'zh-tw'));
  assert.equal(svc.languageChoices('').length, 25);
  assert.equal(svc.languageChoices('iw')[0].value, 'he');
  for (const c of svc.languageChoices('')) assert.ok(c.name.length <= 100);
});

// ── 16. /translate text, detect and languages against a fake interaction ─────

describe('commands', () => {
  function fakeInteraction(options, { guildId = null, sub = 'text' } = {}) {
    const sent = { reply: [], edit: [], follow: [], deferred: false };
    return {
      sent,
      guildId,
      options: { getString: (name) => options[name] ?? null, getSubcommand: () => sub },
      reply: async (p) => { sent.reply.push(p); },
      deferReply: async () => { sent.deferred = true; },
      editReply: async (p) => { sent.edit.push(p); },
      followUp: async (p) => { sent.follow.push(p); },
    };
  }

  test('/translate text works in a DM and shows source and target', async () => {
    installFetch(googleFake('ja', 'It is very hot today, isn\'t it'));
    const i = fakeInteraction({ text: '今日はとても暑いですね', target_language: 'English' });
    await translateCmd.execute(i);
    assert.equal(i.sent.deferred, true);
    const embed = i.sent.edit[0].embeds[0].data;
    assert.match(embed.description, /very hot today/);
    assert.deepEqual(embed.fields.map((f) => `${f.name}=${f.value}`), ['Source=Japanese', 'Target=English']);
    assert.equal(embed.footer.text, 'Original: 今日はとても暑いですね');
  });

  test('/translate text splits a very long result instead of failing (R104, R105)', async () => {
    installFetch(googleFake('es', 'word '.repeat(2000)));
    const i = fakeInteraction({ text: 'hola '.repeat(250), target_language: 'en' });
    await translateCmd.execute(i);
    assert.equal(i.sent.edit.length, 1);
    assert.equal(i.sent.edit[0].embeds[0].data.title.includes('1/3'), true);
    assert.equal(i.sent.follow.length, 2);
    for (const f of i.sent.follow) assert.ok(f.embeds[0].data.description.length <= 4000);
    assert.deepEqual(i.sent.follow[0].allowedMentions, { parse: [] });
  });

  test('/translate text reports the real error, not a blanket one (R104, R255)', async () => {
    installFetch(() => { throw new TypeError('fetch failed'); });
    const i = fakeInteraction({ text: 'hello there', target_language: 'es' });
    await translateCmd.execute(i);
    assert.match(i.sent.edit[0].embeds[0].data.description, /Could not reach/);

    // Paused: answered before deferring.
    installFetch(() => new Response('', { status: 429 }));
    await svc.translateText('hello there', 'es').catch(() => {});
    const paused = fakeInteraction({ text: 'hello there', target_language: 'es' });
    await translateCmd.execute(paused);
    assert.equal(paused.sent.deferred, false);
    assert.match(paused.sent.reply[0].embeds[0].data.title, /Translation Paused/);
    assert.equal(paused.sent.reply[0].ephemeral, true);
  });

  test('/translate text rejects unknown languages with a hint, and empty text', async () => {
    const calls = installFetch(googleFake('en'));
    const bad = fakeInteraction({ text: 'hello there', target_language: 'jp' });
    await translateCmd.execute(bad);
    assert.match(bad.sent.reply[0].embeds[0].data.description, /Did you mean `ja` \(Japanese\)\?/);
    const empty = fakeInteraction({ text: '❤️ <@123456789012345678>', target_language: 'es' });
    await translateCmd.execute(empty);
    assert.match(empty.sent.reply[0].embeds[0].data.title, /Nothing to Translate/);
    assert.equal(calls.length, 0);
  });

  test('/translate detect shows the language and handles Traditional Chinese codes', async () => {
    installFetch(googleFake('iw'));
    const i = fakeInteraction({ text: 'שלום לכולם' }, { sub: 'detect' });
    await translateCmd.execute(i);
    assert.match(i.sent.edit[0].embeds[0].data.description, /\*\*Hebrew\*\* \(`he`\)/);

    installFetch(googleFake('ceb'));
    const j = fakeInteraction({ text: 'Maayong buntag sa tanan' }, { sub: 'detect' });
    await translateCmd.execute(j);
    assert.match(j.sent.edit[0].embeds[0].data.description, /\*\*Cebuano\*\* \(`ceb`\)/);
    assert.match(j.sent.edit[0].embeds[0].data.footer.text, /Detected only/);
  });

  test('/translate languages lists 74 languages and the aliases', async () => {
    const i = fakeInteraction({}, { sub: 'languages' });
    await translateCmd.execute(i);
    const embed = i.sent.edit[0].embeds[0].data;
    assert.equal(embed.description.split('\n').length, 74);
    assert.match(embed.footer.text, /^74 languages · backend: Google/);
    assert.match(embed.fields[0].value, /`iw`→`he`/);
  });
});

// ── 17. /autotranslate against a fake database (R209-R213) ────────────────────

describe('/autotranslate', () => {
  let db;
  let seq = 0;
  beforeEach(() => {
    const rows = [];
    const settings = new Map();
    const same = (r, k) => r.guildId === k.guildId && r.channelId === k.channelId;
    prisma.translateChannel = {
      deleteMany: async ({ where }) => {
        const before = rows.length;
        for (let i = rows.length - 1; i >= 0; i--) {
          if (rows[i].guildId === where.guildId && (where.channelId === undefined || rows[i].channelId === where.channelId)) rows.splice(i, 1);
        }
        return { count: before - rows.length };
      },
      findMany: async ({ where }) => rows.filter((r) => r.guildId === where.guildId).map((r) => ({ channelId: r.channelId, targetLangs: [...r.targetLangs] })),
      findUnique: async ({ where }) => rows.find((r) => same(r, where.guildId_channelId)) ?? null,
      count: async ({ where }) => rows.filter((r) => r.guildId === where.guildId).length,
      upsert: async ({ where, update, create }) => {
        const existing = rows.find((r) => same(r, where.guildId_channelId));
        if (existing) return Object.assign(existing, update);
        const row = { id: rows.length + 1, ...create };
        rows.push(row);
        return row;
      },
    };
    prisma.guildSettings = {
      findUnique: async ({ where }) => settings.get(where.guildId) ?? null,
      upsert: async ({ where, update, create }) => {
        const row = { guildId: where.guildId, translationEnabled: false, embedColor: null, ...(settings.get(where.guildId) ?? create), ...update };
        settings.set(where.guildId, row);
        return row;
      },
    };
    // saveTranslateChannel runs its cap check and upsert in a transaction (the advisory lock is a no-op here).
    prisma.$transaction = async (fn) => fn(prisma);
    prisma.$queryRaw = async () => [];
    db = { rows, settings };
  });

  function interactionFor({ enable, langs = [], channelId = 'c1', allowed = true, goneChannels = [] }) {
    const guildId = `ga${++seq}`;
    const channel = {
      id: channelId, type: ChannelType.GuildText, isTextBased: () => true, isThread: () => false,
      permissionsFor: () => ({ has: () => allowed }), guild: { members: { me: {} } },
    };
    const sent = [];
    return {
      guildId,
      sent,
      channel,
      guild: {
        available: true,
        channels: {
          cache: new Map([[channelId, channel]]),
          fetch: async (id) => {
            if (goneChannels.includes(id)) throw Object.assign(new Error('Unknown Channel'), { code: 10003 });
            return { id };
          },
        },
      },
      options: {
        getBoolean: () => enable,
        getString: (name) => langs[Number(name.replace('language', '')) - 1] ?? null,
        getChannel: () => null,
        getSubcommandGroup: () => null,
        getSubcommand: () => null,
        get: () => null,
      },
      deferReply: async () => {},
      editReply: async (p) => { sent.push(p.embeds[0].data); },
    };
  }
  const fieldOf = (embed, prefix) => embed.fields?.find((f) => f.name.startsWith(prefix));

  test('turning it off needs no language and ignores a bad one', async () => {
    const i = interactionFor({ enable: false, langs: ['definitely-not-a-language'] });
    await autotranslateCmd.execute(i);
    assert.match(i.sent[0].title, /Not Enabled/);
    assert.equal(fieldOf(i.sent[0], 'Auto-translate channels').value, 'None');
  });

  test('enabling saves canonical, de-duplicated targets and turns the module on', async () => {
    const i = interactionFor({ enable: true, langs: ['es', 'ES', 'iw', 'he', 'Japanese'] });
    await autotranslateCmd.execute(i);
    assert.match(i.sent[0].title, /Auto-Translation Enabled/);
    assert.deepEqual(db.rows[0].targetLangs, ['es', 'he', 'ja']);
    assert.equal(db.settings.get(i.guildId).translationEnabled, true);
    assert.equal(fieldOf(i.sent[0], 'Module').value, 'The **Auto-Translate** module was off and is now on.');
    assert.equal(fieldOf(i.sent[0], 'Auto-translate channels (1)').value, '<#c1> → es, he, ja');

    // turning it off again removes the row and lists nothing
    const off = interactionFor({ enable: false });
    off.guildId = i.guildId;
    off.guild = i.guild;
    await autotranslateCmd.execute(off);
    assert.match(off.sent[0].title, /Disabled/);
    assert.equal(db.rows.length, 0);
    assert.equal(fieldOf(off.sent[0], 'Auto-translate channels').value, 'None');
  });

  test('rejects unknown languages with a hint, no languages, and saves nothing', async () => {
    const bad = interactionFor({ enable: true, langs: ['es', 'jp'] });
    await autotranslateCmd.execute(bad);
    assert.match(bad.sent[0].title, /Invalid Language/);
    assert.match(bad.sent[0].description, /Did you mean `ja` \(Japanese\)\?/);
    const none = interactionFor({ enable: true });
    await autotranslateCmd.execute(none);
    assert.match(none.sent[0].title, /No Languages/);
    assert.equal(db.rows.length, 0);
  });

  test('refuses a channel the bot cannot post in, and enforces the channel cap', async () => {
    const denied = interactionFor({ enable: true, langs: ['es'], allowed: false });
    await autotranslateCmd.execute(denied);
    assert.match(denied.sent[0].title, /Missing Permissions/);
    assert.ok(!denied.sent[0].description.includes('..'), 'no doubled punctuation');
    assert.equal(db.rows.length, 0);

    const full = interactionFor({ enable: true, langs: ['es'], channelId: 'new' });
    for (let n = 0; n < LIMITS.TRANSLATE_CHANNELS; n++) db.rows.push({ id: n, guildId: full.guildId, channelId: `x${n}`, targetLangs: ['es'] });
    await autotranslateCmd.execute(full);
    assert.match(full.sent[0].title, /Limit Reached/);
  });

  test('rows of deleted channels are cleaned up, others are kept', async () => {
    const i = interactionFor({ enable: true, langs: ['fr'], goneChannels: ['gone'] });
    db.rows.push({ id: 90, guildId: i.guildId, channelId: 'gone', targetLangs: ['es'] });
    db.rows.push({ id: 91, guildId: i.guildId, channelId: 'archived', targetLangs: ['de'] });
    await autotranslateCmd.execute(i);
    assert.deepEqual(db.rows.map((r) => r.channelId).sort(), ['archived', 'c1']);
    const list = fieldOf(i.sent[0], 'Auto-translate channels').value;
    assert.ok(!list.includes('gone') && list.includes('archived') && list.includes('c1'));
  });
});
