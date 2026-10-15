// pkg-embeds tests (pkg-embeds.md §8, with the architect amendments): names, the pure template mutators, URL and emoji
// checks, role-ping policy, import parsing, the Welcome Channel presets, the hook filter, the plugin declaration, the
// /embed payload, and the service flows (create, mutate, publish, update, delete, the form, welcome-channel) against
// fakes. Offline by default. With TEST_DATABASE_URL (a migrated database) the DB cases also run:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/embeds.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ChannelType, DiscordAPIError, PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const svc = await import('../src/services/embedService.js');
const cmd = await import('../src/commands/admin/embed.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { COLORS, PERMISSION_LEVELS, LIMITS: GUILD_LIMITS } = await import('../src/config/constants.js');
const { buildCustomId, parseCustomId } = await import('../src/config/customIds.js');
const { emptyTemplate, normalizeTemplate } = await import('../src/utils/customEmbed.js');

const {
  EmbedEditError, PRESET_NAMES, addField, editField, removeField, addButton, removeButton, setImages, setAuthor,
  setFooter, setColor, setTitleUrl, validateForSave, normalizeEmbedName, checkImageUrl, checkLinkUrl, isEmojiSyntax,
  parseButtonEmoji, mentionPolicy, resolveDefaultColor, parseImportPayload, buildWelcomePresets, collectUnknownTokens,
  isEmbedDeleteCandidate, isClearWord, renderSavedEmbed, publishHint, unknownVariablesNote, resolvePublishMentions,
} = svc;

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const GUILD = '955014000000000001';
const GUILD2 = '955014000000000002';

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) {
    // The cascade removes the SavedEmbed rows. Deleting the settings row only here: ensureGuildSettings memoises.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: [GUILD, GUILD2] } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ────────────────────────────────────────────────────────────────────

const BOT = '900000000000000099';
const ADMIN = '900000000000000010';
const bits = (...names) => new PermissionsBitField(names.map((n) => PermissionFlagsBits[n]));
const BOT_PERMS = ['ViewChannel', 'SendMessages', 'EmbedLinks', 'ReadMessageHistory', 'ManageChannels', 'CreateInstantInvite',
  'SendMessagesInThreads', 'CreatePublicThreads', 'CreatePrivateThreads'];
const apiError = (code, message = 'x') => new DiscordAPIError({ message, code }, code, 400, 'POST', 'https://discord.test/api', {});

function makeGuild({ id = GUILD, roles = [], botPerms = BOT_PERMS, vanity = null } = {}) {
  const guild = {
    id, name: 'Test Guild', available: true, ownerId: ADMIN, vanityURLCode: vanity, memberCount: 5,
    iconURL: () => 'https://cdn.example/icon.png',
    client: { user: { id: BOT }, emojis: { cache: new Map() } },
    members: { me: { id: BOT, permissions: bits(...botPerms) } },
    roles: { cache: new Map(roles.map((r) => [r.id, r])), everyone: { id } },
    channels: { cache: new Map(), fetch: async (cid) => { throw apiError(10003, `no channel ${cid}`); }, create: null },
  };
  return guild;
}

let msgSeq = 0;
function makeChannel(guild, { id = '900000000000000500', botPerms = BOT_PERMS, memberPerms = ['ViewChannel', 'SendMessages'], everyoneCanSend = false, type = ChannelType.GuildText } = {}) {
  const ch = {
    id, name: 'general', type, guild, sent: [], fetched: new Map(), deleted: [],
    isTextBased: () => true,
    isThread: () => false,
    permissionsFor(who) {
      if (who?.id === BOT) return bits(...botPerms);
      if (who === guild.roles.everyone) return bits(...(everyoneCanSend ? ['SendMessages'] : []));
      return bits(...memberPerms);
    },
    async send(payload) {
      ch.sent.push(payload);
      const m = { id: `8000000000000${String(++msgSeq).padStart(5, '0')}`, author: { id: BOT }, edits: [] };
      m.url = `https://discord.com/channels/${guild.id}/${id}/${m.id}`;
      m.edit = async (p) => { m.edits.push(p); return m; };
      ch.fetched.set(m.id, m);
      return m;
    },
    messages: {
      fetch: async (mid) => {
        const m = ch.fetched.get(mid);
        if (!m) throw apiError(10008, 'Unknown Message');
        return m;
      },
      delete: async (mid) => {
        if (!ch.fetched.has(mid)) throw apiError(10008, 'Unknown Message');
        ch.fetched.delete(mid);
        ch.deleted.push(mid);
      },
    },
    createInvite: async () => ({ url: 'https://discord.gg/madeup' }),
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

function makeMember(guild, { id = ADMIN, perms = ['Administrator'], roleIds = [] } = {}) {
  return {
    id, guild, displayName: 'Admin', permissions: bits(...perms),
    roles: { cache: new Map(roleIds.map((r) => [r, { id: r }])) },
    user: { id, username: 'admin', tag: 'admin#0', bot: false, displayAvatarURL: () => 'https://cdn.example/a.png' },
  };
}

// ── 1. The /embed payload and module exports ─────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('payload: /embed fits Discord limits and has the planned shape', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'embed');
  assert.ok(charCount(json) <= 2900, `payload is ${charCount(json)} characters`);
  assert.equal(json.options.length, 17);
  assert.equal(json.options.find((o) => o.name === 'field').options.length, 4);
  assert.equal(json.options.find((o) => o.name === 'button').options.length, 2);
  const walk = (node, path) => {
    assert.ok(node.description && node.description.length <= 100, `${path} description`);
    assert.equal(node.choices, undefined, `${path} must not use choices`);
    assert.ok((node.options ?? []).length <= 25);
    for (const o of node.options ?? []) walk(o, `${path} ${o.name}`);
  };
  walk(json, '/embed');
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.deepEqual(json.contexts, [0]);
  assert.equal(cmd.module, 'embeds');
  assert.equal(cmd.gated, true);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(typeof cmd.autocomplete, 'function');
});

// ── 2. Names ─────────────────────────────────────────────────────────────────

test('normalizeEmbedName: the verified cases', () => {
  assert.equal(normalizeEmbedName('My Rules!'), 'my-rules');
  assert.equal(normalizeEmbedName('  Welcome  Greeting '), 'welcome-greeting');
  assert.equal(normalizeEmbedName('Ünïcode'), 'unicode');
  assert.equal(normalizeEmbedName('--x--'), 'x');
  assert.equal(normalizeEmbedName('🎉'), null);
  assert.equal(normalizeEmbedName('a'.repeat(40)), 'a'.repeat(32));
  assert.equal(normalizeEmbedName(null), null);
  assert.equal(normalizeEmbedName(42), null);
  for (const n of PRESET_NAMES) assert.equal(normalizeEmbedName(n), n);
});

test('isClearWord', () => {
  for (const w of ['none', 'CLEAR', ' remove ', '-']) assert.equal(isClearWord(w), true);
  for (const w of ['nope', '', null, undefined]) assert.equal(isClearWord(w), false);
});

// ── 3. Mutators ──────────────────────────────────────────────────────────────

test('addField: creates the field, never mutates, caps at 25, positions', () => {
  const base = emptyTemplate();
  const t = addField(base, { title: 'A', value: 'B' });
  assert.deepEqual(t.embed.fields, [{ name: 'A', value: 'B', inline: false }]);
  assert.deepEqual(base, emptyTemplate());
  let full = emptyTemplate();
  for (let i = 0; i < 25; i++) full = addField(full, { title: `T${i}`, value: 'v' });
  assert.throws(() => addField(full, { title: 'x', value: 'y' }), EmbedEditError);
  const two = addField(addField(emptyTemplate(), { title: 'one', value: '1' }), { title: 'two', value: '2' });
  assert.equal(addField(two, { title: 'zero', value: '0', position: 1 }).embed.fields[0].name, 'zero');
  assert.equal(addField(two, { title: 'last', value: 'L', position: 99 }).embed.fields[2].name, 'last');
  assert.throws(() => addField(two, { title: '  ', value: 'y' }), /cannot be empty/);
});

test('editField / removeField', () => {
  const two = addField(addField(emptyTemplate(), { title: 'one', value: '1' }), { title: 'two', value: '2' });
  assert.throws(() => editField(two, 3, { title: 'x' }), EmbedEditError);
  assert.throws(() => editField(two, 0, { title: 'x' }), EmbedEditError);
  const moved = editField(two, 2, { position: 1, inline: true });
  assert.deepEqual(moved.embed.fields.map((f) => f.name), ['two', 'one']);
  assert.equal(moved.embed.fields[0].inline, true);
  assert.equal(editField(two, 1, { value: ' new ' }).embed.fields[0].value, 'new');
  assert.throws(() => editField(two, 1, { title: '' }), EmbedEditError);
  assert.deepEqual(removeField(two, 1).embed.fields.map((f) => f.name), ['two']);
  assert.throws(() => removeField(two, 5), EmbedEditError);
  const one = addField(emptyTemplate(), { title: 'only', value: 'v' });
  assert.ok(validateForSave(removeField(one, 1)).length > 0, 'an empty template is refused');
});

test('addButton / removeButton', () => {
  const t = addButton(emptyTemplate(), { label: ' Site ', url: 'https://example.com', emoji: '🌐' });
  assert.deepEqual(t.buttons, [{ label: 'Site', url: 'https://example.com', emoji: '🌐' }]);
  assert.throws(() => addButton(emptyTemplate(), { label: 'x', url: 'ftp://x.y' }), EmbedEditError);
  assert.throws(() => addButton(emptyTemplate(), { label: 'x', url: `https://a.b/${'a'.repeat(510)}` }), EmbedEditError);
  assert.throws(() => addButton(emptyTemplate(), { label: '  ', url: 'https://a.b' }), /needs a label/);
  let full = emptyTemplate();
  for (let i = 0; i < 25; i++) full = addButton(full, { label: `B${i}`, url: 'https://a.b' });
  assert.throws(() => addButton(full, { label: 'x', url: 'https://a.b' }), /at most 25/);
  assert.equal(addButton(t, { label: 'First', url: 'https://a.b', position: 1 }).buttons[0].label, 'First');
  assert.deepEqual(removeButton(t, 1).buttons, []);
  assert.throws(() => removeButton(t, 2), EmbedEditError);
});

test('setAuthor / setFooter / setImages / setColor / setTitleUrl', () => {
  assert.throws(() => setAuthor(emptyTemplate(), { iconUrl: 'https://a.b/i.png' }), /author text first/);
  const withAuthor = setAuthor(emptyTemplate(), { text: 'X' });
  assert.equal(setAuthor(withAuthor, { text: 'none' }).embed.author, null);
  const both = setFooter(withAuthor, { timestamp: true });
  assert.equal(both.embed.author.name, 'X');
  assert.equal(both.embed.timestamp, true);
  const iconOnly = setAuthor(withAuthor, { iconUrl: 'https://a.b/i.png' });
  assert.equal(iconOnly.embed.author.name, 'X');
  assert.equal(iconOnly.embed.author.iconUrl, 'https://a.b/i.png');
  assert.equal(setAuthor(iconOnly, { iconUrl: null }).embed.author.iconUrl, null);
  const footer = setFooter(emptyTemplate(), { text: 'Foot', iconUrl: 'https://a.b/f.png' });
  assert.deepEqual(footer.embed.footer, { text: 'Foot', iconUrl: 'https://a.b/f.png' });
  assert.equal(setFooter(footer, { text: 'clear' }).embed.footer, null);
  assert.throws(() => setFooter(emptyTemplate(), { iconUrl: 'https://a.b/f.png' }), /footer text first/);
  const imgs = setImages(emptyTemplate(), { image: 'https://a.b/i.png', thumbnail: '{server.icon}' });
  assert.equal(imgs.embed.image, 'https://a.b/i.png');
  assert.equal(setImages(imgs, { image: null }).embed.image, null);
  assert.equal(setImages(imgs, {}).embed.thumbnail, '{server.icon}');
  assert.equal(setColor(emptyTemplate(), 0xff0000).embed.color, 0xff0000);
  assert.equal(setTitleUrl(emptyTemplate(), 'https://a.b').embed.url, 'https://a.b');
});

test('validateForSave: empty, limits and button rules', () => {
  assert.ok(validateForSave(emptyTemplate()).length > 0);
  assert.deepEqual(validateForSave({ content: 'hi' }), []);
  assert.ok(validateForSave({ content: 'x'.repeat(2001) }).length > 0);
  const bad = { content: 'hi', buttons: [{ label: 'L', url: 'https://a.b', emoji: 'not an emoji' }] };
  assert.ok(validateForSave(bad).some((e) => /emoji/.test(e)));
  const longUrl = { content: 'hi', buttons: [{ label: 'L', url: `https://a.b/${'a'.repeat(510)}` }] };
  assert.ok(validateForSave(longUrl).length > 0);
  const errors = validateForSave(longUrl);
  assert.equal(new Set(errors).size, errors.length, 'errors are deduplicated');
});

// ── 4. URL and emoji checks ──────────────────────────────────────────────────

test('checkImageUrl / checkLinkUrl', () => {
  assert.equal(checkImageUrl('https://a.b/c.png'), 'https://a.b/c.png');
  assert.equal(checkImageUrl(' {user.avatar} '), '{user.avatar}');
  assert.equal(checkImageUrl('{server.icon}'), '{server.icon}');
  assert.throws(() => checkImageUrl('javascript:alert(1)'), EmbedEditError);
  assert.throws(() => checkImageUrl('{user.avatar} x'), EmbedEditError);
  assert.throws(() => checkImageUrl(''), EmbedEditError);
  assert.equal(checkLinkUrl('http://x.y/a', 512), 'http://x.y/a');
  assert.throws(() => checkLinkUrl('ftp://x.y'), EmbedEditError);
  assert.throws(() => checkLinkUrl('{user.avatar}'), EmbedEditError);
  assert.throws(() => checkLinkUrl(`https://a.b/${'a'.repeat(600)}`, 512), /at most 512/);
});

test('isEmojiSyntax / parseButtonEmoji', () => {
  for (const e of ['🎉', '🛡️', '1️⃣', '🇺🇸', '👍🏽', '👨‍👩‍👧', '<:x:123456789012345678>', '<a:wave:123456789012345678>']) {
    assert.equal(isEmojiSyntax(e), true, e);
  }
  for (const e of ['abc', ':smile:', '🎉🎉', 'a🎉', '', '1', null]) assert.equal(isEmojiSyntax(e), false, String(e));
  assert.throws(() => parseButtonEmoji('<:x:123456789012345678>', () => false), /servers I'm in/);
  assert.equal(parseButtonEmoji('<:x:123456789012345678>', () => true), '<:x:123456789012345678>');
  assert.throws(() => parseButtonEmoji('hello'), /valid emoji/);
  assert.equal(parseButtonEmoji(' 🎉 '), '🎉');
});

// ── 5. Role pings (amendment 1) ──────────────────────────────────────────────

test('mentionPolicy / resolvePublishMentions: only pingable roles get through', () => {
  const R1 = '123456789012345678';
  const R2 = '223456789012345678';
  const text = `<@&${R1}> hi <@&${R1}> <@&${R2}> @everyone @here`;
  assert.deepEqual(mentionPolicy('x', 'none'), { parse: [] });
  assert.deepEqual(mentionPolicy(text, 'none'), { parse: [] });
  assert.deepEqual(mentionPolicy(text, 'publish'), { parse: ['users'], roles: [] }, 'no channel: fail closed');

  const guild = makeGuild({ roles: [{ id: R1, mentionable: false }, { id: R2, mentionable: true }] });
  const noMention = makeChannel(guild, { memberPerms: ['ViewChannel'] });
  const policy = mentionPolicy(text, 'publish', { channel: noMention, member: makeMember(guild, { perms: ['ViewChannel'] }) });
  assert.deepEqual(policy, { parse: ['users'], roles: [R2] });
  assert.ok(!('everyone' in policy) && !policy.parse.includes('everyone'));
  const r = resolvePublishMentions(text, noMention, makeMember(guild, { perms: ['ViewChannel'] }));
  assert.deepEqual(r.refused, [R1]);
  assert.deepEqual(Object.keys(r.allowedMentions).sort(), ['parse', 'roles'], 'refused never leaks into allowedMentions');

  const canMentionAll = makeChannel(guild, { id: '900000000000000501', memberPerms: ['ViewChannel', 'MentionEveryone'] });
  const open = resolvePublishMentions(text, canMentionAll, makeMember(guild, { perms: ['MentionEveryone'] }));
  assert.deepEqual(open.allowedMentions.roles.sort(), [R1, R2]);
  assert.deepEqual(open.refused, []);
  assert.deepEqual(resolvePublishMentions('no roles', noMention, null), { allowedMentions: { parse: ['users'], roles: [] }, refused: [] });
});

// ── 6. Colours ───────────────────────────────────────────────────────────────

test('resolveDefaultColor', () => {
  assert.equal(resolveDefaultColor({ embedColor: 0xff0000 }), 0xff0000);
  assert.equal(resolveDefaultColor(null), COLORS.INFO);
  assert.equal(resolveDefaultColor({ embedColor: -1 }), COLORS.INFO);
  assert.equal(resolveDefaultColor({ embedColor: 0x1000000 }), COLORS.INFO);
  assert.equal(resolveDefaultColor({ embedColor: 0 }), 0);
});

// ── 7. Import parsing ────────────────────────────────────────────────────────

const EXAMPLE = {
  content: 'Hey {user}, here are the rules!',
  embed: {
    title: '📜 Rules', description: '**1.** Be nice.', url: null, color: 5793266,
    author: { name: '{server}', iconUrl: '{server.icon}', url: null },
    footer: { text: 'Thanks for reading', iconUrl: null },
    thumbnail: '{server.icon}', image: null,
    fields: [{ name: 'Contact', value: 'Ask a moderator', inline: false }],
    timestamp: false,
  },
  buttons: [{ label: 'Website', url: 'https://example.com', emoji: '🌐' }],
};

test('parseImportPayload: round trip, Discord shape, Discohook, errors', () => {
  const rt = parseImportPayload(JSON.stringify(EXAMPLE));
  assert.deepEqual(rt.errors, []);
  assert.deepEqual(rt.template, normalizeTemplate(EXAMPLE));
  const again = parseImportPayload(JSON.stringify(rt.template, null, 2));
  assert.deepEqual(again.template, rt.template);

  const discord = parseImportPayload(JSON.stringify({
    content: 'hi',
    embeds: [{ title: 'T' }, { title: 'U' }],
    components: [{ type: 1, components: [
      { type: 2, style: 5, label: 'L', url: 'https://x.y' },
      { type: 2, style: 1, label: 'B', custom_id: 'z' },
    ] }],
  }));
  assert.equal(discord.template.content, 'hi');
  assert.equal(discord.template.embed.title, 'T');
  assert.deepEqual(discord.template.buttons, [{ label: 'L', url: 'https://x.y', emoji: null }]);
  assert.deepEqual(discord.warnings, ['Only the first embed was imported.', 'Only link buttons were imported.']);

  const emojiBtn = parseImportPayload(JSON.stringify({
    components: [{ type: 1, components: [{ type: 2, style: 5, label: 'L', url: 'https://x.y', emoji: { id: '123456789012345678', name: 'wave', animated: true } }] }],
  }));
  assert.equal(emojiBtn.template.buttons[0].emoji, '<a:wave:123456789012345678>');

  const disco = parseImportPayload(JSON.stringify({ messages: [{ data: { content: 'x' } }, { data: { content: 'y' } }] }));
  assert.equal(disco.template.content, 'x');
  assert.deepEqual(disco.warnings, ['Only the first message was imported.']);
  assert.equal(parseImportPayload(JSON.stringify({ messages: [{ data: { content: 'x' } }] })).template.content, 'x');

  assert.deepEqual(parseImportPayload('{').errors, ['That is not valid JSON.']);
  const empty = parseImportPayload('{}');
  assert.equal(empty.template, null);
  assert.ok(empty.errors.length > 0);
  const big = parseImportPayload(' '.repeat(20_481));
  assert.deepEqual(big.errors, ['The JSON must be 20 KB or smaller.']);
  assert.ok(parseImportPayload('[1,2]').errors.length > 0);
  assert.ok(parseImportPayload('"just text"').errors.length > 0);
  assert.ok(parseImportPayload('null').errors.length > 0);
  const tooLong = parseImportPayload(JSON.stringify({ content: 'x'.repeat(2001) }));
  assert.equal(tooLong.template, null);
  assert.ok(tooLong.errors.some((e) => /2000/.test(e)), 'the real error is reported, not "empty"');
  const bare = parseImportPayload(JSON.stringify({ title: 'Bare', description: 'embed' }));
  assert.equal(bare.template.embed.title, 'Bare');
});

// ── 8. Welcome presets ───────────────────────────────────────────────────────

test('buildWelcomePresets', () => {
  const presets = buildWelcomePresets();
  assert.deepEqual(presets.map((p) => p.name), [...PRESET_NAMES]);
  for (const p of presets) assert.deepEqual(validateForSave(p.template), [], p.name);
  assert.ok(!JSON.stringify(presets).includes('{user'), 'no {user…} tokens');
  const links = (inv) => buildWelcomePresets({ inviteUrl: inv }).find((p) => p.name === 'welcome-links').template;
  assert.deepEqual(links('https://discord.gg/abc').buttons, [{ label: 'Invite your friends', url: 'https://discord.gg/abc', emoji: '📨' }]);
  assert.deepEqual(links(null).buttons, []);
  const team = (ids) => buildWelcomePresets({ modRoleIds: ids }).find((p) => p.name === 'welcome-team').template;
  assert.ok(team(['1', '2']).embed.description.includes('<@&1> <@&2>'));
  const many = team(Array.from({ length: 200 }, (_, i) => `${100000000000000000n + BigInt(i)}`));
  assert.ok(many.embed.description.length <= 4096);
  assert.ok(!team([]).embed.description.includes('Moderators'));
});

// ── 9. Unknown variables and hints ───────────────────────────────────────────

test('collectUnknownTokens / unknownVariablesNote / publishHint', () => {
  const guild = makeGuild();
  const tpl = { content: 'Hi {server}', embed: { title: '{nope} and {server.name}', description: '{nope} {random:1:5}' }, buttons: [] };
  assert.deepEqual(collectUnknownTokens(tpl, { guild }), ['{nope}']);
  const many = { content: Array.from({ length: 30 }, (_, i) => `{x${i}}`).join(' ') };
  assert.equal(collectUnknownTokens(many, { guild }).length, 10);
  assert.match(unknownVariablesNote(tpl, { guild }), /Unknown variables are shown as typed: `\{nope\}`/);
  assert.equal(unknownVariablesNote({ content: 'ok {server}' }, { guild }), '');
  assert.equal(publishHint({ name: 'a', channelId: null, messageId: null }), '');
  assert.match(publishHint({ name: 'rules', channelId: '5', messageId: '6' }), /<#5>.*\/embed update name:rules/);
});

test('renderSavedEmbed: default colour, variables, publish mentions', () => {
  const guild = makeGuild({ roles: [{ id: '123456789012345678', mentionable: true }] });
  const channel = makeChannel(guild);
  const member = makeMember(guild);
  const tpl = { content: 'Hello {user} <@&123456789012345678>', embed: { title: 'Welcome to {server}' } };
  const preview = renderSavedEmbed(tpl, { member, guild, channel, settings: { embedColor: 0x112233 } });
  assert.deepEqual(preview.allowedMentions, { parse: [] });
  assert.equal(preview.embeds[0].data.color, 0x112233);
  assert.equal(preview.embeds[0].data.title, 'Welcome to Test Guild');
  const pub = renderSavedEmbed(tpl, { member, guild, channel, settings: null }, { mentions: 'publish' });
  assert.deepEqual(pub.allowedMentions, { parse: ['users'], roles: ['123456789012345678'] });
  assert.equal(pub.embeds[0].data.color, COLORS.INFO);
  assert.equal(renderSavedEmbed({ content: null }, { member, guild, channel }), null);
});

// ── 10. Hook filter, plugin, customIds ───────────────────────────────────────

test('isEmbedDeleteCandidate', () => {
  const client = { user: { id: 'b' } };
  assert.equal(isEmbedDeleteCandidate({ guildId: 'g', client, message: { id: 'm', partial: true } }), true);
  assert.equal(isEmbedDeleteCandidate({ guildId: 'g', client, message: { id: 'm', author: { id: 'b' } } }), true);
  assert.equal(isEmbedDeleteCandidate({ guildId: 'g', client, message: { id: 'm', author: { id: 'u' } } }), false);
  assert.equal(isEmbedDeleteCandidate({ client, message: { id: 'm', partial: true } }), false);
  assert.equal(isEmbedDeleteCandidate({ guildId: 'g', client, message: {} }), false);
});

test('plugin: valid declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('embeds')), []);
  const p = plugins.find((x) => x.id === 'embeds');
  assert.ok(p, 'collectPlugins returns the embeds plugin');
  assert.equal(p.module, 'embeds');
  assert.equal(p.hooks.length, 1);
  assert.equal(p.hooks[0].event, 'messageDelete');
  assert.equal(p.hooks[0].id, 'embeds');
  assert.equal(p.hooks[0].always, true);
  assert.equal(p.components.length, 1);
  assert.equal(p.components[0].type, 'modal');
  assert.equal(p.components[0].prefix, 'embed-modal');
});

test('customIds', () => {
  assert.ok(buildCustomId('embed-modal', 'new', 'a'.repeat(32)).length <= 100);
  assert.deepEqual(parseCustomId('embed-modal:new:rules').params, ['new', 'rules']);
  assert.equal(buildCustomId('embed-modal', 'new', 'rules'), 'embed-modal:new:rules');
});

test('static: no console, no literal customId, no LIMITS fallback in the package files', () => {
  for (const f of ['src/services/embedService.js', 'src/commands/admin/embed.js', 'src/plugins/embeds.js']) {
    const text = readFileSync(resolve(ROOT, f), 'utf8');
    assert.ok(!/\bconsole\./.test(text), `${f}: console`);
    assert.ok(!/setCustomId\(\s*['"`]/.test(text), `${f}: literal setCustomId`);
    assert.ok(!/custom_?[iI]d\s*:\s*['"`]/.test(text), `${f}: literal customId`);
    assert.ok(!/LIMITS\.[A-Z_]+\s*\?\?/.test(text), `${f}: LIMITS fallback`);
  }
});

// ── 11. Commands against fakes (offline: failures before any DB call) ────────

function makeOptions(values = {}, { sub = 'create', group = null } = {}) {
  return {
    getSubcommand: () => sub,
    getSubcommandGroup: () => group,
    getString: (k) => values[k] ?? null,
    getBoolean: (k) => values[k] ?? null,
    getInteger: (k) => values[k] ?? null,
    getChannel: (k) => values[k] ?? null,
    getAttachment: (k) => values[k] ?? null,
    getFocused: () => values.focused,
  };
}

function makeInteraction(guild, over = {}) {
  const calls = [];
  const i = {
    guild, guildId: guild.id, member: makeMember(guild), user: { id: ADMIN, tag: 'admin#0' },
    channel: null, deferred: false, replied: false, createdTimestamp: Date.now(), calls,
    async reply(p) { i.replied = true; calls.push(['reply', p]); return {}; },
    async deferReply(p) { i.deferred = true; calls.push(['deferReply', p]); },
    async editReply(p) { i.replied = true; calls.push(['editReply', p]); return {}; },
    async followUp(p) { calls.push(['followUp', p]); return {}; },
    async showModal(m) { calls.push(['showModal', m]); },
    isRepliable: () => true,
    async respond(c) { calls.push(['respond', c]); },
    responded: false,
    ...over,
  };
  return i;
}

test('execute: an invalid name is answered with E-NAME before any lookup (create)', async () => {
  const guild = makeGuild();
  const i = makeInteraction(guild, { options: makeOptions({ name: '🎉' }, { sub: 'create' }) });
  await cmd.execute(i, { emojis: { cache: new Map() } });
  assert.equal(i.calls.length, 1);
  assert.equal(i.calls[0][0], 'reply');
  assert.equal(i.calls[0][1].ephemeral, true);
  assert.match(i.calls[0][1].embeds[0].data.title, /Invalid Name/);
});

test('execute: field add with an invalid name defers, then answers in the same reply', async () => {
  const guild = makeGuild();
  const i = makeInteraction(guild, { options: makeOptions({ name: '!!!' }, { sub: 'add', group: 'field' }) });
  await cmd.execute(i, {});
  assert.deepEqual(i.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  assert.match(i.calls[1][1].embeds[0].data.title, /Invalid Name/);
});

test('autocomplete: an index lookup without a valid name answers []', async () => {
  const guild = makeGuild();
  const i = makeInteraction(guild, {
    options: { ...makeOptions({}), getFocused: () => ({ name: 'index', value: '' }), getString: () => '' },
  });
  await cmd.autocomplete(i);
  assert.deepEqual(i.calls, [['respond', []]]);
});

// ── 12. DB cases ─────────────────────────────────────────────────────────────

const mk = (extra = {}) => ({ content: 'hello', ...extra });

test('db: createSavedEmbed ensures settings, rejects duplicates and bad templates', { skip: skipDb }, async () => {
  const row = await svc.createSavedEmbed(GUILD, 'rules', mk({ embed: { title: 'Rules' } }), ADMIN);
  assert.equal(row.guildId, GUILD);
  assert.equal(row.name, 'rules');
  assert.equal(row.messageId, null);
  assert.equal((await prisma.guildSettings.findUnique({ where: { guildId: GUILD } })).guildId, GUILD);
  await assert.rejects(svc.createSavedEmbed(GUILD, 'rules', mk(), ADMIN), (e) => e instanceof EmbedEditError && e.code === 'TAKEN');
  await assert.rejects(svc.createSavedEmbed(GUILD, 'empty', emptyTemplate(), ADMIN), (e) => e instanceof EmbedEditError && e.code === 'INVALID' && e.errors.length > 0);
  assert.equal(await svc.getSavedEmbed(GUILD, 'empty'), null);
  assert.equal((await svc.getSavedEmbed(GUILD, 'rules')).id, row.id);
  assert.ok((await svc.countSavedEmbeds(GUILD)) >= 1);
});

test('db: createSavedEmbed enforces the per-guild cap', { skip: skipDb }, async () => {
  await prisma.guildSettings.upsert({ where: { guildId: GUILD2 }, update: {}, create: { guildId: GUILD2 } });
  const tpl = normalizeTemplate(mk());
  await prisma.savedEmbed.createMany({
    data: Array.from({ length: GUILD_LIMITS.SAVED_EMBEDS }, (_, i) => ({ guildId: GUILD2, name: `e${i}`, template: tpl, createdBy: ADMIN })),
  });
  await assert.rejects(svc.createSavedEmbed(GUILD2, 'one-too-many', mk(), ADMIN), (e) => e instanceof EmbedEditError && e.code === 'LIMIT');
  const rows = await svc.listSavedEmbeds(GUILD2);
  assert.equal(rows.length, GUILD_LIMITS.SAVED_EMBEDS);
  assert.deepEqual(rows.slice(0, 2).map((r) => r.name), ['e0', 'e1']);
  await prisma.savedEmbed.deleteMany({ where: { guildId: GUILD2 } });
});

test('db: mutateSavedEmbed serialises concurrent edits and replaceTemplate reports P2025', { skip: skipDb }, async () => {
  await svc.createSavedEmbed(GUILD, 'mut', mk({ embed: { title: 'T' } }), ADMIN);
  await Promise.all([
    svc.mutateSavedEmbed(GUILD, 'mut', (t) => addField(t, { title: 'A', value: '1' })),
    svc.mutateSavedEmbed(GUILD, 'mut', (t) => addField(t, { title: 'B', value: '2' })),
    svc.mutateSavedEmbed(GUILD, 'mut', (t) => addButton(t, { label: 'L', url: 'https://a.b' })),
  ]);
  const row = await svc.getSavedEmbed(GUILD, 'mut');
  const tpl = normalizeTemplate(row.template);
  assert.deepEqual(tpl.embed.fields.map((f) => f.name).sort(), ['A', 'B']);
  assert.equal(tpl.buttons.length, 1);
  await assert.rejects(svc.mutateSavedEmbed(GUILD, 'nope-nope', (t) => t), (e) => e.code === 'NOTFOUND');
  await assert.rejects(svc.mutateSavedEmbed(GUILD, 'mut', () => emptyTemplate()), (e) => e.code === 'INVALID');
  await assert.rejects(svc.mutateSavedEmbed(GUILD, 'mut', (t) => addButton(t, { label: 'x', url: 'ftp://nope' })), EmbedEditError);
  assert.deepEqual(normalizeTemplate((await svc.getSavedEmbed(GUILD, 'mut')).template), tpl, 'a rejected mutation changes nothing');
  await assert.rejects(svc.replaceTemplate(GUILD, 'cnotarealid000000000000000', mk()), (e) => e.code === 'NOTFOUND');
  const replaced = await svc.replaceTemplate(GUILD, row.id, mk({ content: 'replaced' }));
  assert.equal(normalizeTemplate(replaced.template).content, 'replaced');
});

test('db: searchSavedEmbeds is case-insensitive, ordered and never throws', { skip: skipDb }, async () => {
  for (const n of ['search-b', 'search-a', 'Search-c-extra']) {
    await svc.createSavedEmbed(GUILD, normalizeEmbedName(n), mk(), ADMIN);
  }
  const found = await svc.searchSavedEmbeds(GUILD, 'SEARCH');
  assert.deepEqual(found.map((r) => r.name), ['search-a', 'search-b', 'search-c-extra']);
  assert.deepEqual(Object.keys(found[0]).sort(), ['channelId', 'id', 'messageId', 'name']);
  assert.equal((await svc.searchSavedEmbeds(GUILD, 'search', 2)).length, 2);
  assert.deepEqual(await svc.searchSavedEmbeds(GUILD, 'zzzzzz-none'), []);
  assert.ok((await svc.searchSavedEmbeds(GUILD, '')).length >= 3, 'an empty query lists');
  assert.deepEqual(await svc.searchSavedEmbeds('955014999999999999', 'search'), [], 'other guilds see nothing');
});

test('db: getSavedEmbedTemplate normalises the name and never throws', { skip: skipDb }, async () => {
  await svc.createSavedEmbed(GUILD, 'my-rules', mk({ content: 'the rules' }), ADMIN);
  assert.equal((await svc.getSavedEmbedTemplate(GUILD, 'My Rules')).content, 'the rules');
  assert.equal((await svc.getSavedEmbedTemplate(GUILD, 'my-rules')).content, 'the rules');
  assert.equal(await svc.getSavedEmbedTemplate(GUILD, 'does-not-exist'), null);
  assert.equal(await svc.getSavedEmbedTemplate(GUILD, '🎉'), null);
  assert.equal(await svc.getSavedEmbedTemplate(null, 'my-rules'), null);
});

test('db: the messageDelete hook body clears messageId only', { skip: skipDb }, async () => {
  const row = await svc.createSavedEmbed(GUILD, 'hooked', mk(), ADMIN);
  await svc.setPublished(row.id, { channelId: '900000000000000500', messageId: '800000000000099001' });
  await svc.onEmbedMessageDelete({ guildId: GUILD, message: { id: '800000000000099999' } });
  assert.equal((await svc.getSavedEmbed(GUILD, 'hooked')).messageId, '800000000000099001', 'another message: untouched');
  await svc.onEmbedMessageDelete({ guildId: '955014999999999999', message: { id: '800000000000099001' } });
  assert.equal((await svc.getSavedEmbed(GUILD, 'hooked')).messageId, '800000000000099001', 'another guild: untouched');
  await svc.onEmbedMessageDelete({ guildId: GUILD, message: { id: '800000000000099001' } });
  const after = await svc.getSavedEmbed(GUILD, 'hooked');
  assert.equal(after.messageId, null);
  assert.equal(after.channelId, '900000000000000500', 'the channel stays as a hint');
});

test('db: setPublished swallows a deleted row; deleteSavedEmbedAndMessage on a gone row', { skip: skipDb }, async () => {
  const row = await svc.createSavedEmbed(GUILD, 'doomed', mk(), ADMIN);
  const guild = makeGuild();
  const channel = makeChannel(guild);
  const msg = await channel.send({ content: 'x' });
  await svc.setPublished(row.id, { channelId: channel.id, messageId: msg.id });
  const published = await svc.getSavedEmbed(GUILD, 'doomed');
  const first = await svc.deleteSavedEmbedAndMessage({ guild, row: published, deleteMessage: true });
  assert.deepEqual(first, { deleted: true, messageDeleted: true });
  assert.deepEqual(channel.deleted, [msg.id]);
  assert.equal(await svc.getSavedEmbed(GUILD, 'doomed'), null);
  const again = await svc.deleteSavedEmbedAndMessage({ guild, row: published, deleteMessage: true });
  assert.deepEqual(again, { deleted: false, messageDeleted: false });
  await svc.setPublished(row.id, { channelId: channel.id, messageId: 'whatever' }); // P2025 swallowed
  const keep = await svc.createSavedEmbed(GUILD, 'kept-msg', mk(), ADMIN);
  const noMsg = await svc.deleteSavedEmbedAndMessage({ guild, row: keep, deleteMessage: true });
  assert.deepEqual(noMsg, { deleted: true, messageDeleted: false });
});

test('db: publishSavedEmbed posts, tracks, reports refused roles and replaced copies', { skip: skipDb }, async () => {
  const R1 = '123456789012345678';
  const guild = makeGuild({ roles: [{ id: R1, mentionable: false }] });
  const channel = makeChannel(guild);
  const member = makeMember(guild, { id: '900000000000000011', perms: ['ViewChannel'] }); // not the owner: the owner sees every channel
  const row = await svc.createSavedEmbed(GUILD, 'pub', { content: `ping <@&${R1}>`, embed: { title: 'T' }, buttons: [{ label: 'L', url: 'https://a.b' }] }, ADMIN);

  const noView = makeChannel(guild, { id: '900000000000000502', memberPerms: [] });
  assert.equal((await svc.publishSavedEmbed({ row, channel: noView, member })).code, 'MEMBER_NO_ACCESS');
  const noBot = makeChannel(guild, { id: '900000000000000503', botPerms: ['ViewChannel'] });
  const denied = await svc.publishSavedEmbed({ row, channel: noBot, member });
  assert.equal(denied.code, 'CHANNEL');
  assert.deepEqual(denied.missing.sort(), ['EmbedLinks', 'SendMessages']);

  const ok = await svc.publishSavedEmbed({ row, channel, member });
  assert.equal(ok.ok, true);
  assert.deepEqual(ok.refused, [R1]);
  assert.equal(ok.previous, null);
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: ['users'], roles: [] });
  assert.equal(channel.sent[0].embeds.length, 1);
  assert.equal(channel.sent[0].components.length, 1);
  const tracked = await svc.getSavedEmbed(GUILD, 'pub');
  assert.equal(tracked.messageId, ok.message.id);
  assert.equal(tracked.channelId, channel.id);

  const second = await svc.publishSavedEmbed({ row: tracked, channel, member });
  assert.deepEqual(second.previous, { channelId: channel.id, messageId: ok.message.id });
  assert.equal(channel.fetched.has(ok.message.id), true, 'the previous message is not deleted');

  const rejecting = { ...channel, send: async () => { throw apiError(50035, 'Invalid Form Body'); } };
  const refused = await svc.publishSavedEmbed({ row: tracked, channel: rejecting, member });
  assert.equal(refused.code, 'DISCORD');
  const missingPerm = { ...channel, send: async () => { throw apiError(50013, 'Missing Permissions'); } };
  assert.equal((await svc.publishSavedEmbed({ row: tracked, channel: missingPerm, member })).code, 'CHANNEL');

  const emptyRow = await svc.createSavedEmbed(GUILD, 'pub-empty', { content: '{channel.name}' }, ADMIN);
  const emptyChannel = { ...channel, name: '' };
  const rendered = await svc.publishSavedEmbed({ row: emptyRow, channel: emptyChannel, member });
  assert.equal(rendered.code, 'EMPTY');
});

test('db: updatePublishedEmbed edits in place and heals deleted messages and channels', { skip: skipDb }, async () => {
  const guild = makeGuild();
  const channel = makeChannel(guild);
  const member = makeMember(guild);
  const row = await svc.createSavedEmbed(GUILD, 'upd', { embed: { title: 'Old' } }, ADMIN);

  assert.equal((await svc.updatePublishedEmbed({ row, guild, member })).code, 'NOT_PUBLISHED');
  const pub = await svc.publishSavedEmbed({ row, channel, member });
  await svc.mutateSavedEmbed(GUILD, 'upd', (t) => setImages(setTitleUrl(t, 'https://a.b'), { thumbnail: 'https://a.b/t.png' }));
  const fresh = await svc.getSavedEmbed(GUILD, 'upd');
  const done = await svc.updatePublishedEmbed({ row: fresh, guild, member });
  assert.equal(done.ok, true);
  const edit = pub.message.edits.at(-1);
  assert.equal(edit.content, '', 'an empty content clears old text');
  assert.deepEqual(edit.allowedMentions, { parse: [] });
  assert.equal(edit.embeds[0].data.url, 'https://a.b');
  assert.ok(Array.isArray(edit.components));

  // The message is gone.
  channel.fetched.delete(pub.message.id);
  const gone = await svc.updatePublishedEmbed({ row: fresh, guild, member });
  assert.equal(gone.code, 'MESSAGE_GONE');
  const draft = await svc.getSavedEmbed(GUILD, 'upd');
  assert.equal(draft.messageId, null);
  assert.equal(draft.channelId, channel.id);
  assert.equal((await svc.updatePublishedEmbed({ row: draft, guild, member })).code, 'NOT_PUBLISHED');

  // The channel is gone (REST 10003 on a cache miss).
  await svc.setPublished(row.id, { channelId: '900000000000009999', messageId: '800000000000099777' });
  const lost = await svc.updatePublishedEmbed({ row: await svc.getSavedEmbed(GUILD, 'upd'), guild, member });
  assert.equal(lost.code, 'CHANNEL_GONE');
  const cleared = await svc.getSavedEmbed(GUILD, 'upd');
  assert.equal(cleared.channelId, null);
  assert.equal(cleared.messageId, null);

  // Someone else's message is never edited.
  const foreign = await channel.send({ content: 'not mine' });
  foreign.author = { id: 'somebody-else' };
  await svc.setPublished(row.id, { channelId: channel.id, messageId: foreign.id });
  const notMine = await svc.updatePublishedEmbed({ row: await svc.getSavedEmbed(GUILD, 'upd'), guild, member });
  assert.equal(notMine.code, 'MESSAGE_GONE');
  assert.equal(foreign.edits.length, 0);

  // An archived thread and a missing bot permission.
  const again = await svc.publishSavedEmbed({ row: await svc.getSavedEmbed(GUILD, 'upd'), channel, member });
  again.message.edit = async () => { throw apiError(50083, 'Thread is archived'); };
  assert.equal((await svc.updatePublishedEmbed({ row: await svc.getSavedEmbed(GUILD, 'upd'), guild, member })).code, 'ARCHIVED');
  const blind = makeChannel(guild, { botPerms: ['ViewChannel'] });
  Object.assign(blind, { id: channel.id });
  guild.channels.cache.set(channel.id, blind);
  const noAccess = await svc.updatePublishedEmbed({ row: await svc.getSavedEmbed(GUILD, 'upd'), guild, member });
  assert.equal(noAccess.code, 'NO_ACCESS');
  assert.ok(noAccess.missing.includes('EmbedLinks'));
  assert.ok((await svc.getSavedEmbed(GUILD, 'upd')).messageId, 'nothing is cleared when only a permission is missing');
});

function formInteraction(guild, { customId, values, member = makeMember(guild) }) {
  const i = makeInteraction(guild, {
    member, customId, channel: makeChannel(guild, { id: '900000000000000600' }),
    inCachedGuild: () => true,
    fields: { getTextInputValue: (id) => { if (!(id in values)) throw new Error('missing'); return values[id]; } },
  });
  return i;
}
const FORM = (o = {}) => ({ 'tpl-content': '', 'tpl-title': '', 'tpl-description': '', 'tpl-color': '', 'tpl-footer': '', ...o });

test('db: the form creates, edits and keeps what the form does not hold', { skip: skipDb }, async () => {
  const guild = makeGuild();
  const created = formInteraction(guild, {
    customId: buildCustomId('embed-modal', 'new', 'formed'),
    values: FORM({ 'tpl-title': 'Hello {nope}', 'tpl-description': 'Body', 'tpl-color': '#ff0000' }),
  });
  await svc.handleEmbedModalSubmit(created);
  assert.deepEqual(created.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  const okEmbed = created.calls[1][1].embeds[0];
  assert.match(okEmbed.data.title, /Embed Created/);
  assert.match(okEmbed.data.description, /\{nope\}/);
  assert.equal(okEmbed.data.fields[0].name, 'Variables');
  const row = await svc.getSavedEmbed(GUILD, 'formed');
  assert.equal(normalizeTemplate(row.template).embed.color, 0xff0000);

  // Add parts the form does not hold, then edit through the form: they survive.
  await svc.mutateSavedEmbed(GUILD, 'formed', (t) => setAuthor(addButton(addField(t, { title: 'F', value: 'v' }), { label: 'B', url: 'https://a.b' }), { text: 'Auth' }));
  await svc.setPublished(row.id, { channelId: '900000000000000500', messageId: '800000000000099500' });
  const edited = formInteraction(guild, {
    customId: buildCustomId('embed-modal', row.id),
    values: FORM({ 'tpl-content': 'Text', 'tpl-title': 'New title', 'tpl-description': '' }),
  });
  await svc.handleEmbedModalSubmit(edited);
  const reply = edited.calls.at(-1)[1].embeds[0];
  assert.match(reply.data.title, /Embed Saved/);
  assert.match(reply.data.description, /\/embed update name:formed/, 'the publish hint is shown');
  const tpl = normalizeTemplate((await svc.getSavedEmbed(GUILD, 'formed')).template);
  assert.equal(tpl.content, 'Text');
  assert.equal(tpl.embed.title, 'New title');
  assert.equal(tpl.embed.description, undefined, 'an empty input clears that part');
  assert.equal(tpl.embed.fields.length, 1);
  assert.equal(tpl.embed.author.name, 'Auth');
  assert.equal(tpl.buttons.length, 1);
  assert.equal(tpl.embed.color, undefined, 'a blank colour input clears it');

  // A rejected form hands the typed text back as a draft file.
  const bad = formInteraction(guild, {
    customId: buildCustomId('embed-modal', row.id),
    values: FORM({ 'tpl-content': 'keep me', 'tpl-color': 'not-a-colour' }),
  });
  await svc.handleEmbedModalSubmit(bad);
  const badReply = bad.calls.at(-1)[1];
  assert.match(badReply.embeds[0].data.title, /Invalid Embed/);
  assert.equal(badReply.files[0].name, 'embed-draft.json');
  assert.match(badReply.files[0].attachment.toString('utf8'), /keep me/);
  assert.equal(normalizeTemplate((await svc.getSavedEmbed(GUILD, 'formed')).template).content, 'Text', 'nothing was saved');

  // Name taken: the draft is kept for the admin.
  const taken = formInteraction(guild, { customId: buildCustomId('embed-modal', 'new', 'formed'), values: FORM({ 'tpl-content': 'again' }) });
  await svc.handleEmbedModalSubmit(taken);
  assert.match(taken.calls.at(-1)[1].embeds[0].data.title, /Name Taken/);
  assert.match(taken.calls.at(-1)[1].files[0].attachment.toString('utf8'), /again/);

  // Deleted while the form was open.
  const orphan = formInteraction(guild, { customId: buildCustomId('embed-modal', 'cnotarealid000000000000000'), values: FORM({ 'tpl-content': 'late' }) });
  await svc.handleEmbedModalSubmit(orphan);
  assert.match(orphan.calls.at(-1)[1].embeds[0].data.title, /Embed Not Found/);
  assert.match(orphan.calls.at(-1)[1].files[0].attachment.toString('utf8'), /late/);

  // An empty create form and a non-admin submitter.
  const empty = formInteraction(guild, { customId: buildCustomId('embed-modal', 'new', 'blank'), values: FORM() });
  await svc.handleEmbedModalSubmit(empty);
  assert.match(empty.calls.at(-1)[1].embeds[0].data.title, /Invalid Embed/);
  assert.equal(await svc.getSavedEmbed(GUILD, 'blank'), null);
  const nobody = formInteraction(guild, {
    customId: buildCustomId('embed-modal', 'new', 'sneaky'), values: FORM({ 'tpl-content': 'x' }), member: makeMember(guild, { id: '900000000000000077', perms: [] }),
  });
  await svc.handleEmbedModalSubmit(nobody);
  assert.deepEqual(nobody.calls.map((c) => c[0]), ['reply']);
  assert.match(nobody.calls[0][1].embeds[0].data.title, /Insufficient Permissions/);
  assert.equal(await svc.getSavedEmbed(GUILD, 'sneaky'), null);
});

function welcomeInteraction(guild, options) {
  return makeInteraction(guild, { options: makeOptions(options, { sub: 'welcome-channel' }), channel: makeChannel(guild, { id: '900000000000000700', memberPerms: ['ViewChannel'] }) });
}

test('db: welcome-channel creates a read-only channel, posts four presets and never overwrites', { skip: skipDb }, async () => {
  await prisma.savedEmbed.deleteMany({ where: { guildId: GUILD, name: { in: [...PRESET_NAMES] } } });
  const guild = makeGuild({ roles: [{ id: '123456789012345678', mentionable: false }] });
  let createArgs = null;
  guild.channels.create = async (args) => {
    createArgs = args;
    return makeChannel(guild, { id: '900000000000000800', memberPerms: ['ViewChannel'] });
  };
  const interaction = welcomeInteraction(guild, {});
  const result = await svc.setUpWelcomeChannel({ interaction, create: true, channel: null, settings: { modRoleIds: ['123456789012345678', '999999999999999999'] } });
  assert.equal(result.ok, true, JSON.stringify({ code: result.code }));
  assert.equal(result.createdChannel, true);
  assert.deepEqual(result.createdPresets, [...PRESET_NAMES]);
  assert.equal(result.invite.source, 'created');
  assert.equal(createArgs.name, 'welcome');
  assert.equal(createArgs.position, 0);
  assert.ok(createArgs.permissionOverwrites[0].deny.includes(PermissionFlagsBits.SendMessages));
  assert.equal(createArgs.permissionOverwrites[0].id, guild.id);
  const channel = guild.channels.cache.get('900000000000000800');
  assert.equal(channel.sent.length, 4);
  for (const payload of channel.sent) assert.deepEqual(payload.allowedMentions, { parse: [] });
  const team = channel.sent[3].embeds[0].data.description;
  assert.match(team, /<@&123456789012345678>/);
  assert.ok(!team.includes('999999999999999999'), 'unknown roles are left out');
  assert.equal(channel.sent[2].components.length, 1, 'the links preset carries the invite button');
  for (const name of PRESET_NAMES) {
    const row = await svc.getSavedEmbed(GUILD, name);
    assert.equal(row.channelId, '900000000000000800');
    assert.ok(row.messageId);
  }

  // An admin edits a preset; running it again elsewhere reuses (not overwrites) every preset.
  await svc.mutateSavedEmbed(GUILD, 'welcome-rules', (t) => setColor(t, 0x00ff00));
  const again = await svc.setUpWelcomeChannel({ interaction, create: true, channel: null, settings: null });
  assert.deepEqual(again, { ok: false, code: 'ALREADY', channelId: '900000000000000800' });
  const elsewhere = makeChannel(guild, { id: '900000000000000801', everyoneCanSend: true });
  const reused = await svc.setUpWelcomeChannel({ interaction, create: true, channel: elsewhere, settings: null });
  assert.equal(reused.ok, true);
  assert.deepEqual(reused.createdPresets, []);
  assert.equal(reused.reusedPresets.length, 4);
  assert.match(reused.tip, /make it read-only/);
  assert.equal(elsewhere.sent[1].embeds[0].data.color, 0x00ff00, 'the edited preset was posted as edited');

  // Refusals: missing permissions create nothing, a failing send reports what was posted.
  const poor = makeGuild({ botPerms: ['ViewChannel', 'SendMessages'] });
  poor.channels.create = async () => { throw new Error('must not be called'); };
  const refused = await svc.setUpWelcomeChannel({ interaction: welcomeInteraction(poor, {}), create: true, channel: null, settings: null });
  assert.equal(refused.code, 'BOT_PERMS');
  assert.ok(refused.missing.includes('ManageChannels'));
  const flaky = makeChannel(guild, { id: '900000000000000802' });
  let n = 0;
  flaky.send = async (p) => { if (++n === 3) throw apiError(50013, 'Missing Permissions'); return { id: `8000000000009${n}`, url: 'u', p }; };
  const partial = await svc.setUpWelcomeChannel({ interaction, create: true, channel: flaky, settings: null });
  assert.equal(partial.code, 'PARTIAL');
  assert.deepEqual(partial.posted, ['welcome-greeting', 'welcome-rules']);
  assert.deepEqual(partial.unposted, ['welcome-links', 'welcome-team']);
});

test('db: welcome-channel refuses before creating a channel when the embed cap leaves no room', { skip: skipDb }, async () => {
  await prisma.guildSettings.upsert({ where: { guildId: GUILD2 }, update: {}, create: { guildId: GUILD2 } });
  const tpl = normalizeTemplate(mk());
  await prisma.savedEmbed.createMany({
    data: Array.from({ length: GUILD_LIMITS.SAVED_EMBEDS - 2 }, (_, i) => ({ guildId: GUILD2, name: `fill${i}`, template: tpl, createdBy: ADMIN })),
  });
  const guild = makeGuild({ id: GUILD2 });
  guild.channels.create = async () => { throw new Error('must not be called'); };
  const out = await svc.setUpWelcomeChannel({ interaction: welcomeInteraction(guild, {}), create: true, channel: null, settings: null });
  assert.equal(out.code, 'LIMIT');
  assert.equal(out.needed, 4);
  assert.equal(out.count, GUILD_LIMITS.SAVED_EMBEDS - 2);
  await prisma.savedEmbed.deleteMany({ where: { guildId: GUILD2 } });
});

test('db: /embed command flows (field, button, color, preview, list, delete, export, import)', { skip: skipDb }, async () => {
  const guild = makeGuild();
  const client = { emojis: { cache: new Map() } };
  const channel = makeChannel(guild, { id: '900000000000000900' });
  const run = async (sub, group, values, over = {}) => {
    const i = makeInteraction(guild, { options: makeOptions(values, { sub, group }), channel, ...over });
    await cmd.execute(i, client);
    return i;
  };
  const lastEmbed = (i) => {
    const c = i.calls.filter((x) => x[0] === 'editReply' || x[0] === 'followUp' || x[0] === 'reply').at(-1)[1];
    return c.embeds?.[0]?.data ?? c;
  };

  await svc.createSavedEmbed(GUILD, 'flow', { content: 'hi {nope}', embed: { title: 'Flow' } }, ADMIN);
  let i = await run('add', 'field', { name: 'Flow', title: 'F1', value: 'V1', inline: true });
  assert.match(lastEmbed(i).title, /Field Added/);
  assert.match(lastEmbed(i).description, /Added field 1 to \*\*flow\*\*/);
  assert.match(lastEmbed(i).description, /Unknown variables are shown as typed: `\{nope\}`/);
  i = await run('add', 'field', { name: 'flow', title: 'F0', value: 'V0', position: 1 });
  assert.match(lastEmbed(i).description, /Added field 1/);
  i = await run('edit', 'field', { name: 'flow', index: 1 });
  assert.match(lastEmbed(i).title, /Nothing To Change/);
  i = await run('edit', 'field', { name: 'flow', index: 1, position: 2, title: 'F0b' });
  assert.match(lastEmbed(i).description, /Updated field 2/);
  i = await run('remove', 'field', { name: 'flow', index: 1 });
  assert.match(lastEmbed(i).description, /Removed field 1 \("F1"\)/);
  i = await run('remove', 'field', { name: 'flow', index: 9 });
  assert.match(lastEmbed(i).title, /Could Not Save/);
  assert.match(lastEmbed(i).description, /There is no field 9/);
  i = await run('list', 'field', { name: 'flow' });
  assert.match(lastEmbed(i).description, /F0b/);
  assert.equal(lastEmbed(i).footer.text, '1/25 fields');

  i = await run('add', 'button', { name: 'flow', label: 'Site', url: 'https://example.com', emoji: '🌐' });
  assert.match(lastEmbed(i).title, /Button Added/);
  i = await run('add', 'button', { name: 'flow', label: 'Bad', url: 'ftp://nope' });
  assert.match(lastEmbed(i).title, /Could Not Save/);
  i = await run('add', 'button', { name: 'flow', label: 'Bad', url: 'https://a.b', emoji: 'abc' });
  assert.match(lastEmbed(i).description, /valid emoji/);
  i = await run('remove', 'button', { name: 'flow', index: 1 });
  assert.match(lastEmbed(i).title, /Button Removed/);

  i = await run('color', null, { name: 'flow', color: 'blurple' });
  assert.match(lastEmbed(i).description, /#5865F2/);
  i = await run('color', null, { name: 'flow', color: 'default' });
  assert.match(lastEmbed(i).description, /server default colour/);
  i = await run('color', null, { name: 'flow', color: 'zzz' });
  assert.match(lastEmbed(i).title, /Unknown Colour/);
  i = await run('image', null, { name: 'flow' });
  assert.match(lastEmbed(i).title, /Nothing To Change/);
  i = await run('image', null, { name: 'flow', image_url: 'https://a.b/i.png', thumbnail_url: '{server.icon}' });
  assert.match(lastEmbed(i).title, /Images Updated/);
  i = await run('image', null, { name: 'flow', image_url: 'javascript:alert(1)' });
  assert.match(lastEmbed(i).title, /Could Not Save/);
  i = await run('author', null, { name: 'flow', text: 'Me', icon_url: '{user.avatar}' });
  assert.match(lastEmbed(i).title, /Author Updated/);
  i = await run('author', null, { name: 'flow', text: 'none' });
  assert.match(lastEmbed(i).title, /Author Removed/);
  i = await run('footer', null, { name: 'flow', text: 'Foot', timestamp: true });
  assert.match(lastEmbed(i).title, /Footer Updated/);
  i = await run('title-url', null, { name: 'flow', url: 'https://a.b/t' });
  assert.match(lastEmbed(i).title, /Title Link Updated/);
  i = await run('title-url', null, { name: 'flow', url: 'ftp://a.b' });
  assert.match(lastEmbed(i).title, /Could Not Save/);

  i = await run('preview', null, { name: 'flow' });
  assert.deepEqual(i.calls[0], ['deferReply', { ephemeral: true }]);
  const preview = i.calls.find((c) => c[0] === 'editReply')[1];
  assert.equal(preview.embeds[0].data.title, 'Flow');
  assert.equal(preview.embeds[0].data.footer.text, 'Foot');
  assert.deepEqual(preview.allowedMentions, { parse: [] });
  assert.ok(i.calls.some((c) => c[0] === 'followUp' && c[1].ephemeral && /Unknown Variables/.test(c[1].embeds[0].data.title)));
  i = await run('preview', null, { name: 'missing-embed' });
  assert.match(lastEmbed(i).title, /Embed Not Found/);

  i = await run('publish', null, { name: 'flow', channel });
  assert.match(lastEmbed(i).title, /Embed Published/);
  assert.equal(channel.sent.length, 1);
  i = await run('add', 'field', { name: 'flow', title: 'F9', value: 'V9' });
  assert.match(lastEmbed(i).description, /It is published in <#900000000000000900>/);
  i = await run('update', null, { name: 'flow' });
  assert.match(lastEmbed(i).title, /Embed Updated/);
  i = await run('update', null, { name: 'search-a' });
  assert.match(lastEmbed(i).title, /Not Published/);

  i = await run('export', null, { name: 'flow' });
  const exported = i.calls.find((c) => c[0] === 'editReply')[1];
  assert.equal(exported.files[0].name, 'embed-flow.json');
  const json = exported.files[0].attachment.toString('utf8');
  assert.deepEqual(parseImportPayload(json).template, normalizeTemplate((await svc.getSavedEmbed(GUILD, 'flow')).template));

  i = await run('import', null, { name: 'flow' });
  assert.match(lastEmbed(i).title, /Nothing To Import/);
  i = await run('import', null, { name: 'flow', json: '{}', file: { size: 1, url: 'http://x' } });
  assert.match(lastEmbed(i).title, /Pick One/);
  i = await run('import', null, { name: 'imported', json: '{ nope' });
  assert.match(lastEmbed(i).title, /Invalid JSON/);
  i = await run('import', null, { name: 'imported', json });
  assert.match(lastEmbed(i).title, /Embed Imported/);
  i = await run('import', null, { name: 'imported', json });
  assert.match(lastEmbed(i).title, /Name Taken/);
  assert.match(lastEmbed(i).description, /overwrite:true/);
  i = await run('import', null, { name: 'imported', json: JSON.stringify({ content: 'replaced' }), overwrite: true });
  assert.match(lastEmbed(i).title, /Embed Replaced/);
  assert.equal(normalizeTemplate((await svc.getSavedEmbed(GUILD, 'imported')).template).content, 'replaced');
  i = await run('import', null, { name: 'big', file: { size: 99_999, url: 'http://x' } });
  assert.match(lastEmbed(i).title, /File Too Large/);

  // create / edit answer with a form; a taken name and a missing embed answer ephemerally.
  i = await run('create', null, { name: 'Brand New' });
  assert.equal(i.calls[0][0], 'showModal');
  assert.equal(i.calls[0][1].toJSON().custom_id, 'embed-modal:new:brand-new');
  assert.equal(await svc.getSavedEmbed(GUILD, 'brand-new'), null, 'nothing is written before the form is submitted');
  i = await run('create', null, { name: 'flow' });
  assert.match(lastEmbed(i).title, /Name Taken/);
  i = await run('edit', null, { name: 'flow' });
  assert.equal(i.calls[0][0], 'showModal');
  const flow = await svc.getSavedEmbed(GUILD, 'flow');
  assert.equal(i.calls[0][1].toJSON().custom_id, `embed-modal:${flow.id}`);
  i = await run('edit', null, { name: 'nothing-here' });
  assert.match(lastEmbed(i).title, /Embed Not Found/);
  await svc.mutateSavedEmbed(GUILD, 'flow', (t) => ({ ...t, embed: { ...t.embed, description: 'd'.repeat(4096) } }));
  i = await run('edit', null, { name: 'flow' });
  assert.match(lastEmbed(i).title, /Too Long For The Form/);
  assert.match(lastEmbed(i).description, /description/);
  await svc.mutateSavedEmbed(GUILD, 'flow', (t) => ({ ...t, embed: { ...t.embed, description: 'short' } }));

  // delete: confirm is answered by the collector in production; here the cancelled path and the missing path.
  i = await run('delete', null, { name: 'ghost-embed' });
  assert.match(lastEmbed(i).title, /Embed Not Found/);

  // autocomplete
  const ac = makeInteraction(guild, { options: { ...makeOptions({}), getFocused: () => ({ name: 'name', value: 'FLO' }) } });
  await cmd.autocomplete(ac);
  assert.deepEqual(ac.calls[0], ['respond', [{ name: 'flow · published', value: 'flow' }]]);
  const idx = makeInteraction(guild, {
    options: { ...makeOptions({ name: 'Flow' }, { group: 'field' }), getFocused: () => ({ name: 'index', value: 'f0' }) },
  });
  await cmd.autocomplete(idx);
  assert.deepEqual(idx.calls[0][1].map((c) => c.value), [1]);
  assert.match(idx.calls[0][1][0].name, /^1\. F0b/);
});

test('db: /embed list shows published and draft embeds, and pages after 15', { skip: skipDb }, async () => {
  await prisma.guildSettings.upsert({ where: { guildId: GUILD2 }, update: {}, create: { guildId: GUILD2 } });
  const guild = makeGuild({ id: GUILD2 });
  const run = async () => {
    const i = makeInteraction(guild, { options: makeOptions({}, { sub: 'list' }) });
    // A message the paginator can attach a collector to.
    i.editReply = async (p) => { i.calls.push(['editReply', p]); return { createMessageComponentCollector: () => ({ on() {}, stop() {} }) }; };
    await cmd.execute(i, {});
    return i;
  };
  let i = await run();
  assert.match(i.calls.at(-1)[1].embeds[0].data.title, /No Saved Embeds/);

  const a = await svc.createSavedEmbed(GUILD2, 'a-draft', mk(), ADMIN);
  const b = await svc.createSavedEmbed(GUILD2, 'b-live', mk(), ADMIN);
  await svc.setPublished(b.id, { channelId: '900000000000000500', messageId: '800000000000000123' });
  i = await run();
  assert.deepEqual(i.calls.map((c) => c[0]), ['deferReply', 'editReply']);
  const page = i.calls[1][1];
  assert.equal(page.components.length, 0, 'a single page has no buttons');
  const text = page.embeds[0].toJSON().description;
  assert.match(text, /`a-draft` · 📝 draft · edited <t:\d+:R>/);
  assert.ok(text.includes('`b-live` · 📤 <#900000000000000500> [jump](https://discord.com/channels/955014000000000002/900000000000000500/800000000000000123)'));
  assert.ok(page.embeds[0].toJSON().title.includes('Saved embeds (2/500)'));
  assert.ok(a.id);

  for (let n = 0; n < 15; n++) await svc.createSavedEmbed(GUILD2, `c-${String(n).padStart(2, '0')}`, mk(), ADMIN);
  i = await run();
  const first = i.calls[1][1];
  assert.equal(first.components.length, 1, 'more than 15 rows: Prev/Stop/Next');
  assert.equal(first.embeds[0].toJSON().description.split('\n').length, 15);
  await prisma.savedEmbed.deleteMany({ where: { guildId: GUILD2 } });
});

test('db: /embed delete asks first, then removes the row and optionally the message', { skip: skipDb }, async () => {
  const { EventEmitter } = await import('node:events');
  const guild = makeGuild();
  const channel = makeChannel(guild, { id: '900000000000000950' });
  const member = makeMember(guild);
  const row = await svc.createSavedEmbed(GUILD, 'to-delete', mk({ embed: { title: 'Bye' } }), ADMIN);
  const pub = await svc.publishSavedEmbed({ row, channel, member });
  assert.equal(pub.ok, true);

  const runDelete = async (answer, values) => {
    const i = makeInteraction(guild, { options: makeOptions(values, { sub: 'delete' }), channel });
    const collector = new EventEmitter();
    const message = {
      id: 'confirm-message',
      createMessageComponentCollector: () => {
        setImmediate(() => {
          const yes = i.calls[0][1].components[0].toJSON().components[0].custom_id;
          const no = i.calls[0][1].components[0].toJSON().components[1].custom_id;
          collector.emit('collect', { customId: answer ? yes : no, update: async () => {} });
          collector.emit('end');
        });
        return collector;
      },
    };
    i.reply = async (p) => { i.replied = true; i.calls.push(['reply', p]); return { resource: { message } }; };
    await cmd.execute(i, {});
    return i;
  };

  let i = await runDelete(false, { name: 'to-delete', delete_message: true });
  assert.equal(i.calls[0][0], 'reply');
  assert.equal(i.calls[0][1].ephemeral, true);
  assert.match(i.calls[0][1].embeds[0].data.description, /published message will be deleted too/);
  assert.ok(await svc.getSavedEmbed(GUILD, 'to-delete'), 'cancelled: nothing deleted');
  assert.equal(i.calls.some((c) => c[0] === 'followUp'), false);

  i = await runDelete(true, { name: 'to-delete', delete_message: true });
  const done = i.calls.find((c) => c[0] === 'followUp');
  assert.match(done[1].embeds[0].data.title, /Embed Deleted/);
  assert.match(done[1].embeds[0].data.description, /published message was deleted too/);
  assert.notEqual(done[1].ephemeral, true, 'the result is public');
  assert.equal(await svc.getSavedEmbed(GUILD, 'to-delete'), null);
  assert.deepEqual(channel.deleted, [pub.message.id]);

  // Keep the message: the row goes, the post stays.
  const row2 = await svc.createSavedEmbed(GUILD, 'to-delete-2', mk(), ADMIN);
  const pub2 = await svc.publishSavedEmbed({ row: row2, channel, member });
  i = await runDelete(true, { name: 'to-delete-2' });
  assert.match(i.calls[0][1].embeds[0].data.description, /stays but will no longer be tracked/);
  assert.equal(await svc.getSavedEmbed(GUILD, 'to-delete-2'), null);
  assert.equal(channel.fetched.has(pub2.message.id), true);
});
