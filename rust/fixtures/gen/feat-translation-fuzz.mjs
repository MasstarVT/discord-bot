// Writes rust/fixtures/golden/feat-translation/fuzz.json: translationService's text preparation and sizing on generated,
// emoji- and CJK-heavy inputs (fixed seed), as Node 24 answers them: cleanForTranslation, hasTranslatableText,
// isLinkOnly, prepareAutoInput (google and libretranslate), clampForRequest, splitForRequests, splitForEmbeds, isEcho,
// balanceMarkdown, languageChoices, normalizeLangCode, resolveLanguageInput and the private reconcileDetected (copied).
import { writeFileSync, mkdirSync } from 'node:fs';

delete process.env.TRANSLATE_PROVIDER;
process.env.NODE_ENV = 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};
const T = await import('../../../src/services/translationService.js');

const LATIN_LETTER = /\p{Script=Latin}/u;
const HAN_CHAR = /\p{Script=Han}/u;
const KANA_OR_HANGUL = /[\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Hangul}]/u;
function reconcileDetected(text, detected) {
  if (!detected || LATIN_LETTER.test(text) || !/\p{L}/u.test(text)) return detected;
  let script;
  try {
    script = new Intl.Locale(detected).maximize().script;
  } catch {
    return detected;
  }
  if (script !== 'Latn') return detected;
  return HAN_CHAR.test(text) && !KANA_OR_HANGUL.test(text) ? 'zh-tw' : null;
}

let seed = 0x5eed1234;
const rnd = (n) => { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; return (seed >>> 0) % n; };
const pick = (a) => a[rnd(a.length)];
const bits = ['hello', 'world', 'Hola', 'mundo', ' ', ' ', '  ', '\n', '\n\n\n', '\t', '.', '!', '?', '. ', '! ', '? ', '。', '！', '？', '؟ ', '। ',
  '你好', '世界', '漢字', 'こんにちは', 'カタカナ', '한국어', 'Привет', 'مرحبا', 'नमस्ते', 'ไทย', 'é', 'ß', 'İ', 'ǅ',
  '😀', '👍🏽', '👨‍👩‍👧', '🇯🇵', '1️⃣', '#️⃣', '🏴󠁧󠁢󠁳󠁣󠁴󠁿', '🫨', '🪿', '🫎', '❤️', '☺', '©', '™', ' ', '﻿', ' ', '‍', '️',
  '<:pepe:123456789012345678>', '<a:dance:123456789012345678>', '<@123456789012345678>', '<@!123456789012345678>', '<@&123456789012345678>',
  '<#123456789012345678>', '</cmd sub:123456789012345678>', '<t:1700000000:R>', '<t:-5>', '@everyone', '@here', '@heres',
  'https://example.com/a?b=c', '<https://x.test>', 'http://y.test/ü', '||', '||spoiler||', '```', '`', '**', '__', '~~', '*', '_', String.fromCharCode(92), '%', '+', '&'];
const gen = (n) => {
  let s = '';
  for (let i = 0; i < n; i++) s += pick(bits);
  return s;
};
const texts = [];
for (let i = 0; i < 400; i++) texts.push(gen(1 + rnd(12)));
for (let i = 0; i < 30; i++) texts.push(gen(150 + rnd(200)));
for (const n of [999, 1000, 1001]) texts.push('a'.repeat(n - 1) + '😀', 'é'.repeat(n), `${'word '.repeat(Math.floor(n / 5))}x`);
texts.push('中'.repeat(2000), '😀'.repeat(2100), `${'。'.repeat(10)}${'字'.repeat(990)}`);
const wf = (s) => s.toWellFormed();

const out = {
  texts: texts.map((t) => {
    const r = {
      t,
      clean: T.cleanForTranslation(t),
      has: T.hasTranslatableText(t),
      linkOnly: T.isLinkOnly(t),
      autoGoogle: T.prepareAutoInput(t),
    };
    T.__testing.configure({ provider: 'libretranslate' });
    r.autoLt = T.prepareAutoInput(t);
    T.__testing.reset();
    const maxCp = 1 + rnd(1200);
    const maxBytes = rnd(3) === 0 ? null : 3 + rnd(400);
    r.clampArgs = [maxCp, maxBytes];
    r.clamp = T.clampForRequest(t, { maxCodePoints: maxCp, ...(maxBytes === null ? {} : { maxEncodedBytes: maxBytes }) });
    const splitMax = 1 + rnd(120);
    r.splitMax = splitMax;
    r.split = T.splitForRequests(t, splitMax);
    const embMax = 2 + rnd(150);
    const embParts = 1 + rnd(4);
    r.embArgs = [embMax, embParts];
    r.emb = T.splitForEmbeds(t, embMax, embParts);
    r.balance = T.balanceMarkdown(t);
    const other = rnd(2) ? t.toUpperCase().replace(/[.!?]/g, '') : gen(2);
    r.echoWith = other;
    r.echo = T.isEcho(t, other);
    return r;
  }),
  choices: ['', 'e', 'en', 'EN', 'zh', 'zh-cn', 'iw', 'nb', 'chin', 'ese', 'ish', 'trad', 'x', 'ja', 'japanese', '  Ger  ', 'ü', 'norw', 'fil', 'pt-br']
    .map((q) => [q, T.languageChoices(q)]),
  codes: ['en', 'EN_us', 'zh-Hant-TW', 'zh_HK', 'zh-sg', 'iw', 'ji', 'jw', 'in', 'nb', 'nn', 'fil', 'pb', 'pt-BR', 'und', 'auto', 'ceb', 'haw',
    'x', 'abcd', 'en-', 'en-a', 'en-abcdefghi', 'sr-Latn-RS', 'a-b-c-d-e', ' es ', 'ES', 'zt', 'zh-mo', 'zh-hans-cn', 'mni-Mtei', 'Japanese',
    'japanese', 'Chinese (Traditional)', 'farsi', 'Norwegian Bokmål', 'burmese', 'tagalog', 'jp', 'kr']
    .map((c) => [c, T.normalizeLangCode(c), T.resolveLanguageInput(c), T.suggestLanguage(c)]),
  reconcile: [],
};
const sampleTexts = ['你好世界', '漢字かな', '한국어', 'Привет', 'hello', '123 !!', '中文 abc', 'مرحبا', 'ไทย', '😀', ''];
const det = ['en', 'es', 'de', 'fr', 'zh', 'zh-tw', 'ja', 'ko', 'ru', 'ar', 'tl', 'ceb', 'haw', 'sr', 'uz', 'az', 'mn', 'ku', 'pa', 'sd', 'ms',
  'jv', 'su', 'ha', 'yo', 'ig', 'zu', 'so', 'mt', 'eo', 'la', 'co', 'fy', 'lb', 'gd', 'sm', 'mi', 'hmn', 'ny', 'sn', 'st', 'xh', 'ht', 'ga',
  'xx', 'qaa', 'kk', 'tk', 'ky', 'tg', 'ug', 'iu', 'chr', null];
for (const s of sampleTexts) for (const d of det) out.reconcile.push([wf(s), d, reconcileDetected(s, d)]);
mkdirSync('rust/fixtures/golden/feat-translation', { recursive: true });
writeFileSync('rust/fixtures/golden/feat-translation/fuzz.json', `${JSON.stringify(out)}\n`);
process.stdout.write(`wrote ${out.texts.length} texts
`);
process.exit(0);
