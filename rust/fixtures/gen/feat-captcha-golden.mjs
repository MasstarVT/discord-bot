// Golden data for feat-captcha, produced from the JS code (no servers needed):
//   node rust/fixtures/gen/feat-captcha-golden.mjs
// writes rust/fixtures/golden/feat-captcha/golden.json: pure helper sweeps of captchaService.js and stickyService.js,
// the member-facing payloads (embed timestamps removed), the panel and challenge payloads, the forms, the keys.
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';

const quiet = () => {};
console.error = console.warn = console.log = console.debug = console.info = quiet;

const svc = await imp('src', 'services', 'captchaService.js');
const sticky = await imp('src', 'services', 'stickyService.js');
const stickyPlugin = await imp('src', 'plugins', 'sticky.js');
const { Collection, PermissionsBitField, PermissionFlagsBits } = await import('discord.js');
const { default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');

const json = (v) => JSON.parse(JSON.stringify(v, (k, x) => (x && typeof x.toJSON === 'function' ? x.toJSON() : x)));
const dropTs = (e) => { const o = json(e); delete o.timestamp; return o; };
const payloadJson = (p) => {
  if (typeof p === 'string') return { content: p };
  const out = {};
  for (const k of ['content', 'embeds', 'components', 'attachments', 'allowedMentions']) {
    if (k in p) out[k] = k === 'embeds' ? p.embeds.map(dropTs) : json(p[k]);
  }
  return out;
};

const out = {};

// ── Pure helpers ──────────────────────────────────────────────────────────────
const answers = [' 7kx-4pm ', '7KX4PM', '7kx_4p.m', 'abc', '', null, 'ÿx', 'ß7KX4P', ' 7KX4PM ', '7KX\t4PM\n', 'ﬀ'];
out.normalize = answers.map((a) => [a, svc.normalizeAnswer(a)]);
out.hash = [['G', 'U', '7KX4PM'], ['900000000000000001', '600000000000000001', 'acefhj'], ['G', 'V', ' 7kx-4pm ']].map(([g, u, a]) => [g, u, a, svc.hashAnswer(g, u, a)]);
const h = svc.hashAnswer('G', 'U', '7KX4PM');
out.matches = [h, h.toUpperCase(), null, '', 'abc', 'z'.repeat(64), 'a'.repeat(63)].flatMap((hash) => answers.map((a) => [hash, a, svc.answerMatches(hash, 'G', 'U', a)]));
const now = 1_000_000;
const live = (over = {}) => ({ answerHash: h, attempts: 0, expiresAt: new Date(now + 60_000).toISOString(), ...over });
out.evaluate = [
  [null, '7KX4PM'], [live({ answerHash: null }), '7KX4PM'], [live({ expiresAt: new Date(now - 1).toISOString() }), '7KX4PM'],
  [live({ expiresAt: new Date(now).toISOString() }), '7KX4PM'], [live({ expiresAt: null }), '7KX4PM'], [live({ attempts: 3 }), '7KX4PM'],
  [live(), '7KX4PM'], [live({ attempts: 2 }), '7kx 4pm'], [live(), 'AAAAAA'], [live({ attempts: 1 }), 'AAAAAA'], [live({ attempts: 2 }), 'AAAAAA'],
].map(([row, input]) => [row, input, svc.evaluateAnswer({ row: row && { ...row, expiresAt: row.expiresAt && new Date(row.expiresAt) }, guildId: 'G', userId: 'U', input, now })]);
out.timeout = [0, 4, 5, 60, 10080, 10081, -5, 2.5, '5', null].map((n) => [n, svc.validateTimeoutMinutes(n)]);
const t0 = 5_000_000_000;
out.runAt = [[t0 - 1000, 10], [t0 - 3_600_000, 10], [null, 10], [t0, 5]].map(([j, m]) => [j, m, svc.timeoutRunAt(j, m, t0).getTime()]);
const buttons = [undefined, null, { label: '  Go ', style: 'DANGER' }, { style: 'LINK' }, { style: 'toString' }, { label: 'x'.repeat(81) }, { label: '   ' }, { label: 5, style: 'PRIMARY' }, 'nope'];
out.verifyButton = buttons.map((b) => [b ?? null, svc.parseVerifyButton(b)]);
const templates = [null, { embed: { title: 'X' }, verifyButton: { label: 'Go', style: 'DANGER' } }, { embed: 5 }, 'nope', { content: 'Hi {server}' }, { content: '' }, { embed: { color: 0xff0000 } }];
out.panelTemplate = templates.map((t) => [t, svc.panelTemplate({ captchaTemplate: t })]);
out.defaultPanel = svc.defaultPanelTemplate();

const guild = { id: '900000000000000001', name: 'Test', memberCount: 3, members: { cache: new Collection() }, iconURL: () => null, roles: { cache: new Collection() } };
const channel = { id: '800000000000000001', name: 'verification', type: 0 };
out.panel = [
  { embedColor: 0x123456, captchaTemplate: null },
  { embedColor: null, captchaTemplate: { embed: { title: 'Hi {server} in {channel}' }, verifyButton: { style: 'PRIMARY', label: 'Go' } } },
  { captchaTemplate: { embed: { color: 0xff0000 } } },
  { captchaTemplate: { content: 'Press it, {user}', embed: null, buttons: [{ label: 'Rules', url: 'https://example.com' }], verifyButton: { label: 'Let me in', style: 'DANGER' } } },
].map((s) => [s, payloadJson(svc.buildPanelPayload(s, guild, channel))]);

const expiresAt = new Date(1_800_000_000_000);
const challenge = svc.buildChallengePayload({ settings: { embedColor: 0x00ff00 }, expiresAt, image: Buffer.from('png'), content: 'prefix' });
out.challenge = { ...payloadJson(challenge), files: challenge.files.map((f) => ({ name: f.name, description: f.description ?? null })) };

out.msg = {
  MODULE_OFF: svc.MSG.MODULE_OFF, FONT_MISSING: svc.MSG.FONT_MISSING,
  TEMP_UNAVAILABLE: payloadJson(svc.MSG.TEMP_UNAVAILABLE()), NOT_SET_UP: payloadJson(svc.MSG.NOT_SET_UP()), ALREADY: payloadJson(svc.MSG.ALREADY()),
  SCREENING_FIRST: payloadJson(svc.MSG.SCREENING_FIRST()), RATE_USER: payloadJson(svc.MSG.RATE_USER()), RATE_GUILD: payloadJson(svc.MSG.RATE_GUILD()),
  IMAGE_FAILED: payloadJson(svc.MSG.IMAGE_FAILED()), ROLE_FAILED: payloadJson(svc.MSG.ROLE_FAILED()), VERIFIED: payloadJson(svc.MSG.VERIFIED({ name: 'Cool **Server**' })),
};
const { buildApplyButton } = await imp('src', 'services', 'applicationService.js');
out.needsRole = [
  [['R1'], [], false], [['R1', 'R2'], ['R2'], false], [['R1'], [], true], [['R1', 'R2'], ['R1'], true],
].map(([roleIds, linkedRoleIds, form]) => [roleIds, linkedRoleIds, form, payloadJson(svc.MSG.NEEDS_ROLE({ roleIds, linkedRoleIds, formButton: form ? buildApplyButton({ id: 'FORM1' }, svc.FORM_BUTTON_LABEL) : null }))]);

out.missingRequirement = [
  [null, ['A'], ['A'], null], [['R1'], [], ['A'], null], [['R1', 'R2'], ['R2'], ['R1', 'R2'], null], [['R1', 'R2'], ['X'], ['R1', 'R2'], null],
  [['R1', 'R1', 'V'], [], ['R1', 'V'], 'V'], [['V'], [], ['V'], 'V'], [['R1', 'GONE'], [], ['R1'], null],
].map(([required, member, guildRoles, except]) => [required, member, guildRoles, except, svc.missingRequirement({ requiredRoleIds: required, memberRoleIds: member, guildRoleIds: guildRoles, exceptRoleId: except })]);
out.checkRequired = [
  [{ id: 'G' }, 'G', null], [{ id: 'V' }, 'G', 'V'], [{ id: 'B', managed: true }, 'G', null], [{ id: 'L', managed: true, tags: { guildConnections: true } }, 'G', null], [{ id: 'P' }, 'G', 'V'],
].map(([role, guildId, verifiedRoleId]) => [role, guildId, verifiedRoleId, svc.checkRequiredRole({ role, guildId, verifiedRoleId })]);
const ten = Array.from({ length: 10 }, (_, i) => `R${i}`);
out.planRequired = [
  { current: [], guildRoleIds: ['A'], addId: 'A' }, { current: ['A'], guildRoleIds: ['A'], addId: 'A' }, { current: ten, guildRoleIds: [...ten, 'X'], addId: 'X' },
  { current: ['A', 'B'], guildRoleIds: ['A', 'B'], removeId: 'A' }, { current: ['A'], guildRoleIds: ['A'], removeId: 'Z' }, { current: ['A', 'B'], guildRoleIds: ['A'], clear: true },
  { current: ['A', 'GONE'], guildRoleIds: ['A', 'B'], addId: 'B' }, { current: ['A', 'A'], guildRoleIds: ['A'] }, { current: null, guildRoleIds: [] }, { current: ['A', 'GONE'], guildRoleIds: ['A'] },
].map((c) => [c, svc.planRequiredRoles(c)]);
const form = (over = {}) => ({ id: 'F', name: 'Apply', enabled: true, reviewChannelId: 'C', questions: [{ label: 'Why?' }], requiredRoleIds: [], cooldownHours: 0, ...over });
out.fallbackProblem = [
  [null, false, null], [null, true, null], [form(), true, null], [form({ enabled: false }), true, null], [form({ reviewChannelId: null }), true, null],
  [form({ questions: [] }), true, null], [form(), true, 'I need **Send Messages** in <#C>.'], [form({ requiredRoleIds: ['X'] }), true, null], [form({ requiredRoleIds: ['X'] }), true, null, ['X']],
].map(([f, moduleOn, rcp, roles = []]) => [f, moduleOn, rcp, roles, svc.fallbackFormProblem({ form: f, moduleOn, memberRoleIds: roles, reviewChannelProblem: rcp })]);
out.progress = [{ done: 1000, total: 2000, ok: 900, failed: 5, left: 3 }, { done: 0, total: 0, ok: 0, failed: 0, left: 0 }].map((r) => [r, svc.progressText(r)]);
out.discordError = [50013, 50001, 10003, 10011, 10008, 30013, 99999, null].map((code) => [code, svc.discordErrorText(code === null ? null : { code, message: 'boom' })]);
const reqGuild = { roles: { cache: new Collection([['R1', {}], ['V', {}]]) } };
out.describe = [
  [{ captchaRequiredRoleIds: [], captchaRoleId: 'V' }, { formId: null, form: null, problem: null }],
  [{ captchaRequiredRoleIds: ['R1', 'GONE', 'V'], captchaRoleId: 'V' }, { formId: 'F', form: { name: 'Apply **now**' }, problem: null }],
  [{ captchaRequiredRoleIds: ['R1'], captchaRoleId: null }, { formId: 'F', form: null, problem: 'The form could not be checked right now.' }],
].map(([s, fb]) => [s, fb, svc.describeRequirements(reqGuild, s, fb)]);
out.keys = { timeoutJob: svc.timeoutJobKey('900000000000000001', '600000000000000001'), customIds: svc.CUSTOM_IDS };

// The answer form, captured from the button handler.
let modal = null;
await svc.handleCaptchaButton({ customId: 'captcha:answer', showModal: async (m) => { modal = m; } });
out.answerModal = json(modal);

// ── Sticky ────────────────────────────────────────────────────────────────────
const tpls = [
  { content: 'Hi', embed: { title: 'T', description: 'D' }, buttons: [{ label: 'Go', url: 'https://example.com' }] },
  { content: 'Only text' }, { embed: null, buttons: [{ label: 'L', url: 'https://example.com' }] }, null, { embed: { color: 5 } },
];
out.embedPart = tpls.map((t) => [t, sticky.embedPart(t)]);
const sguild = { id: '900000000000000002', name: 'Sticky **Guild**', memberCount: 5, members: { cache: new Collection() }, iconURL: () => null };
const schannel = { id: '800000000000000002', name: 'general', type: 0 };
out.renderSticky = [
  [{ content: 'Welcome to {server} in {channel}, {user} @everyone', embed: null }, { embedColor: 0x112233 }],
  [{ content: null, embed: { embed: { title: '{server}', description: 'x' }, buttons: [{ label: 'Docs', url: 'https://example.com/{server}' }] } }, null],
  [{ content: '   ', embed: null }, null],
  [{ content: 'a', embed: { embed: { color: 3 }, buttons: [] } }, { embedColor: -1 }],
].map(([row, s]) => [row, s, (() => { const p = sticky.renderSticky(row, { guild: sguild, channel: schannel, settings: s }); return p && payloadJson(p); })()]);
out.stickyNeeds = [sticky.stickyNeeds(true), sticky.stickyNeeds(false)];
out.setResult = [
  [{ created: true, hasEmbed: false, status: 'sent' }, ''], [{ created: false, hasEmbed: true, status: 'current' }, '\nnote'],
  [{ created: true, hasEmbed: false, status: 'off' }, ''], [{ created: true, hasEmbed: false, status: 'perms' }, ''], [{ created: true, hasEmbed: false, status: 'failed' }, ''],
  [{ created: true, hasEmbed: false, status: 'gone' }, ''],
].map(([r, notes]) => [r, notes, dropTs(stickyPlugin.setResultEmbed('800000000000000002', r, notes))]);
const rctx = { guild: sguild, channel: schannel };
out.variableNotes = [
  { content: 'Hi {server}' }, { content: 'Hi {user}' }, { content: 'Hi {user.name} {nope}' }, { content: 'Hi {USER.Mention}' }, { content: '{user.}' },
  { content: null, embed: { embed: { title: '{user}' }, buttons: [] } }, { content: '{unknown_thing} {channel}' }, {},
].map((p) => [p, stickyPlugin.variableNotes(p, rctx)]);
out.stickyModal = [json(stickyPlugin.buildStickyModal('800000000000000002', 'Current text')), json(stickyPlugin.buildStickyModal('800000000000000003', null)), json(stickyPlugin.buildStickyModal('800000000000000004', 'x'.repeat(2100)))];

const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-captcha');
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, 'golden.json'), `${JSON.stringify(out, null, 1)}\n`);
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
