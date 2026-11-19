// Generates the data and the reference results of the dash-schema crate from the JS dashboard
// (src/dashboard/schema.js and src/dashboard/fields.js):
//   rust/crates/dash-schema/src/fields_data.json  every settings group, field descriptor (as publicDescriptor serves it),
//                                                 column map and restated constant, embedded by the crate
//   rust/fixtures/golden/dash-schema/*.json       what the JS functions answer for a wide table of inputs
// Pure: no database or Redis. Usage (repository root): node rust/fixtures/gen/dash-schema-golden.mjs
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';

// Zone-less date strings are local time in V8; the Rust port reads `TZ` (UTC when unset), so the goldens use UTC.
process.env.TZ = 'UTC';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (p) => import(pathToFileURL(join(ROOT, p)).href);
const schema = await imp('src/dashboard/schema.js');
const fields = await imp('src/dashboard/fields.js');
const { MODULE_NAMES } = await imp('src/config/modules.js');

const OUT_GOLDEN = join(ROOT, 'rust', 'fixtures', 'golden', 'dash-schema');
const OUT_DATA = join(ROOT, 'rust', 'crates', 'dash-schema', 'src', 'fields_data.json');
mkdirSync(OUT_GOLDEN, { recursive: true });

const json = (v) => JSON.parse(JSON.stringify(v === undefined ? null : v));
const write = (file, data) => {
  writeFileSync(file, `${JSON.stringify(data, null, 1)}\n`);
  process.stdout.write(`${file.slice(ROOT.length + 1)}\n`);
};

// ── The crate's data ─────────────────────────────────────────────────────────────────────────────────────────
const pubGroup = (g) => ({ id: g.id, title: g.title, ...(g.description ? { description: g.description } : {}),
  fields: g.fields.map((d) => json(schema.publicDescriptor(d))) });
const groupFieldNames = new Set([...fields.GENERAL_GROUPS, ...Object.values(fields.MODULE_GROUPS).flat()]
  .flatMap((g) => g.fields.map((d) => d.field)));
const data = {
  settingsFieldOrder: Object.keys(fields.SETTINGS_FIELDS),
  flagFields: Object.values(fields.SETTINGS_FIELDS).filter((d) => !groupFieldNames.has(d.field))
    .map((d) => json(schema.publicDescriptor(d))),
  generalGroups: fields.GENERAL_GROUPS.map(pubGroup),
  moduleGroups: Object.fromEntries(MODULE_NAMES.map((k) => [k, fields.MODULE_GROUPS[k].map(pubGroup)])),
  // The server-only hooks each field has (the crate restates them; its tests check this table).
  hooks: Object.fromEntries(Object.entries(fields.SETTINGS_FIELDS)
    .map(([f, d]) => [f, ['read', 'check', 'normalize', 'write'].filter((k) => typeof d[k] === 'function')])
    .filter(([, list]) => list.length)),
  settingsColumns: fields.SETTINGS_COLUMNS,
  jsonColumns: fields.JSON_COLUMNS,
  notExposed: fields.NOT_EXPOSED,
  constants: json({
    MAX_SETTINGS_CHANGES: fields.MAX_SETTINGS_CHANGES,
    DEFAULT_TEXTS: fields.DEFAULT_TEXTS,
    DEFAULT_EMBEDS: fields.DEFAULT_EMBEDS,
    DEFAULT_CAPTCHA_PANEL: fields.DEFAULT_CAPTCHA_PANEL,
    DEFAULT_VERIFY_BUTTON: fields.DEFAULT_VERIFY_BUTTON,
    DEFAULT_RANK_CARD_STYLE: fields.DEFAULT_RANK_CARD_STYLE,
    NUMBER_LIMITS: fields.NUMBER_LIMITS,
    AUTOMOD_RULES: fields.AUTOMOD_RULES,
    AUTOMOD_ACTIONS: fields.AUTOMOD_ACTIONS,
    AUTOMOD_DEFAULTS: fields.AUTOMOD_DEFAULTS,
    AUTOMOD_HONEYPOT: fields.AUTOMOD_HONEYPOT,
    LOG_EVENTS: fields.LOG_EVENTS,
    TIER_ACTIONS: fields.TIER_ACTIONS,
  }),
};
write(OUT_DATA, data, true);

// ── Probes ───────────────────────────────────────────────────────────────────────────────────────────────────
const CH = '300000000000000001';
const CH2 = '300000000000000002';
const RL = '400000000000000001';
const RL2 = '400000000000000002';
const GENERIC = [
  null, true, false, 0, 1, -1, 3, 15, 25, 60, 0.5, 1.234, 1.255, 2.5, 1e9, 1e10, 70, 600, 3600, 10_080, 2_419_201,
  '', ' ', 'abc', '  x  ', 'line\nbreak', 'cr\r\nlf', 'ctrl\u0007', 'x'.repeat(33), 'x'.repeat(1001), 'x'.repeat(2001),
  'off', 'OFF', '!', '?!', '!!!!!!', 'a b', '<@', 'coins', 'gold    bars', 'a*b',
  '#5865F2', '#5865f2', '5865F2', '#000000', '#zzzzzz', 16777215, 16777216,
  '12:30', '24:00', '9:30', '2026-01-02T03:04:05Z', '2026-01-02', 'garbage date',
  '🪙', '1️⃣', '👍🏽', '🇩🇪', 'x', '<:coin:111111111111111111>', '<a:spin:222222222222222222>', '<:c:1>',
  'https://example.com/a.png', 'http://x.example', 'ftp://x.example/a.png', `https://x.example/${'a'.repeat(500)}`,
  CH, '12345', 'general', RL,
  'UTC', 'Etc/UTC', 'Europe/Berlin', 'europe/berlin', 'Asia/Calcutta', 'Europe/Kiev', 'US/Eastern', 'EST', '+02:00', 'Mars/Phobos',
  'america/new_york', 'GMT',
  '{rewards}', '🔥', '@x', '#', ':', '/', '\\', '`', '!!!!!', '<', '@here', 'new\nline', '  spaced   out  ', '💎',
  '🐶‍🐱', '👨‍👩‍👧','Hi {user}', '{achievement} {player}', '  Hi {user}  ', 20.5, -0.5, 0.004, 0.005, 1.005, 4.5e-7,
  'PUBLIC', 'IMAGE', 'CHANNEL', 'ALLOW', 'off', 'kick', 'TIMEOUT', '1440', 'SUCCESS', 'voiceJoin',
  [], [CH], [CH, CH], [CH, CH2], [RL, RL2, RL], ['12345'], [1, 2], ['a'], ['  BAD   word ', 'bad word', 'ｆｕｌｌ'], ['fine', '!'],
  ['.*'], ['ok', '(a+)+$'], ['f+o+', 'b[ae]d'], ['https://www.Example.com/x', 'example.com'], ['not a domain'], ['bücher.de'],
  ['messageDelete', 'nope'], ['voiceJoin', 'messageDelete'], ['tenor.com', 'b.com'],
  Array.from({ length: 26 }, (_, i) => String(400000000000000100n + BigInt(i))),
  {}, { x: 1 }, { __proto__: null, constructor: 1 },
  { title: 'Hi', description: 'there', footer: { text: 'f', iconUrl: null }, timestamp: false },
  { title: '', description: '' }, { title: 'x'.repeat(300) },
  { content: 'Hello', embed: null, buttons: [] }, { content: null, embed: null, buttons: [] },
  { content: 'Hi', embed: { title: 'T', color: 5814783 }, buttons: [{ label: 'Site', url: 'https://example.com' }] },
  { label: 'Go', style: 'DANGER' }, { label: '', style: 'SUCCESS' }, { label: 'Go', style: 'PINK' },
  { action: 'kick', threshold: 80 }, { action: 'off', threshold: 49 }, { action: 'kick', threshold: 3 }, { action: 'explode' },
  { timeoutSeconds: 59 }, { action: 'timeout', timeoutSeconds: 3600, roleMode: 'only', roleIds: [RL], channelIds: [CH] },
  { channelIds: [CH], action: 'ban', timeoutSeconds: 600 }, { channelIds: [], action: 'timeout', timeoutSeconds: 3600 },
  { enabled: true, background: '#AABBCC', backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: 0.456 },
  { enabled: false, background: '#AABBCC', backgroundUrl: 'https://img.example/bg.png', textColor: null, accentColor: '#112233', overlayOpacity: 0.5 },
  { overlayOpacity: 1.5 }, { overlayOpacity: 0.333 }, { overlayOpacity: 0.95 }, { backgroundUrl: 'ftp://x.example/a.png' }, { v: 1 },
  { background: '#000000', accentColor: '5865f2' },
  [{ roleId: RL, multiplier: 1.255 }], [{ roleId: RL, multiplier: 2 }, { roleId: RL, multiplier: 3 }], [{ roleId: RL, multiplier: 1 }],
  [{ roleId: RL2, zone: 'america/chicago' }, { roleId: RL, zone: 'Asia/Calcutta' }], [{ roleId: RL, zone: 'EST' }],
  [{ roleId: RL, zone: 'UTC' }, { roleId: RL, zone: 'Asia/Tokyo' }],
  [{ warnCount: 3, withinSeconds: null, action: 'TIMEOUT', durationSeconds: 600 }],
  [{ warnCount: 3, withinSeconds: null, action: 'KICK', durationSeconds: 60 }],
  [{ warnCount: 3, withinSeconds: null, action: 'TIMEOUT', durationSeconds: null }],
  [{ warnCount: 3, withinSeconds: null, action: 'TIMEOUT', durationSeconds: 2_419_201 }],
  [{ warnCount: 3, withinSeconds: null, action: 'TEMPBAN', durationSeconds: null }],
  [{ warnCount: 3, withinSeconds: null, action: 'TIMEOUT', durationSeconds: 600 }, { warnCount: 3, withinSeconds: null, action: 'BAN', durationSeconds: null }],
  [{ warnCount: 3, withinSeconds: 59, action: 'TIMEOUT', durationSeconds: 600 }],
  [{ warnCount: 5, withinSeconds: 3600, action: 'BAN', durationSeconds: null }, { warnCount: 3, withinSeconds: null, action: 'KICK', durationSeconds: null }],
  [{ warnCount: 4, withinSeconds: null, action: 'MUTE' }],
  [{ channelId: CH, name: '{user}', archiveMinutes: '60', closeButton: true }],
  [{ channelId: CH, name: 'a' }, { channelId: CH, name: 'b' }],
  [{ channelId: CH2, roleId: RL }, { channelId: CH2, roleId: RL }], [{ channelId: CH2, roleId: RL }],
];

// Per field: the generic probes, its default and its default with a sub-value changed.
function extraProbes(desc) {
  const out = [];
  if (desc.default !== undefined) out.push(desc.default);
  if (schema.isPlainObject(desc.default)) {
    for (const f of desc.fields ?? []) out.push({ ...desc.default, [f.field]: null });
  }
  return out;
}

// Results are interned: each case lists indexes into the file's `table` of distinct results.
const makeTable = () => {
  const table = [];
  const index = new Map();
  const intern = (v) => {
    const key = JSON.stringify(v);
    if (!index.has(key)) { index.set(key, table.length); table.push(JSON.parse(key)); }
    return index.get(key);
  };
  return { table, intern };
};
const T1 = makeTable();
const T2 = makeTable();

const run = (fn) => {
  try {
    return { ok: json(fn()) };
  } catch (err) {
    if (err instanceof schema.FieldError) return { error: { path: err.path, message: err.message } };
    return { throws: String(err?.message ?? err) };
  }
};

// ── validateSettingsChanges + toStoredChanges per field and probe ───────────────────────────────────────────
const SAMPLE_ROW = {
  automodConfig: { version: 2, rules: { caps: { action: 'kick', threshold: 80 } }, badWords: ['old'], allowedDomains: ['tenor.com', 'b.com'],
    honeypot: { channelIds: [CH], action: 'ban', timeoutSeconds: 600 } },
  infractionEscalation: [
    { id: 4, warnCount: 3, withinSeconds: null, action: 'TIMEOUT', durationSeconds: 600, countFrom: '2026-01-01T00:00:00.000Z' },
    { id: 7, warnCount: 9, withinSeconds: null, action: 'BAN', durationSeconds: null, countFrom: null },
  ],
  captchaTemplate: { content: 'Hello', embed: null, buttons: [], verifyButton: { label: 'Go', style: 'DANGER' } },
};

const settingsCases = [];
for (const [field, desc] of Object.entries(fields.SETTINGS_FIELDS)) {
  const results = [...GENERIC, ...extraProbes(desc)].map((value) => {
    const r = fields.validateSettingsChanges({ changes: { [field]: value } });
    const entry = {};
    if (r.ok) {
      entry.changes = json(r.changes);
      if (r.refs.channels.length || r.refs.roles.length) entry.refs = json(r.refs);
      entry.stored = json(fields.toStoredChanges(r.changes, null));
      const onRow = json(fields.toStoredChanges(r.changes, SAMPLE_ROW));
      if (JSON.stringify(onRow) !== JSON.stringify(entry.stored)) entry.storedOnRow = onRow;
    } else {
      entry.error = { message: r.message, path: r.path ?? null };
    }
    return T1.intern(entry);
  });
  settingsCases.push({ field, extra: json(extraProbes(desc)), results });
}

// ── validateValue / readValue / collectStoredRefs / storedRefKeys for every descriptor, nested ones too ─────
function allDescriptors(list, where, out = []) {
  for (const d of list) {
    out.push([`${where}${d.field}`, d]);
    if (d.fields) allDescriptors(d.fields, `${where}${d.field}.`, out);
    if (d.itemFields) allDescriptors(d.itemFields, `${where}${d.field}[].`, out);
  }
  return out;
}
const SYNTHETIC = [
  { field: 'name', type: 'text', label: 'Name', maxLength: 50 },
  { field: 'nick', type: 'text', label: 'Nick', maxLength: 20, minLength: 3, nullable: true },
  { field: 'code', type: 'text', maxLength: 10, pattern: '[a-z]+', patternMessage: 'Lower-case letters only.' },
  { field: 'plain', type: 'text', maxLength: 10, pattern: '\\d{2}' },
  { field: 'memo', type: 'text', label: 'Memo', maxLength: 20, multiline: true, minLength: 1 },
  { field: 'who', type: 'user', label: 'Member' },
  { field: 'whoMaybe', type: 'user', label: 'Member', nullable: true, default: null },
  { field: 'when', type: 'datetime', label: 'When' },
  { field: 'at', type: 'time', label: 'At', default: '09:30' },
  { field: 'link', type: 'url', label: 'Link' },
  { field: 'linkShort', type: 'url', label: 'Link', maxLength: 20, nullable: true },
  { field: 'icon', type: 'emoji', label: 'Icon', default: '⭐' },
  { field: 'msg', type: 'template', label: 'Message', requireContent: false },
  { field: 'msgReq', type: 'template', label: 'Message' },
  { field: 'emb', type: 'embed', label: 'Embed' },
  { field: 'hexOnly', type: 'color', format: 'hex', label: 'Colour', default: '#FFFFFF' },
  { field: 'tags', type: 'stringList', label: 'Tags', maxItems: 3, itemMaxLength: 8, itemMinLength: 2,
    itemPattern: '[a-z0-9-]+', itemPatternMessage: 'may only use a-z, 0-9 and -.' },
  { field: 'words', type: 'stringList', label: 'Words', maxItems: 5, itemMaxLength: 10 },
  { field: 'lowerWords', type: 'stringList', label: 'Words', maxItems: 5, itemMaxLength: 10, lowercase: true },
  { field: 'picks', type: 'multiEnum', label: 'Picks', options: [{ value: 'a', label: 'A' }, { value: 'b', label: 'B' }] },
  { field: 'picksMax', type: 'multiEnum', label: 'Picks', maxItems: 1, options: [{ value: 'voiceJoin', label: 'A' }, { value: 'b', label: 'B' }] },
  { field: 'items', type: 'objectList', label: 'Items', maxItems: 3, minItems: 1,
    itemFields: [{ field: 'channelId', type: 'channel', label: 'Channel', required: true, channelTypes: ['text'] },
      { field: 'roleId', type: 'role', label: 'Role', assignable: true, allowEveryone: true, default: null, nullable: true },
      { field: 'name', type: 'text', label: 'Name', maxLength: 100, minLength: 1, default: '{content}' }] },
  { field: 'items2', type: 'objectList', label: 'Items', maxItems: 3, minItems: 2, itemFields: [{ field: 'name', type: 'text', maxLength: 5 }] },
  { field: 'anyChannel', type: 'channel', label: 'Channel' },
  { field: 'dangerRoles', type: 'roles', label: 'Roles', maxItems: 2, assignable: true, allowDangerous: true, allowEveryone: true },
  { field: 'noLabel', type: 'int', min: 0, max: 5 },
  { field: 'mystery', type: 'banana', label: 'Mystery' },
  { field: 'ratio', type: 'float', label: 'Ratio', min: -1, max: 1 },
];
const descriptors = [...allDescriptors(Object.values(fields.SETTINGS_FIELDS), ''), ...SYNTHETIC.map((d) => [`synthetic.${d.field}`, d])];
const valueCases = descriptors.map(([at, desc]) => ({
  at,
  extra: json(extraProbes(desc)),
  results: [...GENERIC, ...extraProbes(desc)].map((value) => {
    const refs = schema.emptyRefs();
    const v = run(() => schema.validateValue(desc, value, undefined, refs));
    const stored = schema.collectStoredRefs(desc, value);
    const e = { validate: v, read: json(schema.readValue(desc, value)) };
    if (v.ok !== undefined && (refs.channels.length || refs.roles.length)) e.refs = json(refs);
    if (stored.channels.size || stored.roles.size) e.storedRefs = { channels: [...stored.channels], roles: [...stored.roles] };
    const keys = [...schema.storedRefKeys(desc, value)];
    if (keys.length) e.refKeys = keys;
    return T2.intern(e);
  }),
}));
write(join(OUT_GOLDEN, 'settings-changes.json'), { generic: json(GENERIC), sampleRow: SAMPLE_ROW, table: T1.table, cases: settingsCases });
write(join(OUT_GOLDEN, 'values.json'), { generic: json(GENERIC), synthetic: SYNTHETIC, table: T2.table, cases: valueCases });

// ── Rows: readSettingsValues, collectSettingsRefs, collectSettingsRefKeys, automodConfigOf ───────────────────
const baseline = JSON.parse(readFileSync(join(ROOT, 'rust', 'fixtures', 'golden', 'mockdb', 'baseline.json'), 'utf8'));
const gs = baseline.GuildSettings;
const ROWS = {
  null: null,
  empty: {},
  seedMin: gs.min,
  seedFull: gs.full,
  junk: {
    welcomeMessage: null, welcomeEmbed: { title: 'Custom', card: false }, welcomeUseEmbed: true,
    goodbyeMessage: null, goodbyeUseEmbed: true, levelUpMessage: '   ',
    disabledLogEvents: ['voiceJoin', 'notAnEvent', 'messageDelete'], timezone: 'Not/AZone', currencyEmoji: 'x',
    automodConfig: { version: 2, rules: { caps: { action: 'kick', threshold: 80 } }, badWords: ['ok', 'x', 'fine word'], allowedDomains: [] },
    infractionEscalation: [{ id: 2, warnCount: 5, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom: null },
      { warnCount: 3, action: 'BAN', duration: 600 }],
    captchaTemplate: { content: 'Press it', embed: null, buttons: [], verifyButton: { label: 'Go', style: 'DANGER' } },
  },
  stored: {
    ...SAMPLE_ROW,
    modLogChannelId: CH, modRoleIds: [RL, 'nope'], muteRoleId: RL2, autoroleIds: [RL], timezoneRoles: [{ roleId: RL, zone: 'UTC' }, 'junk'],
    xpBoostRoles: [{ roleId: RL2, multiplier: 2 }], voiceRoleLinks: [{ channelId: CH2, roleId: RL }],
    autoThreadChannels: [{ channelId: CH, name: 'x', archiveMinutes: '60', closeButton: true }, { nope: 1 }],
    welcomeCardStyle: { v: 1, enabled: false, background: '#aabbcc', overlayOpacity: 0.456, backgroundUrl: 'https://x.example/a.png' },
    rankCardStyle: { background: '#000000', overlayOpacity: 2 }, welcomeEmbed: { title: '' }, welcomeDmEmbed: { description: 'dm' },
    goodbyeEmbed: { title: 'Bye', color: 123 }, embedColor: 0, customPrefix: '?', timezone: 'Asia/Calcutta', xpMultiplier: 1.5,
    logIgnoredChannelIds: [CH, 5], birthdayMessage: 'Hi', birthdayMessageAge: ' ', welcomeMessage: '  spaced ',
    welcomeDmMessage: null, goodbyeMessage: 'Bye {user}', levelUpMode: 'NOPE', currencyEmoji: '<:c:111111111111111111>',
  },
  legacyAutomod: { automodConfig: { filterInvites: true, automodAction: 'KICK', badWords: ['legacy'] } },
  automodV2: { automodConfig: { version: 2, rules: { caps: { action: 'kick', threshold: 80, roleMode: 'only', roleIds: [RL] }, spam: 'x' },
    badWords: ['foo', 'bar baz', 'ＢＡＤ', 'x'], badWordPatterns: ['f+o+', '.*', 7], blockedDomains: ['evil.com', 'Evil.com', 'not a domain'],
    allowedDomains: 'x', exemptRoleIds: [RL2, 'x'], exemptChannelIds: [CH], honeypot: { channelIds: [CH], action: 'kick', timeoutSeconds: 120 } } },
};
const rows = Object.fromEntries(Object.entries(ROWS).map(([name, row]) => {
  const refs = fields.collectSettingsRefs(row);
  // Saving every value as read back: what validation answers and the columns that would be written.
  const resaved = fields.validateSettingsChanges({ changes: fields.readSettingsValues(row) });
  const resave = resaved.ok
    ? { ok: true, stored: json(fields.toStoredChanges(resaved.changes, row)) }
    : { ok: false, message: resaved.message, path: resaved.path ?? null };
  return [name, {
    resave,
    row: json(row),
    values: json(fields.readSettingsValues(row)),
    refs: { channels: [...refs.channels], roles: [...refs.roles] },
    refKeys: [...fields.collectSettingsRefKeys(row)],
    automodConfig: json(fields.automodConfigOf(row)),
  }];
}));
write(join(OUT_GOLDEN, 'rows.json'), rows);

// ── Pure helpers ─────────────────────────────────────────────────────────────────────────────────────────────
const words = ['  Hello  World ', 'ｆｕｌｌ', 'a', '!!', 'x'.repeat(65), 'zero​width', 'ok', 42, null, 'Ünïcödé', '١٢', 'a\tb\nc', 'ﬁne'];
const domains = ['Example.COM', 'https://www.example.com/path', '*.example.com', 'bücher.de', 'nope', '', 'a..b', 'example.com:8080',
  'xn--bcher-kva.de', 'http://user@evil.com', 'example.com.', 'www.', 'sub.domain.co.uk/x?y#z', 'ex_ample.com', '-bad.com', 'a.b',
  'ftp://files.example.org', 'https://', '日本.jp', 'a'.repeat(64) + '.com', null, 'HTTPS://WWW.Ex.org:443/a'];
const escalations = [
  null, 'x', [], [{ warnCount: 3, action: 'BAN', duration: 600 }, { warnCount: 5, action: 'kick' }, { warnCount: 0, action: 'BAN' }],
  [{ id: 2, warnCount: 3, withinSeconds: 600, action: 'TIMEOUT', durationSeconds: 10, countFrom: null },
    { id: 2, warnCount: 4, withinSeconds: null, action: 'MUTE', durationSeconds: null, countFrom: '2026-02-01T00:00:00.000Z' },
    { warnCount: 3, withinSeconds: 600, action: 'TEMPBAN', durationSeconds: 99_999_999 },
    { warnCount: 6, withinSeconds: 30, action: 'TIMEOUT', durationSeconds: null }],
  Array.from({ length: 12 }, (_, i) => ({ id: i + 1, warnCount: i + 1, withinSeconds: null, action: 'KICK', durationSeconds: 5 })),
  [{ warnCount: 2, action: 'MUTE', durationSeconds: 30, withinSeconds: '600', countFrom: 'not a date' }, 'junk', null,
    { warnCount: 2, action: 'mute', durationSeconds: 5_000_000_000, withinSeconds: 600, id: 0 },
    { warnCount: 2.5, action: 'KICK' }, { warnCount: 7, action: 'TIMEOUT', duration: 99, countFrom: '2026-03-01' }],
];
const embeds = [null, {}, { title: ' ' }, { title: 'x' }, { footer: { text: 'f' } }, { image: 'https://a.example/x.png' },
  { fields: [{ name: 'a', value: 'b' }] }, { author: { name: 'me' } }, { thumbnail: 'https://a.example/x.png' }, { fields: [] },
  { description: 7 }, { author: { name: '  ' } }, 'title', []];
const kinds = [0, 2, 4, 5, 13, 15, 16, 11, 1, -1, null];
write(join(OUT_GOLDEN, 'helpers.json'), {
  words: words.map((w) => ({ input: w, out: fields.normalizeAutomodWord(w) })),
  domains: domains.map((d) => ({ input: d, out: fields.normalizeAutomodDomain(d) })),
  escalation: escalations.map((e) => ({ input: json(e), out: json(fields.normalizeEscalation(e)) })),
  embedHasContent: embeds.map((e) => ({ input: e, out: fields.embedHasContent(e) })),
  channelKind: kinds.map((k) => ({ input: k, out: schema.channelKind(k) })),
  refKey: [['tiers.2.roleAdd', '1'], ['tiers.0.roleAdd', '1'], ['a', '2'], ['0.1', '3'], ['x.10y.3', '4'], ['', '5']]
    .map(([p, id]) => ({ path: p, id, out: schema.refKey(p, id) })),
  isSnowflake: ['12345678901234567', '123456789012345678901', '1234567890123456', '١٢٣٤٥٦٧٨٩٠١٢٣٤٥٦٧٨', 12345678901234567, ' 12345678901234567']
    .map((v) => ({ input: v, out: schema.isSnowflake(v) })),
});

// ── checkCrossFields, checkRefs, collectAssignableRoles, lintDescriptors, validateSettingsChanges bodies ─────
const base = { xpMin: 15, xpMax: 25, dailyMin: 50, dailyMax: 120, workMin: 100, workMax: 300, economyStartingBalance: 1000,
  economyMaxBalance: 1_000_000_000, levelUpMode: 'CURRENT', levelUpChannelId: null, achievementAnnounceMode: 'CURRENT',
  achievementChannelId: null, modRoleIds: [RL2], immuneRoleIds: [], muteRoleId: null, liveRoleId: RL, liveRoleRequiredRoleId: null };
const mute = [{ warnCount: 3, withinSeconds: null, action: 'MUTE', durationSeconds: null }];
const cross = [
  [{}, {}], [{ xpMin: 30 }, { xpMin: 30 }], [{ xpMax: 10 }, { xpMax: 10 }], [{ xpMin: 30, xpMax: 40 }, { xpMin: 30, xpMax: 40 }],
  [{ xpMin: 30 }, { modEnabled: false }], [{ xpMin: 30 }, null], [{ dailyMin: 200 }, { dailyMin: 200 }], [{ workMax: 50 }, { workMax: 50 }],
  [{ economyMaxBalance: 500 }, { economyMaxBalance: 500 }], [{ levelUpMode: 'CHANNEL' }, { levelUpMode: 'CHANNEL' }],
  [{ levelUpMode: 'CHANNEL', levelUpChannelId: CH }, { levelUpMode: 'CHANNEL', levelUpChannelId: CH }],
  [{ achievementAnnounceMode: 'CHANNEL' }, { achievementAnnounceMode: 'CHANNEL' }],
  [{ achievementAnnounceMode: 'CHANNEL' }, { achievementChannelId: null }],
  [{ captchaRoleId: RL2 }, { captchaRoleId: RL2 }], [{ captchaRoleId: RL, muteRoleId: RL }, { captchaRoleId: RL }],
  [{ captchaRoleId: RL }, { captchaRoleId: RL }], [{ captchaRoleId: RL, immuneRoleIds: [RL] }, { captchaRoleId: RL }],
  [{ captchaRoleId: RL, captchaRequiredRoleIds: [RL] }, { captchaRequiredRoleIds: [RL] }],
  [{ liveRoleRequiredRoleId: RL }, { liveRoleRequiredRoleId: RL }], [{ liveRoleRequiredRoleId: RL }, { liveRoleId: RL }],
  [{ infractionEscalation: mute }, { infractionEscalation: mute }], [{ infractionEscalation: mute, muteRoleId: RL }, { infractionEscalation: mute }],
  [{ infractionEscalation: [{ ...mute[0], durationSeconds: 3600 }] }, { infractionEscalation: [{ ...mute[0], durationSeconds: 3600 }] }],
  [{ infractionEscalation: [{ ...mute[0], durationSeconds: 3_000_000 }] }, { infractionEscalation: [{ ...mute[0], durationSeconds: 3_000_000 }] }],
  [{ infractionEscalation: [{ id: 1, warnCount: 2, action: 'MUTE', withinSeconds: null, durationSeconds: null }] }, { modEnabled: true }],
  [{ infractionEscalation: [{ id: 1, warnCount: 2, action: 'MUTE', withinSeconds: null, durationSeconds: null }] }, null],
  [{ automodConfig: { version: 2, blockedDomains: ['a.com'], allowedDomains: ['a.com'] } }, { automodBlockedDomains: ['a.com'], automodAllowedDomains: ['a.com'] }],
  [{ automodConfig: { version: 2, blockedDomains: ['a.com'], allowedDomains: ['b.com'] } }, { automodAllowedDomains: ['b.com'] }],
  [{ automodConfig: { version: 2, blockedDomains: ['tenor.com'] } }, { automodBlockedDomains: ['tenor.com'] }],
  [{ xpMin: 30, xpMax: 'x' }, { xpMin: 30 }],
];
const crossCases = cross.map(([patch, changes]) => ({ row: { ...base, ...patch }, changes, out: fields.checkCrossFields({ ...base, ...patch }, changes) }));

const resources = {
  channels: [[CH, { kind: 'text' }], [CH2, { kind: 'voice' }], ['300000000000000003', { kind: 'category' }]],
  roles: [['400000000000000000', { everyone: true, managed: false, assignable: true }], [RL, { everyone: false, managed: false, assignable: true }],
    [RL2, { everyone: false, managed: true, assignable: true }], ['400000000000000003', { everyone: false, managed: false, assignable: false }]],
};
const res = { channels: new Map(resources.channels), roles: new Map(resources.roles) };
const ch = (id, kinds) => ({ id, path: 'p', kinds });
const rl = (id, o = {}) => ({ id, path: 'r', assignable: false, allowEveryone: false, allowDangerous: false, ...o });
const refCases = [
  [{ channels: [], roles: [] }, {}],
  [{ channels: [ch(CH, ['text'])], roles: [] }, {}],
  [{ channels: [ch(CH2, ['text', 'announcement'])], roles: [] }, {}],
  [{ channels: [ch('300000000000000099', ['text'])], roles: [] }, {}],
  [{ channels: [ch('300000000000000099', ['text'])], roles: [] }, { keepChannels: ['300000000000000099'] }],
  [{ channels: [ch(CH, ['text'])], roles: [rl('400000000000000000')] }, {}],
  [{ channels: [], roles: [rl('400000000000000000', { allowEveryone: true })] }, {}],
  [{ channels: [], roles: [rl(RL2, { assignable: true })] }, {}],
  [{ channels: [], roles: [rl(RL2)] }, {}],
  [{ channels: [], roles: [rl('400000000000000003', { assignable: true })] }, {}],
  [{ channels: [], roles: [rl('400000000000000099')] }, { keepRoles: ['400000000000000099'] }],
  [{ channels: [], roles: [rl('400000000000000099')] }, {}],
  [{ channels: [], roles: [rl(RL, { assignable: true })] }, {}],
].map(([refs, keep]) => {
  let out = null;
  try {
    schema.checkRefs(refs, res, { keepChannels: new Set(keep.keepChannels ?? []), keepRoles: new Set(keep.keepRoles ?? []) });
  } catch (err) {
    out = { path: err.path, message: err.message };
  }
  return { refs, keep, out };
});

const assignable = [
  [Object.values(fields.SETTINGS_FIELDS), ROWS.stored],
  [Object.values(fields.SETTINGS_FIELDS), { voiceRoleLinks: [{ channelId: CH, roleId: RL }, { roleId: 'x' }, 'x'], autoroleIds: [RL, 5, RL2], captchaRoleId: RL, liveRoleRequiredRoleId: RL2 }],
  [SYNTHETIC.filter((d) => d.type !== 'banana'), { items: [{ roleId: RL }, { roleId: RL2 }], dangerRoles: [RL] }],
  [[], null],
].map(([descs, record]) => ({ fields: descs.map((d) => d.field), synthetic: descs.some((d) => SYNTHETIC.includes(d)),
  record, out: schema.collectAssignableRoles(descs, record) }));

const lint = [
  [[{ field: 'a', type: 'int', label: 'A' }]],
  [[{ field: 'a', type: 'int', label: 'A', min: 0, max: 5, default: 9 }, { field: 'a', type: 'nope' }]],
  [[{ type: 'text', label: '', maxLength: 5 }]],
  [[{ field: 'c', type: 'channel', label: 'C', channelTypes: ['text', 'thread'] }, { field: 'd', type: 'roles', label: 'D' }]],
  [[{ field: 'o', type: 'object', label: 'O', fields: [{ field: 'x', type: 'enum', label: 'X' }], default: {} }]],
  [[{ field: 'l', type: 'objectList', label: 'L', maxItems: 2, itemFields: [{ field: 'y', type: 'stringList', label: 'Y' }] }]],
  [[{ field: 'e', type: 'embed', label: 'E', default: 7 }, { field: 't', type: 'text', label: 'T', maxLength: 3, default: 'toolong' }]],
  [SYNTHETIC],
].map(([descs]) => ({ fields: descs, out: schema.lintDescriptors(descs, 'w/') }));

const bodies = [null, [], {}, { changes: [] }, { changes: {} }, { changes: { modEnabled: true }, extra: 1 },
  JSON.parse('{"changes":{"__proto__":true}}'), { changes: { constructor: 1 } }, { changes: { levelingEnabled: true } },
  { changes: { ['x'.repeat(80)]: 1 } },
  { changes: Object.fromEntries(Array.from({ length: fields.MAX_SETTINGS_CHANGES + 1 }, (_, i) => [`f${i}`, 1])) },
  { changes: { xpMin: 20, xpMax: 30, modRoleIds: [RL, RL2], welcomeChannelId: CH, automodRuleCaps: { action: 'timeout', roleIds: [RL], channelIds: [CH2] } } },
  { changes: { modEnabled: true, xpMin: 0 } },
  { changes: { timezone: 'america/new_york', customPrefix: 'OFF', embedColor: '#5865f2', xpMultiplier: 1.234,
    automodBadWords: ['  BAD   word ', 'bad word', 'ｆｕｌｌ'], automodBlockedDomains: ['https://www.Example.com/x', 'example.com'],
    logEvents: ['voiceJoin', 'messageDelete'], currencyName: 'gold    bars', levelUpMessage: '  ', birthdayMessage: '  Hi {user}  ',
    welcomeMessage: '  keep spaces  ', welcomeEmbed: { title: '', description: '' },
    welcomeCardStyle: { enabled: true, background: '#AABBCC', backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: 0.456 },
    xpBoostRoles: [{ roleId: RL, multiplier: 1.255 }],
    timezoneRoles: [{ roleId: RL2, zone: 'america/chicago' }, { roleId: RL, zone: 'Asia/Calcutta' }] } },
  { changes: { automodBlockedDomains: ['b.com'], automodAllowedDomains: ['b.com', 'c.com'], automodRuleCaps: { action: 'delete' } } },
  { changes: { automodAllowedDomains: ['x.com'], automodBlockedDomains: ['x.com'] } },
  { changes: { captchaVerifyButton: { label: 'Verify me', style: 'PRIMARY' }, captchaTemplate: { content: 'New text', embed: null, buttons: [] } } },
].map((body) => {
  const r = fields.validateSettingsChanges(body);
  const out = { body: json(body), ok: r.ok };
  if (r.ok) {
    out.changes = r.changes;
    out.refs = r.refs;
    out.stored = fields.toStoredChanges(r.changes, null);
    out.storedOnRow = fields.toStoredChanges(r.changes, SAMPLE_ROW);
    out.cross = fields.checkCrossFields({ ...SAMPLE_ROW, ...out.storedOnRow }, r.changes);
  } else {
    out.message = r.message;
    out.path = r.path ?? null;
  }
  return json(out);
});
write(join(OUT_GOLDEN, 'rules.json'), { cross: crossCases, resources, refs: refCases, assignable, lint, bodies });
