import { PERMISSION_LEVELS } from '../config/constants.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { errorEmbed } from '../utils/embedBuilder.js';
import { cache } from '../services/redis.js';
import prisma from '../database/client.js';
import logger from '../utils/logger.js';

// ── Settings cache helper ─────────────────────────────────────────────────────

async function getGuildSettings(guildId) {
  const cacheKey = `guild:${guildId}:settings`;
  const cached = await cache.get(cacheKey);
  if (cached) return cached;

  const settings = await prisma.guildSettings.findUnique({ where: { guildId } });
  if (settings) await cache.set(cacheKey, settings, 300);
  return settings;
}

// ── Safe reply helper — works whether or not we've already replied ─────────────

async function safeReply(interaction, payload) {
  try {
    if (interaction.replied || interaction.deferred) {
      return await interaction.editReply(payload);
    }
    return await interaction.reply({ ...payload, ephemeral: true });
  } catch {
    // Interaction may have expired — silently discard
  }
}

// ── CustomID namespace parser ─────────────────────────────────────────────────
// CustomIDs follow the format:  "action:param1:param2"
// e.g.  "rr:groupId:roleId"  or  "confirm-ban:userId"

function parseCustomId(customId) {
  const parts = customId.split(':');
  return { action: parts[0], params: parts.slice(1) };
}

// ── Main router ───────────────────────────────────────────────────────────────

/**
 * Routes any Discord interaction to the appropriate handler.
 * Called from src/events/interactionCreate.js.
 *
 * @param {import('discord.js').Interaction} interaction
 * @param {import('discord.js').Client}      client
 */
export async function routeInteraction(interaction, client) {

  // ── Slash Commands ──────────────────────────────────────────────────────────
  if (interaction.isChatInputCommand()) {
    const command = client.commands.get(interaction.commandName);
    if (!command) {
      return safeReply(interaction, {
        embeds: [errorEmbed('Unknown Command', 'This command no longer exists. Try `/` to see available commands.')],
      });
    }

    // Permission check
    const requiredLevel = command.permissionLevel ?? PERMISSION_LEVELS.MEMBER;
    if (requiredLevel > PERMISSION_LEVELS.MEMBER && interaction.guild) {
      const settings = await getGuildSettings(interaction.guildId).catch(() => null);
      const memberLevel = resolvePermissionLevel(interaction.member, settings);
      if (memberLevel < requiredLevel) {
        return safeReply(interaction, {
          embeds: [errorEmbed('Insufficient Permissions', "You don't have permission to use this command.")],
        });
      }
    }

    try {
      await command.execute(interaction, client);
    } catch (err) {
      logger.error(`Error in command "${interaction.commandName}"`, err, {
        guildId: interaction.guildId,
        userId:  interaction.user.id,
      });
      await safeReply(interaction, {
        embeds: [errorEmbed('Command Error', 'Something went wrong while running this command.')],
      });
    }
    return;
  }

  // ── Autocomplete ────────────────────────────────────────────────────────────
  if (interaction.isAutocomplete()) {
    const command = client.commands.get(interaction.commandName);
    if (!command?.autocomplete) return;
    try {
      await command.autocomplete(interaction, client);
    } catch (err) {
      logger.error(`Autocomplete error for "${interaction.commandName}"`, err);
    }
    return;
  }

  // ── Button Interactions ─────────────────────────────────────────────────────
  if (interaction.isButton()) {
    const { action } = parseCustomId(interaction.customId);
    const handler = client.buttonHandlers.get(action);
    if (!handler) return;
    try {
      await handler(interaction, client);
    } catch (err) {
      logger.error(`Button handler error for action "${action}"`, err);
      await safeReply(interaction, {
        embeds: [errorEmbed('Button Error', 'Something went wrong processing this button.')],
      });
    }
    return;
  }

  // ── String Select Menu Interactions ─────────────────────────────────────────
  if (interaction.isStringSelectMenu()) {
    const { action } = parseCustomId(interaction.customId);
    const handler = client.selectHandlers.get(action);
    if (!handler) return;
    try {
      await handler(interaction, client);
    } catch (err) {
      logger.error(`Select handler error for action "${action}"`, err);
      await safeReply(interaction, {
        embeds: [errorEmbed('Menu Error', 'Something went wrong processing this selection.')],
      });
    }
    return;
  }

  // ── Modal Submits ───────────────────────────────────────────────────────────
  if (interaction.isModalSubmit()) {
    const { action } = parseCustomId(interaction.customId);
    const handler = client.modalHandlers?.get(action);
    if (!handler) return;
    try {
      await handler(interaction, client);
    } catch (err) {
      logger.error(`Modal handler error for action "${action}"`, err);
      await safeReply(interaction, {
        embeds: [errorEmbed('Modal Error', 'Something went wrong processing your submission.')],
      });
    }
  }
}
