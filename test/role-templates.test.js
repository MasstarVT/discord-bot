// Role templates (pronouns, colors, ..., timezones): the presets, setupRoleTemplate against a fake guild (reuse by name, the
// caps and the assignability checks before anything is created, the colors, the timezone links it writes), the dashboard
// template field, the form's cascading showWhen, and the /reactionrole template choices. Offline.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails fast and Postgres is never reached.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const quiet = { log: console.log, info: console.info, warn: console.warn, error: console.error, debug: console.debug };
for (const k of Object.keys(quiet)) console[k] = () => {};

const { PermissionFlagsBits, PermissionsBitField, Collection } = await import('discord.js');
const { LIMITS } = await import('../src/config/constants.js');
const { isValidTimeZone } = await import('../src/utils/time.js');
const tz = await import('../src/utils/timezones.js');
const tpl = await import('../src/utils/roleTemplates.js');
const { UNICODE_EMOJI_RE } = await import('../src/services/reactionRoleService.js');
const setup = await import('../src/services/roleTemplateSetup.js');
const rrd = await import('../src/dashboard/collections/reactionRoles.js');
const rrCmd = await import('../src/commands/admin/reactionrole.js');
const { default: redis } = await import('../src/services/redis.js');

after(() => {
  Object.assign(console, quiet);
  redis.disconnect();
});

const GUILD = '900000000000000001';

/** A guild with the bot at position 50 holding Manage Roles, and roles.create that adds to the cache. */
function fakeGuild(existing = [], { manageRoles = true } = {}) {
  const cache = new Collection();
  let next = 1;
  const add = ({ name, position = 1, permissions = 0n, managed = false }) => {
    const id = `9100000000000000${String(next++).padStart(2, '0')}`;
    const role = { id, name, position, managed, permissions: new PermissionsBitField(permissions) };
    cache.set(id, role);
    return role;
  };
  cache.set(GUILD, { id: GUILD, name: '@everyone', position: 0, managed: false, permissions: new PermissionsBitField(0n) });
  for (const r of existing) add(r);
  const created = [];
  const guild = {
    id: GUILD,
    ownerId: '1',
    roles: {
      cache,
      create: async (opts) => {
        created.push(opts);
        return add({ name: opts.name });
      },
    },
    members: {
      me: {
        permissions: { has: (bit) => manageRoles && bit === PermissionFlagsBits.ManageRoles },
        roles: { highest: { position: 50 } },
      },
    },
  };
  return { guild, created, add };
}

/** settingsService stand-in: one in-memory row. */
function fakeSettings(row = { guildId: GUILD, timezone: 'UTC', timezoneRoles: null }) {
  const state = { row: { ...row }, writes: 0 };
  return {
    state,
    store: {
      get: async () => state.row,
      updateWith: async (_id, mutator) => {
        state.writes++;
        state.row = { ...state.row, ...mutator(state.row) };
        return state.row;
      },
    },
  };
}

// ── Presets ───────────────────────────────────────────────────────────────────

test('presets: valid zones, unique names, fit a panel and the timezone-role cap', () => {
  for (const [key, t] of Object.entries(tz.TIMEZONE_ROLE_TEMPLATES)) {
    assert.match(key, /^timezones-/);
    assert.ok(t.roles.length > 0 && t.roles.length <= 20 && t.roles.length <= LIMITS.TIMEZONE_ROLES, key);
    assert.equal(new Set(t.roles.map((r) => r.name.toLowerCase())).size, t.roles.length, `${key}: names`);
    for (const r of t.roles) {
      assert.ok(isValidTimeZone(r.zone) && r.zone.includes('/'), `${key}: ${r.name} -> ${r.zone}`);
      assert.ok(r.label.length <= 80 && r.description.length <= 100, `${key}: ${r.name} label/description`);
    }
  }
  assert.equal(tz.TIMEZONE_ROLE_TEMPLATES['timezones-us'].roles.find((r) => r.name === 'CST').zone, 'America/Chicago');
  assert.deepEqual([tz.TIMEZONE_PANEL.type, tz.TIMEZONE_PANEL.layout], ['UNIQUE', 'SELECT']);
});

// ── setupRoleTemplate: timezones ─────────────────────────────────────────────────────────────────────────────────────────────────────

test('creates the missing roles with no permissions, reuses existing ones by name, and links every one', async () => {
  const { guild, created } = fakeGuild([{ name: 'cst', position: 3 }, { name: 'Members', position: 2 }]);
  const s = fakeSettings({ guildId: GUILD, timezone: 'UTC', timezoneRoles: [{ roleId: '999999999999999999', zone: 'UTC' }] });
  const out = await setup.setupRoleTemplate(guild, 'timezones-us', { settingsStore: s.store, reason: 'test' });
  assert.equal(out.roles.length, 6);
  assert.deepEqual([out.created, out.reused], [5, 1]);
  assert.deepEqual(created.map((c) => c.name), ['HST', 'AKST', 'PST', 'MST', 'EST']);
  for (const c of created) assert.deepEqual([c.permissions, c.mentionable, c.hoist, c.reason], [[], false, false, 'test']);
  const cst = out.roles.find((r) => r.name === 'CST');
  assert.equal(cst.created, false);
  assert.equal(guild.roles.cache.get(cst.roleId).name, 'cst');
  // The deleted role's old link is dropped; each template role is linked to its zone.
  assert.deepEqual(s.state.row.timezoneRoles, out.roles.map((r) => ({ roleId: r.roleId, zone: r.zone })));
  assert.equal(setup.describeSetup(out), 'Created 5 roles and reused 1 that already existed.');

  // Running it again reuses all six and writes the same links.
  const again = await setup.setupRoleTemplate(guild, 'timezones-us', { settingsStore: s.store });
  assert.deepEqual([again.created, again.reused], [0, 6]);
  assert.equal(created.length, 5);
  assert.equal(s.state.row.timezoneRoles.length, 6);
  assert.equal(setup.describeSetup(again), 'Reused 6 roles that already existed.');
});

test('refuses before creating anything: no Manage Roles, a reused role it cannot hand out, or too many links', async () => {
  const refuse = async (guildSetup, settings, pattern) => {
    const { guild, created } = guildSetup;
    const s = fakeSettings(settings);
    await assert.rejects(setup.setupRoleTemplate(guild, 'timezones-us', { settingsStore: s.store }),
      (err) => err instanceof setup.RoleTemplateError && pattern.test(err.message));
    assert.equal(created.length, 0, String(pattern));
    assert.equal(s.state.writes, 0, String(pattern));
  };
  await refuse(fakeGuild([], { manageRoles: false }), undefined, /Manage Roles/);
  await refuse(fakeGuild([{ name: 'EST', position: 60 }]), undefined, /EST.*above my highest role/);
  await refuse(fakeGuild([{ name: 'PST', position: 4, permissions: PermissionFlagsBits.Administrator }]), undefined, /PST.*dangerous/);
  const full = fakeGuild();
  const links = Array.from({ length: LIMITS.TIMEZONE_ROLES - 2 }, (_, i) => full.add({ name: `zone${i}` })).map((r) => ({ roleId: r.id, zone: 'UTC' }));
  await refuse(full, { guildId: GUILD, timezone: 'UTC', timezoneRoles: links }, /most is 25/);
  await assert.rejects(setup.setupRoleTemplate(fakeGuild().guild, 'nope', { settingsStore: fakeSettings().store }), /does not exist/);
});

test('a Discord refusal names the roles already created, so a retry reuses them', async () => {
  const { guild } = fakeGuild();
  let n = 0;
  const create = guild.roles.create;
  guild.roles.create = async (opts) => {
    if (++n === 3) throw new Error('Maximum number of guild roles reached (250)');
    return create(opts);
  };
  const s = fakeSettings();
  await assert.rejects(setup.setupRoleTemplate(guild, 'timezones-us', { settingsStore: s.store }),
    (err) => err instanceof setup.RoleTemplateError && /\*\*PST\*\*/.test(err.message) && /Already created: HST, AKST/.test(err.message));
  assert.equal(s.state.writes, 0, 'nothing is linked after a failure');
});

test('findRoleByName ignores case, @everyone and integration roles, and takes the highest', () => {
  const { guild } = fakeGuild([{ name: 'CST', position: 2 }, { name: 'cst', position: 7 }, { name: 'EST', managed: true }]);
  assert.equal(setup.findRoleByName(guild, 'Cst').position, 7);
  assert.equal(setup.findRoleByName(guild, 'EST'), null);
  assert.equal(setup.findRoleByName(guild, '@everyone'), null);
});

// ── Role sets ─────────────────────────────────────────────────────────────────

test('role sets: unique names, panel-sized texts, standard emojis, a mode and layout a template panel can use', () => {
  const keys = Object.keys(tpl.ROLE_TEMPLATES);
  for (const k of ['pronouns', 'colors', 'age', 'regions', 'platforms', 'dms', 'pings', ...Object.keys(tz.TIMEZONE_ROLE_TEMPLATES)]) {
    assert.ok(keys.includes(k), k);
  }
  for (const [key, t] of Object.entries(tpl.ROLE_TEMPLATES)) {
    assert.ok(t.label.length <= 100, `${key}: label`);
    assert.ok(t.roles.length > 0 && t.roles.length <= 20, `${key}: role count`);
    assert.equal(new Set(t.roles.map((r) => r.name.toLowerCase())).size, t.roles.length, `${key}: names`);
    assert.ok(['NORMAL', 'UNIQUE'].includes(t.panel.type) && ['BUTTONS', 'SELECT'].includes(t.panel.layout), `${key}: panel`);
    assert.ok(t.panel.title.length <= 256 && t.panel.description.length <= 4000 && (t.panel.placeholder ?? '').length <= 150, `${key}: texts`);
    assert.equal(t.linksZones, key.startsWith('timezones-'), `${key}: linksZones`);
    for (const r of t.roles) {
      assert.ok(r.name.length <= 100 && r.label.length <= 80 && (r.description ?? '').length <= 100, `${key}: ${r.name}`);
      if (r.emoji != null) assert.match(r.emoji, UNICODE_EMOJI_RE, `${key}: ${r.name} emoji`);
      if (r.color != null) assert.ok(Number.isInteger(r.color) && r.color >= 0 && r.color <= 0xFFFFFF, `${key}: ${r.name} color`);
    }
  }
  // Discord's minimum age is 13.
  for (const r of tpl.ROLE_TEMPLATES.age.roles) assert.ok(Number.parseInt(r.name, 10) >= 13, r.name);
  assert.ok(tpl.ROLE_TEMPLATES.colors.roles.every((r) => r.color > 0));
  assert.equal(tpl.ROLE_TEMPLATES.colors.panel.type, 'UNIQUE');
  assert.equal(tpl.ROLE_TEMPLATES.pronouns.panel.type, 'NORMAL');
  assert.equal(tpl.getRoleTemplate('verify'), null);
  assert.equal(tpl.getRoleTemplate('__proto__'), null);
});

test('a role set creates its roles with their colors, reuses existing ones and writes no settings', async () => {
  const { guild, created } = fakeGuild([{ name: 'red', position: 3 }]);
  const s = fakeSettings();
  const out = await setup.setupRoleTemplate(guild, 'colors', { settingsStore: s.store, reason: 'test' });
  const colors = tpl.ROLE_TEMPLATES.colors.roles;
  assert.deepEqual([out.created, out.reused], [colors.length - 1, 1]);
  assert.deepEqual(created.map((c) => c.name), colors.slice(1).map((r) => r.name));
  for (const c of created) {
    assert.deepEqual([c.permissions, c.mentionable, c.hoist, c.reason], [[], false, false, 'test']);
    assert.equal(c.colors.primaryColor, colors.find((r) => r.name === c.name).color, c.name);
  }
  assert.deepEqual(out.roles.map((r) => r.emoji), colors.map((r) => r.emoji));
  assert.equal(guild.roles.cache.get(out.roles[0].roleId).name, 'red');
  assert.equal(s.state.writes, 0);
  // The reply names the reused role, which members can now give themselves.
  assert.match(setup.describeReused(out), /^Existing role now on the panel: \*\*Red\*\*\. .*check it doesn't unlock/);
  assert.equal(setup.describeReused({ roles: out.roles.map((r) => ({ ...r, created: true })) }), null);

  // A set without colors passes none.
  const plain = fakeGuild();
  await setup.setupRoleTemplate(plain.guild, 'pronouns', { settingsStore: s.store });
  assert.ok(plain.created.every((c) => !Object.hasOwn(c, 'colors')));
  assert.equal(s.state.writes, 0);
});

test('a role set refuses before creating anything near Discord\'s role cap or with a role it cannot hand out', async () => {
  const full = fakeGuild();
  for (let i = 0; i < 245; i++) full.add({ name: `role${i}` });
  await assert.rejects(setup.setupRoleTemplate(full.guild, 'colors', { settingsStore: fakeSettings().store }),
    (err) => err instanceof setup.RoleTemplateError && /limit of 250/.test(err.message));
  assert.equal(full.created.length, 0);

  const high = fakeGuild([{ name: 'They/Them', position: 60 }]);
  await assert.rejects(setup.setupRoleTemplate(high.guild, 'pronouns', { settingsStore: fakeSettings().store }),
    (err) => err instanceof setup.RoleTemplateError && /They\/Them.*above my highest role/.test(err.message));
  assert.equal(high.created.length, 0);
});

// ── Dashboard and command wiring ──────────────────────────────────────────────

test('the dashboard template field: create-only, options from the presets, hides layout, mode and roles', () => {
  const fields = rrd.collection.fields;
  const template = fields.find((f) => f.field === 'template');
  assert.equal(template.default, 'none');
  assert.deepEqual(template.options.map((o) => o.value), ['none', ...Object.keys(tpl.ROLE_TEMPLATES)]);
  assert.ok(template.options.every((o) => (o.description ?? '').length <= 300));
  assert.ok(rrd.collection.createOnly.includes('template'));
  for (const name of ['layout', 'type', 'items']) {
    assert.deepEqual(fields.find((f) => f.field === name).showWhen, { field: 'template', notIn: Object.keys(tpl.ROLE_TEMPLATES) }, name);
  }
  // A saved panel carries no template (create-only fields are not sent back on edit), so its fields stay shown.
  assert.equal(Object.hasOwn(rrd.panelItem({ id: 'g', channelId: '1', layout: 'SELECT', type: 'UNIQUE', title: 't', messageId: '2', items: [] }), 'template'), false);
});

test('forms.js hides a field whose showWhen sibling is itself hidden', () => {
  const src = readFileSync(new URL('../src/dashboard/public/forms.js', import.meta.url), 'utf8');
  assert.match(src, /const shown = !hidden\.has\(field\)/);
});

test('/reactionrole create offers every role template', () => {
  const create = rrCmd.data.toJSON().options.find((o) => o.name === 'create');
  const choices = create.options.find((o) => o.name === 'template').choices.map((c) => c.value);
  assert.deepEqual(choices, ['verify', 'notifications', ...Object.keys(tpl.ROLE_TEMPLATES)]);
  assert.ok(create.options.find((o) => o.name === 'template').description.length <= 100);
});
