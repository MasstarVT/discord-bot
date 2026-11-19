// Reference data for feat-community-b (Modmail, Server Boosts, Auto Threads): the JS bot's own pure functions run over
// sweeps of inputs. Writes rust/fixtures/golden/feat-community-b/pure.json ({ name: [{ in, out }] }), which the Rust
// tests read back. No servers needed.
//
//   node rust/fixtures/gen/feat-community-b-golden.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};

const { MessageFlags, MessageFlagsBitField, MessageType, MessageReferenceType } = await import('discord.js');
const mm = await imp('src', 'services', 'modmailService.js');
const mmp = await imp('src', 'plugins', 'modmail.js');
const bs = await imp('src', 'services', 'boostService.js');
const at = await imp('src', 'services', 'autoThreadService.js');
const { buildCustomId } = await imp('src', 'config', 'customIds.js');
const { default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');

const out = {};
const sweep = (name, inputs, fn) => { out[name] = inputs.map((i) => ({ in: i, out: fn(i) })); };
const embed = (e) => { const j = e.toJSON(); return { title: j.title ?? null, description: j.description ?? null, color: j.color ?? null, footer: j.footer?.text ?? null, author: j.author?.name ?? null, fields: (j.fields ?? []).map((f) => [f.name, f.value]) }; };
const payload = (p) => ({ embeds: (p.embeds ?? []).map(embed), components: (p.components ?? []).map((r) => r.toJSON()) });

// ── Modmail ───────────────────────────────────────────────────────────────────
const texts = ['// ask the owner', '   //note', 'see https://x.y // fine', 'hello', '', ' // nbsp note', '\n//x', '/ / no', '///', 'x//'];
sweep('mm.isInternalNote', texts, (s) => mm.isInternalNote(s));
sweep('mm.threadName', ['Cool Kid!', '!!!', 'a'.repeat(300), 'Ünïcödé User', 'abc_def.ghi', '   ', 'José Ñandú 42', 'emoji😀name', 'UPPER lower'], (u) => mm.threadName({ username: u }));
const relayInputs = [
  { content: 'x'.repeat(5000), attachments: [['cat [1].png', 'https://cdn.example/cat.png']], stickers: ['wave'] },
  { content: '  ', attachments: [], stickers: [] },
  { content: '', attachments: [['a', 'https://x/a']], stickers: [] },
  { content: 'hi', attachments: [], stickers: [] },
  { content: '', attachments: [['   ', 'https://x/b'], [null, 'https://x/c'], ['n*a_m~e|<x>`\\', 'https://x/d'], ['y'.repeat(90), 'https://x/e']], stickers: [null, '[s]'] },
  { content: ' padded text ', attachments: Array.from({ length: 40 }, (_, i) => [`file-${i}-${'z'.repeat(30)}.png`, `https://cdn.example/${i}/${'q'.repeat(20)}`]), stickers: [] },
  { content: '', attachments: [['nourl', '']], stickers: [] },
];
const toMsg = (i) => ({
  content: i.content,
  attachments: new Map(i.attachments.map(([name, url], k) => [String(k), { name, url }])),
  stickers: new Map(i.stickers.map((name, k) => [String(k), { name }])),
});
sweep('mm.relay', relayInputs, (i) => {
  const m = toMsg(i);
  return { lines: mm.attachmentLines(m), parts: mm.relayParts(m), has: mm.hasContent(m) };
});
sweep('mm.whoLines', [['U1', 1_600_000_000_123, 1_700_000_000_999], ['U2', null, null], ['U3', 1_500_000_000_000, null]], ([id, c, j]) => mm.whoLines({ id, createdTimestamp: c }, j ? { joinedTimestamp: j } : null));
sweep('mm.staffAuthor', [['sam', false], ['sam', true], [null, false], ['n'.repeat(150), false]], ([name, anon]) => mm.staffAuthor(name === null ? null : { displayName: name }, { iconURL: () => 'icon' }, anon).name);
sweep('mm.closedEmbed', [['My *Server*', null], ['Plain', 'because'], ['G_u_i_l_d', 'r'.repeat(600)]], ([n, r]) => embed(mmp.closedEmbed({ name: n }, r)));
sweep('mm.confirmPayload', [['900000000000000001', 'Cool *Guild*', '700000000000000001']], ([id, name, mid]) => payload(mmp.confirmPayload({ id, name }, mid)));
const servers = (n) => Array.from({ length: n }, (_, i) => ({ guild: { id: `9000000000000000${String(i).padStart(2, '0')}`, name: i === 1 ? '' : `Server ${i} ${'x'.repeat(i === 2 ? 120 : 0)}` } }));
sweep('mm.openPickPayload', [2, 30], (n) => payload(mmp.openPickPayload(servers(n), '700000000000000002')));
sweep('mm.relayPickPayload', [3], (n) => payload(mmp.relayPickPayload(servers(n).map((s, i) => ({ guild: s.guild, row: { id: `row${i}` } })), '700000000000000003')));
sweep('mm.memberMessageEmbed', relayInputs.slice(0, 5), (i) => embed(mmp.memberMessageEmbed({ embedColor: null }, { id: 'U9', displayName: 'Dee', username: 'dee', displayAvatarURL: () => 'https://cdn.example/av.png' }, toMsg(i))));
sweep('mm.staffMessageEmbed', relayInputs.slice(0, 5), (i) => embed(mmp.staffMessageEmbed({ embedColor: 0x123456 }, { name: 'The Guild', iconURL: () => null }, { name: 'Staff', iconURL: null }, toMsg(i))));
sweep('mm.customIds', [['modmail', 'open', '900000000000000001', '700000000000000001'], ['modmail', 'cancel'], ['modmail', 'close', 'clxyz123'], ['modmail-select', 'open', '7001'], ['modmail-select', 'relay', '7002'], ['autothread', 'close', '600000000000000001']], (a) => buildCustomId(...a));
out['mm.constants'] = [{ in: null, out: { DEFAULT_GREETING: mm.DEFAULT_GREETING, STAFF_HELP: mm.STAFF_HELP, MODMAIL: mm.MODMAIL } }];

// ── Boosts ────────────────────────────────────────────────────────────────────
sweep('bs.parseColor', ['#ff8800', 'FF8800', ' #000000 ', null, '', '#fff', 'red', '#gg0000', '#ff88001', '0xff8800', '#FFFFFF', '  ', 'ABCDEF'], (c) => bs.parseColor(c));
sweep('bs.parseEmoji', ['💎', '👍🏽', '🇩🇪', '1️⃣', '❤️', '👨‍👩‍👧', '', 'ab', '💎💎', '<:gem:123456789>', '💎 hi', 'x'.repeat(40), '#️⃣', '*⃣', '🏴󠁧󠁢󠁳󠁣󠁴󠁿', '🇩', '☺', '©️', ' 💎 ', '👩🏽‍💻', '🧑‍🤝‍🧑', '1', '™', '🫠'], (e) => bs.parseEmoji(e));
const roleNames = ['Moderator', 'Admin Team', '@everyone', 'VIP ★'];
const fakeRoles = new Map(roleNames.map((n, i) => [`R${i}`, { id: `R${i}`, name: n }]));
const guild = { roles: { cache: fakeRoles } };
const nameInputs = ['  Cool   Booster  ', 'a​b', '', '   ​ ', 'x'.repeat(50), 'x'.repeat(51), '@everyone', 'hi @here', 'mail@x', 'a＠b', 'a﹫b', 'https://x.io', 'www.example.net', 'discord.gg/abc', 'discord.com/invite/abc', 'join x.com now', 'moderator', 'MODERATOR', ' Moderator ', 'Mod erator', 'M.o.d.e.r.a.t.o.r', 'Ｍｏｄｅｒａｔｏｒ', 'admin  team', 'Admin-Team', 'VIP', 'vip ☆', 'Café', 'CAFÉ', 'ﬁre', 'İstanbul', 'Σίσυφος', 'tab\there', 'line\nbreak', 'ok.name', 'site.org', 'x.app', 'MyDiscordGG', '­­soft', '😀😀', '🎉 Party'];
sweep('bs.validateRoleName', nameInputs.flatMap((n) => [[n, null], [n, 'R0']]), ([n, except]) => bs.validateRoleName(n, guild, { exceptRoleId: except }));
const DAY = bs.BOOST.DAY_MS;
const now = 10 * DAY;
const planInputs = [];
for (const ended of [null, 1, 2, 5, 9, 0]) for (const member of ['unknown', 'left', 'boosting', 'idle']) for (const grace of [0, 2, 30, 99, -1]) planInputs.push({ ended, member, grace });
sweep('bs.planSweep', planInputs, ({ ended, member, grace }) => {
  const m = member === 'unknown' ? undefined : member === 'left' ? null : { premiumSinceTimestamp: member === 'boosting' ? 1 : null };
  return bs.planSweep({ boostEndedAt: ended === null ? null : new Date(now - ended * DAY) }, m, { boostRoleGraceDays: grace }, now);
});
sweep('bs.graceMs', [0, 3, 30, 31, 99, -4], (d) => bs.graceMs({ boostRoleGraceDays: d }));
sweep('bs.messageTemplate', [null, '   ', 'hi', '\n\t', ' x '], (m) => bs.messageTemplate({ boostMessage: m }));
const thankInputs = [
  { msg: null, boosts: 14, tier: 2, preview: false, max: 2000 },
  { msg: '{user.id} / {boosts} / {tier} / {server}', boosts: 14, tier: 2, preview: false, max: 2000 },
  { msg: 'x'.repeat(5000), boosts: 1, tier: 0, preview: false, max: 2000 },
  { msg: '{unknownthing}', boosts: 1, tier: 0, preview: false, max: 2000 },
  { msg: '{boosts}', boosts: 14, tier: 2, preview: true, max: 2000 },
  { msg: '{boosts}', boosts: 0, tier: 0, preview: false, max: 2000 },
  { msg: null, boosts: 0, tier: 0, preview: true, max: 1500 },
  { msg: '   {tier}   ', boosts: 3, tier: 1, preview: false, max: 2000 },
  { msg: '{user.name} {user.mention} {server.members}', boosts: 3, tier: 1, preview: false, max: 2000 },
];
sweep('bs.renderThankYou', thankInputs, (i) => {
  const g = { id: 'G1', name: 'Test Guild', premiumSubscriptionCount: i.boosts, premiumTier: i.tier, memberCount: 42 };
  const user = { id: 'U1', username: 'booster', bot: false };
  const member = { id: 'U1', displayName: 'Boo', user, guild: g };
  return bs.renderThankYou({ boostMessage: i.msg }, member, g, { maxLength: i.max, preview: i.preview });
});
const codes = ['SETTINGS', 'MODULE_OFF', 'ROLES_OFF', 'NOT_BOOSTING', 'ALREADY', 'NO_ROLE', 'COOLDOWN', 'BAD_NAME', 'BAD_COLOR', 'BAD_EMOJI', 'BAD_ICON', 'ICON_BOTH', 'NO_ROLE_ICONS', 'NOTHING', 'BOT_PERMS', 'BOT_LOW', 'ANCHOR_HIGH', 'ROLE_LIMIT', 'BOT_ROLE', 'DISCORD', 'FAILED', 'WHATEVER'];
const details = { COOLDOWN: [{ ms: 0 }, { ms: 1 }, { ms: 59_001 }, { ms: 60_000 }], BAD_NAME: ['empty', 'too-long', 'at', 'link', 'taken', 'odd'].map((reason) => ({ reason })), BAD_ICON: [{ text: 'Too big.' }, null], DISCORD: [{ text: 'Nope.' }, null] };
sweep('bs.problemEmbed', codes.flatMap((code) => (details[code] ?? [null]).map((detail) => ({ code, detail }))), (r) => embed(bs.problemEmbed(r)));
out['bs.constants'] = [{ in: null, out: { DEFAULT_BOOST_MESSAGE: bs.DEFAULT_BOOST_MESSAGE, BOOST: bs.BOOST } }];

// ── Auto Threads ──────────────────────────────────────────────────────────────
const entries = [
  { channelId: '123456789012345678', name: ' Topic ', archiveMinutes: '60', closeButton: true },
  { channelId: '123456789012345678', name: '', archiveMinutes: 60, closeButton: 'yes' },
  { channelId: '1234', name: 'x' },
  { channelId: 123456789012345678 },
  { channelId: '123456789012345679', name: 'n'.repeat(150), archiveMinutes: '7' },
  null, 'str', [], { channelId: '1234567890123456789012345' }, { channelId: '12345678901234567890123456' },
  { channelId: '12345', archiveMinutes: ['10080'] }, { channelId: '12345', archiveMinutes: 4320.0, name: 7 },
];
sweep('at.normalizeEntry', entries, (e) => at.normalizeEntry(e));
const many = Array.from({ length: 30 }, (_, i) => ({ channelId: `10000000000000000${String(i).padStart(2, '0')}` }));
sweep('at.readEntries', [entries, [...entries, ...many], null, 'x'], (l) => at.readEntries({ autoThreadChannels: l }));
sweep('at.readPublishIds', [['123456789012345678', '123456789012345678', 'x', '1234'], many.map((m) => m.channelId), []], (l) => at.readPublishIds({ autoPublishChannelIds: l }));
const plain = [
  '**Bold** and *italic* and ~~gone~~ and __under__', 'use `code` here', '[the docs](https://example.com/x) please', '# Heading text', '> quoted words',
  '-# small print', '- a bullet', 'hey <@123456789012345678> and <@&123456789012345678> in <#123456789012345678>', 'hello @everyone and @here',
  'hello @​everyone', 'thanks <:pog:123456789012345678> <a:wave:123456789012345678>', 'the answer is ||a big spoiler|| ok', 'mail me: a@b.com',
  '@Bob can you look #general', 'snake_case_name stays', 'tab\tand\u0000control', 'x'.repeat(5000), '```js\ncode```', '_lone italic_ here', '(_paren_)',
  '<t:1700000000:R> later', '</ping:123456789012345678> cmd', 'a b c', '>>> deep quote', '### three', '#### four', '* star bullet', '+ plus',
  '~~a~~~~b~~', '**unclosed', 'emoji 😀 ok', '  spaced   out  ', 'é́ combining', '​zero', '[a]( <https://x.y> )', 'x <y> z', 'back\\slash', '|pipe|',
];
sweep('at.plainText', plain, (s) => at.plainText(s));
const titles = ['First line\nSecond line', '\n\n  \n**bold**\nnext', '<@123456789012345678>\nreal text', '😀😀', '', 'line one\r\nline two', '\n\n\n\n\nsixth', '-\n—\n…\nok', 'Ünïcode words'];
sweep('at.contentTitle', titles, (s) => at.contentTitle(s));
const nameCases = [
  ['{content}', { content: 'How do I reset?\nmore' }], ['{user}: {content}', { content: 'Hi' }], ['{channel} {date}', { content: 'x' }],
  ['{server} - {content}', { content: 'x' }], ['', { content: 'fallback template' }], ['{content}', { content: '😀' }],
  ['{content}', { content: '', member: null, user: { id: 'U', globalName: 'Glob', username: 'u' } }], ['{content}', { content: '', bare: true }],
  ['{content}', { content: 'y'.repeat(400) }], ['{content}', { content: 'word '.repeat(80) }],
  ['{user}', { member: { displayName: '<@123456789012345678> sneaky' }, content: '' }], ['{content}', { content: '{user} {server}' }],
  ['{content}', { content: 'line one\r\nline two' }], ['{date}', { content: '', createdAt: null }], ['  {content}  \t {user} ', { content: 'tabs\tand spaces' }],
  ['{unknown} {content}', { content: 'z' }],
];
sweep('at.threadName', nameCases, ([tpl, over]) => {
  const base = { member: { displayName: 'Alice' }, user: { id: 'U1', username: 'alice', globalName: null }, channel: { id: 'C1', name: 'help-desk' }, guild: { id: 'G', name: 'My Server' }, createdAt: new Date('2026-10-21T23:59:00Z') };
  const src = over.bare ? { content: over.content } : { ...base, ...over };
  return at.threadName(tpl, src);
});
sweep('at.unknownTokens', ['{user} {content} {channel} {date} {server} {user.name}', '{nope} {content}', 'plain', '{a} {b} {a}', '{ user }', '{USER}'], (t) => at.unknownTokens(t));
sweep('at.threadNeeds', [false, true], (b) => at.threadNeeds({ closeButton: b }));
const F = MessageFlags;
const pubCases = [
  { type: MessageType.Default }, { type: MessageType.Reply }, { type: MessageType.ChatInputCommand }, { type: MessageType.ContextMenuCommand },
  { type: MessageType.ChannelPinnedMessage }, { type: MessageType.Default, flags: F.Crossposted }, { type: MessageType.Default, flags: F.IsCrosspost },
  { type: MessageType.Default, flags: F.Loading }, { type: MessageType.Default, poll: true }, { type: MessageType.Default, forward: true },
  { type: MessageType.Default, flags: F.SuppressEmbeds }, { type: MessageType.UserJoin },
];
sweep('at.isPublishable', pubCases, (c) => at.isPublishable({
  type: c.type, poll: c.poll ? {} : null, reference: c.forward ? { type: MessageReferenceType.Forward } : null, flags: new MessageFlagsBitField(c.flags ?? 0),
}));
{
  let t = 1_000_000;
  const lim = at.createPublishLimiter({ now: () => t });
  const ops = [];
  for (let i = 0; i < 12; i++) ops.push(['take', 'A', 0]);
  ops.push(['remaining', 'A', 0], ['take', 'B', 0], ['release', 'A', 0], ['remaining', 'A', 0], ['take', 'A', at.AUTOTHREAD.PUBLISH_WINDOW_MS - 1], ['take', 'A', 2], ['remaining', 'A', 0], ['remaining', 'Z', 0]);
  out['at.limiter'] = ops.map(([op, ch, dt]) => { t += dt; const r = lim[op](ch); return { in: [op, ch, dt], out: op === 'release' ? null : r }; });
}

const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-community-b');
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, 'pure.json'), `${JSON.stringify(out, null, 1)}\n`);
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.stdout.write(`wrote ${Object.keys(out).length} sweeps\n`);
process.exit(0);
