// Web dashboard tests: the catalog and the settings registry (and their agreement with the schema and the bot's own
// rules), configuration parsing, session sealing, the access and CSRF rules, the Discord client's caching and writes,
// and the HTTP API (settings, resources, records) against fake Discord, settings and records dependencies.
// Offline: no Discord, Postgres or Redis (src/dashboard/settingsBridge.js and recordsBridge.js are never imported; the
// parity checks import the bot's services with Redis pointed at a closed port and disconnect it afterwards).
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { request } from 'node:http';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gunzipSync } from 'node:zlib';

process.env.NODE_ENV      ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID     ??= '000000000000000000';
process.env.REDIS_URL     ??= 'redis://127.0.0.1:1';
// Postgres is only reached by the opt-in database cases (TEST_DATABASE_URL, a migrated throw-away database).
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';
const DB = process.env.TEST_DATABASE_URL;

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const { MODULES, MODULE_NAMES } = await import('../src/config/modules.js');
const catalog = await import('../src/dashboard/catalog.js');
const auth = await import('../src/dashboard/auth.js');
const { loadDashboardConfig, REQUIRED_KEYS } = await import('../src/dashboard/config.js');
const {
  createDashboardServer, createParkedServer, createRateLimiter, clientAddress, contactHtml, BODY_LIMIT_BYTES, RECORD_BODY_LIMIT_BYTES,
} = await import('../src/dashboard/server.js');
const discordApi = await import('../src/dashboard/discordApi.js');
const fields = await import('../src/dashboard/fields.js');
const schema = await import('../src/dashboard/schema.js');
const { COLLECTION_LIST, COLLECTIONS, publicCollection } = await import('../src/dashboard/collections/index.js');
const { RecordError } = await import('../src/dashboard/collections/errors.js');
const { LIMITS } = await import('../src/config/constants.js');
const { PLACEHOLDERS } = await import('../src/utils/placeholderParser.js');
const { PermissionFlagsBits } = await import('discord.js');

const { SUB_TOGGLES, CATEGORIES, DESCRIPTIONS, CODES, buildCatalog } = catalog;
const { DiscordApiError } = discordApi;

// ── Shared fixtures ──────────────────────────────────────────────────────────

const SECRET = 'x'.repeat(48);
const KEY = auth.deriveSessionKey(SECRET);
const USER_ID = '111111111111111111';
const OWNER_ID = '999999999999999999';
const TOKEN = 'user-access-token-abc123';
const G = Object.freeze({
  zeta: '200000000000000001', // Manage Server, bot present
  alpha: '200000000000000002', // owner, bot absent
  beta: '200000000000000003', // Administrator, bot present
  plain: '200000000000000004', // no permission, bot present
  broken: '200000000000000005', // settings cannot be read
  stranger: '200000000000000099', // not in the user's guild list
  botOnly: '200000000000000006', // bot present, not in the user's guild list
});

function baseEnv(extra = {}) {
  return {
    CLIENT_ID: '123456789012345678',
    DISCORD_TOKEN: 'bot-token',
    DISCORD_CLIENT_SECRET: 'client-secret',
    DASHBOARD_SESSION_SECRET: SECRET,
    DATABASE_URL: 'postgresql://u:p@localhost/db',
    REDIS_URL: 'redis://localhost:6379',
    ...extra,
  };
}

// With \n line ends: a Windows checkout (core.autocrlf) has \r\n, which the line parsers below would not match.
const readSchema = () => readFileSync(join(ROOT, 'prisma/schema.prisma'), 'utf8').replace(/\r\n?/g, '\n');

function schemaBooleans() {
  const schema = readSchema();
  const model = /^model GuildSettings \{([\s\S]*?)^\}/m.exec(schema)?.[1] ?? '';
  const out = new Map();
  for (const m of model.matchAll(/^\s*(\w+)\s+Boolean(\?)?(?:\s+@default\((true|false)\))?/gm)) {
    out.set(m[1], { optional: m[2] === '?', default: m[3] === undefined ? undefined : m[3] === 'true' });
  }
  return out;
}

// ── Catalog ──────────────────────────────────────────────────────────────────

describe('catalog', () => {
  test('every MODULES key appears in exactly one category, in the contract order', () => {
    assert.deepEqual(CATEGORIES.map((c) => c.id), ['safety', 'community', 'engagement', 'tools']);
    const listed = CATEGORIES.flatMap((c) => c.modules);
    assert.equal(listed.length, new Set(listed).size, 'a module is listed twice');
    assert.deepEqual([...listed].sort(), [...MODULE_NAMES].sort());
    for (const c of CATEGORIES) {
      assert.ok(c.label && c.description, `${c.id} needs a label and a description`);
    }
  });

  test('every module has a short description (a sentence or two)', () => {
    for (const key of MODULE_NAMES) {
      const text = DESCRIPTIONS[key];
      assert.equal(typeof text, 'string', `${key} has no description`);
      assert.ok(text.length >= 20 && text.length <= 140, `${key}: description length ${text.length}`);
      assert.match(text, /\.$/, `${key}: description should be a sentence`);
    }
    assert.deepEqual(Object.keys(DESCRIPTIONS).sort(), [...MODULE_NAMES].sort());
  });

  test('every module has a unique two-letter code', () => {
    assert.deepEqual(Object.keys(CODES).sort(), [...MODULE_NAMES].sort());
    for (const key of MODULE_NAMES) assert.match(CODES[key], /^[A-Z][a-z]$/, `${key}: code ${CODES[key]}`);
    assert.equal(new Set(Object.values(CODES)).size, MODULE_NAMES.length, 'two modules share a code');
  });

  test('the Live Role description warns that switching it off keeps the role on current streamers', () => {
    assert.match(DESCRIPTIONS.liverole, /off here leaves it on current streamers/);
    assert.match(DESCRIPTIONS.liverole, /\/liverole disable in Discord removes it too/);
  });

  test('buildCatalog() mirrors the module registry', () => {
    const c = buildCatalog();
    assert.deepEqual(Object.keys(c.modules).sort(), [...MODULE_NAMES].sort());
    for (const key of MODULE_NAMES) {
      const m = c.modules[key];
      assert.equal(m.key, key);
      assert.equal(m.flag, MODULES[key].flag);
      assert.equal(m.label, MODULES[key].label);
      assert.equal(m.emoji, MODULES[key].emoji);
      assert.equal(m.configCommand, MODULES[key].configCommand);
      assert.equal(m.default, MODULES[key].default);
      assert.equal(m.description, DESCRIPTIONS[key]);
      assert.equal(m.code, CODES[key]);
      assert.ok(Array.isArray(m.subToggles));
      for (const t of m.subToggles) assert.deepEqual(Object.keys(t).sort(), ['default', 'description', 'field', 'label']);
    }
    assert.deepEqual(c.modules.moderation.subToggles.map((t) => t.field), ['modDmTargets']);
    assert.ok(Object.isFrozen(c) && Object.isFrozen(c.modules.welcome.subToggles));
    // Round-trips through JSON unchanged (it is served as is).
    assert.deepEqual(JSON.parse(JSON.stringify(c)), c);
  });

  test('every sub-toggle is a Boolean GuildSettings column whose @default matches the catalog', () => {
    const columns = schemaBooleans();
    const subs = Object.values(SUB_TOGGLES).flat();
    assert.equal(subs.length, 16);
    for (const t of subs) {
      const col = columns.get(t.field);
      assert.ok(col, `${t.field} is not a Boolean column of GuildSettings`);
      assert.equal(col.optional, false, `${t.field} must not be nullable`);
      assert.equal(col.default, t.default, `${t.field}: schema default ${col.default}, catalog ${t.default}`);
      assert.ok(t.label && t.description, `${t.field} needs a label and a description`);
    }
    for (const key of Object.keys(SUB_TOGGLES)) assert.ok(MODULE_NAMES.includes(key), `${key} is not a module`);
  });

  test('every module flag is a Boolean column whose @default matches MODULES', () => {
    const columns = schemaBooleans();
    for (const key of MODULE_NAMES) {
      const col = columns.get(MODULES[key].flag);
      assert.ok(col, `${MODULES[key].flag} is not a Boolean column`);
      assert.equal(col.default, MODULES[key].default, `${MODULES[key].flag}`);
    }
  });

  test('the catalog carries the general page, every module\'s groups and lists, every collection and the placeholders', () => {
    const c = buildCatalog();
    assert.deepEqual(Object.keys(c).sort(), ['categories', 'collections', 'general', 'modules', 'placeholders']);
    assert.equal(c.general.title, 'General');
    assert.deepEqual(c.general.groups.map((g) => g.id), fields.GENERAL_GROUPS.map((g) => g.id));
    assert.deepEqual(c.general.groups[0].fields.map((f) => f.field), ['timezone', 'embedColor', 'customPrefix']);
    for (const key of MODULE_NAMES) {
      const m = c.modules[key];
      assert.deepEqual(m.groups.map((g) => g.id), fields.MODULE_GROUPS[key].map((g) => g.id), key);
      for (const [i, g] of m.groups.entries()) {
        assert.deepEqual(g.fields, JSON.parse(JSON.stringify(fields.MODULE_GROUPS[key][i].fields.map(schema.publicDescriptor))), `${key}/${g.id}`);
      }
      assert.deepEqual(m.collections, COLLECTION_LIST.filter((col) => col.module === key).map((col) => col.key), key);
    }
    assert.deepEqual(c.modules.reactionroles.groups, [], 'nothing beyond the flag');
    assert.deepEqual(Object.keys(c.collections), COLLECTION_LIST.map((col) => col.key));
    for (const col of COLLECTION_LIST) {
      assert.deepEqual(c.collections[col.key], JSON.parse(JSON.stringify(publicCollection(col))), col.key);
      assert.ok(MODULE_NAMES.includes(col.module), `${col.key}: unknown module ${col.module}`);
    }
    assert.deepEqual(c.placeholders.map((p) => p.token), Object.keys(PLACEHOLDERS));
    for (const p of c.placeholders) {
      assert.equal(p.description, PLACEHOLDERS[p.token]);
      assert.ok(['user', 'server', 'channel', 'other'].includes(p.group), p.token);
    }
    assert.equal(c.placeholders.find((p) => p.token === '{server.member_count}').group, 'server');
    assert.equal(c.placeholders.find((p) => p.token === '{channel.name}').group, 'channel');
    assert.equal(c.placeholders.find((p) => p.token === '{random}').group, 'other');
    const text = JSON.stringify(c);
    for (const hook of ['"read"', '"write"', '"normalize"', '"check"']) assert.equal(text.includes(hook), false, `${hook} leaked`);
    assert.ok(Object.isFrozen(c.modules.welcome.groups[0].fields[0]));
  });
});

// ── Settings registry (src/dashboard/fields.js) ──────────────────────────────

/** The GuildSettings columns of prisma/schema.prisma: name -> { type, list, optional, default (raw text) }, and the enums. */
function schemaModel() {
  const text = readSchema();
  const enums = new Map();
  for (const m of text.matchAll(/^enum (\w+) \{([\s\S]*?)^\}/gm)) enums.set(m[1], m[2].split(/\s+/).filter((w) => /^\w+$/.test(w)));
  const models = new Set([...text.matchAll(/^model (\w+) \{/gm)].map((m) => m[1]));
  const body = /^model GuildSettings \{([\s\S]*?)^\}/m.exec(text)?.[1] ?? '';
  const columns = new Map();
  for (const line of body.split('\n')) {
    const m = /^\s*(\w+)\s+(\w+)(\[\])?(\?)?(.*)$/.exec(line);
    if (!m || line.trim().startsWith('//') || models.has(m[2])) continue;
    const def = /@default\(((?:[^()]|\([^)]*\))*)\)/.exec(m[5])?.[1];
    columns.set(m[1], { type: m[2], list: m[3] === '[]', optional: m[4] === '?', default: def });
  }
  return { columns, enums };
}

const allGroups = () => [
  ...fields.GENERAL_GROUPS.map((g) => ['general', g]),
  ...Object.entries(fields.MODULE_GROUPS).flatMap(([key, groups]) => groups.map((g) => [key, g])),
];

describe('settings registry', () => {
  const { SETTINGS_FIELDS, SETTINGS_COLUMNS, NOT_EXPOSED, MODULE_GROUPS, GENERAL_GROUPS } = fields;
  const { columns, enums } = schemaModel();

  test('every group lints clean and every field sits in exactly one group', () => {
    assert.deepEqual(Object.keys(MODULE_GROUPS).sort(), [...MODULE_NAMES].sort());
    const seen = new Map();
    for (const [where, g] of allGroups()) {
      assert.deepEqual(schema.lintDescriptors(g.fields, `${where}/${g.id}/`), [], `${where}/${g.id}`);
      assert.match(g.id, /^[a-z][a-z-]*$/, `${where}: group id`);
      assert.ok(typeof g.title === 'string' && g.title.length > 0, `${where}/${g.id}: title`);
      assert.ok(g.fields.length > 0, `${where}/${g.id}: empty group`);
      for (const d of g.fields) {
        assert.equal(seen.has(d.field), false, `${d.field} is in ${seen.get(d.field)} and ${where}/${g.id}`);
        seen.set(d.field, `${where}/${g.id}`);
        assert.ok(typeof d.description === 'string' && d.description.length >= 10, `${d.field} needs a description`);
        assert.equal(SETTINGS_FIELDS[d.field], d);
      }
    }
    for (const key of MODULE_NAMES) {
      const ids = MODULE_GROUPS[key].map((g) => g.id);
      assert.equal(new Set(ids).size, ids.length, `${key}: duplicate group id`);
    }
    // Module flags are the page switch, not a group field; everything else is in a group.
    const flags = MODULE_NAMES.map((k) => MODULES[k].flag);
    assert.deepEqual(Object.keys(SETTINGS_FIELDS).sort(), [...seen.keys(), ...flags].sort());
    for (const flag of flags) assert.equal(SETTINGS_FIELDS[flag].type, 'boolean');
    assert.ok(GENERAL_GROUPS[0].fields.map((d) => d.field).join() === 'timezone,embedColor,customPrefix');
  });

  test('every field is stored in a real GuildSettings column of a matching type', () => {
    const json = new Set(['object', 'objectList', 'embed', 'template']);
    for (const [field, desc] of Object.entries(SETTINGS_FIELDS)) {
      const column = SETTINGS_COLUMNS[field];
      const col = columns.get(column);
      assert.ok(col, `${field} -> ${column} is not a GuildSettings column`);
      if (field === 'logEvents') {
        assert.ok(col.list && col.type === 'String' && desc.type === 'multiEnum', 'logEvents: the inverse of a String list');
        continue;
      }
      if (column !== field) {
        assert.equal(col.type, 'Json', `${field}: a virtual field must live in a Json column`);
        continue;
      }
      const t = desc.type;
      if (col.type === 'Json') assert.ok(json.has(t), `${field}: Json column, ${t} field`);
      else if (col.list) assert.ok(['channels', 'roles', 'multiEnum', 'stringList'].includes(t), `${field}: list column, ${t} field`);
      else if (col.type === 'Boolean') assert.equal(t, 'boolean', field);
      else if (col.type === 'Float') assert.equal(t, 'float', field);
      else if (col.type === 'Int') assert.ok(t === 'int' || (t === 'color' && desc.format === 'int'), `${field}: Int column, ${t} field`);
      else if (enums.has(col.type)) {
        assert.equal(t, 'enum', field);
        assert.deepEqual(desc.options.map((o) => o.value).sort(), [...enums.get(col.type)].sort(), `${field}: options = enum ${col.type}`);
      } else {
        assert.equal(col.type, 'String', field);
        assert.ok(['text', 'timezone', 'channel', 'role', 'emoji', 'url'].includes(t), `${field}: String column, ${t} field`);
      }
      // A NOT NULL column never receives null.
      if (!col.optional) assert.notEqual(desc.nullable, true, `${field}: the column is not nullable`);
    }
  });

  test('descriptor defaults match the schema @default', () => {
    let checked = 0;
    for (const [field, desc] of Object.entries(SETTINGS_FIELDS)) {
      if (SETTINGS_COLUMNS[field] !== field) continue;
      const col = columns.get(field);
      if (col.default === undefined || col.list) continue;
      checked++;
      if (col.type === 'Boolean') assert.equal(desc.default, col.default === 'true', field);
      else if (col.type === 'Int' || col.type === 'Float') assert.equal(desc.default, Number(col.default), field);
      else if (col.type === 'String') assert.equal(desc.default, JSON.parse(col.default), field);
      else if (enums.has(col.type)) assert.equal(desc.default, col.default, field);
    }
    assert.ok(checked >= 60, `only ${checked} defaults checked`);
  });

  test('every column is either editable or listed in NOT_EXPOSED with a reason; bot-managed state never is editable', () => {
    const exposed = new Set(Object.values(SETTINGS_COLUMNS));
    for (const column of columns.keys()) {
      const hidden = Object.hasOwn(NOT_EXPOSED, column);
      assert.notEqual(exposed.has(column), hidden, `${column}: ${hidden ? 'both exposed and NOT_EXPOSED' : 'neither exposed nor in NOT_EXPOSED'}`);
      if (hidden) assert.ok(NOT_EXPOSED[column].length > 5, `${column}: give a reason`);
    }
    for (const column of Object.keys(NOT_EXPOSED)) assert.ok(columns.has(column), `${column} is not a column`);
    for (const field of ['captchaMessageId', 'captchaLockdownPrior', 'captchaChannelId', 'levelingEnabled', 'xpRate', 'guildId', 'id', 'automodConfig']) {
      assert.equal(Object.hasOwn(SETTINGS_FIELDS, field), false, `${field} must not be a field`);
    }
  });

  test('list caps come from LIMITS', () => {
    const caps = {
      modRoleIds: LIMITS.MOD_ROLES, immuneRoleIds: LIMITS.IMMUNE_ROLES, infractionEscalation: LIMITS.ESCALATION_TIERS,
      autoroleIds: LIMITS.AUTOROLES, noXpRoleIds: LIMITS.NOXP_ROLES, noXpChannelIds: LIMITS.NOXP_CHANNELS,
      xpBoostRoles: LIMITS.XP_BOOST_ROLES, timezoneRoles: LIMITS.TIMEZONE_ROLES, logIgnoredChannelIds: LIMITS.LOG_IGNORED_CHANNELS,
      economyGameChannelIds: LIMITS.ECONOMY_GAME_CHANNELS, automodBadWords: LIMITS.AUTOMOD_WORDS,
      automodBadWordPatterns: LIMITS.AUTOMOD_PATTERNS, automodBlockedDomains: LIMITS.AUTOMOD_DOMAINS,
      automodAllowedDomains: LIMITS.AUTOMOD_DOMAINS, automodExemptRoleIds: LIMITS.AUTOMOD_EXEMPTIONS,
      automodExemptChannelIds: LIMITS.AUTOMOD_EXEMPTIONS,
    };
    for (const [field, max] of Object.entries(caps)) assert.equal(SETTINGS_FIELDS[field].maxItems, max, field);
    for (const key of fields.AUTOMOD_RULE_KEYS) {
      const rule = SETTINGS_FIELDS[`automodRule${key[0].toUpperCase()}${key.slice(1)}`];
      assert.ok(rule, key);
      assert.equal(rule.fields.find((f) => f.field === 'roleIds').maxItems, LIMITS.AUTOMOD_EXEMPTIONS);
      assert.equal(rule.fields.some((f) => f.field === 'threshold'), fields.AUTOMOD_RULES[key].threshold !== null, `${key} threshold`);
    }
    // Roles the bot hands out must pass the assignability check.
    for (const field of ['autoroleIds', 'muteRoleId', 'captchaRoleId', 'birthdayRoleId', 'liveRoleId']) {
      assert.equal(SETTINGS_FIELDS[field].assignable, true, field);
    }
    for (const field of ['modRoleIds', 'immuneRoleIds', 'noXpRoleIds', 'liveRoleRequiredRoleId']) {
      assert.notEqual(SETTINGS_FIELDS[field].assignable, true, field);
    }
  });

  test('message fields offer their extra placeholders', () => {
    assert.deepEqual(SETTINGS_FIELDS.levelUpMessage.placeholders, ['{player}', '{player.name}', '{level}', '{rank}', '{xp}', '{mention}']);
    assert.deepEqual(SETTINGS_FIELDS.birthdayMessageAge.placeholders, ['{age}']);
    for (const t of ['{player}', '{achievement}', '{tier}', '{rewards}']) assert.ok(SETTINGS_FIELDS.achievementMessage.placeholders.includes(t), t);
  });

  test('public descriptors carry no server hooks and survive JSON', () => {
    for (const desc of Object.values(SETTINGS_FIELDS)) {
      const pub = schema.publicDescriptor(desc);
      assert.deepEqual(JSON.parse(JSON.stringify(pub)), pub, desc.field);
      for (const key of ['read', 'write', 'check', 'normalize']) assert.equal(key in pub, false, `${desc.field}.${key}`);
    }
  });

  test('readSettingsValues: every field, defaults and the effective texts for an empty row', () => {
    const v = fields.readSettingsValues(null);
    assert.deepEqual(Object.keys(v).sort(), Object.keys(SETTINGS_FIELDS).sort());
    assert.equal(v.timezone, 'UTC');
    assert.equal(v.customPrefix, '!');
    assert.equal(v.embedColor, null);
    assert.equal(v.welcomeMessage, fields.DEFAULT_TEXTS.welcomeMessage);
    assert.equal(v.levelUpMessage, fields.DEFAULT_TEXTS.levelUpMessage);
    assert.equal(v.welcomeEmbed.title, fields.DEFAULT_EMBEDS.welcomeEmbed.title);
    assert.equal(v.welcomeDmEmbed, null);
    assert.deepEqual(v.automodAllowedDomains, [...fields.AUTOMOD_DEFAULTS.allowedDomains]);
    assert.equal(v.automodRuleCaps.threshold, 70);
    assert.equal(v.automodRuleCaps.action, 'off');
    assert.equal('threshold' in v.automodRuleInvites, false);
    assert.equal(v.captchaTemplate.embed.title, fields.DEFAULT_CAPTCHA_PANEL.embed.title);
    assert.deepEqual(v.captchaVerifyButton, { label: 'Verify', style: 'SUCCESS' });
    assert.deepEqual(v.rankCardStyle, { ...fields.DEFAULT_RANK_CARD_STYLE });
    assert.equal(v.welcomeCardStyle.enabled, true);
    assert.deepEqual(v.infractionEscalation, []);
    assert.equal(v.economyMaxBalance, 1_000_000_000);
  });

  test('readSettingsValues: stored shapes, legacy text rules and junk', () => {
    const v = fields.readSettingsValues({
      welcomeMessage: null, welcomeEmbed: { title: 'Custom', card: false }, welcomeUseEmbed: true,
      goodbyeMessage: null, goodbyeUseEmbed: true, levelUpMessage: '   ',
      disabledLogEvents: ['voiceJoin', 'notAnEvent', 'messageDelete'], timezone: 'Not/AZone', currencyEmoji: 'x',
      automodConfig: { version: 2, rules: { caps: { action: 'kick', threshold: 80 } }, badWords: ['ok', 'x', 'fine word'], allowedDomains: [] },
      infractionEscalation: [{ id: 2, warnCount: 5, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom: null },
        { warnCount: 3, action: 'BAN', duration: 600 }],
      captchaTemplate: { content: 'Press it', embed: null, buttons: [], verifyButton: { label: 'Go', style: 'DANGER' } },
    });
    assert.equal(v.welcomeMessage, '', 'a custom embed without stored text sends no text line');
    assert.equal(v.welcomeCardStyle.enabled, false, 'the legacy card flag');
    assert.equal(v.goodbyeMessage, '');
    assert.equal(v.levelUpMessage, fields.DEFAULT_TEXTS.levelUpMessage);
    assert.deepEqual(v.logEvents, Object.keys(fields.LOG_EVENTS).filter((k) => k !== 'voiceJoin' && k !== 'messageDelete'),
      'every known event except the switched-off ones');
    assert.equal(v.timezone, 'UTC');
    assert.equal(v.currencyEmoji, '🪙');
    assert.equal(v.automodRuleCaps.action, 'kick');
    assert.equal(v.automodRuleCaps.threshold, 80);
    assert.deepEqual(v.automodBadWords, ['ok', 'fine word'], 'one bad entry never hides the rest');
    assert.deepEqual(v.automodAllowedDomains, [], 'an explicit empty allowlist stays empty');
    assert.deepEqual(v.infractionEscalation, [
      { warnCount: 3, withinSeconds: null, action: 'TEMPBAN', durationSeconds: 600 },
      { warnCount: 5, withinSeconds: null, action: 'KICK', durationSeconds: null },
    ]);
    assert.equal(v.captchaTemplate.content, 'Press it');
    assert.deepEqual(v.captchaVerifyButton, { label: 'Go', style: 'DANGER' });
  });

  test('validateSettingsChanges normalises like the commands', () => {
    const r = fields.validateSettingsChanges({
      changes: {
        timezone: 'america/new_york', customPrefix: 'OFF', embedColor: '#5865f2', xpMultiplier: 1.234,
        automodBadWords: ['  BAD   word ', 'bad word', 'ｆｕｌｌ'], automodBlockedDomains: ['https://www.Example.com/x', 'example.com'],
        logEvents: ['voiceJoin', 'messageDelete'], currencyName: 'gold    bars', levelUpMessage: '  ', birthdayMessage: '  Hi {user}  ',
        welcomeMessage: '  keep spaces  ', welcomeEmbed: { title: '', description: '' },
        welcomeCardStyle: { enabled: true, background: '#AABBCC', backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: 0.456 },
        xpBoostRoles: [{ roleId: RL.low, multiplier: 1.255 }],
        timezoneRoles: [{ roleId: RL.mid, zone: 'america/chicago' }, { roleId: RL.low, zone: 'Asia/Calcutta' }],
      },
    });
    assert.equal(r.ok, true, r.message);
    const c = r.changes;
    assert.equal(c.timezone, 'America/New_York');
    assert.equal(c.customPrefix, '');
    assert.equal(c.embedColor, 0x5865f2);
    assert.equal(c.xpMultiplier, 1.23);
    assert.deepEqual(c.automodBadWords, ['bad word', 'full']);
    assert.deepEqual(c.automodBlockedDomains, ['example.com']);
    assert.deepEqual(c.logEvents, ['messageDelete', 'voiceJoin']);
    assert.deepEqual(fields.toStoredChanges({ logEvents: c.logEvents }).disabledLogEvents,
      Object.keys(fields.LOG_EVENTS).filter((k) => k !== 'messageDelete' && k !== 'voiceJoin'), 'stored as the switched-off list');
    assert.equal(c.currencyName, 'gold bars');
    assert.equal(c.levelUpMessage, null, 'blank = the default text');
    assert.equal(c.birthdayMessage, 'Hi {user}');
    assert.equal(c.welcomeMessage, '  keep spaces  ', 'welcome texts are stored as typed');
    assert.equal(c.welcomeEmbed, null, 'an empty embed means the default');
    assert.equal(c.welcomeCardStyle.background, '#AABBCC');
    assert.deepEqual(c.xpBoostRoles, [{ roleId: RL.low, multiplier: 1.25 }]);
    assert.deepEqual(c.timezoneRoles, [{ roleId: RL.mid, zone: 'America/Chicago' }, { roleId: RL.low, zone: 'Asia/Kolkata' }]);
    assert.deepEqual(r.refs.roles.map((x) => [x.id, x.path]).sort((a, b) => a[1].localeCompare(b[1])),
      [[RL.mid, 'timezoneRoles.0.roleId'], [RL.low, 'timezoneRoles.1.roleId'], [RL.low, 'xpBoostRoles.0.roleId']]);
  });

  test('validateSettingsChanges refuses what the commands refuse, naming the path', () => {
    const tier = (o) => ({ warnCount: 3, withinSeconds: null, action: 'TIMEOUT', durationSeconds: 600, ...o });
    const cases = [
      [{ xpMin: 0 }, 'xpMin'],
      [{ xpMax: 1001 }, 'xpMax'],
      [{ xpCooldown: 4 }, 'xpCooldown'],
      [{ voiceXpRate: 51 }, 'voiceXpRate'],
      [{ birthdayHour: 24 }, 'birthdayHour'],
      [{ autoroleDelay: 604_801 }, 'autoroleDelay'],
      [{ captchaTimeoutMinutes: 3 }, 'captchaTimeoutMinutes'],
      [{ dailyMax: 1_000_001 }, 'dailyMax'],
      [{ economyMaxBalance: 0 }, 'economyMaxBalance'],
      [{ embedColor: 0 }, 'embedColor'],
      [{ customPrefix: 'a b' }, 'customPrefix'],
      [{ customPrefix: '!!!!!!' }, 'customPrefix'],
      [{ customPrefix: '<@' }, 'customPrefix'],
      [{ timezone: 'EST' }, 'timezone'],
      [{ timezone: '+02:00' }, 'timezone'],
      [{ currencyName: 'a*b' }, 'currencyName'],
      [{ currencyName: '' }, 'currencyName'],
      [{ currencyEmoji: 'x' }, 'currencyEmoji'],
      [{ currencyEmoji: '1️⃣' }, 'currencyEmoji'],
      [{ achievementMessage: '{rewards}' }, 'achievementMessage'],
      [{ levelUpMessage: 'x'.repeat(1001) }, 'levelUpMessage'],
      [{ welcomeMessage: 'x'.repeat(2001) }, 'welcomeMessage'],
      [{ automodBadWords: ['fine', '!'] }, 'automodBadWords.1'],
      [{ automodBadWordPatterns: ['.*'] }, 'automodBadWordPatterns.0'],
      [{ automodBadWordPatterns: ['ok', '(a+)+$'] }, 'automodBadWordPatterns.1'],
      [{ automodBlockedDomains: ['not a domain'] }, 'automodBlockedDomains.0'],
      [{ automodRuleCaps: { action: 'off', threshold: 49 } }, 'automodRuleCaps.threshold'],
      [{ automodRuleInvites: { action: 'kick', threshold: 3 } }, 'automodRuleInvites.threshold'],
      [{ automodRuleSpam: { action: 'explode' } }, 'automodRuleSpam.action'],
      [{ automodRuleSpam: { timeoutSeconds: 59 } }, 'automodRuleSpam.timeoutSeconds'],
      [{ logEvents: ['messageDelete', 'nope'] }, 'logEvents.1'],
      [{ infractionEscalation: [tier({ action: 'KICK', durationSeconds: 60 })] }, 'infractionEscalation.0.durationSeconds'],
      [{ infractionEscalation: [tier({ durationSeconds: null })] }, 'infractionEscalation.0.durationSeconds'],
      [{ infractionEscalation: [tier({ durationSeconds: 2_419_201 })] }, 'infractionEscalation.0.durationSeconds'],
      [{ infractionEscalation: [tier({ action: 'TEMPBAN', durationSeconds: null })] }, 'infractionEscalation.0.durationSeconds'],
      [{ infractionEscalation: [tier({}), tier({ action: 'BAN', durationSeconds: null })] }, 'infractionEscalation.1.warnCount'],
      [{ infractionEscalation: [tier({ withinSeconds: 59 })] }, 'infractionEscalation.0.withinSeconds'],
      [{ infractionEscalation: Array.from({ length: 11 }, (_, i) => tier({ warnCount: i + 1 })) }, 'infractionEscalation'],
      [{ xpBoostRoles: [{ roleId: RL.low, multiplier: 2 }, { roleId: RL.low, multiplier: 3 }] }, 'xpBoostRoles.1.roleId'],
      [{ xpBoostRoles: [{ roleId: RL.low, multiplier: 1 }] }, 'xpBoostRoles.0.multiplier'],
      [{ timezoneRoles: [{ roleId: RL.low, zone: 'UTC' }, { roleId: RL.low, zone: 'Asia/Tokyo' }] }, 'timezoneRoles.1.roleId'],
      [{ timezoneRoles: [{ roleId: RL.low, zone: 'EST' }] }, 'timezoneRoles.0.zone'],
      [{ timezoneRoles: [{ roleId: RL.low, zone: 'Mars/Olympus' }] }, 'timezoneRoles.0.zone'],
      [{ timezoneRoles: Array.from({ length: 26 }, (_, i) => ({ roleId: `7000000000000000${String(i).padStart(2, '0')}`, zone: 'UTC' })) }, 'timezoneRoles'],
      [{ welcomeCardStyle: { overlayOpacity: 1.5 } }, 'welcomeCardStyle.overlayOpacity'],
      [{ welcomeCardStyle: { backgroundUrl: 'ftp://x.example/a.png' } }, 'welcomeCardStyle.backgroundUrl'],
      [{ welcomeCardStyle: { v: 1 } }, 'welcomeCardStyle.v'],
      [{ rankCardStyle: { overlayOpacity: 0.95 } }, 'rankCardStyle.overlayOpacity'],
      [{ rankCardStyle: { backgroundUrl: `https://x.example/${'a'.repeat(500)}` } }, 'rankCardStyle.backgroundUrl'],
      [{ captchaVerifyButton: { label: '', style: 'SUCCESS' } }, 'captchaVerifyButton.label'],
      [{ captchaVerifyButton: { label: 'Go', style: 'PINK' } }, 'captchaVerifyButton.style'],
      [{ captchaTemplate: { content: null, embed: null, buttons: [] } }, 'captchaTemplate'],
      [{ welcomeChannelId: null }, 'welcomeChannelId'],
      [{ welcomeChannelId: 'general' }, 'welcomeChannelId'],
      [{ modRoleIds: Array.from({ length: 26 }, (_, i) => String(400000000000000100n + BigInt(i))) }, 'modRoleIds'],
      [{ levelingEnabled: true }, 'levelingEnabled'],
      [{ captchaChannelId: CH.text }, 'captchaChannelId'],
    ];
    for (const [changes, path] of cases) {
      const r = fields.validateSettingsChanges({ changes });
      assert.equal(r.ok, false, `accepted ${JSON.stringify(changes).slice(0, 120)}`);
      assert.equal(r.path, path, `${JSON.stringify(changes).slice(0, 120)}: ${r.message}`);
      assert.equal(typeof r.message, 'string');
    }
    for (const body of [null, [], {}, { changes: [] }, { changes: {} }, { changes: { modEnabled: true }, extra: 1 },
      JSON.parse('{"changes":{"__proto__":true}}'),
      { changes: Object.fromEntries(Array.from({ length: fields.MAX_SETTINGS_CHANGES + 1 }, (_, i) => [`f${i}`, 1])) }]) {
      assert.equal(fields.validateSettingsChanges(body).ok, false, JSON.stringify(body)?.slice(0, 80));
    }
  });

  test('checkCrossFields enforces the multi-column rules of the commands, only for changed fields', () => {
    const base = { xpMin: 15, xpMax: 25, dailyMin: 50, dailyMax: 120, workMin: 100, workMax: 300, economyStartingBalance: 1000,
      economyMaxBalance: 1_000_000_000, levelUpMode: 'CURRENT', levelUpChannelId: null, achievementAnnounceMode: 'CURRENT',
      achievementChannelId: null, modRoleIds: [RL.mid], immuneRoleIds: [], muteRoleId: null, liveRoleId: RL.low, liveRoleRequiredRoleId: null };
    const check = (patch, changes) => fields.checkCrossFields({ ...base, ...patch }, changes);
    assert.equal(check({}, {}), null);
    assert.deepEqual(check({ xpMin: 30 }, { xpMin: 30 })?.path, 'xpMin');
    assert.deepEqual(check({ xpMax: 10 }, { xpMax: 10 })?.path, 'xpMax');
    assert.equal(check({ xpMin: 30, xpMax: 40 }, { xpMin: 30, xpMax: 40 }), null);
    assert.equal(check({ xpMin: 30 }, { modEnabled: false }), null, 'an old inconsistency never blocks an unrelated save');
    assert.equal(check({ dailyMin: 200 }, { dailyMin: 200 })?.path, 'dailyMin');
    assert.equal(check({ workMax: 50 }, { workMax: 50 })?.path, 'workMax');
    assert.equal(check({ economyMaxBalance: 500 }, { economyMaxBalance: 500 })?.path, 'economyMaxBalance');
    assert.equal(check({ levelUpMode: 'CHANNEL' }, { levelUpMode: 'CHANNEL' })?.path, 'levelUpMode');
    assert.equal(check({ levelUpMode: 'CHANNEL', levelUpChannelId: CH.text }, { levelUpMode: 'CHANNEL', levelUpChannelId: CH.text }), null);
    assert.equal(check({ achievementAnnounceMode: 'CHANNEL' }, { achievementAnnounceMode: 'CHANNEL' })?.path, 'achievementAnnounceMode');
    assert.equal(check({ captchaRoleId: RL.mid }, { captchaRoleId: RL.mid })?.path, 'captchaRoleId', 'a moderator role');
    assert.equal(check({ captchaRoleId: RL.low, muteRoleId: RL.low }, { captchaRoleId: RL.low })?.path, 'captchaRoleId', 'the mute role');
    assert.equal(check({ captchaRoleId: RL.low }, { captchaRoleId: RL.low }), null);
    assert.equal(check({ liveRoleRequiredRoleId: RL.low }, { liveRoleRequiredRoleId: RL.low })?.path, 'liveRoleRequiredRoleId');
    const mute = [{ warnCount: 3, withinSeconds: null, action: 'MUTE', durationSeconds: null }];
    assert.equal(check({ infractionEscalation: mute }, { infractionEscalation: mute })?.path, 'infractionEscalation.0.action');
    assert.equal(check({ infractionEscalation: mute, muteRoleId: RL.low }, { infractionEscalation: mute }), null);
    const shortMute = [{ ...mute[0], durationSeconds: 3600 }];
    assert.equal(check({ infractionEscalation: shortMute }, { infractionEscalation: shortMute }), null, 'a short mute is a timeout');
    const both = { version: 2, blockedDomains: ['a.com'], allowedDomains: ['a.com'] };
    assert.equal(check({ automodConfig: both }, { automodBlockedDomains: ['a.com'], automodAllowedDomains: ['a.com'] })?.path, 'automodAllowedDomains');
  });

  test('toStoredChanges merges JSON parts into the stored row', () => {
    const row = {
      automodConfig: { version: 2, rules: { caps: { action: 'kick', threshold: 80 } }, badWords: ['old'], allowedDomains: ['tenor.com', 'b.com'],
        honeypot: { channelIds: [CH.text], action: 'ban', timeoutSeconds: 600 } },
      infractionEscalation: [
        { id: 4, warnCount: 3, withinSeconds: null, action: 'TIMEOUT', durationSeconds: 600, countFrom: '2026-01-01T00:00:00.000Z' },
        { id: 7, warnCount: 9, withinSeconds: null, action: 'BAN', durationSeconds: null, countFrom: null },
      ],
      captchaTemplate: { content: 'Hello', embed: null, buttons: [], verifyButton: { label: 'Go', style: 'DANGER' } },
    };
    const valid = (changes) => {
      const r = fields.validateSettingsChanges({ changes });
      assert.equal(r.ok, true, r.message);
      return r.changes;
    };

    // Escalation: a tier with the same warnings and window keeps its id and countFrom, new ones get the next id; sorted.
    const tiers = fields.toStoredChanges(valid({ infractionEscalation: [
      { warnCount: 5, withinSeconds: 3600, action: 'BAN', durationSeconds: null },
      { warnCount: 3, withinSeconds: null, action: 'KICK', durationSeconds: null },
    ] }), row).infractionEscalation;
    assert.deepEqual(tiers, [
      { id: 4, warnCount: 3, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom: '2026-01-01T00:00:00.000Z' },
      { id: 8, warnCount: 5, withinSeconds: 3600, action: 'BAN', durationSeconds: null, countFrom: null },
    ]);

    // Automod: one rule changes, the rest of the config stays; a blocked domain leaves the allowlist.
    const rule = { action: 'delete', threshold: 75, timeoutSeconds: 600, roleMode: 'except', roleIds: [], channelMode: 'only', channelIds: [CH.text] };
    const stored = fields.toStoredChanges(valid({ automodRuleCaps: rule, automodBlockedDomains: ['b.com'] }), row);
    assert.deepEqual(Object.keys(stored), ['automodConfig']);
    assert.deepEqual(stored.automodConfig.rules.caps, rule);
    assert.equal(stored.automodConfig.rules.spam.action, 'off');
    assert.deepEqual(stored.automodConfig.badWords, ['old']);
    assert.deepEqual(stored.automodConfig.blockedDomains, ['b.com']);
    assert.deepEqual(stored.automodConfig.allowedDomains, ['tenor.com'], 'moved to the blocked list');
    assert.equal(stored.automodConfig.version, 2);
    assert.deepEqual(stored.automodConfig.honeypot, { channelIds: [CH.text], action: 'ban', timeoutSeconds: 600 },
      'saving another automod field keeps the honeypot');
    const honeypot = { channelIds: [], action: 'timeout', timeoutSeconds: 3600 };
    const hp = fields.toStoredChanges(valid({ automodHoneypot: honeypot }), row);
    assert.deepEqual(hp.automodConfig.honeypot, honeypot);
    assert.deepEqual(hp.automodConfig.badWords, ['old']);
    const together = fields.toStoredChanges(valid({ automodBlockedDomains: ['b.com'], automodAllowedDomains: ['b.com'] }), row);
    assert.notEqual(fields.checkCrossFields({ ...row, ...together }, { automodBlockedDomains: [], automodAllowedDomains: [] }), null);

    // Captcha: the message and the Verify button share the column without overwriting each other.
    const button = fields.toStoredChanges(valid({ captchaVerifyButton: { label: 'Verify me', style: 'PRIMARY' } }), row).captchaTemplate;
    assert.equal(button.content, 'Hello');
    assert.deepEqual(button.verifyButton, { label: 'Verify me', style: 'PRIMARY' });
    const message = fields.toStoredChanges(valid({ captchaTemplate: { content: 'New text', embed: null, buttons: [] } }), row).captchaTemplate;
    assert.equal(message.content, 'New text');
    assert.deepEqual(message.verifyButton, { label: 'Go', style: 'DANGER' });

    // Card styles and embeds are stored the way the commands write them.
    const card = fields.toStoredChanges(valid({ welcomeCardStyle: {
      enabled: false, background: '#AABBCC', backgroundUrl: 'https://img.example/bg.png', textColor: null, accentColor: '#112233', overlayOpacity: 0.5,
    } })).welcomeCardStyle;
    assert.deepEqual(card, { v: 1, enabled: false, background: '#aabbcc', backgroundUrl: 'https://img.example/bg.png', textColor: null,
      accentColor: '#112233', overlayOpacity: 0.5 });
    const embed = fields.toStoredChanges(valid({ welcomeEmbed: { title: 'Hi', description: 'there', footer: { text: 'f', iconUrl: null }, timestamp: false } }));
    assert.deepEqual(embed.welcomeEmbed, { title: 'Hi', description: 'there', footer: { text: 'f' } });
    assert.deepEqual(fields.toStoredChanges(valid({ rankCardStyle: { overlayOpacity: 0.333 } })).rankCardStyle,
      { ...fields.DEFAULT_RANK_CARD_STYLE, overlayOpacity: 0.33 });
  });
});

// ── Parity with the bot's own rules ──────────────────────────────────────────

describe('settings registry agrees with the bot services', () => {
  let svc;
  before(async () => {
    const load = (p) => import(`../src/${p}`);
    svc = {
      welcome: await load('services/welcomeService.js'),
      xp: await load('services/xpService.js'),
      birthday: await load('services/birthdayService.js'),
      achievement: await load('services/achievementService.js'),
      automod: await load('services/automodService.js'),
      logging: await load('services/loggingService.js'),
      moderation: await load('services/moderationService.js'),
      captcha: await load('services/captchaService.js'),
      economy: await load('services/economyService.js'),
      profileCard: await load('utils/profileCard.js'),
      settingsCmd: await load('commands/admin/settings.js'),
      redis: (await load('services/redis.js')).default,
    };
  });
  after(() => svc?.redis.disconnect());

  test('default texts, embeds and the captcha panel', () => {
    const { DEFAULT_TEXTS, DEFAULT_EMBEDS } = fields;
    assert.equal(DEFAULT_TEXTS.welcomeMessage, svc.welcome.DEFAULT_WELCOME_MESSAGE);
    assert.equal(DEFAULT_TEXTS.welcomeDmMessage, svc.welcome.DEFAULT_WELCOME_DM_MESSAGE);
    assert.equal(DEFAULT_TEXTS.goodbyeMessage, svc.welcome.DEFAULT_GOODBYE_MESSAGE);
    assert.equal(DEFAULT_TEXTS.levelUpMessage, svc.xp.DEFAULT_LEVELUP_MESSAGE);
    assert.equal(DEFAULT_TEXTS.birthdayMessage, svc.birthday.DEFAULT_BIRTHDAY_MESSAGE);
    assert.equal(DEFAULT_TEXTS.birthdayMessageAge, svc.birthday.DEFAULT_BIRTHDAY_MESSAGE_AGE);
    assert.equal(DEFAULT_TEXTS.achievementMessage, svc.achievement.DEFAULT_ACHIEVEMENT_MESSAGE);
    assert.deepEqual(DEFAULT_EMBEDS.welcomeEmbed, { ...svc.welcome.DEFAULT_WELCOME_EMBED });
    assert.deepEqual(DEFAULT_EMBEDS.goodbyeEmbed, { ...svc.welcome.DEFAULT_GOODBYE_EMBED });
    assert.deepEqual(JSON.parse(JSON.stringify(fields.DEFAULT_CAPTCHA_PANEL)), svc.captcha.defaultPanelTemplate());
    assert.deepEqual(fields.DEFAULT_VERIFY_BUTTON, { ...svc.captcha.DEFAULT_VERIFY_BUTTON });
    assert.deepEqual(fields.DEFAULT_RANK_CARD_STYLE, { ...svc.profileCard.DEFAULT_RANK_CARD_STYLE });
    assert.deepEqual(fields.SETTINGS_FIELDS.captchaVerifyButton.fields[1].options.map((o) => o.value).sort(),
      Object.keys(svc.captcha.BUTTON_STYLES).sort());
  });

  test('number limits', () => {
    const L = fields.NUMBER_LIMITS;
    assert.equal(L.xpRangeMax, svc.xp.XP_RANGE_MAX);
    assert.equal(L.economyRangeMax, svc.economy.RANGE_MAX);
    assert.equal(L.workCooldownMaxMinutes, svc.economy.WORK_COOLDOWN_MAX_MINUTES);
    assert.equal(L.autoroleDelayMaxSeconds, svc.welcome.MAX_AUTOROLE_DELAY_SECONDS);
    assert.equal(L.captchaTimeoutMinMinutes, svc.captcha.TIMEOUT_MIN_MINUTES);
    assert.equal(L.captchaTimeoutMaxMinutes, svc.captcha.TIMEOUT_MAX_MINUTES);
    assert.equal(L.timeoutMaxSeconds * 1000, svc.moderation.TIMEOUT_MAX_MS);
    assert.equal(L.timedActionMaxSeconds * 1000, svc.moderation.TIMED_ACTION_MAX_MS);
    assert.deepEqual(fields.SETTINGS_FIELDS.levelUpMode.options.map((o) => o.value), [...svc.xp.ANNOUNCE_MODES]);
    for (const n of [-1, 0, 1, 4, 5, 6, 10_080, 10_081]) {
      const ours = fields.validateSettingsChanges({ changes: { captchaTimeoutMinutes: n } }).ok;
      assert.equal(ours, svc.captcha.validateTimeoutMinutes(n).minutes !== undefined, `captcha timeout ${n}`);
    }
  });

  test('automod rules, actions, defaults and normalisers', () => {
    const { RULES, RULE_KEYS, ACTIONS, ACTION_LABELS } = svc.automod;
    assert.deepEqual(fields.AUTOMOD_RULE_KEYS, [...RULE_KEYS]);
    for (const key of RULE_KEYS) {
      const ours = fields.AUTOMOD_RULES[key];
      assert.equal(ours.label, RULES[key].label, key);
      assert.equal(ours.description, RULES[key].description, key);
      assert.deepEqual(ours.threshold, RULES[key].threshold ? { ...RULES[key].threshold } : null, key);
    }
    assert.deepEqual(fields.AUTOMOD_ACTIONS.map((a) => a.value), [...ACTIONS]);
    for (const a of fields.AUTOMOD_ACTIONS) assert.equal(a.label, ACTION_LABELS[a.value]);
    assert.deepEqual(fields.AUTOMOD_DEFAULTS, {
      allowedDomains: [...svc.automod.DEFAULT_ALLOWED_DOMAINS], timeoutSeconds: svc.automod.DEFAULT_TIMEOUT_SECONDS,
      minTimeoutSeconds: svc.automod.MIN_TIMEOUT_SECONDS, maxTimeoutSeconds: svc.automod.MAX_TIMEOUT_SECONDS,
    });
    assert.deepEqual(fields.AUTOMOD_HONEYPOT.actions.map((a) => a.value), [...svc.automod.HONEYPOT_ACTIONS]);
    for (const a of fields.AUTOMOD_HONEYPOT.actions) assert.equal(a.label, svc.automod.HONEYPOT_ACTION_LABELS[a.value]);
    assert.equal(fields.AUTOMOD_HONEYPOT.timeoutSeconds, svc.automod.HONEYPOT_DEFAULT_TIMEOUT_SECONDS);
    assert.equal(fields.AUTOMOD_HONEYPOT.maxChannels, svc.automod.HONEYPOT_CAP);
    for (const w of ['  Hello  World ', 'ｆｕｌｌ', 'a', '!!', 'x'.repeat(65), 'zero​width', 'ok', 42]) {
      assert.equal(fields.normalizeAutomodWord(w), svc.automod.normalizeWord(w), String(w));
    }
    for (const d of ['Example.COM', 'https://www.example.com/path', '*.example.com', 'bücher.de', 'nope', '', 'a..b', 'example.com:8080', 'xn--bcher-kva.de']) {
      assert.equal(fields.normalizeAutomodDomain(d), svc.automod.normalizeDomain(d), d);
    }
    // A config the bot normalised reads back unchanged.
    const samples = [
      null,
      { version: 2, rules: { caps: { action: 'kick', threshold: 80, roleMode: 'only', roleIds: [RL.low] } }, badWords: ['foo', 'bar baz'],
        badWordPatterns: ['f+o+'], blockedDomains: ['evil.com'], allowedDomains: [], exemptRoleIds: [RL.mid], exemptChannelIds: [CH.text],
        honeypot: { channelIds: [CH.text], action: 'kick', timeoutSeconds: 120 } },
      { filterInvites: true, automodAction: 'KICK', badWords: ['legacy'] },
    ];
    for (const raw of samples) {
      const cfg = svc.automod.normalizeConfig(raw);
      assert.deepEqual(fields.automodConfigOf({ automodConfig: cfg }), cfg, JSON.stringify(raw));
    }
  });

  test('escalation tiers normalise like normalizeEscalationTiers', () => {
    const samples = [
      null, 'x', [],
      [{ warnCount: 3, action: 'BAN', duration: 600 }, { warnCount: 5, action: 'kick' }, { warnCount: 0, action: 'BAN' }],
      [{ id: 2, warnCount: 3, withinSeconds: 600, action: 'TIMEOUT', durationSeconds: 10, countFrom: null },
        { id: 2, warnCount: 4, withinSeconds: null, action: 'MUTE', durationSeconds: null, countFrom: '2026-02-01T00:00:00.000Z' },
        { warnCount: 3, withinSeconds: 600, action: 'TEMPBAN', durationSeconds: 99_999_999 },
        { warnCount: 6, withinSeconds: 30, action: 'TIMEOUT', durationSeconds: null }],
      Array.from({ length: 12 }, (_, i) => ({ id: i + 1, warnCount: i + 1, withinSeconds: null, action: 'KICK', durationSeconds: 5 })),
    ];
    for (const raw of samples) assert.deepEqual(fields.normalizeEscalation(raw), svc.moderation.normalizeEscalationTiers(raw), JSON.stringify(raw));
    assert.deepEqual(fields.TIER_ACTIONS.map((a) => a.value), [...svc.moderation.TIER_ACTIONS]);
    for (const a of fields.TIER_ACTIONS) assert.equal(a.label, svc.moderation.TIER_ACTION_LABEL[a.value]);
  });

  test('log events', () => {
    const { LOG_EVENTS } = svc.logging;
    assert.deepEqual(Object.keys(fields.LOG_EVENTS), Object.keys(LOG_EVENTS));
    for (const [key, label] of Object.entries(fields.LOG_EVENTS)) assert.equal(label, LOG_EVENTS[key].label, key);
  });

  test('prefix, time zone, currency and embed rules', () => {
    const ok = (changes) => fields.validateSettingsChanges({ changes });
    for (const p of ['!', '?!', 'ab', '🔥', '!!!!!', '!!!!!!', 'a b', '<', '@x', '#', ':', '/', '\\', 'off', 'OFF', '\u0007', '`']) {
      const theirs = svc.settingsCmd.validatePrefix(p);
      const ours = ok({ customPrefix: p });
      assert.equal(ours.ok, theirs.ok, `prefix ${JSON.stringify(p)}`);
      if (theirs.ok) assert.equal(ours.changes.customPrefix, theirs.off ? '' : theirs.prefix, `prefix ${JSON.stringify(p)}`);
    }
    for (const z of ['UTC', 'Etc/UTC', 'Europe/Berlin', 'europe/berlin', 'America/New_York', 'Asia/Calcutta', 'Europe/Kiev', 'US/Eastern',
      'EST', '+02:00', 'Mars/Phobos']) {
      const theirs = svc.settingsCmd.normalizeTimeZone(z);
      const ours = ok({ timezone: z });
      assert.equal(ours.ok, theirs.ok, `zone ${z}`);
      if (theirs.ok) assert.equal(ours.changes.timezone, theirs.zone, `zone ${z}`);
    }
    for (const n of ['coins', 'gold bars', '  spaced   out  ', 'a*b', 'x'.repeat(33), '', '@here', 'new\nline']) {
      const theirs = svc.economy.validateCurrencyName(n);
      const ours = ok({ currencyName: n });
      assert.equal(ours.ok, theirs.ok, `currency ${JSON.stringify(n)}`);
      if (theirs.ok) assert.equal(ours.changes.currencyName, theirs.value);
    }
    for (const e of ['🪙', '💎', '🇩🇪', '👍🏽', `<:coin:${'1'.repeat(18)}>`, `<a:spin:${'2'.repeat(18)}>`, 'x', '1️⃣', '<:c:1>']) {
      assert.equal(ok({ currencyEmoji: e }).ok, svc.economy.validateCurrencyEmoji(e).ok, `emoji ${e}`);
    }
    for (const spec of [null, {}, { title: ' ' }, { title: 'x' }, { footer: { text: 'f' } }, { image: 'https://a.example/x.png' },
      { fields: [{ name: 'a', value: 'b' }] }, { author: { name: 'me' } }, { thumbnail: 'https://a.example/x.png' }]) {
      assert.equal(fields.embedHasContent(spec), svc.welcome.hasEmbedContent(spec), JSON.stringify(spec));
    }
  });
});

// ── Configuration ────────────────────────────────────────────────────────────

// ── Front end ────────────────────────────────────────────────────────────────

describe('front end', () => {
  const PUBLIC = join(ROOT, 'src/dashboard/public');
  const sources = readdirSync(PUBLIC).filter((f) => f.endsWith('.js')).map((f) => [f, readFileSync(join(PUBLIC, f), 'utf8')]);

  test('never parses HTML strings, evaluates code or sets inline styles (strict CSP, no XSS sinks)', () => {
    const banned = [
      [/\.(?:inner|outer)HTML\b/, 'innerHTML/outerHTML'],
      [/\binsertAdjacentHTML\b/, 'insertAdjacentHTML'],
      [/\bdocument\.write\b/, 'document.write'],
      [/\bcreateContextualFragment\b/, 'createContextualFragment'],
      [/\beval\s*\(|\bnew Function\s*\(/, 'eval / new Function'],
      [/setAttribute\(\s*['"]style['"]|['"]?\bstyle['"]?\s*:\s*['"`]/, 'an inline style attribute'],
      [/\bprocess\.env\b/, 'process.env'],
    ];
    const problems = [];
    for (const [file, code] of sources) {
      for (const [re, what] of banned) if (re.test(code)) problems.push(`${file}: ${what}`);
    }
    assert.deepEqual(problems, []);
    assert.ok(sources.length >= 5, 'the page modules were found');
  });

  test('index.html has no inline script or style and loads only files the server serves', () => {
    const html = readFileSync(join(PUBLIC, 'index.html'), 'utf8');
    assert.equal(/<script(?![^>]*\bsrc=)[^>]*>/.test(html), false, 'inline <script>');
    assert.equal(/<style\b|\sstyle=/.test(html), false, 'inline style');
    for (const [, ref] of html.matchAll(/(?:src|href)="(\/[^"]*)"/g)) {
      assert.ok(existsSync(join(PUBLIC, ref.slice(1))), `${ref} is referenced but missing`);
    }
  });

  test('the landing page: no inline script or style, links only to served paths, filled from the catalog', async () => {
    const { renderLanding } = await import('../src/dashboard/landing.js');
    const template = readFileSync(join(PUBLIC, 'landing.html'), 'utf8');
    assert.equal(/<script(?![^>]*\bsrc=)[^>]*>/.test(template), false, 'inline <script>');
    assert.equal(/<style\b|\sstyle=/.test(template), false, 'inline style');
    const served = new Set(['/', '/dashboard', '/privacy', '/terms', '/docs', '/changelog', '/site.css', '/landing.css', '/landing.js',
      '/favicon.svg', '/demo/rank-card.png', '/demo/welcome-card.png']);
    for (const [, ref] of template.matchAll(/(?:src|href)="(\/[^"#]*)(?:#[^"]*)?"/g)) assert.ok(served.has(ref), `landing.html: ${ref} is not served`);
    assert.equal(/href="#"/.test(template), false, 'no dead links');
    for (const id of template.matchAll(/href="#([^"]+)"/g)) assert.ok(template.includes(`id="${id[1]}"`), `landing.html: #${id[1]} has no target`);

    const invite = 'https://discord.com/oauth2/authorize?client_id=123456789012345678&scope=bot+applications.commands';
    const changelog = readFileSync(join(ROOT, 'CHANGELOG.md'), 'utf8');
    const html = renderLanding(template, { inviteUrl: invite, contact: { kind: 'email', value: 'owner@example.com' }, supportUrl: 'https://discord.gg/abc', changelog });
    assert.equal(/<!--[a-z]|@@INVITE_URL@@/.test(html), false, 'every marker is filled');
    assert.ok(html.includes('<a href="https://discord.gg/abc">Support server</a>'), 'the support server link');
    assert.equal(renderLanding(template, {}).includes('Support server'), false, 'no support URL: no support link');
    assert.match(html, /<a class="lot-title" href="\/changelog#lot-\d{4}-\d{2}-\d{2}">/, 'the newest changelog entries');
    const commands = JSON.parse(readFileSync(join(ROOT, 'src', 'dashboard', 'commands.json'), 'utf8'));
    for (const c of commands) assert.ok(html.includes(`<code>/${c.name}</code>`), `/${c.name}: in the ingredients`);
    for (const [, src] of html.matchAll(/src="(\/demo\/[^"]+)"/g)) assert.ok(existsSync(join(PUBLIC, src.slice(1))), `${src} exists`);
    assert.ok(html.includes('href="https://discord.com/oauth2/authorize?client_id=123456789012345678&amp;scope=bot+applications.commands"'));
    assert.ok(html.includes('<a href="mailto:owner@example.com">Contact</a>'));
    const esc = (s) => s.replace(/&/g, '&amp;').replace(/'/g, '&#39;').replace(/"/g, '&quot;').replace(/</g, '&lt;');
    for (const key of MODULE_NAMES) {
      assert.ok(html.includes(`>${CODES[key]}<`), `${key}: code on the page`);
      assert.ok(html.includes(esc(MODULES[key].label)), `${key}: label on the page`);
      assert.ok(html.includes(esc(DESCRIPTIONS[key])), `${key}: description on the page`);
    }
    for (const c of CATEGORIES) assert.ok(html.includes(esc(c.label)), `${c.id}: category on the page`);
    assert.equal(renderLanding(template, {}).includes('<!--contact-->'), false, 'no contact: the marker is dropped');
    assert.throws(() => renderLanding('<!--code:nope-->'), /unknown module "nope"/);
    assert.throws(() => renderLanding('<!--no-such-marker-->'), /unknown marker/);
  });

  test('the docs and changelog pages: no inline script or style, served links, every module, command and entry', async () => {
    const { renderDocs, renderChangelog } = await import('../src/dashboard/landing.js');
    const { parseChangelog } = await import('../src/dashboard/changelog.js');
    const served = new Set(['/', '/dashboard', '/privacy', '/terms', '/docs', '/changelog', '/site.css', '/leaflet.css', '/favicon.svg']);
    const changelog = readFileSync(join(ROOT, 'CHANGELOG.md'), 'utf8');
    const commands = JSON.parse(readFileSync(join(ROOT, 'src', 'dashboard', 'commands.json'), 'utf8'));
    const esc = (s) => s.replace(/&/g, '&amp;').replace(/'/g, '&#39;').replace(/"/g, '&quot;').replace(/</g, '&lt;');
    for (const [name, render] of [['docs.html', renderDocs], ['changelog.html', renderChangelog]]) {
      const template = readFileSync(join(PUBLIC, name), 'utf8');
      assert.equal(/<script\b/.test(template), false, `${name}: <script>`);
      assert.equal(/<style\b|\sstyle=/.test(template), false, `${name}: inline style`);
      for (const [, ref] of template.matchAll(/(?:src|href)="(\/[^"#]*)(?:#[^"]*)?"/g)) assert.ok(served.has(ref), `${name}: ${ref} is not served`);
      const html = render(template, { inviteUrl: 'https://discord.com/oauth2/authorize?client_id=1', changelog });
      assert.equal(/<!--[a-z]|@@INVITE_URL@@/.test(html), false, `${name}: every marker is filled`);
    }
    const docs = renderDocs(readFileSync(join(PUBLIC, 'docs.html'), 'utf8'), {});
    for (const key of MODULE_NAMES) {
      assert.ok(docs.includes(`id="mod-${key}"`), `${key}: a section on /docs`);
      assert.ok(docs.includes(esc(DESCRIPTIONS[key])), `${key}: description on /docs`);
    }
    for (const c of commands) assert.ok(docs.includes(`id="cmd-${c.name}"`), `/${c.name}: an entry on /docs`);

    const { entries, intro } = parseChangelog(changelog);
    assert.ok(intro.length > 0 && entries.length > 0, 'CHANGELOG.md has an intro and dated entries');
    for (const e of entries) {
      assert.match(e.date, /^\d{4}-\d{2}-\d{2}$/);
      assert.ok(e.items.length > 0, `${e.date}: has items`);
    }
    assert.deepEqual([...entries].map((e) => e.date), [...entries].map((e) => e.date).sort().reverse(), 'newest first');
    const page = renderChangelog(readFileSync(join(PUBLIC, 'changelog.html'), 'utf8'), { changelog });
    for (const e of entries) assert.ok(page.includes(`id="lot-${e.date}"`), `${e.date}: on /changelog`);
    assert.equal(renderChangelog('<!--changelog-->', { changelog: '- <b>x</b> `y` [z](javascript:alert(1))' }).includes('<b>'), false, 'escaped');
    const md = parseChangelog('## 2026-01-02 · T\n- `a` **b** [c](https://example.com) <i>');
    assert.deepEqual(md.entries[0], { date: '2026-01-02', title: 'T', items: ['`a` **b** [c](https://example.com) <i>'] });
  });

  test('the privacy and terms pages: no script or inline style, a contact marker, served links, header and footer filled', async () => {
    const { renderLegal } = await import('../src/dashboard/landing.js');
    const served = new Set(['/', '/dashboard', '/privacy', '/terms', '/docs', '/changelog', '/site.css', '/leaflet.css', '/favicon.svg']);
    for (const name of ['privacy.html', 'terms.html']) {
      const html = readFileSync(join(PUBLIC, name), 'utf8');
      assert.equal(/<script\b/.test(html), false, `${name}: <script>`);
      assert.equal(/<style\b|\sstyle=/.test(html), false, `${name}: inline style`);
      assert.ok(html.includes('<!--contact-->'), `${name}: contact marker`);
      assert.ok(html.includes('<footer'), `${name}: the shared footer`);
      assert.equal(html.slice(html.indexOf('<footer')).includes('<!--contact-->'), false, `${name}: the footer leaves the contact to the text`);
      for (const [, ref] of html.matchAll(/(?:src|href)="(\/[^"#]*)(?:#[^"]*)?"/g)) assert.ok(served.has(ref), `${name}: ${ref} is not served`);
      // as server.js serves it
      const page = renderLegal(html.replaceAll('<!--contact-->', contactHtml(null)), { inviteUrl: 'https://discord.com/oauth2/authorize?client_id=1' });
      assert.equal(/<!--[a-z]|@@INVITE_URL@@/.test(page), false, `${name}: every marker is filled`);
    }
  });
});

describe('config', () => {
  test('reports every missing required key', () => {
    const r = loadDashboardConfig({});
    assert.equal(r.ok, false);
    assert.deepEqual(r.missing, [...REQUIRED_KEYS]);
    assert.equal(r.config.port, 8080);
    assert.equal(r.config.host, '0.0.0.0');
  });

  test('defaults and derived values', () => {
    const r = loadDashboardConfig(baseEnv());
    assert.equal(r.ok, true, JSON.stringify(r.invalid));
    assert.equal(r.config.url, 'http://localhost:8080');
    assert.equal(r.config.origin, 'http://localhost:8080');
    assert.equal(r.config.redirectUri, 'http://localhost:8080/auth/callback');
    assert.equal(r.config.secureCookies, false);
    assert.equal(r.config.botOwnerId, null);
    assert.equal(r.config.urlIsDefault, true);
    assert.equal(loadDashboardConfig({}).config.urlIsDefault, true);
  });

  test('urlIsDefault is false once DASHBOARD_URL is set, even to the default address', () => {
    for (const url of ['https://dash.example.com', 'http://localhost:8080']) {
      const r = loadDashboardConfig(baseEnv({ DASHBOARD_URL: url }));
      assert.equal(r.config.urlIsDefault, false, url);
      assert.equal(r.config.url, url);
    }
    assert.equal(loadDashboardConfig(baseEnv({ DASHBOARD_URL: '   ' })).config.urlIsDefault, true, 'blank counts as unset');
  });

  test('an https DASHBOARD_URL makes cookies secure; the trailing slash is dropped', () => {
    const r = loadDashboardConfig(baseEnv({ DASHBOARD_URL: 'https://dash.example.com/', DASHBOARD_PORT: '9000', BOT_OWNER_ID: OWNER_ID }));
    assert.equal(r.ok, true);
    assert.equal(r.config.url, 'https://dash.example.com');
    assert.equal(r.config.redirectUri, 'https://dash.example.com/auth/callback');
    assert.equal(r.config.secureCookies, true);
    assert.equal(r.config.port, 9000);
    assert.equal(r.config.botOwnerId, OWNER_ID);
  });

  test('invalid values are reported by key', () => {
    const cases = [
      [{ DASHBOARD_SESSION_SECRET: 'short' }, 'DASHBOARD_SESSION_SECRET'],
      [{ DASHBOARD_URL: 'ftp://example.com' }, 'DASHBOARD_URL'],
      [{ DASHBOARD_URL: 'not a url' }, 'DASHBOARD_URL'],
      [{ DASHBOARD_URL: 'https://example.com/dashboard' }, 'DASHBOARD_URL'],
      [{ DASHBOARD_URL: 'https://user:pw@example.com' }, 'DASHBOARD_URL'],
      [{ DASHBOARD_PORT: '70000' }, 'DASHBOARD_PORT'],
      [{ DASHBOARD_PORT: 'abc' }, 'DASHBOARD_PORT'],
      [{ CLIENT_ID: 'my-app' }, 'CLIENT_ID'],
      [{ DASHBOARD_TRUSTED_PROXIES: '10.0.1.30, cloudflared' }, 'DASHBOARD_TRUSTED_PROXIES'],
      [{ DASHBOARD_TRUSTED_PROXIES: '10.0.1.0/24' }, 'DASHBOARD_TRUSTED_PROXIES'],
    ];
    for (const [extra, key] of cases) {
      const r = loadDashboardConfig(baseEnv(extra));
      assert.equal(r.ok, false, `${JSON.stringify(extra)} was accepted`);
      assert.deepEqual(r.invalid.map((i) => i.key), [key]);
    }
    const owner = loadDashboardConfig(baseEnv({ BOT_OWNER_ID: 'me' }));
    assert.equal(owner.ok, true);
    assert.equal(owner.config.botOwnerId, null);
    assert.equal(owner.warnings.length, 1);
  });

  test('DASHBOARD_TRUSTED_PROXIES: a list of addresses, normalised and deduplicated; unset is empty', () => {
    assert.deepEqual(loadDashboardConfig(baseEnv()).config.trustedProxies, []);
    const r = loadDashboardConfig(baseEnv({ DASHBOARD_TRUSTED_PROXIES: ' 10.0.1.30,10.0.1.31 , ::FFFF:10.0.1.30,,fd7a::1 ' }));
    assert.equal(r.ok, true, JSON.stringify(r.invalid));
    assert.deepEqual(r.config.trustedProxies, ['10.0.1.30', '10.0.1.31', 'fd7a::1']);
  });

  test('DASHBOARD_CONTACT: an email address or an https URL; anything else is a warning and no contact', () => {
    assert.equal(loadDashboardConfig(baseEnv()).config.contact, null);
    assert.deepEqual(loadDashboardConfig(baseEnv({ DASHBOARD_CONTACT: 'owner@example.com' })).config.contact,
      { kind: 'email', value: 'owner@example.com' });
    assert.deepEqual(loadDashboardConfig(baseEnv({ DASHBOARD_CONTACT: 'https://example.com/contact' })).config.contact,
      { kind: 'url', value: 'https://example.com/contact' });
    for (const bad of ['http://example.com', 'javascript:alert(1)', 'owner at example', '<b>@x.y', 'https://u:p@example.com']) {
      const r = loadDashboardConfig(baseEnv({ DASHBOARD_CONTACT: bad }));
      assert.equal(r.ok, true, bad);
      assert.equal(r.config.contact, null, bad);
      assert.equal(r.warnings.length, 1, bad);
    }
  });

  test('DASHBOARD_SUPPORT_URL: an https URL; anything else is a warning and no support link', () => {
    assert.equal(loadDashboardConfig(baseEnv()).config.supportUrl, null);
    assert.equal(loadDashboardConfig(baseEnv({ DASHBOARD_SUPPORT_URL: 'https://discord.gg/abc123' })).config.supportUrl,
      'https://discord.gg/abc123');
    for (const bad of ['http://discord.gg/abc123', 'owner@example.com', 'javascript:alert(1)', 'https://u:p@discord.gg/x']) {
      const r = loadDashboardConfig(baseEnv({ DASHBOARD_SUPPORT_URL: bad }));
      assert.equal(r.ok, true, bad);
      assert.equal(r.config.supportUrl, null, bad);
      assert.equal(r.warnings.length, 1, bad);
    }
  });

  test('every key the dashboard reads is documented in .env.example', () => {
    const env = readFileSync(join(ROOT, '.env.example'), 'utf8');
    for (const key of ['DISCORD_CLIENT_SECRET', 'DASHBOARD_URL', 'DASHBOARD_SESSION_SECRET', 'DASHBOARD_PORT', 'DASHBOARD_HOST',
      'DASHBOARD_TRUSTED_PROXIES', 'DASHBOARD_CONTACT', 'DASHBOARD_SUPPORT_URL']) {
      assert.match(env, new RegExp(`^#\\s*${key}=`, 'm'), `${key} must be a commented-out line`);
    }
  });
});

// ── Sessions, cookies and access rules ───────────────────────────────────────

describe('auth', () => {
  const payload = { uid: USER_ID, username: 'alice', globalName: 'Alice', avatar: null, accessToken: TOKEN, exp: Date.now() + 60_000 };

  test('seal/unseal round-trips and hides the payload', () => {
    const sealed = auth.sealSession(payload, KEY);
    assert.match(sealed, /^v1\.[A-Za-z0-9_-]+$/);
    assert.equal(sealed.includes(TOKEN), false);
    assert.equal(Buffer.from(sealed.slice(3), 'base64url').toString('latin1').includes(TOKEN), false);
    assert.deepEqual(auth.unsealSession(sealed, KEY), payload);
    assert.notEqual(auth.sealSession(payload, KEY), sealed, 'a fresh IV per seal');
  });

  test('tampered, expired, foreign or malformed values unseal to null', () => {
    const sealed = auth.sealSession(payload, KEY);
    const body = sealed.slice(3);
    const mid = Math.floor(body.length / 2);
    const flipped = `v1.${body.slice(0, mid)}${body[mid] === 'A' ? 'B' : 'A'}${body.slice(mid + 1)}`;
    assert.equal(auth.unsealSession(flipped, KEY), null);
    assert.equal(auth.unsealSession(sealed.slice(0, -4), KEY), null);
    assert.equal(auth.unsealSession(`v2.${body}`, KEY), null);
    assert.equal(auth.unsealSession(sealed, auth.deriveSessionKey('y'.repeat(48))), null);
    assert.equal(auth.unsealSession(sealed, KEY, payload.exp), null, 'expired at exp');
    assert.equal(auth.unsealSession(auth.sealSession({ ...payload, exp: 'never' }, KEY), KEY), null);
    assert.equal(auth.unsealSession(auth.sealSession([1, 2], KEY), KEY), null);
    for (const junk of [undefined, null, '', 'v1.', 'v1.!!!', 'abc', 'v1.' + 'A'.repeat(5000), 42]) {
      assert.equal(auth.unsealSession(junk, KEY), null);
    }
  });

  test('cookies parse and serialise', () => {
    const jar = auth.parseCookies('a=1; dash_session=v1.abc; a=2; bad=%E0%A4%A; q="quoted"; =x; flag');
    assert.equal(jar.get('a'), '1');
    assert.equal(jar.get('dash_session'), 'v1.abc');
    assert.equal(jar.has('bad'), false);
    assert.equal(jar.get('q'), 'quoted');
    assert.equal(auth.parseCookies(undefined).size, 0);
    const c = auth.serializeCookie('dash_session', 'v1.x', { maxAge: 60.9, secure: true });
    assert.equal(c, 'dash_session=v1.x; Path=/; HttpOnly; SameSite=Lax; Max-Age=60; Secure');
    assert.match(auth.clearCookie('dash_session'), /^dash_session=; Path=\/; HttpOnly; SameSite=Lax; Max-Age=0; Expires=/);
  });

  test('the OAuth state is random and compared exactly', () => {
    const s = auth.createOAuthState();
    assert.match(s, /^[A-Za-z0-9_-]{43}$/);
    assert.notEqual(auth.createOAuthState(), s);
    assert.equal(auth.statesMatch(s, s), true);
    assert.equal(auth.statesMatch(s, `${s}x`), false);
    assert.equal(auth.statesMatch(s, s.toUpperCase()), false);
    assert.equal(auth.statesMatch(undefined, s), false);
    assert.equal(auth.statesMatch('', ''), false);
    assert.equal(auth.statesMatch(s, null), false);
  });

  test('the authorize URL asks for identify and guilds', () => {
    const url = new URL(auth.buildAuthorizeUrl({ clientId: '123456789012345678', redirectUri: 'https://d.example/auth/callback' }, 'st'));
    assert.equal(url.origin, 'https://discord.com');
    assert.equal(url.pathname, '/oauth2/authorize');
    assert.equal(url.searchParams.get('scope'), 'identify guilds');
    assert.equal(url.searchParams.get('response_type'), 'code');
    assert.equal(url.searchParams.get('redirect_uri'), 'https://d.example/auth/callback');
    assert.equal(url.searchParams.get('state'), 'st');
    assert.equal(url.searchParams.get('prompt'), 'none');
  });

  test('canManageGuild mirrors the ADMIN level, with no BOT_OWNER_ID bypass', () => {
    const cases = [
      [{ owner: true, permissions: '0' }, true],
      [{ owner: false, permissions: '8' }, true],
      [{ owner: false, permissions: '32' }, true],
      [{ owner: false, permissions: '1099511627775' }, true],
      [{ owner: false, permissions: '0' }, false],
      [{ owner: false, permissions: '16' }, false],
      [{ owner: false, permissions: '1099511627735' }, false], // every bit but 0x8 and 0x20
      [{ owner: false, permissions: 'abc' }, false],
      [{ owner: false, permissions: '-8' }, false],
      [{ owner: false }, false],
      [{ owner: 'true', permissions: '0' }, false],
      [undefined, false], // the guild must be in the user's list
      [null, false],
    ];
    for (const [guild, expected] of cases) {
      assert.equal(auth.canManageGuild(guild), expected, JSON.stringify(guild));
    }
  });

  test('checkWriteRequest blocks cross-site writes', () => {
    const config = { origin: 'https://dash.example.com' };
    const req = (method, headers) => ({ method, headers });
    const json = { 'content-type': 'application/json', 'x-dashboard': '1' };
    const cases = [
      [req('GET', json), 405],
      [req('PUT', json), 405],
      [req('DELETE', { 'x-dashboard': '1' }), 415],
      [req('DELETE', json), 200],
      [req('POST', { 'x-dashboard': '1' }), 415],
      [req('POST', { 'content-type': 'text/plain', 'x-dashboard': '1' }), 415],
      [req('POST', { 'content-type': 'application/x-www-form-urlencoded', 'x-dashboard': '1' }), 415],
      [req('POST', { 'content-type': 'application/json' }), 403],
      [req('POST', { ...json, 'x-dashboard': 'true' }), 403],
      [req('PATCH', { ...json, origin: 'https://evil.example' }), 403],
      [req('PATCH', { ...json, origin: 'null' }), 403],
      [req('PATCH', { ...json, origin: 'https://dash.example.com.evil.example' }), 403],
      [req('PATCH', { ...json, 'sec-fetch-site': 'cross-site' }), 403],
      [req('PATCH', { ...json, 'sec-fetch-site': 'same-site' }), 403],
      [req('PATCH', json), 200],
      [req('POST', { ...json, 'content-type': 'Application/JSON; charset=utf-8' }), 200],
      [req('PATCH', { ...json, origin: 'https://dash.example.com', 'sec-fetch-site': 'same-origin' }), 200],
      [req('POST', { ...json, 'sec-fetch-site': 'none' }), 200],
    ];
    for (const [r, status] of cases) {
      const result = auth.checkWriteRequest(r, config);
      if (status === 200) assert.deepEqual(result, { ok: true }, JSON.stringify(r));
      else assert.equal(result.status, status, JSON.stringify(r));
    }
  });

  test('checkWriteRequest names the configured address when a well-formed Origin differs', () => {
    const config = { origin: 'https://dash.example.com' };
    const json = { 'content-type': 'application/json', 'x-dashboard': '1' };
    const check = (origin) => auth.checkWriteRequest({ method: 'PATCH', headers: { ...json, origin } }, config);
    const expected = 'This dashboard is configured for https://dash.example.com. Open it there, or set DASHBOARD_URL to the address you use.';
    for (const origin of ['http://192.168.1.5:8080', 'http://localhost:8080', 'https://evil.example', 'https://dash.example.com.evil.example']) {
      const result = check(origin);
      assert.deepEqual([result.ok, result.status, result.error], [false, 403, 'forbidden'], origin);
      assert.equal(result.message, expected, origin);
      assert.equal(result.message.includes(new URL(origin).host), false, 'the request origin is never echoed');
    }
    // Not a well-formed http(s) origin: the generic message.
    for (const origin of ['null', '', 'not an origin', 'ftp://dash.example.com', 'https://evil.example/path', 'javascript:alert(1)']) {
      const result = check(origin);
      assert.equal(result.status, 403, JSON.stringify(origin));
      assert.equal(result.message, 'Cross-origin requests are not allowed.', JSON.stringify(origin));
    }
    // The matching origin still passes, and Sec-Fetch-Site keeps its own message.
    assert.deepEqual(check('https://dash.example.com'), { ok: true });
    const site = auth.checkWriteRequest({ method: 'PATCH', headers: { ...json, origin: config.origin, 'sec-fetch-site': 'cross-site' } }, config);
    assert.equal(site.message, 'Cross-site requests are not allowed.');
  });
});

// ── Discord client ───────────────────────────────────────────────────────────

describe('discordApi', () => {
  const config = { clientId: '123456789012345678', clientSecret: 's', botToken: 'bot-token', redirectUri: 'http://localhost:8080/auth/callback' };

  /** A fake fetch: `routes(url, init)` returns [status, body] or throws. Records every call. */
  function fakeFetch(routes) {
    const calls = [];
    const impl = async (url, init) => {
      calls.push({ url: String(url), auth: init?.headers?.Authorization });
      const [status, body, headers = {}] = await routes(String(url), init);
      return new Response(body === undefined ? null : JSON.stringify(body), { status, headers });
    };
    return { impl, calls };
  }

  test('CDN URLs and the invite link', () => {
    const api = discordApi.createDiscordApi({ config, fetchImpl: async () => { throw new Error('no network'); } });
    assert.equal(discordApi.userAvatarUrl({ id: '80351110224678912', avatar: null }),
      `https://cdn.discordapp.com/embed/avatars/${Number((80351110224678912n >> 22n) % 6n)}.png`);
    const hash = 'a'.repeat(32);
    assert.equal(discordApi.userAvatarUrl({ id: USER_ID, avatar: hash }), `https://cdn.discordapp.com/avatars/${USER_ID}/${hash}.webp?size=128`);
    assert.match(discordApi.userAvatarUrl({ id: USER_ID, avatar: '../../x' }), /\/embed\/avatars\/\d\.png$/);
    assert.equal(discordApi.guildIconUrl({ id: G.zeta, icon: null }), null);
    assert.equal(discordApi.guildIconUrl({ id: G.zeta, icon: hash }), `https://cdn.discordapp.com/icons/${G.zeta}/${hash}.webp?size=128`);

    const invite = new URL(api.inviteUrl(G.zeta));
    assert.equal(invite.origin, 'https://discord.com');
    assert.equal(invite.searchParams.get('scope'), 'bot applications.commands');
    assert.equal(invite.searchParams.get('guild_id'), G.zeta);
    assert.equal(invite.searchParams.get('disable_guild_select'), 'true');
    const perms = BigInt(invite.searchParams.get('permissions'));
    assert.equal(perms & PermissionFlagsBits.Administrator, 0n, 'never Administrator');
    for (const p of ['ManageRoles', 'ManageChannels', 'BanMembers', 'KickMembers', 'ModerateMembers', 'ManageMessages', 'ManageGuild', 'SendMessages']) {
      assert.equal(perms & PermissionFlagsBits[p], PermissionFlagsBits[p], p);
    }
  });

  test('getUserGuilds caches per user for 60 s and shares concurrent calls', async () => {
    let t = 1_000_000;
    const { impl, calls } = fakeFetch(async () => [200, [{ id: G.zeta, name: 'Z', icon: null, owner: false, permissions: '32' }, { id: 'bad' }]]);
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => t });
    const [a, b] = await Promise.all([api.getUserGuilds(USER_ID, TOKEN), api.getUserGuilds(USER_ID, TOKEN)]);
    assert.equal(calls.length, 1);
    assert.equal(a, b);
    assert.deepEqual(a.map((g) => g.id), [G.zeta], 'invalid entries are dropped');
    assert.equal(calls[0].auth, `Bearer ${TOKEN}`);
    t += 59_000;
    await api.getUserGuilds(USER_ID, TOKEN);
    assert.equal(calls.length, 1);
    t += 2_000;
    await api.getUserGuilds(USER_ID, TOKEN);
    assert.equal(calls.length, 2);
    await api.getUserGuilds(USER_ID, 'another-token');
    assert.equal(calls.length, 3, 'a different token does not reuse the cache');
    api.forgetUser(USER_ID);
    await api.getUserGuilds(USER_ID, 'another-token');
    assert.equal(calls.length, 4);
  });

  test('getUserGuilds maps 401 and 429, serving a recent list on 429', async () => {
    let t = 0;
    let reply = [200, [{ id: G.zeta, name: 'Z', owner: true, permissions: '0' }]];
    const { impl } = fakeFetch(async () => reply);
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => t });
    await api.getUserGuilds(USER_ID, TOKEN);
    t += 61_000;
    reply = [429, { retry_after: 2.5 }];
    assert.deepEqual((await api.getUserGuilds(USER_ID, TOKEN, { allowStale: true })).map((g) => g.id), [G.zeta], 'stale list on 429');
    await assert.rejects(api.getUserGuilds(OWNER_ID, TOKEN), (err) => err instanceof DiscordApiError && err.code === 'rate_limited' && err.retryAfter === 3);
    reply = [401, { message: '401: Unauthorized' }];
    await assert.rejects(api.getUserGuilds(USER_ID, TOKEN), (err) => err.code === 'unauthenticated');
    const down = discordApi.createDiscordApi({ config, fetchImpl: async () => { throw new TypeError('fetch failed'); } });
    await assert.rejects(down.getUserGuilds(USER_ID, TOKEN), (err) => err.code === 'unavailable');
  });

  test('getUserGuilds serves an old list only when the caller allows it, and never past 5 minutes', async () => {
    let t = 0;
    let reply = [200, [{ id: G.zeta, name: 'Z', owner: true, permissions: '0' }]];
    const { impl, calls } = fakeFetch(async () => reply);
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => t });
    await api.getUserGuilds(USER_ID, TOKEN);
    t += 61_000; // past the 60 s TTL, within the 5 minute stale window
    const failures = [[429, { retry_after: 2.5 }, 'rate_limited'], [503, {}, 'unavailable']];
    for (const [status, body, code] of failures) {
      reply = [status, body];
      for (const opts of [undefined, {}, { allowStale: false }]) {
        await assert.rejects(api.getUserGuilds(USER_ID, TOKEN, opts), (err) => err instanceof DiscordApiError && err.code === code,
          `${status} ${JSON.stringify(opts)}: authoritative callers get the error`);
      }
      assert.deepEqual((await api.getUserGuilds(USER_ID, TOKEN, { allowStale: true })).map((g) => g.id), [G.zeta], `${status}: stale list for display`);
    }
    await assert.rejects(api.getUserGuilds(USER_ID, TOKEN), (err) => err.code === 'unavailable' && err.status === 503);

    // Callers sharing one in-flight request each decide for themselves.
    reply = [429, { retry_after: 2.5 }];
    const before = calls.length;
    const [strict, lenient] = await Promise.allSettled([
      api.getUserGuilds(USER_ID, TOKEN), api.getUserGuilds(USER_ID, TOKEN, { allowStale: true }),
    ]);
    assert.equal(calls.length, before + 1, 'one request');
    assert.equal(strict.status, 'rejected');
    assert.equal(strict.reason.code, 'rate_limited');
    assert.equal(strict.reason.retryAfter, 3);
    assert.equal(lenient.status, 'fulfilled');

    t += 5 * 60_000; // the list is now older than 5 minutes
    await assert.rejects(api.getUserGuilds(USER_ID, TOKEN, { allowStale: true }), (err) => err.code === 'rate_limited');

    // A fresh list is served from the cache either way, without a request.
    reply = [200, [{ id: G.zeta, name: 'Z', owner: true, permissions: '0' }]];
    await api.getUserGuilds(USER_ID, TOKEN);
    const cached = calls.length;
    await api.getUserGuilds(USER_ID, TOKEN);
    await api.getUserGuilds(USER_ID, TOKEN, { allowStale: true });
    assert.equal(calls.length, cached);
  });

  test('createInvite: one use, 30 minutes, not temporary, with the audit-log reason', async () => {
    let seen;
    const { impl } = fakeFetch(async (url, init) => {
      seen = { url, method: init.method, headers: init.headers, body: JSON.parse(init.body) };
      return url.endsWith(`/channels/${CH.text}/invites`) ? [200, { code: 'aB-9', expires_at: '2026-10-13T12:30:00.000Z' }] : [200, { code: '<bad>' }];
    });
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => 0 });
    const invite = await api.createInvite(CH.text, { reason: 'Requested by the bot owner' });
    assert.deepEqual(invite, { code: 'aB-9', url: 'https://discord.gg/aB-9', expiresAt: '2026-10-13T12:30:00.000Z' });
    assert.equal(seen.method, 'POST');
    assert.deepEqual(seen.body, { max_age: discordApi.OWNER_INVITE_MAX_AGE_SECONDS, max_uses: 1, unique: true, temporary: false });
    assert.equal(discordApi.OWNER_INVITE_MAX_AGE_SECONDS, 1800);
    assert.equal(seen.headers['X-Audit-Log-Reason'], encodeURIComponent('Requested by the bot owner'));
    assert.match(seen.headers.Authorization, /^Bot /);
    await assert.rejects(api.createInvite(CH.voice), (err) => err instanceof DiscordApiError && err.code === 'unavailable', 'an odd code is refused');
  });

  test('getBotGuilds lists the bot\'s servers with names from the cached list', async () => {
    const { impl, calls } = fakeFetch(async (url) => {
      if (url.includes('/users/@me/guilds')) {
        return [200, [{ id: G.zeta, name: 'zeta', icon: 'c'.repeat(32) }, { id: G.beta }, { id: 'nope', name: 'Bad' }, { id: G.zeta, name: 'again' }]];
      }
      return [404, { code: 10004 }];
    });
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => 0 });
    assert.deepEqual(await api.getBotGuilds(), [
      { id: G.zeta, name: 'zeta', icon: 'c'.repeat(32) },
      { id: G.beta, name: 'Unknown server', icon: null },
    ]);
    const requests = calls.length;
    assert.equal((await api.getBotGuilds()).length, 2);
    assert.equal(await api.isBotInGuild(G.beta, { verify: false }), true, 'one list serves both');
    assert.equal(calls.length, requests, 'served from the cache');
  });

  test('isBotInGuild pages through the bot guild list and double-checks misses', async () => {
    const page1 = Array.from({ length: 200 }, (_, i) => ({ id: String(300000000000000000n + BigInt(i)) }));
    const page2 = [{ id: G.zeta }];
    const { impl, calls } = fakeFetch(async (url) => {
      if (url.includes('/users/@me/guilds')) return [200, new URL(url).searchParams.get('after') === '0' ? page1 : page2];
      if (url.endsWith(`/guilds/${G.beta}`)) return [200, { id: G.beta }];
      if (url.endsWith(`/guilds/${G.alpha}`)) return [403, { code: 50001 }];
      return [404, { code: 10004 }];
    });
    let t = 0;
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => t });
    assert.equal(await api.isBotInGuild(G.zeta), true);
    assert.equal(calls.length, 2, 'two pages');
    assert.equal(calls[1].url.includes(`after=${page1[199].id}`), true);
    assert.equal(calls[0].auth, 'Bot bot-token');
    assert.equal(await api.isBotInGuild(page1[5].id), true);
    assert.equal(calls.length, 2, 'cached');
    assert.equal(await api.isBotInGuild(G.alpha, { verify: false }), false);
    assert.equal(calls.length, 2, 'verify:false never double-checks');
    assert.equal(await api.isBotInGuild(G.alpha), false);
    assert.equal(await api.isBotInGuild(G.stranger), false);
    assert.equal(await api.isBotInGuild(G.beta), true);
    const before = calls.length;
    assert.equal(await api.isBotInGuild(G.beta), true, 'a confirmed guild is remembered');
    assert.equal(calls.length, before);
    assert.equal(await api.isBotInGuild('not-a-snowflake'), false);
  });

  /** A fake fetch for the bot's guild list: page 1 is full, page 2 holds G.zeta; `limit(url)` may answer a 429 first. */
  function pagedBotGuilds(limit) {
    const page1 = Array.from({ length: 200 }, (_, i) => ({ id: String(300000000000000000n + BigInt(i)) }));
    return fakeFetch(async (url) => {
      const after = new URL(url).searchParams.get('after');
      const limited = limit(after);
      if (limited) return limited;
      return [200, after === '0' ? page1 : [{ id: G.zeta }]];
    });
  }

  test('the bot guild load waits out a short 429 mid-pagination and retries the same page', async () => {
    let limits = 1;
    const { impl, calls } = pagedBotGuilds((after) => (after !== '0' && limits-- > 0 ? [429, { retry_after: 0.01 }] : null));
    const sleeps = [];
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, sleep: async (ms) => { sleeps.push(ms); } });
    const ids = await api.getBotGuildIds();
    assert.equal(ids.size, 201);
    assert.equal(ids.has(G.zeta), true);
    assert.equal(calls.length, 3, 'page 1, the limited page 2, page 2 again');
    assert.equal(calls[2].url, calls[1].url, 'the same page is retried');
    assert.deepEqual(sleeps, [1000], 'retry_after is rounded up to whole seconds');
    assert.equal(await api.isBotInGuild(G.zeta, { verify: false }), true);
    assert.equal(calls.length, 3, 'cached afterwards');
  });

  test('the bot guild load fails on a long 429 and after three retries', async () => {
    // A wait longer than 10 s is not worth holding the request for.
    const long = pagedBotGuilds((after) => (after !== '0' ? [429, { retry_after: 30 }] : null));
    const longSleeps = [];
    const api = discordApi.createDiscordApi({ config, fetchImpl: long.impl, sleep: async (ms) => { longSleeps.push(ms); } });
    await assert.rejects(api.getBotGuildIds(), (err) => err instanceof DiscordApiError && err.code === 'rate_limited' && err.retryAfter === 30);
    assert.equal(long.calls.length, 2);
    assert.deepEqual(longSleeps, []);

    // Exactly 10 s is still waited out; the retries are capped at 3 per refresh, counted across pages.
    const stuck = pagedBotGuilds((after) => (after !== '0' ? [429, { retry_after: 10 }] : null));
    const stuckSleeps = [];
    const stuckApi = discordApi.createDiscordApi({ config, fetchImpl: stuck.impl, sleep: async (ms) => { stuckSleeps.push(ms); } });
    await assert.rejects(stuckApi.getBotGuildIds(), (err) => err.code === 'rate_limited');
    assert.deepEqual(stuckSleeps, [10_000, 10_000, 10_000]);
    assert.equal(stuck.calls.length, 1 + 4, 'page 1, then the limited page 2 once and three retries');

    // Other failures are not retried.
    const down = pagedBotGuilds((after) => (after !== '0' ? [503, {}] : null));
    const downSleeps = [];
    const downApi = discordApi.createDiscordApi({ config, fetchImpl: down.impl, sleep: async (ms) => { downSleeps.push(ms); } });
    await assert.rejects(downApi.getBotGuildIds(), (err) => err.code === 'unavailable');
    assert.deepEqual(downSleeps, []);
    assert.equal(down.calls.length, 2);
  });

  test('the default pause between bot guild page retries is a real, unref\'d wait', async () => {
    let limits = 1;
    const { impl, calls } = pagedBotGuilds((after) => (after !== '0' && limits-- > 0 ? [429, { retry_after: 1 }] : null));
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl });
    const started = Date.now();
    const keepAlive = setTimeout(() => {}, 5_000); // the pause itself is unref'd; in the server the listener keeps the loop alive
    try {
      assert.equal((await api.getBotGuildIds()).size, 201);
    } finally {
      clearTimeout(keepAlive);
    }
    assert.ok(Date.now() - started >= 900, 'waited about retry_after seconds');
    assert.equal(calls.length, 3);
  });

  /** A fake Discord REST API: `handler(call)` returns [status, body]. Records method, path, auth, content type and body. */
  function restFetch(handler) {
    const calls = [];
    const impl = async (url, init = {}) => {
      const u = new URL(String(url));
      const call = {
        method: init.method ?? 'GET', path: u.pathname.replace(/^\/api\/v\d+/, ''), search: u.search,
        auth: init.headers?.Authorization, contentType: init.headers?.['Content-Type'], body: init.body ? JSON.parse(init.body) : undefined,
      };
      calls.push(call);
      const [status, body] = await handler(call);
      return new Response(body === undefined ? null : JSON.stringify(body), { status });
    };
    return { impl, calls };
  }

  const c = (n) => String(310000000000000000n + BigInt(n));
  const R = Object.freeze({ bot: '410000000000000001', mod: '410000000000000002', fan: '410000000000000003', top: '410000000000000004', tie: '410000000000000005' });
  const guildBody = (extra = {}) => ({
    id: G.zeta, name: 'Zeta', icon: 'a'.repeat(32), owner_id: GUILD_OWNER, approximate_member_count: 42, premium_tier: 2,
    roles: [
      { id: G.zeta, name: '@everyone', position: 0, color: 0, managed: false, permissions: '1024' },
      { id: R.bot, name: 'Bot', position: 5, color: 0, managed: true, permissions: String(PermissionFlagsBits.ManageRoles) },
      { id: R.mod, name: 'Mods', position: 4, color: 0xff0000, managed: false, permissions: String(PermissionFlagsBits.KickMembers) },
      { id: R.tie, name: 'Tie', position: 2, color: 0, managed: false, permissions: '0' },
      { id: R.fan, name: 'Fans', position: 2, color: 0x00ff00, managed: false, mentionable: true, permissions: '0' },
      { id: R.top, name: 'Owner', position: 9, color: 0, managed: false, permissions: '8' },
    ],
    emojis: [{ id: '610000000000000002', name: 'zebra' }, { id: '610000000000000001', name: 'apple', animated: true }, { id: null, name: '✨' }],
    ...extra,
  });
  const channelsBody = [
    { id: c(1), type: 2, name: 'voice-top', position: 0 },
    { id: c(2), type: 0, name: 'text-top', position: 5 },
    { id: c(3), type: 4, name: 'CAT B', position: 2 },
    { id: c(4), type: 4, name: 'CAT A', position: 1 },
    { id: c(5), type: 0, name: 'b-text', parent_id: c(3), position: 1 },
    { id: c(6), type: 2, name: 'b-voice', parent_id: c(3), position: 0 },
    { id: c(7), type: 5, name: 'a-news', parent_id: c(4), position: 3, nsfw: true },
    { id: c(8), type: 15, name: 'a-forum', parent_id: c(4), position: 1 },
    { id: c(9), type: 0, name: 'orphan', parent_id: '399999999999999999', position: 9 },
    { id: 'bad', type: 0 },
  ];
  const resourcesApi = (extra = {}) => restFetch(async (call) => {
    if (call.path === `/guilds/${G.zeta}`) return [200, guildBody(extra)];
    if (call.path === `/guilds/${G.zeta}/channels`) return [200, channelsBody];
    if (call.path === `/guilds/${G.zeta}/members/${config.clientId}`) return [200, { roles: [R.bot] }];
    if (call.path === `/guilds/${G.zeta}/roles`) return [200, guildBody().roles];
    if (call.path === `/guilds/${G.zeta}/emojis`) return [200, []];
    return [404, { code: 10004 }];
  });

  test('getGuildResources maps channels in sidebar order, roles highest first, emojis by name, and the bot', async () => {
    const { impl, calls } = resourcesApi();
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl });
    const res = await api.getGuildResources(G.zeta);
    assert.deepEqual(calls.map((x) => `${x.method} ${x.path}${x.search}`).sort(), [
      `GET /guilds/${G.zeta}/channels`, `GET /guilds/${G.zeta}/members/${config.clientId}`, `GET /guilds/${G.zeta}?with_counts=true`,
    ]);
    assert.ok(calls.every((x) => x.auth === 'Bot bot-token'));
    assert.deepEqual(res.channels.map((x) => x.name), ['text-top', 'orphan', 'voice-top', 'CAT A', 'a-forum', 'a-news', 'CAT B', 'b-text', 'b-voice']);
    assert.deepEqual(res.channels.find((x) => x.name === 'a-news'),
      { id: c(7), name: 'a-news', type: 5, kind: 'announcement', parentId: c(4), position: 3, nsfw: true });
    assert.equal(res.channels.find((x) => x.name === 'a-forum').kind, 'forum');
    assert.equal(res.channels.find((x) => x.name === 'orphan').parentId, '399999999999999999');
    assert.deepEqual(res.roles.map((r) => r.name), ['Owner', 'Bot', 'Mods', 'Fans', 'Tie', '@everyone']);
    assert.deepEqual(res.roles.map((r) => r.assignable), [false, false, true, true, true, false]);
    assert.deepEqual(res.roles.find((r) => r.id === R.mod), {
      id: R.mod, name: 'Mods', color: '#FF0000', position: 4, managed: false, everyone: false, mentionable: false, assignable: true, dangerous: true,
    });
    assert.equal(res.roles.find((r) => r.id === R.fan).mentionable, true);
    assert.equal(res.roles.find((r) => r.id === G.zeta).everyone, true);
    assert.equal(res.roles.find((r) => r.id === R.fan).dangerous, false);
    assert.equal(res.roles.find((r) => r.id === R.tie).color, null);
    assert.deepEqual(res.emojis, [{ id: '610000000000000001', name: 'apple', animated: true }, { id: '610000000000000002', name: 'zebra', animated: false }]);
    // The fixture's bot role has Manage Roles but not Create Invite.
    assert.deepEqual(res.bot, { userId: config.clientId, topRolePosition: 5, manageRoles: true, createInvite: false });
    assert.deepEqual(res.guild, {
      id: G.zeta, name: 'Zeta', icon: 'a'.repeat(32), memberCount: 42, ownerId: GUILD_OWNER, premiumTier: 2, premiumSubscriptionCount: 0,
      verificationLevel: 0,
    });
    assert.ok(Object.isFrozen(res) && Object.isFrozen(res.roles[0]));
  });

  test('getGuildResources caches for 30 s, shares concurrent loads, and reloads on fresh', async () => {
    let t = 0;
    const { impl, calls } = resourcesApi();
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => t });
    const [a, b] = await Promise.all([api.getGuildResources(G.zeta), api.getGuildResources(G.zeta)]);
    assert.equal(a, b);
    assert.equal(calls.length, 3, 'one load');
    t += 29_000;
    assert.equal(await api.getGuildResources(G.zeta), a);
    assert.equal(calls.length, 3);
    await api.getGuildResources(G.zeta, { fresh: true });
    assert.equal(calls.length, 6, 'fresh skips the cache');
    t += 29_000;
    await api.getGuildResources(G.zeta);
    assert.equal(calls.length, 6, 'the fresh load refreshed the cache');
    t += 31_000;
    await api.getGuildResources(G.zeta);
    assert.equal(calls.length, 9, 'expired');
    await assert.rejects(api.getGuildResources('nope'), (err) => err instanceof DiscordApiError && err.code === 'not_found');
    assert.equal(calls.length, 9);
  });

  test('getGuildResources asks the list routes when the guild carries no roles or emojis, and reports Discord errors', async () => {
    const { impl, calls } = resourcesApi({ roles: undefined, emojis: undefined });
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl });
    const res = await api.getGuildResources(G.zeta);
    assert.ok(calls.some((x) => x.path === `/guilds/${G.zeta}/roles`));
    assert.ok(calls.some((x) => x.path === `/guilds/${G.zeta}/emojis`));
    assert.equal(res.roles.length, 6);

    const denied = restFetch(async (call) => (call.path.endsWith('/channels') ? [403, { message: 'Missing Access', code: 50001 }] : [200, guildBody()]));
    const api2 = discordApi.createDiscordApi({ config, fetchImpl: denied.impl });
    await assert.rejects(api2.getGuildResources(G.zeta), (err) => err instanceof DiscordApiError && err.code === 'forbidden'
      && err.status === 403 && err.discordCode === 50001 && !err.message.includes('bot-token'));
  });

  test('sendMessage, editMessage and deleteMessage use the bot token and never allow @everyone', async () => {
    const { impl, calls } = restFetch(async (call) => {
      if (call.path === `/channels/${c(2)}/messages` && call.method === 'POST') return [200, { id: c(50), channel_id: c(2) }];
      if (call.path === `/channels/${c(2)}/messages/${c(50)}` && call.method === 'PATCH') return [200, { id: c(50), channel_id: c(2) }];
      if (call.path === `/channels/${c(2)}/messages/${c(50)}` && call.method === 'DELETE') return [204];
      if (call.path === `/channels/${c(2)}/messages/${c(51)}`) return [404, { message: 'Unknown Message', code: 10008 }];
      if (call.path === `/channels/${c(3)}/messages`) return [403, { message: 'Missing Permissions', code: 50013 }];
      if (call.path === `/channels/${c(4)}/messages`) return [429, { retry_after: 1.2 }];
      return [404, { code: 10003 }];
    });
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl });
    assert.deepEqual(await api.sendMessage(c(2), { content: 'hi @everyone', allowed_mentions: { parse: ['everyone', 'users', 'roles'] } }),
      { id: c(50), channelId: c(2) });
    let sent = calls.at(-1);
    assert.equal(sent.method, 'POST');
    assert.equal(sent.auth, 'Bot bot-token');
    assert.equal(sent.contentType, 'application/json');
    assert.deepEqual(sent.body, { content: 'hi @everyone', allowed_mentions: { parse: ['users', 'roles'] } });

    await api.sendMessage(c(2), { embeds: [{ title: 'x' }] });
    assert.deepEqual(calls.at(-1).body.allowed_mentions, { parse: [] }, 'no pings unless asked');
    await api.sendMessage(c(2), { content: '<@&1>', allowed_mentions: { roles: [RL.low, 'x'], users: [USER_ID] } });
    assert.deepEqual(calls.at(-1).body.allowed_mentions, { parse: [], users: [USER_ID], roles: [RL.low] });

    assert.deepEqual(await api.editMessage(c(2), c(50), { content: 'edited' }), { id: c(50), channelId: c(2) });
    sent = calls.at(-1);
    assert.equal(sent.method, 'PATCH');
    assert.deepEqual(sent.body, { content: 'edited', allowed_mentions: { parse: [] } });

    assert.equal(await api.deleteMessage(c(2), c(50)), undefined);
    assert.equal(calls.at(-1).method, 'DELETE');
    assert.equal(calls.at(-1).body, undefined);
    assert.equal(await api.deleteMessage(c(2), c(51)), undefined, 'an already deleted message counts as deleted');

    await assert.rejects(api.sendMessage(c(3), { content: 'x' }),
      (err) => err.code === 'forbidden' && err.status === 403 && err.discordCode === 50013 && /POST \/channels\//.test(err.message));
    await assert.rejects(api.editMessage(c(3), c(50), { content: 'x' }), (err) => err.code === 'not_found' && err.discordCode === 10003);
    await assert.rejects(api.sendMessage(c(4), { content: 'x' }), (err) => err.code === 'rate_limited' && err.retryAfter === 2);
    const before = calls.length;
    await assert.rejects(api.sendMessage('nope', { content: 'x' }), (err) => err instanceof DiscordApiError);
    await assert.rejects(api.deleteMessage(c(2), '../x'), (err) => err instanceof DiscordApiError);
    assert.equal(calls.length, before, 'bad ids never reach Discord');
  });

  test('getMember returns role ids, or null for a non-member', async () => {
    const { impl } = restFetch(async (call) => (call.path.endsWith(`/members/${USER_ID}`)
      ? [200, { roles: [RL.low, 'x'] }] : [404, { code: 10007 }]));
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl });
    assert.deepEqual(await api.getMember(G.zeta, USER_ID), { roles: [RL.low] });
    assert.equal(await api.getMember(G.zeta, OWNER_ID), null);
  });

  test('sortChannels keeps uncategorised channels first and voice after text inside each parent', () => {
    const ch = (id, type, position, parentId = null) => ({ id: c(id), type, kind: schema.channelKind(type), position, parentId });
    const sorted = discordApi.sortChannels([ch(1, 2, 0), ch(2, 4, 0), ch(3, 13, 0, c(2)), ch(4, 0, 9, c(2)), ch(5, 0, 1), ch(6, 0, 1)]);
    assert.deepEqual(sorted.map((x) => x.id), [c(5), c(6), c(1), c(2), c(4), c(3)]);
  });
});

// ── HTTP API ─────────────────────────────────────────────────────────────────

const USER_GUILDS = Object.freeze([
  { id: G.zeta, name: 'zeta', icon: null, owner: false, permissions: '32' },
  { id: G.alpha, name: 'Alpha', icon: 'b'.repeat(32), owner: true, permissions: '0' },
  { id: G.beta, name: 'beta', icon: null, owner: false, permissions: '8' },
  { id: G.plain, name: 'Plain', icon: null, owner: false, permissions: '1024' },
  { id: G.broken, name: 'Broken', icon: null, owner: true, permissions: '0' },
]);
const BOT_GUILDS = new Set([G.zeta, G.beta, G.plain, G.broken, G.botOnly]);
/** The bot's own guild list (getBotGuilds): the user's entries where they share one, plus G.botOnly. */
const BOT_GUILD_ENTRIES = Object.freeze([...BOT_GUILDS].map((id) => {
  const g = USER_GUILDS.find((u) => u.id === id);
  return Object.freeze({ id, name: g ? g.name : 'Bot only', icon: g ? g.icon : null });
}));

/** Channel and role ids of the fake server G.zeta. */
const CH = Object.freeze({
  text: '300000000000000001', voice: '300000000000000002', category: '300000000000000003', news: '300000000000000004',
  stale: '300000000000000098', // stored in the fake row, deleted from the server since
  ghost: '300000000000000099', // never existed
});
const RL = Object.freeze({
  low: '400000000000000001', admin: '400000000000000002', mid: '400000000000000003', managed: '400000000000000004',
  high: '400000000000000005', stale: '400000000000000098', ghost: '400000000000000099',
});
const BOT_ID = '123456789012345678';
const GUILD_OWNER = '555555555555555555';

/** What discordApi.getGuildResources returns for G.zeta (bot's highest role: position 6; the user's: RL.mid, 3). */
function fakeResources(guildId) {
  const role = (id, name, position, extra = {}) => ({
    id, name, color: null, position, managed: false, everyone: false, mentionable: false, assignable: position < 6, dangerous: false, ...extra,
  });
  return {
    guild: { id: guildId, name: 'zeta', icon: null, memberCount: 12, ownerId: GUILD_OWNER },
    channels: [
      { id: CH.text, name: 'general', type: 0, kind: 'text', parentId: null, position: 0, nsfw: false },
      { id: CH.voice, name: 'Lounge', type: 2, kind: 'voice', parentId: null, position: 1, nsfw: false },
      { id: CH.category, name: 'INFO', type: 4, kind: 'category', parentId: null, position: 0, nsfw: false },
      { id: CH.news, name: 'news', type: 5, kind: 'announcement', parentId: CH.category, position: 0, nsfw: false },
    ],
    roles: [
      role(RL.high, 'Above the bot', 8),
      role(RL.managed, 'Some bot', 5, { managed: true, assignable: false }),
      role(RL.mid, 'Mid', 3),
      role(RL.admin, 'Admins', 2, { dangerous: true }),
      role(RL.low, 'Low', 1),
      role(guildId, '@everyone', 0, { everyone: true, assignable: false }),
    ],
    emojis: [{ id: '600000000000000001', name: 'coin', animated: false }],
    bot: { userId: BOT_ID, topRolePosition: 6, manageRoles: true, createInvite: guildId !== G.plain },
  };
}

function createFakes() {
  const calls = {
    writeSettings: [], exchangeCode: [], revokeToken: [], forgetUser: [], getUserGuilds: [], resources: [], getMember: [],
    invites: [],
  };
  /** Channels where the fake bot is refused an invite (Discord 50013); CH.text by default, so the next one is tried. */
  const inviteDeny = new Set([CH.text]);
  let row = {
    guildId: G.zeta, modEnabled: true, xpEnabled: false, voiceXpEnabled: true, levelingEnabled: true,
    captchaLockdownPrior: true, captchaMessageId: '700000000000000001', timezone: 'Europe/Berlin', customPrefix: '', embedColor: 0x5865f2,
    logIgnoredChannelIds: [CH.stale], modRoleIds: [RL.stale], xpMin: 15, xpMax: 25,
  };
  const discord = {
    async getUserGuilds(uid, token, opts = {}) {
      calls.getUserGuilds.push({ token, allowStale: opts?.allowStale === true });
      // Stands in for a Discord 429 or outage with an older list on hand: only a caller that allows it gets the list.
      if (token === 'stale-limited-token' || token === 'stale-down-token') {
        if (opts?.allowStale === true) return USER_GUILDS;
        throw token === 'stale-limited-token'
          ? new DiscordApiError('Discord returned HTTP 429', { status: 429, code: 'rate_limited', retryAfter: 4 })
          : new DiscordApiError('Discord request failed', { code: 'unavailable' });
      }
      if (token === 'expired-token') throw new DiscordApiError('Discord returned HTTP 401', { status: 401, code: 'unauthenticated' });
      if (token === 'limited-token') throw new DiscordApiError('Discord returned HTTP 429', { status: 429, code: 'rate_limited', retryAfter: 7 });
      if (token === 'down-token') throw new DiscordApiError('Discord request failed', { code: 'unavailable' });
      if (token === 'broken-token') return null;
      return USER_GUILDS;
    },
    async isBotInGuild(guildId) { return BOT_GUILDS.has(guildId); },
    async getBotGuilds() { return BOT_GUILD_ENTRIES; },
    async createInvite(channelId, opts) {
      calls.invites.push({ channelId, opts });
      if (inviteDeny.has(channelId)) {
        throw new DiscordApiError('Discord returned HTTP 403', { status: 403, code: 'forbidden', discordCode: 50013 });
      }
      return { code: 'abc123', url: 'https://discord.gg/abc123', expiresAt: '2026-10-13T12:30:00.000Z' };
    },
    inviteUrl: (guildId) => `https://discord.com/oauth2/authorize?client_id=123456789012345678&guild_id=${guildId}`,
    async exchangeCode(code) {
      calls.exchangeCode.push(code);
      if (code !== 'good-code') throw new Error('token exchange returned HTTP 400');
      return { accessToken: TOKEN, expiresIn: 604_800 };
    },
    async getCurrentUser() { return { id: USER_ID, username: 'alice', globalName: 'Alice', avatar: null }; },
    async revokeToken(token) { calls.revokeToken.push(token); return true; },
    forgetUser(uid) { calls.forgetUser.push(uid); },
    async getGuildResources(guildId, opts = {}) {
      calls.resources.push({ guildId, fresh: opts.fresh === true });
      if (guildId === G.beta) throw new DiscordApiError('Discord returned HTTP 503', { status: 503, code: 'unavailable' });
      return fakeResources(guildId);
    },
    async getMember(guildId, userId) {
      calls.getMember.push({ guildId, userId });
      return { roles: [RL.low, RL.mid] };
    },
  };
  // Stands in for settingsBridge.writeSettings: the same merge and cross-field rules, without a database.
  const settings = {
    async readGuildSettings(guildId) {
      if (guildId === G.broken) throw new Error('database down');
      return { ...row, guildId };
    },
    async writeSettings(guildId, changes, ctx) {
      calls.writeSettings.push({ guildId, changes, ctx });
      if (guildId === G.broken) throw new Error('database down');
      const data = fields.toStoredChanges(changes, row);
      const problem = fields.checkCrossFields({ ...row, ...data }, changes);
      if (problem) throw new schema.FieldError(problem.path, problem.message);
      row = { ...row, ...data };
      return { row: { ...row, guildId }, notices: Object.hasOwn(changes, 'economyMaxBalance') ? ['Lowered 2 balances to the new maximum.'] : [] };
    },
  };
  return { discord, settings, calls, inviteDeny, getRow: () => row };
}

function recordingLogger() {
  const lines = [];
  const rec = (level) => (...args) => lines.push(`${level} ${args.map((a) => (a instanceof Error ? a.message : String(a))).join(' ')}`);
  return { lines, info: rec('info'), warn: rec('warn'), error: rec('error'), debug: rec('debug'), success: rec('success') };
}

/** One HTTP request without keep-alive. Resolves { status, headers, text, json }. */
function call(port, method, path, { headers = {}, body } = {}) {
  return new Promise((resolvePromise, reject) => {
    // Browsers frame every body with Content-Length; node would send a DELETE body unframed.
    const framed = body === undefined || Object.keys(headers).some((h) => h.toLowerCase() === 'content-length')
      ? headers : { ...headers, 'content-length': Buffer.byteLength(body) };
    const req = request({ host: '127.0.0.1', port, method, path, headers: framed, agent: false }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => {
        const raw = Buffer.concat(chunks);
        const text = raw.toString('utf8');
        let json;
        try { json = JSON.parse(text); } catch { json = undefined; }
        resolvePromise({ status: res.statusCode, headers: res.headers, text, json, raw });
      });
    });
    req.on('error', reject);
    if (body !== undefined) req.write(body);
    req.end();
  });
}

const listen = (server) => new Promise((res) => server.listen(0, '127.0.0.1', () => res(server.address().port)));
const close = (server) => new Promise((res) => { server.close(() => res()); server.closeAllConnections(); });

function sessionCookie(overrides = {}) {
  const sealed = auth.sealSession({
    uid: USER_ID, username: 'alice', globalName: 'Alice', avatar: null, accessToken: TOKEN, exp: Date.now() + 3_600_000, ...overrides,
  }, KEY);
  return `${auth.SESSION_COOKIE}=${sealed}`;
}

const WRITE_HEADERS = Object.freeze({ 'content-type': 'application/json', 'x-dashboard': '1', origin: 'http://localhost:8080' });

describe('HTTP server', () => {
  const { config } = loadDashboardConfig(baseEnv({ BOT_OWNER_ID: OWNER_ID })); // set, so a returning bot-owner bypass is caught
  const fakes = createFakes();
  const log = recordingLogger();
  let publicDir;
  let server;
  let port;
  const get = (path, headers = {}) => call(port, 'GET', path, { headers });
  const authed = (path, extra = {}) => get(path, { cookie: sessionCookie(), ...extra });
  const patch = (guildId, body, headers = {}) => call(port, 'PATCH', `/api/guilds/${guildId}/settings`, {
    headers: { cookie: sessionCookie(), ...WRITE_HEADERS, ...headers },
    body: typeof body === 'string' ? body : JSON.stringify(body),
  });

  before(async () => {
    publicDir = mkdtempSync(join(tmpdir(), 'dashboard-public-'));
    writeFileSync(join(publicDir, 'index.html'), '<!doctype html><title>t</title>');
    writeFileSync(join(publicDir, 'landing.html'), '<!doctype html><title>l</title><p><!--module-count--> <!--code:leveling--> <a href="@@INVITE_URL@@">Add</a></p>');
    writeFileSync(join(publicDir, 'app.js'), 'export {};');
    writeFileSync(join(publicDir, 'styles.css'), 'body { color: red; }\n'.repeat(200));
    writeFileSync(join(publicDir, 'privacy.html'), '<!doctype html><title>p</title><p>Ask <!--contact-->.</p><p><!--module-count--> <a href="@@INVITE_URL@@">Add</a></p>');
    writeFileSync(join(publicDir, 'docs.html'), '<!doctype html><title>d</title><!--docs-modules--><!--docs-core-->');
    writeFileSync(join(publicDir, 'changelog.html'), '<!doctype html><title>c</title><!--changelog-->');
    mkdirSync(join(publicDir, 'demo'));
    writeFileSync(join(publicDir, 'demo', 'rank-card.png'), Buffer.from([0x89, 0x50, 0x4e, 0x47]));
    server = createDashboardServer({ config, deps: { discord: fakes.discord, settings: fakes.settings }, logger: log, publicDir });
    port = await listen(server);
  });

  after(async () => {
    await close(server);
    rmSync(publicDir, { recursive: true, force: true });
  });

  test('GET /healthz answers with the security headers', async () => {
    const r = await get('/healthz');
    assert.equal(r.status, 200);
    assert.deepEqual(r.json, { ok: true });
    assert.match(r.headers['content-security-policy'], /default-src 'self'/);
    assert.match(r.headers['content-security-policy'], /frame-ancestors 'none'/);
    assert.match(r.headers['content-security-policy'], /img-src 'self' https:\/\/cdn\.discordapp\.com data:/);
    assert.equal(r.headers['x-content-type-options'], 'nosniff');
    assert.equal(r.headers['x-frame-options'], 'DENY');
    assert.equal(r.headers['referrer-policy'], 'same-origin');
    assert.equal(r.headers['cross-origin-opener-policy'], 'same-origin');
    assert.match(r.headers['permissions-policy'], /camera=\(\)/);
    assert.equal(r.headers['strict-transport-security'], undefined, 'no HSTS over http');
    assert.equal(r.headers['cache-control'], 'no-store');
    assert.equal((await call(port, 'POST', '/healthz')).status, 405);
    const head = await call(port, 'HEAD', '/healthz');
    assert.equal(head.status, 200);
    assert.equal(head.text, '');
  });

  test('GET /api/me needs a valid session', async () => {
    const none = await get('/api/me');
    assert.equal(none.status, 401);
    assert.equal(none.json.error, 'unauthenticated');
    assert.equal(none.headers['set-cookie'], undefined);

    const forged = await get('/api/me', { cookie: `${auth.SESSION_COOKIE}=v1.forged` });
    assert.equal(forged.status, 401);
    assert.match(forged.headers['set-cookie'][0], /^dash_session=; .*Max-Age=0/);

    const expired = await get('/api/me', { cookie: sessionCookie({ exp: Date.now() - 1 }) });
    assert.equal(expired.status, 401);

    const badShape = await get('/api/me', { cookie: sessionCookie({ uid: 'x' }) });
    assert.equal(badShape.status, 401);

    const ok = await authed('/api/me');
    assert.equal(ok.status, 200);
    assert.deepEqual(ok.json, {
      user: { id: USER_ID, username: 'alice', globalName: 'Alice', avatarUrl: discordApi.userAvatarUrl({ id: USER_ID, avatar: null }) },
    });
    assert.equal(ok.text.includes(TOKEN), false, 'the access token never leaves the server');
  });

  test('GET /api/catalog', async () => {
    assert.equal((await get('/api/catalog')).status, 401);
    const r = await authed('/api/catalog');
    assert.equal(r.status, 200);
    assert.deepEqual(r.json, JSON.parse(JSON.stringify(buildCatalog())));
  });

  test('GET /api/guilds lists manageable guilds, bot-present first, then by name', async () => {
    const r = await authed('/api/guilds');
    assert.equal(r.status, 200);
    assert.equal(r.headers['cache-control'], 'no-store');
    assert.deepEqual(r.json.guilds.map((g) => g.name), ['beta', 'Broken', 'zeta', 'Alpha']);
    const alpha = r.json.guilds.find((g) => g.id === G.alpha);
    assert.equal(alpha.botPresent, false);
    assert.equal(alpha.inviteUrl, fakes.discord.inviteUrl(G.alpha));
    assert.equal(alpha.iconUrl, discordApi.guildIconUrl({ id: G.alpha, icon: 'b'.repeat(32) }));
    const zeta = r.json.guilds.find((g) => g.id === G.zeta);
    assert.deepEqual(zeta, { id: G.zeta, name: 'zeta', iconUrl: null, botPresent: true, inviteUrl: null });
    assert.equal(r.json.guilds.some((g) => g.id === G.plain), false, 'no permission: not listed');
  });

  test('other users get no ownerGuilds and cannot open servers they do not manage', async () => {
    const list = await authed('/api/guilds');
    assert.equal(Object.hasOwn(list.json, 'ownerGuilds'), false);
    for (const id of [G.plain, G.botOnly, G.stranger]) {
      for (const path of [`/api/guilds/${id}/settings`, `/api/guilds/${id}/resources`, `/api/guilds/${id}/members?query=a`]) {
        const r = await authed(path);
        assert.equal(r.status, 403, path);
        assert.equal(r.json.error, 'forbidden', path);
      }
    }
  });

  test('the bot owner keeps their own list and gets every other server the bot is in apart', async () => {
    const cookie = sessionCookie({ uid: OWNER_ID });
    const list = await authed('/api/guilds', { cookie });
    assert.equal(list.status, 200);
    assert.deepEqual(list.json.guilds.map((g) => g.name), ['beta', 'Broken', 'zeta', 'Alpha'], 'their own list is unchanged');
    assert.deepEqual(list.json.ownerGuilds, [
      { id: G.botOnly, name: 'Bot only', iconUrl: null, botPresent: true, inviteUrl: null, ownerAccess: true, member: false },
      { id: G.plain, name: 'Plain', iconUrl: null, botPresent: true, inviteUrl: null, ownerAccess: true, member: true },
    ]);

    // A member without rights (Plain) and a server they are not in (Bot only) both open.
    for (const [id, name] of [[G.plain, 'Plain'], [G.botOnly, 'Bot only']]) {
      const r = await authed(`/api/guilds/${id}/settings`, { cookie });
      assert.equal(r.status, 200, id);
      assert.equal(r.json.guild.name, name);
    }
    // The bot still has to be there.
    const absent = await authed(`/api/guilds/${G.stranger}/settings`, { cookie });
    assert.equal(absent.status, 404);
    assert.equal(absent.json.error, 'bot_not_in_guild');
  });

  test('only the bot owner sees who added the bot; a database failure leaves it out', async () => {
    const joinedAt = new Date('2026-10-01T12:00:00.000Z');
    const stored = new Map([
      [G.zeta, { inviterId: '200000000000000001', inviterName: 'alice', source: 'audit', joinedAt }],
      [G.botOnly, { inviterId: null, inviterName: null, source: null, joinedAt }],
    ]);
    let failing = false;
    const asked = [];
    const botJoins = {
      listBotJoins: async (ids) => {
        asked.push(ids);
        if (failing) throw new Error('database down');
        return new Map(ids.filter((id) => stored.has(id)).map((id) => [id, stored.get(id)]));
      },
    };
    const withJoins = createDashboardServer({ config, logger: recordingLogger(), deps: { discord: fakes.discord, settings: fakes.settings, botJoins } });
    const joinsPort = await listen(withJoins);
    try {
      const mine = await call(joinsPort, 'GET', '/api/guilds', { headers: { cookie: sessionCookie() } });
      assert.equal(mine.status, 200);
      assert.equal([...mine.json.guilds].some((g) => Object.hasOwn(g, 'addedBy')), false);
      assert.deepEqual(asked, [], 'not even read for other users');

      const cookie = sessionCookie({ uid: OWNER_ID });
      const owner = await call(joinsPort, 'GET', '/api/guilds', { headers: { cookie } });
      assert.equal(owner.status, 200);
      const byId = new Map([...owner.json.guilds, ...owner.json.ownerGuilds].map((g) => [g.id, g]));
      assert.deepEqual(byId.get(G.zeta).addedBy, { userId: '200000000000000001', name: 'alice', source: 'audit', joinedAt: joinedAt.toISOString() });
      assert.deepEqual(byId.get(G.botOnly).addedBy, { userId: null, name: null, source: null, joinedAt: joinedAt.toISOString() });
      assert.equal(byId.get(G.plain).addedBy, null, 'nothing recorded yet');
      assert.equal(Object.hasOwn(byId.get(G.alpha), 'addedBy'), false, 'the bot is not there');

      failing = true;
      const down = await call(joinsPort, 'GET', '/api/guilds', { headers: { cookie } });
      assert.equal(down.status, 200);
      assert.equal([...down.json.guilds, ...down.json.ownerGuilds].some((g) => Object.hasOwn(g, 'addedBy')), false);
    } finally {
      await close(withJoins);
    }
  });

  test('Discord failures on /api/guilds', async () => {
    const expired = await authed('/api/guilds', { cookie: sessionCookie({ accessToken: 'expired-token' }) });
    assert.equal(expired.status, 401);
    assert.match(expired.headers['set-cookie'][0], /^dash_session=; /);
    const limited = await authed('/api/guilds', { cookie: sessionCookie({ accessToken: 'limited-token' }) });
    assert.equal(limited.status, 429);
    assert.equal(limited.json.error, 'rate_limited');
    assert.equal(limited.json.retryAfter, 7);
    assert.equal(limited.headers['retry-after'], '7');
    const down = await authed('/api/guilds', { cookie: sessionCookie({ accessToken: 'down-token' }) });
    assert.equal(down.status, 502);
    assert.equal(down.json.error, 'discord_unavailable');
  });

  test('only the read-only server list may use an old guild list; settings GET and PATCH never do', async () => {
    const uid = '444444444444444444';
    const writes = fakes.calls.writeSettings.length;
    for (const [accessToken, status, error] of [['stale-limited-token', 429, 'rate_limited'], ['stale-down-token', 502, 'discord_unavailable']]) {
      const cookie = sessionCookie({ uid, accessToken });
      const list = await authed('/api/guilds', { cookie });
      assert.equal(list.status, 200, `${accessToken}: the listing keeps the stale fallback`);
      assert.ok(list.json.guilds.some((g) => g.id === G.zeta));

      const read = await authed(`/api/guilds/${G.zeta}/settings`, { cookie });
      assert.equal(read.status, status, `${accessToken}: GET settings`);
      assert.equal(read.json.error, error);
      const write = await patch(G.zeta, { changes: { modEnabled: false } }, { cookie });
      assert.equal(write.status, status, `${accessToken}: PATCH settings`);
      assert.equal(write.json.error, error);
      if (status === 429) {
        for (const r of [read, write]) {
          assert.equal(r.json.retryAfter, 4);
          assert.equal(r.headers['retry-after'], '4');
        }
      }
    }
    assert.equal(fakes.calls.writeSettings.length, writes, 'nothing was written');
    const asked = (token) => fakes.calls.getUserGuilds.filter((c) => c.token === token).map((c) => c.allowStale);
    assert.deepEqual(asked('stale-limited-token'), [true, false, false]);
    assert.deepEqual(asked('stale-down-token'), [true, false, false]);
  });

  test('GET settings: 403 without permission, 404 without the bot, 200 shape', async () => {
    const plain = await authed(`/api/guilds/${G.plain}/settings`);
    assert.equal(plain.status, 403);
    assert.equal(plain.json.error, 'forbidden');
    assert.equal((await authed(`/api/guilds/${G.stranger}/settings`)).status, 403);
    assert.equal((await get(`/api/guilds/${G.zeta}/settings`)).status, 401);
    assert.equal((await authed('/api/guilds/abc/settings')).status, 400);

    const absent = await authed(`/api/guilds/${G.alpha}/settings`);
    assert.equal(absent.status, 404);
    assert.equal(absent.json.error, 'bot_not_in_guild');
    assert.equal(absent.json.inviteUrl, fakes.discord.inviteUrl(G.alpha));

    const r = await authed(`/api/guilds/${G.zeta}/settings`);
    assert.equal(r.status, 200);
    assert.deepEqual(Object.keys(r.json).sort(), ['general', 'guild', 'propagationSeconds', 'values']);
    assert.deepEqual(r.json.guild, { id: G.zeta, name: 'zeta', iconUrl: null });
    assert.deepEqual(Object.keys(r.json.values).sort(), Object.keys(fields.SETTINGS_FIELDS).sort(), 'every field');
    const switches = [...MODULE_NAMES.map((key) => MODULES[key].flag), ...Object.values(SUB_TOGGLES).flat().map((t) => t.field)];
    for (const field of switches) assert.equal(typeof r.json.values[field], 'boolean', field);
    assert.equal(r.json.values.modEnabled, true);
    assert.equal(r.json.values.voiceXpEnabled, true);
    assert.equal(r.json.values.welcomeUseEmbed, true, 'missing column reads as the default');
    assert.equal('levelingEnabled' in r.json.values, false);
    assert.equal('captchaMessageId' in r.json.values, false);
    assert.equal(r.json.values.timezone, 'Europe/Berlin');
    assert.equal(r.json.values.embedColor, 0x5865f2);
    assert.equal(r.json.values.customPrefix, '');
    assert.deepEqual(r.json.values.logIgnoredChannelIds, [CH.stale], 'a deleted channel is still shown');
    assert.equal(r.json.values.welcomeMessage, fields.DEFAULT_TEXTS.welcomeMessage, 'null shows the default text');
    assert.deepEqual(r.json.general, { timezone: 'Europe/Berlin', customPrefix: '', embedColor: '#5865F2' });
    assert.equal(r.json.propagationSeconds, 30);

    const broken = await authed(`/api/guilds/${G.broken}/settings`);
    assert.equal(broken.status, 503);
    assert.equal(broken.json.error, 'settings_unavailable');
    assert.equal((await call(port, 'DELETE', `/api/guilds/${G.zeta}/settings`, { headers: { cookie: sessionCookie() } })).status, 405);
  });

  test('PATCH settings: CSRF checks come first', async () => {
    const before = fakes.calls.writeSettings.length;
    const body = { changes: { modEnabled: false } };
    const noHeader = await patch(G.zeta, body, { 'x-dashboard': '' });
    assert.equal(noHeader.status, 403);
    const wrongOrigin = await patch(G.zeta, body, { origin: 'https://evil.example' });
    assert.equal(wrongOrigin.status, 403);
    assert.equal(wrongOrigin.json.error, 'forbidden');
    assert.equal(wrongOrigin.json.message, 'This dashboard is configured for http://localhost:8080. Open it there, or set DASHBOARD_URL to the address you use.');
    assert.equal(wrongOrigin.text.includes('evil.example'), false, 'the request origin is not echoed');
    const nullOrigin = await patch(G.zeta, body, { origin: 'null' });
    assert.equal(nullOrigin.status, 403);
    assert.equal(nullOrigin.json.message, 'Cross-origin requests are not allowed.');
    assert.equal((await patch(G.zeta, body, { 'content-type': 'text/plain' })).status, 415);
    assert.equal((await patch(G.zeta, body, { 'sec-fetch-site': 'cross-site' })).status, 403);
    assert.equal((await call(port, 'PATCH', `/api/guilds/${G.zeta}/settings`, { headers: { ...WRITE_HEADERS }, body: JSON.stringify(body) })).status, 401);
    assert.equal(fakes.calls.writeSettings.length, before);
  });

  test('PATCH settings: 400 on a bad body, 413 on a large one', async () => {
    const before = fakes.calls.writeSettings.length;
    for (const body of ['{', '', 'null', '[]', { changes: {} }, { changes: { modEnabled: 'false' } }, { changes: { levelingEnabled: true } },
      { changes: { nope: true } }, { changes: { captchaMessageId: '700000000000000002' } }, { changes: { automodConfig: {} } }]) {
      const r = await patch(G.zeta, body);
      assert.equal(r.status, 400, `body ${JSON.stringify(body)}`);
      assert.equal(r.json.error, 'invalid_request');
    }
    assert.equal(BODY_LIMIT_BYTES, 256 * 1024);
    const big = await patch(G.zeta, JSON.stringify({ changes: { modEnabled: true }, pad: 'x'.repeat(BODY_LIMIT_BYTES) }));
    assert.equal(big.status, 413);
    assert.equal(fakes.calls.writeSettings.length, before);
  });

  test('PATCH settings: 400 names the path of the first problem', async () => {
    const before = fakes.calls.writeSettings.length;
    const cases = [
      [{ xpMin: 0 }, 'xpMin'],
      [{ modEnabled: 'false' }, 'modEnabled'],
      [{ welcomeCardStyle: { enabled: true, background: 'nope' } }, 'welcomeCardStyle.background'],
      [{ automodBadWords: ['fine word', '!'] }, 'automodBadWords.1'],
      [{ infractionEscalation: [{ warnCount: 3, withinSeconds: null, action: 'KICK', durationSeconds: 60 }] }, 'infractionEscalation.0.durationSeconds'],
    ];
    const cookie = sessionCookie({ uid: '666666666666666666' }); // its own write budget
    for (const [changes, path] of cases) {
      const r = await patch(G.zeta, { changes }, { cookie });
      assert.equal(r.status, 400, JSON.stringify(changes));
      assert.equal(r.json.error, 'invalid_request');
      assert.equal(r.json.path, path);
      assert.equal(typeof r.json.message, 'string');
    }
    assert.equal(fakes.calls.writeSettings.length, before);
  });

  test('PATCH settings: 403/404 before any write', async () => {
    const before = fakes.calls.writeSettings.length;
    assert.equal((await patch(G.plain, { changes: { modEnabled: false } })).status, 403);
    const absent = await patch(G.alpha, { changes: { modEnabled: false } });
    assert.equal(absent.status, 404);
    assert.equal(absent.json.error, 'bot_not_in_guild');
    assert.equal(fakes.calls.writeSettings.length, before);
  });

  test('PATCH settings: writes exactly the validated changes and returns every value', async () => {
    const r = await patch(G.zeta, { changes: { voiceXpEnabled: false, modEnabled: false, timezone: 'europe/paris', welcomeMessage: 'Hi {user}!' } });
    assert.equal(r.status, 200);
    const last = fakes.calls.writeSettings.at(-1);
    assert.equal(last.guildId, G.zeta);
    assert.deepEqual(last.changes, { voiceXpEnabled: false, modEnabled: false, timezone: 'Europe/Paris', welcomeMessage: 'Hi {user}!' });
    assert.equal(last.ctx.discord, fakes.discord, 'the bridge gets the Discord client for its side effects');
    assert.equal(last.ctx.userId, USER_ID);
    assert.deepEqual(Object.keys(r.json), ['values']);
    assert.deepEqual(Object.keys(r.json.values).sort(), Object.keys(fields.SETTINGS_FIELDS).sort());
    assert.equal(r.json.values.voiceXpEnabled, false);
    assert.equal(r.json.values.modEnabled, false);
    assert.equal(r.json.values.timezone, 'Europe/Paris');
    const line = log.lines.find((l) => l.startsWith('info') && l.includes(G.zeta) && l.includes(USER_ID) && l.includes('changed guild'));
    assert.ok(line);
    assert.match(line, /voiceXpEnabled, modEnabled, timezone, welcomeMessage$/, 'field names are logged');
    assert.equal(line.includes('Hi {user}'), false, 'values are not logged');
    assert.equal(fakes.calls.resources.length, 0, 'no ids, no Discord lookup');

    const notices = await patch(G.zeta, { changes: { economyMaxBalance: 5000 } });
    assert.deepEqual(notices.json.notices, ['Lowered 2 balances to the new maximum.']);

    const failed = await patch(G.broken, { changes: { modEnabled: true } });
    assert.equal(failed.status, 500);
    assert.equal(failed.json.error, 'save_failed');
  });

  test('PATCH settings: channel and role ids are checked against the server', async () => {
    const cookie = sessionCookie({ uid: '700000000000000011' });
    const send = (changes, c = cookie) => patch(G.zeta, { changes }, { cookie: c });
    const bad = async (changes, path, pattern) => {
      const writes = fakes.calls.writeSettings.length;
      const r = await send(changes);
      assert.equal(r.status, 400, JSON.stringify(changes));
      assert.equal(r.json.error, 'invalid_request');
      assert.equal(r.json.path, path, r.json.message);
      if (pattern) assert.match(r.json.message, pattern);
      assert.equal(fakes.calls.writeSettings.length, writes, 'nothing written');
    };
    await bad({ logIgnoredChannelIds: [CH.text, CH.ghost] }, 'logIgnoredChannelIds.1', /does not exist/);
    const lookups = fakes.calls.resources.filter((x) => x.guildId === G.zeta);
    assert.deepEqual(lookups.slice(-2).map((x) => x.fresh), [false, true], 'an unknown id is looked up once more, fresh');
    await bad({ welcomeChannelId: CH.voice }, 'welcomeChannelId', /wrong kind/);
    await bad({ statsCategoryId: CH.text }, 'statsCategoryId', /wrong kind/);
    await bad({ modRoleIds: [G.zeta] }, 'modRoleIds.0', /@everyone/);
    await bad({ autoroleIds: [RL.low, RL.high] }, 'autoroleIds.1', /above the bot/);
    await bad({ autoroleIds: [RL.managed] }, 'autoroleIds.0', /managed/);
    await bad({ muteRoleId: RL.admin }, 'muteRoleId', /moderator or admin permissions/);
    await bad({ autoroleIds: [RL.mid] }, 'autoroleIds.0', /not below your highest role/);
    await bad({ automodRuleCaps: { channelIds: [CH.ghost] } }, 'automodRuleCaps.channelIds.0');

    // A stored id that was deleted since may stay; a moderator role may sit above the user (the bot never gives it).
    const ok = await send({ logIgnoredChannelIds: [CH.stale, CH.text], modRoleIds: [RL.stale, RL.high], autoroleIds: [RL.low] });
    assert.equal(ok.status, 200, ok.json?.message);
    assert.deepEqual(ok.json.values.logIgnoredChannelIds, [CH.stale, CH.text]);
    assert.deepEqual(fakes.calls.getMember.at(-1), { guildId: G.zeta, userId: '700000000000000011' });

    // The guild owner may pick any role below the bot without a member lookup.
    const lookupsBefore = fakes.calls.getMember.length;
    const owner = await send({ autoroleIds: [RL.mid] }, sessionCookie({ uid: GUILD_OWNER }));
    assert.equal(owner.status, 200, owner.json?.message);
    assert.equal(fakes.calls.getMember.length, lookupsBefore);
  });

  test('PATCH settings: cross-field rules answer 400 with the path; Discord failures 502', async () => {
    const cookie = sessionCookie({ uid: '700000000000000012' });
    const r = await patch(G.zeta, { changes: { xpMin: 30 } }, { cookie });
    assert.equal(r.status, 400);
    assert.equal(r.json.path, 'xpMin');
    assert.match(r.json.message, /cannot be higher than the maximum/);
    const both = await patch(G.zeta, { changes: { xpMin: 30, xpMax: 40 } }, { cookie });
    assert.equal(both.status, 200);
    assert.equal(both.json.values.xpMin, 30);
    const channel = await patch(G.zeta, { changes: { levelUpMode: 'CHANNEL' } }, { cookie });
    assert.equal(channel.json.path, 'levelUpMode');
    const down = await patch(G.beta, { changes: { welcomeChannelId: CH.text } }, { cookie });
    assert.equal(down.status, 502);
    assert.equal(down.json.error, 'discord_unavailable');
  });

  test('GET resources: access-checked, public shape only', async () => {
    const cookie = sessionCookie({ uid: '700000000000000013' });
    assert.equal((await get(`/api/guilds/${G.zeta}/resources`)).status, 401);
    assert.equal((await authed(`/api/guilds/${G.plain}/resources`, { cookie })).status, 403);
    assert.equal((await authed(`/api/guilds/${G.alpha}/resources`, { cookie })).json.error, 'bot_not_in_guild');
    assert.equal((await authed('/api/guilds/x/resources', { cookie })).status, 400);
    assert.equal((await authed(`/api/guilds/${G.beta}/resources`, { cookie })).status, 502);
    const r = await authed(`/api/guilds/${G.zeta}/resources`, { cookie });
    assert.equal(r.status, 200);
    assert.equal(r.headers['cache-control'], 'no-store');
    assert.deepEqual(Object.keys(r.json).sort(), ['bot', 'channels', 'emojis', 'roles']);
    assert.deepEqual(r.json.bot, { userId: BOT_ID, topRolePosition: 6 });
    assert.deepEqual(Object.keys(r.json.roles[0]).sort(), ['assignable', 'color', 'everyone', 'id', 'managed', 'mentionable', 'name', 'position']);
    assert.deepEqual(Object.keys(r.json.channels[0]).sort(), ['id', 'kind', 'name', 'nsfw', 'parentId', 'position', 'type']);
    assert.deepEqual(r.json.emojis, [{ id: '600000000000000001', name: 'coin', animated: false }]);
    assert.equal(fakes.calls.resources.at(-1).fresh, false);
    await authed(`/api/guilds/${G.zeta}/resources?fresh=1`, { cookie });
    assert.equal(fakes.calls.resources.at(-1).fresh, true);
    assert.equal((await call(port, 'POST', `/api/guilds/${G.zeta}/resources`, { headers: { cookie, ...WRITE_HEADERS }, body: '{}' })).status, 405);
  });

  test('writes are rate limited per user', async () => {
    const cookie = sessionCookie({ uid: '333333333333333333' });
    let last;
    for (let i = 0; i < 21; i++) last = await patch(G.zeta, '{', { cookie });
    assert.equal(last.status, 429);
    assert.equal(last.json.error, 'rate_limited');
    assert.ok(last.json.retryAfter >= 1 && last.json.retryAfter <= 10);
    assert.equal((await patch(G.zeta, '{')).status, 400, 'other users are not affected');
  });

  test('a handler that throws answers 500 instead of crashing', async () => {
    const r = await authed('/api/guilds', { cookie: sessionCookie({ accessToken: 'broken-token' }) });
    assert.equal(r.status, 500);
    assert.equal(r.json.error, 'internal_error');
    assert.equal((await get('/healthz')).status, 200, 'still serving');
  });

  test('static files: whitelist only, no traversal', async () => {
    const landing = await get('/');
    assert.equal(landing.status, 200);
    assert.match(landing.headers['content-type'], /^text\/html/);
    assert.match(landing.headers['content-security-policy'], /script-src 'self'/);
    assert.ok(landing.text.includes(`<p>${MODULE_NAMES.length} ${CODES.leveling} <a href="${fakes.discord.inviteUrl().replaceAll('&', '&amp;')}">Add</a></p>`),
      'the landing page is filled from the catalog and the invite link');
    const docs = await get('/docs');
    assert.equal(docs.status, 200);
    assert.match(docs.headers['content-type'], /^text\/html/);
    assert.ok(docs.text.includes('id="cmd-rank"') && docs.text.includes('id="mod-leveling"'), '/docs is filled from the catalog and commands.json');
    const changelog = await get('/changelog');
    assert.equal(changelog.status, 200);
    assert.match(changelog.text, /id="lot-\d{4}-\d{2}-\d{2}"/, '/changelog is filled from CHANGELOG.md');
    const png = await get('/demo/rank-card.png');
    assert.equal(png.status, 200);
    assert.equal(png.headers['content-type'], 'image/png');
    const index = await get('/dashboard');
    assert.equal(index.status, 200);
    assert.match(index.headers['content-type'], /^text\/html/);
    assert.match(index.headers['content-security-policy'], /script-src 'self'/);
    assert.equal((await get('/dashboard?error=login_failed')).status, 200);
    assert.equal((await get('/index.html')).text, index.text);
    assert.equal((await get('/dashboard/')).status, 404);
    const app = await get('/app.js');
    assert.equal(app.status, 200);
    assert.match(app.headers['content-type'], /^text\/javascript/);
    assert.equal((await get('/favicon.svg')).status, 404, 'missing file: 404, no crash');
    for (const path of ['/../package.json', '/%2e%2e/package.json', '/..%2fpackage.json', '/package.json', '/src/dashboard/server.js',
      '/public/app.js', '//etc/passwd', '/index.html/', '/app.js%00']) {
      const r = await get(path);
      assert.equal(r.status, 404, path);
    }
    assert.equal((await get('/api/nope')).json.error, 'not_found');
  });

  test('the privacy page needs no login and shows the contact in place of its marker', async () => {
    const r = await get('/privacy');
    assert.equal(r.status, 200);
    assert.match(r.headers['content-type'], /^text\/html/);
    assert.equal(r.text.includes('<!--contact-->'), false, 'marker replaced');
    assert.ok(r.text.includes(contactHtml(null)), 'no DASHBOARD_CONTACT: the fallback text');
    assert.ok(r.text.includes(`<p>${MODULE_NAMES.length} <a href="${fakes.discord.inviteUrl().replaceAll('&', '&amp;')}">Add</a></p>`),
      'the header and footer markers are filled');
    assert.equal((await get('/terms')).status, 404, 'missing file: 404');
    assert.equal((await get('/privacy.html')).status, 404, 'served by its route only');
  });

  test('contactHtml links an email or URL and escapes it', () => {
    assert.equal(contactHtml({ kind: 'email', value: 'a@b.co' }), '<a href="mailto:a@b.co">a@b.co</a>');
    assert.equal(contactHtml({ kind: 'url', value: 'https://x.co/?a=1&b="2"' }),
      '<a href="https://x.co/?a=1&amp;b=&quot;2&quot;">https://x.co/?a=1&amp;b=&quot;2&quot;</a>');
    assert.equal(/[<>]/.test(contactHtml(null)), false);
  });

  test('clientAddress believes X-Forwarded-For only from loopback or a trusted proxy', () => {
    const req = (peer, xff) => ({ socket: { remoteAddress: peer }, headers: xff === undefined ? {} : { 'x-forwarded-for': xff } });
    const trusted = new Set(['10.0.1.30']);
    assert.equal(clientAddress(req('203.0.113.9', '198.51.100.1')), '203.0.113.9', 'untrusted peer: header ignored');
    assert.equal(clientAddress(req('10.0.1.30', '198.51.100.1')), '10.0.1.30', 'no trusted list: header ignored');
    assert.equal(clientAddress(req('10.0.1.30', '6.6.6.6, 198.51.100.1'), trusted), '198.51.100.1', 'trusted: last entry');
    assert.equal(clientAddress(req('::ffff:10.0.1.30', '198.51.100.1'), trusted), '198.51.100.1', 'IPv4-mapped peer');
    assert.equal(clientAddress(req('10.0.1.30'), trusted), '10.0.1.30', 'trusted without a header: the peer');
    assert.equal(clientAddress(req('10.0.1.31', '198.51.100.1'), trusted), '10.0.1.31', 'another LAN host is not trusted');
    assert.equal(clientAddress(req('127.0.0.1', '198.51.100.1')), '198.51.100.1', 'loopback is always trusted');
  });

  test('static files: ETag and 304, gzip when it is smaller and asked for', async () => {
    const css = await get('/styles.css');
    assert.equal(css.status, 200);
    assert.equal(css.headers['cache-control'], 'no-cache');
    assert.match(css.headers.etag, /^"[A-Za-z0-9_-]{27}"$/);
    assert.equal(css.headers.vary, 'Accept-Encoding');
    assert.equal(css.headers['content-encoding'], undefined, 'no Accept-Encoding: identity');
    const zipped = await get('/styles.css', { 'accept-encoding': 'br, gzip;q=0.8' });
    assert.equal(zipped.headers['content-encoding'], 'gzip');
    assert.ok(zipped.raw.length < css.raw.length);
    assert.equal(gunzipSync(zipped.raw).toString('utf8'), css.text);
    assert.notEqual(zipped.headers.etag, css.headers.etag, 'each encoding has its own ETag');
    assert.equal((await get('/styles.css', { 'accept-encoding': 'gzip;q=0' })).headers['content-encoding'], undefined);

    for (const tag of [css.headers.etag, `W/${css.headers.etag}`, `"other", ${zipped.headers.etag}`, '*']) {
      const r = await get('/styles.css', { 'if-none-match': tag });
      assert.equal(r.status, 304, tag);
      assert.equal(r.raw.length, 0);
      assert.ok(r.headers.etag);
      assert.equal(r.headers['content-length'], undefined);
    }
    assert.equal((await get('/styles.css', { 'if-none-match': '"stale"' })).status, 200);
    // A tiny file gains nothing from gzip: always identity, no Vary.
    const index = await get('/dashboard', { 'accept-encoding': 'gzip' });
    assert.equal(index.headers['content-encoding'], undefined);
    assert.equal(index.headers.vary, undefined);
    assert.equal((await call(port, 'HEAD', '/styles.css')).raw.length, 0);
  });

  test('GET /auth/login redirects to Discord with a state cookie', async () => {
    const r = await get('/auth/login');
    assert.equal(r.status, 302);
    const location = new URL(r.headers.location);
    assert.equal(location.origin, 'https://discord.com');
    assert.equal(location.searchParams.get('client_id'), config.clientId);
    assert.equal(location.searchParams.get('scope'), 'identify guilds');
    assert.equal(location.searchParams.get('redirect_uri'), 'http://localhost:8080/auth/callback');
    const cookie = r.headers['set-cookie'][0];
    assert.match(cookie, /^dash_oauth_state=[A-Za-z0-9_-]{43}; Path=\/auth; HttpOnly; SameSite=Lax; Max-Age=600$/);
    assert.equal(cookie.split(';')[0].split('=')[1], location.searchParams.get('state'));
  });

  test('GET /auth/callback rejects a missing or mismatched state', async () => {
    const before = fakes.calls.exchangeCode.length;
    const cases = [
      ['/auth/callback?code=good-code&state=abc', `${auth.STATE_COOKIE}=different`],
      ['/auth/callback?code=good-code&state=abc', undefined],
      ['/auth/callback?code=good-code', `${auth.STATE_COOKIE}=abc`],
      ['/auth/callback?error=access_denied&state=abc', `${auth.STATE_COOKIE}=abc`],
      ['/auth/callback?state=abc', `${auth.STATE_COOKIE}=abc`],
    ];
    for (const [path, cookie] of cases) {
      const r = await get(path, cookie ? { cookie } : {});
      assert.equal(r.status, 302, path);
      assert.equal(r.headers.location, '/dashboard?error=login_failed');
      assert.ok(r.headers['set-cookie'].some((c) => /^dash_oauth_state=; Path=\/auth; .*Max-Age=0/.test(c)), 'state cookie cleared');
      assert.equal(r.headers['set-cookie'].some((c) => c.startsWith('dash_session=v1.')), false);
    }
    assert.equal(fakes.calls.exchangeCode.length, before, 'no code exchange without a matching state');
    const bad = await get('/auth/callback?code=bad-code&state=abc', { cookie: `${auth.STATE_COOKIE}=abc` });
    assert.equal(bad.headers.location, '/dashboard?error=login_failed');
  });

  test('GET /auth/callback logs in with a sealed session', async () => {
    const r = await get('/auth/callback?code=good-code&state=s1', { cookie: `${auth.STATE_COOKIE}=s1` });
    assert.equal(r.status, 302);
    assert.equal(r.headers.location, '/dashboard');
    const session = r.headers['set-cookie'].find((c) => c.startsWith('dash_session='));
    assert.match(session, /; Path=\/; HttpOnly; SameSite=Lax; Max-Age=604800$/);
    const value = decodeURIComponent(session.split(';')[0].slice('dash_session='.length));
    const payload = auth.unsealSession(value, KEY);
    assert.equal(payload.uid, USER_ID);
    assert.equal(payload.accessToken, TOKEN);
    assert.ok(payload.exp <= Date.now() + 7 * 24 * 3600 * 1000);
    const me = await get('/api/me', { cookie: `dash_session=${value}` });
    assert.equal(me.status, 200);
    assert.ok(log.lines.some((l) => l.includes('Dashboard login') && l.includes(USER_ID)));
  });

  test('POST /auth/logout clears the session and revokes the token', async () => {
    assert.equal((await call(port, 'POST', '/auth/logout', { headers: { cookie: sessionCookie() } })).status, 415);
    assert.equal((await get('/auth/logout')).status, 405);
    const r = await call(port, 'POST', '/auth/logout', { headers: { cookie: sessionCookie(), ...WRITE_HEADERS }, body: '{}' });
    assert.equal(r.status, 204);
    assert.equal(r.headers['content-length'], undefined, 'a 204 carries no Content-Length');
    assert.equal(r.text, '');
    assert.match(r.headers['set-cookie'][0], /^dash_session=; .*Max-Age=0/);
    await new Promise((res) => setImmediate(res));
    assert.deepEqual(fakes.calls.revokeToken, [TOKEN]);
    assert.deepEqual(fakes.calls.forgetUser, [USER_ID]);
  });

  test('POST /api/guilds/:id/invite: the bot owner gets a single-use invite; nobody else, and only where the bot is', async () => {
    const owner = sessionCookie({ uid: OWNER_ID });
    const invite = (guildId, cookie = owner, headers = WRITE_HEADERS) => call(port, 'POST', `/api/guilds/${guildId}/invite`, {
      headers: { cookie, ...headers },
      body: '{}',
    });

    // A channel the bot may not invite from is skipped (CH.text), the next one is used.
    const before = fakes.calls.invites.length;
    const ok = await invite(G.botOnly);
    assert.equal(ok.status, 200, ok.json?.message);
    assert.deepEqual(ok.json, { url: 'https://discord.gg/abc123', expiresAt: '2026-10-13T12:30:00.000Z', channel: { id: CH.news, name: 'news' } });
    const tried = fakes.calls.invites.slice(before);
    assert.deepEqual(tried.map((c) => c.channelId), [CH.text, CH.news]);
    assert.match(tried[1].opts.reason, /bot owner/, 'the audit log says who asked');

    // Nobody else, not even a Manage Server holder, and before anything shows whether the bot is there.
    for (const id of [G.zeta, G.botOnly, G.stranger]) {
      const r = await invite(id, sessionCookie());
      assert.equal(r.status, 403, id);
      assert.equal(r.json.error, 'forbidden');
    }
    assert.equal((await invite(G.botOnly, owner, { ...WRITE_HEADERS, 'x-dashboard': '0' })).status, 403, 'CSRF guard');
    assert.equal((await call(port, 'GET', `/api/guilds/${G.botOnly}/invite`, { headers: { cookie: owner } })).status, 405);

    const absent = await invite(G.stranger);
    assert.equal(absent.status, 404);
    assert.equal(absent.json.error, 'bot_not_in_guild');

    const noPerm = await invite(G.plain);
    assert.equal(noPerm.status, 422);
    assert.match(noPerm.json.message, /Create Invite permission/);

    const all = [CH.text, CH.news, CH.voice];
    for (const id of all) fakes.inviteDeny.add(id);
    try {
      const none = await invite(G.botOnly);
      assert.equal(none.status, 422);
      assert.match(none.json.message, /could not create an invite/);
    } finally {
      for (const id of all) fakes.inviteDeny.delete(id);
      fakes.inviteDeny.add(CH.text);
    }
    assert.equal(fakes.calls.invites.filter((c) => c.channelId === G.zeta).length, 0);
  });

  // Last on purpose: it writes the shared fake settings row.
  test('the bot owner writes to a server they are not in, as its owner: no role-hierarchy lookup, the bot rules stay', async () => {
    const cookie = sessionCookie({ uid: OWNER_ID });
    const send = (changes) => patch(G.botOnly, { changes }, { cookie });
    assert.equal((await send({ autoroleIds: [] })).status, 200);
    // RL.mid is not below a plain member's top role (it is refused for them above); the bot owner may give it.
    const ok = await send({ autoroleIds: [RL.mid] });
    assert.equal(ok.status, 200, ok.json?.message);
    assert.deepEqual(ok.json.values.autoroleIds, [RL.mid]);
    assert.equal(fakes.calls.getMember.some((c) => c.userId === OWNER_ID), false, 'no member lookup for the bot owner');
    const high = await send({ autoroleIds: [RL.high] });
    assert.equal(high.status, 400);
    assert.match(high.json.message, /above the bot/);
  });

  test('logs never contain tokens or secrets', () => {
    for (const line of log.lines) {
      assert.equal(line.includes(TOKEN), false, line);
      assert.equal(line.includes(SECRET), false, line);
      assert.equal(line.includes('good-code'), false, line);
    }
  });
});

// ── Records API ──────────────────────────────────────────────────────────────

/** A collection the records tests own, so they do not depend on the real collections. */
const THINGS = Object.freeze({
  key: 'things', module: 'embeds', label: 'Things', singular: 'thing', description: '', limit: 5, titleField: 'name',
  fields: [
    { field: 'name', type: 'text', label: 'Name', maxLength: 32, minLength: 1, required: true },
    { field: 'channelId', type: 'channel', label: 'Channel', channelTypes: ['text'], nullable: true, default: null },
    { field: 'roleId', type: 'role', label: 'Role', assignable: true, nullable: true, default: null },
  ],
  createOnly: ['name'],
  readOnly: [],
  actions: [{ id: 'post', label: 'Post', fields: [{ field: 'channelId', type: 'channel', label: 'Channel', channelTypes: ['text'], required: true }] }],
});

/** validateRecordInput's rules, over THINGS. */
function fakeValidateRecordInput(key, data, { mode, actionId, current }) {
  if (key !== 'things') throw new RecordError('not_found', 'Unknown list.');
  if (!schema.isPlainObject(data)) throw new schema.FieldError('', '"data" must be an object.');
  const list = mode === 'action' ? THINGS.actions.find((a) => a.id === actionId).fields : THINGS.fields;
  for (const k of Object.keys(data)) {
    if (!list.some((f) => f.field === k)) throw new schema.FieldError(k, `"${k}" is not something you can set here.`);
    if (mode === 'update' && THINGS.createOnly.includes(k)) throw new schema.FieldError(k, 'cannot be changed after creation');
  }
  const refs = schema.emptyRefs();
  const out = {};
  for (const f of list) {
    if (Object.hasOwn(data, f.field)) out[f.field] = schema.validateValue(f, data[f.field], f.field, refs);
    else if (mode !== 'update') {
      if (f.required) throw new schema.FieldError(f.field, `"${f.label}" is required.`);
      out[f.field] = f.default ?? null;
    }
  }
  if (mode === 'update' && current?.name === 'Locked') throw new schema.FieldError('channelId', 'Locked things keep their channel.');
  return { data: out, refs };
}

function createRecordFakes() {
  const calls = [];
  const items = new Map([
    ['t1', { id: 't1', name: 'One', channelId: CH.stale, roleId: null }],
    ['t3', { id: 't3', name: 'Locked', channelId: CH.text, roleId: null }],
  ]);
  const records = {
    async list(guildId, key) { calls.push(['list', guildId, key]); return [...items.values()]; },
    async get(guildId, key, id) { calls.push(['get', guildId, key, id]); return items.get(id) ?? null; },
    async create(guildId, key, data, actor) {
      calls.push(['create', guildId, key, data, actor]);
      if (data.name === 'taken') throw new RecordError('conflict', 'A thing with that name exists.', { path: 'name' });
      if (data.name === 'full') throw new RecordError('limit', 'This server already has 5 things.');
      if (data.name === 'boom') throw new Error('database down');
      if (data.name === 'half') throw new RecordError('failed', 'The thing was saved, but it was not posted.', { saved: true });
      const item = { id: 't2', ...data };
      items.set('t2', item);
      return item;
    },
    async update(guildId, key, id, data, actor) {
      calls.push(['update', guildId, key, id, data, actor]);
      const item = { ...items.get(id), ...data };
      items.set(id, item);
      return item;
    },
    async remove(guildId, key, id, actor) {
      calls.push(['remove', guildId, key, id, actor]);
      if (!items.delete(id)) throw new RecordError('not_found', 'That thing does not exist any more.');
    },
    async action(guildId, key, id, actionId, data, actor) {
      calls.push(['action', guildId, key, id, actionId, data, actor]);
      if (id === 't3') throw new RecordError('discord', 'The bot cannot post in that channel.');
      return { item: items.get(id), message: 'Posted.' };
    },
  };
  return { records, calls, items };
}

describe('HTTP records API', () => {
  const { config } = loadDashboardConfig(baseEnv());
  const fakes = createFakes();
  const rec = createRecordFakes();
  const log = recordingLogger();
  let server;
  let port;
  const base = `/api/guilds/${G.zeta}/records/things`;
  const cookieFor = (uid) => sessionCookie({ uid });
  const write = (method, path, body, { cookie = cookieFor('800000000000000001'), headers = {} } = {}) => call(port, method, path, {
    headers: { cookie, ...WRITE_HEADERS, ...headers },
    body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
  });
  const getAs = (path, uid = '800000000000000001') => call(port, 'GET', path, { headers: { cookie: cookieFor(uid) } });

  before(async () => {
    server = createDashboardServer({
      config, logger: log,
      deps: { discord: fakes.discord, settings: fakes.settings, records: rec.records,
        collections: { COLLECTIONS: { things: THINGS }, validateRecordInput: fakeValidateRecordInput } },
    });
    port = await listen(server);
  });
  after(() => close(server));

  test('a write that was saved before the bot failed says so (saved: true), so the browser never retries it', async () => {
    const r = await write('POST', base, { data: { name: 'half' } });
    assert.equal(r.status, 422);
    assert.equal(r.json.error, 'action_failed');
    assert.equal(r.json.saved, true);
    assert.match(r.json.message, /was saved, but/);
    const plain = await write('POST', base, { data: { name: 'full' } });
    assert.equal(plain.json.saved, undefined);
  });

  test('GET lists the items with the limit, access-checked', async () => {
    assert.equal((await call(port, 'GET', base)).status, 401);
    assert.equal((await getAs(`/api/guilds/${G.plain}/records/things`)).status, 403);
    assert.equal((await getAs(`/api/guilds/${G.alpha}/records/things`)).json.error, 'bot_not_in_guild');
    const r = await getAs(base);
    assert.equal(r.status, 200);
    assert.deepEqual(r.json, { items: [...rec.items.values()], limit: 5 });
    assert.deepEqual(rec.calls.at(-1), ['list', G.zeta, 'things']);
    const one = await getAs(`${base}/t1`);
    assert.deepEqual(one.json, { item: rec.items.get('t1') });
    assert.equal((await getAs(`${base}/t9`)).status, 404);
  });

  test('unknown lists and actions are 404, malformed ids 400', async () => {
    const r = await getAs(`/api/guilds/${G.zeta}/records/nope`);
    assert.equal(r.status, 404);
    assert.equal(r.json.error, 'not_found');
    assert.equal((await getAs(`${base}/bad.id`)).status, 400);
    assert.equal((await getAs(`${base}/${'x'.repeat(41)}`)).status, 400);
    assert.equal((await write('POST', `${base}/t1/actions/explode`, { data: {} })).status, 404);
    assert.equal((await getAs(`/api/guilds/x/records/things`)).status, 400);
    assert.equal((await write('PUT', base, { data: {} })).status, 405);
    assert.equal((await write('POST', `${base}/t1`, { data: {} })).status, 405);
  });

  test('POST creates with the validated data and the acting admin', async () => {
    const r = await write('POST', base, { data: { name: 'Two', channelId: CH.text } });
    assert.equal(r.status, 201, r.json?.message);
    assert.deepEqual(r.json, { item: { id: 't2', name: 'Two', channelId: CH.text, roleId: null } });
    const [op, guildId, key, data, actor] = rec.calls.at(-1);
    assert.deepEqual([op, guildId, key, data], ['create', G.zeta, 'things', { name: 'Two', channelId: CH.text, roleId: null }]);
    assert.deepEqual(actor, {
      userId: '800000000000000001',
      user: { id: '800000000000000001', username: 'alice', globalName: 'Alice', avatar: null },
      member: { owner: false, permissions: '32' },
    });
    assert.ok(log.lines.some((l) => l.includes('created things/t2') && l.includes(G.zeta)));
  });

  test('POST refuses bad data with the path, and maps store errors', async () => {
    const before = rec.calls.filter((c) => c[0] === 'create').length;
    const cases = [
      [{ data: { name: '' } }, 400, 'name'],
      [{ data: { channelId: CH.text } }, 400, 'name'],
      [{ data: { name: 'x', channelId: CH.ghost } }, 400, 'channelId'],
      [{ data: { name: 'x', channelId: CH.voice } }, 400, 'channelId'],
      [{ data: { name: 'x', roleId: RL.admin } }, 400, 'roleId'],
      [{ data: { name: 'x', roleId: RL.high } }, 400, 'roleId'],
      [{ data: { name: 'x', extra: 1 } }, 400, 'extra'],
      [{ name: 'x' }, 400, undefined],
      [{ data: { name: 'x' }, more: 1 }, 400, undefined],
      ['[]', 400, undefined],
      ['{', 400, undefined],
    ];
    for (const [body, status, path] of cases) {
      const r = await write('POST', base, body, { cookie: cookieFor('800000000000000002') });
      assert.equal(r.status, status, JSON.stringify(body));
      assert.equal(r.json.error, 'invalid_request');
      assert.equal(r.json.path, path, JSON.stringify(body));
    }
    assert.equal(rec.calls.filter((c) => c[0] === 'create').length, before, 'nothing reached the store');

    const taken = await write('POST', base, { data: { name: 'taken' } });
    assert.deepEqual([taken.status, taken.json.error, taken.json.path], [409, 'conflict', 'name']);
    const full = await write('POST', base, { data: { name: 'full' } });
    assert.deepEqual([full.status, full.json.error], [409, 'limit']);
    const boom = await write('POST', base, { data: { name: 'boom' } });
    assert.deepEqual([boom.status, boom.json.error], [500, 'save_failed']);
    assert.equal(boom.text.includes('database down'), false);
  });

  test('PATCH updates with the stored item as `current`; ids the item holds may stay', async () => {
    const kept = await write('PATCH', `${base}/t1`, { data: { channelId: CH.stale } }, { cookie: cookieFor('800000000000000003') });
    assert.equal(kept.status, 200, kept.json?.message);
    assert.equal(kept.json.item.channelId, CH.stale);
    const [op, , , id, data] = rec.calls.at(-1);
    assert.deepEqual([op, id, data], ['update', 't1', { channelId: CH.stale }]);
    const created = await write('PATCH', `${base}/t1`, { data: { name: 'New' } }, { cookie: cookieFor('800000000000000003') });
    assert.deepEqual([created.status, created.json.path], [400, 'name']);
    const locked = await write('PATCH', `${base}/t3`, { data: { channelId: CH.text } }, { cookie: cookieFor('800000000000000003') });
    assert.deepEqual([locked.status, locked.json.path], [400, 'channelId'], 'the collection rule saw the stored item');
    const missing = await write('PATCH', `${base}/t9`, { data: { channelId: CH.text } }, { cookie: cookieFor('800000000000000003') });
    assert.equal(missing.status, 404);
  });

  test('DELETE removes; a second DELETE is 404', async () => {
    const r = await write('DELETE', `${base}/t2`, '{}', { cookie: cookieFor('800000000000000004') });
    assert.equal(r.status, 204);
    assert.equal(r.text, '');
    assert.equal(rec.calls.at(-1)[0], 'remove');
    const empty = await write('DELETE', `${base}/t2`, undefined, { cookie: cookieFor('800000000000000004') });
    assert.deepEqual([empty.status, empty.json.error], [404, 'not_found'], 'an empty body is fine');
  });

  test('POST actions run with validated data; a Discord failure is 502', async () => {
    const cookie = cookieFor('800000000000000005');
    const r = await write('POST', `${base}/t1/actions/post`, { data: { channelId: CH.text } }, { cookie });
    assert.equal(r.status, 200, r.json?.message);
    assert.deepEqual(r.json, { item: rec.items.get('t1'), message: 'Posted.' });
    assert.deepEqual(rec.calls.at(-1).slice(0, 6), ['action', G.zeta, 'things', 't1', 'post', { channelId: CH.text }]);
    const missingField = await write('POST', `${base}/t1/actions/post`, { data: {} }, { cookie });
    assert.deepEqual([missingField.status, missingField.json.path], [400, 'channelId']);
    const failed = await write('POST', `${base}/t3/actions/post`, { data: { channelId: CH.text } }, { cookie });
    assert.deepEqual([failed.status, failed.json.error], [502, 'discord_failed']);
    assert.equal(failed.json.message, 'The bot cannot post in that channel.');
    assert.equal((await write('POST', `${base}/t9/actions/post`, { data: { channelId: CH.text } }, { cookie })).status, 404);
  });

  test('record writes need the CSRF headers, count towards the rate limit and have a body limit', async () => {
    const before = rec.calls.length;
    const noHeader = await write('POST', base, { data: { name: 'x' } }, { headers: { 'x-dashboard': '' } });
    assert.equal(noHeader.status, 403);
    const cross = await write('DELETE', `${base}/t1`, '{}', { headers: { origin: 'https://evil.example' } });
    assert.equal(cross.status, 403);
    const plain = await write('PATCH', `${base}/t1`, { data: {} }, { headers: { 'content-type': 'text/plain' } });
    assert.equal(plain.status, 415);
    assert.equal(rec.calls.length, before, 'nothing reached the store');

    const big = await write('POST', base, JSON.stringify({ data: { name: 'x'.repeat(RECORD_BODY_LIMIT_BYTES) } }), { cookie: cookieFor('800000000000000006') });
    assert.equal(big.status, 413);

    const cookie = cookieFor('800000000000000007');
    let last;
    for (let i = 0; i < 21; i++) last = await write('POST', base, '{', { cookie });
    assert.equal(last.status, 429);
    assert.equal(last.json.error, 'rate_limited');
  });

  test('the real registry: lists every collection and refuses unknown ones; no records bridge = 503', async () => {
    const real = createDashboardServer({ config, logger: recordingLogger(), deps: { discord: fakes.discord, settings: fakes.settings, records: rec.records } });
    const bare = createDashboardServer({ config, logger: recordingLogger(), deps: { discord: fakes.discord, settings: fakes.settings } });
    const realPort = await listen(real);
    const barePort = await listen(bare);
    try {
      const cookie = cookieFor('800000000000000008');
      for (const col of COLLECTION_LIST) {
        const r = await call(realPort, 'GET', `/api/guilds/${G.zeta}/records/${col.key}`, { headers: { cookie } });
        assert.equal(r.status, 200, col.key);
        assert.equal(r.json.limit, COLLECTIONS[col.key].limit);
      }
      assert.equal((await call(realPort, 'GET', `/api/guilds/${G.zeta}/records/things`, { headers: { cookie } })).status, 404);
      const r = await call(barePort, 'GET', `/api/guilds/${G.zeta}/records/${COLLECTION_LIST[0].key}`, { headers: { cookie } });
      assert.deepEqual([r.status, r.json.error], [503, 'unavailable']);
    } finally {
      await close(real);
      await close(bare);
    }
  });
});

describe('HTTP role checks across fields', () => {
  const { config } = loadDashboardConfig(baseEnv());
  const fakes = createFakes();
  const MEMBER = '800000000000000021'; // not the owner; highest role RL.mid (position 3)
  // A record with a role list the bot never gives, a role it gives, and a reward role that may carry moderator rights.
  const PERKS = Object.freeze({
    key: 'perks', module: 'embeds', label: 'Perks', singular: 'perk', description: '', limit: 5, titleField: 'name',
    fields: [
      { field: 'name', type: 'text', label: 'Name', maxLength: 32, minLength: 1, required: true },
      { field: 'pingRoleIds', type: 'roles', label: 'Ping roles', maxItems: 5, default: [] },
      { field: 'roleId', type: 'role', label: 'Role', assignable: true, nullable: true, default: null },
      { field: 'rewardRoleId', type: 'role', label: 'Reward', assignable: true, allowDangerous: true, nullable: true, default: null },
    ],
    createOnly: [], readOnly: [], actions: [],
  });
  // Actions whose roles are given or taken by type, with the type-aware keys automations use.
  const ACTS = Object.freeze({
    key: 'acts', module: 'embeds', label: 'Acts', singular: 'act', description: '', limit: 5, titleField: 'id',
    fields: [{
      field: 'actions', type: 'objectList', label: 'Actions', maxItems: 5, default: [],
      itemFields: [
        { field: 'type', type: 'enum', label: 'Type', options: [{ value: 'GIVE', label: 'Give' }, { value: 'TAKE', label: 'Take' }], default: 'GIVE' },
        { field: 'roleIds', type: 'roles', label: 'Roles', assignable: true, maxItems: 5, default: [] },
      ],
    }],
    createOnly: [], readOnly: [], actions: [],
    refKeys: (item, keys) => {
      const out = new Set([...keys].filter((k) => !k.startsWith('actions.roleIds|')));
      for (const a of item.actions ?? []) for (const id of a.roleIds ?? []) out.add(`actions.${a.type}|${id}`);
      return out;
    },
    refKeyOf: (ref, data) => {
      const m = /^actions\.(\d+)\.roleIds\.\d+$/.exec(ref.path);
      return m ? `actions.${data.actions[Number(m[1])].type}|${ref.id}` : schema.refKey(ref.path, ref.id);
    },
  });
  // A collection that opts in to re-checking every role of the saved record (automations and custom commands do).
  const LOCKS = Object.freeze({
    key: 'locks', module: 'embeds', label: 'Locks', singular: 'lock', description: '', limit: 5, titleField: 'name',
    fields: [
      { field: 'name', type: 'text', label: 'Name', maxLength: 32, minLength: 1, required: true },
      { field: 'enabled', type: 'boolean', label: 'Enabled', default: true },
      {
        field: 'actions', type: 'objectList', label: 'Actions', maxItems: 5, default: [],
        itemFields: [{ field: 'roleId', type: 'role', label: 'Role', assignable: true, nullable: true, default: null }],
      },
    ],
    createOnly: [], readOnly: [], actions: [], checkStoredRoles: true,
  });
  const items = new Map([
    ['p1', { id: 'p1', name: 'One', pingRoleIds: [RL.admin, RL.mid], roleId: null, rewardRoleId: null }],
    ['a1', { id: 'a1', actions: [{ type: 'TAKE', roleIds: [RL.mid] }, { type: 'GIVE', roleIds: [RL.admin] }] }],
    ['l-high', { id: 'l-high', name: 'High', enabled: false, actions: [{ roleId: RL.low }, { roleId: RL.mid }] }],
    ['l-low', { id: 'l-low', name: 'Low', enabled: false, actions: [{ roleId: RL.low }] }],
    ['n-high', { id: 'n-high', name: 'NoFlag', enabled: false, pingRoleIds: [], roleId: RL.mid, rewardRoleId: null }],
  ]);
  const records = {
    async get(guildId, key, id) { return items.get(id) ?? null; },
    async update(guildId, key, id, data) {
      const item = { ...items.get(id), ...data };
      items.set(id, item);
      return item;
    },
  };
  const validateRecordInput = (key, data) => {
    const refs = schema.emptyRefs();
    const out = {};
    const list = ({ acts: ACTS, locks: LOCKS }[key] ?? PERKS).fields;
    for (const f of list) if (Object.hasOwn(data, f.field)) out[f.field] = schema.validateValue(f, data[f.field], f.field, refs);
    return { data: out, refs };
  };
  let server;
  let port;
  before(async () => {
    server = createDashboardServer({
      config, logger: recordingLogger(),
      deps: { discord: fakes.discord, settings: fakes.settings, records, collections: { COLLECTIONS: { perks: PERKS, acts: ACTS, locks: LOCKS }, validateRecordInput } },
    });
    port = await listen(server);
  });
  after(() => close(server));
  const send = (method, path, body, uid = MEMBER) => call(port, method, path, {
    headers: { cookie: sessionCookie({ uid }), ...WRITE_HEADERS }, body: JSON.stringify(body),
  });
  const settingsPatch = (changes, uid) => send('PATCH', `/api/guilds/${G.zeta}/settings`, { changes }, uid);

  test('a role stored in one setting is checked again when it moves into a setting that gives it', async () => {
    assert.equal((await settingsPatch({ modRoleIds: [RL.admin, RL.mid] }, MEMBER)).status, 200, 'mod roles are never given');
    const dangerous = await settingsPatch({ autoroleIds: [RL.admin] }, MEMBER);
    assert.equal(dangerous.status, 400);
    assert.equal(dangerous.json.path, 'autoroleIds.0');
    assert.match(dangerous.json.message, /moderator or admin permissions/);
    const above = await settingsPatch({ autoroleIds: [RL.mid] }, MEMBER);
    assert.equal(above.status, 400);
    assert.match(above.json.message, /not below your highest role/);

    // Already in the same field: it may stay while another role is added.
    assert.equal((await settingsPatch({ autoroleIds: [RL.mid] }, GUILD_OWNER)).status, 200);
    const kept = await settingsPatch({ autoroleIds: [RL.mid, RL.low] }, MEMBER);
    assert.equal(kept.status, 200, kept.json?.message);
    const moved = await settingsPatch({ muteRoleId: RL.mid }, MEMBER);
    assert.equal(moved.status, 400, 'moved from the autoroles to the mute role: checked again');
  });

  test('a role stored in one field of a record is checked again in a field that gives it; allowDangerous fields accept it', async () => {
    const path = `/api/guilds/${G.zeta}/records/perks/p1`;
    const given = await send('PATCH', path, { data: { roleId: RL.admin } });
    assert.equal(given.status, 400);
    assert.equal(given.json.path, 'roleId');
    assert.match(given.json.message, /moderator or admin permissions/);
    const hierarchy = await send('PATCH', path, { data: { roleId: RL.mid } });
    assert.equal(hierarchy.status, 400);
    assert.match(hierarchy.json.message, /not below your highest role/);
    const reward = await send('PATCH', path, { data: { rewardRoleId: RL.admin } });
    assert.equal(reward.status, 200, reward.json?.message);
    assert.equal(items.get('p1').rewardRoleId, RL.admin);
  });

  test('a collection can key kept roles by item type: a Take role turned into a Give role is checked again', async () => {
    const path = `/api/guilds/${G.zeta}/records/acts/a1`;
    // RL.mid is at the member's highest role: kept while it stays a Take action...
    const kept = await send('PATCH', path, { data: { actions: [{ type: 'TAKE', roleIds: [RL.mid] }, { type: 'GIVE', roleIds: [RL.admin] }] } });
    assert.equal(kept.status, 200, kept.json?.message);
    // ...and checked like a new role once the action gives it.
    const flipped = await send('PATCH', path, { data: { actions: [{ type: 'GIVE', roleIds: [RL.mid] }, { type: 'GIVE', roleIds: [RL.admin] }] } });
    assert.equal(flipped.status, 400);
    assert.equal(flipped.json.path, 'actions.0.roleIds.0');
    assert.match(flipped.json.message, /not below your highest role/);
  });

  test('a collection that opts in re-checks every role of the saved record on any update (enabling, renaming, editing)', async () => {
    const high = `/api/guilds/${G.zeta}/records/locks/l-high`;
    const low = `/api/guilds/${G.zeta}/records/locks/l-low`;
    // The record holds a role at the member's highest role: even an enable-only change (no role in the request) is refused.
    const enable = await send('PATCH', high, { data: { enabled: true } });
    assert.equal(enable.status, 400);
    assert.match(enable.json.message, /gives or takes a role that is not below your highest role/);
    assert.equal(enable.json.path, undefined, 'the request names no role field');
    assert.equal(items.get('l-high').enabled, false, 'nothing was saved');
    const rename = await send('PATCH', high, { data: { name: 'Renamed' } });
    assert.equal(rename.status, 400);
    // Sending the held role back unchanged does not get it past the check either; the error points at the field.
    const same = await send('PATCH', high, { data: { actions: [{ roleId: RL.low }, { roleId: RL.mid }] } });
    assert.equal(same.status, 400);
    assert.equal(same.json.path, 'actions.1.roleId');
    // Taking the high role out of the record is allowed: what is saved holds only roles below the member's own.
    const trimmed = await send('PATCH', high, { data: { actions: [{ roleId: RL.low }] } });
    assert.equal(trimmed.status, 200, trimmed.json?.message);

    assert.equal((await send('PATCH', low, { data: { enabled: true } })).status, 200, 'a record of lower roles can be enabled');
    // The guild owner is exempt.
    items.set('l-high', { id: 'l-high', name: 'High', enabled: false, actions: [{ roleId: RL.mid }] });
    assert.equal((await send('PATCH', high, { data: { enabled: true } }, GUILD_OWNER)).status, 200);
    // A collection without the flag keeps the old behaviour: only new roles are checked.
    assert.equal((await send('PATCH', `/api/guilds/${G.zeta}/records/perks/n-high`, { data: { name: 'Quiet' } })).status, 200);
  });

  test('an empty required text says so', () => {
    assert.throws(() => schema.validateValue({ field: 'name', type: 'text', label: 'Name', maxLength: 50 }, null), /"Name" cannot be empty\./);
  });
});

describe('HTTP server over https', () => {
  let server;
  let port;
  before(async () => {
    const { config } = loadDashboardConfig(baseEnv({ DASHBOARD_URL: 'https://dash.example.com' }));
    const fakes = createFakes();
    server = createDashboardServer({ config, deps: { discord: fakes.discord, settings: fakes.settings }, logger: recordingLogger() });
    port = await listen(server);
  });
  after(() => close(server));

  test('sends HSTS and Secure cookies, and checks the public origin', async () => {
    const health = await call(port, 'GET', '/healthz');
    assert.match(health.headers['strict-transport-security'], /max-age=\d+/);
    const login = await call(port, 'GET', '/auth/login');
    assert.match(login.headers['set-cookie'][0], /; Secure$/);
    const r = await call(port, 'PATCH', `/api/guilds/${G.zeta}/settings`, {
      headers: { cookie: sessionCookie(), ...WRITE_HEADERS, origin: 'http://localhost:8080' },
      body: JSON.stringify({ changes: { modEnabled: true } }),
    });
    assert.equal(r.status, 403, 'the origin must be the https DASHBOARD_URL');
    assert.match(r.json.message, /configured for https:\/\/dash\.example\.com\. Open it there, or set DASHBOARD_URL/);
    assert.equal(r.text.includes('localhost'), false, 'the request origin is not echoed');
  });
});

describe('parked server', () => {
  let server;
  let port;
  before(async () => {
    server = createParkedServer({ missing: ['DISCORD_CLIENT_SECRET', 'DASHBOARD_SESSION_SECRET'], invalid: [{ key: 'DASHBOARD_URL', reason: 'x' }] });
    port = await listen(server);
  });
  after(() => close(server));

  test('/healthz stays 200 and everything else names the missing keys', async () => {
    const health = await call(port, 'GET', '/healthz');
    assert.equal(health.status, 200);
    assert.equal(health.json.ok, true);
    const page = await call(port, 'GET', '/');
    assert.equal(page.status, 503);
    assert.match(page.text, /missing DISCORD_CLIENT_SECRET, DASHBOARD_SESSION_SECRET; invalid DASHBOARD_URL/);
    const api = await call(port, 'GET', '/api/me');
    assert.equal(api.status, 503);
    assert.equal(api.json.error, 'not_configured');
    assert.equal(api.headers['x-content-type-options'], 'nosniff');
  });
});

// ── settingsBridge against a database (opt-in: TEST_DATABASE_URL) ────────────

describe('settingsBridge.writeSettings (database)', { skip: DB ? false : 'TEST_DATABASE_URL not set' }, () => {
  // Guild ids unique to this file.
  const GID = '955200000000000001';
  const LEGACY = '955200000000000002';
  let bridge;
  let prisma;
  let settingsService;
  let scheduler;
  let redis;
  const sent = [];
  const edited = [];
  const discord = {
    async getGuildResources(guildId) { return fakeResources(guildId); },
    async sendMessage(channelId, payload) { sent.push({ channelId, payload }); return { id: '700000000000000009', channelId }; },
    async editMessage(channelId, messageId, payload) { edited.push({ channelId, messageId, payload }); return { id: messageId, channelId }; },
  };
  const valid = (changes) => {
    const r = fields.validateSettingsChanges({ changes });
    assert.equal(r.ok, true, r.message);
    return r.changes;
  };
  const write = (guildId, changes) => bridge.writeSettings(guildId, valid(changes), { discord, logger: recordingLogger() });
  const cleanup = async () => {
    await prisma.scheduledJob.deleteMany({ where: { guildId: { in: [GID, LEGACY] } } });
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: [GID, LEGACY] } } });
    for (const g of [GID, LEGACY]) await settingsService.invalidateSettings(g);
  };

  before(async () => {
    bridge = await import('../src/dashboard/settingsBridge.js');
    ({ default: prisma } = await import('../src/database/client.js'));
    settingsService = await import('../src/services/settingsService.js');
    scheduler = await import('../src/services/scheduler.js');
    ({ default: redis } = await import('../src/services/redis.js'));
    await cleanup();
  });
  after(async () => {
    await cleanup();
    await prisma.$disconnect();
    redis.disconnect();
  });

  test('automod rules and lists merge into the stored config; a v1 config is upgraded first', async () => {
    await settingsService.updateSettings(LEGACY, { automodConfig: { filterInvites: true, automodAction: 'KICK', badWords: ['legacy'] } });
    const { row } = await write(LEGACY, { automodRuleCaps: { action: 'delete', threshold: 90 }, automodBadWords: ['legacy', 'new one'] });
    const cfg = row.automodConfig;
    assert.equal(cfg.version, 2);
    assert.equal(cfg.rules.invites.action, 'kick', 'the legacy invite filter survives');
    assert.deepEqual([cfg.rules.caps.action, cfg.rules.caps.threshold], ['delete', 90]);
    assert.deepEqual(cfg.badWords, ['legacy', 'new one']);
    const stored = await settingsService.getSettings(LEGACY, { fresh: true });
    assert.equal(stored.automodConfig.version, 2);
    assert.equal(fields.readSettingsValues(await bridge.readGuildSettings(LEGACY)).automodRuleInvites.action, 'kick');
  });

  test('escalation keeps tier ids, cross-field rules refuse and save nothing', async () => {
    await write(GID, { infractionEscalation: [{ warnCount: 3, withinSeconds: null, action: 'TIMEOUT', durationSeconds: 600 }] });
    const { row } = await write(GID, { infractionEscalation: [
      { warnCount: 3, withinSeconds: null, action: 'KICK', durationSeconds: null },
      { warnCount: 5, withinSeconds: null, action: 'BAN', durationSeconds: null },
    ] });
    assert.deepEqual(row.infractionEscalation.map((t) => [t.id, t.action]), [[1, 'KICK'], [2, 'BAN']]);
    await assert.rejects(write(GID, { infractionEscalation: [{ warnCount: 2, withinSeconds: null, action: 'MUTE', durationSeconds: null }] }),
      (err) => err instanceof schema.FieldError && err.path === 'infractionEscalation.0.action');
    await assert.rejects(write(GID, { xpMin: 500 }), (err) => err instanceof schema.FieldError && err.path === 'xpMin');
    const after = await settingsService.getSettings(GID, { fresh: true });
    assert.equal(after.infractionEscalation.length, 2);
    assert.equal(after.xpMin, 15);
  });

  test('a welcome embed write keeps the effective text and the card flag; a cleared Json column is SQL NULL', async () => {
    const { row } = await write(GID, { welcomeEmbed: { title: 'Hello {user}' } });
    assert.equal(row.welcomeMessage, fields.DEFAULT_TEXTS.welcomeMessage, 'the text in effect is frozen');
    assert.equal(row.welcomeCardStyle.enabled, true);
    assert.equal(row.welcomeEmbed.title, 'Hello {user}');
    await write(GID, { welcomeEmbed: null, welcomeDmEmbed: null });
    const [{ cleared }] = await prisma.$queryRaw`SELECT "welcomeEmbed" IS NULL AS cleared FROM "GuildSettings" WHERE "guildId" = ${GID}`;
    assert.equal(cleared, true);
  });

  test('side effects: captcha kicks, the captcha panel, birthdays, balances and the moderation log', async () => {
    await scheduler.scheduleJob({ guildId: GID, type: 'captcha.timeout', runAt: new Date(Date.now() + 3_600_000),
      payload: { userId: USER_ID }, dedupeKey: `captcha.timeout:${GID}:${USER_ID}` });
    await write(GID, { captchaTimeoutMinutes: 30 });
    const zero = await write(GID, { captchaTimeoutMinutes: 0 });
    assert.deepEqual(zero.notices, ['Cancelled 1 scheduled kick of unverified members.']);
    assert.equal(await scheduler.getJob(`captcha.timeout:${GID}:${USER_ID}`), null);

    await settingsService.updateSettings(GID, { captchaChannelId: CH.text, captchaMessageId: '700000000000000001' });
    const panel = await write(GID, { captchaVerifyButton: { label: 'Let me in', style: 'PRIMARY' } });
    assert.deepEqual(panel.notices, []);
    const edit = edited.at(-1);
    assert.deepEqual([edit.channelId, edit.messageId], [CH.text, '700000000000000001']);
    assert.equal(edit.payload.components[0].components[0].label, 'Let me in');
    assert.deepEqual(edit.payload.allowed_mentions, { parse: [] });
    assert.match(edit.payload.embeds[0].description, /zeta/, '{server} rendered with the server name');

    await write(GID, { birthdayChannelId: CH.text });
    assert.equal((await scheduler.getJob(`birthday.tick:${GID}`))?.status, 'PENDING');

    await prisma.economyAccount.createMany({ data: [
      { guildId: GID, userId: '955200000000000101', balance: 900 }, { guildId: GID, userId: '955200000000000102', balance: 50 },
    ] });
    const clamp = await write(GID, { economyMaxBalance: 100, economyStartingBalance: 10 });
    assert.deepEqual(clamp.notices, ['Lowered 1 balance to the new maximum.']);
    const balances = await prisma.economyAccount.findMany({ where: { guildId: GID }, orderBy: { userId: 'asc' } });
    assert.deepEqual(balances.map((a) => a.balance), [100, 50]);

    await write(GID, { modLogChannelId: CH.news });
    assert.equal(sent.at(-1).channelId, CH.news);
    assert.match(sent.at(-1).payload.embeds[0].title, /Moderation Log/);
  });

  test('every field round-trips through the database', async () => {
    const ROUND = '955200000000000003';
    try {
      const values = fields.readSettingsValues(null);
      const changes = Object.fromEntries(Object.entries(values)
        .filter(([field, v]) => v !== null || fields.SETTINGS_FIELDS[field].nullable));
      Object.assign(changes, {
        welcomeChannelId: CH.text, goodbyeChannelId: CH.text, levelUpChannelId: CH.news, achievementChannelId: CH.text,
        captchaRoleId: RL.low, liveRoleId: RL.mid, muteRoleId: RL.mid, embedColor: 0x123456, customPrefix: '?',
        logEvents: ['voiceJoin'], xpBoostRoles: [{ roleId: RL.mid, multiplier: 2.5 }], automodBadWords: ['nope'],
        infractionEscalation: [{ warnCount: 2, withinSeconds: 86_400, action: 'MUTE', durationSeconds: null }],
        economyGameChannelIds: [CH.text], welcomeDmEmbed: { title: 'DM' }, modVisibility: 'ANONYMOUS', xpMultiplier: 1.5,
      });
      const { row } = await write(ROUND, changes);
      const back = fields.readSettingsValues(row);
      // What the database gives back equals what the pure rules computed.
      const expected = fields.readSettingsValues(fields.toStoredChanges(valid(changes), null));
      for (const field of Object.keys(changes)) assert.deepEqual(back[field], expected[field], field);
      const again = fields.readSettingsValues(await bridge.readGuildSettings(ROUND));
      assert.deepEqual(again, back, 'what was saved is what is read');
      assert.equal(back.customPrefix, '?');
      assert.equal(back.embedColor, 0x123456);
      assert.deepEqual(back.xpBoostRoles, [{ roleId: RL.mid, multiplier: 2.5 }]);
      assert.equal(back.welcomeDmEmbed.title, 'DM');
      assert.equal(back.infractionEscalation[0].withinSeconds, 86_400);
    } finally {
      await prisma.guildSettings.deleteMany({ where: { guildId: ROUND } });
      await settingsService.invalidateSettings(ROUND);
    }
  });

  test('a card background that cannot be downloaded is refused with its path', async () => {
    await assert.rejects(write(GID, { rankCardStyle: { backgroundUrl: 'https://127.0.0.1/bg.png' } }),
      (err) => err instanceof schema.FieldError && err.path === 'rankCardStyle.backgroundUrl');
  });
});

test('createRateLimiter allows `max` hits per window', () => {
  let t = 0;
  const hit = createRateLimiter({ max: 2, windowMs: 1_000, now: () => t });
  assert.equal(hit('a').ok, true);
  assert.equal(hit('a').ok, true);
  assert.deepEqual(hit('a'), { ok: false, retryAfter: 1 });
  assert.equal(hit('b').ok, true);
  t = 1_001;
  assert.equal(hit('a').ok, true);
});
