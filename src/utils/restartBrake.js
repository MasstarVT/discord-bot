import { mkdirSync, readFileSync, renameSync, writeFileSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

// Every start of the bot is a fresh IDENTIFY, and Discord allows 1000 per day
// (going over ends every session and resets the token). A crash loop under
// Docker's restart policy could use that up, so starts are paced here.
const WINDOW_MS    = 60 * 60_000; // count starts in the last hour
const FREE_STARTS  = 5;           // up to this many per hour start immediately
const MAX_DELAY_MS = 30 * 60_000;
const FILE_NAME    = 'restart-brake.json';

/**
 * Delay for the n-th start within the last hour: none up to FREE_STARTS, then
 * 2^(n-5) minutes (2, 4, 8, 16 ...) capped at 30 minutes.
 * @param {number} starts
 * @returns {number} milliseconds
 */
export function brakeDelayMs(starts) {
  if (starts <= FREE_STARTS) return 0;
  return Math.min(MAX_DELAY_MS, 2 ** (starts - FREE_STARTS) * 60_000);
}

function readStarts(file) {
  try {
    const { starts } = JSON.parse(readFileSync(file, 'utf8'));
    return Array.isArray(starts) ? starts.filter(Number.isFinite) : [];
  } catch {
    return []; // first start, or unreadable: start a fresh history
  }
}

function writeStarts(dir, starts) {
  mkdirSync(dir, { recursive: true });
  const file = join(dir, FILE_NAME);
  writeFileSync(`${file}.tmp`, JSON.stringify({ starts }), 'utf8');
  renameSync(`${file}.tmp`, file);
}

/**
 * Records this start in <stateDir>/restart-brake.json, prunes entries older
 * than an hour, and returns how long to wait before connecting to Discord.
 *
 * If stateDir is not writable, the history goes to the OS temp dir instead,
 * which still survives `docker restart` (same container) and so still brakes
 * a crash loop.
 *
 * @param {string} stateDir
 * @param {number} [now=Date.now()]
 * @returns {{ starts: number, delayMs: number, file: string, warning?: string }}
 */
export function recordStart(stateDir, now = Date.now()) {
  const fallbackDir = join(tmpdir(), 'discord-bot-state');
  let warning;

  for (const dir of [stateDir, fallbackDir]) {
    const file   = join(dir, FILE_NAME);
    const starts = readStarts(file).filter((t) => t > now - WINDOW_MS && t <= now);
    starts.push(now);
    try {
      writeStarts(dir, starts);
      return { starts: starts.length, delayMs: brakeDelayMs(starts.length), file, warning };
    } catch (err) {
      warning = `Could not write ${file} (${err.code ?? err.message})`;
    }
  }

  // Neither location is writable: count only this start.
  return { starts: 1, delayMs: 0, file: null, warning: `${warning}; restart brake disabled` };
}
