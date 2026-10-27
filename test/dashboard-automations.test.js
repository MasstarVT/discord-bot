// Dashboard collection `automations` (When → If → Do rules). Offline by default: descriptor lint, request validation for
// many trigger/condition/action combinations, the stored <-> form conversions of every condition and action type (round
// trips through automationService's own normalisers), button numbering, and drift tests that compare the rules the pure
// descriptor restates (src/dashboard/collections/automations.js) with automationService. With TEST_DATABASE_URL (a
// migrated database) the store also runs against Postgres with a fake Discord client:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/dashboard-automations.test.js
// The store is imported only inside the database cases (it is never loaded offline).
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here unless TEST_REDIS_URL is set: the cache fails open (every call falls through).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { LIMITS } = await import('../src/config/constants.js');
const { FieldError, validateValue, emptyRefs } = await import('../src/dashboard/schema.js');
const { COLLECTIONS, lintCollections, publicCollection, validateRecordInput } = await import('../src/dashboard/collections/index.js');
const A = await import('../src/dashboard/collections/automations.js');
// The service, for drift and round-trip checks (it opens the Redis client at import; disconnected below).
const svc = await import('../src/services/automationService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const GUILD = '955030000000000001';
const GUILD2 = '955030000000000002';
const USER = '955030000000000010';
const OTHER = '955030000000000011';
const BOT_USER = '955030000000000012';
const CH = '955030000000000020';
const CH2 = '955030000000000021';
const CAT = '955030000000000022';
const FORUM = '955030000000000023';
const ROLE = '955030000000000030';
const ROLE2 = '955030000000000031';

after(async () => {
  Object.assign(console, quiet);
  if (DB) {
    // The cascade removes the automations and saved embeds of these guilds.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: [GUILD, GUILD2] } } }).catch(() => {});
  }
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const C = COLLECTIONS.automations;
const field = (name) => C.fields.find((f) => f.field === name);
const condDesc = field('conditions');
const actDesc = field('actions');

/** validateRecordInput that returns the FieldError (or RecordError) instead of throwing. */
function invalid(data, opts = { mode: 'create' }) {
  try {
    validateRecordInput('automations', data, opts);
  } catch (err) {
    return err;
  }
  return null;
}
const create = (data) => validateRecordInput('automations', { name: 'test', ...data }, { mode: 'create' }).data;
const cond = (type, extra = {}) => ({ ...A.blankCondition(type), ...extra });
const action = (type, extra = {}) => ({ ...A.blankAction(type), ...extra });

// ── Descriptor ───────────────────────────────────────────────────────────────

test('descriptor: lint is clean, it is no placeholder, and the browser copy is JSON-safe', () => {
  assert.deepEqual(lintCollections().filter((p) => p.startsWith('automations')), []);
  assert.equal(C.placeholder, undefined);
  assert.equal(C.module, 'automations');
  assert.equal(C.limit, LIMITS.AUTOMATIONS);
  const pub = publicCollection(C);
  const json = JSON.parse(JSON.stringify(pub));
  assert.equal(json.fields.length, C.fields.length);
  assert.ok(!('check' in json));
  for (const f of C.createOnly) assert.ok(C.fields.some((d) => d.field === f), f);
  // showWhen only names sibling fields, with values the sibling can take.
  const checkShowWhen = (fields, where) => {
    const byName = new Map(fields.map((f) => [f.field, f]));
    for (const f of fields) {
      if (f.showWhen) {
        const sib = byName.get(f.showWhen.field);
        assert.ok(sib, `${where}${f.field}: showWhen names ${f.showWhen.field}`);
        for (const v of f.showWhen.in ?? []) assert.ok(sib.options.some((o) => o.value === v), `${where}${f.field}: ${v}`);
      }
      if (f.type === 'object') checkShowWhen(f.fields, `${where}${f.field}.`);
      if (f.type === 'objectList') checkShowWhen(f.itemFields, `${where}${f.field}[].`);
    }
  };
  checkShowWhen(C.fields, '');
  // Every condition and action type shows exactly the fields its parameters need (CONDITION_RULES[type].field).
  for (const [type, rule] of Object.entries(A.CONDITION_RULES)) {
    const shown = condDesc.itemFields.filter((f) => f.showWhen?.in.includes(type)).map((f) => f.field);
    if (rule.noParams) assert.deepEqual(shown, [], type);
    else assert.ok(shown.includes(rule.field), `${type} shows ${rule.field}`);
  }
});

// ── Drift: the restated rules equal automationService ────────────────────────

test('drift: triggers, condition and action tables match automationService', () => {
  assert.deepEqual(Object.keys(A.TRIGGER_RULES), Object.keys(svc.TRIGGERS));
  for (const [t, d] of Object.entries(svc.TRIGGERS)) {
    assert.equal(A.TRIGGER_RULES[t].label, d.label, t);
    assert.equal(A.TRIGGER_RULES[t].channel, d.channel, t);
  }
  const sameSet = (a, b, what) => assert.deepEqual([...a].sort(), [...b].sort(), what);
  sameSet(A.TRIGGER_SETS.ALL, svc.T_ALL, 'T_ALL');
  sameSet(A.TRIGGER_SETS.CHANNEL, svc.T_CHANNEL, 'T_CHANNEL');
  sameSet(A.TRIGGER_SETS.CONTENT, svc.T_CONTENT, 'T_CONTENT');
  sameSet(A.TRIGGER_SETS.LIVE, svc.T_LIVE, 'T_LIVE');
  sameSet(A.TRIGGER_SETS.COUNTS, svc.T_COUNTS, 'T_COUNTS');
  sameSet(A.TRIGGER_SETS.THREADY, svc.T_THREADY, 'T_THREADY');

  assert.deepEqual(Object.keys(A.CONDITION_RULES), Object.keys(svc.CONDITION_TYPES));
  for (const [t, d] of Object.entries(svc.CONDITION_TYPES)) {
    const mine = A.CONDITION_RULES[t];
    assert.equal(mine.label, d.label, t);
    sameSet(mine.triggers, d.triggers, t);
    assert.equal(mine.noParams === true, d.noParams === true, t);
    if (d.list) assert.equal(mine.field, d.list === 'userIds' ? 'users' : d.list, t);
  }
  assert.deepEqual(Object.keys(A.ACTION_RULES), Object.keys(svc.ACTION_TYPES));
  for (const [t, d] of Object.entries(svc.ACTION_TYPES)) {
    const mine = A.ACTION_RULES[t];
    assert.equal(mine.label, d.label, t);
    sameSet(mine.triggers, d.triggers, t);
    assert.equal(mine.button === true, d.button === true, t);
    assert.equal(mine.thread === true, d.thread === true, t);
    assert.equal(mine.reward ?? null, d.reward ?? null, t);
  }
  // The form's enums offer exactly these.
  assert.deepEqual(field('trigger').options.map((o) => o.value), Object.keys(svc.TRIGGERS));
  assert.deepEqual(condDesc.itemFields.find((f) => f.field === 'type').options.map((o) => o.value), Object.keys(svc.CONDITION_TYPES));
  assert.deepEqual(actDesc.itemFields.find((f) => f.field === 'type').options.map((o) => o.value), Object.keys(svc.ACTION_TYPES));
});

test('drift: constants, texts, presets and the problem key match automationService', () => {
  assert.equal(A.AUTOMATION_LIMITS.name, svc.AUTOMATION_NAME_MAX);
  assert.equal(A.AUTOMATION_LIMITS.cooldownSeconds, svc.MAX_COOLDOWN_SECONDS);
  assert.equal(A.AUTOMATION_LIMITS.rewardAmount, svc.MAX_REWARD_AMOUNT);
  assert.equal(A.AUTOMATION_LIMITS.buttonIndex, svc.MAX_BUTTON_INDEX);
  assert.deepEqual([...A.THREAD_ARCHIVE_MINUTES], [...svc.THREAD_ARCHIVE_MINUTES]);
  assert.deepEqual(actDesc.itemFields.find((f) => f.field === 'autoArchiveMinutes').options.map((o) => o.value), [...svc.THREAD_ARCHIVE_MINUTES]);
  assert.equal(A.DEFAULT_THREAD_NAME, svc.DEFAULT_THREAD_NAME);
  assert.equal(A.WELCOMER_DEFAULT_TEXT, svc.WELCOMER_DEFAULT_TEXT);
  assert.equal(A.NEEDS_ACTIONS, svc.ERROR_TEXTS.needsActions);
  assert.equal(A.BUTTONS_USED, svc.ERROR_TEXTS.buttonsUsed);
  assert.deepEqual([...A.PRESETS], [...svc.PRESETS]);
  const OPTION = {
    presetChannelId: 'channel', presetMessage: 'message', presetRoleId: 'role', presetEmoji: 'emoji', presetThreshold: 'threshold', presetUserId: 'user',
    presetStaffRoleId: 'role',   // the command's `role` option; its own field because a staff role need not be one the bot can give
  };
  for (const p of svc.PRESETS) assert.deepEqual(A.PRESET_FIELDS[p].map((f) => OPTION[f]).sort(), [...svc.PRESET_OPTIONS[p]].sort(), p);
  // The preset form offers the presets plus "build my own".
  assert.deepEqual(field('preset').options.map((o) => o.value), ['none', ...svc.PRESETS]);
  // The id a create stores for a self reference is one the service accepts.
  assert.ok(svc.normalizeCondition({ type: 'BUTTON_IS', buttons: [{ automationId: A.SELF_REF_ID, index: 1 }] }).ok);
  // problemKey is private in the service: its source must still build the same key.
  const source = readFileSync(resolve(ROOT, 'src/services/automationService.js'), 'utf8');
  assert.ok(source.includes('const problemKey = (automationId) => `automation:problem:${automationId}`;'));
  assert.equal(A.problemKey('abc'), 'automation:problem:abc');
  // The variables offered as chips are the ones buildVars fills in.
  const vars = svc.buildVars({
    message: { content: 'x', url: 'u', id: '1', author: { id: USER, username: 'n' } },
    oldContent: 'o', roleIds: [ROLE], voiceChannel: { id: CH, name: 'v' }, reaction: { display: '⭐' },
  }, { automation: { name: 'a' } });
  assert.deepEqual(A.AUTOMATION_VARIABLES.map((t) => t.slice(1, -1)).sort(), Object.keys(vars).sort());
});

test('drift: the form limits are exactly where the normalisers draw the line', () => {
  const okC = (raw) => svc.normalizeCondition(raw).ok;
  const okA = (raw) => svc.normalizeAction(raw).ok;
  const L = A.AUTOMATION_LIMITS;
  const ids = (n) => Array.from({ length: n }, (_, i) => String(955030000000100000n + BigInt(i)));
  const itemField = (list, name) => list.itemFields.find((f) => f.field === name);
  // Condition lists
  for (const [type, key] of [['CHANNEL_IN', 'channelIds'], ['CATEGORY_IN', 'categoryIds'], ['HAS_ANY_ROLE', 'roleIds'], ['USER_IS', 'userIds']]) {
    assert.ok(okC({ type, [key]: ids(L.idList) }), type);
    assert.ok(!okC({ type, [key]: ids(L.idList + 1) }), type);
  }
  assert.equal(itemField(condDesc, 'channelIds').maxItems, L.idList);
  assert.equal(itemField(condDesc, 'users').maxItems, L.idList);
  assert.ok(okC({ type: 'CONTAINS', words: ['a'.repeat(L.word)] }) && !okC({ type: 'CONTAINS', words: ['a'.repeat(L.word + 1)] }));
  assert.ok(okC({ type: 'EXACT', texts: ['a'.repeat(L.exactText)] }) && !okC({ type: 'EXACT', texts: ['a'.repeat(L.exactText + 1)] }));
  assert.equal(itemField(condDesc, 'words').itemMaxLength, L.word);
  assert.equal(itemField(condDesc, 'texts').itemMaxLength, L.exactText);
  const emojis = (n) => ['😀', '😁', '😂', '🤣', '😃', '😄', '😅', '😆', '😉', '😊', '😋'].slice(0, n);
  assert.ok(okC({ type: 'REACTED_WITH', emojis: emojis(L.reactedEmojis) }) && !okC({ type: 'REACTED_WITH', emojis: emojis(L.reactedEmojis + 1) }));
  assert.ok(okC({ type: 'HAS_REACTIONS', min: L.reactionsMin }) && !okC({ type: 'HAS_REACTIONS', min: L.reactionsMin + 1 }));
  const buttons = (n) => Array.from({ length: n }, (_, i) => ({ automationId: 'abcdefgh', index: i + 1 }));
  assert.ok(okC({ type: 'BUTTON_IS', buttons: buttons(L.idList) }) && !okC({ type: 'BUTTON_IS', buttons: buttons(L.idList + 1) }));
  // Actions
  assert.ok(okA({ type: 'REPLY', content: 'a'.repeat(L.content) }) && !okA({ type: 'REPLY', content: 'a'.repeat(L.content + 1) }));
  assert.ok(okA({ type: 'CREATE_THREAD', name: 'a'.repeat(L.threadName) }) && !okA({ type: 'CREATE_THREAD', name: 'a'.repeat(L.threadName + 1) }));
  const btn = (label, extra) => ({ type: 'REPLY', content: 'x', button: { label, ...extra } });
  assert.ok(okA(btn('a'.repeat(L.buttonLabel), { index: 1 })) && !okA(btn('a'.repeat(L.buttonLabel + 1), { index: 1 })));
  assert.ok(okA(btn('a', { index: L.buttonIndex })) && !okA(btn('a', { index: L.buttonIndex + 1 })));
  const url = (n) => `https://x.io/${'a'.repeat(n - 13)}`;
  assert.ok(okA(btn('a', { url: url(L.buttonUrl) })) && !okA(btn('a', { url: url(L.buttonUrl + 1) })));
  assert.ok(okA({ type: 'GIVE_ROLES', roleIds: ids(L.actionRoles) }) && !okA({ type: 'GIVE_ROLES', roleIds: ids(L.actionRoles + 1) }));
  assert.ok(okA({ type: 'ADD_TO_THREAD', userIds: ids(L.threadUsers) }) && !okA({ type: 'ADD_TO_THREAD', userIds: ids(L.threadUsers + 1) }));
  assert.ok(okA({ type: 'GIVE_XP', amount: L.rewardAmount }) && !okA({ type: 'GIVE_XP', amount: L.rewardAmount + 1 }));
  for (const style of A.BUTTON_STYLES) assert.ok(okA(btn('a', { index: 1, style })), style);
  assert.ok(!okA(btn('a', { index: 1, style: 'LINK' })));
  for (const [name, max] of [['content', L.content], ['threadName', L.threadName], ['amount', L.rewardAmount]]) {
    const d = itemField(actDesc, name);
    assert.equal(d.maxLength ?? d.max, max, name);
  }
  assert.equal(field('cooldownSeconds').max, svc.MAX_COOLDOWN_SECONDS);
});

test('drift: the name pattern accepts exactly what normalizeAutomationName leaves unchanged', () => {
  const re = new RegExp(`^(?:${field('name').pattern})$`, 'u');
  const samples = ['a', 'my-auto', 'my_auto', '-a', 'a-', 'a--b', 'A', 'a b', 'ünï', 'x'.repeat(32), 'x'.repeat(33), '_', '__a__', '0-0', 'a.b', ''];
  let seed = 7;
  const rand = () => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed / 0x7fffffff; };
  const alphabet = 'ab-_Z 9é.';
  for (let i = 0; i < 2000; i++) samples.push(Array.from({ length: 1 + Math.floor(rand() * 8) }, () => alphabet[Math.floor(rand() * alphabet.length)]).join(''));
  for (const s of samples) assert.equal(re.test(s), svc.normalizeAutomationName(s) === s, JSON.stringify(s));
});

test('drift: shapeProblems says what validateAutomationShape says, word for word', () => {
  const triggers = Object.keys(svc.TRIGGERS);
  const condTypes = Object.keys(svc.CONDITION_TYPES);
  const actTypes = Object.keys(svc.ACTION_TYPES);
  const compare = (shape) => assert.deepEqual(A.shapeProblems(shape).map((p) => p.message), svc.validateAutomationShape(shape), JSON.stringify(shape));
  // Every type with every trigger, alone, with and without a channel.
  for (const trigger of triggers) {
    for (const type of condTypes) compare({ trigger, conditions: [{ type }], actions: [], cooldownSeconds: 0 });
    for (const type of actTypes) {
      compare({ trigger, conditions: [], actions: [{ type }], cooldownSeconds: 0 });
      compare({ trigger, conditions: [], actions: [{ type, channelId: CH }], cooldownSeconds: 0 });
      compare({ trigger, conditions: [], actions: [{ type: 'CREATE_THREAD', channelId: CH }, { type }], cooldownSeconds: 0 });
    }
  }
  // Random mixes: repeated flags, IS_REPLY with IS_NOT_REPLY, thread order, button numbers, caps, cooldowns, unknown types.
  let seed = 42;
  const rand = (n) => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return Math.floor((seed / 0x7fffffff) * n); };
  const pick = (list) => list[rand(list.length)];
  for (let i = 0; i < 3000; i++) {
    const conditions = Array.from({ length: rand(13) }, () => ({ type: rand(30) === 0 ? 'NOPE' : pick(condTypes) }));
    const actions = Array.from({ length: rand(13) }, () => {
      const a = { type: rand(30) === 0 ? 'NOPE' : pick(actTypes) };
      if (rand(2)) a.channelId = CH;
      const b = rand(4);
      if (b === 1) a.button = { url: null, index: 1 + rand(4) };
      else if (b === 2) a.button = { url: 'https://example.com', index: null };
      return a;
    });
    const cooldownSeconds = pick([0, 5, -1, 604_800, 604_801, 1.5, undefined]);
    compare({ trigger: rand(40) === 0 ? 'NOPE' : pick(triggers), conditions, actions, cooldownSeconds });
  }
});

// ── Round trips: stored <-> form, through the service's normalisers ──────────

const STORED_CONDITIONS = [
  { type: 'CHANNEL_IN', channelIds: [CH, CH2] },
  { type: 'CHANNEL_NOT_IN', channelIds: [CH] },
  { type: 'CATEGORY_IN', categoryIds: [CAT] },
  { type: 'CATEGORY_NOT_IN', categoryIds: [CAT] },
  { type: 'CONTAINS', words: ['hello', 'good morning'] },
  { type: 'EXACT', texts: ['!rules', 'Read the rules please'] },
  { type: 'NOT_CONTAINS', words: ['spam'] },
  { type: 'MATCHES_REGEX', pattern: '^hi\\b', flags: 'i' },
  { type: 'MATCHES_REGEX', pattern: ' spaced ', flags: '' },
  { type: 'MATCHES_REGEX', pattern: 'a.b', flags: 'imsu' },
  { type: 'ATTACHMENT_AUDIO' }, { type: 'ATTACHMENT_IMAGE' }, { type: 'ATTACHMENT_VIDEO' }, { type: 'ATTACHMENT_TEXT' },
  { type: 'REACTED_WITH', emojis: ['⭐', '<:pepe:955030000000000099>', '👍🏽'] },
  { type: 'HAS_REACTIONS', min: 5, emoji: null },
  { type: 'HAS_REACTIONS', min: 1000, emoji: '<a:party:955030000000000098>' },
  { type: 'IS_REPLY' }, { type: 'IS_NOT_REPLY' },
  { type: 'HAS_ANY_ROLE', roleIds: [ROLE, ROLE2] },
  { type: 'HAS_ALL_ROLES', roleIds: [ROLE] },
  { type: 'HAS_NOT_ANY_ROLE', roleIds: [ROLE] },
  { type: 'HAS_NOT_ALL_ROLES', roleIds: [ROLE2] },
  { type: 'GAINED_ROLE', roleIds: [ROLE] },
  { type: 'LOST_ROLE', roleIds: [ROLE] },
  { type: 'USER_IS', userIds: [USER, OTHER] },
  { type: 'BUTTON_IS', buttons: [{ automationId: 'clotherautomation0001', index: 3 }, { automationId: 'cldeletedautomation01', index: 1 }] },
];

const STORED_ACTIONS = [
  { type: 'CREATE_THREAD', name: svc.DEFAULT_THREAD_NAME, channelId: null, autoArchiveMinutes: 1440 },
  { type: 'CREATE_THREAD', name: 'Ticket for {user.name}', channelId: CH, autoArchiveMinutes: 10080 },
  { type: 'SEND_MESSAGE', content: 'Welcome {user}!', channelId: null, button: null },
  { type: 'SEND_MESSAGE', content: 'Hi', channelId: CH, button: { label: 'Rules', url: 'https://example.com/rules', style: 'PRIMARY', index: null } },
  { type: 'SEND_MESSAGE', content: 'Click', channelId: null, button: { label: 'Verify', url: null, style: 'SUCCESS', index: 4 } },
  { type: 'REPLY', content: 'Pong', button: null },
  { type: 'REPLY', content: 'Pong', button: { label: 'Again', url: null, style: 'DANGER', index: 1 } },
  { type: 'REPOST', channelId: CH2 },
  { type: 'PIN' },
  { type: 'DELETE' },
  { type: 'REACT', emoji: '✅' },
  { type: 'REACT', emoji: '<:pepe:955030000000000099>' },
  { type: 'REMOVE_REACTION', emoji: null },
  { type: 'REMOVE_REACTION', emoji: '❌' },
  { type: 'GIVE_ROLES', roleIds: [ROLE, ROLE2] },
  { type: 'TAKE_ROLES', roleIds: [ROLE] },
  { type: 'ADD_TO_THREAD', userIds: [] },
  { type: 'ADD_TO_THREAD', userIds: [USER, OTHER] },
  { type: 'SEND_IN_THREAD', content: 'Thread hello', button: { label: 'Close', url: null, style: 'SECONDARY', index: 2 } },
  { type: 'GIVE_COINS', amount: 50 },
  { type: 'TAKE_COINS', amount: 1 },
  { type: 'GIVE_XP', amount: 1_000_000 },
  { type: 'TAKE_XP', amount: 10 },
  { type: 'SEND_EMBED', embedName: 'rules', channelId: null },
  { type: 'SEND_EMBED', embedName: 'rules', channelId: CH },
];

const NAMES = new Map([['clotherautomation0001', 'other-auto']]);
const nameOf = (id) => NAMES.get(id) ?? null;
const idOf = (text) => [...NAMES].find(([, n]) => n === text)?.[0] ?? text;
const asValid = (desc, value, path) => validateValue(desc, value, path, emptyRefs());

test('round trip: every stored condition survives stored → form → stored', () => {
  const covered = new Set();
  for (const raw of STORED_CONDITIONS) {
    const stored = svc.normalizeCondition(raw);
    assert.ok(stored.ok, `${raw.type}: ${stored.error}`);
    const form = A.conditionToForm(stored.value, nameOf);
    // The form shape is what the browser sends back, so it must validate against the item descriptor.
    const valid = asValid({ ...condDesc, field: 'conditions' }, [form], 'conditions')[0];
    assert.deepEqual(valid, form, raw.type);
    const back = svc.normalizeCondition(A.conditionToStored(valid, idOf));
    assert.ok(back.ok, `${raw.type}: ${back.error}`);
    assert.deepEqual(back.value, stored.value, raw.type);
    // And the canonical form survives form → stored → form.
    assert.deepEqual(A.conditionToForm(back.value, nameOf), form, raw.type);
    covered.add(raw.type);
  }
  assert.deepEqual([...covered].sort(), Object.keys(svc.CONDITION_TYPES).sort());
  // A known automation shows by name, a deleted one by its id.
  const btn = A.conditionToForm(svc.normalizeCondition(STORED_CONDITIONS.at(-1)).value, nameOf);
  assert.deepEqual(btn.buttons, [{ automation: 'other-auto', index: 3 }, { automation: 'cldeletedautomation01', index: 1 }]);
});

test('round trip: every stored action survives stored → form → stored', () => {
  const covered = new Set();
  for (const raw of STORED_ACTIONS) {
    const stored = svc.normalizeAction(raw);
    assert.ok(stored.ok, `${raw.type}: ${stored.error}`);
    const form = A.actionToForm(stored.value);
    const valid = asValid({ ...actDesc, field: 'actions' }, [form], 'actions')[0];
    assert.deepEqual(valid, form, raw.type);
    const back = svc.normalizeAction(A.actionToStored(valid));
    assert.ok(back.ok, `${raw.type}: ${back.error}`);
    assert.deepEqual(back.value, stored.value, JSON.stringify(raw));
    assert.deepEqual(A.actionToForm(back.value), form, raw.type);
    covered.add(raw.type);
  }
  assert.deepEqual([...covered].sort(), Object.keys(svc.ACTION_TYPES).sort());
});

test('round trip: a whole automation row → item → create data → stored again', () => {
  const row = {
    id: 'clrowautomation000001', name: 'everything', trigger: 'REACTION_ADDED', enabled: true, cooldownSeconds: 30, uses: 7,
    createdBy: USER, createdAt: new Date('2026-01-01T00:00:00Z'), updatedAt: new Date('2026-01-02T00:00:00Z'),
    conditions: svc.normalizeList([
      { type: 'REACTED_WITH', emojis: ['⭐'] }, { type: 'HAS_REACTIONS', min: 3, emoji: '⭐' }, { type: 'CHANNEL_NOT_IN', channelIds: [CH] },
    ], 'condition').items,
    actions: svc.normalizeList([{ type: 'REPOST', channelId: CH }, { type: 'REACT', emoji: '🌟' }], 'action').items,
  };
  const item = A.automationItem(row, { problems: ['p'] });
  assert.equal(item.summary, 'Reaction added · 3 conditions · 2 actions');
  assert.deepEqual(item.problems, ['p']);
  assert.equal(item.createdAt, '2026-01-01T00:00:00.000Z');
  assert.equal(item.createdBy, USER);
  assert.equal(A.automationItem(row, { creator: '@alice' }).createdBy, '@alice');
  const data = Object.fromEntries(C.fields.map((f) => [f.field, item[f.field]]));
  const { data: valid } = validateRecordInput('automations', data, { mode: 'create' });
  assert.deepEqual(valid, data);
  assert.deepEqual(valid.conditions.map((c) => svc.normalizeCondition(A.conditionToStored(c)).value), row.conditions);
  assert.deepEqual(valid.actions.map((a) => svc.normalizeAction(A.actionToStored(a)).value), row.actions);
  assert.equal(A.summaryOf({ trigger: 'MEMBER_JOINED', conditions: [], actions: [{}] }), 'Member joined · always · 1 action');
});

// ── Validation: happy paths ──────────────────────────────────────────────────

test('validate: typical automations pass', () => {
  const cases = [
    {
      trigger: 'MESSAGE_SENT',
      conditions: [cond('CONTAINS', { words: ['hello', 'Hi'] }), cond('CHANNEL_IN', { channelIds: [CH] }), cond('HAS_NOT_ANY_ROLE', { roleIds: [ROLE] })],
      actions: [action('REPLY', { content: 'Hey {user}!', button: { kind: 'automation', label: 'More', url: null, style: 'SUCCESS', index: null } }),
        action('REACT', { emoji: '👋' }), action('GIVE_COINS', { amount: 5 })],
    },
    { trigger: 'MEMBER_JOINED', actions: [action('SEND_MESSAGE', { content: 'Welcome {user}', channelId: CH }), action('GIVE_ROLES', { roleIds: [ROLE] })] },
    {
      trigger: 'REACTION_ADDED',
      conditions: [cond('REACTED_WITH', { emojis: [{ emoji: '⭐' }] }), cond('HAS_REACTIONS', { min: 3, emoji: '⭐' })],
      actions: [action('REPOST', { channelId: CH2 }), action('REMOVE_REACTION')],
    },
    { trigger: 'THREAD_CREATED', conditions: [cond('CHANNEL_IN', { channelIds: [FORUM] })], actions: [action('ADD_TO_THREAD', { users: [{ userId: USER }] }), action('SEND_IN_THREAD', { content: 'Hi' })] },
    { trigger: 'MEMBER_JOINED', actions: [action('CREATE_THREAD', { threadChannelId: CH }), action('ADD_TO_THREAD'), action('SEND_IN_THREAD', { content: 'Welcome' })] },
    { trigger: 'BUTTON_CLICKED', conditions: [cond('BUTTON_IS', { buttons: [{ automation: 'test', index: 1 }] })], actions: [action('GIVE_ROLES', { roleIds: [ROLE] })] },
    { trigger: 'ROLE_GAINED', conditions: [cond('GAINED_ROLE', { roleIds: [ROLE] })], actions: [action('GIVE_XP', { amount: 100 }), action('SEND_EMBED', { embedName: 'rules', channelId: CH })] },
    { trigger: 'MESSAGE_DELETED', conditions: [cond('MATCHES_REGEX', { pattern: '^!\\w+', flags: ['i', 'm'] }), cond('IS_NOT_REPLY')], actions: [action('SEND_MESSAGE', { content: 'Deleted: {message.content}' })] },
    { trigger: 'VOICE_JOINED', conditions: [cond('USER_IS', { users: [{ userId: USER }] })], actions: [action('SEND_MESSAGE', { content: '{user} joined {voice}' })] },
    { trigger: 'MESSAGE_EDITED', conditions: [cond('EXACT', { texts: ['ping'] }), cond('ATTACHMENT_IMAGE')], actions: [action('PIN'), action('DELETE')] },
    { trigger: 'ROLE_LOST', conditions: [cond('LOST_ROLE', { roleIds: [ROLE] })], actions: [action('TAKE_XP', { amount: 1 })], enabled: false },
    { trigger: 'MESSAGE_SENT', actions: [], enabled: false }, // a blank automation, saved off
  ];
  for (const c of cases) assert.doesNotThrow(() => create(c), JSON.stringify(c).slice(0, 120));
  // The values come back normalised and complete.
  const d = create(cases[0]);
  assert.equal(d.preset, 'none');
  assert.deepEqual(d.conditions[0].words, ['hello', 'Hi']);
  assert.equal(d.actions[0].button.kind, 'automation');
  assert.equal(d.cooldownSeconds, 0);
  assert.equal(d.enabled, true);
});

test('validate: presets need what /automation create needs', () => {
  assert.doesNotThrow(() => create({ preset: 'welcomer', presetChannelId: CH }));
  assert.doesNotThrow(() => create({ preset: 'starboard', presetChannelId: CH, presetEmoji: '🔥', presetThreshold: 5 }));
  assert.doesNotThrow(() => create({ preset: 'thread-joiner', presetUserId: USER }));
  assert.doesNotThrow(() => create({ preset: 'commands-only', presetChannelId: CH }));
  assert.doesNotThrow(() => create({ preset: 'commands-only', presetChannelId: CH, presetStaffRoleId: ROLE }));
  // With a preset the builder is ignored, so an empty one is fine.
  assert.doesNotThrow(() => create({ preset: 'welcomer', presetChannelId: CH, actions: [] }));
  const e1 = invalid({ name: 'x', preset: 'welcomer' });
  assert.equal(e1.path, 'presetChannelId');
  assert.match(e1.message, /Welcomer preset needs a channel/);
  assert.equal(invalid({ name: 'x', preset: 'starboard' }).path, 'presetChannelId');
  assert.equal(invalid({ name: 'x', preset: 'thread-joiner' }).path, 'presetUserId');
  assert.match(invalid({ name: 'x', preset: 'commands-only' }).message, /Commands Only preset needs the channel/);
  assert.equal(invalid({ name: 'x', preset: 'starboard', presetChannelId: CH, presetThreshold: 0 }).path, 'presetThreshold');
  // Presets are create-only.
  const current = A.automationItem({ id: 'clx', name: 'x', trigger: 'MESSAGE_SENT', conditions: [], actions: [] });
  assert.match(invalid({ preset: 'welcomer' }, { mode: 'update', current }).message, /cannot be changed after creation/);
});

// ── Validation: refusals ─────────────────────────────────────────────────────

test('validate: conditions and actions that do not fit the trigger are refused at their field', () => {
  let e = invalid({ name: 'x', trigger: 'MEMBER_JOINED', conditions: [cond('CONTAINS', { words: ['a'] })], actions: [action('SEND_MESSAGE', { content: 'x', channelId: CH })] });
  assert.equal(e.path, 'conditions.0.type');
  assert.equal(e.message, 'Condition 1: "Message contains words" doesn\'t work with the trigger "Member joined".');
  e = invalid({ name: 'x', trigger: 'ROLE_GAINED', actions: [action('REPLY', { content: 'x' })] });
  assert.equal(e.path, 'actions.0.type');
  e = invalid({ name: 'x', trigger: 'MEMBER_JOINED', actions: [action('SEND_MESSAGE', { content: 'x' })] });
  assert.equal(e.path, 'actions.0.channelId');
  assert.match(e.message, /doesn't happen in a channel/);
  e = invalid({ name: 'x', trigger: 'MEMBER_JOINED', actions: [action('CREATE_THREAD')] });
  assert.equal(e.path, 'actions.0.threadChannelId');
  e = invalid({ name: 'x', trigger: 'MEMBER_JOINED', actions: [action('ADD_TO_THREAD'), action('CREATE_THREAD', { threadChannelId: CH })] });
  assert.equal(e.path, 'actions.0.type');
  assert.match(e.message, /there is no thread here/);
  e = invalid({ name: 'x', trigger: 'MESSAGE_SENT', conditions: [cond('IS_REPLY'), cond('IS_REPLY')], actions: [action('PIN')] });
  assert.equal(e.path, 'conditions.1.type');
  e = invalid({ name: 'x', trigger: 'MESSAGE_SENT', conditions: [cond('IS_NOT_REPLY'), cond('IS_REPLY')], actions: [action('PIN')] });
  assert.equal(e.path, 'conditions.1.type');
  assert.match(e.message, /can never both be true/);
  // Changing the trigger of a stored automation checks what it already has.
  const current = A.automationItem({
    id: 'clx', name: 'x', trigger: 'MESSAGE_SENT', enabled: true, conditions: [{ type: 'CONTAINS', words: ['a'] }], actions: [{ type: 'PIN' }],
  });
  e = invalid({ trigger: 'ROLE_GAINED' }, { mode: 'update', current });
  assert.equal(e.path, 'conditions.0.type');
});

test('validate: each type asks for its parameters', () => {
  const expectPath = (data, path, re) => {
    const e = invalid({ name: 'x', trigger: 'MESSAGE_SENT', ...data });
    assert.ok(e, `${path}: accepted`);
    assert.equal(e.path, path, e.message);
    if (re) assert.match(e.message, re);
  };
  expectPath({ conditions: [cond('CHANNEL_IN')], actions: [action('PIN')] }, 'conditions.0.channelIds', /at least one channel/);
  expectPath({ conditions: [cond('CATEGORY_NOT_IN')], actions: [action('PIN')] }, 'conditions.0.categoryIds');
  expectPath({ conditions: [cond('CONTAINS')], actions: [action('PIN')] }, 'conditions.0.words');
  expectPath({ conditions: [cond('EXACT')], actions: [action('PIN')] }, 'conditions.0.texts');
  expectPath({ conditions: [cond('MATCHES_REGEX')], actions: [action('PIN')] }, 'conditions.0.pattern', /Write the regular expression/);
  expectPath({ conditions: [cond('MATCHES_REGEX', { pattern: '(a+)+' })], actions: [action('PIN')] }, 'conditions.0.pattern', /^Invalid regex: Nested repetition/);
  expectPath({ conditions: [cond('MATCHES_REGEX', { pattern: '(' })], actions: [action('PIN')] }, 'conditions.0.pattern', /^Invalid regex/);
  expectPath({ conditions: [cond('HAS_ANY_ROLE')], actions: [action('PIN')] }, 'conditions.0.roleIds');
  expectPath({ conditions: [cond('USER_IS')], actions: [action('PIN')] }, 'conditions.0.users');
  expectPath({ conditions: [cond('USER_IS', { users: [{ userId: 'nope' }] })], actions: [action('PIN')] }, 'conditions.0.users.0.userId');
  expectPath({ actions: [action('SEND_MESSAGE')] }, 'actions.0.content', /Write the message/);
  expectPath({ actions: [action('SEND_MESSAGE', { content: '   ' })] }, 'actions.0.content');
  expectPath({ actions: [action('REPLY', { content: 'x', button: { kind: 'link', label: null, url: 'https://a.io', style: 'PRIMARY', index: null } })] }, 'actions.0.button.label');
  expectPath({ actions: [action('REPLY', { content: 'x', button: { kind: 'link', label: 'Go', url: null, style: 'PRIMARY', index: null } })] }, 'actions.0.button.url');
  expectPath({ actions: [action('REPLY', { content: 'x', button: { kind: 'link', label: 'Go', url: 'ftp://a', style: 'PRIMARY', index: null } })] }, 'actions.0.button.url');
  expectPath({ actions: [action('REPOST')] }, 'actions.0.channelId', /repost/);
  expectPath({ actions: [action('REACT')] }, 'actions.0.emoji');
  expectPath({ actions: [action('REACT', { emoji: 'not an emoji' })] }, 'actions.0.emoji');
  expectPath({ actions: [action('GIVE_ROLES')] }, 'actions.0.roleIds');
  expectPath({ actions: [action('SEND_EMBED')] }, 'actions.0.embedName');
  expectPath({ actions: [action('SEND_EMBED', { embedName: 'Bad Name' })] }, 'actions.0.embedName');
  expectPath({ actions: [action('GIVE_XP', { amount: 0 })] }, 'actions.0.amount');
  expectPath({ actions: [action('GIVE_COINS', { amount: 1_000_001 })] }, 'actions.0.amount');
  expectPath({ actions: [action('CREATE_THREAD', { threadName: 'x'.repeat(101) })] }, 'actions.0.threadName');
  expectPath({ actions: [action('CREATE_THREAD', { autoArchiveMinutes: 30 })] }, 'actions.0.autoArchiveMinutes');
  expectPath({ cooldownSeconds: 604_801, actions: [action('PIN')] }, 'cooldownSeconds');
  // A parameter of another type is not required (hidden fields keep their defaults).
  assert.doesNotThrow(() => create({ trigger: 'MESSAGE_SENT', actions: [action('PIN', { content: null, roleIds: [] })] }));
});

test('validate: names, limits and unknown keys', () => {
  assert.equal(invalid({ trigger: 'MESSAGE_SENT' }).path, 'name');
  assert.equal(invalid({ name: 'Has Space', trigger: 'MESSAGE_SENT' }).path, 'name');
  assert.equal(invalid({ name: '-dash', trigger: 'MESSAGE_SENT' }).path, 'name');
  assert.equal(invalid({ name: 'x'.repeat(33) }).path, 'name');
  const many = (n, f) => Array.from({ length: n }, f);
  assert.equal(invalid({ name: 'x', conditions: many(11, () => cond('IS_REPLY')) }).path, 'conditions');
  assert.equal(invalid({ name: 'x', actions: many(11, () => action('PIN')) }).path, 'actions');
  assert.equal(invalid({ name: 'x', bogus: 1 }).path, 'bogus');
  assert.equal(invalid({ name: 'x', actions: [{ ...action('PIN'), extra: 1 }] }).path, 'actions.0.extra');
  assert.equal(invalid({ name: 'x', trigger: 'NOPE' }).path, 'trigger');
  assert.equal(invalid({ name: 'x', conditions: [cond('NOPE')] }).path, 'conditions.0.type');
});

test('validate: on needs an action; emptying the actions of an update is left to the store', () => {
  let e = invalid({ name: 'x', trigger: 'MESSAGE_SENT', actions: [] });
  assert.equal(e.path, 'enabled');
  assert.equal(e.message, svc.ERROR_TEXTS.needsActions);
  const on = A.automationItem({ id: 'clx', name: 'x', trigger: 'MESSAGE_SENT', enabled: true, conditions: [], actions: [{ type: 'PIN' }] });
  const off = A.automationItem({ id: 'clx', name: 'x', trigger: 'MESSAGE_SENT', enabled: false, conditions: [], actions: [] });
  // Removing the last action of an automation that is on: accepted (the store turns it off, as /automation action remove).
  assert.equal(invalid({ actions: [] }, { mode: 'update', current: on }), null);
  // Turning on one without actions, or sending both: refused.
  assert.equal(invalid({ enabled: true }, { mode: 'update', current: off }).path, 'enabled');
  assert.equal(invalid({ enabled: true, actions: [] }, { mode: 'update', current: on }).path, 'enabled');
  // Turning on with a first action: fine.
  assert.equal(invalid({ enabled: true, actions: [action('PIN')] }, { mode: 'update', current: off }), null);
  e = invalid({ name: 'x', trigger: 'MESSAGE_SENT', actions: [], enabled: false });
  assert.equal(e, null);
});

test('validate: automation button numbers', () => {
  const button = (index) => ({ kind: 'automation', label: 'B', url: null, style: 'PRIMARY', index });
  // New automations get their numbers when saved.
  let e = invalid({ name: 'x', trigger: 'MESSAGE_SENT', actions: [action('REPLY', { content: 'x', button: button(3) })] });
  assert.equal(e.path, 'actions.0.button.index');
  assert.match(e.message, /Leave it empty/);
  const current = A.automationItem({
    id: 'clx', name: 'x', trigger: 'MESSAGE_SENT', enabled: true, conditions: [],
    actions: svc.normalizeList([{ type: 'REPLY', content: 'a', button: { label: 'A', index: 2 } }, { type: 'SEND_MESSAGE', content: 'b', button: { label: 'B', index: 5 } }], 'action').items,
  });
  assert.equal(current.actions[1].button.index, 5);
  // Keeping, moving and adding buttons is fine; a number that is not one of its buttons, or used twice, is not.
  assert.equal(invalid({ actions: [action('REPLY', { content: 'x', button: button(5) }), action('REPLY', { content: 'y', button: button(null) })] },
    { mode: 'update', current }), null);
  e = invalid({ actions: [action('REPLY', { content: 'x', button: button(7) })] }, { mode: 'update', current });
  assert.equal(e.path, 'actions.0.button.index');
  e = invalid({ actions: [action('REPLY', { content: 'x', button: button(2) }), action('REPLY', { content: 'x', button: button(2) })] }, { mode: 'update', current });
  assert.equal(e.path, 'actions.1.button.index');
  assert.match(e.message, /used twice/);
});

test('planButtons numbers new buttons like /automation action add', () => {
  const b = (index = null) => action('REPLY', { content: 'x', button: { kind: 'automation', label: 'B', url: null, style: 'PRIMARY', index } });
  const link = action('REPLY', { content: 'x', button: { kind: 'link', label: 'L', url: 'https://a.io', style: 'PRIMARY', index: null } });
  assert.deepEqual(A.planButtons([b(), link, b(), action('PIN')]), { indexes: [1, null, 2, null], buttonSeq: 2 });
  // After buttonSeq: a removed button's number is never given again.
  assert.deepEqual(A.planButtons([b(2), b()], { existing: [2], buttonSeq: 6 }), { indexes: [2, 7], buttonSeq: 7 });
  // Matches the service's nextButtonSeq for a single new button.
  for (const [existing, buttonSeq] of [[[], 0], [[3], 0], [[3], 9], [[1, 2, 40], 12]]) {
    const row = { buttonSeq, actions: existing.map((index) => ({ type: 'REPLY', content: 'x', button: { label: 'b', url: null, style: 'PRIMARY', index } })) };
    assert.equal(A.planButtons([b()], { existing, buttonSeq }).indexes[0], svc.nextButtonSeq(row));
  }
  assert.deepEqual(A.planButtons([b()], { existing: [], buttonSeq: 99 }).error, { path: 'actions.0.button', message: svc.ERROR_TEXTS.buttonsUsed });
  assert.equal(A.planButtons([b(4)], { existing: [3] }).error.path, 'actions.0.button.index');
  // Stored through the service, the planned numbers validate.
  const plan = A.planButtons([b(), b()]);
  const stored = [b(), b()].map((a, i) => svc.normalizeAction(A.actionToStored(a, plan.indexes[i])).value);
  assert.deepEqual(stored.map((a) => a.button.index), [1, 2]);
  assert.deepEqual(svc.validateAutomationShape({ trigger: 'MESSAGE_SENT', conditions: [], actions: stored, cooldownSeconds: 0 }), []);
});

// ── Store against Postgres (opt-in) ──────────────────────────────────────────

function fakeDiscord({ members = { [USER]: false, [OTHER]: false, [BOT_USER]: true } } = {}) {
  const calls = [];
  const resources = {
    guild: { id: GUILD, name: 'Test', icon: null, memberCount: 3, ownerId: USER },
    channels: [
      { id: CH, name: 'general', type: 0, kind: 'text', parentId: null, position: 0, nsfw: false },
      { id: CH2, name: 'news', type: 5, kind: 'announcement', parentId: null, position: 1, nsfw: false },
      { id: CAT, name: 'Cat', type: 4, kind: 'category', parentId: null, position: 2, nsfw: false },
      { id: FORUM, name: 'forum', type: 15, kind: 'forum', parentId: null, position: 3, nsfw: false },
    ],
    roles: [
      { id: ROLE, name: 'Member', position: 2, managed: false, everyone: false, assignable: true, dangerous: false, mentionable: false },
      { id: ROLE2, name: 'Mod', position: 3, managed: false, everyone: false, assignable: true, dangerous: true, mentionable: false },
    ],
    emojis: [],
    bot: { userId: '955030000000000099', topRolePosition: 10, manageRoles: true },
  };
  return {
    calls,
    resources,
    async getGuildResources(guildId, opts) {
      calls.push(['resources', guildId, opts ?? null]);
      return resources;
    },
    async getUsers(ids) {
      calls.push(['users', ids]);
      return new Map(ids.map((id) => [id, id === USER ? { id, username: 'alice', globalName: 'Alice', avatarUrl: '' } : null]));
    },
    async searchMembers(guildId, q) {
      calls.push(['member', q]);
      return Object.hasOwn(members, q) ? [{ id: q, username: `u${q.slice(-2)}`, globalName: null, nick: null, avatarUrl: '', bot: members[q] }] : [];
    },
  };
}

const actor = (discord = fakeDiscord()) => ({ guildId: GUILD, userId: USER, user: null, member: { owner: true, permissions: '8' }, discord, logger: null });

async function storeCreate(store, data, ctx = actor()) {
  const { data: valid } = validateRecordInput('automations', data, { mode: 'create' });
  return store.create(GUILD, valid, ctx);
}
async function storeUpdate(store, id, data, ctx = actor()) {
  const current = await store.get(GUILD, id);
  const { data: valid } = validateRecordInput('automations', data, { mode: 'update', current });
  return store.update(GUILD, id, valid, ctx);
}
const expectRecordError = async (promise, code, path) => {
  await assert.rejects(promise, (err) => {
    assert.equal(err.name, 'RecordError', err.stack);
    assert.equal(err.code, code, err.message);
    if (path !== undefined) assert.equal(err.path, path, err.message);
    return true;
  });
};

test('db: create, list, edit, button numbers, self references, delete', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/automations.store.js');
  const { ensureGuildSettings, updateSettings } = await import('../src/services/settingsService.js');
  await prisma.guildSettings.deleteMany({ where: { guildId: { in: [GUILD, GUILD2] } } });
  await ensureGuildSettings(GUILD);
  await updateSettings(GUILD, { xpEnabled: false, economyEnabled: true });
  await prisma.savedEmbed.create({ data: { guildId: GUILD, name: 'rules', template: { content: 'Rules' }, createdBy: USER } });

  // A button automation that clicks itself: the self reference becomes the new row's id.
  const b = (label, index = null) => ({ kind: 'automation', label, url: null, style: 'PRIMARY', index });
  const first = await storeCreate(store, {
    name: 'verify', trigger: 'BUTTON_CLICKED',
    conditions: [cond('BUTTON_IS', { buttons: [{ automation: 'verify', index: 2 }] })],
    actions: [action('SEND_MESSAGE', { content: 'Click', channelId: CH, button: b('One') }), action('REPLY', { content: 'Thanks', button: b('Two') }),
      action('GIVE_ROLES', { roleIds: [ROLE] }), action('SEND_EMBED', { embedName: 'rules' })],
  });
  assert.equal(first.name, 'verify');
  assert.deepEqual(first.actions.map((a) => a.button.index), [1, 2, null, null]);
  assert.deepEqual(first.conditions[0].buttons, [{ automation: 'verify', index: 2 }]);
  const row = await prisma.automation.findUnique({ where: { id: first.id } });
  assert.equal(row.conditions[0].buttons[0].automationId, first.id);
  assert.equal(row.buttonSeq, 2);
  assert.equal(row.createdBy, USER);
  // The creator shows by name when Discord knows them, else by id.
  assert.equal(first.createdBy, 'Alice (@alice)');
  assert.equal((await store.get(GUILD, first.id)).createdBy, USER);
  assert.equal((await store.get(GUILD, first.id, { guildId: GUILD, discord: fakeDiscord(), logger: null })).createdBy, 'Alice (@alice)');
  assert.deepEqual(first.problems, []);
  assert.equal(first.summary, 'Button clicked · 1 condition · 4 actions');

  // What the bot loads sees it at once (same process: the memo was dropped by invalidateAutomations).
  const byTrigger = await svc.getAutomations(GUILD);
  assert.equal(byTrigger.BUTTON_CLICKED[0].id, first.id);

  // Another automation listens to verify's button 1, by name.
  const second = await storeCreate(store, {
    name: 'listener', trigger: 'BUTTON_CLICKED', conditions: [cond('BUTTON_IS', { buttons: [{ automation: 'verify', index: 1 }] })],
    actions: [action('REPLY', { content: 'ok' })],
  });
  assert.deepEqual(second.conditions[0].buttons, [{ automation: 'verify', index: 1 }]);
  await expectRecordError(storeCreate(store, {
    name: 'bad-ref', trigger: 'BUTTON_CLICKED', conditions: [cond('BUTTON_IS', { buttons: [{ automation: 'verify', index: 9 }] })], actions: [action('REPLY', { content: 'x' })],
  }), 'invalid', 'conditions.0.buttons.0.index');
  await expectRecordError(storeCreate(store, {
    name: 'bad-ref', trigger: 'BUTTON_CLICKED', conditions: [cond('BUTTON_IS', { buttons: [{ automation: 'nobody', index: 1 }] })], actions: [action('REPLY', { content: 'x' })],
  }), 'invalid', 'conditions.0.buttons.0.automation');
  await expectRecordError(storeCreate(store, {
    name: 'bad-self', trigger: 'BUTTON_CLICKED', conditions: [cond('BUTTON_IS', { buttons: [{ automation: 'bad-self', index: 1 }] })], actions: [action('REPLY', { content: 'x' })],
  }), 'invalid', 'conditions.0.buttons.0.index');

  // Server checks: missing saved embed, a reward module that is off, members.
  await expectRecordError(storeCreate(store, { name: 'e', actions: [action('SEND_EMBED', { embedName: 'nope' })] }), 'invalid', 'actions.0.embedName');
  // A saved embed whose text pings a role the user could not ping is refused like /automation's embed action (the owner may).
  await prisma.savedEmbed.create({ data: { guildId: GUILD, name: 'pinger', template: { content: `<@&${ROLE}>` }, createdBy: USER } });
  await expectRecordError(storeCreate(store, { name: 'e', actions: [action('SEND_EMBED', { embedName: 'pinger' })] },
    { ...actor(), userId: OTHER, member: { owner: false, permissions: '0' } }), 'invalid', 'actions.0.embedName');
  await expectRecordError(storeCreate(store, { name: 'e', actions: [action('GIVE_XP', { amount: 5 })] }), 'invalid', 'actions.0.type');
  assert.equal((await storeCreate(store, { name: 'coins', actions: [action('GIVE_COINS', { amount: 5 })] })).actions[0].amount, 5);
  await expectRecordError(storeCreate(store, {
    name: 'e', trigger: 'THREAD_CREATED', actions: [action('ADD_TO_THREAD', { users: [{ userId: BOT_USER }] })],
  }), 'invalid', 'actions.0.users.0.userId');
  await expectRecordError(storeCreate(store, {
    name: 'e', conditions: [cond('USER_IS', { users: [{ userId: '955030000000000077' }] })], actions: [action('PIN')],
  }), 'invalid', 'conditions.0.users.0.userId');
  await expectRecordError(storeCreate(store, { name: 'verify', actions: [action('PIN')] }), 'conflict', 'name');

  // Edit: rename, drop button 1, add a new one (gets 3, never 1 again), keep 2; the self reference follows.
  const edited = await storeUpdate(store, first.id, {
    name: 'verify-2',
    actions: [action('REPLY', { content: 'Thanks', button: b('Two', 2) }), action('SEND_MESSAGE', { content: 'New', button: b('Three') })],
  });
  assert.equal(edited.name, 'verify-2');
  assert.deepEqual(edited.actions.map((a) => a.button.index), [2, 3]);
  assert.deepEqual(edited.conditions[0].buttons, [{ automation: 'verify-2', index: 2 }]);
  assert.equal((await prisma.automation.findUnique({ where: { id: first.id } })).buttonSeq, 3);
  // The listener now points at a removed button: shown as a problem, kept on unrelated edits.
  let listener = await store.get(GUILD, second.id);
  assert.deepEqual(listener.problems, ['Condition 1: button 1 no longer exists on "verify-2".']);
  listener = await storeUpdate(store, second.id, { cooldownSeconds: 30, conditions: listener.conditions });
  assert.equal(listener.cooldownSeconds, 30);
  assert.deepEqual(listener.conditions[0].buttons, [{ automation: 'verify-2', index: 1 }]);
  // A removed number cannot come back: refused by the form rules, and again by the store under the row lock.
  await assert.rejects(storeUpdate(store, first.id, { actions: [action('REPLY', { content: 'x', button: b('X', 1) })] }),
    (err) => err instanceof FieldError && err.path === 'actions.0.button.index');
  await expectRecordError(store.update(GUILD, first.id, { actions: [action('REPLY', { content: 'x', button: b('X', 1) })] }, actor()),
    'invalid', 'actions.0.button.index');

  // Removing every action of an automation that is on turns it off (/automation action remove).
  const emptied = await storeUpdate(store, first.id, { actions: [] });
  assert.equal(emptied.enabled, false);
  await expectRecordError(store.update(GUILD, first.id, { enabled: true }, actor()), 'invalid', 'enabled');

  // A deleted saved embed shows up as a problem.
  const embedUser = await storeCreate(store, { name: 'embed-user', actions: [action('SEND_EMBED', { embedName: 'rules', channelId: CH })] });
  await prisma.savedEmbed.deleteMany({ where: { guildId: GUILD, name: 'rules' } });
  assert.deepEqual((await store.get(GUILD, embedUser.id)).problems, ['Action 1: the saved embed "rules" was not found.']);
  // ... and keeping it on an unrelated edit is fine.
  assert.equal((await storeUpdate(store, embedUser.id, { cooldownSeconds: 5, actions: (await store.get(GUILD, embedUser.id)).actions })).cooldownSeconds, 5);

  // Check for problems: channels and roles against Discord.
  await prisma.automation.update({ where: { id: embedUser.id }, data: { actions: [{ type: 'GIVE_ROLES', roleIds: [ROLE2, '955030000000000039'] }, { type: 'SEND_MESSAGE', content: 'x', channelId: '955030000000000029', button: null }] } });
  const checked = await store.actions.review(GUILD, embedUser.id, {}, actor());
  assert.match(checked.message, /^3 problems: /);
  assert.ok(checked.item.problems.some((p) => p.includes('moderator or admin permissions')));
  assert.ok(checked.item.problems.some((p) => p.includes('the role 955030000000000039 was deleted')));
  assert.ok(checked.item.problems.some((p) => p.includes('the channel 955030000000000029 was not found')));
  assert.equal((await store.actions.review(GUILD, second.id, {}, actor())).message, '1 problem: Condition 1: button 1 no longer exists on "verify-2".');

  // Stored elements the service cannot read are listed, and an unrelated edit leaves the stored JSON alone.
  const raw = [{ type: 'REPLY', content: 'ok', button: null }, { type: 'NOPE' }];
  await prisma.automation.update({ where: { id: second.id }, data: { actions: raw } });
  let withBad = await store.get(GUILD, second.id);
  assert.deepEqual(withBad.problems.slice(0, 1), ['Action 2 is invalid and ignored: Unknown action type.']);
  assert.deepEqual(withBad.actions.map((a) => a.type), ['REPLY']);
  withBad = await storeUpdate(store, second.id, { cooldownSeconds: 60 });
  assert.equal(withBad.cooldownSeconds, 60);
  assert.deepEqual((await prisma.automation.findUnique({ where: { id: second.id } })).actions, raw);

  // Every write drops the bot's automation cache in Redis (when a Redis is running for the test).
  const { cache } = await import('../src/services/redis.js');
  for (let i = 0; process.env.TEST_REDIS_URL && !cache.available() && i < 40; i++) await new Promise((r) => { setTimeout(r, 50); });
  const cached = await cache.set(svc.automationsCacheKey(GUILD), { v: 1, byTrigger: {} }, 60);
  if (process.env.TEST_REDIS_URL) assert.equal(cached, true, 'TEST_REDIS_URL is set but Redis is not reachable');
  if (cached) {
    await storeUpdate(store, second.id, { cooldownSeconds: 61 });
    assert.equal(await cache.get(svc.automationsCacheKey(GUILD)), null);
  }

  // Lists are per server; another server's id is not found.
  const list = await store.list(GUILD, {}, { guildId: GUILD, discord: fakeDiscord(), logger: null });
  assert.ok(list.every((i) => i.createdBy === 'Alice (@alice)'));
  assert.deepEqual(list.map((i) => i.name), ['verify-2', 'listener', 'coins', 'embed-user']);
  assert.equal(await store.get(GUILD2, first.id), null);
  await expectRecordError(store.update(GUILD2, first.id, { cooldownSeconds: 1 }, actor()), 'not_found');
  await expectRecordError(store.remove(GUILD2, first.id), 'not_found');
  await store.remove(GUILD, first.id);
  assert.equal(await store.get(GUILD, first.id), null);
  await expectRecordError(store.remove(GUILD, first.id), 'not_found');
});

test('db: presets, and the per-server cap', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/automations.store.js');
  const { ensureGuildSettings } = await import('../src/services/settingsService.js');
  await prisma.automation.deleteMany({ where: { guildId: GUILD } });
  await ensureGuildSettings(GUILD);

  const welcome = await storeCreate(store, { name: 'welcome', preset: 'welcomer', presetChannelId: CH, presetRoleId: ROLE });
  assert.equal(welcome.trigger, 'MEMBER_JOINED');
  assert.deepEqual(welcome.actions.map((a) => a.type), ['SEND_MESSAGE', 'GIVE_ROLES']);
  assert.equal(welcome.actions[0].content, svc.WELCOMER_DEFAULT_TEXT);
  assert.equal(welcome.enabled, true);
  assert.equal(welcome.preset, 'none');
  await expectRecordError(storeCreate(store, { name: 'w2', preset: 'welcomer', presetChannelId: FORUM }), 'invalid', 'presetChannelId');

  const stars = await storeCreate(store, { name: 'stars', preset: 'starboard', presetChannelId: CH2, presetThreshold: 4 });
  assert.equal(stars.trigger, 'REACTION_ADDED');
  assert.deepEqual(stars.conditions.map((c) => c.type), ['REACTED_WITH', 'HAS_REACTIONS', 'CHANNEL_NOT_IN']);
  assert.equal(stars.conditions[1].min, 4);
  assert.deepEqual(stars.conditions[0].emojis, [{ emoji: '⭐' }]);

  const joiner = await storeCreate(store, { name: 'joiner', preset: 'thread-joiner', presetUserId: OTHER, presetChannelId: FORUM });
  assert.equal(joiner.trigger, 'THREAD_CREATED');
  assert.deepEqual(joiner.actions[0].users, [{ userId: OTHER }]);
  await expectRecordError(storeCreate(store, { name: 'j2', preset: 'thread-joiner', presetUserId: BOT_USER }), 'invalid', 'presetUserId');
  await expectRecordError(storeCreate(store, { name: 'j2', preset: 'thread-joiner', presetUserId: '955030000000000088' }), 'invalid', 'presetUserId');

  // Commands Only reads the prefix and the moderator roles from the server's settings.
  const { updateSettings, updateSettingsWith } = await import('../src/services/settingsService.js');
  await updateSettings(GUILD, { customPrefix: '?' });
  await updateSettingsWith(GUILD, () => ({ modRoleIds: [ROLE2] }));
  const clean = await storeCreate(store, { name: 'clean', preset: 'commands-only', presetChannelId: CH, presetStaffRoleId: ROLE });
  assert.equal(clean.trigger, 'MESSAGE_SENT');
  assert.deepEqual(clean.actions.map((a) => a.type), ['DELETE']);
  const stored = await prisma.automation.findFirst({ where: { guildId: GUILD, name: 'clean' } });
  assert.deepEqual(stored.conditions.map((c) => c.type), ['CHANNEL_IN', 'MATCHES_REGEX', 'HAS_NOT_ANY_ROLE']);
  assert.deepEqual(stored.conditions[0].channelIds, [CH]);
  assert.equal(stored.conditions[1].pattern, '^(?!\\?)');
  assert.deepEqual(stored.conditions[2].roleIds, [ROLE, ROLE2]);
  await expectRecordError(storeCreate(store, { name: 'c2', preset: 'commands-only', presetChannelId: FORUM }), 'invalid', 'presetChannelId');
  await updateSettings(GUILD, { customPrefix: '!' });
  await updateSettingsWith(GUILD, () => ({ modRoleIds: [] }));

  // The cap, under the guild lock.
  const room = LIMITS.AUTOMATIONS - (await prisma.automation.count({ where: { guildId: GUILD } }));
  await prisma.automation.createMany({
    data: Array.from({ length: room }, (_, i) => ({ guildId: GUILD, name: `filler-${i}`, trigger: 'MESSAGE_SENT', createdBy: USER })),
  });
  await expectRecordError(storeCreate(store, { name: 'one-too-many', actions: [action('PIN')] }), 'limit');
  assert.equal((await store.list(GUILD)).length, LIMITS.AUTOMATIONS);
  await prisma.automation.deleteMany({ where: { guildId: GUILD } });
});

test('a role kept in a Take action is checked again once it becomes a Give action (server role checks key by type)', () => {
  const take = '400000000000000002';
  const give = '400000000000000005';
  const item = { actions: [{ type: 'TAKE_ROLES', roleIds: [take] }, { type: 'GIVE_ROLES', roleIds: [give] }] };
  const collection = A.collection;
  const keys = collection.refKeys(item, new Set([`actions.roleIds|${take}`, `actions.roleIds|${give}`, 'conditions.roleIds|1']));
  assert.ok(keys.has('conditions.roleIds|1'), 'other fields keep their keys');
  assert.equal([...keys].some((k) => k.startsWith('actions.roleIds|')), false, 'no untyped action keys remain');
  const data = { actions: [{ type: 'GIVE_ROLES', roleIds: [take] }, { type: 'GIVE_ROLES', roleIds: [give] }] };
  assert.equal(keys.has(collection.refKeyOf({ path: 'actions.0.roleIds.0', id: take }, data)), false, 'Take turned Give: checked');
  assert.equal(keys.has(collection.refKeyOf({ path: 'actions.1.roleIds.0', id: give }, data)), true, 'still a Give: kept');
  const unchanged = { actions: item.actions };
  assert.equal(keys.has(collection.refKeyOf({ path: 'actions.0.roleIds.0', id: take }, unchanged)), true, 'still a Take: kept');
  assert.equal(collection.refKeyOf({ path: 'conditions.0.roleIds.0', id: '1' }, data), 'conditions.roleIds|1');
});
