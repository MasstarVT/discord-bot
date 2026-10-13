// newTargets tests: the `new_<picker>` companions that create (or reuse) a role or channel from a slash command.
// Offline: a real discord.js option resolver (pins the private field the substitution relies on) and fake guilds.
import { test } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const {
  ApplicationCommandOptionType: T, ChannelType, Collection, CommandInteractionOptionResolver,
  PermissionFlagsBits, PermissionsBitField, SlashCommandBuilder,
} = await import('discord.js');
const nt = await import('../src/utils/newTargets.js');
const { channelTypeFor, findByName, companionPairs, leafDefinitions, substituteOption, resolveNewOptions, newRoleOption, newChannelOption } = nt;

const data = new SlashCommandBuilder().setName('demo').setDescription('demo')
  .addSubcommand((s) => s.setName('role').setDescription('d')
    .addRoleOption((o) => o.setName('role').setDescription('d'))
    .addStringOption((o) => newRoleOption(o)))
  .addSubcommand((s) => s.setName('log').setDescription('d')
    .addChannelOption((o) => o.setName('channel').setDescription('d').addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement))
    .addStringOption((o) => newChannelOption(o)))
  .addSubcommand((s) => s.setName('cat').setDescription('d')
    .addChannelOption((o) => o.setName('category').setDescription('d').addChannelTypes(ChannelType.GuildCategory))
    .addStringOption((o) => newChannelOption(o, 'category', 'category')));

const ALL = Object.values(PermissionFlagsBits);

function fakeGuild({ roles = [], channels = [], botPerms = ALL, fail = null } = {}) {
  const calls = { roles: [], channels: [] };
  const roleCache = new Collection([['G', { id: 'G', name: '@everyone', managed: false }], ...roles.map((r) => [r.id, r])]);
  const channelCache = new Collection(channels.map((c) => [c.id, c]));
  let n = 0;
  const guild = {
    id: 'G',
    roles: {
      cache: roleCache,
      async create(o) {
        calls.roles.push(o);
        if (fail) throw fail;
        const r = { id: `R${++n}`, name: o.name, managed: false };
        roleCache.set(r.id, r);
        return r;
      },
    },
    channels: {
      cache: channelCache,
      async create(o) {
        calls.channels.push(o);
        if (fail) throw fail;
        const c = { id: `C${++n}`, name: o.name, type: o.type };
        channelCache.set(c.id, c);
        return c;
      },
    },
    members: { me: { id: 'BOT', permissions: new PermissionsBitField(botPerms) } },
  };
  return { guild, calls };
}

/** A deferred interaction whose options come from a real resolver. `opts` are the leaf options of `sub`. */
function fakeInteraction(guild, sub, opts, { memberPerms = ALL } = {}) {
  const replies = [];
  return {
    commandName: 'demo', guildId: guild.id, guild,
    user: { id: 'U', username: 'admin' },
    memberPermissions: new PermissionsBitField(memberPerms),
    options: new CommandInteractionOptionResolver({}, [{ name: sub, type: T.Subcommand, options: opts }], null),
    deferred: true, replied: false, ephemeral: false,
    async editReply(p) { replies.push(p); return {}; },
    async followUp(p) { replies.push(p); return {}; },
    replies,
  };
}

const str = (name, value) => ({ name, type: T.String, value });
const replyTitle = (i) => i.replies.at(-1)?.embeds?.[0]?.toJSON().title?.replace(/^[^A-Za-z]+/, ''); // errorEmbed prefixes an icon

test('builders: companion names, descriptions and limits', () => {
  const sub = data.toJSON().options.find((o) => o.name === 'cat');
  const c = sub.options.find((o) => o.name === 'new_category');
  assert.equal(c.type, T.String);
  assert.match(c.description, /new category/);
  assert.equal(c.max_length, 100);
  assert.ok(c.description.length <= 100);
});

test('channelTypeFor picks a creatable type, text by default, null for threads only', () => {
  assert.equal(channelTypeFor(undefined), ChannelType.GuildText);
  assert.equal(channelTypeFor([ChannelType.GuildAnnouncement, ChannelType.GuildText]), ChannelType.GuildText);
  assert.equal(channelTypeFor([ChannelType.GuildVoice, ChannelType.GuildStageVoice]), ChannelType.GuildVoice);
  assert.equal(channelTypeFor([ChannelType.GuildCategory]), ChannelType.GuildCategory);
  assert.equal(channelTypeFor([ChannelType.PublicThread]), null);
});

test('findByName prefers an exact match and compares text channels in slug form', () => {
  const items = [{ name: 'live' }, { name: 'Live' }];
  assert.equal(findByName(items, 'Live'), items[1]);
  assert.equal(findByName(items, 'LIVE'), items[0]);
  assert.equal(findByName([{ name: 'mod-logs' }], ' Mod Logs ', { slugged: true })?.name, 'mod-logs');
  assert.equal(findByName([{ name: 'mod-logs' }], 'Mod Logs'), null);
});

test('companionPairs only pairs new_<x> strings with a role or channel sibling', () => {
  const defs = [
    { name: 'role', type: T.Role }, { name: 'new_role', type: T.String },
    { name: 'new_ghost', type: T.String }, { name: 'count', type: T.Integer }, { name: 'new_count', type: T.String },
  ];
  assert.deepEqual(companionPairs(defs).map((p) => p.newName), ['new_role']);
});

test('substituteOption: getRole/getChannel and options.data see the created entity (pins discord.js internals)', () => {
  const { guild } = fakeGuild();
  const i = fakeInteraction(guild, 'role', [str('new_role', 'Live')]);
  assert.deepEqual(leafDefinitions(data, i).map((o) => o.name), ['role', 'new_role']);
  const role = { id: 'R9', name: 'Live' };
  substituteOption(i, 'new_role', 'role', 'role', role);
  assert.equal(i.options.getRole('role', true), role);
  assert.equal(i.options.getString('new_role'), null);
  assert.deepEqual(i.options.data[0].options.map((o) => o.name), ['role']);

  const ch = { id: 'C9', name: 'logs', type: ChannelType.GuildText };
  const j = fakeInteraction(guild, 'log', [str('new_channel', 'logs')]);
  substituteOption(j, 'new_channel', 'channel', 'channel', ch);
  assert.equal(j.options.getChannel('channel', true, [ChannelType.GuildText]), ch);
});

test('resolveNewOptions: nothing typed continues; a required picker left empty is refused', async () => {
  const { guild, calls } = fakeGuild();
  assert.equal(await resolveNewOptions(fakeInteraction(guild, 'role', []), data), true);
  const i = fakeInteraction(guild, 'role', []);
  assert.equal(await resolveNewOptions(i, data, { required: ['role'] }), false);
  assert.equal(replyTitle(i), 'Missing Role');
  assert.equal(calls.roles.length, 0);
});

test('resolveNewOptions: picking and typing at once is refused; a blank name is refused', async () => {
  const { guild, calls } = fakeGuild({ roles: [{ id: 'R0', name: 'Old', managed: false }] });
  const both = fakeInteraction(guild, 'role', [{ name: 'role', type: T.Role, value: 'R0', role: guild.roles.cache.get('R0') }, str('new_role', 'Live')]);
  assert.equal(await resolveNewOptions(both, data), false);
  assert.equal(replyTitle(both), 'Pick One');
  const blank = fakeInteraction(guild, 'role', [str('new_role', '   ')]);
  assert.equal(await resolveNewOptions(blank, data), false);
  assert.equal(replyTitle(blank), 'Name Required');
  assert.equal(calls.roles.length, 0);
});

test('resolveNewOptions: creates a role with no permissions and an audit reason', async () => {
  const { guild, calls } = fakeGuild();
  const i = fakeInteraction(guild, 'role', [str('new_role', '  Live Now ')]);
  assert.equal(await resolveNewOptions(i, data, { required: ['role'] }), true);
  assert.equal(calls.roles.length, 1);
  const o = calls.roles[0];
  assert.equal(o.name, 'Live Now');
  assert.deepEqual(o.permissions, []);
  assert.equal(o.mentionable, false);
  assert.match(o.reason, /^\/demo role by admin \(U\)$/);
  assert.equal(i.options.getRole('role', true).name, 'Live Now');
  assert.equal(i.replies.length, 0);
});

test('resolveNewOptions: an existing role with that name is reused (no permission needed); managed roles and @everyone are not', async () => {
  const { guild, calls } = fakeGuild({ roles: [{ id: 'R5', name: 'Live', managed: false }, { id: 'R6', name: 'Bot', managed: true }] });
  const i = fakeInteraction(guild, 'role', [str('new_role', 'live')], { memberPerms: [] });
  assert.equal(await resolveNewOptions(i, data), true);
  assert.equal(i.options.getRole('role').id, 'R5');
  assert.equal(calls.roles.length, 0);

  const j = fakeInteraction(guild, 'role', [str('new_role', 'Bot')]);
  assert.equal(await resolveNewOptions(j, data), true);
  assert.notEqual(j.options.getRole('role').id, 'R6');
  assert.equal(calls.roles.length, 1);
});

test('resolveNewOptions: creating needs Manage Roles for the invoker and the bot', async () => {
  const user = fakeGuild();
  const i = fakeInteraction(user.guild, 'role', [str('new_role', 'Live')], { memberPerms: [PermissionFlagsBits.ManageGuild] });
  assert.equal(await resolveNewOptions(i, data), false);
  assert.match(i.replies[0].embeds[0].toJSON().description, /You need the \*\*Manage Roles\*\*/);
  assert.equal(user.calls.roles.length, 0);

  const bot = fakeGuild({ botPerms: [PermissionFlagsBits.ViewChannel] });
  const j = fakeInteraction(bot.guild, 'role', [str('new_role', 'Live')]);
  assert.equal(await resolveNewOptions(j, data), false);
  assert.match(j.replies[0].embeds[0].toJSON().description, /I need the \*\*Manage Roles\*\*/);
  assert.equal(bot.calls.roles.length, 0);
});

test('resolveNewOptions: Discord errors become friendly messages', async () => {
  const { guild } = fakeGuild({ fail: Object.assign(new Error('Maximum number of guild roles reached'), { code: 30005 }) });
  const i = fakeInteraction(guild, 'role', [str('new_role', 'Live')]);
  assert.equal(await resolveNewOptions(i, data), false);
  assert.equal(replyTitle(i), 'Could Not Create Role');
  assert.match(i.replies[0].embeds[0].toJSON().description, /maximum number of roles/);
});

test('resolveNewOptions: channels take the picker type; private ones hide from @everyone', async () => {
  const { guild, calls } = fakeGuild();
  const i = fakeInteraction(guild, 'log', [str('new_channel', 'Mod Logs')]);
  assert.equal(await resolveNewOptions(i, data, { private: ['channel'] }), true);
  const o = calls.channels[0];
  assert.equal(o.type, ChannelType.GuildText);
  const everyone = o.permissionOverwrites.find((w) => w.id === 'G');
  assert.deepEqual(everyone.deny, [PermissionFlagsBits.ViewChannel]);
  assert.ok(o.permissionOverwrites.find((w) => w.id === 'BOT').allow.includes(PermissionFlagsBits.SendMessages));
  assert.ok(o.permissionOverwrites.find((w) => w.id === 'U').allow.includes(PermissionFlagsBits.ViewChannel));
  assert.equal(i.options.getChannel('channel').id, guild.channels.cache.last().id);

  const j = fakeInteraction(guild, 'cat', [str('new_category', 'Tickets')]);
  assert.equal(await resolveNewOptions(j, data), true);
  assert.equal(calls.channels[1].type, ChannelType.GuildCategory);
  assert.equal(calls.channels[1].permissionOverwrites, undefined, 'not private unless asked');
});

test('resolveNewOptions: an existing channel of an accepted type is reused in slug form', async () => {
  const { guild, calls } = fakeGuild({ channels: [
    { id: 'V1', name: 'mod-logs', type: ChannelType.GuildVoice },
    { id: 'T1', name: 'mod-logs', type: ChannelType.GuildText },
  ] });
  const i = fakeInteraction(guild, 'log', [str('new_channel', 'Mod Logs')]);
  assert.equal(await resolveNewOptions(i, data), true);
  assert.equal(i.options.getChannel('channel').id, 'T1');
  assert.equal(calls.channels.length, 0);
});

test('resolveNewOptions: two concurrent requests for the same name create one role', async () => {
  const { guild, calls } = fakeGuild();
  const a = fakeInteraction(guild, 'role', [str('new_role', 'Live')]);
  const b = fakeInteraction(guild, 'role', [str('new_role', 'Live')]);
  assert.deepEqual(await Promise.all([resolveNewOptions(a, data), resolveNewOptions(b, data)]), [true, true]);
  assert.equal(calls.roles.length, 1);
  assert.equal(a.options.getRole('role').id, b.options.getRole('role').id);
});

test('resolveNewOptions: an existing role with powerful permissions is never reused by name', async () => {
  const { guild, calls } = fakeGuild({ roles: [{ id: 'R7', name: 'Moderator', managed: false, permissions: new PermissionsBitField([PermissionFlagsBits.BanMembers]) }] });
  const i = fakeInteraction(guild, 'role', [str('new_role', 'Moderator')]);
  assert.equal(await resolveNewOptions(i, data), false);
  assert.equal(replyTitle(i), 'Role Name Taken');
  assert.match(i.replies[0].embeds[0].toJSON().description, /Ban Members[\s\S]*Pick it in `role`/);
  assert.equal(calls.roles.length, 0);
});

test('resolveNewOptions: a private channel only reuses an existing channel that @everyone cannot see', async () => {
  const visible = (yes) => () => new PermissionsBitField(yes ? [PermissionFlagsBits.ViewChannel] : []);
  const pub = fakeGuild({ channels: [{ id: 'T1', name: 'mod-logs', type: ChannelType.GuildText, permissionsFor: visible(true) }] });
  const i = fakeInteraction(pub.guild, 'log', [str('new_channel', 'mod-logs')]);
  assert.equal(await resolveNewOptions(i, data, { private: ['channel'] }), false);
  assert.equal(replyTitle(i), 'Channel Name Taken');
  assert.equal(pub.calls.channels.length, 0);
  // the same public channel is fine when the option is not private
  const j = fakeInteraction(pub.guild, 'log', [str('new_channel', 'mod-logs')]);
  assert.equal(await resolveNewOptions(j, data), true);
  assert.equal(j.options.getChannel('channel').id, 'T1');

  const priv = fakeGuild({ channels: [{ id: 'T2', name: 'mod-logs', type: ChannelType.GuildText, permissionsFor: visible(false) }] });
  const k = fakeInteraction(priv.guild, 'log', [str('new_channel', 'mod-logs')]);
  assert.equal(await resolveNewOptions(k, data, { private: ['channel'] }), true);
  assert.equal(k.options.getChannel('channel').id, 'T2');
});
