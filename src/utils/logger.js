import { inspect } from 'node:util';
import { isMainThread, workerData } from 'node:worker_threads';
import chalk from 'chalk';

const pad = (n) => String(n).padStart(2, '0');

function timestamp() {
  const d = new Date();
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
         `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

function shardPrefix() {
  // discord.js passes the shard id as SHARDS: in `worker` mode (what index.js uses) only through
  // workerData (the worker shares the parent's process.env, so a per-shard value cannot live there);
  // in `process` mode through the child's environment. SHARD_ID is kept as a manual override (R301).
  const fromWorker = isMainThread ? undefined : workerData?.SHARDS;
  const id = fromWorker ?? process.env.SHARDS ?? process.env.SHARD_ID;
  return id !== undefined && id !== '' ? chalk.gray(`[Shard ${id}] `) : '';
}

const CONTROL_ESCAPES = { '\n': '\\n', '\r': '\\r' };

// A log record is one line: C0 controls and DEL in a string (a feed title, a user name) become
// escapes, so an interpolated value can neither start a forged line nor inject terminal codes.
function escapeControls(s) {
  return s.replace(/[\x00-\x1f\x7f]/g, (c) => CONTROL_ESCAPES[c] ?? `\\x${c.charCodeAt(0).toString(16).padStart(2, '0')}`);
}

// Errors print their message (the stack follows in error()); strings print as-is but for the
// escapes above; other objects are inspected, so context objects never show up as
// "[object Object]" (R302) and inspect() already quotes and escapes the strings inside them.
function formatArg(a) {
  if (a instanceof Error) return escapeControls(String(a.message));
  if (typeof a === 'string') return escapeControls(a);
  if (a !== null && typeof a === 'object') return inspect(a, { depth: 3, breakLength: Infinity });
  return escapeControls(String(a));
}

function format(level, color, ...args) {
  const ts   = chalk.gray(`[${timestamp()}]`);
  const lvl  = color(`[${level}]`);
  const shard = shardPrefix();
  // Allow passing Error objects: print message inline, stack below
  const parts = args.map(formatArg);
  return `${ts} ${lvl} ${shard}${parts.join(' ')}`;
}

const logger = {
  info(...args) {
    console.log(format('INFO ', chalk.cyan, ...args));
  },
  warn(...args) {
    console.warn(format('WARN ', chalk.yellow, ...args));
  },
  error(...args) {
    const line = format('ERROR', chalk.red, ...args);
    console.error(line);
    // Print stack traces for any Error objects passed
    for (const a of args) {
      if (a instanceof Error && a.stack) console.error(chalk.red(a.stack));
    }
  },
  debug(...args) {
    if (process.env.NODE_ENV === 'production') return;
    console.debug(format('DEBUG', chalk.magenta, ...args));
  },
  success(...args) {
    console.log(format('OK   ', chalk.green, ...args));
  },
};

export default logger;
