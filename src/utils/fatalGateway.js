// Gateway close codes that reconnecting can never fix. Retrying them only burns
// Discord's IDENTIFY budget (1000 logins per day; going over resets the token),
// so the ShardingManager in index.js parks instead of restarting.
export const FATAL_CLOSE_CODES = {
  4004: {
    name: 'Authentication failed',
    fix:  'Discord rejected DISCORD_TOKEN. Reset the token in the Developer Portal ' +
          '(Bot → Reset Token), put the new one in DISCORD_TOKEN and restart the bot.',
  },
  4013: {
    name: 'Invalid intents',
    fix:  'The intents sent to the gateway are invalid. Check the GatewayIntentBits ' +
          'list in src/bot.js, then restart the bot.',
  },
  4014: {
    name: 'Disallowed intents',
    fix:  'Enable the privileged intents (Server Members, Presence and Message Content) ' +
          'under Bot → Privileged Gateway Intents in the Developer Portal, then restart the bot.',
  },
};

// Key of the message a shard sends to the ShardingManager (client.shard.send)
// when it hits one of the codes above.
export const FATAL_MESSAGE_KEY = '_fatalGatewayClose';

/**
 * Maps a login()/spawn() rejection to one of FATAL_CLOSE_CODES, or null.
 * @discordjs/ws rejects login() with plain Errors for these close codes, and a
 * 401 from the /gateway/bot pre-flight becomes a TokenInvalid error (a bad
 * token caught before the gateway sends 4004).
 * @param {unknown} err
 * @returns {number|null}
 */
export function fatalCodeFromError(err) {
  if (!err) return null;
  if (err.code === 'TokenInvalid') return 4004;
  switch (err.message) {
    case 'Authentication failed':   return 4004;
    case 'Used invalid intents':    return 4013;
    case 'Used disallowed intents': return 4014;
    default:                        return null;
  }
}
