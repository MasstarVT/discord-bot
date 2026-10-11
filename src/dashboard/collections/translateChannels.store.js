// Database access for the `translateChannels` collection (the TranslateChannel table). Same effects as /autotranslate:
// the per-server cap, turning the Auto-Translate module on when a channel is added (through settingsService), and a refresh of
// the channel list in Redis (`guild:{g}:translateChannels`) after every write; the bot's own 30 s memo clears by itself.
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { ensureGuildSettings, getSettings, updateSettings } from '../../services/settingsService.js';
import { normalizeTargetList, refreshTranslateCache } from '../../services/translationService.js';
import { RecordError } from './errors.js';
import { lockGuildSettingsSql } from './levelRoles.store.js';
import { MAX_TARGETS, describeLanguages } from './translateChannels.js';

const TX_OPTS = { timeout: 10_000, maxWait: 10_000 };

const toItem = (row) => {
  const targetLangs = normalizeTargetList(row.targetLangs).valid.slice(0, MAX_TARGETS);
  return { id: row.id, channelId: row.channelId, targetLangs, languages: describeLanguages(targetLangs) };
};

/** Canonical codes, or an error naming the language that is not known. */
function cleanLanguages(input) {
  const { valid, invalid } = normalizeTargetList(input);
  if (invalid.length > 0) {
    throw new RecordError('invalid', `"${String(invalid[0]).slice(0, 40)}" is not a language I know.`, { path: 'targetLangs' });
  }
  if (valid.length === 0) throw new RecordError('invalid', 'Pick at least one language.', { path: 'targetLangs' });
  return valid.slice(0, MAX_TARGETS);
}

const alreadySet = () => new RecordError('conflict',
  'That channel already has auto-translate. Edit its languages instead.', { path: 'channelId' });

export const store = Object.freeze({
  async list(guildId) {
    const rows = await prisma.translateChannel.findMany({ where: { guildId }, orderBy: { createdAt: 'asc' }, take: LIMITS.TRANSLATE_CHANNELS });
    return rows.map(toItem);
  },

  async get(guildId, id) {
    const row = await prisma.translateChannel.findFirst({ where: { id, guildId } });
    return row ? toItem(row) : null;
  },

  async create(guildId, data, ctx = {}) {
    const targetLangs = cleanLanguages(data.targetLangs);
    await ensureGuildSettings(guildId);
    let row;
    try {
      row = await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(lockGuildSettingsSql(guildId));
        const existing = await tx.translateChannel.findUnique({
          where: { guildId_channelId: { guildId, channelId: data.channelId } }, select: { id: true },
        });
        if (existing) throw alreadySet();
        if ((await tx.translateChannel.count({ where: { guildId } })) >= LIMITS.TRANSLATE_CHANNELS) {
          throw new RecordError('limit',
            `This server already has ${LIMITS.TRANSLATE_CHANNELS} auto-translate channels (the maximum). Turn one off first.`);
        }
        return tx.translateChannel.create({ data: { guildId, channelId: data.channelId, targetLangs } });
      }, TX_OPTS);
    } catch (err) {
      throw err?.code === 'P2002' ? alreadySet() : err;
    }
    // /autotranslate switches the module on when a channel is added; the dashboard does the same.
    const settings = await getSettings(guildId);
    if (settings.translationEnabled !== true) await updateSettings(guildId, { translationEnabled: true });
    await refreshTranslateCache(guildId);
    ctx.logger?.info?.(`Dashboard auto-translate ${row.id} (${row.channelId}) created in ${guildId} by ${ctx.userId}`);
    return toItem(row);
  },

  async update(guildId, id, data) {
    if (data.targetLangs !== undefined) {
      const targetLangs = cleanLanguages(data.targetLangs);
      const { count } = await prisma.translateChannel.updateMany({ where: { id, guildId }, data: { targetLangs } });
      if (count === 0) throw new RecordError('not_found', 'That translated channel no longer exists.');
      await refreshTranslateCache(guildId);
    }
    const row = await prisma.translateChannel.findFirst({ where: { id, guildId } });
    if (!row) throw new RecordError('not_found', 'That translated channel no longer exists.');
    return toItem(row);
  },

  async remove(guildId, id, ctx = {}) {
    const { count } = await prisma.translateChannel.deleteMany({ where: { id, guildId } });
    if (count === 0) throw new RecordError('not_found', 'That translated channel no longer exists.');
    await refreshTranslateCache(guildId);
    ctx.logger?.info?.(`Dashboard auto-translate ${id} deleted in ${guildId} by ${ctx.userId}`);
  },

  actions: Object.freeze({}),
});
