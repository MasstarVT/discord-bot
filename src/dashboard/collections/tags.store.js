// Database access of the `tags` collection (custom commands): the CustomTag rows /tag writes, through the same
// tagService helpers (createTagRow takes the per-guild advisory lock and the cap; normalizeActions/syncLegacyColumns
// keep the content/useEmbed mirror columns). Role actions and ping roles follow the /tag rules for the dashboard user.
// The bot memoises the enabled-name index for up to 60 s per shard (invalidateTagIndex is in the bot's memory), so a
// new, renamed or disabled command reaches prefix use within a minute.
import prisma from '../../database/client.js';
import { ensureGuildSettings } from '../../services/settingsService.js';
import {
  TagError, clearTagCooldowns, createTagRow, getEffectiveActions, syncLegacyColumns, validateNewTagName,
} from '../../services/tagService.js';
import { RecordError } from './errors.js';
import { assignRoleProblem, isDiscordApiError, memberFor, pingRoleProblem } from './embeds.js';
import { actionToStored, tagItem } from './tags.js';
import { LIMITS } from '../../config/constants.js';

const notFound = () => new RecordError('not_found', 'That custom command no longer exists.');

const toItem = (row) => tagItem(row, getEffectiveActions(row));

async function findRow(guildId, id) {
  const row = await prisma.customTag.findFirst({ where: { id, guildId } });
  if (!row) throw notFound();
  return row;
}

async function loadResources(ctx, guildId) {
  if (typeof ctx?.discord?.getGuildResources !== 'function') {
    throw new RecordError('unavailable', 'Roles cannot be checked from the dashboard right now.');
  }
  try {
    return await ctx.discord.getGuildResources(guildId);
  } catch (err) {
    if (isDiscordApiError(err)) throw new RecordError('discord', 'The server\'s roles could not be loaded from Discord. Try again in a moment.');
    throw err;
  }
}

/**
 * The /tag action add role rules for every role the form adds: role actions need a role the dashboard user could
 * hand out (assignRoleProblem), ping roles must be pingable by them (pingRoleProblem). Roles the stored command
 * already had are kept without a check, so an unrelated edit never fails on them.
 */
async function checkRoles(guildId, actions, before, ctx) {
  const oldRoles = new Set(before.filter((a) => a.roleId).map((a) => a.roleId));
  const oldPings = new Set(before.flatMap((a) => a.pingRoleIds ?? []));
  const checks = [];
  actions.forEach((a, i) => {
    if (a.type === 'addRole' || a.type === 'removeRole' || a.type === 'toggleRole') {
      if (!oldRoles.has(a.roleId)) {
        checks.push({ path: `actions.${i}.roleId`, run: (res) => assignRoleProblem(res, a.roleId, memberFor(ctx, res)) });
      }
    } else if (a.type !== 'dm') {
      (a.pingRoleIds ?? []).forEach((id, k) => {
        if (oldPings.has(id)) return;
        checks.push({ path: `actions.${i}.pingRoleIds.${k}`, run: (res) => pingRoleProblem(res, id, memberFor(ctx, res)) });
      });
    }
  });
  if (!checks.length) return;
  const resources = await loadResources(ctx, guildId);
  for (const c of checks) {
    const problem = c.run(resources);
    if (problem) throw new RecordError('invalid', problem, { path: c.path });
  }
}

/** The columns written for a form action list: the actions JSON plus the legacy mirror columns. */
function actionColumns(formActions) {
  const stored = formActions.map(actionToStored);
  const mirror = syncLegacyColumns(stored); // {} when there is no message action
  return { actions: stored, content: mirror.content ?? '', useEmbed: mirror.useEmbed ?? false };
}

const COLUMNS = Object.freeze([
  'description', 'enabled', 'allowRoleIds', 'denyRoleIds', 'allowChannelIds', 'denyChannelIds', 'deleteInvocation',
]);

export const store = Object.freeze({
  async list(guildId) {
    const rows = await prisma.customTag.findMany({ where: { guildId }, orderBy: { name: 'asc' }, take: LIMITS.CUSTOM_TAGS });
    return rows.map(toItem);
  },

  async get(guildId, id) {
    const row = await prisma.customTag.findFirst({ where: { id, guildId } });
    return row ? toItem(row) : null;
  },

  async create(guildId, data, ctx) {
    const v = validateNewTagName(data.name);
    if (!v.ok || v.name !== data.name) throw new RecordError('invalid', v.reason ?? 'That name is not valid.', { path: 'name' });
    await checkRoles(guildId, data.actions, [], ctx);
    const row = {
      guildId,
      name: v.name,
      createdBy: String(ctx?.userId ?? '0'),
      ...Object.fromEntries(COLUMNS.map((k) => [k, data[k]])),
      ...actionColumns(data.actions),
      cooldownScope: data.cooldownScope,
      cooldownSeconds: data.cooldownScope === 'NONE' ? 0 : data.cooldownSeconds,
    };
    await ensureGuildSettings(guildId);
    try {
      return toItem(await createTagRow(row));
    } catch (err) {
      if (err?.code === 'P2002') throw new RecordError('conflict', `A custom command named "${v.name}" already exists.`, { path: 'name' });
      if (err instanceof TagError && err.code === 'LIMIT') {
        throw new RecordError('limit', `This server already has ${LIMITS.CUSTOM_TAGS} custom commands, the maximum. Delete one first.`);
      }
      throw err;
    }
  },

  async update(guildId, id, data, ctx) {
    const row = await findRow(guildId, id);
    const patch = {};
    for (const k of COLUMNS) if (Object.hasOwn(data, k)) patch[k] = data[k];
    if (Object.hasOwn(data, 'actions')) {
      await checkRoles(guildId, data.actions, getEffectiveActions(row), ctx);
      Object.assign(patch, actionColumns(data.actions));
    }
    const scopeChanged = Object.hasOwn(data, 'cooldownScope') || Object.hasOwn(data, 'cooldownSeconds');
    if (scopeChanged) {
      const scope = data.cooldownScope ?? row.cooldownScope;
      patch.cooldownScope = scope;
      patch.cooldownSeconds = scope === 'NONE' ? 0 : (data.cooldownSeconds ?? row.cooldownSeconds);
    }
    if (!Object.keys(patch).length) return toItem(row);
    // Optimistic, like writeTag: the form was filled from `row`, so a concurrent change is reported, not overwritten.
    const { count } = await prisma.customTag.updateMany({ where: { id, guildId, updatedAt: row.updatedAt }, data: patch });
    if (!count) {
      if (!(await prisma.customTag.findFirst({ where: { id, guildId }, select: { id: true } }))) throw notFound();
      throw new RecordError('conflict', 'Someone else changed this command meanwhile. Reload it and try again.');
    }
    const cooldownChanged = scopeChanged && (patch.cooldownScope !== row.cooldownScope || patch.cooldownSeconds !== row.cooldownSeconds);
    if (cooldownChanged) await clearTagCooldowns(guildId, id); // as /tag cooldown does
    return toItem(await findRow(guildId, id));
  },

  async remove(guildId, id) {
    await findRow(guildId, id);
    const { count } = await prisma.customTag.deleteMany({ where: { id, guildId } });
    if (!count) throw notFound();
    await clearTagCooldowns(guildId, id);
  },

  actions: Object.freeze({}),
});
