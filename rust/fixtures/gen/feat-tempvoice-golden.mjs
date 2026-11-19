// Golden data for feat-tempvoice, produced by the JS bot's own tempVoiceService / voiceRoleService with plain fakes
// (no servers needed). Writes rust/fixtures/golden/feat-tempvoice/golden.json.
//
//   node rust/fixtures/gen/feat-tempvoice-golden.mjs
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

const tv = await imp('src', 'services', 'tempVoiceService.js');
const vr = await imp('src', 'services', 'voiceRoleService.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const big = (_k, v) => (typeof v === 'bigint' ? v.toString() : v);
const plain = (x) => JSON.parse(JSON.stringify(x, big));
const embed = (e) => {
  const j = plain(e.toJSON ? e.toJSON() : e.data);
  delete j.timestamp;
  return j;
};

// ── Messages ──────────────────────────────────────────────────────────────────
const P = { target: '222', owner: '333', n: 7, at: 1_700_000_000_000, retryAt: 1_700_000_600_000, names: ['Manage Channels', 'Manage Roles'], channelId: '444', code: 50013 };
const msg = {};
for (const [k, fn] of Object.entries(tv.MSG)) msg[k] = { params: embed(fn(P)), none: embed(fn({})) };
msg.NO_CHANGE_LIMIT.zero = embed(tv.MSG.NO_CHANGE_LIMIT({ n: 0 }));
msg.DISCORD_ERROR.unknown = embed(tv.MSG.DISCORD_ERROR({ code: null }));

const ok = {};
for (const a of ['lock', 'unlock', 'hide', 'reveal', 'limit', 'rename', 'kick', 'ban', 'unban', 'transfer', 'claim', 'clean', 'nope']) {
  ok[a] = embed(tv.okEmbed(a, { name: 'My *Room*', target: '222', n: 4, newName: 'New_name' }));
}
ok.limit0 = embed(tv.okEmbed('limit', { name: 'x', n: 0 }));
ok.clean0 = embed(tv.okEmbed('clean', { n: 0 }));
ok.noname = embed(tv.okEmbed('lock', {}));

const describe = Object.fromEntries([50013, 50001, 10003, 30013, 50035, 40032, 12345, null].map((c) => [String(c), tv.describeDiscordError(c)]));

// ── Builders ──────────────────────────────────────────────────────────────────
const CH = '123456789012345678';
const panel = tv.buildControlPanel({ channelId: CH, ownerId: '111', settings: { embedColor: 1193046 } });
const panelOut = { embed: embed(panel.embeds[0]), components: panel.components.map((r) => plain(r.toJSON())), allowedMentions: panel.allowedMentions };
const pickers = {};
for (const [a, banned] of [['kick', []], ['ban', []], ['ban2', ['5', '6']], ['transfer', []]]) {
  const p = tv.buildMemberPicker(a === 'ban2' ? 'ban' : a, CH, banned);
  pickers[a] = { content: p.content, components: p.components.map((r) => plain(r.toJSON())), ephemeral: p.ephemeral, allowedMentions: p.allowedMentions };
}
const modals = {
  limit: plain(tv.buildValueModal('limit', CH, 5).toJSON()),
  limitNull: plain(tv.buildValueModal('limit', CH, null).toJSON()),
  rename: plain(tv.buildValueModal('rename', CH, 'Old name').toJSON()),
  renameEmpty: plain(tv.buildValueModal('rename', CH, '').toJSON()),
};

// ── Name sweeps ───────────────────────────────────────────────────────────────
const fakeGuild = { id: '9', name: 'Srv', iconURL: () => null, memberCount: 3 };
const fakeMember = { displayName: 'Bob', guild: fakeGuild, user: { id: '1', username: 'bob', tag: 'bob', bot: false, displayAvatarURL: () => '' } };
const ctx = { member: fakeMember, guild: fakeGuild };
const formats = [
  tv.DEFAULT_NAME_FORMAT, '{user.name} room', '{user}', '', '   ', 'x'.repeat(300), '{nope} x', '{username} {index}', '  a \t b\nc  ',
  '<#123456789012345678> hi', '{server} lounge', 'x'.repeat(101), '🎮'.repeat(60), '{ nope }', '{Index}!',
];
const render = formats.map((f) => ({ format: f, out: tv.renderChannelName(f, { ...ctx, index: 3 }) }));
const validate = formats.map((f) => ({ format: f, out: tv.validateNameFormat(f, ctx) }));
const names = ['  Hello   World  ', '', '   ', null, 'a'.repeat(150), '🎮'.repeat(120), 'x\r\ny\tz', '   nbsp   ', 'trailing '.repeat(12)];
const normalize = names.map((n) => ({ input: n, out: tv.normalizeChannelName(n) }));
const texts = ["#3 - Bob's Channel", 'Ünïcödé ROOM', '---', '', 'ＦＵＬＬ ｗｉｄｔｈ', 'a -- b', '🎮 Gaming 🎮', 'a'.repeat(150), 'Straße', '日本語 チャンネル', '_under_score_'];
const textNames = texts.map((t) => ({ input: t, out: tv.textChannelName(t, 7) }));

// ── Overwrites and summaries ──────────────────────────────────────────────────
const ow = (list) => list.map((o) => ({ id: o.id, type: o.type, allow: o.allow.toString(), deny: o.deny.toString() }));
const tmpl = [{ id: '9', type: 0, allow: 0n, deny: 1n << 21n }, { id: '400', type: 0, allow: (1n << 28n) | (1n << 10n), deny: 0n }];
const overwrites = {
  voice: ow(tv.computeVoiceOverwrites({ everyoneId: '9', botId: '200', ownerId: '300', template: tmpl, locked: true, hidden: true, bannedUserIds: ['500', '300'], exemptRoleIds: ['400', '9'], grantable: null })),
  voiceMasked: ow(tv.computeVoiceOverwrites({ everyoneId: '9', botId: '200', ownerId: '300', template: tmpl, locked: true, exemptRoleIds: ['400'], grantable: (1n << 10n) | (1n << 20n) | (1n << 4n) | (1n << 24n) | (1n << 28n) })),
  textPrivate: ow(tv.computeTextOverwrites({ everyoneId: '9', botId: '200', ownerId: '300', template: tmpl, membersOnly: true, memberIds: ['300', '600'], exemptRoleIds: ['400'] })),
  textPublic: ow(tv.computeTextOverwrites({ everyoneId: '9', botId: '200', ownerId: '300', template: tmpl, membersOnly: false, bannedUserIds: ['500', '300', '200'], grantable: 1n << 10n })),
};
const hubBase = {
  id: 'h1', guildId: '9', channelId: '700', categoryId: null, nameFormat: 'A `b` c', userLimit: 0, bitrate: null, keepAliveMinutes: 0,
  ownershipLockMinutes: 0, syncPermissions: true, createTextChannel: false, textChannelMembersOnly: true, ignoredRoleIds: [], moderatorRoleIds: [],
};
const hubs = [
  hubBase,
  { ...hubBase, categoryId: '800', userLimit: 5, bitrate: 64500, keepAliveMinutes: 10, ownershipLockMinutes: 3, syncPermissions: false, createTextChannel: true, textChannelMembersOnly: false, ignoredRoleIds: ['11', '12'], moderatorRoleIds: ['13'] },
  { ...hubBase, createTextChannel: true, bitrate: 96000 },
];
const noGuild = { channels: { cache: new Map() } };
const withParent = { channels: { cache: new Map([['700', { parent: { id: '800' } }]]) } };
const summaries = hubs.map((h) => ({ hub: h, top: tv.hubSummary(h, noGuild), parented: tv.hubSummary(h, withParent) }));

// ── Voice roles ───────────────────────────────────────────────────────────────
const linksRaw = [{ channelId: 'c1', roleId: 'r1' }, { channelId: 'c1', roleId: 'r1' }, null, { channelId: 5, roleId: 'r2' }, { channelId: 'cat', roleId: 'r2' }, { channelId: '', roleId: 'r3' }, { channelId: 'c2', roleId: 'r4' }];
const vs = { voiceRoleLinks: linksRaw, voiceRoleAnyRoleIds: ['a1', 'a1', 'r1'], voiceRoleIgnoreAfk: true };
const afkGuild = { afkChannelId: 'afk' };
const voiceRoles = {
  settings: vs,
  links: vr.linksOf(vs),
  any: vr.anyRolesOf(vs),
  managed: [...vr.managedRoleIds(vs)],
  desired: {
    c1: [...vr.desiredRoleIds(vs, afkGuild, { channelId: 'c1', parentId: null })],
    inCat: [...vr.desiredRoleIds(vs, afkGuild, { channelId: 'x', parentId: 'cat' })],
    none: [...vr.desiredRoleIds(vs, afkGuild, null)],
    afk: [...vr.desiredRoleIds(vs, afkGuild, { channelId: 'afk', parentId: null })],
    afkOff: [...vr.desiredRoleIds({ ...vs, voiceRoleIgnoreAfk: false }, afkGuild, { channelId: 'afk', parentId: null })],
  },
  dropChannel: vr.patchDropChannel(vs, 'c1'),
  dropChannelNone: vr.patchDropChannel(vs, 'zzz'),
  dropRole: vr.patchDropRole(vs, 'r1'),
  dropRoleNone: vr.patchDropRole(vs, 'zzz'),
};

const out = {
  msg, ok, describe, panel: panelOut, pickers, modals, render, validate, normalize, textNames, overwrites, summaries,
  botTextPerms: tv.BOT_TEXT_PERMS.toString(),
  keys: { cleanup: tv.cleanupKey(CH), rename: tv.renameKey(CH) },
  voiceRoles,
  reasons: { add: vr.VOICE_ROLES.ADD_REASON, remove: vr.VOICE_ROLES.REMOVE_REASON },
};
const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-tempvoice');
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, 'golden.json'), `${JSON.stringify(out, big, 2)}\n`);
redis.disconnect();
process.stdout.write('wrote rust/fixtures/golden/feat-tempvoice/golden.json\n');
process.exit(0);
