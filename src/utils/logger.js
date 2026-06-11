import chalk from 'chalk';

const pad = (n) => String(n).padStart(2, '0');

function timestamp() {
  const d = new Date();
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
         `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

function shardPrefix() {
  // process.env.SHARD_ID is set by discord.js ShardingManager worker context
  const id = process.env.SHARD_ID ?? process.env.DISCORD_SHARD_ID;
  return id !== undefined ? chalk.gray(`[Shard ${id}] `) : '';
}

function format(level, color, ...args) {
  const ts   = chalk.gray(`[${timestamp()}]`);
  const lvl  = color(`[${level}]`);
  const shard = shardPrefix();
  // Allow passing Error objects: print message inline, stack below
  const parts = args.map((a) => (a instanceof Error ? a.message : String(a)));
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
