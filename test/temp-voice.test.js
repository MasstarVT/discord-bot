// pkg-temp-voice tests (pkg-temp-voice.md §9). Offline by default: the pure helpers, overwrite computation, panel / picker / modal
// builders, messages, the plugin declaration and the command payloads.
// With TEST_DATABASE_URL (a migrated database) the DB cases also run: every RAW_SQL statement, the hub CRUD, the whole create / leave /
// keep-alive / claim / ban / transfer lifecycle against fake discord.js objects, the cleanup job, reconciliation, channelDelete and the
// rule-20 cases (a REST 10003 purges a row, any other error keeps it):
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/temp-voice.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';
delete process.env.BOT_OWNER_ID;

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ChannelType, Collection, OverwriteType, PermissionFlagsBits: P, PermissionsBitField } = await import('discord.js');
const { Prisma } = await import('@prisma/client');
const tv = await import('../src/services/tempVoiceService.js');
const voiceCmd = await import('../src/commands/community/voice.js');
const hubCmd = await import('../src/commands/admin/voice-hub.js');
const pluginMod = await import('../src/plugins/temp-voice.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { buildCustomId, parseCustomId } = await import('../src/config/customIds.js');
const { ensureGuildSettings, updateSettings } = await import('../src/services/settingsService.js');
const { wasDeletedByBot } = await import('../src/utils/deletionTracker.js');

const {
  lowestFreeIndex, normalizeChannelName, textChannelName, parseLimitInput, effectiveBitrate, checkRenameWindow, claimStatus,
  targetProblem, canControl, computeVoiceOverwrites, computeTextOverwrites, reconcileDecision, renderChannelName, validateNameFormat,
  buildControlPanel, buildMemberPicker, buildValueModal, BUTTON_ACTIONS, MSG, OWNER_PERMS, BOT_VOICE_PERMS, BOT_TEXT_PERMS,
  DEFAULT_NAME_FORMAT, describeDiscordError, discordErrorCode, RAW_SQL, MAX_BANS_PER_CHANNEL, MAX_HUB_ROLE_LIST,
} = tv;

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const G = (n) => `95502100000000${String(n).padStart(4, '0')}`; // guild ids unique to this package (port 55021)
const ALL_GUILDS = Array.from({ length: 60 }, (_, i) => G(i + 1));

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) {
    await prisma.scheduledJob.deleteMany({ where: { guildId: { in: ALL_GUILDS } } }).catch(() => {});
    // The cascade removes TempVoiceHub and TempVoiceChannel rows.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: ALL_GUILDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

// ── 1. lowestFreeIndex ────────────────────────────────────────────────────────

test('1. lowestFreeIndex', () => {
  assert.equal(lowestFreeIndex([]), 1);
  assert.equal(lowestFreeIndex([1, 2, 4]), 3);
  assert.equal(lowestFreeIndex([2, 3]), 1);
  assert.equal(lowestFreeIndex([1, 1, 2]), 3);
  assert.equal(lowestFreeIndex([0, -1, 1.5]), 1);
});

// ── 2. normalizeChannelName ───────────────────────────────────────────────────

test('2. normalizeChannelName', () => {
  assert.equal(normalizeChannelName('  a\n b  '), 'a b');
  assert.equal(normalizeChannelName('   '), null);
  assert.equal(normalizeChannelName(null), null);
  assert.equal(normalizeChannelName('x'.repeat(150)).length, 100);
  const emoji = '😀';
  const cut = normalizeChannelName(`${'x'.repeat(99)}${emoji}yyy`);
  assert.equal(cut, `${'x'.repeat(99)}${emoji}`, 'the pair at position 100 is kept whole');
  assert.ok(!/[\uD800-\uDBFF]$/.test(cut), 'never ends with a lone high surrogate');
  assert.equal([...normalizeChannelName(emoji.repeat(150))].length, 100);
});

// ── 3. textChannelName ────────────────────────────────────────────────────────

test('3. textChannelName', () => {
  assert.equal(textChannelName("#1 - Bob's Channel", 1), '1-bobs-channel');
  assert.equal(textChannelName('🔥🔥', 3), 'voice-3');
  assert.equal(textChannelName('Ünïcode  Name!!', 2), 'ünïcode-name');
  assert.equal(textChannelName('  --a--  ', 4), 'a');
  assert.ok(textChannelName('a'.repeat(300), 1).length <= 100);
});

// ── 4. parseLimitInput ────────────────────────────────────────────────────────

test('4. parseLimitInput', () => {
  assert.equal(parseLimitInput('5'), 5);
  assert.equal(parseLimitInput(' 0 '), 0);
  assert.equal(parseLimitInput('99'), 99);
  for (const bad of ['-1', '1.5', '099', '', 'abc', null, undefined]) assert.equal(parseLimitInput(bad), null, String(bad));
});

// ── 5. effectiveBitrate ───────────────────────────────────────────────────────

test('5. effectiveBitrate', () => {
  assert.equal(effectiveBitrate(null, 96000), undefined);
  assert.equal(effectiveBitrate(undefined, 96000), undefined);
  assert.equal(effectiveBitrate(128000, 96000), 96000);
  assert.equal(effectiveBitrate(64000, 96000), 64000);
  assert.equal(effectiveBitrate(1000, 96000), 8000);
});

// ── 6. checkRenameWindow ──────────────────────────────────────────────────────

test('6. checkRenameWindow', () => {
  const now = 1e9;
  assert.deepEqual(checkRenameWindow([], now), { ok: true, recent: [] });
  assert.equal(checkRenameWindow([now - 100_000], now).ok, true);
  const full = checkRenameWindow([now - 500_000, now - 100_000], now);
  assert.equal(full.ok, false);
  assert.equal(full.retryAt, now + 100_000);
  assert.equal(checkRenameWindow([now - 700_000, now - 100_000], now).ok, true, 'the old stamp is outside the window');
  assert.equal(checkRenameWindow([now + 5_000, now + 9_000], now).ok, true, 'future stamps are ignored');
  assert.equal(checkRenameWindow([NaN, 'x', now - 10], now).recent.length, 1);
});

// ── 7. claimStatus ────────────────────────────────────────────────────────────

test('7. claimStatus', () => {
  const now = 1e9;
  const base = { isOwner: false, actorConnected: true, ownerConnected: false, ownerLeftAt: null, lockMinutes: 0, isMod: false, now };
  assert.deepEqual(claimStatus({ ...base, isOwner: true }), { ok: false, code: 'ALREADY_OWNER' });
  assert.deepEqual(claimStatus({ ...base, actorConnected: false }), { ok: false, code: 'NOT_CONNECTED' });
  assert.deepEqual(claimStatus({ ...base, ownerConnected: true }), { ok: false, code: 'OWNER_PRESENT' });
  const left = new Date(now - 60_000);
  assert.deepEqual(claimStatus({ ...base, ownerLeftAt: left, lockMinutes: 2 }), { ok: false, code: 'LOCKED', claimableAt: now + 60_000 });
  assert.deepEqual(claimStatus({ ...base, ownerLeftAt: left, lockMinutes: 2, isMod: true }), { ok: true });
  assert.deepEqual(claimStatus({ ...base, ownerLeftAt: null, lockMinutes: 0 }), { ok: true });
  assert.deepEqual(claimStatus({ ...base, ownerLeftAt: null, lockMinutes: 1 }), { ok: false, code: 'LOCKED', claimableAt: now + 60_000 });
});

// ── 8. targetProblem ──────────────────────────────────────────────────────────

test('8. targetProblem', () => {
  const t = (o) => targetProblem({ actorId: 'a', targetId: 't', ownerId: 'o', ...o });
  // kick
  assert.equal(t({ action: 'kick', targetId: 'a', ownerId: 'a' }), 'TARGET_SELF');
  assert.equal(t({ action: 'kick', targetId: 'o' }), 'TARGET_OWNER');
  assert.equal(t({ action: 'kick', targetProtected: true, targetConnected: true }), 'TARGET_PROTECTED');
  assert.equal(t({ action: 'kick', targetConnected: false }), 'TARGET_NOT_CONNECTED');
  assert.equal(t({ action: 'kick', targetConnected: true }), null);
  assert.equal(t({ action: 'kick', targetConnected: true, targetIsBot: true }), null, 'bots may be kicked');
  // ban
  assert.equal(t({ action: 'ban', targetId: 'a' }), 'TARGET_SELF');
  assert.equal(t({ action: 'ban', targetId: 'o' }), 'TARGET_OWNER');
  assert.equal(t({ action: 'ban', targetProtected: true, isBanned: true }), 'TARGET_PROTECTED');
  assert.equal(t({ action: 'ban', isBanned: true }), 'ALREADY_BANNED');
  assert.equal(t({ action: 'ban', banCount: MAX_BANS_PER_CHANNEL }), 'BAN_LIMIT');
  assert.equal(t({ action: 'ban', banCount: MAX_BANS_PER_CHANNEL - 1 }), null);
  assert.equal(t({ action: 'ban', targetIsBot: true }), null, 'bots may be banned');
  // unban
  assert.equal(t({ action: 'unban', isBanned: false }), 'NOT_BANNED');
  assert.equal(t({ action: 'unban', isBanned: true }), null);
  // transfer
  assert.equal(t({ action: 'transfer', targetId: 'o' }), 'TARGET_ALREADY_OWNER');
  assert.equal(t({ action: 'transfer', targetIsBot: true, targetConnected: true }), 'TARGET_BOT');
  assert.equal(t({ action: 'transfer', targetConnected: false }), 'TARGET_NOT_CONNECTED');
  assert.equal(t({ action: 'transfer', targetConnected: true }), null);
  assert.equal(t({ action: 'lock' }), null);
});

// ── 9. canControl ─────────────────────────────────────────────────────────────

test('9. canControl', () => {
  assert.equal(canControl({ action: 'claim', isOwner: false, isMod: false }), true);
  assert.equal(canControl({ action: 'lock', isOwner: false, isMod: false }), false);
  assert.equal(canControl({ action: 'lock', isOwner: true, isMod: false }), true);
  assert.equal(canControl({ action: 'lock', isOwner: false, isMod: true }), true);
  assert.equal(canControl({ action: 'nonsense', isOwner: true, isMod: true }), false);
});

// ── 10. computeVoiceOverwrites ────────────────────────────────────────────────

const GID = '100';
const BOT = '200';
const OWN = '300';
const ROLE = '400';
const BANNED = '500';
const TEMPLATE = [
  { id: GID, type: 0, allow: 0n, deny: P.Speak },
  { id: ROLE, type: 0, allow: P.ManageRoles | P.ViewChannel, deny: 0n },
];

test('10. computeVoiceOverwrites: locked + hidden + banned, nothing masked', () => {
  const out = tv.computeVoiceOverwrites({
    everyoneId: GID, botId: BOT, ownerId: OWN, template: TEMPLATE, locked: true, hidden: true,
    bannedUserIds: [BANNED], exemptRoleIds: [ROLE], grantable: null,
  });
  assert.deepEqual(out.map((o) => o.id), [GID, BOT, OWN, ROLE, BANNED]);
  const by = Object.fromEntries(out.map((o) => [o.id, o]));
  assert.equal(by[GID].allow, 0n);
  assert.equal(by[GID].deny, P.Speak | P.ViewChannel | P.Connect);
  assert.equal(by[GID].deny, 3146752n);
  assert.equal(by[BOT].allow, 17910800n);
  assert.equal(by[BOT].allow, BOT_VOICE_PERMS);
  assert.equal(by[OWN].allow, 17827088n);
  assert.equal(by[OWN].allow, OWNER_PERMS);
  assert.equal(by[ROLE].allow, P.ManageRoles | P.ViewChannel | P.Connect);
  assert.equal(by[ROLE].allow, 269485056n);
  assert.equal(by[BANNED].deny, P.Connect);
  assert.equal(by[BANNED].deny, 1048576n);
  assert.equal(by[BANNED].type, OverwriteType.Member);
  assert.equal(by[GID].type, OverwriteType.Role);
});

test('10. computeVoiceOverwrites: defaults with a masking grantable set', () => {
  const grantable = P.ViewChannel | P.Connect | P.ManageChannels | P.MoveMembers | P.SendMessages | P.EmbedLinks
    | P.ReadMessageHistory | P.Speak | P.ManageRoles;
  const out = tv.computeVoiceOverwrites({
    everyoneId: GID, botId: BOT, ownerId: OWN, template: TEMPLATE, exemptRoleIds: [ROLE], grantable,
  });
  assert.deepEqual(out.map((o) => o.id), [GID, BOT, OWN, ROLE]);
  const by = Object.fromEntries(out.map((o) => [o.id, o]));
  assert.equal(by[GID].deny, P.Speak);
  assert.equal(by[BOT].allow, 17910800n);
  assert.equal(by[OWN].allow, 17826832n, 'no Priority Speaker: the bot cannot grant it');
  assert.equal(by[ROLE].allow, P.ViewChannel, 'Manage Permissions is stripped by the mask');
});

test('10. computeVoiceOverwrites: owner is never banned, output is deterministic and has no empty entries', () => {
  const input = {
    everyoneId: GID, botId: BOT, ownerId: OWN, template: TEMPLATE, locked: true, bannedUserIds: [OWN, BOT, BANNED], exemptRoleIds: [ROLE],
  };
  const a = tv.computeVoiceOverwrites(input);
  const b = tv.computeVoiceOverwrites(input);
  assert.deepEqual(a, b);
  const own = a.find((o) => o.id === OWN);
  assert.equal(own.deny & P.Connect, 0n, 'the owner listed in bannedUserIds gets no Connect deny');
  assert.equal(a.find((o) => o.id === BOT).deny & P.Connect, 0n);
  for (const o of a) assert.ok(!(o.allow === 0n && o.deny === 0n));
  // the template itself is not mutated
  assert.equal(TEMPLATE[0].deny, P.Speak);
  // the everyone id may appear in the exempt list without effect
  const out = tv.computeVoiceOverwrites({ ...input, exemptRoleIds: [GID] });
  assert.equal(out.find((o) => o.id === GID).deny & P.Connect, P.Connect);
});

// ── 11. computeTextOverwrites ─────────────────────────────────────────────────

test('11. computeTextOverwrites', () => {
  const members = computeTextOverwrites({
    everyoneId: GID, botId: BOT, ownerId: OWN, membersOnly: true, memberIds: ['300', '600'], exemptRoleIds: ['400'],
  });
  const by = Object.fromEntries(members.map((o) => [o.id, o]));
  assert.equal(by[GID].deny, P.ViewChannel);
  for (const id of ['300', '600', '400']) assert.equal(by[id].allow, P.ViewChannel, id);
  assert.equal(by['600'].type, OverwriteType.Member);
  assert.equal(by['400'].type, OverwriteType.Role);
  assert.equal(by[BOT].allow, BOT_TEXT_PERMS);

  const open = computeTextOverwrites({
    everyoneId: GID, botId: BOT, ownerId: OWN, membersOnly: false, memberIds: ['300'], bannedUserIds: ['500', OWN],
  });
  const o2 = Object.fromEntries(open.map((o) => [o.id, o]));
  assert.equal(o2[GID], undefined, 'no @everyone deny when the channel is public');
  assert.equal(o2['500'].deny, P.ViewChannel);
  assert.equal(o2[OWN], undefined, 'the owner is never banned');
  assert.equal(o2[BOT].allow, BOT_TEXT_PERMS);
});

// ── 12. reconcileDecision ─────────────────────────────────────────────────────

test('12. reconcileDecision', () => {
  const now = 1e9;
  const min = 60_000;
  const d = (o) => reconcileDecision({ channelExists: true, humans: 0, keepAliveMinutes: 0, emptySince: null, now, ...o });
  assert.deepEqual(d({ channelExists: false }), { action: 'purge' });
  assert.deepEqual(d({ humans: 2 }), { action: 'occupied' });
  assert.deepEqual(d({}), { action: 'delete' });
  assert.deepEqual(d({ keepAliveMinutes: 5, emptySince: new Date(now - 10 * min) }), { action: 'delete' });
  assert.deepEqual(d({ keepAliveMinutes: 5, emptySince: new Date(now - min) }), { action: 'schedule', runAt: now + 4 * min, setEmptySince: false });
  assert.deepEqual(d({ keepAliveMinutes: 5, emptySince: null }), { action: 'schedule', runAt: now + 5 * min, setEmptySince: true });
});

// ── 13. renderChannelName / validateNameFormat ────────────────────────────────

const fakeGuild = { id: '9', name: 'Srv', iconURL: () => null, memberCount: 3 };
const fakeMember = {
  displayName: 'Bob', guild: fakeGuild,
  user: { id: '1', username: 'bob', tag: 'bob', bot: false, displayAvatarURL: () => '' },
};

test('13. renderChannelName and validateNameFormat', () => {
  const ctx = { member: fakeMember, guild: fakeGuild };
  assert.equal(renderChannelName(DEFAULT_NAME_FORMAT, { ...ctx, index: 3 }), "#3 - Bob's Channel");
  assert.equal(renderChannelName('{user.name} room', { ...ctx, index: 1 }), 'bob room');
  assert.equal(renderChannelName('{user}', { ...ctx, index: 3 }), "#3 - Bob's Channel", 'mentions fall back to the default');
  assert.equal(renderChannelName('', { ...ctx, index: 2 }), "#2 - Bob's Channel");
  assert.equal(renderChannelName('x'.repeat(300), { ...ctx, index: 1 }).length, 100);

  const v = validateNameFormat(DEFAULT_NAME_FORMAT, ctx);
  assert.equal(v.ok, true);
  assert.equal(v.preview, "#1 - Bob's Channel");
  assert.deepEqual(v.warnings, []);

  const mention = validateNameFormat('{user}', ctx);
  assert.equal(mention.ok, false);
  assert.match(mention.error, /mentions/);

  const unknown = validateNameFormat('{nope} x', ctx);
  assert.equal(unknown.ok, true);
  assert.equal(unknown.warnings.length, 1);
  assert.match(unknown.warnings[0], /\{nope\}/);

  assert.equal(validateNameFormat('   ', ctx).ok, false);
  assert.equal(validateNameFormat('x'.repeat(101), ctx).ok, false);
});

// ── 14. Panel, pickers, modal, plugin ─────────────────────────────────────────

const CH = '123456789012345678';

test('14. buildControlPanel', () => {
  const panel = buildControlPanel({ channelId: CH, ownerId: '1', settings: null });
  assert.equal(panel.components.length, 2);
  const buttons = panel.components.flatMap((row) => row.toJSON().components);
  assert.equal(buttons.length, 10);
  assert.equal(panel.components[0].toJSON().components.length, 5);
  for (const b of buttons) {
    const { prefix, params } = parseCustomId(b.custom_id);
    assert.equal(prefix, 'voice');
    assert.ok(BUTTON_ACTIONS.includes(params[0]), b.custom_id);
    assert.equal(params[1], CH);
    assert.ok(b.custom_id.length <= 100);
  }
  assert.deepEqual(new Set(buttons.map((b) => parseCustomId(b.custom_id).params[0])), new Set(BUTTON_ACTIONS));
  assert.deepEqual(panel.allowedMentions, { parse: [] });
  assert.match(panel.embeds[0].data.description, /<@1>/);
});

test('14. buildMemberPicker', () => {
  const ban = buildMemberPicker('ban', CH, ['5']);
  assert.equal(ban.components.length, 2);
  const [first, second] = ban.components.map((r) => r.toJSON().components[0]);
  assert.equal(first.custom_id, `voice-select:ban:${CH}`);
  assert.equal(second.custom_id, `voice-select:unban:${CH}`);
  assert.equal(first.max_values, 1);
  assert.equal(second.max_values, 1);
  assert.match(ban.content, /<@5>/);
  assert.equal(ban.ephemeral, true);
  assert.deepEqual(ban.allowedMentions, { parse: [] });

  const kick = buildMemberPicker('kick', CH);
  assert.equal(kick.components.length, 1);
  assert.equal(kick.components[0].toJSON().components[0].custom_id, `voice-select:kick:${CH}`);
  const transfer = buildMemberPicker('transfer', CH);
  assert.equal(transfer.components[0].toJSON().components[0].custom_id, `voice-select:transfer:${CH}`);
  assert.doesNotMatch(buildMemberPicker('ban', CH, []).content, /Banned:/);
});

test('14. buildValueModal', () => {
  const limit = buildValueModal('limit', CH, 7).toJSON();
  assert.equal(limit.custom_id, `voice-modal:limit:${CH}`);
  const input = limit.components[0].components[0];
  assert.equal(input.custom_id, 'voice-value');
  assert.equal(input.value, '7');
  assert.equal(input.max_length, 2);
  const rename = buildValueModal('rename', CH, 'My room').toJSON();
  assert.equal(rename.components[0].components[0].value, 'My room');
  assert.equal(rename.components[0].components[0].max_length, 100);
  assert.equal(buildValueModal('limit', CH, 0).toJSON().components[0].components[0].value, '0');
});

test('14. plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  const mine = errors.filter((e) => e.includes('temp-voice'));
  assert.deepEqual(mine, [], mine.join('\n'));
  const plugin = plugins.find((p) => p.id === 'temp-voice');
  assert.ok(plugin, 'collectPlugins returns temp-voice');
  assert.equal(plugin.module, 'tempvoice');
  assert.deepEqual(plugin.hooks.map((h) => h.event).sort(), ['channelDelete', 'guildReady', 'voiceStateUpdate']);
  for (const h of plugin.hooks) {
    assert.equal(h.id, 'temp-voice');
    assert.equal(h.always, true);
    assert.ok(HOOK_ORDER[h.event].includes('temp-voice'));
  }
  const comps = plugin.components.map((c) => `${c.type}:${c.prefix}`).sort();
  assert.deepEqual(comps, ['button:voice', 'modal:voice-modal', 'select:voice-select']);
  assert.deepEqual(plugin.jobs.map((j) => j.type), ['tempvoice.cleanup']);
  assert.deepEqual(plugin.provides, {});
  assert.equal(pluginMod.plugin.id, 'temp-voice');
});

test('14. voiceFilter and channelDeleteFilter', () => {
  assert.equal(tv.voiceFilter({ change: 'state', guild: { id: 'no-memo' } }), false);
  assert.equal(tv.voiceFilter({ change: 'join', guild: { id: 'no-memo' }, oldState: {}, newState: { channelId: '1' } }), true, 'no memo: let the task decide');
  assert.equal(tv.channelDeleteFilter({ guild: { id: 'no-memo' }, channel: { id: '1' } }), true);
});

// ── 15. Command payloads ──────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('15. /voice payload', () => {
  const voice = buildCommandPayload(voiceCmd);
  assert.equal(voice.name, 'voice');
  assert.deepEqual(voice.options.map((o) => o.name).sort(), ['ban', 'claim', 'clean', 'hide', 'kick', 'limit', 'lock', 'rename', 'reveal', 'transfer', 'unban', 'unlock']);
  assert.ok(voice.options.every((o) => o.type === 1 && (o.options?.length ?? 0) <= 1));
  assert.ok(charCount(voice) <= 1300, `/voice is ${charCount(voice)} characters`);
  const walk = (n) => [n, ...(n.options ?? []).flatMap(walk)];
  for (const n of walk(voice)) assert.ok((n.description?.length ?? 0) <= 100, `${n.name} description`);
  const limit = voice.options.find((o) => o.name === 'limit').options[0];
  assert.equal(limit.name, 'member_limit');
  assert.equal(limit.min_value, 0);
  assert.equal(limit.max_value, 99);
  assert.equal(voice.options.find((o) => o.name === 'rename').options[0].max_length, 100);
  assert.equal(voice.default_member_permissions ?? null, null);
  assert.deepEqual(voice.contexts, [0]);
  assert.equal(voiceCmd.module, 'tempvoice');
  assert.equal(voiceCmd.gated, true);
  assert.equal(voiceCmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.equal(voiceCmd.allowInDMs, undefined);
});

test('15. /voice-hub payload', () => {
  const hub = buildCommandPayload(hubCmd);
  assert.equal(hub.name, 'voice-hub');
  assert.deepEqual(hub.options.map((o) => o.name), ['create', 'edit', 'delete', 'list', 'ignore-role', 'mod-role']);
  const sub = (name) => hub.options.find((o) => o.name === name);
  assert.equal(sub('create').options.length, 10);
  assert.ok(sub('create').options.every((o) => !o.required));
  assert.equal(sub('edit').options.length, 10);
  assert.deepEqual(sub('edit').options.filter((o) => o.required).map((o) => o.name), ['hub']);
  assert.equal(sub('edit').options[0].name, 'hub');
  assert.deepEqual(sub('delete').options.map((o) => o.name), ['hub', 'delete_channel']);
  for (const g of ['ignore-role', 'mod-role']) {
    assert.deepEqual(sub(g).options.map((o) => o.name), ['add', 'remove']);
    for (const s of sub(g).options) assert.deepEqual(s.options.map((o) => o.name), ['hub', 'role']);
  }
  assert.ok(charCount(hub) <= 1800, `/voice-hub is ${charCount(hub)} characters`);
  assert.equal(hub.default_member_permissions, String(P.ManageGuild));
  assert.deepEqual(hub.contexts, [0]);
  assert.equal(hubCmd.module, 'tempvoice');
  assert.equal(hubCmd.gated, false);
  assert.equal(hubCmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  const opt = (name) => sub('create').options.find((o) => o.name === name);
  assert.equal(opt('keep_alive').max_value, 10080);
  assert.equal(opt('ownership_lock').max_value, 1440);
  assert.equal(opt('bitrate').min_value, 8);
  assert.equal(opt('bitrate').max_value, 384);
  assert.equal(opt('limit').min_value, 0);
  assert.equal(opt('limit').max_value, 99);
});

// ── 16. Messages, errors and small pure pieces ────────────────────────────────

test('16. MSG builds embeds with the documented titles and texts', () => {
  assert.equal(MSG.NOT_IN_TEMP().data.title, '❌  Not In A Temporary Channel');
  assert.equal(MSG.NOT_IN_TEMP().data.description, 'Join your temporary voice channel first, then run this command.');
  assert.match(MSG.TARGET_OWNER({ target: '7' }).data.description, /^<@7> owns this channel\./);
  assert.equal(MSG.NO_CHANGE_LOCK().data.title, '⚠️  No Change');
  assert.equal(MSG.NO_CHANGE_LIMIT({ n: 0 }).data.description, 'The member limit is already off.');
  assert.equal(MSG.NO_CHANGE_LIMIT({ n: 4 }).data.description, 'The member limit is already 4.');
  assert.match(MSG.CLAIM_LOCKED({ at: 1_700_000_000_000 }).data.description, /<t:1700000000:R>/);
  assert.match(MSG.RENAME_RATE({ retryAt: 1_700_000_000_000 }).data.description, /<t:1700000000:R>/);
  assert.match(MSG.BOT_MISSING({ names: ['Manage Channels', 'Move Members'], channelId: '5' }).data.description, /\*\*Manage Channels, Move Members\*\* in <#5>/);
  assert.match(MSG.DISCORD_ERROR({ code: 50013 }).data.description, /missing permissions/);
  assert.equal(tv.okEmbed('clean', { n: 0 }).data.title, 'ℹ️  Nothing To Clean');
  assert.match(tv.okEmbed('clean', { n: 3 }).data.description, /\*\*3\*\*/);
  assert.match(tv.okEmbed('limit', { name: 'a', n: 0 }).data.description, /no member limit/);
  assert.match(tv.okEmbed('rename', { newName: 'x_y' }).data.description, /x\\_y/, 'markdown in names is escaped');
});

test('16. describeDiscordError and discordErrorCode', () => {
  assert.equal(describeDiscordError(50013), 'missing permissions');
  assert.equal(describeDiscordError(50001), 'no access');
  assert.equal(describeDiscordError(10003), 'channel not found');
  assert.equal(describeDiscordError(30013), 'the server has 500 channels');
  assert.equal(describeDiscordError(50035), 'invalid request (is the category full?)');
  assert.equal(describeDiscordError(40032), 'member not connected');
  assert.equal(describeDiscordError(null), 'error unknown');
  assert.equal(describeDiscordError(7), 'error 7');
  assert.equal(discordErrorCode({ code: 10003 }), 10003);
  assert.equal(discordErrorCode({ code: 'ECONNRESET', rawError: { code: 50001 } }), 50001);
  assert.equal(discordErrorCode(new Error('x')), null);
  assert.equal(discordErrorCode(null), null);
});

test('16. exemptRoleIds, isTempVoiceMod, isProtectedTarget, hubSummary, hubProblems', () => {
  const guild = { id: 'g', roles: { cache: new Map([['r1', {}], ['r2', {}], ['r3', {}]]) } };
  const hub = { ignoredRoleIds: ['r1', 'gone'], moderatorRoleIds: ['r2', 'g'] };
  assert.deepEqual(tv.exemptRoleIds(hub, { modRoleIds: ['r3', 'r1'] }, guild).sort(), ['r1', 'r2', 'r3']);
  assert.deepEqual(tv.exemptRoleIds(hub, null, guild).sort(), ['r1', 'r2']);

  const member = (roles, perms = 0n) => ({
    id: 'm', guild, permissions: new PermissionsBitField(perms), roles: { cache: new Collection(roles.map((r) => [r, { id: r }])) },
  });
  const h = { moderatorRoleIds: ['r2'], ignoredRoleIds: ['r1'] };
  assert.equal(tv.isTempVoiceMod(member([]), h, null), false);
  assert.equal(tv.isTempVoiceMod(member(['r2']), h, null), true);
  assert.equal(tv.isTempVoiceMod(member(['r1']), h, null), false, 'an ignored role is protected but not a moderator');
  assert.equal(tv.isTempVoiceMod(member([], P.BanMembers), h, null), true, 'MODERATOR level counts');
  assert.equal(tv.isTempVoiceMod(member(['m1']), h, { modRoleIds: ['m1'] }), true);
  assert.equal(tv.isProtectedTarget(member(['r1']), h, null), true);
  assert.equal(tv.isProtectedTarget(member(['r2']), h, null), true);
  assert.equal(tv.isProtectedTarget(member([]), h, null), false);

  const fakeHubGuild = {
    id: 'g',
    members: { me: { permissions: new PermissionsBitField(P.Administrator) } },
  };
  const everything = () => new PermissionsBitField(P.Administrator);
  const category = { id: '5', name: 'Cat', guild: fakeHubGuild, permissionsFor: everything, children: { cache: new Collection() } };
  fakeHubGuild.channels = {
    cache: new Collection([['10', { id: '10', name: 'hub', parent: category, guild: fakeHubGuild, permissionsFor: everything }]]),
  };
  const summary = tv.hubSummary({
    channelId: '10', categoryId: null, nameFormat: "#{index} - `x`", userLimit: 0, bitrate: 96000, keepAliveMinutes: 0, ownershipLockMinutes: 3,
    syncPermissions: true, createTextChannel: true, textChannelMembersOnly: true, ignoredRoleIds: [], moderatorRoleIds: ['r2'],
  }, fakeHubGuild);
  assert.match(summary, /Hub: <#10> · New channels in: the hub's category/);
  assert.match(summary, /Name: `#\{index\} - ˋxˋ`/);
  assert.match(summary, /Limit: none · Bitrate: 96 kbps · Keep alive: delete at once/);
  assert.match(summary, /Ownership lock: 3 min · Permissions: copied from category/);
  assert.match(summary, /Text channel: private/);
  assert.match(summary, /Mod roles: <@&r2> · Ignored roles: none/);

  const problems = tv.hubProblems({ channelId: '99', categoryId: null }, fakeHubGuild);
  assert.equal(problems.length, 1);
  assert.match(problems[0], /Hub channel deleted/);
  assert.deepEqual(tv.hubProblems({ channelId: '10', categoryId: null }, fakeHubGuild), []);
});

test('16. RAW_SQL builders are Prisma.Sql without a bare now()', () => {
  assert.equal(RAW_SQL.length, 2);
  for (const { name, build } of RAW_SQL) {
    const sql = build();
    assert.ok(sql instanceof Prisma.sql``.constructor, name);
    assert.doesNotMatch(sql.sql, /\bnow\(\)/i, name);
  }
  assert.ok(RAW_SQL.every((r) => r.build().sql.includes('FOR UPDATE')));
});

test('16. every Discord REST call goes through discordCall (static check)', async () => {
  const { readFileSync } = await import('node:fs');
  const src = readFileSync(new URL('../src/services/tempVoiceService.js', import.meta.url), 'utf8').split('\n');
  const offenders = [];
  src.forEach((line, i) => {
    if (/^\s*\/\//.test(line)) return;
    if (!/\.(delete|send|setName|setChannel|setUserLimit|disconnect|pin|edit)\(|permissionOverwrites\.set\(|channels\.create\(|messages\.fetch\(/.test(line)) return;
    if (/discordCall|deleteDiscordChannel|withTimeout|\? cached\.delete|prisma\./.test(line)) return;
    if (/(?:indexMemo|indexLoads|renameStamps|textSyncTimers|pendingRetries|lastCreateAt)\.delete\(/.test(line)) return; // in-memory Maps
    // the continuation of a multi-line discordCall("...", () => x.send(...)) starts on the line above
    if (/discordCall/.test(src[i - 1] ?? '') || /discordCall/.test(src[i - 2] ?? '')) return;
    offenders.push(`${i + 1}: ${line.trim()}`);
  });
  assert.deepEqual(offenders, []);
});

test('16. importing the service has no side effects and exports the contract', () => {
  for (const name of [
    'MSG', 'DEFAULT_NAME_FORMAT', 'HUB_CHANNEL_NAME', 'MAX_HUB_ROLE_LIST', 'REQUIRED_BOT_PERMS', 'HubLimitError', 'discordCall',
    'describeDiscordError', 'validateNameFormat', 'countHumans', 'hubSummary', 'hubProblems', 'runVoiceAction', 'cleanEmptyChannels',
    'countHubs', 'findHubByChannel', 'isTempChannel', 'listHubs', 'listHubChannels', 'createHub', 'updateHub', 'updateHubRoleList',
    'deleteHub', 'onGuildReady', 'onVoiceStateUpdate', 'voiceFilter', 'onChannelDelete', 'channelDeleteFilter', 'handleButton',
    'handleSelect', 'handleModal', 'handleCleanupJob', 'RAW_SQL',
  ]) assert.ok(name in tv, `export ${name}`);
  assert.equal(tv.MAX_HUB_ROLE_LIST, 10);
  assert.equal(tv.HUB_CHANNEL_NAME, '➕ Join to Create');
  assert.deepEqual([...tv.REQUIRED_BOT_PERMS], ['ManageChannels', 'ManageRoles', 'MoveMembers', 'ViewChannel', 'Connect']);
  assert.equal(tv.cleanupKey('1'), 'tempvoice.cleanup:1');
  assert.equal(tv.renameKey('1'), 'tempvoice:rename:1');
});

test('16. component handlers answer without I/O for stale ids and wrong context', async () => {
  const replies = [];
  const make = (customId, extra = {}) => ({
    customId, guildId: G(1), replied: false, deferred: false, inCachedGuild: () => true,
    reply: async (p) => { replies.push(p); return {}; }, ...extra,
  });
  await tv.handleButton(make('voice:explode:123456789012345678'));
  await tv.handleButton(make('voice:lock:notasnowflake'));
  await tv.handleButton(make('voice:lock:1', { inCachedGuild: () => false }));
  await tv.handleSelect(make('voice-select:lock:123456789012345678'));
  await tv.handleModal(make('voice-modal:kick:123456789012345678'));
  await tv.handleModal(make('voice-modal:limit:123456789012345678', { fields: { getTextInputValue: () => 'abc' } }));
  assert.equal(replies[0].content, 'This button is no longer active.');
  assert.equal(replies[1].content, 'This button is no longer active.');
  assert.equal(replies[2].embeds[0].data.title, '❌  Server Only');
  assert.equal(replies[3].content, 'This menu is no longer active.');
  assert.equal(replies[4].content, 'This form has expired. Please run the command again.');
  assert.equal(replies[5].embeds[0].data.title, '❌  Invalid Limit');
  assert.ok(replies.every((r) => r.ephemeral === true));
});

test('16. limits referenced by the package exist', () => {
  assert.equal(LIMITS.TEMPVOICE_HUBS, 10);
  assert.equal(MAX_HUB_ROLE_LIST, 10);
  assert.equal(MAX_BANS_PER_CHANNEL, 50);
  assert.equal(buildCustomId('voice-value'), 'voice-value');
});

// ═════════════════════════════════════════════════════════════════════════════
// DB cases (TEST_DATABASE_URL): fake discord.js objects around the real service and a migrated Postgres.
// ═════════════════════════════════════════════════════════════════════════════

let idSeq = 0;
const nextId = () => `95502199${String(++idSeq).padStart(10, '0')}`; // 18 digits, so customId snowflake checks pass
const BOT_ID = '955021990000000001';
const apiError = (code, message = `Discord error ${code}`) => Object.assign(new Error(message), { code });
const SETTINGS_ON = { tempVoiceEnabled: true, modRoleIds: [] };
const sleepMs = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(fn, timeoutMs = 4000) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) throw new Error('until(): condition not met in time');
    await sleepMs(15);
  }
}

/** A fake guild with just enough of the discord.js surface for the service. `world.fail` injects Discord errors. */
function makeWorld(guildId, { available = true, botPerms = P.Administrator, maximumBitrate = 96000 } = {}) {
  const calls = { created: [], deleted: [], overwrites: [], sent: [], moved: [], disconnected: [], names: [], limits: [], pinned: [], edited: [] };
  const world = { calls, fail: {}, members: new Map(), messages: new Map(), remote: new Map(), fetchLog: [] };
  const botBits = new PermissionsBitField(botPerms);
  const guild = {
    id: guildId, available, ownerId: 'owner0', maximumBitrate,
    roles: { cache: new Collection() }, voiceStates: { cache: new Collection() },
  };
  const me = { id: BOT_ID, permissions: botBits, permissionsIn: () => botBits };
  const channels = new Collection();
  world.channels = channels;
  world.guild = guild;

  const place = (m, ch) => {
    const old = m.voice.channelId ? channels.get(m.voice.channelId) : null;
    old?.members.delete(m.id);
    m.voice.channelId = ch?.id ?? null;
    if (ch) {
      ch.members.set(m.id, m);
      guild.voiceStates.cache.set(m.id, { channelId: ch.id });
    } else guild.voiceStates.cache.delete(m.id);
  };
  world.place = place;

  const deleteChannel = async (id, reason) => {
    const byId = world.fail.deleteById?.[id];
    const code = byId ?? world.fail.delete;
    if (code) throw apiError(code);
    const ch = channels.get(id) ?? world.remote.get(id);
    if (!ch) throw apiError(10003);
    channels.delete(id);
    world.remote.delete(id);
    ch.members?.forEach((m) => { m.voice.channelId = null; guild.voiceStates.cache.delete(m.id); });
    calls.deleted.push({ id, reason });
    return ch;
  };

  const makeMessage = (payload) => {
    const msg = {
      id: nextId(), payload, pinned: false,
      async pin() { if (world.fail.pin) throw apiError(world.fail.pin); msg.pinned = true; calls.pinned.push(msg.id); },
      async edit(p) {
        if (world.fail.edit) throw apiError(world.fail.edit);
        msg.payload = p;
        calls.edited.push({ id: msg.id, payload: p });
        return msg;
      },
    };
    world.messages.set(msg.id, msg);
    return msg;
  };

  const addChannel = (props = {}) => {
    const ch = {
      id: props.id ?? nextId(), name: props.name ?? 'chan', type: props.type ?? ChannelType.GuildVoice, parent: props.parent ?? null,
      guild, userLimit: props.userLimit ?? 0, bitrate: props.bitrate, topic: props.topic ?? null, members: new Collection(),
      children: { cache: new Collection() },
      permissionOverwrites: {
        cache: new Collection(),
        async set(list, reason) {
          if (world.fail.overwrites) throw apiError(world.fail.overwrites);
          calls.overwrites.push({ id: ch.id, list, reason });
          ch.permissionOverwrites.cache.clear();
          for (const o of list) ch.permissionOverwrites.cache.set(o.id, { id: o.id, type: o.type, allow: { bitfield: BigInt(o.allow) }, deny: { bitfield: BigInt(o.deny) } });
        },
      },
      messages: { async fetch(id) { const m = world.messages.get(id); if (!m) throw apiError(10008); return m; } },
      permissionsFor: () => botBits,
      isTextBased: () => true,
      isThread: () => false,
      async setUserLimit(n, reason) {
        if (world.fail.setUserLimit) throw apiError(world.fail.setUserLimit);
        calls.limits.push({ id: ch.id, n, reason });
        ch.userLimit = n;
        return ch;
      },
      async setName(name, reason) {
        if (world.fail.setName) throw apiError(world.fail.setName);
        calls.names.push({ id: ch.id, name, reason });
        ch.name = name;
        return ch;
      },
      async send(payload) {
        if (world.fail.send) throw apiError(world.fail.send);
        const msg = makeMessage(payload);
        calls.sent.push({ channelId: ch.id, payload, message: msg });
        return msg;
      },
      delete: (reason) => deleteChannel(ch.id, reason),
      toString: () => `<#${ch.id}>`,
    };
    for (const o of props.overwrites ?? []) {
      ch.permissionOverwrites.cache.set(String(o.id), { id: String(o.id), type: o.type, allow: { bitfield: BigInt(o.allow) }, deny: { bitfield: BigInt(o.deny) } });
    }
    channels.set(ch.id, ch);
    ch.parent?.children.cache.set(ch.id, ch);
    return ch;
  };
  world.addChannel = addChannel;
  /** Removes a channel from the cache; with `exists` it is still reachable by REST (a mere cache miss). */
  world.uncache = (id, { exists = false } = {}) => {
    const ch = channels.get(id);
    channels.delete(id);
    if (exists && ch) world.remote.set(id, ch);
  };

  guild.channels = {
    cache: channels,
    async fetch(id) {
      world.fetchLog.push(id);
      const code = world.fail.fetchById?.[id] ?? world.fail.fetch;
      if (code) throw apiError(code);
      const ch = channels.get(id) ?? world.remote.get(id);
      if (!ch) throw apiError(10003);
      return ch;
    },
    async create(opts) {
      const f = world.fail.create;
      if (f && (f.type === undefined || f.type === opts.type)) throw apiError(f.code);
      const parent = opts.parent ? channels.get(opts.parent) : null;
      const ch = addChannel({
        name: opts.name, type: opts.type, parent, userLimit: opts.userLimit, bitrate: opts.bitrate, topic: opts.topic, overwrites: opts.permissionOverwrites,
      });
      calls.created.push({ opts, channel: ch });
      return ch;
    },
    delete: (id, reason) => deleteChannel(id, reason),
  };
  guild.members = {
    me,
    async fetch(id) {
      const m = world.members.get(id);
      if (!m) throw apiError(10007);
      return m;
    },
  };

  world.addMember = ({ id = nextId(), name, bot = false, roles = [], perms = 0n } = {}) => {
    const m = {
      id, displayName: name ?? `User${id.slice(-4)}`, guild,
      user: { id, bot, username: (name ?? `user${id.slice(-4)}`).toLowerCase(), tag: `${(name ?? 'user').toLowerCase()}#0`, displayAvatarURL: () => '' },
      roles: { cache: new Collection(roles.map((r) => [r, { id: r }])) }, permissions: new PermissionsBitField(perms),
    };
    m.voice = {
      channelId: null,
      async setChannel(ch, reason) {
        if (world.fail.move) throw apiError(world.fail.move);
        calls.moved.push({ memberId: id, channelId: ch?.id ?? null, reason });
        place(m, ch);
      },
      async disconnect(reason) {
        if (world.fail.disconnect) throw apiError(world.fail.disconnect);
        calls.disconnected.push({ memberId: id, reason });
        place(m, null);
      },
    };
    world.members.set(id, m);
    return m;
  };
  return world;
}

/** A category + hub channel + hub row. */
async function setupHub(world, overrides = {}) {
  const category = world.addChannel({ name: 'Voice', type: ChannelType.GuildCategory });
  const hubChannel = world.addChannel({ name: 'Join to Create', parent: category });
  await ensureGuildSettings(world.guild.id);
  const hub = await prisma.tempVoiceHub.create({ data: { guildId: world.guild.id, channelId: hubChannel.id, ...overrides } });
  tv.invalidateIndex(world.guild.id);
  return { category, hubChannel, hub };
}

/** A temporary channel (fake channel + row) owned by `owner`, who is placed in it. */
async function setupTemp(world, { hub, category }, owner, overrides = {}) {
  const vc = world.addChannel({ name: 'Temp', parent: category });
  const row = await prisma.tempVoiceChannel.create({
    data: { guildId: world.guild.id, hubId: hub.id, channelId: vc.id, ownerId: owner.id, index: 1, ...overrides },
  });
  world.place(owner, vc);
  tv.invalidateIndex(world.guild.id);
  return { vc, row };
}

const getRow = (channelId) => prisma.tempVoiceChannel.findUnique({ where: { channelId } });
const getJob = (channelId) => prisma.scheduledJob.findUnique({ where: { dedupeKey: tv.cleanupKey(channelId) } });
const title = (r) => r.embed.data.title;

// ── RAW_SQL and CRUD ──────────────────────────────────────────────────────────

test('DB: every RAW_SQL statement executes against the migrated schema', { skip: skipDb }, async () => {
  await ensureGuildSettings(G(1));
  for (const { name, build } of RAW_SQL) {
    const rows = await prisma.$transaction((tx) => tx.$queryRaw(build()));
    assert.ok(Array.isArray(rows), name);
  }
});

test('DB: createHub, the guild cap, duplicates, updateHub and findHubByChannel', { skip: skipDb }, async () => {
  const gid = G(1);
  const h1 = await tv.createHub(gid, { channelId: 'c1' });
  assert.equal(h1.guildId, gid);
  assert.equal(h1.nameFormat, DEFAULT_NAME_FORMAT);
  assert.equal(h1.keepAliveMinutes, 0);
  assert.equal(h1.syncPermissions, true);
  assert.deepEqual(h1.moderatorRoleIds, []);
  await assert.rejects(() => tv.createHub(gid, { channelId: 'c1' }), (e) => e.code === 'P2002');
  assert.equal(await tv.countHubs(gid), 1);
  assert.equal((await tv.findHubByChannel(gid, 'c1')).id, h1.id);
  assert.equal(await tv.findHubByChannel(G(2), 'c1'), null, 'another guild cannot see the hub');
  assert.equal(await tv.findHubByChannel(gid, 'nope'), null);

  for (let i = 2; i <= LIMITS.TEMPVOICE_HUBS; i++) await tv.createHub(gid, { channelId: `c${i}` });
  await assert.rejects(() => tv.createHub(gid, { channelId: 'c11' }), (e) => e instanceof tv.HubLimitError);
  assert.equal(await tv.countHubs(gid), LIMITS.TEMPVOICE_HUBS);

  const updated = await tv.updateHub(gid, h1.id, { userLimit: 5, keepAliveMinutes: 3 });
  assert.equal(updated.userLimit, 5);
  assert.equal(updated.keepAliveMinutes, 3);
  assert.equal(await tv.updateHub(G(2), h1.id, { userLimit: 9 }), null, 'a hub of another guild is not touched');
  assert.equal((await prisma.tempVoiceHub.findUnique({ where: { id: h1.id } })).userLimit, 5);
});

test('DB: the hub cap holds under concurrent creation', { skip: skipDb }, async () => {
  const gid = G(3);
  for (let i = 1; i <= LIMITS.TEMPVOICE_HUBS - 2; i++) await tv.createHub(gid, { channelId: `d${i}` });
  const results = await Promise.allSettled([1, 2, 3, 4].map((i) => tv.createHub(gid, { channelId: `e${i}` })));
  assert.equal(results.filter((r) => r.status === 'fulfilled').length, 2);
  assert.ok(results.filter((r) => r.status === 'rejected').every((r) => r.reason instanceof tv.HubLimitError));
  assert.equal(await tv.countHubs(gid), LIMITS.TEMPVOICE_HUBS);
});

test('DB: updateHubRoleList outcomes', { skip: skipDb }, async () => {
  const gid = G(4);
  const hub = await tv.createHub(gid, { channelId: 'r1' });
  const op = (field, o, role) => tv.updateHubRoleList(gid, hub.id, field, o, role);
  assert.equal(await op('ignoredRoleIds', 'add', 'a'), 'ok');
  assert.equal(await op('ignoredRoleIds', 'add', 'a'), 'exists');
  assert.equal(await op('ignoredRoleIds', 'remove', 'zzz'), 'missing');
  assert.equal(await op('moderatorRoleIds', 'add', 'm'), 'ok');
  assert.deepEqual((await prisma.tempVoiceHub.findUnique({ where: { id: hub.id } })).moderatorRoleIds, ['m']);
  assert.equal(await op('ignoredRoleIds', 'remove', 'a'), 'ok');
  for (let i = 0; i < MAX_HUB_ROLE_LIST - 1; i++) assert.equal(await op('moderatorRoleIds', 'add', `x${i}`), 'ok');
  assert.equal(await op('moderatorRoleIds', 'add', 'overflow'), 'limit');
  await assert.rejects(() => op('createdAt', 'add', 'a'), /Invalid role list field/);
  await assert.rejects(() => tv.updateHubRoleList(gid, hub.id, 'ignoredRoleIds', 'toggle', 'a'), /Invalid role list operation/);
  assert.equal(await tv.updateHubRoleList(G(5), hub.id, 'ignoredRoleIds', 'add', 'a'), 'gone', 'wrong guild');
  assert.equal(await tv.updateHubRoleList(gid, 'no-such-hub', 'ignoredRoleIds', 'add', 'a'), 'gone');
});

test('DB: listHubs counts channels; listHubChannels and isTempChannel', { skip: skipDb }, async () => {
  const world = makeWorld(G(6));
  const setup = await setupHub(world);
  const bob = world.addMember({ name: 'Bob' });
  const { vc } = await setupTemp(world, setup, bob);
  const hubs = await tv.listHubs(G(6));
  assert.equal(hubs.length, 1);
  assert.equal(hubs[0]._count.channels, 1);
  assert.equal((await tv.listHubChannels(setup.hub.id)).length, 1);
  assert.equal(await tv.isTempChannel(vc.id), true);
  assert.equal(await tv.isTempChannel(setup.hubChannel.id), false);
});

// ── Hub join (create flow) ────────────────────────────────────────────────────

test('DB: a hub join creates the channel, the text channel and the control panel', { skip: skipDb }, async () => {
  const world = makeWorld(G(7));
  const { category, hubChannel, hub } = await setupHub(world, { createTextChannel: true, userLimit: 5, bitrate: 128000 });
  const bob = world.addMember({ name: 'Bob' });
  world.place(bob, hubChannel);
  await tv.queueHubJoin(world.guild, hub.id, bob, SETTINGS_ON);

  assert.equal(world.calls.created.length, 2);
  const voice = world.calls.created[0];
  assert.equal(voice.opts.name, "#1 - Bob's Channel");
  assert.equal(voice.opts.parent, category.id);
  assert.equal(voice.opts.userLimit, 5);
  assert.equal(voice.opts.bitrate, 96000, 'clamped to the guild maximum');
  const ownerOverwrite = voice.opts.permissionOverwrites.find((o) => o.id === bob.id);
  assert.equal(ownerOverwrite.allow, OWNER_PERMS);
  assert.equal(ownerOverwrite.type, OverwriteType.Member);
  const text = world.calls.created[1];
  assert.equal(text.opts.type, ChannelType.GuildText);
  assert.equal(text.opts.name, '1-bobs-channel');
  assert.equal(text.opts.permissionOverwrites.find((o) => o.id === world.guild.id).deny, P.ViewChannel);
  assert.ok(text.opts.permissionOverwrites.some((o) => o.id === bob.id && o.allow === P.ViewChannel));

  const row = await getRow(voice.channel.id);
  assert.equal(row.ownerId, bob.id);
  assert.equal(row.index, 1);
  assert.equal(row.textChannelId, text.channel.id);
  assert.equal(bob.voice.channelId, voice.channel.id, 'the member was moved in');
  const panel = world.calls.sent.find((s) => s.channelId === text.channel.id);
  assert.ok(panel, 'the panel is posted in the companion text channel');
  assert.equal(panel.payload.components.length, 2);
  assert.equal(panel.message.pinned, true);
  assert.equal(row.controlMessageId, panel.message.id);

  // a second member gets the next index
  const alice = world.addMember({ name: 'Alice' });
  world.place(alice, hubChannel);
  await tv.queueHubJoin(world.guild, hub.id, alice, SETTINGS_ON);
  assert.equal(world.calls.created[2].opts.name, "#2 - Alice's Channel");

  // an owner who joins the hub again is moved back into the existing channel
  world.place(bob, hubChannel);
  const createdBefore = world.calls.created.length;
  await tv.queueHubJoin(world.guild, hub.id, bob, SETTINGS_ON);
  assert.equal(world.calls.created.length, createdBefore);
  assert.equal(bob.voice.channelId, voice.channel.id);
  assert.equal(await prisma.tempVoiceChannel.count({ where: { hubId: hub.id, ownerId: bob.id } }), 1);
});

test('DB: the creation cooldown refuses a second creation within 5 seconds', { skip: skipDb }, async () => {
  const world = makeWorld(G(8));
  const { hubChannel, hub } = await setupHub(world);
  const carol = world.addMember({ name: 'Carol' });
  world.place(carol, hubChannel);
  await tv.queueHubJoin(world.guild, hub.id, carol, SETTINGS_ON);
  assert.equal(world.calls.created.length, 1);
  const first = world.calls.created[0].channel;
  // she leaves; the empty channel is deleted at once (keep alive 0)
  world.place(carol, null);
  await tv.onLeaveTemp(world.guild, first.id, carol);
  assert.equal(world.calls.deleted.length, 1);
  // and rejoins the hub within the cooldown
  world.place(carol, hubChannel);
  await tv.queueHubJoin(world.guild, hub.id, carol, SETTINGS_ON);
  assert.equal(world.calls.created.length, 1, 'no second channel yet (a retry is scheduled)');
  world.place(carol, null); // the retry finds her gone and does nothing
});

test('DB: creation failures leave no row and keep the member in the hub', { skip: skipDb }, async () => {
  // the bot lacks permissions
  const w1 = makeWorld(G(9), { botPerms: P.ViewChannel });
  const s1 = await setupHub(w1);
  const m1 = w1.addMember({ name: 'A' });
  w1.place(m1, s1.hubChannel);
  await tv.queueHubJoin(w1.guild, s1.hub.id, m1, SETTINGS_ON);
  assert.equal(w1.calls.created.length, 0);
  assert.equal(m1.voice.channelId, s1.hubChannel.id);

  // Discord refuses the channel (500 channels)
  const w2 = makeWorld(G(10));
  const s2 = await setupHub(w2);
  w2.fail.create = { code: 30013 };
  const m2 = w2.addMember({ name: 'B' });
  w2.place(m2, s2.hubChannel);
  await tv.queueHubJoin(w2.guild, s2.hub.id, m2, SETTINGS_ON);
  assert.equal(await prisma.tempVoiceChannel.count({ where: { hubId: s2.hub.id } }), 0);
  assert.equal(m2.voice.channelId, s2.hubChannel.id);

  // the move fails (the member left meanwhile): the new channel is deleted again
  const w3 = makeWorld(G(11));
  const s3 = await setupHub(w3);
  w3.fail.move = 40032;
  const m3 = w3.addMember({ name: 'C' });
  w3.place(m3, s3.hubChannel);
  await tv.queueHubJoin(w3.guild, s3.hub.id, m3, SETTINGS_ON);
  assert.equal(w3.calls.created.length, 1);
  assert.equal(w3.calls.deleted.length, 1);
  assert.equal(await prisma.tempVoiceChannel.count({ where: { hubId: s3.hub.id } }), 0);

  // the member left the hub before the task ran; and a retired hub (channel gone) creates nothing
  const w4 = makeWorld(G(12));
  const s4 = await setupHub(w4);
  const m4 = w4.addMember({ name: 'D' });
  await tv.queueHubJoin(w4.guild, s4.hub.id, m4, SETTINGS_ON);
  w4.place(m4, s4.hubChannel);
  w4.uncache(s4.hubChannel.id);
  await tv.queueHubJoin(w4.guild, s4.hub.id, m4, SETTINGS_ON);
  assert.equal(w4.calls.created.length, 0);
});

// ── Leaving and joining temp channels ─────────────────────────────────────────

test('DB: an emptied channel is deleted at once (keep alive 0) and marked as a bot deletion', { skip: skipDb }, async () => {
  const world = makeWorld(G(13));
  const setup = await setupHub(world);
  const bob = world.addMember({ name: 'Bob' });
  const { vc } = await setupTemp(world, setup, bob);
  world.place(bob, null);
  await tv.onLeaveTemp(world.guild, vc.id, bob);
  assert.equal(world.calls.deleted.length, 1);
  assert.equal(await getRow(vc.id), null);
  assert.equal(wasDeletedByBot(vc.id), 'tempvoice');
  assert.equal(await prisma.tempVoiceHub.count({ where: { id: setup.hub.id } }), 1, 'the hub stays while its channel exists');
});

test('DB: keep alive schedules the cleanup job; a return cancels it', { skip: skipDb }, async () => {
  const world = makeWorld(G(14));
  const setup = await setupHub(world, { keepAliveMinutes: 5 });
  const bob = world.addMember({ name: 'Bob' });
  const { vc } = await setupTemp(world, setup, bob);
  world.place(bob, null);
  const before = Date.now();
  await tv.onLeaveTemp(world.guild, vc.id, bob);
  assert.equal(world.calls.deleted.length, 0);
  const row = await getRow(vc.id);
  assert.ok(row.emptySince, 'emptySince is set');
  assert.ok(row.ownerLeftAt, 'the owner left');
  const job = await getJob(vc.id);
  assert.equal(job.type, 'tempvoice.cleanup');
  assert.deepEqual(job.payload, { channelId: vc.id });
  assert.ok(Math.abs(job.runAt.getTime() - (before + 5 * 60_000)) < 5_000);

  world.place(bob, vc);
  await tv.onJoinTemp(world.guild, vc.id, bob);
  const back = await getRow(vc.id);
  assert.equal(back.emptySince, null);
  assert.equal(back.ownerLeftAt, null, 'the owner returned');
  assert.equal(await getJob(vc.id), null, 'the job is cancelled');
});

test('DB: the owner leaving a non-empty channel starts the ownership-lock clock', { skip: skipDb }, async () => {
  const world = makeWorld(G(15));
  const setup = await setupHub(world);
  const bob = world.addMember({ name: 'Bob' });
  const amy = world.addMember({ name: 'Amy' });
  const { vc } = await setupTemp(world, setup, bob);
  world.place(amy, vc);
  world.place(bob, null);
  await tv.onLeaveTemp(world.guild, vc.id, bob);
  assert.equal(world.calls.deleted.length, 0);
  assert.ok((await getRow(vc.id)).ownerLeftAt);
  // a non-owner leaving changes nothing
  const cy = world.addMember({ name: 'Cy' });
  world.place(cy, vc);
  world.place(cy, null);
  const before = (await getRow(vc.id)).ownerLeftAt;
  await tv.onLeaveTemp(world.guild, vc.id, cy);
  assert.deepEqual((await getRow(vc.id)).ownerLeftAt, before);
});

test('DB: a retired hub (its channel is gone) is deleted when its last channel closes', { skip: skipDb }, async () => {
  const world = makeWorld(G(16));
  const setup = await setupHub(world);
  const bob = world.addMember({ name: 'Bob' });
  const { vc } = await setupTemp(world, setup, bob);
  world.uncache(setup.hubChannel.id); // the hub channel was deleted (REST says 10003)
  world.place(bob, null);
  await tv.onLeaveTemp(world.guild, vc.id, bob);
  assert.equal(await getRow(vc.id), null);
  assert.equal(await prisma.tempVoiceHub.count({ where: { id: setup.hub.id } }), 0);
});

test('DB: a failed deletion keeps the row and schedules a retry job', { skip: skipDb }, async () => {
  const world = makeWorld(G(17));
  const setup = await setupHub(world);
  const bob = world.addMember({ name: 'Bob' });
  const { vc } = await setupTemp(world, setup, bob);
  world.place(bob, null);
  world.fail.delete = 50013;
  await tv.onLeaveTemp(world.guild, vc.id, bob);
  assert.ok(await getRow(vc.id), 'the row is kept');
  const job = await getJob(vc.id);
  assert.ok(job);
  assert.ok(job.runAt.getTime() > Date.now() + 4 * 60_000);
});

test('DB: a banned member who is moved into the channel is disconnected, a protected one is not', { skip: skipDb }, async () => {
  const world = makeWorld(G(18));
  const setup = await setupHub(world, { ignoredRoleIds: ['rIgnored'] });
  const bob = world.addMember({ name: 'Bob' });
  const bad = world.addMember({ name: 'Bad' });
  const safe = world.addMember({ name: 'Safe', roles: ['rIgnored'] });
  const { vc } = await setupTemp(world, setup, bob, { bannedUserIds: [bad.id, safe.id] });
  world.place(bad, vc);
  await tv.onJoinTemp(world.guild, vc.id, bad);
  assert.deepEqual(world.calls.disconnected.map((d) => d.memberId), [bad.id]);
  world.place(safe, vc);
  await tv.onJoinTemp(world.guild, vc.id, safe);
  assert.equal(world.calls.disconnected.length, 1, 'the protected member stays');
});

// ── Rule 20: a cache miss is not a deletion ───────────────────────────────────

test('DB: D2 onLeaveTemp: a REST 10003 purges the row, any other answer keeps it', { skip: skipDb }, async () => {
  const world = makeWorld(G(19));
  const setup = await setupHub(world);
  const a = world.addMember({ name: 'A' });
  const b = world.addMember({ name: 'B' });
  const c = world.addMember({ name: 'C' });
  const t1 = await setupTemp(world, setup, a);
  const t2 = await setupTemp(world, setup, b);
  const t3 = await setupTemp(world, setup, c);
  world.place(a, null);
  world.place(b, null);
  world.place(c, null);
  world.uncache(t1.vc.id); // gone for real: fetch answers 10003
  world.uncache(t2.vc.id);
  world.uncache(t3.vc.id);
  world.fail.fetchById = { [t2.vc.id]: 50001 };
  await tv.onLeaveTemp(world.guild, t1.vc.id, a);
  await tv.onLeaveTemp(world.guild, t2.vc.id, b);
  assert.equal(await getRow(t1.vc.id), null, '10003 purges');
  assert.ok(await getRow(t2.vc.id), '50001 keeps the row');
  // the channel exists but is not cached: it is found by REST and handled normally
  world.remote.set(t3.vc.id, t3.vc);
  const before = world.calls.deleted.length;
  await tv.onLeaveTemp(world.guild, t3.vc.id, c);
  assert.equal(world.calls.deleted.length, before + 1, 'a cache miss of an existing channel is not a purge');
  assert.equal(await getRow(t3.vc.id), null);
  // an unavailable guild is never acted on
  const w2 = makeWorld(G(20), { available: false });
  const s2 = await setupHub(w2);
  const d = w2.addMember({ name: 'D' });
  const t4 = await setupTemp(w2, s2, d);
  w2.uncache(t4.vc.id);
  await tv.onLeaveTemp(w2.guild, t4.vc.id, d);
  assert.ok(await getRow(t4.vc.id));
  assert.equal(w2.fetchLog.length, 0, 'no REST call for an unavailable guild');
});

test('DB: handleCleanupJob deletes an empty channel and honours rule 20', { skip: skipDb }, async () => {
  const world = makeWorld(G(21));
  const setup = await setupHub(world, { keepAliveMinutes: 5 });
  const mk = async (name) => {
    const m = world.addMember({ name });
    const t = await setupTemp(world, setup, m, { emptySince: new Date() });
    return { m, ...t };
  };
  const job = (channelId, guildId = G(21)) => ({ guildId, payload: { channelId } });
  const ctx = { guild: world.guild };

  const empty = await mk('Empty');
  world.place(empty.m, null);
  assert.equal(await tv.handleCleanupJob(job(empty.vc.id), ctx), undefined);
  assert.equal(await getRow(empty.vc.id), null);
  assert.equal(world.calls.deleted.length, 1);

  const busy = await mk('Busy');
  assert.equal(await tv.handleCleanupJob(job(busy.vc.id), ctx), undefined);
  assert.ok(await getRow(busy.vc.id), 'occupied: kept');
  assert.equal((await getRow(busy.vc.id)).emptySince, null);

  const gone = await mk('Gone');
  world.place(gone.m, null);
  world.uncache(gone.vc.id);
  assert.equal(await tv.handleCleanupJob(job(gone.vc.id), ctx), undefined);
  assert.equal(await getRow(gone.vc.id), null, '10003 purges the row');

  const unsure = await mk('Unsure');
  world.place(unsure.m, null);
  world.uncache(unsure.vc.id);
  world.fail.fetchById = { [unsure.vc.id]: 500 };
  const retry = await tv.handleCleanupJob(job(unsure.vc.id), ctx);
  assert.ok(retry.nextRunAt instanceof Date && retry.nextRunAt.getTime() > Date.now() + 4 * 60_000, 'a transient error asks to be re-run later');
  assert.ok(await getRow(unsure.vc.id), 'the row is kept');

  const stuck = await mk('Stuck');
  world.place(stuck.m, null);
  world.fail.delete = 50013;
  await assert.rejects(() => tv.handleCleanupJob(job(stuck.vc.id), ctx), /delete failed \(50013\)/);
  assert.ok(await getRow(stuck.vc.id));
  world.fail.delete = null;

  // wrong guild, bad payloads, repeated run
  assert.equal(await tv.handleCleanupJob(job(stuck.vc.id, G(22)), ctx), undefined);
  assert.ok(await getRow(stuck.vc.id));
  assert.equal(await tv.handleCleanupJob({ guildId: G(21), payload: { channelId: 'not-a-snowflake' } }, ctx), undefined);
  assert.equal(await tv.handleCleanupJob({ guildId: G(21), payload: null }, ctx), undefined);
  assert.equal(await tv.handleCleanupJob(job(empty.vc.id), ctx), undefined, 'idempotent');
});

test('DB: D2 runVoiceAction on a channel that is not cached', { skip: skipDb }, async () => {
  const world = makeWorld(G(23));
  const setup = await setupHub(world);
  const bob = world.addMember({ name: 'Bob' });
  const a = await setupTemp(world, setup, bob);
  world.uncache(a.vc.id);
  world.fail.fetchById = { [a.vc.id]: 50001 };
  const keep = await tv.runVoiceAction({ guild: world.guild, actor: bob, channelId: a.vc.id, action: 'lock', settings: SETTINGS_ON, source: 'button' });
  assert.equal(title(keep), '❌  Something Went Wrong');
  assert.ok(await getRow(a.vc.id), 'row kept on a non-10003 error');
  world.fail.fetchById = {};
  const purged = await tv.runVoiceAction({ guild: world.guild, actor: bob, channelId: a.vc.id, action: 'lock', settings: SETTINGS_ON, source: 'button' });
  assert.equal(title(purged), '❌  Channel Gone');
  assert.equal(await getRow(a.vc.id), null, '10003 purges the row');
});

// ── Voice actions ─────────────────────────────────────────────────────────────

async function actionWorld(n, hubOverrides = {}, worldOpts = {}) {
  const world = makeWorld(G(n), worldOpts);
  const setup = await setupHub(world, hubOverrides);
  const owner = world.addMember({ name: 'Owner' });
  const temp = await setupTemp(world, setup, owner);
  const run = (actor, action, extra = {}) => tv.runVoiceAction({
    guild: world.guild, actor, channelId: temp.vc.id, action, settings: SETTINGS_ON, source: 'command', ...extra,
  });
  return { world, setup, owner, temp, run };
}

test('DB: lock, unlock, hide and reveal rewrite the overwrites and the row', { skip: skipDb }, async () => {
  const { world, temp, owner, run } = await actionWorld(24, { ignoredRoleIds: ['rI'] });
  world.guild.roles.cache.set('rI', { id: 'rI' });
  const stranger = world.addMember({ name: 'Stranger' });
  assert.equal(title(await run(stranger, 'lock')), '❌  Not Your Channel');

  const locked = await run(owner, 'lock');
  assert.equal(locked.ok, true);
  assert.equal(title(locked), '✅  Channel Locked');
  const last = () => Object.fromEntries(world.calls.overwrites.at(-1).list.map((o) => [o.id, o]));
  assert.equal(last()[world.guild.id].deny & P.Connect, P.Connect);
  assert.equal(last().rI.allow & P.Connect, P.Connect, 'ignored roles can still join');
  assert.equal(last()[owner.id].allow, OWNER_PERMS);
  assert.equal(last()[BOT_ID].allow, BOT_VOICE_PERMS);
  assert.equal((await getRow(temp.vc.id)).locked, true);
  assert.equal(title(await run(owner, 'lock')), '⚠️  No Change');

  assert.equal(title(await run(owner, 'hide')), '✅  Channel Hidden');
  assert.equal(last()[world.guild.id].deny & P.ViewChannel, P.ViewChannel);
  assert.equal(title(await run(owner, 'unlock')), '✅  Channel Unlocked');
  assert.equal(last()[world.guild.id].deny & P.Connect, 0n);
  assert.equal(title(await run(owner, 'reveal')), '✅  Channel Revealed');
  assert.equal(world.calls.overwrites.at(-1).list.find((o) => o.id === world.guild.id), undefined, 'back to an empty @everyone entry (dropped)');
  const row = await getRow(temp.vc.id);
  assert.equal(row.locked, false);
  assert.equal(row.hidden, false);
  assert.equal(title(await run(owner, 'reveal')), '⚠️  No Change');
  // a Discord failure saves nothing
  world.fail.overwrites = 50013;
  assert.equal(title(await run(owner, 'lock')), '❌  Something Went Wrong');
  assert.equal((await getRow(temp.vc.id)).locked, false);
});

test('DB: a moderator may control any channel; a missing bot permission is reported', { skip: skipDb }, async () => {
  const { world, run } = await actionWorld(25);
  const mod = world.addMember({ name: 'Mod', perms: P.BanMembers });
  assert.equal(title(await run(mod, 'lock')), '✅  Channel Locked');
  const w2 = await actionWorld(26, {}, { botPerms: P.ViewChannel | P.Connect });
  const r = await w2.run(w2.owner, 'lock');
  assert.equal(title(r), '❌  Missing Permissions');
  assert.match(r.embed.data.description, /Manage Channels, Manage Roles/);
  assert.equal(w2.world.calls.overwrites.length, 0);
});

test('DB: limit and rename (with the 2-per-10-minutes limiter)', { skip: skipDb }, async () => {
  const { world, temp, owner, run } = await actionWorld(27);
  assert.equal(title(await run(owner, 'limit', { value: 100 })), '❌  Invalid Limit');
  assert.equal(title(await run(owner, 'limit', { value: 'x' })), '❌  Invalid Limit');
  assert.equal(title(await run(owner, 'limit', { value: 5 })), '✅  Limit Updated');
  assert.equal(temp.vc.userLimit, 5);
  assert.equal(title(await run(owner, 'limit', { value: 5 })), '⚠️  No Change');
  assert.equal(title(await run(owner, 'limit', { value: 0 })), '✅  Limit Updated');

  assert.equal(title(await run(owner, 'rename', { value: '   ' })), '❌  Invalid Name');
  assert.equal(title(await run(owner, 'rename', { value: 'Temp' })), '⚠️  No Change');
  const first = await run(owner, 'rename', { value: 'Study\nroom' });
  assert.equal(title(first), '✅  Channel Renamed');
  assert.equal(temp.vc.name, 'Study room');
  assert.equal(title(await run(owner, 'rename', { value: 'Second' })), '✅  Channel Renamed');
  const third = await run(owner, 'rename', { value: 'Third' });
  assert.equal(title(third), '❌  Slow Down');
  assert.equal(world.calls.names.length, 2, 'the third rename never reached Discord');
  assert.equal(temp.vc.name, 'Second');
});

test('DB: kick', { skip: skipDb }, async () => {
  const { world, temp, owner, run } = await actionWorld(28, { ignoredRoleIds: ['rI'] });
  const guest = world.addMember({ name: 'Guest' });
  const away = world.addMember({ name: 'Away' });
  const protectedGuest = world.addMember({ name: 'Prot', roles: ['rI'] });
  const mod = world.addMember({ name: 'Mod', perms: P.KickMembers });
  world.place(guest, temp.vc);
  world.place(protectedGuest, temp.vc);
  assert.equal(title(await run(owner, 'kick', { targetId: owner.id })), '❌  Invalid Member');
  const modKickOwner = await run(mod, 'kick', { targetId: owner.id });
  assert.equal(title(modKickOwner), '❌  Invalid Member');
  assert.match(modKickOwner.embed.data.description, /owns this channel/);
  assert.equal(title(await run(owner, 'kick', { targetId: protectedGuest.id })), '❌  Protected Member');
  assert.equal(title(await run(owner, 'kick', { targetId: away.id })), '❌  Not In Channel');
  assert.equal(title(await run(owner, 'kick', { targetId: '999999999999999999' })), '❌  Invalid Member');
  assert.equal(title(await run(owner, 'kick', { targetId: guest.id })), '✅  Member Kicked');
  assert.deepEqual(world.calls.disconnected.map((d) => d.memberId), [guest.id]);
  assert.equal(guest.voice.channelId, null);
});

test('DB: ban and unban', { skip: skipDb }, async () => {
  const { world, temp, owner, run } = await actionWorld(29);
  const bad = world.addMember({ name: 'Bad' });
  world.place(bad, temp.vc);
  const banned = await run(owner, 'ban', { targetId: bad.id });
  assert.equal(title(banned), '✅  Member Banned');
  assert.deepEqual((await getRow(temp.vc.id)).bannedUserIds, [bad.id]);
  const entry = world.calls.overwrites.at(-1).list.find((o) => o.id === bad.id);
  assert.equal(entry.deny, P.Connect);
  assert.equal(entry.type, OverwriteType.Member);
  assert.equal(bad.voice.channelId, null, 'a connected member is disconnected');
  assert.equal(title(await run(owner, 'ban', { targetId: bad.id })), '⚠️  No Change');
  assert.equal(title(await run(owner, 'ban', { targetId: owner.id })), '❌  Invalid Member');
  assert.equal(title(await run(owner, 'ban', { targetId: '999999999999999999' })), '❌  Invalid Member');

  assert.equal(title(await run(owner, 'unban', { targetId: bad.id })), '✅  Member Unbanned');
  assert.deepEqual((await getRow(temp.vc.id)).bannedUserIds, []);
  assert.equal(world.calls.overwrites.at(-1).list.find((o) => o.id === bad.id), undefined);
  assert.equal(title(await run(owner, 'unban', { targetId: bad.id })), '⚠️  No Change');
  // unban works for a user who left the server (no member fetch)
  const ghost = '888888888888888888';
  await prisma.tempVoiceChannel.update({ where: { channelId: temp.vc.id }, data: { bannedUserIds: [ghost] } });
  assert.equal(title(await run(owner, 'unban', { targetId: ghost })), '✅  Member Unbanned');
  // the cap
  await prisma.tempVoiceChannel.update({
    where: { channelId: temp.vc.id }, data: { bannedUserIds: Array.from({ length: MAX_BANS_PER_CHANNEL }, (_, i) => `7${String(i).padStart(17, '0')}`) },
  });
  assert.equal(title(await run(owner, 'ban', { targetId: bad.id })), '❌  Limit Reached');
});

test('DB: transfer changes the owner, refreshes the panel and posts a notice', { skip: skipDb }, async () => {
  const { world, temp, owner, run } = await actionWorld(30);
  const panel = await temp.vc.send({ content: 'panel' });
  await prisma.tempVoiceChannel.update({ where: { channelId: temp.vc.id }, data: { controlMessageId: panel.id } });
  const next = world.addMember({ name: 'Next' });
  const away = world.addMember({ name: 'Away' });
  const robot = world.addMember({ name: 'Robot', bot: true });
  world.place(next, temp.vc);
  world.place(robot, temp.vc);
  assert.equal(title(await run(owner, 'transfer', { targetId: owner.id })), '⚠️  No Change');
  assert.equal(title(await run(owner, 'transfer', { targetId: robot.id })), '❌  Invalid Member');
  assert.equal(title(await run(owner, 'transfer', { targetId: away.id })), '❌  Not In Channel');
  await prisma.tempVoiceChannel.update({ where: { channelId: temp.vc.id }, data: { bannedUserIds: [next.id] } });
  const ok = await run(owner, 'transfer', { targetId: next.id });
  assert.equal(title(ok), '✅  Ownership Transferred');
  const row = await getRow(temp.vc.id);
  assert.equal(row.ownerId, next.id);
  assert.deepEqual(row.bannedUserIds, [], 'a new owner is unbanned');
  const list = world.calls.overwrites.at(-1).list;
  assert.equal(list.find((o) => o.id === next.id).allow, OWNER_PERMS);
  assert.equal(list.find((o) => o.id === owner.id), undefined, 'the old owner loses the owner permissions');
  assert.equal(world.calls.edited.length, 1, 'the panel message was edited');
  assert.match(world.calls.edited[0].payload.embeds[0].data.description, new RegExp(`<@${next.id}>`));
  const notice = world.calls.sent.at(-1);
  assert.match(notice.payload.content, new RegExp(`<@${next.id}> is now the owner`));
  assert.deepEqual(notice.payload.allowedMentions, { parse: [] });
  assert.equal(title(await run(owner, 'lock')), '❌  Not Your Channel', 'the former owner no longer controls it');
});

test('DB: claim honours presence, the ownership lock and moderators', { skip: skipDb }, async () => {
  const { world, temp, owner, run } = await actionWorld(31, { ownershipLockMinutes: 2 });
  const amy = world.addMember({ name: 'Amy' });
  const outsider = world.addMember({ name: 'Outsider' });
  const mod = world.addMember({ name: 'Mod', perms: P.BanMembers });
  world.place(amy, temp.vc);
  assert.equal(title(await run(owner, 'claim')), '⚠️  No Change');
  assert.equal(title(await run(outsider, 'claim')), '❌  Not In Channel');
  assert.equal(title(await run(amy, 'claim')), '❌  Owner Still Here');
  // the owner leaves; the lock timer starts
  world.place(owner, null);
  const locked = await run(amy, 'claim');
  assert.equal(title(locked), '❌  Not Yet');
  assert.match(locked.embed.data.description, /<t:\d+:R>/);
  assert.ok((await getRow(temp.vc.id)).ownerLeftAt, 'found absent: ownerLeftAt was set');
  world.place(mod, temp.vc);
  assert.equal(title(await run(mod, 'claim')), '✅  Channel Claimed');
  assert.equal((await getRow(temp.vc.id)).ownerId, mod.id);

  // with no lock the first member may claim at once
  const w2 = await actionWorld(32, { ownershipLockMinutes: 0 });
  const bea = w2.world.addMember({ name: 'Bea' });
  w2.world.place(bea, w2.temp.vc);
  w2.world.place(w2.owner, null);
  assert.equal(title(await w2.run(bea, 'claim')), '✅  Channel Claimed');
  assert.equal((await getRow(w2.temp.vc.id)).ownerId, bea.id);
  assert.equal((await getRow(w2.temp.vc.id)).ownerLeftAt, null);
});

test('DB: a channel without a row is "not in a temporary channel" for /voice', { skip: skipDb }, async () => {
  const { world, owner } = await actionWorld(33);
  const plain = world.addChannel({ name: 'General' });
  const base = { guild: world.guild, actor: owner, channelId: plain.id, settings: SETTINGS_ON };
  assert.equal(title(await tv.runVoiceAction({ ...base, action: 'lock', source: 'command' })), '❌  Not In A Temporary Channel');
  assert.equal(title(await tv.runVoiceAction({ ...base, action: 'lock', source: 'button' })), '❌  Channel Gone');
  assert.equal(title(await tv.runVoiceAction({ ...base, action: 'explode', source: 'command' })), '❌  Something Went Wrong');
});

// ── Component handlers with a fake interaction ────────────────────────────────

function fakeInteraction(world, member, customId, extra = {}) {
  const log = { deferred: null, replies: [], edits: [], modal: null };
  const i = {
    customId, guild: world.guild, guildId: world.guild.id, member, user: member.user, createdTimestamp: Date.now(),
    deferred: false, replied: false, ephemeral: null,
    inCachedGuild: () => true,
    deferReply: async (o) => { i.deferred = true; i.ephemeral = Boolean(o?.ephemeral); log.deferred = o; },
    deferUpdate: async () => { i.deferred = true; },
    editReply: async (p) => { log.edits.push(p); return {}; },
    reply: async (p) => { i.replied = true; log.replies.push(p); return {}; },
    followUp: async (p) => { log.replies.push(p); return {}; },
    showModal: async (m) => { log.modal = m; },
    ...extra,
  };
  return { i, log };
}

test('DB: the panel buttons, picker selects and modals drive the same actions', { skip: skipDb }, async () => {
  const { world, temp, owner } = await actionWorld(34);
  const stranger = world.addMember({ name: 'Stranger' });
  const guest = world.addMember({ name: 'Guest' });
  world.place(guest, temp.vc);
  const id = temp.vc.id;

  // lock by the owner: deferred ephemerally, then the result replaces the thinking state
  let { i, log } = fakeInteraction(world, owner, `voice:lock:${id}`);
  await tv.handleButton(i);
  assert.deepEqual(log.deferred, { ephemeral: true });
  assert.equal(log.edits[0].embeds[0].data.title, '✅  Channel Locked');
  // a stranger is refused
  ({ i, log } = fakeInteraction(world, stranger, `voice:unlock:${id}`));
  await tv.handleButton(i);
  assert.equal(log.edits[0].embeds[0].data.title, '❌  Not Your Channel');

  // rename and limit open a modal as the FIRST response (nothing is deferred)
  ({ i, log } = fakeInteraction(world, owner, `voice:rename:${id}`));
  await tv.handleButton(i);
  assert.equal(log.deferred, null);
  assert.equal(log.modal.toJSON().custom_id, `voice-modal:rename:${id}`);
  assert.equal(log.modal.toJSON().components[0].components[0].value, 'Temp');
  ({ i, log } = fakeInteraction(world, owner, `voice:limit:${id}`));
  await tv.handleButton(i);
  assert.equal(log.modal.toJSON().components[0].components[0].value, '0');
  // kick opens a picker
  ({ i, log } = fakeInteraction(world, owner, `voice:kick:${id}`));
  await tv.handleButton(i);
  assert.equal(log.replies[0].ephemeral, true);
  assert.equal(log.replies[0].components[0].toJSON().components[0].custom_id, `voice-select:kick:${id}`);
  // ... which a stranger cannot even open
  ({ i, log } = fakeInteraction(world, stranger, `voice:ban:${id}`));
  await tv.handleButton(i);
  assert.equal(log.modal, null);
  assert.equal(log.replies[0].embeds[0].data.title, '❌  Not Your Channel');
  // the picker's select
  ({ i, log } = fakeInteraction(world, owner, `voice-select:kick:${id}`, { values: [guest.id] }));
  await tv.handleSelect(i);
  assert.equal(log.edits[0].embeds[0].data.title, '✅  Member Kicked');
  assert.deepEqual(log.edits[0].components, []);
  // the modals
  ({ i, log } = fakeInteraction(world, owner, `voice-modal:limit:${id}`, { fields: { getTextInputValue: () => '7' } }));
  await tv.handleModal(i);
  assert.equal(log.edits[0].embeds[0].data.title, '✅  Limit Updated');
  assert.equal(temp.vc.userLimit, 7);
  ({ i, log } = fakeInteraction(world, owner, `voice-modal:rename:${id}`, { fields: { getTextInputValue: () => 'Renamed' } }));
  await tv.handleModal(i);
  assert.equal(log.edits[0].embeds[0].data.title, '✅  Channel Renamed');
  // a stale channel id
  ({ i, log } = fakeInteraction(world, owner, 'voice:lock:111111111111111111'));
  await tv.handleButton(i);
  assert.equal(log.edits[0].embeds[0].data.title, '❌  Channel Gone');
});

// ── Reconciliation ────────────────────────────────────────────────────────────

test('DB: reconcileGuild repairs rows after a restart', { skip: skipDb }, async () => {
  const world = makeWorld(G(35));
  const setup = await setupHub(world, { keepAliveMinutes: 5 });
  const guest = world.addMember({ name: 'Guest' });
  const tenMinAgo = new Date(Date.now() - 10 * 60_000);
  const owned = async (name, overrides = {}) => {
    const m = world.addMember({ name });
    const t = await setupTemp(world, setup, m, overrides);
    return { m, ...t };
  };

  const expired = await owned('E', { emptySince: tenMinAgo });
  world.place(expired.m, null);
  const fresh = await owned('F');
  world.place(fresh.m, null);
  const absentOwner = await owned('AO');
  world.place(absentOwner.m, null);
  world.place(guest, absentOwner.vc);
  const returned = await owned('R', { ownerLeftAt: new Date(), emptySince: new Date() });
  const gone = await owned('G');
  world.uncache(gone.vc.id);
  const unsure = await owned('U');
  world.uncache(unsure.vc.id);
  world.fail.fetchById = { [unsure.vc.id]: 50001 };

  await tv.reconcileGuild(world.guild);

  assert.equal(await getRow(expired.vc.id), null, 'empty past its keep-alive: deleted');
  assert.ok(world.calls.deleted.some((d) => d.id === expired.vc.id));
  const f = await getRow(fresh.vc.id);
  assert.ok(f.emptySince, 'empty within keep-alive: emptySince set');
  assert.ok(await getJob(fresh.vc.id), '... and the cleanup job (re)scheduled');
  assert.ok((await getRow(absentOwner.vc.id)).ownerLeftAt, 'occupied by a guest, owner absent: the lock clock starts');
  const r = await getRow(returned.vc.id);
  assert.equal(r.ownerLeftAt, null, 'the owner is back');
  assert.equal(r.emptySince, null);
  assert.equal(await getRow(gone.vc.id), null, 'confirmed gone (10003): purged');
  assert.ok(await getRow(unsure.vc.id), 'REST error: kept');

  // an unavailable guild is skipped entirely
  const w2 = makeWorld(G(36), { available: false });
  const s2 = await setupHub(w2);
  const t2 = await setupTemp(w2, s2, w2.addMember({ name: 'X' }));
  w2.uncache(t2.vc.id);
  await tv.reconcileGuild(w2.guild);
  assert.ok(await getRow(t2.vc.id));
  assert.equal(w2.fetchLog.length, 0);
});

test('DB: reconcileGuild deletes a hub only after a confirmed 10003, and serves members waiting in a hub', { skip: skipDb }, async () => {
  const world = makeWorld(G(37));
  const gone = await setupHub(world);
  const unsure = await setupHub(world);
  world.uncache(gone.hubChannel.id);
  world.uncache(unsure.hubChannel.id);
  world.fail.fetchById = { [unsure.hubChannel.id]: 50001 };
  const live = await setupHub(world);
  const waiting = world.addMember({ name: 'Waiting' });
  world.place(waiting, live.hubChannel);

  await tv.reconcileGuild(world.guild);
  assert.equal(await prisma.tempVoiceHub.count({ where: { id: gone.hub.id } }), 0, '10003 and no channels left: the hub is deleted');
  assert.equal(await prisma.tempVoiceHub.count({ where: { id: unsure.hub.id } }), 1, 'a REST error keeps the hub');
  assert.equal(world.calls.created.length, 1, 'the waiting member got a channel');
  assert.equal(world.calls.created[0].opts.name, "#1 - Waiting's Channel");
  assert.equal(waiting.voice.channelId, world.calls.created[0].channel.id);

  // a category that is gone (10003) is detached from its hub
  const w2 = makeWorld(G(38));
  const s2 = await setupHub(w2);
  await prisma.tempVoiceHub.update({ where: { id: s2.hub.id }, data: { categoryId: '777777777777777777' } });
  await tv.reconcileGuild(w2.guild);
  assert.equal((await prisma.tempVoiceHub.findUnique({ where: { id: s2.hub.id } })).categoryId, null);
});

// ── Hooks ─────────────────────────────────────────────────────────────────────

test('DB: the voiceStateUpdate hook creates a channel from a hub and deletes it when emptied', { skip: skipDb }, async () => {
  const world = makeWorld(G(39));
  const { hubChannel } = await setupHub(world);
  const bob = world.addMember({ name: 'Bob' });
  world.place(bob, hubChannel);
  const getSettings = async () => SETTINGS_ON;
  const ctx = (oldId, newId, change) => ({
    guild: world.guild, member: bob, oldState: { channelId: oldId }, newState: { channelId: newId }, change, getSettings,
  });
  assert.equal(tv.voiceFilter(ctx(null, hubChannel.id, 'join')), true, 'no memo yet');
  tv.onVoiceStateUpdate(ctx(null, hubChannel.id, 'join'));
  await until(() => world.calls.created.length === 1 && bob.voice.channelId === world.calls.created[0].channel.id);
  const temp = world.calls.created[0].channel;
  // the gateway reports the move into the new channel
  tv.onVoiceStateUpdate(ctx(hubChannel.id, temp.id, 'move'));
  await until(() => tv.peekIndex(world.guild.id));
  assert.equal(tv.voiceFilter(ctx(null, 'some-other-channel', 'join')), false, 'a fresh memo skips unrelated events');
  assert.equal(tv.voiceFilter(ctx(null, hubChannel.id, 'join')), true);
  assert.equal(tv.voiceFilter(ctx(temp.id, null, 'leave')), true);
  assert.equal(tv.voiceFilter(ctx(temp.id, temp.id, 'state')), false);
  // leaving with the module OFF still cleans up
  world.place(bob, null);
  tv.onVoiceStateUpdate({ ...ctx(temp.id, null, 'leave'), getSettings: async () => ({ tempVoiceEnabled: false }) });
  await until(() => world.calls.deleted.some((d) => d.id === temp.id));
  assert.equal(await getRow(temp.id), null);
});

test('DB: a hub join does nothing while the module is off', { skip: skipDb }, async () => {
  const world = makeWorld(G(40));
  const { hubChannel } = await setupHub(world);
  const bob = world.addMember({ name: 'Bob' });
  world.place(bob, hubChannel);
  tv.onVoiceStateUpdate({
    guild: world.guild, member: bob, oldState: { channelId: null }, newState: { channelId: hubChannel.id }, change: 'join',
    getSettings: async () => ({ tempVoiceEnabled: false }),
  });
  await until(() => tv.peekIndex(world.guild.id));
  await sleepMs(150);
  assert.equal(world.calls.created.length, 0);
  assert.equal(bob.voice.channelId, hubChannel.id);
});

test('DB: the channelDelete hook cleans rows, text channels, hubs and categories', { skip: skipDb }, async () => {
  const world = makeWorld(G(41));
  const setup = await setupHub(world);
  const bob = world.addMember({ name: 'Bob' });
  const b2 = world.addMember({ name: 'B2' });
  const text = world.addChannel({ name: 'text', type: ChannelType.GuildText });
  const a = await setupTemp(world, setup, bob, { textChannelId: text.id });
  const b = await setupTemp(world, setup, b2);

  // the temp channel is deleted by hand: its row and its text channel go
  world.uncache(a.vc.id);
  tv.onChannelDelete({ guild: world.guild, channel: { id: a.vc.id } });
  await until(async () => (await getRow(a.vc.id)) === null);
  assert.ok(world.calls.deleted.some((d) => d.id === text.id), 'the companion text channel was deleted');

  // a text channel deleted by hand is forgotten
  const text2 = world.addChannel({ name: 'text2', type: ChannelType.GuildText });
  await prisma.tempVoiceChannel.update({ where: { channelId: b.vc.id }, data: { textChannelId: text2.id, controlMessageId: '1' } });
  tv.invalidateIndex(world.guild.id);
  tv.onChannelDelete({ guild: world.guild, channel: { id: text2.id } });
  await until(async () => (await getRow(b.vc.id)).textChannelId === null);
  assert.equal((await getRow(b.vc.id)).controlMessageId, null);

  // the hub channel is deleted while a channel is open: the hub is retired (kept)
  tv.onChannelDelete({ guild: world.guild, channel: { id: setup.hubChannel.id } });
  await sleepMs(200);
  assert.equal(await prisma.tempVoiceHub.count({ where: { id: setup.hub.id } }), 1);
  // ... and disappears with its last channel
  world.uncache(setup.hubChannel.id);
  world.place(b2, null);
  await tv.onLeaveTemp(world.guild, b.vc.id, b2);
  assert.equal(await prisma.tempVoiceHub.count({ where: { id: setup.hub.id } }), 0);

  // an empty hub whose channel is deleted is deleted; a deleted category is detached
  const w2 = makeWorld(G(42));
  const s2 = await setupHub(w2);
  await prisma.tempVoiceHub.update({ where: { id: s2.hub.id }, data: { categoryId: s2.category.id } });
  tv.invalidateIndex(w2.guild.id);
  tv.onChannelDelete({ guild: w2.guild, channel: { id: s2.category.id } });
  await until(async () => (await prisma.tempVoiceHub.findUnique({ where: { id: s2.hub.id } })).categoryId === null);
  tv.onChannelDelete({ guild: w2.guild, channel: { id: s2.hubChannel.id } });
  await until(async () => (await prisma.tempVoiceHub.count({ where: { id: s2.hub.id } })) === 0);
});

// ── deleteHub and /voice clean ────────────────────────────────────────────────

test('DB: deleteHub removes the rows and channels and reports failures', { skip: skipDb }, async () => {
  const world = makeWorld(G(43));
  const setup = await setupHub(world);
  const text = world.addChannel({ name: 'text', type: ChannelType.GuildText });
  const a = await setupTemp(world, setup, world.addMember({ name: 'A' }), { textChannelId: text.id });
  const b = await setupTemp(world, setup, world.addMember({ name: 'B' }));
  const c = await setupTemp(world, setup, world.addMember({ name: 'C' }));
  world.uncache(c.vc.id); // already deleted by hand: counts as deleted
  world.fail.deleteById = { [b.vc.id]: 50013 };
  const actor = { tag: 'admin#0' };
  const r = await tv.deleteHub(world.guild, setup.hub, { deleteHubChannel: true, actor });
  assert.deepEqual(r, { deleted: 2, failed: 1, hubChannelFailed: false });
  assert.equal(await prisma.tempVoiceHub.count({ where: { id: setup.hub.id } }), 0);
  assert.equal(await prisma.tempVoiceChannel.count({ where: { hubId: setup.hub.id } }), 0, 'the cascade removed the rows');
  assert.ok(world.calls.deleted.some((d) => d.id === a.vc.id));
  assert.ok(world.calls.deleted.some((d) => d.id === text.id));
  assert.ok(world.calls.deleted.some((d) => d.id === setup.hubChannel.id), 'the hub channel was deleted too');
  assert.ok(world.calls.deleted[0].reason.includes('admin#0'));
  assert.equal((await tv.deleteHub(world.guild, setup.hub, {})).notFound, true, 'a second call finds nothing');

  const w2 = makeWorld(G(44));
  const s2 = await setupHub(w2);
  w2.fail.deleteById = { [s2.hubChannel.id]: 50013 };
  const r2 = await tv.deleteHub(w2.guild, s2.hub, { deleteHubChannel: true });
  assert.equal(r2.hubChannelFailed, true);
});

test('DB: /voice clean deletes only empty channels and only for moderators', { skip: skipDb }, async () => {
  const world = makeWorld(G(45));
  const setup = await setupHub(world, { moderatorRoleIds: ['rMod'] });
  const e1 = world.addMember({ name: 'E1' });
  const e2 = world.addMember({ name: 'E2' });
  const busy = world.addMember({ name: 'Busy' });
  const t1 = await setupTemp(world, setup, e1);
  const t2 = await setupTemp(world, setup, e2);
  const t3 = await setupTemp(world, setup, busy);
  world.place(e1, null);
  world.place(e2, null);
  const member = world.addMember({ name: 'Plain' });
  const denied = await tv.cleanEmptyChannels({ guild: world.guild, actor: member, settings: SETTINGS_ON });
  assert.equal(denied.data.title, '❌  Insufficient Permissions');
  const modByRole = world.addMember({ name: 'RoleMod', roles: ['rMod'] });
  const done = await tv.cleanEmptyChannels({ guild: world.guild, actor: modByRole, settings: SETTINGS_ON });
  assert.equal(done.data.title, '✅  Channels Cleaned');
  assert.match(done.data.description, /\*\*2\*\*/);
  assert.equal(await getRow(t1.vc.id), null);
  assert.equal(await getRow(t2.vc.id), null);
  assert.ok(await getRow(t3.vc.id), 'the occupied channel stays');
  const nothing = await tv.cleanEmptyChannels({ guild: world.guild, actor: world.addMember({ perms: P.BanMembers }), settings: SETTINGS_ON });
  assert.equal(nothing.data.title, 'ℹ️  Nothing To Clean');
});

// ── The command files ─────────────────────────────────────────────────────────

function fakeCommand(world, member, { sub, group = null, opts = {} }) {
  const log = { deferred: null, edits: [], followUps: [], replies: [] };
  const val = (name) => (Object.hasOwn(opts, name) ? opts[name] : null);
  const i = {
    guild: world.guild, guildId: world.guild.id, member, user: member.user, createdTimestamp: Date.now(),
    deferred: false, replied: false, ephemeral: null,
    options: {
      getSubcommand: () => sub,
      getSubcommandGroup: () => group,
      getChannel: val, getString: val, getInteger: val, getBoolean: val, getRole: val, getUser: val,
    },
    deferReply: async (o) => { i.deferred = true; i.ephemeral = Boolean(o?.ephemeral); log.deferred = o ?? {}; },
    editReply: async (p) => { log.edits.push(p); return {}; },
    followUp: async (p) => { log.followUps.push(p); return {}; },
    reply: async (p) => { i.replied = true; log.replies.push(p); return {}; },
  };
  return { i, log };
}
const lastEmbed = (log) => log.edits.at(-1).embeds[0].data;
const fakeRole = (id) => ({ id, toString: () => `<@&${id}>` });

test('DB: /voice-hub create makes the hub channel and the row, and reports problems', { skip: skipDb }, async () => {
  const world = makeWorld(G(46));
  await ensureGuildSettings(G(46));
  const admin = world.addMember({ name: 'Admin', perms: P.ManageGuild });

  // no channel given: the command creates the hub channel
  let { i, log } = fakeCommand(world, admin, { sub: 'create', opts: {} });
  await hubCmd.execute(i);
  assert.equal(log.deferred && typeof log.deferred, 'object', 'replies are public (deferReply without ephemeral)');
  assert.notEqual(log.deferred.ephemeral, true);
  assert.equal(lastEmbed(log).title, '✅  Hub Created');
  const created = world.calls.created.at(-1);
  assert.equal(created.opts.name, tv.HUB_CHANNEL_NAME);
  assert.equal(created.opts.type, ChannelType.GuildVoice);
  const hub = await tv.findHubByChannel(G(46), created.channel.id);
  assert.ok(hub);
  assert.equal(hub.nameFormat, DEFAULT_NAME_FORMAT);
  assert.equal(hub.keepAliveMinutes, 0);
  assert.equal(hub.bitrate, null);
  assert.match(lastEmbed(log).description, new RegExp(`Join <#${created.channel.id}> to get your own voice channel`));
  assert.match(lastEmbed(log).description, /\*\*Preview:\*\* #1 - Admin's Channel/);
  assert.doesNotMatch(lastEmbed(log).description, /module is off/);

  // an existing channel with every setting; the module is off, which adds a note
  await updateSettings(G(46), { tempVoiceEnabled: false });
  const category = world.addChannel({ name: 'Cat', type: ChannelType.GuildCategory });
  const channel = world.addChannel({ name: 'Join', parent: category });
  ({ i, log } = fakeCommand(world, admin, {
    sub: 'create',
    opts: {
      channel, category, name_format: '{user.name} room', limit: 4, bitrate: 128, keep_alive: 15, ownership_lock: 2,
      text_channel: true, text_private: false, sync_permissions: false,
    },
  }));
  await hubCmd.execute(i);
  assert.equal(lastEmbed(log).title, '✅  Hub Created');
  assert.match(lastEmbed(log).description, /The Temporary Channels module is off/);
  assert.match(lastEmbed(log).description, /128 kbps/);
  assert.match(lastEmbed(log).description, /This server currently allows up to 96 kbps/);
  const full = await tv.findHubByChannel(G(46), channel.id);
  assert.deepEqual(
    [full.categoryId, full.nameFormat, full.userLimit, full.bitrate, full.keepAliveMinutes, full.ownershipLockMinutes,
      full.createTextChannel, full.textChannelMembersOnly, full.syncPermissions],
    [category.id, '{user.name} room', 4, 128000, 15, 2, true, false, false],
  );
  await updateSettings(G(46), { tempVoiceEnabled: true });

  // errors
  ({ i, log } = fakeCommand(world, admin, { sub: 'create', opts: { channel } }));
  await hubCmd.execute(i);
  assert.equal(lastEmbed(log).title, '❌  Already A Hub');
  const temp = await setupTemp(world, { hub: full, category }, world.addMember({ name: 'Own' }));
  ({ i, log } = fakeCommand(world, admin, { sub: 'create', opts: { channel: temp.vc } }));
  await hubCmd.execute(i);
  assert.equal(lastEmbed(log).title, '❌  Temporary Channel');
  ({ i, log } = fakeCommand(world, admin, { sub: 'create', opts: { name_format: '{user}' } }));
  await hubCmd.execute(i);
  assert.equal(lastEmbed(log).title, '❌  Invalid Name Format');
  assert.match(lastEmbed(log).description, /mentions/);
  ({ i, log } = fakeCommand(world, admin, { sub: 'create', opts: { name_format: '{nope} room' } }));
  await hubCmd.execute(i);
  assert.equal(lastEmbed(log).title, '✅  Hub Created');
  assert.match(lastEmbed(log).description, /⚠️ Unknown placeholders are kept as typed: `\{nope\}`/);

  // the hub cap: 3 hubs exist, fill up to the limit
  for (let n = await tv.countHubs(G(46)); n < LIMITS.TEMPVOICE_HUBS; n++) await tv.createHub(G(46), { channelId: `cap${n}` });
  const createdBefore = world.calls.created.length;
  ({ i, log } = fakeCommand(world, admin, { sub: 'create', opts: {} }));
  await hubCmd.execute(i);
  assert.equal(lastEmbed(log).title, '❌  Limit Reached');
  assert.equal(world.calls.created.length, createdBefore, 'no channel is created once the cap is reached');

  // the bot lacks permissions
  const w2 = makeWorld(G(47), { botPerms: P.ViewChannel });
  await ensureGuildSettings(G(47));
  const a2 = w2.addMember({ name: 'Admin' });
  ({ i, log } = fakeCommand(w2, a2, { sub: 'create', opts: {} }));
  await hubCmd.execute(i);
  assert.equal(lastEmbed(log).title, '❌  Missing Permissions');
  assert.match(lastEmbed(log).description, /Manage Channels, Manage Roles, Move Members/);
  assert.equal(w2.calls.created.length, 0);

  // Discord refuses the hub channel: nothing is saved
  const w3 = makeWorld(G(48));
  await ensureGuildSettings(G(48));
  w3.fail.create = { code: 30013 };
  ({ i, log } = fakeCommand(w3, w3.addMember({ name: 'Admin' }), { sub: 'create', opts: {} }));
  await hubCmd.execute(i);
  assert.equal(lastEmbed(log).title, '❌  Could Not Create Channel');
  assert.match(lastEmbed(log).description, /the server has 500 channels/);
  assert.equal(await tv.countHubs(G(48)), 0);
});

test('DB: /voice-hub edit, list and the role groups', { skip: skipDb }, async () => {
  const world = makeWorld(G(49));
  const setup = await setupHub(world);
  const admin = world.addMember({ name: 'Admin', perms: P.ManageGuild });
  const hubOpt = setup.hubChannel;
  const run = async (sub, opts, group = null) => {
    const { i, log } = fakeCommand(world, admin, { sub, group, opts });
    await hubCmd.execute(i);
    return lastEmbed(log);
  };

  assert.equal((await run('edit', { hub: hubOpt })).title, '❌  Nothing To Change');
  assert.equal((await run('edit', { hub: world.addChannel({ name: 'x' }), limit: 3 })).title, '❌  Not A Hub');
  const edited = await run('edit', { hub: hubOpt, limit: 5, keep_alive: 10, name_format: '{username} room', ownership_lock: 3, bitrate: 96, text_channel: true });
  assert.equal(edited.title, '✅  Hub Updated');
  for (const line of [
    '• **Limit:** none → 5', '• **Keep alive:** delete at once → 10 min', '• **Ownership lock:** none → 3 min',
    '• **Bitrate:** default → 96 kbps', '• **Text channel:** off → private',
  ]) assert.ok(edited.description.includes(line), line);
  assert.match(edited.description, /• \*\*Name format:\*\* `#\{index\} - \{username\}'s Channel` → `\{username\} room`/);
  const row = await prisma.tempVoiceHub.findUnique({ where: { id: setup.hub.id } });
  assert.deepEqual([row.userLimit, row.keepAliveMinutes, row.ownershipLockMinutes, row.bitrate, row.createTextChannel], [5, 10, 3, 96000, true]);
  assert.equal((await run('edit', { hub: hubOpt, limit: 5 })).title, '⚠️  No Change');
  assert.equal((await run('edit', { hub: hubOpt, name_format: '{user}' })).title, '❌  Invalid Name Format');
  const cat2 = world.addChannel({ name: 'Cat2', type: ChannelType.GuildCategory });
  const moved = await run('edit', { hub: hubOpt, category: cat2, sync_permissions: false, text_private: false });
  assert.match(moved.description, new RegExp(`\\*\\*Category:\\*\\* hub's category → <#${cat2.id}>`));
  assert.match(moved.description, /\*\*Permissions:\*\* category → hub channel/);
  assert.match(moved.description, /\*\*Text channel:\*\* private → public/);

  // list
  const listed = await run('list', {});
  assert.equal(listed.title, 'ℹ️  Temporary Channel Hubs');
  assert.equal(listed.description, `1/${LIMITS.TEMPVOICE_HUBS} hubs`);
  assert.equal(listed.fields.length, 1);
  assert.equal(listed.fields[0].name, '🔊 #Join to Create');
  assert.match(listed.fields[0].value, /Active channels: 0/);
  assert.match(listed.fields[0].value, /Keep alive: 10 min/);
  const w0 = makeWorld(G(50));
  await ensureGuildSettings(G(50));
  const { i: i0, log: log0 } = fakeCommand(w0, w0.addMember({ name: 'A' }), { sub: 'list', opts: {} });
  await hubCmd.execute(i0);
  assert.match(lastEmbed(log0).description, /No hubs yet/);

  // roles
  const role = fakeRole('123456789012345001');
  assert.equal((await run('add', { hub: hubOpt, role }, 'ignore-role')).title, '✅  Ignored Role Added');
  assert.equal((await run('add', { hub: hubOpt, role }, 'ignore-role')).title, '⚠️  No Change');
  assert.match((await run('add', { hub: hubOpt, role }, 'ignore-role')).description, /is already an ignored role/);
  assert.equal((await run('remove', { hub: hubOpt, role }, 'ignore-role')).title, '✅  Ignored Role Removed');
  assert.match((await run('remove', { hub: hubOpt, role }, 'ignore-role')).description, /is not an ignored role/);
  assert.equal((await run('add', { hub: hubOpt, role }, 'mod-role')).title, '✅  Moderator Role Added');
  assert.deepEqual((await prisma.tempVoiceHub.findUnique({ where: { id: setup.hub.id } })).moderatorRoleIds, [role.id]);
  assert.equal((await run('remove', { hub: hubOpt, role }, 'mod-role')).title, '✅  Moderator Role Removed');
  assert.equal((await run('add', { hub: hubOpt, role: fakeRole(world.guild.id) }, 'mod-role')).title, '❌  Invalid Role');
  assert.equal((await run('add', { hub: world.addChannel({ name: 'y' }), role }, 'mod-role')).title, '❌  Not A Hub');
  for (let n = 0; n < MAX_HUB_ROLE_LIST; n++) await run('add', { hub: hubOpt, role: fakeRole(`12345678901234${String(n).padStart(4, '0')}`) }, 'mod-role');
  const over = await run('add', { hub: hubOpt, role: fakeRole('123456789012349999') }, 'mod-role');
  assert.equal(over.title, '❌  Limit Reached');
  assert.match(over.description, /at most 10 moderator roles/);
});

test('DB: /voice-hub delete asks for confirmation first', { skip: skipDb }, async () => {
  const world = makeWorld(G(51));
  const setup = await setupHub(world);
  const admin = world.addMember({ name: 'Admin', perms: P.ManageGuild });
  const stranger = world.addChannel({ name: 'z' });
  let { i, log } = fakeCommand(world, admin, { sub: 'delete', opts: { hub: stranger } });
  await hubCmd.execute(i);
  assert.deepEqual(log.deferred, { ephemeral: true }, 'delete defers ephemerally');
  assert.equal(lastEmbed(log).title, '❌  Not A Hub');
  // the fake interaction cannot show buttons, so the confirmation fails closed: nothing is deleted
  ({ i, log } = fakeCommand(world, admin, { sub: 'delete', opts: { hub: setup.hubChannel, delete_channel: true } }));
  await hubCmd.execute(i);
  assert.equal(await prisma.tempVoiceHub.count({ where: { id: setup.hub.id } }), 1);
  assert.equal(world.calls.deleted.length, 0);
});

test('DB: /voice acts on the channel the member is connected to', { skip: skipDb }, async () => {
  const { world, temp, owner } = await actionWorld(52);
  const run = async (member, sub, opts = {}) => {
    const { i, log } = fakeCommand(world, member, { sub, opts });
    await voiceCmd.execute(i);
    assert.deepEqual(log.deferred, { ephemeral: true });
    return lastEmbed(log);
  };
  assert.equal((await run(owner, 'lock')).title, '✅  Channel Locked');
  const stranger = world.addMember({ name: 'Stranger' });
  assert.equal((await run(stranger, 'lock')).title, '❌  Not In A Temporary Channel', 'not connected anywhere');
  const target = world.addMember({ name: 'Target' });
  world.place(target, temp.vc);
  assert.equal((await run(owner, 'ban', { member: { id: target.id } })).title, '✅  Member Banned');
  assert.equal((await run(owner, 'limit', { member_limit: 3 })).title, '✅  Limit Updated');
  assert.equal((await run(owner, 'rename', { name: 'New name' })).title, '✅  Channel Renamed');
  const plain = world.addChannel({ name: 'General' });
  world.place(stranger, plain);
  assert.equal((await run(stranger, 'unlock')).title, '❌  Not In A Temporary Channel', 'connected to an ordinary channel');
  assert.equal((await run(stranger, 'clean')).title, '❌  Insufficient Permissions');
  const mod = world.addMember({ name: 'Mod', perms: P.BanMembers });
  assert.equal((await run(mod, 'clean')).title, 'ℹ️  Nothing To Clean');
});

// ── Companion text channel ────────────────────────────────────────────────────

test('DB: the companion text channel follows the voice channel members (debounced) and heals a deleted text channel', { skip: skipDb }, async () => {
  const world = makeWorld(G(53));
  const setup = await setupHub(world, { createTextChannel: true, textChannelMembersOnly: true, ignoredRoleIds: ['rI'] });
  world.guild.roles.cache.set('rI', { id: 'rI' });
  const owner = world.addMember({ name: 'Owner' });
  const guest = world.addMember({ name: 'Guest' });
  const text = world.addChannel({ name: 'text', type: ChannelType.GuildText, parent: setup.category });
  const temp = await setupTemp(world, setup, owner, { textChannelId: text.id });
  const textSets = () => world.calls.overwrites.filter((o) => o.id === text.id);

  world.place(guest, temp.vc);
  await tv.onJoinTemp(world.guild, temp.vc.id, guest);
  assert.equal(textSets().length, 0, 'the sync is debounced');
  await until(() => textSets().length === 1, 5000);
  const by = Object.fromEntries(textSets()[0].list.map((o) => [o.id, o]));
  assert.equal(by[world.guild.id].deny, P.ViewChannel);
  assert.equal(by[owner.id].allow, P.ViewChannel);
  assert.equal(by[guest.id].allow, P.ViewChannel);
  assert.equal(by.rI.allow, P.ViewChannel, 'ignored roles keep access');
  assert.equal(by[BOT_ID].allow, BOT_TEXT_PERMS);

  // a burst of requests coalesces into one PATCH
  tv.requestTextSync(world.guild, temp.vc.id);
  tv.requestTextSync(world.guild, temp.vc.id);
  tv.requestTextSync(world.guild, temp.vc.id);
  await until(() => textSets().length === 2, 5000);
  await sleepMs(300);
  assert.equal(textSets().length, 2);

  // the text channel was deleted and REST confirms it: the row forgets it
  world.uncache(text.id);
  tv.requestTextSync(world.guild, temp.vc.id);
  await until(async () => (await getRow(temp.vc.id)).textChannelId === null, 5000);
});

test('DB: a banned user is hidden from a public companion text channel', { skip: skipDb }, async () => {
  const world = makeWorld(G(54));
  const setup = await setupHub(world, { createTextChannel: true, textChannelMembersOnly: false });
  const owner = world.addMember({ name: 'Owner' });
  const bad = world.addMember({ name: 'Bad' });
  const text = world.addChannel({ name: 'text', type: ChannelType.GuildText, parent: setup.category });
  const temp = await setupTemp(world, setup, owner, { textChannelId: text.id });
  const res = await tv.runVoiceAction({
    guild: world.guild, actor: owner, channelId: temp.vc.id, action: 'ban', targetId: bad.id, settings: SETTINGS_ON, source: 'command',
  });
  assert.equal(res.embed.data.title, '✅  Member Banned');
  await until(() => world.calls.overwrites.some((o) => o.id === text.id), 5000);
  const list = world.calls.overwrites.find((o) => o.id === text.id).list;
  assert.equal(list.find((o) => o.id === bad.id).deny, P.ViewChannel);
  assert.equal(list.find((o) => o.id === world.guild.id), undefined, 'a public channel keeps @everyone untouched');
});
