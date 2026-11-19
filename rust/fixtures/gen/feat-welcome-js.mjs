// The JS side of feat-welcome's cross-language tests: the JS bot's own welcomeService / applicationService against the
// test servers (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-welcome-js.mjs welcome-write <guildId> <userId> <roleId1> <roleId2> <pendingUserId>
//       a legacy welcome embed, then updateEmbedOptions, updateCardStyle, updateMessage, addAutorole (x2, delay 10m),
//       assignAutorole for a member who joined a minute ago (a delayed job) and, with force, for one still in
//       Membership Screening (a watch job)
//   node rust/fixtures/gen/feat-welcome-js.mjs welcome-read <guildId> <userId> [pendingUserId]
//       getSettings({ fresh: true }), getJob(autoroleJobKey) of both members, and what the JS makes of them
//   node rust/fixtures/gen/feat-welcome-js.mjs app-write <guildId> <userId> <reviewChannelId>
//       createForm x2 (one named `a_b%`), addQuestion x3, changeRole, updateForm, startDraft
//   node rust/fixtures/gen/feat-welcome-js.mjs app-read <guildId> <userId> <json array of refs> <json array of queries>
//       listForms, findForm of each ref, findFormsForAutocomplete of each query, pendingCount, the member's submissions
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const { default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));
const svc = await imp('src', 'services', 'welcomeService.js');
const app = await imp('src', 'services', 'applicationService.js');
const settingsService = await imp('src', 'services', 'settingsService.js');
const scheduler = await imp('src', 'services', 'scheduler.js');

// A discord.js-shaped guild: the bot can manage roles below position 10.
function fakeGuild(id, roleIds) {
  const roles = new Map(roleIds.map((r, i) => [r, { id: r, name: `Role ${i + 1}`, position: 2, managed: false, permissions: { has: () => false } }]));
  return {
    id, name: 'Cross Guild', ownerId: 'OWNER', available: true, features: [],
    roles: { cache: roles }, channels: { cache: new Map() },
    members: { me: { id: 'BOT', roles: { highest: { position: 10 } }, permissions: { has: () => true } } },
  };
}
function fakeMember(guild, id, joinedTimestamp) {
  return {
    id, guild, pending: false, joinedTimestamp, user: { id, bot: false, username: `user${id}` },
    roles: { cache: new Map(), highest: { position: 100 }, add: async () => {} },
  };
}
const jobJson = (job) => (job ? { type: job.type, guildId: job.guildId, dedupeKey: job.dedupeKey, payload: job.payload, runAt: job.runAt, status: job.status } : null);

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'welcome-write') {
    const [g, u, r1, r2, p] = args;
    await settingsService.ensureGuildSettings(g);
    await prisma.guildSettings.update({ where: { guildId: g }, data: { welcomeEmbed: { title: 'Old', description: 'Legacy', thumbnail: 'user_avatar', card: false } } });
    await settingsService.invalidateSettings(g);
    await svc.updateEmbedOptions(g, 'welcome', { thumbnail: 'server', imageUrl: 'https://example.test/a.png' });
    await svc.updateEmbedOptions(g, 'goodbye', { enabled: true, thumbnail: 'none' });
    await svc.updateCardStyle(g, { background: '#ABCDEF', overlay: 0.456, accentColor: 'blurple' });
    await svc.updateMessage(g, 'dm', { text: 'Psst {user.name}' });
    const guild = fakeGuild(g, [r1, r2]);
    const executor = fakeMember(guild, 'OWNER', Date.now());
    await svc.addAutorole({ guild, executor, role: guild.roles.cache.get(r1), delay: '10m' });
    await svc.addAutorole({ guild, executor, role: guild.roles.cache.get(r2) });
    const joined = Date.now() - 60_000;
    await svc.assignAutorole(fakeMember(guild, u, joined));
    const before = Date.now();
    await svc.assignAutorole({ ...fakeMember(guild, p, joined), pending: true }, { force: true });
    const row = await settingsService.getSettings(g, { fresh: true });
    answer({
      joined, before, settings: row,
      job: jobJson(await scheduler.getJob(svc.autoroleJobKey(g, u))),
      watch: jobJson(await scheduler.getJob(svc.autoroleJobKey(g, p))),
    });
  } else if (cmd === 'welcome-read') {
    const [g, u, p] = args;
    const row = await settingsService.getSettings(g, { fresh: true });
    answer({
      settings: row,
      job: jobJson(await scheduler.getJob(svc.autoroleJobKey(g, u))),
      watch: p ? jobJson(await scheduler.getJob(svc.autoroleJobKey(g, p))) : null,
      cardEnabled: svc.welcomeCardEnabled(row),
      style: svc.parseCardStyle(row.welcomeCardStyle),
      welcome: svc.resolveTemplate('welcome', row),
      goodbye: svc.resolveTemplate('goodbye', row),
      dm: svc.resolveTemplate('dm', row),
    });
  } else if (cmd === 'app-write') {
    const [g, u, review] = args;
    const a = await app.createForm({ guildId: g, name: 'Staff', description: 'Join {server}', reviewChannelId: review });
    const b = await app.createForm({ guildId: g, name: 'a_b%', reviewChannelId: null });
    const clash = await app.createForm({ guildId: g, name: 'STAFF' });
    await app.addQuestion(g, a.form.id, { label: 'Why us?', style: 'PARAGRAPH', required: true, placeholder: 'tell us' });
    await app.addQuestion(g, a.form.id, { label: 'Age', style: 'SHORT', required: false });
    await app.addQuestion(g, a.form.id, { label: 'Timezone', style: 'SHORT', required: true });
    await app.changeRole(g, a.form.id, 'acceptRoleIds', '955008000000000301', false);
    await app.changeRole(g, a.form.id, 'requiredRoleIds', '955008000000000302', false);
    const upd = await app.updateForm(g, a.form.id, { cooldownHours: 2, acceptMessage: 'Welcome {user}!' });
    const form = await app.findForm(g, a.form.id);
    const draft = await app.startDraft({ form, userId: u, memberRoleIds: ['955008000000000302'] });
    answer({ staff: upd.form, other: b.form, clash, draft: draft.submission });
  } else if (cmd === 'app-read') {
    const [g, u, refs, queries] = args;
    const forms = await app.listForms(g);
    const found = {};
    for (const r of JSON.parse(refs)) found[r] = (await app.findForm(g, r))?.id ?? null;
    const auto = {};
    for (const q of JSON.parse(queries)) auto[q] = (await app.findFormsForAutocomplete(g, q)).map((f) => f.name);
    const subs = await prisma.applicationSubmission.findMany({ where: { guildId: g, userId: u }, orderBy: { createdAt: 'asc' } });
    answer({ forms, found, auto, pending: forms.length ? await app.pendingCount(forms[0].id) : 0, subs });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
