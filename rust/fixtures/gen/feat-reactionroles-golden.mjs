// Golden data for feat-reactionroles, produced by the JS bot's own reactionRoleService / roleTemplateSetup /
// roleTemplates / reactionrole command (no servers needed). Writes rust/fixtures/golden/feat-reactionroles/golden.json.
//
//   node rust/fixtures/gen/feat-reactionroles-golden.mjs
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

const rr = await imp('src', 'services', 'reactionRoleService.js');
const setup = await imp('src', 'services', 'roleTemplateSetup.js');
const tpl = await imp('src', 'utils', 'roleTemplates.js');
const cmd = await imp('src', 'commands', 'admin', 'reactionrole.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const out = {};

// ── Emoji ─────────────────────────────────────────────────────────────────────
const named = [
  '⭐', '❤️', '❤', '👍🏽', '👨‍👩‍👧', '🇬🇧', '1️⃣', '#️⃣', '🏴󠁧󠁢󠁥󠁮󠁧󠁿', '🏳️‍🌈', '❤️‍🔥', '🫠', '✅', '🔔', '©️', '🎮',
  'a', '1', '#', 'abc', ':smile:', '100%', '✅✅', '<:x:1>', '', ' ', '  ✅ ', '<:pog:123456789012345678>', '<a:dance:123456789012345678>',
  ':x:', '🇬', '🇬🇧🇫🇷', '👍🏽👍', '*️⃣', '0⃣', '™', '™️', '↔️', '⬆', '🏻', '🧑🏽‍💻', '👩‍❤️‍💋‍👨', '🫶🏿', '🩷', '🩵', '🩶', '🪿', '🫎', '🇪🇺',
  '🏜️', '🖥️', '🟢', '🔴', '📱', '⛔', '🤎', '🧡', '💛', '💚', '💙', '💜', '🤍', '💬', '🌎', '🌴', '🌍', '🌏', '🦘', '📢', '📅', '🎁', '📊', '🎥',
  '\u{FE0F}', '\u{200D}', 'x\u{20E3}', '5\u{FE0F}', '©', '®', '‼️', '〰️', '🀄', '🅰️', '🅰', '🈁', 'Ⓜ️', '⌚', '☺', '☺️',
];
for (const r of tpl.ROLE_TEMPLATES ? Object.values(tpl.ROLE_TEMPLATES) : []) for (const role of r.roles) if (role.emoji) named.push(role.emoji);
const sweep = [];
const ranges = [[0x00a9, 0x00ae], [0x203c, 0x2049], [0x2122, 0x2139], [0x2194, 0x21aa], [0x231a, 0x23ff], [0x24c2, 0x24c2], [0x25aa, 0x27bf],
  [0x2934, 0x2935], [0x2b05, 0x2b55], [0x3030, 0x303d], [0x3297, 0x3299], [0x1f000, 0x1faff]];
for (const [a, b] of ranges) {
  for (let cp = a; cp <= b; cp++) {
    const c = String.fromCodePoint(cp);
    sweep.push(c, c + '\u{FE0F}');
    if (cp >= 0x1f300 && cp % 7 === 0) sweep.push(c + '🏽', c + '\u{200D}' + '🔥');
  }
}
out.emoji = [...new Set(named)].map((s) => ({
  input: s,
  isUnicode: rr.isUnicodeEmoji(s),
  component: rr.componentEmoji(s),
  key: rr.emojiKeyOfStored(s),
  normalized: rr.normalizeEmojiKey(s),
}));
out.emojiSweep = { inputs: sweep, isUnicode: sweep.map((s) => rr.isUnicodeEmoji(s)) };

const party = { id: '555555555555555555', name: 'party', available: true, roles: { cache: new Map() }, toString() { return '<:party:555555555555555555>'; } };
const anim = { id: '555555555555555556', name: 'wave', animated: true, available: true, roles: { cache: new Map() }, toString() { return '<a:wave:555555555555555556>'; } };
const locked = { id: '666666666666666666', name: 'locked', available: true, roles: { cache: new Map([['400000000000000001', {}]]) },
  guild: { members: { me: { roles: { cache: { hasAny: () => false } } } } }, toString() { return '<:locked:666666666666666666>'; } };
const gone = { id: '777777777777777777', name: 'gone', available: false, roles: { cache: new Map() }, toString() { return '<:gone:777777777777777777>'; } };
const guildEmojis = new Map([[party.id, party], [anim.id, anim]]);
guildEmojis.find = function (fn) { for (const v of this.values()) if (fn(v)) return v; return undefined; };
const clientEmojis = new Map([[party.id, party], [anim.id, anim], [locked.id, locked], [gone.id, gone]]);
const emojiCtx = { guild: { emojis: { cache: guildEmojis } }, client: { emojis: { cache: clientEmojis } } };
out.emojiWorld = {
  guild: [party, anim].map((e) => ({ id: e.id, name: e.name, animated: Boolean(e.animated), available: true, limitedForBot: false })),
  client: [party, anim, locked, gone].map((e) => ({ id: e.id, name: e.name, animated: Boolean(e.animated), available: e.available !== false, limitedForBot: e === locked })),
};
const emojiInputs = ['✅', '', null, '✅✅', ':party:', ':PARTY:', ':nope:', 'nope', '<:x:999999999999999999>', '<:gone:777777777777777777>',
  '<:locked:666666666666666666>', '555555555555555555', '555555555555555556', 'hello world', 'x'.repeat(100) + ' y', '  🔔  ', ':wave:',
  '<a:wave:555555555555555556>', '❤️', '❤', 'a-b+c', ':a:', '1️⃣2️⃣', '🇬🇧', 'abc:def'];
out.parseEmoji = emojiInputs.map((input) => ({ input, result: rr.parseEmojiInput(input, emojiCtx) }));

// ── Links, names, errors ─────────────────────────────────────────────────────
const refCtx = { guildId: '111111111111111111', channelId: '333333333333333333' };
const link = (host, tail = '') => `https://${host}/channels/111111111111111111/222222222222222222/444444444444444444${tail}`;
const refs = [link('discord.com'), link('ptb.discord.com'), link('canary.discord.com'), link('discordapp.com'), `<${link('discord.com')}>`,
  link('discord.com', '?x=1'), link('discord.com', '/'), 'https://discord.com/channels/999999999999999999/222222222222222222/444444444444444444',
  '111111111111111111-222222222222222222', '444444444444444444', 'hello', '', 'https://discord.com/channels/@me/222222222222222222/444444444444444444',
  '  444444444444444444  ', 'http://discord.com/channels/111111111111111111/222222222222222222/444444444444444444', '12345'];
out.messageRefs = refs.map((input) => ({ input, ...refCtx, result: rr.parseMessageReference(input, refCtx) }));
const titles = [['\n**Rules** here\nmore', null, 'general'], ['', 'Embedded', 'general'], ['  ', null, 'general'], ['x'.repeat(500), null, 'g'],
  ['> # Big title\r\nbody', null, 'c'], ['***', '', 'chan'], ['`code`', null, 'c']];
out.attachTitles = titles.map(([content, embedTitle, channel]) => ({
  content, embedTitle, channel, result: rr.defaultAttachTitle({ content, embeds: embedTitle === null ? [] : [{ title: embedTitle }] }, channel),
}));
out.panelChoiceNames = [['T'.repeat(256), 'REACTIONS', 3], ['News', 'BUTTONS', 0], ['Drop', 'SELECT', 12], ['Odd', 'GRID', 1]]
  .map(([title, layout, itemCount]) => ({ title, layout, itemCount, result: rr.panelChoiceName({ title, layout, itemCount }) }));
const roleGuild = { roles: { cache: new Map([['400000000000000001', { name: 'News' }]]) } };
out.itemChoiceNames = [['News', '🔔', '400000000000000001'], ['Gone', ':bad:', '400000000000000009'], ['x'.repeat(300), null, '400000000000000001'],
  ['Pog', '<:pog:123456789012345678>', '400000000000000001']]
  .map(([label, emoji, roleId]) => ({ label, emoji, roleName: roleGuild.roles.cache.get(roleId)?.name ?? null, result: rr.itemChoiceName({ label, emoji, roleId }, roleGuild) }));
out.discordErrors = [10003, 10008, 10011, 10014, 30010, 50001, 50005, 50013, 50035, 50083, 12345, null]
  .map((code) => ({ code, text: rr.describeDiscordError(code === null ? null : { code }) }));

// ── Planner ───────────────────────────────────────────────────────────────────
const P = ['A', 'B', 'C'];
const subsets = (arr) => arr.reduce((acc, x) => acc.concat(acc.map((s) => [...s, x])), [[]]);
out.plans = [];
for (const type of [...rr.RR_TYPES, 'ODD']) {
  for (const source of ['button', 'select', 'react', 'unreact']) {
    const targets = source === 'select' ? [...subsets(P), ['Z'], ['A', 'Z']] : [['A'], ['B'], ['Z'], []];
    for (const held of subsets(P)) {
      for (const target of targets) {
        for (const max of [null, 1, 2]) {
          const p = rr.planRoleChange({ type, source, panelRoleIds: P, heldRoleIds: held, targetRoleIds: target, maxRoles: max });
          out.plans.push({ type, source, held, target, max, toAdd: p.toAdd, toRemove: p.toRemove, skipped: p.skipped, reason: p.reason });
        }
      }
    }
  }
}

// ── Payloads ──────────────────────────────────────────────────────────────────
const mkItem = (i, over = {}) => ({
  id: `item${i}`, groupId: 'g1', customId: `rr:g1:40000000000000000${i % 10}${i}`, roleId: `4000000000000000${String(i).padStart(2, '0')}`,
  label: `Role ${i}`, emoji: null, style: 1, position: i, description: null, ...over,
});
const mkGroup = (n, over = {}) => ({
  id: 'g1', title: 'Roles', description: null, type: 'NORMAL', layout: 'BUTTONS', maxRoles: null, plainText: false, messageContent: null,
  placeholder: null, items: Array.from({ length: n }, (_, i) => mkItem(i + 1)), ...over,
});
const emojis = ['🔔', '<:pog:123456789012345678>', ':x:', null, '<a:wave:123456789012345678>', '100%', '❤️'];
const groups = [];
for (const type of rr.RR_TYPES) {
  for (const layout of rr.RR_LAYOUTS) {
    for (const plainText of [false, true]) {
      groups.push(mkGroup(3, { type, layout, plainText, maxRoles: type === 'NORMAL' ? 2 : null,
        items: [1, 2, 3].map((i) => mkItem(i, { emoji: emojis[i], description: i === 2 ? 'About two' : null, style: i })) }));
    }
  }
}
groups.push(mkGroup(7), mkGroup(40), mkGroup(0), mkGroup(0, { plainText: true }), mkGroup(30, { layout: 'SELECT' }),
  mkGroup(2, { messageContent: 'Top text' }), mkGroup(2, { messageContent: 'Hello everyone', plainText: true, layout: 'REACTIONS' }),
  mkGroup(0, { description: 'D'.repeat(4000) }), mkGroup(0, { plainText: true, description: 'D'.repeat(4000), title: 'T'.repeat(400) }),
  mkGroup(2, { description: '', layout: 'REACTIONS' }), mkGroup(2, { title: '' }), mkGroup(2, { layout: 'SELECT', placeholder: 'Pick!' }),
  mkGroup(2, { layout: 'SELECT', placeholder: '' }), mkGroup(5, { layout: 'SELECT', maxRoles: 3 }), mkGroup(5, { layout: 'SELECT', maxRoles: 30 }),
  mkGroup(3, { items: [mkItem(3, { position: 0 }), mkItem(1, { position: 2 }), mkItem(2, { position: 1, style: 9, label: '' })] }),
  mkGroup(2, { items: [mkItem(1, { label: 'L'.repeat(120) }), mkItem(2, { label: 'x', description: 'd'.repeat(150) })], layout: 'SELECT' }),
  mkGroup(2, { type: 'ODD', layout: 'GRID' }), mkGroup(1, { messageContent: 'm'.repeat(2100) }));
const colors = [undefined, 0xff0000, -5, 0x1000000, 0];
out.payloads = groups.map((group, i) => {
  const color = colors[i % colors.length];
  const p = rr.buildPanelPayload(group, { color });
  return { group, color: color ?? null, payload: { content: p.content, embeds: p.embeds.map((e) => e.toJSON()), components: p.components.map((r) => r.toJSON()), allowedMentions: p.allowedMentions } };
});
const selGroup = mkGroup(3, { layout: 'SELECT' });
out.selectRows = [
  { group: selGroup, selected: ['400000000000000001'], placeholder: 'Adjust' },
  { group: mkGroup(3, { layout: 'SELECT', maxRoles: 1 }), selected: ['400000000000000001', '400000000000000002'], placeholder: null },
  { group: mkGroup(3, { layout: 'SELECT', type: 'UNIQUE' }), selected: ['400000000000000003', '400000000000000002'], placeholder: null },
  { group: mkGroup(0, { layout: 'SELECT' }), selected: null, placeholder: null },
  { group: mkGroup(3, { layout: 'SELECT' }), selected: ['999'], placeholder: null },
].map((c) => ({ ...c, row: rr.buildSelectRow(c.group, { selectedRoleIds: c.selected, placeholder: c.placeholder })?.toJSON() ?? null }));
out.footers = [];
for (const type of [...rr.RR_TYPES, 'ODD']) for (const layout of [...rr.RR_LAYOUTS, 'GRID']) for (const maxRoles of [null, 0, 3]) {
  out.footers.push({ type, layout, maxRoles, text: rr.panelFooter({ type, layout, maxRoles }) });
}
out.customIds = [['3f1c2a5e-0000-4000-8000-000000000001', '400000000000000001'], ['clx0abc1230000abcdefghij', '400000000000000002']]
  .map(([g, r]) => ({ groupId: g, roleId: r, item: rr.draftItem(g, { roleId: r, label: 'L' }, 4), select: rr.buildSelectRow(mkGroup(1, { id: g, layout: 'SELECT' })).toJSON().components[0].custom_id }));
out.indexKey = rr.indexKey('900000000000000001');

// ── List pages ────────────────────────────────────────────────────────────────
const panels = Array.from({ length: 25 }, (_, i) => ({
  id: `p${i}`, title: i === 0 ? `${i}-${'T'.repeat(256)}` : `Panel ${i}`, type: rr.RR_TYPES[i % 4], layout: rr.RR_LAYOUTS[i % 3],
  maxRoles: i % 5 === 0 ? 25 : null, attached: i % 2 === 0, plainText: i % 3 === 0,
  channelId: '300000000000000001', messageId: `5000000000000000${String(i).padStart(2, '0')}`, itemCount: i, messageMissingAt: i % 4 === 0 ? new Date(0) : null,
}));
out.listPages = { panels, pages: cmd.buildListPages(panels, { guildId: '900000000000000001', total: 25, settings: { embedColor: 0x123456 } })
  .map((e) => { const j = e.toJSON(); return { title: j.title, color: j.color, fields: j.fields }; }) };

// ── Templates ─────────────────────────────────────────────────────────────────
out.rrTemplates = rr.RR_TEMPLATES;
out.roleTemplates = Object.entries(tpl.ROLE_TEMPLATES).map(([key, t]) => ({
  key, label: t.label, panel: t.panel, note: t.note, linksZones: t.linksZones,
  roles: t.roles.map((r) => ({ name: r.name, label: r.label, emoji: r.emoji ?? null, color: r.color ?? 0, description: r.description ?? null, zone: r.zone ?? null })),
}));
out.templateChoices = cmd.data.toJSON().options.find((o) => o.name === 'create').options.find((o) => o.name === 'template').choices;
out.describeSetup = [[9, 1], [0, 10], [4, 0], [1, 0], [0, 1], [1, 1], [2, 3]].map(([created, reused]) => ({ created, reused, text: setup.describeSetup({ created, reused }) }));
const mkRoles = (flags) => flags.map((c, i) => ({ name: `R${i}`, created: c }));
out.describeReused = [[true, true], [false], [false, true, false], [true]].map((f) => ({ roles: mkRoles(f), text: setup.describeReused({ roles: mkRoles(f) }) }));
const { TIMEZONE_TEMPLATE_PAST_ZONES } = await imp('src', 'utils', 'timezones.js');
const pastUs = TIMEZONE_TEMPLATE_PAST_ZONES['timezones-us'][0];
const pastWorld = TIMEZONE_TEMPLATE_PAST_ZONES['timezones-world'][0];
out.pastTemplates = [pastUs, pastWorld, [...pastUs].reverse(), pastUs.slice(1), [...pastUs, 'Europe/London'], [...pastUs.slice(1), pastUs[1]],
  [...pastUs.slice(0, 5), undefined], []].map((zones) => ({ zones: zones.map((z) => z ?? null), key: setup.matchPastTemplate(zones) }));
out.orderItems = [[['c', 'x', 'a', 'y', 'b'], ['a', 'b', 'c']], [[], ['a']], [['b', 'a'], []], [['a', 'a', 'b'], ['b']]]
  .map(([roles, order]) => ({ roles, order, result: rr.orderItems(roles.map((roleId) => ({ roleId })), order).map((it) => it.roleId) }));

const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-reactionroles');
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, 'golden.json'), JSON.stringify(out, null, 1));
redis.disconnect();
process.exit(0);
