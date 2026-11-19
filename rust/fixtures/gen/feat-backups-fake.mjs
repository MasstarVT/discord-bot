// The fake guild of test/backups.test.js, built from a JSON spec (bit fields are strings), shared by the feat-backups
// fixture scripts. `resetIds()` restarts the id counter (5000) so the Rust fake can make the same ids.
import { AuditLogEvent, ChannelType, OverwriteType, PermissionsBitField, PermissionFlagsBits } from 'discord.js';

const B = (s) => BigInt(s ?? '0');
export const resetIds = () => { nextId = 5000; };
const BOT_ID = '9999';
let nextId = 5000;
const newId = () => String(nextId++);
const mapOf = (items) => new Map(items.map((x) => [x.id, x]));
function fakeRole(spec) {
  return {
    id: spec.id, name: spec.name, colors: { primaryColor: spec.color ?? 0 }, hoist: spec.hoist ?? false, mentionable: spec.mentionable ?? false,
    permissions: { bitfield: typeof spec.perms === 'bigint' ? spec.perms : B(spec.perms) }, rawPosition: spec.pos ?? 1, unicodeEmoji: spec.emoji ?? null, managed: spec.managed ?? false,
  };
}
function fakeChannel(spec) {
  return {
    id: spec.id, type: spec.type ?? ChannelType.GuildText, name: spec.name, parentId: spec.parent ?? null, rawPosition: spec.pos ?? 0,
    topic: spec.topic ?? null, nsfw: spec.nsfw ?? false, rateLimitPerUser: spec.slow ?? 0, bitrate: spec.bitrate ?? null, userLimit: spec.limit ?? 0,
    defaultAutoArchiveDuration: spec.archive ?? null, availableTags: spec.tags ?? [], manageable: spec.manageable ?? true,
    permissionOverwrites: { cache: mapOf((spec.overwrites ?? []).map((o) => ({
      id: o.id, type: o.type ?? OverwriteType.Role, allow: { bitfield: typeof o.allow === 'bigint' ? o.allow : B(o.allow) }, deny: { bitfield: typeof o.deny === 'bigint' ? o.deny : B(o.deny) },
    }))) },
    isThread: () => spec.thread === true,
  };
}
export function makeGuild({ id = '900', ownerId = 'OWNER', roles = [], channels = [], botPerms = '0', botTop = 10, members = [], everyonePerms = '0', features = [], fail = {} } = {}) {
  const log = { create: [], edit: [], put: [], rolePositions: [], channelPositions: [], fetches: [] };
  const audit = [];
  const roleCache = mapOf(roles.map(fakeRole));
  const everyone = fakeRole({ id, name: '@everyone', perms: everyonePerms, pos: 0 });
  roleCache.set(id, everyone);
  const channelCache = mapOf(channels.map(fakeChannel));
  const me = { id: BOT_ID, permissions: new PermissionsBitField(B(botPerms)), roles: { highest: { rawPosition: botTop } } };
  const guild = {
    id, name: 'Test Server', ownerId, available: true, features, maximumBitrate: 96000,
    roles: {
      cache: roleCache, everyone,
      async create(data) {
        if (fail.roleCreate) throw Object.assign(new Error('Maximum number of server roles reached'), { code: 30005 });
        log.create.push(['role', data]);
        const role = fakeRole({ id: newId(), name: data.name, color: data.colors?.primaryColor, hoist: data.hoist, mentionable: data.mentionable, perms: data.permissions, emoji: data.unicodeEmoji, pos: 1 });
        roleCache.set(role.id, role);
        audit.push({ event: AuditLogEvent.RoleCreate, targetId: role.id, executorId: BOT_ID, reason: data.reason, createdTimestamp: Date.now() });
        return role;
      },
      async edit(role, data) {
        log.edit.push(['role', role.id, data]);
        if (data.name !== undefined) role.name = data.name;
        if (data.colors) role.colors = { primaryColor: data.colors.primaryColor };
        if (data.hoist !== undefined) role.hoist = data.hoist;
        if (data.mentionable !== undefined) role.mentionable = data.mentionable;
        if (data.permissions !== undefined) role.permissions = { bitfield: data.permissions };
        if (data.unicodeEmoji !== undefined) role.unicodeEmoji = data.unicodeEmoji;
        return role;
      },
      async setPositions(list) {
        log.rolePositions.push(list);
        for (const e of list) roleCache.get(e.role).rawPosition = e.position;
      },
    },
    channels: {
      cache: channelCache,
      async create(data) {
        if (fail.channelCreate) throw Object.assign(new Error('Missing Permissions'), { code: 50013 });
        log.create.push(['channel', data]);
        const ch = fakeChannel({
          id: newId(), type: data.type, name: data.name, parent: data.parent ?? null, pos: data.position ?? 0, topic: data.topic ?? null,
          nsfw: data.nsfw ?? false, slow: data.rateLimitPerUser ?? 0, bitrate: data.bitrate ?? null, limit: data.userLimit ?? 0,
          overwrites: (data.permissionOverwrites ?? []).map((o) => ({ id: o.id, type: o.type, allow: o.allow, deny: o.deny })),
        });
        channelCache.set(ch.id, ch);
        audit.push({ event: AuditLogEvent.ChannelCreate, targetId: ch.id, executorId: BOT_ID, reason: data.reason, createdTimestamp: Date.now() });
        return ch;
      },
      async edit(ch, data) {
        log.edit.push(['channel', ch.id, data]);
        if (data.name !== undefined) ch.name = data.name;
        if (data.topic !== undefined) ch.topic = data.topic || null;
        if (data.nsfw !== undefined) ch.nsfw = data.nsfw;
        if (data.rateLimitPerUser !== undefined) ch.rateLimitPerUser = data.rateLimitPerUser;
        if (data.bitrate !== undefined) ch.bitrate = data.bitrate;
        if (data.userLimit !== undefined) ch.userLimit = data.userLimit;
        if (data.parent !== undefined) ch.parentId = data.parent;
        return ch;
      },
      async setPositions(list) {
        log.channelPositions.push(list);
        for (const e of list) channelCache.get(e.channel).rawPosition = e.position;
      },
    },
    members: {
      me, cache: new Map(),
      async fetch(userId) {
        log.fetches.push(userId);
        if (!members.includes(userId)) throw Object.assign(new Error('Unknown Member'), { code: 10007 });
        return { id: userId };
      },
    },
    client: { rest: { async put(route, options) { log.put.push({ route, ...options }); return {}; } } },
  };
  guild.log = log;
  guild.fetchAuditLogs = async ({ type }) => ({ entries: mapOf(audit.filter((e) => e.event === type).map((e) => ({ ...e, id: e.targetId }))) });
  return guild;
}

const P = PermissionFlagsBits;
export const bp = (...f) => String(f.reduce((a, x) => a | P[x], 0n));
export const BOT_PERMS = bp('ViewChannel', 'SendMessages', 'EmbedLinks', 'ManageRoles', 'ManageChannels', 'KickMembers', 'BanMembers', 'ManageMessages', 'ReadMessageHistory');
export const ADMIN_PERMS = bp('Administrator', 'ManageRoles');
export const MOD_PERMS = bp('KickMembers', 'BanMembers');
export const MEMBER_PERMS = bp('ViewChannel', 'SendMessages');
export const EVERYONE_PERMS = bp('ViewChannel', 'SendMessages', 'ReadMessageHistory');

export const ORIGINAL = {
  botPerms: BOT_PERMS,
  roles: [
    { id: '101', name: 'Admin', color: 0xff0000, hoist: true, perms: ADMIN_PERMS, pos: 8 },
    { id: '102', name: 'Mod', mentionable: true, perms: MOD_PERMS, pos: 6 },
    { id: '103', name: 'Member', perms: MEMBER_PERMS, pos: 3 },
    { id: '104', name: 'SomeBot', managed: true, perms: bp('ViewChannel'), pos: 2 },
  ],
  channels: [
    { id: '201', type: 4, name: 'Info', pos: 0, overwrites: [{ id: '900', deny: bp('SendMessages') }] },
    { id: '202', name: 'general', parent: '201', pos: 0, topic: 'Say hi', slow: 5, archive: 1440, overwrites: [
      { id: '102', allow: bp('ViewChannel', 'ManageMessages') }, { id: '104', allow: bp('ViewChannel') },
      { id: '777', type: 1, deny: bp('SendMessages') }, { id: '778', type: 1, allow: bp('ViewChannel') },
    ] },
    { id: '203', type: 2, name: 'Lounge', parent: '201', pos: 1, bitrate: 64000, limit: 5 },
    { id: '204', type: 11, name: 'a thread', parent: '202', thread: true },
    { id: '205', type: 14, name: 'directory' },
    { id: '206', type: 15, name: 'help', pos: 2, tags: [{ name: 'Solved', moderated: true, emoji: { id: null, name: '✅' } }, { name: 'Custom', moderated: false, emoji: { id: '55', name: 'x' } }] },
  ],
  everyonePerms: EVERYONE_PERMS,
};
