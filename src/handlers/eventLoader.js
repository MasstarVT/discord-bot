import { readdirSync, statSync } from 'fs';
import { join, resolve, dirname } from 'path';
import { fileURLToPath, pathToFileURL } from 'url';
import logger from '../utils/logger.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const EVENTS_DIR = resolve(__dirname, '../events');

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
 * Loads and registers all event handlers from /src/events.
 * Each event file must export: { name: string, once: boolean, execute: function }
 * The client is injected as the last argument to every handler.
 *
 * @param {import('discord.js').Client} client
 */
export async function loadEvents(client) {
  let files;
  try {
    files = walkDir(EVENTS_DIR);
  } catch {
    logger.warn('Events directory not found. Skipping event load.');
    return;
  }

  let loaded = 0;
  for (const file of files) {
    let mod;
    try {
      mod = await import(pathToFileURL(file).href);
    } catch (err) {
      logger.error(`Failed to import event file: ${file}`, err);
      continue;
    }

    if (!mod.name || typeof mod.execute !== 'function') {
      logger.warn(`Skipping ${file} — missing required exports: { name, execute }`);
      continue;
    }

    const listener = (...args) => mod.execute(...args, client);

    if (mod.once) {
      client.once(mod.name, listener);
    } else {
      client.on(mod.name, listener);
    }

    loaded++;
  }

  logger.success(`Registered ${loaded} event listener(s).`);
}
