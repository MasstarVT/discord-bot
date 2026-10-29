// Dashboard collections owned by collections-core: saved embeds, custom commands, auto-responders, reminders and feeds.
// Offline by default: descriptor lint, request validation (create, update, actions, cross-field rules), and the pure
// helpers (rendering a template into a Discord REST message, the role policies, Discord error mapping, the stored <->
// form conversions, parity with the slash commands' own parsers). With TEST_DATABASE_URL (a migrated database) the
// stores also run against Postgres with a fake Discord client and a stubbed feed fetcher:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/dashboard-collections.test.js
// The stores are imported only inside the database cases (they are never loaded offline).
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { PermissionFlagsBits } = await import('discord.js');
const { LIMITS } = await import('../src/config/constants.js');
const { MODULES } = await import('../src/config/modules.js');
const { renderTemplate } = await import('../src/utils/customEmbed.js');
const { sanitizeName } = await import('../src/utils/text.js');
const { FieldError } = await import('../src/dashboard/schema.js');
const { RecordError } = await import('../src/dashboard/collections/errors.js');
const { COLLECTIONS, lintCollections, publicCollection, validateRecordInput } = await import('../src/dashboard/collections/index.js');
const E = await import('../src/dashboard/collections/embeds.js');
const T = await import('../src/dashboard/collections/tags.js');
const TR = await import('../src/dashboard/collections/triggers.js');
const R = await import('../src/dashboard/collections/reminders.js');
const F = await import('../src/dashboard/collections/feeds.js');
// Services, for parity checks (they open nothing until used, except the Redis client, disconnected below).
const embedService = await import('../src/services/embedService.js');
const tagService = await import('../src/services/tagService.js');
const reminderService = await import('../src/services/reminderService.js');
const feedService = await import('../src/services/feedService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const MINE = ['embeds', 'tags', 'triggers', 'reminders', 'feeds'];
const GUILD = '955017000000000001';
const GUILD2 = '955017000000000002';
const USER = '955017000000000010';
const CH_TEXT = '955017000000000020';
const CH_NEWS = '955017000000000021';
const CH_VOICE = '955017000000000022';
const ROLE_PING = '955017000000000030'; // mentionable, harmless
const ROLE_QUIET = '955017000000000031'; // not mentionable
const ROLE_MOD = '955017000000000032'; // Kick Members
const ROLE_HIGH = '955017000000000033'; // above the bot
const ROLE_NOPERMS = '955017000000000034'; // neither `permissions` nor `dangerous`
const ROLE_FLAGGED = '955017000000000035'; // `dangerous: true`, as getGuildResources sends it
const OWNER = '955017000000000011';
const ICON = 'a'.repeat(32);

after(async () => {
  Object.assign(console, quiet);
  feedService.__setFetchImpl(null);
  redis.disconnect();
  if (DB) {
    // The cascade removes every record and scheduled job of these guilds.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: [GUILD, GUILD2] } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

const role = (id, name, position, extra = {}) => ({
  id, name, color: null, position, managed: false, everyone: false, assignable: true, mentionable: false, permissions: '0', ...extra,
});
const RES = Object.freeze({
  guild: { id: GUILD, name: 'Test Server', icon: ICON, memberCount: 1234, ownerId: OWNER, premiumSubscriptionCount: 7 },
  channels: [
    { id: CH_TEXT, name: 'general', type: 0, kind: 'text', parentId: null, position: 0, nsfw: false },
    { id: CH_NEWS, name: 'news', type: 5, kind: 'announcement', parentId: null, position: 1, nsfw: false },
    { id: CH_VOICE, name: 'Lounge', type: 2, kind: 'voice', parentId: null, position: 2, nsfw: false },
  ],
  roles: [
    role(ROLE_HIGH, 'admins', 10, { assignable: false }),
    role(ROLE_MOD, 'mods', 4, { permissions: String(PermissionFlagsBits.KickMembers) }),
    role(ROLE_PING, 'pings', 2, { mentionable: true }),
    role(ROLE_QUIET, 'quiet', 1),
    (() => { const r = role(ROLE_NOPERMS, 'legacy', 1); delete r.permissions; return r; })(),
    (() => { const r = role(ROLE_FLAGGED, 'helpers', 1, { dangerous: true }); delete r.permissions; return r; })(),
    role(GUILD, '@everyone', 0, { everyone: true, assignable: false }),
  ],
  emojis: [],
  bot: { userId: '955017000000000099', topRolePosition: 5 },
});

/** Asserts a FieldError (or RecordError with a path) at `path` whose message matches `re`. */
function failsAt(fn, path, re) {
  assert.throws(fn, (err) => {
    assert.ok(err instanceof FieldError || err instanceof RecordError, `unexpected ${err?.name}: ${err?.message}`);
    if (path !== undefined) assert.equal(err.path, path, err.message);
    if (re) assert.match(err.message, re);
    return true;
  });
}
const create = (key, data) => validateRecordInput(key, data, { mode: 'create' }).data;

// ── Descriptors ──────────────────────────────────────────────────────────────

test('descriptors: lint clean, JSON-safe public copies, real modules and the shared limits', () => {
  const problems = lintCollections().filter((p) => MINE.some((k) => p.startsWith(k)));
  assert.deepEqual(problems, []);
  const limits = { embeds: LIMITS.SAVED_EMBEDS, tags: LIMITS.CUSTOM_TAGS, triggers: LIMITS.AUTO_TRIGGERS, reminders: LIMITS.REMINDERS, feeds: LIMITS.FEEDS };
  for (const key of MINE) {
    const c = COLLECTIONS[key];
    assert.equal(c.placeholder, undefined, `${key} is still a placeholder`);
    assert.ok(Object.hasOwn(MODULES, c.module), `${key}: module ${c.module}`);
    assert.equal(c.limit, limits[key]);
    const pub = publicCollection(c);
    assert.deepEqual(JSON.parse(JSON.stringify(pub)), pub, `${key} public copy is plain JSON`);
    assert.equal(typeof pub.check, 'undefined');
    for (const f of c.fields) assert.ok(f.label && f.type, `${key}.${f.field}`);
  }
  assert.deepEqual(COLLECTIONS.embeds.actions.map((a) => a.id), ['publish', 'refresh', 'unpublish']);
  assert.equal(COLLECTIONS.embeds.actions.find((a) => a.id === 'unpublish').danger, true);
});

// ── Request validation ───────────────────────────────────────────────────────

test('embeds: create, update and action input', () => {
  const tpl = { content: 'Hi {user}', embed: null, buttons: [] };
  const ok = create('embeds', { name: 'rules', template: tpl });
  assert.equal(ok.name, 'rules');
  assert.equal(ok.template.content, 'Hi {user}');
  for (const bad of ['My Rules', '-x', 'x-', 'a--b', 'Rules', 'a!b']) failsAt(() => create('embeds', { name: bad, template: tpl }), 'name', /lowercase/);
  failsAt(() => create('embeds', { name: 'a'.repeat(33), template: tpl }), 'name');
  failsAt(() => create('embeds', { name: 'rules' }), 'template', /required/);
  failsAt(() => create('embeds', { name: 'rules', template: { content: null, embed: null, buttons: [] } }), 'template', /empty/);
  failsAt(() => create('embeds', { name: 'rules', template: { content: 'x'.repeat(2001) } }), 'template', /2000/);

  const upd = validateRecordInput('embeds', { template: { embed: { title: 'T' } } }, { mode: 'update', current: null }).data;
  assert.deepEqual(Object.keys(upd), ['template']);
  failsAt(() => validateRecordInput('embeds', { name: 'other' }, { mode: 'update' }), 'name', /cannot be changed/);
  failsAt(() => validateRecordInput('embeds', { messageId: '1' }, { mode: 'update' }), 'messageId', /not something you can set/);

  failsAt(() => validateRecordInput('embeds', {}, { mode: 'action', actionId: 'publish' }), 'channelId', /required/);
  const pub = validateRecordInput('embeds', { channelId: CH_TEXT }, { mode: 'action', actionId: 'publish' });
  assert.deepEqual(pub.data, { channelId: CH_TEXT, deletePrevious: true });
  assert.deepEqual(pub.refs.channels[0].kinds, ['text', 'announcement']);
  assert.deepEqual(validateRecordInput('embeds', {}, { mode: 'action', actionId: 'unpublish' }).data, {});
  assert.throws(() => validateRecordInput('embeds', {}, { mode: 'action', actionId: 'nope' }), (e) => e instanceof RecordError && e.code === 'not_found');
});

test('tags: action, access and cooldown rules', () => {
  const d = create('tags', { name: 'hello' });
  assert.equal(d.actions.length, 1);
  assert.equal(d.actions[0].type, 'reply');
  assert.equal(d.cooldownScope, 'NONE');
  assert.equal(d.enabled, true);

  const one = (action) => create('tags', { name: 'x', actions: [action] });
  failsAt(() => one({ type: 'send', responses: [{ text: 'hi' }] }), 'actions.0.channelId', /channel/);
  failsAt(() => one({ type: 'reply', responses: [] }), 'actions.0.responses', /at least one/);
  failsAt(() => one({ type: 'reply', responses: [{ text: 'x'.repeat(2001) }] }), 'actions.0.responses.0.text', /2,000/);
  assert.equal(one({ type: 'reply', embed: true, responses: [{ text: 'x'.repeat(3000) }] }).actions[0].embed, true);
  failsAt(() => one({ type: 'reply', embed: true, responses: [{ text: 'x'.repeat(4001) }] }), 'actions.0.responses.0.text', /4000/);
  failsAt(() => one({ type: 'dm', responses: [{ text: 'hi' }], pingRoleIds: [ROLE_PING] }), 'actions.0.pingRoleIds', /DMs never ping/);
  failsAt(() => one({ type: 'addRole' }), 'actions.0.roleId', /Pick the role/);
  assert.equal(one({ type: 'toggleRole', roleId: ROLE_PING }).actions[0].roleId, ROLE_PING);
  failsAt(() => create('tags', { name: 'x', actions: [] }), 'actions', /at least 1/);
  const reply = { type: 'reply', responses: [{ text: 'a' }] };
  failsAt(() => create('tags', { name: 'x', actions: Array(LIMITS.TAG_ACTIONS + 1).fill(reply) }), 'actions', /at most/);
  failsAt(() => one({ type: 'reply', responses: Array(LIMITS.TAG_RESPONSES + 1).fill({ text: 'a' }) }), 'actions.0.responses', /at most/);
  failsAt(() => create('tags', { name: 'x', allowRoleIds: [ROLE_PING], denyRoleIds: [ROLE_PING] }), 'denyRoleIds.0', /both allowed and denied/);
  failsAt(() => create('tags', { name: 'x', allowChannelIds: [CH_TEXT], denyChannelIds: [CH_TEXT] }), 'denyChannelIds.0', /both/);
  failsAt(() => create('tags', { name: 'x', cooldownScope: 'USER' }), 'cooldownSeconds', /at least 1 second/);
  failsAt(() => create('tags', { name: 'Hello' }), 'name', /lowercase/);
  failsAt(() => create('tags', { name: 'x', description: 'd'.repeat(101) }), 'description', /100/);

  const current = { ...d, id: 'c1', name: 'hello', cooldownSeconds: 30, uses: 0 };
  assert.equal(validateRecordInput('tags', { cooldownScope: 'USER' }, { mode: 'update', current }).data.cooldownScope, 'USER');
  failsAt(() => validateRecordInput('tags', { cooldownScope: 'USER' }, { mode: 'update', current: { ...current, cooldownSeconds: 0 } }), 'cooldownSeconds');
  failsAt(() => validateRecordInput('tags', { name: 'other' }, { mode: 'update', current }), 'name', /cannot be changed/);
  // Access-list channels accept categories and voice channels, like /tag permissions.
  const refs = validateRecordInput('tags', { name: 'x', allowChannelIds: [CH_VOICE] }, { mode: 'create' }).refs;
  assert.ok(refs.channels[0].kinds.includes('category') && refs.channels[0].kinds.includes('voice'));
});

test('triggers: safe regexes, response and cooldown limits', () => {
  const d = create('triggers', { trigger: 'hello', response: 'Hi {user}!' });
  assert.deepEqual(
    { matchType: d.matchType, caseSensitive: d.caseSensitive, cooldown: d.cooldown, enabled: d.enabled, channelIds: d.channelIds },
    { matchType: 'CONTAINS', caseSensitive: false, cooldown: 0, enabled: true, channelIds: [] },
  );
  failsAt(() => create('triggers', { trigger: '(a+)+', matchType: 'REGEX', response: 'x' }), 'trigger', /Nested repetition/);
  failsAt(() => create('triggers', { trigger: '(a)\\1', matchType: 'REGEX', response: 'x' }), 'trigger', /Backreferences/);
  assert.equal(create('triggers', { trigger: '^h(i|ey)$', matchType: 'REGEX', response: 'x' }).matchType, 'REGEX');
  failsAt(() => create('triggers', { trigger: 'x', response: '   ' }), 'response');
  failsAt(() => create('triggers', { trigger: 'x', response: 'y', cooldown: TR.TRIGGER_LIMITS.cooldown + 1 }), 'cooldown');
  failsAt(() => create('triggers', { trigger: 'x'.repeat(501), response: 'y' }), 'trigger');

  const current = { ...d, id: 't1', trigger: '(a+)+', matchType: 'CONTAINS' };
  failsAt(() => validateRecordInput('triggers', { matchType: 'REGEX' }, { mode: 'update', current }), 'trigger', /Nested/);
  // A change that does not touch the pattern never re-checks an old unsafe one.
  assert.equal(validateRecordInput('triggers', { response: 'new' }, { mode: 'update', current: { ...current, matchType: 'REGEX' } }).data.response, 'new');
});

test('reminders: what each mode needs', () => {
  const d = create('reminders', { name: 'standup', channelId: CH_TEXT });
  assert.equal(d.mode, 'TIME_OF_DAY');
  assert.equal(d.timeOfDay, '09:00');
  assert.deepEqual(d.template, { content: "Hey, I'm a reminder!", embed: null, buttons: [] });
  assert.equal(d.enabled, true);
  failsAt(() => create('reminders', { name: 'r', channelId: CH_TEXT, mode: 'ONE_SHOT' }), 'runAt', /date and time/);
  failsAt(() => create('reminders', { name: 'r', channelId: CH_TEXT, mode: 'INTERVAL', intervalMinutes: null }), 'intervalMinutes');
  failsAt(() => create('reminders', { name: 'r', channelId: CH_TEXT, mode: 'INTERVAL', intervalMinutes: 3 }), 'intervalMinutes', /between 5/);
  failsAt(() => create('reminders', { name: 'r', channelId: CH_TEXT, mode: 'INTERVAL', windowStart: '09:00', windowEnd: '09:00' }), 'windowEnd');
  failsAt(() => create('reminders', { name: 'r', channelId: CH_TEXT, mode: 'TIME_OF_DAY', timeOfDay: null }), 'timeOfDay');
  failsAt(() => create('reminders', { name: 'r', channelId: CH_TEXT, weekdays: ['mon', 'funday'] }), 'weekdays.1');
  failsAt(() => create('reminders', { name: 'r', channelId: CH_TEXT, timeOfDay: '25:00' }), 'timeOfDay');
  failsAt(() => create('reminders', { name: 'Daily Standup', channelId: CH_TEXT }), 'name', /lowercase/);
  failsAt(() => create('reminders', { name: 'r' }), 'channelId', /required/);
  assert.deepEqual(validateRecordInput('reminders', {}, { mode: 'action', actionId: 'test' }).data, { channelId: null });
  const current = { ...d, id: 'r1', mode: 'ONE_SHOT', runAt: '2026-01-01T00:00:00.000Z' };
  assert.equal(validateRecordInput('reminders', { channelId: CH_NEWS }, { mode: 'update', current }).data.channelId, CH_NEWS);
});

test('feeds: sources parse like /feed add, reposts are Bluesky-only', () => {
  const d = create('feeds', { type: 'RSS', source: 'example.com/feed', channelId: CH_TEXT });
  assert.equal(d.messageTemplate, null);
  assert.equal(d.enabled, true);
  failsAt(() => create('feeds', { type: 'YOUTUBE', source: '@somebody', channelId: CH_TEXT }), 'source', /handle/i);
  failsAt(() => create('feeds', { type: 'REDDIT', source: 'u/someone', channelId: CH_TEXT }), 'source', /subreddit/i);
  failsAt(() => create('feeds', { type: 'BLUESKY', source: 'not a handle!', channelId: CH_TEXT }), 'source', /Bluesky handle/);
  failsAt(() => create('feeds', { type: 'PODCAST', source: 'x', channelId: CH_TEXT }), 'source', /2 characters/);
  failsAt(() => create('feeds', { type: 'RSS', source: 'https://example.com/f', channelId: CH_TEXT, includeReposts: true }), 'includeReposts');
  failsAt(() => create('feeds', { type: 'RSS', source: 'https://example.com/f', channelId: CH_TEXT, messageTemplate: 'x'.repeat(1001) }), 'messageTemplate');
  assert.equal(create('feeds', { type: 'BLUESKY', source: 'alice', channelId: CH_TEXT, includeReposts: true }).includeReposts, true);

  const current = { id: 'f1', type: 'RSS', source: 'https://example.com/f', channelId: CH_TEXT, includeReposts: false };
  failsAt(() => validateRecordInput('feeds', { type: 'YOUTUBE' }, { mode: 'update', current }), 'type', /cannot be changed/);
  failsAt(() => validateRecordInput('feeds', { source: 'x' }, { mode: 'update', current }), 'source', /cannot be changed/);
  failsAt(() => validateRecordInput('feeds', { includeReposts: true }, { mode: 'update', current }), 'includeReposts');
  assert.equal(validateRecordInput('feeds', { includeReposts: true }, { mode: 'update', current: { ...current, type: 'BLUESKY' } }).data.includeReposts, true);

  assert.deepEqual(F.parseFeedSource('RSS', 'example.com/feed#x'), { ok: true, input: { url: 'https://example.com/feed' } });
  assert.deepEqual(F.parseFeedSource('YOUTUBE', 'https://www.youtube.com/channel/UCabcdefghijklmnopqrstuv'), { ok: true, input: { channelId: 'UCabcdefghijklmnopqrstuv' } });
  assert.deepEqual(F.parseFeedSource('REDDIT', 'r/aww'), { ok: true, input: { name: 'aww' } });
  assert.deepEqual(F.parseFeedSource('BLUESKY', '@Alice'), { ok: true, input: { actor: 'alice.bsky.social' } });
  assert.deepEqual(F.parseFeedSource('PODCAST', 'Serial'), { ok: true, input: { query: 'Serial' } });
  assert.deepEqual(F.parseFeedSource('PODCAST', 'feeds.example.com/show.xml'), { ok: true, input: { url: 'https://feeds.example.com/show.xml' } });
  assert.equal(F.parseFeedSource('NOPE', 'x').ok, false);
  assert.ok(!F.parseFeedSource('YOUTUBE', '@x').message.includes('**'), 'markdown is stripped for the web');
});

// ── Pure helpers ─────────────────────────────────────────────────────────────

test('embeds: a template renders into a plain Discord REST message without a gateway client', () => {
  const tpl = {
    content: `Hi {user} in {channel} <@&${ROLE_PING}>`,
    embed: {
      title: 'Welcome to {server}', description: '{server.member_count} members, {server.boost_count} boosts, owner {server.owner}',
      thumbnail: '{server.icon}', url: 'https://example.com/{server.id}',
      footer: { text: '{user.name}', iconUrl: '{user.avatar}' }, fields: [{ name: 'a', value: 'b', inline: true }], timestamp: true,
    },
    buttons: [{ label: 'Site', url: 'https://example.com', emoji: '🔗' }],
  };
  const rctx = E.renderContextFor(RES, { channelId: CH_TEXT, user: { id: USER, username: 'alice', avatar: null } });
  const rendered = renderTemplate(tpl, rctx, { defaultColor: 0x123456 });
  const body = E.toRestMessage(rendered, { allowedMentions: E.restAllowedMentions({ users: true, roles: [ROLE_PING] }) });
  assert.deepEqual(JSON.parse(JSON.stringify(body)), body, 'plain JSON');
  assert.equal(body.content, `Hi <@${USER}> in <#${CH_TEXT}> <@&${ROLE_PING}>`);
  const [embed] = body.embeds;
  assert.equal(embed.title, 'Welcome to Test Server');
  assert.equal(embed.description, `1,234 members, 7 boosts, owner <@${OWNER}>`);
  assert.equal(embed.url, `https://example.com/${GUILD}`);
  assert.match(renderTemplate({ content: '{server.created_at} {user.created_at}' }, rctx).content, /^<t:\d+:D> <t:\d+:D>$/);
  assert.equal(embed.thumbnail.url, `https://cdn.discordapp.com/icons/${GUILD}/${ICON}.png?size=256`);
  assert.equal(embed.footer.text, 'alice');
  assert.match(embed.footer.icon_url, /^https:\/\/cdn\.discordapp\.com\/embed\/avatars\/\d\.png$/);
  assert.equal(embed.color, 0x123456);
  assert.ok(!Number.isNaN(Date.parse(embed.timestamp)));
  assert.deepEqual(body.components, [{ type: 1, components: [{ type: 2, style: 5, label: 'Site', url: 'https://example.com', emoji: { name: '🔗' } }] }]);
  assert.deepEqual(body.allowed_mentions, { parse: ['users'], roles: [ROLE_PING] });

  // An edit clears removed text and pings nobody.
  const edit = E.toRestMessage(renderTemplate({ embed: { title: 'x' } }, rctx), { edit: true, allowedMentions: E.restAllowedMentions({ users: false }) });
  assert.equal(edit.content, '');
  assert.deepEqual(edit.allowed_mentions, { parse: [] });
  assert.equal(E.toRestMessage(renderTemplate({ embed: { title: 'x' } }, rctx)).content, undefined);
  // Never @everyone, and only snowflakes as roles.
  assert.deepEqual(E.restAllowedMentions({ roles: ['everyone', '@here', ROLE_PING, ROLE_PING] }), { parse: ['users'], roles: [ROLE_PING] });

  // Animated icons as GIF; no icon, no URL; a user avatar hash; a reminder has no user.
  assert.match(E.guildIconUrl({ id: GUILD, icon: `a_${'b'.repeat(32)}` }), /\.gif\?size=256$/);
  assert.equal(E.guildIconUrl({ id: GUILD, icon: null }), null);
  assert.equal(E.userAvatarUrl({ id: USER, avatar: ICON }), `https://cdn.discordapp.com/avatars/${USER}/${ICON}.png?size=256`);
  const noUser = E.renderContextFor(RES, { channelId: CH_NEWS });
  assert.equal(noUser.user, null);
  assert.equal(renderTemplate({ content: '{user} {channel.name}' }, noUser).content, '{user} news');
  assert.equal(E.messageUrl(GUILD, CH_TEXT, USER), `https://discord.com/channels/${GUILD}/${CH_TEXT}/${USER}`);
  assert.equal(E.messageUrl(GUILD, CH_TEXT, null), null);
});

test('role policies follow the slash commands for the dashboard user', () => {
  const plain = { owner: false, permissions: '0' };
  assert.equal(E.pingRoleProblem(RES, ROLE_PING, null), null, 'mentionable roles are fine for anyone');
  assert.match(E.pingRoleProblem(RES, ROLE_QUIET, null), /could not check/);
  assert.match(E.pingRoleProblem(RES, ROLE_QUIET, plain), /not mentionable and you lack/);
  assert.equal(E.pingRoleProblem(RES, ROLE_QUIET, { permissions: String(PermissionFlagsBits.MentionEveryone) }), null);
  assert.equal(E.pingRoleProblem(RES, ROLE_QUIET, { permissions: String(PermissionFlagsBits.Administrator) }), null);
  assert.equal(E.pingRoleProblem(RES, ROLE_QUIET, { owner: true, permissions: '0' }), null);
  assert.match(E.pingRoleProblem(RES, GUILD, { owner: true }), /@everyone/);
  assert.match(E.pingRoleProblem(RES, '955017000000000999', { owner: true }), /does not exist/);
  assert.deepEqual(E.splitPingableRoles([ROLE_PING, ROLE_QUIET, GUILD, ROLE_PING], RES, plain), { allowed: [ROLE_PING], refused: [ROLE_QUIET, GUILD] });
  assert.deepEqual(E.splitPingableRoles([ROLE_QUIET], RES, { owner: true }), { allowed: [ROLE_QUIET], refused: [] });

  const owner = { owner: true, permissions: '0' };
  assert.equal(E.assignRoleProblem(RES, ROLE_PING, owner), null);
  // The member hierarchy is checked when known (the dashboard server checks it for every new assignable role).
  assert.equal(E.assignRoleProblem(RES, ROLE_PING, null), null);
  assert.equal(E.assignRoleProblem(RES, ROLE_PING, { ...plain, highestRolePosition: 5 }), null);
  assert.match(E.assignRoleProblem(RES, ROLE_PING, { ...plain, highestRolePosition: 2 }), /not below your highest role/);
  assert.match(E.assignRoleProblem(RES, ROLE_FLAGGED, owner), /moderator or admin permissions/);
  assert.equal(E.assignRoleProblem({ ...RES, roles: [role(ROLE_PING, 'x', 1, { permissions: undefined, dangerous: false })] }, ROLE_PING, null), null);
  assert.match(E.assignRoleProblem(RES, ROLE_MOD, owner), /dangerous permissions: Kick Members/);
  assert.match(E.assignRoleProblem(RES, ROLE_HIGH, owner), /above the bot's highest role/);
  assert.match(E.assignRoleProblem(RES, ROLE_NOPERMS, owner), /could not check this role's permissions/);
  assert.match(E.assignRoleProblem(RES, GUILD, owner), /@everyone/);
  assert.match(E.assignRoleProblem({ ...RES, roles: [role(ROLE_PING, 'bot', 1, { managed: true })] }, ROLE_PING, owner), /managed/);
  assert.equal(E.memberCanMentionAll({ permissions: 'garbage' }), false);
  // The owner is known from the guild even when the request carries no member data.
  assert.deepEqual(E.memberFor({ userId: OWNER }, RES), { owner: true });
  assert.equal(E.memberFor({ userId: USER }, RES), null);
  assert.deepEqual(E.memberFor({ userId: USER, member: plain }, RES), plain);
  assert.equal(E.pingRoleProblem(RES, ROLE_QUIET, E.memberFor({ userId: OWNER }, RES)), null);
});

test('Discord failures become RecordErrors the admin can act on', () => {
  const apiErr = (code, status, discordCode) => Object.assign(new Error(`Discord returned HTTP ${status}`), {
    name: 'DiscordApiError', code, status, ...(discordCode === undefined ? {} : { discordCode }),
  });
  const channel = { id: CH_TEXT, name: 'general' };
  const send = E.discordFailure(apiErr('forbidden', 403, 50013), { op: 'send', channel });
  assert.ok(send instanceof RecordError);
  assert.equal(send.code, 'discord');
  assert.equal(send.status, 502);
  assert.match(send.message, /can't post in #general: it needs View Channel, Send Messages and Embed Links/);
  assert.doesNotMatch(E.discordFailure(apiErr('forbidden', 403, 50001), { op: 'send', channel, embeds: false }).message, /Embed Links/);
  assert.match(E.discordFailure(apiErr('forbidden', 403), { op: 'edit', channel }).message, /edit its message in #general/);
  assert.match(E.discordFailure(apiErr('forbidden', 403), { op: 'delete', channelId: CH_TEXT }).message, /delete the message in the channel/);
  const invalid = E.discordFailure(apiErr('unavailable', 400, 50035), { op: 'send', channel });
  assert.equal(invalid.code, 'invalid');
  assert.equal(invalid.path, 'template');
  assert.match(E.discordFailure(apiErr('not_found', 404, 10003), { op: 'send', channel }).message, /#general no longer exists/);
  assert.match(E.discordFailure(apiErr('rate_limited', 429), { op: 'send', channel }).message, /rate limiting/);
  assert.match(E.discordFailure(apiErr('unavailable', 503), { op: 'send', channel }).message, /did not answer/);
  const bug = new TypeError('boom');
  assert.equal(E.discordFailure(bug, { op: 'send', channel }), bug, 'non-Discord errors pass through');
});

test('tags: stored actions and form actions convert both ways like normalizeActions', () => {
  const stored = [
    { type: 'reply', dm: false, embed: false, responses: ['a', 'b'], pingRoleIds: [ROLE_PING] },
    { type: 'reply', dm: true, embed: true, responses: ['c'] },
    { type: 'send', channelId: CH_NEWS, embed: false, responses: ['d'] },
    { type: 'toggleRole', roleId: ROLE_PING },
  ];
  const form = stored.map(T.actionToForm);
  assert.deepEqual(form.map((a) => a.type), ['reply', 'dm', 'send', 'toggleRole']);
  assert.deepEqual(form[0], { type: 'reply', responses: [{ text: 'a' }, { text: 'b' }], embed: false, channelId: null, roleId: null, pingRoleIds: [ROLE_PING] });
  assert.deepEqual(form[3], { type: 'toggleRole', responses: [], embed: false, channelId: null, roleId: ROLE_PING, pingRoleIds: [] });
  const back = form.map(T.actionToStored);
  assert.deepEqual(back, tagService.normalizeActions(stored));
  assert.deepEqual(tagService.normalizeActions(back), back, 'the stored shape is a fixed point of normalizeActions');
  // Options that do not belong to the action are dropped, never stored.
  assert.deepEqual(T.actionToStored({ type: 'addRole', roleId: ROLE_PING, responses: [{ text: 'x' }], embed: true, channelId: CH_TEXT, pingRoleIds: [ROLE_PING] }),
    { type: 'addRole', roleId: ROLE_PING });
  assert.deepEqual(T.actionToStored({ type: 'dm', responses: [{ text: 'x' }], embed: false, pingRoleIds: [ROLE_PING] }),
    { type: 'reply', dm: true, embed: false, responses: ['x'] });
  // A legacy row (actions null) shows its content as one reply.
  const legacy = { id: 'x', guildId: GUILD, name: 'old', content: 'old text', useEmbed: true, actions: null, uses: 3, cooldownScope: 'NONE' };
  const item = T.tagItem(legacy, tagService.getEffectiveActions(legacy));
  assert.deepEqual(item.actions, [{ type: 'reply', responses: [{ text: 'old text' }], embed: true, channelId: null, roleId: null, pingRoleIds: [] }]);
  assert.equal(item.uses, 3);
  assert.deepEqual(item.allowRoleIds, []);
});

test('names: the slug pattern accepts exactly the names /embed and /reminder keep unchanged', () => {
  const re = new RegExp(`^(?:${E.SLUG_PATTERN})$`, 'u');
  const samples = ['rules', 'my-rules', 'My Rules', '-x', 'x-', 'a--b', 'a_b', '__', '_-_', 'über', 'a'.repeat(32), 'a'.repeat(33),
    '', 'r00t', 'a-b-c', 'x--', 'ab!c', 'welcome-greeting'];
  for (const s of samples) {
    assert.equal(re.test(s), embedService.normalizeEmbedName(s) === s, `embed name "${s}"`);
    assert.equal(re.test(s), s !== '' && sanitizeName(s, { max: 32, fallback: '' }) === s, `reminder name "${s}"`); // '' is refused by minLength
  }
  const tagRe = new RegExp(`^(?:${COLLECTIONS.tags.fields[0].pattern})$`, 'u');
  for (const s of ['rules', '-x', 'a--b', 'Rules', 'my rules', 'c++', 'a'.repeat(33)]) {
    const v = tagService.validateNewTagName(s);
    assert.equal(tagRe.test(s), v.ok && v.name === s, `tag name "${s}"`);
  }
});

test('reminders: the form schedule feeds validateReminderInput like the slash options', () => {
  const now = new Date('2026-10-12T10:00:00Z');
  const tz = 'Europe/Berlin';
  const base = { timeOfDay: '09:00', weekdays: [], intervalMinutes: 60, windowStart: null, windowEnd: null, runAt: null, requireActivity: false };
  const v = (form) => reminderService.validateReminderInput(R.scheduleInput({ ...base, ...form }), { tz, now, base: null });

  let r = v({ mode: 'TIME_OF_DAY', timeOfDay: '08:30', weekdays: ['fri', 'mon'], requireActivity: true });
  assert.equal(r.ok, true, r.error);
  assert.deepEqual(r.fields, {
    mode: 'TIME_OF_DAY', timeOfDay: '08:30', weekdays: [1, 5], intervalMinutes: null, windowStart: null, windowEnd: null,
    runAt: null, requireActivity: true,
  });
  assert.deepEqual(v({ mode: 'TIME_OF_DAY' }).fields.weekdays, [], 'no days = every day');
  assert.deepEqual(v({ mode: 'TIME_OF_DAY', weekdays: ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'] }).fields.weekdays, []);

  r = v({ mode: 'INTERVAL', intervalMinutes: 120, windowStart: '09:00', windowEnd: '17:00' });
  assert.equal(r.ok, true, r.error);
  assert.deepEqual([r.fields.intervalMinutes, r.fields.windowStart, r.fields.windowEnd, r.fields.timeOfDay], [120, '09:00', '17:00', null]);
  assert.match(v({ mode: 'INTERVAL', intervalMinutes: 1440, windowStart: '09:00' }).error, /under 24 hours/);
  assert.match(v({ mode: 'INTERVAL', intervalMinutes: 2880, weekdays: ['mon'] }).error, /1 day or less/);

  const at = new Date(now.getTime() + 2 * 3_600_000).toISOString();
  r = v({ mode: 'ONE_SHOT', runAt: at, requireActivity: true, weekdays: ['mon'] });
  assert.equal(r.ok, true, r.error);
  assert.equal(r.fields.runAt.toISOString(), at);
  assert.equal(r.fields.requireActivity, false);
  assert.deepEqual(r.fields.weekdays, []);
  assert.match(v({ mode: 'ONE_SHOT', runAt: '2026-10-12T09:00:00.000Z' }).error, /has passed/);
  assert.match(v({ mode: 'ONE_SHOT', runAt: '2028-01-01T00:00:00.000Z' }).error, /within one year/);

  assert.deepEqual(R.weekdayNames([5, 1, 0, 9]), ['mon', 'fri', 'sun']);
  assert.deepEqual(R.weekdayNumbers(['sun', 'fri', 'mon', 'nope']), [0, 1, 5]);
});

test('items: rows become collection items in the wire shapes', () => {
  const at = new Date('2026-10-01T12:00:00Z');
  const posted = E.embedItem({ id: 'e1', guildId: GUILD, name: 'rules', template: { content: 'x' }, channelId: CH_TEXT, messageId: USER, updatedAt: at });
  assert.deepEqual(posted, {
    id: 'e1', name: 'rules', template: { content: 'x', embed: null, buttons: [] }, status: 'Posted', channelId: CH_TEXT, messageId: USER,
    messageUrl: `https://discord.com/channels/${GUILD}/${CH_TEXT}/${USER}`, updatedAt: at.toISOString(),
  });
  const draft = E.embedItem({ id: 'e1', guildId: GUILD, name: 'rules', template: {}, channelId: CH_TEXT, messageId: null, updatedAt: at });
  assert.equal(draft.status, 'Draft');
  assert.equal(draft.messageUrl, null);

  const rem = R.reminderItem({
    id: 'r1', name: 'standup', channelId: CH_TEXT, template: null, mode: 'TIME_OF_DAY', timeOfDay: '09:00', weekdays: [1, 2, 3, 4, 5],
    intervalMinutes: null, windowStart: null, windowEnd: null, runAt: null, requireActivity: false, pingRoleId: null, enabled: true,
    lastSentAt: null,
  }, { tz: 'Europe/Berlin', nextRunAt: at });
  assert.equal(rem.schedule, 'On weekdays at 09:00 (Europe/Berlin)');
  assert.deepEqual(rem.weekdays, ['mon', 'tue', 'wed', 'thu', 'fri']);
  assert.equal(rem.nextRunAt, at.toISOString());
  assert.equal(rem.status, 'Active');
  assert.deepEqual(rem.template, { content: null, embed: null, buttons: [] });
  const done = R.reminderItem({ id: 'r2', mode: 'ONE_SHOT', runAt: at, lastSentAt: new Date(at.getTime() + 1000), enabled: false, weekdays: [] }, { tz: 'UTC', nextRunAt: at });
  assert.equal(done.status, 'Sent');
  assert.equal(done.nextRunAt, null);
  assert.equal(R.scheduleText({ mode: 'INTERVAL', intervalMinutes: 90, windowStart: '09:00', windowEnd: null, weekdays: [0, 6] }, 'UTC'),
    'Every 1 hour 30 minutes between 09:00 and 24:00 (UTC) on weekends');

  const feed = F.feedItem({ id: 'f1', type: 'RSS', source: 'https://x.test/f', channelId: CH_TEXT, displayName: 'X', enabled: true, createdAt: at },
    { statusText: '✅ OK', sourceUrl: 'https://x.test/f' });
  assert.deepEqual([feed.pingRoleId, feed.messageTemplate, feed.includeReposts, feed.createdAt, feed.lastPostedAt], [null, null, false, at.toISOString(), null]);
  const trig = TR.triggerItem({ id: 't1', trigger: 'hi', matchType: 'EXACT', response: 'yo', cooldown: 5, enabled: false, uses: 2 });
  assert.deepEqual([trig.caseSensitive, trig.channelIds, trig.enabled, trig.uses], [false, [], false, 2]);
});

// ── Stores against Postgres (opt-in) ─────────────────────────────────────────

class FakeDiscordApiError extends Error {
  constructor(code, status, discordCode) {
    super(`Discord returned HTTP ${status}`);
    this.name = 'DiscordApiError';
    this.code = code;
    this.status = status;
    if (discordCode !== undefined) this.discordCode = discordCode;
  }
}

/** A Discord REST client double: records every write; `fail.<op>` makes the next call of that op throw. */
function fakeDiscord(resources = RES) {
  let n = 0n;
  const calls = { send: [], edit: [], del: [] };
  const fail = {};
  const take = (op) => {
    const err = fail[op];
    delete fail[op];
    if (err) throw err;
  };
  return {
    calls,
    fail,
    async getGuildResources() { return resources; },
    async sendMessage(channelId, payload) {
      calls.send.push({ channelId, payload });
      take('send');
      n += 1n;
      return { id: String(955017000000100000n + n), channelId };
    },
    async editMessage(channelId, messageId, payload) {
      calls.edit.push({ channelId, messageId, payload });
      take('edit');
      return { id: messageId, channelId };
    },
    async deleteMessage(channelId, messageId) {
      calls.del.push({ channelId, messageId });
      take('del');
    },
  };
}
const silentLogger = { error() {}, warn() {}, info() {}, debug() {} };
// runTask: the bot's channel access check (access.channels) finds no problem; test/sec-dashboard.test.js covers refusals.
const ctxFor = (discord, member = { owner: false, permissions: '0' }) => ({
  userId: USER, user: { id: USER, username: 'alice', globalName: 'Alice', avatar: null }, member, discord, logger: silentLogger,
  runTask: async () => ({ status: 'done', message: 'ok', data: { problems: [] } }),
});
const rejectsWith = (promise, code, extra = {}) => assert.rejects(promise, (err) => {
  assert.ok(err instanceof RecordError, `expected a RecordError, got ${err?.name}: ${err?.message}`);
  assert.equal(err.code, code, err.message);
  if (extra.path !== undefined) assert.equal(err.path, extra.path, err.message);
  if (extra.message) assert.match(err.message, extra.message);
  return true;
});

test('db: saved embeds — create, edit, post, move, update in place, unpost, delete', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/embeds.store.js');
  const discord = fakeDiscord();
  const ctx = ctxFor(discord);
  const data = create('embeds', {
    name: 'rules',
    template: { content: `Read the rules <@&${ROLE_QUIET}> <@&${ROLE_PING}>`, embed: { title: '{server} rules' }, buttons: [] },
  });
  const item = await store.create(GUILD, data, ctx);
  assert.equal(item.status, 'Draft');
  assert.equal(item.name, 'rules');
  await rejectsWith(store.create(GUILD, data, ctx), 'conflict', { path: 'name' });
  assert.equal((await store.list(GUILD)).length, 1);
  assert.equal(await store.get(GUILD2, item.id), null, 'another guild never sees it');
  await rejectsWith(store.update(GUILD2, item.id, { template: data.template }, ctx), 'not_found');
  await rejectsWith(store.create(GUILD, { ...data, name: 'bad', template: { content: null, embed: null, buttons: [{ label: 'x', url: '{user.avatar}' }] } }, ctx),
    'invalid', { path: 'template.buttons.0' });

  // A draft edit touches no message.
  const drafted = await store.update(GUILD, item.id, { template: { ...data.template, content: `Rules <@&${ROLE_QUIET}> <@&${ROLE_PING}>` } }, ctx);
  assert.equal(drafted.template.content, `Rules <@&${ROLE_QUIET}> <@&${ROLE_PING}>`);
  assert.equal(discord.calls.edit.length, 0);
  assert.equal(await embedService.getSavedEmbedTemplate(GUILD, 'rules').then((t) => t?.content), drafted.template.content);

  // Post: rendered with the server, pinging only the mentionable role.
  let r = await store.actions.publish(GUILD, item.id, { channelId: CH_TEXT, deletePrevious: true }, ctx);
  assert.equal(r.item.status, 'Posted');
  assert.equal(r.item.channelId, CH_TEXT);
  const first = discord.calls.send[0];
  assert.equal(first.channelId, CH_TEXT);
  assert.equal(first.payload.embeds[0].title, 'Test Server rules');
  assert.deepEqual(first.payload.allowed_mentions, { parse: ['users'], roles: [ROLE_PING] });
  assert.equal(r.item.messageId, String(955017000000100001n));
  assert.match(r.message, /Posted in #general\. .*Not pinged.*@quiet/);

  // Editing a posted embed edits the message first (no pings, text cleared when removed).
  const edited = await store.update(GUILD, item.id, { template: { content: null, embed: { title: 'New title' }, buttons: [] } }, ctx);
  assert.equal(edited.status, 'Posted');
  const ed = discord.calls.edit.at(-1);
  assert.deepEqual([ed.channelId, ed.messageId, ed.payload.content, ed.payload.embeds[0].title], [CH_TEXT, r.item.messageId, '', 'New title']);
  assert.deepEqual(ed.payload.allowed_mentions, { parse: [] });

  // A refused edit saves nothing.
  discord.fail.edit = new FakeDiscordApiError('forbidden', 403, 50013);
  await rejectsWith(store.update(GUILD, item.id, { template: { content: 'never saved' } }, ctx), 'discord', { message: /edit its message in #general/ });
  assert.equal((await store.get(GUILD, item.id)).template.embed.title, 'New title');

  // Move to #news: post there, delete the old copy.
  r = await store.actions.publish(GUILD, item.id, { channelId: CH_NEWS, deletePrevious: true }, ctx);
  assert.deepEqual(discord.calls.del.at(-1), { channelId: CH_TEXT, messageId: String(955017000000100001n) });
  assert.match(r.message, /previous copy in #general was deleted/);
  assert.equal(r.item.channelId, CH_NEWS);
  // Post a second copy and keep the first one.
  const delsBefore = discord.calls.del.length;
  r = await store.actions.publish(GUILD, item.id, { channelId: CH_TEXT, deletePrevious: false }, ctx);
  assert.equal(discord.calls.del.length, delsBefore);
  assert.match(r.message, /previous copy in #news stays/);
  // A refused post changes nothing.
  discord.fail.send = new FakeDiscordApiError('forbidden', 403, 50001);
  await rejectsWith(store.actions.publish(GUILD, item.id, { channelId: CH_NEWS, deletePrevious: true }, ctx), 'discord', { message: /can't post in #news/ });
  assert.equal((await store.get(GUILD, item.id)).channelId, CH_TEXT);

  // Update in place.
  r = await store.actions.refresh(GUILD, item.id, {}, ctx);
  assert.match(r.message, /was updated/);
  // The message was deleted in Discord: the embed becomes a draft again, the channel is kept as a hint.
  discord.fail.edit = new FakeDiscordApiError('not_found', 404, 10008);
  r = await store.actions.refresh(GUILD, item.id, {}, ctx);
  assert.equal(r.item.status, 'Draft');
  assert.equal(r.item.channelId, CH_TEXT);
  assert.match(r.message, /draft again/);
  await rejectsWith(store.actions.refresh(GUILD, item.id, {}, ctx), 'invalid');
  // The channel was deleted: an edit saves the template and drops the channel.
  r = await store.actions.publish(GUILD, item.id, { channelId: CH_TEXT, deletePrevious: true }, ctx);
  discord.fail.edit = new FakeDiscordApiError('not_found', 404, 10003);
  const gone = await store.update(GUILD, item.id, { template: { content: 'saved anyway' } }, ctx);
  assert.deepEqual([gone.status, gone.channelId, gone.template.content], ['Draft', null, 'saved anyway']);

  // Unpost: deletes the message, keeps the draft; a 404 counts as done.
  r = await store.actions.publish(GUILD, item.id, { channelId: CH_NEWS, deletePrevious: true }, ctx);
  const postedId = r.item.messageId;
  r = await store.actions.unpublish(GUILD, item.id, {}, ctx);
  assert.deepEqual(discord.calls.del.at(-1), { channelId: CH_NEWS, messageId: postedId });
  assert.deepEqual([r.item.status, r.item.messageId, r.item.channelId], ['Draft', null, CH_NEWS]);
  await rejectsWith(store.actions.unpublish(GUILD, item.id, {}, ctx), 'invalid');
  await store.actions.publish(GUILD, item.id, { channelId: CH_NEWS, deletePrevious: true }, ctx);
  discord.fail.del = new FakeDiscordApiError('not_found', 404, 10008);
  assert.equal((await store.actions.unpublish(GUILD, item.id, {}, ctx)).item.status, 'Draft');

  // Delete keeps any posted message (like /embed delete) and forgets the cached template.
  await store.remove(GUILD, item.id, ctx);
  assert.equal(await store.get(GUILD, item.id), null);
  assert.equal(await embedService.getSavedEmbedTemplate(GUILD, 'rules'), null);
  await rejectsWith(store.remove(GUILD, item.id, ctx), 'not_found');
});

test('db: saved embeds — the per-guild cap holds', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/embeds.store.js');
  const ctx = ctxFor(fakeDiscord());
  const first = await store.create(GUILD2, create('embeds', { name: 'first', template: { content: 'x' } }), ctx);
  await prisma.savedEmbed.createMany({
    data: Array.from({ length: LIMITS.SAVED_EMBEDS - 1 }, (_, i) => ({ guildId: GUILD2, name: `bulk-${i}`, template: { content: 'x' }, createdBy: USER })),
  });
  await rejectsWith(store.create(GUILD2, create('embeds', { name: 'one-more', template: { content: 'x' } }), ctx), 'limit');
  await prisma.savedEmbed.deleteMany({ where: { guildId: GUILD2, NOT: { id: first.id } } });
  assert.equal((await store.list(GUILD2)).length, 1);
});

test('db: custom commands — actions, role rules, legacy rows, cooldown reset', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/tags.store.js');
  const discord = fakeDiscord();
  const ctx = ctxFor(discord);
  const data = create('tags', {
    name: 'hello',
    description: 'Says hi',
    actions: [
      { type: 'reply', responses: [{ text: 'Hi {user}' }, { text: 'Hey {1:you}' }], pingRoleIds: [ROLE_PING] },
      { type: 'send', channelId: CH_NEWS, embed: true, responses: [{ text: 'logged' }] },
    ],
    cooldownScope: 'USER',
    cooldownSeconds: 30,
    allowRoleIds: [ROLE_PING],
  });
  const item = await store.create(GUILD, data, ctx);
  assert.equal(item.actions.length, 2);
  assert.deepEqual(item.actions[0].responses, [{ text: 'Hi {user}' }, { text: 'Hey {1:you}' }]);
  const row = await prisma.customTag.findUnique({ where: { id: item.id } });
  assert.deepEqual([row.content, row.useEmbed, row.cooldownSeconds, row.createdBy], ['Hi {user}', false, 30, USER]);
  assert.deepEqual(tagService.normalizeActions(row.actions), row.actions, 'stored like /tag action add stores');
  assert.equal((await tagService.findTag(GUILD, 'hello')).id, item.id, 'the bot finds it by name');
  await rejectsWith(store.create(GUILD, data, ctx), 'conflict', { path: 'name' });

  // Ping roles: a non-mentionable one needs Mention @everyone.
  const withQuiet = [{ ...item.actions[0], pingRoleIds: [ROLE_QUIET] }];
  await rejectsWith(store.update(GUILD, item.id, { actions: withQuiet }, ctx), 'invalid', { path: 'actions.0.pingRoleIds.0' });
  assert.equal((await store.update(GUILD, item.id, { actions: withQuiet }, ctxFor(discord, { owner: true, permissions: '0' }))).actions[0].pingRoleIds[0], ROLE_QUIET);
  // Role actions: never a role with moderator permissions, never one above the member (when known).
  const give = (roleId) => [...item.actions, { type: 'addRole', responses: [], embed: false, channelId: null, roleId, pingRoleIds: [] }];
  await rejectsWith(store.update(GUILD, item.id, { actions: give(ROLE_PING) }, ctxFor(discord, { owner: false, permissions: '0', highestRolePosition: 2 })),
    'invalid', { path: 'actions.2.roleId', message: /highest role/ });
  await rejectsWith(store.update(GUILD, item.id, { actions: give(ROLE_MOD) }, ctxFor(discord, { owner: true })), 'invalid', { message: /dangerous/ });
  await rejectsWith(store.update(GUILD, item.id, { actions: give(ROLE_FLAGGED) }, ctx), 'invalid', { message: /moderator or admin/ });
  const withRole = await store.update(GUILD, item.id, { actions: give(ROLE_PING) }, ctx);
  assert.deepEqual(withRole.actions[2], { type: 'addRole', responses: [], embed: false, channelId: null, roleId: ROLE_PING, pingRoleIds: [] });
  // Unchanged roles are never re-checked.
  assert.equal((await store.update(GUILD, item.id, { actions: withRole.actions, description: 'x' }, ctx)).description, 'x');

  // Cooldown: NONE stores 0 and clears the running cooldowns.
  const none = await store.update(GUILD, item.id, { cooldownScope: 'NONE' }, ctx);
  assert.deepEqual([none.cooldownScope, none.cooldownSeconds], ['NONE', 0]);

  // A legacy command (actions null) shows its content; unrelated edits keep it legacy.
  const legacy = await prisma.customTag.create({ data: { guildId: GUILD, name: 'legacy', content: 'old text', useEmbed: true, createdBy: USER } });
  const shown = await store.get(GUILD, legacy.id);
  assert.deepEqual(shown.actions, [{ type: 'reply', responses: [{ text: 'old text' }], embed: true, channelId: null, roleId: null, pingRoleIds: [] }]);
  await store.update(GUILD, legacy.id, { enabled: false }, ctx);
  assert.equal((await prisma.customTag.findUnique({ where: { id: legacy.id } })).actions, null);
  await store.update(GUILD, legacy.id, { actions: [{ ...shown.actions[0], responses: [{ text: 'new text' }] }] }, ctx);
  const upgraded = await prisma.customTag.findUnique({ where: { id: legacy.id } });
  assert.deepEqual([upgraded.content, upgraded.useEmbed, upgraded.enabled], ['new text', true, false]);

  assert.equal((await store.list(GUILD)).map((t) => t.name).join(), 'hello,legacy');
  await store.remove(GUILD, legacy.id, ctx);
  await rejectsWith(store.remove(GUILD, legacy.id, ctx), 'not_found');
  assert.equal(await store.get(GUILD2, item.id), null);
});

test('db: auto-responders — create, safe patterns, scope, delete, cap', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/triggers.store.js');
  const ctx = ctxFor(fakeDiscord());
  const item = await store.create(GUILD, create('triggers', {
    trigger: '(a+)+', response: 'Hi {user}', channelIds: [CH_TEXT], exemptRoleIds: [ROLE_QUIET], cooldown: 10,
  }), ctx);
  assert.deepEqual([item.matchType, item.channelIds, item.exemptRoleIds, item.cooldown, item.enabled], ['CONTAINS', [CH_TEXT], [ROLE_QUIET], 10, true]);
  // Turning an unsafe pattern into a regex is refused by the store too (not only by the form check).
  await rejectsWith(store.update(GUILD, item.id, { matchType: 'REGEX' }, ctx), 'invalid', { path: 'trigger', message: /Nested/ });
  const paused = await store.update(GUILD, item.id, { enabled: false, response: 'Bye' }, ctx);
  assert.deepEqual([paused.enabled, paused.response], [false, 'Bye']);
  const set = await tagService.getTriggerSet(GUILD);
  assert.equal(set.entries.length, 0, 'the paused trigger is not in the bot\'s set');
  await store.update(GUILD, item.id, { enabled: true }, ctx);
  tagService.__testing.reset(); // the bot's memo; the shared Redis list was dropped by the store
  assert.equal((await tagService.getTriggerSet(GUILD)).entries.length, 1);

  await prisma.autoTrigger.createMany({
    data: Array.from({ length: LIMITS.AUTO_TRIGGERS - 1 }, (_, i) => ({ guildId: GUILD, trigger: `t${i}`, response: 'x', createdBy: USER })),
  });
  await rejectsWith(store.create(GUILD, create('triggers', { trigger: 'more', response: 'x' }), ctx), 'limit');
  assert.equal((await store.list(GUILD))[0].id, item.id, 'creation order');
  await prisma.autoTrigger.deleteMany({ where: { guildId: GUILD, NOT: { id: item.id } } });
  await store.remove(GUILD, item.id, ctx);
  await rejectsWith(store.remove(GUILD, item.id, ctx), 'not_found');
});

test('db: reminders — schedule in the server time zone, pause, resume, test, delete', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/reminders.store.js');
  const { updateSettings } = await import('../src/services/settingsService.js');
  const discord = fakeDiscord();
  const ctx = ctxFor(discord);
  await updateSettings(GUILD, { timezone: 'Europe/Berlin' });
  const job = (id) => prisma.scheduledJob.findUnique({ where: { dedupeKey: `reminder:${id}` } });

  const item = await store.create(GUILD, create('reminders', { name: 'standup', channelId: CH_TEXT, timeOfDay: '09:30', pingRoleId: ROLE_PING }), ctx);
  assert.deepEqual([item.status, item.timezone, item.mode], ['Active', 'Europe/Berlin', 'TIME_OF_DAY']);
  let j = await job(item.id);
  assert.equal(j.type, 'reminder.fire');
  assert.equal(j.payload.reminderId, item.id);
  assert.equal(j.runAt.toISOString(), item.nextRunAt);
  const local = new Intl.DateTimeFormat('en-GB', { timeZone: 'Europe/Berlin', hour: '2-digit', minute: '2-digit' }).format(j.runAt);
  assert.equal(local, '09:30', 'the time of day is in the server time zone');
  await rejectsWith(store.create(GUILD, create('reminders', { name: 'standup', channelId: CH_TEXT }), ctx), 'conflict', { path: 'name' });
  await rejectsWith(store.create(GUILD, create('reminders', { name: 'quiet', channelId: CH_TEXT, pingRoleId: ROLE_QUIET }), ctx), 'invalid', { path: 'pingRoleId' });

  // Pause cancels the job; resume brings it back.
  let upd = await store.update(GUILD, item.id, { enabled: false }, ctx);
  assert.deepEqual([upd.status, upd.nextRunAt, await job(item.id)], ['Paused', null, null]);
  upd = await store.update(GUILD, item.id, { enabled: true }, ctx);
  assert.ok(await job(item.id));

  // Interval with a daily window; then once.
  upd = await store.update(GUILD, item.id, { mode: 'INTERVAL', intervalMinutes: 30, windowStart: '08:00', windowEnd: '18:00' }, ctx);
  assert.match(upd.schedule, /^Every 30 minutes between 08:00 and 18:00/);
  assert.equal((await job(item.id)).runAt.toISOString(), upd.nextRunAt);
  await rejectsWith(store.update(GUILD, item.id, { mode: 'INTERVAL', intervalMinutes: 1440, windowStart: '08:00' }, ctx), 'invalid', { path: 'intervalMinutes' });
  const runAt = new Date(Date.now() + 3 * 3_600_000);
  runAt.setUTCSeconds(0, 0);
  upd = await store.update(GUILD, item.id, { mode: 'ONE_SHOT', runAt: runAt.toISOString() }, ctx);
  assert.deepEqual([upd.runAt, (await job(item.id)).runAt.toISOString(), upd.timeOfDay, upd.intervalMinutes], [runAt.toISOString(), runAt.toISOString(), null, null]);
  await rejectsWith(store.update(GUILD, item.id, { runAt: new Date(Date.now() - 60_000).toISOString() }, ctx), 'invalid', { path: 'runAt', message: /has passed/ });

  // A finished one-off: saving the whole form with only a new channel works; turning it back on needs a new time.
  await prisma.reminder.update({ where: { id: item.id }, data: { enabled: false, runAt: new Date(Date.now() - 86_400_000), lastSentAt: new Date() } });
  const finished = await store.get(GUILD, item.id);
  assert.equal(finished.status, 'Sent');
  const { id: _id, status: _s, schedule: _sc, nextRunAt: _n, lastSentAt: _l, timezone: _t, name: _name, ...form } = finished;
  upd = await store.update(GUILD, item.id, { ...form, channelId: CH_NEWS, weekdays: ['sun', 'mon'], intervalMinutes: 15 }, ctx);
  assert.equal(upd.channelId, CH_NEWS);
  await rejectsWith(store.update(GUILD, item.id, { enabled: true }, ctx), 'invalid', { path: 'runAt', message: /time has passed/ });
  upd = await store.update(GUILD, item.id, { mode: 'TIME_OF_DAY', timeOfDay: '07:00', enabled: true }, ctx);
  assert.equal(upd.status, 'Active');

  // Test post: in the reminder's channel, the ping role shown but not pinged; the schedule is untouched.
  const before = await job(item.id);
  const r = await store.actions.test(GUILD, item.id, { channelId: null }, ctx);
  const sent = discord.calls.send.at(-1);
  assert.equal(sent.channelId, CH_NEWS);
  assert.equal(sent.payload.content, `<@&${ROLE_PING}> Hey, I'm a reminder!`);
  assert.deepEqual(sent.payload.allowed_mentions, { parse: [] });
  assert.match(r.message, /#news/);
  assert.equal((await job(item.id)).version, before.version);

  assert.equal(await store.get(GUILD2, item.id), null);
  await store.remove(GUILD, item.id, ctx);
  assert.equal(await job(item.id), null);
  await rejectsWith(store.remove(GUILD, item.id, ctx), 'not_found');
});

test('db: feeds — resolve and seed, schedule, edit, pause, re-enable, check, remove', { skip: skipDb }, async () => {
  const { store } = await import('../src/dashboard/collections/feeds.store.js');
  const discord = fakeDiscord();
  const ctx = ctxFor(discord);
  const RSS = `<?xml version="1.0"?><rss version="2.0"><channel><title>Example &amp; Co</title><link>https://example.com/</link>
<item><title>Second</title><link>https://example.com/2</link><guid isPermaLink="false">post-2</guid><pubDate>Fri, 09 Oct 2026 10:00:00 GMT</pubDate></item>
<item><title>First</title><link>https://example.com/1</link><pubDate>Thu, 08 Oct 2026 10:00:00 GMT</pubDate></item></channel></rss>`;
  let status = 200;
  const fetched = [];
  const resp = (url) => {
    const buffer = Buffer.from(RSS);
    return {
      ok: status < 300, status, statusText: '', headers: { 'content-type': 'application/rss+xml' }, url, buffer,
      text: () => buffer.toString('utf8'), json: () => JSON.parse(buffer.toString('utf8')),
    };
  };
  feedService.__setFetchImpl({ safeFetch: async (url) => { fetched.push(url); return resp(url); }, safeFetchJson: async () => ({}) });
  const job = (id) => prisma.scheduledJob.findUnique({ where: { dedupeKey: `feed.poll:${id}` } });

  const t0 = Date.now();
  const item = await store.create(GUILD, create('feeds', { type: 'RSS', source: 'feed.test/f.xml', channelId: CH_TEXT, messageTemplate: 'New: {title}\\n{link}' }), ctx);
  assert.deepEqual([item.displayName, item.source, item.sourceUrl], ['Example & Co', 'https://feed.test/f.xml', 'https://feed.test/f.xml']);
  assert.equal(item.messageTemplate, 'New: {title}\n{link}', 'a typed \\n becomes a new line, like /feed');
  const row = await prisma.feed.findUnique({ where: { id: item.id } });
  assert.deepEqual([row.initialized, row.seenIds.length, row.createdBy], [true, 2, USER], 'the probe seeds the feed: no backlog');
  let j = await job(item.id);
  assert.equal(j.type, 'feed.poll');
  assert.ok(Math.abs(j.runAt.getTime() - (t0 + feedService.BASE_INTERVAL_MS.RSS)) < 60_000, 'first check one interval later');
  await rejectsWith(store.create(GUILD, create('feeds', { type: 'RSS', source: 'https://feed.test/f.xml', channelId: CH_TEXT }), ctx), 'conflict');

  let upd = await store.update(GUILD, item.id, { messageTemplate: null, channelId: CH_NEWS, pingRoleId: ROLE_PING }, ctx);
  assert.deepEqual([upd.messageTemplate, upd.channelId, upd.pingRoleId], [null, CH_NEWS, ROLE_PING]);
  await rejectsWith(store.update(GUILD, item.id, { pingRoleId: ROLE_QUIET }, ctx), 'invalid', { path: 'pingRoleId' });
  await rejectsWith(store.update(GUILD, item.id, { includeReposts: true }, ctx), 'invalid', { path: 'includeReposts' });

  // Pause and re-enable like /feed disable|enable.
  upd = await store.update(GUILD, item.id, { enabled: false }, ctx);
  assert.match(upd.statusText, /Paused by an admin/);
  assert.equal(await job(item.id), null);
  await prisma.feed.update({ where: { id: item.id }, data: { failureCount: 7, lastError: 'HTTP 500' } });
  upd = await store.update(GUILD, item.id, { enabled: true }, ctx);
  const after = await prisma.feed.findUnique({ where: { id: item.id } });
  assert.deepEqual([after.enabled, after.failureCount, after.lastError, after.disabledReason, after.initialized], [true, 0, null, null, false]);
  j = await job(item.id);
  assert.ok(j.runAt.getTime() - Date.now() < 30_000, 'checked again within seconds');
  // Re-enabling with a deleted channel is refused.
  await prisma.feed.update({ where: { id: item.id }, data: { enabled: false, channelId: '955017000000000888' } });
  await rejectsWith(store.update(GUILD, item.id, { enabled: true }, ctx), 'invalid', { path: 'channelId' });
  await store.update(GUILD, item.id, { channelId: CH_TEXT, enabled: true }, ctx);

  // Check now: fetches and reports, writes nothing; throttled for a few seconds.
  const before = await prisma.feed.findUnique({ where: { id: item.id } });
  const r = await store.actions.test(GUILD, item.id, {}, ctx);
  assert.match(r.message, /The feed works: 2 item\(s\)\. Newest: "Second"/);
  assert.deepEqual((await prisma.feed.findUnique({ where: { id: item.id } })).updatedAt, before.updatedAt);
  await rejectsWith(store.actions.test(GUILD, item.id, {}, ctx), 'invalid', { message: /just checked/ });

  // A source that does not answer is refused before anything is stored.
  status = 404;
  await rejectsWith(store.create(GUILD, create('feeds', { type: 'RSS', source: 'https://feed.test/missing.xml', channelId: CH_TEXT }), ctx),
    'invalid', { path: 'source', message: /Could Not Fetch: .*HTTP 404/ });
  status = 200;

  // The cap is checked before any request.
  await prisma.feed.createMany({
    data: Array.from({ length: LIMITS.FEEDS - 1 }, (_, i) => ({
      guildId: GUILD, type: 'RSS', source: `https://bulk.test/${i}`, displayName: `b${i}`, fetchUrl: `https://bulk.test/${i}`, channelId: CH_TEXT, createdBy: USER,
    })),
  });
  const calls = fetched.length;
  await rejectsWith(store.create(GUILD, create('feeds', { type: 'RSS', source: 'https://feed.test/other.xml', channelId: CH_TEXT }), ctx), 'limit');
  assert.equal(fetched.length, calls);
  await prisma.feed.deleteMany({ where: { guildId: GUILD, NOT: { id: item.id } } });

  await store.remove(GUILD, item.id, ctx);
  assert.equal(await job(item.id), null);
  await rejectsWith(store.remove(GUILD, item.id, ctx), 'not_found');
});
