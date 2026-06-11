import { Client, Collection, GatewayIntentBits, Partials } from 'discord.js';
import { loadCommands } from './handlers/commandLoader.js';
import { loadEvents }   from './handlers/eventLoader.js';
import { registerHandlers as registerRRHandlers } from './services/reactionRoleService.js';
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
  logger.error('Fatal error during bot startup', err);
  process.exit(1);
});

export default client;
