// The Personal Info automod rule: what counts as a phone number, street address, email, SSN or card number, what
// never does, and that logs blank the details out.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const pi = await import('../src/utils/personalInfo.js');
const { findPersonalInfo, redactPersonalInfo, describePersonalInfo, PERSONAL_INFO_DELETE_REASON } = pi;
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};
after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

/** Inputs that must be caught, with the kinds expected. */
const PERSONAL_INFO_HITS = [
  ['call me at 555-123-4567', ['phone']],
  ['my number is (555) 123 4567', ['phone']],
  ['+1 (555) 123-4567', ['phone']],
  ['1-555-234-5678', ['phone']],
  ['+44 7700 900123', ['phone']],
  ['07700 900123', ['phone']],
  ['06 12 34 56 78', ['phone']],
  ['0412 345 678', ['phone']],
  ['5552345678', ['phone']],
  ['phone:555.234.5678', ['phone']],
  ['5​5​5-123-4567', ['phone']],
  ['\u{FF15}\u{FF15}\u{FF15}-123-4567', ['phone']],
  ['I live at 123 Main St', ['address']],
  ['4521 n main st apt 3', ['address']],
  ['12 Oak Street, Springfield', ['address']],
  ['he lives at 742 Evergreen Terrace', ['address']],
  ['10 Downing Street', ['address']],
  ['1600 Pennsylvania Avenue NW', ['address']],
  ['221b Baker St.', ['address']],
  ['350 W 21st St', ['address']],
  ['email me john.doe@gmail.com', ['email']],
  ['me@x.io', ['email']],
  ['ssn 123-45-6789', ['ssn']],
  ['card 4111 1111 1111 1111', ['card']],
  ['4111111111111111', ['card']],
  ['3782-822463-10005', ['card']],
  ['dox: 742 Evergreen Terrace, 555-123-4567, homer@springfield.com', ['phone', 'address', 'email']],
];

/** Everyday messages that must never be caught. */
const PERSONAL_INFO_MISSES = [
  'I have 3 cats in the way', '5 miles down the road', '2 hard drive', 'I bought 2 SSD drive', 'I ran 5 laps around the court',
  '2 people way too loud', 'I need 2 more st', '2 days ago dr', 'room 12 then lane', 'top 5 drives', '1 Way',
  'score 1234567890', 'id 123456789012345678', '<@123456789012345678>', '<#123456789012345678>', '<t:1764200000:R>',
  'use <:pog:123456789012345678>', 'https://x.com/5552345678', 'https://example.com/a@b.com',
  '2024-11-26', '01.02.2024', '26/11/2024', '10:30 pm', '$2,345,678,901', '1,000,000', '1000000000', 'I scored 9001 points',
  'call 911', 'its 3 am', 'version 0.12.34', 'v1.2.3.4', 'play.server.net 192.168.1.10', 'join 51.79.12.34:25565',
  'my cpu is 3.5 ghz', '4111 1111 1111 1112', 'order #5552345678', '999-99-9999', '000-12-3456',
  '@everyone hi', '@someone.else', 'user@localhost', 'a@b', '',
];

test('findPersonalInfo catches each kind', () => {
  for (const [text, kinds] of PERSONAL_INFO_HITS) assert.deepEqual(findPersonalInfo(text), kinds, text);
});

test('findPersonalInfo leaves everyday numbers, ids, dates, links and game-server IPs alone', () => {
  for (const text of PERSONAL_INFO_MISSES) assert.deepEqual(findPersonalInfo(text), [], text);
  assert.deepEqual(findPersonalInfo(null), []);
  assert.deepEqual(findPersonalInfo(undefined), []);
});

test('redactPersonalInfo blanks every hit and leaves clean text untouched', () => {
  assert.equal(redactPersonalInfo('dox: 742 Evergreen Terrace, 555-123-4567, homer@springfield.com'),
    'dox: [street address], [phone number], [email address]');
  assert.equal(redactPersonalInfo('card 4111 1111 1111 1111 ok'), 'card [card number] ok');
  assert.equal(redactPersonalInfo('see <@123456789012345678> at 5552345678'), 'see <@123456789012345678> at [phone number]');
  for (const text of PERSONAL_INFO_MISSES) assert.equal(redactPersonalInfo(text), text, text);
});

test('describePersonalInfo names kinds, never values', () => {
  assert.equal(describePersonalInfo(['phone', 'email']), 'phone number, email address');
  assert.equal(PERSONAL_INFO_DELETE_REASON, 'automod: personal info');
});

test('the detector stays fast on hostile input', () => {
  const inputs = ['+1' + '2 '.repeat(4000), '0' + '1'.repeat(8000), `1 ${'abc '.repeat(2000)}`, `${'a'.repeat(60)}@`.repeat(130),
    `${'9'.repeat(30)}-`.repeat(260), `+123${'4'.repeat(30)}`.repeat(230), '12 '.repeat(3000) + 'St'];
  for (const s of inputs) {
    const t = Date.now();
    findPersonalInfo(s);
    redactPersonalInfo(s);
    assert.ok(Date.now() - t < 500, `slow on ${s.slice(0, 20)}…`);
  }
});

test('the automod rule: registered, evaluated, and its logs redact', async () => {
  const am = await import('../src/services/automodService.js');
  assert.ok(am.RULE_KEYS.includes('personalInfo'));
  assert.equal(am.RULES.personalInfo.label, 'Personal Info');
  assert.equal(am.resolveRuleKey('doxxing'), 'personalInfo');
  assert.equal(am.resolveRuleKey('Personal Info'), 'personalInfo');
  const config = am.normalizeConfig({ version: 2, rules: { personalInfo: { action: 'delete_warn' } } });
  assert.equal(config.rules.personalInfo.action, 'delete_warn');
  assert.equal(am.normalizeConfig(null).rules.personalInfo.action, 'off');
  const matches = am.evaluateContentRules({ content: 'text me 555-123-4567' }, am.activeRuleKeys(config), config, am.buildMatchers(config), null);
  assert.deepEqual(matches.map((m) => [m.rule, m.action, m.detail]), [['personalInfo', 'delete_warn', 'phone number']]);
  assert.deepEqual(am.evaluateContentRules({ content: 'see you at 10:30' }, ['personalInfo'], config, am.buildMatchers(config), null), []);
  const outcome = am.resolveOutcome(matches);
  assert.equal(am.outcomeHasPersonalInfo(outcome), true);
  assert.equal(am.logContent(outcome, 'text me 555-123-4567'), 'text me [phone number]');
  assert.equal(am.logContent(am.resolveOutcome([{ rule: 'caps', action: 'delete' }]), 'CALL 555-123-4567'), 'CALL 555-123-4567');
  assert.match(am.buildReason(outcome, 'general'), /^\[Automod\] Personal Info: phone number in #general$/);
});

test('the message-delete log blanks personal info out of a Personal Info automod delete only', async () => {
  const ev = await import('../src/services/logEvents.js');
  const message = {
    id: '800000000000000001', partial: false, content: 'call 555-123-4567', author: { id: '1', username: 'u', tag: 'u#0' },
    channelId: '700000000000000001', attachments: new Map(), embeds: [], mentions: { users: new Map(), roles: new Map(), everyone: false },
  };
  const redacted = ev.buildMessageDeleteEmbed({ message, botReason: PERSONAL_INFO_DELETE_REASON, botId: '9' }).toJSON();
  assert.equal(redacted.description, 'call [phone number]');
  assert.equal(redacted.fields.find((f) => f.name === 'Deleted By').value, '<@9> (automod: personal info)');
  assert.equal(ev.buildMessageDeleteEmbed({ message, botReason: 'automod' }).toJSON().description, 'call 555-123-4567');
  assert.equal(ev.buildMessageDeleteEmbed({ message }).toJSON().description, 'call 555-123-4567');
});
