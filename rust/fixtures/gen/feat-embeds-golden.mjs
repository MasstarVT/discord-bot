// Reference data for feat-embeds, produced by the JS bot's own embedService / customEmbed code (no servers needed).
// Writes rust/fixtures/golden/feat-embeds/pure.json. Run from the repository root:
//   node rust/fixtures/gen/feat-embeds-golden.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = 'redis://127.0.0.1:1';

const svc = await imp('src', 'services', 'embedService.js');
const ce = await imp('src', 'utils', 'customEmbed.js');
const { buildCustomId } = await imp('src', 'config', 'customIds.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const attempt = (fn) => {
  try {
    return { ok: fn() };
  } catch (err) {
    return { err: err.message, code: err.code ?? null };
  }
};

// ── Names ──
const nameInputs = [
  'My Rules!', '  Welcome  Greeting ', 'Ünïcode', '--x--', '🎉', 'a'.repeat(40), 'welcome-greeting', 'a_b', 'A_B c',
  'Ｆｕｌｌｗｉｄｔｈ', 'ﬁle', 'Ⅻ roman', 'İstanbul', 'straße', 'café\tbar\nbaz', 'x y', 'x y　z', 'ı dotless',
  `${'b'.repeat(31)}-c`, `${'b'.repeat(31)}--c`, '-_-', '____', '日本語', 'Ω omega', 'Å', 'ǅ', '́a', 'é', '1/2 ½',
  'mixed-CASE_name 99', '', '   ', '- -', 'tab\tsep', 'a-'.repeat(20), '​zero', 'kK', 'ŉ', 'ﬀ', '①②',
];
const names = nameInputs.map((s) => [s, svc.normalizeEmbedName(s)]);

// ── Emoji ──
const emojiInputs = [
  '🎉', '🛡️', '1️⃣', '1⃣', '#️⃣', '*️⃣', '🇺🇸', '👍🏽', '👨‍👩‍👧', '<:x:123456789012345678>', '<a:wave:123456789012345678>',
  'abc', ':smile:', '🎉🎉', 'a🎉', '', '1', '🇺', '🇺🇸🇫🇷', '🇺🇸🎉', '👍🏽👍', '👨‍👩‍👧‍👦', '🏳️‍🌈', '🏴‍☠️', '❤️', '❤', '©', '©️',
  '™️', '☺️', ' 🎉 ', '🎉‍', '‍🎉', '🎉️️', '🏽', '👍‍🏽', '1️⃣🎉', '1️⃣‍🎉', '🇺🇸‍🎉',
  '🎉‍‍🎉', '🎉️‍🎉', '🎉🏽‍🎉', '👨‍👩‍👧‍👦👨‍👩‍👧‍👦', '🧑🏽‍🤝‍🧑🏻', '<:x:1234>', '<:toolong' + 'x'.repeat(30) + ':123456789012345678>',
  '<:a:123456789012345678>', '<b:x:123456789012345678>', '<:x:123456789012345678', '🎉'.repeat(9), '👨‍'.repeat(8) + '👨',
  '🫠', '🪿', '\u{1F000}', '🀄', '#', '*', '0️⃣', '9️⃣', '🔟', 'x️⃣',
];
const emoji = emojiInputs.map((s) => [s, svc.isEmojiSyntax(s)]);
const buttonEmoji = ['<:x:123456789012345678>', 'hello', ' 🎉 '].map((s) => [s, attempt(() => svc.parseButtonEmoji(s))]);

// ── URLs ──
const urlInputs = ['https://a.b/c.png', ' {user.avatar} ', '{server.icon}', 'javascript:alert(1)', '{user.avatar} x', '', 'http://x.y/a',
  'ftp://x.y', '{user.avatar}', `https://a.b/${'a'.repeat(600)}`, 'HTTPS://A.B/x', 'https://', 'https:// space', '{a-b}', '{}', 'https://a.b/' + 'a'.repeat(2040)];
const imageUrls = urlInputs.map((s) => [s, attempt(() => svc.checkImageUrl(s))]);
const linkUrls = urlInputs.map((s) => [s, attempt(() => svc.checkLinkUrl(s)), attempt(() => svc.checkLinkUrl(s, 512))]);

// ── Validation ──
const validateInputs = [
  ce.emptyTemplate(), { content: 'hi' }, { content: 'x'.repeat(2001) },
  { content: 'hi', buttons: [{ label: 'L', url: 'https://a.b', emoji: 'not an emoji' }] },
  { content: 'hi', buttons: [{ label: 'L', url: `https://a.b/${'a'.repeat(510)}` }] },
  { content: 'hi', buttons: [{ label: 'L', url: '{server.icon}' }] },
  { content: 'hi', buttons: [{ label: 'L', url: 'HTTP://a.b' }, { label: 'M', url: 'https://a.b', emoji: '<:x:1>' }] },
  { embed: { title: 'T', color: 0x123456 }, buttons: [{ label: 'L', url: 'https://a.b', emoji: '🎉' }] },
  { embed: { color: 5 } },
];
const validate = validateInputs.map((t) => [t, svc.validateForSave(t)]);

// ── Mutators ──
const two = svc.addField(svc.addField(ce.emptyTemplate(), { title: 'one', value: '1' }), { title: 'two', value: '2' });
const withAuthor = svc.setAuthor(ce.emptyTemplate(), { text: 'X' });
const footer = svc.setFooter(ce.emptyTemplate(), { text: 'Foot', iconUrl: 'https://a.b/f.png' });
const imgs = svc.setImages(ce.emptyTemplate(), { image: 'https://a.b/i.png', thumbnail: '{server.icon}' });
const site = svc.addButton(ce.emptyTemplate(), { label: ' Site ', url: 'https://example.com', emoji: '🌐' });
const mutatorCases = [
  ['addField', ce.emptyTemplate(), { title: 'A', value: 'B' }],
  ['addField', two, { title: 'zero', value: '0', position: 1 }],
  ['addField', two, { title: 'last', value: 'L', position: 99 }],
  ['addField', two, { title: 'mid', value: 'M', position: 2, inline: true }],
  ['addField', two, { title: 'neg', value: 'N', position: -5 }],
  ['addField', two, { title: '  ', value: 'y' }],
  ['addField', { content: 'x', embed: { color: 5 } }, { title: ' T ', value: ' V ' }],
  ['editField', two, 3, { title: 'x' }],
  ['editField', two, 0, { title: 'x' }],
  ['editField', two, 2, { position: 1, inline: true }],
  ['editField', two, 1, { value: ' new ' }],
  ['editField', two, 1, { title: '' }],
  ['editField', two, 1, { position: 9 }],
  ['removeField', two, 1],
  ['removeField', two, 5],
  ['removeField', ce.emptyTemplate(), 1],
  ['addButton', ce.emptyTemplate(), { label: ' Site ', url: 'https://example.com', emoji: '🌐' }],
  ['addButton', ce.emptyTemplate(), { label: 'x', url: 'ftp://x.y' }],
  ['addButton', ce.emptyTemplate(), { label: 'x', url: `https://a.b/${'a'.repeat(510)}` }],
  ['addButton', ce.emptyTemplate(), { label: '  ', url: 'https://a.b' }],
  ['addButton', site, { label: 'First', url: 'https://a.b', position: 1 }],
  ['removeButton', site, 1],
  ['removeButton', site, 2],
  ['setAuthor', ce.emptyTemplate(), { iconUrl: 'https://a.b/i.png' }],
  ['setAuthor', ce.emptyTemplate(), { text: 'X' }],
  ['setAuthor', withAuthor, { text: 'none' }],
  ['setAuthor', withAuthor, { iconUrl: 'https://a.b/i.png' }],
  ['setAuthor', svc.setAuthor(withAuthor, { iconUrl: 'https://a.b/i.png', url: 'https://a.b' }), { iconUrl: null }],
  ['setAuthor', withAuthor, { text: '   ' }],
  ['setFooter', withAuthor, { timestamp: true }],
  ['setFooter', ce.emptyTemplate(), { text: 'Foot', iconUrl: 'https://a.b/f.png' }],
  ['setFooter', footer, { text: 'clear' }],
  ['setFooter', footer, { iconUrl: null }],
  ['setFooter', footer, { text: ' New ', timestamp: false }],
  ['setFooter', ce.emptyTemplate(), { iconUrl: 'https://a.b/f.png' }],
  ['setFooter', ce.emptyTemplate(), { text: ' ' }],
  ['setImages', ce.emptyTemplate(), { image: 'https://a.b/i.png', thumbnail: '{server.icon}' }],
  ['setImages', imgs, { image: null }],
  ['setImages', imgs, {}],
  ['setColor', ce.emptyTemplate(), 0xff0000],
  ['setColor', imgs, null],
  ['setTitleUrl', ce.emptyTemplate(), 'https://a.b'],
  ['setTitleUrl', imgs, null],
];
const mutators = mutatorCases.map(([fn, tpl, ...args]) => {
  const out = attempt(() => svc[fn](tpl, ...args));
  return { fn, tpl, args, ...out, normalized: out.ok ? ce.normalizeTemplate(out.ok) : null };
});

// ── Import ──
const EXAMPLE = {
  content: 'Hey {user}, here are the rules!',
  embed: {
    title: '📜 Rules', description: '**1.** Be nice.', url: null, color: 5793266,
    author: { name: '{server}', iconUrl: '{server.icon}', url: null },
    footer: { text: 'Thanks for reading', iconUrl: null },
    thumbnail: '{server.icon}', image: null,
    fields: [{ name: 'Contact', value: 'Ask a moderator', inline: false }],
    timestamp: false,
  },
  buttons: [{ label: 'Website', url: 'https://example.com', emoji: '🌐' }],
};
const importInputs = [
  JSON.stringify(EXAMPLE),
  JSON.stringify(ce.normalizeTemplate(EXAMPLE), null, 2),
  JSON.stringify({ content: 'hi', embeds: [{ title: 'T' }, { title: 'U' }], components: [{ type: 1, components: [
    { type: 2, style: 5, label: 'L', url: 'https://x.y' }, { type: 2, style: 1, label: 'B', custom_id: 'z' }] }] }),
  JSON.stringify({ components: [{ type: 1, components: [{ type: 2, style: 5, label: 'L', url: 'https://x.y', emoji: { id: '123456789012345678', name: 'wave', animated: true } }] }] }),
  JSON.stringify({ components: [{ type: 1, components: [{ type: 2, style: 5, label: 'L', url: 'https://x.y', emoji: { name: '🎉' } }] }] }),
  JSON.stringify({ components: [{ type: 1, components: [{ type: 2, style: 5, url: 'https://x.y' }] }] }),
  JSON.stringify({ embeds: [{ title: 'T', color: 255, footer: { text: 'f', icon_url: 'https://a.b/i.png' }, author: { name: 'a', url: 'https://a.b' }, fields: [{ name: 'n', value: 'v', inline: true }], thumbnail: { url: 'https://a.b/t.png' }, image: { url: 'https://a.b/i.png' }, timestamp: '2026-01-01T00:00:00.000Z' }] }),
  JSON.stringify({ embeds: [], content: 'only' }),
  JSON.stringify({ embeds: [{ title: 'T' }], embed: { title: 'E' } }),
  JSON.stringify({ messages: [{ data: { content: 'x' } }, { data: { content: 'y' } }] }),
  JSON.stringify({ messages: [{ data: { content: 'x' } }] }),
  JSON.stringify({ messages: [] }),
  JSON.stringify({ messages: [{}] }),
  '{', '{}', ' '.repeat(20_481), '[1,2]', '"just text"', 'null', '42', 'true',
  JSON.stringify({ content: 'x'.repeat(2001) }),
  JSON.stringify({ title: 'Bare', description: 'embed' }),
  JSON.stringify({ content: 'hi', buttons: [{ label: 'L', url: 'https://a.b', emoji: 'bad' }] }),
  JSON.stringify({ content: 'é'.repeat(10_300) }),
];
const imports = importInputs.map((t) => [t, svc.parseImportPayload(t)]);

// ── Presets ──
const presets = {
  default: svc.buildWelcomePresets(),
  invite: svc.buildWelcomePresets({ inviteUrl: 'https://discord.gg/abc', modRoleIds: ['1', '2'] }),
  many: svc.buildWelcomePresets({ modRoleIds: Array.from({ length: 200 }, (_, i) => `${100000000000000000n + BigInt(i)}`) }),
};

// ── Hints, keys, ids ──
const misc = {
  cacheKey: svc.embedCacheKey('900000000000000001', 'my-rules'),
  formatErrors: svc.formatErrors(['a', 'b', 'a', ...Array.from({ length: 12 }, (_, i) => `e${i}`)]),
  publishHint: [svc.publishHint({ name: 'a', channelId: null, messageId: null }), svc.publishHint({ name: 'rules', channelId: '5', messageId: '6' }), svc.publishHint({ name: 'r', channelId: '5', messageId: null })],
  customIds: [buildCustomId('embed-modal', 'new', 'rules'), buildCustomId('embed-modal', 'new', 'a'.repeat(32)), buildCustomId('embed-modal', 'cm1234567890abcdefghijklm')],
  roleMentions: [
    ['<@&123456789012345678> hi <@&123456789012345678> <@&223456789012345678> @everyone @here', svc.extractRoleMentionIds('<@&123456789012345678> hi <@&123456789012345678> <@&223456789012345678> @everyone @here')],
    ['<@&1234> <@&123456789012345678901> <@&12345678901234567> <<@&123456789012345678>>', svc.extractRoleMentionIds('<@&1234> <@&123456789012345678901> <@&12345678901234567> <<@&123456789012345678>>')],
    ['', svc.extractRoleMentionIds(null)],
    ['many', svc.extractRoleMentionIds(Array.from({ length: 120 }, (_, i) => `<@&${100000000000000000n + BigInt(i)}>`).join(' ')).length],
  ],
  defaultColor: [svc.resolveDefaultColor({ embedColor: 0xff0000 }), svc.resolveDefaultColor(null), svc.resolveDefaultColor({ embedColor: -1 }), svc.resolveDefaultColor({ embedColor: 0x1000000 }), svc.resolveDefaultColor({ embedColor: 0 })],
  mentionNone: svc.mentionPolicy('<@&123456789012345678>', 'none'),
  mentionNoChannel: svc.mentionPolicy('<@&123456789012345678>', 'publish'),
  unknownNoCtx: [
    svc.collectUnknownTokens({ content: 'Hi {server}', embed: { title: '{nope} and {server.name}', description: '{nope} {random:1:5}' }, buttons: [{ label: '{btn}', url: 'https://a.b' }] }, {}),
    svc.collectUnknownTokens({ content: Array.from({ length: 30 }, (_, i) => `{x${i}}`).join(' ') }, {}).length,
    svc.unknownVariablesNote({ content: 'a {nope} {zip}', embed: { fields: [{ name: '{f}', value: '{v}' }], author: { name: '{au}' }, footer: { text: '{fo}' } } }, {}),
  ],
  modals: [
    ce.buildTemplateModal({ customId: buildCustomId('embed-modal', 'new', 'rules'), title: 'New embed: rules', template: ce.emptyTemplate() }).toJSON(),
    ce.buildTemplateModal({ customId: buildCustomId('embed-modal', 'cid'), title: 'Edit embed: rules', template: ce.normalizeTemplate(EXAMPLE), overflow: 'throw' }).toJSON(),
  ],
  draft: ce.templateToJson({ content: 'keep me', embed: { title: 'T' } }),
  render: (() => {
    const r = svc.renderSavedEmbed({ content: 'Hello <@&123456789012345678>', embed: { title: 'Welcome', fields: [{ name: 'a', value: 'b' }] }, buttons: [{ label: 'L', url: 'https://a.b', emoji: '🎉' }] }, { member: null, guild: null, channel: null, settings: { embedColor: 0x112233 } });
    return { content: r.content, embeds: r.embeds.map((e) => e.toJSON()), components: r.components.map((c) => c.toJSON?.() ?? c), allowedMentions: r.allowedMentions };
  })(),
};

const out = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-embeds', 'pure.json');
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, `${JSON.stringify({ names, emoji, buttonEmoji, imageUrls, linkUrls, validate, mutators, imports, presets, misc }, null, 2)}\n`);
console.log(`wrote ${out}`);
redis.disconnect();
process.exit(0);
