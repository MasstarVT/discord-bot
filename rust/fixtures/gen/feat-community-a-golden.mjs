// Reference data for feat-community-a (suggestions, confessions, bumps), produced by the JS code itself. No servers:
// Redis and Postgres point at closed ports. Writes rust/fixtures/golden/feat-community-a/*.json.
//
//   node rust/fixtures/gen/feat-community-a-golden.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-community-a');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};

const bump = await imp('src', 'services', 'bumpService.js');
const sug = await imp('src', 'services', 'suggestionService.js');
const conf = await imp('src', 'services', 'confessionService.js');
const { renderPlaceholders } = await imp('src', 'utils', 'placeholderParser.js');
const { DEFAULT_TEXTS } = await imp('src', 'dashboard', 'fields.js');
const { default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');

mkdirSync(OUT, { recursive: true });
const write = (name, value) => writeFileSync(join(OUT, `${name}.json`), `${JSON.stringify(value, null, 2)}\n`);
const json = (x) => JSON.parse(JSON.stringify(x));
const noTimestamp = (e) => { const o = json(e); delete o.timestamp; return o; };

// ── Bumps ─────────────────────────────────────────────────────────────────────

const disboardCases = [
  { description: 'Bump done! :thumbsup:\nCheck it out [on DISBOARD](https://disboard.org/server/1)' },
  { description: 'BUMP DONE!' },
  { description: 'Server bumped!', image: { url: 'https://disboard.org/images/bot-command-image-bump.png' } },
  { description: 'Please wait another 87 minutes until the server can be bumped' },
  { description: 'Please wait another 3 minutes until the server can be bumped', image: { url: 'https://disboard.org/images/bot-command-image-bump.png' } },
  { description: 'Looking for a new server? Try DISBOARD.' },
  {},
  { description: 'please WAIT for it... bump done' },
  { title: 'DISBOARD: The Public Server List', description: 'Bump done :thumbsup:' },
].map((embed) => ({ embeds: [embed], success: bump.isDisboardSuccess({ embeds: [embed] }) }));
disboardCases.push({ embeds: [], success: bump.isDisboardSuccess({ embeds: [] }) });
disboardCases.push({ embeds: [{ description: 'nothing' }, { description: 'Bump done!' }], success: bump.isDisboardSuccess({ embeds: [{ description: 'nothing' }, { description: 'Bump done!' }] }) });

// recordBump would touch Postgres, so this is scheduleReminder's argument written out from the service's own pieces
// (reminderDedupeKey). The real proof of the job row is the cross-language test: the JS bot's row runs under Rust and
// the Rust bot's row reads back through Prisma (rust/fixtures/gen/feat-community-a-js.mjs).
const runAt = new Date(Date.UTC(2026, 0, 2, 3, 4, 5, 678));
const job = { guildId: '900000000000000077', type: 'bump.remind', runAt, payload: { site: 'disboard' }, dedupeKey: bump.reminderDedupeKey('900000000000000077', 'disboard'), onConflict: 'replace' };

const thanks = [
  DEFAULT_TEXTS.bumpThanksMessage,
  'Thanks! That is bump number {count} on {site}. Next {next}.',
  '{count} {site}',
  '   ',
].map((template) => ({
  template,
  count: 7,
  nextMs: runAt.getTime(),
  text: renderPlaceholders(template || bump.DEFAULT_THANKS_MESSAGE, { vars: { next: `<t:${Math.floor(runAt.getTime() / 1000)}:R>`, count: 7, site: 'Disboard' } }, { maxLength: 1900 }),
  textUnknownCount: renderPlaceholders(template || bump.DEFAULT_THANKS_MESSAGE, { vars: { next: `<t:${Math.floor(runAt.getTime() / 1000)}:R>`, count: '?', site: 'Disboard' } }, { maxLength: 1900 }),
}));
const reminders = [null, '', '  ', 'Bump us on {site}!', 'x'.repeat(2100)].map((template) => {
  const text = renderPlaceholders(template || bump.DEFAULT_REMINDER_MESSAGE, { vars: { site: 'Disboard' } }, { maxLength: 1900 }).trim() || bump.DEFAULT_REMINDER_MESSAGE;
  return { template, text, withRole: `<@&123456789012345678> ${text}`.slice(0, 2000) };
});

write('bumps', {
  sites: Object.fromEntries(Object.entries(bump.BUMP_SITES).map(([k, s]) => [k, { botId: s.botId, name: s.name, cooldownMs: s.cooldownMs }])),
  bumpCountMax: bump.BUMP_COUNT_MAX,
  leaderboardSize: bump.LEADERBOARD_SIZE,
  defaultReminder: bump.DEFAULT_REMINDER_MESSAGE,
  defaultThanks: bump.DEFAULT_THANKS_MESSAGE,
  dashboardDefaults: { reminder: DEFAULT_TEXTS.bumpReminderMessage, thanks: DEFAULT_TEXTS.bumpThanksMessage },
  moduleOff: noTimestamp(bump.moduleOffEmbed().toJSON()),
  dedupeKey: bump.reminderDedupeKey('G1', 'disboard'),
  job: json(job),
  disboard: disboardCases,
  thanks,
  reminders,
  rawSqlNames: bump.RAW_SQL.map((r) => r.name),
});

// ── Suggestions ───────────────────────────────────────────────────────────────

const suggestionRow = (over = {}) => ({
  id: 'cmsugg0000000000000000001', guildId: '900000000000000077', number: 12, authorId: '600000000000000001', anonymous: false,
  content: 'Add a **music** channel\nplease', status: 'OPEN', channelId: '800000000000000001', messageId: '700000000000000001', threadId: null,
  upvotes: 3, downvotes: 1, reviewerId: null, reviewNote: null, reviewedAt: null, createdAt: new Date(Date.UTC(2026, 0, 2, 3, 4, 5, 678)), ...over,
});
const sRows = [
  suggestionRow(),
  suggestionRow({ anonymous: true }),
  suggestionRow({ status: 'CONSIDERED', reviewerId: '600000000000000009', reviewedAt: new Date(0) }),
  suggestionRow({ status: 'APPROVED', reviewerId: '600000000000000009', reviewNote: 'Done next week', reviewedAt: new Date(0) }),
  suggestionRow({ status: 'DENIED', reviewerId: '600000000000000009', reviewNote: 'n'.repeat(1200), reviewedAt: new Date(0), upvotes: 0, downvotes: 0 }),
  suggestionRow({ status: 'IMPLEMENTED', messageId: null, content: 'c'.repeat(4100) }),
];
const author = { name: 'Some Member', iconURL: 'https://cdn.discordapp.com/embed/avatars/0.png' };
write('suggestions', {
  constants: json(sug.SUGGESTION),
  statusInfo: json(sug.STATUS_INFO),
  closed: [...sug.CLOSED_STATUSES],
  rawSqlNames: sug.RAW_SQL.map((r) => r.name),
  moduleOff: noTimestamp(sug.moduleOffEmbed().toJSON()),
  cooldownExample: { ms: 90_000 },
  parse: ['', '   ', ' hi ', 'x'.repeat(2000), 'x'.repeat(2001), ` ${'y'.repeat(2000)} `].map((raw) => ({ raw, out: sug.parseContent(raw) })),
  toggle: [[0, 1], [0, -1], [1, 1], [1, -1], [-1, 1], [-1, -1]].map(([c, v]) => ({ current: c, value: v, out: sug.toggleVote(c, v) })),
  posts: sRows.map((row) => ({
    row: json(row),
    embed: json(sug.buildSuggestionEmbed(row, { author }).toJSON()),
    embedNoIcon: json(sug.buildSuggestionEmbed(row, { author: { name: 'x'.repeat(300) } }).toJSON()),
    components: json(sug.buildSuggestionComponents(row).map((r) => r.toJSON())),
    jumpUrl: sug.jumpUrl(row),
    dm: noTimestamp(sug.buildReviewDm('Test Guild', row).toJSON()),
    dmNoGuild: noTimestamp(sug.buildReviewDm(undefined, row).toJSON()),
  })),
  modal: [false, true].map((a) => json(sug.buildSuggestionModal(a).toJSON())),
});

// ── Confessions ───────────────────────────────────────────────────────────────

const confessionRow = (over = {}) => ({
  id: 'cmconf0000000000000000001', guildId: '900000000000000077', number: 5, authorId: '600000000000000001', content: 'I *never* read the rules',
  hasImage: false, status: 'PENDING', channelId: '800000000000000001', messageId: null, reviewMessageId: null, reviewerId: null,
  reviewedAt: null, createdAt: new Date(Date.UTC(2026, 0, 2, 3, 4, 5, 678)), ...over,
});
const linkCases = [
  'hello there', 'see https://example.com', 'www.example.com', 'discord.gg/abc', 'join discord.com/invite/abc', 'discordapp.com/invite/x',
  'dsc.gg/x', 'invite.gg/x', 'bit.ly/abc', 'example.com', 'example.com/', 'example.com/x', 'e.g. this', 'ｈｔｔｐｓ://example.com',
  'disc\u200Bord.gg/x', 'h\u00ADttps://x.y', 'ftp://files', 'mailto:me@x.com', 'a.b/c', '1.5/2', 'end.', 'x://', 'HTTP://X.COM',
  'Discord.GG/abc', 'my.site.co.uk/page', 'no link here at all', 'www', 'wwwexample.com/x', 'café.com/x', 'news.bbc.co.uk',
  'ex-ample.com/p', 'file:///etc', 'ab.cdefghijklmnopqrstuvwxyz/x', 'go to example. com/x', 'example.c/x',
];
write('confessions', {
  constants: json(conf.CONFESSION),
  statusInfo: json(conf.STATUS_INFO),
  rawSqlNames: conf.RAW_SQL.map((r) => r.name),
  moduleOff: noTimestamp(conf.moduleOffEmbed().toJSON()),
  parse: ['', '   ', ' hi ', 'x'.repeat(2000), 'x'.repeat(2001)].map((raw) => ({ raw, out: conf.parseContent(raw) })),
  links: linkCases.map((text) => ({ text, link: conf.containsLink(text) })),
  errTag: [
    { err: { name: 'PrismaClientKnownRequestError', code: 'P2002' }, tag: conf.errTag({ name: 'PrismaClientKnownRequestError', code: 'P2002' }) },
    { err: { name: 'TypeError' }, tag: conf.errTag(new TypeError('x')) },
    { err: null, tag: conf.errTag(null) },
  ],
  post: noTimestamp(conf.buildConfessionEmbed(confessionRow()).toJSON()),
  postImage: noTimestamp(conf.buildConfessionEmbed(confessionRow({ hasImage: true }), { imageName: 'confession.png' }).toJSON()),
  postLong: noTimestamp(conf.buildConfessionEmbed(confessionRow({ content: 'z'.repeat(4100) })).toJSON()),
  review: json(conf.buildReviewEmbed(confessionRow()).toJSON()),
  reviewImage: json(conf.buildReviewEmbed(confessionRow(), { imageName: 'confession.gif' }).toJSON()),
  reviewComponents: json(conf.buildReviewComponents('cmconf0000000000000000001').map((r) => r.toJSON())),
  modal: json(conf.buildConfessionModal().toJSON()),
  panel: (() => { const p = conf.buildPanelPayload(); return { embeds: p.embeds.map(noTimestamp), components: p.components.map((r) => json(r.toJSON())), allowedMentions: p.allowedMentions }; })(),
  imageProblems: [
    { allow: false, attachment: { contentType: 'image/png', size: 10 } },
    { allow: true, attachment: { contentType: 'image/png', size: 10 } },
    { allow: true, attachment: { contentType: 'image/svg+xml', size: 10 } },
    { allow: true, attachment: { contentType: 'text/plain; charset=utf-8', size: 10 } },
    { allow: true, attachment: { contentType: 'IMAGE/PNG; x=y', size: 10 } },
    { allow: true, attachment: { contentType: null, size: 10 } },
    { allow: true, attachment: { contentType: 'image/webp', size: 8_000_001 } },
    { allow: true, attachment: { contentType: 'image/webp', size: 8_000_000 } },
  ].map((c) => {
    const e = conf.imageProblem({ confessionAllowImages: c.allow }, c.attachment);
    return { ...c, problem: e ? noTimestamp(e.toJSON()) : null };
  }),
});

redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.stdout.write(`wrote ${OUT}\n`);
process.exit(0);
