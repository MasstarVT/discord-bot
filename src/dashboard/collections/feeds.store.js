// Database access of the `feeds` collection: the Feed rows /feed writes. Adding resolves and probes the source with the
// same resolveFeedSource (SSRF-safe fetches through safeFetch; the probe seeds the feed so no backlog is posted) and
// schedules `feed.poll` (dedupe `feed.poll:{id}`) like /feed add; enabling and disabling follow /feed enable|disable
// (a re-enable clears the failures and disabledReason and re-seeds). The bot's in-process create mutex does not reach
// this process, so the cap is guarded by a per-guild advisory lock here (the row mapping mirrors createFeedRow).
import { Prisma } from '@prisma/client';
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { cancelJobs } from '../../services/scheduler.js';
import { ensureGuildSettings } from '../../services/settingsService.js';
import {
  BASE_INTERVAL_MS, SEEN_CAP, describeFetchError, feedJobKey, feedStatus, fetchFeedSource, parseMessageOption,
  resolveFeedSource, scheduleFeedJob, sourceLink,
} from '../../services/feedService.js';
import { createThrottle } from '../../utils/async.js';
import { clampText } from '../../utils/text.js';
import { RecordError } from './errors.js';
import { isDiscordApiError, memberFor, pingRoleProblem } from './embeds.js';
import { feedItem, parseFeedSource } from './feeds.js';

const lockSql = (key) => Prisma.sql`SELECT 1 AS ok FROM (SELECT pg_advisory_xact_lock(hashtext(${key}))) AS l`;

/** Raw statements (test/db.test.js runs each one). */
export const RAW_SQL = Object.freeze([
  { name: 'dashboard.feeds.lockGuild', build: () => lockSql('feed-create:0') },
]);

const notFound = () => new RecordError('not_found', 'That feed no longer exists.');
const limitError = () => new RecordError('limit', `This server already follows ${LIMITS.FEEDS} feeds, the maximum. Remove one first.`);
const testThrottle = createThrottle(15_000); // like /feed test: one check per feed every 15 s

function toItem(row) {
  const status = feedStatus(row);
  return feedItem(row, {
    statusText: `${status.icon} ${status.text}`,
    sourceUrl: sourceLink(row).replace(/^<|>$/g, ''),
  });
}

async function findRow(guildId, id) {
  const row = await prisma.feed.findFirst({ where: { id, guildId } });
  if (!row) throw notFound();
  return row;
}

async function loadResources(ctx, guildId) {
  if (typeof ctx?.discord?.getGuildResources !== 'function') {
    throw new RecordError('unavailable', 'The server cannot be read from the dashboard right now.');
  }
  try {
    return await ctx.discord.getGuildResources(guildId);
  } catch (err) {
    if (isDiscordApiError(err)) throw new RecordError('discord', 'The server could not be loaded from Discord. Try again in a moment.');
    throw err;
  }
}

async function checkPingRole(guildId, roleId, ctx) {
  const resources = await loadResources(ctx, guildId);
  const problem = pingRoleProblem(resources, roleId, memberFor(ctx, resources));
  if (problem) throw new RecordError('invalid', problem, { path: 'pingRoleId' });
}

/** The message as stored: `\n` typed literally becomes a new line, blank means the type's default (null). */
function messageOf(raw) {
  const parsed = parseMessageOption(raw);
  if (parsed.error) throw new RecordError('invalid', parsed.error, { path: 'messageTemplate' });
  return parsed.template;
}

export const store = Object.freeze({
  async list(guildId) {
    const rows = await prisma.feed.findMany({ where: { guildId }, orderBy: { createdAt: 'asc' }, take: LIMITS.FEEDS });
    return rows.map(toItem);
  },

  async get(guildId, id) {
    const row = await prisma.feed.findFirst({ where: { id, guildId } });
    return row ? toItem(row) : null;
  },

  async create(guildId, data, ctx) {
    const { type } = data;
    const src = parseFeedSource(type, data.source);
    if (!src.ok) throw new RecordError('invalid', src.message, { path: 'source' });
    const messageTemplate = messageOf(data.messageTemplate);
    if (data.includeReposts && type !== 'BLUESKY') {
      throw new RecordError('invalid', 'Only Bluesky feeds can include reposts.', { path: 'includeReposts' });
    }
    if (data.pingRoleId) await checkPingRole(guildId, data.pingRoleId, ctx);
    // Fast cap check before any network request (enforced again under the lock).
    if (await prisma.feed.count({ where: { guildId } }) >= LIMITS.FEEDS) throw limitError();

    const r = await resolveFeedSource(type, src.input);
    if (!r.ok) throw new RecordError('invalid', `${r.title}: ${String(r.message).replace(/\*\*/g, '').replace(/`/g, '')}`, { path: 'source' });
    const dup = await prisma.feed.findFirst({ where: { guildId, type, source: r.source, channelId: data.channelId }, select: { id: true } });
    if (dup) throw new RecordError('conflict', 'This feed is already posted in that channel.', { path: 'source' });

    await ensureGuildSettings(guildId);
    const now = new Date();
    const probe = r.probe ?? null;
    const seeded = Boolean(probe?.ok);
    const enabled = data.enabled !== false;
    let row;
    try {
      row = await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(lockSql(`feed-create:${guildId}`));
        if (await tx.feed.count({ where: { guildId } }) >= LIMITS.FEEDS) throw limitError();
        return tx.feed.create({
          data: {
            guildId, type, source: r.source, displayName: clampText(r.displayName, 100), fetchUrl: r.fetchUrl,
            channelId: data.channelId, pingRoleId: data.pingRoleId ?? null, messageTemplate,
            includeReposts: type === 'BLUESKY' && data.includeReposts === true, createdBy: String(ctx?.userId ?? '0'),
            initialized: seeded,
            seenIds: seeded ? [...new Set(probe.items.map((i) => i.id))].slice(0, SEEN_CAP) : [],
            etag: seeded ? probe.etag : null,
            lastModified: seeded ? probe.lastModified : null,
            lastCheckedAt: probe ? now : null,
            lastSuccessAt: seeded ? now : null,
            lastError: probe && !probe.ok ? describeFetchError(type, probe.error) : null,
            enabled,
            disabledReason: enabled ? null : 'Paused by an admin',
          },
        });
      });
    } catch (err) {
      if (err?.code === 'P2002') throw new RecordError('conflict', 'This feed is already posted in that channel.', { path: 'source' });
      throw err;
    }
    if (enabled) {
      try {
        // The first check: a full interval after a seeding probe, two minutes when the probe failed (as /feed add).
        await scheduleFeedJob(row, { runAt: new Date(Date.now() + (row.initialized ? BASE_INTERVAL_MS[type] : 2 * 60_000)) });
      } catch (err) {
        // The row exists, so the add is reported as done; the bot's guildReady repair (ensureGuildFeedJobs) or
        // turning the feed off and on again schedules it.
        ctx?.logger?.error?.(`Dashboard: could not schedule feed ${row.id}`, err);
      }
    }
    return toItem(row);
  },

  async update(guildId, id, data, ctx) {
    const row = await findRow(guildId, id);
    const patch = {};
    if (Object.hasOwn(data, 'channelId') && data.channelId !== row.channelId) {
      patch.channelId = data.channelId;
      if (row.failureCount === 0) patch.lastError = null; // it was a channel problem
    }
    if (Object.hasOwn(data, 'pingRoleId') && data.pingRoleId !== row.pingRoleId) {
      if (data.pingRoleId) await checkPingRole(guildId, data.pingRoleId, ctx);
      patch.pingRoleId = data.pingRoleId;
    }
    if (Object.hasOwn(data, 'messageTemplate')) {
      const message = messageOf(data.messageTemplate);
      if (message !== row.messageTemplate) patch.messageTemplate = message;
    }
    if (Object.hasOwn(data, 'includeReposts') && data.includeReposts !== row.includeReposts) {
      if (row.type !== 'BLUESKY') throw new RecordError('invalid', 'Only Bluesky feeds can include reposts.', { path: 'includeReposts' });
      patch.includeReposts = data.includeReposts;
    }
    let job = null;
    if (Object.hasOwn(data, 'enabled') && data.enabled !== row.enabled) {
      if (data.enabled) {
        const channelId = patch.channelId ?? row.channelId;
        const resources = await loadResources(ctx, guildId);
        if (!(resources.channels ?? []).some((c) => c.id === channelId)) {
          throw new RecordError('invalid', 'Its channel no longer exists. Pick a new channel first.', { path: 'channelId' });
        }
        // /feed enable: the first check re-seeds, so items published while it was off are skipped.
        Object.assign(patch, { enabled: true, failureCount: 0, lastError: null, disabledReason: null, initialized: false });
        job = 'schedule';
      } else {
        Object.assign(patch, { enabled: false, disabledReason: 'Paused by an admin' });
        job = 'cancel';
      }
    }
    if (!Object.keys(patch).length) return toItem(row);
    try {
      const { count } = await prisma.feed.updateMany({ where: { id, guildId }, data: patch });
      if (!count) throw notFound();
    } catch (err) {
      if (err?.code === 'P2002') throw new RecordError('conflict', 'That channel already has this feed.', { path: 'channelId' });
      throw err;
    }
    const updated = await findRow(guildId, id);
    if (job === 'schedule') await scheduleFeedJob(updated, { runAt: new Date(Date.now() + 10_000) });
    else if (job === 'cancel') await cancelJobs({ dedupeKey: feedJobKey(id) });
    return toItem(updated);
  },

  async remove(guildId, id) {
    const { count } = await prisma.feed.deleteMany({ where: { id, guildId } });
    if (!count) throw notFound();
    await cancelJobs({ dedupeKey: feedJobKey(id) });
  },

  actions: Object.freeze({
    /** /feed test: a fresh, unconditional fetch; it may refresh the shared fetch cache and never writes the row. */
    async test(guildId, id) {
      const row = await findRow(guildId, id);
      if (!testThrottle(row.id)) throw new RecordError('invalid', 'This feed was just checked. Try again in a few seconds.');
      const res = await fetchFeedSource(row, { useCache: false, conditional: false });
      if (!res.ok) return { item: toItem(row), message: `The check failed: ${describeFetchError(row.type, res.error)}. Nothing was changed.` };
      const items = (res.items ?? []).filter((i) => row.includeReposts || !i.isRepost);
      if (!items.length) return { item: toItem(row), message: 'The feed answered but has no items to show yet.' };
      const newest = items[0];
      const title = clampText(String(newest.title ?? '').trim() || 'untitled', 150);
      return {
        item: toItem(row),
        message: `The feed works: ${res.items.length} item(s). Newest: "${title}"${newest.url ? ` (${clampText(newest.url, 300)})` : ''}.`,
      };
    },
  }),
});
