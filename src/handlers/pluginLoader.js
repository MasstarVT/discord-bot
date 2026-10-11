// Plugin loader (01-foundation §8.1-§8.2). A plugin is `src/plugins/<package-id>.js` exporting
// `plugin = { id, module, hooks, components, jobs, provides, init }`. collectPlugins() imports and
// validates them (pure: no client, used by the tests); installPlugins() registers them.
import { readdirSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { CUSTOM_ID_PREFIXES } from '../config/customIds.js';
import { HOOK_ORDER } from '../config/hookPoints.js';
import { JOB_TYPES } from '../config/jobTypes.js';
import { MODULES, isModuleEnabled } from '../config/modules.js';
import { REWARD_KINDS, registerProvider } from '../services/rewards.js';
import { registerJobHandler } from '../services/scheduler.js';
import { getSettingsSafe, peekSettings } from '../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../utils/async.js';
import logger from '../utils/logger.js';
import { registerHook } from './hookRegistry.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
export const PLUGINS_DIR = resolve(__dirname, '../plugins');

const COMPONENT_TYPES = ['button', 'select', 'modal'];
const COMPONENT_MAPS = { button: 'buttonHandlers', select: 'selectHandlers', modal: 'modalHandlers' };
const isFn = (v) => typeof v === 'function';
const isModuleRef = (m) => m === null || (typeof m === 'string' && Object.hasOwn(MODULES, m));

/**
 * @typedef {object} NormalizedPlugin
 * @property {string} id
 * @property {string|null} module
 * @property {Array<object>} hooks
 * @property {Array<object>} components
 * @property {Array<{ type: string, handler: Function }>} jobs
 * @property {Record<string, { grant: Function, take: Function }>} provides
 * @property {Function|undefined} init
 */

/**
 * Imports and validates every `*.js` file in `dir` (files starting with `_` are skipped). Plain
 * `pathToFileURL` imports with no cache-busting query, so a command file that imports a plugin shares
 * its module instance. Each violation becomes an `errors` entry and drops only the offending entry
 * (the whole plugin when its id or module is wrong). A missing directory gives no plugins and no errors.
 * Never throws.
 * @param {{ dir?: string }} [opts]
 * @returns {Promise<{ plugins: NormalizedPlugin[], errors: string[] }>}
 */
export async function collectPlugins({ dir = PLUGINS_DIR } = {}) {
  const plugins = [];
  const errors = [];
  let files;
  try {
    files = readdirSync(dir).filter((f) => f.endsWith('.js') && !f.startsWith('_')).sort();
  } catch {
    return { plugins, errors };
  }

  const seenHooks = new Set();
  const seenComponents = new Set();
  const seenJobs = new Set();
  const seenProviders = new Set();

  for (const file of files) {
    const id = basename(file, '.js');
    const where = `plugin ${file}`;
    let mod;
    try {
      mod = await import(pathToFileURL(join(dir, file)).href);
    } catch (err) {
      errors.push(`${where}: import failed: ${err?.message ?? err}`);
      continue;
    }

    const p = mod.plugin;
    if (!p || typeof p !== 'object') {
      errors.push(`${where}: missing "export const plugin"`);
      continue;
    }
    if (p.id !== id) {
      errors.push(`${where}: plugin.id "${p.id}" must equal the file name "${id}"`);
      continue;
    }
    if (!isModuleRef(p.module)) {
      errors.push(`${where}: plugin.module must be null or a MODULES key (got ${JSON.stringify(p.module)})`);
      continue;
    }

    const normalized = { id, module: p.module, hooks: [], components: [], jobs: [], provides: {}, init: undefined };

    // ── Hooks ────────────────────────────────────────────────────────────────
    for (const hook of p.hooks ?? []) {
      const tag = `${where}: hook ${hook?.event}:${hook?.id}`;
      if (!hook || !Object.hasOwn(HOOK_ORDER, hook.event)) { errors.push(`${tag}: unknown event`); continue; }
      if (!HOOK_ORDER[hook.event].includes(hook.id)) { errors.push(`${tag}: id is not listed in HOOK_ORDER.${hook.event}`); continue; }
      if (hook.id !== id && !String(hook.id).startsWith(`${id}:`)) { errors.push(`${tag}: id must be "${id}" or start with "${id}:"`); continue; }
      if (!isFn(hook.run)) { errors.push(`${tag}: run must be a function`); continue; }
      if (hook.filter !== undefined && !isFn(hook.filter)) { errors.push(`${tag}: filter must be a function`); continue; }
      if (hook.module !== undefined && !isModuleRef(hook.module)) { errors.push(`${tag}: module must be null or a MODULES key`); continue; }
      const key = `${hook.event}:${hook.id}`;
      if (seenHooks.has(key)) { errors.push(`${tag}: duplicate hook`); continue; }
      seenHooks.add(key);
      normalized.hooks.push(hook);
    }

    // ── Components ───────────────────────────────────────────────────────────
    for (const c of p.components ?? []) {
      const tag = `${where}: component ${c?.type} "${c?.prefix}"`;
      if (!c || !COMPONENT_TYPES.includes(c.type)) { errors.push(`${tag}: type must be button, select or modal`); continue; }
      const info = Object.hasOwn(CUSTOM_ID_PREFIXES, c.prefix) ? CUSTOM_ID_PREFIXES[c.prefix] : null;
      if (!info) { errors.push(`${tag}: prefix is not in CUSTOM_ID_PREFIXES`); continue; }
      if (info.owner !== id) { errors.push(`${tag}: prefix is owned by "${info.owner}", not "${id}"`); continue; }
      if (info.kind !== 'persistent') { errors.push(`${tag}: prefix is "${info.kind}", only persistent prefixes get a registered handler`); continue; }
      if (!info.types.includes(c.type)) { errors.push(`${tag}: the registry lists this prefix for ${info.types.join('/')}`); continue; }
      if (!isFn(c.handler)) { errors.push(`${tag}: handler must be a function`); continue; }
      if (c.module !== undefined && !isModuleRef(c.module)) { errors.push(`${tag}: module must be null or a MODULES key`); continue; }
      const key = `${c.type}:${c.prefix}`;
      if (seenComponents.has(key)) { errors.push(`${tag}: duplicate component`); continue; }
      seenComponents.add(key);
      normalized.components.push(c);
    }

    // ── Jobs ─────────────────────────────────────────────────────────────────
    for (const j of p.jobs ?? []) {
      const tag = `${where}: job "${j?.type}"`;
      const info = j && Object.hasOwn(JOB_TYPES, j.type) ? JOB_TYPES[j.type] : null;
      if (!info) { errors.push(`${tag}: unknown job type`); continue; }
      if (info.owner !== id) { errors.push(`${tag}: owned by "${info.owner}", not "${id}"`); continue; }
      if (!isFn(j.handler)) { errors.push(`${tag}: handler must be a function`); continue; }
      if (seenJobs.has(j.type)) { errors.push(`${tag}: duplicate job handler`); continue; }
      seenJobs.add(j.type);
      normalized.jobs.push(j);
    }

    // ── Reward providers ─────────────────────────────────────────────────────
    for (const [kind, impl] of Object.entries(p.provides ?? {})) {
      const tag = `${where}: provides.${kind}`;
      if (!REWARD_KINDS.includes(kind)) { errors.push(`${tag}: unknown reward kind`); continue; }
      if (!isFn(impl?.grant) || !isFn(impl?.take)) { errors.push(`${tag}: needs grant and take functions`); continue; }
      if (seenProviders.has(kind)) { errors.push(`${tag}: duplicate provider`); continue; }
      seenProviders.add(kind);
      normalized.provides[kind] = impl;
    }

    if (p.init !== undefined) {
      if (isFn(p.init)) normalized.init = p.init;
      else errors.push(`${where}: init must be a function`);
    }

    plugins.push(normalized);
  }

  return { plugins, errors };
}

// ── Component module gating ───────────────────────────────────────────────────

async function ephemeralNotice(interaction, content) {
  try {
    if (interaction.replied || interaction.deferred) await interaction.followUp({ content, ephemeral: true });
    else await interaction.reply({ content, ephemeral: true });
  } catch {
    // The interaction may have expired; nothing else to do.
  }
}

/**
 * Wraps a component handler with the module toggle. It reads the memo first and bounds a cold load
 * at 1000 ms, so the handler keeps enough of the 3-second window to showModal or deferReply.
 * @param {Function} handler
 * @param {string} module a MODULES key
 */
function gateComponent(handler, module) {
  return async (interaction, client) => {
    if (interaction.inGuild()) {
      let settings = peekSettings(interaction.guildId);
      if (!settings) {
        settings = await withTimeout(
          getSettingsSafe(interaction.guildId),
          Math.min(1000, ackBudgetMs(interaction)),
          'component settings',
        ).catch(() => null);
      }
      if (!isModuleEnabled(settings, module)) {
        return ephemeralNotice(interaction, settings
          ? `The **${MODULES[module]?.label ?? module}** module is turned off on this server.`
          : 'Server settings are temporarily unavailable. Please try again in a moment.');
      }
    }
    return handler(interaction, client);
  };
}

/**
 * Registers the collected plugins: hooks, then components (module-gated), job handlers, reward
 * providers, then each plugin's sync `init(client)`. A failing registration is recorded and the
 * rest still install. Never throws.
 * @param {import('discord.js').Client} client
 * @param {{ plugins: NormalizedPlugin[], errors?: string[] }} collected
 * @returns {{ hooks: number, components: number, jobs: number, providers: number, errors: string[] }}
 */
export function installPlugins(client, collected) {
  const result = { hooks: 0, components: 0, jobs: 0, providers: 0, errors: [...(collected?.errors ?? [])] };
  const plugins = collected?.plugins ?? [];
  const fail = (message, err) => result.errors.push(err ? `${message}: ${err?.message ?? err}` : message);

  for (const plugin of plugins) {
    for (const hook of plugin.hooks) {
      try {
        registerHook(hook, { pluginId: plugin.id, pluginModule: plugin.module });
        result.hooks++;
      } catch (err) {
        fail(`${plugin.id}: hook ${hook.event}:${hook.id} not registered`, err);
      }
    }
  }

  for (const plugin of plugins) {
    for (const c of plugin.components) {
      try {
        const module = c.module !== undefined ? c.module : plugin.module;
        const handler = module ? gateComponent(c.handler, module) : c.handler;
        client[COMPONENT_MAPS[c.type]].set(c.prefix, handler);
        result.components++;
      } catch (err) {
        fail(`${plugin.id}: ${c.type} handler "${c.prefix}" not registered`, err);
      }
    }
  }

  for (const plugin of plugins) {
    for (const j of plugin.jobs) {
      try {
        registerJobHandler(j.type, j.handler);
        result.jobs++;
      } catch (err) {
        fail(`${plugin.id}: job handler ${j.type} not registered`, err);
      }
    }
  }

  for (const plugin of plugins) {
    for (const [kind, impl] of Object.entries(plugin.provides)) {
      try {
        registerProvider(kind, impl, plugin.id);
        result.providers++;
      } catch (err) {
        fail(`${plugin.id}: reward provider "${kind}" not registered`, err);
      }
    }
  }

  for (const plugin of plugins) {
    if (!plugin.init) continue;
    try {
      plugin.init(client);
    } catch (err) {
      fail(`${plugin.id}: init failed`, err);
    }
  }

  logger.info(
    `Plugins: ${plugins.length} loaded (${result.hooks} hook(s), ${result.components} component(s), ` +
    `${result.jobs} job handler(s), ${result.providers} provider(s)), ${result.errors.length} error(s).`,
  );
  for (const e of result.errors) logger.error(`Plugin error: ${e}`);
  return result;
}
