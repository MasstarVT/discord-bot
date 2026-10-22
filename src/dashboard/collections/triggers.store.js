// Database access of the `triggers` collection (auto-responders): the AutoTrigger rows /tag trigger writes, through the
// same tagService helpers (createTriggerRow: per-guild advisory lock and cap; writeTrigger: optimistic update;
// validateTriggerPattern: safe regexes). Every write drops the shared Redis list (invalidateTriggerCache); the bot's
// own per-shard memo of it lives up to 60 s, so a change reaches the bot within a minute.
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { ensureGuildSettings } from '../../services/settingsService.js';
import {
  TagError, createTriggerRow, invalidateTriggerCache, validateTriggerPattern, writeTrigger,
} from '../../services/tagService.js';
import { RecordError } from './errors.js';
import { triggerItem } from './triggers.js';

const notFound = () => new RecordError('not_found', 'That auto-responder no longer exists.');

async function findRow(guildId, id) {
  const row = await prisma.autoTrigger.findFirst({ where: { id, guildId } });
  if (!row) throw notFound();
  return row;
}

function checkPattern(trigger, matchType, caseSensitive) {
  const v = validateTriggerPattern(trigger, matchType, caseSensitive);
  if (!v.ok) throw new RecordError('invalid', v.reason, { path: 'trigger' });
}

const FIELDS = Object.freeze([
  'trigger', 'matchType', 'caseSensitive', 'response', 'cooldown', 'deleteMessage', 'channelIds', 'exemptRoleIds', 'enabled',
]);

export const store = Object.freeze({
  async list(guildId) {
    // Creation order: the #numbers /tag trigger list shows, and the order in which triggers are tried.
    const rows = await prisma.autoTrigger.findMany({
      where: { guildId }, orderBy: [{ createdAt: 'asc' }, { id: 'asc' }], take: LIMITS.AUTO_TRIGGERS,
    });
    return rows.map(triggerItem);
  },

  async get(guildId, id) {
    const row = await prisma.autoTrigger.findFirst({ where: { id, guildId } });
    return row ? triggerItem(row) : null;
  },

  async create(guildId, data, ctx) {
    checkPattern(data.trigger, data.matchType, data.caseSensitive);
    await ensureGuildSettings(guildId);
    let row;
    try {
      row = await createTriggerRow({
        guildId, ...Object.fromEntries(FIELDS.map((k) => [k, data[k]])), createdBy: String(ctx?.userId ?? '0'),
      });
    } catch (err) {
      if (err instanceof TagError && err.code === 'LIMIT') {
        throw new RecordError('limit', `This server already has ${LIMITS.AUTO_TRIGGERS} auto-responders, the maximum. Delete one first.`);
      }
      throw err;
    }
    await invalidateTriggerCache(guildId);
    return triggerItem(row);
  },

  async update(guildId, id, data) {
    const row = await findRow(guildId, id);
    const patch = Object.fromEntries(FIELDS.filter((k) => Object.hasOwn(data, k)).map((k) => [k, data[k]]));
    if (!Object.keys(patch).length) return triggerItem(row);
    if (['trigger', 'matchType', 'caseSensitive'].some((k) => Object.hasOwn(patch, k))) {
      checkPattern(patch.trigger ?? row.trigger, patch.matchType ?? row.matchType, patch.caseSensitive ?? row.caseSensitive);
    }
    let updated;
    try {
      updated = await writeTrigger(guildId, row, patch);
    } catch (err) {
      if (err instanceof TagError && err.code === 'CONFLICT') {
        if (!(await prisma.autoTrigger.findFirst({ where: { id, guildId }, select: { id: true } }))) throw notFound();
        throw new RecordError('conflict', 'Someone else changed this auto-responder meanwhile. Reload it and try again.');
      }
      throw err;
    }
    await invalidateTriggerCache(guildId);
    if (!updated) throw notFound();
    return triggerItem(updated);
  },

  async remove(guildId, id) {
    const { count } = await prisma.autoTrigger.deleteMany({ where: { id, guildId } });
    if (!count) throw notFound();
    await invalidateTriggerCache(guildId);
  },

  actions: Object.freeze({}),
});
