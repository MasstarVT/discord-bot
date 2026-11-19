// Calls the ported src/utils functions whose JS arguments include functions or discord.js objects (so the recorder
// cannot capture them) with plain stand-ins, and writes rust/fixtures/golden/bot-util/extra.json in the same
// { "key": [[args, result], ...] } shape as tables.json.
// Run from the repo root: node rust/fixtures/gen/bot-util-extra.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { enc } from './bot-util-encode.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'bot-util');
const imp = (p) => import(pathToFileURL(join(ROOT, p)).href);
const text = await imp('src/utils/text.js');
const tzs = await imp('src/utils/timezones.js');
const ef = await imp('src/utils/errorFormatter.js');

const out = {};
const rec = (key, args, result) => (out[key] ??= []).push([args.map((a) => enc(a)), enc(result)]);

// joinWithLimit with a custom `more` (given as a template, {n} is the omitted count).
const ITEMS = [['<@1>', '<@22>', '<@333>', '<@4444>'], Array.from({ length: 30 }, (_, i) => `<@${100000 + i}>`), ['x'.repeat(50)], ['a', 'b'],
  ['😀😀', '😀', '😀😀😀'], ['short', 'x'.repeat(40), 'y']];
for (const items of ITEMS) {
  for (const tpl of ['+{n}', ' and {n} others', '… {n}', '', '(+{n} more, see the full list in the dashboard)']) {
    for (const max of [0, 1, 5, 10, 20, 30, 60, 1024]) {
      for (const sep of [', ', '\n', '']) {
        rec('extra.joinWithLimit', [items, { sep, max }, tpl], text.joinWithLimit(items, { sep, max, more: (n) => tpl.replace('{n}', n) }));
      }
    }
  }
}

// effectiveText / forwardedSnapshots with plain message stand-ins: [content, snapshot contents or null].
for (const [content, snaps] of [
  ['hello there', null], ['', null], ['', ['join discord.gg/dummyinvite']], ['look', ['a']], ['', ['', 'b']], [null, null], [undefined, null],
  [42, null], ['', ['from array']], ['x', ['from map']], ['own', []], ['own', [null, undefined, null]], ['a', ['b', 'c', '', 'd']],
  ['line\nbreak', ['snap\nshot']],
]) {
  const message = { content, messageSnapshots: snaps === null ? undefined : snaps.map((c) => (c === null || c === undefined ? null : { content: c })) };
  rec('extra.effectiveText', [content, snaps], text.effectiveText(message));
}

// resolveTimezone with role positions given as a map.
const R1 = '111111111111111111';
const R2 = '222222222222222222';
const R3 = '333333333333333333';
const settingsList = [
  { timezone: 'Europe/Paris', timezoneRoles: [{ roleId: R1, zone: 'America/Chicago' }, { roleId: R2, zone: 'Asia/Tokyo' }] },
  { timezone: 'bogus' }, null, { timezone: 'UTC', timezoneRoles: 'nope' },
  { timezone: 'Europe/Paris', timezoneRoles: [{ roleId: R1, zone: 'Bad/Zone' }, { roleId: R2, zone: 'Asia/Tokyo' }, { roleId: R2, zone: 'UTC' }] },
];
for (const settings of settingsList) {
  for (const personal of [null, 'Europe/London', 'bogus', '']) {
    for (const memberRoleIds of [[], [R1], [R1, R2], [R3], [R2, R1]]) {
      for (const positions of [{}, { [R2]: 5, [R1]: 1 }, { [R1]: 9, [R2]: 2 }, { [R1]: 3, [R2]: 3 }]) {
        const rolePosition = (id) => positions[id] ?? 0;
        rec('extra.resolveTimezone', [{ personal, memberRoleIds, positions, settings }], tzs.resolveTimezone({ personal, memberRoleIds, rolePosition, settings }));
      }
    }
  }
}
rec('extra.resolveTimezone', [{}], tzs.resolveTimezone());

// formatError of an Error (stack left out: it is runtime-specific) and of other values.
for (const [message, code, context] of [['boom', 'E_X', { guildId: '1' }], ['x', undefined, {}], ['x', null, { message: 'override' }],
  ['y', 42, { code: 'ctx', name: 'N' }]]) {
  const err = new Error(message);
  if (code !== undefined) err.code = code;
  const { stack, ...rest } = ef.formatError(err, context);
  rec('extra.formatError', [message, code, context], { ...rest, hasStack: typeof stack === 'string' });
}
for (const [value, context] of [['plain', { a: 1 }], [42, {}], [null, { b: 2 }], [undefined, {}]]) {
  rec('extra.formatNonError', [value, context], ef.formatError(value, context));
}

mkdirSync(OUT, { recursive: true });
writeFileSync(join(OUT, 'extra.json'), `${JSON.stringify(out)}\n`);
for (const [k, v] of Object.entries(out)) process.stdout.write(`${k}: ${v.length}\n`);
