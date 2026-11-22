// Reference data for feat-logging, produced by the JS bot's own code (no servers needed):
//   node rust/fixtures/gen/feat-logging-golden.mjs
// writes rust/fixtures/golden/feat-logging/cases.json:
//   embeds:    { name, fn, args, out } for the logEvents.js builders and diffs, the args in the plain view shape the Rust
//              views deserialize (camelCase); out is embed.toJSON() (or the diff result).
//   snapshots: { name, fn, args, out } for the inviteTracker.js snapshot functions.
//   keys:      cache keys the JS builds (the invite snapshot, the timeout dedupe key, the /invites link cooldown).
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
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};

const ev = await imp('src', 'services', 'logEvents.js');
const svc = await imp('src', 'services', 'loggingService.js');
const inv = await imp('src', 'services', 'inviteTracker.js');
const msnap = await imp('src', 'services', 'messageSnapshot.js');
const { plugin } = await imp('src', 'plugins', 'logging.js');
const { cache, default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const invitesCmd = await imp('src', 'commands', 'utility', 'invites.js');

const NOW = Date.UTC(2026, 4, 1, 12, 0, 0);
const MIN = 60_000;
const HOUR = 3_600_000;
const DAY = 86_400_000;

// ── Plain views -> the discord.js-like fakes the builders read ──────────────────
const U = (o) => (o == null ? o : {
  id: o.id, username: o.username, tag: o.tag, globalName: o.globalName ?? null, bot: Boolean(o.bot),
  createdTimestamp: o.createdTimestamp, createdAt: o.createdTimestamp == null ? undefined : new Date(o.createdTimestamp),
  displayAvatarURL: (opts) => (opts?.size ? o.avatarUrl256 : o.avatarUrl) ?? null,
});
const guildOfKnown = (known, names) => (known ? { channels: { cache: new Map(known.map((id) => [id, names?.[id] == null ? {} : { name: names[id] }])) } } : null);
const M = (o, known) => (o == null ? o : {
  id: o.id, partial: Boolean(o.partial), content: o.content ?? null, author: o.author ? U(o.author) : null,
  messageSnapshots: (o.snapshotContents ?? []).map((content) => ({ content })),
  channelId: o.channelId, webhookId: o.webhookId ?? null, guild: guildOfKnown(known),
  createdTimestamp: o.createdTimestamp, createdAt: o.createdTimestamp == null ? undefined : new Date(o.createdTimestamp),
  editedTimestamp: o.editedTimestamp ?? null,
  attachments: new Map((o.attachments ?? []).map((a, i) => [String(i), a])), embeds: Array.from({ length: o.embedCount ?? 0 }, () => ({})),
  url: o.url,
  mentions: { users: new Map((o.mentionUsers ?? []).map((u) => [u.id, U(u)])), roles: new Map((o.mentionRoles ?? []).map((r) => [r.id, r])), everyone: Boolean(o.mentionEveryone) },
});
const MEM = (o, memberCount) => ({
  id: o.id, user: o.user ? U(o.user) : undefined, guild: { id: o.guildId, memberCount }, nickname: o.nickname ?? null, avatar: o.avatar ?? null,
  partial: Boolean(o.partial), joinedTimestamp: o.joinedTimestamp ?? null,
  roles: { cache: new Map((o.roles ?? []).map((r) => [r.id, { id: r.id, position: r.position ?? 0 }])) },
  avatarURL: () => o.avatarUrl256 ?? null,
  communicationDisabledUntilTimestamp: o.timeoutUntil ?? null,
  communicationDisabledUntil: o.timeoutUntil == null ? null : new Date(o.timeoutUntil),
  isCommunicationDisabled() { return o.timeoutUntil != null && o.timeoutUntil > NOW; },
});
const CH = (o) => {
  const c = {
    id: o.id, name: o.name, type: o.type ?? 0, topic: o.topic ?? null, nsfw: Boolean(o.nsfw), rateLimitPerUser: o.rateLimitPerUser ?? 0,
    parentId: o.parentId ?? null, parent: o.parentName ? { name: o.parentName } : null,
    permissionOverwrites: { cache: new Map((o.overwrites ?? []).map((w) => [w.id, { id: w.id, type: w.type, allow: BigInt(w.allow), deny: BigInt(w.deny) }])) },
  };
  if (o.bitrate !== undefined) c.bitrate = o.bitrate;
  if (o.userLimit !== undefined) c.userLimit = o.userLimit;
  if ('rtcRegion' in o) c.rtcRegion = o.rtcRegion;
  return c;
};
const TH = (o, known) => {
  const t = { id: o.id, type: o.type, parentId: o.parentId ?? null, ownerId: o.ownerId ?? null, guild: guildOfKnown(known) };
  for (const k of ['name', 'autoArchiveDuration', 'rateLimitPerUser', 'locked', 'invitable', 'appliedTags']) if (o[k] !== undefined && o[k] !== null) t[k] = o[k];
  if (o.availableTags) t.parent = { availableTags: o.availableTags };
  return t;
};
const R = (o) => ({ ...o, permissions: BigInt(o.permissions ?? 0), createdAt: o.createdTimestamp == null ? undefined : new Date(o.createdTimestamp) });
const G = (o) => ({ ...o, iconURL: () => o.iconUrl ?? null });
const E = (o) => ({ ...o, roles: { cache: new Map((o.roles ?? []).map((r) => [r, {}])) }, imageURL: () => o.imageUrl ?? null, toString: () => o.mention });
const VS = (o) => (o == null ? o : { channelId: o.channelId ?? null, serverMute: o.serverMute, serverDeaf: o.serverDeaf, selfDeaf: o.selfDeaf, selfMute: o.selfMute, streaming: o.streaming });
const INV = (o) => ({ code: o.code, channel: o.channelId ? { id: o.channelId } : null, inviter: o.inviter ? U(o.inviter) : null, maxUses: o.maxUses ?? 0, expiresAt: o.expiresTimestamp ? new Date(o.expiresTimestamp) : null, temporary: Boolean(o.temporary) });
const json = (e) => (e == null ? null : (typeof e.toJSON === 'function' ? JSON.parse(JSON.stringify(e.toJSON())) : JSON.parse(JSON.stringify(e))));

// ── Inputs ────────────────────────────────────────────────────────────────────
const alice = { id: '111111111111111111', username: 'alice', tag: 'alice', globalName: 'Alice', createdTimestamp: NOW - 400 * DAY, avatarUrl: 'https://cdn.discordapp.com/avatars/111111111111111111/a.webp', avatarUrl256: 'https://cdn.discordapp.com/avatars/111111111111111111/a.webp?size=256' };
const bob = { id: '222222222222222222', username: 'bob_`x`', tag: 'bob_`x`', createdTimestamp: NOW - 2 * DAY, avatarUrl: 'https://cdn.discordapp.com/embed/avatars/1.png', avatarUrl256: 'https://cdn.discordapp.com/embed/avatars/1.png' };
const robot = { id: '333333333333333333', username: 'robot', tag: 'robot#0420', bot: true, createdTimestamp: NOW - DAY, avatarUrl: 'https://cdn.discordapp.com/avatars/333333333333333333/a_x.gif' };
const KNOWN = ['700000000000000001', '700000000000000002'];
const msg = (o) => ({ id: '800000000000000001', content: 'hello', author: alice, channelId: '700000000000000001', createdTimestamp: NOW - 5 * MIN, url: 'https://discord.com/channels/9/700000000000000001/800000000000000001', ...o });
const member = (o) => ({ id: bob.id, user: bob, guildId: '955007000000000001', roles: [{ id: '955007000000000001', position: 0 }, { id: '444444444444444441', position: 3 }, { id: '444444444444444442', position: 7 }], joinedTimestamp: NOW - 3 * DAY - 2 * HOUR, ...o });
const chan = (o) => ({ id: '700000000000000001', name: 'general', type: 0, topic: null, nsfw: false, rateLimitPerUser: 0, parentId: null, overwrites: [], ...o });
const thread = (o) => ({ id: '700000000000000777', name: 'bug `reports`', type: 11, parentId: '700000000000000001', ownerId: bob.id, autoArchiveDuration: 1440, rateLimitPerUser: 0, locked: false, invitable: null, appliedTags: [], ...o });
const role = (o) => ({ id: '444444444444444444', name: 'Mods', hexColor: '#000000', hoist: false, mentionable: false, icon: null, unicodeEmoji: null, managed: false, permissions: '0', createdTimestamp: NOW - 100 * DAY, ...o });
const guild = (o) => ({ id: '955007000000000001', name: 'Guild', icon: null, banner: null, splash: null, description: null, ownerId: '1', verificationLevel: 0, explicitContentFilter: 0, defaultMessageNotifications: 0, afkChannelId: null, afkTimeout: 300, systemChannelId: null, rulesChannelId: null, publicUpdatesChannelId: null, vanityURLCode: null, preferredLocale: 'en-US', premiumTier: 0, mfaLevel: 0, nsfwLevel: 0, premiumProgressBarEnabled: false, ...o });
const emoji = (o) => ({ id: '1234', name: 'party', animated: false, roles: [], imageUrl: 'https://cdn.discordapp.com/emojis/1234.webp', mention: '<:party:1234>', ...o });

const embeds = [];
const add = (name, fn, args, out) => embeds.push({ name, fn, args, out });

const del = (name, a) => add(name, 'messageDelete', a, json(ev.buildMessageDeleteEmbed({ message: M(a.message, a.known), botReason: a.botReason ?? null, botId: a.botId ?? null, now: NOW })));
del('delete plain with attachments', { message: msg({ content: 'see `this`', attachments: [{ name: 'cat.png', size: 2_500_000, url: 'u1' }, { name: 'a.txt', size: 1280, url: 'u2' }, { name: 'b.bin', size: 100 }] }), known: KNOWN });
del('delete ghost ping', { message: msg({ content: 'hey', mentionUsers: [alice, bob, robot], mentionRoles: [{ id: '444444444444444444', name: 'Mods' }], mentionEveryone: true }) });
del('delete partial', { message: { id: '8', partial: true, channelId: '700000000000000009' }, known: KNOWN });
// discord.js reads createdTimestamp from the snowflake, so a partial message still says when it was sent.
del('delete partial with sent time', { message: { id: '1573137071177211955', partial: true, channelId: '700000000000000009', createdTimestamp: Number((1573137071177211955n >> 22n) + 1420070400000n) }, known: KNOWN });
del('delete by the bot', { message: msg({ mentionRoles: [{ id: '444444444444444444', name: 'Mods' }] }), botReason: 'automod', botId: '999' });
del('delete by the bot without id', { message: msg({}), botReason: 'r'.repeat(150) });
del('delete embeds only, old', { message: msg({ content: '', embedCount: 2, createdTimestamp: NOW - 2 * HOUR }) });
del('delete forward', { message: msg({ content: '', snapshotContents: ['forwarded text', 'second'] }) });
del('delete huge', { message: msg({ content: 'x'.repeat(5000), attachments: Array.from({ length: 60 }, (_, i) => ({ name: `file-${i}-${'n'.repeat(80)}.png`, size: i * 1000 })) }) });

const edit = (name, a) => add(name, 'messageEdit', a, json(ev.buildMessageEditEmbed({ oldMessage: M(a.oldMessage, a.known), newMessage: M(a.newMessage, a.known), oldKnown: a.oldKnown, now: NOW })));
edit('edit not cached', { oldMessage: { id: '1', partial: true }, newMessage: msg({ content: 'after', editedTimestamp: NOW - 1000 }), oldKnown: false, known: KNOWN });
edit('edit ghost', { oldMessage: msg({ content: 'hey <@222>', mentionUsers: [bob] }), newMessage: msg({ content: 'hey', editedTimestamp: NOW }), oldKnown: true });
edit('edit empty before', { oldMessage: msg({ content: '' }), newMessage: msg({ content: 'b'.repeat(2000), editedTimestamp: NOW, channelId: '700000000000000005' }), oldKnown: true, known: KNOWN });
edit('edit old message', { oldMessage: msg({ content: 'a', mentionUsers: [bob], createdTimestamp: NOW - 2 * HOUR }), newMessage: msg({ content: 'b', createdTimestamp: NOW - 2 * HOUR }), oldKnown: true });

const bulk = (name, a) => {
  const { embed, file } = ev.buildBulkDeleteEmbed({ channel: a.channel ? { id: a.channel.id, name: a.channel.name, guild: guildOfKnown(a.known) } : null, messages: new Map(a.messages.map((m) => [m.id, M(m)])), botReason: a.botReason ?? null, botId: a.botId ?? null });
  add(name, 'bulkDelete', a, { embed: json(embed), file: file ? { name: file.name.replace(/-\d+\.txt$/, '-N.txt'), text: file.attachment.toString('utf8') } : null });
};
const t0 = Date.UTC(2026, 0, 1, 12, 0, 0);
bulk('bulk with transcript', {
  channel: { id: '700000000000000001', name: 'general' }, known: KNOWN, botReason: '/purge by mod (3)', botId: '999',
  messages: [
    msg({ id: '3', content: 'third\nline', createdTimestamp: t0 + 2000, attachments: [{ url: 'https://cdn/x.png' }] }),
    { id: 'p', partial: true },
    msg({ id: '1', author: bob, content: 'first', createdTimestamp: t0 }),
    msg({ id: '2', author: robot, content: 'second\r\nrow', createdTimestamp: t0 + 1000 }),
    msg({ id: '4', content: '', snapshotContents: ['fwd'], createdTimestamp: t0 + 500 }),
  ],
});
bulk('bulk nothing cached', { channel: { id: '7' }, messages: [{ id: '1', partial: true }] });

// A snapshot JS stores (buildSnapshot) and the delete embed JS renders from it (snapshotToMessage): Rust must write the
// same JSON and render the same embed from it.
const snapCase = (name, a) => {
  const snapshot = JSON.parse(JSON.stringify(msnap.buildSnapshot(M(a.message, a.known))));
  add(name, 'messageSnapshot', a, { snapshot, embed: json(ev.buildMessageDeleteEmbed({ message: msnap.snapshotToMessage(snapshot, guildOfKnown(a.known)), botReason: null, botId: null, now: NOW })) });
};
snapCase('snapshot plain with attachments', { message: msg({ content: 'see `this`', attachments: [{ name: 'cat.png', size: 2_500_000, url: 'u1' }, { name: 'a.txt', size: 1280, url: 'u2' }, { name: 'b.bin', size: 100 }, { size: 5, url: 'u4' }, { name: 'long.bin', size: 1, url: `https://x/${'u'.repeat(1000)}` }] }), known: KNOWN });
snapCase('snapshot forward and embeds', { message: msg({ content: 'top', snapshotContents: ['forwarded text', 'second'], embedCount: 2 }) });
snapCase('snapshot huge', { message: msg({ content: 'x'.repeat(5000), attachments: Array.from({ length: 14 }, (_, i) => ({ name: `file-${i}-${'n'.repeat(120)}.png`, size: i * 1000, url: `u${i}` })) }) });
snapCase('snapshot embeds only, no tag', { message: msg({ content: '', embedCount: 1, author: { ...alice, tag: undefined, globalName: null, avatarUrl: undefined }, createdTimestamp: undefined }) });
snapCase('snapshot bot author', { message: msg({ author: robot }) });

add('invite posted', 'invitePosted', { message: msg({ content: 'join discord.gg/Mine and https://discord.com/invite/other' }), codes: ['Mine', 'other'], vanityCode: 'mine' },
  json(ev.buildInvitePostedEmbed({ message: M(msg({ content: 'join discord.gg/Mine and https://discord.com/invite/other' })), codes: ['Mine', 'other'], vanityCode: 'mine' })));
add('find invite codes', 'findInviteCodes', { content: `join discord.gg/abc and https://discord.com/invite/Xy-9 and discordapp.com/invite/abc http://www.discord.gg/a discord.gg/${'z'.repeat(40)} ${Array.from({ length: 12 }, (_, i) => `discord.gg/code${i}`).join(' ')}` },
  ev.findInviteCodes(`join discord.gg/abc and https://discord.com/invite/Xy-9 and discordapp.com/invite/abc http://www.discord.gg/a discord.gg/${'z'.repeat(40)} ${Array.from({ length: 12 }, (_, i) => `discord.gg/code${i}`).join(' ')}`));

const joinCase = (name, a) => add(name, 'memberJoin', a, json(ev.buildMemberJoinEmbed({ member: MEM(a.member, a.memberCount), ...(a.inviteState === 'unset' ? {} : { invite: a.invite }), now: NOW })));
joinCase('join unset', { member: member({ user: alice }), memberCount: 1234, inviteState: 'unset' });
joinCase('join unknown', { member: member({ user: alice }), memberCount: 1234, invite: null });
joinCase('join vanity', { member: member({ user: alice }), memberCount: 5, invite: { code: 'x', inviterId: null, vanity: true } });
joinCase('join code by', { member: member({ user: alice }), memberCount: 5, invite: { code: 'abc', inviterId: '999999999999999999', vanity: false } });
joinCase('join code', { member: member({ user: alice }), memberCount: 5, invite: { code: 'abc', inviterId: null, vanity: false } });
joinCase('join no code', { member: member({ user: alice }), memberCount: 5, invite: { code: null, inviterId: null, vanity: false } });
joinCase('join new account', { member: member({}), memberCount: 1234567, inviteState: 'unset' });
joinCase('join bot', { member: member({ id: robot.id, user: robot }), inviteState: 'unset' });

add('leave full', 'memberLeave', { member: member({}) }, json(ev.buildMemberLeaveEmbed({ member: MEM(member({})), now: NOW })));
add('leave partial', 'memberLeave', { member: member({ partial: true, joinedTimestamp: null }) }, json(ev.buildMemberLeaveEmbed({ member: MEM(member({ partial: true, joinedTimestamp: null })), now: NOW })));
for (const ms of [30_000, 5 * MIN, 3 * HOUR + 4 * MIN, 2 * DAY + 5 * HOUR]) add(`time in server ${ms}`, 'timeInServer', { ms }, ev.formatTimeInServer(ms));

const diffMember = (name, a) => {
  const d = ev.diffMemberUpdate({ oldMember: a.oldMember ? MEM(a.oldMember) : null, newMember: MEM(a.newMember), oldKnown: a.oldKnown });
  const t = d.timeout && { kind: d.timeout.kind, until: d.timeout.until ? d.timeout.until.getTime() : null, from: d.timeout.from ? d.timeout.from.getTime() : null };
  add(name, 'diffMember', a, { ...d, timeout: t });
};
const F = NOW + 5 * HOUR;
diffMember('diff member partial', { oldMember: member({ partial: true, roles: [] }), newMember: member({ nickname: 'x', avatar: 'h' }), oldKnown: false });
diffMember('diff member partial active timeout', { oldMember: member({ partial: true }), newMember: member({ timeoutUntil: F }), oldKnown: false });
diffMember('diff member known', { oldMember: member({ nickname: 'Old', roles: [{ id: '955007000000000001' }, { id: 'r1' }, { id: 'r2' }] }), newMember: member({ nickname: null, avatar: 'abc', avatarUrl256: 'https://cdn.discordapp.com/guilds/1/users/2/avatars/abc.webp?size=256', roles: [{ id: '955007000000000001' }, { id: 'r2' }, { id: 'r3' }, { id: 'r4' }] }), oldKnown: true });
diffMember('diff member timeout added over stale', { oldMember: member({ timeoutUntil: NOW - HOUR }), newMember: member({ timeoutUntil: F }), oldKnown: true });
diffMember('diff member timeout removed', { oldMember: member({ timeoutUntil: F }), newMember: member({}), oldKnown: true });
diffMember('diff member timeout changed', { oldMember: member({ timeoutUntil: F }), newMember: member({ timeoutUntil: F + HOUR }), oldKnown: true });
diffMember('diff member stale cleared', { oldMember: member({ timeoutUntil: NOW - HOUR }), newMember: member({}), oldKnown: true });

const ids60 = Array.from({ length: 60 }, (_, i) => `${100000000000000000n + BigInt(i)}`);
add('roles embed', 'memberRoles', { member: member({}), roles: { added: ids60, removed: ['r9'] } }, json(ev.buildMemberRolesEmbed({ member: MEM(member({})), roles: { added: ids60, removed: ['r9'] } })));
add('nickname embed', 'nickname', { member: member({}), nickname: { before: null, after: 'Ally' } }, json(ev.buildNicknameEmbed({ member: MEM(member({})), nickname: { before: null, after: 'Ally' } })));
for (const [kind, extra] of [['added', { until: F }], ['removed', {}], ['changed', { from: F, until: F + HOUR }], ['active-unknown', { until: F }], ['added', { until: null }]]) {
  const timeout = { kind, ...extra };
  const fake = { kind, until: extra.until == null ? extra.until : new Date(extra.until), from: extra.from == null ? extra.from : new Date(extra.from) };
  add(`timeout ${kind}`, 'timeout', { member: member({}), timeout }, json(ev.buildTimeoutEmbed({ member: MEM(member({})), timeout: fake })));
}
add('avatar embed', 'avatar', { member: member({}), avatar: { before: null, after: 'https://cdn.discordapp.com/x.webp' } }, json(ev.buildAvatarEmbed({ member: MEM(member({})), avatar: { before: null, after: 'https://cdn.discordapp.com/x.webp' } })));

const uOld = { ...alice, username: 'old', globalName: 'Old Name' };
const uNew = { ...alice, username: 'new', globalName: null, avatarUrl256: 'https://cdn.discordapp.com/new.webp?size=256' };
add('user update', 'userUpdate', { oldUser: uOld, user: uNew }, json(ev.buildUserUpdateEmbed({ user: U(uNew), diff: ev.diffUser(U(uOld), U(uNew)) })));
add('user update none', 'userUpdate', { oldUser: alice, user: alice }, json(ev.buildUserUpdateEmbed({ user: U(alice), diff: ev.diffUser(U(alice), U(alice)) })));

add('ban by bot', 'ban', { user: bob, executorId: '999', reason: 'spam', unban: false, botId: '999' }, json(ev.buildBanEmbed({ user: U(bob), executorId: '999', reason: 'spam', unban: false, botId: '999' })));
add('unban unknown', 'ban', { user: robot, unban: true }, json(ev.buildBanEmbed({ user: U(robot), unban: true })));

const voice = (name, a) => {
  const c = ev.classifyVoice(VS(a.oldState), VS(a.newState));
  add(name, 'voice', a, { c, embed: json(ev.buildVoiceEmbed({ member: MEM(a.member), guild: guildOfKnown(a.known, a.names), c })) });
};
voice('voice leave deleted', { member: member({}), oldState: { channelId: '700000000000000009' }, newState: { channelId: null }, known: KNOWN });
voice('voice join', { member: member({}), oldState: null, newState: { channelId: '700000000000000001' }, known: KNOWN });
voice('voice move', { member: member({}), oldState: { channelId: '700000000000000002' }, newState: { channelId: '700000000000000003' }, known: KNOWN });
voice('voice state', { member: member({}), oldState: { channelId: 'A', serverMute: false, serverDeaf: true, selfDeaf: false, selfMute: true, streaming: false }, newState: { channelId: 'A', serverMute: true, serverDeaf: false, selfDeaf: true, selfMute: false, streaming: true } });
const VN = ['700000000000000001', '700000000000000002', '700000000000000003'];
const NAMES = { '700000000000000001': 'Temp Room', '700000000000000002': 'Gaming', '700000000000000003': 'a`b' };
voice('voice join named', { member: member({}), oldState: null, newState: { channelId: '700000000000000001' }, known: VN, names: NAMES });
voice('voice leave named', { member: member({}), oldState: { channelId: '700000000000000002' }, newState: { channelId: null }, known: VN, names: NAMES });
voice('voice move named and deleted', { member: member({}), oldState: { channelId: '700000000000000009' }, newState: { channelId: '700000000000000001' }, known: VN, names: NAMES });
voice('voice state named', { member: member({}), oldState: { channelId: '700000000000000002', selfMute: false }, newState: { channelId: '700000000000000002', selfMute: true }, known: VN, names: NAMES });
voice('voice join backtick name', { member: member({}), oldState: null, newState: { channelId: '700000000000000003' }, known: VN, names: NAMES });
voice('voice nothing', { member: member({}), oldState: { channelId: 'A', selfMute: true }, newState: { channelId: 'A', selfMute: true } });

add('channel create', 'channelCreate', { channel: chan({ topic: 'hello', parentName: 'Cat', name: 'a`b' }) }, json(ev.buildChannelCreateEmbed({ channel: CH(chan({ topic: 'hello', parentName: 'Cat', name: 'a`b' })) })));
const VIEW = 1n << 10n; const SEND = 1n << 11n; const EMB = 1n << 14n; const ATT = 1n << 15n;
const owOld = [{ id: 'r1', type: 0, allow: String(EMB), deny: String(ATT) }, { id: 'r2', type: 0, allow: String(VIEW), deny: '0' }, { id: 'r3', type: 0, allow: String(VIEW), deny: '0' }, { id: 'r5', type: 0, allow: String(EMB), deny: '0' }];
const owNew = [{ id: 'r1', type: 0, allow: String(VIEW | SEND), deny: String(ATT | EMB) }, { id: 'r3', type: 0, allow: String(VIEW), deny: '0' }, { id: 'u9', type: 1, allow: String(VIEW), deny: String(SEND) }, { id: 'r5', type: 0, allow: '0', deny: '0' }, { id: 'r6', type: 0, allow: '0', deny: '0' }];
const chDiff = (name, a) => {
  const lines = ev.diffChannel(CH(a.old), CH(a.new));
  add(name, 'channelUpdate', a, { lines, embed: json(ev.buildChannelUpdateEmbed({ channel: CH(a.new), lines })) });
};
chDiff('channel update everything', { old: chan({ topic: 'a', parentId: 'c1', parentName: 'Old cat', rateLimitPerUser: 10, overwrites: owOld }), new: chan({ name: 'new-name', topic: 'b'.repeat(700), nsfw: true, rateLimitPerUser: 3600, parentId: 'c2', overwrites: owNew }) });
chDiff('channel update voice', { old: chan({ type: 2, bitrate: 64000, userLimit: 0, rtcRegion: null }), new: chan({ type: 2, bitrate: 96500, userLimit: 5, rtcRegion: 'us-east' }) });
chDiff('channel update position only', { old: chan({}), new: chan({}) });
chDiff('channel update type', { old: chan({ type: 0 }), new: chan({ type: 5, rateLimitPerUser: 0 }) });
add('channel delete', 'channelDelete', { channel: chan({ name: 'gone', type: 2, parentName: 'Voice' }), occupants: ['u1', 'u2'], botReason: 'tempvoice', botId: '999' },
  json(ev.buildChannelDeleteEmbed({ channel: CH(chan({ name: 'gone', type: 2, parentName: 'Voice' })), occupants: ['u1', 'u2'], botReason: 'tempvoice', botId: '999' })));
add('channel delete plain', 'channelDelete', { channel: chan({ type: 77 }), occupants: [] }, json(ev.buildChannelDeleteEmbed({ channel: CH(chan({ type: 77 })) })));

add('thread create', 'threadCreate', { thread: thread({ rateLimitPerUser: 60 }) }, json(ev.buildThreadCreateEmbed({ thread: TH(thread({ rateLimitPerUser: 60 })) })));
add('thread create bare', 'threadCreate', { thread: { id: '9', type: 12 } }, json(ev.buildThreadCreateEmbed({ thread: TH({ id: '9', type: 12 }) })));
const thDiff = (name, a) => {
  const lines = ev.diffThread(TH(a.old), TH(a.new));
  add(name, 'threadUpdate', a, { lines, embed: json(ev.buildThreadUpdateEmbed({ thread: TH(a.new), lines })) });
};
thDiff('thread update all', { old: thread({ invitable: false, appliedTags: ['t1', 't3'] }), new: thread({ name: 'solved', locked: true, rateLimitPerUser: 30, autoArchiveDuration: 10080, invitable: true, appliedTags: ['t2', 't3'], availableTags: [{ id: 't2', name: 'Solved' }] }) });
thDiff('thread update odd archive', { old: thread({ autoArchiveDuration: 60 }), new: thread({ autoArchiveDuration: 120, locked: undefined }) });
thDiff('thread update nothing', { old: thread({}), new: thread({}) });
add('thread delete', 'threadDelete', { thread: thread({}), botReason: 'automation', botId: '999', known: KNOWN }, json(ev.buildThreadDeleteEmbed({ thread: TH(thread({}), KNOWN), botReason: 'automation', botId: '999' })));
add('thread delete gone parent', 'threadDelete', { thread: { id: '9', type: 11, parentId: '700000000000000099' }, known: KNOWN }, json(ev.buildThreadDeleteEmbed({ thread: TH({ id: '9', type: 11, parentId: '700000000000000099' }, KNOWN) })));

const MM = 1n << 13n; const KICK = 1n << 1n; const BAN = 1n << 2n; const ADMIN = 1n << 3n;
add('role create', 'roleCreate', { role: role({ managed: true, permissions: String(MM | ADMIN), name: 'x`y' }) }, json(ev.buildRoleCreateEmbed({ role: R(role({ managed: true, permissions: String(MM | ADMIN), name: 'x`y' })) })));
add('role create none', 'roleCreate', { role: role({}) }, json(ev.buildRoleCreateEmbed({ role: R(role({})) })));
const rDiff = (name, a) => {
  const lines = ev.diffRole(R(a.old), R(a.new));
  add(name, 'roleUpdate', a, { lines, embed: json(ev.buildRoleUpdateEmbed({ role: R(a.new), lines })) });
};
rDiff('role update', { old: role({ permissions: String(MM | KICK) }), new: role({ name: 'Staff', hexColor: '#ff0000', hoist: true, mentionable: true, unicodeEmoji: '🔥', permissions: String(MM | BAN) }) });
rDiff('role update nothing', { old: role({}), new: role({}) });
add('role delete', 'roleDelete', { role: role({}) }, json(ev.buildRoleDeleteEmbed({ role: R(role({})) })));

const gDiff = (name, a) => {
  const lines = ev.diffGuild(G(a.old), G(a.new));
  add(name, 'guildUpdate', a, { lines, embed: json(ev.buildGuildUpdateEmbed({ oldGuild: G(a.old), newGuild: G(a.new), lines })) });
};
gDiff('guild update', {
  old: guild({ description: 'old' }),
  new: guild({ name: 'Renamed', icon: 'abc', iconUrl: 'https://cdn.discordapp.com/icons/9/abc.webp', banner: 'b', splash: 's', description: null, ownerId: '2', verificationLevel: 2, explicitContentFilter: 2, defaultMessageNotifications: 1, afkChannelId: '5', afkTimeout: 900, systemChannelId: '6', rulesChannelId: '7', publicUpdatesChannelId: '8', vanityURLCode: 'cool', preferredLocale: 'de', premiumTier: 2, mfaLevel: 1, nsfwLevel: 3, premiumProgressBarEnabled: true }),
});
gDiff('guild update nothing', { old: guild({}), new: guild({}) });

add('emoji create', 'emojiCreate', { emoji: emoji({ roles: ['r1', 'r2'], animated: true, mention: '<a:party:1234>' }) }, json(ev.buildEmojiCreateEmbed({ emoji: E(emoji({ roles: ['r1', 'r2'], animated: true, mention: '<a:party:1234>' })) })));
const eDiff = (name, a) => {
  const lines = ev.diffEmoji(E(a.old), E(a.new));
  add(name, 'emojiUpdate', a, { lines, embed: json(ev.buildEmojiUpdateEmbed({ emoji: E(a.new), lines })) });
};
eDiff('emoji update', { old: emoji({ roles: ['r0'] }), new: emoji({ name: 'party2', roles: ['r1'] }) });
eDiff('emoji update nothing', { old: emoji({}), new: emoji({}) });
add('emoji delete', 'emojiDelete', { emoji: emoji({}) }, json(ev.buildEmojiDeleteEmbed({ emoji: E(emoji({})) })));

add('invite create', 'inviteCreate', { invite: { code: 'abc', channelId: '7', inviter: alice, maxUses: 0, temporary: false } }, json(ev.buildInviteCreateEmbed({ invite: INV({ code: 'abc', channelId: '7', inviter: alice, maxUses: 0, temporary: false }) })));
add('invite create limited', 'inviteCreate', { invite: { code: 'abc', maxUses: 5, expiresTimestamp: NOW + DAY, temporary: true } }, json(ev.buildInviteCreateEmbed({ invite: INV({ code: 'abc', maxUses: 5, expiresTimestamp: NOW + DAY, temporary: true }) })));
add('invite delete', 'inviteDelete', { invite: { code: 'abc' } }, json(ev.buildInviteDeleteEmbed({ invite: INV({ code: 'abc' }) })));

const fit = (name, data) => add(name, 'fitEmbedData', { data }, json(svc.fitEmbedData(data)));
fit('fit 30 fields', { title: 't', fields: Array.from({ length: 30 }, (_, i) => ({ name: `n${i}`, value: `v${i}`, inline: i % 2 === 0 })) });
fit('fit limits', { title: 'x'.repeat(500), description: 'd'.repeat(5000), footer: { text: 'f'.repeat(3000) }, author: { name: 'a'.repeat(300) } });
fit('fit empty fields', { fields: [{ name: '', value: '' }, { name: 'n'.repeat(400), value: 'v'.repeat(2000), inline: true }] });
fit('fit total', { title: 't', description: 'd'.repeat(4096), fields: Array.from({ length: 5 }, (_, i) => ({ name: `n${i}`, value: 'v'.repeat(1000) })) });
fit('fit description cut', { title: 't'.repeat(256), description: 'é'.repeat(4096), footer: { text: 'f'.repeat(2048) } });
add('extract mentions', 'extractMentions', { message: msg({ mentionUsers: [alice, bob, robot], mentionRoles: [{ id: '4', name: 'Mods' }], mentionEveryone: true }), exclude: alice.id },
  svc.extractMentions(M(msg({ mentionUsers: [alice, bob, robot], mentionRoles: [{ id: '4', name: 'Mods' }], mentionEveryone: true })), { excludeUserId: alice.id }));
add('channel type labels', 'channelTypeLabels', {}, [0, 1, 2, 4, 5, 10, 11, 12, 13, 14, 15, 16, 99].map((t) => svc.channelTypeLabel(t)));
add('toggle events', 'toggle', {}, [
  svc.toggleLogEvents(['junk', 'roleDelete', 'roleDelete', 'messageEdit'], 'memberBan', false),
  svc.toggleLogEvents(['messageEdit'], 'all', true), svc.toggleLogEvents([], 'all', false), svc.toggleLogEvents(['memberBan'], 'memberBan', true),
]);
add('registry', 'registry', {}, Object.entries(svc.LOG_EVENTS).map(([key, e]) => ({ key, ...e })));

// ── Snapshots ───────────────────────────────────────────────────────────────────
const snapshots = [];
const S = (codes, extra = {}) => ({ v: 1, takenAt: 1000, codes, tombstones: {}, vanity: null, ...extra });
const c = (uses, maxUses = 0, inviterId = null, expiresAt = null) => ({ uses, maxUses, inviterId, expiresAt });
const o = (code, uses, maxUses = 0, inviterId = null, expiresAt = null) => ({ code, uses, maxUses, inviterId, expiresAt });
const T0 = 10_000_000;
const resolveCase = (name, before, observation, now = T0) => {
  const r = inv.resolveJoin(before, observation, { now });
  snapshots.push({ name, fn: 'resolveJoin', args: { before, observation, now }, out: JSON.parse(JSON.stringify({ match: r.match, reason: r.reason, candidates: r.candidates, next: r.next })) });
};
resolveCase('T1', S({ alice: c(5, 0, 'A'), bob: c(2, 0, 'B') }), { invites: [o('alice', 5), o('bob', 3)] });
resolveCase('T3 consumed', S({ single: c(0, 1, 'C'), perm: c(5) }), { invites: [o('perm', 5)] });
const deleted = inv.applyInviteDelete(S({ single: c(0, 1, 'C'), perm: c(5) }), 'single', { now: T0 });
resolveCase('T3b tombstone', deleted, { invites: [o('perm', 5)] }, T0 + 1000);
resolveCase('T3c tombstone expired', deleted, { invites: [o('perm', 5)] }, T0 + 2 * MIN + 1);
resolveCase('T4c expiry', S({ gone: c(0, 1, 'X', T0 - 1), g2: c(0, 1, 'Y', T0 + 1), g3: c(3, 10) }), { invites: [] });
resolveCase('T5 ambiguous', S({ alice: c(5), bob: c(2), v: c(0, 1, 'V') }), { invites: [o('alice', 6), o('bob', 3)] });
resolveCase('T6 vanity', S({ alice: c(5) }, { vanity: { code: 'cool', uses: 40 } }), { invites: [o('alice', 5)], vanity: { code: 'cool', uses: 41 } });
resolveCase('T6e vanity ambiguous', S({ alice: c(5) }, { vanity: { code: 'cool', uses: 40 } }), { invites: [o('alice', 6)], vanity: { code: 'cool', uses: 41 } });
resolveCase('T6f unknown baseline', S({ alice: c(5) }), { invites: [o('alice', 5)], vanity: { code: 'cool', uses: 41 } });
resolveCase('T7c fresh code', S({ alice: c(5) }), { invites: [o('alice', 5), o('fresh', 1, 0, 'F')] });
resolveCase('inviter from snapshot', S({ a: c(1, 0, 'OLD') }), { invites: [o('a', 2)] });
resolveCase('old tombstones', S({ a: c(1) }, { tombstones: { old: { ...c(0, 1), deletedAt: T0 - 3 * MIN }, fresh: { ...c(0, 2, 'Q'), deletedAt: T0 - 10 } } }), { invites: [o('a', 1)] });
const snap = (name, fn, args, out) => snapshots.push({ name, fn, args, out: JSON.parse(JSON.stringify(out)) });
const prev = S({ a: c(1), b: c(0, 1, 'B') }, { tombstones: { t: { ...c(4, 5), deletedAt: T0 - 10 } }, vanity: { code: 'v', uses: 2 } });
snap('build with previous', 'buildSnapshot', { observation: { invites: [o('a', 1), o('n', 2, 3, 'N', T0 + 9)], vanity: { code: 'v', uses: 3 } }, previous: prev, now: T0 },
  inv.buildSnapshot({ invites: [o('a', 1), o('n', 2, 3, 'N', T0 + 9)], vanity: { code: 'v', uses: 3 } }, { previous: prev, now: T0 }));
snap('build without previous', 'buildSnapshot', { observation: { invites: [o('a', 1)] }, previous: null, now: T0 }, inv.buildSnapshot({ invites: [o('a', 1)] }, { previous: null, now: T0 }));
snap('apply create', 'applyInviteCreate', { snapshot: prev, invite: o('new', 0, 0, 'N'), now: T0 + 3 * MIN }, inv.applyInviteCreate(prev, o('new', 0, 0, 'N'), { now: T0 + 3 * MIN }));
snap('apply create known', 'applyInviteCreate', { snapshot: prev, invite: o('a', 0, 0, 'Z'), now: T0 }, inv.applyInviteCreate(prev, o('a', 0, 0, 'Z'), { now: T0 }));
snap('apply delete', 'applyInviteDelete', { snapshot: prev, code: 'a', now: T0 }, inv.applyInviteDelete(prev, 'a', { now: T0 }));
const shapes = [S({}), { alice: 5 }, null, [], S({}, { vanity: undefined }), S({}, { vanity: { code: 'x', uses: 1.5 } }), S({}, { vanity: { code: 'x', uses: 3 } }), S({}, { v: 2 }), S({}, { takenAt: 'x' }), S({}, { codes: [] })];
snap('isSnapshot', 'isSnapshot', { values: shapes }, shapes.map((x) => inv.isSnapshot(x)));
const H = HOUR;
const rows = [
  { userId: 'a', inviterId: 'I', joinedAt: NOW - H, leftAt: null },
  { userId: 'b', inviterId: 'I', joinedAt: NOW - 48 * H, leftAt: null },
  { userId: 'c', inviterId: 'I', joinedAt: NOW - 100 * H, leftAt: NOW - 70 * H },
  { userId: 'd', inviterId: 'I', joinedAt: NOW - 100 * H, leftAt: NOW - 98 * H },
  { userId: 'I', inviterId: 'I', joinedAt: NOW - 10 * H, leftAt: null },
  { userId: 'e', inviterId: null, joinedAt: NOW - H, leftAt: null },
];
snap('computeStats', 'computeStats', { rows, now: NOW }, inv.computeStats(rows, { now: NOW }));

// ── Keys ────────────────────────────────────────────────────────────────────────
const keys = { snapshot: inv.snapshotKey('955007000000000001') };
// The timeout dedupe key, as the guildMemberUpdate hook builds it.
{
  const realSetNX = cache.setNX;
  cache.setNX = async (key) => { keys.timeout = key; return true; };
  const g = { id: '955007000000000001', members: { me: null }, channels: { cache: new Map() } };
  const until = Date.now() + 2 * HOUR;
  keys.timeoutUntilSec = Math.floor(until / 1000);
  const m = MEM({ id: '222222222222222222', user: bob, guildId: g.id, timeoutUntil: until });
  m.isCommunicationDisabled = () => true;
  const hook = plugin.hooks.find((h) => h.event === 'guildMemberUpdate');
  await hook.run({ client: { user: { id: 'bot' } }, guild: g, guildId: g.id, state: {}, getSettings: async () => ({ loggingEnabled: false, disabledLogEvents: [] }), oldMember: MEM({ id: m.id, partial: true }), newMember: m, member: m, oldKnown: false });
  cache.setNX = realSetNX;
}
// The /invites link cooldown key.
{
  const realSetNX = cache.setNX;
  const realFind = prisma.personalInvite.findUnique;
  cache.setNX = async (key) => { keys.linkCooldown = key; return false; };
  prisma.personalInvite.findUnique = async () => null;
  const g = { id: '955007000000000001', members: { me: {} }, systemChannel: null, client: { user: { id: 'bot' } } };
  const channel = { id: '700000000000000001', type: 0, guild: g, isThread: () => false, permissionsFor: () => ({ has: () => true }) };
  const i = {
    guild: g, user: { id: '222222222222222222', tag: 'bob' }, member: {}, channel, client: g.client, createdTimestamp: Date.now(), replies: [],
    options: { getSubcommand: () => 'link', getChannel: () => null, getUser: () => null, getMember: () => null, getInteger: () => null },
    deferReply: async () => {}, editReply: async (p) => { i.replies.push(p); },
  };
  const settingsService = await imp('src', 'services', 'settingsService.js');
  void settingsService;
  await invitesCmd.execute(i);
  keys.slowDown = i.replies.map((p) => json(p.embeds?.[0])?.title);
  cache.setNX = realSetNX;
  prisma.personalInvite.findUnique = realFind;
}

// ── CDN URLs (the adapters' discord.js defaults) ─────────────────────────────────
const rest = await import('@discordjs/rest');
const cdn = new rest.CDN();
const HASH = '0123456789abcdef0123456789abcdef';
const AHASH = `a_${HASH}`;
const cdnCases = {
  hash: HASH,
  avatar: cdn.avatar('111111111111111111', HASH),
  avatar256: cdn.avatar('111111111111111111', HASH, { size: 256 }),
  animatedAvatar: cdn.avatar('111111111111111111', AHASH),
  animatedAvatar256: cdn.avatar('111111111111111111', AHASH, { size: 256 }),
  memberAvatar256: cdn.guildMemberAvatar('955007000000000001', '111111111111111111', HASH, { size: 256 }),
  animatedMemberAvatar256: cdn.guildMemberAvatar('955007000000000001', '111111111111111111', AHASH, { size: 256 }),
  icon: cdn.icon('955007000000000001', HASH),
  animatedIcon: cdn.icon('955007000000000001', AHASH),
  emoji: cdn.emoji('1234'),
  defaultAvatar: cdn.defaultAvatar(rest.calculateUserDefaultAvatarIndex('111111111111111111')),
};

const out = resolve(ROOT, 'rust', 'fixtures', 'golden', 'feat-logging');
mkdirSync(out, { recursive: true });
writeFileSync(join(out, 'cases.json'), `${JSON.stringify({ now: NOW, embeds, snapshots, keys, cdn: cdnCases }, null, 1)}\n`);
process.stdout.write(`wrote ${embeds.length} embed cases, ${snapshots.length} snapshot cases\n`);
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
