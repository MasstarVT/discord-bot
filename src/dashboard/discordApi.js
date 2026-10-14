// Discord REST access for the dashboard: the logged-in user's profile and guild list (user token, cached),
// whether the bot is in a guild, a guild's channels, roles and emojis, and message writes (bot token), the invite URL
// and CDN image URLs.
// Every call has a timeout and every cache is bounded; the only timer is the short unref'd wait before a
// rate-limited bot-guild page is retried, so nothing keeps the process alive.
import { createHash } from 'node:crypto';
import { PermissionFlagsBits, RouteBases, Routes } from 'discord.js';
import { exchangeCode, revokeToken } from './auth.js';
import { channelKind } from './schema.js';

export const REQUEST_TIMEOUT_MS = 10_000;
export const USER_GUILDS_TTL_MS = 60_000;
export const USER_GUILDS_MAX_STALE_MS = 5 * 60_000;
export const USER_GUILDS_CACHE_MAX = 5_000;
export const BOT_GUILDS_TTL_MS = 60_000;
export const BOT_GUILDS_MAX_PAGES = 100;
export const BOT_GUILDS_MAX_RETRIES = 3;
export const BOT_GUILDS_MAX_WAIT_SECONDS = 10;
export const RESOURCES_TTL_MS = 30_000;
export const RESOURCES_CACHE_MAX = 500;
export const USERS_TTL_MS = 60 * 60_000;
export const USERS_CACHE_MAX = 5_000;
export const MEMBER_SEARCH_MAX = 25;
const USER_LOOKUP_CONCURRENCY = 4;
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

/** Permissions a role the bot hands out must not carry (checkAssignableRole in src/utils/interactionHelpers.js). */
export const DANGEROUS_PERMISSIONS = [
  'Administrator', 'ManageGuild', 'ManageRoles', 'BanMembers', 'KickMembers',
  'ManageChannels', 'ManageWebhooks', 'MentionEveryone', 'ModerateMembers', 'ManageMessages',
].reduce((bits, name) => bits | PermissionFlagsBits[name], 0n);

/**
 * A Discord REST failure. `code`: unauthenticated (401), forbidden, not_found, rate_limited or unavailable;
 * `discordCode`: Discord's JSON error code when it sent one (10003 unknown channel, 10008 unknown message,
 * 50001 missing access, 50013 missing permissions, 50035 invalid form body, ...).
 */
export class DiscordApiError extends Error {
  /**
   * @param {string} message
   * @param {{ status?: number, code: string, retryAfter?: number, discordCode?: number }} info retryAfter in whole seconds
   */
  constructor(message, { status = 0, code, retryAfter, discordCode } = {}) {
    super(message);
    this.name = 'DiscordApiError';
    this.status = status;
    this.code = code;
    if (retryAfter !== undefined) this.retryAfter = retryAfter;
    if (Number.isInteger(discordCode)) this.discordCode = discordCode;
  }
}

const codeForStatus = (status) => {
  if (status === 401) return 'unauthenticated';
  if (status === 403) return 'forbidden';
  if (status === 404) return 'not_found';
  if (status === 429) return 'rate_limited';
  return 'unavailable';
};

function retryAfterSeconds(res, body) {
  const fromBody = Number(body?.retry_after);
  const fromHeader = Number(res.headers?.get?.('retry-after'));
  const seconds = Number.isFinite(fromBody) && fromBody > 0 ? fromBody : Number.isFinite(fromHeader) && fromHeader > 0 ? fromHeader : 1;
  return Math.min(3600, Math.max(1, Math.ceil(seconds)));
}

/**
 * Calls a Discord API route and parses the JSON answer (null for 204 No Content).
 * @param {typeof fetch} fetchImpl
 * @param {string} method
 * @param {string} route
 * @param {string} authorization
 * @param {object} [body] sent as JSON
 * @throws {DiscordApiError}
 */
async function requestJson(fetchImpl, method, route, authorization, body) {
  const where = `${method} ${route.split('?')[0]}`;
  const headers = { Authorization: authorization, Accept: 'application/json' };
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  let res;
  try {
    res = await fetchImpl(`${RouteBases.api}${route}`, {
      method,
      headers,
      ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      redirect: 'error',
    });
  } catch (err) {
    throw new DiscordApiError(`Discord request failed: ${err?.name === 'TimeoutError' ? 'timed out' : err?.message ?? err}`, { code: 'unavailable' });
  }
  if (!res.ok) {
    const data = await res.json().catch(() => null);
    throw new DiscordApiError(`Discord returned HTTP ${res.status} for ${where}`, {
      status: res.status,
      code: codeForStatus(res.status),
      retryAfter: res.status === 429 ? retryAfterSeconds(res, data) : undefined,
      discordCode: Number.isInteger(data?.code) ? data.code : undefined,
    });
  }
  if (res.status === 204) {
    await res.body?.cancel?.().catch(() => {});
    return null;
  }
  try {
    return await res.json();
  } catch {
    throw new DiscordApiError(`Discord returned invalid JSON for ${where}`, { status: res.status, code: 'unavailable' });
  }
}

/** GET a Discord API route and parse the JSON body. @throws {DiscordApiError} */
const getJson = (fetchImpl, route, authorization) => requestJson(fetchImpl, 'GET', route, authorization);

const defaultSleep = (ms) => new Promise((resolve) => { setTimeout(resolve, ms).unref(); });

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

const compareIds = (a, b) => {
  const x = BigInt(a);
  const y = BigInt(b);
  return x < y ? -1 : x > y ? 1 : 0;
};
const nameCollator = new Intl.Collator('en', { sensitivity: 'base', numeric: true });
const VOICE_TYPES = new Set([2, 13]);
const isInt = (v) => Number.isInteger(v);

/**
 * Channels in Discord's sidebar order: channels without a category first, then each category followed by its
 * channels; inside a parent, text-like channels before voice-like ones, then by position and id.
 * @param {object[]} channels mapped channels ({ id, type, kind, parentId, position })
 */
export function sortChannels(channels) {
  const inParent = (a, b) => (Number(VOICE_TYPES.has(a.type)) - Number(VOICE_TYPES.has(b.type)))
    || a.position - b.position || compareIds(a.id, b.id);
  const categories = channels.filter((c) => c.kind === 'category').sort((a, b) => a.position - b.position || compareIds(a.id, b.id));
  const catIds = new Set(categories.map((c) => c.id));
  const children = new Map(categories.map((c) => [c.id, []]));
  const top = [];
  for (const c of channels) {
    if (c.kind === 'category') continue;
    if (c.parentId && catIds.has(c.parentId)) children.get(c.parentId).push(c);
    else top.push(c);
  }
  return [...top.sort(inParent), ...categories.flatMap((cat) => [cat, ...children.get(cat.id).sort(inParent)])];
}

const roleColor = (n) => (isInt(n) && n > 0 && n <= 0xffffff ? `#${n.toString(16).padStart(6, '0').toUpperCase()}` : null);

/** The REST answers of one guild in the shapes of getGuildResources. */
function mapGuildResources(guildId, botUserId, guild, rawChannels, rawRoles, rawEmojis, botMember) {
  const roleList = (Array.isArray(rawRoles) ? rawRoles : [])
    .filter((r) => typeof r?.id === 'string' && SNOWFLAKE_RE.test(r.id));
  const positionOf = new Map(roleList.map((r) => [r.id, isInt(r.position) ? r.position : 0]));
  const botRoleIds = Array.isArray(botMember?.roles) ? botMember.roles.filter((id) => positionOf.has(id)) : [];
  const topRolePosition = Math.max(0, ...botRoleIds.map((id) => positionOf.get(id)));
  const permsOf = (r) => {
    try { return BigInt(r?.permissions ?? 0); } catch { return 0n; }
  };
  const everyone = roleList.find((r) => r.id === guildId);
  const botPerms = [everyone, ...botRoleIds.map((id) => roleList.find((r) => r.id === id))]
    .reduce((bits, r) => bits | permsOf(r), 0n);
  const has = (flag) => (botPerms & PermissionFlagsBits.Administrator) !== 0n || (botPerms & flag) === flag;

  const roles = roleList.map((r) => {
    const position = positionOf.get(r.id);
    const managed = r.managed === true;
    const isEveryone = r.id === guildId;
    return {
      id: r.id,
      name: typeof r.name === 'string' ? r.name : 'unknown role',
      color: roleColor(r.color),
      position,
      managed,
      everyone: isEveryone,
      mentionable: r.mentionable === true,
      assignable: !managed && !isEveryone && position < topRolePosition,
      dangerous: (permsOf(r) & DANGEROUS_PERMISSIONS) !== 0n,
    };
  }).sort((a, b) => b.position - a.position || compareIds(a.id, b.id));

  const channels = sortChannels((Array.isArray(rawChannels) ? rawChannels : [])
    .filter((c) => typeof c?.id === 'string' && SNOWFLAKE_RE.test(c.id) && isInt(c.type))
    .map((c) => ({
      id: c.id,
      name: typeof c.name === 'string' ? c.name : 'unknown',
      type: c.type,
      kind: channelKind(c.type),
      parentId: typeof c.parent_id === 'string' && SNOWFLAKE_RE.test(c.parent_id) ? c.parent_id : null,
      position: isInt(c.position) ? c.position : 0,
      nsfw: c.nsfw === true,
    })));

  const emojis = (Array.isArray(rawEmojis) ? rawEmojis : [])
    .filter((e) => typeof e?.id === 'string' && SNOWFLAKE_RE.test(e.id) && typeof e.name === 'string')
    .map((e) => ({ id: e.id, name: e.name, animated: e.animated === true }))
    .sort((a, b) => nameCollator.compare(a.name, b.name) || compareIds(a.id, b.id));

  const count = guild?.approximate_member_count ?? guild?.member_count;
  return deepFreeze({
    guild: {
      id: guildId,
      name: typeof guild?.name === 'string' ? guild.name : 'Unknown server',
      icon: typeof guild?.icon === 'string' && IMAGE_HASH_RE.test(guild.icon) ? guild.icon : null,
      memberCount: isInt(count) ? count : null,
      ownerId: typeof guild?.owner_id === 'string' && SNOWFLAKE_RE.test(guild.owner_id) ? guild.owner_id : null,
      premiumTier: isInt(guild?.premium_tier) ? guild.premium_tier : 0,
      premiumSubscriptionCount: isInt(guild?.premium_subscription_count) ? guild.premium_subscription_count : 0,
      verificationLevel: isInt(guild?.verification_level) ? guild.verification_level : 0,
    },
    channels,
    roles,
    emojis,
    bot: { userId: botUserId, topRolePosition, manageRoles: has(PermissionFlagsBits.ManageRoles) },
  });
}

function deepFreeze(v) {
  if (v && typeof v === 'object') {
    for (const child of Object.values(v)) deepFreeze(child);
    Object.freeze(v);
  }
  return v;
}

/** allowed_mentions for a dashboard write: never @everyone/@here; nothing at all unless the caller lists it. */
function safeMentions(payload) {
  const am = payload?.allowed_mentions;
  const parse = Array.isArray(am?.parse) ? am.parse.filter((p) => p === 'users' || p === 'roles') : [];
  const ids = (list) => (Array.isArray(list) ? list.filter((id) => typeof id === 'string' && SNOWFLAKE_RE.test(id)).slice(0, 100) : undefined);
  const out = { parse };
  const users = parse.includes('users') ? undefined : ids(am?.users);
  const roles = parse.includes('roles') ? undefined : ids(am?.roles);
  if (users) out.users = users;
  if (roles) out.roles = roles;
  if (am?.replied_user === true) out.replied_user = true;
  return { ...payload, allowed_mentions: out };
}

function messageRef(data, channelId) {
  if (typeof data?.id !== 'string' || !SNOWFLAKE_RE.test(data.id)) {
    throw new DiscordApiError('Discord returned an unexpected message', { code: 'unavailable' });
  }
  return { id: data.id, channelId: typeof data.channel_id === 'string' ? data.channel_id : channelId };
}

/** A member or user as the dashboard shows it (from a guild member object or a bare user object). */
function memberProfile(m) {
  const u = m?.user ?? m;
  if (typeof u?.id !== 'string' || !SNOWFLAKE_RE.test(u.id)) return null;
  return {
    id: u.id,
    username: typeof u.username === 'string' ? u.username : 'unknown',
    globalName: typeof u.global_name === 'string' ? u.global_name : null,
    nick: typeof m?.nick === 'string' ? m.nick : null,
    avatarUrl: userAvatarUrl({ id: u.id, avatar: typeof u.avatar === 'string' ? u.avatar : null }),
    bot: u.bot === true,
  };
}

const assertSnowflake = (id, what) => {
  if (typeof id !== 'string' || !SNOWFLAKE_RE.test(id)) throw new DiscordApiError(`Invalid ${what} id`, { code: 'not_found' });
};

/**
 * Creates the dashboard's Discord client.
 * @param {{ config: { clientId: string, clientSecret: string, botToken: string, redirectUri: string },
 *           fetchImpl?: typeof fetch, now?: () => number, logger?: object,
 *           sleep?: (ms: number) => Promise<void> }} opts sleep: the wait before a rate-limited page is retried
 */
export function createDiscordApi({ config, fetchImpl = fetch, now = Date.now, logger = null, sleep = defaultSleep }) {
  const bot = `Bot ${config.botToken}`;

  /** userId -> { fp, guilds, fetchedAt }; insertion order = age. */
  const userGuilds = new Map();
  /** `${userId}:${fp}` -> Promise */
  const userInflight = new Map();

  let botGuilds = null;
  /** The bot's servers as Discord listed them, `[{ id, name, icon }]`; refreshed together with botGuilds. */
  let botGuildEntries = Object.freeze([]);
  let botGuildsAt = 0;
  let botInflight = null;
  /** guildId -> expiry of a recent "bot is not there" answer. */
  const botMisses = new Map();

  /** guildId -> { value, at }; insertion order = age. */
  const resources = new Map();
  /** guildId -> Promise of a load in flight. */
  const resourcesInflight = new Map();
  /** userId -> { value: profile|null, at }; insertion order = age. */
  const users = new Map();

  async function lookupUser(userId) {
    const hit = users.get(userId);
    if (hit && now() - hit.at < USERS_TTL_MS) return hit.value;
    let value = null;
    try {
      value = memberProfile(await getJson(fetchImpl, Routes.user(userId), bot));
    } catch (err) {
      if (!(err instanceof DiscordApiError) || err.code !== 'not_found') throw err;
    }
    users.delete(userId);
    users.set(userId, { value, at: now() });
    while (users.size > USERS_CACHE_MAX) users.delete(users.keys().next().value);
    return value;
  }

  async function loadGuildResources(guildId) {
    const [guild, channels, botMember] = await Promise.all([
      getJson(fetchImpl, `${Routes.guild(guildId)}?with_counts=true`, bot),
      getJson(fetchImpl, Routes.guildChannels(guildId), bot),
      getJson(fetchImpl, Routes.guildMember(guildId, config.clientId), bot),
    ]);
    // GET /guilds/{id} carries the roles and emojis; the list routes are only asked when it does not.
    const [roles, emojis] = await Promise.all([
      Array.isArray(guild?.roles) ? guild.roles : getJson(fetchImpl, Routes.guildRoles(guildId), bot),
      Array.isArray(guild?.emojis) ? guild.emojis : getJson(fetchImpl, Routes.guildEmojis(guildId), bot),
    ]);
    const value = mapGuildResources(guildId, config.clientId, guild, channels, roles, emojis, botMember);
    resources.delete(guildId);
    resources.set(guildId, { value, at: now() });
    while (resources.size > RESOURCES_CACHE_MAX) resources.delete(resources.keys().next().value);
    return value;
  }

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
      throw err;
    }
  }

  async function refreshBotGuilds() {
    const ids = new Set();
    const entries = [];
    let after = '0';
    let retriesLeft = BOT_GUILDS_MAX_RETRIES;
    // One page; a short 429 is waited out and the same page retried (a few times per refresh), anything else fails.
    const fetchPage = async () => {
      for (;;) {
        try {
          return await getJson(fetchImpl, `${Routes.userGuilds()}?limit=200&after=${after}`, bot);
        } catch (err) {
          const wait = err instanceof DiscordApiError && err.code === 'rate_limited' ? err.retryAfter : undefined;
          if (!Number.isFinite(wait) || wait > BOT_GUILDS_MAX_WAIT_SECONDS || retriesLeft <= 0) throw err;
          retriesLeft--;
          await sleep(wait * 1000);
        }
      }
    };
    for (let page = 0; page < BOT_GUILDS_MAX_PAGES; page++) {
      const data = await fetchPage();
      if (!Array.isArray(data)) throw new DiscordApiError('Discord returned an unexpected guild list', { code: 'unavailable' });
      for (const g of data) {
        if (typeof g?.id !== 'string' || !SNOWFLAKE_RE.test(g.id) || ids.has(g.id)) continue;
        ids.add(g.id);
        entries.push(Object.freeze({
          id: g.id,
          name: typeof g.name === 'string' && g.name ? g.name : 'Unknown server',
          icon: typeof g.icon === 'string' ? g.icon : null,
        }));
      }
      if (data.length < 200) break;
      const last = data[data.length - 1]?.id;
      if (typeof last !== 'string' || !SNOWFLAKE_RE.test(last)) break;
      after = last;
    }
    botGuilds = ids;
    botGuildEntries = Object.freeze(entries);
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
     * shared by concurrent callers. With `allowStale` (display only, never for an access decision) a Discord
     * 429 or outage serves a list up to USER_GUILDS_MAX_STALE_MS old; otherwise the error is thrown.
     * @param {string} userId
     * @param {string} accessToken
     * @param {{ allowStale?: boolean }} [opts] default false
     * @returns {Promise<ReadonlyArray<object>>}
     * @throws {DiscordApiError} unauthenticated, rate_limited (with retryAfter) or unavailable
     */
    async getUserGuilds(userId, accessToken, { allowStale = false } = {}) {
      const fp = tokenFingerprint(accessToken);
      const hit = userGuilds.get(userId);
      if (hit && hit.fp === fp && now() - hit.fetchedAt < USER_GUILDS_TTL_MS) return hit.guilds;
      const key = `${userId}:${fp}`;
      let pending = userInflight.get(key);
      if (!pending) {
        pending = fetchUserGuilds(userId, accessToken, fp).finally(() => userInflight.delete(key));
        userInflight.set(key, pending);
      }
      try {
        return await pending;
      } catch (err) {
        if (allowStale && err instanceof DiscordApiError && (err.code === 'rate_limited' || err.code === 'unavailable')) {
          const stale = userGuilds.get(userId);
          if (stale && stale.fp === fp && now() - stale.fetchedAt <= USER_GUILDS_MAX_STALE_MS) return stale.guilds;
        }
        throw err;
      }
    },

    /** Drops the cached guild list of a user (after logout). */
    forgetUser(userId) {
      userGuilds.delete(userId);
    },

    getBotGuildIds,

    /**
     * The servers the bot is in, from the same cached list as getBotGuildIds (for the bot owner's part of the server
     * list). A server that only isBotInGuild's verify step found is missing until the next refresh.
     * @returns {Promise<readonly { id: string, name: string, icon: string|null }[]>}
     * @throws {DiscordApiError} only when no list was ever loaded
     */
    async getBotGuilds() {
      await getBotGuildIds();
      return botGuildEntries;
    },

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
     * A guild's channels (Discord sidebar order), roles (highest first), emojis (by name) and the bot's highest role
     * position, cached for RESOURCES_TTL_MS per guild (at most RESOURCES_CACHE_MAX guilds); concurrent callers share one
     * load. `fresh` skips the cache (and refreshes it). Role entries also carry `mentionable` and `dangerous` (a permission
     * the bot never hands out), the bot `manageRoles`, for the role checks of the server and the stores; the guild
     * carries `ownerId`, `premiumTier`, `premiumSubscriptionCount` and `verificationLevel`.
     * @param {string} guildId
     * @param {{ fresh?: boolean }} [opts]
     * @returns {Promise<Readonly<{ guild: object, channels: object[], roles: object[], emojis: object[],
     *   bot: { userId: string, topRolePosition: number, manageRoles: boolean } }>>}
     * @throws {DiscordApiError}
     */
    async getGuildResources(guildId, { fresh = false } = {}) {
      assertSnowflake(guildId, 'server');
      const hit = resources.get(guildId);
      if (!fresh && hit && now() - hit.at < RESOURCES_TTL_MS) return hit.value;
      // A plain caller may share a fresh load in flight; a fresh caller never reuses a plain one.
      const key = fresh ? `${guildId}:fresh` : guildId;
      let pending = resourcesInflight.get(key) ?? (fresh ? undefined : resourcesInflight.get(`${guildId}:fresh`));
      if (!pending) {
        pending = loadGuildResources(guildId).finally(() => resourcesInflight.delete(key));
        resourcesInflight.set(key, pending);
      }
      return pending;
    },

    /**
     * A member's role ids (bot token), or null when the user is not in the guild. Not cached.
     * @param {string} guildId
     * @param {string} userId
     * @returns {Promise<{ roles: string[] } | null>}
     * @throws {DiscordApiError}
     */
    async getMember(guildId, userId) {
      assertSnowflake(guildId, 'server');
      assertSnowflake(userId, 'user');
      try {
        const m = await getJson(fetchImpl, Routes.guildMember(guildId, userId), bot);
        return { roles: Array.isArray(m?.roles) ? m.roles.filter((id) => typeof id === 'string' && SNOWFLAKE_RE.test(id)) : [] };
      } catch (err) {
        if (err instanceof DiscordApiError && err.code === 'not_found') return null;
        throw err;
      }
    },

    /**
     * Members whose username or nickname starts with `query` (Discord's member search), or the one member when `query`
     * is a user id.
     * @param {string} guildId
     * @param {string} query 1..100 characters
     * @param {number} [limit] at most MEMBER_SEARCH_MAX
     * @returns {Promise<{ id: string, username: string, globalName: string|null, nick: string|null, avatarUrl: string,
     *   bot: boolean }[]>}
     * @throws {DiscordApiError}
     */
    async searchMembers(guildId, query, limit = MEMBER_SEARCH_MAX) {
      assertSnowflake(guildId, 'server');
      const q = String(query ?? '').trim().slice(0, 100);
      if (!q) return [];
      if (SNOWFLAKE_RE.test(q)) {
        try {
          const one = memberProfile(await getJson(fetchImpl, Routes.guildMember(guildId, q), bot));
          return one ? [one] : [];
        } catch (err) {
          if (err instanceof DiscordApiError && err.code === 'not_found') return [];
          throw err;
        }
      }
      const n = Math.min(Math.max(1, Math.trunc(limit) || 1), MEMBER_SEARCH_MAX);
      const data = await getJson(fetchImpl, `${Routes.guildMembersSearch(guildId)}?${new URLSearchParams({ query: q, limit: String(n) })}`, bot);
      return (Array.isArray(data) ? data : []).map(memberProfile).filter(Boolean);
    },

    /**
     * Names and avatars of users by id (for lists such as the XP leaderboard), cached for an hour. Unknown ids map to
     * null. At most 100 ids per call.
     * @param {string[]} ids
     * @returns {Promise<Map<string, object|null>>}
     * @throws {DiscordApiError} when Discord cannot answer
     */
    async getUsers(ids) {
      const wanted = [...new Set((Array.isArray(ids) ? ids : []).filter((id) => typeof id === 'string' && SNOWFLAKE_RE.test(id)))].slice(0, 100);
      const out = new Map();
      let next = 0;
      const worker = async () => {
        while (next < wanted.length) {
          const id = wanted[next++];
          out.set(id, await lookupUser(id));
        }
      };
      await Promise.all(Array.from({ length: Math.min(USER_LOOKUP_CONCURRENCY, wanted.length) }, worker));
      return out;
    },

    /**
     * Posts a message as the bot. `payload` is Discord REST message JSON; its allowed_mentions never allow
     * @everyone/@here and default to no pings at all.
     * @param {string} channelId
     * @param {object} payload
     * @returns {Promise<{ id: string, channelId: string }>}
     * @throws {DiscordApiError}
     */
    async sendMessage(channelId, payload) {
      assertSnowflake(channelId, 'channel');
      const data = await requestJson(fetchImpl, 'POST', Routes.channelMessages(channelId), bot, safeMentions(payload));
      return messageRef(data, channelId);
    },

    /**
     * Edits a message the bot posted (same payload rules as sendMessage).
     * @param {string} channelId
     * @param {string} messageId
     * @param {object} payload
     * @returns {Promise<{ id: string, channelId: string }>}
     * @throws {DiscordApiError}
     */
    async editMessage(channelId, messageId, payload) {
      assertSnowflake(channelId, 'channel');
      assertSnowflake(messageId, 'message');
      const data = await requestJson(fetchImpl, 'PATCH', Routes.channelMessage(channelId, messageId), bot, safeMentions(payload));
      return messageRef(data, channelId);
    },

    /**
     * Deletes a message. A message (or channel) that is already gone counts as deleted.
     * @param {string} channelId
     * @param {string} messageId
     * @returns {Promise<void>}
     * @throws {DiscordApiError}
     */
    async deleteMessage(channelId, messageId) {
      assertSnowflake(channelId, 'channel');
      assertSnowflake(messageId, 'message');
      try {
        await requestJson(fetchImpl, 'DELETE', Routes.channelMessage(channelId, messageId), bot);
      } catch (err) {
        if (err instanceof DiscordApiError && err.code === 'not_found') return;
        throw err;
      }
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
