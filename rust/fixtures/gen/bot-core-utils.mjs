// Reference data for bot-core's Discord-facing utilities, produced by the JS bot's own code (no servers needed):
// embed factories, permission names, customEmbed (normalise, validate, render, modal build and parse, JSON import),
// the paginator's pages and buttons, the confirm prompt, autocomplete lists, escapeMarkdown, newTargets' pure
// helpers, and discord.js permission / role / CDN-URL results on a real discord.js Guild built from a raw
// GUILD_CREATE payload (the same payload the Rust test feeds to serenity's cache).
//   node rust/fixtures/gen/bot-core-utils.mjs   -> rust/fixtures/golden/bot-core/utils.json
import { writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = join(ROOT, 'rust', 'fixtures', 'golden', 'bot-core', 'utils.json');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';

const djs = await import('discord.js');
const { Client, ChannelType, PermissionFlagsBits, PermissionsBitField, ApplicationCommandOptionType, ButtonStyle } = djs;
const E = await imp('src', 'utils', 'embedBuilder.js');
const CE = await imp('src', 'utils', 'customEmbed.js');
const IH = await imp('src', 'utils', 'interactionHelpers.js');
const PG = await imp('src', 'utils', 'paginator.js');
const NT = await imp('src', 'utils', 'newTargets.js');
const PV = await imp('src', 'utils', 'privacyGuards.js');
const counting = await imp('src', 'commands', 'admin', 'counting.js');
const { buildCommandPayload } = await imp('src', 'handlers', 'commandLoader.js');

const out = {};
const noTs = (j) => { const c = JSON.parse(JSON.stringify(j)); delete c.timestamp; return c; };
const ej = (e) => noTs(typeof e?.toJSON === 'function' ? e.toJSON() : e);

// ── Embed factories ────────────────────────────────────────────────────────────
const long = 'x'.repeat(300);
out.embeds = [
  ['success', ['Saved', 'All good', 'foot']], ['success', [long]], ['error', ['Oops', '', '']], ['error', ['Oops', 'd'.repeat(5000)]],
  ['info', ['T', 'D']], ['warn', ['W', 'x', 'f'.repeat(3000)]], ['neutral', [null, 'desc']], ['neutral', ['Title', null, 'f']],
  ['neutral', ['', '']],
].map(([fn, args]) => ({ fn, args, json: ej(E[`${fn}Embed`](...args)) }));
out.guildEmbeds = [
  [{ embedColor: 0x123456 }, 'T', 'D', 'F'], [{ embedColor: null }, 'T'], [null, '', 'D'], [{ embedColor: 0x1000000 }, 'T'], [{ embedColor: 0 }, 'T'],
].map(([settings, ...args]) => ({ settings, args, json: ej(E.guildEmbed(settings, ...args)) }));

// ── Permission names ─────────────────────────────────────────────────────────
out.permissionNames = [
  ['ManageRoles'], ['ManageGuild', 'ModerateMembers', 'Nope', 'ManageGuild'], ['SendTTSMessages', 'UseVAD', 'ViewAuditLog'],
  ['ManageEmojisAndStickers', 'ManageGuildExpressions'], ['SendMessagesInThreads', 'UseApplicationCommands', 'CreateInstantInvite', 'MentionEveryone'],
  ['ViewCreatorMonetizationAnalytics', 'UseExternalApps', 'BypassSlowmode'],
].map((keys) => ({ keys, names: IH.permissionNames(keys) }));
out.permissionNamesOfBits = ['8', '268435456', '1099511627776', '3072'].map((b) => ({ bits: b, names: IH.permissionNames(BigInt(b)) }));
out.dangerous = IH.DANGEROUS_PERMISSIONS;

// ── Colours, emoji, URLs ──────────────────────────────────────────────────────
const colorInputs = ['#5865F2', '5865f2', '0x5865F2', '#fff', 'RED', ' blurple ', 'black', 'grey', 'gray', '#12345', 'zz', '', 0, 0xffffff, 0x1000000, -1, 1.5, '#ABCDEF', '0XABC', '#0x123'];
out.colors = colorInputs.map((input) => ({ input, parsed: CE.parseColor(input) }));
out.formatColor = [0, 255, 0x5865f2, 0xffffff, 0x1000000, -1, 1.5].map((n) => ({ n, s: CE.formatColor(n) }));
const emojiInputs = ['😀', '😀️', '👍🏽', '🇺🇸', '🇿🇿', '1️⃣', '#️⃣', '👨‍👩‍👧', '❤️', '❤', '🏴󠁧󠁢󠁳󠁣󠁴󠁿', 'a', '😀😀', '<:name:123456789012345678>',
  '<a:wave:123456789012345678>', '<:x:123456789012345678>', '<:name:1234>', '  😀  ', '☺', '©', '©️', '🫠', '🧑‍💻', '🏳️‍🌈', '👁️‍🗨️', '🐻‍❄️', 'x'.repeat(20)];
const parseEmoji = (s) => {
  const r = CE.renderTemplate({ buttons: [{ label: 'L', url: 'https://e.com', emoji: s }] }, {});
  return r?.components?.[0]?.toJSON?.().components?.[0]?.emoji ?? null;
};
out.emoji = emojiInputs.map((input) => ({ input, parsed: parseEmoji(input) }));
const urls = ['https://example.com', 'http://a.b/c?d=e#f', 'ftp://x.com', 'https://', 'https:// x.com', 'example.com', 'https://例え.jp/パス',
  `https://e.com/${'a'.repeat(2040)}`, `https://e.com/${'a'.repeat(2030)}`, 'https://user:pw@host:8080/p', 'HTTPS://EXAMPLE.COM', 'http://[::1]/', 'javascript:alert(1)', 'https://e.com/ x'];
out.httpUrl = urls.map((u) => ({ u, ok: CE.isHttpUrl(u) }));

// ── Templates ─────────────────────────────────────────────────────────────────
const IMAGE = (base) => (opts) => `${base}.${opts?.extension}?size=${opts?.size}`;
const userA = { id: '111111111111111111', username: 'alice', tag: 'alice', discriminator: '0', globalName: 'Alice', bot: false,
  createdTimestamp: 1_600_000_000_123, displayAvatarURL: IMAGE('https://cdn/u/a') };
const guildA = { id: '999', name: 'My Server', ownerId: '111111111111111111', memberCount: 1234567, verificationLevel: 2,
  joinedTimestamp: 1_650_000_000_000, createdTimestamp: 1_500_000_000_000, premiumSubscriptionCount: 14, premiumTier: 3, iconURL: IMAGE('https://cdn/g') };
const memberA = { user: userA, guild: guildA, displayName: 'Ali', joinedTimestamp: 1_700_000_000_500, displayAvatarURL: IMAGE('https://cdn/m/a') };
const channelA = { id: '500', name: 'general', type: ChannelType.GuildText };
const ctx = { member: memberA, channel: channelA };
out.renderContext = { userA: { ...userA, displayAvatarURL: 'https://cdn/u/a.png?size=256' }, guildA: { ...guildA, iconURL: 'https://cdn/g.png?size=256' },
  memberA: { displayName: 'Ali', joinedTimestamp: 1_700_000_000_500, avatarURL: 'https://cdn/m/a.png?size=256' }, channelA };

const T = [
  'Hello {user}!', '   ', 42, null, [], { content: 'hi', embed: null, buttons: [] },
  { title: 'Welcome {user.name}', description: 'to {server}', color: '#ff0000', thumbnail: 'user_avatar', footer: '{server.member_count} members', timestamp: true },
  { title: 'Legacy', author: 'Someone', image: { url: 'https://e.com/i.png' }, thumbnail: 'server_icon', fields: [{ name: 'a', value: 'b', inline: 1 }, { name: '', value: 'x' }, 'junk', { name: 5, value: 6 }] },
  { content: 'c', embed: { color: 0x123456 } }, { content: 'c', embed: { color: 0 } }, { embed: { color: 'red' } },
  { content: null, embeds: [{ title: 'From API', description: 'D', color: 3447003, author: { name: 'A', icon_url: 'https://e.com/a.png', url: 'https://e.com' },
    footer: { text: 'F', icon_url: 'https://e.com/f.png' }, thumbnail: { url: 'https://e.com/t.png' }, image: { url: 'attachment://card.png' }, fields: [{ name: 'n', value: 'v' }], timestamp: '2020-01-01' }],
    components: [{ type: 1, components: [{ type: 2, style: 5, label: 'Site', url: 'https://e.com', emoji: { name: '🌐' } }, { type: 2, style: 1, label: 'Nope', custom_id: 'x' },
      { type: 2, style: 5, label: 'Custom', url: 'https://e.com/2', emoji: { id: '123456789012345678', name: 'pepe', animated: true } }] }] },
  { content: 'x'.repeat(2100), embed: { title: 't'.repeat(300), description: 'd'.repeat(4100), author: { name: 'a'.repeat(300), iconUrl: 'nope', url: 'https://ok.com' },
    footer: { text: 'f'.repeat(2100), iconURL: 'attachment://x.png' }, fields: Array.from({ length: 27 }, (_, i) => ({ name: `n${i}`, value: 'v'.repeat(1100) })), url: 'nope', image: 'ftp://x', thumbnail: '{user.avatar}' } },
  { buttons: [{ label: '', url: 'https://e.com' }, { label: 'L', url: 'nope' }, { label: 'x'.repeat(90), url: `https://e.com/${'a'.repeat(600)}`, emoji: 'bad' }, { label: 'Ok', url: '{user.avatar}', emoji: '👍' }] },
  { buttons: Array.from({ length: 27 }, (_, i) => ({ label: `B${i}`, url: `https://e.com/${i}` })) },
  { embed: { description: 'd'.repeat(3000), fields: Array.from({ length: 6 }, (_, i) => ({ name: `Field ${i}`, value: 'v'.repeat(1000) })), title: 'T'.repeat(250), footer: 'F'.repeat(500) } },
  { embed: { title: '{user.name}', url: '{server.icon}', author: { name: '{user}', iconUrl: '{user.avatar}' }, image: '{server.icon}' } },
  { embed: { fields: [{ name: '{nothing}', value: ' ' }] } }, { content: '{user}', embed: { title: '   ' } }, { embed: {} }, { buttons: [{ label: ' Link ', url: ' https://e.com ', emoji: '<:pepe:123456789012345678>' }] },
];
out.templates = T.map((input) => {
  const rendered = CE.renderTemplate(input, ctx);
  const renderedNoColor = CE.renderEmbed(CE.normalizeTemplate(input).embed ?? {}, ctx, { defaultColor: null });
  let modal = null;
  let modalThrow = null;
  try { modal = CE.buildTemplateModal({ customId: 'tpl:edit', title: 'Edit the welcome message please now', template: input }).toJSON(); } catch (e) { modal = { error: e.message }; }
  try { CE.buildTemplateModal({ customId: 'tpl:edit', template: input, overflow: 'throw' }); modalThrow = null; } catch (e) { modalThrow = { message: e.message, fields: e.fields }; }
  return {
    input,
    normalized: CE.normalizeTemplate(input),
    json: CE.templateToJson(input),
    validate: CE.validateTemplate(input),
    validateLoose: CE.validateTemplate(input, { requireSomething: false }),
    rendered: rendered ? { content: rendered.content, embeds: rendered.embeds.map(ej), components: rendered.components.map((c) => c.toJSON()), allowedMentions: rendered.allowedMentions } : null,
    renderedNoColor: renderedNoColor ? ej(renderedNoColor) : null,
    modal,
    modalThrow,
  };
});
const fieldsOf = (map) => ({ fields: { getTextInputValue: (id) => { if (!(id in map)) throw new Error('missing'); return map[id]; } } });
const MODAL_CASES = [
  [{ 'tpl-content': 'Hi', 'tpl-title': 'T', 'tpl-description': 'D', 'tpl-color': 'red', 'tpl-footer': 'F' }, null],
  [{ 'tpl-content': '', 'tpl-title': '', 'tpl-description': '', 'tpl-color': '', 'tpl-footer': '' }, { content: 'old', embed: { title: 'x', color: 5, footer: { text: 'f', iconUrl: 'https://e.com/i.png' } } }],
  [{ 'tpl-content': '', 'tpl-title': '', 'tpl-description': '', 'tpl-color': '#123', 'tpl-footer': 'New' }, { embed: { footer: { text: 'f', iconUrl: 'https://e.com/i.png' } } }],
  [{ 'tpl-content': 'x', 'tpl-color': 'nope' }, { embed: { title: 'keep' } }],
  [{ 'tpl-content': '', 'tpl-title': '', 'tpl-description': 'd'.repeat(4000), 'tpl-color': '', 'tpl-footer': '' }, { embed: { description: 'd'.repeat(4500) } }],
  [{ 'tpl-content': '', 'tpl-title': '', 'tpl-description': '', 'tpl-color': '', 'tpl-footer': '' }, null],
];
out.modalParse = MODAL_CASES.flatMap(([fields, base]) => [true, false].map((requireSomething) => {
  const r = CE.parseTemplateModal(fieldsOf(fields), base ?? undefined, { requireSomething });
  return { fields, base, requireSomething, template: r.template, errors: r.errors };
}));
out.fromJson = ['{"content":"hi"}', '{"embeds":[{"title":"T"}]}', '{}', '"just text"', `{"content":"${'a'.repeat(21000)}"}`, '{"embed":{"title":"t","url":"nope"}}']
  .map((s) => { const r = CE.templateFromJson(s); return { s: s.length > 100 ? `${s.slice(0, 20)}…(${s.length})` : s, len: s.length, template: r.template, errors: r.errors }; });

// ── Paginator ─────────────────────────────────────────────────────────────────
function fakeTarget() {
  const sent = [];
  const message = { id: 'm1', createMessageComponentCollector: () => ({ on: () => {} }) };
  return { sent, isRepliable: () => true, deferred: false, replied: false, user: { id: 'u1' },
    reply: async (p) => { sent.push(['reply', p]); return { resource: { message } }; },
    editReply: async (p) => { sent.push(['editReply', p]); return message; }, followUp: async (p) => { sent.push(['followUp', p]); return message; } };
}
const pagesOf = (footers) => footers.map((f, i) => new djs.EmbedBuilder({ title: `P${i}`, ...(f === undefined ? {} : { footer: f }) }));
const PAGE_CASES = [
  [[undefined, undefined, undefined], {}], [[{ text: 'Hello' }, { text: 'Hi', icon_url: 'https://e.com/i.png' }], {}], [[{ text: 'Page 1 of 2' }, { text: 'x' }], {}],
  [[{ text: 'f'.repeat(2048) }, { text: 'g' }], {}], [[{ text: 'Hello' }, undefined], { footer: 'replace' }], [[{ text: 'Hello' }, undefined], { footer: 'none' }],
  [[{ text: 'Only' }], {}], [[undefined, undefined, undefined], { startPage: 2 }], [[undefined, undefined], { startPage: 9 }], [[undefined, undefined], { startPage: -3 }],
];
out.paginator = [];
for (const [footers, opts] of PAGE_CASES) {
  const pages = pagesOf(footers);
  const rendered = [];
  for (let start = 0; start < pages.length; start++) {
    const t = fakeTarget();
    await PG.paginate(t, pages, { ...opts, startPage: opts.startPage ?? start });
    const [, payload] = t.sent[0];
    rendered.push({ embed: ej(payload.embeds[0]), components: payload.components.map((c) => c.toJSON()) });
    if (opts.startPage !== undefined) break;
  }
  out.paginator.push({ footers, opts, rendered });
}

// ── Confirm prompt, autocomplete, markdown ────────────────────────────────────
{
  const sent = [];
  const message = { id: 'm2', createMessageComponentCollector: () => ({ on: (ev, fn) => { if (ev === 'end') fn(); } }), edit: async () => {} };
  const i = { deferred: false, replied: false, user: { id: 'u1' }, reply: async (p) => { sent.push(p); return { resource: { message } }; }, editReply: async () => message };
  await IH.awaitConfirm(i, { title: 'Delete it?', description: 'Really' });
  await IH.awaitConfirm(i, { danger: false, confirmLabel: 'Yes please', cancelLabel: 'No' });
  out.confirm = sent.map((p) => ({ embed: ej(p.embeds[0]), components: p.components.map((c) => c.toJSON()), ephemeral: p.ephemeral }));
}
{
  const lists = [
    [{ name: 'a', value: 'A' }, { value: 'onlyValue' }, { name: '', value: 'x' }, { name: 'n'.repeat(150), value: 'v'.repeat(150) }, { name: 'e', value: '' }, { name: 'num', value: 5 }, { value: 1.5 }],
    Array.from({ length: 30 }, (_, i) => ({ name: `n${i}`, value: `v${i}` })),
  ];
  out.autocomplete = [];
  for (const list of lists) {
    let got = null;
    await IH.respondAutocomplete({ responded: false, respond: async (l) => { got = l; } }, list);
    out.autocomplete.push({ input: list, output: got });
  }
}
out.markdown = ['plain', '*bold*', '**strong**', '***both***', '_it_', '__under__', '___x___', '~~s~~', '||sp||', '`code`', '``two``', '```block```', 'a\\b',
  'https://x.com/a_b_c', '<https://x.com/a*b>', '<:e_mo:123>', 'snake_case_name', 'mix *a* _b_ **c** __d__', '# heading', '- list', '[x](y)']
  .map((s) => ({ s, e: djs.escapeMarkdown(s) }));

// ── newTargets (pure) ─────────────────────────────────────────────────────────
out.newTargets = {
  channelTypeFor: [[], [0], [5, 0], [2], [4], [15], [13], [11, 12], [10], [2, 13]].map((t) => ({ t, r: NT.channelTypeFor(t) })),
  findByName: [
    [['Logs', 'logs', 'mod-logs'], 'logs', false], [['Logs', 'mod-logs'], 'logs', false], [['mod-logs'], ' Mod Logs ', true], [['Mod Logs'], 'mod logs', false], [['a'], 'b', false],
  ].map(([names, name, slugged]) => ({ names, name, slugged, r: NT.findByName(names.map((n) => ({ name: n })), name, { slugged })?.name ?? null })),
  companionPairs: NT.companionPairs(NT.leafDefinitions(buildCommandPayload(counting), { options: { getSubcommandGroup: () => null, getSubcommand: () => 'set' } })),
  roleOption: NT.newRoleOption(new djs.SlashCommandStringOption()).toJSON(),
  channelOption: NT.newChannelOption(new djs.SlashCommandStringOption(), 'category', 'category').toJSON(),
};

// ── discord.js permissions, roles, channels and CDN URLs on a real Guild ───────
const G = '900000000000000100';
const BOT = '100000000000000002';
const P = PermissionFlagsBits;
const bits = (...ps) => String(ps.reduce((a, b) => a | b, 0n));
const role = (id, position, permissions, extra = {}) => ({ id, name: `role${id}`, color: 0, hoist: false, position, permissions, managed: false, mentionable: false, flags: 0,
  colors: { primary_color: 0, secondary_color: null, tertiary_color: null }, ...extra });
const member = (id, roles, extra = {}) => ({ user: { id, username: `user${id}`, discriminator: '0', avatar: null, bot: id === BOT, ...(extra.user ?? {}) }, roles,
  joined_at: '2025-01-01T00:00:00.000000+00:00', deaf: false, mute: false, flags: 0, ...(extra.avatar ? { avatar: extra.avatar } : {}), ...(extra.nick ? { nick: extra.nick } : {}) });
const ow = (id, type, allow, deny) => ({ id, type, allow, deny });
const chan = (id, type, name, overwrites, extra = {}) => ({ id, type, name, position: 0, permission_overwrites: overwrites, nsfw: false, parent_id: null, ...extra });
const rawGuild = {
  id: G, name: 'Test Guild', icon: 'a_0123456789abcdef0123456789abcdef', splash: null, discovery_splash: null, owner_id: '600000000000000001', afk_channel_id: null, afk_timeout: 300,
  verification_level: 2, default_message_notifications: 0, explicit_content_filter: 0, emojis: [], features: [], mfa_level: 0, application_id: null,
  system_channel_id: null, system_channel_flags: 0, rules_channel_id: null, max_members: 500000, vanity_url_code: null, description: null, banner: null,
  premium_tier: 2, premium_subscription_count: 7, preferred_locale: 'en-US', public_updates_channel_id: null, nsfw_level: 0, stickers: [], premium_progress_bar_enabled: false,
  joined_at: '2025-01-01T00:00:00.000000+00:00', large: false, unavailable: false, voice_states: [], presences: [], stage_instances: [], guild_scheduled_events: [],
  roles: [
    role(G, 0, bits(P.ViewChannel, P.SendMessages, P.ReadMessageHistory)),
    role('800000000000000001', 5, bits(P.ManageRoles, P.ManageChannels, P.ViewChannel, P.SendMessages, P.EmbedLinks, P.AddReactions), { managed: true, tags: { bot_id: BOT } }),
    role('800000000000000002', 3, bits(P.ManageGuild)),
    role('800000000000000003', 2, '0', { mentionable: true }),
    role('800000000000000004', 1, bits(P.BanMembers, P.KickMembers)),
    role('800000000000000005', 3, '0'),
    role('800000000000000006', 4, bits(P.Administrator)),
    role('800000000000000007', 1, bits(P.MentionEveryone)),
  ],
  members: [
    member(BOT, ['800000000000000001']), member('600000000000000001', []), member('600000000000000002', ['800000000000000002'], { nick: 'Mod Nick' }),
    member('600000000000000003', ['800000000000000003'], { avatar: 'a_ffffffffffffffffffffffffffffffff', user: { avatar: '0123456789abcdef0123456789abcdef', global_name: 'Third' } }),
    member('600000000000000004', ['800000000000000004', '800000000000000007'], { user: { discriminator: '1234' } }), member('600000000000000005', ['800000000000000006']),
  ],
  channels: [
    chan('700000000000000001', 0, 'general', []),
    chan('700000000000000002', 0, 'private', [ow(G, 0, '0', bits(P.ViewChannel)), ow('800000000000000002', 0, bits(P.ViewChannel), '0'), ow(BOT, 1, bits(P.ViewChannel, P.SendMessages), '0')]),
    chan('700000000000000003', 0, 'no-embeds', [ow('800000000000000001', 0, '0', bits(P.EmbedLinks)), ow('600000000000000004', 1, bits(P.ManageMessages), bits(P.SendMessages))]),
    chan('700000000000000004', 2, 'Voice', []),
    chan('700000000000000005', 4, 'Category', []),
    chan('700000000000000006', 0, 'shared', [ow(G, 0, '0', bits(P.ViewChannel)), ow('800000000000000005', 0, bits(P.ViewChannel), '0'), ow('600000000000000003', 1, bits(P.ViewChannel), '0')]),
    chan('700000000000000007', 5, 'news', [ow(G, 0, bits(P.SendMessages), bits(P.AddReactions))]),
  ],
  threads: [
    { id: '700000000000000010', type: 11, name: 'pub-thread', parent_id: '700000000000000002', owner_id: '600000000000000001', guild_id: G, message_count: 0, member_count: 1,
      rate_limit_per_user: 0, nsfw: false, permission_overwrites: [], thread_metadata: { archived: false, auto_archive_duration: 1440, archive_timestamp: '2025-01-01T00:00:00.000000+00:00', locked: false } },
    { id: '700000000000000011', type: 12, name: 'priv-thread', parent_id: '700000000000000001', owner_id: '600000000000000001', guild_id: G, message_count: 0, member_count: 1,
      rate_limit_per_user: 0, nsfw: false, permission_overwrites: [], thread_metadata: { archived: false, auto_archive_duration: 1440, archive_timestamp: '2025-01-01T00:00:00.000000+00:00', locked: false } },
  ],
};
rawGuild.member_count = rawGuild.members.length;
const client = new Client({ intents: [] });
client.user = new djs.ClientUser(client, { id: BOT, username: 'bot', discriminator: '0', bot: true, avatar: null });
const guild = client.guilds._add(rawGuild);
for (const t of rawGuild.threads) guild.channels._add(t, guild);
const members = [...guild.members.cache.values()];
const channels = [...guild.channels.cache.values()];
out.guild = rawGuild;
out.botId = BOT;
out.channelPerms = [];
for (const c of channels) for (const m of members) out.channelPerms.push({ channel: c.id, member: m.id, bits: String(c.permissionsFor(m)?.bitfield ?? 'null') });
out.rolePerms = [];
for (const c of channels) for (const r of guild.roles.cache.values()) out.rolePerms.push({ channel: c.id, role: r.id, bits: String(c.permissionsFor(r)?.bitfield ?? 'null') });
out.guildPerms = members.map((m) => ({ member: m.id, bits: String(m.permissions.bitfield) }));
out.positions = [...guild.roles.cache.values()].map((r) => ({ role: r.id, position: r.position }));
out.highest = members.map((m) => ({ member: m.id, highest: m.roles.highest.id, position: m.roles.highest.position }));
out.everyoneCanView = channels.map((c) => ({ channel: c.id, r: PV.everyoneCanView(c) }));
out.copyLeaks = [];
for (const a of channels) for (const b of channels) out.copyLeaks.push({ source: a.id, target: b.id, r: PV.copyLeaksPrivate(a, b) });
out.memberChannelProblem = [];
for (const c of channels) for (const m of members) for (const needs of [undefined, ['ViewChannel'], ['ViewChannel', 'SendMessages', 'EmbedLinks', 'ManageMessages']]) {
  out.memberChannelProblem.push({ channel: c.id, member: m.id, needs: needs ?? null, r: PV.memberChannelProblem(c, m, needs) });
}
out.roleHierarchy = [];
for (const m of members) for (const ids of [[], ['800000000000000003'], ['800000000000000002', '800000000000000005'], ['800000000000000006'], ['999']]) {
  out.roleHierarchy.push({ member: m.id, ids, r: PV.roleHierarchyProblem(guild, m, ids) });
}
out.assignable = [];
for (const r of [...guild.roles.cache.values(), null]) for (const ex of [null, ...members]) for (const allowDangerous of [false, true]) {
  out.assignable.push({ role: r?.id ?? null, executor: ex?.id ?? null, allowDangerous, r: IH.checkAssignableRole(guild, r, { executor: ex, allowDangerous }) });
}
out.pingable = [];
for (const c of channels.slice(0, 3)) for (const ex of [null, ...members]) {
  out.pingable.push({ channel: c.id, executor: ex?.id ?? null, r: IH.pingableRoleIds(c, ex, ['800000000000000003', '800000000000000002', G, '42', '800000000000000003']) });
}
out.usable = [];
for (const c of channels) for (const needs of [undefined, ['ViewChannel', 'SendMessages', 'EmbedLinks', 'AddReactions', 'ReadMessageHistory']]) {
  out.usable.push({ channel: c.id, needs: needs ?? null, r: IH.checkChannelUsable(c, needs ? { needs } : undefined) });
}
out.missingGuild = IH.missingBotPermissions(guild, ['ManageRoles', 'BanMembers', 'ViewChannel', 'ManageGuild']);
out.avatars = members.map((m) => ({ member: m.id, user: m.user.displayAvatarURL({ extension: 'png', size: 256 }), member_url: m.displayAvatarURL({ extension: 'png', size: 256 }),
  displayName: m.displayName, tag: m.user.tag, discriminator: m.user.discriminator }));
out.guildIcon = guild.iconURL({ extension: 'png', size: 256 });

writeFileSync(OUT, `${JSON.stringify(out, null, 1)}\n`);
console.log(`wrote ${OUT}`);
process.exit(0);
