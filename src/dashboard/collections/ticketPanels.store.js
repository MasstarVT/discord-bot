// Database access of the `ticketPanels` collection: the same TicketPanel / TicketPanelOption rows as /ticket-panel
// (src/services/ticketService.js). Panels and settings are written through the service's Discord-free functions
// (createPanel, updatePanelFields) and, for the whole option list at once, in one transaction under the same advisory lock the
// service's addOption / updateOption / removeOption use. What needs the gateway client is a task run in the bot
// (src/services/dashboardTasks/tickets.js):
//   ticket.check    {categoryIds, transcriptChannelId}  the bot's permissions in new categories / transcript channel (before saving)
//   ticket.publish  {panelId, channelId}                post the panel message, or move / update it (publishPanel)
//   ticket.refresh  {panelId, ...}                      update the posted message (refreshPanelMessage) and back-fill manager roles
//   ticket.delete   {panelId}                           delete the panel and its message (deletePanel)
// A change is saved first and published after: if the bot cannot publish it (missing permission, bot offline) the change stays
// and the admin is told to retry with the "Publish" or "Update posted message" action.
import { Prisma } from '@prisma/client';
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import {
  createPanel, invalidatePanel, parseOptionEmoji, templateUsesToken, updatePanelFields, validateNameFormat,
} from '../../services/ticketService.js';
import { normalizeTemplate } from '../../utils/customEmbed.js';
import { RecordError, taskOutcome } from './errors.js';
import { isDiscordApiError, memberFor, pingRoleProblem } from './embeds.js';
import {
  DEFAULT_INTRO_TEMPLATE, collection, defaultPanelTemplate, diffOptions, isDefaultTemplate, isEmptyTemplate, normalizeOptions, panelItem,
  toJson,
} from './ticketPanels.js';

const lockSql = (key) => Prisma.sql`SELECT 1 AS ok FROM (SELECT pg_advisory_xact_lock(hashtext(${key}))) AS l`;

/** Raw statements (test/db.test.js runs each one). */
export const RAW_SQL = Object.freeze([
  { name: 'dashboard.ticketPanels.lockOptions', build: () => lockSql('ticket-option:0') },
]);

const OPTION_ORDER = [{ position: 'asc' }, { id: 'asc' }];
const ACTIVE = ['OPEN', 'CLAIMED', 'CLOSED'];
const CUSTOM_EMOJI_ID_RE = /:(\d{17,20})>$/;
const BUTTON_TEMPLATES = [defaultPanelTemplate('BUTTONS'), defaultPanelTemplate('SELECT')];

const notFound = () => new RecordError('not_found', 'That ticket panel no longer exists.');

function requireTasks(ctx) {
  if (typeof ctx?.runTask !== 'function') throw new RecordError('unavailable', 'Panels cannot be published from the dashboard right now.');
}

/**
 * The server's roles and emojis. A custom emoji the cached list does not know may be a few seconds old: the list is read
 * again, fresh, before the emoji is refused.
 */
async function loadResources(ctx, guildId, options = []) {
  if (typeof ctx?.discord?.getGuildResources !== 'function') {
    throw new RecordError('unavailable', 'The roles and emojis of this server cannot be checked from the dashboard right now.');
  }
  try {
    let resources = await ctx.discord.getGuildResources(guildId);
    const wanted = options.map((o) => CUSTOM_EMOJI_ID_RE.exec(o.emoji ?? '')?.[1]).filter(Boolean);
    if (wanted.some((id) => !(resources.emojis ?? []).some((e) => e.id === id))) resources = await ctx.discord.getGuildResources(guildId, { fresh: true });
    return resources;
  } catch (err) {
    if (isDiscordApiError(err)) throw new RecordError('discord', 'The server could not be loaded from Discord. Try again in a moment.');
    throw err;
  }
}

/** A Unicode option emoji must be one the bot can put on a button (parseOptionEmoji: exactly one emoji). */
function assertUsableEmojis(options) {
  options.forEach((o, i) => {
    if (!o.emoji || CUSTOM_EMOJI_ID_RE.test(o.emoji)) return;
    const parsed = parseOptionEmoji(o.emoji);
    if (!parsed.ok) throw new RecordError('invalid', parsed.error, { path: `options.${i}.emoji` });
  });
}

const loadRow = (guildId, id) => prisma.ticketPanel.findFirst({ where: { id, guildId }, include: { options: { orderBy: OPTION_ORDER } } });

async function getItem(guildId, id) {
  const row = await loadRow(guildId, id);
  if (!row) return null;
  return panelItem(row, await prisma.ticket.count({ where: { guildId, panelId: id, status: { in: ACTIVE } } }));
}

/** Runs a task; a refusal after the change was saved becomes "saved, but ..." (the change stays). */
async function runAfterSave(ctx, kind, payload, lead) {
  try {
    return taskOutcome(await ctx.runTask(kind, payload));
  } catch (err) {
    if (err instanceof RecordError && (err.code === 'failed' || err.code === 'unavailable')) throw new RecordError(err.code, `${lead} ${err.message}`);
    throw err;
  }
}

/** The stored form of a message: empty or the built-in default reads as "not set" (null); a panel message has no link buttons. */
function storedTemplate(template, defaults, { stripButtons }) {
  if (isEmptyTemplate(template) || isDefaultTemplate(template, defaults)) return null;
  return toJson(stripButtons ? { ...normalizeTemplate(template), buttons: [] } : template);
}

const sameJson = (a, b) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
const asJsonColumn = (v) => (v === null ? Prisma.DbNull : v);

/** Manager roles that still exist, once each (like updateManagers, which prunes deleted roles). */
function liveManagers(ids, resources) {
  const known = new Set((resources.roles ?? []).filter((r) => !r.everyone).map((r) => r.id));
  return [...new Set(ids)].filter((id) => known.has(id));
}

/** Configuration time: a {managers} ping must be something the admin could send themselves (the /ticket-panel rule). */
function assertManagersPingable(ctx, resources, managerIds, introTemplates) {
  if (managerIds.length === 0 || !introTemplates.some((t) => t && templateUsesToken(t, 'managers'))) return;
  const member = memberFor(ctx, resources);
  for (const id of managerIds) {
    const problem = pingRoleProblem(resources, id, member);
    if (problem) {
      throw new RecordError('invalid',
        `A ticket intro of this panel pings {managers}, and one manager role cannot be pinged by you. ${problem}`, { path: 'managerRoleIds' });
    }
  }
}

/** Asks the bot whether it may use the categories and the transcript channel (the checks /ticket-panel makes before saving). */
async function precheck(ctx, { categoryIds, transcriptChannelId }) {
  const ids = [...new Set(categoryIds.filter(Boolean))];
  if (ids.length === 0 && !transcriptChannelId) return;
  requireTasks(ctx);
  taskOutcome(await ctx.runTask('ticket.check', { categoryIds: ids, transcriptChannelId: transcriptChannelId ?? null }));
}

/** Writes the whole option list in one transaction (the service's option lock). Returns the plan that was applied. */
async function replaceOptions(guildId, panelId, next) {
  if (next.length < 1) throw new RecordError('invalid', 'A panel needs at least one option.', { path: 'options' });
  if (next.length > LIMITS.TICKET_PANEL_OPTIONS) {
    throw new RecordError('limit', `A panel can have at most ${LIMITS.TICKET_PANEL_OPTIONS} options.`, { path: 'options' });
  }
  try {
    return await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(lockSql(`ticket-option:${panelId}`));
      if (!await tx.ticketPanel.findFirst({ where: { id: panelId, guildId }, select: { id: true } })) throw notFound();
      const stored = await tx.ticketPanelOption.findMany({ where: { panelId }, orderBy: OPTION_ORDER });
      const plan = diffOptions(stored, next);
      // Tickets of a removed option keep working with optionId null (the foreign key sets it).
      if (plan.remove.length) await tx.ticketPanelOption.deleteMany({ where: { id: { in: plan.remove.map((r) => r.id) }, panelId } });
      for (const u of plan.update) {
        const data = { ...u.data };
        if ('introTemplate' in data) data.introTemplate = asJsonColumn(data.introTemplate);
        await tx.ticketPanelOption.update({ where: { id: u.id }, data });
      }
      for (const c of plan.create) {
        await tx.ticketPanelOption.create({ data: { ...c, introTemplate: asJsonColumn(c.introTemplate), panelId } });
      }
      return plan;
    }, { timeout: 10_000 });
  } finally {
    invalidatePanel(panelId);
  }
}

/** An option change the posted message shows (anything but the category and the intro message). */
const showsOnPanel = (plan) => plan.create.length > 0 || plan.remove.length > 0
  || plan.update.some((u) => Object.keys(u.data).some((k) => k !== 'categoryId' && k !== 'introTemplate'));

const customIds = (options) => new Set((options ?? []).map((o) => CUSTOM_EMOJI_ID_RE.exec(o.emoji ?? '')?.[1]).filter(Boolean));

export const store = Object.freeze({
  async list(guildId) {
    const [rows, counts] = await Promise.all([
      prisma.ticketPanel.findMany({ where: { guildId }, include: { options: { orderBy: OPTION_ORDER } }, orderBy: { name: 'asc' }, take: LIMITS.TICKET_PANELS }),
      prisma.ticket.groupBy({ by: ['panelId'], where: { guildId, status: { in: ACTIVE } }, _count: { _all: true } }),
    ]);
    const byPanel = new Map(counts.map((c) => [c.panelId, c._count._all]));
    return rows.map((row) => panelItem(row, byPanel.get(row.id) ?? 0));
  },

  get: getItem,

  async create(guildId, data, ctx) {
    if (data.channelId) requireTasks(ctx);
    const format = validateNameFormat(data.channelNameFormat);
    if (!format.ok) throw new RecordError('invalid', format.error, { path: 'channelNameFormat' });
    const resources = await loadResources(ctx, guildId, data.options);
    const options = normalizeOptions(data.options, resources);
    assertUsableEmojis(options);
    const managerRoleIds = liveManagers(data.managerRoleIds, resources);
    const template = storedTemplate(data.template, BUTTON_TEMPLATES, { stripButtons: true });
    const introTemplate = storedTemplate(data.introTemplate, [DEFAULT_INTRO_TEMPLATE], { stripButtons: false });
    assertManagersPingable(ctx, resources, managerRoleIds, [introTemplate, ...options.map((o) => o.introTemplate)]);
    await precheck(ctx, {
      categoryIds: [data.openCategoryId, data.claimedCategoryId, data.closedCategoryId, ...options.map((o) => o.categoryId)],
      transcriptChannelId: data.transcriptChannelId,
    });

    const made = await createPanel({
      guildId, name: data.name, style: data.style, askReason: data.askReason, maxOpenPerUser: data.maxOpenPerUser, channelNameFormat: format.format,
    });
    if (!made.ok) {
      if (made.code === 'PANEL_LIMIT') throw new RecordError('limit', `This server already has ${LIMITS.TICKET_PANELS} ticket panels, the maximum. Delete one first.`);
      throw new RecordError('conflict', `A panel called "${data.name}" already exists.`, { path: 'name' });
    }
    const id = made.panel.id;
    try {
      const r = await updatePanelFields(id, guildId, {
        managerRoleIds,
        openCategoryId: data.openCategoryId ?? null,
        claimedCategoryId: data.claimedCategoryId ?? null,
        closedCategoryId: data.closedCategoryId ?? null,
        transcriptChannelId: data.transcriptChannelId ?? null,
        dmTranscript: data.dmTranscript === true,
        template: asJsonColumn(template),
        introTemplate: asJsonColumn(introTemplate),
      });
      if (!r.ok) throw notFound();
      await replaceOptions(guildId, id, options);
    } catch (err) {
      // Nothing is half-made: the panel goes again, then the error.
      await prisma.ticketPanel.deleteMany({ where: { id, guildId } }).catch(() => {});
      invalidatePanel(id);
      throw err;
    }
    ctx.logger?.info?.(`Dashboard ticket panel ${id} created in ${guildId} by ${ctx.userId}`);

    if (data.channelId) {
      await runAfterSave(ctx, 'ticket.publish', { panelId: id, channelId: data.channelId },
        'The panel was saved but could not be published yet. Fix the problem, then use "Publish" on it.');
    }
    return getItem(guildId, id);
  },

  async update(guildId, id, data, ctx) {
    requireTasks(ctx);
    const row = await loadRow(guildId, id);
    if (!row) throw notFound();
    // The cross-field rules again, against the row as it is now (the request was validated against an earlier read).
    const problem = collection.check(data, { mode: 'update', current: panelItem(row) });
    if (problem) throw new RecordError('invalid', problem.message, { path: problem.path });
    const has = (key) => Object.hasOwn(data, key);

    const patch = {};
    for (const key of ['name', 'style', 'askReason', 'dmTranscript', 'maxOpenPerUser', 'openCategoryId', 'claimedCategoryId', 'closedCategoryId', 'transcriptChannelId']) {
      if (has(key) && (data[key] ?? null) !== (row[key] ?? null)) patch[key] = data[key] ?? null;
    }
    if (has('channelNameFormat')) {
      const format = validateNameFormat(data.channelNameFormat);
      if (!format.ok) throw new RecordError('invalid', format.error, { path: 'channelNameFormat' });
      if (format.format !== row.channelNameFormat) patch.channelNameFormat = format.format;
    }
    let template;
    if (has('template')) {
      template = storedTemplate(data.template, BUTTON_TEMPLATES, { stripButtons: true });
      if (!sameJson(template, row.template ? toJson(row.template) : null)) patch.template = asJsonColumn(template);
    }
    let introTemplate;
    if (has('introTemplate')) {
      introTemplate = storedTemplate(data.introTemplate, [DEFAULT_INTRO_TEMPLATE], { stripButtons: false });
      if (!sameJson(introTemplate, row.introTemplate ? toJson(row.introTemplate) : null)) patch.introTemplate = asJsonColumn(introTemplate);
    }

    // Roles, emojis and the ping rule need the server's roles and emojis.
    const needResources = has('managerRoleIds') || has('options') || 'introTemplate' in patch;
    const resources = needResources ? await loadResources(ctx, guildId, data.options ?? []) : null;
    let managersAdded = [];
    let managersRemoved = [];
    let managers = row.managerRoleIds;
    if (has('managerRoleIds')) {
      managers = liveManagers(data.managerRoleIds, resources);
      const live = new Set(liveManagers(row.managerRoleIds, resources));
      managersAdded = managers.filter((r) => !row.managerRoleIds.includes(r));
      managersRemoved = [...live].filter((r) => !managers.includes(r));
      if (managers.length !== row.managerRoleIds.length || managersAdded.length > 0) patch.managerRoleIds = managers;
    }
    let options = null;
    if (has('options')) {
      options = normalizeOptions(data.options, resources, customIds(row.options));
      assertUsableEmojis(options);
    }

    // {managers} in an intro: the roles it pings must be pingable by the admin (when the intros or the roles change).
    const introsNow = [introTemplate === undefined ? row.introTemplate : introTemplate,
      ...(options ? options.map((o) => o.introTemplate) : row.options.map((o) => o.introTemplate))];
    if ('introTemplate' in patch || options || managersAdded.length > 0) {
      assertManagersPingable(ctx, resources, 'introTemplate' in patch || options ? managers : managersAdded, introsNow);
    }

    // The bot must be able to use any category or transcript channel that is new.
    const had = new Set([row.openCategoryId, row.claimedCategoryId, row.closedCategoryId, ...row.options.map((o) => o.categoryId)].filter(Boolean));
    const wanted = [patch.openCategoryId, patch.claimedCategoryId, patch.closedCategoryId, ...(options ?? []).map((o) => o.categoryId)];
    await precheck(ctx, {
      categoryIds: wanted.filter((c) => c && !had.has(c)),
      transcriptChannelId: patch.transcriptChannelId && patch.transcriptChannelId !== row.transcriptChannelId ? patch.transcriptChannelId : null,
    });

    const publishTo = has('channelId') && data.channelId && data.channelId !== row.channelId ? data.channelId : null;

    if (Object.keys(patch).length > 0) {
      const r = await updatePanelFields(id, guildId, patch);
      if (!r.ok) {
        if (r.code === 'NAME_TAKEN') throw new RecordError('conflict', `A panel called "${patch.name}" already exists.`, { path: 'name' });
        throw notFound();
      }
    }
    let plan = null;
    if (options) plan = await replaceOptions(guildId, id, options);
    ctx.logger?.info?.(`Dashboard ticket panel ${id} updated in ${guildId} by ${ctx.userId}`);

    // The posted message follows the style, the panel message and the options; a move re-posts it anyway.
    const published = Boolean(row.channelId && row.messageId);
    const refreshMessage = published && !publishTo && Boolean('style' in patch || 'template' in patch || (plan && showsOnPanel(plan)));
    const managersChanged = managersAdded.length + managersRemoved.length > 0;
    if (publishTo) {
      await runAfterSave(ctx, 'ticket.publish', { panelId: id, channelId: publishTo },
        'Your changes were saved, but the panel was not published in the new channel. Use "Publish" to try again.');
    }
    if (refreshMessage || managersChanged) {
      await runAfterSave(ctx, 'ticket.refresh', { panelId: id, message: refreshMessage, managersAdded, managersRemoved },
        'Your changes were saved, but the published panel was not fully updated. Use "Update posted message" to try again.');
    }
    return getItem(guildId, id);
  },

  async remove(guildId, id, ctx) {
    const row = await loadRow(guildId, id);
    if (!row) throw notFound();
    if (row.channelId && row.messageId) {
      requireTasks(ctx);
      taskOutcome(await ctx.runTask('ticket.delete', { panelId: id }));
    } else {
      // Nothing is published: there is no message to delete, so the bot is not needed. Open tickets keep their channels.
      await prisma.ticketPanel.deleteMany({ where: { id, guildId } });
      invalidatePanel(id);
    }
    ctx.logger?.info?.(`Dashboard ticket panel ${id} deleted in ${guildId} by ${ctx.userId}`);
  },

  actions: Object.freeze({
    /** Posts the panel message in a channel (or moves / updates it there): `{ channelId }`. */
    async publish(guildId, id, data, ctx) {
      requireTasks(ctx);
      if (!await loadRow(guildId, id)) throw notFound();
      const out = taskOutcome(await ctx.runTask('ticket.publish', { panelId: id, channelId: data.channelId }));
      return { item: await getItem(guildId, id), message: out.message };
    },

    /** Applies the saved panel to the published message again. */
    async refresh(guildId, id, data, ctx) {
      requireTasks(ctx);
      if (!await loadRow(guildId, id)) throw notFound();
      const out = taskOutcome(await ctx.runTask('ticket.refresh', { panelId: id }));
      return { item: await getItem(guildId, id), message: out.message };
    },
  }),
});
