// Database access of the `automations` collection: the Automation rows /automation writes, through automationService's
// Discord-free writes (createAutomation: guild lock, the per-server cap and the button counter; mutateAutomation: row
// lock and the shape check; deleteAutomation), which also drop the automation cache in Redis (`guild:{g}:automations`).
// The bot memoises a server's automations for up to 60 s per shard, so a change reaches it within a minute.
// What /automation checks against the server is checked here too: saved embeds exist, reward modules are on, members
// exist (and are not bots where the command refuses them) and "Clicked this button" points at a real button. Channel and
// role ids (kinds, @everyone, roles the bot and the user can give) are checked by the dashboard server from the
// descriptors before a write reaches this store.
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { cache } from '../../services/redis.js';
import { getSettings, getSettingsSafe, isModuleEnabled } from '../../services/settingsService.js';
import { REWARD_MODULES } from '../../services/rewards.js';
import { getSavedEmbedTemplate } from '../../services/embedService.js';
import {
  AutomationError, buildPresetAutomation, createAutomation, deleteAutomation, isUniqueViolation, mutateAutomation,
  normalizeAction, normalizeAutomationName, normalizeCondition, normalizeList,
} from '../../services/automationService.js';
import { RecordError } from './errors.js';
import { isDiscordApiError } from './embeds.js';
import {
  ACTION_RULES, NEEDS_ACTIONS, SELF_REF_ID, actionToStored, automationItem, conditionToStored, planButtons,
  problemKey,
} from './automations.js';

const notFound = () => new RecordError('not_found', 'That automation no longer exists.');
const stale = () => new RecordError('conflict', 'Someone else changed this automation meanwhile. Reload it and try again.');
const asList = (v) => (Array.isArray(v) ? v : []);

/** A row with normalised arrays; invalid stored elements are dropped and listed (automationService's hydrate). */
function hydrate(row) {
  const c = normalizeList(row.conditions, 'condition');
  const a = normalizeList(row.actions, 'action');
  return { ...row, conditions: c.items, actions: a.items, invalidConditions: c.invalid, invalidActions: a.invalid };
}

const automationButtons = (actions) => asList(actions).filter((a) => a?.button && a.button.url == null && Number.isInteger(a.button.index))
  .map((a) => a.button.index);

/** Every automation of the server: id -> { name, buttons } (for button references and their names). */
async function loadRefs(guildId, rows = null) {
  const list = rows ?? await prisma.automation.findMany({
    where: { guildId }, orderBy: { createdAt: 'asc' }, take: LIMITS.AUTOMATIONS, select: { id: true, name: true, actions: true },
  });
  // Rows of list() are hydrated already; the select above still holds the raw JSON.
  return new Map(list.map((r) => [r.id, {
    name: r.name, buttons: new Set(automationButtons(r.invalidActions ? r.actions : normalizeList(r.actions, 'action').items)),
  }]));
}

// ── Problems (automationService.automationProblems, without the gateway client) ──

const rewardLabel = (kind) => (kind === 'xp' ? 'XP' : 'Coin');
const rewardModuleLabel = (kind) => (kind === 'xp' ? 'Levels' : 'Economy');

/** What the list needs to describe every row's problems: settings, saved embeds that exist, the last runtime problems. */
async function problemContext(guildId, rows, refs) {
  const embedNames = [...new Set(rows.flatMap((r) => r.actions.filter((a) => a.type === 'SEND_EMBED').map((a) => a.embedName)))];
  const [settings, embeds, last] = await Promise.all([
    getSettingsSafe(guildId),
    embedNames.length
      ? prisma.savedEmbed.findMany({ where: { guildId, name: { in: embedNames } }, select: { name: true } })
      : Promise.resolve([]),
    Promise.all(rows.map(async (r) => [r.id, await cache.get(problemKey(r.id))])),
  ]);
  return { settings, refs, embeds: new Set(embeds.map((e) => e.name)), last: new Map(last) };
}

/** The problems of one row that the database knows about (deleted channels and roles need Discord: checkWithDiscord). */
function storedProblems(row, pc) {
  const out = [];
  for (const inv of row.invalidConditions ?? []) out.push(`Condition ${inv.index} is invalid and ignored: ${inv.error}`);
  for (const inv of row.invalidActions ?? []) out.push(`Action ${inv.index} is invalid and ignored: ${inv.error}`);
  if (row.enabled && row.actions.length === 0) out.push('It is on but has no actions, so it never runs.');
  row.conditions.forEach((c, k) => {
    for (const b of c.buttons ?? []) {
      const ref = pc.refs.get(b.automationId);
      if (!ref) out.push(`Condition ${k + 1}: refers to a button of an automation that no longer exists.`);
      else if (!ref.buttons.has(b.index)) out.push(`Condition ${k + 1}: button ${b.index} no longer exists on "${ref.name}".`);
    }
  });
  row.actions.forEach((a, k) => {
    if (a.type === 'SEND_EMBED' && !pc.embeds.has(a.embedName)) out.push(`Action ${k + 1}: the saved embed "${a.embedName}" was not found.`);
    const kind = ACTION_RULES[a.type]?.reward;
    if (kind && pc.settings && !isModuleEnabled(pc.settings, REWARD_MODULES[kind])) {
      out.push(`Action ${k + 1}: ${rewardLabel(kind)} rewards need the ${rewardModuleLabel(kind)} module.`);
    }
  });
  const last = pc.last.get(row.id);
  if (last && typeof last.text === 'string') {
    const at = typeof last.at === 'string' && Number.isFinite(Date.parse(last.at)) ? ` (${new Date(last.at).toISOString().slice(0, 16).replace('T', ' ')} UTC)` : '';
    out.push(`Last problem${at}: ${last.text}`);
  }
  return out;
}

/** The problems only Discord can tell: deleted channels and roles, and roles the bot can no longer give. */
function discordProblems(row, res) {
  const out = [];
  const channels = new Map(res.channels.map((c) => [c.id, c]));
  const roles = new Map(res.roles.map((r) => [r.id, r]));
  const channelGone = (label, id) => {
    if (channels.has(id)) return false;
    out.push(`${label}: the channel ${id} was not found (deleted, or a thread the dashboard cannot see).`);
    return true;
  };
  const roleGone = (label, id) => {
    if (roles.has(id)) return false;
    out.push(`${label}: the role ${id} was deleted.`);
    return true;
  };
  row.conditions.forEach((c, k) => {
    const label = `Condition ${k + 1}`;
    for (const id of [...(c.channelIds ?? []), ...(c.categoryIds ?? [])]) channelGone(label, id);
    for (const id of c.roleIds ?? []) roleGone(label, id);
  });
  row.actions.forEach((a, k) => {
    const label = `Action ${k + 1}`;
    if (a.channelId) channelGone(label, a.channelId);
    for (const id of a.roleIds ?? []) {
      if (roleGone(label, id)) continue;
      const role = roles.get(id);
      // checkAssignableRole without an executor, in the bot's words.
      if (role.managed) out.push(`${label}: the role @${role.name} is managed by an integration.`);
      else if (!res.bot?.manageRoles) out.push(`${label}: the bot needs the Manage Roles permission.`);
      else if (!role.assignable) out.push(`${label}: the role @${role.name} is above the bot's highest role.`);
      else if (role.dangerous) out.push(`${label}: the role @${role.name} has moderator or admin permissions, so the bot will not give it.`);
    }
  });
  return out;
}

/** Display names of the creators (ctx.discord.getUsers, cached by the client); ids stay ids when Discord cannot answer. */
async function creatorNames(rows, ctx) {
  if (typeof ctx?.discord?.getUsers !== 'function') return new Map();
  try {
    const users = await ctx.discord.getUsers([...new Set(rows.map((r) => r.createdBy))]);
    const out = new Map();
    for (const [id, u] of users) {
      if (u?.username) out.set(id, u.globalName && u.globalName !== u.username ? `${u.globalName} (@${u.username})` : `@${u.username}`);
    }
    return out;
  } catch (err) {
    if (isDiscordApiError(err)) return new Map();
    throw err;
  }
}

async function items(guildId, rows, refs, ctx) {
  const [pc, creators] = await Promise.all([problemContext(guildId, rows, refs), creatorNames(rows, ctx)]);
  const nameOf = (id) => refs.get(id)?.name ?? null;
  return rows.map((row) => automationItem(row, { nameOf, problems: storedProblems(row, pc), creator: creators.get(row.createdBy) ?? null }));
}

async function getItem(guildId, id, ctx, extraProblems = []) {
  const row = await prisma.automation.findFirst({ where: { id, guildId } });
  if (!row) return null;
  const [item] = await items(guildId, [hydrate(row)], await loadRefs(guildId), ctx);
  if (extraProblems.length) item.problems = [...extraProblems, ...item.problems];
  return item;
}

// ── Checks against the server (the /automation add-time checks) ──────────────

function needDiscord(ctx, what) {
  if (typeof ctx?.discord?.[what] !== 'function') {
    throw new RecordError('unavailable', 'This cannot be checked with Discord from the dashboard right now.');
  }
  return ctx.discord;
}

async function discordCall(fn, what) {
  try {
    return await fn();
  } catch (err) {
    if (isDiscordApiError(err)) throw new RecordError('discord', `${what} could not be checked with Discord. Try again in a moment.`);
    throw err;
  }
}

/** A user id the form adds: it must be a member of the server (and a person, when `people`). */
async function checkMember(guildId, ctx, userId, path, { people = null } = {}) {
  const discord = needDiscord(ctx, 'searchMembers');
  const found = await discordCall(() => discord.searchMembers(guildId, userId), 'Members');
  const member = asList(found).find((m) => m?.id === userId);
  if (!member) throw new RecordError('invalid', 'That user is not in this server.', { path });
  if (people && member.bot) throw new RecordError('invalid', people, { path });
}

/**
 * The checks /automation action add and condition add make against the server, for what the form adds (entries the
 * stored automation already had are kept without a check, so an unrelated edit never fails on them).
 * @param {object[]|null} conditions form conditions (null: unchanged)
 * @param {object[]|null} actions form actions (null: unchanged)
 * @param {object} before the stored row (hydrated), or empty lists on create
 */
async function checkAgainstServer(guildId, ctx, { conditions, actions, before }) {
  const had = (list, key) => new Set(asList(list).flatMap(key));
  const oldEmbeds = had(before.actions, (a) => (a.type === 'SEND_EMBED' ? [a.embedName] : []));
  const oldRewards = had(before.actions, (a) => (ACTION_RULES[a.type]?.reward ? [`${a.type}:${a.amount}`] : []));
  const oldThreadUsers = had(before.actions, (a) => (a.type === 'ADD_TO_THREAD' ? a.userIds : []));
  const oldUsers = had(before.conditions, (c) => (c.type === 'USER_IS' ? c.userIds : []));

  let settings = null;
  for (const [i, a] of asList(actions).entries()) {
    const at = `actions.${i}`;
    if (a.type === 'SEND_EMBED') {
      const name = normalizeAutomationName(a.embedName);
      if (!oldEmbeds.has(name) && !(await getSavedEmbedTemplate(guildId, name))) {
        throw new RecordError('invalid', `There is no saved embed called "${name}". Create it under Embed Messages first.`, { path: `${at}.embedName` });
      }
    }
    const kind = ACTION_RULES[a.type]?.reward;
    if (kind && !oldRewards.has(`${a.type}:${a.amount}`)) {
      settings ??= await getSettings(guildId);
      if (!isModuleEnabled(settings, REWARD_MODULES[kind])) {
        throw new RecordError('invalid', `${rewardLabel(kind)} rewards need the ${rewardModuleLabel(kind)} module. Turn it on first.`, { path: `${at}.type` });
      }
    }
    if (a.type === 'ADD_TO_THREAD') {
      for (const [k, u] of asList(a.users).entries()) {
        if (oldThreadUsers.has(u.userId)) continue;
        await checkMember(guildId, ctx, u.userId, `${at}.users.${k}.userId`, { people: 'Bots cannot be added to threads by automations.' });
      }
    }
  }
  for (const [i, c] of asList(conditions).entries()) {
    if (c.type !== 'USER_IS') continue;
    for (const [k, u] of asList(c.users).entries()) {
      if (!oldUsers.has(u.userId)) await checkMember(guildId, ctx, u.userId, `conditions.${i}.users.${k}.userId`);
    }
  }
}

/**
 * Resolves the "Clicked this button" references of the form: an automation name (this automation's own name, old or
 * new, means itself) or an id the stored condition already had. Buttons of other automations must exist.
 * @returns {object[]} the conditions in the stored shape (not normalised yet); self references carry `selfId`
 */
function resolveConditions(conditions, { refs, selfNames, selfId, before }) {
  const byName = new Map([...refs].map(([id, r]) => [r.name, id]));
  const oldRefs = new Set(asList(before.conditions).flatMap((c) => (c.type === 'BUTTON_IS' ? c.buttons.map((b) => `${b.automationId}:${b.index}`) : [])));
  return conditions.map((c, i) => {
    if (c.type !== 'BUTTON_IS') return conditionToStored(c);
    const ids = c.buttons.map((b, k) => {
      const path = `conditions.${i}.buttons.${k}`;
      const text = String(b.automation ?? '').trim();
      const name = normalizeAutomationName(text);
      let id;
      if (name && selfNames.includes(name)) id = selfId;
      else if (name && byName.has(name)) id = byName.get(name);
      else if (refs.has(text) || oldRefs.has(`${text}:${b.index}`)) id = text;
      else throw new RecordError('invalid', `There is no automation called "${text}".`, { path: `${path}.automation` });
      if (id !== selfId && !oldRefs.has(`${id}:${b.index}`) && !refs.get(id)?.buttons.has(b.index)) {
        throw new RecordError('invalid', `"${refs.get(id)?.name ?? text}" has no button number ${b.index}.`, { path: `${path}.index` });
      }
      return id;
    });
    let k = 0;
    return conditionToStored(c, () => ids[k++]);
  });
}

/** A self reference must name one of this automation's own automation buttons (after the new ones got their numbers). */
function checkSelfButtons(conditions, actions, { selfId, before }) {
  const own = new Set(automationButtons(actions));
  const oldRefs = new Set(asList(before.conditions).flatMap((c) => (c.type === 'BUTTON_IS' ? c.buttons.map((b) => `${b.automationId}:${b.index}`) : [])));
  conditions.forEach((c, i) => {
    (c.buttons ?? []).forEach((b, k) => {
      if (b.automationId !== selfId || own.has(b.index) || oldRefs.has(`${selfId}:${b.index}`)) return;
      throw new RecordError('invalid', `This automation has no button number ${b.index}.`, { path: `conditions.${i}.buttons.${k}.index` });
    });
  });
}

/** automationService's normalisers, one element at a time, so a refusal points at its item. */
function normalizeEach(list, kind) {
  return list.map((raw, i) => {
    const r = kind === 'condition' ? normalizeCondition(raw) : normalizeAction(raw);
    if (!r.ok) throw new RecordError('invalid', `${kind === 'condition' ? 'Condition' : 'Action'} ${i + 1}: ${r.error}`, { path: `${kind}s.${i}` });
    return r.value;
  });
}

/** The trigger, conditions and actions of a /automation create preset, with the command's checks. */
async function presetShape(guildId, data, ctx) {
  const opts = {};
  if (data.preset === 'welcomer' || data.preset === 'starboard') {
    const discord = needDiscord(ctx, 'getGuildResources');
    const res = await discordCall(() => discord.getGuildResources(guildId), 'The channel');
    const channel = res.channels.find((c) => c.id === data.presetChannelId);
    if (channel && !['text', 'announcement'].includes(channel.kind)) {
      throw new RecordError('invalid', 'Pick a text or announcement channel.', { path: 'presetChannelId' });
    }
    if (data.preset === 'welcomer') {
      Object.assign(opts, { channelId: data.presetChannelId, message: data.presetMessage?.trim() || null, roleId: data.presetRoleId ?? null });
    } else {
      Object.assign(opts, { channelId: data.presetChannelId, emoji: data.presetEmoji ?? null, threshold: data.presetThreshold });
    }
  } else {
    await checkMember(guildId, ctx, data.presetUserId, 'presetUserId', { people: 'Pick a person, not a bot.' });
    Object.assign(opts, { userId: data.presetUserId, channelId: data.presetChannelId ?? null });
  }
  try {
    return buildPresetAutomation(data.preset, opts);
  } catch (err) {
    if (err instanceof AutomationError) throw new RecordError('invalid', err.message, { path: data.preset === 'starboard' ? 'presetEmoji' : 'preset' });
    throw err;
  }
}

/** Maps a failed automationService write to the error the admin sees. */
function writeFailure(err, name) {
  if (err instanceof RecordError) return err;
  if (isUniqueViolation(err)) return new RecordError('conflict', `An automation called "${name}" already exists.`, { path: 'name' });
  if (err instanceof AutomationError) {
    if (err.code === 'LIMIT') return new RecordError('limit', `This server already has ${LIMITS.AUTOMATIONS} automations, the maximum. Delete one first.`);
    if (err.code === 'NAME') return new RecordError('invalid', err.message, { path: 'name' });
    if (err.code === 'BUTTONS') return new RecordError('invalid', err.message, { path: 'actions' });
    return new RecordError('invalid', err.message);
  }
  return err;
}

function cleanName(raw) {
  const name = normalizeAutomationName(raw);
  if (!name || name !== raw) throw new RecordError('invalid', 'Automation names can use letters, numbers, - and _ (up to 32 characters).', { path: 'name' });
  return name;
}

const EMPTY = Object.freeze({ conditions: [], actions: [] });

export const store = Object.freeze({
  async list(guildId, _opts, ctx) {
    const rows = (await prisma.automation.findMany({ where: { guildId }, orderBy: { createdAt: 'asc' }, take: LIMITS.AUTOMATIONS })).map(hydrate);
    return items(guildId, rows, await loadRefs(guildId, rows), ctx);
  },

  get: (guildId, id, ctx) => getItem(guildId, id, ctx),

  async create(guildId, data, ctx) {
    const name = cleanName(data.name);
    let shape;
    if (data.preset && data.preset !== 'none') {
      shape = await presetShape(guildId, data, ctx);
    } else {
      const refs = await loadRefs(guildId);
      await checkAgainstServer(guildId, ctx, { conditions: data.conditions, actions: data.actions, before: EMPTY });
      const plan = planButtons(data.actions, { existing: [], buttonSeq: 0 });
      if (plan.error) throw new RecordError('invalid', plan.error.message, { path: plan.error.path });
      const conditions = normalizeEach(resolveConditions(data.conditions, { refs, selfNames: [name], selfId: SELF_REF_ID, before: EMPTY }), 'condition');
      const actions = normalizeEach(data.actions.map((a, i) => actionToStored(a, plan.indexes[i])), 'action');
      checkSelfButtons(conditions, actions, { selfId: SELF_REF_ID, before: EMPTY });
      shape = { trigger: data.trigger, conditions, actions };
    }
    if (data.enabled && shape.actions.length === 0) throw new RecordError('invalid', NEEDS_ACTIONS, { path: 'enabled' });
    let row;
    try {
      row = await createAutomation(guildId, { name, ...shape, enabled: data.enabled, cooldownSeconds: data.cooldownSeconds },
        String(ctx?.userId ?? '0'), { selfRefId: SELF_REF_ID });
    } catch (err) {
      throw writeFailure(err, name);
    }
    return (await getItem(guildId, row.id, ctx)) ?? automationItem(row);
  },

  async update(guildId, id, data, ctx) {
    const row = await prisma.automation.findFirst({ where: { id, guildId } });
    if (!row) throw notFound();
    const before = hydrate(row);
    const name = Object.hasOwn(data, 'name') ? cleanName(data.name) : row.name;
    const hasConditions = Object.hasOwn(data, 'conditions');
    const hasActions = Object.hasOwn(data, 'actions');

    let conditions = null;
    if (hasConditions || hasActions) {
      await checkAgainstServer(guildId, ctx, {
        conditions: hasConditions ? data.conditions : null, actions: hasActions ? data.actions : null, before,
      });
    }
    if (hasConditions) {
      const refs = await loadRefs(guildId);
      conditions = normalizeEach(resolveConditions(data.conditions, { refs, selfNames: [row.name, name], selfId: id, before }), 'condition');
    }

    let saved;
    try {
      ({ row: saved } = await mutateAutomation(guildId, row.name, (cur) => {
        // Optimistic, like the other dashboard lists: the form was filled from `row`, so a concurrent change is reported.
        if (cur.id !== id || cur.updatedAt?.getTime() !== row.updatedAt?.getTime()) throw stale();
        const out = {};
        if (name !== cur.name) out.name = name;
        if (Object.hasOwn(data, 'trigger') && data.trigger !== cur.trigger) out.trigger = data.trigger;
        if (Object.hasOwn(data, 'cooldownSeconds') && data.cooldownSeconds !== cur.cooldownSeconds) out.cooldownSeconds = data.cooldownSeconds;
        let actions = cur.actions;
        if (hasActions) {
          const plan = planButtons(data.actions, { existing: automationButtons(cur.actions), buttonSeq: cur.buttonSeq });
          if (plan.error) throw new RecordError('invalid', plan.error.message, { path: plan.error.path });
          actions = normalizeEach(data.actions.map((a, i) => actionToStored(a, plan.indexes[i])), 'action');
          out.actions = actions;
          if (plan.buttonSeq > (cur.buttonSeq ?? 0)) out.buttonSeq = plan.buttonSeq;
        }
        if (conditions) out.conditions = conditions;
        checkSelfButtons(conditions ?? cur.conditions, actions, { selfId: id, before: cur });
        // On needs an action (/automation enable); emptying the actions turns it off (/automation action remove).
        const enabled = Object.hasOwn(data, 'enabled') ? data.enabled : cur.enabled;
        if (enabled && actions.length === 0) {
          if (data.enabled === true) throw new RecordError('invalid', NEEDS_ACTIONS, { path: 'enabled' });
          if (cur.enabled) out.enabled = false;
        } else if (enabled !== cur.enabled) out.enabled = enabled;
        return { data: out };
      }));
    } catch (err) {
      if (err instanceof AutomationError && err.code === 'NOTFOUND') {
        // Renamed or deleted since it was read.
        if (await prisma.automation.findFirst({ where: { id, guildId }, select: { id: true } })) throw stale();
        throw notFound();
      }
      throw writeFailure(err, name);
    }
    return (await getItem(guildId, id, ctx)) ?? automationItem(hydrate(saved));
  },

  async remove(guildId, id) {
    // As /automation delete: buttons it already posted stop doing anything unless another automation listens for them.
    const count = await deleteAutomation(guildId, id);
    if (!count) throw notFound();
  },

  actions: Object.freeze({
    /** /automation info's problem list, with the channel and role checks done against Discord. */
    async review(guildId, id, _data, ctx) {
      const row = await prisma.automation.findFirst({ where: { id, guildId } });
      if (!row) throw notFound();
      const discord = needDiscord(ctx, 'getGuildResources');
      const res = await discordCall(() => discord.getGuildResources(guildId, { fresh: true }), 'Channels and roles');
      const item = await getItem(guildId, id, ctx, discordProblems(hydrate(row), res));
      if (!item) throw notFound();
      const n = item.problems.length;
      const message = n
        ? `${n} problem${n === 1 ? '' : 's'}: ${item.problems.join(' ')}`.slice(0, 1500)
        : 'No problems found.';
      return { item, message };
    },
  }),
});
