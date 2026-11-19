// The JS side of feat-tickets' cross-language tests: the JS bot's own ticketService (and scheduler) against the test
// database (TEST_DATABASE_URL). Discord is a small fake guild, as in test/tickets.test.js. Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-tickets-js.mjs seed <guildId> <roleId> <ownerId> <idBase>
//       createPanel, updatePanelFields, addOption, updateManagers, openTicket, claimTicket and two merged
//       ticket.managersync jobs, all through the JS service
//   node rust/fixtures/gen/feat-tickets-js.mjs read <guildId> <panelId> <ticketId>
//       findPanel, getTicketById, the guild's ticket.managersync job (read back through mergeManagerSync), and the
//       panel components and intro row the JS builds from those rows
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
console.error = console.warn = console.log = console.debug = console.info = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const svc = await imp('src', 'services', 'ticketService.js');
const scheduler = await imp('src', 'services', 'scheduler.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');
const { ChannelType, Collection, PermissionFlagsBits } = await import('discord.js');

function makeWorld(guildId, base) {
  let next = BigInt(base);
  const nextId = () => String(next++);
  const guild = {
    id: guildId, name: 'JS Guild', available: true, ownerId: '1',
    client: { user: { id: 'bot1', tag: 'bot#0001' }, emojis: { cache: new Collection() }, users: { fetch: async (id) => ({ id, tag: `u${id}` }) } },
    members: { me: { id: 'bot1', permissions: { has: () => true } }, cache: new Collection() },
    roles: { cache: new Collection() },
    emojis: { cache: new Collection() },
  };
  const makeChannel = ({ name, type = ChannelType.GuildText, parent = null }) => {
    const ch = {
      id: nextId(), name, type, parentId: parent, guild,
      permissionsFor: () => ({ has: () => true }),
      isTextBased: () => type === ChannelType.GuildText,
      isThread: () => false,
      send: async () => ({ id: nextId(), url: 'https://discord.com/x' }),
      setParent: async (pid) => { ch.parentId = pid; return ch; },
      permissionOverwrites: { edit: async () => {}, delete: async () => {} },
      messages: { fetch: async () => new Collection() },
    };
    return ch;
  };
  guild.channels = {
    cache: new Collection(),
    create: async (opts) => {
      const ch = makeChannel({ name: opts.name, type: opts.type, parent: opts.parent ?? null });
      guild.channels.cache.set(ch.id, ch);
      return ch;
    },
    fetch: async (id) => {
      const hit = guild.channels.cache.get(id);
      if (hit) return hit;
      throw Object.assign(new Error('Unknown Channel'), { code: 10003 });
    },
  };
  const member = (id, roles = []) => {
    const user = { id, username: `user${id}`, globalName: null, tag: `user${id}`, bot: false };
    const m = { id, displayName: `User ${id}`, user, guild, roles: { cache: new Collection(roles.map((r) => [r, { id: r }])) }, permissions: { has: () => false } };
    guild.members.cache.set(id, m);
    return m;
  };
  return { guild, member, nextId };
}

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'seed') {
    const [guildId, roleId, ownerId, base] = args;
    const world = makeWorld(guildId, base);
    world.guild.roles.cache.set(roleId, { id: roleId, name: 'staff', mentionable: true });
    const created = await svc.createPanel({ guildId, name: 'JS Panel', style: 'SELECT', askReason: true, maxOpenPerUser: 2, channelNameFormat: '{user.name}-{number}' });
    const panelId = created.panel.id;
    await svc.updatePanelFields(panelId, guildId, {
      template: { content: 'Hi', embed: { title: 'T', description: 'D', color: 0x112233 }, buttons: [] }, transcriptChannelId: '955020000000007777', dmTranscript: true,
    });
    const added = await svc.addOption(panelId, guildId, { label: 'Billing', emoji: '💳', description: 'Money', buttonStyle: 3, categoryId: null });
    const managers = await svc.updateManagers(panelId, world.guild, (cur) => ({ ok: true, roles: [...cur, roleId] }));
    const panel = await svc.findPanel(guildId, panelId);
    const opener = world.member(ownerId);
    const staff = world.member(String(BigInt(ownerId) + 1n), [roleId]);
    const opened = await svc.openTicket({
      interaction: { id: world.nextId(), user: opener.user, member: opener }, client: world.guild.client, guild: world.guild, panel, option: panel.options[1],
      reason: 'from JS', settings: null,
    });
    const claimed = await svc.claimTicket({ client: world.guild.client, guild: world.guild, ticket: opened.ticket, actor: staff, settings: null, source: 'command' });
    const runAt = new Date(Date.now() + 3600_000);
    const dedupeKey = `ticket.managersync:${guildId}`;
    await scheduler.scheduleJob({ guildId, type: 'ticket.managersync', runAt, payload: svc.managerSyncPayload(panelId, [roleId]), dedupeKey, onConflict: 'merge', merge: svc.mergeManagerSync });
    await scheduler.scheduleJob({ guildId, type: 'ticket.managersync', runAt, payload: svc.managerSyncPayload(panelId, ['955020000000008888']), dedupeKey, onConflict: 'merge', merge: svc.mergeManagerSync });
    const job = await prisma.scheduledJob.findFirst({ where: { dedupeKey } });
    answer({
      panelId, optionIds: panel.options.map((o) => o.id), added: added.ok, managers, opened: opened.ok, ticketId: opened.ticket?.id,
      channelId: opened.channel?.id, categoryId: opened.channel?.parentId, number: opened.ticket?.number, claimed, jobPayload: job?.payload,
    });
  } else if (cmd === 'read') {
    const [guildId, panelId, ticketId] = args;
    const panel = await svc.findPanel(guildId, panelId);
    const ticket = await svc.getTicketById(guildId, ticketId);
    const job = await prisma.scheduledJob.findFirst({ where: { dedupeKey: `ticket.managersync:${guildId}` } });
    answer({
      panel,
      ticket,
      job: job ? { type: job.type, payload: job.payload, reread: svc.mergeManagerSync(job.payload, {}) } : null,
      components: panel ? svc.buildPanelComponents(panel, panel.options).map((r) => r.toJSON()) : null,
      intro: ticket ? svc.buildIntroRow(ticket.id).toJSON() : null,
      byChannel: ticket ? (await svc.getTicketByChannel(guildId, ticket.channelId))?.id ?? null : null,
    });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
