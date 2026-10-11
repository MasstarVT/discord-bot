// pkg-welcome tests (pkg-welcome.md §8): the template truth table, healing, payloads, card style, autorole
// helpers, the senders and hook bodies against fakes, the command payloads, the plugin declaration and a
// static scan. Offline by default. With TEST_DATABASE_URL (a migrated database) the DB cases also run:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/welcome.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back (flood caps fail open), and Postgres is only used with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { ButtonStyle, PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const svc = await import('../src/services/welcomeService.js');
const card = await import('../src/utils/welcomeCard.js');
const { readImageSize } = await import('../src/utils/imageSize.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const welcomeCmd = await import('../src/commands/admin/welcome.js');
const goodbyeCmd = await import('../src/commands/admin/goodbye.js');
const { Prisma } = await import('@prisma/client');

const {
  DEFAULT_WELCOME_MESSAGE, DEFAULT_WELCOME_DM_MESSAGE, DEFAULT_GOODBYE_MESSAGE,
  DEFAULT_WELCOME_EMBED, DEFAULT_GOODBYE_EMBED, MAX_AUTOROLE_DELAY_SECONDS, SCREENING_WATCH_MS,
  healEmbed, hasEmbedContent, serializeEmbed, parseCardStyle, serializeCardStyle, welcomeCardEnabled, materialiseWelcome,
  resolveTemplate, buildWelcomePayload, parseDelayInput, resolveAutoroles, isCaptchaGated, autoroleDueAt,
  isScreeningCompletion, resolveAutoroleInput, diagnoseWelcome, grantAutoroles, sendWelcomeChannel, sendWelcomeDm,
  sendGoodbye, onMemberJoin, onMemberLeave, buildSetupModal, isStaleSetupForm, toDbData, autoroleJobKey,
  WelcomeConfigError,
} = svc;

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};
after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) {
    // Leave nothing behind in a shared test database.
    await prisma.scheduledJob.deleteMany({ where: { guildId: '955008000000000001' } }).catch(() => {});
    await prisma.guildSettings.deleteMany({ where: { guildId: '955008000000000001' } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ────────────────────────────────────────────────────────────────────

const UID = '111111111111111111';
const BASE = Object.freeze({
  welcomeEnabled: true, welcomeUseEmbed: true, welcomeMessageEnabled: true, goodbyeUseEmbed: false, welcomeDmCard: false,
  welcomeMessage: null, welcomeEmbed: null, welcomeCardStyle: null, welcomeDmMessage: null, welcomeDmEmbed: null,
  welcomeChannelId: null, welcomeDmEnabled: false, goodbyeEnabled: false, goodbyeChannelId: null, goodbyeMessage: null,
  goodbyeEmbed: null, autoroleIds: [], autoroleDelay: 0, embedColor: null, captchaEnabled: false, captchaRoleId: null,
});
const settings = (over = {}) => ({ ...BASE, ...over });

function role(id, { position = 1, managed = false, permissions = [], name } = {}) {
  return { id, name: name ?? `role-${id}`, position, managed, permissions: new PermissionsBitField(permissions) };
}

function makeGuild({ id = 'G', name = 'Test $& Server' } = {}) {
  return {
    id, name, memberCount: 22, ownerId: 'O', available: true,
    roles: { cache: new Map() },
    channels: { cache: new Map(), fetch: async () => { throw Object.assign(new Error('Unknown Channel'), { code: 10003 }); } },
    members: { me: { roles: { highest: { position: 10 } }, permissions: { has: () => true } } },
    iconURL: () => null, features: [],
  };
}

function makeMember(guild, over = {}) {
  const user = {
    id: UID, username: 'newbie', tag: 'newbie', bot: false, globalName: null,
    displayAvatarURL: () => 'https://cdn.discordapp.com/a.png', createdTimestamp: 1_600_000_000_000,
  };
  const added = [];
  const heldRoles = new Map();
  return {
    id: UID, user, displayName: 'Newbie', guild,
    roles: { cache: heldRoles, add: async (id, reason) => { added.push({ id, reason }); heldRoles.set(id, { id }); } },
    pending: false, joinedTimestamp: Date.now(),
    displayAvatarURL: () => 'https://cdn.discordapp.com/a.png',
    sent: [], send: async function send(p) { this.sent.push(p); },
    added,
    ...over,
  };
}

function makeChannel(guild, { id = '200000000000000001', missing = [], sendImpl = null } = {}) {
  const sent = [];
  const channel = {
    id, name: 'welcome', guild, sent,
    isTextBased: () => true, isThread: () => false,
    permissionsFor: () => ({ has: (bit) => !missing.some((k) => PermissionFlagsBits[k] === bit) }),
    send: async (payload) => { sent.push(payload); if (sendImpl) return sendImpl(payload, sent.length); return {}; },
  };
  guild.channels.cache.set(id, channel);
  return channel;
}

const apiError = (code, message = 'api error') => Object.assign(new Error(message), { code });
const noCard = async () => null;
const embedJson = (p) => p.embeds[0].toJSON();

// ── 1. Truth table ───────────────────────────────────────────────────────────

test('resolveTemplate: welcome truth table (all 9 cells)', () => {
  const custom = { title: 'Hello' };
  const r = (over) => resolveTemplate('welcome', settings(over));

  // U false: embed never, the text follows M.
  assert.deepEqual(r({ welcomeUseEmbed: false }), { content: DEFAULT_WELCOME_MESSAGE, embed: null });
  assert.deepEqual(r({ welcomeUseEmbed: false, welcomeMessage: '' }), { content: null, embed: null });
  assert.deepEqual(r({ welcomeUseEmbed: false, welcomeMessage: 'hi' }), { content: 'hi', embed: null });
  assert.deepEqual(r({ welcomeUseEmbed: false, welcomeEmbed: custom }), { content: DEFAULT_WELCOME_MESSAGE, embed: null });

  // U true, no custom embed (empty / card-only / never set).
  assert.deepEqual(r({}), { content: DEFAULT_WELCOME_MESSAGE, embed: DEFAULT_WELCOME_EMBED });
  assert.deepEqual(r({ welcomeEmbed: { card: true } }), { content: DEFAULT_WELCOME_MESSAGE, embed: DEFAULT_WELCOME_EMBED });
  assert.deepEqual(r({ welcomeMessage: '' }), { content: null, embed: DEFAULT_WELCOME_EMBED });
  assert.deepEqual(r({ welcomeMessage: 'hi' }), { content: 'hi', embed: DEFAULT_WELCOME_EMBED });

  // U true, custom embed: a null message stays "no text" (legacy configs unchanged).
  assert.deepEqual(r({ welcomeEmbed: custom }), { content: null, embed: { title: 'Hello' } });
  assert.deepEqual(r({ welcomeEmbed: custom, welcomeMessage: '' }), { content: null, embed: { title: 'Hello' } });
  assert.deepEqual(r({ welcomeEmbed: custom, welcomeMessage: 'hi' }), { content: 'hi', embed: { title: 'Hello' } });
});

test('resolveTemplate: goodbye table, DM rules and forceText', () => {
  const g = (over, opts) => resolveTemplate('goodbye', settings(over), opts);
  assert.deepEqual(g({}), { content: DEFAULT_GOODBYE_MESSAGE, embed: null });
  assert.deepEqual(g({ goodbyeMessage: '' }), { content: null, embed: null });
  assert.deepEqual(g({ goodbyeMessage: 'bye' }), { content: 'bye', embed: null });
  assert.deepEqual(g({ goodbyeUseEmbed: true }), { content: null, embed: DEFAULT_GOODBYE_EMBED });
  assert.deepEqual(g({ goodbyeUseEmbed: true, goodbyeMessage: '' }), { content: null, embed: DEFAULT_GOODBYE_EMBED });
  assert.deepEqual(g({ goodbyeUseEmbed: true, goodbyeMessage: 'bye', goodbyeEmbed: { title: 'T' } }), { content: 'bye', embed: { title: 'T' } });
  assert.deepEqual(g({ goodbyeUseEmbed: true }, { forceText: true }), { content: DEFAULT_GOODBYE_MESSAGE, embed: null });

  const d = (over, opts) => resolveTemplate('dm', settings(over), opts);
  assert.deepEqual(d({}), { content: DEFAULT_WELCOME_DM_MESSAGE, embed: null });
  assert.deepEqual(d({ welcomeDmMessage: '' }), { content: null, embed: null });
  assert.deepEqual(d({ welcomeDmEmbed: { description: 'x' } }), { content: DEFAULT_WELCOME_DM_MESSAGE, embed: { description: 'x' } });
  assert.deepEqual(d({ welcomeDmEmbed: { description: 'x' } }, { forceText: true }), { content: DEFAULT_WELCOME_DM_MESSAGE, embed: null });
});

// ── 2/3. Healing (R053, R015, R052) ──────────────────────────────────────────

test('R053: a card-only legacy embed is "no custom embed"', () => {
  assert.equal(healEmbed({ card: true }), null);
  assert.equal(healEmbed({ card: false }), null);
  assert.equal(healEmbed(null), null);
  assert.equal(healEmbed('x'), null);
  assert.equal(healEmbed([1]), null);
  assert.deepEqual(resolveTemplate('welcome', settings({ welcomeEmbed: { card: false } })).embed, DEFAULT_WELCOME_EMBED);
  // A footer-only or thumbnail-only embed counts as empty; a title, description, fields, author or image does not.
  assert.equal(hasEmbedContent({ footer: { text: 'f' } }), false);
  assert.equal(hasEmbedContent({ thumbnail: 'x' }), false);
  assert.equal(hasEmbedContent({ title: 't' }), true);
  assert.equal(hasEmbedContent({ fields: [{ name: 'a', value: 'b' }] }), true);
  assert.equal(hasEmbedContent({ author: { name: 'a' } }), true);
  assert.equal(hasEmbedContent({ image: 'https://x.test/a.png' }), true);
});

test('R015/R052: bad stored values are healed and rendering never throws', () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const legacy = { title: 'x'.repeat(300), color: 0xFFFFFFF, footer: 'f'.repeat(3000), thumbnail: 'user_avatar', card: true };
  const healed = healEmbed(legacy);
  assert.equal(healed.color, undefined);
  assert.ok(healed.title.length <= 256);
  assert.ok(healed.footer.text.length <= 2048);
  assert.equal(healed.thumbnail, '{user.avatar}');
  assert.equal(healEmbed({ title: 't', color: -1 }).color, undefined);
  assert.equal(healEmbed({ title: 't', color: 0x123456 }).color, 0x123456);
  assert.equal(healEmbed({ title: 't', thumbnail: 'javascript:alert(1)' }).thumbnail, undefined);
  assert.equal(healEmbed({ title: 't', fields: Array.from({ length: 40 }, (_, i) => ({ name: `n${i}`, value: 'v' })) }).fields.length, 25);

  const payload = buildWelcomePayload('welcome', { settings: settings({ welcomeEmbed: legacy }), member, guild });
  const e = embedJson(payload);
  assert.ok(e.title.length <= 256);
  assert.ok(e.footer.text.length <= 2048);
  assert.ok(Number.isInteger(e.color) && e.color >= 0 && e.color <= 0xffffff);

  // Even a settings object that throws on access yields the fallback text, not an exception.
  const hostile = new Proxy({}, { get() { throw new Error('boom'); } });
  const fallback = buildWelcomePayload('welcome', { settings: hostile, member, guild });
  assert.equal(fallback.content, 'Hey <@111111111111111111>, welcome to **Test $& Server**!');
});

test('serializeEmbed keeps only keys that carry a value', () => {
  const out = serializeEmbed({
    title: 'T', description: '', url: null, color: 0, author: { name: 'a', iconUrl: null, url: '' }, footer: { text: 'f', iconUrl: null },
    thumbnail: undefined, image: 'i', fields: [{ name: 'n', value: 'v', inline: false, extra: null }], timestamp: true, card: true, junk: 1,
  });
  assert.deepEqual(out, { title: 'T', color: 0, author: { name: 'a' }, footer: { text: 'f' }, image: 'i', fields: [{ name: 'n', value: 'v', inline: false }], timestamp: true });
  assert.deepEqual(serializeEmbed(null), {});
});

// ── 4-9. Payloads ────────────────────────────────────────────────────────────

test('placeholder fidelity (R076/R291): literal $&, ordinal, and only the new member is pinged', () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const p = buildWelcomePayload('welcome', { settings: settings(), member, guild });
  assert.equal(p.content, 'Hey <@111111111111111111>, welcome to **Test $& Server**!');
  assert.match(embedJson(p).description, /22nd/);
  assert.deepEqual(p.allowedMentions, { parse: [], users: ['111111111111111111'] });
  assert.equal(embedJson(p).color, 0x57F287);
});

test('preview payloads never ping', () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  for (const kind of ['welcome', 'dm', 'goodbye']) {
    const p = buildWelcomePayload(kind, { settings: settings(), member, guild, preview: true });
    assert.deepEqual(p.allowedMentions, { parse: [] }, kind);
  }
});

test('the guild embed colour is the default colour of configured embeds', () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const custom = buildWelcomePayload('welcome', { settings: settings({ welcomeEmbed: { title: 'T' }, embedColor: 0x123456 }), member, guild });
  assert.equal(embedJson(custom).color, 0x123456);
  const goodbye = buildWelcomePayload('goodbye', { settings: settings({ goodbyeUseEmbed: true, embedColor: 99999999 }), member, guild });
  assert.equal(embedJson(goodbye).color, 0x99AAB5); // invalid embedColor is ignored
});

test('DM payload: one disabled grey button, clamped label, null when nothing would be sent', () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const p = buildWelcomePayload('dm', { settings: settings(), member, guild });
  assert.equal(p.components.length, 1);
  const row = p.components[0].toJSON();
  assert.equal(row.components.length, 1);
  const b = row.components[0];
  assert.equal(b.custom_id, 'noop:welcome-dm');
  assert.equal(b.style, ButtonStyle.Secondary);
  assert.equal(b.disabled, true);
  assert.equal(b.label, 'Sent from Test $& Server');
  assert.equal(p.content, 'Have a great time here in **Test $& Server**!');

  const longGuild = makeGuild({ name: 'N'.repeat(120) });
  const long = buildWelcomePayload('dm', { settings: settings(), member: makeMember(longGuild), guild: longGuild });
  assert.ok(long.components[0].toJSON().components[0].label.length <= 80);

  assert.equal(buildWelcomePayload('dm', { settings: settings({ welcomeDmMessage: '' }), member, guild }), null);
  // A card alone is enough for a DM.
  const cardOnly = buildWelcomePayload('dm', { settings: settings({ welcomeDmMessage: '' }), member, guild, card: Buffer.from([1]) });
  assert.equal(cardOnly.files.length, 1);
  assert.equal(cardOnly.components.length, 1);
});

test('goodbye payload: text by default, embed when asked, mentions never ping', () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const p = buildWelcomePayload('goodbye', { settings: settings(), member, guild });
  assert.equal(p.content, '**newbie** just left the server');
  assert.deepEqual(p.allowedMentions.parse, []);
  assert.equal(p.embeds.length, 0);

  const e = buildWelcomePayload('goodbye', { settings: settings({ goodbyeUseEmbed: true }), member, guild });
  assert.equal(e.content, undefined);
  assert.equal(embedJson(e).description, '**newbie** just left the server');

  // A member who was not cached: only the user is known.
  const partial = buildWelcomePayload('goodbye', { settings: settings(), member: null, user: member.user, guild });
  assert.equal(partial.content, '**newbie** just left the server');
  assert.equal(buildWelcomePayload('goodbye', { settings: settings({ goodbyeMessage: '' }), member, guild }), null);
});

test('the card goes into the embed, or is a plain attachment in text mode', () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const png = Buffer.from([1]);
  const inEmbed = buildWelcomePayload('welcome', { settings: settings(), member, guild, card: png });
  assert.equal(inEmbed.files.length, 1);
  assert.equal(inEmbed.files[0].name, 'welcome.png');
  assert.equal(embedJson(inEmbed).image.url, 'attachment://welcome.png');

  const text = buildWelcomePayload('welcome', { settings: settings({ welcomeUseEmbed: false }), member, guild, card: png });
  assert.equal(text.files.length, 1);
  assert.equal(text.embeds.length, 0);

  // Text mode with no text still sends the card.
  const cardOnly = buildWelcomePayload('welcome', { settings: settings({ welcomeUseEmbed: false, welcomeMessage: '' }), member, guild, card: png });
  assert.equal(cardOnly.files.length, 1);
  assert.equal(cardOnly.embeds.length, 0);
  assert.equal(buildWelcomePayload('welcome', { settings: settings({ welcomeUseEmbed: false, welcomeMessage: '' }), member, guild }), null);

  // An embed that has its own image keeps it; the card is then a plain attachment.
  const own = buildWelcomePayload('welcome', {
    settings: settings({ welcomeEmbed: { title: 'T', image: 'https://x.test/a.png' } }), member, guild, card: png,
  });
  assert.equal(embedJson(own).image.url, 'https://x.test/a.png');
});

test('forceText drops the embed and falls back to the default text', () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const p = buildWelcomePayload('welcome', { settings: settings({ welcomeEmbed: { title: 'Custom' } }), member, guild, forceText: true });
  assert.equal(p.content, 'Hey <@111111111111111111>, welcome to **Test $& Server**!');
  assert.equal(p.embeds.length, 0);
  assert.equal(resolveTemplate('welcome', settings({ welcomeEmbed: { title: 'Custom' } }), { forceText: true }).content, DEFAULT_WELCOME_MESSAGE);
});

// ── 10/11. Card flag and materialisation (R072) ──────────────────────────────

test('R072: the card flag survives, wherever it is stored', () => {
  assert.equal(welcomeCardEnabled({ welcomeEmbed: { card: false }, welcomeCardStyle: null }), false);
  assert.equal(welcomeCardEnabled(settings()), true);
  assert.equal(welcomeCardEnabled({ welcomeEmbed: { card: true }, welcomeCardStyle: null }), true);
  // The style's own flag wins over the legacy key.
  assert.equal(welcomeCardEnabled({ welcomeEmbed: { card: false }, welcomeCardStyle: { v: 1, enabled: true } }), true);
  assert.equal(welcomeCardEnabled({ welcomeEmbed: { card: true }, welcomeCardStyle: { v: 1, enabled: false } }), false);

  const legacy = settings({ welcomeEmbed: { card: false, title: 'T' } });
  const data = materialiseWelcome(legacy, { welcomeEmbed: { title: 'T' } });
  assert.equal(data.welcomeCardStyle.enabled, false);
  assert.equal(data.welcomeCardStyle.v, 1);

  const styled = settings({ welcomeCardStyle: { v: 1, enabled: true, background: null, backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: null } });
  assert.equal('welcomeCardStyle' in materialiseWelcome(styled, { welcomeEmbed: { title: 'T' } }), false);
  // A style that is already being written is not replaced.
  const written = materialiseWelcome(legacy, { welcomeCardStyle: { v: 1, enabled: true } });
  assert.deepEqual(written.welcomeCardStyle, { v: 1, enabled: true });
});

test('materialiseWelcome freezes the effective text before the embed changes', () => {
  const fresh = materialiseWelcome(settings({ welcomeMessage: null }), { welcomeEmbed: { title: 'T' } });
  assert.equal(fresh.welcomeMessage, DEFAULT_WELCOME_MESSAGE);
  const legacy = materialiseWelcome(settings({ welcomeMessage: null, welcomeEmbed: { title: 'Old' } }), { welcomeEmbed: { title: 'New' } });
  assert.equal(legacy.welcomeMessage, '');
  const explicit = materialiseWelcome(settings({ welcomeMessage: 'mine' }), { welcomeEmbed: { title: 'T' } });
  assert.equal('welcomeMessage' in explicit, false);
  const writing = materialiseWelcome(settings({ welcomeMessage: null }), { welcomeMessage: 'x' });
  assert.equal(writing.welcomeMessage, 'x');
});

// ── 12-16. Autorole helpers ──────────────────────────────────────────────────

test('parseDelayInput (R225)', () => {
  const secs = (x) => parseDelayInput(x).seconds;
  assert.equal(secs('0'), 0);
  assert.equal(secs('none'), 0);
  assert.equal(secs('OFF'), 0);
  assert.equal(secs('600'), 600);
  assert.equal(secs('10m'), 600);
  assert.equal(secs('1h30m'), 5400);
  assert.equal(secs('7d'), 604_800);
  assert.equal(secs(' 90s '), 90);
  for (const bad of ['8d', 'abc', '5x', '', '99999999', '-5']) assert.equal(typeof parseDelayInput(bad).error, 'string', bad);
  assert.match(parseDelayInput('8d').error, /7 days/);
});

test('autoroleDueAt anchors on the join time and clamps the delay', () => {
  const now = 1_700_000_000_000;
  assert.equal(autoroleDueAt({ joinedTimestamp: now - 1000 }, { autoroleDelay: 60 }, now), now - 1000 + 60_000);
  assert.equal(autoroleDueAt({ joinedTimestamp: now }, { autoroleDelay: 9_999_999 }, now), now + MAX_AUTOROLE_DELAY_SECONDS * 1000);
  assert.equal(autoroleDueAt({ joinedTimestamp: now }, { autoroleDelay: -5 }, now), now);
  assert.equal(autoroleDueAt({ joinedTimestamp: null }, { autoroleDelay: 10 }, now), now + 10_000);
});

test('isScreeningCompletion (R108-R110)', () => {
  const now = 1_700_000_000_000;
  const f = (oldP, newP, oldKnown, joined) => isScreeningCompletion({
    oldMember: { pending: oldP }, newMember: { pending: newP, joinedTimestamp: joined }, oldKnown, now,
  });
  assert.equal(f(true, false, true, undefined), 'cached');
  assert.equal(f(null, false, false, now - 3_600_000), 'maybe');
  assert.equal(f(null, false, false, now - 8 * 86_400_000), null);
  assert.equal(f(null, false, false, now - SCREENING_WATCH_MS - 1), null);
  assert.equal(f(undefined, true, undefined, undefined), null);
  assert.equal(f(false, false, true, undefined), null);
  assert.equal(f(null, false, false, undefined), null);
  assert.equal(f(true, null, true, undefined), null); // still unknown: not a completion
});

test('isCaptchaGated: only while enabled, the role exists and the member lacks it', () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const s = settings({ captchaEnabled: true, captchaRoleId: '300000000000000001' });
  assert.equal(isCaptchaGated(member, s), false); // role was deleted: never lock anyone out
  guild.roles.cache.set('300000000000000001', role('300000000000000001'));
  assert.equal(isCaptchaGated(member, s), true);
  member.roles.cache.set('300000000000000001', {});
  assert.equal(isCaptchaGated(member, s), false);
  assert.equal(isCaptchaGated(makeMember(guild), { ...s, captchaEnabled: false }), false);
  assert.equal(isCaptchaGated(makeMember(guild), settings()), false);

  // Bots are never gated, whether or not captcha managed to give them the verified role.
  const bot = makeMember(guild);
  bot.user.bot = true;
  assert.equal(bot.roles.cache.has('300000000000000001'), false);
  assert.equal(isCaptchaGated(bot, s), false);
  const noUser = makeMember(guild);
  delete noUser.user;
  assert.equal(isCaptchaGated(noUser, s), true, 'a member object without a user (partial) is gated as before');
  assert.equal(isCaptchaGated(makeMember(guild), s), true, 'a human without the role is still gated');
});

test('resolveAutoroles splits existing and deleted ids in order; resolveAutoroleInput', () => {
  const guild = makeGuild();
  const a = '400000000000000001';
  const b = '400000000000000002';
  const c = '400000000000000003';
  guild.roles.cache.set(a, role(a, { name: 'Member' }));
  guild.roles.cache.set(c, role(c, { name: 'Fan Club' }));
  const s = settings({ autoroleIds: [a, b, c, a] });
  const { roles, missingIds } = resolveAutoroles(guild, s);
  assert.deepEqual(roles.map((r) => r.id), [a, c]);
  assert.deepEqual(missingIds, [b]);

  assert.equal(resolveAutoroleInput(guild, s, b), b); // a deleted role can be removed by id
  assert.equal(resolveAutoroleInput(guild, s, `<@&${a}>`), a);
  assert.equal(resolveAutoroleInput(guild, s, '@member'), a);
  assert.equal(resolveAutoroleInput(guild, s, 'FAN CLUB'), c);
  assert.equal(resolveAutoroleInput(guild, s, '499999999999999999'), null);
  assert.equal(resolveAutoroleInput(guild, s, 'nobody'), null);
  assert.equal(resolveAutoroleInput(guild, s, ''), null);
});

// ── 17/18. Card style, image header ──────────────────────────────────────────

test('parseCardStyle and serializeCardStyle', () => {
  const bad = parseCardStyle({ background: '#GGGGGG', overlayOpacity: 3, backgroundUrl: 'ftp://x', enabled: 'yes' });
  assert.deepEqual(bad, { enabled: null, background: null, backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: 1 });
  assert.deepEqual(parseCardStyle(null), { enabled: null, background: null, backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: null });
  assert.equal(parseCardStyle({ overlayOpacity: 0.456 }).overlayOpacity, 0.46);
  assert.equal(parseCardStyle({ overlayOpacity: -2 }).overlayOpacity, 0);
  assert.equal(parseCardStyle({ overlayOpacity: NaN }).overlayOpacity, null);
  assert.equal(parseCardStyle({ textColor: '#ABCDEF' }).textColor, '#abcdef');
  assert.equal(parseCardStyle({ backgroundUrl: `https://x.test/${'a'.repeat(2100)}` }).backgroundUrl, null);

  const full = { v: 1, enabled: false, background: '#112233', backgroundUrl: 'https://x.test/a.png', textColor: '#ffffff', accentColor: '#5865f2', overlayOpacity: 0.4 };
  assert.deepEqual(serializeCardStyle(parseCardStyle(full)), full);
  assert.deepEqual(serializeCardStyle({}), { v: 1, enabled: true, background: null, backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: null });
});

test('readImageSize (the shared header parser) reads PNG and GIF headers', () => {
  const png = Buffer.alloc(33);
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).copy(png);
  png.writeUInt32BE(13, 8);
  png.write('IHDR', 12, 'latin1');
  png.writeUInt32BE(700, 16);
  png.writeUInt32BE(250, 20);
  assert.deepEqual(readImageSize(png), { type: 'png', width: 700, height: 250 });

  const gif = Buffer.alloc(16);
  gif.write('GIF89a', 0, 'latin1');
  gif.writeUInt16LE(320, 6);
  gif.writeUInt16LE(200, 8);
  assert.deepEqual(readImageSize(gif), { type: 'gif', width: 320, height: 200 });
  assert.equal(readImageSize(Buffer.from('definitely not an image at all, just text')), null);
});

// ── Card rendering (offline: no avatar URL, so nothing is fetched) ───────────

test('generateWelcomeCard renders a 700x250 PNG, with and without a style', async () => {
  const guild = makeGuild({ name: 'S'.repeat(80) });
  const member = makeMember(guild, { displayAvatarURL: () => '', displayName: 'D'.repeat(60) });
  member.user.displayAvatarURL = () => '';
  for (const style of [null, { background: '#112233', textColor: '#ffeedd', accentColor: '#ff0000', overlayOpacity: 0.3, backgroundUrl: null }, { background: 'nonsense', overlayOpacity: 'x' }]) {
    const png = await card.generateWelcomeCard(member, guild, style);
    assert.ok(Buffer.isBuffer(png));
    assert.deepEqual(readImageSize(png), { type: 'png', width: card.CARD_WIDTH, height: card.CARD_HEIGHT });
  }
});

test('validateCardBackground refuses non-URLs without touching the network', async () => {
  for (const bad of ['', 'not a url', 'ftp://x.test/a.png', 'javascript:alert(1)', `https://x.test/${'a'.repeat(2100)}`, null, 42]) {
    const r = await card.validateCardBackground(bad);
    assert.equal(r.ok, false);
    assert.match(r.reason, /valid http\(s\) URL/);
  }
  const blocked = await card.validateCardBackground('http://127.0.0.1/a.png');
  assert.equal(blocked.ok, false);
  assert.match(blocked.reason, /cannot be reached/);
  assert.equal(card.backgroundCacheSize() <= 20, true);
});

// ── Senders against fakes ────────────────────────────────────────────────────

test('sendWelcomeChannel posts the exact message and pings only the new member', async () => {
  const guild = makeGuild();
  const channel = makeChannel(guild);
  const member = makeMember(guild);
  await sendWelcomeChannel(member, settings({ welcomeChannelId: channel.id }), noCard);
  assert.equal(channel.sent.length, 1);
  assert.equal(channel.sent[0].content, 'Hey <@111111111111111111>, welcome to **Test $& Server**!');
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [], users: [UID] });
});

test('sendWelcomeChannel respects the toggles, a missing channel and missing permissions', async () => {
  const guild = makeGuild();
  const channel = makeChannel(guild);
  const member = makeMember(guild);
  const base = { welcomeChannelId: channel.id };
  await sendWelcomeChannel(member, settings({ ...base, welcomeEnabled: false }), noCard);
  await sendWelcomeChannel(member, settings({ ...base, welcomeMessageEnabled: false }), noCard);
  await sendWelcomeChannel(member, settings({ welcomeChannelId: null }), noCard);
  await sendWelcomeChannel(member, settings({ welcomeChannelId: '299999999999999999' }), noCard); // deleted
  const mute = makeChannel(guild, { id: '200000000000000002', missing: ['SendMessages'] });
  await sendWelcomeChannel(member, settings({ welcomeChannelId: mute.id }), noCard);
  assert.equal(channel.sent.length, 0);
  assert.equal(mute.sent.length, 0);
});

test('sendWelcomeChannel: no EmbedLinks means text, no AttachFiles means no card (R229)', async () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const noEmbeds = makeChannel(guild, { id: '200000000000000003', missing: ['EmbedLinks'] });
  await sendWelcomeChannel(member, settings({ welcomeChannelId: noEmbeds.id }), noCard);
  assert.equal(noEmbeds.sent[0].embeds.length, 0);
  assert.equal(noEmbeds.sent[0].content, 'Hey <@111111111111111111>, welcome to **Test $& Server**!');

  const noFiles = makeChannel(guild, { id: '200000000000000004', missing: ['AttachFiles'] });
  let cardCalls = 0;
  await sendWelcomeChannel(member, settings({ welcomeChannelId: noFiles.id }), async () => { cardCalls += 1; return Buffer.from([1]); });
  assert.equal(cardCalls, 0);
  assert.equal(noFiles.sent[0].files, undefined);
  assert.equal(noFiles.sent[0].embeds.length, 1);
});

test('sendWelcomeChannel: a 50013 with the card is retried once without it; a 50035 once with the default text', async () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const ch1 = makeChannel(guild, { id: '200000000000000005', sendImpl: (p, n) => { if (n === 1) throw apiError(50013); return {}; } });
  await sendWelcomeChannel(member, settings({ welcomeChannelId: ch1.id }), async () => Buffer.from([1]));
  assert.equal(ch1.sent.length, 2);
  assert.equal(ch1.sent[0].files.length, 1);
  assert.equal(ch1.sent[1].files, undefined);

  const ch2 = makeChannel(guild, { id: '200000000000000006', sendImpl: (p, n) => { if (n === 1) throw apiError(50035); return {}; } });
  await sendWelcomeChannel(member, settings({ welcomeChannelId: ch2.id, welcomeEmbed: { title: 'T' } }), noCard);
  assert.equal(ch2.sent.length, 2);
  assert.equal(ch2.sent[1].content, 'Hey <@111111111111111111>, welcome to **Test $& Server**!');
  assert.equal(ch2.sent[1].embeds, undefined);

  // Any other failure is swallowed (logged): the function never throws.
  const ch3 = makeChannel(guild, { id: '200000000000000007', sendImpl: () => { throw apiError(500, 'boom'); } });
  await sendWelcomeChannel(member, settings({ welcomeChannelId: ch3.id }), noCard);
  assert.equal(ch3.sent.length, 1);
});

test('sendWelcomeDm: closed DMs are silent, bots and the toggle are respected, the card is optional', async () => {
  const guild = makeGuild();
  const on = settings({ welcomeDmEnabled: true });
  const member = makeMember(guild);
  await sendWelcomeDm(member, on, noCard);
  assert.equal(member.sent.length, 1);
  assert.equal(member.sent[0].components[0].toJSON().components[0].custom_id, 'noop:welcome-dm');

  const bot = makeMember(guild);
  bot.user.bot = true;
  await sendWelcomeDm(bot, on, noCard);
  assert.equal(bot.sent.length, 0);

  const off = makeMember(guild);
  await sendWelcomeDm(off, settings({ welcomeDmEnabled: true, welcomeEnabled: false }), noCard);
  await sendWelcomeDm(off, settings(), noCard);
  assert.equal(off.sent.length, 0);

  const closed = makeMember(guild, { send: async () => { throw apiError(50007, 'Cannot send messages to this user'); } });
  await sendWelcomeDm(closed, on, noCard); // resolves without throwing

  const withCard = makeMember(guild);
  await sendWelcomeDm(withCard, settings({ welcomeDmEnabled: true, welcomeDmCard: true }), async () => Buffer.from([1]));
  assert.equal(withCard.sent[0].files.length, 1);

  const retry = makeMember(guild);
  let calls = 0;
  retry.send = async function send(p) { calls += 1; if (calls === 1) throw apiError(50035); this.sent.push(p); };
  await sendWelcomeDm(retry, settings({ welcomeDmEnabled: true, welcomeDmEmbed: { title: 'T' } }), noCard);
  assert.equal(retry.sent.length, 1);
  assert.equal(retry.sent[0].embeds.length, 0);
  assert.equal(retry.sent[0].components.length, 1);
});

test('sendGoodbye posts for a cached or partial member and never pings', async () => {
  const guild = makeGuild();
  const channel = makeChannel(guild);
  const s = settings({ goodbyeEnabled: true, goodbyeChannelId: channel.id });
  const member = makeMember(guild);
  await sendGoodbye(member, member.user, s);
  assert.equal(channel.sent[0].content, '**newbie** just left the server');
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [] });

  const partial = { partial: true, user: member.user, guild };
  await sendGoodbye(partial, partial.user, s);
  assert.equal(channel.sent.length, 2);
  await sendGoodbye(null, member.user, s, guild);
  assert.equal(channel.sent.length, 3);

  await sendGoodbye(member, member.user, { ...s, goodbyeEnabled: false });
  await sendGoodbye(member, member.user, { ...s, welcomeEnabled: false });
  await sendGoodbye(member, member.user, { ...s, goodbyeChannelId: null });
  assert.equal(channel.sent.length, 3);
});

// ── Autoroles against fakes ──────────────────────────────────────────────────

test('grantAutoroles: one role at a time, idempotent, skips what cannot be given', async () => {
  const guild = makeGuild();
  const member = makeMember(guild);
  const ok = role('500000000000000001', { position: 2 });
  const held = role('500000000000000002', { position: 2 });
  const high = role('500000000000000003', { position: 10 });
  const admin = role('500000000000000004', { position: 2, permissions: ['Administrator'] });
  const managed = role('500000000000000005', { position: 2, managed: true });
  member.roles.cache.set(held.id, held);
  const out = await grantAutoroles(member, [ok, held, high, admin, managed]);
  assert.deepEqual(out.added, [ok.id]);
  assert.deepEqual(member.added, [{ id: ok.id, reason: 'Welcome autorole' }]); // a single id, never the array form
  assert.deepEqual(out.failed.map((f) => f.roleId), [high.id, admin.id, managed.id]);
  assert.match(out.failed[1].reason, /dangerous/);
});

test('grantAutoroles error handling: member left stops, role problems continue, others rethrow on request', async () => {
  const guild = makeGuild();
  const r1 = role('500000000000000011');
  const r2 = role('500000000000000012');
  const r3 = role('500000000000000013');
  const gone = makeMember(guild);
  gone.roles.add = async () => { throw apiError(10007); };
  const a = await grantAutoroles(gone, [r1, r2]);
  assert.deepEqual(a, { added: [], failed: [] });

  const mixed = makeMember(guild);
  const seen = [];
  mixed.roles.add = async (id) => { seen.push(id); if (id === r1.id) throw apiError(50013); };
  const b = await grantAutoroles(mixed, [r1, r2]);
  assert.deepEqual(seen, [r1.id, r2.id]);
  assert.deepEqual(b.added, [r2.id]);
  assert.equal(b.failed[0].roleId, r1.id);

  const flaky = makeMember(guild);
  flaky.roles.add = async () => { throw apiError(500, 'server error'); };
  await assert.rejects(() => grantAutoroles(flaky, [r3], { rethrowTransient: true }), /server error/);
  const soft = await grantAutoroles(flaky, [r3]);
  assert.equal(soft.failed.length, 1);
});

function hookCtx(guild, member, s, extra = {}) {
  return { client: { user: { id: 'BOT' } }, guild, guildId: guild.id, member, user: member.user, state: {}, getSettings: async () => s, ...extra };
}

test('onMemberJoin: autoroles are granted at once, whatever the module flag says and whatever the message does', async () => {
  const guild = makeGuild();
  const r = role('500000000000000021', { position: 2 });
  guild.roles.cache.set(r.id, r);
  const channel = makeChannel(guild, { sendImpl: () => { throw apiError(500, 'boom'); } });
  const member = makeMember(guild);
  const style = serializeCardStyle({ enabled: false });
  await onMemberJoin(hookCtx(guild, member, settings({ autoroleIds: [r.id], welcomeChannelId: channel.id, welcomeCardStyle: style })));
  assert.deepEqual(member.added.map((a) => a.id), [r.id]);
  assert.equal(channel.sent.length, 1); // the failing send did not undo or block the role

  const off = makeMember(guild);
  await onMemberJoin(hookCtx(guild, off, settings({ welcomeEnabled: false, autoroleIds: [r.id], welcomeChannelId: channel.id })));
  assert.deepEqual(off.added.map((a) => a.id), [r.id]);
  assert.equal(channel.sent.length, 1); // module off: no message

  const none = makeMember(guild);
  await onMemberJoin(hookCtx(guild, none, null ? null : settings({ welcomeEnabled: false })));
  assert.equal(none.added.length, 0);
  await onMemberJoin({ ...hookCtx(guild, none, null), getSettings: async () => null }); // settings unavailable: does nothing
});

test('onMemberJoin: a captcha-pending join gets no roles yet (captcha calls assignAutorole later)', async () => {
  const guild = makeGuild();
  const r = role('500000000000000022', { position: 2 });
  guild.roles.cache.set(r.id, r);
  const member = makeMember(guild);
  await onMemberJoin(hookCtx(guild, member, settings({ welcomeEnabled: false, autoroleIds: [r.id] }), { state: { captchaPending: true } }));
  assert.equal(member.added.length, 0);
  const gated = makeMember(guild);
  guild.roles.cache.set('300000000000000009', role('300000000000000009'));
  await onMemberJoin(hookCtx(guild, gated, settings({ welcomeEnabled: false, autoroleIds: [r.id], captchaEnabled: true, captchaRoleId: '300000000000000009' })));
  assert.equal(gated.added.length, 0);
});

test('onMemberJoin: a bot is never held back by the captcha gate, even without the verified role', async () => {
  const guild = makeGuild();
  const r = role('500000000000000023', { position: 2 });
  guild.roles.cache.set(r.id, r);
  guild.roles.cache.set('300000000000000009', role('300000000000000009'));
  const captchaOn = settings({ welcomeEnabled: false, autoroleIds: [r.id], captchaEnabled: true, captchaRoleId: '300000000000000009' });

  // Captcha could not give the bot the verified role (hierarchy / permissions); its autoroles still arrive.
  const bot = makeMember(guild);
  bot.user.bot = true;
  await onMemberJoin(hookCtx(guild, bot, captchaOn));
  assert.deepEqual(bot.added.map((a) => a.id), [r.id]);
  assert.equal(bot.roles.cache.has('300000000000000009'), false);

  // A human in the same situation waits for verification.
  const human = makeMember(guild);
  await onMemberJoin(hookCtx(guild, human, captchaOn));
  assert.equal(human.added.length, 0);
});

test('onMemberJoin sends the channel message and the DM sharing one card render', async () => {
  const guild = makeGuild();
  const channel = makeChannel(guild);
  const member = makeMember(guild, { displayAvatarURL: () => '' }); // no avatar URL: the card is drawn without a download
  const s = settings({
    welcomeChannelId: channel.id, welcomeDmEnabled: true, welcomeDmCard: true, welcomeCardStyle: serializeCardStyle({ enabled: false }),
  });
  await onMemberJoin(hookCtx(guild, member, s));
  assert.equal(channel.sent.length, 1);
  assert.equal(member.sent.length, 1);
  assert.equal(member.sent[0].files.length, 1); // the DM card (the channel card is switched off by the style)
});

test('onMemberLeave posts the goodbye only when both flags are on', async () => {
  const guild = makeGuild();
  const channel = makeChannel(guild);
  const member = makeMember(guild);
  await onMemberLeave(hookCtx(guild, member, settings({ goodbyeEnabled: true, goodbyeChannelId: channel.id })));
  assert.equal(channel.sent.length, 1);
  await onMemberLeave(hookCtx(guild, member, settings({ goodbyeEnabled: false, goodbyeChannelId: channel.id })));
  await onMemberLeave(hookCtx(guild, member, settings({ welcomeEnabled: false, goodbyeEnabled: true, goodbyeChannelId: channel.id })));
  assert.equal(channel.sent.length, 1);
});

// ── Setup modal ──────────────────────────────────────────────────────────────

test('buildSetupModal: five inputs, prefilled from what is sent, never rejected by Discord', () => {
  const m = buildSetupModal('welcome', settings()).toJSON();
  assert.equal(m.custom_id, 'welcome-setup');
  const inputs = m.components.map((row) => row.components[0]);
  assert.deepEqual(inputs.map((i) => i.custom_id), ['tpl-content', 'tpl-title', 'tpl-description', 'tpl-color', 'tpl-footer']);
  assert.equal(inputs[0].value, DEFAULT_WELCOME_MESSAGE);
  assert.equal(inputs[1].value, DEFAULT_WELCOME_EMBED.title);
  assert.equal(inputs[3].value, '#57F287');

  assert.equal(buildSetupModal('dm', settings()).toJSON().custom_id, 'welcome-dm-setup');
  assert.equal(buildSetupModal('goodbye', settings()).toJSON().custom_id, 'goodbye-setup');
  const dm = buildSetupModal('dm', settings()).toJSON().components.map((r) => r.components[0]);
  assert.equal(dm[0].value, DEFAULT_WELCOME_DM_MESSAGE);
  assert.equal(dm[1].value, undefined); // no default DM embed

  // An over-long or invalid stored value must not make Discord reject the form (R015).
  const nasty = settings({
    welcomeMessage: 'm'.repeat(2500),
    welcomeEmbed: { title: 't'.repeat(400), description: 'd'.repeat(5000), footer: { text: 'f'.repeat(3000) }, color: 0xFFFFFFF },
  });
  const nastyInputs = buildSetupModal('welcome', nasty).toJSON().components.map((r) => r.components[0]);
  assert.ok(nastyInputs[0].value.length <= 2000);
  assert.ok(nastyInputs[1].value.length <= 256);
  assert.ok(nastyInputs[2].value.length <= 4000);
  assert.ok(nastyInputs[4].value.length <= 2048);
  assert.ok(!nastyInputs[3].value || /^#[0-9A-F]{6}$/.test(nastyInputs[3].value));

  // A stored black colour is shown, not reset (R072).
  const black = buildSetupModal('welcome', settings({ welcomeEmbed: { title: 'T', color: 0 } })).toJSON().components.map((r) => r.components[0]);
  assert.equal(black[3].value, '#000000');
});

test('isStaleSetupForm rejects a form that carries the retired field ids', () => {
  const form = (ids) => ({ fields: { fields: new Map(ids.map((id) => [id, { value: '' }])) } });
  assert.equal(isStaleSetupForm(form(['welcome-title', 'welcome-description', 'welcome-color', 'welcome-thumbnail', 'welcome-footer'])), true);
  assert.equal(isStaleSetupForm(form(['tpl-content', 'tpl-title', 'tpl-description', 'tpl-color', 'tpl-footer'])), false);
  assert.equal(isStaleSetupForm(form(['tpl-content', 'tpl-title'])), true);
  assert.equal(isStaleSetupForm({}), true);
});

// ── Diagnostics and Json writes ──────────────────────────────────────────────

test('diagnoseWelcome reports what stops the feature, in short lines', () => {
  const guild = makeGuild();
  const off = diagnoseWelcome(guild, settings({ welcomeEnabled: false, welcomeChannelId: null }), 'welcome');
  assert.ok(off.some((l) => /module is off/.test(l)));
  assert.ok(off.some((l) => /No welcome channel/.test(l)));
  assert.equal(off.some((l) => l.startsWith('💡')), false, 'the default text pings the member, so there is no tip');
  const tip = diagnoseWelcome(guild, settings({ welcomeMessage: 'Welcome!' }), 'welcome');
  assert.ok(tip.some((l) => l.startsWith('💡')), 'a custom text without {user} gets a tip');
  assert.ok([...off, ...tip].every((l) => l.startsWith('⚠️') || l.startsWith('💡')));

  const channel = makeChannel(guild, { id: '200000000000000010', missing: ['EmbedLinks'] });
  const perms = diagnoseWelcome(guild, settings({ welcomeChannelId: channel.id }), 'welcome');
  assert.ok(perms.some((l) => /Embed Links/.test(l)));
  assert.equal(perms.some((l) => /module is off/.test(l)), false);

  const missing = diagnoseWelcome(guild, settings({ welcomeChannelId: '299999999999999990' }), 'welcome');
  assert.ok(missing.some((l) => /was not found/.test(l)));

  assert.ok(diagnoseWelcome(guild, settings(), 'dm').some((l) => /welcome DM is off/.test(l)));
  assert.ok(diagnoseWelcome(guild, settings({ welcomeDmEnabled: true, welcomeDmMessage: '' }), 'dm').some((l) => /Nothing would be sent/.test(l)));
  assert.ok(diagnoseWelcome(guild, settings(), 'goodbye').some((l) => /goodbye message is off/.test(l)));

  const high = role('600000000000000001', { position: 10 });
  guild.roles.cache.set(high.id, high);
  const auto = diagnoseWelcome(guild, settings({ welcomeEnabled: false, autoroleIds: [high.id, '699999999999999999'] }), 'autoroles');
  assert.ok(auto.some((l) => /no longer exist/.test(l)));
  assert.ok(auto.some((l) => l.includes(`<@&${high.id}>`)));
  assert.ok(auto.some((l) => /run even while the module is off/.test(l)));
});

test('toDbData maps null Json values to Prisma.DbNull (a plain null would store the JSON literal null)', () => {
  const out = toDbData({ welcomeEmbed: null, welcomeCardStyle: { v: 1 }, goodbyeEmbed: null, welcomeDmEmbed: null, welcomeMessage: null, autoroleDelay: 0 });
  assert.equal(out.welcomeEmbed, Prisma.DbNull);
  assert.equal(out.goodbyeEmbed, Prisma.DbNull);
  assert.equal(out.welcomeDmEmbed, Prisma.DbNull);
  assert.deepEqual(out.welcomeCardStyle, { v: 1 });
  assert.equal(out.welcomeMessage, null); // a String? column takes a plain null
  assert.equal(out.autoroleDelay, 0);
  assert.equal(toDbData(null), null);
});

test('WelcomeConfigError carries an optional title', () => {
  const e = new WelcomeConfigError('x', 'Title');
  assert.equal(e.title, 'Title');
  assert.equal(e.message, 'x');
  assert.equal(new WelcomeConfigError('y').title, undefined);
});

// ── 19. Command payloads ─────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

function checkTree(path, options, problems) {
  if (options.length > 25) problems.push(`${path}: ${options.length} options`);
  for (const o of options) {
    if (!o.description || o.description.length > 100) problems.push(`${path} ${o.name}: description length`);
    if (o.choices?.length > 25) problems.push(`${path} ${o.name}: choices`);
    checkTree(`${path} ${o.name}`, o.options ?? [], problems);
  }
}

test('/welcome and /goodbye payloads stay inside Discord limits and export the command contract', () => {
  const problems = [];
  const w = buildCommandPayload(welcomeCmd);
  const g = buildCommandPayload(goodbyeCmd);
  assert.ok(charCount(w) <= 2000, `/welcome is ${charCount(w)} characters`);
  assert.ok(charCount(g) <= 800, `/goodbye is ${charCount(g)} characters`);
  checkTree('/welcome', w.options, problems);
  checkTree('/goodbye', g.options, problems);
  assert.deepEqual(problems, []);

  for (const mod of [welcomeCmd, goodbyeCmd]) {
    assert.equal(mod.module, 'welcome');
    assert.equal(mod.gated, false);
    assert.equal(mod.permissionLevel, PERMISSION_LEVELS.ADMIN);
    assert.equal(mod.allowInDMs, undefined);
    assert.equal(mod.subcommandPermissions, undefined);
    assert.equal(typeof mod.execute, 'function');
    assert.equal(mod.data.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  }
  assert.equal(typeof welcomeCmd.autocomplete, 'function');
  assert.equal(goodbyeCmd.autocomplete, undefined);
  assert.deepEqual(w.options.map((o) => o.name), ['enable', 'disable', 'status', 'channel', 'message', 'embed', 'setup', 'card', 'test', 'dm', 'autorole']);
  assert.deepEqual(g.options.map((o) => o.name), ['enable', 'disable', 'channel', 'message', 'embed', 'setup', 'test']);
  assert.equal(welcomeCmd.registerHandlers, undefined);
});

test('/welcome autorole remove autocomplete lists configured roles, deleted ones included', async () => {
  const guild = makeGuild();
  const a = '700000000000000001';
  guild.roles.cache.set(a, role(a, { name: 'Members' }));
  let answered = null;
  const interaction = {
    guildId: 'G', guild,
    options: { getFocused: () => ({ name: 'role', value: '' }), getSubcommand: () => 'remove' },
    responded: false,
    respond: async (choices) => { answered = choices; },
  };
  // No settings are loaded offline (Postgres is unreachable), so the answer is empty rather than an error.
  await welcomeCmd.autocomplete(interaction);
  assert.deepEqual(answered, []);
});

// ── 20. Plugin shape ─────────────────────────────────────────────────────────

test('the welcome plugin declares its hooks, modals and job, all ungated', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => /welcome/.test(e)), []);
  const p = plugins.find((x) => x.id === 'welcome');
  assert.ok(p, 'plugin welcome collected');
  assert.equal(p.module, 'welcome');
  assert.deepEqual(p.hooks.map((h) => `${h.event}:${h.id}`).sort(), [
    'guildMemberAdd:welcome', 'guildMemberRemove:welcome', 'guildMemberUpdate:welcome', 'roleDelete:welcome',
  ]);
  assert.ok(p.hooks.every((h) => h.module === null));
  assert.deepEqual(p.components.map((c) => `${c.type}:${c.prefix}`).sort(), ['modal:goodbye-setup', 'modal:welcome-dm-setup', 'modal:welcome-setup']);
  assert.ok(p.components.every((c) => c.module === null));
  assert.deepEqual(p.jobs.map((j) => j.type), ['welcome.autorole']);
});

test('plugin hook filters: the bot leaving, screening completions, unrelated role deletions', async () => {
  const { plugins } = await collectPlugins();
  const hooks = Object.fromEntries(plugins.find((x) => x.id === 'welcome').hooks.map((h) => [h.event, h]));
  const client = { user: { id: 'BOT' } };
  assert.equal(hooks.guildMemberRemove.filter({ client, user: { id: 'BOT' } }), false);
  assert.equal(hooks.guildMemberRemove.filter({ client, user: { id: UID } }), true);
  const now = Date.now();
  assert.equal(hooks.guildMemberUpdate.filter({ guildId: 'NOPE', oldKnown: true, oldMember: { pending: false }, newMember: { pending: false } }), false);
  assert.equal(hooks.guildMemberUpdate.filter({ guildId: 'NOPE', oldKnown: true, oldMember: { pending: true }, newMember: { pending: false } }), true);
  assert.equal(hooks.guildMemberUpdate.filter({ guildId: 'NOPE', oldKnown: false, oldMember: { pending: null }, newMember: { pending: false, joinedTimestamp: now - 1000 } }), true);
  // No settings in the memo: the role deletion is processed (the handler re-checks with real settings).
  assert.equal(hooks.roleDelete.filter({ guildId: 'NOPE', role: { id: '1' } }), true);
});

// ── 21. No legacy ────────────────────────────────────────────────────────────

test('static scan: no invite tracking, timers, direct DB client or cache deletes in the service', () => {
  const read = (f) => readFileSync(resolve(ROOT, f), 'utf8');
  const service = read('src/services/welcomeService.js');
  for (const re of [/inviteTracker/, /guildMemberCache/i, /setTimeout\(/, /from ['"]\.\.\/database\/client\.js['"]/, /cache\.del\(/]) {
    assert.doesNotMatch(service, re, String(re));
  }
  // The setup forms allow an emptied form through the option, not by matching the validator's wording.
  assert.doesNotMatch(service, /message is empty/i);
  assert.doesNotMatch(service, /EMPTY_MESSAGE_ERROR/);
  assert.match(service, /parseTemplateModal\(interaction, setupBase\(kind, settings\), \{ requireSomething: false \}\)/);
  assert.match(read('src/commands/admin/welcome.js'), /utils\/logger\.js/);
  assert.match(read('src/commands/admin/goodbye.js'), /utils\/logger\.js/);
  for (const f of ['src/services/welcomeService.js', 'src/commands/admin/welcome.js', 'src/commands/admin/goodbye.js', 'src/utils/welcomeCard.js', 'src/plugins/welcome.js']) {
    assert.doesNotMatch(read(f), /\bconsole\./, `${f} uses console`);
    assert.doesNotMatch(read(f), /LIMITS\.[A-Z_]+\s*\?\?/, `${f} has a LIMITS fallback`);
  }
  for (const f of ['src/commands/admin/welcome.js', 'src/commands/admin/goodbye.js']) {
    assert.doesNotMatch(read(f), /modalHandlers|registerHandlers|database\/client|cache\.del/, f);
  }
  const cardSrc = read('src/utils/welcomeCard.js');
  const fn = cardSrc.slice(cardSrc.indexOf('function ensureFont'), cardSrc.indexOf('export const CARD_WIDTH'));
  assert.equal(cardSrc.match(/registerFromPath/g).length, 1);
  assert.ok(fn.includes('registerFromPath'), 'the font is registered only inside ensureFont');
});

// ═════════════════════════════════════════════════════════════════════════════
// DB cases (TEST_DATABASE_URL)
// ═════════════════════════════════════════════════════════════════════════════

const DBG = '955008000000000001'; // unique to this package
const DBU = '955008000000000002';
const DBR1 = '955008000000000011';
const DBR2 = '955008000000000012';

// Direct writes bypass settingsService, so its in-process memo is dropped after each one.
async function setDb(data) {
  const { invalidateSettings } = await import('../src/services/settingsService.js');
  await prisma.guildSettings.update({ where: { guildId: DBG }, data });
  await invalidateSettings(DBG);
}

async function resetGuild() {
  const { invalidateSettings } = await import('../src/services/settingsService.js');
  await prisma.scheduledJob.deleteMany({ where: { guildId: DBG } });
  await prisma.guildSettings.deleteMany({ where: { guildId: DBG } });
  await prisma.guildSettings.create({ data: { guildId: DBG } });
  await invalidateSettings(DBG);
}

async function dbGuild() {
  const guild = makeGuild({ id: DBG });
  guild.roles.cache.set(DBR1, role(DBR1, { position: 2, name: 'One' }));
  guild.roles.cache.set(DBR2, role(DBR2, { position: 2, name: 'Two' }));
  return guild;
}

test('DB: embed writes use DbNull, keep a legacy card:false and freeze the text', { skip: skipDb }, async () => {
  const { getSettings } = await import('../src/services/settingsService.js');
  await resetGuild();
  // A legacy row: custom embed, card switched off, no text of its own.
  await setDb({ welcomeEmbed: { title: 'Old', description: 'Legacy', color: 0x5865F2, thumbnail: 'user_avatar', card: false } });
  const a = await svc.updateEmbedOptions(DBG, 'welcome', { thumbnail: 'server' });
  assert.equal(a.welcomeEmbed.thumbnail, '{server.icon}');
  assert.equal(a.welcomeEmbed.title, 'Old');
  assert.equal('card' in a.welcomeEmbed, false);
  assert.equal(welcomeCardEnabled(a), false);
  assert.equal(a.welcomeCardStyle.enabled, false);
  assert.equal(a.welcomeMessage, ''); // the legacy "no text line" is now explicit, so a thumbnail edit changes nothing else

  // An embed reset writes a real database NULL (Prisma.DbNull) and keeps the card off.
  const b = await svc.updateEmbedOptions(DBG, 'welcome', { reset: true });
  assert.equal(b.welcomeEmbed, null);
  assert.equal(welcomeCardEnabled(b), false);
  const fresh = await getSettings(DBG, { fresh: true });
  assert.equal(fresh.welcomeEmbed, null);

  // Goodbye embed edit then reset.
  const c = await svc.updateEmbedOptions(DBG, 'goodbye', { enabled: true, thumbnail: 'none' });
  assert.equal(c.goodbyeUseEmbed, true);
  assert.equal(c.goodbyeEmbed.thumbnail, undefined);
  assert.equal(c.goodbyeEmbed.description, DEFAULT_GOODBYE_MESSAGE);
  const d = await svc.updateEmbedOptions(DBG, 'goodbye', { reset: true });
  assert.equal(d.goodbyeEmbed, null);

  await assert.rejects(() => svc.updateEmbedOptions(DBG, 'welcome', {}), /at least one option/);
  await assert.rejects(() => svc.updateEmbedOptions(DBG, 'welcome', { reset: true, thumbnail: 'none' }), /reset on its own/);
  await assert.rejects(() => svc.updateEmbedOptions(DBG, 'welcome', { imageUrl: 'javascript:alert(1)' }), /http\(s\) URL/);
  const img = await svc.updateEmbedOptions(DBG, 'welcome', { imageUrl: 'https://example.test/a.png' });
  assert.equal(img.welcomeEmbed.image, 'https://example.test/a.png');
  const noImg = await svc.updateEmbedOptions(DBG, 'welcome', { imageUrl: 'NONE' });
  assert.equal(noImg.welcomeEmbed.image, undefined);
  await resetGuild();
});

test('DB: message and card style updates', { skip: skipDb }, async () => {
  await resetGuild();
  const a = await svc.updateMessage(DBG, 'welcome', { text: 'Hi {user}!', enabled: false });
  assert.equal(a.welcomeMessage, 'Hi {user}!');
  assert.equal(a.welcomeMessageEnabled, false);
  const b = await svc.updateMessage(DBG, 'welcome', { reset: true });
  assert.equal(b.welcomeMessage, null);
  assert.equal(await svc.updateMessage(DBG, 'dm', {}), null);
  const c = await svc.updateMessage(DBG, 'dm', { text: 'psst' });
  assert.equal(c.welcomeDmMessage, 'psst');
  await assert.rejects(() => svc.updateMessage(DBG, 'goodbye', { text: 'x'.repeat(2500) }), WelcomeConfigError);
  await assert.rejects(() => svc.updateMessage(DBG, 'goodbye', { text: 'x', reset: true }), /either text or reset/);

  const s1 = await svc.updateCardStyle(DBG, { background: '#ABCDEF', overlay: 0.456, enabled: false });
  assert.deepEqual(s1.welcomeCardStyle, {
    v: 1, enabled: false, background: '#abcdef', backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: 0.46,
  });
  await assert.rejects(() => svc.updateCardStyle(DBG, { textColor: 'not-a-colour' }), /Text colour is not a colour/);
  const s2 = await svc.updateCardStyle(DBG, { accentColor: 'blurple' });
  assert.equal(s2.welcomeCardStyle.accentColor, '#5865f2');
  assert.equal(s2.welcomeCardStyle.enabled, false); // untouched options keep their value
  const s3 = await svc.updateCardStyle(DBG, { reset: true });
  assert.equal(s3.welcomeCardStyle.background, null);
  assert.equal(s3.welcomeCardStyle.enabled, false);
  await resetGuild();
});

test('DB: autorole add keeps the stored delay (R074), enforces the cap and removes deleted roles (R230)', { skip: skipDb }, async () => {
  await resetGuild();
  const guild = await dbGuild();
  const executor = makeMember(guild, { id: 'EX' });
  executor.roles.highest = { position: 9 };
  executor.roles.cache = new Map();
  const one = guild.roles.cache.get(DBR1);
  const two = guild.roles.cache.get(DBR2);

  const a = await svc.addAutorole({ guild, executor: { ...executor, id: 'O' }, role: one, delay: '10m' });
  assert.deepEqual(a.settings.autoroleIds, [DBR1]);
  assert.equal(a.settings.autoroleDelay, 600);
  const b = await svc.addAutorole({ guild, executor: { ...executor, id: 'O' }, role: two });
  assert.deepEqual(b.settings.autoroleIds, [DBR1, DBR2]);
  assert.equal(b.settings.autoroleDelay, 600, 'adding without a delay must not reset it');

  await assert.rejects(() => svc.addAutorole({ guild, executor: { ...executor, id: 'O' }, role: two }), /already an autorole/);
  const upd = await svc.addAutorole({ guild, executor: { ...executor, id: 'O' }, role: two, delay: '0' });
  assert.equal(upd.already, true);
  assert.equal(upd.settings.autoroleDelay, 0);
  await assert.rejects(() => svc.addAutorole({ guild, executor: { ...executor, id: 'O' }, role: two, delay: '9d' }), /7 days/);
  const dangerous = role('955008000000000013', { position: 2, permissions: ['BanMembers'] });
  guild.roles.cache.set(dangerous.id, dangerous);
  await assert.rejects(() => svc.addAutorole({ guild, executor: { ...executor, id: 'O' }, role: dangerous }), /dangerous/);

  const d = await svc.setAutoroleDelay(DBG, '2h');
  assert.equal(d.seconds, 7200);
  assert.equal(d.settings.autoroleDelay, 7200);
  await assert.rejects(() => svc.setAutoroleDelay(DBG, '30d'), /7 days/);

  // The cap: LIMITS.AUTOROLES roles at most.
  const { LIMITS } = await import('../src/config/constants.js');
  const many = Array.from({ length: LIMITS.AUTOROLES }, (_, i) => `9550080000001${String(i).padStart(5, '0')}`);
  await setDb({ autoroleIds: many });
  await assert.rejects(() => svc.addAutorole({ guild, executor: { ...executor, id: 'O' }, role: one }), /up to 10 autoroles/);

  // A deleted role can be removed by id, and list-pruning drops ids that no longer exist.
  await setDb({ autoroleIds: [DBR1, '955008000000000099'] });
  const { getSettings } = await import('../src/services/settingsService.js');
  const cur = await getSettings(DBG, { fresh: true });
  const gone = await svc.removeAutorole(guild, cur, '955008000000000099');
  assert.deepEqual(gone.settings.autoroleIds, [DBR1]);
  const none = await svc.removeAutorole(guild, gone.settings, 'nobody');
  assert.equal(none.id, null);
  await setDb({ autoroleIds: [DBR1, '955008000000000098'] });
  const cur2 = await getSettings(DBG, { fresh: true });
  const { missingIds } = resolveAutoroles(guild, cur2);
  assert.equal(await svc.pruneMissingAutoroles(guild, missingIds), 1);
  assert.deepEqual((await getSettings(DBG, { fresh: true })).autoroleIds, [DBR1]);
  await resetGuild();
});

test('DB: a delayed autorole is a durable job anchored on the join time, replaced by a re-join and cancelled on leave (R186/R187)', { skip: skipDb }, async () => {
  const { getJob } = await import('../src/services/scheduler.js');
  await resetGuild();
  const guild = await dbGuild();
  await setDb({ autoroleIds: [DBR1, DBR2], autoroleDelay: 600 });
  const joined = Date.now() - 60_000;
  const member = makeMember(guild, { id: DBU, joinedTimestamp: joined });
  member.user.id = DBU;
  const key = autoroleJobKey(DBG, DBU);

  await svc.assignAutorole(member);
  assert.equal(member.added.length, 0, 'nothing is granted before the delay');
  const job = await getJob(key);
  assert.equal(job.type, 'welcome.autorole');
  assert.equal(job.guildId, DBG);
  assert.deepEqual(job.payload, { userId: DBU });
  assert.equal(job.runAt.getTime(), joined + 600_000);

  // Leaving cancels it.
  await onMemberLeave(hookCtx(guild, member, (await import('../src/services/settingsService.js')).peekSettings(DBG) ?? settings({ autoroleIds: [DBR1] }), { guildId: DBG }));
  assert.equal(await getJob(key), null);

  // Running the job after the delay grants the roles, one at a time, from the current settings.
  const gm = makeMember(guild, { id: DBU, joinedTimestamp: joined });
  gm.user.id = DBU;
  guild.members.fetch = async () => gm;
  await svc.runAutoroleJob({ payload: { userId: DBU } }, { guild });
  assert.deepEqual(gm.added.map((a) => a.id), [DBR1, DBR2]);
  await svc.runAutoroleJob({ payload: { userId: DBU } }, { guild });
  assert.equal(gm.added.length, 2, 'idempotent');

  // A member who left: the job ends quietly.
  guild.members.fetch = async () => { throw apiError(10007); };
  assert.equal(await svc.runAutoroleJob({ payload: { userId: DBU } }, { guild }), undefined);
  // A transient failure is rethrown so the scheduler retries.
  guild.members.fetch = async () => { throw apiError(500, 'discord is down'); };
  await assert.rejects(() => svc.runAutoroleJob({ payload: { userId: DBU } }, { guild }), /discord is down/);
  // A bad payload is permanent.
  await assert.rejects(() => svc.runAutoroleJob({ payload: { userId: 'abc' } }, { guild }), /invalid payload/);

  // Zero delay: granted at once, and an old job is dropped.
  await setDb({ autoroleDelay: 0 });
  await svc.assignAutorole(makeMember(guild, { id: DBU, joinedTimestamp: Date.now() })); // schedules nothing
  await resetGuild();
});

test('DB: a bot keeps its delayed autoroles when captcha could not give it the verified role; a gated human does not get them', { skip: skipDb }, async () => {
  const { getJob } = await import('../src/services/scheduler.js');
  const { getSettings } = await import('../src/services/settingsService.js');
  const VERIFIED = '955008000000000021';
  await resetGuild();
  const guild = await dbGuild();
  guild.roles.cache.set(VERIFIED, role(VERIFIED, { position: 2, name: 'Verified' }));
  await setDb({ autoroleIds: [DBR1, DBR2], autoroleDelay: 600, captchaEnabled: true, captchaRoleId: VERIFIED });
  const key = autoroleJobKey(DBG, DBU);
  const joined = Date.now() - 60_000;
  const fresh = () => getSettings(DBG, { fresh: true });

  // The bot never received the verified role (captcha's autoVerifyBot failed to add it) and is not Verified.
  const bot = makeMember(guild, { id: DBU, joinedTimestamp: joined });
  bot.user.id = DBU;
  bot.user.bot = true;

  // 1. Captcha's autoVerifyBot hands the bot to welcome with force...
  await svc.assignAutorole(bot, { force: true });
  assert.equal(bot.added.length, 0, 'nothing before the delay');
  const job = await getJob(key);
  assert.deepEqual(job.payload, { userId: DBU });
  assert.equal(job.runAt.getTime(), joined + 600_000);
  // 2. ...and welcome's own join hook (which runs after captcha's) neither drops nor replaces it with a worse one.
  await onMemberJoin(hookCtx(guild, bot, await fresh()));
  const kept = await getJob(key);
  assert.deepEqual(kept.payload, { userId: DBU });
  assert.equal(kept.runAt.getTime(), joined + 600_000);
  // 3. When the job fires the roles are granted although the bot still lacks the verified role.
  guild.members.fetch = async () => bot;
  assert.equal(await svc.runAutoroleJob({ payload: { userId: DBU } }, { guild }), undefined);
  assert.deepEqual(bot.added.map((a) => a.id), [DBR1, DBR2]);
  assert.equal(bot.roles.cache.has(VERIFIED), false);
  await prisma.scheduledJob.deleteMany({ where: { guildId: DBG } });

  // A human who has not verified: no job at join (captcha calls assignAutorole after verification)...
  const human = makeMember(guild, { id: DBU, joinedTimestamp: joined });
  human.user.id = DBU;
  await onMemberJoin(hookCtx(guild, human, await fresh(), { state: { captchaPending: true } }));
  assert.equal(await getJob(key), null);
  assert.equal(human.added.length, 0);
  // ...and a screening-watch job that fires while they are still unverified drops out without granting.
  guild.members.fetch = async () => human;
  assert.equal(await svc.runAutoroleJob({ payload: { userId: DBU, watch: true } }, { guild }), undefined);
  assert.equal(human.added.length, 0);
  // Once verified, captcha's forced assignAutorole schedules the same delayed grant for them.
  human.roles.cache.set(VERIFIED, { id: VERIFIED });
  await svc.assignAutorole(human, { force: true });
  assert.deepEqual((await getJob(key)).payload, { userId: DBU });
  await resetGuild();
});

test('DB: a member still in Membership Screening gets a watch job; force never skips that gate', { skip: skipDb }, async () => {
  const { getJob } = await import('../src/services/scheduler.js');
  const { peekSettings } = await import('../src/services/settingsService.js');
  await resetGuild();
  const guild = await dbGuild();
  await setDb({ autoroleIds: [DBR1], autoroleDelay: 0 });
  const key = autoroleJobKey(DBG, DBU);
  const joined = Date.now() - 1000;

  const pending = makeMember(guild, { id: DBU, pending: true, joinedTimestamp: joined });
  pending.user.id = DBU;
  await svc.assignAutorole(pending, { force: true });
  assert.equal(pending.added.length, 0);
  const watch = await getJob(key);
  assert.deepEqual(watch.payload, { userId: DBU, watch: true });
  assert.ok(watch.runAt.getTime() > Date.now() + 5 * 3_600_000);

  // The watch job, while the member is still pending, reschedules itself.
  const stillPending = makeMember(guild, { id: DBU, pending: true, joinedTimestamp: joined });
  guild.members.fetch = async () => stillPending;
  const again = await svc.runAutoroleJob({ payload: { userId: DBU, watch: true } }, { guild });
  assert.ok(again.nextRunAt instanceof Date);
  assert.equal(stillPending.added.length, 0);

  // Screening completed while the bot was offline: the next poll grants.
  const done = makeMember(guild, { id: DBU, pending: false, joinedTimestamp: joined });
  guild.members.fetch = async () => done;
  assert.equal(await svc.runAutoroleJob({ payload: { userId: DBU, watch: true } }, { guild }), undefined);
  assert.deepEqual(done.added.map((a) => a.id), [DBR1]);

  // ...or, with a delay, turns into a delayed grant.
  await setDb({ autoroleDelay: 3600 });
  const fresh = makeMember(guild, { id: DBU, pending: false, joinedTimestamp: Date.now() });
  guild.members.fetch = async () => fresh;
  const next = await svc.runAutoroleJob({ payload: { userId: DBU, watch: true } }, { guild });
  assert.deepEqual(next.payload, { userId: DBU });
  assert.ok(next.nextRunAt.getTime() > Date.now() + 3_000_000);

  // Screening completes through guildMemberUpdate with a cached old member: granted, watch dropped.
  await setDb({ autoroleDelay: 0 });
  await svc.onMemberUpdate({
    guildId: DBG, oldKnown: true, oldMember: { pending: true }, newMember: makeMember(guild, { id: DBU, pending: false, joinedTimestamp: joined }),
    getSettings: async () => (await import('../src/services/settingsService.js')).getSettings(DBG, { fresh: true }),
  });
  assert.equal(await getJob(key), null);
  void peekSettings;

  // An uncached old member ('maybe') is only acted on when a watch job exists.
  const m2 = makeMember(guild, { id: DBU, pending: false, joinedTimestamp: joined });
  await svc.onMemberUpdate({
    guildId: DBG, oldKnown: false, oldMember: { pending: null }, newMember: m2,
    getSettings: async () => (await import('../src/services/settingsService.js')).getSettings(DBG, { fresh: true }),
  });
  assert.equal(m2.added.length, 0, 'no watch job: a moderator-removed role is not re-added');
  await resetGuild();
});

test('DB: roleDelete prunes the autorole list', { skip: skipDb }, async () => {
  const { getSettings } = await import('../src/services/settingsService.js');
  await resetGuild();
  await setDb({ autoroleIds: [DBR1, DBR2] });
  const ctx = { guildId: DBG, role: { id: DBR1 }, getSettings: async () => getSettings(DBG, { fresh: true }) };
  await svc.onRoleDelete(ctx);
  assert.deepEqual((await getSettings(DBG, { fresh: true })).autoroleIds, [DBR2]);
  await svc.onRoleDelete({ ...ctx, role: { id: '955008000000000077' } }); // not an autorole: nothing happens
  assert.deepEqual((await getSettings(DBG, { fresh: true })).autoroleIds, [DBR2]);
  await resetGuild();
});

function modalInteraction(guild, values, { admin = true } = {}) {
  const replies = [];
  const map = new Map(Object.entries(values).map(([k, v]) => [k, { value: v }]));
  return {
    replies,
    guildId: DBG, guild, channel: { id: '800000000000000001' },
    member: { id: 'O', guild, permissions: new PermissionsBitField(admin ? ['ManageGuild'] : []), roles: { cache: new Map() } },
    fields: { fields: map, getTextInputValue: (id) => { if (!map.has(id)) throw new Error('no field'); return map.get(id).value; } },
    deferReply: async () => {},
    editReply: async (p) => { replies.push(p); },
  };
}

test('DB: the setup form saves content and embed, keeps the card flag, refuses stale forms and non-admins', { skip: skipDb }, async () => {
  const { getSettings } = await import('../src/services/settingsService.js');
  await resetGuild();
  const guild = await dbGuild();
  guild.ownerId = 'nobody';
  await setDb({ welcomeEmbed: { title: 'Legacy', color: 0, thumbnail: 'none', card: false } });
  const text = (r) => r.embeds[0].toJSON();

  const fields = { 'tpl-content': 'Welcome {user}!', 'tpl-title': 'Hi {usr}', 'tpl-description': 'You are {server.member_count.ordinal}', 'tpl-color': '#123456', 'tpl-footer': 'bye' };
  const ok = modalInteraction(guild, fields);
  await svc.handleSetupModalSubmit('welcome', ok);
  assert.equal(text(ok.replies[0]).title, '✅  Welcome Message Saved');
  assert.match(text(ok.replies[0]).description, /Unknown variables: `\{usr\}`/);
  const after = await getSettings(DBG, { fresh: true });
  assert.equal(after.welcomeMessage, 'Welcome {user}!');
  assert.equal(after.welcomeEmbed.title, 'Hi {usr}');
  assert.equal(after.welcomeEmbed.color, 0x123456);
  assert.equal(after.welcomeEmbed.footer.text, 'bye');
  assert.equal(welcomeCardEnabled(after), false, 'a card the admin switched off stays off (R072)');

  const badColor = modalInteraction(guild, { ...fields, 'tpl-color': 'FFFFFFF' });
  await svc.handleSetupModalSubmit('welcome', badColor);
  assert.match(text(badColor.replies[0]).title, /Not Saved/);
  assert.equal((await getSettings(DBG, { fresh: true })).welcomeEmbed.color, 0x123456, 'nothing was written');

  const tooLong = modalInteraction(guild, { ...fields, 'tpl-title': 'x'.repeat(300) });
  await svc.handleSetupModalSubmit('welcome', tooLong);
  assert.match(text(tooLong.replies[0]).description, /Title is 300 characters/);

  const stale = modalInteraction(guild, { 'welcome-title': 'x', 'welcome-description': 'y', 'welcome-color': '', 'welcome-thumbnail': '', 'welcome-footer': '' });
  await svc.handleSetupModalSubmit('welcome', stale);
  assert.match(text(stale.replies[0]).title, /Form Expired/);
  assert.equal((await getSettings(DBG, { fresh: true })).welcomeMessage, 'Welcome {user}!', 'a stale form never wipes the config');

  const denied = modalInteraction(guild, fields, { admin: false });
  await svc.handleSetupModalSubmit('welcome', denied);
  assert.match(text(denied.replies[0]).title, /Insufficient Permissions/);

  // An emptied form: the text becomes '' (explicit), the empty embed becomes null, and the warnings say so.
  const emptied = modalInteraction(guild, { 'tpl-content': '', 'tpl-title': '', 'tpl-description': '', 'tpl-color': '', 'tpl-footer': '' });
  await svc.handleSetupModalSubmit('welcome', emptied);
  const wiped = await getSettings(DBG, { fresh: true });
  assert.equal(wiped.welcomeMessage, '');
  assert.equal(wiped.welcomeEmbed, null);
  assert.match(text(emptied.replies[0]).description, /default embed is used/);

  // DM and goodbye forms write their own columns.
  const dm = modalInteraction(guild, { ...fields, 'tpl-content': 'psst' });
  await svc.handleSetupModalSubmit('dm', dm);
  const g = modalInteraction(guild, { ...fields, 'tpl-content': '**{user.idname}** left' });
  await svc.handleSetupModalSubmit('goodbye', g);
  const last = await getSettings(DBG, { fresh: true });
  assert.equal(last.welcomeDmMessage, 'psst');
  assert.equal(last.welcomeDmEmbed.title, 'Hi {usr}');
  assert.equal(last.goodbyeMessage, '**{user.idname}** left');
  assert.equal(last.goodbyeEmbed.title, 'Hi {usr}');
  await resetGuild();
});
