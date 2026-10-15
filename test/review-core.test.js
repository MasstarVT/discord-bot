// Regression tests for the core review: the router's error reply after an interaction was acknowledged,
// IPv4-translated IPv6 addresses in the SSRF check, and the settings write-through when the Redis SET fails.
// Offline: no Discord, Postgres, Redis or network.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

Object.assign(process.env, {
  NODE_ENV: 'test',
  DISCORD_TOKEN: 'test-token',
  CLIENT_ID: '000000000000000000',
  DATABASE_URL: 'postgresql://test:test@127.0.0.1:1/test',
  REDIS_URL: 'redis://127.0.0.1:1',
});

const { routeInteraction } = await import('../src/handlers/interactionRouter.js');
const { createHandlerMaps } = await import('../src/handlers/componentRegistry.js');
const { isBlockedAddress } = await import('../src/utils/safeFetch.js');
const { default: redis, cache } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { updateSettings, updateSettingsWith, settingsKey, getSettings, invalidateSettings, SETTINGS_CACHE_VERSION } =
  await import('../src/services/settingsService.js');

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug };
console.error = console.warn = console.log = console.debug = () => {};

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Router: the error reply never overwrites an acknowledged panel ───────────────

function recorder(over = {}) {
  const calls = [];
  const note = (name) => async (payload) => { calls.push({ name, payload }); };
  return {
    calls,
    replied: false,
    deferred: false,
    ephemeral: null,
    isChatInputCommand: () => false,
    isAutocomplete: () => false,
    isButton: () => false,
    isAnySelectMenu: () => false,
    isModalSubmit: () => false,
    reply: note('reply'),
    editReply: note('editReply'),
    followUp: note('followUp'),
    user: { id: 'u1' },
    ...over,
  };
}

function clientWith(kind, prefix, handler) {
  const client = { commands: new Map() };
  createHandlerMaps(client);
  client[`${kind}Handlers`].set(prefix, handler);
  return client;
}

const names = (i) => i.calls.map((c) => c.name);

test('a button handler that throws after deferUpdate() gets a follow-up, not an edit of the clicked panel', async () => {
  const client = clientWith('button', 'giveaway', async (i) => { i.deferred = true; throw new Error('boom'); });
  const i = recorder({ isButton: () => true, customId: 'giveaway:1' });
  await routeInteraction(i, client);
  assert.deepEqual(names(i), ['followUp']);
  assert.equal(i.calls[0].payload.ephemeral, true);
  assert.match(i.calls[0].payload.embeds[0].data.title, /Button Error/);
});

test('a select handler that throws after update() gets a follow-up', async () => {
  const client = clientWith('select', 'rr-select', async (i) => { i.replied = true; throw new Error('boom'); });
  const i = recorder({ isAnySelectMenu: () => true, customId: 'rr-select:1' });
  await routeInteraction(i, client);
  assert.deepEqual(names(i), ['followUp']);
  assert.equal(i.calls[0].payload.ephemeral, true);
});

test('a modal handler that throws after deferReply() still replaces the deferred placeholder', async () => {
  const client = clientWith('modal', 'welcome-setup', async (i) => { i.deferred = true; i.ephemeral = true; throw new Error('boom'); });
  const i = recorder({ isModalSubmit: () => true, customId: 'welcome-setup' });
  await routeInteraction(i, client);
  assert.deepEqual(names(i), ['editReply']);
  assert.match(i.calls[0].payload.embeds[0].data.title, /Modal Error/);
});

test('a slash command that replied and then threw gets an ephemeral follow-up, not an overwrite of its reply', async () => {
  const client = { commands: new Map([['boom', {
    execute: async (i) => { i.replied = true; i.ephemeral = false; throw new Error('boom'); },
  }]]) };
  createHandlerMaps(client);
  const i = recorder({
    isChatInputCommand: () => true,
    commandName: 'boom',
    guildId: 'g-review-core',
    inGuild: () => true,
    inCachedGuild: () => true,
    options: { getSubcommand: () => null, getSubcommandGroup: () => null },
  });
  await routeInteraction(i, client);
  assert.deepEqual(names(i), ['followUp']);
  assert.equal(i.calls[0].payload.ephemeral, true);
});

test('a handler that throws before acknowledging gets an ephemeral reply', async () => {
  const client = clientWith('button', 'giveaway', async () => { throw new Error('boom'); });
  const i = recorder({ isButton: () => true, customId: 'giveaway:1' });
  await routeInteraction(i, client);
  assert.deepEqual(names(i), ['reply']);
  assert.equal(i.calls[0].payload.ephemeral, true);
});

// ── safeFetch: IPv4-translated IPv6 (::ffff:0:a.b.c.d, RFC 2765) ──────────────────

test('isBlockedAddress blocks IPv4-translated IPv6 forms that embed a private IPv4 address', () => {
  assert.equal(isBlockedAddress('::ffff:0:7f00:1'), true); // 127.0.0.1
  assert.equal(isBlockedAddress('::ffff:0:127.0.0.1'), true);
  assert.equal(isBlockedAddress('::ffff:0:a9fe:a9fe'), true); // 169.254.169.254
  assert.equal(isBlockedAddress('0:0:0:0:ffff:0:c0a8:1'), true); // 192.168.0.1
  assert.equal(isBlockedAddress('::ffff:0:808:808'), false); // 8.8.8.8
  // The neighbouring forms keep their verdicts.
  assert.equal(isBlockedAddress('::ffff:7f00:1'), true);
  assert.equal(isBlockedAddress('::ffff:808:808'), false);
  assert.equal(isBlockedAddress('2606:4700::1111'), false);
});

// ── settingsService: a failed Redis SET must not leave the previous row behind ──────

function stubSettingsStore(initial) {
  const real = prisma.guildSettings;
  let row = { ...initial };
  prisma.guildSettings = {
    findUnique: async () => ({ ...row }),
    upsert: async ({ update }) => { row = { ...row, ...update }; return { ...row }; },
    update: async ({ data }) => { row = { ...row, ...data }; return { ...row }; },
  };
  return () => { prisma.guildSettings = real; };
}

test('updateSettings drops the Redis key when the SET fails, so the old row cannot come back', async () => {
  const guildId = 'g-review-settings-1';
  const restoreDb = stubSettingsStore({ guildId, countingEnabled: false });
  const realSet = cache.set;
  const realDel = cache.del;
  const dels = [];
  cache.set = async () => false; // Redis timed out
  cache.del = async (...keys) => { dels.push(...keys); return true; };
  try {
    const row = await updateSettings(guildId, { countingEnabled: true });
    assert.equal(row.countingEnabled, true);
    assert.deepEqual(dels, [settingsKey(guildId)]);
  } finally {
    cache.set = realSet;
    cache.del = realDel;
    restoreDb();
    await invalidateSettings(guildId);
  }
});

test('updateSettings leaves the key alone when the SET succeeds', async () => {
  const guildId = 'g-review-settings-2';
  const restoreDb = stubSettingsStore({ guildId, countingEnabled: false });
  const realSet = cache.set;
  const realDel = cache.del;
  const sets = [];
  const dels = [];
  cache.set = async (key, value, ttl) => { sets.push({ key, value, ttl }); return true; };
  cache.del = async (...keys) => { dels.push(...keys); return true; };
  try {
    await updateSettings(guildId, { countingEnabled: true });
    assert.equal(sets.length, 1);
    assert.equal(sets[0].key, settingsKey(guildId));
    assert.equal(sets[0].value._cacheV, SETTINGS_CACHE_VERSION);
    assert.equal(sets[0].value.countingEnabled, true);
    assert.deepEqual(dels, []);
  } finally {
    cache.set = realSet;
    cache.del = realDel;
    restoreDb();
    await invalidateSettings(guildId);
  }
});

test('updateSettingsWith publishes through the same path (a failed SET drops the key)', async () => {
  const guildId = 'g-review-settings-3';
  const realSet = cache.set;
  const realDel = cache.del;
  const dels = [];
  const realTx = prisma.$transaction;
  const realGs = prisma.guildSettings;
  let row = { guildId, modRoleIds: ['1'] };
  prisma.$transaction = async (fn) => fn({
    $executeRaw: async () => 1,
    $queryRaw: async () => [],
    guildSettings: {
      findUnique: async () => ({ ...row }),
      update: async ({ data }) => { row = { ...row, ...data }; return { ...row }; },
    },
  });
  cache.set = async () => false;
  cache.del = async (...keys) => { dels.push(...keys); return true; };
  try {
    const out = await updateSettingsWith(guildId, (cur) => ({ modRoleIds: [...cur.modRoleIds, '2'] }));
    assert.deepEqual([...out.modRoleIds], ['1', '2']);
    assert.deepEqual(dels, [settingsKey(guildId)]);
    // The write went through to the memo, so this process still sees the new row.
    prisma.guildSettings = { findUnique: async () => { throw new Error('must come from the memo'); } };
    assert.deepEqual([...(await getSettings(guildId)).modRoleIds], ['1', '2']);
  } finally {
    prisma.$transaction = realTx;
    prisma.guildSettings = realGs;
    cache.set = realSet;
    cache.del = realDel;
    await invalidateSettings(guildId);
  }
});

test('placeholders: a nickname that looks like a mention renders as text, not a mention', async () => {
  const { renderPlaceholders } = await import('../src/utils/placeholderParser.js');
  const user = { id: '1', username: 'u', globalName: null };
  const member = { displayName: 'hi <@123> <@&456> <#789>' };
  const out = renderPlaceholders('{user.display_name}', { user, member });
  assert.doesNotMatch(out, /<@\d|<@&\d|<#\d/);
  assert.equal(out.replace(/\u200b/g, ''), 'hi <@123> <@&456> <#789>', 'the visible text is unchanged');
  assert.equal(renderPlaceholders('{user}', { user, member }), '<@1>', '{user} is still a real mention');
});
