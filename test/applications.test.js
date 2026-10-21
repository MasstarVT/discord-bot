// Applications tests. Offline (no Discord, Postgres or Redis): the pure rules and builders, the form and question writes
// and the draft/pending/cooldown rules against in-memory ApplicationForm / ApplicationSubmission tables, the whole
// apply -> review flow with fake interactions, the cleanup hooks, the plugin declaration and the /application payload.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { embedLength } from 'discord.js';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const svc = await import('../src/services/applicationService.js');
const cmd = await import('../src/commands/community/application.js');
const { plugin } = await import('../src/plugins/applications.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { CUSTOM_ID_PREFIXES } = await import('../src/config/customIds.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { updateSettings } = await import('../src/services/settingsService.js');

const {
  APPLICATION, parseQuestions, pageCount, pageSlice, answerLimit, cleanAnswer, normalizeAnswers, mergeAnswers, missingRequired,
  isClearWord, judgeApply, buildApplyModal, buildReviewEmbed, buildOutcomeEmbed, buildPanelPayload,
} = svc;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory tables ──────────────────────────────────────────────────────────

let tick = 1;
const forms = new Map();
const subs = new Map();

/** The slice of Prisma `where` the service uses: equality, in, has, not, equals/contains (insensitive), OR, NOT. */
function match(row, where) {
  for (const [key, cond] of Object.entries(where ?? {})) {
    if (key === 'OR') { if (!cond.some((w) => match(row, w))) return false; continue; }
    if (key === 'NOT') { if (match(row, cond)) return false; continue; }
    const value = row[key];
    if (cond !== null && typeof cond === 'object' && !(cond instanceof Date)) {
      if ('in' in cond) { if (!cond.in.includes(value)) return false; continue; }
      if ('has' in cond) { if (!(value ?? []).includes(cond.has)) return false; continue; }
      if ('not' in cond) { if (value === cond.not) return false; continue; }
      const fold = (x) => (cond.mode === 'insensitive' ? String(x).toLowerCase() : String(x));
      if ('equals' in cond) { if (fold(value) !== fold(cond.equals)) return false; continue; }
      if ('contains' in cond) { if (!fold(value).includes(fold(cond.contains))) return false; continue; }
      throw new Error(`stub: unsupported where ${JSON.stringify(cond)}`);
    }
    if (value !== cond) return false;
  }
  return true;
}
const order = (rows, orderBy) => {
  if (!orderBy) return rows;
  const [[key, dir]] = Object.entries(orderBy);
  return [...rows].sort((a, b) => (dir === 'desc' ? -1 : 1) * ((a[key]?.valueOf?.() ?? 0) - (b[key]?.valueOf?.() ?? 0)));
};
const clone = (row) => (row ? structuredClone(row) : null);

function table(map, defaults, { include = () => ({}) } = {}) {
  const view = (row, args) => (row ? { ...clone(row), ...(args?.include ? include(row) : {}) } : null);
  return {
    findFirst: async (args = {}) => view(order([...map.values()].filter((r) => match(r, args.where)), args.orderBy)[0], args),
    findMany: async (args = {}) => order([...map.values()].filter((r) => match(r, args.where)), args.orderBy).slice(0, args.take ?? 1e9).map((r) => view(r, args)),
    count: async (args = {}) => [...map.values()].filter((r) => match(r, args.where)).length,
    create: async ({ data }) => {
      const row = { ...defaults(), ...data, id: `${map === forms ? 'F' : 'S'}${tick++}`, createdAt: new Date(1_700_000_000_000 + tick) };
      map.set(row.id, row);
      return clone(row);
    },
    updateMany: async ({ where, data }) => {
      const hit = [...map.values()].filter((r) => match(r, where));
      for (const r of hit) Object.assign(r, structuredClone(data));
      return { count: hit.length };
    },
    deleteMany: async ({ where }) => {
      const hit = [...map.values()].filter((r) => match(r, where));
      for (const r of hit) map.delete(r.id);
      return { count: hit.length };
    },
  };
}

const realForm = prisma.applicationForm;
const realSub = prisma.applicationSubmission;
const realSettings = prisma.guildSettings;
prisma.applicationForm = table(forms, () => ({
  description: null, questions: [], reviewChannelId: null, acceptRoleIds: [], requiredRoleIds: [], cooldownHours: 24,
  acceptMessage: null, denyMessage: null, panelChannelId: null, panelMessageId: null, enabled: true,
}));
prisma.applicationSubmission = {
  ...table(subs, () => ({
    answers: [], status: 'DRAFT', reviewerId: null, reviewNote: null, reviewMessageId: null, submittedAt: null, reviewedAt: null,
  }), { include: (row) => ({ form: clone(forms.get(row.formId)) }) }),
  groupBy: async ({ where }) => {
    const counts = new Map();
    for (const r of subs.values()) if (match(r, where)) counts.set(r.formId, (counts.get(r.formId) ?? 0) + 1);
    return [...counts].map(([formId, n]) => ({ formId, _count: { _all: n } }));
  },
};
prisma.guildSettings = {
  upsert: async ({ where, update, create }) => ({ guildId: where.guildId, ...create, ...update }),
  findUnique: async () => null,
};

after(async () => {
  prisma.applicationForm = realForm;
  prisma.applicationSubmission = realSub;
  prisma.guildSettings = realSettings;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

let seq = 1;
const nid = (p) => `${p}${seq++}`;

function fakeChannel(id) {
  const ch = {
    id, sent: [], deleted: [], edits: [], guild: null,
    permissionsFor: () => ({ has: () => true }),
    isTextBased: () => true,
    async send(payload) { ch.sent.push(payload); return { id: nid('M'), delete: async () => {} }; },
    messages: {
      delete: async (messageId) => { ch.deleted.push(messageId); },
      edit: async (messageId, payload) => { ch.edits.push([messageId, payload]); },
    },
  };
  return ch;
}
const role = (id, { position = 3, managed = false, dangerous = false } = {}) => ({ id, position, managed, permissions: { has: () => dangerous } });

function fakeGuild(id, { channels = [], roles = [], botPosition = 10 } = {}) {
  const members = new Map();
  const guild = {
    id, name: 'Test Guild', ownerId: 'OWNER', members: null,
    channels: { cache: new Map(channels.map((c) => [c.id, c])), fetch: async (cid) => guild.channels.cache.get(cid) ?? null },
    roles: { cache: new Map(roles.map((r) => [r.id, r])) },
  };
  guild.members = {
    me: { id: 'BOT', permissions: { has: () => true }, roles: { highest: { position: botPosition } } },
    cache: members,
    fetch: async (uid) => {
      if (!members.has(uid)) throw Object.assign(new Error('Unknown Member'), { code: 10007 });
      return members.get(uid);
    },
  };
  for (const c of channels) c.guild = guild;
  return guild;
}

function fakeMember(guild, id, { roleIds = [], admin = false, dmCode = null } = {}) {
  const cache = new Map(roleIds.map((r) => [r, { id: r }]));
  const member = {
    id, guild, joinedTimestamp: 1_690_000_000_000, displayName: `name-${id}`, dms: [], given: [],
    permissions: { has: () => admin },
    roles: { cache, highest: { position: admin ? 100 : 1 }, add: async (r) => { member.given.push(r.id); cache.set(r.id, r); } },
  };
  member.user = {
    id, username: `user${id}`, createdTimestamp: 1_600_000_000_000, createdAt: new Date(1_600_000_000_000),
    send: async (payload) => { if (dmCode) throw Object.assign(new Error('closed'), { code: dmCode }); member.dms.push(payload); },
  };
  guild.members.cache.set(id, member);
  return member;
}

function fakeInteraction({ guild, member, customId = '', fields = {}, fromMessage = false, message = null }) {
  const i = {
    guild, guildId: guild.id, member, user: member.user, customId, message, channel: null, calls: [], modal: null,
    createdTimestamp: Date.now(), deferred: false, replied: false, ephemeral: null,
    inCachedGuild: () => true,
    isFromMessage: () => fromMessage,
    fields: { getTextInputValue: (key) => { if (!(key in fields)) throw new Error(`no field ${key}`); return fields[key]; } },
    async reply(p) { i.replied = true; i.calls.push(['reply', p]); },
    async deferReply(o) { i.deferred = true; i.ephemeral = Boolean(o?.ephemeral); i.calls.push(['deferReply', o]); },
    async deferUpdate() { i.deferred = true; i.calls.push(['deferUpdate']); },
    async editReply(p) { i.calls.push(['editReply', p]); },
    async followUp(p) { i.calls.push(['followUp', p]); },
    async showModal(m) { i.modal = m; i.calls.push(['showModal', m]); },
  };
  return i;
}
const lastOf = (i, kind) => [...i.calls].reverse().find((c) => c[0] === kind)?.[1];
const titleOf = (payload) => payload.embeds[0].toJSON().title;
const descOf = (payload) => payload.embeds[0].toJSON().description ?? '';
const rowIds = (payload) => (payload.components ?? []).flatMap((r) => r.toJSON().components.map((c) => c.custom_id));
const modalJson = (i) => i.modal.toJSON();

/** A modal submit for the modal `shown`, carrying `answers` (q1..q5 values). */
function submitFor(shown, { guild, member, answers = {}, fromMessage = false }) {
  const json = shown.toJSON();
  const fields = Object.fromEntries(json.components.map((row, j) => [row.components[0].custom_id, answers[j] ?? '']));
  return fakeInteraction({ guild, member, customId: json.custom_id, fields, fromMessage });
}

let guildNo = 100;
/** A guild with the module on, a review channel and a form with `count` questions. */
async function setup({ count = 7, enabled = true, mods = ['MOD'], form: formExtra = {} } = {}) {
  const gid = `G${guildNo++}`;
  await updateSettings(gid, { applicationsEnabled: enabled, modRoleIds: mods });
  const review = fakeChannel(`RC${gid}`);
  const guild = fakeGuild(gid, { channels: [review], roles: [role('R1'), role('HIGH', { position: 50 })] });
  const created = await svc.createForm({ guildId: gid, name: 'Staff', reviewChannelId: review.id });
  for (let n = 1; n <= count; n += 1) {
    await svc.addQuestion(gid, created.form.id, { label: `Question ${n}`, style: n % 2 ? 'SHORT' : 'PARAGRAPH', required: true });
  }
  if (Object.keys(formExtra).length) await svc.updateForm(gid, created.form.id, formExtra);
  const form = await svc.findForm(gid, created.form.id);
  return { gid, guild, review, form };
}
const subOf = (formId, userId) => [...subs.values()].find((r) => r.formId === formId && r.userId === userId);
const answersFor = (n) => Array.from({ length: n }, (_, k) => `answer ${k + 1}`);

// ── Pure rules ────────────────────────────────────────────────────────────────

test('parseQuestions drops malformed entries and caps the list', () => {
  assert.deepEqual(parseQuestions(null), []);
  assert.deepEqual(parseQuestions([{ label: '  Why?  ', style: 'PARAGRAPH', required: false, placeholder: ' hint ' }, { label: '' }, null, 5, { style: 'SHORT' }]),
    [{ label: 'Why?', style: 'PARAGRAPH', required: false, placeholder: 'hint' }]);
  assert.deepEqual(parseQuestions([{ label: 'A', style: 'weird' }]), [{ label: 'A', style: 'SHORT', required: true }]);
  const many = Array.from({ length: 40 }, (_, n) => ({ label: `Q${n}` }));
  assert.equal(parseQuestions(many).length, LIMITS.APPLICATION_QUESTIONS);
  assert.equal(parseQuestions([{ label: 'x'.repeat(200) }])[0].label.length, APPLICATION.LABEL_MAX);
});

test('pages: five questions each, three pages for fifteen', () => {
  assert.equal(pageCount(0), 1);
  assert.equal(pageCount(5), 1);
  assert.equal(pageCount(6), 2);
  assert.equal(pageCount(15), 3);
  assert.equal(LIMITS.APPLICATION_QUESTIONS, 3 * APPLICATION.PAGE_SIZE);
  const qs = Array.from({ length: 12 }, (_, n) => ({ label: `Q${n}` }));
  assert.deepEqual(pageSlice(qs, 2).map((q) => q.label), ['Q10', 'Q11']);
});

test('answer limits keep fifteen answers inside one embed', () => {
  assert.equal(answerLimit('SHORT', 1), APPLICATION.SHORT_MAX);
  assert.equal(answerLimit('PARAGRAPH', 1), APPLICATION.PARAGRAPH_MAX);
  assert.equal(answerLimit('PARAGRAPH', 15), Math.floor(APPLICATION.ANSWER_BUDGET / 15));
  assert.ok(answerLimit('PARAGRAPH', 15) * 15 <= APPLICATION.ANSWER_BUDGET);
  assert.equal(cleanAnswer('  hello  ', 3), 'hel');
  assert.equal(cleanAnswer(null, 10), '');
});

test('answers only count for the question they were given to', () => {
  const qs = [{ label: 'A' }, { label: 'B' }, { label: 'C' }];
  const stored = [{ question: 'A', answer: 'one' }, { question: 'Old B', answer: 'stale' }];
  assert.deepEqual(normalizeAnswers(qs, stored), ['one', '', '']);
  assert.deepEqual(normalizeAnswers(qs, 'junk'), ['', '', '']);
  assert.deepEqual(mergeAnswers(qs, stored, 0, ['x', 'y', 'z']), [
    { question: 'A', answer: 'x' }, { question: 'B', answer: 'y' }, { question: 'C', answer: 'z' },
  ]);
  const six = Array.from({ length: 6 }, (_, n) => ({ label: `Q${n}`, required: n !== 5 }));
  assert.deepEqual(missingRequired(six, ['a', '', ' ', 'd', 'e', '']), [1, 2]);
  assert.equal(isClearWord(' None '), true);
  assert.equal(isClearWord('clear'), true);
  assert.equal(isClearWord('nope'), false);
});

test('judgeApply: closed, not set up, role, pending, cooldown', () => {
  const now = new Date('2026-10-20T12:00:00Z');
  const base = { id: 'F', enabled: true, reviewChannelId: 'C', questions: [{ label: 'Q' }], requiredRoleIds: [], cooldownHours: 24 };
  const ok = { form: base, memberRoleIds: [], now };
  assert.deepEqual(judgeApply(ok), { ok: true });
  assert.equal(judgeApply({ ...ok, form: { ...base, enabled: false } }).code, 'FORM_DISABLED');
  assert.equal(judgeApply({ ...ok, form: { ...base, reviewChannelId: null } }).code, 'NOT_SET_UP');
  assert.equal(judgeApply({ ...ok, form: { ...base, questions: [] } }).code, 'NOT_SET_UP');
  assert.deepEqual(judgeApply({ ...ok, reviewChannelProblem: 'nope' }), { ok: false, code: 'REVIEW_CHANNEL', data: { reason: 'nope' } });
  const gated = { ...base, requiredRoleIds: ['A', 'B'] };
  assert.equal(judgeApply({ ...ok, form: gated, memberRoleIds: ['X'] }).code, 'MISSING_ROLE');
  assert.equal(judgeApply({ ...ok, form: gated, memberRoleIds: ['B'] }).ok, true, 'any one of the required roles is enough');
  assert.equal(judgeApply({ ...ok, pending: { id: 'S' } }).code, 'PENDING');
  const denied = { reviewedAt: new Date(now.getTime() - 3_600_000) };
  const refusal = judgeApply({ ...ok, lastDenied: denied });
  assert.equal(refusal.code, 'COOLDOWN');
  assert.equal(refusal.data.until.getTime(), now.getTime() + 23 * 3_600_000);
  assert.equal(judgeApply({ ...ok, lastDenied: { reviewedAt: new Date(now.getTime() - 25 * 3_600_000) } }).ok, true);
  assert.equal(judgeApply({ ...ok, form: { ...base, cooldownHours: 0 }, lastDenied: denied }).ok, true, 'cooldown 0 = right away');
});

// ── Builders ──────────────────────────────────────────────────────────────────

test('apply modal: page title, field ids, limits and prefilled answers', () => {
  const questions = parseQuestions(Array.from({ length: 15 }, (_, n) => ({
    label: `Question ${n + 1}`, style: n % 2 ? 'PARAGRAPH' : 'SHORT', required: n !== 6, placeholder: n === 5 ? 'a hint' : undefined,
  })));
  const form = { id: 'F', name: 'x'.repeat(APPLICATION.NAME_MAX) };
  const submission = { id: 'SUB1', answers: questions.map((q, i) => ({ question: q.label, answer: i === 5 ? 'earlier' : (i === 6 ? 'seven' : '') })) };

  const first = buildApplyModal({ form, questions, submission, page: 0 }).toJSON();
  assert.equal(first.custom_id, 'application-modal:page:SUB1:0');
  assert.ok(first.title.length <= 45, first.title);
  assert.match(first.title, /\(1\/3\)$/);
  assert.deepEqual(first.components.map((r) => r.components[0].custom_id), ['application-q1', 'application-q2', 'application-q3', 'application-q4', 'application-q5']);

  const second = buildApplyModal({ form, questions, submission, page: 1 }).toJSON();
  assert.equal(second.custom_id, 'application-modal:page:SUB1:1');
  const inputs = second.components.map((r) => r.components[0]);
  assert.equal(inputs.length, 5);
  assert.equal(inputs[0].label, 'Question 6');
  assert.equal(inputs[0].placeholder, 'a hint');
  assert.equal(inputs[0].value, 'earlier', 'a resumed draft is prefilled');
  assert.equal(inputs[1].required, false);
  assert.equal(inputs[1].value, 'seven');
  assert.equal(inputs[0].max_length, answerLimit(questions[5].style, 15));
  assert.equal(inputs[0].style, questions[5].style === 'PARAGRAPH' ? 2 : 1);

  const single = buildApplyModal({ form: { id: 'F', name: 'Solo' }, questions: questions.slice(0, 2), submission, page: 0 }).toJSON();
  assert.equal(single.title, 'Solo', 'a one-page form has no page counter');
  assert.equal(single.components.length, 2);
});

test('review embed: fifteen long answers still fit one message, before and after the outcome', () => {
  const answers = Array.from({ length: 15 }, (_, n) => ({ question: `Q${n + 1} ${'q'.repeat(40)}`, answer: 'a'.repeat(1000) }));
  const user = { id: 'U1', username: 'user`one', createdTimestamp: 1_600_000_000_000, displayAvatarURL: () => 'https://cdn.example/a.png' };
  const embed = buildReviewEmbed({ form: { name: 'Staff' }, answers, user, member: { joinedTimestamp: 1_690_000_000_000 }, submissionId: 'SUB1' });
  const json = embed.toJSON();
  assert.equal(json.fields.length, 15);
  assert.ok(embedLength(json) <= 6000, `embed is ${embedLength(json)} chars`);
  assert.match(json.description, /<@U1> · `userone`/);
  assert.match(json.description, /Account created/);
  assert.match(json.description, /Joined the server/);
  assert.match(json.footer.text, /Pending review · SUB1/);

  const done = buildOutcomeEmbed(embed, { form: { name: 'Staff' }, status: 'DENIED', reviewerId: 'M1', note: 'n'.repeat(500), extra: 'e'.repeat(2000), submissionId: 'SUB1' }).toJSON();
  assert.ok(embedLength(done) <= 6000, `reviewed embed is ${embedLength(done)} chars`);
  assert.equal(done.fields.at(-1).name, 'Outcome');
  assert.match(done.fields.at(-1).value, /Denied\*\* by <@M1>/);
  assert.equal(done.color, 0xed4245);
  assert.equal(buildOutcomeEmbed(embed, { form: { name: 'Staff' }, status: 'ACCEPTED', reviewerId: 'M1', submissionId: 'S' }).toJSON().color, 0x57f287);

  const empty = buildReviewEmbed({ form: { name: 'Staff' }, answers: [{ question: 'Q', answer: '  ' }], user, submissionId: 'S' }).toJSON();
  assert.equal(empty.fields[0].value, APPLICATION.NO_ANSWER);
});

test('panel payload: embed, Apply button, no pings', () => {
  const payload = buildPanelPayload({ id: 'FORM1', name: 'Staff', description: null, questions: [{ label: 'Q' }, { label: 'R' }] });
  assert.deepEqual(rowIds(payload), ['application:apply:FORM1']);
  assert.deepEqual(payload.allowedMentions, { parse: [] });
  assert.equal(payload.embeds[0].toJSON().footer.text, '2 questions');
  const withText = buildPanelPayload({ id: 'F', name: 'Staff', description: 'Join {server}!', questions: [] }, { id: 'G', name: 'Test Guild' });
  assert.equal(withText.embeds[0].toJSON().description, 'Join Test Guild!', 'admin text goes through the placeholder parser');
});

// ── Forms and questions ───────────────────────────────────────────────────────

test('createForm: names are unique ignoring case, and capped per guild', async () => {
  const gid = `G${guildNo++}`;
  assert.equal((await svc.createForm({ guildId: gid, name: 'Staff' })).ok, true);
  assert.deepEqual(await svc.createForm({ guildId: gid, name: 'STAFF' }), { ok: false, code: 'NAME_TAKEN' });
  for (let n = 1; n < LIMITS.APPLICATION_FORMS; n += 1) assert.equal((await svc.createForm({ guildId: gid, name: `Form ${n}` })).ok, true);
  assert.deepEqual(await svc.createForm({ guildId: gid, name: 'One too many' }), { ok: false, code: 'FORM_LIMIT' });
  assert.equal((await svc.createForm({ guildId: `G${guildNo++}`, name: 'Staff' })).ok, true, 'another guild may reuse the name');
});

test('findForm: by id, then by name; never another guild', async () => {
  const a = await setup({ count: 0 });
  const b = await setup({ count: 0 });
  assert.equal((await svc.findForm(a.gid, a.form.id)).id, a.form.id);
  assert.equal((await svc.findForm(a.gid, 'sTaFf')).id, a.form.id);
  assert.equal(await svc.findForm(b.gid, a.form.id), null);
  assert.equal(await svc.findForm(a.gid, ''), null);
});

test('updateForm: rename clash is refused, other writes go through', async () => {
  const { gid, form } = await setup({ count: 0 });
  const other = await svc.createForm({ guildId: gid, name: 'Mods' });
  assert.deepEqual(await svc.updateForm(gid, other.form.id, { name: 'staff' }), { ok: false, code: 'NAME_TAKEN' });
  const renamed = await svc.updateForm(gid, form.id, { name: 'STAFF', cooldownHours: 0 });
  assert.equal(renamed.ok, true);
  assert.equal(renamed.form.name, 'STAFF');
  assert.equal(renamed.form.cooldownHours, 0);
  assert.deepEqual(await svc.updateForm('OTHER', form.id, { cooldownHours: 1 }), { ok: false, code: 'FORM_NOT_FOUND' });
});

test('questions: cap, duplicates, numbering, removal', async () => {
  const { gid, form } = await setup({ count: 0 });
  for (let n = 1; n <= LIMITS.APPLICATION_QUESTIONS; n += 1) {
    assert.equal((await svc.addQuestion(gid, form.id, { label: `Q${n}`, style: 'SHORT', required: true })).ok, true);
  }
  assert.equal((await svc.addQuestion(gid, form.id, { label: 'More', style: 'SHORT', required: true })).code, 'QUESTION_LIMIT');
  assert.equal((await svc.removeQuestion(gid, form.id, 16)).code, 'QUESTION_NOT_FOUND');
  assert.equal((await svc.removeQuestion(gid, form.id, 0)).code, 'QUESTION_NOT_FOUND');
  const removed = await svc.removeQuestion(gid, form.id, 2);
  assert.equal(removed.removed.label, 'Q2');
  assert.equal(parseQuestions(removed.form.questions)[1].label, 'Q3');
  assert.equal((await svc.addQuestion(gid, form.id, { label: 'q3', style: 'SHORT', required: true })).code, 'QUESTION_DUP');
  assert.equal((await svc.addQuestion('OTHER', form.id, { label: 'x', style: 'SHORT', required: true })).code, 'FORM_NOT_FOUND');
});

test('changeRole: add, remove, no-op and the cap', async () => {
  const { gid, form } = await setup({ count: 0 });
  assert.equal((await svc.changeRole(gid, form.id, 'acceptRoleIds', 'A', false)).changed, true);
  assert.equal((await svc.changeRole(gid, form.id, 'acceptRoleIds', 'A', false)).changed, false);
  assert.equal((await svc.changeRole(gid, form.id, 'requiredRoleIds', 'A', true)).changed, false);
  assert.deepEqual((await svc.changeRole(gid, form.id, 'acceptRoleIds', 'A', true)).form.acceptRoleIds, []);
  for (let n = 0; n < LIMITS.APPLICATION_ROLES; n += 1) await svc.changeRole(gid, form.id, 'requiredRoleIds', `R${n}`, false);
  assert.equal((await svc.changeRole(gid, form.id, 'requiredRoleIds', 'EXTRA', false)).code, 'ROLE_LIMIT');
});

test('listForms counts pending submissions per form', async () => {
  const { gid, form } = await setup({ count: 1 });
  for (const status of ['PENDING', 'PENDING', 'DENIED', 'DRAFT']) await prisma.applicationSubmission.create({ data: { guildId: gid, formId: form.id, userId: nid('U'), status, reviewMessageId: status === 'PENDING' ? nid('RM') : null } });
  await prisma.applicationSubmission.create({ data: { guildId: gid, formId: form.id, userId: nid('U'), status: 'PENDING' } }); // orphaned: its review post is gone
  const list = await svc.listForms(gid);
  assert.equal(list.length, 1);
  assert.equal(list[0].pending, 2);
  assert.equal(await svc.pendingCount(form.id), 2);
});

// ── Draft, pending and cooldown rules ─────────────────────────────────────────

test('startDraft: one draft per member and form, even for simultaneous clicks', async () => {
  const { form } = await setup({ count: 2 });
  const [a, b] = await Promise.all([
    svc.startDraft({ form, userId: 'U1', memberRoleIds: [] }),
    svc.startDraft({ form, userId: 'U1', memberRoleIds: [] }),
  ]);
  assert.equal(a.ok && b.ok, true);
  assert.equal(a.submission.id, b.submission.id);
  assert.equal([...subs.values()].filter((s) => s.formId === form.id && s.userId === 'U1').length, 1);
  const other = await svc.startDraft({ form, userId: 'U2', memberRoleIds: [] });
  assert.notEqual(other.submission.id, a.submission.id);
});

test('startDraft: a pending application blocks, an orphaned one does not', async () => {
  const { gid, form } = await setup({ count: 2 });
  const pending = await prisma.applicationSubmission.create({ data: { guildId: gid, formId: form.id, userId: 'U1', status: 'PENDING', reviewMessageId: 'MSG' } });
  assert.equal((await svc.startDraft({ form, userId: 'U1', memberRoleIds: [] })).code, 'PENDING');
  subs.get(pending.id).reviewMessageId = null; // the review post was deleted
  assert.equal((await svc.startDraft({ form, userId: 'U1', memberRoleIds: [] })).ok, true, 'nobody can review it any more, so it must not lock the member out');
});

test('startDraft: a denied application starts the cooldown, measured from the review', async () => {
  const { gid, form } = await setup({ count: 2 });
  await prisma.applicationSubmission.create({ data: { guildId: gid, formId: form.id, userId: 'U1', status: 'DENIED', reviewedAt: new Date(Date.now() - 2 * 3_600_000) } });
  const refused = await svc.startDraft({ form, userId: 'U1', memberRoleIds: [] });
  assert.equal(refused.code, 'COOLDOWN');
  assert.ok(refused.data.until.getTime() > Date.now());
  assert.equal((await svc.startDraft({ form: { ...form, cooldownHours: 1 }, userId: 'U1', memberRoleIds: [] })).ok, true);
  assert.equal((await svc.startDraft({ form, userId: 'U9', memberRoleIds: [] })).ok, true, 'other members are not affected');
});

test('startDraft refuses before touching the database when the form cannot take applications', async () => {
  const { form } = await setup({ count: 2, formExtra: { enabled: false } });
  const before = subs.size;
  assert.equal((await svc.startDraft({ form: { ...form, enabled: false }, userId: 'U1', memberRoleIds: [] })).code, 'FORM_DISABLED');
  assert.equal((await svc.startDraft({ form: { ...form, requiredRoleIds: ['VIP'] }, userId: 'U1', memberRoleIds: ['x'] })).code, 'MISSING_ROLE');
  assert.equal(subs.size, before, 'no draft rows for refused members');
});

// ── The apply flow ────────────────────────────────────────────────────────────

test('apply: module off, unknown form and ineligible members are refused without a modal', async () => {
  const off = await setup({ enabled: false });
  const m = fakeMember(off.guild, 'A1');
  const i1 = fakeInteraction({ guild: off.guild, member: m });
  await svc.beginApplication(i1, off.form.id);
  const refusal = lastOf(i1, 'reply');
  assert.equal(titleOf(refusal), '⚠️  Module Disabled');
  assert.equal(descOf(refusal), 'The **Applications** module is turned off on this server. An admin can enable it with `/module enable applications`.');
  assert.equal(refusal.ephemeral, true);
  assert.equal(i1.modal, null);

  const on = await setup({ form: { requiredRoleIds: ['VIP'] } });
  const plain = fakeMember(on.guild, 'A2');
  const i2 = fakeInteraction({ guild: on.guild, member: plain });
  await svc.beginApplication(i2, 'no such form');
  assert.equal(titleOf(lastOf(i2, 'reply')), '❌  Application Not Found');
  const i3 = fakeInteraction({ guild: on.guild, member: plain });
  await svc.beginApplication(i3, on.form.id);
  assert.match(descOf(lastOf(i3, 'reply')), /<@&VIP>/);
  assert.equal(i3.modal, null);
  const vip = fakeMember(on.guild, 'A3', { roleIds: ['VIP'] });
  const i4 = fakeInteraction({ guild: on.guild, member: vip });
  await svc.beginApplication(i4, on.form.name);
  assert.ok(i4.modal, 'a member with the role gets the form, found by name');
});

test('apply: a review channel that was deleted or is unusable blocks the form', async () => {
  const { guild, form, review } = await setup();
  const m = fakeMember(guild, 'A1');
  review.permissionsFor = () => ({ has: () => false });
  const i1 = fakeInteraction({ guild, member: m });
  await svc.beginApplication(i1, form.id);
  assert.match(descOf(lastOf(i1, 'reply')), /I need \*\*.*\*\* in <#/);
  guild.channels.cache.delete(review.id);
  const i2 = fakeInteraction({ guild, member: m });
  await svc.beginApplication(i2, form.id);
  assert.match(descOf(lastOf(i2, 'reply')), /review channel was deleted/);
  assert.equal(i2.modal, null);
});

test('apply -> review: two pages, then accept gives the roles and messages the member', async () => {
  const { gid, guild, review, form } = await setup({ count: 7 });
  await svc.changeRole(gid, form.id, 'acceptRoleIds', 'R1', false);
  await svc.changeRole(gid, form.id, 'acceptRoleIds', 'HIGH', false); // above the bot: reported, not fatal
  const applicant = fakeMember(guild, 'A1');
  const mod = fakeMember(guild, 'M1', { roleIds: ['MOD'] });

  // Page 1 (opened by the panel's public button) is answered with a new ephemeral reply.
  const start = fakeInteraction({ guild, member: applicant, customId: `application:apply:${form.id}`, fromMessage: true });
  await svc.handleButton(start, null);
  assert.equal(modalJson(start).components.length, 5);
  const draftId = subOf(form.id, 'A1').id;
  assert.equal(modalJson(start).custom_id, `application-modal:page:${draftId}:0`);

  const page1 = submitFor(start.modal, { guild, member: applicant, answers: answersFor(5), fromMessage: true });
  await svc.handleModal(page1, null);
  assert.deepEqual(page1.calls.map((c) => c[0]), ['deferReply', 'editReply'], 'page 1 never updates the public panel');
  assert.equal(page1.calls[0][1].ephemeral, true);
  assert.deepEqual(rowIds(lastOf(page1, 'editReply')), [`application:next:${draftId}:1`]);
  assert.equal(subs.get(draftId).status, 'DRAFT');
  assert.equal(subs.get(draftId).answers.length, 7, 'answers are kept in question order');

  const next = fakeInteraction({ guild, member: applicant, customId: `application:next:${draftId}:1`, fromMessage: true });
  await svc.handleButton(next, null);
  assert.equal(modalJson(next).components.length, 2);
  assert.match(modalJson(next).title, /\(2\/2\)/);
  const stranger = fakeInteraction({ guild, member: fakeMember(guild, 'A9'), customId: `application:next:${draftId}:1`, fromMessage: true });
  await svc.handleButton(stranger, null);
  assert.equal(stranger.modal, null, "nobody can open someone else's draft");

  const page2 = submitFor(next.modal, { guild, member: applicant, answers: ['six', 'seven'], fromMessage: true });
  await svc.handleModal(page2, null);
  assert.deepEqual(page2.calls.map((c) => c[0]), ['deferUpdate', 'editReply'], 'later pages replace the Continue message');
  assert.equal(titleOf(lastOf(page2, 'editReply')), '✅  Application Submitted');
  assert.deepEqual(lastOf(page2, 'editReply').components, []);

  assert.equal(review.sent.length, 1);
  const post = review.sent[0];
  assert.deepEqual(post.allowedMentions, { parse: [] });
  assert.deepEqual(rowIds(post), [`application:accept:${draftId}`, `application:deny:${draftId}`]);
  const embed = post.embeds[0].toJSON();
  assert.equal(embed.fields.length, 7);
  assert.equal(embed.fields[6].value, 'seven');
  assert.equal(subs.get(draftId).status, 'PENDING');
  assert.ok(subs.get(draftId).reviewMessageId);

  const again = fakeInteraction({ guild, member: applicant });
  await svc.beginApplication(again, form.id);
  assert.match(descOf(lastOf(again, 'reply')), /waiting for review/);

  // A double submit cannot post twice.
  const dupe = submitFor(next.modal, { guild, member: applicant, answers: ['six', 'seven'], fromMessage: true });
  await svc.handleModal(dupe, null);
  assert.match(descOf(lastOf(dupe, 'editReply')), /no longer open or was already submitted/);
  assert.equal(review.sent.length, 1);

  // Review: members and strangers are refused, moderators decide.
  const reviewMessage = { embeds: [post.embeds[0]] };
  const outsider = fakeInteraction({ guild, member: fakeMember(guild, 'X1'), customId: `application:accept:${draftId}`, fromMessage: true, message: reviewMessage });
  await svc.handleButton(outsider, null);
  assert.equal(titleOf(lastOf(outsider, 'reply')), '❌  Insufficient Permissions');
  assert.equal(subs.get(draftId).status, 'PENDING');

  const accept = fakeInteraction({ guild, member: mod, customId: `application:accept:${draftId}`, fromMessage: true, message: reviewMessage });
  await svc.handleButton(accept, null);
  assert.equal(accept.calls[0][0], 'deferUpdate');
  assert.deepEqual(applicant.given, ['R1'], 'only the assignable role was given');
  const final = subs.get(draftId);
  assert.equal(final.status, 'ACCEPTED');
  assert.equal(final.reviewerId, 'M1');
  assert.ok(final.reviewedAt);
  const edited = lastOf(accept, 'editReply');
  assert.deepEqual(edited.components, []);
  const outcome = edited.embeds[0].toJSON().fields.at(-1);
  assert.match(outcome.value, /Accepted\*\* by <@M1>/);
  assert.match(outcome.value, /Roles given: <@&R1>/);
  assert.match(outcome.value, /Could not give <@&HIGH>: That role is above my highest role/);
  assert.equal(applicant.dms.length, 1);
  assert.match(descOf(applicant.dms[0]), /Your application for \*\*Staff\*\* in \*\*Test Guild\*\* was accepted/);
  assert.match(descOf(lastOf(accept, 'followUp')), /Could not give <@&HIGH>/);
  assert.equal(lastOf(accept, 'followUp').ephemeral, true);

  const late = fakeInteraction({ guild, member: fakeMember(guild, 'M2', { roleIds: ['MOD'] }), customId: `application:accept:${draftId}`, fromMessage: true, message: reviewMessage });
  await svc.handleButton(late, null);
  assert.match(descOf(lastOf(late, 'followUp')), /already accepted by <@M1>/);
  assert.equal(applicant.given.length, 1, 'no second role grant');
  assert.equal(applicant.dms.length, 1, 'no second DM');
});

test('review: deny asks for a reason, DMs it and starts the cooldown; the form is its own', async () => {
  const { guild, review, form } = await setup({ count: 1, form: { denyMessage: 'Sorry {user.name}, not this time.' } });
  const applicant = fakeMember(guild, 'A1');
  const mod = fakeMember(guild, 'M1', { roleIds: ['MOD'] });
  const start = fakeInteraction({ guild, member: applicant });
  await svc.beginApplication(start, form.id);
  const sent = submitFor(start.modal, { guild, member: applicant, answers: ['my answer'] });
  await svc.handleModal(sent, null);
  assert.equal(titleOf(lastOf(sent, 'editReply')), '✅  Application Submitted');
  assert.deepEqual(sent.calls.map((c) => c[0]), ['deferReply', 'editReply'], 'a one-page form is acknowledged with a reply too');
  const id = subOf(form.id, 'A1').id;
  const message = { embeds: [review.sent[0].embeds[0]] };

  const click = fakeInteraction({ guild, member: mod, customId: `application:deny:${id}`, fromMessage: true, message });
  await svc.handleButton(click, null);
  assert.equal(click.calls[0][0], 'showModal');
  const modal = modalJson(click);
  assert.equal(modal.custom_id, `application-modal:deny:${id}`);
  assert.equal(modal.components[0].components[0].custom_id, 'application-note');
  assert.equal(modal.components[0].components[0].required, false);

  const nonMod = fakeInteraction({ guild, member: fakeMember(guild, 'X1'), customId: modal.custom_id, fields: { 'application-note': 'x' }, fromMessage: true, message });
  await svc.handleModal(nonMod, null);
  assert.equal(titleOf(lastOf(nonMod, 'reply')), '❌  Insufficient Permissions', 'modal submits skip the router, so the handler checks the level');
  assert.equal(subs.get(id).status, 'PENDING');

  const deny = fakeInteraction({ guild, member: mod, customId: modal.custom_id, fields: { 'application-note': '  Too new to the server  ' }, fromMessage: true, message });
  await svc.handleModal(deny, null);
  assert.equal(subs.get(id).status, 'DENIED');
  assert.equal(subs.get(id).reviewNote, 'Too new to the server');
  assert.match(descOf(applicant.dms[0]), /Sorry userA1, not this time\./);
  assert.match(descOf(applicant.dms[0]), /\*\*Reason:\*\* Too new to the server/);
  const outcome = lastOf(deny, 'editReply').embeds[0].toJSON().fields.at(-1).value;
  assert.match(outcome, /Denied\*\* by <@M1>/);
  assert.match(outcome, /\*\*Reason:\*\* Too new to the server/);
  assert.deepEqual(lastOf(deny, 'editReply').components, []);

  const retry = fakeInteraction({ guild, member: applicant });
  await svc.beginApplication(retry, form.id);
  assert.match(descOf(lastOf(retry, 'reply')), /You can apply again <t:\d+:R>/);
  await svc.updateForm(guild.id, form.id, { cooldownHours: 0 });
  const retry2 = fakeInteraction({ guild, member: applicant });
  await svc.beginApplication(retry2, form.id);
  assert.ok(retry2.modal, 'with no cooldown the member can apply again');
});

test('review: nobody reviews their own application; a missing applicant and closed DMs are reported', async () => {
  const { guild, review, form } = await setup({ count: 1 });
  await svc.changeRole(guild.id, form.id, 'acceptRoleIds', 'R1', false);
  const mod = fakeMember(guild, 'M1', { roleIds: ['MOD'] });
  const own = fakeInteraction({ guild, member: mod });
  await svc.beginApplication(own, form.id);
  await svc.handleModal(submitFor(own.modal, { guild, member: mod, answers: ['mine'] }), null);
  const id = subOf(form.id, 'M1').id;
  const message = { embeds: [review.sent[0].embeds[0]] };
  const self = fakeInteraction({ guild, member: mod, customId: `application:accept:${id}`, fromMessage: true, message });
  await svc.handleButton(self, null);
  assert.match(descOf(lastOf(self, 'followUp')), /cannot review your own application/);
  assert.equal(subs.get(id).status, 'PENDING');

  // Applicant with closed DMs.
  const shy = fakeMember(guild, 'A2', { dmCode: 50007 });
  const a2 = fakeInteraction({ guild, member: shy });
  await svc.beginApplication(a2, form.id);
  await svc.handleModal(submitFor(a2.modal, { guild, member: shy, answers: ['hi'] }), null);
  const id2 = subOf(form.id, 'A2').id;
  const accept = fakeInteraction({ guild, member: mod, customId: `application:accept:${id2}`, fromMessage: true, message: { embeds: [review.sent[1].embeds[0]] } });
  await svc.handleButton(accept, null);
  assert.equal(subs.get(id2).status, 'ACCEPTED');
  assert.deepEqual(shy.given, ['R1']);
  assert.match(descOf(lastOf(accept, 'followUp')), /not messaged: their DMs are closed/);

  // Applicant who left before the review.
  const gone = fakeMember(guild, 'A3');
  const a3 = fakeInteraction({ guild, member: gone });
  await svc.beginApplication(a3, form.id);
  await svc.handleModal(submitFor(a3.modal, { guild, member: gone, answers: ['hi'] }), null);
  const id3 = subOf(form.id, 'A3').id;
  guild.members.cache.delete('A3');
  const accept3 = fakeInteraction({ guild, member: mod, customId: `application:accept:${id3}`, fromMessage: true, message: { embeds: [review.sent[2].embeds[0]] } });
  await svc.handleButton(accept3, null);
  assert.equal(subs.get(id3).status, 'ACCEPTED');
  assert.match(descOf(lastOf(accept3, 'followUp')), /no longer in the server, so no roles were given/);
});

test('submit: required answers, a changed form and an unpostable review channel', async () => {
  const { guild, review, form } = await setup({ count: 2 });
  const m = fakeMember(guild, 'A1');
  const start = fakeInteraction({ guild, member: m });
  await svc.beginApplication(start, form.id);
  const blank = submitFor(start.modal, { guild, member: m, answers: ['only the first', '   '] });
  await svc.handleModal(blank, null);
  const refused = lastOf(blank, 'editReply');
  assert.equal(titleOf(refused), '❌  Answer Required');
  assert.match(descOf(refused), /Question 2/);
  assert.match(rowIds(refused)[0], /^application:next:.+:0$/);
  assert.equal(review.sent.length, 0);

  // The first answer was saved, so the retry modal is prefilled.
  const retry = fakeInteraction({ guild, member: m, customId: rowIds(refused)[0], fromMessage: true });
  await svc.handleButton(retry, null);
  assert.equal(modalJson(retry).components[0].components[0].value, 'only the first');

  // Posting fails: the draft comes back so nothing is lost.
  review.send = async () => { throw Object.assign(new Error('Missing Access'), { code: 50001 }); };
  const broken = submitFor(retry.modal, { guild, member: m, answers: ['only the first', 'second'] });
  await svc.handleModal(broken, null);
  assert.match(descOf(lastOf(broken, 'editReply')), /could not post in the review channel/);
  assert.equal(subOf(form.id, 'A1').status, 'DRAFT');
});

// ── Cleanup hooks ─────────────────────────────────────────────────────────────

test('cleanup: deleted panel and review posts are forgotten, other messages are not', async () => {
  const { gid, form } = await setup({ count: 1 });
  await svc.updateForm(gid, form.id, { panelChannelId: 'PC', panelMessageId: 'PANEL' });
  const sub = await prisma.applicationSubmission.create({ data: { guildId: gid, formId: form.id, userId: 'U1', status: 'PENDING', reviewMessageId: 'REVIEW' } });
  await svc.onMessageDelete({ guildId: gid, message: { id: 'OTHER' } });
  assert.equal(forms.get(form.id).panelMessageId, 'PANEL');
  await svc.onMessageDelete({ guildId: gid, message: { id: 'PANEL' } });
  assert.equal(forms.get(form.id).panelMessageId, null);
  assert.equal(forms.get(form.id).panelChannelId, null);
  assert.equal(subs.get(sub.id).reviewMessageId, 'REVIEW');
  await svc.onMessageDelete({ guildId: gid, message: { id: 'REVIEW' } });
  assert.equal(subs.get(sub.id).reviewMessageId, null);
  assert.equal(subs.get(sub.id).status, 'PENDING');
  await svc.onMessageDelete({ guildId: 'OTHER-GUILD', message: { id: 'REVIEW' } }); // never throws
});

test('cleanup: a deleted review channel closes the form and orphans its review posts; a deleted panel channel clears the panel', async () => {
  const { gid, form, review } = await setup({ count: 1 });
  const second = await svc.createForm({ guildId: gid, name: 'Mods' });
  await svc.updateForm(gid, form.id, { panelChannelId: 'PC', panelMessageId: 'P1' });
  await svc.updateForm(gid, second.form.id, { reviewChannelId: 'OTHER-REVIEW' });
  const mine = await prisma.applicationSubmission.create({ data: { guildId: gid, formId: form.id, userId: 'U1', status: 'PENDING', reviewMessageId: 'R1' } });
  const theirs = await prisma.applicationSubmission.create({ data: { guildId: gid, formId: second.form.id, userId: 'U1', status: 'PENDING', reviewMessageId: 'R2' } });

  await svc.onChannelDelete({ guildId: gid, channel: { id: 'PC' } });
  assert.equal(forms.get(form.id).panelMessageId, null);
  assert.equal(forms.get(form.id).reviewChannelId, review.id, 'a panel channel is not the review channel');

  await svc.onChannelDelete({ guildId: gid, channel: { id: review.id } });
  assert.equal(forms.get(form.id).reviewChannelId, null);
  assert.equal(subs.get(mine.id).reviewMessageId, null);
  assert.equal(subs.get(theirs.id).reviewMessageId, 'R2');
  assert.equal(forms.get(second.form.id).reviewChannelId, 'OTHER-REVIEW');
  assert.equal((await svc.startDraft({ form: forms.get(form.id), userId: 'U5', memberRoleIds: [] })).code, 'NOT_SET_UP');
  await svc.onChannelDelete({ guildId: gid, channel: { id: 'UNRELATED' } }); // nothing to do, never throws
});

test('cleanup: a deleted role leaves the lists; the last required role closes the form instead of opening it to all', async () => {
  const { gid, form } = await setup({ count: 1 });
  await svc.changeRole(gid, form.id, 'acceptRoleIds', 'A', false);
  await svc.changeRole(gid, form.id, 'acceptRoleIds', 'B', false);
  await svc.changeRole(gid, form.id, 'requiredRoleIds', 'A', false);
  await svc.changeRole(gid, form.id, 'requiredRoleIds', 'C', false);

  await svc.onRoleDelete({ guildId: gid, role: { id: 'A' } });
  assert.deepEqual(forms.get(form.id).acceptRoleIds, ['B']);
  assert.deepEqual(forms.get(form.id).requiredRoleIds, ['C']);
  assert.equal(forms.get(form.id).enabled, true, 'another required role is left');

  await svc.onRoleDelete({ guildId: gid, role: { id: 'C' } });
  assert.deepEqual(forms.get(form.id).requiredRoleIds, []);
  assert.equal(forms.get(form.id).enabled, false);

  const open = await svc.createForm({ guildId: gid, name: 'Open' });
  await svc.changeRole(gid, open.form.id, 'acceptRoleIds', 'B', false);
  await svc.onRoleDelete({ guildId: gid, role: { id: 'B' } });
  assert.equal(forms.get(open.form.id).enabled, true, 'a form that never needed a role stays open');
});

// ── Declarations ──────────────────────────────────────────────────────────────

test('plugin declaration', async () => {
  assert.equal(plugin.id, 'applications');
  assert.equal(plugin.module, 'applications');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id, h.always]), [
    ['messageDelete', 'applications', true], ['channelDelete', 'applications', true], ['roleDelete', 'applications', true],
  ]);
  for (const h of plugin.hooks) assert.ok(HOOK_ORDER[h.event].includes('applications'), h.event);
  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix]), [['button', 'application'], ['modal', 'application-modal']]);
  for (const c of plugin.components) assert.equal(CUSTOM_ID_PREFIXES[c.prefix].owner, 'applications');
  for (const field of ['application-q1', 'application-q2', 'application-q3', 'application-q4', 'application-q5', 'application-note']) {
    assert.equal(CUSTOM_ID_PREFIXES[field].kind, 'field');
  }
  assert.deepEqual(plugin.jobs, []);
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'applications'), 'collectPlugins keeps it');
  assert.deepEqual(errors.filter((e) => String(e?.message ?? e).includes('applications')), []);
});

test('hook filters: bot and uncached messages only, text channels only', () => {
  const del = plugin.hooks.find((h) => h.event === 'messageDelete');
  const ctx = (message) => ({ client: { user: { id: 'BOT' } }, message });
  assert.equal(del.filter(ctx({ partial: true })), true);
  assert.equal(del.filter(ctx({ author: { id: 'BOT' } })), true);
  assert.equal(del.filter(ctx({ author: { id: 'SOMEONE' } })), false, 'member chat never costs a query');
  const chan = plugin.hooks.find((h) => h.event === 'channelDelete');
  assert.equal(chan.filter({ channel: { isTextBased: () => true } }), true);
  assert.equal(chan.filter({ channel: { isTextBased: () => false } }), false);
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/application payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'application');
  assert.equal(json.default_member_permissions ?? null, null, 'a mixed command: setting it would hide `apply` from members');
  assert.deepEqual(json.contexts, [0]);
  const byName = Object.fromEntries(json.options.map((o) => [o.name, o]));
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['apply', 1], ['form', 2], ['question', 2], ['panel', 1]]);
  assert.deepEqual(byName.form.options.map((o) => o.name), ['create', 'edit', 'delete', 'list', 'role']);
  assert.deepEqual(byName.question.options.map((o) => o.name), ['add', 'remove']);
  assert.deepEqual(byName.apply.options.map((o) => [o.name, o.type, o.required, o.autocomplete]), [['form', 3, true, true]]);
  const add = byName.question.options[0];
  assert.deepEqual(add.options.map((o) => o.name), ['form', 'label', 'style', 'required', 'placeholder']);
  assert.equal(add.options[1].max_length, APPLICATION.LABEL_MAX);
  assert.equal(add.options[4].max_length, APPLICATION.PLACEHOLDER_MAX);
  assert.deepEqual(add.options[2].choices.map((c) => c.value), ['SHORT', 'PARAGRAPH']);
  assert.ok(charCount(json) <= 3000, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'applications');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  const keys = new Set(json.options.flatMap((o) => (o.type === 1 ? [o.name] : o.options.map((s) => `${o.name} ${s.name}`))));
  assert.deepEqual(Object.keys(cmd.subcommandPermissions).sort(), ['form create', 'form delete', 'form edit', 'form list', 'form role', 'panel', 'question add', 'question remove']);
  for (const [key, level] of Object.entries(cmd.subcommandPermissions)) {
    assert.ok(keys.has(key), key);
    assert.equal(level, PERMISSION_LEVELS.ADMIN, key);
  }
  assert.equal(cmd.subcommandPermissions.apply, undefined, 'apply stays at the MEMBER base level');
});

// ── The command ───────────────────────────────────────────────────────────────

/** A fake chat-input interaction: `path` = [group, sub] (group may be null), `opts` = option values. */
function fakeCommand({ guild, member, path, opts = {}, channel = null }) {
  const i = fakeInteraction({ guild, member });
  i.channel = channel;
  i.options = {
    getSubcommandGroup: () => path[0],
    getSubcommand: () => path[1],
    getString: (n, req) => { if (req && opts[n] === undefined) throw new Error(`missing ${n}`); return opts[n] ?? null; },
    getInteger: (n) => opts[n] ?? null,
    getBoolean: (n) => opts[n] ?? null,
    getChannel: (n) => (opts[n] ? { id: opts[n].id } : null),
    getRole: (n) => (opts[n] ? { id: opts[n].id } : null),
    getFocused: (full) => (full ? { name: opts.__focused, value: opts[opts.__focused] ?? '' } : ''),
  };
  i.respondedWith = null;
  i.respond = async (choices) => { i.respondedWith = choices; };
  return i;
}
const embedOf = (i) => lastOf(i, 'editReply').embeds[0].toJSON();

test('/application form create | question add | role | panel: the setup path', async () => {
  const gid = `G${guildNo++}`;
  await updateSettings(gid, { applicationsEnabled: false });
  const review = fakeChannel('RV');
  const panelChannel = fakeChannel('PNL');
  const guild = fakeGuild(gid, { channels: [review, panelChannel], roles: [role('R1'), role('HIGH', { position: 50 })] });
  const admin = fakeMember(guild, 'ADMIN', { admin: true });
  const run = async (path, opts, extra = {}) => { const i = fakeCommand({ guild, member: admin, path, opts, ...extra }); await cmd.execute(i); return i; };

  const created = await run(['form', 'create'], { name: ' Staff ', description: 'Join us', review_channel: review });
  assert.equal(embedOf(created).title, '✅  Form Created');
  assert.match(embedOf(created).description, /module is off/, 'the admin is told the module is off');
  const form = await svc.findForm(gid, 'Staff');
  assert.equal(form.reviewChannelId, 'RV');
  assert.equal(form.description, 'Join us');
  assert.equal(embedOf(await run(['form', 'create'], { name: 'staff' })).title, '❌  Name Taken');

  const panelEarly = await run([null, 'panel'], { form: form.id, channel: panelChannel });
  assert.equal(embedOf(panelEarly).title, '❌  Not Ready', 'no questions yet');

  const added = await run(['question', 'add'], { form: form.id, label: 'Why us?', style: 'PARAGRAPH', required: true, placeholder: 'tell us' });
  assert.match(embedOf(added).description, /\*\*1\.\*\* Why us\? ¶/);
  assert.equal(embedOf(await run(['question', 'add'], { form: form.id, label: 'why us?' })).title, '❌  Duplicate Question');

  const bad = await run(['form', 'role'], { form: form.id, list: 'accept', role: { id: 'HIGH' } });
  assert.match(embedOf(bad).description, /above my highest role/);
  assert.deepEqual((await svc.findForm(gid, form.id)).acceptRoleIds, []);
  const good = await run(['form', 'role'], { form: form.id, list: 'accept', role: { id: 'R1' } });
  assert.equal(embedOf(good).title, '✅  Form Updated');
  assert.deepEqual((await svc.findForm(gid, form.id)).acceptRoleIds, ['R1']);
  const everyone = await run(['form', 'role'], { form: form.id, list: 'required', role: { id: gid } });
  assert.equal(embedOf(everyone).title, '❌  Cannot Use That Role');
  assert.equal(embedOf(await run(['form', 'role'], { form: form.id, list: 'required', role: { id: 'R1' } })).title, '✅  Form Updated');

  const posted = await run([null, 'panel'], { form: form.id, channel: panelChannel });
  assert.equal(embedOf(posted).title, '✅  Panel Posted');
  assert.equal(panelChannel.sent.length, 1);
  assert.deepEqual(rowIds(panelChannel.sent[0]), [`application:apply:${form.id}`]);
  const withPanel = await svc.findForm(gid, form.id);
  assert.equal(withPanel.panelChannelId, 'PNL');
  assert.ok(withPanel.panelMessageId);

  // Editing the form re-renders the panel; "none" clears text; a bad review channel is refused.
  const edited = await run(['form', 'edit'], { form: form.id, name: 'Staff Team', description: 'none', cooldown_hours: 0, enabled: false });
  assert.equal(embedOf(edited).title, '✅  Form Updated');
  assert.equal(panelChannel.edits.length, 1);
  assert.equal(panelChannel.edits[0][0], withPanel.panelMessageId);
  assert.match(panelChannel.edits[0][1].embeds[0].toJSON().title, /Staff Team/);
  const now = await svc.findForm(gid, form.id);
  assert.deepEqual([now.name, now.description, now.cooldownHours, now.enabled], ['Staff Team', null, 0, false]);
  review.permissionsFor = () => ({ has: () => false });
  assert.equal(embedOf(await run(['form', 'edit'], { form: form.id, review_channel: review })).title, '❌  Cannot Use That Channel');
  assert.equal(embedOf(await run(['form', 'edit'], { form: form.id })).title, '❌  Nothing To Change');

  // Reposting the panel replaces the old message.
  const old = withPanel.panelMessageId;
  await run([null, 'panel'], { form: form.id, channel: panelChannel });
  assert.ok(panelChannel.deleted.includes(old), 'the old panel message was deleted');

  const removed = await run(['question', 'remove'], { form: form.id, question: '1' });
  assert.equal(embedOf(removed).title, '✅  Question Removed');
  assert.equal(embedOf(await run(['question', 'remove'], { form: form.id, question: '1' })).title, '❌  Question Not Found');

  const list = await run(['form', 'list'], {});
  assert.match(embedOf(list).fields[0].value, /Staff Team\*\* · ⏸️ closed · ⚠️ no questions/);
});

test('/application form list is empty-friendly and /application apply refuses while the module is off', async () => {
  const gid = `G${guildNo++}`;
  await updateSettings(gid, { applicationsEnabled: false });
  const guild = fakeGuild(gid);
  const member = fakeMember(guild, 'U1');
  const list = fakeCommand({ guild, member, path: ['form', 'list'] });
  await cmd.execute(list);
  assert.match(embedOf(list).description, /No forms yet/);
  const apply = fakeCommand({ guild, member, path: [null, 'apply'], opts: { form: 'anything' } });
  await cmd.execute(apply);
  assert.equal(titleOf(lastOf(apply, 'reply')), '⚠️  Module Disabled');
  assert.equal(apply.modal, null);
});

test('autocomplete: members see open forms only while the module is on; admins see all; questions are numbered', async () => {
  const { gid, guild, form } = await setup({ count: 3 });
  await svc.createForm({ guildId: gid, name: 'Closed one' });
  await svc.updateForm(gid, (await svc.findForm(gid, 'Closed one')).id, { enabled: false });
  const member = fakeMember(guild, 'U1');

  const apply = fakeCommand({ guild, member, path: [null, 'apply'], opts: { __focused: 'form' } });
  await cmd.autocomplete(apply);
  assert.deepEqual(apply.respondedWith.map((c) => c.name), ['Staff'], 'closed forms are hidden from members');
  assert.equal(apply.respondedWith[0].value, form.id);

  const admin = fakeCommand({ guild, member, path: ['form', 'edit'], opts: { __focused: 'form', form: 'clo' } });
  await cmd.autocomplete(admin);
  assert.deepEqual(admin.respondedWith.map((c) => c.name), ['⏸️ Closed one']);

  const questions = fakeCommand({ guild, member, path: ['question', 'remove'], opts: { __focused: 'question', form: form.id, question: '2' } });
  await cmd.autocomplete(questions);
  assert.deepEqual(questions.respondedWith, [{ name: '2. Question 2', value: '2' }]);

  await updateSettings(gid, { applicationsEnabled: false });
  const off = fakeCommand({ guild, member, path: [null, 'apply'], opts: { __focused: 'form' } });
  await cmd.autocomplete(off);
  assert.deepEqual(off.respondedWith, [], 'no suggestions while the module is off');
});
