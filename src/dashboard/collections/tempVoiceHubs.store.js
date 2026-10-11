// Database access for the `tempVoiceHubs` collection (the TempVoiceHub table). Create and edit go through tempVoiceService.js
// (createHub under a lock on the GuildSettings row, updateHub), with the same checks as /voice-hub: the name format, the
// hub being an existing voice channel that is neither a hub nor a temporary channel, and the per-server cap.
// What the dashboard cannot do: create the Discord channel (pick an existing one) and delete Discord channels. Deleting a hub
// is refused while temporary channels from it are open, because the database would forget them while the channels stayed.
// The bot keeps its routing index of hubs in memory for 5 minutes (INDEX_TTL_MS) and the dashboard cannot clear it.
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import {
  HubLimitError, countHubs, createHub, findHubByChannel, isTempChannel, listHubs, updateHub, validateNameFormat,
} from '../../services/tempVoiceService.js';
import { RecordError } from './errors.js';
import { DEFAULT_NAME_FORMAT } from './tempVoiceHubs.js';

const SAMPLE_USER_ID = '100000000000000001';

const toItem = (row) => ({
  id: row.id,
  channelId: row.channelId,
  categoryId: row.categoryId ?? null,
  nameFormat: row.nameFormat,
  userLimit: row.userLimit,
  bitrate: row.bitrate ? Math.min(384, Math.max(8, Math.round(row.bitrate / 1000))) : null,
  keepAliveMinutes: row.keepAliveMinutes,
  ownershipLockMinutes: row.ownershipLockMinutes,
  syncPermissions: row.syncPermissions !== false,
  createTextChannel: row.createTextChannel === true,
  textChannelMembersOnly: row.textChannelMembersOnly !== false,
  ignoredRoleIds: [...(row.ignoredRoleIds ?? [])],
  moderatorRoleIds: [...(row.moderatorRoleIds ?? [])],
  activeChannels: row._count?.channels ?? 0,
});

/** The service's check of a name format, rendered with a sample member so `{user}` shows up as the mention it would be. */
function cleanNameFormat(format, guildName) {
  const member = {
    id: SAMPLE_USER_ID,
    displayName: 'Sample',
    user: { id: SAMPLE_USER_ID, username: 'sample', bot: false },
  };
  const checked = validateNameFormat(format ?? DEFAULT_NAME_FORMAT, { member, guild: { id: '0', name: guildName ?? 'Server' } });
  if (!checked.ok) throw new RecordError('invalid', checked.error, { path: 'nameFormat' });
  return String(format ?? DEFAULT_NAME_FORMAT).trim();
}

async function guildNameOf(ctx, guildId) {
  try {
    return (await ctx?.discord?.getGuildResources?.(guildId))?.guild?.name ?? null;
  } catch {
    return null;
  }
}

const withCount = { _count: { select: { channels: true } } };

async function getItem(guildId, id) {
  const row = await prisma.tempVoiceHub.findFirst({ where: { id, guildId }, include: withCount });
  return row ? toItem(row) : null;
}

const alreadyHub = () => new RecordError('conflict', 'That channel is already a hub. Edit it instead.', { path: 'channelId' });

export const store = Object.freeze({
  async list(guildId) {
    return (await listHubs(guildId)).slice(0, LIMITS.TEMPVOICE_HUBS).map(toItem);
  },

  get: getItem,

  async create(guildId, data, ctx = {}) {
    const nameFormat = cleanNameFormat(data.nameFormat, await guildNameOf(ctx, guildId));
    if (await countHubs(guildId) >= LIMITS.TEMPVOICE_HUBS) {
      throw new RecordError('limit', `A server can have at most ${LIMITS.TEMPVOICE_HUBS} hubs. Delete one first.`);
    }
    if (await findHubByChannel(guildId, data.channelId)) throw alreadyHub();
    if (await isTempChannel(data.channelId)) {
      throw new RecordError('invalid', 'That is a temporary channel and cannot be a hub.', { path: 'channelId' });
    }
    const hubData = {
      channelId: data.channelId,
      nameFormat,
      userLimit: data.userLimit,
      keepAliveMinutes: data.keepAliveMinutes,
      ownershipLockMinutes: data.ownershipLockMinutes,
      syncPermissions: data.syncPermissions,
      createTextChannel: data.createTextChannel,
      textChannelMembersOnly: data.textChannelMembersOnly,
      ignoredRoleIds: data.ignoredRoleIds,
      moderatorRoleIds: data.moderatorRoleIds,
    };
    if (data.categoryId) hubData.categoryId = data.categoryId;
    if (data.bitrate) hubData.bitrate = data.bitrate * 1000;
    try {
      const row = await createHub(guildId, hubData);
      ctx.logger?.info?.(`Dashboard voice hub ${row.id} (${row.channelId}) created in ${guildId} by ${ctx.userId}`);
      return toItem({ ...row, _count: { channels: 0 } });
    } catch (err) {
      if (err instanceof HubLimitError) throw new RecordError('limit', `A server can have at most ${LIMITS.TEMPVOICE_HUBS} hubs. Delete one first.`);
      if (err?.code === 'P2002') throw alreadyHub();
      throw err;
    }
  },

  async update(guildId, id, data, ctx = {}) {
    const patch = {};
    if (data.categoryId !== undefined) patch.categoryId = data.categoryId;
    if (data.nameFormat !== undefined) patch.nameFormat = cleanNameFormat(data.nameFormat, await guildNameOf(ctx, guildId));
    if (data.bitrate !== undefined) patch.bitrate = data.bitrate === null ? null : data.bitrate * 1000;
    for (const key of [
      'userLimit', 'keepAliveMinutes', 'ownershipLockMinutes', 'syncPermissions', 'createTextChannel', 'textChannelMembersOnly',
      'ignoredRoleIds', 'moderatorRoleIds',
    ]) if (data[key] !== undefined) patch[key] = data[key];
    const hub = Object.keys(patch).length > 0 ? await updateHub(guildId, id, patch) : await prisma.tempVoiceHub.findFirst({ where: { id, guildId } });
    if (!hub) throw new RecordError('not_found', 'That hub no longer exists.');
    return getItem(guildId, id);
  },

  async remove(guildId, id, ctx = {}) {
    const hub = await prisma.tempVoiceHub.findFirst({ where: { id, guildId }, include: withCount });
    if (!hub) throw new RecordError('not_found', 'That hub no longer exists.');
    const open = hub._count.channels;
    if (open > 0) {
      throw new RecordError('invalid',
        `${open} temporary channel${open === 1 ? ' is' : 's are'} still open from this hub. Deleting the hub now would leave them behind in Discord. `
        + 'Wait until they close, or use /voice-hub delete in Discord, which removes them too.');
    }
    await prisma.tempVoiceHub.deleteMany({ where: { id, guildId } });
    ctx.logger?.info?.(`Dashboard voice hub ${id} deleted in ${guildId} by ${ctx.userId}; the Discord channel was kept`);
  },

  actions: Object.freeze({}),
});
