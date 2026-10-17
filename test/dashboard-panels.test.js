// Dashboard panels: the `reactionRoles` and `ticketPanels` record collections (src/dashboard/collections/{reactionRoles,
// ticketPanels}.js and their .store.js) and the tasks the bot runs for them (src/services/dashboardTasks/{reactionRoles,tickets}.js).
// Offline by default: descriptor lint, the validation of create / update / action requests (validateRecordInput, including the
// per-layout rules), the pure helpers, guards that fail when a copied rule drifts from its service or command, and the tasks'
// permission and payload checks. With TEST_DATABASE_URL (a migrated database) the stores run against it too, together with the
// real tasks on fake discord.js objects (a fake `runTask` does what src/dashboard/tasks.js plus the bot's job handler do):
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/dashboard-panels.test.js
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { Collection, ChannelType, PermissionFlagsBits } from 'discord.js';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is only used with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { LIMITS } = await import('../src/config/constants.js');
const { MODULES } = await import('../src/config/modules.js');
const { FieldError } = await import('../src/dashboard/schema.js');
const { RecordError, unwrapResult } = await import('../src/dashboard/collections/errors.js');
const registry = await import('../src/dashboard/collections/index.js');
const { COLLECTIONS, lintCollections, publicCollection, validateRecordInput } = registry;
const rrd = await import('../src/dashboard/collections/reactionRoles.js');
const tpd = await import('../src/dashboard/collections/ticketPanels.js');
const rrSvc = await import('../src/services/reactionRoleService.js');
const tkSvc = await import('../src/services/ticketService.js');
const { tasks: rrTasks } = await import('../src/services/dashboardTasks/reactionRoles.js');
const { tasks: tkTasks } = await import('../src/services/dashboardTasks/tickets.js');
const { TaskError, TASK_KIND_RE } = await import('../src/services/dashboardTasks/shared.js');
const rrCommand = await import('../src/commands/admin/reactionrole.js');
const tkCommand = await import('../src/commands/admin/ticket-panel.js');
const { ReactionRoleType, RRLayout, TicketPanelStyle } = await import('@prisma/client');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { emptyTemplate } = await import('../src/utils/customEmbed.js');

// Silence the Redis reconnect and logger noise (the logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const KEYS = ['reactionRoles', 'ticketPanels'];
const guilds = [];

after(async () => {
  Object.assign(console, quiet);
  if (DB) for (const guildId of guilds) await prisma.guildSettings.deleteMany({ where: { guildId } }).catch(() => {});
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Helpers ──────────────────────────────────────────────────────────────────

const snow = (i) => `8${String(i).padStart(17, '0')}`;
const ROLE1 = snow(1);
const ROLE2 = snow(2);
const ROLE3 = snow(3);
const CHAN = snow(10);
const CHAN2 = snow(11);
const CAT1 = snow(20);
const CAT2 = snow(21);
const CAT3 = snow(22);
const CUSTOM = '<:party:555555555555555555>';

const create = { mode: 'create' };
const update = (current = null) => ({ mode: 'update', current });
const accept = (key, data, opts = create) => validateRecordInput(key, data, opts);

/** Asserts a FieldError (optionally at `path`, optionally matching `message`). */
function refuse(key, data, opts, { path, message } = {}) {
  assert.throws(() => validateRecordInput(key, data, opts), (err) => {
    assert.ok(err instanceof FieldError, `expected a FieldError, got ${err?.name}: ${err?.message}`);
    if (path !== undefined) assert.equal(err.path, path, `path of "${err.message}"`);
    if (message !== undefined) assert.match(err.message, message);
    return true;
  });
}

const rrItem = (roleId, over = {}) => ({ roleId, label: 'Role', emoji: null, style: 1, description: null, ...over });
const rrBody = (over = {}) => ({ channelId: CHAN, layout: 'BUTTONS', type: 'NORMAL', title: 'Pick your roles', items: [rrItem(ROLE1)], ...over });
const opt = (label, over = {}) => ({
  label, emoji: null, description: null, buttonStyle: 1, categoryId: null, introTemplate: emptyTemplate(), id: null, ...over,
});

// ── Descriptors ──────────────────────────────────────────────────────────────

describe('descriptors', () => {
  test('both collections are registered and lint clean', () => {
    for (const key of KEYS) assert.ok(COLLECTIONS[key], key);
    assert.deepEqual(lintCollections().filter((p) => KEYS.some((k) => p.startsWith(`${k}.`) || p.startsWith(`${k}(`) || p.startsWith(`${k}!`))), []);
    assert.equal(COLLECTIONS.reactionRoles.module, 'reactionroles');
    assert.equal(COLLECTIONS.ticketPanels.module, 'tickets');
    for (const key of KEYS) assert.ok(Object.hasOwn(MODULES, COLLECTIONS[key].module), key);
    assert.ok(!COLLECTIONS.reactionRoles.placeholder && !COLLECTIONS.ticketPanels.placeholder, 'no longer placeholders');
  });

  test('limits come from LIMITS', () => {
    assert.equal(COLLECTIONS.reactionRoles.limit, LIMITS.REACTION_ROLE_PANELS);
    assert.equal(COLLECTIONS.ticketPanels.limit, LIMITS.TICKET_PANELS);
    const t = (name) => COLLECTIONS.ticketPanels.fields.find((f) => f.field === name);
    assert.equal(t('options').maxItems, LIMITS.TICKET_PANEL_OPTIONS);
    assert.equal(t('options').minItems, 1, 'a panel keeps at least one option');
    assert.equal(t('managerRoleIds').maxItems, LIMITS.TICKET_MANAGER_ROLES);
    const items = COLLECTIONS.reactionRoles.fields.find((f) => f.field === 'items');
    assert.equal(items.maxItems, Math.max(...Object.values(rrd.MAX_ITEMS)));
  });

  test('title and subtitle fields exist, createOnly fields are fields, and the public copy is plain JSON', () => {
    for (const key of KEYS) {
      const c = COLLECTIONS[key];
      const names = new Set([...c.fields, ...c.readOnly].map((f) => f.field));
      assert.ok(names.has(c.titleField), `${key}: titleField ${c.titleField}`);
      assert.equal(c.subtitleField, 'summary', `${key}: the row subtitle is the item's summary`);
      for (const f of c.createOnly) assert.ok(c.fields.some((d) => d.field === f), `${key}: createOnly ${f}`);
      const pub = publicCollection(c);
      assert.deepEqual(JSON.parse(JSON.stringify(pub)), JSON.parse(JSON.stringify(pub)));
      assert.ok(!JSON.stringify(pub).includes('"check"'), `${key}: no server-only hooks in the public copy`);
    }
    assert.deepEqual([...COLLECTIONS.reactionRoles.createOnly], ['template', 'channelId', 'layout']);
  });

  test('a showWhen only points at a sibling field, and the hidden option id is the only always-hidden field', () => {
    const lists = [
      ['reactionRoles', COLLECTIONS.reactionRoles.fields],
      ['reactionRoles.items', COLLECTIONS.reactionRoles.fields.find((f) => f.field === 'items').itemFields],
      ['ticketPanels', COLLECTIONS.ticketPanels.fields],
      ['ticketPanels.options', COLLECTIONS.ticketPanels.fields.find((f) => f.field === 'options').itemFields],
    ];
    const hidden = [];
    for (const [where, fields] of lists) {
      const names = new Set(fields.map((f) => f.field));
      for (const f of fields.filter((d) => d.showWhen)) {
        assert.ok(names.has(f.showWhen.field), `${where}.${f.field} shows when ${f.showWhen.field}`);
        assert.notEqual(f.showWhen.field, f.field);
        if (f.showWhen.in?.length === 0) hidden.push(`${where}.${f.field}`);
      }
    }
    assert.deepEqual(hidden, ['ticketPanels.options.id']);
    const rr = COLLECTIONS.reactionRoles.fields;
    assert.deepEqual(rr.find((f) => f.field === 'placeholder').showWhen, { field: 'layout', in: ['SELECT'] });
    assert.deepEqual(rr.find((f) => f.field === 'maxRoles').showWhen, { field: 'type', in: ['NORMAL'] });
    // the hidden id is the last item field, so a card is titled by its label
    const options = COLLECTIONS.ticketPanels.fields.find((f) => f.field === 'options').itemFields;
    assert.equal(options.at(-1).field, 'id');
    assert.equal(options[0].field, 'label');
  });

  test('role and channel kinds are the ones the commands accept', () => {
    const rr = (name) => COLLECTIONS.reactionRoles.fields.find((f) => f.field === name);
    const tp = (name) => COLLECTIONS.ticketPanels.fields.find((f) => f.field === name);
    assert.deepEqual(rr('channelId').channelTypes, ['text', 'announcement']);
    const role = rr('items').itemFields.find((f) => f.field === 'roleId');
    assert.equal(role.assignable, true, 'the bot gives these roles');
    assert.ok(!role.allowDangerous && !role.allowEveryone, 'dangerous roles stay a /reactionrole add decision');
    assert.ok(!tp('managerRoleIds').assignable, 'manager roles are never given');
    assert.ok(!tp('managerRoleIds').allowEveryone, '@everyone cannot be a manager role');
    for (const f of ['openCategoryId', 'claimedCategoryId', 'closedCategoryId']) assert.deepEqual(tp(f).channelTypes, ['category'], f);
    assert.deepEqual(tp('transcriptChannelId').channelTypes, ['text', 'announcement']);
    assert.deepEqual(tp('channelId').channelTypes, ['text', 'announcement']);
    assert.deepEqual(tp('options').itemFields.find((f) => f.field === 'categoryId').channelTypes, ['category']);
  });

  test('the actions are the ones the UI shows: repost; publish and refresh', () => {
    assert.deepEqual(publicCollection(COLLECTIONS.reactionRoles).actions.map((a) => a.id), ['repost']);
    assert.deepEqual(publicCollection(COLLECTIONS.ticketPanels).actions.map((a) => a.id), ['publish', 'refresh']);
    // Neither collection has a readOnly channelId+messageId pair, so the UI does not treat them as "posted drafts" whose
    // delete leaves the message behind (here delete takes the message with it).
    for (const key of KEYS) assert.ok(!COLLECTIONS[key].readOnly.some((f) => f.field === 'messageId'), key);
  });

  test('an action that only applies to some items names the field it depends on', () => {
    for (const key of KEYS) {
      const c = COLLECTIONS[key];
      const names = new Set([...c.fields, ...c.readOnly].map((f) => f.field));
      for (const a of publicCollection(c).actions) {
        if (a.showWhen) assert.ok(names.has(a.showWhen.field), `${key}!${a.id} depends on ${a.showWhen.field}`);
      }
    }
    const actions = publicCollection(COLLECTIONS.ticketPanels).actions;
    // "Update posted message" means nothing for a panel that is not posted; Publish always applies
    assert.deepEqual(actions.find((a) => a.id === 'refresh').showWhen, { field: 'status', in: [tpd.STATUS.PUBLISHED] });
    assert.equal(actions.find((a) => a.id === 'publish').showWhen, null);
    // Repost also posts a panel that never was (or whose message was deleted)
    assert.equal(publicCollection(COLLECTIONS.reactionRoles).actions[0].showWhen, null);
  });

  test('enum options match the database enums', () => {
    const rr = COLLECTIONS.reactionRoles.fields;
    const values = (name) => rr.find((f) => f.field === name).options.map((o) => o.value);
    assert.deepEqual(values('type'), Object.values(ReactionRoleType));
    assert.deepEqual(values('layout'), Object.values(RRLayout));
    assert.deepEqual(COLLECTIONS.ticketPanels.fields.find((f) => f.field === 'style').options.map((o) => o.value), Object.values(TicketPanelStyle));
  });
});

// ── Copied rules stay in step with their services and commands ───────────────

describe('drift guards', () => {
  test('reaction role constants equal the service', () => {
    assert.deepEqual(rrd.RR_TYPES, rrSvc.RR_TYPES);
    assert.deepEqual(rrd.RR_LAYOUTS, rrSvc.RR_LAYOUTS);
    assert.deepEqual(rrd.MAX_ITEMS, rrSvc.MAX_ITEMS);
    assert.deepEqual(rrd.TYPE_LABELS, rrSvc.TYPE_LABELS);
    assert.deepEqual(rrd.LAYOUT_LABELS, rrSvc.LAYOUT_LABELS);
    assert.equal(rrd.DEFAULT_TITLE, rrSvc.DEFAULT_TITLE);
    const style = COLLECTIONS.reactionRoles.fields.find((f) => f.field === 'items').itemFields.find((f) => f.field === 'style');
    assert.deepEqual(style.options.map((o) => o.value), Object.values(rrSvc.STYLE_MAP));
  });

  test('emojiKey equals emojiKeyOfStored', () => {
    for (const s of ['⭐', '❤️', '❤', '👍🏽', '<:pog:123456789012345678>', '<a:pog:123456789012345678>', '', null, undefined, '1️⃣', 'abc']) {
      assert.equal(rrd.emojiKey(s), rrSvc.emojiKeyOfStored(s), String(s));
    }
  });

  test('the field limits are the command\'s option limits', () => {
    const json = rrCommand.data.toJSON();
    const sub = (name) => json.options.find((o) => o.name === name).options;
    const max = (name, option) => sub(name).find((o) => o.name === option).max_length;
    const field = (name) => COLLECTIONS.reactionRoles.fields.find((f) => f.field === name);
    const item = (name) => field('items').itemFields.find((f) => f.field === name);
    assert.equal(field('title').maxLength, max('create', 'title'));
    assert.equal(field('description').maxLength, max('create', 'description'));
    assert.equal(field('messageContent').maxLength, max('create', 'message'));
    assert.equal(field('placeholder').maxLength, max('create', 'placeholder'));
    assert.equal(item('label').maxLength, max('add', 'label'));
    assert.equal(item('description').maxLength, max('add', 'description'));
    const maxRoles = sub('edit').find((o) => o.name === 'max_roles');
    assert.equal(field('maxRoles').max, maxRoles.max_value);
    assert.equal(field('maxRoles').min, 1, 'edit offers 0 to clear the limit; the dashboard clears the field');
    assert.equal(rrd.MAX_MAX_ROLES, maxRoles.max_value);
  });

  test('ticket panel limits are the command\'s option limits', () => {
    const json = tkCommand.data.toJSON();
    const sub = (name) => json.options.find((o) => o.name === name).options;
    const grouped = (group, name) => json.options.find((o) => o.name === group).options.find((o) => o.name === name).options;
    const field = (name) => COLLECTIONS.ticketPanels.fields.find((f) => f.field === name);
    const optionField = (name) => field('options').itemFields.find((f) => f.field === name);
    assert.equal(field('name').maxLength, sub('create').find((o) => o.name === 'name').max_length);
    assert.equal(field('channelNameFormat').maxLength, sub('create').find((o) => o.name === 'name_format').max_length);
    assert.equal(field('maxOpenPerUser').max, sub('create').find((o) => o.name === 'max_open').max_value);
    assert.equal(field('maxOpenPerUser').min, sub('create').find((o) => o.name === 'max_open').min_value);
    assert.equal(optionField('label').maxLength, grouped('option', 'add').find((o) => o.name === 'label').max_length);
    assert.equal(optionField('description').maxLength, grouped('option', 'add').find((o) => o.name === 'description').max_length);
    assert.deepEqual(optionField('buttonStyle').options.map((o) => o.value), grouped('option', 'add').find((o) => o.name === 'color').choices.map((c) => c.value));
  });

  test('the ticket defaults and name rules equal the service', () => {
    assert.deepEqual(tpd.NAME_FORMAT_TOKENS, tkSvc.NAME_FORMAT_TOKENS);
    assert.deepEqual({ ...tpd.DEFAULT_OPTION }, { label: tkSvc.DEFAULT_OPTION.label, emoji: tkSvc.DEFAULT_OPTION.emoji, buttonStyle: tkSvc.DEFAULT_OPTION.buttonStyle });
    for (const style of ['BUTTONS', 'SELECT']) assert.deepEqual(tpd.defaultPanelTemplate(style), tkSvc.defaultPanelTemplate(style), style);
    assert.deepEqual(JSON.parse(JSON.stringify(tpd.DEFAULT_INTRO_TEMPLATE)), JSON.parse(JSON.stringify(tkSvc.DEFAULT_INTRO_TEMPLATE)));
    const formats = ['ticket-{number}', '{number}', '{USER.NAME}', '{user.id}-{topic}', '{server}-{number}', '', '   ', 'x'.repeat(91), 'x'.repeat(90), '!!!',
      '  ticket-{number}  ', '{user.display_name} {number}', '{}', '{ number }', 'ticket-{number', '山田-{number}', '{user.tag}'];
    for (const f of formats) assert.deepEqual(tpd.validateNameFormat(f), tkSvc.validateNameFormat(f), JSON.stringify(f));
    for (const s of ['Ticket 0001', '山田 太郎', '!!!', '--a--b--', 'a'.repeat(150), null, 'ÄÖÜ ß']) {
      assert.equal(tpd.sanitizeChannelName(s), tkSvc.sanitizeChannelName(s), String(s));
    }
    const templates = [null, {}, { content: ' ' }, { content: 'x' }, { embed: { color: 5 } }, { embed: { title: 't' } }, { embed: { fields: [{ name: 'a', value: 'b' }] } },
      emptyTemplate(), { content: null, embed: null, buttons: [{ label: 'x', url: 'https://example.com' }] }];
    for (const t of templates) assert.equal(tpd.isEmptyTemplate(t), tkSvc.isEmptyTemplate(t), JSON.stringify(t));
  });
});

// ── Reaction role requests ───────────────────────────────────────────────────

describe('reactionRoles requests', () => {
  test('a minimal create takes the defaults and collects the channel and role ids', () => {
    const { data, refs } = accept('reactionRoles', { channelId: CHAN });
    assert.deepEqual(data, {
      template: 'none', channelId: CHAN, layout: 'BUTTONS', type: 'NORMAL', title: 'Reaction Roles', description: null, messageContent: null, plainText: false,
      maxRoles: null, placeholder: null, items: [],
    });
    assert.deepEqual(refs.channels.map((c) => [c.id, c.kinds]), [[CHAN, ['text', 'announcement']]]);
    const full = accept('reactionRoles', rrBody({ items: [rrItem(ROLE1), rrItem(ROLE2)] }));
    assert.deepEqual(full.refs.roles.map((r) => [r.id, r.path, r.assignable, r.allowDangerous]), [
      [ROLE1, 'items.0.roleId', true, false], [ROLE2, 'items.1.roleId', true, false],
    ]);
  });

  test('the whole panel validates: every layout, type and text field', () => {
    const { data } = accept('reactionRoles', {
      channelId: CHAN, layout: 'SELECT', type: 'UNIQUE', title: '  Colours  ', description: 'Pick one', messageContent: 'Hello', plainText: true,
      maxRoles: null, placeholder: 'Choose', items: [rrItem(ROLE1, { label: 'Red', emoji: CUSTOM, style: 4, description: 'Warm' })],
    });
    assert.equal(data.title, 'Colours');
    assert.equal(data.items[0].emoji, CUSTOM);
    assert.equal(data.placeholder, 'Choose');
    for (const layout of ['BUTTONS', 'SELECT', 'REACTIONS']) {
      const items = layout === 'REACTIONS' ? [rrItem(ROLE1, { emoji: '✅' })] : [rrItem(ROLE1)];
      assert.equal(accept('reactionRoles', rrBody({ layout, items })).data.layout, layout);
    }
    for (const type of ['NORMAL', 'UNIQUE', 'VERIFY', 'REVERSE']) assert.equal(accept('reactionRoles', rrBody({ type })).data.type, type);
  });

  test('required and malformed fields are refused with a path', () => {
    refuse('reactionRoles', {}, create, { path: 'channelId' });
    refuse('reactionRoles', rrBody({ channelId: 'general' }), create, { path: 'channelId' });
    refuse('reactionRoles', rrBody({ layout: 'GRID' }), create, { path: 'layout' });
    refuse('reactionRoles', rrBody({ type: 'MAGIC' }), create, { path: 'type' });
    refuse('reactionRoles', rrBody({ title: '   ' }), create, { path: 'title' });
    refuse('reactionRoles', rrBody({ title: 'x'.repeat(257) }), create, { path: 'title' });
    refuse('reactionRoles', rrBody({ description: 'x'.repeat(4001) }), create, { path: 'description' });
    refuse('reactionRoles', rrBody({ messageContent: 'x'.repeat(2001) }), create, { path: 'messageContent' });
    refuse('reactionRoles', rrBody({ placeholder: 'x'.repeat(151) }), create, { path: 'placeholder' });
    refuse('reactionRoles', rrBody({ maxRoles: 0 }), create, { path: 'maxRoles' });
    refuse('reactionRoles', rrBody({ maxRoles: 26 }), create, { path: 'maxRoles' });
    refuse('reactionRoles', rrBody({ plainText: 'yes' }), create, { path: 'plainText' });
    refuse('reactionRoles', rrBody({ nope: 1 }), create, { path: 'nope' });
    assert.equal(accept('reactionRoles', rrBody({ maxRoles: 25 })).data.maxRoles, 25);
    assert.equal(accept('reactionRoles', rrBody({ description: '   ' })).data.description, null, 'blank clears the text');
  });

  test('the roles of a panel: label, emoji, colour and description limits', () => {
    refuse('reactionRoles', rrBody({ items: [{ ...rrItem(ROLE1), roleId: null }] }), create, { path: 'items.0.roleId' });
    refuse('reactionRoles', rrBody({ items: [rrItem(ROLE1, { label: 'x'.repeat(81) })] }), create, { path: 'items.0.label' });
    refuse('reactionRoles', rrBody({ items: [rrItem(ROLE1, { description: 'x'.repeat(101) })] }), create, { path: 'items.0.description' });
    refuse('reactionRoles', rrBody({ items: [rrItem(ROLE1, { style: 5 })] }), create, { path: 'items.0.style' });
    refuse('reactionRoles', rrBody({ items: [rrItem(ROLE1, { emoji: 'abc' })] }), create, { path: 'items.0.emoji' });
    refuse('reactionRoles', rrBody({ items: [rrItem(ROLE1, { emoji: '✅✅' })] }), create, { path: 'items.0.emoji' });
    // a missing child takes its default
    assert.deepEqual(accept('reactionRoles', rrBody({ items: [{ roleId: ROLE1 }] })).data.items[0], rrItem(ROLE1, { label: null }));
    assert.equal(accept('reactionRoles', rrBody({ items: [rrItem(ROLE1, { label: null })] })).data.items[0].label, null, 'empty label = the role name');
  });

  test('one entry per role, and the cap depends on the layout', () => {
    refuse('reactionRoles', rrBody({ items: [rrItem(ROLE1), rrItem(ROLE2), rrItem(ROLE1)] }), create, { path: 'items.2.roleId', message: /already on the panel/ });
    const many = (n, over = {}) => Array.from({ length: n }, (_, i) => rrItem(snow(100 + i), over));
    assert.equal(accept('reactionRoles', rrBody({ items: many(25) })).data.items.length, 25);
    assert.equal(accept('reactionRoles', rrBody({ layout: 'SELECT', items: many(25) })).data.items.length, 25);
    refuse('reactionRoles', rrBody({ items: many(26) }), create, { path: 'items' });
    const reactions = (n) => Array.from({ length: n }, (_, i) => rrItem(snow(100 + i), { emoji: ['😀', '😃', '😄', '😁', '😆', '😅', '😂', '🤣', '😊', '😇', '🙂', '😉', '😌', '😍', '🥰', '😘', '😗', '😙', '😚', '😋', '😛'][i] }));
    assert.equal(accept('reactionRoles', rrBody({ layout: 'REACTIONS', items: reactions(20) })).data.items.length, 20);
    refuse('reactionRoles', rrBody({ layout: 'REACTIONS', items: reactions(21) }), create, { path: 'items', message: /at most 20/ });
  });

  test('emoji-reaction panels need a distinct emoji on every role', () => {
    refuse('reactionRoles', rrBody({ layout: 'REACTIONS', items: [rrItem(ROLE1)] }), create, { path: 'items.0.emoji', message: /need an emoji/ });
    refuse('reactionRoles', rrBody({ layout: 'REACTIONS', items: [rrItem(ROLE1, { emoji: '✅' }), rrItem(ROLE2)] }), create, { path: 'items.1.emoji' });
    refuse('reactionRoles', rrBody({ layout: 'REACTIONS', items: [rrItem(ROLE1, { emoji: '✅' }), rrItem(ROLE2, { emoji: '✅' })] }), create, { path: 'items.1.emoji', message: /already used/ });
    // a variation selector does not make a different emoji
    refuse('reactionRoles', rrBody({ layout: 'REACTIONS', items: [rrItem(ROLE1, { emoji: '❤️' }), rrItem(ROLE2, { emoji: '❤' })] }), create, { path: 'items.1.emoji' });
    refuse('reactionRoles', rrBody({ layout: 'REACTIONS', items: [rrItem(ROLE1, { emoji: CUSTOM }), rrItem(ROLE2, { emoji: '<a:party:555555555555555555>' })] }), create, { path: 'items.1.emoji' });
    // buttons and dropdowns may repeat an emoji or have none
    accept('reactionRoles', rrBody({ items: [rrItem(ROLE1, { emoji: '✅' }), rrItem(ROLE2, { emoji: '✅' }), rrItem(ROLE3)] }));
  });

  test('updates: createOnly fields are locked, the layout of the panel decides the role rules', () => {
    const current = { layout: 'REACTIONS', attached: false, description: null, messageContent: null, plainText: false };
    refuse('reactionRoles', { channelId: CHAN2 }, update(current), { path: 'channelId', message: /cannot be changed after creation/ });
    refuse('reactionRoles', { layout: 'BUTTONS' }, update(current), { path: 'layout' });
    refuse('reactionRoles', { items: [rrItem(ROLE1)] }, update(current), { path: 'items.0.emoji' });
    assert.deepEqual(accept('reactionRoles', { items: [rrItem(ROLE1, { emoji: '✅' })] }, update(current)).data.items.length, 1);
    assert.deepEqual(accept('reactionRoles', { title: 'New' }, update(current)).data, { title: 'New' }, 'only the fields present');
    assert.deepEqual(accept('reactionRoles', {}, update(current)).data, {});
    // a button panel accepts any number of emoji-less roles
    accept('reactionRoles', { items: [rrItem(ROLE1), rrItem(ROLE2)] }, update({ ...current, layout: 'BUTTONS' }));
  });

  test('an attached panel never has its message text changed', () => {
    const attached = { layout: 'REACTIONS', attached: true, description: null, messageContent: null, plainText: false };
    accept('reactionRoles', { title: 'Renamed', type: 'UNIQUE', maxRoles: 2 }, update(attached));
    accept('reactionRoles', { description: null, plainText: false }, update(attached));
    refuse('reactionRoles', { description: 'new text' }, update(attached), { path: 'description', message: /never edits/ });
    refuse('reactionRoles', { messageContent: 'hi' }, update(attached), { path: 'messageContent' });
    refuse('reactionRoles', { plainText: true }, update(attached), { path: 'plainText' });
    accept('reactionRoles', { description: 'new text' }, update({ ...attached, attached: false }));
  });

  test('the repost action takes an optional channel', () => {
    const act = (data) => accept('reactionRoles', data, { mode: 'action', actionId: 'repost' });
    assert.deepEqual(act({}).data, { channelId: null });
    const { data, refs } = act({ channelId: CHAN2 });
    assert.equal(data.channelId, CHAN2);
    assert.deepEqual(refs.channels.map((c) => c.kinds), [['text', 'announcement']]);
    refuse('reactionRoles', { channelId: 'nope' }, { mode: 'action', actionId: 'repost' }, { path: 'channelId' });
    refuse('reactionRoles', { force: true }, { mode: 'action', actionId: 'repost' }, { path: 'force' });
    assert.throws(() => validateRecordInput('reactionRoles', {}, { mode: 'action', actionId: 'delete' }), RecordError);
  });
});

// ── Ticket panel requests ────────────────────────────────────────────────────

describe('ticketPanels requests', () => {
  test('a minimal create takes the defaults, including the default option', () => {
    const { data, refs } = accept('ticketPanels', { name: 'Support' });
    assert.equal(data.name, 'Support');
    assert.equal(data.style, 'BUTTONS');
    assert.equal(data.channelId, null);
    assert.equal(data.maxOpenPerUser, 1);
    assert.equal(data.channelNameFormat, 'ticket-{number}');
    assert.equal(data.askReason, false);
    assert.equal(data.dmTranscript, false);
    assert.deepEqual(data.managerRoleIds, []);
    assert.equal(data.options.length, 1);
    assert.equal(data.options[0].label, tkSvc.DEFAULT_OPTION.label);
    assert.equal(data.options[0].emoji, tkSvc.DEFAULT_OPTION.emoji);
    assert.deepEqual(refs, { channels: [], roles: [] });
  });

  test('the whole panel validates and every channel and role id is collected', () => {
    const { data, refs } = accept('ticketPanels', {
      name: '  Billing  ', style: 'SELECT', channelId: CHAN, managerRoleIds: [ROLE1, ROLE2, ROLE1],
      openCategoryId: CAT1, claimedCategoryId: CAT2, closedCategoryId: CAT3, transcriptChannelId: CHAN2, dmTranscript: true,
      maxOpenPerUser: 3, channelNameFormat: '{user.name}-{number}', askReason: true,
      options: [opt('Billing', { emoji: '💰', description: 'Payments', categoryId: CAT1, buttonStyle: 3, id: 'abc123' }), opt('Other')],
    });
    assert.equal(data.name, 'Billing');
    assert.deepEqual(data.managerRoleIds, [ROLE1, ROLE2], 'once each');
    assert.deepEqual(refs.channels.map((c) => c.id).sort(), [CHAN, CAT1, CAT2, CAT3, CHAN2, CAT1].sort());
    assert.deepEqual(refs.roles.map((r) => [r.id, r.assignable, r.allowEveryone]), [[ROLE1, false, false], [ROLE2, false, false], [ROLE1, false, false]]);
  });

  test('required and malformed fields are refused with a path', () => {
    refuse('ticketPanels', {}, create, { path: 'name' });
    refuse('ticketPanels', { name: '   ' }, create, { path: 'name' });
    refuse('ticketPanels', { name: 'x'.repeat(51) }, create, { path: 'name' });
    refuse('ticketPanels', { name: 'a\nb' }, create, { path: 'name' });
    refuse('ticketPanels', { name: 'A', style: 'GRID' }, create, { path: 'style' });
    refuse('ticketPanels', { name: 'A', maxOpenPerUser: 0 }, create, { path: 'maxOpenPerUser' });
    refuse('ticketPanels', { name: 'A', maxOpenPerUser: 11 }, create, { path: 'maxOpenPerUser' });
    refuse('ticketPanels', { name: 'A', askReason: 'no' }, create, { path: 'askReason' });
    refuse('ticketPanels', { name: 'A', openCategoryId: 'cat' }, create, { path: 'openCategoryId' });
    refuse('ticketPanels', { name: 'A', unknown: 1 }, create, { path: 'unknown' });
    assert.equal(accept('ticketPanels', { name: 'A', maxOpenPerUser: 10 }).data.maxOpenPerUser, 10);
  });

  test('the channel name format follows validateNameFormat', () => {
    for (const bad of ['{server}-{number}', '!!!', 'x'.repeat(91), '   ']) {
      refuse('ticketPanels', { name: 'A', channelNameFormat: bad }, create, { path: 'channelNameFormat' });
    }
    refuse('ticketPanels', { channelNameFormat: '{nope}' }, update(), { path: 'channelNameFormat', message: /Unknown placeholder/ });
    assert.equal(accept('ticketPanels', { name: 'A', channelNameFormat: '{user.display_name}-{topic}' }).data.channelNameFormat, '{user.display_name}-{topic}');
  });

  test('manager roles: at most 10, each once', () => {
    const roles = (n) => Array.from({ length: n }, (_, i) => snow(200 + i));
    assert.equal(accept('ticketPanels', { name: 'A', managerRoleIds: roles(10) }).data.managerRoleIds.length, 10);
    refuse('ticketPanels', { name: 'A', managerRoleIds: roles(11) }, create, { path: 'managerRoleIds' });
    refuse('ticketPanels', { name: 'A', managerRoleIds: ['everyone'] }, create, { path: 'managerRoleIds.0' });
  });

  test('options: 1 to 25, unique labels, limits per field, the hidden id', () => {
    refuse('ticketPanels', { name: 'A', options: [] }, create, { path: 'options', message: /at least 1 entry/ });
    const many = (n) => Array.from({ length: n }, (_, i) => opt(`Option ${i}`));
    assert.equal(accept('ticketPanels', { name: 'A', options: many(LIMITS.TICKET_PANEL_OPTIONS) }).data.options.length, LIMITS.TICKET_PANEL_OPTIONS);
    refuse('ticketPanels', { name: 'A', options: many(LIMITS.TICKET_PANEL_OPTIONS + 1) }, create, { path: 'options' });
    refuse('ticketPanels', { name: 'A', options: [opt('Help'), opt('help')] }, create, { path: 'options.1.label', message: /already has an option called/ });
    refuse('ticketPanels', { name: 'A', options: [opt('One', { id: 'x1' }), opt('Two', { id: 'x1' })] }, create, { path: 'options.1.label' });
    refuse('ticketPanels', { name: 'A', options: [opt('')] }, create, { path: 'options.0.label' });
    refuse('ticketPanels', { name: 'A', options: [opt('x'.repeat(81))] }, create, { path: 'options.0.label' });
    refuse('ticketPanels', { name: 'A', options: [opt('A', { description: 'x'.repeat(101) })] }, create, { path: 'options.0.description' });
    refuse('ticketPanels', { name: 'A', options: [opt('A', { buttonStyle: 5 })] }, create, { path: 'options.0.buttonStyle' });
    refuse('ticketPanels', { name: 'A', options: [opt('A', { emoji: 'no' })] }, create, { path: 'options.0.emoji' });
    refuse('ticketPanels', { name: 'A', options: [opt('A', { id: 'bad id' })] }, create, { path: 'options.0.id' });
    refuse('ticketPanels', { name: 'A', options: [opt('A', { categoryId: 'cat' })] }, create, { path: 'options.0.categoryId' });
    const ok = accept('ticketPanels', { name: 'A', options: [opt('A', { categoryId: CAT1 })] });
    assert.deepEqual(ok.refs.channels.map((c) => [c.id, c.kinds, c.path]), [[CAT1, ['category'], 'options.0.categoryId']]);
  });

  test('a published panel cannot be unpublished by clearing the channel', () => {
    refuse('ticketPanels', { channelId: null }, update({ channelId: CHAN }), { path: 'channelId', message: /cannot be unpublished/ });
    accept('ticketPanels', { channelId: null }, update({ channelId: null }));
    accept('ticketPanels', { channelId: CHAN2 }, update({ channelId: CHAN }));
  });

  test('updates take only the fields present; the actions validate their own fields', () => {
    assert.deepEqual(accept('ticketPanels', { askReason: true }, update()).data, { askReason: true });
    const publish = (data) => accept('ticketPanels', data, { mode: 'action', actionId: 'publish' });
    assert.equal(publish({ channelId: CHAN }).data.channelId, CHAN);
    refuse('ticketPanels', {}, { mode: 'action', actionId: 'publish' }, { path: 'channelId' });
    refuse('ticketPanels', { channelId: null }, { mode: 'action', actionId: 'publish' }, { path: 'channelId' });
    assert.deepEqual(accept('ticketPanels', {}, { mode: 'action', actionId: 'refresh' }).data, {});
    refuse('ticketPanels', { channelId: CHAN }, { mode: 'action', actionId: 'refresh' }, { path: 'channelId' });
  });

  test('template fields may be empty (the default message) and keep their placeholders', () => {
    const empty = accept('ticketPanels', { name: 'A', template: emptyTemplate(), introTemplate: emptyTemplate() });
    assert.deepEqual(empty.data.template, emptyTemplate());
    const intro = COLLECTIONS.ticketPanels.fields.find((f) => f.field === 'introTemplate');
    assert.deepEqual(intro.placeholders, ['{number}', '{reason}', '{topic}', '{panel}', '{managers}']);
    assert.equal(intro.requireContent, false);
    refuse('ticketPanels', { name: 'A', template: { content: 'x'.repeat(2001), embed: null, buttons: [] } }, create, { path: 'template' });
  });
});

// ── Pure helpers ─────────────────────────────────────────────────────────────

describe('reaction role helpers', () => {
  const row = (over = {}) => ({
    id: 'g1', guildId: snow(900), channelId: CHAN, messageId: snow(901), title: 'T', description: null, messageContent: null, plainText: false,
    maxRoles: null, type: 'NORMAL', layout: 'BUTTONS', attached: false, placeholder: null, messageMissingAt: null,
    items: [
      { id: 'i2', roleId: ROLE2, label: 'Two', emoji: null, style: 2, position: 1, description: null },
      { id: 'i1', roleId: ROLE1, label: 'One', emoji: '✅', style: 3, position: 0, description: 'First' },
    ],
    ...over,
  });

  test('panelItem sorts the roles by position and describes the state of the message', () => {
    const item = rrd.panelItem(row());
    assert.deepEqual(item.items.map((i) => i.roleId), [ROLE1, ROLE2]);
    assert.equal(item.status, 'Posted');
    assert.equal(item.summary, 'Buttons · Normal · 2 roles · Posted');
    assert.equal(item.messageUrl, `https://discord.com/channels/${snow(900)}/${CHAN}/${snow(901)}`);
    assert.equal(item.attached, false);
    assert.deepEqual(Object.keys(item.items[0]).sort(), ['description', 'emoji', 'label', 'roleId', 'style']);
    const unposted = rrd.panelItem(row({ messageId: rrd.UNPOSTED_MESSAGE_ID, messageMissingAt: new Date() }));
    assert.equal(unposted.status, 'Not posted yet');
    assert.equal(unposted.messageUrl, null);
    assert.equal(rrd.panelItem(row({ messageMissingAt: new Date() })).status, 'Message deleted');
    assert.equal(rrd.panelItem(row({ attached: true, layout: 'REACTIONS', items: [row().items[1]] })).status, 'Attached message');
    assert.match(rrd.panelItem(row({ items: [] })).summary, /0 roles/);
    assert.equal(rrd.panelItem(row({ items: [row().items[0]] })).items.length, 1);
    assert.equal(rrd.panelItem(row({ items: [{ ...row().items[0], style: 9 }] })).items[0].style, 1, 'a bad stored style reads as blue');
  });

  test('every wire field of the item is a field or readOnly descriptor (plus the summary and attached)', () => {
    const names = new Set([...COLLECTIONS.reactionRoles.fields, ...COLLECTIONS.reactionRoles.readOnly].map((f) => f.field));
    for (const key of Object.keys(rrd.panelItem(row()))) assert.ok(['id', 'summary', 'attached'].includes(key) || names.has(key), key);
  });

  test('diffPanelItems matches by role: updates keep the row, removals report their emojis', () => {
    const stored = [
      { id: 'a', roleId: ROLE1, label: 'One', emoji: '✅', style: 1, description: null, position: 0 },
      { id: 'b', roleId: ROLE2, label: 'Two', emoji: '🔔', style: 1, description: null, position: 1 },
      { id: 'c', roleId: ROLE3, label: 'Three', emoji: CUSTOM, style: 1, description: null, position: 2 },
    ];
    const same = stored.map(({ id, position, ...rest }) => rest);
    const nothing = rrd.diffPanelItems(stored, same);
    assert.equal(nothing.changed, false);

    // reorder + rename + a new role; Two is removed
    const next = [
      { ...same[2] }, { ...same[0], label: 'Uno' }, { roleId: snow(4), label: 'Four', emoji: '🎉', style: 1, description: null },
    ];
    const d = rrd.diffPanelItems(stored, next);
    assert.deepEqual(d.remove.map((r) => r.id), ['b']);
    assert.deepEqual(d.create.map((c) => [c.roleId, c.position]), [[snow(4), 2]]);
    assert.deepEqual(d.update.map((u) => [u.id, u.data]), [['c', { position: 0 }], ['a', { label: 'Uno', position: 1 }]]);
    assert.deepEqual(d.removedEmojis, ['🔔']);
    assert.equal(d.changed, true);

    // a changed emoji takes the old reaction off; an emoji that moved to another role stays
    const swap = rrd.diffPanelItems(stored, [{ ...same[0], emoji: '🎮' }, { ...same[1], emoji: '✅' }, same[2]]);
    assert.deepEqual(swap.removedEmojis, ['🔔'], 'the old ✅ is still used by Two; only the abandoned 🔔 goes');
    const gone = rrd.diffPanelItems(stored, [{ ...same[0], emoji: '🎮' }, same[1], same[2]]);
    assert.deepEqual(gone.removedEmojis, ['✅']);
  });

  test('normalizeItems: label, colour, description and emoji as /reactionrole add stores them', () => {
    const resources = {
      roles: [{ id: ROLE1, name: 'News' }, { id: ROLE2, name: 'Events' }],
      emojis: [{ id: '555555555555555555', name: 'party', animated: true }],
    };
    const items = [
      rrItem(ROLE1, { label: null, emoji: null, style: 3, description: ' Updates ' }),
      rrItem(ROLE2, { label: '  Mine ', emoji: '<:old:555555555555555555>', style: 4, description: 'x'.repeat(100) }),
    ];
    const buttons = rrd.normalizeItems(items, 'BUTTONS', resources);
    assert.deepEqual(buttons[0], { roleId: ROLE1, label: 'News', emoji: null, style: 3, description: null });
    assert.equal(buttons[1].emoji, '<a:party:555555555555555555>', 'the server\'s name and animation win');
    assert.equal(buttons[1].label, 'Mine');
    const select = rrd.normalizeItems(items, 'SELECT', resources);
    assert.deepEqual(select.map((i) => [i.style, i.description?.length]), [[1, 7], [1, 100]]);
    assert.equal(select[0].description, 'Updates');
    // a custom emoji from elsewhere is refused unless the panel already holds it
    const foreign = [rrItem(ROLE1, { emoji: '<:other:999999999999999999>' })];
    assert.throws(() => rrd.normalizeItems(foreign, 'BUTTONS', resources), (e) => e instanceof RecordError && e.path === 'items.0.emoji');
    assert.equal(rrd.normalizeItems(foreign, 'BUTTONS', resources, new Set(['999999999999999999']))[0].emoji, '<:other:999999999999999999>');
    // the per-layout rules run on the result
    assert.throws(() => rrd.normalizeItems([rrItem(ROLE1)], 'REACTIONS', resources), (e) => e instanceof RecordError && e.path === 'items.0.emoji');
    assert.equal(rrd.normalizeItems([rrItem(snow(999), { label: null })], 'BUTTONS', resources)[0].label, 'Role', 'an unknown role reads as "Role"');
  });
});

describe('ticket panel helpers', () => {
  const optRow = (over = {}) => ({
    id: 'o1', panelId: 'p1', label: 'Help', emoji: '✉️', description: null, buttonStyle: 1, position: 0, introTemplate: null, categoryId: null, ...over,
  });
  const panelRow = (over = {}) => ({
    id: 'p1', guildId: snow(900), name: 'Support', channelId: null, messageId: null, template: null, style: 'BUTTONS', managerRoleIds: [ROLE1],
    openCategoryId: null, claimedCategoryId: null, closedCategoryId: null, introTemplate: null, transcriptChannelId: null, dmTranscript: false,
    maxOpenPerUser: 1, channelNameFormat: 'ticket-{number}', askReason: false, options: [optRow({ id: 'o2', label: 'Two', position: 1 }), optRow()], ...over,
  });

  test('panelItem shows the effective messages and orders the options', () => {
    const item = tpd.panelItem(panelRow(), 4);
    assert.deepEqual(item.options.map((o) => o.label), ['Help', 'Two']);
    assert.equal(item.options[0].id, 'o1');
    assert.deepEqual(item.options[0].introTemplate, emptyTemplate(), 'no override reads as an empty message');
    assert.equal(item.template.embed.title, 'How can we help?');
    assert.equal(item.introTemplate.embed.title, 'Ticket #{number}');
    assert.equal(item.status, tpd.STATUS.DRAFT);
    assert.equal(item.messageUrl, null);
    assert.equal(item.openTickets, 4);
    assert.equal(item.summary, 'Buttons · 2 options · Not published');
    const pub = tpd.panelItem(panelRow({ style: 'SELECT', channelId: CHAN, messageId: snow(5), template: { content: 'Hi', embed: null, buttons: [] } }));
    assert.equal(pub.status, 'Published');
    assert.equal(pub.messageUrl, `https://discord.com/channels/${snow(900)}/${CHAN}/${snow(5)}`);
    assert.equal(pub.template.content, 'Hi');
    assert.match(pub.summary, /^Dropdown/);
    assert.equal(tpd.panelItem(panelRow({ style: 'SELECT' })).template.embed.description, 'Choose a topic below to open a private ticket with our team.');
    for (const key of Object.keys(item)) {
      assert.ok(key === 'id' || key === 'summary' || COLLECTIONS.ticketPanels.fields.concat(COLLECTIONS.ticketPanels.readOnly).some((f) => f.field === key), key);
    }
    assert.deepEqual(Object.keys(item.options[0]).sort(), COLLECTIONS.ticketPanels.fields.find((f) => f.field === 'options').itemFields.map((f) => f.field).sort());
  });

  test('normalizeOptions: intros, descriptions and emojis as /ticket-panel option add stores them', () => {
    const resources = { emojis: [{ id: '555555555555555555', name: 'party', animated: false }] };
    const out = tpd.normalizeOptions([
      opt('A', { description: ' Pay ', emoji: '<:x:555555555555555555>', introTemplate: { content: 'Hi {user}', embed: null, buttons: [] } }),
      opt('B', { description: '   ', buttonStyle: 4, categoryId: CAT1 }),
    ], resources);
    assert.equal(out[0].emoji, '<:party:555555555555555555>');
    assert.equal(out[0].description, 'Pay');
    assert.equal(out[0].introTemplate.content, 'Hi {user}');
    assert.equal(out[1].description, null);
    assert.equal(out[1].introTemplate, null, 'an empty intro means "use the panel\'s"');
    assert.equal(out[1].buttonStyle, 4);
    assert.throws(() => tpd.normalizeOptions([opt('A', { emoji: '<:z:999999999999999999>' })], resources), (e) => e instanceof RecordError && e.path === 'options.0.emoji');
    assert.equal(tpd.normalizeOptions([opt('A', { emoji: '<:z:999999999999999999>' })], resources, new Set(['999999999999999999']))[0].emoji, '<:z:999999999999999999>');
  });

  test('diffOptions keeps a row by id, then by label, and the order is the position', () => {
    const stored = [optRow({ id: 'a', label: 'Alpha', position: 0 }), optRow({ id: 'b', label: 'Beta', position: 1, description: 'old' }), optRow({ id: 'c', label: 'Gamma', position: 2 })];
    const n = (label, over = {}) => ({ id: null, label, emoji: '✉️', description: null, buttonStyle: 1, categoryId: null, introTemplate: null, ...over });
    // nothing changed
    assert.equal(tpd.diffOptions(stored, [n('Alpha'), n('Beta', { description: 'old' }), n('Gamma')]).changed, false);
    // rename Alpha by id, move Gamma first, drop Beta, add Delta
    const d = tpd.diffOptions(stored, [n('Gamma', { id: 'c' }), n('Alpha 2', { id: 'a' }), n('Delta')]);
    assert.deepEqual(d.remove.map((r) => r.id), ['b']);
    assert.deepEqual(d.create.map((c) => [c.label, c.position]), [['Delta', 2]]);
    assert.deepEqual(d.update.map((u) => [u.id, u.data]), [['c', { position: 0 }], ['a', { label: 'Alpha 2', position: 1 }]]);
    // without ids, the label finds the row (case-insensitively)
    const byLabel = tpd.diffOptions(stored, [n('beta'), n('ALPHA'), n('Gamma')]);
    assert.deepEqual(byLabel.remove, []);
    assert.deepEqual(byLabel.create, []);
    assert.deepEqual(byLabel.update.map((u) => u.id).sort(), ['a', 'b']);
    // an unknown id is a new option
    assert.deepEqual(tpd.diffOptions(stored, [n('Alpha'), n('Zed', { id: 'ghost' }), n('Gamma')]).create.map((c) => c.label), ['Zed']);
    // an intro override is compared as a message
    const intro = { content: 'Hi', embed: null, buttons: [] };
    assert.deepEqual(tpd.diffOptions(stored, [n('Alpha', { introTemplate: intro }), n('Beta', { description: 'old' }), n('Gamma')]).update, [{ id: 'a', data: { introTemplate: intro } }]);
    // a swap of two labels is two updates, not a clash
    const swap = tpd.diffOptions(stored.slice(0, 2), [n('Beta', { id: 'a', description: 'old' }), n('Alpha', { id: 'b' })]);
    assert.equal(swap.update.length, 2);
    assert.equal(swap.create.length + swap.remove.length, 0);
  });

  test('isDefaultTemplate recognises the built-in messages', () => {
    assert.equal(tpd.isDefaultTemplate(tpd.defaultPanelTemplate('BUTTONS'), [tpd.defaultPanelTemplate('BUTTONS'), tpd.defaultPanelTemplate('SELECT')]), true);
    assert.equal(tpd.isDefaultTemplate(tpd.defaultPanelTemplate('SELECT'), [tpd.defaultPanelTemplate('BUTTONS'), tpd.defaultPanelTemplate('SELECT')]), true);
    assert.equal(tpd.isDefaultTemplate({ content: 'x', embed: null, buttons: [] }, [tpd.defaultPanelTemplate('BUTTONS')]), false);
    assert.equal(tpd.isDefaultTemplate(tpd.DEFAULT_INTRO_TEMPLATE, [tpd.DEFAULT_INTRO_TEMPLATE]), true);
  });
});

// ── Tasks: guards that need no database ──────────────────────────────────────

describe('tasks', () => {
  const GUILD = snow(700);
  const OWNER = snow(701);
  const member = (id, { admin = false, manage = false } = {}) => {
    const guild = { id: GUILD, ownerId: OWNER };
    return {
      id, guild, roles: { cache: new Collection() },
      permissions: { has: (bit) => (admin && bit === PermissionFlagsBits.Administrator) || (manage && bit === PermissionFlagsBits.ManageGuild) },
    };
  };
  const guild = { id: GUILD, ownerId: OWNER, channels: { cache: new Collection(), fetch: async () => { throw new Error('unexpected'); } } };
  const call = (tasks, kind, actor, payload) => tasks[kind].run({ guild, client: {}, actor, payload, taskId: 't1' });
  const kinds = [[rrTasks, ['rr.publish', 'rr.repost', 'rr.delete', 'rr.roleTemplate']], [tkTasks, ['ticket.check', 'ticket.publish', 'ticket.refresh', 'ticket.delete']]];

  test('the kinds are well formed and owned by their module', () => {
    for (const [tasks, expected] of kinds) {
      assert.deepEqual(Object.keys(tasks).sort(), [...expected].sort());
      for (const [kind, task] of Object.entries(tasks)) {
        assert.match(kind, TASK_KIND_RE, kind);
        assert.equal(typeof task.run, 'function', kind);
      }
    }
  });

  test('every task refuses an actor who left the server', async () => {
    for (const [tasks, list] of kinds) {
      for (const kind of list) {
        await assert.rejects(call(tasks, kind, { id: snow(5), member: null, isBotOwner: false }, { groupId: 'g', panelId: 'p' }),
          (err) => err instanceof TaskError && /no longer a member/.test(err.message), kind);
      }
    }
  });

  test('every task refuses a member below the Admin level', async () => {
    for (const [tasks, list] of kinds) {
      for (const kind of list) {
        for (const m of [member(snow(5)), member(snow(6), { admin: false, manage: false })]) {
          await assert.rejects(call(tasks, kind, { id: m.id, member: m, isBotOwner: false }, { groupId: 'g', panelId: 'p' }),
            (err) => err instanceof TaskError && /Admin level/.test(err.message), kind);
        }
      }
    }
  });

  test('a payload that is not an id is refused before anything is looked up', async () => {
    const admin = member(snow(7), { manage: true });
    const actor = { id: admin.id, member: admin, isBotOwner: false };
    const bad = [undefined, {}, { groupId: 5 }, { groupId: '../x' }, { groupId: 'x'.repeat(41) }, { panelId: '' }];
    for (const payload of bad) {
      for (const kind of ['rr.publish', 'rr.repost', 'rr.delete']) {
        await assert.rejects(call(rrTasks, kind, actor, { mode: 'sync', ...payload }), (err) => err instanceof TaskError && /not valid|not valid\./.test(err.message), `${kind} ${JSON.stringify(payload)}`);
      }
      for (const kind of ['ticket.publish', 'ticket.refresh', 'ticket.delete']) {
        await assert.rejects(call(tkTasks, kind, actor, payload), (err) => err instanceof TaskError && /not valid/.test(err.message), `${kind} ${JSON.stringify(payload)}`);
      }
    }
    await assert.rejects(call(rrTasks, 'rr.publish', actor, { groupId: 'g1', mode: 'explode' }), (err) => err instanceof TaskError && /not valid/.test(err.message));
    for (const template of [undefined, 'verify', 'timezones-mars', 5]) {
      await assert.rejects(call(rrTasks, 'rr.roleTemplate', actor, { groupId: 'g1', template }),
        (err) => err instanceof TaskError && /not valid/.test(err.message), String(template));
    }
    await assert.rejects(call(tkTasks, 'ticket.check', actor, { categoryIds: ['nope'] }), (err) => err instanceof TaskError);
    await assert.rejects(call(tkTasks, 'ticket.check', actor, { categoryIds: [], transcriptChannelId: 5 }), (err) => err instanceof TaskError);
  });
});

describe('ticket.check (no database needed)', () => {
  const GUILD = snow(710);
  const admin = { id: snow(711), guild: { id: GUILD, ownerId: snow(712) }, roles: { cache: new Collection() }, permissions: { has: (b) => b === PermissionFlagsBits.ManageGuild } };
  const actor = { id: admin.id, member: admin, isBotOwner: false };
  const channel = (id, name, type, deny = []) => {
    const c = { id, name, type, deny, isTextBased: () => type === ChannelType.GuildText || type === ChannelType.GuildAnnouncement, isThread: () => false };
    c.permissionsFor = () => ({ has: (bit) => !c.deny.some((k) => PermissionFlagsBits[k] === bit) });
    return c;
  };
  const me = { id: snow(713), permissions: { has: () => true } };
  const guild = { id: GUILD, available: true, members: { me }, channels: { cache: new Collection(), fetch: async () => { throw Object.assign(new Error('x'), { code: 10003 }); } } };
  const add = (id, name, type, deny) => { const c = channel(id, name, type, deny); c.guild = guild; guild.channels.cache.set(id, c); return c; };
  const run = (payload) => tkTasks['ticket.check'].run({ guild, client: {}, actor, payload, taskId: 't' });
  const refused = (payload, re) => assert.rejects(run(payload), (err) => err instanceof TaskError && re.test(err.message), JSON.stringify(payload));

  test('categories need View Channel, Manage Channels and Manage Roles; the transcript channel needs four permissions', async () => {
    const fine = add(snow(720), 'Tickets', ChannelType.GuildCategory);
    const noRoles = add(snow(721), 'Locked', ChannelType.GuildCategory, ['ManageRoles']);
    const text = add(snow(722), 'general', ChannelType.GuildText);
    const voice = add(snow(723), 'Lounge', ChannelType.GuildVoice);
    const transcripts = add(snow(724), 'transcripts', ChannelType.GuildText);
    const noFiles = add(snow(725), 'no-files', ChannelType.GuildText, ['AttachFiles']);
    const news = add(snow(726), 'news', ChannelType.GuildAnnouncement);

    assert.equal((await run({ categoryIds: [fine.id], transcriptChannelId: transcripts.id })).message, 'The bot can use those channels.');
    assert.ok(await run({ categoryIds: [] }), 'nothing to check is fine');
    assert.ok(await run({ categoryIds: [fine.id, fine.id], transcriptChannelId: news.id }), 'an announcement channel is a fine transcript channel');
    await refused({ categoryIds: [noRoles.id] }, /I need Manage Roles in the "Locked" category/);
    await refused({ categoryIds: [text.id] }, /#general is not a category/);
    await refused({ categoryIds: [snow(799)] }, /does not exist in this server any more/);
    await refused({ categoryIds: [fine.id], transcriptChannelId: noFiles.id }, /I need Attach Files in #no-files/);
    await refused({ categoryIds: [], transcriptChannelId: voice.id }, /text or announcement/);
    await refused({ categoryIds: ['not-a-channel'] }, /not valid/);
    await refused({ categoryIds: [], transcriptChannelId: 12345 }, /not valid/);
  });

  test('a payload without a category list checks nothing, and only the first few categories are looked at', async () => {
    assert.ok(await run({}));
    assert.ok(await run({ categoryIds: 'nope' }));
    const many = Array.from({ length: 200 }, (_, i) => snow(800 + i));
    // the 29th id and beyond are never looked up (the first 28 are unknown channels: the first one already refuses)
    await refused({ categoryIds: many }, /does not exist/);
  });
});

// ── Stores and tasks against a real database (opt-in) ────────────────────────

describe('stores and tasks (TEST_DATABASE_URL)', { skip: skipDb }, () => {
  let rrStore;
  let tkStore;
  let settingsSvc;
  let n = 0;
  let nid = 0;
  const BOT = snow(999);
  const ADMIN = snow(998);
  const OWNER = snow(997);
  const discordErr = (code) => Object.assign(new Error(`Discord ${code}`), { code });
  const newId = () => `6${String(++nid).padStart(17, '0')}`;

  /** A fresh guild id (and its GuildSettings row) per test. */
  async function newGuild() {
    const id = `93${String(Date.now()).slice(-9)}${String(++n).padStart(7, '0')}`;
    guilds.push(id);
    await settingsSvc.ensureGuildSettings(id);
    return id;
  }

  const fakeReaction = (message, stored) => {
    const m = /^<a?:(\w+):(\d+)>$/.exec(stored);
    const emoji = m ? { id: m[2], name: m[1] } : { id: null, name: stored };
    const reaction = {
      emoji, me: true, removedUsers: [], removedAll: false,
      users: { remove: async (uid) => { reaction.removedUsers.push(uid); } },
      remove: async () => { reaction.removedAll = true; message.reactions.cache.delete(emoji.id ?? emoji.name); },
    };
    return reaction;
  };

  function makeMessage(channel, payload = {}, id = newId()) {
    const message = {
      id, channelId: channel.id, channel, guild: channel.guild, guildId: channel.guild.id, payload, edits: [], reacted: [], deleted: false,
      url: `https://discord.com/channels/${channel.guild.id}/${channel.id}/${id}`, reactions: { cache: new Collection() }, editError: null,
      async edit(p) {
        if (message.editError) throw message.editError;
        message.edits.push(p);
        message.payload = { ...message.payload, ...p };
      },
      async react(e) {
        message.reacted.push(e);
        const r = fakeReaction(message, e);
        message.reactions.cache.set(r.emoji.id ?? r.emoji.name, r);
      },
      async delete() {
        message.deleted = true;
        channel.store.delete(id);
      },
    };
    channel.store.set(id, message);
    return message;
  }

  function makeChannel(guild, { name = 'chan', type = ChannelType.GuildText, deny = [] } = {}) {
    const channel = {
      id: newId(), name, type, guild, deny, store: new Map(), sent: [], sendError: null, parentId: null, calls: [],
      isTextBased: () => type === ChannelType.GuildText || type === ChannelType.GuildAnnouncement,
      isThread: () => false,
      permissionsFor: () => ({ has: (bit) => !channel.deny.some((k) => PermissionFlagsBits[k] === bit) }),
      messages: {
        fetch: async (mid) => {
          const m = channel.store.get(mid);
          if (!m) throw discordErr(10008);
          return m;
        },
        delete: async (mid) => { channel.calls.push(['messages.delete', mid]); channel.store.delete(mid); },
      },
      permissionOverwrites: {
        edit: async (who, perms) => { channel.calls.push(['overwrite.edit', who, perms]); },
        delete: async (who) => { channel.calls.push(['overwrite.delete', who]); },
      },
      toString: () => `<#${channel.id}>`,
    };
    channel.send = async (payload) => {
      if (channel.sendError) throw channel.sendError;
      const m = makeMessage(channel, payload);
      channel.sent.push(m);
      return m;
    };
    guild.channels.cache.set(channel.id, channel);
    return channel;
  }

  /** A fake server: roles, channels, emojis, and the dashboard-side helpers built on them. */
  function makeWorld(guildId) {
    const guild = { id: guildId, name: 'Test Guild', ownerId: OWNER, available: true, memberCount: 5, premiumTier: 0 };
    guild.client = { user: { id: BOT }, emojis: { cache: new Collection() } };
    guild.members = { me: { id: BOT, permissions: { has: () => true }, roles: { highest: { position: 50 } } }, cache: new Collection() };
    guild.roles = { cache: new Collection(), fetch: async (id) => guild.roles.cache.get(id) ?? null };
    guild.channels = {
      cache: new Collection(),
      fetch: async (id) => {
        const c = guild.channels.cache.get(id);
        if (!c) throw discordErr(10003);
        return c;
      },
    };
    guild.emojis = { cache: new Collection() };
    const roles = [];
    const emojis = [];
    const adminMember = {
      id: ADMIN, guild, roles: { cache: new Collection() },
      permissions: { has: (bit) => bit === PermissionFlagsBits.ManageGuild },
    };
    const world = {
      id: guildId, guild, client: guild.client, runs: [], taskError: null, roles, emojis,
      role: (name, { mentionable = false, position = 5 } = {}) => {
        const role = { id: newId(), name, mentionable, position, managed: false, everyone: false, assignable: true, dangerous: false };
        roles.push(role);
        guild.roles.cache.set(role.id, { ...role, editable: true, permissions: { has: () => false } });
        return role;
      },
      emoji: (name, animated = false) => {
        const id = newId();
        emojis.push({ id, name, animated });
        guild.client.emojis.cache.set(id, { id, name, available: true, roles: { cache: new Collection() }, toString: () => `<${animated ? 'a' : ''}:${name}:${id}>` });
        return { id, name, text: `<${animated ? 'a' : ''}:${name}:${id}>` };
      },
      text: (name = 'text', over = {}) => makeChannel(guild, { name, ...over }),
      category: (name = 'Cat', over = {}) => makeChannel(guild, { name, type: ChannelType.GuildCategory, ...over }),
      actor: (over = {}) => ({ id: ADMIN, member: adminMember, isBotOwner: false, ...over }),
      resources() {
        return {
          guild: { id: guildId, name: guild.name, ownerId: OWNER },
          channels: [...guild.channels.cache.values()].map((c) => ({ id: c.id, name: c.name, type: c.type })),
          roles: [{ id: guildId, name: '@everyone', everyone: true, mentionable: false, assignable: false, managed: false }, ...roles],
          emojis, bot: {},
        };
      },
      /** The context a record store gets; `runTask` does what the dashboard's task runner and the bot's job handler do. */
      ctx(over = {}) {
        const ctx = {
          guildId, userId: ADMIN, user: { id: ADMIN, username: 'admin' }, member: { owner: false, permissions: String(PermissionFlagsBits.ManageGuild) },
          logger: { info() {}, warn() {}, error() {}, debug() {} },
          discord: { getGuildResources: async () => world.resources() },
          runTask: async (kind, payload) => {
            world.runs.push([kind, payload]);
            const task = (kind.startsWith('rr.') ? rrTasks : tkTasks)[kind];
            try {
              const r = await task.run({ guild, client: guild.client, actor: world.actor(), payload, taskId: `t${world.runs.length}` });
              return { status: 'done', message: r?.message ?? 'Done.', data: r?.data ?? null };
            } catch (err) {
              if (err instanceof TaskError) return { status: 'failed', message: err.message, data: null };
              throw err;
            }
          },
          ...over,
        };
        return ctx;
      },
    };
    return world;
  }

  const code = async (promise) => {
    try {
      await promise;
    } catch (err) {
      assert.ok(err instanceof RecordError, `expected a RecordError, got ${err?.stack ?? err}`);
      return err;
    }
    return null;
  };

  /** What the records API hands a store: the request validated against the descriptors. */
  const valid = (key, data, opts = create) => validateRecordInput(key, data, opts).data;

  before(async () => {
    settingsSvc = await import('../src/services/settingsService.js');
    // Creates and updates may carry the bot's note (withMessage); these tests look at the item.
    const items = (store) => ({
      ...store,
      create: async (...args) => unwrapResult(await store.create(...args)).item,
      update: async (...args) => unwrapResult(await store.update(...args)).item,
    });
    rrStore = items((await import('../src/dashboard/collections/reactionRoles.store.js')).store);
    tkStore = items((await import('../src/dashboard/collections/ticketPanels.store.js')).store);
  });

  test('every store has the full interface and its raw statements run', async () => {
    for (const store of [rrStore, tkStore]) {
      for (const fn of ['list', 'get', 'create', 'update', 'remove']) assert.equal(typeof store[fn], 'function', fn);
    }
    assert.deepEqual(Object.keys(rrStore.actions), ['repost']);
    assert.deepEqual(Object.keys(tkStore.actions).sort(), ['publish', 'refresh']);
    const rrSql = await import('../src/dashboard/collections/reactionRoles.store.js');
    const tkSql = await import('../src/dashboard/collections/ticketPanels.store.js');
    for (const { name, build } of [...rrSql.RAW_SQL, ...tkSql.RAW_SQL]) {
      await prisma.$transaction(async (tx) => { await tx.$queryRaw(build()); }).catch((err) => assert.fail(`${name}: ${err.message}`));
    }
  });

  // ── Reaction roles ─────────────────────────────────────────────────────────

  describe('reaction role panels', () => {
    const rowOf = (id) => prisma.reactionRoleGroup.findUnique({ where: { id }, include: { items: { orderBy: { position: 'asc' } } } });

    test('create saves the panel, then the bot posts it with one button per role', async () => {
      const w = makeWorld(await newGuild());
      const news = w.role('News');
      const events = w.role('Events');
      const ch = w.text('roles');
      const item = await rrStore.create(w.id, valid('reactionRoles', rrBody({
        channelId: ch.id, title: 'Pick roles', description: 'Choose', type: 'UNIQUE', maxRoles: 3,
        items: [rrItem(news.id, { label: 'News', emoji: '📰', style: 3 }), rrItem(events.id, { label: null })],
      })), w.ctx());

      assert.deepEqual(w.runs.map((r) => r[0]), ['rr.publish']);
      assert.equal(item.status, 'Posted');
      assert.equal(ch.sent.length, 1);
      const posted = ch.sent[0];
      assert.equal(item.messageUrl, posted.url);
      const buttons = posted.payload.components[0].toJSON().components;
      assert.deepEqual(buttons.map((b) => b.custom_id), [`rr:${item.id}:${news.id}`, `rr:${item.id}:${events.id}`]);
      assert.deepEqual(buttons.map((b) => b.label), ['News', 'Events'], 'the empty label is the role name');
      assert.equal(buttons[0].style, 3);
      assert.equal(posted.payload.embeds[0].data.title, 'Pick roles');
      assert.deepEqual(posted.payload.allowedMentions, { parse: [] });

      const row = await rowOf(item.id);
      assert.equal(row.messageId, posted.id);
      assert.equal(row.messageMissingAt, null);
      assert.equal(row.attached, false);
      assert.equal(row.maxRoles, null, 'the role limit only exists in Normal mode');
      assert.deepEqual(row.items.map((i) => [i.roleId, i.position, i.customId]), [[news.id, 0, `rr:${item.id}:${news.id}`], [events.id, 1, `rr:${item.id}:${events.id}`]]);
      assert.deepEqual(await rrStore.list(w.id), [item]);
      assert.deepEqual(await rrStore.get(w.id, item.id), item);
    });

    test('an emoji-reaction panel gets the bot\'s reactions in order', async () => {
      const w = makeWorld(await newGuild());
      const a = w.role('A');
      const b = w.role('B');
      const party = w.emoji('party');
      const ch = w.text('roles');
      const item = await rrStore.create(w.id, valid('reactionRoles', rrBody({
        channelId: ch.id, layout: 'REACTIONS', items: [rrItem(a.id, { label: 'A', emoji: '✅' }), rrItem(b.id, { label: 'B', emoji: party.text })],
      })), w.ctx());
      const msg = ch.sent[0];
      assert.deepEqual(msg.reacted, ['✅', party.text]);
      assert.equal(msg.payload.components.length, 0);
      assert.match(msg.payload.embeds[0].data.description, /✅ — \*\*A\*\*/);
      assert.equal(item.items[1].emoji, party.text);
    });

    test('a panel the bot cannot post stays "Not posted yet"; Repost posts it once the problem is fixed', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('News');
      const ch = w.text('roles', { deny: ['EmbedLinks'] });
      const err = await code(rrStore.create(w.id, valid('reactionRoles', rrBody({ channelId: ch.id, items: [rrItem(role.id, { label: 'News' })] })), w.ctx()));
      assert.equal(err.code, 'failed');
      assert.match(err.message, /was saved, but it was not posted/);
      assert.match(err.message, /I need Embed Links in #roles/);
      assert.equal(ch.sent.length, 0);

      const [pending] = await rrStore.list(w.id);
      assert.equal(pending.status, 'Not posted yet');
      assert.equal(pending.messageUrl, null);
      const row = await rowOf(pending.id);
      assert.equal(row.messageId, rrd.UNPOSTED_MESSAGE_ID);
      assert.ok(row.messageMissingAt, '/reactionrole list shows it as "message deleted — run repost"');

      ch.deny = [];
      const out = await rrStore.actions.repost(w.id, pending.id, { channelId: null }, w.ctx());
      assert.equal(out.item.status, 'Posted');
      assert.match(out.message, /is now in #roles/);
      assert.equal(ch.sent.length, 1);
      const fixed = await rowOf(pending.id);
      assert.equal(fixed.messageId, ch.sent[0].id);
      assert.equal(fixed.messageMissingAt, null);
    });

    test('saving an unposted panel again posts it (a retry without the action)', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('News');
      const ch = w.text('roles', { deny: ['SendMessages'] });
      await code(rrStore.create(w.id, valid('reactionRoles', rrBody({ channelId: ch.id, items: [rrItem(role.id)] })), w.ctx()));
      const [pending] = await rrStore.list(w.id);
      ch.deny = [];
      const item = await rrStore.update(w.id, pending.id, valid('reactionRoles', { title: 'Renamed' }, update(pending)), w.ctx());
      assert.equal(item.status, 'Posted');
      assert.equal(ch.sent.length, 1);
      assert.equal(ch.sent[0].payload.embeds[0].data.title, 'Renamed');
    });

    test('editing a panel edits its message; roles are matched by role so rows and buttons keep their ids', async () => {
      const w = makeWorld(await newGuild());
      const [a, b, c] = ['A', 'B', 'C'].map((name) => w.role(name));
      const ch = w.text('roles');
      const made = await rrStore.create(w.id, valid('reactionRoles', rrBody({
        channelId: ch.id, items: [rrItem(a.id, { label: 'A' }), rrItem(b.id, { label: 'B' })],
      })), w.ctx());
      const before = await rowOf(made.id);
      const msg = ch.sent[0];
      w.runs.length = 0;

      // new title and mode: one sync
      let item = await rrStore.update(w.id, made.id, valid('reactionRoles', { title: 'New title', type: 'VERIFY', maxRoles: 2 }, update(made)), w.ctx());
      assert.deepEqual(w.runs.map((r) => r[0]), ['rr.publish']);
      assert.equal(msg.edits.length, 1);
      assert.equal(msg.edits[0].embeds[0].data.title, 'New title');
      assert.match(msg.edits[0].embeds[0].data.footer.text, /can't be removed here/);
      assert.equal(item.type, 'VERIFY');
      assert.equal((await rowOf(made.id)).maxRoles, null);

      // reorder, rename one, drop B, add C
      w.runs.length = 0;
      item = await rrStore.update(w.id, made.id, valid('reactionRoles', {
        items: [rrItem(c.id, { label: 'C', style: 4 }), rrItem(a.id, { label: 'A renamed', emoji: '✅' })],
      }, update(item)), w.ctx());
      const after2 = await rowOf(made.id);
      assert.deepEqual(after2.items.map((i) => [i.roleId, i.label, i.position]), [[c.id, 'C', 0], [a.id, 'A renamed', 1]]);
      assert.equal(after2.items.find((i) => i.roleId === a.id).id, before.items.find((i) => i.roleId === a.id).id, 'the row of A stays');
      assert.equal(msg.edits.length, 2);
      const buttons = msg.edits[1].components[0].toJSON().components;
      assert.deepEqual(buttons.map((x) => [x.custom_id, x.label]), [[`rr:${made.id}:${c.id}`, 'C'], [`rr:${made.id}:${a.id}`, 'A renamed']]);
      assert.equal(buttons[0].style, 4);
      assert.deepEqual(item.items.map((i) => i.roleId), [c.id, a.id]);
    });

    test('a save that changes nothing does not wake the bot, and the form a store returns is accepted back', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('News');
      const ch = w.text('roles');
      const made = await rrStore.create(w.id, valid('reactionRoles', rrBody({
        channelId: ch.id, layout: 'SELECT', placeholder: 'Pick', items: [rrItem(role.id, { label: 'News', description: 'Updates', emoji: '📰' })],
      })), w.ctx());
      w.runs.length = 0;
      const { id, summary, status, attached, messageUrl, channelId, layout, ...form } = made;
      const data = valid('reactionRoles', form, update(made));
      assert.deepEqual(Object.keys(data).sort(), Object.keys(form).sort());
      const again = await rrStore.update(w.id, made.id, data, w.ctx());
      assert.deepEqual(again, made);
      assert.equal(w.runs.length, 0);
      assert.equal(ch.sent[0].edits.length, 0);
    });

    test('a dropdown panel stores the placeholder and descriptions; other layouts drop what they do not use', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('News');
      const ch = w.text('roles');
      const select = await rrStore.create(w.id, valid('reactionRoles', rrBody({
        channelId: ch.id, layout: 'SELECT', placeholder: ' Choose one ', items: [rrItem(role.id, { label: 'News', description: 'Updates', style: 4 })],
      })), w.ctx());
      assert.equal(select.placeholder, 'Choose one');
      assert.equal(select.items[0].description, 'Updates');
      assert.equal(select.items[0].style, 1, 'a dropdown has no colours');
      const menu = ch.sent[0].payload.components[0].toJSON().components[0];
      assert.equal(menu.placeholder, 'Choose one');
      assert.equal(menu.options[0].description, 'Updates');

      const buttons = await rrStore.create(w.id, valid('reactionRoles', rrBody({
        channelId: ch.id, placeholder: 'ignored', items: [rrItem(role.id, { label: 'News', description: 'ignored', style: 2 })],
      })), w.ctx());
      assert.equal(buttons.placeholder, null);
      assert.equal(buttons.items[0].description, null);
      assert.equal(buttons.items[0].style, 2);
    });

    test('emoji-reaction panels: a removed role loses its reaction, a new one gains it', async () => {
      const w = makeWorld(await newGuild());
      const [a, b, c] = ['A', 'B', 'C'].map((name) => w.role(name));
      const ch = w.text('roles');
      const made = await rrStore.create(w.id, valid('reactionRoles', rrBody({
        channelId: ch.id, layout: 'REACTIONS', items: [rrItem(a.id, { label: 'A', emoji: '✅' }), rrItem(b.id, { label: 'B', emoji: '🔔' })],
      })), w.ctx());
      const msg = ch.sent[0];
      const bell = msg.reactions.cache.get('🔔');
      assert.ok(bell);
      await rrStore.update(w.id, made.id, valid('reactionRoles', {
        items: [rrItem(a.id, { label: 'A', emoji: '✅' }), rrItem(c.id, { label: 'C', emoji: '🎮' })],
      }, update(made)), w.ctx());
      assert.equal(bell.removedAll, true, 'the bell reaction is taken off');
      assert.ok(!msg.reactions.cache.has('🔔'));
      assert.ok(msg.reactions.cache.has('🎮'));
      assert.ok(msg.reactions.cache.has('✅'));
      assert.match(msg.edits.at(-1).embeds[0].data.description, /🎮 — \*\*C\*\*/);
    });

    test('a custom emoji must be one of the server\'s', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('A');
      const ch = w.text('roles');
      const err = await code(rrStore.create(w.id, valid('reactionRoles', rrBody({
        channelId: ch.id, items: [rrItem(role.id, { emoji: '<:other:999999999999999999>' })],
      })), w.ctx()));
      assert.equal(err.code, 'invalid');
      assert.equal(err.path, 'items.0.emoji');
      assert.deepEqual(await rrStore.list(w.id), [], 'nothing was saved');
    });

    test('a custom emoji created a moment ago is found by reading the server again', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('A');
      const ch = w.text('roles');
      const fresh = { id: '555000000000000001', name: 'newbie', animated: false };
      let reads = 0;
      const discord = {
        getGuildResources: async (gid, opts = {}) => {
          reads += 1;
          const r = w.resources();
          return opts.fresh ? { ...r, emojis: [...r.emojis, fresh] } : r;
        },
      };
      const item = await rrStore.create(w.id, valid('reactionRoles', rrBody({
        channelId: ch.id, items: [rrItem(role.id, { emoji: `<:newbie:${fresh.id}>` })],
      })), w.ctx({ discord }));
      assert.equal(reads, 2, 'the cached list did not know it, the fresh one did');
      assert.equal(item.items[0].emoji, `<:newbie:${fresh.id}>`);
    });

    test('when the panel message was deleted in Discord, the change is saved and the admin is told to repost', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('A');
      const ch = w.text('roles');
      const made = await rrStore.create(w.id, valid('reactionRoles', rrBody({ channelId: ch.id, items: [rrItem(role.id)] })), w.ctx());
      ch.store.delete(ch.sent[0].id);
      const err = await code(rrStore.update(w.id, made.id, valid('reactionRoles', { title: 'Changed' }, update(made)), w.ctx()));
      assert.equal(err.code, 'failed');
      assert.match(err.message, /Your changes were saved/);
      assert.match(err.message, /deleted in Discord/);
      const [stored] = await rrStore.list(w.id);
      assert.equal(stored.title, 'Changed');
      assert.equal(stored.status, 'Message deleted');
      const out = await rrStore.actions.repost(w.id, made.id, { channelId: null }, w.ctx());
      assert.equal(out.item.status, 'Posted');
      assert.equal(ch.sent.length, 2);
    });

    test('Repost moves the panel: the old message goes, the row follows', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('A');
      const one = w.text('one');
      const two = w.text('two');
      const made = await rrStore.create(w.id, valid('reactionRoles', rrBody({ channelId: one.id, items: [rrItem(role.id)] })), w.ctx());
      const old = one.sent[0];
      const out = await rrStore.actions.repost(w.id, made.id, { channelId: two.id }, w.ctx());
      assert.equal(old.deleted, true);
      assert.equal(two.sent.length, 1);
      assert.equal(out.item.channelId, two.id);
      assert.equal(out.item.messageUrl, two.sent[0].url);
      assert.equal((await rowOf(made.id)).messageId, two.sent[0].id);
      // not a text channel
      const voice = w.text('voice', { type: ChannelType.GuildVoice });
      const err = await code(rrStore.actions.repost(w.id, made.id, { channelId: voice.id }, w.ctx()));
      assert.equal(err.code, 'failed');
      assert.match(err.message, /text or announcement/);
    });

    test('delete removes the panel and its message', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('A');
      const ch = w.text('roles');
      const made = await rrStore.create(w.id, valid('reactionRoles', rrBody({ channelId: ch.id, items: [rrItem(role.id)] })), w.ctx());
      await rrStore.remove(w.id, made.id, w.ctx());
      assert.equal(ch.sent[0].deleted, true);
      assert.equal(await rowOf(made.id), null);
      assert.equal(await rrStore.get(w.id, made.id), null);
      assert.equal(await prisma.reactionRoleItem.count({ where: { groupId: made.id } }), 0);
      assert.equal((await code(rrStore.remove(w.id, made.id, w.ctx()))).code, 'not_found');
    });

    test('deleting a panel that was never posted does not need the bot', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('A');
      const ch = w.text('roles', { deny: ['SendMessages'] });
      await code(rrStore.create(w.id, valid('reactionRoles', rrBody({ channelId: ch.id, items: [rrItem(role.id)] })), w.ctx()));
      const [pending] = await rrStore.list(w.id);
      await rrStore.remove(w.id, pending.id, { logger: { info() {} } });
      assert.deepEqual(await rrStore.list(w.id), []);
    });

    test('an attached panel stays attached: the foreign message is never edited, reposted over or deleted', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('Fan');
      const other = w.role('Mod');
      const ch = w.text('rules');
      const foreign = makeMessage(ch, { content: 'Server rules' });
      const id = 'attached-1';
      await rrSvc.createPanel({
        id, guildId: w.id, channelId: ch.id, messageId: foreign.id, title: 'Rules', description: null, type: 'NORMAL', layout: 'REACTIONS', maxRoles: null,
        plainText: false, messageContent: null, attached: true, items: [],
      });
      let item = await rrStore.get(w.id, id);
      assert.equal(item.attached, true);
      assert.equal(item.status, 'Attached message');

      // the title changes without any Discord work; the role mode reaches the bot's panel index but never the message
      item = await rrStore.update(w.id, id, valid('reactionRoles', { title: 'House rules' }, update(item)), w.ctx());
      assert.equal(item.title, 'House rules');
      assert.equal(w.runs.length, 0, 'nothing to do on a message the bot never edits');
      item = await rrStore.update(w.id, id, valid('reactionRoles', { type: 'UNIQUE' }, update(item)), w.ctx());
      assert.equal(item.type, 'UNIQUE');
      assert.deepEqual(w.runs.map((r) => r[0]), ['rr.publish']);
      w.runs.length = 0;
      assert.equal(foreign.edits.length, 0);
      // the message text cannot change
      const refused = await code(rrStore.update(w.id, id, { description: 'new' }, w.ctx()));
      assert.equal(refused.code, 'invalid');
      assert.equal(refused.path, 'description');
      assert.equal((await rowOf(id)).description, null);

      // roles are added: the bot only reacts
      item = await rrStore.update(w.id, id, valid('reactionRoles', { items: [rrItem(role.id, { label: 'Fan', emoji: '⭐' })] }, update(item)), w.ctx());
      assert.deepEqual(foreign.reacted, ['⭐']);
      assert.equal(foreign.edits.length, 0);
      assert.equal(item.status, 'Attached message');
      // and removed: the reaction goes
      const star = foreign.reactions.cache.get('⭐');
      item = await rrStore.update(w.id, id, valid('reactionRoles', { items: [rrItem(other.id, { label: 'Mod', emoji: '🛡️' })] }, update(item)), w.ctx());
      assert.equal(star.removedAll, true);
      assert.ok(foreign.reactions.cache.has('🛡️'));

      // Repost without a channel only puts the reactions back
      foreign.reactions.cache.clear();
      let out = await rrStore.actions.repost(w.id, id, { channelId: null }, w.ctx());
      assert.match(out.message, /reactions back/);
      assert.deepEqual([...foreign.reactions.cache.keys()], ['🛡️']);
      assert.equal(ch.sent.length, 0);

      // delete: the row goes, the foreign message stays, the bot's own reactions come off
      await rrStore.remove(w.id, id, w.ctx());
      assert.equal(foreign.deleted, false);
      assert.deepEqual(foreign.reactions.cache.get('🛡️').removedUsers, [BOT]);
      assert.equal(await rowOf(id), null);

      // Repost with a channel turns an attached panel into a message of the bot's (the original is left alone)
      const again = 'attached-2';
      await rrSvc.createPanel({
        id: again, guildId: w.id, channelId: ch.id, messageId: foreign.id, title: 'Rules', description: null, type: 'NORMAL', layout: 'REACTIONS', maxRoles: null,
        plainText: false, messageContent: null, attached: true, items: [rrSvc.draftItem(again, { roleId: role.id, label: 'Fan', emoji: '⭐' }, 0)],
      });
      const target = w.text('panels');
      out = await rrStore.actions.repost(w.id, again, { channelId: target.id }, w.ctx());
      assert.equal(out.item.attached, false);
      assert.equal(out.item.channelId, target.id);
      assert.equal(foreign.deleted, false);
      assert.deepEqual(target.sent[0].reacted, ['⭐']);
    });

    test('an attached message that was deleted is replaced by a message of the bot\'s on Repost', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('Fan');
      const ch = w.text('rules');
      await rrSvc.createPanel({
        id: 'attached-gone', guildId: w.id, channelId: ch.id, messageId: newId(), title: 'Rules', description: null, type: 'NORMAL', layout: 'REACTIONS', maxRoles: null,
        plainText: false, messageContent: null, attached: true, items: [rrSvc.draftItem('attached-gone', { roleId: role.id, label: 'Fan', emoji: '⭐' }, 0)],
      });
      const out = await rrStore.actions.repost(w.id, 'attached-gone', { channelId: null }, w.ctx());
      assert.equal(ch.sent.length, 1);
      assert.equal(out.item.attached, false);
      assert.equal(out.item.status, 'Posted');
    });

    test('the server holds at most LIMITS.REACTION_ROLE_PANELS panels, even when creates race', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('A');
      const ch = w.text('roles');
      const fill = LIMITS.REACTION_ROLE_PANELS - 1;
      await prisma.reactionRoleGroup.createMany({
        data: Array.from({ length: fill }, (_, i) => ({ guildId: w.id, channelId: ch.id, messageId: `5${String(i).padStart(17, '0')}`, title: `P${i}` })),
      });
      const results = await Promise.all([1, 2, 3].map((i) => code(rrStore.create(w.id, valid('reactionRoles', rrBody({ channelId: ch.id, title: `Race ${i}`, items: [rrItem(role.id)] })), w.ctx()))));
      assert.equal(results.filter((r) => r === null).length, 1, 'exactly one create fits');
      assert.deepEqual(results.filter(Boolean).map((r) => r.code), ['limit', 'limit']);
      assert.equal(await prisma.reactionRoleGroup.count({ where: { guildId: w.id } }), LIMITS.REACTION_ROLE_PANELS);
      assert.equal((await rrStore.list(w.id)).length, LIMITS.REACTION_ROLE_PANELS);
    });

    test('a panel of another server is not found (get, update, remove, repost)', async () => {
      const w1 = makeWorld(await newGuild());
      const w2 = makeWorld(await newGuild());
      const role = w1.role('A');
      const ch = w1.text('roles');
      const made = await rrStore.create(w1.id, valid('reactionRoles', rrBody({ channelId: ch.id, items: [rrItem(role.id)] })), w1.ctx());
      assert.equal(await rrStore.get(w2.id, made.id), null);
      assert.deepEqual(await rrStore.list(w2.id), []);
      assert.equal((await code(rrStore.update(w2.id, made.id, { title: 'x' }, w2.ctx()))).code, 'not_found');
      assert.equal((await code(rrStore.remove(w2.id, made.id, w2.ctx()))).code, 'not_found');
      assert.equal((await code(rrStore.actions.repost(w2.id, made.id, { channelId: null }, w2.ctx()))).code, 'not_found');
      assert.equal((await rowOf(made.id)).title, 'Pick your roles');
      // and a task for another server's panel finds nothing
      await assert.rejects(rrTasks['rr.delete'].run({ guild: w2.guild, client: w2.client, actor: w2.actor(), payload: { groupId: made.id }, taskId: 't' }),
        (err) => err instanceof TaskError && /no longer exists/.test(err.message));
      assert.ok(await rowOf(made.id));
    });

    test('the task payload is not trusted: a reaction an entry still uses is never removed', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('A');
      const ch = w.text('roles');
      const made = await rrStore.create(w.id, valid('reactionRoles', rrBody({
        channelId: ch.id, layout: 'REACTIONS', items: [rrItem(role.id, { label: 'A', emoji: '✅' })],
      })), w.ctx());
      const check = ch.sent[0].reactions.cache.get('✅');
      await rrTasks['rr.publish'].run({
        guild: w.guild, client: w.client, actor: w.actor(), taskId: 't', payload: { groupId: made.id, mode: 'sync', removedEmojis: ['✅', 5, 'x'.repeat(200), null] },
      });
      assert.equal(check.removedAll, false);
      assert.ok(ch.sent[0].reactions.cache.has('✅'));
    });

    test('tasks check the dashboard user again: a member who lost Manage Server cannot post', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('A');
      const ch = w.text('roles');
      const made = await rrStore.create(w.id, valid('reactionRoles', rrBody({ channelId: ch.id, items: [rrItem(role.id)] })), w.ctx());
      const demoted = { id: ADMIN, member: { id: ADMIN, guild: w.guild, roles: { cache: new Collection() }, permissions: { has: () => false } }, isBotOwner: false };
      for (const kind of ['rr.publish', 'rr.repost', 'rr.delete']) {
        await assert.rejects(rrTasks[kind].run({ guild: w.guild, client: w.client, actor: demoted, payload: { groupId: made.id, mode: 'sync' }, taskId: 't' }),
          (err) => err instanceof TaskError && /Admin level/.test(err.message), kind);
      }
      const owner = { id: OWNER, member: null, isBotOwner: true };
      const r = await rrTasks['rr.publish'].run({ guild: w.guild, client: w.client, actor: owner, payload: { groupId: made.id, mode: 'sync' }, taskId: 't' });
      assert.match(r.message, /Updated the panel message/);
      assert.ok(await rowOf(made.id), 'nothing was deleted');
    });

    test('a bot that is offline leaves the saved panel and says so', async () => {
      const w = makeWorld(await newGuild());
      const role = w.role('A');
      const ch = w.text('roles');
      const offline = { runTask: async () => ({ status: 'offline', message: 'The bot did not pick this up in time, so nothing was done.', data: null }) };
      const err = await code(rrStore.create(w.id, valid('reactionRoles', rrBody({ channelId: ch.id, items: [rrItem(role.id)] })), w.ctx(offline)));
      assert.equal(err.code, 'unavailable');
      assert.match(err.message, /was saved, but it was not posted/);
      assert.equal((await rrStore.list(w.id)).length, 1);
      // still working when the wait ends: the item is returned as it is
      const pendingCtx = { runTask: async () => ({ status: 'pending', message: 'The bot is still working on it.', data: null }) };
      const item = await rrStore.update(w.id, (await rrStore.list(w.id))[0].id, { title: 'Later' }, w.ctx(pendingCtx));
      assert.equal(item.title, 'Later');
    });
  });

  // ── Ticket panels ──────────────────────────────────────────────────────────

  describe('ticket panels', () => {
    const rowOf = (id) => prisma.ticketPanel.findUnique({ where: { id }, include: { options: { orderBy: [{ position: 'asc' }, { id: 'asc' }] } } });
    const tbody = (over = {}) => ({ name: 'Support', ...over });

    test('create saves the panel with its options and settings; a default message is stored as "not set"', async () => {
      const w = makeWorld(await newGuild());
      const mgr = w.role('Support team', { mentionable: true });
      const open = w.category('Tickets');
      const transcripts = w.text('transcripts');
      const item = await tkStore.create(w.id, valid('ticketPanels', tbody({
        style: 'SELECT', managerRoleIds: [mgr.id], openCategoryId: open.id, transcriptChannelId: transcripts.id, dmTranscript: true, maxOpenPerUser: 2,
        channelNameFormat: '{user.name}-{number}', askReason: true,
        options: [opt('Billing', { emoji: '💰', description: 'Payments', buttonStyle: 3, categoryId: open.id }), opt('Other')],
      })), w.ctx());

      assert.deepEqual(w.runs.map((r) => r[0]), ['ticket.check'], 'the permissions are checked first; no channel, nothing published');
      assert.deepEqual(w.runs[0][1], { categoryIds: [open.id], transcriptChannelId: transcripts.id });
      assert.equal(item.status, 'Not published');
      assert.equal(item.style, 'SELECT');
      assert.deepEqual(item.managerRoleIds, [mgr.id]);
      assert.equal(item.openCategoryId, open.id);
      assert.equal(item.transcriptChannelId, transcripts.id);
      assert.equal(item.dmTranscript, true);
      assert.equal(item.maxOpenPerUser, 2);
      assert.equal(item.askReason, true);
      assert.equal(item.channelNameFormat, '{user.name}-{number}');
      assert.deepEqual(item.options.map((o) => [o.label, o.emoji, o.description, o.buttonStyle, o.categoryId]), [
        ['Billing', '💰', 'Payments', 3, open.id], ['Other', null, null, 1, null],
      ]);
      assert.ok(item.options.every((o) => typeof o.id === 'string'));
      assert.equal(item.template.embed.title, 'How can we help?');

      const row = await rowOf(item.id);
      assert.equal(row.template, null, 'the default message reads as "not set"');
      assert.equal(row.introTemplate, null);
      assert.deepEqual(row.options.map((o) => o.label), ['Billing', 'Other'], 'the default "Open Ticket" option is replaced');
      assert.equal(row.options.length, 2);
      assert.deepEqual(await tkStore.list(w.id), [item]);
    });

    test('create with a channel publishes the panel: buttons from the options, in order', async () => {
      const w = makeWorld(await newGuild());
      const ch = w.text('support');
      const item = await tkStore.create(w.id, valid('ticketPanels', tbody({
        channelId: ch.id, options: [opt('Billing', { emoji: '💰', buttonStyle: 3 }), opt('Other', { buttonStyle: 2 })],
      })), w.ctx());
      assert.deepEqual(w.runs.map((r) => r[0]), ['ticket.publish']);
      assert.equal(ch.sent.length, 1);
      const sent = ch.sent[0];
      assert.equal(sent.payload.embeds[0].data.title, 'How can we help?');
      const buttons = sent.payload.components[0].toJSON().components;
      assert.deepEqual(buttons.map((b) => [b.label, b.style]), [['Billing', 3], ['Other', 2]]);
      const row = await rowOf(item.id);
      assert.deepEqual([row.channelId, row.messageId], [ch.id, sent.id]);
      assert.equal(item.status, 'Published');
      assert.equal(item.messageUrl, sent.url);
      assert.deepEqual(buttons.map((b) => b.custom_id), row.options.map((o) => `ticket:open:${row.id}:${o.id}`));
    });

    test('a custom panel message and intro are stored as written; the panel message drops link buttons', async () => {
      const w = makeWorld(await newGuild());
      const item = await tkStore.create(w.id, valid('ticketPanels', tbody({
        template: { content: 'Open a ticket', embed: { title: 'Help', description: 'Pick a topic' }, buttons: [{ label: 'Docs', url: 'https://example.com' }] },
        introTemplate: { content: '{user}', embed: { title: 'Hi' }, buttons: [{ label: 'Docs', url: 'https://example.com' }] },
        options: [opt('A', { introTemplate: { content: 'Special intro', embed: null, buttons: [] } }), opt('B')],
      })), w.ctx());
      const row = await rowOf(item.id);
      assert.equal(row.template.content, 'Open a ticket');
      assert.deepEqual(row.template.buttons, []);
      assert.equal(row.introTemplate.buttons.length, 1);
      assert.equal(row.options[0].introTemplate.content, 'Special intro');
      assert.equal(row.options[1].introTemplate, null);
      assert.equal(item.options[1].introTemplate.content, null, 'no override reads as an empty message');
    });

    test('a name that is taken (any case) is a conflict, and a failed create leaves nothing behind', async () => {
      const w = makeWorld(await newGuild());
      await tkStore.create(w.id, valid('ticketPanels', tbody({ name: 'Support' })), w.ctx());
      const err = await code(tkStore.create(w.id, valid('ticketPanels', tbody({ name: 'SUPPORT' })), w.ctx()));
      assert.equal(err.code, 'conflict');
      assert.equal(err.path, 'name');
      assert.equal((await tkStore.list(w.id)).length, 1);
      // a category the bot cannot use is refused before anything is saved
      const bad = w.category('Locked', { deny: ['ManageChannels'] });
      const refused = await code(tkStore.create(w.id, valid('ticketPanels', tbody({ name: 'Other', openCategoryId: bad.id })), w.ctx()));
      assert.equal(refused.code, 'failed');
      assert.match(refused.message, /I need Manage Channels in the "Locked" category/);
      assert.equal((await tkStore.list(w.id)).length, 1);
    });

    test('the transcript channel needs View Channel, Send Messages, Embed Links and Attach Files', async () => {
      const w = makeWorld(await newGuild());
      const ch = w.text('transcripts', { deny: ['AttachFiles'] });
      const err = await code(tkStore.create(w.id, valid('ticketPanels', tbody({ transcriptChannelId: ch.id })), w.ctx()));
      assert.equal(err.code, 'failed');
      assert.match(err.message, /I need Attach Files in #transcripts/);
      ch.deny = [];
      const item = await tkStore.create(w.id, valid('ticketPanels', tbody({ transcriptChannelId: ch.id })), w.ctx());
      assert.equal(item.transcriptChannelId, ch.id);
    });

    test('the server holds at most LIMITS.TICKET_PANELS panels', async () => {
      const w = makeWorld(await newGuild());
      await prisma.ticketPanel.createMany({ data: Array.from({ length: LIMITS.TICKET_PANELS }, (_, i) => ({ guildId: w.id, name: `P${i}` })) });
      const err = await code(tkStore.create(w.id, valid('ticketPanels', tbody({ name: 'One more' })), w.ctx()));
      assert.equal(err.code, 'limit');
    });

    test('editing settings needs no task; a changed message updates the published panel', async () => {
      const w = makeWorld(await newGuild());
      const ch = w.text('support');
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({ channelId: ch.id })), w.ctx());
      const msg = ch.sent[0];
      w.runs.length = 0;

      let item = await tkStore.update(w.id, made.id, valid('ticketPanels', { name: 'Help desk', maxOpenPerUser: 3, askReason: true, dmTranscript: true }, update(made)), w.ctx());
      assert.equal(w.runs.length, 0);
      assert.deepEqual([item.name, item.maxOpenPerUser, item.askReason, item.dmTranscript], ['Help desk', 3, true, true]);

      item = await tkStore.update(w.id, made.id, valid('ticketPanels', {
        template: { content: null, embed: { title: 'Need help?', description: 'Press below' }, buttons: [] },
      }, update(item)), w.ctx());
      assert.deepEqual(w.runs.map((r) => r[0]), ['ticket.refresh']);
      assert.equal(msg.edits.length, 1);
      assert.equal(msg.edits[0].embeds[0].data.title, 'Need help?');
      assert.equal(item.template.embed.title, 'Need help?');

      // the style changes the components
      w.runs.length = 0;
      item = await tkStore.update(w.id, made.id, valid('ticketPanels', { style: 'SELECT' }, update(item)), w.ctx());
      assert.deepEqual(w.runs.map((r) => r[0]), ['ticket.refresh']);
      assert.equal(msg.edits.at(-1).components[0].toJSON().components[0].type, 3, 'a select menu');

      // an empty message goes back to the default
      item = await tkStore.update(w.id, made.id, valid('ticketPanels', { template: emptyTemplate() }, update(item)), w.ctx());
      assert.equal((await rowOf(made.id)).template, null);
      assert.equal(item.template.embed.title, 'How can we help?');
      assert.match(item.template.embed.description, /Choose a topic/);
    });

    test('a rename that collides with another panel is a conflict', async () => {
      const w = makeWorld(await newGuild());
      await tkStore.create(w.id, valid('ticketPanels', tbody({ name: 'First' })), w.ctx());
      const second = await tkStore.create(w.id, valid('ticketPanels', tbody({ name: 'Second' })), w.ctx());
      const err = await code(tkStore.update(w.id, second.id, valid('ticketPanels', { name: 'first' }, update(second)), w.ctx()));
      assert.equal(err.code, 'conflict');
      assert.equal((await rowOf(second.id)).name, 'Second');
    });

    test('options are matched by id: a rename or reorder keeps the row, so its tickets stay linked', async () => {
      const w = makeWorld(await newGuild());
      const ch = w.text('support');
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({ channelId: ch.id, options: [opt('Billing'), opt('Bugs'), opt('Other')] })), w.ctx());
      const [billing, bugs, other] = made.options;
      // a ticket opened with "Bugs"
      const ticket = await prisma.ticket.create({ data: { guildId: w.id, panelId: made.id, optionId: bugs.id, number: 1, channelId: newId(), ownerId: '1', status: 'OPEN' } });
      w.runs.length = 0;

      const form = { options: [{ ...other }, { ...bugs, label: 'Bug reports', description: 'Something broke' }, { ...billing }, opt('New one')] };
      const item = await tkStore.update(w.id, made.id, valid('ticketPanels', form, update(made)), w.ctx());
      assert.deepEqual(item.options.map((o) => o.label), ['Other', 'Bug reports', 'Billing', 'New one']);
      assert.deepEqual(item.options.slice(0, 3).map((o) => o.id), [other.id, bugs.id, billing.id], 'the same rows');
      assert.equal(item.options[1].description, 'Something broke');
      assert.deepEqual(w.runs.map((r) => r[0]), ['ticket.refresh']);
      const posted = ch.sent[0].edits.at(-1).components[0].toJSON().components;
      assert.deepEqual(posted.map((b) => b.label), ['Other', 'Bug reports', 'Billing', 'New one']);

      // remove one: the ticket keeps working with no option
      const smaller = await tkStore.update(w.id, made.id, valid('ticketPanels', { options: item.options.filter((o) => o.label !== 'Bug reports') }, update(item)), w.ctx());
      assert.deepEqual(smaller.options.map((o) => o.label), ['Other', 'Billing', 'New one']);
      assert.equal((await prisma.ticket.findUnique({ where: { id: ticket.id } })).optionId, null);
      assert.equal((await prisma.ticket.findUnique({ where: { id: ticket.id } })).panelId, made.id);
      // swapping two labels is not a clash
      const swapped = await tkStore.update(w.id, made.id, valid('ticketPanels', {
        options: [{ ...smaller.options[0], label: 'Billing' }, { ...smaller.options[1], label: 'Other' }, smaller.options[2]],
      }, update(smaller)), w.ctx());
      assert.deepEqual(swapped.options.map((o) => [o.label, o.id]), [['Billing', smaller.options[0].id], ['Other', smaller.options[1].id], ['New one', smaller.options[2].id]]);
    });

    test('a panel keeps at least one option and at most LIMITS.TICKET_PANEL_OPTIONS', async () => {
      const w = makeWorld(await newGuild());
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({})), w.ctx());
      assert.equal(made.options.length, 1);
      assert.equal((await code(tkStore.update(w.id, made.id, { options: [] }, w.ctx()))).code, 'invalid');
      assert.equal((await code(tkStore.update(w.id, made.id, { options: Array.from({ length: LIMITS.TICKET_PANEL_OPTIONS + 1 }, (_, i) => opt(`O${i}`)) }, w.ctx()))).code, 'limit');
      assert.equal((await rowOf(made.id)).options.length, 1);
    });

    test('a custom emoji on an option must be one of the server\'s', async () => {
      const w = makeWorld(await newGuild());
      const party = w.emoji('party');
      const item = await tkStore.create(w.id, valid('ticketPanels', tbody({ options: [opt('A', { emoji: party.text })] })), w.ctx());
      assert.equal(item.options[0].emoji, party.text);
      const err = await code(tkStore.update(w.id, item.id, valid('ticketPanels', { options: [opt('A', { emoji: '<:other:999999999999999999>' })] }, update(item)), w.ctx()));
      assert.equal(err.code, 'invalid');
      assert.equal(err.path, 'options.0.emoji');
    });

    test('a custom emoji created a moment ago is found by reading the server again', async () => {
      const w = makeWorld(await newGuild());
      const fresh = { id: '555000000000000002', name: 'newbie', animated: true };
      const discord = {
        getGuildResources: async (gid, opts = {}) => {
          const r = w.resources();
          return opts.fresh ? { ...r, emojis: [...r.emojis, fresh] } : r;
        },
      };
      const item = await tkStore.create(w.id, valid('ticketPanels', tbody({ options: [opt('A', { emoji: `<:newbie:${fresh.id}>` })] })), w.ctx({ discord }));
      assert.equal(item.options[0].emoji, `<a:newbie:${fresh.id}>`, 'written as the server has it');
    });

    test('manager roles: saved on the panel, back-filled into existing tickets, and removed again', async () => {
      const w = makeWorld(await newGuild());
      const r1 = w.role('Support', { mentionable: true });
      const r2 = w.role('Billing', { mentionable: true });
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({ managerRoleIds: [r1.id] })), w.ctx());
      const t1 = w.text('ticket-0001');
      const t2 = w.text('ticket-0002');
      await prisma.ticket.createMany({ data: [
        { guildId: w.id, panelId: made.id, number: 1, channelId: t1.id, ownerId: '1', status: 'OPEN' },
        { guildId: w.id, panelId: made.id, number: 2, channelId: t2.id, ownerId: '2', status: 'CLOSED' },
      ] });
      w.runs.length = 0;

      let item = await tkStore.update(w.id, made.id, valid('ticketPanels', { managerRoleIds: [r1.id, r2.id] }, update(made)), w.ctx());
      assert.deepEqual(item.managerRoleIds, [r1.id, r2.id]);
      assert.deepEqual(w.runs.map((r) => [r[0], r[1].managersAdded, r[1].managersRemoved, r[1].message]), [['ticket.refresh', [r2.id], [], false]]);
      for (const t of [t1, t2]) assert.deepEqual(t.calls.map((c) => [c[0], c[1]]), [['overwrite.edit', r2.id]]);

      w.runs.length = 0;
      item = await tkStore.update(w.id, made.id, valid('ticketPanels', { managerRoleIds: [r2.id] }, update(item)), w.ctx());
      assert.deepEqual(item.managerRoleIds, [r2.id]);
      assert.deepEqual(t1.calls.at(-1), ['overwrite.delete', r1.id]);
      assert.deepEqual((await rowOf(made.id)).managerRoleIds, [r2.id]);

      // a role that was deleted in Discord drops out of the list and is not synced
      const gone = snow(777);
      const kept = await tkStore.update(w.id, made.id, valid('ticketPanels', { managerRoleIds: [r2.id, gone] }, update(item)), w.ctx());
      assert.deepEqual(kept.managerRoleIds, [r2.id]);
    });

    test('{managers} in an intro needs manager roles the admin could ping', async () => {
      const w = makeWorld(await newGuild());
      const quiet1 = w.role('Quiet team', { mentionable: false });
      const loud = w.role('Loud team', { mentionable: true });
      const intro = { content: '{managers}', embed: { title: 'Hello' }, buttons: [] };
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({ managerRoleIds: [loud.id], introTemplate: intro })), w.ctx());
      assert.deepEqual(made.managerRoleIds, [loud.id]);
      // adding a role the admin cannot ping
      const err = await code(tkStore.update(w.id, made.id, valid('ticketPanels', { managerRoleIds: [loud.id, quiet1.id] }, update(made)), w.ctx()));
      assert.equal(err.code, 'invalid');
      assert.equal(err.path, 'managerRoleIds');
      assert.match(err.message, /not mentionable/);
      assert.deepEqual((await rowOf(made.id)).managerRoleIds, [loud.id]);
      // an admin who may mention every role can
      const mentionAll = w.ctx({ member: { owner: false, permissions: String(PermissionFlagsBits.MentionEveryone) } });
      const ok = await tkStore.update(w.id, made.id, valid('ticketPanels', { managerRoleIds: [loud.id, quiet1.id] }, update(made)), mentionAll);
      assert.equal(ok.managerRoleIds.length, 2);
      // a panel without {managers} in its intros has no such rule
      const plain = await tkStore.create(w.id, valid('ticketPanels', tbody({ name: 'Plain', managerRoleIds: [quiet1.id] })), w.ctx());
      assert.deepEqual(plain.managerRoleIds, [quiet1.id]);
      // writing {managers} into an intro checks the roles already there
      const err2 = await code(tkStore.update(w.id, plain.id, valid('ticketPanels', { introTemplate: intro }, update(plain)), w.ctx()));
      assert.equal(err2.code, 'invalid');
      assert.equal(err2.path, 'managerRoleIds');
    });

    test('the Publish action posts, updates in place, and moves the panel (the old message goes)', async () => {
      const w = makeWorld(await newGuild());
      const one = w.text('one');
      const two = w.text('two');
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({})), w.ctx());
      assert.equal(made.status, 'Not published');
      let out = await tkStore.actions.publish(w.id, made.id, { channelId: one.id }, w.ctx());
      assert.match(out.message, /Posted "Support" in #one/);
      assert.match(out.message, /no ticket manager roles/, 'the warning /ticket-panel publish gives');
      assert.equal(out.item.status, 'Published');
      const first = one.sent[0];
      out = await tkStore.actions.publish(w.id, made.id, { channelId: one.id }, w.ctx());
      assert.match(out.message, /Updated "Support" in #one/);
      assert.equal(one.sent.length, 1, 'edited in place');
      assert.equal(first.edits.length, 1);
      out = await tkStore.actions.publish(w.id, made.id, { channelId: two.id }, w.ctx());
      assert.equal(two.sent.length, 1);
      assert.deepEqual(one.calls.at(-1), ['messages.delete', first.id], 'the message in the old channel is deleted');
      assert.equal(out.item.channelId, two.id);
      assert.equal(out.item.messageUrl, two.sent[0].url);
    });

    test('picking another channel in the form moves the panel; clearing it is refused', async () => {
      const w = makeWorld(await newGuild());
      const one = w.text('one');
      const two = w.text('two');
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({ channelId: one.id })), w.ctx());
      const item = await tkStore.update(w.id, made.id, valid('ticketPanels', { channelId: two.id }, update(made)), w.ctx());
      assert.equal(item.channelId, two.id);
      assert.equal(two.sent.length, 1);
      const err = await code(tkStore.update(w.id, made.id, { channelId: null }, w.ctx()));
      assert.equal(err.code, 'invalid');
      assert.equal(err.path, 'channelId');
    });

    test('publishing needs the bot\'s permissions; the panel is kept and the message says what is missing', async () => {
      const w = makeWorld(await newGuild());
      const locked = w.text('locked', { deny: ['EmbedLinks'] });
      const err = await code(tkStore.create(w.id, valid('ticketPanels', tbody({ channelId: locked.id })), w.ctx()));
      assert.equal(err.code, 'failed');
      assert.match(err.message, /was saved, but it was not published/);
      assert.match(err.message, /I need Embed Links in #locked/);
      const [panel] = await tkStore.list(w.id);
      assert.equal(panel.status, 'Not published');
      // server-wide permissions
      w.guild.members.me.permissions.has = (bit) => bit !== PermissionFlagsBits.ManageChannels;
      const open = w.text('open');
      const out = await code(tkStore.actions.publish(w.id, panel.id, { channelId: open.id }, w.ctx()));
      assert.equal(out.code, 'failed');
      assert.match(out.message, /To open tickets I need Manage Channels on this server/);
    });

    test('the Update action re-renders the posted message and says when there is none', async () => {
      const w = makeWorld(await newGuild());
      const ch = w.text('support');
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({})), w.ctx());
      const none = await code(tkStore.actions.refresh(w.id, made.id, {}, w.ctx()));
      assert.equal(none.code, 'failed');
      assert.match(none.message, /not published/);
      await tkStore.actions.publish(w.id, made.id, { channelId: ch.id }, w.ctx());
      const out = await tkStore.actions.refresh(w.id, made.id, {}, w.ctx());
      assert.match(out.message, /Updated the published panel message/);
      assert.equal(ch.sent[0].edits.length, 1);
      // the message was deleted by hand
      ch.store.delete(ch.sent[0].id);
      const gone = await code(tkStore.actions.refresh(w.id, made.id, {}, w.ctx()));
      assert.match(gone.message, /was deleted/);
      assert.equal((await tkStore.get(w.id, made.id)).status, 'Not published', 'the panel notices');
    });

    test('delete removes the panel and its message; tickets keep their rows', async () => {
      const w = makeWorld(await newGuild());
      const ch = w.text('support');
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({ channelId: ch.id })), w.ctx());
      const ticket = await prisma.ticket.create({ data: { guildId: w.id, panelId: made.id, number: 1, channelId: newId(), ownerId: '1', status: 'OPEN' } });
      const posted = ch.sent[0].id;
      await tkStore.remove(w.id, made.id, w.ctx());
      assert.deepEqual(ch.calls.at(-1), ['messages.delete', posted]);
      assert.equal(await rowOf(made.id), null);
      assert.equal((await prisma.ticket.findUnique({ where: { id: ticket.id } })).panelId, null);
      assert.equal(await prisma.ticketPanelOption.count({ where: { panelId: made.id } }), 0);
      assert.equal((await code(tkStore.remove(w.id, made.id, w.ctx()))).code, 'not_found');
      // an unpublished panel needs no task
      const draft = await tkStore.create(w.id, valid('ticketPanels', tbody({ name: 'Draft' })), w.ctx());
      w.runs.length = 0;
      await tkStore.remove(w.id, draft.id, { logger: { info() {} } });
      assert.equal(w.runs.length, 0);
      assert.deepEqual(await tkStore.list(w.id), []);
    });

    test('a panel of another server is not found, and its tasks find nothing', async () => {
      const w1 = makeWorld(await newGuild());
      const w2 = makeWorld(await newGuild());
      const made = await tkStore.create(w1.id, valid('ticketPanels', tbody({})), w1.ctx());
      assert.equal(await tkStore.get(w2.id, made.id), null);
      assert.deepEqual(await tkStore.list(w2.id), []);
      assert.equal((await code(tkStore.update(w2.id, made.id, { askReason: true }, w2.ctx()))).code, 'not_found');
      assert.equal((await code(tkStore.remove(w2.id, made.id, w2.ctx()))).code, 'not_found');
      assert.equal((await code(tkStore.actions.publish(w2.id, made.id, { channelId: w2.text('x').id }, w2.ctx()))).code, 'not_found');
      assert.equal((await code(tkStore.actions.refresh(w2.id, made.id, {}, w2.ctx()))).code, 'not_found');
      for (const kind of ['ticket.publish', 'ticket.refresh', 'ticket.delete']) {
        await assert.rejects(tkTasks[kind].run({ guild: w2.guild, client: w2.client, actor: w2.actor(), payload: { panelId: made.id, channelId: w2.text('y').id }, taskId: 't' }),
          (err) => err instanceof TaskError && /no longer exists/.test(err.message), kind);
      }
      assert.ok(await rowOf(made.id));
    });

    test('the form a store returns is accepted back as an update and changes nothing', async () => {
      const w = makeWorld(await newGuild());
      const ch = w.text('support');
      const open = w.category('Tickets');
      const mgr = w.role('Mods', { mentionable: true });
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({
        channelId: ch.id, openCategoryId: open.id, managerRoleIds: [mgr.id], options: [opt('A', { emoji: '💰', description: 'x', categoryId: open.id }), opt('B')],
        template: { content: 'Hi', embed: null, buttons: [] },
      })), w.ctx());
      w.runs.length = 0;
      const { id, summary, status, openTickets, messageUrl, ...form } = made;
      const data = valid('ticketPanels', form, update(made));
      assert.deepEqual(Object.keys(data).sort(), Object.keys(form).sort());
      const again = await tkStore.update(w.id, made.id, data, w.ctx());
      assert.deepEqual(again, made);
      assert.equal(w.runs.length, 0, 'no category is new, no message changed');
      assert.equal(ch.sent[0].edits.length, 0);
    });

    test('a changed category or transcript channel is checked by the bot first and refused when unusable', async () => {
      const w = makeWorld(await newGuild());
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({})), w.ctx());
      const locked = w.category('Locked', { deny: ['ManageRoles'] });
      const err = await code(tkStore.update(w.id, made.id, valid('ticketPanels', { closedCategoryId: locked.id }, update(made)), w.ctx()));
      assert.equal(err.code, 'failed');
      assert.match(err.message, /Manage Roles in the "Locked" category/);
      assert.equal((await rowOf(made.id)).closedCategoryId, null);
      const fine = w.category('Closed');
      const ok = await tkStore.update(w.id, made.id, valid('ticketPanels', { closedCategoryId: fine.id }, update(made)), w.ctx());
      assert.equal(ok.closedCategoryId, fine.id);
      // clearing a category needs no check
      w.runs.length = 0;
      const cleared = await tkStore.update(w.id, made.id, valid('ticketPanels', { closedCategoryId: null }, update(ok)), w.ctx());
      assert.equal(cleared.closedCategoryId, null);
      assert.equal(w.runs.length, 0);
      // a text channel is not a category
      const text = w.text('plain');
      const wrongKind = await code(tkStore.update(w.id, made.id, valid('ticketPanels', { openCategoryId: text.id }, update(made)), w.ctx()));
      assert.match(wrongKind.message, /not a category/);
    });

    test('a permission check the bot has not finished is not a pass: nothing is saved', async () => {
      const w = makeWorld(await newGuild());
      const cat = w.category('Tickets');
      const slow = { runTask: async () => ({ status: 'pending', message: 'The bot is still working on it.', data: null }) };
      const err = await code(tkStore.create(w.id, valid('ticketPanels', tbody({ openCategoryId: cat.id })), w.ctx(slow)));
      assert.equal(err.code, 'unavailable');
      assert.match(err.message, /nothing was saved/);
      assert.deepEqual(await tkStore.list(w.id), []);
    });

    test('a manager change on a published panel only syncs the tickets, not the message', async () => {
      const w = makeWorld(await newGuild());
      const ch = w.text('support');
      const role = w.role('Helpers', { mentionable: true });
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({ channelId: ch.id })), w.ctx());
      w.runs.length = 0;
      await tkStore.update(w.id, made.id, valid('ticketPanels', { managerRoleIds: [role.id] }, update(made)), w.ctx());
      assert.deepEqual(w.runs.map((r) => [r[0], r[1].message, r[1].managersAdded]), [['ticket.refresh', false, [role.id]]]);
      assert.equal(ch.sent[0].edits.length, 0, 'the posted message was not touched');
    });

    test('the delete task confirms the actor again', async () => {
      const w = makeWorld(await newGuild());
      const made = await tkStore.create(w.id, valid('ticketPanels', tbody({})), w.ctx());
      const demoted = { id: ADMIN, member: { id: ADMIN, guild: w.guild, roles: { cache: new Collection() }, permissions: { has: () => false } }, isBotOwner: false };
      await assert.rejects(tkTasks['ticket.delete'].run({ guild: w.guild, client: w.client, actor: demoted, payload: { panelId: made.id }, taskId: 't' }),
        (err) => err instanceof TaskError && /Admin level/.test(err.message));
      assert.ok(await rowOf(made.id));
      const r = await tkTasks['ticket.delete'].run({ guild: w.guild, client: w.client, actor: { id: OWNER, member: null, isBotOwner: true }, payload: { panelId: made.id }, taskId: 't' });
      assert.match(r.message, /Deleted "Support"/);
      assert.equal(await rowOf(made.id), null);
    });
  });
});
