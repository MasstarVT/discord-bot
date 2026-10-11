// Database access for the `statChannels` collection (the StatChannel table). Same rules as /stats: the per-server and
// role-counter caps, one counter per kind (per role for Role members), validateStatTemplate for the name, and the recurring
// `stats.refresh` job (scheduled through statsService, which needs no Discord client), so the bot renames the channel on its
// next run. Edit and delete reuse editTemplate and deleteStatChannel(keepChannel: true).
// What the dashboard cannot do: create the Discord channel (it takes an existing voice channel instead, without the locked
// permission overwrites /stats create sets) and delete it (the channel stays with its last name).
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { ensureGuildSettings, getSettings, isModuleEnabled } from '../../services/settingsService.js';
import {
  EDIT_PULL_DELAY_MS, LIST_KINDS, MEMBER_LIST_MAX_MEMBERS, MIN_NEXT_RUN_MS, StatsError, deleteStatChannel, editTemplate,
  ensureRefreshJob, pullRefresh, validateStatTemplate,
} from '../../services/statsService.js';
import { RecordError } from './errors.js';
import { lockGuildSettingsSql } from './levelRoles.store.js';
import { KIND_INFO, normalizeTemplate } from './statChannels.js';

const TX_OPTS = { timeout: 10_000, maxWait: 10_000 };

const toItem = (row) => ({
  id: row.id,
  channelId: row.channelId,
  kind: row.kind,
  roleId: row.roleId ?? null,
  template: row.template,
  kindLabel: KIND_INFO[row.kind]?.label ?? row.kind,
  lastValue: row.lastValue ?? null,
  peakValue: row.peakValue ?? null,
  lastRenamedAt: row.lastRenamedAt instanceof Date ? row.lastRenamedAt.toISOString() : null,
});

/** The server's name, member count and role names, for rendering the template; null when Discord cannot be asked. */
async function guildFacts(ctx, guildId) {
  try {
    const resources = await ctx?.discord?.getGuildResources?.(guildId);
    return resources ?? null;
  } catch {
    return null;
  }
}

/** validateStatTemplate with a plain guild object (the server tokens only need a name and a count). */
function cleanTemplate(template, { kind, roleId }, facts, guildId) {
  const roleName = kind === 'ROLE_MEMBERS' ? (facts?.roles?.find((r) => r.id === roleId)?.name ?? 'role') : null;
  const guild = { id: guildId, name: facts?.guild?.name ?? 'Server', memberCount: facts?.guild?.memberCount ?? 0 };
  const checked = validateStatTemplate(template, { kind, roleName, guild });
  if (!checked.ok) throw new RecordError('invalid', checked.error, { path: 'template' });
  return checked.template;
}

const conflictCounter = (channelId) => new RecordError('conflict',
  `<#${channelId}> already shows this counter. Edit that one instead.`, { path: 'kind' });

export const store = Object.freeze({
  async list(guildId) {
    const rows = await prisma.statChannel.findMany({ where: { guildId }, orderBy: { createdAt: 'asc' }, take: LIMITS.STAT_CHANNELS });
    return rows.map(toItem);
  },

  async get(guildId, id) {
    const row = await prisma.statChannel.findFirst({ where: { id, guildId } });
    return row ? toItem(row) : null;
  },

  async create(guildId, data, ctx = {}) {
    const { kind, channelId } = data;
    const roleId = kind === 'ROLE_MEMBERS' ? data.roleId : null;
    if (kind === 'ROLE_MEMBERS' && !roleId) throw new RecordError('invalid', 'Pick the role to count for "Role members".', { path: 'roleId' });
    if (roleId === guildId) {
      throw new RecordError('invalid', 'Use the Members counter to count everyone.', { path: 'roleId' });
    }

    const facts = await guildFacts(ctx, guildId);
    if (LIST_KINDS.has(kind) && (facts?.guild?.memberCount ?? 0) > MEMBER_LIST_MAX_MEMBERS) {
      throw new RecordError('invalid',
        'This counter needs the full member list, which the bot only loads for servers with up to '
        + `${MEMBER_LIST_MAX_MEMBERS.toLocaleString('en-US')} members. Use Members instead.`,
        { path: 'kind' });
    }
    const template = cleanTemplate(data.template ?? KIND_INFO[kind].template, { kind, roleId }, facts, guildId);

    await ensureGuildSettings(guildId);
    let row;
    try {
      row = await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(lockGuildSettingsSql(guildId));
        const rows = await tx.statChannel.findMany({ where: { guildId }, select: { channelId: true, kind: true, roleId: true } });
        if (rows.length >= LIMITS.STAT_CHANNELS) {
          throw new RecordError('limit', `A server can have at most ${LIMITS.STAT_CHANNELS} counter channels. Remove one first.`);
        }
        if (kind === 'ROLE_MEMBERS' && rows.filter((r) => r.kind === 'ROLE_MEMBERS').length >= LIMITS.ROLE_STAT_CHANNELS) {
          throw new RecordError('limit', `A server can have at most ${LIMITS.ROLE_STAT_CHANNELS} role counters. Remove one first.`);
        }
        const dup = rows.find((r) => r.kind === kind && (kind !== 'ROLE_MEMBERS' || r.roleId === roleId));
        if (dup) throw conflictCounter(dup.channelId);
        if (rows.some((r) => r.channelId === channelId)) {
          throw new RecordError('conflict', 'That channel already shows a counter.', { path: 'channelId' });
        }
        return tx.statChannel.create({
          data: { guildId, channelId, kind, roleId, template, lastValue: null, lastRenamedAt: null, peakValue: null },
        });
      }, TX_OPTS);
    } catch (err) {
      throw err?.code === 'P2002' ? new RecordError('conflict', 'That channel already shows a counter.', { path: 'channelId' }) : err;
    }
    // The first value arrives with the next refresh of the guild's recurring job.
    await ensureRefreshJob(guildId, new Date(Date.now() + MIN_NEXT_RUN_MS));
    ctx.logger?.info?.(`Dashboard stat channel ${row.id} (${kind}) created in ${guildId} by ${ctx.userId}`);
    return toItem(row);
  },

  async update(guildId, id, data, ctx = {}) {
    const row = await prisma.statChannel.findFirst({ where: { id, guildId } });
    if (!row) throw new RecordError('not_found', 'That counter no longer exists.');
    if (data.template === undefined) return toItem(row);
    // Empty = the default name of the counter type.
    const wanted = data.template === null ? KIND_INFO[row.kind].template : data.template;
    const template = cleanTemplate(wanted, row, await guildFacts(ctx, guildId), guildId);
    if (normalizeTemplate(template) !== row.template) {
      try {
        await editTemplate({ id: guildId }, row, template);
      } catch (err) {
        if (err instanceof StatsError && err.code === 'NOT_FOUND') throw new RecordError('not_found', 'That counter no longer exists.');
        throw err;
      }
    }
    return toItem({ ...row, template });
  },

  async remove(guildId, id, ctx = {}) {
    const row = await prisma.statChannel.findFirst({ where: { id, guildId } });
    if (!row) throw new RecordError('not_found', 'That counter no longer exists.');
    await deleteStatChannel({ id: guildId }, row, { keepChannel: true });
    ctx.logger?.info?.(`Dashboard stat channel ${id} removed in ${guildId} by ${ctx.userId}; the Discord channel was kept`);
  },

  actions: Object.freeze({
    /** /stats refresh: pulls the guild's recurring job forward; the bot's shard does the renaming. */
    async refresh(guildId, id) {
      const row = await prisma.statChannel.findFirst({ where: { id, guildId } });
      if (!row) throw new RecordError('not_found', 'That counter no longer exists.');
      if (!isModuleEnabled(await getSettings(guildId), 'stats')) {
        throw new RecordError('invalid', 'The Statistics Channels module is turned off, so counters are not updated. Turn it on first.');
      }
      if (!(await pullRefresh(guildId, EDIT_PULL_DELAY_MS))) {
        throw new RecordError('unavailable', 'Could not schedule the update. Try again in a moment.');
      }
      return { item: toItem(row), message: 'The bot will update the counters within about a minute.' };
    },
  }),
});
