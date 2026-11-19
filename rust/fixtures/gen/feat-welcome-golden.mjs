// Golden data for feat-welcome, produced by the JS bot's own welcomeService / applicationService with discord.js-shaped
// fakes (no servers needed). Writes rust/fixtures/golden/feat-welcome/golden.json.
//
//   node rust/fixtures/gen/feat-welcome-golden.mjs
//
// Every case carries its inputs (settings overrides on top of BASE, the fake member and guild), so the Rust test
// rebuilds the same situation with serenity objects and compares the outputs.
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

const svc = await imp('src', 'services', 'welcomeService.js');
const app = await imp('src', 'services', 'applicationService.js');
const { cache } = await imp('src', 'services', 'redis.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

// ── Fakes shaped like the serenity objects the Rust test builds ─────────────────────────────────────────────────
const GID = '900000000000000501';
const UID = '111111111111111111';
const defaultAvatar = (id) => `https://cdn.discordapp.com/embed/avatars/${Number((BigInt(id) >> 22n) % 6n)}.png`;
const created = (id) => Number((BigInt(id) >> 22n) + 1420070400000n);

function makeGuild(name = 'Test $& Server') {
  return {
    id: GID, name, memberCount: 22, ownerId: '600000000000000099', available: true, features: [],
    roles: { cache: new Map() }, channels: { cache: new Map() },
    iconURL: () => null, createdTimestamp: created(GID),
  };
}
function makeMember(guild, { id = UID, username = 'newbie' } = {}) {
  const user = {
    id, username, tag: username, discriminator: '0', bot: false, globalName: null,
    displayAvatarURL: () => defaultAvatar(id), createdTimestamp: created(id), createdAt: new Date(created(id)),
    toString: () => `<@${id}>`,
  };
  return {
    id, user, displayName: username, nickname: null, guild, joinedTimestamp: 1_700_000_000_000,
    displayAvatarURL: () => defaultAvatar(id), roles: { cache: new Map() }, toString: () => `<@${id}>`,
  };
}

const BASE = Object.freeze({
  welcomeEnabled: true, welcomeUseEmbed: true, welcomeMessageEnabled: true, goodbyeUseEmbed: false, welcomeDmCard: false,
  welcomeMessage: null, welcomeEmbed: null, welcomeCardStyle: null, welcomeDmMessage: null, welcomeDmEmbed: null,
  welcomeChannelId: null, welcomeDmEnabled: false, goodbyeEnabled: false, goodbyeChannelId: null, goodbyeMessage: null,
  goodbyeEmbed: null, autoroleIds: [], autoroleDelay: 0, embedColor: null, captchaEnabled: false, captchaRoleId: null,
});
const settings = (over = {}) => ({ ...BASE, ...over });

const embedJson = (e) => {
  const j = typeof e?.toJSON === 'function' ? e.toJSON() : e;
  const { timestamp, ...rest } = j ?? {};
  return timestamp === undefined ? rest : { ...rest, timestamp: '<ts>' };
};
const payloadJson = (p) => {
  if (!p) return null;
  const out = {};
  if (p.content) out.content = p.content;
  out.embeds = (p.embeds ?? []).map(embedJson);
  out.components = (p.components ?? []).map((r) => (typeof r.toJSON === 'function' ? r.toJSON() : r));
  out.allowedMentions = p.allowedMentions;
  if (p.files?.length) out.files = p.files.map((f) => f.name);
  return out;
};

// ── welcomeService ──────────────────────────────────────────────────────────────────────────────────────────────
const out = { base: BASE, guildId: GID, userId: UID, welcome: {}, applications: {} };
const W = out.welcome;

W.constants = {
  DEFAULT_WELCOME_MESSAGE: svc.DEFAULT_WELCOME_MESSAGE, DEFAULT_WELCOME_DM_MESSAGE: svc.DEFAULT_WELCOME_DM_MESSAGE,
  DEFAULT_GOODBYE_MESSAGE: svc.DEFAULT_GOODBYE_MESSAGE, DEFAULT_WELCOME_EMBED: svc.DEFAULT_WELCOME_EMBED,
  DEFAULT_GOODBYE_EMBED: svc.DEFAULT_GOODBYE_EMBED, MAX_AUTOROLE_DELAY_SECONDS: svc.MAX_AUTOROLE_DELAY_SECONDS,
  SCREENING_WATCH_MS: svc.SCREENING_WATCH_MS, SCREENING_POLL_MS: svc.SCREENING_POLL_MS, FLOOD_CAPS: svc.FLOOD_CAPS,
  CARD_FILE_NAME: svc.CARD_FILE_NAME, autoroleJobKey: svc.autoroleJobKey(GID, UID), MESSAGE_FIELDS: svc.MESSAGE_FIELDS,
};

const overrides = [
  {}, { welcomeUseEmbed: false }, { welcomeUseEmbed: false, welcomeMessage: '' }, { welcomeUseEmbed: false, welcomeMessage: 'hi' },
  { welcomeUseEmbed: false, welcomeEmbed: { title: 'Hello' } }, { welcomeEmbed: { card: true } }, { welcomeMessage: '' },
  { welcomeMessage: 'hi' }, { welcomeEmbed: { title: 'Hello' } }, { welcomeEmbed: { title: 'Hello' }, welcomeMessage: '' },
  { welcomeEmbed: { title: 'Hello' }, welcomeMessage: 'hi' }, { goodbyeMessage: '' }, { goodbyeMessage: 'bye' },
  { goodbyeUseEmbed: true }, { goodbyeUseEmbed: true, goodbyeMessage: 'bye', goodbyeEmbed: { title: 'T' } },
  { welcomeDmMessage: '' }, { welcomeDmEmbed: { description: 'x' } }, { welcomeEmbed: { card: false } },
  { welcomeEmbed: { title: 'x'.repeat(300), color: 0xFFFFFFF, footer: 'f'.repeat(3000), thumbnail: 'user_avatar', card: true } },
  { welcomeEmbed: { title: 'T', image: 'https://x.test/a.png' } }, { welcomeEmbed: { title: 'T' }, embedColor: 0x123456 },
  { goodbyeUseEmbed: true, embedColor: 99999999 }, { welcomeDmMessage: 'Hi {user.name} from {server}', welcomeDmEmbed: { title: 'T {server.member_count}' } },
  { welcomeCardStyle: { v: 1, enabled: false } }, { welcomeMessage: 'Welcome {user}!', welcomeEmbed: { description: 'hey {usr}', footer: { text: 'f', iconUrl: 'javascript:x' } } },
];
W.resolve = [];
for (const over of overrides) {
  for (const kind of ['welcome', 'dm', 'goodbye']) {
    for (const forceText of [false, true]) {
      W.resolve.push({ over, kind, forceText, out: svc.resolveTemplate(kind, settings(over), { forceText }) });
    }
  }
}

W.heal = [
  { card: true }, { card: false }, null, 'x', [1], { title: 't', color: -1 }, { title: 't', color: 0x123456 },
  { title: 't', thumbnail: 'javascript:alert(1)' }, { title: 't', fields: Array.from({ length: 40 }, (_, i) => ({ name: `n${i}`, value: 'v', inline: i % 2 === 0 })) },
  { title: 'x'.repeat(300), color: 0xFFFFFFF, footer: 'f'.repeat(3000), thumbnail: 'user_avatar', card: true },
  { description: 'd', author: { name: 'a', iconUrl: 'nope' }, footer: { text: 'f', iconUrl: 'https://x.test/i.png' }, image: '{user.avatar}' },
  { footer: { text: 'only' } }, { thumbnail: 'https://x.test/a.png' }, { author: 'Author' }, { image: 'attachment://welcome.png', url: 'https://x.test' },
  { title: 'T', timestamp: true, color: '#ff0000' },
].map((input) => ({ input, out: svc.healEmbed(input), has: svc.hasEmbedContent(input) }));

W.serialize = [
  { title: 'T', description: '', url: null, color: 0, author: { name: 'a', iconUrl: null, url: '' }, footer: { text: 'f', iconUrl: null },
    image: 'i', fields: [{ name: 'n', value: 'v', inline: false, extra: null }], timestamp: true, card: true, junk: 1 },
  null, { thumbnail: '{user.avatar}', fields: [] },
].map((input) => ({ input, out: svc.serializeEmbed(input) }));

W.cardStyle = [
  { background: '#GGGGGG', overlayOpacity: 3, backgroundUrl: 'ftp://x', enabled: 'yes' }, null, { overlayOpacity: 0.456 },
  { overlayOpacity: -2 }, { textColor: '#ABCDEF' }, { backgroundUrl: `https://x.test/${'a'.repeat(2100)}` },
  { v: 1, enabled: false, background: '#112233', backgroundUrl: 'https://x.test/a.png', textColor: '#ffffff', accentColor: '#5865f2', overlayOpacity: 0.4 },
  { overlayOpacity: 0.125 }, { overlayOpacity: 0.005 }, {},
].map((input) => {
  const parsed = svc.parseCardStyle(input);
  return { input, parsed, serialized: svc.serializeCardStyle(parsed), described: svc.describeCardStyle(parsed) };
});
W.cardEnabled = [
  { welcomeEmbed: { card: false }, welcomeCardStyle: null }, {}, { welcomeEmbed: { card: true }, welcomeCardStyle: null },
  { welcomeEmbed: { card: false }, welcomeCardStyle: { v: 1, enabled: true } }, { welcomeEmbed: { card: true }, welcomeCardStyle: { v: 1, enabled: false } },
].map((over) => ({ over, out: svc.welcomeCardEnabled(settings(over)) }));

W.materialise = [
  [{ welcomeEmbed: { card: false, title: 'T' } }, { welcomeEmbed: { title: 'T' } }],
  [{ welcomeCardStyle: { v: 1, enabled: true, background: null, backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: null } }, { welcomeEmbed: { title: 'T' } }],
  [{ welcomeEmbed: { card: false, title: 'T' } }, { welcomeCardStyle: { v: 1, enabled: true } }],
  [{ welcomeMessage: null }, { welcomeEmbed: { title: 'T' } }],
  [{ welcomeMessage: null, welcomeEmbed: { title: 'Old' } }, { welcomeEmbed: { title: 'New' } }],
  [{ welcomeMessage: 'mine' }, { welcomeEmbed: { title: 'T' } }],
  [{ welcomeMessage: null }, { welcomeMessage: 'x' }],
].map(([over, data]) => ({ over, data, out: svc.materialiseWelcome(settings(over), { ...data }) }));

W.delay = ['0', 'none', 'OFF', '600', '10m', '1h30m', '7d', ' 90s ', '8d', 'abc', '5x', '', '99999999', '-5', '1.5h', 'immediately', '604800', '604801', '2 hours']
  .map((input) => ({ input, out: svc.parseDelayInput(input) }));
W.delayText = [0, 60, 600, 5400, 604_800, 9_999_999, -5].map((s) => ({ s, out: svc.delayText(s) }));
const now = 1_700_000_000_000;
W.screening = [
  [true, false, true, undefined], [null, false, false, now - 3_600_000], [null, false, false, now - 8 * 86_400_000],
  [null, false, false, now - svc.SCREENING_WATCH_MS - 1], [undefined, true, undefined, undefined], [false, false, true, undefined],
  [null, false, false, undefined], [true, null, true, undefined], [null, false, false, now - svc.SCREENING_WATCH_MS],
].map(([o, n, k, j]) => ({ oldPending: o ?? null, newPending: n ?? null, oldKnown: k ?? false, joined: j ?? null, now,
  out: svc.isScreeningCompletion({ oldMember: { pending: o }, newMember: { pending: n, joinedTimestamp: j }, oldKnown: k, now }) }));
W.dueAt = [[now - 1000, 60], [now, 9_999_999], [now, -5], [null, 10]].map(([joined, delay]) => ({
  joined, delay, now, out: svc.autoroleDueAt({ joinedTimestamp: joined }, { autoroleDelay: delay }, now),
}));
W.describeMode = overrides.map((over) => ({
  over, welcome: svc.describeMode('welcome', settings(over)), dm: svc.describeMode('dm', settings(over)), goodbye: svc.describeMode('goodbye', settings(over)),
  embedWelcome: svc.describeEmbedSettings('welcome', settings(over)), embedGoodbye: svc.describeEmbedSettings('goodbye', settings(over)),
  card: svc.cardSummary(settings(over)),
  info: { welcome: embedJson(svc.messageInfoEmbed('Welcome Text', 'welcome', settings(over))), goodbye: embedJson(svc.messageInfoEmbed('Goodbye Text', 'goodbye', settings(over))) },
}));
W.channelNeeds = overrides.map((over) => ({ over, out: svc.welcomeChannelNeeds(settings(over)) }));

// Payloads.
const guild = makeGuild();
const member = makeMember(guild);
const longGuild = makeGuild('N'.repeat(120));
const PNG = Buffer.from([1]);
W.payloads = [];
const addPayload = (name, kind, over, opts = {}) => {
  const g = opts.long ? longGuild : guild;
  const m = makeMember(g);
  const p = svc.buildWelcomePayload(kind, {
    settings: settings(over), member: opts.noMember ? null : m, user: m.user, guild: g, card: opts.card ? PNG : null,
    forceText: opts.forceText ?? false, preview: opts.preview ?? false,
  });
  W.payloads.push({ name, kind, over, opts, out: payloadJson(p) });
};
for (const [i, over] of overrides.entries()) {
  for (const kind of ['welcome', 'dm', 'goodbye']) {
    addPayload(`o${i}-${kind}`, kind, over);
    addPayload(`o${i}-${kind}-card`, kind, over, { card: true });
    addPayload(`o${i}-${kind}-text`, kind, over, { forceText: true });
    addPayload(`o${i}-${kind}-preview`, kind, over, { preview: true });
  }
}
addPayload('long-dm', 'dm', {}, { long: true });
addPayload('partial-goodbye', 'goodbye', {}, { noMember: true });
addPayload('partial-goodbye-embed', 'goodbye', { goodbyeUseEmbed: true }, { noMember: true });

// Setup modals.
W.modals = [
  ...overrides.map((over) => ({ over })),
  { over: { welcomeMessage: 'm'.repeat(2500), welcomeEmbed: { title: 't'.repeat(400), description: 'd'.repeat(5000), footer: { text: 'f'.repeat(3000) }, color: 0xFFFFFFF } } },
  { over: { welcomeEmbed: { title: 'T', color: 0 } } },
].map(({ over }) => ({
  over,
  welcome: svc.buildSetupModal('welcome', settings(over)).toJSON(),
  dm: svc.buildSetupModal('dm', settings(over)).toJSON(),
  goodbye: svc.buildSetupModal('goodbye', settings(over)).toJSON(),
}));

// Placeholder helpers.
W.unknown = ['hi {usr}', 'Hey {user}', '{a} {b} {user.nme}', ''].map((text) => ({ text, out: svc.unknownTokenWarning(text, { member, guild }) }));
W.messageUpdate = [
  ['Welcome Text Updated', 'welcome', {}, { text: 'Hi', reset: false, enabled: null }],
  ['Welcome Text Updated', 'welcome', { welcomeMessage: 'Hey {user}', welcomeMessageEnabled: false }, { text: 'Hey {user} {nope}', enabled: false }],
  ['Welcome Text Updated', 'welcome', {}, { reset: true }],
  ['Welcome DM Text Updated', 'dm', { welcomeDmMessage: '' }, { text: '' }],
  ['Goodbye Text Updated', 'goodbye', { goodbyeMessage: 'Bye {user}' }, { text: 'Bye {user}' }],
  ['Goodbye Text Updated', 'goodbye', { goodbyeMessage: 'a```b' }, { text: 'a```b' }],
].map(([title, kind, over, change]) => ({ title, kind, over, change, out: embedJson(svc.messageUpdateEmbed(title, kind, settings(over), change, { member, guild })) }));

// The flood-cap key: record what the service asks the cache for.
const keys = [];
cache.slidingWindow = async (key) => { keys.push(key); return 1; };
await svc.renderWelcomeCard({ ...member, displayAvatarURL: () => '', guild }, settings(), { preview: false });
W.floodKeys = keys;

// ── applicationService ──────────────────────────────────────────────────────────────────────────────────────────
const P = out.applications;
P.constants = { ...app.APPLICATION, DEFAULT_ACCEPT_MESSAGE: app.DEFAULT_ACCEPT_MESSAGE, DEFAULT_DENY_MESSAGE: app.DEFAULT_DENY_MESSAGE };
P.parseQuestions = [
  null, [{ label: '  Why?  ', style: 'PARAGRAPH', required: false, placeholder: ' hint ' }, { label: '' }, null, 5, { style: 'SHORT' }],
  [{ label: 'A', style: 'weird' }], Array.from({ length: 40 }, (_, n) => ({ label: `Q${n}` })), [{ label: 'x'.repeat(200), placeholder: 'p'.repeat(150) }],
].map((input) => ({ input, out: app.parseQuestions(input) }));
P.pageCount = [0, 1, 5, 6, 10, 11, 15].map((n) => ({ n, out: app.pageCount(n) }));
P.answerLimit = [['SHORT', 1], ['PARAGRAPH', 1], ['PARAGRAPH', 15], ['SHORT', 15], ['PARAGRAPH', 4], ['SHORT', 13], ['PARAGRAPH', 0]]
  .map(([style, total]) => ({ style, total, out: app.answerLimit(style, total) }));
P.clear = [' None ', 'clear', 'DEFAULT', 'nope', '', 'none!'].map((v) => ({ v, out: app.isClearWord(v) }));

const qs15 = app.parseQuestions(Array.from({ length: 15 }, (_, n) => ({
  label: `Question ${n + 1}`, style: n % 2 ? 'PARAGRAPH' : 'SHORT', required: n !== 6, placeholder: n === 5 ? 'a hint' : undefined,
})));
const stored = qs15.map((q, i) => ({ question: q.label, answer: i === 5 ? 'earlier' : (i === 6 ? 'seven' : '') }));
P.questions15 = qs15;
P.stored = stored;
P.applyModals = [0, 1, 2].map((page) => ({
  name: 'x'.repeat(32), page, out: app.buildApplyModal({ form: { id: 'F', name: 'x'.repeat(32) }, questions: qs15, submission: { id: 'SUB1', answers: stored }, page }).toJSON(),
}));
P.applyModals.push({ name: 'Solo', page: 0, two: true,
  out: app.buildApplyModal({ form: { id: 'F', name: 'Solo' }, questions: qs15.slice(0, 2), submission: { id: 'SUB1', answers: stored }, page: 0 }).toJSON() });
P.denyModal = app.buildDenyModal('SUB1').toJSON();
P.continueRows = [[0, 3, null], [1, 2, 'Try again'], [2, 3, 'Fix answers']].map(([page, total, label]) => ({ page, total, label, out: app.continueRow('SUB1', page, total, label).toJSON() }));
P.reviewRow = app.reviewRow('SUB1').toJSON();
P.applyCustomId = app.applyCustomId('FORM1');
P.applyButton = app.buildApplyButton({ id: 'FORM1' }, 'Fill out the form').toJSON();
const panelGuild = { id: GID, name: 'Test Guild', memberCount: 3, iconURL: () => null };
P.panels = [
  { id: 'FORM1', name: 'Staff', description: null, questions: [{ label: 'Q' }, { label: 'R' }] },
  { id: 'F2', name: 'Staff', description: 'Join {server}!', questions: [] },
  { id: 'F3', name: 'N'.repeat(300), description: '', questions: [{ label: 'Only' }] },
].map((form) => {
  const p = app.buildPanelPayload(form, panelGuild);
  return { form, out: { embeds: p.embeds.map(embedJson), components: p.components.map((r) => r.toJSON()), allowedMentions: p.allowedMentions } };
});
const reviewUser = { id: UID, username: 'user`one', createdTimestamp: created(UID), displayAvatarURL: () => defaultAvatar(UID).replace('.png', '.png') };
const submittedAt = new Date('2026-10-20T12:00:00.000Z');
const answers15 = Array.from({ length: 15 }, (_, n) => ({ question: `Q${n + 1} ${'q'.repeat(40)}`, answer: 'a'.repeat(1000) }));
const review = app.buildReviewEmbed({ form: { name: 'Staff' }, answers: answers15, user: reviewUser, member: { joinedTimestamp: 1_690_000_000_000 }, submissionId: 'SUB1', submittedAt });
P.review = { submittedAt: submittedAt.toISOString(), out: embedJson(review), timestamp: review.toJSON().timestamp };
P.reviewEmpty = embedJson(app.buildReviewEmbed({ form: { name: 'Staff' }, answers: [{ question: 'Q', answer: '  ' }], user: reviewUser, submissionId: 'S', submittedAt }));
const at = new Date('2026-10-21T08:00:00.000Z');
P.outcomes = [
  { status: 'DENIED', reviewerId: 'M1', note: 'n'.repeat(500), extra: 'e'.repeat(2000), source: true },
  { status: 'ACCEPTED', reviewerId: 'M1', note: null, extra: 'Roles given: <@&R1>', source: true },
  { status: 'ACCEPTED', reviewerId: 'M2', note: null, extra: null, source: false },
].map((o) => ({ ...o, at: at.toISOString(), out: embedJson(app.buildOutcomeEmbed(o.source ? review.toJSON() : null, { form: { name: 'Staff' }, ...o, submissionId: 'SUB1', at })) }));
P.refusals = [
  ['MODULE_OFF'], ['FORM_NOT_FOUND'], ['FORM_DISABLED'], ['NOT_SET_UP'], ['REVIEW_CHANNEL', { reason: 'I need **Send Messages** in <#1>.' }],
  ['REVIEW_CHANNEL', {}], ['MISSING_ROLE', { roleIds: ['A', 'B'] }], ['PENDING'], ['COOLDOWN', { until: new Date(1_800_000_000_000) }], ['SLOW'],
  ['SERVER_ONLY'], ['ERROR'],
].map(([code, data]) => ({ code, data: data ?? {}, out: embedJson(app.refusalEmbed(code, data)) }));
const nowJ = new Date('2026-10-20T12:00:00Z');
const baseForm = { id: 'F', enabled: true, reviewChannelId: 'C', questions: [{ label: 'Q' }], requiredRoleIds: [], cooldownHours: 24 };
P.judge = [
  [{}, {}], [{ enabled: false }, {}], [{ reviewChannelId: null }, {}], [{ questions: [] }, {}], [{}, { reviewChannelProblem: 'nope' }],
  [{ requiredRoleIds: ['A', 'B'] }, { memberRoleIds: ['X'] }], [{ requiredRoleIds: ['A', 'B'] }, { memberRoleIds: ['B'] }], [{}, { pending: { id: 'S' } }],
  [{}, { lastDenied: { reviewedAt: new Date(nowJ.getTime() - 3_600_000) } }], [{}, { lastDenied: { reviewedAt: new Date(nowJ.getTime() - 25 * 3_600_000) } }],
  [{ cooldownHours: 0 }, { lastDenied: { reviewedAt: new Date(nowJ.getTime() - 3_600_000) } }],
].map(([form, facts]) => {
  const r = app.judgeApply({ form: { ...baseForm, ...form }, memberRoleIds: [], now: nowJ, ...facts });
  return { form: { ...baseForm, ...form }, facts: { ...facts, pending: Boolean(facts.pending) }, now: nowJ.toISOString(), out: r.data?.until ? { ...r, data: { until: r.data.until.toISOString() } } : r };
});
P.normalize = [
  [[{ label: 'A' }, { label: 'B' }, { label: 'C' }], [{ question: 'A', answer: 'one' }, { question: 'Old B', answer: 'stale' }]],
  [[{ label: 'A' }], 'junk'], [[{ label: 'A' }, { label: 'B' }], [{ question: 'A', answer: 5 }, { question: 'B', answer: 'b' }]],
].map(([qs, st]) => ({ qs, stored: st, out: app.normalizeAnswers(qs, st), merged: app.mergeAnswers(qs, st, 0, ['x', 'y', 'z'].slice(0, qs.length)) }));

mkdirSync(join(ROOT, 'rust', 'fixtures', 'golden', 'feat-welcome'), { recursive: true });
writeFileSync(join(ROOT, 'rust', 'fixtures', 'golden', 'feat-welcome', 'golden.json'), `${JSON.stringify(out, null, 1)}\n`);
process.stdout.write(`wrote ${W.payloads.length} payloads, ${W.resolve.length} templates\n`);
redis.disconnect();
process.exit(0);
