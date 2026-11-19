// Reference data for feat-automations, produced by the JS bot's own automationService (no servers needed).
// Writes rust/fixtures/golden/feat-automations/pure.json. Run from the repository root:
//   node rust/fixtures/gen/feat-automations-golden.mjs
//
// Inputs that are JSON are kept as JSON text (`in`), so the Rust side parses exactly the same text (`3.0` stays a float
// there, as it would arrive from a request body).
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

const svc = await imp('src', 'services', 'automationService.js');
const { buildCustomId } = await imp('src', 'config', 'customIds.js');
const { TimeoutError } = await imp('src', 'utils', 'async.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const attempt = (fn) => {
  try {
    return { ok: fn() };
  } catch (err) {
    return { err: err.message, code: err.code ?? null };
  }
};
const ID = (n) => String(100000000000000000n + BigInt(n));

// ── Names, ids, words ──
const nameInputs = ['My Rule!', '  Welcome  Bot ', '🎉', '--x--', 'a'.repeat(40), 'Ünïcode Rule', 'a_b c', '', '   ', 'x--y', 'ﬁle', `${'b'.repeat(31)}-c`, 'Rules Panel', 'İstanbul'];
const names = nameInputs.map((s) => [s, svc.normalizeAutomationName(s)]);
const typeInputs = [' gained-role ', 'Gained role', 'GAINED_ROLE', 'has not any role', 'send  message', '', 'x-y_z'];
const types = typeInputs.map((s) => [s, svc.normalizeTypeInput(s)]);
const idTexts = [
  ['<#111111111111111111> 222222222222222222', 'channel'], ['<@&333333333333333333>', 'role'], ['<@&333333333333333333>', 'user'],
  ['<@!444444444444444444> <@444444444444444444>', 'user'], ['junk 12', 'user'], ['1234567890123456789012', 'channel'],
  ['<#111111111111111111><#111111111111111111>,333333333333333333', 'channel'], ['<@&333333333333333333> <#111111111111111111> 555555555555555555', 'role'],
  ['a12345678901234567b', 'user'], ['<@12345678901234567>', 'user'],
];
const idLists = idTexts.map(([t, k]) => [t, k, svc.parseIdList(t, k)]);
const wordTexts = ['Hi, hello ,, HI', ' , ', 'a, B,,a', 'İ, i', 'Straße,STRASSE', 'x'];
const words = wordTexts.map((t) => [t, svc.parseWords(t)]);
const snowflakes = ['111111111111111111', '1234567890123456', '12345678901234567890', '123456789012345678901', ' 111111111111111111', 'abc'].map((s) => [s, svc.isSnowflake(s)]);

// ── Emoji ──
const emojiInputs = ['⭐', '⭐⭐', '❤️', '❤', '1️⃣', '1⃣', '1', '#️⃣', '🇺🇸', '🇺', '👍🏽', '👨‍👩‍👧', '<:pog:123456789012345678>', '<a:xx:123456789012345678>', ':pog:', ':nope:', '123456789012345678', 'abc', '', ' ⭐ ', '©', '©️', '😀😀', '🏳️‍🌈', '™', 'a'.repeat(33), '👨‍👩‍👧‍👦', '🫠'];
const resolver = (x) => (x === 'pog' || x === '123456789012345678' ? { id: '123456789012345678', name: 'pog', animated: false } : null);
const emoji = emojiInputs.map((s) => ({ in: s, stored: svc.isStoredEmoji(s), key: svc.emojiKeyOf(s), parsed: svc.parseEmojiInput(s, resolver) }));

// ── Attachments ──
const attachmentInputs = [
  { name: 'a.PNG', contentType: null }, { name: 'voice-message.ogg', contentType: 'audio/ogg' }, { name: 'x.bin', contentType: 'video/mp4' },
  { name: 'notes.txt', contentType: 'text/plain; charset=utf-8' }, { name: 'a.zip', contentType: 'application/zip' }, { name: 'clip.mp4', contentType: 'image/png' },
  { name: 'data.json', contentType: 'application/json' }, { name: 'x', contentType: null }, { name: 'SONG.FLAC' }, { name: 'a.webm', contentType: '' },
  { name: null, contentType: 'IMAGE/JPEG' }, { name: 'a.tar.gz' }, { name: 'README.md' },
];
const attachments = attachmentInputs.map((a) => [a, svc.attachmentKind(a)]);

// ── Normalisers ──
const r1 = ID(1);
const conditionInputs = [
  { type: 'CHANNEL_IN', channelIds: [r1] }, { type: 'CHANNEL_NOT_IN', channelIds: [r1] }, { type: 'CATEGORY_IN', categoryIds: [r1] },
  { type: 'CATEGORY_NOT_IN', categoryIds: [r1] }, { type: 'CONTAINS', words: ['hi'] }, { type: 'EXACT', texts: ['hello'] }, { type: 'NOT_CONTAINS', words: ['x'] },
  { type: 'MATCHES_REGEX', pattern: '^hi', flags: 'i' }, { type: 'ATTACHMENT_AUDIO' }, { type: 'ATTACHMENT_IMAGE' }, { type: 'ATTACHMENT_VIDEO' },
  { type: 'ATTACHMENT_TEXT' }, { type: 'REACTED_WITH', emojis: ['⭐'] }, { type: 'HAS_REACTIONS', min: 3, emoji: '⭐' }, { type: 'HAS_REACTIONS', min: 3, emoji: null },
  { type: 'HAS_REACTIONS', min: 3 }, { type: 'IS_REPLY' }, { type: 'IS_NOT_REPLY' }, { type: 'HAS_ANY_ROLE', roleIds: [r1] }, { type: 'HAS_ALL_ROLES', roleIds: [r1] },
  { type: 'HAS_NOT_ANY_ROLE', roleIds: [r1] }, { type: 'HAS_NOT_ALL_ROLES', roleIds: [r1] }, { type: 'GAINED_ROLE', roleIds: [r1] }, { type: 'LOST_ROLE', roleIds: [r1] },
  { type: 'USER_IS', userIds: [r1] }, { type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefgh', index: 1 }] },
  { type: 'CHANNEL_IN', channelIds: [] }, { type: 'CHANNEL_IN', channelIds: ['abc'] }, { type: 'CHANNEL_IN', channelIds: Array.from({ length: 26 }, (_, i) => ID(i)) },
  { type: 'CHANNEL_IN', channelIds: [r1, r1, ` ${ID(2)} `], extra: 1 }, { type: 'CHANNEL_IN', channelIds: 'x' }, { type: 'CHANNEL_IN' }, { type: 'CHANNEL_IN', channelIds: [12345678901234567] },
  { type: 'CONTAINS', words: ['Hi', 'hi', ' HI '] }, { type: 'CONTAINS', words: [''] }, { type: 'CONTAINS', words: ['x'.repeat(101)] }, { type: 'EXACT', texts: ['x'.repeat(500)] },
  { type: 'EXACT', texts: ['x'.repeat(501)] }, { type: 'CONTAINS', words: [5] }, { type: 'CONTAINS', words: Array.from({ length: 26 }, (_, i) => `w${i}`) },
  { type: 'MATCHES_REGEX', pattern: '(a+)+' }, { type: 'MATCHES_REGEX', pattern: '^hi' }, { type: 'MATCHES_REGEX', pattern: 'a', flags: 'g' },
  { type: 'MATCHES_REGEX', pattern: 'a', flags: 'umsi' }, { type: 'MATCHES_REGEX', pattern: 'a', flags: 'ii' }, { type: 'MATCHES_REGEX', pattern: 'a', flags: '' },
  { type: 'MATCHES_REGEX', pattern: 'a', flags: null }, { type: 'MATCHES_REGEX', pattern: 'a', flags: 'imsui' }, { type: 'MATCHES_REGEX', pattern: '' }, { type: 'MATCHES_REGEX', pattern: '(' },
  { type: 'MATCHES_REGEX', pattern: 5 }, { type: 'MATCHES_REGEX', pattern: '\\1(a)' },
  { type: 'REACTED_WITH', emojis: ['❤️', '❤', '⭐'] }, { type: 'REACTED_WITH', emojis: [] }, { type: 'REACTED_WITH', emojis: ['nope'] }, { type: 'REACTED_WITH', emojis: Array.from({ length: 11 }, () => '⭐') },
  { type: 'REACTED_WITH', emojis: [' ⭐ '] }, { type: 'HAS_REACTIONS', min: 0 }, { type: 'HAS_REACTIONS', min: 1001 }, { type: 'HAS_REACTIONS', min: 1000, emoji: 'bad' },
  { type: 'HAS_REACTIONS', min: '3' }, { type: 'HAS_REACTIONS', min: 2.5 },
  { type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefgh', index: 100 }] }, { type: 'BUTTON_IS', buttons: [{ automationId: 'short', index: 1 }] },
  { type: 'BUTTON_IS', buttons: [{ automationId: ' clxabcdefgh ', index: 2, automationName: '  panel  ' }, { automationId: 'clxabcdefgh', index: 2 }] },
  { type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefgh', index: 3, automationName: 'x'.repeat(80) }] }, { type: 'BUTTON_IS', buttons: [] },
  { type: 'BUTTON_IS', buttons: ['x'] }, { type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefgh', index: 1, automationName: '   ' }] },
  { type: 'NOPE' }, { type: 'constructor' }, { type: '__proto__' }, { type: 'toString' }, { type: 'hasOwnProperty' }, null, [], 'x', 5, {},
  { type: 'USER_IS', userIds: Array.from({ length: 25 }, (_, i) => ID(i)) },
];
const conditionTexts = [
  ...conditionInputs.map((c) => JSON.stringify(c)),
  '{"type":"HAS_REACTIONS","min":3.0,"emoji":null}', '{"type":"HAS_REACTIONS","min":1e2}', '{"type":"BUTTON_IS","buttons":[{"automationId":"clxabcdefgh","index":4.0}]}',
  `{"type":"CHANNEL_IN","channelIds":["${r1}","${r1}"],"extra":1,"__proto__":{"polluted":true}}`, '{"type":"CHANNEL_IN","channelIds":[null]}',
];
const conditions = conditionTexts.map((t) => {
  const raw = JSON.parse(t);
  return { in: t, plain: svc.normalizeCondition(raw), hints: svc.normalizeCondition(raw, { keepHints: true }) };
});

const actionInputs = [
  { type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', url: 'https://x.y', index: 5, style: 'DANGER' } }, { type: 'SEND_MESSAGE', content: '' },
  { type: 'SEND_MESSAGE', content: 'x'.repeat(2001) }, { type: 'SEND_MESSAGE', content: 'x'.repeat(2000) }, { type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', url: null } },
  { type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', url: 'ftp://x' } }, { type: 'SEND_MESSAGE', content: ' hi ', channelId: ` ${ID(3)} ` },
  { type: 'SEND_MESSAGE', content: 'hi', channelId: 'bad' }, { type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', url: '', index: 7 } },
  { type: 'SEND_MESSAGE', content: 'hi', button: { label: '', url: 'https://x.y' } }, { type: 'SEND_MESSAGE', content: 'hi', button: { label: 'x'.repeat(81), index: 1 } },
  { type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', url: `https://x.y/${'a'.repeat(510)}` } }, { type: 'SEND_MESSAGE', content: 'hi', button: [] },
  { type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', index: 0 } }, { type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', index: 99, style: 'SUCCESS' } },
  { type: 'SEND_MESSAGE', content: 'hi', button: { label: 'Go', index: 3, style: null } }, { type: 'SEND_MESSAGE', content: 5 },
  { type: 'REPLY', content: 'hi', button: { label: 'Go', url: null, index: 3 } }, { type: 'REPLY', content: 'hi', button: { label: 'Go', index: 3, style: 'PURPLE' } }, { type: 'REPLY', content: 'hi' },
  { type: 'CREATE_THREAD', name: 'x', autoArchiveMinutes: 30 }, { type: 'CREATE_THREAD' }, { type: 'CREATE_THREAD', name: null, autoArchiveMinutes: null, channelId: null },
  { type: 'CREATE_THREAD', name: '  T  ', autoArchiveMinutes: 10080, channelId: ID(4) }, { type: 'CREATE_THREAD', name: '' }, { type: 'CREATE_THREAD', name: 5 },
  { type: 'CREATE_THREAD', name: 'x'.repeat(101) },
  { type: 'GIVE_COINS', amount: 0 }, { type: 'GIVE_COINS', amount: 1_000_001 }, { type: 'GIVE_COINS', amount: 1_000_000 }, { type: 'GIVE_XP', amount: 1.5 }, { type: 'TAKE_XP', amount: '5' },
  { type: 'GIVE_ROLES', roleIds: Array.from({ length: 11 }, (_, i) => ID(i)) }, { type: 'GIVE_ROLES', roleIds: [ID(1), ID(1)] }, { type: 'TAKE_ROLES', roleIds: [] },
  { type: 'ADD_TO_THREAD', userIds: [] }, { type: 'ADD_TO_THREAD' }, { type: 'ADD_TO_THREAD', userIds: null }, { type: 'ADD_TO_THREAD', userIds: [ID(5)] },
  { type: 'REPOST' }, { type: 'REPOST', channelId: ID(6) }, { type: 'REPOST', channelId: null }, { type: 'REACT', emoji: 'nope' }, { type: 'REACT', emoji: ' ⭐ ' },
  { type: 'REMOVE_REACTION' }, { type: 'REMOVE_REACTION', emoji: null }, { type: 'REMOVE_REACTION', emoji: '<:pog:123456789012345678>' }, { type: 'REMOVE_REACTION', emoji: 'x' },
  { type: 'SEND_EMBED', embedName: 'My Embed' }, { type: 'SEND_EMBED', embedName: '🎉' }, { type: 'SEND_EMBED', embedName: 'e', channelId: ID(7) }, { type: 'SEND_EMBED' },
  { type: 'PIN', extra: true }, { type: 'DELETE' }, { type: 'SEND_IN_THREAD', content: 'x', button: null }, { type: 'SEND_IN_THREAD', content: 'x', button: { label: 'B', index: 2 } },
  { type: 'constructor' }, { type: '__proto__' }, { type: 'toString' }, { type: 'NOPE' }, null, [], {},
  ...Object.keys(svc.ACTION_TYPES).map((t) => ({ type: t, content: 'x', roleIds: [ID(1)], amount: 5, emoji: '⭐', channelId: ID(2), embedName: 'e', userIds: [] })),
];
const actionTexts = [
  ...actionInputs.map((a) => JSON.stringify(a)),
  '{"type":"GIVE_COINS","amount":5.0}', '{"type":"CREATE_THREAD","autoArchiveMinutes":60.0}', '{"type":"SEND_MESSAGE","content":"hi","button":{"label":"Go","index":2.0}}',
  '{"type":"PIN","__proto__":{"admin":true},"constructor":"x"}',
];
const actions = actionTexts.map((t) => ({ in: t, out: svc.normalizeAction(JSON.parse(t)) }));

const buttons = [
  'null', '{"label":"Go","url":"https://x.y"}', '{"label":" Go ","index":4,"style":"DANGER"}', '[]', '{"label":"Go"}', '{"label":"Go","url":"   "}',
  '{"label":"Go","url":"javascript:alert(1)"}', '{"label":"Go","url":5}', '{"label":"Go","index":3,"style":"primary"}',
].map((t) => ({ in: t, out: svc.normalizeButton(JSON.parse(t)) }));

const lists = [
  ['[{"type":"PIN"},{"type":"NOPE"},{"type":"DELETE"}]', 'action'], ['"nope"', 'action'], ['null', 'condition'],
  [JSON.stringify(Array.from({ length: 105 }, () => ({ type: 'PIN' }))), 'action'], ['[{"type":"IS_REPLY"},5,{"type":"CHANNEL_IN","channelIds":[]}]', 'condition'],
].map(([t, kind]) => ({ in: t, kind, out: svc.normalizeList(JSON.parse(t), kind) }));

// ── Shape validation ──
const msg = { type: 'SEND_MESSAGE', content: 'hi' };
const shapeInputs = [
  ['MESSAGE_SENT', [], [msg], 0], ['MEMBER_JOINED', [], [{ type: 'REPLY', content: 'x' }], 0], ['ROLE_GAINED', [], [msg], 0],
  ['ROLE_GAINED', [], [{ ...msg, channelId: ID(1) }], 0], ['MEMBER_JOINED', [], [{ type: 'SEND_IN_THREAD', content: 'x' }], 0],
  ['MEMBER_JOINED', [], [{ type: 'CREATE_THREAD', channelId: ID(1) }, { type: 'SEND_IN_THREAD', content: 'x' }], 0], ['MEMBER_JOINED', [], [{ type: 'CREATE_THREAD' }], 0],
  ['BUTTON_CLICKED', [{ type: 'CONTAINS', words: ['a'] }], [msg], 0],
  ['MESSAGE_SENT', [], [{ type: 'SEND_MESSAGE', content: 'a', button: { label: 'A', url: null, index: 1 } }, { type: 'SEND_MESSAGE', content: 'b', button: { label: 'B', url: null, index: 1 } }], 0],
  ['MESSAGE_SENT', [{ type: 'IS_REPLY' }, { type: 'IS_NOT_REPLY' }], [msg], 0], ['MESSAGE_SENT', [{ type: 'IS_REPLY' }], [msg], 0],
  ['MESSAGE_SENT', [{ type: 'ATTACHMENT_IMAGE' }, { type: 'ATTACHMENT_IMAGE' }], [msg], 0], ['MESSAGE_SENT', [], Array.from({ length: 11 }, () => msg), 0],
  ['MESSAGE_SENT', Array.from({ length: 11 }, () => ({ type: 'CHANNEL_IN', channelIds: [ID(1)] })), [msg], 0],
  ['MESSAGE_SENT', [], [msg], 604_801], ['MESSAGE_SENT', [], [msg], -1], ['MESSAGE_SENT', [], [msg], 1.5], ['MESSAGE_SENT', [], [msg], null],
  ['MESSAGE_SENT', [{ type: 'HAS_REACTIONS', min: 1 }], [msg], 0], ['NOPE', [], [], 0], ['constructor', [], [], 0],
  ['REACTION_ADDED', [], [{ type: 'ADD_TO_THREAD', userIds: [] }, { type: 'REMOVE_REACTION', emoji: null }, { type: 'PIN' }], 0],
  ['VOICE_JOINED', [{ type: 'REACTED_WITH', emojis: ['⭐'] }], [{ type: 'REPOST', channelId: ID(1) }, { type: 'SEND_EMBED', embedName: 'x' }, { type: 'CREATE_THREAD', channelId: ID(2) }, { type: 'ADD_TO_THREAD', userIds: [] }], 0],
  ['THREAD_CREATED', [], [{ type: 'SEND_IN_THREAD', content: 'x' }, { type: 'SEND_MESSAGE', content: 'x', button: { label: 'A', url: 'https://a.b', index: null } }], 0],
];
const shapes = shapeInputs.map(([trigger, c, a, cooldownSeconds]) => {
  const conds = JSON.stringify(svc.normalizeList(c, 'condition').items);
  const acts = JSON.stringify(svc.normalizeList(a, 'action').items);
  return {
    trigger, conditions: conds, actions: acts, cooldownSeconds: JSON.stringify(cooldownSeconds),
    out: svc.validateAutomationShape({ trigger, conditions: JSON.parse(conds), actions: JSON.parse(acts), cooldownSeconds }),
  };
});

// ── Buttons, add / append / remove ──
const btn = (index) => ({ type: 'SEND_MESSAGE', content: 'x', channelId: null, button: { label: 'B', url: null, style: 'PRIMARY', index } });
const linkAct = { type: 'REPLY', content: 'x', button: { label: 'L', url: 'https://x.y', style: 'PRIMARY', index: null } };
const buttonSeqs = [
  [[], 0], [[btn(1), btn(4), linkAct], 0], [[btn(99)], 0], [[btn(3)], 7], [[], 98], [[], 99], [[btn(2)], null],
].map(([acts, seq]) => ({ actions: acts, buttonSeq: seq, nextIndex: svc.nextButtonIndex(acts), nextSeq: svc.nextButtonSeq({ actions: acts, buttonSeq: seq }) }));
const newBtn = (label) => ({ type: 'SEND_MESSAGE', content: 'x', button: { label, url: null, style: 'PRIMARY', index: null } });
const appendInputs = [
  [{ actions: [], buttonSeq: 0 }, newBtn('B')], [{ actions: [btn(1)], buttonSeq: 1 }, newBtn('C')], [{ actions: [], buttonSeq: 98 }, newBtn('B')],
  [{ actions: [], buttonSeq: 99 }, newBtn('B')], [{ actions: [], buttonSeq: 5 }, { type: 'SEND_MESSAGE', content: 'x', button: { label: 'L', url: 'https://x.y' } }],
  [{ actions: Array.from({ length: 10 }, () => ({ type: 'PIN' })), buttonSeq: 0 }, { type: 'PIN' }], [{ actions: [] }, { type: 'SEND_MESSAGE', content: '' }],
  [{ actions: [btn(5)], buttonSeq: 2 }, { type: 'SEND_MESSAGE', content: 'x', button: { label: 'E', url: '', style: 'DANGER' } }],
  [{}, { type: 'PIN' }],
];
const appends = appendInputs.map(([row, action]) => ({ row, action, out: attempt(() => svc.appendAction(row, action)) }));
const roles = (...n) => n.map((i) => ID(i));
const addInputs = [
  [{ trigger: 'MESSAGE_SENT', conditions: [{ type: 'HAS_ANY_ROLE', roleIds: roles(1) }] }, 'HAS_ANY_ROLE', { roleIds: roles(1, 2) }],
  [{ trigger: 'MESSAGE_SENT', conditions: [{ type: 'HAS_ANY_ROLE', roleIds: roles(1) }] }, 'HAS_ANY_ROLE', { roleIds: roles(1) }],
  [{ trigger: 'MESSAGE_SENT', conditions: [{ type: 'HAS_ANY_ROLE', roleIds: roles(1) }] }, 'HAS_ALL_ROLES', { roleIds: roles(3) }],
  [{ conditions: Array.from({ length: 10 }, (_, i) => ({ type: 'CHANNEL_IN', channelIds: [ID(i)] })) }, 'HAS_ANY_ROLE', { roleIds: roles(1) }],
  [{ conditions: [{ type: 'HAS_ANY_ROLE', roleIds: Array.from({ length: 25 }, (_, i) => ID(i)) }] }, 'HAS_ANY_ROLE', { roleIds: roles(99) }],
  [{ conditions: [{ type: 'CONTAINS', words: ['Hi'] }] }, 'CONTAINS', { words: ['hi', 'there'] }],
  [{ conditions: [{ type: 'REACTED_WITH', emojis: ['❤️'] }] }, 'REACTED_WITH', { emojis: ['❤', '⭐'] }],
  [{ conditions: [{ type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefgh', index: 1 }] }] }, 'BUTTON_IS', { buttons: [{ automationId: 'clxabcdefgh', index: 1 }, { automationId: 'clxabcdefgh', index: 2 }] }],
  [{ conditions: [{ type: 'MATCHES_REGEX', pattern: 'a', flags: 'i' }] }, 'MATCHES_REGEX', { pattern: 'b', flags: 'm' }],
  [{ conditions: [] }, 'IS_REPLY', {}], [{ conditions: [] }, 'NOPE', {}], [{ conditions: [] }, 'CONTAINS', { words: [] }],
  [{ conditions: [{ type: 'IS_REPLY' }] }, 'IS_REPLY', {}], [{}, 'USER_IS', { userIds: roles(4) }],
];
const adds = addInputs.map(([row, type, params]) => ({ row, type, params, out: attempt(() => svc.addCondition(row, type, params)) }));
const removes = [
  [[{ type: 'PIN' }], 2], [[{ type: 'PIN' }, { type: 'DELETE' }], 1], [[], 1], [[{ type: 'PIN' }], 0], [[{ type: 'PIN' }], 1.5], [null, 1],
].map(([list, index]) => ({ list, index, out: attempt(() => svc.removeAt(list, index)) }));

// ── Evaluator ──
const evalCases = [
  [{ type: 'CHANNEL_IN', channelIds: ['c1'] }, { channelId: 'c1' }], [{ type: 'CHANNEL_IN', channelIds: ['c1'] }, { channelId: 't9', parentId: 'c1' }],
  [{ type: 'CHANNEL_NOT_IN', channelIds: ['c1'] }, { channelId: null }], [{ type: 'CHANNEL_NOT_IN', channelIds: ['c1'] }, { channelId: 't9', parentId: 'c1' }],
  [{ type: 'CHANNEL_NOT_IN', channelIds: ['c1'] }, { channelId: 'c2' }], [{ type: 'CATEGORY_NOT_IN', categoryIds: ['k'] }, { channelId: 'c', categoryId: null }],
  [{ type: 'CATEGORY_IN', categoryIds: ['k'] }, { channelId: 'c', categoryId: null }], [{ type: 'CATEGORY_IN', categoryIds: ['k'] }, { channelId: 'c', categoryId: 'k' }],
  [{ type: 'CATEGORY_NOT_IN', categoryIds: ['k'] }, { channelId: 'c', categoryId: 'k' }], [{ type: 'CATEGORY_NOT_IN', categoryIds: ['k'] }, { channelId: null }],
  [{ type: 'CONTAINS', words: ['cunt'] }, { content: 'Scunthorpe' }], [{ type: 'CONTAINS', words: ['jerk'] }, { content: 'you jerk!' }],
  [{ type: 'CONTAINS', words: ['JERK'] }, { content: 'you jerk!' }], [{ type: 'CONTAINS', words: ['jerk'] }, { content: null }],
  [{ type: 'EXACT', texts: ['Hello'] }, { content: '  hello ' }], [{ type: 'EXACT', texts: ['Hello'] }, { content: 'hello there' }],
  [{ type: 'NOT_CONTAINS', words: ['x'] }, { content: null }], [{ type: 'NOT_CONTAINS', words: ['x'] }, { content: 'abc' }], [{ type: 'NOT_CONTAINS', words: ['x'] }, { content: 'a x b' }],
  [{ type: 'MATCHES_REGEX', pattern: '^\\d+$', flags: 'i' }, { content: '123' }], [{ type: 'MATCHES_REGEX', pattern: '^\\d+$', flags: 'i' }, { content: 'a123' }],
  [{ type: 'MATCHES_REGEX', pattern: '(a+)+', flags: 'i' }, { content: 'aaaa' }], [{ type: 'MATCHES_REGEX', pattern: '^(?!!)', flags: '' }, { content: '!rank' }],
  [{ type: 'MATCHES_REGEX', pattern: '^(?!!)', flags: '' }, { content: '' }], [{ type: 'MATCHES_REGEX', pattern: 'HI', flags: '' }, { content: 'hi' }],
  [{ type: 'ATTACHMENT_IMAGE' }, { attachments: [{ name: 'a.png' }] }], [{ type: 'ATTACHMENT_IMAGE' }, { attachments: [] }], [{ type: 'ATTACHMENT_IMAGE' }, { attachments: null }],
  [{ type: 'ATTACHMENT_AUDIO' }, { attachments: [{ name: 'a.png' }, { name: 'b', contentType: 'audio/mpeg' }] }], [{ type: 'ATTACHMENT_TEXT' }, { attachments: [{ name: 'log.LOG' }] }],
  [{ type: 'REACTED_WITH', emojis: ['❤️'] }, { reactionKey: '❤' }], [{ type: 'REACTED_WITH', emojis: ['<:pog:123456789012345678>'] }, { reactionKey: '123456789012345678' }],
  [{ type: 'REACTED_WITH', emojis: ['⭐'] }, { reactionKey: null }],
  [{ type: 'HAS_REACTIONS', min: 3, emoji: '⭐' }, { reactionCounts: { '⭐': 2, '🌟': 5 } }], [{ type: 'HAS_REACTIONS', min: 3, emoji: null }, { reactionCounts: { '⭐': 2, '🌟': 5 } }],
  [{ type: 'HAS_REACTIONS', min: 1, emoji: null }, { reactionCounts: null }], [{ type: 'HAS_REACTIONS', min: 2, emoji: '❤️' }, { reactionCounts: { '❤': 2 } }],
  [{ type: 'HAS_REACTIONS', min: 1, emoji: null }, { reactionCounts: {} }],
  [{ type: 'IS_REPLY' }, { isReply: true }], [{ type: 'IS_REPLY' }, { isReply: null }], [{ type: 'IS_NOT_REPLY' }, { isReply: false }], [{ type: 'IS_NOT_REPLY' }, { isReply: null }],
  [{ type: 'GAINED_ROLE', roleIds: ['a'] }, { changedRoleIds: ['a', 'z'] }], [{ type: 'LOST_ROLE', roleIds: ['a'] }, { changedRoleIds: null }],
  [{ type: 'USER_IS', userIds: ['u'] }, { userId: 'u' }], [{ type: 'USER_IS', userIds: ['u'] }, { userId: null }],
  [{ type: 'BUTTON_IS', buttons: [{ automationId: 'a', index: 1 }] }, { button: { automationId: 'a', index: 1 } }],
  [{ type: 'BUTTON_IS', buttons: [{ automationId: 'a', index: 1 }] }, { button: { automationId: 'a', index: 2 } }], [{ type: 'BUTTON_IS', buttons: [{ automationId: 'a', index: 1 }] }, { button: null }],
  [{ type: 'NOPE' }, {}], [{ type: 'constructor' }, {}], [null, {}],
];
for (const type of ['HAS_ANY_ROLE', 'HAS_ALL_ROLES', 'HAS_NOT_ANY_ROLE', 'HAS_NOT_ALL_ROLES']) {
  for (const roleIds of [['a'], ['a', 'c'], ['c']]) {
    evalCases.push([{ type, roleIds }, { memberRoleIds: ['a', 'b'] }], [{ type, roleIds }, { memberRoleIds: null }]);
  }
}
const evaluate = evalCases.map(([cond, facts]) => ({ cond, facts, out: svc.evaluateCondition(cond, facts) }));
const evalAll = [
  [[], {}], [[{ type: 'USER_IS', userIds: ['u'] }, { type: 'NOPE' }], { userId: 'u' }], [[{ type: 'USER_IS', userIds: ['u'] }], { userId: 'u' }],
].map(([conds, facts]) => ({ conds, facts, out: svc.evaluateConditions(conds, facts) }));
const split = svc.splitConditions([
  { type: 'CONTAINS', words: ['a'] }, { type: 'HAS_REACTIONS', min: 1 }, { type: 'ATTACHMENT_IMAGE' },
  { type: 'REACTED_WITH', emojis: ['⭐'] }, { type: 'CHANNEL_IN', channelIds: ['c'] }, { type: 'HAS_ANY_ROLE', roleIds: ['r'] },
]);
const diffs = [
  [['g', 'a', 'b'], ['g', 'b', 'c'], 'g'], [['a'], ['a', 'g'], 'g'], [[], ['x', 'y'], 'g'], [['x', 'x'], [], 'g'],
].map(([a, b, g]) => ({ old: a, new: b, guild: g, out: svc.roleDiff(a, b, g) }));
const voice = ['join', 'leave', 'move', 'state', 'x'].map((c) => [c, svc.voiceTriggers(c)]);

// ── Presets ──
const ch = ID(10);
const presetInputs = [
  ['welcomer', { channelId: ch }], ['welcomer', { channelId: ch, roleId: ID(11), message: 'Hi {user}' }], ['starboard', { channelId: ch }],
  ['starboard', { channelId: ch, emoji: '<:pog:123456789012345678>', threshold: 5 }], ['thread-joiner', { userId: ID(12) }], ['thread-joiner', { userId: ID(12), channelId: ch }],
  ['commands-only', { channelId: ch, prefix: '!', exemptRoleIds: [ID(11), ID(12), ID(11)] }], ['commands-only', { channelId: ch, prefix: '?.', exemptRoleIds: [] }],
  ['commands-only', { channelId: ch, prefix: null }], ['commands-only', { channelId: ch, prefix: '!', exemptRoleIds: Array.from({ length: 30 }, (_, i) => ID(100 + i)) }],
  ['commands-only', { prefix: '!' }], ['nope', {}], ['welcomer', {}], ['starboard', { channelId: ch, threshold: null, emoji: null }], ['welcomer', { channelId: ch, message: '' }],
  ['thread-joiner', {}], ['commands-only', { channelId: ch, prefix: '$(^)' }],
];
const presets = presetInputs.map(([p, o]) => ({ preset: p, opts: o, out: attempt(() => svc.buildPresetAutomation(p, o)) }));
const patterns = ['!', '?.', '', null, '$(^)', '[x]', 'a\\b', '-'].map((p) => [p, svc.commandsOnlyPattern(p)]);

// ── JSON export / import ──
const c22 = ID(22);
const exportRows = [
  {
    id: 'clxfirstrow01', name: 'first', trigger: 'REACTION_ADDED', enabled: true, cooldownSeconds: 30,
    conditions: [{ type: 'REACTED_WITH', emojis: ['⭐', '<:pog:123456789012345678>'] }, { type: 'HAS_REACTIONS', min: 2, emoji: '⭐' }, { type: 'CHANNEL_NOT_IN', channelIds: [c22] }, { type: 'MATCHES_REGEX', pattern: '^\\w+', flags: 'im' }],
    actions: [
      { type: 'CREATE_THREAD', name: 'T {user.name}', channelId: null, autoArchiveMinutes: 60 }, { type: 'SEND_MESSAGE', content: 'Hello {user}', channelId: c22, button: { label: 'Go', url: null, style: 'SUCCESS', index: 1 } },
      { type: 'REPLY', content: 'Thanks', button: null }, { type: 'REPOST', channelId: c22 }, { type: 'PIN' }, { type: 'REACT', emoji: '👍' }, { type: 'REMOVE_REACTION', emoji: null },
      { type: 'GIVE_ROLES', roleIds: [ID(20)] }, { type: 'TAKE_ROLES', roleIds: [ID(20)] }, { type: 'ADD_TO_THREAD', userIds: [ID(21)] },
    ],
  },
  {
    id: 'clxsecondrow', name: 'second', trigger: 'BUTTON_CLICKED', enabled: false, cooldownSeconds: 0,
    conditions: [{ type: 'BUTTON_IS', buttons: [{ automationId: 'clxfirstrow01', index: 1 }, { automationId: 'clxunknown01', index: 2 }] }, { type: 'NOPE' }],
    actions: [
      { type: 'SEND_IN_THREAD', content: 'x', button: null }, { type: 'GIVE_COINS', amount: 5 }, { type: 'TAKE_COINS', amount: 5 }, { type: 'GIVE_XP', amount: 10 },
      { type: 'TAKE_XP', amount: 10 }, { type: 'SEND_EMBED', embedName: 'rules', channelId: c22 }, { type: 'REPLY', content: 'ok', button: null }, { type: 'NOPE' },
    ],
  },
  { name: null, trigger: 'MESSAGE_SENT', cooldownSeconds: 2.5, enabled: 'yes', conditions: 'x', actions: null },
];
const nameMap = { clxsecondrow: 'second', clxfirstrow01: 'first' };
const exportsOut = exportRows.map((row) => ({ row, names: nameMap, json: svc.toExportJson(row, new Map(Object.entries(nameMap).map(([k, v]) => [k, { name: v }]))), jsonNoNames: svc.toExportJson(row) }));

const okJson = { format: 'automation', version: 1, trigger: 'MESSAGE_SENT', conditions: [], actions: [{ type: 'PIN' }] };
const importTexts = [
  JSON.stringify(okJson), ' '.repeat(33_000), JSON.stringify({ ...okJson, version: 2 }), JSON.stringify({ ...okJson, format: 'embed' }), 'nope', '[1,2]',
  JSON.stringify({ ...okJson, trigger: 'X' }), JSON.stringify({ ...okJson, trigger: 'constructor' }), JSON.stringify({ ...okJson, trigger: '__proto__' }),
  JSON.stringify({ ...okJson, actions: [{ type: 'toString' }] }), JSON.stringify({ ...okJson, actions: [{ type: 'PIN' }, { type: 'NOPE' }] }),
  JSON.stringify({ ...okJson, conditions: [{ type: 'CHANNEL_IN', channelIds: [] }] }), JSON.stringify({ ...okJson, actions: Array.from({ length: 11 }, () => ({ type: 'PIN' })) }),
  JSON.stringify({ ...okJson, conditions: Array.from({ length: 11 }, () => ({ type: 'IS_REPLY' })) }), JSON.stringify({ ...okJson, enabled: 'yes' }),
  JSON.stringify({ ...okJson, cooldownSeconds: -1 }), JSON.stringify({ ...okJson, cooldownSeconds: 1.5 }), JSON.stringify({ ...okJson, conditions: 'x' }),
  '{"format":"automation","version":1,"trigger":"MESSAGE_SENT","actions":[{"type":"PIN","__proto__":{"admin":true},"constructor":"x"}]}',
  JSON.stringify({ ...okJson, exportedId: '../../x' }), JSON.stringify({ ...okJson, exportedId: 'clxabcdefgh' }),
  JSON.stringify({ ...okJson, trigger: 'MEMBER_JOINED', actions: [{ type: 'REPLY', content: 'x' }] }), `﻿${JSON.stringify(okJson)}`,
  JSON.stringify({ version: 1, trigger: 'MESSAGE_SENT' }), JSON.stringify({ ...okJson, format: null }), JSON.stringify({ ...okJson, version: '1' }),
  JSON.stringify({ ...okJson, version: 1.0, cooldownSeconds: 604800, enabled: false, name: 'My Import!' }), JSON.stringify({ ...okJson, name: 5, enabled: null, cooldownSeconds: null }),
  JSON.stringify({ ...okJson, conditions: null, actions: null }), JSON.stringify({ ...okJson, trigger: null }), JSON.stringify({ ...okJson, trigger: { a: 1 } }),
  JSON.stringify({ ...okJson, trigger: [1, 2] }), JSON.stringify({ ...okJson, trigger: 'x'.repeat(60) }), JSON.stringify({ format: 'automation', trigger: 'MESSAGE_SENT' }),
  JSON.stringify({ ...okJson, name: '🎉' }), JSON.stringify({ ...okJson, name: '   ' }), JSON.stringify({ ...okJson, exportedId: 5 }),
  '{"format":"automation","version":1,"trigger":"MESSAGE_SENT","cooldownSeconds":30.0,"version":1}',
  ...exportsOut.slice(0, 2).map((e) => e.json), JSON.stringify({ ...okJson, conditions: [{ type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefgh', index: 1, automationName: 'panel' }] }] }),
  JSON.stringify({ ...okJson, trigger: 'BUTTON_CLICKED', conditions: [{ type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefgh', index: 1, automationName: 'panel' }] }], actions: [{ type: 'REPLY', content: 'x' }] }),
  JSON.stringify({ ...okJson, actions: [{ type: 'REPLY', content: '' }, { type: 'REPLY', content: 'ok' }, { type: 'SEND_MESSAGE', content: 'a', button: { label: 'A', index: 1 } }, { type: 'SEND_MESSAGE', content: 'b', button: { label: 'B', index: 1 } }] }),
  JSON.stringify(okJson).padEnd(32_768, ' '), JSON.stringify(okJson).padEnd(32_769, ' '), '"string"', 'null', '{}',
];
const imports = importTexts.map((t) => ({ in: t, out: svc.parseAutomationJson(t) }));

// ── Descriptions ──
const describeConds = [
  { type: 'CHANNEL_IN', channelIds: [ID(1)] }, { type: 'CHANNEL_NOT_IN', channelIds: Array.from({ length: 25 }, (_, i) => ID(i)) }, { type: 'CATEGORY_IN', categoryIds: [ID(1)] },
  { type: 'CATEGORY_NOT_IN', categoryIds: [ID(1), ID(2)] }, { type: 'CONTAINS', words: ['a', 'b c', `multi\nline   ${'w'.repeat(50)}`] }, { type: 'NOT_CONTAINS', words: Array.from({ length: 25 }, (_, i) => `word${i}`) },
  { type: 'EXACT', texts: ['a'] }, { type: 'MATCHES_REGEX', pattern: 'a`b', flags: 'i' }, { type: 'MATCHES_REGEX', pattern: 'x'.repeat(150), flags: '' },
  { type: 'ATTACHMENT_AUDIO' }, { type: 'ATTACHMENT_IMAGE' }, { type: 'ATTACHMENT_VIDEO' }, { type: 'ATTACHMENT_TEXT' }, { type: 'REACTED_WITH', emojis: ['⭐', '<:pog:123456789012345678>'] },
  { type: 'HAS_REACTIONS', min: 1, emoji: null }, { type: 'HAS_REACTIONS', min: 1500, emoji: '⭐' }, { type: 'HAS_REACTIONS', min: 1 }, { type: 'IS_REPLY' }, { type: 'IS_NOT_REPLY' },
  { type: 'HAS_ANY_ROLE', roleIds: [ID(1)] }, { type: 'HAS_ALL_ROLES', roleIds: [ID(1), ID(2)] }, { type: 'HAS_NOT_ANY_ROLE', roleIds: [ID(1)] }, { type: 'HAS_NOT_ALL_ROLES', roleIds: [ID(1)] },
  { type: 'GAINED_ROLE', roleIds: [ID(1)] }, { type: 'LOST_ROLE', roleIds: [ID(1)] }, { type: 'USER_IS', userIds: [ID(1)] },
  { type: 'BUTTON_IS', buttons: [{ automationId: 'clxabcdefghijklmnop', index: 1 }, { automationId: 'a`b`cdefgh', index: 2 }] }, null, { type: 'NOPE' }, { type: 'CHANNEL_IN' },
];
const describeConditions = describeConds.map((c) => [c, svc.describeCondition(c)]);
const describeActs = [
  { type: 'CREATE_THREAD', name: 'n', channelId: ID(1) }, { type: 'CREATE_THREAD', name: `multi\nline ${'n'.repeat(60)}`, channelId: null },
  { type: 'SEND_MESSAGE', content: 'x'.repeat(300), button: { label: 'L', url: null, index: 2 } }, { type: 'SEND_MESSAGE', content: 'hi', channelId: ID(2), button: { label: 'Link\nlabel'.repeat(8), url: 'https://x.y', index: null } },
  { type: 'REPLY', content: 'x', button: null }, { type: 'REPOST', channelId: ID(1) }, { type: 'REPOST' }, { type: 'PIN' }, { type: 'DELETE' }, { type: 'REACT', emoji: '⭐' },
  { type: 'REMOVE_REACTION', emoji: null }, { type: 'REMOVE_REACTION', emoji: '⭐' }, { type: 'GIVE_ROLES', roleIds: [ID(1)] }, { type: 'TAKE_ROLES', roleIds: Array.from({ length: 10 }, (_, i) => ID(i)) },
  { type: 'ADD_TO_THREAD', userIds: [] }, { type: 'ADD_TO_THREAD', userIds: [ID(1), ID(2)] }, { type: 'SEND_IN_THREAD', content: 'x', button: { label: 'B', url: null, index: 3 } },
  { type: 'GIVE_COINS', amount: 5 }, { type: 'TAKE_COINS', amount: 1234567 }, { type: 'GIVE_XP', amount: 5 }, { type: 'TAKE_XP', amount: 1000 },
  { type: 'SEND_EMBED', embedName: 'e', channelId: null }, { type: 'SEND_EMBED', embedName: 'rules', channelId: ID(3) }, null, { type: 'NOPE' }, { type: 'SEND_MESSAGE' },
];
const settingsVariants = [null, { currencyEmoji: '💎', currencyName: 'gems' }, { currencyEmoji: null, currencyName: null }];
const describeActions = describeActs.map((a) => [a, settingsVariants.map((s) => svc.describeAction(a, { settings: s }))]);

// ── Discord errors, constants, keys ──
const errorCodes = [10008, 10007, 10003, 10011, 10014, 50001, 50013, 30003, 30010, 90001, 50083, 160005, 160006, 30033, 50035, 160004, 40001, null];
const discordErrors = errorCodes.map((code) => [code, svc.discordErrorText({ code, message: `boom ${code}` })]);
discordErrors.push(['timeout', svc.discordErrorText(new TimeoutError('x'))]);
discordErrors.push(['long', svc.discordErrorText({ code: 50035, message: 'x'.repeat(300) })]);
discordErrors.push(['nomsg', svc.discordErrorText({})]);

const out = {
  names, types, idLists, words, snowflakes, emoji, attachments, conditions, actions, buttons, lists, shapes, buttonSeqs, appends, adds, removes,
  evaluate, evalAll, split, diffs, voice, presets, patterns, exports: exportsOut, imports, describeConditions, describeActions, discordErrors,
  tables: {
    triggers: svc.TRIGGERS,
    conditionTypes: Object.fromEntries(Object.entries(svc.CONDITION_TYPES).map(([k, d]) => [k, { ...d, triggers: [...d.triggers] }])),
    actionTypes: Object.fromEntries(Object.entries(svc.ACTION_TYPES).map(([k, d]) => [k, { ...d, triggers: [...d.triggers] }])),
    T_CHANNEL: [...svc.T_CHANNEL], T_CONTENT: [...svc.T_CONTENT], T_LIVE: [...svc.T_LIVE], T_COUNTS: [...svc.T_COUNTS], T_THREADY: [...svc.T_THREADY],
    presetOptions: svc.PRESET_OPTIONS, executorChannelNeeds: svc.EXECUTOR_CHANNEL_NEEDS,
  },
  errorTexts: {
    name: svc.ERROR_TEXTS.name, notFound: svc.ERROR_TEXTS.notFound('x'), taken: svc.ERROR_TEXTS.taken('x'), limit: svc.ERROR_TEXTS.limit(),
    maxConditions: svc.ERROR_TEXTS.maxConditions(), maxActions: svc.ERROR_TEXTS.maxActions(), buttonsUsed: svc.ERROR_TEXTS.buttonsUsed, needsActions: svc.ERROR_TEXTS.needsActions,
  },
  rawSql: svc.RAW_SQL.map(({ name, build }) => { const q = build(); return { name, sql: q.sql, values: q.values }; }),
  keys: { automations: svc.automationsCacheKey('955028000000000001') },
  customIds: [buildCustomId('automation', 'clxabcdefgh', '4'), buildCustomId('automation', 'autoone01', String(99))],
  constants: {
    AUTOMATION_NAME_MAX: svc.AUTOMATION_NAME_MAX, IMPORT_MAX_BYTES: svc.IMPORT_MAX_BYTES, MAX_CHAIN_DEPTH: svc.MAX_CHAIN_DEPTH, CAUSE_TTL_MS: svc.CAUSE_TTL_MS,
    RUN_BUDGET_MAX: svc.RUN_BUDGET_MAX, RUN_BUDGET_WINDOW_MS: svc.RUN_BUDGET_WINDOW_MS, MAX_PENDING_RUNS: svc.MAX_PENDING_RUNS, RUN_TIMEOUT_MS: svc.RUN_TIMEOUT_MS,
    MEMO_TTL_MS: svc.MEMO_TTL_MS, CACHE_TTL_SECONDS: svc.CACHE_TTL_SECONDS, REPOST_DEDUPE_TTL_SECONDS: svc.REPOST_DEDUPE_TTL_SECONDS, PROBLEM_TTL_SECONDS: svc.PROBLEM_TTL_SECONDS,
    MAX_COOLDOWN_SECONDS: svc.MAX_COOLDOWN_SECONDS, MAX_REWARD_AMOUNT: svc.MAX_REWARD_AMOUNT, THREAD_ARCHIVE_MINUTES: svc.THREAD_ARCHIVE_MINUTES,
    DEFAULT_THREAD_NAME: svc.DEFAULT_THREAD_NAME, WELCOMER_DEFAULT_TEXT: svc.WELCOMER_DEFAULT_TEXT, PRESETS: svc.PRESETS, MAX_BUTTON_INDEX: svc.MAX_BUTTON_INDEX,
    EXPORT_FORMAT: svc.EXPORT_FORMAT, EXPORT_VERSION: svc.EXPORT_VERSION,
  },
};

const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-automations');
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, 'pure.json'), `${JSON.stringify(out, null, 1)}\n`);
process.stdout.write(`wrote ${join(dir, 'pure.json')}\n`);
redis.disconnect();
process.exit(0);
