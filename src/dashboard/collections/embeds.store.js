// Database and Discord access of the `embeds` collection: the same SavedEmbed rows, cache keys and rules as /embed
// (src/services/embedService.js), with the posted message written through the dashboard's Discord REST client
// (ctx.discord) instead of a gateway client. The bot's in-process embed lock does not reach this process: the cap is
// guarded by a per-guild advisory lock, and a template change edits Discord first and saves after, so the dashboard
// never reports a change the posted message did not get.
import { Prisma } from '@prisma/client';
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { cache } from '../../services/redis.js';
import { ensureGuildSettings, getSettingsSafe } from '../../services/settingsService.js';
import {
  EMBED_CACHE_MISS_TTL_SECONDS, EMBED_CACHE_TTL_SECONDS, embedCacheKey, extractRoleMentionIds, normalizeEmbedName,
  resolveDefaultColor, validateForSave,
} from '../../services/embedService.js';
import { normalizeTemplate, renderTemplate } from '../../utils/customEmbed.js';
import { RecordError } from './errors.js';
import {
  DISCORD_CODES, SLUG_MESSAGE, discordFailure, embedItem, isDiscordApiError, memberFor, renderContextFor,
  restAllowedMentions, splitPingableRoles, toRestMessage,
} from './embeds.js';

const lockSql = (key) => Prisma.sql`SELECT 1 AS ok FROM (SELECT pg_advisory_xact_lock(hashtext(${key}))) AS l`;

/** Raw statements (test/db.test.js runs each one). */
export const RAW_SQL = Object.freeze([
  { name: 'dashboard.embeds.lockGuild', build: () => lockSql('savedembed:0') },
]);

const notFound = () => new RecordError('not_found', 'That saved embed no longer exists.');

async function findRow(guildId, id) {
  const row = await prisma.savedEmbed.findFirst({ where: { id, guildId } });
  if (!row) throw notFound();
  return row;
}

/** validateForSave (the /embed write rules: Discord limits, link buttons http(s) ≤ 512, emojis) on a template. */
function checkTemplate(raw) {
  const template = normalizeTemplate(raw);
  const errors = validateForSave(template);
  if (errors.length) {
    const button = /^Button (\d+)/.exec(errors[0]);
    throw new RecordError('invalid', errors[0], { path: button ? `template.buttons.${Number(button[1]) - 1}` : 'template' });
  }
  return template;
}

function requireDiscord(ctx, method) {
  if (typeof ctx?.discord?.[method] !== 'function') {
    throw new RecordError('unavailable', 'Posting messages from the dashboard is not available right now.');
  }
}

async function loadResources(ctx, guildId) {
  requireDiscord(ctx, 'getGuildResources');
  try {
    return await ctx.discord.getGuildResources(guildId);
  } catch (err) {
    if (isDiscordApiError(err)) throw new RecordError('discord', 'The server could not be loaded from Discord. Try again in a moment.');
    throw err;
  }
}

const channelIn = (resources, id) => (resources?.channels ?? []).find((c) => c.id === id) ?? null;

/**
 * Renders a template into a REST message body. 'publish' lets `<@&id>` role mentions in the text ping only when the
 * role is mentionable or the dashboard user may mention every role (resolvePublishMentions of /embed publish); any
 * other mode pings nobody (an edit never pings, like /embed update).
 * @returns {{ body: object, refused: string[] }}
 * @throws {RecordError} invalid when the message renders empty
 */
export function renderForDiscord(template, resources, { channelId, ctx, settings, mode }) {
  const renderCtx = renderContextFor(resources, { channelId, user: ctx?.user ?? null, userId: ctx?.userId ?? null });
  const rendered = renderTemplate(template, renderCtx, { defaultColor: resolveDefaultColor(settings) });
  if (!rendered) throw new RecordError('invalid', 'This embed renders empty. Add a title, description, field or text.', { path: 'template' });
  if (mode !== 'publish') {
    return { body: toRestMessage(rendered, { edit: mode === 'edit', allowedMentions: restAllowedMentions({ users: false }) }), refused: [] };
  }
  const { allowed, refused } = splitPingableRoles(extractRoleMentionIds(rendered.content), resources, memberFor(ctx, resources));
  return { body: toRestMessage(rendered, { allowedMentions: restAllowedMentions({ users: true, roles: allowed }) }), refused };
}

/**
 * Edits the posted copy to `template`. A deleted message or channel is not an error: the embed becomes a draft
 * (updatePublishedEmbed's MESSAGE_GONE / CHANNEL_GONE).
 * @returns {Promise<'edited'|'message_gone'|'channel_gone'>}
 * @throws {RecordError} discord / invalid for every other failure
 */
async function editPosted(guildId, row, template, ctx, resources = null) {
  requireDiscord(ctx, 'editMessage');
  const res = resources ?? await loadResources(ctx, guildId);
  const settings = await getSettingsSafe(guildId);
  const { body } = renderForDiscord(template, res, { channelId: row.channelId, ctx, settings, mode: 'edit' });
  try {
    await ctx.discord.editMessage(row.channelId, row.messageId, body);
    return 'edited';
  } catch (err) {
    if (isDiscordApiError(err)) {
      if (err.discordCode === DISCORD_CODES.UNKNOWN_CHANNEL) return 'channel_gone';
      if (err.discordCode === DISCORD_CODES.UNKNOWN_MESSAGE || err.discordCode === DISCORD_CODES.CANNOT_EDIT_OTHERS) return 'message_gone';
      if (err.code === 'not_found' && err.discordCode === undefined) return 'message_gone';
    }
    throw discordFailure(err, { op: 'edit', channel: channelIn(res, row.channelId), channelId: row.channelId });
  }
}

const GONE_PATCH = Object.freeze({
  message_gone: Object.freeze({ messageId: null }),
  channel_gone: Object.freeze({ messageId: null, channelId: null }),
});
const GONE_TEXT = Object.freeze({
  message_gone: 'The posted message was deleted in Discord, so this embed is a draft again. Post it again with "Post in a channel".',
  channel_gone: 'The channel it was posted in was deleted, so this embed is a draft again. Post it again with "Post in a channel".',
});

async function refetch(guildId, id) {
  const row = await prisma.savedEmbed.findFirst({ where: { id, guildId } });
  if (!row) throw notFound();
  return row;
}

export const store = Object.freeze({
  async list(guildId) {
    const rows = await prisma.savedEmbed.findMany({ where: { guildId }, orderBy: { name: 'asc' }, take: LIMITS.SAVED_EMBEDS });
    return rows.map(embedItem);
  },

  async get(guildId, id) {
    const row = await prisma.savedEmbed.findFirst({ where: { id, guildId } });
    return row ? embedItem(row) : null;
  },

  async create(guildId, data, ctx) {
    const name = data.name;
    if (normalizeEmbedName(name) !== name) throw new RecordError('invalid', SLUG_MESSAGE, { path: 'name' });
    const template = checkTemplate(data.template);
    await ensureGuildSettings(guildId);
    let row;
    try {
      row = await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(lockSql(`savedembed:${guildId}`));
        if (await tx.savedEmbed.count({ where: { guildId } }) >= LIMITS.SAVED_EMBEDS) {
          throw new RecordError('limit', `This server already has ${LIMITS.SAVED_EMBEDS} saved embeds, the maximum. Delete one first.`);
        }
        return tx.savedEmbed.create({ data: { guildId, name, template, createdBy: String(ctx?.userId ?? '0') } });
      });
    } catch (err) {
      if (err?.code === 'P2002') throw new RecordError('conflict', `An embed called "${name}" already exists.`, { path: 'name' });
      throw err;
    }
    // Write-through, like createSavedEmbed: Automations read the template through this key.
    await cache.set(embedCacheKey(guildId, name), template, EMBED_CACHE_TTL_SECONDS);
    return embedItem(row);
  },

  async update(guildId, id, data, ctx) {
    const row = await findRow(guildId, id);
    if (!Object.hasOwn(data, 'template')) return embedItem(row);
    const template = checkTemplate(data.template);
    const patch = { template };
    if (row.messageId && row.channelId) {
      // Discord first: a refused edit (missing permission, outage) saves nothing and says why.
      const outcome = await editPosted(guildId, row, template, ctx);
      if (outcome !== 'edited') Object.assign(patch, GONE_PATCH[outcome]);
    }
    const { count } = await prisma.savedEmbed.updateMany({ where: { id, guildId }, data: patch });
    if (!count) throw notFound();
    await cache.set(embedCacheKey(guildId, row.name), template, EMBED_CACHE_TTL_SECONDS);
    return embedItem(await refetch(guildId, id));
  },

  async remove(guildId, id) {
    const row = await findRow(guildId, id);
    const { count } = await prisma.savedEmbed.deleteMany({ where: { id, guildId } });
    if (!count) throw notFound();
    // A short negative entry, like deleteSavedEmbedAndMessage. The posted message stays (no longer tracked).
    await cache.set(embedCacheKey(guildId, row.name), { __none: true }, EMBED_CACHE_MISS_TTL_SECONDS);
  },

  actions: Object.freeze({
    /** Posts the embed in a channel (or moves it there): `{ channelId, deletePrevious }`. */
    async publish(guildId, id, data, ctx) {
      requireDiscord(ctx, 'sendMessage');
      const row = await findRow(guildId, id);
      const resources = await loadResources(ctx, guildId);
      const channel = channelIn(resources, data.channelId);
      if (!channel) {
        throw new RecordError('invalid', 'That channel does not exist in this server any more. Pick another one.', { path: 'channelId' });
      }
      const settings = await getSettingsSafe(guildId);
      const { body, refused } = renderForDiscord(row.template, resources, { channelId: channel.id, ctx, settings, mode: 'publish' });

      let sent;
      try {
        sent = await ctx.discord.sendMessage(channel.id, body);
      } catch (err) {
        throw discordFailure(err, { op: 'send', channel, embeds: body.embeds.length > 0 });
      }
      const previous = row.messageId && row.channelId ? { channelId: row.channelId, messageId: row.messageId } : null;
      const { count } = await prisma.savedEmbed.updateMany({ where: { id, guildId }, data: { channelId: channel.id, messageId: sent.id } });
      if (!count) {
        // Deleted while posting: take the untracked copy down again.
        await ctx.discord.deleteMessage(channel.id, sent.id).catch(() => {});
        throw notFound();
      }

      const notes = [];
      if (previous && previous.messageId !== sent.id) {
        const prevName = channelIn(resources, previous.channelId)?.name;
        const where = prevName ? `#${prevName}` : 'its old channel';
        if (data.deletePrevious !== false) {
          try {
            await ctx.discord.deleteMessage(previous.channelId, previous.messageId);
            notes.push(`The previous copy in ${where} was deleted.`);
          } catch (err) {
            if (!isDiscordApiError(err)) throw err;
            if (err.code !== 'not_found') {
              const prevChannel = channelIn(resources, previous.channelId);
              const why = discordFailure(err, { op: 'delete', channel: prevChannel, channelId: previous.channelId }).message;
              notes.push(`${why} The previous copy in ${where} stays; delete it by hand.`);
            }
          }
        } else {
          notes.push(`The previous copy in ${where} stays but is no longer updated.`);
        }
      }
      if (refused.length) {
        const names = refused.map((rid) => (resources.roles ?? []).find((r) => r.id === rid)?.name).map((n, i) => (n ? `@${n}` : refused[i]));
        notes.push(`Not pinged (not mentionable, and you lack the Mention @everyone permission): ${names.join(', ')}.`);
      }
      return { item: embedItem(await refetch(guildId, id)), message: [`Posted in #${channel.name}.`, ...notes].join(' ') };
    },

    /** Edits the posted copy to the saved message again (/embed update). */
    async refresh(guildId, id, data, ctx) {
      const row = await findRow(guildId, id);
      if (!row.messageId || !row.channelId) throw new RecordError('invalid', 'This embed is not posted anywhere. Post it with "Post in a channel".');
      const outcome = await editPosted(guildId, row, normalizeTemplate(row.template), ctx);
      if (outcome !== 'edited') {
        await prisma.savedEmbed.updateMany({ where: { id, guildId, messageId: row.messageId }, data: GONE_PATCH[outcome] });
        return { item: embedItem(await refetch(guildId, id)), message: GONE_TEXT[outcome] };
      }
      return { item: embedItem(await refetch(guildId, id)), message: 'The posted message was updated.' };
    },

    /** Deletes the posted copy and keeps the saved embed as a draft (the channel is kept as a hint, like the bot does). */
    async unpublish(guildId, id, data, ctx) {
      requireDiscord(ctx, 'deleteMessage');
      const row = await findRow(guildId, id);
      if (!row.messageId || !row.channelId) throw new RecordError('invalid', 'This embed is not posted anywhere.');
      try {
        await ctx.discord.deleteMessage(row.channelId, row.messageId);
      } catch (err) {
        // 404 (message or channel gone) counts as done.
        if (!(isDiscordApiError(err) && err.code === 'not_found')) throw discordFailure(err, { op: 'delete', channelId: row.channelId });
      }
      await prisma.savedEmbed.updateMany({ where: { id, guildId, messageId: row.messageId }, data: { messageId: null } });
      return { item: embedItem(await refetch(guildId, id)), message: 'The posted message was deleted. The embed is a draft again.' };
    },
  }),
});
