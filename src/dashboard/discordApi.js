// Discord REST access for the dashboard: the logged-in user's profile and guild list (user token, cached),
// whether the bot is in a guild (bot token, cached), the invite URL and CDN image URLs.
// Every call has a timeout and every cache is bounded; no timers, so nothing keeps the process alive.
import { createHash } from 'node:crypto';
import { PermissionFlagsBits, RouteBases, Routes } from 'discord.js';
import { exchangeCode, revokeToken } from './auth.js';

export const REQUEST_TIMEOUT_MS = 10_000;
export const USER_GUILDS_TTL_MS = 60_000;
export const USER_GUILDS_MAX_STALE_MS = 5 * 60_000;
export const USER_GUILDS_CACHE_MAX = 5_000;
export const BOT_GUILDS_TTL_MS = 60_000;
export const BOT_GUILDS_MAX_PAGES = 100;
const BOT_MISS_TTL_MS = 5_000;
const BOT_MISS_MAX = 1_000;
const SNOWFLAKE_RE = /^\d{17,20}$/;
const IMAGE_HASH_RE = /^(?:a_)?[0-9a-f]{32}$/;

/** What the invite asks for: what the modules use (README "Setup"), never Administrator. */
export const INVITE_PERMISSIONS = [
  'ViewChannel', 'SendMessages', 'SendMessagesInThreads', 'EmbedLinks', 'AttachFiles', 'ReadMessageHistory',
  'AddReactions', 'UseExternalEmojis', 'ManageMessages', 'ManageRoles', 'ManageChannels', 'ManageGuild',
  'ManageGuildExpressions', 'ViewAuditLog', 'CreateInstantInvite', 'KickMembers', 'BanMembers', 'ModerateMembers',
  'ManageNicknames', 'ManageThreads', 'MoveMembers', 'Connect', 'Speak',
].reduce((bits, name) => bits | PermissionFlagsBits[name], 0n);

/** A Discord REST failure. `code`: unauthenticated (401), forbidden, not_found, rate_limited or unavailable. */
export class DiscordApiError extends Error {
  /**
   * @param {string} message
   * @param {{ status?: number, code: string, retryAfter?: number }} info retryAfter in whole seconds
   */
  constructor(message, { status = 0, code, retryAfter } = {}) {
    super(message);
    this.name = 'DiscordApiError';
    this.status = status;
    this.code = code;
    if (retryAfter !== undefined) this.retryAfter = retryAfter;
  }
}

const codeForStatus = (status) => {
  if (status === 401) return 'unauthenticated';
  if (status === 403) return 'forbidden';
  if (status === 404) return 'not_found';
  if (status === 429) return 'rate_limited';
  return 'unavailable';
};

async function retryAfterSeconds(res) {
  const body = await res.json().catch(() => null);
  const fromBody = Number(body?.retry_after);
  const fromHeader = Number(res.headers?.get?.('retry-after'));
  const seconds = Number.isFinite(fromBody) && fromBody > 0 ? fromBody : Number.isFinite(fromHeader) && fromHeader > 0 ? fromHeader : 1;
  return Math.min(3600, Math.max(1, Math.ceil(seconds)));
}

/**
 * GET a Discord API route and parse the JSON body.
 * @throws {DiscordApiError}
 */
async function getJson(fetchImpl, route, authorization) {
  let res;
  try {
    res = await fetchImpl(`${RouteBases.api}${route}`, {
      headers: { Authorization: authorization, Accept: 'application/json' },
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      redirect: 'error',
    });
  } catch (err) {
    throw new DiscordApiError(`Discord request failed: ${err?.name === 'TimeoutError' ? 'timed out' : err?.message ?? err}`, { code: 'unavailable' });
  }
  if (!res.ok) {
    const retryAfter = res.status === 429 ? await retryAfterSeconds(res) : undefined;
    if (res.status !== 429) await res.body?.cancel?.().catch(() => {});
    throw new DiscordApiError(`Discord returned HTTP ${res.status} for ${route.split('?')[0]}`, {
      status: res.status, code: codeForStatus(res.status), retryAfter,
    });
  }
  try {
    return await res.json();
  } catch {
    throw new DiscordApiError(`Discord returned invalid JSON for ${route.split('?')[0]}`, { status: res.status, code: 'unavailable' });
  }
}

const tokenFingerprint = (token) => createHash('sha256').update(String(token)).digest('base64url').slice(0, 22);

function normalizeGuild(g) {
  if (!g || typeof g !== 'object' || typeof g.id !== 'string' || !SNOWFLAKE_RE.test(g.id)) return null;
  return Object.freeze({
    id: g.id,
    name: typeof g.name === 'string' ? g.name : 'Unknown server',
    icon: typeof g.icon === 'string' ? g.icon : null,
    owner: g.owner === true,
    permissions: typeof g.permissions === 'string' ? g.permissions : '0',
  });
}

/**
 * The avatar URL of a user (the default avatar when there is no custom one).
 * @param {{ id: string, avatar?: string|null }} user
 * @returns {string}
 */
export function userAvatarUrl(user) {
  const id = typeof user?.id === 'string' && SNOWFLAKE_RE.test(user.id) ? user.id : '0';
  if (typeof user?.avatar === 'string' && IMAGE_HASH_RE.test(user.avatar)) {
    return `${RouteBases.cdn}/avatars/${id}/${user.avatar}.webp?size=128`;
  }
  return `${RouteBases.cdn}/embed/avatars/${(BigInt(id) >> 22n) % 6n}.png`;
}

/**
 * The icon URL of a guild, or null when it has none.
 * @param {{ id: string, icon?: string|null }} guild
 * @returns {string|null}
 */
export function guildIconUrl(guild) {
  if (typeof guild?.id !== 'string' || !SNOWFLAKE_RE.test(guild.id)) return null;
  if (typeof guild.icon !== 'string' || !IMAGE_HASH_RE.test(guild.icon)) return null;
  return `${RouteBases.cdn}/icons/${guild.id}/${guild.icon}.webp?size=128`;
}

/**
 * Creates the dashboard's Discord client.
 * @param {{ config: { clientId: string, clientSecret: string, botToken: string, redirectUri: string },
 *           fetchImpl?: typeof fetch, now?: () => number, logger?: object }} opts
 */
export function createDiscordApi({ config, fetchImpl = fetch, now = Date.now, logger = null }) {
  const bot = `Bot ${config.botToken}`;

  /** userId -> { fp, guilds, fetchedAt }; insertion order = age. */
  const userGuilds = new Map();
  /** `${userId}:${fp}` -> Promise */
  const userInflight = new Map();

  let botGuilds = null;
  let botGuildsAt = 0;
  let botInflight = null;
  /** guildId -> expiry of a recent "bot is not there" answer. */
  const botMisses = new Map();

  function rememberUserGuilds(userId, entry) {
    userGuilds.delete(userId);
    userGuilds.set(userId, entry);
    while (userGuilds.size > USER_GUILDS_CACHE_MAX) userGuilds.delete(userGuilds.keys().next().value);
  }

  async function fetchUserGuilds(userId, accessToken, fp) {
    try {
      const data = await getJson(fetchImpl, `${Routes.userGuilds()}?limit=200`, `Bearer ${accessToken}`);
      if (!Array.isArray(data)) throw new DiscordApiError('Discord returned an unexpected guild list', { code: 'unavailable' });
      const guilds = Object.freeze(data.map(normalizeGuild).filter(Boolean));
      rememberUserGuilds(userId, { fp, guilds, fetchedAt: now() });
      return guilds;
    } catch (err) {
      if (err instanceof DiscordApiError && err.code === 'unauthenticated') userGuilds.delete(userId);
      if (err instanceof DiscordApiError && (err.code === 'rate_limited' || err.code === 'unavailable')) {
        const stale = userGuilds.get(userId);
        if (stale && stale.fp === fp && now() - stale.fetchedAt <= USER_GUILDS_MAX_STALE_MS) return stale.guilds;
      }
      throw err;
    }
  }

  async function refreshBotGuilds() {
    const ids = new Set();
    let after = '0';
    for (let page = 0; page < BOT_GUILDS_MAX_PAGES; page++) {
      const data = await getJson(fetchImpl, `${Routes.userGuilds()}?limit=200&after=${after}`, bot);
      if (!Array.isArray(data)) throw new DiscordApiError('Discord returned an unexpected guild list', { code: 'unavailable' });
      for (const g of data) if (typeof g?.id === 'string' && SNOWFLAKE_RE.test(g.id)) ids.add(g.id);
      if (data.length < 200) break;
      const last = data[data.length - 1]?.id;
      if (typeof last !== 'string' || !SNOWFLAKE_RE.test(last)) break;
      after = last;
    }
    botGuilds = ids;
    botGuildsAt = now();
    botMisses.clear();
    return ids;
  }

  /**
   * The bot's guild ids, refreshed at most every BOT_GUILDS_TTL_MS. A failed refresh keeps the old set.
   * @returns {Promise<Set<string>>}
   * @throws {DiscordApiError} only when no list was ever loaded
   */
  async function getBotGuildIds() {
    if (botGuilds && now() - botGuildsAt < BOT_GUILDS_TTL_MS) return botGuilds;
    if (!botInflight) {
      botInflight = refreshBotGuilds().finally(() => { botInflight = null; });
    }
    try {
      return await botInflight;
    } catch (err) {
      if (botGuilds) {
        logger?.warn?.(`Dashboard: could not refresh the bot's server list (${err.message}); using the previous one.`);
        botGuildsAt = now(); // retry after another TTL instead of on every request
        return botGuilds;
      }
      throw err;
    }
  }

  return {
    /**
     * The logged-in user's profile.
     * @param {string} accessToken
     * @returns {Promise<{ id: string, username: string, globalName: string|null, avatar: string|null }>}
     * @throws {DiscordApiError}
     */
    async getCurrentUser(accessToken) {
      const u = await getJson(fetchImpl, Routes.user(), `Bearer ${accessToken}`);
      if (typeof u?.id !== 'string' || !SNOWFLAKE_RE.test(u.id)) {
        throw new DiscordApiError('Discord returned an unexpected user', { code: 'unavailable' });
      }
      return {
        id: u.id,
        username: typeof u.username === 'string' ? u.username : 'unknown',
        globalName: typeof u.global_name === 'string' ? u.global_name : null,
        avatar: typeof u.avatar === 'string' ? u.avatar : null,
      };
    },

    /**
     * The user's guilds (`{ id, name, icon, owner, permissions }`), cached for 60 s per user and token and
     * shared by concurrent callers. On a Discord 429 or outage a recent cached list is served instead.
     * @param {string} userId
     * @param {string} accessToken
     * @returns {Promise<ReadonlyArray<object>>}
     * @throws {DiscordApiError} unauthenticated, rate_limited (with retryAfter) or unavailable
     */
    async getUserGuilds(userId, accessToken) {
      const fp = tokenFingerprint(accessToken);
      const hit = userGuilds.get(userId);
      if (hit && hit.fp === fp && now() - hit.fetchedAt < USER_GUILDS_TTL_MS) return hit.guilds;
      const key = `${userId}:${fp}`;
      let pending = userInflight.get(key);
      if (!pending) {
        pending = fetchUserGuilds(userId, accessToken, fp).finally(() => userInflight.delete(key));
        userInflight.set(key, pending);
      }
      return pending;
    },

    /** Drops the cached guild list of a user (after logout). */
    forgetUser(userId) {
      userGuilds.delete(userId);
    },

    getBotGuildIds,

    /**
     * Whether the bot is in a guild. Uses the cached list; with `verify` (default) a guild missing from it
     * is double-checked with GET /guilds/{id} (403/404 = absent), so a fresh invite shows up at once.
     * @param {string} guildId
     * @param {{ verify?: boolean }} [opts]
     * @returns {Promise<boolean>}
     * @throws {DiscordApiError} when Discord cannot answer
     */
    async isBotInGuild(guildId, { verify = true } = {}) {
      if (typeof guildId !== 'string' || !SNOWFLAKE_RE.test(guildId)) return false;
      let ids = null;
      try {
        ids = await getBotGuildIds();
      } catch (err) {
        if (!verify) throw err;
      }
      if (ids?.has(guildId)) return true;
      if (!verify) return false;

      const missUntil = botMisses.get(guildId);
      if (missUntil && missUntil > now()) return false;
      try {
        await getJson(fetchImpl, Routes.guild(guildId), bot);
      } catch (err) {
        if (err instanceof DiscordApiError && (err.code === 'forbidden' || err.code === 'not_found')) {
          botMisses.delete(guildId);
          botMisses.set(guildId, now() + BOT_MISS_TTL_MS);
          while (botMisses.size > BOT_MISS_MAX) botMisses.delete(botMisses.keys().next().value);
          return false;
        }
        throw err;
      }
      botGuilds?.add(guildId);
      return true;
    },

    /**
     * The bot invite link for one guild (bot + applications.commands, guild preselected).
     * @param {string} guildId
     * @returns {string}
     */
    inviteUrl(guildId) {
      const params = new URLSearchParams({
        client_id: config.clientId,
        scope: 'bot applications.commands',
        permissions: INVITE_PERMISSIONS.toString(),
      });
      if (typeof guildId === 'string' && SNOWFLAKE_RE.test(guildId)) {
        params.set('guild_id', guildId);
        params.set('disable_guild_select', 'true');
      }
      return `https://discord.com/oauth2/authorize?${params}`;
    },

    /** @see exchangeCode in auth.js */
    exchangeCode: (code) => exchangeCode({ config, fetchImpl }, code),

    /** @see revokeToken in auth.js */
    revokeToken: (accessToken) => revokeToken({ config, fetchImpl }, accessToken),
  };
}
