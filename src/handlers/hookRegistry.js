// Hook registry and runner (01-foundation §8.3). Feature packages register hooks through their
// plugin (pluginLoader.installPlugins); the thin event files call runHooks(event, fields).
// The central order is HOOK_ORDER (src/config/hookPoints.js); nothing here knows any feature.
import { hookIndex } from '../config/hookPoints.js';
import { isModuleEnabled } from '../config/modules.js';
import { ensureGuildSettings, getSettingsSafe } from '../services/settingsService.js';
import { TimeoutError, createSemaphore, createThrottle, withTimeout } from '../utils/async.js';
import logger from '../utils/logger.js';

export const DEFAULT_HOOK_TIMEOUT_MS = 10_000;

/** @type {Map<string, Array<object>>} event -> hooks, kept sorted by their HOOK_ORDER index */
const registry = new Map();

// A hook whose id is not in HOOK_ORDER (only possible for hooks registered directly, as in tests)
// sorts after every listed one, in registration order.
const orderOf = (event, id) => {
  const i = hookIndex(event, id);
  return i === -1 ? Number.MAX_SAFE_INTEGER : i;
};

/**
 * Registers one hook. The effective module is the hook's own `module` when it declares one
 * (null = never gated), otherwise the plugin's. A second hook with the same (event, id) is refused.
 * @param {{ event: string, id: string, run: Function, filter?: Function, module?: string|null,
 *   always?: boolean, timeoutMs?: number }} hook
 * @param {{ pluginId: string, pluginModule?: string|null }} owner
 */
export function registerHook(hook, { pluginId, pluginModule = null } = {}) {
  const list = registry.get(hook.event) ?? [];
  if (list.some((h) => h.id === hook.id)) {
    logger.error(`Duplicate hook ${hook.event}:${hook.id} ignored`);
    return;
  }
  list.push({ ...hook, pluginId, module: hook.module !== undefined ? hook.module : pluginModule });
  list.sort((a, b) => orderOf(hook.event, a.id) - orderOf(hook.event, b.id));
  registry.set(hook.event, list);
}

/** Removes every registered hook (tests). */
export function clearHooks() {
  registry.clear();
}

/**
 * The hooks registered for an event, in execution order.
 * @param {string} event
 * @returns {Array<{ id: string, pluginId: string, module: string|null|undefined, always: boolean }>}
 */
export function listHooks(event) {
  return (registry.get(event) ?? []).map((h) => ({
    id: h.id,
    pluginId: h.pluginId,
    module: h.module,
    always: h.always === true,
  }));
}

/**
 * Builds the context every hook of one dispatch receives. The core keys cannot be overridden by `fields`.
 * @param {string} event
 * @param {object} [fields] event-specific fields (01-foundation §9), including `client` and `guild`
 * @returns {object} HookContext
 */
export function createHookContext(event, fields = {}) {
  const guild = fields?.guild ?? null;
  const guildId = guild?.id ?? fields?.guildId ?? null;
  let settings = null;
  const ctx = {
    ...fields,
    event,
    client: fields?.client ?? null,
    guild,
    guildId,
    state: Object.create(null),
    stopped: false,
    stopReason: null,
    stop(reason = 'stopped') {
      ctx.stopped = true;
      ctx.stopReason = reason;
    },
    /** Memoised getSettingsSafe(guildId); null without a guild. Never throws. */
    getSettings() {
      if (!settings) {
        settings = (guildId ? getSettingsSafe(guildId) : Promise.resolve(null)).catch(() => null);
      }
      return settings;
    },
  };
  return ctx;
}

/**
 * Runs every hook of `event` in HOOK_ORDER. Never throws.
 *
 * Per hook: stop when `ctx.stopped`; a sync `filter` returning false (or throwing) skips it; a hook
 * with a module is skipped unless that module is enabled (no settings counts as disabled) unless it is
 * `always`; then `run(ctx)` is invoked synchronously, bounded by its soft timeout. A failing or
 * timed-out hook is logged and the next one still runs.
 *
 * Ordering guarantee (pkg-invites relies on it): nothing before the first `run` awaits, so the first
 * hook of a dispatch that is `always` or ungated (module null) and passes its filter is invoked
 * during the runHooks() call itself, in gateway order.
 * @param {string} event
 * @param {object} [fields]
 * @returns {Promise<object>} the HookContext
 */
export async function runHooks(event, fields = {}) {
  const ctx = createHookContext(event, fields);
  for (const hook of [...(registry.get(event) ?? [])]) {
    try {
      if (ctx.stopped) break;

      if (hook.filter) {
        let pass = false;
        try {
          pass = Boolean(hook.filter(ctx));
        } catch (err) {
          logger.error(`Hook ${event}:${hook.id} filter failed`, err instanceof Error ? err : new Error(String(err)));
        }
        if (!pass) continue;
      }

      if (!hook.always && hook.module) {
        const settings = await ctx.getSettings();
        if (!isModuleEnabled(settings, hook.module)) continue;
      }

      let promise;
      try {
        promise = Promise.resolve(hook.run(ctx));
      } catch (err) {
        promise = Promise.reject(err);
      }
      await withTimeout(promise, hook.timeoutMs ?? DEFAULT_HOOK_TIMEOUT_MS, `hook ${event}:${hook.id}`);
    } catch (err) {
      if (err instanceof TimeoutError) {
        logger.warn(`Hook ${event}:${hook.id} timed out and was abandoned`);
      } else {
        logger.error(`Hook ${event}:${hook.id} failed`, err instanceof Error ? err : new Error(String(err)));
      }
    }
  }
  return ctx;
}

/**
 * Startup work for one guild: ensures its GuildSettings row, then runs the `guildReady` hooks.
 * `reason` is 'startup' (ready.js), 'join' (guildCreate.js) or 'available' (guildAvailable.js);
 * hooks treat 'available' exactly like 'startup'. An unavailable guild has empty caches, so it is
 * skipped here and guildAvailable.js calls this again when it arrives. Never throws.
 * @param {import('discord.js').Client} client
 * @param {import('discord.js').Guild} guild
 * @param {'startup'|'join'|'available'} reason
 */
export async function runGuildReady(client, guild, reason) {
  try {
    if (!guild || !guild.available) return;
    if (reason !== 'available') availableGate.record(guild.id); // runGuildAvailable checked and recorded it already
    try {
      await ensureGuildSettings(guild.id);
    } catch (err) {
      logger.error(`Could not ensure the settings row of guild ${guild.id}`, err instanceof Error ? err : new Error(String(err)));
    }
    await runHooks('guildReady', { client, guild, reason });
  } catch (err) {
    logger.error(`guildReady failed for guild ${guild?.id}`, err instanceof Error ? err : new Error(String(err)));
  }
}

/** How many guildAvailable re-runs of the startup work may run at once (an outage ending brings back many guilds). */
export const AVAILABLE_CONCURRENCY = 2;
/** A guild whose startup work ran this recently skips a guildAvailable re-run (a flapping guild repeats it otherwise). */
export const AVAILABLE_COOLDOWN_MS = 10 * 60_000;

/**
 * The cap and per-guild cooldown in front of guildAvailable re-runs (exported for tests).
 * `record(id)` starts a guild's cooldown; it returns false (and keeps the older start) while one is running.
 */
export function createReadyGate({ concurrency = AVAILABLE_CONCURRENCY, cooldownMs = AVAILABLE_COOLDOWN_MS } = {}) {
  const slots = createSemaphore(concurrency);
  const record = createThrottle(cooldownMs);
  return {
    record,
    /** @returns {Promise<boolean>} false when skipped by the cooldown */
    async run(guildId, fn) {
      if (!record(guildId)) return false;
      await slots.run(fn);
      return true;
    },
  };
}

const availableGate = createReadyGate();

/**
 * guildAvailable: re-runs the startup work of a guild that came back, at most AVAILABLE_CONCURRENCY at once and at most once
 * per guild every AVAILABLE_COOLDOWN_MS (counting its startup or join run). A guild skipped at startup because it was
 * unavailable has no cooldown, so its first arrival always runs. Never throws.
 * @returns {Promise<boolean>} whether the startup work ran
 */
export async function runGuildAvailable(client, guild) {
  if (!guild?.available) return false;
  try {
    return await availableGate.run(guild.id, () => runGuildReady(client, guild, 'available'));
  } catch (err) {
    logger.error(`guildAvailable failed for guild ${guild.id}`, err instanceof Error ? err : new Error(String(err)));
    return false;
  }
}
