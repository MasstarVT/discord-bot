// Database access of the `reactionRoles` collection: the same ReactionRoleGroup / ReactionRoleItem rows and panel index as
// /reactionrole (src/services/reactionRoleService.js). The dashboard only writes the database (under a per-guild advisory
// lock for the panel cap); everything that touches Discord (posting the panel message, editing it, adding the bot's
// reactions, reposting, deleting the message) is a task run in the bot (src/services/dashboardTasks/reactionRoles.js):
//   rr.publish  post a new panel or bring the posted one up to date   (create, edit)
//   rr.repost   post again, in another channel or after the message was deleted   (the "Repost" action)
//   rr.delete   delete the panel and its message   (delete)
// A panel is saved first and posted after: if the bot cannot post it (missing permission, bot offline) the panel stays,
// listed as "Not posted yet", and the admin is told to retry with "Repost" or by saving again.
import { randomUUID } from 'node:crypto';
import { Prisma } from '@prisma/client';
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { ensureGuildSettings } from '../../services/settingsService.js';
import * as rr from '../../services/reactionRoleService.js';
import { RecordError, taskOutcome } from './errors.js';
import { isDiscordApiError } from './embeds.js';
import {
  DEFAULT_TITLE, UNPOSTED_MESSAGE_ID, collection, diffPanelItems, emojiKey, normalizeItems, panelItem,
} from './reactionRoles.js';

const lockSql = (key) => Prisma.sql`SELECT 1 AS ok FROM (SELECT pg_advisory_xact_lock(hashtext(${key}))) AS l`;

/** Raw statements (test/db.test.js runs each one). */
export const RAW_SQL = Object.freeze([
  { name: 'dashboard.reactionRoles.lockGuild', build: () => lockSql('rrpanel:0') },
  { name: 'dashboard.reactionRoles.lockItems', build: () => lockSql('rritems:0') },
]);

const notFound = () => new RecordError('not_found', 'That panel no longer exists.');

function requireTasks(ctx) {
  if (typeof ctx?.runTask !== 'function') throw new RecordError('unavailable', 'Panels cannot be posted from the dashboard right now.');
}

const CUSTOM_EMOJI_ID_RE = /:(\d{17,20})>$/;

/**
 * The server's roles and emojis. A custom emoji the cached list does not know may be a few seconds old: the list is read
 * again, fresh, before the emoji is refused.
 */
async function loadResources(ctx, guildId, items = []) {
  if (typeof ctx?.discord?.getGuildResources !== 'function') {
    throw new RecordError('unavailable', 'The roles of this server cannot be checked from the dashboard right now.');
  }
  try {
    let resources = await ctx.discord.getGuildResources(guildId);
    const wanted = items.map((i) => CUSTOM_EMOJI_ID_RE.exec(i.emoji ?? '')?.[1]).filter(Boolean);
    if (wanted.some((id) => !(resources.emojis ?? []).some((e) => e.id === id))) resources = await ctx.discord.getGuildResources(guildId, { fresh: true });
    return resources;
  } catch (err) {
    if (isDiscordApiError(err)) throw new RecordError('discord', 'The server could not be loaded from Discord. Try again in a moment.');
    throw err;
  }
}

/** A Unicode emoji must be one the bot can put on a panel (parseEmojiInput: exactly one emoji). */
function assertUsableEmojis(items) {
  items.forEach((item, i) => {
    if (item.emoji && !CUSTOM_EMOJI_ID_RE.test(item.emoji) && !rr.isUnicodeEmoji(item.emoji)) {
      throw new RecordError('invalid', `"${item.emoji}" cannot be used as a panel emoji. Pick a standard emoji or one of this server's emojis.`, { path: `items.${i}.emoji` });
    }
  });
}

async function loadRow(guildId, id) {
  const row = await rr.getPanel(guildId, id);
  if (!row) throw notFound();
  return row;
}

async function getItem(guildId, id) {
  const row = await rr.getPanel(guildId, id);
  return row ? panelItem(row) : null;
}

/** Runs a task and turns a refusal into "saved, but ..." (the panel row stays). */
async function runAfterSave(ctx, kind, payload, lead) {
  try {
    return taskOutcome(await ctx.runTask(kind, payload));
  } catch (err) {
    if (err instanceof RecordError && (err.code === 'failed' || err.code === 'unavailable')) {
      throw new RecordError(err.code, `${lead} ${err.message}`);
    }
    throw err;
  }
}

const sentence = (msg) => (msg.endsWith('.') ? msg : `${msg}.`);

export const store = Object.freeze({
  async list(guildId) {
    const rows = await prisma.reactionRoleGroup.findMany({
      where: { guildId },
      include: { items: { orderBy: { position: 'asc' } } },
      orderBy: { createdAt: 'desc' },
      take: LIMITS.REACTION_ROLE_PANELS,
    });
    return rows.map(panelItem);
  },

  get: getItem,

  async create(guildId, data, ctx) {
    requireTasks(ctx);
    const layout = data.layout;
    const type = data.type;
    const items = data.items.length > 0 ? normalizeItems(data.items, layout, await loadResources(ctx, guildId, data.items)) : [];
    assertUsableEmojis(items);
    const id = randomUUID();
    await ensureGuildSettings(guildId);
    try {
      await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(lockSql(`rrpanel:${guildId}`));
        if (await tx.reactionRoleGroup.count({ where: { guildId } }) >= LIMITS.REACTION_ROLE_PANELS) {
          throw new RecordError('limit', `This server already has ${LIMITS.REACTION_ROLE_PANELS} reaction role panels, the maximum. Delete one first.`);
        }
        await tx.reactionRoleGroup.create({
          data: {
            id,
            guildId,
            channelId: data.channelId,
            // Not posted yet: the bot posts it right after and moves the row to the real message.
            messageId: UNPOSTED_MESSAGE_ID,
            messageMissingAt: new Date(),
            title: data.title.trim() || DEFAULT_TITLE,
            description: data.description ?? null,
            type,
            layout,
            maxRoles: type === 'NORMAL' ? (data.maxRoles ?? null) : null,
            plainText: data.plainText === true,
            messageContent: data.messageContent ?? null,
            attached: false,
            placeholder: layout === 'SELECT' ? (data.placeholder ?? null) : null,
          },
        });
        for (const [position, item] of items.entries()) {
          await tx.reactionRoleItem.create({ data: { groupId: id, ...rr.draftItem(id, item, position) } });
        }
      }, { timeout: 10_000 });
    } catch (err) {
      if (err?.code === 'P2002') throw new RecordError('conflict', 'That panel could not be saved because one of its roles is already in use. Try again.');
      throw err;
    }
    await rr.invalidatePanelIndex(guildId);
    ctx.logger?.info?.(`Dashboard reaction role panel ${id} created in ${guildId} by ${ctx.userId}`);

    await runAfterSave(ctx, 'rr.publish', { groupId: id, mode: 'post' },
      'The panel was saved but could not be posted yet. Fix the problem, then use "Repost" on it (or save it again) to post it.');
    return getItem(guildId, id);
  },

  async update(guildId, id, data, ctx) {
    requireTasks(ctx);
    const row = await loadRow(guildId, id);
    // The cross-field rules again, against the row as it is now (the request was validated against an earlier read).
    const problem = collection.check(data, { mode: 'update', current: panelItem(row) });
    if (problem) throw new RecordError('invalid', problem.message, { path: problem.path });

    const patch = {};
    for (const key of ['title', 'description', 'messageContent', 'plainText', 'type']) {
      if (Object.hasOwn(data, key) && data[key] !== row[key] && !(data[key] === null && row[key] === null)) patch[key] = data[key];
    }
    if (patch.title !== undefined) patch.title = patch.title.trim() || DEFAULT_TITLE;
    if ((patch.type ?? row.type) !== 'NORMAL') {
      // Like /reactionrole edit: the role limit only exists in Normal mode.
      if (row.maxRoles !== null) patch.maxRoles = null;
    } else if (Object.hasOwn(data, 'maxRoles') && (data.maxRoles ?? null) !== (row.maxRoles ?? null)) {
      patch.maxRoles = data.maxRoles ?? null;
    }
    if (row.layout === 'SELECT' && Object.hasOwn(data, 'placeholder') && (data.placeholder ?? null) !== (row.placeholder ?? null)) {
      patch.placeholder = data.placeholder ?? null;
    }

    let diff = null;
    if (Object.hasOwn(data, 'items')) {
      const held = new Set(row.items.map((i) => emojiKey(i.emoji)).filter(Boolean));
      const next = normalizeItems(data.items, row.layout, await loadResources(ctx, guildId, data.items), held);
      assertUsableEmojis(next);
      try {
        diff = await prisma.$transaction(async (tx) => {
          await tx.$queryRaw(lockSql(`rritems:${id}`));
          if (!await tx.reactionRoleGroup.findFirst({ where: { id, guildId }, select: { id: true } })) throw notFound();
          // The rows as they are now, under the lock.
          const stored = await tx.reactionRoleItem.findMany({ where: { groupId: id }, orderBy: { position: 'asc' } });
          const plan = diffPanelItems(stored, next);
          for (const gone of plan.remove) await tx.reactionRoleItem.delete({ where: { id: gone.id } });
          for (const u of plan.update) await tx.reactionRoleItem.update({ where: { id: u.id }, data: u.data });
          for (const c of plan.create) await tx.reactionRoleItem.create({ data: { groupId: id, ...rr.draftItem(id, c, c.position) } });
          return plan;
        }, { timeout: 10_000 });
      } catch (err) {
        if (err?.code === 'P2002') throw new RecordError('conflict', 'One of those roles was just added to the panel by someone else. Reload and try again.', { path: 'items' });
        throw err;
      }
    }
    if (Object.keys(patch).length > 0 && !await rr.updatePanel(guildId, id, patch)) throw notFound();
    if (diff?.changed || Object.keys(patch).length > 0) await rr.invalidatePanelIndex(guildId);

    // The posted message follows the saved panel. An attached message is never edited: only its reactions change. A new role
    // mode or limit also reaches the bot's panel index through the task, so it applies at once.
    const messageChanged = ['title', 'description', 'messageContent', 'plainText', 'type', 'maxRoles', 'placeholder'].some((k) => k in patch);
    const unposted = row.messageId === UNPOSTED_MESSAGE_ID;
    if (diff?.changed || (!row.attached && messageChanged) || 'type' in patch || 'maxRoles' in patch || unposted) {
      await runAfterSave(ctx, 'rr.publish', { groupId: id, mode: 'sync', removedEmojis: diff?.removedEmojis ?? [] },
        'Your changes were saved, but the posted panel was not fully updated. Use "Repost" to post the panel again.');
    }
    ctx.logger?.info?.(`Dashboard reaction role panel ${id} updated in ${guildId} by ${ctx.userId}`);
    return getItem(guildId, id);
  },

  async remove(guildId, id, ctx) {
    const row = await loadRow(guildId, id);
    if (row.messageId === UNPOSTED_MESSAGE_ID) {
      // Never posted: there is no message to delete and no reaction to take back, so the bot is not needed.
      await rr.deletePanel(guildId, id);
      await rr.invalidatePanelIndex(guildId);
    } else {
      requireTasks(ctx);
      taskOutcome(await ctx.runTask('rr.delete', { groupId: id }));
    }
    ctx.logger?.info?.(`Dashboard reaction role panel ${id} deleted in ${guildId} by ${ctx.userId}`);
  },

  actions: Object.freeze({
    /** Posts the panel again (another channel, or after its message was deleted): `{ channelId }`. */
    async repost(guildId, id, data, ctx) {
      requireTasks(ctx);
      await loadRow(guildId, id);
      const out = taskOutcome(await ctx.runTask('rr.repost', { groupId: id, channelId: data.channelId ?? null }));
      return { item: await getItem(guildId, id), message: sentence(out.message) };
    },
  }),
});
