// Database access for the `starboards` collection (the Starboard table). Create, edit and delete go through starboardService.js
// (createBoard, updateBoard, deleteBoard: cap and row locks inside, the board cache in Redis invalidated after every write),
// exactly like /starboard. The bot's own 60 s memo clears by itself.
import prisma from '../../database/client.js';
import {
  BoardLimitError, IgnoreLimitError, MAX_IGNORED_CHANNELS, NotFoundError, SettingsConflictError, createBoard, deleteBoard,
  isUniqueViolation, listBoardsWithCounts, normalizeBoardName, parseEmojiList, updateBoard,
} from '../../services/starboardService.js';
import { LIMITS } from '../../config/constants.js';
import { RecordError } from './errors.js';
import { collection } from './starboards.js';

const SELF_STAR_CONFLICT = 'Self-stars are counted on this board, so they cannot also be removed.';

const toItem = (row, reposts = 0) => ({
  id: row.id,
  name: row.name,
  channelId: row.channelId,
  emojis: [...(row.emojis ?? [])],
  threshold: row.threshold,
  selfStar: row.selfStar === true,
  removeSelfStars: row.removeSelfStars === true,
  ignoreBots: row.ignoreBots !== false,
  removeBelowThreshold: row.removeBelowThreshold !== false,
  autoReact: row.autoReact === true,
  maxMessageAgeDays: row.maxMessageAgeDays ?? 0,
  allowNsfw: row.allowNsfw === true,
  ignoredChannelIds: [...(row.ignoredChannelIds ?? [])],
  enabled: row.enabled !== false,
  reposts: row.reposts ?? reposts,
});

const nameTaken = (name) => new RecordError('conflict', `A starboard named "${name}" already exists.`, { path: 'name' });

function cleanName(name) {
  const clean = normalizeBoardName(name);
  if (!clean) throw new RecordError('invalid', 'Board names may use letters a-z, digits, - and _ (max 32).', { path: 'name' });
  return clean;
}

/** Entries are unicode emojis, `<:name:id>` or bare ids; the stored form is the emoji itself or the custom emoji's id. */
function cleanEmojis(entries) {
  if (entries.length === 0) return []; // every emoji counts
  const parsed = parseEmojiList(entries.join(' '), { emojis: { cache: [] } });
  if (!parsed.ok) throw new RecordError('invalid', parsed.error, { path: 'emojis' });
  return parsed.emojis;
}

/** Age-restricted sources need an age-restricted repost channel (checked against the live channel list). */
async function assertNsfwChannel(ctx, guildId, channelId) {
  if (typeof ctx?.discord?.getGuildResources !== 'function') return;
  let resources;
  try {
    resources = await ctx.discord.getGuildResources(guildId);
  } catch {
    throw new RecordError('discord', 'Could not check whether the repost channel is age-restricted. Try again in a moment.');
  }
  const channel = resources.channels.find((c) => c.id === channelId);
  if (channel && !channel.nsfw) {
    throw new RecordError('invalid',
      'To repost from age-restricted channels, the repost channel must be age-restricted too. Mark it as age-restricted in Discord first.',
      { path: 'allowNsfw' });
  }
}

/** Turns the service's typed errors into what the admin sees. */
function mapError(err, name) {
  if (err instanceof BoardLimitError) return new RecordError('limit', `A server can have at most ${LIMITS.STARBOARDS} starboards. Delete one first.`);
  if (err instanceof NotFoundError) return new RecordError('not_found', 'That starboard no longer exists.');
  if (err instanceof IgnoreLimitError) {
    return new RecordError('limit', `A starboard can ignore at most ${MAX_IGNORED_CHANNELS} channels.`, { path: 'ignoredChannelIds' });
  }
  if (err instanceof SettingsConflictError) return new RecordError('invalid', SELF_STAR_CONFLICT, { path: 'removeSelfStars' });
  if (isUniqueViolation(err)) return nameTaken(name);
  return err;
}

async function getItem(guildId, id) {
  const row = await prisma.starboard.findFirst({ where: { id, guildId } });
  if (!row) return null;
  const reposts = await prisma.starboardEntry.count({ where: { starboardId: id, boardMessageId: { not: null } } });
  return toItem(row, reposts);
}

export const store = Object.freeze({
  async list(guildId) {
    return (await listBoardsWithCounts(guildId)).map((row) => toItem(row));
  },

  get: getItem,

  async create(guildId, data, ctx = {}) {
    const name = cleanName(data.name);
    const emojis = cleanEmojis(data.emojis);
    if (data.allowNsfw) await assertNsfwChannel(ctx, guildId, data.channelId);
    try {
      const row = await createBoard(guildId, {
        name,
        channelId: data.channelId,
        emojis,
        threshold: data.threshold,
        selfStar: data.selfStar,
        removeSelfStars: data.removeSelfStars,
        ignoreBots: data.ignoreBots,
        removeBelowThreshold: data.removeBelowThreshold,
        autoReact: data.autoReact,
        maxMessageAgeDays: data.maxMessageAgeDays,
        allowNsfw: data.allowNsfw,
        ignoredChannelIds: data.ignoredChannelIds,
        enabled: data.enabled,
      });
      ctx.logger?.info?.(`Dashboard starboard ${row.id} created in ${guildId} by ${ctx.userId}`);
      return toItem(row);
    } catch (err) {
      throw mapError(err, name);
    }
  },

  async update(guildId, id, data, ctx = {}) {
    const current = await prisma.starboard.findFirst({ where: { id, guildId } });
    if (!current) throw new RecordError('not_found', 'That starboard no longer exists.');

    // The cross-field rules again, against the row as it is now (the request was validated against an earlier read).
    const problem = collection.check(data, { mode: 'update', current: toItem(current) });
    if (problem) throw new RecordError('invalid', problem.message, { path: problem.path });

    const patch = {};
    for (const key of [
      'channelId', 'threshold', 'selfStar', 'removeSelfStars', 'ignoreBots', 'removeBelowThreshold', 'autoReact',
      'maxMessageAgeDays', 'allowNsfw', 'enabled',
    ]) if (data[key] !== undefined) patch[key] = data[key];
    if (data.name !== undefined) patch.name = cleanName(data.name);
    if (data.emojis !== undefined) patch.emojis = cleanEmojis(data.emojis);

    const effective = { ...current, ...patch };
    if (effective.allowNsfw && (patch.allowNsfw !== undefined || patch.channelId !== undefined)) {
      await assertNsfwChannel(ctx, guildId, effective.channelId);
    }

    // The service edits the ignore list one channel at a time: apply the difference pairwise, the first pair with the rest.
    const wanted = data.ignoredChannelIds ?? current.ignoredChannelIds;
    const adds = wanted.filter((c) => !current.ignoredChannelIds.includes(c));
    const removes = current.ignoredChannelIds.filter((c) => !wanted.includes(c));
    try {
      let step = { ...patch, addIgnored: adds.shift(), removeIgnored: removes.shift() };
      for (;;) {
        for (const key of ['addIgnored', 'removeIgnored']) if (step[key] === undefined) delete step[key];
        if (Object.keys(step).length > 0) await updateBoard(guildId, id, step);
        if (adds.length === 0 && removes.length === 0) break;
        step = { addIgnored: adds.shift(), removeIgnored: removes.shift() };
      }
    } catch (err) {
      throw mapError(err, patch.name ?? current.name);
    }
    ctx.logger?.info?.(`Dashboard starboard ${id} updated in ${guildId} by ${ctx.userId}`);
    return getItem(guildId, id);
  },

  async remove(guildId, id, ctx = {}) {
    if ((await deleteBoard(guildId, id)) === 0) throw new RecordError('not_found', 'That starboard no longer exists.');
    ctx.logger?.info?.(`Dashboard starboard ${id} deleted in ${guildId} by ${ctx.userId}`);
  },

  actions: Object.freeze({}),
});
