// Reference data for feat-general, produced by the JS bot's own code (no servers needed):
//   node rust/fixtures/gen/feat-general-golden.mjs
// writes rust/fixtures/golden/feat-general/cases.json:
//   help:     buildGroups / buildPages for every level and several settings rows, showCommand for every command, the
//             autocomplete; the full bot's 79 commands as loaded from src/commands (client.commands).
//   helpers:  customPrefixOf, customCommandLine, tagRolesAllow.
//   module:   /module list and the /module autocomplete (execute and autocomplete driven with a settings stub).
//   settings: parseColorInput, suggestColors, validatePrefix, buildOverviewEmbed.
//   info:     keyPermissionNames, formatRoleMentions, countChannels, isPublicFeature and the three embed builders on
//             the fakes of test/mod-utilities.test.js.
// help.js and module.js keep their builders private, so they are imported from a copy whose relative imports point at
// src/ (and, for module.js, at a settings stub) with the private functions exported; the copy lives in this
// directory next to this script (bare imports resolve from the repository) and is deleted afterwards.
import { mkdirSync, readFileSync, readdirSync, statSync, unlinkSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-general');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.BOT_OWNER_ID = '';
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};

const { Collection, PermissionsBitField, PermissionFlagsBits, UserFlags, UserFlagsBitField } = await import('discord.js');
const { default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { MODULES, MODULE_NAMES } = await imp('src', 'config', 'modules.js');

const temps = [];
function tempFile(name, text) {
  const file = join(ROOT, 'rust', 'fixtures', 'gen', `.feat-general-tmp-${process.pid}-${name}`);
  writeFileSync(file, text);
  temps.push(file);
  return file;
}

// A settings stub for module.js: the row is globalThis.__settings, writes are recorded.
const stub = tempFile('settings-stub.mjs', `
export const getSettingsSafe = async () => globalThis.__settings ?? null;
export const peekSettings = () => globalThis.__settings ?? null;
export const updateSettings = async (g, data) => { (globalThis.__updates ??= []).push(data); return { ...(globalThis.__settings ?? {}), ...data }; };
export const refreshSnapshot = async () => ({ ok: true });
export const cache = { invalidate: async () => 0 };
`);

async function loadPatched(rel, exportsLine, stubs = {}) {
  const abs = join(ROOT, 'src', rel);
  let text = readFileSync(abs, 'utf8');
  text = text.replace(/from '(\.\.?\/[^']+)'/g, (_m, p) => {
    const target = resolve(dirname(abs), p);
    for (const [suffix, file] of Object.entries(stubs)) {
      if (target.split(/[\\/]/).join('/').endsWith(suffix)) return `from '${pathToFileURL(file).href}'`;
    }
    return `from '${pathToFileURL(target).href}'`;
  });
  if (exportsLine) text += `\nexport { ${exportsLine} };\n`;
  return import(pathToFileURL(tempFile(basename(rel), text)).href);
}

const strip = (e) => {
  const j = typeof e?.toJSON === 'function' ? e.toJSON() : e;
  const { timestamp: _t, ...rest } = j;
  return rest;
};

// ── The full bot's commands, as loadCommands fills client.commands ─────────────
function walk(dir) {
  return readdirSync(dir).sort().flatMap((entry) => {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) return walk(full);
    return entry.endsWith('.js') && !entry.startsWith('_') ? [full] : [];
  });
}
const client = { commands: new Collection(), commandCategories: new Collection() };
for (const file of walk(join(ROOT, 'src', 'commands'))) {
  const ns = await import(pathToFileURL(file).href);
  if (!ns.data?.toJSON) continue;
  client.commands.set(ns.data.name, ns);
  client.commandCategories.set(ns.data.name, basename(dirname(file)));
}

// ── Settings rows ─────────────────────────────────────────────────────────────
const baseline = JSON.parse(readFileSync(join(ROOT, 'rust', 'fixtures', 'golden', 'mockdb', 'baseline.json'), 'utf8'));
const base = baseline.GuildSettings.min;
const flags = MODULE_NAMES.map((k) => MODULES[k].flag);
const allOn = { ...base, ...Object.fromEntries(flags.map((f) => [f, true])) };
const allOff = { ...base, ...Object.fromEntries(flags.map((f) => [f, false])) };
const SETTINGS = { none: null, base, allOn, allOff, colored: { ...base, embedColor: 0x123456 } };

// ── /help ─────────────────────────────────────────────────────────────────────
const help = await loadPatched('commands/utility/help.js', 'buildGroups, buildPages, showCommand, visibility');
const helpCases = { groups: [], show: [], autocomplete: [] };
for (const [sname, settings] of Object.entries(SETTINGS)) {
  for (const level of [0, 1, 2, 3]) {
    const { groups, count } = help.buildGroups(client, level, settings);
    const pages = help.buildPages(groups, settings, level, count).map(strip);
    helpCases.groups.push({ settings: sname, level, groups, count, pages });
  }
}
for (const [sname, level] of [['base', 3], ['allOff', 0], ['allOff', 2], ['none', 1], ['colored', 2]]) {
  for (const name of [...client.commands.keys(), 'nope']) {
    let answer = null;
    const interaction = { editReply: async (p) => { answer = p; } };
    await help.showCommand(interaction, client, ` /${name.toUpperCase()} `, level, SETTINGS[sname]);
    helpCases.show.push({ settings: sname, level, name, content: answer.content ?? null, embed: answer.embeds ? strip(answer.embeds[0]) : null });
  }
}
// The autocomplete through its entry point; the level comes from the member (no settings offline: peek is empty).
const memberAt = (level) => ({
  id: level === 3 ? 'owner-id' : 'u1',
  permissions: new PermissionsBitField(level === 2 ? ['ManageGuild'] : level === 1 ? ['KickMembers'] : []),
  roles: { cache: new Collection() },
  guild: { ownerId: 'someone-else' },
});
process.env.BOT_OWNER_ID = 'owner-id';
for (const level of [0, 1, 2, 3]) {
  for (const focused of ['', 'mod', ' /BAN', 'zzz', 'e']) {
    let choices = null;
    const interaction = {
      guildId: '1', member: memberAt(level), responded: false,
      options: { getFocused: () => focused, getSubcommand: () => null },
      respond: async (c) => { choices = c; },
    };
    await help.autocomplete(interaction, client);
    helpCases.autocomplete.push({ level, focused, choices });
  }
}
process.env.BOT_OWNER_ID = '';

const helpers = {
  prefixes: ['', '!', '?', '>>', '12345', '123456', 'two words', ' ', '\t!', 'é', '😀😀', ' ', '﻿', null, 5].map((p) => ({ p, out: help.customPrefixOf(p) })),
  lines: [
    { name: 'rules', description: 'Server rules', prefix: '!' },
    { name: 'faq', description: null, prefix: '>>' },
    { name: 'x', description: 'd'.repeat(300), prefix: null },
  ].map((c) => ({ ...c, out: help.customCommandLine(c, c.prefix) })),
};

// ── /module ───────────────────────────────────────────────────────────────────
const mod = await loadPatched('commands/admin/module.js', 'resolveModuleKey', {
  'services/settingsService.js': stub, 'services/inviteTracker.js': stub, 'services/redis.js': stub,
});
const moduleCases = { list: [], autocomplete: [], resolve: [] };
for (const sname of ['base', 'allOn', 'colored']) {
  globalThis.__settings = SETTINGS[sname];
  let answer = null;
  const interaction = {
    guildId: '1', options: { getSubcommand: () => 'list' },
    deferReply: async () => {}, editReply: async (p) => { answer = p; },
  };
  await mod.execute(interaction);
  moduleCases.list.push({ settings: sname, embed: strip(answer.embeds[0]) });
}
for (const sname of ['none', 'base']) {
  globalThis.__settings = SETTINGS[sname];
  for (const sub of ['enable', 'disable']) {
    for (const focused of ['', 'lev', 'AUDIT', 'zzz', ' Wel ']) {
      let choices = null;
      const interaction = { guildId: '1', responded: false, options: { getSubcommand: () => sub, getFocused: () => focused }, respond: async (c) => { choices = c; } };
      await mod.autocomplete(interaction);
      moduleCases.autocomplete.push({ settings: sname, sub, focused, choices });
    }
  }
}
for (const input of ['leveling', ' LEVELING ', 'Audit Log', 'audit log', 'nope', '', 'xp']) moduleCases.resolve.push({ input, out: mod.resolveModuleKey(input) });
globalThis.__settings = null;

// ── /settings ─────────────────────────────────────────────────────────────────
const settingsCmd = await imp('src', 'commands', 'admin', 'settings.js');
const NOW = new Date(Date.UTC(2026, 6, 1, 12, 34, 0));
const guild = (name, ids) => ({ name, roles: { cache: new Map(ids.map((id) => [id, { id }])) } });
const R1 = '800000000000000001';
const R2 = '800000000000000002';
const settingsCases = {
  now: NOW.getTime(),
  parseColor: ['#5865F2', '#fff', 'RED', ' blurple ', 'black', '#000', '#000000', '0x123456', 'reset', 'Default', 'none', 'clear', '', 'nope', '#12', 'grey', 'gray', '#GGGGGG'].map((input) => ({ input, out: settingsCmd.parseColorInput(input) })),
  suggest: [],
  prefix: ['!', ' ? ', 'off', 'OFF', '', '  ', 'two words', '123456', '😀😀😀😀😀', '😀😀😀😀😀😀', '/', '<', '@', '#', ':', '\\', '`', 'a​', 'a\u0007', '$', '>>', ' x', 'x﻿'].map((input) => ({ input, out: settingsCmd.validatePrefix(input) })),
  overview: [],
};
for (const current of [null, 0x123456]) {
  for (const q of ['', 're', '#abc', 'red', 'xyz', '#000', 'blurple', 'r', '  #ABCDEF0123456789  ', 'gr', 'de']) {
    settingsCases.suggest.push({ q, current, out: settingsCmd.suggestColors(q, { current }) });
  }
}
const overviewRows = {
  base,
  custom: { ...base, timezone: 'America/New_York', embedColor: 0xff0000, customPrefix: '', customCommandsEnabled: true, modRoleIds: [R1, R2, R1], urbanNsfwOnly: false },
  invalidTz: { ...base, timezone: 'Mars/Base`x', customPrefix: '?', modRoleIds: [R2] },
  allOn: { ...allOn, modRoleIds: [R1] },
  allOff,
};
for (const [name, row] of Object.entries(overviewRows)) {
  for (const g of [guild('My *Server*', [R1]), guild(undefined, [])]) {
    settingsCases.overview.push({ settings: name, guildName: g.name ?? null, roles: [...g.roles.cache.keys()], embed: strip(settingsCmd.buildOverviewEmbed(row, g, { now: NOW })) });
  }
}

// ── /info ─────────────────────────────────────────────────────────────────────
const info = await imp('src', 'commands', 'utility', 'info.js');
const permSets = [['Administrator', 'BanMembers'], ['BanMembers', 'ManageRoles'], ['SendMessages'], [], ['ManageGuild', 'ViewAuditLog', 'MentionEveryone', 'ManageGuildExpressions', 'ModerateMembers'], Object.keys(PermissionFlagsBits).filter((k) => k !== 'Administrator')];
const infoCases = {
  keyPermissions: permSets.map((names) => ({ bits: new PermissionsBitField(names).bitfield.toString(), out: info.keyPermissionNames(new PermissionsBitField(names)) })),
  roleMentions: [],
  features: ['VANITY_URL', 'COMMUNITY', 'PIN_PERMISSION_MIGRATION_COMPLETE', 'SOME_EXPERIMENT', 'X_OVERRIDE', 'THREADS_ENABLED', 'threads_enabled', 'NEWS', 'GUILD_ONBOARDING_HAS_PROMPTS', 'BYPASS_SLOWMODE_PERMISSION_MIGRATION', 'ROLE_ICONS'].map((f) => ({ f, out: info.isPublicFeature(f) })),
  channels: info.countChannels([0, 5, 2, 13, 15, 16, 4, 11, 12, 10, 14].map((type) => ({ type }))),
  server: [],
  role: [],
  user: [],
};
const many = Array.from({ length: 200 }, (_, i) => ({ id: String(100_000_000_000_000_000n + BigInt(i)), position: i + 1 }));
for (const [roles, gid, max] of [[[{ id: '900', position: 0 }, { id: '1', position: 1 }, { id: '3', position: 3 }, { id: '2', position: 2 }], '900', 1024], [[{ id: '900', position: 0 }], '900', 1024], [many, '900', 1024], [many, '900', 100]]) {
  infoCases.roleMentions.push({ roles, guildId: gid, max, out: info.formatRoleMentions(roles, gid, max) });
}

// The fakes of test/mod-utilities.test.js, described as the plain views the Rust builders read.
function fakeGuild(view, rest) {
  const channels = view.channelTypes.map((type, i) => ({ id: `c${i}`, type }));
  const guild = {
    id: view.id, name: view.name, description: view.description, ownerId: view.ownerId, memberCount: view.memberCount,
    createdTimestamp: view.createdMs, premiumTier: view.premiumTier, premiumSubscriptionCount: view.premiumSubscriptionCount,
    verificationLevel: view.verificationLevel, explicitContentFilter: view.explicitContentFilter, mfaLevel: view.mfaLevel,
    preferredLocale: view.preferredLocale, vanityURLCode: view.vanityUrlCode, features: view.features,
    iconURL: () => view.iconUrl, bannerURL: () => view.bannerUrl,
    channels: { cache: new Collection(channels.map((c) => [c.id, c])) },
    emojis: { cache: new Collection(view.emojisAnimated.map((a, i) => [`e${i}`, { animated: a }])) },
    roles: { cache: new Collection(Array.from({ length: view.roleCount }, (_, i) => [i === 0 ? view.id : `r${i}`, { id: i === 0 ? view.id : `r${i}`, position: i }])) },
    client: { rest: { async get() { if (rest === 'fail') throw new Error('rate limited'); return rest; } } },
  };
  return guild;
}
const serverViews = [
  [{ id: '9001', name: 'My *Server*', description: 'A _cool_ place', ownerId: '42', memberCount: 321, createdMs: 1_600_000_000_000, premiumTier: 2, premiumSubscriptionCount: 9, verificationLevel: 2, explicitContentFilter: 1, mfaLevel: 1, preferredLocale: 'en-US', vanityUrlCode: 'cool', features: ['VANITY_URL', 'COMMUNITY', 'ANIMATED_ICON', 'PIN_PERMISSION_MIGRATION_COMPLETE'], iconUrl: 'https://cdn.example/icon.png', bannerUrl: null, channelTypes: [0, 0, 5, 2, 13, 15, 4, 11], emojisAnimated: [true, false], roleCount: 1 }, { approximate_member_count: 1234, approximate_presence_count: 77, stickers: [{ id: 's1' }] }],
  [{ id: '9002', name: 'x'.repeat(300), description: null, ownerId: '43', memberCount: 1500, createdMs: 1_600_000_000_999, premiumTier: 0, premiumSubscriptionCount: null, verificationLevel: 9, explicitContentFilter: 2, mfaLevel: 0, preferredLocale: 'en_*GB*', vanityUrlCode: null, features: ['PIN_PERMISSION_MIGRATION_COMPLETE'], iconUrl: null, bannerUrl: 'https://cdn.example/banner.png', channelTypes: [], emojisAnimated: [], roleCount: 5 }, 'fail'],
];
for (const [view, rest] of serverViews) {
  const counts = rest === 'fail' ? { members: view.memberCount, online: null, stickers: null } : { members: rest.approximate_member_count, online: rest.approximate_presence_count, stickers: rest.stickers.length };
  for (const settings of [null, SETTINGS.colored]) {
    infoCases.server.push({ view, counts, settings: settings ? 'colored' : 'none', embed: strip(await info.buildServerEmbed(fakeGuild(view, rest), settings)) });
  }
}
const roleViews = [
  [{ id: '31', name: 'Admins', unicodeEmoji: '🛡️', primaryColor: 0xff0000, secondaryColor: 0x00ff00, iconUrl: null, position: 7, createdMs: 1_650_000_000_000, hoist: true, mentionable: false, managed: false, tags: null, permissions: ['KickMembers', 'ManageMessages'] }, 'counts'],
  [{ id: '32', name: 'Bot', unicodeEmoji: null, primaryColor: 0, secondaryColor: null, iconUrl: 'https://cdn.example/r.png', position: 1, createdMs: 1_650_000_000_000, hoist: false, mentionable: true, managed: true, tags: { botId: '555' }, permissions: [] }, 'fail'],
  [{ id: '33', name: 'Boosters', unicodeEmoji: null, primaryColor: 0xabcdef, secondaryColor: null, iconUrl: null, position: 2, createdMs: 1_650_000_000_000, hoist: false, mentionable: false, managed: true, tags: { premiumSubscriberRole: true }, permissions: ['Administrator'] }, 'counts'],
  [{ id: '34', name: 'Linked', unicodeEmoji: null, primaryColor: 1, secondaryColor: null, iconUrl: null, position: 3, createdMs: 1_650_000_000_000, hoist: false, mentionable: false, managed: false, tags: { guildConnections: true }, permissions: [] }, 'counts'],
  [{ id: '35', name: 'Managed', unicodeEmoji: null, primaryColor: 0, secondaryColor: null, iconUrl: null, position: 4, createdMs: 1_650_000_000_000, hoist: false, mentionable: false, managed: true, tags: null, permissions: [] }, 'counts'],
  [{ id: '9100', name: '@everyone', unicodeEmoji: null, primaryColor: 0, secondaryColor: null, iconUrl: null, position: 0, createdMs: 1_650_000_000_000, hoist: false, mentionable: false, managed: false, tags: null, permissions: ['SendMessages'] }, 'counts'],
];
for (const [index, [view, mode]] of roleViews.entries()) {
  // A guild per case: the role counts are memoised per guild for 60 s.
  const g = fakeGuild({ ...serverViews[0][0], id: view.id === '9100' ? '9100' : `920${index}`, memberCount: 4321, roleCount: 1 }, {});
  const role = {
    id: view.id, name: view.name, position: view.position, createdTimestamp: view.createdMs, hoist: view.hoist, mentionable: view.mentionable,
    managed: view.managed, unicodeEmoji: view.unicodeEmoji, members: new Collection([['a', {}], ['b', {}]]), iconURL: () => view.iconUrl,
    colors: { primaryColor: view.primaryColor, secondaryColor: view.secondaryColor, tertiaryColor: null },
    permissions: new PermissionsBitField(view.permissions), tags: view.tags,
  };
  g.roles.cache.set(role.id, role);
  g.roles.fetchMemberCounts = mode === 'fail' ? async () => { throw new Error('boom'); } : async () => new Collection([['31', 12], ['33', 1234567]]);
  infoCases.role.push({ view, mode, roleCount: g.roles.cache.size, memberCount: 4321, cachedHolders: 2, embed: strip(await info.buildRoleEmbed(g, role, null)) });
}
const userCases = [
  { user: { id: '111', username: 'ali_ce', globalName: 'Alice', bot: false, createdMs: 1_500_000_000_000, avatarUrl: 'https://cdn.example/u.png', flags: ['Staff', 'ActiveDeveloper', 'Quarantined'] },
    member: { displayName: '*Ali*', displayColor: 0x00aa00, avatarUrl: 'https://cdn.example/m.png', joinedMs: 1_700_000_000_000, premiumSinceMs: 1_690_000_000_000, timeoutUntilMs: 4_000_000_000_000, roles: [['9005', 0], ['31', 7], ['30', 2]], permissions: ['BanMembers'], isGuildOwner: false },
    viewer: ['ModerateMembers'] },
  { user: { id: '112', username: 'bob', globalName: null, bot: true, createdMs: 1_500_000_000_123, avatarUrl: 'https://cdn.example/b.png', flags: ['VerifiedBot', 'BugHunterLevel2', 'HypeSquadOnlineHouse1'] },
    member: { displayName: 'Bob', displayColor: 0, avatarUrl: 'https://cdn.example/bm.png', joinedMs: null, premiumSinceMs: null, timeoutUntilMs: 4_000_000_000_000, roles: [['9005', 0]], permissions: ['Administrator'], isGuildOwner: true },
    viewer: [] },
  { user: { id: '113', username: 'ca*rol', globalName: 'C_', bot: false, createdMs: 1_500_000_000_000, avatarUrl: 'https://cdn.example/c.png', flags: null },
    member: null, viewer: ['Administrator'] },
];
for (const c of userCases) {
  const gid = '9005';
  const memberFake = (m, id) => m && ({
    id, guild: { id: gid, ownerId: m.isGuildOwner ? id : '999' }, displayName: m.displayName, displayColor: m.displayColor,
    joinedTimestamp: m.joinedMs, premiumSinceTimestamp: m.premiumSinceMs, communicationDisabledUntilTimestamp: m.timeoutUntilMs,
    roles: { cache: new Collection(m.roles.map(([rid, position]) => [rid, { id: rid, position }])) },
    permissions: new PermissionsBitField(m.permissions), displayAvatarURL: () => m.avatarUrl,
  });
  const member = memberFake(c.member, c.user.id);
  const invoker = memberFake({ displayName: 'inv', displayColor: 0, roles: [], permissions: c.viewer, isGuildOwner: false }, 'invoker');
  const user = {
    id: c.user.id, username: c.user.username, globalName: c.user.globalName, bot: c.user.bot, createdTimestamp: c.user.createdMs,
    flags: c.user.flags ? new UserFlagsBitField(c.user.flags) : null,
    displayAvatarURL: () => c.user.avatarUrl,
  };
  const interaction = {
    guild: { id: gid, ownerId: c.member?.isGuildOwner ? c.user.id : '999', members: { cache: new Collection(member ? [[member.id, member]] : []), fetch: async () => { throw new Error('Unknown Member'); } } },
    member: invoker, user: { id: 'invoker' },
    options: { getUser: () => user },
  };
  const level = c.member ? (c.member.isGuildOwner || c.member.permissions.includes('Administrator') ? 2 : c.member.permissions.includes('BanMembers') ? 1 : 0) : null;
  const flagBits = c.user.flags ? c.user.flags.filter((f) => f in UserFlags).reduce((n, f) => n | BigInt(UserFlags[f]), 0n).toString() : null;
  infoCases.user.push({ ...c, level, flagBits, viewerIsModerator: c.viewer.length > 0, embed: strip(await info.buildUserEmbed(interaction, null)) });
}

mkdirSync(OUT, { recursive: true });
const out = { help: helpCases, helpers, module: moduleCases, settings: settingsCases, info: infoCases };
writeFileSync(join(OUT, 'cases.json'), `${JSON.stringify(out, (_k, v) => (typeof v === 'bigint' ? v.toString() : v), 2)}\n`);
process.stdout.write(`cases.json written\n`);
for (const f of temps) { try { unlinkSync(f); } catch { /* ignore */ } }
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
