import { readdirSync, statSync, readFileSync, writeFileSync } from 'fs';
import { join, resolve, dirname } from 'path';
import { fileURLToPath, pathToFileURL } from 'url';
import { createHash } from 'crypto';
import { REST, Routes } from 'discord.js';
import logger from '../utils/logger.js';

const __dirname    = dirname(fileURLToPath(import.meta.url));
const COMMANDS_DIR = resolve(__dirname, '../commands');
const HASH_FILE    = resolve(__dirname, '../../.deploy-hash');

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

    client.commands.set(name, mod);

    // Tag each command with its subfolder name so /help can group by category
    const parts = file.replace(/\\/g, '/').split('/');
    const cmdIdx = parts.lastIndexOf('commands');
    const category = cmdIdx !== -1 && cmdIdx + 2 < parts.length ? parts[cmdIdx + 1] : 'general';
    client.commandCategories.set(name, category);

    // Allow commands to self-register button/select/modal handlers
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

  let files;
  try {
    files = walkDir(COMMANDS_DIR);
  } catch {
    logger.warn('Commands directory empty — deploying 0 commands.');
    files = [];
  }

  const payloads = [];
  for (const file of files) {
    try {
      const mod = await import(pathToFileURL(file).href);
      if (mod.data?.toJSON) payloads.push(mod.data.toJSON());
    } catch (err) {
      logger.error(`Skipping ${file} during deploy`, err);
    }
  }

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
}

/**
 * Hashes the current command payloads and deploys to Discord only when the
 * set of commands has changed since the last deploy. Safe to call on every
 * startup — no-ops when nothing changed, so it won't burn rate-limit budget.
 */
export async function autoDeployCommands() {
  const token    = process.env.DISCORD_TOKEN;
  const clientId = process.env.CLIENT_ID;
  if (!token || !clientId) {
    logger.warn('DISCORD_TOKEN / CLIENT_ID not set — skipping auto-deploy.');
    return;
  }

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
      if (mod.data?.toJSON) payloads.push(mod.data.toJSON());
    } catch (err) {
      logger.error(`Skipping ${file} during auto-deploy hash`, err);
    }
  }

  // Sort by name so payload order doesn't trigger false positives
  payloads.sort((a, b) => a.name.localeCompare(b.name));
  const hash = createHash('sha256').update(JSON.stringify(payloads)).digest('hex');

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

  const guildId = process.env.DEPLOY_GUILD_ID;
  const rest    = new REST({ version: '10' }).setToken(token);
  const route   = guildId
    ? Routes.applicationGuildCommands(clientId, guildId)
    : Routes.applicationCommands(clientId);

  try {
    const result = await rest.put(route, { body: payloads });
    logger.success(
      `Auto-deployed ${result.length} command(s) ` +
      (guildId ? `to guild ${guildId}` : 'globally') + '.'
    );
    writeFileSync(HASH_FILE, hash, 'utf8');
  } catch (err) {
    logger.error('Auto-deploy failed — bot will still start', err);
    // Don't exit; the bot can run with stale commands rather than not at all
  }
}
