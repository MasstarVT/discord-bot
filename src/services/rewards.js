// Rewards bus (01-foundation §7.1): XP and coins are granted through providers that the levels
// and economy packages register, so giveaways, achievements and automations never import them.
import { isModuleEnabled } from '../config/modules.js';
import logger from '../utils/logger.js';

export const REWARD_KINDS = Object.freeze(['xp', 'coins']);
// The module that backs each kind: a provider only counts as available while that module is on.
export const REWARD_MODULES = Object.freeze({ xp: 'leveling', coins: 'economy' });

const MAX_AMOUNT = 1_000_000_000;

/** @type {Map<string, { impl: { grant: Function, take: Function }, owner: string|null }>} */
const providers = new Map();

/**
 * Registers the provider for a reward kind. `impl = { grant(args), take(args) }` takes the same
 * argument object as grant()/take() below and returns `{ ok, total?, reason? }`; providers enforce
 * their own module toggle (`'module-disabled'`) and caps.
 * @param {'xp'|'coins'} kind
 * @param {{ grant: Function, take: Function }} impl
 * @param {string} [ownerPluginId]
 * @throws {Error} on an unknown kind, an invalid impl or a second registration
 */
export function registerProvider(kind, impl, ownerPluginId) {
  if (!REWARD_KINDS.includes(kind)) throw new Error(`Unknown reward kind "${kind}"`);
  if (typeof impl?.grant !== 'function' || typeof impl?.take !== 'function') {
    throw new TypeError(`Reward provider for "${kind}" must implement grant() and take()`);
  }
  if (providers.has(kind)) {
    throw new Error(`Reward provider for "${kind}" is already registered by ${providers.get(kind).owner ?? 'another plugin'}`);
  }
  providers.set(kind, { impl, owner: ownerPluginId ?? null });
}

/** @returns {boolean} true when a provider is registered for `kind` */
export function hasProvider(kind) {
  return providers.has(kind);
}

/** @returns {string|null} the module key that backs `kind` */
export function providerModule(kind) {
  return REWARD_MODULES[kind] ?? null;
}

/**
 * Pure and synchronous: the provider is registered AND its backing module is enabled for the guild.
 * @param {'xp'|'coins'} kind
 * @param {object|null} settings GuildSettings row
 * @returns {boolean}
 */
export function isRewardAvailable(kind, settings) {
  const module = providerModule(kind);
  return hasProvider(kind) && module !== null && isModuleEnabled(settings, module);
}

async function dispatch(method, kind, args) {
  try {
    const { amount } = args ?? {};
    if (!Number.isInteger(amount) || amount < 1 || amount > MAX_AMOUNT) return { ok: false, reason: 'invalid-amount' };
    const entry = providers.get(kind);
    if (!entry) return { ok: false, reason: 'unavailable' };
    const { guild, userId, reason = '', member = null, channel = null } = args;
    const result = await entry.impl[method]({ guild, userId, amount, reason, member, channel });
    if (!result || typeof result !== 'object' || typeof result.ok !== 'boolean') return { ok: false, reason: 'error' };
    return result;
  } catch (err) {
    logger.error(`Reward provider "${kind}" ${method} failed`, err);
    return { ok: false, reason: 'error' };
  }
}

/**
 * Grants `amount` (an integer, 1 to 1,000,000,000) of `kind` to a user. Never throws.
 * @param {'xp'|'coins'} kind
 * @param {{ guild: object, userId: string, amount: number, reason?: string, member?: object|null, channel?: object|null }} args
 * @returns {Promise<{ ok: boolean, total?: number, reason?: string }>}
 *   reason: 'invalid-amount' | 'unavailable' | 'error' | whatever the provider returns (e.g. 'module-disabled')
 */
export function grant(kind, args) {
  return dispatch('grant', kind, args);
}

/**
 * Takes `amount` of `kind` from a user (same contract as grant). Never throws.
 * @param {'xp'|'coins'} kind
 * @param {{ guild: object, userId: string, amount: number, reason?: string, member?: object|null, channel?: object|null }} args
 * @returns {Promise<{ ok: boolean, total?: number, reason?: string }>}
 */
export function take(kind, args) {
  return dispatch('take', kind, args);
}
