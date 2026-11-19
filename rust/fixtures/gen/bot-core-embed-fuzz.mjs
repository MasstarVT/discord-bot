// Differential fuzz for the pure template functions of src/utils/customEmbed.js: seeded random templates (legacy
// shapes, Discord message JSON, buttons, awkward URLs, numbers, nesting) go through normalizeTemplate,
// validateTemplate, templateToJson and templateFromJson; random strings through isHttpUrl and parseColor. Writes
// rust/fixtures/golden/bot-core/embed-fuzz.json, replayed by crates/bot-core/tests/embed_fuzz.rs.
// Run from the repo root: node rust/fixtures/gen/bot-core-embed-fuzz.mjs
import { writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const ce = await import(pathToFileURL(join(ROOT, 'src/utils/customEmbed.js')).href);

let seed = 0xc0ffee11;
const rnd = () => {
  seed = (seed * 1103515245 + 12345) >>> 0;
  return seed / 2 ** 32;
};
const pick = (a) => a[Math.floor(rnd() * a.length)];

const URLS = [
  'https://a.b', 'http://x', 'https://', 'https://ex ample.com', 'HTTPS://EXAMPLE.COM/x', 'https://例え.jp', 'https://[::1]/a',
  'http://1.2.3.4:99999', 'https://a.b/ ', 'https://a.b/ ', 'https://a.b/﻿', 'attachment://a.png', 'attachment://a/b.png',
  'attachment://', 'ftp://x.com', '//x.com', 'https:x.com', 'https:\\\\x.com', 'https://x.com#{user}', '{user.avatar}', '{server.icon}',
  'javascript:alert(1)', 'http://%41.com', 'https://a..b', 'http://.', 'https://-.com', 'http://0x7f.1', 'https://x.com:0',
  'https://x.com:65536', 'https://user:pw@x.com', 'http://\u0000x', 'https://x.com/' + 'a'.repeat(2040), 'https://x.com/' + 'é'.repeat(600),
  'http://[1:2::3:4]', 'http://[::ffff:1.2.3.4]', 'https://a.b?q=1', 'http://a_b.com', 'http://a%20b.com', 'https://.com', 'http:// x.com',
  'https://x.com\t', ' https://x.com', 'https://。.jp', 'https://xn--nxasmq6b.com', 'https://xn--a.com', 'http://255.255.255.256',
  'http://1.2.3', 'http://0', 'https://*.com', 'https://a<b.com', 'https://a^b.com', 'https://a|b.com', 'blob:https://a.b/x',
  'https://ß.de', 'https://Ⅷ.com', 'https://a.com:', 'http://@x.com', 'http://x.com:80', 'http:x', 'http:/x', 'mailto:a@b.c',
];
const WORDS = ['hi', 'Hello {user}', '  ', '', 'é', '😀', '​', ' x ', '{server}', 'a'.repeat(250), 'b'.repeat(257), 'c'.repeat(1030),
  'd'.repeat(2050), 'e'.repeat(4097), 'f'.repeat(5000), '<a:x_y:123456789012345678>', '<:ok:12345678901234567>', '❤️', '🇺🇸', '👍🏽', 'not emoji',
  '#fff', '#5865F2', '0x123456', 'red', 'Gray', ' blurple ', '#ggg', '123', 'user_avatar', 'server_icon', 'none'];
const NUMS = [0, 1, -1, 1.5, 16777215, 16777216, 1e21, 1e-7, 123456789, -0, 255, 0.1];

const str = () => (rnd() < 0.4 ? pick(URLS) : pick(WORDS));
function val(depth) {
  const r = rnd();
  if (r < 0.45) return str();
  if (r < 0.6) return pick(NUMS);
  if (r < 0.66) return pick([true, false, null]);
  if (depth <= 0) return str();
  if (r < 0.8) return obj(depth - 1);
  return Array.from({ length: Math.floor(rnd() * 4) }, () => val(depth - 1));
}
const KEYS = ['title', 'description', 'url', 'color', 'author', 'footer', 'thumbnail', 'image', 'fields', 'timestamp', 'name', 'text',
  'iconUrl', 'icon_url', 'value', 'inline', 'label', 'emoji', 'id', 'animated', 'type', 'style', 'components', 'content', 'embed', 'embeds',
  'buttons'];
function obj(depth) {
  const o = {};
  const n = Math.floor(rnd() * 6);
  for (let i = 0; i < n; i++) o[pick(KEYS)] = val(depth);
  return o;
}
function embedLike() {
  const e = {};
  if (rnd() < 0.7) e.title = str();
  if (rnd() < 0.5) e.description = str();
  if (rnd() < 0.3) e.url = pick(URLS);
  if (rnd() < 0.5) e.color = rnd() < 0.5 ? pick(NUMS) : pick(WORDS);
  if (rnd() < 0.3) e.author = rnd() < 0.3 ? str() : { name: str(), iconUrl: pick(URLS), icon_url: pick(URLS), url: rnd() < 0.5 ? pick(URLS) : val(1) };
  if (rnd() < 0.3) e.footer = rnd() < 0.3 ? str() : { text: str(), iconUrl: pick(URLS), icon_url: pick(URLS) };
  if (rnd() < 0.3) e.thumbnail = rnd() < 0.5 ? str() : { url: str() };
  if (rnd() < 0.3) e.image = rnd() < 0.5 ? pick(URLS) : { url: pick(URLS) };
  if (rnd() < 0.3) e.fields = Array.from({ length: Math.floor(rnd() * (rnd() < 0.1 ? 30 : 4)) }, () => (rnd() < 0.8 ? { name: val(0), value: val(0), inline: val(0) } : val(1)));
  if (rnd() < 0.2) e.timestamp = pick([true, false, '', 'x', 0, 5, null]);
  return e;
}
function button() {
  if (rnd() < 0.1) return val(1);
  const b = { label: rnd() < 0.8 ? str() : val(1), url: rnd() < 0.8 ? pick(URLS) : val(1) };
  if (rnd() < 0.4) b.emoji = rnd() < 0.6 ? pick(WORDS) : { id: pick(['123', null, '']), name: pick(['x', '', null, '😀']), animated: rnd() < 0.5 };
  return b;
}
function template() {
  const r = rnd();
  if (r < 0.1) return str();
  if (r < 0.15) return val(2);
  if (r < 0.35) return embedLike();
  const t = {};
  if (rnd() < 0.6) t.content = rnd() < 0.8 ? str() : val(1);
  const e = rnd();
  if (e < 0.35) t.embed = embedLike();
  else if (e < 0.6) t.embeds = rnd() < 0.8 ? [embedLike(), embedLike()] : val(1);
  else if (e < 0.65) t.embed = val(1);
  const b = rnd();
  if (b < 0.4) t.buttons = Array.from({ length: Math.floor(rnd() * (rnd() < 0.1 ? 30 : 4)) }, button);
  else if (b < 0.6) t.components = [{ type: 1, components: Array.from({ length: Math.floor(rnd() * 4) }, () => ({ type: pick([2, 3]), style: pick([1, 5]), ...button() })) }, val(1)];
  if (Object.keys(t).length === 0) t.content = null;
  return t;
}

const out = { templates: [], urls: [], colors: [] };
for (let i = 0; i < 800; i++) {
  const input = template();
  const json = JSON.stringify(input);
  const fromJson = ce.templateFromJson(json);
  out.templates.push({
    input,
    normalized: ce.normalizeTemplate(input),
    json: ce.templateToJson(input),
    validate: ce.validateTemplate(input),
    validateLoose: ce.validateTemplate(input, { requireSomething: false }),
    fromJson: { template: fromJson.template, errors: fromJson.errors },
  });
}
for (const u of URLS) out.urls.push({ u, ok: ce.isHttpUrl(u) });
for (let i = 0; i < 300; i++) {
  const u = pick(URLS) + (rnd() < 0.5 ? pick(['', '/', '?', '#', ' ', 'é', '%', ':', '@', '.']) : '');
  out.urls.push({ u, ok: ce.isHttpUrl(u) });
}
const COLOR_IN = ['#fff', '#FFF', 'fff', '0xfff', '#0xfff', '0X123456', '#12345', '#1234567', ' red ', 'RED', 'Ｒed', 'reD ', ' #abc',
  '#abc\n', 'grey', 'gray', 'black', 'white', 'toString', '__proto__', 'constructor', '#ABCDEF', 'abcdeg', '', ' ', 'İ', '#ıff'];
for (const c of COLOR_IN) out.colors.push({ c, n: ce.parseColor(c) });
writeFileSync(join(ROOT, 'rust', 'fixtures', 'golden', 'bot-core', 'embed-fuzz.json'), `${JSON.stringify(out)}\n`);
process.stdout.write(`templates ${out.templates.length}, urls ${out.urls.length}, colors ${out.colors.length}\n`);
