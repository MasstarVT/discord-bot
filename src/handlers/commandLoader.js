import { readdirSync, statSync, readFileSync, writeFileSync } from 'fs';
import { join, resolve, dirname } from 'path';
import { fileURLToPath, pathToFileURL } from 'url';
import { createHash } from 'crypto';
import { ApplicationIntegrationType, InteractionContextType, REST, Routes } from 'discord.js';
import { isModuleKey } from '../config/modules.js';
import logger from '../utils/logger.js';

const __dirname    = dirname(fileURLToPath(import.meta.url));
const COMMANDS_DIR = resolve(__dirname, '../commands');
// DEPLOY_HASH_FILE lets containers keep the hash on a persistent volume, so a
// restart or redeploy doesn't re-PUT an unchanged command set.
const HASH_FILE    = process.env.DEPLOY_HASH_FILE || resolve(__dirname, '../../.deploy-hash');

/**
 * Recursively collects all .js file paths under a directory.
 * @param {string} dir
 * @returns {string[]}
 */
function walkDir(dir) {
  const entries = readdirSync(dir);
  const files = [];
  for (const entry of entries) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      files.push(...walkDir(full));
    } else if (entry.endsWith('.js') && !entry.startsWith('_')) {
      files.push(full);
    }
  }
  return files;
}

/**
 * The payload Discord receives for a command module. Unless the module opts in with
 * `allowInDMs = true`, the command is registered for servers only (contexts [Guild],
 * integration types [GuildInstall]): the router assumes a cached guild for every other command
 * (R123, R124, R271). A command that sets its own contexts or integration types keeps them.
 * @param {{ data: { toJSON(): object }, allowInDMs?: boolean }} mod
 * @returns {object}
 */
export function buildCommandPayload(mod) {
  const json = mod.data.toJSON();
  if (mod.allowInDMs !== true) {
    json.contexts ??= [InteractionContextType.Guild];                    // 0
    json.integration_types ??= [ApplicationIntegrationType.GuildInstall]; // 0
  }
  return json;
}

/**
 * Hash of the command set AND where it is deployed, so changing CLIENT_ID or DEPLOY_GUILD_ID
 * forces a deploy even when the commands themselves are unchanged (R270). Sorted by name, so payload
 * order never causes a false positive. deployCommands() and autoDeployCommands() both use it.
 * @param {string} clientId
 * @param {string|undefined} guildId
 * @param {object[]} payloads
 * @returns {string} hex sha256
 */
export function computeDeployHash(clientId, guildId, payloads) {
  const sorted = [...payloads].sort((a, b) => a.name.localeCompare(b.name));
  return createHash('sha256')
    .update(`${clientId}|${guildId || 'global'}|`)
    .update(JSON.stringify(sorted))
    .digest('hex');
}

/**
 * Imports every command file and returns its deploy payload (files that fail to import are logged and skipped).
 * @param {string} skipLabel what to say when a file is skipped
 * @returns {Promise<object[]>}
 */
async function collectPayloads(skipLabel) {
  let files;
  try {
    files = walkDir(COMMANDS_DIR);
  } catch {
    files = [];
  }

  const payloads = [];
  for (const file of files) {
    try {
      const mod = await import(pathToFileURL(file).href);
      if (mod.data?.toJSON) payloads.push(buildCommandPayload(mod));
    } catch (err) {
      logger.error(`Skipping ${file} during ${skipLabel}`, err);
    }
  }
  return payloads;
}

/**
 * Loads all commands from /src/commands into client.commands.
 * @param {import('discord.js').Client} client
 */
export async function loadCommands(client) {
  let files;
  try {
    files = walkDir(COMMANDS_DIR);
  } catch {
    // Commands directory is empty — fine during Phase 1
    logger.warn('No commands directory found or directory is empty. Skipping command load.');
    return;
  }

  let loaded = 0;
  for (const file of files) {
    let mod;
    try {
      mod = await import(pathToFileURL(file).href);
    } catch (err) {
      logger.error(`Failed to import command file: ${file}`, err);
      continue;
    }

    if (!mod.data || typeof mod.execute !== 'function') {
      logger.warn(`Skipping ${file} — missing required exports: { data, execute }`);
      continue;
    }

    const name = mod.data.name;
    if (client.commands.has(name)) {
      logger.warn(`Duplicate command name "${name}" in ${file} — skipping.`);
      continue;
    }

    // A wrong `module` is reported but not fatal; the router then treats the command as gated by a
    // module that can never be enabled, which test/registry.test.js catches.
    if (mod.module !== undefined && mod.module !== null && !isModuleKey(mod.module)) {
      logger.warn(`Command "${name}" (${file}) declares module ${JSON.stringify(mod.module)}, which is not a MODULES key.`);
    }

    client.commands.set(name, mod);

    // Tag each command with its subfolder name so /help can group by category
    const parts = file.replace(/\\/g, '/').split('/');
    const cmdIdx = parts.lastIndexOf('commands');
    const category = cmdIdx !== -1 && cmdIdx + 2 < parts.length ? parts[cmdIdx + 1] : 'general';
    client.commandCategories.set(name, category);

    // Allow commands to self-register button/select/modal handlers (legacy; plugins are the
    // preferred route). The guarded handler maps refuse a duplicate prefix.
    if (typeof mod.registerHandlers === 'function') {
      mod.registerHandlers(client);
    }

    loaded++;
  }

  logger.success(`Loaded ${loaded} command(s).`);
}

/**
 * Deploys all slash commands to Discord via REST.
 * Uses guild-scoped deployment when DEPLOY_GUILD_ID is set (instant),
 * otherwise pushes globally (up to 1 hour propagation).
 */
export async function deployCommands() {
  const token    = process.env.DISCORD_TOKEN;
  const clientId = process.env.CLIENT_ID;
  const guildId  = process.env.DEPLOY_GUILD_ID;

  if (!token || !clientId) {
    logger.error('DISCORD_TOKEN and CLIENT_ID must be set in .env to deploy commands.');
    process.exit(1);
  }

  const payloads = await collectPayloads('deploy');
  if (payloads.length === 0) logger.warn('No command payloads found — deploying 0 commands.');

  logger.info(`Deploying ${payloads.length} command(s)...`);
  logger.debug('Payload:', JSON.stringify(payloads, null, 2));

  const rest  = new REST({ version: '10' }).setToken(token);
  const route = guildId
    ? Routes.applicationGuildCommands(clientId, guildId)
    : Routes.applicationCommands(clientId);

  try {
    const result = await rest.put(route, { body: payloads });
    logger.success(
      `Successfully registered ${result.length} command(s) ` +
      (guildId ? `to guild ${guildId}` : 'globally') + '.'
    );
  } catch (err) {
    logger.error('Command deployment failed', err);
    process.exit(1);
  }

  // Record what is now live, so the next start does not re-PUT the same set (R270).
  try {
    writeFileSync(HASH_FILE, computeDeployHash(clientId, guildId, payloads), 'utf8');
  } catch (err) {
    logger.warn(`Could not save the command hash to ${HASH_FILE} (${err.message}) — the next start will deploy again.`);
  }
}

/**
 * Hashes the current command payloads and deploys to Discord only when the
 * set of commands (or the deploy target) has changed since the last deploy. Safe to call on every
 * startup — no-ops when nothing changed, so it won't burn rate-limit budget.
 */
export async function autoDeployCommands() {
  const token    = process.env.DISCORD_TOKEN;
  const clientId = process.env.CLIENT_ID;
  if (!token || !clientId) {
    logger.warn('DISCORD_TOKEN / CLIENT_ID not set — skipping auto-deploy.');
    return;
  }

  const guildId  = process.env.DEPLOY_GUILD_ID;
  const payloads = await collectPayloads('auto-deploy hash');
  payloads.sort((a, b) => a.name.localeCompare(b.name));
  const hash = computeDeployHash(clientId, guildId, payloads);

  let storedHash = '';
  try {
    storedHash = readFileSync(HASH_FILE, 'utf8').trim();
  } catch {
    // First run — no hash file yet
  }

  if (hash === storedHash) {
    logger.info('Commands unchanged — skipping deploy.');
    return;
  }

  logger.info(`Commands changed (${payloads.length} total) — deploying...`);

  const rest    = new REST({ version: '10' }).setToken(token);
  const route   = guildId
    ? Routes.applicationGuildCommands(clientId, guildId)
    : Routes.applicationCommands(clientId);

  let result;
  try {
    result = await rest.put(route, { body: payloads });
  } catch (err) {
    logger.error('Auto-deploy failed — bot will still start', err);
    // Don't exit; the bot can run with stale commands rather than not at all
    return;
  }
  logger.success(
    `Auto-deployed ${result.length} command(s) ` +
    (guildId ? `to guild ${guildId}` : 'globally') + '.'
  );

  try {
    writeFileSync(HASH_FILE, hash, 'utf8');
  } catch (err) {
    logger.warn(`Could not save the command hash to ${HASH_FILE} (${err.message}) — the next start will deploy again.`);
  }
}
