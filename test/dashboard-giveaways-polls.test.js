// The dashboard's Giveaways and Polls collections: descriptors (lint, request validation, parity with /giveaway and /poll),
// the item builders, and the dashboard tasks the bot runs for them (src/services/dashboardTasks/{giveaways,polls}.js) against
// fake discord.js objects and stubbed services. With TEST_DATABASE_URL (a migrated database) the stores also run end to end:
// the store asks for a task, the real task runs the real giveawayService / pollService against Postgres and a fake channel:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/dashboard-giveaways-polls.test.js
// The stores are imported only inside the database cases (they are never loaded offline).
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';
const OWNER_ID = '955231000000000999';
process.env.BOT_OWNER_ID = OWNER_ID;

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ChannelType, PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const { LIMITS } = await import('../src/config/constants.js');
const { MODULES } = await import('../src/config/modules.js');
const { FieldError, readValue } = await import('../src/dashboard/schema.js');
const { RecordError } = await import('../src/dashboard/collections/errors.js');
const { COLLECTIONS, lintCollections, publicCollection, validateRecordInput } = await import('../src/dashboard/collections/index.js');
const G = await import('../src/dashboard/collections/giveaways.js');
const P = await import('../src/dashboard/collections/polls.js');
// Services and commands, for parity checks and the stubs (they open nothing until used, except the Redis client below).
const giveawayService = await import('../src/services/giveawayService.js');
const pollService = await import('../src/services/pollService.js');
const giveawayCommand = await import('../src/commands/community/giveaway.js');
const pollCommand = await import('../src/commands/community/poll.js');
const { createGiveawayTasks } = await import('../src/services/dashboardTasks/giveaways.js');
const { createPollTasks } = await import('../src/services/dashboardTasks/polls.js');
const { TASKS, executeTask } = await import('../src/services/dashboardTasks/index.js');
const { TaskError } = await import('../src/services/dashboardTasks/shared.js');
const { PermanentJobError, getJob, scheduleJob } = await import('../src/services/scheduler.js');
const { registerProvider } = await import('../src/services/rewards.js');
const { ensureGuildSettings, updateSettings } = await import('../src/services/settingsService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const MINE = ['giveaways', 'polls'];
const ALL_GUILDS = Array.from({ length: 20 }, (_, i) => `955231${String(i + 1).padStart(12, '0')}`);
const guildNo = (n) => ALL_GUILDS[n - 1];

after(async () => {
  Object.assign(console, quiet);
  if (DB) await prisma.guildSettings.deleteMany({ where: { guildId: { in: ALL_GUILDS } } }).catch(() => {});
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Helpers ───────────────────────────────────────────────────────────────────

const sid = (prefix, n) => `${prefix}${String(n).padStart(18 - prefix.length, '0')}`; // 18-digit snowflake-like id
const CH = sid('4', 1);
const CH2 = sid('4', 2);
const BOT = sid('6', 1);
const ACTOR = sid('7', 1);
const GUILD = ALL_GUILDS[0];
const R_REQ = sid('8', 1);
const R_DENY = sid('8', 2);
const R_BONUS = sid('8', 3);
const R_BONUS2 = sid('8', 4);
const R_PING = sid('8', 5);
const R_QUIET = sid('8', 6); // not mentionable
const R_GONE = sid('8', 99); // never in the server
const GW_ID = `c${'a'.repeat(24)}`;
const POLL_ID = `c${'b'.repeat(24)}`;
const NOW = new Date('2026-10-11T12:00:00.000Z');
const DAY = 86_400_000;
const discordErr = (code) => Object.assign(new Error(`Discord ${code}`), { code });
let seq = 0;
const nextId = () => sid('5', ++seq);

const refuses = (fn, { path, message } = {}) => assert.throws(fn, (err) => {
  assert.ok(err instanceof FieldError || err instanceof RecordError, `a validation error, got ${err}`);
  if (path !== undefined) assert.equal(err.path, path, `path (${err.message})`);
  if (message) assert.match(err.message, message);
  return true;
});
const create = (key, data) => validateRecordInput(key, data, { mode: 'create' }).data;
const update = (key, data, current = null) => validateRecordInput(key, data, { mode: 'update', current }).data;
const action = (key, actionId, data) => validateRecordInput(key, data, { mode: 'action', actionId }).data;
const rejectsTask = (promise, re) => assert.rejects(promise, (err) => {
  assert.ok(err instanceof TaskError, `a TaskError, got ${err?.name}: ${err?.message}`);
  if (re) assert.match(err.message, re);
  return true;
});

// ── Descriptors ───────────────────────────────────────────────────────────────

test('descriptors: lint clean, real (not placeholders), JSON-safe public copies, the module of the command', () => {
  assert.deepEqual(lintCollections().filter((p) => MINE.some((k) => p.startsWith(k))), []);
  for (const key of MINE) {
    const c = COLLECTIONS[key];
    assert.equal(c.placeholder, undefined, `${key} is still a placeholder`);
    assert.ok(Object.hasOwn(MODULES, c.module), `${key}: module ${c.module}`);
    assert.equal(c.module, key);
    assert.ok(c.label && c.singular && c.description, `${key} has a label, a singular and a description`);
    assert.equal(c.limit, null, `${key}: the list holds finished items too, so there is no "n / limit" count`);
    assert.equal(c.searchable, true);
    for (const f of [...c.fields, ...c.readOnly]) assert.ok(f.label, `${key}.${f.field} has a label`);
    for (const f of c.fields) assert.ok(f.description || f.type === 'boolean', `${key}.${f.field} has a description`);
    const pub = publicCollection(c);
    assert.deepEqual(JSON.parse(JSON.stringify(pub)), pub, `${key} public copy is plain JSON`);
    assert.equal(pub.check, undefined);
    for (const a of pub.actions) assert.equal(a.check, undefined);
  }
  assert.deepEqual(COLLECTIONS.giveaways.actions.map((a) => a.id), ['end', 'reroll', 'restart', 'cancel']);
  assert.deepEqual(COLLECTIONS.polls.actions.map((a) => a.id), ['end']);
  assert.equal(COLLECTIONS.giveaways.canDelete, false);
  assert.equal(COLLECTIONS.polls.canEdit, false);
  assert.equal(COLLECTIONS.giveaways.actions.find((a) => a.id === 'cancel').danger, true);
  // Giveaways: only the bonus odds are editable once posted. Polls: nothing is.
  assert.deepEqual(COLLECTIONS.giveaways.fields.map((f) => f.field).filter((f) => !COLLECTIONS.giveaways.createOnly.includes(f)), ['roleWeights']);
  assert.deepEqual(COLLECTIONS.polls.createOnly, COLLECTIONS.polls.fields.map((f) => f.field));
});

test('descriptors: the limits are the services\' (a drift fails here)', () => {
  assert.equal(G.MIN_DURATION_MS, giveawayService.MIN_DURATION_MS);
  assert.equal(G.MAX_DURATION_MS, giveawayService.MAX_DURATION_MS);
  assert.equal(G.MAX_ROLE_WEIGHTS, giveawayService.MAX_ROLE_WEIGHTS);
  assert.equal(G.MIN_BONUS_WEIGHT, giveawayService.MIN_BONUS_WEIGHT);
  assert.equal(G.MAX_WEIGHT, giveawayService.MAX_WEIGHT);
  assert.equal(G.DEFAULT_BONUS_WEIGHT, giveawayService.DEFAULT_BONUS_WEIGHT);
  assert.equal(G.MAX_PRIZE_AMOUNT, giveawayService.MAX_PRIZE_AMOUNT);
  assert.deepEqual(G.STATUS_LABELS && Object.keys(G.STATUS_LABELS), Object.keys(giveawayService.STATUS_EMOJI));
  assert.equal(P.QUESTION_MAX, pollService.POLL.QUESTION_MAX);
  assert.equal(P.OPTION_MAX, pollService.POLL.OPTION_MAX);
  assert.equal(P.OPTIONS_MIN, pollService.POLL.OPTIONS_MIN);
  assert.equal(P.DURATION_MIN_MS, pollService.POLL.DURATION_MIN_MS);
  assert.equal(P.DURATION_MAX_MS, pollService.POLL.DURATION_MAX_MS);
  const f = (c, name) => COLLECTIONS[c].fields.find((x) => x.field === name);
  assert.equal(f('giveaways', 'winnerCount').max, LIMITS.GIVEAWAY_WINNERS);
  assert.equal(f('polls', 'options').maxItems, LIMITS.POLL_OPTIONS);
  assert.equal(f('polls', 'maxChoices').max, LIMITS.POLL_OPTIONS);
  assert.ok(COLLECTIONS.giveaways.description.includes(String(LIMITS.ACTIVE_GIVEAWAYS)));
  assert.ok(COLLECTIONS.polls.description.includes(String(LIMITS.ACTIVE_POLLS)));
  // The duration parser the checks use is the command's.
  for (const text of ['30m', '2h', '3d', '1w', 'soon', '30s', '31d', '']) {
    const own = P.durationProblem(text);
    const theirs = pollService.parseDurationOption(text);
    assert.equal(own === null, !theirs.error, `poll duration "${text}"`);
  }
});

test('parity: every option of /giveaway start and /poll create has a field, with the command\'s bounds', () => {
  const json = giveawayCommand.data.toJSON();
  const sub = (name, list = json.options) => list.find((o) => o.name === name);
  const start = Object.fromEntries(sub('start').options.map((o) => [o.name, o]));
  const MAP = {
    prize: 'prize', duration: 'duration', winners: 'winnerCount', channel: 'channelId', required_role: 'requiredRoleIds',
    denied_role: 'deniedRoleIds', bonus_role: 'roleWeights', bonus_weight: 'roleWeights', xp: 'xpPrize', coins: 'coinPrize',
    ping_role: 'pingRoleId', description: 'description', color: 'embedColor',
  };
  assert.deepEqual(Object.keys(start).sort(), Object.keys(MAP).sort(), 'a new /giveaway start option needs a dashboard field');
  const field = (name) => COLLECTIONS.giveaways.fields.find((f) => f.field === name);
  for (const name of new Set(Object.values(MAP))) assert.ok(field(name), name);
  assert.equal(field('winnerCount').min, start.winners.min_value);
  assert.equal(field('winnerCount').max, start.winners.max_value);
  const weight = field('roleWeights').itemFields.find((f) => f.field === 'weight');
  assert.equal(weight.min, start.bonus_weight.min_value);
  assert.equal(weight.max, start.bonus_weight.max_value);
  assert.equal(field('xpPrize').min, start.xp.min_value);
  assert.equal(field('xpPrize').max, start.xp.max_value);
  assert.equal(field('coinPrize').max, start.coins.max_value);
  assert.equal(field('prize').maxLength, start.prize.max_length);
  assert.equal(field('description').maxLength, start.description.max_length);
  assert.deepEqual(start.channel.channel_types.sort(), [ChannelType.GuildText, ChannelType.GuildAnnouncement].sort());
  assert.deepEqual(field('channelId').channelTypes, ['text', 'announcement']);

  // Subcommands: every one is an action, the create, the odds edit, the list, or named here as not mirrored.
  const subs = json.options.map((o) => o.name).sort();
  assert.deepEqual(subs, ['cancel', 'end', 'entrants', 'list', 'odds', 'reroll', 'restart', 'start'], 'a new /giveaway subcommand needs a decision');
  assert.deepEqual(sub('odds').options.map((o) => o.name).sort(), ['add', 'remove']);
  const restart = Object.fromEntries(sub('restart').options.map((o) => [o.name, o]));
  assert.deepEqual(Object.keys(restart).sort(), ['channel', 'duration', 'giveaway']);
  assert.deepEqual(COLLECTIONS.giveaways.actions.find((a) => a.id === 'restart').fields.map((f) => f.field), ['channelId', 'duration', 'endsAt']);
  const reroll = Object.fromEntries(sub('reroll').options.map((o) => [o.name, o]));
  assert.equal(reroll.winners.max_value, COLLECTIONS.giveaways.actions.find((a) => a.id === 'reroll').fields[0].max);

  const pj = pollCommand.data.toJSON();
  const create = Object.fromEntries(pj.options.find((o) => o.name === 'create').options.map((o) => [o.name, o]));
  const PMAP = {
    duration: 'duration', max_choices: 'maxChoices', allowed_role: 'allowedRoleIds', denied_role: 'deniedRoleIds',
    hide_results: 'hideResults', channel: 'channelId',
  };
  assert.deepEqual(Object.keys(create).sort(), Object.keys(PMAP).sort(), 'a new /poll create option needs a dashboard field');
  const pfield = (name) => COLLECTIONS.polls.fields.find((f) => f.field === name);
  for (const name of Object.values(PMAP)) assert.ok(pfield(name), name);
  assert.equal(pfield('maxChoices').max, create.max_choices.max_value);
  assert.equal(pfield('duration').maxLength, create.duration.max_length);
  const kinds = { 0: 'text', 5: 'announcement', 2: 'voice' };
  assert.deepEqual(create.channel.channel_types.filter((t) => kinds[t]).map((t) => kinds[t]).sort(), [...pfield('channelId').channelTypes].sort());
  assert.deepEqual(pj.options.map((o) => o.name).sort(), ['create', 'end', 'list', 'results']);
});

const GOOD = { prize: 'Nitro', channelId: CH, duration: '2d' };

test('giveaways: a valid create fills the defaults and trims; a full form passes', () => {
  assert.deepEqual(create('giveaways', GOOD), {
    prize: 'Nitro', description: null, channelId: CH, winnerCount: 1, duration: '2d', endsAt: null, requiredRoleIds: [],
    deniedRoleIds: [], roleWeights: [], xpPrize: null, coinPrize: null, pingRoleId: null, embedColor: null,
  });
  const when = new Date(Date.now() + 3 * DAY).toISOString();
  const full = create('giveaways', {
    prize: '  A very good prize  ', description: 'Line one\nLine two', channelId: CH, winnerCount: LIMITS.GIVEAWAY_WINNERS, duration: null,
    endsAt: when, requiredRoleIds: [R_REQ], deniedRoleIds: [R_DENY], roleWeights: [{ roleId: R_BONUS, weight: 10 }, { roleId: R_BONUS2 }],
    xpPrize: 500, coinPrize: 1_000_000, pingRoleId: R_PING, embedColor: '#5865F2',
  });
  assert.equal(full.prize, 'A very good prize');
  assert.equal(full.description, 'Line one\nLine two');
  assert.equal(full.endsAt, when);
  assert.deepEqual(full.roleWeights, [{ roleId: R_BONUS, weight: 10 }, { roleId: R_BONUS2, weight: 2 }]);
  assert.equal(full.embedColor, 0x5865f2);
});

test('giveaways: every rule /giveaway start has is refused with the field to highlight', () => {
  const bad = (over, expected) => refuses(() => create('giveaways', { ...GOOD, ...over }), expected);
  refuses(() => create('giveaways', { channelId: CH, duration: '2d' }), { path: 'prize' });
  bad({ prize: '   ' }, { path: 'prize' });
  bad({ prize: 'x'.repeat(257) }, { path: 'prize' });
  bad({ description: 'x'.repeat(1001) }, { path: 'description' });
  refuses(() => create('giveaways', { prize: 'x', duration: '2d' }), { path: 'channelId' });
  bad({ channelId: 'general' }, { path: 'channelId' });
  for (const winnerCount of [0, LIMITS.GIVEAWAY_WINNERS + 1, 1.5, '2']) bad({ winnerCount }, { path: 'winnerCount' });

  // Exactly one of the length and the end time, inside 1 minute .. 90 days.
  bad({ duration: null }, { path: 'duration', message: /how long/ });
  bad({ duration: '2d', endsAt: new Date(Date.now() + DAY).toISOString() }, { path: 'endsAt', message: /not both/ });
  bad({ duration: 'soon' }, { path: 'duration', message: /30m, 2h, 3d or 1w/ });
  bad({ duration: '30s' }, { path: 'duration', message: /at least 1 minute/ });
  bad({ duration: '91d' }, { path: 'duration', message: /at most 90 days/ });
  bad({ duration: null, endsAt: new Date(Date.now() - 3600_000).toISOString() }, { path: 'endsAt', message: /in the past/ });
  bad({ duration: null, endsAt: new Date(Date.now() + 20_000).toISOString() }, { path: 'endsAt', message: /at least 1 minute/ });
  bad({ duration: null, endsAt: new Date(Date.now() + 91 * DAY).toISOString() }, { path: 'endsAt', message: /at most 90 days/ });
  bad({ duration: null, endsAt: 'tomorrow' }, { path: 'endsAt' });
  assert.equal(create('giveaways', { ...GOOD, duration: '1m' }).duration, '1m');
  assert.equal(create('giveaways', { ...GOOD, duration: '90d' }).duration, '90d');

  bad({ requiredRoleIds: Array.from({ length: 11 }, (_, i) => sid('8', 100 + i)) }, { path: 'requiredRoleIds' });
  bad({ requiredRoleIds: ['everyone'] }, { path: 'requiredRoleIds.0' });
  bad({ requiredRoleIds: [R_REQ], deniedRoleIds: [R_REQ, R_DENY] }, { path: 'deniedRoleIds', message: /both required and blocked/ });
  bad({ roleWeights: [{ roleId: R_BONUS, weight: 1 }] }, { path: 'roleWeights.0.weight' });
  bad({ roleWeights: [{ roleId: R_BONUS, weight: 11 }] }, { path: 'roleWeights.0.weight' });
  bad({ roleWeights: [{ weight: 3 }] }, { path: 'roleWeights.0.roleId' });
  bad({ roleWeights: [{ roleId: R_BONUS, weight: 3, extra: 1 }] }, { path: 'roleWeights.0.extra' });
  bad({ roleWeights: Array.from({ length: 11 }, (_, i) => ({ roleId: sid('8', 100 + i), weight: 2 })) }, { path: 'roleWeights' });
  bad({ roleWeights: [{ roleId: R_BONUS, weight: 2 }, { roleId: R_BONUS2, weight: 3 }, { roleId: R_BONUS, weight: 4 }] },
    { path: 'roleWeights.2.roleId', message: /only once/ });
  for (const xpPrize of [0, -5, 1_000_001, 1.5]) bad({ xpPrize }, { path: 'xpPrize' });
  for (const coinPrize of [0, 1_000_001]) bad({ coinPrize }, { path: 'coinPrize' });
  bad({ pingRoleId: 'role' }, { path: 'pingRoleId' });
  bad({ embedColor: 0 }, { path: 'embedColor' });
  bad({ embedColor: 'blurple' }, { path: 'embedColor' });
  bad({ hostId: ACTOR }, { path: 'hostId', message: /not something you can set/ });
  bad({ status: 'ENDED' }, { path: 'status' });
});

test('giveaways: an update carries only the bonus odds; actions validate their own fields', () => {
  assert.deepEqual(update('giveaways', { roleWeights: [{ roleId: R_BONUS, weight: 4 }] }), { roleWeights: [{ roleId: R_BONUS, weight: 4 }] });
  assert.deepEqual(update('giveaways', { roleWeights: [] }), { roleWeights: [] });
  for (const field of COLLECTIONS.giveaways.createOnly) {
    refuses(() => update('giveaways', { [field]: null }), { path: field, message: /cannot be changed after creation/ });
  }
  refuses(() => update('giveaways', { roleWeights: [{ roleId: R_BONUS, weight: 2 }, { roleId: R_BONUS, weight: 3 }] }), { path: 'roleWeights.1.roleId' });

  assert.deepEqual(action('giveaways', 'end', {}), {});
  assert.deepEqual(action('giveaways', 'cancel', {}), {});
  assert.deepEqual(action('giveaways', 'reroll', {}), { winners: 1 });
  assert.deepEqual(action('giveaways', 'reroll', { winners: 3 }), { winners: 3 });
  for (const winners of [0, LIMITS.GIVEAWAY_WINNERS + 1]) refuses(() => action('giveaways', 'reroll', { winners }), { path: 'winners' });
  assert.deepEqual(action('giveaways', 'restart', {}), { channelId: null, duration: null, endsAt: null });
  assert.equal(action('giveaways', 'restart', { channelId: CH2 }).channelId, CH2);
  assert.equal(action('giveaways', 'restart', { duration: '3d' }).duration, '3d');
  refuses(() => action('giveaways', 'restart', { duration: '3d', endsAt: new Date(Date.now() + DAY).toISOString() }), { path: 'endsAt' });
  refuses(() => action('giveaways', 'restart', { duration: '45s' }), { path: 'duration', message: /at least 1 minute/ });
  refuses(() => action('giveaways', 'restart', { endsAt: new Date(Date.now() - 1000).toISOString() }), { path: 'endsAt' });
  refuses(() => action('giveaways', 'nope', {}), { message: /Unknown action/ });
  refuses(() => action('giveaways', 'end', { winners: 1 }), { path: 'winners' });
});

const GOOD_POLL = { question: 'Best pizza?', options: ['Margherita', 'Pepperoni'], channelId: CH };

test('polls: a valid create fills the defaults; a full form passes', () => {
  assert.deepEqual(create('polls', GOOD_POLL), {
    question: 'Best pizza?', options: ['Margherita', 'Pepperoni'], channelId: CH, maxChoices: 1, duration: null, allowedRoleIds: [],
    deniedRoleIds: [], hideResults: false,
  });
  const full = create('polls', {
    question: ' Q ', options: Array.from({ length: LIMITS.POLL_OPTIONS }, (_, i) => `Option ${i + 1}`), channelId: CH2, maxChoices: 5,
    duration: '30d', allowedRoleIds: [R_REQ], deniedRoleIds: [R_DENY], hideResults: true,
  });
  assert.equal(full.question, 'Q');
  assert.equal(full.options.length, LIMITS.POLL_OPTIONS);
  assert.equal(full.duration, '30d');
});

test('polls: every rule /poll create has is refused with the field to highlight', () => {
  const bad = (over, expected) => refuses(() => create('polls', { ...GOOD_POLL, ...over }), expected);
  refuses(() => create('polls', { options: ['a', 'b'], channelId: CH }), { path: 'question' });
  bad({ question: '  ' }, { path: 'question' });
  bad({ question: 'q'.repeat(257) }, { path: 'question' });
  refuses(() => create('polls', { question: 'q', channelId: CH }), { path: 'options' });
  bad({ options: [] }, { path: 'options', message: /at least 2/ });
  bad({ options: ['only one'] }, { path: 'options', message: /at least 2/ });
  bad({ options: ['Same', 'same'] }, { path: 'options', message: /at least 2 different/ });
  bad({ options: Array.from({ length: LIMITS.POLL_OPTIONS + 1 }, (_, i) => `o${i}`) }, { path: 'options' });
  bad({ options: ['fine', 'x'.repeat(81)] }, { path: 'options.1' });
  bad({ options: ['fine', '   '] }, { path: 'options.1' });
  bad({ options: ['fine', 'two\nlines'] }, { path: 'options.1' });
  refuses(() => create('polls', { question: 'q', options: ['a', 'b'] }), { path: 'channelId' });
  for (const maxChoices of [0, LIMITS.POLL_OPTIONS + 1, 2.5]) bad({ maxChoices }, { path: 'maxChoices' });
  bad({ duration: 'soon' }, { path: 'duration', message: /30m, 2h, 1d or 1w/ });
  bad({ duration: '30s' }, { path: 'duration', message: /at least 1 minute/ });
  bad({ duration: '31d' }, { path: 'duration', message: /at most 30 days/ });
  assert.equal(create('polls', { ...GOOD_POLL, duration: '1m' }).duration, '1m');
  assert.equal(create('polls', { ...GOOD_POLL, duration: '  ' }).duration, null);
  bad({ allowedRoleIds: [R_REQ], deniedRoleIds: [R_REQ] }, { path: 'deniedRoleIds', message: /must be different/ });
  bad({ allowedRoleIds: Array.from({ length: 11 }, (_, i) => sid('8', 100 + i)) }, { path: 'allowedRoleIds' });
  bad({ hideResults: 'yes' }, { path: 'hideResults' });
  bad({ authorId: ACTOR }, { path: 'authorId' });
});

test('polls: nothing can be edited after posting; end takes no input', () => {
  for (const field of COLLECTIONS.polls.fields.map((f) => f.field)) {
    refuses(() => update('polls', { [field]: null }), { path: field, message: /cannot be changed after creation/ });
  }
  assert.deepEqual(action('polls', 'end', {}), {});
});

// ── Items ─────────────────────────────────────────────────────────────────────

const giveawayRow = (over = {}) => ({
  id: GW_ID, guildId: GUILD, channelId: CH, messageId: sid('3', 1), hostId: ACTOR, prize: 'Nitro', description: null, winnerCount: 2,
  requiredRoleIds: [R_REQ], deniedRoleIds: [], roleWeights: [{ roleId: R_BONUS, weight: 3 }], xpPrize: 100, coinPrize: null, pingRoleId: null,
  embedColor: 0x5865f2, endsAt: new Date(NOW.getTime() + DAY), endedAt: null, status: 'ACTIVE', winnerIds: [], createdAt: NOW,
  _count: { entries: 7 }, ...over,
});

test('giveaway items: every value has the shape of its descriptor, summaries read well', () => {
  for (const row of [
    giveawayRow(),
    giveawayRow({ status: 'DRAFT', messageId: null }),
    giveawayRow({ status: 'ENDED', endedAt: NOW, winnerIds: [sid('7', 5), sid('7', 6)], description: 'Hello\nthere' }),
    giveawayRow({ status: 'CANCELLED', endedAt: NOW, xpPrize: null, embedColor: null, roleWeights: null, _count: { entries: 1 } }),
  ]) {
    const item = G.giveawayItem(row);
    for (const d of [...COLLECTIONS.giveaways.fields, ...COLLECTIONS.giveaways.readOnly]) {
      assert.ok(Object.hasOwn(item, d.field), `${d.field} is on the item`);
      assert.deepEqual(readValue(d, item[d.field]), item[d.field], `${d.field} reads back unchanged`);
    }
    assert.equal(item.id, GW_ID);
    assert.equal(item.duration, null);
  }
  const running = G.giveawayItem(giveawayRow());
  assert.equal(running.status, 'Running');
  assert.equal(running.summary, 'Running, ends 2026-10-12 12:00 UTC · 7 entries');
  assert.equal(running.messageUrl, `https://discord.com/channels/${GUILD}/${CH}/${sid('3', 1)}`);
  assert.equal(G.giveawayItem(giveawayRow({ status: 'DRAFT', messageId: null })).messageUrl, null);
  assert.equal(G.giveawayItem(giveawayRow({ status: 'DRAFT' })).summary, 'Draft (its message was deleted) · 7 entries');
  const ended = G.giveawayItem(giveawayRow({ status: 'ENDED', endedAt: NOW, winnerIds: [sid('7', 5)] }), { names: new Map([[ACTOR, 'Alice'], [sid('7', 5), 'Bob']]) });
  assert.equal(ended.summary, 'Ended 2026-10-11 12:00 UTC · 1 winner · 7 entries');
  assert.deepEqual([ended.host, ended.winners], ['Alice', ['Bob']]);
  assert.equal(G.giveawayItem(giveawayRow({ status: 'CANCELLED', _count: { entries: 1 } })).summary, 'Cancelled · 1 entry');
});

const pollRow = (over = {}) => ({
  id: POLL_ID, guildId: GUILD, channelId: CH, messageId: sid('3', 2), authorId: ACTOR, question: 'Best pizza?',
  options: ['Margherita', 'Pepperoni', 'Hawaiian'], maxChoices: 1, allowedRoleIds: [], deniedRoleIds: [R_DENY], hideResults: true,
  endsAt: null, status: 'ACTIVE', createdAt: NOW, ...over,
});

test('poll items: every value has the shape of its descriptor, the tally is spelled out', () => {
  const tally = { counts: [3, 1, 0], votes: 4, voters: 3 };
  for (const row of [pollRow(), pollRow({ endsAt: new Date(NOW.getTime() + DAY) }), pollRow({ status: 'ENDED', endsAt: NOW, messageId: null })]) {
    const item = P.pollItem(row, { tally });
    for (const d of [...COLLECTIONS.polls.fields, ...COLLECTIONS.polls.readOnly]) {
      assert.ok(Object.hasOwn(item, d.field), `${d.field} is on the item`);
      assert.deepEqual(readValue(d, item[d.field]), item[d.field], `${d.field} reads back unchanged`);
    }
  }
  const item = P.pollItem(pollRow(), { tally, names: new Map([[ACTOR, 'Alice']]) });
  assert.deepEqual(item.results, ['1. Margherita: 3 votes (75%)', '2. Pepperoni: 1 vote (25%)', '3. Hawaiian: 0 votes (0%)']);
  assert.deepEqual([item.votes, item.voters, item.author, item.status], [4, 3, 'Alice', 'Active']);
  assert.equal(item.summary, 'Active, open until ended · 4 votes · leading: Margherita (75%)');
  assert.equal(P.pollItem(pollRow({ endsAt: new Date(NOW.getTime() + DAY) }), { tally }).summary, 'Active, ends 2026-10-12 12:00 UTC · 4 votes · leading: Margherita (75%)');
  assert.equal(P.pollItem(pollRow({ status: 'ENDED', endsAt: NOW }), { tally: { counts: [1, 0, 0], votes: 1, voters: 1 } }).summary,
    'Ended 2026-10-11 12:00 UTC · 1 vote · won: Margherita (100%)');
  const tie = P.pollItem(pollRow({ status: 'ENDED', endsAt: NOW, options: ['A', 'B', 'C', 'D', 'E'] }), { tally: { counts: [2, 2, 2, 2, 0], votes: 8, voters: 8 } });
  assert.equal(tie.summary, 'Ended 2026-10-11 12:00 UTC · 8 votes · tie: A, B, C +1');
  const empty = P.pollItem(pollRow());
  assert.deepEqual([empty.votes, empty.voters, empty.results[0]], [0, 0, '1. Margherita: 0 votes (0%)']);
  assert.equal(empty.summary, 'Active, open until ended · 0 votes');
});

// ── Fakes (just enough of discord.js for the tasks) ───────────────────────────

const settingsRow = (over = {}) => ({
  guildId: GUILD, timezone: 'UTC', giveawaysEnabled: true, pollsEnabled: true, xpEnabled: true, economyEnabled: true, modRoleIds: [], ...over,
});

function makeRole(id, { mentionable = true, name = `role-${id.slice(-3)}` } = {}) {
  return { id, name, mentionable, position: 1, managed: false };
}

function makeGuild(id = GUILD, { roles = [], ownerId = null } = {}) {
  const guild = {
    id,
    ownerId,
    available: true,
    roles: {
      cache: new Map([[id, { id, name: '@everyone' }], ...roles.map((r) => [r.id, r])]),
      async fetch(rid) { return guild.roles.cache.get(rid) ?? null; },
    },
    channels: {
      cache: new Map(),
      async fetch(cid) {
        const c = guild.channels.cache.get(cid);
        if (!c) throw discordErr(10003);
        return c;
      },
    },
    members: {
      cache: new Map(),
      me: { id: BOT },
      async fetch(opts) {
        const out = new Map();
        for (const uid of opts?.user ?? []) if (guild.members.cache.has(uid)) out.set(uid, guild.members.cache.get(uid));
        return out;
      },
    },
  };
  return guild;
}

function makeChannel(guild, id, { type = ChannelType.GuildText, name = 'general' } = {}) {
  const denied = new Map(); // member id -> Set of denied permission bits
  const ch = {
    id, name, type, guild, sent: [], edits: [], deleted: [], store: new Map(), failSend: null, failDelete: null,
    isTextBased: () => true,
    isThread: () => false,
    /** Denies permission bits to a member (or to the bot with `{ id: BOT }`). */
    deny(who, ...bits) { denied.set(who.id, new Set([...(denied.get(who.id) ?? []), ...bits])); },
    permissionsFor(who) {
      const d = denied.get(who?.id);
      return { has: (flags) => (Array.isArray(flags) ? flags : [flags]).every((f) => !d?.has(f)) };
    },
    async send(payload) {
      if (ch.failSend) throw ch.failSend;
      const id2 = nextId();
      const message = { id: id2, channel: ch, url: `https://discord.com/channels/${guild.id}/${id}/${id2}`, async delete() { ch.store.delete(id2); } };
      ch.store.set(id2, message);
      ch.sent.push({ ...payload, id: id2 });
      return message;
    },
    messages: {
      async fetch(mid) {
        if (!ch.store.has(mid)) throw discordErr(10008);
        return ch.store.get(mid);
      },
      async edit(mid, payload) {
        if (!ch.store.has(mid)) throw discordErr(10008);
        ch.edits.push({ id: mid, ...payload });
        return ch.store.get(mid);
      },
      async delete(mid) {
        if (ch.failDelete) throw ch.failDelete;
        if (!ch.store.has(mid)) throw discordErr(10008);
        ch.store.delete(mid);
        ch.deleted.push(mid);
      },
    },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function makeMember(guild, id, { perms = 0n, roles = [], displayName = `user-${id.slice(-4)}` } = {}) {
  return {
    id, guild, displayName, user: { id, bot: false }, partial: false,
    permissions: new PermissionsBitField(perms), roles: { cache: new Set(roles) },
  };
}

function makeEnv(extra = {}) {
  const guild = makeGuild(GUILD, {
    roles: [R_REQ, R_DENY, R_BONUS, R_BONUS2, R_PING].map((id) => makeRole(id)).concat(makeRole(R_QUIET, { mentionable: false })),
    ownerId: sid('9', 1),
  });
  const channel = makeChannel(guild, CH, { name: 'giveaways' });
  const moderator = makeMember(guild, ACTOR, { perms: PermissionFlagsBits.ModerateMembers });
  guild.members.cache.set(ACTOR, moderator);
  return { guild, channel, moderator, actor: { id: ACTOR, member: moderator, isBotOwner: false }, client: { user: { id: BOT } }, ...extra };
}

// The xp provider exists (so XP prizes can be offered); the coins provider never does (the "not installed" refusal).
registerProvider('xp', { async grant() { return { ok: true }; }, async take() { return { ok: true }; } }, 'dashboard-giveaways-polls-test');

// ── Giveaway tasks ────────────────────────────────────────────────────────────

function giveawayHarness({ settings = settingsRow(), rows = [], service = {} } = {}) {
  const calls = [];
  const stub = (name, result) => async (...args) => {
    calls.push({ name, args: args[0] });
    return typeof result === 'function' ? result(...args) : result;
  };
  const svc = {
    startGiveaway: stub('startGiveaway', { ok: true, giveaway: { id: GW_ID }, message: {} }),
    endGiveaway: stub('endGiveaway', { ok: true, winnerIds: [], entryCount: 0, announced: true, payWarnings: [] }),
    rerollGiveaway: stub('rerollGiveaway', { ok: true, newWinnerIds: [sid('7', 5)], announced: true, payWarnings: [] }),
    cancelGiveaway: stub('cancelGiveaway', { ok: true }),
    restartGiveaway: stub('restartGiveaway', { ok: true, message: {} }),
    updateOdds: stub('updateOdds', { ok: true }),
    ...service,
  };
  const db = { giveaway: { findFirst: async ({ where }) => rows.find((r) => r.id === where.id && r.guildId === where.guildId) ?? null } };
  const tasks = createGiveawayTasks({ service: svc, getSettings: async () => settings, db, now: () => NOW });
  return { tasks, calls, db };
}

const gwStart = (over = {}) => ({
  prize: '  Nitro  ', description: ' Hi ', channelId: CH, winnerCount: 2, duration: '2d', endsAt: null, requiredRoleIds: [R_REQ],
  deniedRoleIds: [R_DENY], roleWeights: [{ roleId: R_BONUS, weight: 3 }], xpPrize: 100, coinPrize: null, pingRoleId: R_PING,
  embedColor: 0x5865f2, ...over,
});

const runTask = (h, env, kind, payload, over = {}) => h.tasks[kind].run({ guild: env.guild, client: env.client, actor: env.actor, payload, taskId: 't', ...over });

test('giveaway tasks: the kinds are registered with the bot, once each', () => {
  for (const kind of ['giveaway.start', 'giveaway.end', 'giveaway.reroll', 'giveaway.cancel', 'giveaway.restart', 'giveaway.odds',
    'poll.start', 'poll.end', 'poll.delete']) {
    assert.equal(typeof TASKS[kind]?.run, 'function', kind);
  }
});

test('giveaway.start: calls startGiveaway with exactly what /giveaway start passes, as the dashboard user', async () => {
  const env = makeEnv();
  const h = giveawayHarness();
  const out = await runTask(h, env, 'giveaway.start', gwStart());
  assert.deepEqual(out.data, { giveawayId: GW_ID });
  assert.match(out.message, /Started the giveaway for "Nitro" in #giveaways\. It ends 2026-10-13 12:00 UTC\./);
  const { name, args } = h.calls.at(-1);
  assert.equal(name, 'startGiveaway');
  assert.equal(args.guild, env.guild);
  assert.equal(args.client, env.client);
  assert.equal(args.channel, env.channel);
  assert.equal(args.hostId, ACTOR);
  assert.equal(args.prize, 'Nitro');
  assert.equal(args.description, 'Hi');
  assert.equal(args.winnerCount, 2);
  assert.equal(args.endsAt.getTime(), NOW.getTime() + 2 * DAY);
  assert.deepEqual(args.requiredRoleIds, [R_REQ]);
  assert.deepEqual(args.deniedRoleIds, [R_DENY]);
  assert.deepEqual(args.roleWeights, [{ roleId: R_BONUS, weight: 3 }]);
  assert.deepEqual([args.xpPrize, args.coinPrize, args.pingRoleId, args.embedColor], [100, null, R_PING, 0x5865f2]);
  assert.equal(args.settings.guildId, GUILD);
});

test('giveaway.start: the minimal payload and an exact end time', async () => {
  const env = makeEnv();
  const h = giveawayHarness();
  const endsAt = new Date(NOW.getTime() + 3600_000).toISOString();
  await runTask(h, env, 'giveaway.start', { prize: 'Cake', channelId: CH, duration: null, endsAt });
  const { args } = h.calls.at(-1);
  assert.deepEqual([args.winnerCount, args.requiredRoleIds, args.deniedRoleIds, args.roleWeights, args.xpPrize, args.pingRoleId, args.embedColor],
    [1, [], [], [], null, null, null]);
  assert.equal(args.endsAt.toISOString(), endsAt);
});

test('giveaway tasks: the router\'s gate and the command\'s level are checked first', async () => {
  const env = makeEnv();
  // The user left the server.
  await rejectsTask(runTask(giveawayHarness(), env, 'giveaway.end', { giveawayId: GW_ID }, { actor: { id: ACTOR, member: null, isBotOwner: false } }),
    /no longer a member/);
  // Settings unreadable.
  await rejectsTask(runTask(giveawayHarness({ settings: null }), env, 'giveaway.start', gwStart()), /settings could not be loaded/);
  // The module is off.
  const off = giveawayHarness({ settings: settingsRow({ giveawaysEnabled: false }) });
  for (const [kind, payload] of [['giveaway.start', gwStart()], ['giveaway.end', { giveawayId: GW_ID }], ['giveaway.odds', { giveawayId: GW_ID, roleWeights: [] }]]) {
    await rejectsTask(runTask(off, env, kind, payload), /Giveaways module is turned off/);
  }
  assert.equal(off.calls.length, 0);
  // A plain member, a kicker without the mod role.
  const plain = { id: sid('7', 2), member: makeMember(env.guild, sid('7', 2)), isBotOwner: false };
  const h = giveawayHarness();
  await rejectsTask(runTask(h, env, 'giveaway.start', gwStart(), { actor: plain }), /need to be a moderator/);
  assert.equal(h.calls.length, 0);
  // Moderator by role, by Kick Members, as the guild owner and as the bot owner (not in the server) all pass.
  const viaRole = { id: sid('7', 3), member: makeMember(env.guild, sid('7', 3), { roles: [R_REQ] }), isBotOwner: false };
  const hr = giveawayHarness({ settings: settingsRow({ modRoleIds: [R_REQ] }) });
  await runTask(hr, env, 'giveaway.start', gwStart(), { actor: viaRole });
  const kicker = { id: sid('7', 4), member: makeMember(env.guild, sid('7', 4), { perms: PermissionFlagsBits.KickMembers }), isBotOwner: false };
  await runTask(giveawayHarness(), env, 'giveaway.start', gwStart(), { actor: kicker });
  const owner = { id: sid('9', 1), member: makeMember(env.guild, sid('9', 1)), isBotOwner: false };
  await runTask(giveawayHarness(), env, 'giveaway.start', gwStart(), { actor: owner });
  const botOwner = { id: OWNER_ID, member: null, isBotOwner: true };
  const hb = giveawayHarness();
  await runTask(hb, env, 'giveaway.start', gwStart({ pingRoleId: null }), { actor: botOwner });
  assert.equal(hb.calls.at(-1).args.hostId, OWNER_ID);
});

test('giveaway.start: refuses what /giveaway start refuses, with a message for the admin', async () => {
  const env = makeEnv();
  const h = giveawayHarness();
  const bad = (over, re, harness = h) => rejectsTask(runTask(harness, env, 'giveaway.start', gwStart(over)), re);
  // Untrusted payload shapes.
  await rejectsTask(runTask(h, env, 'giveaway.start', null), /not valid/);
  await bad({ prize: 5 }, /not valid/);
  await bad({ channelId: 'general' }, /not valid/);
  await bad({ requiredRoleIds: 'x' }, /not valid/);
  await bad({ requiredRoleIds: ['nope'] }, /not valid/);
  await bad({ roleWeights: [{ roleId: R_BONUS, weight: 99 }] }, /not valid/);
  await bad({ roleWeights: [{ roleId: R_BONUS, weight: 1 }] }, /not valid/);
  await bad({ xpPrize: 0 }, /not valid/);
  await bad({ xpPrize: 1.5 }, /not valid/);
  await bad({ embedColor: -1 }, /not valid/);
  await bad({ description: 'x'.repeat(1001) }, /not valid/);
  await bad({ endsAt: 12345 }, /not valid/);
  await bad({ prize: '   ' }, /prize cannot be empty/);
  await bad({ prize: 'p'.repeat(257) }, /at most 256/);
  for (const winnerCount of [0, 91]) await bad({ winnerCount }, /between 1 and 90 winners/);
  // Time.
  await bad({ duration: null }, /how long the giveaway runs/);
  await bad({ endsAt: new Date(NOW.getTime() + DAY).toISOString() }, /not both/);
  await bad({ duration: 'soon' }, /30m, 2h, 3d or 1w/);
  await bad({ duration: '30s' }, /at least 1 minute/);
  await bad({ duration: '91d' }, /at most 90 days/);
  await bad({ duration: null, endsAt: new Date(NOW.getTime() - 1000).toISOString() }, /in the past/);
  await bad({ duration: null, endsAt: 'tomorrow' }, /couldn't understand/);
  // Roles.
  await bad({ requiredRoleIds: [env.guild.id] }, /not @everyone/);
  await bad({ deniedRoleIds: [env.guild.id] }, /not @everyone/);
  await bad({ requiredRoleIds: [R_GONE] }, /required roles no longer exists/);
  await bad({ deniedRoleIds: [R_GONE] }, /blocked roles no longer exists/);
  await bad({ requiredRoleIds: [R_REQ], deniedRoleIds: [R_REQ] }, /both required and blocked/);
  await bad({ roleWeights: [{ roleId: env.guild.id, weight: 2 }] }, /not @everyone/);
  await bad({ roleWeights: [{ roleId: R_GONE, weight: 2 }] }, /bonus odds no longer exists/);
  await bad({ roleWeights: [{ roleId: R_BONUS, weight: 2 }, { roleId: R_BONUS, weight: 3 }] }, /only once/);
  await bad({ pingRoleId: env.guild.id }, /not @everyone/);
  await bad({ pingRoleId: R_GONE }, /role to ping no longer exists/);
  // Prizes: the xp provider exists but its module can be off; the coins provider is not installed.
  await bad({ coinPrize: 50 }, /Coin prizes are not available on this bot/);
  await bad({ xpPrize: 50 }, /XP prizes need the .*Levels.* module/, giveawayHarness({ settings: settingsRow({ xpEnabled: false }) }));
  assert.equal(h.calls.length, 0, 'nothing was started');
});

test('giveaway.start: channel and permission checks, for the bot and for the dashboard user', async () => {
  const env = makeEnv();
  const h = giveawayHarness();
  const bad = (over, re) => rejectsTask(runTask(h, env, 'giveaway.start', gwStart(over)), re);
  await bad({ channelId: CH2 }, /channel no longer exists/);
  // The bot cannot see the channel (50001 / 50013) is said; any other REST failure is a real error, not a TaskError.
  const blind = giveawayHarness({ service: { fetchChannel: async () => { throw discordErr(50001); } } });
  await rejectsTask(runTask(blind, env, 'giveaway.start', gwStart()), /can't see that channel/);
  const broken = giveawayHarness({ service: { fetchChannel: async () => { throw new Error('REST down'); } } });
  await assert.rejects(runTask(broken, env, 'giveaway.start', gwStart()), (err) => !(err instanceof TaskError) && /REST down/.test(err.message));
  const voice = makeChannel(env.guild, CH2, { type: ChannelType.GuildVoice, name: 'Lounge' });
  await bad({ channelId: voice.id }, /text or announcement channels only/);
  const news = makeChannel(env.guild, sid('4', 3), { type: ChannelType.GuildAnnouncement, name: 'news' });
  await runTask(h, env, 'giveaway.start', gwStart({ channelId: news.id, pingRoleId: null }));
  assert.equal(h.calls.at(-1).args.channel, news);

  env.channel.deny(env.moderator, PermissionFlagsBits.SendMessages);
  await bad({}, /You can't send messages in #giveaways/);
  env.channel.deny({ id: BOT }, PermissionFlagsBits.EmbedLinks, PermissionFlagsBits.ReadMessageHistory);
  const env2 = makeEnv();
  env2.channel.deny({ id: BOT }, PermissionFlagsBits.EmbedLinks, PermissionFlagsBits.ReadMessageHistory);
  await rejectsTask(runTask(h, env2, 'giveaway.start', gwStart()), /I need Embed Links, Read Message History in #giveaways to run a giveaway there/);

  // The ping role: mentionable ones pass; a quiet one needs Mention Everyone for the bot and for the user.
  const env3 = makeEnv();
  await runTask(h, env3, 'giveaway.start', gwStart({ pingRoleId: R_QUIET })); // both may mention everyone: fine
  env3.channel.deny({ id: BOT }, PermissionFlagsBits.MentionEveryone);
  await rejectsTask(runTask(h, env3, 'giveaway.start', gwStart({ pingRoleId: R_QUIET })), /I can't ping @role-006 in #giveaways/);
  const env4 = makeEnv();
  env4.channel.deny(env4.moderator, PermissionFlagsBits.MentionEveryone);
  await rejectsTask(runTask(h, env4, 'giveaway.start', gwStart({ pingRoleId: R_QUIET })), /You don't have permission to ping @role-006/);
  await runTask(h, env4, 'giveaway.start', gwStart({ pingRoleId: R_PING })); // mentionable: fine for everyone
});

test('giveaway.start: the service\'s refusals become friendly messages', async () => {
  const env = makeEnv();
  const fail = (code) => giveawayHarness({ service: { startGiveaway: async () => ({ ok: false, code }) } });
  await rejectsTask(runTask(fail('LIMIT'), env, 'giveaway.start', gwStart()), /already has 100 running or draft giveaways/);
  await rejectsTask(runTask(fail('SEND_FAILED'), env, 'giveaway.start', gwStart()), /couldn't post the giveaway in #giveaways/);
  await rejectsTask(runTask(fail('DB_ERROR'), env, 'giveaway.start', gwStart()), /did not respond/);
});

const gwRow = (over = {}) => ({
  id: GW_ID, guildId: GUILD, channelId: CH, messageId: sid('3', 1), hostId: ACTOR, prize: 'Nitro', winnerCount: 1,
  roleWeights: [], endsAt: new Date(NOW.getTime() + DAY), status: 'ACTIVE', ...over,
});

test('giveaway.end: the command\'s status checks and result codes', async () => {
  const env = makeEnv();
  const w1 = sid('7', 5);
  const w2 = sid('7', 6);
  env.guild.members.cache.set(w1, makeMember(env.guild, w1, { displayName: 'Bob' }));
  const ok = giveawayHarness({ rows: [gwRow()], service: { endGiveaway: async () => ({ ok: true, winnerIds: [w1, w2], entryCount: 5, announced: true, payWarnings: [] }) } });
  const out = await runTask(ok, env, 'giveaway.end', { giveawayId: GW_ID });
  assert.match(out.message, /Ended "Nitro"\. Winners: Bob, user 700000000000000006\./);
  assert.deepEqual(out.data, { giveawayId: GW_ID });

  const none = giveawayHarness({ rows: [gwRow()], service: { endGiveaway: async () => ({ ok: true, winnerIds: [], entryCount: 0, announced: false, payWarnings: ['⚠️ The XP prize could not be paid.'] }) } });
  assert.match((await runTask(none, env, 'giveaway.end', { giveawayId: GW_ID })).message,
    /with no valid entrants\. Note: The XP prize could not be paid\. I could not post the announcement/);

  const status = (s, re) => rejectsTask(runTask(giveawayHarness({ rows: [gwRow({ status: s })] }), env, 'giveaway.end', { giveawayId: GW_ID }), re);
  await status('DRAFT', /is a draft/);
  await status('ENDED', /already ended/);
  await status('CANCELLED', /is cancelled/);
  await rejectsTask(runTask(giveawayHarness(), env, 'giveaway.end', { giveawayId: GW_ID }), /no longer exists/);
  await rejectsTask(runTask(giveawayHarness({ rows: [gwRow({ guildId: sid('1', 1) })] }), env, 'giveaway.end', { giveawayId: GW_ID }), /no longer exists/);
  await rejectsTask(runTask(giveawayHarness(), env, 'giveaway.end', { giveawayId: 'x; DROP' }), /no longer exists/);

  const code = (res, re) => rejectsTask(runTask(giveawayHarness({ rows: [gwRow()], service: { endGiveaway: async () => res } }), env, 'giveaway.end', { giveawayId: GW_ID }), re);
  await code({ ok: false, code: 'NOT_ACTIVE' }, /already ended/);
  await code({ ok: false, code: 'DRAFTED' }, /moved to drafts/);
  await code({ ok: false, code: 'NO_ACCESS', missing: ['EmbedLinks'] }, /I need Embed Links in the giveaway channel/);
  await code({ ok: false, code: 'NO_ACCESS', missing: [], reason: 'I need **Send Messages** in <#1>.' }, /I need Send Messages in/);
  await code({ ok: false, code: 'RESOLVE_FAILED' }, /couldn't check every entrant/);
  await code({ ok: false, code: 'BOOM' }, /did not respond/);
  const seen = giveawayHarness({ rows: [gwRow()] });
  await runTask(seen, env, 'giveaway.end', { giveawayId: GW_ID });
  assert.equal(seen.calls.at(-1).args.source, 'command');
  assert.equal(seen.calls.at(-1).args.giveawayId, GW_ID);
});

test('giveaway.reroll and giveaway.cancel: the command\'s checks', async () => {
  const env = makeEnv();
  const ended = gwRow({ status: 'ENDED' });
  const h = giveawayHarness({ rows: [ended] });
  const out = await runTask(h, env, 'giveaway.reroll', { giveawayId: GW_ID, winners: 3 });
  assert.match(out.message, /New winner of "Nitro": user 700000000000000005\./);
  assert.deepEqual([h.calls.at(-1).name, h.calls.at(-1).args.count], ['rerollGiveaway', 3]);
  await runTask(h, env, 'giveaway.reroll', { giveawayId: GW_ID });
  assert.equal(h.calls.at(-1).args.count, 1);
  await rejectsTask(runTask(h, env, 'giveaway.reroll', { giveawayId: GW_ID, winners: 91 }), /between 1 and 90/);
  await rejectsTask(runTask(h, env, 'giveaway.reroll', { giveawayId: GW_ID, winners: 0 }), /between 1 and 90/);
  await rejectsTask(runTask(giveawayHarness({ rows: [gwRow()] }), env, 'giveaway.reroll', { giveawayId: GW_ID }), /still running/);
  await rejectsTask(runTask(giveawayHarness({ rows: [gwRow({ status: 'CANCELLED' })] }), env, 'giveaway.reroll', { giveawayId: GW_ID }), /Only ended/);
  const code = (res, re) => rejectsTask(runTask(giveawayHarness({ rows: [ended], service: { rerollGiveaway: async () => res } }), env, 'giveaway.reroll', { giveawayId: GW_ID }), re);
  await code({ ok: false, code: 'NO_ENTRANTS' }, /no other eligible entrants/);
  await code({ ok: false, code: 'CONFLICT' }, /changed while I was drawing/);
  await code({ ok: false, code: 'RESOLVE_FAILED' }, /couldn't check every entrant/);
  await code({ ok: false, code: 'NOT_ENDED' }, /Only ended/);

  for (const status of ['ACTIVE', 'DRAFT']) {
    const c = giveawayHarness({ rows: [gwRow({ status })] });
    assert.match((await runTask(c, env, 'giveaway.cancel', { giveawayId: GW_ID })).message, /Cancelled the giveaway for "Nitro"/);
    assert.equal(c.calls.at(-1).name, 'cancelGiveaway');
  }
  await rejectsTask(runTask(giveawayHarness({ rows: [gwRow({ status: 'ENDED' })] }), env, 'giveaway.cancel', { giveawayId: GW_ID }), /already ended/);
  await rejectsTask(runTask(giveawayHarness({ rows: [gwRow()], service: { cancelGiveaway: async () => ({ ok: false, code: 'NOT_OPEN' }) } }), env,
    'giveaway.cancel', { giveawayId: GW_ID }), /already ended or was cancelled/);
});

test('giveaway.restart: a draft goes live again like /giveaway restart', async () => {
  const env = makeEnv();
  const draft = gwRow({ status: 'DRAFT', messageId: null });
  const h = giveawayHarness({ rows: [draft] });
  // No new end: the old one (a day away) is kept, in the original channel.
  const kept = await runTask(h, env, 'giveaway.restart', { giveawayId: GW_ID });
  assert.match(kept.message, /"Nitro" is live again in #giveaways and ends 2026-10-12 12:00 UTC/);
  let args = h.calls.at(-1).args;
  assert.equal(args.channel, env.channel);
  assert.equal(args.endsAt.getTime(), draft.endsAt.getTime());
  // A new length and a new channel.
  const news = makeChannel(env.guild, CH2, { type: ChannelType.GuildAnnouncement, name: 'news' });
  await runTask(h, env, 'giveaway.restart', { giveawayId: GW_ID, channelId: CH2, duration: '3d' });
  args = h.calls.at(-1).args;
  assert.equal(args.channel, news);
  assert.equal(args.endsAt.getTime(), NOW.getTime() + 3 * DAY);
  // An exact end time.
  const exact = new Date(NOW.getTime() + 5 * DAY).toISOString();
  await runTask(h, env, 'giveaway.restart', { giveawayId: GW_ID, endsAt: exact });
  assert.equal(h.calls.at(-1).args.endsAt.toISOString(), exact);

  await rejectsTask(runTask(h, env, 'giveaway.restart', { giveawayId: GW_ID, duration: '1d', endsAt: exact }), /not both/);
  await rejectsTask(runTask(h, env, 'giveaway.restart', { giveawayId: GW_ID, duration: '10s' }), /at least 1 minute/);
  await rejectsTask(runTask(h, env, 'giveaway.restart', { giveawayId: GW_ID, channelId: sid('4', 77) }), /That channel no longer exists/);
  await rejectsTask(runTask(h, env, 'giveaway.restart', { giveawayId: GW_ID, channelId: 'x' }), /not valid/);
  const stale = giveawayHarness({ rows: [gwRow({ status: 'DRAFT', endsAt: new Date(NOW.getTime() + 30_000) })] });
  await rejectsTask(runTask(stale, env, 'giveaway.restart', { giveawayId: GW_ID }), /end time has passed/);
  await runTask(stale, env, 'giveaway.restart', { giveawayId: GW_ID, duration: '1h' });
  await rejectsTask(runTask(giveawayHarness({ rows: [gwRow()] }), env, 'giveaway.restart', { giveawayId: GW_ID }), /Only draft giveaways/);
  const gone = giveawayHarness({ rows: [gwRow({ status: 'DRAFT', channelId: sid('4', 88) })] });
  await rejectsTask(runTask(gone, env, 'giveaway.restart', { giveawayId: GW_ID }), /original channel no longer exists/);
  const code = (res, re) => rejectsTask(runTask(giveawayHarness({ rows: [draft], service: { restartGiveaway: async () => res } }), env, 'giveaway.restart', { giveawayId: GW_ID }), re);
  await code({ ok: false, code: 'CONFLICT' }, /changed meanwhile/);
  await code({ ok: false, code: 'NOT_DRAFT' }, /changed meanwhile/);
  await code({ ok: false, code: 'SEND_FAILED' }, /couldn't post the giveaway/);
});

test('giveaway.odds: removals first, then additions, like /giveaway odds remove and add', async () => {
  const env = makeEnv();
  const row = gwRow({ roleWeights: [{ roleId: R_BONUS, weight: 3 }, { roleId: R_BONUS2, weight: 2 }] });
  const h = giveawayHarness({ rows: [row] });
  const out = await runTask(h, env, 'giveaway.odds', { giveawayId: GW_ID, roleWeights: [{ roleId: R_BONUS, weight: 5 }, { roleId: R_PING, weight: 2 }] });
  assert.match(out.message, /Updated the bonus odds of "Nitro"/);
  assert.deepEqual(h.calls.map((c) => [c.args.op, c.args.roleId, c.args.weight]), [
    ['remove', R_BONUS2, undefined], ['add', R_BONUS, 5], ['add', R_PING, 2],
  ]);
  assert.equal(h.calls[0].args.giveawayId, GW_ID);

  // Nothing to do.
  const same = giveawayHarness({ rows: [row] });
  const idle = await runTask(same, env, 'giveaway.odds', { giveawayId: GW_ID, roleWeights: [{ roleId: R_BONUS2, weight: 2 }, { roleId: R_BONUS, weight: 3 }] });
  assert.match(idle.message, /already like that/);
  assert.equal(same.calls.length, 0);
  // Clearing the list removes every role.
  const clear = giveawayHarness({ rows: [row] });
  await runTask(clear, env, 'giveaway.odds', { giveawayId: GW_ID, roleWeights: [] });
  assert.deepEqual(clear.calls.map((c) => c.args.op), ['remove', 'remove']);

  await rejectsTask(runTask(giveawayHarness({ rows: [gwRow({ status: 'ENDED' })] }), env, 'giveaway.odds', { giveawayId: GW_ID, roleWeights: [] }), /running or in drafts/);
  await rejectsTask(runTask(h, env, 'giveaway.odds', { giveawayId: GW_ID, roleWeights: 'x' }), /not valid/);
  await rejectsTask(runTask(h, env, 'giveaway.odds', { giveawayId: GW_ID, roleWeights: [{ roleId: R_BONUS, weight: 1 }] }), /not valid/);
  await rejectsTask(runTask(h, env, 'giveaway.odds', { giveawayId: GW_ID, roleWeights: [{ roleId: R_BONUS, weight: 2 }, { roleId: R_BONUS, weight: 3 }] }), /only once/);
  await rejectsTask(runTask(h, env, 'giveaway.odds', { giveawayId: GW_ID, roleWeights: [{ roleId: R_GONE, weight: 2 }] }), /no longer exists/);
  const code = (res, re) => rejectsTask(runTask(giveawayHarness({ rows: [gwRow()], service: { updateOdds: async () => res } }), env, 'giveaway.odds',
    { giveawayId: GW_ID, roleWeights: [{ roleId: R_BONUS, weight: 2 }] }), re);
  await code({ ok: false, code: 'LIMIT' }, /at most 10 bonus-odds roles/);
  await code({ ok: false, code: 'NOT_OPEN' }, /running or in drafts/);
  await code({ ok: false, code: 'NOT_FOUND' }, /no longer exists/);
  // A role already removed meanwhile is not an error.
  const raced = giveawayHarness({ rows: [row], service: { updateOdds: async ({ op }) => (op === 'remove' ? { ok: false, code: 'NOT_SET' } : { ok: true }) } });
  await runTask(raced, env, 'giveaway.odds', { giveawayId: GW_ID, roleWeights: [{ roleId: R_BONUS, weight: 3 }] });
});

test('a failing task reaches the dashboard as the message of a PermanentJobError', async () => {
  const env = makeEnv();
  const tasks = { 'giveaway.start': giveawayHarness({ settings: settingsRow({ giveawaysEnabled: false }) }).tasks['giveaway.start'] };
  const fakeGuild = { ...env.guild, members: { ...env.guild.members, fetch: async () => env.moderator } };
  await assert.rejects(
    executeTask(tasks, { id: 'job1', guildId: GUILD, payload: { kind: 'giveaway.start', actorId: ACTOR, payload: gwStart() } }, { client: env.client, guild: fakeGuild }),
    (err) => err instanceof PermanentJobError && /Giveaways module is turned off/.test(err.message),
  );
});

// ── Poll tasks ────────────────────────────────────────────────────────────────

function pollHarness({ settings = settingsRow(), rows = [], service = {}, scheduleFails = false, updateFails = false } = {}) {
  const calls = [];
  const dbCalls = [];
  const svc = {
    createPoll: async (client, guild, pending, input) => {
      calls.push({ name: 'createPoll', pending, input });
      return {
        ok: true,
        poll: {
          id: POLL_ID, guildId: guild.id, channelId: pending.channelId, authorId: pending.userId, question: input.question, options: input.options,
          maxChoices: input.maxChoices, allowedRoleIds: pending.allowedRoleIds, deniedRoleIds: pending.deniedRoleIds, hideResults: pending.hideResults,
          endsAt: pending.durationMs ? new Date(NOW.getTime() + pending.durationMs) : null, status: 'ACTIVE', createdAt: NOW, messageId: null,
        },
      };
    },
    endPoll: async (client, id, opts) => {
      calls.push({ name: 'endPoll', id, opts });
      return { ok: true, poll: {}, tally: {}, posted: true, messageGone: false };
    },
    ...service,
  };
  const db = {
    poll: {
      findFirst: async ({ where }) => rows.find((r) => r.id === where.id && r.guildId === where.guildId) ?? null,
      update: async (args) => {
        dbCalls.push({ name: 'update', args });
        if (updateFails) throw new Error('db down');
        return {};
      },
      delete: async (args) => { dbCalls.push({ name: 'delete', args }); return {}; },
      deleteMany: async (args) => { dbCalls.push({ name: 'deleteMany', args }); return { count: 1 }; },
    },
  };
  const scheduled = [];
  const schedule = async (job) => {
    scheduled.push(job);
    if (scheduleFails) throw new Error('scheduler down');
    return { id: 'j' };
  };
  const tasks = createPollTasks({ service: svc, getSettings: async () => settings, db, schedule });
  return { tasks, calls, dbCalls, scheduled };
}

const pollStart = (over = {}) => ({
  question: ' Best pizza? ', options: ['Margherita', 'Pepperoni', 'Hawaiian'], channelId: CH, maxChoices: 2, duration: '1d',
  allowedRoleIds: [R_REQ], deniedRoleIds: [R_DENY], hideResults: true, ...over,
});

test('poll.start: posts the poll like the form submit does', async () => {
  const env = makeEnv();
  const h = pollHarness();
  const out = await runTask(h, env, 'poll.start', pollStart());
  assert.deepEqual(out.data, { pollId: POLL_ID });
  assert.match(out.message, /Posted the poll "Best pizza\?" in #giveaways\. It ends 2026-10-12 12:00 UTC\./);
  const { pending, input } = h.calls.at(-1);
  assert.deepEqual(pending, {
    guildId: GUILD, channelId: CH, userId: ACTOR, maxChoices: 2, allowedRoleIds: [R_REQ], deniedRoleIds: [R_DENY], hideResults: true, durationMs: DAY,
  });
  assert.deepEqual(input, { question: 'Best pizza?', options: ['Margherita', 'Pepperoni', 'Hawaiian'], maxChoices: 2 });
  // The message: the poll embed and the vote components (a select for several choices).
  const sent = env.channel.sent.at(-1);
  assert.equal(sent.embeds.length, 1);
  assert.equal(sent.components.length, 1);
  assert.deepEqual(sent.allowedMentions, { parse: [] });
  assert.deepEqual(h.dbCalls.map((c) => c.name), ['update']);
  assert.deepEqual(h.dbCalls[0].args, { where: { id: POLL_ID }, data: { messageId: sent.id } });
  assert.equal(h.scheduled.length, 1);
  assert.deepEqual([h.scheduled[0].type, h.scheduled[0].guildId, h.scheduled[0].dedupeKey, h.scheduled[0].payload],
    ['poll.end', GUILD, `poll.end:${POLL_ID}`, { pollId: POLL_ID }]);
  assert.equal(h.scheduled[0].runAt.getTime(), NOW.getTime() + DAY);
});

test('poll.start: single choice gets buttons, no length schedules no end, markers are stripped, choices are clamped', async () => {
  const env = makeEnv();
  const h = pollHarness();
  const out = await runTask(h, env, 'poll.start', pollStart({ maxChoices: 1, duration: null, options: ['1. Yes', '- No'], allowedRoleIds: [env.guild.id] }));
  assert.equal(h.scheduled.length, 0, 'no end job without a length');
  assert.doesNotMatch(out.message, /It ends/);
  assert.deepEqual(h.calls.at(-1).input.options, ['Yes', 'No']);
  assert.deepEqual(h.calls.at(-1).pending.allowedRoleIds, [], '@everyone as the allowed role means no restriction');
  assert.equal(env.channel.sent.at(-1).components.length, 1, 'two buttons fit one row');

  const clamped = await runTask(h, env, 'poll.start', pollStart({ maxChoices: 9, options: ['a', 'b'] }));
  assert.match(clamped.message, /Choices per member was lowered to 2/);
  assert.equal(h.calls.at(-1).input.maxChoices, 2);
});

test('poll.start: refuses what the poll form and /poll create refuse', async () => {
  const env = makeEnv();
  const h = pollHarness();
  const bad = (over, re) => rejectsTask(runTask(h, env, 'poll.start', pollStart(over)), re);
  await rejectsTask(runTask(h, env, 'poll.start', null), /not valid/);
  await bad({ question: 5 }, /not valid/);
  await bad({ options: 'a\nb' }, /not valid/);
  await bad({ options: ['a', 3] }, /not valid/);
  await bad({ options: ['a', 'b\nc'] }, /not valid/);
  await bad({ channelId: 'x' }, /not valid/);
  await bad({ allowedRoleIds: 'x' }, /not valid/);
  await bad({ hideResults: 'yes' }, /not valid/);
  await bad({ duration: 5 }, /not valid/);
  await bad({ maxChoices: 0 }, /between 1 and 20/);
  await bad({ maxChoices: 21 }, /between 1 and 20/);
  await bad({ question: '  ' }, /question is empty/);
  await bad({ question: 'q'.repeat(257) }, /257 characters \(max 256\)/);
  await bad({ options: ['only'] }, /at least 2 options/);
  await bad({ options: ['Same', 'same'] }, /Option 2 repeats option 1/);
  await bad({ options: ['ok', 'x'.repeat(81)] }, /Option 2 is 81 characters \(max 80\)/);
  await bad({ options: Array.from({ length: 21 }, (_, i) => `o${i}`) }, /21 options \(max 20\)/);
  await bad({ duration: 'soon' }, /30m, 2h, 1d or 1w/);
  await bad({ duration: '30s' }, /at least 1 minute/);
  await bad({ duration: '31d' }, /at most 30 days/);
  await bad({ deniedRoleIds: [env.guild.id] }, /can't block @everyone/);
  await bad({ allowedRoleIds: [R_REQ], deniedRoleIds: [R_REQ] }, /must be different/);
  await bad({ allowedRoleIds: [R_GONE] }, /role you picked no longer exists/);
  await bad({ channelId: sid('4', 77) }, /channel no longer exists/);
  const envBot = makeEnv();
  envBot.channel.deny({ id: BOT }, PermissionFlagsBits.EmbedLinks);
  await rejectsTask(runTask(h, envBot, 'poll.start', pollStart()), /I need Embed Links in #giveaways to post a poll/);
  const envMember = makeEnv();
  envMember.channel.deny(envMember.moderator, PermissionFlagsBits.SendMessages);
  await rejectsTask(runTask(h, envMember, 'poll.start', pollStart()), /You need permission to view and send messages in #giveaways/);
  assert.equal(h.calls.filter((c) => c.name === 'createPoll').length, 0);
});

test('poll.start: the cap, a failed post and a failed save are cleaned up like the form submit does', async () => {
  const env = makeEnv();
  const cap = pollHarness({ service: { createPoll: async () => ({ ok: false, code: 'LIMIT' }) } });
  await rejectsTask(runTask(cap, env, 'poll.start', pollStart()), /already has 50 active polls/);

  const env2 = makeEnv();
  env2.channel.failSend = discordErr(50013);
  const sendFail = pollHarness();
  await rejectsTask(runTask(sendFail, env2, 'poll.start', pollStart()), /couldn't post the poll in #giveaways: I'm missing permissions there/);
  assert.deepEqual(sendFail.dbCalls.map((c) => [c.name, c.args.where.id]), [['delete', POLL_ID]]);
  assert.equal(sendFail.scheduled.length, 0);

  const env3 = makeEnv();
  const saveFail = pollHarness({ updateFails: true });
  await rejectsTask(runTask(saveFail, env3, 'poll.start', pollStart()), /couldn't save the poll/);
  assert.deepEqual(saveFail.dbCalls.map((c) => c.name), ['update', 'delete']);
  assert.equal(env3.channel.store.size, 0, 'the posted message was deleted again');

  const env4 = makeEnv();
  const timer = pollHarness({ scheduleFails: true });
  const out = await runTask(timer, env4, 'poll.start', pollStart());
  assert.match(out.message, /end timer could not be scheduled/);
  assert.deepEqual(out.data, { pollId: POLL_ID });
});

test('poll tasks: the same gate and level as the command', async () => {
  const env = makeEnv();
  await rejectsTask(runTask(pollHarness(), env, 'poll.end', { pollId: POLL_ID }, { actor: { id: ACTOR, member: null, isBotOwner: false } }), /no longer a member/);
  await rejectsTask(runTask(pollHarness({ settings: null }), env, 'poll.start', pollStart()), /settings could not be loaded/);
  const off = pollHarness({ settings: settingsRow({ pollsEnabled: false }) });
  for (const [kind, payload] of [['poll.start', pollStart()], ['poll.end', { pollId: POLL_ID }], ['poll.delete', { pollId: POLL_ID }]]) {
    await rejectsTask(runTask(off, env, kind, payload), /Polls module is turned off/);
  }
  const plain = { id: sid('7', 2), member: makeMember(env.guild, sid('7', 2)), isBotOwner: false };
  const h = pollHarness();
  await rejectsTask(runTask(h, env, 'poll.start', pollStart(), { actor: plain }), /need to be a moderator/);
  assert.equal(h.calls.length, 0);
  // The bot owner outside the server posts as themselves; the bot's own access is still checked.
  await runTask(h, env, 'poll.start', pollStart(), { actor: { id: OWNER_ID, member: null, isBotOwner: true } });
  assert.equal(h.calls.at(-1).pending.userId, OWNER_ID);
});

const pRow = (over = {}) => ({ id: POLL_ID, guildId: GUILD, channelId: CH, messageId: sid('3', 2), question: 'Best pizza?', status: 'ACTIVE', ...over });

test('poll.end: /poll end', async () => {
  const env = makeEnv();
  const h = pollHarness({ rows: [pRow()] });
  const out = await runTask(h, env, 'poll.end', { pollId: POLL_ID });
  assert.match(out.message, /Ended "Best pizza\?"\. The results were posted in #giveaways\./);
  assert.deepEqual(h.calls.at(-1), { name: 'endPoll', id: POLL_ID, opts: { reason: 'manual', guildId: GUILD } });
  const quiet2 = pollHarness({ rows: [pRow()], service: { endPoll: async () => ({ ok: true, posted: false }) } });
  assert.match((await runTask(quiet2, env, 'poll.end', { pollId: POLL_ID })).message, /couldn't post the results in #giveaways/);
  const gone = pollHarness({ rows: [pRow()], service: { endPoll: async () => ({ ok: false, code: 'ALREADY_ENDED' }) } });
  await rejectsTask(runTask(gone, env, 'poll.end', { pollId: POLL_ID }), /already ended/);
  const vanished = pollHarness({ rows: [pRow()], service: { endPoll: async () => ({ ok: false, code: 'NOT_FOUND' }) } });
  await rejectsTask(runTask(vanished, env, 'poll.end', { pollId: POLL_ID }), /no longer exists/);
  await rejectsTask(runTask(pollHarness(), env, 'poll.end', { pollId: POLL_ID }), /no longer exists/);
  await rejectsTask(runTask(pollHarness({ rows: [pRow({ guildId: sid('1', 1) })] }), env, 'poll.end', { pollId: POLL_ID }), /no longer exists/);
  await rejectsTask(runTask(pollHarness(), env, 'poll.end', { pollId: 'nope' }), /no longer exists/);
});

test('poll.delete: ends an active poll silently, removes its message and the record', async () => {
  const env = makeEnv();
  const message = await env.channel.send({ content: 'poll' });
  const row = pRow({ messageId: message.id });
  const h = pollHarness({ rows: [row] });
  const out = await runTask(h, env, 'poll.delete', { pollId: POLL_ID });
  assert.match(out.message, /Deleted the poll "Best pizza\?" and its votes\.$/);
  assert.deepEqual(h.calls.map((c) => [c.name, c.opts?.reason]), [['endPoll', 'deleted']]);
  assert.deepEqual(env.channel.deleted, [message.id]);
  assert.deepEqual(h.dbCalls, [{ name: 'deleteMany', args: { where: { id: POLL_ID, guildId: GUILD } } }]);

  // An ended poll is not ended again; a message that is already gone is fine.
  const done = pollHarness({ rows: [pRow({ status: 'ENDED' })] });
  assert.doesNotMatch((await runTask(done, env, 'poll.delete', { pollId: POLL_ID })).message, /by hand/);
  assert.equal(done.calls.length, 0);
  assert.equal(done.dbCalls.length, 1);

  // Missing permissions or an unexpected failure are said, the record still goes.
  const env2 = makeEnv();
  const m2 = await env2.channel.send({ content: 'poll' });
  env2.channel.failDelete = discordErr(50013);
  const noAccess = pollHarness({ rows: [pRow({ messageId: m2.id })] });
  assert.match((await runTask(noAccess, env2, 'poll.delete', { pollId: POLL_ID })).message, /missing permissions there/);
  assert.equal(noAccess.dbCalls.length, 1);
  env2.channel.failDelete = new Error('boom');
  const boom = pollHarness({ rows: [pRow({ messageId: m2.id })] });
  assert.match((await runTask(boom, env2, 'poll.delete', { pollId: POLL_ID })).message, /could not delete its message/);
  // A deleted channel counts as the message being gone.
  const env3 = makeEnv();
  env3.guild.channels.cache.delete(CH);
  const noChannel = pollHarness({ rows: [pRow({ status: 'ENDED' })] });
  assert.doesNotMatch((await runTask(noChannel, env3, 'poll.delete', { pollId: POLL_ID })).message, /by hand/);
  await rejectsTask(runTask(pollHarness(), env, 'poll.delete', { pollId: POLL_ID }), /no longer exists/);
});

// ── Stores against a database (opt-in) ────────────────────────────────────────

const HOST = ACTOR;
const WINNER_BASE = (n) => sid('3', 100 + n);

/** A fake dashboard context whose runTask runs the REAL task against the database and the fake guild. */
async function dbEnv(no, { memberCount = 0 } = {}) {
  const guildId = guildNo(no);
  await ensureGuildSettings(guildId);
  const guild = makeGuild(guildId, {
    roles: [R_REQ, R_DENY, R_BONUS, R_BONUS2, R_PING].map((id) => makeRole(id)),
    ownerId: sid('9', 1),
  });
  const channel = makeChannel(guild, nextId(), { name: 'giveaways' });
  const moderator = makeMember(guild, HOST, { perms: PermissionFlagsBits.ModerateMembers, displayName: 'Host Name' });
  guild.members.cache.set(HOST, moderator);
  for (let i = 0; i < memberCount; i++) {
    guild.members.cache.set(WINNER_BASE(i), makeMember(guild, WINNER_BASE(i), { displayName: `Winner ${i}`, roles: [R_REQ] }));
  }
  const client = { user: { id: BOT }, guilds: { cache: new Map([[guildId, guild]]) }, channels: { cache: guild.channels.cache, fetch: (id) => guild.channels.fetch(id) } };
  const giveawayTasks = createGiveawayTasks();
  const pollTasks = createPollTasks();
  const all = { ...giveawayTasks, ...pollTasks };
  const calls = [];
  const discord = {
    async getUsers(ids) {
      return new Map(ids.map((id) => [id, id === HOST ? { id, username: 'host', globalName: 'Host Name' } : null]));
    },
  };
  const ctx = {
    guildId, userId: HOST, user: { id: HOST }, member: null, discord, logger: { warn() {}, info() {} },
    async runTask(kind, payload) {
      calls.push(kind);
      const actor = { id: HOST, member: moderator, isBotOwner: false };
      try {
        const r = await all[kind].run({ guild, client, actor, payload: JSON.parse(JSON.stringify(payload)), taskId: 'test' });
        return { status: 'done', message: r.message, data: r.data ?? null };
      } catch (err) {
        if (err instanceof TaskError) return { status: 'failed', message: err.message, data: null };
        throw err;
      }
    },
  };
  return { guildId, guild, channel, client, ctx, calls, moderator };
}

const dbGw = (env, over = {}) => prisma.giveaway.create({
  data: {
    guildId: env.guildId, channelId: env.channel.id, hostId: HOST, prize: 'Row prize', winnerCount: 1, roleWeights: [], status: 'ACTIVE',
    endsAt: new Date(Date.now() + 3600_000), ...over,
  },
});

test('giveaways store: start, list, odds, end, reroll, cancel, restart end to end', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/giveaways.store.js');
  const env = await dbEnv(1, { memberCount: 5 });
  const { guildId, ctx } = env;

  // Start: the real task posts the message with the Participate button and schedules the end.
  const item = await store.create(guildId, create('giveaways', {
    prize: 'Nitro', description: 'Be quick', channelId: env.channel.id, winnerCount: 2, duration: '2d', requiredRoleIds: [R_REQ],
    roleWeights: [{ roleId: R_BONUS, weight: 3 }], pingRoleId: R_PING, embedColor: '#5865F2',
  }), ctx);
  assert.equal(item.prize, 'Nitro');
  assert.equal(item.status, 'Running');
  assert.equal(item.host, 'Host Name');
  assert.equal(item.entries, 0);
  assert.deepEqual(item.roleWeights, [{ roleId: R_BONUS, weight: 3 }]);
  assert.deepEqual(item.requiredRoleIds, [R_REQ]);
  assert.equal(item.embedColor, 0x5865f2);
  const sent = env.channel.sent.at(-1);
  assert.equal(item.messageUrl, `https://discord.com/channels/${guildId}/${env.channel.id}/${sent.id}`);
  assert.equal(sent.components[0].components[0].data.custom_id, `giveaway:join:${item.id}`);
  assert.equal(sent.content, `<@&${R_PING}>`);
  const row = await prisma.giveaway.findUnique({ where: { id: item.id } });
  assert.equal(row.hostId, HOST);
  assert.equal(row.status, 'ACTIVE');
  assert.equal(row.messageId, sent.id);
  const job = await getJob(`giveaway.end:${item.id}`);
  assert.equal(job.type, 'giveaway.end');
  assert.ok(Math.abs(job.runAt.getTime() - (Date.now() + 2 * DAY)) < 60_000, 'ends in about two days');
  assert.equal((await store.get(guildId, item.id)).prize, 'Nitro');
  assert.equal(await store.get(guildId, 'cnotthere00000000000000000'), null);
  assert.equal(await store.get(guildOther(), item.id), null, 'another server cannot see it');

  // List: running first (soonest end first), drafts, then finished ones newest first; search; paging.
  const later = await dbGw(env, { prize: 'Later', endsAt: new Date(Date.now() + 5 * DAY) });
  const sooner = await dbGw(env, { prize: 'Sooner Nitro', endsAt: new Date(Date.now() + 3600_000) });
  const draft = await dbGw(env, { prize: 'Draft one', status: 'DRAFT' });
  for (let i = 0; i < 27; i++) {
    await dbGw(env, { prize: `Old ${String(i).padStart(2, '0')}`, status: i % 2 ? 'ENDED' : 'CANCELLED', endedAt: new Date(), createdAt: new Date(Date.now() - (i + 1) * 60_000), winnerIds: i % 2 ? [WINNER_BASE(0)] : [] });
  }
  const first = await store.list(guildId, { discord: ctx.discord });
  assert.deepEqual(first.items.slice(0, 4).map((i) => i.prize), ['Sooner Nitro', 'Nitro', 'Later', 'Draft one']);
  assert.equal(first.items.length, 4 + 25);
  assert.deepEqual(first.items.slice(4, 6).map((i) => i.prize), ['Old 00', 'Old 01'], 'finished ones: newest first');
  assert.match(first.nextCursor, /^\d+:c[a-z0-9]+$/);
  const second = await store.list(guildId, { cursor: first.nextCursor });
  assert.deepEqual(second.items.map((i) => i.prize), ['Old 25', 'Old 26']);
  assert.equal(second.nextCursor, undefined);
  assert.equal(first.items[0].host, 'Host Name');
  assert.equal(first.items[5].winners[0], WINNER_BASE(0), 'a winner without a name keeps the id');
  assert.deepEqual((await store.list(guildId, { cursor: 'garbage' })).items, []);
  const found = await store.list(guildId, { query: 'nitro' });
  assert.deepEqual(found.items.map((i) => i.prize), ['Sooner Nitro', 'Nitro']);
  assert.deepEqual((await store.list(guildId, { query: 'old 2' })).items.map((i) => i.prize), ['Old 20', 'Old 21', 'Old 22', 'Old 23', 'Old 24', 'Old 25', 'Old 26']);
  assert.equal((await store.list(guildOther())).items.length, 0);
  await prisma.giveaway.deleteMany({ where: { guildId, id: { not: item.id } } });

  // Odds: the only editable thing; the same service calls as /giveaway odds.
  const edited = await store.update(guildId, item.id, update('giveaways', { roleWeights: [{ roleId: R_BONUS2, weight: 5 }] }), ctx);
  assert.deepEqual(edited.roleWeights, [{ roleId: R_BONUS2, weight: 5 }]);
  assert.ok(env.channel.edits.length >= 1, 'the embed was refreshed');
  const same = await store.update(guildId, item.id, {}, ctx);
  assert.deepEqual(same.roleWeights, [{ roleId: R_BONUS2, weight: 5 }]);
  await assert.rejects(store.update(guildId, 'cnotthere00000000000000000', { roleWeights: [] }, ctx), (e) => e.code === 'not_found');
  await assert.rejects(store.remove(guildId, item.id, ctx), (e) => e.code === 'invalid' && /cannot be deleted/.test(e.message));

  // End: the real draw over three entrants (members are in the fake guild), announced in the channel.
  await prisma.giveawayEntry.createMany({ data: [0, 1, 2].map((i) => ({ giveawayId: item.id, userId: WINNER_BASE(i) })) });
  assert.equal((await store.get(guildId, item.id)).entries, 3);
  const ended = await store.actions.end(guildId, item.id, {}, ctx);
  assert.equal(ended.item.status, 'Ended');
  assert.equal(ended.item.winners.length, 2);
  assert.match(ended.message, /^Ended "Nitro"\. Winners: /);
  assert.equal(await getJob(`giveaway.end:${item.id}`), null, 'the end job is gone');
  assert.ok(env.channel.sent.some((m) => /Congratulations/.test(m.content ?? '')));
  await assert.rejects(store.actions.end(guildId, item.id, {}, ctx), (e) => e.code === 'failed' && /already ended/.test(e.message));
  await assert.rejects(store.update(guildId, item.id, { roleWeights: [] }, ctx), (e) => e.code === 'failed' && /running or in drafts/.test(e.message));

  // Reroll: one of the remaining entrants, the earlier winners keep theirs.
  const before = ended.item.winners;
  const again = await store.actions.reroll(guildId, item.id, { winners: 1 }, ctx);
  assert.equal(again.item.winners.length, 3);
  assert.deepEqual(again.item.winners.slice(0, 2), before);
  await assert.rejects(store.actions.reroll(guildId, item.id, { winners: 1 }, ctx), (e) => e.code === 'failed' && /no other eligible entrants/.test(e.message));

  // Cancel a running one; a draft restarts.
  const run = await dbGw(env, { prize: 'To cancel', messageId: (await env.channel.send({ content: 'x' })).id });
  await scheduleJob({ guildId, type: 'giveaway.end', runAt: run.endsAt, payload: { giveawayId: run.id }, dedupeKey: `giveaway.end:${run.id}` });
  const cancelled = await store.actions.cancel(guildId, run.id, {}, ctx);
  assert.equal(cancelled.item.status, 'Cancelled');
  assert.equal(await getJob(`giveaway.end:${run.id}`), null);
  await assert.rejects(store.actions.cancel(guildId, run.id, {}, ctx), (e) => e.code === 'failed' && /is cancelled/.test(e.message));

  const dr = await dbGw(env, { prize: 'Draft restart', status: 'DRAFT', endsAt: new Date(Date.now() + 10_000) });
  await assert.rejects(store.actions.restart(guildId, dr.id, action('giveaways', 'restart', {}), ctx), (e) => e.code === 'failed' && /end time has passed/.test(e.message));
  const live = await store.actions.restart(guildId, dr.id, action('giveaways', 'restart', { duration: '1d' }), ctx);
  assert.equal(live.item.status, 'Running');
  assert.ok(live.item.messageUrl);
  assert.ok(await getJob(`giveaway.end:${dr.id}`));
  await assert.rejects(store.actions.restart(guildId, dr.id, action('giveaways', 'restart', {}), ctx), (e) => e.code === 'failed' && /Only draft/.test(e.message));
  await assert.rejects(store.actions.end(guildId, 'cnotthere00000000000000000', {}, ctx), (e) => e.code === 'not_found');
  await assert.rejects(store.actions.end(guildOther(), item.id, {}, ctx), (e) => e.code === 'not_found');
});

const guildOther = () => guildNo(20);

test('giveaways store: the cap, a module that is off, a missing channel and an offline bot', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/giveaways.store.js');
  const env = await dbEnv(2);
  const { guildId, ctx } = env;
  await prisma.giveaway.createMany({
    data: Array.from({ length: LIMITS.ACTIVE_GIVEAWAYS }, (_, i) => ({
      guildId, channelId: env.channel.id, hostId: HOST, prize: `P${i}`, winnerCount: 1, roleWeights: [], status: i % 2 ? 'ACTIVE' : 'DRAFT',
      endsAt: new Date(Date.now() + 3600_000),
    })),
  });
  const input = create('giveaways', { prize: 'One too many', channelId: env.channel.id, duration: '1d' });
  await assert.rejects(store.create(guildId, input, ctx), (e) => e.code === 'failed' && /already has 100 running or draft giveaways/.test(e.message));
  assert.equal(env.channel.sent.length, 0, 'nothing was posted');
  assert.equal(await prisma.giveaway.count({ where: { guildId } }), LIMITS.ACTIVE_GIVEAWAYS);
  // The list shows them all (the cap is on running and draft ones), with no count limit.
  assert.equal((await store.list(guildId)).items.length, LIMITS.ACTIVE_GIVEAWAYS);

  await prisma.giveaway.deleteMany({ where: { guildId } });
  await updateSettings(guildId, { giveawaysEnabled: false });
  await assert.rejects(store.create(guildId, input, ctx), (e) => e.code === 'failed' && /module is turned off/.test(e.message));
  await updateSettings(guildId, { giveawaysEnabled: true });
  await assert.rejects(store.create(guildId, { ...input, channelId: sid('4', 999) }, ctx), (e) => e.code === 'failed' && /channel no longer exists/.test(e.message));
  env.channel.failSend = discordErr(50013);
  await assert.rejects(store.create(guildId, input, ctx), (e) => e.code === 'failed' && /couldn't post the giveaway/.test(e.message));
  assert.equal(await prisma.giveaway.count({ where: { guildId } }), 0, 'a failed post leaves no row');

  // The bot never answers: offline is unavailable (503); a missing runner too.
  const offline = { ...ctx, runTask: async () => ({ status: 'offline', message: 'The bot did not pick this up in time.', data: null }) };
  await assert.rejects(store.create(guildId, input, offline), (e) => e.code === 'unavailable');
  await assert.rejects(store.create(guildId, input, { ...ctx, runTask: undefined }), (e) => e.code === 'unavailable');
  // Still working: the giveaway the task started is found by its host and time; with none, the wait message is the answer.
  env.channel.failSend = null;
  const pending = { ...ctx, runTask: async (kind, payload) => { await ctx.runTask(kind, payload); return { status: 'pending', message: 'Still working.', data: null }; } };
  const found = await store.create(guildId, input, pending);
  assert.equal(found.prize, 'One too many');
  await prisma.giveaway.deleteMany({ where: { guildId } });
  const lost = { ...ctx, runTask: async () => ({ status: 'pending', message: 'Still working.', data: null }) };
  await assert.rejects(store.create(guildId, { ...input, prize: 'Never started' }, lost), (e) => e.code === 'unavailable' && /Still working/.test(e.message));
  // A finished task without its data (Redis down) is found the same way.
  const noData = { ...ctx, runTask: async (kind, payload) => { const r = await ctx.runTask(kind, payload); return { ...r, data: null }; } };
  assert.equal((await store.create(guildId, { ...input, prize: 'No data' }, noData)).prize, 'No data');
});

const dbPoll = (env, over = {}) => prisma.poll.create({
  data: {
    guildId: env.guildId, channelId: env.channel.id, authorId: HOST, question: 'Row question', options: ['A', 'B', 'C'], maxChoices: 1,
    allowedRoleIds: [], deniedRoleIds: [], hideResults: false, status: 'ACTIVE', ...over,
  },
});

test('polls store: create, tally, list, end, delete end to end', { skip: skipDb }, async () => {
  const { store, RAW_SQL } = await import('../src/dashboard/collections/polls.store.js');
  const env = await dbEnv(3);
  const { guildId, ctx } = env;

  const item = await store.create(guildId, create('polls', {
    question: 'Best pizza?', options: ['Margherita', 'Pepperoni', 'Hawaiian'], channelId: env.channel.id, maxChoices: 2, duration: '1d',
    allowedRoleIds: [R_REQ], hideResults: true,
  }), ctx);
  assert.equal(item.question, 'Best pizza?');
  assert.equal(item.status, 'Active');
  assert.equal(item.author, 'Host Name');
  assert.deepEqual([item.maxChoices, item.hideResults, item.allowedRoleIds], [2, true, [R_REQ]]);
  assert.deepEqual(item.results, ['1. Margherita: 0 votes (0%)', '2. Pepperoni: 0 votes (0%)', '3. Hawaiian: 0 votes (0%)']);
  const sent = env.channel.sent.at(-1);
  assert.equal(item.messageUrl, `https://discord.com/channels/${guildId}/${env.channel.id}/${sent.id}`);
  assert.equal(sent.components[0].components[0].data.custom_id, `poll-select:${item.id}`);
  const row = await prisma.poll.findUnique({ where: { id: item.id } });
  assert.deepEqual([row.authorId, row.messageId, row.hideResults, row.status], [HOST, sent.id, true, 'ACTIVE']);
  assert.ok(row.endsAt && Math.abs(row.endsAt.getTime() - (Date.now() + DAY)) < 60_000);
  assert.equal((await getJob(`poll.end:${item.id}`)).type, 'poll.end');

  const open = await store.create(guildId, create('polls', { question: 'Open ended', options: ['Yes', 'No'], channelId: env.channel.id }), ctx);
  assert.equal(await getJob(`poll.end:${open.id}`), null, 'no length, no end job');
  assert.equal((await prisma.poll.findUnique({ where: { id: open.id } })).endsAt, null);
  assert.equal(open.summary, 'Active, open until ended · 0 votes');

  // Votes: a multi-choice voter counts once as a voter.
  await prisma.pollVote.createMany({
    data: [
      { pollId: item.id, userId: sid('3', 1), optionIndex: 0 }, { pollId: item.id, userId: sid('3', 1), optionIndex: 2 },
      { pollId: item.id, userId: sid('3', 2), optionIndex: 0 }, { pollId: item.id, userId: sid('3', 3), optionIndex: 1 },
    ],
  });
  const listed = await store.list(guildId, { discord: ctx.discord });
  const mine = listed.items.find((i) => i.id === item.id);
  assert.deepEqual([mine.votes, mine.voters], [4, 3]);
  assert.deepEqual(mine.results, ['1. Margherita: 2 votes (50%)', '2. Pepperoni: 1 vote (25%)', '3. Hawaiian: 1 vote (25%)']);
  assert.equal(mine.summary.startsWith('Active, ends '), true);
  assert.equal((await store.get(guildId, item.id)).votes, 4);

  // Paging and search: active first (newest first), then ended ones newest first.
  for (let i = 0; i < 27; i++) {
    await dbPoll(env, { question: `Old poll ${String(i).padStart(2, '0')}`, status: 'ENDED', endsAt: new Date(), createdAt: new Date(Date.now() - (i + 1) * 60_000) });
  }
  const first = await store.list(guildId);
  assert.equal(first.items.length, 2 + 25);
  assert.deepEqual(first.items.slice(0, 2).map((i) => i.question), ['Open ended', 'Best pizza?']);
  assert.equal(first.items[2].question, 'Old poll 00');
  const second = await store.list(guildId, { cursor: first.nextCursor });
  assert.deepEqual(second.items.map((i) => i.question), ['Old poll 25', 'Old poll 26']);
  assert.equal(second.nextCursor, undefined);
  assert.deepEqual((await store.list(guildId, { query: 'PIZZA' })).items.map((i) => i.question), ['Best pizza?']);
  await prisma.poll.deleteMany({ where: { guildId, status: 'ENDED' } });

  // Posted polls cannot be edited.
  await assert.rejects(store.update(guildId, item.id, {}, ctx), (e) => e.code === 'invalid');

  // End now: the final results are posted, the job is gone, the item is ended with its tally.
  const ended = await store.actions.end(guildId, item.id, {}, ctx);
  assert.equal(ended.item.status, 'Ended');
  assert.match(ended.message, /Ended "Best pizza\?"\. The results were posted in #giveaways\./);
  assert.equal(ended.item.votes, 4);
  assert.equal(await getJob(`poll.end:${item.id}`), null);
  assert.ok(env.channel.edits.some((e) => e.id === sent.id), 'the poll message was edited to its final state');
  await assert.rejects(store.actions.end(guildId, item.id, {}, ctx), (e) => e.code === 'failed' && /already ended/.test(e.message));
  await assert.rejects(store.actions.end(guildId, 'cnotthere00000000000000000', {}, ctx), (e) => e.code === 'not_found');
  await assert.rejects(store.actions.end(guildOther(), open.id, {}, ctx), (e) => e.code === 'not_found');

  // Delete: an active poll ends silently (no results post), its message and the record (with the votes) go.
  const sentBefore = env.channel.sent.length;
  const openMessage = (await prisma.poll.findUnique({ where: { id: open.id } })).messageId;
  await store.remove(guildId, open.id, ctx);
  assert.equal(await prisma.poll.findUnique({ where: { id: open.id } }), null);
  assert.equal(env.channel.sent.length, sentBefore, 'no results were posted');
  assert.ok(!env.channel.store.has(openMessage), 'its message was deleted');
  await store.remove(guildId, item.id, ctx);
  assert.equal(await prisma.pollVote.count({ where: { pollId: item.id } }), 0);
  await assert.rejects(store.remove(guildId, item.id, ctx), (e) => e.code === 'not_found');
  await assert.rejects(store.remove(guildOther(), open.id, ctx), (e) => e.code === 'not_found');

  // The statement behind the vote counts runs.
  for (const stmt of RAW_SQL) await prisma.$queryRaw(stmt.build());
});

test('polls store: the cap, a module that is off, a missing channel and a failed post', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/polls.store.js');
  const env = await dbEnv(4);
  const { guildId, ctx } = env;
  const input = create('polls', { question: 'One too many', options: ['Yes', 'No'], channelId: env.channel.id, duration: '1h' });
  await prisma.poll.createMany({
    data: Array.from({ length: LIMITS.ACTIVE_POLLS }, (_, i) => ({
      guildId, channelId: env.channel.id, authorId: HOST, question: `P${i}`, options: ['A', 'B'], maxChoices: 1, status: 'ACTIVE',
    })),
  });
  await assert.rejects(store.create(guildId, input, ctx), (e) => e.code === 'failed' && /already has 50 active polls/.test(e.message));
  assert.equal(await prisma.poll.count({ where: { guildId } }), LIMITS.ACTIVE_POLLS);
  assert.equal((await store.list(guildId)).items.length, LIMITS.ACTIVE_POLLS);
  await prisma.poll.deleteMany({ where: { guildId } });

  await updateSettings(guildId, { pollsEnabled: false });
  await assert.rejects(store.create(guildId, input, ctx), (e) => e.code === 'failed' && /module is turned off/.test(e.message));
  await updateSettings(guildId, { pollsEnabled: true });
  await assert.rejects(store.create(guildId, { ...input, channelId: sid('4', 999) }, ctx), (e) => e.code === 'failed' && /channel no longer exists/.test(e.message));
  env.channel.failSend = discordErr(50013);
  await assert.rejects(store.create(guildId, input, ctx), (e) => e.code === 'failed' && /couldn't post the poll/.test(e.message));
  assert.equal(await prisma.poll.count({ where: { guildId } }), 0, 'a failed post leaves no row');
  env.channel.failSend = null;

  const offline = { ...ctx, runTask: async () => ({ status: 'offline', message: 'The bot did not pick this up in time.', data: null }) };
  await assert.rejects(store.create(guildId, input, offline), (e) => e.code === 'unavailable');
  const lost = { ...ctx, runTask: async () => ({ status: 'pending', message: 'Still working.', data: null }) };
  await assert.rejects(store.create(guildId, input, lost), (e) => e.code === 'unavailable' && /Still working/.test(e.message));
  const noData = { ...ctx, runTask: async (kind, payload) => { const r = await ctx.runTask(kind, payload); return { ...r, data: null }; } };
  assert.equal((await store.create(guildId, { ...input, question: 'No data' }, noData)).question, 'No data');
});
