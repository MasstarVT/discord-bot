import { Client, Collection, Events, GatewayIntentBits, Partials } from 'discord.js';
import { loadCommands } from './handlers/commandLoader.js';
import { loadEvents }   from './handlers/eventLoader.js';
import { registerHandlers as registerRRHandlers } from './services/reactionRoleService.js';
import { disconnect as disconnectDatabase } from './database/client.js';
import redis             from './services/redis.js';
import { FATAL_CLOSE_CODES, FATAL_MESSAGE_KEY, fatalCodeFromError } from './utils/fatalGateway.js';
import logger            from './utils/logger.js';

const client = new Client({
  intents: [
    GatewayIntentBits.Guilds,
    GatewayIntentBits.GuildMembers,          // Privileged
    GatewayIntentBits.GuildModeration,
    GatewayIntentBits.GuildEmojisAndStickers,
    GatewayIntentBits.GuildIntegrations,
    GatewayIntentBits.GuildWebhooks,
    GatewayIntentBits.GuildInvites,
    GatewayIntentBits.GuildVoiceStates,
    GatewayIntentBits.GuildPresences,        // Privileged
    GatewayIntentBits.GuildMessages,
    GatewayIntentBits.GuildMessageReactions,
    GatewayIntentBits.DirectMessages,
    GatewayIntentBits.MessageContent,        // Privileged
  ],
  partials: [
    Partials.Channel,
    Partials.Message,
    Partials.Reaction,
    Partials.GuildMember,
    Partials.User,
  ],
});

// ── Component handler registries ─────────────────────────────────────────────
// Modules register their handlers here at startup or lazily.
// Keys follow the customId namespace: "action" from "action:param1:param2"
client.commands           = new Collection();
client.commandCategories  = new Map();
client.buttonHandlers     = new Map();
client.selectHandlers     = new Map();
client.modalHandlers      = new Map();

// ── Fatal gateway close codes ────────────────────────────────────────────────
// 4004 / 4013 / 4014 (bad token, invalid or disallowed intents) never fix
// themselves. Tell the ShardingManager so it parks instead of restarting.
// prependListener: this must reach the manager before discord.js's own
// shardDisconnect → "_disconnect" message does.
let fatalReported = false;

function reportFatal(code) {
  if (fatalReported || !FATAL_CLOSE_CODES[code]) return;
  fatalReported = true;
  client.shard?.send({ [FATAL_MESSAGE_KEY]: code }).catch(() => {});
}

client.prependListener(Events.ShardDisconnect, (event) => reportFatal(event?.code));

// ── Shutdown hook ────────────────────────────────────────────────────────────
// Worker threads don't receive signals. index.js calls this on each shard via
// eval when it gets SIGTERM/SIGINT.
client.shutdown = async () => {
  await client.destroy();
  await Promise.allSettled([
    disconnectDatabase(),
    redis.status === 'ready' ? redis.quit() : redis.disconnect(),
  ]);
};

// ── Boot sequence ─────────────────────────────────────────────────────────────
async function start() {
  // Register persistent component handlers before loading commands so that any
  // command's registerHandlers() call can safely overwrite or extend these.
  registerRRHandlers(client);

  logger.info('Loading commands...');
  await loadCommands(client);

  logger.info('Loading events...');
  await loadEvents(client);

  logger.info('Logging in to Discord...');
  await client.login(process.env.DISCORD_TOKEN);
}

start().catch((err) => {
  const code = fatalCodeFromError(err);
  if (code) {
    // Report before exiting so the manager parks rather than restarting.
    reportFatal(code);
    logger.error(`Discord login failed with ${code} (${FATAL_CLOSE_CODES[code].name}). ${FATAL_CLOSE_CODES[code].fix}`);
  } else {
    logger.error('Fatal error during bot startup', err);
  }
  process.exit(1); // ends this worker thread; index.js decides what happens next
});

export default client;
